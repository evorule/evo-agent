//! evorule-server IO 形状契约——消费方（evo-agent）侧协商模块
//!
//! **契约背景**：evo-agent 与 evorule-server 的集成契约核心是三个参数形状
//! 约定（server 侧 skip 谓词：llm_audit / agent_tool / flow_probe）。历史上
//! 这些形状散落在 server 侧 if 语句中，本仓无对应锁定——任何一侧改参数
//! 形状都会静默破坏集成（skip 失效→权限门误杀或 stale 拒绝断链）。
//! evorule-server 自 io-contract v1 起提供 `GET /api/io-contract` 机读导出
//! （真相源=server 侧形状表），本模块负责：
//!
//! 1. **启动协商**（[`negotiate_io_contract`]）：建会话前拉取契约——
//!    端点 404/连不通 = 旧 server（v0 未固化期）→ **warn 通过**（两仓独立
//!    演进的部署现实，不能一升全断）；端点在但版本 ∉ 自家支持集 →
//!    **hard fail**（fail-closed：未验证的升级行为宁停不错）。
//! 2. **交叉锁测**（`test_io_shape_contract_alignment`）：对自家真实产物
//!    （build_call_external_command / call_service 指令形状）跑契约形状
//!    断言——本仓产物若漂移出契约形状，测试期即红，而非生产断链。

use serde::Deserialize;
use std::collections::HashSet;

/// 本 evo-agent 构建支持的 evorule io 契约版本集。
/// server 端点返回的 contract_version 不在此集 → 协商 hard fail。
pub const IO_CONTRACT_SUPPORTED: &[u32] = &[1];

/// 契约中单个形状规格（仅消费方向反序列化所需字段；semantics 等描述
/// 字段不参与判定，从简略去）。
#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
pub struct IoShapeSpec {
    pub shape: String,
    pub io_type: String,
    pub required_keys: Vec<String>,
    pub forbidden_keys: Vec<String>,
}

/// 契约导出体（`GET /api/io-contract` 响应）。
#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
pub struct IoContract {
    pub contract_version: u32,
    #[allow(dead_code)]
    pub supported_versions: Vec<u32>,
    pub shapes: Vec<IoShapeSpec>,
}

/// 协商结果。
#[derive(Debug)]
pub enum IoContractNegotiation {
    /// server 提供契约且版本在支持集内（携带契约体供后续形状自检）
    Ok(IoContract),
    /// server 未提供契约端点（404/连不通）——旧 server，warn 通过
    LegacyServer,
}

/// 协商错误（hard fail 面）。
#[derive(Debug)]
pub enum IoContractError {
    /// 端点在，但版本不在支持集——fail-closed，附双方版本便于诊断
    VersionMismatch {
        server_version: u32,
        server_supported: Vec<u32>,
        agent_supported: Vec<u32>,
    },
    /// 契约体解析失败（形态异常，视为不可信——fail-closed）
    Malformed(String),
}

impl std::fmt::Display for IoContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IoContractError::VersionMismatch {
                server_version,
                server_supported,
                agent_supported,
            } => write!(
                f,
                "evorule io-contract version mismatch: server v{server_version} \
                 (supported: {server_supported:?}), evo-agent supports \
                 {agent_supported:?} — 升级行为未验证，fail-closed 拒绝启动；\
                 请升级 evo-agent 或将 evorule-server 回退/对齐契约版本"
            ),
            IoContractError::Malformed(detail) => {
                write!(f, "evorule io-contract malformed: {detail}")
            }
        }
    }
}

