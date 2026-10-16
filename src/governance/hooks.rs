//! 六类事件源接线点（事件类型清单）
//!
//! 每类事件源一个 `gov_*` 薄钩子：构造摘要级 [`GovDraft`] 并 fire-and-forget
//! 写入 gov 账本——进程未初始化 GovLedger（CLI 面/单测进程）时 no-op，
//! 写入失败由 record 内 `tracing::warn` 留痕，绝不阻断事件源主流程。
//!
//! | kind | 事件源 | 接线点 |
//! |---|---|---|
//! | `approval_requested` / `approval_resolved` | G8 审批卡（人工窗） | [`GovAuditedApproval`] 装饰器——build_approval_callback 手工档与 policy 降级路 |
//! | `policy_verdict` | policy 三态判定 | approval_policy.rs 判定输出点（步骤 6 留痕处） |
//! | `s4_publish` | S4 产物定稿 | WS `ClientMessage::S4Publish`（前端 markFinalized 首次定稿产物时经信号桥发帧） |
//! | `terminal_session_create` / `terminal_session_close` | PTY 会话生命周期 | pty_api.rs 创建/收尾（record-only 治理分级） |
//! | `b21_journal_milestone` | B21 journal 里程碑 | journal.rs resumed/compaction 落账点（镜像，refs 互指） |
//!
//! 高频事件不收录：`auto_policy` 判定式决策端逐工具调用
//! 触发、不产生审批卡，不接 approval_* 钩子（其判定全文已走 auto_policy
//! 理由必产留痕，治理真相面由 policy_verdict 与审批卡事件承载）。

use std::sync::Arc;

use async_trait::async_trait;

use super::ledger::{current_ledger, GovActor, GovEventKind, GovRefs, TerminalAction};
use crate::agent::approval::{ApprovalCallback, ApprovalDecision, ApprovalRequest};

/// 摘要上限（防膨胀；超出截断——隐私边界：gov 不存全文，全文走 refs 指针）
const SUMMARY_MAX_CHARS: usize = 240;

pub(crate) fn clamp_summary(s: String) -> String {
    if s.chars().count() <= SUMMARY_MAX_CHARS {
        return s;
    }
    let cut: String = s.chars().take(SUMMARY_MAX_CHARS).collect();
    format!("{cut}…")
}

/// 待写入治理事件草稿（纯构造器产物，单测锚定映射；fire 消费）
#[doc(hidden)]
pub(crate) struct GovDraft {
    pub kind: GovEventKind,
    pub session_id: String,
    pub actor: GovActor,
    pub summary: String,
    pub refs: GovRefs,
}

/// fire-and-forget 写入（未初始化=CLI/测试 no-op）
fn fire(draft: GovDraft) {
    if let Some(ledger) = current_ledger() {
        tokio::spawn(async move {
            let _ = ledger
                .record(
                    draft.kind,
                    &draft.session_id,
                    draft.actor,
                    clamp_summary(draft.summary),
                    draft.refs,
                )
                .await;
        });
    }
}

// =============================================================================
// 纯构造器（kind/actor/摘要/refs 映射单测锚定）
// =============================================================================

pub(crate) fn approval_requested_draft(req: &ApprovalRequest) -> GovDraft {
    GovDraft {
        kind: GovEventKind::ApprovalRequested,
        session_id: req.session_id.clone(),
        actor: GovActor::Agent,
        summary: format!("{} 请求批准：{}", req.tool_name, req.command),
        refs: GovRefs {
            approval_id: Some(req.proposal_id.clone()),
            ..Default::default()
        },
    }
}

pub(crate) fn approval_resolved_draft(req: &ApprovalRequest, d: &ApprovalDecision) -> GovDraft {
    // 行为者映射：审批卡人工决定=user；policy 评审/系统自动=policy
    let actor = if d.approver == "policy-review" || d.approver == "auto" {
        GovActor::Policy
    } else {
        GovActor::User
    };
    let outcome = if d.approved { "批准" } else { "拒绝" };
    let mut summary = format!("{} {}（{}）", req.tool_name, outcome, d.approver);
    if d.auto_rejected {
        summary.push_str(" 自动拒绝（超时/通道关闭）");
    }
    if !d.reason.is_empty() {
        summary.push_str(&format!("：{}", d.reason));
    }
    GovDraft {
        kind: GovEventKind::ApprovalResolved,
        session_id: req.session_id.clone(),
        actor,
        summary,
        refs: GovRefs {
            approval_id: Some(req.proposal_id.clone()),
            ..Default::default()
        },
    }
}

