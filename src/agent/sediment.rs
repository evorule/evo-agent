// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! C1 会话沉淀通道 —— 会话结束时一次性完成「当前 → 中期 → 长期」。
//!
//! ## 设计动机
//!
//! 会话结束时（Stable/Error 分支），除了已持久化的 messages（短期记忆），
//! 还需要把整段对话的摘要和稳定事实写入共享空间，供后续会话召回。
//!
//! ## 三级沉淀
//!
//! 1. **当前级**：messages 已由 `MessagePersistMode` 在运行时逐条写入（P0）
//! 2. **中期级**：整会话摘要写入 `shared.{ns}.sessions.{sid}.summary`
//! 3. **长期级**：稳定事实写入 `shared.{ns}.stable.{key}`（跨会话共享）
//!
//! C4 的 rollup（合并旧摘要）已实现：合并最旧摘要并标记旧摘要为 rolled_up（L-3），避免重复 rollup/膨胀。
//!
//! ## Best-effort 语义
//!
//! 所有写入操作都是 best-effort：失败时记 `tracing::warn!` 日志，不阻断
//! 会话返回。这与 `MemoryManager::set_scoped` 的 fail-open 语义一致。

use crate::agent::audited_llm::AuditedLlm;
use crate::agent::journal::{JournalEvent, JournalLine};
use crate::agent::memory::{MemoryManager, MemoryRecord, MemoryScope, PersistOutcome};
use crate::agent::memory_event::event::{EventSource, EventType, MemoryEvent};
use crate::agent::memory_event::extraction::{extract_json_from_text, EventExtractor};
use crate::agent::summarizer::ContextSummarizer;
use crate::agent::translator::Message;

/// 沉淀配置
#[derive(Debug, Clone)]
pub struct SedimentConfig {
    /// 命名空间（与 MemoryManager.namespace 一致）
    pub namespace: String,
    /// 是否启用事件提取（memory_type != "none" 时为 true）
    pub enable_event_extraction: bool,
    /// 保留的会话摘要上限（C4 rollup 用）
    pub max_session_summaries: usize,
    /// 注入事件上限（C2 召回用，C1 仅占位）
    pub max_injected_events: usize,
    /// 摘要 rollup 触发阈值（C4 用）
    pub summary_rollup_threshold: usize,
    /// B5：写入 `stable.llm.{model}.*` 域所用的模型标识
    ///
    /// 路径段经消毒（非 `[a-zA-Z0-9-_]` 替换为 `-`）保证单一路径段；
    /// 原始模型名记入 value.source（`llm:{raw}`）。
    pub llm_model_id: String,
    /// 是否启用知识候选提取（会话收尾巩固管线最小版）
    ///
    /// 会话收尾时在摘要/事实/事件产物之外，增一次 sidecar LLM 调用提取
    /// 知识候选（fact/procedure/heuristic/narrative/model 五类），落
    /// `shared.{ns}.knowledge_candidates.{event_id}`（MemoryEvent，
    /// kind=Custom("knowledge_candidate")，即知识候选事件类型的 serde 映射）。
    pub enable_knowledge_extraction: bool,
    /// 触发知识候选提取的最小消息条数（太短会话无知识可提取）
    pub min_messages_for_extraction: usize,
    /// 是否启用知识候选巩固（阶段 5 F-613 完整版第一增量：跨会话聚类→
    /// sidecar 合并提议→Consolidated 落账；缺省开，跟随最小版先例）
    pub enable_consolidation: bool,
    /// 是否启用 journal 摘要投影（跨源注册规格：确定性结构投影，零 LLM；
    /// Recipe sources.journal_digest 数据化开关，缺省关=既有 agent 零影响）
    pub enable_journal_digest: bool,
}

impl Default for SedimentConfig {
    fn default() -> Self {
        Self {
            namespace: "default".to_string(),
            enable_event_extraction: true,
            max_session_summaries: 3,
            max_injected_events: 5,
            summary_rollup_threshold: 10,
            llm_model_id: "unknown".to_string(),
            enable_knowledge_extraction: true,
            min_messages_for_extraction: 4,
            enable_consolidation: true,
            enable_journal_digest: false,
        }
    }
}

/// 沉淀依赖（借用 runner 的各组件）
///
/// 采用借用而非拥有，避免在 `AgentRunner::sediment_session` 中 clone 组件。
/// - `memory`：可变借用 MemoryManager（写入摘要/事实需要 &mut）
/// - `summarizer`：不可变借用（调 LLM 生成摘要，无状态变更）
/// - `extractor`：可变借用（C1 阶段暂未使用，占位供后续事件提取）
pub struct SedimentDeps<'a> {
    /// 内存管理器（写入共享空间）
    pub memory: &'a mut MemoryManager,
    /// 上下文摘要器（None 时不生成摘要）
    pub summarizer: Option<&'a ContextSummarizer>,
    /// 事件提取器（R07/E17 接线后实际使用；None = memory 未启用）
    pub extractor: Option<&'a mut EventExtractor>,
    /// 事件证据链账本（双写——shared 召回 + __memory__ 证据链，
    /// 非 RL-B5 双写：同一数据两个消费面，__memory__ 为权威）
    pub event_store: Option<&'a mut crate::agent::memory_event::store::MemoryEventStore>,
    /// 审计链执行器（None = 不提取——纪律①：知识候选提取属沉淀
    /// 提取面，无审计通路则跳过并 warn，禁止新增直连 provider 调用路径）
    pub auditor: Option<&'a AuditedLlm>,
    /// journal 全量行（会话收尾投影消费；调用方经 `JournalWriter::read_lines`
    /// 预读，读取失败=空集如实降级）
    pub journal_lines: Vec<JournalLine>,
}

/// 沉淀结果
#[derive(Debug, Default)]
pub struct SedimentResult {
    /// 摘要是否成功写入共享空间
    pub summary_written: bool,
    /// 成功写入的稳定事实 key 列表
    pub stable_facts: Vec<String>,
    /// 仅本地 cache 的稳定事实 key 列表（持久化失败 CacheOnly，
    /// 不计入 stable_facts 防虚报成功；由 B3 对账补偿）
    pub stable_facts_cache_only: Vec<String>,
    /// 提取并写入共享账本的事件 ID 列表（R07/E17 接线后实际填充）
    pub events: Vec<String>,
    /// 写入共享账本的知识候选 event_id 列表
    pub knowledge_candidates: Vec<String>,
    /// rollup 是否执行（C4）
    pub rollup_done: bool,
    /// journal 摘要投影是否成功写入共享空间（跨源注册规格）
    pub journal_digest_written: bool,
    /// 巩固产物 event_id 列表（Consolidated 落账；阶段 5 F-613）
    pub knowledge_consolidated: Vec<String>,
}

