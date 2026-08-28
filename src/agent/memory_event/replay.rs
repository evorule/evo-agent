// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 032 ReplayEngine —— 因果链遍历 + 确定性回放 + NL 包装
//!
//! ## 核心原则(032 设计文档 §7.1)
//!
//! **事实由 evorule 决定,自然语言由 LLM 包装。**
//!
//! | 层 | 职责 | 是否确定性 |
//! |---|---|---|
//! | evorule Fact log | 记录每一步,blake3 哈希链防篡改 | ✅ 完全确定 |
//! | evo-agent MemoryEvent | 结构化事件 + 因果链 + 情感 | ✅ 完全确定(写入时定型) |
//! | 回放引擎 | 沿因果链 + 时间序遍历事件 | ✅ 算法确定 |
//! | LLM NL 包装 | 把结构化事件翻译成自然语言叙述 | ❌ 非确定(temperature=0 缓解) |
//!
//! **关键**:即使 LLM 包装每次不同,**事实不变**。用户可以"看事实"验证 LLM 说的是否属实。

use serde::{Deserialize, Serialize};

use crate::api::evorule_client::FactLogEntry;
use crate::io_handler::IoHandler;
use crate::io_handlers::LlmHandler;
use crate::json_convert::serde_to_tcb;

use super::event::{EventRef, MemoryEvent};
use super::evidence::NarrativeWithEvidence;
use super::store::{MemoryEventStore, StoreError};

/// 回放方向
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReplayDirection {
    /// 从现在追到过去(沿 cause 链回溯)
    Backward,
    /// 从过去走到现在(沿 effects 链前进)
    Forward,
}

/// 叙述结果 —— LLM 包装后的自然语言 + 引用的事件/Fact
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Narrative {
    /// LLM 生成的自然语言叙述
    pub text: String,
    /// 引用的事件 ID 列表(用户可 click 查看原始事件)
    pub cited_events: Vec<String>,
    /// 引用的 Fact ID 列表(用户可 click 查看原始 Fact)
    #[serde(default)]
    pub cited_facts: Vec<u64>,
}

/// 对话轮类型（对话投影的输出分类）
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TurnType {
    UserCommand,
    ToolCall,
    ToolResult,
    Stable,
}

/// 对话轮（对话投影的输出）
#[derive(Debug, Clone)]
pub struct ConversationTurn {
    pub version: u64,
    pub source_fact_id: u64,
    pub turn_type: TurnType,
    pub content: String,
    pub error: Option<String>,
}

/// 时间旅行投影结果
#[derive(Debug, Clone, Default)]
pub struct MemoryProjection {
    pub version: u64,
    pub records: Vec<crate::agent::memory::MemoryRecord>,
    pub events: Vec<MemoryEvent>,
}

/// 032 ReplayEngine —— 因果链遍历 + 确定性回放
///
/// 回放算法(032 设计文档 §7.2):
/// 1. 从目标事件出发,沿 `cause` 链回溯到根事件
/// 2. 按时间序排列
/// 3. 遍历事件,输出结构化事实(确定性)
/// 4. (可选)LLM 把结构化事实包装成 NL 叙述(非确定,但事实不变)
pub struct ReplayEngine {
    store: MemoryEventStore,
    /// LLM handler(可选,None 时 narrate 返回结构化摘要)
    llm: Option<LlmHandler>,
    /// NL 包装用的模型名称(None = fallback 到 llm 的 default_model)
    narration_model: Option<String>,
}

impl ReplayEngine {
    /// 创建回放引擎
    pub fn new(store: MemoryEventStore) -> Self {
        Self {
            store,
            llm: None,
            narration_model: None,
        }
    }

    /// 设置 LLM handler(用于 NL 包装)
    pub fn with_llm(mut self, llm: LlmHandler) -> Self {
        self.llm = Some(llm);
        self
    }

    /// 设置 NL 包装模型名称
    pub fn with_narration_model(mut self, model: &str) -> Self {
        self.narration_model = Some(model.to_string());
        self
    }

