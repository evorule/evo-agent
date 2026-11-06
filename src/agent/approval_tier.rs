// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 工具审批风险分级 allowlist 判定面（T0 自动 / T2 必问）
//!
//! ## 设计
//!
//! 工具侧 candidate 提案生成逻辑零改动（判断逻辑原样），分级判定做在决策端
//! 装饰层：[`TieredApproval`] 包在人工审批窗（`HttpApproval`）外层、治理留痕
//! 装饰器（`GovAuditedApproval`）内层——每笔自动批准决策同样过
//! approval_requested/resolved 留痕，T0 静默执行可回放可审计。
//!
//! 判定依据 per-workspace 配置 `{workdir}/.evo/approval-allowlist.json`
//! （与 `approval-policy.toml` 同域；该文件由 IDE 前端「本工作区始终允许」
//! 三选一写入，schema 兼容其 `{version:1,allow:[{tool,prefix}]}` 形态，
//! 并扩展 `workspace_root` / `tier` / `deny` 字段）：
//!
//! ```json
//! {
//!   "version": 1,
//!   "workspace_root": "D:\\demo",
//!   "allow": [
//!     { "tool": "shell_exec", "prefix": "find ", "tier": "T0" },
//!     { "tool": "shell_exec", "prefix": "git ", "tier": "T0",
//!       "deny_note": "见顶层 deny——deny 恒压过 allow" }
//!   ],
//!   "deny": [
//!     { "tool": "*", "pattern": "*evorule-seed*" }
//!   ]
//! }
//! ```
//!
//! ## 判定序（`verdict`，按序命中即返回）
//!
//! 1. **deny 恒压过 allow**：deny 条目命中（tool 通配或相等 + pattern 通配）
//!    → 必问。deny 优先防越权，配置不可放宽；
//! 2. **危险子命令硬守卫**：删除/移动/远端推送/网络外发/进程服务操纵/凭据
//!    权限面（与 IDE 前端会话记忆守卫同族镜像）→ 必问。代码级守卫，配置
//!    不可放宽；仍可人工单次批准/拒绝（人在环不削弱）；
//! 3. **allow 命中**（tool 相等 + 归一化前缀匹配）→ T0 静默批准。`tier`
//!    字段非缺省/"T0"（如 "T1"）时保守必问——会话内重复免批由 IDE 前端
//!    会话记忆承担，后端不重复做双决策；
//! 4. 无命中 → 必问（现状行为）。
//!
//! ### python_exec 专用通道（P0 沙箱收权，设计 §四）
//!
//! `python_exec` 提案无 command 字段（code 摘要在 description），判定基底由
//! 装饰器组装为 `python_exec <code>`：deny 通配可匹配代码内容；shell 守卫表
//! 不适用（`del`/`rm` 等 shell 语义对 python code 误报），由逃逸意图静态
//! 检测（[`hits_escape_intent`]：文件写面/网络外发/进程外逃逸）承担同类
//! 职责——逃逸意图=恒必问，沙箱内（cwd 锚定 workdir 执行）纯计算+allowlist
//! 条目命中=T0 可自动（条目 prefix="python_exec" 即命中头部 token）。
//!
//! ## 归一化（设计 §3.5 prefix 归一化）
//!
//! 命令与前缀两侧同规则：去首尾空白 + 内部空白折叠 + 统一小写 +
//! 反斜杠路径分隔符统一为正斜杠 + 去除双引号字符。前缀匹配带词元边界
//! （`find` 匹配 `find . -name x` 而不匹配 `findstr`）。
//!
//! ## 缺省不变式与降级
//!
//! - allowlist 文件不存在 → 判定面整体旁路，行为与现状零差异；
//! - 文件解析失败/版本不支持 → 显式 warn + 旁路（绝不静默按放行处理，
//!   与 `approval-policy.toml` 加载失败降级同口径；降级方向=少自动多人工）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::{info, warn};

use crate::agent::approval::{ApprovalCallback, ApprovalDecision, ApprovalRequest};

// =============================================================================
// 归一化与匹配（纯函数）
// =============================================================================

/// 命令/前缀归一化：去首尾空白 + 内部空白折叠为单空格 + 统一小写 +
/// 反斜杠统一为正斜杠 + 去除双引号字符。
pub fn normalize(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .replace('\\', "/")
        .replace('"', "")
}

/// 词元边界感知的前缀匹配：`prefix_matches("find . -name x", "find")` = true,
/// `prefix_matches("findstr /s x", "find")` = false。前缀以非字母数字字符
/// 结尾（如路径分隔符 `python scripts/`、旗标 `cargo test -`）时允许直接
/// 续接（`train.py` 紧跟 `/` 属合法续接）。
pub fn prefix_matches(command: &str, prefix: &str) -> bool {
    let c = normalize(command);
    let p = normalize(prefix);
    if p.is_empty() {
        return false;
    }
    let separator_ended = p
        .chars()
        .last()
        .map(|ch| !ch.is_ascii_alphanumeric())
        .unwrap_or(false);
    match c.strip_prefix(&p) {
        Some(rest) => rest.is_empty() || rest.starts_with(' ') || separator_ended,
        None => false,
    }
}

