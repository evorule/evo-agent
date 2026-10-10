// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! B21 PR-4:journal(× 审计链 × transcript)→ ATIF v1.8 轨迹导出
//!
//! 实现依据:`知识库 B21/ATIF映射表终稿-20260930.md`(RFC 0001 全文逐字段核对)。
//! 三源 join 模型(ATIF 映射表 §一):
//! - **journal**(骨架):步级事件序(seq 单调)/事件类型/token 口径/join 键;
//! - **审计链**(完整内容):IoRequest/IoResponse 事实全文,来源 = evorule
//!   `GET /api/sessions/{id}/replay`(`EvoruleApiClient::replay`,按版本序返回
//!   fact.to_json() 列表)。注意 audit/export 端点只含哈希链元数据(content_hash/
//!   prev_hash),**无事实内容**,不可作内容源(2026-09-30 读码实证);
//! - **transcript**(消息投影):`load_transcript` 投影的 MessageRecord 列表,
//!   system/user 步全文源 + 本地 ReAct 路径(arguments/observation)回退源。
//!
//! 本模块是**纯函数导出器**:输入三源快照,输出 ATIF 轨迹对象;IO(读 journal/
//! 拉 transcript/facts)由调用方(G2 壳层)完成,便于单测与幂等复算。
//!
//! 确定性裁定(ATIF 映射表 §七):
//! - `tool_call_id = "t{seq}"`(journal 已合成,原样引用);
//! - token 口径:provider 真值(`tokens`)优先,fallback `tokens_est` 按 7:3
//!   拆分(prompt:completion),两种来源混合由 root.notes 固定声明;
//! - 排序一律以 journal seq 为序,时间戳仅展示;
//! - 数值字段不虚构:无据可依一律缺省(ATIF 全 Optional 设计),禁填 0/null 冒充;
//! - **幂等**:同 session 重导出逐字节一致(P3 验收 #7)——`extra.exported_at`
//!   由 journal 尾事件 ts 派生而非取当前时钟(映射表 §三 exported_at 与 §十 #7
//!   幂等的冲突点,取幂等优先;语义 = 会话末次活动时刻,注释留痕)。
//!
//! 已知边界(v1 留痕,02-实施日志同口径;现状随接线更新):
//! - delegate 子代理 journal 已接线(DelegateContext.journal_dir 注入,
//!   子代理事件流落 data/sessions/{sid}.jsonl)——子轨迹经既有导出通路
//!   独立产出,父链路经 AtifSources.parent_session_id 标注(调用方经
//!   journal::scan_delegate_spawns 反查传入);主轨迹中 delegate 仍呈现为
//!   普通工具调用,委托锚事件(delegate_spawned)不映射为步(映射表 §六 v1 口径);
//! - `compaction_performed` 运行时已接线(裁剪时落 journal),导出按映射表
//!   §四.4 产出 context_management 系统步;
//! - `session_crashed` 运行时不写(PR-2 resume 检测补写),导出遇此事件即截断。

use std::collections::HashMap;

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::agent::journal::{JournalEvent, JournalLine};
use crate::agent::memory::MessageRecord;

/// ATIF 规范版本(常量,映射表 §三)
pub const ATIF_SCHEMA_VERSION: &str = "ATIF-v1.8";

/// root.notes 固定声明文本(映射表 §八,逐字写入,导出器不拼接动态内容)
pub const ATIF_NOTES: &str =
    "Exported by evo-agent atif.rs from local journal + evorule audit chain.\n\
Token accounting: metrics use provider-reported usage when available;\n\
steps lacking provider usage use estimated tokens split 7:3 (prompt:completion).\n\
tool_call_id is synthesized as \"t{journal_seq}\" (no provider ids in evo-agent).\n\
cost_usd/cached_tokens omitted: subscription pricing, no per-token billing;\n\
provider does not report cache-hit tokens.\n\
Sidecar LLM calls (summarize/rollup) are internal context management, not\n\
agent steps. Steps with is_copied_context=true are rebuilt history (SFT: filter).";

/// tokens_est 无真值时的拆分比例(映射表 §四.3:prompt:completion = 7:3)
const EST_SPLIT_PROMPT: u64 = 7;
const EST_SPLIT_COMPLETION: u64 = 3;

/// 导出输入三源快照(纯数据,调用方装配)
pub struct AtifSources<'a> {
    /// evorule 会话 id(原样;root.session_id / trajectory_id 同值)
    pub session_id: &'a str,
    /// journal 事件流(`journal::read_all` 产物,seq 已连续性校验)
    pub journal: &'a [JournalLine],
    /// transcript 消息投影(`session_index::load_transcript` 产物,idx 升序)
    pub transcript: &'a [MessageRecord],
    /// 审计链事实全文(`EvoruleApiClient::replay` 产物,fact.to_json() 列表)
    pub audit_facts: &'a [Value],
    /// 工具定义(OpenAI function calling schema 数组;无则缺省)
    pub tool_definitions: Option<Value>,
    /// 父会话链路(委托子轨迹标注;调用方经父 journal 扫描
    /// `scan_delegate_spawns` 反查后传入,无则缺省——schema 零改动)
    pub parent_session_id: Option<String>,
    /// 审计锚点列表(server ≥0.9.2 `/anchors` 产物整包;无锚点/旧 server
    /// 传 None——extra 扩展位承载,schema 零改动)
    pub audit_anchors: Option<&'a Value>,
}

/// 导出错误(fail-visible,不静默产出半截轨迹)
#[derive(Debug, thiserror::Error)]
pub enum AtifExportError {
    /// 输入数据不足以构成合法轨迹
    #[error("atif export: {0}")]
    InvalidInput(String),
}

/// ATIF 轨迹(root;字段序即序列化序,均按 RFC 命名)
#[derive(Debug, Clone, Serialize)]
pub struct AtifTrajectory {
    /// ATIF 兼容版本标识
    pub schema_version: String,
    /// 运行(run)级会话 id
    pub session_id: String,
    /// 文档级轨迹 id(standalone 导出 = session_id)
    pub trajectory_id: String,
    /// agent 配置
    pub agent: AtifAgent,
    /// 步序列(step_id 从 1 连续)
    pub steps: Vec<AtifStep>,
    /// 固定声明文本(§八)
    pub notes: String,
    /// 全程聚合指标
    pub final_metrics: AtifFinalMetrics,
    /// 导出器自定义元数据
    pub extra: AtifRootExtra,
}

/// ATIF AgentSchema(RFC §AgentSchema)
#[derive(Debug, Clone, Serialize)]
pub struct AtifAgent {
    /// agent 系统名
    pub name: String,
    /// agent 版本(CARGO_PKG_VERSION)
    pub version: String,
    /// 默认模型(首个 react llm_called.model)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// 工具定义(OpenAI function calling schema 数组)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_definitions: Option<Value>,
}

/// ATIF FinalMetricsSchema(映射表 §三:仅输出两 token 聚合,不虚构 cached/cost)
#[derive(Debug, Clone, Serialize)]
pub struct AtifFinalMetrics {
    /// Σ prompt_tokens(react 步)
    pub total_prompt_tokens: u64,
    /// Σ completion_tokens(react 步)
    pub total_completion_tokens: u64,
}

/// root.extra(映射表 §三)
#[derive(Debug, Clone, Serialize)]
pub struct AtifRootExtra {
    /// journal 文件相对路径
    pub source_journal: String,
    /// 导出时刻(= journal 尾事件 ts 派生,见模块文档幂等裁定)
    pub exported_at: String,
    /// journal seq 覆盖范围 [first, last]
    pub journal_seq_range: [u64; 2],
    /// 导出器标识
    pub exporter: String,
    /// 父会话链路(委托子轨迹标注;journal 含委托锚事件时落值,
    /// 无则缺省——ATIF v1.8 无父子轨迹字段,链路信息走 extra 扩展位,
    /// schema 零改动)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// 轨迹是否为崩溃后续接(恢复标记在账时为 true;extra 扩展位,
    /// 既有导出恒 false 不序列化,字节不变)
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub resumed_after_crash: bool,
    /// 事实链(审计链)末哈希——锚点绑定基准;receipt `audit_chain_head` 同源
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_chain_head: Option<String>,
    /// 审计锚点背书(末锚点签名段;V-1 抗篡改扩展位,格式版本自标)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_anchor: Option<AtifAnchorEndorsement>,
    /// 脱敏器执行记录(命中数>0 才序列化;零命中=原文导出字节不变)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sanitized: Option<AtifSanitizeReport>,
}

/// 末锚点签名段(extra.audit_anchor;数据来自 server `/anchors` 末元素)
#[derive(Debug, Clone, Serialize)]
pub struct AtifAnchorEndorsement {
    /// 锚点格式版本(独立于 ATIF schema_version 自标)
    pub anchor_format: String,
    /// 锚点序号
    pub seq: u64,
    /// 覆盖事实区间
    pub fact_range: [u64; 2],
    /// 签名密钥标识
    pub key_id: String,
    /// 引擎/部署标识
    pub engine_id: String,
    /// 锚点哈希(载荷哈希)
    pub anchor_hash: String,
    /// ed25519 签名(hex)
    pub signature: String,
}

/// 脱敏执行报告(extra.sanitized)
#[derive(Debug, Clone, Serialize)]
pub struct AtifSanitizeReport {
    /// 命中的秘密模式计数(按模式)
    pub hits: std::collections::BTreeMap<String, u64>,
    /// 总替换次数
    pub total_replaced: u64,
}

/// ATIF StepObject(RFC §StepObject)
#[derive(Debug, Clone, Serialize)]
pub struct AtifStep {
    /// 步序号(从 1 连续递增)
    pub step_id: u64,
    /// ISO 8601 时间戳(UTC)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// system|user|agent
    pub source: String,
    /// 本步模型(agent 步)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// 消息全文(必填,可空串)
    pub message: String,
    /// 工具调用(agent 步)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<AtifToolCall>>,
    /// 环境反馈
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<AtifObservation>,
    /// LLM 指标(agent 步)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<AtifMetrics>,
    /// 步级自定义元数据
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Value>,
    /// 本步 LLM 推理次数(One-LLM-per-step = 1)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_call_count: Option<u64>,
}

/// ATIF ToolCallSchema(RFC §ToolCallSchema)
#[derive(Debug, Clone, Serialize)]
pub struct AtifToolCall {
    /// 合成调用 id("t{seq}")
    pub tool_call_id: String,
    /// 工具名
    pub function_name: String,
    /// 参数全文(必为 JSON object;空参 = {})
    pub arguments: Value,
    /// 调用级元数据(审批轨迹)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<Value>,
}

/// ATIF ObservationSchema(RFC §ObservationSchema)
#[derive(Debug, Clone, Serialize)]
pub struct AtifObservation {
    /// 结果列表(与 tool_calls 按 source_call_id 配对)
    pub results: Vec<AtifObservationResult>,
}

