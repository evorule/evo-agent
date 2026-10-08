//! 长程设计档阶段 2（F-610/R1）：MemoryRecipe——记忆策略规则集（账本定律）。
//!
//! 定位：**策略即规则集**——召回权重/半衰期/降级序/生命周期阈值全部为
//! 可序列化数据（definition 内嵌 `memory.recipe`），版本随结果落链；
//! 改策略不改码（热重载随阶段 2 演进）。
//!
//! 边界纪律（设计档 R1/本档 §边界）：**抽取器是机制（代码），权重是策略
//! （本数据）**——分词器/索引/账本通路不进 Recipe。
//!
//! 默认值=现行硬编码行为（无 recipe 时零影响，既有测试逐字节兼容）。

use serde::{Deserialize, Serialize};

/// 检索策略配置（记忆设计档 §八 retrieval 节）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalConfig {
    /// 相关性权重（词法 bigram 命中）
    #[serde(default = "default_w_relevance")]
    pub w_relevance: f32,
    /// 新鲜度权重
    #[serde(default = "default_w_recency")]
    pub w_recency: f32,
    /// 重要性权重
    #[serde(default = "default_w_importance")]
    pub w_importance: f32,
    /// importance 内 confidence 权重（w_c，F-616 stable 面接通）
    #[serde(default = "default_w_confidence")]
    pub w_confidence: f32,
    /// importance 内 usage 加成权重（w_u，k 封顶防垄断）
    #[serde(default = "default_w_usage")]
    pub w_usage: f32,
    /// importance 内来源权威权重（w_a；缺省 0=既有行为零影响；
    /// user 1.0/system 0.8/llm 0.5,与置信度演化 w_e 同源,账本记忆设计档 §5.1）
    #[serde(default)]
    pub w_authority: f32,
    /// importance 内实体度权重（w_e；批内实体共现度归一;缺省 0=零影响）
    #[serde(default)]
    pub w_entity: f32,
    /// usage 计数封顶（k：min(usage,k) 防高频条目垄断排序）
    #[serde(default = "default_usage_cap")]
    pub usage_cap: u32,
    /// 半衰期（天）：情景/语义分型
    #[serde(default = "default_half_life")]
    pub half_life_days: HalfLifeDays,
}

fn default_w_relevance() -> f32 {
    0.5
}
fn default_w_recency() -> f32 {
    0.3
}
fn default_w_importance() -> f32 {
    0.2
}
fn default_w_confidence() -> f32 {
    1.0
}
fn default_w_usage() -> f32 {
    0.2
}
fn default_usage_cap() -> u32 {
    10
}
fn default_half_life() -> HalfLifeDays {
    HalfLifeDays {
        episodic: 14.0,
        semantic: 90.0,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HalfLifeDays {
    /// 情景记忆半衰期（天）
    pub episodic: f64,
    /// 语义记忆半衰期（天）
    pub semantic: f64,
}

impl Default for HalfLifeDays {
    fn default() -> Self {
        default_half_life()
    }
}

/// 生命周期阈值（F-609 状态机迁移参数）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LifecycleConfig {
    /// 晋升最低置信度
    #[serde(default = "default_promote_confidence")]
    pub promote_min_confidence: f32,
    /// 晋升最少使用次数
    #[serde(default = "default_promote_uses")]
    pub promote_min_uses: u32,
    /// 空闲归档天数
    #[serde(default = "default_archive_idle")]
    pub archive_after_idle_days: u64,
    /// rollup 阈值（现 def.memory.summary_rollup_threshold 的 Recipe 覆盖位）
    #[serde(default = "default_rollup_threshold")]
    pub rollup_threshold: usize,
    /// 置信度演化开关（缺省关=既有 agent 零影响;开=reinforce 佐证 +0.05·w_e/
    /// 裁决败者矛盾 −0.10·w_e,公式见账本记忆设计档 §4.2）
    #[serde(default)]
    pub confidence_evolution: bool,
    /// decay 闲置阈值（天）:非 Captured 且零引用超此限 → Decayed（缺省 90;
    /// 需 < archive_after_idle_days 方有意义,否则归档先至）
    #[serde(default = "default_decay_after_idle_days")]
    pub decay_after_idle_days: u64,
}

fn default_promote_confidence() -> f32 {
    0.8
}
fn default_promote_uses() -> u32 {
    3
}
fn default_archive_idle() -> u64 {
    180
}
fn default_decay_after_idle_days() -> u64 {
    90
}
fn default_rollup_threshold() -> usize {
    10
}

/// 降级序（A-1 口径：默认 stable>summaries>events）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetConfig {
    /// 预算降级顺序（fit_recall 执行序）
    #[serde(default = "default_degradation")]
    pub degradation_order: Vec<String>,
}

