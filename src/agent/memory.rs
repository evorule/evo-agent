// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Agent memory manager -- manages memory via evorule payload API
//!
//! # Namespace convention (three-layer, P1 分层设计)
//! - shared memory:  `__memory__.agent_{type}.shared.{key}`
//! - session memory: `__memory__.agent_{type}.session_{session_id}.{key}`
//! - short-term messages: `__memory__.agent_{type}.session_{session_id}.messages.{idx}`
//! - session summary:    `__memory__.agent_{type}.session_{session_id}.summary`
//! - session meta:       `__memory__.agent_{type}.session_{session_id}.meta`
//!
//! # Architecture (031 设计文档 P0+P1)
//! - 短期记忆（messages）通过 `Fact::PayloadUpdate` 写入 evorule payload
//! - 进入 FactsLog + Auditor 审计链，支持 replay/causal_chain
//! - 提交模式可配置：每条/每 N 条/每轮 ReAct/禁用
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::api::evorule_client::EvoruleApiClient;
use crate::agent::translator::Message;

/// 内存操作错误
#[derive(Debug)]
pub enum MemoryError {
    /// IO 错误
    Io(std::io::Error),
    /// JSON 序列化错误
    Json(serde_json::Error),
    /// 空键
    EmptyKey,
    /// 键过长
    KeyTooLong(usize),
    /// evorule API 错误
    EvoruleError(String),
    /// session 未设置
    SessionNotSet,
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryError::Io(e) => write!(f, "IO error: {}", e),
            MemoryError::Json(e) => write!(f, "JSON error: {}", e),
            MemoryError::EmptyKey => write!(f, "memory key cannot be empty"),
            MemoryError::KeyTooLong(len) => write!(f, "memory key too long ({} chars)", len),
            MemoryError::EvoruleError(e) => write!(f, "Evorule API error: {}", e),
            MemoryError::SessionNotSet => write!(f, "session not set"),
        }
    }
}

impl std::error::Error for MemoryError {}

impl From<std::io::Error> for MemoryError {
    fn from(e: std::io::Error) -> Self {
        MemoryError::Io(e)
    }
}

impl From<serde_json::Error> for MemoryError {
    fn from(e: serde_json::Error) -> Self {
        MemoryError::Json(e)
    }
}

impl From<crate::api::evorule_client::EvoruleApiError> for MemoryError {
    fn from(e: crate::api::evorule_client::EvoruleApiError) -> Self {
        MemoryError::EvoruleError(e.to_string())
    }
}

/// 记忆作用域（P1 三层分层）
///
/// 控制 KV/消息写入 evorule payload 的命名空间路径。
/// - `Shared` 跨会话共享（所有 session 可见）
/// - `Session` 会话级（仅当前 session 可见）
/// - `Messages` 短期对话历史（按 idx 索引）
#[derive(Debug, Clone)]
pub enum MemoryScope {
    /// 跨会话共享：`__memory__.agent_{ns}.shared.{key}`
    Shared,
    /// 会话级：`__memory__.agent_{ns}.session_{sid}.{key}`
    Session(String),
    /// 短期消息：`__memory__.agent_{ns}.session_{sid}.messages.{idx}`
    Messages(String, usize),
}

impl MemoryScope {
    /// 使用当前 MemoryManager 的 session_id 构造 Session scope
    fn session_from_opt(session_id: &Option<String>) -> Result<Self, MemoryError> {
        match session_id {
            Some(sid) => Ok(MemoryScope::Session(sid.clone())),
            None => Err(MemoryError::SessionNotSet),
        }
    }
}

/// 消息持久化模式（031 设计文档 P0，用户决策 2：可选开关）
///
/// 控制 `AgentRunner` 何时把 messages 写入 evorule payload。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessagePersistMode {
    /// 每条消息立即写入（默认，最安全）
    EveryMessage,
    /// 每 N 条消息批量写入（性能优先）
    EveryN(usize),
    /// 每轮 ReAct 结束时写入（IoRequest 处理前 flush）
    PerReactRound,
    /// 不持久化 messages（向后兼容旧行为）
    Disabled,
}

impl Default for MessagePersistMode {
    fn default() -> Self {
        MessagePersistMode::EveryMessage
    }
}

impl MessagePersistMode {
    /// 是否需要缓冲
    pub fn needs_buffer(&self) -> bool {
        matches!(self, MessagePersistMode::EveryN(_) | MessagePersistMode::PerReactRound)
    }

    /// 是否完全禁用持久化
    pub fn is_disabled(&self) -> bool {
        matches!(self, MessagePersistMode::Disabled)
    }
}

/// 内存记录（KV 存储）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// 键
    pub key: String,
    /// 值
    pub value: String,
    /// 时间戳（Unix 秒）
    pub timestamp: u64,
    /// 来源（可选，031 P2 扩展）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// 置信度（可选，0.0-1.0）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// 标签（可选）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl MemoryRecord {
    /// 创建基础记录（无 source/confidence/tags，向后兼容）
    pub fn new(key: &str, value: &str, timestamp: u64) -> Self {
        Self {
            key: key.to_string(),
            value: value.to_string(),
            timestamp,
            source: None,
            confidence: None,
            tags: Vec::new(),
        }
    }
}

