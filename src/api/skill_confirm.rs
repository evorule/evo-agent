// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 技能管理动作两段式确认（调用方身份模型根修层）：
//!
//! **挑战-确认语义**——特权管理动作（approve/revoke/import）在两段式
//! 模式开启时不再直接执行，而是签发短时挑战（`202`）；人工在壳
//! webview 之外的原生对话框上裁决后，壳持特权令牌消费挑战（confirm）
//! 才真正执行。**执行通道物理隔离在 webview 沙箱之外**——即便调用方
//! 偷到特权令牌（webview localStorage），无人裁决挑战就不会执行，
//! 令牌窃取不再等价于管理面通行证。
//!
//! **会话级授权**（壳每引导一次裁决）：壳在首次挑战时弹原生
//! 「允许本会话技能变更」，允许后经 grant 端点置位（serve 进程内存态，
//! 随 serve 重启消散）；授权在位期间管理动作直接执行（不再签发挑战），
//! 账本 `confirmed_via=session-grant` 留痕——把关粒度=壳会话，授权前/
//! 拒绝时回落逐次确认。
//!
//! **模式开关**：仅桌面壳置 `EVO_AGENT_SKILLS__CHALLENGE_CONFIRM` 时启用
//! 两段式；纯 web（无壳，无原生对话框承载面）不带该 env=单段直执
//! （旧行为逐字节兼容，缺省不变式）。
//!
//! 挑战为内存态（不落盘）：serve 重启即全消散，重发即可；一次性消费，
//! TTL 到期惰性清除。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 挑战有效期（秒）：原生对话框是模态交互，人在场即应答；过长滞留
/// 增大窗口，过短打断人在场的正常裁决。10 分钟=宽松人在场口径。
pub const CHALLENGE_TTL_SECS: u64 = 600;

/// 挑战动作判别（载荷按动作反序列化回原请求体执行）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeAction {
    /// 批准技能（SKILL.md 泄露检查通过后钉哈希置 active）
    Approve,
    /// 撤销技能（三态置 revoked，账本保留历史行）
    Revoke,
    /// 导入 skill-pack bundle（解包落盘，导入≠生效）
    Import,
}

impl ChallengeAction {
    /// 线格式标签（账本/日志字符串形态）
    pub fn as_str(self) -> &'static str {
        match self {
            ChallengeAction::Approve => "approve",
            ChallengeAction::Revoke => "revoke",
            ChallengeAction::Import => "import",
        }
    }

    /// 从字符串判别（confirm 消费侧路由用）
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "approve" => Some(ChallengeAction::Approve),
            "revoke" => Some(ChallengeAction::Revoke),
            "import" => Some(ChallengeAction::Import),
            _ => None,
        }
    }
}

/// 挑战状态机：Pending →(confirm) Executed | (deny) Denied | (TTL) Expired
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeState {
    /// 已签发待裁决
    Pending,
    /// 已确认执行（result 携带原动作响应体）
    Executed,
    /// 原生对话框拒绝
    Denied,
    /// TTL 到期未裁决
    Expired,
}

impl ChallengeState {
    /// 线格式标签（轮询面字符串形态）
    pub fn as_str(self) -> &'static str {
        match self {
            ChallengeState::Pending => "pending",
            ChallengeState::Executed => "executed",
            ChallengeState::Denied => "denied",
            ChallengeState::Expired => "expired",
        }
    }
}

#[derive(Debug, Clone)]
struct Challenge {
    action: ChallengeAction,
    payload: serde_json::Value,
    /// 签发时刻（unix 秒；审计回看用，当前无读取面——forensic 保留字段）
    #[allow(dead_code)]
    created_at: u64,
    expires_at: u64,
    state: ChallengeState,
    /// 执行产物（原端点响应体；Executed 后经 set_result 回填供轮询面取）
    result: Option<serde_json::Value>,
}

/// 挑战库 + 会话授权态（serve 进程内存；无盘面）
#[derive(Debug)]
pub struct SkillConfirmState {
    challenges: Mutex<HashMap<String, Challenge>>,
    seq: AtomicU64,
    /// 会话级授权（壳引导裁决后置位；serve 重启即消散）
    session_grant: AtomicBool,
    /// 两段式模式开关（启动期从 env 读入定死；运行期只读）
    armed: bool,
}

