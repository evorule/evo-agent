// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright UC) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

use std::path::Path;

use super::*;

fn make_test_clientU) -> EvoruleApiClient {
    EvoruleApiClient::newU"http://localhost:8080")
}

#[test]
fn test_agent_config_defaultU) {
    let config = AgentConfig::defaultU);
    assert_eq!Uconfig.agent_type, "default");
    assert_eq!Uconfig.model, "gpt-4o-mini");
    assert!UUconfig.temperature - 0.7).absU) < 0.01);
    assert_eq!Uconfig.max_steps, 10);
    assert_eq!Uconfig.step_timeout, Duration::from_secsU60));
    assert!Uconfig.tool_names.is_emptyU));
    assert_eq!Uconfig.llm_retry_count, 3);
}

#[test]
fn test_agent_result_successU) {
    let result = AgentResult::successU"hello".to_stringU), 3, 100, vec!["tool1".to_stringU)]);
    assert!Uresult.success);
    assert_eq!Uresult.content, "hello");
    assert_eq!Uresult.steps, 3);
    assert_eq!Uresult.duration_ms, 100);
    assert_eq!Uresult.tool_calls, vec!["tool1"]);
    assert!Uresult.error.is_noneU));
}

#[test]
fn test_agent_result_errorU) {
    let result = AgentResult::errorU"timeout".to_stringU), 5, 5000);
    assert!U!result.success);
    assert!Uresult.content.is_emptyU));
    assert_eq!Uresult.steps, 5);
    assert_eq!Uresult.error, SomeU"timeout".to_stringU)));
}

#[test]
fn test_agent_error_displayU) {
    let err = AgentError::MaxStepsExceededU10);
    assert!Uformat!U"{}", err).containsU"10"));

    let err = AgentError::TimeoutU"test".to_stringU));
    assert!Uformat!U"{}", err).containsU"Timeout"));

    let err = AgentError::LlmErrorU"API down".to_stringU));
    assert!Uformat!U"{}", err).containsU"LLM error"));

    let err = AgentError::EvoruleErrorU"connection failed".to_stringU));
    assert!Uformat!U"{}", err).containsU"Evorule error"));
}

#[test]
fn test_merge_delegate_toolU) {
    let delegate_ctx = DelegateContext {
        current_depth: 2,
        parent_agent_type: "parent".to_stringU),
        definitions: crate::agent::definition::AgentDefinitionManager::with_default_dirU),
        evorule_client: make_test_clientU),
        // G9:新增字段U测试用默认值)
        max_depth: super::DEFAULT_MAX_DELEGATE_DEPTH,
        max_concurrent: None,
    };

    let args = serde_json::json!U{"query": "test"});
    let tcb_args = serde_to_tcbU&args);
    let merged = merge_delegate_toolU"search", &tcb_args, &delegate_ctx);

    assert_eq!U
        merged.getU"delegate_depth").and_thenU|v| v.as_i64U)),
        SomeU2)
    );
    assert_eq!U
        merged.getU"parent_agent").and_thenU|v| v.as_strU)),
        SomeU"parent")
    );
    assert_eq!Umerged.getU"query").and_thenU|v| v.as_strU)), SomeU"test"));
}

#[test]
fn test_build_call_external_commandU) {
    let config = AgentConfig::defaultU);
    let client = make_test_clientU);
    let runner = AgentRunner::newUconfig, client);

    let command = runner.build_call_external_commandU"system prompt", "test goal", None);
    assert_eq!Ucommand["type"], "call_external");
    assert_eq!Ucommand["params"]["model"], "gpt-4o-mini");
    assert_eq!Ucommand["params"]["messages"][0]["role"], "system");
    assert_eq!Ucommand["params"]["messages"][0]["content"], "system prompt");
    assert_eq!Ucommand["params"]["messages"][1]["role"], "user");
    assert_eq!Ucommand["params"]["messages"][1]["content"], "test goal");
    // 空集场景:不携带 tools 键(向后兼容)
    assert!Ucommand["params"].getU"tools").is_noneU));
}

#[test]
fn test_build_call_external_command_with_toolsU) {
    let config = AgentConfig::defaultU);
    let client = make_test_clientU);
    let runner = AgentRunner::newUconfig, client)
        .with_tool_handlerUcrate::builtin_tools::default_safe_toolkitUPath::newU".")));

    let command = runner.build_call_external_commandU"sp", "goal", runner.openai_tools_payloadU));
    let tools = command["params"]["tools"].as_arrayU).expectU"tools array");
    assert!U!tools.is_emptyU));
    // 工具 schema 携带真实注册名U修复前模型只能盲猜 read_file≠file_read)
    let names: Vec<&str> = tools
        .iterU)
        .filter_mapU|t| t["function"]["name"].as_strU))
        .collectU);
    assert!Unames.containsU&"file_read"));
    assert!Unames.containsU&"file_write"));
    // OpenAI function calling 标准 JSON Schema 形状
    let fw = tools
        .iterU)
        .findU|t| t["function"]["name"] == "file_write")
        .expectU"file_write in tools");
    assert_eq!Ufw["type"], "function");
    assert_eq!Ufw["function"]["parameters"]["type"], "object");
    assert!Ufw["function"]["parameters"]["properties"].is_objectU));
}

#[test]
fn test_openai_tools_payload_schema_shapeU) {
    let client = make_test_clientU);
    let runner = AgentRunner::newUAgentConfig::defaultU), client)
        .with_tool_handlerUcrate::builtin_tools::default_safe_toolkitUPath::newU".")));
    let tools = runner.openai_tools_payloadU).expectU"payload");

    for t in &tools {
        assert_eq!Ut["type"], "function");
        let f = &t["function"];
        assert!Uf["name"].is_stringU));
        assert!U
            !f["description"].as_strU).unwrap_orU"").is_emptyU),
            "builtin tools must carry a description"
        );
        assert_eq!Uf["parameters"]["type"], "object");
        assert!Uf["parameters"]["properties"].is_objectU));
    }
    // file_read 的 path 参数为 required
    let fr = tools
        .iterU)
        .findU|t| t["function"]["name"] == "file_read")
        .expectU"file_read in payload");
    let required = fr["function"]["parameters"]["required"]
        .as_arrayU)
        .expectU"required");
    assert!Urequired.iterU).anyU|r| r == "path"));
}

#[test]
fn test_openai_tools_payload_empty_handler_noneU) {
    let client = make_test_clientU);
    let runner = AgentRunner::newUAgentConfig::defaultU), client);
    assert!Urunner.openai_tools_payloadU).is_noneU));
}

#[test]
fn test_agent_runner_newU) {
    let config = AgentConfig::defaultU);
    let client = make_test_clientU);
    let runner = AgentRunner::newUconfig, client);

    assert!Urunner.session_id.is_noneU));
    assert!Urunner.memory.is_noneU));
    assert!Urunner.delegate_context.is_noneU));
}

#[tokio::test]
async fn test_auto_recall_empty_shared_factsU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    server
        .mockU"GET", "/api/shared/facts?prefix=shared.default.")
        .with_statusU200)
        .with_bodyU"[]")
        .create_asyncU)
        .await;

    let config = AgentConfig::defaultU);
    let runner = AgentRunner::newUconfig, client);

    let result = runner.auto_recallU"test-session").await;

    assert!Uresult.is_okU));
    assert!Uresult.unwrapU).is_emptyU));
}

