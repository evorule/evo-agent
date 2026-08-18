// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 032 MemoryEvent 模块 —— 结构化记忆事件 + 因果链 + 确定性回放
//!
//! ## 模块结构
//!
//! ```text
//! src/agent/memory_event/
//! ├── mod.rs          # 本文件:模块声明 + re-exports
//! ├── event.rs        # MemoryEvent + EventType + Emotion + EventSource
//! ├── entity.rs       # Entity + EntityRef + EntityIndex
//! ├── store.rs        # MemoryEventStore:CRUD + evorule payload 写入 + 因果链维护
//! ├── replay.rs       # ReplayEngine:因果链遍历 + 确定性回放 + NL 包装
//! └── extraction.rs   # EventExtractor:从对话/工具结果中提取 MemoryEvent
//! ```
//!
//! ## 与 031 的关系
//!
//! - **031 `MemoryRecord`**(KV):偏好/配置/短期状态,路径 `__memory__.agent_X.session_Y.{key}`
//! - **032 `MemoryEvent`**(结构化):人生事件/对话里程碑/情感时刻,路径 `__memory__.agent_X.events.{event_id}`
//! - 两者共存,不是替换(032 设计文档 §九)
//!
//! ## 不改 evorule 核心(严守 AGENTS.md)
//!
//! - 只用 `Fact::PayloadUpdate`,不改 Fact 枚举
//! - 不改 Reactor 主循环
//! - 因果链在应用层(`cause` 在事件 JSON 里,非 Fact 原生)

pub mod entity;
pub mod event;
pub mod evidence;
pub mod extraction;
pub mod replay;
pub mod store;

// 核心类型 re-export
pub use entity::{Entity, EntityIndex, EntityRef, EntityStatus, EntityType};
pub use event::{
    ConversationSubtype, Emotion, EmotionSubject, EventSource, EventType, FactId, HabitSubtype,
    HealthSubtype, IOTriggerSubtype, ItemSubtype, LocationSubtype, MemoryEvent, MilestoneSubtype,
    RelationshipSubtype,
};
pub use extraction::{EventExtractor, ExtractionConfig, ExtractionTrigger};
pub use replay::{Narrative, ReplayDirection, ReplayEngine};
pub use store::{MemoryEventStore, StoreError};
