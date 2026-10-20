// 自动更新链（M4）：检查 → 通知确认 → 下载验签 → NSIS 覆盖安装。
//
// - 端点来源：shell.toml [update].endpoints 非空时完全覆盖，空时用
//   tauri.conf.json plugins.updater.endpoints（=GitHub Releases 稳定
//   latest 资产 URL，随最新发布自动追踪；Gitee 为人工下载主渠道，更新器
//   不可达时 fail-soft 落手动下载）
// - 全流程 fail-soft：任何失败只记 shell.log，壳主功能不受影响；
//   手动检查场景另加原生报错对话框
// - 版本比较：插件 check() 内置 semver 比较（remote > current 才返回
//   Some(Update)，否则 None）——K3 源码定谳，壳侧不重复比较
// - 安装语义（K2 源码定谳）：install 内部顺序 = 解压到 %TEMP% → on_before_exit
//   回调（此处优雅停编排器）→ ShellExecuteW 启动 NSIS（缺省 /P passive 进度条
//   + /UPDATE + /R 装完自动重启新版）→ exit(0) 本进程

use std::sync::Arc;
use std::time::Duration;

use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::UpdaterExt;

use crate::supervisor::Supervisor;

const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

fn log_line(paths: &std::path::Path, msg: &str) {
    if let Some(dir) = paths.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths)
    {
        use std::io::Write;
        let _ = writeln!(f, "{msg}");
    }
}

fn set_tray_tooltip(app: &AppHandle, text: &str) {
    if let Some(tray) = app.tray_by_id("main-tray") {
        let _ = tray.set_tooltip(Some(text));
    }
}

/// 更新流程统一入口（自动周期检查与手动检查共用）。
/// manual=true 时失败/无新版给用户原生反馈；manual=false 全静默。
pub fn run_update_flow(app: &AppHandle, sup: &Arc<Supervisor>, manual: bool) {
    if let Err(e) = run_inner(app, sup, manual) {
        log_line(
            &sup.paths.logs_dir.join("shell.log"),
            &format!("[update] error: {e}"),
        );
        if manual {
            app.dialog()
                .message(format!("检查更新失败：{e}"))
                .title("evo-agent 更新")
                .blocking_show();
        }
    }
}

fn run_inner(app: &AppHandle, sup: &Arc<Supervisor>, manual: bool) -> Result<(), String> {
    let shell_log = sup.paths.logs_dir.join("shell.log");
    let cfg = crate::shell_config::load().update;
    if !cfg.enabled {
        if manual {
            app.dialog()
                .message("自动更新已在 shell.toml [update] 中禁用。")
                .title("evo-agent 更新")
                .blocking_show();
        }
        return Ok(());
    }

    let mut builder = app.updater_builder();
    if !cfg.endpoints.is_empty() {
        let mut urls = Vec::new();
        for raw in &cfg.endpoints {
            let url: tauri::Url = raw
                .parse()
                .map_err(|_| format!("invalid endpoint url: {raw}"))?;
            urls.push(url);
        }
        builder = builder
            .endpoints(urls)
            .map_err(|e| format!("set endpoints: {e}"))?;
    }
    // NSIS 安装器启动前、本进程退出前的优雅停钩子（不退出、杀子进程、
    // 防守护线程重启；进程结束由 KILL_ON_JOB_CLOSE 兜底）
    let sup_for_exit = sup.clone();
    builder = builder.on_before_exit(Box::new(move || sup_for_exit.stop_for_update()));
    builder = builder.timeout(CHECK_TIMEOUT);
    let updater = builder.build().map_err(|e| format!("init updater: {e}"))?;

    // 检查：None = 无新版（插件内置 semver 比较已拒绝 ≤ 当前版本）
    let update = tauri::async_runtime::block_on(updater.check())
        .map_err(|e| format!("check failed: {e}"))?;
    let Some(update) = update else {
        log_line(&shell_log, "[update] up-to-date");
        if manual {
            app.dialog()
                .message("当前已是最新版本。")
                .title("evo-agent 更新")
                .blocking_show();
        }
        return Ok(());
    };

    let remote = &update.version;
    let notes = update.body.clone().unwrap_or_default();
    let notes = notes.trim();
    let notes_line = if notes.is_empty() {
        String::new()
    } else {
        format!("\n\n更新说明：{}", notes)
    };
    log_line(
        &shell_log,
        &format!("[update] new version {remote} available"),
    );

    if !manual {
        // 自动场景先发托盘气泡（fail-soft），随后原生确认对话框
        let _ = app
            .notification()
            .builder()
            .title("evo-agent 有新版本")
            .body(format!("发现新版 v{remote}，请在弹窗中确认是否安装。"))
            .show();
    }
    let confirmed = app
        .dialog()
        .message(format!("发现新版 v{remote}：{notes_line}是否下载并安装？"))
        .title("evo-agent 更新")
        .buttons(MessageDialogButtons::OkCancelCustom(
            "立即更新".into(),
            "以后再说".into(),
        ))
        .blocking_show();
    if !confirmed {
        log_line(&shell_log, "[update] user declined");
        return Ok(());
    }

    set_tray_tooltip(app, "evo-agent 正在下载更新…");
    log_line(&shell_log, &format!("[update] installing v{remote}"));
    // 下载 + 验签 + 启动 NSIS 安装器 + 退出本进程（on_before_exit 已挂优雅停）；
    // 成功路径不会返回，走到 Err 分支说明下载/启动安装器失败
    let result =
        tauri::async_runtime::block_on(update.download_and_install(|_chunk, _total| {}, || {}));
    // 失败恢复托盘 tooltip，报错留痕（壳照常运行）
    set_tray_tooltip(app, "evo-agent");
    result.map_err(|e| format!("download/install failed: {e}"))
}
