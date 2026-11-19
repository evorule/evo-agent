// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 自主交接双工具（handover_write / handover_read）——会话间状态的结构化读写。
//!
//! 交接档=新会话的聚焦信号重注入：会话收尾时 LLM 调 `handover_write` 落
//! 结构化交接档，续接会话首动作 `handover_read` 读档核对（交接叙事档 +
//! query_journal 事实账双源互证）。
//!
//! 落点与 schema（自主交接设计 §3.2）：
//! - 交接档落 evorule shared facts `shared.{namespace}.handovers.{id}`，
//!   `id = handover-YYYYMMDD-HHMMSS`（UTC，系统生成；字典序=时间序）；
//! - LLM 必填 6 字段（goal/done/todo_next/anchors/env_state/verification），
//!   缺项/空值 fail-visible 一次列明全部缺项后拒写；handover_version /
//!   id / parent_session 系统填（LLM 无权指定，防身份漂移）；
//! - `handover_read` 返回交接档+完整性校验（缺字段 → `missing_fields`
//!   警示，fail-visible 警示不拒读——读侧审计面暴露残档）。
//!
//! 注册形态（查账工具族同构）：占位执行体启动期注册（default_safe_toolkit，
//! 未接线时调用 fail-visible 如实报错），runner 会话期重绑（session_id /
//! namespace / client 在手）——开关过滤（agentTools.handover）/声明收紧/
//! LLM 契约三面自动工作，零特殊通路。Standard 免裁决 + AutoPolicy；每次
//! 成功写入落 journal `handover_written` 事件（runner 侧按工具名分支）。

use serde_json::{json, Value};

use crate::agent::journal::{JournalEvent, JournalLine};
use crate::agent::writeoff::FreshnessState;
use crate::api::evorule_client::EvoruleApiClient;
use crate::io_handlers::tool_handler::ToolFunction;

/// 交接写工具名
pub const HANDOVER_WRITE_TOOL: &str = "handover_write";
/// 交接读工具名
pub const HANDOVER_READ_TOOL: &str = "handover_read";

/// 交接档 schema 必填字段（LLM 面；handover_version/id/parent_session 系统填不列）
pub const HANDOVER_REQUIRED_FIELDS: &[&str] = &[
    "goal",
    "done",
    "todo_next",
    "anchors",
    "env_state",
    "verification",
];

/// 交接档结构校验（纯函数）：返回缺失/空值必填字段列表（空 = 通过）
///
/// 判空口径：字符串 trim 后非空；数组/对象非空；其余类型视为缺。
pub fn handover_schema_missing(handover: &Value) -> Vec<String> {
    let mut missing = Vec::new();
    for field in HANDOVER_REQUIRED_FIELDS {
        let present = match handover.get(*field) {
            Some(Value::String(s)) => !s.trim().is_empty(),
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::Object(o)) => !o.is_empty(),
            _ => false,
        };
        if !present {
            missing.push((*field).to_string());
        }
    }
    missing
}

/// unix 秒 → UTC 时间戳串 `YYYYMMDD-HHMMSS`（确定性儒略日算法，无外部依赖）
fn utc_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    // Howard Hinnant civil_from_days（公有域算法；与 memory_tool 日期族同源）
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    let d = doy - (153 * mp + 2) / 5 + 1;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 参数抽取 + schema 校验（fail-visible：缺/空必填一次列明全部缺项后拒写）
fn extract_handover_fields(args: &Value) -> Result<Value, String> {
    let mut missing: Vec<String> = Vec::new();
    let string_field = |name: &str, missing: &mut Vec<String>| -> Option<String> {
        args.get(name)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                missing.push(name.to_string());
                None
            })
    };
    let string_list_field = |name: &str, missing: &mut Vec<String>| -> Option<Vec<String>> {
        match args.get(name).and_then(|v| v.as_array()) {
            Some(items) if !items.is_empty() => Some(
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
            ),
            _ => {
                missing.push(name.to_string());
                None
            }
        }
    };
    let object_field = |name: &str, missing: &mut Vec<String>| -> Option<Value> {
        match args.get(name).and_then(|v| v.as_object()) {
            Some(o) if !o.is_empty() => Some(Value::Object(o.clone())),
            _ => {
                missing.push(name.to_string());
                None
            }
        }
    };
    let goal = string_field("goal", &mut missing);
    let done = string_list_field("done", &mut missing);
    let todo_next = string_list_field("todo_next", &mut missing);
    let anchors = object_field("anchors", &mut missing);
    let env_state = object_field("env_state", &mut missing);
    let verification = string_field("verification", &mut missing);
    if !missing.is_empty() {
        return Err(format!(
            "handover_write: missing/empty required field(s): {} — a handover without \
             them is not a handover (schema fail-visible); fill them and retry",
            missing.join("、")
        ));
    }
    Ok(json!({
        "goal": goal,
        "done": done,
        "todo_next": todo_next,
        "anchors": anchors,
        "env_state": env_state,
        "verification": verification,
    }))
}