/// deny 模式通配匹配（仅支持 `*`；无 `*` 时退化为整串相等）。
pub fn pattern_matches(pattern: &str, command: &str) -> bool {
    let p = normalize(pattern);
    let c = normalize(command);
    let parts: Vec<&str> = p.split('*').collect();
    if parts.len() == 1 {
        return c == p;
    }
    let mut rest = c.as_str();
    if let Some(first) = parts.first().filter(|s| !s.is_empty()) {
        if !rest.starts_with(first) {
            return false;
        }
        rest = &rest[first.len()..];
    }
    for mid in &parts[1..parts.len() - 1] {
        if mid.is_empty() {
            continue;
        }
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    match parts.last().filter(|s| !s.is_empty()) {
        Some(last) => rest.ends_with(last),
        None => true,
    }
}

/// 危险子命令硬守卫（小写前缀表，与 IDE 前端会话记忆守卫同族镜像：
/// 删除/移动/远端推送/网络外发/进程服务操纵/凭据与权限面）。
/// 命中者永不自动批准，仍可人工单次批准/拒绝。
const AUTO_DENY_PREFIXES: &[&str] = &[
    "rm ",
    "rmdir",
    "del ",
    "erase ",
    "remove-item",
    "ri ",
    "rd ",
    "mv ",
    "move ",
    "ren ",
    "rename ",
    "git push",
    "git reset",
    "git clean",
    "git rebase",
    "git checkout",
    "git restore",
    "git branch -d",
    "git tag -d",
    "curl ",
    "wget ",
    "invoke-webrequest",
    "iwr ",
    "invoke-restmethod",
    "irm ",
    "invoke-expression",
    "iex ",
    "start-process",
    "start-job",
    "taskkill",
    "stop-process",
    "stop-service",
    "restart-service",
    "reg ",
    "regedit",
    "schtasks",
    "sc.exe",
    "netsh",
    "net user",
    "net localgroup",
    "format ",
    "diskpart",
    "cipher /w",
    "ssh ",
    "scp ",
    "nc ",
    "ncat ",
    "telnet ",
    "chmod",
    "chown",
    "icacls",
    "takeown",
];

/// 危险子命令守卫判定（纯函数）：空命令按守卫处理（不自动批）。
pub fn hits_auto_deny_guard(command: &str) -> bool {
    let n = normalize(command);
    if n.is_empty() {
        return true;
    }
    AUTO_DENY_PREFIXES
        .iter()
        .any(|p| n == *p || n.starts_with(p))
}

// =============================================================================
// python_exec 逃逸意图静态检测（P0 沙箱收权，设计 §四）
// =============================================================================

/// 文件写面意图子串（归一化后小写匹配；读不拦——沙箱承诺覆盖写面+网络，
/// 读出界与 shell_exec allowlist 现状同口径）。
const ESCAPE_WRITE_MARKERS: &[&str] = &[
    ".write_text(",
    ".write_bytes(",
    ".write(",
    "writelines(",
    "shutil.",
    "os.remove",
    "os.unlink",
    "os.rename",
    "os.replace",
    "os.mkdir",
    "os.makedirs",
    "os.rmdir",
    "os.truncate",
    "path.write", // pathlib Path.write_* 兜底
];

/// 网络外发意图子串（import/调用双覆盖；子串匹配保守从严——注释与字符串
/// 字面量命中同判，误报方向=必问）。
const ESCAPE_NETWORK_MARKERS: &[&str] = &[
    "import socket",
    "from socket",
    "socket.",
    "import requests",
    "from requests",
    "import urllib",
    "from urllib",
    "urllib.",
    "urlopen",
    "http.client",
    "import ftplib",
    "import smtplib",
    "import telnetlib",
    "import aiohttp",
    "import httpx",
    "requests.get",
    "requests.post",
    "requests.put",
    "requests.delete",
];

/// 进程外逃逸意图子串（拉起宿主任意进程=逃出 python 沙箱控制面）。
const ESCAPE_PROCESS_MARKERS: &[&str] = &[
    "import subprocess",
    "from subprocess",
    "subprocess.",
    "os.system",
    "os.popen",
    "os.exec",
    "os.spawn",
    "pty.spawn",
    "commands.getoutput",
];

/// `open(` 写模式判定：窗口内显式读模式（无逗号=单参默认读 / `,` 后随
/// "r"/"rb"/"rt" 字面量）放行；显式写模式（w/a/x/+）或变量模式（无法静态
/// 判定）一律按写意图命中（保守从严，误报方向=必问）。
fn open_call_is_write(normalized: &str) -> bool {
    let mut search_from = 0;
    while let Some(pos) = normalized[search_from..].find("open(") {
        let start = search_from + pos;
        // 窗口：open( 后 80 字符内找首个闭括号（嵌套按窗口截断，保守覆盖）
        let window_end = (start + 5 + 80).min(normalized.len());
        let window = &normalized[start + 5..window_end];
        let close = window.find(')').unwrap_or(window.len());
        let args = &window[..close];
        if !args.contains(',') {
            // 单参数 = 默认读模式
            search_from = start + 5;
            continue;
        }
        let read_mode = ["\"r\"", "'r'", "\"rb\"", "'rb'", "\"rt\"", "'rt'"]
            .iter()
            .any(|m| args.contains(m));
        if read_mode {
            search_from = start + 5;
            continue;
        }
        return true;
    }
    false
}

/// python code 逃逸意图静态检测（纯函数）：文件写面 / 网络外发 / 进程外
/// 逃逸三类，任一命中即判逃逸（设计 §四：沙箱内=T0 可自动，逃逸意图=T2）。
/// 输入为原始 code；归一化在函数内完成。空 code 不判逃逸（走无命中必问）。
pub fn hits_escape_intent(code: &str) -> bool {
    let n = normalize(code);
    if n.is_empty() {
        return false;
    }
    if open_call_is_write(&n) {
        return true;
    }
    ESCAPE_WRITE_MARKERS.iter().any(|m| n.contains(m))
        || ESCAPE_NETWORK_MARKERS.iter().any(|m| n.contains(m))
        || ESCAPE_PROCESS_MARKERS.iter().any(|m| n.contains(m))
}

// =============================================================================
// allowlist 配置面（per-workspace，schema 兼容前端写入形态）
// =============================================================================

/// allow 条目：`{tool, prefix, tier?}`。`tier` 缺省或 "T0" = 静默批准；
/// 其余值（如 "T1"）保守必问（见模块头判定序 3）。
#[derive(Debug, Clone, Deserialize)]
pub struct AllowEntry {
    /// 工具名（与提案 tool_name 精确相等才匹配）
    pub tool: String,
    /// 归一化命令前缀（词元边界感知匹配）
    pub prefix: String,
    /// 风险档位（缺省 "T0"；非 "T0" 值保守必问）
    #[serde(default)]
    pub tier: Option<String>,
}

/// deny 条目：`{tool, pattern}`。`tool` 支持 `*` 通配；`pattern` 支持 `*` 通配。
#[derive(Debug, Clone, Deserialize)]
pub struct DenyEntry {
    /// 工具名（`*` = 全工具）
    pub tool: String,
    /// 归一化命令模式（`*` 通配，如 `*evorule-seed*`）
    pub pattern: String,
}

/// allowlist 文档：`{version, workspace_root?, allow[], deny[]}`。
/// `version` 仅支持 1（其余显式拒绝降级旁路）。
#[derive(Debug, Clone, Deserialize)]
pub struct AllowlistFile {
    /// schema 版本（当前仅 1）
    pub version: u32,
    /// 工作区根（展示面冗余字段，判定不消费）
    #[serde(default)]
    pub workspace_root: Option<String>,
    /// 放行清单
    #[serde(default)]
    pub allow: Vec<AllowEntry>,
    /// 拒绝清单（恒压过 allow）
    #[serde(default)]
    pub deny: Vec<DenyEntry>,
}

/// allowlist 文件在 workdir 内的落位（与 `approval-policy.toml` 同域）
pub fn allowlist_path(workdir: &Path) -> PathBuf {
    workdir.join(".evo").join("approval-allowlist.json")
}

/// 加载 allowlist：文件缺失 → None（缺省不变式，行为零变化）；
/// 解析失败/版本不支持 → warn + None（fail-visible 降级旁路，方向=保守）。
pub fn load_allowlist(workdir: &Path) -> Option<AllowlistFile> {
    let path = allowlist_path(workdir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "approval-allowlist.json unreadable; tier grading bypassed (all approvals manual)");
            return None;
        }
    };
    match serde_json::from_str::<AllowlistFile>(&text) {
        Ok(f) if f.version == 1 => Some(f),
        Ok(f) => {
            warn!(path = %path.display(), version = f.version, "approval-allowlist.json unsupported version; tier grading bypassed");
            None
        }
        Err(e) => {
            warn!(path = %path.display(), error = %e, "approval-allowlist.json parse failed; tier grading bypassed (all approvals manual)");
            None
        }
    }
}

