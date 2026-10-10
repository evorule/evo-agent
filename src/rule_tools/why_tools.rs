// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 查账 why/order 工具（工具面统一架构 PR-11b）：因果查询面三件
//!
//! 相对 what 层（query_journal/query_trace/read_back/diff_runs，PR-11a），
//! 本组回答「为什么发生 / 谁导致谁 / 凭什么变成现在这样」：
//!
//! - `explain_denial`：给定会话内一次拒绝事实（Violation），返回结构化
//!   拒因——`rule_index`/`reason`/被拒命令原文 + 命中的规则正本条目
//!   （`GET /api/rules` 的 core_eval 数组按索引原样透出，对账即一致）；
//!   io_guard 输出门禁拒绝（rule_index 保留值）不走规则下标对账，改由
//!   事实自身携带的门禁命中记录（instruction）逐字透出，闭环自证；
//! - `causal_order`：同会话审计链内两事实的因果序——序由 cause 指针
//!   （链式哈希链）确定，**非墙钟**；无直接因果路径时按链位
//!   （logical_time，链上串行化序）定先后并如实标注 `causally_related:
//!   false`；跨链域（非同一会话审计链）不可比，返回域说明——诚实边界
//!   （设计档 §11.6 域边界裁定）；
//! - `lineage_of`：规则谱系两账拼接——版本链（workspace RuleVersionRecord）+
//!   晋升账（l2-inventory 投影的 `00_constraint_promoted_*`
//!   `promoted_from/promoted_at/promoted_by`，经 rule_version 锚与版本链对账）。
//!
//! 全部只读：仅 GET 审计链/规则正本/版本账，零写路径；server 侧零改动
//! （全部消费既有端点，尽调结论 2026-10-06）。
//!
//! 治理语义与 11a 四工具同型：Standard（免裁决但落账）+ AutoPolicy（免审）
//! + `default_switch.is_none()`（不绑开关）。

use serde_json::{json, Value};

use crate::api::evorule_client::EvoruleApiClient;
use crate::api::workspace_client::WorkspaceApiClient;
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};
use std::sync::Arc;

/// 解析 u64 参数（宽容数字/数字字符串，对齐 evolution_signals 先例口径）
fn parse_u64_arg(args: &Value, key: &str) -> Result<u64, String> {
    match args.get(key) {
        None | Some(Value::Null) => Err(format!("missing required parameter: {key}")),
        Some(Value::Number(n)) => n
            .as_u64()
            .ok_or_else(|| format!("{key} must be a non-negative integer")),
        Some(Value::String(s)) => s
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("{key} must be a valid integer string")),
        Some(_) => Err(format!("{key} must be a non-negative integer")),
    }
}

/// 必填字符串参数
fn require_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing required parameter: {key}"))
}

// =============================================================================
// explain_denial —— 拒因解释（结构化拒因 + 规则正本对账）
// =============================================================================

/// 审计条目中抽取 Violation 事实的归因字段（content_json 宽容形态：
/// 顶层或 payload 子对象；server 侧 Fact::to_json 的字段名以实测为准，
/// E2E 校准点）。
///
/// 返回 `(rule_index 显示值, 是否保留值, reason, cause)`。rule_index 解析
/// 宽容 u64/i64/整数字符串：保留值原样透出并标记 `reserved`——io_guard
/// 输出门禁 Violation 的 rule_index=u64::MAX 经 server 侧 TCB `J::integer`
/// (i64) 序列化落盘为 -1，两形态（-1 / 18446744073709551615）都不参与
/// core_eval 下标对账。
fn parse_rule_index(v: &Value) -> Option<(Value, bool)> {
    let str_int = |s: &str| {
        let s = s.trim();
        s.parse::<u64>()
            .ok()
            .map(|n| json!(n))
            .or_else(|| s.parse::<i64>().ok().map(|n| json!(n)))
    };
    match v {
        Value::Number(_) => {
            let reserved = v.as_u64() == Some(u64::MAX) || v.as_i64() == Some(-1);
            Some((v.clone(), reserved))
        }
        Value::String(s) => {
            let t = s.trim();
            let reserved = t == "-1" || t.parse::<u64>() == Ok(u64::MAX);
            Some((str_int(s)?, reserved))
        }
        _ => None,
    }
}

fn violation_probe(probe: &Value) -> Option<(Value, bool, String, Option<u64>)> {
    let (rule_index, reserved) = parse_rule_index(probe.get("rule_index")?)?;
    let reason = probe
        .get("reason")
        .and_then(|v| v.as_str())
        .map(str::to_string)?;
    let cause = probe
        .get("cause")
        .and_then(|v| v.as_u64())
        .or_else(|| probe.get("cause").and_then(|v| v.as_str()?.parse().ok()));
    Some((rule_index, reserved, reason, cause))
}

fn violation_fields(content: &Value) -> Option<(Value, bool, String, Option<u64>)> {
    violation_probe(content).or_else(|| violation_probe(&content["payload"]))
}

fn is_violation_type(fact_type: &str) -> bool {
    fact_type.to_ascii_lowercase().contains("violation")
}

/// WAL 重载链兜底：审计条目 fact_type 失真（重载映射抹平为非违规型名）时，
/// content_json.type 仍保留原始型别——以内容型别补充甄别。
fn content_type_is_violation(content: &Value) -> bool {
    content
        .get("type")
        .and_then(|v| v.as_str())
        .is_some_and(is_violation_type)
}

pub struct ExplainDenialTool {
    ev: EvoruleApiClient,
}

impl ExplainDenialTool {
    pub fn new(ev: EvoruleApiClient) -> Self {
        Self { ev }
    }
}

