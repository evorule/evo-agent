// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 032 MemoryEvent —— 结构化记忆事件核心类型
//!
//! 与 031 的 `MemoryRecord`(KV)不同,`MemoryEvent` 是结构化的事件,
//! 携带因果链(`cause`)、实体引用(`entities`)、情感维度(`emotion`),
//! 支持"则灵"人生回放的确定性回放。
//!
//! ## 设计要点(032 设计文档 §四)
//!
//! - `event_type` 是业务语义分类(11 种),不是 evorule 控制流类型
//! - `content` 是开放式 JSON,每种 event_type 有约定 schema(由应用层校验)
//! - `emotion` 是一等公民,事件发生时定型,回放时不允许 LLM 重新生成
//! - `cause: Option<u64>` 指向触发本事件的源 FactId(应用层因果,不改 Fact 枚举)
//! - `effects: Vec<String>` 反向链,记录本事件触发的下游事件 ID

use serde::{Deserialize, Serialize};

/// FactId 类型别名(evo-agent 侧用 u64,与 evorule 的 FactId 对应)
pub type FactId = u64;

/// 记忆事件 —— 032 的核心数据单元
///
/// 与 031 的 `MemoryRecord`(KV)不同,`MemoryEvent` 是结构化的事件,
/// 携带因果链、实体引用、情感维度,支持确定性回放。
///
/// 存储路径:`__memory__.agent_{type}.events.{event_id}`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryEvent {
    /// 事件唯一 ID(应用层生成,格式如 "E001" 或 UUID v7)
    pub event_id: String,

    /// 事件类型(见 [`EventType`])
    pub event_type: EventType,

    /// 事件发生时间(Unix 秒)
    pub timestamp: u64,

    /// 关联实体列表(见 [`crate::agent::memory_event::entity::EntityRef`])
    #[serde(default)]
    pub entities: Vec<crate::agent::memory_event::entity::EntityRef>,

    /// 结构化内容(因 event_type 而异,开放式 JSON)
    pub content: serde_json::Value,

    /// 情感维度(可选,见 [`Emotion`])
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emotion: Option<Emotion>,

    /// 因果链:触发本事件的源 FactId
    ///
    /// - `None` — 根事件(用户主动输入、系统定时观察)
    /// - `Some(fact_id)` — 指向触发本事件的源 Fact
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<FactId>,

    /// 派生事件:本事件触发的下游事件 ID 列表(反向链)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<String>,

    /// 来源(见 [`EventSource`])
    pub source: EventSource,

    /// 置信度(0.0-1.0,user_input=1.0,llm_extraction 视情况)
    #[serde(default = "default_confidence")]
    pub confidence: f32,

    /// 标签(自由分类,与 031 兼容)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,

    /// 关联的 evorule session(便于跨会话追溯)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,

    /// 关联的 evorule FactId(本事件写入 evorule 后由 system 填充)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact_id: Option<FactId>,
}

fn default_confidence() -> f32 {
    1.0
}

impl MemoryEvent {
    /// 创建根事件(无 cause)
    pub fn new_root(
        event_id: &str,
        event_type: EventType,
        timestamp: u64,
        source: EventSource,
    ) -> Self {
        Self {
            event_id: event_id.to_string(),
            event_type,
            timestamp,
            entities: Vec::new(),
            content: serde_json::Value::Null,
            emotion: None,
            cause: None,
            effects: Vec::new(),
            source,
            confidence: 1.0,
            tags: Vec::new(),
            session_id: None,
            fact_id: None,
        }
    }

    /// 创建带因果的事件
    pub fn with_cause(mut self, cause: FactId) -> Self {
        self.cause = Some(cause);
        self
    }

    /// 设置结构化内容
    pub fn with_content(mut self, content: serde_json::Value) -> Self {
        self.content = content;
        self
    }

    /// 设置情感
    pub fn with_emotion(mut self, emotion: Emotion) -> Self {
        self.emotion = Some(emotion);
        self
    }

    /// 添加实体引用
    pub fn with_entity(
        mut self,
        entity_ref: crate::agent::memory_event::entity::EntityRef,
    ) -> Self {
        self.entities.push(entity_ref);
        self
    }

    /// 设置 session_id
    pub fn with_session(mut self, session_id: &str) -> Self {
        self.session_id = Some(session_id.to_string());
        self
    }

    /// 设置置信度
    pub fn with_confidence(mut self, confidence: f32) -> Self {
        self.confidence = confidence;
        self
    }

