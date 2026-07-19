// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Agent API 鈥斺€?HTTP 鎺ュ彛绠＄悊 Agent 鎵ц

use axum::{extract::State, http::StatusCode, Json, Router};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::agent::{AgentDefinitionManager, AgentRunner};
use crate::api::evorule_client::EvoruleApiClient;

/// Agent 杩愯璇锋眰
#[derive(Debug, Serialize, Deserialize)]
pub struct AgentRunRequest {
    /// Agent 绫诲瀷
    pub agent_type: String,
    /// 鐩爣
    pub goal: String,
    /// 鏈€澶ф楠わ紙鍙€夛級
    pub max_steps: Option<usize>,
    /// 娓╁害鍙傛暟锛堝彲閫夛級
    pub temperature: Option<f32>,
    /// 妯″瀷鍚嶇О锛堝彲閫夛級
    pub model: Option<String>,
}

/// Agent 杩愯鍝嶅簲
#[derive(Debug, Serialize)]
pub struct AgentRunResponse {
    /// 鏄惁鎴愬姛
    pub success: bool,
    /// 缁撴灉鍐呭
    pub content: String,
    /// 鎵ц姝ラ鏁
    pub steps: usize,
    /// 鎵ц鑰楁椂锛堟绉掞級
    pub duration_ms: u64,
    /// 閿欒淇℃伅锛堝鏋滃け璐ワ級
    pub error: Option<String>,
}

/// Agent 鍒楄〃鍝嶅簲
#[derive(Debug, Serialize)]
pub struct AgentListResponse {
    /// Agent 鍒楄〃
    pub agents: Vec<AgentInfo>,
}

/// Agent 淇℃伅
#[derive(Debug, Serialize)]
pub struct AgentInfo {
    /// Agent 绫诲瀷
    pub agent_type: String,
    /// 鐗堟湰鍙
    pub version: String,
    /// 鎻忚堪
    pub description: String,
    /// 宸ュ叿鍒楄〃
    pub tools: Vec<String>,
}

/// Agent 瀹氫箟鍝嶅簲
#[derive(Debug, Serialize)]
pub struct AgentDefinitionResponse {
    /// Agent 绫诲瀷
    pub agent_type: String,
    /// 鐗堟湰鍙
    pub version: String,
    /// 鎻忚堪
    pub description: String,
    /// 绯荤粺鎻愮ず璇
    pub system_prompt: String,
    /// 妯″瀷鍚嶇О
    pub model: String,
    /// 娓╁害鍙傛暟
    pub temperature: f32,
    /// 鏈€澶ф楠
    pub max_steps: usize,
    /// 宸ュ叿鍒楄〃
    pub tools: Vec<String>,
    /// 鍐呭瓨閰嶇疆
    pub memory_config: Option<crate::agent::MemoryConfig>,
}

/// Agent API 鐘舵€
#[derive(Debug, Clone)]
pub struct AgentApiState {
    definitions: AgentDefinitionManager,
    evorule_client: EvoruleApiClient,
}

impl AgentApiState {
    /// 鍒涘缓鏂扮殑 Agent API 鐘舵€
    pub fn new(definitions: AgentDefinitionManager, evorule_client: EvoruleApiClient) -> Self {
        Self {
            definitions,
            evorule_client,
        }
    }
}

/// 鍒涘缓 Agent API 璺敱
pub fn router(state: AgentApiState) -> Router {
    Router::new()
        .route("/agents", axum::routing::get(list_agents))
        .route("/agents/{agent_type}", axum::routing::get(get_agent))
        .route("/agents/{agent_type}/run", axum::routing::post(run_agent))
        .with_state(state)
}

async fn list_agents(State(state): State<AgentApiState>) -> Json<AgentListResponse> {
    let types = match state.definitions.list_types() {
        Ok(t) => t,
        Err(e) => {
            warn!("Failed to list agents: {}", e);
            return Json(AgentListResponse { agents: Vec::new() });
        }
    };

    let mut agents = Vec::new();
    for agent_type in types {
        if let Ok(def) = state.definitions.load(&agent_type) {
            agents.push(AgentInfo {
                agent_type: def.agent_type,
                version: def.version,
                description: def.description,
                tools: def.tools,
            });
        }
    }

    Json(AgentListResponse { agents })
}

