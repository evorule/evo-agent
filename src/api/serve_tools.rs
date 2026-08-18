// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! E1:serve 模式工具组装 —— union toolkit + 按白名单过滤
//!
//! serve 模式下 `cmd_serve` 在启动时调用 [`build_union_toolkit`] 一次,组装
//! 内置 6 + 规则 20 = 26 个工具的 union toolkit,存入 `AgentApiState.toolkit`。
//!
//! 每次 `/agents/{type}/run` 请求时,handler 调用 [`build_filtered_toolkit`]
//! 按 `def.tools` 白名单从 union 中过滤出该 agent 可用的工具,实现安全隔离。

use std::path::Path;

use crate::api::evorule_client::EvoruleApiClient;
use crate::api::workspace_client::WorkspaceApiClient;
use crate::builtin_tools::default_safe_toolkit;
use crate::io_handlers::tool_handler::ToolHandler;
use crate::rule_tools::full_rule_toolkit;

/// union toolkit 中包含的全部规则工具名(20 个)
///
/// workspace 2 + rule 12 + translate 3 + audit 3 = 20
const RULE_TOOL_NAMES: &[&str] = &[
    // workspace_tools (2)
    "ws_list",
    "ws_create",
    // rule_tools (12)
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
    // translate_tools (3)
    "rule_to_transform",
    "rule_to_conditional",
    "rule_validate",
    // audit_tools (3)
    "audit_get",
    "audit_verify",
    "session_rewind",
];

/// 构建 union toolkit(内置 6 + 规则 20 = 26 工具,启动时一次组装)
///
/// 在 `cmd_serve` 启动时调用一次,结果存入 `AgentApiState.toolkit`。
pub fn build_union_toolkit(
    workdir: &Path,
    ws: &WorkspaceApiClient,
    ev: &EvoruleApiClient,
) -> ToolHandler {
    let mut handler = default_safe_toolkit(workdir);
    let rule_handler = full_rule_toolkit(ws, ev);
    // 合并规则工具到 handler(按白名单逐个取出,保证只注册已知工具)
    for name in RULE_TOOL_NAMES {
        if let Some(tool) = rule_handler.get_tool(name) {
            handler.register_tool(name, tool);
        }
    }
    handler
}

/// 按 agent 白名单过滤 toolkit(serve 模式安全隔离)
///
/// 从 union toolkit 中只取出 `whitelist` 中列出的工具,构造一个新的
/// `ToolHandler`。未注册的工具名会被静默跳过(防御性:agent.json 写了
/// 不存在的工具名不应 500,`from_definition` 会兜底报错)。
pub fn build_filtered_toolkit(union: &ToolHandler, whitelist: &[String]) -> ToolHandler {
    let mut filtered = ToolHandler::new();
    for name in whitelist {
        if let Some(tool) = union.get_tool(name) {
            filtered.register_tool(name, tool);
        }
    }
    filtered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_clients() -> (WorkspaceApiClient, EvoruleApiClient) {
        (
            WorkspaceApiClient::new("http://localhost:0"),
            EvoruleApiClient::new("http://localhost:0"),
        )
    }

    #[test]
    fn test_build_union_toolkit_registers_26_tools() {
        let (ws, ev) = make_clients();
        let handler = build_union_toolkit(Path::new("."), &ws, &ev);

        // 6 个内置工具
        for name in [
            "file_read",
            "file_list",
            "file_write",
            "search_files",
            "shell_exec",
            "http_get",
        ] {
            assert!(
                handler.has_tool(name),
                "builtin tool {} should be registered",
                name
            );
        }

        // 20 个规则工具
        for name in RULE_TOOL_NAMES {
            assert!(
                handler.has_tool(name),
                "rule tool {} should be registered",
                name
            );
        }

        // 总数 = 6 + 20 = 26(逐个验证所有预期工具都在)
        let all_names: Vec<&str> = [
            "file_read",
            "file_list",
            "file_write",
            "search_files",
            "shell_exec",
            "http_get",
        ]
        .iter()
        .copied()
        .chain(RULE_TOOL_NAMES.iter().copied())
        .collect();
        assert_eq!(all_names.len(), 26, "expected 26 total tool names");
        for name in &all_names {
            assert!(
                handler.has_tool(name),
                "tool {} missing from union toolkit",
                name
            );
        }
    }

    #[test]
    fn test_build_filtered_toolkit_whitelist_subset() {
        let (ws, ev) = make_clients();
        let union = build_union_toolkit(Path::new("."), &ws, &ev);

        // 只取 3 个工具的白名单
        let whitelist: Vec<String> = vec![
            "file_read".to_string(),
            "rule_list".to_string(),
            "audit_get".to_string(),
        ];
        let filtered = build_filtered_toolkit(&union, &whitelist);

        assert!(filtered.has_tool("file_read"));
        assert!(filtered.has_tool("rule_list"));
        assert!(filtered.has_tool("audit_get"));
        // 白名单外的工具不应存在
        assert!(!filtered.has_tool("file_write"));
        assert!(!filtered.has_tool("shell_exec"));
        assert!(!filtered.has_tool("ws_create"));
    }

    #[test]
    fn test_build_filtered_toolkit_unknown_name_skipped() {
        let (ws, ev) = make_clients();
        let union = build_union_toolkit(Path::new("."), &ws, &ev);

        // 白名单含不存在的工具名 —— 应被静默跳过,不 panic
        let whitelist: Vec<String> = vec!["file_read".to_string(), "nonexistent_tool".to_string()];
        let filtered = build_filtered_toolkit(&union, &whitelist);

        assert!(filtered.has_tool("file_read"));
        assert!(!filtered.has_tool("nonexistent_tool"));
    }

    #[test]
    fn test_build_filtered_toolkit_empty_whitelist() {
        let (ws, ev) = make_clients();
        let union = build_union_toolkit(Path::new("."), &ws, &ev);

        let filtered = build_filtered_toolkit(&union, &[]);
        // 空白名单 → 空 toolkit(所有工具都不存在)
        assert!(!filtered.has_tool("file_read"));
        assert!(!filtered.has_tool("rule_list"));
    }

    #[test]
    fn test_get_tool_returns_some_for_registered() {
        let (ws, ev) = make_clients();
        let union = build_union_toolkit(Path::new("."), &ws, &ev);

        assert!(union.get_tool("file_read").is_some());
        assert!(union.get_tool("rule_list").is_some());
    }

    #[test]
    fn test_get_tool_returns_none_for_unregistered() {
        let (ws, ev) = make_clients();
        let union = build_union_toolkit(Path::new("."), &ws, &ev);

        assert!(union.get_tool("nonexistent").is_none());
    }
}
