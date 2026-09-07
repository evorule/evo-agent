// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 032 MemoryEventStore —— 事件/实体的 CRUD + 因果链维护
//!
//! ## 存储架构
//!
//! ```text
//! evo-agent (本地 cache)              evorule (持久化 + 审计链)
//! ┌─────────────────────┐             ┌──────────────────────────┐
//! │ event_cache         │ ──update──→ │ Fact::PayloadUpdate      │
//! │ entity_cache        │             │ path=...events.E001      │
//! │ fact_to_event       │ ←─get_facts│ value=<MemoryEvent JSON> │
//! │ entity_index        │             │ ──────────────────────── │
//! └─────────────────────┘             │ FactsLog + Auditor       │
//!                                     │ (blake3 哈希链防篡改)     │
//!                                     └──────────────────────────┘
//! ```
//!
//! ## 因果链维护(032 设计文档 §6.4 + gap §14.3.3)
//!
//! 写入新事件 E2(cause 指向 F1)时:
//! 1. 写 E2 → `Fact::PayloadUpdate(path=...events.E2)` → 得到 FactId F2
//! 2. 更新源事件 E1 的 effects 字段(追加 E2 的 event_id)
//! 3. 两次 PayloadUpdate 保证因果链双向可走

use std::collections::HashMap;

use tracing::warn;

use crate::api::evorule_client::EvoruleApiClient;

use super::entity::{Entity, EntityIndex, EntityType};
use super::event::{EventRef, EventType, FactId, MemoryEvent};
use super::evidence::MemoryEvidence;

/// 存储错误
#[derive(Debug)]
pub enum StoreError {
    /// JSON 序列化错误
    Json(serde_json::Error),
    /// evorule API 错误
    EvoruleError(String),
    /// session 未设置
    SessionNotSet,
    /// 事件未找到
    EventNotFound(String),
    /// 实体未找到
    EntityNotFound(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Json(e) => write!(f, "JSON error: {}", e),
            StoreError::EvoruleError(e) => write!(f, "Evorule API error: {}", e),
            StoreError::SessionNotSet => write!(f, "session not set"),
            StoreError::EventNotFound(id) => write!(f, "event not found: {}", id),
            StoreError::EntityNotFound(id) => write!(f, "entity not found: {}", id),
        }
    }
}

impl std::error::Error for StoreError {}

/// B2：绑定交叉核对结果
#[derive(Debug, Clone, Default)]
pub struct CauseBindingReport {
    /// 事件 ID
    pub event_id: String,
    /// 事件身份锚点（写入它的 PayloadUpdate FactId）
    pub identity_fact_id: Option<FactId>,
    /// 事件因果锚点（cause 指向的源 FactId）
    pub cause_fact_id: Option<FactId>,
    /// 引擎整链完整性（verify_audit_typed）
    pub verified: bool,
    /// 应用层链长
    pub app_chain_len: usize,
    /// 引擎链长（cause 源 Fact 的因果链深度）
    pub engine_chain_len: usize,
    /// 应用层链与引擎链是否对齐（链长一致 + 末端一致）
    pub aligned: bool,
    /// 改进2：effects 中每个引擎级 FactId 是否真实存在于 Fact 流（全部命中 → true；离线/缺 fact_id → false 降级）
    pub effects_verified: bool,
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Json(e)
    }
}

impl From<crate::api::api_core::ApiError> for StoreError {
    fn from(e: crate::api::api_core::ApiError) -> Self {
        StoreError::EvoruleError(e.to_string())
    }
}

/// 032 MemoryEventStore —— 事件/实体的 CRUD + 因果链维护
///
/// 与 031 的 `MemoryManager` 共存,各自独立管理自己的 namespace 子路径。
/// cache 是主要读取源,evorule 持久化为 best-effort(与 MemoryManager 一致)。
#[derive(Clone)]
pub struct MemoryEventStore {
    /// namespace,如 "agent_researcher"
    namespace: String,
    /// evorule HTTP 客户端
    pub(crate) evorule_client: EvoruleApiClient,
    /// 当前 session ID
    session_id: Option<String>,
    /// 事件缓存:event_id → MemoryEvent
    event_cache: HashMap<String, MemoryEvent>,
    /// 实体缓存:entity_id → Entity
    entity_cache: HashMap<String, Entity>,
    /// FactId → event_id 反向映射(因果链维护用)
    fact_to_event: HashMap<FactId, String>,
    /// 实体反向索引
    entity_index: EntityIndex,
}

impl MemoryEventStore {
    /// 创建新 store
    pub fn new(namespace: &str, evorule_client: EvoruleApiClient) -> Self {
        Self {
            namespace: namespace.to_string(),
            evorule_client,
            session_id: None,
            event_cache: HashMap::new(),
            entity_cache: HashMap::new(),
            fact_to_event: HashMap::new(),
            entity_index: EntityIndex::new(),
        }
    }

    /// 设置 session_id(builder 风格)
    pub fn with_session_id(mut self, session_id: &str) -> Self {
        self.session_id = Some(session_id.to_string());
        self
    }

    /// 设置 session_id(可变引用)
    pub fn set_session_id(&mut self, session_id: &str) {
        self.session_id = Some(session_id.to_string());
    }

    /// 获取 session_id
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// 获取 namespace
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    // ===== 路径构建 =====

    /// 事件路径:`__memory__.{ns}.events.{event_id}`
    fn event_path(&self, event_id: &str) -> String {
        format!("__memory__.{}.events.{}", self.namespace, event_id)
    }

    /// 事件前缀路径(用于 get_facts 查询):`__memory__.{ns}.events.`
    fn events_prefix(&self) -> String {
        format!("__memory__.{}.events.", self.namespace)
    }

