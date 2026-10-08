// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 本地逻辑工具 handler（D-1 分类表 6 个）——无法表驱动为纯透传、带本地语义
//! 变换/校验/渲染的工具统一外置于本文件；纯透传族见 [`super::adapter`]。
//!
//! - audit_verify：验证审计链 → 包装为 {"verified": bool}；
//! - bundle_export：治理域带证据导出（verdict/subset 前置形状校验）；
//! - skill_pack_to_bundle：skill 规则壳 → 执行域快照包桥接（crate 算哈希 +
//!   本地条目结构预检；knowledge 段不进执行域，闸门一缺省 fail）；
//! - meta_summary：L2 约束清单摘要（读取与陈述，展示层）；
//! - evolution_signals：会话违规信号聚合摘要（读取与陈述，展示层）；
//! - rule_promote：约束层晋升提名（kind 硬编码 meta_promotion 防旁路，
//!   进人审队列，本工具不提供任何审批能力）。
//!
//! 共用渲染模板（工具面与前馈注入同一模板源）：
//! - [`render_l2_inventory_summary`]（serve 前馈三路径 system_prompt 追加）；
//! - [`render_evolution_signals_summary`]（前馈感知段）。
//!
//! 边界纪律：meta/evolution 只做**读取与陈述**（不触碰 L2 引擎面、不写
//! rules_dir、不改变任何门禁行为）；rule_promote 只做**提名转发**，审批/落盘
//! 全部在治理链人审闭环内，agent 面不存在审批通道。

use std::sync::Arc;

use serde_json::Value;

use evorule_bundle::{
    BundleAudit, BundleDatasetMeta, BundleEntry, BundleError, BundleImporter, BundleTests,
    DatasetBundle, DomainSchemaResolver, EntryKind, Provenance, RecipeSnapshot, TestVerdict,
    VersionSelection, VersionSelectionMode, BUNDLE_SCHEMA_VERSION,
};

use crate::api::evorule_client::EvoruleApiClient;
use crate::api::workspace_client::{SubmitPublishRequest, WorkspaceApiClient};
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};

// =============================================================================
// 共用渲染模板（单一模板源）
// =============================================================================

/// 无 L2 约束时的明示文本（工具面不返回空串）
pub const NO_L2_TEXT: &str = "当前无 L2 约束规则。";

