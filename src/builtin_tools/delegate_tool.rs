// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! G9:`delegate` 工具 —— 让 LLM 在 ReAct 循环中调用子 agent
//!
//! ## 作用
//!
//! 把 [`DelegateContext::delegate`](crate::agent::delegate::DelegateContext::delegate)
//! 包装成 [`ToolFunction`],注册到 `ToolHandler`。这样 LLM 在 ReAct 循环中
//! 可以像调用 `file_read` / `http_get` 一样调用 `delegate`,把子任务委托给
//! 其他类型的 agent。
//!
//! ## 调用参数
//!
//! ```json
//! { "agent_type": "researcher", "task": "调研 Rust 异步生态" }
//! ```
//!
//! ## 返回
//!
//! - 成功:`{"status":"ok","agent_type":"researcher","content":"..."}`
//! - 失败:`Err("delegate to 'researcher' failed: ...")`
//!   (失败时返回 Err,与 `delegate()` 的 `Result<String, String>` 语义一致;
//!   runner 的并行工具路径会把 Err 转成 `{"status":"error","error":...}` JSON
//!   喂回 LLM,串行路径则触发 `AgentError::ToolError`)
//!
//! ## 暴露给 LLM
//!
//! 工具是否对 LLM 可见取决于 agent.json 的 `tools` 列表 + system prompt。
//! 注册到 `ToolHandler` 只提供**执行能力**:LLM 发出 `delegate` tool_call 时,
//! runner 能通过 `tool_handler.execute_by_name("delegate", ...)` 执行它。
//!
//! ## 接线
//!
//! 由 [`AgentRunner::with_delegate_context`](crate::agent::runner::AgentRunner::with_delegate_context)
//! 自动注册:设置 `delegate_context` 时,同步把 `DelegateTool` 注册进 `tool_handler`。

use evorule_tcb::JsonValue;

use crate::agent::delegate::DelegateContext;
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// `delegate` 工具 —— 在 ReAct 循环中委托子 agent
#[derive(Clone)]
pub struct DelegateTool {
    ctx: DelegateContext,
}

impl std::fmt::Debug for DelegateTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelegateTool")
            .field("current_depth", &self.ctx.current_depth)
            .field("max_depth", &self.ctx.max_depth)
            .finish()
    }
}

impl DelegateTool {
    /// 创建 delegate 工具
    ///
    /// `ctx` 会被 clone(共享 `Arc<Semaphore>` 限流计数 + `AgentDefinitionManager`)。
    pub fn new(ctx: DelegateContext) -> Self {
        Self { ctx }
    }

    /// 只读访问内部 `DelegateContext`
    pub fn delegate_context(&self) -> &DelegateContext {
        &self.ctx
    }
}

#[async_trait::async_trait]
impl ToolFunction for DelegateTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let agent_type = args
            .get("agent_type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: agent_type (string)".to_string())?;
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: task (string)".to_string())?;

        tracing::info!(
            agent_type,
            task = %task,
            depth = self.ctx.current_depth,
            "delegate tool invoked by LLM"
        );

        let result = self.ctx.delegate(agent_type, task).await;
        match result {
            Ok(content) => {
                let mut map = std::collections::BTreeMap::new();
                map.insert("status".to_string(), JsonValue::string("ok"));
                map.insert(
                    "agent_type".to_string(),
                    JsonValue::string(agent_type.to_string()),
                );
                map.insert("content".to_string(), JsonValue::string(content));
                Ok(JsonValue::object(map))
            }
            Err(e) => Err(format!("delegate to '{}' failed: {}", agent_type, e)),
        }
    }
}

/// G9:构造 `delegate` 工具的 spec(给 LLM 看)
///
/// 由需要支持子 agent 委托的 agent 在 `tools` 列表中声明 `"delegate"` 时使用。
pub fn delegate_tool_spec() -> crate::builtin_tools::ToolSpec {
    crate::builtin_tools::ToolSpec {
        name: "delegate".to_string(),
        description: "Delegate a sub-task to another agent type. \
                      Use this to break down complex tasks: e.g. delegate research to a 'researcher' \
                      agent, or writing to a 'writer' agent. The sub-agent runs its own ReAct loop \
                      and returns its final answer."
            .to_string(),
        parameters: vec![
            crate::builtin_tools::ParameterSpec {
                name: "agent_type".to_string(),
                r#type: "string".to_string(),
                description: "Type of the sub-agent to delegate to (must match an agent definition, \
                              e.g. \"researcher\", \"writer\")"
                    .to_string(),
                required: true,
            },
            crate::builtin_tools::ParameterSpec {
                name: "task".to_string(),
                r#type: "string".to_string(),
                description: "The task description to give to the sub-agent".to_string(),
                required: true,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::definition::AgentDefinitionManager;
    use crate::api::evorule_client::EvoruleApiClient;

    fn make_ctx() -> DelegateContext {
        let definitions = AgentDefinitionManager::with_default_dir();
        DelegateContext::new(
            "parent",
            definitions,
            EvoruleApiClient::new("http://localhost:8080"),
        )
    }

    #[tokio::test]
    async fn test_delegate_tool_missing_agent_type() {
        let tool = DelegateTool::new(make_ctx());
        let args = JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("task".to_string(), JsonValue::string("do thing"));
            m
        });
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("agent_type"));
    }

    #[tokio::test]
    async fn test_delegate_tool_missing_task() {
        let tool = DelegateTool::new(make_ctx());
        let args = JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("agent_type".to_string(), JsonValue::string("researcher"));
            m
        });
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("task"));
    }

    #[tokio::test]
    async fn test_delegate_tool_depth_exceeded_returns_err() {
        // max_depth=0 → 任何 delegate 都超限,无需实际 evorule 调用
        let ctx = make_ctx().with_max_depth(0);
        let tool = DelegateTool::new(ctx);
        let args = JsonValue::object({
            let mut m = std::collections::BTreeMap::new();
            m.insert("agent_type".to_string(), JsonValue::string("researcher"));
            m.insert("task".to_string(), JsonValue::string("anything"));
            m
        });
        let result = tool.call(&args).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("delegate to 'researcher' failed"));
        assert!(err.contains("max delegate depth exceeded"));
    }

    #[test]
    fn test_delegate_tool_spec() {
        let spec = delegate_tool_spec();
        assert_eq!(spec.name, "delegate");
        assert_eq!(spec.parameters.len(), 2);
        assert!(spec
            .parameters
            .iter()
            .any(|p| p.name == "agent_type" && p.required));
        assert!(spec
            .parameters
            .iter()
            .any(|p| p.name == "task" && p.required));
    }

    #[test]
    fn test_delegate_tool_clone_and_debug() {
        let tool = DelegateTool::new(make_ctx());
        let _cloned = tool.clone();
        let debug_str = format!("{:?}", tool);
        assert!(debug_str.contains("DelegateTool"));
    }
}