    /// 从某事件出发,沿因果链回溯/前进
    ///
    /// - `Backward`:从当前事件沿 cause 链回溯到根事件
    /// - `Forward`:从当前事件沿 effects 链前进到末端事件
    ///
    /// 返回的事件列表按遍历顺序排列(Backward: 当前→过去;Forward: 当前→未来)。
    pub async fn replay_from(
        &mut self,
        event_id: &str,
        direction: ReplayDirection,
    ) -> Result<Vec<MemoryEvent>, StoreError> {
        match direction {
            ReplayDirection::Backward => {
                // 沿 cause 链回溯
                self.store.causal_chain(event_id).await
            }
            ReplayDirection::Forward => {
                // 沿 effects 链前进
                let mut chain = Vec::new();
                let mut current_id = Some(event_id.to_string());
                let mut visited = std::collections::HashSet::new();

                while let Some(eid) = current_id {
                    if !visited.insert(eid.clone()) {
                        break; // 循环检测
                    }
                    let event = self.store.read_event(&eid).await?;
                    match event {
                        Some(ev) => {
                            current_id = ev.effects.last().map(|r| r.event_id.clone());
                            chain.push(ev);
                        }
                        None => break,
                    }
                }
                Ok(chain)
            }
        }
    }

    /// 按实体回放("豆豆的所有事件")
    ///
    /// 返回该实体关联的所有事件,按时间序排列(从过去到现在)。
    pub async fn replay_by_entity(
        &mut self,
        entity_id: &str,
    ) -> Result<Vec<MemoryEvent>, StoreError> {
        let mut events = self.store.events_for_entity(entity_id);
        events.sort_by_key(|e| e.timestamp);
        Ok(events)
    }

    /// 按时间序回放某实体的所有事件(可指定时间范围)
    ///
    /// - `from`:起始时间(可选,None 表示不限制)
    /// - `to`:结束时间(可选,None 表示不限制)
    pub async fn replay_entity_timeline(
        &mut self,
        entity_id: &str,
        from: Option<u64>,
        to: Option<u64>,
    ) -> Result<Vec<MemoryEvent>, StoreError> {
        let mut events = self.store.events_for_entity(entity_id);
        events.retain(|e| from.is_none_or(|t| e.timestamp >= t));
        events.retain(|e| to.is_none_or(|t| e.timestamp <= t));
        events.sort_by_key(|e| e.timestamp);
        Ok(events)
    }

    /// 确定性回放 + LLM NL 包装(temperature=0)
    ///
    /// 把结构化事件链包装成自然语言叙述。
    /// 关键约束:
    /// 1. 只能用 events 里的字段,不能编造
    /// 2. 必须标注 cited_events 和 cited_facts,用户可 click 查看原始 Fact
    /// 3. 情感直接读 emotion 字段,不重新生成
    ///
    /// 如果未设置 LLM handler,返回结构化文本摘要(确定性)。
    pub async fn narrate(&mut self, events: &[MemoryEvent]) -> Result<Narrative, String> {
        let cited_events: Vec<String> = events.iter().map(|e| e.event_id.clone()).collect();
        let cited_facts: Vec<u64> = events.iter().filter_map(|e| e.fact_id).collect();

        // 无 LLM 时:返回结构化摘要(完全确定)
        let llm = match &self.llm {
            Some(llm) => llm,
            None => {
                let text = self.build_structured_summary(events);
                return Ok(Narrative {
                    text,
                    cited_events,
                    cited_facts,
                });
            }
        };

        // 有 LLM 时:temperature=0 包装
        let prompt = self.build_narration_prompt(events);
        let mut messages_vec: Vec<serde_json::Value> = Vec::with_capacity(2);
        messages_vec.push(serde_json::json!({
            "role": "system",
            "content": NARRATION_SYSTEM_PROMPT,
        }));
        messages_vec.push(serde_json::json!({
            "role": "user",
            "content": prompt,
        }));

        let mut params_map = serde_json::Map::new();
        if let Some(model) = &self.narration_model {
            params_map.insert(
                "model".to_string(),
                serde_json::Value::String(model.clone()),
            );
        }
        params_map.insert(
            "temperature".to_string(),
            serde_json::json!(0.0), // temperature=0 保证最大确定性
        );
        params_map.insert("max_tokens".to_string(), serde_json::json!(1024));
        params_map.insert(
            "messages".to_string(),
            serde_json::Value::Array(messages_vec),
        );
        let params_json = serde_json::Value::Object(params_map);
        let params = serde_to_tcb(&params_json);

        let result = llm.execute(&params).await.map_err(|e| e.to_string())?;

        // 解析 LLM 响应
        let response: crate::agent::translator::LlmResponse =
            serde_json::from_str(&result.to_string())
                .map_err(|e| format!("parse LLM response: {}", e))?;

        Ok(Narrative {
            text: response.content,
            cited_events,
            cited_facts,
        })
    }

