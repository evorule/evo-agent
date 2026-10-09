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

/// 粒级检查点的结果内联阈值(字节):≤ 阈值直存检查点事件,超限全文走
/// checkpoint_blob 专属事件——账本永久面不存大载荷(与 wire blob 同款
/// 不膨胀口径)
pub const CHECKPOINT_INLINE_LIMIT: usize = 4096;

/// 粒级检查点的结果引用(≤ 阈值内联直存;超限 inline 缺省即 None,
/// 全文在同账本 checkpoint_blob 事件,按 node_id+hash 配对检索)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointResultRef {
    /// 全文 digest(evorule-hash 口径;"blake3:"+64hex)
    pub hash: String,
    /// 全文字节数
    pub len: usize,
    /// ≤ 阈值内联直存;超限 None(全文在同账本 checkpoint_blob 事件)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline: Option<String>,
}

/// 计划循环检查点的预算计数器快照(驱动预算四维的三计数器投影;
/// 与驱动内存态同构,字段语义见驱动侧定义)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct BudgetCountersSnapshot {
    /// 已成功完成节点数
    pub nodes_executed: u64,
    /// 累计墙钟毫秒
    pub wall_ms: u64,
    /// 累计 token
    pub tokens_used: u64,
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

/// journal 事件(19 种;schema 终版 = ATIF 映射表 §二增补)
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
    /// 工具执行重试落账(幂等类工具遇瞬态故障自动重试;每次重试独立一条,
    /// 首次执行不落此事件——观测面,不参与步映射)
    ToolRetried {
        /// 工具名
        tool: String,
        /// 重试次序(从 1 计:第 attempt 次重试)
        attempt: u64,
        /// 失败是否判定为瞬态(连接/超时类错误形态)
        transient: bool,
    },
    /// 粒级检查点(节点完成/跳过后、下一层规划前落账;账先于状态转移——
    /// 账本状态 ≥ 内存状态)。恢复 = 回放本事件重建已完成/已跳过集合;
    /// 计划锚不匹配 = 拒绝恢复(宁可重跑不可错续)。观测面,不参与步映射。
    NodeCheckpointed {
        /// 工作流 id(run 级账本会话以 planrun- 前缀按工作流 id+唯一指纹命名)
        workflow_id: String,
        /// 计划锚(物化 Workflow canonical JSON 的 64-hex hash,evorule-hash
        /// 口径;防错版本续跑的关键闸)
        plan_hash: String,
        /// 节点 id
        node_id: String,
        /// completed|skipped
        status: String,
        /// 结果引用(completed 带产出引用;skipped 为零长引用)
        result_ref: CheckpointResultRef,
    },
    /// 粒级检查点的大结果全文载体(结果超内联阈值时随行落账;与 wire blob
    /// 同款机制:引用面只存 hash+len,全文在专属事件;全文级重建随保留策略
    /// 降级时,hash+长度仍可校验完整性)。观测面,不参与步映射。
    CheckpointBlob {
        /// 配对检索键:与 NodeCheckpointed.plan_hash 同值
        plan_hash: String,
        /// 对应节点 id
        node_id: String,
        /// 全文 digest(evorule-hash 口径)
        hash: String,
        /// 全文字节数
        len: usize,
        /// 全文
        full_text: String,
    },
    /// 计划循环检查点(外层驱动状态:版本/replan 计数/已执行注册表/预算计数器/
    /// 计划形态锚/当前版工作流全文)。写点:每版计划物化后+每次 replan 物化后+
    /// 每轮执行后计数器注册表落定处。恢复 = 回放最新本事件重建驱动状态、
    /// 其后检查点尾段回放粒级进度。观测面,不参与步映射。
    PlanLoopCheckpointed {
        /// 当前计划版本(v1 起)
        version: u32,
        /// 已发生 replan 次数
        replan_count: u32,
        /// 已执行注册表 (node_id, agent_type) 跨版本累积(静态拦截比对源)
        executed_registry: Vec<(String, String)>,
        /// 预算计数器(节点数/墙钟毫秒/token)
        counters: BudgetCountersSnapshot,
        /// 计划形态锚(注入后 PlanFact canonical JSON 的 64-hex hash;Dsl 形态
        /// None——Dsl 工作流由调用方入参可复建,不落本事件)
        cur_canonical_hash: Option<String>,
        /// 原始目标文本(replan 任务构造的输入;None = 无目标形态)
        goal: Option<String>,
        /// 协作标记会话 id(run 的治理身份;恢复必须复用同会话,全新标记
        /// 会话会让 phase 前置门误拦;None = 未启用标记)
        marks_session: Option<String>,
        /// 原子粒记忆集(曾携带 atomic 标记的节点 id;原子性跨版本持续有效
        /// ——replan 重产计划不带该字段,记忆集合补事实连续性;恢复不归零)
        atomic_granules: Vec<String>,
        /// 按节点重切计数(原子粒重切预算判定的跨版本累计输入;恢复不归零
        /// ——归零=恢复 run 至多多切 N 次,预算面方差)
        recut_counts: Vec<(String, u32)>,
        /// 当前版工作流全文(replan 产物源自非确定 LLM 输出,不落全文即不可
        /// 确定性重建——这是恢复面唯一的状态载体)
        cur_workflow: String,
    },
    /// 计划循环终态标记(驱动循环任意出口落地:ok=完成,error=终断)。恢复面
    /// 凭本事件判别「已完成/已终断,不再列可恢复」;无终态标记且计划检查点
    /// 在账 = 中断 run(可恢复候选)。观测面,不参与步映射。
    PlanLoopFinished {
        /// ok|error
        status: String,
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
        /// 写入共享账本的知识候选数（serde default 保旧 journal 兼容）
        #[serde(default)]
        knowledge_candidates: usize,
        /// 会话收尾补写成功的离线积压事件数（serde default 保旧 journal 兼容）
        #[serde(default)]
        flushed_events: usize,
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
    /// C-3(收尾清偿批,F-905 初版):I2 分区间字面级冲突扫描报告——组装后
    /// 检出跨分区「禁令×声明」矛盾时落账(仅检出时写,不阻断会话;与 F-201
    /// 加载拒载分层)。REST 可查=ATIF context_management 步。
    I2ScanReport {
        /// 轮序号(与 turn_started.turn_seq 同源)
        round: u64,
        /// 冲突记录(初版词法子集,见 context_inspector)
        conflicts: Vec<crate::agent::context_inspector::I2ConflictRecord>,
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
    /// 摘要保真对照（规格修正批交付物 B）：G10 摘要替换后，被裁剪消息
    /// 确定性锚点 vs 摘要文本的命中对照落账（I2ScanReport 同款形态；
    /// ratio 为四舍五入 4 位小数；summary_empty=空摘要跳过判定）
    SummaryFidelityScan {
        /// 会话 ID
        session: String,
        /// 被裁剪消息数
        trimmed_n: usize,
        /// 锚点数
        anchors_n: usize,
        /// 命中锚点数
        hit_n: usize,
        /// 保真比（0.0-1.0，4 位小数）
        ratio: f64,
        /// 摘要为空（跳过判定；事件照落）
        summary_empty: bool,
    },
    /// wire blob 过期标记(journal 体积治理批:保留期窗口外降级可见,
    /// 不留静默空洞——I4 全文级重建保证随窗口关闭降级为 hash 校验)
    WireBlobExpired {
        /// 轮序号(与 wire_rendered.round 同源)
        round: u64,
        /// wire 全文 digest(降级后仅存校验凭证)
        content_hash: String,
        /// wire 字节长度(与 hash 组成完整性校验对)
        wire_len: usize,
    },
    /// 召回集观测（检索质量观测批 K-11 观测级）：每次 recall 的命中集
    /// （"层@序:key" 条目，排序位即分位）。ground truth 判据列二期，
    /// 先积累数据（context-inspector A/B 面读取展示）
    RecallSet {
        /// 会话 ID
        session: String,
        /// 命中集条目（"层@序:key" 格式）
        hits: Vec<String>,
    },
    /// LexStore 缓存观测（补齐路线图 P2-1/TTL 窗口可见性；recall_set 同族
    /// 观测级事件）：三计数器累计快照——「跨代理写不可见」的 TTL 窗口从
    /// 已声明边界升级为可观测边界
    LexCacheStats {
        /// 会话 ID
        session: String,
        /// cached_facts 命中次数（读到 TTL 窗口内缓存）
        hit: u64,
        /// 缓存不可用次数（TTL 过期/从未拉取/读取失败）
        expired: u64,
        /// 全量拉取（replace_partition）执行次数
        fetch: u64,
    },
    /// 冷迁落账（F-617 冷热分层）：沉淀收尾冷迁批次的事件计数——
    /// 热库 Archived/Tombstoned/Decayed 行事务移入 lex-cold.db。
    /// 账面事件不映射 ATIF 步（观测面）。
    ColdMoved {
        /// 本次冷迁行数
        count: u64,
    },
    /// 子代理委托观测（delegate 子代理上下文规格批）：父会话在 delegate 工具
    /// 调用帧内实际创建的子会话锚——父→子链路唯一可发现锚点（子 journal
    /// 文件以 child_session_id 命名,无本事件则子轨迹成孤儿）。主轨迹 ATIF
    /// 导出跳过本事件（delegate 仍呈现为普通工具调用,映射口径不变）。
    DelegateSpawned {
        /// 子 evorule 会话 id
        child_session_id: String,
        /// 子代理类型
        agent_type: String,
        /// 委托深度（父自身为第 0 层,子代理为 current_depth+1）
        depth: usize,
        /// 委托任务文本 digest（evorule-hash 口径,与 args_digest 同源）
        task_digest: String,
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
    /// 自主交接落账(自主交接设计 PR-H2):handover_write 成功写交接档后落账。
    /// 交接点=跨会话链的因果锚(续接会话首动作 handover_read 消费);写档
    /// 动作镜像已在 tool_invoked/tool_result,本事件携带结构化锚(path/id)
    /// 供跨会话链对账与死信可见性。
    HandoverWritten {
        /// 会话 ID(写方)
        session: String,
        /// 交接档 fact 路径(shared.{namespace}.handovers.{id})
        path: String,
        /// schema 校验结果(写侧 fail-visible 拒写,应恒 true;保留字段容错)
        schema_ok: bool,
    },
    /// 子会话派生落账(自主交接设计 PR-H3):session_spawn 成功 fork 出子会话
    /// 后落账。server 侧 parent_session_id 链入账为机制层权威,本事件为应用
    /// 层派生锚——链对账/深度审计/派生时序取证消费。
    SessionSpawned {
        /// 父会话 ID(spawn 发起方)
        parent: String,
        /// 子会话 ID(fork 产物)
        child: String,
        /// 子会话链深度(根=0;深度硬顶护栏的审计面)
        depth: u32,
    },
    /// 会话链熔断落账(自主交接设计 §3.4 护栏三件):新会话连续失败熔断或
    /// spawn 同签名重复触发停链后落账。reason 为确定性判据描述(轮数/签名
    /// 计数),停链后链上后续 spawn 预检拒绝(可查)。
    ChainHalted {
        /// 停链事件所在会话 ID
        session: String,
        /// 熔断判据(确定性:轮数/同签名计数/预算/深度)
        reason: String,
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
    /// 同会话已有活跃写者(并发双写会使 seq 交错损坏账面,fail-fast;
    /// 旧写者 Drop 释放占位后可重开)
    #[error("journal writer already active for session: {0}")]
    WriterActive(String),
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
    /// 会话 ID(open 时登记;turn 守卫链熔断落账需要会话锚,免透传)
    session_id: String,
}

/// journal 写入器(廉价 Clone,内部 Arc+Mutex;每会话一个实例)
#[derive(Clone)]
pub struct JournalWriter {
    core: Arc<Mutex<JournalInner>>,
    /// 进程内活跃写者注册表键(sanitized session_id);最后一个克隆
    /// Drop 时释放占位
    registry_key: String,
    /// journal 文件路径(会话收尾投影读回用;与写者并存只读句柄)
    file_path: PathBuf,
}

/// 进程内活跃写者注册表(键=sanitized session_id)
static ACTIVE_WRITERS: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
    std::sync::Mutex::new(None);

impl Drop for JournalWriter {
    fn drop(&mut self) {
        // 仅最后一个克隆释放占位(中间克隆 drop 时 Arc 强计数 > 1,不放锁)
        if Arc::strong_count(&self.core) == 1 {
            if let Ok(mut guard) = ACTIVE_WRITERS.lock() {
                if let Some(set) = guard.as_mut() {
                    set.remove(&self.registry_key);
                }
            }
        }
    }
}

impl JournalWriter {
    /// 打开(或续接)会话 journal;已有文件恢复 last_seq/轮数(G15 续跑同文件续写)。
    /// 既有文件 seq 不连续 = 日志损坏,open 即失败(fail-visible)。
    pub fn open(dir: &Path, session_id: &str) -> Result<JournalWriter, JournalError> {
        std::fs::create_dir_all(dir)?;
        let path = Self::path_for(dir, session_id);
        let mut last_seq = 0u64;
        let mut turn_count = 0u64;
        // 尾部悬挂检测:turn_started 无配对 turn_ended = 上一进程死前轮未收尾。
        // 崩溃标记只在打开时补写(进程死亡瞬间什么也写不了,运行时不产生此
        // 事件);末事件已是崩溃标记则不重复补写(重复打开幂等)。
        let mut tail_hung = false;
        let mut last_was_crash = false;
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
                    tail_hung = true;
                } else if matches!(parsed.event, JournalEvent::TurnEnded { .. }) {
                    tail_hung = false;
                }
                last_was_crash = matches!(parsed.event, JournalEvent::SessionCrashed { .. });
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        // 占位进程内活跃写者——同会话第二个写者在此 fail-fast,
        // 不再出现两写者各自恢复 last_seq 后交错 append(seq 重复/空洞=账面损坏)
        let registry_key = sanitize_session_id(session_id);
        {
            let mut guard = ACTIVE_WRITERS
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let set = guard.get_or_insert_with(std::collections::HashSet::new);
            if !set.insert(registry_key.clone()) {
                return Err(JournalError::WriterActive(registry_key));
            }
        }
        let writer = JournalWriter {
            core: Arc::new(Mutex::new(JournalInner {
                file,
                last_seq,
                turn_count,
                turn_open: false,
                session_id: session_id.to_string(),
            })),
            registry_key,
            file_path: path,
        };
        if tail_hung && !last_was_crash {
            writer.push(JournalEvent::SessionCrashed {
                reason: "unclean_tail".to_string(),
            })?;
        }
        Ok(writer)
    }

    /// 本写者对应的会话 ID(open 时登记)
    pub fn session_id(&self) -> String {
        self.core
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .session_id
            .clone()
    }

    /// 读取全量 journal 行(会话收尾投影消费;与写者并存只读句柄)。
    /// 读取失败如实上抛(调用方 best-effort 降级)。
    pub fn read_lines(&self) -> Result<Vec<JournalLine>, JournalError> {
        read_all(&self.file_path)
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
            chain_watch: None,
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

    /// 续跑恢复标记:恢复路径入口落账。replay_seq = 重建所回放到的账目
    /// seq;rebuilt = 重建项清单(消息历史/pending 审批/Runaway 计数等条目)。
    pub fn session_resumed(
        &self,
        replay_seq: u64,
        rebuilt: Vec<String>,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::SessionResumed {
            replay_seq,
            rebuilt,
        })
    }

    /// 工具执行重试落账(attempt 从 1 计:第 attempt 次重试;幂等类工具
    /// 遇瞬态故障的重试面观测事件)。
    pub fn tool_retried(
        &self,
        tool: &str,
        attempt: u64,
        transient: bool,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::ToolRetried {
            tool: tool.to_string(),
            attempt,
            transient,
        })
    }

    /// 粒级检查点落账:NodeCheckpointed 事件 + 结果超内联阈值时自动追加
    /// CheckpointBlob 全文载体事件。返回 (checkpoint_seq, blob_seq:Option)。
    /// completed 传产出全文;skipped 传空串(引用=零长哈希,inline 直存)。
    pub fn node_checkpointed(
        &self,
        workflow_id: &str,
        plan_hash: &str,
        node_id: &str,
        status: &str,
        result: &str,
    ) -> Result<(u64, Option<u64>), JournalError> {
        let rref = CheckpointResultRef {
            hash: evorule_digest(result),
            len: result.len(),
            inline: if result.len() <= CHECKPOINT_INLINE_LIMIT {
                Some(result.to_string())
            } else {
                None
            },
        };
        let event = JournalEvent::NodeCheckpointed {
            workflow_id: workflow_id.to_string(),
            plan_hash: plan_hash.to_string(),
            node_id: node_id.to_string(),
            status: status.to_string(),
            result_ref: rref,
        };
        let seq = self.push(event)?;
        if result.len() > CHECKPOINT_INLINE_LIMIT {
            let blob = JournalEvent::CheckpointBlob {
                plan_hash: plan_hash.to_string(),
                node_id: node_id.to_string(),
                hash: evorule_digest(result),
                len: result.len(),
                full_text: result.to_string(),
            };
            let blob_seq = self.push(blob)?;
            return Ok((seq, Some(blob_seq)));
        }
        Ok((seq, None))
    }

    /// 计划循环检查点落账(驱动状态全量投影;恢复面回放最新一条重建,
    /// 其后检查点尾段回放粒级进度)。
    #[allow(clippy::too_many_arguments)]
    pub fn plan_loop_checkpointed(
        &self,
        version: u32,
        replan_count: u32,
        executed_registry: Vec<(String, String)>,
        counters: BudgetCountersSnapshot,
        cur_canonical_hash: Option<String>,
        goal: Option<String>,
        marks_session: Option<String>,
        atomic_granules: Vec<String>,
        recut_counts: Vec<(String, u32)>,
        cur_workflow: &str,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::PlanLoopCheckpointed {
            version,
            replan_count,
            executed_registry,
            counters,
            cur_canonical_hash,
            goal,
            marks_session,
            atomic_granules,
            recut_counts,
            cur_workflow: cur_workflow.to_string(),
        })
    }

    /// 计划循环终态标记(ok=完成/error=终断;扫尾面凭此把已终局 run 排除出
    /// 可恢复列表)
    pub fn plan_loop_finished(&self, status: &str) -> Result<u64, JournalError> {
        self.push(JournalEvent::PlanLoopFinished {
            status: status.to_string(),
        })
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
        self.llm_called(
            model,
            "react",
            evorule_request_id,
            tokens,
            tokens_est,
            request_messages,
            response_content,
        )
    }

    /// llm_called 通用形态(sidecar 用途落账——语义精判等;purpose 透传,
    /// ATIF 侧 sidecar 用途不映射步=既有口径)
    pub fn llm_called(
        &self,
        model: &str,
        purpose: &str,
        evorule_request_id: Option<u64>,
        tokens: Option<TokenRecord>,
        tokens_est: Option<u64>,
        request_messages: usize,
        response_content: &str,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::LlmCalled {
            model: model.to_string(),
            purpose: purpose.to_string(),
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

    /// sediment 结果落 journal（受信通道持久化信号 + 沉淀结果对账依据）
    pub fn sediment_performed(
        &self,
        summary_written: bool,
        stable_facts: Vec<String>,
        stable_facts_cache_only: Vec<String>,
        events_count: usize,
        rollup_done: bool,
        knowledge_candidates: usize,
        flushed_events: usize,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::SedimentPerformed {
            summary_written,
            stable_facts,
            stable_facts_cache_only,
            events_count,
            rollup_done,
            knowledge_candidates,
            flushed_events,
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

    /// C-3:i2_scan_report——I2 冲突扫描报告落账(仅检出冲突时调用)
    pub fn i2_scan_report(
        &self,
        round: u64,
        conflicts: Vec<crate::agent::context_inspector::I2ConflictRecord>,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::I2ScanReport { round, conflicts })
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
    /// 召回集观测落账(检索质量观测批 K-11 观测级;每会话 recall 后调用,
    /// journal 在位才落——ground truth 判据列二期,先积累数据)。
    pub fn recall_set(&self, session: &str, hits: Vec<String>) -> Result<u64, JournalError> {
        self.push(JournalEvent::RecallSet {
            session: session.to_string(),
            hits,
        })
    }

    /// LexStore 缓存观测落账（补齐路线图 P2-1；recall_set 同族观测级，
    /// best-effort——调用方决定失败处置）
    pub fn lex_cache_stats(
        &self,
        session: &str,
        hit: u64,
        expired: u64,
        fetch: u64,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::LexCacheStats {
            session: session.to_string(),
            hit,
            expired,
            fetch,
        })
    }

    /// wire blob 过期标记落账(journal 体积治理批;离线 GC 经 writer 追加,
    /// 复用活跃写者锁=并发安全)。best-effort 调用方决定失败处置。
    pub fn wire_blob_expired(
        &self,
        round: u64,
        content_hash: &str,
        wire_len: usize,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::WireBlobExpired {
            round,
            content_hash: content_hash.to_string(),
            wire_len,
        })
    }

    /// 摘要保真对照落账(规格修正批交付物 B;每次 G10 摘要替换自动对照,
    /// I5 从"原则上可对照"升级为"每次压缩自动对照")。best-effort 调用方
    /// 决定失败处置(留痕不阻塞)。
    pub fn summary_fidelity_scan(
        &self,
        session: &str,
        trimmed_n: usize,
        anchors_n: usize,
        hit_n: usize,
        ratio: f64,
        summary_empty: bool,
    ) -> Result<u64, JournalError> {
        self.push_with(|_| JournalEvent::SummaryFidelityScan {
            session: session.to_string(),
            trimmed_n,
            anchors_n,
            hit_n,
            ratio,
            summary_empty,
        })
    }

    /// 自主交接落账(自主交接设计 PR-H2):handover_write 成功写交接档后落
    /// handover_written(结构化锚:path/id;写档动作镜像已在 tool_invoked/
    /// tool_result,本事件供跨会话链对账)。
    pub fn handover_written(
        &self,
        session: &str,
        path: &str,
        schema_ok: bool,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::HandoverWritten {
            session: session.to_string(),
            path: path.to_string(),
            schema_ok,
        })
    }

    /// 子会话派生落账(自主交接设计 PR-H3):session_spawn 成功 fork 后由
    /// runner 侧按工具名分支调用(best-effort,与 handover_written 同风格)。
    pub fn session_spawned(
        &self,
        parent: &str,
        child: &str,
        depth: u32,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::SessionSpawned {
            parent: parent.to_string(),
            child: child.to_string(),
            depth,
        })
    }

    /// 会话链熔断落账(自主交接设计 §3.4):连续失败熔断/spawn 同签名重复
    /// 停链后落账(turn 守卫与 spawn 执行体两个触发源,自带会话锚)。
    pub fn chain_halted(&self, session: &str, reason: &str) -> Result<u64, JournalError> {
        self.push(JournalEvent::ChainHalted {
            session: session.to_string(),
            reason: reason.to_string(),
        })
    }

    pub fn policy_judged(&self, verdict: &str, evidence: &str) -> Result<u64, JournalError> {
        let evidence = evidence.to_string();
        self.push_with(|seq| JournalEvent::PolicyJudged {
            judgement_id: format!("j{seq}"),
            verdict: verdict.to_string(),
            evidence: truncate_text(&evidence, 256),
        })
    }

    /// 冷迁计数落账(F-617;best-effort 调用方决定失败处置)
    pub fn cold_moved(&self, count: u64) -> Result<u64, JournalError> {
        self.push(JournalEvent::ColdMoved { count })
    }

    /// 子代理委托观测落账（delegate 子代理上下文规格批;delegate 工具
    /// ToolResult 写账前调用,best-effort 调用方决定失败处置）
    pub fn delegate_spawned(
        &self,
        child_session_id: &str,
        agent_type: &str,
        depth: usize,
        task_digest: &str,
    ) -> Result<u64, JournalError> {
        self.push(JournalEvent::DelegateSpawned {
            child_session_id: child_session_id.to_string(),
            agent_type: agent_type.to_string(),
            depth,
            task_digest: task_digest.to_string(),
        })
    }
}

