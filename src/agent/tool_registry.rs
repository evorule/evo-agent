// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 宸ュ叿娉ㄥ唽涓績 鈥斺€?绠＄悊 Agent 鍙敤鐨勫伐鍏?
use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use tier0_tcb::JsonValue;
use tokio::sync::RwLock;

#[async_trait]
pub trait ToolFunction: Send + Sync + 'static {
    async fn call(&self, args: &JsonValue) -> Result<JsonValue, String>;
}

/// 宸ュ叿瑙勬牸
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// 宸ュ叿鍚嶇О
    pub name: String,
    /// 宸ュ叿鎻忚堪
    pub description: String,
    /// 鍙傛暟瑙勬牸鍒楄〃
    pub parameters: Vec<ParameterSpec>,
    /// 蹇呭～鍙傛暟鍚嶇О鍒楄〃
    pub required: Vec<String>,
}

/// 鍙傛暟瑙勬牸
#[derive(Debug, Clone)]
pub struct ParameterSpec {
    /// 鍙傛暟鍚嶇О
    pub name: String,
    /// 鍙傛暟绫诲瀷
    pub r#type: String,
    /// 鍙傛暟鎻忚堪
    pub description: String,
    /// 鏄惁蹇呭～
    pub required: bool,
}

/// 宸ュ叿娉ㄥ唽涓績
pub struct ToolRegistry {
    tools: RwLock<BTreeMap<String, ToolEntry>>,
}

struct ToolEntry {
    func: Arc<dyn ToolFunction>,
    spec: ToolSpec,
}