    /// B4：带证据的叙述
    ///
    /// 在 `narrate` 基础上，为每个事件附加紧凑证据标记（render_compact）。
    /// server 不可用时 evidence_map 为空（fail-open），叙述文本不受影响。
    pub async fn narrate_with_evidence(
        &mut self,
        events: &[MemoryEvent],
    ) -> Result<NarrativeWithEvidence, String> {
        let narrative = self.narrate(events).await?;
        let mut evidence_map = std::collections::BTreeMap::new();

        for event in events {
            if let Some(ev) = self
                .store
                .evidence_for_event(&event.event_id)
                .await
                .ok()
                .flatten()
            {
                evidence_map.insert(event.event_id.clone(), ev.render_compact());
            }
        }

        Ok(NarrativeWithEvidence {
            text: narrative.text,
            cited_events: narrative.cited_events,
            cited_facts: narrative.cited_facts,
            evidence: evidence_map,
        })
    }

    /// 构建结构化文本摘要(无 LLM 时用,完全确定)
    fn build_structured_summary(&self, events: &[MemoryEvent]) -> String {
        let mut lines = Vec::new();
        for event in events {
            let time = format_timestamp(event.timestamp);
            // 用 {:?} 包含完整类型信息(含子类型,如 Milestone(Achievement))
            let type_str = format!("{:?}", event.event_type);
            let emotion_str = event
                .emotion
                .as_ref()
                .map(|e| {
                    let labels = e.labels.join(",");
                    format!("[{}]", labels)
                })
                .unwrap_or_default();

            let summary = event
                .content
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            lines.push(format!(
                "[{}] {} {} {} — {}",
                time, type_str, event.event_id, emotion_str, summary
            ));
        }
        lines.join("\n")
    }

    /// 构建 NL 包装 prompt(只放结构化事实,不允许编造)
    fn build_narration_prompt(&self, events: &[MemoryEvent]) -> String {
        let mut facts = Vec::new();
        for event in events {
            let mut fact = format!(
                "事件 {}: 类型={}, 时间={}",
                event.event_id,
                event.event_type.kind_str(),
                format_timestamp(event.timestamp)
            );

            if let Some(emotion) = &event.emotion {
                fact.push_str(&format!(", 情感标签={:?}", emotion.labels));
            }

            if !event.entities.is_empty() {
                let entities: Vec<String> = event
                    .entities
                    .iter()
                    .map(|e| format!("{}({})", e.entity_id, e.role))
                    .collect();
                fact.push_str(&format!(", 实体=[{}]", entities.join(", ")));
            }

            if let Some(summary) = event.content.get("summary").and_then(|v| v.as_str()) {
                fact.push_str(&format!(", 摘要=\"{}\"", summary));
            }

            facts.push(fact);
        }

        format!(
            "请根据以下结构化事件事实,生成一段自然语言叙述。\n\
             要求:\n\
             1. 只能使用以下事实,不能编造任何不存在的信息\n\
             2. 情感直接引用,不要重新生成\n\
             3. 用中文输出,不超过 500 字\n\n\
             事件事实:\n{}",
            facts.join("\n")
        )
    }

    /// 获取内部 store 引用(供外部查询)
    pub fn store(&self) -> &MemoryEventStore {
        &self.store
    }

    /// 获取内部 store 可变引用
    pub fn store_mut(&mut self) -> &mut MemoryEventStore {
        &mut self.store
    }

    // ===== B3：Fact 流回放 =====

    /// B3：版本区间 Fact 流重放（read_from 等价）
    ///
    /// 从 evorule server 拉取指定版本区间的 Fact 流。
    /// `from`/`to` 均为可选，None 表示不限制。
    pub async fn replay_facts(
        &self,
        from: Option<u64>,
        to: Option<u64>,
    ) -> Result<Vec<FactLogEntry>, StoreError> {
        let session_id = self.store.session_id().ok_or(StoreError::SessionNotSet)?;
        Ok(self
            .store
            .evorule_client
            .replay_range(session_id, from, to)
            .await?)
    }