// =============================================================================
// 分级判定（纯函数）
// =============================================================================

/// 分级判定结果
#[derive(Debug, Clone, PartialEq)]
pub enum TierVerdict {
    /// allowlist 命中 → 静默批准
    ///
    /// * `tier` — 恒 "T0"（自动档）
    /// * `matched` — 命中的 allow 前缀（留痕用）
    Auto {
        /// 风险档位（恒 "T0"）
        tier: &'static str,
        /// 命中的 allow 前缀
        matched: String,
    },
    /// 人工必问（deny 命中 / 危险守卫 / allow tier 降档 / 无命中）
    Ask {
        /// 必问原因
        reason: AskReason,
    },
}

/// 必问原因（留痕与测试断言用）
#[derive(Debug, Clone, PartialEq)]
pub enum AskReason {
    /// deny 条目命中
    ///
    /// * `pattern` — 命中的 deny 模式
    Deny {
        /// 命中的 deny 模式
        pattern: String,
    },
    /// 危险子命令硬守卫命中
    Guard,
    /// allow 命中但 tier 非自动档（值如 "T1"）
    Tier(String),
    /// 无命中
    NoMatch,
}

/// 分级判定（纯函数，按模块头判定序）
///
/// `python_exec` 走专用通道（P0 沙箱收权，设计 §四）：判定基底由调用方
/// 组装为 `python_exec <code>`（自匹配 token+代码原文）——deny 通配可匹配
/// 代码内容；shell 危险子命令守卫表不适用（`del`/`rm` 等 shell 语义对
/// python code 误报），由 [`hits_escape_intent`] 承担同类职责：逃逸意图
/// （写面/网络/进程外）=恒必问，沙箱内纯计算+allowlist 条目命中=T0 可自动。
pub fn verdict(tool: &str, command: &str, allowlist: &AllowlistFile) -> TierVerdict {
    // 1. deny 恒压过 allow
    for d in &allowlist.deny {
        if (d.tool == "*" || d.tool == tool) && pattern_matches(&d.pattern, command) {
            return TierVerdict::Ask {
                reason: AskReason::Deny {
                    pattern: d.pattern.clone(),
                },
            };
        }
    }
    if tool == "python_exec" {
        // 2'. 逃逸意图静态检测（代码级，配置不可放宽）
        if hits_escape_intent(command) {
            return TierVerdict::Ask {
                reason: AskReason::Guard,
            };
        }
        // 3'. allow 条目（tool 相等 + 词元边界前缀匹配；前端「本工作区始终
        // 允许」写入形态 prefix="python_exec" 即命中代码基底头部 token）
        for e in &allowlist.allow {
            if e.tool == tool && prefix_matches(command, &e.prefix) {
                return match e.tier.as_deref() {
                    None | Some("T0") | Some("t0") => TierVerdict::Auto {
                        tier: "T0",
                        matched: e.prefix.clone(),
                    },
                    Some(other) => TierVerdict::Ask {
                        reason: AskReason::Tier(other.to_string()),
                    },
                };
            }
        }
        // 4'. 无命中
        return TierVerdict::Ask {
            reason: AskReason::NoMatch,
        };
    }
    // 2. 危险子命令硬守卫（代码级，配置不可放宽）
    if hits_auto_deny_guard(command) {
        return TierVerdict::Ask {
            reason: AskReason::Guard,
        };
    }
    // 3. allow 命中
    for e in &allowlist.allow {
        if e.tool == tool && prefix_matches(command, &e.prefix) {
            return match e.tier.as_deref() {
                None | Some("T0") | Some("t0") => TierVerdict::Auto {
                    tier: "T0",
                    matched: e.prefix.clone(),
                },
                Some(other) => TierVerdict::Ask {
                    reason: AskReason::Tier(other.to_string()),
                },
            };
        }
    }
    // 4. 无命中
    TierVerdict::Ask {
        reason: AskReason::NoMatch,
    }
}

