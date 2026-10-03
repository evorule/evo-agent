// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! B21 PR-1:journal 会话事件流——会话唯一真相源(append-only)
//!
//! 文件:`data/sessions/{session_id}.jsonl`,每行
//! `{"seq":u64,"ts":unix_ms,"type":"<event>","payload":{...}}`,seq 从 1 连续递增。
//!
//! 设计裁定(00-立项方案 D1 + ATIF 映射表 §二增补终版):
//! - 写侧:进程内 Mutex 串行化(seq 分配 + 行追加同临界区,原子);append 后
//!   flush 不 fsync(v1 吞吐优先,崩溃容忍最后一条丢失,由尾部 turn_ended
//!   缺失语义兜底);打开失败 fail-soft(降级为无 journal 会话,调用方记
//!   warn,与 metrics/tool_traces 同风格);读侧 seq 连续性校验 fail-visible
//!   (空洞 = 日志损坏,报错不静默)
//! - 标识合成确定性:`tool_call_id = "t{seq}"`、`judgement_id = "j{seq}"`
//!   (provider 无 id;重导出幂等、对账可复算)
//! - digest 用 evorule-hash 口径(`"blake3:"+64hex`,E 族 B 约束,不私写格式)
//! - `session_crashed` 不在运行时写入:进程死亡时什么也写不了;由 PR-2 resume
//!   重放检测「尾部无 turn_ended」后补写 crash 标记 + `session_resumed`
//! - 事件 payload 存摘要/digest(口径 C),完整内容在 evorule 审计链与
//!   transcript payload;导出时经 `evorule_request_id` / `call_id` join
//!   (ATIF 映射表 §一三源 join 模型)
//! - 本地 ReAct 路径工具执行不经 evorule IoRequest,`tool_invoked` 的
//!   `evorule_request_id = None`,全文内容源 = transcript payload;call_service
//!   路径 `evorule_request_id = Some(IoRequest.id)`(审计链 join 键)

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// digest 口径:evorule-hash(`"blake3:"+64hex`;E 族 B 约束,与生态哈希纪律一致)
pub fn evorule_digest(s: &str) -> String {
    format!("blake3:{}", blake3::hash(s.as_bytes()).to_hex())
}

/// token 计数三元组(provider 真值 `LlmResponse.token_usage` 映射;
/// 估算 fallback 存总量,导出期按 7:3 拆分——ATIF 映射表 §四.3)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TokenRecord {
    /// 输入 token(累计发送至模型的输入)
    pub prompt: u64,
    /// 输出 token(模型生成)
    pub completion: u64,
    /// 总量(prompt + completion,provider 口径)
    pub total: u64,
}

