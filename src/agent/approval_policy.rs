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

判定依据分三源，优先级从高到低：
1. 拒绝规则：操作详情命中拒绝规则（含红线类操作）即拒绝，任何其他来源不可覆盖；
2. 会话内批准先例：与本次任务中人类已批准的同类先例一致的操作可参照放行（若提供）；
3. 预置条件：对条件清单逐条判定。

每条条件输出 pass（true=符合 / false=不符合 / null=无法判定）与 evidence（判定依据摘要）；存在无法判定的条件时 verdict 置为 abstained。输出且仅输出一个 JSON 对象，格式：
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
/// 新 purpose 接入需同步核对 server 侧 journal LlmCalled.purpose 映射）
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
        // 引用的 instruction.params.messages 契约一致
        let params = serde_json::json!({
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
/// （空清单 fail-visible 降级人工）→ 修正重查上限检查 → 独立评审调用
/// （30s 超时）→ 判定 JSON 解析+三态机械重算 → 分派。任何判定不可信
/// 环节（超时/调用失败/解析失败/条件弃权）一律降级人工，绝不静默放行。
pub struct PolicyReview {
    policy: ApprovalPolicyFile,
    exemption_override: bool,
    invoker: Arc<dyn PolicyReviewInvoker>,
    fallback: Arc<dyn ApprovalCallback>,
    corrections: Mutex<HashMap<String, u32>>,
}

impl PolicyReview {
    /// 构造（policy=条件文件解析结果；invoker=评审通道；fallback=降级人工端；
    /// exemption_override=用户显式关闭红线强制——留痕必带 exemption_overridden 标记）
    pub fn new(
        policy: ApprovalPolicyFile,
        invoker: Arc<dyn PolicyReviewInvoker>,
        fallback: Arc<dyn ApprovalCallback>,
        exemption_override: bool,
    ) -> Self {
        Self {
            policy,
            exemption_override,
            invoker,
            fallback,
            corrections: Mutex::new(HashMap::new()),
        }
    }

    /// 修正重查指纹键（同会话+同工具+同命令的重查共享计数）
    fn correction_key(req: &ApprovalRequest) -> String {
        format!("{}|{}|{}", req.session_id, req.tool_name, req.command)
    }

    fn corrections_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, u32>> {
        self.corrections.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[async_trait]
impl ApprovalCallback for PolicyReview {
    async fn request_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        // 1. 红线豁免：默认任何档位强制人工；用户显式关闭（override=true）
        //    仅解除强制，评审留痕必须携带 exemption_overridden 标记
        let red_line = hits_red_line(&req.tool_name, &req.args);
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

        // 4. 独立评审调用（30s 超时；失败/超时=判定不可信→降级人工）
        let payload = build_review_payload(req, conditions);
        let raw = match tokio::time::timeout(
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
        };

        // 5. 判定解析+三态机械重算（解析失败=判定不可信→降级人工）
        let judgement = match parse_judgement(&raw) {
            Ok(j) => j,
            Err(e) => {
                warn!(tool = %req.tool_name, error = %e, "policy review judgement unparseable, fallback to manual");
                return self.fallback.request_approval(req).await;
            }
        };
        let verdict = judgement.recomputed_verdict();

        // 6. 留痕：判定请求与全文进 serve 侧结构化日志（封存面）；决策摘要随
        //    ApprovalDecision.reason 进 tool_trace approval 子对象入审计链
        info!(
            tool = %req.tool_name,
            proposal_id = %req.proposal_id,
            verdict = verdict.as_str(),
            override_flag = self.exemption_override,
            request = %payload,
            judgement = %raw,
            "policy review judged"
        );

        // 7. 分派
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
                ApprovalDecision {
                    approved: true,
                    approver: POLICY_REVIEW_APPROVER.to_string(),
                    verified: false,
                    reason: format!(
                        "policy-review: {} 条件全部符合{}",
                        summary_of(&judgement),
                        marker
                    ),
                    auto_rejected: false,
                }
            }
            ReviewVerdict::Rejected => {
                *self.corrections_lock().entry(key).or_insert(0) += 1;
                ApprovalDecision {
                    approved: false,
                    approver: POLICY_REVIEW_APPROVER.to_string(),
                    verified: false,
                    reason: format!(
                        "policy-review: 条件未全部符合，请修正操作后重试。未符合条目：{}；总体理由：{}{}",
                        failures_of(&judgement),
                        judgement.reason,
                        marker
                    ),
                    auto_rejected: false,
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

/// 组装评审请求（操作详情+条件清单——不含 agent 对话上下文与执行意图）
fn build_review_payload(req: &ApprovalRequest, conditions: &[String]) -> String {
    let args_summary = serde_json::to_string(&req.args).unwrap_or_default();
    let mut payload = String::with_capacity(1024);
    payload.push_str("【操作详情】\n");
    payload.push_str(&format!("工具：{}\n", req.tool_name));
    payload.push_str(&format!("命令描述：{}\n", req.command));
    payload.push_str(&format!("风险说明：{}\n", req.risk));
    payload.push_str(&format!("参数：{}\n", args_summary));
    payload.push_str("\n【条件清单】\n");
    for (i, c) in conditions.iter().enumerate() {
        payload.push_str(&format!("{}. {}\n", i + 1, c));
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

/// 单工具组的审批条件清单（人类预置）
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ToolGroupPolicy {
    /// 条件字符串清单（v1 语法：可机械核验的表达式；自然语言条件判定稳定性弱，不放开）
    #[serde(default)]
    pub conditions: Vec<String>,
}

/// 通用限制（`[policy_limits]`，所有 policy 工具共享）
#[derive(Debug, Clone, PartialEq, Deserialize)]
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
    pub fn from_toml(raw: &str) -> Result<Self, PolicyFileError> {
        let parsed: Self =
            toml::from_str(raw).map_err(|e| PolicyFileError::Schema(e.to_string()))?;
        Ok(parsed)
    }

    /// 取指定工具组的条件清单（无该组返回空切片）
    pub fn conditions_for(&self, tool: &str) -> &[String] {
        self.groups
            .get(tool)
            .map(|g| g.conditions.as_slice())
            .unwrap_or(&[])
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
}

impl RedLine {
    /// 审计留痕标记名（决策端落账用）
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::IrreversibleDelete => "red_line.delete",
            Self::RemotePush => "red_line.remote_push",
            Self::DataExfiltration => "red_line.data_exfiltration",
            Self::GovernanceConfigWrite => "red_line.governance_config_write",
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
        "file_write" | "file_create" | "file_move" | "file_edit" | "write_file" => {
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
        assert_eq!(f.conditions_for("absent"), &[] as &[String]);
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
    }

    impl MockInvoker {
        fn new(responses: Vec<Result<String, String>>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses),
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl PolicyReviewInvoker for MockInvoker {
        async fn review(&self, payload: &str) -> Result<String, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(!payload.contains("对话历史"), "评审请求不得携带对话上下文");
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
        let payload = build_review_payload(&req, &["仅白名单 cargo 命令".to_string()]);
        assert!(payload.contains("cargo build"));
        assert!(payload.contains("仅白名单 cargo 命令"));
        assert!(!payload.contains("message"));
    }
}