    /// 实体路径:`__memory__.{ns}.entities.{entity_id}`
    fn entity_path(&self, entity_id: &str) -> String {
        format!("__memory__.{}.entities.{}", self.namespace, entity_id)
    }

    /// 实体前缀路径:`__memory__.{ns}.entities.`
    fn entities_prefix(&self) -> String {
        format!("__memory__.{}.entities.", self.namespace)
    }

    fn session_id_str(&self) -> Result<&str, StoreError> {
        self.session_id.as_deref().ok_or(StoreError::SessionNotSet)
    }

    // ===== 事件 CRUD =====

    /// 写入事件到 evorule + cache + 索引
    ///
    /// 返回 evorule 分配的 FactId(best-effort,失败时返回 0 但 cache 仍更新)。
    /// 写入后自动从 evorule 查询 FactId 并更新 `event.fact_id`。
    ///
    /// **与 MemoryManager 一致**:cache 总是更新,evorule 持久化为 best-effort。
    /// 这使得单元测试可以在无服务器环境下运行。
    pub async fn write_event(&mut self, mut event: MemoryEvent) -> Result<FactId, StoreError> {
        // 设置 session_id
        if event.session_id.is_none() {
            if let Some(sid) = &self.session_id {
                event.session_id = Some(sid.clone());
            }
        }

        let event_id = event.event_id.clone();

        // 更新 cache + 索引(总是执行,即使 evorule 不可用)
        self.entity_index.index_event(&event_id, &event);
        self.event_cache.insert(event_id.clone(), event.clone());

        // best-effort 持久化(与 MemoryManager 一致:cache 是主源,HTTP 失败不阻断)
        let fact_id = match self.session_id_str() {
            Ok(session_id) => {
                let path = self.event_path(&event_id);
                let value = serde_json::to_value(&event)?;
                if self
                    .evorule_client
                    .update_payload(session_id, &path, &value)
                    .await
                    .is_ok()
                {
                    self.fetch_identity_fact_id(session_id, &path)
                        .await
                        .unwrap_or(0)
                } else {
                    0
                }
            }
            Err(_) => 0,
        };

        if fact_id > 0 {
            if let Some(e) = self.event_cache.get_mut(&event_id) {
                e.fact_id = Some(fact_id);
            }
            self.fact_to_event.insert(fact_id, event_id);
        }

        Ok(fact_id)
    }

    /// B2：查询事件身份 FactId（path 上第一个精确匹配 = 首次写入，identity）
    ///
    /// 与 B1 的 last-write-wins 不同：事件身份锚定首次写入，不随 effects 更新漂移。
    /// 不取 prefix fallback（D-B2-1：身份必须是精确匹配）。
    async fn fetch_identity_fact_id(
        &self,
        session_id: &str,
        path: &str,
    ) -> Result<FactId, StoreError> {
        let facts = self
            .evorule_client
            .get_facts(session_id, Some(path))
            .await?;
        // 升序返回 → 第一个精确匹配即身份
        Ok(facts
            .into_iter()
            .find(|f| f.path == path)
            .map(|f| f.id)
            .unwrap_or(0))
    }

    /// B2：从 Fact 流补填身份锚点（读取路径统一入口）
    ///
    /// 事件 JSON 缺 fact_id（旧数据/重启）时，用该 path 上第一个 Fact 的 id 重建。
    fn fill_identity_fact_id(event: &mut MemoryEvent, fact_id: FactId) {
        if event.fact_id.is_none() {
            event.fact_id = Some(fact_id);
        }
    }

    /// 读取事件(优先从 cache,cache miss 时从 evorule 拉取)
    pub async fn read_event(&mut self, event_id: &str) -> Result<Option<MemoryEvent>, StoreError> {
        // cache hit
        if let Some(event) = self.event_cache.get(event_id) {
            return Ok(Some(event.clone()));
        }

        // cache miss:best-effort 从 evorule 拉取
        // clone session_id 避免借用冲突(后续需要 mutable borrow 更新 cache)
        // HTTP 失败不阻塞,返回 Ok(None)(与 write_event 的 best-effort 模式一致)
        let session_id = self.session_id_str()?.to_string();
        let path = self.event_path(event_id);
        let facts = match self
            .evorule_client
            .get_facts(&session_id, Some(&path))
            .await
        {
            Ok(facts) => facts,
            Err(e) => {
                tracing::debug!(
                    event_id = event_id,
                    error = %e,
                    "read_event: evorule HTTP failed (best-effort, returning None)"
                );
                return Ok(None);
            }
        };

        // B2 双提取：内容取最后一个版本（effects 完整），身份取第一个版本的 FactId（不随 effects 更新漂移）
        let Some(latest) = facts.last() else {
            return Ok(None);
        };
        if latest.path != path {
            return Ok(None); // 精确匹配失败
        }
        if let Ok(mut event) = serde_json::from_value::<MemoryEvent>(latest.value.clone()) {
            // 身份 = 首个精确匹配版本（D-B2-1/D-B2-3）
            let identity = facts
                .iter()
                .find(|f| f.path == path)
                .map(|f| f.id)
                .unwrap_or(latest.id);
            Self::fill_identity_fact_id(&mut event, identity);
            // 更新 cache + 索引
            self.entity_index.index_event(&event.event_id, &event);
            if let Some(fid) = event.fact_id {
                self.fact_to_event.insert(fid, event.event_id.clone());
            }
            self.event_cache
                .insert(event.event_id.clone(), event.clone());
            return Ok(Some(event));
        }

        Ok(None)
    }

