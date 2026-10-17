// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! skills 注册审批管理面 REST（skills 全动态装配 C 形态）
//!
//! ## 通路语义（关键区分）
//!
//! 注册审批 = **定义期供应链准入**（这内容获不获得系统背书地位）——
//! 人工动作、动作留痕在 registry.json 账本；**不走** io_request /
//! ApprovalCallback（那是会话期运行审批，两者不混装）。
//!
//! ## 端点
//!
//! | 端点 | 语义 |
//! |---|---|
//! | `GET /api/skills` | 全清单（三态 + 来源级 + 哈希 + description 摘要）——管理面权威视图 |
//! | `GET /api/skills/pending` | discovered 项 + 正文预览（审批前必读，人工过目 = 供应链闸口） |
//! | `POST /api/skills/approve` | 泄露检查（署名硬拦 / 技术词警告）→ blake3 落账 → active |
//! | `POST /api/skills/revoke` | 状态置 revoked（下会话 manifest 移除；账本保留历史行） |
//! | `POST /api/skills/import` | skill-pack bundle 解包落盘 → 计算态 discovered（导入≠生效，人在环在 approve） |
//!
//! 工作台管理 UI 留 IDE 化批；本面 REST 即可操作（curl / REST 消费面先例充分）。
//! 鉴权随路由面（`/api/skills*` 不进 PUBLIC_PATHS，默认 Bearer token 内）。

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::definition::parse_skill_frontmatter;
use crate::agent::skill_store::{
    check_skill_leaks, hash_skill_file_prefixed, merge_skill_manifest, parse_skill_body,
    scan_skills_dir, SkillLevel, SkillRegistry, SkillStatus,
};
use crate::api::agent_api::AgentApiState;
use crate::api::human_gate::CallerIdentity;

/// 用户级 skill 目录（serve workdir 相对，运行时资产，gitignore 管辖）
pub fn user_skills_dir(state: &AgentApiState) -> std::path::PathBuf {
    state.workdir().join("data").join("skills")
}

/// 项目级 skill 目录（现状单工作区：workdir 即沙箱基目录；对齐 `.evo/` 工作区约定）
pub fn project_skills_dir(state: &AgentApiState) -> std::path::PathBuf {
    state.workdir().join(".evo").join("skills")
}

fn registry_path(state: &AgentApiState) -> std::path::PathBuf {
    user_skills_dir(state).join("registry.json")
}

// =============================================================================
// 视图与请求/响应形态
// =============================================================================

/// 单条 skill 视图（GET /api/skills 元素）
#[derive(Debug, Serialize)]
pub struct SkillView {
    /// 技能名（skill 子目录名）
    pub name: String,
    /// 来源级："user" | "project"
    pub level: String,
    /// SKILL.md 绝对路径
    pub path: String,
    /// 三态："discovered" | "active" | "revoked"
    pub status: String,
    /// frontmatter description（账本 stale 行为空串）
    pub description: String,
    /// 批准钉死的 blake3 哈希（prefixed；无管理动作时缺省）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// 批准时刻（unix secs）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<u64>,
    /// 批准者（缺省 local-operator）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
}

/// GET /api/skills 响应（管理面权威全清单）
#[derive(Debug, Serialize)]
pub struct SkillInventoryResponse {
    /// 全部 skill 视图（user 级在前，级内按名排序）
    pub skills: Vec<SkillView>,
}

/// 待审项（GET /api/skills/pending 元素）：discovered + 正文全文预览
/// （审批前必读内容——人工过目 = 供应链闸口）
#[derive(Debug, Serialize)]
pub struct PendingSkillView {
    /// 技能名
    pub name: String,
    /// 来源级："user" | "project"
    pub level: String,
    /// SKILL.md 绝对路径
    pub path: String,
    /// frontmatter description
    pub description: String,
    /// SKILL.md 全文（人工过目用；frontmatter 损坏时为错误说明占位）
    pub content: String,
}

/// GET /api/skills/pending 响应
#[derive(Debug, Serialize)]
pub struct PendingSkillsResponse {
    /// discovered 项列表（正文全文随行）
    pub pending: Vec<PendingSkillView>,
}

/// approve/revoke 请求体
#[derive(Debug, Deserialize)]
pub struct SkillNameRequest {
    /// 目标技能名（skill 子目录名）
    pub name: String,
    /// 来源级："user" | "project"
    pub level: String,
    /// 操作者（可选；v1 无操作者身份体系，缺省 local-operator）
    #[serde(default)]
    pub operator: Option<String>,
}

/// bundle 导入请求体（`POST /api/skills/import`）
#[derive(Debug, Deserialize)]
pub struct SkillImportRequest {
    /// 工作区内 skill-pack bundle 文件路径（JSON；复用 B7 同源路径语义）
    pub path: String,
}

/// bundle 内单条 skill 条目
#[derive(Debug, Deserialize)]
pub struct SkillImportEntry {
    /// 技能名（= skill 子目录名；`[a-zA-Z0-9][a-zA-Z0-9_-]*`，防路径穿越）
    pub name: String,
    /// 来源级："user" | "project"
    pub level: String,
    /// SKILL.md 全文（含 frontmatter 围栏）
    pub body: String,
}

/// bundle 文件 v1 格式：`{ "skills": [ { name, level, body } ] }`
#[derive(Debug, Deserialize)]
pub struct SkillImportBundle {
    /// 技能条目列表（空数组整体 400）
    pub skills: Vec<SkillImportEntry>,
}

