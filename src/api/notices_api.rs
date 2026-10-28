// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 公告与展示发布面(批 J1 段 1)。
//!
//! 「发布者 → 用户」单向信息面:发布者编辑 `<workdir>/data/notices/notices.json`
//! 即生效(端点直读,无缓存层);公告图片与单文件 HTML 展示页放
//! `<workdir>/data/notices/assets/`,由 `/notices-static` 静态挂载
//! (evo-agent.rs 组装段,tower-http ServeDir,免鉴权公开面,启动自建目录)。
//!
//! fail-soft 口径(与 file_api::browse 同族):文件缺失/坏 JSON/条目超限
//! 一律不 500,空表或截断 + `error` 字段说明。
//! 本端点不消费 `X-Evo-Workspace` —— 公告是全局面,与工作区无关。

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 条目数上限(超限截断 + error 提示)
pub const MAX_NOTICES: usize = 50;
/// 单条正文字节上限(超限条目剔除 + error 提示)
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// notices.json 文件字节上限(超限整体拒绝,空表 + error)
pub const MAX_FILE_BYTES: usize = 256 * 1024;

/// 公告索引文件路径(`<workdir>/data/notices/notices.json`)
pub fn notices_file(workdir: &Path) -> PathBuf {
    workdir.join("data").join("notices").join("notices.json")
}

/// 公告静态资源目录(`<workdir>/data/notices/assets`;/notices-static 挂载根)
pub fn notices_assets_dir(workdir: &Path) -> PathBuf {
    workdir.join("data").join("notices").join("assets")
}

/// 公告内链接(功能展示页等;前端渲染为新标签页按钮)
#[derive(Debug, Deserialize, Serialize)]
pub struct NoticeLink {
    /// 链接显示文案
    pub label: String,
    /// 目标 URL(如 /notices-static/showcases/xxx.html)
    pub href: String,
}

/// 发布者书写的原始条目(pinned/links 可缺省)
#[derive(Debug, Deserialize)]
pub struct NoticeRaw {
    /// 唯一标识(前端 dismiss 等按 id 定位;本期仅作 key)
    pub id: String,
    /// 发布日期(ISO yyyy-mm-dd;降序排序键)
    pub date: String,
    /// 徽标级别:info | warn | feature(未知值前端兜底为 info)
    #[serde(default)]
    pub level: String,
    /// 标题
    #[serde(default)]
    pub title: String,
    /// 正文(markdown;图片用 /notices-static/ 绝对路径引用)
    #[serde(default)]
    pub body_md: String,
    /// 置顶(排序第一键)
    #[serde(default)]
    pub pinned: bool,
    /// 展示页/外链按钮列表
    #[serde(default)]
    pub links: Vec<NoticeLink>,
}

/// 规范化后的公告条目(响应面)
#[derive(Debug, Serialize)]
pub struct Notice {
    /// 唯一标识
    pub id: String,
    /// 发布日期
    pub date: String,
    /// 徽标级别
    pub level: String,
    /// 标题
    pub title: String,
    /// 正文(markdown)
    pub body_md: String,
    /// 置顶
    pub pinned: bool,
    /// 链接按钮列表
    pub links: Vec<NoticeLink>,
}

/// 响应:排序后的公告列表 + fail-soft 错误说明
#[derive(Debug, Serialize)]
pub struct NoticesResponse {
    /// 排序后的公告(pinned 置顶 → date 降序)
    pub notices: Vec<Notice>,
    /// fail-soft 错误说明(文件超限/坏 JSON/条目剔除/截断;None=正常)
    pub error: Option<String>,
}

impl NoticesResponse {
    fn empty() -> Self {
        NoticesResponse {
            notices: Vec::new(),
            error: None,
        }
    }

    fn with_error(error: impl Into<String>) -> Self {
        NoticesResponse {
            notices: Vec::new(),
            error: Some(error.into()),
        }
    }
}

/// 排序:pinned 置顶 → date 降序(ISO 形态字符串字典序即时间序;非规范日期
/// 字典序沉底)→ 原数组序稳定
fn sort_notices(list: &mut [Notice]) {
    list.sort_by(|a, b| b.pinned.cmp(&a.pinned).then_with(|| b.date.cmp(&a.date)));
}

/// 读取 + 规范化 + 排序 + 上限钳制(纯函数,测试直打)。
///
/// - 文件缺失 → 空表 + `error: None`(未发布态是常态,不算错误);
/// - 文件超限/坏 JSON → 空表 + error;
/// - 单条 body_md 超 `MAX_BODY_BYTES` → 该条剔除,error 计数提示;
/// - 条目数超 `MAX_NOTICES` → 截断,error 提示。
pub fn load_notices(workdir: &Path) -> NoticesResponse {
    let file = notices_file(workdir);
    let raw = match std::fs::read(&file) {
        Ok(bytes) => {
            if bytes.len() > MAX_FILE_BYTES {
                return NoticesResponse::with_error(format!(
                    "notices.json 超过 {}KB 上限,已拒绝渲染",
                    MAX_FILE_BYTES / 1024
                ));
            }
            bytes
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return NoticesResponse::empty(),
        Err(e) => return NoticesResponse::with_error(format!("读取失败: {e}")),
    };
    let parsed: Vec<NoticeRaw> = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(e) => {
            return NoticesResponse::with_error(format!("notices.json 解析失败: {e}"));
        }
    };
    let mut dropped_oversize = 0usize;
    let mut notices: Vec<Notice> = parsed
        .into_iter()
        .filter_map(|n| {
            if n.body_md.len() > MAX_BODY_BYTES {
                dropped_oversize += 1;
                return None;
            }
            Some(Notice {
                id: n.id,
                date: n.date,
                level: n.level,
                title: n.title,
                body_md: n.body_md,
                pinned: n.pinned,
                links: n.links,
            })
        })
        .collect();
    sort_notices(&mut notices);
    if notices.len() > MAX_NOTICES {
        notices.truncate(MAX_NOTICES);
        return NoticesResponse {
            error: Some(format!("公告条目超过 {MAX_NOTICES} 条上限,已截断")),
            notices,
        };
    }
    if dropped_oversize > 0 {
        return NoticesResponse {
            error: Some(format!("{dropped_oversize} 条公告正文超限被剔除")),
            notices,
        };
    }
    NoticesResponse {
        notices,
        error: None,
    }
}

