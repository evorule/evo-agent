// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! O-513 起服冒烟守卫：真实 `evo-agent serve` 二进制 + 真实 stdio MCP server。
//!
//! 守卫目标 = 本缺陷类三连：
//! 1. 「起不来」——配置可连通 MCP server 时 serve 永久挂死、端口不起
//!    （O-513：临时 runtime 桥接 drop 挂死；修复前 /health 探活必超时失败）；
//! 2. 「注册在案」——MCP 工具经 /mcp 暴露面 tools/list 可见；
//! 3. 「假活」——连接产物（stdio reader task）跨 runtime 逃逸后调用必死；
//!    tools/call 必须真实回显（补丁式绕开挂死但 reader 死亡的回归在此步暴露）。
//!
//! python 缺席时优雅跳过（与仓内既有真实子进程测试同先例）。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// 最小 stdio MCP server（JSON-RPC 行协议应答器）：initialize / tools/list /
/// tools/call(echo 回显)；通知帧（无 id）静默忽略。
const MOCK_SCRIPT: &str = r#"import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    mid = msg.get("id")
    if mid is None:
        continue
    method = msg.get("method")
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                  "serverInfo": {"name": "mock-echo", "version": "0.0.1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "echo back the msg argument",
                             "inputSchema": {"type": "object",
                                             "properties": {"msg": {"type": "string"}},
                                             "required": ["msg"]}}]}
    elif method == "tools/call":
        args = (msg.get("params") or {}).get("arguments") or {}
        result = {"content": [{"type": "text",
                               "text": json.dumps({"echo": args.get("msg", "")})}],
                  "isError": False}
    else:
        result = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()
"#;

/// 最小 agent 定义（启动期预载校验 fail-fast；step_timeout_secs 必填）。
const AGENT_JSON: &str = r#"{
  "agent_type": "general",
  "version": "0.3.0",
  "description": "O-513 smoke guard agent",
  "license": "AGPL-3.0-or-later",
  "system_prompt": "smoke test",
  "model": "MiniMax-M3",
  "temperature": 0.1,
  "max_steps": 5,
  "context_window_tokens": 100000,
  "step_timeout_secs": 60,
  "tools": ["file_read"]
}"#;

/// 进程树收割（panic/断言路径也兜底，防 serve/python 孤儿泄漏）。
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = Command::new("taskkill")
            .args(["/PID", &self.0.id().to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.0.wait();
    }
}

fn find_python() -> Option<String> {
    let ok = Command::new("python")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then(|| "python".to_string())
}

/// 手写最小 HTTP/1.1 客户端（Connection: close，读至对端关闭；零新增依赖）。
fn http_request(port: u16, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))
        .unwrap_or_else(|e| panic!("tcp connect :{port} failed: {e}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(35)))
        .unwrap();
    let payload = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .unwrap_or_else(|e| panic!("read response failed: {e}"));
    let raw = String::from_utf8_lossy(&raw).to_string();
    let status: u16 = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let resp_body = raw
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, resp_body)
}

/// POST /mcp JSON-RPC 并返回解包后的 result（HTTP 非 200 即 panic 带现场）。
fn rpc(port: u16, id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
    let body = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        .to_string();
    let (status, resp) = http_request(port, "POST", "/mcp", Some(&body));
    assert_eq!(status, 200, "{method} -> HTTP {status}: {resp}");
    let v: serde_json::Value =
        serde_json::from_str(resp.trim_start_matches(|c: char| c != '{')).expect("json envelope");
    v["result"].clone()
}

#[test]
fn smoke_serve_with_stdio_mcp_reaches_listening_and_client_face_works() {
    let Some(python) = find_python() else {
        eprintln!("smoke skipped: python not found on PATH");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let workdir = tmp.path();

    // 资产：mock server 脚本 / 最小 agent 定义 / MCP 配置（白名单直跑免审批）
    // / /mcp 暴露面开关（mcpServer.enabled 缺省 false fail-closed → User 层开启）
    std::fs::write(workdir.join("mock_mcp_server.py"), MOCK_SCRIPT).unwrap();
    std::fs::create_dir_all(workdir.join("agents")).unwrap();
    std::fs::write(workdir.join("agents/general.json"), AGENT_JSON).unwrap();
    std::fs::create_dir_all(workdir.join(".evo")).unwrap();
    let toml = format!(
        "[[mcp.servers]]\nname = \"mock\"\ncommand = \"{python}\"\nargs = [\"{}\"]\ntimeout_secs = 30\nalways_allow = [\"echo\"]\n",
        workdir
            .join("mock_mcp_server.py")
            .to_str()
            .unwrap()
            .replace('\\', "\\\\")
    );
    std::fs::write(workdir.join(".evo/mcp.toml"), toml).unwrap();
    std::fs::create_dir_all(workdir.join("data")).unwrap();
    std::fs::write(
        workdir.join("data/workbench_settings.json"),
        "{\"mcpServer.enabled\": true}",
    )
    .unwrap();

    // 空闲端口（探测口先关，serve 复用；极小竞态窗可接受）
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };

    // 起真实 serve 二进制（stdout/stderr 继承，失败现场随 cargo test 输出可见）
    let child = Command::new(env!("CARGO_BIN_EXE_evo-agent"))
        .args([
            "serve",
            "--workdir",
            workdir.to_str().unwrap(),
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--no-auth",
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn evo-agent serve");
    let mut serve = KillOnDrop(child);

    // 判据 1（O-513 主判据）：/health 限时探活——修复前起服挂死此步必超时
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut healthy = false;
    while Instant::now() < deadline {
        if serve
            .0
            .try_wait()
            .map(|s| s.is_some())
            .unwrap_or(false)
        {
            panic!("serve process exited before listening (see inherited output)");
        }
        if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            let req = format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
            if s.write_all(req.as_bytes()).is_ok() {
                let mut raw = Vec::new();
                if s.read_to_end(&mut raw).is_ok() {
                    let head = String::from_utf8_lossy(&raw);
                    if head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200") {
                        healthy = true;
                        break;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(
        healthy,
        "serve did not become healthy within 60s — O-513 startup hang regression?"
    );

    // 判据 2：/mcp initialize + tools/list —— mcp_mock_echo 注册在案
    let init = rpc(
        port,
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "serve-mcp-smoke", "version": "0"}
        }),
    );
    assert_eq!(init["serverInfo"]["name"], "evo-agent", "init: {init}");
    let tools = rpc(port, 2, "tools/list", serde_json::json!({}));
    assert!(
        tools["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .any(|t| t["name"] == "mcp_mock_echo"),
        "mcp_mock_echo must be registered into /mcp face, got: {tools}"
    );

    // 判据 3（假活杀手）：tools/call 经 client face 真实回显——reader task
    // 若跨 runtime 逃逸/死亡，此处挂到超时失败
    let call = rpc(
        port,
        3,
        "tools/call",
        serde_json::json!({"name": "mcp_mock_echo", "arguments": {"msg": "o513-smoke"}}),
    );
    assert_eq!(call["isError"], false, "tools/call failed: {call}");
    let text = call["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("o513-smoke"),
        "echo must round-trip through the stdio MCP client face, got: {text}"
    );
}