/// journal 事件(13 种;schema 终版 = ATIF 映射表 §二增补)
///
/// 序列化形态:`{"type":"<event>","payload":{...}}`(adjacently tagged,
/// 与文件行内 seq/ts 平铺后即全行)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum JournalEvent {
    /// 轮开始(一轮 = 一次用户 goal 驱动的 run_streaming;G15 续跑同文件续轮)
    TurnStarted {
        /// 轮序号(journal 内从 1 递增)
        turn_seq: u64,
        /// 用户 goal(截 512 字符)
        goal: String,
    },
    /// LLM 调用(成功完成时写;purpose=react 参与步映射,sidecar 用途不映射)
    LlmCalled {
        /// 模型名(react_model 或 runner 默认)
        model: String,
        /// 调用用途:react|summarize|session_summary|rollup|compaction
        purpose: String,
        /// evorule IoRequest FactId(call_external;审计链 join 键)
        evorule_request_id: Option<u64>,
        /// provider token 真值(无则 fallback tokens_est)
        tokens: Option<TokenRecord>,
        /// 近似估算总量(count_messages+completion 估算)
        tokens_est: Option<u64>,
        /// 本轮送入 LLM 的消息数
        request: usize,
        /// assistant 全文摘要(截 200 字符)
        response: String,
    },
    /// 工具调用分发(call_id 由本事件 seq 确定性合成)
    ToolInvoked {
        /// 合成调用 id("t{seq}")
        call_id: String,
        /// 工具名
        tool: String,
        /// 参数全文 digest(evorule-hash 口径)
        args_digest: String,
        /// evorule IoRequest FactId(call_service 路径;本地 ReAct=None)
        evorule_request_id: Option<u64>,
    },
    /// 工具结果返回(与 tool_invoked 经 call_id 配对)
    ToolResult {
        /// 配对的调用 id
        call_id: String,
        /// ok|error
        status: String,
        /// 输出全文字节数
        size_bytes: usize,
        /// 输出全文 digest(evorule-hash 口径)
        content_digest: String,
    },
    /// 审批请求开启(60s 窗口 / policy 判定前)
    ApprovalRequested {
        /// 审批提案 id(proposal_id)
        approval_id: String,
        /// 工具名
        tool: String,
        /// 审批载荷摘要(command,截 256 字符)
        payload: String,
    },
    /// 审批决定(approved / rejected / auto_rejected)
    ApprovalResolved {
        /// 审批提案 id
        approval_id: String,
        /// 决定标签
        decision: String,
    },
    /// 治理裁决输出(意图裁决 allowed / blocked)
    PolicyJudged {
        /// 合成判定 id("j{seq}")
        judgement_id: String,
        /// allowed|blocked
        verdict: String,
        /// 判定依据摘要(工具名+scope/intent,截 256 字符)
        evidence: String,
    },
    /// 窗口压力压缩执行(PR-3 接线,事件面先就绪)
    ///
    /// 接线纪律(2026-10-01 立规):压缩动作前被压缩原文必须全文在账
    /// (审计链权威面,压缩只改工作记忆视图);压缩后窗口必须可由「账上
    /// 事件+纯函数 render」确定性重建并留重建演示;压缩器若调 LLM 必走
    /// AuditedLlm sidecar(purpose=compaction);本事件须携带被压缩原文的
    /// 账面锚(call_id/FactId join 键)供 ATIF 导出对账。
    CompactionPerformed {
        /// 压缩前 token 估算
        before_est: usize,
        /// 压缩后 token 估算
        after_est: usize,
        /// 被清除的工具结果 call_id 列表
        cleared_call_ids: Vec<String>,
    },
    /// 会话沉淀结果落账：sediment 四项持久化结果进步级账面。
    /// CacheOnly 事实仅本地 cache、由 B3 对账补偿——落账后「沉淀成功与否」有对账依据，
    /// ATIF 导出 context_management 段随之充实（F-902 同精神）。
    SedimentPerformed {
        /// 摘要是否写入共享空间
        summary_written: bool,
        /// 已落审计链的稳定事实 key 列表
        stable_facts: Vec<String>,
        /// 仅本地 cache 的稳定事实 key 列表
        stable_facts_cache_only: Vec<String>,
        /// 写入共享账本的事件数
        events_count: usize,
        /// rollup 是否执行
        rollup_done: bool,
    },
    /// B-1(收尾清偿批):逐轮 wire 留痕——本轮组装完成的完整上下文 wire 落账。
    /// 每轮全量(不裁剪)双写成本已裁定接受;F-903 重建演示以此为逐字节比对基准。
    WireRendered {
        /// 轮序号(与 turn_started.turn_seq 同源)
        round: u64,
        /// wire 字节长度(UTF-8)
        wire_len: usize,
        /// wire 全文 digest(evorule-hash 口径)
        content_hash: String,
        /// wire 全文(未截断)
        full_text: String,
    },
    /// 轮收尾(优雅终止路径显式写;异常路径由 TurnEndGuard drop 补写 aborted)
    TurnEnded {
        /// success|error|cancelled|aborted
        status: String,
        /// 本轮步数
        steps: u64,
        /// 本轮时长(ms)
        duration_ms: u64,
    },
    /// 崩溃标记(P2 resume 检测到尾部无 turn_ended 后补写,运行时不写)
    SessionCrashed {
        /// 崩溃原因
        reason: String,
    },
    /// 续跑恢复标记(PR-2 接线)
    SessionResumed {
        /// 重放到的 seq
        replay_seq: u64,
        /// 重建项列表(消息历史/pending 审批/Runaway 计数)
        rebuilt: Vec<String>,
    },
}

