// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Evorule HTTP API 客户端 —— 封装 session/audit 相关 API 调用。
//! 持有共享 ApiCore（base_url + client + auth），与 WorkspaceApiClient 职责分离。

use crate::api::api_core::{ApiCore, ApiError};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::pin::Pin;

#[derive(Debug, Clone)]
pub struct EvoruleApiClient {
    core: ApiCore,
}

impl EvoruleApiClient {
    /// Create new API client.
    /// If `EVORULE_AUTH_TOKEN` env var is set, use it as Bearer token.
    /// Otherwise, send no auth header (server must be in dev mode / no auth).
    pub fn new(base_url: &str) -> Self {
        Self {
            core: ApiCore::new(base_url),
        }
    }

    /// 返回 evorule server 的 base_url(供 serve 模式构造 WorkspaceApiClient 等)
    pub fn base_url(&self) -> &str {
        self.core.base_url()
    }

    pub async fn create_session(
        &self,
        initial_content: Option<&Value>,
    ) -> Result<String, ApiError> {
        let url = self.core.url("/api/sessions");

        let body = if let Some(content) = initial_content {
            serde_json::json!({ "initial_content": content })
        } else {
            serde_json::json!({})
        };

        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        let session_id = result["session_id"]
            .as_u64()
            .ok_or(ApiError::InvalidResponse)?
            .to_string();

        Ok(session_id)
    }

    pub async fn create_session_fork(
        &self,
        parent_id: &str,
        version: Option<u64>,
    ) -> Result<String, ApiError> {
        let url = if let Some(v) = version {
            format!(
                "{}/api/sessions/fork/{}?version={}",
                self.core.base_url(),
                parent_id,
                v
            )
        } else {
            format!("{}/api/sessions/fork/{}", self.core.base_url(), parent_id)
        };

        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        let session_id = result["session_id"]
            .as_u64()
            .ok_or(ApiError::InvalidResponse)?
            .to_string();

        Ok(session_id)
    }

    pub async fn submit_command(&self, session_id: &str, command: &Value) -> Result<(), ApiError> {
        let url = format!(
            "{}/api/sessions/{}/command",
            self.core.base_url(),
            session_id
        );

        let body = serde_json::json!({ "instruction": command });
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        Ok(())
    }

    pub async fn submit_io_response(
        &self,
        session_id: &str,
        request_id: u64,
        result: &Value,
        error: Option<&str>,
    ) -> Result<(), ApiError> {
        let url = format!(
            "{}/api/sessions/{}/io_response",
            self.core.base_url(),
            session_id
        );

        let body = serde_json::json!({
            "request_id": request_id,
            "result": result,
            "error": error,
        });

        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        Ok(())
    }

    pub async fn update_payload(
        &self,
        session_id: &str,
        path: &str,
        value: &Value,
    ) -> Result<(), ApiError> {
        let url = format!(
            "{}/api/sessions/{}/payload",
            self.core.base_url(),
            session_id
        );

        let body = serde_json::json!({
            "path": path,
            "value": value,
        });

        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        Ok(())
    }

