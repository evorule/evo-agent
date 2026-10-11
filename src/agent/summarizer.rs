// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! G10:记忆压缩/摘要 —— Context Window 裁剪后用 LLM 生成摘要,替代简单丢弃
//!
//! ## 设计动机
//!
//! `ContextWindowManager::trim_detailed()` 裁剪历史消息时,默认插入
//! `[earlier N messages trimmed]` 占位提示。这对 LLM 来说信息量为零 —— 它
//! 完全不知道之前聊了什么。
//!
//! G10 的做法:把被裁剪的消息送给一个**摘要 LLM**,生成一段简洁摘要,
//! 替换占位提示。这样 LLM 虽然看不到原始消息,但能从摘要中恢复关键上下文。
//!
//! ## Q9 策略选择:Strategy B(阈值触发)
//!
//! 不每次裁剪都调 LLM(太贵),设一个阈值 `summary_threshold`(默认 5):
//! - 裁剪消息数 < 阈值 → 不调 LLM,保留原 `[earlier N messages trimmed]` 提示
//! - 裁剪消息数 ≥ 阈值 → 调 LLM 生成摘要
//!
//! 理由:丢弃 1-2 条消息时 LLM 通常不受影响;丢弃 5+ 条才值得花一次 API 调用
//! 生成摘要。
//!
//! ## 摘要模型
//!
//! 摘要可以用与主对话不同的(更便宜的)模型,通过 `summary_model` 配置。
//! 如果未配置,fallback 到 `LlmHandler` 的 `default_model`。
//!
//! ## 上下文压缩硬纪律(2026-10-01 立规,接线/扩展时不得违反)
//!
//! **纪律① 记账先于压缩**:凡进入 LLM 调用的最终 prompt 必须在调用前以
//! 原始形态落账;压缩/摘要/裁剪只准发生在账后的视图装配层。本模块全部
//! 摘要调用必须经 [`AuditedLlm`] sidecar 协议(prompt/response 全文入
//! evorule 审计链),**禁止新增任何直连 provider 的调用路径**;违者=审计
//! 链断裂,按红线处置。被裁剪消息的原始全文已在 sidecar 命令事实中留证。
//!
//! **纪律② 视图可重建**:摘要产出(io_response 事实)+被裁原文(命令事实)
//! 均在账,装配产物可由「账上事件+纯函数 render」确定性重建——
//! 模型看到什么 = f(账),而非 f(历史累积)。未来任何摘要变体(新 purpose、
//! 新触发点)接线时必须维持此性质,并提供重建演示留痕。

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tracing::warn;

use crate::agent::audited_llm::AuditedLlm;
use crate::agent::translator::{LlmResponse, Message};
use crate::io_handler::IoHandler;
use crate::io_handlers::LlmHandler;

/// G10:摘要触发的最小裁剪消息数(Q9 Strategy B,默认 5)
pub const DEFAULT_SUMMARY_THRESHOLD: usize = 5;

/// G10:摘要最小 token 数(输入极小时的下限;长会话截断缺陷修复后实际值随输入规模自适应)
///
/// 缺陷背景:固定 512 在长会话(51k token prompt)下必然截断——模型对长输入
/// 倾向更长的输出且可能夹杂格式噪音,512 上限把摘要硬截在半句。
/// 修复=`adaptive_max_tokens()` 按输入规模放大,512 保留为下限。
const SUMMARY_MAX_TOKENS: u64 = 512;

/// 摘要 max_tokens 自适应上限(防长输入下 512 硬截断)
const SUMMARY_MAX_TOKENS_CAP: u64 = 8192;

/// 按输入规模自适应摘要 max_tokens
///
/// - 输入 token 估算 = 字符数 / 3(中英混合粗估)
/// - max_tokens = clamp(估算值 / 4, 512, 8192):摘要长度随输入次线性增长,
///   /4 给足 JSON 结构与格式噪音余量;下限 512 保证小会话完整表述,
///   上限 8192 封顶防成本失控
fn adaptive_max_tokens(input_chars: usize) -> u64 {
    let est_input_tokens = (input_chars / 3) as u64;
    (est_input_tokens / 4).clamp(SUMMARY_MAX_TOKENS, SUMMARY_MAX_TOKENS_CAP)
}

/// C1:整会话摘要 + 稳定事实 LLM 输出结构
///
/// `summarize_session()` 让 LLM 一次调用同时产出摘要和稳定事实列表，
/// 避免对同一段对话发两次 LLM 请求。
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SessionSummaryOut {
    /// 整会话摘要文本
    pub summary: String,
    /// 稳定事实列表（用户偏好、决策、约束等跨会话信息）
    #[serde(default)]
    pub stable_facts: Vec<StableFactOut>,
}

/// C1:单条稳定事实 LLM 输出结构
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct StableFactOut {
    /// 事实 key（如 "preferred_language"、"timezone"）
    pub key: String,
    /// 事实 value（如 "zh-CN"、"Asia/Shanghai"）
    ///
    /// 宽容反序列化：LLM 固有输出不确定性——Battle B r4 实测 MiniMax-M2.5
    /// 输出 {"key": "max_response_time_ms", "value": 500}（数字而非字符串），
    /// 严格 String 反序列化导致整条摘要解析失败（summary_written=false）。
    /// 数字/布尔/null/嵌套值一律字符串化收纳。
    #[serde(deserialize_with = "deserialize_flexible_string")]
    pub value: String,
    /// 置信度 0.0-1.0（LLM 自评，缺失时默认 0.0）
    #[serde(default)]
    pub confidence: f32,
}

/// 宽容字符串反序列化：string 原样，number/bool/null 字符串化，
/// 数组/对象 serde_json 字符串化（LLM 输出类型漂移的兜底，
/// 见 Battle B r4 stable_facts value=500 实录）
fn deserialize_flexible_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let v = serde_json::Value::deserialize(deserializer)?;
    Ok(match v {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    })
}

/// G10:摘要系统提示(中文,引导 LLM 生成结构化摘要)
const DEFAULT_SUMMARY_PROMPT: &str = "\
你是对话摘要助手。请将以下对话历史压缩为简洁的摘要,保留:\n\
1. 用户的意图和核心请求\n\
2. 关键信息、实体和数据\n\
3. 已做出的决定和结论\n\
4. 尚未解决的问题或待办事项\n\n\
要求:\n\
- 用中文输出,不超过 300 字\n\
- 只输出摘要纯文本本身,禁止输出任何工具调用格式(如 <minimax:tool_call>)、JSON 或代码块\n\
- 不要编造对话中不存在的信息\n\
- 不要包含寒暄、客套等无关内容\n\
- 用要点格式(1. 2. 3.)组织,便于快速阅读";