/// 单行 journal 记录(读侧重放消费形态)
#[derive(Debug, Clone, PartialEq)]
pub struct JournalLine {
    /// 事件序号(从 1 连续递增)
    pub seq: u64,
    /// 事件时间戳(unix ms)
    pub ts: u64,
    /// 事件本体
    pub event: JournalEvent,
}

/// journal 错误面(打开/写入/重放全部显式化,不静默吞错)
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// 文件系统 IO 错误(打开/写入/读取)
    #[error("journal io: {0}")]
    Io(#[from] std::io::Error),
    /// 行级损坏(JSON 非法/缺字段/未知事件)
    #[error("journal corrupted: {0}")]
    Corrupt(String),
    /// seq 连续性破坏(空洞/重复 = 日志损坏,fail-visible)
    #[error("journal seq gap: expected {expected}, found {found}")]
    SeqGap {
        /// 期望的 seq
        expected: u64,
        /// 实际读到的 seq
        found: u64,
    },
    /// 事件序列化失败
    #[error("journal encode: {0}")]
    Encode(String),
}

/// 平铺编码:seq/ts + 事件的 type/payload 键合并为单行 JSON
/// (手工合并,避免 serde flatten 与 adjacently tagged 枚举组合的数值精度坑)
fn encode_line(seq: u64, ts: u64, event: &JournalEvent) -> Result<String, JournalError> {
    let ev = serde_json::to_value(event).map_err(|e| JournalError::Encode(e.to_string()))?;
    let ev_obj = match ev {
        serde_json::Value::Object(m) => m,
        _ => {
            return Err(JournalError::Encode(
                "event must serialize to object".into(),
            ))
        }
    };
    let mut line = serde_json::Map::new();
    line.insert("seq".to_string(), serde_json::json!(seq));
    line.insert("ts".to_string(), serde_json::json!(ts));
    for (k, v) in ev_obj {
        line.insert(k, v);
    }
    Ok(serde_json::Value::Object(line).to_string())
}

/// 单行解码:剥离 seq/ts 后按 adjacently tagged 枚举还原事件
fn decode_line(raw: &str) -> Result<JournalLine, JournalError> {
    let v: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| JournalError::Corrupt(format!("invalid json line: {e}")))?;
    let obj = v
        .as_object()
        .ok_or_else(|| JournalError::Corrupt("line is not an object".into()))?;
    let seq = obj
        .get("seq")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| JournalError::Corrupt("missing seq".into()))?;
    let ts = obj
        .get("ts")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| JournalError::Corrupt("missing ts".into()))?;
    let mut rest = obj.clone();
    rest.remove("seq");
    rest.remove("ts");
    let event: JournalEvent = serde_json::from_value(serde_json::Value::Object(rest))
        .map_err(|e| JournalError::Corrupt(format!("unknown event at seq {seq}: {e}")))?;
    Ok(JournalLine { seq, ts, event })
}

struct JournalInner {
    file: File,
    last_seq: u64,
    turn_count: u64,
    turn_open: bool,
}

/// journal 写入器(廉价 Clone,内部 Arc+Mutex;每会话一个实例)
#[derive(Clone)]
pub struct JournalWriter {
    core: Arc<Mutex<JournalInner>>,
}