    pub async fn get_state(&self, session_id: &str) -> Result<Value, ApiError> {
        let url = format!("{}/api/sessions/{}/state", self.core.base_url(), session_id);

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    pub async fn get_facts(
        &self,
        session_id: &str,
        prefix: Option<&str>,
    ) -> Result<Vec<FactEntry>, ApiError> {
        let url = if let Some(p) = prefix {
            format!(
                "{}/api/sessions/{}/facts?prefix={}",
                self.core.base_url(),
                session_id,
                p
            )
        } else {
            format!("{}/api/sessions/{}/facts", self.core.base_url(), session_id)
        };

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Vec<FactEntry> = resp.json().await?;
        Ok(result)
    }

    pub async fn get_shared_facts(
        &self,
        prefix: Option<&str>,
    ) -> Result<Vec<SharedFactEntry>, ApiError> {
        let url = if let Some(p) = prefix {
            format!("{}/api/shared/facts?prefix={}", self.core.base_url(), p)
        } else {
            format!("{}/api/shared/facts", self.core.base_url())
        };

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Vec<SharedFactEntry> = resp.json().await?;
        Ok(result)
    }

    /// `POST /api/shared/facts/rollup` — 标记一批共享事实为已 rollup
    ///
    /// 用于 C1 sediment 的 C4 rollup：把被合并的旧会话摘要标记成 `rolled_up`，
    /// 使它们在 server 端 `facts_by_path_prefix` 查询中被过滤，避免下次仍计入
    /// 阈值、反复 rollup 导致共享空间膨胀（L-3 修复）。
    /// 标记后仍可通过 `fact_by_id` 访问，保留审计可追溯性。
    pub async fn mark_shared_facts_rollup(
        &self,
        fact_ids: &[u64],
    ) -> Result<(), ApiError> {
        let url = format!("{}/api/shared/facts/rollup", self.core.base_url());
        let body = serde_json::json!({
            "fact_ids": fact_ids,
        });
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        Ok(())
    }

    pub async fn get_shared_fact_source(&self, fact_id: u64) -> Result<SharedFactEntry, ApiError> {
        let url = format!(
            "{}/api/shared/facts/{}/source",
            self.core.base_url(),
            fact_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: SharedFactEntry = resp.json().await?;
        Ok(result)
    }

    pub async fn record_used_at_startup(
        &self,
        session_id: &str,
        fact_ids: &[u64],
    ) -> Result<(), ApiError> {
        let url = format!(
            "{}/api/sessions/{}/used_at_startup",
            self.core.base_url(),
            session_id
        );

        let body = serde_json::json!({
            "fact_ids": fact_ids,
        });

        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        Ok(())
    }

    pub async fn get_used_at_startup(&self, session_id: &str) -> Result<Vec<u64>, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/used_at_startup",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        let fact_ids: Vec<u64> = result["fact_ids"]
            .as_array()
            .ok_or(ApiError::InvalidResponse)?
            .iter()
            .filter_map(|v| v.as_u64())
            .collect();

        Ok(fact_ids)
    }

    pub async fn get_sessions_using_fact(&self, fact_id: u64) -> Result<Vec<u64>, ApiError> {
        let url = format!(
            "{}/api/shared/facts/{}/used_by",
            self.core.base_url(),
            fact_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        let sessions: Vec<u64> = result["sessions"]
            .as_array()
            .ok_or(ApiError::InvalidResponse)?
            .iter()
            .filter_map(|v| v.as_u64())
            .collect();

        Ok(sessions)
    }

    pub async fn get_audit_report(&self, session_id: &str) -> Result<Value, ApiError> {
        let url = format!("{}/api/sessions/{}/audit", self.core.base_url(), session_id);

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    /// **已废弃**：使用 `verify_audit_typed` 替代。此方法字段名已修正（valid→verified）。
    pub async fn verify_audit(&self, session_id: &str) -> Result<bool, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/audit/verify",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        let valid = result["verified"]
            .as_bool()
            .ok_or(ApiError::InvalidResponse)?;

        Ok(valid)
    }

    /// **已废弃**：使用 `get_causal_chain_typed` 替代。此方法已修正为按对象解析。
    pub async fn get_causal_chain(
        &self,
        session_id: &str,
        fact_id: u64,
    ) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/audit/causal/{}",
            self.core.base_url(),
            session_id,
            fact_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    /// B2/B4：类型化整链验证 —— GET /api/sessions/{id}/audit/verify → AuditVerify
    pub async fn verify_audit_typed(
        &self,
        session_id: &str,
    ) -> Result<crate::agent::memory_event::evidence::AuditVerify, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/audit/verify",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result = resp.json().await?;
        Ok(result)
    }

    /// B2/B4：类型化因果链 —— GET /api/sessions/{id}/audit/causal/{fact_id} → CausalChain
    pub async fn get_causal_chain_typed(
        &self,
        session_id: &str,
        fact_id: u64,
    ) -> Result<crate::agent::memory_event::evidence::CausalChain, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/audit/causal/{}",
            self.core.base_url(),
            session_id,
            fact_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result = resp.json().await?;
        Ok(result)
    }