#[async_trait::async_trait]
impl ToolFunction for ExplainDenialTool {
    async fn call(&self, args: &Value) -> IoResult {
        let session_id = parse_u64_arg(args, "session_id")?;
        let fact_id = parse_u64_arg(args, "fact_id")?;
        let session_str = session_id.to_string();

        let report = self
            .ev
            .get_audit_report_with_content(&session_str)
            .await
            .map_err(|e| format!("audit report fetch failed: {e}"))?;
        let entries = report
            .get("entries")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "audit report has no entries array".to_string())?;

        // 1. 定位 Violation 事实
        let target = entries
            .iter()
            .find(|e| e.get("fact_id").and_then(|v| v.as_u64()) == Some(fact_id))
            .ok_or_else(|| {
                format!("fact {fact_id} not found in session {session_id} audit chain")
            })?;
        let fact_type = target
            .get("fact_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content = target.get("content_json").cloned().unwrap_or(Value::Null);
        // 双字段甄别：live 审计路径 fact_type 直接携带违规型别；WAL 重载/
        // 归档还原路径经治理静态事实型表映射会把 fact_type 抹平，而
        // content_json.type 保留原始型别——两字段取或，重载链上的拒绝
        // 事实不被误拒，非违规事实（含被抹平的普通事实）仍如实拒绝。
        if !is_violation_type(fact_type) && !content_type_is_violation(&content) {
            return Err(format!(
                "fact {fact_id} in session {session_id} is a {fact_type:?} fact, not a \
                 violation — explain_denial only explains denial (Violation) facts"
            ));
        }
        let (rule_index, reserved_index, reason, cause) =
            violation_fields(&content).ok_or_else(|| {
                format!(
                    "violation fact {fact_id} lacks structured rule_index/reason fields \
                     (content shape mismatch — server-side fact shape needs calibration)"
                )
            })?;

        // 2. 归因对账：常规拒绝 = core_eval[rule_index] 原样透出（GET /api/rules）；
        // 保留值拒绝（io_guard 输出门禁，rule_index=u64::MAX 经 TCB i64 序列化
        // 落盘为 -1）= 不拉规则正本——门禁命中记录由事实自身 instruction 携带
        // （系统独占发射，闭环自证），逐字透出即为权威归因。
        let (matched_rule, reconciliation, note) = if reserved_index {
            let instr = content.get("instruction").cloned().unwrap_or(Value::Null);
            let gate_record = instr.get("type").and_then(|v| v.as_str()) == Some("io_guard");
            (
                Some(instr),
                gate_record,
                if gate_record {
                    "rule_index is the reserved sentinel for the io_guard output gate \
                     (u64::MAX; serialized as -1 on the audit chain) — not a core_eval rule \
                     index. matched_rule is the gate's own hit record carried verbatim in \
                     the fact's instruction (system-exclusive emission, self-contained; \
                     domain/phrase/mode under params). Reconcile the domain against the \
                     deployed feature table 00_constraint_io_guard.json."
                } else {
                    "rule_index is a reserved sentinel value but the fact carries no io_guard \
                     instruction record — matched_rule absent; check the emitting mechanism"
                },
            )
        } else {
            let rules = self
                .ev
                .get_rules()
                .await
                .map_err(|e| format!("rules fetch failed: {e}"))?;
            let core_eval = rules
                .get("core_eval")
                .and_then(|v| v.as_array())
                .ok_or_else(|| "GET /api/rules response has no core_eval array".to_string())?;
            let matched = rule_index
                .as_u64()
                .and_then(|i| core_eval.get(i as usize))
                .cloned();
            let reconciliation = matched.is_some();
            (
                matched,
                reconciliation,
                if reconciliation {
                    "matched_rule is the verbatim core_eval entry from GET /api/rules (canonical \
                     effective rule set, indexed by the engine's rule_index)"
                } else {
                    "rule_index points outside the current core_eval array (rule set reloaded since \
                     the denial?) — matched_rule absent, cross-check the rule set version"
                },
            )
        };

        // 3. 被拒命令原文（cause 指向的事实）
        let denied_command = cause.and_then(|cid| {
            entries.iter().find_map(|e| {
                (e.get("fact_id").and_then(|v| v.as_u64()) == Some(cid))
                    .then(|| e.get("content_json").cloned().unwrap_or(Value::Null))
            })
        });

        Ok(json!({
            "session_id": session_id,
            "fact_id": fact_id,
            "violation": {
                "rule_index": rule_index,
                "reason": reason,
                "cause": cause,
            },
            "matched_rule": matched_rule,
            "reconciliation": reconciliation,
            "denied_command": denied_command,
            "note": note,
        }))
    }
}

// =============================================================================
// causal_order —— 两事实因果序（链式哈希序，非墙钟；限同链域内）
// =============================================================================

fn entry_fact_id(e: &Value) -> Option<u64> {
    e.get("fact_id").and_then(|v| v.as_u64())
}

fn entry_cause(e: &Value) -> Option<u64> {
    e.get("cause")
        .and_then(|v| v.as_u64())
        .or_else(|| e.get("cause").and_then(|v| v.as_str()?.parse().ok()))
}

fn entry_logical_time(e: &Value) -> u64 {
    e.get("logical_time").and_then(|v| v.as_u64()).unwrap_or(0)
}

pub struct CausalOrderTool {
    ev: EvoruleApiClient,
}

impl CausalOrderTool {
    pub fn new(ev: EvoruleApiClient) -> Self {
        Self { ev }
    }
}

#[async_trait::async_trait]
impl ToolFunction for CausalOrderTool {
    async fn call(&self, args: &Value) -> IoResult {
        let session_id = parse_u64_arg(args, "session_id")?;
        let fact_a = parse_u64_arg(args, "fact_a")?;
        let fact_b = parse_u64_arg(args, "fact_b")?;
        let session_str = session_id.to_string();

        // 单次拉取全链审计报告（含 content）。cause 解析 = 条目级 cause 优先，
        // content_json.cause 兜底——实测（PR-11b E2E, session 607）Violation
        // 事实的因果指针只在 content 层透出（governance auditor 条目级 cause
        // 对 Violation 为 null），两级取或即覆盖全形态。
        let report = self
            .ev
            .get_audit_report_with_content(&session_str)
            .await
            .map_err(|e| format!("audit report fetch failed: {e}"))?;
        let entries = report
            .get("entries")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "audit report has no entries array".to_string())?;