impl JournalWriter {
    /// 打开(或续接)会话 journal;已有文件恢复 last_seq/轮数(G15 续跑同文件续写)。
    /// 既有文件 seq 不连续 = 日志损坏,open 即失败(fail-visible)。
    pub fn open(dir: &Path, session_id: &str) -> Result<JournalWriter, JournalError> {
        std::fs::create_dir_all(dir)?;
        let path = Self::path_for(dir, session_id);
        let mut last_seq = 0u64;
        let mut turn_count = 0u64;
        if path.exists() {
            let mut expected = 1u64;
            for line in BufReader::new(File::open(&path)?).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let parsed = decode_line(&line)?;
                if parsed.seq != expected {
                    return Err(JournalError::SeqGap {
                        expected,
                        found: parsed.seq,
                    });
                }
                expected += 1;
                last_seq = parsed.seq;
                if matches!(parsed.event, JournalEvent::TurnStarted { .. }) {
                    turn_count += 1;
                }
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(JournalWriter {
            core: Arc::new(Mutex::new(JournalInner {
                file,
                last_seq,
                turn_count,
                turn_open: false,
            })),
        })
    }

    /// 会话 journal 文件路径(`{dir}/{session_id}.jsonl`,session_id 消毒防路径注入)
    pub fn path_for(dir: &Path, session_id: &str) -> PathBuf {
        dir.join(format!("{}.jsonl", sanitize_session_id(session_id)))
    }

    /// seq 分配 + 行追加同一临界区,由调用方闭包在拿到 seq 后构造事件
    /// (tool_call_id/judgement_id 的确定性合成依赖此点)
    fn push_with<F: FnOnce(u64) -> JournalEvent>(&self, f: F) -> Result<u64, JournalError> {
        let mut g = self.core.lock().unwrap_or_else(|p| p.into_inner());
        let seq = g.last_seq + 1;
        let event = f(seq);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let line = encode_line(seq, ts, &event)?;
        g.file.write_all(line.as_bytes())?;
        g.file.write_all(b"\n")?;
        g.file.flush()?;
        g.last_seq = seq;
        Ok(seq)
    }

    fn push(&self, event: JournalEvent) -> Result<u64, JournalError> {
        self.push_with(|_| event)
    }

    /// turn 开始:写 turn_started,返回轮守卫(drop 未显式 end 时补写 aborted)
    pub fn begin_turn(&self, goal: &str) -> Result<TurnEndGuard, JournalError> {
        let turn_seq;
        {
            let mut g = self.core.lock().unwrap_or_else(|p| p.into_inner());
            turn_seq = g.turn_count + 1;
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let line = encode_line(
                g.last_seq + 1,
                ts,
                &JournalEvent::TurnStarted {
                    turn_seq,
                    goal: truncate_text(goal, 512),
                },
            )?;
            g.file.write_all(line.as_bytes())?;
            g.file.write_all(b"\n")?;
            g.file.flush()?;
            g.last_seq += 1;
            g.turn_count = turn_seq;
            g.turn_open = true;
        }
        Ok(TurnEndGuard {
            writer: self.clone(),
            turn_seq,
        })
    }

    /// turn 收尾(幂等:已收尾后重复调用为 no-op)
    pub fn end_turn(
        &self,
        status: &str,
        steps: u64,
        duration_ms: u64,
    ) -> Result<u64, JournalError> {
        let open = {
            let mut g = self.core.lock().unwrap_or_else(|p| p.into_inner());
            let open = g.turn_open;
            g.turn_open = false;
            open
        };
        if !open {
            return Ok(0);
        }
        self.push(JournalEvent::TurnEnded {
            status: status.to_string(),
            steps,
            duration_ms,
        })
    }

    /// 是否存在未收尾的 turn(测试与守卫语义用)
    pub fn turn_open(&self) -> bool {
        self.core
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .turn_open
    }

    /// llm_called(主循环 react 用途;provider token 真值优先,tokens_est 兜底)
    pub fn llm_called_react(
        &self,
        model: &str,
        evorule_request_id: Option<u64>,
        tokens: Option<TokenRecord>,
        tokens_est: Option<u64>,
        request_messages: usize,
        response_content: &str,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::LlmCalled {
            model: model.to_string(),
            purpose: "react".to_string(),
            evorule_request_id,
            tokens,
            tokens_est,
            request: request_messages,
            response: truncate_text(response_content, 200),
        })
    }

    /// tool_invoked:写事件并返回合成 call_id("t{seq}");
    /// 本路径失败时调用方跳过配对 tool_result(两件套同进同退)
    pub fn tool_invoked(
        &self,
        tool: &str,
        args: &serde_json::Value,
        evorule_request_id: Option<u64>,
    ) -> Result<String, JournalError> {
        let tool = tool.to_string();
        let args_digest = evorule_digest(&serde_json::to_string(args).unwrap_or_default());
        let seq = self.push_with(|seq| JournalEvent::ToolInvoked {
            call_id: format!("t{seq}"),
            tool,
            args_digest,
            evorule_request_id,
        })?;
        Ok(format!("t{seq}"))
    }

