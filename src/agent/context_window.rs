// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 上下文窗口管理 —— token 计数 + 消息裁剪
//!
//! ## G2 目标
//!
//! 防止长对话爆 context window;在接近上限时自动裁剪历史消息,
//! 保留 system + 最近若干轮(含 tool_call / tool_result 原子对)。
//!
//! ## 计数策略
//!
//! 默认 `ApproxTokenCounter`,无外部依赖:
//! - ASCII 字符:4 chars ≈ 1 token
//! - CJK 字符:1 char ≈ 1 token(中文 1 字 ≈ 1 token 是经验值)
//! - 每条消息固定开销 4 token(role 标签 + 结构分隔符)
//! - 旁路图片 base64 按 ASCII 同率折算(4 chars ≈ 1 token;媒体载荷
//!   不计入会让压缩触发对图文消息失明)
//!
//! 精度 ±15%,对裁剪决策足够。如需高精度可后续加 `tiktoken` feature。
//!
//! ## 裁剪策略(KeepSystemKeepLast)
//!
//! 1. 拆分 system / non-system
//! 2. 保留所有 system 消息
//! 3. 从尾部往前累加 non-system,直到接近 budget
//! 4. tool_call 与其紧随的连续 tool_result 段(并行 tool_calls 产生多条)
//!    视为原子组,要么都留要么都丢
//! 5. 中间插入一条 system 提示 "[earlier N messages trimmed]"
//!
//! ## 上下文压缩硬纪律(2026-10-01 立规)
//!
//! 本模块的裁剪是**账后视图装配**:裁剪决策(`trim_detailed`)必须保持
//! 确定性纯函数(同输入同输出,无时钟/随机/外部读取——KeepSystemKeepLast
//! 算法满足);被裁消息(dropped)不得静默丢失,必须交由 summarizer 侧以
//! 原始全文落账(审计链权威面)后才允许从工作记忆移除。任何新的裁剪
//! 策略/窗口策略扩展必须同时维持:①确定性可重放;②被裁内容账上可查;
//! ③装配产物可由「账上事件+纯函数」重建(模型看到什么 = f(账))。

use crate::agent::translator::Message;

/// G10:trim 详细结果(含被丢弃的消息,供 summarizer 使用)
#[derive(Debug, Clone)]
pub struct TrimResult {
    /// 裁剪后的消息列表
    pub messages: Vec<Message>,
    /// 被丢弃的非 system 消息(供 ContextSummarizer 生成摘要)
    pub dropped: Vec<Message>,
}

/// ref_id 段字符白名单(文件名安全;其余替换下划线,段长截断 80;
/// 出站引用化与陈旧降级两面共用的唯一白名单实现)
pub(crate) fn sanitize_ref_segment(call_id: &str) -> String {
    let cleaned: String = call_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut s: String = cleaned.chars().take(80).collect();
    if s.is_empty() {
        s.push_str("call");
    }
    s
}

/// 陈旧工具结果降级的待落盘产物(offload 清单项)
///
/// ref_id 内容寻址(`stale-{digest hex 段}`):同内容同 ref_id——引用文件
/// 幂等覆盖、重放可重建;Tool 消息无 call_id,内容寻址是唯一确定性键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleOffload {
    /// 引用标识(引用文件 stem;hex 段已过 [`sanitize_ref_segment`] 白名单)
    pub ref_id: String,
    /// 工具名(占位块回显)
    pub tool_name: String,
    /// 原文全文(落盘体,与权威历史逐字节一致)
    pub content: String,
    /// 原文摘要(evorule-hash 口径,占位块/取回对账同源)
    pub digest: String,
}

/// G12(B-316):机制标记防伪——引用块真实性校验(纯函数,零 I/O)。
/// 旧判定仅凭 `starts_with(标记)`:内容以标记字面量开头的工具输出/
/// 用户输入会被误判为机制产物(跳过降级/误判已引用化)。现行判定:
/// ①ref_id 与 digest 行齐备;②内容寻址族(stale-/mech-/trim-)ref_id
/// 后缀须与 digest 尾段一致(G15 指纹契约同源);③call-id 族(其余
/// ref_id 形态)须携带 path 行(引用文件指针)。结构不符 = 普通内容,
/// 不触发「已引用化/已降级」旁路。
fn is_authentic_offload_block(content: &str, marker: &str) -> bool {
    let Some(rest) = content.strip_prefix(marker) else {
        return false;
    };
    let mut ref_id = None;
    let mut digest = None;
    let mut has_path = false;
    for line in rest.lines().take(10) {
        if let Some(v) = line.strip_prefix("ref_id: ") {
            ref_id = Some(v.trim());
        } else if let Some(v) = line.strip_prefix("digest: ") {
            digest = Some(v.trim());
        } else if line.starts_with("path: ") {
            has_path = true;
        }
        if ref_id.is_some() && digest.is_some() && has_path {
            break;
        }
    }
    let (Some(r), Some(d)) = (ref_id, digest) else {
        return false;
    };
    let digest_hex = d.rsplit(':').next().unwrap_or(d);
    let content_addressed = ["stale-", "mech-", "trim-"]
        .iter()
        .filter_map(|p| r.strip_prefix(p))
        .any(|hex| {
            !hex.is_empty() && hex == digest_hex && hex.chars().all(|c| c.is_ascii_hexdigit())
        });
    content_addressed || has_path
}

/// 陈旧工具结果发送视图降级(确定性纯函数;无时钟/无随机/无外部读取,
/// 账后视图装配纪律同款:同输入同输出,可由账上原文+本函数重建发送视图)
///
/// 把历史中已消费的陈旧工具结果在发送视图里替换为占位块,原文落盘由调用方
/// 执行(账本原文不动)。三条件全部满足才降级:
/// ① 结果已被消费——其后存在 Assistant 消息;
/// ② 距当前 ≥ `min_turns` 个 assistant 轮——其后 Assistant 消息数 ≥ `min_turns`;
/// ③ 结果字节数 > `min_bytes`。
///
/// 已引用化内容(G12 真实性校验通过的引用块)跳过不二次降级;Tool 消息旁路
/// images 原样保留;只替换 Tool 消息 content,配对 Assistant(tool_calls)
/// 零改动——trim 原子组纪律天然兼容(消息条数不变,下标对齐保持)。
pub fn demote_stale_tool_results(
    messages: &[Message],
    min_turns: usize,
    min_bytes: usize,
) -> (Vec<Message>, Vec<StaleOffload>) {
    let mut view = messages.to_vec();
    let mut offloads: Vec<StaleOffload> = Vec::new();
    for (i, msg) in view.iter_mut().enumerate() {
        let Message::Tool {
            content, tool_name, ..
        } = msg
        else {
            continue;
        };
        // 条件①②:其后 Assistant 消息数(0 = 尚未消费,永不降级)
        let assistants_after = messages[i + 1..]
            .iter()
            .filter(|m| matches!(m, Message::Assistant { .. }))
            .count();
        if assistants_after == 0 || assistants_after < min_turns {
            continue;
        }
        // 条件③:字节数须严格超过阈值(结果比引用块自身头尾还小则降级无收益)
        if content.len() <= min_bytes {
            continue;
        }
        // 已引用化内容跳过(不二次降级);G12(B-316):真实性校验——
        // 仅凭前缀会被同形内容骗过
        if is_authentic_offload_block(content, "[TOOL_RESULT_REF]") {
            continue;
        }
        let digest = crate::agent::journal::evorule_digest(content);
        // 内容寻址:digest hex 段过引用段白名单(与出站引用化同口径)
        let hex_segment = digest.rsplit(':').next().unwrap_or(&digest);
        let ref_id = format!("stale-{}", sanitize_ref_segment(hex_segment));
        let placeholder = format!(
            "[STALE_TOOL_RESULT]\nref_id: {ref_id}\ntool: {tool_name}\nbytes: {}\ndigest: {digest}\n(陈旧工具结果已降级。需要原文时调用 tool_result_fetch 工具,按 ref_id 分页读取。)",
            content.len()
        );
        offloads.push(StaleOffload {
            ref_id,
            tool_name: tool_name.clone(),
            content: content.clone(),
            digest,
        });
        *content = placeholder;
    }
    (view, offloads)
}

/// 五轮裁定批 G8/B-303:trim 丢弃面 offload 产物(`trim-` 前缀内容寻址,
/// stale-/mech- 同族)。"被裁内容必须可查可重建"纪律的全链闭合件——
/// trim 丢弃的消息经此落盘可取回,不再对模型永久不可见。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrimmedOffload {
    /// 引用标识(`trim-{digest hex 段}`,hex 段已过白名单)
    pub ref_id: String,
    /// 落盘体(逐条 JSON 行,消息全字段保真)
    pub body: String,
    /// 落盘体指纹(evorule-hash 口径;fetch 复验同源)
    pub digest: String,
}

/// trim 丢弃消息的确定性 offload 构造(纯函数;同 dropped 同体同 digest):
/// 体 = 逐条 serde_json 单行(保真含 images/tool_calls 元数据),digest =
/// evorule_digest(body),ref_id = `trim-{hex 段}`。空列表返回 None。
pub fn build_trimmed_offload(dropped: &[Message]) -> Option<TrimmedOffload> {
    if dropped.is_empty() {
        return None;
    }
    let lines: Vec<String> = dropped
        .iter()
        .map(|m| serde_json::to_string(m).unwrap_or_default())
        .collect();
    let body = lines.join("\n");
    let digest = crate::agent::journal::evorule_digest(&body);
    let hex_segment = digest.rsplit(':').next().unwrap_or(&digest);
    let ref_id = format!("trim-{}", sanitize_ref_segment(hex_segment));
    Some(TrimmedOffload {
        ref_id,
        body,
        digest,
    })
}