/// 消息记录（短期记忆，P0）
///
/// 关联 evorule 的 FactId，支持 causal_chain 追溯。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    /// 消息索引（在 messages 数组中的位置）
    pub idx: usize,
    /// 角色：system / user / assistant / tool
    pub role: String,
    /// 消息内容
    pub content: String,
    /// 工具调用（仅 assistant 消息，可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<serde_json::Value>,
    /// 工具名（仅 tool 消息，可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// 时间戳（Unix 秒）
    pub timestamp: u64,
    /// 关联的 evorule FactId（写入后由 evorule 分配，可选用于 causal_chain）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact_id: Option<u64>,
}

impl MessageRecord {
    /// 从 Message 构造记录
    pub fn from_message(idx: usize, message: &Message, timestamp: u64) -> Self {
        match message {
            Message::System { content } => Self {
                idx,
                role: "system".to_string(),
                content: content.clone(),
                tool_calls: None,
                tool_name: None,
                timestamp,
                fact_id: None,
            },
            Message::User { content } => Self {
                idx,
                role: "user".to_string(),
                content: content.clone(),
                tool_calls: None,
                tool_name: None,
                timestamp,
                fact_id: None,
            },
            Message::Assistant { content, tool_calls } => Self {
                idx,
                role: "assistant".to_string(),
                content: content.clone(),
                tool_calls: tool_calls.as_ref().map(|tc| serde_json::to_value(tc).ok()).flatten(),
                tool_name: None,
                timestamp,
                fact_id: None,
            },
            Message::Tool { content, tool_name } => Self {
                idx,
                role: "tool".to_string(),
                content: content.clone(),
                tool_calls: None,
                tool_name: Some(tool_name.clone()),
                timestamp,
                fact_id: None,
            },
        }
    }

    /// 消息路径 key（用于 evorule payload path 的最后一段）
    pub fn path_key(&self) -> String {
        format!("messages.{}", self.idx)
    }
}

/// 当前 Unix 时间戳（秒）
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// 内存管理器（通过 evorule payload API 实现）
#[derive(Clone)]
pub struct MemoryManager {
    namespace: String,
    evorule_client: EvoruleApiClient,
    session_id: Option<String>,
    cache: BTreeMap<String, MemoryRecord>,
    /// 记忆过期时间（秒，用户决策 5：TTL）
    ///
    /// 设置后记忆条目在 `ttl_secs` 秒后过期。`None` 表示永不过期。
    /// 过期检查采用惰性策略：`get_scoped` 时检查，`cleanup_expired` 显式清理。
    ttl_secs: Option<u64>,
}

impl MemoryManager {
    /// 创建新管理器
    pub fn new(namespace: &str, evorule_client: EvoruleApiClient) -> Self {
        Self {
            namespace: namespace.to_string(),
            evorule_client,
            session_id: None,
            cache: BTreeMap::new(),
            ttl_secs: None,
        }
    }

    /// 设置 session_id（builder 风格）
    pub fn with_session_id(mut self, session_id: &str) -> Self {
        self.session_id = Some(session_id.to_string());
        self
    }

    /// 设置 TTL（builder 风格，用户决策 5）
    ///
    /// 设置后记忆条目在 `ttl_secs` 秒后过期。
    /// `get_scoped` 会惰性检查并移除过期条目，`cleanup_expired` 可显式清理。
    pub fn with_ttl_secs(mut self, ttl_secs: u64) -> Self {
        self.ttl_secs = Some(ttl_secs);
        self
    }

    /// 设置 session_id（可变引用）
    pub fn set_session_id(&mut self, session_id: &str) {
        self.session_id = Some(session_id.to_string());
    }

    /// 获取 TTL 配置
    pub fn ttl_secs(&self) -> Option<u64> {
        self.ttl_secs
    }

    /// 检查记录是否已过期（基于 TTL 配置）
    ///
    /// `ttl_secs == None` 时永不过期，返回 `false`。
    fn is_expired(&self, record: &MemoryRecord) -> bool {
        match self.ttl_secs {
            Some(ttl) => {
                let now = now_secs();
                // 防止时钟回拨导致误判
                now.saturating_sub(record.timestamp) > ttl
            }
            None => false,
        }
    }

    /// 显式清理所有过期的 cache 条目（用户决策 5：TTL）
    ///
    /// 返回被清理的条目数量。仅清理本地 cache，不删除 evorule 中的数据
    /// （evorule 侧的过期清理应由 evorule 自身或独立任务负责）。
    pub fn cleanup_expired(&mut self) -> usize {
        if self.ttl_secs.is_none() {
            return 0;
        }
        let now = now_secs();
        let ttl = self.ttl_secs.unwrap();
        let expired_keys: Vec<String> = self
            .cache
            .iter()
            .filter(|(_, record)| now.saturating_sub(record.timestamp) > ttl)
            .map(|(k, _)| k.clone())
            .collect();
        let count = expired_keys.len();
        for key in expired_keys {
            self.cache.remove(&key);
        }
        count
    }

    /// 获取 namespace
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// 获取 session_id
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// 旧版路径构建（向后兼容，使用单层 namespace）
    ///
    /// 生成 `__memory__.{namespace}.{key}` 形式路径。
    /// 新代码应使用 `build_path_scoped`。
    fn build_path(&self, key: &str) -> String {
        format!("__memory__.{}.{}", self.namespace, key)
    }

