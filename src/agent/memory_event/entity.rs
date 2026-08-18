// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 032 Entity 模型 —— 人/宠物/地点/项目/习惯作为一等公民
//!
//! 实体支持"某实体的所有事件"查询,是 032 确定性回放的关键维度。
//! 实体本身也通过 `Fact::PayloadUpdate` 存储在 evorule payload,进入审计链。
//!
//! ## 存储路径(032 设计文档 §5.4)
//!
//! ```text
//! __memory__.agent_{type}.entities.{entity_id}   # Entity 完整定义
//! __memory__.agent_{type}.index.by_entity.{id}   # 反向索引(可选)
//! __memory__.agent_{type}.index.by_type.{type}   # 反向索引(可选)
//! ```

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::event::EventType;

/// 实体 —— 一等公民,支持"某实体的所有事件"查询
///
/// entity_id 不变(应用层生成,如 "person_doudou"),display_name 可变。
/// 实体状态(active/archived/lost/deceased)支持生命周期管理。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entity {
    /// 实体唯一 ID(应用层生成,如 "person_mom" / "pet_doudou")
    pub entity_id: String,

    /// 实体类型
    pub entity_type: EntityType,

    /// 显示名(可变,但 entity_id 不变)
    pub display_name: String,

    /// 别名(用于自然语言匹配)
    #[serde(default)]
    pub aliases: Vec<String>,

    /// 创建时间(Unix 秒)
    pub created_at: u64,

    /// 属性(开放 KV,业务自定义)
    #[serde(default)]
    pub attributes: serde_json::Value,

    /// 实体状态
    #[serde(default)]
    pub status: EntityStatus,
}

impl Entity {
    /// 创建新实体
    pub fn new(
        entity_id: &str,
        entity_type: EntityType,
        display_name: &str,
        created_at: u64,
    ) -> Self {
        Self {
            entity_id: entity_id.to_string(),
            entity_type,
            display_name: display_name.to_string(),
            aliases: Vec::new(),
            created_at,
            attributes: serde_json::Value::Null,
            status: EntityStatus::Active,
        }
    }

    /// 添加别名
    pub fn with_alias(mut self, alias: &str) -> Self {
        self.aliases.push(alias.to_string());
        self
    }

    /// 设置属性
    pub fn with_attributes(mut self, attrs: serde_json::Value) -> Self {
        self.attributes = attrs;
        self
    }

    /// 设置状态
    pub fn with_status(mut self, status: EntityStatus) -> Self {
        self.status = status;
        self
    }
}

/// 实体类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EntityType {
    /// 人
    Person,
    /// 宠物
    Pet,
    /// 地点
    Place,
    /// 项目
    Project,
    /// 习惯
    Habit,
    /// 组织
    Organization,
    /// 物品
    Object,
    /// 自定义
    Custom(String),
}

/// 实体状态
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[derive(Default)]
pub enum EntityStatus {
    /// 活跃
    #[default]
    Active,
    /// 归档
    Archived,
    /// 失联/丢失
    Lost,
    /// 去世(人/宠物)
    Deceased,
}


/// 实体引用 —— 出现在 `MemoryEvent.entities` 中
///
/// 只引用 entity_id + 角色,不内联完整 Entity 定义。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntityRef {
    /// 引用的实体 ID
    pub entity_id: String,

    /// 该实体在本事件中的角色(开放字符串)
    ///
    /// 如 "subject" / "recipient" / "participant" / "location"
    pub role: String,
}

impl EntityRef {
    /// 创建实体引用
    pub fn new(entity_id: &str, role: &str) -> Self {
        Self {
            entity_id: entity_id.to_string(),
            role: role.to_string(),
        }
    }
}

/// 实体索引 —— 本地反向索引(非 evorule 核心)
///
/// 维护两个反向索引,加速"某实体的所有事件"和"某类型的所有事件"查询。
/// 索引在本地 cache 维护,不每次写 evorule(性能优化)。
#[derive(Debug, Clone, Default)]
pub struct EntityIndex {
    /// entity_id → 关联事件 ID 列表(按时间倒序)
    by_entity: HashMap<String, Vec<String>>,

    /// event_type kind_str → 事件 ID 列表
    by_type: HashMap<String, Vec<String>>,
}

impl EntityIndex {
    /// 创建空索引
    pub fn new() -> Self {
        Self::default()
    }

    /// 添加事件到索引
    ///
    /// 去重:同一事件可能在多个角色中引用同一实体(如既是 subject 又是 observer),
    /// 但索引中每个事件对每个实体只记录一次。
    pub fn index_event(&mut self, event_id: &str, event: &super::event::MemoryEvent) {
        // by_entity: 每个关联实体都加入(去重)
        for ent_ref in &event.entities {
            let entry = self.by_entity.entry(ent_ref.entity_id.clone()).or_default();
            if !entry.iter().any(|id| id == event_id) {
                entry.push(event_id.to_string());
            }
        }

        // by_type: 按 kind_str 索引(去重)
        let entry = self
            .by_type
            .entry(event.event_type.kind_str().to_string())
            .or_default();
        if !entry.iter().any(|id| id == event_id) {
            entry.push(event_id.to_string());
        }
    }

