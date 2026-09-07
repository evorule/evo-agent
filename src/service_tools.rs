// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 服务代理工具 —— 服务消费桥（发现 → 注册 → 直调）
//!
//! agent 启动时经 `GET /api/services` 发现 evorule-server 插件服务，
//! 按 `config.evorule.service_tools` 白名单将每个服务注册为代理工具；
//! 工具执行经 `POST /api/services/{name}/invoke` 直调 server 服务链
//! （与 io_request 同一执行路径，无第二实现）。
//!
//! 治理语义：
//! - 白名单为空 = 不注册任何服务工具（安全默认）；
//! - `sensitive=true` 的服务跳过注册（server 侧 invoke 对敏感服务 403，
//!   敏感操作必须走会话审计链）；
//! - 服务执行失败 fail-fast 透传（无静默降级）。

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use evorule_tcb::JsonValue;

use crate::api::evorule_client::EvoruleApiClient;
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};

/// 服务描述注册表：注册时缓存对账清单的 description。
///
/// 供 runner 组装 tools schema 的降级分支查询（动态注册的工具无静态 spec，
/// 无此表则 schema 的 description 为空，LLM 只能凭工具名猜用途）。
static SERVICE_DESCRIPTIONS: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();

fn descriptions() -> &'static RwLock<HashMap<String, String>> {
    SERVICE_DESCRIPTIONS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 查询已注册服务工具的描述（未注册返回 None）
pub fn service_description(name: &str) -> Option<String> {
    descriptions().read().ok()?.get(name).cloned()
}

/// 服务代理工具：`call` = `POST /api/services/{name}/invoke`(args 原样透传)
struct ServiceProxyTool {
    ev: EvoruleApiClient,
    service_name: String,
}

#[async_trait::async_trait]
impl ToolFunction for ServiceProxyTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let serde_args = crate::json_convert::tcb_to_serde(args);
        self.ev
            .invoke_service(&self.service_name, &serde_args)
            .await
            .map(|v| crate::json_convert::serde_to_tcb(&v))
            .map_err(|e| format!("service {} invoke failed: {e}", self.service_name))
    }
}

