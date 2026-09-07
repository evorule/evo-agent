// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 规则管理工具集 —— 把 WorkspaceApiClient/EvoruleApiClient 封装成 ToolFunction 工具

pub mod audit_tools;
pub mod bundle_tools;
pub mod dataset_tools;
pub mod knowledge_tools;
pub mod production_tools;
pub mod publish_tools;
pub mod rule_crud;
pub mod sandbox_tools;
pub mod translate_tools;
pub mod workspace_tools;

use crate::api::evorule_client::EvoruleApiClient;
use crate::api::workspace_client::WorkspaceApiClient;
use crate::builtin_tools::ToolSpec;
use crate::io_handlers::tool_handler::ToolHandler;

/// 组装规则管理工具集（M1：workspace 2 + rule 12 + audit 3 = 17 工具）
pub fn rule_management_toolkit(ws: &WorkspaceApiClient, ev: &EvoruleApiClient) -> ToolHandler {
    let mut h = ToolHandler::new();
    workspace_tools::register(&mut h, ws);
    rule_crud::register(&mut h, ws);
    audit_tools::register(&mut h, ev);
    h
}

/// 组装完整规则工具集（M3：内置 20 + 沙盒/发布 14 + bundles/knowledge 8 = 42 工具）
pub fn full_rule_toolkit(ws: &WorkspaceApiClient, ev: &EvoruleApiClient) -> ToolHandler {
    let mut h = rule_management_toolkit(ws, ev);
    translate_tools::register(&mut h, ws);
    sandbox_tools::register(&mut h, ws);
    dataset_tools::register(&mut h, ws);
    publish_tools::register(&mut h, ws);
    production_tools::register(&mut h, ws);
    bundle_tools::register(&mut h, ws, ev);
    knowledge_tools::register(&mut h, ev);
    h
}

