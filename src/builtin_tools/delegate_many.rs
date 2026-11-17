// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `delegate_many` —— 批量并行子代理委托(P2 并行面;design-104 方案 A)。
//!
//! 把 [`DelegateContext::delegate_parallel_with_context`](crate::agent::delegate::DelegateContext::delegate_parallel_with_context)
//! (既有并发原语:信号量并发帽+递归死锁防线+join_all)暴露为 LLM 工具面——
//! 单回合发一批子任务,并发执行,逐项结果回喂。
//!
//! ## 语义(design-104 批准稿)
//!
//! - **EffectClass=Mutating 保持**:调度器视角=单调用串行(外层互斥);并发
//!   在工具内部 join_all 展开——不重分类,不进 ReadOnly 臂;
//! - **逐项 Result**:批内部分失败不整体失败——成功项回 `content`,失败项
//!   回 `error`,LLM 逐项消费(design-104 D2 批准语义);
//! - **并发帽**:ctx 既有 `max_concurrent` 信号量(缺省 None=批大小即并发数;
//!   设置面=后续批,design-104 D3);
//! - **注册面**:与 `delegate` 同——`with_delegate_context` 接线时注册;
//!   LLM 可见性由 agent.json `tools` 列表决定(design-104 D1 批准=缺省面,
//!   与 delegate 同暴露)。

use serde_json::Value;

use crate::agent::delegate::DelegateContext;
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// `delegate_many` 工具 —— 批量并行子代理委托
#[derive(Clone)]
pub struct DelegateManyTool {
    ctx: DelegateContext,
}

impl std::fmt::Debug for DelegateManyTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelegateManyTool")
            .field("current_depth", &self.ctx.current_depth)
            .finish()
    }
}

impl DelegateManyTool {
    /// 创建批量委托工具(`ctx` clone 共享限流/深度/定义管理)
    pub fn new(ctx: DelegateContext) -> Self {
        Self { ctx }
    }
}

/// 解析 tasks 数组为并行委托三元组(严格形态校验,索引化报错)
fn parse_tasks(args: &Value) -> Result<Vec<(String, String, Option<String>)>, String> {
    let tasks = args
        .get("tasks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            "missing required arg: tasks (array of {agent_type, task, context?})".to_string()
        })?;
    if tasks.is_empty() {
        return Err("tasks must be a non-empty array".to_string());
    }
    let mut parsed = Vec::with_capacity(tasks.len());
    for (i, t) in tasks.iter().enumerate() {
        let obj = t
            .as_object()
            .ok_or_else(|| format!("tasks[{i}] must be an object with agent_type and task"))?;
        let agent_type = obj
            .get("agent_type")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| format!("tasks[{i}]: missing or empty agent_type"))?;
        let task = obj
            .get("task")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| format!("tasks[{i}]: missing or empty task"))?;
        let context = obj.get("context").and_then(|v| v.as_str());
        parsed.push((
            agent_type.to_string(),
            task.to_string(),
            context.map(|c| c.to_string()),
        ));
    }
    Ok(parsed)
}

#[async_trait::async_trait]
impl ToolFunction for DelegateManyTool {
    async fn call(&self, args: &Value) -> IoResult {
        let parsed = parse_tasks(args)?;
        let agent_types: Vec<String> = parsed.iter().map(|(a, _, _)| a.clone()).collect();

        tracing::info!(
            batch_size = parsed.len(),
            depth = self.ctx.current_depth,
            "delegate_many invoked by LLM"
        );

        let results = self.ctx.delegate_parallel_with_context(parsed).await;

        let mut ok_count = 0usize;
        let mut error_count = 0usize;
        let items: Vec<Value> = results
            .into_iter()
            .zip(agent_types.iter())
            .map(|(r, agent_type)| match r {
                Ok(content) => {
                    ok_count += 1;
                    serde_json::json!({
                        "agent_type": agent_type,
                        "status": "ok",
                        "content": content,
                    })
                }
                Err(e) => {
                    error_count += 1;
                    serde_json::json!({
                        "agent_type": agent_type,
                        "status": "error",
                        "error": e,
                    })
                }
            })
            .collect();

        Ok(serde_json::json!({
            "results": items,
            "count": items.len(),
            "ok_count": ok_count,
            "error_count": error_count,
        }))
    }
}

/// G9+:构造 `delegate_many` 工具的 spec(给 LLM 看;与 `delegate` 同暴露面)
pub fn delegate_many_tool_spec() -> crate::builtin_tools::ToolSpec {
    crate::builtin_tools::ToolSpec {
        name: "delegate_many".to_string(),
        description: "Delegate a BATCH of sub-tasks to agent types and run them \
                      concurrently. Each task returns its own result entry; partial \
                      failures do not fail the batch (per-item status). Concurrency is \
                      governed by the delegation semaphore; recursive delegation from \
                      a sub-agent fails fast instead of deadlocking. Use for \
                      embarrassingly-parallel research/analysis fan-out; sequential \
                      dependent steps should use individual delegate calls."
            .to_string(),
        parameters: vec![crate::builtin_tools::ParameterSpec {
            name: "tasks".to_string(),
            r#type: "array".to_string(),
            description: "Non-empty array of {agent_type, task, context?} objects; \
                          results preserve task order."
                .to_string(),
            required: true,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool() -> DelegateManyTool {
        // 校验类测试在 ctx 执行前即拒(守卫先行),ctx 仅需可构造
        DelegateManyTool::new(test_ctx())
    }

    fn test_ctx() -> DelegateContext {
        let definitions = crate::agent::definition::AgentDefinitionManager::with_default_dir();
        let mut ctx = DelegateContext::new("parent", definitions, make_client());
        ctx.journal_dir = None;
        ctx
    }

    fn make_client() -> crate::api::evorule_client::EvoruleApiClient {
        crate::api::evorule_client::EvoruleApiClient::new("http://localhost:8080")
    }

    #[tokio::test]
    async fn missing_tasks_is_fail_visible() {
        let err = tool().call(&json!({})).await.unwrap_err();
        assert!(err.contains("missing required arg: tasks"), "got: {err}");
    }

    #[tokio::test]
    async fn empty_tasks_is_fail_visible() {
        let err = tool().call(&json!({"tasks": []})).await.unwrap_err();
        assert!(err.contains("non-empty"), "got: {err}");
    }

    #[tokio::test]
    async fn malformed_entry_is_indexed() {
        let err = tool()
            .call(&json!({"tasks": [{"agent_type": "researcher"}]}))
            .await
            .unwrap_err();
        assert!(err.contains("tasks[0]"), "got: {err}");
        let err = tool()
            .call(&json!({"tasks": ["not-an-object"]}))
            .await
            .unwrap_err();
        assert!(err.contains("must be an object"), "got: {err}");
    }

    #[tokio::test]
    async fn spec_carries_delegate_many_name() {
        let spec = delegate_many_tool_spec();
        assert_eq!(spec.name, "delegate_many");
        assert!(!spec.parameters.is_empty());
    }
}
