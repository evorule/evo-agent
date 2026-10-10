// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 技能双层注册（跨源注册规格策略批）：声明面技能的记忆域镜像。
//!
//! 两层分工：
//! - **元数据层（真账镜像）**：声明技能 → `shared.{ns}.procedural.skills.{name}`
//!   真账本事实（payload 通道，会话期写入）。治理权威仍在本地 registry.json
//!   （三态+哈希钉定，skill_store），账本行是**检索投影**（权威=human/指令级，
//!   confidence 1.0）；声明撤销=同 path null 值新版本（墓碑语义，追加不改写，
//!   删除可证明）。
//! - **正文索引层（本地合成）**：已声明技能的 SKILL.md 按 markdown 节投影
//!   （节标题+首段）→ `local.skills.` 家族（LexStore 本地行，合成 id bit63，
//!   path 带节锚回指盘上文件）。正文是工作区资产非账本事实，**不进账本**；
//!   read_skill 渐进披露语义不变（检索命中给位置，装载仍走 read_skill）。
//!   整族 replace_partition 全量重建=撤销技能正文自然出局（本地层无需墓碑）。
//!
//! 门控与同步：Recipe `sources.skills_index`（缺省关=既有 agent 零影响）；
//! 会话创建后调用一次（session_id 在手）；对账=拉镜像族 latest-wins 比对，
//! 只写差异（账本版本不空转）。全程 best-effort：账本不可达=诚实降级
//! （degraded 置位），不阻塞会话。

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::agent::definition::SkillManifestEntry;
use crate::agent::lexstore::{mem_type, synthetic_fact_id, LexStore};
use crate::agent::memory::{latest_values_by_path, MemoryManager, MemoryRecord};

/// 本地正文索引家族前缀
pub const LOCAL_SKILLS_PREFIX: &str = "local.skills.";

/// 同步结果（观测面；skipped=门控关，degraded=账本不可达诚实降级）
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SkillsMirrorStats {
    /// 门控关闭未执行（`sources.skills_index` 为 false）
    pub skipped: bool,
    /// 账本不可达/会话未就绪的诚实降级（不阻塞会话）
    pub degraded: bool,
    /// 成功写入的声明元数据事实条数
    pub metadata_written: usize,
    /// 摘除的过期镜像事实（墓碑）条数
    pub tombstoned: usize,
    /// 正文索引写入的分段条数
    pub body_sections: usize,
}

/// 技能双层注册同步（会话创建后调用一次；`sources.skills_index` 门控）。
///
/// 前置：MemoryManager 的 session_id 已由 runner 同步（payload 写通道
/// 会话期语义）。skills=生效清单（serve 路径两源合并后的 manifest）。
pub async fn sync_skills_mirror(
    mem: &mut MemoryManager,
    skills: &[SkillManifestEntry],
) -> SkillsMirrorStats {
    let mut stats = SkillsMirrorStats::default();
    let gate = mem
        .recipe
        .as_ref()
        .map(|r| r.sources.skills_index)
        .unwrap_or(false);
    if !gate {
        stats.skipped = true;
        return stats;
    }
    let Some(session) = mem.session_id().map(str::to_string) else {
        tracing::warn!("skills mirror: session not set; sync skipped");
        stats.degraded = true;
        return stats;
    };
    let ns = mem.namespace().to_string();
    let prefix = format!("shared.{ns}.procedural.skills.");

    // 期望集：声明技能 → (账本 path, 投影文本)
    let mut desired: BTreeMap<String, String> = BTreeMap::new();
    for s in skills {
        desired.insert(
            format!("{prefix}{}", s.name),
            format!("{}: {}", s.name, s.description),
        );
    }

    // 对账：拉镜像族 latest-wins（账本不可达=诚实降级，不阻塞会话）
    let remote = match mem.evorule_client.get_shared_facts(Some(&prefix)).await {
        Ok(facts) => facts,
        Err(e) => {
            tracing::warn!(error = %e, "skills mirror: ledger unreachable; sync degraded");
            stats.degraded = true;
            return stats;
        }
    };
    let entries: Vec<(String, u64, Value)> = remote
        .into_iter()
        .map(|f| (f.path, f.version, f.value))
        .collect();
    let (alive, _) = latest_values_by_path(entries);

    // 差异写入：缺失或投影文本变化才写（版本不空转）
    let now = now_secs();
    for (path, text) in &desired {
        let current = alive
            .get(path)
            .and_then(|v| v.get("value"))
            .and_then(Value::as_str);
        if current == Some(text.as_str()) {
            continue;
        }
        let name = path.strip_prefix(&prefix).unwrap_or(path);
        let mut record = MemoryRecord::new(name, text, now);
        record.lifecycle_state = Some("Settled".to_string());
        record.source = Some("system".to_string());
        record.confidence = Some(1.0);
        record.tags = vec!["skill".to_string(), "declared".to_string()];
        let Ok(payload) = serde_json::to_value(&record) else {
            continue;
        };
        match mem
            .evorule_client
            .update_payload(&session, path, &payload)
            .await
        {
            Ok(_) => stats.metadata_written += 1,
            Err(e) => {
                tracing::warn!(path = %path, error = %e, "skills mirror: metadata write failed");
                stats.degraded = true;
            }
        }
    }

    // 撤销对账：账本存活行 ∉ 声明集 → null 值新版本（墓碑，删除可证明）
    for path in alive.keys() {
        if desired.contains_key(path) {
            continue;
        }
        match mem
            .evorule_client
            .update_payload(&session, path, &Value::Null)
            .await
        {
            Ok(_) => stats.tombstoned += 1,
            Err(e) => {
                tracing::warn!(path = %path, error = %e, "skills mirror: tombstone failed");
                stats.degraded = true;
            }
        }
    }

    // 正文索引层：整族重建（撤销技能自然出局；本地行 mem_type 逐行覆盖）
    let store = mem.lex_store.clone();
    if let Some(store) = store {
        stats.body_sections = rebuild_body_index(&store, skills, now);
    }
    stats
}