/// G10:上下文摘要器 —— 裁剪消息后用 LLM 生成摘要
///
/// 由 `AgentRunner::from_definition` 在 `summary_model` 配置时自动构造。
/// 如果未配置 `summary_model`,runner 不持有 summarizer,裁剪时只保留原 hint。
///
/// # 工作流
///
/// ```text
/// trim_detailed() → TrimResult { messages, dropped }
///                        │
///                        ▼
///              dropped.len() >= threshold?
///                   │           │
///                  否           是
///                   │           │
///                   ▼           ▼
///            保留原 hint    summarize_dropped(dropped)
///                               │
///                               ▼
///                      替换 hint 为 LLM 生成的摘要
/// ```
#[derive(Debug, Clone)]
pub struct ContextSummarizer {
    /// 摘要用的 LLM handler(从主 handler clone 而来)
    llm: LlmHandler,
    /// P2-V3 结构性修复：审计链执行器（None = 直连，仅供测试/独立用途）
    ///
    /// 挂载后所有摘要类 LLM 调用经一次性 sidecar 会话走完整
    /// call_external 审计协议（prompt/response 均为事实）；
    /// 生产构造点 `AgentRunner::from_definition` 必挂。
    audited: Option<AuditedLlm>,
    /// 摘要模型名称(None = fallback 到 llm.default_model)
    summary_model: Option<String>,
    /// 摘要系统提示
    summary_prompt: String,
    /// Q9 Strategy B:触发摘要的最小裁剪消息数
    summary_threshold: usize,
    /// 滚动摘要缓存:(frozen_dropped_len, 不含头缀的摘要正文)
    ///
    /// summarizer 随 runner 构造(每次运行新建实例),dropped 序列随裁剪单调
    /// 增长,按 frozen 长度做增量摘要,消除每轮全量重算:
    /// - dropped.len() == frozen → 直接复用,零 LLM 调用
    /// - dropped.len() > frozen → 输入 = 旧摘要(并入 system prompt)+ dropped[frozen..]
    /// - LLM 失败不写缓存,旧值保留(下次重试仍从旧 frozen 增量)
    /// - dropped.len() < frozen(理论不发生,防御)→ 忽略缓存全量重算
    /// - 前提:缓存随 runner 实例私有(现状成立);若未来跨会话共享
    ///   summarizer,需为缓存键引入会话维度
    summary_cache: Arc<Mutex<Option<RollingCache>>>,
}

/// 滚动摘要缓存条目（R3 落链批：缓存带代数，供落链 metadata 与 payload 回读种子）。
#[derive(Debug, Clone)]
struct RollingCache {
    frozen_len: usize,
    summary_text: String,
    gen: u64,
}

/// 一次新生成摘要的落链元数据（R3 方案 B：PayloadUpdate 专用命名空间，
/// 见 knowledge/上下文管理/07-R3 摘要升格 Fact 研究）。
#[derive(Debug, Clone)]
pub struct SummaryGenMetadata {
    /// 摘要代数（会话内从 1 递增）
    pub gen: u64,
    /// 上一代 frozen 边界（首代 0）
    pub frozen_len_before: usize,
    /// 本代 frozen 边界（= dropped.len()）
    pub frozen_len_after: usize,
    /// 摘要策略指纹（RL-B3：可版本化、人类可读）
    pub strategy_fingerprint: String,
    /// 上一代代数（首代 None）
    pub parent_gen: Option<u64>,
    /// 摘要正文（不含 [earlier conversation summary] 头缀）
    pub summary_text: String,
    /// 含头缀的完整 hint 替换文本（与既有 wire 形态一致）
    pub formatted: String,
}

/// 一次 summarize_dropped 的结果分类（R3 落链批）。
#[derive(Debug, Clone)]
pub enum SummarizeOutcome {
    /// dropped 为空或低于阈值——无摘要（调用方保留原 hint）
    Empty,
    /// 缓存命中——无新代，不落链
    CacheHit(String),
    /// 新代生成——调用方应落链（PayloadUpdate），并将 formatted 写入 hint
    Generated(SummaryGenMetadata),
}

impl SummarizeOutcome {
    /// hint 替换文本（Empty → None）。
    pub fn formatted(&self) -> Option<String> {
        match self {
            SummarizeOutcome::Empty => None,
            SummarizeOutcome::CacheHit(f) => Some(f.clone()),
            SummarizeOutcome::Generated(m) => Some(m.formatted.clone()),
        }
    }
}

/// F-702:观察价值权重——工具类别 × 输出长度 × goal 关键词命中。
///
/// 词法权重表固定（09 规格 F-702"确定性:词法权重表固定"，零向量）：
/// - 类别基权（工具名小写子串匹配，取首个命中；未命中 0.5）：
///   变更类(write/edit/create/delete/update/apply)=0.9 ▸
///   执行类(shell/exec/run/command/bash)=0.8 ▸
///   检索类(search/find/grep/query/list)=0.7 ▸
///   读取类(read/cat/open/show/get)=0.6
/// - 长度因子：(len/4000).min(1.0)——长输出更需摘要择要
/// - goal 命中：1 + 0.5×命中比（R05 词法，cap 1.0）
///
/// 最终 = 基权 × (0.6+0.4×长度因子) × (1.0+0.5×命中比)——仅用于相对分档。
#[allow(dead_code)] // 仅 cfg(test) F-702 单测引用,lib 构建无调用方
pub(crate) fn observation_value_weight(tool_name: &str, output: &str, goal: &str) -> f32 {
    const TABLE: &[(&str, f32)] = &[
        ("write", 0.9),
        ("edit", 0.9),
        ("create", 0.9),
        ("delete", 0.9),
        ("update", 0.9),
        ("apply", 0.9),
        ("shell", 0.8),
        ("exec", 0.8),
        ("run", 0.8),
        ("command", 0.8),
        ("bash", 0.8),
        ("search", 0.7),
        ("find", 0.7),
        ("grep", 0.7),
        ("query", 0.7),
        ("list", 0.7),
        ("read", 0.6),
        ("cat", 0.6),
        ("open", 0.6),
        ("show", 0.6),
        ("get", 0.6),
    ];
    let n = tool_name.to_lowercase();
    let category = TABLE
        .iter()
        .find(|(k, _)| n.contains(k))
        .map(|(_, w)| *w)
        .unwrap_or(0.5);
    let len_factor = (output.len() as f32 / 4000.0).min(1.0);
    let mut goal_uniq = crate::agent::memory::tokenize_for_match(goal);
    goal_uniq.sort();
    goal_uniq.dedup();
    let hit_ratio = if goal_uniq.is_empty() {
        0.0
    } else {
        let out_tokens: std::collections::HashSet<String> =
            crate::agent::memory::tokenize_for_match(output)
                .into_iter()
                .collect();
        let hits = goal_uniq.iter().filter(|g| out_tokens.contains(*g)).count();
        (hits as f32 / goal_uniq.len() as f32).min(1.0)
    };
    category * (0.6 + 0.4 * len_factor) * (1.0 + 0.5 * hit_ratio)
}

/// F-702:观察价值标注段——附于摘要 system prompt,引导 LLM 择要覆盖重点。
///
/// 对 dropped 中的 Tool 消息逐条计权重,降序分档列出（高≥0.9/中≥0.7/低）。
/// 同分按行字典序（确定性）。无 Tool 消息或 goal 为空时不产标注段
/// （goal 空即旧调用路径,行为字节级兼容）。
#[allow(dead_code)] // 仅 cfg(test) F-702 单测引用,lib 构建无调用方
fn observation_annotations(dropped: &[Message], goal: &str) -> String {
    if goal.is_empty() {
        return String::new();
    }
    let mut rows: Vec<(f32, String)> = Vec::new();
    for (idx, msg) in dropped.iter().enumerate() {
        if let Message::Tool { content, tool_name } = msg {
            let w = observation_value_weight(tool_name, content, goal);
            rows.push((
                w,
                format!("#{} {} len={} w={:.2}", idx, tool_name, content.len(), w),
            ));
        }
    }
    if rows.is_empty() {
        return String::new();
    }
    rows.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut s =
        String::from("\n\n[观察价值标注(F-702,确定性词法):摘要请优先覆盖高价值观察的结论与影响]\n");
    for (w, row) in &rows {
        // 分档阈值对齐实际得分域(0.9×0.6×1.0=0.54 ~ 0.9×1.0×1.5=1.35):
        // 高≥0.8(变更类+命中/长输出),中≥0.6,低<0.6
        let tier = if *w >= 0.8 {
            "高"
        } else if *w >= 0.6 {
            "中"
        } else {
            "低"
        };
        s.push_str(&format!("- [{}] {}\n", tier, row));
    }
    s
}

