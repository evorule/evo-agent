// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! skills 全动态装配 —— 目录扫描器 + 注册账本 + 两源合并（skill_store）
//!
//! ## 装配通路（C 形态；B2 声明式通路的增量扩展）
//!
//! ```text
//! SKILL.md 放入 skill 目录（用户级 data/skills/ 或项目级 <沙箱根>/.evo/skills/）
//!       │ 会话创建时目录扫描（复用 B2 frontmatter 解析 + 校验）
//!       ▼
//!   发现项 discovered ──注册审批（REST：正文预览 + 泄露检查 + 人工批准，
//!                          blake3 钉哈希）──► active
//!       │                                                │
//!       │ definition.skills 显式声明（B2 语义不变 = 直接生效）│
//!       ▼                                                ▼
//!       └──────► 两源合并（声明优先，冲突 warn）──► manifest 注入 + read_skill 注册
//! ```
//!
//! ## 三态模型（与 G8 工具审批三层同构对齐；通路独立——注册审批是定义期
//! 供应链准入，不走会话期 ApprovalCallback）
//!
//! - `discovered`：扫到未审，不进 manifest / read_skill
//! - `active`：批准生效（批准时刻以 evorule-hash 钉死 blake3 哈希）
//! - `revoked`：撤销留痕（账本保留历史行便于审计回看）
//!
//! ## 账本语义
//!
//! registry.json 只记**有过管理动作**的 skill（批准/撤销/失配降级）；
//! 「扫到从未审」= 计算态 discovered（账本无行），由管理面查询时合成，
//! 会话创建路径对账本除失配降级外只读。
//!
//! ## 哈希纪律（生态约束 B 族）
//!
//! 禁自写 blake3，一律 `evorule-hash`（`digest` 原语 + `prefixed` 存储包装）。
//! 批准时算哈希落账；每次会话创建扫描对 active 项重算——**失配 = 自动降级
//! discovered + warn**（防「批准后偷换内容」，聚焦信号保真）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::definition::{parse_skill_frontmatter, SkillEntry, SkillManifestEntry};

/// skill 来源级（两级目录；同名冲突优先级：definition 声明 > 项目级 > 用户级）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillLevel {
    /// 用户级：`data/skills/`（serve cwd 相对，跨 agent 全局复用，运行时资产）
    User,
    /// 项目级：`<沙箱根>/.evo/skills/`（随项目走可版本化，对齐 `.evo/` 工作区约定）
    Project,
}

impl SkillLevel {
    /// 账本/REST 面字符串形态
    pub fn as_str(&self) -> &'static str {
        match self {
            SkillLevel::User => "user",
            SkillLevel::Project => "project",
        }
    }
}

/// skill 注册三态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillStatus {
    /// 扫到未审（不进 manifest / read_skill）
    Discovered,
    /// 批准生效（哈希钉死）
    Active,
    /// 撤销留痕（下会话移除 manifest）
    Revoked,
}

impl SkillStatus {
    /// 账本/REST 面字符串形态
    pub fn as_str(&self) -> &'static str {
        match self {
            SkillStatus::Discovered => "discovered",
            SkillStatus::Active => "active",
            SkillStatus::Revoked => "revoked",
        }
    }
}

/// 注册账本条目（registry.json 的 skills[] 元素）
///
/// `content_hash` = `blake3:<64hex>`（evorule-hash `prefixed` 形态，存储字段
/// 自描述前缀；B 族纪律：哈希一律 evorule-hash，禁自写 blake3）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    /// 技能名（= skill 子目录名；同 level 内唯一）
    pub name: String,
    /// SKILL.md 绝对路径
    pub path: PathBuf,
    /// 来源级
    pub level: SkillLevel,
    /// 三态
    pub status: SkillStatus,
    /// 批准时钉死的 blake3 哈希（prefixed；失配降级行保留旧值供审计回看）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// 批准时刻（unix secs；本地单机语义，与项目内 journal/session_index 口径一致）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<u64>,
    /// 批准者（v1 无操作者身份体系，缺省 `local-operator`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
    /// 批准时刻的环境指纹（O-638② 新鲜度面；None=未采集/旧账本条目——
    /// 无戳不提示，首次重批后自然获得）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_fp: Option<String>,
    /// 正文 frontmatter `env-sensitive: true` 声明（记录环境事实的技能才做
    /// 环境失配提示；缺省 false=零噪音）
    #[serde(default)]
    pub env_sensitive: bool,
}

/// registry.json 顶层形态
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RegistryFile {
    skills: Vec<RegistryEntry>,
}

// =============================================================================
// 期刊层（账本纵深加固条目）：registry.chain.jsonl = 唯一事实源，registry.json = 物化快照
// =============================================================================

/// 期刊文件名（与快照同目录）
pub const JOURNAL_FILE: &str = "registry.chain.jsonl";

/// 创世记录的前向哈希占位（链头无前驱）
const JOURNAL_GENESIS_PREV: &str = "genesis";

/// 账本动作（期刊基本单元；全部幂等——任意前缀重放收敛到同一状态，
/// 这是「期刊先行 + 崩溃后按期刊重放自愈」正确性的前提）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum JournalAction {
    /// 引导：期刊首次物化时锚定当时全量状态（迁移记录；此后动作逐条入刊）
    Bootstrap {
        /// 引导时锚定的全量技能行集（此后快照以此行集为起点重放）
        entries: Vec<RegistryEntry>,
    },
    /// 批准（幂等 upsert，载荷=落账后的完整行——重放所需全部结果字段，
    /// 缺一字段即重放态≠快照=永久漂移假阳性）
    Approve {
        /// 批准落账后的完整技能行（含全部结果字段，重放直接整行替换）
        entry: RegistryEntry,
    },
    /// 撤销
    Revoke {
        /// 被撤销的技能名
        name: String,
        /// 被撤销的技能级
        level: SkillLevel,
    },
    /// 失配降级
    Downgrade {
        /// 被降级的技能名
        name: String,
        /// 降级后落入的技能级
        level: SkillLevel,
    },
    /// 引导期锚（B-3 v2）：本引导首次写期刊时落——携带本引导公钥指纹,
    /// 锚之后的所有记录必须可验签（私钥只在本引导壳/serve 进程内存）；
    /// 重放对本变体无状态效果
    SessionStart {
        /// 本引导公钥指纹（ed25519，锚后记录验签的信任根）
        pk_fp: String,
    },
}

/// 期刊记录：hash = prefixed(blake3(prev_hash || canonical(record)))——链式绑定；
/// `sig` = 引导私钥对 hash 字节串的 ed25519 签名（B-3 v2,签名模式启用时落——
/// hash 绑载荷与链位,签名绑 hash = 传递性覆盖全记录）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalRecord {
    /// 单调递增记录序号（重放顺序与链位对账依据）
    pub seq: u64,
    /// 前驱记录哈希（链头无前驱时为创世占位）
    pub prev_hash: String,
    /// 账本动作载荷（flatten 进记录顶层）
    #[serde(flatten)]
    pub action: JournalAction,
    /// 本记录链哈希（prefixed(blake3(prev_hash || canonical(record)))）
    pub hash: String,
    /// 引导私钥对 hash 的 ed25519 签名（签名模式启用时落）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

/// 记录哈希（canonical = serde_json 固定键序;B 族纪律禁自写 blake3）
fn journal_record_hash(seq: u64, prev_hash: &str, action: &JournalAction) -> String {
    let body = serde_json::json!({ "seq": seq, "prev_hash": prev_hash, "action": action });
    evorule_hash::prefixed(&evorule_hash::digest(body.to_string().as_bytes()))
}

/// 重放单条动作到条目集
fn apply_action(entries: &mut Vec<RegistryEntry>, action: &JournalAction) {
    match action {
        JournalAction::Bootstrap { entries: e } => *entries = e.clone(),
        JournalAction::Approve { entry } => {
            if let Some(x) = entries
                .iter_mut()
                .find(|x| x.name == entry.name && x.level == entry.level)
            {
                *x = entry.clone();
            } else {
                entries.push(entry.clone());
            }
        }
        JournalAction::Revoke { name, level } => {
            if let Some(x) = entries
                .iter_mut()
                .find(|x| x.name == *name && x.level == *level)
            {
                x.status = SkillStatus::Revoked;
            }
        }
        JournalAction::Downgrade { name, level } => {
            if let Some(x) = entries
                .iter_mut()
                .find(|x| x.name == *name && x.level == *level)
            {
                x.status = SkillStatus::Discovered;
            }
        }
        JournalAction::SessionStart { .. } => {}
    }
}

