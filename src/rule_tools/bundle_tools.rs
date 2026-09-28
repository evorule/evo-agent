// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! bundles 部署闭环工具（本地逻辑部分）：bundle_export
//!
//! 透传族 bundle_import_dry_run / bundle_import / bundle_active_list /
//! bundle_imports_list 已迁入 adapter 表驱动（rule_tools::adapter）；本文件仅
//! 保留带本地证据校验的导出工具。
//!
//! 部署闭环链路：
//!
//! ```text
//! bundle_export(治理域 :18081) → bundle_import_dry_run(执行域预检)
//!   → bundle_import(执行域落盘+reload+激活) → bundle_active_list(确认)
//!   → bundle_imports_list(溯源)
//! ```
//!
//! 错误纪律：走 check_response_full——校验失败(400)的 {"error": "..."}
//! 详情透出，LLM agent 可自诊断修复（不静默）。

use std::sync::Arc;

use serde_json::Value;

use crate::api::workspace_client::WorkspaceApiClient;
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};

// =============================================================================
// bundle_export —— 治理域带证据导出（闭环上游）
// =============================================================================

#[derive(Clone)]
pub struct BundleExportTool {
    client: WorkspaceApiClient,
}

impl BundleExportTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for BundleExportTool {
    async fn call(&self, args: &Value) -> IoResult {
        let dataset_id = args
            .get("dataset_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: dataset_id".to_string())?;
        let version = args
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: version".to_string())?;
        let verdict = args
            .get("verdict")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: verdict".to_string())?;
        if verdict != "pass" && verdict != "fail" {
            return Err("verdict must be \"pass\" or \"fail\"".to_string());
        }
        // 前置形状校验（与治理域 回归验证 B1 同口径，提前拦截省一次往返）：
        // verdict=pass 必带可追溯标记，防零证据 pass 导出
        let subset: Vec<String> = args
            .get("subset")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if verdict == "pass"
            && (subset.is_empty()
                || !subset
                    .iter()
                    .all(|s| s.starts_with("sandbox:") || s.starts_with("human:")))
        {
            return Err(
                "verdict=pass requires traceable evidence: subset must be non-empty and each \
                 item must start with \"sandbox:<id>\" or \"human:<actor>\""
                    .to_string(),
            );
        }
        let trim = args.get("trim").and_then(|v| v.as_str());
        let result = self
            .client
            .export_bundle(dataset_id, version, verdict, subset, trim)
            .await
            .map_err(|e| e.to_string())?;
        Ok(result.clone())
    }
}

// =============================================================================
// register / specs
// =============================================================================

pub fn register(h: &mut ToolHandler, client: &WorkspaceApiClient) {
    h.register_tool(
        "bundle_export",
        Arc::new(BundleExportTool::new(client.clone())),
    );
}

pub fn specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "bundle_export".to_string(),
        description: "Export a dataset bundle with sandbox test evidence from the \
                      governance domain (POST /bundles/export). Returns the DatasetBundle \
                      JSON to pass to bundle_import_dry_run / bundle_import."
            .to_string(),
        parameters: vec![
            ParameterSpec {
                name: "dataset_id".to_string(),
                r#type: "string".to_string(),
                description: "Dataset id to export.".to_string(),
                required: true,
            },
            ParameterSpec {
                name: "version".to_string(),
                r#type: "string".to_string(),
                description: "Dataset version to export (current version uses live \
                              entries; historical versions rebuilt from snapshots)."
                    .to_string(),
                required: true,
            },
            ParameterSpec {
                name: "verdict".to_string(),
                r#type: "string".to_string(),
                description: "Test verdict: \"pass\" (requires traceable subset) or \
                              \"fail\" (explicit unverified export)."
                    .to_string(),
                required: true,
            },
            ParameterSpec {
                name: "subset".to_string(),
                r#type: "array".to_string(),
                description: "Traceable test evidence refs (array of strings). Required \
                              for verdict=pass: each item must be \"sandbox:<id>\" \
                              (machine attestation) or \"human:<actor>\" (explicit \
                              human downgrade)."
                    .to_string(),
                required: false,
            },
            ParameterSpec {
                name: "trim".to_string(),
                r#type: "string".to_string(),
                description: "Optional trim-view syntax: \"tag:core\" / \"domain:tax\" / \
                              \"ids:id1,id2\" (multiple segments joined by \";\", \
                              intersection)."
                    .to_string(),
                required: false,
            },
        ],
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ws() -> WorkspaceApiClient {
        WorkspaceApiClient::new("http://localhost:0")
    }

    #[test]
    fn test_specs_count() {
        assert_eq!(specs().len(), 1);
    }

    #[test]
    fn test_register_tools() {
        let mut h = ToolHandler::new();
        register(&mut h, &make_ws());
        assert!(h.has_tool("bundle_export"));
    }

    #[tokio::test]
    async fn test_bundle_export_missing_dataset_id() {
        let tool = BundleExportTool::new(make_ws());
        let args = Value::Object(serde_json::Map::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: dataset_id"));
    }

    #[tokio::test]
    async fn test_bundle_export_invalid_verdict() {
        let tool = BundleExportTool::new(make_ws());
        let mut m = serde_json::Map::new();
        m.insert("dataset_id".to_string(), Value::from("ds1"));
        m.insert("version".to_string(), Value::from("v1"));
        m.insert("verdict".to_string(), Value::from("maybe"));
        let args = Value::Object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("verdict must be"));
    }

    #[tokio::test]
    async fn test_bundle_export_pass_without_traceable_subset_rejected() {
        // 前置形状校验：pass + 空 subset → 拒绝（与治理域 回归验证 B1 同口径）
        let tool = BundleExportTool::new(make_ws());
        let mut m = serde_json::Map::new();
        m.insert("dataset_id".to_string(), Value::from("ds1"));
        m.insert("version".to_string(), Value::from("v1"));
        m.insert("verdict".to_string(), Value::from("pass"));
        let args = Value::Object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("verdict=pass requires traceable evidence"));
    }

    #[tokio::test]
    async fn test_bundle_export_pass_with_bad_prefix_rejected() {
        // pass + subset 存在但前缀不是 sandbox:/human: → 拒绝
        let tool = BundleExportTool::new(make_ws());
        let mut m = serde_json::Map::new();
        m.insert("dataset_id".to_string(), Value::from("ds1"));
        m.insert("version".to_string(), Value::from("v1"));
        m.insert("verdict".to_string(), Value::from("pass"));
        m.insert(
            "subset".to_string(),
            Value::Array(vec![Value::from("opaque-ref")]),
        );
        let args = Value::Object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("verdict=pass requires traceable evidence"));
    }

    #[tokio::test]
    async fn test_bundle_export_fail_without_subset_passes_shape_check() {
        // fail（显式未验证）无 subset 要求 → 通过本地形状校验，走到网络层失败
        let tool = BundleExportTool::new(make_ws());
        let mut m = serde_json::Map::new();
        m.insert("dataset_id".to_string(), Value::from("ds1"));
        m.insert("version".to_string(), Value::from("v1"));
        m.insert("verdict".to_string(), Value::from("fail"));
        let args = Value::Object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(!msg.contains("missing required parameter"));
        assert!(!msg.contains("requires traceable evidence"));
    }
}
