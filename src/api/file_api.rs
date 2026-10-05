// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 工作台文件 REST 面 —— IDE 文件树浏览与编辑器保存的消费端点
//!
//! 六个端点,全部复用 builtin_tools 的 file 工具实现(同一沙箱 / 同一校验 /
//! 同一资源限制),不在本模块重复任何路径安全逻辑:
//!
//! - `GET /api/files/list?dir=`  → [`FileListTool`](crate::builtin_tools::file_list::FileListTool)
//!   (union toolkit 内**同一实例**)
//! - `GET /api/files/read?path=` → [`FileReadTool`](crate::builtin_tools::file_read::FileReadTool)
//!   (union toolkit 内**同一实例**)
//! - `PUT /api/files/write`      → [`FileWriteTool`](crate::builtin_tools::file_write::FileWriteTool)
//!   (同实现,`writable_dir="."`:人工编辑面 = workdir 全域)
//! - `POST /api/files/create`    → [`FileCreateTool`](crate::builtin_tools::file_create::FileCreateTool)
//!   (同实现,`writable_dir="."`;持树写互斥锁)
//! - `POST /api/files/move`      → [`FileMoveTool`](crate::builtin_tools::file_move::FileMoveTool)
//!   (同实现,`writable_dir="."`;持树写互斥锁)
//! - `DELETE /api/files?path=`   → [`FileDeleteTool`](crate::builtin_tools::file_delete::FileDeleteTool)
//!   (同实现,软删除进 `.evo-trash`;持树写互斥锁)
//! - `POST /api/files/search`    → [`grep_core`](crate::builtin_tools::grep_files::grep_core)
//!   (全项目内容搜索;沙箱同款;30s 硬超时 partial)
//! - `POST /api/files/replace`   → [`replace_core`](crate::builtin_tools::grep_files::replace_core)
//!   (apply=false 预览零写盘 / apply=true 重匹配+原子写;核心层内部持树写互斥锁)
//!
//! ## 语义边界(与 agent 工具通道的差别,设计定稿留痕)
//!
//! - 本面服务的是**人**在工作台编辑器中的直接操作(打开 / 编辑 / 保存落盘)。
//!   人的 UI 操作不构造 agent 会话事实,因此**不进 agent 会话审计链**——
//!   审计链记录的是 agent 执行行为,把人的编辑操作伪造成 agent 事实反而污染
//!   审计真实性;写面操作落**人工审计账**([`human_gate`],独立 JSONL,
//!   两本账可经时间戳关联);
//! - 写面身份门(HumanGate):human(auth token 校验通过)可写并落人工账;
//!   anonymous(auth disabled)**只读**——写面一律 403,响应体带开启指引,
//!   拒绝本身同样落人工账(本地回环 + auth=false 的过渡宽限口径见 human_gate
//!   模块文档);
//! - agent 路径的 `file_write`(`writable_dir=workspace` + 管道⑤ candidate
//!   审批 + call_external 过引擎入审计链)**完全不受影响**,两个消费面互不干涉;
//! - 写面差异是主体差异的忠实反映:agent 只能写 `workspace/`(防误写源码),
//!   人(项目方)在本机本就有完整文件系统权限,workdir 沙箱对人是 UX 边界
//!   (防误操作出 workdir),不是安全边界。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::Value;

use crate::api::agent_api::AgentApiState;
use crate::api::human_gate::{ensure_write_allowed, CallerIdentity};
use crate::builtin_tools::file_create::FileCreateTool;
use crate::builtin_tools::file_delete::FileDeleteTool;
use crate::builtin_tools::file_move::FileMoveTool;
use crate::builtin_tools::file_write::FileWriteTool;
use crate::builtin_tools::fs_safety::tree_mutation_lock;
use crate::io_handlers::tool_handler::ToolFunction;

/// `GET /api/files/list` 查询参数
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// 相对 workdir 的目录路径(缺省 = workdir 根)
    pub dir: Option<String>,
}

/// `GET /api/files/read` 查询参数
#[derive(Debug, Deserialize)]
pub struct ReadQuery {
    /// 相对 workdir 的文件路径
    pub path: String,
}