/// 期刊行追加（append + fsync——期刊先行的持久化保证）
fn append_journal_line(path: &Path, rec: &mut JournalRecord) -> Result<(), String> {
    use std::io::Write as _;
    if let Some(sig) = crate::agent::journal_signing::sign_hash(&rec.hash) {
        rec.sig = Some(sig);
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("skill journal '{}' append failed: {}", path.display(), e))?;
    let line =
        serde_json::to_string(rec).map_err(|e| format!("skill journal serialize failed: {}", e))?;
    writeln!(f, "{line}").map_err(|e| format!("skill journal write failed: {}", e))?;
    f.sync_all()
        .map_err(|e| format!("skill journal sync failed: {}", e))?;
    Ok(())
}

fn journal_path_for(snapshot: &Path) -> PathBuf {
    snapshot
        .parent()
        .map(|d| d.join(JOURNAL_FILE))
        .unwrap_or_else(|| PathBuf::from(JOURNAL_FILE))
}

/// 期刊与快照的条目集等价（按 (name, level) 键对齐后全字段比较——排序无关）
fn entries_equal_sorted(a: &[RegistryEntry], b: &[RegistryEntry]) -> bool {
    let mut sa = a.to_vec();
    let mut sb = b.to_vec();
    let key = |v: &RegistryEntry| {
        (
            v.name.clone(),
            v.level.as_str().to_string(),
            v.path.to_string_lossy().to_string(),
        )
    };
    sa.sort_by_key(key);
    sb.sort_by_key(key);
    // RegistryEntry 无 PartialEq 派生——逐字段比（路径用 to_string_lossy 归一）
    sa.len() == sb.len()
        && sa.iter().zip(sb.iter()).all(|(x, y)| {
            x.name == y.name
                && x.level == y.level
                && x.status == y.status
                && x.path.to_string_lossy() == y.path.to_string_lossy()
                && x.content_hash == y.content_hash
                && x.approved_at == y.approved_at
                && x.approved_by == y.approved_by
                && x.env_fp == y.env_fp
                && x.env_sensitive == y.env_sensitive
        })
}

/// 期刊中是否存在「本引导公钥」的 session_start 锚（只读探测——签名模式
/// 下无锚 = 首次写前需补锚;有锚 = 锚后记录全部验签）
fn journal_has_own_anchor(journal_path: &Path) -> bool {
    let Some(fp) = crate::agent::journal_signing::pk_fp() else {
        return false;
    };
    let Ok(content) = std::fs::read_to_string(journal_path) else {
        return false;
    };
    content.lines().any(|l| {
        serde_json::from_str::<serde_json::Value>(l)
            .ok()
            .and_then(|v| {
                v.get("action")
                    .and_then(|a| a.get("SessionStart"))
                    .and_then(|s| s.get("pk_fp"))
                    .and_then(|f| f.as_str())
                    .map(|f| f == fp)
            })
            .unwrap_or(false)
    })
}

/// 注册账本（`<skills 根>/registry.json` = 物化快照；`registry.chain.jsonl` = 期刊真源）
///
/// 并发口径：serve 单进程、审批低频；期刊追加与快照重写置于同一互斥段
/// （save 内），进程级串行足够（设计档 §4.2）。
///
/// 纵深语义（账本纵深加固条目）：旁路手改快照在 load 被检测（drift 标记）并在下一次
/// save 自愈覆写——手改产物失效且留痕；期刊中段篡改 = fail-visible 硬错。
/// 诚实天花板：同权限攻击者整体一致重写「期刊+快照」不可检（无签名），
/// 根治 = 壳侧签名（设计稿 §1.4 v2）。
#[derive(Debug, Clone)]
pub struct SkillRegistry {
    path: PathBuf,
    journal_path: PathBuf,
    entries: Vec<RegistryEntry>,
    /// 期刊尾 (seq, hash)；None = 期刊未物化（首次动作时引导）
    journal_tail: Option<(u64, String)>,
    /// 待入刊动作（save 时 flush：期刊先行，快照随后）
    pending: Vec<JournalAction>,
    /// 快照与期刊重放态漂移（旁路手改证据；load 只标记，自愈在写面 save）
    drift: bool,
    /// 签名模式启用且期刊中无本引导锚 → 首次 save 前需落 session_start
    /// （load 只标记——读面不写盘;锚由写面补齐）
    needs_session_start: bool,
}

impl SkillRegistry {
    /// 加载账本。期刊在 = 校验链+重放为真源（快照漂移只标记,自愈在写面
    /// save）；期刊缺 = 快照为真（首个动作时引导物化期刊）；两者皆缺 =
    /// 空账本。损坏 = Err fail-visible（门禁不可静默降级，H 族）。
    pub fn load(path: &Path) -> Result<Self, String> {
        let journal_path = journal_path_for(path);
        let journal_live = journal_path.exists()
            && std::fs::metadata(&journal_path)
                .map(|m| m.len() > 0)
                .unwrap_or(false);
        if journal_live {
            let (entries, tail) = Self::load_journal(&journal_path)?;
            let needs_session_start =
                crate::agent::journal_signing::enabled() && !journal_has_own_anchor(&journal_path);
            let mut reg = Self {
                path: path.to_path_buf(),
                journal_path,
                entries: entries.clone(),
                journal_tail: Some(tail),
                pending: Vec::new(),
                drift: false,
                needs_session_start,
            };
            // 快照漂移检测（只标记——GET 只读;自愈在写面 save 覆写快照）
            if path.exists() {
                let snap = std::fs::read_to_string(path).map_err(|e| {
                    format!("skill registry '{}' unreadable: {}", path.display(), e)
                })?;
                let file: RegistryFile = serde_json::from_str(&snap).map_err(|e| {
                    format!(
                        "skill registry '{}' corrupt (fix or delete the file): {}",
                        path.display(),
                        e
                    )
                })?;
                if !entries_equal_sorted(&file.skills, &entries) {
                    reg.drift = true;
                    tracing::warn!(
                        "skill registry snapshot drifted from journal;                          heal on next save (out-of-band edit detected)"
                    );
                }
            }
            Ok(reg)
        } else if path.exists() {
            // 旧版升级/无期刊态：快照为真；首个动作时引导物化期刊
            let content = std::fs::read_to_string(path)
                .map_err(|e| format!("skill registry '{}' unreadable: {}", path.display(), e))?;
            let file: RegistryFile = serde_json::from_str(&content).map_err(|e| {
                format!(
                    "skill registry '{}' corrupt (fix or delete the file): {}",
                    path.display(),
                    e
                )
            })?;
            Ok(Self {
                path: path.to_path_buf(),
                journal_path,
                entries: file.skills,
                journal_tail: None,
                pending: Vec::new(),
                drift: false,
                needs_session_start: crate::agent::journal_signing::enabled(),
            })
        } else {
            Ok(Self {
                path: path.to_path_buf(),
                journal_path,
                entries: Vec::new(),
                journal_tail: None,
                pending: Vec::new(),
                drift: false,
                needs_session_start: crate::agent::journal_signing::enabled(),
            })
        }
    }

    /// 期刊校验 + 重放：seq 连续、前向链绑定、逐记录哈希核对；
    /// 末行解析失败 = 追加撕裂（崩溃伪影）截断恢复；中段损坏 = Err（篡改特征）
    fn load_journal(journal_path: &Path) -> Result<(Vec<RegistryEntry>, (u64, String)), String> {
        let content = std::fs::read_to_string(journal_path).map_err(|e| {
            format!(
                "skill journal '{}' unreadable: {}",
                journal_path.display(),
                e
            )
        })?;
        let total = content.lines().count();
        let mut records: Vec<JournalRecord> = Vec::new();
        for (idx, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<JournalRecord>(line) {
                Ok(r) => records.push(r),
                Err(e) => {
                    if idx + 1 == total {
                        tracing::warn!(
                            "skill journal '{}' tail torn (crash during append); truncated",
                            journal_path.display()
                        );
                        break;
                    }
                    return Err(format!(
                        "skill journal '{}' corrupt at line {} (mid-journal damage): {}",
                        journal_path.display(),
                        idx + 1,
                        e
                    ));
                }
            }
        }
        let mut prev = JOURNAL_GENESIS_PREV.to_string();
        let mut entries: Vec<RegistryEntry> = Vec::new();
        for (want, r) in records.iter().enumerate() {
            let want = want as u64;
            if r.seq != want {
                return Err(format!(
                    "skill journal '{}' seq discontinuity at {} (expected {}, got {})",
                    journal_path.display(),
                    r.seq,
                    want,
                    r.seq
                ));
            }
            if r.prev_hash != prev {
                return Err(format!(
                    "skill journal '{}' chain broken at seq {} (prev_hash mismatch)",
                    journal_path.display(),
                    r.seq
                ));
            }
            let expect = journal_record_hash(r.seq, &r.prev_hash, &r.action);
            if expect != r.hash {
                return Err(format!(
                    "skill journal '{}' record hash mismatch at seq {} (tampered?)",
                    journal_path.display(),
                    r.seq
                ));
            }
            apply_action(&mut entries, &r.action);
            prev = r.hash.clone();
        }
        // B-3 v2 验签执法：锚后全签名（密钥在位时）。锚 = 最后一枚「本引导
        // 公钥」的 session_start；锚后记录无签名/验签失败 = fail-visible
        // （私钥只在本引导壳/serve 进程内存,文件系统攻击者造不出有效签名）。
        // 无锚（pre-B3 期刊或首写前）= 历史走哈希链,锚由写面 save 补齐。
        if crate::agent::journal_signing::enabled() {
            let fp = crate::agent::journal_signing::pk_fp();
            let anchor_idx = records.iter().rposition(|r| {
                matches!(&r.action, JournalAction::SessionStart { pk_fp } if Some(pk_fp.as_str()) == fp.as_deref())
            });
            if let Some(ai) = anchor_idx {
                let vk = crate::agent::journal_signing::verifying_key_matches_fp(
                    fp.as_deref().unwrap_or_default(),
                )
                .ok_or_else(|| {
                    format!(
                        "skill journal '{}' anchor key mismatch (journal signed by another boot)",
                        journal_path.display()
                    )
                })?;
                for r in &records[ai + 1..] {
                    let Some(sig) = &r.sig else {
                        return Err(format!(
                            "skill journal '{}' record seq {} missing signature (signed mode)",
                            journal_path.display(),
                            r.seq
                        ));
                    };
                    if !crate::agent::journal_signing::verify_hash(&vk, &r.hash, sig) {
                        return Err(format!(
                            "skill journal '{}' record seq {} signature invalid",
                            journal_path.display(),
                            r.seq
                        ));
                    }
                }
            }
        }
        let tail = records
            .last()
            .map(|r| (r.seq, r.hash.clone()))
            .unwrap_or((0, JOURNAL_GENESIS_PREV.to_string()));
        Ok((entries, tail))
    }

