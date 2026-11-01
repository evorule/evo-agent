// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! MCP server 长驻进程生命周期治理（资源边界设计第一批）
//!
//! ## 孤儿兜底（无条件，防御性新增）
//!
//! 显式 close/shutdown 路径的收割在 transport（kill+wait）与 manager（三条路径）
//! 早已齐备；缺口在**非优雅退出**（panic/abort/强杀）——子进程全部孤儿化。兜底：
//! - Windows：Job Object（`KILL_ON_JOB_CLOSE`）+ `CREATE_SUSPENDED` 起手 →
//!   assign → resume（闭合 assign 竞态窗口：子进程在进 job 前不可创建孙进程）；
//!   父进程任何形态消失，OS 收割整棵进程树。
//! - Unix：进程组（`process_group(0)`）；树级收割 best-effort（平台差如实声明）。
//! - 两侧叠加 `kill_on_drop`（unwind panic 路径的运行时层兜底）。
//!
//! ## 生命周期策略（显式配置，缺省全关 = 现行为）
//!
//! `max_idle_secs`（空闲收割）/`max_lifetime_secs`（硬寿命）/`hang_threshold`
//! （连续请求失败计数→判定挂死；精确超时分类在各外层，L1 以连续失败为信号，
//! 如实近似）——由 [`crate::mcp::client::McpClient::enable_lifecycle`] 启动
//! 监控任务执行；触限动作=**收割 + 结构化事件 + 通知**（重启决策在 manager/
//! serve 层，退避序列见 [`Backoff`]）。
//!
//! ## 许可与运行分面
//!
//! 生命周期/限额参数 = 运维面（mcp.toml）+ 代码内缺省值；不进治理清单
//! （资源边界设计：许可与运行分面）。触限/收割事件与拉起判定共用 `mcp_spawn_gate` 事件面
//! 形态——P2c 入链挂点。

use std::time::Duration;

/// 单 server 生命周期策略（全部缺省关；显式配置才生效——缺省不变式）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LifecyclePolicy {
    /// 空闲收割：无在途请求且距上次请求超过该秒数
    pub max_idle_secs: Option<u64>,
    /// 硬寿命上界：进程存活超过该秒数即收割
    pub max_lifetime_secs: Option<u64>,
    /// 连续请求失败计数达到该值即判定挂死并收割（连续成功重置）
    pub hang_threshold: u32,
}

impl LifecyclePolicy {
    /// 从 server 配置解析（运维面参数；缺省全关=现行为）
    pub fn from_config(cfg: &crate::config::McpServerConfig) -> Self {
        Self {
            max_idle_secs: cfg.max_idle_secs,
            max_lifetime_secs: cfg.max_lifetime_secs,
            hang_threshold: cfg.hang_threshold.unwrap_or(0),
        }
    }

    /// 是否有任一策略生效（全关 = 不启动监控任务，零开销 = 现行为）
    pub fn is_active(&self) -> bool {
        self.max_idle_secs.is_some() || self.max_lifetime_secs.is_some() || self.hang_threshold > 0
    }

    /// 监控巡检节奏：策略窗口的合理分数（无策略=不调用）
    pub(crate) fn tick(&self) -> Duration {
        let mut secs = self
            .max_idle_secs
            .or(self.max_lifetime_secs)
            .unwrap_or(5)
            .min(60);
        secs = (secs / 2).max(1);
        Duration::from_secs(secs)
    }
}

/// 退避序列：1s、2s、4s…封顶 60s（crash loop 不风暴）；成功稳定后 [`Backoff::reset`]
#[derive(Debug)]
pub struct Backoff {
    next_secs: u64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    pub fn new() -> Self {
        Self { next_secs: 1 }
    }

    /// 下一级退避时长（1s 起步，倍增至 60s 封顶）
    pub fn next_delay(&mut self) -> Duration {
        let d = Duration::from_secs(self.next_secs);
        self.next_secs = (self.next_secs.saturating_mul(2)).min(60);
        d
    }

    pub fn reset(&mut self) {
        self.next_secs = 1;
    }
}

