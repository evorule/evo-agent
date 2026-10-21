// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 工作台文件 REST 面 —— IDE 文件树浏览与编辑器保存的消费端点
//!
//! 九个端点,全部复用 builtin_tools 的 file 工具实现(同一沙箱 / 同一校验 /
//! 同一资源限制),不在本模块重复任何路径安全逻辑:
//!
//! - `GET /api/files/list?dir=`  → [`FileListTool`](crate::builtin_tools::file_list::FileListTool)
//!   (union toolkit 内**同一实例**)
//! - `GET /api/files/read?path=` → [`FileReadTool`](crate::builtin_tools::file_read::FileReadTool)
//!   (union toolkit 内**同一实例**)
//! - `PUT /api/files/write`      → [`FileWriteTool`](crate::builtin_tools::file_write::FileWriteTool)
//!   (同实现,`writable_dir="."`:人工编辑面 = workdir 全域)
//! - `POST /api/files/create`    → [`FileCreateTool`](crate::builtin_tools::file_create::FileCreateTool)
//!   (同实现,`writable_dir="."`;持树写互斥锁)
//! - `POST /api/files/move`      → [`FileMoveTool`](crate::builtin_tools::file_move::FileMoveTool)
//!   (同实现,`writable_dir="."`;持树写互斥锁)
//! - `DELETE /api/files?path=`   → [`FileDeleteTool`](crate::builtin_tools::file_delete::FileDeleteTool)
//!   (同实现,软删除进 `.evo-trash`;持树写互斥锁)
//! - `POST /api/files/search`    → [`grep_core`](crate::builtin_tools::grep_files::grep_core)
//!   (全项目内容搜索;沙箱同款;30s 硬超时 partial)
//! - `POST /api/files/replace`   → [`replace_core`](crate::builtin_tools::grep_files::replace_core)
//!   (apply=false 预览零写盘 / apply=true 重匹配+原子写;核心层内部持树写互斥锁)
//! - `POST /api/files/upload`    → base64 解码直写 `.evo-agent/uploads/`(对话附件
//!   上传;二进制不走 file 工具文本通道;固定子目录无用户可控路径段)
//!
//! ## 语义边界(与 agent 工具通道的差别,设计定稿留痕)
//!
//! - 本面服务的是**人**在工作台编辑器中的直接操作(打开 / 编辑 / 保存落盘)。
//!   人的 UI 操作不构造 agent 会话事实,因此**不进 agent 会话审计链**——
//!   审计链记录的是 agent 执行行为,把人的编辑操作伪造成 agent 事实反而污染
//!   审计真实性;写面操作落**人工审计账**([`human_gate`],独立 JSONL,
//!   两本账可经时间戳关联);
//! - 写面身份门(HumanGate):human(auth token 校验通过)可写并落人工账;
//!   anonymous(auth disabled)**只读**——写面一律 403,响应体带开启指引,
//!   拒绝本身同样落人工账(本地回环 + auth=false 的过渡宽限口径见 human_gate
//!   模块文档);
//! - agent 路径的 `file_write`(`writable_dir=workspace` + 管道⑤ candidate
//!   审批 + call_external 过引擎入审计链)**完全不受影响**,两个消费面互不干涉;
//! - 写面差异是主体差异的忠实反映:agent 只能写 `workspace/`(防误写源码),
//!   人(项目方)在本机本就有完整文件系统权限,workdir 沙箱对人是 UX 边界
//!   (防误操作出 workdir),不是安全边界。

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

use crate::api::agent_api::AgentApiState;
use crate::api::human_gate::{ensure_write_allowed, CallerIdentity};
use crate::api::serve_tools::simplify_verbatim;
use crate::builtin_tools::file_create::FileCreateTool;
use crate::builtin_tools::file_delete::FileDeleteTool;
use crate::builtin_tools::file_list::FileListTool;
use crate::builtin_tools::file_move::FileMoveTool;
use crate::builtin_tools::file_read::FileReadTool;
use crate::builtin_tools::file_write::FileWriteTool;
use crate::builtin_tools::fs_safety::tree_mutation_lock;
use crate::io_handlers::tool_handler::ToolFunction;

/// 会话边界根请求头(批 I 文件面重锚):值 = 会话工作区绝对路径
/// (与连接级 `?workspace=` 同源,前端对文件面请求统一注入)。
pub const WORKSPACE_HEADER: &str = "x-evo-workspace";

/// 会话边界根解析(批 I 文件面重锚):
/// - 头缺省/空 → serve workdir(**严格模式行为零变化**,既有测试锁护航);
/// - 显式值 → 须为绝对路径(相对路径 400 拒绝),作为本请求的沙箱基准。
///
/// 安全语义:fs_safety 的越界拒绝 / `..` 上跳拒绝 / `\\?\` verbatim 处理 /
/// 守护目标防线全部不变,仅「根的尺寸」由会话工作区显式选定——与 agent
/// 通道 CapabilityBoundary 同语义(sandbox_root 即工作区),人工编辑面的
/// workdir 沙箱定位本是 UX 边界(见模块文档),安全边界由会话能力边界与
/// 鉴权承担。upload 端点不消费本头(附件固定落 workdir 产品存储面)。
fn resolve_fs_root(
    state: &AgentApiState,
    header: Option<&HeaderValue>,
) -> Result<PathBuf, (StatusCode, String)> {
    let raw = match header {
        None => return Ok(state.workdir().to_path_buf()),
        Some(h) => h
            .to_str()
            .map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    "X-Evo-Workspace must be valid ASCII".to_string(),
                )
            })?
            .trim(),
    };
    if raw.is_empty() {
        return Ok(state.workdir().to_path_buf());
    }
    let p = PathBuf::from(raw);
    if !p.is_absolute() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("X-Evo-Workspace must be an absolute path, got: {raw}"),
        ));
    }
    Ok(p)
}

/// 读面工具 manifest 在场检查(PR-3 入口收口的运行期镜像,现场构造路径
/// 同样适用:无 manifest 的工具拒载,防读面越界能力域)。
fn ensure_manifest(state: &AgentApiState, name: &str) -> Result<(), (StatusCode, String)> {
    if state.toolkit().manifest(name).is_none() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "tool '{name}' has no manifest (NoManifest) — refusing to invoke \
                 (entry-point closure: read surface must not reach unmanifested tools)"
            ),
        ));
    }
    Ok(())
}

/// `GET /api/files/list` 查询参数
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// 相对 workdir 的目录路径(缺省 = workdir 根)
    pub dir: Option<String>,
}

/// `GET /api/files/read` 查询参数
#[derive(Debug, Deserialize)]
pub struct ReadQuery {
    /// 相对 workdir 的文件路径
    pub path: String,
}

/// `PUT /api/files/write` 请求体
#[derive(Debug, Deserialize)]
pub struct WriteBody {
    /// 相对 workdir 的文件路径
    pub path: String,
    /// 完整文件内容(整体覆盖写)
    pub content: String,
}

