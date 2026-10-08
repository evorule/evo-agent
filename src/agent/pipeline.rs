// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! ToolExecutionPipeline —— 工具执行唯一管道（工具面统一架构「一管道」，§3.2）
//!
//! 所有 agent 面工具执行必须经过的唯一函数。管道阶段是**结构性在场**的——
//! 不存在「没挂门禁的调用点」。八阶段语义：
//!
//! | 阶段 | 语义 | PR-2 状态 |
//! |---|---|---|
//! | ① 查表 | manifest 必须存在（静态表 + handler 注册的动态源） | 显式实装 |
//! | ② 聚焦过滤 | tool_name ∈ focus_snapshot（装配期允许面快照） | 显式实装 |
//! | ③ 意图信号 | 按 adjudication_class 提交 pending_tool_intent | 复用既有裁决通道（原 execute_tool_call 内联逻辑逐字迁入） |
//! | ④ 规则裁决 | version 判据（放行/拦截，fail-closed 语义不变） | 同上 |
//! | ⑤ 分级审批 | 按 approval_policy：AlwaysDeny 拒 / 其余走既有 proposal 协议 | 显式实装（决策端两调协议仍归 runner，PR-4 统一） |
//! | ⑥ 机制沙箱 | fs_safety/net_guard 路径围栏 | 占位阶段——守卫仍在工具实现内联（双层分工不变，实现可在 handler 内二次防御） |
//! | ⑦ 执行 | 注入执行器 + 真实结局采集（metrics/轨迹） | 显式实装 |
//! | ⑧ 结局落账 | LedgerRecord 全程账 + journal 写失败显式化（fail-visible） | 显式实装（链侧 tool_trace 指令接线 = 信号契约批次） |
//!
//! 行为等价纪律（§七.2 硬验收）：本模块是**结构重构而非策略变更**——治理拦截
//! 的两态 JSON 文案、裁决 fail-closed 语义、轨迹/指标采集点均与原 execute_tool_call
//! 逐字对齐；runner_tests 既有基线测试（放行/拦截/审批）不改一行保持全绿。
//!
//! PR-3 已收口：G13 并行预执行=管道并行实例（ParallelPreflight 入口）、
//! 缓存收口（precomputed=已过闸执行产物，命中调用①-⑤⑧照常仅⑦免重执行）、
//! file_api 读面 manifest 查表（调用侧）。PR-8（delegate 统一装配）已接线：
//! 子代理路径入口类记 Delegate（构建期标记）、聚焦快照=装配面（注册面本身，
//! LLM 契约面与允许面同源）。仍范围外：两面身份与 HumanGate（PR-4）、
//! 链侧 tool_trace 统一指令（信号契约批次）。

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;

use crate::agent::adjudicator::AdjudicationChannel;
use crate::agent::definition::CapabilityBoundary;
use crate::agent::journal::{JournalError, JournalWriter};
use crate::agent::runner::{
    intent_signal, resolve_target_scope, resolve_tool_intent, tool_intent_signal, AgentError,
};
use crate::agent::tool_manifest::{ApprovalPolicy, ToolManifest};
use crate::agent::tool_trace::ToolTraceCollector;
use crate::api::metrics::Metrics;

// =============================================================================
// 请求/依赖（管道输入面）
// =============================================================================

/// 调用方入口（PR-2 最小集 + PR-3 并行预执行入口；完整 CallerContext
/// 随 PR-4 两面扩展——只增不改）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineEntry {
    /// runner 主循环工具调用
    React,
    /// 审批批准后重执行
    ApprovalReexec,
    /// G13 并行预执行（PR-3 收口：预执行=管道并行实例，产物=已过闸结果）
    ParallelPreflight,
    /// 子代理 delegate 路径（delegate 统一装配批接线：子 runner 构建期经
    /// `with_delegate_pipeline_entry` 标记，账面 caller.entry 落账）
    Delegate,
}

/// 调用方上下文（设计档 §3.2：agent def id / serve|cli|delegate / session id；
/// PR-2 先落 session_id 与入口类，其余字段随后续 PR 扩展——只增不改）
#[derive(Debug, Clone, Serialize)]
pub struct CallerContext {
    /// 调用入口类
    pub entry: PipelineEntry,
    /// 主会话 id（裁决审计关联用）
    pub session_id: Option<String>,
}

/// 聚焦快照（本次允许面：def.tools ∩ 开关 ∩ surface 的已装配集合）
///
/// 主路径快照 = 装配期注册面 ∪ 静态表（行为等价口径：②不产生新拒绝；
/// 收窄为注册面严格子集随装配收口批次落地，B2 断言测试盯守）。G13 并行
/// 预执行实例与主路径共用同一快照构造（run_pipeline 单点）。快照本身可
/// 序列化，随调用落账（聚焦范围=可审计对象）。
#[derive(Debug, Clone, Serialize)]
pub struct FocusSnapshot {
    allowed: BTreeSet<String>,
}

impl FocusSnapshot {
    /// 从允许工具名集合构建快照
    pub fn from_names<I>(names: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        Self {
            allowed: names.into_iter().map(Into::into).collect(),
        }
    }

    /// 工具是否在允许面内
    pub fn allows(&self, tool_name: &str) -> bool {
        self.allowed.contains(tool_name)
    }

    /// 允许面大小
    pub fn len(&self) -> usize {
        self.allowed.len()
    }

    /// 是否为空面
    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty()
    }
}

/// 管道执行请求（设计档 §3.2 PipelineRequest）
pub struct PipelineRequest<'a> {
    /// 工具名（LLM 面稳定标识）
    pub tool_name: &'a str,
    /// 工具参数（JSON）
    pub args: &'a Value,
    /// 调用方上下文
    pub caller: CallerContext,
    /// 本次允许面快照（装配期已过滤集合）
    pub focus: &'a FocusSnapshot,
    /// 已过闸执行产物（G13 缓存收口面，PR-3）：预执行管道实例的落账结果。
    /// Some = 命中缓存——①-⑤与⑧随本次调用照常（意图/裁决/账面必须逐调用
    /// 在场，P0-3/A1 关闭判据），仅⑦免重执行直接采信缓存值（缓存键=「已过
    /// 门禁的证据」）。None = 常规执行。
    pub precomputed: Option<Value>,
    /// 预供给审批决策（⑤收编 PR-4）：流式/重执行路径的决策端在外部编排层
    /// 已完成决策（callback 结论），随 ApprovalReexec 入口带入——⑤评估出
    /// proposal 时直接消费此决策（批准→⑦ / 拒绝→显式拒绝回喂），不再询问。
    /// None = 无预供给：⑤评估出 proposal 时暂停（ApprovalPending）。
    pub approval_preset: Option<crate::agent::approval::ApprovalDecision>,
}

/// 阶段⑦执行器抽象（runner 注入 call_service 通路；测试注入桩。
/// Sync 超界：PipelineDeps 需跨 await 持有（流式路径 Send 要求））
pub trait PipelineExecutor: Sync {
    /// 执行工具调用（step_timeout 语义由实现持有，管道不另设超时）
    fn execute_tool(
        &self,
        tool_name: &str,
        args: &Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, AgentError>> + Send + '_>>;
}

/// 阶段⑧账面写入口（journal 注入缝：§七.6「journal 写失败注入 → 显式报错」
/// 验收测试经此替换失败桩；生产实现 = JournalWriter）
pub trait PolicyJudgedSink {
    /// 记录一次意图裁决输出（verdict = allowed/blocked）
    fn policy_judged(&self, verdict: &str, evidence: &str) -> Result<u64, JournalError>;
    /// 记录一次工具执行重试（attempt 从 1 计；幂等类工具瞬态故障重试面）
    fn tool_retried(&self, tool: &str, attempt: u64, transient: bool) -> Result<u64, JournalError>;
}

impl PolicyJudgedSink for JournalWriter {
    fn policy_judged(&self, verdict: &str, evidence: &str) -> Result<u64, JournalError> {
        JournalWriter::policy_judged(self, verdict, evidence)
    }

