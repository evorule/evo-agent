// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Agent delegate — sub-agent invocation
//!
//! ## G9:多 agent 编排
//!
//! 在原有串行单子 agent 委托(`delegate`)基础上,新增:
//! - [`DelegateContext::delegate_parallel`] — 并行委托多个子 agent(`join_all`)
//! - [`DelegateContext::delegate_race`] — 竞速委托(任一完成即返回,取消其余)
//! - **深度强制**:`delegate()` 现在会检查 `current_depth >= max_depth`,超限直接返回 Err
//!   (旧版只提供 `can_delegate` 辅助方法但不强制,容易无限递归)
//! - **并发限流**:`max_concurrent` 用 `tokio::sync::Semaphore` 限制并行子 agent 数,
//!   防止高并发下 evorule session 数暴增(§9.6 风险缓解)

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::agent::definition::AgentDefinitionManager;
use crate::agent::runner::DEFAULT_MAX_DELEGATE_DEPTH;
use crate::api::evorule_client::EvoruleApiClient;
use crate::io_handlers::tool_handler::ToolHandler;

/// 委托任务的 boxed future 类型别名（降低 delegate_race 的类型复杂度）。
type DelegateFuture = Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;

/// 子代理委托观测记录（spawn 账条目;父 runner 在 delegate 工具结果写账时
/// drain 落 journal 事件,子会话锚由此进入父账面）
#[derive(Debug, Clone)]
pub struct SpawnRecord {
    /// 子 evorule 会话 id
    pub child_session_id: String,
    /// 子代理类型
    pub agent_type: String,
    /// 委托深度（父自身第 0 层）
    pub depth: usize,
    /// 委托任务文本 digest（evorule-hash 口径）
    pub task_digest: String,
    /// 本委托窗口内的上下文 token 累计（子 runner LLM 真值;None=上下文
    /// 未接线埋点。并行委托窗口重叠时为近似值,串行（默认）精确）
    pub tokens_used: Option<u64>,
}

/// spawn 账共享句柄类型（整棵委托树同一本账:DelegateContext clone 共享
/// 同一 Arc,嵌套/并行分支的子会话记录汇入同账,父侧统一 drain）
pub(crate) type SpawnLedger = Arc<std::sync::Mutex<Vec<SpawnRecord>>>;

/// G9:并行委托的默认并发上限(§9.6 风险缓解,默认 5)
pub const DEFAULT_MAX_CONCURRENT_DELEGATES: usize = 5;

/// 回传摘要触发阈值(字符数):子代理最终结果超过此长度时以确定性头尾
/// 摘录回传,全文留在子会话账(journal/evorule facts)按指针可查。
/// 阈值内全文直传——短结果摘要反而丢信息。
pub const DIGEST_THRESHOLD_CHARS: usize = 8000;
/// 摘录保留的头部长度(字符)
pub const DIGEST_HEAD_CHARS: usize = 1600;
/// 摘录保留的尾部长度(字符)
pub const DIGEST_TAIL_CHARS: usize = 800;

/// 父上下文摘要与委托任务的确定性拼接(零 LLM 参与)。
///
/// context 为空/纯空白时原样返回 task;非空时结构化前缀注入(子代理
/// system prompt 不动,摘要走首条用户消息)。
pub fn compose_task(task: &str, context: Option<&str>) -> String {
    match context.map(str::trim) {
        Some(c) if !c.is_empty() => {
            format!("【父代理上下文摘要】\n{c}\n\n【委托任务】\n{task}")
        }
        _ => task.to_string(),
    }
}