/// `POST /api/files/create` 请求体
#[derive(Debug, Deserialize)]
pub struct CreateBody {
    /// 相对 workdir 的创建目标路径(必须不存在)
    pub path: String,
    /// `file` | `dir`(缺省 file)
    pub kind: Option<String>,
}

/// `POST /api/files/move` 请求体
#[derive(Debug, Deserialize)]
pub struct MoveBody {
    /// 相对 workdir 的源路径(必须已存在)
    pub path: String,
    /// 相对 workdir 的目标目录(必须已存在)
    pub target_dir: String,
    /// 目标名(缺省 = 保留原名)
    pub new_name: Option<String>,
}

/// `DELETE /api/files` 查询参数
#[derive(Debug, Deserialize)]
pub struct DeleteQuery {
    /// 相对 workdir 的删除目标(软删除进 `.evo-trash`)
    pub path: String,
}

/// `POST /api/files/upload` 请求体
#[derive(Debug, Deserialize)]
pub struct UploadBody {
    /// 原始文件名(仅取基名,净化后落盘)
    pub name: String,
    /// 文件内容 base64(标准字母表,可含 padding)
    pub data_base64: String,
}

/// 附件上传解码上限(字节)
const UPLOAD_MAX_BYTES: usize = 10 * 1024 * 1024;
/// 附件落盘子目录(相对 workdir;agent 文件工具同沙箱根,可读)
const UPLOAD_DIR: &str = ".evo-agent/uploads";

/// 扩展名 → MIME(附件块标注用;未知回落 application/octet-stream)
fn upload_mime(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "json" => "application/json",
        "txt" | "md" | "log" | "csv" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// 文件名净化:取基名,白名单字符(其余替为 `_`),截长 80,去首尾空白与
/// 尾部点/空格(Windows 文件名约束),空/`.`/`..` 兜底 `file`
fn sanitize_upload_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let kept: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | ' ' | '(' | ')') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed: String = kept.trim().chars().take(80).collect();
    let trimmed = trimmed.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "file".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 工具错误字符串 → HTTP 状态码
/// (不存在的路径 404 / 已存在 409 / 其余 400)
fn err_status(msg: &str) -> StatusCode {
    if msg.contains("does not exist") {
        StatusCode::NOT_FOUND
    } else if msg.contains("already exists") {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    }
}

/// 调 union toolkit 中的 file 工具(list/read 与 agent 同一实例)
///
/// PR-3 入口收口(读面):直读保留(无写风险),但调用前查 manifest 在场——
/// 无 manifest 的工具拒载(NoManifest 显式拒绝,防读面越界能力域;注册期
/// manifest 强制不变量的运行期镜像面)。
async fn call_toolkit_tool(
    state: &AgentApiState,
    name: &str,
    args: Value,
) -> Result<Value, (StatusCode, String)> {
    let tool = state.toolkit().get_tool(name).ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("tool not registered in union toolkit: {name}"),
        )
    })?;
    if state.toolkit().manifest(name).is_none() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "tool '{name}' has no manifest (NoManifest) — refusing to invoke \
                 (entry-point closure: read surface must not reach unmanifested tools)"
            ),
        ));
    }
    tool.call(&args).await.map_err(|e| (err_status(&e), e))
}

/// HumanGate 写面准入门:human 放行;anonymous 拒绝(403+开启指引)且
/// 拒绝本身落人工账(outcome=denied)。
fn gate_write(
    state: &AgentApiState,
    op: &str,
    path: &str,
) -> Result<CallerIdentity, (StatusCode, String)> {
    match ensure_write_allowed(state.auth_config()) {
        Ok(identity) => Ok(identity),
        Err(e) => {
            state
                .human_gate()
                .record(op, CallerIdentity::Anonymous, path, "denied");
            Err(e)
        }
    }
}

/// 写面操作结局落人工账(执行成功 ok / 失败 error)
fn record_outcome(
    state: &AgentApiState,
    op: &str,
    identity: CallerIdentity,
    path: &str,
    result: &Result<Value, (StatusCode, String)>,
) {
    let outcome = if result.is_ok() { "ok" } else { "error" };
    state.human_gate().record(op, identity, path, outcome);
}

/// `GET /api/files/list` —— 列目录(委托 file_list 工具)
///
/// `X-Evo-Workspace` 头在场时以该绝对路径为基准现场构造(同实现同校验);
/// 缺省走 union toolkit 共享实例(严格模式行为零变化)。
/// 错误 → 404(目录不存在)/ 400(路径越界等)。
pub async fn list_dir(
    State(state): State<AgentApiState>,
    Query(q): Query<ListQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let root = resolve_fs_root(&state, headers.get(WORKSPACE_HEADER))?;
    let mut args = serde_json::Map::new();
    if let Some(d) = &q.dir {
        args.insert("dir".to_string(), Value::from(d.as_str()));
    }
    let v = if headers.contains_key(WORKSPACE_HEADER) {
        ensure_manifest(&state, "file_list")?;
        FileListTool::new(root)
            .call(&Value::Object(args))
            .await
            .map_err(|e| (err_status(&e), e))?
    } else {
        call_toolkit_tool(&state, "file_list", Value::Object(args)).await?
    };
    Ok(Json(v))
}

/// `GET /api/files/read` —— 读文件(委托 file_read 工具)
///
/// `X-Evo-Workspace` 重锚语义同 list_dir。
/// 错误 → 404(文件不存在)/ 400(路径越界 / 过大 / 非常规文件)。
pub async fn read_file(
    State(state): State<AgentApiState>,
    Query(q): Query<ReadQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let root = resolve_fs_root(&state, headers.get(WORKSPACE_HEADER))?;
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), Value::from(q.path.as_str()));
    let v = if headers.contains_key(WORKSPACE_HEADER) {
        ensure_manifest(&state, "file_read")?;
        FileReadTool::new(root)
            .call(&Value::Object(args))
            .await
            .map_err(|e| (err_status(&e), e))?
    } else {
        call_toolkit_tool(&state, "file_read", Value::Object(args)).await?
    };
    Ok(Json(v))
}

/// `PUT /api/files/write` —— 写文件(委托 file_write 实现,`writable_dir="."`)
///
/// IDE 保存语义:已打开的文件必然已存在,`overwrite=true` 即「保存覆盖」。
/// HumanGate:human 可写(落人工账);anonymous 403(拒绝留痕)。
/// 错误 → 403(anonymous)/ 400(路径越界 / 内容过大)。
pub async fn write_file(
    State(state): State<AgentApiState>,
    headers: HeaderMap,
    Json(body): Json<WriteBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let root = resolve_fs_root(&state, headers.get(WORKSPACE_HEADER))?;
    let identity = gate_write(&state, "file_write", &body.path)?;
    let tool = FileWriteTool::new(root).with_writable_dir(".");
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), Value::from(body.path.as_str()));
    args.insert("content".to_string(), Value::from(body.content.as_str()));
    args.insert("overwrite".to_string(), Value::Bool(true));
    args.insert("create_parents".to_string(), Value::Bool(true));
    let result = tool
        .call(&Value::Object(args))
        .await
        .map_err(|e| (err_status(&e), e));
    record_outcome(&state, "file_write", identity, &body.path, &result);
    result.map(Json)
}

