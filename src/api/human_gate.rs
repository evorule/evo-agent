// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 人工面 HumanGate —— 写面身份门 + 人工审计账
//!
//! 身份三类（对齐 caller_role 语义）在 HTTP 层的判定边界：
//!
//! | 身份 | 判定 | 可用面 | 审计 |
//! |---|---|---|---|
//! | human | auth enabled 且 token 校验通过（[`auth_middleware`](crate::api::auth::auth_middleware) 已在路由层把关） | agent 面 + 人工面（HumanGate） | 人工账 |
//! | llm | runner/delegate/ai-plugin 通路 | 仅 agent 面（管道） | agent 审计链 |
//! | anonymous | auth disabled | **只读**；写面一律 403 | 拒绝本身留痕 |
//!
//! anonymous 拒写修的是「绑定 0.0.0.0 且 auth=false」的暴露面；本地回环 +
//! auth=false 组合给过渡宽限——读面全通，写面 403 响应体携带开启指引
//! （正式收紧前出公告版说明）。
//!
//! 人工审计账：独立于 agent 审计链的轻量 JSONL（时间/操作/路径/身份/结果），
//! 落 `<workdir>/data/human_gate_ledger.jsonl`；两本账可经时间戳关联，
//! 但互不伪造（人的 UI 操作不进 agent 会话事实，反之亦然）。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use axum::http::StatusCode;

use crate::api::auth::AuthConfig;
use crate::api::session_index::unix_now;

/// anonymous 写面 403 响应体（含开启指引——过渡宽限的告知面）
const ANONYMOUS_WRITE_DENIED: &str =
    "anonymous identity is read-only: file mutations require authentication. \
Start the serve with `--auth-token <secret>` (or set [auth] in the config file) \
and send `Authorization: Bearer <token>` from the client.";

/// anonymous 写面拒绝文案（ensure_write_allowed 错误路径需要重复携带时用）
pub fn anonymous_write_denied() -> String {
    ANONYMOUS_WRITE_DENIED.to_string()
}

/// HTTP 层可见的调用者身份（llm 身份只在 agent 面，不出现在本判定）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerIdentity {
    /// auth enabled：中间件已校验 token，到达 handler 即 human
    Human,
    /// auth disabled：匿名（只读）
    Anonymous,
}

impl CallerIdentity {
    fn as_str(self) -> &'static str {
        match self {
            CallerIdentity::Human => "human",
            CallerIdentity::Anonymous => "anonymous",
        }
    }
}

/// token 类别（特权闸开启时的两级区分;闸关闭全部为特权=旧版行为）
///
/// - `Privileged`：特权列表内的人工 token（或鉴权关闭/闸关闭态的合法调用）
/// - `Operator`：合法但非特权列表的 token（agent 运行时/工具链持有的那把）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerClass {
    /// 特权列表内的人工 token（或鉴权关闭/特权闸关闭态的一切合法调用）。
    Privileged,
    /// 合法但非特权列表的 token（agent 运行时/工具链持有的那把）。
    Operator,
}

impl CallerClass {
    /// 按特权闸判定结果归类（true=特权，false=运营）。
    pub fn from_flag(privileged: bool) -> Self {
        if privileged {
            CallerClass::Privileged
        } else {
            CallerClass::Operator
        }
    }

    /// 线格式标签（审计/日志面字符串形态）。
    pub fn as_str(self) -> &'static str {
        match self {
            CallerClass::Privileged => "privileged",
            CallerClass::Operator => "operator",
        }
    }
}

/// 判定调用者身份
pub fn caller_identity(auth: &AuthConfig) -> CallerIdentity {
    if auth.enabled() {
        CallerIdentity::Human
    } else {
        CallerIdentity::Anonymous
    }
}

/// 写面准入门：anonymous 一律 403（拒绝留痕由调用方经 [`HumanGateLedger`] 完成）
pub fn ensure_write_allowed(auth: &AuthConfig) -> Result<CallerIdentity, (StatusCode, String)> {
    match caller_identity(auth) {
        CallerIdentity::Human => Ok(CallerIdentity::Human),
        CallerIdentity::Anonymous => {
            Err((StatusCode::FORBIDDEN, ANONYMOUS_WRITE_DENIED.to_string()))
        }
    }
}

