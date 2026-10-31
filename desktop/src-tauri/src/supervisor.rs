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

use crate::health;
use crate::job::JobHandle;
use crate::orchestrator::{self, LaunchContext, Runtime};
use crate::shell_config::{Console, ResolvedPaths};

const CRASH_WINDOW: Duration = Duration::from_secs(60);
const CRASH_LIMIT: usize = 3;
const MONITOR_TICK: Duration = Duration::from_millis(300);
const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(5);
/// 执行体重生健康探测窗口（与编排器初始拉起同口径）
const EXECUTOR_HEALTH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, PartialEq)]
pub enum ChildKind {
    Engine,
    Serve,
    /// LLM 执行体：退出走单独重生路径（不拖累整组），崩溃超限仅停用 LLM 面
    LlmExecutor,
}

impl ChildKind {
    fn name(self) -> &'static str {
        match self {
            ChildKind::Engine => "engine",
            ChildKind::Serve => "serve",
            ChildKind::LlmExecutor => "llm-executor",
        }
    }
}

struct SupState {
    runtime: Option<Runtime>,
    crash_instants: Vec<Instant>,
    /// 执行体单独崩溃计数（与整组计数分账：执行体是可选组件，超限只停
    /// LLM 面，不进降级页）
    executor_crashes: Vec<Instant>,
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
    pub executor_token: String,
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
    #[allow(clippy::too_many_arguments)] // 九项输入均为编排必需，结构体化反增噪音
    pub fn new(
        paths: ResolvedPaths,
        console: Console,
        engine_extra_args: Vec<String>,
        serve_token: String,
        engine_token: String,
        executor_token: String,
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
            executor_token,
            version,
            job,
            window,
            nav_port,
            self_assigned,
            state: Mutex::new(SupState {
                runtime: None,
                crash_instants: Vec::new(),
                executor_crashes: Vec::new(),
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
            executor_token: &self.executor_token,
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

    /// 就绪：登记 runtime、按需导航工作台、启动监视。
    /// 粘滞端口配套（连接韧性）：仅当端口变化或曾离席（降级页把 nav_port
    /// 置 None）才重导航——同口组重启跳过 navigate，前端 WS 指数退避重连
    /// 直接命中，用户零整页刷新。
    fn on_running(self: &Arc<Self>, rt: Runtime) {
        let need_nav = {
            let mut np = self.nav_port.write().unwrap();
            let need = *np != Some(rt.serve_port);
            *np = Some(rt.serve_port);
            need
        };
        if need_nav {
            let url = format!("http://127.0.0.1:{}/", rt.serve_port);
            if let Ok(u) = Url::parse(&url) {
                let _ = self.window.navigate(u);
            }
        }
        let (epoch, watch_executor) = {
            let mut st = self.state.lock().unwrap();
            st.monitor_epoch += 1;
            let watch_executor = rt.ai_plugin.is_some();
            st.runtime = Some(rt);
            (st.monitor_epoch, watch_executor)
        };
        self.monitor(ChildKind::Engine, epoch);
        self.monitor(ChildKind::Serve, epoch);
        if watch_executor {
            self.monitor(ChildKind::LlmExecutor, epoch);
        }
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
                    ChildKind::LlmExecutor => rt.ai_plugin.as_mut(),
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
                if kind == ChildKind::LlmExecutor {
                    sup.handle_executor_exit();
                } else {
                    sup.handle_exit(kind);
                }
                break;
            }
        });
    }

    /// 执行体退出：单独重生（复用原端口，serve 端点 env 不变）；崩溃超限
    /// 只停用 LLM 面（LLM 调用如实报错），不进降级页、不拖累整组。
    fn handle_executor_exit(self: &Arc<Self>) {
        let respawn_ok = {
            let mut st = self.state.lock().unwrap();
            if st.shutting_down || st.degraded || st.runtime.is_none() {
                return;
            }
            // 清理退出的句柄，保留端口记录（重生复用）
            if let Some(rt) = st.runtime.as_mut() {
                rt.ai_plugin = None;
            }
            st.executor_crashes.retain(|t| t.elapsed() < CRASH_WINDOW);
            st.executor_crashes.push(Instant::now());
            st.executor_crashes.len() < CRASH_LIMIT
        };
        if respawn_ok {
            self.log_line("[guard] llm-executor exited; solo respawn");
            self.spawn_executor_restart();
        } else {
            self.log_line(
                "[guard] llm-executor crash limit reached; LLM disabled until services restart",
            );
        }
    }