/// 正文索引整族重建：SKILL.md 按 markdown 节投影（节标题+首段）→
/// `local.skills.` 合成 id 行。
fn rebuild_body_index(store: &LexStore, skills: &[SkillManifestEntry], now: u64) -> usize {
    let mut rows: Vec<(u64, String, Value)> = Vec::new();
    for s in skills {
        let Ok(content) = std::fs::read_to_string(&s.path) else {
            tracing::warn!(skill = %s.name, path = %s.path.display(), "skills mirror: body unreadable; section index skipped");
            continue;
        };
        let body = strip_frontmatter(&content);
        for (i, section) in split_skill_sections(body).into_iter().enumerate() {
            let anchor = format!("{}#{}", s.path.display(), i + 1);
            let fact_id = synthetic_fact_id(&anchor);
            let value = json!({
                "key": format!("skills.{}#{}", s.name, i + 1),
                "value": section,
                "timestamp": now,
                "lifecycle_state": "Settled",
                "mem_type": mem_type::PROCEDURAL,
            });
            rows.push((fact_id, anchor, value));
        }
    }
    let n = rows.len();
    if let Err(e) = store.replace_partition(LOCAL_SKILLS_PREFIX, &rows) {
        tracing::warn!(error = %e, "skills mirror: body index rebuild failed");
        return 0;
    }
    n
}

/// frontmatter 剥离：首非空行为 `---` 围栏时返回闭合围栏之后正文；
/// 无围栏/未闭合=原文返回（宽松口径，与 frontmatter 解析器一致）
fn strip_frontmatter(content: &str) -> &str {
    let body = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut lines = body.lines();
    let Some(first) = lines.next() else {
        return body;
    };
    if first.trim_end() != "---" {
        return body;
    }
    let mut offset = first.len() + 1;
    for line in lines {
        offset += line.len() + 1;
        if line.trim_end() == "---" {
            return &body[offset.min(body.len())..];
        }
    }
    body
}

/// markdown 标题行判定：至少一个 `#` 且后随空白或行尾（防 `#tag` 误判）
fn is_heading(line: &str) -> bool {
    let t = line.trim_end();
    let hashes = t.chars().take_while(|c| *c == '#').count();
    hashes > 0 && (hashes == t.len() || t[hashes..].starts_with(' '))
}