/// trim 丢弃面引用块(占位 hint 用;fetch 入口指引与 stale-/mech- 同文案纪律)
pub fn trimmed_messages_block(off: &TrimmedOffload) -> String {
    format!(
        "[TRIMMED_MESSAGES]\nref_id: {ref}\nbytes: {bytes}\ndigest: {d}\n(此前的对话轮次已裁剪,原文已落盘。需要原文时调用 tool_result_fetch 工具,按 ref_id 分页读取。)",
        bytes = off.body.len(),
        ref = off.ref_id,
        d = off.digest
    )
}

/// 陈旧机制消息发送视图降级(γ 批 B-268,β 同构三条件;确定性纯函数):
/// 目标 = 首条 System(头部原 system)与最后一条 System(常驻最新机制
/// 消息)之外的 System 消息。三条件全部满足才降级:①已消费——其后存在
/// Assistant 消息;②距当前 ≥ `min_turns` 个 assistant 轮;③字节数 >
/// `min_bytes`。降级为 digest 引用块(`mech-` 前缀内容寻址,Tool 消息
/// stale- 前缀同族),原文落盘由调用方执行(fail-soft);禁写 journal
/// (与 β 同纪律)。已降级占位块跳过不二次降级;消息条数不变。
pub fn demote_stale_mechanism_messages(
    messages: &[Message],
    min_turns: usize,
    min_bytes: usize,
) -> (Vec<Message>, Vec<StaleOffload>) {
    let system_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m, Message::System { .. }))
        .map(|(i, _)| i)
        .collect();
    let first_sys = system_indices.first().copied();
    let last_sys = system_indices.last().copied();
    let mut view = messages.to_vec();
    let mut offloads: Vec<StaleOffload> = Vec::new();
    for (i, msg) in view.iter_mut().enumerate() {
        let Message::System { content, .. } = msg else {
            continue;
        };
        // 两端排除:首条 = 头部原 system,末条 = 常驻最新机制消息
        if Some(i) == first_sys || Some(i) == last_sys {
            continue;
        }
        // 条件①②:其后 Assistant 消息数(0 = 尚未消费,永不降级)
        let assistants_after = messages[i + 1..]
            .iter()
            .filter(|m| matches!(m, Message::Assistant { .. }))
            .count();
        if assistants_after == 0 || assistants_after < min_turns {
            continue;
        }
        // 条件③:字节数须严格超过阈值(引用块自身头尾比原文还小则降级无收益)
        if content.len() <= min_bytes {
            continue;
        }
        // 已降级占位块跳过(不二次降级);G12(B-316):指纹契约校验——
        // ref_id 后缀与 digest 尾段一致才是真降级块
        if is_authentic_offload_block(content, "[STALE_MECHANISM_MESSAGE]") {
            continue;
        }
        let digest = crate::agent::journal::evorule_digest(content);
        let hex_segment = digest.rsplit(':').next().unwrap_or(&digest);
        let ref_id = format!("mech-{}", sanitize_ref_segment(hex_segment));
        let placeholder = format!(
            "[STALE_MECHANISM_MESSAGE]\nref_id: {ref_id}\ntool: mechanism\nbytes: {}\ndigest: {digest}\n(陈旧机制消息已降级。需要原文时调用 tool_result_fetch 工具,按 ref_id 分页读取。)",
            content.len()
        );
        offloads.push(StaleOffload {
            ref_id,
            tool_name: "mechanism".to_string(),
            content: content.clone(),
            digest,
        });
        *content = placeholder;
    }
    (view, offloads)
}

/// Token 计数器 trait
pub trait TokenCounter: Send + Sync + std::fmt::Debug {
    /// 估算 messages 数组的总 token 数(含 role 开销 + 结尾标记)
    fn count_messages(&self, messages: &[Message]) -> usize;

    /// 估算单条消息的 token 数
    fn count_message(&self, msg: &Message) -> usize {
        self.count_messages(std::slice::from_ref(msg))
    }
}

/// 近似计数器(无外部依赖,CJK-aware)
///
/// 适用场景:开发/测试/不允许引入 tiktoken 依赖时。
/// 精度:±15%,对裁剪决策足够。
#[derive(Debug, Clone)]
pub struct ApproxTokenCounter {
    /// 每条消息的固定开销(role 标签 + 结构分隔符)
    per_message_overhead: usize,
    /// ASCII 字符→token 比例的倒数(4 = 4 chars/token)
    ascii_chars_per_token: usize,
}

impl Default for ApproxTokenCounter {
    fn default() -> Self {
        Self {
            per_message_overhead: 4, // <|im_start|>role\n ... <|im_end|>\n
            ascii_chars_per_token: 4,
        }
    }
}

impl ApproxTokenCounter {
    /// 创建默认配置的计数器
    pub fn new() -> Self {
        Self::default()
    }

    /// 估算一段文本的 token 数(CJK-aware)
    ///
    /// - CJK 字符(含全角标点、日韩文):1 char ≈ 1 token
    /// - 其他字符(ASCII / 拉丁):4 chars ≈ 1 token
    fn count_text(&self, s: &str) -> usize {
        let mut cjk = 0usize;
        let mut other = 0usize;
        for ch in s.chars() {
            if is_cjk(ch) {
                cjk += 1;
            } else {
                other += 1;
            }
        }
        let ascii_tokens = other / self.ascii_chars_per_token.max(1);
        // 余数向上取整(零碎字符也算 1 token)
        let ascii_remainder = if !other.is_multiple_of(self.ascii_chars_per_token.max(1)) {
            1
        } else {
            0
        };
        cjk + ascii_tokens + ascii_remainder
    }
}

/// 判断字符是否属于 CJK(中文/日文/韩文)或全角标点
///
/// 这些字符在 BPE 分词后通常 1 char ≈ 1 token。
fn is_cjk(ch: char) -> bool {
    let c = ch as u32;
    // CJK 统一表意文字(常用汉字)
    (0x4E00..=0x9FFF).contains(&c)
    // CJK 扩展 A
    || (0x3400..=0x4DBF).contains(&c)
    // CJK 兼容表意文字
    || (0xF900..=0xFAFF).contains(&c)
    // 平假名
    || (0x3040..=0x309F).contains(&c)
    // 片假名
    || (0x30A0..=0x30FF).contains(&c)
    // 韩文音节
    || (0xAC00..=0xD7AF).contains(&c)
    // 全角标点(CJK Symbols and Punctuation)
    || (0x3000..=0x303F).contains(&c)
    // 半角/全角形式
    || (0xFF00..=0xFFEF).contains(&c)
}

impl TokenCounter for ApproxTokenCounter {
    fn count_messages(&self, messages: &[Message]) -> usize {
        if messages.is_empty() {
            return 0;
        }
        let mut total = 0usize;
        for msg in messages {
            total += self.per_message_overhead;
            total += self.count_text(msg.content());
            // 图片旁路 base64 计入(User/Tool 两变体;base64 长度按 ASCII 同率
            // 折算 4 chars/token)。不计入会让百分比触发的压缩/裁剪对媒体载荷
            // 失明(几 MB 截图在计数里≈0);折算方向保守(宁早触发不晚触发)
            match msg {
                Message::User { images, .. } | Message::Tool { images, .. } => {
                    for img in images {
                        total += self.count_text(&img.data_base64);
                    }
                }
                _ => {}
            }
            if let Message::Assistant {
                tool_calls: Some(calls),
                ..
            } = msg
            {
                for tc in calls {
                    // tool_name + arguments JSON
                    total += self.count_text(&tc.name);
                    let args_str = tc.arguments.to_string();
                    total += self.count_text(&args_str);
                }
            }
            if let Message::Tool { tool_name, .. } = msg {
                total += self.count_text(tool_name);
            }
        }
        // 结尾 <|start|>assistant<|message|> 等
        total + 3
    }
}

/// 裁剪策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrimStrategy {
    /// 保留 system + 最近 N 条(丢弃中间),tool_call/tool_result 原子对
    #[default]
    KeepSystemKeepLast,
    /// 不裁剪(让 LLM 自己报错,仅计数用于观测)
    None,
}

/// 上下文窗口管理器
#[derive(Debug)]
pub struct ContextWindowManager {
    counter: Box<dyn TokenCounter>,
    max_tokens: usize,
    /// 预留给响应的 token 数(默认 max_tokens 的 1/4)
    reserve_for_response: usize,
    strategy: TrimStrategy,
}

impl ContextWindowManager {
    /// 创建一个 manager
    ///
    /// - `max_tokens`:模型 context window 大小(如 8192)
    /// - `reserve_for_response`:预留给响应的 token 数(如 2048),
    ///   实际可用的输入 token = `max_tokens - reserve_for_response`
    pub fn new(
        counter: Box<dyn TokenCounter>,
        max_tokens: usize,
        reserve_for_response: usize,
        strategy: TrimStrategy,
    ) -> Self {
        Self {
            counter,
            max_tokens,
            reserve_for_response,
            strategy,
        }
    }

