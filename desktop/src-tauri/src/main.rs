// 发布构建不挂控制台窗口；调试构建保留 stderr 便于排障。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(e) = evo_agent_desktop::run() {
        eprintln!("desktop shell failed: {e}");
        std::process::exit(1);
    }
}
