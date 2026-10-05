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
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let line = entry.to_string();
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