/// 特权面准入门（技能注册面 approve/revoke/import 等人工把关动作）：
/// 类别闸开启时非特权 token 一律 403；闸关闭（`[auth] human_tokens` 未配）
/// 全放行 = 旧版行为，部署侧随时可单独收紧。
pub fn ensure_privileged(
    auth: &AuthConfig,
    class: CallerClass,
) -> Result<(), (StatusCode, String)> {
    if class == CallerClass::Privileged {
        return Ok(());
    }
    if !auth.privileged_gate_active() {
        return Ok(());
    }
    Err((
        StatusCode::FORBIDDEN,
        "this management action is reserved for the privileged token: add the \
         caller's token to [auth] human_tokens (config) to grant human approval \
         rights; the regular token is operator-class and cannot self-approve"
            .to_string(),
    ))
}

/// 人工审计账（JSONL 追加写；独立于 agent 审计链）
#[derive(Debug)]
pub struct HumanGateLedger {
    path: PathBuf,
    /// 串行化追加写（防并发交错撕行）
    sink: Mutex<()>,
}

impl HumanGateLedger {
    /// 账本落点 = `<workdir>/data/human_gate_ledger.jsonl`
    pub fn new(workdir: &Path) -> Self {
        Self {
            path: workdir.join("data").join("human_gate_ledger.jsonl"),
            sink: Mutex::new(()),
        }
    }

    /// 落一条人工面操作记录（执行成功/失败与门拒绝通用）
    pub fn record(&self, op: &str, identity: CallerIdentity, path: &str, outcome: &str) {
        let entry = serde_json::json!({
            "ts": unix_now(),
            "op": op,
            "path": path,
            "identity": identity.as_str(),
            "outcome": outcome,
        });
        self.write_line(entry);
    }

    /// 落一条带类别的记录（注册面管理动作专用：identity 回答「auth 与否」，
    /// class 回答「特权/操作者 token」——两者合起来才能归因人工把关）
    pub fn record_class(
        &self,
        op: &str,
        identity: CallerIdentity,
        class: CallerClass,
        path: &str,
        outcome: &str,
    ) {
        let entry = serde_json::json!({
            "ts": unix_now(),
            "op": op,
            "path": path,
            "identity": identity.as_str(),
            "class": class.as_str(),
            "outcome": outcome,
        });
        self.write_line(entry);
    }

    /// 落一条带类别+确认通道的记录（两段式确认面专用）：confirmed_via 回答
    /// 「这次人工把关经何通道兑现」（native-shell=壳原生对话框逐次确认 /
    /// session-grant=壳会话级授权在位直接执行）；None=未走确认通道
    /// （纯 web 单段形态，字段缺席=旧账本行同构）。
    pub fn record_class_via(
        &self,
        op: &str,
        identity: CallerIdentity,
        class: CallerClass,
        path: &str,
        outcome: &str,
        confirmed_via: Option<&str>,
    ) {
        let mut entry = serde_json::json!({
            "ts": unix_now(),
            "op": op,
            "path": path,
            "identity": identity.as_str(),
            "class": class.as_str(),
            "outcome": outcome,
        });
        if let Some(via) = confirmed_via {
            entry["confirmed_via"] = serde_json::Value::from(via);
        }
        self.write_line(entry);
    }

    fn write_line(&self, entry: serde_json::Value) {
        let line = entry.to_string();
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _guard = self.sink.lock();
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_identity_from_auth_config() {
        assert_eq!(
            caller_identity(&AuthConfig::disabled()),
            CallerIdentity::Anonymous
        );
        let enabled = AuthConfig::new(vec!["secret".to_string()], true);
        assert_eq!(caller_identity(&enabled), CallerIdentity::Human);
    }

    #[test]
    fn test_anonymous_write_denied_with_guidance() {
        let err = ensure_write_allowed(&AuthConfig::disabled()).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        assert!(err.1.contains("--auth-token"), "guidance must be present");
    }

    #[test]
    fn test_human_write_allowed() {
        let enabled = AuthConfig::new(vec!["secret".to_string()], true);
        assert!(ensure_write_allowed(&enabled).is_ok());
    }

    #[test]
    fn test_ledger_records_operations() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = HumanGateLedger::new(dir.path());
        ledger.record("file_write", CallerIdentity::Human, "a.txt", "ok");
        ledger.record("file_delete", CallerIdentity::Anonymous, "b.txt", "denied");
        let content =
            std::fs::read_to_string(dir.path().join("data/human_gate_ledger.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["op"], "file_write");
        assert_eq!(first["identity"], "human");
        assert_eq!(first["outcome"], "ok");
        assert!(first["ts"].as_u64().is_some(), "timestamp must be present");
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["identity"], "anonymous");
        assert_eq!(second["outcome"], "denied");
    }
}
