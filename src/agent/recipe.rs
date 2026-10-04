//! 长程设计档阶段 2（F-610/R1）：MemoryRecipe——记忆策略规则集（账本定律）。
//!
//! 定位：**策略即规则集**——召回权重/半衰期/降级序/生命周期阈值全部为
//! 可序列化数据（definition 内嵌 `memory.recipe`），版本随结果落链；
//! 改策略不改码（热重载随阶段 2 演进）。
//!
//! 边界纪律（15 号 R1/本档 §边界）：**抽取器是机制（代码），权重是策略
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
}

impl Default for MemoryRecipe {
    fn default() -> Self {
        Self {
            recipe_version: "memory-v1.0".to_string(),
            retrieval: RetrievalConfig {
                w_relevance: default_w_relevance(),
                w_recency: default_w_recency(),
                w_importance: default_w_importance(),
                half_life_days: default_half_life(),
            },
            lifecycle: LifecycleConfig {
                promote_min_confidence: default_promote_confidence(),
                promote_min_uses: default_promote_uses(),
                archive_after_idle_days: default_archive_idle(),
                rollup_threshold: default_rollup_threshold(),
            },
            budget: BudgetConfig {
                degradation_order: default_degradation(),
            },
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
/// - `importance`：v1=confidence（usage/实体度随 F-616 接入）
#[derive(Debug, Clone)]
pub struct RetrievalPolicy {
    pub w_relevance: f32,
    pub w_recency: f32,
    pub w_importance: f32,
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
}
