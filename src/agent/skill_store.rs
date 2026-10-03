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
}

/// registry.json 顶层形态
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RegistryFile {
    skills: Vec<RegistryEntry>,
}

/// 注册账本（`<skills 根>/registry.json`）
///
/// 并发口径：serve 单进程、审批低频，原子写（临时文件 + rename）即可，
/// 无锁机制（设计档 §4.2）。
#[derive(Debug, Clone)]
pub struct SkillRegistry {
    path: PathBuf,
    entries: Vec<RegistryEntry>,
}

impl SkillRegistry {
    /// 加载账本。文件缺失 = 空账本（首次运行常态）；JSON 损坏 = Err
    /// fail-visible（门禁不可静默降级，H 族）。
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self {
                path: path.to_path_buf(),
                entries: Vec::new(),
            });
        }
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
            entries: file.skills,
        })
    }

    /// 原子写（临时文件 + rename；rename 在 Windows 上覆盖已存在目标）
    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("skill registry dir '{}' unwritable: {}", parent.display(), e))?;
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
        });
    }

    /// 撤销：置 revoked（保留历史行便于审计回看）；无行时 no-op
    /// （撤销一个账本没记过的 skill = 撤销一个从未生效的 skill，状态无变化）
    pub fn revoke(&mut self, name: &str, level: SkillLevel) {
        if let Some(e) = self.find_mut(name, level) {
            e.status = SkillStatus::Revoked;
        }
    }

    /// 失配降级：active → discovered（保留旧哈希供审计回看「批准时的内容已变」）
    fn downgrade(&mut self, name: &str, level: SkillLevel) {
        if let Some(e) = self.find_mut(name, level) {
            e.status = SkillStatus::Discovered;
        }
    }

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

/// 两源合并 → 生效 manifest 清单（会话创建装配链与未来 IDE 挂接点共用——机制只建一次）
///
/// 优先级：definition 声明 > 项目级 active > 用户级 active；同名取高优先 + warn
/// （显式人工动作压倒自动发现；不 fail-fast——目录内容不因优先级语义而炸会话创建）。
///
/// active 项重算哈希（evorule-hash `digest`），失配 = 自动降级 discovered + warn +
/// 写回账本（防「批准后偷换内容」）。discovered/revoked 项不进清单。
///
/// Err 仅目录级失败（目录不可读）——单 skill 问题见 `scan_skills_dir`（跳过 + warn）。
pub fn merge_skill_manifest(
    declared: Vec<SkillManifestEntry>,
    user_dir: &Path,
    project_dir: Option<&Path>,
    registry: &mut SkillRegistry,
) -> Result<Vec<SkillManifestEntry>, String> {
    let scanned_user = scan_skills_dir(user_dir, SkillLevel::User)?;
    let scanned_project = match project_dir {
        Some(d) => scan_skills_dir(d, SkillLevel::Project)?,
        None => Vec::new(),
    };

    // 账本核对：active 项重算哈希，失配降级（写回账本由调用方 save）
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
        }
    }

    // 组装 manifest：优先级序遍历，同名取高优先 + warn
    let mut merged: Vec<SkillManifestEntry> = Vec::new();
    let mut push = |e: SkillManifestEntry, source: &str, merged: &mut Vec<SkillManifestEntry>| {
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
    Ok(merged)
}

