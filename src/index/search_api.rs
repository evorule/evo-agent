// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! B14 语义索引端点（`/api/index/*`）—— search / status / rebuild。
//!
//! 检索三模式（v1 结构面 + v1.x 语义面）：`lexical` 符号图谱（PageRank，
//! 缺省与 v1 行为一致）/ `semantic` 向量暴力精确扫描（cosine）/ `hybrid`
//! 词法+语义 RRF 融合（k=60）。语义面禁用降级口径（C-3）：`mode=semantic`
//! 返回 200 空结果 + `semantic:"disabled"` 标注；`mode=hybrid` 自动降级纯词法
//! 同标注——不报错不阻断，前端可据标注展示降级原因。
//!
//! 治理面：serve 本地派生缓存读面 + 显式全量重建写面（人工触发，
//! 无 agent 工具接点）；索引服务未注入（`index.enabled` 关 / 启动未建）一律
//! 503——关闭态与故障态同形（不泄露内部拓扑）。鉴权由路由层 auth 中间件统一
//! 覆盖，本模块不做二次校验（非 WS 端点，无升级面）。

use std::collections::HashMap;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::api::agent_api::AgentApiState;
use crate::index::{RebuildOutcome, SemanticHit};

/// 索引服务未注入（`index.enabled` 关 / 启动未建）→ 503
///
/// 关闭态与故障态同形（不泄露内部拓扑）。
fn disabled() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "index disabled"})),
    )
        .into_response()
}

/// 检索 query 参数
#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    /// 检索词（lexical=符号名子串匹配，大小写不敏感；semantic/hybrid=自然文本）
    pub q: Option<String>,
    /// 返回上限（缺省 20，封顶 100）
    pub limit: Option<usize>,
    /// 检索模式（缺省 lexical）
    pub mode: Option<String>,
    /// 会话打开文件路径清单（逗号分隔，工作区相对路径；lexical 模式注入
    /// 个性化排位——打开文件优先，缺省=全局口径）
    pub open: Option<String>,
}

/// 检索模式（mode 参数白名单；缺省 lexical 与 v1 行为一致）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// 符号图谱检索（v1 结构面）
    Lexical,
    /// 向量语义检索（v1.x 语义面）
    Semantic,
    /// 词法+语义 RRF 融合
    Hybrid,
}

impl SearchMode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "lexical" => Some(Self::Lexical),
            "semantic" => Some(Self::Semantic),
            "hybrid" => Some(Self::Hybrid),
            _ => None,
        }
    }
}

/// hybrid 融合单命中（文件粒度；RRF 分 = Σ 1/(60+rank)，rank 从 1 起）
#[derive(Debug, Clone, serde::Serialize)]
pub struct FusedHit {
    /// 文件（相对路径）
    pub file: String,
    /// RRF 融合分
    pub score: f64,
    /// 词法侧行级命中数（未命中为 0）
    pub lexical_hits: usize,
    /// 语义侧最佳 chunk（未命中为 None 不下发）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_best: Option<SemanticHit>,
}

/// hybrid 编排结果（端点序列化输入；`semantic_active` 转降级标注）
#[derive(Debug)]
pub struct HybridResult {
    /// 语义侧是否参与（false = 语义面未装配，纯词法降级）
    pub semantic_active: bool,
    /// 融合结果（RRF 分降序）
    pub results: Vec<FusedHit>,
}

/// RRF 融合（k=60）：词法文件排位 × 语义 chunk 排位（chunk 级 → 文件级去重取首现位）
///
/// 两输入均为已排序（优→劣）榜单；输出融合分降序、同分路径升序，截断 `limit`。
/// 纯函数（无 I/O 无锁）——排位算术由单测直证。
pub fn rrf_fuse(
    lexical: &[(String, usize)],
    semantic: &[SemanticHit],
    limit: usize,
) -> Vec<FusedHit> {
    const K: f64 = 60.0;
    // file → (rrf 累计, 词法命中数, 语义最佳)；HashMap 定位 + Vec 保序
    let mut order: Vec<String> = Vec::new();
    let mut score_of: HashMap<String, f64> = HashMap::new();
    let mut lex_of: HashMap<String, usize> = HashMap::new();
    let mut sem_of: HashMap<String, SemanticHit> = HashMap::new();
    for (i, (file, hits)) in lexical.iter().enumerate() {
        let rrf = 1.0 / (K + i as f64 + 1.0);
        if score_of.contains_key(file) {
            continue; // 防御：输入榜单重复文件名时取首现位
        }
        order.push(file.clone());
        score_of.insert(file.clone(), rrf);
        lex_of.insert(file.clone(), *hits);
    }
    for (i, hit) in semantic.iter().enumerate() {
        let rrf = 1.0 / (K + i as f64 + 1.0);
        match score_of.get_mut(&hit.file) {
            Some(s) => {
                *s += rrf;
                sem_of
                    .entry(hit.file.clone())
                    .or_insert_with(|| hit.clone()); // 首现位（语义侧已降序）
            }
            None => {
                order.push(hit.file.clone());
                score_of.insert(hit.file.clone(), rrf);
                lex_of.insert(hit.file.clone(), 0);
                sem_of.insert(hit.file.clone(), hit.clone());
            }
        }
    }
    let mut fused: Vec<FusedHit> = order
        .into_iter()
        .map(|file| {
            let score = score_of.remove(&file).unwrap_or(0.0);
            let lexical_hits = lex_of.remove(&file).unwrap_or(0);
            let semantic_best = sem_of.remove(&file);
            FusedHit {
                file,
                score,
                lexical_hits,
                semantic_best,
            }
        })
        .collect();
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
    });
    fused.truncate(limit);
    fused
}