/// 单条导入结果
#[derive(Debug, Serialize)]
pub struct SkillImportResult {
    /// 技能名
    pub name: String,
    /// 来源级（"user" | "project"）
    pub level: String,
    /// 落盘的 SKILL.md 绝对路径
    pub path: String,
    /// 状态："imported"（落盘，待审批）| "rejected"（reason 随行）
    pub status: String,
    /// rejected 时的原因（imported 时缺省）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `POST /api/skills/import` — skill-pack bundle 导入 → 解包落盘 skills 目录
///
/// F2-D1 闭环：导入 = 把 bundle 内 SKILL.md 写入 `<skills_dir>/<name>/SKILL.md`，
/// 扫描即计算态 discovered → 待审批组出现 → approve 既有链生效（导入≠生效，
/// 人在环兜底在 approve 侧：泄露检查 + blake3 落账）。
///
/// - bundle 路径必须在工作区内（canonicalize 越界拒；对齐 B7 文件面语义）
/// - name 非法（含路径分隔/穿越）→ 该项 rejected；frontmatter 不可解析 → rejected
/// - 同名同 level 账本行已 active/revoked → rejected（已审批内容不静默覆盖；
///   先 revoke 或改名）——discovered（账本无行）允许重导入覆盖（内容更新语义）
/// - HumanGate：写面语义，落人工账（op="skills_import"）
pub async fn import_skills(
    State(state): State<AgentApiState>,
    Json(req): Json<SkillImportRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // 写面人工账（对齐 file_api gate_write 语义）
    let identity = crate::api::human_gate::ensure_write_allowed(state.auth_config());
    let gate = |outcome: &str| {
        state.human_gate().record(
            "skills_import",
            CallerIdentity::Anonymous,
            &req.path,
            outcome,
        );
    };
    if let Err(e) = identity {
        gate("denied");
        return Err((e.0, Json(serde_json::json!({ "error": e.1 }))));
    }

    // bundle 路径越界守卫（工作区内；canonicalize 防穿越）
    let bundle_path = state.workdir().join(&req.path);
    let canon = bundle_path.canonicalize().map_err(|e| {
        gate("error");
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("bundle file '{}' unreadable: {}", req.path, e)
            })),
        )
    })?;
    let workdir_canon = state
        .workdir()
        .canonicalize()
        .unwrap_or_else(|_| state.workdir().to_path_buf());
    if !canon.starts_with(&workdir_canon) {
        gate("error");
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("bundle path '{}' escapes workspace", req.path)
            })),
        ));
    }

    let raw = std::fs::read_to_string(&canon).map_err(|e| {
        gate("error");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("bundle '{}' unreadable: {}", req.path, e)
            })),
        )
    })?;
    let bundle: SkillImportBundle = serde_json::from_str(&raw).map_err(|e| {
        gate("error");
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("bundle '{}' is not a valid skill-pack ({{skills:[{{name,level,body}}]}}): {}", req.path, e)
            })),
        )
    })?;
    if bundle.skills.is_empty() {
        gate("error");
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "bundle contains no skills" })),
        ));
    }

    // 账本一次加载：同名同 level 已 active/revoked → 拒（不静默覆盖已审批内容）
    let registry = SkillRegistry::load(&registry_path(&state)).map_err(|e| {
        gate("error");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e })),
        )
    })?;

    let name_re = regex_lite_skill_name();
    let mut results: Vec<SkillImportResult> = Vec::new();
    for entry in bundle.skills {
        let mut item = |status: &str, path: String, reason: Option<String>| {
            results.push(SkillImportResult {
                name: entry.name.clone(),
                level: entry.level.clone(),
                path,
                status: status.to_string(),
                reason,
            });
        };
        // name 安全面：仅字母数字下划线连字符，首字符字母数字（防穿越/隐藏目录）
        if !name_re.is_match(&entry.name) {
            item(
                "rejected",
                String::new(),
                Some(format!("invalid skill name '{}'", entry.name)),
            );
            continue;
        }
        let Some(level) = level_from_str(&entry.level) else {
            item(
                "rejected",
                String::new(),
                Some(format!("invalid level '{}' (user|project)", entry.level)),
            );
            continue;
        };
        // frontmatter 可解析（导入对象必须是合法 skill；正文级问题留给 approve 泄露检查）
        let fm = match parse_skill_frontmatter(&entry.body) {
            Ok(fm) => fm,
            Err(e) => {
                item("rejected", String::new(), Some(e));
                continue;
            }
        };
        // frontmatter name 与条目 name 一致性（防钓鱼：目录名≠声明名）
        if let Some(declared) = &fm.name {
            if declared != &entry.name {
                item(
                    "rejected",
                    String::new(),
                    Some(format!(
                        "frontmatter name '{}' mismatches entry name '{}'",
                        declared, entry.name
                    )),
                );
                continue;
            }
        }
        // 已审批内容保护
        if let Some(row) = registry.find(&entry.name, level) {
            if matches!(row.status, SkillStatus::Active | SkillStatus::Revoked) {
                item(
                    "rejected",
                    String::new(),
                    Some(format!(
                        "skill '{}' is {} — revoke it first or use another name (approved content is not silently overwritten)",
                        entry.name,
                        match row.status {
                            SkillStatus::Active => "active",
                            _ => "revoked",
                        }
                    )),
                );
                continue;
            }
        }
        // 落盘 `<skills_dir>/<name>/SKILL.md`（子目录式，市面惯例一致）
        let dir = match level {
            SkillLevel::User => user_skills_dir(&state),
            SkillLevel::Project => project_skills_dir(&state),
        };
        let skill_dir = dir.join(&entry.name);
        let write = std::fs::create_dir_all(&skill_dir)
            .and_then(|_| std::fs::write(skill_dir.join("SKILL.md"), &entry.body));
        match write {
            Ok(()) => item(
                "imported",
                skill_dir.join("SKILL.md").display().to_string(),
                None,
            ),
            Err(e) => item(
                "rejected",
                String::new(),
                Some(format!("write failed: {}", e)),
            ),
        }
    }

    let imported = results.iter().filter(|r| r.status == "imported").count();
    gate("ok");
    tracing::info!(
        bundle = %req.path,
        imported,
        total = results.len(),
        "skill bundle imported (pending approval; import != activate)"
    );
    Ok(Json(serde_json::json!({
        "bundle": req.path,
        "imported": imported,
        "results": results,
    })))
}

