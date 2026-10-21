// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! ask_user 人在环澄清卡（批K 段7）——agent 向用户发结构化问题并阻塞等回答。
//!
//! man-in-loop 产品线的原生契合件（批J 设计稿 §3.7）：LLM 在关键分歧点
//! 主动澄清（question + options[2-4] 可多选），用户点选/作答后 agent 续跑；
//! 超时（缺省 300s）fail-visible 返回 timeout 形态，agent 自行决策续跑。
//!
//! ## 两阶段协议（对齐 candidate 工具两次调用协议先例）
//!
//! 1. **首调**（无 `answer` 参数）：参数校验 fail-visible → 生成 question_id
//!    → 返回 `{"status":"ask_user_pending", question_id, question, options,
//!    multi_select, timeout_secs}`（本工具瞬时返回，不阻塞）；
//! 2. **runner 等待**：识别 pending 形态 → yield `AgentEvent::AskUser` 推帧
//!    → 经 [`AskUserCallback`] 阻塞等回答（等待在 runner 层不占轮次——
//!    与 ApprovalPending 同构）；
//! 3. **重调**（带 `answer` 参数）：runner 拿到回答后带 answer 重调本工具
//!    → 校验答案合法性（单选∈options / 多选⊆options / 自由文本非空）→
//!    返回 `{"status":"ok", answer, answered_by}` 回喂 LLM；
//! 4. **超时/通道关闭**：runner 侧超时 → `{"status":"timeout", ...}` 回喂
//!    （fail-visible），前端卡置灰。
//!
//! ## 不复用审批通道的裁定
//!
//! `ApprovalRequest` 是纯审批语义（command/risk/alternative）+ 60s 审批窗
//! （HttpApproval 级字段）——问答硬塞会污染 approved bool / reason 语义且
//! 超时窗不匹配。故形态同构（pending map + oneshot + 超时自动收场，先例
//! approval.rs）而协议独立（新帧型 ask_user / ask_user_result + 新端点
//! POST /agents/{t}/answer）。
//!
//! ## 治理面
//!
//! Standard 免裁决 + AutoPolicy（交互零风险，无文件/网络/子进程触点）；
//! 不进 EXFIL_SCAN_TOOLS（question 发给会话数据有权方=用户本人，非网络
//! 外发通道）；开关 agentTools.askUser 默认开（关=callback 不装配，resolve
//! 侧立即返回无人态 fail-visible，不干等超时）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::io_handlers::tool_handler::ToolFunction;

/// 问题文本字符上限
pub const ASK_USER_MAX_QUESTION_CHARS: usize = 2000;
/// 选项数上限（2-4 项，对标主流 AskUserQuestion 先例）
pub const ASK_USER_MAX_OPTIONS: usize = 4;
/// 选项数下限
pub const ASK_USER_MIN_OPTIONS: usize = 2;
/// 单个选项字符上限
pub const ASK_USER_MAX_OPTION_CHARS: usize = 200;
/// 回答项数上限（多选时 ≤ options 数）
pub const ASK_USER_MAX_ANSWERS: usize = ASK_USER_MAX_OPTIONS;
/// 超时下限（秒）
pub const ASK_USER_MIN_TIMEOUT_SECS: u64 = 60;
/// 超时上限（秒；防 LLM 传大值挂死会话）
pub const ASK_USER_MAX_TIMEOUT_SECS: u64 = 600;
/// 超时缺省（秒）
pub const ASK_USER_DEFAULT_TIMEOUT_SECS: u64 = 300;

/// 工具名
pub const ASK_USER_TOOL: &str = "ask_user";

/// 生成新的问题 ID（`au-` 前缀 + 纳秒时间戳十六进制，对齐 ap- 先例）
pub fn new_question_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("au-{:x}", nanos)
}