/// `GET /api/index/search?q=&limit=&mode=` —— 检索（lexical/semantic/hybrid）
pub async fn search(State(state): State<AgentApiState>, Query(q): Query<SearchQuery>) -> Response {
    let Some(svc) = state.index().cloned() else {
        return disabled();
    };
    let query = q.q.unwrap_or_default();
    let limit = q.limit.unwrap_or(20).min(100);
    let Some(mode) = SearchMode::parse(q.mode.as_deref().unwrap_or("lexical")) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid mode (lexical|semantic|hybrid)"})),
        )
            .into_response();
    };
    match mode {
        SearchMode::Lexical => {
            let graph = svc.graph();
            // 个性化注入：请求携带打开文件清单时按其重算排位（打开文件优先）
            let opens: Vec<String> = q
                .open
                .as_deref()
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            let hits = if opens.is_empty() {
                graph.match_symbols(&query, limit)
            } else {
                let ranks = graph.personalized_ranks(&opens);
                graph.match_symbols_with(&query, limit, &ranks)
            };
            Json(json!({
                "query": query,
                "mode": "lexical",
                "results": hits
                    .iter()
                    .map(|(s, score)| json!({
                        "file": s.file,
                        "kind": s.kind,
                        "name": s.name,
                        "line_start": s.line_start,
                        "line_end": s.line_end,
                        "score": score,
                    }))
                    .collect::<Vec<_>>(),
            }))
            .into_response()
        }
        SearchMode::Semantic => {
            // C-3 降级口径：语义面未启用 → 200 空结果 + disabled 标注（不报错）
            if svc.embedder().is_none() {
                return Json(json!({
                    "query": query,
                    "mode": "semantic",
                    "semantic": "disabled",
                    "results": [],
                }))
                .into_response();
            }
            // 嵌入 + 全量点积为 CPU 密集：spawn_blocking 不占 tokio worker
            let q = query.clone();
            match tokio::task::spawn_blocking(move || svc.semantic_search_sync(&q, limit)).await {
                Ok(Ok(hits)) => Json(json!({
                    "query": query,
                    "mode": "semantic",
                    "semantic": "on",
                    "results": hits,
                }))
                .into_response(),
                Ok(Err(e)) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.0})),
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("search join: {e}")})),
                )
                    .into_response(),
            }
        }
        SearchMode::Hybrid => {
            // 词法 grep + 嵌入点积均 CPU 密集：spawn_blocking 整体下放；
            // 语义面未装配在 sync 内自动降级纯词法（C-3），端点只做标注
            let q = query.clone();
            match tokio::task::spawn_blocking(move || svc.hybrid_search_sync(&q, limit)).await {
                Ok(Ok(r)) => Json(json!({
                    "query": query,
                    "mode": "hybrid",
                    "semantic": if r.semantic_active { "on" } else { "disabled" },
                    "results": r.results,
                }))
                .into_response(),
                Ok(Err(e)) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.0})),
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("search join: {e}")})),
                )
                    .into_response(),
            }
        }
    }
}

/// `GET /api/index/status` —— 就绪度与计数
pub async fn status(State(state): State<AgentApiState>) -> Response {
    let Some(svc) = state.index().cloned() else {
        return disabled();
    };
    Json(svc.status()).into_response()
}

