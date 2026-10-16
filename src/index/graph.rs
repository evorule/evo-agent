// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 符号引用图谱与个性化 PageRank（B14 结构面检索排序核心）。
//!
//! 口径（Aider repomap 路线）：
//! - 节点 = 文件；边 = 引用（`from_file → owner(被引符号名)`，weight=√出现次数）；
//! - owner 映射：同名多 def 取首见文件（file 升序，与 [`crate::index::store::IndexStore::refresh_refs`]
//!   同口径）；自环剔除；
//! - 个性化 PageRank：seed 权重按文件注入（会话打开文件×50，其余 1.0），
//!   damping 0.85，悬挂节点的质量按个性化向量重分配，收敛阈值 1e-9 或迭代上限。
//!
//! 图谱是纯派生物（由 symbols/refs 两表全量构建），不落库、不持有锁——
//! rebuild/增量更新完成后重建整个 [`SymbolGraph`] 并整体替换（`RwLock` 写侧瞬间切换）。

use std::collections::{HashMap, HashSet};

use super::store::{RefRow, SymbolMin};

/// 个性化 PageRank 缺省参数
const DAMPING: f64 = 0.85;
/// 迭代上限（万级文件量级单轮毫秒级，100 轮上限足够收敛）
const MAX_ITERATIONS: usize = 100;
/// 收敛阈值（L∞ 变化量）
const CONVERGENCE_EPS: f64 = 1e-9;

/// 符号引用图谱（不可变快照；重建即整体替换）
pub struct SymbolGraph {
    /// 文件 → PageRank 分数（归一化和为 1）
    ranks: HashMap<String, f64>,
    /// 文件 → 该文件定义的符号清单（检索候选）
    symbols_by_file: HashMap<String, Vec<SymbolMin>>,
    /// 边数（status 面就绪度展示）
    edge_count: usize,
}

impl SymbolGraph {
    /// 由 symbols/refs 两表全量构建图谱并运行个性化 PageRank
    ///
    /// `refs` 为文件级物化边（[`crate::index::store::IndexStore::refresh_refs`]
    /// 产物：to_file = 被引符号 owner 文件，自环已剔除，weight=Σ√count）。
    /// `seeds`：文件 → seed 权重（缺省 1.0）；会话打开文件建议注入 50.0。
    /// 空库/无边时退化为按 seed 均分——检索仍可用（按 seed 排序）。
    pub fn build(
        symbols: &[SymbolMin],
        refs: &[RefRow],
        seeds: &HashMap<String, f64>,
    ) -> SymbolGraph {
        let mut symbols_by_file: HashMap<String, Vec<SymbolMin>> = HashMap::new();
        for s in symbols {
            symbols_by_file
                .entry(s.file.clone())
                .or_default()
                .push(s.clone());
        }
        // 出边表（防御性再剔除自环、空权边与无符号目标；正常由 refresh_refs 保证）
        let mut weights: HashMap<(String, String), f64> = HashMap::new();
        for r in refs {
            if r.to_file == r.from_file || r.weight <= 0.0 {
                continue;
            }
            if !symbols_by_file.contains_key(&r.to_file) {
                continue; // to_file 无符号定义（owner 语义破坏，防御）
            }
            *weights
                .entry((r.from_file.clone(), r.to_file.clone()))
                .or_insert(0.0) += r.weight;
        }
        let mut out: HashMap<String, Vec<(String, f64)>> = HashMap::new();
        for ((from, to), weight) in weights {
            out.entry(from).or_default().push((to, weight));
        }
        let edge_count: usize = out.values().map(|v| v.len()).sum();
        // 节点集 = 有符号的文件 ∪ 有边的两端
        let mut nodes: HashSet<String> = symbols_by_file.keys().cloned().collect();
        for (from, targets) in &out {
            nodes.insert(from.clone());
            for (to, _) in targets {
                nodes.insert(to.clone());
            }
        }
        let ranks = personalize_pagerank(&nodes, &out, seeds);
        SymbolGraph {
            ranks,
            symbols_by_file,
            edge_count,
        }
    }