/// 收割原因（事件面与重启决策的共用语义）
#[derive(Debug, Clone)]
pub enum ReapReason {
    /// 空闲超窗（idle_secs = 实际空闲秒数）
    Idle { idle_secs: u64 },
    /// 硬寿命到（age_secs = 实际存活秒数）
    Lifetime { age_secs: u64 },
    /// 连续请求失败判定挂死（consecutive = 计数值）
    Hung { consecutive: u32 },
}

impl ReapReason {
    pub fn code(&self) -> &'static str {
        match self {
            ReapReason::Idle { .. } => "idle",
            ReapReason::Lifetime { .. } => "lifetime",
            ReapReason::Hung { .. } => "hung",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            ReapReason::Idle { idle_secs } => format!("空闲 {idle_secs}s 超窗"),
            ReapReason::Lifetime { age_secs } => format!("存活 {age_secs}s 到达硬寿命上界"),
            ReapReason::Hung { consecutive } => format!("连续 {consecutive} 次请求失败判定挂死"),
        }
    }
}

/// 收割通知（监控任务 → manager/serve 重启监督的通道载荷）
#[derive(Debug, Clone)]
pub struct ReapNotice {
    pub server_name: String,
    pub reason: ReapReason,
}

/// 监控任务：巡检生命周期策略，触限即收割（close transport 杀进程树）并通知。
///
/// 由 [`crate::mcp::client::McpClient::enable_lifecycle`] 启动；`activity`/
/// `in_flight`/`hang` 三个共享计数由 client 请求路径维护。收割动作 =
/// `close()`（kill 子进程树）+ 结构化事件 + 通知发送（通道关闭=无消费者，
/// fail-soft 不影响收割本身）。
pub(crate) fn spawn_monitor(
    server_name: String,
    policy: LifecyclePolicy,
    activity: std::sync::Arc<std::sync::atomic::AtomicU64>,
    in_flight: std::sync::Arc<std::sync::atomic::AtomicU32>,
    hang: std::sync::Arc<std::sync::atomic::AtomicU32>,
    close: std::sync::Arc<dyn Fn() + Send + Sync>,
    reap_tx: tokio::sync::mpsc::UnboundedSender<ReapNotice>,
) {
    let started = std::time::Instant::now();
    tokio::spawn(async move {
        let tick = policy.tick();
        loop {
            tokio::time::sleep(tick).await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default();
            let last = activity.load(std::sync::atomic::Ordering::Relaxed);
            // 硬寿命（与活动无关，到点即收）
            if let Some(life) = policy.max_lifetime_secs {
                let age = started.elapsed().as_secs();
                if age >= life {
                    reap(
                        &server_name,
                        &close,
                        &reap_tx,
                        ReapReason::Lifetime { age_secs: age },
                    );
                    return;
                }
            }
            // 空闲（仅在途请求为零——有在途请求说明 server 正被使用）
            if let Some(idle_limit) = policy.max_idle_secs {
                if in_flight.load(std::sync::atomic::Ordering::Relaxed) == 0
                    && last > 0
                    && now.saturating_sub(last) >= idle_limit
                {
                    reap(
                        &server_name,
                        &close,
                        &reap_tx,
                        ReapReason::Idle {
                            idle_secs: now - last,
                        },
                    );
                    return;
                }
            }
            // 挂死（连续失败计数达到阈值）
            if policy.hang_threshold > 0 {
                let consecutive = hang.load(std::sync::atomic::Ordering::Relaxed);
                if consecutive >= policy.hang_threshold {
                    reap(
                        &server_name,
                        &close,
                        &reap_tx,
                        ReapReason::Hung { consecutive },
                    );
                    return;
                }
            }
        }
    });
}

fn reap(
    server_name: &str,
    close: &std::sync::Arc<dyn Fn() + Send + Sync>,
    reap_tx: &tokio::sync::mpsc::UnboundedSender<ReapNotice>,
    reason: ReapReason,
) {
    tracing::warn!(
        target: "mcp_spawn_gate",
        server = %server_name,
        reason = reason.code(),
        detail = %reason.detail(),
        "MCP lifecycle: 生命周期策略触限，收割 server 进程树"
    );
    // 治理事实出证（收割与拉生死同链；command/清单来源在监控层不可得，缺省）
    crate::governance::hooks::mcp_spawn(
        &crate::governance::hooks::McpSpawnFacts {
            server_name: server_name.to_string(),
            command: String::new(),
            args_prefix: None,
            server_key: None,
            approval_ref: None,
            source: None,
            policy: None,
        },
        crate::governance::hooks::McpSpawnVerdict::Reap,
    );
    close();
    let _ = reap_tx.send(ReapNotice {
        server_name: server_name.to_string(),
        reason,
    });
}