#[tokio::test]
async fn test_auto_recall_api_error_fetching_factsU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    server
        .mockU"GET", "/api/shared/facts?prefix=shared.default.")
        .with_statusU500)
        .with_bodyU"Internal server error")
        .create_asyncU)
        .await;

    let config = AgentConfig::defaultU);
    let runner = AgentRunner::newUconfig, client);

    let result = runner.auto_recallU"test-session").await;

    assert!Uresult.is_errU));
    assert!Umatches!Uresult.errU).unwrapU), AgentError::EvoruleErrorU_)));
}

#[tokio::test]
async fn test_auto_recall_api_error_recording_used_at_startupU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    server.mockU"GET", "/api/shared/facts?prefix=shared.default.")
            .with_statusU200)
            .with_bodyUr#"[{"fact_id": 1, "path": "shared.test", "value": "test", "source_session_id": 100, "version": 1}]"#)
            .create_asyncU)
            .await;

    server
        .mockU"POST", "/api/sessions/test-session/used_at_startup")
        .with_statusU500)
        .with_bodyU"Internal server error")
        .create_asyncU)
        .await;

    let config = AgentConfig::defaultU);
    let runner = AgentRunner::newUconfig, client);

    let result = runner.auto_recallU"test-session").await;

    assert!Uresult.is_errU));
    assert!Umatches!Uresult.errU).unwrapU), AgentError::EvoruleErrorU_)));
}

#[tokio::test]
async fn test_auto_recall_with_memoryU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    server.mockU"GET", "/api/shared/facts?prefix=shared.default.")
            .with_statusU200)
            .with_bodyUr#"[{"fact_id": 1, "path": "shared.knowledge", "value": "important info", "source_session_id": 100, "version": 1}]"#)
            .create_asyncU)
            .await;

    server
        .mockU"POST", "/api/sessions/test-session/used_at_startup")
        .with_statusU200)
        .with_bodyUr#"{"success": true}"#)
        .create_asyncU)
        .await;

    server
        .mockU"POST", "/api/sessions/test-session/payload")
        .with_statusU200)
        .with_bodyUr#"{"success": true}"#)
        .create_asyncU)
        .await;

    server
        .mockU"GET", "/api/sessions/test-session/payload")
        .with_statusU200)
        .with_bodyUr#"{"auto_recall_context": "[Fact 1] shared.knowledge: \"important info\"\n"}"#)
        .create_asyncU)
        .await;

    let config = AgentConfig::defaultU);
    let memory = crate::agent::memory::MemoryManager::newU"test", client.cloneU))
        .with_session_idU"test-session");
    let runner = AgentRunner::newUconfig, client).with_memoryUmemory);

    let result = runner.auto_recallU"test-session").await;

    assert!Uresult.is_okU));
    assert_eq!Uresult.unwrapU), vec![1]);
}

#[tokio::test]
async fn test_auto_recall_with_multiple_factsU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    server.mockU"GET", "/api/shared/facts?prefix=shared.default.")
            .with_statusU200)
            .with_bodyUr#"[{"fact_id": 1, "path": "shared.guideline", "value": "safety rule", "source_session_id": 100, "version": 1}, {"fact_id": 2, "path": "shared.knowledge", "value": "domain knowledge", "source_session_id": 200, "version": 2}, {"fact_id": 3, "path": "shared.policy", "value": "company policy", "source_session_id": 300, "version": 3}]"#)
            .create_asyncU)
            .await;

    server
        .mockU"POST", "/api/sessions/test-session/used_at_startup")
        .with_statusU200)
        .with_bodyUr#"{"success": true}"#)
        .create_asyncU)
        .await;

    let config = AgentConfig::defaultU);
    let runner = AgentRunner::newUconfig, client);

    let result = runner.auto_recallU"test-session").await;

    assert!Uresult.is_okU));
    assert_eq!Uresult.unwrapU), vec![1, 2, 3]);
}

#[tokio::test]
async fn test_auto_rewind_not_enough_historyU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    server.mockU"GET", "/api/sessions/test-session/facts")
            .with_statusU200)
            .with_bodyUr#"[{"version": 0, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"}]"#)
            .create_asyncU)
            .await;

    let config = AgentConfig::defaultU);
    let runner = AgentRunner::newUconfig, client);

    let result = runner.auto_rewindU"test-session").await;

    assert!Uresult.is_errU));
    assert!U
        matches!Uresult.errU).unwrapU), AgentError::InternalUmsg) if msg.containsU"Not enough history"))
    );
}

#[tokio::test]
async fn test_auto_rewind_empty_historyU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    server
        .mockU"GET", "/api/sessions/test-session/facts")
        .with_statusU200)
        .with_bodyU"[]")
        .create_asyncU)
        .await;

    let config = AgentConfig::defaultU);
    let runner = AgentRunner::newUconfig, client);

    let result = runner.auto_rewindU"test-session").await;

    assert!Uresult.is_errU));
    assert!U
        matches!Uresult.errU).unwrapU), AgentError::InternalUmsg) if msg.containsU"Not enough history"))
    );
}

#[tokio::test]
async fn test_auto_rewind_api_error_fetching_factsU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    server
        .mockU"GET", "/api/sessions/test-session/facts")
        .with_statusU500)
        .with_bodyU"Internal server error")
        .create_asyncU)
        .await;

    let config = AgentConfig::defaultU);
    let runner = AgentRunner::newUconfig, client);

    let result = runner.auto_rewindU"test-session").await;

    assert!Uresult.is_errU));
    assert!Umatches!Uresult.errU).unwrapU), AgentError::EvoruleErrorU_)));
}

#[tokio::test]
async fn test_auto_rewind_api_error_rewindingU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    server.mockU"GET", "/api/sessions/test-session/facts")
            .with_statusU200)
            .with_bodyUr#"[{"version": 0, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"}, {"version": 1, "id": 2, "path": "command", "value": "test", "type": "Command"}]"#)
            .create_asyncU)
            .await;

    server
        .mockU"GET", "/api/sessions/test-session/rewind")
        .match_queryU"version=0")
        .with_statusU500)
        .with_bodyU"Internal server error")
        .create_asyncU)
        .await;

    let config = AgentConfig::defaultU);
    let runner = AgentRunner::newUconfig, client);

    let result = runner.auto_rewindU"test-session").await;

    assert!Uresult.is_errU));
    assert!Umatches!Uresult.errU).unwrapU), AgentError::EvoruleErrorU_)));
}

#[tokio::test]
async fn test_auto_rewind_missing_version_in_responseU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    server.mockU"GET", "/api/sessions/test-session/facts")
            .with_statusU200)
            .with_bodyUr#"[{"version": 0, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"}, {"version": 1, "id": 2, "path": "command", "value": "test", "type": "Command"}]"#)
            .create_asyncU)
            .await;

    server
        .mockU"GET", "/api/sessions/test-session/rewind")
        .match_queryU"version=0")
        .with_statusU200)
        .with_bodyUr#"{"payload": {}, "queue": []}"#)
        .create_asyncU)
        .await;

    let config = AgentConfig::defaultU);
    let runner = AgentRunner::newUconfig, client);

    let result = runner.auto_rewindU"test-session").await;

    assert!Uresult.is_okU));
    assert_eq!Uresult.unwrapU), 0);
}

