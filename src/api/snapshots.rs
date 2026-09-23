// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 会话消息本地快照与保留期管理(O-093)—— 展示层持久化
//!
//! ## 三层边界(设计留痕,项目方批准的方案红线)
//!
//! - **审计链 FactsLog**:append-only + BLAKE3,永不删,权威 —— 本模块零关联,
//!   不写入、不读取任何审计面;
//! - **回放/时光机器**:引擎自审计链重建,零依赖本快照 —— 不受影响;
//! - **本快照**:展示层**非权威副本**(文件内嵌 `authoritative:false`),
//!   服务「evorule 会话 30min 闲置 TTL 回收后历史仍可见」(O-093),
//!   按保留期配置可删除;**删除只作用于 `data/snapshots/` 目录,永不越界**。
//!
//! ## 日期分片与清理口径
//!
//! - 目录 = `data/snapshots/YYYY/MM/DD/session_{sid}.json`,日期取**保存时刻
//!   的 UTC 日**(纯整数算法无时区依赖;分片是磁盘管理粒度,不承诺本地日界);
//! - 同会话同 UTC 日重复保存 = 原子覆盖(单文件);跨日产生新文件,读取取
//!   `saved_at` 最新一份;
//! - 清理按**目录日期**整体删除(同日快照同批过期):`当前 UTC 日 - 目录日
//!   >= 保留天数` 即删;`forever` 永不删。保留期每次清理时从
//!   `workbench_config.json` 现读,改配置在下次清理时生效。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::memory::MessageRecord;
use crate::api::evorule_client::EvoruleApiClient;
use crate::api::session_index::{load_transcript, unix_now};

// =============================================================================
// 保留期(与 UI 下拉共用标签)
// =============================================================================

/// 快照保留期(UI 下拉与 workbench_config.json 共用标签口径)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionPolicy {
    /// 1 天
    Day1,
    /// 1 个月(按 30 天计)
    Month1,
    /// 3 个月(按 90 天计,缺省)
    Month3,
    /// 半年(按 180 天计)
    Month6,
    /// 1 年(按 365 天计)
    Year1,
    /// 长期保留(永不清理)
    Forever,
}

impl RetentionPolicy {
    /// 全部合法标签与展示文案(顺序即 UI 下拉顺序)
    pub const ALL: [(&'static str, &'static str); 6] = [
        ("1d", "1 天"),
        ("1m", "1 个月"),
        ("3m", "3 个月"),
        ("6m", "半年"),
        ("1y", "1 年"),
        ("forever", "长期保留"),
    ];

    /// 标签 → 策略(非法标签返回 None)
    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "1d" => Some(Self::Day1),
            "1m" => Some(Self::Month1),
            "3m" => Some(Self::Month3),
            "6m" => Some(Self::Month6),
            "1y" => Some(Self::Year1),
            "forever" => Some(Self::Forever),
            _ => None,
        }
    }

    /// 策略 → 标签
    pub fn label(self) -> &'static str {
        match self {
            Self::Day1 => "1d",
            Self::Month1 => "1m",
            Self::Month3 => "3m",
            Self::Month6 => "6m",
            Self::Year1 => "1y",
            Self::Forever => "forever",
        }
    }

    /// 保留天数;`None` = 永久
    pub fn days(self) -> Option<u64> {
        match self {
            Self::Day1 => Some(1),
            Self::Month1 => Some(30),
            Self::Month3 => Some(90),
            Self::Month6 => Some(180),
            Self::Year1 => Some(365),
            Self::Forever => None,
        }
    }
}

// =============================================================================
// 工作台配置(data/workbench_config.json,原子写)
// =============================================================================

/// 工作台配置(serve 侧持久化,PUT /api/workbench/config 写入)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkbenchConfig {
    /// 快照保留期标签(1d|1m|3m|6m|1y|forever)
    pub retention: String,
}

