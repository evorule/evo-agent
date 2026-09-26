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
//!
//! ## 语义边界(与 agent 工具通道的差别,设计定稿留痕)
//!
//! - 本面服务的是**人**在工作台编辑器中的直接操作(打开 / 编辑 / 保存落盘)。
//!   人的 UI 操作不构造 agent 会话事实,因此**不进 agent 会话审计链**——
//!   审计链记录的是 agent 执行行为,把人的编辑操作伪造成 agent 事实反而污染
//!   审计真实性;
//! - agent 路径的 `file_write`(`writable_dir=workspace` + candidate 审批 +
//!   call_external 过引擎入审计链)**完全不受影响**,两个消费面互不干涉;
//! - 写面差异是主体差异的忠实反映:agent 只能写 `workspace/`(防误写源码),
//!   人(项目方)在本机本就有完整文件系统权限,workdir 沙箱对人是 UX 边界
//!   (防误操作出 workdir),不是安全边界。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::Value;

use crate::api::agent_api::AgentApiState;
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
    tool.call(&args).await.map_err(|e| (err_status(&e), e))
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
/// 错误 → 400(路径越界 / 内容过大)。
pub async fn write_file(
    State(state): State<AgentApiState>,
    Json(body): Json<WriteBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let tool = FileWriteTool::new(state.workdir().to_path_buf()).with_writable_dir(".");
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), Value::from(body.path.as_str()));
    args.insert("content".to_string(), Value::from(body.content.as_str()));
    args.insert("overwrite".to_string(), Value::Bool(true));
    args.insert("create_parents".to_string(), Value::Bool(true));
    let v = tool
        .call(&Value::Object(args))
        .await
        .map_err(|e| (err_status(&e), e))?;
    Ok(Json(v))
}

/// `POST /api/files/create` —— 创建文件/目录(委托 file_create 实现,`writable_dir="."`)
///
/// 人工面语义:`approved=true` 绕过 candidate 审批(REST 面无 agent 会话
/// 审批通道,审批对象是 agent 行为而非人的点击),`create_parents=true`
/// 与写面同语义(IDE 保存即自动建父目录)。持树写互斥锁串行执行。
/// 错误 → 409(已存在)/ 400(越界 / 保留名 / 非法字符 / kind 非法)。
pub async fn create_file(
    State(state): State<AgentApiState>,
    Json(body): Json<CreateBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let _guard = tree_mutation_lock().lock().await;
    let tool = FileCreateTool::new(state.workdir().to_path_buf()).with_writable_dir(".");
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), Value::from(body.path.as_str()));
    if let Some(kind) = &body.kind {
        args.insert("kind".to_string(), Value::from(kind.as_str()));
    }
    args.insert("create_parents".to_string(), Value::Bool(true));
    args.insert("approved".to_string(), Value::Bool(true));
    let v = tool
        .call(&Value::Object(args))
        .await
        .map_err(|e| (err_status(&e), e))?;
    Ok(Json(v))
}

/// `POST /api/files/move` —— 移动/重命名(委托 file_move 实现,`writable_dir="."`)
///
/// 源与目标目录都必须已存在;移入自身子树被拒。持树写互斥锁串行执行。
/// 错误 → 404(源或目标目录不存在)/ 400(越界 / 保留名 / 移入自身子树)。
pub async fn move_file(
    State(state): State<AgentApiState>,
    Json(body): Json<MoveBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
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
    args.insert("approved".to_string(), Value::Bool(true));
    let v = tool
        .call(&Value::Object(args))
        .await
        .map_err(|e| (err_status(&e), e))?;
    Ok(Json(v))
}

/// `DELETE /api/files` —— 删除(软删除进 `.evo-trash`;委托 file_delete 实现)
///
/// workdir 根 / writable 根 / 回收目录自身三类守护目标拒绝。
/// 持树写互斥锁串行执行。
/// 错误 → 404(目标不存在)/ 400(越界 / 守护目标)。
pub async fn delete_file(
    State(state): State<AgentApiState>,
    Query(q): Query<DeleteQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let _guard = tree_mutation_lock().lock().await;
    let tool = FileDeleteTool::new(state.workdir().to_path_buf()).with_writable_dir(".");
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), Value::from(q.path.as_str()));
    args.insert("approved".to_string(), Value::Bool(true));
    let v = tool
        .call(&Value::Object(args))
        .await
        .map_err(|e| (err_status(&e), e))?;
    Ok(Json(v))
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
    #[tokio::test]
    async fn test_write_file_handler_writes_via_state_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = AgentApiState::new_with_metrics(
            crate::agent::definition::AgentDefinitionManager::new(dir_canon.join("agents")),
            crate::api::evorule_client::EvoruleApiClient::new("http://localhost:0"),
            std::sync::Arc::new(crate::api::metrics::Metrics::new().unwrap()),
            dir_canon.clone(),
            std::sync::Arc::new(crate::api::workspace_client::WorkspaceApiClient::new(
                "http://localhost:0",
            )),
            std::sync::Arc::new(crate::io_handlers::tool_handler::ToolHandler::new()),
        );
        let body = WriteBody {
            path: "notes/todo.md".to_string(),
            content: "- [ ] item".to_string(),
        };
        let resp = write_file(State(state), Json(body)).await;
        assert!(resp.is_ok(), "write should succeed, got: {:?}", resp.err());
        let written = std::fs::read_to_string(dir_canon.join("notes/todo.md")).unwrap();
        assert_eq!(written, "- [ ] item");
    }

    // =========================================================================
    // 增删改端点(PR2):create / move / delete handler 级集成测试
    // =========================================================================

    /// 构造指向 tempdir 的 handler 级 AgentApiState(与上方 write 测试同型)
    fn mutation_state(dir_canon: &std::path::Path) -> AgentApiState {
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
}
