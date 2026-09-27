// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 工作台 Git REST 面 —— SCM 侧栏视图与 diff 视图的消费端点（B3）
//!
//! 七个端点全部薄委托 [`crate::git::GitOps`](crate::git::GitOps)（单一实现，
//! agent `git_tools` 与 REST 面共享同一语义）；全挂 G16 鉴权中间件（挂载于
//! `agent_api::router` 的 route_layer）。与 `file_api` 同款主体语义：
//!
//! - 本面服务的是**人**在工作台 SCM 视图中的直接操作（查看/暂存/丢弃/提交），
//!   **不进 agent 会话审计链**——审计链记录 agent 执行行为，人的 UI 操作
//!   伪造成 agent 事实反而污染审计真实性；
//! - agent 路径的 git 工具（`git_tools`，走 candidate 审批 + call_external
//!   过引擎入审计链）完全不受影响，两个消费面互不干涉；
//! - serve 不代写 git config：身份缺失只返回结构化错误引导用户自行配置。
//!
//! ## 端点一览
//!
//! - `GET  /api/git/status`   → 双态 status（分支/暂存组/更改组）
//! - `GET  /api/git/diff?path=` → HEAD 版 vs 工作区版两版全文
//! - `POST /api/git/stage`    `{paths: []}` 暂存
//! - `POST /api/git/unstage`  `{paths: []}` 取消暂存
//! - `POST /api/git/discard`  `{paths: []}` 丢弃工作区变更（前端强制确认）
//! - `POST /api/git/commit`   `{message}` 提交（身份预检 + hooks 分流）
//! - `GET  /api/git/identity` → 提交身份预检（前端先查，后端再查双保险）

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::Value;

use crate::api::agent_api::AgentApiState;
use crate::git::{GitDiff, GitError, GitOps, GitStatus};

/// git 错误 → HTTP 响应体（400 语义明确：非 git 仓/边缘形态/身份缺失/hooks 拒绝）
fn err_response(e: GitError) -> (StatusCode, Json<Value>) {
    let mut body = serde_json::Map::new();
    body.insert("error".to_string(), Value::from(e.message()));
    if let Some(hint) = e.hint() {
        body.insert("hint".to_string(), Value::from(hint));
    }
    (e.http_status(), Json(Value::Object(body)))
}

/// 阻塞型 git 操作统一跑 spawn_blocking（statuses 在大仓可秒级，不占 async 线程）
async fn git_blocking<T, F>(state: &AgentApiState, f: F) -> Result<T, (StatusCode, Json<Value>)>
where
    F: FnOnce(GitOps) -> Result<T, GitError> + Send + 'static,
    T: Send + 'static,
{
    let workdir = state.workdir().to_path_buf();
    tokio::task::spawn_blocking(move || f(GitOps::new(workdir)))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(Value::from(format!("git task panicked: {e}"))),
            )
        })?
        .map_err(err_response)
}

/// `GET /api/git/status` 查询参数（占位：无参数，显式声明空结构便于扩展）
#[derive(Debug, Deserialize)]
pub struct EmptyQuery {}

/// `GET /api/git/diff` 查询参数
#[derive(Debug, Deserialize)]
pub struct DiffQuery {
    /// 相对 workdir 的文件路径
    pub path: String,
}

/// `POST /api/git/stage|unstage|discard` 请求体
#[derive(Debug, Deserialize)]
pub struct PathsBody {
    /// 相对 workdir 的路径列表（untracked 目录折叠条目带尾 `/`）
    pub paths: Vec<String>,
}

/// `POST /api/git/commit` 请求体
#[derive(Debug, Deserialize)]
pub struct CommitBody {
    /// 提交消息（空消息后端拒绝；前端先行拦截）
    pub message: String,
}

/// `GET /api/git/status` —— 双态 status（SCM 视图 + Explorer 装饰单状态源）
pub async fn status(
    State(state): State<AgentApiState>,
    Query(_q): Query<EmptyQuery>,
) -> Result<Json<GitStatus>, (StatusCode, Json<Value>)> {
    git_blocking(&state, |ops| ops.status()).await.map(Json)
}

