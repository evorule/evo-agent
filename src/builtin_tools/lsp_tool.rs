// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `lsp` 内建工具:语言服务器诊断与文档符号查询(agent 侧自持客户端)。
//!
//! 只读面:查询诊断/符号,零写路径。模型只能选 action 与给文件路径——
//! 不存在模型可控命令面(服务器进程由语言门规格固定)。
//! server 不在 PATH = fail-visible,错误串携带安装指引。

#![forbid(unsafe_code)]

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::io_handler::IoResult;

use crate::io_handlers::tool_handler::ToolFunction;

/// 工具名(LLM 契约面;静态清单/锁三处同步)。
pub const LSP_TOOL: &str = "lsp";

/// 工具本体(工作区根锚定;路径参数相对解析)。
pub struct LspTool {
    workdir: PathBuf,
}

impl LspTool {
    /// 构造(workdir=装配工作区根)。
    pub fn new(workdir: PathBuf) -> Self {
        Self { workdir }
    }
}

#[async_trait::async_trait]
impl ToolFunction for LspTool {
    async fn call(&self, args: &Value) -> IoResult {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required string param: action".to_string())?;
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required string param: path".to_string())?;
        let wait_ms = args.get("wait_ms").and_then(|v| v.as_u64());
        match action {
            "diagnostics" => {
                let mut out =
                    crate::lsp::client::diagnostics_for_tool(&self.workdir, path, wait_ms).await?;
                if let Some(obj) = out.as_object_mut() {
                    obj.insert("action".to_string(), Value::from(action));
                    obj.insert("path".to_string(), Value::from(path));
                }
                Ok(out)
            }
            "document_symbols" => {
                let mut out =
                    crate::lsp::client::document_symbols_for_tool(&self.workdir, path).await?;
                if let Some(obj) = out.as_object_mut() {
                    obj.insert("action".to_string(), Value::from(action));
                    obj.insert("path".to_string(), Value::from(path));
                }
                Ok(out)
            }
            other => Err(format!(
                "unknown action '{other}' (supported: diagnostics | document_symbols)"
            )),
        }
    }
}

/// LLM 契约面参数 schema(与 call 判定同源;action 封闭枚举)。
pub fn lsp_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["diagnostics", "document_symbols"],
                "description": "Query kind: file diagnostics, or document symbol outline.",
            },
            "path": {
                "type": "string",
                "description": "File path relative to the workspace root \
                                (supported: .rs, .ts/.tsx/.jsx, .py/.pyi).",
            },
            "wait_ms": {
                "type": "number",
                "description": "Optional max wait for the first diagnostics publish \
                                (default 10000, cap 10000).",
            },
        },
        "required": ["action", "path"],
    })
}

/// LLM 契约面描述(英文;只读定位+失败形态如实声明)。
pub fn lsp_description() -> String {
    "Query language-server intelligence for a single file (read-only): \
     structured diagnostics (errors/warnings with precise ranges) after an \
     edit — much faster than running a full build for feedback — or the \
     document symbol outline. Supported languages: Rust (rust-analyzer), \
     TypeScript/JavaScript (typescript-language-server), Python (pyright). \
     The server must be on PATH; a missing server fails with install guidance. \
     First query in a session spawns and initializes the server (slower); \
     later queries reuse it. For whole-project compile errors use build_check."
        .to_string()
}

/// 参数形态自查(与 call 判定同源;schema 漂移执法锚)。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_requires_action_and_path() {
        let s = lsp_schema();
        let required = s["required"].as_array().unwrap();
        assert_eq!(required.len(), 2);
        assert_eq!(s["properties"]["action"]["enum"][0], "diagnostics");
        assert_eq!(s["properties"]["action"]["enum"][1], "document_symbols");
    }

    #[tokio::test]
    async fn unknown_action_fails_visible() {
        let tool = LspTool::new(std::env::temp_dir());
        let args = json!({"action": "hover", "path": "a.rs"});
        let err = tool.call(&args).await.unwrap_err();
        assert!(err.contains("unknown action"));
    }

    #[tokio::test]
    async fn missing_params_fail_visible() {
        let tool = LspTool::new(std::env::temp_dir());
        let err = tool.call(&json!({"path": "a.rs"})).await.unwrap_err();
        assert!(err.contains("action"));
        let err = tool
            .call(&json!({"action": "diagnostics"}))
            .await
            .unwrap_err();
        assert!(err.contains("path"));
    }

    #[tokio::test]
    async fn unsupported_extension_fails_visible() {
        let tool = LspTool::new(std::env::temp_dir());
        let args = json!({"action": "diagnostics", "path": "readme.md"});
        let err = tool.call(&args).await.unwrap_err();
        assert!(err.contains("unsupported language"));
    }
}