/// 全部规则工具 spec（42 个）
pub fn rule_tool_specs() -> Vec<ToolSpec> {
    let mut specs = Vec::new();
    specs.extend(workspace_tools::specs());
    specs.extend(rule_crud::specs());
    specs.extend(translate_tools::specs());
    specs.extend(audit_tools::specs());
    specs.extend(sandbox_tools::specs());
    specs.extend(dataset_tools::specs());
    specs.extend(publish_tools::specs());
    specs.extend(production_tools::specs());
    specs.extend(bundle_tools::specs());
    specs.extend(knowledge_tools::specs());
    specs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rule_tool_specs_count() {
        let specs = rule_tool_specs();
        assert_eq!(specs.len(), 42, "expected 42 rule tool specs");
    }

    #[test]
    fn test_rule_tool_specs_names() {
        let specs = rule_tool_specs();
        let names: std::collections::HashSet<&str> =
            specs.iter().map(|s| s.name.as_str()).collect();
        // D2 白名单一致性：rule-copilot.json 的 tools 必须是 spec 名称集合的子集
        let whitelist = [
            "ws_list",
            "ws_create",
            "rule_list",
            "rule_get",
            "rule_create",
            "rule_update",
            "rule_versions",
            "rule_version_get",
            "rule_submit",
            "rule_activate",
            "rule_block",
            "rule_archive",
            "rule_fork",
            "rule_to_transform",
            "rule_to_conditional",
            "rule_validate",
            "rule_reload",
            "audit_get",
            "audit_verify",
            "session_rewind",
        ];
        for name in &whitelist {
            assert!(
                names.contains(*name),
                "whitelist tool '{}' not in rule_tool_specs",
                name
            );
        }
    }

    #[test]
    fn test_rule_copilot_json_whitelist_subset_of_specs() {
        // 从 agents/rule-copilot.json 加载并验证白名单一致性
        let json_str = include_str!("../../agents/rule-copilot.json");
        let def: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let tools = def["tools"].as_array().unwrap();
        let spec_names: std::collections::HashSet<String> =
            rule_tool_specs().iter().map(|s| s.name.clone()).collect();
        for tool in tools {
            let name = tool.as_str().unwrap();
            assert!(
                spec_names.contains(name),
                "rule-copilot.json tool '{}' not in rule_tool_specs",
                name
            );
        }
        assert_eq!(tools.len(), 20, "expected 20 tools in rule-copilot.json");
    }

    #[test]
    fn test_rule_copilot_json_no_unsafe_tools() {
        // 确保白名单不包含文件/shell/http 等不安全工具
        let json_str = include_str!("../../agents/rule-copilot.json");
        let def: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let tools = def["tools"].as_array().unwrap();
        let unsafe_tools = [
            "file_read",
            "file_list",
            "file_write",
            "search_files",
            "shell_exec",
            "http_get",
        ];
        for tool in tools {
            let name = tool.as_str().unwrap();
            assert!(
                !unsafe_tools.contains(&name),
                "unsafe tool '{}' in rule-copilot whitelist",
                name
            );
        }
    }

    #[test]
    fn test_new_d4_tools_in_specs() {
        // D4 新增的 14 个工具名必须在 rule_tool_specs 中
        let specs = rule_tool_specs();
        let spec_names: std::collections::HashSet<&str> =
            specs.iter().map(|s| s.name.as_str()).collect();
        let d4_tools = [
            "sandbox_start",
            "sandbox_list",
            "sandbox_get",
            "sandbox_close",
            "sandbox_report",
            "dataset_create",
            "dataset_list",
            "publish_submit",
            "publish_list",
            "publish_queue_get",
            "publish_review",
            "publish_rollback",
            "prod_state",
            "prod_audit",
        ];
        for name in &d4_tools {
            assert!(
                spec_names.contains(name),
                "D4 tool '{}' not in rule_tool_specs",
                name
            );
        }
    }

    #[test]
    fn test_w2_bundle_knowledge_tools_in_specs() {
        // UV-084 W2 新增的 8 个工具（bundles 部署闭环 5 + knowledge 数据面 3）
        let specs = rule_tool_specs();
        let spec_names: std::collections::HashSet<&str> =
            specs.iter().map(|s| s.name.as_str()).collect();
        let w2_tools = [
            "bundle_export",
            "bundle_import_dry_run",
            "bundle_import",
            "bundle_active_list",
            "bundle_imports_list",
            "knowledge_datasets",
            "knowledge_search",
            "knowledge_entry_get",
        ];
        for name in &w2_tools {
            assert!(
                spec_names.contains(name),
                "W2 tool '{}' not in rule_tool_specs",
                name
            );
        }
    }

    #[test]
    fn test_full_rule_toolkit_registers_all_42() {
        // 验证 full_rule_toolkit 注册了全部 42 个工具（has_tool 逐个校验）
        let ws = WorkspaceApiClient::new("http://localhost:0");
        let ev = EvoruleApiClient::new("http://localhost:0");
        let h = full_rule_toolkit(&ws, &ev);
        let all_tools = [
            // workspace 2
            "ws_list",
            "ws_create",
            // rule 12
            "rule_list",
            "rule_get",
            "rule_create",
            "rule_update",
            "rule_versions",
            "rule_version_get",
            "rule_submit",
            "rule_activate",
            "rule_block",
            "rule_archive",
            "rule_fork",
            "rule_reload",
            // translate 3
            "rule_to_transform",
            "rule_to_conditional",
            "rule_validate",
            // audit 3
            "audit_get",
            "audit_verify",
            "session_rewind",
            // sandbox 5
            "sandbox_start",
            "sandbox_list",
            "sandbox_get",
            "sandbox_close",
            "sandbox_report",
            // dataset 2
            "dataset_create",
            "dataset_list",
            // publish 5
            "publish_submit",
            "publish_list",
            "publish_queue_get",
            "publish_review",
            "publish_rollback",
            // production 2
            "prod_state",
            "prod_audit",
            // bundles 5 (UV-084 W2)
            "bundle_export",
            "bundle_import_dry_run",
            "bundle_import",
            "bundle_active_list",
            "bundle_imports_list",
            // knowledge 3 (UV-084 W2)
            "knowledge_datasets",
            "knowledge_search",
            "knowledge_entry_get",
        ];
        assert_eq!(all_tools.len(), 42);
        for name in &all_tools {
            assert!(
                h.has_tool(name),
                "tool '{}' should be registered in full_rule_toolkit",
                name
            );
        }
    }
}