/// `POST /api/files/upload` —— 附件上传(base64 解码落盘)
///
/// 语义:人把本地文件(图片/文档等)作为对话附件送入工作区。落盘固定在
/// `{workdir}/.evo-agent/uploads/<UTC毫秒时间戳>-<净化名>`,在 workdir 沙箱内,
/// agent 文件工具同根可读;二进制直写(file 工具走文本通道,不适用)。
/// 不持树写互斥锁:仅向专用子目录增量新增(时间戳文件名),与 write_file
/// 同类(非树结构改组)。HumanGate:human 可写(落人工账 op=file_upload);
/// anonymous 403(拒绝留痕)。
/// 错误 → 403(anonymous)/ 400(base64 非法)/ 413(超 10MB)。
pub async fn upload_file(
    State(state): State<AgentApiState>,
    Json(body): Json<UploadBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let clean = sanitize_upload_name(&body.name);
    let rel = format!(
        "{UPLOAD_DIR}/{}-{clean}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S%3f")
    );
    let identity = gate_write(&state, "file_upload", &rel)?;
    // base64 长度预检(解码前粗检,防无谓大分配)+ 解码后精确校验
    if body.data_base64.len() > UPLOAD_MAX_BYTES / 3 * 4 + 4 {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("upload exceeds {} bytes", UPLOAD_MAX_BYTES),
        ));
    }
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body.data_base64.trim())
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid base64: {e}")))?;
    if bytes.len() > UPLOAD_MAX_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("upload exceeds {} bytes", UPLOAD_MAX_BYTES),
        ));
    }
    let full = state.workdir().join(&rel);
    // create_parents:仅固定子目录段,无用户可控路径,无越界面
    let result = std::fs::create_dir_all(full.parent().unwrap_or(&full))
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("mkdir failed: {e}"),
            )
        })
        .and_then(|()| {
            std::fs::write(&full, &bytes)
                .map(|_| {
                    serde_json::json!({
                        "name": clean,
                        "path": rel,
                        "size": bytes.len(),
                        "mime": upload_mime(&clean),
                    })
                })
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("write failed: {e}"),
                    )
                })
        });
    record_outcome(&state, "file_upload", identity, &rel, &result);
    result.map(Json)
}

/// `GET /api/fs/browse` 查询参数
#[derive(Debug, Deserialize)]
pub struct BrowseQuery {
    /// 要列的目录绝对路径(缺省 = 列文件系统根:Windows 盘符 / POSIX 根)
    pub path: Option<String>,
}

/// 目录浏览条目(只列子目录——本端点服务于「选工作区」目录选择器)
#[derive(Debug, Serialize)]
pub struct BrowseEntry {
    /// 目录名
    pub name: String,
    /// 目录绝对路径(规整形态,可直接回填下一跳 path 参数)
    pub path: String,
}

/// 目录浏览响应
#[derive(Debug, Serialize)]
pub struct BrowseResponse {
    /// 当前目录绝对路径(根列表模式 = None)
    pub path: Option<String>,
    /// 父目录绝对路径(已是根 = None,面包屑上移用)
    pub parent: Option<String>,
    /// true = 盘符/根列表模式(path 缺省)
    pub roots: bool,
    /// 子目录列表(只列目录)
    pub entries: Vec<BrowseEntry>,
    /// 读取失败信息(权限等;fail-soft 不 500,空表 + 本字段)
    pub error: Option<String>,
}

/// Windows/POSIX 根列表(path 缺省态)
fn list_roots() -> BrowseResponse {
    #[cfg(windows)]
    {
        let entries = (b'A'..=b'Z')
            .map(|c| c as char)
            .filter(|letter| std::path::Path::new(&format!("{letter}:\\")).exists())
            .map(|letter| BrowseEntry {
                name: format!("{letter}:"),
                path: format!("{letter}:\\"),
            })
            .collect();
        BrowseResponse {
            path: None,
            parent: None,
            roots: true,
            entries,
            error: None,
        }
    }
    #[cfg(not(windows))]
    {
        let (entries, error) = match std::fs::read_dir("/") {
            Ok(rd) => {
                let mut es: Vec<BrowseEntry> = rd
                    .flatten()
                    .filter(|e| e.path().is_dir())
                    .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                    .map(|e| BrowseEntry {
                        name: e.file_name().to_string_lossy().into_owned(),
                        path: format!("/{}", e.file_name().to_string_lossy()),
                    })
                    .collect();
                es.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                (es, None)
            }
            Err(e) => (Vec::new(), Some(format!("read_dir failed: {e}"))),
        };
        BrowseResponse {
            path: Some("/".to_string()),
            parent: None,
            roots: true,
            entries,
            error,
        }
    }
}

/// `GET /api/fs/browse` —— 目录浏览(批 I 段 2,首页/工作台目录选择器消费面)
///
/// 语义:
/// - `path` 缺省 → 列文件系统根(Windows = 存在的盘符;POSIX = `/`),`roots=true`;
/// - `path` 显式 → 必须绝对路径(相对 400),必须存在(404)且为目录(400),
///   返回其**子目录**列表(只列目录,不触文件内容);
/// - 目录读取失败(权限等)fail-soft:空表 + `error` 字段;
/// - 路径返回形态统一 canonicalize + 去 verbatim(前端直接可用)。
///
/// 定位与文件面一致:人的 UI 操作(选工作区),UX 边界非安全边界——本端点
/// 只暴露目录名与目录路径,不读任何文件内容;鉴权由路由层 G16 中间件统一
/// 承担。不消费 `X-Evo-Workspace`(本端点的使命就是自由选工作区)。
pub async fn browse_dirs(
    Query(q): Query<BrowseQuery>,
) -> Result<Json<BrowseResponse>, (StatusCode, String)> {
    let raw = match q.path.as_deref() {
        None | Some("") => return Ok(Json(list_roots())),
        Some(p) => p.trim(),
    };
    let p = PathBuf::from(raw);
    if !p.is_absolute() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("path must be an absolute directory, got: {raw}"),
        ));
    }
    let canon = simplify_verbatim(
        p.canonicalize()
            .map_err(|_| (StatusCode::NOT_FOUND, format!("path does not exist: {raw}")))?,
    );
    if !canon.is_dir() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("path is not a directory: {}", canon.display()),
        ));
    }
    let parent = canon.parent().map(|pp| {
        simplify_verbatim(pp.to_path_buf())
            .to_string_lossy()
            .into_owned()
    });
    let (mut entries, error) = match std::fs::read_dir(&canon) {
        Ok(rd) => {
            let mut es: Vec<BrowseEntry> = rd
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| BrowseEntry {
                    name: e.file_name().to_string_lossy().into_owned(),
                    path: simplify_verbatim(canon.join(e.file_name()))
                        .to_string_lossy()
                        .into_owned(),
                })
                .collect();
            es.sort_by_key(|a| a.name.to_lowercase());
            (es, None)
        }
        Err(e) => (Vec::new(), Some(format!("read_dir failed: {e}"))),
    };
    entries.retain(|e| !e.name.starts_with('.'));
    Ok(Json(BrowseResponse {
        path: Some(canon.to_string_lossy().into_owned()),
        parent,
        roots: false,
        entries,
        error,
    }))
}