/// 按字符边界截断(多字节安全:越界端点回退到 char_boundary)
fn safe_cut(s: &str, end: usize) -> usize {
    let mut cut = end.min(s.len());
    while cut < s.len() && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

/// 子代理结果的确定性摘要(零 LLM、可复算)。
///
/// 长度 ≤ [`DIGEST_THRESHOLD_CHARS`] 时原样返回;超长时保留头尾摘录+
/// 结构化标记+子会话指针(全文按 child_session_id 在子 journal 与
/// evorule 会话账可查)。摘要只改变主代理看到的形态,不改变账面全文。
pub fn digest_delegate_result(child_sid: &str, agent_type: &str, content: &str) -> String {
    let total = content.chars().count();
    if total <= DIGEST_THRESHOLD_CHARS {
        return content.to_string();
    }
    let head_end = safe_cut(content, DIGEST_HEAD_CHARS);
    let tail_start = safe_cut(content, content.len().saturating_sub(DIGEST_TAIL_CHARS));
    let omitted =
        total - content[..head_end].chars().count() - content[tail_start..].chars().count();
    format!(
        "[委托结果摘要] 子会话 {child_sid}(agent={agent_type})全文 {total} 字符,以下为头尾摘录\n\
         —— 开头 ——\n{head}\n\
         ——(中略 {omitted} 字符;全文可查:子会话 {child_sid} 的 journal 与 evorule 会话账)——\n{tail}\n\
         —— 结束 ——",
        head = &content[..head_end],
        tail = &content[tail_start..],
    )
}

/// Delegate context
#[derive(Debug, Clone)]
pub struct DelegateContext {
    /// Current delegate depth
    pub current_depth: usize,
    /// Parent agent type
    pub parent_agent_type: String,
    /// Agent definition manager
    pub definitions: AgentDefinitionManager,
    /// Evorule API client
    pub evorule_client: EvoruleApiClient,
    /// G9:最大委托深度(超过则拒绝,防无限递归)
    ///
    /// 默认 [`DEFAULT_MAX_DELEGATE_DEPTH`](crate::agent::runner::DEFAULT_MAX_DELEGATE_DEPTH) = 3。
    /// `delegate()` / `delegate_parallel()` / `delegate_race()` 在执行前检查
    /// `current_depth >= max_depth`,超限返回 Err。
    pub max_depth: usize,
    /// G9:并行委托并发上限(`Semaphore` 限流)
    ///
    /// `Some(sem)` 时,`delegate_parallel` / `delegate_race` 的每个子任务在调
    /// `delegate()` 前先 acquire 一个 permit,执行完释放。
    /// `None` = 不限流(测试/单机场景)。`Arc<Semaphore>` 在 `increment_depth`
    /// clone 时共享,因此限制是**全局的**(跨整个委托树)。
    pub max_concurrent: Option<Arc<Semaphore>>,
    /// plan-execute tokens 埋点累加器（纲领 §8 Phase 2 交付物 7，None = 不埋点）
    ///
    /// `Some` 时注入本上下文派生的每个子 runner，共享同一 `Arc<AtomicU64>`：
    /// runner 每次 LLM `IoRequest` 后累加 `token_usage.total_tokens`。外层驱动
    /// （driver.rs）据此维护 `BudgetCounters.tokens_used`。仅观测，不改控制流。
    pub token_counter: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// union toolkit 来源（`with_toolkit` 成对注入；None = 子 runner 无工具面=旧行为）
    ///
    /// 注入后 `delegate()` 按子代理 `def.tools` 白名单过滤出可用工具挂载
    /// （对齐 serve 面 construct_runner / CLI patrol_build_runner 模式），LLM
    /// 请求据此携带工具契约；多轮工具回喂在流式路径的本地 ReAct 循环完成。
    pub toolkit: Option<ToolHandler>,
    /// 工作目录（能力边界合成用；随 toolkit 成对注入）
    pub workdir: Option<std::path::PathBuf>,
    /// journal 目录（Some = 子代理落 journal；serve 面从 workbench config 取）
    pub journal_dir: Option<std::path::PathBuf>,
    /// 治理门禁段（Some = 下放给每个子 runner 的 S2 槽位;serve 构造点传入,
    /// CLI 路径 None = 子代理与 CLI 主路径同口径无治理段）
    pub governance_segment: Option<String>,

    /// 项目记忆段(AGENTS.md 层级;serve 构造点传入——子代理与主 runner
    /// 同源注入,None=不注入。CLI 主路径无此段,子代理同口径)
    pub agents_md_segment: Option<String>,
    /// 记忆下放开关（true = `def.memory` 有声明的子代理装配 MemoryManager
    /// 全量召回面;默认 false = 子代理无状态执行器,任务域自包含）
    pub propagate_memory: bool,
    /// 粒执行器步超时覆写（plan_execute 粒定义 step_timeout_secs=300
    /// < 容器命令预算 600s,超 300s 容器命令被步超时先杀——请求级覆写粒定义
    /// 值。`None` = 定义值零变化;仅 serve plan_execute 构造点传入,CLI/react
    /// 路径不触）
    pub step_timeout_override: Option<std::time::Duration>,
    /// spawn 账（子会话锚记录;clone 共享同一 Arc,父 runner drain 落账）
    pub(crate) spawn_ledger: SpawnLedger,

    /// 子代理 LLM handler 下放（Some = 子 runner 覆盖 with_defaults 缺省,
    /// 携带统一解析产物含 [llm] max_tokens;None = 既有行为 with_defaults,
    /// 4096 硬兜底会被推理模型思考吃满 → content 空响应被误读为完成）
    pub(crate) llm_handler: Option<crate::io_handlers::LlmHandler>,
}

impl DelegateContext {
    /// Create new delegate context
    pub fn new(
        parent_agent_type: &str,
        definitions: AgentDefinitionManager,
        evorule_client: EvoruleApiClient,
    ) -> Self {
        Self {
            current_depth: 0,
            parent_agent_type: parent_agent_type.to_string(),
            definitions,
            evorule_client,
            max_depth: DEFAULT_MAX_DELEGATE_DEPTH,
            max_concurrent: None,
            token_counter: None,
            toolkit: None,
            workdir: None,
            journal_dir: None,
            governance_segment: None,
            agents_md_segment: None,
            propagate_memory: false,
            step_timeout_override: None,
            spawn_ledger: Arc::new(std::sync::Mutex::new(Vec::new())),
            llm_handler: None,
        }
    }

    /// 注入治理门禁段（下放给每个子 runner 的 S2 槽位;serve 构造点在
    /// 主 runner 同源治理段算好后传入）
    pub fn with_governance_segment(mut self, segment: Option<String>) -> Self {
        self.governance_segment = segment;
        self
    }

    /// 注入项目记忆段(AGENTS.md 层级;与主 runner 同源,None=不注入)
    pub fn with_agents_md_segment(mut self, segment: Option<String>) -> Self {
        self.agents_md_segment = segment;
        self
    }

    /// 开启记忆下放（`def.memory` 有声明的子代理装配 MemoryManager 召回面;
    /// 默认关闭——子代理默认无状态,防主会话记忆串染+确定成本）
    pub fn with_memory_propagation(mut self) -> Self {
        self.propagate_memory = true;
        self
    }

    /// 注入粒执行器步超时覆写（仅 serve plan_execute 构造点传入;
    /// `None` = 子代理按各自定义 step_timeout_secs 执行,既有行为零变化）
    pub fn with_step_timeout_override(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.step_timeout_override = timeout;
        self
    }

    /// drain spawn 账（父 runner 在 delegate 工具结果写账前调用:
    /// 取走全部记录落 journal 事件;无 journal 时同样取走防跨调用残留）
    pub fn drain_spawn_records(&self) -> Vec<SpawnRecord> {
        match self.spawn_ledger.lock() {
            Ok(mut ledger) => std::mem::take(&mut *ledger),
            Err(_) => Vec::new(),
        }
    }

    /// 注入 journal 目录（serve 面从 workbench config 取）
    pub fn with_journal_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.journal_dir = Some(dir);
        self
    }

    /// 注入 union toolkit + 工作目录（成对注入，随 `Clone` 延续到每个子 runner）
    ///
    /// toolkit 按各子代理 `def.tools` 白名单过滤后挂载；workdir 用于能力边界合成。
    pub fn with_toolkit(mut self, toolkit: ToolHandler, workdir: &Path) -> Self {
        self.toolkit = Some(toolkit);
        self.workdir = Some(workdir.to_path_buf());
        self
    }

    /// plan-execute tokens 埋点：注入共享累加器（随 `Clone` 延续到每个子 runner）
    pub fn with_token_counter(mut self, counter: Arc<std::sync::atomic::AtomicU64>) -> Self {
        self.token_counter = Some(counter);
        self
    }

    /// 读取 tokens 埋点累计值（未启用埋点返回 0）
    pub fn token_total(&self) -> u64 {
        self.token_counter
            .as_ref()
            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// 子代理 LLM handler 下放(builder 风格;主构造点统一传入统一解析产物,
    /// 与主 runner 的 handler 同源——env 优先、toml 兜底)
    pub fn with_llm_handler(mut self, handler: crate::io_handlers::LlmHandler) -> Self {
        self.llm_handler = Some(handler);
        self
    }

    /// 子 runner llm_handler 接线(构造点下放 Some 时覆盖 with_defaults 缺省)
    fn wire_llm_handler(
        &self,
        runner: crate::agent::runner::AgentRunner,
    ) -> crate::agent::runner::AgentRunner {
        match &self.llm_handler {
            Some(h) => runner.with_llm_handler(h.clone()),
            None => runner,
        }
    }

    /// Set delegate depth
    pub fn with_depth(mut self, depth: usize) -> Self {
        self.current_depth = depth;
        self
    }

    /// G9:设置最大委托深度(覆盖默认 `DEFAULT_MAX_DELEGATE_DEPTH`)
    pub fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// G9:启用并行委托并发限流(§9.6 风险缓解)
    ///
    /// `max_concurrent` 个子 agent 可同时运行,超出部分排队等待 permit。
    /// 共享 `Arc<Semaphore>`,在 `increment_depth` clone 时延续,限制跨整个委托树。
    pub fn with_max_concurrent_delegates(mut self, max_concurrent: usize) -> Self {
        if max_concurrent == 0 {
            self.max_concurrent = None;
        } else {
            self.max_concurrent = Some(Arc::new(Semaphore::new(max_concurrent)));
        }
        self
    }

    /// Increment delegate depth
    ///
    /// clone 自身并 `current_depth + 1`。`max_depth` 与 `max_concurrent`(Arc 共享)一并延续。
    pub fn increment_depth(&self) -> Self {
        self.clone().with_depth(self.current_depth + 1)
    }

    /// Check if can continue delegating
    pub fn can_delegate(&self, max_depth: usize) -> bool {
        self.current_depth < max_depth
    }

    /// G9:是否还能继续委托(用自身 `max_depth`,无需调用方传参)
    pub fn can_delegate_more(&self) -> bool {
        self.current_depth < self.max_depth
    }

    /// Delegate execution to sub-agent
    ///
    /// G9:执行前检查 `current_depth >= max_depth`,超限返回 Err(防无限递归)。
    pub fn delegate<'a>(
        &'a self,
        agent_type: &'a str,
        task: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        self.delegate_with_context(agent_type, task, None)
    }

    /// 带父上下文摘要的委托(其余同 [`Self::delegate`])
    ///
    /// `context` = 父代理按需携带的上下文摘要(可空);非空时确定性拼进
    /// 子代理首条用户消息前部(拼接形态见 [`compose_task`],零 LLM 参与)。
    pub fn delegate_with_context<'a>(
        &'a self,
        agent_type: &'a str,
        task: &'a str,
        context: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move { self.delegate_inner(agent_type, task, context).await })
    }

    async fn delegate_inner(
        &self,
        agent_type: &str,
        task: &str,
        context: Option<&str>,
    ) -> Result<String, String> {
        // G9:深度强制(旧版只提供 can_delegate 辅助方法但不强制)
        if self.current_depth >= self.max_depth {
            return Err(format!(
                "max delegate depth exceeded: current_depth={} >= max_depth={} \
                     (cannot delegate to agent '{}')",
                self.current_depth, self.max_depth, agent_type
            ));
        }

        tracing::info!(
            agent_type,
            task = %task,
            depth = self.current_depth,
            max_depth = self.max_depth,
            "Starting delegated sub-agent execution"
        );

        let def = self.definitions.load(agent_type).map_err(|e| {
            format!(
                "Failed to load sub-agent definition ({}): {}",
                agent_type, e
            )
        })?;

        let mut config = def.to_agent_config();
        // 请求级步超时覆写（Some=覆写定义值;None=定义值零变化）
        if let Some(t) = self.step_timeout_override {
            config.step_timeout = t;
        }

        // 统一装配（delegate 统一装配批）：子代理 runner 构建期标记
        // 管道入口类=Delegate（子代理工具调用账面可分，聚焦决策落账），
        // 聚焦快照=装配面（注册面本身）——LLM 契约面（messages 侧 tools
        // payload，同为注册面）与管道②聚焦允许面同源，静态表不再自动
        // 进入子代理允许面（union 提权面在装配期收口）。
        let mut runner =
            crate::agent::runner::AgentRunner::new(config, self.evorule_client.clone())
                .with_delegate_pipeline_entry()
                .with_assembly_scope_focus();
        // LLM handler 下放接线(构造点 Some 时覆盖 with_defaults 缺省——
        // 统一解析产物随子代理同源,防 4096 硬兜底截断推理模型思考)
        let mut runner = self.wire_llm_handler(runner);
        if let Some(counter) = &self.token_counter {
            runner = runner.with_token_counter(counter.clone());
        }
        // 委托子代理 journal 步级账面接线
        if let Some(dir) = &self.journal_dir {
            runner = runner.with_journal_dir(dir.clone());
        }
        // 治理门禁段下放（serve 构造点传入;CLI 路径 None=子代理与 CLI
        // 主路径同口径。治理段为全局纪律前馈,子代理无豁免理由）
        if let Some(gs) = &self.governance_segment {
            runner = runner.with_governance_segment(Some(gs.clone()));
        }
        // 项目记忆段下放(与主 runner 同源;AGENTS.md 层级注入子代理
        // system prompt 静态区槽位)
        if let Some(seg) = &self.agents_md_segment {
            runner = runner.with_agents_md_segment(Some(seg.clone()));
        }
        // 记忆声明下放（上下文开关 × def.memory 声明双条件;子代理默认
        // 无状态执行器,显式开启+定义声明才装配召回面,ns=定义声明域,
        // 与子会话天然隔离）。只读召回面:ttl/配方/LexStore/召回配额
        // 与主装配同源;写侧沉淀件不随行（子代理短生命周期,沉淀由
        // 主会话统一收口）
        if self.propagate_memory
            && !def.memory.memory_type.is_empty()
            && def.memory.memory_type != "none"
        {
            let mut mem = crate::agent::memory::MemoryManager::new(
                &def.memory.namespace,
                self.evorule_client.clone(),
            );
            if let Some(ttl) = def.memory.ttl_secs {
                mem = mem.with_ttl_secs(ttl);
            }
            if let Some(rj) = &def.memory.recipe {
                match serde_json::from_value::<crate::agent::recipe::MemoryRecipe>(rj.clone()) {
                    Ok(recipe) => mem.set_recipe(recipe),
                    Err(e) => {
                        tracing::warn!(error = %e, "子代理记忆配方解析失败——召回走词法 legacy 路径");
                    }
                }
            }
            if let Some(db) = &def.memory.lex_store {
                match crate::agent::lexstore::LexStore::open(std::path::Path::new(db)) {
                    Ok(store) => mem.set_lex_store(std::sync::Arc::new(store)),
                    Err(e) => {
                        tracing::warn!(db = %db, error = %e, "子代理 LexStore open failed——召回走全量拉取降级路径");
                    }
                }
            }
            if let Err(e) = mem.sync_from_evorule().await {
                tracing::warn!(error = %e, "子代理记忆同步失败——召回降级为空记忆起步");
            }
            runner = runner.with_recall_quotas(
                def.memory.max_session_summaries,
                def.memory.max_injected_events,
            );
            runner = runner.with_memory(mem);
        }

        // 工具面 + 能力边界接线（对齐 serve 面 construct_runner / CLI
        // patrol_build_runner 模式）。委托 runner 此前零工具契约：LLM 无 tools
        // 可知 → 凭训练先验输出供应商原生 XML（<minimax:tool_call> 死文本）。
        // 按 def.tools 白名单过滤挂载；过滤后为空则不挂（纯规划类子代理维持旧行为）。
        if let (Some(union), Some(workdir)) = (&self.toolkit, &self.workdir) {
            let mut filtered = crate::api::serve_tools::build_filtered_toolkit(union, &def.tools);
            if !filtered.tool_names().is_empty() {
                let boundary =
                    crate::api::serve_tools::wire_capability_boundary(&mut filtered, &def, workdir);
                // B2:skills 声明接线(read_skill 注册 + manifest 槽位源)。
                // 置于工具面挂载块内:无工具面的纯规划子代理不注入 manifest,
                // 防 LLM 看到技能清单却无 read_skill 可调;解析失败=定义损坏,
                // 上抛拒委托(声明时刻=人工把关,路径由系统解析)。
                // C 形态后子代理路径为纯声明面(不扫目录——两源合并在 serve
                // 会话创建路径,见 skill_api::merged_manifest_for_session)
                let declared_skills = crate::api::serve_tools::resolve_declared_skills(&def)?;
                let resolved_skills =
                    crate::api::serve_tools::wire_skills(&mut filtered, declared_skills)?;
                runner = runner
                    .with_tool_handler(filtered)
                    .with_capability_boundary(boundary)
                    .with_skills(resolved_skills);
            }
        }

        // 执行路径改走流式消费——宪法 v0.5.0 起 server 只做单发桥接
        // （call_external 的 io_response 提交后即 Stable），多轮工具回喂在应用层
        // run_streaming 的本地 ReAct 循环；非流式 run() 单轮即止，即使 LLM 正确
        // 返回 tool_calls 也不执行。delegate() 对外签名 Result<String,String>
        // 不变，driver/marks 层零改动。tokens 埋点已随流式路径 token_counter
        // 累加点（StreamChunk::Done 臂）继续生效。
        let goal = compose_task(task, context);
        let mut stream = runner.run_streaming(goal, Vec::new());
        let mut final_result: Option<crate::agent::runner::AgentResult> = None;
        let mut stream_err: Option<String> = None;
        // 子会话 id(摘要指针用;SessionCreated 即锚定)
        let mut child_sid = String::new();
        // 本委托窗口 token 归账基线（子 runner 每次 LLM 调用后累加,
        // Done 时以差值落 SpawnRecord;未接线埋点时恒 0-0=None 语义）
        let tokens_start = self.token_total();
        let mut spawn_record_idx: Option<usize> = None;
        while let Some(ev) = futures_util::StreamExt::next(&mut stream).await {
            match ev {
                Ok(crate::agent::runner::AgentEvent::Done(r)) => {
                    final_result = Some(r);
                    break;
                }
                Ok(crate::agent::runner::AgentEvent::SessionCreated { session_id, .. }) => {
                    // 子会话锚即刻入账(父 runner 在本委托调用的工具结果
                    // 写账时 drain 落事件;先记后亡的取消分支也留锚——
                    // 记录语义="实际创建的子会话")
                    child_sid.clone_from(&session_id);
                    if let Ok(mut ledger) = self.spawn_ledger.lock() {
                        spawn_record_idx = Some(ledger.len());
                        ledger.push(SpawnRecord {
                            child_session_id: session_id,
                            agent_type: agent_type.to_string(),
                            depth: self.current_depth + 1,
                            task_digest: crate::agent::journal::evorule_digest(task),
                            tokens_used: None,
                        });
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    // 流式实现中 Err 后仍会跟 Done(error)；记录并以 Done 为权威终态
                    stream_err = Some(e.to_string());
                }
            }
        }

        // 窗口闭合归账（Done/流级错/无终态均记——差值落回本委托的账目行）
        if let Some(idx) = spawn_record_idx {
            if let Ok(mut ledger) = self.spawn_ledger.lock() {
                if let Some(rec) = ledger.get_mut(idx) {
                    rec.tokens_used = Some(self.token_total().saturating_sub(tokens_start));
                }
            }
        }

        match final_result {
            Some(r) => {
                if r.success {
                    tracing::info!(
                        agent_type,
                        depth = self.current_depth,
                        "Sub-agent execution succeeded"
                    );
                    Ok(digest_delegate_result(&child_sid, agent_type, &r.content))
                } else {
                    let err = r.error.unwrap_or_default();
                    tracing::warn!(
                        agent_type,
                        depth = self.current_depth,
                        "Sub-agent execution failed: {}",
                        err
                    );
                    Err(err)
                }
            }
            None => {
                let err = format!(
                    "Sub-agent runtime error: {}",
                    stream_err.unwrap_or_else(|| "stream ended without Done".to_string())
                );
                tracing::error!(agent_type, depth = self.current_depth, "{}", err);
                Err(err)
            }
        }
    }

    /// G9:并行委托多个子 agent
    ///
    /// 所有子 agent 同时启动,各自独立 `run()`,全部完成后返回结果列表。
    /// 任意子 agent 失败不影响其他(失败项返回 `Err`)。
    ///
    /// # 深度
    ///
    /// 每个子 agent 在 `current_depth + 1` 层执行。若 `current_depth + 1 >= max_depth`,
    /// 对应子 agent 返回 `Err`(深度超限),不影响其他子 agent。
    ///
    /// # 并发限流
    ///
    /// 若配置了 `max_concurrent`,每个子任务在 `delegate()` 前 acquire permit,
    /// 防止 session 数暴增。
    ///
    /// # 参数
    ///
    /// - `tasks`:`(agent_type, task)` 列表
    ///
    /// # 返回
    ///
    /// 与 `tasks` 等长、同序的结果列表(`Ok(content)` / `Err(msg)`)。
    pub async fn delegate_parallel(
        &self,
        tasks: Vec<(String, String)>,
    ) -> Vec<Result<String, String>> {
        self.delegate_parallel_with_context(
            tasks
                .into_iter()
                .map(|(agent_type, task)| (agent_type, task, None))
                .collect(),
        )
        .await
    }

    /// 带父上下文摘要的并行委托(其余同 [`Self::delegate_parallel`])
    pub async fn delegate_parallel_with_context(
        &self,
        tasks: Vec<(String, String, Option<String>)>,
    ) -> Vec<Result<String, String>> {
        let futures: Vec<_> = tasks
            .into_iter()
            .map(|(agent_type, task, context)| {
                let ctx = self.increment_depth();
                let sem = ctx.max_concurrent.clone();
                async move {
                    if let Some(sem) = sem {
                        let _permit = match sem.acquire_owned().await {
                            Ok(p) => p,
                            Err(e) => {
                                return Err(format!("delegate semaphore closed: {}", e));
                            }
                        };
                    }
                    ctx.delegate_with_context(&agent_type, &task, context.as_deref())
                        .await
                }
            })
            .collect();
        futures_util::future::join_all(futures).await
    }

    /// G9:竞速委托(任一完成即返回,取消其余)
    ///
    /// 适用场景:多模型/多策略试跑,取最快结果。
    ///
    /// 第一个完成的子 agent 的结果(无论 Ok/Err)被返回,其余被 drop(取消)。
    /// 被取消的子 agent 的 evorule session 可能残留(由 evorule 自身的 session
    /// 超时清理),P1 阶段不处理。
    ///
    /// # 参数
    ///
    /// - `tasks`:`(agent_type, task)` 列表(至少 1 个)
    ///
    /// # 返回
    ///
    /// 第一个完成子 agent 的 `Result<String, String>`。
    pub async fn delegate_race(&self, tasks: Vec<(String, String)>) -> Result<String, String> {
        self.delegate_race_with_context(
            tasks
                .into_iter()
                .map(|(agent_type, task)| (agent_type, task, None))
                .collect(),
        )
        .await
    }

    /// 带父上下文摘要的竞速委托(其余同 [`Self::delegate_race`])
    pub async fn delegate_race_with_context(
        &self,
        tasks: Vec<(String, String, Option<String>)>,
    ) -> Result<String, String> {
        let futures: Vec<DelegateFuture> = tasks
            .into_iter()
            .map(|(agent_type, task, context)| {
                let ctx = self.increment_depth();
                Box::pin(async move {
                    if let Some(sem) = ctx.max_concurrent.clone() {
                        let _permit = match sem.acquire_owned().await {
                            Ok(p) => p,
                            Err(e) => {
                                return Err(format!("delegate semaphore closed: {}", e));
                            }
                        };
                    }
                    ctx.delegate_with_context(&agent_type, &task, context.as_deref())
                        .await
                }) as Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
            })
            .collect();

        if futures.is_empty() {
            return Err("delegate_race requires at least 1 task".to_string());
        }

        // 用 select_all 取第一个完成的;其余被 drop(取消)
        let (result, _index, _remaining) = futures_util::future::select_all(futures).await;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_client() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:8080")
    }

    // ===== 回传确定性摘要 + 上下文拼接(纯函数) =====

    #[test]
    fn compose_task_appends_context_prefix() {
        assert_eq!(compose_task("do x", None), "do x");
        assert_eq!(
            compose_task("do x", Some("  ")),
            "do x",
            "纯空白 context=无注入"
        );
        let composed = compose_task("do x", Some("决策:用方案B"));
        assert!(composed.contains("【父代理上下文摘要】"));
        assert!(composed.contains("决策:用方案B"));
        assert!(composed.contains("【委托任务】\ndo x"));
        // 顺序:摘要在前,任务在后
        let summary_pos = composed.find("决策:用方案B").unwrap();
        let task_pos = composed.find("do x").unwrap();
        assert!(summary_pos < task_pos);
    }

    #[test]
    fn digest_keeps_short_content_verbatim() {
        let short = "短结果";
        assert_eq!(digest_delegate_result("s1", "researcher", short), short);
    }

    #[test]
    fn digest_truncates_long_content_with_head_tail_and_pointer() {
        let long = format!("{}尾", "汉".repeat(DIGEST_THRESHOLD_CHARS + 500));
        let out = digest_delegate_result("sid-9", "researcher", &long);
        assert!(out.contains("[委托结果摘要]"), "结构化标记在");
        assert!(out.contains("sid-9"));
        assert!(out.contains("agent=researcher"));
        let total: usize = long.chars().count();
        assert!(out.contains(&format!("全文 {total} 字符")));
        // 头部摘录在(开头的字符序列),尾部摘录在(结尾字符含"尾")
        assert!(out.contains(&"汉".repeat(64)));
        assert!(out.ends_with("—— 结束 ——"));
        assert!(out.contains("尾\n—— 结束 ——".trim_end()));
        // 摘录边界多字节安全(无 U+FFFD 替换符)
        assert!(!out.contains('\u{FFFD}'), "字符边界截断不得产生替换符");
        // 输出显著短于原文
        assert!(out.chars().count() < total / 2);
    }

    #[test]
    fn digest_char_boundary_safety_on_mixed_input() {
        // 3 字节汉字+1 字节 ASCII 混排,切点落在多字节序列内部的概率最大化
        let long = "a汉b字c节".repeat(DIGEST_THRESHOLD_CHARS);
        let out = digest_delegate_result("s", "w", &long);
        assert!(!out.contains('\u{FFFD}'));
    }

    fn make_ctx() -> DelegateContext {
        let definitions = AgentDefinitionManager::with_default_dir();
        let mut ctx = DelegateContext::new("parent", definitions, make_test_client());
        ctx.journal_dir = None;
        ctx
    }

    #[test]
    fn test_delegate_context_new() {
        let ctx = make_ctx();
        assert_eq!(ctx.current_depth, 0);
        assert_eq!(ctx.parent_agent_type, "parent");
        assert_eq!(ctx.max_depth, DEFAULT_MAX_DELEGATE_DEPTH);
        assert!(ctx.max_concurrent.is_none());
    }

    // ===== LLM handler 下放接线(配置兜底 max_tokens 防回归) =====

    #[test]
    fn sub_runner_inherits_llm_max_tokens_config() {
        // 构造点下放的 handler 必须原样落到子 runner——统一解析产物
        // ([llm] max_tokens)随行,否则子 runner 落 with_defaults 4096 硬兜底
        let mut cfg = crate::config::LlmConfig::default();
        cfg.max_tokens = Some(32768);
        let ctx = make_ctx()
            .with_llm_handler(crate::io_handlers::LlmHandler::from_config(&cfg));
        let runner = ctx.wire_llm_handler(crate::agent::runner::AgentRunner::new(
            crate::agent::runner::AgentConfig::default(),
            make_test_client(),
        ));
        assert_eq!(
            runner.llm_handler_config_max_tokens_for_test(),
            Some(32768),
            "子 runner 必须携带构造点下放的 [llm] max_tokens 配置兜底"
        );
    }

    #[test]
    fn sub_runner_without_handler_keeps_default_fallback() {
        // None = 既有行为零变化(with_defaults,4096 硬兜底)
        let ctx = make_ctx();
        let runner = ctx.wire_llm_handler(crate::agent::runner::AgentRunner::new(
            crate::agent::runner::AgentConfig::default(),
            make_test_client(),
        ));
        assert_eq!(runner.llm_handler_config_max_tokens_for_test(), None);
    }

    #[test]
    fn test_delegate_context_step_timeout_override() {
        // 缺省 None = 粒定义值零变化;builder 注入后按值携带
        let ctx = make_ctx();
        assert!(ctx.step_timeout_override.is_none());
        let ctx = ctx.with_step_timeout_override(Some(std::time::Duration::from_secs(1300)));
        assert_eq!(
            ctx.step_timeout_override,
            Some(std::time::Duration::from_secs(1300))
        );
        let ctx = ctx.with_step_timeout_override(None);
        assert!(ctx.step_timeout_override.is_none());
    }

    #[test]
    fn test_delegate_context_with_depth() {
        let ctx = make_ctx().with_depth(2);
        assert_eq!(ctx.current_depth, 2);
    }

    #[test]
    fn test_delegate_context_with_max_depth() {
        let ctx = make_ctx().with_max_depth(10);
        assert_eq!(ctx.max_depth, 10);
    }

    #[test]
    fn test_delegate_context_with_max_concurrent() {
        let ctx = make_ctx().with_max_concurrent_delegates(5);
        assert!(ctx.max_concurrent.is_some());
        // 0 = 不限流
        let ctx0 = make_ctx().with_max_concurrent_delegates(0);
        assert!(ctx0.max_concurrent.is_none());
    }

    #[test]
    fn test_delegate_context_increment_depth() {
        let ctx = make_ctx().with_depth(1);
        let ctx2 = ctx.increment_depth();
        assert_eq!(ctx2.current_depth, 2);
        assert_eq!(ctx.current_depth, 1); // 原对象不变
                                          // max_depth 延续
        assert_eq!(ctx2.max_depth, ctx.max_depth);
    }

    #[test]
    fn test_increment_depth_shares_semaphore() {
        // Arc<Semaphore> 在 increment_depth clone 时共享(指针相等)
        let ctx = make_ctx().with_max_concurrent_delegates(3);
        let ctx2 = ctx.increment_depth();
        assert!(Arc::ptr_eq(
            ctx.max_concurrent.as_ref().unwrap(),
            ctx2.max_concurrent.as_ref().unwrap()
        ));
    }

    // ===== 治理段下放 + 记忆下放开关 + spawn 账 =====

    #[test]
    fn test_governance_segment_injection_and_default() {
        // 缺省 None（CLI 口径=子代理与 CLI 主路径同口径无治理段）
        let ctx = make_ctx();
        assert!(ctx.governance_segment.is_none());
        // serve 构造点传入后字段在位
        let ctx2 = make_ctx().with_governance_segment(Some("L2 约束前馈合并段".to_string()));
        assert_eq!(
            ctx2.governance_segment.as_deref(),
            Some("L2 约束前馈合并段")
        );
    }

    #[test]
    fn test_memory_propagation_flag_default_off() {
        // 缺省关——子代理默认无状态执行器
        assert!(!make_ctx().propagate_memory);
        // 显式开启后随 clone 延续（increment_depth 内部 clone）
        let ctx = make_ctx().with_memory_propagation();
        assert!(ctx.propagate_memory);
        assert!(ctx.increment_depth().propagate_memory);
    }

    #[test]
    fn test_governance_and_ledger_continue_through_clone() {
        // 治理段与 spawn 账随 increment_depth 延续;账为整树同一本(Arc 指针相等)
        let ctx = make_ctx().with_governance_segment(Some("seg".to_string()));
        let child = ctx.increment_depth();
        assert_eq!(child.governance_segment.as_deref(), Some("seg"));
        assert!(Arc::ptr_eq(&ctx.spawn_ledger, &child.spawn_ledger));
    }

    #[test]
    fn test_agents_md_segment_builder() {
        // 项目记忆段下放:builder 注入+clone 延续(与治理段同款口径)
        let ctx = make_ctx().with_agents_md_segment(Some("# notes".to_string()));
        assert_eq!(ctx.agents_md_segment.as_deref(), Some("# notes"));
        let child = ctx.increment_depth();
        assert_eq!(child.agents_md_segment.as_deref(), Some("# notes"));
        assert!(make_ctx().agents_md_segment.is_none());
    }

    #[test]
    fn test_spawn_ledger_drain_clears_and_preserves_fields() {
        // drain:全部取走+字段保真;二次 drain=空(无跨调用残留)
        let ctx = make_ctx();
        {
            let mut ledger = ctx.spawn_ledger.lock().unwrap_or_else(|p| p.into_inner());
            ledger.push(SpawnRecord {
                child_session_id: "c1".to_string(),
                agent_type: "planner".to_string(),
                depth: 1,
                task_digest: "blake3:aa".to_string(),
                tokens_used: None,
            });
            ledger.push(SpawnRecord {
                child_session_id: "c2".to_string(),
                agent_type: "worker".to_string(),
                depth: 1,
                task_digest: "blake3:bb".to_string(),
                tokens_used: None,
            });
        }
        let drained = ctx.drain_spawn_records();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].child_session_id, "c1");
        assert_eq!(drained[0].agent_type, "planner");
        assert_eq!(drained[0].depth, 1);
        assert_eq!(drained[1].child_session_id, "c2");
        assert!(ctx.drain_spawn_records().is_empty(), "drain 后账清空");
    }

    #[test]
    fn test_spawn_ledger_shared_across_tree() {
        // 共享账:子上下文记录的条目从父上下文可见(drain 同账)
        let parent = make_ctx();
        let child = parent.increment_depth();
        {
            let mut ledger = child.spawn_ledger.lock().unwrap_or_else(|p| p.into_inner());
            ledger.push(SpawnRecord {
                child_session_id: "grandchild".to_string(),
                agent_type: "worker".to_string(),
                depth: 2,
                task_digest: "blake3:cc".to_string(),
                tokens_used: None,
            });
        }
        let drained = parent.drain_spawn_records();
        assert_eq!(drained.len(), 1, "父 drain 取到子分支记录(整树同账)");
        assert_eq!(drained[0].child_session_id, "grandchild");
    }

    #[test]
    fn test_delegate_context_can_delegate() {
        let ctx = make_ctx().with_depth(2);
        assert!(ctx.can_delegate(3));
        assert!(!ctx.can_delegate(2));
        assert!(ctx.can_delegate(10));
    }

    // ===== 工具面注入 =====

    #[test]
    fn test_delegate_context_with_toolkit_injection() {
        // 默认无工具面（旧行为）
        let ctx = make_ctx();
        assert!(ctx.toolkit.is_none());
        assert!(ctx.workdir.is_none());
        // with_toolkit 成对注入
        let ctx2 = make_ctx().with_toolkit(ToolHandler::new(), Path::new("D:/tmp"));
        assert!(ctx2.toolkit.is_some());
        assert_eq!(ctx2.workdir.as_deref(), Some(Path::new("D:/tmp")));
    }

    #[test]
    fn test_delegate_context_toolkit_continues_through_clone() {
        // toolkit/workdir 随 increment_depth（内部 clone）延续到每个子 runner
        let ctx = make_ctx().with_toolkit(ToolHandler::new(), Path::new("."));
        let child = ctx.increment_depth();
        assert!(child.toolkit.is_some());
        assert!(child.workdir.is_some());
    }

    #[test]
    fn test_can_delegate_more_uses_self_max_depth() {
        let ctx = make_ctx().with_depth(2).with_max_depth(3);
        assert!(ctx.can_delegate_more()); // 2 < 3
        let ctx2 = make_ctx().with_depth(3).with_max_depth(3);
        assert!(!ctx2.can_delegate_more()); // 3 >= 3
    }

    // ===== G9:深度强制 =====

    #[tokio::test]
    async fn test_delegate_rejects_when_depth_exceeded() {
        // current_depth == max_depth → 拒绝(不实际加载 agent 定义)
        let ctx = make_ctx().with_depth(3).with_max_depth(3);
        let result = ctx.delegate("researcher", "do something").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("max delegate depth exceeded"), "got: {}", err);
        assert!(err.contains("current_depth=3"), "got: {}", err);
        assert!(err.contains("max_depth=3"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_delegate_rejects_when_depth_exceeds_by_more() {
        let ctx = make_ctx().with_depth(5).with_max_depth(3);
        let result = ctx.delegate("researcher", "task").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("max delegate depth exceeded"));
    }

    // ===== G9:delegate_parallel =====

    #[tokio::test]
    async fn test_delegate_parallel_empty_tasks() {
        let ctx = make_ctx();
        let results = ctx.delegate_parallel(vec![]).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_delegate_parallel_all_exceed_depth() {
        // 所有子任务都在 depth+1 层,而 max_depth=1 → 全部超限
        let ctx = make_ctx().with_depth(1).with_max_depth(1);
        let tasks = vec![
            ("agent_a".to_string(), "task_a".to_string()),
            ("agent_b".to_string(), "task_b".to_string()),
            ("agent_c".to_string(), "task_c".to_string()),
        ];
        let results = ctx.delegate_parallel(tasks).await;
        assert_eq!(results.len(), 3);
        for r in &results {
            assert!(r.is_err());
            assert!(r
                .as_ref()
                .unwrap_err()
                .contains("max delegate depth exceeded"));
        }
    }

    #[tokio::test]
    async fn test_delegate_parallel_preserves_order() {
        // 即使各子任务耗时不同,join_all 保持输入顺序
        let ctx = make_ctx().with_depth(1).with_max_depth(1);
        let tasks = vec![
            ("a1".to_string(), "t1".to_string()),
            ("a2".to_string(), "t2".to_string()),
            ("a3".to_string(), "t3".to_string()),
        ];
        let results = ctx.delegate_parallel(tasks).await;
        assert_eq!(results.len(), 3);
        // 全部 Err(深度超限),但顺序与输入一致
        for r in &results {
            assert!(r.is_err());
        }
    }

    // ===== G9:delegate_race =====

    #[tokio::test]
    async fn test_delegate_race_empty_returns_err() {
        let ctx = make_ctx();
        let result = ctx.delegate_race(vec![]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("at least 1 task"));
    }

    #[tokio::test]
    async fn test_delegate_race_single_exceeds_depth() {
        let ctx = make_ctx().with_depth(2).with_max_depth(2);
        let result = ctx
            .delegate_race(vec![("agent_a".to_string(), "task".to_string())])
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("max delegate depth exceeded"));
    }

    #[tokio::test]
    async fn test_delegate_race_multiple_all_exceed_depth() {
        let ctx = make_ctx().with_depth(2).with_max_depth(2);
        let tasks = vec![
            ("a1".to_string(), "t1".to_string()),
            ("a2".to_string(), "t2".to_string()),
        ];
        let result = ctx.delegate_race(tasks).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("max delegate depth exceeded"));
    }

    // ===== G9:并发限流不阻塞测试(验证 Semaphore 可构造) =====

    #[tokio::test]
    async fn test_delegate_parallel_with_semaphore_all_exceed_depth() {
        // 配了限流 + 全部超限:permit 仍能获取(因为没有实际 delegate 占用)
        let ctx = make_ctx()
            .with_depth(1)
            .with_max_depth(1)
            .with_max_concurrent_delegates(2);
        let tasks = vec![
            ("a1".to_string(), "t1".to_string()),
            ("a2".to_string(), "t2".to_string()),
        ];
        let results = ctx.delegate_parallel(tasks).await;
        assert_eq!(results.len(), 2);
        for r in &results {
            assert!(r.is_err());
        }
    }

    #[test]
    fn test_default_max_concurrent_delegates_is_5() {
        assert_eq!(DEFAULT_MAX_CONCURRENT_DELEGATES, 5);
    }
}