/// ATIF ObservationResultSchema(RFC §ObservationResultSchema)
#[derive(Debug, Clone, Serialize)]
pub struct AtifObservationResult {
    /// 配对的 tool_call_id
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_call_id: Option<String>,
    /// 输出全文(序列化文本;无据可依时缺省,不虚构)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// ATIF MetricsSchema(映射表 §四.3:仅 prompt/completion,cached/cost 不输出)
#[derive(Debug, Clone, Serialize)]
pub struct AtifMetrics {
    /// 输入 token(含缓存命中部分)
    pub prompt_tokens: u64,
    /// 输出 token
    pub completion_tokens: u64,
}

/// 审计链事实索引(IoRequest by id / IoResponse by request_id)
struct AuditIndex<'a> {
    io_requests: HashMap<u64, &'a Value>,
    io_responses: HashMap<u64, &'a Value>,
}

impl<'a> AuditIndex<'a> {
    /// 从 replay 事实列表构建索引(非 IoRequest/IoResponse 事实跳过)
    fn build(facts: &'a [Value]) -> AuditIndex<'a> {
        let mut idx = AuditIndex {
            io_requests: HashMap::new(),
            io_responses: HashMap::new(),
        };
        for f in facts {
            match f.get("type").and_then(|t| t.as_str()) {
                Some("IoRequest") => {
                    if let Some(id) = f.get("id").and_then(|v| v.as_u64()) {
                        idx.io_requests.insert(id, f);
                    }
                }
                Some("IoResponse") => {
                    if let Some(rid) = f.get("request_id").and_then(|v| v.as_u64()) {
                        idx.io_responses.insert(rid, f);
                    }
                }
                _ => {}
            }
        }
        idx
    }

    /// IoRequest(call_service).params.args → 参数全文;缺参/null 规范化为 {}
    fn service_args(&self, request_id: u64) -> Option<Value> {
        let req = self.io_requests.get(&request_id)?;
        let args = req.get("params").and_then(|p| p.get("args"));
        Some(match args {
            Some(v @ Value::Object(_)) => v.clone(),
            _ => json!({}),
        })
    }

    /// IoResponse(request_id) → 观察内容文本:
    /// error=Some → 错误描述;否则 result 序列化(result null 且无 error → 空串)
    fn response_content(&self, request_id: u64) -> Option<String> {
        let resp = self.io_responses.get(&request_id)?;
        if let Some(err) = resp.get("error").and_then(|e| e.as_str()) {
            return Some(err.to_string());
        }
        match resp.get("result") {
            Some(v) if !v.is_null() => Some(v.to_string()),
            _ => Some(String::new()),
        }
    }

    /// IoResponse(request_id).result.content → assistant 全文(LLM 推理结果)
    fn llm_content(&self, request_id: u64) -> Option<String> {
        let resp = self.io_responses.get(&request_id)?;
        resp.get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
    }
}

/// agent 步累积器(一个 react llm_called 开步,收步时终化)
struct AgentAcc {
    /// 开步事件 ts(unix ms)
    ts: u64,
    /// model 名
    model: String,
    /// journal llm_called.response 摘要(兜底消息源)
    response_digest: String,
    /// provider token 真值
    tokens: Option<crate::agent::journal::TokenRecord>,
    /// 估算总量(fallback)
    tokens_est: Option<u64>,
    /// 本步是第几个 react 调用(0 起,对位 transcript assistant 消息)
    react_ordinal: usize,
    /// 本步 evorule_request_id(审计链 join 键;本地路径 None)
    evorule_request_id: Option<u64>,
    /// 步内工具调用(按 seq 序)
    calls: Vec<CallAcc>,
    /// 治理裁决(verdict 按 seq 序追加)
    policy_verdicts: Vec<String>,
}

/// 步内单工具调用累积器
struct CallAcc {
    /// 合成 id("t{seq}")
    call_id: String,
    /// 工具名
    tool: String,
    /// 同名调用在步内的序号(0 起;transcript arguments 对位用)
    name_ordinal: usize,
    /// 审计链 IoRequest id(call_service;本地 None)
    evorule_request_id: Option<u64>,
    /// 已配对结果
    result: Option<CallResult>,
    /// 审批轨迹 [approval_id, decision](decision 未决 = None)
    approval: Option<(String, Option<String>)>,
}

/// 工具结果
struct CallResult {
    /// 结果在步内本地回退序(0 起;仅未配审计链的调用计数)
    local_ordinal: Option<usize>,
}

/// 导出三源快照为 ATIF v1.8 轨迹(纯函数;确定性/幂等见模块文档)
///
/// journal 空流返回错误(fail-visible):空会话不构成合法参赛轨迹。
pub fn export(sources: AtifSources<'_>) -> Result<AtifTrajectory, AtifExportError> {
    if sources.journal.is_empty() {
        return Err(AtifExportError::InvalidInput(
            "journal is empty (no events recorded for this session)".into(),
        ));
    }
    let audit = AuditIndex::build(sources.audit_facts);

    // transcript 预索引:system 首 / user 列表 / assistant 列表
    let system_msg = sources
        .transcript
        .iter()
        .find(|m| m.role == "system")
        .map(|m| m.content.clone());
    let user_msgs: Vec<&MessageRecord> = sources
        .transcript
        .iter()
        .filter(|m| m.role == "user")
        .collect();
    let assistant_msgs: Vec<&MessageRecord> = sources
        .transcript
        .iter()
        .filter(|m| m.role == "assistant")
        .collect();

    let mut steps: Vec<AtifStep> = Vec::new();
    let mut next_step_id: u64 = 1;
    let mut user_ordinal: usize = 0;
    let mut react_ordinal: usize = 0;
    let mut acc: Option<AgentAcc> = None;
    let mut total_prompt: u64 = 0;
    let mut total_completion: u64 = 0;

    // system 步(首步;transcript 无 system 消息则不产出,RFC 不强制)
    // system 步是否已产出(恢复点打标跳过 system 步用;system_msg 随步构造 move)
    let has_system_step = system_msg.is_some();
    if let Some(sys) = system_msg {
        steps.push(AtifStep {
            step_id: next_step_id,
            timestamp: None,
            source: "system".into(),
            model_name: None,
            message: sys,
            tool_calls: None,
            observation: None,
            metrics: None,
            extra: None,
            llm_call_count: None,
        });
        next_step_id += 1;
    }

    // 崩溃后是否续跑:journal 含恢复标记 = 截断点后还有续接段(导出两段);
    // 纯崩溃轨迹维持截断导出语义(恢复标记缺席即在此截断)。
    let resumed_after_crash = sources
        .journal
        .iter()
        .any(|l| matches!(l.event, JournalEvent::SessionResumed { .. }));

    for line in sources.journal {
        // session_crashed = 轨迹截断点(映射表 §四.5/§六);续接轨迹不在此
        // 截断——恢复点之后的续接段照常映射,此前的对话步在恢复点整体
        // 标记重建历史(is_copied_context,SFT 过滤面)
        if matches!(line.event, JournalEvent::SessionCrashed { .. }) {
            if resumed_after_crash {
                continue;
            }
            break;
        }
        match &line.event {
            JournalEvent::TurnStarted { goal, .. } => {
                // 轮界:上一个 agent 步必已随 turn_ended 收步;开用户步
                let message = match user_msgs.get(user_ordinal) {
                    Some(m) => m.content.clone(),
                    // transcript 缺对位(投影丢失)时回退 journal goal(截 512)
                    None => goal.clone(),
                };
                user_ordinal += 1;
                steps.push(AtifStep {
                    step_id: next_step_id,
                    timestamp: Some(iso8601_from_unix_ms(line.ts)),
                    source: "user".into(),
                    model_name: None,
                    message,
                    tool_calls: None,
                    observation: None,
                    metrics: None,
                    extra: None,
                    llm_call_count: None,
                });
                next_step_id += 1;
            }
            JournalEvent::LlmCalled {
                model,
                purpose,
                evorule_request_id,
                tokens,
                tokens_est,
                response,
                ..
            } => {
                if purpose != "react" {
                    // sidecar 调用不映射为步(映射表 §六)
                    continue;
                }
                // 先收上一个 agent 步(One-LLM-per-step)
                if let Some(a) = acc.take() {
                    finalize_agent_step(
                        a,
                        &audit,
                        sources.transcript,
                        &assistant_msgs,
                        &mut steps,
                        &mut next_step_id,
                        &mut total_prompt,
                        &mut total_completion,
                    );
                }
                acc = Some(AgentAcc {
                    ts: line.ts,
                    model: model.clone(),
                    response_digest: response.clone(),
                    tokens: tokens.clone(),
                    tokens_est: *tokens_est,
                    react_ordinal,
                    evorule_request_id: *evorule_request_id,
                    calls: Vec::new(),
                    policy_verdicts: Vec::new(),
                });
                react_ordinal += 1;
            }
            JournalEvent::ToolInvoked {
                call_id,
                tool,
                evorule_request_id,
                ..
            } => {
                // 无开步时(异常流:工具先于首个 react)合成空步承接,RFC 合法
                let a = acc.get_or_insert_with(|| AgentAcc {
                    ts: line.ts,
                    model: String::new(),
                    response_digest: String::new(),
                    tokens: None,
                    tokens_est: None,
                    react_ordinal: usize::MAX,
                    evorule_request_id: None,
                    calls: Vec::new(),
                    policy_verdicts: Vec::new(),
                });
                // 同名序号:跨本步既有调用计数
                let name_ordinal = a.calls.iter().filter(|c| c.tool == *tool).count();
                a.calls.push(CallAcc {
                    call_id: call_id.clone(),
                    tool: tool.clone(),
                    name_ordinal,
                    evorule_request_id: *evorule_request_id,
                    result: None,
                    approval: None,
                });
            }
            JournalEvent::ToolResult { call_id, .. } => {
                if let Some(a) = acc.as_mut() {
                    if let Some(pos) = a.calls.iter().rposition(|c| &c.call_id == call_id) {
                        // 本地回退序 = 该调用之前既有本地调用数(0 起)
                        let local_ordinal = if a.calls[pos].evorule_request_id.is_none() {
                            Some(
                                a.calls[..pos]
                                    .iter()
                                    .filter(|x| x.evorule_request_id.is_none())
                                    .count(),
                            )
                        } else {
                            None
                        };
                        a.calls[pos].result = Some(CallResult { local_ordinal });
                    }
                }
            }
            JournalEvent::ApprovalRequested { approval_id, .. } => {
                // 审批归属于最近一次工具调用(映射表 §六)
                if let Some(a) = acc.as_mut() {
                    if let Some(c) = a.calls.last_mut() {
                        c.approval = Some((approval_id.clone(), None));
                    }
                }
            }
            JournalEvent::ApprovalResolved {
                approval_id,
                decision,
            } => {
                if let Some(a) = acc.as_mut() {
                    if let Some(c) = a.calls.iter_mut().rev().find(|c| {
                        c.approval.as_ref().map(|(id, _)| id.as_str()) == Some(approval_id.as_str())
                    }) {
                        c.approval = Some((approval_id.clone(), Some(decision.clone())));
                    }
                }
            }
            JournalEvent::PolicyJudged { verdict, .. } => {
                if let Some(a) = acc.as_mut() {
                    a.policy_verdicts.push(verdict.clone());
                }
            }
            JournalEvent::CompactionPerformed {
                cleared_call_ids, ..
            } => {
                // 压缩边界:先收 agent 步,再产 context_management 系统步(§四.4)
                if let Some(a) = acc.take() {
                    finalize_agent_step(
                        a,
                        &audit,
                        sources.transcript,
                        &assistant_msgs,
                        &mut steps,
                        &mut next_step_id,
                        &mut total_prompt,
                        &mut total_completion,
                    );
                }
                let ids = cleared_call_ids.join(", ");
                steps.push(AtifStep {
                    step_id: next_step_id,
                    timestamp: Some(iso8601_from_unix_ms(line.ts)),
                    source: "system".into(),
                    model_name: None,
                    message: "Context compaction performed".into(),
                    tool_calls: None,
                    observation: Some(AtifObservation {
                        results: vec![AtifObservationResult {
                            source_call_id: None,
                            content: Some(format!(
                                "cleared {} tool results: [{}]",
                                cleared_call_ids.len(),
                                ids
                            )),
                        }],
                    }),
                    metrics: None,
                    extra: Some(json!({
                        "context_management": {"type": "compaction", "boundary": "replace"}
                    })),
                    llm_call_count: None,
                });
                next_step_id += 1;
            }
            JournalEvent::SedimentPerformed {
                summary_written,
                stable_facts,
                stable_facts_cache_only,
                events_count,
                rollup_done,
                knowledge_candidates,
                ..
            } => {
                // 沉淀边界——context_management 系统步（四项持久化结果可对账）
                if let Some(a) = acc.take() {
                    finalize_agent_step(
                        a,
                        &audit,
                        sources.transcript,
                        &assistant_msgs,
                        &mut steps,
                        &mut next_step_id,
                        &mut total_prompt,
                        &mut total_completion,
                    );
                }
                steps.push(AtifStep {
                    step_id: next_step_id,
                    timestamp: Some(iso8601_from_unix_ms(line.ts)),
                    source: "system".into(),
                    model_name: None,
                    message: "Session sediment performed".into(),
                    tool_calls: None,
                    observation: Some(AtifObservation {
                        results: vec![AtifObservationResult {
                            source_call_id: None,
                            content: Some(format!(
                                "summary_written={} persisted_facts={} cache_only_facts={} events={} rollup={} knowledge_candidates={}",
                                summary_written,
                                stable_facts.len(),
                                stable_facts_cache_only.len(),
                                events_count,
                                rollup_done,
                                knowledge_candidates
                            )),
                        }],
                    }),
                    metrics: None,
                    extra: Some(json!({
                        "context_management": {"type": "sediment", "boundary": "session_end"}
                    })),
                    llm_call_count: None,
                });
                next_step_id += 1;
            }
            JournalEvent::WireRendered {
                round,
                wire_len,
                content_hash,
                ..
            } => {
                // 逐轮 wire 留痕——context_management 系统步(B-1)。wire 全文
                // 已在 journal 事件 payload(权威面),导出步仅留指针字段
                // (round/len/hash)不重复全文;重建比对在 journal 侧进行(F-903)
                if let Some(a) = acc.take() {
                    finalize_agent_step(
                        a,
                        &audit,
                        sources.transcript,
                        &assistant_msgs,
                        &mut steps,
                        &mut next_step_id,
                        &mut total_prompt,
                        &mut total_completion,
                    );
                }
                steps.push(AtifStep {
                    step_id: next_step_id,
                    timestamp: Some(iso8601_from_unix_ms(line.ts)),
                    source: "system".into(),
                    model_name: None,
                    message: "Context wire rendered".into(),
                    tool_calls: None,
                    observation: Some(AtifObservation {
                        results: vec![AtifObservationResult {
                            source_call_id: None,
                            content: Some(format!(
                                "round={} wire_len={} hash={}",
                                round, wire_len, content_hash
                            )),
                        }],
                    }),
                    metrics: None,
                    extra: Some(json!({
                        "context_management": {
                            "type": "wire_rendered",
                            "round": round,
                            "wire_len": wire_len,
                            "content_hash": content_hash
                        }
                    })),
                    llm_call_count: None,
                });
                next_step_id += 1;
            }
            JournalEvent::I2ScanReport { round, conflicts } => {
                // I2 冲突扫描报告——context_management 系统步(C-3/F-905)。
                // 冲突记录随事件 payload 在 journal(权威面),导出步全量携带
                // (初版低频事件,不做摘录截断)
                if let Some(a) = acc.take() {
                    finalize_agent_step(
                        a,
                        &audit,
                        sources.transcript,
                        &assistant_msgs,
                        &mut steps,
                        &mut next_step_id,
                        &mut total_prompt,
                        &mut total_completion,
                    );
                }
                steps.push(AtifStep {
                    step_id: next_step_id,
                    timestamp: Some(iso8601_from_unix_ms(line.ts)),
                    source: "system".into(),
                    model_name: None,
                    message: "Context I2 conflict scan".into(),
                    tool_calls: None,
                    observation: Some(AtifObservation {
                        results: vec![AtifObservationResult {
                            source_call_id: None,
                            content: Some(format!("round={} conflicts={}", round, conflicts.len())),
                        }],
                    }),
                    metrics: None,
                    extra: Some(json!({
                        "context_management": {
                            "type": "i2_scan_report",
                            "round": round,
                            "conflicts": conflicts
                        }
                    })),
                    llm_call_count: None,
                });
                next_step_id += 1;
            }
            JournalEvent::SummaryFidelityScan {
                session,
                trimmed_n,
                anchors_n,
                hit_n,
                ratio,
                summary_empty,
            } => {
                // 摘要保真对照——context_management 系统步(规格修正批交付物 B)。
                // 事件 payload 全量随 extra 导出(低频事件不做摘录)
                if let Some(a) = acc.take() {
                    finalize_agent_step(
                        a,
                        &audit,
                        sources.transcript,
                        &assistant_msgs,
                        &mut steps,
                        &mut next_step_id,
                        &mut total_prompt,
                        &mut total_completion,
                    );
                }
                steps.push(AtifStep {
                    step_id: next_step_id,
                    timestamp: Some(iso8601_from_unix_ms(line.ts)),
                    source: "system".into(),
                    model_name: None,
                    message: "Summary fidelity scan".into(),
                    tool_calls: None,
                    observation: Some(AtifObservation {
                        results: vec![AtifObservationResult {
                            source_call_id: None,
                            content: Some(format!(
                                "session={session} trimmed={trimmed_n} anchors={anchors_n} hit={hit_n} ratio={ratio}"
                            )),
                        }],
                    }),
                    metrics: None,
                    extra: Some(json!({
                        "context_management": {
                            "type": "summary_fidelity_scan",
                            "session": session,
                            "trimmed_n": trimmed_n,
                            "anchors_n": anchors_n,
                            "hit_n": hit_n,
                            "ratio": ratio,
                            "summary_empty": summary_empty
                        }
                    })),
                    llm_call_count: None,
                });
                next_step_id += 1;
            }
            JournalEvent::WireBlobExpired {
                round,
                content_hash,
                wire_len,
            } => {
                // wire blob 过期——context_management 系统步(体积治理批)。
                // 降级可见:hash+长度仍可校验完整性,全文级重建随窗口关闭降级
                if let Some(a) = acc.take() {
                    finalize_agent_step(
                        a,
                        &audit,
                        sources.transcript,
                        &assistant_msgs,
                        &mut steps,
                        &mut next_step_id,
                        &mut total_prompt,
                        &mut total_completion,
                    );
                }
                steps.push(AtifStep {
                    step_id: next_step_id,
                    timestamp: Some(iso8601_from_unix_ms(line.ts)),
                    source: "system".into(),
                    model_name: None,
                    message: "Wire blob expired".into(),
                    tool_calls: None,
                    observation: Some(AtifObservation {
                        results: vec![AtifObservationResult {
                            source_call_id: None,
                            content: Some(format!(
                                "round={round} wire_len={wire_len} hash={content_hash}"
                            )),
                        }],
                    }),
                    metrics: None,
                    extra: Some(json!({
                        "context_management": {
                            "type": "wire_blob_expired",
                            "round": round,
                            "content_hash": content_hash,
                            "wire_len": wire_len
                        }
                    })),
                    llm_call_count: None,
                });
                next_step_id += 1;
            }
            JournalEvent::RecallSet { session, hits } => {
                // 召回集观测——context_management 系统步(检索质量观测批
                // K-11 观测级;ground truth 判据列二期)
                if let Some(a) = acc.take() {
                    finalize_agent_step(
                        a,
                        &audit,
                        sources.transcript,
                        &assistant_msgs,
                        &mut steps,
                        &mut next_step_id,
                        &mut total_prompt,
                        &mut total_completion,
                    );
                }
                steps.push(AtifStep {
                    step_id: next_step_id,
                    timestamp: Some(iso8601_from_unix_ms(line.ts)),
                    source: "system".into(),
                    model_name: None,
                    message: "Recall set observation".into(),
                    tool_calls: None,
                    observation: Some(AtifObservation {
                        results: vec![AtifObservationResult {
                            source_call_id: None,
                            content: Some(format!("session={session} hits={}", hits.join(" | "))),
                        }],
                    }),
                    metrics: None,
                    extra: Some(json!({
                        "context_management": {
                            "type": "recall_set",
                            "session": session,
                            "hits": hits
                        }
                    })),
                    llm_call_count: None,
                });
                next_step_id += 1;
            }
            JournalEvent::LexCacheStats {
                session,
                hit,
                expired,
                fetch,
            } => {
                // LexStore 缓存观测——context_management 系统步(补齐路线图
                // P2-1/TTL 窗口可见性;recall_set 同族观测级)
                if let Some(a) = acc.take() {
                    finalize_agent_step(
                        a,
                        &audit,
                        sources.transcript,
                        &assistant_msgs,
                        &mut steps,
                        &mut next_step_id,
                        &mut total_prompt,
                        &mut total_completion,
                    );
                }
                steps.push(AtifStep {
                    step_id: next_step_id,
                    timestamp: Some(iso8601_from_unix_ms(line.ts)),
                    source: "system".into(),
                    model_name: None,
                    message: "Lex cache stats".into(),
                    tool_calls: None,
                    observation: Some(AtifObservation {
                        results: vec![AtifObservationResult {
                            source_call_id: None,
                            content: Some(format!(
                                "session={session} hit={hit} expired={expired} fetch={fetch}"
                            )),
                        }],
                    }),
                    metrics: None,
                    extra: Some(json!({
                        "context_management": {
                            "type": "lex_cache_stats",
                            "session": session,
                            "hit": hit,
                            "expired": expired,
                            "fetch": fetch
                        }
                    })),
                    llm_call_count: None,
                });
                next_step_id += 1;
            }
            JournalEvent::TurnEnded { .. } => {
                // 轮界收步(映射表 §四.3 边界切割)
                if let Some(a) = acc.take() {
                    finalize_agent_step(
                        a,
                        &audit,
                        sources.transcript,
                        &assistant_msgs,
                        &mut steps,
                        &mut next_step_id,
                        &mut total_prompt,
                        &mut total_completion,
                    );
                }
            }
            // SessionResumed:恢复点——此前全部对话步=重建历史(重建时原样
            // 回喂进续跑上下文,非本段新鲜产出),整体标 is_copied_context
            //(SFT 过滤面);system 步为持久脚手架不属重建历史,不标
            JournalEvent::SessionResumed { .. } => {
                let skip = usize::from(has_system_step);
                for s in steps.iter_mut().skip(skip) {
                    let mut extra = s
                        .extra
                        .take()
                        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
                    if let Some(obj) = extra.as_object_mut() {
                        obj.insert(
                            "is_copied_context".to_string(),
                            serde_json::Value::Bool(true),
                        );
                    }
                    s.extra = Some(extra);
                }
            }
            // 委托子会话锚:元数据事件不映射为步——主轨迹中 delegate 仍呈现为
            // 普通工具调用(映射口径 v1 不变);链路经 root.extra.parent_session_id
            // 在子轨迹侧标注
            JournalEvent::DelegateSpawned { .. } => {}
            // 冷迁计数:账面观测事件不映射步(观测面口径)
            JournalEvent::ColdMoved { .. } => {}
            // 工具重试观测:不映射步(观测面口径;重试明细在 journal 事件)
            JournalEvent::ToolRetried { .. } => {}
            // 粒级检查点及其大结果载体:不映射步(观测面口径;恢复面在
            // journal 侧回放,轨迹步只反映对话粒)
            JournalEvent::NodeCheckpointed { .. } => {}
            JournalEvent::CheckpointBlob { .. } => {}
            // 计划循环检查点:不映射步(观测面口径;驱动状态在 journal 侧回放)
            JournalEvent::PlanLoopCheckpointed { .. } => {}
            // 计划循环终态标记:不映射步(观测面口径)
            JournalEvent::PlanLoopFinished { .. } => {}
            // SessionCrashed 已在循环头截断
            JournalEvent::SessionCrashed { .. } => {}
            // HandoverWritten:交接点语义锚(写档动作镜像已在 tool_invoked/
            // tool_result);v1 忽略,跨会话链步映射待 session_spawn 接线后一并设计
            JournalEvent::HandoverWritten { .. } => {}
            // SessionSpawned/ChainHalted(自主交接批):会话链派生/熔断语义锚
            // ——派生因果权威在 server 侧 parent_session_id 链,熔断由 turn_ended
            // 序列本身可见;v1 忽略,链级轨迹映射待跨会话步设计一并落
            JournalEvent::SessionSpawned { .. } => {}
            JournalEvent::ChainHalted { .. } => {}
        }
    }
    // 流末悬挂 agent 步(无 turn_ended 尾:crash/截断场景)
    if let Some(a) = acc.take() {
        finalize_agent_step(
            a,
            &audit,
            sources.transcript,
            &assistant_msgs,
            &mut steps,
            &mut next_step_id,
            &mut total_prompt,
            &mut total_completion,
        );
    }

    let first_seq = sources.journal.first().map(|l| l.seq).unwrap_or(1);
    let last_seq = sources.journal.last().map(|l| l.seq).unwrap_or(0);
    let exported_at = iso8601_from_unix_ms(sources.journal.last().map(|l| l.ts).unwrap_or(0));
    let model_name = assistant_model_of(sources.journal);

    // V-1:末锚点签名段与事实链头(来自 server `/anchors`;无锚点全缺省——
    // 既有导出路径字节不变)。链头取末锚点 chain_head(锚点绑定基准)。
    let (audit_chain_head, audit_anchor) = anchor_endorsement_of(sources.audit_anchors);

    // V-1:脱敏器——steps 文本面秘密扫描(零命中=报告 None,字节不变;
    // 命中即替换并落 extra.sanitized,秘密零漏出断言由测试背书)
    let sanitized = sanitize_trajectory(&mut steps);

    Ok(AtifTrajectory {
        schema_version: ATIF_SCHEMA_VERSION.to_string(),
        session_id: sources.session_id.to_string(),
        trajectory_id: sources.session_id.to_string(),
        agent: AtifAgent {
            name: "evo-agent".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            model_name,
            tool_definitions: sources.tool_definitions.clone(),
        },
        steps,
        notes: ATIF_NOTES.to_string(),
        final_metrics: AtifFinalMetrics {
            total_prompt_tokens: total_prompt,
            total_completion_tokens: total_completion,
        },
        extra: AtifRootExtra {
            source_journal: format!(
                "data/sessions/{}.jsonl",
                crate::agent::journal::sanitize_session_id(sources.session_id)
            ),
            exported_at,
            journal_seq_range: [first_seq, last_seq],
            exporter: format!("evo-agent atif v{}", env!("CARGO_PKG_VERSION")),
            parent_session_id: sources.parent_session_id.clone(),
            resumed_after_crash,
            audit_chain_head,
            audit_anchor,
            sanitized,
        },
    })
}

/// 脱敏器(V-1):导出前秘密模式扫描与替换
///
/// 范围:`steps[].message` + `steps[].observation[].results[].content`
/// (transcript 与 journal 事件的人类可读投影面;结构化字段不扫——
/// 工具 schema/审计哈希属非秘密)。命中替换为 `[REDACTED:<mode>]` 并计数;
/// 零命中返回 None(extra.sanitized 不序列化,导出字节不变)。
///
/// 模式集(保守白名单式,宁漏报不误伤正文):
/// - `sk-` 前缀 token(OpenAI 风格,20+ 连续词字符)
/// - `Bearer <token>`(HTTP 授权头)
/// - `EVORULE_ANCHOR_SEED` 环境变量赋值形态
/// - 长 hex 串(64 位,密钥种子形态)
fn sanitize_trajectory(steps: &mut [AtifStep]) -> Option<AtifSanitizeReport> {
    use std::collections::BTreeMap;
    let mut hits: BTreeMap<String, u64> = BTreeMap::new();
    let replace = |s: &mut String, hits: &mut BTreeMap<String, u64>| {
        let orig_len = s.len();
        let (out, n) = sanitize_text(s, hits);
        *s = out;
        orig_len != s.len() || n > 0
    };
    for step in steps.iter_mut() {
        replace(&mut step.message, &mut hits);
        if let Some(obs) = step.observation.as_mut() {
            for r in obs.results.iter_mut() {
                if let Some(c) = r.content.as_mut() {
                    replace(c, &mut hits);
                }
            }
        }
    }
    if hits.is_empty() {
        return None;
    }
    let total_replaced = hits.values().sum();
    Some(AtifSanitizeReport { hits, total_replaced })
}

/// 单串秘密扫描(返回替换后文本与命中计数)
fn sanitize_text(
    input: &str,
    hits: &mut std::collections::BTreeMap<String, u64>,
) -> (String, u64) {
    let mut out = input.to_string();
    let mut total: u64 = 0;
    // sk- token:前缀锚定,20+ 词字符
    let (o, n) = replace_pattern(&out, "sk-token", r"sk-[A-Za-z0-9_-]{20,}", hits);
    out = o;
    total += n;
    // Bearer token
    let (o, n) = replace_pattern(&out, "bearer", r"(?i)bearer\s+[A-Za-z0-9._~+/-]{16,}", hits);
    out = o;
    total += n;
    // 种子环境变量赋值
    let (o, n) = replace_pattern(
        &out,
        "anchor-seed",
        r"EVORULE_ANCHOR_SEED\s*[=:]\s*[0-9a-fA-F]{64}",
        hits,
    );
    out = o;
    total += n;
    // 裸 64-hex(密钥种子形态;前后须非 hex 字符防截长哈希误伤——链哈希常见 64hex,
    // 但链哈希只出现在结构化字段不在扫面,此处文本面命中即按秘密处理)
    let (o, n) = replace_pattern(&out, "hex64", r"(?<![0-9a-fA-F])[0-9a-fA-F]{64}(?![0-9a-fA-F])", hits);
    out = o;
    total += n;
    (out, total)
}

/// 正则替换并按模式计数(依赖由 atif 模块顶部 `use` 引入 regex——
/// Cargo 既有依赖,零新增)
fn replace_pattern(
    input: &str,
    mode: &str,
    pattern: &str,
    hits: &mut std::collections::BTreeMap<String, u64>,
) -> (String, u64) {
    let re = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(_) => return (input.to_string(), 0),
    };
    let n = re.find_iter(input).count() as u64;
    if n == 0 {
        return (input.to_string(), 0);
    }
    *hits.entry(mode.to_string()).or_insert(0) += n;
    (re.replace_all(input, regex::NoExpand(&format!("[REDACTED:{mode}]"))).to_string(), n)
}

