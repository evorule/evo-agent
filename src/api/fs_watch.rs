// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 工作台文件树实时刷新 —— workdir 文件系统监听与 WS 广播
//!
//! serve 启动时对 workdir 递归 watch(notify RecommendedWatcher,
//! Windows = ReadDirectoryChangesW),经 400ms 去抖合并后规整为批量事件
//! [`FsEventBatch`](相对 workdir 路径),经 [`FsEventHub`](tokio broadcast)
//! 向所有 WS 连接广播,帧形如:
//!
//! ```json
//! {"type":"fs_events","events":{"added":["a.txt"],"updated":[],"removed":[],"moved":[]}}
//! ```
//!
//! ## 规整层职责(设计定稿)
//!
//! - **rename 配对**:notify-debouncer-full 已把 From/To 配对为单事件
//!   (`Modify(Name(Both))`,`paths = [from, to]`),直接转 `moved` 条目;
//! - **自写噪声**:tmp+rename 原子写/覆盖写在同一去抖批内表现为
//!   「同路径 Removed + Upsert 对」→ 规整为一条 `updated`(设计 §3.2 裁定);
//! - **排除规则**:首段 `.` 开头组件(`.git`/`.evo`/`.evo-trash`/`.env`…)
//!   与 `target`/`node_modules`/`data` 三个目录不上报——口径与文件树
//!   listDir 的默认跳过规则一致(00-立项方案 §一);
//! - **不补发**:连接级广播,断连期间事件丢失(设计取舍,前端手动刷新兜底),
//!   落后过多(Lagged)的订阅者直接跳过。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, DebouncedEvent};
use serde::Serialize;
use std::sync::Arc;

/// 去抖窗口(设计定稿 400ms;实测覆盖编辑器保存/脚本批量写场景)
pub const FS_DEBOUNCE_MS: u64 = 400;

/// 广播通道容量(批量事件条数;Lagged 跳过不补发,容量仅平滑突发)
const FS_EVENT_CHANNEL_CAP: usize = 128;

// =============================================================================
// 事件模型
// =============================================================================

/// 重命名条目(from → to,相对 workdir 的 `/` 分隔路径)
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct MovedPath {
    /// 原路径
    pub from: String,
    /// 新路径
    pub to: String,
}

/// 规整后的批量事件(单帧负载;空数组也序列化,前端消费面统一)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FsEventBatch {
    /// 新增路径
    pub added: Vec<String>,
    /// 内容变更路径(含原子写重写)
    pub updated: Vec<String>,
    /// 移除路径
    pub removed: Vec<String>,
    /// 重命名条目
    pub moved: Vec<MovedPath>,
}

impl FsEventBatch {
    /// 空批(不发布)
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.updated.is_empty()
            && self.removed.is_empty()
            && self.moved.is_empty()
    }
}

/// 原始事件(规整层输入;notify 事件的最小投影,便于纯函数单测)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawEvent {
    /// 创建或内容修改(notify Create/Modify 合并;新增 vs 修改由规整层
    /// 结合同批 Removed 判定)
    Upsert(PathBuf),
    /// 移除
    Removed(PathBuf),
    /// 重命名对(debouncer 已配对)
    Renamed {
        /// 原路径
        from: PathBuf,
        /// 新路径
        to: PathBuf,
    },
}

// =============================================================================
// 规整层(纯函数)
// =============================================================================

/// 排除目录(首段精确匹配;与文件树 listDir 默认跳过口径一致)
const EXCLUDED_FIRST_SEGMENTS: &[&str] = &["target", "node_modules", "data"];

/// 排除判定:首段 `.` 开头组件 或 `target`/`node_modules`/`data`
fn excluded(rel: &str) -> bool {
    let first = rel.split('/').next().unwrap_or(rel);
    first.starts_with('.') || EXCLUDED_FIRST_SEGMENTS.contains(&first)
}