    fn tool_retried(&self, tool: &str, attempt: u64, transient: bool) -> Result<u64, JournalError> {
        JournalWriter::tool_retried(self, tool, attempt, transient)
    }
}

/// 阶段⑤评估单源函数形态(类型别名化解内联复杂类型)
pub type ProposalEvalFn<'a> = &'a (dyn Fn(&str, &Value) -> Option<Value> + Sync);

/// 管道逐调用依赖（runner 装配件借用注入——裁决通道/账面/指标均为 runner
/// 持有的既有实例，管道自身无状态，避免装配环）
pub struct PipelineDeps<'a> {
    /// 阶段⑦执行器
    pub executor: &'a dyn PipelineExecutor,
    /// 阶段③④裁决通道（与 runner 同源；tokio Mutex 语义保持）
    pub adjudicator: &'a tokio::sync::Mutex<AdjudicationChannel>,
    /// 阶段①查表访问器（静态表 + handler 动态注册条目）
    pub manifest_of: &'a (dyn Fn(&str) -> Option<ToolManifest> + Sync),
    /// 阶段⑤审批评估单源（PR-4 收编：candidate proposal 由注册执行器实例
    /// 的 evaluate_proposal 钩子求值——评估与执行同源，冒名注册以实际
    /// 执行器为准）。None = 管道测试桩场景（评估缺省直通）。
    pub proposal_of: Option<ProposalEvalFn<'a>>,
    /// 阶段⑧账面（None = 不启用——CLI/子代理现状语义保持）
    pub journal: Option<&'a (dyn PolicyJudgedSink + Sync)>,
    /// 能力边界（意图快筛判据）
    pub boundary: Option<&'a CapabilityBoundary>,
    /// 工具轨迹采集器（P1 既有采集点，采集语义不变）
    pub traces: Option<&'a std::sync::Mutex<ToolTraceCollector>>,
    /// 指标桥（G17 既有观测点）
    pub metrics: Option<&'a Metrics>,
    /// 幂等类工具瞬态故障重试的回退基数（第 n 次重试等待 = 基数 × n；
    /// 生产装配 = 1s 与 LLM 传输层同量级，测试注入极短值）
    pub retry_backoff: std::time::Duration,
}

// =============================================================================
// 结局/账面（管道输出面）
// =============================================================================

/// 显式拒绝阶段（设计档 §3.2 Denial{stage, reason}）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialStage {
    /// ①查表失败：无 manifest
    NoManifest,
    /// ②聚焦过滤：不在允许面
    OutOfFocus,
    /// ③④治理拦截（规则层裁决不放行）
    Governance,
    /// ⑤分级审批拒绝（AlwaysDeny / 决策端拒绝）
    ApprovalDenied,
    /// ⑤分级审批暂停（candidate proposal 已评估、等待决策端结论——
    /// 决策在外部编排：非流式=runner 决策端，流式=事件循环审批窗；
    /// 并行预执行实例经此形态表达「candidate 不入缓存」G13 语义）
    ApprovalPending,
    /// ③④裁决通道故障（fail-closed 语义：通道错误显式上抛）
    Channel,
    /// ⑧账面写失败（fail-visible：账写不掉=错误显式化）
    Ledger,
}

/// 显式拒绝（无静默路径：每条 Denial 带阶段与原因）
#[derive(Debug, Clone, Serialize)]
pub struct PipelineDenial {
    /// 拒绝发生的管道阶段
    pub stage: DenialStage,
    /// 拒绝原因（显式文本，回喂/审计两用）
    pub reason: String,
    /// LLM 可见面负载（治理拦截=现状两态 blocked JSON，原样回喂；
    /// 其余拒绝 None——由调用方决定错误形态）
    pub llm_payload: Option<Value>,
}

impl PipelineDenial {
    fn new(stage: DenialStage, reason: impl Into<String>) -> Self {
        Self {
            stage,
            reason: reason.into(),
            llm_payload: None,
        }
    }
}

/// 裁决事实（账面：意图信号+裁决结论同条记录）
#[derive(Debug, Clone, Serialize)]
pub struct AdjudicationFact {
    /// 通道标识（target_scope_r1 = M5-c file 面；tool_intent_p2 = 治理级工具）
    pub channel: &'static str,
    /// 规则层裁决结论（true=放行）
    pub allowed: bool,
    /// 证据（scope / intent 序列化摘要）
    pub evidence: String,
}

/// 执行事实（账面：真实结局）
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionFact {
    /// 执行是否成功（false = ⑦已发生的真实失败，区别于拒绝）
    pub ok: bool,
    /// 执行耗时（毫秒）
    pub duration_ms: u64,
}

/// 全程账（设计档 §3.2 LedgerRecord：各阶段事实，§七.6 审计完整性断言的数据面——
/// 意图信号/裁决结论/审批策略/执行结局四类事实俱在）
#[derive(Debug, Clone, Serialize)]
pub struct LedgerRecord {
    /// 工具名
    pub tool_name: String,
    /// 调用方上下文（入口类 + 会话 id）
    pub caller: CallerContext,
    /// ①查表
    pub manifest_found: bool,
    /// ②聚焦
    pub in_focus: bool,
    /// ⑤审批策略（查表所得，含未触发场景——策略在场即可审计）
    pub approval_policy: Option<ApprovalPolicy>,
    /// ⑤审批决策记录（PR-4 收编：决策端结论入统一账面——
    /// 形态与流式审批留痕同构 proposal_id/tool/decision/approver/verified/
    /// decided_at/reason；未触发审批 = None）
    pub approval: Option<Value>,
    /// ③④裁决事实（按提交顺序）
    pub adjudication: Vec<AdjudicationFact>,
    /// ⑦执行结局
    pub execution: Option<ExecutionFact>,
    /// 拒绝（无静默路径）
    pub denial: Option<DenialStage>,
}

impl LedgerRecord {
    fn new(tool_name: &str, caller: CallerContext) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            caller,
            manifest_found: false,
            in_focus: false,
            approval_policy: None,
            approval: None,
            adjudication: Vec::new(),
            execution: None,
            denial: None,
        }
    }
}

/// 管道失败（执行失败 ≠ 拒绝：拒绝=⑦之前的显式不执行；执行失败=⑦已发生
/// 的真实结局——账面 execution.ok=false 与失败原因并行在案）
#[derive(Debug)]
pub enum PipelineFailure {
    /// 显式拒绝（各阶段 Denial）
    Denial(PipelineDenial),
    /// 阶段⑦执行错误（原样透传——错误显式回喂语义与原实现一致）
    Execution(AgentError),
}

/// 管道结局（设计档 §3.2 PipelineOutcome）
pub struct PipelineOutcome {
    /// Ok = 放行并执行完毕；Err = 显式拒绝或执行错误（无静默路径）
    pub result: Result<Value, PipelineFailure>,
    /// 全程账
    pub ledger: LedgerRecord,
}

// =============================================================================
// 管道本体
// =============================================================================

/// 工具执行管道（无状态编排器；依赖经 [`PipelineDeps`] 逐调用注入）
#[derive(Debug, Default, Clone, Copy)]
pub struct ToolExecutionPipeline;