/// 交接档文档组装（纯函数）：系统字段（handover_version/id/parent_session）
/// + LLM 六字段。返回 (fact_path, id, doc)。
fn compose_handover_doc(
    namespace: &str,
    session_id: &str,
    secs: u64,
    fields: Value,
) -> (String, String, Value) {
    let id = format!("handover-{}", utc_stamp(secs));
    let path = format!("shared.{namespace}.handovers.{id}");
    let mut doc = json!({
        "handover_version": "1.0",
        "id": id,
        "parent_session": session_id,
    });
    if let (Some(dst), Some(src)) = (doc.as_object_mut(), fields.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    let doc_id = doc["id"].as_str().unwrap_or_default().to_string();
    (path, doc_id, doc)
}

/// `handover_write` 执行体（schema fail-visible → 系统组装 → 落 shared facts）
pub(crate) async fn handover_write_exec(
    namespace: &str,
    client: &EvoruleApiClient,
    session_id: &str,
    args: &Value,
) -> Result<Value, String> {
    let fields = extract_handover_fields(args)?;
    let (path, id, doc) = compose_handover_doc(namespace, session_id, now_secs(), fields);
    client
        .update_payload(session_id, &path, &doc)
        .await
        .map_err(|e| format!("handover_write: persist failed ({e})"))?;
    Ok(json!({
        "status": "ok",
        "path": path,
        "id": id,
        "schema_ok": true,
        "note": "handover recorded; the continuation session's first action should be \
                 handover_read and verify the verification criterion",
    }))
}

/// `handover_read` 执行体（缺省最新 / 指定 id；完整性校验警示不拒读）
pub(crate) async fn handover_read_exec(
    namespace: &str,
    client: &EvoruleApiClient,
    args: &Value,
) -> Result<Value, String> {
    let prefix = format!("shared.{namespace}.handovers.");
    let facts = client
        .get_shared_facts(Some(&prefix))
        .await
        .map_err(|e| format!("handover_read: ledger unreachable ({e})"))?;
    if facts.is_empty() {
        return Ok(json!({
            "status": "empty",
            "note": "no handover documents found for this namespace",
        }));
    }
    let requested = args
        .get("handover_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // id 构词（时间戳）保证字典序=时间序：缺省取字典序最大；指定 id 精确匹配
    let picked = match requested {
        Some(want) => facts
            .iter()
            .find(|f| f.path.rsplit('.').next().is_some_and(|t| t == want))
            .ok_or_else(|| format!("handover_read: handover '{want}' not found"))?,
        None => facts
            .iter()
            .max_by(|a, b| a.path.cmp(&b.path))
            .ok_or_else(|| "handover_read: ledger empty".to_string())?,
    };
    let id = picked
        .path
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_string();
    let missing = handover_schema_missing(&picked.value);
    let mut out = json!({
        "status": "ok",
        "id": id,
        "path": picked.path,
        "handover": picked.value,
    });
    if !missing.is_empty() {
        out["missing_fields"] = json!(missing);
        out["warning"] = json!(
            "handover completeness check failed (missing mandatory fields) — treat the \
             remaining fields with care; fail-visible warning, not a refusal to read"
        );
    }
    Ok(out)
}

/// 交接底座包驱动侧生成器（Z-B109-E01 选项①）：从本 namespace 交接档
/// shared facts 查最新一档，确定性映射为 `HandoffPackage`（纯查表，零 LLM）。
///
/// 语义：底座层（确定性）与滚动摘要（语义面）分层配对——定义未声明
/// handoff 时由此补位，消灭「摘要注入中而底座缺位」的单腿降级。
/// fail-soft：无档/残档/账本不可达 = 返回 None（调用方保持未声明态，
/// 装配期 pairing 断言照旧 warn——诚实降级，不静默造底座）。
pub(crate) async fn handoff_package_from_latest_handover(
    namespace: &str,
    client: &EvoruleApiClient,
) -> Option<crate::agent::definition::HandoffPackage> {
    let prefix = format!("shared.{namespace}.handovers.");
    let facts = client.get_shared_facts(Some(&prefix)).await.ok()?;
    let doc = latest_complete_handover_doc(&facts)?;
    Some(handover_doc_to_package(doc))
}

/// 交接档选取（纯函数）：字典序最大 id（构词保证=时间序最新）且 schema
/// 完整（残档不采用——读工具面已 fail-visible 警示残档，底座层宁缺毋滥）。
fn latest_complete_handover_doc(
    facts: &[crate::api::evorule_client::SharedFactEntry],
) -> Option<&Value> {
    facts
        .iter()
        .max_by(|a, b| a.path.cmp(&b.path))
        .filter(|f| handover_schema_missing(&f.value).is_empty())
        .map(|f| &f.value)
}

/// 交接档 → 底座包确定性映射（纯函数）：
/// - `goal` → goal（schema 已保证非空）；
/// - `done` → milestone_current（字符串直取；数组取末项=最近完成）；
/// - `todo_next` → next_step（字符串直取；数组取首项=下一动作）；
/// - `anchors` → verified_facts（数组逐条；对象展平 `k = v`；上限 20 条
///   防巨锚爆底座）；
/// - `env_state`/`verification` 无底座对应槽，不映射（底座只收可复用面，
///   环境快照属当程语境）；`dead_ends` 交接档 schema 无此面，保持空——
///   失败教训回喂走双通道笔记既有通道，不在此重复。
fn handover_doc_to_package(doc: &Value) -> crate::agent::definition::HandoffPackage {
    let non_empty_str = |v: Option<&Value>| {
        v.and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let milestone_current = non_empty_str(doc.get("done")).or_else(|| {
        doc.get("done")
            .and_then(|x| x.as_array())
            .and_then(|a| a.last())
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    });
    let next_step = non_empty_str(doc.get("todo_next")).or_else(|| {
        doc.get("todo_next")
            .and_then(|x| x.as_array())
            .and_then(|a| a.first())
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    });
    let verified_facts = match doc.get("anchors") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .take(20)
            .map(str::to_string)
            .collect(),
        Some(Value::Object(map)) => map
            .iter()
            .take(20)
            .map(|(k, v)| match v.as_str() {
                Some(s) => format!("{k} = {s}"),
                None => format!("{k} = {v}"),
            })
            .collect(),
        _ => Vec::new(),
    };
    crate::agent::definition::HandoffPackage {
        goal: non_empty_str(doc.get("goal")).unwrap_or_default(),
        milestone_current,
        next_step,
        verified_facts,
        dead_ends: Vec::new(),
    }
}

// ===== Checkpoint-block mechanization (long-horizon write-off batch) =====
//
// Three pieces, all extensions of the existing handover surface (zero new
// paradigm): the machine-projected block draft, the two-level freshness
// gate, and the replay projection for the checkpointed journal event.
// Layering philosophy is the established one: the deterministic base layer
// projects facts from the journal, the model supplies only the semantic
// face; gate truth stays runner-computed and the model never overrides
// machine-derived facts.

/// Anchor cap for the machine-projected anchors face (same quota as the
/// handoff-package mapping above: a giant checkpoint trail must not bloat
/// the block; when the window exceeds the cap the oldest entries drop and
/// the newest hashes win per path).
pub const CHECKPOINT_ANCHOR_CAP: usize = 20;

/// Level-1 soft guidance appended to the tool result when the write-off
/// projection finds the breakpoint block stale (fail-visible, visible to
/// the model on the next round; same injection style as the stagnation
/// marks). The text stays free of double quotes/backslashes so it can be
/// interpolated into JSON tool-result envelopes unescaped.
pub const CHECKPOINT_STALE_GUIDANCE: &str = "[checkpoint stale: the breakpoint block lags the latest completed todo transition - write handover_write with all six fields before submitting task_done again; todo_next must pin the exact next command]";

/// Level-2 escalation after a repeated stale finding: the write-off gate
/// keeps refusing task_done, so the guidance names the honest exit.
pub const CHECKPOINT_STALE_ESCALATED: &str = "[checkpoint stale, escalated: the write-off gate has refused task_done more than once over a stale breakpoint - write handover_write now (six fields; todo_next pins the next command) or close out honestly via task_blocked]";

/// Machine-projected checkpoint window (pure projection over journal
/// lines; same input always yields a byte-identical draft).
#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointWindow {
    /// Window number: turn_seq of the newest turn boundary (the window is
    /// the current goal progress unit; lines before the boundary are out of
    /// scope, mirroring the write-off window discipline).
    pub window: u64,
    /// Todo item ids that reached the completed class inside the window
    /// (first-transition order, deduplicated).
    pub completed_items: Vec<String>,
    /// Checkpoint-block draft: the machine-derivable fact fields of the
    /// six-field block (goal / done / anchors / verification). The semantic
    /// fields (todo_next / env_state) are never invented by the machine.
    pub draft: Value,
}

/// Project the checkpoint-block draft from journal lines (pure, zero IO,
/// zero clock reads; deterministic by construction).
///
/// - `goal` = the newest turn boundary's goal;
/// - `done` = completed-class todo transitions in the window (first
///   transition order, latest content per item) followed by verify-run
///   verdict summaries in chain order;
/// - `anchors` = file checkpoints as path→hash (latest hash per path,
///   capped at [`CHECKPOINT_ANCHOR_CAP`], oldest dropped);
/// - `verification` = the newest verify verdict summary in the window, or
///   an explicit unknown marker when the window holds no verify run
///   (unknown is never dressed up as a pass).
///
/// `None` = no turn boundary in the journal: no window, no draft.
pub fn project_checkpoint_window(lines: &[JournalLine]) -> Option<CheckpointWindow> {
    let start_idx = lines
        .iter()
        .rposition(|l| matches!(l.event, JournalEvent::TurnStarted { .. }))?;
    let (window, goal) = match &lines[start_idx].event {
        JournalEvent::TurnStarted { turn_seq, goal } => (*turn_seq, goal.clone()),
        _ => unreachable!("rposition matched TurnStarted"),
    };
    let mut completed_items: Vec<String> = Vec::new();
    let mut completed_content: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut verify_entries: Vec<String> = Vec::new();
    let mut latest_verify: Option<String> = None;
    let mut anchors: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut anchor_order: Vec<String> = Vec::new();
    for line in &lines[start_idx..] {
        match &line.event {
            JournalEvent::TodoUpdated { items } => {
                for item in items {
                    if item.status == "completed" {
                        if !completed_items.contains(&item.id) {
                            completed_items.push(item.id.clone());
                        }
                        completed_content.insert(item.id.clone(), item.content.clone());
                    }
                }
            }
            JournalEvent::VerifyRan {
                name,
                verdict,
                exit_code,
                ..
            } => {
                let exit = exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "n/a".to_string());
                verify_entries.push(format!("verify {name}: {verdict} (exit {exit})"));
                latest_verify = Some(format!("{verdict} (exit {exit})"));
            }
            JournalEvent::FileCheckpointTaken { path, hash, .. } => {
                if !anchors.contains_key(path) {
                    anchor_order.push(path.clone());
                }
                anchors.insert(path.clone(), hash.clone());
            }
            _ => {}
        }
    }
    while anchor_order.len() > CHECKPOINT_ANCHOR_CAP {
        let oldest = anchor_order.remove(0);
        anchors.remove(&oldest);
    }
    let mut done: Vec<String> = completed_items
        .iter()
        .filter_map(|id| {
            completed_content
                .get(id)
                .map(|content| format!("completed: {content}"))
        })
        .collect();
    done.extend(verify_entries);
    let draft = json!({
        "goal": goal,
        "done": done,
        "anchors": anchors,
        "verification": latest_verify
            .unwrap_or_else(|| "unknown: no verify run in window".to_string()),
    });
    Some(CheckpointWindow {
        window,
        completed_items,
        draft,
    })
}