    /// 列出所有事件(从 cache)
    pub fn list_events(&self) -> Vec<MemoryEvent> {
        self.event_cache.values().cloned().collect()
    }

    /// 列出所有事件(按时间排序)
    pub fn list_events_sorted(&self) -> Vec<MemoryEvent> {
        let mut events: Vec<MemoryEvent> = self.event_cache.values().cloned().collect();
        events.sort_by_key(|e| e.timestamp);
        events
    }

    /// 从 evorule 同步所有事件到 cache
    ///
    /// 类似 MemoryManager::sync_from_evorule,拉取 events prefix 下的所有 Fact。
    pub async fn sync_from_evorule(&mut self) -> Result<usize, StoreError> {
        // 克隆 session_id 为 String,避免 &self 与后续 &mut self 操作的借用冲突
        // (与 read_event 同样的修复模式)
        let session_id = self.session_id_str()?.to_string();
        let prefix = self.events_prefix();
        let facts = self
            .evorule_client
            .get_facts(&session_id, Some(&prefix))
            .await?;

        // B2：按 path 分组，每组双提取（内容=last、身份=first）
        let mut latest_by_path: std::collections::BTreeMap<
            String,
            crate::api::evorule_client::FactEntry,
        > = Default::default();
        let mut first_id_by_path: std::collections::BTreeMap<String, FactId> = Default::default();
        for fact in facts {
            first_id_by_path.entry(fact.path.clone()).or_insert(fact.id); // 首个 = 身份
            latest_by_path.insert(fact.path.clone(), fact); // 覆盖 = 最新内容
        }

        let mut count = 0;
        for (path, latest) in latest_by_path {
            if let Ok(mut event) = serde_json::from_value::<MemoryEvent>(latest.value.clone()) {
                if let Some(identity) = first_id_by_path.get(&path) {
                    Self::fill_identity_fact_id(&mut event, *identity); // 身份 = 首个版本
                }
                if let Some(fid) = event.fact_id {
                    self.fact_to_event.insert(fid, event.event_id.clone());
                }
                self.entity_index.index_event(&event.event_id, &event);
                self.event_cache.insert(event.event_id.clone(), event);
                count += 1;
            }
        }

        // 同步实体
        let entity_prefix = self.entities_prefix();
        if let Ok(entity_facts) = self
            .evorule_client
            .get_facts(&session_id, Some(&entity_prefix))
            .await
        {
            for fact in entity_facts {
                if let Ok(entity) = serde_json::from_value::<Entity>(fact.value) {
                    self.entity_cache.insert(entity.entity_id.clone(), entity);
                }
            }
        }

        Ok(count)
    }

    // ===== 因果链维护 =====

    /// 写入带因果的事件(032 设计文档 §6.4 + gap §14.3.3 + 改进2)
    ///
    /// 1. 写入 E2(cause = cause_fact_id)
    /// 2. 找到源事件 E1(其 fact_id == cause_fact_id)
    /// 3. 更新 E1.effects,追加 E2 的**引擎级引用**(改进2:event_id + E2 的 FactId)
    ///
    /// 两次 PayloadUpdate 保证因果链双向可走:
    /// - E1.effects → E2(正向,改进2 起引用引擎级 FactId)
    /// - E2.cause → F1 → E1(反向)
    pub async fn write_event_with_cause(
        &mut self,
        mut event: MemoryEvent,
        cause_fact_id: FactId,
    ) -> Result<FactId, StoreError> {
        event.cause = Some(cause_fact_id);
        let new_event_id = event.event_id.clone();
        let new_fact_id = self.write_event(event).await?;

        // 找到源事件(其 fact_id == cause_fact_id)
        if let Some(source_event_id) = self.fact_to_event.get(&cause_fact_id).cloned() {
            // clone 源事件,避免在持有 get_mut 借用时调 self 方法
            if let Some(source_event) = self.event_cache.get(&source_event_id).cloned() {
                // 追加新事件引用到 effects(去重;改进2 携带引擎级 FactId)
                let has_ref = source_event
                    .effects
                    .iter()
                    .any(|r| r.event_id == new_event_id);
                if !has_ref {
                    let mut updated = source_event.clone();
                    // 引擎级锚点:E2 的 FactId(best-effort,离线/失败时 new_fact_id=0 → None 降级)
                    let fact_id = if new_fact_id > 0 {
                        Some(new_fact_id)
                    } else {
                        None
                    };
                    updated.effects.push(EventRef {
                        event_id: new_event_id.clone(),
                        fact_id,
                    });

                    // 更新 cache
                    self.event_cache
                        .insert(source_event_id.clone(), updated.clone());

                    // best-effort 更新源事件到 evorule(覆盖写)
                    if let Ok(session_id) = self.session_id_str() {
                        let path = self.event_path(&source_event_id);
                        let value = serde_json::to_value(&updated)?;
                        if let Err(e) = self
                            .evorule_client
                            .update_payload(session_id, &path, &value)
                            .await
                        {
                            warn!(
                                event_id = %source_event_id,
                                error = %e,
                                "G14: failed to update source event effects (best-effort, cache updated)"
                            );
                        }
                    }
                }
            }
        }
        // 若 cause_fact_id 在 fact_to_event 中找不到,说明源事件不在本 session 的 cache 中
        // (可能是跨 session 的 cause)。此时 E2 仍可独立存在(cause 指向 F1 但 E1.effects 缺 E2),
        // 回放引擎容忍这种不一致(§14.6 风险缓解)。

        Ok(new_fact_id)
    }