    pub async fn rewind(&self, session_id: &str, version: u64) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/rewind?version={}",
            self.core.base_url(),
            session_id,
            version
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    pub async fn replay(&self, session_id: &str) -> Result<Vec<Value>, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/replay",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Vec<Value> = resp.json().await?;
        Ok(result)
    }

    /// B3：版本区间 Fact 流重放（read_from 等价）
    /// GET /api/sessions/{id}/replay?from={from}&to={to}
    pub async fn replay_range(
        &self,
        session_id: &str,
        from: Option<u64>,
        to: Option<u64>,
    ) -> Result<Vec<FactLogEntry>, ApiError> {
        let mut url = format!(
            "{}/api/sessions/{}/replay",
            self.core.base_url(),
            session_id
        );
        let mut qs = Vec::new();
        if let Some(f) = from {
            qs.push(format!("from={}", f));
        }
        if let Some(t) = to {
            qs.push(format!("to={}", t));
        }
        if !qs.is_empty() {
            url.push('?');
            url.push_str(&qs.join("&"));
        }
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        Ok(resp.json().await?)
    }

    pub async fn diff(&self, session_id: &str, a: u64, b: u64) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/diff?a={}&b={}",
            self.core.base_url(),
            session_id,
            a,
            b
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    /// E2: POST /api/sessions/{id}/join — 对齐 04b JoinRequest
    /// cluster_id=None → 创建新集群；Some(id) → 加入指定集群
    pub async fn join_cluster(
        &self,
        session_id: &str,
        cluster_id: Option<u64>,
    ) -> Result<JoinResponse, ApiError> {
        let url = format!("{}/api/sessions/{}/join", self.core.base_url(), session_id);
        let body = serde_json::json!({
            "cluster_id": cluster_id,
            "direction": "bidirectional"
        });
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        Ok(resp.json().await?)
    }

    pub async fn leave_cluster(&self, session_id: &str) -> Result<(), ApiError> {
        let url = format!("{}/api/sessions/{}/leave", self.core.base_url(), session_id);

        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        Ok(())
    }

    pub async fn get_cluster_status(&self, session_id: &str) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/cluster",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    pub async fn subscribe_events(&self, session_id: &str) -> Result<EvoruleEventStream, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/events",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(ApiError::ApiError {
                status: resp.status().as_u16(),
                message: resp.text().await.unwrap_or_default(),
            });
        }

        Ok(EvoruleEventStream {
            buffer: Vec::new(),
            stream: Box::pin(resp.bytes_stream()),
        })
    }

    pub async fn get_debug_phase(&self, session_id: &str) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/debug/phase",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    pub async fn get_debug_queue(&self, session_id: &str) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/debug/queue",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    pub async fn get_debug_pending_io(&self, session_id: &str) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/debug/pending_io",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }
}