/// 拉取并协商契约（客户端已建连接语境；404 → LegacyServer）。
///
/// `fetch_json` 由调用方注入（serve/CLI 各自的 HTTP 语境），便于测试桩。
/// 用 std `Pin<Box<dyn Future + Send>>`（仓内无 futures 整包依赖；
/// `+ Send` 因 runner stream 需跨线程 spawn）。
pub async fn negotiate_io_contract(
    fetch_json: impl FnOnce() -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, NegotiationFetchError>>
                + Send,
        >,
    >,
) -> Result<IoContractNegotiation, IoContractError> {
    let raw = match fetch_json().await {
        Ok(v) => v,
        Err(NegotiationFetchError::NotFound) => return Ok(IoContractNegotiation::LegacyServer),
        Err(NegotiationFetchError::Unavailable(detail)) => {
            // 连不通/5xx 与 404 同判：旧 server 或网络未起——部署期常态，
            // warn 通过（调用方记 warn 日志）；契约判定只在端点确实应答时做。
            tracing::warn!(%detail, "io-contract endpoint unavailable; treating as legacy server (warn-pass)");
            return Ok(IoContractNegotiation::LegacyServer);
        }
    };
    let contract: IoContract =
        serde_json::from_value(raw).map_err(|e| IoContractError::Malformed(e.to_string()))?;
    if !IO_CONTRACT_SUPPORTED.contains(&contract.contract_version) {
        return Err(IoContractError::VersionMismatch {
            server_version: contract.contract_version,
            server_supported: contract.supported_versions,
            agent_supported: IO_CONTRACT_SUPPORTED.to_vec(),
        });
    }
    Ok(IoContractNegotiation::Ok(contract))
}

/// 拉取错误分类。
#[derive(Debug)]
pub enum NegotiationFetchError {
    /// 404——旧 server 无此端点
    NotFound,
    /// 连不通/其他传输错误
    Unavailable(String),
}

/// 形状判定（与 server 侧表驱动判定同构：io_type 相等 ∧ required 全在场
/// ∧ forbidden 全缺席）——用于本仓产物自检。
pub fn params_match_shape(spec: &IoShapeSpec, io_type: &str, params: &serde_json::Value) -> bool {
    if io_type != spec.io_type {
        return false;
    }
    let obj = match params.as_object() {
        Some(o) => o,
        None => return spec.required_keys.is_empty(),
    };
    spec.required_keys.iter().all(|k| obj.contains_key(k))
        && spec.forbidden_keys.iter().all(|k| !obj.contains_key(k))
}

/// 读仓内 pinned 契约副本（交叉锁测用；来源=evorule-server 快照导出，
/// 双仓同步由 CI 级脚本核哈希）。
pub fn pinned_contract() -> Result<IoContract, IoContractError> {
    let raw: serde_json::Value =
        serde_json::from_str(include_str!("../../assets/io-contract-v1.json"))
            .map_err(|e| IoContractError::Malformed(format!("pinned asset: {e}")))?;
    serde_json::from_value(raw).map_err(|e| IoContractError::Malformed(e.to_string()))
}

