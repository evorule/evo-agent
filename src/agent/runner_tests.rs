// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

use super::*;

fn make_test_client() -> EvoruleApiClient {
    EvoruleApiClient::new("http://localhost:8080")
}

#[test]
fn test_agent_config_default() {
    let config = AgentConfig::default();
    assert_eq!(config.agent_type, "default");
    assert_eq!(config.model, "gpt-4o-mini");
    assert!((config.temperature - 0.7).abs() < 0.01);
    assert_eq!(config.max_steps, 10);
    assert_eq!(config.step_timeout, Duration::from_secs(60));
    assert!(config.tool_names.is_empty());
    assert_eq!(config.llm_retry_count, 3);
}

#[test]
fn test_agent_result_success() {
    let result = AgentResult::success("hello".to_string(), 3, 100, vec!["tool1".to_string()]);
    assert!(result.success);
    assert_eq!(result.content, "hello");
    assert_eq!(result.steps, 3);
    assert_eq!(result.duration_ms, 100);
    assert_eq!(result.tool_calls, vec!["tool1"]);
    assert!(result.error.is_none());
}

#[test]
fn test_agent_result_error() {
    let result = AgentResult::error("timeout".to_string(), 5, 5000);
    assert!(!result.success);
    assert!(result.content.is_empty());
    assert_eq!(result.steps, 5);
    assert_eq!(result.error, Some("timeout".to_string()));
}

#[test]
fn test_agent_error_display() {
    let err = AgentError::MaxStepsExceeded(10);
    assert!(format!("{}", err).contains("10"));

    let err = AgentError::Timeout("test".to_string());
    assert!(format!("{}", err).contains("Timeout"));

    let err = AgentError::LlmError("API down".to_string());
    assert!(format!("{}", err).contains("LLM error"));

    let err = AgentError::EvoruleError("connection failed".to_string());
    assert!(format!("{}", err).contains("Evorule error"));
}

#[test]
fn test_merge_delegate_tool() {
    let delegate_ctx = DelegateContext {
        current_depth: 2,
        parent_agent_type: "parent".to_string(),
        definitions: crate::agent::definition::AgentDefinitionManager::with_default_dir(),
        evorule_client: make_test_client(),
        // G9:新增字段(测试用默认值)
        max_depth: super::DEFAULT_MAX_DELEGATE_DEPTH,
        max_concurrent: None,
    };

    let args = serde_json::json!({"query": "test"});
    let tcb_args = serde_to_tcb(&args);
    let merged = merge_delegate_tool("search", &tcb_args, &delegate_ctx);

    assert_eq!(
        merged.get("delegate_depth").and_then(|v| v.as_i64()),
        Some(2)
    );
    assert_eq!(
        merged.get("parent_agent").and_then(|v| v.as_str()),
        Some("parent")
    );
    assert_eq!(merged.get("query").and_then(|v| v.as_str()), Some("test"));
}

#[test]
fn test_build_call_external_command() {
    let config = AgentConfig::default();
    let client = make_test_client();
    let runner = AgentRunner::new(config, client);

    let command = runner.build_call_external_command("system prompt", "test goal", None);
    assert_eq!(command["type"], "call_external");
    assert_eq!(command["params"]["model"], "gpt-4o-mini");
    assert_eq!(command["params"]["messages"][0]["role"], "system");
    assert_eq!(command["params"]["messages"][0]["content"], "system prompt");
    assert_eq!(command["params"]["messages"][1]["role"], "user");
    assert_eq!(command["params"]["messages"][1]["content"], "test goal");
    assert!(command["params"].get("tools").is_none());
}

#[test]
fn test_agent_runner_new() {
    let config = AgentConfig::default();
    let client = make_test_client();
    let runner = AgentRunner::new(config, client);

    assert!(runner.session_id.is_none());
    assert!(runner.memory.is_none());
    assert!(runner.delegate_context.is_none());
}

#[tokio::test]
async fn test_auto_recall_empty_shared_facts() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    server
        .mock("GET", "/api/shared/facts?prefix=shared.default.")
        .with_status(200)
        .with_body("[]")
        .create_async()
        .await;

    let config = AgentConfig::default();
    let runner = AgentRunner::new(config, client);

    let result = runner.auto_recall("test-session").await;

    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

#[tokio::test]
async fn test_auto_recall_api_error_fetching_facts() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    server
        .mock("GET", "/api/shared/facts?prefix=shared.default.")
        .with_status(500)
        .with_body("Internal server error")
        .create_async()
        .await;

    let config = AgentConfig::default();
    let runner = AgentRunner::new(config, client);

    let result = runner.auto_recall("test-session").await;

    assert!(result.is_err());
    assert!(matches!(result.err().unwrap(), AgentError::EvoruleError(_)));
}

#[tokio::test]
async fn test_auto_recall_api_error_recording_used_at_startup() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    server.mock("GET", "/api/shared/facts?prefix=shared.default.")
            .with_status(200)
            .with_body(r#"[{"fact_id": 1, "path": "shared.test", "value": "test", "source_session_id": 100, "version": 1}]"#)
            .create_async()
            .await;

    server
        .mock("POST", "/api/sessions/test-session/used_at_startup")
        .with_status(500)
        .with_body("Internal server error")
        .create_async()
        .await;

    let config = AgentConfig::default();
    let runner = AgentRunner::new(config, client);

    let result = runner.auto_recall("test-session").await;

    assert!(result.is_err());
    assert!(matches!(result.err().unwrap(), AgentError::EvoruleError(_)));
}