/// `POST /api/files/create` —— 创建文件/目录(委托 file_create 实现,`writable_dir="."`)
///
/// 人工面语义:人工面不走 agent 审批(人即最终审批者,REST 面无 agent 会话
/// 审批通道),`create_parents=true` 与写面同语义(IDE 保存即自动建父目录)。
/// HumanGate:human 可写(落人工账);anonymous 403(拒绝留痕)。
/// 持树写互斥锁串行执行。
/// 错误 → 403(anonymous)/ 409(已存在)/ 400(越界 / 保留名 / 非法字符 / kind 非法)。
pub async fn create_file(
    State(state): State<AgentApiState>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let root = resolve_fs_root(&state, headers.get(WORKSPACE_HEADER))?;
    let identity = gate_write(&state, "file_create", &body.path)?;
    let _guard = tree_mutation_lock().lock().await;
    let tool = FileCreateTool::new(root).with_writable_dir(".");
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), Value::from(body.path.as_str()));
    if let Some(kind) = &body.kind {
        args.insert("kind".to_string(), Value::from(kind.as_str()));
    }
    args.insert("create_parents".to_string(), Value::Bool(true));
    let result = tool
        .call(&Value::Object(args))
        .await
        .map_err(|e| (err_status(&e), e));
    record_outcome(&state, "file_create", identity, &body.path, &result);
    result.map(Json)
}

/// `POST /api/files/move` —— 移动/重命名(委托 file_move 实现,`writable_dir="."`)
///
/// 源与目标目录都必须已存在;移入自身子树被拒。
/// HumanGate:human 可写(落人工账);anonymous 403(拒绝留痕)。
/// 持树写互斥锁串行执行。
/// 错误 → 403(anonymous)/ 404(源或目标目录不存在)/ 400(越界 / 保留名 / 移入自身子树)。
pub async fn move_file(
    State(state): State<AgentApiState>,
    headers: HeaderMap,
    Json(body): Json<MoveBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let root = resolve_fs_root(&state, headers.get(WORKSPACE_HEADER))?;
    let identity = gate_write(&state, "file_move", &body.path)?;
    let _guard = tree_mutation_lock().lock().await;
    let tool = FileMoveTool::new(root).with_writable_dir(".");
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), Value::from(body.path.as_str()));
    args.insert(
        "target_dir".to_string(),
        Value::from(body.target_dir.as_str()),
    );
    if let Some(new_name) = &body.new_name {
        args.insert("new_name".to_string(), Value::from(new_name.as_str()));
    }
    let result = tool
        .call(&Value::Object(args))
        .await
        .map_err(|e| (err_status(&e), e));
    record_outcome(&state, "file_move", identity, &body.path, &result);
    result.map(Json)
}

/// `DELETE /api/files` —— 删除(软删除进 `.evo-trash`;委托 file_delete 实现)
///
/// workdir 根 / writable 根 / 回收目录自身三类守护目标拒绝。
/// HumanGate:human 可写(落人工账);anonymous 403(拒绝留痕)。
/// 持树写互斥锁串行执行。
/// 错误 → 403(anonymous)/ 404(目标不存在)/ 400(越界 / 守护目标)。
pub async fn delete_file(
    State(state): State<AgentApiState>,
    Query(q): Query<DeleteQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, String)> {
    let root = resolve_fs_root(&state, headers.get(WORKSPACE_HEADER))?;
    let identity = gate_write(&state, "file_delete", &q.path)?;
    let _guard = tree_mutation_lock().lock().await;
    let tool = FileDeleteTool::new(root).with_writable_dir(".");
    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), Value::from(q.path.as_str()));
    let result = tool
        .call(&Value::Object(args))
        .await
        .map_err(|e| (err_status(&e), e));
    record_outcome(&state, "file_delete", identity, &q.path, &result);
    result.map(Json)
}

/// `POST /api/files/search` —— 全项目内容搜索(委托 grep_core 核心层)
///
/// 请求体与 agent grep_files 工具 args 同形(query/isRegex/caseSensitive/
/// wholeWord/smartCase/dir/includeGlobs/excludeGlobs/useIgnoreFiles/maxResults,
/// camelCase/snake_case 双认);不委托 union toolkit 实例——工具实例把
/// maxResults 钳到 DEFAULT_MAX_RESULTS,而 REST 面 maxResults 上限是
/// 20 000(设置键 search.maxResults 同域)。
/// 30s 硬超时返回已收集 partial(truncated+timedOut 标注)。
/// 错误 → 404(dir 不存在)/ 400(非法正则 / 非法 glob / 参数非法 / 越界)。
pub async fn search_files(
    State(state): State<AgentApiState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let params = crate::builtin_tools::grep_files::GrepParams::from_args(&body)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let workdir = resolve_fs_root(&state, headers.get(WORKSPACE_HEADER))?;
    // G13:核心层为同步 fs 遍历,spawn_blocking 包装
    let v = tokio::task::spawn_blocking(move || {
        crate::builtin_tools::grep_files::grep_core(&workdir, &params, None)
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("search task failed: {e}"),
        )
    })?
    .map_err(|e| (err_status(&e), e))?;
    Ok(Json(v))
}

