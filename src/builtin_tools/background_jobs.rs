// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 后台作业注册表 —— dev-feedback P0a 余量(后台执行预授权)的承载面。
//!
//! shell_exec 的 `background=true` 分支经审批链(与前台同一条件评估,零旁路)
//! 后由本注册表收容子进程:输出重定向至 `{workdir}/.evo/jobs/{id}.log`,
//! 进程组脱离(Unix `process_group(0)` / Windows `CREATE_NO_WINDOW`),
//! 终止复用 [`crate::builtin_tools::shell_exec::terminate_family_by_pid`]
//! 族级语义(含 cargo test 产出的孙进程)。
//!
//! ## 生命周期
//!
//! - 注册表=进程级单例(OnceLock):serve 存续期即作业存续期;
//! - 查询/读尾/终止经 `job_control` 工具面;注册表条目不持久化——serve
//!   重启后旧 job_id 如实报 unknown(日志文件在盘,可手工查)。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 作业读尾的缺省字节帽
pub const DEFAULT_TAIL_BYTES: u64 = 4096;

/// 单条后台作业(句柄+溯源面)
pub struct JobEntry {
    /// 子进程本体(try_wait 轮询/退出码)
    pub child: std::process::Child,
    /// spawn 时快照(族级终止锚)
    pub pid: u32,
    /// 输出重定向文件(append 形;stdout+stderr 合流)
    pub log_path: PathBuf,
    /// 原始命令串(留痕/回显)
    pub command: String,
    /// 启动时刻(epoch ms)
    pub started_epoch_ms: u64,
    /// 回收到的退出码缓存(try_wait 幂等窗口;None=running/信号终止)
    pub exit_code_cache: Option<i32>,
}

/// 作业状态视图(`job_control action=status/read` 的公共段)
pub struct JobStatusView {
    /// 作业 id(`job-{epoch_ms}-{seq}` 形态)
    pub job_id: String,
    /// running | completed
    pub status: &'static str,
    /// completed 时的退出码(信号终止等无码形态=None,如实缺席)
    pub exit_code: Option<i32>,
    /// 自 spawn 起的墙钟毫秒
    pub elapsed_ms: u64,
    /// 合流输出日志文件路径(stdout+stderr append 形)
    pub log_path: PathBuf,
    /// 日志现势字节数
    pub log_bytes: u64,
}

/// 进程级作业注册表(OnceLock 单例;Mutex 串行化——临界区无 await)
pub struct JobRegistry {
    jobs: Mutex<BTreeMap<String, JobEntry>>,
    seq: AtomicU64,
}

static REGISTRY: OnceLock<JobRegistry> = OnceLock::new();

/// 进程级注册表句柄(serve 存续期=作业存续期)
pub fn registry() -> &'static JobRegistry {
    REGISTRY.get_or_init(|| JobRegistry {
        jobs: Mutex::new(BTreeMap::new()),
        seq: AtomicU64::new(1),
    })
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 日志目录约定:`{workdir}/.evo/jobs`(与 .evo 配置面同域;目录惰性建)
fn jobs_dir(workdir: Option<&Path>) -> PathBuf {
    let base = match workdir {
        Some(d) => d.join(".evo").join("jobs"),
        None => PathBuf::from(".evo").join("jobs"),
    };
    let _ = std::fs::create_dir_all(&base);
    base
}

