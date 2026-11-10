// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 审批策略条件文件与红线豁免清单（审批三档制封存基座，2026-09-26 设计/2026-10-15 实施）
//!
//! 职责边界（SEAL 红线：判定依据 serve 侧封存，agent 上下文只有只读副本）：
//! - 条件文件 `.evo/approval-policy.toml` 由 serve 侧独占读取——人类预置审批条件
//!   按「每工具组 conditions + policy_limits 通用限制」组织；加载失败 fail-visible：
//!   调用方必须将 policy 档降级 manual 并显式留痕，不得静默按无条件放行处理
//! - 红线豁免清单（默认强制人工审批）：命中任何一行的操作在任何审批档位下强制
//!   回落 manual；`approval.exemptionOverride=true`（用户显式关闭）可解除强制，
//!   但判定留痕必须加 `exemption_overridden` 标记（由决策端消费方落账，本模块
//!   只提供判定原语）；deny 规则（blocked）优先于一切配置，override 亦不可关闭

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use tracing::{info, warn};

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::agent::approval::{ApprovalCallback, ApprovalDecision, ApprovalRequest};

// ===== 评审决策端（policy 档：独立评审调用+条件逐条判定） =====

/// 评审调用超时（秒）——无响应按「判定不可信兜底」降级人工
pub const POLICY_REVIEW_TIMEOUT_SECS: u64 = 30;

/// 评审员决策端标识（tool_trace approval 子对象 approver 字段）
pub const POLICY_REVIEW_APPROVER: &str = "policy-review";

/// 评审员 system prompt（评审独立会话专用——不含 agent 执行上下文与对话历史，
/// 独立性保证：杜绝自我审批偏袒）
pub const REVIEW_SYSTEM_PROMPT: &str = r#"你是审批评审员。对照以下逐条条件判定该操作是否可放行。只依据给定操作详情与条件判定，不推测意图。

判定依据分两源，优先级从高到低：
1. 拒绝规则：操作详情命中拒绝规则（含红线类操作）即拒绝，任何其他来源不可覆盖；
2. 预置条件：对条件清单逐条判定。

条件清单只含自然语言条件——机械可核验条件（白名单/排除/外部验证器）已由系统先行裁决并给出证据，不在你的判定范围内。每条条件输出 pass（true=符合 / false=不符合 / null=无法判定）与 evidence（判定依据摘要）；存在无法判定的条件时 verdict 置为 abstained。输出且仅输出一个 JSON 对象，格式：
{"conditions":[{"id":1,"pass":true,"evidence":"..."}],"verdict":"approved|rejected|abstained","reason":"总体判定理由"}"#;

/// 条件判定项（评审员输出契约）
#[derive(Debug, Clone, Deserialize)]
pub struct ConditionJudgement {
    /// 条件序号（评审 payload 中 1 起编号，对应条件清单行号）
    pub id: u32,
    /// true=符合 / false=不符合 / null=无法判定（弃权——不计符合也不计不符合）
    pub pass: Option<bool>,
    /// 判定依据摘要（入留痕；评审员自行截取的原文要点）
    #[serde(default)]
    pub evidence: String,
}

/// 评审判定（决策端解析目标）
///
/// verdict 三态由决策端按条件结果**机械重算**（approve 语义不可由评审自报
/// 单方面决定）：任何条件弃权→整体弃权；有不符合→拒绝；全部符合→放行；
/// 条件清单为空→弃权（评审未按契约输出=判定不可信）。评审自报 verdict
/// 字段仅入留痕比对，不参与分派。
#[derive(Debug, Clone, Deserialize)]
pub struct PolicyJudgement {
    /// 逐条条件判定结果（缺失=空清单→整体弃权）
    #[serde(default)]
    pub conditions: Vec<ConditionJudgement>,
    /// 评审员总体理由（入留痕；Rejected 时随修正指令回喂 agent）
    #[serde(default)]
    pub reason: String,
}

/// 重算后的判定三态（CONFIRMED/REFUTED/ABSTAINED 对应放行/拒绝/弃权）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewVerdict {
    /// 全部条件符合→放行（清修正计数）
    Approved,
    /// 存在不符合条件→拒绝（修正指令回喂 agent，计数+1）
    Rejected,
    /// 任何条件弃权/清单为空→判定不可信→降级人工
    Abstained,
}

impl ReviewVerdict {
    /// 结构化日志/留痕用小写串
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Abstained => "abstained",
        }
    }
}

impl PolicyJudgement {
    /// 三态机械重算（不信评审自报 verdict）：
    /// 空清单/任一弃权→Abstained；有不符合→Rejected；全符合→Approved
    pub fn recomputed_verdict(&self) -> ReviewVerdict {
        if self.conditions.is_empty() {
            return ReviewVerdict::Abstained;
        }
        let mut any_fail = false;
        for c in &self.conditions {
            match c.pass {
                None => return ReviewVerdict::Abstained,
                Some(false) => any_fail = true,
                Some(true) => {}
            }
        }
        if any_fail {
            ReviewVerdict::Rejected
        } else {
            ReviewVerdict::Approved
        }
    }
}

/// 评审通道抽象——调用方以独立评审调用实现（评审专用 system prompt +
/// 非流式单轮补全，经审计留痕桥走 sidecar 会话，无静默直连兜底）；
/// 决策端只依赖本 trait，测试可注入桩
#[async_trait]
#[allow(clippy::double_must_use)] // async_trait 宏生成的 must_use 与 Boxed Future 类型语义重复,源上无法消（同 approval.rs ApprovalCallback 先例）
pub trait PolicyReviewInvoker: Send + Sync {
    /// 提交一次评审请求，返回评审员原始输出文本（ Err=调用失败）
    async fn review(&self, user_payload: &str) -> Result<String, String>;
}

/// 评审调用的审计 purpose 值（sidecar 命令事实 audit_purpose 键；
/// 新 purpose 接入需同步核对 journal 侧 LlmCalled.purpose 枚举（journal.rs）
/// 与 ATIF 导出映射（atif.rs 跳过臂））
pub const POLICY_REVIEW_PURPOSE: &str = "policy_review";

/// AuditedLlm 评审适配器——独立评审调用经审计链 sidecar 协议执行
/// （评审 prompt 与判定全文均入 evorule 审计链；无静默直连兜底，
/// Err=判定不可信，由决策端降级人工）
pub struct AuditedReviewInvoker {
    audited: crate::agent::audited_llm::AuditedLlm,
}

impl AuditedReviewInvoker {
    /// 从审计留痕桥构造（client=evorule 会话面，llm=经执行体中继的 LLM handler）
    pub fn new(audited: crate::agent::audited_llm::AuditedLlm) -> Self {
        Self { audited }
    }
}

#[async_trait]
impl PolicyReviewInvoker for AuditedReviewInvoker {
    async fn review(&self, user_payload: &str) -> Result<String, String> {
        // temperature=0 保证评审最大确定性；messages 形状与 core_eval 规则
        // 引用的 instruction.params.messages 契约一致。model 键必带（服务端
        // call_external 规则要求 instruction.params.model，缺失即 TCB 拒、
        // 评审静默降级人工——o31 实录真因）。
        let params = serde_json::json!({
            "model": self.audited.review_model(),
            "messages": [
                { "role": "system", "content": REVIEW_SYSTEM_PROMPT },
                { "role": "user", "content": user_payload },
            ],
            "temperature": 0.0,
            "max_tokens": 2048,
        });
        let out = self.audited.execute(POLICY_REVIEW_PURPOSE, &params).await?;
        out.get("content")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "audited review response missing 'content'".to_string())
    }
}

/// policy 档评审式决策端
///
/// 流程：红线豁免检查（命中且未显式关闭→强制人工）→ 条件清单拉取
/// （空清单 fail-visible 降级人工）→ 修正重查上限检查 → 三层判定管线：
/// ①机械层（match/exclude 条目零 LLM 求值，任一不符→拒绝并回喂修正指令，
/// 跳过后层）→ ②验证层（verify 条目 serve 侧执行，exit 0=符合 / 非 0=不符 /
/// spawn 失败与超时=判定不可信降级人工）→ ③LLM 兜底层（仅存自然语言条件时
/// 调用独立评审，30s 超时；全机械全过=零 LLM 快路径）。任何判定不可信环节
/// （超时/调用失败/解析失败/条件弃权）一律降级人工，绝不静默放行。
pub struct PolicyReview {
    policy: ApprovalPolicyFile,
    exemption_override: bool,
    invoker: Arc<dyn PolicyReviewInvoker>,
    fallback: Arc<dyn ApprovalCallback>,
    corrections: Mutex<HashMap<String, u32>>,
    /// 会话工作区根（verify 条目的执行工作目录；None=verify 条目视为不可
    /// 执行→降级人工，fail-closed）
    workdir: Option<std::path::PathBuf>,
    /// verify 条目执行通道（生产=tokio 进程 spawn；测试=打桩）
    verify_executor: Arc<dyn VerifyExecutor>,
}

impl PolicyReview {
    /// 构造（policy=条件文件解析结果；invoker=评审通道；fallback=降级人工端；
    /// exemption_override=用户显式关闭红线强制——留痕必带 exemption_overridden
    /// 标记；workdir=会话工作区根，None 时 verify 条目降级人工）
    pub fn new(
        policy: ApprovalPolicyFile,
        invoker: Arc<dyn PolicyReviewInvoker>,
        fallback: Arc<dyn ApprovalCallback>,
        exemption_override: bool,
        workdir: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            policy,
            exemption_override,
            invoker,
            fallback,
            corrections: Mutex::new(HashMap::new()),
            verify_executor: match &workdir {
                Some(w) => Arc::new(ProcessVerifyExecutor::new(w)),
                None => Arc::new(UnboundVerifyExecutor),
            },
            workdir,
        }
    }

    /// 注入自定义验证器执行通道（测试打桩面；生产构造用缺省即可）
    pub fn with_verify_executor(mut self, executor: Arc<dyn VerifyExecutor>) -> Self {
        self.verify_executor = executor;
        self
    }

    /// 修正重查指纹键（同会话+同工具+同命令的重查共享计数）
    fn correction_key(req: &ApprovalRequest) -> String {
        format!("{}|{}|{}", req.session_id, req.tool_name, req.command)
    }

    fn corrections_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, u32>> {
        self.corrections.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 豁免覆盖标记（红线命中且 override=true 时 reason 尾缀）
    fn override_marker(&self, req: &ApprovalRequest) -> String {
        let red_line = hits_red_line_for_request(req);
        if red_line.is_some() {
            format!(
                " [exemption_overridden:{}]",
                red_line.map(|r| r.as_str()).unwrap_or("")
            )
        } else {
            String::new()
        }
    }

    /// 机械/验证层拒绝分派（修正指令携未符合条目 evidence；修正计数+1；
    /// 判定全文 seal 留痕——验证器结论可跨重启回放核验）
    async fn reject_mechanical(
        &self,
        req: &ApprovalRequest,
        mech: &[MechEntry],
        key: &str,
    ) -> ApprovalDecision {
        let marker = self.override_marker(req);
        info!(
            tool = %req.tool_name,
            proposal_id = %req.proposal_id,
            verdict = "rejected",
            override_flag = self.exemption_override,
            request = %serde_json::to_string(&req.args).unwrap_or_default(),
            judgement = "(mechanical evaluation, no LLM call)",
            "policy review judged"
        );
        crate::governance::hooks::policy_verdict(
            req,
            "rejected",
            &mech_summary(mech),
            self.exemption_override,
        );
        let verdict_seal = serde_json::json!({
            "proposal_id": req.proposal_id,
            "tool": req.tool_name,
            "verdict": "rejected",
            "exemption_override": self.exemption_override,
            "request": serde_json::to_string(&req.args).unwrap_or_default(),
            "judgement": { "mechanical": true },
            "mechanical": mech.iter().map(|e| e.to_json()).collect::<Vec<_>>(),
        })
        .to_string();
        *self.corrections_lock().entry(key.to_string()).or_insert(0) += 1;
        ApprovalDecision {
            approved: false,
            approver: POLICY_REVIEW_APPROVER.to_string(),
            verified: false,
            reason: format!(
                "policy-review: 条件未全部符合，请修正操作后重试。未符合条目：{}{}",
                mech_failures(mech),
                marker
            ),
            auto_rejected: false,
            verdict_json: Some(verdict_seal),
        }
    }
}