/// markdown 节切分（确定性）：标题行开新节，节文本=标题行+其后首个
/// 非空段落；首标题前的导语并入第一节（无标题文档=单节全量）。
fn split_skill_sections(body: &str) -> Vec<String> {
    let mut sections: Vec<(Option<String>, Vec<&str>)> = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim_end();
        if is_heading(trimmed) {
            sections.push((
                Some(trimmed.trim_start_matches('#').trim().to_string()),
                Vec::new(),
            ));
        } else if let Some((_, lines)) = sections.last_mut() {
            lines.push(trimmed);
        } else {
            // 首标题前导语：以空标题开节
            sections.push((None, vec![trimmed]));
        }
    }
    sections
        .into_iter()
        .filter_map(|(title, lines)| {
            let first_para: Vec<&str> = lines
                .split(|l| l.trim().is_empty())
                .find(|p| p.iter().any(|l| !l.trim().is_empty()))
                .map(|p| p.to_vec())
                .unwrap_or_default();
            let para = first_para.join("\n");
            match (title, para.is_empty()) {
                (Some(t), false) => Some(format!("{t}\n{para}")),
                (Some(t), true) => Some(t),
                (None, false) => Some(para),
                (None, true) => None,
            }
        })
        .collect()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::evorule_client::EvoruleApiClient;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn make_mgr() -> MemoryManager {
        MemoryManager::new("ns", EvoruleApiClient::new("http://localhost:8080"))
    }

    fn make_recipe(skills_index: bool) -> crate::agent::recipe::MemoryRecipe {
        let mut r = crate::agent::recipe::MemoryRecipe::default();
        r.sources.skills_index = skills_index;
        r
    }

    fn manifest(entries: &[(&str, &str, &str)]) -> Vec<SkillManifestEntry> {
        entries
            .iter()
            .map(|(n, p, d)| SkillManifestEntry {
                name: n.to_string(),
                path: PathBuf::from(p),
                description: d.to_string(),
            })
            .collect()
    }

    #[tokio::test]
    async fn test_gate_off_is_skipped() {
        // 缺省关=既有 agent 零影响（零网络调用，离线客户端安全）
        let mut mem = make_mgr();
        mem.set_recipe(make_recipe(false));
        let stats = sync_skills_mirror(&mut mem, &manifest(&[("a", "/x/SKILL.md", "d")])).await;
        assert!(stats.skipped);
        assert!(!stats.degraded);
    }

    #[tokio::test]
    async fn test_ledger_unreachable_degrades_gracefully() {
        // 门开+账本不可达 → 诚实降级（不 panic 不阻塞）
        let mut mem = make_mgr();
        mem.set_recipe(make_recipe(true));
        mem.set_session_id("s1");
        let stats = sync_skills_mirror(&mut mem, &manifest(&[("a", "/x/SKILL.md", "d")])).await;
        assert!(!stats.skipped);
        assert!(stats.degraded);
    }

    #[test]
    fn test_split_skill_sections() {
        let body = "# 用法\n\n第一步做甲。\n\n## 注意\n\n不要做乙。续行也属首段。\n";
        let sections = split_skill_sections(body);
        assert_eq!(
            sections,
            vec!["用法\n第一步做甲。", "注意\n不要做乙。续行也属首段。"]
        );
    }

    #[test]
    fn test_split_skill_sections_preamble_and_empty() {
        // 首标题前导语成独立节；全空文档=空集
        let preamble = "导语一段。\n# 标题\n正文。";
        assert_eq!(
            split_skill_sections(preamble),
            vec!["导语一段。", "标题\n正文。"]
        );
        assert!(split_skill_sections("\n\n").is_empty());
    }

    #[test]
    fn test_body_index_rebuild_local_family() {
        // 正文索引：合成 id 置位、mem_type 逐行覆盖、整族可读（TTL 免除）
        let dir = std::env::temp_dir().join(format!("skills-mirror-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let skill_path = dir.join("SKILL.md");
        std::fs::write(
            &skill_path,
            "# 用法\n\n第一步做甲。\n\n## 注意\n\n不要做乙。\n",
        )
        .unwrap();
        let skills = manifest(&[("demo", &skill_path.to_string_lossy(), "d")]);
        let store = Arc::new(
            LexStore::open(&{
                let p = std::env::temp_dir()
                    .join(format!("skills-mirror-db-{}.db", std::process::id()));
                let _ = std::fs::remove_file(&p);
                p
            })
            .unwrap(),
        );
        let n = rebuild_body_index(&store, &skills, 1_700_000_000);
        assert_eq!(n, 2);
        let rows = store.local_facts(LOCAL_SKILLS_PREFIX).unwrap();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert!(row.fact_id & crate::agent::lexstore::SYNTHETIC_ID_FLAG != 0);
            assert_eq!(row.value["mem_type"], "procedural");
            assert!(row
                .path
                .starts_with(&skill_path.to_string_lossy().to_string()));
        }
        // 撤销技能=空清单整族重建 → 正文自然出局
        let n2 = rebuild_body_index(&store, &[], 1_700_000_001);
        assert_eq!(n2, 0);
        assert!(store.local_facts(LOCAL_SKILLS_PREFIX).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_strip_frontmatter_variants() {
        let with_fm = "---\nname: demo\ndescription: d\n---\n\n# 用法\n正文。";
        assert_eq!(strip_frontmatter(with_fm), "\n# 用法\n正文。");
        assert_eq!(strip_frontmatter("# 无围栏\n正文"), "# 无围栏\n正文");
        // 闭合围栏缺失=原文返回（宽松口径）
        let unclosed = "---\nname: x\n正文";
        assert_eq!(strip_frontmatter(unclosed), unclosed);
    }
}