/// skill 名合法形态（单次编译缓存；防路径穿越/隐藏目录）
fn regex_lite_skill_name() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9_-]*$").unwrap())
}

/// approve 成功响应
#[derive(Debug, Serialize)]
pub struct ApproveResponse {
    /// 技能名
    pub name: String,
    /// 来源级
    pub level: String,
    /// 结果状态（恒 "active"）
    pub status: String,
    /// 批准钉死的 blake3 哈希（prefixed 形态）
    pub content_hash: String,
    /// 警告级命中（技术词——合法产品内容，可批；明示供操作者裁量留痕）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::agent::skill_store::LeakHit>,
}

/// 拒绝/错误响应（approve 硬拦与其他端点错误共用形态）
#[derive(Debug, Serialize)]
pub struct LeakRejectResponse {
    /// 是否为泄露检查拒绝（其余错误为 false）
    pub rejected: bool,
    /// 拒绝/错误原因
    pub reason: String,
    /// 硬拦命中明细（署名泄露族；非泄露错误时为空）
    pub hits: Vec<crate::agent::skill_store::LeakHit>,
}

/// revoke 成功响应
#[derive(Debug, Serialize)]
pub struct RevokeResponse {
    /// 技能名
    pub name: String,
    /// 来源级
    pub level: String,
    /// 结果状态（恒 "revoked"）
    pub status: String,
}

fn level_from_str(s: &str) -> Option<SkillLevel> {
    match s {
        "user" => Some(SkillLevel::User),
        "project" => Some(SkillLevel::Project),
        _ => None,
    }
}

/// 全清单合成：目录扫描（计算态）+ 账本（管理动作态）
///
/// - 扫到且账本无行 → 计算态 discovered
/// - 扫到且账本有行 → 账本行状态（active/revoked/失配降级 discovered）
/// - 账本有行但目录已无此 skill → 原样显示账本行（stale 行，description 空）
fn build_inventory(state: &AgentApiState) -> Result<SkillInventoryResponse, String> {
    let user_dir = user_skills_dir(state);
    let project_dir = project_skills_dir(state);
    let scanned_user = scan_skills_dir(&user_dir, SkillLevel::User)?;
    let scanned_project = scan_skills_dir(&project_dir, SkillLevel::Project)?;
    let registry = SkillRegistry::load(&registry_path(state))?;

    let mut views: Vec<SkillView> = Vec::new();
    let push_scanned = |s: &crate::agent::skill_store::ScannedSkill, views: &mut Vec<SkillView>| {
        let (status, content_hash, approved_at, approved_by) = match registry.find(&s.name, s.level)
        {
            Some(e) => (
                e.status,
                e.content_hash.clone(),
                e.approved_at,
                e.approved_by.clone(),
            ),
            None => (SkillStatus::Discovered, None, None, None),
        };
        views.push(SkillView {
            name: s.name.clone(),
            level: s.level.as_str().to_string(),
            path: s.path.display().to_string(),
            status: status.as_str().to_string(),
            description: s.description.clone(),
            content_hash,
            approved_at,
            approved_by,
        });
    };
    for s in &scanned_user {
        push_scanned(s, &mut views);
    }
    for s in &scanned_project {
        push_scanned(s, &mut views);
    }
    // 账本 stale 行（目录里已扫不到）：原样显示，description 空
    for e in registry.entries() {
        let still_scanned = scanned_user
            .iter()
            .chain(scanned_project.iter())
            .any(|s| s.name == e.name && s.level == e.level);
        if !still_scanned {
            views.push(SkillView {
                name: e.name.clone(),
                level: e.level.as_str().to_string(),
                path: e.path.display().to_string(),
                status: e.status.as_str().to_string(),
                description: String::new(),
                content_hash: e.content_hash.clone(),
                approved_at: e.approved_at,
                approved_by: e.approved_by.clone(),
            });
        }
    }
    views.sort_by(|a, b| (a.level.as_str(), &a.name).cmp(&(b.level.as_str(), &b.name)));
    Ok(SkillInventoryResponse { skills: views })
}

// =============================================================================
// Handlers
// =============================================================================

/// `GET /api/skills` — 全清单（三态 + 来源级 + 哈希 + description 摘要）
pub async fn list_skills(
    State(state): State<AgentApiState>,
) -> Result<Json<SkillInventoryResponse>, (StatusCode, Json<LeakRejectResponse>)> {
    build_inventory(&state).map(Json).map_err(|e| {
        tracing::error!(error = %e, "skill inventory failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(LeakRejectResponse {
                rejected: false,
                reason: e,
                hits: Vec::new(),
            }),
        )
    })
}