/// `GET /api/git/diff` —— 两版全文（Monaco DiffEditor 直接消费）
pub async fn diff(
    State(state): State<AgentApiState>,
    Query(q): Query<DiffQuery>,
) -> Result<Json<GitDiff>, (StatusCode, Json<Value>)> {
    let path = q.path;
    git_blocking(&state, move |ops| ops.diff(&path))
        .await
        .map(Json)
}

/// `POST /api/git/stage` —— 暂存（文件/折叠目录）
pub async fn stage(
    State(state): State<AgentApiState>,
    Json(body): Json<PathsBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let paths = body.paths;
    git_blocking(&state, move |ops| ops.stage(&paths).map(Value::from))
        .await
        .map(Json)
}

/// `POST /api/git/unstage` —— 取消暂存
pub async fn unstage(
    State(state): State<AgentApiState>,
    Json(body): Json<PathsBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let paths = body.paths;
    git_blocking(&state, move |ops| ops.unstage(&paths).map(Value::from))
        .await
        .map(Json)
}

/// `POST /api/git/discard` —— 丢弃工作区变更（危险；前端强制 ConfirmDialog）
pub async fn discard(
    State(state): State<AgentApiState>,
    Json(body): Json<PathsBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let paths = body.paths;
    git_blocking(&state, move |ops| ops.discard(&paths).map(Value::from))
        .await
        .map(Json)
}

/// `POST /api/git/commit` —— 提交（身份预检 + hooks 检测分流；空消息 400）
pub async fn commit(
    State(state): State<AgentApiState>,
    Json(body): Json<CommitBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let message = body.message;
    git_blocking(&state, move |ops| {
        ops.commit(&message)
            .map(|commit_id| serde_json::json!({ "commitId": commit_id }))
    })
    .await
    .map(Json)
}

