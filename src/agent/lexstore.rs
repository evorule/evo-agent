//! 长程设计档阶段 1(F-618)LexStore——检索优先的本地索引缓存(存储设计档 §四)。
//!
//! 定位:**索引皆视图**——本 store 是 evorule 共享账本的派生缓存,不是第二
//! 真相源(RL-B1);payload 以可丢弃缓存形态存储(设计档允许),损坏/
//! 落后的代价只是性能与新鲜度,调用方回退全量拉取路径(I14 降级兜底)。
//!
//! v0 范围:postings(bigram 倒排)+ facts(分区事实缓存)+ partitions
//! (分区新鲜度)。实体表/因果表随阶段推进补齐(设计档 §4.2)。
//!
//! 跨源扩展机制(注册规格批 A):facts/postings 增行级型别直证列
//! (`source`=行来源类别,`mem_type`=记忆四型)——跨源检索的 kind 过滤
//! 走列直证而非前缀约定;受限本地源用合成 id(bit63 标记)登记,
//! 与账本正 id 空间永不相交。门禁:默认查询(无型别过滤)结果
//! 与历史逐字节一致。
//!
//! 新鲜度协议:`cached_facts(prefix, ttl)` 在 TTL 内返回缓存(零网络),
//! 过期返回 None——调用方全量拉取后 `replace_partition` 整分区替换
//! (确定性,分区内旧事实随之淘汰)。TTL 窗口内的跨代理写不可见,
//! 属 v0 已声明的边界(设计档开放问题二)。

use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 记忆四型(`mem_type` 列取值;行级型别直证,跨源检索过滤键)
pub mod mem_type {
    /// 语义记忆(stable 事实/知识碎片等)
    pub const SEMANTIC: &str = "semantic";
    /// 情景记忆(events)
    pub const EPISODIC: &str = "episodic";
    /// 程序记忆(技能/材料等)
    pub const PROCEDURAL: &str = "procedural";
    /// 工作记忆(会话摘要/journal 摘要等)
    pub const WORK: &str = "work";
}

/// 行来源类别(`source` 列取值)
pub mod row_source {
    /// 账本原生行(经 replace_partition 同步的共享账本事实)
    pub const LEDGER: &str = "ledger";
}

/// 受限本地源合成 id 的标记位(bit63;账本正 id 空间从 0 顺序分配,永不相交)
pub const SYNTHETIC_ID_FLAG: u64 = 0x8000_0000_0000_0000;

/// 受限本地源合成 id:bit63 置 1,低 63 位 = path(trim 规范化后)的
/// 规范哈希(evorule-hash)截断——确定性、重注册稳定。
/// 仅用于确不入账的本地源(如技能正文索引);入账源一律用真 fact_id。
pub fn synthetic_fact_id(path: &str) -> u64 {
    let hex = evorule_hash::digest(path.trim().as_bytes());
    let low =
        u64::from_str_radix(&hex[..hex.len().min(16)], 16).unwrap_or(0) & 0x7FFF_FFFF_FFFF_FFFF;
    low | SYNTHETIC_ID_FLAG
}

/// 前缀→型别推导(机制内置映射;逐行覆盖权在写入方 value JSON 的
/// `mem_type` 字段)。知识候选家族缺省 semantic(五类碎片逐行覆盖
/// 待其写入侧补字段后生效)。
fn mem_type_for_prefix(prefix: &str) -> &'static str {
    if prefix.contains(".procedural.") {
        mem_type::PROCEDURAL
    } else if prefix.contains(".events.") {
        mem_type::EPISODIC
    } else if prefix.contains(".sessions.") || prefix.contains(".work.") {
        mem_type::WORK
    } else {
        mem_type::SEMANTIC
    }
}

/// 行级型别判定:value JSON 的 `mem_type` 字段=写入方逐行覆盖(策略数据);
/// 缺省=前缀推导(机制)。`source` 列不由 value JSON 承载(MemoryRecord
/// 既有 source 域语义为来源域标注,不同物)——由写入 API 赋值,
/// 账本同步路径恒为 ledger。
fn row_mem_type(value: &serde_json::Value, prefix: &str) -> String {
    value
        .get("mem_type")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| mem_type_for_prefix(prefix).to_string())
}

/// LexStore 错误(简洁口径:存储层错误不携带链上语义)
#[derive(Debug)]
pub struct LexError(pub String);

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lexstore: {}", self.0)
    }
}