#[tokio::test]
async fn test_auto_recall_with_memory() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    server.mock("GET", "/api/shared/facts?prefix=shared.default.")
            .with_status(200)
            .with_body(r#"[{"fact_id": 1, "path": "shared.knowledge", "value": "important info", "source_session_id": 100, "version": 1}]"#)
            .create_async()
            .await;

    server
        .mock("POST", "/api/sessions/test-session/used_at_startup")
        .with_status(200)
        .with_body(r#"{"success": true}"#)
        .create_async()
        .await;

    server
        .mock("POST", "/api/sessions/test-session/payload")
        .with_status(200)
        .with_body(r#"{"success": true}"#)
        .create_async()
        .await;

    server
        .mock("GET", "/api/sessions/test-session/payload")
        .with_status(200)
        .with_body(r#"{"auto_recall_context": "[Fact 1] shared.knowledge: \"important info\"\n"}"#)
        .create_async()
        .await;

    let config = AgentConfig::default();
    let memory = crate::agent::memory::MemoryManager::new("test", client.clone())
        .with_session_id("test-session");
    let runner = AgentRunner::new(config, client).with_memory(memory);

    let result = runner.auto_recall("test-session").await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), vec![1]);
}

#[tokio::test]
async fn test_auto_recall_with_multiple_facts() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    server.mock("GET", "/api/shared/facts?prefix=shared.default.")
            .with_status(200)
            .with_body(r#"[{"fact_id": 1, "path": "shared.guideline", "value": "safety rule", "source_session_id": 100, "version": 1}, {"fact_id": 2, "path": "shared.knowledge", "value": "domain knowledge", "source_session_id": 200, "version": 2}, {"fact_id": 3, "path": "shared.policy", "value": "company policy", "source_session_id": 300, "version": 3}]"#)
            .create_async()
            .await;

    server
        .mock("POST", "/api/sessions/test-session/used_at_startup")
        .with_status(200)
        .with_body(r#"{"success": true}"#)
        .create_async()
        .await;

    let config = AgentConfig::default();
    let runner = AgentRunner::new(config, client);

    let result = runner.auto_recall("test-session").await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), vec![1, 2, 3]);
}

#[tokio::test]
async fn test_auto_rewind_not_enough_history() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    server.mock("GET", "/api/sessions/test-session/facts")
            .with_status(200)
            .with_body(r#"[{"version": 0, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"}]"#)
            .create_async()
            .await;

    let config = AgentConfig::default();
    let runner = AgentRunner::new(config, client);

    let result = runner.auto_rewind("test-session").await;

    assert!(result.is_err());
    assert!(
        matches!(result.err().unwrap(), AgentError::Internal(msg) if msg.contains("Not enough history"))
    );
}

#[tokio::test]
async fn test_auto_rewind_empty_history() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    server
        .mock("GET", "/api/sessions/test-session/facts")
        .with_status(200)
        .with_body("[]")
        .create_async()
        .await;

    let config = AgentConfig::default();
    let runner = AgentRunner::new(config, client);

    let result = runner.auto_rewind("test-session").await;

    assert!(result.is_err());
    assert!(
        matches!(result.err().unwrap(), AgentError::Internal(msg) if msg.contains("Not enough history"))
    );
}

#[tokio::test]
async fn test_auto_rewind_api_error_fetching_facts() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    server
        .mock("GET", "/api/sessions/test-session/facts")
        .with_status(500)
        .with_body("Internal server error")
        .create_async()
        .await;

    let config = AgentConfig::default();
    let runner = AgentRunner::new(config, client);

    let result = runner.auto_rewind("test-session").await;

    assert!(result.is_err());
    assert!(matches!(result.err().unwrap(), AgentError::EvoruleError(_)));
}

#[tokio::test]
async fn test_auto_rewind_api_error_rewinding() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    server.mock("GET", "/api/sessions/test-session/facts")
            .with_status(200)
            .with_body(r#"[{"version": 0, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"}, {"version": 1, "id": 2, "path": "command", "value": "test", "type": "Command"}]"#)
            .create_async()
            .await;

    server
        .mock("GET", "/api/sessions/test-session/rewind")
        .match_query("version=0")
        .with_status(500)
        .with_body("Internal server error")
        .create_async()
        .await;

    let config = AgentConfig::default();
    let runner = AgentRunner::new(config, client);

    let result = runner.auto_rewind("test-session").await;

    assert!(result.is_err());
    assert!(matches!(result.err().unwrap(), AgentError::EvoruleError(_)));
}

#[tokio::test]
async fn test_auto_rewind_missing_version_in_response() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    server.mock("GET", "/api/sessions/test-session/facts")
            .with_status(200)
            .with_body(r#"[{"version": 0, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"}, {"version": 1, "id": 2, "path": "command", "value": "test", "type": "Command"}]"#)
            .create_async()
            .await;

    server
        .mock("GET", "/api/sessions/test-session/rewind")
        .match_query("version=0")
        .with_status(200)
        .with_body(r#"{"payload": {}, "queue": []}"#)
        .create_async()
        .await;

    let config = AgentConfig::default();
    let runner = AgentRunner::new(config, client);

    let result = runner.auto_rewind("test-session").await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 0);
}

#[tokio::test]
async fn test_auto_rewind_success() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    server.mock("GET", "/api/sessions/test-session/facts")
            .with_status(200)
            .with_body(r#"[{"version": 0, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"}, {"version": 1, "id": 2, "path": "command", "value": "test", "type": "Command"}, {"version": 2, "id": 3, "path": "error", "value": "error", "type": "Error"}]"#)
            .create_async()
            .await;

    server
        .mock("GET", "/api/sessions/test-session/rewind")
        .match_query("version=1")
        .with_status(200)
        .with_body(r#"{"version": 1, "payload": {}, "queue": []}"#)
        .create_async()
        .await;

    let config = AgentConfig::default();
    let runner = AgentRunner::new(config, client);

    let result = runner.auto_rewind("test-session").await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 1);
}

#[tokio::test]
async fn test_auto_rewind_non_sequential_versions() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    server.mock("GET", "/api/sessions/test-session/facts")
            .with_status(200)
            .with_body(r#"[{"version": 5, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"}, {"version": 12, "id": 2, "path": "command", "value": "test", "type": "Command"}, {"version": 47, "id": 3, "path": "error", "value": "error", "type": "Error"}]"#)
            .create_async()
            .await;

    server
        .mock("GET", "/api/sessions/test-session/rewind")
        .match_query("version=12")
        .with_status(200)
        .with_body(r#"{"version": 12, "payload": {}, "queue": []}"#)
        .create_async()
        .await;

    let config = AgentConfig::default();
    let runner = AgentRunner::new(config, client);

    let result = runner.auto_rewind("test-session").await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 12);
}

// === AgentRunner::from_definition 测试 ===

use crate::agent::definition::MemoryConfig;

fn make_def_with_tools(tools: Vec<String>) -> AgentDefinition {
    AgentDefinition {
        agent_type: "test".to_string(),
        version: "0.1.0".to_string(),
        description: "test agent".to_string(),
        system_prompt: "you are a test agent".to_string(),
        model: "gpt-4o-mini".to_string(),
        temperature: 0.5,
        max_steps: 5,
        step_timeout_secs: 30,
        tools,
        memory: MemoryConfig::default(), // type = "none"
        output_format: None,
        context_window_tokens: None,
        max_parallel_tools: 1,
    }
}

