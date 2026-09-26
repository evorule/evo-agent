//! console 审计页 sidecar 自动拉起(纯展示层功能,零引擎/协议触碰)。
//!
//! 背景:工作台审计 tab 的「在审计页查看」深链指向 console-cloud 审计页
//! (独立 SvelteKit dev server,缺省 `127.0.0.1:5174`)。该服务未运行时深链
//! 为死链(首验实测)。本模块提供配置化自动拉起:`workbench.console_dir`
//! 配置 console-cloud 仓目录后,serve 启动期探测端口,未监听则以子进程拉起
//! vite dev;**fail-soft**——目录无效/依赖缺失/端口占用/拉起失败仅告警,
//! 绝不阻断 serve 主功能;缺省不配置 = 零副作用(公开部署零耦合)。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use tokio::net::TcpListener;

/// console dev server 缺省端口(与 console-cloud vite.config 端口一致)
pub const DEFAULT_CONSOLE_PORT: u16 = 5174;

/// 探测 `127.0.0.1:port` 是否已有服务监听(绑定失败即已被占用)
pub async fn is_port_listening(port: u16) -> bool {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    TcpListener::bind(addr).await.is_err()
}

/// 校验 console 目录具备启动条件,返回 vite 入口路径
/// (`<dir>/node_modules/vite/bin/vite.js` 必须存在,即已 npm install)
pub fn vite_entry(dir: &Path) -> Option<PathBuf> {
    let entry = dir
        .join("node_modules")
        .join("vite")
        .join("bin")
        .join("vite.js");
    entry.is_file().then_some(entry)
}

/// 确保 console dev server 在运行(幂等:端口已监听则跳过)。
///
/// 返回 `(是否执行了拉起, 说明信息)` 供启动日志。子进程故意脱离 serve
/// 生命周期(dev server 为独立服务语义):serve 退出后 console 继续存活,
/// 下次 serve 启动探测到端口已监听即跳过,天然幂等。
///
/// `log_file`:Some 时子进程 stdout/stderr 追加写入该文件(vite 启动失败
/// 的诊断证据);None 时丢弃。
pub async fn ensure_console_dev(
    console_dir: &str,
    port: u16,
    log_file: Option<&Path>,
) -> (bool, String) {
    if is_port_listening(port).await {
        return (false, format!("port {port} already listening, skip"));
    }
    let dir = Path::new(console_dir);
    if !dir.is_dir() {
        return (
            false,
            format!("console_dir not found: {console_dir} (check workbench.console_dir)"),
        );
    }
    let Some(entry) = vite_entry(dir) else {
        return (
            false,
            format!(
                "vite entry missing under {console_dir} (run: npm --prefix {console_dir} install)"
            ),
        );
    };
    // 子进程输出:落日志文件(诊断)或丢弃;绝不 inherit(serve 为后台进程)
    let (stdout, stderr) = match log_file {
        Some(log) => {
            if let Some(parent) = log.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let open = || {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log)
            };
            match (open(), open()) {
                (Ok(o), Ok(e)) => (std::process::Stdio::from(o), std::process::Stdio::from(e)),
                _ => (std::process::Stdio::null(), std::process::Stdio::null()),
            }
        }
        None => (std::process::Stdio::null(), std::process::Stdio::null()),
    };
    let port_str = port.to_string();
    match tokio::process::Command::new("node")
        .arg(&entry)
        .args([
            "dev",
            "--port",
            &port_str,
            "--strictPort",
            "--host",
            "127.0.0.1",
        ])
        .current_dir(dir)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
    {
        Ok(child) => (
            true,
            format!(
                "vite dev spawned (pid {}) from {console_dir}",
                child.id().map(|i| i.to_string()).unwrap_or_default()
            ),
        ),
        Err(e) => (false, format!("spawn failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_is_port_listening_true_when_bound() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(is_port_listening(port).await);
    }

    #[tokio::test]
    async fn test_is_port_listening_false_when_free() {
        // 系统分配临时端口后立即释放,再探测应为空闲(极小概率竞态,命中则放弃断言)
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        if is_port_listening(port).await {
            return;
        }
        assert!(!is_port_listening(port).await);
    }

    #[test]
    fn test_vite_entry_missing_dir() {
        assert!(vite_entry(Path::new("Z:/definitely/not/a/dir")).is_none());
    }

    #[tokio::test]
    async fn test_ensure_console_dev_missing_dir_fail_soft() {
        let (spawned, msg) = ensure_console_dev("Z:/definitely/not/a/dir", 0, None).await;
        assert!(!spawned);
        assert!(msg.contains("console_dir not found"));
    }
}
