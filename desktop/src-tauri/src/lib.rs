// 桌面壳入口：窗口 + 编排 + token 注入 + 退出树杀 + 桌面体验（M3）。
//
// 启动序列：
//   1. 读壳配置（shell.toml，缺省全缺省）+ 窗口尺寸记忆（window.json）
//   2. 单实例：二次启动唤起既有窗口（tauri-plugin-single-instance）
//   3. 建主窗口（1280×800 缺省/960×600 最小/尺寸位置记忆）加载内置加载页；
//      initialization_script 预置工作台 token（每次文档创建都执行）
//   4. 守护线程编排：engine → 健康探测 → serve → 健康探测 → runtime.json；
//      之后子进程监视：意外退出 → 60s 滑窗 3 次退避内整组重启，超限降级页
//   5. 托盘：打开主窗 / 重启服务 / 打开数据目录 / 退出
//   6. 关窗缺省最小化到托盘（shell.toml [ui].close_directly 可配直接退出）；
//      退出走两段式优雅停（Ctrl+C → 5s 超时强杀）+ KILL_ON_JOB_CLOSE 兜底

mod health;
mod job;
mod orchestrator;
mod shell_config;
mod supervisor;
mod update;

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use tauri::{
    Manager, RunEvent, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_single_instance::init as single_instance_init;

use supervisor::Supervisor;

type NavPort = Arc<RwLock<Option<u16>>>;
type SupHandle = Arc<Supervisor>;

/// 仅保留 RFC 3986 unreserved 字符，其余 percent-encode（query 值编码）。
pub(crate) fn percent_encode(s: &str) -> String {
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

fn open_in_explorer(dir: &std::path::Path) {
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
}

// ---- 窗口尺寸记忆（%APPDATA%\evo-agent-desktop\window.json） ----

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct WindowState {
    w: f64,
    h: f64,
    x: f64,
    y: f64,
}

fn window_state_path(paths: &shell_config::ResolvedPaths) -> PathBuf {
    paths.app_config_dir.join("window.json")
}

fn load_window_state(paths: &shell_config::ResolvedPaths) -> Option<WindowState> {
    let raw = std::fs::read_to_string(window_state_path(paths)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_window_state(paths: &shell_config::ResolvedPaths, win: &WebviewWindow) {
    if !win.is_visible().unwrap_or(false) {
        return; // 隐藏态（托盘最小化）无有效几何，跳过
    }
    let state = WindowState {
        w: win.outer_size().map(|s| s.width as f64).unwrap_or(1280.0),
        h: win.outer_size().map(|s| s.height as f64).unwrap_or(800.0),
        x: win.outer_position().map(|p| p.x as f64).unwrap_or(0.0),
        y: win.outer_position().map(|p| p.y as f64).unwrap_or(0.0),
    };
    if let Some(dir) = window_state_path(paths).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(&state) {
        let _ = std::fs::write(window_state_path(paths), json);
    }
}

// ---- Tauri commands（降级页按钮 / 托盘复用同路径） ----

#[tauri::command]
fn restart_services(sup: tauri::State<'_, SupHandle>) -> Result<(), String> {
    sup.manual_restart();
    Ok(())
}

#[tauri::command]
fn open_logs_dir(sup: tauri::State<'_, SupHandle>) -> Result<(), String> {
    open_in_explorer(&sup.paths.logs_dir);
    Ok(())
}

/// 手动检查更新（托盘菜单/命令同路径）：独立线程执行，避免阻塞调用线程
/// （确认对话框为 blocking 原生弹窗，不能在主线程）
#[tauri::command]
fn check_updates(app: tauri::AppHandle, sup: tauri::State<'_, SupHandle>) {
    let sup = sup.inner().clone();
    std::thread::spawn(move || update::run_update_flow(&app, &sup, true));
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = shell_config::load();
    let version = env!("CARGO_PKG_VERSION").to_string();
    // 路径解析失败直接退（无窗口必要；错误可经 shell.toml 修复后重试）
    let paths = shell_config::resolve(&cfg)?;
    let close_directly = cfg.ui.close_directly;
    // console 通道生效配置 = 产物在位探测结果（W6：在位即启用，见 shell_config）
    let console = paths.console.clone();
    let engine_extra_args = cfg.engine.extra_args.clone();

    let app = tauri::Builder::default()
        // 单实例必须最先注册：二次启动唤起既有窗口
        .plugin(single_instance_init(move |app, _args, _cwd| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
        }))
        // 更新链三插件（Rust 侧调用，capability 仅作 IPC 面兜底）
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            restart_services,
            open_logs_dir,
            check_updates
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let nav_port: NavPort = Arc::new(RwLock::new(None));
            let serve_token = orchestrator::gen_token();
            let engine_token = orchestrator::gen_token();

            // 窗口尺寸记忆：缺省 1280×800，最小 960×600
            let mut builder = WebviewWindowBuilder::new(
                &handle,
                "main",
                WebviewUrl::App("index.html".into()),
            )
            .title("evo-agent")
            .min_inner_size(960.0, 600.0);
            builder = match load_window_state(&paths) {
                Some(ws) if ws.w >= 960.0 && ws.h >= 600.0 => {
                    builder.inner_size(ws.w, ws.h).position(ws.x, ws.y)
                }
                _ => builder.inner_size(1280.0, 800.0),
            };
            let win = builder
                .initialization_script(init_script(&serve_token))
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

            // 守护器：job 创建 + 自收编在此完成
            let sup = Supervisor::new(
                paths.clone(),
                console,
                engine_extra_args,
                serve_token,
                engine_token,
                version,
                win.clone(),
                nav_port.clone(),
            )?;
            app.manage(sup.clone());

            // 关窗行为：缺省最小化到托盘；[ui].close_directly = true 时直接退出
            let sup_for_close = sup.clone();
            let paths_for_close = paths.clone();
            let win_for_close = win.clone();
            win.on_window_event(move |event| {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    save_window_state(&paths_for_close, &win_for_close);
                    if close_directly {
                        sup_for_close.shutdown();
                    } else {
                        let _ = win_for_close.hide();
                    }
                }
            });

            // 托盘：打开主窗 / 重启服务 / 打开数据目录 / 检查更新 / 退出
            use tauri::menu::{MenuBuilder, MenuItem};
            let mk = |id: &str, label: &str| {
                MenuItem::with_id(app, id, label, true, None::<&str>)
            };
            let menu = MenuBuilder::new(app)
                .item(&mk("open", "打开主窗")?)
                .item(&mk("restart", "重启服务")?)
                .item(&mk("data", "打开数据目录")?)
                .item(&mk("check-update", "检查更新")?)
                .separator()
                .item(&mk("quit", "退出")?)
                .build()?;
            let sup_for_tray = sup.clone();
            tauri::tray::TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("evo-agent")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(move |app, event| {
                    let sup = &sup_for_tray;
                    match event.id().as_ref() {
                        "open" => {
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = w.show();
                                let _ = w.unminimize();
                                let _ = w.set_focus();
                            }
                        }
                        "restart" => {
                            sup.manual_restart();
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                        "data" => open_in_explorer(&sup.paths.data_dir),
                        "check-update" => {
                            let app_h = app.clone();
                            let sup = sup_for_tray.clone();
                            std::thread::spawn(move || {
                                update::run_update_flow(&app_h, &sup, true)
                            });
                        }
                        "quit" => sup.shutdown(),
                        _ => {}
                    }
                })
                .build(app)?;

            sup.start();

            // 自动更新周期检查（M4）：延迟 90s 首查（避让启动高峰），
            // 之后按 [update].check_interval_hours 周期；enabled=false 不启动。
            // 全 fail-soft，任何异常不影响壳主功能。
            if cfg.update.enabled {
                let app_for_update = app.handle().clone();
                let sup_for_update = sup.clone();
                let interval_hours = cfg.update.check_interval_hours.max(1);
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(90));
                    loop {
                        update::run_update_flow(&app_for_update, &sup_for_update, false);
                        std::thread::sleep(std::time::Duration::from_secs(
                            interval_hours * 3600,
                        ));
                    }
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!())?;

    // 兜底：任何未被拦截的退出请求同样走优雅停序列（shutting_down 幂等护栏）
    app.run(move |app, event| {
        if let RunEvent::ExitRequested { .. } = event {
            if let Some(sup) = app.try_state::<SupHandle>() {
                sup.shutdown();
            }
        }
    });
    Ok(())
}