impl Default for WorkbenchConfig {
    fn default() -> Self {
        // 缺省 3 个月(项目方批准的 O-093 方案默认值)
        Self {
            retention: "3m".to_string(),
        }
    }
}

/// 工作台配置存储(`data/workbench_config.json`;原子写)
#[derive(Debug)]
pub struct WorkbenchConfigStore {
    path: PathBuf,
}

impl WorkbenchConfigStore {
    /// 构造配置存储
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// 读取配置:缺文件 / 损坏 / 字段非法 → 缺省(3 个月)
    pub fn load(&self) -> WorkbenchConfig {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return WorkbenchConfig::default();
        };
        match serde_json::from_str::<WorkbenchConfig>(&text) {
            Ok(c) if RetentionPolicy::from_label(&c.retention).is_some() => c,
            _ => WorkbenchConfig::default(),
        }
    }

    /// 原子保存(tmp + rename);标签非法返回 Err 不落盘
    pub fn save(&self, cfg: &WorkbenchConfig) -> Result<(), String> {
        if RetentionPolicy::from_label(&cfg.retention).is_none() {
            return Err(format!("invalid retention label: {}", cfg.retention));
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create config dir failed: {e}"))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        {
            let text = serde_json::to_string_pretty(cfg)
                .map_err(|e| format!("serialize config failed: {e}"))?;
            let mut f = std::fs::File::create(&tmp).map_err(|e| format!("write tmp failed: {e}"))?;
            use std::io::Write as _;
            f.write_all(text.as_bytes())
                .map_err(|e| format!("write tmp failed: {e}"))?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("rename failed: {e}"))?;
        Ok(())
    }
}

// =============================================================================
// 快照存储(data/snapshots/YYYY/MM/DD/session_{sid}.json)
// =============================================================================

/// 快照文件内容(与 transcript 投影同源同构 + 元数据)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotFile {
    /// 恒 `"snapshot"`(与 live 投影区分)
    pub kind: String,
    /// 恒 `false` —— 展示层副本,真相源是 evorule(payload/facts)
    pub authoritative: bool,
    /// evorule 会话 ID
    pub session_id: String,
    /// agent 类型
    pub agent_type: String,
    /// 保存时刻(Unix 秒)
    pub saved_at: u64,
    /// 消息条数
    pub count: usize,
    /// 消息列表(与 transcript 端点同构的 MessageRecord)
    pub messages: Vec<MessageRecord>,
}

/// 快照存储(目录 = `<workdir>/data/snapshots`,按 YYYY/MM/DD 日期分片)
#[derive(Debug)]
pub struct SnapshotStore {
    root: PathBuf,
}

impl SnapshotStore {
    /// 构造快照存储
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// 确保根目录存在(serve 启动时调用)
    pub fn ensure_dir(&self) {
        let _ = std::fs::create_dir_all(&self.root);
    }

    /// 会话 ID 安全校验:只允许字母数字与 `-_`(load/save 的文件名拼接口径;
    /// load 面向用户输入,防 `..`/分隔符路径穿越)
    fn safe_session_id(session_id: &str) -> bool {
        !session_id.is_empty()
            && session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }

    fn day_dir(&self, y: i64, m: u32, d: u32) -> PathBuf {
        self.root
            .join(format!("{y:04}"))
            .join(format!("{m:02}"))
            .join(format!("{d:02}"))
    }