    /// 添加标签
    pub fn with_tag(mut self, tag: &str) -> Self {
        self.tags.push(tag.to_string());
        self
    }
}

/// 事件类型 —— 业务语义分类,不是 evorule 控制流类型
///
/// 设计原则:
/// - 枚举值是开放集合,`Custom(String)` 保留扩展
/// - 不与 evorule 的 ControlFlowType/IOType 重叠
/// - serde 表示:`{"kind": "Conversation", "subtype": "Farewell"}`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "subtype")]
pub enum EventType {
    /// 对话事件
    Conversation(ConversationSubtype),
    /// 关系事件(相识/分手/和解/迁居)
    Relationship(RelationshipSubtype),
    /// 里程碑事件(生日/毕业/入职/离职)
    Milestone(MilestoneSubtype),
    /// 习惯事件(每日打卡/中断/恢复)
    Habit(HabitSubtype),
    /// 健康事件
    Health(HealthSubtype),
    /// 情绪事件(显著情绪波动)
    EmotionEvent,
    /// 位置事件(到达/离开)
    Location(LocationSubtype),
    /// 物品事件(购入/丢失/赠送)
    Item(ItemSubtype),
    /// I/O 触发事件(天气查询/外部 API 调用结果)
    IOTrigger(IOTriggerSubtype),
    /// 系统观察(自动检测到的模式,如"本周通话频率下降")
    SystemObservation,
    /// 自定义(业务扩展)
    Custom(String),
}

impl EventType {
    /// 获取 kind 字符串(用于索引/查询)
    pub fn kind_str(&self) -> &str {
        match self {
            EventType::Conversation(_) => "Conversation",
            EventType::Relationship(_) => "Relationship",
            EventType::Milestone(_) => "Milestone",
            EventType::Habit(_) => "Habit",
            EventType::Health(_) => "Health",
            EventType::EmotionEvent => "EmotionEvent",
            EventType::Location(_) => "Location",
            EventType::Item(_) => "Item",
            EventType::IOTrigger(_) => "IOTrigger",
            EventType::SystemObservation => "SystemObservation",
            EventType::Custom(_) => "Custom",
        }
    }
}

/// 对话事件子类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ConversationSubtype {
    /// 首次交流
    FirstInteraction,
    /// 道歉
    Apology,
    /// 表白
    Confession,
    /// 告别
    Farewell,
    /// 冲突
    Conflict,
    /// 和解
    Reconciliation,
    /// 日常
    Routine,
}

/// 关系事件子类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RelationshipSubtype {
    /// 相识
    Met,
    /// 走近
    BecameClose,
    /// 疏远
    DriftedApart,
    /// 重新联系
    Reconnected,
    /// 失去(去世/失联)
    Lost,
}

/// 里程碑事件子类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MilestoneSubtype {
    /// 生日
    Birthday,
    /// 毕业
    Graduation,
    /// 第一份工作
    FirstJob,
    /// 新工作
    NewJob,
    /// 离职
    Leaving,
    /// 成就
    Achievement,
}

/// 习惯事件子类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum HabitSubtype {
    /// 开始习惯
    Started,
    /// 打卡
    CheckedIn,
    /// 缺失
    Missed,
    /// 连续达成
    Streak,
    /// 中断
    Broken,
    /// 恢复
    Resumed,
}

/// 健康事件子类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum HealthSubtype {
    /// 症状
    Symptom,
    /// 诊断
    Diagnosis,
    /// 治疗
    Treatment,
    /// 康复
    Recovery,
}

/// 位置事件子类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LocationSubtype {
    /// 到达
    Arrived,
    /// 离开
    Left,
    /// 访问
    Visited,
}

/// 物品事件子类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ItemSubtype {
    /// 获得物品
    Acquired,
    /// 丢失物品
    Lost,
    /// 赠送物品
    Gifted,
    /// 丢弃物品
    Discarded,
}

/// I/O 触发事件子类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IOTriggerSubtype {
    /// 天气预警
    WeatherAlert,
    /// 日历提醒
    CalendarReminder,
    /// 外部通知
    ExternalNotification,
}

/// 情感维度 —— 一等公民,不可由 LLM 事后编造
///
/// 事件发生时由用户显式标记或 LLM 从对话中提取(但提取结果进 `confidence` 字段)。
/// 回放时情感直接读取,不允许 LLM 重新生成。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Emotion {
    /// 情感极性(-1.0 极负 ~ +1.0 极正)
    pub valence: f32,

    /// 情感强度(0.0 平静 ~ 1.0 极强烈)
    pub arousal: f32,

    /// 情感标签(开放集合,如 joy/sadness/anger/fear/love/nostalgia)
    #[serde(default)]
    pub labels: Vec<String>,

    /// 情感来源(谁的情感)
    pub subject: EmotionSubject,
}