// =============================================================================
// 决策端装饰器
// =============================================================================

/// 分级判定决策端装饰器：allowlist T0 命中 → 静默批准（留痕必产）；
/// 其余一律透传人工审批窗（现状行为）。文件缺失/加载失败 → 整体旁路。
///
/// 装配位置：`GovAuditedApproval`（治理留痕）内层、`HttpApproval`
/// （人工窗）外层——自动批准决策同样过 approval_requested/resolved 入账。
pub struct TieredApproval {
    /// 人工审批窗（透传目标）
    inner: Arc<dyn ApprovalCallback>,
    /// allowlist 文件路径（每次决策现读——前端「本工作区始终允许」落盘即时生效）
    allowlist_path: PathBuf,
}

impl TieredApproval {
    /// 构造（workdir = 会话工作区根，allowlist 落 `{workdir}/.evo/` 下）
    pub fn new(inner: Arc<dyn ApprovalCallback>, workdir: &Path) -> Self {
        Self {
            inner,
            allowlist_path: allowlist_path(workdir),
        }
    }

    /// 直接指定 allowlist 文件路径（测试用）
    pub fn with_path(inner: Arc<dyn ApprovalCallback>, path: PathBuf) -> Self {
        Self {
            inner,
            allowlist_path: path,
        }
    }
}