/// C1 主入口：会话结束时调用（best-effort，错误记日志不阻断）
///
/// # 执行顺序
///
/// 1. 调 `summarizer.summarize_session()` 生成整会话摘要 + 稳定事实（一次 LLM 调用）
/// 2. 摘要 → 共享空间 `write_shared_summary()`
/// 3. 稳定事实 → 共享空间 `set_scoped(Shared, ...)`
/// 4. 事件提取（R07/E17 接线：触发式提取 → 写入 `shared.{ns}.events.*`）
/// 5. rollup 检查（C4 占位，返回 false）
/// 6. 知识候选提取（sidecar 审计调用 → 写入
///    `shared.{ns}.knowledge_candidates.*`，kind=Custom("knowledge_candidate")）
///
/// # 参数
///
/// - `deps`：沉淀依赖（借用 runner 组件）
/// - `cfg`：沉淀配置
/// - `session_id`：evorule 会话 ID
/// - `messages`：本次会话的完整消息历史
pub async fn sediment(
    deps: &mut SedimentDeps<'_>,
    cfg: &SedimentConfig,
    session_id: &str,
    messages: &[Message],
) -> SedimentResult {
    let mut result = SedimentResult::default();

    // 1. 整会话摘要 + 稳定事实（一次 LLM 调用）
    if let Some(summarizer) = deps.summarizer {
        let conv_text = conversation_text(messages);
        match summarizer.summarize_session(&conv_text).await {
            Ok(out) => {
                // 2. summary → 共享空间
                match deps
                    .memory
                    .write_shared_summary(session_id, &out.summary)
                    .await
                {
                    // 缺陷登记项②:CacheOnly 不计入 summary_written（防虚报；B3 对账补偿）
                    Ok(PersistOutcome::Persisted { .. }) => result.summary_written = true,
                    Ok(PersistOutcome::CacheOnly) => tracing::warn!(
                        session_id = %session_id,
                        "sediment: summary persisted cache-only; not counted as written"
                    ),
                    Err(e) => tracing::warn!(error = %e, "sediment: write summary failed"),
                }

                // 3. 稳定事实 → 共享空间（B5：写入 llm 域 `stable.llm.{model}.*`，
                //    与用户/系统域隔离；source 由系统填充为 llm:{raw_model}）
                for fact in &out.stable_facts {
                    let key = format!(
                        "stable.llm.{}.{}",
                        sanitize_model_id(&cfg.llm_model_id),
                        fact.key
                    );
                    let source = format!("llm:{}", cfg.llm_model_id);
                    match deps
                        .memory
                        .set_scoped_with_source(MemoryScope::Shared, &key, &fact.value, &source)
                        .await
                    {
                        // 区分 Persisted/CacheOnly——CacheOnly 不计入 stable_facts 防虚报
                        Ok(PersistOutcome::Persisted { .. }) => {
                            result.stable_facts.push(fact.key.clone())
                        }
                        Ok(PersistOutcome::CacheOnly) => {
                            result.stable_facts_cache_only.push(fact.key.clone())
                        }
                        Err(e) => tracing::warn!(
                            error = %e,
                            key = %fact.key,
                            "sediment: write stable fact failed"
                        ),
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "sediment: summarize_session failed"),
        }
    }

    // 4. 事件提取（R07/E17 接线）
    // 修复前：本步为占位 no-op——`enable_event_extraction` 从未被读取、
    // `extractor` 从未被使用（配置面宣称启用，行为上是空操作，实证报告 §6.3）。
    // 修复后：读取配置开关 + 实际使用 extractor，且写入目标为共享账本
    // `shared.{ns}.events.*`（与召回层 `recall_context` 读取前缀一致——
    // 报告 §6.3 增量结论：仅接线 extractor 而不改写入目标，事件层仍不可达）。
    if cfg.enable_event_extraction {
        if let Some(extractor) = deps.extractor.take() {
            extract_and_store_events(extractor, deps, session_id, messages, &mut result).await;
        }
    }

    // 5. C4 rollup（阈值检查）：合并最旧摘要并标记旧摘要为 rolled_up
    if result.summary_written && cfg.summary_rollup_threshold > 0 {
        match rollup_old_summaries(deps, cfg).await {
            Ok(done) => result.rollup_done = done,
            Err(e) => tracing::warn!(error = %e, "sediment: rollup failed"),
        }
    }

    // 6. 知识候选提取（巩固管线最小版）：sidecar 审计调用 →
    //    候选落 shared.{ns}.knowledge_candidates.*（与 sediment 既有产物并列）
    if cfg.enable_knowledge_extraction {
        extract_knowledge_candidates(deps, cfg, session_id, messages, &mut result).await;
    }

    // 6.5 巩固管线（阶段 5 F-613 完整版第一增量）：跨会话候选确定性聚类 →
    //    sidecar 合并提议 → Consolidated 落账（溯源=consolidates 清单）
    if cfg.enable_consolidation {
        consolidate_knowledge_candidates(deps, cfg, session_id, &mut result).await;
    }

    // 7. journal 摘要投影（跨源注册规格）：确定性结构投影（零 LLM）——
    //    轮目标/步数/工具分布/错误与死路 → shared.{ns}.work.journal.{sid}。
    //    journal 本体「唯一真相源、不进 prompt」纪律不变；检索可达的是
    //    有界派生品，逐字节引用仍以 journal 为准（digest 内附 session 回指）。
    if cfg.enable_journal_digest {
        let lines = std::mem::take(&mut deps.journal_lines);
        if lines.is_empty() {
            tracing::debug!(session_id = %session_id, "sediment: no journal lines; digest skipped");
        } else {
            let digest = build_journal_digest(session_id, &lines);
            if deps.memory.write_journal_digest(session_id, &digest).await {
                result.journal_digest_written = true;
            } else {
                tracing::warn!(
                    session_id = %session_id,
                    "sediment: journal digest write failed (best-effort)"
                );
            }
        }
    }

    result
}

/// journal 摘要投影构造（确定性纯函数；同输入逐字节同输出）。
///
/// 投影面：轮目标（去重截 5 条）/LLM 调用计数（react 与 sidecar 分列）/
/// 工具调用总量与按名分布（字典序）/成功失败计数/错误与死路清单（工具
/// 失败逐条+policy 拦截+审批拒绝计数，seq 序）/收尾状态。零 LLM、
/// 零向量（检索红线内）。
pub(crate) fn build_journal_digest(session_id: &str, lines: &[JournalLine]) -> String {
    let mut goals: Vec<String> = Vec::new();
    let mut llm_total = 0usize;
    let mut llm_react = 0usize;
    let mut tool_by_name: std::collections::BTreeMap<String, usize> = Default::default();
    let mut call_tool: std::collections::BTreeMap<String, String> = Default::default();
    let mut tool_ok = 0usize;
    let mut tool_err: Vec<String> = Vec::new();
    let mut policy_blocked = 0usize;
    let mut approvals_rejected = 0usize;
    let mut ended: Option<(&str, u64, u64)> = None;
    for line in lines {
        match &line.event {
            JournalEvent::TurnStarted { goal, .. } => {
                if !goals.contains(goal) && goals.len() < 5 {
                    goals.push(goal.clone());
                }
            }
            JournalEvent::LlmCalled { purpose, .. } => {
                llm_total += 1;
                if purpose == "react" {
                    llm_react += 1;
                }
            }
            JournalEvent::ToolInvoked { call_id, tool, .. } => {
                *tool_by_name.entry(tool.clone()).or_insert(0) += 1;
                call_tool.insert(call_id.clone(), tool.clone());
            }
            JournalEvent::ToolResult {
                call_id, status, ..
            } => {
                if status == "ok" {
                    tool_ok += 1;
                } else {
                    let tool = call_tool.get(call_id).cloned().unwrap_or_default();
                    tool_err.push(format!("{tool} 调用失败({call_id})"));
                }
            }
            JournalEvent::PolicyJudged { verdict, .. } => {
                if verdict == "blocked" {
                    policy_blocked += 1;
                }
            }
            JournalEvent::ApprovalResolved { decision, .. } => {
                if decision != "approved" {
                    approvals_rejected += 1;
                }
            }
            JournalEvent::TurnEnded {
                status,
                steps,
                duration_ms,
            } => ended = Some((status.as_str(), *steps, *duration_ms)),
            _ => {}
        }
    }
    let tool_total: usize = tool_by_name.values().sum();
    let dist = tool_by_name
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut s = format!("会话 journal 摘要（session {session_id}）。\n");
    if goals.is_empty() {
        s.push_str("轮目标: 无\n");
    } else {
        s.push_str(&format!("轮目标: {}\n", goals.join("；")));
    }
    s.push_str(&format!("LLM 调用: {llm_total}（react {llm_react}）。\n"));
    if tool_total == 0 {
        s.push_str("工具调用: 无。\n");
    } else {
        s.push_str(&format!(
            "工具调用: {tool_total}（成功 {tool_ok} / 失败 {}）；分布: {dist}。\n",
            tool_total - tool_ok
        ));
    }
    let dead_ends = if tool_err.is_empty() && policy_blocked == 0 && approvals_rejected == 0 {
        "无".to_string()
    } else {
        let mut parts = tool_err;
        if policy_blocked > 0 {
            parts.push(format!("policy 拦截 {policy_blocked} 次"));
        }
        if approvals_rejected > 0 {
            parts.push(format!("审批拒绝 {approvals_rejected} 次"));
        }
        parts.join("；")
    };
    s.push_str(&format!("错误与死路: {dead_ends}。\n"));
    match ended {
        Some((status, steps, ms)) => {
            s.push_str(&format!("收尾: {status}，{steps} 步，{ms}ms。\n"));
        }
        None => s.push_str("收尾: 无 turn_ended 记录。\n"),
    }
    s
}

// ===== 巩固管线（阶段 5 F-613 完整版第一增量）=====
//
// 11 号 §六 完整版三步的最小闭环：
// 1. 跨会话候选加载（确定性：账本 knowledge_candidates 家族全量拉取）；
// 2. 聚类（确定性：分词集 Jaccard ≥ 阈值贪心成簇——与矛盾裁决同函数族）；
// 3. sidecar LLM 合并提议（purpose=knowledge_consolidation，审计在链）→
//    Consolidated 事件落账（content.consolidates 清单=溯源一键展开；
//    整数 cause 链接随下一增量，源 id 为字符串键不适用 FactId 通路口径）。
//
// 语义边界：LLM 只做合并提议，落账=规则通路（set_scoped 受信通道，
// Settled 直落——巩固产物是系统沉淀动作而非候选提案，与最小版产物同级）；
// 治理闸行权面（Draft→Active）随治理域演进。

/// 巩固聚类相似阈值（分词集 Jaccard；与矛盾裁决同族口径）
const CONSOLIDATE_SIMILARITY_THRESHOLD: f32 = 0.5;

/// 巩固候选面（从账本 MemoryEvent JSON 解析的投影）
#[derive(Debug, Clone)]
struct CandidateFace {
    event_id: String,
    knowledge_kind: String,
    title: String,
    body: String,
}

/// 贪心聚类（确定性）：按 (kind, event_id) 序遍历，未分配者成种子，
/// 相似 ≥ 阈值者并入；返回簇（成员为原表下标），单元素簇不出（无合并对象）。
fn cluster_candidates(cands: &[CandidateFace], threshold: f32) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..cands.len()).collect();
    order.sort_by(|a, b| {
        cands[*a]
            .knowledge_kind
            .cmp(&cands[*b].knowledge_kind)
            .then(cands[*a].event_id.cmp(&cands[*b].event_id))
    });
    let toks: Vec<Vec<String>> = cands
        .iter()
        .map(|c| {
            let mut t = crate::agent::memory::tokenize_for_match(&format!(
                "{} {}",
                c.title, c.body
            ));
            t.sort();
            t.dedup();
            t
        })
        .collect();
    let sim = |x: usize, y: usize| -> f32 {
        let (sa, sb) = (&toks[x], &toks[y]);
        if sa.is_empty() || sb.is_empty() {
            return 0.0;
        }
        let inter = sa.iter().filter(|t| sb.contains(t)).count();
        let union = sa.len() + sb.len() - inter;
        if union == 0 {
            0.0
        } else {
            inter as f32 / union as f32
        }
    };
    let mut assigned = vec![false; cands.len()];
    let mut clusters = Vec::new();
    for seed in &order {
        let seed = *seed;
        if assigned[seed] {
            continue;
        }
        let mut cluster = vec![seed];
        assigned[seed] = true;
        for &cand in order.iter() {
            if assigned[cand] {
                continue;
            }
            if sim(seed, cand) >= threshold {
                cluster.push(cand);
                assigned[cand] = true;
            }
        }
        if cluster.len() >= 2 {
            clusters.push(cluster);
        }
    }
    clusters
}