        let cause_of = |e: &Value| -> Option<u64> {
            entry_cause(e).or_else(|| {
                let c = e.get("content_json").and_then(|c| c.get("cause"))?;
                c.as_u64().or_else(|| c.as_str()?.parse().ok())
            })
        };

        // 域边界（设计档 §11.6）：任一事实不在本会话链上 → 不可比 + 域说明
        let has_a = entries.iter().any(|e| entry_fact_id(e) == Some(fact_a));
        let has_b = entries.iter().any(|e| entry_fact_id(e) == Some(fact_b));
        if !has_a || !has_b {
            return Ok(json!({
                "session_id": session_id,
                "fact_a": fact_a,
                "fact_b": fact_b,
                "comparable": false,
                "reason": "one or both facts are absent from this session's audit chain — order \
                           is defined only within a single session's hash-chained audit chain; \
                           facts from other sessions/domains (agent journal seq, other WAL \
                           chains) are NOT comparable to it",
            }));
        }

        // 因果路径：从 X 沿 cause 指针回溯（祖先链；visited 防环）
        let ancestors_of = |start: u64| -> Vec<u64> {
            let mut path = Vec::new();
            let mut cur = Some(start);
            while let Some(id) = cur {
                if path.contains(&id) {
                    break;
                }
                path.push(id);
                cur = entries
                    .iter()
                    .find(|e| entry_fact_id(e) == Some(id))
                    .and_then(cause_of);
            }
            path
        };
        let path_a = ancestors_of(fact_a); // [a, parent(a), ..., genesis]
        let path_b = ancestors_of(fact_b);

        let (order, middle, causally_related) = if path_a.contains(&fact_b) && fact_a != fact_b {
            // b 是 a 的祖先：b → … → a
            let pos = path_a.iter().position(|&x| x == fact_b).unwrap_or(0);
            let mut seg: Vec<u64> = path_a[..=pos].to_vec();
            seg.reverse(); // [b, ..., a]
            ("b_before_a", Some(seg), true)
        } else if path_b.contains(&fact_a) {
            let pos = path_b.iter().position(|&x| x == fact_a).unwrap_or(0);
            let mut seg: Vec<u64> = path_b[..=pos].to_vec();
            seg.reverse(); // [a, ..., b]
            ("a_before_b", Some(seg), true)
        } else {
            // 无直接因果路径：按链位（logical_time = 链上串行化位置）定先后
            let ta = entries
                .iter()
                .find(|e| entry_fact_id(e) == Some(fact_a))
                .map(entry_logical_time)
                .unwrap_or(0);
            let tb = entries
                .iter()
                .find(|e| entry_fact_id(e) == Some(fact_b))
                .map(entry_logical_time)
                .unwrap_or(0);
            let ord = if ta <= tb { "a_before_b" } else { "b_before_a" };
            (ord, None, false)
        };

        Ok(json!({
            "session_id": session_id,
            "fact_a": fact_a,
            "fact_b": fact_b,
            "comparable": true,
            "order": order,
            "middle_chain": middle,
            "causally_related": causally_related,
            "basis": "hash-chained audit chain: direct causal path via cause pointers when one \
                      exists, otherwise chain position (logical_time assigned by the serialized \
                      engine) — never wall-clock timestamps",
        }))
    }
}

// =============================================================================
// lineage_of —— 规则谱系（版本链 + 晋升账两账拼接）
// =============================================================================

const PROMOTED_FROM_PREFIX: &str = "rule_version:";

pub struct LineageOfTool {
    ws: WorkspaceApiClient,
    ev: EvoruleApiClient,
}

impl LineageOfTool {
    pub fn new(ws: WorkspaceApiClient, ev: EvoruleApiClient) -> Self {
        Self { ws, ev }
    }
}

#[async_trait::async_trait]
impl ToolFunction for LineageOfTool {
    async fn call(&self, args: &Value) -> IoResult {
        let workspace_id = require_str(args, "workspace_id")?;
        let rule_id = require_str(args, "rule_id")?;

        // 账一：规则主状态 + 版本链（workspace 正本）
        let rule = self
            .ws
            .get_rule(&workspace_id, &rule_id)
            .await
            .map_err(|e| format!("rule fetch failed: {e}"))?;
        let rule_json = serde_json::to_value(&rule).map_err(|e| e.to_string())?;
        let versions = self
            .ws
            .list_rule_versions(&workspace_id, &rule_id)
            .await
            .map_err(|e| format!("rule versions fetch failed: {e}"))?;

        // 账二：晋升账（l2-inventory 投影的晋升条目；join 锚 =
        // promoted_from 的 rule_version:<版本id> 命中版本链任一版本 id。
        // core_eval 节点为引擎执行语义投影，不携带 id/metadata——晋升账
        // 权威在 L2 约束文件 metadata，经 l2-inventory 透出）
        let l2 = self
            .ev
            .get_l2_inventory()
            .await
            .map_err(|e| format!("l2 inventory fetch failed: {e}"))?;
        let promoted_entry = l2
            .get("files")
            .and_then(|v| v.as_array())
            .and_then(|files| {
                files.iter().find(|f| {
                    f.get("promoted_from")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.strip_prefix(PROMOTED_FROM_PREFIX))
                        .is_some_and(|vid| versions.iter().any(|v| v.id == vid))
                })
            })
            .cloned()
            .unwrap_or(Value::Null);
        let promotion = if promoted_entry.is_null() {
            None
        } else {
            Some(json!({
                "promoted_from": promoted_entry.get("promoted_from").cloned().unwrap_or(Value::Null),
                "promoted_at": promoted_entry.get("promoted_at").cloned().unwrap_or(Value::Null),
                "promoted_by": promoted_entry.get("promoted_by").cloned().unwrap_or(Value::Null),
            }))
        };
        // 晋升源版本 id（promoted_from: "rule_version:<id>"）→ 版本链对齐标记
        let promoted_version_id = promotion.as_ref().and_then(|p| {
            p.get("promoted_from")
                .and_then(|v| v.as_str())
                .and_then(|s| s.strip_prefix(PROMOTED_FROM_PREFIX))
                .map(|s| s.to_string())
        });