/// 末锚点签名段提取(V-1)
///
/// 输入为 server `/anchors` 响应(`{count, anchors: [...]}`)或 anchors 数组本体;
/// 返回 (链头, 末锚点段)。无锚点/字段缺失 → (None, None)——诚实降级,不虚构。
fn anchor_endorsement_of(
    anchors: Option<&Value>,
) -> (Option<String>, Option<AtifAnchorEndorsement>) {
    let arr = anchors
        .map(|v| {
            v.get("anchors")
                .and_then(|a| a.as_array())
                .cloned()
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let last = match arr.last() {
        Some(a) => a,
        None => return (None, None),
    };
    let chain_head = last
        .get("chain_head")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let endorsement = AtifAnchorEndorsement {
        anchor_format: "evorule-anchor/1".to_string(),
        seq: last.get("seq").and_then(|v| v.as_u64()).unwrap_or(0),
        fact_range: [
            last.pointer("/fact_range/lo").and_then(|v| v.as_u64()).unwrap_or(0),
            last.pointer("/fact_range/hi").and_then(|v| v.as_u64()).unwrap_or(0),
        ],
        key_id: last
            .get("key_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        engine_id: last
            .get("engine_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        anchor_hash: last
            .get("anchor_hash")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        signature: last
            .get("signature")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    };
    (chain_head, Some(endorsement))
}

/// 首个 react llm_called 的 model(root.agent.model_name)
fn assistant_model_of(journal: &[JournalLine]) -> Option<String> {
    journal.iter().find_map(|l| match &l.event {
        JournalEvent::LlmCalled { purpose, model, .. } if purpose == "react" => Some(model.clone()),
        _ => None,
    })
}

/// 终化 agent 步:消息/metrics/tool_calls/observation 全源解析后入 steps
#[allow(clippy::too_many_arguments)]
fn finalize_agent_step(
    a: AgentAcc,
    audit: &AuditIndex<'_>,
    transcript: &[MessageRecord],
    assistant_msgs: &[&MessageRecord],
    steps: &mut Vec<AtifStep>,
    next_step_id: &mut u64,
    total_prompt: &mut u64,
    total_completion: &mut u64,
) {
    // 消息选取按轮型分流:审计链 call_external IoResponse.content 是「turn
    // 收尾全文」语义(react 中间轮不提交 io_response,单 turn 单 request_id),
    // 中间轮按 request_id 查询会命中收尾轮全文造成步间错位 → 仅收尾轮
    // (无 tool_calls)首源用审计链;中间轮直取 transcript 对位 assistant,
    // journal 摘要兜底。
    let mut message = if a.calls.is_empty() {
        a.evorule_request_id
            .and_then(|rid| audit.llm_content(rid))
            .or_else(|| {
                assistant_msgs
                    .get(a.react_ordinal)
                    .map(|m| m.content.clone())
            })
            .unwrap_or_else(|| a.response_digest.clone())
    } else {
        assistant_msgs
            .get(a.react_ordinal)
            .map(|m| m.content.clone())
            .unwrap_or_else(|| a.response_digest.clone())
    };

    // 合成空步承接的异常流:react_ordinal = MAX 表示无 react 调用,消息置空
    if a.react_ordinal == usize::MAX {
        message = String::new();
    }

    // metrics:provider 真值优先;fallback tokens_est 按 7:3(整除)
    let metrics = match &a.tokens {
        Some(t) => {
            *total_prompt += t.prompt;
            *total_completion += t.completion;
            Some(AtifMetrics {
                prompt_tokens: t.prompt,
                completion_tokens: t.completion,
            })
        }
        None => a.tokens_est.map(|est| {
            let p = est * EST_SPLIT_PROMPT / (EST_SPLIT_PROMPT + EST_SPLIT_COMPLETION);
            let c = est - p;
            *total_prompt += p;
            *total_completion += c;
            AtifMetrics {
                prompt_tokens: p,
                completion_tokens: c,
            }
        }),
    };

    // tool_calls + observation
    let mut tool_calls: Vec<AtifToolCall> = Vec::new();
    let mut results: Vec<AtifObservationResult> = Vec::new();
    if !a.calls.is_empty() {
        // transcript 对位:本步 assistant 消息(arguments 源)与其后 tool 消息(content 源)
        let asst = assistant_msgs.get(a.react_ordinal).copied();
        let tool_msgs: Vec<&MessageRecord> = match asst.map(|m| m.idx) {
            Some(idx) => transcript
                .iter()
                .skip_while(|m| m.idx <= idx)
                .take_while(|m| m.role == "tool")
                .collect(),
            None => Vec::new(),
        };
        let mut local_seen: usize = 0;
        for c in &a.calls {
            // arguments:审计链 params.args → transcript assistant.tool_calls 对位 → {}
            let arguments = match c.evorule_request_id {
                Some(rid) => audit.service_args(rid).unwrap_or_else(|| json!({})),
                None => asst
                    .and_then(|m| m.tool_calls.as_ref())
                    .map(|tc| arguments_from_transcript(tc, &c.tool, c.name_ordinal))
                    .unwrap_or_else(|| json!({})),
            };
            // extra:审批轨迹
            let call_extra: Option<Value> = c.approval.as_ref().map(|(id, decision)| {
                let mut m = Map::new();
                m.insert("approval_id".into(), json!(id));
                if let Some(d) = decision {
                    m.insert("decision".into(), json!(d));
                }
                Value::Object(m)
            });
            tool_calls.push(AtifToolCall {
                tool_call_id: c.call_id.clone(),
                function_name: c.tool.clone(),
                arguments,
                extra: call_extra,
            });
            // observation content:审计链 IoResponse → transcript tool 消息对位 → 缺省
            let content = match c.evorule_request_id {
                Some(rid) => audit.response_content(rid),
                None => {
                    let ord = c
                        .result
                        .as_ref()
                        .and_then(|r| r.local_ordinal)
                        .unwrap_or(local_seen);
                    local_seen += 1;
                    tool_msgs.get(ord).map(|m| m.content.clone())
                }
            };
            results.push(AtifObservationResult {
                source_call_id: Some(c.call_id.clone()),
                content,
            });
        }
    }

    let extra = if a.policy_verdicts.is_empty() {
        None
    } else {
        Some(json!({ "policy_verdict": a.policy_verdicts }))
    };

    steps.push(AtifStep {
        step_id: *next_step_id,
        timestamp: Some(iso8601_from_unix_ms(a.ts)),
        source: "agent".into(),
        model_name: if a.model.is_empty() {
            None
        } else {
            Some(a.model.clone())
        },
        message,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        observation: if results.is_empty() {
            None
        } else {
            Some(AtifObservation { results })
        },
        metrics,
        extra,
        llm_call_count: if a.react_ordinal == usize::MAX {
            None
        } else {
            Some(1)
        },
    });
    *next_step_id += 1;
}

/// transcript assistant.tool_calls → 参数 JSON(按工具名 + 同名序对位)
///
/// 兼容 OpenAI wire 形态(function.arguments 为 JSON 字符串)与
/// 已解析对象形态;解析失败/缺失规范化为 {}(映射表 §七.5)。
fn arguments_from_transcript(tool_calls: &Value, tool: &str, name_ordinal: usize) -> Value {
    let Some(arr) = tool_calls.as_array() else {
        return json!({});
    };
    // 两种持久化形状兼容(实测 transcript 投影为引擎 payload 形状):
    // - 引擎 payload 形状:{"tool_name": "...", "args": {...}}
    // - OpenAI function 形状:{"function": {"name": "...", "arguments": "..."}}
    let mut seen = 0usize;
    for entry in arr {
        let name = entry.get("tool_name").and_then(|v| v.as_str()).or_else(|| {
            entry
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
        });
        if name == Some(tool) {
            if seen == name_ordinal {
                if let Some(args) = entry.get("args") {
                    return args.clone();
                }
                let args = entry.get("function").and_then(|f| f.get("arguments"));
                return match args {
                    Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
                    Some(v @ Value::Object(_)) => v.clone(),
                    _ => json!({}),
                };
            }
            seen += 1;
        }
    }
    json!({})
}

/// unix 毫秒 → ISO 8601 UTC(`YYYY-MM-DDTHH:MM:SS.mmmZ`)
///
/// 无 chrono/time 依赖,Howard Hinnant civil_from_days 算法;确定性纯函数。
pub fn iso8601_from_unix_ms(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let millis = (ms % 1000) as u32;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (h, m, s) = (sod / 3_600, (sod % 3_600) / 60, sod % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_097) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::journal::{JournalEvent as JE, TokenRecord};

    /// 组装 journal 行(seq/ts 自动递增)
    struct JFix {
        lines: Vec<JournalLine>,
        seq: u64,
        ts: u64,
    }
    impl JFix {
        fn new() -> Self {
            Self {
                lines: Vec::new(),
                seq: 0,
                ts: 1_760_000_000_000,
            }
        }
        fn push(&mut self, ev: JE) -> u64 {
            self.seq += 1;
            self.ts += 1_000;
            self.lines.push(JournalLine {
                seq: self.seq,
                ts: self.ts,
                event: ev,
            });
            self.seq
        }
    }

    fn msg(idx: usize, role: &str, content: &str) -> MessageRecord {
        MessageRecord {
            idx,
            role: role.into(),
            content: content.into(),
            tool_calls: None,
            tool_name: None,
            timestamp: 0,
            fact_id: None,
        }
    }

    fn tok(p: u64, c: u64) -> Option<TokenRecord> {
        Some(TokenRecord {
            prompt: p,
            completion: c,
            total: p + c,
        })
    }

    #[test]
    fn iso8601_known_dates() {
        assert_eq!(iso8601_from_unix_ms(0), "1970-01-01T00:00:00.000Z");
        // 2025-10-16T14:30:00Z = 1760625000
        assert_eq!(
            iso8601_from_unix_ms(1_760_625_000_123),
            "2025-10-16T14:30:00.123Z"
        );
        // 闰年 2024-02-29T00:00:00Z = 1709164800
        assert_eq!(
            iso8601_from_unix_ms(1_709_164_800_000),
            "2024-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn empty_journal_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let _ = dir; // 占位
        let src = AtifSources {
            session_id: "s",
            journal: &[],
            transcript: &[],
            audit_facts: &[],
            tool_definitions: None,
            parent_session_id: None,
            audit_anchors: None,
        };
        assert!(export(src).is_err());
    }

    #[test]
    fn happy_path_call_service_full_join() {
        let mut j = JFix::new();
        j.push(JE::TurnStarted {
            turn_seq: 1,
            goal: "查股价".into(),
        });
        j.push(JE::LlmCalled {
            model: "glm-5.3".into(),
            purpose: "react".into(),
            evorule_request_id: Some(100),
            tokens: tok(520, 80),
            tokens_est: None,
            request: 3,
            response: "我去查".into(),
        });
        let t3 = j.push(JE::ToolInvoked {
            call_id: "t3".into(),
            tool: "financial_search".into(),
            args_digest: "blake3:aa".into(),
            evorule_request_id: Some(7),
        });
        assert_eq!(t3, 3);
        j.push(JE::ToolResult {
            call_id: "t3".into(),
            status: "ok".into(),
            size_bytes: 10,
            content_digest: "blake3:bb".into(),
        });
        j.push(JE::LlmCalled {
            model: "glm-5.3".into(),
            purpose: "react".into(),
            evorule_request_id: Some(101),
            tokens: tok(600, 44),
            tokens_est: None,
            request: 6,
            response: "答案是".into(),
        });
        j.push(JE::TurnEnded {
            status: "success".into(),
            steps: 2,
            duration_ms: 5_000,
        });

        let audit = vec![
            json!({
                "type": "IoRequest", "id": 7u64, "cause": 6u64,
                "io_type": "call_service",
                "params": {"tool_name": "financial_search", "args": {"ticker": "GOOGL"}}
            }),
            json!({
                "type": "IoResponse", "id": 8u64, "request_id": 7u64,
                "result": {"price": 185.35}, "error": null
            }),
            json!({
                "type": "IoRequest", "id": 100u64, "cause": 1u64,
                "io_type": "call_external", "params": {"model": "glm-5.3"}
            }),
            json!({
                "type": "IoResponse", "id": 102u64, "request_id": 100u64,
                "result": {"content": "我去查股价", "token_usage": null}, "error": null
            }),
            json!({
                "type": "IoResponse", "id": 103u64, "request_id": 101u64,
                "result": {"content": "GOOGL 现价 185.35", "token_usage": null}, "error": null
            }),
        ];
        let transcript = vec![
            msg(0, "system", "你是助手"),
            msg(1, "user", "查股价"),
            msg(2, "assistant", "我去查股价"),
            {
                let mut m = msg(3, "assistant", "GOOGL 现价 185.35");
                m.timestamp = 9;
                m
            },
        ];
        let tools = Some(json!([{
            "type": "function",
            "function": {"name": "financial_search", "parameters": {"type": "object"}}
        }]));

        let src = AtifSources {
            session_id: "42",
            journal: &j.lines,
            transcript: &transcript,
            audit_facts: &audit,
            tool_definitions: tools,
            parent_session_id: None,
            audit_anchors: None,
        };
        let t = export(src).unwrap();
        assert_eq!(t.schema_version, "ATIF-v1.8");
        assert_eq!(t.session_id, "42");
        assert_eq!(t.trajectory_id, "42");
        assert_eq!(t.agent.name, "evo-agent");
        assert_eq!(t.agent.model_name.as_deref(), Some("glm-5.3"));
        assert!(t.agent.tool_definitions.is_some());
        assert_eq!(t.notes, ATIF_NOTES);
        // 步:system + user + agent + agent
        assert_eq!(t.steps.len(), 4);
        assert_eq!(t.steps[0].source, "system");
        assert_eq!(t.steps[0].message, "你是助手");
        assert_eq!(t.steps[1].source, "user");
        assert_eq!(t.steps[1].message, "查股价");
        assert!(t.steps[1].timestamp.is_some());
        // agent 步 1
        let a1 = &t.steps[2];
        assert_eq!(a1.source, "agent");
        assert_eq!(a1.message, "我去查股价", "消息取审计链全文");
        assert_eq!(a1.llm_call_count, Some(1));
        let m1 = a1.metrics.as_ref().unwrap();
        assert_eq!((m1.prompt_tokens, m1.completion_tokens), (520, 80));
        let calls = a1.tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool_call_id, "t3");
        assert_eq!(calls[0].function_name, "financial_search");
        assert_eq!(calls[0].arguments, json!({"ticker": "GOOGL"}));
        let obs = a1.observation.as_ref().unwrap();
        assert_eq!(obs.results.len(), 1);
        assert_eq!(obs.results[0].source_call_id.as_deref(), Some("t3"));
        assert_eq!(
            obs.results[0].content.as_deref(),
            Some(r#"{"price":185.35}"#)
        );
        // agent 步 2
        let a2 = &t.steps[3];
        assert_eq!(a2.message, "GOOGL 现价 185.35");
        assert!(a2.tool_calls.is_none());
        assert!(a2.observation.is_none());
        assert_eq!(a2.metrics.as_ref().unwrap().prompt_tokens, 600);
        // final_metrics 与逐步 Σ 一致(验收 #8)
        assert_eq!(t.final_metrics.total_prompt_tokens, 1_120);
        assert_eq!(t.final_metrics.total_completion_tokens, 124);
        // extra 元数据
        assert_eq!(t.extra.journal_seq_range, [1, 6]);
        assert!(t.extra.source_journal.starts_with("data/sessions/42"));
        assert!(t.extra.exporter.starts_with("evo-agent atif v"));
    }

    #[test]
    fn local_react_path_falls_back_to_transcript() {
        let mut j = JFix::new();
        j.push(JE::TurnStarted {
            turn_seq: 1,
            goal: "读文件".into(),
        });
        j.push(JE::LlmCalled {
            model: "glm-5.3".into(),
            purpose: "react".into(),
            evorule_request_id: None,
            tokens: None,
            tokens_est: Some(100),
            request: 3,
            response: "读一下".into(),
        });
        j.push(JE::ToolInvoked {
            call_id: "t3".into(),
            tool: "file_read".into(),
            args_digest: "blake3:cc".into(),
            evorule_request_id: None,
        });
        j.push(JE::ToolResult {
            call_id: "t3".into(),
            status: "ok".into(),
            size_bytes: 5,
            content_digest: "blake3:dd".into(),
        });
        j.push(JE::TurnEnded {
            status: "success".into(),
            steps: 1,
            duration_ms: 100,
        });

        let mut asst = msg(1, "assistant", "读一下");
        asst.tool_calls = Some(json!([
            {"function": {"name": "file_read", "arguments": "{\"path\":\"a.txt\"}"}}
        ]));
        let transcript = vec![
            msg(0, "system", "sys"),
            msg(1, "user", "读文件"),
            asst,
            msg(2, "tool", "文件内容全文"),
        ];
        let src = AtifSources {
            session_id: "s1",
            journal: &j.lines,
            transcript: &transcript,
            audit_facts: &[],
            tool_definitions: None,
            parent_session_id: None,
            audit_anchors: None,
        };
        let t = export(src).unwrap();
        // 步:system + user + agent(transcript 无对位 assistant 回退 journal 摘要?有 asst idx1 → 对位)
        let a = t.steps.last().unwrap();
        assert_eq!(a.source, "agent");
        // tokens_est 7:3 拆分
        let m = a.metrics.as_ref().unwrap();
        assert_eq!((m.prompt_tokens, m.completion_tokens), (70, 30));
        assert_eq!(t.final_metrics.total_prompt_tokens, 70);
        assert_eq!(t.final_metrics.total_completion_tokens, 30);
        // arguments 来自 transcript tool_calls(字符串反序列化)
        let calls = a.tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].arguments, json!({"path": "a.txt"}));
        // content 来自 transcript tool 消息对位
        let obs = a.observation.as_ref().unwrap();
        assert_eq!(obs.results[0].content.as_deref(), Some("文件内容全文"));
    }

    #[test]
    fn arguments_from_transcript_engine_payload_shape() {
        // 引擎 payload 形状({"tool_name","args"})是 transcript 投影实测形态
        let tc = json!([
            {"tool_name": "file_list", "args": {"dir": "."}},
            {"tool_name": "file_write", "args": {"path": "a.txt", "content": "x"}},
        ]);
        assert_eq!(
            arguments_from_transcript(&tc, "file_list", 0),
            json!({"dir": "."})
        );
        assert_eq!(
            arguments_from_transcript(&tc, "file_write", 0),
            json!({"path": "a.txt", "content": "x"})
        );
        // OpenAI function 形状仍兼容
        let tc_openai = json!([
            {"function": {"name": "file_read", "arguments": "{\"path\":\"b.txt\"}"}}
        ]);
        assert_eq!(
            arguments_from_transcript(&tc_openai, "file_read", 0),
            json!({"path": "b.txt"})
        );
        // 未知名/序溢出规范化 {}
        assert_eq!(arguments_from_transcript(&tc, "grep_files", 0), json!({}));
        assert_eq!(arguments_from_transcript(&tc, "file_list", 1), json!({}));
    }

    #[test]
    fn intermediate_react_steps_not_poisoned_by_final_audit_response() {
        // 单 turn 多轮 react:call_external request_id 全轮相同,审计链只有
        // 收尾 IoResponse → 中间轮 message 必须走 transcript 对位,不得命中
        // 收尾全文(实测回归:4 步 message 全变成最终结论)
        let mut j = JFix::new();
        j.push(JE::TurnStarted {
            turn_seq: 1,
            goal: "g".into(),
        });
        j.push(JE::LlmCalled {
            model: "glm-5.3".into(),
            purpose: "react".into(),
            evorule_request_id: Some(1),
            tokens: tok(100, 10),
            tokens_est: None,
            request: 2,
            response: "第一轮:去列表".into(),
        });
        j.push(JE::ToolInvoked {
            call_id: "t3".into(),
            tool: "file_list".into(),
            args_digest: "blake3:aa".into(),
            evorule_request_id: None,
        });
        j.push(JE::ToolResult {
            call_id: "t3".into(),
            status: "ok".into(),
            size_bytes: 5,
            content_digest: "blake3:bb".into(),
        });
        j.push(JE::LlmCalled {
            model: "glm-5.3".into(),
            purpose: "react".into(),
            evorule_request_id: Some(1),
            tokens: tok(120, 8),
            tokens_est: None,
            request: 4,
            response: "完成".into(),
        });
        j.push(JE::TurnEnded {
            status: "success".into(),
            steps: 2,
            duration_ms: 10,
        });
        let mut asst0 = msg(1, "assistant", "第一轮:去列表");
        asst0.tool_calls = Some(json!([
            {"tool_name": "file_list", "args": {"dir": "."}}
        ]));
        let transcript = vec![
            msg(0, "system", "sys"),
            msg(2, "user", "g"),
            asst0,
            msg(4, "tool", "[]"),
            msg(5, "assistant", "完成"),
        ];
        let audit = vec![json!({
            "type": "IoResponse", "id": 9u64, "request_id": 1u64,
            "result": {"content": "完成"}, "error": null
        })];
        let src = AtifSources {
            session_id: "s",
            journal: &j.lines,
            transcript: &transcript,
            audit_facts: &audit,
            tool_definitions: None,
            parent_session_id: None,
            audit_anchors: None,
        };
        let t = export(src).unwrap();
        let agent_steps: Vec<&AtifStep> = t.steps.iter().filter(|s| s.source == "agent").collect();
        assert_eq!(agent_steps.len(), 2);
        // 中间轮:transcript 对位,非收尾全文
        assert_eq!(agent_steps[0].message, "第一轮:去列表");
        // 收尾轮:审计链全文
        assert_eq!(agent_steps[1].message, "完成");
        // 中间轮 arguments 引擎形状对位成功
        let calls = agent_steps[0].tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].arguments, json!({"dir": "."}));
    }

    #[test]
    fn sidecar_llm_and_policy_events_not_mapped_as_steps() {
        let mut j = JFix::new();
        j.push(JE::TurnStarted {
            turn_seq: 1,
            goal: "g".into(),
        });
        j.push(JE::LlmCalled {
            model: "glm-5.3".into(),
            purpose: "react".into(),
            evorule_request_id: None,
            tokens: tok(10, 5),
            tokens_est: None,
            request: 2,
            response: "r".into(),
        });
        // sidecar 调用不映射为步
        j.push(JE::LlmCalled {
            model: "MiniMax-M2.5".into(),
            purpose: "summarize".into(),
            evorule_request_id: Some(9),
            tokens: tok(51_044, 512),
            tokens_est: None,
            request: 40,
            response: "summary".into(),
        });
        j.push(JE::PolicyJudged {
            judgement_id: "j4".into(),
            verdict: "allowed".into(),
            evidence: "e".into(),
        });
        j.push(JE::TurnEnded {
            status: "success".into(),
            steps: 1,
            duration_ms: 10,
        });
        let transcript = vec![msg(0, "system", "sys"), msg(1, "user", "g")];
        let src = AtifSources {
            session_id: "s",
            journal: &j.lines,
            transcript: &transcript,
            audit_facts: &[],
            tool_definitions: None,
            parent_session_id: None,
            audit_anchors: None,
        };
        let t = export(src).unwrap();
        assert_eq!(t.steps.len(), 3, "system+user+agent,sidecar 不成步");
        let a = t.steps.last().unwrap();
        assert_eq!(
            a.extra,
            Some(json!({"policy_verdict": ["allowed"]})),
            "policy_judged 记入步 extra"
        );
        // sidecar token 不计入 final_metrics
        assert_eq!(t.final_metrics.total_prompt_tokens, 10);
    }

    #[test]
    fn compaction_emits_context_management_step() {
        let mut j = JFix::new();
        j.push(JE::TurnStarted {
            turn_seq: 1,
            goal: "g".into(),
        });
        j.push(JE::LlmCalled {
            model: "m".into(),
            purpose: "react".into(),
            evorule_request_id: None,
            tokens: tok(1, 1),
            tokens_est: None,
            request: 2,
            response: "r".into(),
        });
        j.push(JE::CompactionPerformed {
            before_est: 9_000,
            after_est: 4_000,
            cleared_call_ids: vec!["t3".into(), "t5".into()],
        });
        j.push(JE::TurnEnded {
            status: "success".into(),
            steps: 1,
            duration_ms: 10,
        });
        let src = AtifSources {
            session_id: "s",
            journal: &j.lines,
            transcript: &[msg(0, "system", "sys"), msg(1, "user", "g")],
            audit_facts: &[],
            tool_definitions: None,
            parent_session_id: None,
            audit_anchors: None,
        };
        let t = export(src).unwrap();
        // system + user + agent + compaction-system
        assert_eq!(t.steps.len(), 4);
        let cs = &t.steps[3];
        assert_eq!(cs.source, "system");
        assert_eq!(
            cs.extra,
            Some(json!({"context_management": {"type": "compaction", "boundary": "replace"}}))
        );
        let obs = cs.observation.as_ref().unwrap();
        assert_eq!(
            obs.results[0].content.as_deref(),
            Some("cleared 2 tool results: [t3, t5]")
        );
    }

    #[test]
    fn i2_scan_report_emits_context_management_step() {
        let mut j = JFix::new();
        j.push(JE::TurnStarted {
            turn_seq: 1,
            goal: "g".into(),
        });
        j.push(JE::I2ScanReport {
            round: 1,
            conflicts: vec![crate::agent::context_inspector::I2ConflictRecord {
                kind: "deny_vs_capability".into(),
                token: "web_search".into(),
                section_a: "(基底段)".into(),
                section_b: "【能力边界声明】".into(),
                excerpt_a: "禁止使用 web_search".into(),
                excerpt_b: "可用工具:web_search".into(),
                semantic_verdict: None,
            }],
        });
        j.push(JE::TurnEnded {
            status: "success".into(),
            steps: 1,
            duration_ms: 10,
        });
        let src = AtifSources {
            session_id: "s",
            journal: &j.lines,
            transcript: &[msg(0, "system", "sys"), msg(1, "user", "g")],
            audit_facts: &[],
            tool_definitions: None,
            parent_session_id: None,
            audit_anchors: None,
        };
        let t = export(src).unwrap();
        // system + user + i2_scan_report-system(turn 内无 LLM 步,agent 步不产生)
        assert_eq!(t.steps.len(), 3);
        let cs = &t.steps[2];
        assert_eq!(cs.source, "system");
        assert_eq!(
            cs.extra,
            Some(json!({
                "context_management": {
                    "type": "i2_scan_report",
                    "round": 1,
                    "conflicts": [{
                        "kind": "deny_vs_capability",
                        "token": "web_search",
                        "section_a": "(基底段)",
                        "section_b": "【能力边界声明】",
                        "excerpt_a": "禁止使用 web_search",
                        "excerpt_b": "可用工具:web_search"
                    }]
                }
            }))
        );
    }

    #[test]
    fn wire_rendered_emits_context_management_step() {
        let mut j = JFix::new();
        j.push(JE::TurnStarted {
            turn_seq: 1,
            goal: "g".into(),
        });
        j.push(JE::WireRendered {
            round: 1,
            wire_len: 1234,
            content_hash: "blake3:deadbeef".into(),
            full_text: "sys".into(),
        });
        j.push(JE::TurnEnded {
            status: "success".into(),
            steps: 1,
            duration_ms: 10,
        });
        let src = AtifSources {
            session_id: "s",
            journal: &j.lines,
            transcript: &[msg(0, "system", "sys"), msg(1, "user", "g")],
            audit_facts: &[],
            tool_definitions: None,
            parent_session_id: None,
            audit_anchors: None,
        };
        let t = export(src).unwrap();
        // system + user + wire_rendered-system(turn 内无 LLM 步,agent 步不产生)
        assert_eq!(t.steps.len(), 3);
        let cs = &t.steps[2];
        assert_eq!(cs.source, "system");
        assert_eq!(
            cs.extra,
            Some(json!({
                "context_management": {
                    "type": "wire_rendered",
                    "round": 1,
                    "wire_len": 1234,
                    "content_hash": "blake3:deadbeef"
                }
            }))
        );
        let obs = cs.observation.as_ref().unwrap();
        assert_eq!(
            obs.results[0].content.as_deref(),
            Some("round=1 wire_len=1234 hash=blake3:deadbeef")
        );
    }

    #[test]
    fn unpaired_tool_invoked_lists_call_without_observation() {
        let mut j = JFix::new();
        j.push(JE::TurnStarted {
            turn_seq: 1,
            goal: "g".into(),
        });
        j.push(JE::LlmCalled {
            model: "m".into(),
            purpose: "react".into(),
            evorule_request_id: None,
            tokens: tok(1, 1),
            tokens_est: None,
            request: 2,
            response: "r".into(),
        });
        // 调用后无 tool_result(崩溃/截断场景)
        j.push(JE::ToolInvoked {
            call_id: "t3".into(),
            tool: "shell".into(),
            args_digest: "blake3:ee".into(),
            evorule_request_id: Some(11),
        });
        let audit = vec![json!({
            "type": "IoRequest", "id": 11u64, "cause": 1u64,
            "io_type": "call_service",
            "params": {"tool_name": "shell", "args": {"cmd": "ls"}}
        })];
        let transcript = vec![msg(0, "system", "sys"), msg(1, "user", "g")];
        let src = AtifSources {
            session_id: "s",
            journal: &j.lines,
            transcript: &transcript,
            audit_facts: &audit,
            tool_definitions: None,
            parent_session_id: None,
            audit_anchors: None,
        };
        let t = export(src).unwrap();
        let a = t.steps.last().unwrap();
        let calls = a.tool_calls.as_ref().unwrap();
        assert_eq!(
            calls[0].arguments,
            json!({"cmd": "ls"}),
            "arguments 取审计链全文"
        );
        let obs = a.observation.as_ref().unwrap();
        assert_eq!(obs.results.len(), 1);
        assert_eq!(
            obs.results[0].content, None,
            "无 IoResponse 时不虚构 content"
        );
    }

    #[test]
    fn session_crashed_truncates_stream() {
        let mut j = JFix::new();
        j.push(JE::TurnStarted {
            turn_seq: 1,
            goal: "g".into(),
        });
        j.push(JE::LlmCalled {
            model: "m".into(),
            purpose: "react".into(),
            evorule_request_id: None,
            tokens: tok(1, 1),
            tokens_est: None,
            request: 2,
            response: "r".into(),
        });
        j.push(JE::TurnEnded {
            status: "success".into(),
            steps: 1,
            duration_ms: 1,
        });
        j.push(JE::SessionCrashed {
            reason: "stream error".into(),
        });
        // crash 之后的事件不参与导出
        j.push(JE::TurnStarted {
            turn_seq: 2,
            goal: "later".into(),
        });
        let transcript = vec![msg(0, "system", "sys"), msg(1, "user", "g")];
        let src = AtifSources {
            session_id: "s",
            journal: &j.lines,
            transcript: &transcript,
            audit_facts: &[],
            tool_definitions: None,
            parent_session_id: None,
            audit_anchors: None,
        };
        let t = export(src).unwrap();
        assert_eq!(t.steps.len(), 3, "crash 截断:后续 turn_started 不入步");
        assert!(!t.steps.iter().any(|s| s.message == "later"));
    }

    #[test]
    fn session_resumed_exports_both_segments_with_copied_marks() {
        // 续接轨迹:crash 断点不截断,恢复点前对话步整体标 is_copied_context
        //(重建历史,SFT 过滤面;system 步不标),续接段照常映射,
        // root.extra 显式标记续接
        let mut j = JFix::new();
        // 崩溃前:turn 1 完整一轮
        j.push(JE::TurnStarted {
            turn_seq: 1,
            goal: "g".into(),
        });
        j.push(JE::LlmCalled {
            model: "m".into(),
            purpose: "react".into(),
            evorule_request_id: None,
            tokens: tok(1, 1),
            tokens_est: None,
            request: 2,
            response: "a1".into(),
        });
        j.push(JE::TurnEnded {
            status: "success".into(),
            steps: 1,
            duration_ms: 1,
        });
        j.push(JE::SessionCrashed {
            reason: "unclean_tail".into(),
        });
        j.push(JE::SessionResumed {
            replay_seq: 4,
            rebuilt: vec!["messages".into()],
        });
        // 续接段:turn 2
        j.push(JE::TurnStarted {
            turn_seq: 2,
            goal: "g2".into(),
        });
        j.push(JE::LlmCalled {
            model: "m".into(),
            purpose: "react".into(),
            evorule_request_id: None,
            tokens: tok(1, 1),
            tokens_est: None,
            request: 4,
            response: "a2".into(),
        });
        j.push(JE::TurnEnded {
            status: "success".into(),
            steps: 1,
            duration_ms: 1,
        });
        let transcript = vec![
            msg(0, "system", "sys"),
            msg(1, "user", "g"),
            msg(2, "assistant", "a1"),
            msg(3, "user", "g2"),
            msg(4, "assistant", "a2"),
        ];
        let src = AtifSources {
            session_id: "s",
            journal: &j.lines,
            transcript: &transcript,
            audit_facts: &[],
            tool_definitions: None,
            parent_session_id: None,
            audit_anchors: None,
        };
        let t = export(src).unwrap();
        assert_eq!(t.steps.len(), 5, "两段都入步:system+崩溃前 2 步+续接 2 步");
        assert!(t.extra.resumed_after_crash, "root.extra 续接标记");
        let copied = |s: &AtifStep| {
            s.extra
                .as_ref()
                .and_then(|e| e.get("is_copied_context"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        };
        assert!(!copied(&t.steps[0]), "system 步不标(持久脚手架)");
        assert!(copied(&t.steps[1]), "崩溃前 user 步=重建历史");
        assert!(copied(&t.steps[2]), "崩溃前 agent 步=重建历史");
        assert!(!copied(&t.steps[3]), "续接段 user 步=新鲜产出");
        assert!(!copied(&t.steps[4]), "续接段 agent 步=新鲜产出");
        // 序数对齐:续接轮消费 transcript 对位消息(重建历史不重复入 transcript)
        assert_eq!(t.steps[3].message, "g2");
        assert_eq!(t.steps[4].message, "a2");
    }

    #[test]
    fn tool_retried_event_maps_to_no_step() {
        // 工具重试观测事件不映射步(观测面口径),导出零影响
        let mut j = JFix::new();
        j.push(JE::TurnStarted {
            turn_seq: 1,
            goal: "g".into(),
        });
        j.push(JE::LlmCalled {
            model: "m".into(),
            purpose: "react".into(),
            evorule_request_id: None,
            tokens: tok(1, 1),
            tokens_est: None,
            request: 2,
            response: "r".into(),
        });
        j.push(JE::ToolRetried {
            tool: "grep_files".into(),
            attempt: 1,
            transient: true,
        });
        j.push(JE::TurnEnded {
            status: "success".into(),
            steps: 1,
            duration_ms: 1,
        });
        let transcript = vec![msg(0, "system", "sys"), msg(1, "user", "g")];
        let src = AtifSources {
            session_id: "s",
            journal: &j.lines,
            transcript: &transcript,
            audit_facts: &[],
            tool_definitions: None,
            parent_session_id: None,
            audit_anchors: None,
        };
        let t = export(src).unwrap();
        assert_eq!(t.steps.len(), 3, "system+user+agent 三步,重试事件不产生步");
    }

    #[test]
    fn approval_events_attach_to_tool_call_extra() {
        let mut j = JFix::new();
        j.push(JE::TurnStarted {
            turn_seq: 1,
            goal: "g".into(),
        });
        j.push(JE::LlmCalled {
            model: "m".into(),
            purpose: "react".into(),
            evorule_request_id: None,
            tokens: tok(1, 1),
            tokens_est: None,
            request: 2,
            response: "r".into(),
        });
        j.push(JE::ToolInvoked {
            call_id: "t3".into(),
            tool: "file_write".into(),
            args_digest: "blake3:ff".into(),
            evorule_request_id: None,
        });
        j.push(JE::ApprovalRequested {
            approval_id: "p1".into(),
            tool: "file_write".into(),
            payload: "write".into(),
        });
        j.push(JE::ApprovalResolved {
            approval_id: "p1".into(),
            decision: "approved".into(),
        });
        j.push(JE::ToolResult {
            call_id: "t3".into(),
            status: "ok".into(),
            size_bytes: 1,
            content_digest: "blake3:00".into(),
        });
        j.push(JE::TurnEnded {
            status: "success".into(),
            steps: 1,
            duration_ms: 1,
        });
        let transcript = vec![
            msg(0, "system", "sys"),
            msg(1, "user", "g"),
            msg(2, "tool", "done"),
        ];
        let src = AtifSources {
            session_id: "s",
            journal: &j.lines,
            transcript: &transcript,
            audit_facts: &[],
            tool_definitions: None,
            parent_session_id: None,
            audit_anchors: None,
        };
        let t = export(src).unwrap();
        let a = t.steps.last().unwrap();
        let calls = a.tool_calls.as_ref().unwrap();
        assert_eq!(
            calls[0].extra,
            Some(json!({"approval_id": "p1", "decision": "approved"}))
        );
    }

    #[test]
    // ===== V-1:锚点背书+脱敏器 =====

    #[test]
    fn v1_anchor_endorsement_extracted() {
        // server /anchors 响应 → extra.audit_anchor + audit_chain_head
        let anchors = serde_json::json!({
            "count": 2,
            "anchors": [
                {"seq": 0, "fact_range": {"lo": 0, "hi": 5}, "chain_head": "aa",
                 "key_id": "k1", "engine_id": "eng", "anchor_hash": "h0", "signature": "s0"},
                {"seq": 1, "fact_range": {"lo": 5, "hi": 7}, "chain_head": "bb",
                 "key_id": "k1", "engine_id": "eng", "anchor_hash": "h1", "signature": "s1"}
            ]
        });
        let (head, endo) = anchor_endorsement_of(Some(&anchors));
        assert_eq!(head.as_deref(), Some("bb"), "链头必须取末锚点");
        let e = endo.expect("末锚点段必须在");
        assert_eq!(e.seq, 1);
        assert_eq!(e.fact_range, [5, 7]);
        assert_eq!(e.signature, "s1");
        assert_eq!(e.anchor_format, "evorule-anchor/1");
    }

    #[test]
    fn v1_anchor_none_when_absent() {
        assert!(anchor_endorsement_of(None).0.is_none());
        assert!(anchor_endorsement_of(None).1.is_none());
        let empty = serde_json::json!({"count": 0, "anchors": []});
        assert!(anchor_endorsement_of(Some(&empty)).1.is_none());
    }

    #[test]
    fn v1_sanitize_catches_secrets() {
        let mut hits = std::collections::BTreeMap::new();
        let (out, n) = sanitize_text(
            "call with sk-abcdefghijklmnopqrstuvwx and Bearer abcdef0123456789abcdef",
            &mut hits,
        );
        assert!(out.contains("[REDACTED:sk-token]"), "out={out}");
        assert!(out.contains("[REDACTED:bearer]"), "out={out}");
        assert_eq!(n, 2);
        assert_eq!(hits.get("sk-token"), Some(&1));
    }

    #[test]
    fn v1_sanitize_clean_text_untouched() {
        let mut hits = std::collections::BTreeMap::new();
        let (out, n) = sanitize_text("普通正文,无秘密", &mut hits);
        assert_eq!(out, "普通正文,无秘密");
        assert_eq!(n, 0);
        assert!(hits.is_empty());
    }

    #[test]
    fn v1_export_sanitizes_end_to_end() {
        // 端到端:transcript 含 sk- 秘密 → 导出全文本零漏出 + sanitized 落账
        let mut j = JFix::new();
        j.push(JE::TurnStarted { turn_seq: 1, goal: "g".into() });
        j.push(JE::TurnEnded { status: "success".into(), steps: 1, duration_ms: 1 });
        let transcript = vec![
            msg(0, "system", "sys"),
            msg(1, "user", "key is sk-abcdefghijklmnopqrstuvwx ok?"),
        ];
        let src = AtifSources {
            session_id: "s",
            journal: &j.lines,
            transcript: &transcript,
            audit_facts: &[],
            tool_definitions: None,
            parent_session_id: None,
            audit_anchors: None,
        };
        let t = export(src).expect("导出须成功");
        let json = serde_json::to_string(&t).unwrap();
        assert!(!json.contains("sk-abcdefghijklmnopqrstuvwx"), "秘密漏出!");
        assert!(json.contains("[REDACTED:sk-token]"));
        let sani = t.extra.sanitized.expect("脱敏报告须在");
        assert_eq!(sani.total_replaced, 1);
    }

    #[test]
    fn v1_sanitize_seed_env_form() {
        let mut hits = std::collections::BTreeMap::new();
        let (out, n) = sanitize_text(
            "EVORULE_ANCHOR_SEED=abababababababababababababababababababababababababababababababab",
            &mut hits,
        );
        assert_eq!(n, 1);
        assert!(out.contains("[REDACTED:"), "out={out}");
    }

    fn export_is_byte_idempotent() {
        // 验收 #7:同 session 重导两次逐字节一致(exported_at 由 journal 尾 ts 派生)
        let mut j = JFix::new();
        j.push(JE::TurnStarted {
            turn_seq: 1,
            goal: "g".into(),
        });
        j.push(JE::LlmCalled {
            model: "m".into(),
            purpose: "react".into(),
            evorule_request_id: Some(3),
            tokens: tok(5, 5),
            tokens_est: None,
            request: 2,
            response: "r".into(),
        });
        j.push(JE::TurnEnded {
            status: "success".into(),
            steps: 1,
            duration_ms: 1,
        });
        let audit = vec![json!({
            "type": "IoResponse", "id": 4u64, "request_id": 3u64,
            "result": {"content": "hi"}, "error": null
        })];
        let transcript = vec![msg(0, "system", "sys"), msg(1, "user", "g")];
        let mk = || AtifSources {
            session_id: "s",
            journal: &j.lines,
            transcript: &transcript,
            audit_facts: &audit,
            tool_definitions: None,
            parent_session_id: None,
            audit_anchors: None,
        };
        let a = serde_json::to_string(&export(mk()).unwrap()).unwrap();
        let b = serde_json::to_string(&export(mk()).unwrap()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn error_response_content_uses_error_text() {
        let audit = vec![json!({
            "type": "IoResponse", "id": 2u64, "request_id": 1u64,
            "result": null, "error": "tool exploded"
        })];
        let idx = AuditIndex::build(&audit);
        assert_eq!(
            idx.response_content(1).as_deref(),
            Some("tool exploded"),
            "error=Some 时 content=错误描述(映射表 §五)"
        );
    }

    #[test]
    fn name_ordinal_and_args_normalization() {
        // 同名多次调用按序对位;非 object 参数规范化 {}
        let tc = json!([
            {"function": {"name": "shell", "arguments": "{\"cmd\":\"a\"}"}},
            {"function": {"name": "shell", "arguments": "{\"cmd\":\"b\"}"}},
            {"function": {"name": "other", "arguments": "not-json"}},
            {"function": {"name": "shell", "arguments": null}}
        ]);
        assert_eq!(
            arguments_from_transcript(&tc, "shell", 1),
            json!({"cmd": "b"})
        );
        assert_eq!(
            arguments_from_transcript(&tc, "other", 0),
            json!({}),
            "非法 JSON 回退空对象"
        );
        assert_eq!(
            arguments_from_transcript(&tc, "shell", 2),
            json!({}),
            "arguments null 回退空对象"
        );
        assert_eq!(arguments_from_transcript(&tc, "missing", 0), json!({}));
        // 对象形态直通
        let tc2 = json!([{"function": {"name": "x", "arguments": {"k": 1}}}]);
        assert_eq!(arguments_from_transcript(&tc2, "x", 0), json!({"k": 1}));
    }
    #[test]
    fn delegate_spawned_not_mapped_as_step() {
        // 委托锚事件为元数据:主轨迹不产出步(delegate 仍呈现为普通工具
        // 调用,映射口径 v1 不变);无锚事件时步序列不受影响
        let mut j = JFix::new();
        j.push(JE::TurnStarted {
            turn_seq: 1,
            goal: "delegate a task".into(),
        });
        j.push(JE::ToolInvoked {
            call_id: "t1".into(),
            tool: "delegate".into(),
            args_digest: "blake3:aa".into(),
            evorule_request_id: None,
        });
        j.push(JE::DelegateSpawned {
            child_session_id: "child-7".into(),
            agent_type: "researcher".into(),
            depth: 1,
            task_digest: "blake3:bb".into(),
        });
        j.push(JE::ToolResult {
            call_id: "t1".into(),
            status: "ok".into(),
            size_bytes: 4,
            content_digest: "blake3:cc".into(),
        });
        j.push(JE::TurnEnded {
            status: "success".into(),
            steps: 1,
            duration_ms: 10,
        });
        let transcript = vec![msg(0, "system", "sys"), msg(1, "user", "delegate a task")];
        let mk = || AtifSources {
            session_id: "s",
            journal: &j.lines,
            transcript: &transcript,
            audit_facts: &[],
            tool_definitions: None,
            parent_session_id: None,
            audit_anchors: None,
        };
        let t = serde_json::to_value(export(mk()).unwrap()).unwrap();
        let serialized = serde_json::to_string(&t).unwrap();
        assert!(
            !serialized.contains("child-7"),
            "委托锚不进主轨迹任何字段(含 extra 之外的步/观察面)"
        );
        // 对照:同一事件流去掉锚事件后步数不变(锚=零步)
        let mut j2 = JFix::new();
        for l in &j.lines {
            if !matches!(l.event, JE::DelegateSpawned { .. }) {
                j2.push(l.event.clone());
            }
        }
        let mk2 = || AtifSources {
            session_id: "s",
            journal: &j2.lines,
            transcript: &transcript,
            audit_facts: &[],
            tool_definitions: None,
            parent_session_id: None,
            audit_anchors: None,
        };
        let a = serde_json::to_value(export(mk()).unwrap()).unwrap();
        let b = serde_json::to_value(export(mk2()).unwrap()).unwrap();
        assert_eq!(
            a["steps"].as_array().unwrap().len(),
            b["steps"].as_array().unwrap().len(),
            "锚事件=零步,步序列与无锚事件流一致"
        );
    }

    #[test]
    fn parent_session_id_extra_field_semantics() {
        // 子轨迹链路标注:Some → root.extra 落值;None → 字段缺省
        let mut j = JFix::new();
        j.push(JE::TurnStarted {
            turn_seq: 1,
            goal: "g".into(),
        });
        j.push(JE::TurnEnded {
            status: "success".into(),
            steps: 0,
            duration_ms: 1,
        });
        let transcript = vec![msg(0, "system", "sys"), msg(1, "user", "g")];
        let mk = |parent: Option<String>| AtifSources {
            session_id: "child-7",
            journal: &j.lines,
            transcript: &transcript,
            audit_facts: &[],
            tool_definitions: None,
            parent_session_id: parent,
            audit_anchors: None,
        };
        let with_parent =
            serde_json::to_value(export(mk(Some("parent-3".into()))).unwrap()).unwrap();
        assert_eq!(
            with_parent["extra"]["parent_session_id"], "parent-3",
            "链路标注落 extra 扩展位"
        );
        let without = serde_json::to_value(export(mk(None)).unwrap()).unwrap();
        assert!(
            without["extra"].get("parent_session_id").is_none(),
            "无链路=字段缺省(ATIF Optional 口径,不冒充空值)"
        );
    }
}
