// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `read_skill` —— 按名装载已声明技能的 SKILL.md 全文(skills 装配 B2 批)
//!
//! ## 安全模型(与 file_read 的本质区别)
//! - **参数仅 `skill_name`**:LLM 不给路径——查 agent definition 的 skills
//!   声明表映射绝对路径后读文件。声明时刻 = 人工把关(供应链闸口前移),
//!   路径来源 = 人工声明而非 LLM 参数,故**不走 path_scope 沙箱**——
//!   沙箱攻击面(路径穿越/symlink 逃逸)在此不存在
//! - 未声明/不存在的 name → 显式错误 + 列出可用清单(fail-visible)
//! - 只读常规文件;大小上限与 file_read 同款(默认 10 MB)
//! - 调用/结果走 G17 插桩 + journal tool_invoked/tool_result 既有记账
//!   (零新事件类;skill 装载史 = 普通工具调用史,会话重放天然含)

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::agent::definition::SkillManifestEntry;
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// `read_skill` 工具
///
/// 通过 `new()` 构造时绑定技能声明表,实例化后**只能读表中声明的 SKILL.md**。
#[derive(Clone)]
pub struct SkillReadTool {
    /// 声明表:name → SKILL.md 绝对路径
    skills: BTreeMap<String, PathBuf>,
    max_bytes: u64,
}

impl SkillReadTool {
    /// 由解析后的技能清单构造(见 `agent::definition::resolve_skill_manifest_entries`)
    pub fn new(entries: &[SkillManifestEntry]) -> Self {
        Self {
            skills: entries
                .iter()
                .map(|e| (e.name.clone(), e.path.clone()))
                .collect(),
            max_bytes: crate::builtin_tools::file_read::DEFAULT_MAX_BYTES,
        }
    }

    /// 设置最大文件大小(字节)
    pub fn with_max_bytes(mut self, n: u64) -> Self {
        self.max_bytes = n;
        self
    }
}

#[async_trait::async_trait]
impl ToolFunction for SkillReadTool {
    /// G13:async 入口 — 用 spawn_blocking 包装同步 fs 操作
    async fn call(&self, args: &Value) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("read_skill tool panicked: {}", e))?
    }
}

impl SkillReadTool {
    /// 同步实现(供 spawn_blocking 调用)
    fn call_sync(&self, args: &Value) -> IoResult {
        let name = args
            .get("skill_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: skill_name (string)".to_string())?;

        if self.skills.is_empty() {
            return Err(
                "no skills are declared for this agent (definition 'skills' list is empty)"
                    .to_string(),
            );
        }
        let Some(path) = self.skills.get(name) else {
            let available: Vec<&str> = self.skills.keys().map(String::as_str).collect();
            return Err(format!(
                "skill '{}' is not declared for this agent; available skills: {}",
                name,
                available.join(", ")
            ));
        };

        let metadata =
            std::fs::metadata(path).map_err(|e| format!("stat failed: {}", e))?;
        if !metadata.is_file() {
            return Err(format!(
                "declared skill path is not a regular file: '{}'",
                path.display()
            ));
        }
        if metadata.len() > self.max_bytes {
            return Err(format!(
                "skill file too large: {} bytes (max {} bytes)",
                metadata.len(),
                self.max_bytes
            ));
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("read failed (binary or permission?): {}", e))?;

        let mut map = serde_json::Map::new();
        map.insert("skill_name".to_string(), Value::from(name));
        map.insert("path".to_string(), Value::from(path.display().to_string()));
        map.insert("size".to_string(), Value::from(metadata.len() as i64));
        map.insert("content".to_string(), Value::from(content));
        Ok(Value::Object(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    fn write_skill(dir: &Path, rel: &str, body: &str) -> PathBuf {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        path
    }

    fn entry(name: &str, path: PathBuf, description: &str) -> SkillManifestEntry {
        SkillManifestEntry {
            name: name.to_string(),
            path,
            description: description.to_string(),
        }
    }

    fn args(skill_name: &str) -> Value {
        serde_json::json!({ "skill_name": skill_name })
    }

    #[test]
    fn test_read_declared_skill() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_skill(
            dir.path(),
            "git/SKILL.md",
            "---\nname: git-discipline\ndescription: d\n---\n# 正文\n先 git status。",
        );
        let tool = SkillReadTool::new(&[entry("git-discipline", p, "d")]);

        let result = tool.call_sync(&args("git-discipline"));
        assert!(result.is_ok());
        let v = result.unwrap();
        assert_eq!(v["skill_name"], "git-discipline");
        assert_eq!(v["content"], "---\nname: git-discipline\ndescription: d\n---\n# 正文\n先 git status。");
        assert!(v["size"].as_i64().unwrap() > 0);
        assert!(v["path"].as_str().unwrap().ends_with("SKILL.md"));
    }

    #[test]
    fn test_reject_undeclared_name_lists_available() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_skill(dir.path(), "a/SKILL.md", "---\nname: a\n---\nA");
        let b = write_skill(dir.path(), "b/SKILL.md", "---\nname: b\n---\nB");
        let tool = SkillReadTool::new(&[
            entry("alpha", a, "d"),
            entry("beta", b, "d"),
        ]);

        let err = tool.call_sync(&args("nope")).unwrap_err();
        assert!(err.contains("skill 'nope' is not declared"), "got: {}", err);
        assert!(err.contains("alpha, beta"), "must list available: {}", err);
    }

    #[test]
    fn test_reject_when_no_skills_declared() {
        let tool = SkillReadTool::new(&[]);
        let err = tool.call_sync(&args("x")).unwrap_err();
        assert!(err.contains("no skills are declared"), "got: {}", err);
    }

    #[test]
    fn test_missing_skill_name_arg() {
        let tool = SkillReadTool::new(&[]);
        let err = tool.call_sync(&Value::Object(serde_json::Map::new())).unwrap_err();
        assert!(err.contains("missing required arg"), "got: {}", err);
    }

    #[test]
    fn test_reject_oversized_skill_file() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big/SKILL.md");
        std::fs::create_dir_all(big.parent().unwrap()).unwrap();
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(2 * 1024 * 1024).unwrap(); // 2 MB 稀疏文件
        drop(f);

        let tool = SkillReadTool::new(&[entry("big", big, "d")]).with_max_bytes(1024 * 1024);
        let err = tool.call_sync(&args("big")).unwrap_err();
        assert!(err.contains("too large"), "got: {}", err);
    }

    #[test]
    fn test_declared_path_is_not_file() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("skills-dir");
        std::fs::create_dir_all(&sub).unwrap();
        let tool = SkillReadTool::new(&[entry("d", sub, "desc")]);
        let err = tool.call_sync(&args("d")).unwrap_err();
        assert!(err.contains("not a regular file"), "got: {}", err);
    }

    /// spec 形态:read_skill 在静态目录中,参数仅 skill_name(必填)
    #[test]
    fn test_read_skill_spec_shape() {
        let specs = crate::builtin_tools::default_tool_specs();
        let spec = specs
            .iter()
            .find(|s| s.name == "read_skill")
            .expect("read_skill spec must be in default_tool_specs");
        assert!(spec.description.contains("declared"));
        assert_eq!(spec.parameters.len(), 1);
        assert_eq!(spec.parameters[0].name, "skill_name");
        assert!(spec.parameters[0].required);
    }
}