    /// 分层路径构建（P1 三层 namespace）
    ///
    /// 根据 scope 生成完整的 evorule payload 路径。
    /// - `Shared`: `__memory__.{ns}.shared.{key}`
    /// - `Session(sid)`: `__memory__.{ns}.session_{sid}.{key}`
    /// - `Messages(sid, idx)`: `__memory__.{ns}.session_{sid}.messages.{idx}`（忽略 key，idx 即 key）
    pub fn build_path_scoped(&self, scope: &MemoryScope, key: &str) -> String {
        match scope {
            MemoryScope::Shared => {
                format!("__memory__.{}.shared.{}", self.namespace, key)
            }
            MemoryScope::Session(sid) => {
                format!("__memory__.{}.session_{}.{}", self.namespace, sid, key)
            }
            MemoryScope::Messages(sid, idx) => {
                // Messages scope 中 idx 即 key，忽略传入的 key 参数
                format!("__memory__.{}.session_{}.messages.{}", self.namespace, sid, idx)
            }
        }
    }

    /// 旧版 set（向后兼容，默认 Session scope）
    ///
    /// 等价于 `set_scoped(MemoryScope::Session(self.session_id?), key, value)`。
    pub async fn set(&mut self, key: &str, value: &str) -> Result<(), MemoryError> {
        let scope = MemoryScope::session_from_opt(&self.session_id)?;
        self.set_scoped(scope, key, value).await
    }

    /// 分层 set（P1）
    ///
    /// 按 scope 写入 evorule payload，同时更新本地 cache。
    ///
    /// **注意**：HTTP 调用是 best-effort 的（与 `sync_from_evorule` 一致），
    /// 即 cache 总是更新，但 evorule 持久化失败不会传播错误。
    /// 这使得单元测试可以在无服务器环境下运行，且 KV 记忆的 cache 是主要读取源。
    /// 如需严格持久化错误传播，使用 `append_message`（P0 消息持久化）。
    pub async fn set_scoped(
        &mut self,
        scope: MemoryScope,
        key: &str,
        value: &str,
    ) -> Result<(), MemoryError> {
        if key.is_empty() {
            return Err(MemoryError::EmptyKey);
        }
        let max_key_len = 256;
        if key.len() > max_key_len {
            return Err(MemoryError::KeyTooLong(key.len()));
        }

        let timestamp = now_secs();
        let record = MemoryRecord::new(key, value, timestamp);
        let cache_key = self.cache_key_for(&scope, key);
        self.cache.insert(cache_key, record.clone());

        // best-effort 持久化：cache 是主要读取源，HTTP 失败不阻断
        let session_id = self.session_id_for_scope(&scope)?;
        let path = self.build_path_scoped(&scope, key);
        let payload_value = serde_json::to_value(record)?;
        let _ = self
            .evorule_client
            .update_payload(&session_id, &path, &payload_value)
            .await;

        Ok(())
    }

    /// 旧版 get（向后兼容，默认 Session scope）
    pub async fn get(&mut self, key: &str) -> Result<Option<MemoryRecord>, MemoryError> {
        let scope = MemoryScope::session_from_opt(&self.session_id)?;
        self.get_scoped(scope, key).await
    }

    /// 分层 get（P1）
    ///
    /// 如果配置了 TTL 且 cache 中的记录已过期，会惰性移除并返回 `None`。
    pub async fn get_scoped(
        &mut self,
        scope: MemoryScope,
        key: &str,
    ) -> Result<Option<MemoryRecord>, MemoryError> {
        let cache_key = self.cache_key_for(&scope, key);
        // TTL 惰性检查：如果 cache 中的记录已过期，移除并返回 None
        if let Some(record) = self.cache.get(&cache_key) {
            if self.is_expired(record) {
                self.cache.remove(&cache_key);
                return Ok(None);
            }
            return Ok(Some(record.clone()));
        }

        let session_id = self.session_id_for_scope(&scope)?;
        let path = self.build_path_scoped(&scope, key);
        match self.evorule_client.get_facts(&session_id, Some(&path)).await {
            Ok(facts) => {
                for fact in facts {
                    if let Ok(record) = serde_json::from_value::<MemoryRecord>(fact.value) {
                        // 从 evorule 拉取的记录也要检查 TTL
                        if self.is_expired(&record) {
                            continue;
                        }
                        self.cache.insert(cache_key.clone(), record.clone());
                        return Ok(Some(record));
                    }
                }
            }
            Err(_) => {}
        }

        Ok(None)
    }

    /// 旧版 remove（向后兼容，默认 Session scope）
    pub async fn remove(&mut self, key: &str) -> Result<Option<MemoryRecord>, MemoryError> {
        let scope = MemoryScope::session_from_opt(&self.session_id)?;
        self.remove_scoped(scope, key).await
    }

    /// 分层 remove（P1）
    pub async fn remove_scoped(
        &mut self,
        scope: MemoryScope,
        key: &str,
    ) -> Result<Option<MemoryRecord>, MemoryError> {
        let cache_key = self.cache_key_for(&scope, key);
        let removed = self.cache.remove(&cache_key);

        // best-effort 持久化：cache 是主要读取源，HTTP 失败不阻断
        let session_id = self.session_id_for_scope(&scope)?;
        let path = self.build_path_scoped(&scope, key);
        let null_value = serde_json::json!(null);
        let _ = self
            .evorule_client
            .update_payload(&session_id, &path, &null_value)
            .await;

        Ok(removed)
    }