/// 渲染 L2 约束清单摘要（前馈注入与 meta_summary 工具共用的单一模板源）
///
/// 输入为 `GET /api/rules/l2-inventory` 响应（`{count, files:[{path,title,guard_for}]}`）。
/// 返回 `None` = 空清单（无 L2），调用方按语义处理（工具返回明示文本；前馈不注入）。
pub fn render_l2_inventory_summary(inv: &Value) -> Option<String> {
    let count = inv.get("count").and_then(|c| c.as_u64()).unwrap_or(0);
    let files = inv.get("files").and_then(|f| f.as_array())?;
    if count == 0 || files.is_empty() {
        return None;
    }
    let mut out = String::new();
    out.push_str("【L2 约束边界（生成规则草稿前必读）】\n");
    out.push_str(
        "你生成的业务规则不得修改或绕过以下守卫语义；关键动作必须以守卫标记为前置条件。\n",
    );
    out.push_str("禁项：\n");
    out.push_str(
        "- 禁止在业务规则中声明约束层层级标记（metadata.tier=\"constraint\" 或旧值 \"meta\"）冒充约束层——层级门禁会拒载该文件；\n",
    );
    out.push_str("- 禁止写入守卫保留的 metadata 保留域（保留域拒绝写入）；\n");
    out.push_str(
        "- 禁止在业务规则中携带引擎级强制原语 enforce——该原语仅随治理链晋升的约束层下发，业务规则携带会在导入期拒载；如需强制约束，请走治理链晋升流程。\n",
    );
    out.push_str(
        "路径读写约定：set 的 attr 相对 payload 解析（不带前缀）；domain 的 path 相对执行根解析（读取 payload 须带 payload. 前缀）。\n",
    );
    out.push_str("当前生效的约束规则清单：\n");
    for f in files {
        let path = f.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let title = f.get("title").and_then(|t| t.as_str()).unwrap_or("");
        let guards = f
            .get("guard_for")
            .and_then(|g| g.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        if guards.is_empty() {
            out.push_str(&format!("- {path}: {title}\n"));
        } else {
            out.push_str(&format!("- {path}: {title}（守卫指令类型: {guards}）\n"));
        }
    }
    Some(out)
}

/// 无进化信号时的明示文本（工具面不返回空串）
pub const NO_SIGNALS_TEXT: &str = "当前无进化信号。";

/// 渲染进化信号摘要（evolution_signals 工具的单一模板源）
///
/// 输入为 `GET /api/sessions/{id}/evolution-signals` 响应。返回 `None` =
/// 无信号，调用方按语义处理（工具返回明示文本）。
pub fn render_evolution_signals_summary(resp: &Value) -> Option<String> {
    let signals = resp.get("signals").and_then(|s| s.as_array())?;
    if signals.is_empty() {
        return None;
    }
    let total = resp
        .get("total_violations")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let session_id = resp.get("session_id").and_then(|s| s.as_u64()).unwrap_or(0);
    let mut out = String::new();
    out.push_str(&format!("【进化信号（会话 {session_id}）】\n"));
    out.push_str(&format!(
        "审计链共记录 {total} 条违规拦截，聚合为以下信号：\n"
    ));
    for s in signals {
        let rule_ref = s.get("rule_ref").and_then(|r| r.as_str()).unwrap_or("?");
        let count = s.get("count").and_then(|c| c.as_u64()).unwrap_or(0);
        let last_version = s.get("last_version").and_then(|v| v.as_u64()).unwrap_or(0);
        let instr = s
            .get("last_instr_type")
            .and_then(|i| i.as_str())
            .unwrap_or("unknown");
        let reason = s
            .get("reason_summary")
            .and_then(|r| r.as_str())
            .unwrap_or("");
        out.push_str(&format!(
            "- {rule_ref} ×{count}（末次: v{last_version}，指令: {instr}）— {reason}\n"
        ));
    }
    if let Some(queue) = resp.get("queue") {
        let normal = queue
            .get("pending_normal")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let promo = queue
            .get("pending_meta_promotion")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        out.push_str(&format!(
            "治理队列现状：待审普通规则 {normal} 条；待审约束层晋升 {promo} 条。\n"
        ));
    }
    out.push_str(
        "提示：起草改进规则使用 rule_create；约束层变更须经治理链提名并人工审批，不得旁路。\n",
    );
    Some(out)
}

// =============================================================================
// audit_verify —— 验证审计 → {"verified": bool}
// =============================================================================

#[derive(Clone)]
pub struct AuditVerifyTool {
    client: EvoruleApiClient,
}

impl AuditVerifyTool {
    pub fn new(client: EvoruleApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for AuditVerifyTool {
    async fn call(&self, args: &Value) -> IoResult {
        let session_id = args
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: session_id".to_string())?;
        let verified = self
            .client
            .verify_audit(session_id)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::json!({ "verified": verified });
        Ok(v.clone())
    }
}

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

/// 系统侧策略快照构造（O-377① 批 1：bundle_export 注入通路）。
///
/// 快照来源 = 进程级策略槽（`agent::recipe::current_published_recipe`，由
/// MemoryManager::set_recipe 单点发布）——**不开放 LLM 传参**，防伪造与
/// 「导出不伪造 verdict」同哲学。返回序列化后的 RecipeSnapshot JSON
/// （evorule-bundle 契约形态）；无策略在位 → None（导出不带快照，
/// 字节兼容缺省语义）。序列化失败显式报错（fail-fast，不静默降级）。
fn build_recipe_snapshot_value() -> Result<Option<Value>, String> {
    match crate::agent::recipe::current_published_recipe() {
        None => Ok(None),
        Some(recipe) => Ok(Some(recipe_snapshot_value(&recipe)?)),
    }
}

/// 快照序列化纯函数（给定策略 → RecipeSnapshot 契约 JSON；单测确定性入口）
fn recipe_snapshot_value(recipe: &crate::agent::recipe::MemoryRecipe) -> Result<Value, String> {
    let recipe_value =
        serde_json::to_value(recipe).map_err(|e| format!("recipe serialize: {e}"))?;
    let snapshot = RecipeSnapshot {
        recipe_version: recipe.recipe_version.clone(),
        recipe: recipe_value,
        snapshot_at: rfc3339_utc_now(),
    };
    serde_json::to_value(&snapshot).map_err(|e| format!("snapshot serialize: {e}"))
}

#[async_trait::async_trait]
impl ToolFunction for BundleExportTool {
    async fn call(&self, args: &Value) -> IoResult {
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
        // 前置形状校验（与治理域 回归验证 B1 同口径，提前拦截省一次往返）：
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
        // 系统侧注入策略快照（非 LLM 传参——args 中同名字段一律忽略）
        let recipe_snapshot = build_recipe_snapshot_value()?;
        let result = self
            .client
            .export_bundle(dataset_id, version, verdict, subset, trim, recipe_snapshot)
            .await
            .map_err(|e| e.to_string())?;
        Ok(result.clone())
    }
}

// =============================================================================
// skill_pack_to_bundle —— skill 规则壳 → 执行域快照包桥接（纯本地构造+自洽校验）
// =============================================================================

/// skill-rule-pack（tools/skill-adapter 产物）→ DatasetBundle 转换器。
///
/// 设计边界（三条硬边界）：
/// - 仅搬运 `pack.rules`（判定标准四条件全过段落的规则壳，rule_body 零转译）；
///   `knowledge_index`/`llm_core_note` 不进执行域——知识条目 D3 强校验
///   schema_ref 必填，skill 知识体的正当归宿是上下文面（read_skill 装载），
///   本工具不提供绕门禁形态；
/// - content_hash 全程由 evorule-bundle crate 计算（零复刻零旁路，哈希纪律）；
/// - 闸门一语义（fail-closed 硬边界）：本工具恒出 fail 包（未验证不得激活）。
///   与 bundle_export 不同——export 的 pass 是治理侧已有证据的转述，本工具
///   是凭空构造，治理域在先事实不存在，故不持有 pass 发放权：verdict/evidence
///   参数已移除，传入即拒。pass 重出包属治理域职责（治理侧取得沙箱证据后经
///   bundle_export 带证据导出）。骨架规则（on_true.noop 占位）须先填充并取得
///   沙箱证据，再经治理域通路升级 verdict——本工具定位是「规范化落包+结构
///   预检」，不是一键激活，更不是 verdict 升级器。
#[derive(Clone)]
pub struct SkillPackToBundleTool;

impl SkillPackToBundleTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SkillPackToBundleTool {
    fn default() -> Self {
        Self
    }
}

/// epoch 毫秒 → RFC3339 UTC（YYYY-MM-DDTHH:MM:SSZ），零依赖（civil_from_days 算法）。
fn rfc3339_utc_now() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let secs = ms as i64 / 1000;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[async_trait::async_trait]
impl ToolFunction for SkillPackToBundleTool {
    async fn call(&self, args: &Value) -> IoResult {
        let pack = args
            .get("pack")
            .and_then(|v| v.as_object())
            .ok_or(
                "missing required parameter: pack (skill-rule-pack object as produced by the \
                 skill adapter)",
            )
            .map_err(|e| e.to_string())?;
        // fail-closed 硬边界：verdict/evidence 参数已移除（本工具不持有 pass
        // 发放权——凭空构造的包无治理域在先证据，前缀格式校验不构成真实证据）。
        // 任一参数出现即拒，防旧调用方/幻觉参数绕过闸门一。
        if args.get("verdict").is_some() || args.get("evidence").is_some() {
            return Err(
                "skill_pack_to_bundle always produces an unverified (fail) bundle: \
                 verdict/evidence parameters are not accepted — a verified pass bundle must \
                 be re-exported from the governance domain (bundle_export) after sandbox \
                 evidence is obtained there"
                    .to_string(),
            );
        }
        let rules = pack
            .get("rules")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "pack.rules missing or not an array".to_string())?;
        if rules.is_empty() {
            return Err("pack.rules is empty: nothing to convert".to_string());
        }
        let skill_name = pack
            .get("source_skill")
            .and_then(|s| s.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let skill_version = pack
            .get("source_skill")
            .and_then(|s| s.get("version"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let pack_id = pack
            .get("pack_id")
            .and_then(|v| v.as_str())
            .unwrap_or("skill-pack-unknown");
        let str_arg = |name: &str| args.get(name).and_then(|v| v.as_str()).map(String::from);
        let dataset_id = str_arg("dataset_id").unwrap_or_else(|| pack_id.to_string());
        let dataset_name = str_arg("dataset_name").unwrap_or_else(|| format!("skill:{skill_name}"));
        let tenant_id = str_arg("tenant_id").unwrap_or_else(|| "local".to_string());
        let instance_id = str_arg("instance_id").unwrap_or_else(|| "evo-agent".to_string());

        let mut entries = Vec::with_capacity(rules.len());
        for r in rules {
            let entry_id = r
                .get("entry_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "pack rule missing entry_id".to_string())?
                .to_string();
            let rule_body = r
                .get("rule_body")
                .cloned()
                .ok_or_else(|| format!("pack rule {entry_id} missing rule_body"))?;
            let domain = r
                .get("domain")
                .and_then(|v| v.as_str())
                .unwrap_or("skill")
                .to_string();
            let tags = r
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            entries.push(BundleEntry {
                entry_id,
                entry_kind: EntryKind::Rule,
                rule_body,
                schema_ref: None,
                provenance: Provenance {
                    source: format!("skill-adapter:{skill_name}@{skill_version}"),
                    clause: None,
                    document_id: None,
                    effective_from: None,
                    effective_to: None,
                    last_verified: None,
                    verified_by: None,
                },
                domain,
                tags,
                dependencies: Vec::new(),
                // 知识资产化四字段（0.4.0 契约随行）：rule 壳条目均 None
                knowledge_kind: None,
                trust_level: None,
                license_ref: None,
                execution_contract: None,
            });
        }

        let bundle = DatasetBundle {
            bundle_schema_version: BUNDLE_SCHEMA_VERSION.to_string(),
            bundle_id: format!("bundle-{dataset_id}"),
            dataset: BundleDatasetMeta {
                dataset_id,
                name: dataset_name,
                tenant_id,
                instance_id,
                versioning: evorule_bundle::Versioning::default(),
                version_selection: Some(VersionSelection {
                    mode: VersionSelectionMode::Pinned,
                    pinned_version: Some("v1".to_string()),
                    pinned_include_patch: None,
                }),
                law_ref: None,
                view_of: None,
                event_schemas: Vec::new(),
            },
            entries,
            data_dependencies: None,
            // skill 桥接无策略资产语境：不带快照（None 不序列化，字节兼容）
            recipe_snapshot: None,
            tests: BundleTests {
                subset: Vec::new(),
                fixtures: Vec::new(),
                verdict: TestVerdict::Fail,
            },
            audit: BundleAudit {
                exported_at: rfc3339_utc_now(),
                exported_by: "skill-adapter-bridge".to_string(),
                source_version: "v1".to_string(),
                content_hash: String::new(),
                hash_algo: "blake3".to_string(),
            },
        };
        let mut bundle = bundle;
        bundle.audit.content_hash = bundle.compute_content_hash();

        // 条目级结构预检：全量收集（BundleImporter::validate 整体 fail-fast，这里逐条显式清单）
        let resolver: DomainSchemaResolver<'_> = &|_: &str| None;
        let declared: Vec<String> = Vec::new();
        let mut structural_errors: Vec<String> = Vec::new();
        for e in &bundle.entries {
            if let Err(be) = BundleImporter::validate_entry(e, &declared, resolver) {
                structural_errors.push(format!("{}: {be}", e.entry_id));
            }
        }
        let gate_one_status = match BundleImporter::validate(&bundle, resolver) {
            Ok(_) => "pass".to_string(),
            Err(BundleError::TestsNotPassed { .. }) => {
                "fail (expected for unverified skeleton rules: fill rule bodies, obtain \
                 sandbox evidence via the governance domain, then re-export the verified \
                 bundle there with bundle_export)"
                    .to_string()
            }
            Err(be) => {
                structural_errors.push(format!("bundle-level: {be}"));
                "blocked".to_string()
            }
        };
        let entry_count = bundle.entries.len();
        let bundle_json = serde_json::to_value(&bundle).map_err(|e| e.to_string())?;
        Ok(serde_json::json!({
            "bundle": bundle_json,
            "validation": {
                "entry_count": entry_count,
                "structural_errors": structural_errors,
                "gate_one_status": gate_one_status,
            }
        }))
    }
}

// =============================================================================
// meta_summary —— L2 约束清单摘要
// =============================================================================

#[derive(Clone)]
pub struct MetaSummaryTool {
    client: EvoruleApiClient,
}

impl MetaSummaryTool {
    pub fn new(client: EvoruleApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for MetaSummaryTool {
    async fn call(&self, _args: &Value) -> IoResult {
        let inv = self
            .client
            .get_l2_inventory()
            .await
            .map_err(|e| e.to_string())?;
        Ok(Value::String(
            render_l2_inventory_summary(&inv).unwrap_or_else(|| NO_L2_TEXT.to_string()),
        ))
    }
}

// =============================================================================
// evolution_signals —— 会话违规信号聚合摘要
// =============================================================================

/// 解析 session_id 参数（宽容数字/数字字符串，对齐会话端点 u64 口径）
fn parse_session_id(args: &Value) -> Result<u64, String> {
    match args.get("session_id") {
        None | Some(Value::Null) => Err("missing required parameter: session_id".to_string()),
        Some(Value::Number(n)) => n
            .as_u64()
            .ok_or_else(|| "session_id must be a non-negative integer".to_string()),
        Some(Value::String(s)) => s
            .parse::<u64>()
            .map_err(|_| "session_id must be a valid integer string".to_string()),
        Some(_) => Err("session_id must be a non-negative integer".to_string()),
    }
}

#[derive(Clone)]
pub struct EvolutionSignalsTool {
    client: EvoruleApiClient,
}

impl EvolutionSignalsTool {
    pub fn new(client: EvoruleApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for EvolutionSignalsTool {
    async fn call(&self, args: &Value) -> IoResult {
        let session_id = parse_session_id(args)?;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        if args.get("limit").is_some() && args.get("limit") != Some(&Value::Null) && limit.is_none()
        {
            return Err("limit must be a non-negative integer".to_string());
        }
        let resp = self
            .client
            .get_evolution_signals(session_id, limit)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Value::String(
            render_evolution_signals_summary(&resp).unwrap_or_else(|| NO_SIGNALS_TEXT.to_string()),
        ))
    }
}

// =============================================================================
// rule_promote —— 约束层晋升提名（治理链 enqueue，人审闭环）
// =============================================================================

/// 解析 rule_version_ids 参数（宽容单字符串/字符串数组两种形态）
fn parse_rule_version_ids(args: &Value) -> Result<Vec<String>, String> {
    match args.get("rule_version_ids") {
        None | Some(Value::Null) => Err("missing required parameter: rule_version_ids".to_string()),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(arr)) => {
            let ids: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            if ids.is_empty() || ids.len() != arr.len() {
                return Err("rule_version_ids must be a non-empty array of strings".to_string());
            }
            Ok(ids)
        }
        Some(_) => Err("rule_version_ids must be a string or an array of strings".to_string()),
    }
}

fn require_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing required parameter: {key}"))
}

#[derive(Clone)]
pub struct RulePromoteTool {
    client: WorkspaceApiClient,
}

impl RulePromoteTool {
    pub fn new(client: WorkspaceApiClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ToolFunction for RulePromoteTool {
    async fn call(&self, args: &Value) -> IoResult {
        let workspace_id = require_str(args, "workspace_id")?;
        let rule_version_ids = parse_rule_version_ids(args)?;
        // 转写产物必填：约束层内容 JSON 字符串（服务端做 schema/门禁前置校验）
        let meta_rule_content = require_str(args, "meta_rule_content")?;
        let submitted_by = require_str(args, "submitted_by")?;
        let role = require_str(args, "role")?;
        // 闸门一硬约束：meta_promotion 必须在提交时关联已关闭的沙盒测试
        // （服务端审批 fail-closed，缺失必死路）。容忍将数字字符串化的常见形态。
        let raw = args
            .get("test_report_sandbox_id")
            .filter(|v| !v.is_null())
            .ok_or_else(|| {
                "missing required parameter: test_report_sandbox_id (gate-one evidence: run \
                 sandbox_start + sandbox_close on the source rule version first, then pass \
                 the closed sandbox id here)"
                    .to_string()
            })?;
        let test_report_sandbox_id = match raw {
            Value::Number(n) => n.as_i64().ok_or_else(|| {
                "test_report_sandbox_id must be an integer sandbox id".to_string()
            })?,
            Value::String(s) => s
                .trim()
                .parse::<i64>()
                .map_err(|_| "test_report_sandbox_id must be an integer sandbox id".to_string())?,
            _ => return Err("test_report_sandbox_id must be an integer sandbox id".to_string()),
        };
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // kind 硬编码 meta_promotion：提名工具无改道 normal 通道的口子（防旁路）
        let req = SubmitPublishRequest {
            workspace_id,
            rule_version_ids,
            test_report_sandbox_id: Some(test_report_sandbox_id),
            description,
            kind: Some("meta_promotion".to_string()),
            meta_rule_content: Some(meta_rule_content),
        };
        let item = self
            .client
            .submit_publish(req, &submitted_by, &role)
            .await
            .map_err(|e| e.to_string())?;
        let v = serde_json::to_value(&item).unwrap_or_default();
        Ok(v)
    }
}

// =============================================================================
// register / specs
// =============================================================================

/// 注册全部本地逻辑工具（6 个）
pub fn register(h: &mut ToolHandler, ws: &WorkspaceApiClient, ev: &EvoruleApiClient) {
    h.register_static("audit_verify", Arc::new(AuditVerifyTool::new(ev.clone())));
    h.register_static("bundle_export", Arc::new(BundleExportTool::new(ws.clone())));
    h.register_static(
        "skill_pack_to_bundle",
        Arc::new(SkillPackToBundleTool::new()),
    );
    h.register_static("meta_summary", Arc::new(MetaSummaryTool::new(ev.clone())));
    h.register_static(
        "evolution_signals",
        Arc::new(EvolutionSignalsTool::new(ev.clone())),
    );
    h.register_static("rule_promote", Arc::new(RulePromoteTool::new(ws.clone())));
}

/// 全部本地逻辑工具 spec（5 个）
pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "audit_verify".to_string(),
            description: "Verify the audit chain for a session. Returns {\"verified\": bool}."
                .to_string(),
            parameters: vec![ParameterSpec {
                name: "session_id".to_string(),
                r#type: "string".to_string(),
                description: "Session id.".to_string(),
                required: true,
            }],
        },
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
            name: "skill_pack_to_bundle".to_string(),
            description: "Convert a skill-rule-pack (the JSON produced by the skill adapter) \
                          into a DatasetBundle for the execution domain: pack.rules become \
                          Rule entries (rule_body passed through unchanged), the content hash \
                          is computed by the evorule-bundle crate, and a per-entry structural \
                          pre-check runs locally. The tool always produces an unverified \
                          (fail) bundle: verdict/evidence parameters are not accepted — a \
                          verified pass bundle must be re-exported from the governance domain \
                          (bundle_export) after sandbox evidence is obtained there. Feed the \
                          returned bundle to bundle_import_dry_run / bundle_import. Knowledge \
                          sections of the pack are NOT included: they belong to the context \
                          plane (read_skill), not the execution domain."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "pack".to_string(),
                    r#type: "object".to_string(),
                    description: "The skill-rule-pack object (pack_id / source_skill / rules \
                                  array with entry_id + rule_body + domain + tags per rule)."
                        .to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "dataset_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Dataset id for the bundle (defaults to the pack's pack_id)."
                        .to_string(),
                    required: false,
                },
                ParameterSpec {
                    name: "dataset_name".to_string(),
                    r#type: "string".to_string(),
                    description: "Human-readable dataset name (defaults to \"skill:<name>\")."
                        .to_string(),
                    required: false,
                },
            ],
        },
        ToolSpec {
            name: "meta_summary".to_string(),
            description: "Summarize the currently effective L2 constraint (meta) rules — the \
                          guard boundaries a rule draft must respect (read-only)."
                .to_string(),
            parameters: vec![],
        },
        ToolSpec {
            name: "evolution_signals".to_string(),
            description: "Fetch the aggregated violation signals of a session (read-only) — \
                          which rules keep causing enforce-blocked violations, how often, and \
                          what is pending in the governance queue."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "session_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Session id (integer or integer string).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "limit".to_string(),
                    r#type: "integer".to_string(),
                    description: "Max number of signals to return (non-negative; 0 = unbounded)."
                        .to_string(),
                    required: false,
                },
            ],
        },
        ToolSpec {
            name: "rule_promote".to_string(),
            description: "Nominate a drafted rule for meta-promotion into the L2 constraint \
                          layer via the governance publish queue (kind is fixed to \
                          meta_promotion; a human reviewer must approve before it takes \
                          effect — no bypass). The reviewer's gate requires sandbox \
                          evidence: run sandbox_start + sandbox_close on the source rule \
                          version first, then pass the closed sandbox id here."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "workspace_id".to_string(),
                    r#type: "string".to_string(),
                    description: "Workspace the source rule belongs to.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "rule_version_ids".to_string(),
                    r#type: "array".to_string(),
                    description: "Source rule version id(s) (string or array of strings); \
                                  recorded as promotion provenance."
                        .to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "meta_rule_content".to_string(),
                    r#type: "string".to_string(),
                    description: "Translated meta-rule JSON (string) with metadata.tier and \
                                  transforms; server validates and authority-fills provenance."
                        .to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "submitted_by".to_string(),
                    r#type: "string".to_string(),
                    description: "Identity of the nominator (recorded for audit).".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "role".to_string(),
                    r#type: "string".to_string(),
                    description: "Submitter role accepted by the governance queue \
                                  (\"department_head\" or \"admin\")."
                        .to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "test_report_sandbox_id".to_string(),
                    r#type: "integer".to_string(),
                    description: "Sandbox id (i64) of a CLOSED sandbox test run on the source \
                                  rule version; gate-one review fails closed without it. \
                                  Accepts an integer or numeric string."
                        .to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "description".to_string(),
                    r#type: "string".to_string(),
                    description: "Optional nomination note for the reviewer.".to_string(),
                    required: false,
                },
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn make_ws() -> WorkspaceApiClient {
        WorkspaceApiClient::new("http://localhost:0")
    }

    fn make_ev() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:0")
    }

    // =========================================================================
    // 策略快照注入（O-377① 批 1）
    // =========================================================================

    #[test]
    fn test_recipe_snapshot_value_shape() {
        // 纯函数确定性测试：契约形态三字段齐备，recipe 为完整 opaque 载荷
        let recipe = crate::agent::recipe::MemoryRecipe::default();
        let snap = recipe_snapshot_value(&recipe).unwrap();
        assert_eq!(snap["recipe_version"], "memory-v1.0");
        assert!(snap["recipe"].is_object(), "recipe 应为完整策略 JSON 载荷");
        assert_eq!(
            snap["recipe"]["retrieval"]["w_relevance"],
            serde_json::to_value(&recipe).unwrap()["retrieval"]["w_relevance"]
        );
        assert!(
            !snap["snapshot_at"].as_str().unwrap().is_empty(),
            "snapshot_at 应为导出时刻"
        );
        // 契约回读：序列化产物可反序列化为 evorule-bundle RecipeSnapshot（根路径导出）
        let back: RecipeSnapshot = serde_json::from_value(snap).unwrap();
        assert_eq!(back.recipe_version, "memory-v1.0");
    }

    // =========================================================================
    // 渲染模板（L2 清单）
    // =========================================================================

    fn sample_inventory() -> Value {
        serde_json::json!({
            "count": 1,
            "files": [
                {
                    "path": "00_constraint_seed.json",
                    "title": "种子元规则：运动安全哨兵",
                    "guard_for": ["validate_precision", "robot_move"]
                }
            ]
        })
    }

    #[test]
    fn test_render_contains_guard_line_and_boundary_text() {
        let text = render_l2_inventory_summary(&sample_inventory()).expect("非空清单应渲染");
        assert!(text.contains("00_constraint_seed.json"));
        assert!(text.contains("种子元规则"));
        assert!(text.contains("validate_precision, robot_move"));
        // 边界声明 + 禁项 + 路径约定四段齐全
        assert!(text.contains("不得修改或绕过"));
        assert!(text.contains("enforce"));
        assert!(text.contains("治理链晋升"));
        assert!(text.contains("payload"));
    }

    #[test]
    fn test_render_empty_inventory_returns_none() {
        let empty = serde_json::json!({"count": 0, "files": []});
        assert!(render_l2_inventory_summary(&empty).is_none());
    }

    #[test]
    fn test_render_l2_no_internal_numbering_tokens() {
        // 公开面纪律：模板文本禁内部编号字样（方案号/批次号模式）
        let text = render_l2_inventory_summary(&sample_inventory()).unwrap();
        for token in ["UV-", "P0", "P1", "P2", "W1", "W2", "W3", "号文", "批 "] {
            assert!(!text.contains(token), "模板文本不得含内部编号字样: {token}");
        }
    }

    // =========================================================================
    // 渲染模板（进化信号）
    // =========================================================================

    fn sample_response() -> Value {
        serde_json::json!({
            "session_id": 42,
            "total_violations": 5,
            "signals": [
                {
                    "kind": "violation",
                    "rule_ref": "rule_index=0",
                    "reason_summary": "运动指令缺少安全清场前置",
                    "count": 5,
                    "last_version": 23,
                    "last_instr_type": "branch"
                }
            ],
            "queue": { "pending_normal": 1, "pending_meta_promotion": 0 }
        })
    }

    #[test]
    fn test_render_contains_signals_and_queue() {
        let text = render_evolution_signals_summary(&sample_response()).expect("有信号应渲染");
        assert!(text.contains("会话 42"));
        assert!(text.contains("5 条违规拦截"));
        assert!(text.contains("rule_index=0"));
        assert!(text.contains("×5"));
        assert!(text.contains("v23"));
        assert!(text.contains("branch"));
        assert!(text.contains("运动指令缺少安全清场前置"));
        assert!(text.contains("待审普通规则 1 条"));
        assert!(text.contains("待审约束层晋升 0 条"));
        assert!(text.contains("人工审批"));
    }

    #[test]
    fn test_render_empty_signals_returns_none() {
        let empty = serde_json::json!({
            "session_id": 7,
            "total_violations": 0,
            "signals": [],
            "queue": { "pending_normal": 0, "pending_meta_promotion": 0 }
        });
        assert!(render_evolution_signals_summary(&empty).is_none());
    }

    #[test]
    fn test_render_signals_no_internal_numbering_tokens() {
        // 公开面纪律：模板文本禁内部编号字样（方案号/批次号模式）
        let text = render_evolution_signals_summary(&sample_response()).unwrap();
        for token in ["UV-", "P0", "P1", "P2", "W1", "W2", "W3", "号文", "批 "] {
            assert!(!text.contains(token), "模板文本不得含内部编号字样: {token}");
        }
    }

    // =========================================================================
    // register / specs
    // =========================================================================

    #[test]
    fn test_specs_count() {
        assert_eq!(specs().len(), 6);
    }

    #[test]
    fn test_register_tools() {
        let mut h = ToolHandler::new();
        register(&mut h, &make_ws(), &make_ev());
        for name in [
            "audit_verify",
            "bundle_export",
            "meta_summary",
            "evolution_signals",
            "rule_promote",
        ] {
            assert!(h.has_tool(name), "tool {name} should be registered");
        }
    }

    #[test]
    fn test_rule_promote_spec_hides_kind() {
        // rule_promote：kind 不暴露给 LLM（防旁路 normal 通道），人审必需参数齐全
        let specs = specs();
        let promote = specs.iter().find(|s| s.name == "rule_promote").unwrap();
        let names: Vec<&str> = promote.parameters.iter().map(|p| p.name.as_str()).collect();
        for expected in [
            "workspace_id",
            "rule_version_ids",
            "meta_rule_content",
            "submitted_by",
            "role",
        ] {
            assert!(names.contains(&expected), "rule_promote 缺参数 {expected}");
        }
        assert!(!names.contains(&"kind"), "kind 不得暴露给 LLM（防旁路）");
    }

    // =========================================================================
    // audit_verify
    // =========================================================================

    #[tokio::test]
    async fn test_audit_verify_missing_session_id() {
        let tool = AuditVerifyTool::new(make_ev());
        let args = Value::Object(serde_json::Map::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: session_id"));
    }

    // =========================================================================
    // bundle_export
    // =========================================================================

    #[tokio::test]
    async fn test_bundle_export_missing_dataset_id() {
        let tool = BundleExportTool::new(make_ws());
        let args = Value::Object(serde_json::Map::new());
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("missing required parameter: dataset_id"));
    }

    #[tokio::test]
    async fn test_bundle_export_invalid_verdict() {
        let tool = BundleExportTool::new(make_ws());
        let mut m = serde_json::Map::new();
        m.insert("dataset_id".to_string(), Value::from("ds1"));
        m.insert("version".to_string(), Value::from("v1"));
        m.insert("verdict".to_string(), Value::from("maybe"));
        let args = Value::Object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("verdict must be"));
    }

    #[tokio::test]
    async fn test_bundle_export_pass_without_traceable_subset_rejected() {
        // 前置形状校验：pass + 空 subset → 拒绝（与治理域 回归验证 B1 同口径）
        let tool = BundleExportTool::new(make_ws());
        let mut m = serde_json::Map::new();
        m.insert("dataset_id".to_string(), Value::from("ds1"));
        m.insert("version".to_string(), Value::from("v1"));
        m.insert("verdict".to_string(), Value::from("pass"));
        let args = Value::Object(m);
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
        let mut m = serde_json::Map::new();
        m.insert("dataset_id".to_string(), Value::from("ds1"));
        m.insert("version".to_string(), Value::from("v1"));
        m.insert("verdict".to_string(), Value::from("pass"));
        m.insert(
            "subset".to_string(),
            Value::Array(vec![Value::from("opaque-ref")]),
        );
        let args = Value::Object(m);
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
        let mut m = serde_json::Map::new();
        m.insert("dataset_id".to_string(), Value::from("ds1"));
        m.insert("version".to_string(), Value::from("v1"));
        m.insert("verdict".to_string(), Value::from("fail"));
        let args = Value::Object(m);
        let result = tool.call(&args).await;
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(!msg.contains("missing required parameter"));
        assert!(!msg.contains("requires traceable evidence"));
    }

    // =========================================================================
    // skill_pack_to_bundle
    // =========================================================================

    /// 最小合法 pack fixture：骨架规则体与 skill-adapter tool-router 骨架同构。
    fn demo_pack() -> Value {
        serde_json::json!({
            "pack_id": "skill-pack-demo",
            "source_skill": {"name": "demo-skill", "version": "1.0.0",
                             "path": "skills/demo/SKILL.md", "description": "demo"},
            "rules": [{
                "entry_id": "demo-a-01",
                "domain": "audit",
                "tags": ["skill-adapter", "tpl-tool-router"],
                "rule_body": {
                    "rule_id": "demo-a-01", "version": 1, "description": "router skeleton",
                    "transform": [{"type": "branch", "params": {
                        "domain": {"type": "instruction", "instruction_type": "audit_tool"},
                        "on_true": [{"type": "push",
                                     "params": {"instructions": [{"type": "noop"}]}}],
                        "on_false": []}}]
                }
            }],
            "knowledge_index": [{"key": "k1", "summary": "s", "trigger_kw": ["t"],
                                 "src": "x#k1", "kind": "ref"}],
            "llm_core_note": [],
            "machine_judge": {"items": []},
            "coverage": {"unmapped": 0}
        })
    }

    #[tokio::test]
    async fn test_skill_pack_to_bundle_happy_fail() {
        let tool = SkillPackToBundleTool::new();
        let args = serde_json::json!({ "pack": demo_pack() });
        let out = tool.call(&args).await.expect("convert should succeed");
        let bundle = &out["bundle"];
        assert_eq!(bundle["bundle_id"], "bundle-skill-pack-demo");
        assert_eq!(bundle["bundle_schema_version"], "1.0");
        assert_eq!(bundle["dataset"]["dataset_id"], "skill-pack-demo");
        assert_eq!(bundle["dataset"]["version_selection"]["mode"], "pinned");
        assert_eq!(bundle["entries"][0]["entry_id"], "demo-a-01");
        assert_eq!(bundle["entries"][0]["entry_kind"], "rule");
        assert_eq!(
            bundle["entries"][0]["provenance"]["source"],
            "skill-adapter:demo-skill@1.0.0"
        );
        assert_eq!(bundle["tests"]["verdict"], "fail");
        assert!(bundle["audit"]["content_hash"]
            .as_str()
            .unwrap()
            .starts_with("blake3:"));
        assert_eq!(out["validation"]["entry_count"], 1);
        assert!(
            out["validation"]["structural_errors"]
                .as_array()
                .unwrap()
                .is_empty(),
            "skeleton rule must pass structural gate"
        );
        assert!(
            out["validation"]["gate_one_status"]
                .as_str()
                .unwrap()
                .starts_with("fail"),
            "unverified skeleton must land on gate one (expected fail)"
        );
    }

    #[tokio::test]
    async fn test_skill_pack_to_bundle_verify_hash_roundtrip() {
        let tool = SkillPackToBundleTool::new();
        let args = serde_json::json!({ "pack": demo_pack() });
        let out = tool.call(&args).await.expect("convert should succeed");
        let rebuilt: DatasetBundle =
            serde_json::from_value(out["bundle"].clone()).expect("bundle must round-trip");
        rebuilt
            .verify_content_hash()
            .expect("content hash must verify after JSON round-trip");
    }

    #[tokio::test]
    async fn test_skill_pack_to_bundle_pass_parameter_rejected() {
        let tool = SkillPackToBundleTool::new();
        // fail-closed 硬边界：verdict/evidence 参数已移除，任一出现即拒
        // （凭空构造的包无治理域在先证据，不持有 pass 发放权）。
        let args = serde_json::json!({ "pack": demo_pack(), "verdict": "pass" });
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("parameters are not accepted"));

        let tool = SkillPackToBundleTool::new();
        let args = serde_json::json!({
            "pack": demo_pack(),
            "verdict": "fail",
            "evidence": ["sandbox:sb-1"]
        });
        let result = tool.call(&args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("parameters are not accepted"));
    }

    #[tokio::test]
    async fn test_skill_pack_to_bundle_structural_error() {
        let mut pack = demo_pack();
        pack["rules"][0]["rule_body"] = serde_json::json!({
            "rule_id": "demo-a-01", "version": 1,
            "transform": [{"type": "nonexistent_hint"}]
        });
        let tool = SkillPackToBundleTool::new();
        let args = serde_json::json!({ "pack": pack });
        let out = tool
            .call(&args)
            .await
            .expect("convert itself should succeed");
        let errs = out["validation"]["structural_errors"]
            .as_array()
            .expect("structural errors list");
        assert!(!errs.is_empty(), "illegal rule_body must be reported");
    }

    #[tokio::test]
    async fn test_skill_pack_to_bundle_knowledge_not_included() {
        let tool = SkillPackToBundleTool::new();
        let args = serde_json::json!({ "pack": demo_pack() });
        let out = tool.call(&args).await.expect("convert should succeed");
        // pack 含 1 条 knowledge_index + 0 条 llm_core：bundle 只收 rules（1 条）
        assert_eq!(out["bundle"]["entries"].as_array().unwrap().len(), 1);
        assert_eq!(out["validation"]["entry_count"], 1);
    }

    // =========================================================================
    // meta_summary / evolution_signals / rule_promote
    // =========================================================================

    #[test]
    fn test_missing_session_id_is_error() {
        let err = parse_session_id(&serde_json::json!({})).unwrap_err();
        assert!(err.contains("missing required parameter: session_id"));
    }

    #[test]
    fn test_parse_session_id_accepts_number_and_string() {
        assert_eq!(
            parse_session_id(&serde_json::json!({"session_id": 42})).unwrap(),
            42
        );
        assert_eq!(
            parse_session_id(&serde_json::json!({"session_id": "42"})).unwrap(),
            42
        );
        assert!(parse_session_id(&serde_json::json!({"session_id": "abc"})).is_err());
        assert!(parse_session_id(&serde_json::json!({"session_id": -1})).is_err());
    }

    #[test]
    fn test_parse_rule_version_ids_flex_shapes() {
        assert_eq!(
            parse_rule_version_ids(&serde_json::json!({"rule_version_ids": "rv7"})).unwrap(),
            vec!["rv7".to_string()]
        );
        assert_eq!(
            parse_rule_version_ids(&serde_json::json!({"rule_version_ids": ["rv1", "rv2"]}))
                .unwrap(),
            vec!["rv1".to_string(), "rv2".to_string()]
        );
        assert!(parse_rule_version_ids(&serde_json::json!({})).is_err());
        assert!(parse_rule_version_ids(&serde_json::json!({"rule_version_ids": []})).is_err());
        assert!(parse_rule_version_ids(&serde_json::json!({"rule_version_ids": [1, 2]})).is_err());
    }

    #[tokio::test]
    async fn test_rule_promote_missing_required_params_fail_fast() {
        // 缺任一必需参数 → 错误文本，不发请求
        let tool = RulePromoteTool::new(make_ws());
        let base = serde_json::json!({
            "workspace_id": "ws1",
            "rule_version_ids": ["rv1"],
            "meta_rule_content": "{\"metadata\":{\"tier\":\"constraint\"}}",
            "submitted_by": "agent-01",
            "role": "DepartmentHead"
        });
        for key in [
            "workspace_id",
            "rule_version_ids",
            "meta_rule_content",
            "submitted_by",
            "role",
        ] {
            let mut args = base.clone();
            args.as_object_mut().unwrap().remove(key);
            let err = tool.call(&args).await.unwrap_err();
            assert!(
                err.contains(&format!("missing required parameter: {key}")),
                "缺 {key} 应报缺参错误，实际: {err}"
            );
        }
    }

    #[tokio::test]
    async fn test_evolution_signals_fail_soft_on_client_error() {
        // 端点不可达（localhost:0 连接失败）→ 工具返回 Err 文本，不 panic
        let tool = EvolutionSignalsTool::new(make_ev());
        let args = serde_json::json!({"session_id": 42});
        let result = tool.call(&args).await;
        assert!(result.is_err(), "拉取失败应向调用方透出错误而非空结果");
    }

    #[tokio::test]
    async fn test_meta_summary_fail_soft_on_client_error() {
        // 端点不可达（localhost:0 连接失败）→ 工具返回 Err 文本，不 panic
        let tool = MetaSummaryTool::new(make_ev());
        let args = Value::Object(serde_json::Map::new());
        let result = tool.call(&args).await;
        assert!(result.is_err(), "拉取失败应向调用方透出错误而非空结果");
    }
}
