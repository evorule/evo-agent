use mockito::{Server, ServerGuard};
use serde_json::json;
use evo_agent::agent::runner::{AgentConfig, AgentRunner};
use evo_agent::api::evorule_client::EvoruleApiClient;

#[tokio::test]
async fn test_auto_recall_with_mock_server() {
    let mut server = Server::new_async().await;
    let server_url = server.url();

    server.mock("POST", "/api/sessions")
        .with_status(200)
        .with_body(r#"{"session_id": 123}"#)
        .create_async()
        .await;

    server.mock("GET", "/api/shared/facts?prefix=shared.")
        .with_status(200)
        .with_body(json!([
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
        ]).to_string())
        .create_async()
        .await;

    server.mock("POST", "/api/sessions/123/used_at_startup")
        .with_status(200)
        .with_body(r#"{"success": true, "message": "used_at_startup recorded"}"#)
        .create_async()
        .await;

    server.mock("POST", "/api/sessions/123/command")
        .with_status(200)
        .with_body(r#"{"success": true, "fact_id": 1}"#)
        .create_async()
        .await;

    let sse_events = "data: {\"type\":\"io_request\",\"payload\":{\"io_type\":\"call_external\",\"id\":1,\"params\":{\"model\":\"gpt-4o-mini\",\"system_prompt\":\"You are a helpful assistant\",\"goal\":\"Summarize research notes\",\"tool_names\":[]}}}\r\n\r\ndata: {\"type\":\"stable\",\"payload\":\"Task completed successfully\"}\r\n\r\n";
    server.mock("GET", "/api/sessions/123/events")
        .with_status(200)
        .with_header("Content-Type", "text/event-stream")
        .with_body(sse_events)
        .create_async()
        .await;

    server.mock("GET", "/api/sessions/123/state")
        .with_status(200)
        .with_body(r#"{"payload": "Task completed successfully"}"#)
        .create_async()
        .await;

    server.mock("POST", "/api/sessions/123/io_response")
        .with_status(200)
        .with_body(r#"{"success": true, "message": "IoResponse submitted"}"#)
        .create_async()
        .await;

    let client = EvoruleApiClient::new(&server_url);
    let config = AgentConfig::default();
    let mut runner = AgentRunner::new(config, client);

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

    server.mock("POST", "/api/sessions")
        .with_status(200)
        .with_body(r#"{"session_id": 456}"#)
        .create_async()
        .await;

    server.mock("GET", "/api/shared/facts?prefix=shared.")
        .with_status(200)
        .with_body("[]")
        .create_async()
        .await;

    server.mock("POST", "/api/sessions/456/command")
        .with_status(200)
        .with_body(r#"{"success": true, "fact_id": 1}"#)
        .create_async()
        .await;

    let sse_events_error = "data: {\"type\":\"error\",\"payload\":{\"message\":\"LLM timeout\"}}\r\n\r\n";
    server.mock("GET", "/api/sessions/456/events")
        .with_status(200)
        .with_header("Content-Type", "text/event-stream")
        .with_body(sse_events_error)
        .create_async()
        .await;

    server.mock("GET", "/api/sessions/456/facts")
        .with_status(200)
        .with_body(json!([
            {"version": 0, "id": 1, "path": "init", "value": "init", "type": "PayloadUpdate"},
            {"version": 1, "id": 2, "path": "command", "value": "test", "type": "Command"}
        ]).to_string())
        .create_async()
        .await;

    server.mock("GET", "/api/sessions/456/rewind/0")
        .with_status(200)
        .with_body(r#"{"version": 0, "payload": {}, "queue": []}"#)
        .create_async()
        .await;

    let client = EvoruleApiClient::new(&server_url);
    let config = AgentConfig::default();
    let mut runner = AgentRunner::new(config, client);

    let result = runner.run("Test rewind on error").await;

    assert!(result.is_ok());
    let result = result.unwrap();
    assert!(!result.success);
    assert!(result.error.is_some());
}

#[tokio::test]
async fn test_auto_recall_no_shared_facts() {
    let mut server = Server::new_async().await;
    let server_url = server.url();

    server.mock("POST", "/api/sessions")
        .with_status(200)
        .with_body(r#"{"session_id": 789}"#)
        .create_async()
        .await;

    server.mock("GET", "/api/shared/facts?prefix=shared.")
        .with_status(200)
        .with_body("[]")
        .create_async()
        .await;

    server.mock("POST", "/api/sessions/789/command")
        .with_status(200)
        .with_body(r#"{"success": true, "fact_id": 1}"#)
        .create_async()
        .await;

    let sse_events = "data: {\"type\":\"io_request\",\"payload\":{\"io_type\":\"call_external\",\"id\":1,\"params\":{\"model\":\"gpt-4o-mini\",\"system_prompt\":\"You are a helpful assistant\",\"goal\":\"Simple task\",\"tool_names\":[]}}}\r\n\r\ndata: {\"type\":\"stable\",\"payload\":\"Simple task completed\"}\r\n\r\n";
    server.mock("GET", "/api/sessions/789/events")
        .with_status(200)
        .with_header("Content-Type", "text/event-stream")
        .with_body(sse_events)
        .create_async()
        .await;

    server.mock("GET", "/api/sessions/789/state")
        .with_status(200)
        .with_body(r#"{"payload": "Simple task completed"}"#)
        .create_async()
        .await;

    server.mock("POST", "/api/sessions/789/io_response")
        .with_status(200)
        .with_body(r#"{"success": true, "message": "IoResponse submitted"}"#)
        .create_async()
        .await;

    let client = EvoruleApiClient::new(&server_url);
    let config = AgentConfig::default();
    let mut runner = AgentRunner::new(config, client);

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

    server.mock("POST", "/api/sessions")
        .with_status(200)
        .with_body(r#"{"session_id": 999}"#)
        .create_async()
        .await;

    server.mock("GET", "/api/shared/facts?prefix=shared.")
        .with_status(200)
        .with_body(json!([
            {
                "fact_id": 10,
                "path": "shared.knowledge.rules",
                "value": "Always follow safety guidelines",
                "source_session_id": 200,
                "version": 5
            }
        ]).to_string())
        .create_async()
        .await;

    server.mock("POST", "/api/sessions/999/used_at_startup")
        .with_status(200)
        .with_body(r#"{"success": true}"#)
        .create_async()
        .await;

    server.mock("POST", "/api/sessions/999/command")
        .with_status(200)
        .with_body(r#"{"success": true, "fact_id": 1}"#)
        .create_async()
        .await;

    let sse_events = "data: {\"type\":\"phase_change\",\"payload\":{\"phase\":\"planning\"}}\r\n\r\ndata: {\"type\":\"io_request\",\"payload\":{\"io_type\":\"call_external\",\"id\":1,\"params\":{\"model\":\"gpt-4o-mini\",\"system_prompt\":\"Safety first assistant\",\"goal\":\"Analyze safety protocols\",\"tool_names\":[\"safety_check\"]}}}\r\n\r\ndata: {\"type\":\"state_transition\",\"payload\":{}}\r\n\r\ndata: {\"type\":\"stable\",\"payload\":\"Safety analysis complete\"}\r\n\r\n";
    server.mock("GET", "/api/sessions/999/events")
        .with_status(200)
        .with_header("Content-Type", "text/event-stream")
        .with_body(sse_events)
        .create_async()
        .await;

    server.mock("GET", "/api/sessions/999/state")
        .with_status(200)
        .with_body(r#"{"payload": "Safety analysis complete"}"#)
        .create_async()
        .await;

    server.mock("POST", "/api/sessions/999/io_response")
        .with_status(200)
        .with_body(r#"{"success": true, "message": "IoResponse submitted"}"#)
        .create_async()
        .await;

    let client = EvoruleApiClient::new(&server_url);
    let mut config = AgentConfig::default();
    config.system_prompt = "Safety first assistant".to_string();
    config.tool_names = vec!["safety_check".to_string()];

    let mut runner = AgentRunner::new(config, client);

    let result = runner.run("Analyze safety protocols").await;

    assert!(result.is_ok());
    let result = result.unwrap();
    assert!(result.success);
    assert_eq!(result.content, "Safety analysis complete");
    assert_eq!(result.steps, 1);
}