        let version_chain: Vec<Value> = versions
            .iter()
            .map(|v| {
                json!({
                    "id": v.id,
                    "version": v.version,
                    "state": v.state,
                    "created_by": v.created_by,
                    "created_at": v.created_at,
                    "content_hash": v.content_hash,
                    "promotion_origin": promoted_version_id.as_deref() == Some(v.id.as_str()),
                })
            })
            .collect();

        Ok(json!({
            "workspace_id": workspace_id,
            "rule_id": rule_id,
            "rule_state": rule_json.get("state").cloned().unwrap_or(Value::Null),
            "version_chain": version_chain,
            "promotion": promotion,
            "notes": match (&promotion, promoted_version_id) {
                (None, _) => vec![
                    "no promoted constraint entry anchors to any version of this rule in the L2 \
                     promotion ledger — either not promoted or promoted from a version outside \
                     this rule's version chain",
                ],
                (Some(_), None) => vec![
                    "promotion entry found but its promoted_from carries no rule_version anchor",
                ],
                (Some(_), Some(_)) => vec![],
            },
        }))
    }
}

// =============================================================================
// register / specs
// =============================================================================

/// 注册 why/order 三工具（full_rule_toolkit 装配点）
pub fn register(h: &mut ToolHandler, ws: &WorkspaceApiClient, ev: &EvoruleApiClient) {
    h.register_static(
        "explain_denial",
        Arc::new(ExplainDenialTool::new(ev.clone())),
    );
    h.register_static("causal_order", Arc::new(CausalOrderTool::new(ev.clone())));
    h.register_static(
        "lineage_of",
        Arc::new(LineageOfTool::new(ws.clone(), ev.clone())),
    );
}