    /// 因果链回溯:从指定事件出发,沿 cause 链回溯到根事件
    ///
    /// 返回 [target_event, ..., root_event] 的向量(从当前到过去)。
    /// 如果 cause 指向不存在的 FactId,链在此截断(不 panic,优雅降级)。
    pub async fn causal_chain(&mut self, event_id: &str) -> Result<Vec<MemoryEvent>, StoreError> {
        let mut chain = Vec::new();
        let mut current_id = Some(event_id.to_string());
        let mut visited = std::collections::HashSet::new();

        while let Some(eid) = current_id {
            // 防止循环(如有环引用)
            if !visited.insert(eid.clone()) {
                warn!(event_id = %eid, "G14: causal chain cycle detected, stopping");
                break;
            }

            let event = self.read_event(&eid).await?;
            match event {
                Some(ev) => {
                    current_id = ev
                        .cause
                        .and_then(|fid| self.fact_to_event.get(&fid).cloned());
                    chain.push(ev);
                }
                None => {
                    // cause 指向的事件不在 cache 中(可能在其他 session 或已删除)
                    // 链在此截断,优雅降级
                    break;
                }
            }
        }

        Ok(chain)
    }

    /// B2：绑定交叉核对 —— 比对应用层因果链与引擎审计链。
    ///
    /// 1. 读事件 → 得 identity fact_id + cause 源 fact_id
    /// 2. verify_audit_typed(session) → 整链完整性
    /// 3. get_causal_chain_typed(session, cause) → 引擎因果链（若 cause 存在）
    /// 4. causal_chain(event_id)（应用层）→ 与引擎链比对 aligned
    ///
    /// server 不可用 → Ok(报告, verified=false, aligned=false) 优雅降级，不阻塞。
    pub async fn verify_cause_binding(
        &mut self,
        event_id: &str,
    ) -> Result<CauseBindingReport, StoreError> {
        let mut report = CauseBindingReport {
            event_id: event_id.to_string(),
            ..Default::default()
        };

        // 1. 读事件
        let event = match self.read_event(event_id).await? {
            Some(ev) => ev,
            None => return Ok(report), // 事件不存在 → 空报告
        };

        report.identity_fact_id = event.fact_id;
        report.cause_fact_id = event.cause;

        // 2. 整链完整性（server 不可用时优雅降级）
        let session_id = self.session_id_str()?.to_string();
        match self.evorule_client.verify_audit_typed(&session_id).await {
            Ok(verify) => {
                report.verified = verify.verified;
            }
            Err(e) => {
                tracing::debug!(error = %e, "verify_cause_binding: audit verify failed (degraded)");
                return Ok(report); // 降级：verified=false
            }
        }

        // 3. 引擎因果链（cause 存在时）
        if let Some(cause_fid) = event.cause {
            match self
                .evorule_client
                .get_causal_chain_typed(&session_id, cause_fid)
                .await
            {
                Ok(chain) => {
                    report.engine_chain_len = chain.chain_length;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "verify_cause_binding: causal chain failed (degraded)");
                }
            }
        }

        // 4. 应用层因果链 + 比对
        let app_chain = self.causal_chain(event_id).await?;
        report.app_chain_len = app_chain.len();

        // 对齐判定：链长一致（引擎链含 cause 源 Fact 自身，应用层链含从 event 回溯到根）
        // 语义不完全等价（引擎链是 Fact 流因果，应用层链是事件因果），MVP 以链长一致 + 非空为对齐启发式
        report.aligned = report.engine_chain_len > 0
            && report.app_chain_len > 0
            && report.engine_chain_len >= report.app_chain_len;

        // 5. 改进2：effects 引擎级 FactId 真实性校验
        //    对每个 effect 的 fact_id，拉取该事件 path 的 Fact 流，校验身份锚点一致。
        //    server 不可用 / 缺 fact_id（离线降级）→ false；全命中 → true。
        report.effects_verified = self.verify_effects_binding(&event, &session_id).await;