    /// B3：从 Fact 流重建 MemoryEvent 列表
    ///
    /// 过滤 PayloadUpdate + events 前缀的 Fact，按 path 分组做双提取：
    /// - 身份 = 第一个版本的 fact_id（不随 effects 更新漂移）
    /// - 内容 = 最后一个版本的 value（effects 完整）
    /// 按 version 排序。
    pub async fn replay_events_from_fact_stream(
        &self,
        from: Option<u64>,
        to: Option<u64>,
    ) -> Result<Vec<MemoryEvent>, StoreError> {
        let facts = self.replay_facts(from, to).await?;
        let events_prefix = format!("__memory__.{}.events.", self.store.namespace());

        // 过滤 PayloadUpdate + events 前缀，按 path 分组
        let mut by_path: std::collections::BTreeMap<String, Vec<&FactLogEntry>> =
            Default::default();
        for fact in &facts {
            if fact.fact_type != "PayloadUpdate" {
                continue;
            }
            let path = match fact.path() {
                Some(p) if p.starts_with(&events_prefix) => p,
                _ => continue,
            };
            by_path.entry(path.to_string()).or_default().push(fact);
        }

        // 双提取：identity = first version's id, content = last version's value
        let mut events: Vec<(u64, MemoryEvent)> = Vec::new();
        for (_, group) in by_path {
            let mut sorted = group;
            sorted.sort_by_key(|f| f.version);

            let identity = sorted.first().map(|f| f.id).unwrap_or(0);
            let sort_version = sorted.last().map(|f| f.version).unwrap_or(0);

            if let Some(last) = sorted.last() {
                if let Some(value) = last.value() {
                    if let Ok(mut event) = serde_json::from_value::<MemoryEvent>(value.clone()) {
                        event.fact_id = Some(identity);
                        events.push((sort_version, event));
                    }
                }
            }
        }

        events.sort_by_key(|(v, _)| *v);
        Ok(events.into_iter().map(|(_, e)| e).collect())
    }

    /// B3：对话投影 —— 从 Fact 流重建对话轮
    ///
    /// 过滤 Command/IoRequest/IoResponse/Stable 类型，映射为 ConversationTurn。
    pub async fn replay_conversation(
        &self,
        from: Option<u64>,
        to: Option<u64>,
    ) -> Result<Vec<ConversationTurn>, StoreError> {
        let facts = self.replay_facts(from, to).await?;
        let mut turns: Vec<ConversationTurn> = Vec::new();

        for fact in facts {
            let (turn_type, content, error) = match fact.fact_type.as_str() {
                "Command" => {
                    let content = fact
                        .instruction()
                        .map(extract_text)
                        .unwrap_or_default();
                    (TurnType::UserCommand, content, None)
                }
                "IoRequest" => {
                    let content = fact
                        .payload
                        .get("params")
                        .map(extract_text)
                        .unwrap_or_default();
                    (TurnType::ToolCall, content, None)
                }
                "IoResponse" => {
                    let content = fact
                        .payload
                        .get("result")
                        .map(extract_text)
                        .unwrap_or_default();
                    let error = fact
                        .payload
                        .get("error")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    (TurnType::ToolResult, content, error)
                }
                "Stable" => (TurnType::Stable, "Session stable".to_string(), None),
                _ => continue,
            };
            turns.push(ConversationTurn {
                version: fact.version,
                source_fact_id: fact.id,
                turn_type,
                content,
                error,
            });
        }

        Ok(turns)
    }

