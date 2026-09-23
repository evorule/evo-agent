// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 工作台文件 REST 面 —— IDE 文件树浏览与编辑器保存的消费端点
//!
//! 三个端点,全部复用 builtin_tools 的 file 工具实现(同一沙箱 / 同一校验 /
//! 同一资源限制),不在本模块重复任何路径安全逻辑:
//!
//! - `GET /api/files/list?dir=`  → [`FileListTool`](crate::builtin_tools::file_list::FileListTool)
//!   (union toolkit 内**同一实例**)
//! - `GET /api/files/read?path=` → [`FileReadTool`](crate::builtin_tools::file_read::FileReadTool)
//!   (union toolkit 内**同一实例**)
//! - `PUT /api/files/write`      → [`FileWriteTool`](crate::builtin_tools::file_write::FileWriteTool)
//!   (同实现,`writable_dir="."`:人工编辑面 = workdir 全域)
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
use crate::builtin_tools::file_write::FileWriteTool;
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

/// 工具错误字符串 → HTTP 状态码(不存在的路径 404,其余 400)
fn err_status(msg: &str) -> StatusCode {
    if msg.contains("does not exist") {
        StatusCode::NOT_FOUND
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
}