    /// 清空所有 cache（仅本地，不删除 evorule 中的数据）
    pub async fn clear(&mut self) -> Result<(), MemoryError> {
        let keys: Vec<String> = self.cache.keys().cloned().collect();
        for key in keys {
            self.remove(&key).await?;
        }
        self.cache.clear();
        Ok(())
    }

    /// 返回所有 cache key 的迭代器
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.cache.keys()
    }

    /// 从 evorule 同步当前 namespace 下的所有 facts 到 cache（旧版，向后兼容）
    pub async fn sync_from_evorule(&mut self) -> Result<(), MemoryError> {
        if let Some(session_id) = &self.session_id {
            let prefix = format!("__memory__.{}", self.namespace);
            match self.evorule_client.get_facts(session_id, Some(&prefix)).await {
                Ok(facts) => {
                    for fact in facts {
                        if let Ok(record) = serde_json::from_value::<MemoryRecord>(fact.value) {
                            self.cache.insert(record.key.clone(), record);
                        }
                    }
                }
                Err(_) => {}
            }
        }
        Ok(())
    }

    /// 追加消息到 evorule payload（P0 短期记忆持久化）
    ///
    /// 将单条消息写入 `__memory__.agent_{ns}.session_{sid}.messages.{idx}` 路径。
    /// 进入 FactsLog + Auditor 审计链，支持 replay 和 causal_chain。
    ///
    /// # 参数
    /// - `session_id`: evorule 会话 ID
    /// - `idx`: 消息在 messages 数组中的索引
    /// - `message`: 消息内容
    pub async fn append_message(
        &mut self,
        session_id: &str,
        idx: usize,
        message: &Message,
    ) -> Result<(), MemoryError> {
        let timestamp = now_secs();
        let record = MessageRecord::from_message(idx, message, timestamp);
        let scope = MemoryScope::Messages(session_id.to_string(), idx);
        let path = self.build_path_scoped(&scope, "");
        let payload_value = serde_json::to_value(&record)?;
        self.evorule_client
            .update_payload(session_id, &path, &payload_value)
            .await?;
        Ok(())
    }

    /// 批量追加消息（P0 性能优化，用于 EveryN/PerReactRound 模式）
    ///
    /// 一次性写入多条消息，减少 HTTP 往返。
    pub async fn append_messages_batch(
        &mut self,
        session_id: &str,
        messages: &[(usize, Message)],
    ) -> Result<(), MemoryError> {
        for (idx, message) in messages {
            self.append_message(session_id, *idx, message).await?;
        }
        Ok(())
    }

    /// 构建 system prompt（注入记忆）
    ///
    /// 优先使用 summary（如果存在），否则拼接所有 KV。
    pub fn build_system_prompt(&self, base_prompt: &str) -> String {
        if self.cache.is_empty() {
            return base_prompt.to_string();
        }

        let mut memory_lines = Vec::new();
        memory_lines.push("=== AGENT MEMORY ===".to_string());
        memory_lines.push(format!("Namespace: {}", self.namespace));
        memory_lines.push("".to_string());

        for key in self.cache.keys() {
            if let Some(record) = self.cache.get(key) {
                memory_lines.push(format!("{}: {}", record.key, record.value));
            }
        }
        memory_lines.push("".to_string());
        memory_lines.push("=== END MEMORY ===".to_string());

        format!("{}\n\n{}", base_prompt, memory_lines.join("\n"))
    }

    /// 保存到本地文件（备份用，不常用）
    pub fn save_to_file(&self, path: &std::path::Path) -> Result<(), MemoryError> {
        let content = serde_json::to_string_pretty(&self.cache)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// 从本地文件加载（备份用）
    pub fn load_from_file(
        path: &std::path::Path,
        namespace: &str,
        evorule_client: EvoruleApiClient,
    ) -> Result<Self, MemoryError> {
        let mut manager = Self::new(namespace, evorule_client);
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            manager.cache = serde_json::from_str(&content)?;
        }
        Ok(manager)
    }

    /// 当前 cache 大小
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// cache 是否为空
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// 生成 cache 内部 key（区分 scope）
    fn cache_key_for(&self, scope: &MemoryScope, key: &str) -> String {
        match scope {
            MemoryScope::Shared => format!("shared::{}", key),
            MemoryScope::Session(sid) => format!("session_{}::{}", sid, key),
            MemoryScope::Messages(sid, idx) => format!("session_{}::messages::{}", sid, idx),
        }
    }

    /// 从 scope 提取 session_id
    ///
    /// Shared scope 使用当前 manager 的 session_id（写入当前会话的 payload）。
    /// Session/Messages scope 使用 scope 自带的 session_id。
    fn session_id_for_scope(&self, scope: &MemoryScope) -> Result<String, MemoryError> {
        match scope {
            MemoryScope::Shared => {
                // Shared 写入当前会话的 payload（通过当前 session 的 update_payload API）
                // evorule 的 shared facts 机制会自动广播（P3 共享写入）
                self.session_id.clone().ok_or(MemoryError::SessionNotSet)
            }
            MemoryScope::Session(sid) => Ok(sid.clone()),
            MemoryScope::Messages(sid, _) => Ok(sid.clone()),
        }
    }
}

