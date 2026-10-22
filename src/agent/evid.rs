// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! EVID 证据投影层——将内部审计链投影为标准化、可离线独立验证的公开证据文档。
//!
//! # 核心理念
//!
//! 故意丢弃密钥。EVID 投影层不含任何密钥——它用无密钥的 content_hash
//! 换取"任何第三方、零依赖、跨语言可验"的公开可验证性。
//!
//! 代价：任何人都能构造一条合法的伪造 EVID 链。所以 EVID 永远不能单独作为
//! "执行确实发生过"的证据，必须与签名锚点联用——
//! 哈希链证明内部自洽，签名锚点证明来源可信，EVID 投影证明公开可验。
//! 三层各司其职，缺一不可。签名由部署侧锚点密钥体系附加，投影器全程无密钥。
//!
//! # 哈希契约（字节级——先定义后实现）
//!
//! ```text
//! evid_hash = blake3( canonical_json( [
//!     string(seq),           // 序号（十进制字符串）
//!     string(step_id),       // 确定性步骤标识
//!     string(fact_type),     // 事实类型名
//!     cause_or_null,         // 因果前驱（JSON null 或数字）
//!     string(previous_evid), // 前条 evid_hash（空串=创世）
//!     string(fact_hash),     // 原始审计链内容哈希
//!     domain_check_or_null   // 域判定（JSON null 或字符串）
//! ]))
//! ```
//!
//! canonical_json = `json.dumps(array, ensure_ascii=False, separators=(",",":"))`
//! ——与跨语言黄金向量（scripts/evid_golden_vectors.json）同编码。
//! 全部字段以字符串入哈希（序号走十进制字符串，杜绝数值编码分裂）。
//! 契约值由 Python 独立推导、Rust 测试逐值断言——双实现任一漂移即红。

use crate::agent::journal::{
    journal_chain_step, journal_content_hash, journal_normalized_content, JOURNAL_CHAIN_GENESIS,
};
use serde::{Deserialize, Serialize};

/// 创世 previous_evid（首条无前驱）
pub const EVID_GENESIS: &str = "";

/// EVID 因果元组——哈希输入的 7 字段（最小完整因果）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidCausal {
    /// 会话内单调序号（从 1 起）
    pub seq: u64,
    /// 确定性步骤标识 `"s{seq}"`
    pub step_id: String,
    /// 事实类型名（Command / IoResponse / …）
    pub fact_type: String,
    /// 因果前驱记录序号（None=根事实）
    pub cause: Option<u64>,
    /// 前条 evid_hash（首条 = 空串 = 创世）
    pub previous_evid: String,
    /// 原始审计链的内容哈希（直接引用，不重算）
    pub fact_hash: String,
    /// 域判定结果（None=非域事实）
    pub domain_check: Option<String>,
}

/// 计算 7 字段的 canonical JSON 字节（哈希输入）
fn canonical_evid_json(c: &EvidCausal) -> String {
    let arr = serde_json::json!([
        c.seq.to_string(),
        c.step_id,
        c.fact_type,
        c.cause,
        c.previous_evid,
        c.fact_hash,
        c.domain_check,
    ]);
    // 对 Value 数组序列化不会失败；空串仅在病态场景出现且必然验证失败
    serde_json::to_string(&arr).unwrap_or_default()
}

/// 计算 evid_hash = blake3(canonical_json)
pub fn evid_hash(c: &EvidCausal) -> String {
    let canonical = canonical_evid_json(c);
    format!("blake3:{}", blake3::hash(canonical.as_bytes()).to_hex())
}

/// EVID 证据文档中的完整记录（因果 + 计算字段 + 展示字段）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvidRecord {
    /// 因果元组（参与哈希的 7 字段）
    #[serde(flatten)]
    pub causal: EvidCausal,
    /// 计算字段：evid_hash = blake3(canonical_json(causal))
    pub evid_hash: String,
    /// 展示字段：Unix 秒（不参与哈希——非确定性来源）
    pub timestamp: u64,
}