/// notify 事件 → 原始事件(适配层;噪声语义在此归一)
fn to_raw_events(ev: &DebouncedEvent) -> Vec<RawEvent> {
    match &ev.kind {
        // debouncer 配对成功的重命名单事件(paths = [from, to])
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if ev.paths.len() >= 2 => {
            vec![RawEvent::Renamed {
                from: ev.paths[0].clone(),
                to: ev.paths[1].clone(),
            }]
        }
        // 未配对的 rename 起点 = 文件移出(被移走/被原子写删除)
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => ev
            .paths
            .first()
            .map(|p| vec![RawEvent::Removed(p.clone())])
            .unwrap_or_default(),
        EventKind::Remove(_) => ev
            .paths
            .first()
            .map(|p| vec![RawEvent::Removed(p.clone())])
            .unwrap_or_default(),
        // 访问类事件与元事件不反映树变更,忽略
        EventKind::Access(_) | EventKind::Other => Vec::new(),
        // 其余(Create/Modify(Data,Metadata)/未配对 To/Any)= upsert
        _ => ev
            .paths
            .first()
            .map(|p| vec![RawEvent::Upsert(p.clone())])
            .unwrap_or_default(),
    }
}

/// 把一批原始事件规整为批量事件(纯函数,单测正本)。
///
/// 路径越出 workdir(strip 失败)或落在排除区的事件被丢弃;rename 的
/// from/to 两侧分别判级(移入可见区 = added,移出 = removed)。
pub fn normalize(workdir: &Path, raws: &[RawEvent]) -> FsEventBatch {
    let mut added = BTreeSet::new();
    let mut updated = BTreeSet::new();
    let mut removed = BTreeSet::new();
    let mut moved_pairs = BTreeSet::new();

    // 相对路径化:越出 workdir 丢弃;Windows 反斜杠统一为 `/`
    let rel = |p: &Path| -> Option<String> {
        let r = p.strip_prefix(workdir).ok()?;
        if r.as_os_str().is_empty() {
            return None;
        }
        Some(r.to_string_lossy().replace('\\', "/"))
    };

    for raw in raws {
        match raw {
            RawEvent::Upsert(p) => {
                if let Some(r) = rel(p).filter(|r| !excluded(r)) {
                    added.insert(r);
                }
            }
            RawEvent::Removed(p) => {
                if let Some(r) = rel(p).filter(|r| !excluded(r)) {
                    removed.insert(r);
                }
            }
            RawEvent::Renamed { from, to } => {
                let rf = rel(from);
                let rt = rel(to);
                match (rf, rt) {
                    (Some(f), Some(t)) => {
                        let (ef, et) = (excluded(&f), excluded(&t));
                        match (ef, et) {
                            (false, false) => {
                                moved_pairs.insert((f, t));
                            }
                            // 从排除区移入可见区 = 新增;移出可见区 = 移除
                            (true, false) => {
                                added.insert(t);
                            }
                            (false, true) => {
                                removed.insert(f);
                            }
                            (true, true) => {}
                        }
                    }
                    // 一侧越出 workdir:按可见侧语义判级
                    (None, Some(t)) => {
                        if !excluded(&t) {
                            added.insert(t);
                        }
                    }
                    (Some(f), None) => {
                        if !excluded(&f) {
                            removed.insert(f);
                        }
                    }
                    (None, None) => {}
                }
            }
        }
    }

    // 自写噪声:同批同路径 Removed + Upsert → updated(原子写重写)
    for p in added.intersection(&removed).cloned().collect::<Vec<_>>() {
        added.remove(&p);
        removed.remove(&p);
        updated.insert(p);
    }
    // rename 落点优先:to 已入 moved 时,从 added/removed 剔除重复条目
    for (_, t) in moved_pairs.iter() {
        added.remove(t);
        removed.remove(t);
    }

    FsEventBatch {
        added: added.into_iter().collect(),
        updated: updated.into_iter().collect(),
        removed: removed.into_iter().collect(),
        moved: moved_pairs
            .into_iter()
            .map(|(from, to)| MovedPath { from, to })
            .collect(),
    }
}

// =============================================================================
// 广播 hub
// =============================================================================

/// 文件系统事件广播 hub(serve 单例;连接级订阅,Lagged 跳过不补发)
#[derive(Debug, Clone)]
pub struct FsEventHub {
    tx: tokio::sync::broadcast::Sender<Arc<FsEventBatch>>,
}

impl FsEventHub {
    /// 新 hub(空订阅集)
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(FS_EVENT_CHANNEL_CAP);
        Self { tx }
    }

    /// 订阅(每个 WS 连接建立时调用一次)
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Arc<FsEventBatch>> {
        self.tx.subscribe()
    }

    /// 发布批量事件(空批不发布;无订阅者时静默丢弃)
    pub fn publish(&self, batch: FsEventBatch) {
        if batch.is_empty() {
            return;
        }
        let _ = self.tx.send(Arc::new(batch));
    }
}

