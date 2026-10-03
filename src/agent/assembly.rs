// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 组装配方 AssemblyRecipe——元层先行批（F-302 升级 ①版本落账深化 + ②配方数据外置）
//!
//! 范围裁定（专项设计档 §三,2026-10-03）:配方 = **槽位级声明数据**——槽位清单/
//! 顺序/开关、预算数字、降级顺序声明、召回范围、裁剪选型与参数;槽内渲染与算法 =
//! **机制**（确定性纯函数执行器）。配方变更 = minor（recipe-v1.x）;执行器算法变更 =
//! major（assembly-v2.x,三阶版本落账见 runner.rs `effective_params`）。
//!
//! 等价迁移铁律:[`AssemblyRecipe::default`] 逐字段 = 组装主干现状硬编码值
//! （实测锚点:memory.rs `ContextBudget`/`fit_recall`/`build_system_prompt_with_recall`、
//! context_window.rs `KeepSystemKeepLast`、runner.rs `TOOL_RESULT_MAX_CHARS`/
//! 响应预留 1/4）——definition 未声明 assembly 段时,行为与历史版本逐字节一致。
//!
//! 宿主形态:definition 内嵌段 `assembly`（Option,照 capability_boundary 先例）;
//! 扩展形态 `$ref` 外部配方文件（共享/复用场景,相对 definition 目录解析）。
//! 热重载不做 = ③引擎收编范围。

use std::path::Path;

use serde::{Deserialize, Serialize};

/// 默认配方版本（recipe-v1.0 = 现状硬编码值的规则化形态）
pub const DEFAULT_RECIPE_VERSION: &str = "recipe-v1.0";

/// 槽位来源白名单（执行器只认这些来源;新增来源 = 执行器升级 = major）
pub const SLOT_SOURCES: &[&str] = &[
    "definition.system_prompt",
    "recall",
    "definition.capability_boundary.awareness_segment",
    "manifest",
    "goal",
    "rolling_summary_or_hint",
    "messages",
];

/// 记忆区降级层白名单（降级顺序声明的合法值）
pub const DEGRADATION_LAYERS: &[&str] = &["stable", "summaries", "events"];

/// 裁剪策略白名单（现状唯一实现;新增策略 = 执行器升级 = major）
pub const TRIM_STRATEGIES: &[&str] = &["KeepSystemKeepLast"];

/// 预算基准白名单（A-1 口径:现状 `total_window`;修正为 `input` 属配方 minor 变更）
pub const BUDGET_BASES: &[&str] = &["total_window", "input"];

/// 单槽位声明
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotSpec {
    /// 槽位标识（拼接序内唯一;如 "S1_base"/"S3_memory"）
    pub id: String,
    /// 内容来源（白名单见 [`SLOT_SOURCES`]）
    pub source: String,
    /// 超预算时是否可降级裁剪（false = 硬注入,如系统提示）
    #[serde(default)]
    pub degradable: bool,
    /// 内容缺失时是否静默跳过（true = 无内容不报错,如边界段未声明）
    #[serde(default)]
    pub optional: bool,
    /// 开关（false = 槽位停用;如 S4b_skills 预留位）
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 记忆区预算声明（仅 source=recall 槽位）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<SlotBudget>,
    /// 降级顺序声明（白名单见 [`DEGRADATION_LAYERS`];执行器按声明序跑降级）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degradation_order: Option<Vec<String>>,
    /// 区内部位属性（notices_first 等;标题/行格式留机制）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<SlotSections>,
    /// 与前文的分隔符（如 "\n\n"）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separator: Option<String>,
    /// 消息角色声明（如 S5_task → "user"）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// 裁剪选型与参数（仅 source=messages 槽位）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trim: Option<TrimSpec>,
}

/// 槽位预算声明（数字进配方,算法留机制）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotBudget {
    /// 记忆区占窗口比例（现状 0.25;越声明 clamp 区间加载期即拒）
    pub ratio: f32,
    /// 预算基准（"total_window" = A-1 现状口径;"input" = 修正口径,PR-4 演示）
    pub base: String,
    /// 收敛区间 [min, max]（现状 clamp(0.1, 0.5)）
    pub clamp: [f32; 2],
}