/// EVID 证据文档（完整链）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidChain {
    /// 格式版本（当前 "1"）
    pub version: String,
    /// 链标识（导出侧默认 = journal 会话名）
    pub chain_id: String,
    /// 创建时间（ISO 8601，从链尾记录时间戳派生——同输入同输出，不取当前时钟）
    pub created_at: String,
    /// 因果记录序列
    pub records: Vec<EvidRecord>,
    /// 链根 = 末条 evid_hash（空链 = blake3("evid_genesis")）
    pub chain_root: String,
    /// 链长度（记录条数）
    pub chain_length: usize,
    /// 链根签名（可选——由部署侧锚点密钥体系附加；投影器自身无密钥）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<ChainSignature>,
}

/// 链根签名（Ed25519 + 长度防移植——签名同时覆盖链长度，短链签名不可冒充长链）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChainSignature {
    /// 被签名的链根哈希
    pub root_hash: String,
    /// Ed25519 签名（128 hex）
    pub signature: String,
    /// 签名者公钥（64 hex）
    pub public_key: String,
    /// 签名时链长度（防签名移植攻击）
    pub chain_length: usize,
}

impl ChainSignature {
    /// 签名载荷：root_hash + 链长度——覆盖长度防移植
    pub fn signature_payload(&self) -> Vec<u8> {
        format!("{}:{}", self.root_hash, self.chain_length).into_bytes()
    }
}

/// EVID 投影错误面（导出全链路显式化，不静默吞错）
#[derive(Debug, thiserror::Error)]
pub enum EvidError {
    /// journal 行损坏（链三元组校验失败——fail-closed，不产出证据文档）
    #[error("journal corrupted at line {line}: {reason}")]
    Corrupt {
        /// 行号（1 起）
        line: usize,
        /// 失败原因
        reason: String,
    },
    /// journal 无链化记录（全部为存量未哈希行——无法投影）
    #[error("journal has no chained records")]
    NoChainedRecords,
}

/// 创世链根（空链）
pub fn genesis_root() -> String {
    format!("blake3:{}", blake3::hash(b"evid_genesis").to_hex())
}

/// 从因果元组列表投影为 EVID 链（确定性：同输入同输出）
pub fn project(causals: &[EvidCausal], timestamps: &[u64]) -> EvidChain {
    let mut records = Vec::with_capacity(causals.len());
    let mut prev_evid = EVID_GENESIS.to_string();
    for (i, c) in causals.iter().enumerate() {
        let mut rc = c.clone();
        rc.previous_evid = prev_evid.clone();
        let eh = evid_hash(&rc);
        prev_evid = eh.clone();
        records.push(EvidRecord {
            causal: rc,
            evid_hash: eh,
            timestamp: timestamps.get(i).copied().unwrap_or(0),
        });
    }
    let root = if prev_evid.is_empty() {
        genesis_root()
    } else {
        prev_evid
    };
    let created_at = records
        .last()
        .map(|r| iso8601_utc(r.timestamp))
        .unwrap_or_default();
    EvidChain {
        version: "1".into(),
        chain_id: String::new(),
        created_at,
        records,
        chain_root: root,
        chain_length: causals.len(),
        signature: None,
    }
}

/// journal 投影结果（附验证统计——运维诚实性）
#[derive(Debug, Clone)]
pub struct JournalProjection {
    /// 投影出的 EVID 链
    pub chain: EvidChain,
    /// 链化行数（参与投影并通过三元组校验）
    pub chained_lines: usize,
    /// 跳过的存量未哈希行数（无链三元组——不参与投影，如实计数）
    pub skipped_unhashed: usize,
}