/// `GET /api/git/identity` —— 提交身份预检（`{name,email}` | `{missing:true}`）
pub async fn identity(
    State(state): State<AgentApiState>,
    Query(_q): Query<EmptyQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    git_blocking(&state, |ops| {
        ops.identity().map(|id| match id {
            Some((name, email)) => serde_json::json!({ "name": name, "email": email }),
            None => serde_json::json!({ "missing": true }),
        })
    })
    .await
    .map(Json)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::api::agent_api::{router, AgentApiState};
    use crate::api::evorule_client::EvoruleApiClient;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use git2::Repository;
    use tower::ServiceExt;

    use std::path::PathBuf;
    use std::sync::Arc;

    /// 带临时 git 仓 workdir 的测试 state（new_with_metrics 注入 workdir）
    fn make_git_state(workdir: PathBuf) -> AgentApiState {
        AgentApiState::new_with_metrics(
            crate::agent::AgentDefinitionManager::with_default_dir(),
            EvoruleApiClient::new("http://localhost:0"),
            Arc::new(crate::api::Metrics::new().unwrap_or_else(|e| panic!("metrics: {e}"))),
            workdir,
            Arc::new(crate::api::workspace_client::WorkspaceApiClient::new(
                "http://localhost:0",
            )),
            Arc::new(crate::io_handlers::tool_handler::ToolHandler::new()),
        )
    }

    /// 临时 git 仓夹具：init + 身份 + 初始提交
    fn seed_repo(workdir: &PathBuf) {
        let repo = Repository::init(workdir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Tester").unwrap();
        cfg.set_str("user.email", "t@example.com").unwrap();
        // 隔离宿主全局 core.autocrlf 对 checkout 内容的改写（测试内容断言稳定性）
        cfg.set_bool("core.autocrlf", false).unwrap();
        drop(cfg);
        drop(repo);
        std::fs::write(workdir.join("a.txt"), "base\n").unwrap();
        let ops = GitOps::new(workdir.clone());
        ops.stage(&["a.txt".to_string()]).unwrap();
        ops.commit("init").unwrap();
    }

    async fn call(
        app: axum::Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let req = match body {
            Some(v) => Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        };
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    #[tokio::test]
    async fn test_status_400_on_non_repo() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(make_git_state(dir.path().to_path_buf()));
        let (status, body) = call(app, "GET", "/api/git/status", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "not a git repository");
    }

    #[tokio::test]
    async fn test_status_and_stage_unstage_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        seed_repo(&dir.path().to_path_buf());
        std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "n\n").unwrap();

        let app = router(make_git_state(dir.path().to_path_buf()));
        let (status, body) = call(app.clone(), "GET", "/api/git/status", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["branch"], "master");
        assert_eq!(body["dirty"], true);
        assert_eq!(body["changes"].as_array().unwrap().len(), 2);

        // stage 往返
        let (s, b) = call(
            app.clone(),
            "POST",
            "/api/git/stage",
            Some(serde_json::json!({ "paths": ["a.txt"] })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "stage: {b}");
        let (_, b) = call(app.clone(), "GET", "/api/git/status", None).await;
        assert!(
            b["staged"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["path"] == "a.txt"),
            "a.txt staged: {b}"
        );

        // unstage 回到更改组
        let (s, b) = call(
            app.clone(),
            "POST",
            "/api/git/unstage",
            Some(serde_json::json!({ "paths": ["a.txt"] })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "unstage: {b}");
        let (_, b) = call(app, "GET", "/api/git/status", None).await;
        assert!(
            b["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["path"] == "a.txt"),
            "a.txt back in changes: {b}"
        );
    }

    #[tokio::test]
    async fn test_diff_and_discard_flow() {
        let dir = tempfile::tempdir().unwrap();
        seed_repo(&dir.path().to_path_buf());
        std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();

        let app = router(make_git_state(dir.path().to_path_buf()));
        let (s, b) = call(app.clone(), "GET", "/api/git/diff?path=a.txt", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["original"], "base\n");
        assert_eq!(b["modified"], "changed\n");

        // discard 恢复 + 400 语义（不存在的路径）
        let (s, b) = call(
            app.clone(),
            "POST",
            "/api/git/discard",
            Some(serde_json::json!({ "paths": ["a.txt"] })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "discard: {b}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "base\n"
        );

        // 越界路径 400
        let (s, _) = call(
            app,
            "POST",
            "/api/git/discard",
            Some(serde_json::json!({ "paths": ["../escape.txt"] })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_commit_and_identity_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        seed_repo(&dir.path().to_path_buf());
        std::fs::write(dir.path().join("a.txt"), "v2\n").unwrap();

        let app = router(make_git_state(dir.path().to_path_buf()));

        // 身份预检：已配置 → {name,email}
        let (s, b) = call(app.clone(), "GET", "/api/git/identity", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["name"], "Tester");

        // 空消息 → 400
        let (s, _) = call(
            app.clone(),
            "POST",
            "/api/git/commit",
            Some(serde_json::json!({ "message": "  " })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        // 提交 → commitId + 状态干净
        let (s, b) = call(
            app.clone(),
            "POST",
            "/api/git/commit",
            Some(serde_json::json!({ "message": "workbench commit" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "commit: {b}");
        assert_eq!(b["commitId"].as_str().unwrap().len(), 40);
        let (_, b) = call(app, "GET", "/api/git/status", None).await;
        assert_eq!(b["dirty"], false, "clean after commit: {b}");
    }

    #[tokio::test]
    async fn test_commit_identity_missing_structured_error() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().to_path_buf();
        let repo = Repository::init(&workdir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "").unwrap();
        cfg.set_str("user.email", "").unwrap();
        drop(cfg);
        drop(repo);
        std::fs::write(workdir.join("a.txt"), "x\n").unwrap();

        let app = router(make_git_state(workdir));
        // 身份预检端点：missing:true
        let (s, b) = call(app.clone(), "GET", "/api/git/identity", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["missing"], true);
        // 提交 → 400 identity_missing + hint
        let (s, b) = call(
            app,
            "POST",
            "/api/git/commit",
            Some(serde_json::json!({ "message": "m" })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(b["error"], "identity_missing");
        assert!(b["hint"].as_str().unwrap().contains("git config"));
    }
}