/// G10:被裁剪消息摘要（legacy 便捷封装）。
///
/// # 返回值
///
/// - `Ok("")`:被裁剪消息为空,或低于阈值(Q9 Strategy B),不生成摘要
/// - `Ok(summary)`:摘要生成成功,格式为 `[earlier conversation summary]\n{内容}`
/// - `Err(e)`:LLM 调用或解析失败,调用方应 fallback 到原 hint
///
/// # 参数
///
/// - `dropped`:`trim_detailed()` 返回的被裁剪消息列表
impl ContextSummarizer {
    /// 创建摘要器
    ///
    /// - `llm`:从主 LlmHandler clone 而来(共享配置/API key,独立调用)
    /// - `summary_model`:摘要模型名称,None 时用 llm 的 default_model
    pub fn new(llm: LlmHandler, summary_model: Option<String>) -> Self {
        Self {
            llm,
            audited: None,
            summary_model,
            summary_prompt: DEFAULT_SUMMARY_PROMPT.to_string(),
            summary_threshold: DEFAULT_SUMMARY_THRESHOLD,
            summary_cache: Arc::new(Mutex::new(None)),
        }
    }

    /// 挂载审计链执行器（P2-V3 结构性修复）
    ///
    /// 挂载后 LLM 调用经 evorule 审计链（sidecar 会话），
    /// 未挂载时保持直连（旁路计数器留痕）。
    pub fn with_auditor(mut self, audited: AuditedLlm) -> Self {
        self.audited = Some(audited);
        self
    }

    /// 审计执行器只读访问
    ///
    /// sediment 知识候选提取复用同一 sidecar 通路（纪律①：沉淀提取面
    /// 禁止新增直连 provider 调用路径）。
    pub fn auditor(&self) -> Option<&AuditedLlm> {
        self.audited.as_ref()
    }

    /// 自定义摘要阈值(Q9 Strategy B,测试用)
    pub fn with_threshold(mut self, threshold: usize) -> Self {
        self.summary_threshold = threshold;
        self
    }

    /// 自定义摘要系统提示(测试用)
    pub fn with_prompt(mut self, prompt: &str) -> Self {
        self.summary_prompt = prompt.to_string();
        self
    }

    /// 当前摘要阈值(只读访问)
    pub fn threshold(&self) -> usize {
        self.summary_threshold
    }

    /// LLM 调用分流：有审计执行器走 sidecar 审计链，否则直连 + 旁路计数
    ///
    /// P2-V3 结构性修复后，生产路径（from_definition 构造）恒走审计分支；
    /// 直连分支仅存在于未挂载 auditor 的场景（单元测试/独立使用），
    /// 属显式配置而非静默兜底。
    async fn call_llm(&self, purpose: &str, params: &Value) -> Result<Value, String> {
        match &self.audited {
            Some(audited) => audited.execute(purpose, params).await,
            None => {
                // P2-V3 止血指标：此调用不经审计链，计数留痕
                crate::metrics::bypass_audit(purpose);
                self.llm.execute(params).await
            }
        }
    }

    /// 摘要模型名称(只读访问)
    pub fn summary_model(&self) -> Option<&str> {
        self.summary_model.as_deref()
    }

    /// 被裁剪消息摘要便捷入口(不携带落链元数据;goal 空=不产观察价值标注)
    pub async fn summarize_dropped(&self, dropped: &[Message]) -> Result<String, String> {
        // F-702:goal 空串=不产观察价值标注(旧调用路径行为字节级兼容)
        match self.summarize_dropped_with_metadata(dropped, "").await? {
            SummarizeOutcome::Empty => Ok(String::new()),
            SummarizeOutcome::CacheHit(formatted) => Ok(formatted),
            SummarizeOutcome::Generated(meta) => Ok(meta.formatted),
        }
    }

    /// R3 落链批：带落链元数据的摘要生成。
    ///
    /// - `Empty`：无摘要（保留原 hint），不落链
    /// - `CacheHit`：缓存命中（零 LLM 调用），不落链
    /// - `Generated(meta)`：新代生成——调用方应将 meta 落链
    ///   （PayloadUpdate 至 `__memory__.{ns}.session_{sid}.rolling_summary`，
    ///   见 knowledge/上下文管理/07-R3 研究档），并将 meta.formatted 写入 hint。
    ///   `goal` 非空时附观察价值标注段（F-702），引导摘要择要覆盖高价值观察。
    pub async fn summarize_dropped_with_metadata(
        &self,
        dropped: &[Message],
        goal: &str,
    ) -> Result<SummarizeOutcome, String> {
        self.summarize_dropped_with_purpose(dropped, goal, "summarize")
            .await
    }

    /// purpose 可变的通用摘要形态（B21 D3 主动压缩经此以 purpose=compaction
    /// 调用，走 audited_llm 留痕；摘要机制/滚动缓存/落链元数据与
    /// [`Self::summarize_dropped_with_metadata`] 全量同构，仅审计用途标签不同）。
    pub async fn summarize_dropped_with_purpose(
        &self,
        dropped: &[Message],
        _goal: &str,
        purpose: &str,
    ) -> Result<SummarizeOutcome, String> {
        // 空列表:无需摘要
        if dropped.is_empty() {
            return Ok(SummarizeOutcome::Empty);
        }

        // Q9 Strategy B:低于阈值不调 LLM(避免小裁剪浪费 token)
        if dropped.len() < self.summary_threshold {
            tracing::debug!(
                dropped = dropped.len(),
                threshold = self.summary_threshold,
                "G10: dropped below threshold, skipping summary"
            );
            return Ok(SummarizeOutcome::Empty);
        }

        // 滚动缓存快照(锁内仅取快照,不做 await)。长度命中时直接复用,零 LLM 调用
        let (frozen, rolling, prev_gen) = {
            let guard = self.summary_cache.lock().unwrap_or_else(|p| p.into_inner());
            match guard.as_ref() {
                Some(c) if c.frozen_len == dropped.len() => {
                    tracing::debug!(dropped = dropped.len(), "G10: summary cache hit");
                    let formatted = format!("[earlier conversation summary]\n{}", c.summary_text);
                    return Ok(SummarizeOutcome::CacheHit(formatted));
                }
                Some(c) if c.frozen_len < dropped.len() => (
                    Some(c.frozen_len),
                    Some(c.summary_text.clone()),
                    Some(c.gen),
                ),
                _ => (None, None, None),
            }
        };
        let incremental_from = frozen.unwrap_or(0);

        tracing::debug!(
            dropped = dropped.len(),
            incremental_from = incremental_from,
            model = ?self.summary_model,
            "G10: generating summary for dropped messages"
        );

        // 构造 messages 数组:system prompt + dropped messages
        // Message 实现了 Serialize(tag = "role"),直接序列化为 {role, content, ...}
        // 增量模式:system prompt 附带旧摘要,输入 = 增量段而非全量重算
        let mut system_content = self.summary_prompt.clone();
        if let Some(old) = &rolling {
            system_content.push_str(
                "\n\n[以下是更早消息的既有摘要,请在其基础上合并续写,保留仍然有效的要点]\n",
            );
            system_content.push_str(old);
        }
        let mut messages_vec: Vec<serde_json::Value> = Vec::with_capacity(dropped.len() + 1);
        messages_vec.push(serde_json::json!({
            "role": "system",
            "content": system_content,
        }));
        for msg in &dropped[incremental_from..] {
            let serialized = serde_json::to_value(msg)
                .map_err(|e| format!("serialize dropped message: {}", e))?;
            messages_vec.push(serialized);
        }
        let messages_json = serde_json::Value::Array(messages_vec);

        // 构造 LLM 调用参数
        // 用 serde_json::Value 构造再转 Value,确保类型正确
        // (temperature 必须是 number,不是 string)
        let mut params_map = serde_json::Map::new();
        if let Some(model) = &self.summary_model {
            params_map.insert(
                "model".to_string(),
                serde_json::Value::String(model.clone()),
            );
        }
        params_map.insert(
            "temperature".to_string(),
            serde_json::json!(0.0), // temperature=0 保证摘要确定性
        );
        params_map.insert(
            "max_tokens".to_string(),
            serde_json::json!(adaptive_max_tokens(messages_json.to_string().len())),
        );
        params_map.insert("messages".to_string(), messages_json);
        let params_json = serde_json::Value::Object(params_map);
        let params = params_json.clone();

        // 调用 LLM（经审计链或直连，见 call_llm 分流说明）
        let result = self.call_llm(purpose, &params).await?;

        // 解析响应为 LlmResponse
        let response: LlmResponse = serde_json::from_str(&result.to_string())
            .map_err(|e| format!("parse summary LLM response: {}", e))?;

        let summary = response.content.trim();
        if summary.is_empty() {
            warn!("G10: LLM returned empty summary, keeping original hint");
            return Ok(SummarizeOutcome::Empty);
        }

        // 成功后才更新缓存;失败路径不触碰缓存(下次重试仍从旧 frozen 增量)
        // R3:缓存带代数;新代元数据交由调用方落链(PayloadUpdate)
        let gen = prev_gen.unwrap_or(0) + 1;
        {
            let mut guard = self.summary_cache.lock().unwrap_or_else(|p| p.into_inner());
            *guard = Some(RollingCache {
                frozen_len: dropped.len(),
                summary_text: summary.to_string(),
                gen,
            });
        }

        let meta = SummaryGenMetadata {
            gen,
            frozen_len_before: frozen.unwrap_or(0),
            frozen_len_after: dropped.len(),
            strategy_fingerprint: self.strategy_fingerprint(),
            parent_gen: prev_gen,
            summary_text: summary.to_string(),
            formatted: format!("[earlier conversation summary]\n{}", summary),
        };
        Ok(SummarizeOutcome::Generated(meta))
    }

