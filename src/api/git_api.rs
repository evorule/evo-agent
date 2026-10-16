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
//! - `POST /api/git/commit-message/generate` → staged diff → LLM → 提交消息（B15）
//!
//! B15 生成边界（板块设计档）：生成 = **只读操作**（diff 读取 + LLM 调用），
//! 无工作区写入、无提交动作，不触审批流——提交动作永远在人；结果仅写入
//! 前端提交输入框供编辑，serve 绝不自动 commit。diff 内容发往用户自配的
//! LLM 供应商（与对话同通道 `llm_config`）；`aiCommit.excludePaths` 命中的
//! lockfile/生成物只留摘要行不进 prompt。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::Value;
use tracing::info;

use crate::api::agent_api::AgentApiState;
use crate::git::{GitDiff, GitError, GitOps, GitStatus};
use crate::io_handler::IoHandler as _;
use crate::io_handlers::LlmHandler;

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

// =====================================================================================
// B15：AI commit message 生成（staged diff → 三级截断 → 风格示例 → LLM 薄委托）
// =====================================================================================

/// 生成输入三级截断预算（定值：24k 字符；不新增设置键）
const GEN_PATCH_BUDGET: usize = 24_000;

/// `POST /api/git/commit-message/generate` 请求体（空对象占位，显式声明便于扩展）
#[derive(Debug, Deserialize)]
pub struct GenerateBody {}

/// 生成管线的单文件输入切片（`StagedPatch` + 排除标记的端点侧视图）
#[derive(Debug, Clone)]
struct GenFile {
    path: String,
    binary: bool,
    excluded: bool,
    additions: usize,
    deletions: usize,
    patch: String,
    hunks: Vec<String>,
}

/// 摘要行（L1 排除件注记 / L3 摘要表共用形态）
fn summary_line(f: &GenFile) -> String {
    let kind = if f.binary {
        "binary"
    } else if f.excluded {
        "excluded"
    } else {
        "changed"
    };
    format!(
        "- {} | +{} -{} | {}",
        f.path, f.additions, f.deletions, kind
    )
}

