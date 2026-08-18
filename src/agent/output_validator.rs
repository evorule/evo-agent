// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! G11:结构化输出校验器
//!
//! 当 `AgentDefinition.output_format` 配置了格式要求时,`AgentRunner` 使用本模块:
//! - `build_output_format_instruction` 生成注入 system prompt 的格式指令
//! - `OutputValidator::clean_output` 清理 LLM 输出(去掉 markdown 代码块标记)
//! - `OutputValidator::validate` 校验输出是否符合 JSON Schema
//!
//! 校验失败时,runner 注入校正消息并让 LLM 重试(最多 `max_retries` 次)。

use crate::agent::definition::OutputFormat;
use jsonschema::JSONSchema;

/// 结构化输出校验器
pub struct OutputValidator {
    /// 编译后的 JSON Schema(None 表示只校验是否合法 JSON,不校验结构)
    schema: Option<JSONSchema>,
    /// 格式类型("json" / "text" / ...)
    format_type: String,
    /// 预构建的格式指令(注入 system prompt)
    instruction: String,
    /// G11:校验失败时的最大重试次数(默认 2)
    ///
    /// runner 在 `handle_call_external` 中据此决定是注入校正消息重试,
    /// 还是降级接受原输出。
    max_retries: usize,
}

/// G11:`max_retries` 的默认值
const DEFAULT_MAX_RETRIES: usize = 2;

impl OutputValidator {
    /// 从 `OutputFormat` 构造校验器
    ///
    /// # 错误
    /// - JSON Schema 编译失败(格式非法)
    pub fn from_output_format(format: &OutputFormat) -> Result<Self, String> {
        let schema = if let Some(s) = &format.schema {
            Some(JSONSchema::compile(s).map_err(|e| format!("invalid JSON schema: {}", e))?)
        } else {
            None
        };
        let instruction = build_output_format_instruction(format);
        let max_retries = format.max_retries.unwrap_or(DEFAULT_MAX_RETRIES);
        Ok(Self {
            schema,
            format_type: format.format_type.clone(),
            instruction,
            max_retries,
        })
    }

    /// 获取格式指令(注入 system prompt)
    pub fn instruction(&self) -> &str {
        &self.instruction
    }

    /// G11:获取最大重试次数
    pub fn max_retries(&self) -> usize {
        self.max_retries
    }

    /// 清理 LLM 输出(去掉 markdown 代码块标记等)
    ///
    /// 对于 JSON 格式,去掉 ```json ... ``` 包裹;
    /// 其他格式原样返回。
    pub fn clean_output(&self, content: &str) -> String {
        if self.format_type == "json" {
            let trimmed = content.trim();
            if trimmed.starts_with("```") {
                // 去掉开头的 ```json 或 ```
                let inner = trimmed
                    .strip_prefix("```json")
                    .or_else(|| trimmed.strip_prefix("```"))
                    .unwrap_or(trimmed);
                let inner = inner.trim();
                // 去掉结尾的 ```
                if let Some(end) = inner.rfind("```") {
                    return inner[..end].trim().to_string();
                }
                // 没有结尾 ``` 的异常情况,返回 inner
                return inner.to_string();
            }
        }
        content.to_string()
    }

    /// 校验输出是否符合格式要求
    ///
    /// - JSON 格式:先解析为 JSON,再用 Schema 校验结构
    /// - text 格式:不校验(返回 Ok)
    ///
    /// # 错误
    /// - JSON 解析失败
    /// - Schema 校验失败(含具体字段路径和错误类型)
    pub fn validate(&self, content: &str) -> Result<(), String> {
        match self.format_type.as_str() {
            "json" => {
                let value: serde_json::Value = serde_json::from_str(content)
                    .map_err(|e| format!("output is not valid JSON: {}", e))?;

                if let Some(schema) = &self.schema {
                    if let Err(errors) = schema.validate(&value) {
                        let err_msgs: Vec<String> = errors
                            .map(|e| {
                                let path_str = e.instance_path.to_string();
                                let path = if path_str.is_empty() {
                                    "(root)".to_string()
                                } else {
                                    path_str
                                };
                                format!("  {}: {:?}", path, e.kind)
                            })
                            .collect();
                        return Err(format!(
                            "output does not match schema:\n{}",
                            err_msgs.join("\n")
                        ));
                    }
                }
                Ok(())
            }
            "text" => Ok(()),
            _ => Ok(()),
        }
    }
}

