// 崩溃守护与生命周期治理（M3）。
//
// - 监视：300ms 轮询 try_wait 两个子进程（monitor_epoch 隔离旧监视线程）
// - 退避：60s 滑窗内崩溃 ≥3 次 → 停止自动重启进降级页；未超限 → 整组重启
//   （serve/engine 任一退出，另一伴生进程一并终止后全组重拉，避免半连通态）
// - 降级页：状态信息 + 重启服务按钮（手动重启清零计数，见决策 4.2）
// - 退出：两段式优雅停（Ctrl+C 尽力送达 → 5s 超时强杀）→ 删握手文件 →
//   cleanup_before_exit（KILL_ON_JOB_CLOSE 兜底整树回收）

use std::io::Write;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use tauri::{Manager, Url, WebviewWindow};

use crate::job::JobHandle;
use crate::orchestrator::{self, LaunchContext, Runtime};
use crate::shell_config::{Console, ResolvedPaths};

const CRASH_WINDOW: Duration = Duration::from_secs(60);
const CRASH_LIMIT: usize = 3;
const MONITOR_TICK: Duration = Duration::from_millis(300);
const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq)]
pub enum ChildKind {
    Engine,
    Serve,
}

impl ChildKind {
    fn name(self) -> &'static str {
        match self {
            ChildKind::Engine => "engine",
            ChildKind::Serve => "serve",
        }
    }
}

struct SupState {
    runtime: Option<Runtime>,
    crash_instants: Vec<Instant>,
    degraded: bool,
    shutting_down: bool,
    /// 每次整组重启自增；旧监视线程见到 epoch 不匹配即退出
    monitor_epoch: u64,
}

pub struct Supervisor {
    pub paths: ResolvedPaths,
    pub console: Console,
    pub engine_extra_args: Vec<String>,
    pub serve_token: String,
    pub engine_token: String,
    pub version: String,
    pub job: Arc<JobHandle>,
    pub window: WebviewWindow,
    pub nav_port: Arc<RwLock<Option<u16>>>,
    self_assigned: bool,
    state: Mutex<SupState>,
}

pub enum Decision {
    Restart,
    Degrade,
}

impl Supervisor {
    #[allow(clippy::too_many_arguments)] // 八项输入均为编排必需，结构体化反增噪音
    pub fn new(
        paths: ResolvedPaths,
        console: Console,
        engine_extra_args: Vec<String>,
        serve_token: String,
        engine_token: String,
        version: String,
        window: WebviewWindow,
        nav_port: Arc<RwLock<Option<u16>>>,
    ) -> Result<Arc<Self>, String> {
        let job = Arc::new(JobHandle::create()?);
        // 壳不自收编（M4.1 教训）：updater install 时 ShellExecuteW 启动的
        // NSIS 安装器同为壳子进程，若壳自收编，install 末尾壳 exit(0) 会经
        // KILL_ON_JOB_CLOSE 顺杀安装器，覆盖安装被中断。改为 serve/engine
        // 在 spawn 点逐个收编（orchestrator）；杀壳/崩溃时 job 句柄随壳进程
        // 关闭，服务树回收语义不变。
        let self_assigned = false;
        if let Ok(mut log) = open_shell_log(&paths) {
            writeln!(log, "[guard] job created (self_assigned={self_assigned})").ok();
        }
        Ok(Arc::new(Self {
            paths,
            console,
            engine_extra_args,
            serve_token,
            engine_token,
            version,
            job,
            window,
            nav_port,
            self_assigned,
            state: Mutex::new(SupState {
                runtime: None,
                crash_instants: Vec::new(),
                degraded: false,
                shutting_down: false,
                monitor_epoch: 0,
            }),
        }))
    }

