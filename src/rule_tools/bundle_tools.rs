// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! bundles 部署闭环工具（5 个，UV-084 W2 · 41 号 §4.3 缺口 1）
//!
//! 打通 evo-agent 独立完成"治理域导出 → 执行域部署"的全链路（此前
//! publish_tools 止步于治理域，部署最后一步只能靠 console 或人工）：
//!
//! ```text
//! bundle_export(治理域 :18081) → bundle_import_dry_run(执行域预检)
//!   → bundle_import(执行域落盘+reload+激活) → bundle_active_list(确认)
//!   → bundle_imports_list(溯源)
//! ```
//!
//! 错误纪律：全部走 check_response_full——校验失败(400)的 {"error": "..."}
//! 详情透出，LLM agent 可自诊断修复（不静默）。

use std::sync::Arc;

use evorule_tcb::JsonValue;

use crate::api::evorule_client::EvoruleApiClient;
use crate::api::workspace_client::WorkspaceApiClient;
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};
use crate::json_convert::{serde_to_tcb, tcb_to_serde};

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
    async fn call(&self, args: &JsonValue) -> IoResult {
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
        // 前置形状校验（与治理域 UV-080 B1 同口径，提前拦截省一次往返）：
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
        Ok(serde_to_tcb(&result))
    }
}

// =============================================================================
// bundle_import_dry_run —— 执行域导入预检（校验链全跑，不落盘）
// =============================================================================

#[derive(Clone)]
pub struct BundleImportDryRunTool {
    client: EvoruleApiClient,
}

impl BundleImportDryRunTool {
    pub fn new(client: EvoruleApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for BundleImportDryRunTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let bundle = args
            .get("bundle")
            .ok_or_else(|| "missing required parameter: bundle".to_string())?;
        if !bundle.is_object() {
            return Err(
                "bundle must be an object (DatasetBundle JSON from bundle_export)".to_string(),
            );
        }
        let result = self
            .client
            .bundle_import_dry_run(&tcb_to_serde(bundle))
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_to_tcb(&result))
    }
}

// =============================================================================
// bundle_import —— 执行域导入并激活（破坏性：落盘 + reload）
// =============================================================================

#[derive(Clone)]
pub struct BundleImportTool {
    client: EvoruleApiClient,
}

impl BundleImportTool {
    pub fn new(client: EvoruleApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for BundleImportTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let bundle = args
            .get("bundle")
            .ok_or_else(|| "missing required parameter: bundle".to_string())?;
        if !bundle.is_object() {
            return Err(
                "bundle must be an object (DatasetBundle JSON from bundle_export)".to_string(),
            );
        }
        let result = self
            .client
            .bundle_import(&tcb_to_serde(bundle))
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_to_tcb(&result))
    }
}

// =============================================================================
// bundle_active_list —— 当前激活 bundle 列表
// =============================================================================

#[derive(Clone)]
pub struct BundleActiveListTool {
    client: EvoruleApiClient,
}

impl BundleActiveListTool {
    pub fn new(client: EvoruleApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for BundleActiveListTool {
    async fn call(&self, _args: &JsonValue) -> IoResult {
        let result = self
            .client
            .bundle_active_list()
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_to_tcb(&result))
    }
}

// =============================================================================
// bundle_imports_list —— 导入溯源记录（只读审计）
// =============================================================================

#[derive(Clone)]
pub struct BundleImportsListTool {
    client: EvoruleApiClient,
}

impl BundleImportsListTool {
    pub fn new(client: EvoruleApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for BundleImportsListTool {
    async fn call(&self, _args: &JsonValue) -> IoResult {
        let result = self
            .client
            .bundle_imports_list()
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_to_tcb(&result))
    }
}

// =============================================================================
// register / specs
// =============================================================================

pub fn register(h: &mut ToolHandler, ws: &WorkspaceApiClient, ev: &EvoruleApiClient) {
    h.register_tool("bundle_export", Arc::new(BundleExportTool::new(ws.clone())));
    h.register_tool(
        "bundle_import_dry_run",
        Arc::new(BundleImportDryRunTool::new(ev.clone())),
    );
    h.register_tool("bundle_import", Arc::new(BundleImportTool::new(ev.clone())));
    h.register_tool(
        "bundle_active_list",
        Arc::new(BundleActiveListTool::new(ev.clone())),
    );
    h.register_tool(
        "bundle_imports_list",
        Arc::new(BundleImportsListTool::new(ev.clone())),
    );
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
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
        },
        ToolSpec {
            name: "bundle_import_dry_run".to_string(),
            description: "Pre-check a bundle import in the execution domain (all 8 \
                          validations run, no disk write, no reload). Pass the bundle object \
                          returned by bundle_export."
                .to_string(),
            parameters: vec![ParameterSpec {
                name: "bundle".to_string(),
                r#type: "object".to_string(),
                description: "DatasetBundle JSON object (as returned by bundle_export)."
                    .to_string(),
                required: true,
            }],
        },
        ToolSpec {
            name: "bundle_import".to_string(),
            description: "Import and activate a bundle in the execution domain (destructive: \
                          atomic write to rules/bundles/ + reload; new sessions pick up the \
                          new ruleset). Run bundle_import_dry_run first."
                .to_string(),
            parameters: vec![ParameterSpec {
                name: "bundle".to_string(),
                r#type: "object".to_string(),
                description: "DatasetBundle JSON object (as returned by bundle_export)."
                    .to_string(),
                required: true,
            }],
        },
        ToolSpec {
            name: "bundle_active_list".to_string(),
            description: "List currently active bundles in the execution domain (from \
                          rules/bundles/*/bundle_manifest.json)."
                .to_string(),
            parameters: vec![],
        },
        ToolSpec {
            name: "bundle_imports_list".to_string(),
            description: "List bundle import provenance records in the execution domain \
                          (read-only audit trail)."
                .to_string(),
            parameters: vec![],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ws() -> WorkspaceApiClient {
        WorkspaceApiClient::new("http://localhost:0")
    }

    fn make_ev() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:0")
    }