    /// 文件 PageRank 分数（未知文件为 0）
    pub fn rank_of(&self, file: &str) -> f64 {
        self.ranks.get(file).copied().unwrap_or(0.0)
    }

    /// 边数（status 面展示）
    pub fn edge_count(&self) -> usize {
        self.edge_count
    }

    /// 是否已构建（空图视为未就绪）
    pub fn is_empty(&self) -> bool {
        self.ranks.is_empty()
    }

    /// 符号名检索：子串匹配（大小写不敏感），按所在文件 PageRank 降序
    ///
    /// 返回 `(符号, 分数)`；分数 = 所在文件 PageRank（文件级重要性传递到符号）。
    pub fn match_symbols(&self, query: &str, limit: usize) -> Vec<(SymbolMin, f64)> {
        let q = query.trim().to_lowercase();
        if q.is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut hits: Vec<(SymbolMin, f64)> = Vec::new();
        for (file, syms) in &self.symbols_by_file {
            let rank = self.rank_of(file);
            for s in syms {
                if s.name.to_lowercase().contains(&q) {
                    hits.push((s.clone(), rank));
                }
            }
        }
        hits.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.name.cmp(&b.0.name))
                .then_with(|| a.0.file.cmp(&b.0.file))
                .then_with(|| a.0.line_start.cmp(&b.0.line_start))
        });
        hits.truncate(limit);
        hits
    }
}