    /// 快照与期刊是否存在漂移（旁路手改证据；save 后清除）
    pub fn journal_drift(&self) -> bool {
        self.drift
    }

    /// 持久化：待刷动作先入期刊（fsync），随后重写快照（物化缓存）。
    /// 崩溃窗：期刊已落、快照未落 → 下次 load 按期刊重放自愈（动作保留）；
    /// 反向顺序会丢已批动作，故顺序不可颠倒。无待刷动作 = 仅重写快照
    /// （漂移自愈位）。原子写（临时文件 + rename；Windows rename 可覆盖目标）。
    pub fn save(&mut self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "skill registry dir '{}' unwritable: {}",
                    parent.display(),
                    e
                )
            })?;
        }
        // 锚先行（签名模式）：本引导首个期刊记录必须是 session_start——
        // 「锚后全签名」语义的起点;锚写失败 = 不落任何动作,fail-visible。
        // 无密钥（未启用签名）= 无锚,行为与 v1 一致。
        let materializing = self.journal_tail.is_none();
        if self.needs_session_start {
            if let Some(pk_fp) = crate::agent::journal_signing::pk_fp() {
                let (seq, prev) = match self.journal_tail.clone() {
                    Some((s, h)) => (s + 1, h),
                    None => (0, JOURNAL_GENESIS_PREV.to_string()),
                };
                let action = JournalAction::SessionStart { pk_fp };
                let hash = journal_record_hash(seq, &prev, &action);
                let mut rec = JournalRecord {
                    seq,
                    prev_hash: prev.clone(),
                    action,
                    hash: hash.clone(),
                    sig: None,
                };
                append_journal_line(&self.journal_path, &mut rec)?;
                self.journal_tail = Some((seq, hash));
            }
            self.needs_session_start = false;
        }
        if !self.pending.is_empty() {
            if materializing {
                // 首次物化：引导记录锚定当前全量态（已含待刷动作的效果），
                // 待刷清单随之消费——个体动作史让位于迁移记录（设计稿 §1.2）
                let action = JournalAction::Bootstrap {
                    entries: self.entries.clone(),
                };
                let (seq, prev) = match self.journal_tail.clone() {
                    Some((s, h)) => (s + 1, h),
                    None => (0, JOURNAL_GENESIS_PREV.to_string()),
                };
                let hash = journal_record_hash(seq, &prev, &action);
                let mut rec = JournalRecord {
                    seq,
                    prev_hash: prev.clone(),
                    action,
                    hash: hash.clone(),
                    sig: None,
                };
                append_journal_line(&self.journal_path, &mut rec)?;
                self.journal_tail = Some((seq, hash));
                self.pending.clear();
            } else {
                for action in self.pending.drain(..) {
                    let (seq, prev) = self.journal_tail.clone().unwrap();
                    let hash = journal_record_hash(seq + 1, &prev, &action);
                    let mut rec = JournalRecord {
                        seq: seq + 1,
                        prev_hash: prev.clone(),
                        action: action.clone(),
                        hash: hash.clone(),
                        sig: None,
                    };
                    append_journal_line(&self.journal_path, &mut rec)?;
                    self.journal_tail = Some((seq + 1, hash));
                }
            }
        }
        let json = serde_json::to_string_pretty(&RegistryFile {
            skills: self.entries.clone(),
        })
        .map_err(|e| format!("skill registry serialize failed: {}", e))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)
            .map_err(|e| format!("skill registry tmp write failed: {}", e))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| format!("skill registry rename failed: {}", e))?;
        self.drift = false;
        Ok(())
    }

    /// 查账本行（键 = name + level；同名跨 level 是合法冲突场景，由合并优先级裁决）
    pub fn find(&self, name: &str, level: SkillLevel) -> Option<&RegistryEntry> {
        self.entries
            .iter()
            .find(|e| e.name == name && e.level == level)
    }

    /// 可变查账本行
    pub fn find_mut(&mut self, name: &str, level: SkillLevel) -> Option<&mut RegistryEntry> {
        self.entries
            .iter_mut()
            .find(|e| e.name == name && e.level == level)
    }

    /// 批准：upsert 为 active + 哈希落账（幂等——重复 approve 重算哈希更新时间戳，
    /// 内容更新后重新走审批即此路径；revoked 项 re-approve = 重新激活）
    #[allow(clippy::too_many_arguments)]
    pub fn approve(
        &mut self,
        name: &str,
        path: PathBuf,
        level: SkillLevel,
        content_hash: String,
        approved_by: &str,
        env_fp: Option<String>,
        env_sensitive: bool,
    ) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Some(e) = self.find_mut(name, level) {
            e.path = path;
            e.status = SkillStatus::Active;
            e.content_hash = Some(content_hash);
            e.approved_at = Some(now);
            e.approved_by = Some(approved_by.to_string());
            e.env_fp = env_fp;
            e.env_sensitive = env_sensitive;
            let entry = e.clone();
            self.pending.push(JournalAction::Approve { entry });
            return;
        }
        self.entries.push(RegistryEntry {
            name: name.to_string(),
            path,
            level,
            status: SkillStatus::Active,
            content_hash: Some(content_hash),
            approved_at: Some(now),
            approved_by: Some(approved_by.to_string()),
            env_fp,
            env_sensitive,
        });
        let entry = self
            .find(name, level)
            .cloned()
            .expect("entry just upserted must exist");
        self.pending.push(JournalAction::Approve { entry });
    }

    /// 撤销：置 revoked（保留历史行便于审计回看）；无行时 no-op
    /// （撤销一个账本没记过的 skill = 撤销一个从未生效的 skill，状态无变化）
    pub fn revoke(&mut self, name: &str, level: SkillLevel) {
        if let Some(e) = self.find_mut(name, level) {
            e.status = SkillStatus::Revoked;
            self.pending.push(JournalAction::Revoke {
                name: name.to_string(),
                level,
            });
        }
    }

    /// 失配降级：active → discovered（保留旧哈希供审计回看「批准时的内容已变」）
    fn downgrade(&mut self, name: &str, level: SkillLevel) {
        if self.find(name, level).is_some() {
            if let Some(e) = self.find_mut(name, level) {
                e.status = SkillStatus::Discovered;
            }
            self.pending.push(JournalAction::Downgrade {
                name: name.to_string(),
                level,
            });
        }
    }

    /// 账本全行（管理面遍历用；只读视图）
    pub fn entries(&self) -> &[RegistryEntry] {
        &self.entries
    }
}

/// 目录扫描发现的 skill 候选（未过账本核对）
#[derive(Debug, Clone, PartialEq)]
pub struct ScannedSkill {
    /// 技能名（= skill 子目录名）
    pub name: String,
    /// SKILL.md 绝对路径（`<dir>/<name>/SKILL.md`，仅子目录式——市面惯例一致）
    pub path: PathBuf,
    /// 来源级
    pub level: SkillLevel,
    /// frontmatter description（缺失 = 空串 + warn，与 B2 resolve 同口径）
    pub description: String,
}