/// 自省工具暴露面声明（F-611）。
///
/// 工具暴露是策略不是代码：哪些自省记忆工具随 LLM 请求下发
/// （`openai_tools_payload`）由本节声明，未声明=不暴露（既有 agent 零影响）。
/// 前置条件=LexStore 已配置（检索缓存是自省检索的数据前提）。
/// 只读两件（`memory_search`/`memory_get`）随声明注册；写面工具
/// （propose/link/forget）依赖治理闸，未开放声明（声明了也不注册）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ToolsSection {
    /// 暴露的自省工具名集合（白名单语义；未知名在接线时 warn 跳过）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose: Vec<String>,
    /// memory_link 关系类型白名单（A-MEM 式关联；空=内建四类
    /// related/derives/supports/contradicts，声明即整体覆盖）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub link_relations: Vec<String>,
}

/// 矛盾裁决配置（阶段 3 F-612，记忆设计档 §八 adjudication 节）。
///
/// 裁决是策略：维度序/否定词表/相似阈值全部数据化。`enabled` 缺省关=
/// 既有 agent 零影响（裁决会改变 wire 视图呈现集——败者不再进 prompt，
/// 属行为变更，须显式开启）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdjudicationConfig {
    /// 是否启用召回期矛盾裁决（缺省关）
    #[serde(default)]
    pub enabled: bool,
    /// 裁决维度序（缺省 authority>confidence>freshness）
    #[serde(default = "default_adjudication_order")]
    pub order: Vec<String>,
    /// 否定词表（词法极性判定；缺省中英常用否定词）
    #[serde(default = "default_negation_markers")]
    pub negation_markers: Vec<String>,
    /// 词面相似阈值（分词集 Jaccard；缺省 0.6）
    #[serde(default = "default_similarity_threshold")]
    pub similarity_threshold: f32,
}

fn default_adjudication_order() -> Vec<String> {
    vec![
        "authority".to_string(),
        "confidence".to_string(),
        "freshness".to_string(),
    ]
}

/// 否定词表缺省值：数据文件外置（`data/negation_markers.json`，include_str!
/// 编译期嵌入；解析失败回退内置保守表）——治理批扩词表改数据文件即可，
/// 不动代码。
///
/// 扩充纪律（词法极性判定是子串包含匹配，`contains` 口径）：
/// - 只收「良性碰撞率低 + 否定义无歧义」的词形；高频良性子串不入表
///   （如 `非`⊂非常/非法、`别`⊂特别/级别、`未`⊂未来、`no`⊂node/now）——
///   误翻极性的代价比漏检更高（I2 词法子集同款保守取向）。
/// - 英文优先短语级（`must not`/`do not`），单字短词慎收。
/// - 经验教训（补齐路线图 P1-3 实测）：否定词内插会打碎 CJK 双字 bigram
///   相似度——词表 breadth 与相似度阈值需联调，一切以试运行探针实测为准。
fn default_negation_markers() -> Vec<String> {
    const EMBEDDED: &str = include_str!("data/negation_markers.json");
    match serde_json::from_str::<serde_json::Value>(EMBEDDED) {
        Ok(v) => v
            .get("markers")
            .and_then(|m| m.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_else(|| fallback_negation_markers()),
        Err(_) => fallback_negation_markers(),
    }
}

/// 回退保守表（历史缺省；数据文件损坏时兜底，行为等同旧版）
fn fallback_negation_markers() -> Vec<String> {
    vec![
        "不".to_string(),
        "无".to_string(),
        "禁止".to_string(),
        "not".to_string(),
        "never".to_string(),
    ]
}

fn default_similarity_threshold() -> f32 {
    0.6
}

impl Default for AdjudicationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            order: default_adjudication_order(),
            negation_markers: default_negation_markers(),
            similarity_threshold: default_similarity_threshold(),
        }
    }
}