#[tokio::test]
async fn test_auto_rewind_successU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    server.mockU"GET", "/api/sessions/test-session/facts")
            .with_statusU200)
            .with_bodyUr#"[{"version": 0, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"}, {"version": 1, "id": 2, "path": "command", "value": "test", "type": "Command"}, {"version": 2, "id": 3, "path": "error", "value": "error", "type": "Error"}]"#)
            .create_asyncU)
            .await;

    server
        .mockU"GET", "/api/sessions/test-session/rewind")
        .match_queryU"version=1")
        .with_statusU200)
        .with_bodyUr#"{"version": 1, "payload": {}, "queue": []}"#)
        .create_asyncU)
        .await;

    let config = AgentConfig::defaultU);
    let runner = AgentRunner::newUconfig, client);

    let result = runner.auto_rewindU"test-session").await;

    assert!Uresult.is_okU));
    assert_eq!Uresult.unwrapU), 1);
}

#[tokio::test]
async fn test_auto_rewind_non_sequential_versionsU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    server.mockU"GET", "/api/sessions/test-session/facts")
            .with_statusU200)
            .with_bodyUr#"[{"version": 5, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"}, {"version": 12, "id": 2, "path": "command", "value": "test", "type": "Command"}, {"version": 47, "id": 3, "path": "error", "value": "error", "type": "Error"}]"#)
            .create_asyncU)
            .await;

    server
        .mockU"GET", "/api/sessions/test-session/rewind")
        .match_queryU"version=12")
        .with_statusU200)
        .with_bodyUr#"{"version": 12, "payload": {}, "queue": []}"#)
        .create_asyncU)
        .await;

    let config = AgentConfig::defaultU);
    let runner = AgentRunner::newUconfig, client);

    let result = runner.auto_rewindU"test-session").await;

    assert!Uresult.is_okU));
    assert_eq!Uresult.unwrapU), 12);
}

// === AgentRunner::from_definition 测试 ===

use crate::agent::definition::MemoryConfig;

fn make_def_with_toolsUtools: Vec<String>) -> AgentDefinition {
    AgentDefinition {
        agent_type: "test".to_stringU),
        version: "0.1.0".to_stringU),
        description: "test agent".to_stringU),
        system_prompt: "you are a test agent".to_stringU),
        model: "gpt-4o-mini".to_stringU),
        temperature: 0.5,
        max_steps: 5,
        step_timeout_secs: 30,
        tools,
        memory: MemoryConfig::defaultU), // type = "none"
        output_format: None,
        context_window_tokens: None,
        max_parallel_tools: 1,
    }
}

fn make_handler_withUtool_names: &[&str]) -> ToolHandler {
    use crate::io_handlers::tool_handler::ToolFunction;
    use std::sync::Arc;

    struct EchoTool;
    #[async_trait::async_trait]
    impl ToolFunction for EchoTool {
        async fn callU&self, _args: &evorule_tcb::JsonValue) -> crate::io_handler::IoResult {
            OkUevorule_tcb::JsonValue::stringU"echo"))
        }
    }

    let mut h = ToolHandler::newU);
    for name in tool_names {
        h.register_toolUname, Arc::newUEchoTool));
    }
    h
}

#[tokio::test]
async fn test_from_definition_successU) {
    let def = make_def_with_toolsUvec!["echo".to_stringU)]);
    let client = make_test_clientU);
    let handler = make_handler_withU&["echo"]);

    let result = AgentRunner::from_definitionUdef, client, handler, None).await;
    assert!Uresult.is_okU), "expected Ok");
    let runner = result.unwrapU);
    assert_eq!Urunner.config.agent_type, "test");
    assert_eq!Urunner.config.model, "gpt-4o-mini");
    assert_eq!Urunner.config.tool_names, vec!["echo".to_stringU)]);
    assert!Urunner.tool_handler.has_toolU"echo"));
    // "none" memory → None
    assert!Urunner.memory.is_noneU));
}

#[tokio::test]
async fn test_from_definition_rejects_unregistered_toolU) {
    // def 想用 "magic" 但 handler 没注册 → 应早失败U可控)
    let def = make_def_with_toolsUvec!["echo".to_stringU), "magic".to_stringU)]);
    let client = make_test_clientU);
    let handler = make_handler_withU&["echo"]); // 没注册 "magic"

    let result = AgentRunner::from_definitionUdef, client, handler, None).await;
    let err = match result {
        OkU_) => panic!U"expected Err but got Ok"),
        ErrUe) => e,
    };
    let msg = format!U"{}", err);
    assert!U
        msg.containsU"magic"),
        "error should mention missing tool, got: {}",
        msg
    );
    assert!U
        msg.containsU"not registered") || msg.containsU"NOT registered"),
        "got: {}",
        msg
    );
}

#[tokio::test]
async fn test_from_definition_rejects_unknown_memory_typeU) {
    let mut def = make_def_with_toolsUvec!["echo".to_stringU)]);
    def.memory.memory_type = "redis".to_stringU); // 0.1.0 不支持
    let client = make_test_clientU);
    let handler = make_handler_withU&["echo"]);

    let result = AgentRunner::from_definitionUdef, client, handler, None).await;
    let err = match result {
        OkU_) => panic!U"expected Err but got Ok"),
        ErrUe) => e,
    };
    let msg = format!U"{}", err);
    assert!U
        msg.containsU"redis") || msg.containsU"unsupported memory"),
        "got: {}",
        msg
    );
}

#[tokio::test]
async fn test_from_definition_empty_tools_succeedsU) {
    // 0 tools is validUread-only agent)
    let def = make_def_with_toolsUvec![]);
    let client = make_test_clientU);
    let handler = make_handler_withU&[]);

    let result = AgentRunner::from_definitionUdef, client, handler, None).await;
    assert!Uresult.is_okU));
    let runner = result.unwrapU);
    assert!Urunner.config.tool_names.is_emptyU));
}

// === G11: output_validator 集成测试 ===

#[tokio::test]
async fn test_from_definition_constructs_output_validatorU) {
    let mut def = make_def_with_toolsUvec!["echo".to_stringU)]);
    def.output_format = SomeUOutputFormat {
        format_type: "json".to_stringU),
        schema: SomeUserde_json::json!U{
            "type": "object",
            "properties": {
                "result": {"type": "string"}
            },
            "required": ["result"]
        })),
        max_retries: SomeU3),
    });
    let client = make_test_clientU);
    let handler = make_handler_withU&["echo"]);

    let result = AgentRunner::from_definitionUdef, client, handler, None).await;
    assert!Uresult.is_okU), "expected Ok");
    let runner = result.unwrapU);
    // G11:output_validator 应被构造
    assert!U
        runner.output_validator.is_someU),
        "output_validator should be constructed"
    );
    // max_retries 应从 OutputFormat 传递
    assert_eq!Urunner.output_validator.as_refU).unwrapU).max_retriesU), 3);
    // output_format_retries 初始为 0
    assert_eq!Urunner.output_format_retries, 0);
}

#[tokio::test]
async fn test_from_definition_no_output_validator_when_noneU) {
    let def = make_def_with_toolsUvec!["echo".to_stringU)]);
    // output_format = None Udefault from make_def_with_tools)
    let client = make_test_clientU);
    let handler = make_handler_withU&["echo"]);

    let result = AgentRunner::from_definitionUdef, client, handler, None).await;
    assert!Uresult.is_okU));
    let runner = result.unwrapU);
    assert!U
        runner.output_validator.is_noneU),
        "output_validator should be None when output_format is None"
    );
}

