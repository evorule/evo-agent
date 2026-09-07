// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 宪法 schema 校验桥（M7-B1/B2，2026-08-27）
//!
//! 把 evorule-system-rules 的 `agent_def/v1.0.json` / `workflow_dag/v1.0.json`
//! 接入加载期：资产（或其 sidecar 标注层）反序列化后先过 jsonschema 全量校验，
//! 再进入业务门卫（`AgentDefinition::validate` 等）。
//!
//! ## Schema 来源与 fail-fast 策略（审计⑥ C12）
//!
//! 查找顺序：
//! 1. `EVORULE_SYSTEM_RULES` 环境变量指向的仓根
//! 2. 可执行文件向上 ancestor 中的 `evorule-system-rules/schemas`（兄弟目录约定）
//!
//! 都找不到时**拒绝加载资产（fail-fast）**,错误消息给出自助修复指引。
//! 早期为"降级为仅业务门卫"的放行设计,审计⑥判定为治理缺口:门禁缺失
//! 意味着资产未经宪法 schema 校验就进入运行时,对确定性执行不可接受。
//! struct 级门卫（取值范围、标识符白名单）不受此影响,始终生效。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use jsonschema::JSONSchema;

/// 已编译的 agent_def v1.0 校验器（进程内单例）
static AGENT_DEF_SCHEMA: OnceLock<Option<JSONSchema>> = OnceLock::new();
/// 已编译的 workflow_dag v1.0 校验器（进程内单例）
static WORKFLOW_DAG_SCHEMA: OnceLock<Option<JSONSchema>> = OnceLock::new();

/// 定位宪法仓 schemas 目录
pub fn locate_schemas_dir() -> Option<PathBuf> {
    // 1) 显式环境变量
    if let Ok(root) = std::env::var("EVORULE_SYSTEM_RULES") {
        let p = PathBuf::from(root).join("schemas");
        if p.is_dir() {
            return Some(p);
        }
    }
    // 2) exe 向上找兄弟目录
    let exe = std::env::current_exe().ok()?;
    let mut cur: Option<&Path> = exe.parent();
    while let Some(dir) = cur {
        let candidate = dir.join("evorule-system-rules").join("schemas");
        if candidate.is_dir() {
            return Some(candidate);
        }
        cur = dir.parent();
    }
    None
}

/// 把 schema 文本中指向 `../_meta/v1.0.json` 的 `$ref` 内联展开。
///
/// jsonschema 0.18 无法解析跨文件相对引用(无 registry 基建),而宪法采用
/// "agent_def allOf → _meta"的 SSOT 组合。此处读取 _meta 文档,把 ref 节点
/// 整体替换为其内容——JSON Schema 语义等价(allOf 成员本来就是一个完整
/// schema 对象),不改变 SSOT:_meta 仍是磁盘上的单一权威源,展开仅发生在
/// 运行时内存中。
fn inline_meta_ref(doc: &mut serde_json::Value, schemas_dir: &Path) -> Result<(), String> {
    let Some(obj) = doc.as_object_mut() else {
        return Ok(());
    };
    let Some(all_of) = obj.get_mut("allOf") else {
        return Ok(());
    };
    let Some(items) = all_of.as_array_mut() else {
        return Ok(());
    };
    let mut meta_doc: Option<serde_json::Value> = None;
    for item in items.iter() {
        if item.get("$ref").and_then(|r| r.as_str()) == Some("../_meta/v1.0.json") {
            let meta_path = schemas_dir.join("_meta/v1.0.json");
            let text = std::fs::read_to_string(&meta_path)
                .map_err(|e| format!("read {}: {}", meta_path.display(), e))?;
            let parsed: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| format!("parse _meta: {}", e))?;
            meta_doc = Some(parsed);
            break;
        }
    }
    if let Some(meta) = meta_doc {
        for item in items.iter_mut() {
            if item.get("$ref").and_then(|r| r.as_str()) == Some("../_meta/v1.0.json") {
                *item = meta.clone();
            }
        }
    }
    Ok(())
}

