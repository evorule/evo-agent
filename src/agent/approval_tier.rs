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
//!   "workspace_root": "D:\\knowledge",
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
        match verdict(&req.tool_name, &req.command, &allowlist) {
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
        // 无 command 字段的提案(如 python_exec/http_get)永不匹配 → 空命令按
        // 守卫处理(必问,不自动批)
        let al = allowlist(
            json!([{ "tool": "python_exec", "prefix": "python scripts/" }]),
            json!([]),
        );
        assert_eq!(
            verdict("python_exec", "", &al),
            TierVerdict::Ask {
                reason: AskReason::Guard
            }
        );
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
}