    /// B3：时间旅行投影 —— 投影到指定版本的状态
    ///
    /// 过滤 version <= 指定版本的 FactLogEntry：
    /// - PayloadUpdate 且 path 不以 events 前缀开头 → MemoryRecord（last-write-wins）
    /// - PayloadUpdate 且 path 以 events 前缀开头 → MemoryEvent（双提取）
    pub async fn project_at_version(&self, version: u64) -> Result<MemoryProjection, StoreError> {
        let facts = self.replay_facts(None, Some(version)).await?;
        let events_prefix = format!("__memory__.{}.events.", self.store.namespace());

        let mut records: std::collections::BTreeMap<String, crate::agent::memory::MemoryRecord> =
            Default::default();
        let mut events_by_path: std::collections::BTreeMap<String, Vec<&FactLogEntry>> =
            Default::default();

        for fact in &facts {
            if fact.version > version {
                continue;
            }
            if fact.fact_type != "PayloadUpdate" {
                continue;
            }
            let path = match fact.path() {
                Some(p) => p,
                None => continue,
            };
            if path.starts_with(&events_prefix) {
                events_by_path
                    .entry(path.to_string())
                    .or_default()
                    .push(fact);
            } else {
                // MemoryRecord（last-write-wins）
                if let Some(value) = fact.value() {
                    if let Ok(record) =
                        serde_json::from_value::<crate::agent::memory::MemoryRecord>(value.clone())
                    {
                        records.insert(path.to_string(), record);
                    }
                }
            }
        }

        // 事件双提取
        let mut events: Vec<(u64, MemoryEvent)> = Vec::new();
        for (_, group) in events_by_path {
            let mut sorted = group;
            sorted.sort_by_key(|f| f.version);

            let identity = sorted.first().map(|f| f.id).unwrap_or(0);
            let sort_version = sorted.last().map(|f| f.version).unwrap_or(0);

            if let Some(last) = sorted.last() {
                if let Some(value) = last.value() {
                    if let Ok(mut event) = serde_json::from_value::<MemoryEvent>(value.clone()) {
                        event.fact_id = Some(identity);
                        events.push((sort_version, event));
                    }
                }
            }
        }
        events.sort_by_key(|(v, _)| *v);

        Ok(MemoryProjection {
            version,
            records: records.into_values().collect(),
            events: events.into_iter().map(|(_, e)| e).collect(),
        })
    }
}

/// 从 JSON Value 提取文本：字符串直接返回，其他序列化为 JSON 字符串
fn extract_text(v: &serde_json::Value) -> String {
    v.as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| v.to_string())
}

/// NL 包装系统提示
const NARRATION_SYSTEM_PROMPT: &str = "\
你是一个人生回放叙述助手。你的任务是把结构化事件事实包装成自然语言叙述。\n\
\n\
严格约束:\n\
1. 只能使用用户提供的事实,绝对不能编造任何不存在的信息\n\
2. 情感维度直接引用事件中的 emotion 字段,不要重新生成或推测\n\
3. 叙述要温情、真实,像在回忆过去\n\
4. 用中文输出,不超过 500 字\n\
5. 如果事实中有时间、地点、人物,要自然地融入叙述";

/// 格式化时间戳为可读字符串
fn format_timestamp(ts: u64) -> String {
    // 简单格式化:Unix 秒 → "YYYY-MM-DD HH:MM"
    // 不引入 chrono 依赖,用基本计算
    let secs = ts;
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;

    // 粗略计算日期(从 1970-01-01 开始)
    let (year, month, day) = days_to_date(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        year, month, day, hours, minutes
    )
}

/// Unix 天数 → (年, 月, 日) 粗略计算
fn days_to_date(days: u64) -> (u64, u64, u64) {
    let mut year = 1970u64;
    let mut remaining = days;

    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        year += 1;
    }

    let month_days: [u64; 12] = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 1u64;
    for &dm in &month_days {
        if remaining < dm {
            break;
        }
        remaining -= dm;
        month += 1;
    }

    (year, month, remaining + 1)
}

fn is_leap_year(year: u64) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

#[cfg(test)]
mod tests {
    use super::super::event::{
        ConversationSubtype, Emotion, EmotionSubject, EventSource, EventType, MilestoneSubtype,
    };
    use super::*;
    use crate::api::evorule_client::{EvoruleApiClient, FactLogEntry};

    fn make_store() -> MemoryEventStore {
        let client = EvoruleApiClient::new("http://localhost:9999");
        MemoryEventStore::new("agent_test", client).with_session_id("1")
    }

    fn make_event(id: &str, et: EventType, ts: u64) -> MemoryEvent {
        MemoryEvent::new_root(id, et, ts, EventSource::UserInput)
            .with_content(serde_json::json!({"summary": format!("event {}", id)}))
            .with_entity(crate::agent::memory_event::entity::EntityRef::new(
                "pet_doudou",
                "subject",
            ))
    }