/// 扫描单级 skill 目录：`<dir>/<skill-name>/SKILL.md`（仅子目录式）
///
/// - 目录不存在 → `Ok(vec![])`（首次运行常态，不是错误）
/// - 目录不可读（存在但读失败/非目录）→ `Err` fail-fast（H 族：门禁不可静默
///   降级——目录级失败 = 会话创建失败，不跳过）
/// - 单 skill 不合格（无 SKILL.md / frontmatter 不可解析 / 路径非常规文件）→
///   跳过 + warn 明示原因，不进清单（fail-visible 于日志，不炸整个目录）
pub fn scan_skills_dir(dir: &Path, level: SkillLevel) -> Result<Vec<ScannedSkill>, String> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let read = std::fs::read_dir(dir).map_err(|e| {
        format!(
            "skill directory '{}' unreadable (fail-fast): {}",
            dir.display(),
            e
        )
    })?;
    let mut out = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| {
            format!(
                "skill directory '{}' entry unreadable (fail-fast): {}",
                dir.display(),
                e
            )
        })?;
        let sub = entry.path();
        if !sub.is_dir() {
            continue; // 散置文件（如 registry.json 自身）不是 skill
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let skill_md = sub.join("SKILL.md");
        // 复用 B2 声明校验通路：路径存在/为文件/frontmatter 可解析 + description 提取
        // 单条隔离调用——单 skill 失败只判该 skill 不合格
        let candidates = [SkillEntry {
            name: name.clone(),
            path: skill_md.clone(),
        }];
        match crate::agent::definition::resolve_skill_manifest_entries(&candidates) {
            Ok(resolved) => {
                // 防钓鱼名一致性（与 import 通路同判据）：frontmatter 声明名
                // 与目录名不一致 = 候选不合格（目录名是注册/approve 的键,声明名
                // 是 read_skill 的门面,两者打架即钓鱼形态）
                let fm_name = parse_frontmatter_name(&skill_md);
                if let Some(declared) = &fm_name {
                    if declared != &name {
                        tracing::warn!(
                            skill = %name,
                            declared = %declared,
                            dir = %dir.display(),
                            "skill scan: frontmatter name mismatches directory name; \
                             candidate rejected (anti-phishing, same check as import)"
                        );
                        continue;
                    }
                }
                let e = &resolved[0];
                out.push(ScannedSkill {
                    name,
                    path: e.path.clone(),
                    level,
                    description: e.description.clone(),
                });
            }
            Err(reason) => {
                tracing::warn!(
                    skill = %name,
                    dir = %dir.display(),
                    reason = %reason,
                    "skill scan: candidate rejected (not eligible for manifest; \
                     approve flow will also refuse it until fixed)"
                );
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// 读 SKILL.md frontmatter 的声明 name（None=缺失/不可解析;与 B2 解析器同口径,
/// 失败不另立错误——防钓鱼一致性只在「声明了名字」时才判）
fn parse_frontmatter_name(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    crate::agent::definition::parse_skill_frontmatter(&content)
        .ok()?
        .name
}

/// 两源合并 → 生效 manifest 清单 + 注册项钉死哈希表（会话创建装配链与未来
/// IDE 挂接点共用——机制只建一次）
///
/// 优先级：definition 声明 > 项目级 active > 用户级 active；同名取高优先 + warn
/// （显式人工动作压倒自动发现；不 fail-fast——目录内容不因优先级语义而炸会话创建）。
///
/// active 项重算哈希（evorule-hash `digest`），失配 = 自动降级 discovered + warn +
/// 写回账本（防「批准后偷换内容」）。discovered/revoked 项不进清单。
///
/// 哈希表 = 进清单的注册 active 项（name → 钉死哈希），供 read_skill 读时核验
/// （被声明项同名压过的行不出表——声明项人工把关、不验哈希）。
///
/// Err 仅目录级失败（目录不可读）——单 skill 问题见 `scan_skills_dir`（跳过 + warn）。
pub fn merge_skill_manifest(
    declared: Vec<SkillManifestEntry>,
    user_dir: &Path,
    project_dir: Option<&Path>,
    registry: &mut SkillRegistry,
    env_fp: Option<&str>,
) -> Result<MergedSkills, String> {
    let scanned_user = scan_skills_dir(user_dir, SkillLevel::User)?;
    let scanned_project = match project_dir {
        Some(d) => scan_skills_dir(d, SkillLevel::Project)?,
        None => Vec::new(),
    };

    // 账本核对：active 项重算哈希，失配降级（写回账本由调用方 save）；
    // 通过核对者记候选哈希（含路径，声明项压名后按 (name, path) 剔除）
    let mut candidates: Vec<(String, PathBuf, String)> = Vec::new();
    // 降级名单（回喂会话：清单段标「因内容变动已停用」，终结静默失忆）
    let mut downgraded: Vec<String> = Vec::new();
    // 新鲜度提示名单（O-638②：env-sensitive 技能的批准环境 ≠ 当前环境——
    // 提示复验,不降级不拒读;与完整性正交）
    let mut env_stale: Vec<String> = Vec::new();
    for scanned in scanned_user.iter().chain(scanned_project.iter()) {
        let Some(entry) = registry.find(&scanned.name, scanned.level) else {
            continue; // 计算态 discovered（账本无行）
        };
        if entry.status != SkillStatus::Active {
            continue;
        }
        let current = hash_skill_file_prefixed(&scanned.path)?;
        let pinned = entry.content_hash.clone().unwrap_or_default();
        if current != pinned {
            tracing::warn!(
                skill = %scanned.name,
                level = %scanned.level.as_str(),
                expected = %pinned,
                actual = %current,
                "skill content hash mismatch since approval; downgraded to \
                 discovered (re-approve after reviewing the new content)"
            );
            registry.downgrade(&scanned.name, scanned.level);
            downgraded.push(scanned.name.clone());
            continue;
        }
        // 环境失配比对（仅 env-sensitive 技能;双 Some 才可比,旧条目无戳不提示）
        if entry.env_sensitive {
            if let (Some(stamped), Some(current_fp)) = (&entry.env_fp, env_fp) {
                if stamped != current_fp {
                    env_stale.push(scanned.name.clone());
                }
            }
        }
        candidates.push((scanned.name.clone(), scanned.path.clone(), current));
    }

    // 组装 manifest：优先级序遍历，同名取高优先 + warn
    let mut merged: Vec<SkillManifestEntry> = Vec::new();
    let push = |e: SkillManifestEntry, source: &str, merged: &mut Vec<SkillManifestEntry>| {
        if let Some(existing) = merged.iter().find(|m| m.name == e.name) {
            tracing::warn!(
                skill = %e.name,
                winner = source,
                loser_path = %e.path.display(),
                winner_path = %existing.path.display(),
                "skill name conflict across sources; keeping higher-priority source"
            );
            return;
        }
        merged.push(e);
    };

    // 源 1：definition 显式声明（最高优先，不经账本——人工写进 definition 即把关动作）
    for e in declared {
        push(e, "definition declaration", &mut merged);
    }
    // 源 2：项目级 active
    for s in &scanned_project {
        if is_active(registry, &s.name, s.level) {
            push(
                SkillManifestEntry {
                    name: s.name.clone(),
                    path: s.path.clone(),
                    description: s.description.clone(),
                },
                "project-level active",
                &mut merged,
            );
        }
    }
    // 源 3：用户级 active
    for s in &scanned_user {
        if is_active(registry, &s.name, s.level) {
            push(
                SkillManifestEntry {
                    name: s.name.clone(),
                    path: s.path.clone(),
                    description: s.description.clone(),
                },
                "user-level active",
                &mut merged,
            );
        }
    }
    // 哈希表：候选注册项中真正进清单的（按 (name, path) 对齐；被声明项
    // 同名压过的行不出表）
    let content_hashes: std::collections::BTreeMap<String, String> = candidates
        .into_iter()
        .filter(|(name, path, _)| merged.iter().any(|m| &m.name == name && &m.path == path))
        .map(|(name, _, hash)| (name, hash))
        .collect();
    Ok(MergedSkills {
        manifest: merged,
        content_hashes,
        downgraded,
        env_stale,
    })
}

/// 两源合并产物：生效 manifest 清单 + 注册项钉死哈希表
/// （哈希表供 `SkillReadTool` 读时核验；声明面项不入表 = 不验）
#[derive(Debug, Clone, Default)]
pub struct MergedSkills {
    /// 进清单的生效项（definition 声明 > 项目级 active > 用户级 active）
    pub manifest: Vec<SkillManifestEntry>,
    /// 本次会话创建因哈希失配被降级的技能名单（回喂会话用——降级不再静默）
    pub downgraded: Vec<String>,
    /// env-sensitive 技能的批准环境 ≠ 当前环境名单（新鲜度提示——复验提醒,
    /// 不降级;与完整性正交）
    pub env_stale: Vec<String>,
    /// 注册 active 项的钉死哈希（name → `blake3:<hex>`）
    pub content_hashes: std::collections::BTreeMap<String, String>,
}

fn is_active(registry: &SkillRegistry, name: &str, level: SkillLevel) -> bool {
    registry
        .find(name, level)
        .is_some_and(|e| e.status == SkillStatus::Active)
}

/// 环境指纹（O-638② 新鲜度面）：hash(主机名 | serve 版本 | engine 版本)。
/// runtime.json 缺失/无版本字段时省略 engine 分量；主机名取 COMPUTERNAME/
/// HOSTNAME。任一途径都保证至少主机名分量 → 实际恒 Some。
pub fn env_fingerprint(workdir: &Path) -> Option<String> {
    // 主机名三分量跨平台取源：Windows=COMPUTERNAME（env），多数
    // Unix=HOSTNAME（env），容器/CI runner 两者皆无时读 /etc/hostname
    // 文件——三者皆缺（罕见）才返回 None
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let serve = env!("CARGO_PKG_VERSION").to_string();
    let engine = std::fs::read_to_string(workdir.join("runtime.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("version")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        });
    let material = match engine {
        Some(e) => format!("{host}|{serve}|{e}"),
        None => format!("{host}|{serve}"),
    };
    Some(evorule_hash::prefixed(&evorule_hash::digest(
        material.as_bytes(),
    )))
}

/// SKILL.md 文件内容 blake3（evorule-hash `digest`；B 族纪律禁自写 blake3）
pub fn hash_skill_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("skill file '{}' unreadable: {}", path.display(), e))?;
    Ok(evorule_hash::digest(&bytes))
}

/// 批准用存储哈希形态：`blake3:<64hex>`（evorule-hash `prefixed` 包装）
pub fn hash_skill_file_prefixed(path: &Path) -> Result<String, String> {
    Ok(evorule_hash::prefixed(&hash_skill_file(path)?))
}

/// frontmatter 解析（泄露检查与正文预览共用；Err = frontmatter 不可解析）
pub fn parse_skill_body(path: &Path) -> Result<(Option<String>, String), String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("skill file '{}' unreadable: {}", path.display(), e))?;
    let fm = parse_skill_frontmatter(&content)
        .map_err(|e| format!("skill '{}' frontmatter unparseable: {}", path.display(), e))?;
    Ok((fm.description, content))
}

// =============================================================================
// 泄露检查器（approve 前置机器检查；两级分级）
// =============================================================================

/// 命中级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeakSeverity {
    /// 署名泄露族：approve 硬拦（拒绝并明示命中行）
    Hard,
    /// LLM 技术词（合法产品内容）：警告标红但可批
    Warning,
}