impl Default for FsEventHub {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// watcher 启动
// =============================================================================

/// 启动 workdir 递归监听(serve 启动时调用一次)。
///
/// 事件经 400ms 去抖 + [`normalize`] 规整后经 `hub` 广播。debouncer 以
/// `std::mem::forget` 主动泄漏——serve 进程生命周期即 watcher 生命周期,
/// 其内部线程持续运行;失败返回 Err 时调用方 fail-soft 降级(仅告警)。
pub fn spawn_watcher(workdir: &Path, hub: FsEventHub) -> Result<(), String> {
    let wd = workdir.to_path_buf();
    let mut debouncer = new_debouncer(
        Duration::from_millis(FS_DEBOUNCE_MS),
        None,
        move |result: DebounceEventResult| {
            let events = match result {
                Ok(events) => events,
                Err(errors) => {
                    // watcher 自身错误(权限/瞬时不可达):warn 留痕,跳过该批
                    tracing::warn!(
                        count = errors.len(),
                        "fs_watch: watcher errors (batch skipped)"
                    );
                    return;
                }
            };
            let raw: Vec<RawEvent> = events.iter().flat_map(to_raw_events).collect();
            let batch = normalize(&wd, &raw);
            hub.publish(batch);
        },
    )
    .map_err(|e| format!("failed to create file watcher: {e}"))?;
    debouncer
        .watch(workdir, RecursiveMode::Recursive)
        .map_err(|e| format!("failed to watch {}: {e}", workdir.display()))?;
    std::mem::forget(debouncer);
    Ok(())
}

// =============================================================================
// 单元测试
// =============================================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::time::Instant;

    fn wd() -> PathBuf {
        PathBuf::from("/work")
    }

    fn norm(raws: &[RawEvent]) -> FsEventBatch {
        normalize(&wd(), raws)
    }

    // ===== notify 事件适配层 =====

    fn debounced(kind: EventKind, paths: Vec<PathBuf>) -> DebouncedEvent {
        let mut event = notify::Event::new(kind);
        event.paths = paths;
        DebouncedEvent::new(event, Instant::now())
    }

    #[test]
    fn test_adapter_rename_both_pair() {
        let ev = debounced(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            vec![wd().join("a.txt"), wd().join("b.txt")],
        );
        let raw = to_raw_events(&ev);
        assert_eq!(
            raw,
            vec![RawEvent::Renamed {
                from: wd().join("a.txt"),
                to: wd().join("b.txt"),
            }]
        );
    }

    #[test]
    fn test_adapter_remove_and_create() {
        let rm = debounced(
            EventKind::Remove(notify::event::RemoveKind::Any),
            vec![wd().join("gone.txt")],
        );
        let cr = debounced(
            EventKind::Create(notify::event::CreateKind::File),
            vec![wd().join("new.txt")],
        );
        let raw: Vec<RawEvent> = [&rm, &cr].iter().flat_map(|e| to_raw_events(e)).collect();
        assert_eq!(
            raw,
            vec![
                RawEvent::Removed(wd().join("gone.txt")),
                RawEvent::Upsert(wd().join("new.txt")),
            ]
        );
    }

    #[test]
    fn test_adapter_unmatched_rename_from_is_removed() {
        let ev = debounced(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            vec![wd().join("old.txt")],
        );
        let raw = to_raw_events(&ev);
        assert_eq!(raw, vec![RawEvent::Removed(wd().join("old.txt"))]);
    }

    // ===== 规整层 =====

    #[test]
    fn test_normalize_rename_pair_to_moved() {
        let batch = norm(&[RawEvent::Renamed {
            from: wd().join("a.txt"),
            to: wd().join("sub/b.txt"),
        }]);
        assert!(batch.added.is_empty() && batch.updated.is_empty() && batch.removed.is_empty());
        assert_eq!(batch.moved.len(), 1);
        assert_eq!(batch.moved[0].from, "a.txt");
        assert_eq!(batch.moved[0].to, "sub/b.txt");
    }