#[async_trait]
impl ApprovalCallback for PolicyReview {
    async fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        // 1. 红线豁免：默认任何档位强制人工；用户显式关闭（override=true）
        //    仅解除强制，评审留痕必须携带 exemption_overridden 标记
        let red_line = hits_red_line_for_request(req);
        if red_line.is_some() && !self.exemption_override {
            info!(
                tool = %req.tool_name,
                marker = red_line.map(|r| r.as_str()).unwrap_or(""),
                "policy review: red line hit, forced manual approval"
            );
            return self.fallback.request_approval(req).await;
        }

        // 2. 条件清单：空=未预置（fail-visible——无条件不可评审，降级人工）
        let conditions = self.policy.conditions_for(&req.tool_name);
        if conditions.is_empty() {
            warn!(
                tool = %req.tool_name,
                "policy review: no conditions configured for tool, fallback to manual"
            );
            return self.fallback.request_approval(req).await;
        }

        // 3. 修正重查上限：同指纹重查次数达到 max_corrections 即降级人工（防无限循环）
        let key = Self::correction_key(req);
        let max = self.policy.policy_limits.max_corrections;
        let used = *self.corrections_lock().get(&key).unwrap_or(&0);
        if used >= max {
            warn!(
                tool = %req.tool_name,
                used,
                max,
                "policy review: correction limit reached, fallback to manual"
            );
            self.corrections_lock().remove(&key);
            return self.fallback.request_approval(req).await;
        }

        // 4. 机械层（零 LLM）：match/exclude 条目逐条求值；NL/verify 条目归后层
        let mut mech: Vec<MechEntry> = Vec::new();
        let mut failed: Vec<MechEntry> = Vec::new();
        let mut nl: Vec<(u32, String)> = Vec::new();
        let mut verify_conds: Vec<(u32, &VerifySpec)> = Vec::new();
        for (i, cond) in conditions.iter().enumerate() {
            let id = (i + 1) as u32;
            match cond {
                PolicyCondition::Natural(s) => nl.push((id, s.clone())),
                PolicyCondition::Match { patterns, .. } => {
                    let (pass, evidence) = eval_match(patterns, &req.args);
                    let e = MechEntry {
                        id,
                        kind: "match",
                        pass,
                        evidence,
                    };
                    if !e.pass {
                        failed.push(e.clone());
                    }
                    mech.push(e);
                }
                PolicyCondition::Exclude { terms, .. } => {
                    let (pass, evidence) = eval_exclude(terms, &req.args);
                    let e = MechEntry {
                        id,
                        kind: "exclude",
                        pass,
                        evidence,
                    };
                    if !e.pass {
                        failed.push(e.clone());
                    }
                    mech.push(e);
                }
                PolicyCondition::PathPrefix { prefixes, .. } => {
                    let (pass, evidence) = eval_path_prefix(prefixes, &req.args);
                    let e = MechEntry {
                        id,
                        kind: "path_prefix",
                        pass,
                        evidence,
                    };
                    if !e.pass {
                        failed.push(e.clone());
                    }
                    mech.push(e);
                }
                PolicyCondition::Verify { spec, .. } => verify_conds.push((id, spec)),
            }
        }
        if !failed.is_empty() {
            // 任一机械不符 → Rejected（修正指令携该条 evidence，计数+1，
            // 跳过验证层与 LLM 兜底层）
            return self.reject_mechanical(req, &mech, &key).await;
        }

        // 5. 验证层：逐条执行 verify 命令（不经 shell，workdir=会话工作区根；
        //    exit 0=符合 / exit≠0=不符 / spawn 失败与超时=判定不可信降级人工）
        for (id, spec) in verify_conds {
            if self.workdir.is_none() {
                warn!(
                    tool = %req.tool_name,
                    "policy review: verify condition present but no workdir bound, fallback to manual"
                );
                return self.fallback.request_approval(req).await;
            }
            let outcome = tokio::time::timeout(
                Duration::from_secs(spec.timeout_secs),
                self.verify_executor.run(&spec.argv),
            )
            .await;
            let entry = match outcome {
                Err(_) => {
                    warn!(
                        tool = %req.tool_name,
                        timeout_secs = spec.timeout_secs,
                        "policy review verify command timed out, fallback to manual"
                    );
                    return self.fallback.request_approval(req).await;
                }
                Ok(Err(e)) => {
                    warn!(
                        tool = %req.tool_name,
                        error = %e,
                        "policy review verify command failed to run, fallback to manual"
                    );
                    return self.fallback.request_approval(req).await;
                }
                Ok(Ok(out)) => {
                    let pass = out.exit_code == 0;
                    let evidence = if pass {
                        "verifier exit 0".to_string()
                    } else {
                        format!(
                            "verifier exit {}；输出尾段: {}",
                            out.exit_code, out.output_tail
                        )
                    };
                    MechEntry {
                        id,
                        kind: "verify",
                        pass,
                        evidence,
                    }
                }
            };
            if !entry.pass {
                // 验证器只能收紧判定（不符→Rejected），永远不能放宽——
                // 外部锚点是「修正到符合」的 serve 侧实证，不是旁路
                failed.push(entry.clone());
                mech.push(entry);
                return self.reject_mechanical(req, &mech, &key).await;
            }
            mech.push(entry);
        }

        // 6. LLM 兜底层：仅存自然语言条件时调用（payload 只含 NL 条件+已通过
        //    机械条目证据摘要）；全机械全过=零 LLM 快路径，评审 invoker 零调用
        let fast_path = nl.is_empty();
        let payload = if fast_path {
            String::new()
        } else {
            build_review_payload(req, &nl, &mech)
        };
        let raw = if fast_path {
            String::new()
        } else {
            // 独立评审调用（30s 超时；失败/超时=判定不可信→降级人工）
            match tokio::time::timeout(
                Duration::from_secs(POLICY_REVIEW_TIMEOUT_SECS),
                self.invoker.review(&payload),
            )
            .await
            {
                Ok(Ok(raw)) => raw,
                Ok(Err(e)) => {
                    warn!(tool = %req.tool_name, error = %e, "policy review invocation failed, fallback to manual");
                    return self.fallback.request_approval(req).await;
                }
                Err(_) => {
                    warn!(
                        tool = %req.tool_name,
                        timeout_secs = POLICY_REVIEW_TIMEOUT_SECS,
                        "policy review timed out, fallback to manual"
                    );
                    return self.fallback.request_approval(req).await;
                }
            }
        };
        // 判定解析+三态机械重算（解析失败=判定不可信→降级人工）；
        // 机械重算逻辑对 NL 子集不变（评审自报 verdict 不作数）
        let judgement = if fast_path {
            None
        } else {
            match parse_judgement(&raw) {
                Ok(j) => Some(j),
                Err(e) => {
                    warn!(tool = %req.tool_name, error = %e, "policy review judgement unparseable, fallback to manual");
                    return self.fallback.request_approval(req).await;
                }
            }
        };
        let verdict = judgement
            .as_ref()
            .map(|j| j.recomputed_verdict())
            .unwrap_or(ReviewVerdict::Approved);

        // 7. 留痕：判定请求与全文进 serve 侧结构化日志（封存面）；决策摘要随
        //    ApprovalDecision.reason 进 tool_trace approval 子对象入审计链
        let args_summary = serde_json::to_string(&req.args).unwrap_or_default();
        info!(
            tool = %req.tool_name,
            proposal_id = %req.proposal_id,
            verdict = verdict.as_str(),
            override_flag = self.exemption_override,
            request = %if fast_path { &args_summary } else { &payload },
            judgement = %if fast_path { "(mechanical evaluation, no LLM call)" } else { &raw },
            "policy review judged"
        );
        // B22:policy_verdict 治理事件（三态判定+判定依据摘要——serve 侧封存
        // 锚点的持久化镜像，best-effort 不阻断分派）
        let conditions_summary = match &judgement {
            None => mech_summary(&mech),
            Some(j) if matches!(verdict, ReviewVerdict::Approved) => {
                format!("{}；{}", mech_summary(&mech), summary_of(j))
            }
            Some(j) => format!(
                "{}；{}；总体理由：{}",
                mech_summary(&mech),
                failures_of(j),
                j.reason
            ),
        };
        crate::governance::hooks::policy_verdict(
            req,
            verdict.as_str(),
            &conditions_summary,
            self.exemption_override,
        );

        // 判定全文封存 JSON（随 ApprovalDecision 下行，runner 落账 journal——
        // 进程日志与 gov 摘要镜像之外的跨重启回放权威面；mechanical 数组
        // 记录机械/验证层逐条结论，「是验证器说的，不是评审说的」可核验）
        let seal_judgement = match &judgement {
            Some(_) => serde_json::from_str::<serde_json::Value>(&raw)
                .unwrap_or_else(|_| serde_json::Value::String(raw.clone())),
            None => serde_json::json!({ "mechanical": true }),
        };
        let verdict_seal = serde_json::json!({
            "proposal_id": req.proposal_id,
            "tool": req.tool_name,
            "verdict": verdict.as_str(),
            "exemption_override": self.exemption_override,
            "request": if fast_path { args_summary } else { payload.clone() },
            "judgement": seal_judgement,
            "mechanical": mech.iter().map(|e| e.to_json()).collect::<Vec<_>>(),
        })
        .to_string();

        // 8. 分派
        let marker = if red_line.is_some() {
            format!(
                " [exemption_overridden:{}]",
                red_line.map(|r| r.as_str()).unwrap_or("")
            )
        } else {
            String::new()
        };
        match verdict {
            ReviewVerdict::Approved => {
                self.corrections_lock().remove(&key);
                let approver_prefix = if fast_path {
                    "policy-review[mechanical]"
                } else {
                    "policy-review"
                };
                let detail = match &judgement {
                    Some(j) => summary_of(j),
                    None => mech_summary(&mech),
                };
                ApprovalDecision {
                    approved: true,
                    approver: POLICY_REVIEW_APPROVER.to_string(),
                    verified: false,
                    reason: format!("{approver_prefix}: {detail} 条件全部符合{}", marker),
                    auto_rejected: false,
                    verdict_json: Some(verdict_seal.clone()),
                }
            }
            ReviewVerdict::Rejected => {
                *self.corrections_lock().entry(key).or_insert(0) += 1;
                let j = judgement
                    .as_ref()
                    .expect("rejected verdict must carry judgement");
                ApprovalDecision {
                    approved: false,
                    approver: POLICY_REVIEW_APPROVER.to_string(),
                    verified: false,
                    reason: format!(
                        "policy-review: 条件未全部符合，请修正操作后重试。未符合条目：{}；总体理由：{}{}",
                        failures_of(j),
                        j.reason,
                        marker
                    ),
                    auto_rejected: false,
                    verdict_json: Some(verdict_seal),
                }
            }
            ReviewVerdict::Abstained => {
                warn!(
                    tool = %req.tool_name,
                    "policy review abstained (verifier fault or unjudgeable), fallback to manual"
                );
                self.fallback.request_approval(req).await
            }
        }
    }
}

/// 从评审原始输出提取判定 JSON（容忍 ```json 围栏与前后缀文本）
fn parse_judgement(raw: &str) -> Result<PolicyJudgement, String> {
    let trimmed = raw.trim();
    let candidate = if trimmed.starts_with('{') {
        trimmed.to_string()
    } else {
        let start = trimmed.find('{').ok_or("no JSON object found")?;
        let end = trimmed.rfind('}').ok_or("no JSON object terminator")?;
        if end <= start {
            return Err("malformed JSON object".to_string());
        }
        trimmed[start..=end].to_string()
    };
    serde_json::from_str(&candidate).map_err(|e| format!("judgement parse: {e}"))
}

