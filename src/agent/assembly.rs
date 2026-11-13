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

/// 默认配方版本（recipe-v1.0 = 现状硬编码值的规则化形态；recipe-v1.1 =
/// 批K 段4 增补 S3c_todo 槽位——optional 槽,无 todo 态时逐字节等价 v1.0；
/// recipe-v1.2 = 增补 S2c_agents_md 槽位——项目记忆段(AGENTS.md 层级)
/// 从 system prompt 尾段挪入静态区(知识契约之后、记忆召回之前),optional
/// 槽,无段时逐字节等价 v1.1;动态区(recall/todo)集中尾置的前缀稳定前提；
/// recipe-v1.3 = 增补 S2d_catalog 槽位——服务能力目录段(静态区尾槽,
/// 项目记忆之后、记忆召回之前),optional 槽,注册表空时逐字节等价 v1.2）
pub const DEFAULT_RECIPE_VERSION: &str = "recipe-v1.3";

/// 槽位来源白名单（执行器只认这些来源;新增来源 = 执行器升级 = major）
pub const SLOT_SOURCES: &[&str] = &[
    "definition.system_prompt",
    "governance_segment",
    "knowledge_segment",
    "agents_md",
    "service_catalog",
    "recall",
    "todo_summary",
    "definition.capability_boundary.awareness_segment",
    "manifest",
    "goal",
    "rolling_summary_or_hint",
    "messages",
];

/// 记忆区降级层白名单（降级顺序声明的合法值）
pub const DEGRADATION_LAYERS: &[&str] = &["stable", "summaries", "events"];

/// 记忆区预算缺省比例（L-1 残余单源化：S3_memory 声明与槽位未声明
/// budget 时两个 None 分支的默认口径共引此常数，消灭三处字面量重复）
const DEFAULT_MEMORY_RATIO: f32 = 0.25;

/// 裁剪策略白名单（现状唯一实现;新增策略 = 执行器升级 = major）
pub const TRIM_STRATEGIES: &[&str] = &["KeepSystemKeepLast"];

/// 预算基准白名单(`total_window` = 现状口径;`input` = A-1 修正口径,
/// PR-4 起执行器已消费——配方 minor 变更即可切换,不改码)
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
    /// 预算基准（"total_window" = 现状口径;"input" = A-1 修正口径,
    /// 扣除响应预留后作基数）
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
    /// 工具输出回喂字符上限（现状 TOOL_RESULT_MAX_CHARS = 48000;
    /// K-1 案 A 后为旧键——生效值按 ASCII 基准 /4 折算 token,仅在
    /// 新键未显式声明时参与折算,见 [`BudgetSpec::tool_result_max_tokens`])
    #[serde(default = "default_tool_result_max_chars")]
    pub tool_result_max_chars: usize,
    /// 工具输出回喂 token 上限（K-1 案 A token 化迁移;缺省 12000 ≈ 48K
    /// ASCII 字符/4,与旧字符口径等价）。None = 未显式声明,按旧键
    /// `tool_result_max_chars` /4 折算(兼容既有配方);显式声明优先于旧键。
    /// 生效值 clamp 1000..=100000,见 `AssemblyExecutor::tool_result_max_tokens`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_max_tokens: Option<usize>,
    /// 陈旧工具结果发送视图降级声明（可选段;五轮裁定批 G2/B-303:缺省/缺键
    /// = Some(默认参数) = 开启——"被裁内容必须可查可重建"纪律缺省成立;
    /// 显式 `null` = 关闭（逐字节回退旧行为）。缺省翻转属行为变更,
    /// 项目方 2026-11-12 裁定留痕于审计 §五）
    #[serde(default = "default_stale_demotion_enabled")]
    pub stale_result_demotion: Option<StaleDemotionSpec>,
    /// 机制消息开关声明（可选段;γ 批 B-268:动态区[记忆分区/Handoff Base/todo]
    /// 外移为独立机制消息,静态区跨轮字节级稳定 = 被动前缀缓存命中前提）。
    /// 五轮裁定批 G2/B-303:缺省/缺键 = Some(默认参数) = 开启（前缀缓存收益
    /// 缺省可得）;显式 `null` = 关闭 = 动态区仍嵌 system prompt 内。
    #[serde(default = "default_mechanism_messages_enabled")]
    pub mechanism_messages: Option<MechanismMessagesSpec>,
}

/// 机制消息开关声明（γ 批 B-268:动态区[记忆分区/Handoff Base/todo]外移为
/// 独立机制消息,静态区跨轮字节级稳定 = 被动前缀缓存命中前提）。降级参数
/// 与 β 陈旧降级同参(复用 4/2000,不另立调参面)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MechanismMessagesSpec {
    /// 机制消息发送视图降级:距当前最小 assistant 轮数（默认 4 = β 同参;
    /// γ-3 消费,γ-1 仅声明面）
    #[serde(
        default = "default_stale_min_turns",
        skip_serializing_if = "stale_min_turns_is_default"
    )]
    pub stale_min_turns: usize,
    /// 机制消息发送视图降级:字节阈值（默认 2000 = β 同参;γ-3 消费,γ-1 仅声明面）
    #[serde(
        default = "default_stale_min_bytes",
        skip_serializing_if = "stale_min_bytes_is_default"
    )]
    pub stale_min_bytes: usize,
}

impl Default for MechanismMessagesSpec {
    fn default() -> Self {
        Self {
            stale_min_turns: default_stale_min_turns(),
            stale_min_bytes: default_stale_min_bytes(),
        }
    }
}

fn stale_min_turns_is_default(v: &usize) -> bool {
    *v == default_stale_min_turns()
}
fn stale_min_bytes_is_default(v: &usize) -> bool {
    *v == default_stale_min_bytes()
}

/// 配方版本次版本号 +1("recipe-v1.3" → "recipe-v1.4";解析失败原样返回)
fn bump_recipe_version(v: &str) -> String {
    let Some((prefix, minor)) = v.rsplit_once('.') else {
        return v.to_string();
    };
    match minor.parse::<u32>() {
        Ok(n) => format!("{prefix}.{}", n + 1),
        Err(_) => v.to_string(),
    }
}

/// 陈旧工具结果发送视图降级声明（确定性纯函数,三条件全部满足才降级:
/// ①结果已被消费——其后存在 Assistant 消息;②距当前 ≥ min_turns 个
/// assistant 轮;③结果字节数 > min_bytes）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleDemotionSpec {
    /// 距当前最小 assistant 轮数（默认 4;声明 < 1 校验期拒绝）
    #[serde(default = "default_stale_min_turns")]
    pub min_turns: usize,
    /// 结果字节阈值（默认 2000,锚定 runner 引用块头摘要量
    /// TOOL_RESULT_REF_HEAD_CHARS——结果比引用块自身头尾还小则降级无收益;
    /// 声明 0 校验期拒绝）
    #[serde(default = "default_stale_min_bytes")]
    pub min_bytes: usize,
}

impl Default for StaleDemotionSpec {
    fn default() -> Self {
        Self {
            min_turns: default_stale_min_turns(),
            min_bytes: default_stale_min_bytes(),
        }
    }
}

/// 五轮裁定批 G2/B-303:两层安全网缺省开启(显式 null 仍可关)
fn default_stale_demotion_enabled() -> Option<StaleDemotionSpec> {
    Some(StaleDemotionSpec::default())
}

fn default_mechanism_messages_enabled() -> Option<MechanismMessagesSpec> {
    Some(MechanismMessagesSpec::default())
}

fn default_stale_min_turns() -> usize {
    4
}
fn default_stale_min_bytes() -> usize {
    2_000
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
        Self {
            notices_first: true,
        }
    }
}