    #[test]
    fn test_specs_count() {
        assert_eq!(specs().len(), 5);
    }

    #[test]
    fn test_register_tools() {
        let ws = make_ws();
        let ev = make_ev();
        let mut h = ToolHandler::new();
        register(&mut h, &ws, &ev);
        for name in [
            "bundle_export",
            "bundle_import_dry_run",
            "bundle_import",
            "bundle_active_list",
            "bundle_imports_list",
        ] {
            assert!(h.has_tool(name), "tool {} should be registered", name);
        }
    }

    #[tokio::test]
    async fn test_bundle_export_missing_dataset_id() {
        let tool = BundleExportTool::new(make_ws());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: dataset_id"));
    }

    #[tokio::test]
    async fn test_bundle_export_invalid_verdict() {
        let tool = BundleExportTool::new(make_ws());
        let mut m = std::collections::BTreeMap::new();
        m.insert("dataset_id".to_string(), JsonValue::string("ds1"));
        m.insert("version".to_string(), JsonValue::string("v1"));
        m.insert("verdict".to_string(), JsonValue::string("maybe"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("verdict must be"));
    }

    #[tokio::test]
    async fn test_bundle_export_pass_without_traceable_subset_rejected() {
        // 前置形状校验：pass + 空 subset → 拒绝（与治理域 UV-080 B1 同口径）
        let tool = BundleExportTool::new(make_ws());
        let mut m = std::collections::BTreeMap::new();
        m.insert("dataset_id".to_string(), JsonValue::string("ds1"));
        m.insert("version".to_string(), JsonValue::string("v1"));
        m.insert("verdict".to_string(), JsonValue::string("pass"));
        let args = JsonValue::object(m);
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
        let mut m = std::collections::BTreeMap::new();
        m.insert("dataset_id".to_string(), JsonValue::string("ds1"));
        m.insert("version".to_string(), JsonValue::string("v1"));
        m.insert("verdict".to_string(), JsonValue::string("pass"));
        m.insert(
            "subset".to_string(),
            JsonValue::array(vec![JsonValue::string("opaque-ref")]),
        );
        let args = JsonValue::object(m);
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
        let mut m = std::collections::BTreeMap::new();
        m.insert("dataset_id".to_string(), JsonValue::string("ds1"));
        m.insert("version".to_string(), JsonValue::string("v1"));
        m.insert("verdict".to_string(), JsonValue::string("fail"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(!msg.contains("missing required parameter"));
        assert!(!msg.contains("requires traceable evidence"));
    }

    #[tokio::test]
    async fn test_bundle_import_missing_bundle() {
        let tool = BundleImportTool::new(make_ev());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: bundle"));
    }

    #[tokio::test]
    async fn test_bundle_import_non_object_bundle_rejected() {
        let tool = BundleImportTool::new(make_ev());
        let mut m = std::collections::BTreeMap::new();
        m.insert("bundle".to_string(), JsonValue::string("not-an-object"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("bundle must be an object"));
    }

    #[tokio::test]
    async fn test_bundle_import_dry_run_missing_bundle() {
        let tool = BundleImportDryRunTool::new(make_ev());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: bundle"));
    }

    #[tokio::test]
    async fn test_bundle_import_dry_run_non_object_bundle_rejected() {
        let tool = BundleImportDryRunTool::new(make_ev());
        let mut m = std::collections::BTreeMap::new();
        m.insert("bundle".to_string(), JsonValue::Integer(42));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("bundle must be an object"));
    }
}