/// 组装评审请求（操作详情+自然语言条件清单+已通过机械条目证据摘要——
/// 不含 agent 对话上下文与执行意图；机械/验证条目已由系统先行裁决，
/// 不进条件清单，评审员只判 NL 子集）
fn build_review_payload(
    req: &ApprovalRequest,
    nl_conditions: &[(u32, String)],
    passed_mech: &[MechEntry],
) -> String {
    let args_summary = serde_json::to_string(&req.args).unwrap_or_default();
    let mut payload = String::with_capacity(1024);
    payload.push_str("【操作详情】\n");
    payload.push_str(&format!("工具：{}\n", req.tool_name));
    payload.push_str(&format!("命令描述：{}\n", req.command));
    payload.push_str(&format!("风险说明：{}\n", req.risk));
    payload.push_str(&format!("参数：{}\n", args_summary));
    payload.push_str("\n【条件清单】\n");
    for (id, text) in nl_conditions {
        payload.push_str(&format!("#{id}. {text}\n"));
    }
    if !passed_mech.is_empty() {
        payload.push_str("\n【已由系统机械核验通过的条目】（无需你判定）\n");
        for e in passed_mech {
            payload.push_str(&format!("#{}[{}] {}\n", e.id, e.kind, e.evidence));
        }
    }
    payload
}

/// 判定摘要（放行 reason 用——逐条 id/pass）
fn summary_of(j: &PolicyJudgement) -> String {
    j.conditions
        .iter()
        .map(|c| {
            format!(
                "#{}={}",
                c.id,
                match c.pass {
                    Some(true) => "符合",
                    Some(false) => "不符合",
                    None => "弃权",
                }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// 未符合条目清单（拒绝 reason 用=修正指令主体）
fn failures_of(j: &PolicyJudgement) -> String {
    j.conditions
        .iter()
        .filter(|c| c.pass == Some(false))
        .map(|c| format!("#{}（{}）", c.id, c.evidence))
        .collect::<Vec<_>>()
        .join("；")
}

// ===== 条件类型扩展（字符串|表联合形态：机械层/验证层/LLM 兜底层三属） =====

/// 外部验证器超时缺省值（秒，对表 verify.rs 同族缺省）
pub const VERIFY_DEFAULT_TIMEOUT_SECS: u64 = 120;
/// 外部验证器超时上限（秒，超限装载期拒绝）
pub const VERIFY_MAX_TIMEOUT_SECS: u64 = 600;
/// 验证器输出尾段证据帽（字符；修正指令只携带尾段，防巨型构建日志刷屏）
const VERIFY_EVIDENCE_TAIL_CHARS: usize = 1000;

/// 条件条目（字符串 | 表联合形态，向后兼容）
///
/// - 字符串形态：自然语言条件 → LLM 评审兜底层（行为与条件类型扩展前逐位一致）
/// - 表形态：可机械核验的结构化条件，三选一：
///   `match`（白名单 glob，零 LLM 快路径）/ `exclude`（词法排除）/
///   `verify`（serve 侧执行外部验证器，exit 0=符合）
///
/// 全 fail-closed：装载期校验（未知键/混用/元字符/超上限一律拒载），
/// 判定期任何不可信环节降级人工，绝不静默放行。
#[derive(Debug, Clone, PartialEq)]
pub enum PolicyCondition {
    /// 自然语言条件（LLM 评审兜底层）
    Natural(String),
    /// 白名单模式（机械层：操作详情叶子串须命中至少一个 glob，零命中=不符）
    Match {
        /// 人类可读描述
        text: String,
        /// 白名单 glob 模式（归一化小写 + `*` 通配）
        patterns: Vec<String>,
    },
    /// 词法排除（机械层：任一叶子串含排除词=不符，零命中=符合）
    Exclude {
        /// 人类可读描述
        text: String,
        /// 排除词（子串包含判定，大小写归一）
        terms: Vec<String>,
    },
    /// 路径前缀白名单（机械层：args 中路径语义键识别出的路径值须全部落在
    /// 白名单前缀内；任一越界=不符，零识别路径=不符——fail-closed 防参数
    /// 键名漂移导致条件静默失效）
    PathPrefix {
        /// 人类可读描述
        text: String,
        /// 白名单路径前缀（分隔符/大小写归一后前缀比对）
        prefixes: Vec<String>,
    },
    /// 外部可执行验证锚点（验证层：serve 侧执行验证命令，不经 shell）
    Verify {
        /// 人类可读描述
        text: String,
        /// 验证命令规格（argv+超时）
        spec: VerifySpec,
    },
}

impl PolicyCondition {
    /// 条件类别标识（留痕/试判定用）
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Natural(_) => "natural",
            Self::Match { .. } => "match",
            Self::Exclude { .. } => "exclude",
            Self::PathPrefix { .. } => "path_prefix",
            Self::Verify { .. } => "verify",
        }
    }

    /// 人类可读描述（评审 payload 与留痕用）
    pub fn text(&self) -> &str {
        match self {
            Self::Natural(s) => s,
            Self::Match { text, .. }
            | Self::Exclude { text, .. }
            | Self::PathPrefix { text, .. }
            | Self::Verify { text, .. } => text,
        }
    }

    /// 机械实判（match/exclude/path_prefix 条目专用；其余形态返回 None——
    /// verify 条目由验证层执行、NL 条目交 LLM 兜底）
    pub fn evaluate_mechanical(&self, args: &serde_json::Value) -> Option<(bool, String)> {
        match self {
            Self::Match { patterns, .. } => Some(eval_match(patterns, args)),
            Self::Exclude { terms, .. } => Some(eval_exclude(terms, args)),
            Self::PathPrefix { prefixes, .. } => Some(eval_path_prefix(prefixes, args)),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for PolicyCondition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = toml::Value::deserialize(deserializer)?;
        PolicyCondition::from_raw(raw).map_err(serde::de::Error::custom)
    }
}

impl PolicyCondition {
    /// 装载期形态裁决（fail-fast：未知键/空声明/混用一律拒绝，不猜）
    fn from_raw(raw: toml::Value) -> Result<Self, String> {
        match raw {
            toml::Value::String(s) => Ok(PolicyCondition::Natural(s)),
            toml::Value::Table(t) => {
                for k in t.keys() {
                    if !matches!(
                        k.as_str(),
                        "text" | "match" | "exclude" | "verify" | "path_prefix"
                    ) {
                        return Err(format!(
                            "unknown condition field '{k}' (accepted: text / match / exclude / verify / path_prefix)"
                        ));
                    }
                }
                let sc: StructuredCondition =
                    StructuredCondition::deserialize(toml::Value::Table(t))
                        .map_err(|e| format!("condition: {e}"))?;
                let has_verify = sc.verify.is_some();
                let declared_kinds = [
                    !sc.match_patterns.is_empty(),
                    !sc.exclude.is_empty(),
                    !sc.path_prefix.is_empty(),
                    has_verify,
                ]
                .iter()
                .filter(|b| **b)
                .count();
                if declared_kinds > 1 {
                    return Err(
                        "match / exclude / path_prefix / verify cannot be combined in the same condition"
                            .to_string(),
                    );
                }
                if has_verify {
                    let spec = sc.verify.expect("checked above");
                    return Ok(PolicyCondition::Verify {
                        text: sc.text,
                        spec,
                    });
                }
                if !sc.match_patterns.is_empty() {
                    return Ok(PolicyCondition::Match {
                        text: sc.text,
                        patterns: sc.match_patterns,
                    });
                }
                if !sc.exclude.is_empty() {
                    return Ok(PolicyCondition::Exclude {
                        text: sc.text,
                        terms: sc.exclude,
                    });
                }
                if !sc.path_prefix.is_empty() {
                    return Ok(PolicyCondition::PathPrefix {
                        text: sc.text,
                        prefixes: sc.path_prefix,
                    });
                }
                Err(
                    "condition table must declare one of match / exclude / path_prefix / verify"
                        .to_string(),
                )
            }
            _ => Err("condition must be a string or a table".to_string()),
        }
    }
}

/// 结构化条件中间形态（装载期解析目标，裁决后归入 [`PolicyCondition`] 变体）
#[derive(Debug, Clone, Deserialize)]
struct StructuredCondition {
    /// 人类可读描述（必填）
    text: String,
    /// 白名单 glob 模式（TOML 键 `match` 为 Rust 关键字，rename）
    #[serde(default, rename = "match")]
    match_patterns: Vec<String>,
    /// 词法排除词
    #[serde(default)]
    exclude: Vec<String>,
    /// 路径前缀白名单（TOML 键 `path_prefix`）
    #[serde(default)]
    path_prefix: Vec<String>,
    /// 外部验证器
    #[serde(default)]
    verify: Option<VerifySpec>,
}

/// 外部验证器规格（serve 侧执行；不经 shell——字符串形态装载期切分为 argv
/// 并做元字符校验，杜绝注入）
#[derive(Debug, Clone, PartialEq)]
pub struct VerifySpec {
    /// 验证命令 argv（精确形态）
    pub argv: Vec<String>,
    /// 超时秒数（缺省 [`VERIFY_DEFAULT_TIMEOUT_SECS`]，上限 [`VERIFY_MAX_TIMEOUT_SECS`]）
    pub timeout_secs: u64,
}

impl<'de> Deserialize<'de> for VerifySpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            cmd: RawCmd,
            #[serde(default = "default_verify_timeout")]
            timeout_secs: u64,
        }
        fn default_verify_timeout() -> u64 {
            VERIFY_DEFAULT_TIMEOUT_SECS
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.timeout_secs > VERIFY_MAX_TIMEOUT_SECS {
            return Err(serde::de::Error::custom(format!(
                "verify.timeout_secs {} exceeds cap {VERIFY_MAX_TIMEOUT_SECS}",
                raw.timeout_secs
            )));
        }
        let argv = raw.cmd.into_argv().map_err(serde::de::Error::custom)?;
        Ok(VerifySpec {
            argv,
            timeout_secs: raw.timeout_secs,
        })
    }
}

/// verify.cmd 原始形态（数组=精确 argv；字符串=按空白切分）
#[derive(Deserialize)]
#[serde(untagged)]
enum RawCmd {
    Argv(Vec<String>),
    Line(String),
}

impl RawCmd {
    fn into_argv(self) -> Result<Vec<String>, String> {
        match self {
            Self::Argv(items) => {
                if items.is_empty() {
                    return Err("verify.cmd array must not be empty".to_string());
                }
                if items.iter().any(|s| s.trim().is_empty()) {
                    return Err("verify.cmd contains an empty argument".to_string());
                }
                Ok(items)
            }
            Self::Line(s) => {
                let tokens: Vec<String> = s.split_whitespace().map(|t| t.to_string()).collect();
                if tokens.is_empty() {
                    return Err("verify.cmd is empty".to_string());
                }
                // 不经 shell 执行：串形态含 shell 元字符=拒载（fail-fast 杜绝注入）
                if tokens.iter().any(|t| t.contains(['|', ';', '&', '>', '<'])) {
                    return Err(
                        "verify.cmd contains shell metacharacters ('|' ';' '&' '>' '<'); \
                         commands run without a shell — pass an argv array instead"
                            .to_string(),
                    );
                }
                Ok(tokens)
            }
        }
    }
}

/// 验证器单条执行产出（Ok=进程完成，exit code 可判；Err=spawn/等待失败）
#[derive(Debug, Clone)]
pub struct VerifyOutput {
    /// 进程退出码（Ok 路径恒有值）
    pub exit_code: i32,
    /// 输出尾段（stdout+stderr 合并，截取尾部作证据）
    pub output_tail: String,
}

/// 验证器执行通道抽象（生产=tokio 进程 spawn；测试=打桩）
// async_trait 展开体的 must_use 语义重叠压制（同 PolicyReviewInvoker 先例）
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait VerifyExecutor: Send + Sync {
    /// 执行一条验证命令（argv 已装载期校验；工作目录由实现自带）
    async fn run(&self, argv: &[String]) -> Result<VerifyOutput, String>;
}

/// 生产 [`VerifyExecutor`]：不经 shell 直接 spawn argv
/// （工作目录=会话工作区根；超时由决策端包裹——超时 drop 本 future，
/// kill_on_drop 负责终止进程）
pub struct ProcessVerifyExecutor {
    workdir: std::path::PathBuf,
}

impl ProcessVerifyExecutor {
    /// 构造（workdir = 验证命令的执行工作目录）
    pub fn new(workdir: &Path) -> Self {
        Self {
            workdir: workdir.to_path_buf(),
        }
    }
}

#[async_trait]
impl VerifyExecutor for ProcessVerifyExecutor {
    async fn run(&self, argv: &[String]) -> Result<VerifyOutput, String> {
        let Some(program) = argv.first() else {
            return Err("verify argv is empty".to_string());
        };
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(&argv[1..])
            .current_dir(&self.workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let child = cmd
            .spawn()
            .map_err(|e| format!("verify command spawn failed: {e}"))?;
        let out = child
            .wait_with_output()
            .await
            .map_err(|e| format!("verify command wait failed: {e}"))?;
        let exit_code = out
            .status
            .code()
            .ok_or_else(|| "verify command terminated by signal".to_string())?;
        let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
        let tail: String = combined
            .chars()
            .rev()
            .take(VERIFY_EVIDENCE_TAIL_CHARS)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        Ok(VerifyOutput {
            exit_code,
            output_tail: tail.trim().to_string(),
        })
    }
}

/// 未绑定工作区的缺省执行通道——恒 Err（fail-closed 兜底；正常路径
/// workdir=None 时验证层在此之前已降级人工，本臂不可达）
struct UnboundVerifyExecutor;

#[async_trait]
impl VerifyExecutor for UnboundVerifyExecutor {
    async fn run(&self, _argv: &[String]) -> Result<VerifyOutput, String> {
        Err("verify executor has no workdir bound".to_string())
    }
}

/// 白名单模式机械求值：操作详情叶子串（复用红线同款递归原语）须命中
/// 至少一个 glob（归一化小写 + `*` 通配，词元边界同 approval_tier 族）；
/// 零命中=不符（fail-closed）
fn eval_match(patterns: &[String], args: &serde_json::Value) -> (bool, String) {
    let mut leaves = Vec::new();
    collect_strings(args, &mut leaves);
    for p in patterns {
        for leaf in &leaves {
            if crate::agent::approval_tier::pattern_matches(p, leaf) {
                return (true, format!("命中白名单模式 \"{p}\"（操作串 \"{leaf}\"）"));
            }
        }
    }
    (
        false,
        format!(
            "操作详情叶子串均未命中白名单模式（patterns: {}）",
            patterns.join(", ")
        ),
    )
}

/// 词法排除机械求值：任一叶子串含排除词=不符；零命中=符合
fn eval_exclude(terms: &[String], args: &serde_json::Value) -> (bool, String) {
    let mut leaves = Vec::new();
    collect_strings(args, &mut leaves);
    for leaf in &leaves {
        let norm = crate::agent::approval_tier::normalize(leaf);
        for t in terms {
            if norm.contains(&crate::agent::approval_tier::normalize(t)) {
                return (false, format!("命中排除词 \"{t}\"（操作串 \"{leaf}\"）"));
            }
        }
    }
    (true, "无排除词命中".to_string())
}

/// 路径语义键显式清单（`path_prefix` 机械求值的识别面；键名小写归一比对。
/// 清单外键名不识别——宁可判不符交修正，不做键名猜测）
const PATH_SEMANTIC_KEYS: &[&str] = &[
    "path",
    "file_path",
    "file",
    "target",
    "target_path",
    "new_path",
    "old_path",
    "dest",
    "destination",
    "dir",
    "directory",
    "root",
    "workspace",
    "folder",
];

/// 路径归一：分隔符统一为 `/` + 小写（Windows 大小写容忍）；前缀与参数值
/// 两侧同规归一后前缀比对
fn normalize_path(s: &str) -> String {
    s.replace('\\', "/").to_lowercase()
}

/// 收集 args 中路径语义键下的字符串叶子值（递归对象与数组；键名小写归一
/// 后命中 [`PATH_SEMANTIC_KEYS`] 才收集）
fn collect_path_values(value: &serde_json::Value, key_hint: &str, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => {
            let k = key_hint.to_lowercase();
            if PATH_SEMANTIC_KEYS.contains(&k.as_str()) {
                out.push(s.clone());
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_path_values(item, key_hint, out);
            }
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                collect_path_values(v, k, out);
            }
        }
        _ => {}
    }
}