#[async_trait]
impl ApprovalCallback for TieredApproval {
    async fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        // 文件读取与判定均同步轻量（小 JSON），不跨 await 持锁
        let text = match std::fs::read_to_string(&self.allowlist_path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return self.inner.request_approval(req).await;
            }
            Err(e) => {
                warn!(path = %self.allowlist_path.display(), error = %e,
                      "approval-allowlist.json unreadable; tier grading bypassed (all approvals manual)");
                return self.inner.request_approval(req).await;
            }
        };
        let allowlist: AllowlistFile = match serde_json::from_str::<AllowlistFile>(&text) {
            Ok(f) if f.version == 1 => f,
            Ok(f) => {
                warn!(path = %self.allowlist_path.display(), version = f.version,
                      "approval-allowlist.json unsupported version; tier grading bypassed");
                return self.inner.request_approval(req).await;
            }
            Err(e) => {
                warn!(path = %self.allowlist_path.display(), error = %e,
                      "approval-allowlist.json parse failed; tier grading bypassed (all approvals manual)");
                return self.inner.request_approval(req).await;
            }
        };
        // python_exec 判定基底组装（P0 沙箱收权）：自匹配 token+code 原文——
        // deny 通配可匹配代码内容；前端放行条目 prefix="python_exec" 命中头部
        // token。code 缺失/为空时不组 token（空基底不命中条目→必问，防空执行
        // 静默批）。其余工具用提案 command 原样。
        let base_command = if req.tool_name == "python_exec" {
            let code = req.args.get("code").and_then(|v| v.as_str()).unwrap_or("");
            if code.is_empty() {
                String::new()
            } else {
                format!("python_exec {code}")
            }
        } else {
            req.command.clone()
        };
        match verdict(&req.tool_name, &base_command, &allowlist) {
            TierVerdict::Auto { tier, matched } => {
                let reason = format!(
                    "allowlist {} match: tool={} prefix={:?} (workspace approval allowlist)",
                    tier, req.tool_name, matched
                );
                info!(
                    session = %req.session_id,
                    tool = %req.tool_name,
                    command = %req.command,
                    proposal_id = %req.proposal_id,
                    "tier T0: candidate auto-approved by workspace allowlist"
                );
                ApprovalDecision {
                    approved: true,
                    approver: "workspace-allowlist".to_string(),
                    verified: false,
                    reason,
                    auto_rejected: false,
                    verdict_json: None,
                }
            }
            TierVerdict::Ask { reason } => {
                match &reason {
                    AskReason::Deny { pattern } => info!(
                        session = %req.session_id,
                        tool = %req.tool_name,
                        command = %req.command,
                        "tier T2: deny pattern {:?} hit, forced manual approval", pattern
                    ),
                    AskReason::Guard => info!(
                        session = %req.session_id,
                        tool = %req.tool_name,
                        command = %req.command,
                        "tier T2: guarded subcommand, forced manual approval"
                    ),
                    _ => {}
                }
                self.inner.request_approval(req).await
            }
        }
    }
}