    #[test]
    fn test_normalize_atomic_write_noise_to_updated() {
        // tmp+rename 原子写覆盖已有文件:同批「同路径 Removed + Upsert」→ updated
        let batch = norm(&[
            RawEvent::Removed(wd().join("src/lib.rs")),
            RawEvent::Upsert(wd().join("src/lib.rs")),
        ]);
        assert!(batch.added.is_empty() && batch.removed.is_empty() && batch.moved.is_empty());
        assert_eq!(batch.updated, vec!["src/lib.rs"]);
    }

    #[test]
    fn test_normalize_excluded_dirs_dropped() {
        let batch = norm(&[
            RawEvent::Upsert(wd().join("target/debug/x.bin")),
            RawEvent::Upsert(wd().join(".git/index")),
            RawEvent::Upsert(wd().join(".evo/settings.json")),
            RawEvent::Upsert(wd().join("node_modules/y.js")),
            RawEvent::Upsert(wd().join("data/session_index.jsonl")),
            RawEvent::Upsert(wd().join(".evo-trash/1758864000_gone.txt")),
        ]);
        assert!(batch.is_empty(), "all excluded events must be dropped");
    }

    #[test]
    fn test_normalize_root_file_and_visible_dirs_pass() {
        let batch = norm(&[
            RawEvent::Upsert(wd().join("main.rs")),
            RawEvent::Upsert(wd().join("src/nested/deep.txt")),
        ]);
        assert_eq!(batch.added, vec!["main.rs", "src/nested/deep.txt"]);
    }

    #[test]
    fn test_normalize_create_and_remove_stay_separate() {
        let batch = norm(&[
            RawEvent::Upsert(wd().join("new.txt")),
            RawEvent::Removed(wd().join("old.txt")),
        ]);
        assert_eq!(batch.added, vec!["new.txt"]);
        assert_eq!(batch.removed, vec!["old.txt"]);
        assert!(batch.updated.is_empty());
    }

    #[test]
    fn test_normalize_rename_in_and_out_of_excluded_zone() {
        // .git 内移出可见区:from 排除 → to 视为新增
        let batch = norm(&[RawEvent::Renamed {
            from: wd().join(".git/HEAD"),
            to: wd().join("HEAD.txt"),
        }]);
        assert_eq!(batch.added, vec!["HEAD.txt"]);
        // 可见区移入 target:from 视为移除
        let batch = norm(&[RawEvent::Renamed {
            from: wd().join("keep.txt"),
            to: wd().join("target/x.txt"),
        }]);
        assert_eq!(batch.removed, vec!["keep.txt"]);
    }

    #[test]
    fn test_normalize_rename_takes_precedence_over_upsert() {
        // rename 落点同批又被修改:moved 优先,added 不重复
        let batch = norm(&[
            RawEvent::Renamed {
                from: wd().join("a.txt"),
                to: wd().join("b.txt"),
            },
            RawEvent::Upsert(wd().join("b.txt")),
        ]);
        assert!(batch.added.is_empty());
        assert_eq!(batch.moved.len(), 1);
        assert_eq!(batch.moved[0].to, "b.txt");
    }

    #[test]
    fn test_normalize_out_of_workdir_dropped() {
        let batch = norm(&[
            RawEvent::Upsert(PathBuf::from("/elsewhere/x.txt")),
            RawEvent::Upsert(wd().clone()), // workdir 根自身:空相对路径,丢弃
        ]);
        assert!(batch.is_empty());
    }

    // ===== hub =====

    #[tokio::test]
    async fn test_hub_publish_subscribe_roundtrip() {
        let hub = FsEventHub::new();
        let mut rx = hub.subscribe();
        let batch = FsEventBatch {
            added: vec!["a.txt".to_string()],
            ..Default::default()
        };
        hub.publish(batch);
        let received = rx.recv().await.unwrap();
        assert_eq!(received.added, vec!["a.txt".to_string()]);
    }

    #[tokio::test]
    async fn test_hub_empty_batch_not_published() {
        let hub = FsEventHub::new();
        let mut rx = hub.subscribe();
        hub.publish(FsEventBatch::default());
        assert!(
            rx.try_recv().is_err(),
            "empty batch must not reach subscribers"
        );
    }

    #[test]
    fn test_hub_no_subscriber_is_silent() {
        let hub = FsEventHub::new();
        hub.publish(FsEventBatch {
            added: vec!["x".to_string()],
            ..Default::default()
        });
    }
}