impl ToolExecutionPipeline {
    /// 八阶段执行（PR-2 骨架：①②⑤⑦⑧显式实装，③④复用既有裁决通道，⑥占位）
    pub async fn execute(
        &self,
        req: PipelineRequest<'_>,
        deps: PipelineDeps<'_>,
    ) -> PipelineOutcome {
        let mut ledger = LedgerRecord::new(req.tool_name, req.caller.clone());

        // ── ① 查表：manifest 必须存在（无 manifest 拒绝注册的镜像面：
        // 无 manifest 的调用同样拒绝——显式拒绝回喂，不静默降级） ──
        let Some(manifest) = (deps.manifest_of)(req.tool_name) else {
            ledger.denial = Some(DenialStage::NoManifest);
            return PipelineOutcome {
                result: Err(PipelineFailure::Denial(PipelineDenial::new(
                    DenialStage::NoManifest,
                    format!("tool not found: {}", req.tool_name),
                ))),
                ledger,
            };
        };
        ledger.manifest_found = true;
        ledger.approval_policy = Some(manifest.approval_policy);

        // ── ② 聚焦过滤：tool_name ∈ focus_snapshot（装配期允许面） ──
        if !req.focus.allows(req.tool_name) {
            ledger.denial = Some(DenialStage::OutOfFocus);
            return PipelineOutcome {
                result: Err(PipelineFailure::Denial(PipelineDenial::new(
                    DenialStage::OutOfFocus,
                    format!(
                        "tool '{}' is not in the allowed focus set ({} tools)",
                        req.tool_name,
                        req.focus.len()
                    ),
                ))),
                ledger,
            };
        }
        ledger.in_focus = true;

        // ── ③ 意图信号 + ④ 规则裁决 ──
        // R1 通道（M5-c）：file_read/file_write 的 target_scope 随意图指令进链，
        // 由协作验收规则 enforce 裁决。原 execute_tool_call 内联逻辑逐字迁入。
        if let Some(scope) = resolve_target_scope(req.tool_name, req.args, deps.boundary) {
            let allowed = match deps
                .adjudicator
                .lock()
                .await
                .await_verdict(&intent_signal(scope), req.caller.session_id.as_deref())
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    // fail-closed 语义保持：通道故障显式上抛（原 map_err(AgentError::Internal)?）
                    ledger.denial = Some(DenialStage::Channel);
                    return PipelineOutcome {
                        result: Err(PipelineFailure::Denial(PipelineDenial::new(
                            DenialStage::Channel,
                            format!("adjudication channel error: {e}"),
                        ))),
                        ledger,
                    };
                }
            };
            ledger.adjudication.push(AdjudicationFact {
                channel: "target_scope_r1",
                allowed,
                evidence: format!("{tool} target_scope={scope}", tool = req.tool_name),
            });
            // B21:policy_judged（fail-visible——§七.6 账写不掉=错误显式化；
            // 原 `let _ =` fail-soft 消灭）
            if let Some(j) = deps.journal {
                if let Err(e) = j.policy_judged(
                    if allowed { "allowed" } else { "blocked" },
                    &format!("{} target_scope={}", req.tool_name, scope),
                ) {
                    ledger.denial = Some(DenialStage::Ledger);
                    return PipelineOutcome {
                        result: Err(PipelineFailure::Denial(PipelineDenial::new(
                            DenialStage::Ledger,
                            format!("journal policy_judged write failed: {e}"),
                        ))),
                        ledger,
                    };
                }
            }
            if !allowed {
                tracing::warn!(
                    main_session = ?req.caller.session_id, tool = %req.tool_name, scope = %scope,
                    "tool intent blocked by governance rule (collab acceptance, adjudication channel)"
                );
                // 被治理拦截的调用也是真实执行史——进轨迹(status=blocked)。
                // 轨迹锁显式处理：中毒=前持锁 panic，轨迹账面组件不可用=显式
                // 失败（此时尚未执行，Ledger 拒绝无重复执行险）。
                if let Some(tt) = deps.traces {
                    match tt.lock() {
                        Ok(mut tt) => {
                            tt.record(req.tool_name, req.args, "blocked_by_governance", 0);
                        }
                        Err(_) => {
                            tracing::error!(
                                main_session = ?req.caller.session_id, tool = %req.tool_name,
                                "tool trace lock poisoned at blocked_by_governance record"
                            );
                            ledger.denial = Some(DenialStage::Ledger);
                            return PipelineOutcome {
                                result: Err(PipelineFailure::Denial(PipelineDenial::new(
                                    DenialStage::Ledger,
                                    format!(
                                        "tool trace write failed (poisoned lock): {} \
                                         blocked_by_governance",
                                        req.tool_name
                                    ),
                                ))),
                                ledger,
                            };
                        }
                    }
                }
                ledger.denial = Some(DenialStage::Governance);
                return PipelineOutcome {
                    result: Err(PipelineFailure::Denial(PipelineDenial {
                        stage: DenialStage::Governance,
                        reason: "governance rule rejected target scope".to_string(),
                        llm_payload: Some(r1_blocked_payload(
                            req.tool_name,
                            scope,
                            req.args,
                            deps.boundary,
                        )),
                    })),
                    ledger,
                };
            }
        }
        // P2 通道：治理级工具事前意图裁决（分级=manifest 派生）。与 R1 通道
        // 并存互不干扰；裁决会话轮内复用。原内联逻辑逐字迁入。D6 终态：
        // runtime manifest 传阶段①查得条目——动态源（MCP/ServiceProxy，
        // 默认 Sensitive）入 P2；静态名防降级由 is_p2_adjudicated_runtime
        // 的静态优先判定序保证（不直接消费 handler 优先的 manifest_of 结果
        // 做静态判定）。
        if let Some(mut intent) =
            resolve_tool_intent(req.tool_name, req.args, deps.boundary, Some(&manifest))
        {
            // 契约 session_ref 补齐：主会话 id 由调用方上下文写入解析输出，
            // tool_intent_signal 派生 tool_intent.v1 契约时随 value 落链
            // （裁决账面审计关联）。
            if let Some(sid) = req.caller.session_id.as_deref() {
                intent["session_ref"] = Value::from(sid);
            }
            // 账面证据统一契约形态：裁决事实与 journal 留痕均落 tool_intent.v1
            // 契约 JSON（schema_ver/args_digest 锚点在账，摘要可从轨迹 args 复算）。
            let contract = crate::agent::tool_intent::ToolIntentV1::from_resolved(&intent);
            let contract_json = serde_json::to_string(&contract).unwrap_or_default();
            let allowed = match deps
                .adjudicator
                .lock()
                .await
                .await_verdict(
                    &tool_intent_signal(&intent),
                    req.caller.session_id.as_deref(),
                )
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    ledger.denial = Some(DenialStage::Channel);
                    return PipelineOutcome {
                        result: Err(PipelineFailure::Denial(PipelineDenial::new(
                            DenialStage::Channel,
                            format!("adjudication channel error: {e}"),
                        ))),
                        ledger,
                    };
                }
            };
            ledger.adjudication.push(AdjudicationFact {
                channel: "tool_intent_p2",
                allowed,
                evidence: contract_json.clone(),
            });
            if let Some(j) = deps.journal {
                if let Err(e) = j.policy_judged(
                    if allowed { "allowed" } else { "blocked" },
                    &format!("{} intent={}", req.tool_name, contract_json),
                ) {
                    ledger.denial = Some(DenialStage::Ledger);
                    return PipelineOutcome {
                        result: Err(PipelineFailure::Denial(PipelineDenial::new(
                            DenialStage::Ledger,
                            format!("journal policy_judged write failed: {e}"),
                        ))),
                        ledger,
                    };
                }
            }
            if !allowed {
                tracing::warn!(
                    main_session = ?req.caller.session_id, tool = %req.tool_name,
                    "tool intent blocked by governance rule (tool intent adjudication, adjudication channel)"
                );
                if let Some(tt) = deps.traces {
                    // 轨迹锁显式处理（同 R1 拦截臂：账面组件不可用=显式失败）
                    match tt.lock() {
                        Ok(mut tt) => {
                            tt.record(req.tool_name, req.args, "blocked_by_governance", 0);
                        }
                        Err(_) => {
                            tracing::error!(
                                main_session = ?req.caller.session_id, tool = %req.tool_name,
                                "tool trace lock poisoned at blocked_by_governance record"
                            );
                            ledger.denial = Some(DenialStage::Ledger);
                            return PipelineOutcome {
                                result: Err(PipelineFailure::Denial(PipelineDenial::new(
                                    DenialStage::Ledger,
                                    format!(
                                        "tool trace write failed (poisoned lock): {} \
                                         blocked_by_governance",
                                        req.tool_name
                                    ),
                                ))),
                                ledger,
                            };
                        }
                    }
                }
                ledger.denial = Some(DenialStage::Governance);
                return PipelineOutcome {
                    result: Err(PipelineFailure::Denial(PipelineDenial {
                        stage: DenialStage::Governance,
                        reason: "governance rule rejected tool intent".to_string(),
                        llm_payload: Some(serde_json::json!({
                            "status": "blocked_by_governance_rule",
                            "tool": req.tool_name,
                            "intent": intent,
                            "reason": "tool intent rejected by governance rule \
                                       (tool intent adjudication; see adjudication session \
                                       audit Violation for rule attribution)",
                        })),
                    })),
                    ledger,
                };
            }
        }

        // ── ⑤ 分级审批（approval_policy 派发 + candidate 评估单源，PR-4 收编）──
        // AlwaysDeny：执行前拒绝（逃逸出口/不可逆破坏类预留；现状静态表无
        // AlwaysDeny 工具，此臂为策略完备性在案）。
        // ManualDefault/AutoPolicy/HumanOnly 的 candidate 形态：proposal 由
        // 注册执行器实例的 evaluate_proposal 钩子求值（评估与执行同源，工具
        // 侧两调协议消灭——call 不再自管 approved 旗标分支）：
        // - 有预供给决策（ApprovalReexec 入口）→ 消费决策（批准→⑦ / 拒绝→
        //   显式拒绝回喂），决策记录入统一账面（ledger.approval）；
        // - 无预供给 → 暂停（ApprovalPending 携带 proposal JSON），决策在
        //   外部编排层（非流式=runner 决策端 / 流式=事件循环审批窗 / 并行
        //   预执行=candidate 不入缓存）。
        if manifest.approval_policy == ApprovalPolicy::AlwaysDeny {
            ledger.denial = Some(DenialStage::ApprovalDenied);
            return PipelineOutcome {
                result: Err(PipelineFailure::Denial(PipelineDenial::new(
                    DenialStage::ApprovalDenied,
                    format!(
                        "tool '{}' is configured AlwaysDeny (irreversible/escape-class)",
                        req.tool_name
                    ),
                ))),
                ledger,
            };
        }
        if let Some(mut proposal) = deps
            .proposal_of
            .and_then(|evaluate| evaluate(req.tool_name, req.args))
        {
            // proposal_id 单源：评估臂生成并注入 proposal JSON，决策记录与
            // 决策端 parse 共用同一 id（账面/留痕/决策端三方一致）
            let proposal_id = crate::agent::approval::new_proposal_id();
            if let Some(obj) = proposal.as_object_mut() {
                obj.insert("proposal_id".to_string(), Value::from(proposal_id.clone()));
            }
            match &req.approval_preset {
                Some(decision) => {
                    // 决策记录（字段序与流式审批留痕逐字同构）
                    let decided_at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let decision_label = if decision.approved {
                        "approved"
                    } else if decision.auto_rejected {
                        "auto_rejected"
                    } else {
                        "rejected"
                    };
                    let record = serde_json::json!({
                        "proposal_id": proposal_id,
                        "tool": req.tool_name,
                        "decision": decision_label,
                        "approver": decision.approver,
                        "verified": decision.verified,
                        "decided_at": decided_at,
                        "reason": decision.reason,
                    });
                    ledger.approval = Some(record.clone());
                    if !decision.approved {
                        tracing::warn!(
                            main_session = ?req.caller.session_id, tool = %req.tool_name,
                            "pipeline ⑤: candidate rejected by preset decision"
                        );
                        // 轨迹：先落一条审批拒绝条目再附加决策记录（拒绝路径
                        // 无⑦执行条目，无前置条目可附——显式新写防串话）
                        if let Some(tt) = deps.traces {
                            if let Ok(mut tt) = tt.lock() {
                                tt.record(req.tool_name, req.args, "approval_denied", 0);
                                tt.attach_approval_to_last(record);
                            } else {
                                tracing::error!(
                                    main_session = ?req.caller.session_id, tool = %req.tool_name,
                                    "tool trace lock poisoned at approval_denied record (ledger gap)"
                                );
                            }
                        }
                        ledger.denial = Some(DenialStage::ApprovalDenied);
                        return PipelineOutcome {
                            result: Err(PipelineFailure::Denial(PipelineDenial {
                                stage: DenialStage::ApprovalDenied,
                                reason: format!(
                                    "tool '{}' candidate action rejected by approver",
                                    req.tool_name
                                ),
                                llm_payload: Some(serde_json::json!({
                                    "status": "rejected",
                                    "message": "User denied approval",
                                })),
                            })),
                            ledger,
                        };
                    }
                    // 批准 → 直通⑦（真实执行，无 approved 旗标注入——决策在
                    // 管道内闭环，工具不再自管协议）
                }
                None => {
                    // 无预供给：暂停等决策（决策端在编排层）。轨迹先落
                    // approval_pending 条目——决策端拒绝时 attach 载体在案。
                    if let Some(tt) = deps.traces {
                        if let Ok(mut tt) = tt.lock() {
                            tt.record(req.tool_name, req.args, "approval_pending", 0);
                        } else {
                            tracing::error!(
                                main_session = ?req.caller.session_id, tool = %req.tool_name,
                                "tool trace lock poisoned at approval_pending record (ledger gap)"
                            );
                        }
                    }
                    ledger.denial = Some(DenialStage::ApprovalPending);
                    return PipelineOutcome {
                        result: Err(PipelineFailure::Denial(PipelineDenial {
                            stage: DenialStage::ApprovalPending,
                            reason: format!(
                                "tool '{}' candidate action awaits approval decision",
                                req.tool_name
                            ),
                            llm_payload: Some(proposal),
                        })),
                        ledger,
                    };
                }
            }
        }

        // ── ⑥ 机制沙箱（占位）──
        // fs_safety 路径围栏/net_guard 仍在工具实现内联执行（双层分工：
        // 意图快筛供规则层裁决，handler 精判为最终防线）；管道侧阶段位在案，
        // PR-6 执行契约批次再评估守卫上提。

        // ── ⑦ 执行（真实结局采集：metrics + 轨迹，G17 同点同规格）──
        // precomputed=缓存命中（G13 收口）：⑦免重执行直接采信已过闸产物，
        // 观测照常（时长≈0 的真实结局——本次调用确实被服务完成）。
        // 瞬态故障重试面（执行阶段内部循环，治理段 ①-⑥ 每调用只走一次）：
        // 幂等类工具遇连接/超时类错误形态自动重试（有限次、线性回退，与
        // LLM 传输层重试面同参数量级）；非幂等工具不自动重试——盲重试写
        // 操作是漂移风险源，失败显式回喂。每次重试先落账再推进。
        let pre = req.precomputed.clone();
        let tool_start = Instant::now();
        let raw = match pre {
            Some(cached) => Ok(cached),
            None => {
                let mut outcome = deps.executor.execute_tool(req.tool_name, req.args).await;
                let mut retry_no: u32 = 0;
                while let Err(err) = &outcome {
                    if retry_no >= crate::agent::tool_retry::MAX_TOOL_RETRIES {
                        break;
                    }
                    let transient = crate::agent::tool_retry::is_transient_error(&err.to_string());
                    let retryable = matches!(
                        crate::agent::tool_retry::retry_class(req.tool_name),
                        crate::agent::tool_retry::RetryClass::IdempotentRead
                            | crate::agent::tool_retry::RetryClass::IdempotentWrite
                    );
                    if !(transient && retryable) {
                        break;
                    }
                    retry_no += 1;
                    if let Some(sink) = deps.journal {
                        if let Err(e) =
                            sink.tool_retried(req.tool_name, u64::from(retry_no), transient)
                        {
                            // fail-visible 同款语义：账写不掉 = 错误显式化
                            ledger.denial = Some(DenialStage::Ledger);
                            return PipelineOutcome {
                                result: Err(PipelineFailure::Denial(PipelineDenial::new(
                                    DenialStage::Ledger,
                                    format!("journal tool_retried write failed: {e}"),
                                ))),
                                ledger,
                            };
                        }
                    }
                    tokio::time::sleep(deps.retry_backoff * retry_no).await;
                    outcome = deps.executor.execute_tool(req.tool_name, req.args).await;
                }
                outcome
            }
        };
        let tool_duration = tool_start.elapsed();
        let tool_ok = raw.is_ok();
        ledger.execution = Some(ExecutionFact {
            ok: tool_ok,
            duration_ms: tool_duration.as_millis() as u64,
        });
        if let Some(m) = deps.metrics {
            m.observe_tool_call(req.tool_name, tool_duration, tool_ok);
        }
        // 轨迹采集（脱敏+截断在 collector 内；std Mutex 临界区无 await）。
        // 轨迹锁显式处理：中毒=前持锁 panic（进程状态可疑），error 留痕——
        // 但执行已发生且真实结局必须回喂（此处拒绝会诱导 LLM 对已执行工具
        // 重试，重复执行险大于账面缺口），故豁免 Ledger 拒绝、仅显式留痕
        // 账面缺口（轨迹缺失以 error 日志为信号，不做静默跳过）。
        if let Some(tt) = deps.traces {
            match tt.lock() {
                Ok(mut tt) => {
                    tt.record(
                        req.tool_name,
                        req.args,
                        if tool_ok { "ok" } else { "error" },
                        tool_duration.as_millis() as u64,
                    );
                }
                Err(_) => {
                    tracing::error!(
                        main_session = ?req.caller.session_id, tool = %req.tool_name,
                        "tool trace lock poisoned after execution; trace entry lost (ledger gap)"
                    );
                }
            }
        }

        // ── ⑧ 结局落账 ──
        // LedgerRecord 已含执行结局（管道内事实层账面）；链侧 tool_trace
        // 指令统一接线（工具名/结局/时长/摘要）随信号契约批次落地。
        // 执行错误≠拒绝（语义分离见 PipelineFailure）：原样透传回喂。
        PipelineOutcome {
            result: raw.map_err(PipelineFailure::Execution),
            ledger,
        }
    }
}