/// 区内部位属性（位置数据化;分区标题/行格式/notices 文案留机制）
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SlotSections {
    /// 降级通知置于记忆区最前（F3 fail-visible,现状 true）
    #[serde(default = "default_true")]
    pub notices_first: bool,
}

/// 裁剪选型与参数（选型 = 数据,算法 = 机制）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrimSpec {
    /// 策略名（白名单见 [`TRIM_STRATEGIES`]）
    pub strategy: String,
    /// 预算缓冲百分比（现状 budget/20 = 5%,最低 5 token 的下限留机制）
    #[serde(default = "default_buffer_pct")]
    pub buffer_pct: u32,
    /// 截断提示消息的预算 token（现状 15）
    #[serde(default = "default_hint_budget_tokens")]
    pub hint_budget_tokens: usize,
}

/// 全局预算声明
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetSpec {
    /// 窗口 token 来源（"definition.context_window_tokens" = 单一事实源）
    #[serde(default = "default_window_tokens_source")]
    pub window_tokens_source: String,
    /// 响应预留百分比（现状 max_tokens/4 = 25%）
    #[serde(default = "default_reserve_pct")]
    pub reserve_for_response_pct: u32,
    /// 工具输出回喂字符上限（现状 TOOL_RESULT_MAX_CHARS = 48000）
    #[serde(default = "default_tool_result_max_chars")]
    pub tool_result_max_chars: usize,
}

/// 召回范围声明（引用式:单一事实源在 definition.memory,配方只声明「从哪取」,
/// 防两处配置漂移）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecallScopeSpec {
    /// 会话摘要条数来源（"sediment_config" = definition.memory.max_session_summaries）
    #[serde(default = "default_sediment_ref")]
    pub max_session_summaries_ref: String,
    /// 注入事件条数来源（"sediment_config" = definition.memory.max_injected_events）
    #[serde(default = "default_sediment_ref")]
    pub max_injected_events_ref: String,
}

fn default_true() -> bool {
    true
}
fn default_buffer_pct() -> u32 {
    5
}
fn default_hint_budget_tokens() -> usize {
    15
}
fn default_window_tokens_source() -> String {
    "definition.context_window_tokens".to_string()
}
fn default_reserve_pct() -> u32 {
    25
}
fn default_tool_result_max_chars() -> usize {
    48_000
}
fn default_sediment_ref() -> String {
    "sediment_config".to_string()
}
fn default_recipe_version() -> String {
    DEFAULT_RECIPE_VERSION.to_string()
}

impl Default for SlotSections {
    fn default() -> Self {
        Self { notices_first: true }
    }
}

impl Default for BudgetSpec {
    fn default() -> Self {
        Self {
            window_tokens_source: default_window_tokens_source(),
            reserve_for_response_pct: default_reserve_pct(),
            tool_result_max_chars: default_tool_result_max_chars(),
        }
    }
}

impl Default for RecallScopeSpec {
    fn default() -> Self {
        Self {
            max_session_summaries_ref: default_sediment_ref(),
            max_injected_events_ref: default_sediment_ref(),
        }
    }
}