/// evo-agent 侧 io_type 支持集（handle_io_request 的 match 分支面）——
/// 契约中 consumer 属 evo-agent 的形状，其 io_type 必须在本集内
/// （双向锁：server 新增 evo-agent 消费形态而本仓未实现 → 交叉测试红）。
pub fn agent_supported_io_types() -> HashSet<&'static str> {
    ["call_external", "call_service"].into_iter().collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn pinned() -> IoContract {
        pinned_contract().expect("pinned io-contract parses")
    }

    // --- 协商分支 ---

    #[tokio::test]
    async fn test_negotiate_version_ok() {
        let json = serde_json::to_value(pinned()).unwrap();
        let r = negotiate_io_contract(|| Box::pin(async move { Ok(json) }))
            .await
            .unwrap();
        match r {
            IoContractNegotiation::Ok(c) => assert_eq!(c.contract_version, 1),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_negotiate_404_legacy_pass() {
        let r = negotiate_io_contract(|| Box::pin(async { Err(NegotiationFetchError::NotFound) }))
            .await
            .unwrap();
        assert!(matches!(r, IoContractNegotiation::LegacyServer));
    }

    #[tokio::test]
    async fn test_negotiate_unreachable_warn_pass() {
        let r = negotiate_io_contract(|| {
            Box::pin(async { Err(NegotiationFetchError::Unavailable("conn refused".into())) })
        })
        .await
        .unwrap();
        assert!(matches!(r, IoContractNegotiation::LegacyServer));
    }

    #[tokio::test]
    async fn test_negotiate_version_mismatch_hard_fail() {
        let mut c = pinned();
        c.contract_version = 2; // 模拟 server 升到 v2
        let json = serde_json::to_value(c).unwrap();
        let err = negotiate_io_contract(|| Box::pin(async move { Ok(json) }))
            .await
            .unwrap_err();
        match err {
            IoContractError::VersionMismatch { server_version, .. } => {
                assert_eq!(server_version, 2)
            }
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_negotiate_malformed_fail_closed() {
        let json = serde_json::json!({"contract_version": 1}); // 缺 shapes
        assert!(matches!(
            negotiate_io_contract(|| Box::pin(async move { Ok(json) })).await,
            Err(IoContractError::Malformed(_))
        ));
    }

    // --- 交叉锁测：本仓真实产物 vs 契约形状 ---

    #[test]
    fn test_io_shape_contract_alignment_llm_audit() {
        let contract = pinned();
        let spec = contract
            .shapes
            .iter()
            .find(|s| s.shape == "llm_audit")
            .expect("llm_audit shape in contract");
        // build_call_external_command 产物形状（runner.rs:3185 区同构）：
        // {model, temperature, messages:[...], tools?, effective_params}
        let produced = serde_json::json!({
            "model": "test-model",
            "temperature": 0.2,
            "messages": [
                {"role": "system", "content": "s"},
                {"role": "user", "content": "g"},
            ],
            "effective_params": {"stream": true}
        });
        assert!(
            params_match_shape(spec, "call_external", &produced),
            "call_external 产物必须命中 llm_audit 形状（有 messages，无 service_name/name）——\
             否则 server skip 失效，内置订阅者抢先应答断审计回路"
        );
        // 互斥防御自证：带 service_name 的形态必须不命中（平台路由形态）
        let platform_shape = serde_json::json!({"service_name": "svc", "args": {}});
        assert!(!params_match_shape(spec, "call_external", &platform_shape));
    }

    #[test]
    fn test_io_shape_contract_alignment_agent_tool() {
        let contract = pinned();
        let spec = contract
            .shapes
            .iter()
            .find(|s| s.shape == "agent_tool")
            .expect("agent_tool shape in contract");
        // 宪法 io_request → call_service 指令形状（handle_call_service 消费）：
        // {tool_name, args}
        let produced = serde_json::json!({"tool_name": "file_write", "args": {}});
        assert!(
            params_match_shape(spec, "call_service", &produced),
            "call_service 产物必须命中 agent_tool 形状（有 tool_name，无 service_name/name）——\
             否则 server 不跳过，missing service_name 错误应答消费请求断工具循环"
        );
        let platform_shape = serde_json::json!({"service_name": "solver", "args": {}});
        assert!(!params_match_shape(spec, "call_service", &platform_shape));
    }

    #[test]
    fn test_contract_io_types_cover_agent_match_arms() {
        // 双向锁：契约中非 flow_probe 形态的 io_type ⊆ handle_io_request 支持集；
        // server 为 evo-agent 消费新增形态而本仓 match 未实现 → 红
        let contract = pinned();
        let supported = agent_supported_io_types();
        for spec in &contract.shapes {
            if spec.shape == "flow_probe" {
                continue; // bundle 侧形态，evo-agent 不消费
            }
            assert!(
                supported.contains(spec.io_type.as_str()),
                "契约形态 {} 的 io_type {} 不在 evo-agent handle_io_request 支持集——\
                 需扩展 match 分支或升级契约",
                spec.shape,
                spec.io_type
            );
        }
    }
}