    /// 摘要策略指纹（RL-B3：可版本化、人类可读；策略参数变化 → 指纹变化）。
    pub fn strategy_fingerprint(&self) -> String {
        format!(
            "summarizer:threshold={};model={};clamp=512-8192;temp=0",
            self.summary_threshold,
            self.summary_model.as_deref().unwrap_or("default")
        )
    }

    /// R3/G-3：从落链 payload 回读种子滚动缓存（continuation/重启后
    /// 不触发 LLM 重算——重建读账，不重算）。
    pub fn seed_cache(&self, frozen_len: usize, summary_text: String, gen: u64) {
        let mut guard = self.summary_cache.lock().unwrap_or_else(|p| p.into_inner());
        // 只在回读代数更新时覆盖（防旧账回灌）
        if guard.as_ref().map(|c| c.gen).unwrap_or(0) < gen {
            *guard = Some(RollingCache {
                frozen_len,
                summary_text,
                gen,
            });
        }
    }

    /// C1:整会话摘要 + 稳定事实（一次调用，返回结构化 JSON）
    ///
    /// 与 `summarize_dropped` 的区别：
    /// - `summarize_dropped` 是上下文窗口裁剪时的即时压缩（输入是 Message 列表，输出纯文本）
    /// - `summarize_session` 是会话结束时的整段沉淀（输入是拼接后的对话文本，输出结构化 JSON）
    /// - `summarize_session` 额外提取稳定事实（用户偏好、决策、约束），供跨会话共享
    ///
    /// # 返回值
    ///
    /// - `Ok(out)`:摘要 + 稳定事实（stable_facts 可能为空）
    /// - `Err(e)`:LLM 调用或 JSON 解析失败，调用方应 best-effort 跳过
    ///
    /// # 参数
    ///
    /// - `conversation`:拼接后的对话纯文本（由 `sediment::conversation_text()` 生成）
    pub async fn summarize_session(&self, conversation: &str) -> Result<SessionSummaryOut, String> {
        let prompt = format!(
            "Summarize the following conversation and extract stable facts \
             (user preferences, decisions, constraints).\n\
             Output JSON: {{\"summary\": \"...\", \"stable_facts\": \
             [{{\"key\": \"...\", \"value\": \"...\", \"confidence\": 0.9}}]}}\n\n\
             Conversation:\n{}",
            conversation
        );

        let mut messages_vec: Vec<serde_json::Value> = Vec::with_capacity(2);
        messages_vec.push(serde_json::json!({
            "role": "system",
            "content": "你是对话摘要助手。请总结对话并提取稳定事实（用户偏好、决策、约束）。只输出 JSON，不要输出其他内容，禁止输出任何工具调用格式。",
        }));
        messages_vec.push(serde_json::json!({
            "role": "user",
            "content": prompt,
        }));

        let mut params_map = serde_json::Map::new();
        if let Some(model) = &self.summary_model {
            params_map.insert(
                "model".to_string(),
                serde_json::Value::String(model.clone()),
            );
        }
        params_map.insert(
            "temperature".to_string(),
            serde_json::json!(0.0), // temperature=0 保证最大确定性
        );
        params_map.insert(
            "max_tokens".to_string(),
            serde_json::json!(adaptive_max_tokens(prompt.len())),
        );
        params_map.insert(
            "messages".to_string(),
            serde_json::Value::Array(messages_vec),
        );
        let params_json = serde_json::Value::Object(params_map);
        let params = params_json.clone();

        // 调用 LLM（经审计链或直连，见 call_llm 分流说明）
        let result = self.call_llm("session_summary", &params).await?;

        // 解析响应为 LlmResponse
        let response: LlmResponse = serde_json::from_str(&result.to_string())
            .map_err(|e| format!("parse session summary LLM response: {}", e))?;

        // 从可能包含 markdown 代码块的文本中提取 JSON
        let json_str = extract_json_from_text(&response.content);
        let out: SessionSummaryOut = serde_json::from_str(&json_str).map_err(|e| {
            format!(
                "parse session summary JSON: {} (raw: {})",
                e, response.content
            )
        })?;

        Ok(out)
    }