    /// 保存快照(同会话同 UTC 日原子覆盖;失败返回 Err,fail-soft 由调用方兜底)
    pub fn save(
        &self,
        session_id: &str,
        agent_type: &str,
        messages: &[MessageRecord],
    ) -> Result<PathBuf, String> {
        if !Self::safe_session_id(session_id) {
            return Err(format!("unsafe session id for snapshot: {session_id}"));
        }
        let (y, m, d) = civil_from_days((unix_now() / 86_400) as i64);
        let dir = self.day_dir(y, m, d);
        std::fs::create_dir_all(&dir).map_err(|e| format!("create snapshot dir failed: {e}"))?;
        let name = format!("session_{session_id}.json");
        let snap = SnapshotFile {
            kind: "snapshot".to_string(),
            authoritative: false,
            session_id: session_id.to_string(),
            agent_type: agent_type.to_string(),
            saved_at: unix_now(),
            count: messages.len(),
            messages: messages.to_vec(),
        };
        let text = serde_json::to_string(&snap).map_err(|e| format!("serialize failed: {e}"))?;
        // 原子写:tmp + rename(Windows rename 覆盖已存在文件)
        let tmp = dir.join(format!(".{name}.tmp"));
        {
            let mut f =
                std::fs::File::create(&tmp).map_err(|e| format!("write tmp failed: {e}"))?;
            use std::io::Write as _;
            f.write_all(text.as_bytes())
                .map_err(|e| format!("write tmp failed: {e}"))?;
        }
        let target = dir.join(&name);
        std::fs::rename(&tmp, &target).map_err(|e| format!("rename failed: {e}"))?;
        Ok(target)
    }

    /// 读取某会话最新快照(遍历全部日期分片,取 `saved_at` 最大);
    /// 无快照 / 会话 ID 非法 / 反序列化失败 → `None`
    pub fn load(&self, session_id: &str) -> Option<SnapshotFile> {
        if !Self::safe_session_id(session_id) {
            return None;
        }
        let name = format!("session_{session_id}.json");
        let mut best: Option<SnapshotFile> = None;
        for y in numeric_subdirs(&self.root) {
            for m in numeric_subdirs(&y.path()) {
                for d in numeric_subdirs(&m.path()) {
                    let path = d.path().join(&name);
                    let Ok(text) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    let Ok(snap) = serde_json::from_str::<SnapshotFile>(&text) else {
                        continue; // 损坏文件跳过
                    };
                    if best
                        .as_ref()
                        .map_or(true, |b| snap.saved_at > b.saved_at)
                    {
                        best = Some(snap);
                    }
                }
            }
        }
        best
    }

    /// 按保留期清理过期日期分片(整目录删除;返回删除的日期分片数)
    ///
    /// 判据:`当前 UTC 日 - 目录日 >= 保留天数`;`Forever` 直接返回 0。
    /// 非 4 位年 / 2 位月日 / 非数字目录名一律跳过(保守不删)。
    pub fn cleanup_expired(&self, retention: RetentionPolicy) -> usize {
        let Some(max_age) = retention.days() else {
            return 0;
        };
        let today = (unix_now() / 86_400) as i64;
        let mut removed = 0;
        for y in numeric_subdirs(&self.root) {
            let Ok(yv) = y.file_name().to_str().unwrap_or_default().parse::<i64>() else {
                continue;
            };
            for m in numeric_subdirs(&y.path()) {
                let Ok(mv) = m.file_name().to_str().unwrap_or_default().parse::<u32>() else {
                    continue;
                };
                for d in numeric_subdirs(&m.path()) {
                    let Ok(dv) = d.file_name().to_str().unwrap_or_default().parse::<u32>() else {
                        continue;
                    };
                    let dir_days = days_from_civil(yv, mv, dv);
                    if today - dir_days >= max_age as i64 {
                        if std::fs::remove_dir_all(&d.path()).is_ok() {
                            removed += 1;
                            // 空月目录顺手移除(非空则失败忽略)
                            let _ = std::fs::remove_dir(&m.path());
                        }
                    }
                }
                // 整月删空后移除空月目录
                if is_dir_empty(&m.path()) {
                    let _ = std::fs::remove_dir(&m.path());
                }
            }
            // 整年删空后移除空年目录
            if is_dir_empty(&y.path()) {
                let _ = std::fs::remove_dir(&y.path());
            }
        }
        removed
    }
}