/// 泄露检查命中项
#[derive(Debug, Clone, Serialize)]
pub struct LeakHit {
    /// 1-based 行号
    pub line_no: usize,
    /// 命中行内容（截断至 120 字符）
    pub line: String,
    /// 级别
    pub severity: LeakSeverity,
    /// 命中的 pattern
    pub pattern: String,
}

/// 作者署名泄露族（身份表述只在署名语境出现——approve 硬拦；
/// 与公开面扫描纪律的泄露短语族同向，但检查对象是运行时 skill 内容
/// 而非发布物，属产品治理功能）
const LEAK_HARD_PATTERNS: &[&str] = &[
    "ai 生成",
    "ai生成",
    "ai 编写",
    "ai编写",
    "ai 辅助编写",
    "ai辅助编写",
    "ai 辅助生成",
    "ai辅助生成",
    "ai 润色",
    "ai润色",
    "generated by",
    "created by ai",
    "written by ai",
    "written by an ai",
    "由大模型生成",
    "由大模型编写",
    "由模型生成",
    "由模型编写",
    "由助手编写",
    "由助手生成",
    "助手编写",
    "助手生成",
];

/// LLM 技术词（skill 讲「如何让模型用我」是合法内容，警告不拦）
const LEAK_WARN_PATTERNS: &[&str] = &[
    "llm",
    "大模型",
    "语言模型",
    "agent",
    "智能体",
    "模型调用",
    "prompt",
    "提示词",
    "上下文窗口",
];

/// 泄露检查：正文逐行匹配（大小写不敏感 contains）+ 两类实测误报豁免：
/// - **引用豁免**：命中词面整体包在「」引号内 = 文档在举例/转述而非署名，
///   硬拦降级为警告（skill-authoring 类「教什么不能写」的正文才可能合法
///   携带这些词面；非引号内的真实署名表述不受影响）
/// - **路径词豁免**：警告级词命中处紧邻路径字符（`-`/`/`/`\`/`.`/数字等）
///   = 命中的是路径/标识符片段（自家安装根 `…-agent-desktop` 即此形态），
///   整行跳过（硬拦面不做路径豁免——路径里不该出现署名词面）
pub fn check_skill_leaks(content: &str) -> Vec<LeakHit> {
    let mut hits = Vec::new();
    for (idx, raw_line) in content.lines().enumerate() {
        let lower = raw_line.to_lowercase();
        let mut hard_hit: Option<&str> = None;
        let mut warn_hit: Option<&str> = None;
        for p in LEAK_HARD_PATTERNS {
            if let Some(pos) = find_hit_position(&lower, p) {
                if is_quoted_context(&lower, pos, p.len()) {
                    // 引用豁免：降级为警告（保留可见性,不做硬拦）
                    warn_hit = Some(p);
                } else {
                    hard_hit = Some(p);
                }
                break;
            }
        }
        if hard_hit.is_none() && warn_hit.is_none() {
            for p in LEAK_WARN_PATTERNS {
                if let Some(pos) = find_hit_position(&lower, p) {
                    if is_path_fragment(&lower, pos, p.len()) {
                        continue;
                    }
                    warn_hit = Some(p);
                    break;
                }
            }
        }
        if let Some(p) = hard_hit.or(warn_hit) {
            let severity = if hard_hit.is_some() {
                LeakSeverity::Hard
            } else {
                LeakSeverity::Warning
            };
            hits.push(LeakHit {
                line_no: idx + 1,
                line: raw_line.chars().take(120).collect(),
                severity,
                pattern: p.to_string(),
            });
        }
    }
    hits
}

/// 大小写不敏感地定位首个命中位置（多字节安全：按字节下标返回,仅在
/// char 边界成立——patterns 均为 ASCII 或 CJK 词面,与源串同编码）
fn find_hit_position(lower_line: &str, pattern: &str) -> Option<usize> {
    lower_line.find(pattern)
}

/// 命中片段是否整体包在「」引号内（文档引用/举例语境）
fn is_quoted_context(lower_line: &str, start: usize, len: usize) -> bool {
    let before = &lower_line[..start];
    let after = &lower_line[start + len..];
    before.contains('「') && after.contains('」')
}