/// 晋升治理门配置（阶段 4 F-609 完善，资产化时序「批即行权、机器上限」）。
///
/// 治理闸只管晋升不管记录：开启后，状态机阈值合格的晋升候选**不再机械
/// 晋升**，须经治理写通路提议入账（`propose_knowledge_entry`，服务端
/// 全闸链：入账契约+领域 schema+LLM 边界+凭据扫描，Draft 落账=资格
/// 凭据），回执成功才打 Promoted；提议失败保持 Captured 留待下次批
/// （M3 重复提案拒绝=天然幂等防护）。缺省关=机械复制既有行为零影响。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromoteGateConfig {
    /// 是否启用晋升治理门（缺省关=机械复制既有行为）
    #[serde(default)]
    pub enabled: bool,
    /// 治理层知识数据集 ID（提议入账目标；启用时必填，缺省视为未启用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_id: Option<String>,
    /// 提议入账后自动机器行权（A2-4 接线；缺省关=Draft 只存不动，保守起步）：
    /// 开启后 propose 回执成功即调 transition_knowledge_entry（服务端机器闸六检，
    /// 全过放行 Active，非全过 422 fail-visible 候选保持 Promoted-Draft 形态）。
    #[serde(default)]
    pub auto_transition: bool,
}

impl Default for PromoteGateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dataset_id: None,
            auto_transition: false,
        }
    }
}

/// 跨源检索源启用开关（跨源注册规格策略面）。
///
/// 源启用是策略不是代码：新增记忆源（journal 摘要投影/技能索引/
/// 北极星残余节/材料）的注册与检索暴露由本节声明，缺省全关=
/// 既有 agent 零影响。semantic/episodic/会话摘要三族的既有覆盖不受
/// 本节管辖（恒启用）。serde 宽容语义与既有节一致（未知名忽略）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SourcesSection {
    /// journal 摘要投影源（work 型，sediment 会话末派生品）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub journal_digest: bool,
    /// 技能索引源（procedural 型，声明面镜像+正文本地索引）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skills_index: bool,
    /// 北极星 pack 残余节源（procedural 型，mirror 通道扩展）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub northstar_pack: bool,
    /// 材料源（procedural 型，随程序记忆统一批落地）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub materials: bool,
    /// 双通道笔记事件驱动草稿源（work 型，sediment 确定性投影）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failure_drafts: bool,
}

fn default_degradation() -> Vec<String> {
    vec![
        "stable".to_string(),
        "summaries".to_string(),
        "events".to_string(),
    ]
}

/// MemoryRecipe：记忆策略规则集（F-610）
///
/// 来源三形态（同 AssemblyRecipe 先例）：definition 内嵌 / `$ref` 外部文件 /
/// 默认常量。`recipe_version` 随召回结果落链（版本戳纪律，同 F-302）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecipe {
    /// 配方版本（memory-v1.0 起）
    pub recipe_version: String,
    /// 检索策略
    pub retrieval: RetrievalConfig,
    /// 生命周期阈值
    pub lifecycle: LifecycleConfig,
    /// 预算降级序
    pub budget: BudgetConfig,
    /// 自省工具暴露面（F-611；缺省=不暴露任何工具）
    #[serde(default)]
    pub tools: ToolsSection,
    /// 跨源检索源启用开关（注册规格策略面；缺省=全关，既有 agent 零影响）
    #[serde(default)]
    pub sources: SourcesSection,
    /// 矛盾裁决配置（F-612；缺省 enabled=false 既有 agent 零影响）
    #[serde(default)]
    pub adjudication: AdjudicationConfig,
    /// 晋升治理门配置（F-609 完善；缺省关=机械复制既有行为）
    #[serde(default)]
    pub promote_gate: PromoteGateConfig,
    /// LexStore 检索缓存 TTL（秒；存储设计档 §九.4——缓存参数迁 Recipe，
    /// 机制不再藏策略参数。None=机制缺省 60s，既有 agent 零影响）
    #[serde(default)]
    pub lex_ttl_secs: Option<u64>,
}