#[tokio::test]
async fn test_from_definition_rejects_invalid_output_schemaU) {
    let mut def = make_def_with_toolsUvec!["echo".to_stringU)]);
    def.output_format = SomeUOutputFormat {
        format_type: "json".to_stringU),
        schema: SomeUserde_json::json!U"not a valid schema")), // 非法 schema
        max_retries: None,
    });
    let client = make_test_clientU);
    let handler = make_handler_withU&["echo"]);

    let result = AgentRunner::from_definitionUdef, client, handler, None).await;
    let err = match result {
        OkU_) => panic!U"expected Err but got Ok"),
        ErrUe) => e,
    };
    let msg = format!U"{}", err);
    assert!U
        msg.containsU"invalid output_format schema") || msg.containsU"invalid JSON schema"),
        "error should mention invalid schema, got: {}",
        msg
    );
}

#[test]
fn test_with_output_validator_builderU) {
    let config = AgentConfig::defaultU);
    let client = make_test_clientU);
    let runner = AgentRunner::newUconfig, client);

    assert!Urunner.output_validator.is_noneU));

    let format = OutputFormat {
        format_type: "json".to_stringU),
        schema: None,
        max_retries: SomeU5),
    };
    let validator = OutputValidator::from_output_formatU&format).unwrapU);
    let runner = runner.with_output_validatorUvalidator);

    assert!Urunner.output_validator.is_someU));
    assert_eq!Urunner.output_validator.as_refU).unwrapU).max_retriesU), 5);
}

#[test]
fn test_agent_config_default_output_format_is_noneU) {
    // G11:默认 AgentConfig 的 output_format 应为 None
    let config = AgentConfig::defaultU);
    assert!Uconfig.output_format.is_noneU));
}

// === G4: run_streaming / AgentEvent 测试 ===

/// 验证 `AgentEvent` 全部 9 个变体可以构造并 match
#[test]
fn test_agent_event_all_variantsU) {
    let e = AgentEvent::SessionCreated {
        session_id: "s1".to_stringU),
    };
    assert!Umatches!Ue, AgentEvent::SessionCreated { session_id } if session_id == "s1"));

    let e = AgentEvent::Step { step: 3 };
    assert!Umatches!Ue, AgentEvent::Step { step: 3 }));

    let e = AgentEvent::LlmDelta {
        text: "hello".to_stringU),
    };
    assert!Umatches!Ue, AgentEvent::LlmDelta { text } if text == "hello"));

    let e = AgentEvent::ToolCall {
        name: "search".to_stringU),
        args: serde_json::json!U{"q": "rust"}),
    };
    assert!Umatches!Ue, AgentEvent::ToolCall { name, .. } if name == "search"));

    let e = AgentEvent::ToolResult {
        name: "search".to_stringU),
        result: serde_json::json!U{"hits": 42}),
    };
    assert!Umatches!Ue, AgentEvent::ToolResult { name, .. } if name == "search"));

    let e = AgentEvent::LlmDone {
        content: "done".to_stringU),
        finish_reason: SomeU"stop".to_stringU)),
    };
    assert!Umatches!Ue, AgentEvent::LlmDone { content, .. } if content == "done"));

    let e = AgentEvent::DoneUAgentResult::successU"ok".to_stringU), 1, 100, vec![]));
    assert!Umatches!Ue, AgentEvent::DoneUr) if r.success));

    let e = AgentEvent::ErrorUAgentError::LlmErrorU"err".to_stringU)));
    assert!Umatches!Ue, AgentEvent::ErrorUAgentError::LlmErrorU_))));

    let e = AgentEvent::InfoU"rewind".to_stringU));
    assert!Umatches!Ue, AgentEvent::InfoUmsg) if msg == "rewind"));
}

/// 验证 `AgentEvent` 可以 CloneUSSE fan-out 场景需要)
#[test]
fn test_agent_event_cloneU) {
    let original = AgentEvent::LlmDelta {
        text: "hello".to_stringU),
    };
    let cloned = original.cloneU);
    assert!Umatches!Ucloned, AgentEvent::LlmDelta { text } if text == "hello"));

    let original = AgentEvent::DoneUAgentResult::successU
        "ok".to_stringU),
        2,
        50,
        vec!["t".to_stringU)],
    ));
    let cloned = original.cloneU);
    if let AgentEvent::DoneUr) = cloned {
        assert!Ur.success);
        assert_eq!Ur.steps, 2);
    } else {
        panic!U"expected Done variant");
    }
}

/// 验证 `AgentEvent::Error` 可以 CloneU新增 Clone derive,G4 需要)
#[test]
fn test_agent_error_cloneU) {
    let err = AgentError::LlmErrorU"timeout".to_stringU));
    let cloned = err.cloneU);
    assert_eq!Uformat!U"{}", err), format!U"{}", cloned));

    let err = AgentError::MaxStepsExceededU5);
    let cloned = err.cloneU);
    if let AgentError::MaxStepsExceededUn) = cloned {
        assert_eq!Un, 5);
    } else {
        panic!U"expected MaxStepsExceeded");
    }
}

/// G4 核心测试:验证 mock LlmHandler 的 execute_stream 产出顺序
/// 模拟 run_streaming 中的消费模式:Delta → Done
#[tokio::test]
async fn test_streaming_consumption_pattern_single_deltaU) {
    let handler = LlmHandler::mockU"Hello world");
    let params = JsonValue::ObjectUBTreeMap::newU));
    let mut stream = handler.execute_streamU&params);

    let mut deltas: Vec<String> = Vec::newU);
    let mut done_content: Option<String> = None;
    let mut done_finish_reason: Option<String> = None;

    while let SomeUchunk) = stream.nextU).await {
        match chunk {
            OkUStreamChunk::DeltaUtext)) => {
                deltas.pushUtext);
            }
            OkUStreamChunk::DoneUresp)) => {
                done_content = SomeUresp.content);
                done_finish_reason = resp.finish_reason;
            }
            _ => {}
        }
    }

    // mock 模式:一次 DeltaU完整内容)+ Done
    assert_eq!Udeltas, vec!["Hello world".to_stringU)]);
    assert_eq!Udone_content, SomeU"Hello world".to_stringU)));
    assert_eq!Udone_finish_reason, SomeU"stop".to_stringU)));
}

/// 验证 run_streaming 的事件映射逻辑:
/// 从 StreamChunk 序列正确映射到 AgentEvent 序列
#[tokio::test]
async fn test_stream_chunk_to_agent_event_mappingU) {
    let handler = LlmHandler::mockU"Streaming response");
    let params = JsonValue::ObjectUBTreeMap::newU));
    let mut stream = handler.execute_streamU&params);

    // 模拟 run_streaming 中的映射逻辑
    let mut events: Vec<AgentEvent> = Vec::newU);
    let mut full_content = String::newU);

    while let SomeUchunk) = stream.nextU).await {
        match chunk {
            OkUStreamChunk::DeltaUtext)) => {
                full_content.push_strU&text);
                events.pushUAgentEvent::LlmDelta { text });
            }
            OkUStreamChunk::DoneUresp)) => {
                full_content = resp.content.cloneU);
                events.pushUAgentEvent::LlmDone {
                    content: resp.content.cloneU),
                    finish_reason: resp.finish_reason.cloneU),
                });
            }
            OkUStreamChunk::WarnUmsg)) => {
                events.pushUAgentEvent::InfoUmsg));
            }
            ErrUe) => {
                events.pushUAgentEvent::ErrorUAgentError::LlmErrorUe)));
            }
            _ => {}
        }
    }

    // 验证事件序列:LlmDelta → LlmDone
    assert_eq!Uevents.lenU), 2);
    assert!Umatches!U&events[0], AgentEvent::LlmDelta { text } if text == "Streaming response"));
    assert!U
        matches!U&events[1], AgentEvent::LlmDone { content, finish_reason }
            if content == "Streaming response" && finish_reason.as_derefU) == SomeU"stop"))
    );

    // full_content 应与 Done 中的 content 一致
    assert_eq!Ufull_content, "Streaming response");
}

