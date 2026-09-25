// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! O-120:裁决通道(AdjudicationChannel)—— file 类工具意图的独立 evorule 会话裁决
//!
//! ## 背景(O-120 本体缺陷)
//! 主会话 ReAct 循环中 call_external 在途时,引擎命令串行评估语义使后续
//! 命令(含意图 set)仅在 IoResponse 后才被评估——主会话内「提交 + 1s
//! 轮询 version」的裁决原语恒超时,合法相对路径操作被假拦。引擎侧每会话
//! 独立反应器(governance session.rs),独立会话裁决不受主会话 io 在途
//! 影响(原型 PV2 实测 73ms 放行);server 级规则集(含 R1)自动覆盖
//! 裁决会话,PV1 实证 R1 拦截只丢弃指令不推进 version,会话可复用。
//!
//! ## 设计(立项档 O-120 §11.1-11.3,四不变式)
//! - 意图指令形态不变:仍是中性 `set meta_tool.pending_target_scope`
//!   (机制层生产规范字段、规则层裁决,宪法 §七分工不变);
//! - R1 规则资产零改动;fail-closed 不变:version 未推进=拦截,传输错误
//!   =失效重建一次重试,仍失败 fail-fast 上抛;
//! - workflow 场景零改动:phase 门/marks 继续走主会话
//!   `submit_signal_and_await_verdict`,本通道只服务 ReAct 工具意图。
//!
//! ## 生命周期
//! - 每 runner 一条裁决会话,惰性创建、轮内多次工具调用复用
//!   (version 持续推进不影响 before/after 判据);
//! - 会话不留存清理(历史即审计证据);initial_content 自述
//!   `{kind:"intent_adjudication", agent_type, main_session}` 供审计关联。

use serde_json::Value;

use crate::api::evorule_client::EvoruleApiClient;

/// 轮询窗口(与主会话裁决原语同参:20×50ms=1s;裁决会话无在途 io,
/// 引擎毫秒级推进,1s 上限宽裕)
const VERDICT_POLLS: usize = 20;
const VERDICT_INTERVAL_MS: u64 = 50;

/// O-120:裁决通道 —— 每 runner 一条独立 evorule 裁决会话
pub struct AdjudicationChannel {
    client: EvoruleApiClient,
    /// 裁决会话 id(None = 尚未创建,惰性建;传输错误后 reset 回 None)
    session_id: Option<String>,
    /// 主 agent 类型(进 initial_content 供审计关联)
    agent_type: String,
    /// 主会话 id(裁决时刻由 runner 传入,进 initial_content 供审计关联)
    main_session: Option<String>,
}

impl AdjudicationChannel {
    /// 创建裁决通道(会话惰性建,构造零网络开销)
    pub fn new(client: EvoruleApiClient, agent_type: &str) -> Self {
        Self {
            client,
            session_id: None,
            agent_type: agent_type.to_string(),
            main_session: None,
        }
    }

    /// 惰性创建裁决会话(已建则复用)。创建失败 fail-fast。
    async fn ensure_session(&mut self) -> Result<String, String> {
        if let Some(id) = &self.session_id {
            return Ok(id.clone());
        }
        // 审计关联载体:裁决会话自述身份 + 主会话(已知则带)
        let mut initial = serde_json::json!({
            "kind": "intent_adjudication",
            "agent_type": self.agent_type,
        });
        if let Some(ms) = &self.main_session {
            initial["main_session"] = Value::from(ms.clone());
        }
        let id = self
            .client
            .create_session(Some(&initial))
            .await
            .map_err(|e| e.to_string())?;
        self.session_id = Some(id.clone());
        Ok(id)
    }

    /// 失效重建(传输错误后调用):仅作废会话句柄,下次裁决惰性重建。
    /// R1 拦截**不**触发 reset(PV1 实证拦截不破坏会话,可继续复用)。
    pub fn reset(&mut self) {
        self.session_id = None;
    }