    #[tokio::test]
    async fn test_replay_from_backward() {
        let store = make_store();
        let mut engine = ReplayEngine::new(store);

        // 手动构建因果链 E1 ← E2 ← E3
        let mut e1 = make_event("E001", EventType::EmotionEvent, 1000);
        e1.fact_id = Some(10);
        let mut e2 = make_event("E002", EventType::EmotionEvent, 2000);
        e2.cause = Some(10);
        e2.fact_id = Some(20);
        let mut e3 = make_event("E003", EventType::EmotionEvent, 3000);
        e3.cause = Some(20);

        let store = engine.store_mut();
        store.insert_event_raw(e1);
        store.insert_event_raw(e2);
        store.insert_event_raw(e3.clone());
        store.set_fact_mapping_raw(10, "E001".into());
        store.set_fact_mapping_raw(20, "E002".into());

        let chain = engine
            .replay_from("E003", ReplayDirection::Backward)
            .await
            .unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].event_id, "E003");
        assert_eq!(chain[1].event_id, "E002");
        assert_eq!(chain[2].event_id, "E001");
    }

    #[tokio::test]
    async fn test_replay_from_forward() {
        let store = make_store();
        let mut engine = ReplayEngine::new(store);

        let mut e1 = make_event("E001", EventType::EmotionEvent, 1000);
        e1.effects = vec![EventRef {
            event_id: "E002".to_string(),
            fact_id: Some(20),
        }];
        let mut e2 = make_event("E002", EventType::EmotionEvent, 2000);
        e2.effects = vec![EventRef {
            event_id: "E003".to_string(),
            fact_id: Some(30),
        }];
        let e3 = make_event("E003", EventType::EmotionEvent, 3000);

        let store = engine.store_mut();
        store.insert_event_raw(e1);
        store.insert_event_raw(e2);
        store.insert_event_raw(e3);

        let chain = engine
            .replay_from("E001", ReplayDirection::Forward)
            .await
            .unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].event_id, "E001");
        assert_eq!(chain[1].event_id, "E002");
        assert_eq!(chain[2].event_id, "E003");
    }

    #[tokio::test]
    async fn test_replay_by_entity() {
        let store = make_store();
        let mut engine = ReplayEngine::new(store);

        let e1 = make_event("E003", EventType::EmotionEvent, 3000);
        let e2 = make_event("E001", EventType::EmotionEvent, 1000);
        let e3 = make_event("E002", EventType::EmotionEvent, 2000);

        let s = engine.store_mut();
        let _ = s.write_event(e1).await;
        let _ = s.write_event(e2).await;
        let _ = s.write_event(e3).await;

        let timeline = engine.replay_by_entity("pet_doudou").await.unwrap();
        assert_eq!(timeline.len(), 3);
        assert_eq!(timeline[0].event_id, "E001"); // ts=1000
        assert_eq!(timeline[1].event_id, "E002"); // ts=2000
        assert_eq!(timeline[2].event_id, "E003"); // ts=3000
    }

    #[tokio::test]
    async fn test_replay_entity_timeline_with_range() {
        let store = make_store();
        let mut engine = ReplayEngine::new(store);

        let _ = engine
            .store_mut()
            .write_event(make_event("E001", EventType::EmotionEvent, 1000))
            .await;
        let _ = engine
            .store_mut()
            .write_event(make_event("E002", EventType::EmotionEvent, 2000))
            .await;
        let _ = engine
            .store_mut()
            .write_event(make_event("E003", EventType::EmotionEvent, 3000))
            .await;

        let timeline = engine
            .replay_entity_timeline("pet_doudou", Some(1500), Some(2500))
            .await
            .unwrap();
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].event_id, "E002"); // only ts=2000 is in [1500, 2500]
    }

    #[tokio::test]
    async fn test_narrate_without_llm() {
        let store = make_store();
        let mut engine = ReplayEngine::new(store); // no LLM

        let event = MemoryEvent::new_root(
            "E001",
            EventType::Milestone(MilestoneSubtype::Achievement),
            1721540000,
            EventSource::UserInput,
        )
        .with_content(serde_json::json!({"summary": "豆豆第一次叫妈妈"}))
        .with_emotion(Emotion::new(0.9, 0.8, EmotionSubject::User).with_labels(&["joy", "pride"]));

        let narrative = engine.narrate(&[event]).await.unwrap();
        assert!(!narrative.text.is_empty());
        assert_eq!(narrative.cited_events, vec!["E001"]);
        assert!(narrative.text.contains("Achievement"));
        assert!(narrative.text.contains("豆豆第一次叫妈妈"));
    }

    #[tokio::test]
    async fn test_narrate_with_mock_llm() {
        let store = make_store();
        let llm = LlmHandler::mock("豆豆14个月大时第一次叫了妈妈,你当时又惊又喜。");
        let mut engine = ReplayEngine::new(store).with_llm(llm);

        let event = MemoryEvent::new_root(
            "E001",
            EventType::Milestone(MilestoneSubtype::Achievement),
            1721540000,
            EventSource::UserInput,
        )
        .with_content(serde_json::json!({"summary": "豆豆第一次叫妈妈"}));

        let narrative = engine.narrate(&[event]).await.unwrap();
        assert!(narrative.text.contains("豆豆"));
        assert_eq!(narrative.cited_events, vec!["E001"]);
    }

    #[test]
    fn test_format_timestamp() {
        // 2024-07-21 00:00:00 UTC = 1721548800
        let ts = 1721548800u64;
        let formatted = format_timestamp(ts);
        assert!(formatted.starts_with("2024-07-"));
    }

    #[test]
    fn test_days_to_date() {
        // 1970-01-01 = day 0
        assert_eq!(days_to_date(0), (1970, 1, 1));
        // 1970-01-02 = day 1
        assert_eq!(days_to_date(1), (1970, 1, 2));
        // 1971-01-01 = day 365
        assert_eq!(days_to_date(365), (1971, 1, 1));
    }

    #[test]
    fn test_is_leap_year() {
        assert!(is_leap_year(2000));
        assert!(!is_leap_year(1900));
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(2023));
    }

    #[tokio::test]
    async fn test_narrate_deterministic_without_llm() {
        // 确定性测试:相同事件序列 → 相同输出(不含 LLM)
        let store1 = make_store();
        let store2 = make_store();
        let mut engine1 = ReplayEngine::new(store1);
        let mut engine2 = ReplayEngine::new(store2);

        let event = MemoryEvent::new_root(
            "E001",
            EventType::Conversation(ConversationSubtype::Farewell),
            1721540000,
            EventSource::UserInput,
        )
        .with_content(serde_json::json!({"summary": "最后一次通话"}))
        .with_emotion(
            Emotion::new(0.3, 0.6, EmotionSubject::User).with_labels(&["love", "nostalgia"]),
        );

        let n1 = engine1.narrate(&[event.clone()]).await.unwrap();
        let n2 = engine2.narrate(&[event]).await.unwrap();
        assert_eq!(
            n1.text, n2.text,
            "narrate without LLM must be deterministic"
        );
    }

    // ===== B3 Fact 流回放测试 =====

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
        let json = r#"{"version": 2, "type": "PayloadUpdate", "fact_id": 20, "path": "__memory__.agent_test.events.E001", "value": {"event_id": "E001", "event_type": {"kind": "EmotionEvent"}, "timestamp": 1000}}"#;
        let entry: FactLogEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.fact_type, "PayloadUpdate");
        assert_eq!(entry.id, 20);
        assert_eq!(entry.path(), Some("__memory__.agent_test.events.E001"));
        assert!(entry.value().is_some());
        assert_eq!(entry.value().unwrap()["event_id"], "E001");

        // Stable 变体
        let json = r#"{"version": 3, "type": "Stable", "fact_id": 30, "final_snapshot": {"payload": "done"}}"#;
        let entry: FactLogEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.fact_type, "Stable");
        assert_eq!(entry.id, 30);
        assert_eq!(entry.payload["final_snapshot"]["payload"], "done");
    }

    #[test]
    fn test_conversation_turn_types() {
        // 所有变体互不相等
        let types = [
            TurnType::UserCommand,
            TurnType::ToolCall,
            TurnType::ToolResult,
            TurnType::Stable,
        ];
        for i in 0..types.len() {
            for j in (i + 1)..types.len() {
                assert_ne!(types[i], types[j], "TurnType variants must be distinct");
            }
        }
        // Copy 语义
        let t = TurnType::UserCommand;
        let t_copy = t;
        assert_eq!(t, t_copy);
    }

    #[tokio::test]
    async fn test_replay_facts_degraded_no_server() {
        // server 不可用（localhost:9999 无监听）→ replay_facts 返回 Err
        let store = make_store();
        let engine = ReplayEngine::new(store);
        let result = engine.replay_facts(None, None).await;
        assert!(
            result.is_err(),
            "replay_facts should fail when server is unavailable"
        );
    }

    #[tokio::test]
    async fn test_replay_events_from_fact_stream_degraded() {
        // server 不可用 → replay_events_from_fact_stream 返回 Err
        let store = make_store();
        let engine = ReplayEngine::new(store);
        let result = engine.replay_events_from_fact_stream(None, None).await;
        assert!(
            result.is_err(),
            "replay_events_from_fact_stream should fail when server is unavailable"
        );
    }

    #[tokio::test]
    async fn test_project_at_version_degraded() {
        // server 不可用 → project_at_version 返回 Err
        let store = make_store();
        let engine = ReplayEngine::new(store);
        let result = engine.project_at_version(1).await;
        assert!(
            result.is_err(),
            "project_at_version should fail when server is unavailable"
        );
    }

    // ===== B4：narrate_with_evidence 测试 =====

    #[tokio::test]
    async fn test_narrate_with_evidence_degraded() {
        // server 不可用 → narrate 仍工作（无 LLM 结构化摘要），evidence_map 为空（fail-open）
        let store = make_store();
        let mut engine = ReplayEngine::new(store);

        let mut event = make_event("E001", EventType::EmotionEvent, 1000);
        event.fact_id = Some(42);
        engine.store_mut().insert_event_raw(event);

        let events = engine.store().list_events();
        let result = engine.narrate_with_evidence(&events).await;
        assert!(
            result.is_ok(),
            "narrate_with_evidence should succeed in degraded mode"
        );
        let n = result.unwrap();
        assert!(!n.text.is_empty(), "narrative text should not be empty");
        assert!(n.cited_events.contains(&"E001".to_string()));
        // server 不可用 → evidence_for_event fail-open 但返回 Some(fail-open evidence)
        // 实际上 evidence_for_event 会返回 Some(verified=false) 证据，所以 evidence_map 非空
        // 但如果 fact_id 为 None 则跳过。这里 fact_id=Some(42) → evidence_for_event 返回 Some
        assert!(
            !n.evidence.is_empty(),
            "evidence map should have entry for E001 (fail-open)"
        );
        let ev_str = n.evidence.get("E001").unwrap();
        assert!(ev_str.contains("fact#42"), "evidence string: {}", ev_str);
        assert!(
            ev_str.contains("✗"),
            "degraded evidence should be unverified: {}",
            ev_str
        );
    }

    #[tokio::test]
    async fn test_narrate_with_evidence_no_fact_id_empty_evidence() {
        // 事件无 fact_id → evidence_for_event 返回 None → evidence_map 为空
        let store = make_store();
        let mut engine = ReplayEngine::new(store);

        let event = make_event("E001", EventType::EmotionEvent, 1000);
        // fact_id 为 None（make_event 不设置 fact_id）
        engine.store_mut().insert_event_raw(event);

        let events = engine.store().list_events();
        let result = engine.narrate_with_evidence(&events).await;
        assert!(result.is_ok());
        let n = result.unwrap();
        assert!(!n.text.is_empty());
        assert!(n.cited_events.contains(&"E001".to_string()));
        assert!(
            n.evidence.is_empty(),
            "events without fact_id should produce empty evidence map"
        );
    }

    #[tokio::test]
    async fn test_narrate_with_evidence_empty_events() {
        // 空事件列表 → 空叙述 + 空 evidence
        let store = make_store();
        let mut engine = ReplayEngine::new(store);

        let result = engine.narrate_with_evidence(&[]).await;
        assert!(result.is_ok());
        let n = result.unwrap();
        // narrate 对空事件返回空文本
        assert!(n.text.is_empty() || n.text.len() < 50);
        assert!(n.cited_events.is_empty());
        assert!(n.evidence.is_empty());
    }
}