/// `PUT /api/files/write` 请求体
#[derive(Debug, Deserialize)]
pub struct WriteBody {
    /// 相对 workdir 的文件路径
    pub path: String,
    /// 完整文件内容(整体覆盖写)
    pub content: String,
}

/// `POST /api/files/create` 请求体
#[derive(Debug, Deserialize)]
pub struct CreateBody {
    /// 相对 workdir 的创建目标路径(必须不存在)
    pub path: String,
    /// `file` | `dir`(缺省 file)
    pub kind: Option<String>,
}

/// `POST /api/files/move` 请求体
#[derive(Debug, Deserialize)]
pub struct MoveBody {
    /// 相对 workdir 的源路径(必须已存在)
    pub path: String,
    /// 相对 workdir 的目标目录(必须已存在)
    pub target_dir: String,
    /// 目标名(缺省 = 保留原名)
    pub new_name: Option<String>,
}

/// `DELETE /api/files` 查询参数
#[derive(Debug, Deserialize)]
pub struct DeleteQuery {
    /// 相对 workdir 的删除目标(软删除进 `.evo-trash`)
    pub path: String,
}

/// 工具错误字符串 → HTTP 状态码
/// (不存在的路径 404 / 已存在 409 / 其余 400)
fn err_status(msg: &str) -> StatusCode {
    if msg.contains("does not exist") {
        StatusCode::NOT_FOUND
    } else if msg.contains("already exists") {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    }
}

/// 调 union toolkit 中的 file 工具(list/read 与 agent 同一实例)
///
/// PR-3 入口收口(读面):直读保留(无写风险),但调用前查 manifest 在场——
/// 无 manifest 的工具拒载(NoManifest 显式拒绝,防读面越界能力域;注册期
/// manifest 强制不变量的运行期镜像面)。
async fn call_toolkit_tool(
    state: &AgentApiState,
    name: &str,
    args: Value,
) -> Result<Value, (StatusCode, String)> {
    let tool = state.toolkit().get_tool(name).ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("tool not registered in union toolkit: {name}"),
        )
    })?;
    if state.toolkit().manifest(name).is_none() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "tool '{name}' has no manifest (NoManifest) — refusing to invoke \
                 (entry-point closure: read surface must not reach unmanifested tools)"
            ),
        ));
    }
    tool.call(&args).await.map_err(|e| (err_status(&e), e))
}

/// HumanGate 写面准入门:human 放行;anonymous 拒绝(403+开启指引)且
/// 拒绝本身落人工账(outcome=denied)。
fn gate_write(
    state: &AgentApiState,
    op: &str,
    path: &str,
) -> Result<CallerIdentity, (StatusCode, String)> {
    match ensure_write_allowed(state.auth_config()) {
        Ok(identity) => Ok(identity),
        Err(e) => {
            state
                .human_gate()
                .record(op, CallerIdentity::Anonymous, path, "denied");
            Err(e)
        }
    }
}

/// 写面操作结局落人工账(执行成功 ok / 失败 error)
fn record_outcome(
    state: &AgentApiState,
    op: &str,
    identity: CallerIdentity,
    path: &str,
    result: &Result<Value, (StatusCode, String)>,
) {
    let outcome = if result.is_ok() { "ok" } else { "error" };
    state.human_gate().record(op, identity, path, outcome);
}

/// `GET /api/files/list` —— 列目录(委托 file_list 工具)
///
/// 错误 → 404(目录不存在)/ 400(路径越界等)。
pub async fn list_dir(
    State(state): State<AgentApiState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut args = serde_json::Map::new();
    if let Some(d) = &q.dir {
        args.insert("dir".to_string(), Value::from(d.as_str()));
    }
    let v = call_toolkit_tool(&state, "file_list", Value::Object(args)).await?;
    Ok(Json(v))
}

/// `GET /api/files/read` —— 读文件(委托 file_read 工具)
///
/// 错误 → 404(文件不存在)/ 400(路径越界 / 过大 / 非常规文件)。
pub async fn read_file(
    State(state): State<AgentApiState>,
    Query(q): Query<ReadQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), Value::from(q.path.as_str()));
    let v = call_toolkit_tool(&state, "file_read", Value::Object(args)).await?;
    Ok(Json(v))
}