/// 首调参数校验 + 规范化（纯函数；fail-visible 一次列明全部问题后拒问）
///
/// 规则：question 必填非空 ≤2000 字符；options 可选（给了须 2-4 项各非空
/// ≤200 字符）；multi_select 可选布尔（仅 options 存在时有意义）；timeout_secs
/// 可选 60-600（越帽 clamp 到边界，缺省 300）。
pub fn validate_ask_user_args(args: &Value) -> Result<AskUserQuery, String> {
    let mut problems: Vec<String> = Vec::new();

    let question = args
        .get("question")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let question = match question {
        Some(q) if q.chars().count() > ASK_USER_MAX_QUESTION_CHARS => {
            problems.push(format!(
                "'question' exceeds {} chars (got {})",
                ASK_USER_MAX_QUESTION_CHARS,
                q.chars().count()
            ));
            None
        }
        Some(q) => Some(q.to_string()),
        None => {
            if args.get("question").is_some() {
                problems.push("'question' must be a non-empty string".to_string());
            } else {
                problems.push("missing required parameter 'question'".to_string());
            }
            None
        }
    };

    let options: Option<Vec<String>> = match args.get("options") {
        None | Some(Value::Null) => None,
        Some(Value::Array(arr)) => {
            let opts: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::trim))
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if opts.len() != arr.len() {
                problems.push("'options' entries must be non-empty strings".to_string());
                None
            } else if !(ASK_USER_MIN_OPTIONS..=ASK_USER_MAX_OPTIONS).contains(&opts.len()) {
                problems.push(format!(
                    "'options' must have {}-{} entries (got {})",
                    ASK_USER_MIN_OPTIONS,
                    ASK_USER_MAX_OPTIONS,
                    opts.len()
                ));
                None
            } else if let Some(bad) = opts
                .iter()
                .find(|o| o.chars().count() > ASK_USER_MAX_OPTION_CHARS)
            {
                problems.push(format!(
                    "'options' entry exceeds {} chars: {}...",
                    ASK_USER_MAX_OPTION_CHARS,
                    bad.chars().take(20).collect::<String>()
                ));
                None
            } else {
                Some(opts)
            }
        }
        Some(_) => {
            problems.push("'options' must be an array of strings".to_string());
            None
        }
    };

    let multi_select = match args.get("multi_select") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            problems.push("'multi_select' must be a boolean".to_string());
            false
        }
    };

    let timeout_secs = match args.get("timeout_secs") {
        None | Some(Value::Null) => ASK_USER_DEFAULT_TIMEOUT_SECS,
        Some(Value::Number(n)) => {
            let v = n.as_u64().unwrap_or(0);
            v.clamp(ASK_USER_MIN_TIMEOUT_SECS, ASK_USER_MAX_TIMEOUT_SECS)
        }
        Some(_) => {
            problems.push("'timeout_secs' must be a number (seconds)".to_string());
            ASK_USER_DEFAULT_TIMEOUT_SECS
        }
    };

    if problems.is_empty() {
        Ok(AskUserQuery {
            session_id: String::new(),
            question_id: new_question_id(),
            question: question.unwrap_or_default(),
            options: options.unwrap_or_default(),
            multi_select,
            timeout_secs,
        })
    } else {
        Err(problems.join("; "))
    }
}

/// runner → 决策端的问答请求（对齐 ApprovalRequest 在审批链中的位置）
#[derive(Debug, Clone, PartialEq)]
pub struct AskUserQuery {
    /// 会话 ID（HTTP 模式下关联 /answer 端点）
    pub session_id: String,
    /// 问题 ID（`au-` 前缀；回传校验主键，防串话对齐 proposal_id）
    pub question_id: String,
    /// 问题文本
    pub question: String,
    /// 选项（空 = 自由文本问答）
    pub options: Vec<String>,
    /// 是否多选（仅 options 非空时有意义）
    pub multi_select: bool,
    /// 等待超时（秒；runner 侧执行）
    pub timeout_secs: u64,
}