impl Default for MemoryRecipe {
    fn default() -> Self {
        Self {
            recipe_version: "memory-v1.0".to_string(),
            retrieval: RetrievalConfig {
                w_relevance: default_w_relevance(),
                w_recency: default_w_recency(),
                w_importance: default_w_importance(),
                w_confidence: default_w_confidence(),
                w_usage: default_w_usage(),
                w_authority: 0.0,
                w_entity: 0.0,
                usage_cap: default_usage_cap(),
                half_life_days: default_half_life(),
            },
            lifecycle: LifecycleConfig {
                promote_min_confidence: default_promote_confidence(),
                promote_min_uses: default_promote_uses(),
                archive_after_idle_days: default_archive_idle(),
                rollup_threshold: default_rollup_threshold(),
                confidence_evolution: false,
                decay_after_idle_days: default_decay_after_idle_days(),
            },
            budget: BudgetConfig {
                degradation_order: default_degradation(),
            },
            tools: ToolsSection::default(),
            sources: SourcesSection::default(),
            adjudication: AdjudicationConfig::default(),
            promote_gate: PromoteGateConfig::default(),
            lex_ttl_secs: None,
        }
    }
}

/// R1：检索策略上下文——召回入口构造，贯穿候选→评分→截断→分档全链。
///
/// 三因子评分（记忆设计档 §5.1）：
/// `S = w_r·rel + w_t·recency + w_i·importance`
///
/// - `rel`：词法 bigram 命中数（R05，0..=cap 归一）
/// - `recency`：半衰期指数衰减（0..1]
/// - `importance`：w_c·confidence + w_u·min(usage,k)（F-616 stable 面接通；
///   usage=存量计数+本会话增量，k 封顶防垄断；配方可调）
#[derive(Debug, Clone)]
pub struct RetrievalPolicy {
    pub w_relevance: f32,
    pub w_recency: f32,
    pub w_importance: f32,
    /// importance 内 confidence 权重（w_c）
    pub w_confidence: f32,
    /// importance 内 usage 加成权重（w_u）
    pub w_usage: f32,
    /// importance 内来源权威权重（缺省 0=零影响）
    pub w_authority: f32,
    /// importance 内实体度权重（缺省 0=零影响）
    pub w_entity: f32,
    /// usage 计数封顶（k）
    pub usage_cap: u32,
    /// 情景半衰期（天）
    pub half_life_episodic_days: f64,
    /// 语义半衰期（天）
    pub half_life_semantic_days: f64,
    /// 预算降级序（fit_recall 执行序）
    pub degradation_order: Vec<String>,
    /// 配方版本戳（随召回结果落链）
    pub recipe_version: String,
}

impl RetrievalPolicy {
    /// 从 Recipe 构造（召回入口单点）
    pub fn from_recipe(recipe: &MemoryRecipe) -> Self {
        Self {
            w_relevance: recipe.retrieval.w_relevance,
            w_recency: recipe.retrieval.w_recency,
            w_importance: recipe.retrieval.w_importance,
            w_confidence: recipe.retrieval.w_confidence,
            w_usage: recipe.retrieval.w_usage,
            w_authority: recipe.retrieval.w_authority,
            w_entity: recipe.retrieval.w_entity,
            usage_cap: recipe.retrieval.usage_cap,
            half_life_episodic_days: recipe.retrieval.half_life_days.episodic,
            half_life_semantic_days: recipe.retrieval.half_life_days.semantic,
            degradation_order: recipe.budget.degradation_order.clone(),
            recipe_version: recipe.recipe_version.clone(),
        }
    }

    /// 默认策略（无 Recipe 时的现行行为等价：纯词法排序权重）
    pub fn default_lexical() -> Self {
        Self {
            w_relevance: 1.0,
            w_recency: 0.0,
            w_importance: 0.0,
            w_confidence: 1.0,
            w_usage: 0.2,
            w_authority: 0.0,
            w_entity: 0.0,
            usage_cap: 10,
            half_life_episodic_days: 14.0,
            half_life_semantic_days: 90.0,
            degradation_order: default_degradation(),
            recipe_version: "lexical-legacy".to_string(),
        }
    }

    /// 半衰期新鲜度因子（0..1]：0.5^(age_days / half_life)
    pub fn recency_factor(&self, age_days: f64, half_life_days: f64) -> f32 {
        if age_days <= 0.0 || half_life_days <= 0.0 {
            return 1.0;
        }
        (0.5f64).powf(age_days / half_life_days) as f32
    }

    /// 词法 legacy 判定：新鲜度/重要性权重全零 = 历史行为模式
    /// （该模式下评分退化为纯词法命中数，与策略数据化前逐字节一致）
    pub fn is_legacy(&self) -> bool {
        self.w_recency == 0.0 && self.w_importance == 0.0
    }
}