/// 排除件注记块：任何截断级别下排除文件都只留摘要行
fn compose_with_excluded(body: String, files: &[GenFile]) -> String {
    let mut out = body;
    let excluded: Vec<String> = files
        .iter()
        .filter(|f| f.excluded)
        .map(summary_line)
        .collect();
    if !excluded.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("\n以下文件按 excludePaths 规则省略正文:\n");
        for line in excluded {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// 三级截断管线：全文 ≤24k → per-file stat 骨架 → 摘要表
///
/// L2 骨架 = 文件头（路径+±行数）+ hunk 头（含函数上下文）；L3 仅摘要表，
/// 超预算再按行截断并留省略注记。
fn truncate_pipeline(files: &[GenFile], budget: usize) -> String {
    let included: Vec<&GenFile> = files.iter().filter(|f| !f.excluded).collect();

    // L1：全文 patch
    let full: String = included
        .iter()
        .map(|f| f.patch.as_str())
        .collect::<Vec<_>>()
        .join("");
    if full.chars().count() <= budget {
        return compose_with_excluded(full, files);
    }

    // L2：per-file stat 骨架（路径+±行数+hunk 头函数上下文）
    let skeleton: String = included
        .iter()
        .map(|f| {
            let mut s = format!(
                "diff --git a/{p} b/{p} | +{a} -{d}\n",
                p = f.path,
                a = f.additions,
                d = f.deletions
            );
            if f.binary {
                s.push_str("  (binary)\n");
            }
            for h in &f.hunks {
                s.push_str("  ");
                s.push_str(h);
                s.push('\n');
            }
            s
        })
        .collect();
    if skeleton.chars().count() <= budget {
        return compose_with_excluded(skeleton, files);
    }

    // L3：仅变更摘要表（超预算再截行 + 省略注记）
    let mut used = 0usize;
    let mut kept: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    for f in &included {
        let line = summary_line(f);
        let cost = line.chars().count() + 1;
        if used + cost > budget {
            dropped += 1;
            continue;
        }
        used += cost;
        kept.push(line);
    }
    if dropped > 0 {
        kept.push(format!("… 其余 {dropped} 个文件的摘要已省略"));
    }
    compose_with_excluded(kept.join("\n"), files)
}

/// 生成 prompt 组装（跟随仓库风格，不硬编码 Conventional Commits）
fn build_prompt(diff_input: &str, subjects: &[String]) -> String {
    let style = if subjects.is_empty() {
        "（本仓库还没有历史提交，请使用清晰简洁的祈使句风格）".to_string()
    } else {
        format!(
            "以下是本仓库最近的提交消息（新→旧），请跟随其语言与风格：\n{}",
            subjects
                .iter()
                .map(|s| format!("- {s}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    format!(
        "你是提交消息撰写助手。根据下面的 staged 变更 diff，为本仓库撰写一条提交消息。\n\
         要求：\n\
         1. 第一行是主题行：概括最重要的变更，不超过 72 个字符；\n\
         2. 跟随仓库既有提交风格；\n\
         3. 如变更较复杂，可在主题行后空一行补充要点，每行不超过 72 个字符；\n\
         4. 只输出提交消息本身，不要输出任何解释、代码围栏或额外文字。\n\n\
         {style}\n\n\
         staged diff（超大 diff 已按摘要骨架截断，尾部为排除/摘要注记）：\n{diff_input}"
    )
}

/// 剥离 LLM 输出可能包裹的 ``` 围栏（llm_ops 同款逻辑）
fn strip_code_fence(s: &str) -> String {
    let t = s.trim();
    let mut body = t.to_string();
    for m in ["```json", "```JSON", "```"] {
        if let Some(rest) = t.strip_prefix(m) {
            body = rest.trim_start().to_string();
            break;
        }
    }
    if body.ends_with("```") {
        body = body[..body.len() - 3].trim_end().to_string();
    }
    body
}

/// `POST /api/git/commit-message/generate` —— staged diff → LLM → 提交消息（B15）
///
/// 只读生成：diff 读取 + LLM 调用，无工作区写入、无提交动作、不触审批流；
/// 无 staged → 400（staged-only 门禁，不静默回退未暂存）；`aiCommit.enabled`
/// 关闭 → 400；LLM 失败/空响应 → 502（前端 toast 透传）。
pub async fn generate(
    State(state): State<AgentApiState>,
    Json(_body): Json<GenerateBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let started = std::time::Instant::now();
    let (settings, _) = state.workbench_settings().merged();
    let enabled = settings
        .get("aiCommit.enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if !enabled {
        return Err(err_response(GitError::Invalid(
            "ai commit generation is disabled by settings".into(),
        )));
    }

    // excludePaths → GlobSet（非法 glob 忽略，fail-soft；默认值见 schema）
    let mut builder = globset::GlobSetBuilder::new();
    if let Some(arr) = settings
        .get("aiCommit.excludePaths")
        .and_then(|v| v.as_array())
    {
        for g in arr.iter().filter_map(|v| v.as_str()) {
            if let Ok(glob) = globset::Glob::new(g) {
                builder.add(glob);
            }
        }
    }
    let exclude = builder.build().ok();

    // git 侧输入（阻塞段）：staged patch 切片 + 风格示例（最近 20 条 subject）
    let (patches, subjects) = git_blocking(&state, |ops| {
        let patches = ops.staged_patch()?;
        if patches.is_empty() {
            return Err(GitError::Invalid("no staged changes".into()));
        }
        let subjects = ops
            .log(20)?
            .into_iter()
            .map(|c| c.summary)
            .collect::<Vec<_>>();
        Ok((patches, subjects))
    })
    .await?;

    // 排除标记 → 三级截断 → prompt
    let files: Vec<GenFile> = patches
        .into_iter()
        .map(|p| {
            let excluded = exclude
                .as_ref()
                .map(|gs| gs.is_match(&p.path))
                .unwrap_or(false);
            GenFile {
                path: p.path,
                binary: p.binary,
                excluded,
                additions: p.additions,
                deletions: p.deletions,
                patch: p.patch,
                hunks: p.hunks,
            }
        })
        .collect();
    let diff_input = truncate_pipeline(&files, GEN_PATCH_BUDGET);
    let prompt = build_prompt(&diff_input, &subjects);

    // LLM 薄委托（llm_ops 同构：mock 开关 + 统一解析产物；非流式）
    let llm = match std::env::var("EVO_AGENT_LLM_MOCK_CONTENT") {
        Ok(mock) => LlmHandler::mock(&mock),
        Err(_) => LlmHandler::from_config(state.llm_config()),
    };
    let io = llm
        .execute(&serde_json::json!({ "prompt": prompt, "temperature": 0.2 }))
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                Json(Value::from(format!("llm call failed: {e}"))),
            )
        })?;
    let content = io
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let message = strip_code_fence(&content).trim().to_string();
    if message.is_empty() {
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(Value::from("llm returned empty commit message")),
        ));
    }
    info!(
        chars = message.chars().count(),
        ms = started.elapsed().as_millis() as u64,
        "AI commit message generated"
    );
    // 治理留痕（record-only，best-effort）：生成动作+耗时+产物长度,
    // 不记 diff 内容与 message 正文；生成写输入框可编辑、绝不自动提交
    crate::governance::hooks::ai_commit(&format!(
        "done in {}ms, {} chars",
        started.elapsed().as_millis(),
        message.chars().count()
    ));
    Ok(Json(serde_json::json!({ "message": message })))
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
            Arc::new(std::sync::RwLock::new(
                crate::io_handlers::tool_handler::ToolHandler::new(),
            )),
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

    // =================================================================================
    // B15：AI commit message 生成
    // =================================================================================

    /// 合成 GenFile（截断管线单测夹具；patch 带文件头行模拟真实形态）
    fn gen_file(path: &str, patch_chars: usize, hunks: usize) -> GenFile {
        GenFile {
            path: path.to_string(),
            binary: false,
            excluded: false,
            additions: patch_chars,
            deletions: 0,
            patch: format!(
                "diff --git a/{p} b/{p}\n{body}",
                p = path,
                body = "x".repeat(patch_chars)
            ),
            hunks: (0..hunks)
                .map(|i| format!("@@ -{i} +{i} @@ fn ctx_{i}()"))
                .collect(),
        }
    }

    #[test]
    fn test_truncate_pipeline_level1_full_patch() {
        let files = vec![gen_file("a.rs", 100, 1), gen_file("b.rs", 50, 1)];
        let out = truncate_pipeline(&files, GEN_PATCH_BUDGET);
        assert!(out.contains(&"x".repeat(100)), "L1 keeps full patch");
        assert!(out.contains("diff --git"), "L1 is raw patch text");
    }

    #[test]
    fn test_truncate_pipeline_level2_stat_skeleton() {
        // 单文件 patch 30k > 24k 预算 → 降 L2 骨架（路径+±行数+hunk 头）
        let files = vec![gen_file("big.rs", 30_000, 2)];
        let out = truncate_pipeline(&files, GEN_PATCH_BUDGET);
        assert!(
            out.chars().count() < 30_000,
            "skeleton far smaller than full patch: {}",
            out.chars().count()
        );
        assert!(out.contains("diff --git a/big.rs b/big.rs | +30000 -0"));
        assert!(
            out.contains("@@ -0 +0 @@ fn ctx_0()"),
            "hunk ctx kept: {out}"
        );
        assert!(!out.contains(&"x".repeat(100)), "full body dropped");
    }

    #[test]
    fn test_truncate_pipeline_level3_summary_only() {
        // 1000 文件骨架 ~48k > 24k → 降 L3 摘要表；摘要表 ~34k 仍超 → 截行+省略注记
        let files: Vec<GenFile> = (0..1000)
            .map(|i| gen_file(&format!("f{i}.txt"), 40, 1))
            .collect();
        let out = truncate_pipeline(&files, GEN_PATCH_BUDGET);
        assert!(out.contains("| +40 -0"), "summary rows: {out}");
        assert!(out.contains("省略"), "ellipsis note present");
        assert!(
            out.chars().count() <= GEN_PATCH_BUDGET + 100,
            "bounded output"
        );
    }

    #[test]
    fn test_truncate_pipeline_excluded_keeps_summary_row_only() {
        let mut f = gen_file("pkg/pnpm-lock.yaml", 100, 1);
        f.excluded = true;
        let files = vec![gen_file("a.rs", 20, 1), f];
        let out = truncate_pipeline(&files, GEN_PATCH_BUDGET);
        assert!(out.contains("a.rs"), "included patch kept");
        assert!(out.contains("x".repeat(20).as_str()));
        assert!(out.contains("- pkg/pnpm-lock.yaml | +100 -0 | excluded"));
        assert!(out.contains("excludePaths 规则省略正文"));
    }

    #[tokio::test]
    async fn test_generate_400_when_no_staged() {
        let dir = tempfile::tempdir().unwrap();
        seed_repo(&dir.path().to_path_buf()); // 干净仓
        let app = router(make_git_state(dir.path().to_path_buf()));
        let (status, body) = call(
            app,
            "POST",
            "/api/git/commit-message/generate",
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "no staged changes");
    }

    #[tokio::test]
    async fn test_generate_disabled_by_workspace_setting() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().to_path_buf();
        seed_repo(&workdir);
        std::fs::create_dir_all(workdir.join(".evo")).unwrap();
        std::fs::write(
            workdir.join(".evo/settings.json"),
            r#"{ "aiCommit.enabled": false }"#,
        )
        .unwrap();
        std::fs::write(workdir.join("a.txt"), "changed\n").unwrap();

        let app = router(make_git_state(workdir));
        let (status, body) = call(
            app,
            "POST",
            "/api/git/commit-message/generate",
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"].as_str().unwrap().contains("disabled"),
            "disabled gate: {body}"
        );
    }

    #[tokio::test]
    async fn test_generate_mock_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().to_path_buf();
        seed_repo(&workdir);
        std::fs::write(workdir.join("a.txt"), "v2\n").unwrap();
        std::fs::write(workdir.join("pkg.lock"), "locked\n").unwrap();
        let app = router(make_git_state(workdir.clone()));

        // 暂存两个文件（lock 命中默认 excludePaths，只留摘要行）
        let (s, b) = call(
            app.clone(),
            "POST",
            "/api/git/stage",
            Some(serde_json::json!({ "paths": ["a.txt", "pkg.lock"] })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "stage: {b}");

        // mock LLM 全链路（无 staged 之外的 LLM 消费方，env 设置无并行串扰）
        std::env::set_var("EVO_AGENT_LLM_MOCK_CONTENT", "feat: mock generated message");
        let (status, body) = call(
            app.clone(),
            "POST",
            "/api/git/commit-message/generate",
            Some(serde_json::json!({})),
        )
        .await;
        std::env::remove_var("EVO_AGENT_LLM_MOCK_CONTENT");
        assert_eq!(status, StatusCode::OK, "generate: {body}");
        assert_eq!(body["message"], "feat: mock generated message");

        // 生成是只读操作：不提交、不改动工作区（status 仍 dirty）
        let (_, b) = call(app, "GET", "/api/git/status", None).await;
        assert_eq!(b["dirty"], true, "generation must not commit");
    }
}