impl Emotion {
    /// 创建情感
    pub fn new(valence: f32, arousal: f32, subject: EmotionSubject) -> Self {
        Self {
            valence: valence.clamp(-1.0, 1.0),
            arousal: arousal.clamp(0.0, 1.0),
            labels: Vec::new(),
            subject,
        }
    }

    /// 添加情感标签
    pub fn with_label(mut self, label: &str) -> Self {
        self.labels.push(label.to_string());
        self
    }

    /// 添加多个情感标签
    pub fn with_labels(mut self, labels: &[&str]) -> Self {
        self.labels.extend(labels.iter().map(|s| s.to_string()));
        self
    }
}

/// 情感来源(谁的情感)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EmotionSubject {
    /// 用户的情感
    User,
    /// 某实体的情感(如宠物的快乐)
    Entity(String),
    /// 群体情感(如家庭氛围)
    Collective,
}

/// 事件来源
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EventSource {
    /// 用户主动输入
    UserInput,
    /// LLM 辅助提取
    LlmExtraction,
    /// 工具结果触发
    ToolResult,
    /// I/O 响应触发
    IoResponse,
    /// 系统自动观察
    SystemObservation,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_event_serialize_roundtrip() {
        let event = MemoryEvent::new_root(
            "E001",
            EventType::Milestone(MilestoneSubtype::Achievement),
            1721540000,
            EventSource::UserInput,
        )
        .with_content(serde_json::json!({
            "milestone": "first_word_mama",
            "age_months": 14
        }))
        .with_emotion(
            Emotion::new(0.9, 0.8, EmotionSubject::User).with_labels(&["joy", "pride", "surprise"]),
        )
        .with_entity(crate::agent::memory_event::entity::EntityRef::new(
            "pet_doudou",
            "subject",
        ))
        .with_session("123")
        .with_tag("milestone");

        let json = serde_json::to_string(&event).unwrap();
        let deserialized: MemoryEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_event_type_serde_with_subtype() {
        let et = EventType::Conversation(ConversationSubtype::Farewell);
        let json = serde_json::to_string(&et).unwrap();
        assert!(json.contains("\"kind\":\"Conversation\""));
        assert!(json.contains("\"subtype\":\"Farewell\""));

        let de: EventType = serde_json::from_str(&json).unwrap();
        assert_eq!(et, de);
    }

    #[test]
    fn test_event_type_serde_no_subtype() {
        let et = EventType::EmotionEvent;
        let json = serde_json::to_string(&et).unwrap();
        assert!(json.contains("\"kind\":\"EmotionEvent\""));
        assert!(!json.contains("subtype"));

        let de: EventType = serde_json::from_str(&json).unwrap();
        assert_eq!(et, de);
    }

    #[test]
    fn test_event_type_serde_system_observation() {
        let et = EventType::SystemObservation;
        let json = serde_json::to_string(&et).unwrap();
        let de: EventType = serde_json::from_str(&json).unwrap();
        assert_eq!(et, de);
    }

    #[test]
    fn test_event_type_custom() {
        let et = EventType::Custom("emotion_correction".to_string());
        let json = serde_json::to_string(&et).unwrap();
        assert!(json.contains("\"kind\":\"Custom\""));
        assert!(json.contains("\"subtype\":\"emotion_correction\""));

        let de: EventType = serde_json::from_str(&json).unwrap();
        assert_eq!(et, de);
    }

    #[test]
    fn test_event_type_kind_str() {
        assert_eq!(EventType::EmotionEvent.kind_str(), "EmotionEvent");
        assert_eq!(
            EventType::Conversation(ConversationSubtype::Routine).kind_str(),
            "Conversation"
        );
        assert_eq!(EventType::SystemObservation.kind_str(), "SystemObservation");
        assert_eq!(EventType::Custom("x".into()).kind_str(), "Custom");
    }

    #[test]
    fn test_emotion_clamp() {
        let e = Emotion::new(2.0, -1.0, EmotionSubject::User);
        assert_eq!(e.valence, 1.0); // clamped to [-1.0, 1.0]
        assert_eq!(e.arousal, 0.0); // clamped to [0.0, 1.0]
    }

    #[test]
    fn test_emotion_subject_serde() {
        let subjects = vec![
            EmotionSubject::User,
            EmotionSubject::Entity("pet_doudou".to_string()),
            EmotionSubject::Collective,
        ];
        for s in subjects {
            let json = serde_json::to_string(&s).unwrap();
            let de: EmotionSubject = serde_json::from_str(&json).unwrap();
            assert_eq!(s, de);
        }
    }

    #[test]
    fn test_event_source_serde() {
        let sources = vec![
            EventSource::UserInput,
            EventSource::LlmExtraction,
            EventSource::ToolResult,
            EventSource::IoResponse,
            EventSource::SystemObservation,
        ];
        for s in sources {
            let json = serde_json::to_string(&s).unwrap();
            let de: EventSource = serde_json::from_str(&json).unwrap();
            assert_eq!(s, de);
        }
    }

    #[test]
    fn test_event_with_cause() {
        let event = MemoryEvent::new_root(
            "E002",
            EventType::SystemObservation,
            1721630000,
            EventSource::SystemObservation,
        )
        .with_cause(42);

        assert_eq!(event.cause, Some(42));
        assert_eq!(event.confidence, 1.0);
    }

    #[test]
    fn test_all_event_subtypes_roundtrip() {
        // 确保所有子类型枚举都能正确序列化/反序列化
        let subtypes: Vec<EventType> = vec![
            EventType::Conversation(ConversationSubtype::FirstInteraction),
            EventType::Conversation(ConversationSubtype::Apology),
            EventType::Conversation(ConversationSubtype::Confession),
            EventType::Conversation(ConversationSubtype::Farewell),
            EventType::Conversation(ConversationSubtype::Conflict),
            EventType::Conversation(ConversationSubtype::Reconciliation),
            EventType::Conversation(ConversationSubtype::Routine),
            EventType::Relationship(RelationshipSubtype::Met),
            EventType::Relationship(RelationshipSubtype::BecameClose),
            EventType::Relationship(RelationshipSubtype::DriftedApart),
            EventType::Relationship(RelationshipSubtype::Reconnected),
            EventType::Relationship(RelationshipSubtype::Lost),
            EventType::Milestone(MilestoneSubtype::Birthday),
            EventType::Milestone(MilestoneSubtype::Graduation),
            EventType::Milestone(MilestoneSubtype::FirstJob),
            EventType::Milestone(MilestoneSubtype::NewJob),
            EventType::Milestone(MilestoneSubtype::Leaving),
            EventType::Milestone(MilestoneSubtype::Achievement),
            EventType::Habit(HabitSubtype::Started),
            EventType::Habit(HabitSubtype::CheckedIn),
            EventType::Habit(HabitSubtype::Missed),
            EventType::Habit(HabitSubtype::Streak),
            EventType::Habit(HabitSubtype::Broken),
            EventType::Habit(HabitSubtype::Resumed),
            EventType::Health(HealthSubtype::Symptom),
            EventType::Health(HealthSubtype::Diagnosis),
            EventType::Health(HealthSubtype::Treatment),
            EventType::Health(HealthSubtype::Recovery),
            EventType::Location(LocationSubtype::Arrived),
            EventType::Location(LocationSubtype::Left),
            EventType::Location(LocationSubtype::Visited),
            EventType::Item(ItemSubtype::Acquired),
            EventType::Item(ItemSubtype::Lost),
            EventType::Item(ItemSubtype::Gifted),
            EventType::Item(ItemSubtype::Discarded),
            EventType::IOTrigger(IOTriggerSubtype::WeatherAlert),
            EventType::IOTrigger(IOTriggerSubtype::CalendarReminder),
            EventType::IOTrigger(IOTriggerSubtype::ExternalNotification),
            EventType::EmotionEvent,
            EventType::SystemObservation,
            EventType::Custom("test".to_string()),
        ];

        for et in &subtypes {
            let json = serde_json::to_string(et).unwrap();
            let de: EventType = serde_json::from_str(&json).unwrap();
            assert_eq!(*et, de, "roundtrip failed for: {}", json);
        }
    }

    #[test]
    fn test_event_skip_empty_fields() {
        // effects/tags 为空时不应出现在 JSON 中(skip_serializing_if)
        let event =
            MemoryEvent::new_root("E001", EventType::EmotionEvent, 100, EventSource::UserInput);
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("effects"));
        assert!(!json.contains("tags"));
        assert!(!json.contains("emotion"));
        assert!(!json.contains("cause"));
        assert!(!json.contains("fact_id"));
    }
}