    /// 使用默认 `ApproxTokenCounter` 构造
    pub fn with_approx_counter(
        max_tokens: usize,
        reserve_for_response: usize,
        strategy: TrimStrategy,
    ) -> Self {
        Self::new(
            Box::new(ApproxTokenCounter::new()),
            max_tokens,
            reserve_for_response,
            strategy,
        )
    }

    /// 实际可用于输入消息的 token 预算
    pub fn budget(&self) -> usize {
        self.max_tokens.saturating_sub(self.reserve_for_response)
    }

    /// 模型上下文窗口总大小(观测显示用;与 agent_api 暴露的窗口口径同源)
    pub fn window_tokens(&self) -> usize {
        self.max_tokens
    }

    /// 估算 messages 的 token 数
    pub fn count(&self, messages: &[Message]) -> usize {
        self.counter.count_messages(messages)
    }

    /// 裁剪 messages 使其 token 数 ≤ budget
    ///
    /// 返回 `(裁剪后的 messages, dropped_count)`。
    /// `dropped_count` 是被丢弃的非 system 消息条数(不含插入的提示)。
    pub fn trim(&self, messages: &[Message]) -> (Vec<Message>, usize) {
        let result = self.trim_detailed(messages);
        (result.messages, result.dropped.len())
    }

    /// G10:裁剪并返回详细结果(含被丢弃的消息)
    ///
    /// 与 `trim()` 逻辑相同,但把被丢弃的非 system 消息收集到 `dropped` 字段,
    /// 供 `ContextSummarizer` 生成摘要。`trim()` 内部调此方法。
    pub fn trim_detailed(&self, messages: &[Message]) -> TrimResult {
        let budget = self.budget();
        let total = self.counter.count_messages(messages);

        if total <= budget {
            return TrimResult {
                messages: messages.to_vec(),
                dropped: Vec::new(),
            };
        }

        match self.strategy {
            TrimStrategy::None => TrimResult {
                messages: messages.to_vec(),
                dropped: Vec::new(),
            },
            TrimStrategy::KeepSystemKeepLast => self.trim_keep_system_keep_last(messages, budget),
        }
    }

    /// 实现策略:保留 system + 最近若干轮(tool_call/tool_result 原子对)
    /// γ 批 B-268:机制消息独立原子组——头部原 system(首条 System)全保;
    /// 机制消息(首条之后的 System 消息)只保最新一条常驻,其余进入可淘汰面
    /// (发送视图已先行按 β 同构三条件降级为 digest 引用块;此处作为普通
    /// 池成员参与尾部累加,预算不足时丢弃)
    fn trim_keep_system_keep_last(&self, messages: &[Message], budget: usize) -> TrimResult {
        // 1. 拆分:头部原 system / 机制消息 / 可淘汰池(保原序)
        let mut base_system: Option<Message> = None;
        let mut mech: Vec<(usize, Message)> = Vec::new();
        let mut pool: Vec<(usize, Message)> = Vec::new(); // (原 idx, msg)
        for (i, msg) in messages.iter().enumerate() {
            match msg {
                Message::System { .. } if base_system.is_none() => {
                    base_system = Some(msg.clone());
                }
                Message::System { .. } => mech.push((i, msg.clone())),
                _ => pool.push((i, msg.clone())),
            }
        }
        // 最新一条机制消息常驻(与头部原 system 同待遇);其余机制消息进池
        let latest_mech = mech.pop();
        pool.extend(mech);
        pool.sort_by_key(|(i, _)| *i);

        let mut always: Vec<Message> = Vec::new();
        if let Some(b) = &base_system {
            always.push(b.clone());
        }
        if let Some((_, m)) = &latest_mech {
            always.push(m.clone());
        }

        if pool.is_empty() {
            return TrimResult {
                messages: always,
                dropped: Vec::new(),
            };
        }

        // 2. 计算常驻面占用的 token
        let always_tokens = self.counter.count_messages(&always);
        // hint 消息约 13 token("[earlier N messages trimmed due to context window limit]")
        // buffer 按 budget 的 5% 预留,最低 5 token,应对计数误差
        let hint_overhead = 15usize;
        let buffer = (budget / 20).max(5);
        let available_for_recent = budget
            .saturating_sub(always_tokens)
            .saturating_sub(hint_overhead)
            .saturating_sub(buffer);

        if available_for_recent == 0 {
            // 极端情况:常驻面本身已占满 budget。
            // 四轮审计 G16(B-285):不得把末条 User 一起丢。分层保底:
            // ① base+latest_mech+末条 User 都放得下 → 全保(γ 契约不破);
            // ② 否则 latest_mech 让位,保底 base+末条 User+hint;
            // ③ base+末条 User 仍超预算(系统提示本身超大)或无 User → 原行为。
            let last_user = pool
                .iter()
                .rev()
                .find(|(_, m)| matches!(m, Message::User { .. }))
                .cloned();
            if let Some((user_idx, last_user_msg)) = last_user {
                let mut base_only: Vec<Message> = Vec::new();
                if let Some(b) = &base_system {
                    base_only.push(b.clone());
                }
                let base_tokens = self.counter.count_messages(&base_only);
                let user_tokens = self
                    .counter
                    .count_messages(std::slice::from_ref(&last_user_msg));
                let mech_tokens = latest_mech
                    .as_ref()
                    .map(|(_, m)| self.counter.count_messages(std::slice::from_ref(m)))
                    .unwrap_or(0);
                // ① γ 契约优先:三者都放得下就全保
                if base_tokens + mech_tokens + user_tokens <= budget {
                    let dropped: Vec<Message> = pool
                        .into_iter()
                        .filter(|(idx, _)| *idx != user_idx)
                        .map(|(_, m)| m)
                        .collect();
                    let dropped_count = dropped.len();
                    let mut result = always;
                    if dropped_count > 0 {
                        result.push(Message::System {
                            content: format!(
                                "[earlier {dropped_count} messages trimmed due to context window limit]"
                            ),
                        });
                    }
                    result.push(last_user_msg);
                    return TrimResult {
                        messages: result,
                        dropped,
                    };
                }
                // ② latest_mech 让位,保底末条 User
                if base_tokens + user_tokens <= budget {
                    let dropped: Vec<Message> = pool
                        .into_iter()
                        .filter(|(idx, _)| *idx != user_idx)
                        .map(|(_, m)| m)
                        .collect();
                    let dropped_count = dropped.len();
                    let mut result = base_only;
                    if dropped_count > 0 {
                        result.push(Message::System {
                            content: format!(
                                "[earlier {dropped_count} messages trimmed due to context window limit]"
                            ),
                        });
                    }
                    result.push(last_user_msg);
                    return TrimResult {
                        messages: result,
                        dropped,
                    };
                }
            }
            // ③ 原行为兜底:只能保留常驻面
            let dropped: Vec<Message> = pool.into_iter().map(|(_, m)| m).collect();
            let dropped_count = dropped.len();
            let mut result = always;
            if dropped_count > 0 {
                result.push(Message::System {
                    content: format!(
                        "[earlier {} messages trimmed due to context window limit]",
                        dropped_count
                    ),
                });
            }
            return TrimResult {
                messages: result,
                dropped,
            };
        }

        // 3. 从尾部往前累加,tool_call/tool_result 视为原子组
        let mut kept: Vec<(usize, Message)> = Vec::new();
        let mut kept_tokens = 0usize;
        let mut i = pool.len();
        while i > 0 {
            i -= 1;
            let (_, msg) = &pool[i];
            // 如果是 tool_result(Message::Tool),它所属的连续 Tool 结果段必须与
            // 发起 tool_calls 的 assistant 整体成组:并行 tool_calls 会产生多条
            // 连续 Tool 消息,只合并紧邻 assistant 会在段中间切开,留下孤立的
            // tool_result(LLM 收到无配对 tool_call_id 的 tool 消息会报错)
            let mut group: Vec<(usize, Message)> = vec![pool[i].clone()];
            if let Message::Tool { .. } = msg {
                // 向前收集连续 Tool 段(多并行 tool_calls 的结果队列)
                while i > 0 && matches!(pool[i - 1].1, Message::Tool { .. }) {
                    group.insert(0, pool[i - 1].clone());
                    i -= 1;
                }
                // 段前紧邻的 Assistant(含 tool_calls)一并成组
                let mut has_caller = false;
                if i > 0 {
                    if let Message::Assistant { tool_calls, .. } = &pool[i - 1].1 {
                        if tool_calls.as_ref().map(|c| !c.is_empty()).unwrap_or(false) {
                            group.insert(0, pool[i - 1].clone());
                            i -= 1; // 跳过已合并的 assistant
                            has_caller = true;
                        }
                    }
                }
                if !has_caller {
                    // 四轮审计 G17(B-285):孤儿 Tool 段(前邻非 Assistant
                    // (tool_calls))整组放弃——发出即 wire 孤儿(tool_call_id
                    // 无配对),上游 400。段留在可淘汰池,自然落入 dropped。
                    continue;
                }
            }
            // 估算 group 的 token
            let group_msgs: Vec<Message> = group.iter().map(|(_, m)| m.clone()).collect();
            let group_tokens = self.counter.count_messages(&group_msgs);
            if kept_tokens + group_tokens > available_for_recent && !kept.is_empty() {
                // 加上这个 group 会超预算,停止
                break;
            }
            kept_tokens += group_tokens;
            // prepend(kept 是反序累加,最后要反转)
            let mut new_kept = group;
            new_kept.extend(kept);
            kept = new_kept;
        }

        // 4. 收集被丢弃的消息(可淘汰池中不在 kept 中的)
        let kept_indices: std::collections::HashSet<usize> =
            kept.iter().map(|(idx, _)| *idx).collect();
        let dropped: Vec<Message> = pool
            .iter()
            .filter(|(idx, _)| !kept_indices.contains(idx))
            .map(|(_, m)| m.clone())
            .collect();
        let dropped_count = dropped.len();

        // 5. 组装结果:常驻面 + (可选 hint) + kept(按原顺序)
        // 注意:hint 插在常驻面和 kept 之间,kept 的最后一条是最近的消息
        let mut result = always;
        if dropped_count > 0 {
            result.push(Message::System {
                content: format!(
                    "[earlier {} messages trimmed due to context window limit]",
                    dropped_count
                ),
            });
        }
        for (_, m) in kept {
            result.push(m);
        }

        TrimResult {
            messages: result,
            dropped,
        }
    }
}

