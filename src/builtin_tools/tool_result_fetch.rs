// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `tool_result_fetch` 契约面占位执行器(schema-only stub)
//!
//! **执行真源在 runner**:[`AgentRunner::execute_tool_result_fetch`]
//! (runner.rs 保留名直拦,先于治理管道)——持有 journal_dir 与
//! tool_result_max_chars 会话态,是本工具的唯一真执行点。
//!
//! 本 stub 的存在意义仅为「注册必须 manifest+执行器成对在场」的工具面
//! 统一架构硬规则 1:把工具挂进 LLM 契约面。此前该工具不在任何
//! 工具集——超大结果被引用化后,引用块尾部指引「调用 tool_result_fetch
//! 工具按 ref_id 取回」,而 schema 里根本没有这个工具,schema 严格的模型
//! 会拒调,取回链对这类模型断裂(证据丢弃观感的来源)。
//!
//! 可达性:主管道(单独调用)在进入管道前被 runner 直拦,永远到不了这里;
//! 仅并行预取臂(管道并行实例)可能到达——返回 fail-visible 提示,模型
//! 单独重调即走真执行。只读、无副作用、无状态。

use serde_json::Value;

use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// 契约面占位执行器(无状态;真实取回逻辑在 runner 直拦路径,
/// 参数语义 ref_id/offset/max_chars 见 default_tool_specs 条目)
pub struct ToolResultFetchStub;

#[async_trait::async_trait]
impl ToolFunction for ToolResultFetchStub {
    async fn call(&self, _args: &Value) -> IoResult {
        Ok(serde_json::json!({
            "error": "tool_result_fetch is executed by the runner as a reserved name; \
                      re-issue it as a direct (non-parallel) tool call"
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_stub_fail_visible() {
        // 并行臂路径到达 stub:fail-visible 错误 JSON(模型据此单独重调)
        let out = ToolResultFetchStub
            .call(&serde_json::json!({"ref_id": "x"}))
            .await;
        let v = out.expect("stub returns Ok with error JSON");
        assert!(v.get("error").is_some(), "got: {v}");
    }
}