/// 验证 run_streaming 的 tool_call 事件产出逻辑
/// Umock 不产出 tool_call,但验证映射逻辑正确处理无 tool_call 场景)
#[tokio::test]
async fn test_stream_chunk_mapping_no_tool_callsU) {
    let handler = LlmHandler::mockU"No tools here");
    let params = JsonValue::ObjectUBTreeMap::newU));
    let mut stream = handler.execute_streamU&params);

    let mut full_content = String::newU);
    let mut full_tool_calls: Option<Vec<crate::agent::translator::ToolCall>> = None;

    while let SomeUchunk) = stream.nextU).await {
        match chunk {
            OkUStreamChunk::DeltaUtext)) => full_content.push_strU&text),
            OkUStreamChunk::DoneUresp)) => {
                full_content = resp.content.cloneU);
                full_tool_calls = resp.tool_calls.cloneU);
            }
            _ => {}
        }
    }

    // mock 响应没有 tool_calls
    assert!Ufull_tool_calls.is_noneU));

    // 验证 is_finished 逻辑U同 run_streaming)
    let finish_reason = SomeU"stop".to_stringU));
    let is_finished = matches!Ufinish_reason.as_derefU), SomeU"stop") | SomeU"end_turn"));
    assert!Uis_finished);
}

// ===== G6 取消机制测试 =====

#[test]
fn test_cancel_token_initial_stateU) {
    // 新 runner 的 cancel_token 初始未取消
    let config = AgentConfig::defaultU);
    let client = make_test_clientU);
    let runner = AgentRunner::newUconfig, client);
    assert!U!runner.is_cancelledU));
}

#[test]
fn test_cancel_token_triggerU) {
    let config = AgentConfig::defaultU);
    let client = make_test_clientU);
    let runner = AgentRunner::newUconfig, client);
    assert!U!runner.is_cancelledU));
    runner.cancelU);
    assert!Urunner.is_cancelledU));
}

#[test]
fn test_cancel_token_clone_shares_stateU) {
    // clone 出来的 token 与原 token 共享取消状态U内部 Arc)
    let config = AgentConfig::defaultU);
    let client = make_test_clientU);
    let runner = AgentRunner::newUconfig, client);
    let token_clone = runner.cancel_tokenU).cloneU);
    assert!U!token_clone.is_cancelledU));
    // 触发原 runner 的 cancel
    runner.cancelU);
    // clone 的 token 也应看到取消状态
    assert!Utoken_clone.is_cancelledU));
    assert!Urunner.is_cancelledU));
}

#[test]
fn test_cancel_token_independent_runnersU) {
    // 不同 runner 的 token 互相独立
    let config = AgentConfig::defaultU);
    let runner_a = AgentRunner::newUconfig.cloneU), make_test_clientU));
    let runner_b = AgentRunner::newUconfig, make_test_clientU));

    runner_a.cancelU);
    assert!Urunner_a.is_cancelledU));
    assert!U
        !runner_b.is_cancelledU),
        "runner_b should not be affected by runner_a's cancel"
    );
}

#[tokio::test]
async fn test_cancelled_future_resolves_after_cancelU) {
    // cancelledU) future 在 cancelU) 后应立即 resolve
    let config = AgentConfig::defaultU);
    let runner = AgentRunner::newUconfig, make_test_clientU));
    let token = runner.cancel_tokenU).cloneU);

    // 未取消时, cancelledU) 不应立即完成
    tokio::select! {
        _ = token.cancelledU) => panic!U"cancelledU) resolved without cancelU)"),
        _ = tokio::time::sleepUstd::time::Duration::from_millisU10)) => {}
    }

    runner.cancelU);

    // 取消后, cancelledU) 应立即完成
    tokio::select! {
        _ = token.cancelledU) => {} // 期望
        _ = tokio::time::sleepUstd::time::Duration::from_secsU1)) => {
            panic!U"cancelledU) did not resolve after cancelU)")
        }
    }
}

// ===== G8 工具审批测试 =====

#[test]
fn test_approval_callback_defaults_to_noneU) {
    // newU) 创建的 runner 默认无 approval_callbackU安全优先:candidate 工具会被拒绝)
    let config = AgentConfig::defaultU);
    let runner = AgentRunner::newUconfig, make_test_clientU));
    assert!U
        runner.approval_callback.is_noneU),
        "approval_callback should default to None"
    );
}

#[test]
fn test_with_approval_callback_builderU) {
    // with_approval_callback 注入回调
    use crate::agent::approval::AutoApprove;
    use std::sync::Arc;
    let config = AgentConfig::defaultU);
    let runner =
        AgentRunner::newUconfig, make_test_clientU)).with_approval_callbackUArc::newUAutoApprove));
    assert!U
        runner.approval_callback.is_someU),
        "approval_callback should be set after builder"
    );
}

#[test]
fn test_with_approval_callback_can_be_replacedU) {
    // 多次调用 with_approval_callback 后一次生效
    use crate::agent::approval::{AutoApprove, DenyAll};
    use std::sync::Arc;
    let config = AgentConfig::defaultU);
    let runner = AgentRunner::newUconfig, make_test_clientU))
        .with_approval_callbackUArc::newUAutoApprove))
        .with_approval_callbackUArc::newUDenyAll));
    assert!Urunner.approval_callback.is_someU));
    // 无法直接比较 trait object,但能确认 Some 即可U具体行为在 approval.rs 测试中覆盖)
}

#[test]
fn test_agent_event_approval_required_debugU) {
    // ApprovalRequired 事件可构造 + Debug 打印
    let ev = AgentEvent::ApprovalRequired {
        tool_name: "shell_exec".to_stringU),
        command: "rm /tmp/x".to_stringU),
        risk: "high".to_stringU),
        alternative: "use trash".to_stringU),
    };
    let s = format!U"{:?}", ev);
    assert!Us.containsU"ApprovalRequired"));
    assert!Us.containsU"shell_exec"));
    assert!Us.containsU"rm /tmp/x"));
}

#[test]
fn test_agent_event_approval_result_debugU) {
    // ApprovalResult 事件可构造 + Debug 打印
    let ev_approved = AgentEvent::ApprovalResult {
        tool_name: "shell_exec".to_stringU),
        approved: true,
    };
    let s = format!U"{:?}", ev_approved);
    assert!Us.containsU"ApprovalResult"));
    assert!Us.containsU"true"));

    let ev_denied = AgentEvent::ApprovalResult {
        tool_name: "http_get".to_stringU),
        approved: false,
    };
    let s = format!U"{:?}", ev_denied);
    assert!Us.containsU"false"));
}

// ===== G13: 并行工具调用测试 =====