/// R1 通道治理拦截负载（原 execute_tool_call 内联文案逐字保留——行为等价）
///
/// 拒绝文案两态：绝对路径在快筛一律按形态判 out（判据与 handler 权威沙箱
/// 检查同源），但目标实际落在边界内时（join 后 starts_with 成立）"越界"
/// 语义不成立——改用形态判定文案（相对路径口径），避免审计链出现
/// "target 在 boundary 前缀内却被称 outside"的自相矛盾留痕；真·越界
/// （join 逃逸/`..` 穿越）维持 containment 文案。
fn r1_blocked_payload(
    tool_name: &str,
    scope: &str,
    args: &Value,
    boundary: Option<&CapabilityBoundary>,
) -> Value {
    let raw_path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let boundary_root = boundary
        .map(|b| b.sandbox_root.display().to_string())
        .unwrap_or_default();
    let inside_boundary = boundary.is_some_and(|b| {
        let p = std::path::Path::new(raw_path);
        p.is_absolute() && b.sandbox_root.join(p).starts_with(&b.sandbox_root)
    });
    let reason = if inside_boundary {
        format!(
            "absolute path form not allowed: '{raw_path}' (paths are relative \
             to the sandbox root '{boundary_root}'; the target resolves inside the \
             boundary); the collaboration acceptance rule rejected this \
             tool intent (see session audit Violation for rule attribution)"
        )
    } else {
        format!(
            "target '{raw_path}' is outside the sandbox boundary '{boundary_root}'; \
             the collaboration acceptance rule rejected this tool intent \
             (see session audit Violation for rule attribution)"
        )
    };
    serde_json::json!({
        "status": "blocked_by_governance_rule",
        "tool": tool_name,
        "target_scope": scope,
        "reason": reason,
    })
}