/// 服务发现 → 白名单过滤 → 注册代理工具。
///
/// - 白名单为空：直接返回 0（不发起任何网络请求）；
/// - 服务发现失败：返回 Err（fail-fast，调用方决定是否致命）；
/// - `sensitive=true` 的服务跳过（invoke 403 守卫，注册了也是坏工具面）；
/// - 白名单中不存在的服务名：跳过并 warn（配置拼写错误可见）；
/// - 与既有本地工具重名的服务：跳过（本地工具优先，防覆盖）。
///
/// 返回实际注册的工具数量。
pub async fn register_service_tools(
    handler: &mut ToolHandler,
    ev: &EvoruleApiClient,
    whitelist: &[String],
) -> Result<usize, String> {
    if whitelist.is_empty() {
        return Ok(0);
    }
    let services = ev
        .list_services()
        .await
        .map_err(|e| format!("服务发现失败（GET /api/services）: {e}"))?;

    let mut registered = 0usize;
    for want in whitelist {
        let Some(info) = services
            .as_array()
            .and_then(|arr| arr.iter().find(|s| s["name"].as_str() == Some(want.as_str())))
        else {
            eprintln!(
                "[service-tools] 跳过白名单项 '{want}'：不在服务对账清单（GET /api/services）"
            );
            continue;
        };
        if info["sensitive"].as_bool() == Some(true) {
            eprintln!(
                "[service-tools] 跳过服务 '{want}'：sensitive 服务禁止直调（须走会话审计链）"
            );
            continue;
        }
        if handler.has_tool(want) {
            eprintln!(
                "[service-tools] 跳过服务 '{want}'：与本地工具重名（本地工具优先）"
            );
            continue;
        }
        let description = info["description"].as_str().unwrap_or("").to_string();
        descriptions()
            .write()
            .map(|mut m| {
                m.insert(want.clone(), description);
            })
            .map_err(|_| "服务描述注册表写入失败".to_string())?;
        handler.register_tool(
            want,
            Arc::new(ServiceProxyTool {
                ev: ev.clone(),
                service_name: want.clone(),
            }),
        );
        registered += 1;
    }
    Ok(registered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_handlers::tool_handler::ToolHandler;

    #[test]
    fn empty_whitelist_registers_nothing() {
        // 安全默认：空白名单零注册零网络请求
        let mut handler = ToolHandler::new();
        let ev = EvoruleApiClient::new("http://127.0.0.1:1");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let n = rt
            .block_on(register_service_tools(&mut handler, &ev, &[]))
            .unwrap();
        assert_eq!(n, 0);
        assert!(handler.tool_names().is_empty());
    }

    #[test]
    fn discovery_failure_is_fail_fast() {
        // 不可达 base_url → 服务发现显式失败（不静默吞掉）
        let mut handler = ToolHandler::new();
        let ev = EvoruleApiClient::new("http://127.0.0.1:1");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(register_service_tools(
                &mut handler,
                &ev,
                &["finance_config_get".to_string()],
            ))
            .unwrap_err();
        assert!(err.contains("服务发现失败"), "应 fail-fast: {err}");
    }

    // —— 进程内顺序应答式 HTTP 假服务（仅替代远端 server 的传输层）——

    struct CapturedRequest {
        path: String,
        body: String,
    }

    /// 按顺序对每个请求回一个 (status, body)；响应带 Connection: close 强制
    /// 逐请求新建连接；捕获 (path, body) 供请求形状断言。
    fn spawn_http_fixture(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, Arc<std::sync::Mutex<Vec<CapturedRequest>>>) {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let (stream, _) = listener.accept().unwrap();
                let mut stream = stream;
                let path;
                let mut req_body = Vec::new();
                {
                    let mut reader = BufReader::new(&stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    path = line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    let mut content_length = 0usize;
                    loop {
                        let mut h = String::new();
                        reader.read_line(&mut h).unwrap();
                        if h.trim().is_empty() {
                            break;
                        }
                        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                            content_length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    if content_length > 0 {
                        req_body = vec![0u8; content_length];
                        reader.read_exact(&mut req_body).unwrap();
                    }
                }
                captured_clone.lock().unwrap().push(CapturedRequest {
                    path,
                    body: String::from_utf8_lossy(&req_body).into_owned(),
                });
                let reason = match status {
                    200 => "OK",
                    _ => "Internal Server Error",
                };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(resp.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });
        (format!("http://{addr}"), captured)
    }

    #[test]
    fn proxy_tool_invoke_request_shape_and_result() {
        // 直调契约：POST /api/services/{name}/invoke，body = args 原样透传，
        // 响应 JSON 原样返回；描述从对账清单进注册表供 schema 透出。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (base, captured) = spawn_http_fixture(vec![
            (
                200,
                r#"[{"name":"shape_svc","source":"native","description":"形状测试服务","sensitive":false}]"#,
            ),
            (200, r#"{"result":"ok"}"#),
        ]);
        let mut handler = ToolHandler::new();
        let ev = EvoruleApiClient::new(&base);
        let n = rt
            .block_on(register_service_tools(
                &mut handler,
                &ev,
                &["shape_svc".to_string()],
            ))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            service_description("shape_svc").as_deref(),
            Some("形状测试服务")
        );

        let out = rt
            .block_on(handler.execute_by_name(
                "shape_svc",
                &crate::json_convert::serde_to_tcb(&serde_json::json!({"key": "demo"})),
            ))
            .unwrap();
        assert_eq!(
            crate::json_convert::tcb_to_serde(&out),
            serde_json::json!({"result": "ok"})
        );

        let cap = captured.lock().unwrap();
        assert_eq!(cap[0].path, "/api/services", "第一次请求应为服务发现");
        assert_eq!(
            cap[1].path, "/api/services/shape_svc/invoke",
            "第二次请求应为服务直调"
        );
        assert_eq!(cap[1].body, r#"{"key":"demo"}"#, "args 应原样透传为 body");
    }

    #[test]
    fn proxy_tool_error_is_fail_fast_passthrough() {
        // 服务执行失败（HTTP 500）必须显式报错透传，不静默吞掉。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (base, _captured) = spawn_http_fixture(vec![
            (
                200,
                r#"[{"name":"err_svc","source":"native","sensitive":false}]"#,
            ),
            (500, r#"{"error":"boom"}"#),
        ]);
        let mut handler = ToolHandler::new();
        let ev = EvoruleApiClient::new(&base);
        let n = rt
            .block_on(register_service_tools(
                &mut handler,
                &ev,
                &["err_svc".to_string()],
            ))
            .unwrap();
        assert_eq!(n, 1);

        let err = rt
            .block_on(handler.execute_by_name(
                "err_svc",
                &crate::json_convert::serde_to_tcb(&serde_json::json!({})),
            ))
            .unwrap_err();
        assert!(err.contains("500"), "应透传 HTTP 状态码: {err}");
        assert!(
            err.contains("service err_svc invoke failed"),
            "应带服务名上下文: {err}"
        );
    }
}