/// 决策端 → runner 的回答（对齐 ApprovalDecision）
#[derive(Debug, Clone, PartialEq)]
pub struct AskUserAnswer {
    /// 回答内容（单选=1 项且 ∈options；多选=1..=options 项 ⊆options；
    /// 自由文本=1 项；timed_out=true 时为空）
    pub answer: Vec<String>,
    /// 回答者标识（用户名 / unverified / cli-user / auto）
    pub answered_by: String,
    /// 身份是否经平台认证端验证
    pub verified: bool,
    /// 是否无人回答（超时 / 通道关闭 / 无人值守）——runner 据此走
    /// timeout 形态回喂
    pub timed_out: bool,
}

/// HTTP 问答等待项（对齐 PendingApproval）
#[derive(Debug)]
pub struct PendingQuestion {
    /// 回答回传通道
    pub tx: oneshot::Sender<AskUserAnswer>,
    /// 等待中的问题 ID（/answer 端点校验防串话）
    pub question_id: String,
}

/// HTTP 问答等待注册表（serve 装配创建，/answer 端点与 HttpAskUser 共享；
/// 形态对齐 ApprovalStore）
pub type QuestionStore = Arc<Mutex<HashMap<String, PendingQuestion>>>;

/// 问答回调 trait —— 与 ApprovalCallback 平行（不同运行模式不同实现）
///
/// runner 在识别 ask_user_pending 后调用 `request_answer()`，该方法阻塞
/// 直到用户回答或超时；无人值守/callback 缺位由调用侧短路（不干等）。
#[async_trait]
#[allow(clippy::double_must_use)] // async_trait 宏生成的 must_use 与 Boxed Future 类型语义重复,源上无法消（同 approval.rs ApprovalCallback 先例）
pub trait AskUserCallback: Send + Sync {
    /// 问用户。返回 [`AskUserAnswer`]（timed_out=true = 无人回答形态）
    async fn request_answer(&self, query: &AskUserQuery) -> AskUserAnswer;
}

/// HTTP/SSE 模式：通过 oneshot channel 等 POST /agents/{t}/answer
///
/// - `request_answer()` 创建 oneshot channel，把 sender 存入 `pending` map
/// - HTTP 端点 `POST /agents/{t}/answer` 从 map 取出 sender 并发送回答
///   （question_id 校验防串话，对齐 /approve 的 proposal_id 校验）
/// - 超时（query.timeout_secs，缺省 300s）后返回无人回答形态
pub struct HttpAskUser {
    /// session_id → 问答等待项（与 /answer 端点共享）
    pub pending: QuestionStore,
}

#[async_trait]
impl AskUserCallback for HttpAskUser {
    async fn request_answer(&self, query: &AskUserQuery) -> AskUserAnswer {
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self
                .pending
                .lock()
                .expect("pending_questions mutex poisoned");
            map.insert(
                query.session_id.clone(),
                PendingQuestion {
                    tx,
                    question_id: query.question_id.clone(),
                },
            );
        }

        tracing::info!(
            session = %query.session_id,
            question_id = %query.question_id,
            timeout_secs = query.timeout_secs,
            "ask_user: awaiting HTTP answer"
        );

        match tokio::time::timeout(Duration::from_secs(query.timeout_secs), rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => {
                // sender 被 drop（防御性处理，对齐 approval 通道关闭臂）
                tracing::warn!(
                    session = %query.session_id,
                    "ask_user: answer channel closed unexpectedly"
                );
                AskUserAnswer {
                    answer: vec![],
                    answered_by: "auto".to_string(),
                    verified: true,
                    timed_out: true,
                }
            }
            Err(_) => {
                // 超时：清理等待项，返回无人回答形态
                let mut map = self
                    .pending
                    .lock()
                    .expect("pending_questions mutex poisoned");
                map.remove(&query.session_id);
                tracing::warn!(
                    session = %query.session_id,
                    timeout_secs = query.timeout_secs,
                    "ask_user: timed out, returning no-answer"
                );
                AskUserAnswer {
                    answer: vec![],
                    answered_by: "auto".to_string(),
                    verified: true,
                    timed_out: true,
                }
            }
        }
    }
}