/// 从 evo-agent journal 原文投影 EVID 链。
///
/// 逐行校验链三元组（与 journal 验证侧同口径、同 SSOT 函数）：
/// 内容哈希重算 + prev 链接 + 链步重算——任一失配即 fail-closed 返回
/// [`EvidError::Corrupt`]（journal 链已断时不出具证据文档）。
/// 存量未哈希行跳过并计数。`fact_hash` = "blake3:" ++ journal content_hash。
pub fn project_journal(raw: &str, chain_id: &str) -> Result<JournalProjection, EvidError> {
    let mut causals = Vec::new();
    let mut timestamps = Vec::new();
    let mut skipped_unhashed = 0usize;
    let mut running_prev: Option<String> = None;

    for (idx, line) in raw.lines().enumerate() {
        let lineno = idx + 1;
        if line.trim().is_empty() {
            continue;
        }
        let obj: serde_json::Value =
            serde_json::from_str(line).map_err(|e| EvidError::Corrupt {
                line: lineno,
                reason: format!("JSON 解析失败: {e}"),
            })?;

        let content_hash = obj
            .get("content_hash")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let prev_hash = obj
            .get("prev_hash")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let chain_hash = obj
            .get("chain_hash")
            .and_then(|v| v.as_str())
            .map(str::to_owned);

        // 链三元组不完整 = 存量未哈希行（跳过并计数，不参与投影）
        let (Some(content_hash), Some(prev_hash), Some(chain_hash)) =
            (content_hash, prev_hash, chain_hash)
        else {
            skipped_unhashed += 1;
            continue;
        };

        // 三元组三查（与 journal 验证侧同 SSOT——fail-closed）
        let normalized = journal_normalized_content(line).ok_or_else(|| EvidError::Corrupt {
            line: lineno,
            reason: "归一化失败".into(),
        })?;
        let recomputed_content = journal_content_hash(&normalized);
        if recomputed_content != content_hash {
            return Err(EvidError::Corrupt {
                line: lineno,
                reason: format!(
                    "content_hash 失配: 存储 {content_hash:.20}… 重算 {recomputed_content:.20}…"
                ),
            });
        }
        let expect_prev = running_prev
            .clone()
            .unwrap_or_else(|| JOURNAL_CHAIN_GENESIS.to_string());
        if prev_hash != expect_prev {
            return Err(EvidError::Corrupt {
                line: lineno,
                reason: format!(
                    "prev_hash 链接断裂: 存储 {prev_hash:.20}… 期望 {expect_prev:.20}…"
                ),
            });
        }
        let recomputed_chain = journal_chain_step(&prev_hash, &content_hash);
        if recomputed_chain != chain_hash {
            return Err(EvidError::Corrupt {
                line: lineno,
                reason: format!(
                    "chain_hash 链步失配: 存储 {chain_hash:.20}… 重算 {recomputed_chain:.20}…"
                ),
            });
        }
        running_prev = Some(chain_hash);

        // 投影为 EVID 因果元组（EVID 自有编号体系，cause 指向前条投影记录）
        let seq = causals.len() as u64 + 1;
        let fact_type = obj
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();
        let ts_ms = obj.get("ts").and_then(|v| v.as_u64()).unwrap_or(0);
        causals.push(EvidCausal {
            seq,
            step_id: format!("s{seq}"),
            fact_type,
            cause: (seq > 1).then_some(seq - 1),
            previous_evid: String::new(), // project() 内按链序回填
            fact_hash: format!("blake3:{content_hash}"),
            domain_check: None, // journal 行无域判定信息（引擎侧事实才有）
        });
        timestamps.push(ts_ms / 1000);
    }

    if causals.is_empty() {
        return Err(EvidError::NoChainedRecords);
    }
    let chained_lines = causals.len();
    let mut chain = project(&causals, &timestamps);
    chain.chain_id = chain_id.to_string();
    Ok(JournalProjection {
        chain,
        chained_lines,
        skipped_unhashed,
    })
}

