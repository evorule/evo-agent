// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 会话索引与历史回放 —— 对话与历史阶段(工作台)的会话枚举 + 消息投影
//!
//! ## 数据源与职责边界
//!
//! - **消息历史**:`MemoryManager` 已把每条消息持久化到 evorule
//!   payload(`__memory__.{ns}.session_{sid}.messages.{idx}`,P0 短期记忆持久化,
//!   进 FactsLog 审计链)。本模块只做**读取投影**(`load_transcript`:get_facts
//!   前缀读 → 同 idx 后写覆盖 → 按 idx 排序),零写入、零新事实类型。
//! - **会话枚举**:evorule facts 以会话为维度,无跨会话枚举端点;本模块维护
//!   serve 侧本地索引(JSONL,append-only,读时按 session_id 去重:
//!   created_at 取最小 / last_active 取最大 / title 取首个非空),
//!   由 WS 处理器在 SessionCreated / TurnEnd 时记录。
//!
//! ## 边界说明(设计留痕)
//!
//! - 索引只覆盖经过 serve WS 面创建的会话(工作台消费面);HTTP `run` 响应
//!   本身不含 session_id(O-086「id 未关联」),该路径不挂索引,console 侧
//!   恢复属 console 仓任务;
//! - 索引文件是**持久化记录**而非审计链级留痕(可读、不参与哈希);真相源
//!   仍是 evorule(payload/facts),索引丢失仅影响列表展示,消息历史可凭
//!   session_id 随时重新投影。

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::agent::memory::MessageRecord;
use crate::api::evorule_client::EvoruleApiClient;

/// 当前 Unix 秒(时钟回退兜底为 0)
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 会话索引条目(去重合并后的视图)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionIndexEntry {
    /// evorule 会话 ID
    pub session_id: String,
    /// agent 类型(工作台当前 = general)
    pub agent_type: String,
    /// 首次创建时间(Unix 秒)
    pub created_at: u64,
    /// 最近活跃时间(Unix 秒)
    pub last_active: u64,
    /// 标题(首轮用户消息截断;空 = 未记录)
    #[serde(default)]
    pub title: String,
}

/// 会话索引(JSONL append-only,读时去重)
#[derive(Debug)]
pub struct SessionIndex {
    path: Mutex<PathBuf>,
}

impl SessionIndex {
    /// 构造索引(路径 = `<workdir>/data/session_index.jsonl`)
    pub fn new(path: PathBuf) -> Self {
        Self {
            path: Mutex::new(path),
        }
    }

    /// 记录一次会话活动(SessionCreated / TurnEnd 时调用)
    ///
    /// 幂等追加一行;同 session_id 多行在读时合并。
    /// 失败仅 warn 不阻断会话流程(fail-soft,索引是展示辅助非真相源)。
    pub fn record(&self, entry: &SessionIndexEntry) {
        let Ok(path) = self.path.lock() else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(error = %e, "session index: create_dir_all failed");
                return;
            }
        }
        let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&*path)
        else {
            tracing::warn!("session index: open for append failed");
            return;
        };
        let Ok(mut line) = serde_json::to_string(entry) else {
            return;
        };
        line.push('\n');
        if let Err(e) = f.write_all(line.as_bytes()) {
            tracing::warn!(error = %e, "session index: append failed");
        }
    }

    /// 读取去重合并后的会话列表(按 last_active 降序)
    pub fn list(&self) -> Vec<SessionIndexEntry> {
        let Ok(path) = self.path.lock() else {
            return Vec::new();
        };
        let Ok(content) = std::fs::read_to_string(&*path) else {
            return Vec::new();
        };
        // 同 session_id 合并:created_at 最小 / last_active 最大 / title 首个非空
        let mut merged: std::collections::HashMap<String, SessionIndexEntry> =
            std::collections::HashMap::new();
        for line in content.lines() {
            let Ok(e) = serde_json::from_str::<SessionIndexEntry>(line) else {
                continue; // 跳过损坏行(部分写等)
            };
            match merged.get_mut(&e.session_id) {
                Some(prev) => {
                    prev.created_at = prev.created_at.min(e.created_at);
                    prev.last_active = prev.last_active.max(e.last_active);
                    if prev.title.is_empty() && !e.title.is_empty() {
                        prev.title = e.title;
                    }
                }
                None => {
                    merged.insert(e.session_id.clone(), e);
                }
            }
        }
        let mut out: Vec<SessionIndexEntry> = merged.into_values().collect();
        out.sort_by(|a, b| {
            b.last_active
                .cmp(&a.last_active)
                .then(a.session_id.cmp(&b.session_id))
        });
        out
    }
}

/// 从 evorule facts 投影某会话的消息历史(唯一真相读取路径)
///
/// 读取 `__memory__.{ns}.session_{sid}.messages.` 前缀下全部 PayloadUpdate,
/// 同 idx 后写覆盖(last-write-wins),按 idx 升序返回。
pub async fn load_transcript(
    client: &EvoruleApiClient,
    session_id: &str,
    namespace: &str,
) -> Result<Vec<MessageRecord>, String> {
    let prefix = format!("__memory__.{namespace}.session_{session_id}.messages.");
    let facts = client
        .get_facts(session_id, Some(&prefix))
        .await
        .map_err(|e| format!("fetch facts failed: {e}"))?;

    // facts 顺序 = 版本顺序;同 idx 后写的覆盖先写的
    let mut by_idx: std::collections::BTreeMap<usize, MessageRecord> =
        std::collections::BTreeMap::new();
    for f in facts {
        // 记录体存在 value 字段(任意 JSON),反序列化为 MessageRecord
        let Ok(record) = serde_json::from_value::<MessageRecord>(f.value) else {
            continue; // 非消息记录或结构漂移,跳过
        };
        by_idx.insert(record.idx, record);
    }
    Ok(by_idx.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(sid: &str, created: u64, active: u64, title: &str) -> SessionIndexEntry {
        SessionIndexEntry {
            session_id: sid.to_string(),
            agent_type: "general".to_string(),
            created_at: created,
            last_active: active,
            title: title.to_string(),
        }
    }

    #[test]
    fn test_record_and_list_dedupe() {
        let dir = tempfile::tempdir().unwrap();
        let idx = SessionIndex::new(dir.path().join("data/session_index.jsonl"));
        // 同会话三行:创建 → 活跃更新 → 活跃更新
        idx.record(&entry("s1", 100, 100, "第一条消息标题"));
        idx.record(&entry("s1", 999, 200, ""));
        idx.record(&entry("s1", 50, 300, ""));
        // 另一会话且更早活跃
        idx.record(&entry("s2", 10, 90, "第二条"));

        let list = idx.list();
        assert_eq!(list.len(), 2);
        // last_active 降序:s1(300) 在前
        assert_eq!(list[0].session_id, "s1");
        assert_eq!(list[0].created_at, 50, "created_at 取最小");
        assert_eq!(list[0].last_active, 300);
        assert_eq!(list[0].title, "第一条消息标题", "title 取首个非空");
        assert_eq!(list[1].session_id, "s2");
    }

    #[test]
    fn test_list_skips_corrupt_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idx.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{broken json\n{}\n",
                serde_json::to_string(&entry("s1", 1, 2, "t")).unwrap()
            ),
        )
        .unwrap();
        let idx = SessionIndex::new(path);
        let list = idx.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_id, "s1");
    }

    #[test]
    fn test_list_empty_returns_vec() {
        let dir = tempfile::tempdir().unwrap();
        let idx = SessionIndex::new(dir.path().join("nonexistent.jsonl"));
        assert!(idx.list().is_empty());
    }
}
