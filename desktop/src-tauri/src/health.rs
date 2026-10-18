// 服务健康探测：极简 HTTP GET /health（std TcpStream，零额外依赖）。
//
// 指数退避 200ms 起步、2s 封顶，总超时由调用方给定（M1 设计 30s）。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// 单次探测：连接 + GET {path}，HTTP 200 即视为健康。
fn probe_once(port: u16, path: &str) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_millis(800)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 128];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return false,
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200")
}

/// 指数退避轮询直到健康或超时。
///
/// 路径按进程区分：evorule-server = /api/health，evo-agent serve = /health。
pub fn wait_healthy(port: u16, health_path: &str, total_timeout: Duration) -> bool {
    let start = Instant::now();
    let mut delay = 200u64;
    loop {
        if probe_once(port, health_path) {
            return true;
        }
        if start.elapsed() >= total_timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(delay));
        delay = (delay.saturating_mul(2)).min(2000);
    }
}
