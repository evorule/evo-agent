// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 索引库持久层（B14 结构面）—— SQLite（rusqlite bundled / WAL）
//!
//! 表结构（15 号 §3.1，v2 语义）：
//! - `files`：path（相对工作区根，`/` 分隔）+ content_hash（blake3 hex）+ mtime
//!   + size —— 增量对账键（启动哈希对账 / notify 增量判定）；
//! - `symbols`：file/kind/name/line_start/line_end —— 符号抽取产物；
//! - `idents`：file/name/count —— 标识符出现计数（name 级原始事实，按文件聚合）；
//! - `refs`：from_file/to_file/weight —— 文件级引用边（PageRank 图谱直接输入，
//!   派生物化视图：idents ∩ def 名集合 → owner 归属，weight=Σ√count）；
//!   name 级原始事实保留在 `idents`，可随时重刷，无信息损失。
//!
//! 并发纪律（15 号 §八风险 5 裁决）：单连接 + `Mutex` 串行写；WAL 允许读并发。
//! 落盘边界（D8）：源码内容不落索引库——只存哈希 + 符号 + 元数据，展示时按需
//! 回读源文件。
//!
//! 与 LexStore（[`crate::agent::lexstore`]）同款单写者形态；差异在定位：索引库
//! 是 serve 本地派生缓存（可随时 rebuild 重建），非第二真相源。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection};

/// schema 版本（迁移判据；破坏性变更须升版并写迁移路径）
///
/// v2：新增 `idents`（标识符出现计数，按文件聚合落库）——refs 升格为派生
/// 物化视图（idents ∩ def 名集合，`refresh_refs` 全量重刷），无内存态、
/// 重启自愈、增量天然正确。
pub const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files (
    path         TEXT PRIMARY KEY,
    content_hash TEXT NOT NULL,
    mtime        INTEGER NOT NULL,
    size         INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS symbols (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    file       TEXT NOT NULL,
    kind       TEXT NOT NULL,
    name       TEXT NOT NULL,
    line_start INTEGER NOT NULL,
    line_end   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file);
CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
CREATE TABLE IF NOT EXISTS idents (
    file  TEXT NOT NULL,
    name  TEXT NOT NULL,
    count INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_idents_name ON idents(name);
CREATE INDEX IF NOT EXISTS idx_idents_file ON idents(file);
CREATE TABLE IF NOT EXISTS refs (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    from_file TEXT NOT NULL,
    to_file   TEXT NOT NULL,
    weight    REAL NOT NULL DEFAULT 1.0
);
CREATE INDEX IF NOT EXISTS idx_refs_from ON refs(from_file);
CREATE INDEX IF NOT EXISTS idx_refs_to   ON refs(to_file);
";

/// 索引库错误（存储层错误不携带链上语义）
#[derive(Debug)]
pub struct IndexError(pub String);

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "index: {}", self.0)
    }
}

impl std::error::Error for IndexError {}

/// 符号行（chunker 抽取产物；所属文件由写入 API 的 `path` 参数承载，行内不重复存）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    /// 符号类别（fn / class / method / struct / interface / ...）
    pub kind: String,
    /// 符号名
    pub name: String,
    /// 起始行（1 起）
    pub line_start: u32,
    /// 结束行（含）
    pub line_end: u32,
}

/// 引用边（文件级派生物化——`refresh_refs` 重刷；to_file = 被引符号 owner 文件）
#[derive(Debug, Clone, PartialEq)]
pub struct RefRow {
    /// 引用来源文件
    pub from_file: String,
    /// 被引符号 owner 文件（同名多 def 取首见，file 升序）
    pub to_file: String,
    /// 边权（Σ√出现次数）
    pub weight: f64,
}

/// 文件哈希行（启动对账键）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHash {
    /// 相对路径（`/` 分隔）
    pub path: String,
    /// 内容哈希（blake3 hex）
    pub content_hash: String,
    /// 修改时间（UNIX 秒）
    pub mtime: i64,
}