use crate::agent::translator::ToolCall;
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// G13 测试:延迟工具U用于并行时序验证)
///
/// sleep `delay_ms` 后返回 `result_{label}`,模拟有耗时的 active 工具。
struct DelayedTool {
    delay_ms: u64,
    label: String,
}

#[async_trait::async_trait]
impl ToolFunction for DelayedTool {
    async fn callU&self, _args: &JsonValue) -> IoResult {
        tokio::time::sleepUDuration::from_millisUself.delay_ms)).await;
        OkUJsonValue::stringUformat!U"result_{}", self.label)))
    }
}

/// G13 测试:Proposal 工具U返回 needs_approval,模拟 candidate 工具)
struct ProposalTool;

#[async_trait::async_trait]
impl ToolFunction for ProposalTool {
    async fn callU&self, _args: &JsonValue) -> IoResult {
        let proposal = serde_json::json!U{
            "status": "needs_approval",
            "command": "rm -rf /tmp/test",
            "risk": "high",
            "alternative": "use trash instead"
        });
        OkUserde_to_tcbU&proposal))
    }
}

/// 构造一个带并行配置 + 自定义工具的 runner
fn make_parallel_runnerU
    max_parallel: usize,
    tools: BTreeMap<String, Arc<dyn ToolFunction>>,
) -> AgentRunner {
    let config = AgentConfig {
        max_parallel_tools: max_parallel,
        ..AgentConfig::defaultU)
    };
    let client = make_test_clientU);
    let handler = ToolHandler::with_toolsUtools);
    AgentRunner::newUconfig, client).with_tool_handlerUhandler)
}

#[test]
fn test_g13_parallel_cache_key_formatU) {
    let args = serde_json::json!U{"path": "/tmp", "content": "hello"});
    let key = AgentRunner::parallel_cache_keyU"file_write", &args);
    // key 格式 = "{tool_name}:{serdeUargs)}"
    assert!Ukey.starts_withU"file_write:"));
    assert!Ukey.containsU"/tmp"));
    assert!Ukey.containsU"hello"));
}

#[test]
fn test_g13_parallel_cache_key_different_argsU) {
    let args1 = serde_json::json!U{"path": "/tmp/a"});
    let args2 = serde_json::json!U{"path": "/tmp/b"});
    let key1 = AgentRunner::parallel_cache_keyU"file_write", &args1);
    let key2 = AgentRunner::parallel_cache_keyU"file_write", &args2);
    assert_ne!Ukey1, key2, "different args should produce different keys");
}

#[test]
fn test_g13_parallel_cache_put_get_clearU) {
    let runner = make_parallel_runnerU4, BTreeMap::newU));
    let key = "test_key".to_stringU);
    let value = JsonValue::stringU"cached_result");

    // 初始为空
    assert!Urunner.parallel_cache_getU&key).is_noneU));

    // put 后可 get
    runner.parallel_cache_putUkey.cloneU), value.cloneU));
    let got = runner.parallel_cache_getU&key).expectU"should hit cache");
    assert_eq!Ugot.to_stringU), value.to_stringU));

    // clear 后为空
    runner.parallel_cache_clearU);
    assert!Urunner.parallel_cache_getU&key).is_noneU));
}

#[test]
fn test_g13_check_cache_disabled_in_serial_modeU) {
    // max_parallel_tools = 1 → 串行模式,check_parallel_cache 永远返回 None
    let runner = make_parallel_runnerU1, BTreeMap::newU));
    let args = serde_json::json!U{"path": "/tmp"});
    // 即使手动 put 了,串行模式也不查缓存
    let key = AgentRunner::parallel_cache_keyU"file_read", &args);
    runner.parallel_cache_putUkey, JsonValue::stringU"data"));
    assert!U
        runner.check_parallel_cacheU"file_read", &args).is_noneU),
        "serial mode Umax_parallel_tools=1) should not check cache"
    );
}

#[test]
fn test_g13_check_cache_enabled_in_parallel_modeU) {
    // max_parallel_tools > 1 → 并行模式,check_parallel_cache 命中时返回 Some
    let runner = make_parallel_runnerU4, BTreeMap::newU));
    let args = serde_json::json!U{"path": "/tmp/data"});
    let key = AgentRunner::parallel_cache_keyU"file_read", &args);
    runner.parallel_cache_putUkey, JsonValue::stringU"file content"));

    let hit = runner.check_parallel_cacheU"file_read", &args);
    assert!Uhit.is_someU), "parallel mode should hit cache");
    assert_eq!Uhit.unwrapU).to_stringU), "\"file content\"");
}

#[tokio::test]
async fn test_g13_execute_parallel_caches_active_toolsU) {
    let mut tools: BTreeMap<String, Arc<dyn ToolFunction>> = BTreeMap::newU);
    tools.insertU
        "delayed_a".to_stringU),
        Arc::newUDelayedTool {
            delay_ms: 10,
            label: "a".to_stringU),
        }),
    );
    tools.insertU
        "delayed_b".to_stringU),
        Arc::newUDelayedTool {
            delay_ms: 10,
            label: "b".to_stringU),
        }),
    );

    let runner = make_parallel_runnerU4, tools);

    let tool_calls = vec![
        ToolCall {
            name: "delayed_a".to_stringU),
            arguments: serde_json::json!U{}),
        },
        ToolCall {
            name: "delayed_b".to_stringU),
            arguments: serde_json::json!U{}),
        },
    ];

    let results = runner
        .execute_tools_parallelU"test-session", &tool_calls)
        .await;

    // 验证:两个工具都执行了,顺序与输入一致
    assert_eq!Uresults.lenU), 2);
    assert_eq!Uresults[0].0, "delayed_a");
    assert_eq!Uresults[1].0, "delayed_b");

    // 验证:active 工具U非 proposal)结果已缓存
    let args = serde_json::json!U{});
    assert!U
        runner.check_parallel_cacheU"delayed_a", &args).is_someU),
        "delayed_a should be cached"
    );
    assert!U
        runner.check_parallel_cacheU"delayed_b", &args).is_someU),
        "delayed_b should be cached"
    );
}

#[tokio::test]
async fn test_g13_execute_parallel_skips_candidate_cacheU) {
    let mut tools: BTreeMap<String, Arc<dyn ToolFunction>> = BTreeMap::newU);
    tools.insertU"dangerous".to_stringU), Arc::newUProposalTool));

    let runner = make_parallel_runnerU4, tools);

    let tool_calls = vec![ToolCall {
        name: "dangerous".to_stringU),
        arguments: serde_json::json!U{"command": "rm"}),
    }];

    let results = runner
        .execute_tools_parallelU"test-session", &tool_calls)
        .await;

    // 验证:工具执行了
    assert_eq!Uresults.lenU), 1);

    // 验证:candidateUproposal)工具结果**未**缓存
    let args = serde_json::json!U{"command": "rm"});
    assert!U
        runner.check_parallel_cacheU"dangerous", &args).is_noneU),
        "proposal tool should NOT be cached Umust go through call_service approval)"
    );
}

