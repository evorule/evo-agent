// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 工作区写面检查点仓库(写面撤销批)
//!
//! 每 workdir 一个 `.evo-checkpoint/` 栈目录,为写面工具(file_write /
//! edit_file / file_create)提供「写前快照 + 弹栈回退」能力:
//!
//! ```text
//! .evo-checkpoint/
//!   manifest.jsonl   # 栈序:{seq, ts, tool, path, kind, hash, bytes, journal_seq}
//!   blobs/<hex>      # 快照内容(blob 文件名取 evorule-hash 的 hex 段,
//!                    #  Windows 文件名禁 ':' —— manifest 内仍存完整口径)
//! ```
//!
//! ## 快照两段式(设计批 §5.2 的执行序落定)
//!
//! - **pre-capture**(⑦执行器运行前):读取目标既有状态——存在即 content
//!   快照(整文件字节入 blob,同内容去重),不存在即 tombstone(「此前不存
//!   在」的存在性原状)。必须在写前完成:覆盖场景写后再读,原内容已丢失。
//! - **post-commit**(⑦成功后):journal [`JournalEvent::FileCheckpointTaken`]
//!   入账 + manifest 栈条目落盘(双账以 (hash, journal_seq) 关联)。⑦失败
//!   则丢弃 pending,blob 留作孤儿由帽清理回收。
//!
//! ## 生命周期
//!
//! - 全局栈 + 会话无关(跨会话可恢复);manifest.jsonl 为盘上 SSOT,进程级
//!   per-root 锁互斥(多实例并发以盘面为准)。
//! - 栈深帽 32(对齐 REWIND_BUDGET_LIMIT 先例)、总字节帽 64MB,超限 LRU
//!   淘汰最旧条目(引用计数归零的 blob 一并回收)。
//! - 回退 = 弹栈单步:恢复路径复验 [`fs_safety::resolve_writable_target`]
//!   (防目录重绑后漂移越界)、blob hash 复算核对(防损坏/篡改)、写回后
//!   实测复算(不信快照自报)。任一步失败 fail-visible,栈不动。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde::{Deserialize, Serialize};

use crate::agent::journal::JournalWriter;

/// 栈深上限(对齐 journal::REWIND_BUDGET_LIMIT=32 语义先例)
pub const MAX_STACK_DEPTH: usize = 32;

/// 总字节帽(content 快照 bytes 合计;超限 LRU 淘汰最旧)
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// 快照类:content(既有内容快照)| tombstone(此前不存在)
pub const KIND_CONTENT: &str = "content";
/// 快照类:tombstone(此前不存在;恢复语义=删除该路径)
pub const KIND_TOMBSTONE: &str = "tombstone";

/// evorule-hash 口径 digest(与 journal::evorule_digest 同源;字节面变体)
fn evorule_digest_bytes(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

/// 进程级 per-root 全局锁注册表(照 journal ACTIVE_WRITERS 静态先例):
/// undo 工具与 runner 钩子各自持 CheckpointStore 实例(同 root),互斥靠本表
/// ——盘上 manifest.jsonl 是 SSOT,锁只保证进程内读写窗口不交叠。
fn root_locks() -> &'static Mutex<HashMap<PathBuf, &'static Mutex<()>>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, &'static Mutex<()>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_for(root: &Path) -> MutexGuard<'static, ()> {
    let map: &mut HashMap<_, _> = &mut root_locks().lock().unwrap_or_else(|p| p.into_inner());
    let lock: &'static Mutex<()> = map
        .entry(root.to_path_buf())
        .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))));
    lock.lock().unwrap_or_else(|p| p.into_inner())
}

/// manifest.jsonl 单行(栈条目;字段契约见模块文档)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ManifestEntry {
    /// 栈序(全生命周期单调递增,不因弹栈重置——popped_seq 引用语义)
    seq: u64,
    /// 入账时间戳(unix ms)
    ts: u64,
    /// 触发快照的写面工具名
    tool: String,
    /// 工作区相对路径
    path: String,
    /// 快照类(content | tombstone)
    kind: String,
    /// 快照内容 digest(evorule-hash 口径;tombstone=空内容固定值)
    hash: String,
    /// 快照字节数(tombstone=0)
    bytes: u64,
    /// 关联的 FileCheckpointTaken journal 行 seq(无 journal 面=None)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    journal_seq: Option<u64>,
}