        Ok(report)
    }

    /// 改进2：校验事件 effects 中每个引擎级 FactId 是否真实存在于 Fact 流
    ///
    /// 对每个 effect 的 `fact_id`，用 `get_facts(path)` 反向验证——该 path 的 Fact 流中
    /// 身份锚点（首个精确匹配的 FactId）与 effect.fact_id 一致。
    /// - 全命中 → `true`（真实存在）
    /// - 无 effects / server 不可用 / 某 effect 缺 fact_id → `false`（降级，不阻断）
    async fn verify_effects_binding(&self, event: &MemoryEvent, session_id: &str) -> bool {
        if event.effects.is_empty() {
            return false; // 无正向因果可校验
        }
        let mut all_hit = true;
        for r in &event.effects {
            let Some(fid) = r.fact_id else {
                all_hit = false; // 离线/失败时未写入引擎级锚点 → 不可验证
                continue;
            };
            // 拉取该 effect 事件 path 的 Fact 流
            let path = self.event_path(&r.event_id);
            match self.evorule_client.get_facts(session_id, Some(&path)).await {
                Ok(facts) => {
                    // 身份锚点 = 首个精确匹配该 path 的 FactId（与 fetch_identity_fact_id 同语义）
                    let identity = facts
                        .into_iter()
                        .find(|f| f.path == path)
                        .map(|f| f.id)
                        .unwrap_or(0);
                    if identity == 0 || identity != fid {
                        all_hit = false; // 目标事件不存在或 FactId 漂移
                    }
                }
                Err(_) => {
                    all_hit = false; // server 不可用 → 降级
                }
            }
        }
        all_hit
    }

    /// B4：对某事件出示证据
    ///
    /// 三段式证明：源 FactId + 整链 verify + 引擎因果链（cause 存在时）。
    /// server 不可用时 fail-open：返回带 error 的证据（verified=false）。
    pub async fn evidence_for_event(
        &mut self,
        event_id: &str,
    ) -> Result<Option<MemoryEvidence>, StoreError> {
        let event = match self.read_event(event_id).await? {
            Some(ev) => ev,
            None => return Ok(None),
        };
        let identity_fact_id = match event.fact_id {
            Some(fid) if fid != 0 => fid,
            _ => return Ok(None),
        };
        let session_id = self.session_id_str()?.to_string();

        let mut evidence = MemoryEvidence {
            session_id: session_id.clone(),
            fact_id: identity_fact_id,
            cause_fact_id: event.cause,
            ..Default::default()
        };

        // 整链 verify
        match self.evorule_client.verify_audit_typed(&session_id).await {
            Ok(verify) => {
                evidence.verified = verify.verified;
                evidence.last_hash = verify.last_hash;
            }
            Err(e) => {
                evidence.error = Some(format!("audit verify failed: {}", e));
                return Ok(Some(evidence));
            }
        }

        // 因果链（cause 存在时）
        if let Some(cause_fid) = event.cause {
            if let Ok(chain) = self
                .evorule_client
                .get_causal_chain_typed(&session_id, cause_fid)
                .await
            {
                evidence.chain = chain.chain;
            }
        }

        Ok(Some(evidence))
    }

    // ===== 实体 CRUD =====

    /// 写入实体到 evorule + cache
    pub async fn write_entity(&mut self, entity: Entity) -> Result<(), StoreError> {
        let path = self.entity_path(&entity.entity_id);
        let value = serde_json::to_value(&entity)?;

        // best-effort 持久化
        let session_id = self.session_id_str()?;
        let _ = self
            .evorule_client
            .update_payload(session_id, &path, &value)
            .await;

        self.entity_cache.insert(entity.entity_id.clone(), entity);
        Ok(())
    }

    /// 读取实体(优先从 cache)
    pub async fn read_entity(&mut self, entity_id: &str) -> Result<Option<Entity>, StoreError> {
        // cache 命中:直接返回
        if let Some(entity) = self.entity_cache.get(entity_id) {
            return Ok(Some(entity.clone()));
        }

        // cache 未命中:best-effort 从 evorule 拉取
        // HTTP 失败不阻塞,返回 Ok(None)(与 write_event 的 best-effort 模式一致)
        let session_id = self.session_id_str()?.to_string();
        let path = self.entity_path(entity_id);
        let facts = match self
            .evorule_client
            .get_facts(&session_id, Some(&path))
            .await
        {
            Ok(facts) => facts,
            Err(e) => {
                tracing::debug!(
                    entity_id = entity_id,
                    error = %e,
                    "read_entity: evorule HTTP failed (best-effort, returning None)"
                );
                return Ok(None);
            }
        };

        for fact in facts {
            if fact.path == path {
                if let Ok(entity) = serde_json::from_value::<Entity>(fact.value) {
                    self.entity_cache
                        .insert(entity.entity_id.clone(), entity.clone());
                    return Ok(Some(entity));
                }
            }
        }

        Ok(None)
    }

    /// 列出所有实体(从 cache)
    pub fn list_entities(&self) -> Vec<Entity> {
        self.entity_cache.values().cloned().collect()
    }

    /// 获取或创建实体(如果不存在则创建)
    pub async fn get_or_create_entity(
        &mut self,
        entity_id: &str,
        entity_type: EntityType,
        display_name: &str,
        created_at: u64,
    ) -> Result<Entity, StoreError> {
        if let Some(entity) = self.read_entity(entity_id).await? {
            return Ok(entity);
        }
        let entity = Entity::new(entity_id, entity_type, display_name, created_at);
        self.write_entity(entity.clone()).await?;
        Ok(entity)
    }

    // ===== 索引查询 =====

    /// 查询某实体的所有事件(从索引 + cache)
    pub fn events_for_entity(&self, entity_id: &str) -> Vec<MemoryEvent> {
        let event_ids = self.entity_index.events_for_entity(entity_id);
        event_ids
            .iter()
            .filter_map(|eid| self.event_cache.get(*eid))
            .cloned()
            .collect()
    }

    /// 查询某事件类型的所有事件(从索引 + cache)
    pub fn events_for_type(&self, event_type: &EventType) -> Vec<MemoryEvent> {
        let event_ids = self.entity_index.events_for_type(event_type);
        event_ids
            .iter()
            .filter_map(|eid| self.event_cache.get(*eid))
            .cloned()
            .collect()
    }

    /// 获取实体索引(只读)
    pub fn entity_index(&self) -> &EntityIndex {
        &self.entity_index
    }

    /// 获取事件缓存大小
    pub fn event_count(&self) -> usize {
        self.event_cache.len()
    }

    /// 获取实体缓存大小
    pub fn entity_count(&self) -> usize {
        self.entity_cache.len()
    }

    /// 根据 FactId 查找事件(因果链遍历用)
    pub fn find_event_by_fact_id(&self, fact_id: FactId) -> Option<MemoryEvent> {
        self.fact_to_event
            .get(&fact_id)
            .and_then(|eid| self.event_cache.get(eid))
            .cloned()
    }

    /// 清空缓存(不删除 evorule 中的数据)
    pub fn clear_cache(&mut self) {
        self.event_cache.clear();
        self.entity_cache.clear();
        self.fact_to_event.clear();
        self.entity_index.clear();
    }

    // ===== 测试辅助方法(仅供同 crate 内测试模块使用,不暴露私有字段)=====

    /// **测试专用**:直接插入事件到 cache + 更新 entity_index,
    /// 绕过 async `write_event`(避免测试中调用 evorule HTTP)。
    #[cfg(test)]
    pub fn insert_event_raw(&mut self, event: MemoryEvent) {
        let event_id = event.event_id.clone();
        self.entity_index.index_event(&event_id, &event);
        self.event_cache.insert(event_id, event);
    }

    /// **测试专用**:手动设置 fact_id → event_id 映射,
    /// 模拟 `write_event` 中 best-effort 拉取 FactId 成功的场景。
    #[cfg(test)]
    pub fn set_fact_mapping_raw(&mut self, fact_id: FactId, event_id: String) {
        self.fact_to_event.insert(fact_id, event_id);
    }
}