/// 符号最小读出行（检索匹配 / owner 映射用）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolMin {
    /// 所属文件（相对路径，`/` 分隔）
    pub file: String,
    /// 符号类别
    pub kind: String,
    /// 符号名
    pub name: String,
    /// 起始行（1 起）
    pub line_start: u32,
    /// 结束行（含）
    pub line_end: u32,
}

/// 索引库（单连接 + `Mutex` 串行写；WAL）
pub struct IndexStore {
    conn: Mutex<Connection>,
}

impl IndexStore {
    /// 打开索引库（建父目录；迁移幂等：CREATE IF NOT EXISTS + user_version 记账）
    ///
    /// 现库 `user_version` 高于本二进制认知时拒绝打开（防旧版二进制写坏新版库）。
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| IndexError(format!("create_dir_all: {e}")))?;
        }
        let conn = Connection::open(path).map_err(|e| IndexError(format!("open: {e}")))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| IndexError(format!("wal: {e}")))?;
        let current: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| IndexError(format!("read user_version: {e}")))?;
        if current > SCHEMA_VERSION {
            return Err(IndexError(format!(
                "index db schema v{current} is newer than supported v{SCHEMA_VERSION}"
            )));
        }
        conn.execute_batch(SCHEMA)
            .map_err(|e| IndexError(format!("schema: {e}")))?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|e| IndexError(format!("set user_version: {e}")))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 整文件替换（事务内：删旧 symbols/refs/idents → upsert files → 插新行）
    ///
    /// 单文件增量更新的原子入口：解析失败/中途出错整事务回滚，库内不残留半新半旧。
    /// 调用方随后应调 [`IndexStore::refresh_refs`] 重刷派生 refs。
    pub fn replace_file(
        &self,
        path: &str,
        content_hash: &str,
        mtime: i64,
        size: i64,
        symbols: &[SymbolRow],
        idents: &[(String, u32)],
    ) -> Result<(), IndexError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| IndexError(format!("replace tx: {e}")))?;
        tx.execute("DELETE FROM idents WHERE file = ?1", params![path])
            .map_err(|e| IndexError(format!("replace del idents: {e}")))?;
        tx.execute("DELETE FROM symbols WHERE file = ?1", params![path])
            .map_err(|e| IndexError(format!("replace del symbols: {e}")))?;
        tx.execute(
            "INSERT INTO files(path, content_hash, mtime, size) VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET
             content_hash = excluded.content_hash, mtime = excluded.mtime, size = excluded.size",
            params![path, content_hash, mtime, size],
        )
        .map_err(|e| IndexError(format!("replace upsert file: {e}")))?;
        for s in symbols {
            tx.execute(
                "INSERT INTO symbols(file, kind, name, line_start, line_end) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![path, s.kind, s.name, s.line_start, s.line_end],
            )
            .map_err(|e| IndexError(format!("replace insert symbol: {e}")))?;
        }
        for (name, count) in idents {
            tx.execute(
                "INSERT INTO idents(file, name, count) VALUES(?1, ?2, ?3)",
                params![path, name, count],
            )
            .map_err(|e| IndexError(format!("replace insert ident: {e}")))?;
        }
        tx.commit()
            .map_err(|e| IndexError(format!("replace commit: {e}")))?;
        Ok(())
    }

    /// 移除文件及其符号/引用（删除文件或重建剔除时）
    pub fn remove_file(&self, path: &str) -> Result<(), IndexError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| IndexError(format!("remove tx: {e}")))?;
        tx.execute("DELETE FROM refs WHERE from_file = ?1", params![path])
            .map_err(|e| IndexError(format!("remove del refs: {e}")))?;
        tx.execute("DELETE FROM idents WHERE file = ?1", params![path])
            .map_err(|e| IndexError(format!("remove del idents: {e}")))?;
        tx.execute("DELETE FROM symbols WHERE file = ?1", params![path])
            .map_err(|e| IndexError(format!("remove del symbols: {e}")))?;
        tx.execute("DELETE FROM files WHERE path = ?1", params![path])
            .map_err(|e| IndexError(format!("remove del file: {e}")))?;
        tx.commit()
            .map_err(|e| IndexError(format!("remove commit: {e}")))?;
        Ok(())
    }

    /// 全部文件哈希行（启动对账：磁盘实况 vs 库内清单，差集决定增删）
    pub fn file_hashes(&self) -> Result<Vec<FileHash>, IndexError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = conn
            .prepare("SELECT path, content_hash, mtime FROM files ORDER BY path")
            .map_err(|e| IndexError(format!("file_hashes prepare: {e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok(FileHash {
                    path: r.get(0)?,
                    content_hash: r.get(1)?,
                    mtime: r.get(2)?,
                })
            })
            .map_err(|e| IndexError(format!("file_hashes query: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| IndexError(format!("file_hashes row: {e}")))?);
        }
        Ok(out)
    }

    /// 计数三元组 `(files, symbols, refs)`（status 面就绪度输入）
    pub fn counts(&self) -> Result<(u64, u64, u64), IndexError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let one = |sql: &str| -> Result<u64, IndexError> {
            conn.query_row(sql, [], |r| r.get::<_, i64>(0))
                .map(|n| n.max(0) as u64)
                .map_err(|e| IndexError(format!("counts({sql}): {e}")))
        };
        Ok((
            one("SELECT COUNT(*) FROM files")?,
            one("SELECT COUNT(*) FROM symbols")?,
            one("SELECT COUNT(*) FROM refs")?,
        ))
    }

    /// 重刷派生 refs：`idents ∩ def 名集合`（同名多 def 取首见文件为 owner），
    /// 自环剔除，weight=√出现次数。返回写入边数。
    ///
    /// 全量集合运算（单连接内两读一写）；rebuild 后与每次增量替换后调用。
    pub fn refresh_refs(&self) -> Result<usize, IndexError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        // def 名 → owner 文件（首见，file 升序稳定）
        let mut stmt = conn
            .prepare("SELECT name, file FROM symbols ORDER BY file")
            .map_err(|e| IndexError(format!("refresh_refs owner prepare: {e}")))?;
        let mut owner: HashMap<String, String> = HashMap::new();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| IndexError(format!("refresh_refs owner query: {e}")))?;
        for row in rows {
            let (name, file) =
                row.map_err(|e| IndexError(format!("refresh_refs owner row: {e}")))?;
            owner.entry(name).or_insert(file);
        }
        drop(stmt);
        // idents → 文件级边
        let mut stmt = conn
            .prepare("SELECT file, name, count FROM idents")
            .map_err(|e| IndexError(format!("refresh_refs idents prepare: {e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| IndexError(format!("refresh_refs idents query: {e}")))?;
        let mut edges: HashMap<(String, String), f64> = HashMap::new();
        for row in rows {
            let (from, name, count) =
                row.map_err(|e| IndexError(format!("refresh_refs idents row: {e}")))?;
            if let Some(to) = owner.get(&name) {
                if *to == from {
                    continue; // 自环（def 文件内的自引用不成边）
                }
                *edges.entry((from, to.clone())).or_insert(0.0) += (count.max(0) as f64).sqrt();
            }
        }
        drop(stmt);
        // 物化写回（文件级边：to_file = owner）
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| IndexError(format!("refresh_refs tx: {e}")))?;
        tx.execute("DELETE FROM refs", [])
            .map_err(|e| IndexError(format!("refresh_refs clear: {e}")))?;
        let n = edges.len();
        for ((from, to), weight) in &edges {
            tx.execute(
                "INSERT INTO refs(from_file, to_file, weight) VALUES(?1, ?2, ?3)",
                params![from, to, weight],
            )
            .map_err(|e| IndexError(format!("refresh_refs insert: {e}")))?;
        }
        tx.commit()
            .map_err(|e| IndexError(format!("refresh_refs commit: {e}")))?;
        Ok(n)
    }

    /// 全部 refs 边（PageRank 图谱直接输入；文件级）
    pub fn all_refs(&self) -> Result<Vec<RefRow>, IndexError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = conn
            .prepare("SELECT from_file, to_file, weight FROM refs")
            .map_err(|e| IndexError(format!("all_refs prepare: {e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok(RefRow {
                    from_file: r.get(0)?,
                    to_file: r.get(1)?,
                    weight: r.get(2)?,
                })
            })
            .map_err(|e| IndexError(format!("all_refs query: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| IndexError(format!("all_refs row: {e}")))?);
        }
        Ok(out)
    }

    /// 全部符号最小行集（file 升序；万级文件量级单读可承受）
    pub fn all_symbols_min(&self) -> Result<Vec<SymbolMin>, IndexError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = conn
            .prepare("SELECT file, kind, name, line_start, line_end FROM symbols ORDER BY file")
            .map_err(|e| IndexError(format!("all_symbols_min prepare: {e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SymbolMin {
                    file: r.get(0)?,
                    kind: r.get(1)?,
                    name: r.get(2)?,
                    line_start: r.get(3)?,
                    line_end: r.get(4)?,
                })
            })
            .map_err(|e| IndexError(format!("all_symbols_min query: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| IndexError(format!("all_symbols_min row: {e}")))?);
        }
        Ok(out)
    }

    /// 指定文件的符号行（检索结果附符号；file 升序）
    pub fn file_symbols(&self, file: &str) -> Result<Vec<SymbolRow>, IndexError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = conn
            .prepare(
                "SELECT kind, name, line_start, line_end FROM symbols
                 WHERE file = ?1 ORDER BY line_start",
            )
            .map_err(|e| IndexError(format!("file_symbols prepare: {e}")))?;
        let rows = stmt
            .query_map(params![file], |r| {
                Ok(SymbolRow {
                    kind: r.get(0)?,
                    name: r.get(1)?,
                    line_start: r.get(2)?,
                    line_end: r.get(3)?,
                })
            })
            .map_err(|e| IndexError(format!("file_symbols query: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| IndexError(format!("file_symbols row: {e}")))?);
        }
        Ok(out)
    }

    /// 清空全部索引数据（rebuild 全量重建前；表结构保留）
    pub fn clear(&self) -> Result<(), IndexError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| IndexError(format!("clear tx: {e}")))?;
        for sql in [
            "DELETE FROM refs",
            "DELETE FROM idents",
            "DELETE FROM symbols",
            "DELETE FROM files",
        ] {
            tx.execute(sql, [])
                .map_err(|e| IndexError(format!("clear: {e}")))?;
        }
        tx.commit()
            .map_err(|e| IndexError(format!("clear commit: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, IndexStore) {
        let td = tempfile::tempdir().unwrap();
        let s = IndexStore::open(&td.path().join("idx").join("index.db")).unwrap();
        (td, s)
    }

    #[test]
    fn open_is_idempotent_and_sets_schema_version() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("index.db");
        {
            let _s = IndexStore::open(&db).unwrap();
        }
        let s2 = IndexStore::open(&db).unwrap();
        assert_eq!(s2.counts().unwrap(), (0, 0, 0));
        let conn = s2.conn.lock().unwrap_or_else(|p| p.into_inner());
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn open_rejects_newer_schema() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("index.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        let Err(err) = IndexStore::open(&db) else {
            panic!("expected newer-schema rejection");
        };
        assert!(err.0.contains("newer than supported"));
    }

    #[test]
    fn replace_file_upserts_atomically() {
        let (_td, s) = store();
        let syms = |name: &str| {
            vec![SymbolRow {
                kind: "fn".into(),
                name: name.into(),
                line_start: 1,
                line_end: 2,
            }]
        };
        // a.rs 定义 alpha 并引用 beta（b.rs 定义）→ 重刷后成 1 条边
        s.replace_file(
            "a.rs",
            "hash-a",
            100,
            10,
            &syms("alpha"),
            &[("beta".into(), 3)],
        )
        .unwrap();
        s.replace_file("b.rs", "hash-b", 200, 20, &syms("beta"), &[])
            .unwrap();
        assert_eq!(s.refresh_refs().unwrap(), 1);
        assert_eq!(s.counts().unwrap(), (2, 2, 1));
        let refs = s.all_refs().unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].from_file, "a.rs");
        assert_eq!(refs[0].to_file, "b.rs");
        assert!((refs[0].weight - 3.0f64.sqrt()).abs() < 1e-9);
        // 同文件重写：旧 symbols/idents 整体淘汰，重刷后旧边不残留
        s.replace_file("a.rs", "hash-a2", 300, 11, &syms("alpha2"), &[])
            .unwrap();
        assert_eq!(s.refresh_refs().unwrap(), 0);
        assert_eq!(s.counts().unwrap(), (2, 2, 0));
        let hs = s.file_hashes().unwrap();
        let a = hs.iter().find(|h| h.path == "a.rs").unwrap();
        assert_eq!(a.content_hash, "hash-a2");
        assert_eq!(a.mtime, 300);
    }

    #[test]
    fn refresh_refs_owner_map_and_self_edge_exclusion() {
        let (_td, s) = store();
        let syms = |name: &str| {
            vec![SymbolRow {
                kind: "fn".into(),
                name: name.into(),
                line_start: 1,
                line_end: 1,
            }]
        };
        // a.rs 定义 alpha+dup；b.rs 定义 beta+dup；dup 的 owner 取首见文件（file 升序 = a.rs）
        s.replace_file(
            "a.rs",
            "h",
            1,
            1,
            &syms("alpha"),
            &[
                ("alpha".into(), 2), // 自环剔除
                ("beta".into(), 4),  // → b.rs，weight=√4=2
                ("dup".into(), 1),   // owner=a.rs，自环剔除
            ],
        )
        .unwrap();
        s.replace_file(
            "b.rs",
            "h",
            1,
            1,
            &syms("beta"),
            &[
                ("beta".into(), 1),  // 自环剔除
                ("alpha".into(), 9), // → a.rs，weight=√9=3
            ],
        )
        .unwrap();
        s.replace_file(
            "c.rs",
            "h",
            1,
            1,
            &[],
            &[
                ("alpha".into(), 2), // → a.rs，√2
                ("beta".into(), 2),  // → b.rs，√2
                ("ghost".into(), 5), // 无 def，不成边
            ],
        )
        .unwrap();
        assert_eq!(s.refresh_refs().unwrap(), 4);
        let refs = s.all_refs().unwrap();
        let w = |from: &str, to: &str| -> f64 {
            refs.iter()
                .find(|r| r.from_file == from && r.to_file == to)
                .map(|r| r.weight)
                .unwrap_or_else(|| panic!("missing edge {from} -> {to}"))
        };
        assert!((w("a.rs", "b.rs") - 2.0).abs() < 1e-9);
        assert!((w("b.rs", "a.rs") - 3.0).abs() < 1e-9);
        assert!((w("c.rs", "a.rs") - 2.0f64.sqrt()).abs() < 1e-9);
        assert!((w("c.rs", "b.rs") - 2.0f64.sqrt()).abs() < 1e-9);
        assert!(!refs.iter().any(|r| r.from_file == r.to_file));
    }

    #[test]
    fn remove_file_clears_all_tables() {
        let (_td, s) = store();
        s.replace_file(
            "a.rs",
            "hash-a",
            1,
            5,
            &[SymbolRow {
                kind: "fn".into(),
                name: "f".into(),
                line_start: 1,
                line_end: 1,
            }],
            &[("g".into(), 2)],
        )
        .unwrap();
        // g 无 def，重刷不成边
        assert_eq!(s.refresh_refs().unwrap(), 0);
        s.remove_file("a.rs").unwrap();
        assert_eq!(s.counts().unwrap(), (0, 0, 0));
        // 幂等：重复删除不报错
        s.remove_file("a.rs").unwrap();
    }

    #[test]
    fn clear_empties_but_keeps_schema() {
        let (_td, s) = store();
        s.replace_file("a.rs", "h", 1, 1, &[], &[]).unwrap();
        s.clear().unwrap();
        assert_eq!(s.counts().unwrap(), (0, 0, 0));
        s.replace_file("b.rs", "h", 1, 1, &[], &[]).unwrap();
        assert_eq!(s.counts().unwrap(), (1, 0, 0));
    }
}