/// spawn 前的孤儿兜底装配（平台原语；无条件应用——防御性新增）
///
/// - Windows：`CREATE_SUSPENDED` 起手（闭合 assign 竞态：子进程在进 job 前
///   无法创建孙进程）；assign 完成后由 [`job::resume_process_threads`] 恢复。
/// - Unix：进程组（`process_group(0)`），树级收割 best-effort（平台差如实声明）。
pub(crate) fn configure_orphan_proofing(cmd: &mut tokio::process::Command) {
    // unwind panic 路径的运行时层兜底（abort/强杀由平台原语兜底）
    cmd.kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        cmd.creation_flags(CREATE_SUSPENDED);
    }
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
}

/// spawn 后的孤儿兜底收尾：assign job（Windows）/无操作（Unix）。
/// 返回值在 Windows 侧为必须**持有至 transport 销毁**的 job 句柄
/// （句柄关闭 = `KILL_ON_JOB_CLOSE` 收割整棵进程树）。
#[cfg(windows)]
pub(crate) fn finish_orphan_proofing(
    child: &tokio::process::Child,
) -> Result<Option<job::JobHandle>, String> {
    let raw = child
        .raw_handle()
        .ok_or_else(|| "MCP child missing process handle".to_string())?;
    let job = job::create_kill_on_close()?;
    // SAFETY（job 模块治理内）：raw_handle 的裸指针即进程 HANDLE
    let raw_handle = windows::Win32::Foundation::HANDLE(raw as _);
    job.assign(raw_handle)?;
    let pid = child
        .id()
        .ok_or_else(|| "MCP child missing pid".to_string())?;
    job.resume_process_threads(pid);
    Ok(Some(job))
}

/// Unix side: no Job Object to assign; orphan protection is the process
/// group set up in `configure_orphan_proofing`, so this is a no-op.
#[cfg(unix)]
pub(crate) fn finish_orphan_proofing(child: &tokio::process::Child) -> Result<Option<()>, String> {
    let _ = child;
    Ok(None)
}