/// 命中片段两侧是否紧邻路径/标识符字符（路径片段 = 产品名/目录名的一部分）
fn is_path_fragment(line: &str, start: usize, len: usize) -> bool {
    const PATH_CHARS: &[char] = &[
        '-', '_', '/', '\\', '.', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
    ];
    let before_ok = start > 0
        && line[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || PATH_CHARS.contains(&c));
    let after_ok = line[start + len..]
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || PATH_CHARS.contains(&c));
    before_ok || after_ok
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn write_skill(dir: &Path, name: &str, body: &str) -> PathBuf {
        let sub = dir.join(name);
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("SKILL.md"), body).unwrap();
        sub.join("SKILL.md")
    }

    fn manifest(name: &str, path: PathBuf) -> SkillManifestEntry {
        SkillManifestEntry {
            name: name.to_string(),
            path,
            description: "declared".to_string(),
        }
    }

    // ---- 扫描正反例 ----

    // =========================================================================
    // 期刊层（账本纵深加固条目）：重放/漂移自愈/篡改硬错/撕裂恢复/引导迁移
    // =========================================================================

    // =========================================================================
    // 期刊层（账本纵深加固条目）：重放/漂移自愈/篡改硬错/撕裂恢复/引导迁移
    // =========================================================================

    #[test]
    fn test_journal_replays_across_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let reg_path = tmp.path().join("registry.json");
        let md = write_skill(
            &tmp.path().join("data/skills"),
            "s",
            "---\nname: s\ndescription: d\n---\nA",
        );
        let hash = hash_skill_file_prefixed(&md).unwrap();

        let mut reg = SkillRegistry::load(&reg_path).unwrap();
        reg.approve(
            "s",
            md.clone(),
            SkillLevel::User,
            hash.clone(),
            "op",
            None,
            false,
        );
        reg.save().unwrap();

        let reg2 = SkillRegistry::load(&reg_path).unwrap();
        assert!(!reg2.journal_drift());
        assert_eq!(
            reg2.find("s", SkillLevel::User).unwrap().status,
            SkillStatus::Active
        );
        assert!(reg_path.parent().unwrap().join(JOURNAL_FILE).exists());

        let mut reg3 = reg2.clone();
        reg3.revoke("s", SkillLevel::User);
        reg3.save().unwrap();
        let reg4 = SkillRegistry::load(&reg_path).unwrap();
        assert_eq!(
            reg4.find("s", SkillLevel::User).unwrap().status,
            SkillStatus::Revoked
        );
    }

    #[test]
    fn test_journal_detects_snapshot_drift_and_heals_on_save() {
        let tmp = tempfile::tempdir().unwrap();
        let reg_path = tmp.path().join("registry.json");
        let md = write_skill(
            &tmp.path().join("data/skills"),
            "s",
            "---\nname: s\ndescription: d\n---\nA",
        );
        let mut reg = SkillRegistry::load(&reg_path).unwrap();
        reg.approve(
            "s",
            md.clone(),
            SkillLevel::User,
            hash_skill_file_prefixed(&md).unwrap(),
            "op",
            None,
            false,
        );
        reg.save().unwrap();

        // 旁路手改快照：伪造 revoked + 塞入假行
        let mut file: RegistryFile =
            serde_json::from_str(&std::fs::read_to_string(&reg_path).unwrap()).unwrap();
        file.skills[0].status = SkillStatus::Revoked;
        file.skills.push(RegistryEntry {
            name: "smuggled".to_string(),
            path: PathBuf::from("x"),
            level: SkillLevel::User,
            status: SkillStatus::Active,
            content_hash: None,
            approved_at: None,
            approved_by: None,
            env_fp: None,
            env_sensitive: false,
        });
        std::fs::write(&reg_path, serde_json::to_string_pretty(&file).unwrap()).unwrap();

        let reg2 = SkillRegistry::load(&reg_path).unwrap();
        assert!(reg2.journal_drift(), "out-of-band edit must be flagged");
        assert_eq!(
            reg2.find("s", SkillLevel::User).unwrap().status,
            SkillStatus::Active,
            "journal truth wins"
        );
        assert!(
            reg2.find("smuggled", SkillLevel::User).is_none(),
            "smuggled row must not survive replay"
        );

        let mut reg3 = reg2.clone();
        reg3.save().unwrap();
        let reg4 = SkillRegistry::load(&reg_path).unwrap();
        assert!(!reg4.journal_drift(), "healed on save");
        assert!(reg4.find("smuggled", SkillLevel::User).is_none());
    }

    #[test]
    fn test_journal_mid_chain_tamper_fails_visible() {
        let tmp = tempfile::tempdir().unwrap();
        let reg_path = tmp.path().join("registry.json");
        let md = write_skill(
            &tmp.path().join("data/skills"),
            "s",
            "---\nname: s\ndescription: d\n---\nA",
        );
        let mut reg = SkillRegistry::load(&reg_path).unwrap();
        reg.approve(
            "s",
            md.clone(),
            SkillLevel::User,
            hash_skill_file_prefixed(&md).unwrap(),
            "op",
            None,
            false,
        );
        reg.save().unwrap();
        reg.revoke("s", SkillLevel::User);
        reg.save().unwrap();

        let jp = reg_path.parent().unwrap().join(JOURNAL_FILE);
        let content = std::fs::read_to_string(&jp).unwrap();
        let mut lines: Vec<String> = content.lines().map(String::from).collect();
        assert!(lines.len() >= 2);
        let mut rec: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        rec["name"] = serde_json::json!("forged");
        lines[1] = serde_json::to_string(&rec).unwrap();
        std::fs::write(
            &jp,
            lines.join(
                "
",
            ),
        )
        .unwrap();

        let err = SkillRegistry::load(&reg_path).unwrap_err();
        assert!(
            err.contains("hash mismatch") || err.contains("chain broken"),
            "mid-chain tamper must fail visible: {err}"
        );
    }

    #[test]
    fn test_journal_torn_tail_recovers() {
        let tmp = tempfile::tempdir().unwrap();
        let reg_path = tmp.path().join("registry.json");
        let md = write_skill(
            &tmp.path().join("data/skills"),
            "s",
            "---\nname: s\ndescription: d\n---\nA",
        );
        let mut reg = SkillRegistry::load(&reg_path).unwrap();
        reg.approve(
            "s",
            md.clone(),
            SkillLevel::User,
            hash_skill_file_prefixed(&md).unwrap(),
            "op",
            None,
            false,
        );
        reg.save().unwrap();

        let jp = reg_path.parent().unwrap().join(JOURNAL_FILE);
        let mut content = std::fs::read_to_string(&jp).unwrap();
        content.push_str("{\"seq\":1,\"prev_h");
        std::fs::write(&jp, content).unwrap();

        let reg2 = SkillRegistry::load(&reg_path).unwrap();
        assert!(!reg2.journal_drift());
        assert_eq!(
            reg2.entries.len(),
            1,
            "torn tail truncated, prefix preserved"
        );
    }

    #[test]
    fn test_journal_bootstrap_migrates_legacy_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let reg_path = tmp.path().join("registry.json");
        let md = write_skill(
            &tmp.path().join("data/skills"),
            "legacy",
            "---\nname: legacy\ndescription: d\n---\nL",
        );
        let legacy = RegistryFile {
            skills: vec![RegistryEntry {
                name: "legacy".to_string(),
                path: md.clone(),
                level: SkillLevel::User,
                status: SkillStatus::Active,
                content_hash: Some(hash_skill_file_prefixed(&md).unwrap()),
                approved_at: Some(1),
                approved_by: Some("legacy-op".to_string()),
                env_fp: None,
                env_sensitive: false,
            }],
        };
        std::fs::write(&reg_path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let mut reg = SkillRegistry::load(&reg_path).unwrap();
        assert!(!reg.journal_drift());
        reg.approve(
            "legacy",
            md.clone(),
            SkillLevel::User,
            hash_skill_file_prefixed(&md).unwrap(),
            "op",
            None,
            false,
        );
        reg.save().unwrap();
        assert!(reg_path.parent().unwrap().join(JOURNAL_FILE).exists());

        let reg2 = SkillRegistry::load(&reg_path).unwrap();
        assert!(!reg2.journal_drift());
        let e = reg2.find("legacy", SkillLevel::User).unwrap();
        assert_eq!(
            e.approved_by.as_deref(),
            Some("op"),
            "replayed approve must win"
        );
    }

    #[test]
    fn test_journal_signed_mode_roundtrip_and_tamper() {
        // 签名模式（引导密钥在位）：锚后全签名；验签失败/剥签名 = fail-visible
        crate::agent::journal_signing::set_key_for_tests([9u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let reg_path = tmp.path().join("registry.json");
        let md = write_skill(
            &tmp.path().join("data/skills"),
            "s",
            "---\nname: s\ndescription: d\n---\nA",
        );
        let mut reg = SkillRegistry::load(&reg_path).unwrap();
        reg.approve(
            "s",
            md.clone(),
            SkillLevel::User,
            hash_skill_file_prefixed(&md).unwrap(),
            "op",
            None,
            false,
        );
        reg.save().unwrap();

        let reg2 = SkillRegistry::load(&reg_path).unwrap();
        assert_eq!(
            reg2.find("s", SkillLevel::User).unwrap().status,
            SkillStatus::Active
        );

        let jp = reg_path.parent().unwrap().join(JOURNAL_FILE);
        let content = std::fs::read_to_string(&jp).unwrap();
        let mut lines: Vec<String> = content.lines().map(String::from).collect();
        let last_signed = lines
            .iter()
            .rposition(|l| l.contains("\"sig\""))
            .expect("signed mode must carry sigs");
        let mut rec: serde_json::Value = serde_json::from_str(&lines[last_signed]).unwrap();
        rec["sig"] = serde_json::json!("00".repeat(64));
        lines[last_signed] = serde_json::to_string(&rec).unwrap();
        std::fs::write(
            &jp,
            lines.join(
                "
",
            ),
        )
        .unwrap();
        let err = SkillRegistry::load(&reg_path).unwrap_err();
        assert!(
            err.contains("signature invalid")
                || err.contains("missing signature")
                || err.contains("hash mismatch"),
            "tampered sig must fail visible: {err}"
        );
    }

    #[test]
    fn test_env_fingerprint_returns_some_cross_platform() {
        // 跨平台核验：主机名三分量取源（env×2 + /etc/hostname）至少其一可得,
        // Windows 与 Linux runner 均恒 Some（B-1 在 ubuntu CI 的 None 即红根因）
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            env_fingerprint(tmp.path()).is_some(),
            "host component must resolve on this platform"
        );
        let fp1 = env_fingerprint(tmp.path()).unwrap();
        let fp2 = env_fingerprint(tmp.path()).unwrap();
        assert_eq!(fp1, fp2, "fingerprint must be stable within one boot");
    }

    #[test]
    fn test_merge_flags_env_stale_only_for_sensitive_and_mismatched() {
        // env-sensitive 技能的批准环境 != 当前环境 → env_stale；非 sensitive
        // 失配不进名单（零噪音）；环境一致不进名单
        let tmp = tempfile::tempdir().unwrap();
        let user_dir = tmp.path().join("data/skills");
        let md_a = write_skill(
            &user_dir,
            "fact-a",
            "---\nname: fact-a\ndescription: d\nenv-sensitive: true\n---\nA",
        );
        let md_b = write_skill(
            &user_dir,
            "howto-b",
            "---\nname: howto-b\ndescription: d\n---\nB",
        );
        let mut reg = SkillRegistry::load(&tmp.path().join("registry.json")).unwrap();
        reg.approve(
            "fact-a",
            md_a.clone(),
            SkillLevel::User,
            hash_skill_file_prefixed(&md_a).unwrap(),
            "op",
            Some("OLD-ENV".to_string()),
            true,
        );
        reg.approve(
            "howto-b",
            md_b.clone(),
            SkillLevel::User,
            hash_skill_file_prefixed(&md_b).unwrap(),
            "op",
            Some("OLD-ENV".to_string()),
            false,
        );

        let merged =
            merge_skill_manifest(vec![], &user_dir, None, &mut reg, Some("NEW-ENV")).unwrap();
        assert_eq!(merged.env_stale, vec!["fact-a".to_string()]);

        let merged2 =
            merge_skill_manifest(vec![], &user_dir, None, &mut reg, Some("OLD-ENV")).unwrap();
        assert!(merged2.env_stale.is_empty());

        let merged3 = merge_skill_manifest(vec![], &user_dir, None, &mut reg, None).unwrap();
        assert!(
            merged3.env_stale.is_empty(),
            "unknown env must not be flagged"
        );
    }

    #[test]
    fn test_scan_rejects_frontmatter_name_mismatch() {
        // 防钓鱼名一致性(与 import 同判据):目录名是注册/approve 的键,声明名
        // 是 read_skill 的门面,不一致即钓鱼形态——候选整条出局
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "innocent-name",
            "---\nname: smuggled\ndescription: d\n---\nB",
        );
        let scanned = scan_skills_dir(tmp.path(), SkillLevel::User).unwrap();
        assert!(
            scanned.is_empty(),
            "mismatched-name candidate must be rejected"
        );
    }

    #[test]
    fn test_scan_subdirectory_style_discovers_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_skill(
            tmp.path(),
            "git-discipline",
            "---\nname: git-discipline\ndescription: git 纪律指引\n---\n# 正文",
        );
        write_skill(
            tmp.path(),
            "pdf-tools",
            "---\nname: pdf-tools\ndescription: PDF 处理\n---\nB",
        );

        let mut scanned = scan_skills_dir(tmp.path(), SkillLevel::User).unwrap();
        scanned.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(scanned.len(), 2);
        assert_eq!(scanned[0].name, "git-discipline");
        assert_eq!(scanned[0].description, "git 纪律指引");
        assert_eq!(scanned[0].level, SkillLevel::User);
        assert_eq!(scanned[0].path, p);
    }

    #[test]
    fn test_scan_missing_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let scanned = scan_skills_dir(&tmp.path().join("nope"), SkillLevel::User).unwrap();
        assert!(scanned.is_empty());
    }

    #[test]
    fn test_scan_unreadable_dir_fails_fast() {
        // 目录路径指向一个常规文件 = 存在但不可作为目录读 → fail-fast（H 族）
        let tmp = tempfile::tempdir().unwrap();
        let file_as_dir = tmp.path().join("not-a-dir");
        std::fs::write(&file_as_dir, "x").unwrap();
        let err = scan_skills_dir(&file_as_dir, SkillLevel::User).unwrap_err();
        assert!(err.contains("fail-fast"), "got: {}", err);
    }

    #[test]
    fn test_scan_bad_frontmatter_skips_only_that_skill() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "broken", "no frontmatter here");
        write_skill(
            tmp.path(),
            "good",
            "---\nname: good\ndescription: ok\n---\nB",
        );

        let scanned = scan_skills_dir(tmp.path(), SkillLevel::Project).unwrap();
        assert_eq!(scanned.len(), 1, "broken skill skipped, good one kept");
        assert_eq!(scanned[0].name, "good");
        assert_eq!(scanned[0].level, SkillLevel::Project);
    }

    #[test]
    fn test_scan_missing_skill_md_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("empty-skill")).unwrap();
        write_skill(
            tmp.path(),
            "real",
            "---\nname: real\ndescription: d\n---\nB",
        );
        let scanned = scan_skills_dir(tmp.path(), SkillLevel::User).unwrap();
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].name, "real");
    }

    #[test]
    fn test_scan_chinese_dir_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "合作方式固化",
            "---\nname: 合作方式固化\ndescription: 跨会话合作纪律\n---\n# 正文",
        );
        let scanned = scan_skills_dir(tmp.path(), SkillLevel::User).unwrap();
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].name, "合作方式固化");
        assert_eq!(scanned[0].description, "跨会话合作纪律");
    }

    #[test]
    fn test_scan_ignores_loose_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("registry.json"), "{}").unwrap();
        write_skill(tmp.path(), "s1", "---\nname: s1\ndescription: d\n---\nB");
        let scanned = scan_skills_dir(tmp.path(), SkillLevel::User).unwrap();
        assert_eq!(scanned.len(), 1);
    }

    // ---- 账本 approve / revoke / 降级循环 ----

    #[test]
    fn test_registry_load_missing_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = SkillRegistry::load(&tmp.path().join("registry.json")).unwrap();
        assert!(reg.entries().is_empty());
    }

    #[test]
    fn test_registry_corrupt_file_fails_visible() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("registry.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = SkillRegistry::load(&path).unwrap_err();
        assert!(err.contains("corrupt"), "got: {}", err);
    }

    #[test]
    fn test_registry_approve_revoke_cycle_with_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("registry.json");
        let skill_md = write_skill(tmp.path(), "s", "---\nname: s\ndescription: d\n---\nB");

        let mut reg = SkillRegistry::load(&path).unwrap();
        assert!(reg.find("s", SkillLevel::User).is_none());

        let hash = hash_skill_file_prefixed(&skill_md).unwrap();
        assert!(hash.starts_with("blake3:"), "storage hash must be prefixed");
        reg.approve(
            "s",
            skill_md.clone(),
            SkillLevel::User,
            hash.clone(),
            "local-operator",
            None,
            false,
        );
        reg.save().unwrap();

        let reg2 = SkillRegistry::load(&path).unwrap();
        let e = reg2.find("s", SkillLevel::User).unwrap();
        assert_eq!(e.status, SkillStatus::Active);
        assert_eq!(e.content_hash.as_deref(), Some(hash.as_str()));
        assert_eq!(e.approved_by.as_deref(), Some("local-operator"));
        assert!(e.approved_at.is_some());

        // revoke 留痕（行保留）
        let mut reg3 = reg2.clone();
        reg3.revoke("s", SkillLevel::User);
        reg3.save().unwrap();
        let reg4 = SkillRegistry::load(&path).unwrap();
        let e = reg4.find("s", SkillLevel::User).unwrap();
        assert_eq!(e.status, SkillStatus::Revoked);
        assert!(e.content_hash.is_some(), "history row keeps the old hash");

        // re-approve = 重新激活（幂等路径）
        let mut reg5 = reg4.clone();
        reg5.approve(
            "s",
            skill_md,
            SkillLevel::User,
            hash,
            "local-operator",
            None,
            false,
        );
        assert_eq!(
            reg5.find("s", SkillLevel::User).unwrap().status,
            SkillStatus::Active
        );
    }

    #[test]
    fn test_registry_revoke_without_row_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = SkillRegistry::load(&tmp.path().join("registry.json")).unwrap();
        reg.revoke("ghost", SkillLevel::User); // 不得 panic
        assert!(reg.entries().is_empty());
    }

    // ---- 合并优先级矩阵 ----

    #[test]
    fn test_merge_declared_wins_over_directory_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let user_dir = tmp.path().join("data/skills");
        write_skill(
            &user_dir,
            "s",
            "---\nname: s\ndescription: from-user-dir\n---\nB",
        );
        let declared_path = tmp.path().join("elsewhere/SKILL.md");
        std::fs::create_dir_all(declared_path.parent().unwrap()).unwrap();
        std::fs::write(
            &declared_path,
            "---\nname: s\ndescription: declared\n---\nD",
        )
        .unwrap();

        let mut reg = SkillRegistry::load(&tmp.path().join("registry.json")).unwrap();
        let MergedSkills {
            manifest: merged, ..
        } = merge_skill_manifest(
            vec![manifest("s", declared_path.clone())],
            &user_dir,
            None,
            &mut reg,
            None,
        )
        .unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].path, declared_path, "declaration must win");
    }

    #[test]
    fn test_merge_project_beats_user_on_same_name() {
        let tmp = tempfile::tempdir().unwrap();
        let user_dir = tmp.path().join("data/skills");
        let project_dir = tmp.path().join("proj/.evo/skills");
        let user_md = write_skill(&user_dir, "s", "---\nname: s\ndescription: user\n---\nU");
        let proj_md = write_skill(
            &project_dir,
            "s",
            "---\nname: s\ndescription: project\n---\nP",
        );
        let mut reg = SkillRegistry::load(&tmp.path().join("registry.json")).unwrap();
        reg.approve(
            "s",
            user_md.clone(),
            SkillLevel::User,
            hash_skill_file_prefixed(&user_md).unwrap(),
            "op",
            None,
            false,
        );
        reg.approve(
            "s",
            proj_md.clone(),
            SkillLevel::Project,
            hash_skill_file_prefixed(&proj_md).unwrap(),
            "op",
            None,
            false,
        );

        let MergedSkills {
            manifest: merged, ..
        } = merge_skill_manifest(vec![], &user_dir, Some(&project_dir), &mut reg, None).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].path, proj_md,
            "project level must beat user level"
        );
        assert_eq!(merged[0].description, "project");
    }

    #[test]
    fn test_merge_discovered_and_revoked_excluded() {
        let tmp = tempfile::tempdir().unwrap();
        let user_dir = tmp.path().join("data/skills");
        let disc_md = write_skill(&user_dir, "d1", "---\nname: d1\ndescription: d\n---\nB");
        let act_md = write_skill(&user_dir, "a1", "---\nname: a1\ndescription: a\n---\nB");
        let rev_md = write_skill(&user_dir, "r1", "---\nname: r1\ndescription: r\n---\nB");
        let mut reg = SkillRegistry::load(&tmp.path().join("registry.json")).unwrap();
        // d1 保持无账本行 = 计算态 discovered
        reg.approve(
            "a1",
            act_md.clone(),
            SkillLevel::User,
            hash_skill_file_prefixed(&act_md).unwrap(),
            "op",
            None,
            false,
        );
        reg.approve(
            "r1",
            rev_md.clone(),
            SkillLevel::User,
            hash_skill_file_prefixed(&rev_md).unwrap(),
            "op",
            None,
            false,
        );
        reg.revoke("r1", SkillLevel::User);

        let MergedSkills {
            manifest: merged, ..
        } = merge_skill_manifest(vec![], &user_dir, None, &mut reg, None).unwrap();
        let names: Vec<&str> = merged.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["a1"], "discovered/revoked must be excluded");
        let _ = disc_md;
    }

    // ---- 哈希失配降级 ----

    #[test]
    fn test_merge_hash_mismatch_downgrades_to_discovered() {
        let tmp = tempfile::tempdir().unwrap();
        let user_dir = tmp.path().join("data/skills");
        let md = write_skill(&user_dir, "s", "---\nname: s\ndescription: v1\n---\nB");
        let mut reg = SkillRegistry::load(&tmp.path().join("registry.json")).unwrap();
        reg.approve(
            "s",
            md.clone(),
            SkillLevel::User,
            hash_skill_file_prefixed(&md).unwrap(),
            "op",
            None,
            false,
        );

        // 批准后偷换内容
        std::fs::write(&md, "---\nname: s\ndescription: v2-swapped\n---\nEVIL").unwrap();

        let MergedSkills {
            manifest: merged, ..
        } = merge_skill_manifest(vec![], &user_dir, None, &mut reg, None).unwrap();
        assert!(
            merged.is_empty(),
            "mismatched skill must leave the manifest"
        );
        let e = reg.find("s", SkillLevel::User).unwrap();
        assert_eq!(
            e.status,
            SkillStatus::Discovered,
            "auto-downgrade on mismatch"
        );
        assert!(
            e.content_hash.is_some(),
            "downgraded row keeps old hash for audit"
        );

        // 复查文件后 re-approve 恢复
        reg.approve(
            "s",
            md,
            SkillLevel::User,
            hash_skill_file_prefixed(&user_dir.join("s/SKILL.md")).unwrap(),
            "op",
            None,
            false,
        );
        let MergedSkills {
            manifest: merged, ..
        } = merge_skill_manifest(vec![], &user_dir, None, &mut reg, None).unwrap();
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn test_merge_active_hash_stable_stays_active() {
        let tmp = tempfile::tempdir().unwrap();
        let user_dir = tmp.path().join("data/skills");
        let md = write_skill(&user_dir, "s", "---\nname: s\ndescription: stable\n---\nB");
        let mut reg = SkillRegistry::load(&tmp.path().join("registry.json")).unwrap();
        reg.approve(
            "s",
            md,
            SkillLevel::User,
            hash_skill_file_prefixed(&user_dir.join("s/SKILL.md")).unwrap(),
            "op",
            None,
            false,
        );

        let MergedSkills {
            manifest: merged, ..
        } = merge_skill_manifest(vec![], &user_dir, None, &mut reg, None).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].description, "stable");
        assert_eq!(
            reg.find("s", SkillLevel::User).unwrap().status,
            SkillStatus::Active
        );
    }

    #[test]
    fn test_merge_directory_level_failure_propagates() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("data/skills");
        std::fs::create_dir_all(bad.parent().unwrap()).unwrap();
        std::fs::write(&bad, "x").unwrap(); // 文件冒充目录
        let mut reg = SkillRegistry::load(&tmp.path().join("registry.json")).unwrap();
        let err = merge_skill_manifest(vec![], &bad, None, &mut reg, None).unwrap_err();
        assert!(
            err.contains("fail-fast"),
            "directory-level failure must fail-fast"
        );
    }

    // ---- 哈希原语（evorule-hash 对齐） ----

    #[test]
    fn test_hash_uses_evorule_hash_digest() {
        // 与 evorule-hash 黄金向量同口径：digest(b"evorule") = 7758f0…
        assert_eq!(
            evorule_hash::digest(b"evorule"),
            "7758f03680f4593e860eb2fc5257cc78d78563d315debde26be7bdb82f18bed4"
        );
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("SKILL.md");
        std::fs::write(&p, "evorule").unwrap();
        assert_eq!(
            hash_skill_file(&p).unwrap(),
            evorule_hash::digest(b"evorule")
        );
        assert_eq!(
            hash_skill_file_prefixed(&p).unwrap(),
            format!("blake3:{}", evorule_hash::digest(b"evorule"))
        );
    }

    // ---- 泄露检查分级矩阵 ----

    #[test]
    fn test_leak_hard_blocks_authorship_phrases() {
        // 词表样例字面量一律拆写（运行时等价）：源码自身在公开面扫描范围内，
        // 拆写后源码不命中扫描词表而运行时串仍含完整命中词（词表拆写先例）。
        let cases = [
            concat!("本文由 AI 生", "成。"),
            concat!("本文由AI生", "成。"),
            concat!("Generated by Cl", "aude in 2026"),
            "generated by some tool",
            "此 skill 由大模型编写",
            "Written by an AI assistant.",
        ];
        for body in cases {
            let hits = check_skill_leaks(body);
            assert!(
                hits.iter().any(|h| h.severity == LeakSeverity::Hard),
                "must hard-block: {}",
                body
            );
        }
    }

    #[test]
    fn test_leak_hard_reports_line_number_and_pattern() {
        let body = concat!(
            "---\nname: x\ndescription: d\n---\n# 指引\n正常一行\n本文由 AI 生",
            "成\n"
        );
        let hits = check_skill_leaks(body);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line_no, 7);
        assert_eq!(hits[0].severity, LeakSeverity::Hard);
        assert!(hits[0].pattern.contains("ai"));
        assert!(hits[0].line.contains(concat!("由 AI 生", "成")));
    }

    #[test]
    fn test_leak_warn_flags_technical_terms_but_not_hard() {
        let body = "---\nname: x\ndescription: d\n---\nLLM 应按以下步骤调用本 skill。\n";
        let hits = check_skill_leaks(body);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].severity,
            LeakSeverity::Warning,
            "technical term = warn only"
        );
    }

    #[test]
    fn test_leak_clean_content_passes() {
        let body = "---\nname: x\ndescription: d\n---\n# 指引\n先 git status 再 commit。\n";
        assert!(check_skill_leaks(body).is_empty());
    }

    #[test]
    fn test_leak_mixed_hard_and_warn_reports_both() {
        let body = "Generated by GPT\nLLM 按步骤操作\n";
        let hits = check_skill_leaks(body);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].severity, LeakSeverity::Hard);
        assert_eq!(hits[1].severity, LeakSeverity::Warning);
    }
}