/// `GET /api/skills/pending` — discovered 项 + 正文预览（审批前必读）
pub async fn list_pending_skills(
    State(state): State<AgentApiState>,
) -> Result<Json<PendingSkillsResponse>, (StatusCode, Json<LeakRejectResponse>)> {
    match build_inventory(&state) {
        Ok(inv) => {
            let mut pending = Vec::new();
            for v in inv.skills.iter().filter(|v| v.status == "discovered") {
                let path = std::path::PathBuf::from(&v.path);
                match parse_skill_body(&path) {
                    Ok((_, content)) => pending.push(PendingSkillView {
                        name: v.name.clone(),
                        level: v.level.clone(),
                        path: v.path.clone(),
                        description: v.description.clone(),
                        content,
                    }),
                    Err(e) => {
                        // frontmatter 损坏的 discovered 项：仍列出但正文预览带错误说明
                        // （审批端点会拒绝它；fail-visible 于管理面）
                        pending.push(PendingSkillView {
                            name: v.name.clone(),
                            level: v.level.clone(),
                            path: v.path.clone(),
                            description: String::new(),
                            content: format!("<unreadable: {}>", e),
                        });
                    }
                }
            }
            Ok(Json(PendingSkillsResponse { pending }))
        }
        Err(e) => {
            tracing::error!(error = %e, "skill pending list failed");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(LeakRejectResponse {
                    rejected: false,
                    reason: e,
                    hits: Vec::new(),
                }),
            ))
        }
    }
}

/// `POST /api/skills/approve` — 泄露检查（4.5.1）→ blake3 落账 → active
///
/// - 署名泄露族命中 → 422 拒绝 + 命中行明细
/// - 技术词命中 → 200 + warnings（可批，警告随响应留痕）
/// - 未知 name/level → 404；frontmatter 不可解析 → 422
/// - 幂等：重复 approve 重算哈希更新时间戳（内容更新后重新走审批即此路径）
pub async fn approve_skill(
    State(state): State<AgentApiState>,
    Json(req): Json<SkillNameRequest>,
) -> Result<Json<ApproveResponse>, (StatusCode, Json<LeakRejectResponse>)> {
    let Some(level) = level_from_str(&req.level) else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(LeakRejectResponse {
                rejected: true,
                reason: format!(
                    "unknown level '{}' (must be 'user' or 'project')",
                    req.level
                ),
                hits: Vec::new(),
            }),
        ));
    };
    let dir = match level {
        SkillLevel::User => user_skills_dir(&state),
        SkillLevel::Project => project_skills_dir(&state),
    };
    // 从扫描结果定位（单 skill 不合格的候选天然不在其中——approve 拒绝）
    let scanned = scan_skills_dir(&dir, level).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(LeakRejectResponse {
                rejected: false,
                reason: e,
                hits: Vec::new(),
            }),
        )
    })?;
    let Some(s) = scanned.iter().find(|s| s.name == req.name) else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(LeakRejectResponse {
                rejected: true,
                reason: format!(
                    "skill '{}' not found under {} (or not eligible: fix SKILL.md first)",
                    req.name,
                    dir.display()
                ),
                hits: Vec::new(),
            }),
        ));
    };

    // 正文读取 + frontmatter 校验（审批对象必须是可解析的有效 skill）
    let (_, content) = parse_skill_body(&s.path).map_err(|e| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(LeakRejectResponse {
                rejected: true,
                reason: e,
                hits: Vec::new(),
            }),
        )
    })?;

    // 泄露检查（两级分级）：硬拦命中 → 422 + 明细
    let hits = check_skill_leaks(&content);
    let hard: Vec<_> = hits
        .iter()
        .filter(|h| h.severity == crate::agent::skill_store::LeakSeverity::Hard)
        .cloned()
        .collect();
    let warnings: Vec<_> = hits
        .into_iter()
        .filter(|h| h.severity == crate::agent::skill_store::LeakSeverity::Warning)
        .collect();
    if !hard.is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(LeakRejectResponse {
                rejected: true,
                reason: format!(
                    "authorship-leak phrases found in skill '{}'; rewrite the lines \
                     before approving (hard-blocked by registration gate)",
                    req.name
                ),
                hits: hard,
            }),
        ));
    }

    // blake3 落账（evorule-hash；B 族纪律）→ active
    let content_hash = hash_skill_file_prefixed(&s.path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(LeakRejectResponse {
                rejected: false,
                reason: e,
                hits: Vec::new(),
            }),
        )
    })?;
    let operator = req
        .operator
        .as_deref()
        .unwrap_or("local-operator")
        .to_string();
    let mut registry = SkillRegistry::load(&registry_path(&state)).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(LeakRejectResponse {
                rejected: false,
                reason: e,
                hits: Vec::new(),
            }),
        )
    })?;
    registry.approve(
        &req.name,
        s.path.clone(),
        level,
        content_hash.clone(),
        &operator,
    );
    registry.save().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(LeakRejectResponse {
                rejected: false,
                reason: e,
                hits: Vec::new(),
            }),
        )
    })?;
    tracing::info!(
        skill = %req.name,
        level = %req.level,
        hash = %content_hash,
        warnings = warnings.len(),
        "skill approved (registration gate passed)"
    );
    Ok(Json(ApproveResponse {
        name: req.name,
        level: req.level,
        status: "active".to_string(),
        content_hash,
        warnings,
    }))
}