// =============================================================================
// 测试（管道单元面；runner 端到端行为等价由 runner_tests 既有基线锁守）
// ==============================================================================
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::agent::tool_manifest::lookup_static;

    /// 正常执行桩（到达⑦即返回固定值；未到达则 panic 暴露阶段跳转错误）
    struct OkExecutor;
    impl PipelineExecutor for OkExecutor {
        fn execute_tool(
            &self,
            tool_name: &str,
            _args: &Value,
        ) -> Pin<Box<dyn Future<Output = Result<Value, AgentError>> + Send + '_>> {
            assert_ne!(tool_name, "", "executor must receive a tool name");
            Box::pin(async { Ok(Value::from("executed")) })
        }
    }

    /// 计数执行桩:前 `fails_left` 次返回固定错误文本,此后成功;记录总调用数
    /// (重试面验收:调用计数=首次执行+实际发生的重试次数)
    struct FlakyExecutor {
        fails_left: std::sync::atomic::AtomicUsize,
        err: String,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl FlakyExecutor {
        fn failing_times(n: usize, err: &str) -> Self {
            Self {
                fails_left: std::sync::atomic::AtomicUsize::new(n),
                err: err.to_string(),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    impl PipelineExecutor for FlakyExecutor {
        fn execute_tool(
            &self,
            _tool_name: &str,
            _args: &Value,
        ) -> Pin<Box<dyn Future<Output = Result<Value, AgentError>> + Send + '_>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let fails_left = &self.fails_left;
            let err = self.err.clone();
            Box::pin(async move {
                if fails_left.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) > 0 {
                    Err(AgentError::ToolError(err))
                } else {
                    Ok(Value::from("executed"))
                }
            })
        }
    }

    /// 不应到达⑦的执行器（到达即 panic）
    struct UnreachableExecutor;
    impl PipelineExecutor for UnreachableExecutor {
        fn execute_tool(
            &self,
            tool_name: &str,
            _args: &Value,
        ) -> Pin<Box<dyn Future<Output = Result<Value, AgentError>> + Send + '_>> {
            panic!("stage ⑦ must not be reached, but executed tool: {tool_name}")
        }
    }

    /// 恒失败账面（§七.6 写失败注入桩）
    struct FailingJournal;
    impl PolicyJudgedSink for FailingJournal {
        fn policy_judged(&self, _verdict: &str, _evidence: &str) -> Result<u64, JournalError> {
            Err(JournalError::Corrupt("injected write failure".to_string()))
        }

        fn tool_retried(
            &self,
            _tool: &str,
            _attempt: u64,
            _transient: bool,
        ) -> Result<u64, JournalError> {
            Err(JournalError::Corrupt("injected write failure".to_string()))
        }
    }

    fn dummy_adjudicator() -> tokio::sync::Mutex<AdjudicationChannel> {
        // 测试桩：仅测①②⑤⑧路径时不触网；触③④的场景由 runner_tests
        // mockito 基线覆盖（见模块文档）。
        tokio::sync::Mutex::new(AdjudicationChannel::new(
            crate::api::evorule_client::EvoruleApiClient::new("http://127.0.0.1:1"),
            "default",
        ))
    }

    /// 断言结局为 Denial 并取出（执行失败与拒绝语义分离）
    fn denial_of(outcome: &PipelineOutcome) -> PipelineDenial {
        match &outcome.result {
            Err(PipelineFailure::Denial(d)) => d.clone(),
            other => panic!("expected denial, got: {other:?}"),
        }
    }

    fn deps_for<'a>(
        executor: &'a dyn PipelineExecutor,
        adj: &'a tokio::sync::Mutex<AdjudicationChannel>,
        manifest_of: &'a (dyn Fn(&str) -> Option<ToolManifest> + Sync),
    ) -> PipelineDeps<'a> {
        PipelineDeps {
            executor,
            adjudicator: adj,
            manifest_of,
            proposal_of: None,
            journal: None,
            boundary: None,
            traces: None,
            metrics: None,
            retry_backoff: std::time::Duration::from_millis(1),
        }
    }

    fn req_for<'a>(
        tool_name: &'a str,
        args: &'a Value,
        focus: &'a FocusSnapshot,
    ) -> PipelineRequest<'a> {
        PipelineRequest {
            tool_name,
            args,
            caller: CallerContext {
                entry: PipelineEntry::React,
                session_id: Some("s-test".to_string()),
            },
            focus,
            precomputed: None,
            approval_preset: None,
        }
    }

    #[tokio::test]
    async fn stage1_unknown_tool_is_no_manifest_denial() {
        // ①查表：无 manifest = 显式拒绝，原因文本与 ToolHandler not-found 等价
        let adj = dummy_adjudicator();
        let manifests = |_n: &str| None;
        let focus = FocusSnapshot::from_names(Vec::<String>::new());
        let args = Value::Null;
        let deps = deps_for(&UnreachableExecutor, &adj, &manifests);
        let out = ToolExecutionPipeline
            .execute(req_for("no_such_tool", &args, &focus), deps)
            .await;
        let denial = denial_of(&out);
        assert_eq!(denial.stage, DenialStage::NoManifest);
        assert_eq!(denial.reason, "tool not found: no_such_tool");
        assert!(denial.llm_payload.is_none());
        assert!(!out.ledger.manifest_found);
        assert_eq!(out.ledger.denial, Some(DenialStage::NoManifest));
    }

    #[tokio::test]
    async fn stage2_out_of_focus_denied_before_execution() {
        // ②聚焦：查表通过但不在允许面 = OutOfFocus，且不触达⑦
        let adj = dummy_adjudicator();
        let manifests = |n: &str| lookup_static(n);
        let focus = FocusSnapshot::from_names(Vec::<String>::new()); // 空允许面
        let args = Value::Null;
        let deps = deps_for(&UnreachableExecutor, &adj, &manifests);
        let out = ToolExecutionPipeline
            .execute(req_for("grep_files", &args, &focus), deps)
            .await;
        let denial = denial_of(&out);
        assert_eq!(denial.stage, DenialStage::OutOfFocus);
        assert!(out.ledger.manifest_found);
        assert!(!out.ledger.in_focus);
        assert_eq!(out.ledger.denial, Some(DenialStage::OutOfFocus));
    }

    #[tokio::test]
    async fn stage5_always_deny_denied_without_execution() {
        // ⑤分级审批：AlwaysDeny = 执行前拒绝（不经③④——无 scope/无 intent）
        let adj = dummy_adjudicator();
        let manifests = |n: &str| {
            lookup_static(n).map(|mut m| {
                m.approval_policy = ApprovalPolicy::AlwaysDeny;
                m
            })
        };
        let focus = FocusSnapshot::from_names(["grep_files"]);
        let args = Value::Null;
        let deps = deps_for(&UnreachableExecutor, &adj, &manifests);
        let out = ToolExecutionPipeline
            .execute(req_for("grep_files", &args, &focus), deps)
            .await;
        let denial = denial_of(&out);
        assert_eq!(denial.stage, DenialStage::ApprovalDenied);
        assert_eq!(out.ledger.approval_policy, Some(ApprovalPolicy::AlwaysDeny));
        assert!(out.ledger.execution.is_none());
    }

    #[tokio::test]
    async fn precomputed_serves_cached_result_without_executor_and_keeps_ledger() {
        // 缓存收口（PR-3）：precomputed=Some 时⑦免重执行（UnreachableExecutor
        // 证明执行器不触达），①②⑤照常、⑧结局落账照常——命中调用不缺账面。
        let adj = dummy_adjudicator();
        let manifests = |n: &str| lookup_static(n);
        let focus = FocusSnapshot::from_names(["grep_files"]);
        let args = Value::Null;
        let deps = deps_for(&UnreachableExecutor, &adj, &manifests);
        let mut req = req_for("grep_files", &args, &focus);
        req.precomputed = Some(Value::from("cached-result"));
        let out = ToolExecutionPipeline.execute(req, deps).await;
        assert_eq!(out.result.expect("缓存命中必须回喂缓存值"), "cached-result");
        let execution = out.ledger.execution.as_ref().expect("命中调用也必须落账");
        assert!(execution.ok);
        // 入口类可审计：缓存命中走的仍是管道（非旁路）
        assert_eq!(out.ledger.caller.entry, PipelineEntry::React);
    }

    #[tokio::test]
    async fn happy_path_records_full_ledger() {
        // 放行直通：①②⑤通过（无③④触发工具）→⑦执行→⑧账面四类事实俱在
        let adj = dummy_adjudicator();
        let manifests = |n: &str| lookup_static(n);
        let focus = FocusSnapshot::from_names(["grep_files"]);
        let args = Value::Null;
        let deps = deps_for(&OkExecutor, &adj, &manifests);
        let out = ToolExecutionPipeline
            .execute(req_for("grep_files", &args, &focus), deps)
            .await;
        assert!(out.result.is_ok(), "allowed tool must execute");
        let ledger = &out.ledger;
        assert!(ledger.manifest_found);
        assert!(ledger.in_focus);
        assert_eq!(ledger.approval_policy, Some(ApprovalPolicy::AutoPolicy));
        assert!(ledger.adjudication.is_empty(), "非治理工具不应有裁决事实");
        let execution = ledger.execution.as_ref().expect("执行结局必须落账");
        assert!(execution.ok);
        assert_eq!(out.result.unwrap(), Value::from("executed"));
        // 账面可序列化（§七.6 审计完整性断言的数据面契约）
        serde_json::to_string(ledger).expect("ledger must serialize");
    }

    #[tokio::test]
    async fn transient_failure_retries_idempotent_read_and_records_attempt() {
        // 幂等读类+瞬态错误:自动重试一次后成功;每次重试独立落账
        let adj = dummy_adjudicator();
        let manifests = |n: &str| lookup_static(n);
        let focus = FocusSnapshot::from_names(["grep_files"]);
        let args = Value::Null;
        let exec = FlakyExecutor::failing_times(1, "operation timed out");
        let journal = RecordingJournal(std::sync::Mutex::new(Vec::new()));
        let mut deps = deps_for(&exec, &adj, &manifests);
        deps.journal = Some(&journal);
        let out = ToolExecutionPipeline
            .execute(req_for("grep_files", &args, &focus), deps)
            .await;
        assert!(out.result.is_ok(), "瞬态失败必须在重试后恢复");
        assert_eq!(exec.call_count(), 2, "首次执行+1 次重试");
        let rec = journal.0.lock().unwrap();
        assert_eq!(rec.len(), 1, "恰好一条重试落账");
        assert_eq!(rec[0].0, "tool_retried");
        assert_eq!(rec[0].1, "grep_files/1/transient=true");
    }

    #[tokio::test]
    async fn non_retryable_class_never_auto_retries() {
        // 非幂等类(网络调用族):瞬态错误也不自动重试——失败显式回喂(负例)
        //(选 http_get:不在治理裁决面、无 candidate 形态,可直达⑦)
        let adj = dummy_adjudicator();
        let manifests = |n: &str| lookup_static(n);
        let focus = FocusSnapshot::from_names(["http_get"]);
        let args = serde_json::json!({"url": "http://127.0.0.1:1/x"});
        let exec = FlakyExecutor::failing_times(5, "operation timed out");
        let journal = RecordingJournal(std::sync::Mutex::new(Vec::new()));
        let mut deps = deps_for(&exec, &adj, &manifests);
        deps.journal = Some(&journal);
        let out = ToolExecutionPipeline
            .execute(req_for("http_get", &args, &focus), deps)
            .await;
        assert!(out.result.is_err(), "执行失败必须显式回喂");
        assert_eq!(exec.call_count(), 1, "非幂等类零自动重试");
        assert!(journal.0.lock().unwrap().is_empty(), "零重试零落账");
    }

    #[tokio::test]
    async fn non_transient_failure_does_not_retry() {
        // 幂等类+非瞬态错误(参数形态):不自动重试
        let adj = dummy_adjudicator();
        let manifests = |n: &str| lookup_static(n);
        let focus = FocusSnapshot::from_names(["grep_files"]);
        let args = Value::Null;
        let exec = FlakyExecutor::failing_times(5, "invalid arguments: missing field `path`");
        let deps = deps_for(&exec, &adj, &manifests);
        let out = ToolExecutionPipeline
            .execute(req_for("grep_files", &args, &focus), deps)
            .await;
        assert!(out.result.is_err());
        assert_eq!(exec.call_count(), 1, "非瞬态失败零自动重试");
    }

    #[tokio::test]
    async fn retry_exhaustion_stops_at_cap() {
        // 重试耗尽即透传终错;每次重试都留账(attempt 连续递增)
        let adj = dummy_adjudicator();
        let manifests = |n: &str| lookup_static(n);
        let focus = FocusSnapshot::from_names(["grep_files"]);
        let args = Value::Null;
        let exec = FlakyExecutor::failing_times(10, "connection refused");
        let journal = RecordingJournal(std::sync::Mutex::new(Vec::new()));
        let mut deps = deps_for(&exec, &adj, &manifests);
        deps.journal = Some(&journal);
        let out = ToolExecutionPipeline
            .execute(req_for("grep_files", &args, &focus), deps)
            .await;
        assert!(out.result.is_err(), "重试耗尽必须透传终错");
        assert_eq!(
            exec.call_count(),
            1 + crate::agent::tool_retry::MAX_TOOL_RETRIES as usize,
            "首次执行+重试次数封顶"
        );
        let rec = journal.0.lock().unwrap();
        assert_eq!(rec.len(), 2);
        assert_eq!(rec[0].1, "grep_files/1/transient=true");
        assert_eq!(rec[1].1, "grep_files/2/transient=true");
    }

    #[tokio::test]
    async fn retry_ledger_write_failure_is_fail_visible() {
        // 重试落账失败 = 显式拒绝(Ledger 阶段);落账失败即断,不带账缺口重试
        let adj = dummy_adjudicator();
        let manifests = |n: &str| lookup_static(n);
        let focus = FocusSnapshot::from_names(["grep_files"]);
        let args = Value::Null;
        let exec = FlakyExecutor::failing_times(5, "operation timed out");
        let deps = PipelineDeps {
            journal: Some(&FailingJournal),
            ..deps_for(&exec, &adj, &manifests)
        };
        let out = ToolExecutionPipeline
            .execute(req_for("grep_files", &args, &focus), deps)
            .await;
        let denial = denial_of(&out);
        assert_eq!(denial.stage, DenialStage::Ledger);
        assert_eq!(exec.call_count(), 1, "落账失败即断,不得带账缺口推进");
    }

    #[tokio::test]
    async fn channel_failure_is_fail_closed() {
        // ③④裁决通道故障 = 显式上抛（fail-closed 镜像；原 map_err(Internal) 语义）
        let adj = dummy_adjudicator();
        let manifests = |n: &str| lookup_static(n);
        let focus = FocusSnapshot::from_names(["file_create"]);
        let args = serde_json::json!({"path": "rel.txt"});
        let deps = deps_for(&UnreachableExecutor, &adj, &manifests);
        let out = ToolExecutionPipeline
            .execute(req_for("file_create", &args, &focus), deps)
            .await;
        let denial = denial_of(&out);
        assert_eq!(denial.stage, DenialStage::Channel);
        assert!(denial.reason.contains("adjudication channel error"));
    }

    #[tokio::test]
    async fn journal_write_failure_is_visible() {
        // §七.6：journal 写失败注入 → 显式报错非静默。
        // mockito 放行判据（before=0 + poll=1，同 adjudicator.rs 形态）——
        // P2 通道裁决放行后 policy_judged 写失败 → Ledger 拒绝（fail-visible），
        // ⑦不触达。
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"session_id": 90}"#)
            .create_async()
            .await;
        // before 首查命中 version 0，轮询命中 version 1 = 放行（同
        // adjudicator.rs 测试形态；单 mock 恒 1 会令 before=1 恒拦截）
        server
            .mock("GET", "/api/sessions/90/state")
            .with_status(200)
            .with_body(r#"{"version": 0}"#)
            .expect(1)
            .create_async()
            .await;
        server
            .mock("GET", "/api/sessions/90/state")
            .with_status(200)
            .with_body(r#"{"version": 1}"#)
            .create_async()
            .await;
        server
            .mock("POST", "/api/sessions/90/command")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;

        let adj = tokio::sync::Mutex::new(AdjudicationChannel::new(
            crate::api::evorule_client::EvoruleApiClient::new(&server.url()),
            "default",
        ));
        let manifests = |n: &str| lookup_static(n);
        let focus = FocusSnapshot::from_names(["file_create"]);
        let args = serde_json::json!({"path": "rel.txt"});
        let mut deps = deps_for(&UnreachableExecutor, &adj, &manifests);
        deps.journal = Some(&FailingJournal);
        let out = ToolExecutionPipeline
            .execute(req_for("file_create", &args, &focus), deps)
            .await;
        let denial = denial_of(&out);
        assert_eq!(denial.stage, DenialStage::Ledger);
        assert!(denial.reason.contains("journal policy_judged write failed"));
        // 裁决已发生且放行——账面应记裁决事实后死于账面写
        assert_eq!(out.ledger.adjudication.len(), 1);
        assert!(out.ledger.adjudication[0].allowed);
    }

    // ----- 账面可复算（LedgerRecord ↔ journal + tool_trace 一致性）-----

    /// 账面记录桩：捕获 policy_judged 调用序列（账面可复算断言用）
    struct RecordingJournal(std::sync::Mutex<Vec<(String, String)>>);
    impl PolicyJudgedSink for RecordingJournal {
        fn policy_judged(&self, verdict: &str, evidence: &str) -> Result<u64, JournalError> {
            if let Ok(mut seq) = self.0.lock() {
                seq.push((verdict.to_string(), evidence.to_string()));
            }
            Ok(0)
        }

        fn tool_retried(
            &self,
            tool: &str,
            attempt: u64,
            transient: bool,
        ) -> Result<u64, JournalError> {
            if let Ok(mut seq) = self.0.lock() {
                seq.push((
                    "tool_retried".to_string(),
                    format!("{tool}/{attempt}/transient={transient}"),
                ));
            }
            Ok(0)
        }
    }

    /// 装配放行判据 mock（before=0 命中一次 + 轮询=1，同 adjudicator.rs 形态）
    async fn mock_allow_once(server: &mut mockito::Server) {
        server
            .mock("POST", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"session_id": 90}"#)
            .create_async()
            .await;
        server
            .mock("GET", "/api/sessions/90/state")
            .with_status(200)
            .with_body(r#"{"version": 0}"#)
            .expect(1)
            .create_async()
            .await;
        server
            .mock("GET", "/api/sessions/90/state")
            .with_status(200)
            .with_body(r#"{"version": 1}"#)
            .create_async()
            .await;
        server
            .mock("POST", "/api/sessions/90/command")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;
    }

    /// 装配拦截判据 mock（version 恒 0：1 次首查 + 20 次轮询，fail-closed 判据）
    async fn mock_block_always(server: &mut mockito::Server) {
        server
            .mock("POST", "/api/sessions")
            .with_status(200)
            .with_body(r#"{"session_id": 90}"#)
            .create_async()
            .await;
        server
            .mock("GET", "/api/sessions/90/state")
            .with_status(200)
            .with_body(r#"{"version": 0}"#)
            .expect(21)
            .create_async()
            .await;
        server
            .mock("POST", "/api/sessions/90/command")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;
    }

    /// 账面可复算断言：从 journal（policy_judged 序列）+ tool_trace（record
    /// 序列）重建与 LedgerRecord 一致的事实——裁决条数/结论/证据、执行结局
    /// 与轨迹逐项对齐（账写不掉的镜像面：账面必须可由持久账复算）。
    fn assert_ledger_rebuildable_from_books(
        ledger: &LedgerRecord,
        journal: &[(String, String)],
        traces: &[Value],
    ) {
        // 裁决事实：journal 序列与 ledger.adjudication 逐条对齐
        assert_eq!(
            journal.len(),
            ledger.adjudication.len(),
            "裁决账条数必须一致"
        );
        for ((verdict, evidence), fact) in journal.iter().zip(&ledger.adjudication) {
            assert_eq!(
                verdict,
                if fact.allowed { "allowed" } else { "blocked" },
                "裁决结论必须一致: {evidence}"
            );
            assert!(
                evidence.contains(&fact.evidence),
                "journal 证据必须含 ledger 证据: {evidence} vs {}",
                fact.evidence
            );
        }
        // 执行事实：轨迹与 ledger.execution 对齐（被拒调用无执行结局，
        // 轨迹记 blocked_by_governance；放行调用结局/时长逐值一致）
        match (&ledger.execution, traces.last()) {
            (Some(exec), Some(trace)) => {
                assert_eq!(trace["tool_name"], ledger.tool_name);
                assert_eq!(
                    trace["status"],
                    if exec.ok { "ok" } else { "error" },
                    "轨迹结局必须与账面一致"
                );
                assert_eq!(trace["duration_ms"], exec.duration_ms);
            }
            (None, Some(trace)) => {
                assert_eq!(trace["status"], "blocked_by_governance");
                assert_eq!(trace["duration_ms"], 0);
            }
            (None, None) => {}
            (Some(_), None) => panic!("账面有执行结局而轨迹为零——账面不可复算"),
        }
    }

    #[tokio::test]
    async fn ledger_is_rebuildable_from_journal_and_traces_p2_allowed() {
        // P2 放行链路：裁决放行→执行→三账（ledger/journal/tool_trace）互证；
        // 契约摘要可从轨迹 args 复算（账面可复算）。
        let mut server = mockito::Server::new_async().await;
        mock_allow_once(&mut server).await;
        let adj = tokio::sync::Mutex::new(AdjudicationChannel::new(
            crate::api::evorule_client::EvoruleApiClient::new(&server.url()),
            "default",
        ));
        let manifests = |n: &str| lookup_static(n);
        let focus = FocusSnapshot::from_names(["file_create"]);
        let args = serde_json::json!({"path": "rel.txt"});
        let journal = RecordingJournal(std::sync::Mutex::new(Vec::new()));
        let traces = std::sync::Mutex::new(ToolTraceCollector::default());
        let mut deps = deps_for(&OkExecutor, &adj, &manifests);
        deps.journal = Some(&journal);
        deps.traces = Some(&traces);
        let out = ToolExecutionPipeline
            .execute(req_for("file_create", &args, &focus), deps)
            .await;
        assert!(out.result.is_ok(), "allowed tool must execute");
        let jseq = journal.0.lock().expect("journal seq").clone();
        let tseq = traces.lock().expect("traces").drain();
        assert_ledger_rebuildable_from_books(&out.ledger, &jseq, &tseq);
        // P2 契约证据：schema_ver/session_ref 在账、摘要可由轨迹 args 复算
        let fact = &out.ledger.adjudication[0];
        assert_eq!(fact.channel, "tool_intent_p2");
        let contract: Value = serde_json::from_str(&fact.evidence).expect("契约 JSON");
        assert_eq!(contract["schema_ver"], "tool_intent.v1");
        assert_eq!(contract["session_ref"], "s-test");
        assert_eq!(
            contract["args_digest"],
            crate::agent::tool_intent::args_digest(&tseq[0]["args"]),
            "摘要必须可从轨迹 args 复算"
        );
    }

    #[tokio::test]
    async fn ledger_is_rebuildable_from_journal_and_traces_p2_blocked() {
        // P2 拦截链路：裁决拦截→轨迹记 blocked→三账互证（无执行结局）
        let mut server = mockito::Server::new_async().await;
        mock_block_always(&mut server).await;
        let adj = tokio::sync::Mutex::new(AdjudicationChannel::new(
            crate::api::evorule_client::EvoruleApiClient::new(&server.url()),
            "default",
        ));
        let manifests = |n: &str| lookup_static(n);
        let focus = FocusSnapshot::from_names(["file_create"]);
        let args = serde_json::json!({"path": "rel.txt"});
        let journal = RecordingJournal(std::sync::Mutex::new(Vec::new()));
        let traces = std::sync::Mutex::new(ToolTraceCollector::default());
        let mut deps = deps_for(&UnreachableExecutor, &adj, &manifests);
        deps.journal = Some(&journal);
        deps.traces = Some(&traces);
        let out = ToolExecutionPipeline
            .execute(req_for("file_create", &args, &focus), deps)
            .await;
        let denial = denial_of(&out);
        assert_eq!(denial.stage, DenialStage::Governance);
        assert_eq!(out.ledger.denial, Some(DenialStage::Governance));
        assert!(out.ledger.execution.is_none());
        let jseq = journal.0.lock().expect("journal seq").clone();
        let tseq = traces.lock().expect("traces").drain();
        assert_ledger_rebuildable_from_books(&out.ledger, &jseq, &tseq);
        assert!(!out.ledger.adjudication[0].allowed, "拦截结论须入账");
    }

    #[tokio::test]
    async fn ledger_is_rebuildable_from_traces_without_adjudication() {
        // 非治理工具直通：journal 零条、轨迹 1 条 ok，与账面一致
        let adj = dummy_adjudicator();
        let manifests = |n: &str| lookup_static(n);
        let focus = FocusSnapshot::from_names(["grep_files"]);
        let args = Value::Null;
        let journal = RecordingJournal(std::sync::Mutex::new(Vec::new()));
        let traces = std::sync::Mutex::new(ToolTraceCollector::default());
        let mut deps = deps_for(&OkExecutor, &adj, &manifests);
        deps.journal = Some(&journal);
        deps.traces = Some(&traces);
        let out = ToolExecutionPipeline
            .execute(req_for("grep_files", &args, &focus), deps)
            .await;
        assert!(out.result.is_ok());
        assert!(out.ledger.adjudication.is_empty());
        let jseq = journal.0.lock().expect("journal seq").clone();
        assert!(jseq.is_empty(), "无裁决无 journal 条目");
        let tseq = traces.lock().expect("traces").drain();
        assert_ledger_rebuildable_from_books(&out.ledger, &jseq, &tseq);
    }

    // ----- 轨迹锁中毒显式处理（账面组件不可用=显式失败）-----

    /// 制造锁中毒（前持锁 panic），返回被毒化的采集器互斥体
    fn poisoned_traces() -> std::sync::Arc<std::sync::Mutex<ToolTraceCollector>> {
        let traces = std::sync::Arc::new(std::sync::Mutex::new(ToolTraceCollector::default()));
        let t = std::sync::Arc::clone(&traces);
        let _ = std::thread::spawn(move || {
            let _guard = t.lock().expect("guard before poisoning panic");
            panic!("poison the tool trace lock");
        })
        .join();
        traces
    }

    #[tokio::test]
    async fn poisoned_trace_lock_at_blocked_path_is_explicit_ledger_failure() {
        // 拦截臂轨迹锁中毒：账写不掉=显式 Ledger 拒绝（未执行，无重复执行险）
        let mut server = mockito::Server::new_async().await;
        mock_block_always(&mut server).await;
        let adj = tokio::sync::Mutex::new(AdjudicationChannel::new(
            crate::api::evorule_client::EvoruleApiClient::new(&server.url()),
            "default",
        ));
        let manifests = |n: &str| lookup_static(n);
        let focus = FocusSnapshot::from_names(["file_create"]);
        let args = serde_json::json!({"path": "rel.txt"});
        let traces = poisoned_traces();
        let mut deps = deps_for(&UnreachableExecutor, &adj, &manifests);
        deps.traces = Some(traces.as_ref());
        let out = ToolExecutionPipeline
            .execute(req_for("file_create", &args, &focus), deps)
            .await;
        let denial = denial_of(&out);
        assert_eq!(denial.stage, DenialStage::Ledger);
        assert!(denial.reason.contains("tool trace write failed"));
        assert_eq!(out.ledger.denial, Some(DenialStage::Ledger));
    }

    #[tokio::test]
    async fn poisoned_trace_lock_after_execution_keeps_result_visible() {
        // 执行臂轨迹锁中毒：执行已发生，真实结局必须回喂（拒绝会诱导重复
        // 执行）；账面缺口仅 error 留痕，结局/账面不受影响。
        let mut server = mockito::Server::new_async().await;
        mock_allow_once(&mut server).await;
        let adj = tokio::sync::Mutex::new(AdjudicationChannel::new(
            crate::api::evorule_client::EvoruleApiClient::new(&server.url()),
            "default",
        ));
        let manifests = |n: &str| lookup_static(n);
        let focus = FocusSnapshot::from_names(["file_create"]);
        let args = serde_json::json!({"path": "rel.txt"});
        let traces = poisoned_traces();
        let mut deps = deps_for(&OkExecutor, &adj, &manifests);
        deps.traces = Some(traces.as_ref());
        let out = ToolExecutionPipeline
            .execute(req_for("file_create", &args, &focus), deps)
            .await;
        assert!(out.result.is_ok(), "执行结局必须回喂");
        let exec = out.ledger.execution.as_ref().expect("执行结局必须入账");
        assert!(exec.ok);
    }
}