/// 等价迁移:现状组装主干的槽位声明形态（逐字段 = 硬编码值,勿单点修改——
/// 改了就是行为变更,须走配方版本 minor 升级）
fn default_slots() -> Vec<SlotSpec> {
    vec![
        SlotSpec {
            id: "S1_base".to_string(),
            source: "definition.system_prompt".to_string(),
            degradable: false,
            optional: false,
            enabled: true,
            budget: None,
            degradation_order: None,
            sections: None,
            separator: None,
            role: None,
            trim: None,
        },
        SlotSpec {
            id: "S3_memory".to_string(),
            source: "recall".to_string(),
            degradable: true,
            optional: false,
            enabled: true,
            budget: Some(SlotBudget {
                ratio: 0.25,
                base: "total_window".to_string(),
                clamp: [0.1, 0.5],
            }),
            degradation_order: Some(
                DEGRADATION_LAYERS.iter().map(|s| s.to_string()).collect(),
            ),
            sections: Some(SlotSections::default()),
            separator: None,
            role: None,
            trim: None,
        },
        SlotSpec {
            id: "S4_boundary".to_string(),
            source: "definition.capability_boundary.awareness_segment".to_string(),
            degradable: false,
            optional: true,
            enabled: true,
            budget: None,
            degradation_order: None,
            sections: None,
            separator: Some("\n\n".to_string()),
            role: None,
            trim: None,
        },
        SlotSpec {
            id: "S4b_skills".to_string(),
            source: "manifest".to_string(),
            degradable: false,
            optional: true,
            enabled: false, // B2 v0.4 预留位,现状不注入
            budget: None,
            degradation_order: None,
            sections: None,
            separator: None,
            role: None,
            trim: None,
        },
        SlotSpec {
            id: "S5_task".to_string(),
            source: "goal".to_string(),
            degradable: false,
            optional: false,
            enabled: true,
            budget: None,
            degradation_order: None,
            sections: None,
            separator: None,
            role: Some("user".to_string()),
            trim: None,
        },
        SlotSpec {
            id: "S6_summary".to_string(),
            source: "rolling_summary_or_hint".to_string(),
            degradable: false,
            optional: false,
            enabled: true,
            budget: None,
            degradation_order: None,
            sections: None,
            separator: None,
            role: None,
            trim: None,
        },
        SlotSpec {
            id: "S7_history".to_string(),
            source: "messages".to_string(),
            degradable: false,
            optional: false,
            enabled: true,
            budget: None,
            degradation_order: None,
            sections: None,
            separator: None,
            role: None,
            trim: Some(TrimSpec {
                strategy: "KeepSystemKeepLast".to_string(),
                buffer_pct: 5,
                hint_budget_tokens: 15,
            }),
        },
    ]
}

/// 组装配方:上下文组装主干的槽位级声明数据（范式判据——组装策略从「写死在
/// 框架代码」变为「表达为可转移的规则数据」）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssemblyRecipe {
    /// 配方版本（minor = 槽位/参数变更;执行器算法变更 = major 另行落账）
    #[serde(default = "default_recipe_version")]
    pub recipe_version: String,
    /// 槽位清单（数组序 = 拼接序）
    #[serde(default = "default_slots")]
    pub slots: Vec<SlotSpec>,
    /// 全局预算
    #[serde(default)]
    pub budget: BudgetSpec,
    /// 召回范围（引用式）
    #[serde(default)]
    pub recall_scope: RecallScopeSpec,
}

impl Default for AssemblyRecipe {
    /// 等价迁移默认配方:逐字段 = 现状硬编码值（等价迁移铁律,勿单点修改;
    /// 行为变更须走配方版本 minor 升级,参照 PR-4 演示）
    fn default() -> Self {
        Self {
            recipe_version: default_recipe_version(),
            slots: default_slots(),
            budget: BudgetSpec::default(),
            recall_scope: RecallScopeSpec::default(),
        }
    }
}