// =============================================================================
// 测试
// =============================================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    fn allowlist(allow: serde_json::Value, deny: serde_json::Value) -> AllowlistFile {
        serde_json::from_value(json!({
            "version": 1,
            "allow": allow,
            "deny": deny,
        }))
        .unwrap()
    }

    // ===== 归一化 =====

    #[test]
    fn test_normalize_folds_whitespace_case_separators_quotes() {
        assert_eq!(normalize("  Find   \"D:\\X Y\"  "), "find d:/x y");
        assert_eq!(normalize("GIT\tSTATUS"), "git status");
        assert_eq!(normalize(""), "");
    }

    // ===== 前缀边界匹配 =====

    #[test]
    fn test_prefix_matches_boundary() {
        assert!(prefix_matches("find . -name x", "find"));
        assert!(prefix_matches("find", "find"));
        assert!(!prefix_matches("findstr /s x", "find"));
        assert!(prefix_matches("python scripts/train.py", "python scripts/"));
        // 归一化对齐:路径分隔符/引号/大小写
        assert!(prefix_matches("Find \"D:\\X y\"", "find d:/x"));
        // 空前缀不匹配(防全量放行)
        assert!(!prefix_matches("anything", ""));
        assert!(!prefix_matches("", "find"));
    }

    // ===== deny 通配模式 =====

    #[test]
    fn test_pattern_matches_wildcards() {
        assert!(pattern_matches(
            "*evorule-seed*",
            "cat /tmp/evorule-seed/x.json"
        ));
        assert!(pattern_matches("*evorule-seed*", "EVORULE-SEED"));
        assert!(pattern_matches("git *", "git status"));
        assert!(pattern_matches("git push*", "git push --force origin main"));
        assert!(!pattern_matches("git *", "cargo test"));
        assert!(pattern_matches("exact", "exact"));
        assert!(!pattern_matches("exact", "exact-ish"));
    }

    // ===== 危险子命令守卫 =====

    #[test]
    fn test_guard_hits_destructive_and_network() {
        assert!(hits_auto_deny_guard("rm -rf /tmp/build"));
        assert!(hits_auto_deny_guard("git push --force origin main"));
        assert!(hits_auto_deny_guard("curl https://example.com/exfil"));
        assert!(hits_auto_deny_guard("Remove-Item C:\\x"));
        assert!(hits_auto_deny_guard(""));
        assert!(!hits_auto_deny_guard("cargo test --workspace"));
        assert!(!hits_auto_deny_guard("git status"));
        assert!(!hits_auto_deny_guard("python scripts/train.py"));
    }

    // ===== 判定序 =====

    #[test]
    fn test_verdict_allow_hit_auto_t0() {
        let al = allowlist(
            json!([{ "tool": "shell_exec", "prefix": "find " }]),
            json!([]),
        );
        assert_eq!(
            verdict("shell_exec", "find D:/x -name a.rs", &al),
            TierVerdict::Auto {
                tier: "T0",
                matched: "find ".to_string()
            }
        );
    }

    #[test]
    fn test_verdict_deny_overrides_allow() {
        let al = allowlist(
            json!([{ "tool": "shell_exec", "prefix": "git " }]),
            json!([{ "tool": "*", "pattern": "git push*" }]),
        );
        assert_eq!(
            verdict("shell_exec", "git push --force", &al),
            TierVerdict::Ask {
                reason: AskReason::Deny {
                    pattern: "git push*".to_string()
                }
            }
        );
        // 同前缀非 deny 命中照常自动
        assert!(matches!(
            verdict("shell_exec", "git status", &al),
            TierVerdict::Auto { .. }
        ));
    }

    #[test]
    fn test_verdict_guard_beats_allow() {
        let al = allowlist(
            json!([{ "tool": "shell_exec", "prefix": "rm " }]),
            json!([]),
        );
        // allowlist 试图放行删除类:守卫代码级压过配置
        assert_eq!(
            verdict("shell_exec", "rm -rf /tmp/x", &al),
            TierVerdict::Ask {
                reason: AskReason::Guard
            }
        );
    }

    #[test]
    fn test_verdict_tool_must_match_exactly() {
        let al = allowlist(
            json!([{ "tool": "shell_exec", "prefix": "find " }]),
            json!([]),
        );
        assert_eq!(
            verdict("python_exec", "find x", &al),
            TierVerdict::Ask {
                reason: AskReason::NoMatch
            }
        );
    }

    #[test]
    fn test_verdict_tier_t1_conservative_ask() {
        let al = allowlist(
            json!([{ "tool": "shell_exec", "prefix": "find ", "tier": "T1" }]),
            json!([]),
        );
        assert_eq!(
            verdict("shell_exec", "find x", &al),
            TierVerdict::Ask {
                reason: AskReason::Tier("T1".to_string())
            }
        );
    }

    #[test]
    fn test_verdict_empty_command_never_auto() {
        // 无 command 字段的提案(如 http_get/file_* 类)永不匹配 → 空命令按
        // 守卫处理(必问,不自动批)
        let al = allowlist(
            json!([{ "tool": "http_get", "prefix": "python scripts/" }]),
            json!([]),
        );
        assert_eq!(
            verdict("http_get", "", &al),
            TierVerdict::Ask {
                reason: AskReason::Guard
            }
        );
    }

    // ===== python_exec 逃逸意图静态检测（P0 沙箱收权）=====

    #[test]
    fn test_escape_intent_network() {
        assert!(hits_escape_intent("import requests\nprint(1)"));
        assert!(hits_escape_intent("import socket\ns = socket.socket()"));
        assert!(hits_escape_intent("from urllib.request import urlopen"));
        assert!(hits_escape_intent("import httpx"));
        assert!(hits_escape_intent("data = urllib.parse.quote(x)"));
    }

    #[test]
    fn test_escape_intent_process() {
        assert!(hits_escape_intent(
            "import subprocess\nsubprocess.run(['ls'])"
        ));
        assert!(hits_escape_intent("os.system('dir')"));
        assert!(hits_escape_intent("import subprocess"));
    }

    #[test]
    fn test_escape_intent_write() {
        assert!(hits_escape_intent("open('out.txt', 'w').write(x)"));
        assert!(hits_escape_intent("open(p, mode)")); // 变量模式=无法判定,保守命中
        assert!(hits_escape_intent("Path(x).write_text('y')"));
        assert!(hits_escape_intent("import shutil\nshutil.rmtree(d)"));
        assert!(hits_escape_intent("os.remove(f)"));
    }

    #[test]
    fn test_escape_intent_read_and_compute_pass() {
        // 读模式与纯计算不判逃逸（沙箱内=T0 eligible）
        assert!(!hits_escape_intent("data = open('in.csv').read()"));
        assert!(!hits_escape_intent("f = open('a.txt', 'r')"));
        assert!(!hits_escape_intent(
            "import json\nprint(json.dumps({'a': 1}))"
        ));
        assert!(!hits_escape_intent("import math\nprint(math.sqrt(2))"));
        assert!(!hits_escape_intent(
            "import pandas as pd\nprint(pd.DataFrame())"
        ));
        assert!(!hits_escape_intent(""));
    }

    // ===== python_exec T0 通道（verdict 专用分支）=====

    /// 基底组装:python_exec 判定基底="python_exec "+code(装饰器组装,测试同款)
    fn py_base(code: &str) -> String {
        format!("python_exec {code}")
    }

    #[test]
    fn test_verdict_python_exec_sandbox_clean_auto() {
        let al = allowlist(
            json!([{ "tool": "python_exec", "prefix": "python_exec" }]),
            json!([]),
        );
        // 纯计算+条目命中 → T0 静默批准
        assert!(matches!(
            verdict("python_exec", &py_base("print(sum(range(10)))"), &al),
            TierVerdict::Auto { .. }
        ));
    }

    #[test]
    fn test_verdict_python_exec_escape_always_ask() {
        let al = allowlist(
            json!([{ "tool": "python_exec", "prefix": "python_exec" }]),
            json!([]),
        );
        // 逃逸意图(网络/进程/写面)恒必问——守卫压过 allow 配置
        for code in [
            "import requests\nr = requests.get(u)",
            "import subprocess\nsubprocess.run(x)",
            "open('x.txt', 'w').write(d)",
        ] {
            assert_eq!(
                verdict("python_exec", &py_base(code), &al),
                TierVerdict::Ask {
                    reason: AskReason::Guard
                }
            );
        }
    }

    #[test]
    fn test_verdict_python_exec_no_entry_ask() {
        let al = allowlist(
            json!([{ "tool": "shell_exec", "prefix": "find " }]),
            json!([]),
        );
        // 沙箱内但无条目 → 必问(不自动批)
        assert_eq!(
            verdict("python_exec", &py_base("print(1)"), &al),
            TierVerdict::Ask {
                reason: AskReason::NoMatch
            }
        );
    }

    #[test]
    fn test_verdict_python_exec_empty_code_ask() {
        // 空 code(无 code 字段)不判逃逸,装饰器空基底不命中条目 → 必问(防空执行静默批)
        let al = allowlist(
            json!([{ "tool": "python_exec", "prefix": "python_exec" }]),
            json!([]),
        );
        assert_eq!(
            verdict("python_exec", "", &al),
            TierVerdict::Ask {
                reason: AskReason::NoMatch
            }
        );
    }

    #[test]
    fn test_verdict_python_exec_deny_on_code_content() {
        let al = allowlist(
            json!([{ "tool": "python_exec", "prefix": "python_exec" }]),
            json!([{ "tool": "*", "pattern": "*evorule-seed*" }]),
        );
        // deny 恒压:代码内容命中 deny 模式 → 必问
        assert!(matches!(
            verdict("python_exec", &py_base("print('evorule-seed')"), &al),
            TierVerdict::Ask {
                reason: AskReason::Deny { .. }
            }
        ));
    }

    // ===== schema 兼容(前端写入形态) =====

    #[test]
    fn test_schema_compat_frontend_shape() {
        // IDE 前端「本工作区始终允许」写入形态:无 deny/workspace_root/tier
        let al: AllowlistFile = serde_json::from_str(
            r#"{"version":1,"allow":[{"tool":"shell_exec","prefix":"find"}]}"#,
        )
        .unwrap();
        assert!(al.deny.is_empty());
        assert!(al.workspace_root.is_none());
        assert_eq!(
            verdict("shell_exec", "find . -name x", &al),
            TierVerdict::Auto {
                tier: "T0",
                matched: "find".to_string()
            }
        );
    }

    #[test]
    fn test_load_missing_file_returns_none() {
        let tmp = std::env::temp_dir().join(format!("evo-al-missing-{}", std::process::id()));
        assert!(load_allowlist(&tmp).is_none());
    }

    #[test]
    fn test_load_invalid_json_returns_none() {
        let dir = std::env::temp_dir().join(format!("evo-al-invalid-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".evo")).unwrap();
        std::fs::write(dir.join(".evo").join("approval-allowlist.json"), "{oops").unwrap();
        assert!(load_allowlist(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_unsupported_version_returns_none() {
        let dir = std::env::temp_dir().join(format!("evo-al-ver-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".evo")).unwrap();
        std::fs::write(
            dir.join(".evo").join("approval-allowlist.json"),
            r#"{"version":2,"allow":[]}"#,
        )
        .unwrap();
        assert!(load_allowlist(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ===== 装饰器行为 =====

    use crate::agent::approval::{AutoApprove, DenyAll};

    fn req(tool: &str, command: &str) -> ApprovalRequest {
        ApprovalRequest {
            session_id: "s1".to_string(),
            tool_name: tool.to_string(),
            args: json!({"command": command}),
            command: command.to_string(),
            risk: "high".to_string(),
            alternative: String::new(),
            proposal_id: "ap-1".to_string(),
        }
    }

    #[tokio::test]
    async fn test_decorator_auto_approves_on_allow_hit() {
        let dir = std::env::temp_dir().join(format!("evo-al-dec-a-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".evo")).unwrap();
        std::fs::write(
            dir.join(".evo").join("approval-allowlist.json"),
            r#"{"version":1,"allow":[{"tool":"shell_exec","prefix":"cargo test"}]}"#,
        )
        .unwrap();
        // inner=DenyAll:若走到人工窗必拒——断言自动批准确实发生在装饰层
        let dec = TieredApproval::new(Arc::new(DenyAll), &dir);
        let d = dec
            .request_approval(&req("shell_exec", "cargo test --workspace"))
            .await;
        assert!(d.approved);
        assert_eq!(d.approver, "workspace-allowlist");
        assert!(d.reason.contains("T0"));
        assert!(!d.auto_rejected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_decorator_passthrough_without_file() {
        // 缺省不变式:无 allowlist 文件 → 行为=inner 原样
        let dir = std::env::temp_dir().join(format!("evo-al-dec-b-{}", std::process::id()));
        let dec = TieredApproval::new(Arc::new(AutoApprove), &dir);
        let d = dec.request_approval(&req("shell_exec", "cargo test")).await;
        assert!(d.approved);
        assert_eq!(d.approver, "auto");
    }

    #[tokio::test]
    async fn test_decorator_forces_manual_on_deny_and_guard() {
        let dir = std::env::temp_dir().join(format!("evo-al-dec-c-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".evo")).unwrap();
        std::fs::write(
            dir.join(".evo").join("approval-allowlist.json"),
            r#"{"version":1,
                "allow":[{"tool":"shell_exec","prefix":"git "},{"tool":"shell_exec","prefix":"rm "}],
                "deny":[{"tool":"*","pattern":"git push*"}]}"#,
        )
        .unwrap();
        let dec = TieredApproval::new(Arc::new(DenyAll), &dir);
        // deny 命中 → 透传人工窗(DenyAll 拒)
        let d = dec
            .request_approval(&req("shell_exec", "git push --force"))
            .await;
        assert!(!d.approved);
        // allow 命中但守卫(删除类)压过 → 透传人工窗
        let d = dec.request_approval(&req("shell_exec", "rm -rf x")).await;
        assert!(!d.approved);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_decorator_invalid_file_passthrough() {
        let dir = std::env::temp_dir().join(format!("evo-al-dec-d-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".evo")).unwrap();
        std::fs::write(
            dir.join(".evo").join("approval-allowlist.json"),
            "{bad json",
        )
        .unwrap();
        let dec = TieredApproval::new(Arc::new(AutoApprove), &dir);
        let d = dec.request_approval(&req("shell_exec", "cargo test")).await;
        assert!(d.approved);
        assert_eq!(d.approver, "auto");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ===== python_exec 装饰器通道（P0 沙箱收权）=====

    fn py_req(code: &str) -> ApprovalRequest {
        ApprovalRequest {
            session_id: "s1".to_string(),
            tool_name: "python_exec".to_string(),
            args: json!({"code": code}),
            command: String::new(), // python_exec 提案无 command 字段
            risk: "high".to_string(),
            alternative: String::new(),
            proposal_id: "ap-py-1".to_string(),
        }
    }

    #[tokio::test]
    async fn test_decorator_python_exec_sandbox_clean_auto() {
        let dir = std::env::temp_dir().join(format!("evo-al-dec-py-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".evo")).unwrap();
        std::fs::write(
            dir.join(".evo").join("approval-allowlist.json"),
            r#"{"version":1,"allow":[{"tool":"python_exec","prefix":"python_exec"}]}"#,
        )
        .unwrap();
        // 沙箱内纯计算+条目命中 → 静默批准(inner=DenyAll 证明未走人工窗)
        let dec = TieredApproval::new(Arc::new(DenyAll), &dir);
        let d = dec.request_approval(&py_req("print(sum(range(10)))")).await;
        assert!(d.approved);
        assert_eq!(d.approver, "workspace-allowlist");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_decorator_python_exec_escape_forces_manual() {
        let dir = std::env::temp_dir().join(format!("evo-al-dec-pye-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".evo")).unwrap();
        std::fs::write(
            dir.join(".evo").join("approval-allowlist.json"),
            r#"{"version":1,"allow":[{"tool":"python_exec","prefix":"python_exec"}]}"#,
        )
        .unwrap();
        // 逃逸意图(网络/写面)→ 透传人工窗(DenyAll 拒)
        let dec = TieredApproval::new(Arc::new(DenyAll), &dir);
        let d = dec
            .request_approval(&py_req("import requests\nprint(requests.get(u))"))
            .await;
        assert!(!d.approved);
        let d = dec
            .request_approval(&py_req("open('x','w').write(d)"))
            .await;
        assert!(!d.approved);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