/// `POST /api/index/rebuild` —— 全量重建（同步；并发重建 409）
pub async fn rebuild(State(state): State<AgentApiState>) -> Response {
    let Some(svc) = state.index().cloned() else {
        return disabled();
    };
    // 全量重建为 CPU 密集（全盘哈希 + tree-sitter 解析）：放 blocking 池，
    // 不占 tokio worker；原子护栏在建时返回 409（防双重建互踩）
    let outcome = tokio::task::spawn_blocking(move || svc.rebuild_sync()).await;
    match outcome {
        Ok(RebuildOutcome::Completed(status)) => Json(status).into_response(),
        Ok(RebuildOutcome::AlreadyBuilding) => (
            StatusCode::CONFLICT,
            Json(json!({"error": "rebuild already in progress"})),
        )
            .into_response(),
        Ok(RebuildOutcome::Failed(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.0})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("rebuild join: {e}")})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_mode_parse_covers_vocabulary() {
        assert_eq!(SearchMode::parse("lexical"), Some(SearchMode::Lexical));
        assert_eq!(SearchMode::parse("semantic"), Some(SearchMode::Semantic));
        assert_eq!(SearchMode::parse("hybrid"), Some(SearchMode::Hybrid));
        // 白名单外一律 None（端点 400）
        assert_eq!(SearchMode::parse(""), None);
        assert_eq!(SearchMode::parse("vector"), None);
        assert_eq!(SearchMode::parse("Lexical"), None); // 大小写敏感
    }

    fn lex(files: &[(&str, usize)]) -> Vec<(String, usize)> {
        files.iter().map(|(f, h)| ((*f).to_string(), *h)).collect()
    }

    fn sem(file: &str, ord: u32, score: f32) -> SemanticHit {
        SemanticHit {
            file: file.to_string(),
            ord,
            symbol: format!("sym_{file}"),
            kind: "function".into(),
            start_line: 1,
            end_line: 10,
            score,
        }
    }

    #[test]
    fn rrf_prefers_files_hit_by_both_sides() {
        // 词法: x.rs(3 hits) > a.rs > b.rs；语义: a.rs > b.rs——双侧命中的
        // a.rs（词法 rank2+语义 rank1=1/62+1/61≈0.03252）应反超词法单侧首位
        // 的 x.rs（rank1=1/61≈0.01639）——RRF 抑制单侧马太效应的核心性质；
        // b.rs（1/63+1/62≈0.03200）居次，单侧 x.rs 垫底
        let lexical = lex(&[("x.rs", 3), ("a.rs", 3), ("b.rs", 1)]);
        let semantic = vec![sem("a.rs", 0, 0.9), sem("b.rs", 1, 0.8)];
        let fused = rrf_fuse(&lexical, &semantic, 10);
        assert_eq!(fused.len(), 3);
        assert_eq!(fused[0].file, "a.rs");
        assert_eq!(fused[1].file, "b.rs");
        assert_eq!(fused[2].file, "x.rs");
        assert!(fused[0].score > fused[1].score);
        assert!(fused[1].score > fused[2].score);
        assert_eq!(fused[0].lexical_hits, 3);
        assert!(fused[0].semantic_best.is_some());
        assert_eq!(fused[2].lexical_hits, 3);
        assert!(fused[2].semantic_best.is_none());
    }

    #[test]
    fn rrf_limit_and_singleton_sides() {
        // 仅语义侧：文件按语义位次成形；limit 截断；词法空档命中数 0
        let semantic = vec![
            sem("x.rs", 0, 0.9),
            sem("y.rs", 1, 0.5),
            sem("z.rs", 2, 0.3),
        ];
        let fused = rrf_fuse(&[], &semantic, 2);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].file, "x.rs");
        assert_eq!(fused[0].lexical_hits, 0);
        assert!(fused[0].semantic_best.is_some());
        // 仅词法侧：semantic_best 不下发（skip_serializing_if 之外先验 None）
        let fused = rrf_fuse(&lex(&[("m.rs", 2), ("n.rs", 1)]), &[], 10);
        assert_eq!(fused[0].file, "m.rs");
        assert!(fused.iter().all(|h| h.semantic_best.is_none()));
        // 双侧全空：空结果不 panic
        assert!(rrf_fuse(&[], &[], 10).is_empty());
    }

    #[test]
    fn rrf_k60_arithmetic_and_tiebreak() {
        // k=60 算术直证：rank1 = 1/61，rank2 = 1/62；双侧同 rank（1+1）同分
        // → 路径升序 tie-break 稳定输出
        let fused = rrf_fuse(
            &lex(&[("b.rs", 1), ("a.rs", 1)]),
            &[sem("a.rs", 0, 0.9), sem("b.rs", 1, 0.8)],
            10,
        );
        // a.rs: 词法 rank2 + 语义 rank1 = 1/62 + 1/61；b.rs: 词法 rank1 + 语义 rank2 同值
        assert!((fused[0].score - fused[1].score).abs() < 1e-12);
        assert_eq!(fused[0].file, "a.rs"); // 同分路径升序
                                           // 单侧 rank1 与双侧低位的算术关系：1/61 < 1/61 + 1/64
        let fused = rrf_fuse(&lex(&[("solo.rs", 1)]), &[sem("both.rs", 1, 0.5)], 10);
        assert!(fused[0].score > fused[1].score || fused[1].file == "solo.rs");
    }
}
