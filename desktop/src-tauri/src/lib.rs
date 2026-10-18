// 桌面壳入口：窗口 + 编排 + token 注入 + 退出树杀。
//
// 启动序列（M1）：
//   1. 读壳配置（shell.toml，缺省全缺省）
//   2. 建主窗口加载内置加载页；initialization_script 预置工作台 token
//      （WebView2 每次文档创建都会执行，跨 origin 导航后仍生效）
//   3. 后台编排：engine → 健康探测 → serve → 健康探测 → runtime.json
//   4. 成功后 webview 导航到工作台（http://127.0.0.1:{serve_port}/）；
//      失败导航到内置降级页（错误信息 + 日志路径）
//   5. 退出时 terminate job（JobHandle Drop 关句柄，KILL_ON_JOB_CLOSE 兜底）

mod health;
mod job;
mod orchestrator;
mod shell_config;

use std::os::windows::process::CommandExt;
use std::sync::{Arc, Mutex, RwLock};

use tauri::{RunEvent, Url, WebviewUrl, WebviewWindowBuilder};

type NavPort = Arc<RwLock<Option<u16>>>;
type RuntimeState = Arc<Mutex<Option<orchestrator::Runtime>>>;

/// 仅保留 RFC 3986 unreserved 字符，其余 percent-encode（query 值编码）。
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 工作台 token 注入脚本（每次文档创建执行；JSON 字符串字面量即合法 JS 字面量）。
fn init_script(token: &str) -> String {
    let lit = serde_json::to_string(token).unwrap_or_default();
    format!(
        "try{{localStorage.setItem('evo_agent_token',{lit});}}catch(e){{}}"
    )
}

/// 导航白名单：壳内置页（tauri.localhost）与工作台（127.0.0.1:{serve_port}）；
/// 其余一律转系统浏览器打开并阻止 webview 内加载。
fn navigation_allowed(url: &Url, nav_port: &NavPort) -> bool {
    if url.host_str() == Some("tauri.localhost") {
        return true;
    }
    if url.host_str() == Some("127.0.0.1") {
        if let (Some(port), Some(allowed)) = (url.port(), *nav_port.read().unwrap()) {
            return port == allowed;
        }
    }
    false
}

fn open_in_system_browser(url: &str) {
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .creation_flags(orchestrator::CREATE_NO_WINDOW)
        .spawn();
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = shell_config::load();
    let version = env!("CARGO_PKG_VERSION").to_string();
    let runtime_state: RuntimeState = Arc::new(Mutex::new(None));

    let app = tauri::Builder::default()
        .setup({
            let runtime_state = runtime_state.clone();
            move |app| {
                let handle = app.handle().clone();
                let nav_port: NavPort = Arc::new(RwLock::new(None));
                let serve_token = orchestrator::gen_token();
                let engine_token = orchestrator::gen_token();

                let win = WebviewWindowBuilder::new(
                    &handle,
                    "main",
                    WebviewUrl::App("index.html".into()),
                )
                .title("evo-agent")
                .inner_size(1280.0, 800.0)
                .min_inner_size(960.0, 600.0)
                .initialization_script(&init_script(&serve_token))
                .on_navigation({
                    let nav_port = nav_port.clone();
                    move |url| {
                        if navigation_allowed(url, &nav_port) {
                            return true;
                        }
                        open_in_system_browser(url.as_str());
                        false
                    }
                })
                .build()?;

                let nav_port_for_task = nav_port.clone();
                let runtime_state_for_task = runtime_state.clone();
                let version_for_task = version.clone();
                std::thread::spawn(move || {
                    eprintln!("[shell] orchestration thread started");
                    // 路径解析失败也走降级页（配置可修后重试），不崩壳
                    let outcome = shell_config::resolve(&cfg).and_then(|paths| {
                        orchestrator::launch(orchestrator::LaunchInput {
                            paths: &paths,
                            console: &cfg.console,
                            engine_extra_args: &cfg.engine.extra_args,
                            serve_token: &serve_token,
                            engine_token: &engine_token,
                            version: &version_for_task,
                        })
                    });
                    match outcome {
                        Ok(rt) => {
                            *nav_port_for_task.write().unwrap() = Some(rt.serve_port);
                            let url = format!("http://127.0.0.1:{}/", rt.serve_port);
                            if let Ok(u) = Url::parse(&url) {
                                let _ = win.navigate(u);
                            }
                            *runtime_state_for_task.lock().unwrap() = Some(rt);
                        }
                        Err(e) => {
                            let logs = std::env::var("LOCALAPPDATA")
                                .map(|d| {
                                    std::path::PathBuf::from(d)
                                        .join("evo-agent-desktop")
                                        .join("logs")
                                        .display()
                                        .to_string()
                                })
                                .unwrap_or_default();
                            let url = format!(
                                "http://tauri.localhost/error.html?msg={}&logs={}",
                                percent_encode(&e),
                                percent_encode(&logs),
                            );
                            if let Ok(u) = Url::parse(&url) {
                                let _ = win.navigate(u);
                            }
                        }
                    }
                });
                Ok(())
            }
        })
        .build(tauri::generate_context!())?;

    let runtime_state_for_run = runtime_state.clone();
    app.run(move |_, event| {
        if let RunEvent::ExitRequested { .. } = event {
            if let Some(rt) = runtime_state_for_run.lock().unwrap().as_ref() {
                rt.shutdown();
            }
        }
    });
    Ok(())
}
