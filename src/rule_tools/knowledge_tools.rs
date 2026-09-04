// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! knowledge 执行侧数据面工具（3 个，UV-084 W2 · 41 号 §4.3 缺口 2）
//!
//! 消费执行域 knowledge 数据资产（GET /api/knowledge 三端点，只读）：
//! - knowledge_datasets：已承载数据集清单；
//! - knowledge_search：条目检索（q 包含匹配 / domain 精确 / tags 任一命中）；
//! - knowledge_entry_get：单条直取（payload 零转译原样）。
//!
//! 错误纪律：走 check_response_full——数据集未承载(404)/库加载失败(500)
//! 的 {"error": "..."} 详情透出（不静默空列表，不误判为会话不存在）。

use std::sync::Arc;

use evorule_tcb::JsonValue;

use crate::api::evorule_client::EvoruleApiClient;
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};
use crate::json_convert::serde_to_tcb;

// =============================================================================
// knowledge_datasets —— 数据集清单
// =============================================================================

#[derive(Clone)]
pub struct KnowledgeDatasetsTool {
    client: EvoruleApiClient,
}

impl KnowledgeDatasetsTool {
    pub fn new(client: EvoruleApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for KnowledgeDatasetsTool {
    async fn call(&self, _args: &JsonValue) -> IoResult {
        let result = self
            .client
            .knowledge_datasets()
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_to_tcb(&result))
    }
}

// =============================================================================
// knowledge_search —— 条目检索
// =============================================================================

#[derive(Clone)]
pub struct KnowledgeSearchTool {
    client: EvoruleApiClient,
}

impl KnowledgeSearchTool {
    pub fn new(client: EvoruleApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for KnowledgeSearchTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let dataset = args
            .get("dataset")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: dataset".to_string())?;
        let q = args.get("q").and_then(|v| v.as_str());
        let domain = args.get("domain").and_then(|v| v.as_str());
        let tags = args.get("tags").and_then(|v| v.as_str());
        let result = self
            .client
            .knowledge_entries(dataset, q, domain, tags)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_to_tcb(&result))
    }
}

// =============================================================================
// knowledge_entry_get —— 单条直取
// =============================================================================

#[derive(Clone)]
pub struct KnowledgeEntryGetTool {
    client: EvoruleApiClient,
}

impl KnowledgeEntryGetTool {
    pub fn new(client: EvoruleApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for KnowledgeEntryGetTool {
    async fn call(&self, args: &JsonValue) -> IoResult {
        let dataset = args
            .get("dataset")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: dataset".to_string())?;
        let entry_id = args
            .get("entry_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: entry_id".to_string())?;
        let result = self
            .client
            .knowledge_entry(dataset, entry_id)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_to_tcb(&result))
    }
}

// =============================================================================
// register / specs
// =============================================================================

pub fn register(h: &mut ToolHandler, client: &EvoruleApiClient) {
    h.register_tool(
        "knowledge_datasets",
        Arc::new(KnowledgeDatasetsTool::new(client.clone())),
    );
    h.register_tool(
        "knowledge_search",
        Arc::new(KnowledgeSearchTool::new(client.clone())),
    );
    h.register_tool(
        "knowledge_entry_get",
        Arc::new(KnowledgeEntryGetTool::new(client.clone())),
    );
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "knowledge_datasets".to_string(),
            description: "List knowledge datasets hosted in the execution domain (read-only \
                          data plane, GET /api/knowledge)."
                .to_string(),
            parameters: vec![],
        },
        ToolSpec {
            name: "knowledge_search".to_string(),
            description: "Search entries in a knowledge dataset (GET \
                          /api/knowledge/{ds}/entries). Dataset not hosted → explicit 404."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "dataset".to_string(),
                    r#type: "string".to_string(),
                    description: "Dataset id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "q".to_string(),
                    r#type: "string".to_string(),
                    description: "Substring match across entry_id / schema_ref / bundle_id / \
                                  payload."
                        .to_string(),
                    required: false,
                },
                ParameterSpec {
                    name: "domain".to_string(),
                    r#type: "string".to_string(),
                    description: "Domain exact match (case-insensitive).".to_string(),
                    required: false,
                },
                ParameterSpec {
                    name: "tags".to_string(),
                    r#type: "string".to_string(),
                    description: "Comma-separated tags (any-match).".to_string(),
                    required: false,
                },
            ],
        },
        ToolSpec {
            name: "knowledge_entry_get".to_string(),
            description: "Fetch a single knowledge entry by id (payload returned verbatim, \
                          zero translation)."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "dataset".to_string(),
                    r#type: "string".to_string(),
                    description: "Dataset id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "entry_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Entry id.".to_string(),
                    required: true,
                },
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_client() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:0")
    }

    #[test]
    fn test_specs_count() {
        assert_eq!(specs().len(), 3);
    }

    #[test]
    fn test_register_tools() {
        let client = make_client();
        let mut h = ToolHandler::new();
        register(&mut h, &client);
        assert!(h.has_tool("knowledge_datasets"));
        assert!(h.has_tool("knowledge_search"));
        assert!(h.has_tool("knowledge_entry_get"));
    }

    #[tokio::test]
    async fn test_knowledge_search_missing_dataset() {
        let tool = KnowledgeSearchTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: dataset"));
    }

    #[tokio::test]
    async fn test_knowledge_entry_get_missing_dataset() {
        let tool = KnowledgeEntryGetTool::new(make_client());
        let args = JsonValue::object(std::collections::BTreeMap::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: dataset"));
    }

    #[tokio::test]
    async fn test_knowledge_entry_get_missing_entry_id() {
        let tool = KnowledgeEntryGetTool::new(make_client());
        let mut m = std::collections::BTreeMap::new();
        m.insert("dataset".to_string(), JsonValue::string("ds1"));
        let args = JsonValue::object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: entry_id"));
    }
}
