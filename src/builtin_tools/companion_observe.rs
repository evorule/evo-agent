//! 伴跑只读观测工具族：LLM 经此主动查询伴跑状态机呈现面（状态/提案清单/
//! 覆盖率三只读 GET），替代"启动期看一眼的静态事实行"。
//!
//! 铁律边界：**只读零写路径**——干预面（A/B 切换、提案审批/驳回、上链重试）
//! 不在本族且永不注册（干预权留管理面，人在环不可破）。server 不可达/未
//! 装配（503）/旧版无路由（404）时调用期 fail-visible 如实报错，不猜不降级。

use crate::api::evorule_client::EvoruleApiClient;
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;
use serde_json::{json, Value};
use std::sync::Arc;

/// 工具名（LLM 面稳定标识）
pub const COMPANION_STATUS_TOOL: &str = "companion_status";
/// 提案清单只读观测工具名
pub const COMPANION_PROPOSALS_TOOL: &str = "companion_proposals";
/// 覆盖率只读观测工具名
pub const COMPANION_COVERAGE_TOOL: &str = "companion_coverage";

/// 伴跑只读观测工具（持 client 装配期注入；三变体共用一执行体）
#[derive(Clone)]
pub struct CompanionObserveTool {
    ev: Arc<EvoruleApiClient>,
    kind: &'static str,
}

impl CompanionObserveTool {
    /// 伴跑状态机+均衡态快照（含 equilibrium 块：窗快照/差分速率/stable/last_cause）
    pub fn status(ev: Arc<EvoruleApiClient>) -> Self {
        Self { ev, kind: "status" }
    }

    /// 伴跑提案清单（积压/裁决状态只读盘点；不含审批/驳回动作）
    pub fn proposals(ev: Arc<EvoruleApiClient>) -> Self {
        Self {
            ev,
            kind: "proposals",
        }
    }

    /// 规则集覆盖率快照（伴跑分析对定律库的覆盖读数）
    pub fn coverage(ev: Arc<EvoruleApiClient>) -> Self {
        Self {
            ev,
            kind: "coverage",
        }
    }

    fn endpoint(&self) -> &'static str {
        match self.kind {
            "proposals" => "/api/companion/proposals",
            "coverage" => "/api/companion/coverage",
            _ => "/api/companion/status",
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for CompanionObserveTool {
    async fn call(&self, _args: &Value) -> IoResult {
        let result = match self.kind {
            "proposals" => self.ev.get_companion_proposals().await,
            "coverage" => self.ev.get_companion_coverage().await,
            _ => self.ev.get_companion_status().await,
        };
        match result {
            Ok(v) => Ok(v),
            Err(e) => Ok(json!({
                "status": "error",
                "error": format!("伴跑观测面不可达（{}）: {e}", self.endpoint()),
            })),
        }
    }
}

/// 三工具注册描述（中性技术措辞；装配点经 dynamic_manifest 产出契约）
pub fn tool_description(kind: &str) -> String {
    match kind {
        "proposals" => {
            "查询伴跑机当前提案清单（只读）：返回积压提案及其裁决状态。\
             用于盘点自动提案池、核对提案去向。"
        }
        "coverage" => {
            "查询规则集覆盖率快照（只读）：返回伴跑分析对定律库的覆盖读数。\
             用于评估均衡判据的覆盖供给。"
        }
        _ => {
            "查询伴跑状态机快照（只读）：返回运行状态、证书版本链与均衡观\
             测块（窗口计数、进出速率、稳定判定、未稳归因）。用于回答「伴\
             跑现在什么状态/是否处于稳态」。"
        }
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_nonempty_and_neutral() {
        for kind in ["status", "proposals", "coverage"] {
            let d = tool_description(kind);
            assert!(!d.trim().is_empty());
            // 公开面纪律：禁内部登记号形态（字母前缀-数字）
            assert!(
                !d.contains("B-1"),
                "description must not carry internal ids: {d}"
            );
        }
    }

    #[test]
    fn endpoint_mapping_covers_three_kinds() {
        let mk = |k: &'static str| CompanionObserveTool {
            ev: Arc::new(EvoruleApiClient::new("http://127.0.0.1:1")),
            kind: k,
        };
        assert_eq!(mk("status").endpoint(), "/api/companion/status");
        assert_eq!(mk("proposals").endpoint(), "/api/companion/proposals");
        assert_eq!(mk("coverage").endpoint(), "/api/companion/coverage");
    }

    #[tokio::test]
    async fn unreachable_server_fails_visible() {
        // 端口 1 无监听：调用期 fail-visible（error 形态，不 panic 不猜）
        let t = CompanionObserveTool::status(Arc::new(EvoruleApiClient::new("http://127.0.0.1:1")));
        let out = t.call(&json!({})).await;
        let body = out.expect("call must return Ok(json payload)");
        let s = body.to_string();
        assert!(s.contains("error"), "must fail-visible: {s}");
        assert!(
            s.contains("/api/companion/status"),
            "must name endpoint: {s}"
        );
    }
}