/// 个性化 PageRank 主循环（seed 归一化为初始分布；悬挂节点质量按个性化向量重分配）
fn personalize_pagerank(
    nodes: &HashSet<String>,
    out: &HashMap<String, Vec<(String, f64)>>,
    seeds: &HashMap<String, f64>,
) -> HashMap<String, f64> {
    let n = nodes.len();
    if n == 0 {
        return HashMap::new();
    }
    let seed_of = |f: &str| seeds.get(f).copied().unwrap_or(1.0).max(0.0);
    let total_seed: f64 = nodes.iter().map(|f| seed_of(f)).sum();
    let total_seed = if total_seed > 0.0 { total_seed } else { 1.0 };
    let mut rank: HashMap<String, f64> = nodes
        .iter()
        .map(|f| (f.clone(), seed_of(f) / total_seed))
        .collect();
    // 悬挂节点集：不在出边表中的节点（其质量每轮按个性化向量重分配，
    // 保证 rank 总质量守恒——否则无边子图的质量会被凭空消灭）
    let dangling_set: Vec<&String> = nodes.iter().filter(|f| !out.contains_key(*f)).collect();
    for _ in 0..MAX_ITERATIONS {
        let mut dangling: f64 = dangling_set.iter().map(|f| rank[*f]).sum();
        let mut next: HashMap<String, f64> = nodes.iter().map(|f| (f.clone(), 0.0)).collect();
        for (from, targets) in out {
            let rf = rank[from];
            let sum: f64 = targets.iter().map(|(_, w)| w).sum();
            if sum <= 0.0 {
                dangling += rf; // 出边表内空权退化（防御分支）
                continue;
            }
            for (to, w) in targets {
                if let Some(v) = next.get_mut(to) {
                    *v += rf * w / sum;
                }
            }
        }
        let mut max_delta = 0.0f64;
        for (f, v) in next.iter_mut() {
            let p = seed_of(f) / total_seed;
            let nv = (1.0 - DAMPING) * p + DAMPING * (*v + dangling * p);
            max_delta = max_delta.max((nv - rank[f]).abs());
            *v = nv;
        }
        rank = next;
        if max_delta < CONVERGENCE_EPS {
            break;
        }
    }
    rank
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(file: &str, name: &str) -> SymbolMin {
        SymbolMin {
            file: file.into(),
            kind: "fn".into(),
            name: name.into(),
            line_start: 1,
            line_end: 1,
        }
    }

    fn refrow(from: &str, to: &str, weight: f64) -> RefRow {
        RefRow {
            from_file: from.into(),
            to_file: to.into(),
            weight,
        }
    }

    #[test]
    fn empty_inputs_yield_empty_graph() {
        let g = SymbolGraph::build(&[], &[], &HashMap::new());
        assert!(g.is_empty());
        assert_eq!(g.edge_count(), 0);
        assert!(g.match_symbols("anything", 10).is_empty());
    }

    #[test]
    fn self_edges_and_ghost_targets_are_dropped() {
        let symbols = vec![sym("a.rs", "alpha")];
        let refs = vec![
            refrow("a.rs", "a.rs", 9.0),     // 自环（store 层本应剔除，防御再剔）
            refrow("a.rs", "ghost.rs", 3.0), // to_file 无符号定义（owner 语义破坏）
        ];
        let g = SymbolGraph::build(&symbols, &refs, &HashMap::new());
        assert_eq!(g.edge_count(), 0);
        // 单节点图：rank 全部质量归 a.rs（悬挂质量守恒）
        assert!((g.rank_of("a.rs") - 1.0).abs() < 1e-6);
    }

    #[test]
    fn file_edges_flow_and_cited_file_outranks() {
        // b.rs → a.rs（owner 归属由 store refresh_refs 裁定，graph 直接消费文件边）
        let symbols = vec![sym("a.rs", "dup"), sym("b.rs", "dup")];
        let refs = vec![refrow("b.rs", "a.rs", 4.0)];
        let g = SymbolGraph::build(&symbols, &refs, &HashMap::new());
        assert_eq!(g.edge_count(), 1);
        assert!(g.rank_of("a.rs") > g.rank_of("b.rs"));
        // 总质量守恒（悬挂重分配正确性）
        let total: f64 = ["a.rs", "b.rs"].iter().map(|f| g.rank_of(f)).sum();
        assert!((total - 1.0).abs() < 1e-6);
    }

    #[test]
    fn seed_boost_lifts_rank() {
        let symbols = vec![sym("a.rs", "alpha"), sym("b.rs", "beta")];
        let refs = vec![];
        let mut seeds = HashMap::new();
        seeds.insert("b.rs".to_string(), 50.0);
        let plain = SymbolGraph::build(&symbols, &refs, &HashMap::new());
        let boosted = SymbolGraph::build(&symbols, &refs, &seeds);
        // 无边：均匀分布
        assert!((plain.rank_of("a.rs") - 0.5).abs() < 1e-6);
        // seed 加权后 b.rs 占优
        assert!(boosted.rank_of("b.rs") > boosted.rank_of("a.rs"));
        assert!((boosted.rank_of("a.rs") + boosted.rank_of("b.rs") - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cited_file_outranks_isolated_file() {
        // c.rs 引用 a.rs（alpha 的 owner）→ a.rs 获得 c.rs 的质量传递
        let symbols = vec![
            sym("a.rs", "alpha"),
            sym("b.rs", "beta"),
            sym("c.rs", "main"),
        ];
        let refs = vec![refrow("c.rs", "a.rs", 2.0)];
        let g = SymbolGraph::build(&symbols, &refs, &HashMap::new());
        assert_eq!(g.edge_count(), 1);
        assert!(g.rank_of("a.rs") > g.rank_of("b.rs"));
        assert!(g.rank_of("c.rs") > 0.0);
    }

    #[test]
    fn match_symbols_ranks_by_file_score_and_dedupes_order() {
        let symbols = vec![
            sym("hot.rs", "parse_config"),
            sym("cold.rs", "parse_args"),
            sym("cold.rs", "unrelated"),
        ];
        // hot.rs 被引用 → 分数更高
        let refs = vec![refrow("cold.rs", "hot.rs", 2.0)];
        let g = SymbolGraph::build(&symbols, &refs, &HashMap::new());
        let hits = g.match_symbols("parse", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0.name, "parse_config"); // hot.rs 分高在前
        assert!(hits[0].1 > hits[1].1);
        // 大小写不敏感 + limit 截断
        assert_eq!(g.match_symbols("PARSE", 10).len(), 2);
        assert_eq!(g.match_symbols("parse", 1).len(), 1);
        // 空查询不炸
        assert!(g.match_symbols("   ", 10).is_empty());
    }
}