pub(crate) fn policy_verdict_draft(
    req: &ApprovalRequest,
    verdict: &str,
    conditions_summary: &str,
    override_flag: bool,
) -> GovDraft {
    let mut summary = format!(
        "{} policy 评审判定 {}（条件：{}{}）",
        req.tool_name,
        verdict,
        conditions_summary,
        if override_flag {
            "；豁免覆盖开启"
        } else {
            ""
        }
    );
    if summary.len() > SUMMARY_MAX_CHARS {
        // 判定依据摘要级（持久化镜像），超限截断
        summary = clamp_summary(summary);
    }
    GovDraft {
        kind: GovEventKind::PolicyVerdict,
        session_id: req.session_id.clone(),
        actor: GovActor::Policy,
        summary,
        refs: GovRefs {
            approval_id: Some(req.proposal_id.clone()),
            ..Default::default()
        },
    }
}

pub(crate) fn s4_publish_draft(session_id: &str, path: &str) -> GovDraft {
    GovDraft {
        kind: GovEventKind::S4Publish,
        session_id: session_id.to_string(),
        actor: GovActor::User,
        summary: format!("产物定稿：{path}"),
        refs: GovRefs::default(),
    }
}

/// AI commit message 生成（record-only：耗时+产物长度；生成写输入框可编辑、
/// 绝不自动提交——留痕只记动作不记 diff 内容）
pub(crate) fn ai_commit_draft(detail: &str) -> GovDraft {
    GovDraft {
        kind: GovEventKind::AiCommit,
        session_id: "workbench".to_string(),
        actor: GovActor::User,
        summary: format!("AI commit message 生成：{detail}"),
        refs: GovRefs::default(),
    }
}

pub(crate) fn terminal_session_draft(
    term_id: &str,
    action: TerminalAction,
    detail: &str,
) -> GovDraft {
    let (kind, summary) = match action {
        TerminalAction::Create => (
            GovEventKind::TerminalSession(TerminalAction::Create),
            format!("终端会话开启（{detail}）"),
        ),
        TerminalAction::Close => (
            GovEventKind::TerminalSession(TerminalAction::Close),
            format!("终端会话关闭（{detail}）"),
        ),
    };
    GovDraft {
        kind,
        session_id: term_id.to_string(),
        actor: GovActor::User,
        summary,
        refs: GovRefs::default(),
    }
}

pub(crate) fn journal_milestone_draft(
    session_id: &str,
    milestone: &str,
    journal_seq: u64,
) -> GovDraft {
    GovDraft {
        kind: GovEventKind::B21JournalMilestone,
        session_id: session_id.to_string(),
        actor: GovActor::Agent,
        summary: milestone.to_string(),
        refs: GovRefs {
            journal_seq: Some(journal_seq),
            ..Default::default()
        },
    }
}

// =============================================================================
// 薄钩子（事件源调用面）
// =============================================================================

/// 审批卡请求（G8 审批链，装饰器入口）
pub fn approval_requested(req: &ApprovalRequest) {
    fire(approval_requested_draft(req));
}

/// 审批卡决定（人工批/拒/超时自动拒/评审降级后的人工决定）
pub fn approval_resolved(req: &ApprovalRequest, decision: &ApprovalDecision) {
    fire(approval_resolved_draft(req, decision));
}

/// policy 三态判定（判定依据摘要的持久化镜像）
pub fn policy_verdict(
    req: &ApprovalRequest,
    verdict: &str,
    conditions_summary: &str,
    override_flag: bool,
) {
    fire(policy_verdict_draft(
        req,
        verdict,
        conditions_summary,
        override_flag,
    ));
}

/// S4 产物定稿（前端 markFinalized 首次定稿经 WS 信号桥通知）
pub fn s4_publish(session_id: &str, path: &str) {
    fire(s4_publish_draft(session_id, path));
}

/// AI commit message 生成（record-only）
pub fn ai_commit(detail: &str) {
    fire(ai_commit_draft(detail));
}

/// terminal PTY 会话生命周期（record-only）
pub fn terminal_session(term_id: &str, action: TerminalAction, detail: &str) {
    fire(terminal_session_draft(term_id, action, detail));
}

/// B21 journal 里程碑镜像（compaction 完成/resume 恢复；refs.journal_seq 互指）
pub fn journal_milestone(session_id: &str, milestone: &str, journal_seq: u64) {
    fire(journal_milestone_draft(session_id, milestone, journal_seq));
}

// =============================================================================
// 审批卡装饰器（approval_requested/resolved 事件源载体）
// =============================================================================

/// G8 审批卡治理留痕装饰器——包装 [`ApprovalCallback`]，入口/出口各落一条
/// gov 事件。仅用于**会产生人工审批卡**的决策端（manual 档 HttpApproval、
/// policy 档降级 fallback）；auto_policy 判定式无卡不包装（高频排除）。
pub struct GovAuditedApproval {
    inner: Arc<dyn ApprovalCallback>,
}