/// pre-capture 产出的待入账快照(内存态;⑦成功后 commit,失败即丢弃)
#[derive(Debug, Clone)]
pub struct PendingSnapshot {
    entry: ManifestEntry,
}

impl PendingSnapshot {
    /// 快照的完整 digest(诊断/测试用)
    pub fn hash(&self) -> &str {
        &self.entry.hash
    }

    /// 快照类(content | tombstone)
    pub fn kind(&self) -> &str {
        &self.entry.kind
    }
}

/// undo 弹栈恢复结果(工具返回 JSON 与 journal 入账共用形态)
#[derive(Debug, Clone, Serialize)]
pub struct UndoOutcome {
    /// 工作区相对路径(content=恢复路径;tombstone=移除路径)
    pub path: String,
    /// 恢复类(content | tombstone)
    pub kind: String,
    /// 恢复内容 digest(写回后实测复算;tombstone=空内容固定值)
    pub restored_hash: String,
    /// 恢复字节数(tombstone=0)
    pub bytes: u64,
    /// 被消费的快照栈序
    pub popped_seq: u64,
    /// 弹栈后剩余栈深
    pub remaining_depth: usize,
}

/// 工作区写面检查点仓库
#[derive(Debug, Clone)]
pub struct CheckpointStore {
    /// 工作区根(canonical 形;快照相对路径的基准)
    workdir: PathBuf,
    /// 栈目录(workdir/.evo-checkpoint)
    root: PathBuf,
    /// 写域(相对 workdir,与写面工具实例同口径——agent 面恒 "workspace")
    writable_dir: PathBuf,
}

impl CheckpointStore {
    /// 构造仓库(workdir/writable_dir 口径与写面工具实例一致)
    pub fn new(workdir: &Path, writable_dir: &str) -> Self {
        let workdir = workdir
            .canonicalize()
            .unwrap_or_else(|_| workdir.to_path_buf());
        Self {
            root: workdir.join(".evo-checkpoint"),
            workdir,
            writable_dir: PathBuf::from(writable_dir),
        }
    }

    // ------------------------------------------------------------------
    // 快照钩子(pre-capture / commit / discard)
    // ------------------------------------------------------------------

    /// **写前快照**(⑦执行器运行前调用):读取目标既有状态,内容入 blob。
    /// 失败返回 Err(reason)——调用方 fail-visible 附注,不阻塞写本身。
    pub fn pre_capture(&self, raw_path: &str, tool: &str) -> Result<PendingSnapshot, String> {
        let _g = lock_for(&self.root);
        self.pre_capture_locked(raw_path, tool)
    }