    fn spawn_executor_restart(self: &Arc<Self>) {
        let sup = self.clone();
        std::thread::spawn(move || {
            let (port, engine_port) = {
                let st = sup.state.lock().unwrap();
                match st.runtime.as_ref() {
                    Some(rt) => match rt.ai_plugin_port {
                        Some(p) => (p, rt.engine_port),
                        // 执行体本就未启用（监视线程不会注册此态，防御分支）
                        None => return,
                    },
                    None => return,
                }
            };
            let ctx = sup.context();
            let result = match open_shell_log(&sup.paths) {
                Ok(mut log) => {
                    orchestrator::restart_llm_executor(&ctx, port, engine_port, &mut log)
                }
                Err(_) => orchestrator::restart_llm_executor(
                    &ctx,
                    port,
                    engine_port,
                    &mut std::io::sink(),
                ),
            };
            match result {
                Ok(mut child) => {
                    if !health::wait_healthy(port, "/health", EXECUTOR_HEALTH_TIMEOUT) {
                        sup.log_line("[guard] llm-executor respawn health timeout; LLM disabled");
                        let _ = child.kill();
                        return;
                    }
                    let rearm = {
                        let mut st = sup.state.lock().unwrap();
                        // 经 MutexGuard Deref 的字段访问不享字段不相交豁免：
                        // 先离线判定，再单点写回，match 臂内不再碰 st
                        if st.shutting_down || st.degraded || st.runtime.is_none() {
                            let _ = child.kill();
                            false
                        } else {
                            st.runtime.as_mut().unwrap().ai_plugin = Some(child);
                            st.executor_crashes.clear();
                            true
                        }
                    };
                    if rearm {
                        sup.log_line(&format!(
                            "[guard] llm-executor respawn healthy on port {port}"
                        ));
                        let epoch = sup.state.lock().unwrap().monitor_epoch;
                        sup.monitor(ChildKind::LlmExecutor, epoch);
                    }
                }
                Err(e) => {
                    sup.log_line(&format!("[guard] llm-executor respawn failed: {e}"));
                }
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
                if let Some(c) = rt.ai_plugin.as_mut() {
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
                self.log_line(&format!(
                    "[guard] {} exited; auto group restart",
                    kind.name()
                ));
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
            st.executor_crashes.clear();
            if let Some(rt) = st.runtime.as_mut() {
                if let Some(c) = rt.serve.as_mut() {
                    let _ = c.kill();
                }
                if let Some(c) = rt.engine.as_mut() {
                    let _ = c.kill();
                }
                if let Some(c) = rt.ai_plugin.as_mut() {
                    let _ = c.kill();
                }
            }
            st.runtime = None;
        }
        self.log_line("[guard] manual restart requested");
        self.spawn_restart();
    }

    /// 降级页导航（纯壳内资源，不经 serve）。离席标记：nav_port 置 None，
    /// 使恢复重启后 on_running 必重导航回工作台（粘滞同口也不跳过）。
    pub fn show_degrade(self: &Arc<Self>, msg: &str) {
        self.log_line(&format!("[guard] degrade: {msg}"));
        *self.nav_port.write().unwrap() = None;
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
            orchestrator::graceful_stop(
                &mut rt.engine,
                &mut rt.serve,
                &mut rt.ai_plugin,
                &mut rt.codegraph,
                &mut rt.wasm_host,
                &mut rt.inspector,
                GRACEFUL_TIMEOUT,
            );
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
            orchestrator::graceful_stop(
                &mut rt.engine,
                &mut rt.serve,
                &mut rt.ai_plugin,
                &mut rt.codegraph,
                &mut rt.wasm_host,
                &mut rt.inspector,
                GRACEFUL_TIMEOUT,
            );
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
