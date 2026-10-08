// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 回写通道上报（分类路由终态：能力缺口事件直发收件端点）
//!
//! ## 定位
//!
//! 原子粒重切预算耗尽（能力缺口）时，外层驱动向回写收件端点上报
//! `RuleFailureEvent`（schema 单源 evorule-rule `model::writeback`——事件类型
//! 白名单仅 `rule_failure`，`failure.type` 为自由字段，能力缺口分类以
//! `capability_gap` 标签入该字段）。执行侧无经会话转发器的通路（转发器只
//! 携带 Violation 族事实），故本模块**直连收件端点**。
//!
//! ## 旗标缺省关（fail-soft，先例对齐）
//!
//! `EVORULE_WRITEBACK_URL` 未配置/为空时**不发送**（零旁路开销）——与
//! evorule-server 回写转发器同款旗标纪律（同名 env，同部署两进程可共享配置）：
//!
//! - `EVORULE_WRITEBACK_URL`：回写服务基址（收件路径固定拼接
//!   `/v1/writeback/rule_failure`）；
//! - `EVORULE_WRITEBACK_KEY`：X-Api-Key（scope=writeback:rule_failure，收件侧
//!   自管认证）；
//! - `EVORULE_WRITEBACK_TENANT`：事件归属租户（缺省 `org-evorule`）；
//! - `EVORULE_WRITEBACK_DATASET`：事件归属数据集（缺省 `evo-agent-workflows`，
//!   与 server 会话转发来源区分——收件箱查询友好）。
//!
//! 发送为**尽力而为的观察面**（fail-soft）：网络/远端失败仅 warn 留痕，绝不
//! 影响驱动循环——审计责任在 marks 会话链（能力缺口信号入链为 fail-fast 硬
//! 义务，与本面正交）。

use serde_json::{json, Value};

/// 回写上报配置（env 解析产物；URL 空 = off）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritebackConfig {
    /// 收件服务基址（尾部 `/` 归一）
    pub base_url: String,
    /// X-Api-Key（scope=writeback:rule_failure）
    pub api_key: String,
    /// 事件归属租户
    pub tenant_id: String,
    /// 事件归属数据集
    pub dataset_id: String,
}

/// 解析配置（纯函数，便于单测）；URL 空/空白 = None（旗标关）
pub fn build_config(
    url: String,
    api_key: String,
    tenant: Option<String>,
    dataset: Option<String>,
) -> Option<WritebackConfig> {
    let url = url.trim().trim_end_matches('/').to_string();
    if url.is_empty() {
        return None;
    }
    Some(WritebackConfig {
        base_url: url,
        api_key,
        tenant_id: tenant.unwrap_or_else(|| "org-evorule".to_string()),
        dataset_id: dataset.unwrap_or_else(|| "evo-agent-workflows".to_string()),
    })
}

/// 从进程 env 读取配置（读取一次由调用方持有；URL 缺省 = 旗标关）
pub fn config_from_env() -> Option<WritebackConfig> {
    build_config(
        std::env::var("EVORULE_WRITEBACK_URL").unwrap_or_default(),
        std::env::var("EVORULE_WRITEBACK_KEY").unwrap_or_default(),
        std::env::var("EVORULE_WRITEBACK_TENANT").ok(),
        std::env::var("EVORULE_WRITEBACK_DATASET").ok(),
    )
}

/// 构造能力缺口回写事件（纯函数；形态对齐 RuleFailureEvent schema——
/// `failure.type` = `capability_gap` 即失败分类标签，切法空间描述入 detail，
/// 失败原文入 observed 供回溯）
pub fn capability_gap_event(
    cfg: &WritebackConfig,
    node_id: &str,
    agent_type: Option<&str>,
    plan_version: u32,
    recuts: u32,
    error_message: &str,
) -> Value {
    let detail = format!(
        "atomic granule '{node_id}' is unsolvable within the recut space: recut budget \
         ({recuts}) exhausted across alternative dimensions; capability gap reported for \
         capability-line review"
    );
    json!({
        "event_type": "rule_failure",
        "tenant_id": cfg.tenant_id,
        "dataset_id": cfg.dataset_id,
        "version_used": format!("plan_v{plan_version}"),
        "entry_id": format!("workflow_node:{node_id}"),
        "occurred_at": now_iso(),
        "execution_ctx": {
            "agent_type": agent_type,
        },
        "failure": {
            "type": "capability_gap",
            "detail": detail,
            "observed": error_message,
        },
    })
}

/// ISO-8601 UTC 墙钟（秒精度；时钟异常回退空串，不 panic——收件端 required
/// 字段空串可入库，先例同 server 转发器纪元回退口径）
fn now_iso() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