    /// 查询某实体的所有事件 ID
    pub fn events_for_entity(&self, entity_id: &str) -> Vec<&str> {
        self.by_entity
            .get(entity_id)
            .map(|v| v.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default()
    }

    /// 查询某事件类型的所有事件 ID
    pub fn events_for_type(&self, event_type: &EventType) -> Vec<&str> {
        self.by_type
            .get(event_type.kind_str())
            .map(|v| v.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default()
    }

    /// 从索引中移除事件
    pub fn remove_event(&mut self, event_id: &str, event: &super::event::MemoryEvent) {
        for ent_ref in &event.entities {
            if let Some(list) = self.by_entity.get_mut(&ent_ref.entity_id) {
                list.retain(|id| id != event_id);
                if list.is_empty() {
                    self.by_entity.remove(&ent_ref.entity_id);
                }
            }
        }
        let kind = event.event_type.kind_str().to_string();
        if let Some(list) = self.by_type.get_mut(&kind) {
            list.retain(|id| id != event_id);
            if list.is_empty() {
                self.by_type.remove(&kind);
            }
        }
    }

    /// 清空索引
    pub fn clear(&mut self) {
        self.by_entity.clear();
        self.by_type.clear();
    }

    /// 索引中的实体数量
    pub fn entity_count(&self) -> usize {
        self.by_entity.len()
    }

    /// 索引中的类型数量
    pub fn type_count(&self) -> usize {
        self.by_type.len()
    }
}

#[cfg(test)]
mod tests {
    use super::super::event::{EventSource, EventType, MemoryEvent, MilestoneSubtype};
    use super::*;

    #[test]
    fn test_entity_serialize_roundtrip() {
        let entity = Entity::new("pet_doudou", EntityType::Pet, "豆豆", 1721540000)
            .with_alias("小豆")
            .with_attributes(serde_json::json!({"species": "dog", "breed": "golden"}))
            .with_status(EntityStatus::Active);

        let json = serde_json::to_string(&entity).unwrap();
        let de: Entity = serde_json::from_str(&json).unwrap();
        assert_eq!(entity, de);
    }

    #[test]
    fn test_entity_status_default() {
        assert_eq!(EntityStatus::default(), EntityStatus::Active);
    }

    #[test]
    fn test_entity_type_custom() {
        let et = EntityType::Custom("vehicle".to_string());
        let json = serde_json::to_string(&et).unwrap();
        let de: EntityType = serde_json::from_str(&json).unwrap();
        assert_eq!(et, de);
    }

    #[test]
    fn test_entity_ref_new() {
        let r = EntityRef::new("person_mom", "recipient");
        assert_eq!(r.entity_id, "person_mom");
        assert_eq!(r.role, "recipient");
    }

    #[test]
    fn test_entity_index_basic() {
        let mut index = EntityIndex::new();
        let event = MemoryEvent::new_root(
            "E001",
            EventType::Milestone(MilestoneSubtype::Achievement),
            1721540000,
            EventSource::UserInput,
        )
        .with_entity(EntityRef::new("pet_doudou", "subject"))
        .with_entity(EntityRef::new("person_me", "observer"));

        index.index_event("E001", &event);

        // by_entity 查询
        let doudou_events = index.events_for_entity("pet_doudou");
        assert_eq!(doudou_events, vec!["E001"]);

        let me_events = index.events_for_entity("person_me");
        assert_eq!(me_events, vec!["E001"]);

        // by_type 查询
        let milestone_events =
            index.events_for_type(&EventType::Milestone(MilestoneSubtype::Birthday));
        assert_eq!(milestone_events, vec!["E001"]);

        assert_eq!(index.entity_count(), 2);
        assert_eq!(index.type_count(), 1);
    }

    #[test]
    fn test_entity_index_remove() {
        let mut index = EntityIndex::new();
        let event = MemoryEvent::new_root(
            "E001",
            EventType::Milestone(MilestoneSubtype::Achievement),
            1721540000,
            EventSource::UserInput,
        )
        .with_entity(EntityRef::new("pet_doudou", "subject"));

        index.index_event("E001", &event);
        assert_eq!(index.events_for_entity("pet_doudou"), vec!["E001"]);

        index.remove_event("E001", &event);
        assert!(index.events_for_entity("pet_doudou").is_empty());
        assert_eq!(index.entity_count(), 0);
    }

    #[test]
    fn test_entity_index_clear() {
        let mut index = EntityIndex::new();
        let event =
            MemoryEvent::new_root("E001", EventType::EmotionEvent, 100, EventSource::UserInput)
                .with_entity(EntityRef::new("e1", "subject"));

        index.index_event("E001", &event);
        assert!(!index.events_for_entity("e1").is_empty());

        index.clear();
        assert!(index.events_for_entity("e1").is_empty());
        assert_eq!(index.entity_count(), 0);
    }

    #[test]
    fn test_entity_with_deceased_status() {
        let entity = Entity::new("person_mom", EntityType::Person, "妈妈", 1000)
            .with_status(EntityStatus::Deceased);

        let json = serde_json::to_string(&entity).unwrap();
        let de: Entity = serde_json::from_str(&json).unwrap();
        assert_eq!(entity, de);
        assert_eq!(de.status, EntityStatus::Deceased);
    }
}