impl std::error::Error for LexError {}

/// 缓存事实(fact_id / path / 反序列化后的值)
#[derive(Debug, Clone)]
pub struct CachedFact {
    pub fact_id: u64,
    pub path: String,
    pub value: serde_json::Value,
}

/// LexStore:本地检索缓存(WAL;内部 Mutex 串行化——单写者纪律)
pub struct LexStore {
    conn: Mutex<Connection>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 旧库幂等补列(PRAGMA table_info 检查后 ALTER ADD COLUMN;DEFAULT 兜底)
fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    column_ddl: &str,
) -> Result<(), LexError> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| LexError(format!("pragma: {e}")))?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| LexError(format!("pragma query: {e}")))?
        .any(|c| c.map(|n| n == column).unwrap_or(false));
    drop(stmt);
    if !exists {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column_ddl}"))
            .map_err(|e| LexError(format!("migrate: {e}")))?;
    }
    Ok(())
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS partitions(
    prefix TEXT PRIMARY KEY,
    fetched_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS facts(
    prefix TEXT NOT NULL,
    fact_id INTEGER NOT NULL,
    path TEXT NOT NULL,
    value_json TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'ledger',
    mem_type TEXT NOT NULL DEFAULT 'semantic',
    PRIMARY KEY(prefix, fact_id)
);
CREATE TABLE IF NOT EXISTS postings(
    bigram TEXT NOT NULL,
    fact_id INTEGER NOT NULL,
    prefix TEXT NOT NULL,
    mem_type TEXT NOT NULL DEFAULT 'semantic',
    PRIMARY KEY(bigram, fact_id, prefix)
);
CREATE INDEX IF NOT EXISTS idx_postings_bigram ON postings(bigram);
CREATE TABLE IF NOT EXISTS entities(
    entity TEXT NOT NULL,
    fact_id INTEGER NOT NULL,
    prefix TEXT NOT NULL,
    PRIMARY KEY(entity, fact_id)
);
CREATE TABLE IF NOT EXISTS timeline(
    fact_id INTEGER PRIMARY KEY,
    prefix TEXT NOT NULL,
    kind TEXT,
    valid_at INTEGER,
    lifecycle_state TEXT,
    superseded_by TEXT
);
CREATE INDEX IF NOT EXISTS idx_timeline_prefix ON timeline(prefix, kind, valid_at);
CREATE TABLE IF NOT EXISTS causes(
    fact_id INTEGER NOT NULL,
    cause_id INTEGER NOT NULL,
    PRIMARY KEY(fact_id, cause_id)
);
";

impl LexStore {
    /// 打开(或创建)本地缓存库;WAL 模式;schema 幂等初始化
    pub fn open(path: &Path) -> Result<Self, LexError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| LexError(format!("create_dir_all: {e}")))?;
        }
        let conn = Connection::open(path).map_err(|e| LexError(format!("open: {e}")))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| LexError(format!("wal: {e}")))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| LexError(format!("schema: {e}")))?;
        // 旧库迁移:幂等补型别直证列(DEFAULT 兜底,存量行自动归类)
        ensure_column(
            &conn,
            "facts",
            "source",
            "source TEXT NOT NULL DEFAULT 'ledger'",
        )?;
        ensure_column(
            &conn,
            "facts",
            "mem_type",
            "mem_type TEXT NOT NULL DEFAULT 'semantic'",
        )?;
        ensure_column(
            &conn,
            "postings",
            "mem_type",
            "mem_type TEXT NOT NULL DEFAULT 'semantic'",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 整分区替换(全量拉取后调用):淘汰该前缀旧事实与倒排,写入新事实,
    /// 并为每条事实的 path+value 分词倒排(R05 同源分词=同一实现同一索引)。
    pub fn replace_partition(
        &self,
        prefix: &str,
        facts: &[(u64, String, serde_json::Value)],
    ) -> Result<(), LexError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| LexError(format!("tx: {e}")))?;
        tx.execute("DELETE FROM facts WHERE prefix = ?1", [prefix])
            .map_err(|e| LexError(format!("delete facts: {e}")))?;
        tx.execute("DELETE FROM postings WHERE prefix = ?1", [prefix])
            .map_err(|e| LexError(format!("delete postings: {e}")))?;
        tx.execute("DELETE FROM entities WHERE prefix = ?1", [prefix])
            .map_err(|e| LexError(format!("delete entities: {e}")))?;
        tx.execute("DELETE FROM timeline WHERE prefix = ?1", [prefix])
            .map_err(|e| LexError(format!("delete timeline: {e}")))?;
        tx.execute(
            "DELETE FROM causes WHERE fact_id IN (SELECT fact_id FROM timeline WHERE prefix = ?1)",
            [prefix],
        )
        .map_err(|e| LexError(format!("delete causes: {e}")))?;
        for (fact_id, path, value) in facts {
            let value_json =
                serde_json::to_string(value).map_err(|e| LexError(format!("serialize: {e}")))?;
            let mem_type = row_mem_type(value, prefix);
            tx.execute(
                "INSERT INTO facts(prefix, fact_id, path, value_json, source, mem_type)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    prefix,
                    fact_id,
                    path,
                    value_json,
                    row_source::LEDGER,
                    mem_type
                ],
            )
            .map_err(|e| LexError(format!("insert fact: {e}")))?;
            let text = format!("{path} {value}");
            for token in crate::agent::memory::tokenize_for_match(&text) {
                tx.execute(
                    "INSERT OR IGNORE INTO postings(bigram, fact_id, prefix, mem_type)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![token, fact_id, prefix, mem_type],
                )
                .map_err(|e| LexError(format!("insert posting: {e}")))?;
            }
            // F-609 落标 + P5 时间线 + P2 实体 + P3 因果：抽取器管线
            // （从 value JSON 确定性提取，零 LLM——抽取器即机制本体）
            let ts = value.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
            let state = value
                .get("lifecycle_state")
                .and_then(|v| v.as_str())
                .unwrap_or("Settled");
            tx.execute(
                "INSERT INTO timeline(fact_id, prefix, kind, valid_at, lifecycle_state, superseded_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL)
                 ON CONFLICT(fact_id) DO UPDATE SET lifecycle_state = excluded.lifecycle_state",
                rusqlite::params![fact_id, prefix, "fact", ts, state],
            )
            .map_err(|e| LexError(format!("insert timeline: {e}")))?;
            if let Some(entities) = value.get("entities").and_then(|v| v.as_array()) {
                for ent in entities {
                    if let Some(name) = ent.get("name").and_then(|v| v.as_str()) {
                        tx.execute(
                            "INSERT OR IGNORE INTO entities(entity, fact_id, prefix) VALUES (?1, ?2, ?3)",
                            rusqlite::params![name, fact_id, prefix],
                        )
                        .map_err(|e| LexError(format!("insert entity: {e}")))?;
                    }
                }
            }
            if let Some(cause) = value.get("cause_fact_id").and_then(|v| v.as_u64()) {
                tx.execute(
                    "INSERT OR IGNORE INTO causes(fact_id, cause_id) VALUES (?1, ?2)",
                    rusqlite::params![fact_id, cause],
                )
                .map_err(|e| LexError(format!("insert cause: {e}")))?;
            }
        }
        tx.execute(
            "INSERT INTO partitions(prefix, fetched_at) VALUES (?1, ?2)
             ON CONFLICT(prefix) DO UPDATE SET fetched_at = excluded.fetched_at",
            rusqlite::params![prefix, now_secs() as i64],
        )
        .map_err(|e| LexError(format!("upsert partition: {e}")))?;
        tx.commit().map_err(|e| LexError(format!("commit: {e}")))?;
        Ok(())
    }

    /// TTL 内返回缓存事实(零网络);过期/无缓存返回 None(调用方走全量拉取
    /// + replace_partition 刷新)。
    pub fn cached_facts(&self, prefix: &str, ttl_secs: u64) -> Option<Vec<CachedFact>> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let fetched_at: i64 = conn
            .query_row(
                "SELECT fetched_at FROM partitions WHERE prefix = ?1",
                [prefix],
                |row| row.get(0),
            )
            .ok()?;
        let now = now_secs() as i64;
        if now.saturating_sub(fetched_at) >= ttl_secs as i64 {
            return None;
        }
        let mut stmt = conn
            .prepare("SELECT fact_id, path, value_json FROM facts WHERE prefix = ?1")
            .ok()?;
        let rows = stmt
            .query_map([prefix], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .ok()?;
        let mut out = Vec::new();
        for row in rows {
            let (fact_id, path, value_json) = row.ok()?;
            let value = serde_json::from_str(&value_json).ok()?;
            out.push(CachedFact {
                fact_id: fact_id as u64,
                path,
                value,
            });
        }
        Some(out)
    }

    /// 反查 fact_id → path(F-616 usage 批量回写的路径定位)
    pub fn paths_by_fact_ids(&self, fact_ids: &[u64]) -> std::collections::HashMap<u64, String> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut out = std::collections::HashMap::new();
        for fid in fact_ids {
            if let Ok(path) = conn.query_row(
                "SELECT path FROM facts WHERE fact_id = ?1 LIMIT 1",
                [fid],
                |row| row.get::<_, String>(0),
            ) {
                out.insert(*fid, path);
            }
        }
        out
    }

    /// 行级类别直证:(source, mem_type)。供工具响应面直证域与调用方
    /// 型别判定;行不在缓存=None。
    pub fn fact_class(&self, fact_id: u64) -> Option<(String, String)> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.query_row(
            "SELECT source, mem_type FROM facts WHERE fact_id = ?1 LIMIT 1",
            [fact_id as i64],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .ok()
    }

    /// P2 实体检索原语：按实体名直查候选集。
    pub fn entity_scan(&self, entity: &str, prefix: &str) -> Result<Vec<u64>, LexError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = conn
            .prepare("SELECT fact_id FROM entities WHERE entity = ?1 AND prefix = ?2")
            .map_err(|e| LexError(format!("entity_scan: {e}")))?;
        let rows = stmt
            .query_map(rusqlite::params![entity, prefix], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|e| LexError(format!("entity_scan: {e}")))?;
        Ok(rows.filter_map(|r| r.ok()).map(|i| i as u64).collect())
    }

    /// P3 因果展开原语：沿 causes 邻接表递归展开（带环保护，深度上限 16）。
    pub fn causal_expand(&self, fact_id: u64, max_depth: usize) -> Result<Vec<u64>, LexError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut out = Vec::new();
        let mut frontier = vec![fact_id];
        let mut seen = std::collections::HashSet::new();
        seen.insert(fact_id);
        for _ in 0..max_depth {
            let mut next = Vec::new();
            for fid in &frontier {
                let mut stmt = conn
                    .prepare("SELECT cause_id FROM causes WHERE fact_id = ?1")
                    .map_err(|e| LexError(format!("causal_expand: {e}")))?;
                let rows = stmt
                    .query_map([fid], |row| row.get::<_, i64>(0))
                    .map_err(|e| LexError(format!("causal_expand: {e}")))?;
                for cid in rows {
                    let cid = cid.map_err(|e| LexError(format!("row: {e}")))? as u64;
                    if seen.insert(cid) {
                        out.push(cid);
                        next.push(cid);
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    /// P5 时间线窗口原语：valid_at ∈ [t0, t1] 的候选集。
    pub fn timeline_window(&self, prefix: &str, t0: u64, t1: u64) -> Result<Vec<u64>, LexError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = conn
            .prepare(
                "SELECT fact_id FROM timeline WHERE prefix = ?1 AND valid_at BETWEEN ?2 AND ?3",
            )
            .map_err(|e| LexError(format!("timeline_window: {e}")))?;
        let rows = stmt
            .query_map(rusqlite::params![prefix, t0 as i64, t1 as i64], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|e| LexError(format!("timeline_window: {e}")))?;
        Ok(rows.filter_map(|r| r.ok()).map(|i| i as u64).collect())
    }

    /// P6 生命周期状态过滤原语。
    pub fn state_filter(&self, prefix: &str, state: &str) -> Result<Vec<u64>, LexError> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = conn
            .prepare("SELECT fact_id FROM timeline WHERE prefix = ?1 AND lifecycle_state = ?2")
            .map_err(|e| LexError(format!("state_filter: {e}")))?;
        let rows = stmt
            .query_map(rusqlite::params![prefix, state], |row| row.get::<_, i64>(0))
            .map_err(|e| LexError(format!("state_filter: {e}")))?;
        Ok(rows.filter_map(|r| r.ok()).map(|i| i as u64).collect())
    }

    /// P1 检索原语:goal 词法倒排候选集(按命中 token 数降序,上限 limit)。
    /// 返回 (fact_id, hits)——评分语义在调用方(存储不藏策略,设计档原则 5)。
    pub fn lookup_candidates(
        &self,
        prefix: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(u64, usize)>, LexError> {
        self.lookup_candidates_typed(std::slice::from_ref(&prefix.to_string()), query, limit, &[])
    }

    /// P1 型别过滤扩展:跨前缀族一次查询(候选集统一限上,排序 tie-break
    /// 与单族版逐字节同规则),`mem_types` 过滤行级型别直证列。
    /// 空切片=不过滤(与旧口径逐字节一致);空前缀集=空结果。
    pub fn lookup_candidates_typed(
        &self,
        prefixes: &[String],
        query: &str,
        limit: usize,
        mem_types: &[&str],
    ) -> Result<Vec<(u64, usize)>, LexError> {
        if prefixes.is_empty() {
            return Ok(Vec::new());
        }
        let tokens = crate::agent::memory::tokenize_for_match(query);
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut hits: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();

        // WHERE 动态拼装:prefix IN (...) [AND mem_type IN (...)] AND bigram = ?
        let mut sql = format!(
            "SELECT fact_id FROM postings WHERE prefix IN ({})",
            (1..=prefixes.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut params: Vec<String> = prefixes.to_vec();
        if !mem_types.is_empty() {
            let base = prefixes.len() + 1;
            sql.push_str(&format!(
                " AND mem_type IN ({})",
                (0..mem_types.len())
                    .map(|i| format!("?{}", base + i))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            params.extend(mem_types.iter().map(|s| s.to_string()));
        }
        params.push(String::new()); // bigram 占位(逐 token 替换)
        let bigram_idx = params.len();
        sql.push_str(&format!(" AND bigram = ?{bigram_idx}"));

        for token in &tokens {
            params[bigram_idx - 1] = token.clone();
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| LexError(format!("lookup: {e}")))?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(|e| LexError(format!("lookup: {e}")))?;
            for id in rows {
                *hits
                    .entry(id.map_err(|e| LexError(format!("row: {e}")))?)
                    .or_insert(0) += 1;
            }
        }
        let mut out: Vec<(u64, usize)> = hits.into_iter().map(|(id, n)| (id as u64, n)).collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out.truncate(limit);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("lexstore-test-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn facts() -> Vec<(u64, String, serde_json::Value)> {
        vec![
            (
                1,
                "shared.ns.stable.llm.m.a".to_string(),
                serde_json::json!({"key": "a", "value": "记忆预算裁剪规则说明", "timestamp": 100}),
            ),
            (
                2,
                "shared.ns.stable.llm.m.b".to_string(),
                serde_json::json!({"key": "b", "value": "用户喜欢 Rust", "timestamp": 200}),
            ),
            (
                3,
                "shared.ns.events.e1".to_string(),
                serde_json::json!({"key": "e1", "value": "部署完成", "timestamp": 300}),
            ),
        ]
    }

    #[test]
    fn test_replace_and_cached_roundtrip() {
        let path = temp_db("rt");
        let store = LexStore::open(&path).unwrap();
        store
            .replace_partition("shared.ns.stable.", &facts()[..2])
            .unwrap();
        // TTL 内命中
        let got = store.cached_facts("shared.ns.stable.", 60).unwrap();
        assert_eq!(got.len(), 2);
        // TTL 过期 → None(调用方走全量刷新)
        assert!(store.cached_facts("shared.ns.stable.", 0).is_none());
    }

    #[test]
    fn test_partition_replace_evicts_old() {
        let path = temp_db("evict");
        let store = LexStore::open(&path).unwrap();
        store
            .replace_partition("shared.ns.stable.", &facts()[..2])
            .unwrap();
        // 替换为仅 1 条 → 旧条目随分区淘汰
        store
            .replace_partition("shared.ns.stable.", &facts()[..1])
            .unwrap();
        let got = store.cached_facts("shared.ns.stable.", 60).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].fact_id, 1);
    }

    #[test]
    fn test_p2_p3_p5_p6_primitives() {
        let path = temp_db("prims");
        let store = LexStore::open(&path).unwrap();
        let facts = vec![
            (
                1u64,
                "shared.ns.events.E1".to_string(),
                serde_json::json!({"key": "E1", "value": "部署完成", "timestamp": 1000,
                               "entities": [{"name": "calc.py"}], "cause_fact_id": 99,
                               "lifecycle_state": "Settled"}),
            ),
            (
                2u64,
                "shared.ns.events.E2".to_string(),
                serde_json::json!({"key": "E2", "value": "修复 calc.py bug", "timestamp": 2000,
                               "lifecycle_state": "Captured"}),
            ),
        ];
        store
            .replace_partition("shared.ns.events.", &facts)
            .unwrap();

        // P2 entity_scan
        let hits = store.entity_scan("calc.py", "shared.ns.events.").unwrap();
        assert!(hits.contains(&1));

        // P3 causal_expand
        let chain = store.causal_expand(1, 8).unwrap();
        assert!(chain.contains(&99));

        // P5 timeline_window
        let tl = store
            .timeline_window("shared.ns.events.", 500, 1500)
            .unwrap();
        assert!(tl.contains(&1));
        assert!(!tl.contains(&2)); // ts=2000 超窗

        // P6 state_filter
        let settled = store.state_filter("shared.ns.events.", "Settled").unwrap();
        assert!(settled.contains(&1));
    }

    #[test]
    fn test_lookup_candidates_ranking() {
        let path = temp_db("rank");
        let store = LexStore::open(&path).unwrap();
        store
            .replace_partition("shared.ns.stable.", &facts()[..2])
            .unwrap();
        // goal 命中 fact1 的词更多 → 排前
        let out = store
            .lookup_candidates("shared.ns.stable.", "记忆预算 裁剪 rust", 10)
            .unwrap();
        assert!(!out.is_empty());
        assert_eq!(out[0].0, 1);
        // limit 截断
        let out = store
            .lookup_candidates("shared.ns.stable.", "记忆 预算 rust", 1)
            .unwrap();
        assert_eq!(out.len(), 1);
    }

    // ===== 跨源注册机制批:型别直证列 / 合成 id / typed 倒排 =====

    /// 直读型别列(测试探针)
    fn fact_columns(store: &LexStore, fact_id: u64) -> (String, String) {
        let conn = store.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.query_row(
            "SELECT source, mem_type FROM facts WHERE fact_id = ?1",
            [fact_id as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    #[test]
    fn test_mem_type_columns_derivation_and_override() {
        let path = temp_db("memtype");
        let store = LexStore::open(&path).unwrap();
        // 各前缀族播种(推导路径)
        let stable = vec![(
            1u64,
            "shared.ns.stable.llm.m.a".to_string(),
            serde_json::json!({"key": "a", "value": "语义记忆", "timestamp": 1}),
        )];
        let events = vec![(
            2u64,
            "shared.ns.events.e1".to_string(),
            serde_json::json!({"key": "e1", "value": "情景记忆", "timestamp": 2}),
        )];
        let sessions = vec![(
            3u64,
            "shared.ns.sessions.s1".to_string(),
            serde_json::json!({"key": "s1", "value": "工作记忆", "timestamp": 3}),
        )];
        // 逐行覆盖路径:stable 前缀行声明 procedural
        let overridden = vec![(
            4u64,
            "shared.ns.stable.kc.k1".to_string(),
            serde_json::json!({"key": "k1", "value": "程序碎片", "timestamp": 4, "mem_type": "procedural"}),
        )];
        store
            .replace_partition("shared.ns.stable.", &stable)
            .unwrap();
        store
            .replace_partition("shared.ns.events.", &events)
            .unwrap();
        store
            .replace_partition("shared.ns.sessions.", &sessions)
            .unwrap();

        assert_eq!(
            fact_columns(&store, 1),
            ("ledger".into(), "semantic".into())
        );
        assert_eq!(
            fact_columns(&store, 2),
            ("ledger".into(), "episodic".into())
        );
        assert_eq!(fact_columns(&store, 3), ("ledger".into(), "work".into()));

        // 逐行覆盖压过前缀推导(策略压机制默认)——注意 replace_partition
        // 是整分区替换,覆盖行以单行分区写入(旧行随之淘汰)
        store
            .replace_partition("shared.ns.stable.", &overridden)
            .unwrap();
        assert_eq!(
            fact_columns(&store, 4),
            ("ledger".into(), "procedural".into())
        );
    }

    #[test]
    fn test_synthetic_fact_id_deterministic_and_flagged() {
        let a1 = synthetic_fact_id("local.skills.x#1");
        // trim 规范化:同路径不同留白同 id
        assert_eq!(a1, synthetic_fact_id(" local.skills.x#1 "));
        // 确定性:同路径重复计算同 id
        assert_eq!(a1, synthetic_fact_id("local.skills.x#1"));
        // bit63 标记位必置(与账本正 id 空间不相交)
        assert_eq!(a1 & SYNTHETIC_ID_FLAG, SYNTHETIC_ID_FLAG);
        // 异路径异 id
        assert_ne!(a1, synthetic_fact_id("local.skills.x#2"));
        assert_ne!(synthetic_fact_id("p1"), synthetic_fact_id("p2"));
    }

    #[test]
    fn test_typed_lookup_multifamily_and_filter() {
        let path = temp_db("typed");
        let store = LexStore::open(&path).unwrap();
        let stable = vec![(
            1u64,
            "shared.ns.stable.llm.m.a".to_string(),
            serde_json::json!({"key": "a", "value": "部署完成事项", "timestamp": 1}),
        )];
        let events = vec![(
            2u64,
            "shared.ns.events.e1".to_string(),
            serde_json::json!({"key": "e1", "value": "部署完成事件", "timestamp": 2}),
        )];
        let procedural = vec![(
            3u64,
            "shared.ns.procedural.skills.x".to_string(),
            serde_json::json!({"key": "x", "value": "部署完成手册", "timestamp": 3, "mem_type": "procedural"}),
        )];
        store
            .replace_partition("shared.ns.stable.", &stable)
            .unwrap();
        store
            .replace_partition("shared.ns.events.", &events)
            .unwrap();
        store
            .replace_partition("shared.ns.procedural.", &procedural)
            .unwrap();

        let prefixes = vec![
            "shared.ns.stable.".to_string(),
            "shared.ns.events.".to_string(),
            "shared.ns.procedural.".to_string(),
        ];
        // 无型别过滤:跨族候选集统一(单族旧口径结果 ⊆ 跨族结果)
        let all = store
            .lookup_candidates_typed(&prefixes, "部署", 10, &[])
            .unwrap();
        let ids: Vec<u64> = all.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&1) && ids.contains(&2) && ids.contains(&3));
        // 单族旧口径与 typed 单族无过滤逐字节一致(门禁)
        let legacy = store
            .lookup_candidates("shared.ns.stable.", "部署", 10)
            .unwrap();
        let single = store
            .lookup_candidates_typed(&["shared.ns.stable.".to_string()], "部署", 10, &[])
            .unwrap();
        assert_eq!(legacy, single);
        // 型别过滤:semantic 只剩 fact1
        let sem = store
            .lookup_candidates_typed(&prefixes, "部署", 10, &[mem_type::SEMANTIC])
            .unwrap();
        assert_eq!(sem.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![1]);
        // 型别过滤:procedural 只剩 fact3
        let pro = store
            .lookup_candidates_typed(&prefixes, "部署", 10, &[mem_type::PROCEDURAL])
            .unwrap();
        assert_eq!(pro.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![3]);
        // 空前缀集=空结果
        assert!(store
            .lookup_candidates_typed(&[], "部署", 10, &[])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_legacy_db_migration_adds_type_columns() {
        // 旧 schema(无型别列)建库 → open 幂等迁移补列 → 读写正常
        const LEGACY_SCHEMA: &str = "
        CREATE TABLE IF NOT EXISTS partitions(prefix TEXT PRIMARY KEY, fetched_at INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS facts(
            prefix TEXT NOT NULL, fact_id INTEGER NOT NULL, path TEXT NOT NULL,
            value_json TEXT NOT NULL, PRIMARY KEY(prefix, fact_id));
        CREATE TABLE IF NOT EXISTS postings(
            bigram TEXT NOT NULL, fact_id INTEGER NOT NULL, prefix TEXT NOT NULL,
            PRIMARY KEY(bigram, fact_id, prefix));
        ";
        let path = temp_db("mig");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(LEGACY_SCHEMA).unwrap();
        }
        let store = LexStore::open(&path).unwrap();
        store
            .replace_partition("shared.ns.stable.", &facts()[..1])
            .unwrap();
        // 迁移后写入的行携带直证列默认值
        assert_eq!(
            fact_columns(&store, 1),
            ("ledger".into(), "semantic".into())
        );
        assert_eq!(
            store.cached_facts("shared.ns.stable.", 60).unwrap().len(),
            1
        );
    }
}