/// 上报能力缺口事件（fail-soft 尽力而为）：POST /v1/writeback/rule_failure，
/// 非 2xx / 传输错误 → Err 文本（调用方 warn 留痕，不影响驱动循环）。
pub async fn report_event(cfg: &WritebackConfig, event: &Value) -> Result<(), String> {
    let url = format!("{}/v1/writeback/rule_failure", cfg.base_url);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("writeback http client build failed: {e}"))?;
    let resp = client
        .post(&url)
        .header("X-Api-Key", &cfg.api_key)
        .json(event)
        .send()
        .await
        .map_err(|e| format!("writeback report failed: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("writeback endpoint returned non-2xx: {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> WritebackConfig {
        cfg_at("http://127.0.0.1:18081".to_string())
    }

    fn cfg_at(base_url: String) -> WritebackConfig {
        WritebackConfig {
            base_url,
            api_key: "k".to_string(),
            tenant_id: "org-evorule".to_string(),
            dataset_id: "evo-agent-workflows".to_string(),
        }
    }

    #[test]
    fn build_config_flag_off_and_defaults() {
        // 旗标缺省关：URL 空/空白 = None
        assert!(build_config(String::new(), String::new(), None, None).is_none());
        assert!(build_config("   ".into(), String::new(), None, None).is_none());
        // URL 归一（去尾斜杠）+ 缺省租户/数据集
        let c = build_config(
            "http://127.0.0.1:18081/".into(),
            "evorule_key".into(),
            None,
            None,
        )
        .expect("config on");
        assert_eq!(c.base_url, "http://127.0.0.1:18081");
        assert_eq!(c.tenant_id, "org-evorule");
        assert_eq!(c.dataset_id, "evo-agent-workflows");
        // 显式覆盖
        let c2 = build_config(
            "http://rule:18081".into(),
            "k".into(),
            Some("org-x".into()),
            Some("ds-y".into()),
        )
        .expect("config on");
        assert_eq!(c2.tenant_id, "org-x");
        assert_eq!(c2.dataset_id, "ds-y");
    }

    #[test]
    fn capability_gap_event_carries_class_label_and_schema_fields() {
        // RuleFailureEvent schema 关键字段齐全；class 标签入 failure.type
        let ev = capability_gap_event(
            &cfg(),
            "atomic_boom",
            Some("ghost_agent"),
            1,
            3,
            "workflow node 'atomic_boom' failed: boom",
        );
        assert_eq!(ev["event_type"], "rule_failure");
        assert_eq!(ev["tenant_id"], "org-evorule");
        assert_eq!(ev["dataset_id"], "evo-agent-workflows");
        assert_eq!(ev["version_used"], "plan_v1");
        assert_eq!(ev["entry_id"], "workflow_node:atomic_boom");
        assert_eq!(ev["failure"]["type"], "capability_gap");
        let detail = ev["failure"]["detail"].as_str().expect("detail present");
        assert!(
            detail.contains("unsolvable within the recut space"),
            "{detail}"
        );
        assert!(detail.contains("atomic_boom"), "{detail}");
        assert_eq!(
            ev["failure"]["observed"],
            "workflow node 'atomic_boom' failed: boom"
        );
        assert_eq!(ev["execution_ctx"]["agent_type"], "ghost_agent");
        // occurred_at 为 ISO-8601 Z 形态
        let ts = ev["occurred_at"].as_str().expect("occurred_at present");
        assert_eq!(ts.len(), 20, "{ts}");
        assert!(ts.ends_with('Z'), "{ts}");
        // 同输入同输出（occurred_at 秒精度内稳定——纯函数确定性以字段构造验证）
        let ev2 = capability_gap_event(
            &cfg(),
            "atomic_boom",
            Some("ghost_agent"),
            1,
            3,
            "workflow node 'atomic_boom' failed: boom",
        );
        assert_eq!(ev["failure"], ev2["failure"]);
        assert_eq!(ev["entry_id"], ev2["entry_id"]);
    }

    #[test]
    fn capability_gap_event_without_agent_type() {
        let ev = capability_gap_event(&cfg(), "a", None, 2, 1, "boom");
        assert!(ev["execution_ctx"]["agent_type"].is_null());
        assert_eq!(ev["version_used"], "plan_v2");
    }

    #[tokio::test]
    async fn report_event_posts_with_api_key_header() {
        let mut server = mockito::Server::new_async().await;
        let c = cfg_at(server.url());
        let ev = capability_gap_event(&c, "a", None, 1, 3, "boom");
        let m = server
            .mock("POST", "/v1/writeback/rule_failure")
            .match_header("X-Api-Key", "k")
            .match_body(mockito::Matcher::PartialJson(ev.clone()))
            .with_status(201)
            .with_body("{}")
            .create_async()
            .await;
        report_event(&c, &ev)
            .await
            .expect("successful receipt must yield Ok");
        m.assert_async().await;
    }

    #[tokio::test]
    async fn report_event_fails_soft_on_non_2xx() {
        // 收件端拒收（如认证失败）→ Err 文本，不 panic——调用方 warn 留痕
        let mut server = mockito::Server::new_async().await;
        let c = cfg_at(server.url());
        let ev = capability_gap_event(&c, "a", None, 1, 3, "boom");
        server
            .mock("POST", "/v1/writeback/rule_failure")
            .with_status(401)
            .with_body("{\"error\":\"bad key\"}")
            .create_async()
            .await;
        let err = report_event(&c, &ev).await.expect_err("non-2xx must err");
        assert!(err.contains("non-2xx"), "{err}");
    }

    #[tokio::test]
    async fn report_event_fails_soft_on_transport_error() {
        // 不可达端口 → Err（fail-soft：上报是观察面，审计责任在 marks 链）
        let c = WritebackConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: "k".to_string(),
            tenant_id: "t".to_string(),
            dataset_id: "d".to_string(),
        };
        let ev = capability_gap_event(&c, "a", None, 1, 3, "boom");
        assert!(report_event(&c, &ev).await.is_err());
    }
}