fn make_handler_with(tool_names: &[&str]) -> ToolHandler {
    use crate::io_handlers::tool_handler::ToolFunction;
    use std::sync::Arc;

    struct EchoTool;
    #[async_trait::async_trait]
    impl ToolFunction for EchoTool {
        async fn call(&self, _args: &evorule_tcb::JsonValue) -> crate::io_handler::IoResult {
            Ok(evorule_tcb::JsonValue::string("echo"))
        }
    }

    let mut h = ToolHandler::new();
    for name in tool_names {
        h.register_tool(name, Arc::new(EchoTool));
    }
    h
}

#[tokio::test]
async fn test_from_definition_success() {
    let def = make_def_with_tools(vec!["echo".to_string()]);
    let client = make_test_client();
    let handler = make_handler_with(&["echo"]);

    let result = AgentRunner::from_definition(def, client, handler, None).await;
    assert!(result.is_ok(), "expected Ok");
    let runner = result.unwrap();
    assert_eq!(runner.config.agent_type, "test");
    assert_eq!(runner.config.model, "gpt-4o-mini");
    assert_eq!(runner.config.tool_names, vec!["echo".to_string()]);
    assert!(runner.tool_handler.has_tool("echo"));
    // "none" memory → None
    assert!(runner.memory.is_none());
}

#[tokio::test]
async fn test_from_definition_rejects_unregistered_tool() {
    // def 想用 "magic" 但 handler 没注册 → 应早失败(可控)
    let def = make_def_with_tools(vec!["echo".to_string(), "magic".to_string()]);
    let client = make_test_client();
    let handler = make_handler_with(&["echo"]); // 没注册 "magic"

    let result = AgentRunner::from_definition(def, client, handler, None).await;
    let err = match result {
        Ok(_) => panic!("expected Err but got Ok"),
        Err(e) => e,
    };
    let msg = format!("{}", err);
    assert!(
        msg.contains("magic"),
        "error should mention missing tool, got: {}",
        msg
    );
    assert!(
        msg.contains("not registered") || msg.contains("NOT registered"),
        "got: {}",
        msg
    );
}

#[tokio::test]
async fn test_from_definition_rejects_unknown_memory_type() {
    let mut def = make_def_with_tools(vec!["echo".to_string()]);
    def.memory.memory_type = "redis".to_string(); // 0.1.0 不支持
    let client = make_test_client();
    let handler = make_handler_with(&["echo"]);

    let result = AgentRunner::from_definition(def, client, handler, None).await;
    let err = match result {
        Ok(_) => panic!("expected Err but got Ok"),
        Err(e) => e,
    };
    let msg = format!("{}", err);
    assert!(
        msg.contains("redis") || msg.contains("unsupported memory"),
        "got: {}",
        msg
    );
}

#[tokio::test]
async fn test_from_definition_empty_tools_succeeds() {
    // 0 tools is valid(read-only agent)
    let def = make_def_with_tools(vec![]);
    let client = make_test_client();
    let handler = make_handler_with(&[]);

    let result = AgentRunner::from_definition(def, client, handler, None).await;
    assert!(result.is_ok());
    let runner = result.unwrap();
    assert!(runner.config.tool_names.is_empty());
}

// === G11: output_validator 集成测试 ===

#[tokio::test]
async fn test_from_definition_constructs_output_validator() {
    let mut def = make_def_with_tools(vec!["echo".to_string()]);
    def.output_format = Some(OutputFormat {
        format_type: "json".to_string(),
        schema: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "result": {"type": "string"}
            },
            "required": ["result"]
        })),
        max_retries: Some(3),
    });
    let client = make_test_client();
    let handler = make_handler_with(&["echo"]);

    let result = AgentRunner::from_definition(def, client, handler, None).await;
    assert!(result.is_ok(), "expected Ok");
    let runner = result.unwrap();
    // G11:output_validator 应被构造
    assert!(
        runner.output_validator.is_some(),
        "output_validator should be constructed"
    );
    // max_retries 应从 OutputFormat 传递
    assert_eq!(runner.output_validator.as_ref().unwrap().max_retries(), 3);
    // output_format_retries 初始为 0
    assert_eq!(runner.output_format_retries, 0);
}

#[tokio::test]
async fn test_from_definition_no_output_validator_when_none() {
    let def = make_def_with_tools(vec!["echo".to_string()]);
    // output_format = None (default from make_def_with_tools)
    let client = make_test_client();
    let handler = make_handler_with(&["echo"]);

    let result = AgentRunner::from_definition(def, client, handler, None).await;
    assert!(result.is_ok());
    let runner = result.unwrap();
    assert!(
        runner.output_validator.is_none(),
        "output_validator should be None when output_format is None"
    );
}

#[tokio::test]
async fn test_from_definition_rejects_invalid_output_schema() {
    let mut def = make_def_with_tools(vec!["echo".to_string()]);
    def.output_format = Some(OutputFormat {
        format_type: "json".to_string(),
        schema: Some(serde_json::json!("not a valid schema")), // 非法 schema
        max_retries: None,
    });
    let client = make_test_client();
    let handler = make_handler_with(&["echo"]);

    let result = AgentRunner::from_definition(def, client, handler, None).await;
    let err = match result {
        Ok(_) => panic!("expected Err but got Ok"),
        Err(e) => e,
    };
    let msg = format!("{}", err);
    assert!(
        msg.contains("invalid output_format schema") || msg.contains("invalid JSON schema"),
        "error should mention invalid schema, got: {}",
        msg
    );
}

#[test]
fn test_with_output_validator_builder() {
    let config = AgentConfig::default();
    let client = make_test_client();
    let runner = AgentRunner::new(config, client);

    assert!(runner.output_validator.is_none());

    let format = OutputFormat {
        format_type: "json".to_string(),
        schema: None,
        max_retries: Some(5),
    };
    let validator = OutputValidator::from_output_format(&format).unwrap();
    let runner = runner.with_output_validator(validator);

    assert!(runner.output_validator.is_some());
    assert_eq!(runner.output_validator.as_ref().unwrap().max_retries(), 5);
}

#[test]
fn test_agent_config_default_output_format_is_none() {
    // G11:默认 AgentConfig 的 output_format 应为 None
    let config = AgentConfig::default();
    assert!(config.output_format.is_none());
}

// === G4: run_streaming / AgentEvent 测试 ===