    async fn session_version(&self, session_id: &str) -> Result<u64, String> {
        let state = self
            .client
            .get_state(session_id)
            .await
            .map_err(|e| e.to_string())?;
        state
            .get("version")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| format!("adjudication session {} state missing version", session_id))
    }

    /// 单次裁决尝试(不含重试)。`Err` = 感知通道故障(传输错误)。
    async fn try_verdict(&mut self, command: &Value) -> Result<bool, String> {
        let session_id = self.ensure_session().await?;
        let before = self.session_version(&session_id).await?;
        self.client
            .submit_command(&session_id, command)
            .await
            .map_err(|e| e.to_string())?;
        for _ in 0..VERDICT_POLLS {
            tokio::time::sleep(std::time::Duration::from_millis(VERDICT_INTERVAL_MS)).await;
            if self.session_version(&session_id).await? > before {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 提交意图指令并按裁决会话 version 判别规则层裁决结果
    ///
    /// - `main_session`:主会话 id(Some 时进裁决会话 initial_content 审计关联);
    /// - 返回 `Ok(true)` = 放行(version 推进);`Ok(false)` = 被 enforce
    ///   拦截(引擎丢弃指令不推进 version);`Err` = 通道故障;
    /// - 传输错误 → `reset()` 重建一次重试 → 仍失败 fail-fast
    ///   (fail-closed 语义不变,立项档 §11.3)。
    pub async fn await_verdict(
        &mut self,
        command: &Value,
        main_session: Option<&str>,
    ) -> Result<bool, String> {
        self.main_session = main_session.map(|s| s.to_string());
        match self.try_verdict(command).await {
            Ok(v) => Ok(v),
            Err(first) => {
                tracing::warn!(
                    error = %first,
                    "adjudication channel transport error; resetting session and retrying once"
                );
                self.reset();
                self.try_verdict(command).await.map_err(|second| {
                    format!(
                        "adjudication channel failed after reset retry: {second} (first error: {first})"
                    )
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_client(server: &mockito::Server) -> EvoruleApiClient {
        EvoruleApiClient::new(&server.url())
    }

    fn intent_like(scope: &str) -> Value {
        // 与生产 intent_signal 同形(中性 set meta_tool.pending_target_scope)
        serde_json::json!({
            "type": "set",
            "params": {
                "attr": "meta_tool.pending_target_scope",
                "operation": "set",
                "value": scope
            }
        })
    }

    #[tokio::test]
    async fn o120_creates_session_lazily_and_allows_on_version_advance() {
        let mut server = mockito::Server::new_async().await;
        let mut ch = AdjudicationChannel::new(make_client(&server), "tester");

        // 首次裁决:lazy create + before(version 0) + command + poll(version 1) = 放行
        let m_create = server
            .mock("POST", "/api/sessions")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "initial_content": {
                    "kind": "intent_adjudication",
                    "agent_type": "tester",
                    "main_session": "42"
                }
            })))
            .with_status(200)
            .with_body(r#"{"session_id": 77}"#)
            .expect(1)
            .create_async()
            .await;
        // before 首查命中 version 0,轮询命中 version 1(创建顺序匹配)
        let m_state_before = server
            .mock("GET", "/api/sessions/77/state")
            .with_status(200)
            .with_body(r#"{"version": 0}"#)
            .expect(1)
            .create_async()
            .await;
        let _m_state_after = server
            .mock("GET", "/api/sessions/77/state")
            .with_status(200)
            .with_body(r#"{"version": 1}"#)
            .create_async()
            .await;
        let m_cmd = server
            .mock("POST", "/api/sessions/77/command")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;

        let allowed = ch
            .await_verdict(&intent_like("in_sandbox"), Some("42"))
            .await
            .expect("verdict channel must not fail");
        assert!(allowed, "version advance must be read as allow");
        m_create.assert_async().await;
        m_state_before.assert_async().await;
        m_cmd.assert_async().await;
    }

    #[tokio::test]
    async fn o120_reuses_session_across_verdicts() {
        let mut server = mockito::Server::new_async().await;
        let mut ch = AdjudicationChannel::new(make_client(&server), "tester");

        // create 仅 1 次(第二次裁决复用会话);version 跨裁决持续推进,
        // 每次裁决内部 before→poll 都能一次命中(0→1,1→2):
        // 匹配序 = 4 次 GET 依次命中 v0、v1(poll1)、v1(before2)、v2(poll2)
        let m_create = server
            .mock("POST", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"session_id": 77}"#)
            .expect(1)
            .create_async()
            .await;
        let _m_state_0 = server
            .mock("GET", "/api/sessions/77/state")
            .with_status(200)
            .with_body(r#"{"version": 0}"#)
            .expect(1)
            .create_async()
            .await;
        let _m_state_1 = server
            .mock("GET", "/api/sessions/77/state")
            .with_status(200)
            .with_body(r#"{"version": 1}"#)
            .expect(2)
            .create_async()
            .await;
        let _m_state_2 = server
            .mock("GET", "/api/sessions/77/state")
            .with_status(200)
            .with_body(r#"{"version": 2}"#)
            .create_async()
            .await;
        let _m_cmd = server
            .mock("POST", "/api/sessions/77/command")
            .with_status(200)
            .with_body("{}")
            .expect(2)
            .create_async()
            .await;

        let a1 = ch
            .await_verdict(&intent_like("in_sandbox"), Some("42"))
            .await
            .expect("first verdict must not fail");
        let a2 = ch
            .await_verdict(&intent_like("in_sandbox"), Some("42"))
            .await
            .expect("second verdict must not fail");
        assert!(a1 && a2);
        m_create.assert_async().await; // 复用实证:只建过一次会话
    }

    #[tokio::test]
    async fn o120_blocks_when_version_stalls() {
        let mut server = mockito::Server::new_async().await;
        let mut ch = AdjudicationChannel::new(make_client(&server), "tester");

        // version 恒 0(1 次首查 + 20 次轮询 = 21):窗口耗尽 → Ok(false)
        // (fail-closed:被 enforce 拦截的引擎语义 = 丢弃指令不推进 version)
        let m_state = server
            .mock("GET", "/api/sessions/77/state")
            .with_status(200)
            .with_body(r#"{"version": 0}"#)
            .expect(21)
            .create_async()
            .await;
        server
            .mock("POST", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"session_id": 77}"#)
            .create_async()
            .await;
        server
            .mock("POST", "/api/sessions/77/command")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;

        let allowed = ch
            .await_verdict(&intent_like("out_of_sandbox"), Some("42"))
            .await
            .expect("channel must not fail on block");
        assert!(!allowed, "stalled version must be read as blocked");
        m_state.assert_async().await;
    }

    #[tokio::test]
    async fn o120_resets_and_retries_once_on_transport_error() {
        let mut server = mockito::Server::new_async().await;
        let mut ch = AdjudicationChannel::new(make_client(&server), "tester");

        // 第 1 次:会话 77 建好后 state 读取 500(传输错误)→ reset 重建
        // 第 2 次:会话 78 建好 → before=5 → command → poll=6 → 放行
        let m_create1 = server
            .mock("POST", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"session_id": 77}"#)
            .expect(1)
            .create_async()
            .await;
        let m_state_500 = server
            .mock("GET", "/api/sessions/77/state")
            .with_status(500)
            .expect(1)
            .create_async()
            .await;
        // mockito 按创建顺序匹配:create1 耗尽后第二个 create 请求落到 m_create2
        let m_create2 = server
            .mock("POST", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"session_id": 78}"#)
            .expect(1)
            .create_async()
            .await;
        let m_state78_before = server
            .mock("GET", "/api/sessions/78/state")
            .with_status(200)
            .with_body(r#"{"version": 5}"#)
            .expect(1)
            .create_async()
            .await;
        let _m_state78_after = server
            .mock("GET", "/api/sessions/78/state")
            .with_status(200)
            .with_body(r#"{"version": 6}"#)
            .create_async()
            .await;
        let m_cmd78 = server
            .mock("POST", "/api/sessions/78/command")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;

        let allowed = ch
            .await_verdict(&intent_like("in_sandbox"), Some("42"))
            .await
            .expect("retry after reset must succeed");
        assert!(allowed, "recreated session verdict must be read");
        m_create1.assert_async().await;
        m_state_500.assert_async().await;
        m_create2.assert_async().await;
        m_state78_before.assert_async().await;
        m_cmd78.assert_async().await;
    }

    #[tokio::test]
    async fn o120_fails_fast_when_retry_also_fails() {
        let mut server = mockito::Server::new_async().await;
        let mut ch = AdjudicationChannel::new(make_client(&server), "tester");

        // 两次 create 都成功但 state 恒 500 → 重建重试后仍失败 = fail-fast
        server
            .mock("POST", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"session_id": 77}"#)
            .expect(2)
            .create_async()
            .await;
        server
            .mock("GET", "/api/sessions/77/state")
            .with_status(500)
            .expect(2)
            .create_async()
            .await;

        let r = ch.await_verdict(&intent_like("in_sandbox"), None).await;
        assert!(
            r.is_err(),
            "transport error after reset retry must surface as channel error"
        );
        let msg = r.unwrap_err();
        assert!(
            msg.contains("after reset retry"),
            "error must mark the reset-retry semantics, got: {}",
            msg
        );
    }
}