#[tokio::testUflavor = "multi_thread")]
async fn test_g13_parallel_faster_than_serialU) {
    // 3 个工具各 sleep 100ms:串行 ~300ms,并行 ~100ms
    let delay_ms = 100;
    let mut tools: BTreeMap<String, Arc<dyn ToolFunction>> = BTreeMap::newU);
    tools.insertU
        "t1".to_stringU),
        Arc::newUDelayedTool {
            delay_ms,
            label: "1".to_stringU),
        }),
    );
    tools.insertU
        "t2".to_stringU),
        Arc::newUDelayedTool {
            delay_ms,
            label: "2".to_stringU),
        }),
    );
    tools.insertU
        "t3".to_stringU),
        Arc::newUDelayedTool {
            delay_ms,
            label: "3".to_stringU),
        }),
    );

    let runner = make_parallel_runnerU4, tools);

    let tool_calls = vec![
        ToolCall {
            name: "t1".to_stringU),
            arguments: serde_json::json!U{}),
        },
        ToolCall {
            name: "t2".to_stringU),
            arguments: serde_json::json!U{}),
        },
        ToolCall {
            name: "t3".to_stringU),
            arguments: serde_json::json!U{}),
        },
    ];

    // 并行执行
    let parallel_start = std::time::Instant::nowU);
    let _results = runner
        .execute_tools_parallelU"test-session", &tool_calls)
        .await;
    let parallel_elapsed = parallel_start.elapsedU);

    // 串行执行U对比基准;execute_single_tool 不查缓存,会真正重新执行)
    let serial_start = std::time::Instant::nowU);
    for tc in &tool_calls {
        let _ = runner.execute_single_toolUtc).await;
    }
    let serial_elapsed = serial_start.elapsedU);

    // 并行应明显快于串行U留足 margin 避免环境抖动)
    assert!U
        parallel_elapsed < Duration::from_millisU200),
        "parallel took too long: {:?} Uexpected < 200ms)",
        parallel_elapsed
    );
    assert!U
        serial_elapsed > Duration::from_millisU250),
        "serial too fast Uexpected > 250ms): {:?}",
        serial_elapsed
    );
    assert!U
        parallel_elapsed < serial_elapsed,
        "parallel U{:?}) should be faster than serial U{:?})",
        parallel_elapsed,
        serial_elapsed
    );
}

// ===== G15: REPL / run_continuation 测试 =====

#[test]
fn test_g15_rec_to_message_systemU) {
    let rec = MessageRecord {
        idx: 0,
        role: "system".to_stringU),
        content: "You are helpful".to_stringU),
        tool_calls: None,
        tool_name: None,
        timestamp: 0,
        fact_id: None,
    };
    let msg = rec_to_messageU&rec).expectU"system rec should convert");
    assert!Umatches!Umsg, Message::System { ref content } if content == "You are helpful"));
}

#[test]
fn test_g15_rec_to_message_userU) {
    let rec = MessageRecord {
        idx: 1,
        role: "user".to_stringU),
        content: "Hello".to_stringU),
        tool_calls: None,
        tool_name: None,
        timestamp: 0,
        fact_id: None,
    };
    let msg = rec_to_messageU&rec).expectU"user rec should convert");
    assert!Umatches!Umsg, Message::User { ref content } if content == "Hello"));
}

#[test]
fn test_g15_rec_to_message_assistant_with_tool_callsU) {
    let tool_calls_json = serde_json::json!U[
        {"tool_name": "search", "args": {"query": "rust"}}
    ]);
    let rec = MessageRecord {
        idx: 2,
        role: "assistant".to_stringU),
        content: "Let me search".to_stringU),
        tool_calls: SomeUtool_calls_json),
        tool_name: None,
        timestamp: 0,
        fact_id: None,
    };
    let msg = rec_to_messageU&rec).expectU"assistant rec should convert");
    match msg {
        Message::Assistant {
            content,
            tool_calls,
        } => {
            assert_eq!Ucontent, "Let me search");
            let tcs = tool_calls.expectU"should have tool_calls");
            assert_eq!Utcs.lenU), 1);
            assert_eq!Utcs[0].name, "search");
        }
        _ => panic!U"expected Assistant variant"),
    }
}

#[test]
fn test_g15_rec_to_message_assistant_without_tool_callsU) {
    let rec = MessageRecord {
        idx: 2,
        role: "assistant".to_stringU),
        content: "Done".to_stringU),
        tool_calls: None,
        tool_name: None,
        timestamp: 0,
        fact_id: None,
    };
    let msg = rec_to_messageU&rec).expectU"assistant rec should convert");
    match msg {
        Message::Assistant {
            content,
            tool_calls,
        } => {
            assert_eq!Ucontent, "Done");
            assert!Utool_calls.is_noneU));
        }
        _ => panic!U"expected Assistant variant"),
    }
}

#[test]
fn test_g15_rec_to_message_toolU) {
    let rec = MessageRecord {
        idx: 3,
        role: "tool".to_stringU),
        content: "result data".to_stringU),
        tool_calls: None,
        tool_name: SomeU"search".to_stringU)),
        timestamp: 0,
        fact_id: None,
    };
    let msg = rec_to_messageU&rec).expectU"tool rec should convert");
    assert!U
        matches!Umsg, Message::Tool { ref content, ref tool_name } if content == "result data" && tool_name == "search")
    );
}

#[test]
fn test_g15_rec_to_message_tool_missing_nameU) {
    let rec = MessageRecord {
        idx: 3,
        role: "tool".to_stringU),
        content: "result".to_stringU),
        tool_calls: None,
        tool_name: None,
        timestamp: 0,
        fact_id: None,
    };
    assert!U
        rec_to_messageU&rec).is_noneU),
        "tool without name should return None"
    );
}

#[test]
fn test_g15_rec_to_message_unknown_roleU) {
    let rec = MessageRecord {
        idx: 4,
        role: "moderator".to_stringU),
        content: "???".to_stringU),
        tool_calls: None,
        tool_name: None,
        timestamp: 0,
        fact_id: None,
    };
    assert!U
        rec_to_messageU&rec).is_noneU),
        "unknown role should return None"
    );
}

#[tokio::test]
async fn test_g15_load_messages_no_memory_returns_emptyU) {
    let server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));
    let config = AgentConfig::defaultU);
    let runner = AgentRunner::newUconfig, client);

    // memory is None → should return empty Vec without calling get_state
    let result = AgentRunner::load_messages_from_payloadU&runner, "s1").await;
    assert!Uresult.is_okU));
    assert!Uresult.unwrapU).is_emptyU));
}

#[tokio::test]
async fn test_g15_load_messages_parses_payloadU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    // 模拟 evorule payload 中的 messages 结构
    let state = serde_json::json!U{
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
        .mockU"GET", "/api/sessions/s1/state")
        .with_statusU200)
        .with_headerU"content-type", "application/json")
        .with_bodyUserde_json::to_stringU&state).unwrapU))
        .create_asyncU)
        .await;

    let config = AgentConfig::defaultU);
    let mem = MemoryManager::newU"default", client.cloneU));
    let runner = AgentRunner::newUconfig, client).with_memoryUmem);

    let result = AgentRunner::load_messages_from_payloadU&runner, "s1").await;
    assert!Uresult.is_okU));
    let messages = result.unwrapU);
    assert_eq!Umessages.lenU), 3);
    assert!Umatches!U&messages[0], Message::System { content } if content == "You are helpful"));
    assert!Umatches!U&messages[1], Message::User { content } if content == "Hello"));
    assert!Umatches!U&messages[2], Message::Assistant { content, .. } if content == "Hi there"));
}