/// 路径前缀白名单机械求值：args 中路径语义键识别出的路径值逐个归一化后
/// 须命中至少一个前缀——任一越界=不符（修正指令携越界值证据）；**零识别
/// 路径=不符**（fail-closed：防参数键名漂移导致条件静默失效，与 match
/// 「零命中=不符」同型）
fn eval_path_prefix(prefixes: &[String], args: &serde_json::Value) -> (bool, String) {
    let mut paths = Vec::new();
    collect_path_values(args, "", &mut paths);
    if paths.is_empty() {
        return (
            false,
            format!(
                "参数中未识别到任何路径语义键（识别面: {}）——条件按不符处理（fail-closed）",
                PATH_SEMANTIC_KEYS.join(", ")
            ),
        );
    }
    let norm_prefixes: Vec<String> = prefixes.iter().map(|p| normalize_path(p)).collect();
    for p in &paths {
        let norm = normalize_path(p);
        if !norm_prefixes
            .iter()
            .any(|pre| norm.starts_with(pre.as_str()))
        {
            return (
                false,
                format!(
                    "路径 \"{p}\" 越出白名单前缀（prefixes: {}）",
                    prefixes.join(", ")
                ),
            );
        }
    }
    (
        true,
        format!(
            "{} 项路径全部命中白名单前缀（{}）",
            paths.len(),
            prefixes.join(", ")
        ),
    )
}

/// 机械/验证层单条求值结果（留痕 seal 的 mechanical 数组要素）
#[derive(Debug, Clone)]
struct MechEntry {
    id: u32,
    kind: &'static str,
    pass: bool,
    evidence: String,
}

impl MechEntry {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "kind": self.kind,
            "pass": self.pass,
            "evidence": self.evidence,
        })
    }
}

/// 机械求值清单摘要（reason/治理事件摘要用）
fn mech_summary(entries: &[MechEntry]) -> String {
    entries
        .iter()
        .map(|e| {
            format!(
                "#{}[{}]={}",
                e.id,
                e.kind,
                if e.pass { "符合" } else { "不符合" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// 未符合机械条目清单（拒绝 reason 用=修正指令主体）
fn mech_failures(entries: &[MechEntry]) -> String {
    entries
        .iter()
        .filter(|e| !e.pass)
        .map(|e| format!("#{}[{}]（{}）", e.id, e.kind, e.evidence))
        .collect::<Vec<_>>()
        .join("；")
}

/// 单工具组的审批条件清单（人类预置）
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ToolGroupPolicy {
    /// 条件条目清单（字符串=自然语言→LLM 兜底；表=match/exclude（机械层）
    /// 或 verify（外部验证器）——装载期 fail-fast 校验）
    #[serde(default)]
    pub conditions: Vec<PolicyCondition>,
}

/// 通用限制（`[policy_limits]`，所有 policy 工具共享）
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyLimits {
    /// 修正重查上限（同一操作 fail 后重查次数，超限降级 manual——防无限循环）
    #[serde(default = "default_max_corrections")]
    pub max_corrections: u32,
    /// 单次任务连续自动操作上限（超限暂停等人——Runaway 护栏）
    #[serde(default = "default_max_actions_per_run")]
    pub max_actions_per_run: u32,
}

fn default_max_corrections() -> u32 {
    3
}

fn default_max_actions_per_run() -> u32 {
    200
}

impl Default for PolicyLimits {
    fn default() -> Self {
        Self {
            max_corrections: default_max_corrections(),
            max_actions_per_run: default_max_actions_per_run(),
        }
    }
}

/// 条件文件整体（顶层动态工具组表 + 可选 policy_limits）
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct ApprovalPolicyFile {
    /// 动态工具组（键=工具名或组名，如 shell_exec）
    #[serde(flatten)]
    pub groups: BTreeMap<String, ToolGroupPolicy>,
    /// 通用限制（缺省 3 次/200 次）
    #[serde(default)]
    pub policy_limits: PolicyLimits,
}

impl ApprovalPolicyFile {
    /// 从给定路径加载条件文件（serve 侧封存入口）
    ///
    /// fail-visible：文件不存在返回 [`PolicyFileError::NotFound`]（调用方视为
    /// 「未预置条件」——policy 档同样降级 manual，因为空条件清单无法评审）；
    /// 解析/schema 失败返回对应错误并必须显式留痕。
    pub fn load(path: &Path) -> Result<Self, PolicyFileError> {
        let raw = std::fs::read_to_string(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => PolicyFileError::NotFound(e),
            _ => PolicyFileError::Io(e),
        })?;
        Self::from_toml(&raw)
    }

    /// 从 TOML 文本解析（校验入口，测试与试判定共用）
    ///
    /// 装载期 fail-fast：形态裁决（未知键/空声明/verify 混用）与
    /// verify 规格（元字符/超上限）在反序列化内联校验——此处集中承接，
    /// 任何拒绝均显式 Schema 错（不猜、不静默丢弃）
    pub fn from_toml(raw: &str) -> Result<Self, PolicyFileError> {
        let parsed: Self =
            toml::from_str(raw).map_err(|e| PolicyFileError::Schema(e.to_string()))?;
        Ok(parsed)
    }

    /// 取指定工具组的条件清单（无该组返回空切片）
    pub fn conditions_for(&self, tool: &str) -> &[PolicyCondition] {
        self.groups
            .get(tool)
            .map(|g| g.conditions.as_slice())
            .unwrap_or(&[])
    }
}

/// 工具→审批组归面单源（每组档位键 `approval.mode.<group>` 的路由依据；
/// 每工具/组三档设计意图的落地面）
///
/// - `shell_exec` → `shellExec`（既有会话缺省键，语义不变）
/// - 文件写族（file_write/file_create/file_move/file_edit/write_file/edit_file）
///   → `fileEdit`
/// - `file_delete` → `fileDelete`（注意：该工具恒命中不可逆删除红线——
///   任何档位仍先过红线闸强制人工，组档位只在豁免显式开启后生效）
/// - 其余工具 → `None`（回落会话缺省档位=shellExec 键，缺省不变式）
pub fn tool_group(tool: &str) -> Option<&'static str> {
    match tool {
        "shell_exec" => Some("shellExec"),
        "file_write" | "file_create" | "file_move" | "file_edit" | "write_file" | "edit_file" => {
            Some("fileEdit")
        }
        "file_delete" => Some("fileDelete"),
        _ => None,
    }
}

/// 条件文件加载错误（fail-visible 语义，禁止调用方静默吞掉）
#[derive(Debug)]
pub enum PolicyFileError {
    /// 文件不存在（未预置条件——policy 档降级 manual）
    NotFound(std::io::Error),
    /// 读失败（权限等 I/O 错误）
    Io(std::io::Error),
    /// TOML 解析或 schema 校验失败
    Schema(String),
}

impl fmt::Display for PolicyFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(e) => write!(f, "approval policy file not found: {e}"),
            Self::Io(e) => write!(f, "approval policy file read failed: {e}"),
            Self::Schema(e) => write!(f, "approval policy file invalid: {e}"),
        }
    }
}