async fn get_agent(
    State(state): State<AgentApiState>,
    axum::extract::Path(agent_type): axum::extract::Path<String>,
) -> Result<Json<AgentDefinitionResponse>, StatusCode> {
    let def = state
        .definitions
        .load(&agent_type)
        .map_err(|_| StatusCode::NOT_FOUND)?;

    Ok(Json(AgentDefinitionResponse {
        agent_type: def.agent_type,
        version: def.version,
        description: def.description,
        system_prompt: def.system_prompt,
        model: def.model,
        temperature: def.temperature,
        max_steps: def.max_steps,
        tools: def.tools,
        memory_config: Some(def.memory),
    }))
}

async fn run_agent(
    State(state): State<AgentApiState>,
    axum::extract::Path(agent_type): axum::extract::Path<String>,
    Json(req): Json<AgentRunRequest>,
) -> Result<Json<AgentRunResponse>, StatusCode> {
    let def = state
        .definitions
        .load(&agent_type)
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let mut config = def.to_agent_config();
    if let Some(max_steps) = req.max_steps {
        config.max_steps = max_steps;
    }
    if let Some(temperature) = req.temperature {
        config.temperature = temperature;
    }
    if let Some(model) = req.model {
        config.model = model;
    }

    info!(
        agent_type = agent_type,
        goal = %req.goal,
        "Starting agent execution"
    );

    let mut runner = AgentRunner::new(config, state.evorule_client.clone());

    let result = runner.run(&req.goal).await;

    match result {
        Ok(r) => {
            info!(
                agent_type = agent_type,
                steps = r.steps,
                duration_ms = r.duration_ms,
                "Agent execution completed"
            );
            Ok(Json(AgentRunResponse {
                success: r.success,
                content: r.content,
                steps: r.steps,
                duration_ms: r.duration_ms,
                error: r.error,
            }))
        }
        Err(e) => {
            warn!(agent_type = agent_type, error = %e, "Agent execution failed");
            Ok(Json(AgentRunResponse {
                success: false,
                content: String::new(),
                steps: 0,
                duration_ms: 0,
                error: Some(e.to_string()),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn make_test_state() -> AgentApiState {
        AgentApiState::new(
            AgentDefinitionManager::with_default_dir(),
            EvoruleApiClient::new("http://localhost:8080"),
        )
    }

    #[tokio::test]
    async fn test_list_agents_empty() {
        let state = make_test_state();
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/agents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_get_agent_not_found() {
        let state = make_test_state();
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/agents/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_run_agent_not_found() {
        let state = make_test_state();
        let app = router(state);

        let request = AgentRunRequest {
            agent_type: "nonexistent".to_string(),
            goal: "test".to_string(),
            max_steps: None,
            temperature: None,
            model: None,
        };

        let response = app
            .oneshot(
                Request::builder()
                    .method(http::Method::POST)
                    .uri("/agents/nonexistent/run")
                    .header("Content-Type", "application/json")
                    .body(Body::from(serde_json::to_string(&request).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_agent_run_request_deserialize() {
        let json = r#"{
            "agent_type": "researcher",
            "goal": "search for AI news",
            "max_steps": 5,
            "temperature": 0.3,
            "model": "gpt-4"
        }"#;
        let req: AgentRunRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.agent_type, "researcher");
        assert_eq!(req.goal, "search for AI news");
        assert_eq!(req.max_steps, Some(5));
        assert_eq!(req.temperature, Some(0.3));
        assert_eq!(req.model, Some("gpt-4".to_string()));
    }

    #[test]
    fn test_agent_run_response_serialize() {
        let resp = AgentRunResponse {
            success: true,
            content: "hello".to_string(),
            steps: 3,
            duration_ms: 100,
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("success"));
        assert!(json.contains("hello"));
        assert!(json.contains("3"));
    }
}