    /// C4 第三级:把多条旧摘要合并为一条 rollup 摘要
    ///
    /// 当共享空间的 L1 会话摘要数量超过 `summary_rollup_threshold` 时，
    /// `sediment::rollup_old_summaries` 取最旧的若干条调用本方法合并为一条，
    /// 减少召回时的注入条数和 token 占用。
    ///
    /// # 返回值
    ///
    /// - `Ok("")`:输入为空
    /// - `Ok(s)`:合并后的摘要文本（单条输入时原样返回）
    /// - `Err(e)`:LLM 调用或解析失败
    pub async fn rollup_summaries(&self, old_summaries: &[String]) -> Result<String, String> {
        if old_summaries.is_empty() {
            return Ok(String::new());
        }
        if old_summaries.len() == 1 {
            return Ok(old_summaries[0].clone());
        }

        let combined = old_summaries.join("\n---\n");
        let prompt = format!(
            "You are a summarization assistant. Combine the following session summaries into a single concise summary. \
            Preserve key facts, decisions, and preferences. Output only the summary text.\n\n\
            Summaries to combine:\n{}",
            combined
        );

        let mut messages_vec: Vec<serde_json::Value> = Vec::with_capacity(2);
        messages_vec.push(serde_json::json!({
            "role": "system",
            "content": "You are a summarization assistant.",
        }));
        messages_vec.push(serde_json::json!({
            "role": "user",
            "content": prompt,
        }));

        let mut params_map = serde_json::Map::new();
        if let Some(model) = &self.summary_model {
            params_map.insert(
                "model".to_string(),
                serde_json::Value::String(model.clone()),
            );
        }
        params_map.insert("temperature".to_string(), serde_json::json!(0.0));
        params_map.insert(
            "max_tokens".to_string(),
            serde_json::json!(adaptive_max_tokens(combined.len())),
        );
        params_map.insert(
            "messages".to_string(),
            serde_json::Value::Array(messages_vec),
        );
        let params_json = serde_json::Value::Object(params_map);
        let params = params_json.clone();

        // 调用 LLM（经审计链或直连，见 call_llm 分流说明）
        let result = self.call_llm("rollup", &params).await?;
        let response: LlmResponse = serde_json::from_str(&result.to_string())
            .map_err(|e| format!("parse rollup summary LLM response: {}", e))?;

        Ok(response.content)
    }
}

