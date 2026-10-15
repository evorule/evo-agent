// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! B23 PR-3:MCP server 暴露面 —— 把 serve toolkit 以 MCP 协议开放
//!
//! ## 治理等价原则(01 立项方案 §三裁断)
//!
//! MCP 接入方与 LLM 同权:`/mcp` 读到的工具面 = serve union toolkit 当前态
//! (agentTools.* 裁剪 + E1 白名单已在启动期完成),tools/call 走同一
//! [`ToolHandler::execute_by_name`] 执行链(超时/审批语义与 LLM 调用一致)。
//! 不新增暴露白名单、不放行任何 candidate 工具的免审批通道。
//!
//! ## 协议口径
//!
//! - Streamable HTTP 单一 endpoint `POST /mcp`;响应为单 JSON(规范允许的
//!   合法响应形态;SSE 流响应不做,诚实边界)
//! - 协议版本 `2024-11-05`(与 client 面共用 [`PROTOCOL_VERSION`])
//! - 业务错误走 MCP `result.isError=true` 形态;协议错误(未知方法/解析
//!   失败)走 JSON-RPC error
//! - `enabled=false`(缺省)时不挂载路由 → 404,fail-closed
//!
//! ## 鉴权
//!
//! 复用 G16 中间件(Bearer 双 token 轮换)——`/mcp` 挂在 agent_api router
//! 链内自动覆盖,与 `/api/mcp/*` 工作台面同语义,不建独立 token 面。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::api::agent_api::AgentApiState;
use crate::mcp::client::PROTOCOL_VERSION;

/// 单条 MCP 结果文本上限(B16 工作台口径同源:100KB)
const MCP_RESULT_TEXT_LIMIT: usize = 100 * 1024;

/// `POST /mcp`:JSON-RPC 2.0 分派(initialize / tools/list / tools/call)
pub async fn mcp_post(State(state): State<AgentApiState>, body: String) -> Response {
    let req: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return jsonrpc_error(Value::Null, -32700, &format!("parse error: {e}"));
        }
    };
    let method = req["method"].as_str().unwrap_or("").to_string();
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let is_notification = !req.get("id").is_some() || id.is_null();

    match method.as_str() {
        // 通知:无响应体,202(规范语义;initialized 握手通知在此)
        _ if is_notification => {
            debug!(method = %method, "MCP server notification received");
            StatusCode::ACCEPTED.into_response()
        }
        "initialize" => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "serverInfo": {
                    "name": "evo-agent",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {"tools": {}},
            }
        }))
        .into_response(),
        "tools/list" => match tools_view(&state) {
            Ok(tools) => Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"tools": tools}
            }))
            .into_response(),
            Err(e) => jsonrpc_error(id, -32603, &e),
        },
        "tools/call" => match call_tool(&state, &req["params"]).await {
            Ok(result) => Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result
            }))
            .into_response(),
            Err(e) => jsonrpc_error(id, -32603, &e),
        },
        other => jsonrpc_error(id, -32601, &format!("method not found: {other}")),
    }
}

/// 工具面映射:union toolkit manifests → MCP tools 形
///
/// 治理等价的落点:这里没有独立白名单,读的就是 serve 当前生效 toolkit。
fn tools_view(state: &AgentApiState) -> Result<Vec<Value>, String> {
    let toolkit = state
        .toolkit_lock()
        .read()
        .map_err(|_| "toolkit lock poisoned".to_string())?;
    let mut tools = Vec::new();
    for m in toolkit.manifests() {
        let (description, parameters) = match m.resolve_spec() {
            Some((d, p)) => (d, p),
            None => (String::new(), json!({"type": "object"})),
        };
        tools.push(json!({
            "name": m.name,
            "description": description,
            "inputSchema": parameters,
        }));
    }
    Ok(tools)
}

/// tools/call:执行工具并映射 MCP result 形态
///
/// 业务错误(工具执行失败)映射 `isError:true` + 文本 content——MCP 规范
/// 语义下这是正常 result 而非协议错误;100KB 截断沿用 B16 口径。
async fn call_tool(state: &AgentApiState, params: &Value) -> Result<Value, String> {
    let name = params["name"]
        .as_str()
        .ok_or_else(|| "tools/call params.name must be a string".to_string())?
        .to_string();
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    // std 锁 guard 严禁跨 await(非 Send → Handler bound 破坏):短暂持锁
    // 克隆 ToolHandler(内部 Arc,廉价)后立即释放,再在锁外执行。
    let toolkit = {
        let guard = state
            .toolkit_lock()
            .read()
            .map_err(|_| "toolkit lock poisoned".to_string())?;
        guard.clone()
    };
    let result = toolkit.execute_by_name(&name, &args).await;
    match result {
        Ok(value) => {
            let text = match value {
                Value::String(s) => s,
                other => {
                    serde_json::to_string_pretty(&other).unwrap_or_else(|_| "null".to_string())
                }
            };
            let (text, truncated) = if text.len() > MCP_RESULT_TEXT_LIMIT {
                let mut cut = MCP_RESULT_TEXT_LIMIT;
                while cut > 0 && !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                (text[..cut].to_string(), true)
            } else {
                (text, false)
            };
            let mut content = json!([{"type": "text", "text": text}]);
            if truncated {
                content.as_array_mut().unwrap().push(json!({
                    "type": "text",
                    "text": format!("[truncated at {} bytes]", MCP_RESULT_TEXT_LIMIT)
                }));
            }
            Ok(json!({"content": content, "isError": false}))
        }
        Err(e) => {
            warn!(tool = %name, error = %e, "MCP tools/call tool-level error");
            Ok(json!({
                "content": [{"type": "text", "text": e}],
                "isError": true,
            }))
        }
    }
}