impl AssemblyRecipe {
    /// 配方语义校验(加载期 fail-fast——反模式「配置错误运行期才爆」)
    ///
    /// 拦截:槽位 id 空/重复、来源白名单外、ratio 越声明 clamp 区间、clamp 区间
    /// 本身非法、降级序未知层/重复、裁剪策略白名单外、骨架槽位缺失、全局预算越界。
    pub fn validate(&self) -> Result<(), String> {
        if self.slots.is_empty() {
            return Err("slots must not be empty".to_string());
        }
        let mut seen = std::collections::HashSet::new();
        for slot in &self.slots {
            if slot.id.is_empty() {
                return Err(format!(
                    "slot id must not be empty (source: {})",
                    slot.source
                ));
            }
            if !seen.insert(slot.id.as_str()) {
                return Err(format!("duplicate slot id '{}'", slot.id));
            }
            if !SLOT_SOURCES.contains(&slot.source.as_str()) {
                return Err(format!(
                    "slot '{}' has unknown source '{}' (allowed: {:?})",
                    slot.id, slot.source, SLOT_SOURCES
                ));
            }
            if let Some(b) = &slot.budget {
                if !BUDGET_BASES.contains(&b.base.as_str()) {
                    return Err(format!(
                        "slot '{}' budget.base '{}' unknown (allowed: {:?})",
                        slot.id, b.base, BUDGET_BASES
                    ));
                }
                let [lo, hi] = b.clamp;
                if !lo.is_finite() || !hi.is_finite() || lo <= 0.0 || hi > 1.0 || lo >= hi {
                    return Err(format!(
                        "slot '{}' budget.clamp [{}, {}] invalid (require 0 < min < max <= 1)",
                        slot.id, lo, hi
                    ));
                }
                if !(b.ratio >= lo && b.ratio <= hi) {
                    return Err(format!(
                        "slot '{}' budget.ratio {} out of declared clamp [{}, {}]",
                        slot.id, b.ratio, lo, hi
                    ));
                }
            }
            if let Some(order) = &slot.degradation_order {
                if order.is_empty() {
                    return Err(format!(
                        "slot '{}' degradation_order must not be empty",
                        slot.id
                    ));
                }
                let mut layer_seen = std::collections::HashSet::new();
                for layer in order {
                    if !DEGRADATION_LAYERS.contains(&layer.as_str()) {
                        return Err(format!(
                            "slot '{}' degradation_order has unknown layer '{}' (allowed: {:?})",
                            slot.id, layer, DEGRADATION_LAYERS
                        ));
                    }
                    if !layer_seen.insert(layer.as_str()) {
                        return Err(format!(
                            "slot '{}' degradation_order repeats layer '{}'",
                            slot.id, layer
                        ));
                    }
                }
            }
            if let Some(t) = &slot.trim {
                if !TRIM_STRATEGIES.contains(&t.strategy.as_str()) {
                    return Err(format!(
                        "slot '{}' trim.strategy '{}' unknown (allowed: {:?})",
                        slot.id, t.strategy, TRIM_STRATEGIES
                    ));
                }
                if t.buffer_pct > 50 {
                    return Err(format!(
                        "slot '{}' trim.buffer_pct {} out of range [0, 50]",
                        slot.id, t.buffer_pct
                    ));
                }
            }
        }
        // 组装骨架必填:S1_base 系统提示 / S5_task 任务 / S7_history 消息历史
        // （S3_memory 非必填——memory_type=none 场景合法;S4/S4b/S6 本就 optional/可空）
        for required in ["S1_base", "S5_task", "S7_history"] {
            if !self.slots.iter().any(|s| s.id == required) {
                return Err(format!("required slot '{}' missing", required));
            }
        }
        if self.budget.reserve_for_response_pct == 0
            || self.budget.reserve_for_response_pct >= 100
        {
            return Err(format!(
                "budget.reserve_for_response_pct {} out of range (1..99)",
                self.budget.reserve_for_response_pct
            ));
        }
        if self.budget.tool_result_max_chars == 0 {
            return Err("budget.tool_result_max_chars must be > 0".to_string());
        }
        Ok(())
    }
}

