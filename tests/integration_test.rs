// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// Integration tests for evo-agent run loop against a mock evorule server.
//
// Notes:
// - mockito mocks the evorule HTTP API (create_session / events / io_response / state / etc.)
// - LlmHandler::mock() short-circuits the LLM HTTP call so tests stay deterministic
//   and don't require a real MiniMax / DeepSeek / OpenAI API key.
// - The final AgentResult.content comes from the stable SSE event's payload
//   (forwarded to get_state on the mock server), NOT from the LLM response.

use evo_agent::agent::approval::AutoApprove;
use evo_agent::agent::memory::MemoryManager;
use evo_agent::agent::runner::{AgentConfig, AgentRunner};
use evo_agent::agent::sediment::{sediment, SedimentConfig, SedimentDeps};
use evo_agent::agent::summarizer::ContextSummarizer;
use evo_agent::agent::Message;
use evo_agent::api::evorule_client::EvoruleApiClient;
use evo_agent::io_handler::IoResult;
use evo_agent::io_handlers::tool_handler::ToolFunction;
use evo_agent::io_handlers::LlmHandler;
use evorule_tcb::JsonValue;
use mockito::Server;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

#[tokio::test]
async fn test_auto_recall_with_mock_server() {
    let mut server = Server::new_async().await;
    let server_url = server.url();

    server
        .mock("POST", "/api/sessions")
        .with_status(200)
        .with_body(r#"{"session_id": 123}"#)
        .create_async()
        .await;

    server
        .mock("GET", "/api/shared/facts?prefix=shared.default.")
        .with_status(200)
        .with_body(
            json!([
                {
                    "fact_id": 1,
                    "path": "shared.research.note1",
                    "value": "Evo-Agent design principles",
                    "source_session_id": 100,
                    "version": 1
                },
                {
                    "fact_id": 2,
                    "path": "shared.research.note2",
                    "value": "SSE event-driven architecture",
                    "source_session_id": 100,
                    "version": 2
                }
            ])
            .to_string(),
        )
        .create_async()
        .await;

    server
        .mock("POST", "/api/sessions/123/used_at_startup")
        .with_status(200)
        .with_body(r#"{"success": true, "message": "used_at_startup recorded"}"#)
        .create_async()
        .await;

    server
        .mock("POST", "/api/sessions/123/command")
        .with_status(200)
        .with_body(r#"{"success": true, "fact_id": 1}"#)
        .create_async()
        .await;

    // 注意：事件类型值必须与 evorule-server 0.3.1 实际发出的 PascalCase 一致
    // （fact_to_sse_data 发 "IoRequest"/"Stable"），且 IoRequest 为扁平结构。
    let sse_events = "data: {\"type\":\"IoRequest\",\"io_type\":\"call_external\",\"id\":1,\"params\":{\"model\":\"gpt-4o-mini\",\"system_prompt\":\"You are a helpful assistant\",\"goal\":\"Summarize research notes\",\"tool_names\":[]}}\r\n\r\ndata: {\"type\":\"Stable\",\"payload\":\"Task completed successfully\"}\r\n\r\n";
    server
        .mock("GET", "/api/sessions/123/events")
        .with_status(200)
        .with_header("Content-Type", "text/event-stream")
        .with_body(sse_events)
        .create_async()
        .await;

    server
        .mock("GET", "/api/sessions/123/state")
        .with_status(200)
        .with_body(r#"{"payload": "Task completed successfully"}"#)
        .create_async()
        .await;

    server
        .mock("POST", "/api/sessions/123/io_response")
        .with_status(200)
        .with_body(r#"{"success": true, "message": "IoResponse submitted"}"#)
        .create_async()
        .await;

    let client = EvoruleApiClient::new(&server_url);
    let config = AgentConfig::default();
    let mut runner =
        AgentRunner::new(config, client).with_llm_handler(LlmHandler::mock("Mock LLM response"));

    let result = runner.run("Summarize research notes").await;

    assert!(result.is_ok());
    let result = result.unwrap();
    assert!(result.success);
    assert_eq!(result.content, "Task completed successfully");
}

#[tokio::test]
async fn test_auto_rewind_on_error() {
    let mut server = Server::new_async().await;
    let server_url = server.url();

    server
        .mock("POST", "/api/sessions")
        .with_status(200)
        .with_body(r#"{"session_id": 456}"#)
        .create_async()
        .await;

    server
        .mock("GET", "/api/shared/facts?prefix=shared.default.")
        .with_status(200)
        .with_body("[]")
        .create_async()
        .await;

    server
        .mock("POST", "/api/sessions/456/command")
        .with_status(200)
        .with_body(r#"{"success": true, "fact_id": 1}"#)
        .create_async()
        .await;

    let sse_events_error =
        "data: {\"type\":\"Error\",\"payload\":{\"message\":\"LLM timeout\"}}\r\n\r\n";
    server
        .mock("GET", "/api/sessions/456/events")
        .with_status(200)
        .with_header("Content-Type", "text/event-stream")
        .with_body(sse_events_error)
        .create_async()
        .await;

    server
        .mock("GET", "/api/sessions/456/facts")
        .with_status(200)
        .with_body(
            json!([
                {"version": 0, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"},
                {"version": 1, "id": 2, "path": "command", "value": "test", "type": "Command"}
            ])
            .to_string(),
        )
        .create_async()
        .await;

    let rewind_mock = server
        .mock("GET", "/api/sessions/456/rewind?version=0")
        .with_status(200)
        .with_body(r#"{"version": 0, "payload": {}, "queue": []}"#)
        .expect(1)
        .create_async()
        .await;

    let client = EvoruleApiClient::new(&server_url);
    let config = AgentConfig::default();
    let mut runner = AgentRunner::new(config, client);

    let result = runner.run("Test rewind on error").await;

    assert!(result.is_ok());
    let result = result.unwrap();
    // 收紧断言：Error 事件必须被正常解析并真实走入 auto-rewind 分支。
    // 通过断言 rewind 端点确实被调用，来区分「错误路径生效」与「SSE 被静默吞掉」。
    rewind_mock.assert();
    assert!(!result.success);
    assert!(result.error.is_some());
}

#[tokio::test]
async fn test_auto_recall_no_shared_facts() {
    let mut server = Server::new_async().await;
    let server_url = server.url();

    server
        .mock("POST", "/api/sessions")
        .with_status(200)
        .with_body(r#"{"session_id": 789}"#)
        .create_async()
        .await;

    server
        .mock("GET", "/api/shared/facts?prefix=shared.default.")
        .with_status(200)
        .with_body("[]")
        .create_async()
        .await;

    server
        .mock("POST", "/api/sessions/789/command")
        .with_status(200)
        .with_body(r#"{"success": true, "fact_id": 1}"#)
        .create_async()
        .await;

    let sse_events = "data: {\"type\":\"IoRequest\",\"io_type\":\"call_external\",\"id\":1,\"params\":{\"model\":\"gpt-4o-mini\",\"system_prompt\":\"You are a helpful assistant\",\"goal\":\"Simple task\",\"tool_names\":[]}}\r\n\r\ndata: {\"type\":\"Stable\",\"payload\":\"Simple task completed\"}\r\n\r\n";
    server
        .mock("GET", "/api/sessions/789/events")
        .with_status(200)
        .with_header("Content-Type", "text/event-stream")
        .with_body(sse_events)
        .create_async()
        .await;

    server
        .mock("GET", "/api/sessions/789/state")
        .with_status(200)
        .with_body(r#"{"payload": "Simple task completed"}"#)
        .create_async()
        .await;

    server
        .mock("POST", "/api/sessions/789/io_response")
        .with_status(200)
        .with_body(r#"{"success": true, "message": "IoResponse submitted"}"#)
        .create_async()
        .await;

    let client = EvoruleApiClient::new(&server_url);
    let config = AgentConfig::default();
    let mut runner =
        AgentRunner::new(config, client).with_llm_handler(LlmHandler::mock("Mock LLM response"));

    let result = runner.run("Simple task").await;

    assert!(result.is_ok());
    let result = result.unwrap();
    assert!(result.success);
    assert_eq!(result.content, "Simple task completed");
}

#[tokio::test]
async fn test_full_workflow_with_all_features() {
    let mut server = Server::new_async().await;
    let server_url = server.url();

    server
        .mock("POST", "/api/sessions")
        .with_status(200)
        .with_body(r#"{"session_id": 999}"#)
        .create_async()
        .await;

    server
        .mock("GET", "/api/shared/facts?prefix=shared.default.")
        .with_status(200)
        .with_body(
            json!([
                {
                    "fact_id": 10,
                    "path": "shared.knowledge.rules",
                    "value": "Always follow safety guidelines",
                    "source_session_id": 200,
                    "version": 5
                }
            ])
            .to_string(),
        )
        .create_async()
        .await;

    server
        .mock("POST", "/api/sessions/999/used_at_startup")
        .with_status(200)
        .with_body(r#"{"success": true}"#)
        .create_async()
        .await;

    server
        .mock("POST", "/api/sessions/999/command")
        .with_status(200)
        .with_body(r#"{"success": true, "fact_id": 1}"#)
        .create_async()
        .await;

    let sse_events = "data: {\"type\":\"PhaseChange\",\"payload\":{\"phase\":\"planning\"}}\r\n\r\ndata: {\"type\":\"IoRequest\",\"io_type\":\"call_external\",\"id\":1,\"params\":{\"model\":\"gpt-4o-mini\",\"system_prompt\":\"Safety first assistant\",\"goal\":\"Analyze safety protocols\",\"tool_names\":[\"safety_check\"]}}\r\n\r\ndata: {\"type\":\"StateTransition\",\"payload\":{}}\r\n\r\ndata: {\"type\":\"Stable\",\"payload\":\"Safety analysis complete\"}\r\n\r\n";
    server
        .mock("GET", "/api/sessions/999/events")
        .with_status(200)
        .with_header("Content-Type", "text/event-stream")
        .with_body(sse_events)
        .create_async()
        .await;

    server
        .mock("GET", "/api/sessions/999/state")
        .with_status(200)
        .with_body(r#"{"payload": "Safety analysis complete"}"#)
        .create_async()
        .await;

    server
        .mock("POST", "/api/sessions/999/io_response")
        .with_status(200)
        .with_body(r#"{"success": true, "message": "IoResponse submitted"}"#)
        .create_async()
        .await;

    let client = EvoruleApiClient::new(&server_url);
    let config = AgentConfig {
        system_prompt: "Safety first assistant".to_string(),
        tool_names: vec!["safety_check".to_string()],
        ..AgentConfig::default()
    };

    let mut runner =
        AgentRunner::new(config, client).with_llm_handler(LlmHandler::mock("Mock LLM response"));

    let result = runner.run("Analyze safety protocols").await;

    assert!(result.is_ok());
    let result = result.unwrap();
    assert!(result.success);
    assert_eq!(result.content, "Safety analysis complete");
    assert_eq!(result.steps, 1);
}

/// L-3 回归测试：当共享空间的会话摘要数量达到阈值时，sediment 的 C4 rollup
/// 应把被合并的最旧摘要通过 `POST /api/shared/facts/rollup` 标记为 rolled_up，
/// 使其从 server 端 `facts_by_path_prefix` 查询结果中过滤，避免下次仍计入阈值、
/// 反复 rollup 造成共享空间膨胀。
///
/// 这里用 mockito 模拟 evorule server：
/// - `GET /api/shared/facts?prefix=shared.default.sessions.` 返回 12 条普通摘要（>= 阈值 10）
/// - `POST /api/shared/facts/rollup` 期望收到最旧 5 条（fact_id 1..5）的标记请求
#[tokio::test]
async fn test_sediment_rollup_marks_old_summaries_as_rolled_up() {
    let mut server = Server::new_async().await;
    let server_url = server.url();

    // 12 条普通会话摘要（>= 阈值 10）。fact_id 1..12，timestamp 1001..1012。
    // 路径不含 .rollup. ，全部计入 regular；最旧的 5 条（fact_id 1..5, ts 最小）将被合并。
    let mut summaries = Vec::new();
    for i in 1..=12u64 {
        let ts = 1000 + i;
        summaries.push(json!({
            "fact_id": i,
            "path": format!("shared.default.sessions.s{}.summary", i),
            "value": {
                "key": format!("sessions.s{}.summary", i),
                "value": format!("summary content {}", i),
                "timestamp": ts
            },
            "source_session_id": 100,
            "version": 1
        }));
    }
    server
        .mock("GET", "/api/shared/facts?prefix=shared.default.sessions.")
        .with_status(200)
        .with_body(json!(summaries).to_string())
        .create_async()
        .await;

    // rollup 标记端点：期望收到最旧 5 条（fact_id 1..5）的标记请求
    let rollup_mock = server
        .mock("POST", "/api/shared/facts/rollup")
        .with_status(200)
        .with_body(r#"{"success": true, "message": "5 facts marked as rolled up"}"#)
        .match_body(mockito::Matcher::Json(
            json!({ "fact_ids": [1, 2, 3, 4, 5] }),
        ))
        .expect(1)
        .create_async()
        .await;

    let client = EvoruleApiClient::new(&server_url);
    let mut memory = MemoryManager::new("default", client).with_session_id("s1");
    // 阈值 10 → rollup_count = 5（最旧 5 条被合并并标记）
    let cfg = SedimentConfig {
        summary_rollup_threshold: 10,
        ..SedimentConfig::default()
    };
    // summarize_session 会把 LLM 返回的 content 解析为 SessionSummaryOut，
    // 因此 mock 内容必须是合法 JSON {"summary":..,"stable_facts":[..]}。
    let summarizer = ContextSummarizer::new(
        LlmHandler::mock(r#"{"summary":"session summary","stable_facts":[]}"#),
        None,
    );
    let mut deps = SedimentDeps {
        memory: &mut memory,
        summarizer: Some(&summarizer),
        extractor: None,
    };
    let messages = vec![Message::User {
        content: "some conversation".to_string(),
    }];

    let result = sediment(&mut deps, &cfg, "s1", &messages).await;
    assert!(result.summary_written, "summary should be written");
    // 核心断言：L-3 修复后 rollup 应执行并标记旧摘要
    assert!(result.rollup_done, "rollup should be performed (L-3 fix)");

    // 验证 rollup 标记请求确实按预期发送（body 含最旧 5 条 fact_id）
    rollup_mock.assert();
}

// ===== P0-1 核心路径集成测试:多轮 ReAct 循环 + 审批模式 =====
//
// SSE 事件流由 mock 脚本化（代替引擎 constitution 驱动），覆盖 runner 作为
// io 处理方的循环语义：call_external(LLM) ↔ call_service(工具) ↔ Stable。
// LLM 侧用独立 mockito 服务器模拟 OpenAI 兼容 API（含 tool_calls 归一化回归）。

/// 通用 evorule server mock：create_session / shared facts(空) / command / events(SSE) / state / io_response(兜底)
async fn mock_evorule_base(server: &mut Server, session_id: &str, sse_body: String) {
    server
        .mock("POST", "/api/sessions")
        .with_status(200)
        .with_body(r#"{"session_id": 777}"#)
        .create_async()
        .await;
    server
        .mock("GET", "/api/shared/facts?prefix=shared.default.")
        .with_status(200)
        .with_body("[]")
        .create_async()
        .await;
    server
        .mock(
            "POST",
            format!("/api/sessions/{session_id}/command").as_str(),
        )
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;
    server
        .mock("GET", format!("/api/sessions/{session_id}/events").as_str())
        .with_status(200)
        .with_header("Content-Type", "text/event-stream")
        .with_body(sse_body)
        .create_async()
        .await;
    server
        .mock("GET", format!("/api/sessions/{session_id}/state").as_str())
        .with_status(200)
        .with_body(r#"{"payload":{"llm_response":{"content":"done"}}}"#)
        .create_async()
        .await;
    // io_response 兜底(先创建;需要 body 断言的 mock 在各测试中后创建,优先级更高)
    server
        .mock(
            "POST",
            format!("/api/sessions/{session_id}/io_response").as_str(),
        )
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;
}

fn sse_line(event: &serde_json::Value) -> String {
    format!("data: {}\r\n\r\n", event)
}

fn io_request(id: u64, io_type: &str, params: serde_json::Value) -> serde_json::Value {
    json!({"type":"IoRequest","io_type":io_type,"id":id,"params":params})
}

/// 带执行计数与可编程结果工具
struct RecordingTool {
    calls: Arc<AtomicUsize>,
    result: &'static str,
}
#[async_trait::async_trait]
impl ToolFunction for RecordingTool {
    async fn call(&self, _args: &JsonValue) -> IoResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(JsonValue::string(self.result))
    }
}

/// 多轮 ReAct 循环:LLM turn1 返回 tool_calls(OpenAI 形状) → 工具执行 →
/// LLM turn2 看到工具结果(消息历史含 TOOL_RESULT_MARKER)返回最终答案 → Stable。
///
/// 同时固化 F2 回归:execute() 必须把 OpenAI 形状 tool_calls 归一化为内部
/// {tool_name,args}——否则 handle_call_external 反序列化 LlmResponse 即失败。
#[tokio::test]
async fn test_multi_turn_react_loop_with_tool_call() {
    // ── LLM mock 服务器:turn2 的带 marker 匹配 mock 后创建(优先级更高) ──
    let mut llm_server = Server::new_async().await;
    // turn1 兜底:OpenAI 形状 tool_calls(修复前此形状导致 run() 必失败)
    llm_server
        .mock("POST", "/")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"choices":[{"message":{"content":"","tool_calls":[
                {"id":"c1","type":"function",
                 "function":{"name":"lookup","arguments":"{\"q\":\"evorule\"}"}}]},
                "finish_reason":"tool_calls"}]}"#,
        )
        .create_async()
        .await;
    // turn2:请求消息历史含工具结果 marker(证明工具结果回流 LLM)
    let llm_turn2 = llm_server
        .mock("POST", "/")
        .match_body(mockito::Matcher::Regex("TOOL_RESULT_MARKER".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"choices":[{"message":{"content":"final answer"},"finish_reason":"stop"}]}"#)
        .create_async()
        .await;

    // ── evorule server mock:SSE 脚本化 3 个 IoRequest + Stable ──
    let mut server = Server::new_async().await;
    let sse = format!(
        "{}{}{}{}",
        sse_line(&io_request(
            1,
            "call_external",
            json!({"model":"mock-model"})
        )),
        sse_line(&io_request(
            2,
            "call_service",
            json!({"tool_name":"lookup","args":{"q":"evorule"}})
        )),
        sse_line(&io_request(
            3,
            "call_external",
            json!({"model":"mock-model"})
        )),
        sse_line(&json!({"type":"Stable"})),
    );
    mock_evorule_base(&mut server, "777", sse).await;

    // ── runner:真实 LlmHandler(HTTP) + 自定义工具 ──
    let client = EvoruleApiClient::new(&server.url());
    let llm = LlmHandler::new("mock-model", &llm_server.url(), Some("k".to_string()))
        .with_retry_config(0, 0.001, 0.01);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools: BTreeMap<String, Arc<dyn ToolFunction>> = BTreeMap::new();
    tools.insert(
        "lookup".to_string(),
        Arc::new(RecordingTool {
            calls: calls.clone(),
            result: "TOOL_RESULT_MARKER the knowledge base says hi",
        }),
    );
    let mut runner = AgentRunner::new(AgentConfig::default(), client)
        .with_llm_handler(llm)
        .with_tool_handler(evo_agent::io_handlers::ToolHandler::with_tools(tools));

    let result = runner
        .run("find knowledge about evorule")
        .await
        .expect("run should not Err");
    assert!(
        result.success,
        "multi-turn run should succeed: {:?}",
        result.content
    );
    assert_eq!(result.content, "done");
    assert_eq!(result.steps, 3, "3 IoRequest events = 3 steps");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "tool must be executed exactly once via call_service"
    );
    assert_eq!(result.tool_calls, vec!["lookup".to_string()]);
    llm_turn2.assert_async().await;
}

/// 审批-拒绝路径:工具返回 needs_approval proposal,无 approval callback
/// (默认拒绝,安全优先)→ 不重执行,拒绝结果作为 Tool 消息回传。
#[tokio::test]
async fn test_approval_denied_by_default_in_loop() {
    let mut llm_server = Server::new_async().await;
    llm_server
        .mock("POST", "/")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"choices":[{"message":{"content":"","tool_calls":[
                {"id":"c1","type":"function",
                 "function":{"name":"risky_delete","arguments":"{\"path\":\"/tmp/x\"}"}}]},
                "finish_reason":"tool_calls"}]}"#,
        )
        .create_async()
        .await;

    let mut server = Server::new_async().await;
    let sse = format!(
        "{}{}{}",
        sse_line(&io_request(
            1,
            "call_external",
            json!({"model":"mock-model"})
        )),
        sse_line(&io_request(
            2,
            "call_service",
            json!({"tool_name":"risky_delete","args":{"path":"/tmp/x"}})
        )),
        sse_line(&json!({"type":"Stable"})),
    );
    mock_evorule_base(&mut server, "777", sse).await;
    // 断言 io_response 携带拒绝结果
    let io_resp = server
        .mock("POST", "/api/sessions/777/io_response")
        .match_body(mockito::Matcher::Regex("User denied approval".to_string()))
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;

    let calls = Arc::new(AtomicUsize::new(0));
    let client = EvoruleApiClient::new(&server.url());
    let llm = LlmHandler::new("mock-model", &llm_server.url(), Some("k".to_string()))
        .with_retry_config(0, 0.001, 0.01);
    let mut tools: BTreeMap<String, Arc<dyn ToolFunction>> = BTreeMap::new();
    tools.insert(
        "risky_delete".to_string(),
        Arc::new(ProposalTool {
            calls: calls.clone(),
        }),
    );
    let mut runner = AgentRunner::new(AgentConfig::default(), client)
        .with_llm_handler(llm)
        .with_tool_handler(evo_agent::io_handlers::ToolHandler::with_tools(tools));
    // 不设置 approval callback → 默认拒绝

    let result = runner
        .run("delete the file")
        .await
        .expect("run should not Err");
    assert!(result.success, "denied tool call should not fail the run");
    assert_eq!(result.steps, 2);
    assert_eq!(result.tool_calls, vec!["risky_delete".to_string()]);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "denied tool must not be re-executed"
    );
    io_resp.assert_async().await;
}

/// 审批-批准路径(AutoApprove):proposal → 批准 → 带 approved:true 重执行 →
/// 工具返回执行结果回传。
#[tokio::test]
async fn test_approval_auto_approved_reexecutes_with_flag() {
    let mut llm_server = Server::new_async().await;
    llm_server
        .mock("POST", "/")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"choices":[{"message":{"content":"","tool_calls":[
                {"id":"c1","type":"function",
                 "function":{"name":"risky_delete","arguments":"{}"}}]},
                "finish_reason":"tool_calls"}]}"#,
        )
        .create_async()
        .await;

    let mut server = Server::new_async().await;
    let sse = format!(
        "{}{}{}",
        sse_line(&io_request(
            1,
            "call_external",
            json!({"model":"mock-model"})
        )),
        sse_line(&io_request(
            2,
            "call_service",
            json!({"tool_name":"risky_delete","args":{"path":"/tmp/y"}})
        )),
        sse_line(&json!({"type":"Stable"})),
    );
    mock_evorule_base(&mut server, "777", sse).await;
    // 断言重执行结果(而非 proposal)回传
    let io_resp = server
        .mock("POST", "/api/sessions/777/io_response")
        .match_body(mockito::Matcher::Regex(
            "EXECUTED_AFTER_APPROVAL".to_string(),
        ))
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;

    let calls = Arc::new(AtomicUsize::new(0));
    let client = EvoruleApiClient::new(&server.url());
    let llm = LlmHandler::new("mock-model", &llm_server.url(), Some("k".to_string()))
        .with_retry_config(0, 0.001, 0.01);
    let mut tools: BTreeMap<String, Arc<dyn ToolFunction>> = BTreeMap::new();
    tools.insert(
        "risky_delete".to_string(),
        Arc::new(ApprovalAwareTool {
            calls: calls.clone(),
        }),
    );
    let mut runner = AgentRunner::new(AgentConfig::default(), client)
        .with_llm_handler(llm)
        .with_tool_handler(evo_agent::io_handlers::ToolHandler::with_tools(tools))
        .with_approval_callback(Arc::new(AutoApprove));

    let result = runner
        .run("delete the file")
        .await
        .expect("run should not Err");
    assert!(result.success);
    assert_eq!(result.steps, 2);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "proposal call + approved re-execution"
    );
    io_resp.assert_async().await;
}

/// 返回 needs_approval proposal 的工具(顶层必须是 JSON object,
/// parse_approval_request 才能识别 status 字段)
struct ProposalTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolFunction for ProposalTool {
    async fn call(&self, _args: &JsonValue) -> IoResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(JsonValue::object_from_pairs(&[
            ("status", JsonValue::string("needs_approval")),
            ("command", JsonValue::string("rm -rf /tmp/x")),
            ("risk", JsonValue::string("high")),
        ]))
    }
}

/// 首次调用返回 proposal;args 带 approved:true 时真正执行
struct ApprovalAwareTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolFunction for ApprovalAwareTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // 结构化检查而非字符串匹配:JsonValue::Display 是非紧凑格式(": " 带空格),
        // contains("\"approved\":true") 永远失配
        let approved = args
            .get("approved")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if approved {
            Ok(JsonValue::string("EXECUTED_AFTER_APPROVAL"))
        } else {
            Ok(JsonValue::object_from_pairs(&[
                ("status", JsonValue::string("needs_approval")),
                ("command", JsonValue::string("rm -rf /tmp/y")),
                ("risk", JsonValue::string("high")),
            ]))
        }
    }
}

/// 工具调用契约对齐锁定：constitution 模板/io_request 键与内部 ToolCall 序列化形状
/// ({tool_name, args}) 一致 —— 防三处契约再漂移（collect 模板 / io_request 键 / 消息回传）。
/// ①collect 模板消费 {{tool_name}}；②call_service io_request 参数键统一 tool_name；
/// ③消息回传出站转换由 llm_handler::to_openai_wire_messages 单测锁定。
#[test]
fn test_constitution_tool_call_contract_alignment() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let path = std::path::Path::new(manifest).join("assets/agent_constitution.json");
    let text = std::fs::read_to_string(&path).expect("read agent_constitution.json");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("constitution is valid JSON");

    fn walk(v: &serde_json::Value, keys: &mut Vec<String>, strings: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, val) in map {
                    keys.push(k.clone());
                    walk(val, keys, strings);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(item, keys, strings);
                }
            }
            serde_json::Value::String(s) => strings.push(s.clone()),
            _ => {}
        }
    }
    let mut keys = Vec::new();
    let mut strings = Vec::new();
    walk(&doc, &mut keys, &mut strings);

    // ①无 {{name}} 模板引用（内部 tool_calls 元素形状为 {tool_name, args}）
    assert!(
        !strings.iter().any(|s| s.contains("{{name}}")),
        "constitution 不得再引用 name 模板（内部 tool_calls 形状字段是 tool_name）"
    );
    // ②无 service_name 参数键（runner handle_call_service 统一读 tool_name）
    assert!(
        !keys.iter().any(|k| k == "service_name"),
        "constitution 不得再用 service_name 作为参数键"
    );
    // ①collect 模板消费 {{tool_name}}
    assert!(
        strings.iter().any(|s| s.contains("{{tool_name}}")),
        "collect 模板必须消费 tool_name 字段"
    );
    // ②call_service io_request 携带 tool_name 键
    fn find_call_service_io<'a>(v: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
        match v {
            serde_json::Value::Object(map) => {
                if map.get("type").and_then(|t| t.as_str()) == Some("io_request") {
                    out.push(v);
                }
                for val in map.values() {
                    find_call_service_io(val, out);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    find_call_service_io(item, out);
                }
            }
            _ => {}
        }
    }
    let mut io_requests = Vec::new();
    find_call_service_io(&doc, &mut io_requests);
    assert!(
        io_requests
            .iter()
            .any(|r| r["params"]["io_type"] == "call_service"
                && r["params"].get("tool_name").is_some()),
        "call_service io_request 必须携带 tool_name 键"
    );
}

/// 端到端接线锁:tools schema 必须出现在两处请求面上 ——
/// ① submit_command 指令面(openai_tools_payload 注入,经 constitution 中继);
/// ② 真正打向 LLM API 的请求体(handle_call_external 转发 io_request params.tools)。
/// 同时验证多轮 ReAct 真实执行:LLM 回标准 tool_calls → file_write 真实落盘 →
/// 工具结果回流消息历史 → LLM 给出含路径确认的最终回复。
///
/// 匹配设计:turn1 mock 仅匹配含 "file_write" 的请求体(该词只能来自 tools schema,
/// 缺失则 404 → LLM 调用失败 → 测试红);turn2 mock 匹配含工具结果路径的请求体
/// (证明工具结果回流 LLM)。turn2 后创建,优先匹配。
#[tokio::test]
async fn test_react_loop_llm_request_carries_tools_schema() {
    // ── LLM mock 服务器 ──
    let mut llm_server = Server::new_async().await;
    // turn1:请求体必须携带 tools schema("file_write" 只能来自 tools 数组)
    let llm_turn1 = llm_server
        .mock("POST", "/")
        .match_body(mockito::Matcher::Regex("file_write".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"choices":[{"message":{"content":"","tool_calls":[
                {"id":"c1","type":"function",
                 "function":{"name":"file_write","arguments":"{\"path\":\"workspace/expenses_2026.json\",\"content\":\"amount=45.50\"}"}}]},
                "finish_reason":"tool_calls"}]}"#,
        )
        .create_async()
        .await;
    // turn2:请求消息历史必须含工具结果(真实写盘返回的 path)
    let llm_turn2 = llm_server
        .mock("POST", "/")
        .match_body(mockito::Matcher::Regex("expenses_2026".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"choices":[{"message":{"content":"Wrote workspace/expenses_2026.json (amount=45.50)"},"finish_reason":"stop"}]}"#,
        )
        .create_async()
        .await;

    // ── evorule server mock:SSE 脚本化(params.tools 模拟 constitution `tools?` 中继)──
    let mut server = Server::new_async().await;
    // mockito 1.7 匹配序:同路径多 mock 按创建序优先喂"期望未满"者,
    // 带断言的特定 mock 必须先于 base 兜底创建,否则被兜底截胡
    let tools_schema = json!([
        {
            "type": "function",
            "function": {
                "name": "file_write",
                "description": "write a file",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }
            }
        }
    ]);
    let sse = format!(
        "{}{}{}{}",
        sse_line(&io_request(
            1,
            "call_external",
            json!({"model": "mock-model", "tools": tools_schema})
        )),
        sse_line(&io_request(
            2,
            "call_service",
            json!({"tool_name": "file_write",
                   "args": {"path": "workspace/expenses_2026.json", "content": "amount=45.50"}})
        )),
        sse_line(&io_request(
            3,
            "call_external",
            json!({"model": "mock-model", "tools": tools_schema})
        )),
        sse_line(&json!({"type": "Stable"})),
    );
    // ①指令面:submit_command 必须携带工具 schema
    let cmd_mock = server
        .mock("POST", "/api/sessions/777/command")
        .match_body(mockito::Matcher::Regex("file_write".to_string()))
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;
    // state:最终 content 含路径确认(先于 base 创建以获得请求)
    let state_mock = server
        .mock("GET", "/api/sessions/777/state")
        .with_status(200)
        .with_body(
            r#"{"payload":{"llm_response":{"content":"Wrote workspace/expenses_2026.json (amount=45.50)"}}}"#,
        )
        .create_async()
        .await;
    mock_evorule_base(&mut server, "777", sse).await;

    // ── runner:真实 LlmHandler + 真实 file_write(tempdir 沙箱)──
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir(dir.path().join("workspace")).expect("workspace dir");
    let client = EvoruleApiClient::new(&server.url());
    let llm = LlmHandler::new("mock-model", &llm_server.url(), Some("k".to_string()))
        .with_retry_config(0, 0.001, 0.01);
    let mut runner = AgentRunner::new(AgentConfig::default(), client)
        .with_llm_handler(llm)
        .with_tool_handler(evo_agent::builtin_tools::default_safe_toolkit(dir.path()));

    let result = runner
        .run("record the expense")
        .await
        .expect("run should not Err");
    assert!(
        result.success,
        "multi-turn run should succeed: {:?}",
        result.error
    );
    assert_eq!(result.steps, 3, "3 IoRequest events = 3 steps");
    assert_eq!(result.tool_calls, vec!["file_write".to_string()]);
    assert!(
        result.content.contains("expenses_2026.json"),
        "final content must confirm the written path, got: {}",
        result.content
    );
    // ②LLM 请求面:两轮 mock 均按预期命中(缺 tools schema 时 turn1 必失配 → 测试红)
    llm_turn1.assert_async().await;
    llm_turn2.assert_async().await;
    // ①指令面 schema 注入命中
    cmd_mock.assert_async().await;
    state_mock.assert_async().await;
    // file_write 真实落盘
    let written = std::fs::read_to_string(dir.path().join("workspace/expenses_2026.json"))
        .expect("file_write must have created the file");
    assert!(
        written.contains("45.50"),
        "written content mismatch: {}",
        written
    );
}

/// 假成功回归锁(边界如实声明):LLM 回复含工具意图文本但无
/// tool_calls 字段时(如供应商原生 XML 形态),agent 不解析、不执行工具,
/// 原始文本作为 llm_response.content 原样回传引擎(审计留痕)——
/// 静默不执行优于幻觉执行。tools schema 注入修复后模型应返回标准 tool_calls;
/// 此锁防止"回退到 XML 时代也照常报成功"的假成功形态再次无声出现。
#[tokio::test]
async fn test_llm_text_intent_without_tool_calls_does_not_execute_tools() {
    let mut server = Server::new_async().await;
    let sse = format!(
        "{}{}",
        sse_line(&io_request(
            1,
            "call_external",
            json!({"model": "mock-model"})
        )),
        sse_line(&json!({"type": "Stable"})),
    );
    // 审计留痕:XML 原文作为 llm_response.content 回传引擎(io_response body 可查)
    // 先于 base 兜底创建,否则单次 POST 被兜底截胡(mockito 1.7 匹配序)
    let io_resp = server
        .mock("POST", "/api/sessions/777/io_response")
        .match_body(mockito::Matcher::Regex("minimax:tool_call".to_string()))
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;
    mock_evorule_base(&mut server, "777", sse).await;

    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools: BTreeMap<String, Arc<dyn ToolFunction>> = BTreeMap::new();
    tools.insert(
        "file_write".to_string(),
        Arc::new(RecordingTool {
            calls: calls.clone(),
            result: "SHOULD_NOT_EXECUTE",
        }),
    );
    let client = EvoruleApiClient::new(&server.url());
    let xml_text = r#"<minimax:tool_call>{"name":"file_write","parameters":{"path":"workspace/pwned.txt","content":"should not run"}}</minimax:tool_call>"#;
    let mut runner = AgentRunner::new(AgentConfig::default(), client)
        .with_llm_handler(LlmHandler::mock(xml_text))
        .with_tool_handler(evo_agent::io_handlers::ToolHandler::with_tools(tools));

    let result = runner
        .run("record the expense")
        .await
        .expect("run should not Err");
    assert!(result.success, "text-only reply must not fail the run");
    assert_eq!(result.steps, 1, "no tool round: single IoRequest only");
    assert!(
        result.tool_calls.is_empty(),
        "no tool must be recorded as executed"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "XML intent text must not trigger tool execution"
    );
    // 审计面:原文全量回传,不静默丢弃
    io_resp.assert_async().await;
}