/// 巩固合并提议 LLM 输出信封
#[derive(Debug, serde::Deserialize)]
struct ConsolidationOut {
    title: String,
    body: String,
    #[serde(default = "default_consolidation_kind")]
    knowledge_kind: String,
    #[serde(default = "default_candidate_confidence")]
    confidence: f32,
}

fn default_consolidation_kind() -> String {
    "model".to_string()
}

fn parse_consolidation(json_str: &str) -> Result<ConsolidationOut, String> {
    let out: ConsolidationOut =
        serde_json::from_str(json_str).map_err(|e| format!("parse consolidation JSON: {}", e))?;
    if out.title.trim().is_empty() || out.body.trim().is_empty() {
        return Err("consolidation empty title/body".to_string());
    }
    Ok(out)
}

/// 从账本行解析候选面（MemoryEvent JSON；payload 包裹与顶层双兼容）
fn parse_candidate_face(path: &str, value: &serde_json::Value) -> Option<CandidateFace> {
    let face = value
        .get("payload")
        .and_then(|v| v.as_object())
        .unwrap_or(value.as_object()?);
    let content = face.get("content").and_then(|v| v.as_object())?;
    let event_id = path.rsplit('.').next()?.to_string();
    Some(CandidateFace {
        event_id,
        knowledge_kind: content.get("knowledge_kind").and_then(|v| v.as_str())?.to_string(),
        title: content.get("title").and_then(|v| v.as_str())?.to_string(),
        body: content.get("body").and_then(|v| v.as_str())?.to_string(),
    })
}

/// 主入口：跨会话候选巩固（best-effort，审计 sidecar 通路复用纪律①）
async fn consolidate_knowledge_candidates(
    deps: &mut SedimentDeps<'_>,
    cfg: &SedimentConfig,
    session_id: &str,
    result: &mut SedimentResult,
) {
    let auditor = match deps.auditor {
        Some(a) => a,
        None => {
            tracing::warn!(
                session_id = %session_id,
                "sediment: consolidation skipped (no audited LLM path)"
            );
            return;
        }
    };
    let prefix = format!("shared.{}.knowledge_candidates.", deps.memory.namespace());
    let facts = match deps
        .memory
        .evorule_client
        .get_shared_facts(Some(&prefix))
        .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "sediment: consolidation candidate load failed (best-effort skip)"
            );
            return;
        }
    };
    let mut faces: Vec<CandidateFace> = Vec::new();
    for f in &facts {
        if let Some(face) = parse_candidate_face(&f.path, &f.value) {
            faces.push(face);
        }
    }
    if faces.len() < 2 {
        return; // 少于 2 条无合并对象
    }
    let clusters = cluster_candidates(&faces, CONSOLIDATE_SIMILARITY_THRESHOLD);
    for cluster in clusters {
        let sources: Vec<String> =
            cluster.iter().map(|i| faces[*i].event_id.clone()).collect();
        let corpus = cluster
            .iter()
            .map(|i| {
                let c = &faces[*i];
                format!("- [{}] {}: {}", c.knowledge_kind, c.title, c.body)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "以下同主题知识候选经确定性聚类判为可合并。请合并/去重/抽象为一条语义候选,输出 JSON。\n\n候选:\n{corpus}\n\n输出 JSON 格式:\n{{\"title\":\"合并后标题\",\"body\":\"自包含合并正文(须覆盖各候选要点)\",\"knowledge_kind\":\"五类之一\",\"confidence\":0.8}}\n\n只输出 JSON,不要输出其他内容。"
        );
        let mut params_map = serde_json::Map::new();
        params_map.insert(
            "model".to_string(),
            serde_json::Value::String(cfg.llm_model_id.clone()),
        );
        params_map.insert("temperature".to_string(), serde_json::json!(0.0));
        params_map.insert("max_tokens".to_string(), serde_json::json!(1024));
        params_map.insert(
            "messages".to_string(),
            serde_json::json!([
                {"role": "system", "content": "你是知识巩固助手:把同主题候选合并为一条更凝练的语义候选。"},
                {"role": "user", "content": prompt}
            ]),
        );
        let out = match auditor
            .execute(
                "knowledge_consolidation",
                &serde_json::Value::Object(params_map),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "sediment: consolidation LLM call failed (cluster skipped)"
                );
                continue;
            }
        };
        let text = out
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or(&out.to_string())
            .to_string();
        let merged = match parse_consolidation(&text) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "sediment: consolidation parse failed (cluster skipped)"
                );
                continue;
            }
        };
        // Consolidated 事件落账（溯源=content.consolidates 清单一键展开）
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let event_id = format!("CC-{}-{}", sanitize_model_id(session_id), now);
        let mut event = crate::agent::memory_event::event::MemoryEvent::new_root(
            &event_id,
            crate::agent::memory_event::event::EventType::Custom(
                "knowledge_consolidated".to_string(),
            ),
            now,
            crate::agent::memory_event::event::EventSource::LlmExtraction,
        )
        .with_confidence(merged.confidence.clamp(0.0, 1.0))
        .with_tag("consolidated")
        .with_tag(&merged.knowledge_kind)
        .with_session(session_id);
        event.content = serde_json::json!({
            "knowledge_kind": merged.knowledge_kind,
            "title": merged.title,
            "body": merged.body,
            "consolidates": sources,
        });
        let value = match serde_json::to_string(&event) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "sediment: consolidation serialize failed");
                continue;
            }
        };
        let key = format!("knowledge_candidates.consolidated.{}", event_id);
        match deps
            .memory
            .set_scoped(crate::agent::memory::MemoryScope::Shared, &key, &value)
            .await
        {
            Ok(_) => {
                result.knowledge_consolidated.push(event_id.clone());
                if let Some(store) = deps.event_store.as_mut() {
                    if let Err(e) = store.write_event(event).await {
                        tracing::warn!(
                            error = %e,
                            event_id = %event_id,
                            "sediment: consolidation dual-write failed"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "sediment: consolidation write failed");
            }
        }
    }
}