/// `GET /api/notices` —— 公告列表(批 J1 段 1;鉴权同文件面,G16 中间件覆盖)
pub async fn list_notices(State(state): State<crate::api::AgentApiState>) -> Json<NoticesResponse> {
    Json(load_notices(state.workdir()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_fixture(dir: &Path, json: &str) -> PathBuf {
        let notices_dir = dir.join("data").join("notices");
        fs::create_dir_all(&notices_dir).unwrap();
        let file = notices_dir.join("notices.json");
        fs::write(&file, json).unwrap();
        file
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("evo-notices-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn normal_read_sorts_pinned_then_date_desc() {
        let dir = tmpdir("sort");
        write_fixture(
            &dir,
            r#"[
            {"id":"a","date":"2026-10-01","title":"旧","body_md":"x"},
            {"id":"b","date":"2026-10-20","title":"新","body_md":"x"},
            {"id":"c","date":"2026-10-05","title":"置顶旧","body_md":"x","pinned":true}
        ]"#,
        );
        let resp = load_notices(&dir);
        assert!(resp.error.is_none());
        let ids: Vec<&str> = resp.notices.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "b", "a"]);
        assert!(resp.notices[0].pinned);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_empty_with_no_error() {
        let dir = tmpdir("missing");
        let resp = load_notices(&dir);
        assert!(resp.notices.is_empty());
        assert!(resp.error.is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_json_fails_soft_with_error() {
        let dir = tmpdir("broken");
        write_fixture(&dir, "[{not json");
        let resp = load_notices(&dir);
        assert!(resp.notices.is_empty());
        assert!(resp.error.as_deref().unwrap().contains("解析失败"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oversize_entries_dropped_and_count_truncated() {
        let dir = tmpdir("limit");
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        let mut items = vec![format!(
            r#"{{"id":"big","date":"2026-10-21","title":"超限","body_md":"{big}"}}"#
        )];
        for i in 0..(MAX_NOTICES + 5) {
            items.push(format!(
                r#"{{"id":"n{i}","date":"2026-10-20","title":"t{i}","body_md":"ok"}}"#
            ));
        }
        write_fixture(&dir, &format!("[{}]", items.join(",")));
        let resp = load_notices(&dir);
        assert_eq!(resp.notices.len(), MAX_NOTICES);
        assert!(resp.error.as_deref().unwrap().contains("截断"));
        assert!(resp.notices.iter().all(|n| n.id != "big"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn response_never_leaks_workdir_path() {
        let dir = tmpdir("leak");
        write_fixture(
            &dir,
            r#"[{"id":"a","date":"2026-10-21","title":"t","body_md":"b","links":[{"label":"L","href":"/notices-static/showcases/x.html"}]}]"#,
        );
        let resp = load_notices(&dir);
        let serialized = serde_json::to_string(&resp).unwrap();
        assert!(!serialized.contains(dir.to_string_lossy().as_ref()));
        assert!(serialized.contains("/notices-static/showcases/x.html"));
        fs::remove_dir_all(&dir).ok();
    }

    // ===== 公开静态面收敛验收(bin 装配同款:/notices-static → ServeDir(assets))=====

    mod static_face {
        use super::*;
        use axum::{http::Request, routing::get};
        use tower::ServiceExt;

        /// bin serve 装配同款形态:nest_service(ServeDir(assets_root))
        fn test_app(assets_root: &Path) -> axum::Router {
            axum::Router::new()
                .route("/health", get(|| async { "ok" }))
                .nest_service(
                    "/notices-static",
                    tower_http::services::ServeDir::new(assets_root),
                )
        }

        async fn status_for(app: axum::Router, uri: &str) -> u16 {
            app.oneshot(
                Request::builder()
                    .uri(uri)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
            .as_u16()
        }

        #[tokio::test]
        async fn public_face_serves_assets_and_rejects_traversal() {
            let dir = tmpdir("static");
            let assets = notices_assets_dir(&dir);
            fs::create_dir_all(&assets).unwrap();
            fs::write(assets.join("hello.html"), "<html>ok</html>").unwrap();
            // 边界外文件:同属 data/notices 树,但不在 assets 挂载根内
            fs::create_dir_all(dir.join("data").join("notices")).unwrap();
            fs::write(dir.join("data").join("notices").join("notices.json"), "[]").unwrap();

            let app = test_app(&assets);
            // 免鉴权公开面:发布者主动放置的公开物,浏览器直链可取
            assert_eq!(
                status_for(app.clone(), "/notices-static/hello.html").await,
                200
            );
            // 目录收敛边界:挂载根外文件(含同树上层的 notices.json)不可达
            assert_eq!(
                status_for(app.clone(), "/notices-static/notices.json").await,
                404
            );
            // 穿越载荷:URL 编码的 .. 不得逃出挂载根
            assert_eq!(
                status_for(app.clone(), "/notices-static/%2e%2e/notices.json").await,
                404
            );
            assert_eq!(
                status_for(app, "/notices-static/../../secret.txt").await,
                404
            );
            fs::remove_dir_all(&dir).ok();
        }
    }
}