    /// tool_result:与 tool_invoked 经 call_id 配对;content = 工具输出全文
    /// (digest/size 就地计算,journal 不再复制大文本)
    pub fn tool_result(
        &self,
        call_id: &str,
        status: &str,
        content: &str,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::ToolResult {
            call_id: call_id.to_string(),
            status: status.to_string(),
            size_bytes: content.len(),
            content_digest: evorule_digest(content),
        })
    }

    /// F-902:压缩事件落 journal（上下文管理 09 规格 F-902）
    pub fn compaction_performed(
        &self,
        before_est: usize,
        after_est: usize,
        summary_generated: bool,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::CompactionPerformed {
            before_est,
            after_est,
            cleared_call_ids: vec![format!("summary_generated={}", summary_generated)],
        })
    }

    /// sediment 结果落 journal（受信通道持久化信号 + 四项结果对账依据）
    pub fn sediment_performed(
        &self,
        summary_written: bool,
        stable_facts: Vec<String>,
        stable_facts_cache_only: Vec<String>,
        events_count: usize,
        rollup_done: bool,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::SedimentPerformed {
            summary_written,
            stable_facts,
            stable_facts_cache_only,
            events_count,
            rollup_done,
        })
    }

    /// B-1:wire_rendered——本轮组装完成的 wire 全文落账(len/hash 就地计算)
    pub fn wire_rendered(&self, round: u64, full_text: &str) -> Result<u64, JournalError> {
        self.push(JournalEvent::WireRendered {
            round,
            wire_len: full_text.len(),
            content_hash: evorule_digest(full_text),
            full_text: full_text.to_string(),
        })
    }

    /// 审批请求开启
    pub fn approval_requested(
        &self,
        approval_id: &str,
        tool: &str,
        payload: &str,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::ApprovalRequested {
            approval_id: approval_id.to_string(),
            tool: tool.to_string(),
            payload: truncate_text(payload, 256),
        })
    }

    /// 审批决定
    pub fn approval_resolved(
        &self,
        approval_id: &str,
        decision: &str,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::ApprovalResolved {
            approval_id: approval_id.to_string(),
            decision: decision.to_string(),
        })
    }

    /// 治理裁决输出(judgement_id 由本事件 seq 确定性合成)
    pub fn policy_judged(&self, verdict: &str, evidence: &str) -> Result<u64, JournalError> {
        let evidence = evidence.to_string();
        self.push_with(|seq| JournalEvent::PolicyJudged {
            judgement_id: format!("j{seq}"),
            verdict: verdict.to_string(),
            evidence: truncate_text(&evidence, 256),
        })
    }
}

/// turn 守卫:优雅终止路径显式 `end(status, steps, duration)`;异常终止路径
/// (yield Err 早退 / panic unwind)未 end 即 drop 时补写 `turn_ended(aborted)`
/// ——保证 journal 尾部无「悬挂 open turn」(除进程死亡场景,由 PR-2 crash
/// 检测兜底)。
pub struct TurnEndGuard {
    writer: JournalWriter,
    /// 本 guard 对应的轮序号(begin_turn 时分配,与 turn_started.turn_seq 一致)
    turn_seq: u64,
}

impl TurnEndGuard {
    /// 本轮轮序号(wire_rendered 等逐轮事件的 round 来源)
    pub fn turn_seq(&self) -> u64 {
        self.turn_seq
    }

    /// 显式收尾(消费守卫;此后 drop 不再补写)
    pub fn end(self, status: &str, steps: u64, duration_ms: u64) {
        let _ = self.writer.end_turn(status, steps, duration_ms);
    }
}

impl Drop for TurnEndGuard {
    fn drop(&mut self) {
        // end_turn 幂等:显式 end 后此处为 no-op
        let _ = self.writer.end_turn("aborted", 0, 0);
    }
}

/// 顺序重放:逐行解析 + seq 连续性校验(空洞/重复 = 日志损坏,fail-visible)
pub fn read_all(path: &Path) -> Result<Vec<JournalLine>, JournalError> {
    let file = File::open(path)?;
    let mut out = Vec::new();
    let mut expected = 1u64;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed = decode_line(&line)?;
        if parsed.seq != expected {
            return Err(JournalError::SeqGap {
                expected,
                found: parsed.seq,
            });
        }
        expected = parsed.seq + 1;
        out.push(parsed);
    }
    Ok(out)
}