impl std::error::Error for PolicyFileError {}

/// 红线豁免类别（默认强制人工审批的操作面）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedLine {
    /// 不可逆删除（file_delete 含批量——.evo-trash 兜底仍在，但自删+自审批叠加风险不可接受）
    IrreversibleDelete,
    /// 推送远端 / force push（影响超出本地工作区）
    RemotePush,
    /// 数据外发（网络外发命令 / http_get 抓取——数据出境红线）
    DataExfiltration,
    /// 改治理配置文件本身（自我扩权防护——条件文件/设置文件任何档位下必经人工）
    GovernanceConfigWrite,
    /// 宿主任意代码执行（python_exec 未装配 workspace 沙箱即落宿主解释器，
    /// 无白名单可比 shell_exec——静态逃逸检测可被动态构造绕过，故按操作面
    /// 而非代码内容判定；sandbox-trusted 档不出提案，本红线天然不触发）
    HostCodeExecution,
}

impl RedLine {
    /// 审计留痕标记名（决策端落账用）
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::IrreversibleDelete => "red_line.delete",
            Self::RemotePush => "red_line.remote_push",
            Self::DataExfiltration => "red_line.data_exfiltration",
            Self::GovernanceConfigWrite => "red_line.governance_config_write",
            Self::HostCodeExecution => "red_line.host_code_execution",
        }
    }
}

/// 网络外发命令词法名单（v1 机械口径：宁可误报多一次人工审批，不可漏报自动放行）
const EXFIL_COMMANDS: &[&str] = &[
    "curl",
    "wget",
    "nc",
    "ncat",
    "netcat",
    "ssh",
    "scp",
    "sftp",
    "ftp",
    "telnet",
    "rsync",
    "invoke-webrequest",
    "invoke-restmethod",
    "iwr",
    "irm",
];

/// 治理配置文件路径匹配（\.evo\ 审批条件/设置文件——agent 改写即自我扩权）
fn is_governance_config_path(raw: &str) -> bool {
    let norm = raw.replace('\\', "/").to_ascii_lowercase();
    norm.contains(".evo/approval-policy.toml") || norm.contains(".evo/settings.json")
}

/// 从操作参数中收集全部叶子字符串（递归——覆盖批量路径数组等形状）
fn collect_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => {
            for it in items {
                collect_strings(it, out);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_strings(v, out);
            }
        }
        _ => {}
    }
}

/// shell 命令中是否命中外发命令词（按词匹配可执行名，含管道/串联后的段）
fn command_hits_exfil(command: &str) -> bool {
    command
        .split(|c: char| c.is_whitespace() || matches!(c, '|' | ';' | '&' | '(' | ')'))
        .filter(|w| !w.is_empty())
        .any(|w| {
            let name = w
                .trim_matches(|c: char| matches!(c, '"' | '\''))
                .to_ascii_lowercase();
            let base = name.rsplit(['/', '\\']).next().unwrap_or(&name);
            EXFIL_COMMANDS.contains(&base)
        })
}

/// shell 命令中是否命中推送远端模式（git push 含 force push）
fn command_hits_push(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains("git") && lower.split_whitespace().any(|w| w == "push")
}

/// 宿主任意代码执行红线判定原语：按操作面（工具+执行上下文）而非代码内容判定
///
/// python_exec 未装配 workspace 沙箱（提案 `host_execution=true`）即落宿主
/// 解释器任意执行——代码内容的静态逃逸检测（`hits_escape_intent`）只覆盖
/// 已知形态，动态构造（exec/eval/拼接导入）可绕过；红线以操作面兜底。
/// 纯函数、零 IO；`approval.exemptionOverride` 语义由决策端消费方实现。
pub fn host_code_execution_red_line(tool: &str, host_execution: bool) -> Option<RedLine> {
    if tool == "python_exec" && host_execution {
        Some(RedLine::HostCodeExecution)
    } else {
        None
    }
}

/// 决策端入口：红线判定（操作面清单 + 宿主任意代码执行）同源同口径
///
/// [`hits_red_line`] 覆盖参数形态可判定的清单；宿主代码执行由提案携带的
/// 执行上下文（`host_execution`）判定，随 [`crate::agent::approval::ApprovalRequest`]
/// 到达决策端。所有运行时决策端（红线前置闸/policy 评审前置闸）一律走本
/// 入口，禁止各端自行拼装口径。
pub fn hits_red_line_for_request(req: &crate::agent::approval::ApprovalRequest) -> Option<RedLine> {
    hits_red_line(&req.tool_name, &req.args)
        .or_else(|| host_code_execution_red_line(&req.tool_name, req.host_execution))
}