// =============================================================================
// 进程级策略快照槽（O-377① 批 1：bundle 内嵌 Recipe 快照的注入通路）
// =============================================================================

/// 进程级快照槽句柄类型
pub type RecipeSlot = std::sync::Arc<std::sync::RwLock<Option<MemoryRecipe>>>;

static RECIPE_SLOT: std::sync::OnceLock<RecipeSlot> = std::sync::OnceLock::new();

/// 进程级快照槽（幂等单例）。
///
/// 架构事实：serve 模式 union toolkit 进程级单次组装（`serve_tools::build_union_toolkit`），
/// `bundle_export` 工具在组装期取得读取面；发布面 = [`MemoryManager::set_recipe`]
/// （初始注入与热重载的单一收口）。批 1 取「最后发布者生效」进程级语义——
/// 同一进程的会话通常出自同一 recipe 定义源，槽内容与各会话实际策略一致；
/// 多会话异策略的会话级归属细化随恢复接线批收。
pub fn recipe_slot() -> RecipeSlot {
    RECIPE_SLOT
        .get_or_init(|| std::sync::Arc::new(std::sync::RwLock::new(None)))
        .clone()
}

/// 发布面：策略在位时写入槽（`MemoryManager::set_recipe` 单点调用）
pub fn publish_recipe(recipe: &MemoryRecipe) {
    if let Ok(mut slot) = recipe_slot().write() {
        *slot = Some(recipe.clone());
    }
}

/// 消费面：打包时刻读取当前策略（None = 无策略在位，导出不带快照——
/// 字节兼容缺省语义，与「无证据导出显式标注」同理：如实，不伪造）
pub fn current_published_recipe() -> Option<MemoryRecipe> {
    recipe_slot().read().ok().and_then(|slot| slot.clone())
}

/// 恢复侧校验原语（薄壳）：读进程槽取本地策略后走纯函数比对。
///
/// 批 1 落函数 + 单测；真实接线（bundle_import 恢复路径消费）随 O-376①② 批。
pub fn verify_bundle_recipe_snapshot(bundled: &serde_json::Value) -> Result<(), String> {
    let local =
        current_published_recipe().ok_or_else(|| "本地无策略在位，无法校验包内快照".to_string())?;
    verify_snapshot_against(bundled, &local)
}