impl JobRegistry {
    /// spawn 一条后台作业:输出重定向至日志文件,进程组脱离,入册。
    ///
    /// 审批语义由调用方(shell_exec 审批链)先行完成——本函数零审批判断,
    /// 只做 spawn 与收容(预授权面=调用方的条件评估,零旁路)。
    pub fn spawn_job(
        &self,
        program: &str,
        args: &[&str],
        command: &str,
        workdir: Option<&Path>,
    ) -> Result<(String, PathBuf), String> {
        let id = format!(
            "job-{}-{:04}",
            epoch_ms(),
            self.seq.fetch_add(1, Ordering::Relaxed)
        );
        let log_path = jobs_dir(workdir).join(format!("{id}.log"));
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| format!("background job log create failed: {e}"))?;
        let log_err = log
            .try_clone()
            .map_err(|e| format!("background job log handle clone failed: {e}"))?;
        let mut cmd = std::process::Command::new(program);
        cmd.args(args)
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .stdin(Stdio::null());
        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }
        // 进程组脱离:Unix pgid=pid(族级终止锚);Windows 抑制控制台窗口
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let child = cmd
            .spawn()
            .map_err(|e| format!("background spawn failed: {e}"))?;
        let pid = child.id();
        let entry = JobEntry {
            child,
            pid,
            log_path: log_path.clone(),
            command: command.to_string(),
            started_epoch_ms: epoch_ms(),
            exit_code_cache: None,
        };
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), entry);
        Ok((id, log_path))
    }

    /// 收容快照(状态视图;try_wait 同步回收退出态)
    fn view(&self, id: &str) -> Result<(JobStatusView, bool), String> {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let entry = jobs.get_mut(id).ok_or_else(|| {
            format!(
                "unknown job id \"{id}\" (jobs are process-local; a serve restart retires old ids)"
            )
        })?;
        let done = match entry.child.try_wait() {
            Ok(Some(status)) => {
                entry.exit_code_cache = status.code();
                true
            }
            Ok(None) => false,
            Err(e) => return Err(format!("job status poll failed: {e}")),
        };
        let log_bytes = std::fs::metadata(&entry.log_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let elapsed_ms = epoch_ms().saturating_sub(entry.started_epoch_ms);
        Ok((
            JobStatusView {
                job_id: id.to_string(),
                status: if done { "completed" } else { "running" },
                exit_code: entry.exit_code_cache,
                elapsed_ms,
                log_path: entry.log_path.clone(),
                log_bytes,
            },
            done,
        ))
    }

    /// 状态查询(`job_control action=status`)
    pub fn status(&self, id: &str) -> Result<JobStatusView, String> {
        self.view(id).map(|(v, _)| v)
    }

    /// 读日志尾(`job_control action=read`;从文件尾向前取至多 `max_bytes`)
    pub fn read_tail(&self, id: &str, max_bytes: u64) -> Result<(JobStatusView, String), String> {
        let (view, _) = self.view(id)?;
        let raw = std::fs::read(&view.log_path)
            .map_err(|e| format!("background job log read failed: {e}"))?;
        let start = raw.len().saturating_sub(max_bytes as usize);
        let tail = String::from_utf8_lossy(&raw[start..]).to_string();
        Ok((view, tail))
    }

    /// 终止(`job_control action=kill`;族级语义——含 cargo test 孙进程;
    /// 幂等:已完成作业直接回状态视图)
    pub fn kill(&self, id: &str) -> Result<JobStatusView, String> {
        // 锁纪律:terminate 与回收轮询都在注册表锁**外**执行(锁内跨阻塞
        // wait=全册死锁形态,测试挂死实证);回收带 5s 有界帽,进程若因平台
        // 差异未即死,如实回当前视图(fail-visible)而非挂死调用方。
        {
            let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
            let entry = jobs.get_mut(id).ok_or_else(|| {
                format!(
                    "unknown job id \"{id}\" (jobs are process-local; a serve restart retires old ids)"
                )
            })?;
            crate::builtin_tools::shell_exec::terminate_family_by_pid(entry.pid);
        }
        for _ in 0..50 {
            {
                let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
                let entry = jobs.get_mut(id).expect("entry alive within bounded reap");
                match entry.child.try_wait() {
                    Ok(Some(status)) => {
                        entry.exit_code_cache = status.code();
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => return Err(format!("kill reap failed: {e}")),
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let (view, _) = self.view(id)?;
        Ok(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全生命周期:spawn→running→kill→completed(幂等)→unknown 拒识
    /// (跨平台 sleeper:Windows ping 计数挂起/Unix sleep;均为直参 argv,
    /// 无 shell 解释面)
    #[test]
    fn lifecycle_create_status_read_kill() {
        let reg = registry();
        let (program, args): (&str, Vec<&str>) = if cfg!(windows) {
            ("ping", vec!["-n", "30", "127.0.0.1"])
        } else {
            ("sleep", vec!["30"])
        };
        let (id, log_path) = reg
            .spawn_job(
                program,
                &args,
                "sleeper",
                Some(std::env::temp_dir().as_path()),
            )
            .expect("spawn sleeper");
        assert!(id.starts_with("job-"), "id 形态: {id}");
        assert!(
            log_path.to_string_lossy().contains("jobs"),
            "日志在 jobs 目录: {log_path:?}"
        );

        let v = reg.status(&id).expect("running 状态可查");
        assert_eq!(v.status, "running", "30s 挂起作业即刻应 running");
        assert!(v.exit_code.is_none());

        let killed = reg.kill(&id).expect("kill 应成功");
        assert_eq!(killed.status, "completed", "族级终止后=completed");
        let idempotent = reg.kill(&id).expect("kill 幂等");
        assert_eq!(idempotent.status, "completed");
        assert!(reg.status("job-nonexistent").is_err(), "未知 id 拒识");
    }

    /// read 尾读:输出落盘且按字节帽截取
    #[test]
    fn read_tail_caps_bytes() {
        let reg = registry();
        let (id, _log) = reg
            .spawn_job(
                "git",
                &["--version"],
                "git --version",
                Some(std::env::temp_dir().as_path()),
            )
            .expect("spawn git --version");
        // 轮询至完成(git --version 秒级;10s 帽)
        let mut view = None;
        for _ in 0..100 {
            let v = reg.status(&id).expect("status ok");
            if v.status == "completed" {
                view = Some(v);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let v = view.expect("git --version 秒级完成");
        assert_eq!(v.exit_code, Some(0));
        let (v2, tail) = reg.read_tail(&id, DEFAULT_TAIL_BYTES).unwrap();
        assert_eq!(v2.status, "completed");
        assert!(
            tail.to_lowercase().contains("version"),
            "尾读应含版本行: {tail}"
        );
        // 1 字节帽=截取生效(尾读长度有界)
        let (_, tiny) = reg.read_tail(&id, 1).unwrap();
        assert!(tiny.chars().count() <= 4, "1 字节帽下尾读应有界: {tiny:?}");
    }
}
