// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! B14 语义索引端点（`/api/index/*`，v1 结构面）—— search / status / rebuild。
//!
//! 治理面（15 号 §四）：serve 本地派生缓存读面 + 显式全量重建写面（人工触发，
//! 无 agent 工具接点）；索引服务未注入（`index.enabled` 关 / 启动未建）一律
//! 503——关闭态与故障态同形（不泄露内部拓扑）。鉴权由路由层 auth 中间件统一
//! 覆盖，本模块不做二次校验（非 WS 端点，无升级面）。

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::api::agent_api::AgentApiState;
use crate::index::RebuildOutcome;

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
    /// 检索词（符号名子串匹配，大小写不敏感）
    pub q: Option<String>,
    /// 返回上限（缺省 20，封顶 100）
    pub limit: Option<usize>,
}

/// `GET /api/index/search?q=&limit=` —— 符号名检索（PageRank 排序）
pub async fn search(State(state): State<AgentApiState>, Query(q): Query<SearchQuery>) -> Response {
    let Some(svc) = state.index().cloned() else {
        return disabled();
    };
    let query = q.q.unwrap_or_default();
    let limit = q.limit.unwrap_or(20).min(100);
    let graph = svc.graph();
    let hits = graph.match_symbols(&query, limit);
    Json(json!({
        "query": query,
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