/// B5：模型标识消毒为合法路径段（非 `[a-zA-Z0-9-_]` 替换为 `-`）
///
/// 保证 `stable.llm.{model}.{key}` 中 model 恒为单一路径段；
/// 原始模型名已记录在 value.source（`llm:{raw}`），此处仅影响路径可读性。
fn sanitize_model_id(model: &str) -> String {
    model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

// ===== 知识候选提取（会话收尾巩固管线最小版） =====
//
// 会话收尾时 sediment 产物之外增「知识候选提取」sidecar
// 调用（提示词模板=model 类知识的第一个实例，自举）→ 候选落 MemoryEvent
// （kind=custom:knowledge_candidate）。
//
// 口径映射：方案字面 `custom:knowledge_candidate` →
// `EventType::Custom("knowledge_candidate")`（serde 形态
// `{"kind":"Custom","subtype":"knowledge_candidate"}`）。
//
// 自举：提示词内置一个 model 类知识示例（按 evorule-rule 内置壳
// `builtin:knowledge/model` 的最小结构构造）——即「第一个实例」；系统
// 启动时知识库为空，第一个实例只能编译期内置，未来 Active 条目反哺提示词
// 属后续批次。

/// 知识候选五类（与 evorule-rule 内置域 schema 五件一一对应）
const KNOWLEDGE_KINDS: &[&str] = &["fact", "procedure", "heuristic", "narrative", "model"];

/// 提取系统提示（对齐 EventExtractor 纪律：只提取对话中明确存在的
/// 信息，输出 JSON，无候选返回空列表）
const KNOWLEDGE_EXTRACTION_SYSTEM_PROMPT: &str = "\
你是一个知识候选提取助手。你的任务是从对话中识别值得沉淀为可复用知识的片段,提取为结构化候选。\n\
\n\
知识分五类:\n\
- fact: 客观事实(某配置项含义/某接口行为/某约束存在)\n\
- procedure: 操作步骤(如何完成某任务的步骤序列)\n\
- heuristic: 经验法则(什么情况下用什么方法更好/避坑经验)\n\
- narrative: 叙事性知识(决策背景/来龙去脉)\n\
- model: 概念模型(对某事物的结构化理解,如某机制的工作原理)\n\
\n\
严格约束:\n\
1. 只提取对话中明确存在的信息,不能编造或臆测\n\
2. 每条候选必须自包含(脱离对话上下文仍可读)\n\
3. 输出必须是 JSON 格式,不要输出自然语言解释\n\
4. knowledge_kind 只能取五类之一\n\
5. 如果对话中没有值得沉淀的知识,返回 {\"candidates\": []}";

/// model 类知识示例——提示词模板的「第一个实例」（自举）
const MODEL_EXAMPLE: &str = r#"{"knowledge_kind":"model","title":"规则条目生命周期模型","body":"规则条目按状态机演进:Draft(草稿,仅作者可见)→Candidate(候选,待审)→Active(生效,可被检索注入)→Published(发布,归档)。状态迁移必经治理闸,每次迁移落 StateChange 审计事实。","tags":["lifecycle","governance"],"confidence":0.9}"#;

/// 单条知识候选 LLM 输出结构
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct KnowledgeCandidateOut {
    /// 知识类别（五类之一；越界候选在解析后丢弃）
    pub knowledge_kind: String,
    /// 候选标题（自包含短语）
    pub title: String,
    /// 候选正文（自包含、脱离上下文可读）
    pub body: String,
    /// 自由标签
    #[serde(default)]
    pub tags: Vec<String>,
    /// 置信度 0.0-1.0（LLM 自评，缺失默认 0.5——提取性内容低于用户直述）
    #[serde(default = "default_candidate_confidence")]
    pub confidence: f32,
}

fn default_candidate_confidence() -> f32 {
    0.5
}

/// LLM 输出信封（candidates 缺省=无候选）
#[derive(Debug, serde::Deserialize)]
struct KnowledgeExtractionOut {
    #[serde(default)]
    candidates: Vec<KnowledgeCandidateOut>,
}

/// 知识类别白名单判定（A2-1 提取面过滤与 A2-2 memory_propose 结构闸共用）
pub fn is_valid_knowledge_kind(kind: &str) -> bool {
    KNOWLEDGE_KINDS.contains(&kind)
}

/// 知识候选 event_id 生成（KC- 前缀 + 会话消毒段 + 秒级时间戳 + 批内序号；
/// 提取路与工具路共用单点，保证命名零漂移）
pub fn knowledge_candidate_event_id(session_id: &str, now: u64, seq: usize) -> String {
    format!("KC-{}-{}-{}", sanitize_model_id(session_id), now, seq)
}

/// 共用构造：知识候选 → MemoryEvent（A2-1 sediment 提取路与 A2-2
/// memory_propose 工具路**同一构造单点**，两路产物逐字段同构）。
///
/// 产物形态：`Custom("knowledge_candidate")` + `EventSource::LlmExtraction` +
/// confidence clamp[0,1] + tags=[knowledge_candidate, kind, ...候选自带] +
/// session 锚 + content={knowledge_kind,title,body}。
/// A2-2 工具路的 `llm_generated` 旗标由 [`mark_llm_generated`] 在本函数
/// 产物之上强制追加（提取路产物不带该旗标——sediment 侧车调用本身也是
/// LLM 提取，但其产物语义=系统沉淀动作，旗标口径以 A2-1 落地为准）。
pub fn build_knowledge_candidate_event(
    event_id: &str,
    session_id: &str,
    now: u64,
    cand: &KnowledgeCandidateOut,
) -> MemoryEvent {
    let mut event = MemoryEvent::new_root(
        event_id,
        // 方案字面 custom:knowledge_candidate → Custom("knowledge_candidate")
        EventType::Custom("knowledge_candidate".to_string()),
        now,
        EventSource::LlmExtraction,
    )
    .with_confidence(cand.confidence.clamp(0.0, 1.0))
    .with_tag("knowledge_candidate")
    .with_tag(&cand.knowledge_kind)
    .with_session(session_id);
    event.content = serde_json::json!({
        "knowledge_kind": cand.knowledge_kind,
        "title": cand.title,
        "body": cand.body,
    });
    for t in &cand.tags {
        if !t.trim().is_empty() {
            event = event.with_tag(t);
        }
    }
    event
}

/// A2-2（F-611 写件）：`llm_generated` 旗标强制——content 字段 + tag 双落，
/// 由系统写死（工具参数 schema 不收该字段，LLM 无法伪造 human 来源）。
pub fn mark_llm_generated(mut event: MemoryEvent) -> MemoryEvent {
    event.content["llm_generated"] = serde_json::Value::Bool(true);
    event.with_tag("llm_generated")
}

/// 主入口：会话收尾时提取知识候选并写入共享账本（best-effort）
///
/// 流程：
/// 1. 前置闸：消息数 < `min_messages_for_extraction` → 跳过；
/// 2. 无 auditor → 跳过 + warn（纪律①：无审计通路不提取，不直连 provider）；
/// 3. 整会话一次 sidecar LLM 调用（purpose=knowledge_candidate_extraction，
///    prompt/response 全文入审计链）；
/// 4. 解析候选（五类越界/正文空丢弃）；
/// 5. 每候选一个 MemoryEvent 写入 `shared.{ns}.knowledge_candidates.{event_id}`
///    （event_id 前缀 KC-，与事件提取 E- 风格对齐）+ __memory__ 证据链双写。
async fn extract_knowledge_candidates(
    deps: &mut SedimentDeps<'_>,
    cfg: &SedimentConfig,
    session_id: &str,
    messages: &[Message],
    result: &mut SedimentResult,
) {
    // 前置闸：太短会话无知识可提取（省一次 LLM 调用）
    if messages.len() < cfg.min_messages_for_extraction {
        return;
    }
    // 纪律①：知识候选提取属沉淀提取面，必须经审计 sidecar；
    // 无审计通路则跳过（不新增直连 provider 调用路径）。
    let auditor = match deps.auditor {
        Some(a) => a,
        None => {
            tracing::warn!(
                session_id = %session_id,
                "sediment: knowledge extraction skipped (no audited LLM path)"
            );
            return;
        }
    };

    let conversation = conversation_text(messages);
    let prompt = format!(
        "请从以下对话中提取值得沉淀为知识候选的片段,输出 JSON 格式。\n\n\
         对话:\n{}\n\n\
         输出 JSON 格式(候选列表,可为空):\n\
         {{\"candidates\": [{{\n\
           \"knowledge_kind\": \"model\",\n\
           \"title\": \"候选标题\",\n\
           \"body\": \"自包含正文\",\n\
           \"tags\": [\"标签\"],\n\
           \"confidence\": 0.8\n\
         }}]}}\n\n\
         model 类候选示例(输出参照此实例的结构与颗粒度):\n{}\n\n\
         knowledge_kind 可取: {}\n\
         只输出 JSON,不要输出其他内容。",
        conversation,
        MODEL_EXAMPLE,
        KNOWLEDGE_KINDS.join(", ")
    );

    let mut messages_vec: Vec<serde_json::Value> = Vec::with_capacity(2);
    messages_vec.push(serde_json::json!({
        "role": "system",
        "content": KNOWLEDGE_EXTRACTION_SYSTEM_PROMPT,
    }));
    messages_vec.push(serde_json::json!({
        "role": "user",
        "content": prompt,
    }));
    let mut params_map = serde_json::Map::new();
    // server 治理声明要求 instruction.params.model 必须存在（on_missing=error），
    // sidecar 命令缺 model 会被规则拒收（path_not_found，E2E 实证）。
    // 取 B5 既有权威 llm_model_id（summary_model 回退主模型），与摘要调用同一模型口径。
    params_map.insert(
        "model".to_string(),
        serde_json::Value::String(cfg.llm_model_id.clone()),
    );
    params_map.insert(
        "temperature".to_string(),
        serde_json::json!(0.0), // temperature=0 保证最大确定性（对齐事件提取）
    );
    params_map.insert("max_tokens".to_string(), serde_json::json!(1024));
    params_map.insert(
        "messages".to_string(),
        serde_json::Value::Array(messages_vec),
    );

    // sidecar 审计调用：prompt/response 全文入 evorule 审计链（纪律①）
    let audited_result = match auditor
        .execute(
            "knowledge_candidate_extraction",
            &serde_json::Value::Object(params_map),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                session_id = %session_id,
                "sediment: knowledge candidate extraction LLM call failed"
            );
            return;
        }
    };

    // 解析审计回包（形态与直连一致：LlmResponse JSON → content → 内嵌 JSON）
    let response: crate::agent::translator::LlmResponse =
        match serde_json::from_str(&audited_result.to_string()) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    session_id = %session_id,
                    "sediment: parse audited LLM response failed"
                );
                return;
            }
        };
    let json_str = extract_json_from_text(&response.content);
    let candidates = match parse_knowledge_candidates(&json_str) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                error = %e,
                session_id = %session_id,
                "sediment: parse knowledge candidates failed"
            );
            return;
        }
    };
    if candidates.is_empty() {
        return; // 无候选是正常路径
    }

    write_knowledge_candidates(deps, session_id, candidates, result).await;
}

