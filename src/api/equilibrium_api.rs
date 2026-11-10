// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 均衡观测代理端点:工作台均衡仪表面数据源。
//!
//! - `GET /api/equilibrium/status`:serve 侧只读透传 evorule-server 伴跑
//!   status 的 `equilibrium` 观测块(窗快照/差分速率/三层相位/Φ 位势/警量
//!   分账),前端 StatusBar 相位点与 popover 的唯一数据源。
//!
//! 观测面最小暴露:只透传 `equilibrium` 块——伴跑管理面字段(proposals/
//! uplinks/chain/counters/capacity 等)在代理层剥离,观测数据与治理面数据
//! 分离;干预权留管理面,本端点零写零扰动(纯 GET 读态,不触发 server 侧
//! 任何状态机推进)。
//!
//! 鉴权:auth 中间件单点执法(Authorization Bearer 或 `?ticket=` 一次性短票;
//! `?token=` 长期凭据恒 401),handler 层不再二次校验。
//! 降级口径:evorule-server 不可达/未装配/旧版无 equilibrium 块一律
//! 503+结构化错误,前端据此灰置(灰空心点,禁止显示任何相位色,防假绿)。

#![forbid(unsafe_code)]

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::api::agent_api::AgentApiState;
use crate::api::api_core::ApiError;

/// 从伴跑 status 全量 JSON 中提取 `equilibrium` 观测块。
///
/// 旧版 evorule-server 无该块(观测面晚于伴跑面落地)返回 None——契约缺失
/// 属降级态而非空观测,调用方按 503 灰置处理,不得构造空块伪装可用。
pub(crate) fn extract_equilibrium(status_json: &Value) -> Option<Value> {
    status_json.get("equilibrium").cloned()
}

/// 组装 200 响应体:观测时间戳(ms)+equilibrium 块原样透传。
///
/// 块内容不做字段级改写(server 是观测面权威,代理层只加 `observed_at`
/// 一键),前端契约随 server 演进(只加不改新键前端自然可见)。
pub(crate) fn render_status(observed_at_ms: u64, equilibrium: Value) -> Value {
    json!({
        "observed_at": observed_at_ms,
        "equilibrium": equilibrium,
    })
}

/// 把 EvoruleApiClient 的错误映射为代理降级响应(恒 503,结构化)。
///
/// - server 明确 503(伴跑中枢未装配)/404(旧版无路由)与传输层失败
///   (停机/端口不通)统一灰置——前端语义只有「观测不可用」一种;
/// - 错误详情透出 `detail` 供 tooltip,不吞不猜。
pub(crate) fn map_client_error(err: &ApiError) -> Value {
    let detail = match err {
        ApiError::ApiError { status, message } => {
            format!("upstream status {status}: {message}")
        }
        other => other.to_string(),
    };
    json!({
        "error": "equilibrium observation unavailable",
        "server_reachable": false,
        "detail": detail,
    })
}

/// `GET /api/equilibrium/status` — 均衡观测只读代理。
pub async fn status(State(state): State<AgentApiState>) -> Response {
    let observed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    match state.evorule_client().get_companion_status().await {
        Ok(v) => match extract_equilibrium(&v) {
            Some(eq) => (StatusCode::OK, Json(render_status(observed_at, eq))).into_response(),
            None => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": "equilibrium observation unavailable",
                    "server_reachable": true,
                    "detail": "upstream status carries no equilibrium block (legacy server)",
                })),
            )
                .into_response(),
        },
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(map_client_error(&err)),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 透传映射:正常 status 中的 equilibrium 块原样透出+observed_at 加盖。
    #[test]
    fn extract_and_render_passthrough() {
        let status = json!({
            "enabled": true,
            "state": "idle",
            "proposals": [{"secret": "governance-only"}],
            "chain": {"seq": 7},
            "equilibrium": {
                "phi_total": 3,
                "system_phase": "relaxing",
                "system_balanced": false,
                "layer_phases": {"capability": "relaxing", "data": "absorbing", "rules": "relaxing"},
                "alert_attribution": {"capability": 1, "domains": {"thermal": 2}, "total": 3},
            },
        });
        let eq = extract_equilibrium(&status).expect("block must exist");
        // 管理面字段不在块内:提取即完成最小暴露
        assert!(eq.get("system_phase").is_some());
        assert!(eq.get("proposals").is_none());
        assert!(eq.get("chain").is_none());
        let rendered = render_status(1234, eq);
        assert_eq!(rendered["observed_at"], 1234);
        assert_eq!(rendered["equilibrium"]["system_phase"], "relaxing");
        assert_eq!(rendered["equilibrium"]["alert_attribution"]["total"], 3);
        // 顶层管理面字段不进响应
        assert!(rendered.get("proposals").is_none());
        assert!(rendered.get("chain").is_none());
    }

    /// 旧版缺块:无 equilibrium 键返回 None(降级依据),不构造空块。
    #[test]
    fn legacy_status_without_block_is_none() {
        let legacy = json!({"enabled": true, "state": "idle", "counters": {}});
        assert!(extract_equilibrium(&legacy).is_none());
        let empty = json!({});
        assert!(extract_equilibrium(&empty).is_none());
    }

    /// 错误映射:任何上游错误统一 503 语义的结构化体(可达性 false+详情)。
    #[test]
    fn client_errors_map_to_structured_unavailable() {
        let uninstalled = ApiError::ApiError {
            status: 503,
            message: "companion hub 未装配".into(),
        };
        let legacy_route = ApiError::ApiError {
            status: 404,
            message: "no route".into(),
        };
        let transport = ApiError::InvalidResponse;
        for err in [&uninstalled, &legacy_route, &transport] {
            let mapped = map_client_error(err);
            assert_eq!(mapped["error"], "equilibrium observation unavailable");
            assert_eq!(mapped["server_reachable"], false);
            assert!(mapped["detail"].as_str().is_some_and(|s| !s.is_empty()));
        }
        // 详情携带上游状态码,便于排障
        let mapped = map_client_error(&uninstalled);
        let detail = mapped["detail"].as_str().unwrap();
        assert!(
            detail.contains("503"),
            "detail must carry upstream status: {detail}"
        );
    }
}