/// 04b JoinResponse — join_cluster 接口返回体
#[derive(Debug, Clone, Deserialize)]
pub struct JoinResponse {
    pub cluster_id: u64,
    #[serde(default)]
    pub members: Vec<u64>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FactEntry {
    pub version: u64,
    /// server 返回 "fact_id"；映射到 id 保持现有引用不变（B1 契约修复）
    #[serde(rename = "fact_id", default)]
    pub id: u64,
    pub path: String,
    pub value: Value,
    /// server 不返回 "type"；default 兜底避免反序列化失败（B1 契约修复）
    #[serde(rename = "type", default)]
    pub fact_type: String,
}

/// /replay 返回的单个 Fact（fact.to_json() + version）
#[derive(Debug, Clone, Deserialize)]
pub struct FactLogEntry {
    pub version: u64,
    #[serde(rename = "type", default)]
    pub fact_type: String,
    #[serde(rename = "fact_id", default)]
    pub id: u64,
    #[serde(default)]
    pub cause: Option<u64>,
    /// 变体字段扁平捕获（path/value/instruction/io_type/params/result/...）
    #[serde(flatten)]
    pub payload: Value,
}

impl FactLogEntry {
    pub fn path(&self) -> Option<&str> {
        self.payload.get("path").and_then(|v| v.as_str())
    }
    pub fn value(&self) -> Option<&Value> {
        self.payload.get("value")
    }
    pub fn instruction(&self) -> Option<&Value> {
        self.payload.get("instruction")
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct SharedFactEntry {
    pub fact_id: u64,
    pub path: String,
    pub value: Value,
    pub source_session_id: u64,
    pub version: u64,
}

pub struct EvoruleEventStream {
    buffer: Vec<u8>,
    stream: Pin<Box<dyn futures_core::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
}

#[derive(Debug, Deserialize)]
pub struct EvoruleEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(flatten)]
    pub payload: Value,
}

impl EvoruleEventStream {
    pub async fn next(&mut self) -> Option<EvoruleEvent> {
        let mut data = String::new();

        loop {
            loop {
                let (line_end, line_len) =
                    if let Some(pos) = self.buffer.windows(2).position(|w| w == b"\r\n") {
                        (pos, 2)
                    } else if let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
                        (pos, 1)
                    } else {
                        break;
                    };

                let line = String::from_utf8_lossy(&self.buffer[..line_end])
                    .trim_end_matches('\r')
                    .to_string();
                self.buffer.drain(..line_end + line_len);

                if line.starts_with("data: ") {
                    let json_part = line.strip_prefix("data: ").unwrap_or(&line);
                    data.push_str(json_part);
                } else if line.is_empty() && !data.is_empty() {
                    match serde_json::from_str::<EvoruleEvent>(&data) {
                        Ok(ev) => return Some(ev),
                        Err(e) => {
                            tracing::warn!("Failed to parse event: {}", e);
                            data.clear();
                        }
                    }
                }
            }

            match self.stream.next().await {
                Some(Ok(chunk)) => {
                    self.buffer.extend_from_slice(&chunk);
                }
                Some(Err(e)) => {
                    tracing::warn!("SSE stream error: {}", e);
                    return None;
                }
                None => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_client() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:8080")
    }

    #[test]
    fn test_client_new() {
        let client = make_test_client();
        assert_eq!(client.core.base_url(), "http://localhost:8080");
    }

    // ===== B1 FactEntry 契约修复测试 =====

    #[test]
    fn test_fact_entry_deserialize_server_format() {
        // server 返回 "fact_id"（非 "id"），不返回 "type"
        let json = r#"{"fact_id": 42, "version": 7, "path": "__memory__.x.session_1.k", "value": {"key":"k","value":"v","timestamp":1700000000}}"#;
        let fact: FactEntry = serde_json::from_str(json).unwrap();
        assert_eq!(fact.id, 42); // rename 生效：fact_id → id
        assert_eq!(fact.version, 7);
        assert_eq!(fact.path, "__memory__.x.session_1.k");
        assert_eq!(fact.fact_type, ""); // server 不返回 type，default 兜底
    }

    #[test]
    fn test_fact_entry_deserialize_with_type() {
        // 如果 server 返回了 type，也能正确解析
        let json =
            r#"{"fact_id": 1, "version": 1, "path": "test", "value": {}, "type": "PayloadUpdate"}"#;
        let fact: FactEntry = serde_json::from_str(json).unwrap();
        assert_eq!(fact.fact_type, "PayloadUpdate");
    }

    #[test]
    fn test_fact_entry_deserialize_missing_fact_id() {
        // fact_id 缺失时 default 兜底为 0
        let json = r#"{"version": 1, "path": "test", "value": {}}"#;
        let fact: FactEntry = serde_json::from_str(json).unwrap();
        assert_eq!(fact.id, 0);
    }

    // ===== B3 FactLogEntry 反序列化测试 =====

    #[test]
    fn test_fact_log_entry_deserialize() {
        // Command 变体
        let json = r#"{"version": 1, "type": "Command", "fact_id": 10, "cause": null, "instruction": {"type": "call_external", "params": {"goal": "hello"}}}"#;
        let entry: FactLogEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.version, 1);
        assert_eq!(entry.fact_type, "Command");
        assert_eq!(entry.id, 10);
        assert!(entry.cause.is_none());
        assert!(entry.instruction().is_some());
        assert_eq!(entry.instruction().unwrap()["params"]["goal"], "hello");

        // PayloadUpdate 变体
        let json = r#"{"version": 2, "type": "PayloadUpdate", "fact_id": 20, "path": "__memory__.x.session_1.k", "value": {"key": "k", "value": "v", "timestamp": 1700000000}}"#;
        let entry: FactLogEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.fact_type, "PayloadUpdate");
        assert_eq!(entry.id, 20);
        assert_eq!(entry.path(), Some("__memory__.x.session_1.k"));
        assert!(entry.value().is_some());
        assert_eq!(entry.value().unwrap()["key"], "k");

        // Stable 变体
        let json = r#"{"version": 3, "type": "Stable", "fact_id": 30, "final_snapshot": {"payload": "done"}}"#;
        let entry: FactLogEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.fact_type, "Stable");
        assert_eq!(entry.id, 30);
        assert_eq!(entry.payload["final_snapshot"]["payload"], "done");

