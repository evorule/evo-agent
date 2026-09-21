// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! L2 约束（元规则）只读消费面（1 个工具）
//!
//! - meta_summary：查询当前 L2 约束清单摘要（GET /api/rules/l2-inventory，只读）；
//! - [`render_l2_inventory_summary`]：清单 → 人类可读摘要的渲染函数，
//!   meta_summary 工具与前馈注入（serve 三路径 system_prompt 追加）共用同一模板源。
//!
//! 边界纪律：本模块只做 L2 摘要的**读取与陈述**（展示层），不触碰 L2 引擎面
//! （不写 rules_dir、不经治理链、不改变任何门禁行为）。

use std::sync::Arc;

use serde_json::Value;

use crate::api::evorule_client::EvoruleApiClient;
use crate::builtin_tools::ToolSpec;
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};

/// 无 L2 约束时的明示文本（工具面不返回空串）
pub const NO_L2_TEXT: &str = "当前无 L2 约束规则。";

/// 渲染 L2 约束清单摘要（前馈注入与 meta_summary 工具共用的单一模板源）
///
/// 输入为 `GET /api/rules/l2-inventory` 响应（`{count, files:[{path,title,guard_for}]}`）。
/// 返回 `None` = 空清单（无 L2），调用方按语义处理（工具返回明示文本；前馈不注入）。
pub fn render_l2_inventory_summary(inv: &Value) -> Option<String> {
    let count = inv.get("count").and_then(|c| c.as_u64()).unwrap_or(0);
    let files = inv.get("files").and_then(|f| f.as_array())?;
    if count == 0 || files.is_empty() {
        return None;
    }
    let mut out = String::new();
    out.push_str("【L2 约束边界（生成规则草稿前必读）】\n");
    out.push_str(
        "你生成的业务规则不得修改或绕过以下守卫语义；关键动作必须以守卫标记为前置条件。\n",
    );
    out.push_str("禁项：\n");
    out.push_str(
        "- 禁止在业务规则中声明约束层层级标记（metadata.tier=\"constraint\" 或旧值 \"meta\"）冒充约束层——层级门禁会拒载该文件；\n",
    );
    out.push_str("- 禁止写入守卫保留的 metadata 保留域（保留域拒绝写入）；\n");
    out.push_str(
        "- 禁止在业务规则中携带引擎级强制原语 enforce——该原语仅随治理链晋升的约束层下发，业务规则携带会在导入期拒载；如需强制约束，请走治理链晋升流程。\n",
    );
    out.push_str(
        "路径读写约定：set 的 attr 相对 payload 解析（不带前缀）；domain 的 path 相对执行根解析（读取 payload 须带 payload. 前缀）。\n",
    );
    out.push_str("当前生效的约束规则清单：\n");
    for f in files {
        let path = f.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let title = f.get("title").and_then(|t| t.as_str()).unwrap_or("");
        let guards = f
            .get("guard_for")
            .and_then(|g| g.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        if guards.is_empty() {
            out.push_str(&format!("- {path}: {title}\n"));
        } else {
            out.push_str(&format!("- {path}: {title}（守卫指令类型: {guards}）\n"));
        }
    }
    Some(out)
}

// =============================================================================
// meta_summary —— L2 约束清单摘要
// =============================================================================

#[derive(Clone)]
pub struct MetaSummaryTool {
    client: EvoruleApiClient,
}

impl MetaSummaryTool {
    pub fn new(client: EvoruleApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for MetaSummaryTool {
    async fn call(&self, _args: &Value) -> IoResult {
        let inv = self
            .client
            .get_l2_inventory()
            .await
            .map_err(|e| e.to_string())?;
        Ok(Value::String(
            render_l2_inventory_summary(&inv).unwrap_or_else(|| NO_L2_TEXT.to_string()),
        ))
    }
}

// =============================================================================
// register / specs
// =============================================================================

pub fn register(h: &mut ToolHandler, client: &EvoruleApiClient) {
    h.register_tool(
        "meta_summary",
        Arc::new(MetaSummaryTool::new(client.clone())),
    );
}

pub fn specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "meta_summary".to_string(),
        description: "Summarize the currently effective L2 constraint (meta) rules — the \
                      guard boundaries a rule draft must respect (read-only)."
            .to_string(),
        parameters: vec![],
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_client() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:0")
    }

    fn sample_inventory() -> Value {
        serde_json::json!({
            "count": 1,
            "files": [
                {
                    "path": "00_constraint_seed.json",
                    "title": "种子元规则：运动安全哨兵",
                    "guard_for": ["validate_precision", "robot_move"]
                }
            ]
        })
    }

    #[test]
    fn test_render_contains_guard_line_and_boundary_text() {
        let text = render_l2_inventory_summary(&sample_inventory()).expect("非空清单应渲染");
        assert!(text.contains("00_constraint_seed.json"));
        assert!(text.contains("种子元规则"));
        assert!(text.contains("validate_precision, robot_move"));
        // 边界声明 + 禁项 + 路径约定四段齐全
        assert!(text.contains("不得修改或绕过"));
        assert!(text.contains("enforce"));
        assert!(text.contains("治理链晋升"));
        assert!(text.contains("payload"));
    }

    #[test]
    fn test_render_empty_inventory_returns_none() {
        let empty = serde_json::json!({"count": 0, "files": []});
        assert!(render_l2_inventory_summary(&empty).is_none());
    }

    #[test]
    fn test_render_no_internal_numbering_tokens() {
        // 公开面纪律：模板文本禁内部编号字样（方案号/批次号模式）
        let text = render_l2_inventory_summary(&sample_inventory()).unwrap();
        for token in ["UV-", "P0", "P1", "P2", "W1", "W2", "W3", "号文", "批 "] {
            assert!(!text.contains(token), "模板文本不得含内部编号字样: {token}");
        }
    }

    #[test]
    fn test_specs_count() {
        assert_eq!(specs().len(), 1);
    }

    #[test]
    fn test_register_tool() {
        let mut h = ToolHandler::new();
        register(&mut h, &make_client());
        assert!(h.has_tool("meta_summary"));
    }

    #[tokio::test]
    async fn test_meta_summary_fail_soft_on_client_error() {
        // 端点不可达（localhost:0 连接失败）→ 工具返回 Err 文本，不 panic
        let tool = MetaSummaryTool::new(make_client());
        let args = Value::Object(serde_json::Map::new());
        let result = tool.call(&args).await;
        assert!(result.is_err(), "拉取失败应向调用方透出错误而非空结果");
    }
}