/// 解析 LLM 输出为候选列表（纯函数，单测覆盖）
fn parse_knowledge_candidates(json_str: &str) -> Result<Vec<KnowledgeCandidateOut>, String> {
    let out: KnowledgeExtractionOut =
        serde_json::from_str(json_str).map_err(|e| format!("parse candidates JSON: {}", e))?;
    // 逐条过滤：五类越界/正文空 → 丢弃（治理口径：越界候选将来过不了
    // A2-3 契约校验，直接在提取面拒收并留 warn）
    let kept: Vec<KnowledgeCandidateOut> = out
        .candidates
        .into_iter()
        .filter(|c| {
            let kind_ok = is_valid_knowledge_kind(&c.knowledge_kind);
            let body_ok = !c.body.trim().is_empty() && !c.title.trim().is_empty();
            if !kind_ok {
                tracing::warn!(kind = %c.knowledge_kind, "sediment: drop candidate (unknown knowledge_kind)");
            } else if !body_ok {
                tracing::warn!(kind = %c.knowledge_kind, "sediment: drop candidate (empty title/body)");
            }
            kind_ok && body_ok
        })
        .collect();
    Ok(kept)
}

/// 候选写入共享账本 + __memory__ 证据链双写（best-effort）
async fn write_knowledge_candidates(
    deps: &mut SedimentDeps<'_>,
    session_id: &str,
    candidates: Vec<KnowledgeCandidateOut>,
    result: &mut SedimentResult,
) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for (seq, cand) in candidates.into_iter().enumerate() {
        // 事件 ID：会话内唯一 + 路径安全（KC- 前缀与事件提取 E- 风格对齐）
        let event_id = knowledge_candidate_event_id(session_id, now, seq);
        // 事件构造收敛共用单点（A2-2 工具路同构零漂移）
        let event = build_knowledge_candidate_event(&event_id, session_id, now, &cand);

        let value = match serde_json::to_string(&event) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, event_id = %event_id, "sediment: serialize knowledge candidate failed");
                continue;
            }
        };
        let key = format!("knowledge_candidates.{}", event_id);
        match deps
            .memory
            .set_scoped(MemoryScope::Shared, &key, &value)
            .await
        {
            Ok(_) => {
                result.knowledge_candidates.push(event_id.clone());
                // 双写到 __memory__ 证据链（与事件提取同型）
                if let Some(store) = deps.event_store.as_mut() {
                    if let Err(e) = store.write_event(event.clone()).await {
                        tracing::warn!(
                            error = %e,
                            event_id = %event_id,
                            "sediment: knowledge candidate dual-write failed"
                        );
                    }
                }
            }
            Err(e) => tracing::warn!(
                error = %e,
                event_id = %event_id,
                "sediment: write knowledge candidate to shared ledger failed"
            ),
        }
    }
}

/// R07（E17 接线）：扫描会话消息，触发式提取结构化事件并写入共享账本
///
/// 写入目标 = `shared.{ns}.events.{event_id}`：经 `set_scoped(Shared)` →
/// 会话 payload 更新 + 服务端 P3 广播进共享表，落点正是召回层
/// `recall_context` 读取的 `shared.{ns}.events.` 前缀（E17 的实现级阻断点）。
///
/// 流程（Q13 方案 C）：
/// 1. 逐条 User 消息 `detect_trigger`（显式短语/关键词，纯文本，不调 LLM）；
/// 2. 命中 → LLM 提取结构化字段（temperature=0，`extract_from_conversation`）；
/// 3. `MemoryEvent` 全量序列化进 `MemoryRecord.value` 写入共享账本
///    （保留结构化字段供回放/因果链；R05 的 CJK 分词对 JSON 文本同样可命中）。
///
/// 全程 best-effort：LLM 判定"无事件"（`Custom("none")`）是正常路径走 debug；
/// 其余失败 warn 留痕，不阻断会话返回。
async fn extract_and_store_events(
    extractor: &mut EventExtractor,
    deps: &mut SedimentDeps<'_>,
    session_id: &str,
    messages: &[Message],
    result: &mut SedimentResult,
) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut seq = 0usize;

    for (i, msg) in messages.iter().enumerate() {
        let Message::User { content } = msg else {
            continue;
        };
        // 触发检测（显式/关键词，纯文本匹配，不调 LLM）
        if extractor.detect_trigger(content).is_none() {
            continue;
        }
        // assistant 上下文 = 紧随其后的 Assistant 消息（如有）
        let assistant = messages.get(i + 1).and_then(|m| match m {
            Message::Assistant { content, .. } => Some(content.as_str()),
            _ => None,
        });
        // 事件 ID：会话内唯一 + 路径安全（复用模型名消毒保证单一路径段）
        let event_id = format!("E-{}-{}-{}", sanitize_model_id(session_id), now, seq);
        seq += 1;

        match extractor
            .extract_from_conversation(content, assistant, &event_id, now)
            .await
        {
            Ok(Some(event)) => {
                let key = format!("events.{}", event.event_id);
                let value = match serde_json::to_string(&event) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            event_id = %event.event_id,
                            "sediment: serialize event failed"
                        );
                        continue;
                    }
                };
                match deps
                    .memory
                    .set_scoped(MemoryScope::Shared, &key, &value)
                    .await
                {
                    Ok(_) => {
                        result.events.push(event.event_id.clone());
                        // 双写事件到 __memory__ 证据链（如果 event_store 可用）
                        if let Some(store) = deps.event_store.as_mut() {
                            if let Err(e) = store.write_event(event.clone()).await {
                                tracing::warn!(
                                    error = %e,
                                    event_id = %event.event_id,
                                    "sediment: MemoryEventStore dual-write failed"
                                );
                            }
                        }
                    }
                    Err(e) => tracing::warn!(
                        error = %e,
                        event_id = %event.event_id,
                        "sediment: write event to shared ledger failed"
                    ),
                }
            }
            Ok(None) => {}
            Err(e) => {
                if e.contains("no event") {
                    tracing::debug!(event_id = %event_id, "sediment: no event extracted");
                } else {
                    tracing::warn!(
                        error = %e,
                        event_id = %event_id,
                        "sediment: event extraction failed"
                    );
                }
            }
        }
    }
}

/// 把消息列表拼接为纯文本对话（供 LLM 摘要）
///
/// 格式：`Role: content\n` 逐行拼接。
/// role 映射：System/User/Assistant/Tool（首字母大写）。
fn conversation_text(messages: &[Message]) -> String {
    let mut text = String::new();
    for msg in messages {
        let role = match msg {
            Message::System { .. } => "System",
            Message::User { .. } => "User",
            Message::Assistant { .. } => "Assistant",
            Message::Tool { .. } => "Tool",
        };
        text.push_str(&format!("{}: {}\n", role, msg.content()));
    }
    text
}

