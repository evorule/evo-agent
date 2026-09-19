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

use crate::agent::memory::{MemoryManager, MemoryRecord, MemoryScope};
use crate::agent::memory_event::extraction::EventExtractor;
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
}

/// 沉淀结果
#[derive(Debug, Default)]
pub struct SedimentResult {
    /// 摘要是否成功写入共享空间
    pub summary_written: bool,
    /// 成功写入的稳定事实 key 列表
    pub stable_facts: Vec<String>,
    /// 提取并写入共享账本的事件 ID 列表（R07/E17 接线后实际填充）
    pub events: Vec<String>,
    /// rollup 是否执行（C4）
    pub rollup_done: bool,
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
                    Ok(_) => result.summary_written = true,
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
                        Ok(_) => result.stable_facts.push(fact.key.clone()),
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

    result
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
                    Ok(_) => result.events.push(event.event_id.clone()),
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
    use super::*;

    #[test]
    fn test_sediment_config_default() {
        let cfg = SedimentConfig::default();
        assert_eq!(cfg.namespace, "default");
        assert!(cfg.enable_event_extraction);
        assert_eq!(cfg.max_session_summaries, 3);
        assert_eq!(cfg.max_injected_events, 5);
        assert_eq!(cfg.summary_rollup_threshold, 10);
        assert_eq!(cfg.llm_model_id, "unknown");
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
}