/// CLI 模式：stdin 打印问题+编号选项读行；`auto=true`（无人值守）时**立即
/// 返回无人态**——不模拟回答、不干等超时（对齐 PolicyApproval「零人工决策
/// 事件」哲学；问答不可模拟，fail-visible 让 agent 自行决策续跑）
pub struct CliAskUser {
    /// 无人值守旗标（true=立即无人态）
    pub auto: bool,
}

#[async_trait]
impl AskUserCallback for CliAskUser {
    async fn request_answer(&self, query: &AskUserQuery) -> AskUserAnswer {
        if self.auto {
            return AskUserAnswer {
                answer: vec![],
                answered_by: "auto".to_string(),
                verified: true,
                timed_out: true,
            };
        }

        // spawn_blocking 避免 stdin 读取阻塞 tokio runtime（CliApproval 同款）
        let question = query.question.clone();
        let options = query.options.clone();
        let multi_select = query.multi_select;
        // 提示面用副本进闭包(move),选项文本映射留原值在闭包外用
        let options_for_prompt = options.clone();

        let raw = tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Write};
            eprintln!("\n❓ Agent asks:");
            eprintln!("  {}", question);
            if options_for_prompt.is_empty() {
                eprint!("Your answer: ");
            } else {
                for (i, opt) in options_for_prompt.iter().enumerate() {
                    eprintln!("  {}. {}", i + 1, opt);
                }
                if multi_select {
                    eprint!("Choose (comma-separated numbers): ");
                } else {
                    eprint!("Choose a number: ");
                }
            }
            let _ = std::io::stderr().flush();
            let line = std::io::stdin().lock().lines().next();
            line.map(|l| l.unwrap_or_default()).unwrap_or_default()
        })
        .await
        .unwrap_or_default();

        // 编号选择 → 选项文本；其余按自由文本（合法性校验在工具二阶段）
        let trimmed = raw.trim().to_string();
        let answer: Vec<String> = if !options.is_empty() && !multi_select {
            trimmed
                .parse::<usize>()
                .ok()
                .filter(|&n| (1..=options.len()).contains(&n))
                .map(|n| vec![options[n - 1].clone()])
                .unwrap_or_else(|| vec![trimmed])
        } else if !options.is_empty() && multi_select {
            trimmed
                .split(',')
                .filter_map(|p| p.trim().parse::<usize>().ok())
                .filter(|&n| (1..=options.len()).contains(&n))
                .map(|n| options[n - 1].clone())
                .collect()
        } else {
            vec![trimmed]
        };

        AskUserAnswer {
            answer,
            answered_by: "cli-user".to_string(),
            verified: false,
            timed_out: false,
        }
    }
}

// ---------- 工具执行体（无状态：校验+形态返回；等待在 runner 层） ----------