/// session_id 消毒:仅保留 [A-Za-z0-9._-],其余替 '_',防路径注入/跨平台文件名问题
/// (公开:atif 导出面需以同口径回写 source_journal 相对路径)
pub fn sanitize_session_id(sid: &str) -> String {
    let cleaned: String = sid
        .chars()
        .take(128)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

/// 摘要截断(按字符数,不切 UTF-8 字节面)
fn truncate_text(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        s.chars().take(max_chars).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use JournalEvent as JE;

    #[test]
    fn roundtrip_all_13_events() {
        let events = vec![
            JE::TurnStarted {
                turn_seq: 1,
                goal: "写个快排".into(),
            },
            JE::LlmCalled {
                model: "glm-5.3".into(),
                purpose: "react".into(),
                evorule_request_id: Some(42),
                tokens: Some(TokenRecord {
                    prompt: 100,
                    completion: 50,
                    total: 150,
                }),
                tokens_est: Some(160),
                request: 7,
                response: "assistant 全文摘要".into(),
            },
            JE::ToolInvoked {
                call_id: "t3".into(),
                tool: "file_read".into(),
                args_digest: evorule_digest(r#"{"path":"a.txt"}"#),
                evorule_request_id: None,
            },
            JE::ToolResult {
                call_id: "t3".into(),
                status: "ok".into(),
                size_bytes: 128,
                content_digest: evorule_digest("file body"),
            },
            JE::ApprovalRequested {
                approval_id: "p1".into(),
                tool: "file_write".into(),
                payload: "write /tmp/x".into(),
            },
            JE::ApprovalResolved {
                approval_id: "p1".into(),
                decision: "approved".into(),
            },
            JE::PolicyJudged {
                judgement_id: "j7".into(),
                verdict: "allowed".into(),
                evidence: "file_read in_sandbox".into(),
            },
            JE::CompactionPerformed {
                before_est: 9000,
                after_est: 4000,
                cleared_call_ids: vec!["t3".into()],
            },
            JE::SedimentPerformed {
                summary_written: true,
                stable_facts: vec!["stable.llm.x".into()],
                stable_facts_cache_only: vec![],
                events_count: 2,
                rollup_done: false,
            },
            JE::WireRendered {
                round: 1,
                wire_len: 9,
                content_hash: evorule_digest("wire body"),
                full_text: "wire body".into(),
            },
            JE::TurnEnded {
                status: "success".into(),
                steps: 5,
                duration_ms: 1234,
            },
            JE::SessionCrashed {
                reason: "stream error".into(),
            },
            JE::SessionResumed {
                replay_seq: 9,
                rebuilt: vec!["pending_approvals".into()],
            },
        ];
        for (i, ev) in events.iter().enumerate() {
            let line = encode_line(i as u64 + 1, 1000 + i as u64, ev).unwrap();
            let parsed = decode_line(&line).unwrap();
            assert_eq!(parsed.seq, i as u64 + 1);
            assert_eq!(parsed.ts, 1000 + i as u64);
            assert_eq!(&parsed.event, ev, "roundtrip mismatch at variant {i}");
        }
    }

    #[test]
    fn wire_rendered_fields_and_hash_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let w = JournalWriter::open(dir.path(), "s-wire").unwrap();
        let text = "S1_base\n\nS3_memory(裁剪后)";
        w.wire_rendered(1, text).unwrap();
        let lines = read_all(&JournalWriter::path_for(dir.path(), "s-wire")).unwrap();
        assert_eq!(lines.len(), 1);
        match &lines[0].event {
            JE::WireRendered {
                round,
                wire_len,
                content_hash,
                full_text,
            } => {
                assert_eq!(*round, 1);
                assert_eq!(*wire_len, text.len());
                assert_eq!(full_text, text);
                assert_eq!(
                    content_hash,
                    &evorule_digest(text),
                    "hash 可由全文复算(F-903 重建比对基准)"
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn turn_guard_exposes_turn_seq() {
        let dir = tempfile::tempdir().unwrap();
        let w = JournalWriter::open(dir.path(), "s-seq").unwrap();
        let g1 = w.begin_turn("g1").unwrap();
        assert_eq!(g1.turn_seq(), 1);
        g1.end("success", 0, 0);
        let g2 = w.begin_turn("g2").unwrap();
        assert_eq!(g2.turn_seq(), 2);
        g2.end("success", 0, 0);
    }

    #[test]
    fn line_format_is_flat_seq_ts_type_payload() {
        let line = encode_line(
            3,
            1700000000000,
            &JE::ToolInvoked {
                call_id: "t3".into(),
                tool: "grep".into(),
                args_digest: "blake3:ab".into(),
                evorule_request_id: Some(7),
            },
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["seq"], 3);
        assert_eq!(v["ts"], 1_700_000_000_000u64);
        assert_eq!(v["type"], "tool_invoked");
        assert_eq!(v["payload"]["tool"], "grep");
        assert_eq!(v["payload"]["call_id"], "t3");
    }

    #[test]
    fn append_sequential_and_reopen_recovers_last_seq() {
        let dir = tempfile::tempdir().unwrap();
        let sid = "sess-1";
        {
            let w = JournalWriter::open(dir.path(), sid).unwrap();
            w.begin_turn("goal A").unwrap().end("success", 2, 100);
            w.tool_invoked("file_read", &serde_json::json!({"path":"a"}), None)
                .unwrap();
        }
        // 重开(模拟 serve 重启后续接同一会话)
        let w2 = JournalWriter::open(dir.path(), sid).unwrap();
        let seq = w2
            .tool_invoked("grep", &serde_json::json!({"pattern":"x"}), Some(9))
            .unwrap();
        assert_eq!(seq, "t4"); // 3 条已有 + 本次
        let lines = read_all(&JournalWriter::path_for(dir.path(), sid)).unwrap();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines.last().unwrap().seq, 4);
        assert_eq!(
            lines[0].event,
            JE::TurnStarted {
                turn_seq: 1,
                goal: "goal A".into()
            }
        );
    }

    #[test]
    fn seq_gap_is_fail_visible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gap.jsonl");
        std::fs::write(
            &path,
            "{\"seq\":1,\"ts\":1,\"type\":\"turn_started\",\"payload\":{\"turn_seq\":1,\"goal\":\"g\"}}\n\
             {\"seq\":3,\"ts\":3,\"type\":\"turn_ended\",\"payload\":{\"status\":\"success\",\"steps\":1,\"duration_ms\":1}}\n",
        )
        .unwrap();
        match read_all(&path) {
            Err(JournalError::SeqGap { expected, found }) => {
                assert_eq!((expected, found), (2, 3));
            }
            other => panic!("expected SeqGap, got {other:?}"),
        }
    }

    #[test]
    fn turn_guard_explicit_end_writes_once() {
        let dir = tempfile::tempdir().unwrap();
        let w = JournalWriter::open(dir.path(), "s").unwrap();
        {
            let g = w.begin_turn("goal").unwrap();
            assert!(w.turn_open());
            g.end("cancelled", 4, 500);
        }
        assert!(!w.turn_open());
        let lines = read_all(&JournalWriter::path_for(dir.path(), "s")).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[1].event,
            JE::TurnEnded {
                status: "cancelled".into(),
                steps: 4,
                duration_ms: 500
            }
        );
    }

    #[test]
    fn turn_guard_drop_writes_aborted() {
        let dir = tempfile::tempdir().unwrap();
        let w = JournalWriter::open(dir.path(), "s").unwrap();
        {
            let _g = w.begin_turn("goal").unwrap();
            // 不显式 end,直接 drop(模拟 yield Err 早退/panic)
        }
        let lines = read_all(&JournalWriter::path_for(dir.path(), "s")).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[1].event,
            JE::TurnEnded {
                status: "aborted".into(),
                steps: 0,
                duration_ms: 0
            }
        );
    }

    #[test]
    fn sanitize_session_id_blocks_path_tricks() {
        assert_eq!(sanitize_session_id("../.."), ".._..");
        assert_eq!(sanitize_session_id("a/b\\c d"), "a_b_c_d");
        assert_eq!(sanitize_session_id(""), "unknown");
        let long = "x".repeat(300);
        assert_eq!(sanitize_session_id(&long).len(), 128);
    }
}
