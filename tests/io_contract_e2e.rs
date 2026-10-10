//! E-8 两仓 E2E：evo-agent 协商握手 × evorule-server 契约端点（34 号档 B3）
//!
//! **测试对象**：B2 的协商链路（EvoruleApiClient::fetch_io_contract →
//! negotiate_io_contract）的三个场景端到端实证：
//!
//! 1. **live 契约拉取+协商通过**：真 evorule-server（io-contract v1 端点，
//!    B1 已落）返回 v1 → 协商 Ok，且契约体与仓内 pinned 副本形状一致。
//! 2. **版本不匹配 hard fail**：mock server（axum，仓内既有惯例）返回 v2
//!    结构 → `VersionMismatch` ——fail-closed 拒启的端到端实证。
//! 3. **404 → warn 通过**：mock 返回 404（旧 server 无端点）→
//!    `LegacyServer` ——两仓独立演进的部署现实不打断。
//!
//! **跑法**：`cargo test --test io_contract_e2e`。live 场景默认探
//! `EVORULE_TEST_SERVER`（默认 http://127.0.0.1:18080），未起则跳过提示。

use evo_agent::api::evorule_client::EvoruleApiClient;
use evo_agent::api::io_contract::{
    negotiate_io_contract, IoContract, IoContractError, IoContractNegotiation,
    NegotiationFetchError,
};
use serde_json::Value;

/// 仓内 pinned 副本（基准面）
fn pinned() -> IoContract {
    evo_agent::api::io_contract::pinned_contract().expect("pinned contract parses")
}

/// 契约体 v1 全字段比对（不比 semantics 描述字段——两端各自维护文案，
/// 结构对齐即可；semantics 变更走契约版本演进）。
fn assert_same_v1_shape(a: &IoContract, b: &IoContract) {
    assert_eq!(a.contract_version, b.contract_version, "contract_version");
    assert_eq!(a.shapes.len(), b.shapes.len(), "shapes count");
    for (sa, sb) in a.shapes.iter().zip(b.shapes.iter()) {
        assert_eq!(sa.shape, sb.shape, "shape name");
        assert_eq!(sa.io_type, sb.io_type, "shape io_type");
        assert_eq!(sa.required_keys, sb.required_keys, "required_keys");
        assert_eq!(sa.forbidden_keys, sb.forbidden_keys, "forbidden_keys");
    }
}

/// 起 axum mock server（返回给定状态码+JSON body），返回 base url。
async fn spawn_mock(status: u16, body: Value) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/api/io-contract",
        axum::routing::get(move || async move {
            axum::response::IntoResponse::into_response((
                axum::http::StatusCode::from_u16(status).unwrap(),
                axum::Json(body.clone()),
            ))
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

// ---------------------------------------------------------------------------
// 场景 2/3：axum mock（不依赖真 server）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_e2e_version_mismatch_hard_fail_on_mock_v2() {
    let mut v2_body = serde_json::to_value(pinned()).unwrap();
    v2_body["contract_version"] = serde_json::json!(2);
    let base = spawn_mock(200, v2_body).await;
    let client = EvoruleApiClient::new(&base);
    let err = negotiate_io_contract(move || {
        let c = client.clone();
        Box::pin(async move { c.fetch_io_contract().await })
    })
    .await
    .unwrap_err();
    match err {
        IoContractError::VersionMismatch { server_version, .. } => {
            assert_eq!(server_version, 2, "must report server v2");
        }
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn test_e2e_404_legacy_server_warn_pass() {
    // axum 未注册路由 → 404，正好模拟旧 server 无端点
    let base = spawn_mock(404, serde_json::json!({})).await;
    let client = EvoruleApiClient::new(&base);
    let r = negotiate_io_contract(move || {
        let c = client.clone();
        Box::pin(async move { c.fetch_io_contract().await })
    })
    .await
    .unwrap();
    assert!(matches!(r, IoContractNegotiation::LegacyServer));
}

#[tokio::test]
async fn test_e2e_unreachable_warn_pass() {
    // 端口 1（保留端口，几乎必然连接拒绝）——传输错 → Unavailable → warn 通过
    let client = EvoruleApiClient::new("http://127.0.0.1:1");
    let r = negotiate_io_contract(move || {
        let c = client.clone();
        Box::pin(async move { c.fetch_io_contract().await })
    })
    .await
    .unwrap();
    assert!(matches!(r, IoContractNegotiation::LegacyServer));
}

// ---------------------------------------------------------------------------
// 场景 1：live server（未起则跳过，输出提示）
// ---------------------------------------------------------------------------

fn live_server_base() -> String {
    std::env::var("EVORULE_TEST_SERVER")
        .ok()
                .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:18080".to_string())
}

#[tokio::test]
async fn test_e2e_live_server_contract_negotiation_ok() {
    let base = live_server_base();
    let client = EvoruleApiClient::new(&base);
    match client.fetch_io_contract().await {
        Ok(v) => {
            // 端点在：协商必须过且与 pinned 形状一致
            let contract: IoContract =
                serde_json::from_value(v).expect("live contract parses");
            assert_same_v1_shape(&pinned(), &contract);
        }
        Err(NegotiationFetchError::NotFound) => {
            // 旧 server（无端点）——不视为失败：当前生产部署兼容面
            eprintln!("note: server at {base} has no /api/io-contract (legacy)");
        }
        Err(NegotiationFetchError::Unavailable(d)) => {
            // 未起 server：跳过提示；严格模式可 fail
            eprintln!("skip: no live server at {base} ({d})");
        }
    }
}