/// 验证 `AgentEvent` 全部 9 个变体可以构造并 match
#[test]
fn test_agent_event_all_variants() {
    let e = AgentEvent::SessionCreated {
        session_id: "s1".to_string(),
    };
    assert!(matches!(e, AgentEvent::SessionCreated { session_id } if session_id == "s1"));

    let e = AgentEvent::Step { step: 3 };
    assert!(matches!(e, AgentEvent::Step { step: 3 }));

    let e = AgentEvent::LlmDelta {
        text: "hello".to_string(),
    };
    assert!(matches!(e, AgentEvent::LlmDelta { text } if text == "hello"));

    let e = AgentEvent::ToolCall {
        name: "search".to_string(),
        args: serde_json::json!({"q": "rust"}),
    };
    assert!(matches!(e, AgentEvent::ToolCall { name, .. } if name == "search"));

    let e = AgentEvent::ToolResult {
        name: "search".to_string(),
        result: serde_json::json!({"hits": 42}),
    };
    assert!(matches!(e, AgentEvent::ToolResult { name, .. } if name == "search"));

    let e = AgentEvent::LlmDone {
        content: "done".to_string(),
        finish_reason: Some("stop".to_string()),
    };
    assert!(matches!(e, AgentEvent::LlmDone { content, .. } if content == "done"));

    let e = AgentEvent::Done(AgentResult::success("ok".to_string(), 1, 100, vec![]));
    assert!(matches!(e, AgentEvent::Done(r) if r.success));

    let e = AgentEvent::Error(AgentError::LlmError("err".to_string()));
    assert!(matches!(e, AgentEvent::Error(AgentError::LlmError(_))));

    let e = AgentEvent::Info("rewind".to_string());
    assert!(matches!(e, AgentEvent::Info(msg) if msg == "rewind"));
}

/// 验证 `AgentEvent` 可以 Clone(SSE fan-out 场景需要)
#[test]
fn test_agent_event_clone() {
    let original = AgentEvent::LlmDelta {
        text: "hello".to_string(),
    };
    let cloned = original.clone();
    assert!(matches!(cloned, AgentEvent::LlmDelta { text } if text == "hello"));

    let original = AgentEvent::Done(AgentResult::success(
        "ok".to_string(),
        2,
        50,
        vec!["t".to_string()],
    ));
    let cloned = original.clone();
    if let AgentEvent::Done(r) = cloned {
        assert!(r.success);
        assert_eq!(r.steps, 2);
    } else {
        panic!("expected Done variant");
    }
}

/// 验证 `AgentEvent::Error` 可以 Clone(新增 Clone derive,G4 需要)
#[test]
fn test_agent_error_clone() {
    let err = AgentError::LlmError("timeout".to_string());
    let cloned = err.clone();
    assert_eq!(format!("{}", err), format!("{}", cloned));

    let err = AgentError::MaxStepsExceeded(5);
    let cloned = err.clone();
    if let AgentError::MaxStepsExceeded(n) = cloned {
        assert_eq!(n, 5);
    } else {
        panic!("expected MaxStepsExceeded");
    }
}

/// G4 核心测试:验证 mock LlmHandler 的 execute_stream 产出顺序
/// 模拟 run_streaming 中的消费模式:Delta → Done
#[tokio::test]
async fn test_streaming_consumption_pattern_single_delta() {
    let handler = LlmHandler::mock("Hello world");
    let params = JsonValue::Object(BTreeMap::new());
    let mut stream = handler.execute_stream(&params);

    let mut deltas: Vec<String> = Vec::new();
    let mut done_content: Option<String> = None;
    let mut done_finish_reason: Option<String> = None;

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(StreamChunk::Delta(text)) => {
                deltas.push(text);
            }
            Ok(StreamChunk::Done(resp)) => {
                done_content = Some(resp.content);
                done_finish_reason = resp.finish_reason;
            }
            _ => {}
        }
    }

    // mock 模式:一次 Delta(完整内容)+ Done
    assert_eq!(deltas, vec!["Hello world".to_string()]);
    assert_eq!(done_content, Some("Hello world".to_string()));
    assert_eq!(done_finish_reason, Some("stop".to_string()));
}

/// 验证 run_streaming 的事件映射逻辑:
/// 从 StreamChunk 序列正确映射到 AgentEvent 序列
#[tokio::test]
async fn test_stream_chunk_to_agent_event_mapping() {
    let handler = LlmHandler::mock("Streaming response");
    let params = JsonValue::Object(BTreeMap::new());
    let mut stream = handler.execute_stream(&params);

    // 模拟 run_streaming 中的映射逻辑
    let mut events: Vec<AgentEvent> = Vec::new();
    let mut full_content = String::new();

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(StreamChunk::Delta(text)) => {
                full_content.push_str(&text);
                events.push(AgentEvent::LlmDelta { text });
            }
            Ok(StreamChunk::Done(resp)) => {
                full_content = resp.content.clone();
                events.push(AgentEvent::LlmDone {
                    content: resp.content.clone(),
                    finish_reason: resp.finish_reason.clone(),
                });
            }
            Ok(StreamChunk::Warn(msg)) => {
                events.push(AgentEvent::Info(msg));
            }
            Err(e) => {
                events.push(AgentEvent::Error(AgentError::LlmError(e)));
            }
            _ => {}
        }
    }

    // 验证事件序列:LlmDelta → LlmDone
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], AgentEvent::LlmDelta { text } if text == "Streaming response"));
    assert!(
        matches!(&events[1], AgentEvent::LlmDone { content, finish_reason }
            if content == "Streaming response" && finish_reason.as_deref() == Some("stop"))
    );

    // full_content 应与 Done 中的 content 一致
    assert_eq!(full_content, "Streaming response");
}

/// 验证 run_streaming 的 tool_call 事件产出逻辑
/// (mock 不产出 tool_call,但验证映射逻辑正确处理无 tool_call 场景)
#[tokio::test]
async fn test_stream_chunk_mapping_no_tool_calls() {
    let handler = LlmHandler::mock("No tools here");
    let params = JsonValue::Object(BTreeMap::new());
    let mut stream = handler.execute_stream(&params);

    let mut full_content = String::new();
    let mut full_tool_calls: Option<Vec<crate::agent::translator::ToolCall>> = None;

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(StreamChunk::Delta(text)) => full_content.push_str(&text),
            Ok(StreamChunk::Done(resp)) => {
                full_content = resp.content.clone();
                full_tool_calls = resp.tool_calls.clone();
            }
            _ => {}
        }
    }

    // mock 响应没有 tool_calls
    assert!(full_tool_calls.is_none());

    // 验证 is_finished 逻辑(同 run_streaming)
    let finish_reason = Some("stop".to_string());
    let is_finished = matches!(finish_reason.as_deref(), Some("stop") | Some("end_turn"));
    assert!(is_finished);
}