/// `POST /api/skills/revoke` — 状态置 revoked（账本保留历史行）
///
/// 账本无行但目录扫到 → 写 revoked 行（显式拒绝记录）；两者皆无 → 404。
pub async fn revoke_skill(
    State(state): State<AgentApiState>,
    Json(req): Json<SkillNameRequest>,
) -> Result<Json<RevokeResponse>, (StatusCode, Json<LeakRejectResponse>)> {
    let Some(level) = level_from_str(&req.level) else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(LeakRejectResponse {
                rejected: true,
                reason: format!(
                    "unknown level '{}' (must be 'user' or 'project')",
                    req.level
                ),
                hits: Vec::new(),
            }),
        ));
    };
    let mut registry = SkillRegistry::load(&registry_path(&state)).map_err(internal_err)?;
    let known_in_registry = registry.find(&req.name, level).is_some();
    if !known_in_registry {
        // 目录扫到才可写显式拒绝行；否则视为未知 skill
        let dir = match level {
            SkillLevel::User => user_skills_dir(&state),
            SkillLevel::Project => project_skills_dir(&state),
        };
        let scanned = scan_skills_dir(&dir, level).map_err(internal_err)?;
        if !scanned.iter().any(|s| s.name == req.name) {
            return Err((
                StatusCode::NOT_FOUND,
                Json(LeakRejectResponse {
                    rejected: true,
                    reason: format!("skill '{}' not found (level {})", req.name, req.level),
                    hits: Vec::new(),
                }),
            ));
        }
    }
    registry.revoke(&req.name, level);
    registry.save().map_err(internal_err)?;
    tracing::info!(skill = %req.name, level = %req.level, "skill revoked");
    Ok(Json(RevokeResponse {
        name: req.name,
        level: req.level,
        status: "revoked".to_string(),
    }))
}

fn internal_err(e: String) -> (StatusCode, Json<LeakRejectResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(LeakRejectResponse {
            rejected: false,
            reason: e,
            hits: Vec::new(),
        }),
    )
}

/// 路由挂载（router_with_auth 内调用；鉴权内，不进 PUBLIC_PATHS）
pub fn skills_routes() -> axum::Router<AgentApiState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/api/skills", get(list_skills))
        .route("/api/skills/pending", get(list_pending_skills))
        .route("/api/skills/approve", post(approve_skill))
        .route("/api/skills/revoke", post(revoke_skill))
        .route("/api/skills/import", post(import_skills))
}

/// 会话创建装配链入口（PR-3 接线用）：扫描 + 账本核对 + 两源合并的
/// serve 侧封装（目录解析单点；merge 为纯函数，IDE 挂接未来直接调它）
pub fn merged_manifest_for_session(
    state: &AgentApiState,
    declared: Vec<crate::agent::definition::SkillManifestEntry>,
) -> Result<
    (
        Vec<crate::agent::definition::SkillManifestEntry>,
        std::path::PathBuf,
    ),
    String,