impl SkillConfirmState {
    /// 构造（armed=两段式模式开关；serve 启动期由 env 注入定死）
    pub fn new(armed: bool) -> Self {
        Self {
            challenges: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
            session_grant: AtomicBool::new(false),
            armed,
        }
    }

    /// 两段式模式是否开启（壳置 env；纯 web=false）
    pub fn armed(&self) -> bool {
        self.armed
    }

    /// 会话级授权是否在位
    pub fn session_grant_active(&self) -> bool {
        self.session_grant.load(Ordering::SeqCst)
    }

    /// 置/清会话级授权（特权面端点调用；壳原生裁决后到达）
    pub fn set_session_grant(&self, enabled: bool) {
        self.session_grant.store(enabled, Ordering::SeqCst);
    }

    /// 当前 unix 秒（测试确定性与溢出防御共用）
    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// 签发挑战（调用方已过写面+特权面门；载荷=原请求体）。
    /// 返回 (challenge_id, expires_at unix secs)。id = 时间+进程内单调
    /// 序号（进程内唯一；serve 重启消散旧库，无跨进程唯一性需求）。
    pub fn issue(&self, action: ChallengeAction, payload: serde_json::Value) -> (String, u64) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let now = Self::now();
        let id = format!("chg-{now}-{seq}");
        let expires_at = now + CHALLENGE_TTL_SECS;
        let mut challenges = self.challenges.lock().unwrap_or_else(|p| p.into_inner());
        Self::sweep(&mut challenges, now);
        challenges.insert(
            id.clone(),
            Challenge {
                action,
                payload,
                created_at: now,
                expires_at,
                state: ChallengeState::Pending,
                result: None,
            },
        );
        (id, expires_at)
    }

    /// 取挑战状态视图（含执行结果载荷；惰性清过期）
    pub fn status(&self, id: &str) -> Option<ChallengeStatusView> {
        let mut challenges = self.challenges.lock().unwrap_or_else(|p| p.into_inner());
        Self::sweep(&mut challenges, Self::now());
        let c = challenges.get_mut(id)?;
        Some(ChallengeStatusView {
            challenge_id: id.to_string(),
            action: c.action.as_str().to_string(),
            state: c.state.as_str().to_string(),
            expires_at: c.expires_at,
            created_at: c.created_at,
            result: c.result.clone(),
        })
    }

    /// 只读窥探挑战载荷（deny 端点构建账本标签用；不改状态）
    pub fn peek_payload(&self, id: &str) -> Option<serde_json::Value> {
        let challenges = self.challenges.lock().unwrap_or_else(|p| p.into_inner());
        challenges.get(id).map(|c| c.payload.clone())
    }

    /// 消费挑战（confirm 路径）：Pending 且未过期 → 置 Executed 并返回
    /// (action, payload)；其余（不存在/已消费/已拒/已过期）→ None（调方
    /// 决定 404/410 口径，status 端点可查询细分态）
    pub fn consume(&self, id: &str) -> Option<(ChallengeAction, serde_json::Value)> {
        let mut challenges = self.challenges.lock().unwrap_or_else(|p| p.into_inner());
        Self::sweep(&mut challenges, Self::now());
        let c = challenges.get_mut(id)?;
        if c.state != ChallengeState::Pending {
            return None;
        }
        let now = Self::now();
        if now >= c.expires_at {
            c.state = ChallengeState::Expired;
            return None;
        }
        c.state = ChallengeState::Executed;
        Some((c.action, c.payload.clone()))
    }

    /// 回填执行产物（confirm 端点执行原动作成功后调用；轮询面由此取结果）
    pub fn set_result(&self, id: &str, result: serde_json::Value) {
        let mut challenges = self.challenges.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(c) = challenges.get_mut(id) {
            c.result = Some(result);
        }
    }

    /// 拒绝挑战（壳原生对话框「拒绝」路径）：Pending → Denied
    pub fn deny(&self, id: &str) -> bool {
        let mut challenges = self.challenges.lock().unwrap_or_else(|p| p.into_inner());
        Self::sweep(&mut challenges, Self::now());
        let Some(c) = challenges.get_mut(id) else {
            return false;
        };
        if c.state != ChallengeState::Pending {
            return false;
        }
        c.state = ChallengeState::Denied;
        true
    }

    /// 惰性清理：Pending 过期置 Expired；终态超窗移除
    fn sweep(challenges: &mut HashMap<String, Challenge>, now: u64) {
        challenges.retain(|_, c| {
            if c.state == ChallengeState::Pending && now >= c.expires_at {
                c.state = ChallengeState::Expired;
            }
            // 终态保留一段窗口供 status 查询（过期后再留一个 TTL），超时清
            !(matches!(
                c.state,
                ChallengeState::Executed | ChallengeState::Denied | ChallengeState::Expired
            ) && now >= c.expires_at + CHALLENGE_TTL_SECS)
        });
    }
}

