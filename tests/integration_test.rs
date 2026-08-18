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

use evo_agent::agent::runner::{AgentConfig, AgentRunner};
use evo_agent::agent::sediment::{sediment, SedimentConfig, SedimentDeps};
use evo_agent::agent::summarizer::ContextSummarizer;
use evo_agent::agent::Message;
use evo_agent::agent::memory::MemoryManager;
use evo_agent::api::evorule_client::EvoruleApiClient;
use evo_agent::io_handlers::LlmHandler;
use mockito::Server;
use serde_json::json;

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
        .mock("GET", "/api/shared/facts?prefix=shared.")
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
        .mock("GET", "/api/shared/facts?prefix=shared.")
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
        .mock("GET", "/api/shared/facts?prefix=shared.")
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
        .mock("GET", "/api/shared/facts?prefix=shared.")
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
    let mut config = AgentConfig::default();
    config.system_prompt = "Safety first assistant".to_string();
    config.tool_names = vec!["safety_check".to_string()];

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
        .match_body(mockito::Matcher::Json(json!({ "fact_ids": [1, 2, 3, 4, 5] })))
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
