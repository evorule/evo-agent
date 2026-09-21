// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Tool registry center -- manages tools available to Agent
use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::RwLock;

#[async_trait]
/// TODO: doc
pub trait ToolFunction: Send + Sync + 'static {
    /// TODO: doc
    async fn call(&self, args: &Value) -> Result<Value, String>;
}

/// Tool spec
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// Tool name
    pub name: String,
    /// Tool description
    pub description: String,
    /// Parameter spec list
    pub parameters: Vec<ParameterSpec>,
    /// Required parameter name list
    pub required: Vec<String>,
}

/// Parameter spec
#[derive(Debug, Clone)]
pub struct ParameterSpec {
    /// Parameter name
    pub name: String,
    /// Parameter type
    pub r#type: String,
    /// Parameter description
    pub description: String,
    /// Whether required
    pub required: bool,
}

/// Tool registry
pub struct ToolRegistry {
    tools: RwLock<BTreeMap<String, ToolEntry>>,
}

struct ToolEntry {
    func: Arc<dyn ToolFunction>,
    spec: ToolSpec,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    /// Create new tool registry
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(BTreeMap::new()),
        }
    }

    /// Register tool
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

    /// Unregister tool
    pub async fn unregister(&self, name: &str) -> bool {
        self.tools.write().await.remove(name).is_some()
    }

    /// Get tool function
    pub async fn get(&self, name: &str) -> Option<Arc<dyn ToolFunction>> {
        self.tools
            .read()
            .await
            .get(name)
            .map(|entry| entry.func.clone())
    }

    /// Get tool spec
    pub async fn get_spec(&self, name: &str) -> Option<ToolSpec> {
        self.tools
            .read()
            .await
            .get(name)
            .map(|entry| entry.spec.clone())
    }

    /// List all tool specs
    pub async fn list_tools(&self) -> Vec<ToolSpec> {
        self.tools
            .read()
            .await
            .values()
            .map(|entry| entry.spec.clone())
            .collect()
    }

    /// Get tool count
    pub async fn len(&self) -> usize {
        self.tools.read().await.len()
    }

    /// Check whether empty
    pub async fn is_empty(&self) -> bool {
        self.tools.read().await.is_empty()
    }

    /// Convert to OpenAI tool schema format
    pub async fn to_openai_schema(&self) -> Vec<Value> {
        let mut schema = Vec::new();
        for spec in self.list_tools().await {
            let mut params = serde_json::Map::new();
            let mut required = Vec::new();

            for param in &spec.parameters {
                let mut param_schema = serde_json::Map::new();
                param_schema.insert("type".to_string(), Value::from(param.r#type.clone()));
                param_schema.insert(
                    "description".to_string(),
                    Value::from(param.description.clone()),
                );
                params.insert(param.name.clone(), Value::Object(param_schema));

                if param.required {
                    required.push(param.name.clone());
                }
            }

            let mut tool = serde_json::Map::new();
            tool.insert("type".to_string(), Value::from("function"));
            tool.insert(
                "function".to_string(),
                Value::Object({
                    let mut func = serde_json::Map::new();
                    func.insert("name".to_string(), Value::from(spec.name));
                    func.insert("description".to_string(), Value::from(spec.description));
                    func.insert("parameters".to_string(), Value::Object(params));
                    if !required.is_empty() {
                        func.insert(
                            "required".to_string(),
                            Value::Array(required.into_iter().map(Value::String).collect()),
                        );
                    }
                    func
                }),
            );

            schema.push(Value::Object(tool));
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
        async fn call(&self, args: &Value) -> Result<Value, String> {
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
            description: "input text".to_string(),
            required: true,
        }];

        registry
            .register("echo", "echo input text", params, Arc::new(EchoTool))
            .await;

        assert!(!registry.is_empty().await);
        assert_eq!(registry.len().await, 1);

        let func = registry.get("echo").await;
        assert!(func.is_some());

        let spec = registry.get_spec("echo").await.unwrap();
        assert_eq!(spec.name, "echo");
        assert_eq!(spec.description, "echo input text");
    }

    #[tokio::test]
    async fn test_tool_registry_unregister() {
        let registry = ToolRegistry::new();
        let params = vec![ParameterSpec {
            name: "text".to_string(),
            r#type: "string".to_string(),
            description: "input".to_string(),
            required: true,
        }];

        registry
            .register("test", "test tool", params, Arc::new(EchoTool))
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
            description: "query".to_string(),
            required: true,
        }];
        registry
            .register("search", "search", params1, Arc::new(EchoTool))
            .await;

        let params2 = vec![ParameterSpec {
            name: "file".to_string(),
            r#type: "string".to_string(),
            description: "file path".to_string(),
            required: true,
        }];
        registry
            .register("read_file", "read file", params2, Arc::new(EchoTool))
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
                description: "search query".to_string(),
                required: true,
            },
            ParameterSpec {
                name: "limit".to_string(),
                r#type: "integer".to_string(),
                description: "result count".to_string(),
                required: false,
            },
        ];

        registry
            .register("web_search", "web search", params, Arc::new(EchoTool))
            .await;

        let schema = registry.to_openai_schema().await;
        assert_eq!(schema.len(), 1);

        let tool = schema.first().unwrap();
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
            description: "input".to_string(),
            required: true,
        }];

        registry
            .register("echo", "echo", params, Arc::new(EchoTool))
            .await;

        let func = registry.get("echo").await.unwrap();
        let args = serde_json::json!({"text": "hello"});
        let result = func.call(&args).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), args);
    }
}