/// `PUT /api/files/write` —— 写文件(委托 file_write 实现,`writable_dir="."`)
///
/// IDE 保存语义:已打开的文件必然已存在,`overwrite=true` 即「保存覆盖」。
/// HumanGate:human 可写(落人工账);anonymous 403(拒绝留痕)。
/// 错误 → 403(anonymous)/ 400(路径越界 / 内容过大)。
pub async fn write_file(
    State(state): State<AgentApiState>,
    Json(body): Json<WriteBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let identity = gate_write(&state, "file_write", &body.path)?;
    let tool = FileWriteTool::new(state.workdir().to_path_buf()).with_writable_dir(".");
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), Value::from(body.path.as_str()));
    args.insert("content".to_string(), Value::from(body.content.as_str()));
    args.insert("overwrite".to_string(), Value::Bool(true));
    args.insert("create_parents".to_string(), Value::Bool(true));
    let result = tool
        .call(&Value::Object(args))
        .await
        .map_err(|e| (err_status(&e), e));
    record_outcome(&state, "file_write", identity, &body.path, &result);
    result.map(Json)
}

/// `POST /api/files/create` —— 创建文件/目录(委托 file_create 实现,`writable_dir="."`)
///
/// 人工面语义:人工面不走 agent 审批(人即最终审批者,REST 面无 agent 会话
/// 审批通道),`create_parents=true` 与写面同语义(IDE 保存即自动建父目录)。
/// HumanGate:human 可写(落人工账);anonymous 403(拒绝留痕)。
/// 持树写互斥锁串行执行。
/// 错误 → 403(anonymous)/ 409(已存在)/ 400(越界 / 保留名 / 非法字符 / kind 非法)。
pub async fn create_file(
    State(state): State<AgentApiState>,
    Json(body): Json<CreateBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let identity = gate_write(&state, "file_create", &body.path)?;
    let _guard = tree_mutation_lock().lock().await;
    let tool = FileCreateTool::new(state.workdir().to_path_buf()).with_writable_dir(".");
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), Value::from(body.path.as_str()));
    if let Some(kind) = &body.kind {
        args.insert("kind".to_string(), Value::from(kind.as_str()));
    }
    args.insert("create_parents".to_string(), Value::Bool(true));
    let result = tool
        .call(&Value::Object(args))
        .await
        .map_err(|e| (err_status(&e), e));
    record_outcome(&state, "file_create", identity, &body.path, &result);
    result.map(Json)
}

/// `POST /api/files/move` —— 移动/重命名(委托 file_move 实现,`writable_dir="."`)
///
/// 源与目标目录都必须已存在;移入自身子树被拒。
/// HumanGate:human 可写(落人工账);anonymous 403(拒绝留痕)。
/// 持树写互斥锁串行执行。
/// 错误 → 403(anonymous)/ 404(源或目标目录不存在)/ 400(越界 / 保留名 / 移入自身子树)。
pub async fn move_file(
    State(state): State<AgentApiState>,
    Json(body): Json<MoveBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let identity = gate_write(&state, "file_move", &body.path)?;
    let _guard = tree_mutation_lock().lock().await;
    let tool = FileMoveTool::new(state.workdir().to_path_buf()).with_writable_dir(".");
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), Value::from(body.path.as_str()));
    args.insert(
        "target_dir".to_string(),
        Value::from(body.target_dir.as_str()),
    );
    if let Some(new_name) = &body.new_name {
        args.insert("new_name".to_string(), Value::from(new_name.as_str()));
    }
    let result = tool
        .call(&Value::Object(args))
        .await
        .map_err(|e| (err_status(&e), e));
    record_outcome(&state, "file_move", identity, &body.path, &result);
    result.map(Json)
}

/// `DELETE /api/files` —— 删除(软删除进 `.evo-trash`;委托 file_delete 实现)
///
/// workdir 根 / writable 根 / 回收目录自身三类守护目标拒绝。
/// HumanGate:human 可写(落人工账);anonymous 403(拒绝留痕)。
/// 持树写互斥锁串行执行。
/// 错误 → 403(anonymous)/ 404(目标不存在)/ 400(越界 / 守护目标)。
pub async fn delete_file(
    State(state): State<AgentApiState>,
    Query(q): Query<DeleteQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let identity = gate_write(&state, "file_delete", &q.path)?;
    let _guard = tree_mutation_lock().lock().await;
    let tool = FileDeleteTool::new(state.workdir().to_path_buf()).with_writable_dir(".");
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), Value::from(q.path.as_str()));
    let result = tool
        .call(&Value::Object(args))
        .await
        .map_err(|e| (err_status(&e), e));
    record_outcome(&state, "file_delete", identity, &q.path, &result);
    result.map(Json)
}