/// 构建注入 system prompt 的格式指令
///
/// 根据 `OutputFormat` 的 `format_type` 和 `schema` 生成中文指令文本。
pub fn build_output_format_instruction(format: &OutputFormat) -> String {
    match format.format_type.as_str() {
        "json" => {
            let schema_str = format
                .schema
                .as_ref()
                .map(|s| serde_json::to_string_pretty(s).unwrap_or_else(|_| "{}".to_string()))
                .unwrap_or_else(|| "{}".to_string());
            format!(
                "\n\n## 输出格式要求\n\
                 你必须输出符合以下 JSON Schema 的 JSON 对象。\n\
                 不要输出任何其他内容(不要 markdown 代码块标记,不要解释文字):\n\
                 ```json\n{}\n```",
                schema_str
            )
        }
        "text" => {
            if let Some(schema) = &format.schema {
                format!("\n\n## 输出格式要求\n{}", schema)
            } else {
                String::new()
            }
        }
        other => format!("\n\n## 输出格式要求\n输出格式: {}", other),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    fn make_json_format(schema: Option<serde_json::Value>) -> OutputFormat {
        OutputFormat {
            format_type: "json".to_string(),
            schema,
            max_retries: None,
        }
    }

    // ===== build_output_format_instruction 测试 =====

    #[test]
    fn test_instruction_json_with_schema() {
        let format = make_json_format(Some(json!({
            "type": "object",
            "properties": {
                "summary": {"type": "string"}
            },
            "required": ["summary"]
        })));
        let instruction = build_output_format_instruction(&format);
        assert!(instruction.contains("JSON Schema"));
        assert!(instruction.contains("summary"));
        assert!(instruction.contains("不要 markdown"));
    }

    #[test]
    fn test_instruction_json_without_schema() {
        let format = make_json_format(None);
        let instruction = build_output_format_instruction(&format);
        assert!(instruction.contains("JSON Schema"));
    }

    #[test]
    fn test_instruction_text_with_schema() {
        let format = OutputFormat {
            format_type: "text".to_string(),
            schema: Some(json!("输出一段不超过 100 字的摘要")),
            max_retries: None,
        };
        let instruction = build_output_format_instruction(&format);
        assert!(instruction.contains("不超过 100 字"));
    }

    #[test]
    fn test_instruction_text_without_schema() {
        let format = OutputFormat {
            format_type: "text".to_string(),
            schema: None,
            max_retries: None,
        };
        let instruction = build_output_format_instruction(&format);
        assert!(instruction.is_empty());
    }

    // ===== clean_output 测试 =====

    #[test]
    fn test_clean_output_strips_markdown_json_block() {
        let validator = OutputValidator::from_output_format(&make_json_format(None)).unwrap();
        let cleaned = validator.clean_output("```json\n{\"key\": \"value\"}\n```");
        assert_eq!(cleaned, r#"{"key": "value"}"#);
    }

    #[test]
    fn test_clean_output_strips_bare_code_block() {
        let validator = OutputValidator::from_output_format(&make_json_format(None)).unwrap();
        let cleaned = validator.clean_output("```\n{\"key\": \"value\"}\n```");
        assert_eq!(cleaned, r#"{"key": "value"}"#);
    }

    #[test]
    fn test_clean_output_no_markdown() {
        let validator = OutputValidator::from_output_format(&make_json_format(None)).unwrap();
        let cleaned = validator.clean_output(r#"{"key": "value"}"#);
        assert_eq!(cleaned, r#"{"key": "value"}"#);
    }

    #[test]
    fn test_clean_output_text_format_unchanged() {
        let format = OutputFormat {
            format_type: "text".to_string(),
            schema: None,
            max_retries: None,
        };
        let validator = OutputValidator::from_output_format(&format).unwrap();
        let input = "```json\nthis should not be stripped\n```";
        let cleaned = validator.clean_output(input);
        assert_eq!(cleaned, input);
    }

    // ===== validate 测试 =====

    #[test]
    fn test_validate_valid_json_no_schema() {
        let validator = OutputValidator::from_output_format(&make_json_format(None)).unwrap();
        assert!(validator.validate(r#"{"key": "value"}"#).is_ok());
    }

    #[test]
    fn test_validate_invalid_json() {
        let validator = OutputValidator::from_output_format(&make_json_format(None)).unwrap();
        let result = validator.validate("not json at all");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not valid JSON"));
    }

    #[test]
    fn test_validate_valid_json_matching_schema() {
        let format = make_json_format(Some(json!({
            "type": "object",
            "properties": {
                "summary": {"type": "string"},
                "count": {"type": "number"}
            },
            "required": ["summary"]
        })));
        let validator = OutputValidator::from_output_format(&format).unwrap();
        assert!(validator
            .validate(r#"{"summary": "hello", "count": 42}"#)
            .is_ok());
    }

    #[test]
    fn test_validate_json_missing_required_field() {
        let format = make_json_format(Some(json!({
            "type": "object",
            "properties": {
                "summary": {"type": "string"}
            },
            "required": ["summary"]
        })));
        let validator = OutputValidator::from_output_format(&format).unwrap();
        let result = validator.validate(r#"{"other": "value"}"#);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("does not match schema"));
    }

    #[test]
    fn test_validate_json_wrong_type() {
        let format = make_json_format(Some(json!({
            "type": "object",
            "properties": {
                "count": {"type": "number"}
            },
            "required": ["count"]
        })));
        let validator = OutputValidator::from_output_format(&format).unwrap();
        let result = validator.validate(r#"{"count": "not a number"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_text_always_ok() {
        let format = OutputFormat {
            format_type: "text".to_string(),
            schema: None,
            max_retries: None,
        };
        let validator = OutputValidator::from_output_format(&format).unwrap();
        assert!(validator.validate("anything").is_ok());
        assert!(validator.validate("").is_ok());
    }

    // ===== 端到端:clean + validate =====

    #[test]
    fn test_clean_then_validate_markdown_wrapped_json() {
        let format = make_json_format(Some(json!({
            "type": "object",
            "properties": {
                "result": {"type": "string"}
            },
            "required": ["result"]
        })));
        let validator = OutputValidator::from_output_format(&format).unwrap();

        let raw = "```json\n{\"result\": \"success\"}\n```";
        let cleaned = validator.clean_output(raw);
        assert!(validator.validate(&cleaned).is_ok());
    }

    #[test]
    fn test_invalid_schema_returns_error() {
        let format = make_json_format(Some(json!("not an object")));
        let result = OutputValidator::from_output_format(&format);
        // jsonschema crate 应拒绝非法 schema
        assert!(result.is_err());
    }

    // ===== G11: max_retries 测试 =====

    #[test]
    fn test_max_retries_defaults_to_2_when_none() {
        let format = make_json_format(None);
        let validator = OutputValidator::from_output_format(&format).unwrap();
        assert_eq!(validator.max_retries(), 2);
    }

    #[test]
    fn test_max_retries_configurable() {
        let format = OutputFormat {
            format_type: "json".to_string(),
            schema: None,
            max_retries: Some(5),
        };
        let validator = OutputValidator::from_output_format(&format).unwrap();
        assert_eq!(validator.max_retries(), 5);
    }

    #[test]
    fn test_max_retries_zero_allowed() {
        // Some(0) = 不重试,首次失败即降级接受
        let format = OutputFormat {
            format_type: "json".to_string(),
            schema: None,
            max_retries: Some(0),
        };
        let validator = OutputValidator::from_output_format(&format).unwrap();
        assert_eq!(validator.max_retries(), 0);
    }

    #[test]
    fn test_output_format_deserialize_with_max_retries() {
        let json = r#"{
            "type": "json",
            "schema": {"type": "object"},
            "max_retries": 3
        }"#;
        let format: OutputFormat = serde_json::from_str(json).expect("parse");
        assert_eq!(format.format_type, "json");
        assert!(format.schema.is_some());
        assert_eq!(format.max_retries, Some(3));
    }

    #[test]
    fn test_output_format_deserialize_without_max_retries() {
        // 旧版 agent.json 不含 max_retries 字段,应反序列化为 None
        let json = r#"{
            "type": "json",
            "schema": {"type": "object"}
        }"#;
        let format: OutputFormat = serde_json::from_str(json).expect("parse");
        assert_eq!(format.max_retries, None);
    }

    #[test]
    fn test_output_format_serialize_skips_none_max_retries() {
        let format = make_json_format(None);
        let json = serde_json::to_string(&format).expect("serialize");
        // max_retries 为 None 时不应出现在序列化结果中
        assert!(!json.contains("max_retries"));
    }
}