/// C1:从可能包含 markdown 代码块的文本中提取 JSON
///
/// LLM 输出经常被包裹在 ```json ... ``` 代码块中，此函数尝试多种策略
/// 提取纯 JSON 文本：
/// 1. 直接以 `{` 开头 → 原样返回
/// 2. 包含 ```json ... ``` → 提取代码块内容
/// 3. 包含 ``` ... ``` → 提取代码块内容（跳过语言标识）
/// 4. 包含 `{` ... `}` → 截取第一个到最后一个大括号之间的内容
fn extract_json_from_text(text: &str) -> String {
    let trimmed = text.trim();

    // 尝试直接解析
    if trimmed.starts_with('{') {
        return trimmed.to_string();
    }

    // 尝试从 ```json ... ``` 中提取
    if let Some(start) = trimmed.find("```json") {
        let after_json = &trimmed[start + 7..];
        if let Some(end) = after_json.find("```") {
            return after_json[..end].trim().to_string();
        }
    }

    // 尝试从 ``` ... ``` 中提取
    if let Some(start) = trimmed.find("```") {
        let after_code = &trimmed[start + 3..];
        // 跳过语言标识(如 json)
        let after_lang = after_code.strip_prefix("json").unwrap_or(after_code);
        if let Some(end) = after_lang.find("```") {
            return after_lang[..end].trim().to_string();
        }
    }

    // 尝试找到第一个 { 和最后一个 }
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            return trimmed[start..=end].to_string();
        }
    }

    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::translator::ToolCall;
    use serde_json::json;

    fn user(content: &str) -> Message {
        Message::User {
            content: content.to_string(),
        }
    }

    fn assistant(content: &str) -> Message {
        Message::Assistant {
            content: content.to_string(),
            tool_calls: None,
        }
    }

    fn make_dropped(n: usize) -> Vec<Message> {
        (0..n).map(|i| user(&format!("message {}", i))).collect()
    }

    // ========== 基础结构测试 ==========

    #[test]
    fn test_f702_observation_value_weight() {
        // 类别表:变更 > 执行 > 检索 > 读取 > 未知(0.5 兜底)
        let w_write = observation_value_weight("file_write", "x", "");
        let w_exec = observation_value_weight("shell_exec", "x", "");
        let w_search = observation_value_weight("file_search", "x", "");
        let w_read = observation_value_weight("file_read", "x", "");
        let w_unknown = observation_value_weight("mystery_tool", "x", "");
        assert!(w_write > w_exec && w_exec > w_search && w_search > w_read);
        // 未知类别=0.5 基权,短输出+空 goal:w=0.5×(0.6+0.4/4000)≈0.300
        assert!((w_unknown - 0.3).abs() < 0.001);
        assert!(w_unknown < w_read);
        // 长度因子:长输出权重更高
        assert!(
            observation_value_weight("file_read", &"x".repeat(8000), "")
                > observation_value_weight("file_read", "x", "")
        );
        // goal 命中加成:命中输出 > 无关输出
        assert!(
            observation_value_weight("file_read", "记忆预算裁剪的细节结论", "记忆预算裁剪")
                > observation_value_weight("file_read", "完全无关的内容", "记忆预算裁剪")
        );
    }

    #[test]
    fn test_f702_observation_annotations() {
        let dropped = vec![
            Message::Tool {
                content: "short".to_string(),
                tool_name: "file_read".to_string(),
            },
            Message::Tool {
                content: "记忆预算裁剪的关键结论".to_string(),
                tool_name: "file_write".to_string(),
            },
        ];
        let a = observation_annotations(&dropped, "记忆预算裁剪");
        assert!(a.contains("[高]"));
        assert!(a.contains("[低]"));
        // 写工具+goal 命中排在读工具前(降序)
        let write_pos = a.find("file_write").unwrap();
        let read_pos = a.find("file_read").unwrap();
        assert!(write_pos < read_pos);
        // 空 goal → 无标注(旧路径字节级兼容)
        assert!(observation_annotations(&dropped, "").is_empty());
    }

    #[test]
    fn test_summarizer_new_with_model() {
        let llm = LlmHandler::mock("summary content");
        let s = ContextSummarizer::new(llm, Some("gpt-4o-mini".to_string()));
        assert_eq!(s.summary_model(), Some("gpt-4o-mini"));
        assert_eq!(s.threshold(), DEFAULT_SUMMARY_THRESHOLD);
    }

    #[test]
    fn test_summarizer_new_without_model() {
        let llm = LlmHandler::mock("summary content");
        let s = ContextSummarizer::new(llm, None);
        assert_eq!(s.summary_model(), None);
        assert_eq!(s.threshold(), DEFAULT_SUMMARY_THRESHOLD);
    }

    #[test]
    fn test_summarizer_with_threshold() {
        let llm = LlmHandler::mock("summary");
        let s = ContextSummarizer::new(llm, None).with_threshold(3);
        assert_eq!(s.threshold(), 3);
    }

    #[test]
    fn test_summarizer_with_custom_prompt() {
        let llm = LlmHandler::mock("summary");
        let s = ContextSummarizer::new(llm, None).with_prompt("custom prompt");
        assert_eq!(s.summary_prompt, "custom prompt");
    }

    #[test]
    fn test_default_threshold_is_5() {
        assert_eq!(DEFAULT_SUMMARY_THRESHOLD, 5);
    }

    // ========== adaptive_max_tokens 测试 ==========

    #[test]
    fn test_adaptive_max_tokens_small_input_floors_at_512() {
        // 小输入(甚至 0 字符)→ 下限 512
        assert_eq!(adaptive_max_tokens(0), 512);
        assert_eq!(adaptive_max_tokens(3000), 512); // est 1000 tok / 4 = 250 → 512
    }

    #[test]
    fn test_adaptive_max_tokens_scales_with_input() {
        // 51k token 级输入(长会话实测场景):~153k 字符 → est 51000 /4 = 12750 → 封顶 8192
        assert_eq!(adaptive_max_tokens(153_000), 8192);
        // 中等输入:60k 字符 → est 20000 /4 = 5000
        assert_eq!(adaptive_max_tokens(60_000), 5000);
    }

    #[test]
    fn test_adaptive_max_tokens_caps_at_8192() {
        // 超大输入 → 上限 8192(防成本失控)
        assert_eq!(adaptive_max_tokens(10_000_000), 8192);
    }

    // ========== summarize_dropped 测试 ==========

    #[tokio::test]
    async fn test_summarize_empty_dropped_returns_empty() {
        let llm = LlmHandler::mock("should not be called");
        let s = ContextSummarizer::new(llm, None);
        let result = s.summarize_dropped(&[]).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_summarize_below_threshold_returns_empty() {
        // 阈值 5,只丢弃 3 条 → 不调 LLM
        let llm = LlmHandler::mock("should not be called");
        let s = ContextSummarizer::new(llm, None); // threshold = 5
        let dropped = make_dropped(3);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.is_empty(), "below threshold should return empty");
    }

    #[tokio::test]
    async fn test_summarize_at_threshold_calls_llm() {
        // 阈值 5,丢弃正好 5 条 → 调 LLM
        let llm = LlmHandler::mock("这是对话摘要");
        let s = ContextSummarizer::new(llm, None);
        let dropped = make_dropped(5);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.contains("[earlier conversation summary]"));
        assert!(result.contains("这是对话摘要"));
    }

    #[tokio::test]
    async fn test_summarize_above_threshold_calls_llm() {
        let llm = LlmHandler::mock("摘要内容");
        let s = ContextSummarizer::new(llm, None);
        let dropped = make_dropped(10);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.contains("[earlier conversation summary]"));
        assert!(result.contains("摘要内容"));
    }

    #[tokio::test]
    async fn test_summarize_with_custom_threshold() {
        // 阈值设为 2,丢弃 3 条 → 调 LLM
        let llm = LlmHandler::mock("short summary");
        let s = ContextSummarizer::new(llm, None).with_threshold(2);
        let dropped = make_dropped(3);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(!result.is_empty());
    }

    #[tokio::test]
    async fn test_summarize_with_custom_threshold_below() {
        // 阈值设为 10,丢弃 5 条 → 不调 LLM
        let llm = LlmHandler::mock("should not be called");
        let s = ContextSummarizer::new(llm, None).with_threshold(10);
        let dropped = make_dropped(5);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_summarize_result_format() {
        let llm = LlmHandler::mock("用户讨论了天气和行程安排");
        let s = ContextSummarizer::new(llm, Some("summary-model".to_string()));
        let dropped = make_dropped(5);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(
            result.starts_with("[earlier conversation summary]\n"),
            "result should start with summary header, got: {}",
            result
        );
    }

    #[tokio::test]
    async fn test_summarize_empty_llm_response_returns_empty() {
        // LLM 返回空内容 → 返回空字符串(fallback 到原 hint)
        let llm = LlmHandler::mock("");
        let s = ContextSummarizer::new(llm, None);
        let dropped = make_dropped(5);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.is_empty(), "empty LLM response should return empty");
    }

    #[tokio::test]
    async fn test_summarize_whitespace_only_llm_response_returns_empty() {
        let llm = LlmHandler::mock("   \n  \t  ");
        let s = ContextSummarizer::new(llm, None);
        let dropped = make_dropped(5);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(
            result.is_empty(),
            "whitespace-only response should return empty after trim"
        );
    }

    #[tokio::test]
    async fn test_summarize_with_mixed_message_types() {
        // 混合 System/User/Assistant/Tool 消息
        let llm = LlmHandler::mock("混合消息摘要");
        let s = ContextSummarizer::new(llm, None);
        let dropped = vec![
            user("你好"),
            assistant("你好!有什么可以帮你的?"),
            Message::Tool {
                content: "搜索结果: 今天晴天".to_string(),
                tool_name: "search".to_string(),
            },
            assistant("今天天气不错"),
            user("那我们去公园吧"),
        ];
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.contains("混合消息摘要"));
    }

    #[tokio::test]
    async fn test_summarize_with_assistant_tool_calls() {
        // Assistant 消息带 tool_calls
        let llm = LlmHandler::mock("工具调用摘要");
        let s = ContextSummarizer::new(llm, None);
        let dropped = vec![
            user("帮我查天气"),
            Message::Assistant {
                content: "好的,我来查".to_string(),
                tool_calls: Some(vec![ToolCall {
                    name: "search".to_string(),
                    arguments: json!({"q": "北京天气"}),
                }]),
            },
            Message::Tool {
                content: "北京今天 25°C 晴".to_string(),
                tool_name: "search".to_string(),
            },
            assistant("北京今天 25 度,晴天"),
            user("谢谢"),
        ];
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.contains("工具调用摘要"));
    }

    #[tokio::test]
    async fn test_summarize_threshold_boundary() {
        // 边界测试:threshold=5
        // 4 条 → 不调
        let llm = LlmHandler::mock("summary");
        let s = ContextSummarizer::new(llm, None); // threshold=5
        assert!(s
            .summarize_dropped(&make_dropped(4))
            .await
            .unwrap()
            .is_empty());
        // 5 条 → 调
        let llm = LlmHandler::mock("summary");
        let s = ContextSummarizer::new(llm, None);
        assert!(!s
            .summarize_dropped(&make_dropped(5))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn test_summarize_threshold_zero_always_calls() {
        // threshold=0 → 即使 1 条也调 LLM
        let llm = LlmHandler::mock("single message summary");
        let s = ContextSummarizer::new(llm, None).with_threshold(0);
        let dropped = make_dropped(1);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(!result.is_empty(), "threshold=0 should always call LLM");
    }

    #[tokio::test]
    async fn test_summarize_with_summary_model_configured() {
        // 配置 summary_model 时,params 中应包含 model 字段
        // mock 不关心 params,只验证流程通畅
        let llm = LlmHandler::mock("model-specific summary");
        let s = ContextSummarizer::new(llm, Some("gpt-4o-mini".to_string()));
        let dropped = make_dropped(5);
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.contains("model-specific summary"));
        assert_eq!(s.summary_model(), Some("gpt-4o-mini"));
    }

    #[tokio::test]
    async fn test_summarize_dropped_preserves_order() {
        // 验证 dropped 消息按原顺序传给 LLM(mock 不验证,但确保不 panic)
        let llm = LlmHandler::mock("ordered summary");
        let s = ContextSummarizer::new(llm, None);
        let dropped = vec![
            user("第一条"),
            user("第二条"),
            user("第三条"),
            user("第四条"),
            user("第五条"),
        ];
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(result.contains("ordered summary"));
    }

    #[test]
    fn test_summarizer_clone() {
        let llm = LlmHandler::mock("test");
        let s = ContextSummarizer::new(llm, Some("model".to_string())).with_threshold(3);
        let s2 = s.clone();
        assert_eq!(s.threshold(), s2.threshold());
        assert_eq!(s.summary_model(), s2.summary_model());
    }

    // ========== C1: summarize_session 测试 ==========

    #[tokio::test]
    async fn test_summarize_session_returns_structured_json() {
        let mock_json = r#"{"summary":"用户讨论了Rust学习","stable_facts":[{"key":"language","value":"Rust","confidence":0.95}]}"#;
        let llm = LlmHandler::mock(mock_json);
        let s = ContextSummarizer::new(llm, None);
        let result = s
            .summarize_session("User: 我想学Rust\nAssistant: 好的")
            .await
            .unwrap();
        assert_eq!(result.summary, "用户讨论了Rust学习");
        assert_eq!(result.stable_facts.len(), 1);
        assert_eq!(result.stable_facts[0].key, "language");
        assert_eq!(result.stable_facts[0].value, "Rust");
        assert!((result.stable_facts[0].confidence - 0.95).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_summarize_session_with_markdown_code_block() {
        // LLM 输出被包裹在 ```json ... ``` 中
        let mock_response = "```json\n{\"summary\":\"摘要\",\"stable_facts\":[]}\n```";
        let llm = LlmHandler::mock(mock_response);
        let s = ContextSummarizer::new(llm, None);
        let result = s.summarize_session("User: hi").await.unwrap();
        assert_eq!(result.summary, "摘要");
        assert!(result.stable_facts.is_empty());
    }

    #[tokio::test]
    async fn test_summarize_session_no_stable_facts_field() {
        // stable_facts 字段缺失时，serde(default) 应填充空 Vec
        let mock_json = r#"{"summary":"只有摘要"}"#;
        let llm = LlmHandler::mock(mock_json);
        let s = ContextSummarizer::new(llm, None);
        let result = s.summarize_session("User: hi").await.unwrap();
        assert_eq!(result.summary, "只有摘要");
        assert!(result.stable_facts.is_empty());
    }

    #[tokio::test]
    async fn test_summarize_session_invalid_json_returns_error() {
        let llm = LlmHandler::mock("not json at all");
        let s = ContextSummarizer::new(llm, None);
        let result = s.summarize_session("User: hi").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_summarize_session_with_model_configured() {
        let mock_json = r#"{"summary":"摘要","stable_facts":[]}"#;
        let llm = LlmHandler::mock(mock_json);
        let s = ContextSummarizer::new(llm, Some("gpt-4o-mini".to_string()));
        let result = s.summarize_session("User: hi").await.unwrap();
        assert_eq!(result.summary, "摘要");
    }

    #[tokio::test]
    async fn test_summarize_session_fact_without_confidence() {
        // confidence 字段缺失时，serde(default) 应填充 0.0
        let mock_json = r#"{"summary":"摘要","stable_facts":[{"key":"k","value":"v"}]}"#;
        let llm = LlmHandler::mock(mock_json);
        let s = ContextSummarizer::new(llm, None);
        let result = s.summarize_session("User: hi").await.unwrap();
        assert_eq!(result.stable_facts[0].confidence, 0.0);
    }

    // ========== C4: rollup_summaries 测试 ==========

    // ========== P2-V3 结构性修复：with_auditor 分流验证 ==========

    #[tokio::test]
    async fn test_summarize_dropped_routes_through_audited_llm() {
        // 挂载 auditor 后走 sidecar 审计链：LLM 结果来自审计路径的独立 mock，
        // 直连 handler 的返回内容不应出现
        let mut server = mockito::Server::new_async().await;
        let create_mock = server
            .mock("POST", "/api/sessions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"session_id":31}"#)
            .create_async()
            .await;
        let sse = concat!(
            "data: {\"type\":\"IoRequest\",\"id\":7}\n\n",
            "data: {\"type\":\"Stable\"}\n\n"
        );
        let events_mock = server
            .mock("GET", "/api/sessions/31/events")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse)
            .create_async()
            .await;
        let command_mock = server
            .mock("POST", "/api/sessions/31/command")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "instruction": {
                    "params": {"audit_purpose": "summarize"}
                }
            })))
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;
        let io_response_mock = server
            .mock("POST", "/api/sessions/31/io_response")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;

        let direct = LlmHandler::mock("DIRECT-PATH-MARKER");
        let audited = crate::agent::audited_llm::AuditedLlm::new(
            crate::api::evorule_client::EvoruleApiClient::new(&server.url()),
            LlmHandler::mock(r#"{"content":"audited summary content"}"#),
        );
        let s = ContextSummarizer::new(direct, None)
            .with_auditor(audited)
            .with_threshold(2);
        let dropped = vec![user("a"), user("b"), user("c")];
        let result = s.summarize_dropped(&dropped).await.unwrap();
        assert!(
            result.contains("audited summary content"),
            "should use audited path, got: {result}"
        );
        assert!(
            !result.contains("DIRECT-PATH-MARKER"),
            "direct path must not be hit when auditor attached"
        );
        // 协议四端点全部被调用 → 证明走了完整 sidecar 审计回路
        create_mock.assert_async().await;
        events_mock.assert_async().await;
        command_mock.assert_async().await;
        io_response_mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_rollup_summaries_empty() {
        let llm = LlmHandler::mock("should not be called");
        let s = ContextSummarizer::new(llm, None);
        let result = s.rollup_summaries(&[]).await.unwrap();
        assert!(result.is_empty(), "empty input should return empty string");
    }

    #[tokio::test]
    async fn test_rollup_summaries_single() {
        let llm = LlmHandler::mock("should not be called");
        let s = ContextSummarizer::new(llm, None);
        let input = vec!["only one summary".to_string()];
        let result = s.rollup_summaries(&input).await.unwrap();
        assert_eq!(result, "only one summary", "single input returned as-is");
    }

    #[tokio::test]
    async fn test_rollup_summaries_multiple_calls_llm() {
        // 多条摘要 → 调 LLM 合并
        let llm = LlmHandler::mock("combined summary");
        let s = ContextSummarizer::new(llm, None);
        let input = vec![
            "summary one".to_string(),
            "summary two".to_string(),
            "summary three".to_string(),
        ];
        let result = s.rollup_summaries(&input).await.unwrap();
        assert_eq!(result, "combined summary");
    }

    // ========== C1: extract_json_from_text 测试 ==========

    #[test]
    fn test_extract_json_direct() {
        let json = r#"{"key":"value"}"#;
        assert_eq!(extract_json_from_text(json), json);
    }

    #[test]
    fn test_extract_json_from_markdown_json_block() {
        let text = "```json\n{\"key\":\"value\"}\n```";
        assert_eq!(extract_json_from_text(text), r#"{"key":"value"}"#);
    }

    #[test]
    fn test_extract_json_from_markdown_block() {
        let text = "```\n{\"key\":\"value\"}\n```";
        assert_eq!(extract_json_from_text(text), r#"{"key":"value"}"#);
    }

    #[test]
    fn test_extract_json_with_surrounding_text() {
        let text = r#"Here is the result: {"key":"value"} done."#;
        assert_eq!(extract_json_from_text(text), r#"{"key":"value"}"#);
    }

    #[test]
    fn test_extract_json_no_braces_returns_original() {
        let text = "no json here";
        assert_eq!(extract_json_from_text(text), "no json here");
    }

    #[test]
    fn test_session_summary_out_deserialize() {
        let json =
            r#"{"summary":"test","stable_facts":[{"key":"k","value":"v","confidence":0.8}]}"#;
        let out: SessionSummaryOut = serde_json::from_str(json).unwrap();
        assert_eq!(out.summary, "test");
        assert_eq!(out.stable_facts.len(), 1);
        assert_eq!(out.stable_facts[0].key, "k");
    }

    #[test]
    fn test_session_summary_out_deserialize_no_facts() {
        let json = r#"{"summary":"test"}"#;
        let out: SessionSummaryOut = serde_json::from_str(json).unwrap();
        assert_eq!(out.summary, "test");
        assert!(out.stable_facts.is_empty());
    }

    // ========== 滚动摘要缓存测试 ==========

    fn marker_msgs(prefix: &str, n: usize) -> Vec<Message> {
        (0..n).map(|i| user(&format!("{}-{}", prefix, i))).collect()
    }

    fn openai_body(content: &str) -> String {
        serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": content}}]
        })
        .to_string()
    }

    #[tokio::test]
    async fn test_summarize_dropped_cache_hit_zero_llm_calls() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(openai_body("cached summary text").as_str())
            .expect(1)
            .create_async()
            .await;
        let llm = LlmHandler::new("m", &server.url(), None).with_max_retries(0);
        let s = ContextSummarizer::new(llm, None).with_threshold(2);
        let dropped = make_dropped(3);
        let first = s.summarize_dropped(&dropped).await.unwrap();
        assert!(first.contains("cached summary text"));
        // 同长度再次调用 → 命中缓存,零 LLM 调用
        let second = s.summarize_dropped(&dropped).await.unwrap();
        assert_eq!(second, first);
        mock.assert_async().await; // expect(1):第二次若再击中服务端则失败
    }

    #[tokio::test]
    async fn test_summarize_dropped_cache_incremental_input() {
        let mut server = mockito::Server::new_async().await;
        // 第一次:全量输入(含 old-0 原文)
        let m1 = server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Regex("old-0".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(openai_body("FIRST-ROUND-SUMMARY").as_str())
            .expect(1)
            .create_async()
            .await;
        // 第二次:增量输入 = 旧摘要(并入 system prompt)+ dropped[frozen..]
        // (含 new-0 与旧摘要正文,不含 old-0 原文)
        let m2 = server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Regex(
                "FIRST-ROUND-SUMMARY.*new-0".to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(openai_body("SECOND-ROUND-SUMMARY").as_str())
            .expect(1)
            .create_async()
            .await;
        let llm = LlmHandler::new("m", &server.url(), None).with_max_retries(0);
        let s = ContextSummarizer::new(llm, None).with_threshold(2);
        let dropped = marker_msgs("old", 5);
        let first = s.summarize_dropped(&dropped).await.unwrap();
        assert!(first.contains("FIRST-ROUND-SUMMARY"));
        // dropped 增长后再次调用:增量输入 = 旧摘要(并入 system prompt)+ dropped[frozen..]
        let mut grown = dropped.clone();
        grown.extend(marker_msgs("new", 3));
        let second = s.summarize_dropped(&grown).await.unwrap();
        assert!(second.contains("SECOND-ROUND-SUMMARY"));
        m1.assert_async().await;
        m2.assert_async().await;
    }

    #[tokio::test]
    async fn test_summarize_dropped_cache_failure_keeps_old_cache() {
        let mut server = mockito::Server::new_async().await;
        // 第一次成功(全量,含 old-0 原文)
        let ok1 = server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Regex("old-0".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(openai_body("FIRST-SUMMARY").as_str())
            .expect(1)
            .create_async()
            .await;
        // 第二次失败(增量请求,500;mock 饱和后不再匹配,第三次落到 ok2)
        let fail = server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Regex("new-0".to_string()))
            .with_status(500)
            .expect(1)
            .create_async()
            .await;
        // 第三次成功:失败后缓存保留,仍从旧 frozen 增量(输入含 new-0 而非全量重算)
        let ok2 = server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Regex("new-0".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(openai_body("SECOND-SUMMARY").as_str())
            .expect(1)
            .create_async()
            .await;
        let llm = LlmHandler::new("m", &server.url(), None).with_max_retries(0);
        let s = ContextSummarizer::new(llm, None).with_threshold(2);
        let dropped = marker_msgs("old", 5);

        let first = s.summarize_dropped(&dropped).await.unwrap();
        assert!(first.contains("FIRST-SUMMARY"));

        // dropped 增长 → 增量请求(500 失败)
        let mut grown = dropped.clone();
        grown.extend(marker_msgs("new", 3));
        assert!(s.summarize_dropped(&grown).await.is_err());

        // 失败后缓存保留:重试仍从旧 frozen 增量(输入含 new-0 而非全量重算)
        let third = s.summarize_dropped(&grown).await.unwrap();
        assert!(third.contains("SECOND-SUMMARY"));

        ok1.assert_async().await;
        fail.assert_async().await;
        ok2.assert_async().await;
    }

    // ========== R3 落链批：元数据 / 代数 / payload 种子（G-3 关闭） ==========

    #[tokio::test]
    async fn test_r3_generated_metadata_and_gen_increment() {
        let mut server = mockito::Server::new_async().await;
        let m1 = server
            .mock("POST", "/")
            .with_body(openai_body("GEN-1-TEXT").as_str())
            .expect(1)
            .create_async()
            .await;
        let m2 = server
            .mock("POST", "/")
            .with_body(openai_body("GEN-2-TEXT").as_str())
            .expect(1)
            .create_async()
            .await;
        let llm = LlmHandler::new("m", &server.url(), None).with_max_retries(0);
        let s = ContextSummarizer::new(llm, None).with_threshold(2);

        // 第一代：frozen 0→3，gen=1，parent=None
        let d1 = make_dropped(3);
        let out1 = s
            .summarize_dropped_with_metadata(&d1, "test goal")
            .await
            .unwrap();
        let meta1 = match &out1 {
            SummarizeOutcome::Generated(m) => m,
            other => panic!("期望 Generated，实得 {other:?}"),
        };
        assert_eq!(meta1.gen, 1);
        assert_eq!(meta1.frozen_len_before, 0);
        assert_eq!(meta1.frozen_len_after, 3);
        assert_eq!(meta1.parent_gen, None);
        assert!(meta1.summary_text.contains("GEN-1"));
        assert!(meta1
            .formatted
            .starts_with("[earlier conversation summary]"));
        assert!(meta1.strategy_fingerprint.contains("threshold=2"));
        m1.assert_async().await;

        // 第二代：frozen 3→5，gen=2，parent=1
        let d2 = make_dropped(5);
        let out2 = s
            .summarize_dropped_with_metadata(&d2, "test goal")
            .await
            .unwrap();
        let meta2 = match &out2 {
            SummarizeOutcome::Generated(m) => m,
            other => panic!("期望 Generated，实得 {other:?}"),
        };
        assert_eq!(meta2.gen, 2);
        assert_eq!(meta2.frozen_len_before, 3);
        assert_eq!(meta2.frozen_len_after, 5);
        assert_eq!(meta2.parent_gen, Some(1));
        assert!(meta2.summary_text.contains("GEN-2"));
        m2.assert_async().await;
    }

    #[test]
    fn test_r3_seed_cache_closes_g3_without_llm() {
        // G-3 关闭证明：payload 回读种子后，同 frozen 长度调用零 LLM 直接命中
        let llm = LlmHandler::new("m", "http://127.0.0.1:9", None).with_max_retries(0);
        let s = ContextSummarizer::new(llm, None).with_threshold(2);
        // 模拟从 payload 回读（gen=2，frozen=5）
        s.seed_cache(5, "SEEDED-FROM-PAYLOAD".to_string(), 2);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let dropped = make_dropped(5);
            let out = s
                .summarize_dropped_with_metadata(&dropped, "test goal")
                .await
                .unwrap();
            match out {
                SummarizeOutcome::CacheHit(formatted) => {
                    assert!(
                        formatted.contains("SEEDED-FROM-PAYLOAD"),
                        "种子文本必须命中"
                    );
                }
                other => panic!("期望 CacheHit，实得 {other:?}"),
            }
        });
        // URL 指向必死端口 + 零网络调用即证 G-3 关闭（不重算）
    }

    #[test]
    fn test_r3_seed_ignores_stale_gen() {
        let llm = LlmHandler::new("m", "http://127.0.0.1:9", None).with_max_retries(0);
        let s = ContextSummarizer::new(llm, None).with_threshold(2);
        s.seed_cache(5, "NEWER".to_string(), 3);
        s.seed_cache(2, "STALE".to_string(), 2); // 旧账回灌防御
        let guard = s.summary_cache.lock().unwrap();
        let c = guard.as_ref().unwrap();
        assert_eq!(c.summary_text, "NEWER");
        assert_eq!(c.gen, 3);
    }
}