/// 从工具返回 JSON 解析问答请求（runner 流式路径消费）
///
/// 与审批链 `parse_approval_request` 对齐的解析入口：status=ask_user_pending
/// 时取出两阶段协议形态；question_id 缺失（畸形形态）返回 None 原样回喂。
pub fn parse_ask_user_query(session_id: &str, tool_result_str: &str) -> Option<AskUserQuery> {
    let result_json: Value = serde_json::from_str(tool_result_str).ok()?;
    if result_json.get("status").and_then(|v| v.as_str()) != Some("ask_user_pending") {
        return None;
    }
    Some(AskUserQuery {
        session_id: session_id.to_string(),
        question_id: result_json
            .get("question_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)?,
        question: result_json
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        options: result_json
            .get("options")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        multi_select: result_json
            .get("multi_select")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        timeout_secs: result_json
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(ASK_USER_DEFAULT_TIMEOUT_SECS),
    })
}

/// ask_user：人在环澄清卡
///
/// 首调（无 answer）返回 pending 形态；重调（带 answer，由 runner 注入）
/// 校验答案合法性并回执。工具本身瞬时返回，阻塞等待在 runner 层。
pub struct AskUserTool;

impl AskUserTool {
    /// 注册实例（无占位/重绑形态——工具无状态，等待在 runner）
    pub fn new() -> Self {
        Self
    }
}

impl Default for AskUserTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ToolFunction for AskUserTool {
    async fn call(&self, args: &Value) -> crate::io_handler::IoResult {
        // 二阶段：runner 带回答重调（args.answer 由 runner 注入，对齐
        // approved:true 只能由 runner 注入的决策门纪律）
        if let Some(ans) = args.get("answer") {
            return self.resolve_answer(args, ans);
        }

        // 首调：校验 + pending 形态
        let query = validate_ask_user_args(args)?;
        Ok(json!({
            "status": "ask_user_pending",
            "question_id": query.question_id,
            "question": query.question,
            "options": query.options,
            "multi_select": query.multi_select,
            "timeout_secs": query.timeout_secs,
        }))
    }
}

impl AskUserTool {
    /// 二阶段答案校验（单选∈options / 多选⊆options / 自由文本非空）
    fn resolve_answer(&self, args: &Value, ans: &Value) -> crate::io_handler::IoResult {
        let answers: Vec<String> = match ans {
            Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            Value::String(s) => vec![s.clone()],
            _ => return Err("'answer' must be a string or array of strings".to_string()),
        };
        if answers.is_empty() || answers.iter().any(String::is_empty) {
            return Err("'answer' must be non-empty".to_string());
        }
        if answers.len() > ASK_USER_MAX_ANSWERS {
            return Err(format!(
                "'answer' exceeds max {} entries",
                ASK_USER_MAX_ANSWERS
            ));
        }

        // options 存在时校验答案与选项对齐（runner 从 pending 形态得知 options）
        let options: Vec<String> = args
            .get("options")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let multi_select = args
            .get("multi_select")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !options.is_empty() {
            let unknown: Vec<&String> = answers.iter().filter(|a| !options.contains(a)).collect();
            if !unknown.is_empty() {
                return Err(format!(
                    "'answer' entries not in options: {unknown:?} (options: {options:?})"
                ));
            }
            if !multi_select && answers.len() > 1 {
                return Err("'answer' has multiple entries but multi_select is false".to_string());
            }
        }

        let answered_by = args
            .get("answered_by")
            .and_then(|v| v.as_str())
            .unwrap_or("unverified")
            .to_string();
        Ok(json!({
            "status": "ok",
            "answer": answers,
            "answered_by": answered_by,
        }))
    }
}

// ---------- 测试 ----------

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    // ===== validate_ask_user_args =====

    #[test]
    fn test_validate_minimal_free_text_defaults() {
        let q = validate_ask_user_args(&json!({"question": "用哪个方案?"})).unwrap();
        assert_eq!(q.question, "用哪个方案?");
        assert!(q.options.is_empty());
        assert!(!q.multi_select);
        assert_eq!(q.timeout_secs, ASK_USER_DEFAULT_TIMEOUT_SECS);
        assert!(q.question_id.starts_with("au-"));
    }

    #[test]
    fn test_validate_full_options() {
        let q = validate_ask_user_args(&json!({
            "question": "选方案",
            "options": ["甲", "乙", "丙"],
            "multi_select": true,
            "timeout_secs": 120,
        }))
        .unwrap();
        assert_eq!(q.options, vec!["甲", "乙", "丙"]);
        assert!(q.multi_select);
        assert_eq!(q.timeout_secs, 120);
    }

    #[test]
    fn test_validate_missing_or_empty_question_fails() {
        assert!(validate_ask_user_args(&json!({}))
            .unwrap_err()
            .contains("missing required parameter 'question'"));
        assert!(validate_ask_user_args(&json!({"question": "   "}))
            .unwrap_err()
            .contains("non-empty"));
    }

    #[test]
    fn test_validate_question_over_cap_fails() {
        let long = "字".repeat(ASK_USER_MAX_QUESTION_CHARS + 1);
        let err = validate_ask_user_args(&json!({"question": long})).unwrap_err();
        assert!(err.contains("exceeds 2000 chars"), "got: {err}");
    }

    #[test]
    fn test_validate_options_count_bounds_fail_visible_once() {
        // 1 项（低于下限）与 5 项（高于上限）与含空项各拒
        let one = validate_ask_user_args(&json!({"question": "q", "options": ["甲"]})).unwrap_err();
        assert!(one.contains("2-4 entries"), "got: {one}");
        let five = validate_ask_user_args(&json!({
            "question": "q",
            "options": ["1", "2", "3", "4", "5"]
        }))
        .unwrap_err();
        assert!(five.contains("2-4 entries"), "got: {five}");
        let blank =
            validate_ask_user_args(&json!({"question": "q", "options": ["甲", "  "]})).unwrap_err();
        assert!(blank.contains("non-empty strings"), "got: {blank}");
    }

    #[test]
    fn test_validate_timeout_clamps_to_bounds() {
        let low = validate_ask_user_args(&json!({"question": "q", "timeout_secs": 5})).unwrap();
        assert_eq!(low.timeout_secs, ASK_USER_MIN_TIMEOUT_SECS);
        let high =
            validate_ask_user_args(&json!({"question": "q", "timeout_secs": 99999})).unwrap();
        assert_eq!(high.timeout_secs, ASK_USER_MAX_TIMEOUT_SECS);
    }

    #[test]
    fn test_validate_multi_problems_fail_visible_once() {
        let err = validate_ask_user_args(&json!({"question": "", "options": ["x"]})).unwrap_err();
        assert!(
            err.contains("'question' must be a non-empty string"),
            "got: {err}"
        );
        assert!(err.contains("2-4 entries"), "got: {err}");
    }

    // ===== question_id =====

    #[test]
    fn test_new_question_id_format_and_unique() {
        let a = new_question_id();
        let b = new_question_id();
        assert!(a.starts_with("au-"));
        assert_ne!(a, b, "纳秒精度下连续调用应产生不同 ID");
    }

    // ===== 工具两阶段 =====

    #[tokio::test]
    async fn test_first_call_returns_pending_form() {
        let tool = AskUserTool::new();
        let out = tool
            .call(&json!({"question": "继续吗?", "options": ["是", "否"]}))
            .await
            .unwrap();
        assert_eq!(out["status"], "ask_user_pending");
        assert!(out["question_id"].as_str().unwrap().starts_with("au-"));
        assert_eq!(out["timeout_secs"], ASK_USER_DEFAULT_TIMEOUT_SECS);
    }

    #[tokio::test]
    async fn test_first_call_invalid_args_fail_visible() {
        let tool = AskUserTool::new();
        let err = tool.call(&json!({"options": ["是"]})).await.unwrap_err();
        assert!(err.contains("'question'"), "got: {err}");
    }

    #[tokio::test]
    async fn test_second_call_single_select_validates_membership() {
        let tool = AskUserTool::new();
        let base = json!({
            "question": "q",
            "options": ["甲", "乙"],
            "answer": ["甲"],
            "answered_by": "alice",
        });
        let out = tool.call(&base).await.unwrap();
        assert_eq!(out["status"], "ok");
        assert_eq!(out["answer"][0], "甲");
        assert_eq!(out["answered_by"], "alice");

        // 不在 options 内 → 拒
        let bad = json!({"options": ["甲", "乙"], "answer": ["丙"]});
        let err = tool.call(&bad).await.unwrap_err();
        assert!(err.contains("not in options"), "got: {err}");

        // 单选给了多项 → 拒
        let multi = json!({"options": ["甲", "乙"], "multi_select": false, "answer": ["甲", "乙"]});
        assert!(tool
            .call(&multi)
            .await
            .unwrap_err()
            .contains("multi_select is false"));
    }

    #[tokio::test]
    async fn test_second_call_multi_select_subset_ok() {
        let tool = AskUserTool::new();
        let out = tool
            .call(&json!({
                "options": ["甲", "乙", "丙"],
                "multi_select": true,
                "answer": ["甲", "丙"],
            }))
            .await
            .unwrap();
        assert_eq!(out["status"], "ok");
        assert_eq!(out["answer"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_second_call_free_text_ok_and_empty_rejected() {
        let tool = AskUserTool::new();
        let out = tool.call(&json!({"answer": "就用甲吧"})).await.unwrap();
        assert_eq!(out["status"], "ok");
        assert!(tool.call(&json!({"answer": []})).await.is_err());
        assert!(tool.call(&json!({"answer": ""})).await.is_err());
    }

    // ===== HttpAskUser =====

    fn query(session: &str) -> AskUserQuery {
        AskUserQuery {
            session_id: session.to_string(),
            question_id: new_question_id(),
            question: "q".to_string(),
            options: vec!["甲".to_string(), "乙".to_string()],
            multi_select: false,
            timeout_secs: ASK_USER_DEFAULT_TIMEOUT_SECS,
        }
    }

    #[tokio::test]
    async fn test_http_ask_user_resolves_on_answer() {
        let cb = HttpAskUser {
            pending: Arc::new(Mutex::new(HashMap::new())),
        };
        let pending = cb.pending.clone();
        let q = query("s1");

        let wait = tokio::spawn(async move { cb.request_answer(&q).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let item = {
            let mut map = pending.lock().unwrap();
            map.remove("s1").unwrap()
        };
        assert!(item.question_id.starts_with("au-"));
        item.tx
            .send(AskUserAnswer {
                answer: vec!["甲".to_string()],
                answered_by: "alice".to_string(),
                verified: true,
                timed_out: false,
            })
            .unwrap();

        let ans = wait.await.unwrap();
        assert!(!ans.timed_out);
        assert_eq!(ans.answer, vec!["甲"]);
        assert_eq!(ans.answered_by, "alice");
    }

    #[tokio::test]
    async fn test_http_ask_user_timeout_cleans_pending() {
        let cb = HttpAskUser {
            pending: Arc::new(Mutex::new(HashMap::new())),
        };
        let pending = cb.pending.clone();
        let mut q = query("s2");
        q.timeout_secs = 1; // 1 秒超时

        let ans = cb.request_answer(&q).await;
        assert!(ans.timed_out);
        assert_eq!(ans.answered_by, "auto");
        assert!(
            pending.lock().unwrap().is_empty(),
            "超时后 pending 应被清理"
        );
    }

    #[tokio::test]
    async fn test_http_ask_user_channel_closed_returns_no_answer() {
        let cb = HttpAskUser {
            pending: Arc::new(Mutex::new(HashMap::new())),
        };
        let pending = cb.pending.clone();
        let q = query("s3");

        let wait = tokio::spawn(async move { cb.request_answer(&q).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(pending.lock().unwrap().remove("s3").unwrap()); // drop sender

        let ans = wait.await.unwrap();
        assert!(ans.timed_out);
    }

    // ===== CliAskUser =====

    #[tokio::test]
    async fn test_cli_ask_user_auto_returns_immediately_no_answer() {
        let cb = CliAskUser { auto: true };
        let ans = cb.request_answer(&query("s4")).await;
        assert!(ans.timed_out, "无人值守立即返回无人态，不干等");
        assert_eq!(ans.answered_by, "auto");
    }
}