impl ContextWindowManager {
    /// 四轮审计 G13(B-285):摘要回填前的预算收顶。
    ///
    /// trim 只为 hint 预留 `hint_overhead=15` token,摘要回填若不限长,
    /// 刚 fit 的发送视图会被重新撑爆。本方法按当前预算对摘要文本收顶:
    ///
    /// - 本就放得下 → 原样放行;
    /// - 超限 → 按 1/4 步长收缩字符数直到放得下;
    /// - 收缩到 64 字符仍放不下 → 返回 fallback(调用方以原 hint 兜底)。
    ///
    /// `base_view` 为不含摘要槽位的发送视图(调用方先摘除 hint)。
    pub fn cap_summary_for_budget(
        &self,
        formatted: &str,
        fallback: &str,
        base_view: &[Message],
    ) -> String {
        let budget = self.max_tokens.saturating_sub(self.reserve_for_response);
        let fits = |summary: &str| {
            let mut probe = base_view.to_vec();
            probe.push(Message::System {
                content: summary.to_string(),
            });
            self.counter.count_messages(&probe) <= budget
        };
        if fits(formatted) {
            return formatted.to_string();
        }
        let mut candidate = formatted.to_string();
        while candidate.chars().count() > 64 {
            let len = candidate.chars().count();
            let keep = len - (len / 4).max(1);
            candidate = candidate.chars().take(keep).collect();
            if fits(&candidate) {
                return candidate;
            }
        }
        fallback.to_string()
    }
}