/// `POST /api/files/search` —— 全项目内容搜索(委托 grep_core 核心层)
///
/// 请求体与 agent grep_files 工具 args 同形(query/isRegex/caseSensitive/
/// wholeWord/smartCase/dir/includeGlobs/excludeGlobs/useIgnoreFiles/maxResults,
/// camelCase/snake_case 双认);不委托 union toolkit 实例——工具实例把
/// maxResults 钳到 DEFAULT_MAX_RESULTS,而 REST 面 maxResults 上限是
/// 20 000(设置键 search.maxResults 同域)。
/// 30s 硬超时返回已收集 partial(truncated+timedOut 标注)。
/// 错误 → 404(dir 不存在)/ 400(非法正则 / 非法 glob / 参数非法 / 越界)。
pub async fn search_files(
    State(state): State<AgentApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let params = crate::builtin_tools::grep_files::GrepParams::from_args(&body)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let workdir = state.workdir().to_path_buf();
    // G13:核心层为同步 fs 遍历,spawn_blocking 包装
    let v = tokio::task::spawn_blocking(move || {
        crate::builtin_tools::grep_files::grep_core(&workdir, &params, None)
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("search task failed: {e}"),
        )
    })?
    .map_err(|e| (err_status(&e), e))?;
    Ok(Json(v))
}