/// Unix 秒 → ISO 8601 UTC（秒精度；非法值返回空串）
fn iso8601_utc(secs: u64) -> String {
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn sample_causal(seq: u64, cause: Option<u64>) -> EvidCausal {
        EvidCausal {
            seq,
            step_id: format!("s{seq}"),
            fact_type: "Command".into(),
            cause,
            previous_evid: String::new(),
            fact_hash: format!("blake3:{:064x}", seq),
            domain_check: None,
        }
    }

    #[test]
    fn evid_hash_is_deterministic() {
        let c = sample_causal(1, None);
        assert_eq!(evid_hash(&c), evid_hash(&c), "同输入必同哈希");
    }

    #[test]
    fn evid_hash_differs_for_different_input() {
        let c1 = sample_causal(1, None);
        let mut c2 = sample_causal(1, None);
        c2.fact_hash = "blake3:changed".into();
        assert_ne!(evid_hash(&c1), evid_hash(&c2), "异输入必异哈希");
    }

    #[test]
    fn evid_hash_null_vs_absent_domain_differs() {
        let mut c = sample_causal(1, None);
        c.domain_check = None;
        let h1 = evid_hash(&c);
        c.domain_check = Some("sat".into());
        let h2 = evid_hash(&c);
        assert_ne!(h1, h2, "domain null vs Some 必须不同");
    }

    #[test]
    fn chain_root_empty_is_genesis() {
        assert_eq!(genesis_root().len(), 71); // "blake3:" + 64 hex
    }

    #[test]
    fn project_chain_is_deterministic() {
        let causals: Vec<EvidCausal> = (1..=3).map(|i| sample_causal(i, None)).collect();
        let ts = vec![100u64, 200, 300];
        let c1 = project(&causals, &ts);
        let c2 = project(&causals, &ts);
        assert_eq!(c1.chain_root, c2.chain_root, "同输入同链根");
        assert_eq!(c1.chain_length, 3);
        assert_eq!(c1.records.len(), 3);
        // 链式引用正确
        assert_eq!(c1.records[0].causal.previous_evid, EVID_GENESIS);
        assert_eq!(c1.records[1].causal.previous_evid, c1.records[0].evid_hash);
        assert_eq!(c1.records[2].causal.previous_evid, c1.records[1].evid_hash);
        // 链根 = 末条 evid_hash；created_at 从链尾时间派生
        assert_eq!(c1.chain_root, c1.records[2].evid_hash);
        assert_eq!(c1.created_at, iso8601_utc(300));
    }

    /// 篡改注入：改因果字段 → evid_hash 必变
    #[test]
    fn tamper_causal_field_changes_hash() {
        let mut c = sample_causal(1, Some(0));
        let h1 = evid_hash(&c);
        c.cause = Some(99);
        let h2 = evid_hash(&c);
        assert_ne!(h1, h2);
    }

    /// 跨语言黄金向量：Python 独立推导的期望值，Rust 逐值断言——
    /// 双实现（scripts/evid_verify.py ↔ 本模块）任一漂移即红。
    /// 向量正本 = scripts/evid_golden_vectors.json。
    #[test]
    fn golden_evid_hash_cross_language_locked() {
        let v1 = EvidCausal {
            seq: 1,
            step_id: "s1".into(),
            fact_type: "Command".into(),
            cause: None,
            previous_evid: EVID_GENESIS.to_string(),
            fact_hash: "blake3:56de9ce95d41896edf081f452b4ea5d3ad6508de3a0781a36fa5fbd13c586aa1"
                .into(),
            domain_check: None,
        };
        let h1 = evid_hash(&v1);
        assert_eq!(
            h1, "blake3:b559488746f4e07b9a565be57293bdd498f026175d75aea72149e97073375083",
            "v1 创世记录（null cause / null domain）"
        );

        let v2 = EvidCausal {
            seq: 2,
            step_id: "s2".into(),
            fact_type: "IoResponse".into(),
            cause: Some(1),
            previous_evid: h1.clone(),
            fact_hash: "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
            domain_check: Some("sat".into()),
        };
        let h2 = evid_hash(&v2);
        assert_eq!(
            h2, "blake3:68f99218b5d21220715d7da0e25f52e550633860bde32aa18b7c82d4a089c729",
            "v2 数值 cause / 字符串 domain"
        );

        let v3 = EvidCausal {
            seq: 3,
            step_id: "s3".into(),
            fact_type: "用户消息".into(),
            cause: Some(2),
            previous_evid: h2.clone(),
            fact_hash: "blake3:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .into(),
            domain_check: None,
        };
        let h3 = evid_hash(&v3);
        assert_eq!(
            h3, "blake3:4a72451f16928681e820bc79e6d492b811c894947b651cb78d1ad52f28d9451a",
            "v3 非 ASCII 类型名（UTF-8 原样入哈希，不做 \\u 转义）"
        );

        let v4 = EvidCausal {
            seq: u64::MAX,
            step_id: "s18446744073709551615".into(),
            fact_type: "Command".into(),
            cause: None,
            previous_evid: EVID_GENESIS.to_string(),
            fact_hash: v1.fact_hash.clone(),
            domain_check: None,
        };
        let h4 = evid_hash(&v4);
        assert_eq!(
            h4, "blake3:178ad5c28ce27ac9a28b9b88f0abf39a42370cbfc9c18ba1836318a6b342af80",
            "v4 u64::MAX 序号（十进制字符串无精度损失）"
        );

        let v5 = EvidCausal {
            seq: 7,
            step_id: "s7".into(),
            fact_type: "TransitionTrace".into(),
            cause: Some(6),
            previous_evid: h4.clone(),
            fact_hash: "blake3:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                .into(),
            domain_check: Some("violated".into()),
        };
        assert_eq!(
            evid_hash(&v5),
            "blake3:0a05b6ee1a0b265470a37210a9dc867939689f032b8146e8890e415f3fffe23a",
            "v5 域判定 violated"
        );

        // 创世链根常量锁定
        assert_eq!(
            genesis_root(),
            "blake3:d474d757dfcc53544d8f870dfcd8c74d894a24c24225b9c1a9dc9329fc2c8a34"
        );
    }

    // ── journal 投影（真实写入器产物 → 投影 → 三元组 fail-closed）──

    /// 用 journal 写入器 SSOT 造一条链化行（与生产写入同路径）
    fn chained_line(prev: &str, seq: u64, ts_ms: u64, fact_type: &str) -> String {
        let base =
            format!(r#"{{"seq":{seq},"ts":{ts_ms},"type":"{fact_type}","payload":{{"k":{seq}}}}}"#);
        let normalized = journal_normalized_content(&base).unwrap();
        let content_hash = journal_content_hash(&normalized);
        let chain_hash = journal_chain_step(prev, &content_hash);
        format!(
            r#"{{"chain_hash":"{chain_hash}","content_hash":"{content_hash}","payload":{{"k":{seq}}},"prev_hash":"{prev}","seq":{seq},"ts":{ts_ms},"type":"{fact_type}"}}"#
        )
    }

    #[test]
    fn project_journal_valid_lines() {
        let l1 = chained_line(JOURNAL_CHAIN_GENESIS, 1, 1791633351484, "turn_started");
        let l2 = chained_line(&extract_chain_hash(&l1), 2, 1791633351500, "tool_invoked");
        let raw = format!("{l1}\n{l2}\n");
        let proj = project_journal(&raw, "session-evid-test").unwrap();
        assert_eq!(proj.chained_lines, 2);
        assert_eq!(proj.skipped_unhashed, 0);
        assert_eq!(proj.chain.chain_id, "session-evid-test");
        assert_eq!(proj.chain.chain_length, 2);
        assert_eq!(proj.chain.records[0].causal.cause, None);
        assert_eq!(proj.chain.records[1].causal.cause, Some(1));
        assert_eq!(proj.chain.records[0].causal.fact_type, "turn_started");
        // fact_hash 带 blake3: 前缀引用 journal content_hash
        assert!(proj.chain.records[0]
            .causal
            .fact_hash
            .starts_with("blake3:"));
        // ts 毫秒 → 秒
        assert_eq!(proj.chain.records[0].timestamp, 1791633351);
        // created_at 从链尾时间派生（确定性）
        assert_eq!(proj.chain.created_at, iso8601_utc(1791633351));
    }

    #[test]
    fn project_journal_skips_legacy_unhashed_lines() {
        let l1 = chained_line(JOURNAL_CHAIN_GENESIS, 1, 1000, "turn_started");
        let legacy = r#"{"seq":0,"ts":999,"type":"legacy","payload":{}}"#;
        let l2 = chained_line(&extract_chain_hash(&l1), 2, 2000, "tool_invoked");
        let raw = format!("{legacy}\n{l1}\n{legacy}\n{l2}\n");
        let proj = project_journal(&raw, "s").unwrap();
        assert_eq!(proj.chained_lines, 2);
        assert_eq!(proj.skipped_unhashed, 2, "存量未哈希行跳过并计数");
        assert_eq!(proj.chain.records[0].causal.seq, 1);
        assert_eq!(
            proj.chain.records[1].causal.seq, 2,
            "EVID 自有编号不受 journal 序号影响"
        );
    }

    #[test]
    fn project_journal_tampered_content_fails_closed() {
        let l1 = chained_line(JOURNAL_CHAIN_GENESIS, 1, 1000, "turn_started");
        // 篡改 payload 内容（content_hash 不再匹配）
        let tampered = l1.replace("\"k\":1", "\"k\":999");
        assert_ne!(l1, tampered);
        let err = project_journal(&tampered, "s").unwrap_err();
        assert!(
            err.to_string().contains("content_hash"),
            "内容失配 fail-closed: {err}"
        );
    }

    #[test]
    fn project_journal_broken_prev_link_fails_closed() {
        let l1 = chained_line(JOURNAL_CHAIN_GENESIS, 1, 1000, "turn_started");
        // 伪造 prev（链接断裂）
        let l2 = chained_line("deadbeef", 2, 2000, "tool_invoked");
        let raw = format!("{l1}\n{l2}\n");
        let err = project_journal(&raw, "s").unwrap_err();
        assert!(
            err.to_string().contains("prev_hash"),
            "链接断裂 fail-closed: {err}"
        );
    }

    #[test]
    fn project_journal_no_chained_records_is_error() {
        let raw = "{\"seq\":1,\"ts\":1,\"type\":\"legacy\",\"payload\":{}}\n";
        let err = project_journal(raw, "s").unwrap_err();
        assert!(matches!(err, EvidError::NoChainedRecords));
    }

    fn extract_chain_hash(line: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        v.get("chain_hash")
            .and_then(|x| x.as_str())
            .unwrap()
            .to_string()
    }

    /// 端到端（lib 级）：真实 JournalWriter 写链化 journal → 文件 → 投影。
    /// 覆盖生产写入路径（encode_line_chained）与投影校验的同源性。
    #[test]
    fn project_journal_from_real_writer() {
        use crate::agent::journal::JournalWriter;
        let dir = std::env::temp_dir();
        let sid = "evid-e2e-writer-test";
        let path = JournalWriter::path_for(&dir, sid);
        let _ = std::fs::remove_file(&path); // 干净起点（历史遗留不干扰）

        let writer = JournalWriter::open(&dir, sid).unwrap();
        let _guard = writer.begin_turn("验证 EVID 投影端到端").unwrap();
        writer.end_turn("completed", 2, 150).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let proj = project_journal(&raw, sid).unwrap();
        assert!(
            proj.chained_lines >= 2,
            "至少 turn_started + turn_ended 两行"
        );
        assert_eq!(proj.skipped_unhashed, 0);

        let recs = &proj.chain.records;
        assert_eq!(recs[0].causal.cause, None);
        for (i, r) in recs.iter().enumerate().skip(1) {
            assert_eq!(r.causal.cause, Some(i as u64), "EVID 因果链");
            assert_eq!(r.causal.previous_evid, recs[i - 1].evid_hash);
        }
        assert_eq!(proj.chain.chain_root, recs.last().unwrap().evid_hash);
        let _ = std::fs::remove_file(&path);
    }
}