impl Default for BudgetSpec {
    fn default() -> Self {
        Self {
            window_tokens_source: default_window_tokens_source(),
            reserve_for_response_pct: default_reserve_pct(),
            tool_result_max_chars: default_tool_result_max_chars(),
            tool_result_max_tokens: None,
            stale_result_demotion: default_stale_demotion_enabled(),
            mechanism_messages: default_mechanism_messages_enabled(),
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
            id: "S2_governance".to_string(),
            source: "governance_segment".to_string(),
            degradable: false,
            optional: true,
            enabled: true,
            budget: None,
            degradation_order: None,
            sections: None,
            separator: None,
            role: None,
            trim: None,
        },
        SlotSpec {
            id: "S2b_knowledge".to_string(),
            source: "knowledge_segment".to_string(),
            degradable: false,
            optional: true,
            enabled: true,
            budget: None,
            degradation_order: None,
            sections: None,
            separator: None,
            role: None,
            trim: None,
        },
        SlotSpec {
            id: "S2c_agents_md".to_string(),
            source: "agents_md".to_string(),
            // 项目记忆段(AGENTS.md 层级):静态区尾槽(知识契约之后、记忆
            // 召回之前)。内容仅随文件编辑变化,跨轮字节级稳定——被动前缀
            // 缓存的跨轮命中段因此从静态头延长到静态头+项目记忆;动态区
            // (recall/todo)集中尾置。超限裁剪由装载器负责并带标注,槽内
            // 不再裁剪;None/空 = 未启用,槽位静默跳过 = 既有 agent 逐字节
            // 零影响(recipe-v1.2 minor 增补的等价性依据)
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
            id: "S2d_catalog".to_string(),
            source: "service_catalog".to_string(),
            // 服务能力目录段:紧凑按域分组+典型触发场景+成本纪律尾注——
            // LLM 侧能力锚常驻(确定性服务发现主修)。内容=启动期注册面
            // 渲染(service_tools 注册表只含实际注册可调用服务),进程内
            // 跨轮字节级稳定(三级全排序);静态区尾槽(项目记忆之后、记忆
            // 召回之前)——被动前缀缓存命中段因此延长到静态头+项目记忆
            // +服务目录。None/空 = 服务注册为空,槽位静默跳过 = 既有
            // agent 逐字节零影响(minor 增补的等价性依据)
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
            id: "S3_memory".to_string(),
            source: "recall".to_string(),
            degradable: true,
            optional: false,
            enabled: true,
            budget: Some(SlotBudget {
                ratio: DEFAULT_MEMORY_RATIO,
                base: "input".to_string(),
                clamp: [0.1, 0.5],
            }),
            degradation_order: Some(DEGRADATION_LAYERS.iter().map(|s| s.to_string()).collect()),
            sections: Some(SlotSections::default()),
            separator: None,
            role: None,
            trim: None,
        },
        SlotSpec {
            id: "S3c_todo".to_string(),
            source: "todo_summary".to_string(),
            // 小体量结构字段完整性优先,不占记忆预算(Handoff Base 底座块
            // 同口径);无清单/开关关时 runner 不传内容,槽位静默跳过 =
            // 既有 agent 逐字节零影响(recipe-v1.1 minor 增补的等价性依据)
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
            // B2 批启用:skills 未声明(None/空)时槽位静默跳过(optional),
            // 行为与预留期逐字节一致;声明后注入 manifest 段
            enabled: true,
            budget: None,
            degradation_order: None,
            sections: None,
            separator: Some("\n\n".to_string()),
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
        if self.budget.reserve_for_response_pct == 0 || self.budget.reserve_for_response_pct >= 100
        {
            return Err(format!(
                "budget.reserve_for_response_pct {} out of range (1..99)",
                self.budget.reserve_for_response_pct
            ));
        }
        if self.budget.tool_result_max_chars == 0 {
            return Err("budget.tool_result_max_chars must be > 0".to_string());
        }
        if let Some(sd) = &self.budget.stale_result_demotion {
            if sd.min_turns < 1 {
                return Err("budget.stale_result_demotion.min_turns must be >= 1".to_string());
            }
            if sd.min_bytes == 0 {
                return Err("budget.stale_result_demotion.min_bytes must be > 0".to_string());
            }
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

/// 渲染技能 manifest 段(skills 装配 B2 批,设计档 §4.2 形态:标题+
/// 每 skill 一行 `name: description`+按需装载尾注)
///
/// 定位:渐进披露的常驻半区——LLM 知有哪些能力锚,正文经 read_skill
/// 工具按需装载(独立段,不并入 L2 前馈,约束面语义纯净性)。
fn render_skills_manifest(skills: &[crate::agent::definition::SkillManifestEntry]) -> String {
    let mut seg = String::from("【可用技能清单】以下技能可经 read_skill 工具装载正文：");
    for s in skills {
        seg.push_str("\n- ");
        seg.push_str(&s.name);
        seg.push_str(": ");
        seg.push_str(&s.description);
    }
    seg.push_str("\n按需装载，不要一次性全部读取。");
    seg.push_str("\n注：清单自本次会话创建时生效；会话中新建或修改的技能，自下次会话起出现。");
    seg
}

/// 渲染新鲜度提示（env-sensitive 技能的批准环境 ≠ 当前环境——复验提醒,
/// 不降级不拒读;与完整性正交。文本不回显环境指纹——prompt 注入面卫生）
pub fn render_skills_env_notice(stale: &[String]) -> String {
    format!(
        "【技能环境提示】以下技能（已声明 env-sensitive）核验于旧环境，当轮引用其结论前须重新实测：{}",
        stale.join("、")
    )
}

/// 渲染降级停用通知（会话创建装配链的哈希失配名单——降级回喂会话，
/// 终结「改完文件没重批 → 下会话技能凭空消失」的静默失忆）
pub fn render_skills_downgraded_notice(downgraded: &[String]) -> String {
    format!("【技能停用通知】以下技能因批准后内容变动已被停用（正门与 read_skill 均不可用），重新走 approve 审批后恢复：{}", downgraded.join("、"))
}

#[cfg(test)]
mod env_notice_tests {
    use super::*;

    #[test]
    fn test_render_skills_env_notice_lists_names_without_fingerprint() {
        let notice = render_skills_env_notice(&["fact-a".to_string(), "fact-b".to_string()]);
        assert!(notice.contains("fact-a、fact-b"));
        assert!(notice.contains("重新实测"), "must demand re-verification");
        assert!(
            !notice.contains("blake3"),
            "must not echo fingerprint details"
        );
    }
}

/// 渲染交接底座包为 S3 槽内 "## Handoff Base" 结构化块(上下文延续归一件)
///
/// 确定性纯函数:字段序固定(goal→milestone→next_step→verified_facts 逐条→
/// dead_ends 逐条),可选项缺省整行省略,空清单整节省略——同输入逐字节同输出,
/// 适用 wire 可重建验收(完整性锚同款)。dead_ends 标注「勿重试」= 失败教训
/// 强制回喂。
fn render_handoff_base(h: &crate::agent::definition::HandoffPackage) -> String {
    let mut seg = String::from("## Handoff Base\n");
    seg.push_str("- Goal: ");
    seg.push_str(&h.goal);
    seg.push('\n');
    if let Some(m) = &h.milestone_current {
        seg.push_str("- Milestone: ");
        seg.push_str(m);
        seg.push('\n');
    }
    if let Some(n) = &h.next_step {
        seg.push_str("- Next Step: ");
        seg.push_str(n);
        seg.push('\n');
    }
    if !h.verified_facts.is_empty() {
        seg.push_str("- Verified Facts:\n");
        for (i, f) in h.verified_facts.iter().enumerate() {
            seg.push_str(&format!("  {}. {f}\n", i + 1));
        }
    }
    if !h.dead_ends.is_empty() {
        seg.push_str("- Dead Ends (do not retry):\n");
        for (i, d) in h.dead_ends.iter().enumerate() {
            seg.push_str(&format!("  {}. {d}\n", i + 1));
        }
    }
    // 尾部收敛为单换行(与技能段同风格,拼接时统一由调用方补 "\n\n")
    while seg.ends_with('\n') {
        seg.pop();
    }
    seg
}

/// G5/B-311:组装入参对象(15 槽位源命名对齐原位置参数语义;统帅 2026-11-12
/// 裁定 B 案收拢,推翻 L894 原注释"参数对象反损槽位可读性"取舍——37 处
/// 调用点逐点核语义改写,行为零变化)
pub struct AssemblyInputs<'a> {
    /// S1_base 源(definition.system_prompt)
    pub base_prompt: &'a str,
    /// F-101 身份资产段(None = 未声明,S1 槽仅基底块;声明后 S1 槽内拼接序
    /// = 基底块→"\n\n"→身份段)
    pub identity_segment: Option<&'a str>,
    /// C-4 北极星锚(S1 槽内紧随身份段;None = 未声明,跳过)
    pub north_star: Option<&'a str>,
    /// S3_memory 源载体(None = memory 未启用,槽位静默跳过 = 现状 memory
    /// none 分支行为)
    pub memory: Option<&'a crate::agent::memory::MemoryManager>,
    /// S3_memory 内容(已在调用侧完成召回;预算裁剪在渲染器内)
    pub recall: &'a crate::agent::memory::RecallContext,
    /// 记忆区预算基准的原始输入(默认配方 `base: input` 扣除响应预留后作
    /// 基数;显式 `base: total_window` 直接作基数=兼容口径)
    pub total_window: usize,
    /// S4_boundary 边界感知段(None = 未声明,槽位跳过;分隔前缀由配方
    /// separator 声明)
    pub boundary_segment: Option<&'a str>,
    /// S4b_skills 技能清单数据(None/空 = 清单段不注入,槽位静默跳过)
    pub skills: Option<&'a [crate::agent::definition::SkillManifestEntry]>,
    /// S4b_skills 清单尾注提示段(None = 未声明,不注入;与清单段以 "\n\n"
    /// 相接)
    pub skills_notice: Option<&'a str>,
    /// S3 槽内尾随交接底座包(None = 未声明,跳过)
    pub handoff: Option<&'a crate::agent::definition::HandoffPackage>,
    /// S2_governance 源(None = 未声明,槽位跳过)
    pub governance_segment: Option<&'a str>,
    /// S2b_knowledge 源(None/空 = 未声明治理知识数据集,槽位跳过 =
    /// 既有定义零影响)
    pub knowledge_segment: Option<&'a str>,
    /// S2c_agents_md 项目记忆段(AGENTS.md 层级;None/空 = 未启用,槽位
    /// 静默跳过)
    pub agents_md: Option<&'a str>,
    /// S3c_todo 源(None = 无清单/开关关,槽位静默跳过;runner 侧确定性
    /// 渲染,小体量不占记忆预算)
    pub todo_summary: Option<&'a str>,
    /// S2d_catalog 源(None/空 = 服务注册为空,槽位静默跳过;service_tools
    /// 启动期注册面渲染,进程内跨轮字节级稳定)
    pub service_catalog: Option<&'a str>,
}

/// 组装执行器:配方 → system_prompt 与预算参数的**单一确定性纯函数组**
///
/// 双路径一致性由代码结构保证(run/流式两组装点收敛为同一次 `assemble` 调用,
/// 不再是「纪律要求同步」而是「物理上同一段代码」)。槽内渲染(S3 记忆区分区
/// 标题/行格式/notices 位置与 L2 安全审计)留在 `MemoryManager::build_system_
/// prompt_with_recall` ——配方是槽位级声明,渲染机制不进配方(§三 范围裁定)。
#[derive(Debug, Clone)]
pub struct AssemblyExecutor {
    recipe: AssemblyRecipe,
}

impl AssemblyExecutor {
    /// 由配方构造执行器
    pub fn new(recipe: AssemblyRecipe) -> Self {
        Self { recipe }
    }

    /// 内置默认配方执行器(等价迁移:现状硬编码行为)
    pub fn default_executor() -> Self {
        Self::new(AssemblyRecipe::default())
    }

    /// 底层配方(版本落账读取 recipe_version/内容哈希用)
    pub fn recipe(&self) -> &AssemblyRecipe {
        &self.recipe
    }

    /// 记忆区预算比例(配方 S3 槽位声明;None = 配方无 S3 槽位)
    pub fn memory_budget_ratio(&self) -> Option<f32> {
        self.slot("S3_memory")
            .and_then(|s| s.budget.as_ref())
            .map(|b| b.ratio)
    }

    /// 响应预留百分比(默认配方 25 = 现状 max_tokens/4)
    pub fn reserve_for_response_pct(&self) -> u32 {
        self.recipe.budget.reserve_for_response_pct
    }

    /// 工具输出回喂字符上限(默认配方 48000 = 现状 TOOL_RESULT_MAX_CHARS;
    /// K-1 案 A 后为旧键,仅作兼容读取与折算基准,见
    /// [`Self::tool_result_max_tokens`])
    pub fn tool_result_max_chars(&self) -> usize {
        self.recipe.budget.tool_result_max_chars
    }

    /// 工具输出回喂 token 上限生效值(K-1 案 A token 化迁移)
    ///
    /// 折算与钳制口径:新键显式声明优先;None 时按旧键 ASCII 基准 /4 折算
    /// (48K chars → 12K tokens,语义近似保留);生效值 clamp 1000..=100000
    /// (构造期钳制,防声明过小把回喂全截没/过大失去预算意义)。
    pub fn tool_result_max_tokens(&self) -> usize {
        let raw = self
            .recipe
            .budget
            .tool_result_max_tokens
            .unwrap_or(self.recipe.budget.tool_result_max_chars / 4);
        raw.clamp(1_000, 100_000)
    }

    /// 陈旧工具结果发送视图降级声明(None = 默认关闭——发送视图零变化)
    pub fn stale_result_demotion(&self) -> Option<&StaleDemotionSpec> {
        self.recipe.budget.stale_result_demotion.as_ref()
    }

    /// 机制消息开关声明(None = 默认关闭——动态区仍嵌 system prompt 内,
    /// 组装与现行为逐字节一致)
    pub fn mechanism_messages(&self) -> Option<&MechanismMessagesSpec> {
        self.recipe.budget.mechanism_messages.as_ref()
    }

    /// 有效配方版本(γ-4 升版纪律):机制消息开启态改变 system prompt 形态——
    /// 升版动作绑定开启态首次生效(声明版本次版本号 +1),默认关闭态原样
    /// 返回声明版本(默认配方 = recipe-v1.3 不升版)。F-302 落账版本戳消费。
    pub fn effective_recipe_version(&self) -> String {
        if self.recipe.budget.mechanism_messages.is_some() {
            bump_recipe_version(&self.recipe.recipe_version)
        } else {
            self.recipe.recipe_version.clone()
        }
    }

    /// S7 裁剪参数(策略名/buffer%/hint token,供 ContextWindowManager 构造)
    pub fn trim_params(&self) -> Option<(&str, u32, usize)> {
        self.slot("S7_history")
            .and_then(|s| s.trim.as_ref())
            .map(|t| (t.strategy.as_str(), t.buffer_pct, t.hint_budget_tokens))
    }

    fn slot(&self, id: &str) -> Option<&SlotSpec> {
        self.recipe.slots.iter().find(|s| s.id == id)
    }

    /// 槽位预算基数解析:`input` = 默认口径(2026-10-03 A-1 完全切换:总窗扣除
    /// 响应预留后的输入侧空间作基数,整数算术与 runner reserve 同族);
    /// `total_window` = 兼容口径(显式声明仍合法,总窗直接作基数——2026-10-03
    /// 前的历史行为,已部署配置显式声明者零影响)。未知值 Err(validate 白名单
    /// 应已拦截,此处运行时兜底 fail-fast)。
    fn budget_base_tokens(&self, base: &str, total_window: usize) -> Result<usize, String> {
        match base {
            "total_window" => Ok(total_window),
            "input" => {
                let pct = self.recipe.budget.reserve_for_response_pct;
                Ok(total_window - total_window * pct as usize / 100)
            }
            other => Err(format!(
                "slot budget.base '{other}' unknown (allowed: {BUDGET_BASES:?})"
            )),
        }
    }

    /// 组装 system_prompt(按配方槽位序拼接)
    ///
    /// 槽位源逐参数说明随 [`AssemblyInputs`] 字段注释收拢迁移(G5/B-311)。
    /// Err 仅当配方声明了执行器不认识的预算基准(budget.base——validate 白名单
    /// 应已拦截,此处运行时兜底 fail-fast),不做静默降级。
    // G5/B-311:原注释"收拢为参数对象反损槽位可读性"经统帅 2026-11-12 裁定 B 案推翻——37 处调用点逐点核语义改写,行为零变化
    pub fn assemble(&self, inp: AssemblyInputs<'_>) -> Result<String, String> {
        let AssemblyInputs {
            base_prompt,
            identity_segment,
            north_star,
            memory,
            recall,
            total_window,
            boundary_segment,
            skills,
            skills_notice,
            handoff,
            governance_segment,
            knowledge_segment,
            agents_md,
            todo_summary,
            service_catalog,
        } = inp;
        self.assemble_inner(
            AssemblyInputs {
                base_prompt,
                identity_segment,
                north_star,
                memory,
                recall,
                total_window,
                boundary_segment,
                skills,
                skills_notice,
                handoff,
                governance_segment,
                knowledge_segment,
                agents_md,
                todo_summary,
                service_catalog,
            },
            None,
        )
    }

    /// 组装分叉版(γ 批 B-268):配方开启 `mechanism_messages` 时,动态区
    /// (S3_memory 记忆分区/Handoff Base、S3c_todo)改渲染进独立机制段返回,
    /// 静态区不含任何机制分区标记(跨轮字节级稳定 = 被动前缀缓存命中前提);
    /// 关闭态/无动态内容物时与 [`Self::assemble`] 逐字节一致且机制段 None。
    /// 机制段内序 = 槽位序(记忆分区 → Handoff Base → todo)。
    pub fn assemble_split(
        &self,
        inp: AssemblyInputs<'_>,
    ) -> Result<(String, Option<String>), String> {
        let AssemblyInputs {
            base_prompt,
            identity_segment,
            north_star,
            memory,
            recall,
            total_window,
            boundary_segment,
            skills,
            skills_notice,
            handoff,
            governance_segment,
            knowledge_segment,
            agents_md,
            todo_summary,
            service_catalog,
        } = inp;
        let mut mechanism = String::new();
        let prompt = self.assemble_inner(
            AssemblyInputs {
                base_prompt,
                identity_segment,
                north_star,
                memory,
                recall,
                total_window,
                boundary_segment,
                skills,
                skills_notice,
                handoff,
                governance_segment,
                knowledge_segment,
                agents_md,
                todo_summary,
                service_catalog,
            },
            Some(&mut mechanism),
        )?;
        let mechanism = if mechanism.is_empty() {
            None
        } else {
            Some(mechanism.trim().to_string())
        };
        Ok((prompt, mechanism))
    }

    /// 组装内核:`mechanism_out = Some` 且配方开启 `mechanism_messages` 时,
    /// 动态区槽位改渲染进机制段;`None` 或关闭态 = 全量嵌入(现行为)。
    fn assemble_inner(
        &self,
        inp: AssemblyInputs<'_>,
        mut mechanism_out: Option<&mut String>,
    ) -> Result<String, String> {
        let AssemblyInputs {
            base_prompt,
            identity_segment,
            north_star,
            memory,
            recall,
            total_window,
            boundary_segment,
            skills,
            skills_notice,
            handoff,
            governance_segment,
            knowledge_segment,
            agents_md,
            todo_summary,
            service_catalog,
        } = inp;
        let mut prompt = String::new();
        // γ 批 B-268:机制段外移模式 = 分叉调用在场 且 配方声明开启。
        // 两者缺一(默认配方/直调 assemble) = 全量嵌入,逐字节现行为。
        let mechanism_mode =
            mechanism_out.is_some() && self.recipe.budget.mechanism_messages.is_some();
        for slot in &self.recipe.slots {
            if !slot.enabled {
                continue; // 开关停用(如 S4b_skills 预留位)
            }
            match slot.source.as_str() {
                // S1_base:系统提示(硬注入,不裁剪)。F-101:身份资产段紧随
                // 基底块,C-4:北极星锚紧随身份段(槽内拼接序固定,后续槽位
                // 仍按配方序)
                "definition.system_prompt" => {
                    prompt.push_str(base_prompt);
                    if let Some(seg) = identity_segment {
                        prompt.push_str("\n\n");
                        prompt.push_str(seg);
                    }
                    if let Some(ns) = north_star {
                        prompt.push_str("\n\n");
                        prompt.push_str(ns);
                    }
                }
                // S2_governance:治理门禁段(serve 三段合并:L2 约束前馈/
                // 进化信号感知/规范入口索引;authority=L2 约束层,独立分区
                // 不可降级)。v2 序=权威最高者紧跟 S1 之后(装配序设计 §5.1)
                "governance_segment" => {
                    if let Some(seg) = governance_segment {
                        if !seg.trim().is_empty() {
                            prompt.push_str("\n\n");
                            prompt.push_str(seg);
                        }
                    }
                }
                // S2b_knowledge:治理知识契约段(runner 组装期按定义声明实时
                // 拉取渲染;authority=治理知识条目,独立分区不可降级。None/
                // 空 = 未声明数据集,槽位跳过 = 既有定义零影响)
                "knowledge_segment" => {
                    if let Some(seg) = knowledge_segment {
                        if !seg.trim().is_empty() {
                            prompt.push_str("\n\n");
                            prompt.push_str(seg);
                        }
                    }
                }
                // S2c_agents_md:项目记忆段(AGENTS.md 层级)——静态区尾槽
                // (知识契约之后、记忆召回之前)。内容仅随文件编辑变化,
                // 跨轮字节级稳定;与动态区(recall/todo)分区隔离是被动前缀
                // 缓存跨轮命中的前提。None/空 = 未启用,槽位静默跳过
                "agents_md" => {
                    if let Some(seg) = agents_md {
                        if !seg.trim().is_empty() {
                            let sep = slot.separator.as_deref().unwrap_or("\n\n");
                            prompt.push_str(sep);
                            prompt.push_str(seg);
                        }
                    }
                }
                // S2d_catalog:服务能力目录段(紧凑按域分组+典型触发场景+
                // 成本纪律尾注;service_tools 启动期注册面渲染,内容只含
                // 实际注册可调用服务)。None/空 = 注册为空,槽位静默跳过 =
                // 既有 agent 逐字节零影响
                "service_catalog" => {
                    if let Some(seg) = service_catalog {
                        if !seg.trim().is_empty() {
                            let sep = slot.separator.as_deref().unwrap_or("\n\n");
                            prompt.push_str(sep);
                            prompt.push_str(seg);
                        }
                    }
                }
                // S3_memory:记忆区(渲染机制在 MemoryManager,含 fit_recall
                // 预算裁剪 + L2 安全审计 + 分区渲染;比例/基准由配方声明。
                // 渲染器以「当前累积 prompt」为 base 前缀,返回 base+记忆区)。
                // 槽内尾随:交接底座包 "## Handoff Base" 结构化块(确定性包=
                // 底座层,与滚动摘要语义面分层配对;纯底座可独立续跑——
                // memory None 而 handoff 在场时照常渲染)。底座块不占记忆
                // 预算(结构字段完整性优先,体量小;与降级通知同口径)。
                "recall" => {
                    if mechanism_mode {
                        // γ 批 B-268:动态区外移——记忆分区/Handoff Base 渲染进
                        // 机制段(渲染器以 base="" 调用得纯记忆区,首段 "\n\n"
                        // 前缀去除;静态区不改写 = 跨轮字节级稳定)。底座块
                        // 不占记忆预算的口径与嵌入态一致。
                        let mech = mechanism_out.as_mut().expect("mechanism_mode implies Some");
                        if let Some(mem) = memory {
                            let budget = match slot.budget.as_ref() {
                                Some(b) => {
                                    let base_tokens =
                                        self.budget_base_tokens(&b.base, total_window)?;
                                    crate::agent::memory::ContextBudget::new(base_tokens, b.ratio)
                                }
                                // 槽位未声明 budget:默认口径(input 基×DEFAULT_MEMORY_RATIO,
                                // 与默认配方 S3 声明一致)
                                None => crate::agent::memory::ContextBudget::new(
                                    self.budget_base_tokens("input", total_window)?,
                                    DEFAULT_MEMORY_RATIO,
                                ),
                            };
                            let region = mem.build_system_prompt_with_recall("", recall, &budget);
                            mech.push_str(region.trim_start());
                        }
                        if let Some(h) = handoff {
                            if !mech.is_empty() {
                                mech.push_str("\n\n");
                            }
                            mech.push_str(&render_handoff_base(h));
                        } else if !recall.summaries.is_empty() {
                            // 配对完整性断言(松弛实现):前会话语义面摘要注入中
                            // 而底座包缺位=上下文延续单腿行走。warn 降级可见
                            // (缺任一层不阻塞);反向(纯底座无摘要)合法不告警。
                            tracing::warn!(
                                summaries = recall.summaries.len(),
                                "handoff pairing: session summaries present but no handoff base package declared; continuing degraded"
                            );
                        }
                    } else {
                        if let Some(mem) = memory {
                            let budget = match slot.budget.as_ref() {
                                Some(b) => {
                                    let base_tokens =
                                        self.budget_base_tokens(&b.base, total_window)?;
                                    crate::agent::memory::ContextBudget::new(base_tokens, b.ratio)
                                }
                                // 槽位未声明 budget:默认口径(input 基×DEFAULT_MEMORY_RATIO,
                                // 与默认配方 S3 声明一致)
                                None => crate::agent::memory::ContextBudget::new(
                                    self.budget_base_tokens("input", total_window)?,
                                    DEFAULT_MEMORY_RATIO,
                                ),
                            };
                            prompt = mem.build_system_prompt_with_recall(&prompt, recall, &budget);
                        }
                        if let Some(h) = handoff {
                            prompt.push_str("\n\n");
                            prompt.push_str(&render_handoff_base(h));
                        } else if !recall.summaries.is_empty() {
                            // 配对完整性断言(松弛实现):前会话语义面摘要注入中
                            // 而底座包缺位=上下文延续单腿行走。warn 降级可见
                            // (缺任一层不阻塞);反向(纯底座无摘要)合法不告警。
                            tracing::warn!(
                                summaries = recall.summaries.len(),
                                "handoff pairing: session summaries present but no handoff base package declared; continuing degraded"
                            );
                        }
                    }
                }
                // S3c_todo:todo 清单摘要(批K 段4)——LLM 自管理任务状态的
                // 常驻注入(每轮随装配重建,状态锚定不靠消息历史记忆)。内容
                // 由 runner 侧确定性渲染(render_todo_summary),None/空 =
                // 无清单/开关关,槽位静默跳过;不占记忆预算(结构字段完整性
                // 优先,Handoff Base 底座块同口径)
                "todo_summary" => {
                    if mechanism_mode {
                        // γ 批 B-268:动态区外移——todo 段渲染进机制段(机制段
                        // 为空时不带分隔前缀,段内容与嵌入态逐字节一致)
                        if let Some(seg) = todo_summary {
                            if !seg.trim().is_empty() {
                                let mech =
                                    mechanism_out.as_mut().expect("mechanism_mode implies Some");
                                let sep = slot.separator.as_deref().unwrap_or("\n\n");
                                if !mech.is_empty() {
                                    mech.push_str(sep);
                                }
                                mech.push_str(seg);
                            }
                        }
                    } else if let Some(seg) = todo_summary {
                        if !seg.trim().is_empty() {
                            let sep = slot.separator.as_deref().unwrap_or("\n\n");
                            prompt.push_str(sep);
                            prompt.push_str(seg);
                        }
                    }
                }
                // S4_boundary:边界段(现状 "\n\n" 前缀由配方 separator 声明)
                "definition.capability_boundary.awareness_segment" => {
                    if let Some(seg) = boundary_segment {
                        let sep = slot.separator.as_deref().unwrap_or("\n\n");
                        prompt.push_str(sep);
                        prompt.push_str(seg);
                    }
                }
                // S4b_skills:技能 manifest 段(B2)——声明面人工把关后,LLM 侧
                // 能力锚常驻+read_skill 按需装载正文的渐进披露读法
                "manifest" => {
                    let mut seg = String::new();
                    if let Some(skills) = skills {
                        if !skills.is_empty() {
                            seg.push_str(&render_skills_manifest(skills));
                        }
                    }
                    if let Some(notice) = skills_notice {
                        if !seg.is_empty() {
                            seg.push_str("\n\n");
                        }
                        seg.push_str(notice);
                    }
                    if !seg.is_empty() {
                        let sep = slot.separator.as_deref().unwrap_or("\n\n");
                        prompt.push_str(sep);
                        prompt.push_str(&seg);
                    }
                }
                // S5_task/S6_summary/S7_history:messages 面槽位,不在
                // system_prompt 组装内(裁剪参数经 trim_params 消费)
                _ => {}
            }
        }
        Ok(prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::memory::RecallContext;

    /// 等价迁移铁律的机械执行:Default 逐字段 = 现状硬编码值对照表
    /// （锚点:memory.rs ContextBudget::new/fit_recall/build_system_prompt_with_recall、
    /// context_window.rs trim_keep_system_keep_last、runner.rs TOOL_RESULT_MAX_CHARS
    /// 与响应预留 1/4）
    #[test]
    fn test_default_recipe_matches_current_hardcoded_values() {
        let r = AssemblyRecipe::default();
        assert_eq!(r.recipe_version, "recipe-v1.3");
        let ids: Vec<&str> = r.slots.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "S1_base",
                "S2_governance",
                "S2b_knowledge",
                "S2c_agents_md",
                "S2d_catalog",
                "S3_memory",
                "S3c_todo",
                "S4_boundary",
                "S4b_skills",
                "S5_task",
                "S6_summary",
                "S7_history"
            ]
        );
        // S2 治理门禁:S2 前移(v2 目标序;不可降级,缺席合法)
        let s2 = &r.slots[1];
        assert_eq!(s2.id, "S2_governance");
        assert!(!s2.degradable);
        assert!(s2.optional);
        // S2b 治理知识契约:同 S2 形态(不可降级,缺席合法)
        let s2b = &r.slots[2];
        assert_eq!(s2b.id, "S2b_knowledge");
        assert!(!s2b.degradable);
        assert!(s2b.optional);
        // S2c 项目记忆段(AGENTS.md 层级):静态区尾槽(知识契约之后、记忆
        // 召回之前;optional+不可降级+"\n\n",超限裁剪归装载器)
        let s2c = &r.slots[3];
        assert_eq!(s2c.id, "S2c_agents_md");
        assert_eq!(s2c.source, "agents_md");
        assert!(s2c.optional);
        assert!(!s2c.degradable);
        assert!(s2c.enabled);
        assert!(s2c.budget.is_none());
        assert_eq!(s2c.separator.as_deref(), Some("\n\n"));
        // S2d 服务目录段:静态区尾槽(项目记忆之后、记忆召回之前;默认启用,
        // 目录空=槽位静默跳过;旧自定义配方无此槽=兼容)
        let s2d = &r.slots[4];
        assert_eq!(s2d.id, "S2d_catalog");
        assert_eq!(s2d.source, "service_catalog");
        assert!(s2d.optional);
        assert!(!s2d.degradable);
        assert!(s2d.enabled);
        assert!(s2d.budget.is_none());
        assert_eq!(s2d.separator.as_deref(), Some("\n\n"));
        // S3 记忆区:C3 默认 0.25 + clamp(0.1,0.5) + 降级序 stable>summaries>events + notices 置前
        let s3 = &r.slots[5];
        assert_eq!(s3.source, "recall");
        assert!(s3.degradable);
        let b = s3.budget.as_ref().unwrap();
        assert!((b.ratio - 0.25).abs() < 1e-6);
        assert_eq!(b.base, "input"); // A-1 完全切换口径(2026-10-03 默认基数=输入侧;显式 total_window 声明仍合法=向后兼容)
        assert_eq!(b.clamp, [0.1, 0.5]);
        assert_eq!(
            s3.degradation_order.as_ref().unwrap(),
            &["stable", "summaries", "events"]
        );
        assert!(s3.sections.unwrap().notices_first);
        // S3c todo 清单摘要(批K 段4):optional + 不可降级 + "\n\n"(清单空/开关关
        // =槽位静默跳过,既有 agent 逐字节零影响)
        let s3c = &r.slots[6];
        assert_eq!(s3c.id, "S3c_todo");
        assert_eq!(s3c.source, "todo_summary");
        assert!(s3c.optional);
        assert!(!s3c.degradable);
        assert!(s3c.enabled);
        assert!(s3c.budget.is_none());
        assert_eq!(s3c.separator.as_deref(), Some("\n\n"));
        // S4 边界段:optional + "\n\n"
        assert!(r.slots[7].optional);
        assert_eq!(r.slots[7].separator.as_deref(), Some("\n\n"));
        // S4b:B2 启用(optional + skills 未声明时跳过 = 预留期行为逐字节一致)
        assert!(r.slots[8].enabled);
        assert!(r.slots[7].optional);
        assert_eq!(r.slots[8].separator.as_deref(), Some("\n\n"));
        // S5 任务:user 角色
        assert_eq!(r.slots[9].role.as_deref(), Some("user"));
        // S7 裁剪:KeepSystemKeepLast + buffer 5% + hint 15
        let t = r.slots[11].trim.as_ref().unwrap();
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

    // ===== K-1 案 A:tool_result_max_tokens 配置面(测试矩阵 ①②③⑥)=====

    /// ①缺省 12000(= 旧键 48000 ASCII 基准 /4,等价迁移)
    #[test]
    fn test_tool_result_max_tokens_default_12000() {
        let exec = AssemblyExecutor::default_executor();
        assert_eq!(exec.tool_result_max_tokens(), 12_000);
        // 缺省序列化面不携带新键(skip_serializing_if None)
        let s = serde_json::to_string(&AssemblyRecipe::default()).unwrap();
        assert!(!s.contains("tool_result_max_tokens"));
    }

    /// ②旧键折算:仅声明旧键 tool_result_max_chars 时按 ASCII 基准 /4 折算
    #[test]
    fn test_tool_result_max_tokens_old_key_divides_by_four() {
        let r: AssemblyRecipe =
            serde_json::from_str(r#"{"budget":{"tool_result_max_chars":20000}}"#).unwrap();
        let exec = AssemblyExecutor::new(r);
        assert_eq!(exec.tool_result_max_tokens(), 5_000);
    }

    /// ③新键优先:两键并存时显式声明的 token 值生效,旧键不参与折算
    #[test]
    fn test_tool_result_max_tokens_new_key_wins() {
        let r: AssemblyRecipe = serde_json::from_str(
            r#"{"budget":{"tool_result_max_chars":48000,"tool_result_max_tokens":5000}}"#,
        )
        .unwrap();
        let exec = AssemblyExecutor::new(r);
        assert_eq!(exec.tool_result_max_tokens(), 5_000);
    }

    /// ⑥clamp 边界:生效值钳制 1000..=100000(999→1000,100001→100000)
    #[test]
    fn test_tool_result_max_tokens_clamped() {
        let low: AssemblyRecipe =
            serde_json::from_str(r#"{"budget":{"tool_result_max_tokens":999}}"#).unwrap();
        assert_eq!(AssemblyExecutor::new(low).tool_result_max_tokens(), 1_000);
        let high: AssemblyRecipe =
            serde_json::from_str(r#"{"budget":{"tool_result_max_tokens":100001}}"#).unwrap();
        assert_eq!(
            AssemblyExecutor::new(high).tool_result_max_tokens(),
            100_000
        );
    }

    /// 五轮裁定批 G2/B-303:两层安全网缺省开启(缺省/缺键 = Some);
    /// 序列化面恒显式(Some 输出参数对象,None 输出 null——显式关经
    /// roundtrip 不语义泄漏回开启)
    #[test]
    fn test_budget_safety_nets_on_by_default() {
        let r = AssemblyRecipe::default();
        assert!(r.budget.stale_result_demotion.is_some());
        assert!(r.budget.mechanism_messages.is_some());
        let empty: AssemblyRecipe = serde_json::from_str("{}").unwrap();
        assert!(empty.budget.stale_result_demotion.is_some());
        assert!(empty.budget.mechanism_messages.is_some());
        let s = serde_json::to_string(&AssemblyRecipe::default()).unwrap();
        assert!(
            s.contains("stale_result_demotion") && s.contains("mechanism_messages"),
            "缺省开启段应显式出现在序列化面"
        );
    }

    /// 显式 null = 关闭(旧行为逐字节回退);roundtrip 不泄漏回开启
    #[test]
    fn test_budget_explicit_null_keeps_safety_nets_off() {
        let off: AssemblyRecipe = serde_json::from_str(
            r#"{"budget":{"stale_result_demotion":null,"mechanism_messages":null}}"#,
        )
        .unwrap();
        assert!(off.budget.stale_result_demotion.is_none());
        assert!(off.budget.mechanism_messages.is_none());
        let s = serde_json::to_string(&off).unwrap();
        assert!(s.contains("stale_result_demotion\":null"));
        let round: AssemblyRecipe = serde_json::from_str(&s).unwrap();
        assert!(round.budget.stale_result_demotion.is_none());
        assert!(round.budget.mechanism_messages.is_none());
    }

    /// StaleDemotionSpec serde 缺省与 Default 同值(min_turns=4/min_bytes=2000)
    #[test]
    fn test_stale_demotion_spec_defaults() {
        let d = StaleDemotionSpec::default();
        assert_eq!(d.min_turns, 4);
        assert_eq!(d.min_bytes, 2000);
        let sd: StaleDemotionSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(sd, StaleDemotionSpec::default());
    }

    /// 显式声明段:反序列化落位 + validate 通过 + 执行器访问器透传
    #[test]
    fn test_budget_stale_demotion_deserializes_and_propagates() {
        let json = r#"{"budget": {"stale_result_demotion": {"min_turns": 2, "min_bytes": 100}}}"#;
        let r: AssemblyRecipe = serde_json::from_str(json).unwrap();
        r.validate().expect("显式段合法值必须过校验");
        let exec = AssemblyExecutor::new(r);
        let sd = exec.stale_result_demotion().expect("访问器透传");
        assert_eq!(sd.min_turns, 2);
        assert_eq!(sd.min_bytes, 100);
    }

    /// fail-fast:min_turns < 1 / min_bytes = 0 拒绝(加载期拦截,不进运行期)
    #[test]
    fn test_validate_rejects_stale_demotion_invalid_params() {
        let mut r = AssemblyRecipe::default();
        r.budget.stale_result_demotion = Some(StaleDemotionSpec {
            min_turns: 0,
            min_bytes: 100,
        });
        let err = r.validate().unwrap_err();
        assert!(
            err.contains("stale_result_demotion") && err.contains("min_turns"),
            "got: {err}"
        );

        let mut r = AssemblyRecipe::default();
        r.budget.stale_result_demotion = Some(StaleDemotionSpec {
            min_turns: 2,
            min_bytes: 0,
        });
        let err = r.validate().unwrap_err();
        assert!(
            err.contains("stale_result_demotion") && err.contains("min_bytes"),
            "got: {err}"
        );
    }

    /// S2c_agents_md 静态区槽位:段落序=知识契约之后、动态尾区(todo)之前
    /// (跨轮前缀稳定的结构前提;被动前缀缓存命中段=静态头+项目记忆。
    /// recall 侧序由配方槽序黄金断言锁定 S2c<S3,此处行为面验 todo 侧)
    #[test]
    fn test_agents_md_slot_sits_between_knowledge_and_volatile_tail() {
        let executor = AssemblyExecutor::new(AssemblyRecipe::default());
        let out = executor
            .assemble(AssemblyInputs {
                base_prompt: "BASE",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &RecallContext::default(),
                total_window: 0,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: Some("GOV"),
                knowledge_segment: Some("KNOW"),
                agents_md: Some("AGENTSMD"),
                todo_summary: Some("TODO"),
                service_catalog: None,
            })
            .unwrap();
        let know = out.find("KNOW").expect("knowledge segment present");
        let agents = out.find("AGENTSMD").expect("agents md segment present");
        let todo = out.find("TODO").expect("todo summary present");
        assert!(know < agents, "项目记忆必须在知识契约之后(静态区内)");
        assert!(agents < todo, "项目记忆必须在动态尾区(recall/todo)之前");
    }

    /// 默认配方通过自身校验(自洽)
    #[test]
    fn test_default_recipe_passes_validate() {
        AssemblyRecipe::default()
            .validate()
            .expect("default recipe must be valid");
    }

    /// S2b_knowledge:治理知识契约槽位在位(默认配方;槽位序 S2 之后 S3 之前)
    #[test]
    fn test_default_recipe_contains_knowledge_slot() {
        let recipe = AssemblyRecipe::default();
        recipe.validate().expect("default recipe must be valid");
        let ids: Vec<&str> = recipe.slots.iter().map(|s| s.id.as_str()).collect();
        let s2 = ids.iter().position(|&i| i == "S2_governance").unwrap();
        let s2b = ids.iter().position(|&i| i == "S2b_knowledge").unwrap();
        let s3 = ids.iter().position(|&i| i == "S3_memory").unwrap();
        assert!(
            s2 < s2b && s2b < s3,
            "S2b_knowledge must sit between S2 and S3, got: {ids:?}"
        );
    }

    /// S2b_knowledge 渲染:Some → 紧随治理段之后;None → 槽位跳过(既有行为零变化)
    #[test]
    fn test_assemble_knowledge_segment_slot_order() {
        let exec = AssemblyExecutor::default_executor();
        let out = exec
            .assemble(AssemblyInputs {
                base_prompt: "BASE",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &RecallContext::default(),
                total_window: 0,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: Some("GOV"),
                knowledge_segment: Some("KNOW"),
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert!(out.starts_with("BASE"));
        let gov = out.find("GOV").expect("governance segment present");
        let know = out.find("KNOW").expect("knowledge segment present");
        assert!(
            gov < know,
            "knowledge segment must follow governance segment"
        );

        // None → 槽位跳过(未声明数据集的既有定义零影响)
        let out2 = exec
            .assemble(AssemblyInputs {
                base_prompt: "BASE",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &RecallContext::default(),
                total_window: 0,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: Some("GOV"),
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert!(out2.contains("GOV"));
        assert!(!out2.contains("KNOW"));
    }

    /// ratio 越声明 clamp 区间:fail-fast
    #[test]
    fn test_validate_rejects_ratio_out_of_clamp() {
        let mut r = AssemblyRecipe::default();
        r.slots
            .iter_mut()
            .find(|s| s.id == "S3_memory")
            .unwrap()
            .budget
            .as_mut()
            .unwrap()
            .ratio = 0.9;
        let err = r.validate().unwrap_err();
        assert!(err.contains("budget.ratio"), "got: {}", err);
    }

    /// clamp 区间本身非法(min >= max)
    #[test]
    fn test_validate_rejects_invalid_clamp() {
        let mut r = AssemblyRecipe::default();
        r.slots
            .iter_mut()
            .find(|s| s.id == "S3_memory")
            .unwrap()
            .budget
            .as_mut()
            .unwrap()
            .clamp = [0.5, 0.1];
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
        r.slots
            .iter_mut()
            .find(|s| s.id == "S3_memory")
            .unwrap()
            .degradation_order
            .as_mut()
            .unwrap()[0] = "chat_history".to_string();
        assert!(r.validate().unwrap_err().contains("unknown layer"));
    }

    /// 未知裁剪策略:拒绝
    #[test]
    fn test_validate_rejects_unknown_trim_strategy() {
        let mut r = AssemblyRecipe::default();
        r.slots
            .iter_mut()
            .find(|s| s.id == "S7_history")
            .unwrap()
            .trim
            .as_mut()
            .unwrap()
            .strategy = "DropEverything".to_string();
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
        assert!(r
            .validate()
            .unwrap_err()
            .contains("reserve_for_response_pct"));
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
        let mut doc = serde_json::json!({ "assembly": { "$ref": recipe_path.to_str().unwrap() } });
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

    /// 黄金样本回归(等价迁移第一验收):executor 输出与重构前现状组装逻辑
    /// 逐字节一致。fixtures 由重构前现状代码捕获生成(2026-10-03,覆盖 CJK
    /// 记忆区/极端小窗降级通知/boundary+memory none 三场景)。
    #[test]
    fn test_golden_samples_byte_identical() {
        use crate::agent::definition::CapabilityBoundary;
        use crate::agent::memory::{MemoryManager, MemoryRecord, RecallContext};
        use std::path::PathBuf;

        let client = crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:18080");
        let mem = MemoryManager::new("golden", client);
        let mk = |k: &str, v: &str| MemoryRecord {
            key: k.to_string(),
            value: v.to_string(),
            timestamp: 1_760_000_000,
            source: None,
            confidence: None,
            tags: Vec::new(),
            // 夹具代表账本来源条目(带锚);[unanchored] 标注走独立单测
            fact_id: Some(1),
            usage_count: 0,
            effective_uses: None,
            lifecycle_state: None,
            cause_fact_id: None,
            evidence: None,
        };
        let base = "你是测试助手,负责回答关于项目的问题。";
        let recall = RecallContext {
            stable: vec![
                mk("fact_lang", "项目主语言为 Rust,前端使用 Svelte 4。"),
                mk("fact_rule", "宪法规则 RL-B1 要求单一真相源,禁止双写。"),
                mk("fact_style", "回复使用中文,代码注释保持确定性口径描述。"),
            ],
            summaries: vec![
                mk("sum_1", "上次会话完成了上下文窗口裁剪策略的回归测试。"),
                mk("sum_2", "此前一轮讨论了记忆区预算比例与弹性归还机制。"),
            ],
            events: vec![
                mk("ev_1", "用户批准了元层先行批设计稿并下达开工指令。"),
                mk("ev_2", "PR-1 组装配方 schema 已推送并通过全部测试。"),
            ],
            ..Default::default()
        };
        let exec = AssemblyExecutor::default_executor();

        // 场景 1:CJK 长记忆(正常预算 8192×0.25,无降级)
        let out1 = exec
            .assemble(AssemblyInputs {
                base_prompt: base,
                identity_segment: None,
                north_star: None,
                memory: Some(&mem),
                recall: &recall,
                total_window: 8192,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert_eq!(
            out1,
            std::fs::read_to_string("tests/fixtures/golden_cjk_memory.txt").unwrap()
        );

        // 场景 2:极端小窗口(60 token)强制降级通知
        let out2 = exec
            .assemble(AssemblyInputs {
                base_prompt: base,
                identity_segment: None,
                north_star: None,
                memory: Some(&mem),
                recall: &recall,
                total_window: 60,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert_eq!(
            out2,
            std::fs::read_to_string("tests/fixtures/golden_degradation.txt").unwrap()
        );

        // 场景 3:memory none 路径 + boundary 段追加(组装点现状语义)
        let boundary = CapabilityBoundary {
            mode: "read_only".to_string(),
            sandbox_root: PathBuf::from("/tmp/sandbox-golden"),
            read_root: None,
            tools: vec!["file_read".to_string()],
        };
        let out3 = exec
            .assemble(AssemblyInputs {
                base_prompt: base,
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &RecallContext::default(),
                total_window: 0,
                boundary_segment: Some(&boundary.awareness_segment()),
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert_eq!(
            out3,
            std::fs::read_to_string("tests/fixtures/golden_boundary.txt").unwrap()
        );
    }

    /// F-101/C-4:S1 槽内拼接序 = 基底块→身份段→北极星锚;None = 基底块
    /// 原样(既有行为零变化)
    #[test]
    fn test_assemble_identity_segment_order() {
        let exec = AssemblyExecutor::default_executor();
        // 无身份段:输出 = 基底块原样
        let out_none = exec
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &Default::default(),
                total_window: 0,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert_eq!(out_none, "base");
        // 有身份段:紧跟基底块("\n\n" 分隔),且在边界段之前(S1 槽序 < S4 槽序)
        let out_id = exec
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: Some("【身份资产】我是谁/服务谁/边界自述/基调"),
                north_star: None,
                memory: None,
                recall: &Default::default(),
                total_window: 0,
                boundary_segment: Some("【能力边界声明】boundary"),
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert!(out_id.starts_with("base\n\n【身份资产】我是谁/服务谁/边界自述/基调"));
        let id_pos = out_id.find("【身份资产】").unwrap();
        let boundary_pos = out_id.find("【能力边界声明】").unwrap();
        assert!(id_pos < boundary_pos, "S1 槽序:基底块→身份段→…→边界段");
        // C-4:身份段+北极星锚:S1 槽内序 = 基底块→身份段→北极星锚(逐字节)
        let out_ns = exec
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: Some("【身份资产】identity"),
                north_star: Some("【北极星】north star"),
                memory: None,
                recall: &Default::default(),
                total_window: 0,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert_eq!(
            out_ns,
            "base\n\n【身份资产】identity\n\n【北极星】north star"
        );
    }

    /// S3c_todo(批K 段4):todo 清单摘要经配方槽位注入——Some → S3_memory
    /// 之后 S4_boundary 之前(separator "\n\n");None → 槽位静默跳过(既有
    /// agent 逐字节零影响,v1.0→v1.1 minor 增补等价铁律)
    #[test]
    fn test_assemble_todo_summary_slot_order() {
        let exec = AssemblyExecutor::default_executor();
        // 有摘要:紧随基底块(唯一非空槽,"\n\n" 分隔逐字节);且在边界段之前
        let out = exec
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &Default::default(),
                total_window: 0,
                boundary_segment: Some("【能力边界声明】boundary"),
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: Some("【清单】- [*] t1 先做"),
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert_eq!(
            out,
            "base\n\n【清单】- [*] t1 先做\n\n【能力边界声明】boundary"
        );
        let todo_pos = out.find("【清单】").unwrap();
        let boundary_pos = out.find("【能力边界声明】").unwrap();
        assert!(
            todo_pos < boundary_pos,
            "S3c 槽序:todo 清单摘要 前于 边界段"
        );
        // None → 槽位静默跳过(输出与 v1.0 逐字节一致)
        let out_none = exec
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &Default::default(),
                total_window: 0,
                boundary_segment: Some("【能力边界声明】boundary"),
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert!(!out_none.contains("【清单】"));
    }

    /// 执行器预算参数访问:默认配方 = 现状硬编码值
    #[test]
    fn test_executor_budget_params() {
        let exec = AssemblyExecutor::default_executor();
        assert_eq!(exec.memory_budget_ratio(), Some(0.25));
        assert_eq!(exec.reserve_for_response_pct(), 25);
        assert_eq!(exec.tool_result_max_chars(), 48_000);
        let (strategy, buffer_pct, hint) = exec.trim_params().unwrap();
        assert_eq!(strategy, "KeepSystemKeepLast");
        assert_eq!(buffer_pct, 5);
        assert_eq!(hint, 15);
    }

    /// manifest 槽位:skills 未声明 → 不注入(历史行为逐字节等价)
    #[test]
    fn test_assemble_manifest_without_skills_not_injected() {
        let exec = AssemblyExecutor::default_executor();
        let out = exec
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &Default::default(),
                total_window: 0,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert_eq!(out, "base");
        assert!(!out.contains("可用技能清单"));
    }

    /// manifest 槽位:skills 声明 → 渲染技能清单段(名称+描述逐行)
    #[test]
    fn test_assemble_manifest_renders_skill_lines() {
        use crate::agent::definition::SkillManifestEntry;
        let skills = vec![
            SkillManifestEntry {
                name: "pdf-processing".to_string(),
                path: std::path::PathBuf::from("/x/pdf/SKILL.md"),
                description: "提取 PDF 文本/表格并生成摘要".to_string(),
            },
            SkillManifestEntry {
                name: "git-discipline".to_string(),
                path: std::path::PathBuf::from("/x/git/SKILL.md"),
                description: "提交前先看 diff".to_string(),
            },
        ];
        let exec = AssemblyExecutor::default_executor();
        let out = exec
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &Default::default(),
                total_window: 0,
                boundary_segment: None,
                skills: Some(&skills),
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert!(out.starts_with("base\n\n"));
        assert!(out.contains("【可用技能清单】以下技能可经 read_skill 工具装载正文："));
        assert!(out.contains("\n- pdf-processing: 提取 PDF 文本/表格并生成摘要"));
        assert!(out.contains("\n- git-discipline: 提交前先看 diff"));
        assert!(out.ends_with("\n按需装载，不要一次性全部读取。\n注：清单自本次会话创建时生效；会话中新建或修改的技能，自下次会话起出现。"));
    }

    /// manifest 槽位:紧随边界段之后(同为稳定尾注位,语义分层:能力锚在边界锚后)
    #[test]
    fn test_assemble_manifest_follows_boundary_segment() {
        use crate::agent::definition::SkillManifestEntry;
        let skills = vec![SkillManifestEntry {
            name: "s".to_string(),
            path: std::path::PathBuf::from("/x/SKILL.md"),
            description: "d".to_string(),
        }];
        let exec = AssemblyExecutor::default_executor();
        let out = exec
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &Default::default(),
                total_window: 0,
                boundary_segment: Some("【能力边界声明】boundary"),
                skills: Some(&skills),
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        let boundary_pos = out.find("【能力边界声明】").expect("boundary present");
        let manifest_pos = out.find("【可用技能清单】").expect("manifest present");
        assert!(boundary_pos < manifest_pos, "manifest must follow boundary");
        assert!(out.contains("【能力边界声明】boundary\n\n【可用技能清单】"));
    }

    /// manifest 槽位:配方声明 enabled=false → 硬关(有技能也不注入)
    #[test]
    fn test_assemble_manifest_disabled_hard_off() {
        use crate::agent::definition::SkillManifestEntry;
        let skills = vec![SkillManifestEntry {
            name: "s".to_string(),
            path: std::path::PathBuf::from("/x/SKILL.md"),
            description: "d".to_string(),
        }];
        let mut recipe = AssemblyRecipe::default();
        for s in &mut recipe.slots {
            if s.id == "S4b_skills" {
                s.enabled = false;
            }
        }
        let out = AssemblyExecutor::new(recipe)
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &Default::default(),
                total_window: 0,
                boundary_segment: None,
                skills: Some(&skills),
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert_eq!(out, "base");
    }

    /// manifest 槽位:skills 空数组 → 不注入(与 None 同语义)
    #[test]
    fn test_assemble_manifest_empty_skills_not_injected() {
        let exec = AssemblyExecutor::default_executor();
        let out = exec
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &Default::default(),
                total_window: 0,
                boundary_segment: None,
                skills: Some(&[]),
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert_eq!(out, "base");
    }

    /// A-1 完全切换验证(2026-10-03):默认配方基数=input(输入侧);显式
    /// `base: total_window` 声明=兼容口径仍合法——同内容记忆在 input 口径下
    /// 预算更小、裁剪更早,显式声明者行为与历史一致(向后兼容实测)
    #[test]
    fn test_budget_base_input_switches_memory_cap() {
        use crate::agent::memory::{MemoryManager, MemoryRecord, RecallContext};

        let client = crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:18080");
        let mem = MemoryManager::new("a1-demo", client);
        let mk = |k: &str, v: &str| MemoryRecord {
            key: k.to_string(),
            value: v.to_string(),
            timestamp: 1_760_000_000,
            source: None,
            confidence: None,
            tags: Vec::new(),
            // 夹具代表账本来源条目(带锚);[unanchored] 标注走独立单测
            fact_id: Some(1),
            usage_count: 0,
            effective_uses: None,
            lifecycle_state: None,
            cause_fact_id: None,
            evidence: None,
        };
        // CJK 1:1 估算:600 字符/条 ×3 条 ≈1800 tokens,落在 input 口径
        // cap(8192-25%=6144;6144×0.25=1536)与 total_window 口径
        // cap(8192×0.25=2048)之间——两侧裁剪行为必然分叉
        let long_fact = "长".repeat(600);
        let recall = RecallContext {
            stable: vec![
                mk("f1", &long_fact),
                mk("f2", &long_fact),
                mk("f3", &long_fact),
            ],
            ..Default::default()
        };

        // 默认配方(基数=input,完全切换后口径)
        let out_input = AssemblyExecutor::new(AssemblyRecipe::default())
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: Some(&mem),
                recall: &recall,
                total_window: 8192,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        // 显式兼容口径(base=total_window,历史行为)
        let mut total_recipe = AssemblyRecipe::default();
        for s in &mut total_recipe.slots {
            if s.id == "S3_memory" {
                if let Some(b) = s.budget.as_mut() {
                    b.base = "total_window".to_string();
                }
            }
        }
        let out_total = AssemblyExecutor::new(total_recipe)
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: Some(&mem),
                recall: &recall,
                total_window: 8192,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();

        assert_ne!(out_input, out_total, "口径切换必须改变记忆区预算效果");
        assert!(
            out_input.len() < out_total.len(),
            "input 口径 cap(1536) < total_window 口径 cap(2048),记忆区应裁得更紧"
        );
    }

    /// 「改配方不改码」演示(PR-4):v1.1 配方 JSON(base=input)与 v1.0 JSON
    /// (base=total_window)同为合法数据——schema/执行器零改动,旧配方向后
    /// 兼容,新配方纯数据 diff 生效(配方资产属性实弹验证)
    #[test]
    fn test_recipe_v1_1_json_data_only_evolution() {
        let v10 = r#"{
            "recipe_version": "recipe-v1.0",
            "slots": [
                { "id": "S1_base", "source": "definition.system_prompt" },
                { "id": "S3_memory", "source": "recall", "degradable": true,
                  "budget": { "ratio": 0.25, "base": "total_window", "clamp": [0.1, 0.5] },
                  "degradation_order": ["stable", "summaries", "events"],
                  "sections": { "notices_first": true } },
                { "id": "S5_task", "source": "goal", "role": "user" },
                { "id": "S7_history", "source": "messages",
                  "trim": { "strategy": "KeepSystemKeepLast", "buffer_pct": 5, "hint_budget_tokens": 15 } }
            ],
            "budget": { "window_tokens_source": "definition.context_window_tokens",
                        "reserve_for_response_pct": 25, "tool_result_max_chars": 48000 },
            "recall_scope": { "max_session_summaries_ref": "sediment_config",
                              "max_injected_events_ref": "sediment_config" }
        }"#;
        // v1.1 = v1.0 的纯数据 diff:版本号 minor bump + 预算基准切换
        let v11 = v10
            .replace("recipe-v1.0", "recipe-v1.1")
            .replace("total_window", "input");

        // 旧配方向后兼容:仍加载、仍 validate、仍可组装
        let r10: AssemblyRecipe = serde_json::from_str(v10).unwrap();
        r10.validate().unwrap();
        AssemblyExecutor::new(r10)
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &Default::default(),
                total_window: 8192,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        // 新配方:加载 → validate → 字段落位
        let r11: AssemblyRecipe = serde_json::from_str(&v11).unwrap();
        r11.validate().unwrap();
        assert_eq!(r11.recipe_version, "recipe-v1.1");
        let exec11 = AssemblyExecutor::new(r11);
        assert_eq!(
            exec11
                .slot("S3_memory")
                .unwrap()
                .budget
                .as_ref()
                .unwrap()
                .base,
            "input"
        );
        exec11
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &Default::default(),
                total_window: 8192,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
    }

    /// 交接底座包渲染:字段序固定+可选项省略+确定性(同输入逐字节同输出)
    #[test]
    fn test_render_handoff_base_deterministic_shape() {
        use crate::agent::definition::HandoffPackage;
        let full = HandoffPackage {
            goal: "完成数据管线迁移".to_string(),
            milestone_current: Some("阶段 2 完成".to_string()),
            next_step: Some("跑验收测试".to_string()),
            verified_facts: vec!["构建通过".to_string(), "单测全绿".to_string()],
            dead_ends: vec!["方案甲:内存缓存不可回放".to_string()],
        };
        let out1 = render_handoff_base(&full);
        let out2 = render_handoff_base(&full);
        assert_eq!(out1, out2, "确定性:同输入逐字节同输出");
        assert!(out1.starts_with(
            "## Handoff Base
"
        ));
        assert!(out1.contains(
            "- Goal: 完成数据管线迁移
"
        ));
        assert!(out1.contains(
            "- Milestone: 阶段 2 完成
"
        ));
        assert!(out1.contains(
            "- Next Step: 跑验收测试
"
        ));
        assert!(out1.contains(
            "- Verified Facts:
  1. 构建通过
  2. 单测全绿
"
        ));
        assert!(out1.contains(
            "- Dead Ends (do not retry):
  1. 方案甲:内存缓存不可回放"
        ));

        // 可选项/空清单 → 整行/整节省略(最小包只含 goal)
        let minimal = HandoffPackage {
            goal: "只带目标".to_string(),
            milestone_current: None,
            next_step: None,
            verified_facts: Vec::new(),
            dead_ends: Vec::new(),
        };
        let out3 = render_handoff_base(&minimal);
        assert_eq!(
            out3,
            "## Handoff Base
- Goal: 只带目标"
        );
        assert!(!out3.contains("Milestone"));
        assert!(!out3.contains("Verified Facts"));
        assert!(!out3.contains("Dead Ends"));
    }

    /// 交接底座包装配:memory 在场=记忆分区后尾随底座块;memory None=
    /// 纯底座独立续跑;handoff None=零影响(既有组装逐字节不变)
    #[test]
    fn test_assemble_handoff_base_pairing() {
        use crate::agent::definition::HandoffPackage;
        use crate::agent::memory::{MemoryManager, MemoryRecord, RecallContext};
        let exec = AssemblyExecutor::default_executor();
        let handoff = HandoffPackage {
            goal: "跨会话接续".to_string(),
            milestone_current: Some("第 2 程".to_string()),
            next_step: None,
            verified_facts: vec!["F1".to_string()],
            dead_ends: Vec::new(),
        };

        // 场景 1:memory + handoff → 底座块尾随在记忆分区之后(S3 槽内)
        let mem = MemoryManager::new(
            "h1",
            crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:18080"),
        );
        let mut recall = RecallContext::default();
        recall
            .stable
            .push(MemoryRecord::new("k", "稳定事实内容", 1000));
        let out1 = exec
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: Some(&mem),
                recall: &recall,
                total_window: 8192,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: Some(&handoff),
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        let stable_pos = out1
            .find("## Stable Facts")
            .expect("memory section present");
        let handoff_pos = out1.find("## Handoff Base").expect("handoff block present");
        assert!(stable_pos < handoff_pos, "底座块尾随在记忆分区之后");
        assert!(out1.contains("- Goal: 跨会话接续"));

        // 场景 2:纯底座独立续跑(memory None)
        let out2 = exec
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &RecallContext::default(),
                total_window: 8192,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: Some(&handoff),
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert!(out2.contains("## Handoff Base"));
        assert!(out2.starts_with("base"));

        // 场景 3:handoff None → 零影响
        let out3 = exec
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &RecallContext::default(),
                total_window: 8192,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert_eq!(out3, "base");
        assert!(!out3.contains("Handoff Base"));
    }

    // ===== 注入序黄金样本（装配守护） =====
    //
    // golden v1 = 当前运行序,**含已知缺陷序(serve 注入段先于身份段)**——
    // 固化现状不等于认可现状:此后任何配方/注入序变更,CI 上 golden 不匹配即红,
    // "字面成立、结构靠后"类漂移从静默累积变为硬失败。
    // 重录方式:`GOLDEN_REWRITE=1 cargo test --lib injection_order -- --nocapture`
    // (重录必配人工段序裁定;双版本 golden 为回滚对照基线)。
    // L2 前馈段以固定库存 fixture 经渲染纯函数驱动(真实通路=服务端库存拉取,
    // 渲染函数同源;追加模式与 apply_l2_feed_forward 一致)。

    #[tokio::test]
    async fn test_injection_order_golden_sample() {
        // S2 前移 golden v2(装配序设计 §5.1 v2 目标序落地):
        // v2 全文序=基底 → 身份段 → 北极星锚 → 治理门禁段 → 记忆区 → …
        // (K-17 缺陷序已修正:治理段经组装层 S2_governance 独立槽位渲染,
        // 不再改写 def.system_prompt——身份段前于治理段,结构断言随 v2 翻转)。
        // golden v1 双件(assembly/full)留档为回滚对照基线,不再断言。
        // 重录方式:GOLDEN_REWRITE=1 cargo test --lib injection_order -- --nocapture
        use crate::agent::definition::{CapabilityBoundary, HandoffPackage, SkillManifestEntry};

        use crate::rule_tools::local_handlers::render_l2_inventory_summary;
        use std::path::PathBuf;

        const GOLDEN_ASSEMBLY: &str = "tests/fixtures/golden_injection_order_assembly_v1.txt";
        const GOLDEN_FULL_V2: &str = "tests/fixtures/golden_injection_order_full_v2.txt";

        // ---- 固定输入件(全确定性,零网络) ----
        let base = "【基底】固定基底提示词。";
        let identity = "【身份资产】测试身份段。";
        let north_star = "【北极星】测试目标锚。";
        let boundary = CapabilityBoundary {
            mode: "read_only".to_string(),
            sandbox_root: PathBuf::from("/srvbox/golden"),
            read_root: None,
            tools: vec!["file_read".to_string()],
        };
        let skills = vec![SkillManifestEntry {
            name: "demo-skill".to_string(),
            path: PathBuf::from("/skills/demo/SKILL.md"),
            description: "演示技能(黄金样本固定件)".to_string(),
        }];
        let handoff = HandoffPackage {
            goal: "黄金样本固定目标".to_string(),
            milestone_current: Some("固化期".to_string()),
            next_step: None,
            verified_facts: vec!["组装纯函数".to_string()],
            dead_ends: Vec::new(),
        };
        let mut recall = crate::agent::memory::RecallContext::default();
        let mut stable = crate::agent::memory::MemoryRecord::new(
            "golden.stable",
            "固定稳定事实内容。",
            1_700_000_000,
        );
        stable.source = Some("system".to_string());
        // 夹具代表账本来源条目(带锚);[unanchored] 标注走独立单测
        stable.fact_id = Some(1);
        recall.stable.push(stable);
        let mut golden_session = crate::agent::memory::MemoryRecord::new(
            "golden.session",
            "固定会话摘要。",
            1_700_000_100,
        );
        golden_session.fact_id = Some(2);
        recall.summaries.push(golden_session);
        let mut golden_event = crate::agent::memory::MemoryRecord::new(
            "golden.events.e1",
            "固定事件。",
            1_700_000_200,
        );
        golden_event.fact_id = Some(3);
        recall.events.push(golden_event);
        let mem = crate::agent::memory::MemoryManager::new(
            "golden-ns",
            crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:18080"),
        );
        let exec = AssemblyExecutor::default_executor();

        // ---- (a) assemble 单体黄金(无治理段;CLI 形态,S2 槽位缺席合法) ----
        let assembly_out = exec
            .assemble(AssemblyInputs {
                base_prompt: base,
                identity_segment: Some(identity),
                north_star: Some(north_star),
                memory: Some(&mem),
                recall: &recall,
                total_window: 8192,
                boundary_segment: Some(&boundary.awareness_segment()),
                skills: Some(&skills),
                skills_notice: None, // skills_notice
                handoff: Some(&handoff),
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();

        // ---- (b) serve 形态黄金 v2:治理段经 S2 槽位进组装(身份段前于治理段) ----
        // 三段构造与 serve_tools::build_governance_segment 同源同序:
        // L2 约束前馈(固定库存 fixture 经渲染纯函数) → 进化信号感知 → 规范入口索引
        let mut governance = String::new();
        let l2_inventory = serde_json::json!({
            "count": 1,
            "files": [
                {"path": "guard-demo", "title": "演示守卫规则", "guard_for": ["deploy"]}
            ],
        });
        if let Some(l2) = render_l2_inventory_summary(&l2_inventory) {
            governance.push_str(&l2);
            governance.push_str(
                "

",
            );
        }
        governance.push_str(crate::api::serve_tools::EVOLUTION_AWARENESS_SEGMENT);
        governance.push_str(
            "

",
        );
        governance.push_str(crate::api::serve_tools::REGULATION_INDEX_AWARENESS_SEGMENT);
        let full_v2 = exec
            .assemble(AssemblyInputs {
                base_prompt: base,
                identity_segment: Some(identity),
                north_star: Some(north_star),
                memory: Some(&mem),
                recall: &recall,
                total_window: 8192,
                boundary_segment: Some(&boundary.awareness_segment()),
                skills: Some(&skills),
                skills_notice: None, // skills_notice
                handoff: Some(&handoff),
                governance_segment: Some(&governance),
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();

        // ---- 写录/比对 ----
        if std::env::var("GOLDEN_REWRITE").is_ok() {
            std::fs::write(GOLDEN_ASSEMBLY, &assembly_out).unwrap();
            std::fs::write(GOLDEN_FULL_V2, &full_v2).unwrap();
            println!("[golden] rewritten: {GOLDEN_ASSEMBLY} + {GOLDEN_FULL_V2}");
            return;
        }
        let expect_v2 = std::fs::read_to_string(GOLDEN_FULL_V2).unwrap_or_else(|e| {
            panic!(
                "golden v2 缺失({e});重录=GOLDEN_REWRITE=1 cargo test --lib injection_order -- --nocapture"
            )
        });
        // v1 断言维持:无治理段=CLI 形态输出与 golden v1 逐字节一致
        assert_eq!(
            assembly_out,
            std::fs::read_to_string(GOLDEN_ASSEMBLY).unwrap()
        );
        assert_eq!(
            full_v2, expect_v2,
            "golden v2 失配——注入序/配方/治理段变更必须显式重录(装配守护语义)"
        );
        // v2 结构断言(相对 v1 翻转):身份段/北极星锚 前于 治理门禁段
        let identity_pos = full_v2.find(identity).unwrap();
        let anchor_pos = full_v2.find(north_star).unwrap();
        let l2_pos = full_v2.find("【L2 约束边界").unwrap();
        assert!(
            identity_pos < l2_pos && anchor_pos < l2_pos,
            "v2 结构预期:身份段/北极星锚 前于治理门禁段(S2 前移已落地)"
        );
    }

    // ---- S2d 服务目录段 ----

    /// 目录槽位:Some 形态必须落在 agents_md(静态区尾)之后、todo(动态尾区)
    /// 之前;None 形态 = 目录空,槽位静默跳过(输出逐字节可预期)
    #[test]
    fn test_assemble_service_catalog_slot_order() {
        let executor = AssemblyExecutor::new(AssemblyRecipe::default());
        let out = executor
            .assemble(AssemblyInputs {
                base_prompt: "BASE",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &RecallContext::default(),
                total_window: 0,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: Some("GOV"),
                knowledge_segment: Some("KNOW"),
                agents_md: Some("AGENTSMD"),
                todo_summary: Some("TODO"),
                service_catalog: Some("## 服务能力目录\nCATALOG"),
            })
            .unwrap();
        let agents = out.find("AGENTSMD").expect("agents md segment present");
        let catalog = out.find("CATALOG").expect("catalog segment present");
        let todo = out.find("TODO").expect("todo summary present");
        assert!(agents < catalog, "目录段必须位于项目记忆之后(静态区内)");
        assert!(catalog < todo, "目录段必须位于动态尾区之前");
        let out_none = executor
            .assemble(AssemblyInputs {
                base_prompt: "BASE",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &RecallContext::default(),
                total_window: 0,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: Some("AGENTSMD"),
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert_eq!(out_none, "BASE\n\nAGENTSMD");
    }

    /// 目录槽位:配方声明 enabled=false → 硬关(有目录数据也不注入)
    #[test]
    fn test_assemble_service_catalog_disabled_hard_off() {
        let mut recipe = AssemblyRecipe::default();
        for s in &mut recipe.slots {
            if s.id == "S2d_catalog" {
                s.enabled = false;
            }
        }
        let out = AssemblyExecutor::new(recipe)
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &RecallContext::default(),
                total_window: 0,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: Some("CATALOG"),
            })
            .unwrap();
        assert_eq!(out, "base");
    }

    /// 旧自定义配方(无 S2d 槽)仍合法:目录数据在场也不注入(槽位驱动,非数据驱动)
    #[test]
    fn test_recipe_without_catalog_slot_stays_valid() {
        let mut recipe = AssemblyRecipe::default();
        recipe.slots.retain(|s| s.id != "S2d_catalog");
        let out = AssemblyExecutor::new(recipe)
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &RecallContext::default(),
                total_window: 0,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: Some("AGENTSMD"),
                todo_summary: None,
                service_catalog: Some("CATALOG"),
            })
            .unwrap();
        assert_eq!(out, "base\n\nAGENTSMD");
    }

    // ===== 批 B-268 γ-1:机制消息开关与动态区外移分叉 =====

    /// 开启态配方夹具(mechanism_messages 声明 = Some)
    fn mechanism_on_recipe() -> AssemblyRecipe {
        let mut r = AssemblyRecipe::default();
        r.budget.mechanism_messages = Some(MechanismMessagesSpec::default());
        r
    }

    /// 显式关闭配方夹具(五轮裁定批 G2/B-303:缺省已翻转,显式 null 才是关闭)
    fn mechanism_off_recipe() -> AssemblyRecipe {
        let mut r = AssemblyRecipe::default();
        r.budget.mechanism_messages = None;
        r
    }

    /// 缺省不变式(五轮裁定批 G2/B-303):默认配方 mechanism_messages = Some
    /// (开启);显式关闭态 assemble_split 与 assemble 逐字节一致且机制段 None
    /// (动态区仍嵌 system prompt 内 = 旧缺省行为)
    #[test]
    fn test_mechanism_messages_default_on_and_explicit_off_byte_identical() {
        use crate::agent::memory::{MemoryManager, MemoryRecord};
        assert!(AssemblyRecipe::default()
            .budget
            .mechanism_messages
            .is_some());
        let exec = AssemblyExecutor::new(mechanism_off_recipe());
        let mem = MemoryManager::new(
            "b268-off",
            crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:18080"),
        );
        let mut recall = RecallContext::default();
        recall
            .stable
            .push(MemoryRecord::new("k", "稳定事实内容", 1000));
        let embedded = exec
            .assemble(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: Some(&mem),
                recall: &recall,
                total_window: 8192usize,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: Some("TODO_SEG"),
                service_catalog: None,
            })
            .unwrap();
        let (static_prompt, mechanism) = exec
            .assemble_split(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: Some(&mem),
                recall: &recall,
                total_window: 8192usize,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: Some("TODO_SEG"),
                service_catalog: None,
            })
            .unwrap();
        assert!(mechanism.is_none(), "关闭态机制段必须为 None");
        assert_eq!(static_prompt, embedded, "关闭态静态区 = 全量组装逐字节一致");
    }

    /// 开启态:动态区(记忆分区/handoff/todo)外移为机制段;静态区不含任何
    /// 机制分区标记;机制段内序 = 记忆分区 → Handoff Base → todo(槽位序)
    #[test]
    fn test_assemble_split_enabled_moves_dynamic_region_out() {
        use crate::agent::definition::HandoffPackage;
        use crate::agent::memory::{MemoryManager, MemoryRecord};
        let exec = AssemblyExecutor::new(mechanism_on_recipe());
        let mem = MemoryManager::new(
            "b268-on",
            crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:18080"),
        );
        let mut recall = RecallContext::default();
        recall
            .stable
            .push(MemoryRecord::new("k", "稳定事实内容", 1000));
        let handoff = HandoffPackage {
            goal: "跨会话接续".to_string(),
            milestone_current: None,
            next_step: None,
            verified_facts: Vec::new(),
            dead_ends: Vec::new(),
        };
        let (static_prompt, mechanism) = exec
            .assemble_split(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: Some(&mem),
                recall: &recall,
                total_window: 8192,
                boundary_segment: None,
                skills: None,
                skills_notice: None, // skills_notice
                handoff: Some(&handoff),
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: Some("## Current TODO\n- t1"),
                service_catalog: None,
            })
            .unwrap();
        assert!(static_prompt.starts_with("base"), "静态区保留头部基底");
        assert!(
            !static_prompt.contains("## Stable Facts")
                && !static_prompt.contains("## Handoff Base")
                && !static_prompt.contains("Current TODO"),
            "开启态静态区不含任何机制分区标记: {static_prompt}"
        );
        let seg = mechanism.expect("开启态机制段在场");
        let stable_pos = seg.find("## Stable Facts").expect("记忆分区在机制段");
        let handoff_pos = seg.find("## Handoff Base").expect("底座块在机制段");
        let todo_pos = seg.find("## Current TODO").expect("todo 在机制段");
        assert!(
            stable_pos < handoff_pos && handoff_pos < todo_pos,
            "机制段内序 = 槽位序"
        );
    }

    /// 开启态无动态内容物(memory/handoff/todo 全 None)→ 机制段 None,
    /// 静态区 = 全量组装(无机制消息可追加)
    #[test]
    fn test_assemble_split_enabled_no_dynamic_sources_yields_none() {
        let exec = AssemblyExecutor::new(mechanism_on_recipe());
        let (static_prompt, mechanism) = exec
            .assemble_split(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: None,
                recall: &RecallContext::default(),
                total_window: 8192,
                boundary_segment: None,
                skills: None,
                skills_notice: None,
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert!(mechanism.is_none());
        assert_eq!(static_prompt, "base");
    }

    /// 机制消息降级同参声明(spec 预留字段,γ-3 消费):缺省 = β 同参 4/2000
    #[test]
    fn test_mechanism_messages_spec_defaults_reuse_beta_params() {
        let spec = MechanismMessagesSpec::default();
        assert_eq!(spec.stale_min_turns, 4);
        assert_eq!(spec.stale_min_bytes, 2_000);
    }

    /// JSON 形态:缺键 = None(关闭);显式声明 = Some;关闭态序列化不出现
    /// 该键(形状与 β 段同款:Option+skip_serializing_if)
    #[test]
    fn test_mechanism_messages_spec_json_roundtrip() {
        #[derive(Serialize, Deserialize)]
        struct Probe {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            mechanism_messages: Option<MechanismMessagesSpec>,
        }
        let off: Probe = serde_json::from_str("{}").unwrap();
        assert!(off.mechanism_messages.is_none());
        assert_eq!(serde_json::to_string(&off).unwrap(), "{}");
        let on: Probe = serde_json::from_str(r#"{"mechanism_messages":{}}"#).unwrap();
        assert!(on.mechanism_messages.is_some());
        assert_eq!(
            serde_json::to_string(&on).unwrap(),
            r#"{"mechanism_messages":{}}"#
        );
    }

    // ===== 批 B-268 γ-4:开启态升版绑定 + 五件验收 =====

    /// 升版绑定开启态(五轮裁定批 G2/B-303 语义):缺省配方(翻转后=开启)
    /// effective 版本 = 次版本 +1;显式关闭(声明 None)沿用声明版本不升版;
    /// 用户声明版开启同样按 +1 升(版本面反映实际生效形态)
    #[test]
    fn test_effective_recipe_version_binds_to_enabled_state() {
        let on = AssemblyExecutor::default_executor();
        assert_eq!(on.effective_recipe_version(), "recipe-v1.4");
        let mut off_recipe = mechanism_off_recipe();
        off_recipe.recipe_version = "recipe-v1.3".to_string();
        let off = AssemblyExecutor::new(off_recipe);
        assert_eq!(off.effective_recipe_version(), "recipe-v1.3");
        let mut declared = mechanism_on_recipe();
        declared.recipe_version = "recipe-v1.0".to_string();
        let d = AssemblyExecutor::new(declared);
        assert_eq!(d.effective_recipe_version(), "recipe-v1.1");
    }

    /// 开启态相邻轮静态区字节级稳定:动态内容物(记忆分区)变化只进机制段,
    /// 静态区逐字节一致 = 被动前缀缓存命中前提(五件验收②)
    #[test]
    fn test_assemble_split_static_region_stable_across_dynamic_changes() {
        use crate::agent::memory::{MemoryManager, MemoryRecord};
        let exec = AssemblyExecutor::new(mechanism_on_recipe());
        let mem1 = MemoryManager::new(
            "b268-turn1",
            crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:18080"),
        );
        let mut recall1 = RecallContext::default();
        recall1.stable.push(MemoryRecord::new("k", "事实一", 1000));
        let (static1, mech1) = exec
            .assemble_split(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: Some(&mem1),
                recall: &recall1,
                total_window: 8192,
                boundary_segment: None,
                skills: None,
                skills_notice: None,
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        let mem2 = MemoryManager::new(
            "b268-turn2",
            crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:18080"),
        );
        let mut recall2 = RecallContext::default();
        recall2.stable.push(MemoryRecord::new("k", "事实一", 1000));
        recall2.stable.push(MemoryRecord::new("k2", "事实二", 2000));
        let (static2, mech2) = exec
            .assemble_split(AssemblyInputs {
                base_prompt: "base",
                identity_segment: None,
                north_star: None,
                memory: Some(&mem2),
                recall: &recall2,
                total_window: 8192,
                boundary_segment: None,
                skills: None,
                skills_notice: None,
                handoff: None,
                governance_segment: None,
                knowledge_segment: None,
                agents_md: None,
                todo_summary: None,
                service_catalog: None,
            })
            .unwrap();
        assert_eq!(static1, static2, "静态区跨轮字节级稳定");
        assert_ne!(mech1, mech2, "机制段承载动态变化");
    }
}
