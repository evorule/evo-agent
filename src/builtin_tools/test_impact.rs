// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `test_impact` —— 变更→测试建议(只读;确定性惯例映射)。
//!
//! 输入变更文件清单,按语言惯例映射候选测试文件(盘上存在性核实),
//! 返回建议清单与聚合命令提示——**只建议不执行**(执行仍走
//! build_check/shell_exec 审批链,本工具零命令面)。
//!
//! ## 惯例映射(保守集)
//!
//! - Python:`src/foo.py` → `tests/test_foo.py` / `test/test_foo.py` /
//!   同目录 `test_foo.py`;命令提示 `python -m pytest <files> -q`;
//! - Rust:`src/foo.rs` → `tests/foo.rs`;命令提示 `cargo test --test foo`
//!   (集成测试形态;单元测试在文件内,建议=该文件自身);
//! - TS/JS:`a.ts` → 同目录 `a.test.ts` / `a.spec.ts` 与 `tests/` 镜像;
//! - 已是测试文件的建议=直接重跑自身。
//!
//! 符号级影响面(谁调用了我→波及谁)不在本工具:经 `codegraph` 的
//! `impact` 服务显式查询(需已索引仓)。

#![forbid(unsafe_code)]

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::io_handler::IoResult;

use crate::io_handlers::tool_handler::ToolFunction;

/// 工具名(LLM 契约面/manifest/注册三处同键)
pub const TEST_IMPACT_TOOL: &str = "test_impact";

/// 单次输入路径帽(超出截断并标注)
pub const MAX_PATHS: usize = 20;
/// 单变更文件的建议帽
const SUGGESTIONS_PER_PATH: usize = 10;

/// 工具本体(工作区根锚定)
pub struct TestImpactTool {
    workdir: PathBuf,
}

/// 语言族(判定+映射)
enum Lang {
    Python,
    Rust,
    Script,
}

fn lang_for(path: &str) -> Option<Lang> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".py") {
        Some(Lang::Python)
    } else if lower.ends_with(".rs") {
        Some(Lang::Rust)
    } else if lower.ends_with(".ts")
        || lower.ends_with(".tsx")
        || lower.ends_with(".js")
        || lower.ends_with(".jsx")
    {
        Some(Lang::Script)
    } else {
        None
    }
}

fn is_test_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit(['/', '\\']).next().unwrap_or(&lower);
    name.starts_with("test_")
        || name.ends_with("_test.py")
        || name.ends_with("_test.rs")
        || name.ends_with(".test.ts")
        || name.ends_with(".test.tsx")
        || name.ends_with(".test.js")
        || name.ends_with(".spec.ts")
        || name.ends_with(".spec.js")
        || matches!(name, "tests.rs" | "test.rs")
}

/// 惯例候选(相对路径形态;存在性由调用方核实)
fn candidates(path: &str, lang: &Lang) -> Vec<String> {
    let normalized = path.replace('\\', "/");
    let stem = normalized
        .rsplit('/')
        .next()
        .unwrap_or("")
        .split('.')
        .next()
        .unwrap_or("")
        .to_string();
    let dir = normalized.rsplit_once('/').map(|(d, _)| d.to_string());
    match lang {
        Lang::Python => {
            let mut c = vec![
                format!("tests/test_{stem}.py"),
                format!("test/test_{stem}.py"),
            ];
            if let Some(d) = &dir {
                c.push(format!("{d}/test_{stem}.py"));
            }
            c
        }
        Lang::Rust => vec![format!("tests/{stem}.rs")],
        Lang::Script => {
            let mut c = Vec::new();
            if dir.is_some() {
                let base = normalized
                    .rsplit_once('.')
                    .map(|(b, _)| b.to_string())
                    .unwrap_or_else(|| normalized.clone());
                c.push(format!("{base}.test.ts"));
                c.push(format!("{base}.spec.ts"));
            }
            c.push(format!("tests/{stem}.test.ts"));
            c
        }
    }
}

impl TestImpactTool {
    /// 构造(workdir=装配工作区根)
    pub fn new(workdir: PathBuf) -> Self {
        Self { workdir }
    }

    /// 单变更文件→存在的测试文件建议(确定性;帽内)
    fn suggest(&self, path: &str) -> Value {
        let exists = |rel: &str| self.workdir.join(rel).is_file();
        if is_test_file(path) {
            return json!({
                "changed": path,
                "tests": [path],
                "command_hint": "run this test file directly (it is itself a test)",
            });
        }
        let Some(lang) = lang_for(path) else {
            return json!({
                "changed": path,
                "tests": [],
                "command_hint": Value::Null,
                "note": "unsupported extension (supported: .py/.rs/.ts/.tsx/.js/.jsx)",
            });
        };
        let mut tests: Vec<String> = Vec::new();
        for c in candidates(path, &lang) {
            if tests.len() >= SUGGESTIONS_PER_PATH {
                break;
            }
            if exists(&c) && !tests.contains(&c) {
                tests.push(c);
            }
        }
        let hint = match lang {
            Lang::Python if !tests.is_empty() => {
                Some(format!("python -m pytest {} -q", tests.join(" ")))
            }
            Lang::Rust if !tests.is_empty() => {
                let stems: Vec<String> = tests
                    .iter()
                    .filter_map(|t| {
                        t.rsplit('/')
                            .next()
                            .map(|f| f.trim_end_matches(".rs").to_string())
                    })
                    .collect();
                Some(format!("cargo test {}", stems.join(" ")))
            }
            _ => None,
        };
        json!({
            "changed": path,
            "tests": tests,
            "command_hint": hint.map(Value::from).unwrap_or(Value::Null),
        })
    }
}