/// `POST /api/files/replace` —— 全局搜索替换(委托 replace_core 核心层)
///
/// 请求体=搜索参数 + `replacement`(必填)+ `apply`(缺省 false)+
/// `paths`(限定替换文件集,「按所选文件替换」)。
/// `apply=false` → 预览 `{preview:[{path,edits:[{line,before,after}]}],fileCount,matchCount}`,
/// 零写盘;`apply=true` → **重新匹配**(不信任预览快照)后逐文件 tmp+rename
/// 原子写,返回 `{appliedFiles,appliedMatches,failed:[{path,reason}]}`。
/// 治理:人的 UI 写操作,不进 agent 审计链;误操作防护=强制预览+按文件应用+
/// failed 留痕(设计 §四);树写互斥由 replace_core 内部持有。
/// HumanGate:`apply=true` 属写面(多文件原子写)——human 可写(落人工账),
/// anonymous 403(拒绝留痕);`apply=false` 预览只读,不设门。
/// 错误 → 403(anonymous 写)/ 404(dir 不存在)/ 400(缺 replacement / 非法正则 / glob / 参数非法)。
pub async fn replace_files(
    State(state): State<AgentApiState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let params = crate::builtin_tools::grep_files::ReplaceParams::from_args(&body)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    // HumanGate:apply=true 写面准入;账目 path 字段落操作选择器 query
    // (本操作无单一路径语义;params 进 blocking 闭包前先取出)
    let query_selector = params.search.query.clone();
    if params.apply {
        gate_write(&state, "files_replace", &query_selector)?;
    }
    let workdir = resolve_fs_root(&state, headers.get(WORKSPACE_HEADER))?;
    // G13:apply=true 在 replace_core 内部 blocking_lock 持树写锁,
    // 必须 spawn_blocking(禁 async 上下文直调);apply 快照供闭包后判定
    let apply = params.apply;
    let result = tokio::task::spawn_blocking(move || {
        crate::builtin_tools::grep_files::replace_core(&workdir, &params, None)
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("replace task failed: {e}"),
        )
    })?
    .map_err(|e| (err_status(&e), e));
    if apply {
        record_outcome(
            &state,
            "files_replace",
            CallerIdentity::Human,
            &query_selector,
            &result,
        );
    }
    result.map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin_tools::file_write::FileWriteTool;

    /// S4:上传文件名净化——基名提取/白名单替换/截长/尾部点清理/危险名兜底
    #[test]
    fn test_sanitize_upload_name() {
        assert_eq!(sanitize_upload_name("a.png"), "a.png");
        assert_eq!(
            sanitize_upload_name(r"C:\Users\test\我 的 图.png"),
            "我 的 图.png"
        );
        assert_eq!(sanitize_upload_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_upload_name("bad<script>.txt"), "bad_script_.txt");
        let long = sanitize_upload_name(&"x".repeat(100));
        assert_eq!(long.chars().count(), 80);
        assert_eq!(sanitize_upload_name("name...   "), "name");
        assert_eq!(sanitize_upload_name("  "), "file");
        assert_eq!(sanitize_upload_name(".."), "file");
    }

    /// S4:MIME 按扩展名标注,未知回落 application/octet-stream
    #[test]
    fn test_upload_mime() {
        assert_eq!(upload_mime("a.PNG"), "image/png");
        assert_eq!(upload_mime("b.jpg"), "image/jpeg");
        assert_eq!(upload_mime("c.svg"), "image/svg+xml");
        assert_eq!(upload_mime("d.md"), "text/plain");
        assert_eq!(upload_mime("e.xyz"), "application/octet-stream");
        assert_eq!(upload_mime("noext"), "application/octet-stream");
    }

    /// `writable_dir="."` 是本模块引入的新用法(IDE 人工编辑面 = workdir 全域)。
    /// 核心前提:workdir 根下的文件可写(join(".") 产生的 CurDir 组件会被
    /// `Path::components()` 规范化跳过,`starts_with` containment 判定不受影响)。
    #[tokio::test]
    async fn test_write_tool_writable_dir_dot_allows_workdir_root() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileWriteTool::new(dir.path().to_path_buf()).with_writable_dir(".");
        let result = tool
            .call(&serde_json::json!({
                "path": "root_file.txt",
                "content": "written by workbench",
                "overwrite": true
            }))
            .await;
        assert!(
            result.is_ok(),
            "root file write should pass, got: {:?}",
            result
        );
        let written = std::fs::read_to_string(dir.path().join("root_file.txt")).unwrap();
        assert_eq!(written, "written by workbench");
    }

    /// `writable_dir="."` 放宽到 workdir 全域,但沙箱边界(绝对路径 / `..` /
    /// symlink 逃逸校验)必须原样保留。
    #[tokio::test]
    async fn test_write_tool_writable_dir_dot_still_sandboxed() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileWriteTool::new(dir.path().to_path_buf()).with_writable_dir(".");
        let result = tool
            .call(&serde_json::json!({
                "path": "../escape.txt",
                "content": "x",
                "overwrite": true
            }))
            .await;
        assert!(result.is_err(), "parent-dir escape must still be rejected");
    }

    /// 覆盖写已存在文件(IDE 保存的最常见路径)在 `writable_dir="."` 下可用。
    #[tokio::test]
    async fn test_write_tool_writable_dir_dot_overwrite_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exists.txt"), b"old").unwrap();
        let tool = FileWriteTool::new(dir.path().to_path_buf()).with_writable_dir(".");
        let result = tool
            .call(&serde_json::json!({
                "path": "exists.txt",
                "content": "new",
                "overwrite": true
            }))
            .await;
        assert!(result.is_ok());
        let written = std::fs::read_to_string(dir.path().join("exists.txt")).unwrap();
        assert_eq!(written, "new");
    }

    /// 端点级冒烟:write_file handler 走真实 AgentApiState(workdir 指向 tempdir)。
    /// HumanGate:写面需 human 身份(state 注入 auth enabled;中间件层已验
    /// token 的语义由 handler 前置的 gate_write 承接)。
    #[tokio::test]
    async fn test_write_file_handler_writes_via_state_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = human_state(&dir_canon);
        let body = WriteBody {
            path: "notes/todo.md".to_string(),
            content: "- [ ] item".to_string(),
        };
        let resp = write_file(State(state), HeaderMap::new(), Json(body)).await;
        assert!(resp.is_ok(), "write should succeed, got: {:?}", resp.err());
        let written = std::fs::read_to_string(dir_canon.join("notes/todo.md")).unwrap();
        assert_eq!(written, "- [ ] item");
    }

    /// 构造 human 身份(写面放行)的 handler 级 AgentApiState
    fn human_state(dir_canon: &std::path::Path) -> AgentApiState {
        mutation_state(dir_canon)
    }

    /// 构造 anonymous 身份(auth disabled)的 handler 级 AgentApiState。
    /// toolkit 用生产同款内置装配(default_safe_toolkit_with_profile,15 工具
    /// 带 manifest)——读面 list/read 的 manifest 在场检查是生产行为的一部分,
    /// 测试态必须镜像;规则工具需远端引擎,单测不装配。
    fn anonymous_state(dir_canon: &std::path::Path) -> AgentApiState {
        AgentApiState::new_with_metrics(
            crate::agent::definition::AgentDefinitionManager::new(dir_canon.join("agents")),
            crate::api::evorule_client::EvoruleApiClient::new("http://localhost:0"),
            std::sync::Arc::new(crate::api::metrics::Metrics::new().unwrap()),
            dir_canon.to_path_buf(),
            std::sync::Arc::new(crate::api::workspace_client::WorkspaceApiClient::new(
                "http://localhost:0",
            )),
            std::sync::Arc::new(std::sync::RwLock::new(
                crate::builtin_tools::default_safe_toolkit_with_profile(
                    dir_canon,
                    dir_canon,
                    crate::builtin_tools::shell_exec::ExecutionProfile::Workbench,
                ),
            )),
        )
    }

    /// 构造指向 tempdir 的 handler 级 AgentApiState(写面测试缺省 human 态:
    /// auth enabled——增删改端点的既有行为断言全部以 human 身份走过 HumanGate)
    fn mutation_state(dir_canon: &std::path::Path) -> AgentApiState {
        anonymous_state(dir_canon).with_auth_config(crate::api::auth::AuthConfig::new(
            vec!["test-token".to_string()],
            true,
        ))
    }

    // =========================================================================
    // HumanGate(PR-4 两面):身份门 + 人工审计账
    // =========================================================================

    /// anonymous 真实拒绝路径:auth disabled 状态下调写面 → 403 + 开启指引,
    /// 文件未被触碰,拒绝本身落人工账。
    #[tokio::test]
    async fn test_anonymous_write_file_denied_403_with_ledger_record() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = anonymous_state(&dir_canon); // auth disabled = anonymous
        let resp = write_file(
            State(state),
            HeaderMap::new(),
            Json(WriteBody {
                path: "should-not-exist.txt".to_string(),
                content: "nope".to_string(),
            }),
        )
        .await;
        let (status, msg) = resp.expect_err("anonymous write must be denied");
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            msg.contains("--auth-token"),
            "403 body must carry enablement guidance, got: {msg}"
        );
        assert!(
            !dir_canon.join("should-not-exist.txt").exists(),
            "denied write must not touch the filesystem"
        );
        // 拒绝本身留痕
        let ledger = std::fs::read_to_string(dir_canon.join("data/human_gate_ledger.jsonl"))
            .expect("denial must be recorded");
        let entry: serde_json::Value =
            serde_json::from_str(ledger.lines().last().unwrap()).unwrap();
        assert_eq!(entry["op"], "file_write");
        assert_eq!(entry["identity"], "anonymous");
        assert_eq!(entry["outcome"], "denied");
        assert_eq!(entry["path"], "should-not-exist.txt");
    }

    /// anonymous 调 create/delete 同拒(写面四端点同一门,代表性覆盖两态)
    #[tokio::test]
    async fn test_anonymous_create_and_delete_denied_403() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::write(dir_canon.join("keep.txt"), b"x").unwrap();
        let state = anonymous_state(&dir_canon);
        let (status, _) = create_file(
            State(state.clone()),
            HeaderMap::new(),
            Json(CreateBody {
                path: "new.txt".to_string(),
                kind: None,
            }),
        )
        .await
        .expect_err("anonymous create must be denied");
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = delete_file(
            State(state),
            Query(DeleteQuery {
                path: "keep.txt".to_string(),
            }),
            HeaderMap::new(),
        )
        .await
        .expect_err("anonymous delete must be denied");
        assert_eq!(status, StatusCode::FORBIDDEN);
        // 只读面不受影响 + 文件未被触碰
        assert!(dir_canon.join("keep.txt").is_file());
        assert!(!dir_canon.join("new.txt").exists());
    }

    /// human 写面放行且操作落人工账(outcome=ok)
    #[tokio::test]
    async fn test_human_write_recorded_in_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = human_state(&dir_canon);
        let resp = create_file(
            State(state),
            HeaderMap::new(),
            Json(CreateBody {
                path: "ledgertest/a.md".to_string(),
                kind: None,
            }),
        )
        .await;
        assert!(resp.is_ok(), "human create should succeed");
        let ledger = std::fs::read_to_string(dir_canon.join("data/human_gate_ledger.jsonl"))
            .expect("human write must be recorded");
        let entry: serde_json::Value =
            serde_json::from_str(ledger.lines().last().unwrap()).unwrap();
        assert_eq!(entry["op"], "file_create");
        assert_eq!(entry["identity"], "human");
        assert_eq!(entry["outcome"], "ok");
        assert_eq!(entry["path"], "ledgertest/a.md");
    }

    /// replace apply=true 属写面:anonymous 403;预览(apply=false)匿名可通
    #[tokio::test]
    async fn test_anonymous_replace_apply_denied_preview_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::write(dir_canon.join("doc.txt"), b"foo\n").unwrap();
        let state = anonymous_state(&dir_canon);
        // 预览只读:匿名放行
        let preview = replace_files(
            State(state.clone()),
            HeaderMap::new(),
            Json(serde_json::json!({ "query": "foo", "replacement": "bar" })),
        )
        .await;
        assert!(preview.is_ok(), "anonymous preview must be allowed");
        assert_eq!(
            std::fs::read_to_string(dir_canon.join("doc.txt")).unwrap(),
            "foo\n",
            "preview must not write"
        );
        // apply=true:匿名 403,文件未被改写
        let (status, _) = replace_files(
            State(state),
            HeaderMap::new(),
            Json(serde_json::json!({
                "query": "foo",
                "replacement": "bar",
                "apply": true
            })),
        )
        .await
        .expect_err("anonymous apply must be denied");
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            std::fs::read_to_string(dir_canon.join("doc.txt")).unwrap(),
            "foo\n"
        );
    }

    // =========================================================================
    // 增删改端点(PR2):create / move / delete handler 级集成测试
    // =========================================================================

    #[test]
    fn test_err_status_mapping() {
        assert_eq!(
            err_status("path does not exist or cannot resolve: 'x'"),
            StatusCode::NOT_FOUND
        );
        assert_eq!(err_status("already exists: 'x'"), StatusCode::CONFLICT);
        assert_eq!(
            err_status("absolute path not allowed: 'x'"),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(err_status("illegal character"), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_create_file_handler_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = create_file(
            State(state),
            HeaderMap::new(),
            Json(CreateBody {
                path: "notes/todo.md".to_string(),
                kind: None,
            }),
        )
        .await;
        let v = resp.expect("create should succeed");
        assert_eq!(v["created"], serde_json::json!(true));
        assert_eq!(v["kind"], serde_json::json!("file"));
        assert!(dir_canon.join("notes/todo.md").is_file());
    }

    #[tokio::test]
    async fn test_create_dir_handler_creates_dir() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = create_file(
            State(state),
            HeaderMap::new(),
            Json(CreateBody {
                path: "docs/guides".to_string(),
                kind: Some("dir".to_string()),
            }),
        )
        .await;
        let v = resp.expect("create dir should succeed");
        assert_eq!(v["kind"], serde_json::json!("dir"));
        assert!(dir_canon.join("docs/guides").is_dir());
    }

    #[tokio::test]
    async fn test_create_duplicate_returns_409() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let mk_body = || {
            Json(CreateBody {
                path: "dup.txt".to_string(),
                kind: None,
            })
        };
        let _ = create_file(State(state.clone()), HeaderMap::new(), mk_body())
            .await
            .unwrap();
        let second = create_file(State(state), HeaderMap::new(), mk_body()).await;
        let (status, msg) = second.expect_err("duplicate create must fail");
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(msg.contains("already exists"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_create_escape_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = create_file(
            State(state),
            HeaderMap::new(),
            Json(CreateBody {
                path: "../escape.txt".to_string(),
                kind: None,
            }),
        )
        .await;
        let (status, _) = resp.expect_err("parent-dir escape must be rejected");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_move_handler_moves_and_renames() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::create_dir(dir_canon.join("src")).unwrap();
        std::fs::create_dir(dir_canon.join("dst")).unwrap();
        std::fs::write(dir_canon.join("src/a.txt"), b"payload").unwrap();
        let state = mutation_state(&dir_canon);
        let resp = move_file(
            State(state),
            HeaderMap::new(),
            Json(MoveBody {
                path: "src/a.txt".to_string(),
                target_dir: "dst".to_string(),
                new_name: Some("b.txt".to_string()),
            }),
        )
        .await;
        let v = resp.expect("move should succeed");
        assert_eq!(v["name"], serde_json::json!("b.txt"));
        assert!(!dir_canon.join("src/a.txt").exists());
        let moved = std::fs::read_to_string(dir_canon.join("dst/b.txt")).unwrap();
        assert_eq!(moved, "payload");
    }

    #[tokio::test]
    async fn test_move_missing_source_returns_404() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::create_dir(dir_canon.join("dst")).unwrap();
        let state = mutation_state(&dir_canon);
        let resp = move_file(
            State(state),
            HeaderMap::new(),
            Json(MoveBody {
                path: "nope.txt".to_string(),
                target_dir: "dst".to_string(),
                new_name: None,
            }),
        )
        .await;
        let (status, msg) = resp.expect_err("missing source must fail");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(msg.contains("does not exist"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_delete_handler_soft_deletes_into_trash() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::write(dir_canon.join("gone.txt"), b"bye").unwrap();
        let state = mutation_state(&dir_canon);
        let resp = delete_file(
            State(state),
            Query(DeleteQuery {
                path: "gone.txt".to_string(),
            }),
            HeaderMap::new(),
        )
        .await;
        let v = resp.expect("delete should succeed");
        assert!(
            !dir_canon.join("gone.txt").exists(),
            "original must be gone"
        );
        let trash_path = v["trash_path"].as_str().expect("trash_path in response");
        let restored = std::fs::read_to_string(trash_path).unwrap();
        assert_eq!(restored, "bye", "content must be preserved in trash");
    }

    #[tokio::test]
    async fn test_delete_missing_returns_404() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = delete_file(
            State(state),
            Query(DeleteQuery {
                path: "nope.txt".to_string(),
            }),
            HeaderMap::new(),
        )
        .await;
        let (status, _) = resp.expect_err("missing target must fail");
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_delete_guarded_root_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = delete_file(
            State(state),
            Query(DeleteQuery {
                path: ".".to_string(),
            }),
            HeaderMap::new(),
        )
        .await;
        let (status, _) = resp.expect_err("workdir root must be guarded");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    // =========================================================================
    // 搜索/替换端点(B2-PR3):search / replace handler 级集成测试
    // =========================================================================

    #[tokio::test]
    async fn test_search_handler_returns_groups() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::write(dir_canon.join("code.txt"), "needle here\nnothing\n").unwrap();
        let state = mutation_state(&dir_canon);
        let resp = search_files(
            State(state),
            HeaderMap::new(),
            Json(serde_json::json!({ "query": "needle", "smartCase": false })),
        )
        .await;
        let v = resp.expect("search should succeed");
        assert_eq!(v["totalMatches"], serde_json::json!(1));
        assert_eq!(v["fileCount"], serde_json::json!(1));
        assert_eq!(v["groups"][0]["path"], serde_json::json!("code.txt"));
        assert_eq!(v["groups"][0]["hits"][0]["line"], serde_json::json!(1));
        assert_eq!(v["groups"][0]["hits"][0]["col"], serde_json::json!(0));
    }

    #[tokio::test]
    async fn test_search_invalid_regex_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = search_files(
            State(state),
            HeaderMap::new(),
            Json(serde_json::json!({ "query": "(", "isRegex": true })),
        )
        .await;
        let (status, msg) = resp.expect_err("invalid regex must fail");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("invalid regex"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_search_missing_dir_returns_404() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = search_files(
            State(state),
            HeaderMap::new(),
            Json(serde_json::json!({ "query": "x", "dir": "nope" })),
        )
        .await;
        let (status, _) = resp.expect_err("missing dir must fail");
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_search_escape_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = search_files(
            State(state),
            HeaderMap::new(),
            Json(serde_json::json!({ "query": "x", "dir": "../outside" })),
        )
        .await;
        let (status, _) = resp.expect_err("parent-dir escape must be rejected");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_replace_preview_handler_no_writes() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::write(dir_canon.join("a.txt"), b"foo bar\n").unwrap();
        let state = mutation_state(&dir_canon);
        let resp = replace_files(
            State(state),
            HeaderMap::new(),
            Json(serde_json::json!({
                "query": "foo",
                "replacement": "baz",
                "smartCase": false
            })),
        )
        .await;
        let v = resp.expect("preview should succeed");
        assert_eq!(v["matchCount"], serde_json::json!(1));
        assert_eq!(
            v["preview"][0]["edits"][0]["before"],
            serde_json::json!("foo")
        );
        assert_eq!(
            v["preview"][0]["edits"][0]["after"],
            serde_json::json!("baz")
        );
        assert_eq!(
            std::fs::read_to_string(dir_canon.join("a.txt")).unwrap(),
            "foo bar\n",
            "preview must not write"
        );
    }

    #[tokio::test]
    async fn test_replace_apply_handler_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        std::fs::write(dir_canon.join("a.txt"), b"foo bar\n").unwrap();
        let state = mutation_state(&dir_canon);
        let resp = replace_files(
            State(state),
            HeaderMap::new(),
            Json(serde_json::json!({
                "query": "foo",
                "replacement": "baz",
                "smartCase": false,
                "apply": true
            })),
        )
        .await;
        let v = resp.expect("apply should succeed");
        assert_eq!(v["appliedFiles"], serde_json::json!(1));
        assert_eq!(v["appliedMatches"], serde_json::json!(1));
        assert_eq!(v["failed"].as_array().unwrap().len(), 0);
        assert_eq!(
            std::fs::read_to_string(dir_canon.join("a.txt")).unwrap(),
            "baz bar\n"
        );
    }

    #[tokio::test]
    async fn test_replace_missing_replacement_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = replace_files(
            State(state),
            HeaderMap::new(),
            Json(serde_json::json!({ "query": "x" })),
        )
        .await;
        let (status, msg) = resp.expect_err("missing replacement must fail");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("replacement"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_replace_invalid_regex_returns_400() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        let state = mutation_state(&dir_canon);
        let resp = replace_files(
            State(state),
            HeaderMap::new(),
            Json(serde_json::json!({ "query": "(", "isRegex": true, "replacement": "x" })),
        )
        .await;
        let (status, msg) = resp.expect_err("invalid regex must fail");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("invalid regex"), "got: {msg}");
    }

    // =========================================================================
    // 批 I:会话工作区重锚(X-Evo-Workspace 头)
    // =========================================================================

    /// 构造携带会话工作区头的 HeaderMap
    fn ws_headers(v: &std::path::Path) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert(
            WORKSPACE_HEADER,
            HeaderValue::from_str(&v.to_string_lossy()).unwrap(),
        );
        m
    }

    /// tempdir 路径 canonicalize(Windows 下取 \\?\ verbatim 去除后的实路径)
    fn canon(p: &std::path::Path) -> std::path::PathBuf {
        p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
    }

    /// workspace 指向 workdir 外目录:list / read / write 均以工作区为基准,
    /// workdir 自身不被触碰(严格/自由隔离的核心锁)。
    #[tokio::test]
    async fn test_workspace_header_reanchors_read_and_write() {
        let dir = tempfile::tempdir().unwrap(); // serve workdir
        let ws = tempfile::tempdir().unwrap(); // 会话工作区(workdir 外)
        let dir_canon = canon(dir.path());
        let ws_canon = canon(ws.path());
        std::fs::write(ws_canon.join("outer.md"), b"outside content").unwrap();
        let state = mutation_state(&dir_canon);

        // list:工作区根可见 outer.md
        let v = list_dir(
            State(state.clone()),
            Query(ListQuery { dir: None }),
            ws_headers(&ws_canon),
        )
        .await
        .expect("list at workspace root should succeed");
        assert!(
            v.to_string().contains("outer.md"),
            "workspace root listing must see workspace files, got: {v:?}"
        );

        // read:读工作区内文件
        let v = read_file(
            State(state.clone()),
            Query(ReadQuery {
                path: "outer.md".to_string(),
            }),
            ws_headers(&ws_canon),
        )
        .await
        .expect("read at workspace root should succeed");
        assert_eq!(v["content"], "outside content");

        // write:写进工作区(写面走 HumanGate,mutation_state=human)
        let resp = write_file(
            State(state.clone()),
            ws_headers(&ws_canon),
            Json(WriteBody {
                path: "new-in-ws.txt".to_string(),
                content: "hi".to_string(),
            }),
        )
        .await;
        assert!(resp.is_ok(), "write into workspace should succeed");
        assert_eq!(
            std::fs::read_to_string(ws_canon.join("new-in-ws.txt")).unwrap(),
            "hi"
        );
        // workdir 未被触碰
        assert!(
            !dir_canon.join("new-in-ws.txt").exists(),
            "write must land in workspace, not serve workdir"
        );
    }

    /// 相对路径 workspace 拒绝(400):重锚只接受绝对路径
    #[tokio::test]
    async fn test_workspace_header_relative_path_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let state = mutation_state(&canon(dir.path()));
        let mut m = HeaderMap::new();
        m.insert(WORKSPACE_HEADER, HeaderValue::from_static("relative/path"));
        let resp = list_dir(State(state), Query(ListQuery { dir: None }), m).await;
        let (status, msg) = resp.expect_err("relative workspace must be rejected");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("absolute path"), "got: {msg}");
    }

    /// 重锚后沙箱防线不松:工作区外(`..` 上跳)仍拒
    #[tokio::test]
    async fn test_workspace_header_escape_still_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let state = mutation_state(&canon(dir.path()));
        let resp = list_dir(
            State(state),
            Query(ListQuery {
                dir: Some("..".to_string()),
            }),
            ws_headers(&canon(ws.path())),
        )
        .await;
        let (status, _) = resp.expect_err("parent escape at workspace root must be rejected");
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// 空 workspace 头 = 严格模式回落 serve workdir(行为零变化口径)
    #[tokio::test]
    async fn test_workspace_header_empty_falls_back_to_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = canon(dir.path());
        std::fs::write(dir_canon.join("inside.md"), b"inside").unwrap();
        let state = mutation_state(&dir_canon);
        let mut m = HeaderMap::new();
        m.insert(WORKSPACE_HEADER, HeaderValue::from_static("   "));
        let v = list_dir(State(state), Query(ListQuery { dir: None }), m)
            .await
            .expect("empty header must fall back to workdir");
        assert!(
            v.to_string().contains("inside.md"),
            "fallback listing must see workdir files, got: {v:?}"
        );
    }

    // =========================================================================
    // 批 I 段 2:GET /api/fs/browse 目录浏览(选工作区目录选择器消费面)
    // =========================================================================

    /// path 缺省 = 根列表模式(Windows 盘符 / POSIX 根),entries 非空
    #[tokio::test]
    async fn test_browse_default_returns_roots() {
        let v = browse_dirs(Query(BrowseQuery { path: None }))
            .await
            .expect("default browse should return root list");
        assert!(v.roots, "default browse must be roots mode");
        assert!(v.path.is_none(), "roots mode has no current path");
        assert!(
            !v.entries.is_empty(),
            "root list must be non-empty on any CI runner"
        );
        assert!(v.parent.is_none(), "roots mode has no parent");
    }

    /// 显式目录:只列子目录(不列文件),path/parent 规整返回
    #[tokio::test]
    async fn test_browse_lists_dirs_only() {
        let dir = tempfile::tempdir().unwrap();
        let dir_canon = canon(dir.path());
        std::fs::create_dir(dir_canon.join("sub")).unwrap();
        std::fs::write(dir_canon.join("file.txt"), b"x").unwrap();
        let v = browse_dirs(Query(BrowseQuery {
            path: Some(dir_canon.to_string_lossy().into_owned()),
        }))
        .await
        .expect("browse tempdir should succeed");
        assert!(!v.roots);
        assert_eq!(
            v.path.as_deref(),
            Some(
                simplify_verbatim(dir_canon.clone())
                    .to_string_lossy()
                    .as_ref()
            ),
            "current path must be canonicalized (verbatim-stripped)"
        );
        assert_eq!(
            v.parent.as_deref(),
            dir_canon
                .parent()
                .map(|p| simplify_verbatim(p.to_path_buf())
                    .to_string_lossy()
                    .into_owned())
                .as_deref(),
            "parent must be the canonical parent dir"
        );
        assert_eq!(v.entries.len(), 1, "only subdirs listed, files skipped");
        assert_eq!(v.entries[0].name, "sub");
        assert_eq!(
            v.entries[0].path,
            simplify_verbatim(dir_canon.join("sub"))
                .to_string_lossy()
                .as_ref()
        );
    }

    /// 相对路径拒绝(400)——浏览面同样只收绝对路径
    #[tokio::test]
    async fn test_browse_rejects_relative_path() {
        let resp = browse_dirs(Query(BrowseQuery {
            path: Some("relative/dir".to_string()),
        }))
        .await;
        let (status, msg) = resp.expect_err("relative path must be rejected");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            msg.contains("absolute"),
            "error must say absolute, got: {msg}"
        );
    }

    /// 不存在路径 404;指向文件(非目录)400
    #[tokio::test]
    async fn test_browse_missing_and_file_paths() {
        let missing = browse_dirs(Query(BrowseQuery {
            path: Some("Z:\\definitely-not-evo-browse\\nope".to_string()),
        }))
        .await;
        #[cfg(not(windows))]
        let missing = browse_dirs(Query(BrowseQuery {
            path: Some("/definitely-not-evo-browse/nope".to_string()),
        }))
        .await;
        let (status, _) = missing.expect_err("missing path must 404");
        assert_eq!(status, StatusCode::NOT_FOUND);

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain.txt");
        std::fs::write(&file, b"x").unwrap();
        let not_dir = browse_dirs(Query(BrowseQuery {
            path: Some(file.to_string_lossy().into_owned()),
        }))
        .await;
        let (status, msg) = not_dir.expect_err("file path must be rejected");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("not a directory"), "got: {msg}");
    }
}