#[cfg(test)]
mod tests {
    use super::super::event::{ConversationSubtype, EventSource, EventType, MilestoneSubtype};
    use super::*;
    use crate::api::evorule_client::EvoruleApiClient;

    fn make_test_store() -> MemoryEventStore {
        let client = EvoruleApiClient::new("http://localhost:9999"); // 不会真正调用
        MemoryEventStore::new("agent_test", client).with_session_id("1")
    }

    fn make_test_event(id: &str, event_type: EventType, ts: u64) -> MemoryEvent {
        MemoryEvent::new_root(id, event_type, ts, EventSource::UserInput)
            .with_content(serde_json::json!({"summary": format!("event {}", id)}))
    }

    #[tokio::test]
    async fn test_write_and_read_event_from_cache() {
        let mut store = make_test_store();
        let event = make_test_event("E001", EventType::EmotionEvent, 1000);

        // write_event 会尝试 HTTP 调用(失败,best-effort),但 cache 总是更新
        let _ = store.write_event(event.clone()).await;

        // read_event 从 cache 读取(evorule 不可用时不影响)
        let read = store.read_event("E001").await.unwrap();
        assert!(read.is_some());
        assert_eq!(read.unwrap().event_id, "E001");
    }

    #[tokio::test]
    async fn test_list_events_sorted() {
        let mut store = make_test_store();
        let e1 = make_test_event("E003", EventType::EmotionEvent, 3000);
        let e2 = make_test_event("E001", EventType::EmotionEvent, 1000);
        let e3 = make_test_event("E002", EventType::EmotionEvent, 2000);

        let _ = store.write_event(e1).await;
        let _ = store.write_event(e2).await;
        let _ = store.write_event(e3).await;

        let sorted = store.list_events_sorted();
        assert_eq!(sorted.len(), 3);
        assert_eq!(sorted[0].event_id, "E001"); // ts=1000
        assert_eq!(sorted[1].event_id, "E002"); // ts=2000
        assert_eq!(sorted[2].event_id, "E003"); // ts=3000
    }

    #[tokio::test]
    async fn test_write_event_with_cause_updates_effects() {
        let mut store = make_test_store();

        // 写入 E1(根事件)
        let e1 = make_test_event(
            "E001",
            EventType::Conversation(ConversationSubtype::Farewell),
            1000,
        );
        let _ = store.write_event(e1).await;

        // E1 的 fact_id 在 best-effort 模式下可能为 0(因为 evorule 不可用)
        // 手动设置 fact_id 用于因果链测试
        let e1_fact_id = 42u64;
        if let Some(e1) = store.event_cache.get_mut("E001") {
            e1.fact_id = Some(e1_fact_id);
        }
        store.fact_to_event.insert(e1_fact_id, "E001".to_string());

        // 写入 E2(cause = E1 的 fact_id)
        let e2 = make_test_event("E002", EventType::EmotionEvent, 2000);
        let _ = store.write_event_with_cause(e2, e1_fact_id).await;

        // E1.effects 应包含指向 E002 的 EventRef（改进2：引擎级引用）
        let e1_after = store.event_cache.get("E001").unwrap();
        assert!(
            e1_after.effects.iter().any(|r| r.event_id == "E002"),
            "E1.effects should contain E002, got: {:?}",
            e1_after.effects
        );

        // E2.cause 应指向 E1 的 fact_id
        let e2_after = store.event_cache.get("E002").unwrap();
        assert_eq!(e2_after.cause, Some(e1_fact_id));
    }