/// 恢复侧校验纯函数：包内快照 vs 给定本地策略一致性。
///
/// 比对口径：`recipe_version` 相等 + `recipe` opaque 值级相等（serde_json
/// 结构化相等，与序列化键序无关）。防篡改不在此处——bundle 全包哈希校验
/// 由导入链 `verify_content_hash` 承担，本原语只回答「包内快照是否与本机
/// 当前策略一致」（恢复决策输入，不构成安全边界）。
pub fn verify_snapshot_against(
    bundled: &serde_json::Value,
    local: &MemoryRecipe,
) -> Result<(), String> {
    let bundled_version = bundled
        .get("recipe_version")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "包内快照缺 recipe_version 字段".to_string())?;
    if bundled_version != local.recipe_version {
        return Err(format!(
            "策略版本不一致: bundle={bundled_version} local={}",
            local.recipe_version
        ));
    }
    let local_recipe = serde_json::to_value(local).map_err(|e| e.to_string())?;
    if bundled.get("recipe") != Some(&local_recipe) {
        return Err("策略内容不一致（opaque 值级比对失配）".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_recipe_matches_current_hardcoded_behavior() {
        let r = MemoryRecipe::default();
        assert_eq!(r.recipe_version, "memory-v1.0");
        assert_eq!(r.retrieval.w_relevance, 0.5);
        assert_eq!(r.lifecycle.rollup_threshold, 10);
        assert_eq!(
            r.budget.degradation_order,
            vec!["stable", "summaries", "events"]
        );
    }

    #[test]
    fn test_negation_markers_embedded_file_loads_p2_3() {
        // 补齐路线图 P2-3:否定词表数据文件外置——嵌入文件可解析、
        // 治理批扩充词在位、碰撞敏感词不入表、回退表=历史缺省
        let markers = default_negation_markers();
        assert!(markers.len() > 5, "治理批扩充后应多于历史 5 词");
        for core in ["不", "无", "禁止", "not", "never"] {
            assert!(markers.iter().any(|m| m == core), "核心词缺失: {core}");
        }
        // 扩充抽查（中英）
        for added in ["严禁", "不得", "拒绝", "must not", "forbidden"] {
            assert!(markers.iter().any(|m| m == added), "扩充词缺失: {added}");
        }
        // 高频良性碰撞子串不入表（contains 匹配下的误翻防护：
        // 非⊂非常 / 别⊂特别 / 未⊂未来 / no⊂node）
        for banned in ["非", "别", "未", "no"] {
            assert!(
                !markers.iter().any(|m| m == banned),
                "碰撞敏感词不应入表: {banned}"
            );
        }
        // 回退表=历史缺省（数据文件损坏时行为等同旧版）
        assert_eq!(
            fallback_negation_markers(),
            vec!["不", "无", "禁止", "not", "never"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_recipe_serde_roundtrip_with_partial_input() {
        // 部分字段缺失 → serde default 补齐（配置面宽容语义）
        let json = r#"{
            "recipe_version": "memory-v1.1",
            "retrieval": { "w_relevance": 0.6, "half_life_days": { "episodic": 7, "semantic": 60 } },
            "lifecycle": {},
            "budget": {}
        }"#;
        let r: MemoryRecipe = serde_json::from_str(json).unwrap();
        assert_eq!(r.recipe_version, "memory-v1.1");
        assert!((r.retrieval.w_relevance - 0.6).abs() < 1e-6);
        assert_eq!(r.retrieval.half_life_days.episodic, 7.0);
        // 缺省子字段回默认
        assert_eq!(r.retrieval.w_recency, 0.3);
        assert_eq!(r.lifecycle.rollup_threshold, 10);
        assert_eq!(r.budget.degradation_order[0], "stable");
    }

    #[test]
    fn test_recency_factor_half_life() {
        let p = RetrievalPolicy::default_lexical();
        assert!((p.recency_factor(0.0, 14.0) - 1.0).abs() < 1e-6);
        // 一个半衰期 → 0.5
        assert!((p.recency_factor(14.0, 14.0) - 0.5).abs() < 1e-6);
        // 两个半衰期 → 0.25
        assert!((p.recency_factor(28.0, 14.0) - 0.25).abs() < 1e-6);
        // 无半衰期（0）→ 恒 1（不衰减）
        assert!((p.recency_factor(100.0, 0.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_policy_from_recipe() {
        let recipe = MemoryRecipe::default();
        let p = RetrievalPolicy::from_recipe(&recipe);
        assert_eq!(p.recipe_version, "memory-v1.0");
        assert_eq!(p.degradation_order.len(), 3);
    }

    #[test]
    fn test_snapshot_slot_publish_and_consume() {
        // 进程级槽机制冒烟：发布后槽在位（并行测试只替换不置空，is_some 无竞态；
        // 等值断言不用于全局槽——多测试并行发布会互相覆盖，等值走纯函数测试）
        let recipe = MemoryRecipe::default();
        publish_recipe(&recipe);
        assert!(current_published_recipe().is_some());
    }

    #[test]
    fn test_verify_snapshot_against_green_and_red() {
        let local = MemoryRecipe::default();
        let bundled = serde_json::json!({
            "recipe_version": local.recipe_version,
            "recipe": serde_json::to_value(&local).unwrap(),
            "snapshot_at": "2026-10-08T00:00:00Z"
        });
        // 一致 → 绿
        assert!(verify_snapshot_against(&bundled, &local).is_ok());

        // 版本篡改 → 红（伪造版本一致性负验证）
        let mut tampered = bundled.clone();
        tampered["recipe_version"] = serde_json::json!("memory-v9.9");
        let err = verify_snapshot_against(&tampered, &local).unwrap_err();
        assert!(err.contains("版本不一致"), "{err}");

        // 内容篡改 → 红（opaque 值级比对失配）
        let mut tampered = bundled.clone();
        tampered["recipe"]["retrieval"]["w_relevance"] = serde_json::json!(0.99);
        let err = verify_snapshot_against(&tampered, &local).unwrap_err();
        assert!(err.contains("内容不一致"), "{err}");

        // 缺字段 → 红（形状不完整显式拒绝，不静默）
        let err = verify_snapshot_against(&serde_json::json!({}), &local).unwrap_err();
        assert!(err.contains("recipe_version"), "{err}");
    }
}