/// C4 第三级：共享空间 sessions.* 摘要数超阈值 → 合并最旧若干为 rollup
///
/// 当共享空间的普通会话摘要（排除 `.rollup.` 路径）数量达到
/// `summary_rollup_threshold` 时，取最旧的 `threshold/2` 条调用
/// `ContextSummarizer::rollup_summaries` 合并为一条 rollup 摘要，
/// 写入 `{ns}.sessions.rollup.{ts}` 路径（recall 时被排除出普通摘要名额）。
///
/// 读取共享账本失败时 fail-open 返回 `Ok(false)`（与 `recall_context` 一致）。
async fn rollup_old_summaries(
    deps: &mut SedimentDeps<'_>,
    cfg: &SedimentConfig,
) -> Result<bool, String> {
    // 进程内串行化——读-合并-标记五步非原子,跨会话并发 sediment
    // 会同批双 rollup(近似摘要双写+LLM 成本双花);进程级互斥消除交错。
    // 诚实边界:跨进程并发归 server 侧归属(登记维持)。
    static ROLLUP_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _rollup_guard = ROLLUP_GUARD.lock().await;
    let sessions_prefix = format!("shared.{}.sessions.", cfg.namespace);
    // fail-open：读取失败视为无摘要，不触发 rollup
    let facts = match deps
        .memory
        .evorule_client
        .get_shared_facts(Some(&sessions_prefix))
        .await
    {
        Ok(f) => f,
        Err(_) => return Ok(false),
    };

    // 过滤掉 rollup 路径，只保留普通摘要
    let regular: Vec<_> = facts
        .into_iter()
        .filter(|f| !f.path.contains(".rollup."))
        .collect();

    if regular.len() < cfg.summary_rollup_threshold {
        return Ok(false); // 未超阈值，不需要 rollup
    }

    // 解析为 (fact_id, timestamp, value) 并按时间正序（最旧在前）
    // 保留 fact_id 以便合并后通过 mark_as_rollup 标记旧摘要为 rolled_up（L-3 修复）
    let mut summaries: Vec<(u64, u64, String)> = regular
        .into_iter()
        .filter_map(|f| {
            serde_json::from_value::<MemoryRecord>(f.value.clone())
                .ok()
                .map(|r| (f.fact_id, r.timestamp, r.value))
        })
        .collect();
    summaries.sort_by_key(|(_, ts, _)| *ts);

    // 取最旧的 threshold/2 条进行合并
    let rollup_count = cfg.summary_rollup_threshold / 2;
    let to_rollup: Vec<String> = summaries
        .iter()
        .take(rollup_count)
        .map(|(_, _, s)| s.clone())
        .collect();
    // 被合并旧摘要的 fact_id，写 rollup 后标记 rolled_up 以阻止重复 rollup/膨胀（L-3）
    let rolled_fact_ids: Vec<u64> = summaries
        .iter()
        .take(rollup_count)
        .map(|(id, _, _)| *id)
        .collect();

    if to_rollup.is_empty() {
        return Ok(false);
    }

    // 需要 summarizer 来合并
    let summarizer = match deps.summarizer {
        Some(s) => s,
        None => return Ok(false),
    };
    let rolled = summarizer.rollup_summaries(&to_rollup).await?;

    // 写入 rollup 摘要到共享空间（路径含 .rollup. ，recall 时排除出普通名额）
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let rollup_path = format!("sessions.rollup.{}", ts);
    deps.memory
        .set_scoped(MemoryScope::Shared, &rollup_path, &rolled)
        .await
        .map_err(|e| format!("write rollup failed: {}", e))?;

    // L-3 修复：标记被合并的旧摘要为 rolled_up，
    // 使其从 server 端 facts_by_path_prefix 查询结果中过滤，
    // 避免下次仍计入阈值、反复 rollup 造成共享空间膨胀。
    // best-effort：失败不阻断会话返回。
    // F4（audit-chain 专项 2026-08-28）：失败重试 1 次——标记缺失会导致
    // 同批旧摘要下轮再次 rollup（浪费 + 审计噪声），一次重试可消除大部分
    // 瞬态错误造成的重复合并；仍失败才 warn（现状语义保留）。
    if !rolled_fact_ids.is_empty() {
        let mut marked = deps
            .memory
            .evorule_client
            .mark_shared_facts_rollup(&rolled_fact_ids)
            .await
            .is_ok();
        if !marked {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            marked = deps
                .memory
                .evorule_client
                .mark_shared_facts_rollup(&rolled_fact_ids)
                .await
                .is_ok();
        }
        if !marked {
            tracing::warn!(
                ids = ?rolled_fact_ids,
                "sediment: mark old summaries as rolled_up failed after retry (best-effort) — 同批旧摘要下轮可能再次 rollup"
            );
        }
    }

    Ok(true)
}

#[cfg(test)]
mod tests {

    #[test]
    fn probe_sim_detail() {
        let mut a = crate::agent::memory::tokenize_for_match("登录超时阈值 登录超时阈值为 30 秒,超时即断开");
        a.sort(); a.dedup();
        let mut b = crate::agent::memory::tokenize_for_match("登录超时阈值说明 登录超时阈值 30 秒的相关说明");
        b.sort(); b.dedup();
        println!("a={a:?}");
        println!("b={b:?}");
        let inter = a.iter().filter(|t| b.contains(t)).count();
        println!("inter={inter} union={}", a.len() + b.len() - inter);
    }



    use super::*;
    use crate::agent::translator::Message;
    use crate::io_handlers::LlmHandler;

    fn make_test_client() -> crate::api::evorule_client::EvoruleApiClient {
        crate::api::evorule_client::EvoruleApiClient::new("http://localhost:8080")
    }


    #[test]
    fn test_cluster_candidates_greedy_deterministic() {
        // 巩固聚类:同主题成簇、异主题孤立(单元素簇不出)、同输入同簇序
        let face = |id: &str, kind: &str, title: &str, body: &str| CandidateFace {
            event_id: id.to_string(),
            knowledge_kind: kind.to_string(),
            title: title.to_string(),
            body: body.to_string(),
        };
        let cands = vec![
            face("KC-1", "fact", "登录超时阈值", "登录超时阈值为 30 秒,超时即断开"),
            face("KC-2", "fact", "登录超时阈值说明", "登录超时阈值 30 秒的相关说明"),
            face("KC-3", "fact", "数据库连接池", "连接池大小默认为 10"),
        ];
        let c1 = cluster_candidates(&cands, 0.3);
        let c2 = cluster_candidates(&cands, 0.3);
        assert_eq!(c1, c2, "确定性:同输入同簇序");
        assert_eq!(c1.len(), 1, "同主题两候选成簇,异主题孤立");
        assert_eq!(c1[0].len(), 2);
    }

    #[test]
    fn test_parse_consolidation_output() {
        let out = parse_consolidation(
            r#"{"title":"合并标题","body":"合并正文","knowledge_kind":"fact","confidence":0.9}"#,
        )
        .unwrap();
        assert_eq!(out.title, "合并标题");
        assert_eq!(out.knowledge_kind, "fact");
        // 空标题拒绝
        assert!(parse_consolidation(
            r#"{"title":"","body":"x"}"#
        )
        .is_err());
    }