    fn pre_capture_locked(&self, raw_path: &str, tool: &str) -> Result<PendingSnapshot, String> {
        let (target, _) = crate::builtin_tools::fs_safety::resolve_writable_target(
            &self.workdir,
            &self.writable_dir,
            raw_path,
        )?;
        let rel = target
            .strip_prefix(&self.workdir)
            .map_err(|_| format!("path '{raw_path}' escapes workdir"))?
            .to_string_lossy()
            .replace('\\', "/");

        // 目标不存在(且非悬空链接——resolve 已排除)→ tombstone(存在性原状)
        let meta = match std::fs::symlink_metadata(&target) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PendingSnapshot {
                    entry: ManifestEntry {
                        seq: 0, // commit 期定序
                        ts: 0,
                        tool: tool.to_string(),
                        path: rel,
                        kind: KIND_TOMBSTONE.to_string(),
                        hash: evorule_digest_bytes(&[]),
                        bytes: 0,
                        journal_seq: None,
                    },
                });
            }
            Err(e) => {
                return Err(format!("checkpoint stat failed for '{raw_path}': {e}"));
            }
        };
        if meta.is_dir() {
            // 目录上不存在「整文件内容」快照语义(写面工具对目录写入也必然
            // 失败);v1 明确不支持——fail-visible 附注,不阻塞
            return Err(format!("checkpoint skipped: '{raw_path}' is a directory"));
        }
        if meta.file_type().is_symlink() {
            return Err(format!("checkpoint skipped: '{raw_path}' is a symlink"));
        }

        let content = std::fs::read(&target)
            .map_err(|e| format!("checkpoint read failed for '{raw_path}': {e}"))?;
        let hash = evorule_digest_bytes(&content);
        self.write_blob(&hash, &content)?;

        Ok(PendingSnapshot {
            entry: ManifestEntry {
                seq: 0, // commit 期定序
                ts: 0,
                tool: tool.to_string(),
                path: rel,
                kind: KIND_CONTENT.to_string(),
                hash,
                bytes: content.len() as u64,
                journal_seq: None,
            },
        })
    }

    /// **⑦成功后入账**:FileCheckpointTaken 落 journal(可选面)+ manifest 栈
    /// 条目落盘 + 帽 enforcement。返回 journal 行 seq(journal 缺席=None)。
    pub fn commit(
        &self,
        pending: &PendingSnapshot,
        journal: Option<&JournalWriter>,
    ) -> Result<Option<u64>, String> {
        let _g = lock_for(&self.root);
        let mut entry = pending.entry.clone();
        entry.ts = unix_ms();

        if let Some(j) = journal {
            let seq = j
                .file_checkpoint_taken(
                    &entry.path,
                    &entry.kind,
                    &entry.hash,
                    entry.bytes,
                    &entry.tool,
                )
                .map_err(|e| format!("checkpoint journal failed: {e}"))?;
            entry.journal_seq = Some(seq);
        }

        self.ensure_dirs()?;
        let mut entries = self.read_manifest_locked();
        entry.seq = entries.iter().map(|e| e.seq).max().unwrap_or(0) + 1;
        let journal_seq = entry.journal_seq;
        entries.push(entry);
        self.write_manifest_locked(&entries)?;
        self.enforce_caps_locked();
        Ok(journal_seq)
    }

    /// ⑦失败时丢弃 pending(无盘面动作;孤儿 blob 由帽清理回收)
    pub fn discard(&self, _pending: &PendingSnapshot) {}

    // ------------------------------------------------------------------
    // undo(弹栈单步回退)
    // ------------------------------------------------------------------

    /// **弹栈回退**:栈顶条目恢复(路径复验 → hash 复算核对 → 写回/删除 →
    /// 实测复算 → manifest 移除 → journal 入账)。任一步失败 fail-visible,
    /// 栈不动。空栈 = 显式报错。
    pub fn undo(&self, journal: Option<&JournalWriter>) -> Result<UndoOutcome, String> {
        let _g = lock_for(&self.root);
        let mut entries = self.read_manifest_locked();
        let top = entries
            .last()
            .cloned()
            .ok_or_else(|| "no checkpoint to undo".to_string())?;

        // 恢复路径复验:快照时合法 ≠ 恢复时合法(目录重绑/白名单漂移防御)
        let (target, _) = crate::builtin_tools::fs_safety::resolve_writable_target(
            &self.workdir,
            &self.writable_dir,
            &top.path,
        )?;

        let outcome = if top.kind == KIND_CONTENT {
            let blob = self
                .read_blob(&top.hash)
                .map_err(|e| format!("checkpoint blob unavailable (seq {}): {e}", top.seq))?;
            let recomputed = evorule_digest_bytes(&blob);
            if recomputed != top.hash {
                // blob 损坏/篡改:拒恢复,栈不动(fail-visible)
                return Err(format!(
                    "checkpoint blob hash mismatch (seq {}): manifest '{}' vs recomputed '{}'",
                    top.seq, top.hash, recomputed
                ));
            }
            atomic_write(&target, &blob)?;
            // 恢复后实测复算——不信快照自报
            let restored = std::fs::read(&target)
                .map_err(|e| format!("restore verify read failed for '{}': {e}", top.path))?;
            let restored_hash = evorule_digest_bytes(&restored);
            if restored_hash != top.hash {
                return Err(format!(
                    "restore verify failed (seq {}): restored '{}' != snapshot '{}'",
                    top.seq, restored_hash, top.hash
                ));
            }
            UndoOutcome {
                path: top.path.clone(),
                kind: KIND_CONTENT.to_string(),
                restored_hash,
                bytes: blob.len() as u64,
                popped_seq: top.seq,
                remaining_depth: entries.len() - 1,
            }
        } else {
            // tombstone:恢复「此前不存在」——删除现内容(文件/空目录);
            // 目标已不存在 = 幂等通过
            match std::fs::symlink_metadata(&target) {
                Ok(meta) if meta.is_dir() => std::fs::remove_dir(&target)
                    .map_err(|e| format!("restore remove_dir failed for '{}': {e}", top.path))?,
                Ok(_) => std::fs::remove_file(&target)
                    .map_err(|e| format!("restore remove_file failed for '{}': {e}", top.path))?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(format!("restore stat failed for '{}': {e}", top.path));
                }
            }
            UndoOutcome {
                path: top.path.clone(),
                kind: KIND_TOMBSTONE.to_string(),
                restored_hash: evorule_digest_bytes(&[]),
                bytes: 0,
                popped_seq: top.seq,
                remaining_depth: entries.len() - 1,
            }
        };

        // 弹栈(恢复全成功后才移除;失败路径 manifest 未动)
        entries.pop();
        self.write_manifest_locked(&entries)?;
        // 引用归零的 blob 回收(磁盘即时释放)
        self.reclaim_unreferenced_blobs(&entries);

        if let Some(j) = journal {
            j.file_checkpoint_restored(
                &outcome.path,
                &outcome.kind,
                &outcome.restored_hash,
                outcome.popped_seq,
            )
            .map_err(|e| format!("undo journal failed: {e}"))?;
        }
        Ok(outcome)
    }

    /// 当前栈深(诊断/测试面)
    pub fn depth(&self) -> usize {
        let _g = lock_for(&self.root);
        self.read_manifest_locked().len()
    }

    // ------------------------------------------------------------------
    // 盘面原语
    // ------------------------------------------------------------------

    fn ensure_dirs(&self) -> Result<(), String> {
        std::fs::create_dir_all(self.root.join("blobs"))
            .map_err(|e| format!("checkpoint dir create failed: {e}"))
    }

    fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.jsonl")
    }

    /// 读 manifest(缺文件=空栈;损坏行跳过降级——快照面非治理权威面,
    /// journal 才是审计权威)
    fn read_manifest_locked(&self) -> Vec<ManifestEntry> {
        let Ok(text) = std::fs::read_to_string(self.manifest_path()) else {
            return Vec::new();
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| match serde_json::from_str::<ManifestEntry>(l) {
                Ok(e) => Some(e),
                Err(e) => {
                    tracing::warn!("checkpoint manifest line skipped: {e}");
                    None
                }
            })
            .collect()
    }

    fn write_manifest_locked(&self, entries: &[ManifestEntry]) -> Result<(), String> {
        self.ensure_dirs()?;
        let mut text = String::new();
        for e in entries {
            text.push_str(&serde_json::to_string(e).map_err(|e| e.to_string())?);
            text.push('\n');
        }
        let tmp = self.root.join("manifest.jsonl.tmp");
        std::fs::write(&tmp, text.as_bytes()).map_err(|e| format!("manifest write failed: {e}"))?;
        std::fs::rename(&tmp, self.manifest_path())
            .map_err(|e| format!("manifest rename failed: {e}"))
    }

    /// blob 文件名:evorule-hash 的 hex 段(Windows 文件名禁 ':')
    fn blob_path(&self, hash: &str) -> PathBuf {
        let hex = hash.strip_prefix("blake3:").unwrap_or(hash);
        self.root.join("blobs").join(hex)
    }

    /// 写 blob(同内容去重:已存在即跳过)
    fn write_blob(&self, hash: &str, content: &[u8]) -> Result<(), String> {
        let path = self.blob_path(hash);
        if path.exists() {
            return Ok(());
        }
        self.ensure_dirs()?;
        let tmp = self
            .root
            .join("blobs")
            .join(format!(".tmp-{}", hex_part(hash)));
        std::fs::write(&tmp, content).map_err(|e| format!("blob write failed: {e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("blob rename failed: {e}"))
    }

    fn read_blob(&self, hash: &str) -> Result<Vec<u8>, String> {
        std::fs::read(self.blob_path(hash)).map_err(|e| format!("blob read failed: {e}"))
    }

    /// 帽 enforcement(LRU:栈深 32 + 总字节 64MB,超限淘汰最旧;引用计数
    /// 归零的 blob 一并回收,顺带清孤儿 blob)
    fn enforce_caps_locked(&self) {
        let mut entries = self.read_manifest_locked();
        let mut evicted: Vec<String> = Vec::new();
        loop {
            let total: u64 = entries.iter().map(|e| e.bytes).sum();
            if entries.len() <= MAX_STACK_DEPTH && total <= MAX_TOTAL_BYTES {
                break;
            }
            let Some(oldest) = entries.first().cloned() else {
                break;
            };
            entries.remove(0);
            evicted.push(format!("seq={} path={}", oldest.seq, oldest.path));
        }
        if !evicted.is_empty() {
            if let Err(e) = self.write_manifest_locked(&entries) {
                tracing::warn!("checkpoint cap eviction manifest write failed: {e}");
                return;
            }
            tracing::warn!("checkpoint cap eviction: {}", evicted.join("; "));
        }
        self.reclaim_unreferenced_blobs(&entries);
    }

    /// 引用计数回收:manifest 无引用的 blob 删除(含孤儿——discard 残留)
    fn reclaim_unreferenced_blobs(&self, entries: &[ManifestEntry]) {
        let mut referenced: HashMap<&str, usize> = HashMap::new();
        for e in entries {
            *referenced.entry(e.hash.as_str()).or_insert(0) += 1;
        }
        let Ok(dir) = std::fs::read_dir(self.root.join("blobs")) else {
            return;
        };
        for item in dir.flatten() {
            let Some(name) = item.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            if name.starts_with(".tmp-") {
                let _ = std::fs::remove_file(item.path()); // 残件清理
                continue;
            }
            let digest = format!("blake3:{name}");
            if !referenced.contains_key(digest.as_str()) {
                let _ = std::fs::remove_file(item.path());
            }
        }
    }
}

fn hex_part(hash: &str) -> &str {
    hash.strip_prefix("blake3:").unwrap_or(hash)
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 原子写回(同目录 tmp + rename;edit_file 先例口径)
fn atomic_write(target: &Path, content: &[u8]) -> Result<(), String> {
    let file_name = target
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .ok_or_else(|| "invalid target file name".to_string())?;
    let tmp = target
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!("{file_name}.evo-checkpoint-tmp"));
    let res = (|| -> Result<(), String> {
        std::fs::write(&tmp, content).map_err(|e| format!("restore tmp write failed: {e}"))?;
        std::fs::rename(&tmp, target).map_err(|e| format!("restore rename failed: {e}"))
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp); // 残件清理,fail-visible
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::journal::JournalWriter;

    fn temp_workdir() -> (tempfile::TempDir, CheckpointStore) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("workspace")).unwrap();
        let store = CheckpointStore::new(dir.path(), "workspace");
        (dir, store)
    }

    fn write_target(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn content_snapshot_then_undo_restores_bytes() {
        let (dir, store) = temp_workdir();
        write_target(dir.path(), "workspace/a.txt", "hello\r\nold");
        let pending = store.pre_capture("workspace/a.txt", "file_write").unwrap();
        assert_eq!(pending.kind(), KIND_CONTENT);
        store.commit(&pending, None).unwrap();
        assert_eq!(store.depth(), 1);

        // 覆盖改坏
        write_target(dir.path(), "workspace/a.txt", "corrupted");
        let out = store.undo(None).unwrap();
        assert_eq!(out.kind, KIND_CONTENT);
        assert_eq!(out.path, "workspace/a.txt");
        assert_eq!(out.bytes, "hello\r\nold".len() as u64);
        // 字节级恢复(CRLF 保真)
        let restored = std::fs::read(dir.path().join("workspace/a.txt")).unwrap();
        assert_eq!(restored, b"hello\r\nold");
        assert_eq!(store.depth(), 0);
        // 弹栈后 blob 引用归零被回收
        assert!(!store
            .blob_path(&format!(
                "blake3:{}",
                blake3::hash(b"hello\r\nold").to_hex()
            ))
            .exists());
    }

    #[test]
    fn tombstone_snapshot_then_undo_removes_file() {
        let (dir, store) = temp_workdir();
        // 目标不存在 → tombstone
        let pending = store
            .pre_capture("workspace/new.txt", "file_create")
            .unwrap();
        assert_eq!(pending.kind(), KIND_TOMBSTONE);
        assert_eq!(pending.hash(), &evorule_digest_bytes(&[]));
        store.commit(&pending, None).unwrap();

        write_target(dir.path(), "workspace/new.txt", "agent wrote this");
        let out = store.undo(None).unwrap();
        assert_eq!(out.kind, KIND_TOMBSTONE);
        assert_eq!(out.path, "workspace/new.txt");
        assert!(!dir.path().join("workspace/new.txt").exists());
    }

    #[test]
    fn undo_on_empty_stack_fails_visibly() {
        let (_dir, store) = temp_workdir();
        let err = store.undo(None).unwrap_err();
        assert!(err.contains("no checkpoint to undo"), "got: {err}");
    }

    #[test]
    fn stack_depth_cap_evicts_oldest() {
        let (dir, store) = temp_workdir();
        for i in 0..(MAX_STACK_DEPTH + 2) {
            write_target(dir.path(), "workspace/f.txt", &format!("v{i}"));
            let p = store.pre_capture("workspace/f.txt", "file_write").unwrap();
            store.commit(&p, None).unwrap();
        }
        assert_eq!(store.depth(), MAX_STACK_DEPTH);
        // 最旧两条(v0/v1)已淘汰:undo 两次应回到 v32 状态
        store.undo(None).unwrap();
        let out = store.undo(None).unwrap();
        let restored = std::fs::read(dir.path().join("workspace/f.txt")).unwrap();
        assert_eq!(restored, b"v32");
        assert_eq!(out.popped_seq, 33); // seq 单调,不因淘汰重置
    }

    #[test]
    fn total_bytes_cap_evicts_oldest() {
        let (dir, store) = temp_workdir();
        let big = "x".repeat(40 * 1024 * 1024); // 40MB ×2 = 80MB > 64MB
        write_target(dir.path(), "workspace/big.txt", &big);
        let p1 = store
            .pre_capture("workspace/big.txt", "file_write")
            .unwrap();
        store.commit(&p1, None).unwrap();
        write_target(dir.path(), "workspace/big.txt", &format!("{big}y"));
        let p2 = store
            .pre_capture("workspace/big.txt", "file_write")
            .unwrap();
        store.commit(&p2, None).unwrap();
        // 总帽 64MB:第二条(41MB)入账后合计超限,最旧被淘汰
        assert_eq!(store.depth(), 1);
        let out = store.undo(None).unwrap();
        assert_eq!(out.bytes, (40 * 1024 * 1024 + 1) as u64);
    }

    #[test]
    fn tampered_blob_is_rejected_and_stack_untouched() {
        let (dir, store) = temp_workdir();
        write_target(dir.path(), "workspace/a.txt", "original");
        let p = store.pre_capture("workspace/a.txt", "file_write").unwrap();
        store.commit(&p, None).unwrap();
        // 篡改 blob 内容(保持文件名)
        let hex = p.hash().strip_prefix("blake3:").unwrap();
        std::fs::write(store.root.join("blobs").join(hex), b"tampered").unwrap();
        let err = store.undo(None).unwrap_err();
        assert!(err.contains("hash mismatch"), "got: {err}");
        assert_eq!(store.depth(), 1); // 栈不动
        assert_eq!(
            std::fs::read(dir.path().join("workspace/a.txt")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn restore_path_is_reverified_against_writable_dir() {
        let (dir, store) = temp_workdir();
        write_target(dir.path(), "workspace/a.txt", "x");
        let p = store.pre_capture("workspace/a.txt", "file_write").unwrap();
        store.commit(&p, None).unwrap();
        // 模拟盘面被改:manifest 内 path 换成越界相对路径(如历史数据漂移)
        let entries = vec![ManifestEntry {
            seq: p.entry.seq.max(1),
            ts: 0,
            tool: "file_write".into(),
            path: "../outside.txt".into(),
            kind: KIND_CONTENT.into(),
            hash: p.hash().to_string(),
            bytes: 1,
            journal_seq: None,
        }];
        store.write_manifest_locked(&entries).unwrap();
        let err = store.undo(None).unwrap_err();
        // resolve_writable_target 对 `..` 一律拒
        assert!(err.contains(".."), "got: {err}");
    }

    #[test]
    fn journal_face_records_taken_and_restored() {
        let (dir, store) = temp_workdir();
        let journal_dir = dir.path().join("data").join("sessions").join("s1");
        std::fs::create_dir_all(&journal_dir).unwrap();
        let journal = JournalWriter::open(&journal_dir, "s1").unwrap();

        write_target(dir.path(), "workspace/a.txt", "v1");
        let p = store.pre_capture("workspace/a.txt", "file_write").unwrap();
        let jseq = store.commit(&p, Some(&journal)).unwrap().unwrap();
        write_target(dir.path(), "workspace/a.txt", "v2");
        store.undo(Some(&journal)).unwrap();

        let path = JournalWriter::path_for(&journal_dir, "s1");
        let lines = crate::agent::journal::read_all(&path).unwrap();
        let kinds: Vec<String> = lines
            .iter()
            .map(|l| match &l.event {
                crate::agent::journal::JournalEvent::FileCheckpointTaken { .. } => {
                    "taken".to_string()
                }
                crate::agent::journal::JournalEvent::FileCheckpointRestored { .. } => {
                    "restored".to_string()
                }
                _ => String::new(),
            })
            .collect();
        assert_eq!(kinds, vec!["taken".to_string(), "restored".to_string()]);
        // manifest 行与 journal seq 关联
        let manifest = std::fs::read_to_string(store.manifest_path()).unwrap();
        assert!(
            manifest.trim().is_empty(),
            "stack should be empty after undo"
        );
        let _ = jseq;
    }

    #[test]
    fn pre_capture_rejects_paths_outside_writable_dir() {
        let (_dir, store) = temp_workdir();
        let err = store.pre_capture("../evil.txt", "file_write").unwrap_err();
        assert!(
            err.contains("..") || err.contains("not allowed"),
            "got: {err}"
        );
    }

    #[test]
    fn manifest_survives_store_recreation() {
        let (dir, store) = temp_workdir();
        write_target(dir.path(), "workspace/a.txt", "v1");
        let p = store.pre_capture("workspace/a.txt", "file_write").unwrap();
        store.commit(&p, None).unwrap();
        // 新实例(同 root)读到同一 SSOT 栈——undo 工具与 runner 钩子双实例形态
        let store2 = CheckpointStore::new(dir.path(), "workspace");
        assert_eq!(store2.depth(), 1);
        let out = store2.undo(None).unwrap();
        assert_eq!(out.popped_seq, 1);
    }
}