fn is_active(registry: &SkillRegistry, name: &str, level: SkillLevel) -> bool {
    registry
        .find(name, level)
        .is_some_and(|e| e.status == SkillStatus::Active)
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

    #[test]
    fn test_scan_subdirectory_style_discovers_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_skill(
            tmp.path(),
            "git-discipline",
            "---\nname: git-discipline\ndescription: git 纪律指引\n---\n# 正文",
        );
        write_skill(tmp.path(), "pdf-tools", "---\nname: pdf\ndescription: PDF 处理\n---\nB");

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
        write_skill(tmp.path(), "good", "---\nname: good\ndescription: ok\n---\nB");

        let scanned = scan_skills_dir(tmp.path(), SkillLevel::Project).unwrap();
        assert_eq!(scanned.len(), 1, "broken skill skipped, good one kept");
        assert_eq!(scanned[0].name, "good");
        assert_eq!(scanned[0].level, SkillLevel::Project);
    }

    #[test]
    fn test_scan_missing_skill_md_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("empty-skill")).unwrap();
        write_skill(tmp.path(), "real", "---\nname: real\ndescription: d\n---\nB");
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
        reg.approve("s", skill_md.clone(), SkillLevel::User, hash.clone(), "local-operator");
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
        reg5.approve("s", skill_md, SkillLevel::User, hash, "local-operator");
        assert_eq!(reg5.find("s", SkillLevel::User).unwrap().status, SkillStatus::Active);
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
        let md = write_skill(
            &user_dir,
            "s",
            "---\nname: s\ndescription: from-user-dir\n---\nB",
        );
        let declared_path = tmp.path().join("elsewhere/SKILL.md");
        std::fs::create_dir_all(declared_path.parent().unwrap()).unwrap();
        std::fs::write(&declared_path, "---\nname: s\ndescription: declared\n---\nD").unwrap();

        let mut reg = SkillRegistry::load(&tmp.path().join("registry.json")).unwrap();
        let merged = merge_skill_manifest(
            vec![manifest("s", declared_path.clone())],
            &user_dir,
            None,
            &mut reg,
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
        let user_md = write_skill(
            &user_dir,
            "s",
            "---\nname: s\ndescription: user\n---\nU",
        );
        let proj_md = write_skill(
            &project_dir,
            "s",
            "---\nname: s\ndescription: project\n---\nP",
        );
        let mut reg = SkillRegistry::load(&tmp.path().join("registry.json")).unwrap();
        reg.approve("s", user_md.clone(), SkillLevel::User, hash_skill_file_prefixed(&user_md).unwrap(), "op");
        reg.approve("s", proj_md.clone(), SkillLevel::Project, hash_skill_file_prefixed(&proj_md).unwrap(), "op");

        let merged = merge_skill_manifest(vec![], &user_dir, Some(&project_dir), &mut reg).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].path, proj_md, "project level must beat user level");
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
        reg.approve("a1", act_md.clone(), SkillLevel::User, hash_skill_file_prefixed(&act_md).unwrap(), "op");
        reg.approve("r1", rev_md.clone(), SkillLevel::User, hash_skill_file_prefixed(&rev_md).unwrap(), "op");
        reg.revoke("r1", SkillLevel::User);

        let merged = merge_skill_manifest(vec![], &user_dir, None, &mut reg).unwrap();
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
        reg.approve("s", md.clone(), SkillLevel::User, hash_skill_file_prefixed(&md).unwrap(), "op");

        // 批准后偷换内容
        std::fs::write(&md, "---\nname: s\ndescription: v2-swapped\n---\nEVIL").unwrap();

        let merged = merge_skill_manifest(vec![], &user_dir, None, &mut reg).unwrap();
        assert!(merged.is_empty(), "mismatched skill must leave the manifest");
        let e = reg.find("s", SkillLevel::User).unwrap();
        assert_eq!(e.status, SkillStatus::Discovered, "auto-downgrade on mismatch");
        assert!(e.content_hash.is_some(), "downgraded row keeps old hash for audit");

        // 复查文件后 re-approve 恢复
        reg.approve("s", md, SkillLevel::User, hash_skill_file_prefixed(&user_dir.join("s/SKILL.md")).unwrap(), "op");
        let merged = merge_skill_manifest(vec![], &user_dir, None, &mut reg).unwrap();
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn test_merge_active_hash_stable_stays_active() {
        let tmp = tempfile::tempdir().unwrap();
        let user_dir = tmp.path().join("data/skills");
        let md = write_skill(&user_dir, "s", "---\nname: s\ndescription: stable\n---\nB");
        let mut reg = SkillRegistry::load(&tmp.path().join("registry.json")).unwrap();
        reg.approve("s", md, SkillLevel::User, hash_skill_file_prefixed(&user_dir.join("s/SKILL.md")).unwrap(), "op");

        let merged = merge_skill_manifest(vec![], &user_dir, None, &mut reg).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].description, "stable");
        assert_eq!(reg.find("s", SkillLevel::User).unwrap().status, SkillStatus::Active);
    }

    #[test]
    fn test_merge_directory_level_failure_propagates() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("data/skills");
        std::fs::create_dir_all(bad.parent().unwrap()).unwrap();
        std::fs::write(&bad, "x").unwrap(); // 文件冒充目录
        let mut reg = SkillRegistry::load(&tmp.path().join("registry.json")).unwrap();
        let err = merge_skill_manifest(vec![], &bad, None, &mut reg).unwrap_err();
        assert!(err.contains("fail-fast"), "directory-level failure must fail-fast");
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
        assert_eq!(hash_skill_file(&p).unwrap(), evorule_hash::digest(b"evorule"));
        assert_eq!(
            hash_skill_file_prefixed(&p).unwrap(),
            format!("blake3:{}", evorule_hash::digest(b"evorule"))
        );
    }
}