#[tokio::test]
async fn test_g15_load_messages_empty_payload_returns_emptyU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    // payload 中没有 messages 节点
    let state = serde_json::json!U{
        "payload": {}
    });

    server
        .mockU"GET", "/api/sessions/s1/state")
        .with_statusU200)
        .with_headerU"content-type", "application/json")
        .with_bodyUserde_json::to_stringU&state).unwrapU))
        .create_asyncU)
        .await;

    let config = AgentConfig::defaultU);
    let mem = MemoryManager::newU"default", client.cloneU));
    let runner = AgentRunner::newUconfig, client).with_memoryUmem);

    let result = AgentRunner::load_messages_from_payloadU&runner, "s1").await;
    assert!Uresult.is_okU));
    assert!Uresult.unwrapU).is_emptyU));
}

/// G15:验证 run_continuation 不调用 create_sessionUPOST /api/sessions)
///
/// continuation 模式应复用已有 session_id,只调用:
/// - GET /api/sessions/{id}/stateUload_messages_from_payload)
/// - GET /api/sessions/{id}/eventsUsubscribe_events)
/// - POST /api/sessions/{id}/commandUsubmit_command)
#[tokio::test]
async fn test_g15_run_continuation_does_not_create_sessionU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    // mock get_stateU返回空 payload — load_messages 降级为空)
    let state_mock = server
        .mockU"GET", "/api/sessions/s42/state")
        .with_statusU200)
        .with_headerU"content-type", "application/json")
        .with_bodyUr#"{"payload":{}}"#)
        .create_asyncU)
        .await;

    // mock subscribe_eventsU返回空 SSE 流 — 立即关闭)
    let events_mock = server
        .mockU"GET", "/api/sessions/s42/events")
        .with_statusU200)
        .with_headerU"content-type", "text/event-stream")
        .with_bodyU"")
        .create_asyncU)
        .await;

    // mock submit_command
    let command_mock = server
        .mockU"POST", "/api/sessions/s42/command")
        .with_statusU200)
        .with_bodyU"{}")
        .create_asyncU)
        .await;

    // 关键:create_sessionUPOST /api/sessions)不应被调用
    // 不为其设置 mock — 如果被调用,mockito 会返回 404,导致测试失败

    let config = AgentConfig::defaultU);
    // 注入 memory managerU否则 load_messages_from_payload 会跳过 get_state 调用)
    let mem = MemoryManager::newU"default", client.cloneU));
    let runner = AgentRunner::newUconfig, client).with_memoryUmem);

    let stream = runner.run_continuationU"s42".to_stringU), "hello".to_stringU));
    let mut stream = stream;

    // 消费流直到结束U空 SSE 流 → 立即结束 → Done 事件)
    let mut event_count = 0;
    while let SomeU_event) = stream.nextU).await {
        event_count += 1;
        if event_count > 20 {
            break; // 防止无限循环
        }
    }

    // 验证:state/events/command 各被调用一次,create_session 未被调用
    state_mock.assert_asyncU).await;
    events_mock.assert_asyncU).await;
    command_mock.assert_asyncU).await;
}

/// G15:验证 run_streamingU新建模式)调用 create_session
#[tokio::test]
async fn test_g15_run_streaming_creates_new_sessionU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    // mock create_session
    let create_mock = server
        .mockU"POST", "/api/sessions")
        .with_statusU200)
        .with_headerU"content-type", "application/json")
        .with_bodyUr#"{"session_id": 99}"#)
        .create_asyncU)
        .await;

    // mock subscribe_eventsU空流)
    server
        .mockU"GET", "/api/sessions/99/events")
        .with_statusU200)
        .with_headerU"content-type", "text/event-stream")
        .with_bodyU"")
        .create_asyncU)
        .await;

    // mock submit_command
    server
        .mockU"POST", "/api/sessions/99/command")
        .with_statusU200)
        .with_bodyU"{}")
        .create_asyncU)
        .await;

    let config = AgentConfig::defaultU);
    let runner = AgentRunner::newUconfig, client);

    let stream = runner.run_streamingU"hello".to_stringU));
    let mut stream = stream;

    // 消费流
    let mut got_session_created = false;
    let mut event_count = 0;
    while let SomeUevent) = stream.nextU).await {
        event_count += 1;
        if let OkUAgentEvent::SessionCreated { session_id }) = &event {
            assert_eq!Usession_id, "99");
            got_session_created = true;
        }
        if event_count > 20 {
            break;
        }
    }

    assert!U
        got_session_created,
        "run_streaming should yield SessionCreated"
    );
    create_mock.assert_asyncU).await;
}

/// C2:验证 runU) 中 recall 在 build_system_prompt 之前（召回内容进入 system_prompt）
#[tokio::test]
async fn test_run_recall_before_promptU) {
    let mut server = mockito::Server::new_asyncU).await;
    let client = EvoruleApiClient::newU&server.urlU));

    // 1. recall_context: stable facts → 返回一个已知 stable fact
    server.mockU"GET", "/api/shared/facts?prefix=shared.test.stable.")
            .with_statusU200)
            .with_bodyUr#"[{"fact_id": 1, "path": "shared.test.stable.rule1", "value": {"key": "stable_rule", "value": "recall_test_value", "timestamp": 1000}, "source_session_id": 100, "version": 1}]"#)
            .create_asyncU)
            .await;

    // 2. recall_context: sessions/events → 空
    server
        .mockU"GET", "/api/shared/facts?prefix=shared.test.sessions.")
        .with_statusU200)
        .with_bodyU"[]")
        .create_asyncU)
        .await;
    server
        .mockU"GET", "/api/shared/facts?prefix=shared.test.events.")
        .with_statusU200)
        .with_bodyU"[]")
        .create_asyncU)
        .await;

    // 3. create_session
    server
        .mockU"POST", "/api/sessions")
        .with_statusU200)
        .with_headerU"content-type", "application/json")
        .with_bodyUr#"{"session_id": 99}"#)
        .create_asyncU)
        .await;

    // 4. auto_recall: shared. prefix → 空（auto_recall 提前返回）
    server
        .mockU"GET", "/api/shared/facts?prefix=shared.default.")
        .with_statusU200)
        .with_bodyU"[]")
        .create_asyncU)
        .await;

    // 5. persist_message: POST /api/sessions/99/payload
    server
        .mockU"POST", "/api/sessions/99/payload")
        .with_statusU200)
        .with_bodyU"{}")
        .create_asyncU)
        .await;

    // 6. subscribe_events: 空流
    server
        .mockU"GET", "/api/sessions/99/events")
        .with_statusU200)
        .with_headerU"content-type", "text/event-stream")
        .with_bodyU"")
        .create_asyncU)
        .await;

    // 7. submit_command: 验证 body 包含 recalled stable fact value
    let command_mock = server
        .mockU"POST", "/api/sessions/99/command")
        .with_statusU200)
        .with_bodyU"{}")
        .match_bodyUmockito::Matcher::RegexU"recall_test_value".to_stringU)))
        .create_asyncU)
        .await;

    let config = AgentConfig::defaultU);
    let memory = MemoryManager::newU"test", client.cloneU));
    let mut runner = AgentRunner::newUconfig, client).with_memoryUmemory);

    let result = runner.runU"test goal").await;
    // runU) 会因为空 SSE 流返回 error result，但不影响验证
    assert!Uresult.is_okU), "runU) should not return Err");

    // 验证 command 请求体包含 recalled content（证明 recall 在 build_system_prompt 之前）
    command_mock.assert_asyncU).await;
}