impl ToolRegistry {
    /// 鍒涘缓鏂扮殑宸ュ叿娉ㄥ唽涓績
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(BTreeMap::new()),
        }
    }

    /// 娉ㄥ唽宸ュ叿
    pub async fn register(
        &self,
        name: &str,
        description: &str,
        parameters: Vec<ParameterSpec>,
        func: Arc<dyn ToolFunction>,
    ) {
        let required: Vec<String> = parameters
            .iter()
            .filter(|p| p.required)
            .map(|p| p.name.clone())
            .collect();

        let spec = ToolSpec {
            name: name.to_string(),
            description: description.to_string(),
            parameters,
            required,
        };

        self.tools
            .write()
            .await
            .insert(name.to_string(), ToolEntry { func, spec });
    }

    /// 娉ㄩ攢宸ュ叿
    pub async fn unregister(&self, name: &str) -> bool {
        self.tools.write().await.remove(name).is_some()
    }

    /// 鑾峰彇宸ュ叿鍑芥暟
    pub async fn get(&self, name: &str) -> Option<Arc<dyn ToolFunction>> {
        self.tools
            .read()
            .await
            .get(name)
            .map(|entry| entry.func.clone())
    }

    /// 鑾峰彇宸ュ叿瑙勬牸
    pub async fn get_spec(&self, name: &str) -> Option<ToolSpec> {
        self.tools
            .read()
            .await
            .get(name)
            .map(|entry| entry.spec.clone())
    }

    /// 鍒楀嚭鎵€鏈夊伐鍏疯鏍
    pub async fn list_tools(&self) -> Vec<ToolSpec> {
        self.tools
            .read()
            .await
            .values()
            .map(|entry| entry.spec.clone())
            .collect()
    }

    /// 鑾峰彇宸ュ叿鏁伴噺
    pub async fn len(&self) -> usize {
        self.tools.read().await.len()
    }

    /// 鍒ゆ柇鏄惁涓虹┖
    pub async fn is_empty(&self) -> bool {
        self.tools.read().await.is_empty()
    }

    /// 杞崲涓?OpenAI 宸ュ叿 schema 鏍煎紡
    pub async fn to_openai_schema(&self) -> Vec<JsonValue> {
        let mut schema = Vec::new();
        for spec in self.list_tools().await {
            let mut params = BTreeMap::new();
            let mut required = Vec::new();

            for param in &spec.parameters {
                let mut param_schema = BTreeMap::new();
                param_schema.insert("type".to_string(), JsonValue::string(param.r#type.clone()));
                param_schema.insert(
                    "description".to_string(),
                    JsonValue::string(param.description.clone()),
                );
                params.insert(param.name.clone(), JsonValue::Object(param_schema));

                if param.required {
                    required.push(param.name.clone());
                }
            }

            let mut tool = BTreeMap::new();
            tool.insert("type".to_string(), JsonValue::string("function"));
            tool.insert(
                "function".to_string(),
                JsonValue::Object({
                    let mut func = BTreeMap::new();
                    func.insert("name".to_string(), JsonValue::string(spec.name));
                    func.insert(
                        "description".to_string(),
                        JsonValue::string(spec.description),
                    );
                    func.insert("parameters".to_string(), JsonValue::Object(params));
                    if !required.is_empty() {
                        func.insert(
                            "required".to_string(),
                            JsonValue::Array(required.into_iter().map(JsonValue::string).collect()),
                        );
                    }
                    func
                }),
            );

            schema.push(JsonValue::Object(tool));
        }
        schema
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool;

    #[async_trait]
    impl ToolFunction for EchoTool {
        async fn call(&self, args: &JsonValue) -> Result<JsonValue, String> {
            Ok(args.clone())
        }
    }

    #[tokio::test]
    async fn test_tool_registry_new() {
        let registry = ToolRegistry::new();
        assert!(registry.is_empty().await);
        assert_eq!(registry.len().await, 0);
    }

    #[tokio::test]
    async fn test_tool_registry_register_and_get() {
        let registry = ToolRegistry::new();

        let params = vec![ParameterSpec {
            name: "text".to_string(),
            r#type: "string".to_string(),
            description: "杈撳叆鏂囨湰".to_string(),
            required: true,
        }];

        registry
            .register("echo", "杩斿洖杈撳叆鏂囨湰", params, Arc::new(EchoTool))
            .await;

        assert!(!registry.is_empty().await);
        assert_eq!(registry.len().await, 1);

        let func = registry.get("echo").await;
        assert!(func.is_some());

        let spec = registry.get_spec("echo").await.unwrap();
        assert_eq!(spec.name, "echo");
        assert_eq!(spec.description, "杩斿洖杈撳叆鏂囨湰");
    }

    #[tokio::test]
    async fn test_tool_registry_unregister() {
        let registry = ToolRegistry::new();
        let params = vec![ParameterSpec {
            name: "text".to_string(),
            r#type: "string".to_string(),
            description: "杈撳叆".to_string(),
            required: true,
        }];

        registry
            .register("test", "娴嬭瘯宸ュ叿", params, Arc::new(EchoTool))
            .await;
        assert_eq!(registry.len().await, 1);

        let removed = registry.unregister("test").await;
        assert!(removed);
        assert_eq!(registry.len().await, 0);

        let removed = registry.unregister("nonexistent").await;
        assert!(!removed);
    }

    #[tokio::test]
    async fn test_tool_registry_list_tools() {
        let registry = ToolRegistry::new();

        let params1 = vec![ParameterSpec {
            name: "q".to_string(),
            r#type: "string".to_string(),
            description: "鏌ヨ".to_string(),
            required: true,
        }];
        registry
            .register("search", "鎼滅储", params1, Arc::new(EchoTool))
            .await;

        let params2 = vec![ParameterSpec {
            name: "file".to_string(),
            r#type: "string".to_string(),
            description: "鏂囦欢鍚".to_string(),
            required: true,
        }];
        registry
            .register("read_file", "璇诲彇鏂囦欢", params2, Arc::new(EchoTool))
            .await;

        let tools = registry.list_tools().await;
        assert_eq!(tools.len(), 2);

        let names: Vec<String> = tools.into_iter().map(|t| t.name).collect();
        assert!(names.contains(&"search".to_string()));
        assert!(names.contains(&"read_file".to_string()));
    }

    #[tokio::test]
    async fn test_tool_registry_to_openai_schema() {
        let registry = ToolRegistry::new();

        let params = vec![
            ParameterSpec {
                name: "query".to_string(),
                r#type: "string".to_string(),
                description: "鎼滅储鏌ヨ".to_string(),
                required: true,
            },
            ParameterSpec {
                name: "limit".to_string(),
                r#type: "integer".to_string(),
                description: "缁撴灉鏁伴噺".to_string(),
                required: false,
            },
        ];

        registry
            .register("web_search", "缃戠粶鎼滅储", params, Arc::new(EchoTool))
            .await;

        let schema = registry.to_openai_schema().await;
        assert_eq!(schema.len(), 1);

        let tool = schema.get(0).unwrap();
        assert_eq!(tool.get("type").and_then(|v| v.as_str()), Some("function"));

        let func = tool.get("function").and_then(|v| v.as_object());
        assert!(func.is_some());
        assert_eq!(
            func.unwrap().get("name").and_then(|v| v.as_str()),
            Some("web_search")
        );
    }

    #[tokio::test]
    async fn test_tool_execution() {
        let registry = ToolRegistry::new();

        let params = vec![ParameterSpec {
            name: "text".to_string(),
            r#type: "string".to_string(),
            description: "杈撳叆".to_string(),
            required: true,
        }];

        registry
            .register("echo", "鍥炴樉", params, Arc::new(EchoTool))
            .await;

        let func = registry.get("echo").await.unwrap();
        let args = JsonValue::object_from_pairs(&[("text", JsonValue::string("hello"))]);
        let result = func.call(&args).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), args);
    }
}