// =============================================================================
// TurnEnd 快照采集(与 transcript 端点同一投影路径)
// =============================================================================

/// TurnEnd 快照采集:复用 `load_transcript`(与 `GET .../transcript` 同一
/// 读取路径,保证快照与活投影同源同构),落本地快照。
///
/// fail-soft 由调用方兜底:快照是展示辅助,失败只 warn 不阻断会话流程。
pub async fn capture_from_engine(
    client: &EvoruleApiClient,
    store: &SnapshotStore,
    session_id: &str,
    agent_type: &str,
    namespace: &str,
) -> Result<PathBuf, String> {
    let messages = load_transcript(client, session_id, namespace).await?;
    store.save(session_id, agent_type, &messages)
}

/// evorule 会话是否已不可达(30min 闲置 TTL 回收 → facts 查询 404 →
/// `ApiError::SessionNotFound` → Display `"Session not found"`)
pub fn is_session_gone(err: &str) -> bool {
    err.to_ascii_lowercase().contains("session not found")
}

// =============================================================================
// 日期纯整数算法(Howard Hinnant,公历,无时区/无浮点)
// =============================================================================

/// Unix 日数 → (年, 月, 日)(civil_from_days)
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (年, 月, 日) → Unix 日数(days_from_civil)
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as u64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

// =============================================================================
// 目录遍历辅助
// =============================================================================

/// 列出目录下的全部子目录(读失败 → 空)
fn subdirs(dir: &Path) -> Vec<std::fs::DirEntry> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect()
}

/// 列出目录下名字为纯数字的子目录(分片目录名;跳过一切杂项)
fn numeric_subdirs(dir: &Path) -> Vec<std::fs::DirEntry> {
    subdirs(dir)
        .into_iter()
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                .unwrap_or(false)
        })
        .collect()
}

/// 目录是否为空(读失败按非空处理,保守不删)
fn is_dir_empty(dir: &Path) -> bool {
    let Ok(mut rd) = std::fs::read_dir(dir) else {
        return false;
    };
    rd.next().is_none()
}