        // cause 字段
        let json = r#"{"version": 4, "type": "IoRequest", "fact_id": 40, "cause": 10, "io_type": "call_external", "params": {"model": "test"}}"#;
        let entry: FactLogEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.fact_type, "IoRequest");
        assert_eq!(entry.cause, Some(10));
        assert_eq!(entry.payload["io_type"], "call_external");
        assert_eq!(entry.payload["params"]["model"], "test");
    }

    #[test]
    fn test_build_call_external_command() {
        let command = serde_json::json!({
            "type": "call_external",
            "params": {
                "model": "gpt-4o-mini",
                "temperature": 0.7,
                "system_prompt": "You are a helpful assistant",
                "goal": "Test task",
                "tool_names": ["search", "calculator"],
            }
        });

        assert_eq!(command["type"], "call_external");
        assert_eq!(command["params"]["model"], "gpt-4o-mini");
        assert_eq!(command["params"]["goal"], "Test task");
    }

    #[test]
    fn test_evorule_event_deserialize() {
        let json = r#"{"type":"IoRequest","id":2,"cause":1,"io_type":"call_external","params":{"model":"test"}}"#;
        let event: EvoruleEvent = serde_json::from_str(json).unwrap();

        assert_eq!(event.event_type, "IoRequest");
        assert_eq!(event.payload["io_type"], "call_external");
        assert_eq!(event.payload["id"], 2);
        assert_eq!(event.payload["params"]["model"], "test");
    }

    #[test]
    fn test_evorule_event_deserialize_stable() {
        let json = r#"{"type":"Stable","id":3,"final_snapshot":{"payload":"task completed"}}"#;
        let event: EvoruleEvent = serde_json::from_str(json).unwrap();

        assert_eq!(event.event_type, "Stable");
        assert_eq!(event.payload["id"], 3);
        assert_eq!(event.payload["final_snapshot"]["payload"], "task completed");
    }

    #[tokio::test]
    async fn test_fact_closed_loop_workflow() {
        let _client = make_test_client();

        let goal = "What is the weather today?";
        let system_prompt = "You are a weather assistant";

        let command = serde_json::json!({
            "type": "call_external",
            "params": {
                "model": "gpt-4o-mini",
                "temperature": 0.7,
                "system_prompt": system_prompt,
                "goal": goal,
                "tool_names": ["weather_api"],
            }
        });

        assert_eq!(command["type"], "call_external");
        assert_eq!(command["params"]["goal"], goal);
        assert_eq!(command["params"]["system_prompt"], system_prompt);

        let io_response = serde_json::json!({
            "content": "The weather is sunny today",
            "tool_calls": null,
            "is_finished": true,
        });

        assert_eq!(io_response["content"], "The weather is sunny today");
        assert!(io_response["is_finished"].as_bool().unwrap());
    }

    #[test]
    fn test_io_response_payload_structure() {
        let request_id: u64 = 123;
        let content = "Tool execution result";
        let used_facts = vec!["fact1".to_string(), "fact2".to_string()];

        let io_response_body = serde_json::json!({
            "request_id": request_id,
            "content": content,
            "used_facts": used_facts,
        });

        assert_eq!(io_response_body["request_id"], request_id);
        assert_eq!(io_response_body["content"], content);
        assert_eq!(io_response_body["used_facts"].as_array().unwrap().len(), 2);
    }

    // ===== E2 join_cluster body / JoinResponse 反序列化测试 =====

    #[test]
    fn test_join_cluster_body_create() {
        // cluster_id=None → 创建新集群
        let body = serde_json::json!({
            "cluster_id": Option::<u64>::None,
            "direction": "bidirectional"
        });
        assert_eq!(body["cluster_id"], serde_json::Value::Null);
        assert_eq!(body["direction"], "bidirectional");
    }

    #[test]
    fn test_join_cluster_body_join() {
        // cluster_id=Some(42) → 加入已有集群
        let body = serde_json::json!({
            "cluster_id": Some(42u64),
            "direction": "bidirectional"
        });
        assert_eq!(body["cluster_id"], 42);
        assert_eq!(body["direction"], "bidirectional");
    }

    #[test]
    fn test_join_response_deserialize() {
        let json = r#"{"cluster_id": 42, "members": [1, 2, 3]}"#;
        let resp: JoinResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.cluster_id, 42);
        assert_eq!(resp.members, vec![1, 2, 3]);
    }

    #[test]
    fn test_join_response_deserialize_empty_members() {
        // members 缺失时 default 兜底为空数组
        let json = r#"{"cluster_id": 7}"#;
        let resp: JoinResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.cluster_id, 7);
        assert!(resp.members.is_empty());
    }
}