/// 解析 definition JSON 树中的 assembly `$ref` 外部配方引用（原位替换为内嵌形态）
///
/// `"assembly": {"$ref": "recipes/assembly-v1.json"}` 相对 definition 文件所在
/// 目录解析;替换后反序列化/校验只见内嵌形态。防路径穿越:拒绝绝对路径与
/// 含 `..` 段（与 agent_type 门卫同族）。错误为可读字符串,由调用方包装。
pub fn resolve_assembly_ref(value: &mut serde_json::Value, base_dir: &Path) -> Result<(), String> {
    let Some(obj) = value.get("assembly") else {
        return Ok(()); // 未声明 assembly = 内置默认配方,无需解析
    };
    let Some(ref_path) = obj.get("$ref").and_then(|v| v.as_str()) else {
        return Ok(()); // 内嵌形态,原样
    };
    // $ref 形态必须纯引用(混合内嵌字段 = 歧义,拒绝)
    let extra = obj
        .as_object()
        .map(|o| o.keys().filter(|k| k.as_str() != "$ref").count())
        .unwrap_or(0);
    if extra > 0 {
        return Err("assembly $ref object must contain only the '$ref' key".to_string());
    }
    let rel = Path::new(ref_path);
    if rel.is_absolute() {
        return Err(format!(
            "assembly $ref '{}' must be a relative path",
            ref_path
        ));
    }
    if rel
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(format!(
            "assembly $ref '{}' must not contain '..' (path traversal guard)",
            ref_path
        ));
    }
    let full = base_dir.join(rel);
    let content = std::fs::read_to_string(&full)
        .map_err(|e| format!("assembly $ref '{}' read failed: {}", ref_path, e))?;
    let recipe: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("assembly $ref '{}' parse failed: {}", ref_path, e))?;
    value
        .as_object_mut()
        .expect("definition root must be an object")
        .insert("assembly".to_string(), recipe);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 等价迁移铁律的机械执行:Default 逐字段 = 现状硬编码值对照表
    /// （锚点:memory.rs ContextBudget::new/fit_recall/build_system_prompt_with_recall、
    /// context_window.rs trim_keep_system_keep_last、runner.rs TOOL_RESULT_MAX_CHARS
    /// 与响应预留 1/4）
    #[test]
    fn test_default_recipe_matches_current_hardcoded_values() {
        let r = AssemblyRecipe::default();
        assert_eq!(r.recipe_version, "recipe-v1.0");
        let ids: Vec<&str> = r.slots.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "S1_base",
                "S3_memory",
                "S4_boundary",
                "S4b_skills",
                "S5_task",
                "S6_summary",
                "S7_history"
            ]
        );
        // S3 记忆区:C3 默认 0.25 + clamp(0.1,0.5) + 降级序 stable>summaries>events + notices 置前
        let s3 = &r.slots[1];
        assert_eq!(s3.source, "recall");
        assert!(s3.degradable);
        let b = s3.budget.as_ref().unwrap();
        assert!((b.ratio - 0.25).abs() < 1e-6);
        assert_eq!(b.base, "total_window"); // A-1 现状口径(PR-4 才修正)
        assert_eq!(b.clamp, [0.1, 0.5]);
        assert_eq!(
            s3.degradation_order.as_ref().unwrap(),
            &["stable", "summaries", "events"]
        );
        assert!(s3.sections.unwrap().notices_first);
        // S4 边界段:optional + "\n\n"
        assert!(r.slots[2].optional);
        assert_eq!(r.slots[2].separator.as_deref(), Some("\n\n"));
        // S4b:B2 v0.4 预留位,现状停用
        assert!(!r.slots[3].enabled);
        // S5 任务:user 角色
        assert_eq!(r.slots[4].role.as_deref(), Some("user"));
        // S7 裁剪:KeepSystemKeepLast + buffer 5% + hint 15
        let t = r.slots[6].trim.as_ref().unwrap();
        assert_eq!(t.strategy, "KeepSystemKeepLast");
        assert_eq!(t.buffer_pct, 5);
        assert_eq!(t.hint_budget_tokens, 15);
        // 全局预算:响应预留 25% + 工具输出 48000 + 窗口来源
        assert_eq!(r.budget.reserve_for_response_pct, 25);
        assert_eq!(r.budget.tool_result_max_chars, 48_000);
        assert_eq!(
            r.budget.window_tokens_source,
            "definition.context_window_tokens"
        );
        // 召回范围:引用式
        assert_eq!(r.recall_scope.max_session_summaries_ref, "sediment_config");
        assert_eq!(r.recall_scope.max_injected_events_ref, "sediment_config");
    }

    /// serde 缺省形态与 Default 等价(空对象 = 内置默认配方)
    #[test]
    fn test_empty_object_deserializes_to_default() {
        let r: AssemblyRecipe = serde_json::from_str("{}").unwrap();
        assert_eq!(r, AssemblyRecipe::default());
    }

    /// 默认配方通过自身校验(自洽)
    #[test]
    fn test_default_recipe_passes_validate() {
        AssemblyRecipe::default()
            .validate()
            .expect("default recipe must be valid");
    }

    /// ratio 越声明 clamp 区间:fail-fast
    #[test]
    fn test_validate_rejects_ratio_out_of_clamp() {
        let mut r = AssemblyRecipe::default();
        r.slots[1].budget.as_mut().unwrap().ratio = 0.9;
        let err = r.validate().unwrap_err();
        assert!(err.contains("budget.ratio"), "got: {}", err);
    }

    /// clamp 区间本身非法(min >= max)
    #[test]
    fn test_validate_rejects_invalid_clamp() {
        let mut r = AssemblyRecipe::default();
        r.slots[1].budget.as_mut().unwrap().clamp = [0.5, 0.1];
        assert!(r.validate().unwrap_err().contains("clamp"));
    }

    /// 槽位 id 重复:拒绝
    #[test]
    fn test_validate_rejects_duplicate_slot_id() {
        let mut r = AssemblyRecipe::default();
        let dup = r.slots[0].clone();
        r.slots.push(dup);
        assert!(r.validate().unwrap_err().contains("duplicate slot id"));
    }

    /// 未知来源:拒绝
    #[test]
    fn test_validate_rejects_unknown_source() {
        let mut r = AssemblyRecipe::default();
        r.slots[0].source = "definition.system_prompt2".to_string();
        assert!(r.validate().unwrap_err().contains("unknown source"));
    }

    /// 降级序未知层:拒绝
    #[test]
    fn test_validate_rejects_unknown_degradation_layer() {
        let mut r = AssemblyRecipe::default();
        r.slots[1].degradation_order.as_mut().unwrap()[0] = "chat_history".to_string();
        assert!(r.validate().unwrap_err().contains("unknown layer"));
    }

    /// 未知裁剪策略:拒绝
    #[test]
    fn test_validate_rejects_unknown_trim_strategy() {
        let mut r = AssemblyRecipe::default();
        r.slots[6].trim.as_mut().unwrap().strategy = "DropEverything".to_string();
        assert!(r.validate().unwrap_err().contains("trim.strategy"));
    }

    /// 骨架槽位缺失:拒绝
    #[test]
    fn test_validate_rejects_missing_required_slot() {
        let mut r = AssemblyRecipe::default();
        r.slots.retain(|s| s.id != "S7_history");
        assert!(r.validate().unwrap_err().contains("required slot"));
    }

    /// 响应预留百分比越界:拒绝
    #[test]
    fn test_validate_rejects_bad_reserve_pct() {
        let mut r = AssemblyRecipe::default();
        r.budget.reserve_for_response_pct = 100;
        assert!(r.validate().unwrap_err().contains("reserve_for_response_pct"));
    }

    /// 内嵌正例:自定义参数通过(改配方不改码的承载面)
    #[test]
    fn test_deserialize_inline_custom_recipe() {
        let json = r#"{
            "recipe_version": "recipe-v1.0",
            "slots": [
                { "id": "S1_base", "source": "definition.system_prompt" },
                { "id": "S5_task", "source": "goal", "role": "user" },
                { "id": "S7_history", "source": "messages",
                  "trim": { "strategy": "KeepSystemKeepLast", "buffer_pct": 10, "hint_budget_tokens": 20 } }
            ],
            "budget": { "reserve_for_response_pct": 30, "tool_result_max_chars": 10000 }
        }"#;
        let r: AssemblyRecipe = serde_json::from_str(json).unwrap();
        r.validate().expect("custom recipe must be valid");
        assert_eq!(r.budget.reserve_for_response_pct, 30);
        assert_eq!(r.slots[2].trim.as_ref().unwrap().buffer_pct, 10);
    }

    /// $ref 解析:正例 + 穿越拒绝 + 绝对路径拒绝 + 混合形态拒绝 + 未声明无操作
    #[test]
    fn test_resolve_assembly_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let recipe_dir = tmp.path().join("recipes");
        std::fs::create_dir_all(&recipe_dir).unwrap();
        let recipe_path = recipe_dir.join("assembly-v1.json");
        std::fs::write(&recipe_path, r#"{"recipe_version":"recipe-v1.0"}"#).unwrap();

        // 正例:相对路径引用被原位替换为内嵌形态
        let mut doc = serde_json::json!({
            "agent_type": "x",
            "assembly": { "$ref": "recipes/assembly-v1.json" }
        });
        resolve_assembly_ref(&mut doc, tmp.path()).unwrap();
        assert_eq!(doc["assembly"]["recipe_version"], "recipe-v1.0");

        // 穿越拒绝
        let mut doc = serde_json::json!({ "assembly": { "$ref": "../outside.json" } });
        assert!(resolve_assembly_ref(&mut doc, tmp.path()).is_err());

        // 绝对路径拒绝
        let mut doc =
            serde_json::json!({ "assembly": { "$ref": recipe_path.to_str().unwrap() } });
        assert!(resolve_assembly_ref(&mut doc, tmp.path()).is_err());

        // 混合形态拒绝
        let mut doc = serde_json::json!({
            "assembly": { "$ref": "recipes/assembly-v1.json", "slots": [] }
        });
        assert!(resolve_assembly_ref(&mut doc, tmp.path()).is_err());

        // 未声明 assembly:无操作
        let mut doc = serde_json::json!({ "agent_type": "x" });
        resolve_assembly_ref(&mut doc, tmp.path()).unwrap();
        assert!(doc.get("assembly").is_none());
    }
}