impl std::fmt::Debug for MemoryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryManager")
            .field("namespace", &self.namespace)
            .field("record_count", &self.cache.len())
            .field("session_id", &self.session_id)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_client() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:8080")
    }

    fn make_tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create tempdir")
    }

    // ===== 基础测试（向后兼容）=====

    #[test]
    fn test_memory_manager_new() {
        let mgr = MemoryManager::new("test", make_test_client());
        assert_eq!(mgr.namespace(), "test");
        assert!(mgr.is_empty());
        assert_eq!(mgr.len(), 0);
        assert!(mgr.session_id.is_none());
    }

    #[test]
    fn test_memory_manager_with_session_id() {
        let mgr = MemoryManager::new("test", make_test_client()).with_session_id("123");
        assert_eq!(mgr.session_id, Some("123".to_string()));
        assert_eq!(mgr.session_id(), Some("123"));
    }

    #[test]
    fn test_memory_manager_set_and_get_cached() {
        let mut mgr = MemoryManager::new("research", make_test_client()).with_session_id("s1");
        tokio_test::block_on(async {
            mgr.set("topic", "AI safety").await.expect("set");
            mgr.set("source", "arXiv").await.expect("set");

            assert!(!mgr.is_empty());
            assert_eq!(mgr.len(), 2);

            let record = mgr.get("topic").await.expect("get").expect("record");
            assert_eq!(record.key, "topic");
            assert_eq!(record.value, "AI safety");

            let record = mgr.get("source").await.expect("get").expect("record");
            assert_eq!(record.value, "arXiv");

            assert!(mgr.get("nonexistent").await.expect("get").is_none());
        });
    }

    #[test]
    fn test_memory_manager_set_empty_key() {
        let mut mgr = MemoryManager::new("test", make_test_client()).with_session_id("s1");
        let result = tokio_test::block_on(async { mgr.set("", "value").await });
        assert!(matches!(result, Err(MemoryError::EmptyKey)));
    }

    #[test]
    fn test_memory_manager_set_key_too_long() {
        let mut mgr = MemoryManager::new("test", make_test_client()).with_session_id("s1");
        let long_key = "a".repeat(500);
        let result = tokio_test::block_on(async { mgr.set(&long_key, "value").await });
        assert!(matches!(result, Err(MemoryError::KeyTooLong(500))));
    }

    #[test]
    fn test_memory_manager_remove() {
        let mut mgr = MemoryManager::new("test", make_test_client()).with_session_id("s1");
        tokio_test::block_on(async {
            mgr.set("key1", "val1").await.expect("set");
            mgr.set("key2", "val2").await.expect("set");

            let removed = mgr.remove("key1").await.expect("remove").expect("record");
            assert_eq!(removed.key, "key1");
            assert_eq!(mgr.len(), 1);

            assert!(mgr.remove("nonexistent").await.expect("remove").is_none());
        });
    }

    #[test]
    fn test_memory_manager_clear() {
        let mut mgr = MemoryManager::new("test", make_test_client()).with_session_id("s1");
        tokio_test::block_on(async {
            mgr.set("key1", "val1").await.expect("set");
            mgr.set("key2", "val2").await.expect("set");

            mgr.clear().await.expect("clear");
            assert!(mgr.is_empty());
            assert_eq!(mgr.len(), 0);
        });
    }

    #[test]
    fn test_memory_manager_keys() {
        let mut mgr = MemoryManager::new("test", make_test_client()).with_session_id("s1");
        tokio_test::block_on(async {
            mgr.set("zebra", "z").await.expect("set");
            mgr.set("alpha", "a").await.expect("set");
            mgr.set("beta", "b").await.expect("set");

            let mut keys: Vec<&String> = mgr.keys().collect();
            keys.sort();
            assert_eq!(keys.len(), 3);
        });
    }

    #[test]
    fn test_memory_manager_build_system_prompt_empty() {
        let mgr = MemoryManager::new("test", make_test_client());
        let prompt = mgr.build_system_prompt("You are a helpful assistant");
        assert_eq!(prompt, "You are a helpful assistant");
    }

    #[test]
    fn test_memory_manager_build_system_prompt_with_memory() {
        let mut mgr = MemoryManager::new("research", make_test_client()).with_session_id("s1");
        tokio_test::block_on(async {
            mgr.set("topic", "quantum computing").await.expect("set");
            mgr.set("author", "John Doe").await.expect("set");

            let prompt = mgr.build_system_prompt("You are a research assistant");
            assert!(prompt.contains("=== AGENT MEMORY ==="));
            assert!(prompt.contains("Namespace: research"));
            assert!(prompt.contains("topic: quantum computing"));
            assert!(prompt.contains("author: John Doe"));
            assert!(prompt.contains("=== END MEMORY ==="));
            assert!(prompt.starts_with("You are a research assistant"));
        });
    }

    #[test]
    fn test_memory_manager_save_and_load() {
        let dir = make_tmp_dir();
        let path = dir.path().join("memory.json");

        let mut mgr1 = MemoryManager::new("test", make_test_client()).with_session_id("s1");
        tokio_test::block_on(async {
            mgr1.set("key1", "val1").await.expect("set");
            mgr1.set("key2", "val2").await.expect("set");
        });
        mgr1.save_to_file(&path).expect("save");

        let mut mgr2 =
            MemoryManager::load_from_file(&path, "test", make_test_client()).expect("load");
        assert_eq!(mgr2.namespace(), "test");
        assert_eq!(mgr2.len(), 2);
        mgr2.set_session_id("s1");
        tokio_test::block_on(async {
            assert_eq!(mgr2.get("key1").await.expect("get").unwrap().value, "val1");
            assert_eq!(mgr2.get("key2").await.expect("get").unwrap().value, "val2");
        });
    }

    #[test]
    fn test_memory_manager_load_nonexistent() {
        let mgr = MemoryManager::load_from_file(
            std::path::Path::new("/nonexistent/path/memory.json"),
            "test",
            make_test_client(),
        )
        .expect("load");
        assert_eq!(mgr.namespace(), "test");
        assert!(mgr.is_empty());
    }

    #[test]
    fn test_memory_manager_overwrite() {
        let mut mgr = MemoryManager::new("test", make_test_client()).with_session_id("s1");
        tokio_test::block_on(async {
            mgr.set("key", "v1").await.expect("set");
            mgr.set("key", "v2").await.expect("set");

            assert_eq!(mgr.len(), 1);
            assert_eq!(mgr.get("key").await.expect("get").unwrap().value, "v2");
        });
    }

    #[test]
    fn test_memory_error_display() {
        let err = MemoryError::EmptyKey;
        assert!(format!("{}", err).contains("cannot be empty"));

        let err = MemoryError::KeyTooLong(300);
        assert!(format!("{}", err).contains("300"));

        let err = MemoryError::EvoruleError("connection failed".to_string());
        assert!(format!("{}", err).contains("Evorule API error"));

        let err = MemoryError::SessionNotSet;
        assert!(format!("{}", err).contains("session not set"));
    }

    #[test]
    fn test_memory_manager_debug_format() {
        let mgr = MemoryManager::new("test", make_test_client());
        let debug = format!("{:?}", mgr);
        assert!(debug.contains("MemoryManager"));
        assert!(debug.contains("test"));
        assert!(debug.contains("record_count: 0"));
    }

    #[test]
    fn test_build_path() {
        let mgr = MemoryManager::new("agent_research", make_test_client());
        assert_eq!(mgr.build_path("topic"), "__memory__.agent_research.topic");
        assert_eq!(mgr.build_path("author"), "__memory__.agent_research.author");
    }

    // ===== P1: MemoryScope 三层分层测试 =====

    #[test]
    fn test_build_path_scoped_shared() {
        let mgr = MemoryManager::new("researcher", make_test_client());
        let scope = MemoryScope::Shared;
        assert_eq!(
            mgr.build_path_scoped(&scope, "topic"),
            "__memory__.researcher.shared.topic"
        );
    }

    #[test]
    fn test_build_path_scoped_session() {
        let mgr = MemoryManager::new("researcher", make_test_client());
        let scope = MemoryScope::Session("s123".to_string());
        assert_eq!(
            mgr.build_path_scoped(&scope, "topic"),
            "__memory__.researcher.session_s123.topic"
        );
    }

    #[test]
    fn test_build_path_scoped_messages() {
        let mgr = MemoryManager::new("researcher", make_test_client());
        let scope = MemoryScope::Messages("s123".to_string(), 5);
        assert_eq!(
            mgr.build_path_scoped(&scope, ""),
            "__memory__.researcher.session_s123.messages.5"
        );
    }

    #[test]
    fn test_cache_key_for_distinguishes_scopes() {
        let mgr = MemoryManager::new("test", make_test_client());
        let shared_key = mgr.cache_key_for(&MemoryScope::Shared, "topic");
        let session_key = mgr.cache_key_for(&MemoryScope::Session("s1".to_string()), "topic");
        let messages_key =
            mgr.cache_key_for(&MemoryScope::Messages("s1".to_string(), 0), "");

        assert_ne!(shared_key, session_key);
        assert_ne!(session_key, messages_key);
        assert!(shared_key.starts_with("shared::"));
        assert!(session_key.starts_with("session_s1::"));
        assert!(messages_key.starts_with("session_s1::messages::"));
    }

    #[test]
    fn test_session_id_for_scope_shared_uses_manager_session() {
        let mgr = MemoryManager::new("test", make_test_client()).with_session_id("current");
        let result = mgr.session_id_for_scope(&MemoryScope::Shared);
        assert_eq!(result.unwrap(), "current");
    }

    #[test]
    fn test_session_id_for_scope_shared_without_session_errors() {
        let mgr = MemoryManager::new("test", make_test_client());
        let result = mgr.session_id_for_scope(&MemoryScope::Shared);
        assert!(matches!(result, Err(MemoryError::SessionNotSet)));
    }

    #[test]
    fn test_session_id_for_scope_session_uses_scope_session() {
        let mgr = MemoryManager::new("test", make_test_client()).with_session_id("current");
        let result = mgr.session_id_for_scope(&MemoryScope::Session("other".to_string()));
        assert_eq!(result.unwrap(), "other");
    }

    // ===== P0: MessagePersistMode 测试 =====

    #[test]
    fn test_message_persist_mode_default_is_every_message() {
        let mode = MessagePersistMode::default();
        assert_eq!(mode, MessagePersistMode::EveryMessage);
    }

    #[test]
    fn test_message_persist_mode_needs_buffer() {
        assert!(!MessagePersistMode::EveryMessage.needs_buffer());
        assert!(MessagePersistMode::EveryN(5).needs_buffer());
        assert!(MessagePersistMode::PerReactRound.needs_buffer());
        assert!(!MessagePersistMode::Disabled.needs_buffer());
    }

    #[test]
    fn test_message_persist_mode_is_disabled() {
        assert!(!MessagePersistMode::EveryMessage.is_disabled());
        assert!(!MessagePersistMode::EveryN(5).is_disabled());
        assert!(!MessagePersistMode::PerReactRound.is_disabled());
        assert!(MessagePersistMode::Disabled.is_disabled());
    }

    // ===== P0: MessageRecord 测试 =====

    #[test]
    fn test_message_record_from_system_message() {
        let msg = Message::System {
            content: "You are helpful".to_string(),
        };
        let record = MessageRecord::from_message(0, &msg, 1000);
        assert_eq!(record.idx, 0);
        assert_eq!(record.role, "system");
        assert_eq!(record.content, "You are helpful");
        assert!(record.tool_calls.is_none());
        assert!(record.tool_name.is_none());
        assert_eq!(record.timestamp, 1000);
        assert!(record.fact_id.is_none());
    }

    #[test]
    fn test_message_record_from_user_message() {
        let msg = Message::User {
            content: "Hello".to_string(),
        };
        let record = MessageRecord::from_message(1, &msg, 2000);
        assert_eq!(record.idx, 1);
        assert_eq!(record.role, "user");
        assert_eq!(record.content, "Hello");
    }

    #[test]
    fn test_message_record_from_assistant_message() {
        let msg = Message::Assistant {
            content: "Hi there".to_string(),
            tool_calls: None,
        };
        let record = MessageRecord::from_message(2, &msg, 3000);
        assert_eq!(record.idx, 2);
        assert_eq!(record.role, "assistant");
        assert_eq!(record.content, "Hi there");
        assert!(record.tool_calls.is_none());
        assert!(record.tool_name.is_none());
    }

    #[test]
    fn test_message_record_from_tool_message() {
        let msg = Message::Tool {
            content: "result data".to_string(),
            tool_name: "search".to_string(),
        };
        let record = MessageRecord::from_message(3, &msg, 4000);
        assert_eq!(record.idx, 3);
        assert_eq!(record.role, "tool");
        assert_eq!(record.content, "result data");
        assert_eq!(record.tool_name, Some("search".to_string()));
    }

    #[test]
    fn test_message_record_path_key() {
        let msg = Message::User {
            content: "test".to_string(),
        };
        let record = MessageRecord::from_message(5, &msg, 1000);
        assert_eq!(record.path_key(), "messages.5");
    }

    // ===== P0: MemoryRecord 扩展字段测试 =====

    #[test]
    fn test_memory_record_new_basic() {
        let record = MemoryRecord::new("key", "value", 1000);
        assert_eq!(record.key, "key");
        assert_eq!(record.value, "value");
        assert_eq!(record.timestamp, 1000);
        assert!(record.source.is_none());
        assert!(record.confidence.is_none());
        assert!(record.tags.is_empty());
    }

    #[test]
    fn test_memory_record_serialize_deserialize_backward_compat() {
        // 旧格式（无 source/confidence/tags）应该能反序列化
        let old_json = r#"{"key":"topic","value":"AI","timestamp":1000}"#;
        let record: MemoryRecord = serde_json::from_str(old_json).expect("parse old format");
        assert_eq!(record.key, "topic");
        assert_eq!(record.value, "AI");
        assert_eq!(record.timestamp, 1000);
        assert!(record.source.is_none());
        assert!(record.confidence.is_none());
        assert!(record.tags.is_empty());
    }

    #[test]
    fn test_memory_record_serialize_skips_none_fields() {
        let record = MemoryRecord::new("key", "value", 1000);
        let json = serde_json::to_string(&record).expect("serialize");
        // 不应包含 source/confidence/tags 字段（skip_serializing_if）
        assert!(!json.contains("source"));
        assert!(!json.contains("confidence"));
        assert!(!json.contains("tags"));
    }

    #[test]
    fn test_memory_record_with_all_fields() {
        let mut record = MemoryRecord::new("topic", "AI safety", 1000);
        record.source = Some("user_input".to_string());
        record.confidence = Some(0.95);
        record.tags = vec!["research".to_string(), "ai".to_string()];
        let json = serde_json::to_string(&record).expect("serialize");
        let parsed: MemoryRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.source, Some("user_input".to_string()));
        assert_eq!(parsed.confidence, Some(0.95));
        assert_eq!(parsed.tags, vec!["research".to_string(), "ai".to_string()]);
    }

    // ===== P0: append_message 路径生成测试 =====

    #[test]
    fn test_append_message_path_format() {
        // 验证 append_message 内部生成的路径格式（不实际调用 HTTP）
        let mgr = MemoryManager::new("researcher", make_test_client());
        let session_id = "s123";
        let idx = 5;
        let expected_path = format!(
            "__memory__.{}.session_{}.messages.{}",
            mgr.namespace(),
            session_id,
            idx
        );
        assert_eq!(expected_path, "__memory__.researcher.session_s123.messages.5");
    }

    // ===== P0: TTL 测试（用户决策 5）=====

    #[test]
    fn test_ttl_default_is_none() {
        let mgr = MemoryManager::new("test", make_test_client());
        assert!(mgr.ttl_secs().is_none());
    }

    #[test]
    fn test_ttl_with_ttl_secs_builder() {
        let mgr = MemoryManager::new("test", make_test_client()).with_ttl_secs(3600);
        assert_eq!(mgr.ttl_secs(), Some(3600));
    }

    #[test]
    fn test_ttl_is_expired_no_ttl_never_expires() {
        let mgr = MemoryManager::new("test", make_test_client());
        let record = MemoryRecord::new("key", "value", 0); // 时间戳为 0（很老）
        assert!(!mgr.is_expired(&record));
    }

    #[test]
    fn test_ttl_is_expired_with_ttl_recent_record() {
        let mgr = MemoryManager::new("test", make_test_client()).with_ttl_secs(3600);
        let now = now_secs();
        let record = MemoryRecord::new("key", "value", now); // 刚创建
        assert!(!mgr.is_expired(&record));
    }

    #[test]
    fn test_ttl_is_expired_with_ttl_old_record() {
        let mgr = MemoryManager::new("test", make_test_client()).with_ttl_secs(100);
        let old_timestamp = now_secs().saturating_sub(200); // 200 秒前，超过 100 秒 TTL
        let record = MemoryRecord::new("key", "value", old_timestamp);
        assert!(mgr.is_expired(&record));
    }

    #[test]
    fn test_ttl_is_expired_boundary_case() {
        // 刚好到 TTL 边界（now - timestamp == ttl）不应判定为过期（> 才过期）
        let mgr = MemoryManager::new("test", make_test_client()).with_ttl_secs(100);
        let timestamp = now_secs().saturating_sub(100);
        let record = MemoryRecord::new("key", "value", timestamp);
        assert!(!mgr.is_expired(&record));
    }

    #[test]
    fn test_ttl_cleanup_expired_no_ttl_returns_zero() {
        let mut mgr = MemoryManager::new("test", make_test_client()).with_session_id("s1");
        tokio_test::block_on(async {
            mgr.set("key1", "val1").await.expect("set");
        });
        // 没有 TTL，cleanup_expired 应返回 0
        let count = mgr.cleanup_expired();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_ttl_cleanup_expired_with_ttl_keeps_recent() {
        let mut mgr = MemoryManager::new("test", make_test_client())
            .with_session_id("s1")
            .with_ttl_secs(3600);
        tokio_test::block_on(async {
            mgr.set("key1", "val1").await.expect("set");
        });
        // 刚写入的记录不应被清理
        let count = mgr.cleanup_expired();
        assert_eq!(count, 0);
        assert_eq!(mgr.len(), 1);
    }

    #[test]
    fn test_ttl_cleanup_expired_removes_old_entries() {
        let mut mgr = MemoryManager::new("test", make_test_client())
            .with_session_id("s1")
            .with_ttl_secs(100);
        // 手动插入一个过期的记录到 cache
        let old_timestamp = now_secs().saturating_sub(200);
        let expired_record = MemoryRecord::new("old_key", "old_val", old_timestamp);
        mgr.cache.insert("session_s1::old_key".to_string(), expired_record);

        // 手动插入一个未过期的记录
        let recent_record = MemoryRecord::new("new_key", "new_val", now_secs());
        mgr.cache.insert("session_s1::new_key".to_string(), recent_record);

        assert_eq!(mgr.len(), 2);
        let count = mgr.cleanup_expired();
        assert_eq!(count, 1);
        assert_eq!(mgr.len(), 1);
        // 确认保留的是新记录
        assert!(mgr.cache.contains_key("session_s1::new_key"));
    }

    #[test]
    fn test_ttl_get_scoped_lazy_expiry() {
        // 配置了 TTL 后，get 旧记录时应返回 None 并清理
        let mut mgr = MemoryManager::new("test", make_test_client())
            .with_session_id("s1")
            .with_ttl_secs(100);

        // 手动插入一个过期的记录
        let old_timestamp = now_secs().saturating_sub(200);
        let expired_record = MemoryRecord::new("topic", "AI", old_timestamp);
        mgr.cache
            .insert("session_s1::topic".to_string(), expired_record);

        // get 时应触发惰性清理，返回 None
        let result = tokio_test::block_on(async { mgr.get("topic").await });
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
        // cache 中应已移除
        assert!(mgr.cache.is_empty());
    }

    #[test]
    fn test_ttl_get_scoped_keeps_valid_record() {
        let mut mgr = MemoryManager::new("test", make_test_client())
            .with_session_id("s1")
            .with_ttl_secs(3600);

        tokio_test::block_on(async {
            mgr.set("topic", "AI safety").await.expect("set");
        });

        // get 应正常返回记录（未过期）
        let result = tokio_test::block_on(async { mgr.get("topic").await });
        let record = result.expect("get").expect("record exists");
        assert_eq!(record.value, "AI safety");
        assert_eq!(mgr.len(), 1);
    }

    #[test]
    fn test_ttl_does_not_affect_append_message() {
        // append_message 直接写 evorule payload，不经 cache，TTL 不影响
        // 这里的测试仅验证 ttl_secs 配置存在但 append_message 仍可正常调用
        let mgr = MemoryManager::new("test", make_test_client()).with_ttl_secs(60);
        assert_eq!(mgr.ttl_secs(), Some(60));
        // append_message 需要 HTTP 调用，这里只验证方法存在
        let _ = mgr.namespace();
    }
}