> {
    let user_dir = user_skills_dir(state);
    let project_dir = project_skills_dir(state);
    let registry_path = registry_path(state);
    let mut registry = SkillRegistry::load(&registry_path)?;
    let manifest = merge_skill_manifest(declared, &user_dir, Some(&project_dir), &mut registry)?;
    // 失配降级等账本变更落盘（无变更时也幂等重写，量小无碍）
    registry.save()?;
    Ok((manifest, registry_path))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::api::agent_api::{router, AgentApiState};
    use crate::api::evorule_client::EvoruleApiClient;
    use crate::api::metrics::Metrics;
    use crate::api::workspace_client::WorkspaceApiClient;
    use crate::io_handlers::tool_handler::ToolHandler;
    use std::sync::Arc;

    fn make_state(workdir: std::path::PathBuf) -> AgentApiState {
        AgentApiState::new_with_metrics(
            crate::agent::AgentDefinitionManager::with_default_dir(),
            EvoruleApiClient::new("http://localhost:8080"),
            Arc::new(Metrics::new().unwrap()),
            workdir,
            Arc::new(WorkspaceApiClient::new("http://localhost:8080")),
            Arc::new(std::sync::RwLock::new(ToolHandler::new())),
        )
    }

    /// human 身份(写面放行;对齐 file_api mutation_state——auth enabled 即中间件
    /// 已验 token 语义)。bundle 导入是写面(HumanGate),测试须经此构造。
    fn human_state(workdir: std::path::PathBuf) -> AgentApiState {
        make_state(workdir).with_auth_config(crate::api::auth::AuthConfig::new(
            vec!["test-token".to_string()],
            true,
        ))
    }

    /// human_state + router_with_auth(bundle 导入测试专用;`router()` 会用
    /// AuthConfig::disabled 覆盖 state 的 auth config,导致写面门恒 403)
    fn human_router(workdir: std::path::PathBuf) -> axum::Router {
        let auth = crate::api::auth::AuthConfig::new(vec!["test-token".to_string()], true);
        crate::api::agent_api::router_with_auth(human_state(workdir), auth)
    }

    fn write_skill(workdir: &std::path::Path, level_dir: &str, name: &str, body: &str) {
        let sub = workdir.join(level_dir).join(name);
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("SKILL.md"), body).unwrap();
    }

    /// 带认证的 POST(human_state 测试用;token 须与 AuthConfig 配置一致)
    #[allow(dead_code)]
    async fn post_authed(
        app: axum::Router,
        uri: &str,
        json: serde_json::Value,
        token: &str,
    ) -> (StatusCode, serde_json::Value) {
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::from(json.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let body = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    async fn get(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
        get_authed(app, uri, None).await
    }

    /// 带认证的 GET(human_router 测试用)
    async fn get_authed(
        app: axum::Router,
        uri: &str,
        token: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = axum::http::Request::builder().uri(uri);
        if let Some(t) = token {
            builder = builder.header("authorization", format!("Bearer {t}"));
        }
        let resp = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    async fn post(
        app: axum::Router,
        uri: &str,
        json: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(json.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let body = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    const CLEAN_SKILL: &str = "---\nname: git-discipline\ndescription: git 纪律指引\n---\n# 正文\n先 git status 再 commit。\n";

    #[tokio::test]
    async fn test_list_skills_empty_workdir() {
        let tmp = tempfile::tempdir().unwrap();
        let app = router(make_state(tmp.path().to_path_buf()));
        let (status, body) = get(app, "/api/skills").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["skills"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_pending_shows_discovered_with_content_preview() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "data/skills", "git-discipline", CLEAN_SKILL);
        let app = router(make_state(tmp.path().to_path_buf()));

        // 全清单：计算态 discovered
        let (status, body) = get(app.clone(), "/api/skills").await;
        assert_eq!(status, StatusCode::OK);
        let skills = body["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0]["name"], "git-discipline");
        assert_eq!(skills[0]["level"], "user");
        assert_eq!(skills[0]["status"], "discovered");
        assert_eq!(skills[0]["description"], "git 纪律指引");

        // 待审列表：discovered + 正文全文预览（审批前必读）
        let (status, body) = get(app, "/api/skills/pending").await;
        assert_eq!(status, StatusCode::OK);
        let pending = body["pending"].as_array().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["name"], "git-discipline");
        assert!(pending[0]["content"]
            .as_str()
            .unwrap()
            .contains("git status"));
    }

    #[tokio::test]
    async fn test_approve_then_inventory_active() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "data/skills", "git-discipline", CLEAN_SKILL);
        let app = router(make_state(tmp.path().to_path_buf()));

        let (status, body) = post(
            app.clone(),
            "/api/skills/approve",
            serde_json::json!({"name": "git-discipline", "level": "user"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "active");
        let hash = body["content_hash"].as_str().unwrap().to_string();
        assert!(hash.starts_with("blake3:"), "pinned hash must be prefixed");

        // 账本落盘验证
        let reg = SkillRegistry::load(&tmp.path().join("data/skills/registry.json")).unwrap();
        let e = reg.find("git-discipline", SkillLevel::User).unwrap();
        assert_eq!(e.status, SkillStatus::Active);
        assert_eq!(e.content_hash.as_deref(), Some(hash.as_str()));
        assert_eq!(e.approved_by.as_deref(), Some("local-operator"));

        // 全清单翻转 active；pending 清空
        let (status, body) = get(app.clone(), "/api/skills").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["skills"][0]["status"], "active");
        let (_, body) = get(app, "/api/skills/pending").await;
        assert!(body["pending"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_approve_unknown_name_and_bad_level() {
        let tmp = tempfile::tempdir().unwrap();
        let app = router(make_state(tmp.path().to_path_buf()));

        let (status, body) = post(
            app.clone(),
            "/api/skills/approve",
            serde_json::json!({"name": "ghost", "level": "user"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["rejected"], true);

        let (status, _) = post(
            app,
            "/api/skills/approve",
            serde_json::json!({"name": "x", "level": "bogus"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_approve_rejects_authorship_leak_with_hits() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "data/skills",
            "leaky",
            concat!(
                "---\nname: leaky\ndescription: d\n---\n# 正文\n本文由 AI 生",
                "成，请放心使用。\n"
            ),
        );
        let app = router(make_state(tmp.path().to_path_buf()));

        let (status, body) = post(
            app,
            "/api/skills/approve",
            serde_json::json!({"name": "leaky", "level": "user"}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["rejected"], true);
        let hits = body["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["severity"], "hard");
        assert_eq!(hits[0]["line_no"], 6);
        assert!(hits[0]["line"]
            .as_str()
            .unwrap()
            .contains(concat!("由 AI 生", "成")));
    }

    #[tokio::test]
    async fn test_approve_warns_on_technical_terms_but_passes() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "data/skills",
            "tech",
            "---\nname: tech\ndescription: d\n---\nLLM 按以下步骤调用本 skill。\n",
        );
        let app = router(make_state(tmp.path().to_path_buf()));

        let (status, body) = post(
            app,
            "/api/skills/approve",
            serde_json::json!({"name": "tech", "level": "user"}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "technical terms warn but do not block"
        );
        let warnings = body["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0]["severity"], "warning");
    }

    #[tokio::test]
    async fn test_reapprove_is_idempotent_and_revoked_can_reactivate() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "data/skills", "s", CLEAN_SKILL);
        let app = router(make_state(tmp.path().to_path_buf()));
        let req = serde_json::json!({"name": "s", "level": "user"});

        // 首批 + 重复 approve（幂等）
        let (s1, b1) = post(app.clone(), "/api/skills/approve", req.clone()).await;
        let (s2, b2) = post(app.clone(), "/api/skills/approve", req.clone()).await;
        assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));
        assert_eq!(b1["content_hash"], b2["content_hash"]);

        // revoke → 200；清单翻转 revoked
        let (status, body) = post(app.clone(), "/api/skills/revoke", req.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "revoked");
        let (_, inv) = get(app.clone(), "/api/skills").await;
        assert_eq!(inv["skills"][0]["status"], "revoked");
        assert!(
            !inv["skills"][0]["content_hash"].is_null(),
            "revoked row keeps history hash"
        );

        // revoked 再 approve = 重新激活
        let (status, body) = post(app.clone(), "/api/skills/approve", req.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "active");
    }

    #[tokio::test]
    async fn test_revoke_unknown_name_404_and_project_level_flow() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), ".evo/skills", "proj-skill", CLEAN_SKILL);
        let app = router(make_state(tmp.path().to_path_buf()));

        // 未知名 revoke → 404
        let (status, _) = post(
            app.clone(),
            "/api/skills/revoke",
            serde_json::json!({"name": "ghost", "level": "project"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // 项目级全流程：discovered（level=project）→ approve → active
        let (_, inv) = get(app.clone(), "/api/skills").await;
        assert_eq!(inv["skills"][0]["level"], "project");
        assert_eq!(inv["skills"][0]["status"], "discovered");

        let (status, _) = post(
            app.clone(),
            "/api/skills/approve",
            serde_json::json!({"name": "proj-skill", "level": "project", "operator": "damu"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let reg = SkillRegistry::load(&tmp.path().join("data/skills/registry.json")).unwrap();
        let e = reg.find("proj-skill", SkillLevel::Project).unwrap();
        assert_eq!(e.status, SkillStatus::Active);
        assert_eq!(e.approved_by.as_deref(), Some("damu"), "operator override");
    }

    // =========================================================================
    // 会话创建装配链(C 形态 PR-3):merged_manifest_for_session + wire_skills
    // =========================================================================

    /// 装配链辅助:对给定 workdir 跑「合并 → wire_skills」返回 (read_skill 是否
    /// 注册, manifest 清单)
    fn assemble(
        workdir: &std::path::Path,
        declared: Vec<crate::agent::definition::SkillManifestEntry>,
    ) -> (bool, Vec<crate::agent::definition::SkillManifestEntry>) {
        let state = make_state(workdir.to_path_buf());
        let (manifest, reg_path) = merged_manifest_for_session(&state, declared).unwrap();
        assert!(reg_path.ends_with("registry.json"));
        let mut handler = crate::io_handlers::tool_handler::ToolHandler::new();
        let resolved = crate::api::serve_tools::wire_skills(&mut handler, Some(manifest)).unwrap();
        (handler.has_tool("read_skill"), resolved.unwrap_or_default())
    }

    #[tokio::test]
    async fn test_assembly_active_skill_enters_manifest_and_read_skill() {
        // active 项(经 REST approve)→ 进 manifest + read_skill 可读
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "data/skills", "git-discipline", CLEAN_SKILL);
        let app = router(make_state(tmp.path().to_path_buf()));
        let (status, _) = post(
            app,
            "/api/skills/approve",
            serde_json::json!({"name": "git-discipline", "level": "user"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (has_read_skill, manifest) = assemble(tmp.path(), vec![]);
        assert!(has_read_skill, "active skill must register read_skill");
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest[0].name, "git-discipline");
        assert_eq!(manifest[0].description, "git 纪律指引");
    }

    #[tokio::test]
    async fn test_assembly_discovered_skill_absent_everywhere() {
        // discovered 项(扫到未审)→ manifest 无 + read_skill 不注册
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "data/skills", "not-approved", CLEAN_SKILL);
        let (has_read_skill, manifest) = assemble(tmp.path(), vec![]);
        assert!(
            !has_read_skill,
            "discovered skill must NOT register read_skill"
        );
        assert!(
            manifest.is_empty(),
            "discovered skill must NOT enter manifest"
        );
    }

    #[tokio::test]
    async fn test_assembly_declared_entry_independent_of_registry() {
        // definition 声明项(B2 语义不变)→ 不经账本直接生效
        let tmp = tempfile::tempdir().unwrap();
        let declared_dir = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&declared_dir).unwrap();
        let declared_path = declared_dir.join("SKILL.md");
        std::fs::write(&declared_path, CLEAN_SKILL).unwrap();
        let declared = vec![crate::agent::definition::SkillManifestEntry {
            name: "git-discipline".to_string(),
            path: declared_path,
            description: "declared".to_string(),
        }];
        let (has_read_skill, manifest) = assemble(tmp.path(), declared);
        assert!(has_read_skill);
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest[0].description, "declared");
    }

    #[tokio::test]
    async fn test_assembly_declared_beats_registered_on_name_conflict() {
        // 同名冲突:declaration > 用户级注册项(高优先保留 + warn 不 fail-fast)
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "data/skills", "git-discipline", CLEAN_SKILL);
        let app = router(make_state(tmp.path().to_path_buf()));
        let (status, _) = post(
            app,
            "/api/skills/approve",
            serde_json::json!({"name": "git-discipline", "level": "user"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let declared_dir = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&declared_dir).unwrap();
        let declared_path = declared_dir.join("SKILL.md");
        std::fs::write(
            &declared_path,
            CLEAN_SKILL.replace("git 纪律指引", "declared wins"),
        )
        .unwrap();
        let declared = vec![crate::agent::definition::SkillManifestEntry {
            name: "git-discipline".to_string(),
            path: declared_path.clone(),
            description: "declared wins".to_string(),
        }];
        let (has_read_skill, manifest) = assemble(tmp.path(), declared);
        assert!(has_read_skill);
        assert_eq!(manifest.len(), 1, "conflict resolves to one entry");
        assert_eq!(manifest[0].path, declared_path, "declaration must win");
    }

    #[tokio::test]
    async fn test_assembly_project_level_active_enters_manifest() {
        // 项目级 active 项同样进 manifest(两级目录均扫描)
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), ".evo/skills", "proj-flow", CLEAN_SKILL);
        let app = router(make_state(tmp.path().to_path_buf()));
        let (status, _) = post(
            app,
            "/api/skills/approve",
            serde_json::json!({"name": "proj-flow", "level": "project"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (has_read_skill, manifest) = assemble(tmp.path(), vec![]);
        assert!(has_read_skill);
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest[0].name, "proj-flow");
    }

    // =========================================================================
    // bundle 导入（F2-D1 闭环：导入→discovered→approve 全链）
    // =========================================================================

    /// 写 bundle 文件到工作区并返回其相对路径
    fn write_bundle(workdir: &std::path::Path, rel: &str, bundle: serde_json::Value) -> String {
        let p = workdir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bundle.to_string()).unwrap();
        rel.to_string()
    }

    #[tokio::test]
    async fn test_import_bundle_lands_discovered_then_approve_activates() {
        // DoD 全链:bundle 导入 → pending 出现 → approve → active
        let tmp = tempfile::tempdir().unwrap();
        let app = human_router(tmp.path().to_path_buf());
        let rel = write_bundle(
            tmp.path(),
            "data/skills/pack.bundle.json",
            serde_json::json!({
                "skills": [
                    {"name": "git-discipline", "level": "user", "body": CLEAN_SKILL}
                ]
            }),
        );

        let (status, body) = post_authed(
            app.clone(),
            "/api/skills/import",
            serde_json::json!({"path": rel}),
            "test-token",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body: {}", body);
        assert_eq!(body["imported"], 1);
        assert_eq!(body["results"][0]["status"], "imported");
        // 落盘形态:<skills_dir>/<name>/SKILL.md
        assert!(tmp
            .path()
            .join("data/skills/git-discipline/SKILL.md")
            .exists());

        // 导入即发现:计算态 discovered,待审批组出现(正文预览随行)
        let (_, inv) = get_authed(app.clone(), "/api/skills", Some("test-token")).await;
        assert_eq!(inv["skills"][0]["status"], "discovered");
        let (_, pending) = get_authed(app.clone(), "/api/skills/pending", Some("test-token")).await;
        assert_eq!(pending["pending"][0]["name"], "git-discipline");

        // 既有审批链照常生效
        let (status, body) = post_authed(
            app.clone(),
            "/api/skills/approve",
            serde_json::json!({"name": "git-discipline", "level": "user"}),
            "test-token",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "active");

        // 已审批内容保护:再导入同名 → rejected(不静默覆盖)
        let (_, body) = post_authed(
            app,
            "/api/skills/import",
            serde_json::json!({"path": rel}),
            "test-token",
        )
        .await;
        assert_eq!(body["imported"], 0);
        assert_eq!(body["results"][0]["status"], "rejected");
        assert!(body["results"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("active"));
    }

    #[tokio::test]
    async fn test_import_rejects_path_traversal_name() {
        let tmp = tempfile::tempdir().unwrap();
        let app = human_router(tmp.path().to_path_buf());
        let rel = write_bundle(
            tmp.path(),
            "evil.bundle.json",
            serde_json::json!({
                "skills": [
                    {"name": "../evil", "level": "user", "body": CLEAN_SKILL}
                ]
            }),
        );
        let (status, body) = post_authed(
            app,
            "/api/skills/import",
            serde_json::json!({"path": rel}),
            "test-token",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "per-item reject, not whole-request");
        assert_eq!(body["imported"], 0);
        assert_eq!(body["results"][0]["status"], "rejected");
        // 越界目录未创建
        assert!(!tmp.path().join("data/skills").join("evil").exists());
    }

    #[tokio::test]
    async fn test_import_rejects_bad_frontmatter_and_bundle_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let app = human_router(tmp.path().to_path_buf());
        // frontmatter 损坏项 rejected
        let rel = write_bundle(
            tmp.path(),
            "bad.bundle.json",
            serde_json::json!({
                "skills": [
                    {"name": "no-fm", "level": "user", "body": "没有围栏的正文"}
                ]
            }),
        );
        let (_, body) = post_authed(
            app.clone(),
            "/api/skills/import",
            serde_json::json!({"path": rel}),
            "test-token",
        )
        .await;
        assert_eq!(body["results"][0]["status"], "rejected");

        // bundle 路径越界 → 400(文件须存在才触发 escape 检查,否则 404)
        let outside = tmp
            .path()
            .parent()
            .unwrap()
            .join("__wuying_escape_test.bundle.json");
        std::fs::write(&outside, "{}").unwrap();
        let (status, body) = post_authed(
            app,
            "/api/skills/import",
            serde_json::json!({"path": "../__wuying_escape_test.bundle.json"}),
            "test-token",
        )
        .await;
        let _ = std::fs::remove_file(&outside);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("escapes"));
    }

    #[tokio::test]
    async fn test_import_anonymous_denied_403() {
        // 写面纪律:anonymous(auth disabled)一律 403,文件未被触碰
        // (对齐 file_api anonymous 拒绝路径;真机已实测 anonymous 拒)
        let tmp = tempfile::tempdir().unwrap();
        let app = router(make_state(tmp.path().to_path_buf())); // 无 auth = anonymous
        let rel = write_bundle(
            tmp.path(),
            "data/skills/anon.bundle.json",
            serde_json::json!({
                "skills": [
                    {"name": "should-not-land", "level": "user", "body": CLEAN_SKILL}
                ]
            }),
        );
        let (status, body) =
            post(app, "/api/skills/import", serde_json::json!({"path": rel})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body["error"].as_str().unwrap().contains("read-only"));
        assert!(
            !tmp.path().join("data/skills/should-not-land").exists(),
            "denied import must not touch skills dir"
        );
    }
}