/// 委托树发现（纯函数）:单份 journal 内全部子代理委托锚,按 seq 升序。
/// 消费面=子轨迹批量导出（父 journal 扫描即得全树 sid 清单,子轨迹
/// 经既有导出通路逐个产出）。
pub fn scan_delegate_spawns(journal: &[JournalLine]) -> Vec<DelegateSpawnRecord> {
    journal
        .iter()
        .filter_map(|line| match &line.event {
            JournalEvent::DelegateSpawned {
                child_session_id,
                agent_type,
                depth,
                task_digest,
            } => Some(DelegateSpawnRecord {
                seq: line.seq,
                child_session_id: child_session_id.clone(),
                agent_type: agent_type.clone(),
                depth: *depth,
                task_digest: task_digest.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// 单条委托锚记录（scan_delegate_spawns 产物）
#[derive(Debug, Clone, PartialEq)]
pub struct DelegateSpawnRecord {
    /// 事件序号（父 journal 内 seq,时序即此）
    pub seq: u64,
    /// 子 evorule 会话 id
    pub child_session_id: String,
    /// 子代理类型
    pub agent_type: String,
    /// 委托深度
    pub depth: usize,
    /// 委托任务文本 digest
    pub task_digest: String,
}

/// turn 守卫:优雅终止路径显式 `end(status, steps, duration)`;异常终止路径
/// (yield Err 早退 / panic unwind)未 end 即 drop 时补写 `turn_ended(aborted)`
/// ——保证 journal 尾部无「悬挂 open turn」(除进程死亡场景,由 PR-2 crash
/// 检测兜底)。
pub struct TurnEndGuard {
    writer: JournalWriter,
    /// 本 guard 对应的轮序号(begin_turn 时分配,与 turn_started.turn_seq 一致)
    turn_seq: u64,
    /// 会话链熔断观察(自主交接设计 §3.4 护栏三件;None = 非链成员会话,零开销)。
    /// 显式 end 时观察轮结局;连续失败达阈值 → 共享链态置停链标记 + 本 journal
    /// 落 chain_halted(链上后续 spawn 预检拒绝,停链可查)。
    chain_watch: Option<crate::agent::session_spawn_tool::ChainWatch>,
}

impl TurnEndGuard {
    /// 本轮轮序号(wire_rendered 等逐轮事件的 round 来源)
    pub fn turn_seq(&self) -> u64 {
        self.turn_seq
    }

    /// 挂接会话链熔断观察(自主交接批;clone_for_spawn 构造的子会话 runner
    /// 在 turn_guard 建立时挂入,链外会话不挂)
    pub(crate) fn attach_chain_watch(
        &mut self,
        watch: crate::agent::session_spawn_tool::ChainWatch,
    ) {
        self.chain_watch = Some(watch);
    }

    /// 显式收尾(消费守卫;此后 drop 不再补写)
    pub fn end(mut self, status: &str, steps: u64, duration_ms: u64) {
        let _ = self.writer.end_turn(status, steps, duration_ms);
        // take() 而非 move 字段:TurnEndGuard 实现 Drop,整体消费路径上
        // 不可部分移出字段(E0509);take 后 drop 兜底路径不受影响
        if let Some(mut watch) = self.chain_watch.take() {
            if let Some(reason) = watch.observe(self.turn_seq, status) {
                if let Err(e) = self.writer.chain_halted(&self.writer.session_id(), &reason) {
                    tracing::warn!(error = %e, "chain_halted journal append failed");
                }
            }
        }
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
/// journal 体积治理 GC 报告（规格修正批批次四/K-01）
#[derive(Debug, Default, Clone, PartialEq)]
pub struct GcReport {
    /// 扫描的 journal 文件数
    pub files_scanned: usize,
    /// 归档到旁路件的 wire blob 数（全文出 journal，可回读）
    pub wire_archived: usize,
    /// 过期删除的 wire blob 数（仅存 hash+长度，降级可见）
    pub wire_expired: usize,
    /// 处理前字节总量
    pub bytes_before: u64,
    /// 处理后字节总量
    pub bytes_after: u64,
}

fn collect_journal_jsonl(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), JournalError> {
    let entries = std::fs::read_dir(dir).map_err(JournalError::Io)?;
    for entry in entries {
        let path = entry.map_err(JournalError::Io)?.path();
        if path.is_dir() {
            collect_journal_jsonl(&path, out)?;
        } else if path.extension().map(|e| e == "jsonl").unwrap_or(false)
            && !path.to_string_lossy().ends_with("wire-archive.jsonl")
        {
            out.push(path);
        }
    }
    Ok(())
}

/// 目录级 wire blob 治理（离线命令；热路径零改动）。
///
/// 分层保留：骨架事件永久保留；wire_rendered 全文按保留期窗口三段处理——
/// `wire_blob_days` 内留 journal 正文，超窗入旁路归档件
/// （`{session}.wire-archive.jsonl`，同 encode 口径自描述行），超
/// `expire_days` 过期删除（journal 落 `wire_blob_expired` 标记事件，
/// 降级可见不留静默空洞）。并发安全=活跃写者注册表锁拒绝活跃会话
/// （fail-visible）；原子性=临时文件+rename；行重写走裸 JSON 操作
/// （保留未知键，非 typed 全量重编）；幂等（重跑无二次副作用）。
pub fn gc_wire_blobs(
    dir: &Path,
    now_ms: u64,
    wire_blob_days: u64,
    expire_days: u64,
) -> Result<GcReport, JournalError> {
    let wire_blob_ms = wire_blob_days * 86_400_000;
    let expire_ms = expire_days * 86_400_000;
    let mut stats = GcReport::default();
    let mut jsonls: Vec<PathBuf> = Vec::new();
    collect_journal_jsonl(dir, &mut jsonls)?;
    jsonls.sort();
    for path in &jsonls {
        stats.files_scanned += 1;
        let stem = path
            .file_stem()
            .map(|st| st.to_string_lossy().to_string())
            .unwrap_or_default();
        {
            let active = ACTIVE_WRITERS
                .lock()
                .map(|g| g.as_ref().map_or(false, |set| set.contains(&stem)))
                .unwrap_or(false);
            if active {
                return Err(JournalError::WriterActive(stem));
            }
        }
        let content = std::fs::read_to_string(path).map_err(JournalError::Io)?;
        stats.bytes_before += content.len() as u64;
        let sidecar_path = path.with_extension("wire-archive.jsonl");
        let mut modified = false;
        let mut out_lines: Vec<String> = Vec::new();
        let mut archive_lines: Vec<String> = Vec::new();
        let mut expired_infos: Vec<(u64, String, usize)> = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut obj: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => {
                    // 非 journal schema 的行（他种账本）原样保留不治理
                    out_lines.push(line.to_string());
                    continue;
                }
            };
            // 事件字段在 "payload" 包裹层(真实 journal 形态);顶层兜底
            // (ts 在顶层,先取出避免借用冲突)
            let line_ts = obj.get("ts").and_then(|v| v.as_u64()).unwrap_or(0);
            let payload_face = match obj.get_mut("payload") {
                Some(v) => v.as_object_mut(),
                None => obj.as_object_mut(),
            };
            let Some(face) = payload_face else {
                out_lines.push(obj.to_string());
                continue;
            };
            let is_wire = face.contains_key("full_text")
                && face.contains_key("content_hash")
                && face.contains_key("round");
            if is_wire {
                let ts = line_ts;
                let full = face
                    .get("full_text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let age = now_ms.saturating_sub(ts);
                if !full.is_empty() && age >= expire_ms {
                    face.insert(
                        "full_text".to_string(),
                        serde_json::Value::String(String::new()),
                    );
                    let round = face.get("round").and_then(|v| v.as_u64()).unwrap_or(0);
                    let hash = face
                        .get("content_hash")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let len = face.get("wire_len").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    expired_infos.push((round, hash, len));
                    stats.wire_expired += 1;
                    modified = true;
                } else if !full.is_empty() && age >= wire_blob_ms {
                    archive_lines.push(line.to_string());
                    face.insert(
                        "full_text".to_string(),
                        serde_json::Value::String(String::new()),
                    );
                    stats.wire_archived += 1;
                    modified = true;
                }
            }
            out_lines.push(obj.to_string());
        }
        if !archive_lines.is_empty() {
            let mut sc = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&sidecar_path)
                .map_err(JournalError::Io)?;
            for l in &archive_lines {
                writeln!(sc, "{l}").map_err(JournalError::Io)?;
            }
        }
        if sidecar_path.exists() {
            let sc_content = std::fs::read_to_string(&sidecar_path).map_err(JournalError::Io)?;
            let mut kept: Vec<String> = Vec::new();
            for l in sc_content.lines() {
                if l.trim().is_empty() {
                    continue;
                }
                let obj: serde_json::Value = match serde_json::from_str(l) {
                    Ok(v) => v,
                    Err(_) => {
                        kept.push(l.to_string());
                        continue;
                    }
                };
                let ts = obj.get("ts").and_then(|v| v.as_u64()).unwrap_or(0);
                if now_ms.saturating_sub(ts) >= expire_ms {
                    let round = obj.get("round").and_then(|v| v.as_u64()).unwrap_or(0);
                    let hash = obj
                        .get("content_hash")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let len = obj.get("wire_len").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    expired_infos.push((round, hash, len));
                    stats.wire_expired += 1;
                } else {
                    kept.push(l.to_string());
                }
            }
            let orig_count = sc_content.lines().count();
            if kept.len() != orig_count {
                let mut joined = kept.join("\n");
                if !joined.is_empty() {
                    joined.push('\n');
                }
                std::fs::write(&sidecar_path, joined).map_err(JournalError::Io)?;
                modified = true;
            }
        }
        if modified {
            let tmp = path.with_extension("jsonl.gc-tmp");
            let mut joined = out_lines.join("\n");
            if !joined.is_empty() {
                joined.push('\n');
            }
            std::fs::write(&tmp, joined).map_err(JournalError::Io)?;
            std::fs::rename(&tmp, path).map_err(JournalError::Io)?;
        }
        stats.bytes_after += std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if !expired_infos.is_empty() {
            let w = JournalWriter::open(path.parent().unwrap_or(Path::new(".")), &stem)?;
            for (round, hash, len) in &expired_infos {
                w.wire_blob_expired(*round, hash, *len)?;
            }
        }
    }
    Ok(stats)
}

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

    #[test]
    fn test_unclean_tail_appends_crash_marker() {
        // 尾部悬挂检测:上一进程死前 turn 未收尾(直写 turn_started,无配对
        // turn_ended,不触轮守卫)→ 打开即补写崩溃标记,seq 顺延
        let dir = std::env::temp_dir().join(format!("jf-hang-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let w = JournalWriter::open(&dir, "s-hang").unwrap();
            w.push(JournalEvent::TurnStarted {
                turn_seq: 1,
                goal: "g1".into(),
            })
            .unwrap();
        }
        {
            // 重开(崩溃检测补写点;上一写者已 drop、注册表已释放)
            let _w = JournalWriter::open(&dir, "s-hang").unwrap();
        }
        let lines = read_all(&JournalWriter::path_for(&dir, "s-hang")).unwrap();
        assert_eq!(lines.len(), 2, "turn_started + 补写的 crash 标记");
        assert_eq!(lines[1].seq, 2);
        match &lines[1].event {
            JournalEvent::SessionCrashed { reason } => {
                assert_eq!(reason, "unclean_tail");
            }
            other => panic!("expected session_crashed, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_crash_marker_not_duplicated_on_reopen() {
        // 重复打开已补写崩溃标记的悬挂 journal:不二次补写(幂等)
        let dir = std::env::temp_dir().join(format!("jf-hang2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let w = JournalWriter::open(&dir, "s-hang2").unwrap();
            w.push(JournalEvent::TurnStarted {
                turn_seq: 1,
                goal: "g1".into(),
            })
            .unwrap();
        }
        let first = {
            // 首次重开:补写崩溃标记
            let _w = JournalWriter::open(&dir, "s-hang2").unwrap();
            read_all(&JournalWriter::path_for(&dir, "s-hang2")).unwrap()
        };
        {
            // 再次打开:末事件已是崩溃标记,不二次补写
            let _w2 = JournalWriter::open(&dir, "s-hang2").unwrap();
        }
        let second = read_all(&JournalWriter::path_for(&dir, "s-hang2")).unwrap();
        assert_eq!(first.len(), second.len(), "重复打开不得追加新事件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_clean_tail_open_appends_nothing() {
        // 正常收尾(turn 成对)后打开:零新事件(负例)
        let dir = std::env::temp_dir().join(format!("jf-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let w = JournalWriter::open(&dir, "s-clean").unwrap();
            w.begin_turn("g1").unwrap();
            w.end_turn("ok", 1, 10).unwrap();
        }
        let before = read_all(&JournalWriter::path_for(&dir, "s-clean")).unwrap();
        {
            let _w = JournalWriter::open(&dir, "s-clean").unwrap();
        }
        let after = read_all(&JournalWriter::path_for(&dir, "s-clean")).unwrap();
        assert_eq!(before.len(), after.len(), "干净尾部打开不得追加事件");
        assert_eq!(after.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_checkpoint_inline_and_blob_paths() {
        // 粒级检查点:内联阈值分岔——小结果直存 inline,大结果 inline 缺省+
        // checkpoint_blob 全文载体随行落账;读回 hash 一致
        let dir = std::env::temp_dir().join(format!("jf-ckpt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let big = "x".repeat(CHECKPOINT_INLINE_LIMIT + 1);
        {
            let w = JournalWriter::open(&dir, "s-ckpt").unwrap();
            let (seq1, blob1) = w
                .node_checkpointed("wf", "planhash-small", "n1", "completed", "ok")
                .unwrap();
            assert!(blob1.is_none(), "小结果零 blob");
            let (seq2, blob2) = w
                .node_checkpointed("wf", "planhash-big", "n2", "completed", &big)
                .unwrap();
            assert!(blob2.is_some(), "大结果必带 blob 载体");
            let (seq3, _) = w
                .node_checkpointed("wf", "planhash-small", "n3", "skipped", "")
                .unwrap();
            assert!(seq3 > seq1 && seq2 > seq1);
        }
        let lines = read_all(&JournalWriter::path_for(&dir, "s-ckpt")).unwrap();
        assert_eq!(lines.len(), 4, "3 条检查点 + 1 条 blob 载体");
        match &lines[0].event {
            JournalEvent::NodeCheckpointed {
                node_id,
                status,
                result_ref,
                ..
            } => {
                assert_eq!(node_id, "n1");
                assert_eq!(status, "completed");
                assert_eq!(result_ref.inline.as_deref(), Some("ok"));
                assert_eq!(result_ref.hash, evorule_digest("ok"));
                assert_eq!(result_ref.len, 2);
            }
            other => panic!("expected node_checkpointed, got {other:?}"),
        }
        match &lines[1].event {
            JournalEvent::NodeCheckpointed {
                node_id,
                result_ref,
                ..
            } => {
                assert_eq!(node_id, "n2");
                assert!(result_ref.inline.is_none(), "超限结果不内联");
                assert_eq!(result_ref.len, big.len());
            }
            other => panic!("expected node_checkpointed, got {other:?}"),
        }
        match &lines[2].event {
            JournalEvent::CheckpointBlob {
                node_id,
                hash,
                len,
                full_text,
                ..
            } => {
                assert_eq!(node_id, "n2");
                assert_eq!(full_text, &big);
                assert_eq!(hash, &evorule_digest(&big));
                assert_eq!(*len, big.len());
            }
            other => panic!("expected checkpoint_blob, got {other:?}"),
        }
        match &lines[3].event {
            JournalEvent::NodeCheckpointed {
                node_id,
                status,
                result_ref,
                ..
            } => {
                assert_eq!(node_id, "n3");
                assert_eq!(status, "skipped");
                assert_eq!(result_ref.len, 0, "跳过节点=零长引用");
            }
            other => panic!("expected node_checkpointed, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_session_resumed_and_tool_retried_roundtrip() {
        // 恢复标记+工具重试观测事件:落账+读回
        let dir = std::env::temp_dir().join(format!("jf-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let w = JournalWriter::open(&dir, "s-resume").unwrap();
            w.session_resumed(
                7,
                vec!["messages".to_string(), "pending_approvals".to_string()],
            )
            .unwrap();
            w.tool_retried("grep_files", 1, true).unwrap();
            w.tool_retried("shell_exec", 1, false).unwrap();
        }
        let lines = read_all(&JournalWriter::path_for(&dir, "s-resume")).unwrap();
        assert_eq!(lines.len(), 3);
        match &lines[0].event {
            JournalEvent::SessionResumed {
                replay_seq,
                rebuilt,
            } => {
                assert_eq!(*replay_seq, 7);
                assert_eq!(rebuilt.len(), 2);
            }
            other => panic!("expected session_resumed, got {other:?}"),
        }
        match &lines[1].event {
            JournalEvent::ToolRetried {
                tool,
                attempt,
                transient,
            } => {
                assert_eq!(tool, "grep_files");
                assert_eq!(*attempt, 1);
                assert!(*transient);
            }
            other => panic!("expected tool_retried, got {other:?}"),
        }
        match &lines[2].event {
            JournalEvent::ToolRetried {
                tool,
                attempt,
                transient,
            } => {
                assert_eq!(tool, "shell_exec");
                assert_eq!(*attempt, 1);
                assert!(!*transient);
            }
            other => panic!("expected tool_retried, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_recall_set_event_roundtrip() {
        // 检索质量观测批 K-11:事件落账+读回(观测级)
        let dir = std::env::temp_dir().join(format!("jf-recall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let w = JournalWriter::open(&dir, "s-recall").unwrap();
            w.recall_set(
                "s-recall",
                vec![
                    "stable@1:k.a".to_string(),
                    "summaries@1:s1".to_string(),
                    "events@1:e1".to_string(),
                ],
            )
            .unwrap();
        }
        let lines = read_all(&JournalWriter::path_for(&dir, "s-recall")).unwrap();
        let mut seen = Vec::new();
        for l in lines {
            if let JournalEvent::RecallSet { session, hits } = l.event {
                seen.push((session, hits));
            }
        }
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "s-recall");
        assert_eq!(seen[0].1.len(), 3);
        assert_eq!(seen[0].1[0], "stable@1:k.a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_lex_cache_stats_event_roundtrip() {
        // 补齐路线图 P2-1:LexStore 缓存观测三计数落账+读回(观测级,
        // recall_set 同族 best-effort)
        let dir = std::env::temp_dir().join(format!("jf-lexstats-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let w = JournalWriter::open(&dir, "s-lexstats").unwrap();
            w.lex_cache_stats("s-lexstats", 7, 2, 1).unwrap();
        }
        let lines = read_all(&JournalWriter::path_for(&dir, "s-lexstats")).unwrap();
        let mut seen = Vec::new();
        for l in lines {
            if let JournalEvent::LexCacheStats {
                session,
                hit,
                expired,
                fetch,
            } = l.event
            {
                seen.push((session, hit, expired, fetch));
            }
        }
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], ("s-lexstats".to_string(), 7, 2, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_gc_wire_blobs_tiered_retention() {
        // 规格修正批批次四验收:构造含过期/中窗/新窗 wire_rendered 的 journal
        // → GC → 过期全文出账+expired 事件在账+中窗入旁路件+新窗不动
        // +骨架事件逐字节不动+seq 连续(read_all 过)
        use crate::agent::journal::gc_wire_blobs;
        let dir = std::env::temp_dir().join(format!("journal-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let now = 1_800_000_000_000u64; // 固定"当前"毫秒
        let day = 86_400_000u64;
        // 手工构造 journal(带回溯时间戳):turn_started + 3 条 wire_rendered
        // (过期 100d/中窗 30d/新窗 1d)+ skeleton 行
        let mk_wire = |seq: u64, ts: u64, round: u64, text: &str| {
            format!(
                "{{\"seq\":{seq},\"ts\":{ts},\"type\":\"wire_rendered\",\"payload\":{{\"round\":{round},\"wire_len\":{},\"content_hash\":\"hash-{round}\",\"full_text\":\"{text}\"}}}}",
                text.len()
            )
        };
        let lines = vec![
            format!("{{\"seq\":1,\"ts\":{},\"type\":\"turn_started\",\"payload\":{{\"turn_seq\":1,\"goal\":\"g\"}}}}", now - 100 * day),
            mk_wire(2, now - 100 * day, 1, "过期全文"),
            mk_wire(3, now - 30 * day, 2, "中窗全文"),
            mk_wire(4, now - 1 * day, 3, "新窗全文"),
        ];
        std::fs::write(dir.join("s-gc.jsonl"), lines.join("\n") + "\n").unwrap();
        // GC:7d 入旁路,90d 过期
        let stats = gc_wire_blobs(&dir, now, 7, 90).unwrap();
        assert_eq!(stats.files_scanned, 1);
        assert_eq!(stats.wire_archived, 1, "中窗入旁路");
        assert_eq!(stats.wire_expired, 1, "过期删除+标记");
        // journal:过期/中窗行 full_text 清空;新窗保留;expired 标记在账
        let after = read_all(&JournalWriter::path_for(&dir, "s-gc")).unwrap();
        let mut wire_texts = Vec::new();
        let mut expired_n = 0;
        let mut skeleton_ok = false;
        for l in &after {
            match &l.event {
                JournalEvent::WireRendered {
                    round,
                    full_text,
                    content_hash,
                    ..
                } => {
                    if *round == 3 {
                        assert!(full_text.starts_with("新窗全文"), "新窗不动");
                    } else {
                        assert!(full_text.is_empty(), "超窗全文应出 journal");
                    }
                    assert_eq!(content_hash, &format!("hash-{round}"), "hash 校验凭证保持");
                    wire_texts.push(*round);
                }
                JournalEvent::WireBlobExpired { round, .. } => {
                    expired_n += 1;
                    assert_eq!(*round, 1, "仅过期窗出标记");
                }
                JournalEvent::TurnStarted { goal, .. } => {
                    skeleton_ok = goal == "g";
                }
                _ => {}
            }
        }
        assert_eq!(wire_texts.len(), 3);
        assert_eq!(expired_n, 1);
        assert!(skeleton_ok, "骨架事件逐字节不动");
        // 旁路件:中窗全文在档(可回读),过期全文不在
        let sidecar = std::fs::read_to_string(dir.join("s-gc.wire-archive.jsonl")).unwrap();
        assert!(sidecar.contains("中窗全文"));
        assert!(!sidecar.contains("过期全文"));
        // 幂等:重跑无二次副作用
        let stats2 = gc_wire_blobs(&dir, now, 7, 90).unwrap();
        assert_eq!(stats2.wire_archived, 0);
        assert_eq!(stats2.wire_expired, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_summary_fidelity_scan_event_roundtrip() {
        // 规格修正批交付物 B:事件落账+读回(验收例 2 账面半边)
        let dir = std::env::temp_dir().join(format!("jf-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let w = JournalWriter::open(&dir, "s-scan").unwrap();
            w.summary_fidelity_scan("s-scan", 4, 6, 1, 0.1667, false)
                .unwrap();
            w.summary_fidelity_scan("s-scan", 2, 0, 0, 0.0, true)
                .unwrap();
        }
        let lines = read_all(&JournalWriter::path_for(&dir, "s-scan")).unwrap();
        let mut seen = Vec::new();
        for l in lines {
            if let JournalEvent::SummaryFidelityScan {
                session,
                trimmed_n,
                anchors_n,
                hit_n,
                ratio,
                summary_empty,
            } = l.event
            {
                seen.push((session, trimmed_n, anchors_n, hit_n, ratio, summary_empty));
            }
        }
        assert_eq!(
            seen,
            vec![
                ("s-scan".to_string(), 4, 6, 1, 0.1667, false),
                ("s-scan".to_string(), 2, 0, 0, 0.0, true),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;
    use JournalEvent as JE;

    #[test]
    fn roundtrip_all_14_events() {
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
                knowledge_candidates: 1,
                flushed_events: 0,
            },
            JE::WireRendered {
                round: 1,
                wire_len: 9,
                content_hash: evorule_digest("wire body"),
                full_text: "wire body".into(),
            },
            JE::I2ScanReport {
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
            JE::LexCacheStats {
                session: "s-lex".into(),
                hit: 7,
                expired: 2,
                fetch: 1,
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
    fn test_writer_active_registry_blocks_double_open_then_releases() {
        let dir = tempfile::tempdir().unwrap();
        let w = JournalWriter::open(dir.path(), "reg-test").unwrap();
        // 同会话第二写者 fail-fast(并发双写会使 seq 交错损坏账面)
        assert!(matches!(
            JournalWriter::open(dir.path(), "reg-test"),
            Err(JournalError::WriterActive(_))
        ));
        // 最后一个写者 Drop → 占位释放,可重开(续跑场景)
        drop(w);
        assert!(JournalWriter::open(dir.path(), "reg-test").is_ok());
    }

    #[test]
    fn turn_guard_explicit_end_writes_once() {
        let dir = tempfile::tempdir().unwrap();
        let w = JournalWriter::open(dir.path(), "s-explicit").unwrap();
        {
            let g = w.begin_turn("goal").unwrap();
            assert!(w.turn_open());
            g.end("cancelled", 4, 500);
        }
        assert!(!w.turn_open());
        let lines = read_all(&JournalWriter::path_for(dir.path(), "s-explicit")).unwrap();
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
        let w = JournalWriter::open(dir.path(), "s-guard-drop").unwrap();
        {
            let _g = w.begin_turn("goal").unwrap();
            // 不显式 end,直接 drop(模拟 yield Err 早退/panic)
        }
        let lines = read_all(&JournalWriter::path_for(dir.path(), "s-guard-drop")).unwrap();
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

    #[test]
    fn delegate_spawned_event_order_and_fields() {
        // 委托锚事件:写入序 tool_invoked → delegate_spawned → tool_result,
        // 读回序一致且字段保真(seq 单调)
        let dir = tempfile::tempdir().unwrap();
        let w = JournalWriter::open(dir.path(), "s-spawn").unwrap();
        let call_id = w
            .tool_invoked(
                "delegate",
                &serde_json::json!({"agent_type": "researcher", "task": "do research"}),
                None,
            )
            .unwrap();
        let spawn_seq = w
            .delegate_spawned("child-9", "researcher", 1, &evorule_digest("do research"))
            .unwrap();
        let result_seq = w.tool_result(&call_id, "ok", "done").unwrap();
        assert!(
            spawn_seq < result_seq,
            "事件序 delegate_spawned({spawn_seq}) < tool_result({result_seq})"
        );
        let lines = read_all(&JournalWriter::path_for(dir.path(), "s-spawn")).unwrap();
        assert_eq!(lines.len(), 3);
        assert!(
            matches!(lines[0].event, JournalEvent::ToolInvoked { .. }),
            "首事件=tool_invoked"
        );
        match &lines[1].event {
            JournalEvent::DelegateSpawned {
                child_session_id,
                agent_type,
                depth,
                task_digest,
            } => {
                assert_eq!(child_session_id, "child-9");
                assert_eq!(agent_type, "researcher");
                assert_eq!(*depth, 1);
                assert_eq!(task_digest, &evorule_digest("do research"));
            }
            other => panic!("expected delegate_spawned, got {other:?}"),
        }
        assert!(
            matches!(&lines[2].event, JournalEvent::ToolResult { call_id: c, status, .. } if *c == call_id && status == "ok"),
            "尾事件=配对 tool_result"
        );
    }

    #[test]
    fn scan_delegate_spawns_finds_only_spawn_events_in_order() {
        // 发现面:混合事件流只取委托锚,seq 升序;空流=空清单
        let dir = tempfile::tempdir().unwrap();
        let w = JournalWriter::open(dir.path(), "s-scan-spawn").unwrap();
        let _guard = w.begin_turn("g").unwrap();
        w.delegate_spawned("c1", "planner", 1, &evorule_digest("t1"))
            .unwrap();
        w.llm_called_react("m", None, None, Some(10), 2, "resp")
            .unwrap();
        w.delegate_spawned("c2", "worker", 1, &evorule_digest("t2"))
            .unwrap();
        w.end_turn("success", 3, 100).unwrap();
        let lines = read_all(&JournalWriter::path_for(dir.path(), "s-scan-spawn")).unwrap();
        let found = scan_delegate_spawns(&lines);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].child_session_id, "c1");
        assert_eq!(found[0].agent_type, "planner");
        assert_eq!(found[1].child_session_id, "c2");
        assert!(found[0].seq < found[1].seq, "按 seq 升序");
        assert_eq!(scan_delegate_spawns(&[]).len(), 0, "空流=空清单");
    }
}