fn compile_schema(
    cell: &'static OnceLock<Option<JSONSchema>>,
    file: &str,
    kind_label: &str,
) -> Option<&'static JSONSchema> {
    cell.get_or_init(|| {
        let Some(dir) = locate_schemas_dir() else {
            tracing::warn!(
                kind = kind_label,
                "constitution schema not found (set EVORULE_SYSTEM_RULES or checkout as sibling dir); \
                 falling back to struct-level guards only"
            );
            return None;
        };
        let path = dir.join(file);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(kind = kind_label, path = %path.display(), error = %e,
                    "failed to read constitution schema; falling back to struct-level guards only");
                return None;
            }
        };
        let result = serde_json::from_str::<serde_json::Value>(&content)
            .map_err(|e| e.to_string())
            .and_then(|mut v| {
                inline_meta_ref(&mut v, &dir)?;
                JSONSchema::compile(&v).map_err(|e| e.to_string())
            });
        match result {
            Ok(schema) => Some(schema),
            Err(e) => {
                tracing::warn!(kind = kind_label, path = %path.display(), error = %e,
                    "failed to compile constitution schema; falling back to struct-level guards only");
                None
            }
        }
    })
    .as_ref()
}

/// 给裸 body 合成最小标注壳,使其满足 _meta 的 5 字段要求。
///
/// 运行时加载的 agents/*.json 是无壳 body(设计如此);宪法 schema 描述的
/// 是"发布形态"(带 5 标注字段)。发布形态合规由宪法仓扫描门禁
/// (scan_repo_json / check_evoagent_assets.py)负责;运行时在此以占位壳
/// 校验 **body 结构**,壳字段的治理合规不在加载器职责内。
fn shelve(body: &serde_json::Value, kind: &str, schema_url: &str) -> serde_json::Value {
    let mut doc = serde_json::Map::new();
    // 占位壳(schema 对壳只做存在性/基本类型检查)
    let raw_id = body
        .get("agent_type")
        .or_else(|| body.get("workflow_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("placeholder");
    // 消毒到 _meta id 语法([a-z][a-z0-9_]* 段):连字符等一律折叠为下划线
    let mut sanitized = String::with_capacity(raw_id.len());
    for c in raw_id.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.' {
            sanitized.push(c);
        } else {
            sanitized.push('_');
        }
    }
    let doc_id = format!("com.evoagent.runtime.{}", sanitized);
    doc.insert("$schema".into(), serde_json::json!(schema_url));
    doc.insert("kind".into(), serde_json::json!(kind));
    doc.insert("id".into(), serde_json::json!(doc_id));
    if body.get("version").is_none() {
        doc.insert("version".into(), serde_json::json!("0.0.0"));
    }
    doc.insert("metadata".into(), serde_json::json!({}));
    // body 字段随后(不覆盖占位壳已设键之外的任何内容)
    if let Some(obj) = body.as_object() {
        for (k, v) in obj {
            doc.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    serde_json::Value::Object(doc)
}

/// 用 agent_def v1.0 校验裸文档（无壳 body）。schema 不可用时 fail-fast 报错。
pub fn validate_agent_def(body: &serde_json::Value) -> Result<(), Vec<String>> {
    validate_with(
        &shelve(
            body,
            "agent_def",
            "https://evorule.org/schemas/agent_def/v1.0.json",
        ),
        compile_schema(&AGENT_DEF_SCHEMA, "agent_def/v1.0.json", "agent_def"),
        "agent_def",
    )
}

/// 用 workflow_dag v1.0 校验裸文档（无壳 body）。schema 不可用时 fail-fast 报错。
pub fn validate_workflow_dag(body: &serde_json::Value) -> Result<(), Vec<String>> {
    validate_with(
        &shelve(
            body,
            "workflow_dag",
            "https://evorule.org/schemas/workflow_dag/v1.0.json",
        ),
        compile_schema(
            &WORKFLOW_DAG_SCHEMA,
            "workflow_dag/v1.0.json",
            "workflow_dag",
        ),
        "workflow_dag",
    )
}

fn validate_with(
    doc: &serde_json::Value,
    schema: Option<&JSONSchema>,
    kind_label: &str,
) -> Result<(), Vec<String>> {
    let Some(schema) = schema else {
        // C12 fail-fast（审计⑥）: schema 缺失不再降级放行——门禁缺失意味着
        // 资产未经宪法校验就进入运行时。错误消息按系统自愈原则给出修复步骤。
        return Err(vec![format!(
            "constitution schema ({kind_label}) unavailable — refusing to load asset \
             without schema-level validation. Fix: (1) set EVORULE_SYSTEM_RULES to the \
             evorule-system-rules repo root, or (2) checkout evorule-system-rules as a \
             sibling directory of the executable; then restart."
        )]);
    };
    // jsonschema 0.18 API:JSONSchema::validate -> Result<(), ErrorIterator<ValidationError>>
    match schema.validate(doc) {
        Ok(()) => Ok(()),
        Err(err_iter) => {
            let errs: Vec<String> = err_iter
                .take(10)
                .map(|e| {
                    let path: String = e
                        .instance_path
                        .iter()
                        .map(|chunk| match chunk {
                            jsonschema::paths::PathChunk::Property(p) => p.to_string(),
                            jsonschema::paths::PathChunk::Index(i) => i.to_string(),
                            jsonschema::paths::PathChunk::Keyword(k) => (*k).to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join("/");
                    if path.is_empty() {
                        format!("{}", e)
                    } else {
                        format!("{}: {}", path, e)
                    }
                })
                .collect();
            Err(errs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_locate_schemas_dir_finds_constitution_repo() {
        match locate_schemas_dir() {
            Some(dir) => {
                assert!(dir.join("agent_def/v1.0.json").is_file());
                assert!(dir.join("_meta/v1.0.json").is_file());
                eprintln!("Located: {}", dir.display());
            }
            None => {
                panic!(
                    "schemas dir not found; exe={:?} — sibling-dir convention failed",
                    std::env::current_exe()
                );
            }
        }
    }

    #[test]
    fn test_validate_agent_def_rejects_bad_temperature_when_schema_available() {
        // 仅当能找到宪法仓时本测试才有意义；schema 不可用时走 fail-fast 分支（见下）
        let Some(_) = locate_schemas_dir() else {
            return;
        };
        let bad = serde_json::json!({
            "agent_type": "x", "version": "1", "description": "", "system_prompt": "s",
            "model": "m", "temperature": 99.0, "max_steps": 1,
            "step_timeout_secs": 1, "tools": []
        });
        assert!(validate_agent_def(&bad).is_err());
    }

    /// C12 fail-fast: schema 不可用时必须拒绝加载（不再降级放行 Ok）。
    /// 测试环境难注入"schema 必不可见"（OnceLock 全局 + exe 路径固定），
    /// 故以 locate 失败分支反证：locate None ⇒ validate 必 Err 且含修复指引。
    #[test]
    fn test_validate_fails_fast_when_schema_unavailable() {
        if locate_schemas_dir().is_some() {
            eprintln!("schemas dir located — fail-fast branch not reachable here, skipped");
            return;
        }
        let ok = serde_json::json!({
            "agent_type": "x", "version": "1", "description": "", "system_prompt": "s",
            "model": "m", "temperature": 0.3, "max_steps": 1,
            "step_timeout_secs": 1, "tools": []
        });
        let err =
            validate_agent_def(&ok).expect_err("fail-fast must reject when schema unavailable");
        assert!(
            err[0].contains("EVORULE_SYSTEM_RULES"),
            "错误须含修复指引: {err:?}"
        );
    }

    #[test]
    fn test_validate_workflow_rejects_empty_nodes_when_schema_available() {
        let Some(_) = locate_schemas_dir() else {
            return;
        };
        let bad = serde_json::json!({
            "workflow_id": "w", "nodes": [], "output_node": "x"
        });
        assert!(validate_workflow_dag(&bad).is_err());
    }

    #[test]
    fn test_validate_accepts_real_shapes() {
        // 结构形状正确时必须通过（无论是否降级）
        let ok_agent = serde_json::json!({
            "agent_type": "researcher", "version": "0.1.0", "description": "d",
            "system_prompt": "s", "model": "m", "temperature": 0.3,
            "max_steps": 20, "step_timeout_secs": 60,
            "tools": ["file_read"]
        });
        assert!(validate_agent_def(&ok_agent).is_ok());
        let ok_wf = serde_json::json!({
            "workflow_id": "wf", "description": "",
            "nodes": [{"id": "a", "agent_type": "researcher", "task": "t"}],
            "output_node": "a"
        });
        assert!(validate_workflow_dag(&ok_wf).is_ok());
    }
}