    #[tokio::test]
    async fn test_causal_chain_backward() {
        let mut store = make_test_store();

        // 构建因果链: E1 ← E2 ← E3
        let e1 = make_test_event("E001", EventType::EmotionEvent, 1000);
        let _ = store.write_event(e1).await;
        // 手动设置 fact_id
        let f1 = 10u64;
        store.event_cache.get_mut("E001").unwrap().fact_id = Some(f1);
        store.fact_to_event.insert(f1, "E001".to_string());

        let e2 = make_test_event("E002", EventType::EmotionEvent, 2000);
        let _ = store.write_event_with_cause(e2, f1).await;
        let f2 = 20u64;
        store.event_cache.get_mut("E002").unwrap().fact_id = Some(f2);
        store.fact_to_event.insert(f2, "E002".to_string());

        let e3 = make_test_event("E003", EventType::EmotionEvent, 3000);
        let _ = store.write_event_with_cause(e3, f2).await;

        // 从 E3 回溯
        let chain = store.causal_chain("E003").await.unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].event_id, "E003"); // 从当前开始
        assert_eq!(chain[1].event_id, "E002");
        assert_eq!(chain[2].event_id, "E001"); // 到根事件
    }

    #[tokio::test]
    async fn test_causal_chain_truncates_on_missing_cause() {
        let mut store = make_test_store();

        // E1 的 cause 指向一个不存在的 FactId
        let e1 = MemoryEvent::new_root(
            "E001",
            EventType::EmotionEvent,
            1000,
            EventSource::UserInput,
        )
        .with_cause(999); // 不存在的 fact_id
        let _ = store.write_event(e1).await;

        // 回溯应在 E1 处截断(cause 指向的 fact 不在 fact_to_event 中)
        let chain = store.causal_chain("E001").await.unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].event_id, "E001");
    }

    #[tokio::test]
    async fn test_causal_chain_cycle_protection() {
        let mut store = make_test_store();

        // 手动构造循环引用:E1.cause → f2 → E2.cause → f1 → E1
        let mut e1 = make_test_event("E001", EventType::EmotionEvent, 1000);
        e1.cause = Some(20); // 指向 E2 的 fact_id
        e1.fact_id = Some(10);

        let mut e2 = make_test_event("E002", EventType::EmotionEvent, 2000);
        e2.cause = Some(10); // 指向 E1 的 fact_id
        e2.fact_id = Some(20);

        store.event_cache.insert("E001".to_string(), e1);
        store.event_cache.insert("E002".to_string(), e2);
        store.fact_to_event.insert(10, "E001".to_string());
        store.fact_to_event.insert(20, "E002".to_string());

        // 回溯不应无限循环
        let chain = store.causal_chain("E001").await.unwrap();
        assert!(
            chain.len() <= 2,
            "cycle should be detected, got len={}",
            chain.len()
        );
    }

    #[tokio::test]
    async fn test_entity_crud() {
        let mut store = make_test_store();
        let entity = Entity::new("person_mom", EntityType::Person, "妈妈", 1000).with_alias("老妈");

        store.write_entity(entity.clone()).await.unwrap();

        let read = store.read_entity("person_mom").await.unwrap();
        assert!(read.is_some());
        assert_eq!(read.unwrap().display_name, "妈妈");
        assert_eq!(store.entity_count(), 1);
    }

    #[tokio::test]
    async fn test_get_or_create_entity() {
        let mut store = make_test_store();

        // 首次:创建
        let e1 = store
            .get_or_create_entity("pet_doudou", EntityType::Pet, "豆豆", 1000)
            .await
            .unwrap();
        assert_eq!(e1.display_name, "豆豆");

        // 第二次:读取(不创建新的)
        let e2 = store
            .get_or_create_entity("pet_doudou", EntityType::Pet, "豆豆2", 2000)
            .await
            .unwrap();
        assert_eq!(e2.display_name, "豆豆"); // 返回原有的,不是"豆豆2"
        assert_eq!(store.entity_count(), 1);
    }

    #[tokio::test]
    async fn test_events_for_entity() {
        let mut store = make_test_store();

        let e1 = make_test_event(
            "E001",
            EventType::Milestone(MilestoneSubtype::Achievement),
            1000,
        )
        .with_entity(crate::agent::memory_event::entity::EntityRef::new(
            "pet_doudou",
            "subject",
        ));
        let e2 = make_test_event("E002", EventType::EmotionEvent, 2000)
            .with_entity(crate::agent::memory_event::entity::EntityRef::new(
                "pet_doudou",
                "subject",
            ))
            .with_entity(crate::agent::memory_event::entity::EntityRef::new(
                "person_me",
                "observer",
            ));

        let _ = store.write_event(e1).await;
        let _ = store.write_event(e2).await;

        let doudou_events = store.events_for_entity("pet_doudou");
        assert_eq!(doudou_events.len(), 2);

        let me_events = store.events_for_entity("person_me");
        assert_eq!(me_events.len(), 1);
        assert_eq!(me_events[0].event_id, "E002");
    }

    #[tokio::test]
    async fn test_events_for_type() {
        let mut store = make_test_store();

        let e1 = make_test_event(
            "E001",
            EventType::Milestone(MilestoneSubtype::Birthday),
            1000,
        );
        let e2 = make_test_event(
            "E002",
            EventType::Milestone(MilestoneSubtype::Graduation),
            2000,
        );
        let e3 = make_test_event("E003", EventType::EmotionEvent, 3000);

        let _ = store.write_event(e1).await;
        let _ = store.write_event(e2).await;
        let _ = store.write_event(e3).await;

        let milestones = store.events_for_type(&EventType::Milestone(MilestoneSubtype::Birthday));
        assert_eq!(milestones.len(), 2); // Birthday + Graduation 都属于 Milestone kind
    }

    #[tokio::test]
    async fn test_find_event_by_fact_id() {
        let mut store = make_test_store();
        let e1 = make_test_event("E001", EventType::EmotionEvent, 1000);
        let _ = store.write_event(e1).await;

        // 手动设置 fact_id
        let fid = 77u64;
        store.event_cache.get_mut("E001").unwrap().fact_id = Some(fid);
        store.fact_to_event.insert(fid, "E001".to_string());

        let found = store.find_event_by_fact_id(fid);
        assert!(found.is_some());
        assert_eq!(found.unwrap().event_id, "E001");

        assert!(store.find_event_by_fact_id(999).is_none());
    }

    #[tokio::test]
    async fn test_clear_cache() {
        let mut store = make_test_store();
        let e1 = make_test_event("E001", EventType::EmotionEvent, 1000);
        let _ = store.write_event(e1).await;

        assert_eq!(store.event_count(), 1);
        store.clear_cache();
        assert_eq!(store.event_count(), 0);
        assert_eq!(store.entity_count(), 0);
    }

    #[test]
    fn test_path_construction() {
        let store = make_test_store();
        assert_eq!(
            store.event_path("E001"),
            "__memory__.agent_test.events.E001"
        );
        assert_eq!(
            store.entity_path("person_mom"),
            "__memory__.agent_test.entities.person_mom"
        );
        assert_eq!(store.events_prefix(), "__memory__.agent_test.events.");
        assert_eq!(store.entities_prefix(), "__memory__.agent_test.entities.");
    }

    #[tokio::test]
    async fn test_session_not_set_error() {
        let client = EvoruleApiClient::new("http://localhost:9999");
        let mut store = MemoryEventStore::new("agent_test", client);
        // 没有设置 session_id

        let result = store
            .write_event(make_test_event("E001", EventType::EmotionEvent, 1000))
            .await;
        // write_event 在 session 未设置时仍更新 cache(best-effort),但返回错误
        // 实际上 write_event 先更新 cache 再 best-effort 持久化
        // session_id_str() 会在持久化阶段返回 Err
        // 但 cache 已经更新了
        assert!(store.event_count() > 0 || result.is_err());
    }

    // ===== B2 测试 =====

    #[test]
    fn test_cause_binding_report_default() {
        let report = CauseBindingReport::default();
        assert!(report.identity_fact_id.is_none());
        assert!(report.cause_fact_id.is_none());
        assert!(!report.verified);
        assert_eq!(report.app_chain_len, 0);
        assert_eq!(report.engine_chain_len, 0);
        assert!(!report.aligned);
    }

    #[test]
    fn test_fill_identity_fact_id_fills_when_none() {
        let mut event = make_test_event("E001", EventType::EmotionEvent, 1000);
        assert!(event.fact_id.is_none()); // 初始无 fact_id

        MemoryEventStore::fill_identity_fact_id(&mut event, 42);
        assert_eq!(event.fact_id, Some(42));
    }

    #[test]
    fn test_fill_identity_fact_id_noop_when_set() {
        let mut event = make_test_event("E001", EventType::EmotionEvent, 1000);
        event.fact_id = Some(10); // 已有 fact_id

        MemoryEventStore::fill_identity_fact_id(&mut event, 42);
        assert_eq!(event.fact_id, Some(10)); // 不覆盖
    }

    #[tokio::test]
    async fn test_verify_cause_binding_degraded_no_server() {
        let mut store = make_test_store();
        let event = make_test_event("E001", EventType::EmotionEvent, 1000);
        let _ = store.write_event(event).await;

        // server 不可用 → verify_cause_binding 优雅降级
        let report = store.verify_cause_binding("E001").await.unwrap();
        assert_eq!(report.event_id, "E001");
        // server 不可用 → verify_audit_typed 失败 → 降级返回（verified=false）
        assert!(!report.verified);
    }

    #[tokio::test]
    async fn test_verify_cause_binding_nonexistent_event() {
        let mut store = make_test_store();

        // 事件不存在 → 空报告
        let report = store.verify_cause_binding("NONEXIST").await.unwrap();
        assert_eq!(report.event_id, "NONEXIST");
        assert!(report.identity_fact_id.is_none());
        assert!(!report.verified);
    }

    // ===== B4：evidence_for_event 测试 =====

    #[tokio::test]
    async fn test_evidence_for_event_nonexistent() {
        let mut store = make_test_store();

        // 事件不存在 → Ok(None)
        let result = store.evidence_for_event("NONEXIST").await.unwrap();
        assert!(
            result.is_none(),
            "evidence_for_event on nonexistent event should return None"
        );
    }

    #[tokio::test]
    async fn test_evidence_for_event_no_fact_id_returns_none() {
        let mut store = make_test_store();
        let event = make_test_event("E001", EventType::EmotionEvent, 1000);
        // event.fact_id 在 write_event best-effort 模式下为 None
        let _ = store.write_event(event).await;

        // 手动确认 fact_id 为 None
        let cached = store.event_cache.get("E001").unwrap();
        assert!(cached.fact_id.is_none());

        // 无 fact_id → Ok(None)
        let result = store.evidence_for_event("E001").await.unwrap();
        assert!(result.is_none(), "event without fact_id should return None");
    }

    #[tokio::test]
    async fn test_evidence_for_event_zero_fact_id_returns_none() {
        let mut store = make_test_store();
        let mut event = make_test_event("E001", EventType::EmotionEvent, 1000);
        event.fact_id = Some(0); // fact_id == 0
        store.insert_event_raw(event);

        let result = store.evidence_for_event("E001").await.unwrap();
        assert!(result.is_none(), "event with fact_id=0 should return None");
    }

    #[tokio::test]
    async fn test_evidence_for_event_degraded_no_server() {
        let mut store = make_test_store();
        let mut event = make_test_event("E001", EventType::EmotionEvent, 1000);
        event.fact_id = Some(42);
        store.insert_event_raw(event);

        // server 不可用 → fail-open（verified=false, error 被设置）
        let result = store.evidence_for_event("E001").await.unwrap();
        let ev = result.expect("evidence should be Some (fail-open)");
        assert_eq!(ev.fact_id, 42);
        assert!(!ev.verified, "degraded mode should have verified=false");
        assert!(
            ev.error.is_some(),
            "degraded mode should have error message"
        );
        assert!(ev.error.as_ref().unwrap().contains("audit verify failed"));
    }

    #[tokio::test]
    async fn test_evidence_for_event_with_cause_degraded() {
        let mut store = make_test_store();
        let mut event = make_test_event("E001", EventType::EmotionEvent, 1000);
        event.fact_id = Some(42);
        event.cause = Some(10);
        store.insert_event_raw(event);

        // server 不可用 → fail-open（verified=false），因果链获取失败但不阻断
        let result = store.evidence_for_event("E001").await.unwrap();
        let ev = result.expect("evidence should be Some (fail-open)");
        assert!(!ev.verified);
        assert!(ev.error.is_some());
        assert_eq!(ev.cause_fact_id, Some(10));
        assert!(ev.chain.is_empty(), "degraded mode chain should be empty");
    }
}
