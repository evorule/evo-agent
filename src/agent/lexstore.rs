//! 长程设计档阶段 1(F-618)LexStore——检索优先的本地索引缓存(存储设计档 §四)。
//!
//! 定位:**索引皆视图**——本 store 是 evorule 共享账本的派生缓存,不是第二
//! 真相源(RL-B1);payload 以可丢弃缓存形态存储(设计档允许),损坏/
//! 落后的代价只是性能与新鲜度,调用方回退全量拉取路径(I14 降级兜底)。
//!
//! v0 范围:postings(bigram 倒排)+ facts(分区事实缓存)+ partitions
//! (分区新鲜度)。实体表/因果表随阶段推进补齐(设计档 §4.2)。
//!
//! 新鲜度协议:`cached_facts(prefix, ttl)` 在 TTL 内返回缓存(零网络),
//! 过期返回 None——调用方全量拉取后 `replace_partition` 整分区替换
//! (确定性,分区内旧事实随之淘汰)。TTL 窗口内的跨代理写不可见,
//! 属 v0 已声明的边界(设计档开放问题二)。

use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

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
    PRIMARY KEY(prefix, fact_id)
);
CREATE TABLE IF NOT EXISTS postings(
    bigram TEXT NOT NULL,
    fact_id INTEGER NOT NULL,
    prefix TEXT NOT NULL,
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
            tx.execute(
                "INSERT INTO facts(prefix, fact_id, path, value_json) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![prefix, fact_id, path, value_json],
            )
            .map_err(|e| LexError(format!("insert fact: {e}")))?;
            let text = format!("{path} {value}");
            for token in crate::agent::memory::tokenize_for_match(&text) {
                tx.execute(
                    "INSERT OR IGNORE INTO postings(bigram, fact_id, prefix) VALUES (?1, ?2, ?3)",
                    rusqlite::params![token, fact_id, prefix],
                )
                .map_err(|e| LexError(format!("insert posting: {e}")))?;
            }
            // F-609 落标 + P5 时间线 + P2 实体 + P3 因果：抽取器管线
            // （从 value JSON 确定性提取，零 LLM——12 号 §四抽取器=机制）
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
        let tokens = crate::agent::memory::tokenize_for_match(query);
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut hits: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
        for token in &tokens {
            let mut stmt = conn
                .prepare("SELECT fact_id FROM postings WHERE prefix = ?1 AND bigram = ?2")
                .map_err(|e| LexError(format!("lookup: {e}")))?;
            let rows = stmt
                .query_map(rusqlite::params![prefix, token], |row| row.get::<_, i64>(0))
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
}