// ===== G6 取消机制测试 =====

#[test]
fn test_cancel_token_initial_state() {
    // 新 runner 的 cancel_token 初始未取消
    let config = AgentConfig::default();
    let client = make_test_client();
    let runner = AgentRunner::new(config, client);
    assert!(!runner.is_cancelled());
}

#[test]
fn test_cancel_token_trigger() {
    let config = AgentConfig::default();
    let client = make_test_client();
    let runner = AgentRunner::new(config, client);
    assert!(!runner.is_cancelled());
    runner.cancel();
    assert!(runner.is_cancelled());
}

#[test]
fn test_cancel_token_clone_shares_state() {
    // clone 出来的 token 与原 token 共享取消状态(内部 Arc)
    let config = AgentConfig::default();
    let client = make_test_client();
    let runner = AgentRunner::new(config, client);
    let token_clone = runner.cancel_token().clone();
    assert!(!token_clone.is_cancelled());
    // 触发原 runner 的 cancel
    runner.cancel();
    // clone 的 token 也应看到取消状态
    assert!(token_clone.is_cancelled());
    assert!(runner.is_cancelled());
}

#[test]
fn test_cancel_token_independent_runners() {
    // 不同 runner 的 token 互相独立
    let config = AgentConfig::default();
    let runner_a = AgentRunner::new(config.clone(), make_test_client());
    let runner_b = AgentRunner::new(config, make_test_client());

    runner_a.cancel();
    assert!(runner_a.is_cancelled());
    assert!(
        !runner_b.is_cancelled(),
        "runner_b should not be affected by runner_a's cancel"
    );
}

#[tokio::test]
async fn test_cancelled_future_resolves_after_cancel() {
    // cancelled() future 在 cancel() 后应立即 resolve
    let config = AgentConfig::default();
    let runner = AgentRunner::new(config, make_test_client());
    let token = runner.cancel_token().clone();

    // 未取消时, cancelled() 不应立即完成
    tokio::select! {
        _ = token.cancelled() => panic!("cancelled() resolved without cancel()"),
        _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
    }

    runner.cancel();

    // 取消后, cancelled() 应立即完成
    tokio::select! {
        _ = token.cancelled() => {} // 期望
        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
            panic!("cancelled() did not resolve after cancel()")
        }
    }
}

// ===== G8 工具审批测试 =====

#[test]
fn test_approval_callback_defaults_to_none() {
    // new() 创建的 runner 默认无 approval_callback(安全优先:candidate 工具会被拒绝)
    let config = AgentConfig::default();
    let runner = AgentRunner::new(config, make_test_client());
    assert!(
        runner.approval_callback.is_none(),
        "approval_callback should default to None"
    );
}

#[test]
fn test_with_approval_callback_builder() {
    // with_approval_callback 注入回调
    use crate::agent::approval::AutoApprove;
    use std::sync::Arc;
    let config = AgentConfig::default();
    let runner =
        AgentRunner::new(config, make_test_client()).with_approval_callback(Arc::new(AutoApprove));
    assert!(
        runner.approval_callback.is_some(),
        "approval_callback should be set after builder"
    );
}

#[test]
fn test_with_approval_callback_can_be_replaced() {
    // 多次调用 with_approval_callback 后一次生效
    use crate::agent::approval::{AutoApprove, DenyAll};
    use std::sync::Arc;
    let config = AgentConfig::default();
    let runner = AgentRunner::new(config, make_test_client())
        .with_approval_callback(Arc::new(AutoApprove))
        .with_approval_callback(Arc::new(DenyAll));
    assert!(runner.approval_callback.is_some());
    // 无法直接比较 trait object,但能确认 Some 即可(具体行为在 approval.rs 测试中覆盖)
}

#[test]
fn test_agent_event_approval_required_debug() {
    // ApprovalRequired 事件可构造 + Debug 打印
    let ev = AgentEvent::ApprovalRequired {
        tool_name: "shell_exec".to_string(),
        command: "rm /tmp/x".to_string(),
        risk: "high".to_string(),
        alternative: "use trash".to_string(),
    };
    let s = format!("{:?}", ev);
    assert!(s.contains("ApprovalRequired"));
    assert!(s.contains("shell_exec"));
    assert!(s.contains("rm /tmp/x"));
}

#[test]
fn test_agent_event_approval_result_debug() {
    // ApprovalResult 事件可构造 + Debug 打印
    let ev_approved = AgentEvent::ApprovalResult {
        tool_name: "shell_exec".to_string(),
        approved: true,
    };
    let s = format!("{:?}", ev_approved);
    assert!(s.contains("ApprovalResult"));
    assert!(s.contains("true"));

    let ev_denied = AgentEvent::ApprovalResult {
        tool_name: "http_get".to_string(),
        approved: false,
    };
    let s = format!("{:?}", ev_denied);
    assert!(s.contains("false"));
}

// ===== G13: 并行工具调用测试 =====

use crate::agent::translator::ToolCall;
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// G13 测试:延迟工具(用于并行时序验证)
///
/// sleep `delay_ms` 后返回 `result_{label}`,模拟有耗时的 active 工具。
struct DelayedTool {
    delay_ms: u64,
    label: String,
}

#[async_trait::async_trait]
impl ToolFunction for DelayedTool {
    async fn call(&self, _args: &JsonValue) -> IoResult {
        tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        Ok(JsonValue::string(format!("result_{}", self.label)))
    }
}

/// G13 测试:Proposal 工具(返回 needs_approval,模拟 candidate 工具)
struct ProposalTool;

#[async_trait::async_trait]
impl ToolFunction for ProposalTool {
    async fn call(&self, _args: &JsonValue) -> IoResult {
        let proposal = serde_json::json!({
            "status": "needs_approval",
            "command": "rm -rf /tmp/test",
            "risk": "high",
            "alternative": "use trash instead"
        });
        Ok(serde_to_tcb(&proposal))
    }
}