/// 红线豁免判定原语：操作是否命中默认强制人工审批清单
///
/// 纯函数、零 IO；命中即「任何审批档位下强制回落 manual」（deny 优先级更高，
/// 由 manifest blocked 分支先行拒绝，不经本函数）。`approval.exemptionOverride`
/// 的解除语义由决策端消费方实现——本函数恒返回客观命中结果。
pub fn hits_red_line(tool: &str, args: &serde_json::Value) -> Option<RedLine> {
    match tool {
        "file_delete" => return Some(RedLine::IrreversibleDelete),
        "http_get" => return Some(RedLine::DataExfiltration),
        "shell_exec" => {
            let mut strings = Vec::new();
            collect_strings(args, &mut strings);
            if strings.iter().any(|s| command_hits_exfil(s)) {
                return Some(RedLine::DataExfiltration);
            }
            if strings.iter().any(|s| command_hits_push(s)) {
                return Some(RedLine::RemotePush);
            }
        }
        "file_write" | "file_create" | "file_move" | "file_edit" | "write_file" | "edit_file"
        | "workspace_undo" => {
            let mut strings = Vec::new();
            collect_strings(args, &mut strings);
            if strings.iter().any(|s| is_governance_config_path(s)) {
                return Some(RedLine::GovernanceConfigWrite);
            }
        }
        _ => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SAMPLE: &str = r#"
[shell_exec]
conditions = [
  "command 匹配白名单模式 ^cargo|^git status|^git diff",
  "仅工作区内路径"
]
[file_read]
conditions = ["仅工作区内路径"]
[policy_limits]
max_corrections = 3
max_actions_per_run = 200
"#;

    #[test]
    fn parse_full_sample() {
        let f = ApprovalPolicyFile::from_toml(SAMPLE).unwrap();
        assert_eq!(f.groups.len(), 2);
        assert_eq!(f.conditions_for("shell_exec").len(), 2);
        assert_eq!(f.conditions_for("file_read").len(), 1);
        assert_eq!(f.conditions_for("absent"), &[] as &[PolicyCondition]);
        assert_eq!(f.policy_limits.max_corrections, 3);
        assert_eq!(f.policy_limits.max_actions_per_run, 200);
    }

    #[test]
    fn limits_default_when_absent() {
        let f = ApprovalPolicyFile::from_toml("[shell_exec]\nconditions = [\"x\"]").unwrap();
        assert_eq!(f.policy_limits, PolicyLimits::default());
        assert_eq!(f.policy_limits.max_corrections, 3);
        assert_eq!(f.policy_limits.max_actions_per_run, 200);
    }

    #[test]
    fn bad_toml_is_visible_error() {
        let err = ApprovalPolicyFile::from_toml("not [valid toml").unwrap_err();
        assert!(matches!(err, PolicyFileError::Schema(_)));
        assert!(err.to_string().contains("invalid"));
    }

    #[test]
    fn load_missing_file_reports_not_found() {
        let err = ApprovalPolicyFile::load(Path::new("Z:/definitely/absent/x.toml")).unwrap_err();
        assert!(matches!(err, PolicyFileError::NotFound(_)));
    }

    #[test]
    fn red_line_delete_always_hits() {
        assert_eq!(
            hits_red_line("file_delete", &json!({"path": "a.txt"})),
            Some(RedLine::IrreversibleDelete)
        );
    }

    #[test]
    fn host_code_execution_red_line_by_surface() {
        // 宿主任意代码执行:按操作面判定(工具+执行上下文),与代码内容无关
        assert_eq!(
            host_code_execution_red_line("python_exec", true),
            Some(RedLine::HostCodeExecution)
        );
        // workspace 沙箱锚定不触发
        assert_eq!(host_code_execution_red_line("python_exec", false), None);
        // 其他工具不在此面
        assert_eq!(host_code_execution_red_line("shell_exec", true), None);
        assert_eq!(
            RedLine::HostCodeExecution.as_str(),
            "red_line.host_code_execution"
        );
    }

    #[test]
    fn red_line_for_request_combines_surface_and_context() {
        let mut req = review_req("python_exec", "", json!({"code": "print(1)"}));
        assert_eq!(hits_red_line_for_request(&req), None);
        req.host_execution = true;
        assert_eq!(
            hits_red_line_for_request(&req),
            Some(RedLine::HostCodeExecution)
        );
        // 参数形态清单判定同入口可达(同源同口径)
        let del = review_req("file_delete", "", json!({"path": "a.txt"}));
        assert_eq!(
            hits_red_line_for_request(&del),
            Some(RedLine::IrreversibleDelete)
        );
    }

    #[test]
    fn red_line_http_get_is_exfiltration() {
        assert_eq!(
            hits_red_line("http_get", &json!({"url": "https://example.com"})),
            Some(RedLine::DataExfiltration)
        );
    }

    #[test]
    fn red_line_shell_exfil_commands() {
        assert_eq!(
            hits_red_line(
                "shell_exec",
                &json!({"command": "curl https://evil.example"})
            ),
            Some(RedLine::DataExfiltration)
        );
        assert_eq!(
            hits_red_line(
                "shell_exec",
                &json!({"command": "echo hi | wget -q http://x"})
            ),
            Some(RedLine::DataExfiltration)
        );
        // 全路径形态的可执行名同样命中
        assert_eq!(
            hits_red_line("shell_exec", &json!({"command": "/usr/bin/nc -l 8080"})),
            Some(RedLine::DataExfiltration)
        );
    }

    #[test]
    fn red_line_shell_push() {
        assert_eq!(
            hits_red_line("shell_exec", &json!({"command": "git push origin main"})),
            Some(RedLine::RemotePush)
        );
        assert_eq!(
            hits_red_line(
                "shell_exec",
                &json!({"command": "git push --force origin main"})
            ),
            Some(RedLine::RemotePush)
        );
        // 非推送的 git 操作不命中
        assert_eq!(
            hits_red_line("shell_exec", &json!({"command": "git status"})),
            None
        );
        assert_eq!(
            hits_red_line("shell_exec", &json!({"command": "cargo build"})),
            None
        );
    }

    #[test]
    fn red_line_governance_config_write() {
        assert_eq!(
            hits_red_line(
                "file_write",
                &json!({"path": ".evo/approval-policy.toml", "content": "x"})
            ),
            Some(RedLine::GovernanceConfigWrite)
        );
        // Windows 反斜杠路径归一后同样命中
        assert_eq!(
            hits_red_line(
                "file_create",
                &json!({"path": "D:\\work\\.evo\\settings.json"})
            ),
            Some(RedLine::GovernanceConfigWrite)
        );
        // 普通文件写不命中
        assert_eq!(
            hits_red_line("file_write", &json!({"path": "src/main.rs"})),
            None
        );
    }

    /// 回归：内置注册名 edit_file 与 workspace_undo 必须命中治理配置红线
    /// （修复前名单缺 edit_file——`.` 边界下可绕强制人工直改 .evo/ 治理配置）
    #[test]
    fn red_line_edit_and_undo_governance_config() {
        assert_eq!(
            hits_red_line(
                "edit_file",
                &json!({"path": ".evo/settings.json", "old_string": "a", "new_string": "b"})
            ),
            Some(RedLine::GovernanceConfigWrite)
        );
        assert_eq!(
            hits_red_line("workspace_undo", &json!({})),
            None,
            "无参 undo 不携带路径，不命中治理配置红线（恢复路径校验在 CheckpointStore 复验层）"
        );
    }

    #[test]
    fn red_line_unrelated_tools_pass() {
        assert_eq!(hits_red_line("file_read", &json!({"path": "a.rs"})), None);
        assert_eq!(hits_red_line("grep_files", &json!({"pattern": "x"})), None);
    }

    #[test]
    fn red_line_marker_names_stable() {
        assert_eq!(RedLine::IrreversibleDelete.as_str(), "red_line.delete");
        assert_eq!(RedLine::RemotePush.as_str(), "red_line.remote_push");
        assert_eq!(
            RedLine::DataExfiltration.as_str(),
            "red_line.data_exfiltration"
        );
        assert_eq!(
            RedLine::GovernanceConfigWrite.as_str(),
            "red_line.governance_config_write"
        );
    }

    // ===== 评审决策端（PolicyReview） =====

    use crate::agent::approval::{ApprovalCallback, ApprovalRequest};

    /// 可编程评审桩：按预置响应序列返回（Err 视为调用失败），记录调用次数
    struct MockInvoker {
        responses: Mutex<Vec<Result<String, String>>>,
        calls: std::sync::atomic::AtomicUsize,
        payloads: Mutex<Vec<String>>,
    }

    impl MockInvoker {
        fn new(responses: Vec<Result<String, String>>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses),
                calls: std::sync::atomic::AtomicUsize::new(0),
                payloads: Mutex::new(Vec::new()),
            })
        }
        fn count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn recorded_payloads(&self) -> Vec<String> {
            self.payloads
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        }
    }

    #[async_trait]
    impl PolicyReviewInvoker for MockInvoker {
        async fn review(&self, payload: &str) -> Result<String, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(!payload.contains("对话历史"), "评审请求不得携带对话上下文");
            self.payloads
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(payload.to_string());
            self.responses
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pop()
                .unwrap_or_else(|| Err("invoker exhausted".to_string()))
        }
    }

    /// 人工降级桩：记录调用次数，返回固定人工决策
    struct FallbackSpy {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl FallbackSpy {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
        fn count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ApprovalCallback for FallbackSpy {
        async fn request_approval(&self, _req: &ApprovalRequest) -> ApprovalDecision {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ApprovalDecision {
                approved: false,
                approver: "manual-fallback".to_string(),
                verified: false,
                reason: "human window".to_string(),
                auto_rejected: false,
                verdict_json: None,
            }
        }
    }

    fn judgement_json(pass_values: &[Option<bool>], reason: &str) -> String {
        let conditions: Vec<String> = pass_values
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let v = match p {
                    Some(true) => "true".to_string(),
                    Some(false) => "false".to_string(),
                    None => "null".to_string(),
                };
                format!(r#"{{"id":{},"pass":{},"evidence":"e{}"}}"#, i + 1, v, i + 1)
            })
            .collect();
        format!(
            r#"{{"conditions":[{}],"verdict":"approved","reason":"{}"}}"#,
            conditions.join(","),
            reason
        )
    }

    fn review_req(tool: &str, command: &str, args: serde_json::Value) -> ApprovalRequest {
        ApprovalRequest {
            session_id: "s-1".to_string(),
            tool_name: tool.to_string(),
            args,
            command: command.to_string(),
            risk: "medium".to_string(),
            alternative: String::new(),
            proposal_id: "ap-test".to_string(),
            host_execution: false,
        }
    }

    fn policy_with_conditions(max_corrections: u32) -> ApprovalPolicyFile {
        let toml_src = format!(
            "[shell_exec]\nconditions = [\"仅白名单 cargo 命令\", \"仅工作区内路径\"]\n[file_delete]\nconditions = [\"仅限工作区临时目录\"]\n[policy_limits]\nmax_corrections = {}\nmax_actions_per_run = 200",
            max_corrections
        );
        ApprovalPolicyFile::from_toml(&toml_src).unwrap()
    }

    async fn run_review(
        invoker: Arc<MockInvoker>,
        fallback: Arc<FallbackSpy>,
        req: &ApprovalRequest,
    ) -> ApprovalDecision {
        let review = PolicyReview::new(
            policy_with_conditions(3),
            invoker,
            fallback.clone() as Arc<dyn ApprovalCallback>,
            false,
            None,
        );
        review.request_approval(req).await
    }

    #[tokio::test]
    async fn approved_when_all_conditions_pass() {
        let invoker = MockInvoker::new(vec![Ok(judgement_json(&[Some(true), Some(true)], "ok"))]);
        let fallback = FallbackSpy::new();
        let d = run_review(
            invoker,
            fallback.clone(),
            &review_req(
                "shell_exec",
                "cargo build",
                serde_json::json!({"command": "cargo build"}),
            ),
        )
        .await;
        assert!(d.approved);
        assert_eq!(d.approver, "policy-review");
        assert!(d.reason.contains("#1=符合"));
        assert_eq!(fallback.count(), 0);
    }

    #[tokio::test]
    async fn rejected_returns_correction_directive() {
        let invoker = MockInvoker::new(vec![Ok(judgement_json(
            &[Some(true), Some(false)],
            "路径越界",
        ))]);
        let fallback = FallbackSpy::new();
        let d = run_review(
            invoker,
            fallback.clone(),
            &review_req(
                "shell_exec",
                "cargo build",
                serde_json::json!({"command": "cargo build"}),
            ),
        )
        .await;
        assert!(!d.approved);
        assert!(d.reason.contains("修正"));
        assert!(d.reason.contains("#2"));
        assert_eq!(fallback.count(), 0);
    }

    #[tokio::test]
    async fn abstained_when_condition_null() {
        let invoker = MockInvoker::new(vec![Ok(judgement_json(&[Some(true), None], "?"))]);
        let fallback = FallbackSpy::new();
        let d = run_review(
            invoker,
            fallback.clone(),
            &review_req(
                "shell_exec",
                "cargo build",
                serde_json::json!({"command": "cargo build"}),
            ),
        )
        .await;
        assert!(!d.approved);
        assert_eq!(d.approver, "manual-fallback");
        assert_eq!(fallback.count(), 1);
    }

    #[tokio::test]
    async fn self_reported_verdict_not_trusted() {
        // 评审自报 approved 但存在 fail 条目——机械重算必须拒绝
        let raw = judgement_json(&[Some(false)], "自报放行");
        let invoker = MockInvoker::new(vec![Ok(raw)]);
        let fallback = FallbackSpy::new();
        let d = run_review(
            invoker,
            fallback.clone(),
            &review_req(
                "shell_exec",
                "cargo build",
                serde_json::json!({"command": "cargo build"}),
            ),
        )
        .await;
        assert!(!d.approved);
        assert_eq!(d.approver, "policy-review");
        // 判定全文封存:决策携带可解析的全文 JSON(含请求与评审输出)
        let vj = d
            .verdict_json
            .as_deref()
            .expect("review decision must carry full verdict json");
        let v: serde_json::Value = serde_json::from_str(vj).unwrap();
        assert_eq!(v["verdict"], "rejected");
        assert!(v["judgement"].is_object(), "judgement 应为解析后的对象");
        assert!(v["request"].as_str().unwrap().contains("cargo build"));
    }

    #[tokio::test]
    async fn parse_failure_falls_back() {
        let invoker = MockInvoker::new(vec![Ok("评审员表示：```json\n{oops\n```".to_string())]);
        let fallback = FallbackSpy::new();
        let d = run_review(
            invoker,
            fallback.clone(),
            &review_req(
                "shell_exec",
                "cargo build",
                serde_json::json!({"command": "cargo build"}),
            ),
        )
        .await;
        assert!(!d.approved);
        assert_eq!(d.approver, "manual-fallback");
    }

    #[tokio::test]
    async fn invocation_error_falls_back() {
        let invoker = MockInvoker::new(vec![Err("llm unreachable".to_string())]);
        let fallback = FallbackSpy::new();
        let d = run_review(
            invoker,
            fallback.clone(),
            &review_req(
                "shell_exec",
                "cargo build",
                serde_json::json!({"command": "cargo build"}),
            ),
        )
        .await;
        assert!(!d.approved);
        assert_eq!(d.approver, "manual-fallback");
    }

    #[tokio::test]
    async fn empty_conditions_fall_back() {
        let review = PolicyReview::new(
            ApprovalPolicyFile::default(),
            MockInvoker::new(vec![]),
            FallbackSpy::new() as Arc<dyn ApprovalCallback>,
            false,
            None,
        );
        let d = review
            .request_approval(&review_req(
                "shell_exec",
                "cargo build",
                serde_json::json!({"command": "cargo build"}),
            ))
            .await;
        assert!(!d.approved);
        assert_eq!(d.approver, "manual-fallback");
    }

    #[tokio::test]
    async fn correction_limit_degrades_to_manual() {
        let review = PolicyReview::new(
            policy_with_conditions(1),
            MockInvoker::new(vec![
                Ok(judgement_json(&[Some(false)], "仍不符合")),
                Ok(judgement_json(&[Some(false)], "首次不符合")),
            ]),
            FallbackSpy::new() as Arc<dyn ApprovalCallback>,
            false,
            None,
        );
        let req = review_req(
            "shell_exec",
            "cargo build",
            serde_json::json!({"command": "cargo build"}),
        );
        let d1 = review.request_approval(&req).await;
        assert!(!d1.approved);
        assert_eq!(d1.approver, "policy-review");
        // 同指纹重查第二次：used=1 >= max=1 → 降级人工
        let d2 = review.request_approval(&req).await;
        assert!(!d2.approved);
        assert_eq!(d2.approver, "manual-fallback");
    }

    #[tokio::test]
    async fn red_line_forces_manual_unless_overridden() {
        // 默认：file_delete 强制人工（评审桩不应被调用）
        let invoker = MockInvoker::new(vec![]);
        let fallback = FallbackSpy::new();
        let d = run_review(
            invoker,
            fallback.clone(),
            &review_req(
                "file_delete",
                "rm a.txt",
                serde_json::json!({"path": "a.txt"}),
            ),
        )
        .await;
        assert_eq!(d.approver, "manual-fallback");
        assert_eq!(fallback.count(), 1);

        // override=true：红线解除强制，评审链继续，reason 携带标记
        let invoker = MockInvoker::new(vec![Ok(judgement_json(&[Some(true), Some(true)], "ok"))]);
        let review = PolicyReview::new(
            policy_with_conditions(3),
            invoker,
            FallbackSpy::new() as Arc<dyn ApprovalCallback>,
            true,
            None,
        );
        let d = review
            .request_approval(&review_req(
                "file_delete",
                "rm a.txt",
                serde_json::json!({"path": "a.txt"}),
            ))
            .await;
        assert!(d.approved);
        assert!(d.reason.contains("exemption_overridden"));
    }

    #[test]
    fn judgement_parses_with_code_fence() {
        let raw = format!(
            "评审结论如下：\n```json\n{}\n```",
            judgement_json(&[Some(true)], "ok")
        );
        let j = parse_judgement(&raw).unwrap();
        assert_eq!(j.recomputed_verdict(), ReviewVerdict::Approved);
    }

    #[test]
    fn review_payload_contains_conditions_but_no_history() {
        let req = review_req(
            "shell_exec",
            "cargo build",
            serde_json::json!({"command": "cargo build"}),
        );
        let payload = build_review_payload(
            &req,
            &[(1, "仅白名单 cargo 命令".to_string())],
            &[MechEntry {
                id: 2,
                kind: "match",
                pass: true,
                evidence: "命中白名单模式 \"cargo*\"".to_string(),
            }],
        );
        assert!(payload.contains("cargo build"));
        assert!(payload.contains("仅白名单 cargo 命令"));
        assert!(payload.contains("#2[match]"), "已通过机械条目证据随附");
        assert!(!payload.contains("message"));
    }

    // ===== 条件类型扩展（机械层/验证层/零 LLM 快路径） =====

    use std::sync::atomic::AtomicUsize;

    fn policy_from(toml_src: &str) -> ApprovalPolicyFile {
        ApprovalPolicyFile::from_toml(toml_src).unwrap()
    }

    /// verify 打桩执行器：按序返回预设产出（Err=spawn/等待失败），记录调用数
    struct StubExecutor {
        outputs: Mutex<Vec<Result<VerifyOutput, String>>>,
        calls: AtomicUsize,
    }

    impl StubExecutor {
        fn new(outputs: Vec<Result<VerifyOutput, String>>) -> Arc<Self> {
            Arc::new(Self {
                outputs: Mutex::new(outputs),
                calls: AtomicUsize::new(0),
            })
        }
        fn count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl VerifyExecutor for StubExecutor {
        async fn run(&self, _argv: &[String]) -> Result<VerifyOutput, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.outputs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pop()
                .unwrap_or_else(|| Err("stub executor exhausted".to_string()))
        }
    }

    /// 永挂起执行器（超时路径打桩——start_paused 虚拟时钟下超时闸先行收敛）
    struct PendingExecutor;

    #[async_trait]
    impl VerifyExecutor for PendingExecutor {
        async fn run(&self, _argv: &[String]) -> Result<VerifyOutput, String> {
            std::future::pending().await
        }
    }

    fn verify_only_policy() -> ApprovalPolicyFile {
        policy_from(
            "[shell_exec]\nconditions = [{ text = \"工作区构建通过\", verify = { cmd = [\"cargo\", \"check\"], timeout_secs = 1 } }]",
        )
    }

    fn build_review(
        policy: ApprovalPolicyFile,
        invoker: Arc<dyn PolicyReviewInvoker>,
        fallback: Arc<FallbackSpy>,
        executor: Arc<dyn VerifyExecutor>,
    ) -> PolicyReview {
        PolicyReview::new(
            policy,
            invoker,
            fallback as Arc<dyn ApprovalCallback>,
            false,
            Some(std::path::PathBuf::from(".")),
        )
        .with_verify_executor(executor)
    }

    #[test]
    fn schema_accepts_structured_conditions_and_defaults() {
        let f = policy_from(
            r#"[shell_exec]
conditions = [
  "仅工作区内路径",
  { text = "白名单", match = ["cargo*"] },
  { text = "禁外发", exclude = ["curl"] },
  { text = "构建过", verify = { cmd = "cargo check", timeout_secs = 180 } },
]"#,
        );
        let conds = f.conditions_for("shell_exec");
        assert_eq!(conds.len(), 4);
        assert_eq!(conds[0].kind(), "natural");
        assert_eq!(conds[1].kind(), "match");
        assert_eq!(conds[2].kind(), "exclude");
        assert_eq!(conds[3].kind(), "verify");
        // 字符串 cmd 装载期切分为 argv
        match &conds[3] {
            PolicyCondition::Verify { spec, .. } => {
                assert_eq!(spec.argv, vec!["cargo", "check"]);
                assert_eq!(spec.timeout_secs, 180);
            }
            other => panic!("expected verify, got {other:?}"),
        }
        // timeout 缺省 120（对表 verify.rs 同族缺省）
        let f2 = policy_from(
            "[shell_exec]\nconditions = [{ text = \"x\", verify = { cmd = [\"ls\"] } }]",
        );
        match &f2.conditions_for("shell_exec")[0] {
            PolicyCondition::Verify { spec, .. } => {
                assert_eq!(spec.timeout_secs, VERIFY_DEFAULT_TIMEOUT_SECS)
            }
            other => panic!("expected verify, got {other:?}"),
        }
    }

    #[test]
    fn schema_rejects_unknown_condition_field() {
        let err = ApprovalPolicyFile::from_toml(
            "[shell_exec]\nconditions = [{ text = \"x\", whitelist = [\"a\"] }]",
        )
        .unwrap_err();
        assert!(matches!(err, PolicyFileError::Schema(_)));
        assert!(err.to_string().contains("unknown condition field"));
    }

    #[test]
    fn schema_rejects_verify_mixed_with_match() {
        let err = ApprovalPolicyFile::from_toml(
            "[shell_exec]\nconditions = [{ text = \"x\", match = [\"a*\"], verify = { cmd = [\"cargo\", \"check\"] } }]",
        )
        .unwrap_err();
        assert!(err.to_string().contains("cannot be combined"));
    }

    #[test]
    fn schema_rejects_empty_condition_table() {
        let err = ApprovalPolicyFile::from_toml("[shell_exec]\nconditions = [{ text = \"x\" }]")
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("must declare one of match / exclude / path_prefix / verify"));
    }

    #[test]
    fn schema_rejects_shell_metacharacters_in_cmd_string() {
        for bad in [
            "cargo check | head",
            "cargo check; rm -rf",
            "cargo check > out.txt",
            "a & b",
            "cargo check <in",
        ] {
            let src = format!(
                "[shell_exec]\nconditions = [{{ text = \"x\", verify = {{ cmd = \"{bad}\" }} }}]"
            );
            let err = ApprovalPolicyFile::from_toml(&src).unwrap_err();
            assert!(
                err.to_string().contains("metacharacters"),
                "case: {bad}, got: {err}"
            );
        }
    }

    #[test]
    fn schema_rejects_timeout_over_cap() {
        let err = ApprovalPolicyFile::from_toml(
            "[shell_exec]\nconditions = [{ text = \"x\", verify = { cmd = [\"cargo\", \"check\"], timeout_secs = 601 } }]",
        )
        .unwrap_err();
        assert!(err.to_string().contains("exceeds cap"));
    }

    #[test]
    fn schema_rejects_unknown_policy_limits_key() {
        let err = ApprovalPolicyFile::from_toml(
            "[shell_exec]\nconditions = [\"a\"]\n[policy_limits]\nmax_corrections = 3\nbogus = 1",
        )
        .unwrap_err();
        assert!(matches!(err, PolicyFileError::Schema(_)));
    }

    #[test]
    fn match_condition_truth_table() {
        let cond = PolicyCondition::Match {
            text: "白名单".to_string(),
            patterns: vec!["cargo build*".to_string(), "git status".to_string()],
        };
        // 命中（glob 通配 + 归一化）
        let (pass, ev) = cond
            .evaluate_mechanical(&json!({"command": "cargo build --release"}))
            .unwrap();
        assert!(pass);
        assert!(ev.contains("cargo build"));
        // 整串模式无通配=全等（词元边界：statusx 不得命中 status）
        let (pass, _) = cond
            .evaluate_mechanical(&json!({"command": "git statusx"}))
            .unwrap();
        assert!(!pass);
        // 零命中=不符（fail-closed）
        let (pass, _) = cond
            .evaluate_mechanical(&json!({"command": "rm -rf /"}))
            .unwrap();
        assert!(!pass);
        // 叶子串递归收集（数组/嵌套参数形状）
        let (pass, _) = cond
            .evaluate_mechanical(&json!({"commands": ["echo hi", "git status"]}))
            .unwrap();
        assert!(pass);
        // 大小写/空白归一
        let (pass, _) = cond
            .evaluate_mechanical(&json!({"command": "  CARGO   BUILD "}))
            .unwrap();
        assert!(pass);
    }

    #[test]
    fn exclude_condition_truth_table() {
        let cond = PolicyCondition::Exclude {
            text: "禁外发".to_string(),
            terms: vec!["git push".to_string(), "curl".to_string()],
        };
        // 命中排除词=不符
        let (pass, ev) = cond
            .evaluate_mechanical(&json!({"command": "git push origin main"}))
            .unwrap();
        assert!(!pass);
        assert!(ev.contains("git push"));
        let (pass, _) = cond
            .evaluate_mechanical(&json!({"command": "curl http://x"}))
            .unwrap();
        assert!(!pass);
        // 零命中=符合
        let (pass, _) = cond
            .evaluate_mechanical(&json!({"command": "cargo check"}))
            .unwrap();
        assert!(pass);
        // 非字符串叶子忽略
        let (pass, _) = cond.evaluate_mechanical(&json!({"count": 3})).unwrap();
        assert!(pass);
    }

    #[tokio::test]
    async fn verify_exit_zero_passes_zero_llm_fast_path() {
        let invoker = MockInvoker::new(vec![]);
        let fallback = FallbackSpy::new();
        let exec = StubExecutor::new(vec![Ok(VerifyOutput {
            exit_code: 0,
            output_tail: String::new(),
        })]);
        let review = build_review(
            verify_only_policy(),
            invoker.clone() as Arc<dyn PolicyReviewInvoker>,
            fallback.clone(),
            exec.clone() as Arc<dyn VerifyExecutor>,
        );
        let d = review
            .request_approval(&review_req(
                "shell_exec",
                "cargo build",
                json!({"command": "cargo build"}),
            ))
            .await;
        assert!(d.approved);
        assert!(
            d.reason.contains("policy-review[mechanical]"),
            "reason: {}",
            d.reason
        );
        // 零 LLM 快路径：评审 invoker 零调用
        assert_eq!(invoker.count(), 0);
        assert_eq!(fallback.count(), 0);
        assert_eq!(exec.count(), 1);
        // seal mechanical 数组：验证器结论可跨重启回放核验
        let v: serde_json::Value =
            serde_json::from_str(d.verdict_json.as_deref().unwrap()).unwrap();
        assert_eq!(v["mechanical"][0]["kind"], "verify");
        assert_eq!(v["mechanical"][0]["pass"], true);
        assert_eq!(v["verdict"], "approved");
    }

    #[tokio::test]
    async fn verify_nonzero_exit_rejects_with_output_tail() {
        let invoker = MockInvoker::new(vec![]);
        let fallback = FallbackSpy::new();
        let exec = StubExecutor::new(vec![Ok(VerifyOutput {
            exit_code: 101,
            output_tail: "error[E0308]: mismatched types".to_string(),
        })]);
        let review = build_review(
            verify_only_policy(),
            invoker.clone() as Arc<dyn PolicyReviewInvoker>,
            fallback.clone(),
            exec.clone() as Arc<dyn VerifyExecutor>,
        );
        let d = review
            .request_approval(&review_req(
                "shell_exec",
                "cargo build",
                json!({"command": "cargo build"}),
            ))
            .await;
        assert!(!d.approved);
        assert_eq!(d.approver, "policy-review");
        // 修正指令携 exit code+输出尾段
        assert!(
            d.reason.contains("verifier exit 101"),
            "reason: {}",
            d.reason
        );
        assert!(d.reason.contains("mismatched types"));
        assert!(d.reason.contains("修正"));
        // LLM 兜底层被跳过
        assert_eq!(invoker.count(), 0);
        assert_eq!(fallback.count(), 0);
        let v: serde_json::Value =
            serde_json::from_str(d.verdict_json.as_deref().unwrap()).unwrap();
        assert_eq!(v["verdict"], "rejected");
        assert_eq!(v["mechanical"][0]["pass"], false);
    }

    #[tokio::test]
    async fn verify_spawn_failure_falls_back_to_manual() {
        let invoker = MockInvoker::new(vec![]);
        let fallback = FallbackSpy::new();
        let exec = StubExecutor::new(vec![Err("spawn failed".to_string())]);
        let review = build_review(
            verify_only_policy(),
            invoker.clone() as Arc<dyn PolicyReviewInvoker>,
            fallback.clone(),
            exec.clone() as Arc<dyn VerifyExecutor>,
        );
        let d = review
            .request_approval(&review_req(
                "shell_exec",
                "cargo build",
                json!({"command": "cargo build"}),
            ))
            .await;
        // 判定不可信 → 降级人工（绝不静默放行）
        assert_eq!(d.approver, "manual-fallback");
        assert_eq!(fallback.count(), 1);
        assert_eq!(invoker.count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn verify_timeout_falls_back_to_manual() {
        let invoker = MockInvoker::new(vec![]);
        let fallback = FallbackSpy::new();
        let review = build_review(
            verify_only_policy(),
            invoker.clone() as Arc<dyn PolicyReviewInvoker>,
            fallback.clone(),
            Arc::new(PendingExecutor) as Arc<dyn VerifyExecutor>,
        );
        let d = review
            .request_approval(&review_req(
                "shell_exec",
                "cargo build",
                json!({"command": "cargo build"}),
            ))
            .await;
        assert_eq!(d.approver, "manual-fallback");
        assert_eq!(fallback.count(), 1);
    }

    #[tokio::test]
    async fn verify_without_workdir_falls_back_to_manual() {
        let invoker = MockInvoker::new(vec![]);
        let fallback = FallbackSpy::new();
        let review = PolicyReview::new(
            verify_only_policy(),
            invoker.clone() as Arc<dyn PolicyReviewInvoker>,
            fallback.clone() as Arc<dyn ApprovalCallback>,
            false,
            None,
        );
        let d = review
            .request_approval(&review_req(
                "shell_exec",
                "cargo build",
                json!({"command": "cargo build"}),
            ))
            .await;
        assert_eq!(d.approver, "manual-fallback");
        assert_eq!(fallback.count(), 1);
        assert_eq!(invoker.count(), 0);
    }

    #[tokio::test]
    async fn all_mechanical_conditions_pass_zero_llm() {
        let invoker = MockInvoker::new(vec![]);
        let fallback = FallbackSpy::new();
        let review = build_review(
            policy_from(
                "[shell_exec]\nconditions = [{ text = \"白名单\", match = [\"cargo*\"] }, { text = \"禁外发\", exclude = [\"git push\"] }]",
            ),
            invoker.clone() as Arc<dyn PolicyReviewInvoker>,
            fallback.clone(),
            Arc::new(PendingExecutor) as Arc<dyn VerifyExecutor>,
        );
        let d = review
            .request_approval(&review_req(
                "shell_exec",
                "cargo check",
                json!({"command": "cargo check"}),
            ))
            .await;
        assert!(d.approved);
        assert!(d.reason.contains("[mechanical]"));
        assert_eq!(invoker.count(), 0);
        assert_eq!(fallback.count(), 0);
        let v: serde_json::Value =
            serde_json::from_str(d.verdict_json.as_deref().unwrap()).unwrap();
        let kinds: Vec<&str> = v["mechanical"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, vec!["match", "exclude"]);
    }

    #[tokio::test]
    async fn mixed_list_payload_contains_only_nl_conditions() {
        let invoker = MockInvoker::new(vec![Ok(judgement_json(&[Some(true)], "ok"))]);
        let fallback = FallbackSpy::new();
        let review = build_review(
            policy_from(
                "[shell_exec]\nconditions = [{ text = \"白名单限定\", match = [\"cargo*\"] }, \"仅工作区内路径\"]",
            ),
            invoker.clone() as Arc<dyn PolicyReviewInvoker>,
            fallback.clone(),
            Arc::new(PendingExecutor) as Arc<dyn VerifyExecutor>,
        );
        let d = review
            .request_approval(&review_req(
                "shell_exec",
                "cargo check",
                json!({"command": "cargo check"}),
            ))
            .await;
        assert!(d.approved);
        // LLM 兜底层被调用，但 payload 只含 NL 条目+已通过机械条目证据摘要
        assert_eq!(invoker.count(), 1);
        let payloads = invoker.recorded_payloads();
        assert!(
            payloads[0].contains("仅工作区内路径"),
            "NL 条目必须在清单内"
        );
        assert!(payloads[0].contains("已由系统机械核验通过的条目"));
        assert!(payloads[0].contains("#1[match]"), "机械条目以证据摘要出现");
        assert!(
            !payloads[0].contains("白名单限定"),
            "机械条件文本不进条件清单"
        );
        // 放行 reason 携两段摘要
        assert!(d.reason.contains("#1=符合"));
    }

    #[tokio::test]
    async fn mechanical_fail_skips_verify_and_llm() {
        let invoker = MockInvoker::new(vec![]);
        let fallback = FallbackSpy::new();
        let exec = StubExecutor::new(vec![Ok(VerifyOutput {
            exit_code: 0,
            output_tail: String::new(),
        })]);
        let review = build_review(
            policy_from(
                "[shell_exec]\nconditions = [{ text = \"白名单限定\", match = [\"cargo*\"] }, { text = \"构建过\", verify = { cmd = [\"cargo\", \"check\"] } }]",
            ),
            invoker.clone() as Arc<dyn PolicyReviewInvoker>,
            fallback.clone(),
            exec.clone() as Arc<dyn VerifyExecutor>,
        );
        let d = review
            .request_approval(&review_req(
                "shell_exec",
                "git status",
                json!({"command": "git status"}),
            ))
            .await;
        assert!(!d.approved);
        assert!(d.reason.contains("#1[match]"), "reason: {}", d.reason);
        // 机械不符短路：验证层与 LLM 兜底层都不发生
        assert_eq!(exec.count(), 0);
        assert_eq!(invoker.count(), 0);
    }

    #[tokio::test]
    async fn red_line_beats_whitelist_match() {
        // 白名单 match 本应命中 git push——但红线闸先于一切机械判定，
        // 白名单不可放宽红线：仍强制人工
        let invoker = MockInvoker::new(vec![]);
        let fallback = FallbackSpy::new();
        let review = build_review(
            policy_from("[shell_exec]\nconditions = [{ text = \"git 操作\", match = [\"git*\"] }]"),
            invoker.clone() as Arc<dyn PolicyReviewInvoker>,
            fallback.clone(),
            Arc::new(PendingExecutor) as Arc<dyn VerifyExecutor>,
        );
        let d = review
            .request_approval(&review_req(
                "shell_exec",
                "git push origin main",
                json!({"command": "git push origin main"}),
            ))
            .await;
        assert_eq!(d.approver, "manual-fallback");
        assert_eq!(fallback.count(), 1);
        assert_eq!(invoker.count(), 0);
    }

    #[test]
    fn review_system_prompt_two_sources_no_history_precedent() {
        // 死条目修正：载荷从不提供会话内批准先例，判定依据收缩为两源
        assert!(!REVIEW_SYSTEM_PROMPT.contains("先例"));
        assert!(REVIEW_SYSTEM_PROMPT.contains("两源"));
        assert!(REVIEW_SYSTEM_PROMPT.contains("机械可核验条件"));
    }

    // ==== path_prefix 条件类型（机械层新属）====

    /// 表形态装载：path_prefix 解析为 PathPrefix 变体，kind/text 留痕面正确
    #[test]
    fn schema_parses_path_prefix_condition() {
        let file = ApprovalPolicyFile::from_toml(
            "[file_edit]\nconditions = [{ text = \"仅工作区内\", path_prefix = [\"D:/work\"] }]",
        )
        .expect("valid policy");
        let conds = file.conditions_for("file_edit");
        assert_eq!(conds.len(), 1);
        match &conds[0] {
            PolicyCondition::PathPrefix { text, prefixes } => {
                assert_eq!(text, "仅工作区内");
                assert_eq!(prefixes, &["D:/work".to_string()]);
            }
            other => panic!("expected PathPrefix, got: {other:?}"),
        }
        assert_eq!(conds[0].kind(), "path_prefix");
        assert_eq!(conds[0].text(), "仅工作区内");
    }

    /// 装载期拒载面：机械键混用（path_prefix 与 match/exclude/verify 同条）拒绝
    #[test]
    fn schema_rejects_path_prefix_mixed_with_other_kinds() {
        let err = ApprovalPolicyFile::from_toml(
            "[shell_exec]\nconditions = [{ text = \"x\", match = [\"a*\"], path_prefix = [\"D:/w\"] }]",
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("cannot be combined in the same condition"));
        let err = ApprovalPolicyFile::from_toml(
            "[shell_exec]\nconditions = [{ text = \"x\", exclude = [\"b\"], path_prefix = [\"D:/w\"] }]",
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("cannot be combined in the same condition"));
        let err = ApprovalPolicyFile::from_toml(
            "[shell_exec]\nconditions = [{ text = \"x\", verify = { cmd = \"cargo check\" }, path_prefix = [\"D:/w\"] }]",
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("cannot be combined in the same condition"));
    }

    /// 空条件表错误文案更新回归：四机械键全缺的新文案
    #[test]
    fn schema_empty_condition_message_lists_path_prefix() {
        let err = ApprovalPolicyFile::from_toml("[shell_exec]\nconditions = [{ text = \"x\" }]")
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("must declare one of match / exclude / path_prefix / verify"));
    }

    /// path_prefix 机械求值真值表：
    /// 命中/越界/零识别路径/归一化（分隔符+大小写）/路径语义键识别面/嵌套数组
    #[test]
    fn path_prefix_truth_table() {
        let cond = PolicyCondition::PathPrefix {
            text: "仅工作区内".to_string(),
            prefixes: vec!["D:/work".to_string(), "./out".to_string()],
        };
        // 命中（path 语义键 + 分隔符/大小写归一：反斜杠、大写盘符）
        let (pass, ev) = cond
            .evaluate_mechanical(&json!({"path": "d:\\WORK\\src\\main.rs"}))
            .unwrap();
        assert!(pass, "normalized backslash+case should hit: {ev}");
        // 越界=不符（evidence 携越界路径）
        let (pass, ev) = cond
            .evaluate_mechanical(&json!({"path": "C:/elsewhere/x.rs"}))
            .unwrap();
        assert!(!pass);
        assert!(ev.contains("C:/elsewhere/x.rs"));
        // 零识别路径=不符（fail-closed：键名不在语义键清单）
        let (pass, ev) = cond
            .evaluate_mechanical(&json!({"command": "echo hi", "blob": "D:/work/a.rs"}))
            .unwrap();
        assert!(!pass, "non-path keys must not satisfy path_prefix");
        assert!(ev.contains("未识别到任何路径语义键"));
        // file_path 语义键识别
        let (pass, _) = cond
            .evaluate_mechanical(&json!({"file_path": "D:/work/a.rs"}))
            .unwrap();
        assert!(pass);
        // 嵌套对象/数组形态下的路径值收集（语义键下数组）
        let (pass, _) = cond
            .evaluate_mechanical(&json!({"path": ["D:/work/a.rs", "D:/work/b.rs"]}))
            .unwrap();
        assert!(pass);
        // 数组中一项越界=整体不符
        let (pass, _) = cond
            .evaluate_mechanical(&json!({"path": ["D:/work/a.rs", "D:/nope/b.rs"]}))
            .unwrap();
        assert!(!pass);
        // 空前缀清单装载即拒（全缺报错面）
        let err = ApprovalPolicyFile::from_toml(
            "[shell_exec]\nconditions = [{ text = \"x\", path_prefix = [] }]",
        )
        .unwrap_err();
        assert!(err.to_string().contains("must declare one of"));
    }

    /// 工具组归面单源：三组键映射+未列名工具回落 None（会话缺省）
    #[test]
    fn tool_group_classification() {
        assert_eq!(tool_group("shell_exec"), Some("shellExec"));
        assert_eq!(tool_group("file_write"), Some("fileEdit"));
        assert_eq!(tool_group("file_create"), Some("fileEdit"));
        assert_eq!(tool_group("file_move"), Some("fileEdit"));
        assert_eq!(tool_group("file_edit"), Some("fileEdit"));
        assert_eq!(tool_group("write_file"), Some("fileEdit"));
        assert_eq!(tool_group("edit_file"), Some("fileEdit"));
        assert_eq!(tool_group("file_delete"), Some("fileDelete"));
        assert_eq!(tool_group("file_read"), None);
        assert_eq!(tool_group("grep_files"), None);
        assert_eq!(tool_group("workspace_undo"), None);
    }
}