    fn context(&self) -> LaunchContext<'_> {
        LaunchContext {
            paths: &self.paths,
            console: &self.console,
            engine_extra_args: &self.engine_extra_args,
            serve_token: &self.serve_token,
            engine_token: &self.engine_token,
            version: &self.version,
            job: &self.job,
            self_assigned: self.self_assigned,
        }
    }

    fn log_line(&self, line: &str) {
        if let Ok(mut log) = open_shell_log(&self.paths) {
            writeln!(log, "{line}").ok();
        }
    }

    /// 初始启动（后台线程；成功进工作台，失败进降级页）。
    pub fn start(self: &Arc<Self>) {
        let sup = self.clone();
        std::thread::spawn(move || {
            let ctx = sup.context();
            let result = match open_shell_log(&sup.paths) {
                Ok(mut log) => orchestrator::launch(&ctx, &mut log),
                Err(_) => orchestrator::launch(&ctx, &mut std::io::sink()),
            };
            match result {
                Ok(rt) => sup.on_running(rt),
                Err(e) => sup.show_degrade(&e),
            }
        });
    }

    /// 就绪：登记 runtime、导航工作台、启动监视。
    fn on_running(self: &Arc<Self>, rt: Runtime) {
        *self.nav_port.write().unwrap() = Some(rt.serve_port);
        let url = format!("http://127.0.0.1:{}/", rt.serve_port);
        if let Ok(u) = Url::parse(&url) {
            let _ = self.window.navigate(u);
        }
        let epoch = {
            let mut st = self.state.lock().unwrap();
            st.monitor_epoch += 1;
            st.runtime = Some(rt);
            st.monitor_epoch
        };
        self.monitor(ChildKind::Engine, epoch);
        self.monitor(ChildKind::Serve, epoch);
    }

    /// 子进程监视：退出即触发守护决策。
    fn monitor(self: &Arc<Self>, kind: ChildKind, epoch: u64) {
        let sup = self.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(MONITOR_TICK);
            let exited = {
                let mut st = sup.state.lock().unwrap();
                if st.shutting_down || st.degraded || st.monitor_epoch != epoch {
                    break;
                }
                let rt = match st.runtime.as_mut() {
                    Some(rt) => rt,
                    None => break,
                };
                let child = match kind {
                    ChildKind::Engine => rt.engine.as_mut(),
                    ChildKind::Serve => rt.serve.as_mut(),
                };
                match child {
                    Some(c) => match c.try_wait() {
                        Ok(Some(_)) => true,
                        Ok(None) => false,
                        Err(_) => true,
                    },
                    None => break,
                }
            };
            if exited {
                sup.handle_exit(kind);
                break;
            }
        });
    }

    fn handle_exit(self: &Arc<Self>, kind: ChildKind) {
        let decision = {
            let mut st = self.state.lock().unwrap();
            if st.shutting_down || st.degraded || st.runtime.is_none() {
                return;
            }
            // 整组重启：伴生进程一并终止，避免半连通态
            if let Some(rt) = st.runtime.as_mut() {
                if let Some(c) = rt.serve.as_mut() {
                    let _ = c.kill();
                }
                if let Some(c) = rt.engine.as_mut() {
                    let _ = c.kill();
                }
            }
            st.runtime = None;
            st.crash_instants.retain(|t| t.elapsed() < CRASH_WINDOW);
            st.crash_instants.push(Instant::now());
            if st.crash_instants.len() >= CRASH_LIMIT {
                st.degraded = true;
                Decision::Degrade
            } else {
                Decision::Restart
            }
        };
        match decision {
            Decision::Restart => {
                self.log_line(&format!("[guard] {} exited; auto group restart", kind.name()));
                self.spawn_restart();
            }
            Decision::Degrade => {
                self.log_line(&format!(
                    "[guard] {} exited; crash limit reached, degrading",
                    kind.name()
                ));
                self.show_degrade(&format!(
                    "{} 进程异常退出，60 秒内连续崩溃已达 {} 次，已停止自动重启。",
                    kind.name(),
                    CRASH_LIMIT
                ));
            }
        }
    }

    fn spawn_restart(self: &Arc<Self>) {
        let sup = self.clone();
        std::thread::spawn(move || {
            let ctx = sup.context();
            let result = match open_shell_log(&sup.paths) {
                Ok(mut log) => orchestrator::restart_services(&ctx, &mut log),
                Err(_) => orchestrator::restart_services(&ctx, &mut std::io::sink()),
            };
            match result {
                Ok(rt) => {
                    sup.log_line("[guard] group restart complete");
                    sup.on_running(rt);
                }
                Err(e) => {
                    sup.log_line(&format!("[guard] group restart failed: {e}"));
                    sup.state.lock().unwrap().degraded = true;
                    sup.show_degrade(&format!("服务重启失败：{e}"));
                }
            }
        });
    }

    /// 手动重启（托盘/降级页按钮）：清零崩溃计数后整组重启。
    pub fn manual_restart(self: &Arc<Self>) {
        {
            let mut st = self.state.lock().unwrap();
            if st.shutting_down {
                return;
            }
            st.degraded = false;
            st.crash_instants.clear();
            if let Some(rt) = st.runtime.as_mut() {
                if let Some(c) = rt.serve.as_mut() {
                    let _ = c.kill();
                }
                if let Some(c) = rt.engine.as_mut() {
                    let _ = c.kill();
                }
            }
            st.runtime = None;
        }
        self.log_line("[guard] manual restart requested");
        self.spawn_restart();
    }

    /// 降级页导航（纯壳内资源，不经 serve）。
    pub fn show_degrade(self: &Arc<Self>, msg: &str) {
        self.log_line(&format!("[guard] degrade: {msg}"));
        let url = format!(
            "http://tauri.localhost/error.html?mode=degrade&msg={}&logs={}",
            crate::percent_encode(msg),
            crate::percent_encode(&self.paths.logs_dir.display().to_string()),
        );
        if let Ok(u) = Url::parse(&url) {
            let _ = self.window.navigate(u);
        }
        let _ = self.window.show();
    }

    /// 更新安装前停服务：置 shutting_down 阻止守护线程重启 + 两段式优雅停 +
    /// 删握手文件，但不退出进程（updater install 接管；进程结束时
    /// KILL_ON_JOB_CLOSE 兜底）。与 shutdown 的区别仅在于不调 app.exit。
    pub fn stop_for_update(self: &Arc<Self>) {
        {
            let mut st = self.state.lock().unwrap();
            if st.shutting_down {
                return;
            }
            st.shutting_down = true;
        }
        self.log_line("[guard] stop for update (graceful two-phase)");
        let mut runtime = self.state.lock().unwrap().runtime.take();
        if let Some(rt) = runtime.as_mut() {
            orchestrator::graceful_stop(&mut rt.engine, &mut rt.serve, GRACEFUL_TIMEOUT);
        }
        let _ = std::fs::remove_file(self.paths.data_dir.join("runtime.json"));
    }

    /// 退出序列：两段式优雅停 → 删握手文件 → 清理退出。
    pub fn shutdown(self: &Arc<Self>) {
        {
            let mut st = self.state.lock().unwrap();
            if st.shutting_down {
                return;
            }
            st.shutting_down = true;
        }
        self.log_line("[guard] shutdown begin (graceful two-phase)");
        let mut runtime = self.state.lock().unwrap().runtime.take();
        if let Some(rt) = runtime.as_mut() {
            orchestrator::graceful_stop(&mut rt.engine, &mut rt.serve, GRACEFUL_TIMEOUT);
        }
        let _ = std::fs::remove_file(self.paths.data_dir.join("runtime.json"));
        // 正常清理退出（tauri 移除托盘等）；进程结束即关 job 句柄，
        // KILL_ON_JOB_CLOSE 兜底回收任何残存子进程
        let app = self.window.app_handle();
        app.cleanup_before_exit();
        app.exit(0);
    }
}

fn open_shell_log(paths: &ResolvedPaths) -> Result<std::fs::File, String> {
    std::fs::create_dir_all(&paths.logs_dir).map_err(|e| e.to_string())?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs_dir.join("shell.log"))
        .map_err(|e| format!("open shell.log: {e}"))
}