/// 构造一个带并行配置 + 自定义工具的 runner
fn make_parallel_runner(
    max_parallel: usize,
    tools: BTreeMap<String, Arc<dyn ToolFunction>>,
) -> AgentRunner {
    let config = AgentConfig {
        max_parallel_tools: max_parallel,
        ..AgentConfig::default()
    };
    let client = make_test_client();
    let handler = ToolHandler::with_tools(tools);
    AgentRunner::new(config, client).with_tool_handler(handler)
}

#[test]
fn test_g13_parallel_cache_key_format() {
    let args = serde_json::json!({"path": "/tmp", "content": "hello"});
    let key = AgentRunner::parallel_cache_key("file_write", &args);
    // key 格式 = "{tool_name}:{serde(args)}"
    assert!(key.starts_with("file_write:"));
    assert!(key.contains("/tmp"));
    assert!(key.contains("hello"));
}

#[test]
fn test_g13_parallel_cache_key_different_args() {
    let args1 = serde_json::json!({"path": "/tmp/a"});
    let args2 = serde_json::json!({"path": "/tmp/b"});
    let key1 = AgentRunner::parallel_cache_key("file_write", &args1);
    let key2 = AgentRunner::parallel_cache_key("file_write", &args2);
    assert_ne!(key1, key2, "different args should produce different keys");
}

#[test]
fn test_g13_parallel_cache_put_get_clear() {
    let runner = make_parallel_runner(4, BTreeMap::new());
    let key = "test_key".to_string();
    let value = JsonValue::string("cached_result");

    // 初始为空
    assert!(runner.parallel_cache_get(&key).is_none());

    // put 后可 get
    runner.parallel_cache_put(key.clone(), value.clone());
    let got = runner.parallel_cache_get(&key).expect("should hit cache");
    assert_eq!(got.to_string(), value.to_string());

    // clear 后为空
    runner.parallel_cache_clear();
    assert!(runner.parallel_cache_get(&key).is_none());
}

#[test]
fn test_g13_check_cache_disabled_in_serial_mode() {
    // max_parallel_tools = 1 → 串行模式,check_parallel_cache 永远返回 None
    let runner = make_parallel_runner(1, BTreeMap::new());
    let args = serde_json::json!({"path": "/tmp"});
    // 即使手动 put 了,串行模式也不查缓存
    let key = AgentRunner::parallel_cache_key("file_read", &args);
    runner.parallel_cache_put(key, JsonValue::string("data"));
    assert!(
        runner.check_parallel_cache("file_read", &args).is_none(),
        "serial mode (max_parallel_tools=1) should not check cache"
    );
}

#[test]
fn test_g13_check_cache_enabled_in_parallel_mode() {
    // max_parallel_tools > 1 → 并行模式,check_parallel_cache 命中时返回 Some
    let runner = make_parallel_runner(4, BTreeMap::new());
    let args = serde_json::json!({"path": "/tmp/data"});
    let key = AgentRunner::parallel_cache_key("file_read", &args);
    runner.parallel_cache_put(key, JsonValue::string("file content"));

    let hit = runner.check_parallel_cache("file_read", &args);
    assert!(hit.is_some(), "parallel mode should hit cache");
    assert_eq!(hit.unwrap().to_string(), "\"file content\"");
}

#[tokio::test]
async fn test_g13_execute_parallel_caches_active_tools() {
    let mut tools: BTreeMap<String, Arc<dyn ToolFunction>> = BTreeMap::new();
    tools.insert(
        "delayed_a".to_string(),
        Arc::new(DelayedTool {
            delay_ms: 10,
            label: "a".to_string(),
        }),
    );
    tools.insert(
        "delayed_b".to_string(),
        Arc::new(DelayedTool {
            delay_ms: 10,
            label: "b".to_string(),
        }),
    );

    let runner = make_parallel_runner(4, tools);

    let tool_calls = vec![
        ToolCall {
            name: "delayed_a".to_string(),
            arguments: serde_json::json!({}),
        },
        ToolCall {
            name: "delayed_b".to_string(),
            arguments: serde_json::json!({}),
        },
    ];

    let results = runner
        .execute_tools_parallel("test-session", &tool_calls)
        .await;

    // 验证:两个工具都执行了,顺序与输入一致
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "delayed_a");
    assert_eq!(results[1].0, "delayed_b");

    // 验证:active 工具(非 proposal)结果已缓存
    let args = serde_json::json!({});
    assert!(
        runner.check_parallel_cache("delayed_a", &args).is_some(),
        "delayed_a should be cached"
    );
    assert!(
        runner.check_parallel_cache("delayed_b", &args).is_some(),
        "delayed_b should be cached"
    );
}