/// JSON-RPC error 响应
fn jsonrpc_error(id: Value, code: i64, message: &str) -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message}
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::agent_api::AgentApiState;
    use crate::api::evorule_client::EvoruleApiClient;
    use crate::api::metrics::Metrics;

    fn state_with_toolkit() -> AgentApiState {
        let toolkit = crate::io_handlers::tool_handler::ToolHandler::new();
        AgentApiState::new_with_metrics(
            crate::agent::AgentDefinitionManager::with_default_dir(),
            EvoruleApiClient::new("http://localhost:0"),
            std::sync::Arc::new(Metrics::new().unwrap()),
            std::env::temp_dir().join("b23-mcp-server-tests"),
            std::sync::Arc::new(crate::api::workspace_client::WorkspaceApiClient::new(
                "http://localhost:0",
            )),
            std::sync::Arc::new(std::sync::RwLock::new(toolkit)),
        )
    }

    /// 注册测试工具:静态表 manifest(真实契约,resolve_spec 可解析)+
    /// 无副作用回显 func(不真执行工具逻辑,隔离文件系统)
    fn register_echo(state: &AgentApiState) {
        use crate::io_handlers::tool_handler::ToolFunction;
        struct Echo;
        #[async_trait::async_trait]
        impl ToolFunction for Echo {
            async fn call(&self, args: &Value) -> crate::io_handler::IoResult {
                Ok(json!({"echo": args}))
            }
        }
        let manifest = crate::agent::tool_manifest::lookup_static("file_list")
            .expect("file_list static manifest must exist");
        let mut toolkit = state.toolkit_lock().write().unwrap();
        toolkit.register(manifest, std::sync::Arc::new(Echo));
    }

    fn post_body(method: &str, id: Value, params: Value) -> String {
        serde_json::json!({"jsonrpc":"2.0","method":method,"id":id,"params":params}).to_string()
    }

    #[tokio::test]
    async fn test_initialize_returns_server_info() {
        let resp = mcp_post(
            State(state_with_toolkit()),
            post_body("initialize", json!(1), json!({})),
        )
        .await;
        let v = parse_json(resp).await;
        assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(v["result"]["serverInfo"]["name"], "evo-agent");
        assert_eq!(v["result"]["capabilities"]["tools"], json!({}));
    }

    #[tokio::test]
    async fn test_tools_list_maps_manifests() {
        let state = state_with_toolkit();
        register_echo(&state);
        let resp = mcp_post(State(state), post_body("tools/list", json!(2), json!({}))).await;
        let v = parse_json(resp).await;
        let tools = v["result"]["tools"].as_array().unwrap();
        let echo = tools.iter().find(|t| t["name"] == "file_list").unwrap();
        assert!(echo["inputSchema"].is_object());
    }

    #[tokio::test]
    async fn test_tools_call_executes_and_maps_result() {
        let state = state_with_toolkit();
        register_echo(&state);
        let resp = mcp_post(
            State(state),
            post_body(
                "tools/call",
                json!(3),
                json!({"name": "file_list", "arguments": {"msg": "hi"}}),
            ),
        )
        .await;
        let v = parse_json(resp).await;
        assert_eq!(v["result"]["isError"], false);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("hi"));
    }

    #[tokio::test]
    async fn test_tools_call_unknown_tool_is_error_result() {
        let resp = mcp_post(
            State(state_with_toolkit()),
            post_body("tools/call", json!(4), json!({"name": "no_such_tool"})),
        )
        .await;
        let v = parse_json(resp).await;
        // 业务错走 isError=true result,非 JSON-RPC error
        assert_eq!(v["result"]["isError"], true);
        assert!(v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not found"));
    }

    #[tokio::test]
    async fn test_unknown_method_is_jsonrpc_error() {
        let resp = mcp_post(
            State(state_with_toolkit()),
            post_body("prompts/list", json!(5), json!({})),
        )
        .await;
        let v = parse_json(resp).await;
        assert_eq!(v["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn test_notification_returns_202() {
        let resp = mcp_post(
            State(state_with_toolkit()),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn test_malformed_body_is_parse_error() {
        let resp = mcp_post(State(state_with_toolkit()), "not-json".to_string()).await;
        let v = parse_json(resp).await;
        assert_eq!(v["error"]["code"], -32700);
    }

    /// 路由开关:enabled=false /mcp 404(fail-closed);true initialize 可达
    #[tokio::test]
    async fn test_router_mcp_expose_toggle() {
        use axum::body::Body;
        use tower::ServiceExt;

        let make_req = || {
            axum::http::Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .body(Body::from(post_body("initialize", json!(1), json!({}))))
                .unwrap()
        };
        // false:路由不挂载 → 404
        let app = crate::api::agent_api::router_with_auth(
            state_with_toolkit(),
            crate::api::auth::AuthConfig::disabled(),
        );
        let resp = app.oneshot(make_req()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        // true:G16 链内可达,initialize 返回
        let app = crate::api::agent_api::router_with_auth(
            state_with_toolkit().with_mcp_server_enabled(true),
            crate::api::auth::AuthConfig::disabled(),
        );
        let resp = app.oneshot(make_req()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = parse_json(resp).await;
        assert_eq!(v["result"]["serverInfo"]["name"], "evo-agent");
    }

    async fn parse_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
}