/// Windows Job Object 封装（孤儿兜底的 OS 层机制）
///
/// unsafe 治理：本模块为全仓第二 allow 点（先例=builtin_tools::appcontainer，
/// 同类 OS 低盒 Win32 FFI 面）——进程树治理原语（Job Object 三调用+线程快照
/// 枚举+句柄清理）无 std 兑现形态；unsafe 集中本模块，审计面不外溢。
#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) mod job {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// Job 句柄（持有至 transport 销毁；Send+Sync 供 transport 结构体持有）
    pub(crate) struct JobHandle(HANDLE);

    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    impl JobHandle {
        /// 把子进程主进程句柄 assign 进 job
        pub(crate) fn assign(&self, child: HANDLE) -> Result<(), String> {
            unsafe {
                AssignProcessToJobObject(self.0, child)
                    .map_err(|e| format!("AssignProcessToJobObject failed: {e}"))
            }
        }

        /// 恢复 `CREATE_SUSPENDED` 起手挂起的全部子进程线程
        /// （线程快照枚举 + ResumeThread；累加恢复计数至耗尽）
        pub(crate) fn resume_process_threads(&self, pid: u32) {
            use windows::Win32::System::Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                THREADENTRY32,
            };
            use windows::Win32::System::Threading::{
                OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
            };
            unsafe {
                let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
                    tracing::warn!(
                        pid,
                        "MCP child thread snapshot failed (child left suspended)"
                    );
                    return;
                };
                let mut entry = THREADENTRY32 {
                    dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                    ..Default::default()
                };
                if Thread32First(snapshot, &mut entry).is_err() {
                    tracing::warn!(
                        pid,
                        "MCP child thread enumeration failed (child left suspended)"
                    );
                    let _ = windows::Win32::Foundation::CloseHandle(snapshot);
                    return;
                }
                loop {
                    if entry.th32OwnerProcessID == pid {
                        if let Ok(thread) =
                            OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID)
                        {
                            // 返回值=前序挂起计数；0/-1 表示无更多挂起层
                            let _ = ResumeThread(thread);
                            let _ = windows::Win32::Foundation::CloseHandle(thread);
                        }
                    }
                    if Thread32Next(snapshot, &mut entry).is_err() {
                        break;
                    }
                }
                let _ = windows::Win32::Foundation::CloseHandle(snapshot);
            }
        }
    }

    /// 创建带 `KILL_ON_JOB_CLOSE` 的 job：job 句柄关闭（含父进程消亡）即
    /// 收割整棵进程树——非优雅退出的孤儿兜底机制
    pub(crate) fn create_kill_on_close() -> Result<JobHandle, String> {
        unsafe {
            let handle = CreateJobObjectW(None, None)
                .map_err(|e| format!("CreateJobObjectW failed: {e}"))?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .map_err(|e| format!("SetInformationJobObject failed: {e}"))?;
            Ok(JobHandle(handle))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_sequence_caps_at_60() {
        let mut b = Backoff::new();
        let seq: Vec<u64> = (0..8).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(seq, vec![1, 2, 4, 8, 16, 32, 60, 60]);
        b.reset();
        assert_eq!(b.next_delay().as_secs(), 1);
    }

    #[test]
    fn policy_defaults_are_off_and_inactive() {
        let p = LifecyclePolicy::default();
        assert!(!p.is_active());
        assert_eq!(p.hang_threshold, 0);
        assert_eq!(p.max_idle_secs, None);
        assert_eq!(p.max_lifetime_secs, None);
    }

    #[test]
    fn policy_active_detection_and_tick() {
        let p = LifecyclePolicy {
            max_idle_secs: Some(300),
            ..Default::default()
        };
        assert!(p.is_active());
        // tick = 窗口一半，钳制 60s 上限后取半（300→60→30），下限 1s
        assert_eq!(p.tick(), Duration::from_secs(30));
        let p2 = LifecyclePolicy {
            hang_threshold: 3,
            ..Default::default()
        };
        assert!(p2.is_active());
        // 仅挂死策略：巡检基线 5s 取半（整数除法=2s）
        assert_eq!(p2.tick(), Duration::from_secs(2));
    }

    // ===== 行为测试：真进程 lifetime 收割（生命周期治理核心承诺） =====

    #[tokio::test]
    async fn lifetime_policy_reaps_real_process() {
        use super::super::transport::McpTransport as _;
        use std::sync::Arc;
        use std::time::Instant;

        // 真长驻子进程（非 MCP server——监控在 transport 层直接驱动）
        let (command, args): (&str, Vec<String>) = if cfg!(windows) {
            ("ping", vec!["-n".into(), "30".into(), "127.0.0.1".into()])
        } else {
            ("sleep", vec!["30".into()])
        };
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let transport = Arc::new(
            super::super::transport::StdioTransport::spawn(command, &arg_refs)
                .await
                .expect("spawn probe child"),
        );

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let t2 = transport.clone();
        let close: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let t = t2.clone();
            tokio::spawn(async move {
                let _ = t.close().await;
            });
        });
        let policy = LifecyclePolicy {
            max_idle_secs: None,
            max_lifetime_secs: Some(1),
            hang_threshold: 0,
        };
        spawn_monitor(
            "probe".to_string(),
            policy,
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
            Arc::new(std::sync::atomic::AtomicU32::new(0)),
            Arc::new(std::sync::atomic::AtomicU32::new(0)),
            close,
            tx,
        );

        let started = Instant::now();
        let notice = rx.recv().await.expect("reap notice within window");
        assert_eq!(notice.reason.code(), "lifetime");
        // 自然寿命 30s：1s 策略触限即收——远早于自然退出即证明收割发生
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "reap must fire on policy window, not natural exit"
        );
    }

    #[test]
    fn reap_reason_codes_and_details() {
        assert_eq!(ReapReason::Idle { idle_secs: 5 }.code(), "idle");
        assert_eq!(ReapReason::Lifetime { age_secs: 9 }.code(), "lifetime");
        assert_eq!(ReapReason::Hung { consecutive: 3 }.code(), "hung");
        assert!(ReapReason::Hung { consecutive: 3 }
            .detail()
            .contains("3 次"));
    }
}