impl Clone for ContextWindowManager {
    /// Clone 时新建 Box(因为 `Box<dyn Trait>` 不自动 Clone)
    fn clone(&self) -> Self {
        // ApproxTokenCounter 是默认且唯一实现,直接复用
        // 如果未来加更多实现,需要用 Any 或 trait_clone 模式
        Self {
            counter: Box::new(ApproxTokenCounter::new()),
            max_tokens: self.max_tokens,
            reserve_for_response: self.reserve_for_response,
            strategy: self.strategy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::translator::{ImageData, ToolCall};
    use serde_json::json;

    /// G8/B-303:trim offload 确定性——同输入同 ref_id 同体同指纹;
    /// 体逐条 JSON 行保真(含 images/tool_calls 元数据)
    #[test]
    fn test_trimmed_offload_deterministic_and_faithful() {
        let dropped = vec![
            user("第一轮用户输入"),
            Message::Assistant {
                content: "助手回复".to_string(),
                tool_calls: Some(vec![ToolCall {
                    id: Some("call-1".to_string()),
                    name: "bash".to_string(),
                    arguments: serde_json::Value::String("{}".to_string()),
                }]),
            },
        ];
        let a = build_trimmed_offload(&dropped).expect("非空 dropped 必有 offload");
        let b = build_trimmed_offload(&dropped).expect("重放一致");
        assert_eq!(a, b, "同 dropped 同产物(纯函数确定性)");
        assert!(a.ref_id.starts_with("trim-"));
        assert!(
            a.ref_id[5..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "ref_id 段过白名单"
        );
        let body_lines: Vec<&str> = a.body.lines().collect();
        assert_eq!(body_lines.len(), 2, "逐条一行 jsonl");
        assert!(body_lines[0].contains("第一轮用户输入"), "content 保真");
        assert!(body_lines[1].contains("call-1"), "tool_calls 元数据保真");
        let digest_hex = a.digest.rsplit(':').next().unwrap();
        assert!(a.ref_id.contains(&digest_hex[..digest_hex.len().min(16)]));
    }

    /// G8/B-303:空 dropped = None;引用块含 ref_id/bytes/digest/取回指引
    #[test]
    fn test_trimmed_offload_empty_none_and_block_shape() {
        assert!(build_trimmed_offload(&[]).is_none());
        let off = build_trimmed_offload(&[user("hello")]).unwrap();
        let block = trimmed_messages_block(&off);
        assert!(block.starts_with("[TRIMMED_MESSAGES]"));
        assert!(block.contains(&format!("ref_id: {}", off.ref_id)));
        assert!(block.contains(&format!("bytes: {}", off.body.len())));
        assert!(block.contains(&off.digest));
        assert!(block.contains("tool_result_fetch"), "取回入口指引在场");
    }

    fn user(content: &str) -> Message {
        Message::User {
            content: content.to_string(),
            images: Vec::new(),
        }
    }

    fn tool(content: &str) -> Message {
        Message::Tool {
            content: content.to_string(),
            tool_name: "echo".to_string(),
            images: Vec::new(),
        }
    }

    fn four_consumed_assistants() -> Vec<Message> {
        vec![
            assistant_with_tool_call("消费轮1", "x"),
            assistant_with_tool_call("消费轮2", "x"),
            assistant_with_tool_call("消费轮3", "x"),
            assistant_with_tool_call("消费轮4", "x"),
        ]
    }

    // ========== G12(B-316):机制标记防伪(引用块真实性校验) ==========

    /// 同形伪装:Tool 消息内容以 [TOOL_RESULT_REF] 字面量开头但无引用块
    /// 结构(无 ref_id/digest/path 行)——不得被误判为已引用化而跳过降级,
    /// 须按普通陈旧内容降级(内容同形伪造面)。
    #[test]
    fn test_g12_fake_marker_tool_result_still_demoted() {
        let fake = "[TOOL_RESULT_REF]\n用户粘贴的同形文本,无引用块结构。\n".repeat(40);
        let mut messages = vec![system("base"), tool(&fake)];
        messages.extend(four_consumed_assistants());
        let (view, offloads) = demote_stale_tool_results(&messages, 4, 100);
        assert_eq!(offloads.len(), 1, "同形伪装内容必须被降级");
        assert!(
            view[1].content().starts_with("[STALE_TOOL_RESULT]"),
            "降级后为 stale 引用块: {}",
            view[1].content()
        );
    }

    /// 真引用块(call-id 族,带 ref_id/digest/path 行)仍跳过二次降级
    /// (回归锁:防伪校验不得误伤真块)。
    #[test]
    fn test_g12_genuine_tool_ref_block_still_skipped() {
        let genuine = "[TOOL_RESULT_REF]\nref_id: call-1-1700000000\ntool: bash\nbytes: 100\nchars: 90\ndigest: sha3:abc123\npath: tool_results/call-1-1700000000.txt\nhead:\n数据...\n".repeat(8);
        let mut messages = vec![system("base"), tool(&genuine)];
        messages.extend(four_consumed_assistants());
        let (view, offloads) = demote_stale_tool_results(&messages, 4, 100);
        assert_eq!(offloads.len(), 0, "真引用块不二次降级");
        assert!(
            view[1].content().starts_with("[TOOL_RESULT_REF]"),
            "内容原样保留"
        );
    }

    /// 同形伪装:System 机制消息内容以 [STALE_MECHANISM_MESSAGE] 开头但
    /// ref_id 后缀与 digest 尾段不一致(指纹契约不成立)——按普通陈旧
    /// 机制消息降级,不再凭前缀误判为已降级块。
    #[test]
    fn test_g12_inconsistent_stale_mech_block_still_demoted() {
        let fake = "[STALE_MECHANISM_MESSAGE]\nref_id: stale-0000\ntool: mechanism\nbytes: 1\ndigest: sha3:beef\n(伪装的同形文本。)\n".repeat(20);
        let mut messages = vec![system("base"), system(&fake)];
        messages.extend(four_consumed_assistants());
        messages.push(system("最新机制消息"));
        let (view, offloads) = demote_stale_mechanism_messages(&messages, 4, 100);
        assert_eq!(offloads.len(), 1, "指纹不一致的同形块必须被降级");
        assert!(
            view[1]
                .content()
                .starts_with("[STALE_MECHANISM_MESSAGE]\nref_id: mech-"),
            "降级后为 mech 引用块: {}",
            view[1].content()
        );
    }

    /// 真降级块(ref_id 后缀= digest 尾段,G15 指纹契约同源)仍跳过
    /// 二次降级(回归锁)。
    #[test]
    fn test_g12_genuine_stale_mech_block_still_skipped() {
        let hex = "abcd1234abcd1234abcd1234abcd1234";
        let genuine = format!("[STALE_MECHANISM_MESSAGE]\nref_id: stale-{hex}\ntool: mechanism\nbytes: 1\ndigest: sha3:{hex}\n(陈旧机制消息已降级。)\n").repeat(20);
        let mut messages = vec![system("base"), system(&genuine)];
        messages.extend(four_consumed_assistants());
        messages.push(system("最新机制消息"));
        let (view, offloads) = demote_stale_mechanism_messages(&messages, 4, 100);
        assert_eq!(offloads.len(), 0, "真降级块不二次降级");
        assert!(
            view[1].content().contains("ref_id: stale-abcd1234"),
            "内容原样保留"
        );
    }

    fn system(content: &str) -> Message {
        Message::System {
            content: content.to_string(),
        }
    }

    fn assistant_with_tool_call(content: &str, tool_name: &str) -> Message {
        Message::Assistant {
            content: content.to_string(),
            tool_calls: Some(vec![ToolCall {
                name: tool_name.to_string(),
                arguments: json!({"q": "test"}),
                id: None,
            }]),
        }
    }

    fn tool_result(content: &str, tool_name: &str) -> Message {
        Message::Tool {
            content: content.to_string(),
            tool_name: tool_name.to_string(),
            images: Vec::new(),
        }
    }

    // ========== ApproxTokenCounter 测试 ==========

    #[test]
    fn test_count_empty() {
        let c = ApproxTokenCounter::new();
        assert_eq!(c.count_messages(&[]), 0);
    }

    #[test]
    fn test_count_ascii() {
        // 4 chars ≈ 1 token,per_message_overhead=4,结尾 +3
        // "hello" = 5 chars → 2 tokens (5/4=1 余 1,向上取整)
        let c = ApproxTokenCounter::new();
        let msgs = vec![user("hello")];
        let total = c.count_messages(&msgs);
        // 4 (overhead) + 2 (text) + 3 (tail) = 9
        assert_eq!(total, 9);
    }

    #[test]
    fn test_count_cjk_one_char_per_token() {
        // 中文 1 char ≈ 1 token
        let c = ApproxTokenCounter::new();
        let msgs = vec![user("你好世界")]; // 4 个汉字 → 4 tokens
        let total = c.count_messages(&msgs);
        // 4 (overhead) + 4 (cjk) + 3 (tail) = 11
        assert_eq!(total, 11);
    }

    #[test]
    fn test_count_mixed_cjk_ascii() {
        let c = ApproxTokenCounter::new();
        // "你好 hello" = 2 CJK + 6 ASCII (含空格)
        // CJK: 2 tokens
        // ASCII: 6/4 = 1 余 2 → 2 tokens
        // 总 text: 4 tokens
        let msgs = vec![user("你好 hello")];
        let total = c.count_messages(&msgs);
        // 4 (overhead) + 4 (text) + 3 (tail) = 11
        assert_eq!(total, 11);
    }

    #[test]
    fn test_count_assistant_with_tool_calls() {
        let c = ApproxTokenCounter::new();
        let msgs = vec![assistant_with_tool_call("calling", "search")];
        let total = c.count_messages(&msgs);
        // overhead 4 + content "calling" (7 chars → 2 tokens)
        // + tool_name "search" (6 chars → 2 tokens)
        // + args {"q":"test"} (12 chars → 3 tokens)
        // + tail 3
        // = 4 + 2 + 2 + 3 + 3 = 14
        assert_eq!(total, 14);
    }

    // ========== 图片旁路 base64 计入测试 ==========

    /// 媒体载荷失明回归锁:User 轮旁路图 base64 必须按长度折算进计数
    /// (几 MB 截图在 token 计数里≈0 会让百分比触发对图片载荷失明)
    #[test]
    fn test_count_user_image_base64_counted() {
        let c = ApproxTokenCounter::new();
        // 400 个 ASCII 字符(base64 形态)→ 100 tokens
        let b64 = "QUJD".repeat(100);
        let msgs = vec![Message::User {
            content: "hello".to_string(),
            images: vec![ImageData {
                mime_type: "image/png".to_string(),
                data_base64: b64,
            }],
        }];
        // 4 (overhead) + 2 (text) + 100 (image) + 3 (tail) = 109
        assert_eq!(c.count_messages(&msgs), 109);
    }

    /// Tool 变体旁路图同样计入
    #[test]
    fn test_count_tool_image_base64_counted() {
        let c = ApproxTokenCounter::new();
        let b64 = "QUJD".repeat(100); // 400 chars → 100 tokens
        let msgs = vec![Message::Tool {
            content: "hi".to_string(),     // 2 chars → 1 token
            tool_name: "echo".to_string(), // 4 chars → 1 token
            images: vec![ImageData {
                mime_type: "image/png".to_string(),
                data_base64: b64,
            }],
        }];
        // 4 (overhead) + 1 (content) + 1 (tool_name) + 100 (image) + 3 (tail) = 109
        assert_eq!(c.count_messages(&msgs), 109);
    }

    /// 无图消息计数与既有口径逐 token 一致(基线保护)
    #[test]
    fn test_count_without_images_unchanged() {
        let c = ApproxTokenCounter::new();
        let msgs = vec![user("hello")];
        assert_eq!(c.count_messages(&msgs), 9);
    }

    // ========== ContextWindowManager.trim 测试 ==========

    #[test]
    fn test_trim_no_trimming_when_under_budget() {
        let mgr = ContextWindowManager::with_approx_counter(8192, 2048, TrimStrategy::default());
        let msgs = vec![system("sys"), user("hi")];
        let (trimmed, dropped) = mgr.trim(&msgs);
        assert_eq!(dropped, 0);
        assert_eq!(trimmed.len(), 2);
    }

    #[test]
    fn test_trim_keeps_system_when_over_budget() {
        // 极小 budget,强制裁剪
        let mgr = ContextWindowManager::with_approx_counter(
            50, // 总 budget
            10, // reserve
            TrimStrategy::KeepSystemKeepLast,
        );
        // budget = 40
        // system "sys prompt long enough" ≈ overhead 4 + 6 tokens = 10
        // 每条 user "hello" ≈ 4 + 2 = 6 tokens
        let msgs = vec![
            system("sys prompt long enough"),
            user("hello"),
            user("hello"),
            user("hello"),
            user("hello"),
            user("hello"),
            user("hello"),
        ];
        let (trimmed, dropped) = mgr.trim(&msgs);
        assert!(dropped > 0, "should drop some messages");
        // system 必须保留
        assert!(matches!(trimmed[0], Message::System { .. }));
        // 最后一条必须是最近的 user("hello")
        assert!(matches!(trimmed.last(), Some(Message::User { .. })));
        // 应包含 hint 消息
        let has_hint = trimmed
            .iter()
            .any(|m| m.content().contains("[earlier") && m.content().contains("messages trimmed"));
        assert!(has_hint, "should contain trim hint");
    }

    #[test]
    fn test_trim_tool_call_pair_atomic() {
        // tool_call(assistant) + tool_result(tool) 必须一起保留或一起丢
        let mgr =
            ContextWindowManager::with_approx_counter(60, 10, TrimStrategy::KeepSystemKeepLast);
        // budget = 50
        let msgs = vec![
            system("sys"),
            user("q1"),
            assistant_with_tool_call("thinking", "search"),
            tool_result("result1", "search"),
            user("q2"),
            assistant_with_tool_call("thinking2", "search"),
            tool_result("result2", "search"),
            user("q3"),
        ];
        let (trimmed, dropped) = mgr.trim(&msgs);
        let _ = dropped;

        // 验证:如果有 tool_result,前面必有对应的 assistant(tool_calls)
        for i in 0..trimmed.len() {
            if let Message::Tool { .. } = &trimmed[i] {
                assert!(i > 0, "tool_result at index {} has no preceding message", i);
                match &trimmed[i - 1] {
                    Message::Assistant { tool_calls, .. } => {
                        assert!(
                            tool_calls.as_ref().map(|c| !c.is_empty()).unwrap_or(false),
                            "tool_result at {} is preceded by assistant without tool_calls",
                            i
                        );
                    }
                    _ => panic!(
                        "tool_result at {} is preceded by non-assistant message: {:?}",
                        i,
                        trimmed[i - 1]
                    ),
                }
            }
        }
    }

    #[test]
    fn test_trim_parallel_tool_calls_group_atomic() {
        // 多并行 tool_calls:assistant(tc×3) + 3 条连续 tool_result 是一个原子组,
        // 裁剪边界切进组中间时不得留下孤立的 tool_result
        let mgr =
            ContextWindowManager::with_approx_counter(40, 10, TrimStrategy::KeepSystemKeepLast);
        // budget = 30;system "sys" ≈ 8 token,available_for_recent ≈ 2 token,
        // 尾部工具组整体超预算,但必须整组保留(不得切进组中间)
        let three_calls = Message::Assistant {
            content: "calling".to_string(),
            tool_calls: Some(vec![
                ToolCall {
                    name: "search".to_string(),
                    arguments: json!({"q": "test"}),
                    id: None,
                },
                ToolCall {
                    name: "search".to_string(),
                    arguments: json!({"q": "test"}),
                    id: None,
                },
                ToolCall {
                    name: "search".to_string(),
                    arguments: json!({"q": "test"}),
                    id: None,
                },
            ]),
        };
        let msgs = vec![
            system("sys"),
            user("q1"),
            three_calls,
            tool_result("result1", "search"),
            tool_result("result2", "search"),
            tool_result("result3", "search"),
        ];
        let (trimmed, dropped) = mgr.trim(&msgs);
        assert_eq!(dropped, 1, "only the earliest user msg should drop");
        // 原子性:3 条 tool_result 必须全部一起保留
        let tool_count = trimmed
            .iter()
            .filter(|m| matches!(m, Message::Tool { .. }))
            .count();
        assert_eq!(
            tool_count, 3,
            "all 3 parallel tool results must survive together"
        );
        // 配对完整性:每条连续 tool_result 段的段头前面必须是有 tool_calls 的
        // assistant(段中消息的合法前驱就是前一条 tool_result,无需检查)
        for i in 0..trimmed.len() {
            if !matches!(trimmed[i], Message::Tool { .. }) {
                continue;
            }
            if i > 0 && matches!(trimmed[i - 1], Message::Tool { .. }) {
                continue;
            }
            let prev = i.checked_sub(1).map(|p| &trimmed[p]);
            match prev {
                Some(Message::Assistant { tool_calls, .. }) => {
                    assert!(
                        tool_calls.as_ref().map(|c| !c.is_empty()).unwrap_or(false),
                        "tool_result run head at {} preceded by assistant without tool_calls",
                        i
                    );
                }
                other => panic!(
                    "orphan tool_result run at {}: head preceded by {:?}",
                    i, other
                ),
            }
        }
    }

    #[test]
    fn test_trim_strategy_none_no_trimming() {
        let mgr = ContextWindowManager::with_approx_counter(10, 5, TrimStrategy::None);
        let msgs = vec![system("sys"), user("hello world this is long")];
        let (trimmed, dropped) = mgr.trim(&msgs);
        assert_eq!(dropped, 0);
        assert_eq!(trimmed.len(), msgs.len());
    }

    #[test]
    fn test_trim_extreme_system_fills_budget() {
        // system 消息本身超 budget,所有 non-system 被丢
        let mgr = ContextWindowManager::with_approx_counter(20, 5, TrimStrategy::default());
        // budget = 15
        // system "this is a very long system prompt" ≈ 4 + 9 = 13 tokens
        // 已用 13,剩 2 给 non-system(不够任何一条)
        let msgs = vec![
            system("this is a very long system prompt"),
            user("hello"),
            user("world"),
        ];
        let (trimmed, dropped) = mgr.trim(&msgs);
        assert_eq!(dropped, 2);
        // system 保留 + hint 插入
        assert!(matches!(trimmed[0], Message::System { .. }));
        // 后面应有 hint 提示
        assert!(trimmed.iter().any(|m| m.content().contains("[earlier")));
    }

    #[test]
    fn test_trim_preserves_order_of_kept_messages() {
        let mgr = ContextWindowManager::with_approx_counter(80, 20, TrimStrategy::default());
        // budget = 60
        let msgs = vec![
            system("sys"),
            user("msg1"),
            user("msg2 with more text to inflate tokens"),
            user("msg3"),
            user("msg4 with more text to inflate tokens"),
            user("msg5"),
        ];
        let (trimmed, dropped) = mgr.trim(&msgs);
        // 验证:kept 部分的相对顺序与原数组一致
        let _ = dropped;
        let mut prev_idx = usize::MAX;
        // 找出 trimmed 中各消息在原数组的位置
        for m in &trimmed {
            if let Message::User { content, .. } = m {
                if let Some(idx) = msgs.iter().position(|orig| {
                    if let Message::User { content: oc, .. } = orig {
                        oc == content
                    } else {
                        false
                    }
                }) {
                    if prev_idx != usize::MAX {
                        assert!(
                            idx > prev_idx,
                            "kept messages out of order: {} after {}",
                            idx,
                            prev_idx
                        );
                    }
                    prev_idx = idx;
                }
            }
        }
    }

    #[test]
    fn test_budget_calculation() {
        let mgr = ContextWindowManager::with_approx_counter(8192, 2048, TrimStrategy::default());
        assert_eq!(mgr.budget(), 6144);
    }

    #[test]
    fn test_count_method() {
        let mgr = ContextWindowManager::with_approx_counter(8192, 2048, TrimStrategy::default());
        let msgs = vec![system("sys"), user("hello")];
        let count = mgr.count(&msgs);
        assert!(count > 0);
    }

    #[test]
    fn test_clone_preserves_config() {
        let mgr = ContextWindowManager::with_approx_counter(4096, 1024, TrimStrategy::None);
        let cloned = mgr.clone();
        assert_eq!(cloned.budget(), 3072);
        assert_eq!(cloned.strategy, TrimStrategy::None);
    }

    #[test]
    fn test_is_cjk_basic() {
        // 汉字
        assert!(is_cjk('你'));
        assert!(is_cjk('好'));
        assert!(is_cjk('世'));
        assert!(is_cjk('界'));
        // 平假名
        assert!(is_cjk('あ'));
        // 片假名
        assert!(is_cjk('ア'));
        // 韩文
        assert!(is_cjk('한'));
        // 全角标点
        assert!(is_cjk('，'));
        assert!(is_cjk('。'));
        // ASCII
        assert!(!is_cjk('a'));
        assert!(!is_cjk('Z'));
        assert!(!is_cjk(' '));
        assert!(!is_cjk('1'));
    }

    /// 集成测试:100 条消息,context_window=8192,验证 trim 后不超 budget
    #[test]
    fn test_integration_large_history_fits_budget() {
        // 用更小的 budget 触发裁剪(每条消息约 30 token,100 条 = 3000 token)
        let mgr = ContextWindowManager::with_approx_counter(1024, 256, TrimStrategy::default());
        // budget = 768
        let mut msgs = vec![system("You are a helpful assistant.")];
        // 每条 user 消息约 30 token,100 条 = 3000+ token,远超 768
        for i in 0..100 {
            msgs.push(user(&format!(
                "message number {:03} with some padding text to inflate the token count a bit more",
                i
            )));
        }
        let (trimmed, dropped) = mgr.trim(&msgs);
        assert!(dropped > 0, "should drop some messages");
        let final_count = mgr.count(&trimmed);
        assert!(
            final_count <= 768 + 50,
            "trimmed messages {} tokens exceed budget 768",
            final_count
        );
        // system 保留
        assert!(matches!(trimmed[0], Message::System { .. }));
        // 最后一条是最近的 message number 099
        let last = trimmed.last().unwrap();
        assert!(last.content().contains("message number 099"));
    }

    // ========== 陈旧工具结果发送视图降级(纯变换) ==========

    use crate::agent::journal::evorule_digest;

    /// 构造降级场景的最小历史:user → tool(原文 content)→ 其后 n 条 Assistant
    fn history_with_tool_after_n_assistants(
        content: &str,
        assistants_after: usize,
    ) -> Vec<Message> {
        let mut msgs = vec![user("q")];
        msgs.push(tool_result(content, "grep_files"));
        for i in 0..assistants_after {
            msgs.push(Message::Assistant {
                content: format!("a{i}"),
                tool_calls: None,
            });
        }
        msgs
    }

    /// 黄金向量确定性:同输入两次跑,输出视图与 offload 清单逐字节相等
    #[test]
    fn test_demote_stale_deterministic_golden() {
        let long = "L".repeat(300);
        let msgs = vec![
            user("q"),
            tool_result(&long, "grep_files"),
            Message::Assistant {
                content: "a".to_string(),
                tool_calls: None,
            },
            user("q2"),
            Message::Assistant {
                content: "b".to_string(),
                tool_calls: None,
            },
        ];
        let (v1, o1) = demote_stale_tool_results(&msgs, 2, 100);
        let (v2, o2) = demote_stale_tool_results(&msgs, 2, 100);
        assert_eq!(
            serde_json::to_string(&v1).unwrap(),
            serde_json::to_string(&v2).unwrap(),
            "视图两次输出逐字节相等"
        );
        assert_eq!(o1, o2, "offload 清单两次输出相等");
        // 黄金形态锚:占位块全文确定性(字段序固定)
        let digest = evorule_digest(&long);
        let hex = digest.rsplit(':').next().unwrap();
        let expected = format!(
            "[STALE_TOOL_RESULT]\nref_id: stale-{hex}\ntool: grep_files\nbytes: {}\ndigest: {digest}\n(陈旧工具结果已降级。需要原文时调用 tool_result_fetch 工具,按 ref_id 分页读取。)",
            long.len()
        );
        assert_eq!(v1[1].content(), expected);
        assert_eq!(o1.len(), 1);
        assert_eq!(o1[0].ref_id, format!("stale-{hex}"));
        assert_eq!(o1[0].digest, digest);
        assert_eq!(o1[0].tool_name, "grep_files");
        assert_eq!(o1[0].content, long);
    }

    /// 条件②轮距边界:K-1 轮不降级,K 轮降级(显式小值 K=3)
    #[test]
    fn test_demote_turn_boundary() {
        let content = "X".repeat(500);
        // 其后 2 条 Assistant(< K=3)→ 不降级,原文保留
        let msgs = history_with_tool_after_n_assistants(&content, 2);
        let (view, offs) = demote_stale_tool_results(&msgs, 3, 100);
        assert!(offs.is_empty());
        assert_eq!(view[1].content(), &content);
        // 其后 3 条 Assistant(= K)→ 降级
        let msgs = history_with_tool_after_n_assistants(&content, 3);
        let (view, offs) = demote_stale_tool_results(&msgs, 3, 100);
        assert_eq!(offs.len(), 1);
        assert!(view[1].content().starts_with("[STALE_TOOL_RESULT]"));
    }

    /// 条件③字节阈值边界:T 字节不降级(条件为严格大于),T+1 降级(显式小值 T=100)
    #[test]
    fn test_demote_bytes_threshold_boundary() {
        // 恰 100 字节 = T → 不降级
        let msgs = history_with_tool_after_n_assistants(&"Y".repeat(100), 2);
        let (view, offs) = demote_stale_tool_results(&msgs, 2, 100);
        assert!(offs.is_empty());
        assert_eq!(view[1].content().len(), 100);
        // 101 字节 = T+1 → 降级
        let msgs = history_with_tool_after_n_assistants(&"Y".repeat(101), 2);
        let (view, offs) = demote_stale_tool_results(&msgs, 2, 100);
        assert_eq!(offs.len(), 1);
        assert!(view[1].content().starts_with("[STALE_TOOL_RESULT]"));
    }

    /// 条件①已消费边界:其后无 Assistant 消息 = 尚未消费,永不降级
    /// (病态 min_turns=0 输入同样被本条件拦住)
    #[test]
    fn test_demote_requires_consumed_assistant_after() {
        let content = "Z".repeat(500);
        let msgs = vec![user("q"), tool_result(&content, "t")];
        let (view, offs) = demote_stale_tool_results(&msgs, 1, 100);
        assert!(offs.is_empty());
        assert_eq!(view[1].content(), &content);
        let (view, offs) = demote_stale_tool_results(&msgs, 0, 100);
        assert!(offs.is_empty());
        assert_eq!(view[1].content(), &content);
    }

    /// 已引用化内容跳过:不二次降级,原样保留
    /// (G12(B-316)改指向真块形态:引用块须带 ref_id/digest/path 结构,
    /// 防伪校验不误伤真块;原 fixture 缺 digest/path 行,按新判据=同形
    /// 伪装应被降级,见 test_g12_fake_marker_tool_result_still_demoted)
    #[test]
    fn test_demote_skips_existing_reference_block() {
        let ref_block = format!(
            "[TOOL_RESULT_REF]\nref_id: call-1-2\ntool: bash\nbytes: 500\nchars: 500\ndigest: sha3:abc123\npath: tool_results/call-1-2.txt\nhead:\n{}",
            "x".repeat(500)
        );
        let msgs = history_with_tool_after_n_assistants(&ref_block, 2);
        let (view, offs) = demote_stale_tool_results(&msgs, 2, 100);
        assert!(offs.is_empty());
        assert_eq!(view[1].content(), &ref_block, "引用块原样保留");
    }

    /// Tool 消息旁路 images 原样保留;offload 只携带原文不带图
    #[test]
    fn test_demote_preserves_tool_images() {
        let img = ImageData {
            mime_type: "image/png".to_string(),
            data_base64: "QUJD".to_string(),
        };
        let long = "W".repeat(500);
        let mut msgs = vec![user("q")];
        msgs.push(Message::Tool {
            content: long.clone(),
            tool_name: "screenshot".to_string(),
            images: vec![img],
        });
        msgs.push(Message::Assistant {
            content: "a".to_string(),
            tool_calls: None,
        });
        msgs.push(Message::Assistant {
            content: "b".to_string(),
            tool_calls: None,
        });
        let (view, offs) = demote_stale_tool_results(&msgs, 2, 100);
        assert_eq!(offs.len(), 1);
        match &view[1] {
            Message::Tool { images, .. } => assert_eq!(images.len(), 1, "旁路图原样保留"),
            other => panic!("expected Tool, got {other:?}"),
        }
        assert_eq!(offs[0].content, long, "offload 携带原文全文");
    }

    /// 占位块含 ref_id/digest/bytes/tool/取回指引;原子组兼容
    /// (配对 Assistant 侧消息零改动;权威历史入参零改动;消息条数不变)
    #[test]
    fn test_demote_placeholder_shape_and_atomic_group_compatibility() {
        let content = "P".repeat(500);
        let mut msgs = vec![user("q")];
        msgs.push(Message::Assistant {
            content: "thinking".to_string(),
            tool_calls: Some(vec![ToolCall {
                name: "grep_files".to_string(),
                arguments: json!({"q": "test"}),
                id: None,
            }]),
        });
        msgs.push(tool_result(&content, "grep_files"));
        msgs.push(Message::Assistant {
            content: "after".to_string(),
            tool_calls: None,
        });
        msgs.push(Message::Assistant {
            content: "after2".to_string(),
            tool_calls: None,
        });
        let snapshot = serde_json::to_string(&msgs).unwrap();
        let (view, offs) = demote_stale_tool_results(&msgs, 2, 100);
        assert_eq!(offs.len(), 1);
        let digest = evorule_digest(&content);
        let hex = digest.rsplit(':').next().unwrap();
        let ph = view[2].content();
        assert!(ph.starts_with("[STALE_TOOL_RESULT]"));
        assert!(ph.contains(&format!("ref_id: stale-{hex}")));
        assert!(ph.contains(&format!("digest: {digest}")));
        assert!(ph.contains(&format!("bytes: {}", content.len())));
        assert!(ph.contains("tool: grep_files"));
        assert!(ph.contains("tool_result_fetch"), "取回指引在场");
        // 原子组兼容:Assistant 侧消息零改动(逐字段比对)
        match &view[1] {
            Message::Assistant {
                content,
                tool_calls,
            } => {
                assert_eq!(content, "thinking");
                assert_eq!(tool_calls.as_ref().map(|c| c.len()).unwrap_or(0), 1);
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
        assert!(matches!(
            &view[3],
            Message::Assistant { content, .. } if content == "after"
        ));
        assert!(matches!(
            &view[4],
            Message::Assistant { content, .. } if content == "after2"
        ));
        // 权威历史入参零改动;消息条数不变(下标对齐保持)
        assert_eq!(serde_json::to_string(&msgs).unwrap(), snapshot);
        assert_eq!(view.len(), msgs.len());
    }

    // ===== 批 B-268 γ-3:机制消息独立原子组 + β 同构降级变换 =====

    /// 机制消息降级目标 = 首条(头部原 system)与最后一条(常驻最新机制消息)
    /// 之外的 System 消息:中间旧机制消息满足三条件降级,两端零改动
    #[test]
    fn test_demote_mechanism_targets_inner_systems_only() {
        let old_mech = "O".repeat(500);
        let msgs = vec![
            system("base system prompt"),
            system(&old_mech),
            system("latest mechanism"),
            user("q"),
            Message::Assistant {
                content: "a1".to_string(),
                tool_calls: None,
            },
            Message::Assistant {
                content: "a2".to_string(),
                tool_calls: None,
            },
        ];
        let (view, offs) = demote_stale_mechanism_messages(&msgs, 2, 100);
        assert_eq!(offs.len(), 1, "仅中间旧机制消息降级: {offs:?}");
        assert_eq!(offs[0].tool_name, "mechanism");
        assert_eq!(offs[0].content, old_mech, "offload 携带原文全文");
        assert_eq!(
            view[0].content(),
            "base system prompt",
            "头部原 system 零改动"
        );
        assert_eq!(
            view[2].content(),
            "latest mechanism",
            "常驻最新机制消息零改动"
        );
        assert!(view[1].content().starts_with("[STALE_MECHANISM_MESSAGE]"));
    }

    /// 无中间旧机制消息 = 零降级:仅 [base] 或 [base, latest] 形态原样返回
    #[test]
    fn test_demote_mechanism_no_inner_systems_is_noop() {
        let msgs = vec![system("base"), system("latest"), user("q")];
        let (view, offs) = demote_stale_mechanism_messages(&msgs, 1, 10);
        assert!(offs.is_empty());
        assert_eq!(view, msgs);
        let msgs = vec![system("base only"), user("q")];
        let (view, offs) = demote_stale_mechanism_messages(&msgs, 1, 10);
        assert!(offs.is_empty());
        assert_eq!(view, msgs);
    }

    /// β 同构三条件:①已消费+②距当前 ≥ min_turns 个 assistant 轮+③字节
    /// 严格大于 min_bytes——任一不满足不降级
    #[test]
    fn test_demote_mechanism_conditions_turns_and_bytes() {
        let content = "C".repeat(500);
        // 轮数边界:其后 1 个 assistant,min_turns=2 → 不降级;min_turns=1 → 降级
        let msgs = vec![
            system("base"),
            system(&content),
            system("latest"),
            user("q"),
            Message::Assistant {
                content: "a".to_string(),
                tool_calls: None,
            },
        ];
        let (view, offs) = demote_stale_mechanism_messages(&msgs, 2, 100);
        assert!(offs.is_empty(), "不足 min_turns 不降级");
        assert_eq!(view[1].content(), &content);
        let (view, offs) = demote_stale_mechanism_messages(&msgs, 1, 100);
        assert_eq!(offs.len(), 1);
        assert!(view[1].content().starts_with("[STALE_MECHANISM_MESSAGE]"));
        // 字节边界:T=500 不降级(严格大于),501 降级
        let msgs = vec![
            system("base"),
            system(&"D".repeat(500)),
            system("latest"),
            user("q"),
            Message::Assistant {
                content: "a".to_string(),
                tool_calls: None,
            },
            Message::Assistant {
                content: "a".to_string(),
                tool_calls: None,
            },
        ];
        let (_, offs) = demote_stale_mechanism_messages(&msgs, 1, 500);
        assert!(offs.is_empty(), "恰 T 字节不降级");
        let msgs = vec![
            system("base"),
            system(&"D".repeat(501)),
            system("latest"),
            user("q"),
            Message::Assistant {
                content: "a".to_string(),
                tool_calls: None,
            },
            Message::Assistant {
                content: "a".to_string(),
                tool_calls: None,
            },
        ];
        let (view, offs) = demote_stale_mechanism_messages(&msgs, 1, 500);
        assert_eq!(offs.len(), 1);
        assert!(view[1].content().starts_with("[STALE_MECHANISM_MESSAGE]"));
    }

    /// 占位块形态:ref_id = mech-{digest hex 段};含 digest/bytes/取回指引;
    /// 条数不变;入参零改动;已降级内容跳过不二次降级
    #[test]
    fn test_demote_mechanism_placeholder_shape_and_idempotent() {
        let content = "P".repeat(500);
        let snapshot = content.clone();
        let msgs = vec![
            system("base"),
            system(&content),
            system("latest"),
            user("q"),
            Message::Assistant {
                content: "a".to_string(),
                tool_calls: None,
            },
        ];
        let (view, offs) = demote_stale_mechanism_messages(&msgs, 1, 100);
        assert_eq!(offs.len(), 1);
        let digest = evorule_digest(&content);
        let hex = digest.rsplit(':').next().unwrap();
        let ph = view[1].content();
        assert!(ph.starts_with("[STALE_MECHANISM_MESSAGE]"));
        assert!(ph.contains(&format!("ref_id: mech-{}", sanitize_ref_segment(hex))));
        assert!(ph.contains(&format!("digest: {digest}")));
        assert!(ph.contains(&format!("bytes: {}", content.len())));
        assert!(ph.contains("tool_result_fetch"), "取回指引在场");
        assert_eq!(offs[0].content, snapshot);
        assert_eq!(view.len(), msgs.len(), "条数不变");
        assert_eq!(view[0].content(), "base");
        assert_eq!(view[2].content(), "latest");
        // 幂等:已降级占位块再过一遍零改动
        let (view2, offs2) = demote_stale_mechanism_messages(&view, 1, 100);
        assert!(offs2.is_empty(), "已引用化内容不二次降级");
        assert_eq!(view2[1].content(), view[1].content());
    }

    /// trim 机制消息独立原子组:预算紧张时头部原 system + 最新机制消息
    /// 常驻,旧机制消息与历史消息同入可淘汰面被丢弃
    #[test]
    fn test_trim_mechanism_group_only_latest_kept_under_tight_budget() {
        let mgr =
            ContextWindowManager::with_approx_counter(60, 10, TrimStrategy::KeepSystemKeepLast);
        let msgs = vec![
            system("sys base prompt"),
            system(&format!("old mechanism payload {}", "X".repeat(200))),
            system("mech latest"),
            user("hello"),
            user("hello"),
            user("hello"),
        ];
        let (trimmed, dropped) = mgr.trim(&msgs);
        assert!(dropped > 0);
        assert_eq!(
            trimmed[0].content(),
            "sys base prompt",
            "头部原 system 常驻"
        );
        assert_eq!(trimmed[1].content(), "mech latest", "最新机制消息常驻");
        assert!(
            !trimmed
                .iter()
                .any(|m| m.content().contains("old mechanism payload")),
            "旧机制消息紧张预算下被丢弃"
        );
        assert!(matches!(trimmed.last(), Some(Message::User { .. })));
    }

    /// 单条机制消息 = 现行为等价:与头部 system 一同常驻(独立原子组仅一条
    /// 时成员=该条,trim 行为与 γ-3 前逐字节一致)
    #[test]
    fn test_trim_single_mechanism_message_kept_as_before() {
        let mgr =
            ContextWindowManager::with_approx_counter(50, 10, TrimStrategy::KeepSystemKeepLast);
        let msgs = vec![
            system("sys prompt long enough"),
            system("mech latest"),
            user("hello"),
            user("hello"),
            user("hello"),
            user("hello"),
            user("hello"),
            user("hello"),
        ];
        let (trimmed, dropped) = mgr.trim(&msgs);
        assert!(dropped > 0);
        assert!(matches!(trimmed[0], Message::System { .. }));
        assert_eq!(
            trimmed[1].content(),
            "mech latest",
            "单条机制消息=最新一条,常驻"
        );
        let has_hint = trimmed
            .iter()
            .any(|m| m.content().contains("[earlier") && m.content().contains("messages trimmed"));
        assert!(has_hint, "should contain trim hint");
    }

    /// 预算充足:旧机制消息经可淘汰面照常保留,零丢弃
    #[test]
    fn test_trim_mechanism_pool_kept_when_budget_allows() {
        let mgr =
            ContextWindowManager::with_approx_counter(8192, 2048, TrimStrategy::KeepSystemKeepLast);
        let msgs = vec![
            system("sys base prompt"),
            system("old mechanism payload"),
            system("mech latest"),
            user("q1"),
            user("q2"),
        ];
        let (trimmed, dropped) = mgr.trim(&msgs);
        assert_eq!(dropped, 0);
        assert_eq!(trimmed.len(), 5, "全部保留");
        assert_eq!(
            trimmed[1].content(),
            "old mechanism payload",
            "旧机制消息保留"
        );
        assert_eq!(trimmed[2].content(), "mech latest");
    }

    // ========== B-285 四轮审计缝缺陷红测(G13/G16/G17) ==========

    /// G13: 摘要回填前的预算收顶——回填超预算时必须收缩;本就够小原样
    /// 放行;连最小摘要都容不下时回落 fallback(即不回填)。
    #[test]
    fn test_cap_summary_shrinks_to_budget() {
        let mgr =
            ContextWindowManager::with_approx_counter(100, 0, TrimStrategy::KeepSystemKeepLast);
        let base_view = vec![system("sys"), user("recent question")];
        let huge = "S".repeat(4000); // ≈1000 tokens,远超剩余空间
        let capped = mgr.cap_summary_for_budget(&huge, "[fallback]", &base_view);
        let mut probe = base_view.clone();
        probe.push(system(&capped));
        let used = ApproxTokenCounter::new().count_messages(&probe);
        assert!(used <= 100, "回填后必须仍在预算内: used={used}");
        assert!(capped.chars().count() < huge.chars().count(), "必须收顶");
        // 本就够小 → 原样放行
        let small = "SUMMARY text";
        assert_eq!(
            mgr.cap_summary_for_budget(small, "[fallback]", &base_view),
            small
        );
        // 极小预算:任何摘要都容不下 → fallback
        let tiny =
            ContextWindowManager::with_approx_counter(20, 0, TrimStrategy::KeepSystemKeepLast);
        assert_eq!(
            tiny.cap_summary_for_budget(&huge, "[fallback]", &base_view),
            "[fallback]"
        );
    }

    /// G16: 常驻面(base+latest_mech)占满预算的极档,不得把末条 User 一起
    /// 丢——latest_mech 让位,保底 base_system+末条 User+hint。
    #[test]
    fn test_trim_extreme_keeps_last_user() {
        let mgr =
            ContextWindowManager::with_approx_counter(60, 0, TrimStrategy::KeepSystemKeepLast);
        // budget=60;base "BASE"×20=80 ASCII→27 token;mech 同 27;
        // always=54,available=60-54-15-5→0 → 极档分支
        let msgs = vec![
            system(&"BASE".repeat(20)),
            system(&"MECH".repeat(20)),
            user("old task"),
            user("final task"),
        ];
        let (trimmed, _dropped) = mgr.trim(&msgs);
        assert!(
            trimmed
                .iter()
                .any(|m| matches!(m, Message::User { content, .. } if content == "final task")),
            "极档必须保底末条 User: {trimmed:?}"
        );
        assert!(
            !trimmed
                .iter()
                .any(|m| matches!(m, Message::System { content } if content.starts_with("MECH"))),
            "极档让位的 latest_mech 不得常驻"
        );
    }

    /// G17: Tool 段前邻不是 Assistant(tool_calls) → 孤儿段不得入发送视图
    /// (发出即 wire 孤儿,上游 400;原子组纪律须覆盖非紧邻形态)。
    #[test]
    fn test_trim_never_sends_orphan_tool_segment() {
        let mgr =
            ContextWindowManager::with_approx_counter(40, 0, TrimStrategy::KeepSystemKeepLast);
        // budget=40;sys 8;available=40-8-15-5=12;
        // 尾组孤儿 tool≈19 token>12,现行为 kept 空时照加 → 修复后整组放弃
        let orphan = "orphan result without any assistant call. ".repeat(6);
        let msgs = vec![
            system("sys"),
            user("q1"),
            user("q2"),
            tool_result(&orphan, "ghost"),
        ];
        let (trimmed, _dropped) = mgr.trim(&msgs);
        assert!(
            !trimmed.iter().any(|m| matches!(m, Message::Tool { .. })),
            "孤儿 Tool 段不得入发送视图: {trimmed:?}"
        );
    }
}
