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
//!
//! 工作台管理 UI 留 IDE 化批；本面 REST 即可操作（curl / REST 消费面先例充分）。
//! 鉴权随路由面（`/api/skills*` 不进 PUBLIC_PATHS，默认 Bearer token 内）。

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};

use crate::agent::skill_store::{
    check_skill_leaks, hash_skill_file_prefixed, merge_skill_manifest, parse_skill_body,
    scan_skills_dir, SkillLevel, SkillRegistry, SkillStatus,
};
use crate::api::agent_api::AgentApiState;

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
    pub name: String,
    pub level: String,
    pub path: String,
    pub status: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SkillInventoryResponse {
    pub skills: Vec<SkillView>,
}

/// 待审项（GET /api/skills/pending 元素）：discovered + 正文全文预览
/// （审批前必读内容——人工过目 = 供应链闸口）
#[derive(Debug, Serialize)]
pub struct PendingSkillView {
    pub name: String,
    pub level: String,
    pub path: String,
    pub description: String,
    /// SKILL.md 全文（人工过目用）
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct PendingSkillsResponse {
    pub pending: Vec<PendingSkillView>,
}

#[derive(Debug, Deserialize)]
pub struct SkillNameRequest {
    pub name: String,
    /// "user" | "project"
    pub level: String,
    /// 操作者（可选；v1 无操作者身份体系，缺省 local-operator）
    #[serde(default)]
    pub operator: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApproveResponse {
    pub name: String,
    pub level: String,
    pub status: String,
    pub content_hash: String,
    /// 警告级命中（技术词——合法产品内容，可批；明示供操作者裁量留痕）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::agent::skill_store::LeakHit>,
}

#[derive(Debug, Serialize)]
pub struct LeakRejectResponse {
    pub rejected: bool,
    pub reason: String,
    /// 硬拦命中明细（署名泄露族）
    pub hits: Vec<crate::agent::skill_store::LeakHit>,
}

#[derive(Debug, Serialize)]
pub struct RevokeResponse {
    pub name: String,
    pub level: String,
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
    let mut push_scanned = |s: &crate::agent::skill_store::ScannedSkill,
                            views: &mut Vec<SkillView>| {
        let (status, content_hash, approved_at, approved_by) =
            match registry.find(&s.name, s.level) {
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
pub async fn list_skills(State(state): State<AgentApiState>) -> Result<Json<SkillInventoryResponse>, (StatusCode, Json<LeakRejectResponse>)> {
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
    registry.approve(&req.name, s.path.clone(), level, content_hash.clone(), &operator);
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
}

/// 会话创建装配链入口（PR-3 接线用）：扫描 + 账本核对 + 两源合并的
/// serve 侧封装（目录解析单点；merge 为纯函数，IDE 挂接未来直接调它）
pub fn merged_manifest_for_session(
    state: &AgentApiState,
    declared: Vec<crate::agent::definition::SkillManifestEntry>,
) -> Result<(Vec<crate::agent::definition::SkillManifestEntry>, std::path::PathBuf), String> {
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
            Arc::new(ToolHandler::new()),
        )
    }

    fn write_skill(workdir: &std::path::Path, level_dir: &str, name: &str, body: &str) {
        let sub = workdir.join(level_dir).join(name);
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("SKILL.md"), body).unwrap();
    }

    async fn get(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap(),
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
            &axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap(),
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
        assert!(pending[0]["content"].as_str().unwrap().contains("git status"));
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
            "---\nname: leaky\ndescription: d\n---\n# 正文\n本文由 AI 生成，请放心使用。\n",
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
        assert!(hits[0]["line"].as_str().unwrap().contains("由 AI 生成"));
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
        assert_eq!(status, StatusCode::OK, "technical terms warn but do not block");
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
            inv["skills"][0]["content_hash"].is_null() == false,
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
}