/// 挑战签发视图（202 响应体）
#[derive(Debug, serde::Serialize)]
pub struct ChallengeIssuedView {
    /// 挑战标识（confirm/deny/轮询的引用键）
    pub challenge_id: String,
    /// 动作判别（approve/revoke/import）
    pub action: String,
    /// 有效期（秒）
    pub expires_in: u64,
}

/// 挑战状态视图（GET 轮询响应体；result=执行产物，Executed 态才有值）
#[derive(Debug, serde::Serialize)]
pub struct ChallengeStatusView {
    /// 挑战标识
    pub challenge_id: String,
    /// 动作判别
    pub action: String,
    /// 状态（pending/executed/denied/expired）
    pub state: String,
    /// 到期时刻（unix 秒）
    pub expires_at: u64,
    /// 签发时刻（unix 秒；供轮询面显示等待时长）
    pub created_at: u64,
    /// 执行产物（原动作响应体；Executed 态才有值）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

/// 确认/拒绝请求体
#[derive(Debug, serde::Deserialize)]
pub struct ConfirmRequest {
    /// 挑战标识
    pub challenge_id: String,
}

/// 会话授权请求体
#[derive(Debug, serde::Deserialize)]
pub struct SessionGrantRequest {
    /// true=置位（直执），false=撤销（回落挑战模式）
    pub enable: bool,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::time::Duration;

    #[test]
    fn issue_and_consume_roundtrip() {
        let st = SkillConfirmState::new(true);
        let (id, expires_at) = st.issue(ChallengeAction::Approve, serde_json::json!({"k": 1}));
        assert!(expires_at > 0);
        assert_eq!(st.status(&id).unwrap().state, "pending");
        let (action, payload) = st.consume(&id).unwrap();
        assert_eq!(action, ChallengeAction::Approve);
        assert_eq!(payload["k"], 1);
        st.set_result(&id, serde_json::json!({"status": "active"}));
        let view = st.status(&id).unwrap();
        assert_eq!(view.state, "executed");
        assert_eq!(view.result.unwrap()["status"], "active");
        // 一次性：二次消费为 None
        assert!(st.consume(&id).is_none());
    }

    #[test]
    fn deny_kills_pending_only() {
        let st = SkillConfirmState::new(true);
        let (id, _) = st.issue(ChallengeAction::Revoke, serde_json::json!({}));
        assert!(st.deny(&id));
        assert_eq!(st.status(&id).unwrap().state, "denied");
        assert!(st.consume(&id).is_none());
        assert!(!st.deny(&id), "终态不可再拒");
        assert!(!st.deny("chg-nonexistent"));
    }

    #[test]
    fn session_grant_flag_roundtrip() {
        let st = SkillConfirmState::new(true);
        assert!(!st.session_grant_active());
        st.set_session_grant(true);
        assert!(st.session_grant_active());
        st.set_session_grant(false);
        assert!(!st.session_grant_active());
    }

    #[test]
    fn unknown_id_status_none() {
        let st = SkillConfirmState::new(true);
        assert!(st.status("nope").is_none());
    }

    #[test]
    fn ttl_constant_reasonable() {
        assert!(CHALLENGE_TTL_SECS >= 60, "至少 1 分钟人在场窗口");
        assert!(CHALLENGE_TTL_SECS <= 3600, "不超过 1 小时滞留窗");
        let _ = Duration::from_secs(CHALLENGE_TTL_SECS);
    }
}