// =============================================================================
// 单测
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(idx: usize, role: &str, content: &str) -> MessageRecord {
        serde_json::from_value(json!({
            "idx": idx, "role": role, "content": content, "timestamp": 1
        }))
        .unwrap()
    }

    #[test]
    fn test_civil_roundtrip() {
        // 已知锚点:1970-01-01 = day 0;2026-09-23 = 20719
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2026, 9, 23), 20_719);
        assert_eq!(civil_from_days(20_719), (2026, 9, 23));
        // 随机往返:2020-2100 每 11 天一抽
        let start = days_from_civil(2020, 1, 1);
        let end = days_from_civil(2100, 1, 1);
        let mut z = start;
        while z <= end {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z, "roundtrip at day {z}");
            z += 11;
        }
    }

    #[test]
    fn test_retention_policy() {
        assert_eq!(RetentionPolicy::from_label("3m"), Some(RetentionPolicy::Month3));
        assert_eq!(RetentionPolicy::from_label("nope"), None);
        assert_eq!(WorkbenchConfig::default().retention, "3m");
        assert_eq!(RetentionPolicy::Forever.days(), None);
        assert_eq!(RetentionPolicy::Month3.days(), Some(90));
    }

    #[test]
    fn test_is_session_gone() {
        assert!(is_session_gone("fetch facts failed: Session not found"));
        assert!(is_session_gone("SESSION NOT FOUND"));
        assert!(!is_session_gone("fetch facts failed: connection refused"));
    }

    #[test]
    fn test_save_load_roundtrip_and_latest_wins() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        store.ensure_dir();
        let msgs = vec![record(0, "user", "你好"), record(1, "assistant", "您好")];
        store.save("s1", "general", &msgs).unwrap();
        // 同会话再存(更新版,内容不同)
        let msgs2 = vec![record(0, "user", "你好"), record(1, "assistant", "改版")];
        store.save("s1", "general", &msgs2).unwrap();

        let snap = store.load("s1").expect("snapshot should exist");
        assert_eq!(snap.kind, "snapshot");
        assert!(!snap.authoritative);
        assert_eq!(snap.session_id, "s1");
        assert_eq!(snap.agent_type, "general");
        assert_eq!(snap.count, 2);
        // saved_at 秒级相同 → 任取一份均合法;消息数一致
        assert_eq!(snap.messages.len(), 2);
        // 同 UTC 日只落一个文件(原子覆盖)
        let today = civil_from_days((unix_now() / 86_400) as i64);
        let day_dir = store.day_dir(today.0, today.1, today.2);
        assert_eq!(std::fs::read_dir(&day_dir).unwrap().count(), 1);
    }

    #[test]
    fn test_load_missing_and_unsafe_sid() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        assert!(store.load("nope").is_none());
        // 路径穿越 / 分隔符 / 空串一律 None
        assert!(store.load("../etc").is_none());
        assert!(store.load("a/b").is_none());
        assert!(store.load("").is_none());
        // save 同口径拒绝
        assert!(store.save("../evil", "general", &[]).is_err());
    }

    #[test]
    fn test_cleanup_by_day_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        store.ensure_dir();
        let today = (unix_now() / 86_400) as i64;
        // 手工造三个日期分片:今天 / 昨天 / 100 天前
        for &(offset, sid) in &[(0i64, "s_now"), (-1, "s_yesterday"), (-100, "s_old")] {
            let (y, m, d) = civil_from_days(today + offset);
            let dirp = store.day_dir(y, m, d);
            std::fs::create_dir_all(&dirp).unwrap();
            std::fs::write(
                dirp.join(format!("session_{sid}.json")),
                serde_json::to_string(&SnapshotFile {
                    kind: "snapshot".into(),
                    authoritative: false,
                    session_id: sid.into(),
                    agent_type: "general".into(),
                    saved_at: unix_now(),
                    count: 0,
                    messages: vec![],
                })
                .unwrap(),
            )
            .unwrap();
        }
        // 3 个月(90 天):今天/昨天保留,100 天前删除
        assert_eq!(store.cleanup_expired(RetentionPolicy::Month3), 1);
        assert!(store.load("s_now").is_some());
        assert!(store.load("s_yesterday").is_some());
        assert!(store.load("s_old").is_none());
        // forever:不删任何
        assert_eq!(store.cleanup_expired(RetentionPolicy::Forever), 0);
    }

    #[test]
    fn test_cleanup_skips_non_numeric_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("snapshots");
        // 非法分片名:保守跳过不删
        let weird = root.join("notayear").join("xx").join("yy");
        std::fs::create_dir_all(&weird).unwrap();
        std::fs::write(weird.join("session_x.json"), "{}").unwrap();
        let store = SnapshotStore::new(root);
        assert_eq!(store.cleanup_expired(RetentionPolicy::Day1), 0);
    }

    #[test]
    fn test_config_store_roundtrip_and_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkbenchConfigStore::new(dir.path().join("data/workbench_config.json"));
        // 缺文件 → 默认
        assert_eq!(store.load(), WorkbenchConfig::default());
        // 合法保存 + 读回
        store
            .save(&WorkbenchConfig {
                retention: "forever".into(),
            })
            .unwrap();
        assert_eq!(store.load().retention, "forever");
        // 非法标签:save 拒绝 / load 回默认
        assert!(store
            .save(&WorkbenchConfig {
                retention: "999".into()
            })
            .is_err());
        // 手工写坏文件 → load 回默认
        std::fs::write(&store.path, "not json").unwrap();
        assert_eq!(store.load(), WorkbenchConfig::default());
    }

    // capture_from_engine 依赖远端 evorule(网络):投影路径由
    // session_index::load_transcript 单测与 E2E 覆盖,此处不重复。
}