impl GovAuditedApproval {
    /// 包装底层审批决策端(构造器;装饰后 request_approval 自动落两条 gov 事件)
    pub fn new(inner: Arc<dyn ApprovalCallback>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl ApprovalCallback for GovAuditedApproval {
    async fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        approval_requested(req);
        let decision = self.inner.request_approval(req).await;
        approval_resolved(req, &decision);
        decision
    }
}

// =============================================================================
// 单测：六类 draft 映射（PR2 判据——事件源→gov 留痕映射锚定；live 断言
// 「每源触发→get_shared_facts 可查」随批次 E2E 实测）
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_req() -> ApprovalRequest {
        ApprovalRequest {
            session_id: "sess-9".to_string(),
            tool_name: "shell_exec".to_string(),
            args: serde_json::json!({"cmd": "cargo test"}),
            command: "cargo test".to_string(),
            risk: "低".to_string(),
            alternative: String::new(),
            proposal_id: "ap-18f3".to_string(),
        }
    }

    #[test]
    fn approval_requested_draft_maps_kind_actor_refs() {
        let d = approval_requested_draft(&sample_req());
        assert_eq!(d.kind, GovEventKind::ApprovalRequested);
        assert_eq!(d.session_id, "sess-9");
        assert_eq!(d.actor, GovActor::Agent);
        assert!(d.summary.contains("shell_exec") && d.summary.contains("cargo test"));
        assert_eq!(d.refs.approval_id.as_deref(), Some("ap-18f3"));
        assert!(d.refs.journal_seq.is_none());
    }

    #[test]
    fn approval_resolved_draft_maps_actor_by_approver() {
        let req = sample_req();
        // 人工批 → user
        let d = approval_resolved_draft(
            &req,
            &ApprovalDecision {
                approved: true,
                approver: "operator-a".to_string(),
                verified: true,
                reason: String::new(),
                auto_rejected: false,
                verdict_json: None,
            },
        );
        assert_eq!(d.kind, GovEventKind::ApprovalResolved);
        assert_eq!(d.actor, GovActor::User);
        assert!(d.summary.contains("批准") && d.summary.contains("operator-a"));
        // policy 评审 approver → policy
        let d = approval_resolved_draft(
            &req,
            &ApprovalDecision {
                approved: true,
                approver: "policy-review".to_string(),
                verified: false,
                reason: "policy-review: 3 条件全部符合".to_string(),
                auto_rejected: false,
                verdict_json: None,
            },
        );
        assert_eq!(d.actor, GovActor::Policy);
        // 超时自动拒 → policy + 自动拒绝标记
        let d = approval_resolved_draft(
            &req,
            &ApprovalDecision {
                approved: false,
                approver: "auto".to_string(),
                verified: false,
                reason: String::new(),
                auto_rejected: true,
                verdict_json: None,
            },
        );
        assert_eq!(d.actor, GovActor::Policy);
        assert!(d.summary.contains("自动拒绝"));
    }

    #[test]
    fn policy_verdict_draft_carries_verdict_and_override() {
        let d = policy_verdict_draft(&sample_req(), "approved", "3 条件全部符合", false);
        assert_eq!(d.kind, GovEventKind::PolicyVerdict);
        assert_eq!(d.actor, GovActor::Policy);
        assert!(d.summary.contains("approved") && d.summary.contains("3 条件全部符合"));
        assert!(!d.summary.contains("豁免覆盖"));
        let d = policy_verdict_draft(&sample_req(), "rejected", "2 条件未符合", true);
        assert!(d.summary.contains("rejected") && d.summary.contains("豁免覆盖开启"));
        assert_eq!(d.refs.approval_id.as_deref(), Some("ap-18f3"));
    }

    #[test]
    fn s4_and_terminal_and_milestone_drafts_map_shapes() {
        let d = s4_publish_draft("sess-9", "src/main.rs");
        assert_eq!(d.kind, GovEventKind::S4Publish);
        assert_eq!(d.actor, GovActor::User);
        assert!(d.summary.contains("src/main.rs"));

        let d = terminal_session_draft("term-1", TerminalAction::Create, "pwsh, pid=4242");
        assert_eq!(
            d.kind,
            GovEventKind::TerminalSession(TerminalAction::Create)
        );
        assert_eq!(d.session_id, "term-1");
        assert!(d.summary.contains("开启") && d.summary.contains("pid=4242"));

        let d = terminal_session_draft("term-1", TerminalAction::Close, "exited, code=0");
        assert_eq!(d.kind, GovEventKind::TerminalSession(TerminalAction::Close));

        let d = journal_milestone_draft("sess-9", "compaction performed", 77);
        assert_eq!(d.kind, GovEventKind::B21JournalMilestone);
        assert_eq!(d.actor, GovActor::Agent);
        assert_eq!(d.refs.journal_seq, Some(77));
    }

    #[test]
    fn clamp_summary_truncates_over_limit() {
        let short = "短摘要".to_string();
        assert_eq!(clamp_summary(short.clone()), short);
        let long = "字".repeat(SUMMARY_MAX_CHARS + 10);
        let clamped = clamp_summary(long);
        assert_eq!(clamped.chars().count(), SUMMARY_MAX_CHARS + 1); // 含省略号
        assert!(clamped.ends_with('…'));
    }
}