/// Merge result of the checkpoint-block write gate.
#[derive(Debug, Clone, PartialEq)]
pub struct MergedCheckpointBlock {
    /// The complete six-field block (schema-complete by construction).
    pub fields: Value,
    /// Fact fields the machine draft supplied (machine-prefilled
    /// provenance; reported back so the model sees what it did not write).
    pub machine_prefilled: Vec<&'static str>,
}

/// Merge the machine draft (fact face) with the model's args into a
/// schema-complete six-field checkpoint block (pure; fail-visible).
///
/// Field-authority boundary:
/// - fact fields (`goal` / `done` / `anchors` / `verification`): the
///   machine draft wins whenever it can derive the field (non-empty);
///   model-supplied values for machine-derived fields are discarded — the
///   model never overrides machine-computed truth (same split as the
///   runner-filled acceptance flag). Only fields the machine could NOT
///   derive may be filled from the args;
/// - semantic fields (`todo_next` / `env_state`): model-only — the machine
///   never invents them; missing or empty semantic fields refuse the write
///   with every gap listed at once (fail-visible).
pub fn merge_checkpoint_block(
    draft: &Value,
    args: &Value,
) -> Result<MergedCheckpointBlock, String> {
    let mut missing: Vec<String> = Vec::new();
    let mut machine_prefilled: Vec<&'static str> = Vec::new();
    let draft_str = |name: &str| -> Option<String> {
        draft
            .get(name)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let draft_list = |name: &str| -> Option<Vec<String>> {
        draft.get(name).and_then(|v| v.as_array()).and_then(|a| {
            if a.is_empty() {
                None
            } else {
                Some(
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect(),
                )
            }
        })
    };
    let draft_obj = |name: &str| -> Option<Value> {
        match draft.get(name).and_then(|v| v.as_object()) {
            Some(o) if !o.is_empty() => Some(Value::Object(o.clone())),
            _ => None,
        }
    };
    let arg_str = |name: &str| -> Option<String> {
        args.get(name)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let arg_list = |name: &str| -> Option<Vec<String>> {
        match args.get(name).and_then(|v| v.as_array()) {
            Some(items) if !items.is_empty() => Some(
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
            ),
            _ => None,
        }
    };
    let arg_obj = |name: &str| -> Option<Value> {
        match args.get(name).and_then(|v| v.as_object()) {
            Some(o) if !o.is_empty() => Some(Value::Object(o.clone())),
            _ => None,
        }
    };
    // Semantic face: model-only — the machine never invents these.
    let todo_next = arg_list("todo_next");
    if todo_next.is_none() {
        missing.push("todo_next".to_string());
    }
    let env_state = arg_obj("env_state");
    if env_state.is_none() {
        missing.push("env_state".to_string());
    }
    // Fact face: machine draft wins; args fill only undrivable gaps.
    let goal = draft_str("goal");
    let goal = if goal.is_some() {
        machine_prefilled.push("goal");
        goal
    } else {
        arg_str("goal")
    };
    if goal.is_none() {
        missing.push("goal".to_string());
    }
    let done = draft_list("done");
    let done = if done.is_some() {
        machine_prefilled.push("done");
        done
    } else {
        arg_list("done")
    };
    if done.is_none() {
        missing.push("done".to_string());
    }
    let anchors = draft_obj("anchors");
    let anchors = if anchors.is_some() {
        machine_prefilled.push("anchors");
        anchors
    } else {
        arg_obj("anchors")
    };
    if anchors.is_none() {
        missing.push("anchors".to_string());
    }
    let verification = draft_str("verification");
    let verification = if verification.is_some() {
        machine_prefilled.push("verification");
        verification
    } else {
        arg_str("verification")
    };
    if verification.is_none() {
        missing.push("verification".to_string());
    }
    if !missing.is_empty() {
        return Err(format!(
            "handover_write: checkpoint block incomplete - missing/empty field(s): {} - the machine draft covers only journal-derivable facts and the semantic face is model-only; fill the listed fields and retry",
            missing.join("、")
        ));
    }
    Ok(MergedCheckpointBlock {
        fields: json!({
            "goal": goal,
            "done": done,
            "todo_next": todo_next,
            "anchors": anchors,
            "env_state": env_state,
            "verification": verification,
        }),
        machine_prefilled,
    })
}

/// `handover_write` execution body in long-horizon mode: merge the machine
/// draft with the model args (fail-visible), then persist as usual. The ok
/// response names the machine-prefilled fact fields (provenance is
/// observable, not silent substitution).
async fn handover_write_exec_checkpoint(
    namespace: &str,
    client: &EvoruleApiClient,
    session_id: &str,
    draft: &Value,
    args: &Value,
) -> Result<Value, String> {
    let merged = merge_checkpoint_block(draft, args)?;
    let schema_gaps = handover_schema_missing(&merged.fields);
    if !schema_gaps.is_empty() {
        return Err(format!(
            "handover_write: merged checkpoint block failed the schema check (missing: {}) - refusing to persist an incomplete block",
            schema_gaps.join("、")
        ));
    }
    let (path, id, doc) = compose_handover_doc(namespace, session_id, now_secs(), merged.fields);
    client
        .update_payload(session_id, &path, &doc)
        .await
        .map_err(|e| format!("handover_write: persist failed ({e})"))?;
    Ok(json!({
        "status": "ok",
        "path": path,
        "id": id,
        "schema_ok": true,
        "checkpoint_prefilled": merged.machine_prefilled,
        "note": "checkpoint block recorded; fact fields marked checkpoint_prefilled were projected by the runner from the journal, the semantic fields are model-supplied",
    }))
}

/// Two-level soft-guidance verdict of the checkpoint-freshness gate (same
/// shape as the stagnation two-level form: first violation warns, a repeat
/// escalates; a fresh window resets the count; an unknowable face neither
/// counts nor resets — unknowable is the hard gate's face, not a nag).
#[derive(Debug)]
pub enum FreshnessGuidance {
    /// Fresh window (or unknowable face): no guidance.
    None,
    /// First stale finding: level-1 guidance mark.
    Warn(&'static str),
    /// Repeated stale finding: level-2 escalation mark.
    Escalate(&'static str),
}

/// Checkpoint-freshness gate (long-horizon mode). Observed at the task_done
/// write-off gate with the journal-projected freshness piece; a stale or
/// missing breakpoint produces a fail-visible guidance mark appended to the
/// rejection feedback, and the hard refusal itself stays with the write-off
/// conjunction (this gate only guides, it never admits).
#[derive(Default)]
pub struct FreshnessGate {
    violations: u32,
}

impl FreshnessGate {
    /// 创建检测器（零历史违例）
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe one freshness projection at the task_done gate.
    pub fn observe(&mut self, freshness: FreshnessState) -> FreshnessGuidance {
        match freshness {
            FreshnessState::Fresh => {
                self.violations = 0;
                FreshnessGuidance::None
            }
            FreshnessState::Unknown => FreshnessGuidance::None,
            FreshnessState::Stale | FreshnessState::Missing => {
                self.violations += 1;
                if self.violations == 1 {
                    FreshnessGuidance::Warn(CHECKPOINT_STALE_GUIDANCE)
                } else {
                    FreshnessGuidance::Escalate(CHECKPOINT_STALE_ESCALATED)
                }
            }
        }
    }

    /// Violations since the last fresh window (wiring/test observability).
    pub fn violation_count(&self) -> u32 {
        self.violations
    }
}

/// Rebuilt checkpoint-block state from the newest writeoff checkpoint
/// event (latest wins, same replay rule as the plan-loop checkpoint
/// family; the digest is verified against the carried draft before the
/// state is handed out — a corrupted line refuses the rebuild).
#[derive(Debug, Clone, PartialEq)]
pub struct WriteoffCheckpointState {
    /// Window number carried by the event.
    pub window: u64,
    /// Completed-transition list carried by the event.
    pub completed_items: Vec<String>,
    /// The six-field block draft as recorded (fact face; semantic fields
    /// were model-supplied at write time and are not reconstructed here).
    pub block: Value,
    /// Digest of the draft as recorded.
    pub block_digest: String,
}

/// Replay the newest writeoff checkpoint event from journal lines (pure;
/// latest wins). `Ok(None)` = no checkpoint event in the journal. A digest
/// mismatch or an unparsable draft fails visibly — a corrupted checkpoint
/// line must never degrade into a silently wrong rebuild.
pub fn replay_writeoff_checkpoint(
    lines: &[JournalLine],
) -> Result<Option<WriteoffCheckpointState>, String> {
    let mut latest: Option<(&JournalLine, u64, &Vec<String>, &str, &str)> = None;
    for line in lines {
        if let JournalEvent::WriteoffCheckpointed {
            window,
            completed_items,
            block_draft,
            block_digest,
        } = &line.event
        {
            latest = Some((line, *window, completed_items, block_draft, block_digest));
        }
    }
    let Some((_, window, completed_items, block_draft, block_digest)) = latest else {
        return Ok(None);
    };
    let recomputed = crate::agent::journal::evorule_digest(block_draft);
    if recomputed != block_digest {
        return Err(format!(
            "writeoff checkpoint replay refused: block digest mismatch (recorded {block_digest}, recomputed {recomputed}) - the checkpoint line is corrupted; refusing the rebuild instead of continuing on a wrong breakpoint"
        ));
    }
    let block: Value = serde_json::from_str(block_draft).map_err(|e| {
        format!("writeoff checkpoint replay refused: block draft is not valid JSON ({e})")
    })?;
    Ok(Some(WriteoffCheckpointState {
        window,
        completed_items: completed_items.clone(),
        block,
        block_digest: block_digest.to_string(),
    }))
}

/// `handover_write` 执行器（占位/接线双态；会话期重绑——session_id 在手）
pub struct HandoverWriteTool {
    namespace: Option<String>,
    client: Option<EvoruleApiClient>,
    session_id: Option<String>,
    /// Long-horizon checkpoint-block draft provider (None = mode off, the
    /// legacy full-six-field contract applies unchanged — default
    /// invariant). When wired, the runner supplies a closure projecting the
    /// machine fact face from the session journal at call time; the model
    /// then only fills the semantic fields (see
    /// [`merge_checkpoint_block`]).
    writeoff_draft: Option<std::sync::Arc<dyn Fn() -> Option<Value> + Send + Sync>>,
}

impl HandoverWriteTool {
    /// 占位构造（启动期 default_safe_toolkit 注册；调用 fail-visible 报错）
    pub fn unwired() -> Self {
        Self {
            namespace: None,
            client: None,
            session_id: None,
            writeoff_draft: None,
        }
    }

    /// 接线构造（runner 会话期重绑；wire_accounting 同构）
    pub fn wired(namespace: String, client: EvoruleApiClient, session_id: String) -> Self {
        Self {
            namespace: Some(namespace),
            client: Some(client),
            session_id: Some(session_id),
            writeoff_draft: None,
        }
    }

    /// Long-horizon mode wiring: attach the machine draft provider (the
    /// runner builds it from the session journal; absence of a draft at
    /// call time fails visibly instead of writing a fact-less block).
    pub fn with_writeoff_draft(
        mut self,
        provider: std::sync::Arc<dyn Fn() -> Option<Value> + Send + Sync>,
    ) -> Self {
        self.writeoff_draft = Some(provider);
        self
    }

    fn parts(&self) -> Result<(&str, &EvoruleApiClient, &str), String> {
        match (&self.namespace, &self.client, &self.session_id) {
            (Some(ns), Some(c), Some(sid)) => Ok((ns, c, sid)),
            _ => Err(format!(
                "{}: not wired to a session yet (registered as a startup placeholder; \
                 it is re-bound once a run session exists)",
                HANDOVER_WRITE_TOOL
            )),
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for HandoverWriteTool {
    fn is_wired(&self) -> bool {
        self.namespace.is_some() && self.client.is_some()
    }

    async fn call(&self, args: &Value) -> Result<Value, String> {
        let (namespace, client, session_id) = self.parts()?;
        if let Some(provider) = &self.writeoff_draft {
            let draft = provider().ok_or_else(|| {
                format!(
                    "{HANDOVER_WRITE_TOOL}: machine checkpoint draft unavailable (journal \
                     disabled, unreadable, or no turn window) - the fact fields cannot be \
                     established without it; write refused (fail-visible)"
                )
            })?;
            return handover_write_exec_checkpoint(namespace, client, session_id, &draft, args)
                .await;
        }
        handover_write_exec(namespace, client, session_id, args).await
    }
}

/// `handover_read` 执行器（占位/接线双态）
pub struct HandoverReadTool {
    namespace: Option<String>,
    client: Option<EvoruleApiClient>,
}

impl HandoverReadTool {
    /// 占位构造（启动期 default_safe_toolkit 注册；调用 fail-visible 报错）
    pub fn unwired() -> Self {
        Self {
            namespace: None,
            client: None,
        }
    }

    /// 接线构造（runner 会话期重绑）
    pub fn wired(namespace: String, client: EvoruleApiClient) -> Self {
        Self {
            namespace: Some(namespace),
            client: Some(client),
        }
    }

    fn parts(&self) -> Result<(&str, &EvoruleApiClient), String> {
        match (&self.namespace, &self.client) {
            (Some(ns), Some(c)) => Ok((ns, c)),
            _ => Err(format!(
                "{}: not wired to a session yet (registered as a startup placeholder; \
                 it is re-bound once a run session exists)",
                HANDOVER_READ_TOOL
            )),
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for HandoverReadTool {
    fn is_wired(&self) -> bool {
        self.namespace.is_some() && self.client.is_some()
    }

    async fn call(&self, args: &Value) -> Result<Value, String> {
        let (namespace, client) = self.parts()?;
        handover_read_exec(namespace, client, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::evorule_client::SharedFactEntry;

    fn fact(path: &str, value: Value) -> SharedFactEntry {
        SharedFactEntry {
            fact_id: 0,
            path: path.to_string(),
            value,
            source_session_id: 0,
            version: 1,
            origin_fact_id: None,
        }
    }

    fn complete_doc() -> Value {
        json!({
            "goal": "deliver v0.6.0", "done": "CI chain green",
            "todo_next": "upload installer",
            "anchors": {"main_sha": "4be4bc8"}, "env_state": {"os": "win"},
            "verification": "smoke passed",
        })
    }

    #[test]
    fn latest_complete_handover_picks_newest_and_rejects_incomplete() {
        let older = fact(
            "shared.demo.handovers.handover-20261102-000000",
            complete_doc(),
        );
        let newer_incomplete = fact(
            "shared.demo.handovers.handover-20261103-120000",
            json!({ "goal": "g" }), // 残档:缺 5 必填
        );
        let newest_complete = fact(
            "shared.demo.handovers.handover-20261103-130000",
            complete_doc(),
        );
        // 最新残档不采用,回退到最新完整档
        let facts = vec![older.clone(), newer_incomplete.clone(), newest_complete];
        let picked = latest_complete_handover_doc(&facts);
        assert_eq!(
            picked.and_then(|v| v["goal"].as_str()),
            Some("deliver v0.6.0")
        );
        // 全残/空 → None
        assert!(latest_complete_handover_doc(&[]).is_none());
        let only_incomplete = vec![newer_incomplete];
        assert!(latest_complete_handover_doc(&only_incomplete).is_none());
        // 唯一完整档直取
        let only_older = vec![older];
        assert!(latest_complete_handover_doc(&only_older).is_some());
    }

    #[test]
    fn handover_doc_to_package_maps_scalar_fields() {
        let pkg = handover_doc_to_package(&complete_doc());
        assert_eq!(pkg.goal, "deliver v0.6.0");
        assert_eq!(pkg.milestone_current.as_deref(), Some("CI chain green"));
        assert_eq!(pkg.next_step.as_deref(), Some("upload installer"));
        assert_eq!(pkg.verified_facts, vec!["main_sha = 4be4bc8".to_string()]);
        assert!(pkg.dead_ends.is_empty());
    }

    #[test]
    fn handover_doc_to_package_array_and_object_variants() {
        let mut doc = complete_doc();
        doc["done"] = json!(["milestone A", "milestone B"]); // 数组取末项
        doc["todo_next"] = json!(["first step", "second step"]); // 数组取首项
        doc["anchors"] = json!(["anchor 1", "anchor 2"]); // 数组逐条
        let pkg = handover_doc_to_package(&doc);
        assert_eq!(pkg.milestone_current.as_deref(), Some("milestone B"));
        assert_eq!(pkg.next_step.as_deref(), Some("first step"));
        assert_eq!(
            pkg.verified_facts,
            vec!["anchor 1".to_string(), "anchor 2".to_string()]
        );
        // anchors 超限:上限 20 条防巨锚爆底座
        let mut many = complete_doc();
        many["anchors"] = json!((1..=30).map(|i| format!("a{i}")).collect::<Vec<_>>());
        assert_eq!(handover_doc_to_package(&many).verified_facts.len(), 20);
    }

    #[test]
    fn handover_schema_missing_detects_missing_and_empty() {
        let full = json!({
            "goal": "g", "done": ["a"], "todo_next": ["b"],
            "anchors": {"k": "v"}, "env_state": {"HEAD": "x"},
            "verification": "v",
        });
        assert!(handover_schema_missing(&full).is_empty());
        // 缺字段
        let no_verification = json!({
            "goal": "g", "done": ["a"], "todo_next": ["b"],
            "anchors": {"k": "v"}, "env_state": {"HEAD": "x"},
        });
        assert_eq!(
            handover_schema_missing(&no_verification),
            vec!["verification"]
        );
        // 空串/空数组/空对象
        let empties = json!({
            "goal": "  ", "done": [], "todo_next": ["b"],
            "anchors": {}, "env_state": {"HEAD": "x"},
            "verification": "v",
        });
        assert_eq!(
            handover_schema_missing(&empties),
            vec!["goal", "done", "anchors"]
        );
        // 全缺
        assert_eq!(handover_schema_missing(&json!({})).len(), 6);
    }

    #[test]
    fn handover_write_extract_rejects_missing_fields_fail_visible() {
        // 缺 verification：错误信息列明缺项
        let args = json!({
            "goal": "g", "done": ["a"], "todo_next": ["b"],
            "anchors": {"k": "v"}, "env_state": {"HEAD": "x"},
        });
        let err = extract_handover_fields(&args).unwrap_err();
        assert!(err.contains("verification"), "{err}");
        // 缺 done + anchors：一次列全（不逐个挤牙膏）
        let args = json!({
            "goal": "g", "todo_next": ["b"],
            "env_state": {"HEAD": "x"}, "verification": "v",
        });
        let err = extract_handover_fields(&args).unwrap_err();
        assert!(err.contains("done") && err.contains("anchors"), "{err}");
    }

    #[test]
    fn handover_write_extract_assembles_valid_fields() {
        let args = json!({
            "goal": "g", "done": ["a"], "todo_next": ["b"],
            "anchors": {"k": "v"}, "env_state": {"HEAD": "x"},
            "verification": "v",
        });
        let fields = extract_handover_fields(&args).unwrap();
        assert!(handover_schema_missing(&fields).is_empty());
    }

    #[test]
    fn compose_handover_doc_fills_system_fields() {
        let fields = json!({
            "goal": "g", "done": ["a"], "todo_next": ["b"],
            "anchors": {"k": "v"}, "env_state": {"HEAD": "x"},
            "verification": "v",
        });
        let (path, id, doc) = compose_handover_doc("general", "sess-1", 1_791_500_000, fields);
        assert!(
            path.starts_with("shared.general.handovers.handover-"),
            "{path}"
        );
        assert_eq!(path, format!("shared.general.handovers.{id}"));
        assert_eq!(doc["parent_session"], json!("sess-1"));
        assert_eq!(doc["handover_version"], json!("1.0"));
        assert_eq!(doc["goal"], json!("g"));
        assert_eq!(doc["env_state"], json!({"HEAD": "x"}));
    }

    #[test]
    fn utc_stamp_is_deterministic() {
        // 确定性锚：epoch 原点与整日/整时推进
        assert_eq!(utc_stamp(0), "19700101-000000");
        assert_eq!(utc_stamp(3_600), "19700101-010000");
        assert_eq!(utc_stamp(86_400), "19700102-000000");
        // 跨年推进（1970-12-31 → 1971-01-01）
        assert_eq!(utc_stamp(365 * 86_400), "19710101-000000");
        // 闰年含 2 月 29 日（1972 年；epoch+789 天 = 1972-02-29）
        assert_eq!(utc_stamp(789 * 86_400), "19720229-000000");
    }

    #[tokio::test]
    async fn unwired_tools_fail_visible() {
        let w = HandoverWriteTool::unwired();
        let err = w.call(&serde_json::json!({})).await.unwrap_err();
        assert!(err.contains("not wired"), "{err}");
        let r = HandoverReadTool::unwired();
        let err = r.call(&serde_json::json!({})).await.unwrap_err();
        assert!(err.contains("not wired"), "{err}");
    }

    // ===== Checkpoint-block mechanization (long-horizon write-off batch) =====

    use crate::agent::journal::{JournalWriter, TodoItemSnapshot};

    /// Line builder: seq ordered, explicit ts, no chain hashes (projections
    /// never read the hash column).
    fn cline(seq: u64, ts: u64, event: JournalEvent) -> JournalLine {
        JournalLine {
            seq,
            ts,
            event,
            hashes: None,
        }
    }

    fn cturn(seq: u64, ts: u64, turn: u64, goal: &str) -> JournalLine {
        cline(
            seq,
            ts,
            JournalEvent::TurnStarted {
                turn_seq: turn,
                goal: goal.to_string(),
            },
        )
    }

    fn ctodo(seq: u64, ts: u64, id: &str, content: &str, status: &str) -> JournalLine {
        cline(
            seq,
            ts,
            JournalEvent::TodoUpdated {
                items: vec![TodoItemSnapshot {
                    id: id.to_string(),
                    content: content.to_string(),
                    status: status.to_string(),
                    priority: "medium".to_string(),
                }],
            },
        )
    }

    fn cverify(seq: u64, ts: u64, verdict: &str) -> JournalLine {
        cline(
            seq,
            ts,
            JournalEvent::VerifyRan {
                name: "accept".to_string(),
                command: "true".to_string(),
                exit_code: Some(0),
                error_count: 0,
                diags_digest: "d".to_string(),
                verdict: verdict.to_string(),
                output_digest: format!("out-{seq}"),
            },
        )
    }

    fn ccheckpoint(seq: u64, ts: u64, path: &str, hash: &str) -> JournalLine {
        cline(
            seq,
            ts,
            JournalEvent::FileCheckpointTaken {
                path: path.to_string(),
                kind: "content".to_string(),
                hash: hash.to_string(),
                bytes: 3,
                tool: "file_write".to_string(),
            },
        )
    }

    #[test]
    fn checkpoint_draft_projection_deterministic_window_scoped() {
        // Same journal input projected twice → byte-identical draft
        // (machine-draft determinism criterion); the window scopes at the
        // newest turn boundary and the anchors face keeps the newest hash
        // per path.
        let lines = vec![
            cturn(1, 100, 1, "old goal"),
            ccheckpoint(2, 110, "stale.txt", "old-hash"),
            cverify(3, 120, "pass"),
            cturn(4, 200, 2, "current goal"),
            ctodo(5, 210, "t1", "write the parser", "completed"),
            ctodo(6, 211, "t2", "write tests", "in_progress"),
            ccheckpoint(7, 220, "a.txt", "hash-7"),
            ccheckpoint(8, 221, "b.txt", "hash-8"),
            ccheckpoint(9, 222, "a.txt", "hash-9"),
            cverify(10, 230, "pass"),
        ];
        let a = project_checkpoint_window(&lines).expect("turn boundary in window");
        let b = project_checkpoint_window(&lines).expect("same window again");
        assert_eq!(
            serde_json::to_string(&a.draft).unwrap(),
            serde_json::to_string(&b.draft).unwrap(),
            "same input must project byte-identical drafts"
        );
        assert_eq!(a.window, 2, "window number is the newest turn_seq");
        assert_eq!(a.completed_items, vec!["t1".to_string()]);
        assert_eq!(a.draft["goal"], json!("current goal"));
        assert_eq!(
            a.draft["done"],
            json!([
                "completed: write the parser",
                "verify accept: pass (exit 0)"
            ]),
            "done = completed transitions + verify verdict summaries in chain order"
        );
        assert_eq!(
            a.draft["anchors"],
            json!({"a.txt": "hash-9", "b.txt": "hash-8"}),
            "latest hash per path; pre-window checkpoints out of scope"
        );
        assert_eq!(a.draft["verification"], json!("pass (exit 0)"));
    }

    #[test]
    fn checkpoint_draft_anchor_cap_drops_oldest() {
        let mut lines = vec![cturn(0, 0, 1, "g")];
        for i in 0..(CHECKPOINT_ANCHOR_CAP + 2) {
            lines.push(ccheckpoint(
                i as u64 + 1,
                100 + i as u64,
                &format!("p{i}.txt"),
                &format!("h{i}"),
            ));
        }
        let proj = project_checkpoint_window(&lines).expect("window");
        let anchors = proj.draft["anchors"].as_object().expect("object face");
        assert_eq!(anchors.len(), CHECKPOINT_ANCHOR_CAP, "cap enforced");
        assert!(
            !anchors.contains_key("p0.txt") && !anchors.contains_key("p1.txt"),
            "oldest entries drop when the window exceeds the cap"
        );
        assert_eq!(
            anchors.get(&format!("p{}.txt", CHECKPOINT_ANCHOR_CAP + 1)),
            Some(&json!(format!("h{}", CHECKPOINT_ANCHOR_CAP + 1))),
            "newest entries stay"
        );
    }

    #[test]
    fn checkpoint_draft_requires_turn_window() {
        let lines = vec![cverify(1, 100, "pass"), ccheckpoint(2, 110, "a", "h")];
        assert!(
            project_checkpoint_window(&lines).is_none(),
            "no turn boundary = no window = no draft (machine never invents a goal)"
        );
        assert!(project_checkpoint_window(&[]).is_none());
    }

    #[test]
    fn merge_boundary_machine_facts_win_llm_semantics_required() {
        // Boundary test: machine-derived fact fields take the draft values
        // (model-supplied fact values are discarded); semantic fields are
        // model-only and missing ones refuse the write.
        let draft = json!({
            "goal": "machine goal",
            "done": ["completed: a", "verify accept: pass (exit 0)"],
            "anchors": {"a.txt": "hash-1"},
            "verification": "pass (exit 0)",
        });
        // The model tries to smuggle its own fact face and skips the
        // semantic face: refused, every missing semantic field listed once.
        let smuggle = json!({
            "goal": "model goal",
            "done": ["model done"],
            "anchors": {"x": "y"},
            "verification": "model verdict",
        });
        let err = merge_checkpoint_block(&draft, &smuggle).unwrap_err();
        assert!(err.contains("todo_next"), "{err}");
        assert!(err.contains("env_state"), "{err}");
        // Semantic face supplied: machine facts win, provenance reported.
        let ok = json!({
            "goal": "model goal",
            "todo_next": ["run cargo test"],
            "anchors": {"x": "y"},
            "env_state": {"os": "linux"},
            "verification": "model verdict",
        });
        let merged = merge_checkpoint_block(&draft, &ok).expect("complete block");
        assert_eq!(merged.fields["goal"], json!("machine goal"), "machine wins");
        assert_eq!(
            merged.fields["done"],
            json!(["completed: a", "verify accept: pass (exit 0)"]),
            "machine wins"
        );
        assert_eq!(
            merged.fields["anchors"],
            json!({"a.txt": "hash-1"}),
            "machine wins"
        );
        assert_eq!(
            merged.fields["verification"],
            json!("pass (exit 0)"),
            "machine wins"
        );
        assert_eq!(
            merged.fields["todo_next"],
            json!(["run cargo test"]),
            "model-only"
        );
        assert_eq!(
            merged.fields["env_state"],
            json!({"os": "linux"}),
            "model-only"
        );
        assert_eq!(
            merged.machine_prefilled,
            vec!["goal", "done", "anchors", "verification"],
            "provenance lists exactly the machine-prefilled fact fields"
        );
        assert!(handover_schema_missing(&merged.fields).is_empty());
    }

    #[test]
    fn merge_lists_every_missing_field_in_one_shot() {
        let draft = json!({
            "goal": "g", "done": ["d"], "anchors": {}, "verification": "unknown: no verify run in window",
        });
        let err = merge_checkpoint_block(&draft, &json!({})).unwrap_err();
        // Semantic face plus the undrivable fact gap (empty machine anchors)
        // are all listed at once, never drip-fed.
        for field in ["todo_next", "env_state", "anchors"] {
            assert!(err.contains(field), "{field} must be listed: {err}");
        }
    }

    #[test]
    fn merge_fills_undrivable_fact_gap_from_args() {
        // Empty machine anchors = the one fact field the machine could not
        // derive in this window; the model may fill it (fill-only, never
        // override), and provenance excludes it.
        let draft = json!({
            "goal": "g", "done": ["d"], "anchors": {}, "verification": "v",
        });
        let args = json!({
            "anchors": {"recovered.txt": "hash-x"},
            "todo_next": ["next"],
            "env_state": {"k": "v"},
        });
        let merged = merge_checkpoint_block(&draft, &args).expect("complete block");
        assert_eq!(
            merged.fields["anchors"],
            json!({"recovered.txt": "hash-x"}),
            "undrivable fact gap is filled from the model args"
        );
        assert_eq!(
            merged.machine_prefilled,
            vec!["goal", "done", "verification"],
            "anchors was model-filled, not machine-prefilled"
        );
    }

    #[test]
    fn freshness_gate_two_level_with_reset_and_unknown_passthrough() {
        let mut gate = FreshnessGate::new();
        // First stale finding: level-1 warning mark.
        match gate.observe(FreshnessState::Stale) {
            FreshnessGuidance::Warn(mark) => {
                assert_eq!(mark, CHECKPOINT_STALE_GUIDANCE);
                assert!(mark.contains("handover_write"), "{mark}");
            }
            other => panic!("first violation must warn, got {other:?}"),
        }
        // Repeat: escalation.
        match gate.observe(FreshnessState::Missing) {
            FreshnessGuidance::Escalate(mark) => assert_eq!(mark, CHECKPOINT_STALE_ESCALATED),
            other => panic!("repeat violation must escalate, got {other:?}"),
        }
        assert_eq!(gate.violation_count(), 2);
        // Unknowable face: no nag, no reset.
        assert!(matches!(
            gate.observe(FreshnessState::Unknown),
            FreshnessGuidance::None
        ));
        assert_eq!(gate.violation_count(), 2);
        // Fresh window: reset — the next stale finding warns again (level 1).
        assert!(matches!(
            gate.observe(FreshnessState::Fresh),
            FreshnessGuidance::None
        ));
        assert_eq!(gate.violation_count(), 0);
        assert!(matches!(
            gate.observe(FreshnessState::Stale),
            FreshnessGuidance::Warn(CHECKPOINT_STALE_GUIDANCE)
        ));
    }

    #[test]
    fn writeoff_draft_provider_is_mode_gated() {
        // Mode off (default): no draft provider — the legacy full-six-field
        // contract applies unchanged (default invariant).
        let plain = HandoverWriteTool::wired("ns".into(), test_client_stub(), "s".into());
        assert!(plain.writeoff_draft.is_none());
        // Mode on: the builder attaches the provider.
        let wired = plain.with_writeoff_draft(std::sync::Arc::new(|| Some(json!({"goal": "g"}))));
        assert!(wired.writeoff_draft.is_some());
    }

    /// Stub for constructor-shape tests only (never called).
    fn test_client_stub() -> EvoruleApiClient {
        EvoruleApiClient::new("http://127.0.0.1:1")
    }

    #[test]
    fn replay_latest_wins_and_refuses_tampered_digest() {
        // Latest-wins replay family rule, then tamper detection: a corrupted
        // checkpoint line refuses the rebuild instead of continuing on a
        // wrong breakpoint.
        let draft1 = json!({"goal": "w1", "done": [], "anchors": {}, "verification": "v1"});
        let draft2 =
            json!({"goal": "w2", "done": ["d"], "anchors": {"a": "h"}, "verification": "v2"});
        let d1 = serde_json::to_string(&draft1).unwrap();
        let d2 = serde_json::to_string(&draft2).unwrap();
        let lines = vec![
            cline(
                1,
                10,
                JournalEvent::WriteoffCheckpointed {
                    window: 1,
                    completed_items: vec!["old".to_string()],
                    block_draft: d1.clone(),
                    block_digest: crate::agent::journal::evorule_digest(&d1),
                },
            ),
            cline(
                2,
                20,
                JournalEvent::WriteoffCheckpointed {
                    window: 2,
                    completed_items: vec!["t1".to_string(), "t2".to_string()],
                    block_draft: d2.clone(),
                    block_digest: crate::agent::journal::evorule_digest(&d2),
                },
            ),
        ];
        let state = replay_writeoff_checkpoint(&lines)
            .expect("replay ok")
            .expect("checkpoint present");
        assert_eq!(state.window, 2, "latest checkpoint wins");
        assert_eq!(
            state.completed_items,
            vec!["t1".to_string(), "t2".to_string()]
        );
        assert_eq!(state.block, draft2, "block rebuilds field-by-field");
        assert_eq!(
            state.block_digest,
            crate::agent::journal::evorule_digest(&d2)
        );
        // No checkpoint at all → Ok(None).
        assert!(replay_writeoff_checkpoint(&[]).unwrap().is_none());
        // Tampered draft: digest mismatch refuses the rebuild (fail-visible).
        let tampered = vec![cline(
            1,
            10,
            JournalEvent::WriteoffCheckpointed {
                window: 1,
                completed_items: vec![],
                block_draft: d1.clone(),
                block_digest: crate::agent::journal::evorule_digest(&d2),
            },
        )];
        let err = replay_writeoff_checkpoint(&tampered).unwrap_err();
        assert!(err.contains("digest mismatch"), "{err}");
    }

    #[test]
    fn crash_injection_replay_field_by_field_consistent() {
        // Crash-injection criterion: the runner dies mid-window (writer
        // dropped, process gone) right after the checkpoint persisted; on
        // restart the recovery read rebuilds the breakpoint block from the
        // newest writeoff_checkpointed event and every field matches the
        // pre-crash projection.
        let dir = std::env::temp_dir().join(format!("jf-crash-wocp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = "s-crash";
        // --- pre-crash session: window with completed todo, verify, two
        // --- checkpoints; the machine draft is projected, then the
        // --- checkpoint event is appended (runner-side hook order).
        let pre_crash = {
            let w = JournalWriter::open(&dir, session).unwrap();
            w.begin_turn("long goal: build and verify").unwrap();
            w.todo_updated(vec![
                TodoItemSnapshot {
                    id: "t1".to_string(),
                    content: "write the parser".to_string(),
                    status: "completed".to_string(),
                    priority: "high".to_string(),
                },
                TodoItemSnapshot {
                    id: "t2".to_string(),
                    content: "write the tests".to_string(),
                    status: "in_progress".to_string(),
                    priority: "medium".to_string(),
                },
            ])
            .unwrap();
            w.verify_ran("accept", "cargo test", Some(0), 0, "d", "pass", "out-1")
                .unwrap();
            w.file_checkpoint_taken("src/lib.rs", "content", "hash-a", 10, "file_write")
                .unwrap();
            w.file_checkpoint_taken("src/main.rs", "content", "hash-b", 20, "file_write")
                .unwrap();
            // The runner projects the machine draft from the flushed lines…
            let lines = w.read_lines().unwrap();
            let proj = crate::agent::handover_tool::project_checkpoint_window(&lines)
                .expect("turn window present");
            // …and appends the checkpoint event (handover write succeeded).
            let draft_str = serde_json::to_string(&proj.draft).unwrap();
            let digest = crate::agent::journal::evorule_digest(&draft_str);
            w.writeoff_checkpointed(proj.window, &proj.completed_items, &draft_str, &digest)
                .unwrap();
            (proj, draft_str, digest)
        };
        let (proj, draft_str, digest) = pre_crash;
        // --- crash: the writer block above has dropped with the turn still
        // --- open — that drop is the process death; nothing is flushed
        // --- afterwards.
        // --- restart: recovery read (tolerant of a torn tail) + replay.
        let path = JournalWriter::path_for(&dir, session);
        let recovered = crate::agent::journal::read_all_tolerant(&path).unwrap();
        let state = replay_writeoff_checkpoint(&recovered)
            .expect("replay must not refuse an honest checkpoint")
            .expect("checkpoint event survived the crash");
        // Field-by-field comparison against the pre-crash projection:
        let fields: Vec<(&str, bool)> = vec![
            ("window", state.window == proj.window),
            (
                "completed_items",
                state.completed_items == proj.completed_items,
            ),
            ("block.goal", state.block["goal"] == proj.draft["goal"]),
            ("block.done", state.block["done"] == proj.draft["done"]),
            (
                "block.anchors",
                state.block["anchors"] == proj.draft["anchors"],
            ),
            (
                "block.verification",
                state.block["verification"] == proj.draft["verification"],
            ),
            ("block_digest", state.block_digest == digest),
            (
                "draft_bytes",
                serde_json::to_string(&state.block).unwrap() == draft_str,
            ),
        ];
        for (name, ok) in &fields {
            assert!(ok, "field {name} diverged after crash replay");
        }
        assert_eq!(fields.len(), 8, "every compared field is accounted for");
        assert_eq!(state.completed_items, vec!["t1".to_string()]);
        assert_eq!(state.block["goal"], json!("long goal: build and verify"));
        // Journal truncation variant: crash tears the file mid-tail after
        // the checkpoint line — the tolerant read ignores the torn tail and
        // the replay still rebuilds the identical state.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(b'{'); // torn partial line
        std::fs::write(&path, &bytes).unwrap();
        let truncated = crate::agent::journal::read_all_tolerant(&path).unwrap();
        let state2 = replay_writeoff_checkpoint(&truncated)
            .expect("torn tail must not refuse the rebuild")
            .expect("checkpoint event still present");
        assert_eq!(
            state2, state,
            "truncation after the checkpoint changes nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