/// `POST /api/files/replace` —— 全局搜索替换(委托 replace_core 核心层)
///
/// 请求体=搜索参数 + `replacement`(必填)+ `apply`(缺省 false)+
/// `paths`(限定替换文件集,「按所选文件替换」)。
/// `apply=false` → 预览 `{preview:[{path,edits:[{line,before,after}]}],fileCount,matchCount}`,
/// 零写盘;`apply=true` → **重新匹配**(不信任预览快照)后逐文件 tmp+rename
/// 原子写,返回 `{appliedFiles,appliedMatches,failed:[{path,reason}]}`。
/// 治理:人的 UI 写操作,不进 agent 审计链;误操作防护=强制预览+按文件应用+
/// failed 留痕(设计 §四);树写互斥由 replace_core 内部持有。
/// HumanGate:`apply=true` 属写面(多文件原子写)——human 可写(落人工账),
/// anonymous 403(拒绝留痕);`apply=false` 预览只读,不设门。
/// 错误 → 403(anonymous 写)/ 404(dir 不存在)/ 400(缺 replacement / 非法正则 / glob / 参数非法)。
pub async fn replace_files(
    State(state): State<AgentApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let params = crate::builtin_tools::grep_files::ReplaceParams::from_args(&body)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    // HumanGate:apply=true 写面准入;账目 path 字段落操作选择器 query
    // (本操作无单一路径语义;params 进 blocking 闭包前先取出)
    let query_selector = params.search.query.clone();
    if params.apply {
        gate_write(&state, "files_replace", &query_selector)?;
    }
    let workdir = state.workdir().to_path_buf();
    // G13:apply=true 在 replace_core 内部 blocking_lock 持树写锁,
    // 必须 spawn_blocking(禁 async 上下文直调);apply 快照供闭包后判定
    let apply = params.apply;
    let result = tokio::task::spawn_blocking(move || {
        crate::builtin_tools::grep_files::replace_core(&workdir, &params, None)
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("replace task failed: {e}"),
        )
    })?
    .map_err(|e| (err_status(&e), e));
    if apply {
        record_outcome(
            &state,
            "files_replace",
            CallerIdentity::Human,
            &query_selector,
            &result,
        );
    }
    result.map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin_tools::file_write::FileWriteTool;

    /// `writable_dir="."` 是本模块引入的新用法(IDE 人工编辑面 = workdir 全域)。
    /// 核心前提:workdir 根下的文件可写(join(".") 产生的 CurDir 组件会被
    /// `Path::components()` 规范化跳过,`starts_with` containment 判定不受影响)。
    #[tokio::test]
    async fn test_write_tool_writable_dir_dot_allows_workdir_root() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileWriteTool::new(dir.path().to_path_buf()).with_writable_dir(".");
        let result = tool
            .call(&serde_json::json!({
                "path": "root_file.txt",
                "content": "written by workbench",
                "overwrite": true
            }))
            .await;
        assert!(
            result.is_ok(),
            "root file write should pass, got: {:?}",
            result
        );
        let written = std::fs::read_to_string(dir.path().join("root_file.txt")).unwrap();
        assert_eq!(written, "written by workbench");
    }

    /// `writable_dir="."` 放宽到 workdir 全域,但沙箱边界(绝对路径 / `..` /
    /// symlink 逃逸校验)必须原样保留。
    #[tokio::test]
    async fn test_write_tool_writable_dir_dot_still_sandboxed() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileWriteTool::new(dir.path().to_path_buf()).with_writable_dir(".");
        let result = tool
            .call(&serde_json::json!({
                "path": "../escape.txt",
                "content": "x",
                "overwrite": true
            }))
            .await;
        assert!(result.is_err(), "parent-dir escape must still be rejected");
    }

    /// 覆盖写已存在文件(IDE 保存的最常见路径)在 `writable_dir="."` 下可用。
    #[tokio::test]
    async fn test_write_tool_writable_dir_dot_overwrite_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exists.txt"), b"old").unwrap();
        let tool = FileWriteTool::new(dir.path().to_path_buf()).with_writable_dir(".");
        let result = tool
            .call(&serde_json::json!({
                "path": "exists.txt",
                "content": "new",
                "overwrite": true
            }))
            .await;
        assert!(result.is_ok());
        let written = std::fs::read_to_string(dir.path().join("exists.txt")).unwrap();
        assert_eq!(written, "new");
    }

    /// 端点级冒烟:write_file handler 走真实 AgentApiState(workdir 指向 tempdir)。
    /// HumanGate:写面需 human 身份(state 注入 auth enabled;中间件层已验
    /// token 的语义由 handler 前置的 gate_write 承接)。
    #[tokio::test]
    async fn test_write_file_handler_writes_via_state_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = human_state(&dir_canon);
        let body = WriteBody {
            path: "notes/todo.md".to_string(),
            content: "- [ ] item".to_string(),
        };
        let resp = write_file(State(state), Json(body)).await;
        assert!(resp.is_ok(), "write should succeed, got: {:?}", resp.err());
        let written = std::fs::read_to_string(dir_canon.join("notes/todo.md")).unwrap();
        assert_eq!(written, "- [ ] item");
    }

    /// 构造 human 身份(写面放行)的 handler 级 AgentApiState
    fn human_state(dir_canon: &std::path::Path) -> AgentApiState {
        mutation_state(dir_canon)
    }

    /// 构造 anonymous 身份(auth disabled)的 handler 级 AgentApiState
    fn anonymous_state(dir_canon: &std::path::Path) -> AgentApiState {
        AgentApiState::new_with_metrics(
            crate::agent::definition::AgentDefinitionManager::new(dir_canon.join("agents")),
            crate::api::evorule_client::EvoruleApiClient::new("http://localhost:0"),
            std::sync::Arc::new(crate::api::metrics::Metrics::new().unwrap()),
            dir_canon.to_path_buf(),
            std::sync::Arc::new(crate::api::workspace_client::WorkspaceApiClient::new(
                "http://localhost:0",
            )),
            std::sync::Arc::new(crate::io_handlers::tool_handler::ToolHandler::new()),
        )
    }

    /// 构造指向 tempdir 的 handler 级 AgentApiState(写面测试缺省 human 态:
    /// auth enabled——增删改端点的既有行为断言全部以 human 身份走过 HumanGate)
    fn mutation_state(dir_canon: &std::path::Path) -> AgentApiState {
        anonymous_state(dir_canon).with_auth_config(crate::api::auth::AuthConfig::new(
            vec!["test-token".to_string()],
            true,
        ))
    }

    // =========================================================================
    // HumanGate(PR-4 两面):身份门 + 人工审计账
    // =========================================================================

    /// anonymous 真实拒绝路径:auth disabled 状态下调写面 → 403 + 开启指引,
    /// 文件未被触碰,拒绝本身落人工账。
    #[tokio::test]
    async fn test_anonymous_write_file_denied_403_with_ledger_record() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = anonymous_state(&dir_canon); // auth disabled = anonymous
        let resp = write_file(
            State(state),
            Json(WriteBody {
                path: "should-not-exist.txt".to_string(),
                content: "nope".to_string(),
            }),
        )
        .await;
        let (status, msg) = resp.expect_err("anonymous write must be denied");
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            msg.contains("--auth-token"),
            "403 body must carry enablement guidance, got: {msg}"
        );
        assert!(
            !dir_canon.join("should-not-exist.txt").exists(),
            "denied write must not touch the filesystem"
        );
        // 拒绝本身留痕
        let ledger = std::fs::read_to_string(dir_canon.join("data/human_gate_ledger.jsonl"))
            .expect("denial must be recorded");
        let entry: serde_json::Value =
            serde_json::from_str(ledger.lines().last().unwrap()).unwrap();
        assert_eq!(entry["op"], "file_write");
        assert_eq!(entry["identity"], "anonymous");
        assert_eq!(entry["outcome"], "denied");
        assert_eq!(entry["path"], "should-not-exist.txt");
    }

    /// anonymous 调 create/delete 同拒(写面四端点同一门,代表性覆盖两态)
    #[tokio::test]
    async fn test_anonymous_create_and_delete_denied_403() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::write(dir_canon.join("keep.txt"), b"x").unwrap();
        let state = anonymous_state(&dir_canon);
        let (status, _) = create_file(
            State(state.clone()),
            Json(CreateBody {
                path: "new.txt".to_string(),
                kind: None,
            }),
        )
        .await
        .expect_err("anonymous create must be denied");
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = delete_file(
            State(state),
            Query(DeleteQuery {
                path: "keep.txt".to_string(),
            }),
        )
        .await
        .expect_err("anonymous delete must be denied");
        assert_eq!(status, StatusCode::FORBIDDEN);
        // 只读面不受影响 + 文件未被触碰
        assert!(dir_canon.join("keep.txt").is_file());
        assert!(!dir_canon.join("new.txt").exists());
    }

    /// human 写面放行且操作落人工账(outcome=ok)
    #[tokio::test]
    async fn test_human_write_recorded_in_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = human_state(&dir_canon);
        let resp = create_file(
            State(state),
            Json(CreateBody {
                path: "ledgertest/a.md".to_string(),
                kind: None,
            }),
        )
        .await;
        assert!(resp.is_ok(), "human create should succeed");
        let ledger = std::fs::read_to_string(dir_canon.join("data/human_gate_ledger.jsonl"))
            .expect("human write must be recorded");
        let entry: serde_json::Value =
            serde_json::from_str(ledger.lines().last().unwrap()).unwrap();
        assert_eq!(entry["op"], "file_create");
        assert_eq!(entry["identity"], "human");
        assert_eq!(entry["outcome"], "ok");
        assert_eq!(entry["path"], "ledgertest/a.md");
    }

    /// replace apply=true 属写面:anonymous 403;预览(apply=false)匿名可通
    #[tokio::test]
    async fn test_anonymous_replace_apply_denied_preview_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::write(dir_canon.join("doc.txt"), b"foo\n").unwrap();
        let state = anonymous_state(&dir_canon);
        // 预览只读:匿名放行
        let preview = replace_files(
            State(state.clone()),
            Json(serde_json::json!({ "query": "foo", "replacement": "bar" })),
        )
        .await;
        assert!(preview.is_ok(), "anonymous preview must be allowed");
        assert_eq!(
            std::fs::read_to_string(dir_canon.join("doc.txt")).unwrap(),
            "foo\n",
            "preview must not write"
        );
        // apply=true:匿名 403,文件未被改写
        let (status, _) = replace_files(
            State(state),
            Json(serde_json::json!({
                "query": "foo",
                "replacement": "bar",
                "apply": true
            })),
        )
        .await
        .expect_err("anonymous apply must be denied");
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            std::fs::read_to_string(dir_canon.join("doc.txt")).unwrap(),
            "foo\n"
        );
    }

    // =========================================================================
    // 增删改端点(PR2):create / move / delete handler 级集成测试
    // =========================================================================

    #[test]
    fn test_err_status_mapping() {
        assert_eq!(
            err_status("path does not exist or cannot resolve: 'x'"),
            StatusCode::NOT_FOUND
        );
        assert_eq!(err_status("already exists: 'x'"), StatusCode::CONFLICT);
        assert_eq!(
            err_status("absolute path not allowed: 'x'"),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(err_status("illegal character"), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_create_file_handler_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = create_file(
            State(state),
            Json(CreateBody {
                path: "notes/todo.md".to_string(),
                kind: None,
            }),
        )
        .await;
        let v = resp.expect("create should succeed");
        assert_eq!(v["created"], serde_json::json!(true));
        assert_eq!(v["kind"], serde_json::json!("file"));
        assert!(dir_canon.join("notes/todo.md").is_file());
    }

    #[tokio::test]
    async fn test_create_dir_handler_creates_dir() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = create_file(
            State(state),
            Json(CreateBody {
                path: "docs/guides".to_string(),
                kind: Some("dir".to_string()),
            }),
        )
        .await;
        let v = resp.expect("create dir should succeed");
        assert_eq!(v["kind"], serde_json::json!("dir"));
        assert!(dir_canon.join("docs/guides").is_dir());
    }

    #[tokio::test]
    async fn test_create_duplicate_returns_409() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let mk_body = || {
            Json(CreateBody {
                path: "dup.txt".to_string(),
                kind: None,
            })
        };
        let _ = create_file(State(state.clone()), mk_body()).await.unwrap();
        let second = create_file(State(state), mk_body()).await;
        let (status, msg) = second.expect_err("duplicate create must fail");
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(msg.contains("already exists"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_create_escape_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = create_file(
            State(state),
            Json(CreateBody {
                path: "../escape.txt".to_string(),
                kind: None,
            }),
        )
        .await;
        let (status, _) = resp.expect_err("parent-dir escape must be rejected");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_move_handler_moves_and_renames() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::create_dir(dir_canon.join("src")).unwrap();
        std::fs::create_dir(dir_canon.join("dst")).unwrap();
        std::fs::write(dir_canon.join("src/a.txt"), b"payload").unwrap();
        let state = mutation_state(&dir_canon);
        let resp = move_file(
            State(state),
            Json(MoveBody {
                path: "src/a.txt".to_string(),
                target_dir: "dst".to_string(),
                new_name: Some("b.txt".to_string()),
            }),
        )
        .await;
        let v = resp.expect("move should succeed");
        assert_eq!(v["name"], serde_json::json!("b.txt"));
        assert!(!dir_canon.join("src/a.txt").exists());
        let moved = std::fs::read_to_string(dir_canon.join("dst/b.txt")).unwrap();
        assert_eq!(moved, "payload");
    }

    #[tokio::test]
    async fn test_move_missing_source_returns_404() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::create_dir(dir_canon.join("dst")).unwrap();
        let state = mutation_state(&dir_canon);
        let resp = move_file(
            State(state),
            Json(MoveBody {
                path: "nope.txt".to_string(),
                target_dir: "dst".to_string(),
                new_name: None,
            }),
        )
        .await;
        let (status, msg) = resp.expect_err("missing source must fail");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(msg.contains("does not exist"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_delete_handler_soft_deletes_into_trash() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::write(dir_canon.join("gone.txt"), b"bye").unwrap();
        let state = mutation_state(&dir_canon);
        let resp = delete_file(
            State(state),
            Query(DeleteQuery {
                path: "gone.txt".to_string(),
            }),
        )
        .await;
        let v = resp.expect("delete should succeed");
        assert!(
            !dir_canon.join("gone.txt").exists(),
            "original must be gone"
        );
        let trash_path = v["trash_path"].as_str().expect("trash_path in response");
        let restored = std::fs::read_to_string(trash_path).unwrap();
        assert_eq!(restored, "bye", "content must be preserved in trash");
    }

    #[tokio::test]
    async fn test_delete_missing_returns_404() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = delete_file(
            State(state),
            Query(DeleteQuery {
                path: "nope.txt".to_string(),
            }),
        )
        .await;
        let (status, _) = resp.expect_err("missing target must fail");
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_delete_guarded_root_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = delete_file(
            State(state),
            Query(DeleteQuery {
                path: ".".to_string(),
            }),
        )
        .await;
        let (status, _) = resp.expect_err("workdir root must be guarded");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    // =========================================================================
    // 搜索/替换端点(B2-PR3):search / replace handler 级集成测试
    // =========================================================================

    #[tokio::test]
    async fn test_search_handler_returns_groups() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::write(dir_canon.join("code.txt"), "needle here\nnothing\n").unwrap();
        let state = mutation_state(&dir_canon);
        let resp = search_files(
            State(state),
            Json(serde_json::json!({ "query": "needle", "smartCase": false })),
        )
        .await;
        let v = resp.expect("search should succeed");
        assert_eq!(v["totalMatches"], serde_json::json!(1));
        assert_eq!(v["fileCount"], serde_json::json!(1));
        assert_eq!(v["groups"][0]["path"], serde_json::json!("code.txt"));
        assert_eq!(v["groups"][0]["hits"][0]["line"], serde_json::json!(1));
        assert_eq!(v["groups"][0]["hits"][0]["col"], serde_json::json!(0));
    }

    #[tokio::test]
    async fn test_search_invalid_regex_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = search_files(
            State(state),
            Json(serde_json::json!({ "query": "(", "isRegex": true })),
        )
        .await;
        let (status, msg) = resp.expect_err("invalid regex must fail");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("invalid regex"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_search_missing_dir_returns_404() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = search_files(
            State(state),
            Json(serde_json::json!({ "query": "x", "dir": "nope" })),
        )
        .await;
        let (status, _) = resp.expect_err("missing dir must fail");
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_search_escape_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = search_files(
            State(state),
            Json(serde_json::json!({ "query": "x", "dir": "../outside" })),
        )
        .await;
        let (status, _) = resp.expect_err("parent-dir escape must be rejected");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_replace_preview_handler_no_writes() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::write(dir_canon.join("a.txt"), b"foo bar\n").unwrap();
        let state = mutation_state(&dir_canon);
        let resp = replace_files(
            State(state),
            Json(serde_json::json!({
                "query": "foo",
                "replacement": "baz",
                "smartCase": false
            })),
        )
        .await;
        let v = resp.expect("preview should succeed");
        assert_eq!(v["matchCount"], serde_json::json!(1));
        assert_eq!(
            v["preview"][0]["edits"][0]["before"],
            serde_json::json!("foo")
        );
        assert_eq!(
            v["preview"][0]["edits"][0]["after"],
            serde_json::json!("baz")
        );
        assert_eq!(
            std::fs::read_to_string(dir_canon.join("a.txt")).unwrap(),
            "foo bar\n",
            "preview must not write"
        );
    }

    #[tokio::test]
    async fn test_replace_apply_handler_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::write(dir_canon.join("a.txt"), b"foo bar\n").unwrap();
        let state = mutation_state(&dir_canon);
        let resp = replace_files(
            State(state),
            Json(serde_json::json!({
                "query": "foo",
                "replacement": "baz",
                "smartCase": false,
                "apply": true
            })),
        )
        .await;
        let v = resp.expect("apply should succeed");
        assert_eq!(v["appliedFiles"], serde_json::json!(1));
        assert_eq!(v["appliedMatches"], serde_json::json!(1));
        assert_eq!(v["failed"].as_array().unwrap().len(), 0);
        assert_eq!(
            std::fs::read_to_string(dir_canon.join("a.txt")).unwrap(),
            "baz bar\n"
        );
    }

    #[tokio::test]
    async fn test_replace_missing_replacement_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = replace_files(State(state), Json(serde_json::json!({ "query": "x" }))).await;
        let (status, msg) = resp.expect_err("missing replacement must fail");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("replacement"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_replace_invalid_regex_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = replace_files(
            State(state),
            Json(serde_json::json!({ "query": "(", "isRegex": true, "replacement": "x" })),
        )
        .await;
        let (status, msg) = resp.expect_err("invalid regex must fail");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("invalid regex"), "got: {msg}");
    }
}