    #[tokio::test]
    async fn test_consolidate_skips_small_candidate_set() {
        // 候选 <2 → 无合并对象直接返回(不调 LLM 不写账;离线客户端安全)
        let mut mgr = MemoryManager::new("ns", make_test_client());
        let mut recipe = crate::agent::recipe::MemoryRecipe::default();
        mgr.set_recipe(recipe.clone());
        let mut cfg = SedimentConfig::default();
        cfg.enable_consolidation = true;
        let llm = LlmHandler::mock(r#"{"title":"x","body":"y"}"#);
        let auditor = AuditedLlm::new(make_test_client(), llm);
        let mut deps = SedimentDeps {
            memory: &mut mgr,
            summarizer: None,
            extractor: None,
            event_store: None,
            auditor: Some(&auditor),
            journal_lines: Vec::new(),
        };
        let mut result = SedimentResult::default();
        // 账本加载离线失败 → best-effort skip(不 panic 不写账)
        consolidate_knowledge_candidates(&mut deps, &cfg, "s1", &mut result).await;
        assert!(result.knowledge_consolidated.is_empty());
    }

    #[test]
    fn test_build_journal_digest_deterministic_projection() {
        // 跨源批 C:journal 摘要投影构造器——确定性纯函数,同输入逐字节同输出
        let lines = vec![
            JournalLine {
                seq: 1,
                ts: 1,
                event: JournalEvent::TurnStarted {
                    turn_seq: 1,
                    goal: "部署服务".into(),
                },
            },
            JournalLine {
                seq: 2,
                ts: 2,
                event: JournalEvent::LlmCalled {
                    model: "m".into(),
                    purpose: "react".into(),
                    evorule_request_id: None,
                    tokens: None,
                    tokens_est: Some(100),
                    request: 3,
                    response: "r".into(),
                },
            },
            JournalLine {
                seq: 3,
                ts: 3,
                event: JournalEvent::ToolInvoked {
                    call_id: "t3".into(),
                    tool: "shell_exec".into(),
                    args_digest: "d".into(),
                    evorule_request_id: None,
                },
            },
            JournalLine {
                seq: 4,
                ts: 4,
                event: JournalEvent::ToolResult {
                    call_id: "t3".into(),
                    status: "error".into(),
                    size_bytes: 0,
                    content_digest: "d".into(),
                },
            },
            JournalLine {
                seq: 5,
                ts: 5,
                event: JournalEvent::ToolInvoked {
                    call_id: "t5".into(),
                    tool: "file_read".into(),
                    args_digest: "d".into(),
                    evorule_request_id: None,
                },
            },
            JournalLine {
                seq: 6,
                ts: 6,
                event: JournalEvent::ToolResult {
                    call_id: "t5".into(),
                    status: "ok".into(),
                    size_bytes: 10,
                    content_digest: "d".into(),
                },
            },
            JournalLine {
                seq: 7,
                ts: 7,
                event: JournalEvent::TurnEnded {
                    status: "success".into(),
                    steps: 2,
                    duration_ms: 800,
                },
            },
        ];
        let d1 = build_journal_digest("s1", &lines);
        let d2 = build_journal_digest("s1", &lines);
        assert_eq!(d1, d2, "确定性:同输入逐字节同输出");
        assert!(d1.contains("session s1"));
        assert!(d1.contains("轮目标: 部署服务"));
        assert!(d1.contains("LLM 调用: 1（react 1）"));
        assert!(d1.contains("工具调用: 2（成功 1 / 失败 1）"));
        // 按名分布字典序(确定性)
        assert!(d1.contains("file_read=1, shell_exec=1"));
        // 错误与死路:失败工具逐条(call_id 回指)
        assert!(d1.contains("shell_exec 调用失败(t3)"));
        assert!(d1.contains("收尾: success，2 步，800ms。"));
        // 空 journal → 各字段如实「无」(fail-visible 不虚构)
        let empty = build_journal_digest("s2", &[]);
        assert!(empty.contains("轮目标: 无"));
        assert!(empty.contains("工具调用: 无。"));
        assert!(empty.contains("错误与死路: 无。"));
        assert!(empty.contains("收尾: 无 turn_ended 记录。"));
    }

    #[test]
    fn test_sediment_config_default() {
        let cfg = SedimentConfig::default();
        assert_eq!(cfg.namespace, "default");
        assert!(cfg.enable_event_extraction);
        assert_eq!(cfg.max_session_summaries, 3);
        assert_eq!(cfg.max_injected_events, 5);
        assert_eq!(cfg.summary_rollup_threshold, 10);
        assert_eq!(cfg.llm_model_id, "unknown");
        // 知识候选提取默认开、最短会话 4 条
        assert!(cfg.enable_knowledge_extraction);
        assert_eq!(cfg.min_messages_for_extraction, 4);
    }

    #[test]
    fn test_sanitize_model_id() {
        assert_eq!(sanitize_model_id("gpt-4o"), "gpt-4o");
        assert_eq!(sanitize_model_id("deepseek-chat"), "deepseek-chat");
        // 带点的模型名消毒为单一路径段
        assert_eq!(sanitize_model_id("gpt-4.1"), "gpt-4-1");
        assert_eq!(sanitize_model_id("qwen/max"), "qwen-max");
        assert_eq!(sanitize_model_id(""), "");
    }

    #[test]
    fn test_sediment_config_clone() {
        let cfg = SedimentConfig::default();
        let cloned = cfg.clone();
        assert_eq!(cfg.namespace, cloned.namespace);
        assert_eq!(
            cfg.summary_rollup_threshold,
            cloned.summary_rollup_threshold
        );
    }

    #[test]
    fn test_sediment_result_default() {
        let result = SedimentResult::default();
        assert!(!result.summary_written);
        assert!(result.stable_facts.is_empty());
        assert!(result.events.is_empty());
        assert!(result.knowledge_candidates.is_empty());
        assert!(!result.rollup_done);
    }

    #[test]
    fn test_conversation_text_with_messages() {
        let messages = vec![
            Message::System {
                content: "You are helpful".to_string(),
            },
            Message::User {
                content: "Hello".to_string(),
            },
            Message::Assistant {
                content: "Hi there".to_string(),
                tool_calls: None,
            },
            Message::Tool {
                content: "result".to_string(),
                tool_name: "search".to_string(),
            },
        ];
        let text = conversation_text(&messages);
        assert!(text.contains("System: You are helpful"));
        assert!(text.contains("User: Hello"));
        assert!(text.contains("Assistant: Hi there"));
        assert!(text.contains("Tool: result"));
    }

    #[test]
    fn test_conversation_text_empty() {
        let messages: Vec<Message> = Vec::new();
        let text = conversation_text(&messages);
        assert!(text.is_empty());
    }

    #[tokio::test]
    async fn test_rollup_old_summaries_below_threshold() {
        // 无服务器或摘要数低于阈值时返回 Ok(false)（fail-open，不触发 rollup）
        use crate::api::evorule_client::EvoruleApiClient;
        let client = EvoruleApiClient::new("http://localhost:8080");
        let mut memory = MemoryManager::new("test", client).with_session_id("s1");
        let cfg = SedimentConfig::default();
        let mut deps = SedimentDeps {
            memory: &mut memory,
            summarizer: None,
            extractor: None,
            event_store: None,
            auditor: None,
            journal_lines: Vec::new(),
        };
        let result = rollup_old_summaries(&mut deps, &cfg).await;
        assert!(result.is_ok(), "below threshold / no server should be Ok");
        assert!(!result.unwrap(), "should not trigger rollup");
    }

    #[tokio::test]
    async fn test_sediment_no_summarizer_returns_empty() {
        // 没有 summarizer 时，sediment 应返回空结果（不 panic）
        use crate::api::evorule_client::EvoruleApiClient;
        let client = EvoruleApiClient::new("http://localhost:8080");
        let mut memory = MemoryManager::new("test", client).with_session_id("s1");
        let cfg = SedimentConfig::default();
        let mut deps = SedimentDeps {
            memory: &mut memory,
            summarizer: None,
            extractor: None,
            event_store: None,
            auditor: None,
            journal_lines: Vec::new(),
        };
        let messages = vec![Message::User {
            content: "hello".to_string(),
        }];
        let result = sediment(&mut deps, &cfg, "s1", &messages).await;
        assert!(!result.summary_written);
        assert!(result.stable_facts.is_empty());
        assert!(!result.rollup_done);
    }

    // ===== R07（E17 接线）：事件提取 → shared.{ns}.events.* =====

    #[tokio::test]
    async fn test_sediment_extracts_and_writes_events() {
        let mut server = mockito::Server::new_async().await;
        use crate::api::evorule_client::EvoruleApiClient;
        use crate::io_handlers::LlmHandler;
        let client = EvoruleApiClient::new(&server.url());
        let mut memory = MemoryManager::new("test", client).with_session_id("s1");

        let cfg = SedimentConfig {
            namespace: "test".to_string(),
            ..Default::default()
        };
        let mock_response = r#"{"event_type":{"kind":"Milestone","subtype":"Birthday"},"entities":[],"content":{"summary":"用户生日"},"emotion":null,"tags":["birthday"]}"#;
        let mut extractor = EventExtractor::with_defaults(LlmHandler::mock(mock_response));
        let mut deps = SedimentDeps {
            memory: &mut memory,
            summarizer: None,
            extractor: Some(&mut extractor),
            event_store: None,
            auditor: None,
            journal_lines: Vec::new(),
        };
        let messages = vec![
            Message::User {
                content: "今天是我生日".to_string(),
            },
            Message::Assistant {
                content: "生日快乐！".to_string(),
                tool_calls: None,
            },
        ];

        // set_scoped(Shared) → 会话 payload 更新（P3 广播由服务端完成）。
        // 请求体断言双重点：路径落在共享账本 events 域（E17 实现级阻断点）+
        // value 携带完整 MemoryEvent 序列化内容。
        let m1 = server
            .mock("POST", "/api/sessions/s1/payload")
            .with_status(200)
            .match_body(mockito::Matcher::Regex(
                r#"shared\.test\.events\.E-s1-\d+-0[\s\S]*Milestone[\s\S]*用户生日"#.to_string(),
            ))
            .create_async()
            .await;

        let result = sediment(&mut deps, &cfg, "s1", &messages).await;

        // E17 主断言：事件被提取并写入（修复前 result.events 恒为空）
        assert_eq!(result.events.len(), 1, "关键词触发的事件应被提取");
        assert!(result.events[0].starts_with("E-s1-"));
        m1.assert_async().await;
    }

    #[tokio::test]
    async fn test_sediment_event_extraction_disabled_skips() {
        let mut server = mockito::Server::new_async().await;
        use crate::api::evorule_client::EvoruleApiClient;
        use crate::io_handlers::LlmHandler;
        let client = EvoruleApiClient::new(&server.url());
        let mut memory = MemoryManager::new("test", client).with_session_id("s1");

        let cfg = SedimentConfig {
            namespace: "test".to_string(),
            enable_event_extraction: false,
            ..Default::default()
        };
        let mut extractor = EventExtractor::with_defaults(LlmHandler::mock(""));
        let mut deps = SedimentDeps {
            memory: &mut memory,
            summarizer: None,
            extractor: Some(&mut extractor),
            event_store: None,
            auditor: None,
            journal_lines: Vec::new(),
        };
        let messages = vec![Message::User {
            content: "今天是我生日".to_string(),
        }];
        // 开关关闭：不应有任何写入（配置语义从"静默失效"变为"真实生效"）
        let m1 = server
            .mock("POST", "/api/sessions/s1/payload")
            .with_status(200)
            .expect(0)
            .create_async()
            .await;

        let result = sediment(&mut deps, &cfg, "s1", &messages).await;
        assert!(result.events.is_empty());
        m1.assert_async().await;
    }

    #[tokio::test]
    async fn test_sediment_no_trigger_no_extraction() {
        let mut server = mockito::Server::new_async().await;
        use crate::api::evorule_client::EvoruleApiClient;
        use crate::io_handlers::LlmHandler;
        let client = EvoruleApiClient::new(&server.url());
        let mut memory = MemoryManager::new("test", client).with_session_id("s1");

        let cfg = SedimentConfig {
            namespace: "test".to_string(),
            ..Default::default()
        };
        let mut extractor = EventExtractor::with_defaults(LlmHandler::mock(""));
        let mut deps = SedimentDeps {
            memory: &mut memory,
            summarizer: None,
            extractor: Some(&mut extractor),
            event_store: None,
            auditor: None,
            journal_lines: Vec::new(),
        };
        let messages = vec![Message::User {
            content: "今天天气不错".to_string(),
        }];
        // 无触发：不调 LLM、不写事件
        let m1 = server
            .mock("POST", "/api/sessions/s1/payload")
            .with_status(200)
            .expect(0)
            .create_async()
            .await;

        let result = sediment(&mut deps, &cfg, "s1", &messages).await;
        assert!(result.events.is_empty());
        m1.assert_async().await;
    }

    // ===== 知识候选提取（巩固管线最小版） =====

    #[test]
    fn test_parse_knowledge_candidates_keeps_valid() {
        let json = r#"{"candidates":[
            {"knowledge_kind":"model","title":"生命周期模型","body":"状态机演进说明","tags":["a"],"confidence":0.9},
            {"knowledge_kind":"fact","title":"配置含义","body":"某配置项的语义"}
        ]}"#;
        let out = parse_knowledge_candidates(json).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].knowledge_kind, "model");
        // confidence 缺省 0.5（fact 条未带）
        assert!((out[1].confidence - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_parse_knowledge_candidates_filters_invalid() {
        let json = r#"{"candidates":[
            {"knowledge_kind":"unknown_kind","title":"x","body":"y"},
            {"knowledge_kind":"heuristic","title":"","body":"y"},
            {"knowledge_kind":"procedure","title":"步骤","body":"   "},
            {"knowledge_kind":"narrative","title":"背景","body":"决策来龙去脉"}
        ]}"#;
        let out = parse_knowledge_candidates(json).unwrap();
        assert_eq!(out.len(), 1, "越界 kind 与空 title/body 应被过滤");
        assert_eq!(out[0].knowledge_kind, "narrative");
    }

    #[test]
    fn test_parse_knowledge_candidates_empty_envelope() {
        // 显式空列表与缺省信封都=无候选
        assert!(parse_knowledge_candidates(r#"{"candidates": []}"#)
            .unwrap()
            .is_empty());
        assert!(parse_knowledge_candidates("{}").unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_sediment_knowledge_extraction_disabled_skips() {
        let mut server = mockito::Server::new_async().await;
        use crate::api::evorule_client::EvoruleApiClient;
        let client = EvoruleApiClient::new(&server.url());
        let mut memory = MemoryManager::new("test", client).with_session_id("s1");
        let cfg = SedimentConfig {
            namespace: "test".to_string(),
            enable_knowledge_extraction: false,
            ..Default::default()
        };
        let mut deps = SedimentDeps {
            memory: &mut memory,
            summarizer: None,
            extractor: None,
            event_store: None,
            auditor: None,
            journal_lines: Vec::new(),
        };
        // 4 条消息：长度过前置闸，证明跳过来自开关而非长度
        let messages = vec![
            Message::User {
                content: "第一条".to_string(),
            },
            Message::Assistant {
                content: "回复一".to_string(),
                tool_calls: None,
            },
            Message::User {
                content: "第二条".to_string(),
            },
            Message::Assistant {
                content: "回复二".to_string(),
                tool_calls: None,
            },
        ];
        let m1 = server
            .mock("POST", "/api/sessions/s1/payload")
            .with_status(200)
            .expect(0)
            .create_async()
            .await;
        let result = sediment(&mut deps, &cfg, "s1", &messages).await;
        assert!(result.knowledge_candidates.is_empty());
        m1.assert_async().await;
    }

    #[tokio::test]
    async fn test_sediment_knowledge_short_session_skips() {
        let mut server = mockito::Server::new_async().await;
        use crate::api::evorule_client::EvoruleApiClient;
        let client = EvoruleApiClient::new(&server.url());
        let mut memory = MemoryManager::new("test", client).with_session_id("s1");
        let cfg = SedimentConfig {
            namespace: "test".to_string(),
            ..Default::default()
        };
        let mut deps = SedimentDeps {
            memory: &mut memory,
            summarizer: None,
            extractor: None,
            event_store: None,
            auditor: None,
            journal_lines: Vec::new(),
        };
        let messages = vec![
            Message::User {
                content: "你好".to_string(),
            },
            Message::Assistant {
                content: "你好！".to_string(),
                tool_calls: None,
            },
        ];
        // 2 条 < min_messages_for_extraction(4)：不提取、无写入
        let m1 = server
            .mock("POST", "/api/sessions/s1/payload")
            .with_status(200)
            .expect(0)
            .create_async()
            .await;
        let result = sediment(&mut deps, &cfg, "s1", &messages).await;
        assert!(result.knowledge_candidates.is_empty());
        m1.assert_async().await;
    }

    #[tokio::test]
    async fn test_sediment_knowledge_no_auditor_skips() {
        // 纪律①：无审计通路不提取（不直连 provider），跳过并 warn
        let mut server = mockito::Server::new_async().await;
        use crate::api::evorule_client::EvoruleApiClient;
        let client = EvoruleApiClient::new(&server.url());
        let mut memory = MemoryManager::new("test", client).with_session_id("s1");
        let cfg = SedimentConfig {
            namespace: "test".to_string(),
            ..Default::default()
        };
        let mut deps = SedimentDeps {
            memory: &mut memory,
            summarizer: None,
            extractor: None,
            event_store: None,
            auditor: None,
            journal_lines: Vec::new(),
        };
        let messages = vec![
            Message::User {
                content: "第一条".to_string(),
            },
            Message::Assistant {
                content: "回复一".to_string(),
                tool_calls: None,
            },
            Message::User {
                content: "第二条".to_string(),
            },
            Message::Assistant {
                content: "回复二".to_string(),
                tool_calls: None,
            },
        ];
        let m1 = server
            .mock("POST", "/api/sessions/s1/payload")
            .with_status(200)
            .expect(0)
            .create_async()
            .await;
        let result = sediment(&mut deps, &cfg, "s1", &messages).await;
        assert!(result.knowledge_candidates.is_empty());
        m1.assert_async().await;
    }

    #[tokio::test]
    async fn test_write_knowledge_candidates_writes_shared() {
        // 写入通路断言双重点：路径落 knowledge_candidates 域（KC- 前缀）+
        // value 携带 MemoryEvent（Custom kind + 候选内容）——D3 映射实证
        let mut server = mockito::Server::new_async().await;
        use crate::api::evorule_client::EvoruleApiClient;
        let client = EvoruleApiClient::new(&server.url());
        let mut memory = MemoryManager::new("test", client).with_session_id("s1");
        let cfg = SedimentConfig {
            namespace: "test".to_string(),
            ..Default::default()
        };
        let mut deps = SedimentDeps {
            memory: &mut memory,
            summarizer: None,
            extractor: None,
            event_store: None,
            auditor: None,
            journal_lines: Vec::new(),
        };
        let candidates = vec![KnowledgeCandidateOut {
            knowledge_kind: "model".to_string(),
            title: "生命周期模型".to_string(),
            body: "规则条目按状态机演进".to_string(),
            tags: vec!["lifecycle".to_string()],
            confidence: 0.8,
        }];
        let mut result = SedimentResult::default();
        let m1 = server
            .mock("POST", "/api/sessions/s1/payload")
            .with_status(200)
            .match_body(mockito::Matcher::Regex(
                // 注意锚顺序：json! 序列化后 content 按键字母序 body 先于 title
                r#"shared\.test\.knowledge_candidates\.KC-s1-\d+-0[\s\S]*Custom[\s\S]*knowledge_candidate[\s\S]*状态机[\s\S]*生命周期模型"#.to_string(),
            ))
            .create_async()
            .await;
        let _ = &cfg;
        write_knowledge_candidates(&mut deps, "s1", candidates, &mut result).await;
        assert_eq!(result.knowledge_candidates.len(), 1);
        assert!(result.knowledge_candidates[0].starts_with("KC-s1-"));
        m1.assert_async().await;
    }
}