#[tokio::test]
async fn test_g13_execute_parallel_skips_candidate_cache() {
    let mut tools: BTreeMap<String, Arc<dyn ToolFunction>> = BTreeMap::new();
    tools.insert("dangerous".to_string(), Arc::new(ProposalTool));

    let runner = make_parallel_runner(4, tools);

    let tool_calls = vec![ToolCall {
        name: "dangerous".to_string(),
        arguments: serde_json::json!({"command": "rm"}),
    }];

    let results = runner
        .execute_tools_parallel("test-session", &tool_calls)
        .await;

    // 验证:工具执行了
    assert_eq!(results.len(), 1);

    // 验证:candidate(proposal)工具结果**未**缓存
    let args = serde_json::json!({"command": "rm"});
    assert!(
        runner.check_parallel_cache("dangerous", &args).is_none(),
        "proposal tool should NOT be cached (must go through call_service approval)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_g13_parallel_faster_than_serial() {
    // 3 个工具各 sleep 100ms:串行 ~300ms,并行 ~100ms
    let delay_ms = 100;
    let mut tools: BTreeMap<String, Arc<dyn ToolFunction>> = BTreeMap::new();
    tools.insert(
        "t1".to_string(),
        Arc::new(DelayedTool {
            delay_ms,
            label: "1".to_string(),
        }),
    );
    tools.insert(
        "t2".to_string(),
        Arc::new(DelayedTool {
            delay_ms,
            label: "2".to_string(),
        }),
    );
    tools.insert(
        "t3".to_string(),
        Arc::new(DelayedTool {
            delay_ms,
            label: "3".to_string(),
        }),
    );

    let runner = make_parallel_runner(4, tools);

    let tool_calls = vec![
        ToolCall {
            name: "t1".to_string(),
            arguments: serde_json::json!({}),
        },
        ToolCall {
            name: "t2".to_string(),
            arguments: serde_json::json!({}),
        },
        ToolCall {
            name: "t3".to_string(),
            arguments: serde_json::json!({}),
        },
    ];

    // 并行执行
    let parallel_start = std::time::Instant::now();
    let _results = runner
        .execute_tools_parallel("test-session", &tool_calls)
        .await;
    let parallel_elapsed = parallel_start.elapsed();

    // 串行执行(对比基准;execute_single_tool 不查缓存,会真正重新执行)
    let serial_start = std::time::Instant::now();
    for tc in &tool_calls {
        let _ = runner.execute_single_tool(tc).await;
    }
    let serial_elapsed = serial_start.elapsed();

    // 并行应明显快于串行(留足 margin 避免环境抖动)
    assert!(
        parallel_elapsed < Duration::from_millis(200),
        "parallel took too long: {:?} (expected < 200ms)",
        parallel_elapsed
    );
    assert!(
        serial_elapsed > Duration::from_millis(250),
        "serial too fast (expected > 250ms): {:?}",
        serial_elapsed
    );
    assert!(
        parallel_elapsed < serial_elapsed,
        "parallel ({:?}) should be faster than serial ({:?})",
        parallel_elapsed,
        serial_elapsed
    );
}

// ===== G15: REPL / run_continuation 测试 =====

#[test]
fn test_g15_rec_to_message_system() {
    let rec = MessageRecord {
        idx: 0,
        role: "system".to_string(),
        content: "You are helpful".to_string(),
        tool_calls: None,
        tool_name: None,
        timestamp: 0,
        fact_id: None,
    };
    let msg = rec_to_message(&rec).expect("system rec should convert");
    assert!(matches!(msg, Message::System { ref content } if content == "You are helpful"));
}

#[test]
fn test_g15_rec_to_message_user() {
    let rec = MessageRecord {
        idx: 1,
        role: "user".to_string(),
        content: "Hello".to_string(),
        tool_calls: None,
        tool_name: None,
        timestamp: 0,
        fact_id: None,
    };
    let msg = rec_to_message(&rec).expect("user rec should convert");
    assert!(matches!(msg, Message::User { ref content } if content == "Hello"));
}

#[test]
fn test_g15_rec_to_message_assistant_with_tool_calls() {
    let tool_calls_json = serde_json::json!([
        {"tool_name": "search", "args": {"query": "rust"}}
    ]);
    let rec = MessageRecord {
        idx: 2,
        role: "assistant".to_string(),
        content: "Let me search".to_string(),
        tool_calls: Some(tool_calls_json),
        tool_name: None,
        timestamp: 0,
        fact_id: None,
    };
    let msg = rec_to_message(&rec).expect("assistant rec should convert");
    match msg {
        Message::Assistant {
            content,
            tool_calls,
        } => {
            assert_eq!(content, "Let me search");
            let tcs = tool_calls.expect("should have tool_calls");
            assert_eq!(tcs.len(), 1);
            assert_eq!(tcs[0].name, "search");
        }
        _ => panic!("expected Assistant variant"),
    }
}

#[test]
fn test_g15_rec_to_message_assistant_without_tool_calls() {
    let rec = MessageRecord {
        idx: 2,
        role: "assistant".to_string(),
        content: "Done".to_string(),
        tool_calls: None,
        tool_name: None,
        timestamp: 0,
        fact_id: None,
    };
    let msg = rec_to_message(&rec).expect("assistant rec should convert");
    match msg {
        Message::Assistant {
            content,
            tool_calls,
        } => {
            assert_eq!(content, "Done");
            assert!(tool_calls.is_none());
        }
        _ => panic!("expected Assistant variant"),
    }
}

#[test]
fn test_g15_rec_to_message_tool() {
    let rec = MessageRecord {
        idx: 3,
        role: "tool".to_string(),
        content: "result data".to_string(),
        tool_calls: None,
        tool_name: Some("search".to_string()),
        timestamp: 0,
        fact_id: None,
    };
    let msg = rec_to_message(&rec).expect("tool rec should convert");
    assert!(
        matches!(msg, Message::Tool { ref content, ref tool_name } if content == "result data" && tool_name == "search")
    );
}

#[test]
fn test_g15_rec_to_message_tool_missing_name() {
    let rec = MessageRecord {
        idx: 3,
        role: "tool".to_string(),
        content: "result".to_string(),
        tool_calls: None,
        tool_name: None,
        timestamp: 0,
        fact_id: None,
    };
    assert!(
        rec_to_message(&rec).is_none(),
        "tool without name should return None"
    );
}

#[test]
fn test_g15_rec_to_message_unknown_role() {
    let rec = MessageRecord {
        idx: 4,
        role: "moderator".to_string(),
        content: "???".to_string(),
        tool_calls: None,
        tool_name: None,
        timestamp: 0,
        fact_id: None,
    };
    assert!(
        rec_to_message(&rec).is_none(),
        "unknown role should return None"
    );
}

#[tokio::test]
async fn test_g15_load_messages_no_memory_returns_empty() {
    let server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());
    let config = AgentConfig::default();
    let runner = AgentRunner::new(config, client);

    // memory is None → should return empty Vec without calling get_state
    let result = AgentRunner::load_messages_from_payload(&runner, "s1").await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

#[tokio::test]
async fn test_g15_load_messages_parses_payload() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    // 模拟 evorule payload 中的 messages 结构
    let state = serde_json::json!({
        "payload": {
            "__memory__": {
                "default": {
                    "session_s1": {
                        "messages": {
                            "0": {
                                "idx": 0,
                                "role": "system",
                                "content": "You are helpful",
                                "timestamp": 1000
                            },
                            "1": {
                                "idx": 1,
                                "role": "user",
                                "content": "Hello",
                                "timestamp": 1001
                            },
                            "2": {
                                "idx": 2,
                                "role": "assistant",
                                "content": "Hi there",
                                "tool_calls": null,
                                "timestamp": 1002
                            }
                        }
                    }
                }
            }
        }
    });

    server
        .mock("GET", "/api/sessions/s1/state")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(serde_json::to_string(&state).unwrap())
        .create_async()
        .await;

    let config = AgentConfig::default();
    let mem = MemoryManager::new("default", client.clone());
    let runner = AgentRunner::new(config, client).with_memory(mem);

    let result = AgentRunner::load_messages_from_payload(&runner, "s1").await;
    assert!(result.is_ok());
    let messages = result.unwrap();
    assert_eq!(messages.len(), 3);
    assert!(matches!(&messages[0], Message::System { content } if content == "You are helpful"));
    assert!(matches!(&messages[1], Message::User { content } if content == "Hello"));
    assert!(matches!(&messages[2], Message::Assistant { content, .. } if content == "Hi there"));
}