#[async_trait::async_trait]
impl ToolFunction for TestImpactTool {
    async fn call(&self, args: &Value) -> IoResult {
        let paths = args
            .get("paths")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                "missing required array param: paths (list of changed files)".to_string()
            })?;
        if paths.is_empty() {
            return Err("test_impact: paths must be a non-empty array".to_string());
        }
        let mut items = Vec::new();
        let mut commands: Vec<String> = Vec::new();
        let mut truncated = false;
        for p in paths.iter().take(MAX_PATHS) {
            let Some(s) = p.as_str() else {
                return Err("test_impact: paths entries must be strings".to_string());
            };
            let sug = self.suggest(s);
            if let Some(h) = sug.get("command_hint").and_then(|v| v.as_str()) {
                if !commands.contains(&h.to_string()) {
                    commands.push(h.to_string());
                }
            }
            items.push(sug);
        }
        if paths.len() > MAX_PATHS {
            truncated = true;
        }
        Ok(json!({
            "paths": items.iter().map(|i| i["changed"].clone()).collect::<Vec<_>>(),
            "suggestions": items,
            "commands": commands,
            "truncated": truncated,
            "note": "heuristic conventional mapping only (files verified on disk); \
                     for symbol-level impact use the codegraph impact service on an \
                     indexed repo. Suggestions are advisory — execution still goes \
                     through build_check/shell_exec approval.",
        }))
    }
}

/// LLM 契约面参数 schema(与 call 判定同源)
pub fn test_impact_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "paths": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Changed file paths relative to the workspace root \
                                (supported: .py/.rs/.ts/.tsx/.js/.jsx; cap 20).",
            },
        },
        "required": ["paths"],
    })
}

/// LLM 契约面描述(英文;只读+只建议不执行)
pub fn test_impact_description() -> String {
    "Given changed files, suggest which test files likely cover them \
     (conventional mapping verified on disk: tests/test_<stem>.py for Python, \
     tests/<stem>.rs for Rust integration tests, <file>.test.ts siblings for \
     TS/JS) plus aggregate run-command hints. Advisory only — it never \
     executes anything; run the suggested commands via build_check or \
     shell_exec. For symbol-level impact analysis use the codegraph impact \
     service on an indexed repository."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_handlers::tool_handler::ToolFunction;

    fn tool_with_ws() -> (tempfile::TempDir, TestImpactTool) {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let tool = TestImpactTool::new(ws.clone());
        (dir, tool)
    }

    #[test]
    fn schema_requires_paths() {
        let s = test_impact_schema();
        assert_eq!(s["required"].as_array().unwrap().len(), 1);
        assert_eq!(s["properties"]["paths"]["type"], "array");
    }

    #[tokio::test]
    async fn missing_paths_fails_visible() {
        let (_d, tool) = tool_with_ws();
        let err = tool.call(&json!({})).await.unwrap_err();
        assert!(err.contains("paths"));
        let err = tool.call(&json!({"paths": []})).await.unwrap_err();
        assert!(err.contains("non-empty"));
    }

    #[tokio::test]
    async fn unsupported_extension_noted() {
        let (_d, tool) = tool_with_ws();
        let r = tool.call(&json!({"paths": ["readme.md"]})).await.unwrap();
        let first = &r["suggestions"][0];
        assert!(first["note"].as_str().unwrap().contains("unsupported"));
    }

    #[tokio::test]
    async fn python_convention_suggests_existing_test() {
        let (_d, tool) = tool_with_ws();
        let ws = tool.workdir.clone();
        std::fs::create_dir_all(ws.join("tests")).unwrap();
        std::fs::write(ws.join("src_mod.py"), b"").unwrap();
        std::fs::write(ws.join("tests").join("test_src_mod.py"), b"").unwrap();
        let r = tool.call(&json!({"paths": ["src_mod.py"]})).await.unwrap();
        let sug = &r["suggestions"][0];
        assert_eq!(sug["tests"][0].as_str(), Some("tests/test_src_mod.py"));
        assert_eq!(
            r["commands"][0].as_str(),
            Some("python -m pytest tests/test_src_mod.py -q")
        );
    }

    #[tokio::test]
    async fn absent_candidates_yield_empty_tests() {
        let (_d, tool) = tool_with_ws();
        std::fs::write(tool.workdir.join("lonely.py"), b"").unwrap();
        let r = tool.call(&json!({"paths": ["lonely.py"]})).await.unwrap();
        let sug = &r["suggestions"][0];
        assert_eq!(sug["tests"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_file_itself_suggests_direct_rerun() {
        let (_d, tool) = tool_with_ws();
        std::fs::create_dir_all(tool.workdir.join("tests")).unwrap();
        std::fs::write(tool.workdir.join("tests").join("test_x.py"), b"").unwrap();
        let r = tool
            .call(&json!({"paths": ["tests/test_x.py"]}))
            .await
            .unwrap();
        let sug = &r["suggestions"][0];
        assert_eq!(sug["tests"][0].as_str(), Some("tests/test_x.py"));
        assert!(sug["command_hint"]
            .as_str()
            .unwrap()
            .contains("it is itself a test"));
    }
}