/// why/order 三工具 spec（LLM 可见面）
pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "explain_denial".to_string(),
            description: "Explain why a command was denied in a session: locate the Violation \
                          fact on the session's hash-chained audit chain and return the \
                          structured denial (rule_index, reason), the verbatim effective rule \
                          entry it maps to (reconciled against GET /api/rules), and the denied \
                          command's original content. Output-gate (io_guard) denials carry a \
                          reserved rule_index and are explained via the gate's own hit record \
                          carried in the fact itself. Read-only."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "session_id".to_string(),
                    r#type: "integer".to_string(),
                    description: "Server session id (integer or integer string).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "fact_id".to_string(),
                    r#type: "integer".to_string(),
                    description: "Fact id of the denial (Violation) fact on the audit chain. \
                                  Fails honestly if the fact exists but is not a violation."
                        .to_string(),
                    required: true,
                },
            ],
        },
        ToolSpec {
            name: "causal_order".to_string(),
            description: "Determine the causal order of two facts within one session's \
                          hash-chained audit chain. Order comes from cause pointers (direct \
                          causal path, with the intermediate chain segment returned) or, when \
                          no causal path exists, from chain position (logical_time assigned by \
                          the serialized engine) — never wall-clock timestamps. Facts outside \
                          this session's chain are reported as not comparable (domain \
                          boundary). Read-only."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "session_id".to_string(),
                    r#type: "integer".to_string(),
                    description: "Server session id (integer or integer string).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "fact_a".to_string(),
                    r#type: "integer".to_string(),
                    description: "First fact id.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "fact_b".to_string(),
                    r#type: "integer".to_string(),
                    description: "Second fact id.".to_string(),
                    required: true,
                },
            ],
        },
        ToolSpec {
            name: "lineage_of".to_string(),
            description: "Return the full lineage of a rule by stitching two ledgers: the \
                          version chain (workspace rule versions with state/author/hash, \
                          newest first) and the promotion ledger (the promoted constraint \
                          entry's promoted_from/promoted_at/promoted_by from the effective \
                          rule set, with the promoted version marked in the chain). Read-only."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace id owning the rule.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "rule_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Rule id (e.g. com.evorule.constraint.safety).".to_string(),
                    required: true,
                },
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::Arc;

    // —— 进程内顺序应答式 HTTP 假服务（同 service_tools.rs tests 先例）——

    fn spawn_http_fixture(responses: Vec<(u16, String)>) -> String {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let (stream, _) = listener.accept().unwrap();
                let mut stream = stream;
                let mut req_body;
                {
                    let mut reader = BufReader::new(&stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let _path = line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    let mut content_length = 0usize;
                    loop {
                        let mut h = String::new();
                        reader.read_line(&mut h).unwrap();
                        if h.trim().is_empty() {
                            break;
                        }
                        if let Ok(v) = h
                            .to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .unwrap_or("")
                            .trim()
                            .parse::<usize>()
                        {
                            content_length = v;
                        }
                    }
                    if content_length > 0 {
                        req_body = vec![0u8; content_length];
                        reader.read_exact(&mut req_body).unwrap();
                    }
                }
                let reason = if status == 200 { "OK" } else { "Error" };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(resp.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });
        format!("http://{addr}")
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn audit_entries_json() -> String {
        // 审计链 fixture：7=IoRequest（被拒命令）/8=Violation（rule_index=2,
        // reason, cause=7）/9=后续事实；logical_time 链上串行化位置
        r#"{
          "session_id": 42, "fact_count": 3, "last_hash": "aa..ff", "verified": true,
          "entries": [
            {"fact_id": 7, "fact_type": "io_request", "logical_time": 7,
             "content_hash": "h7", "prev_hash": "h6", "cause": 6,
             "content_json": {"kind": "io_request", "tool": "shell_exec", "params": {"command": "rm -rf /"}}},
            {"fact_id": 8, "fact_type": "violation", "logical_time": 8,
             "content_hash": "h8", "prev_hash": "h7", "cause": 7,
             "content_json": {"kind": "violation", "rule_index": 2, "reason": "blocked by seed constraint", "cause": 7}},
            {"fact_id": 9, "fact_type": "io_response", "logical_time": 9,
             "content_hash": "h9", "prev_hash": "h8", "cause": 8,
             "content_json": {"kind": "io_response", "result": "halted"}}
          ]
        }"#.to_string()
    }

    fn rules_json() -> String {
        r#"{
          "count": 3,
          "core_eval": [
            {"id": "com.evorule.seed.zero", "metadata": {"id": "com.evorule.seed.zero", "tier": "seed"}},
            {"id": "com.evorule.seed.one", "metadata": {"id": "com.evorule.seed.one", "tier": "seed"}},
            {"id": "com.evorule.constraint.safety", "transform": [{"type": "guard"}],
             "metadata": {"id": "com.evorule.constraint.safety", "tier": "constraint",
                          "promoted_from": "rule_version:01M3BDCC39Y5FQVGYHSE034R36",
                          "promoted_at": "2026-09-25T04:33:05Z", "promoted_by": "console"}}
          ],
          "tiers": {}
        }"#.to_string()
    }

    // =========================================================================
    // explain_denial
    // =========================================================================

    #[test]
    fn explain_denial_reconciles_rule_with_canonical_source() {
        // 对账锚：matched_rule 必须是 GET /api/rules core_eval[rule_index]
        // 条目逐字透传（设计验收：规则 id/版本须与治理正本一致）
        let base = spawn_http_fixture(vec![(200, audit_entries_json()), (200, rules_json())]);
        let rt = rt();
        let handler = ToolHandler::new();
        let mut handler = handler;
        handler.register_static(
            "explain_denial",
            Arc::new(ExplainDenialTool::new(EvoruleApiClient::new(&base))),
        );
        let out = rt
            .block_on(
                handler
                    .execute_by_name("explain_denial", &json!({"session_id": "42", "fact_id": 8})),
            )
            .unwrap();

        assert_eq!(out["violation"]["rule_index"], 2);
        assert_eq!(out["violation"]["reason"], "blocked by seed constraint");
        assert_eq!(out["violation"]["cause"], 7);
        // 对账：与正本 core_eval[2] 逐字一致
        let rules: Value = serde_json::from_str(&rules_json()).unwrap();
        assert_eq!(
            out["matched_rule"], rules["core_eval"][2],
            "matched_rule must be the verbatim canonical entry"
        );
        assert_eq!(out["reconciliation"], true);
        // 被拒命令原文抽取（cause=7）
        assert_eq!(out["denied_command"]["tool"], "shell_exec");
        assert_eq!(out["denied_command"]["params"]["command"], "rm -rf /");
    }

    #[test]
    fn explain_denial_non_violation_fails_honestly() {
        let base = spawn_http_fixture(vec![(200, audit_entries_json())]);
        let rt = rt();
        let mut handler = ToolHandler::new();
        handler.register_static(
            "explain_denial",
            Arc::new(ExplainDenialTool::new(EvoruleApiClient::new(&base))),
        );
        let err = rt
            .block_on(
                handler
                    .execute_by_name("explain_denial", &json!({"session_id": "42", "fact_id": 7})),
            )
            .unwrap_err();
        assert!(
            err.contains("not a violation"),
            "应如实拒绝非违规事实: {err}"
        );
    }

    #[test]
    fn explain_denial_accepts_violation_whose_fact_type_was_lost_on_reload() {
        // WAL 重载/归档还原镜像：治理静态事实型表无违规型变体，重载链上
        // 拒绝事实的 fact_type 被抹平——双字段甄别以 content_json.type
        // 为准放行，拒因三字段照常解析。
        let entries = json!([
            {"fact_id": 5, "fact_type": "Unknown", "logical_time": 5, "cause": null,
             "content_json": {"type": "Command", "id": 5}},
            {"fact_id": 6, "fact_type": "Unknown", "logical_time": 6, "cause": null,
             "content_json": {"type": "Violation", "id": 6, "cause": 5,
                              "rule_index": 1, "reason": "reloaded denial"}}
        ]);
        let base = spawn_http_fixture(vec![(200, audit_report_resp(entries)), (200, rules_json())]);
        let rt = rt();
        let mut handler = ToolHandler::new();
        handler.register_static(
            "explain_denial",
            Arc::new(ExplainDenialTool::new(EvoruleApiClient::new(&base))),
        );
        let out = rt
            .block_on(
                handler
                    .execute_by_name("explain_denial", &json!({"session_id": "42", "fact_id": 6})),
            )
            .unwrap();

        assert_eq!(out["violation"]["rule_index"], 1);
        assert_eq!(out["violation"]["reason"], "reloaded denial");
        assert_eq!(out["violation"]["cause"], 5);
    }

    #[test]
    fn explain_denial_still_rejects_non_violation_whose_fact_type_was_lost() {
        // 甄别不是对失真型名无条件放行：内容型别同样不是违规（如被抹平的
        // 普通指令事实）时仍如实拒绝。
        let entries = json!([
            {"fact_id": 5, "fact_type": "Unknown", "logical_time": 5, "cause": null,
             "content_json": {"type": "Command", "id": 5}}
        ]);
        let base = spawn_http_fixture(vec![(200, audit_report_resp(entries))]);
        let rt = rt();
        let mut handler = ToolHandler::new();
        handler.register_static(
            "explain_denial",
            Arc::new(ExplainDenialTool::new(EvoruleApiClient::new(&base))),
        );
        let err = rt
            .block_on(
                handler
                    .execute_by_name("explain_denial", &json!({"session_id": "42", "fact_id": 5})),
            )
            .unwrap_err();
        assert!(err.contains("not a violation"), "应如实拒绝: {err}");
    }

    #[test]
    fn explain_denial_io_guard_reserved_index_reads_gate_record() {
        // io_guard 输出门禁 Violation：rule_index 保留值（u64::MAX 经 TCB i64
        // 序列化落盘为 -1）+ instruction 携带门禁自产命中记录（domain/phrase/
        // mode）——matched_rule 逐字透出 instruction（闭环自证），不误导为
        // 「规则集重载」；保留值分支不依赖 GET /api/rules。
        let entries = json!([
            {"fact_id": 7, "fact_type": "io_response", "logical_time": 7, "cause": 6,
             "content_json": {"type": "IoResponse", "id": 7}},
            {"fact_id": 8, "fact_type": "violation", "logical_time": 8, "cause": 7,
             "content_json": {"type": "Violation", "id": 8, "cause": 7, "rule_index": -1,
              "reason": "输出门禁命中：收尾文本含「已执行」动作特征（shell_exec 域）",
              "instruction": {"type": "io_guard", "params": {
                  "domain": "shell_exec", "phrase": "已执行", "request_id": 5, "mode": "observe"}}}}
        ]);
        let base = spawn_http_fixture(vec![(200, audit_report_resp(entries))]);
        let rt = rt();
        let mut handler = ToolHandler::new();
        handler.register_static(
            "explain_denial",
            Arc::new(ExplainDenialTool::new(EvoruleApiClient::new(&base))),
        );
        let out = rt
            .block_on(
                handler
                    .execute_by_name("explain_denial", &json!({"session_id": "42", "fact_id": 8})),
            )
            .unwrap();

        assert_eq!(out["violation"]["rule_index"], -1);
        assert_eq!(out["matched_rule"]["type"], "io_guard");
        assert_eq!(out["matched_rule"]["params"]["domain"], "shell_exec");
        assert_eq!(out["matched_rule"]["params"]["phrase"], "已执行");
        assert_eq!(out["matched_rule"]["params"]["mode"], "observe");
        assert_eq!(out["reconciliation"], true);
        assert!(out["note"].as_str().unwrap().contains("reserved sentinel"));
        // 被拒命令原文抽取（cause=7）不受分支影响
        assert_eq!(out["denied_command"]["type"], "IoResponse");
    }

    #[test]
    fn explain_denial_io_guard_reserved_index_unsigned_max_form() {
        // 保留值无符号大数形态（18446744073709551615）同样走门禁分支——
        // 数字面 -1 与 u64::MAX 两形态等价处理。
        let entries = json!([
            {"fact_id": 9, "fact_type": "Violation", "logical_time": 9, "cause": null,
             "content_json": {"type": "Violation", "id": 9, "cause": null,
              "rule_index": 18446744073709551615u64,
              "reason": "输出门禁命中（enforce）",
              "instruction": {"type": "io_guard", "params": {"domain": "file_write"}}}}
        ]);
        let base = spawn_http_fixture(vec![(200, audit_report_resp(entries))]);
        let rt = rt();
        let mut handler = ToolHandler::new();
        handler.register_static(
            "explain_denial",
            Arc::new(ExplainDenialTool::new(EvoruleApiClient::new(&base))),
        );
        let out = rt
            .block_on(
                handler
                    .execute_by_name("explain_denial", &json!({"session_id": "42", "fact_id": 9})),
            )
            .unwrap();

        assert_eq!(out["reconciliation"], true);
        assert_eq!(out["matched_rule"]["params"]["domain"], "file_write");
    }

    #[test]
    fn explain_denial_reserved_index_without_gate_record_reports_honestly() {
        // 保留值但事实缺 io_guard instruction 记录：不强行归因，matched_rule
        // 缺席 + note 如实说明。
        let entries = json!([
            {"fact_id": 4, "fact_type": "Violation", "logical_time": 4, "cause": null,
             "content_json": {"type": "Violation", "id": 4, "cause": null,
                              "rule_index": -1, "reason": "unknown reserved denial"}}
        ]);
        let base = spawn_http_fixture(vec![(200, audit_report_resp(entries))]);
        let rt = rt();
        let mut handler = ToolHandler::new();
        handler.register_static(
            "explain_denial",
            Arc::new(ExplainDenialTool::new(EvoruleApiClient::new(&base))),
        );
        let out = rt
            .block_on(
                handler
                    .execute_by_name("explain_denial", &json!({"session_id": "42", "fact_id": 4})),
            )
            .unwrap();

        assert_eq!(out["matched_rule"], Value::Null);
        assert_eq!(out["reconciliation"], false);
        assert!(out["note"].as_str().unwrap().contains("no io_guard"));
    }

    // =========================================================================
    // causal_order
    // =========================================================================

    fn audit_report_resp(entries: Value) -> String {
        json!({
            "session_id": 42, "fact_count": entries.as_array().map(|a| a.len()).unwrap_or(0),
            "last_hash": "aa..ff", "verified": true, "entries": entries
        })
        .to_string()
    }

    #[test]
    fn causal_order_ancestor_path_determines_order_not_wallclock() {
        // 链式哈希序锚定：7 是 9 的祖先（cause 指针），序=7 before 9——
        // 即使响应携带的任何时间类字段被交换，序仍由 cause 指针决定
        let entries = json!([
            {"fact_id": 9, "fact_type": "io_response", "logical_time": 9, "cause": 8},
            {"fact_id": 8, "fact_type": "violation", "logical_time": 8, "cause": 7},
            {"fact_id": 7, "fact_type": "io_request", "logical_time": 7, "cause": 6}
        ]);
        let base = spawn_http_fixture(vec![(200, audit_report_resp(entries))]);
        let rt = rt();
        let mut handler = ToolHandler::new();
        handler.register_static(
            "causal_order",
            Arc::new(CausalOrderTool::new(EvoruleApiClient::new(&base))),
        );
        let out = rt
            .block_on(handler.execute_by_name(
                "causal_order",
                &json!({"session_id": "42", "fact_a": 9, "fact_b": 7}),
            ))
            .unwrap();

        assert_eq!(out["comparable"], true);
        assert_eq!(out["order"], "b_before_a");
        assert_eq!(out["causally_related"], true);
        assert_eq!(out["middle_chain"], json!([7, 8, 9]));
        assert!(out["basis"].as_str().unwrap().contains("never wall-clock"));
    }

    #[test]
    fn causal_order_no_causal_path_falls_back_to_chain_position() {
        // 平行事实（互不为祖先，条目级与内容级均无 cause）：按链位
        // （logical_time）定先后并如实标注 causally_related:false
        let entries = json!([
            {"fact_id": 11, "fact_type": "io_request", "logical_time": 11, "cause": null},
            {"fact_id": 10, "fact_type": "io_request", "logical_time": 10, "cause": null}
        ]);
        let base = spawn_http_fixture(vec![(200, audit_report_resp(entries))]);
        let rt = rt();
        let mut handler = ToolHandler::new();
        handler.register_static(
            "causal_order",
            Arc::new(CausalOrderTool::new(EvoruleApiClient::new(&base))),
        );
        let out = rt
            .block_on(handler.execute_by_name(
                "causal_order",
                &json!({"session_id": "42", "fact_a": 11, "fact_b": 10}),
            ))
            .unwrap();

        assert_eq!(out["comparable"], true);
        assert_eq!(out["order"], "b_before_a"); // logical_time 10 < 11
        assert_eq!(out["causally_related"], false);
        assert_eq!(out["middle_chain"], Value::Null);
    }

    #[test]
    fn causal_order_absent_fact_reports_honest_domain_boundary() {
        // 诚实边界（设计档 §11.6 域边界裁定）：事实不在本会话链上 → 不可比 + 域说明
        let entries = json!([
            {"fact_id": 9, "fact_type": "io_response", "logical_time": 9, "cause": 8}
        ]);
        let base = spawn_http_fixture(vec![(200, audit_report_resp(entries))]);
        let rt = rt();
        let mut handler = ToolHandler::new();
        handler.register_static(
            "causal_order",
            Arc::new(CausalOrderTool::new(EvoruleApiClient::new(&base))),
        );
        let out = rt
            .block_on(handler.execute_by_name(
                "causal_order",
                &json!({"session_id": "42", "fact_a": 9, "fact_b": 999}),
            ))
            .unwrap();

        assert_eq!(out["comparable"], false);
        let reason = out["reason"].as_str().unwrap();
        assert!(reason.contains("NOT comparable"), "域说明在场: {reason}");
    }

    #[test]
    fn causal_order_violation_cause_falls_back_to_content_layer() {
        // PR-11b E2E 实测校准（session 607）：Violation 事实的条目级 cause
        // 为 null（governance auditor extract_cause 不覆盖 Violation），因果
        // 指针只在 content_json.cause 透出——兜底解析后祖先路径成立，
        // 30125（被拒指令）before 7（Violation），causally_related:true。
        let entries = json!([
            {"fact_id": 7, "fact_type": "Violation", "logical_time": 45,
             "cause": null,
             "content_json": {"type": "Violation", "id": 7, "cause": 30125,
                              "rule_index": 20, "reason": "shell_exec danger_hits"}},
            {"fact_id": 30125, "fact_type": "Command", "logical_time": 43,
             "cause": null,
             "content_json": {"type": "Command", "id": 30125}}
        ]);
        let base = spawn_http_fixture(vec![(200, audit_report_resp(entries))]);
        let rt = rt();
        let mut handler = ToolHandler::new();
        handler.register_static(
            "causal_order",
            Arc::new(CausalOrderTool::new(EvoruleApiClient::new(&base))),
        );
        let out = rt
            .block_on(handler.execute_by_name(
                "causal_order",
                &json!({"session_id": "42", "fact_a": 30125, "fact_b": 7}),
            ))
            .unwrap();

        assert_eq!(out["comparable"], true);
        assert_eq!(out["order"], "a_before_b");
        assert_eq!(out["causally_related"], true);
        assert_eq!(out["middle_chain"], json!([30125, 7]));
    }

    // =========================================================================
    // lineage_of
    // =========================================================================

    fn versions_json() -> String {
        r#"[
          {"id": "01M3BDCC39Y5FQVGYHSE034R36", "rule_id": "com.evorule.constraint.safety",
           "version": 2, "content_hash": "sha256:bbb", "content": "{}",
           "state": "current", "created_by": "agent", "created_at": "2026-09-25T04:30:00Z"},
          {"id": "01M3BDCC39YAAAAAAAAAAAAAAA", "rule_id": "com.evorule.constraint.safety",
           "version": 1, "content_hash": "sha256:aaa", "content": "{}",
           "state": "superseded", "created_by": "agent", "created_at": "2026-09-24T10:00:00Z"}
        ]"#
        .to_string()
    }

    fn rule_record_json() -> String {
        // 对齐 workspace_client::RuleRecord 全字段（非 Option 字段必须在场）
        r#"{"id": "com.evorule.constraint.safety", "workspace_id": "ws1", "name": "协作验收规则",
            "current_version_id": "01M3BDCC39Y5FQVGYHSE034R36", "state": "active",
            "description": null, "created_by": "agent", "created_at": "2026-09-24T10:00:00Z",
            "updated_at": "2026-09-25T04:33:05Z", "archived_at": null, "metadata": "{}"}"#
            .to_string()
    }

    fn l2_inventory_json() -> String {
        // l2-inventory 投影 fixture：晋升条目 promoted_from 锚定版本链 id
        r#"{
          "count": 1,
          "files": [
            {"path": "00_constraint_promoted_65ad7d1b1910a98e.json",
             "title": "协作验收规则（边界强制 + 实施前置 + 核收前置）",
             "guard_for": [],
             "promoted_from": "rule_version:01M3BDCC39Y5FQVGYHSE034R36",
             "promoted_at": "2026-09-25T04:33:05Z",
             "promoted_by": "console"}
          ]
        }"#
        .to_string()
    }

    #[test]
    fn lineage_of_stitches_version_chain_and_promotion_ledger() {
        // 两账拼接：版本链（workspace）+ 晋升账（l2-inventory 晋升条目），
        // promoted_from 锚对齐版本 id → promotion_origin 标记
        let base = spawn_http_fixture(vec![
            (200, rule_record_json()),
            (200, versions_json()),
            (200, l2_inventory_json()),
        ]);
        let rt = rt();
        let mut handler = ToolHandler::new();
        handler.register_static(
            "lineage_of",
            Arc::new(LineageOfTool::new(
                WorkspaceApiClient::new(&base),
                EvoruleApiClient::new(&base),
            )),
        );
        let out = rt
            .block_on(handler.execute_by_name(
                "lineage_of",
                &json!({"workspace_id": "ws1", "rule_id": "com.evorule.constraint.safety"}),
            ))
            .unwrap();

        assert_eq!(out["rule_state"], "active");
        let chain = out["version_chain"].as_array().unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0]["promotion_origin"], true); // promoted_from 锚命中
        assert_eq!(chain[1]["promotion_origin"], false);
        assert_eq!(out["promotion"]["promoted_by"], "console");
        assert_eq!(
            out["promotion"]["promoted_from"],
            "rule_version:01M3BDCC39Y5FQVGYHSE034R36"
        );
        assert!(out["notes"].as_array().unwrap().is_empty());
    }

    #[test]
    fn lineage_of_unpromoted_rule_states_it_honestly() {
        // 未晋升规则：promotion=null + notes 如实说明
        let base = spawn_http_fixture(vec![
            (200, rule_record_json()),
            (200, versions_json()),
            (
                200,
                r#"{"count": 0, "files": []}"#.to_string(), // l2-inventory 无晋升条目
            ),
        ]);
        let rt = rt();
        let mut handler = ToolHandler::new();
        handler.register_static(
            "lineage_of",
            Arc::new(LineageOfTool::new(
                WorkspaceApiClient::new(&base),
                EvoruleApiClient::new(&base),
            )),
        );
        let out = rt
            .block_on(handler.execute_by_name(
                "lineage_of",
                &json!({"workspace_id": "ws1", "rule_id": "com.evorule.other.unpromoted"}),
            ))
            .unwrap();

        assert_eq!(out["promotion"], Value::Null);
        assert!(out["version_chain"].as_array().unwrap().len() == 2);
        let notes = out["notes"].as_array().unwrap();
        assert!(notes
            .iter()
            .any(|n| n.as_str().unwrap().contains("not promoted")));
    }

    #[test]
    fn lineage_of_promotion_anchor_outside_version_chain_reports_honestly() {
        // 晋升条目在场但其 promoted_from 锚不指向本规则版本链任一版本 id
        // → 不强行拼接，promotion=null + notes 如实说明
        let base = spawn_http_fixture(vec![
            (200, rule_record_json()),
            (200, versions_json()),
            (
                200,
                r#"{
                  "count": 1,
                  "files": [
                    {"path": "00_constraint_promoted_other.json", "title": "他者约束",
                     "guard_for": [],
                     "promoted_from": "rule_version:01OTHERAAAAAAAAAAAAAAAAAAAA",
                     "promoted_at": "2026-09-26T00:00:00Z", "promoted_by": "console"}
                  ]
                }"#
                .to_string(),
            ),
        ]);
        let rt = rt();
        let mut handler = ToolHandler::new();
        handler.register_static(
            "lineage_of",
            Arc::new(LineageOfTool::new(
                WorkspaceApiClient::new(&base),
                EvoruleApiClient::new(&base),
            )),
        );
        let out = rt
            .block_on(handler.execute_by_name(
                "lineage_of",
                &json!({"workspace_id": "ws1", "rule_id": "com.evorule.constraint.safety"}),
            ))
            .unwrap();

        assert_eq!(out["promotion"], Value::Null);
        let notes = out["notes"].as_array().unwrap();
        assert!(notes
            .iter()
            .any(|n| n.as_str().unwrap().contains("version chain")));
    }

    // =========================================================================
    // manifest 治理定性（与 11a 同型断言）
    // =========================================================================

    #[test]
    fn manifest_governance_fields_for_why_tools() {
        // 快照锁随动（PR-11b）：静态计数 20/49/74 由
        // tool_manifest::test_static_manifest_count_locked 锁守；此处锁
        // rule_tool_specs 侧 3 spec 在场 + manifest 治理定性
        // （Standard+AutoPolicy+无开关——不绑开关、不进快照）
        let specs = super::super::rule_tool_specs();
        for name in ["explain_denial", "causal_order", "lineage_of"] {
            let spec = specs
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("spec '{name}' must be in rule_tool_specs"));
            assert!(!spec.description.is_empty());
            assert!(!spec.parameters.is_empty());
            let m = crate::agent::tool_manifest::lookup_static(name)
                .unwrap_or_else(|| panic!("manifest '{name}' must be in static table"));
            assert_eq!(
                m.adjudication_class,
                crate::agent::tool_manifest::AdjudicationClass::Standard
            );
            assert_eq!(
                m.approval_policy,
                crate::agent::tool_manifest::ApprovalPolicy::AutoPolicy
            );
            assert!(m.default_switch.is_none(), "why/order tools bind no switch");
        }
    }
}
