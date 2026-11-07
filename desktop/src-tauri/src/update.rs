// 自动更新链（M4）：检查 → 通知确认 → 下载验签 → NSIS 覆盖安装。
//
// - 端点来源：shell.toml [update].endpoints 非空时完全覆盖，空时用
//   tauri.conf.json plugins.updater.endpoints（=GitHub Releases 稳定
//   latest 资产 URL，随最新发布自动追踪；GitHub 为唯一发布渠道，更新器
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

/// 版本检查超时：latest.json 是 KB 级小请求，30s 足够；超时即放弃本轮
/// 检查（fail-soft 落 shell.log）。由调用点 [`tokio::time::timeout`] 单独
/// 施加——插件 builder 的 timeout 语义是 check 与 download 共用一把
/// reqwest 总请求超时，若把 30s 直接给 builder，数十 MB 级安装包的
/// 下载在慢速直连下恒超时（下载失败的唯一痕迹只有一行 shell.log）。
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// 下载+安装总超时：安装包数十 MB 级，须容忍慢速直连（GitHub 下载常态
/// 数分钟到十几分钟）；30 分钟覆盖到 ~30KB/s 持续带宽。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

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
    builder = builder.on_before_exit(Box::new(move || {
        // 覆盖安装发生前，把部署副本的部署侧扩展块 stash 进壳配置目录
        // （装机会外幸存），下次启动由 prepare_workspace 幂等合回——部署侧
        // 扩展不随模板覆盖丢失。失败 fail-soft（shell.log 留痕，不阻断更新）。
        let stash = || -> std::io::Result<()> {
            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(sup_for_exit.paths.logs_dir.join("shell.log"))?;
            use std::io::Write;
            match crate::orchestrator::stash_deploy_extensions(
                &sup_for_exit.paths.evo_agent_home.join("evo-agent.toml"),
                &sup_for_exit
                    .paths
                    .app_config_dir
                    .join("deploy-extensions.bak"),
            ) {
                Ok(true) => writeln!(log, "[update] deployed extension block stashed pre-install"),
                Ok(false) => Ok(()),
                Err(e) => writeln!(log, "[update] stash deployed extensions failed: {e}"),
            }
        };
        if let Err(e) = stash() {
            log_line(
                &sup_for_exit.paths.logs_dir.join("shell.log"),
                &format!("[update] stash extensions io error: {e}"),
            );
        }
        sup_for_exit.stop_for_update();
    }));
    // timeout 放开到下载级（见 DOWNLOAD_TIMEOUT 常量注释）；检查侧的 30s
    // 由下方调用点 tokio::time::timeout 单独包裹恢复
    builder = builder.timeout(DOWNLOAD_TIMEOUT);
    let updater = builder.build().map_err(|e| format!("init updater: {e}"))?;

    // 检查：None = 无新版（插件内置 semver 比较已拒绝 ≤ 当前版本）。
    // CHECK_TIMEOUT 在调用点施加（下载级 builder 超时不反噬检查响应性）
    let checked = tauri::async_runtime::block_on(async {
        tokio::time::timeout(CHECK_TIMEOUT, updater.check()).await
    });
    let update = match checked {
        Ok(inner) => inner.map_err(|e| format!("check failed: {e}"))?,
        Err(_) => {
            return Err(format!(
                "check timed out after {}s",
                CHECK_TIMEOUT.as_secs()
            ));
        }
    };
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

#[cfg(test)]
mod update_timeout_guard_tests {
    use super::{CHECK_TIMEOUT, DOWNLOAD_TIMEOUT};

    #[test]
    fn check_timeout_must_stay_below_download_timeout() {
        // 历史缺陷：30s 总超时同时作用 check 与 download（插件共用一把
        // reqwest 总请求超时），数十 MB 级安装包在慢速直连下下载恒超时。
        // 检查侧超时必须在调用点单独包裹且远小于下载侧预算；下载侧预算
        // 须覆盖慢速直连（<30KB/s 持续带宽 × 15 分钟以上）。
        assert!(
            CHECK_TIMEOUT < DOWNLOAD_TIMEOUT,
            "check timeout must be applied at the call site, below the download budget"
        );
        assert!(
            DOWNLOAD_TIMEOUT.as_secs() >= 15 * 60,
            "download budget must cover slow direct connections"
        );
    }
}

#[cfg(test)]
mod installer_hooks_guard_tests {
    // The NSIS installer rescue hook (deployed toml migration backup) is
    // wired via tauri.conf.json bundle.windows.nsis.installerHooks and runs
    // PREINSTALL - before the installer copies the shipped resources tree
    // over the deployed copy (this includes silent /UPDATE upgrades started
    // by the in-app updater). These assertions keep the wiring and the hook
    // body from being silently unwired, which would reintroduce silent loss
    // of user-deployed agent configuration on upgrades.

    #[test]
    fn installer_hooks_wired_and_rescue_copy_present() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
        let conf = std::fs::read_to_string(manifest).expect("tauri.conf.json readable");
        assert!(
            conf.contains("installerHooks"),
            "tauri.conf.json must wire bundle.windows.nsis.installerHooks"
        );

        let hooks = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("installer-hooks.nsi");
        let body = std::fs::read_to_string(hooks).expect("installer-hooks.nsi readable");
        assert!(
            body.contains("NSIS_HOOK_PREINSTALL"),
            "PREINSTALL hook must be defined"
        );
        assert!(
            body.contains("resources") && body.contains("evo-agent.toml"),
            "hook must reference the deployed toml source path"
        );
        assert!(
            body.contains("deployed-toml-migration-backup.toml"),
            "hook must write the migration backup copy"
        );
        assert!(
            body.contains("APPDATA"),
            "backup must land outside the install directory so it survives reinstalls"
        );
    }
}

#[cfg(test)]
mod uninstall_hooks_guard_tests {
    // The PREUNINSTALL hook backs up the deployed toml (covering manual
    // uninstalls, where the app-level exit stash cannot run) and removes
    // runtime-created leftovers under the resources tree so no non-empty
    // directories survive the uninstall. These assertions pin the macro.

    #[test]
    fn uninstall_hooks_backup_and_cleanup_present() {
        let hooks = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("installer-hooks.nsi");
        let body = std::fs::read_to_string(&hooks).expect("installer-hooks.nsi readable");
        assert!(
            body.contains("NSIS_HOOK_PREUNINSTALL"),
            "PREUNINSTALL hook must be defined"
        );
        assert!(
            body.contains("deployed-toml-uninstall-backup.toml"),
            "hook must back up the deployed toml on uninstall"
        );
        assert!(
            body.contains("resources\\console"),
            "hook must clean the console leftovers"
        );
        assert!(
            body.contains("resources\\evo-agent"),
            "hook must clean the evo-agent resources leftovers"
        );
    }
}