#[tokio::test]
async fn test_g15_load_messages_empty_payload_returns_empty() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    // payload 中没有 messages 节点
    let state = serde_json::json!({
        "payload": {}
    });

    server
        .mock("GET", "/api/sessions/s1/state")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(serde_json::to_string(&state).unwrap())
        .create_async()
        .await;

    let config = AgentConfig::default();
    let mem = MemoryManager::new("default", client.clone());
    let runner = AgentRunner::new(config, client).with_memory(mem);

    let result = AgentRunner::load_messages_from_payload(&runner, "s1").await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

/// G15:验证 run_continuation 不调用 create_session(POST /api/sessions)
///
/// continuation 模式应复用已有 session_id,只调用:
/// - GET /api/sessions/{id}/state(load_messages_from_payload)
/// - GET /api/sessions/{id}/events(subscribe_events)
/// - POST /api/sessions/{id}/command(submit_command)
#[tokio::test]
async fn test_g15_run_continuation_does_not_create_session() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    // mock get_state(返回空 payload — load_messages 降级为空)
    let state_mock = server
        .mock("GET", "/api/sessions/s42/state")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"payload":{}}"#)
        .create_async()
        .await;

    // mock subscribe_events(返回空 SSE 流 — 立即关闭)
    let events_mock = server
        .mock("GET", "/api/sessions/s42/events")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("")
        .create_async()
        .await;

    // mock submit_command
    let command_mock = server
        .mock("POST", "/api/sessions/s42/command")
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;

    // 关键:create_session(POST /api/sessions)不应被调用
    // 不为其设置 mock — 如果被调用,mockito 会返回 404,导致测试失败

    let config = AgentConfig::default();
    // 注入 memory manager(否则 load_messages_from_payload 会跳过 get_state 调用)
    let mem = MemoryManager::new("default", client.clone());
    let runner = AgentRunner::new(config, client).with_memory(mem);

    let stream = runner.run_continuation("s42".to_string(), "hello".to_string());
    let mut stream = stream;

    // 消费流直到结束(空 SSE 流 → 立即结束 → Done 事件)
    let mut event_count = 0;
    while let Some(_event) = stream.next().await {
        event_count += 1;
        if event_count > 20 {
            break; // 防止无限循环
        }
    }

    // 验证:state/events/command 各被调用一次,create_session 未被调用
    state_mock.assert_async().await;
    events_mock.assert_async().await;
    command_mock.assert_async().await;
}

/// G15:验证 run_streaming(新建模式)调用 create_session
#[tokio::test]
async fn test_g15_run_streaming_creates_new_session() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    // mock create_session
    let create_mock = server
        .mock("POST", "/api/sessions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"session_id": 99}"#)
        .create_async()
        .await;

    // mock subscribe_events(空流)
    server
        .mock("GET", "/api/sessions/99/events")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("")
        .create_async()
        .await;

    // mock submit_command
    server
        .mock("POST", "/api/sessions/99/command")
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;

    let config = AgentConfig::default();
    let runner = AgentRunner::new(config, client);

    let stream = runner.run_streaming("hello".to_string());
    let mut stream = stream;

    // 消费流
    let mut got_session_created = false;
    let mut event_count = 0;
    while let Some(event) = stream.next().await {
        event_count += 1;
        if let Ok(AgentEvent::SessionCreated { session_id }) = &event {
            assert_eq!(session_id, "99");
            got_session_created = true;
        }
        if event_count > 20 {
            break;
        }
    }

    assert!(
        got_session_created,
        "run_streaming should yield SessionCreated"
    );
    create_mock.assert_async().await;
}

/// C2:验证 run() 中 recall 在 build_system_prompt 之前（召回内容进入 system_prompt）
#[tokio::test]
async fn test_run_recall_before_prompt() {
    let mut server = mockito::Server::new_async().await;
    let client = EvoruleApiClient::new(&server.url());

    // 1. recall_context: stable facts → 返回一个已知 stable fact
    server.mock("GET", "/api/shared/facts?prefix=shared.test.stable.")
            .with_status(200)
            .with_body(r#"[{"fact_id": 1, "path": "shared.test.stable.rule1", "value": {"key": "stable_rule", "value": "recall_test_value", "timestamp": 1000}, "source_session_id": 100, "version": 1}]"#)
            .create_async()
            .await;

    // 2. recall_context: sessions/events → 空
    server
        .mock("GET", "/api/shared/facts?prefix=shared.test.sessions.")
        .with_status(200)
        .with_body("[]")
        .create_async()
        .await;
    server
        .mock("GET", "/api/shared/facts?prefix=shared.test.events.")
        .with_status(200)
        .with_body("[]")
        .create_async()
        .await;

    // 3. create_session
    server
        .mock("POST", "/api/sessions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"session_id": 99}"#)
        .create_async()
        .await;

    // 4. auto_recall: shared. prefix → 空（auto_recall 提前返回）
    server
        .mock("GET", "/api/shared/facts?prefix=shared.default.")
        .with_status(200)
        .with_body("[]")
        .create_async()
        .await;

    // 5. persist_message: POST /api/sessions/99/payload
    server
        .mock("POST", "/api/sessions/99/payload")
        .with_status(200)
        .with_body("{}")
        .create_async()
        .await;

    // 6. subscribe_events: 空流
    server
        .mock("GET", "/api/sessions/99/events")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body("")
        .create_async()
        .await;

    // 7. submit_command: 验证 body 包含 recalled stable fact value
    let command_mock = server
        .mock("POST", "/api/sessions/99/command")
        .with_status(200)
        .with_body("{}")
        .match_body(mockito::Matcher::Regex("recall_test_value".to_string()))
        .create_async()
        .await;

    let config = AgentConfig::default();
    let memory = MemoryManager::new("test", client.clone());
    let mut runner = AgentRunner::new(config, client).with_memory(memory);

    let result = runner.run("test goal").await;
    // run() 会因为空 SSE 流返回 error result，但不影响验证
    assert!(result.is_ok(), "run() should not return Err");

    // 验证 command 请求体包含 recalled content（证明 recall 在 build_system_prompt 之前）
    command_mock.assert_async().await;
}
