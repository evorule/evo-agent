// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Agent 瀹氫箟 鈥斺€?浠?agent.json 鍔犺浇 Agent 閰嶇疆

use std::path::{Path, PathBuf};

use crate::agent::runner::AgentConfig;

/// Agent 瀹氫箟鍔犺浇閿欒
#[derive(Debug)]
pub enum AgentDefinitionError {
    /// IO 閿欒
    Io(std::io::Error),
    /// JSON 瑙ｆ瀽閿欒
    Json(serde_json::Error),
    /// Agent 绫诲瀷鏈壘鍒
    NotFound(String),
    /// 定义非法(门卫:取值越界/标识符不合法)
    InvalidDefinition(String),
}

impl std::fmt::Display for AgentDefinitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentDefinitionError::Io(e) => write!(f, "IO error: {}", e),
            AgentDefinitionError::Json(e) => write!(f, "JSON parse error: {}", e),
            AgentDefinitionError::NotFound(t) => write!(f, "Agent type not found: {}", t),
            AgentDefinitionError::InvalidDefinition(msg) => {
                write!(f, "Invalid agent definition: {}", msg)
            }
        }
    }
}

impl std::error::Error for AgentDefinitionError {}

impl From<std::io::Error> for AgentDefinitionError {
    fn from(e: std::io::Error) -> Self {
        AgentDefinitionError::Io(e)
    }
}

impl From<serde_json::Error> for AgentDefinitionError {
    fn from(e: serde_json::Error) -> Self {
        AgentDefinitionError::Json(e)
    }
}

/// 消息持久化配置（031 设计文档 P0，用户决策 2：可选开关）
///
/// 控制 `AgentRunner` 何时把对话消息写入 evorule payload。
/// 在 agent.json 中按如下配置：
/// ```json
/// "memory": {
///   "type": "persistent",
///   "namespace": "researcher",
///   "message_persist": { "mode": "every_n", "n": 5 }
/// }
/// ```
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MessagePersistConfig {
    /// 持久化模式：
    /// - `"every_message"`: 每条消息立即写入（默认，最安全）
    /// - `"every_n"`: 每 N 条消息批量写入
    /// - `"per_react_round"`: 每轮 ReAct 结束时写入
    /// - `"disabled"`: 不持久化消息（向后兼容旧行为）
    pub mode: String,
    /// 每 N 条消息刷写一次（仅 `mode == "every_n"` 时生效，必须 > 0）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<usize>,
}

impl Default for MessagePersistConfig {
    fn default() -> Self {
        Self {
            mode: "every_message".to_string(),
            n: None,
        }
    }
}

impl MessagePersistConfig {
    /// 解析为 `MemoryManager` 内部使用的 `MessagePersistMode` 枚举
    ///
    /// # 错误
    /// - 未知 mode 字符串
    /// - `every_n` 模式下 `n` 为 0 或缺失
    pub fn to_mode(&self) -> Result<crate::agent::memory::MessagePersistMode, String> {
        match self.mode.as_str() {
            "every_message" => Ok(crate::agent::memory::MessagePersistMode::EveryMessage),
            "every_n" => {
                let n = self.n.unwrap_or(0);
                if n == 0 {
                    return Err("message_persist.n must be > 0 when mode == 'every_n'".to_string());
                }
                Ok(crate::agent::memory::MessagePersistMode::EveryN(n))
            }
            "per_react_round" => Ok(crate::agent::memory::MessagePersistMode::PerReactRound),
            "disabled" => Ok(crate::agent::memory::MessagePersistMode::Disabled),
            other => Err(format!("unknown message_persist mode: {}", other)),
        }
    }
}

/// 鍐呭瓨閰嶇疆
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryConfig {
    #[serde(rename = "type")]
    /// 鍐呭瓨绫诲瀷锛堝涓 "none", "persistent"锛
    pub memory_type: String,
    /// 鍐呭瓨鍛藉悕绌洪棿
    pub namespace: String,
    /// 消息持久化配置（用户决策 2：可选开关，默认 `every_message`）
    #[serde(default)]
    pub message_persist: MessagePersistConfig,
    /// 记忆过期时间（秒，可选，用户决策 5：TTL）
    ///
    /// 设置后记忆条目在 `ttl_secs` 秒后过期，由 `MemoryManager` 惰性清理。
    /// 未设置（`None`）时记忆永久保留。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
    /// 摘要模型名称（可选，用户决策 3：单独配置 `summary_model`）
    ///
    /// 设置后 P4 记忆压缩使用此模型生成 summary，否则 fallback 到主 `model`。
    /// P0+P1 阶段不使用此字段，仅占位。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_model: Option<String>,
    /// G14/Q18:事件提取模型名称（可选，单独配置 `extraction_model`）
    ///
    /// 设置后 032 EventExtractor 使用此模型从对话中提取结构化事件字段，
    /// 否则 fallback 到主 `model`。可用便宜模型(如 GPT-4o-mini)降低成本。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction_model: Option<String>,
    /// 事件提取关键词列表（可选；缺省=内置默认表）
    ///
    /// 覆盖 032 EventExtractor 的自动提取触发关键词（原实现配置面
    /// 宣称可覆盖但恒用 Default——本字段使宣称成立）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction_keywords: Option<Vec<String>>,
    /// 显式触发短语列表（可选；缺省=内置默认表）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction_explicit_phrases: Option<Vec<String>>,
    /// 阶段 1(F-618):LexStore 检索缓存 DB 路径(可选)
    ///
    /// 配置后 stable/events 召回走本地 SQLite 索引缓存(60s TTL,过期才
    /// 全量刷新)——省每轮 O(N) 网络拉取;store 错误一律降级全量路径
    /// (I14)。未配置零影响。tb-agent 竞赛配置建议启用。
    /// 另为自省记忆工具(阶段 3)的数据前提:工具暴露还需
    /// memory.recipe 的 tools.expose 声明(白名单,缺省不暴露)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lex_store: Option<String>,
    /// 阶段 2(F-610):MemoryRecipe 策略规则集(可选;内嵌 JSON 形态,
    /// 解析为 recipe::MemoryRecipe——检索权重/半衰期/生命周期阈值/
    /// 自省工具暴露面(tools.expose)数据化;
    /// 未配置=词法 legacy 行为,既有 agent 零影响)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<serde_json::Value>,
    /// C3: 记忆区占窗口的比例（默认 0.25）
    #[serde(default = "default_memory_budget_ratio")]
    pub memory_budget_ratio: f32,
    /// C2: 最近 N 个会话摘要（默认 3）
    #[serde(default = "default_max_session_summaries")]
    pub max_session_summaries: usize,
    /// C2: top-K 事件（默认 5）
    #[serde(default = "default_max_injected_events")]
    pub max_injected_events: usize,
    /// C4: L1 摘要 rollup 阈值（默认 10）
    #[serde(default = "default_summary_rollup_threshold")]
    pub summary_rollup_threshold: usize,
    /// C1: 是否启用事件提取（默认 true）
    #[serde(default = "default_true")]
    pub enable_event_extraction: bool,
    /// 是否启用知识候选提取（默认 true）
    ///
    /// 会话收尾时 sediment 增一次 sidecar 审计 LLM 调用提取知识候选
    /// （五类），落 shared.{ns}.knowledge_candidates.*；无审计通路时
    /// 自动跳过（纪律①）。
    #[serde(default = "default_true")]
    pub enable_knowledge_extraction: bool,
}

fn default_memory_budget_ratio() -> f32 {
    0.25
}
fn default_max_session_summaries() -> usize {
    3
}
fn default_max_injected_events() -> usize {
    5
}
fn default_summary_rollup_threshold() -> usize {
    10
}
fn default_true() -> bool {
    true
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            memory_type: "none".to_string(),
            namespace: String::new(),
            message_persist: MessagePersistConfig::default(),
            ttl_secs: None,
            summary_model: None,
            extraction_model: None,
            extraction_keywords: None,
            extraction_explicit_phrases: None,
            lex_store: None,
            recipe: None,
            memory_budget_ratio: default_memory_budget_ratio(),
            max_session_summaries: default_max_session_summaries(),
            max_injected_events: default_max_injected_events(),
            summary_rollup_threshold: default_summary_rollup_threshold(),
            enable_event_extraction: default_true(),
            enable_knowledge_extraction: default_true(),
        }
    }
}

/// Output format configuration
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OutputFormat {
    #[serde(rename = "type")]
    /// Format type (e.g. "json", "text")
    pub format_type: String,
    /// Output schema (optional)
    pub schema: Option<serde_json::Value>,
    /// G11:校验失败时的最大重试次数(可选,默认 2)
    ///
    /// LLM 输出不符合 schema 时,runner 注入校正消息让 LLM 重试。
    /// 超过 `max_retries` 次后降级为接受原输出(避免死循环)。
    /// 设为 `Some(0)` 表示不重试,首次失败即降级接受。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<usize>,
}

/// 沙箱类工具清单:出现在顶层 `tools` 时,`capability_boundary.tools` 必须覆盖
/// (单一事实源门卫的判定基准)
pub const SANDBOX_CAPABLE_TOOLS: &[&str] = &["file_read", "file_write"];

/// 能力边界声明（M5-a 全局观机制,2026-09-25;agent_def v1.1 增量字段）
///
/// 声明是 file 类工具沙箱检查的**唯一权威**（宪法 §七 反模式④封堵:禁声明与
/// 执行双源并存——启动配置仅作为「未声明时」的缺省来源）,同时在会话建立时
/// 注入系统级边界段与会话事实:首要读者是 agent 自身（LLM 自知边界,不再
/// 「摸不着自己」——越界请求能自述边界而非误报「文件不存在」）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CapabilityBoundary {
    /// 访问模式:"read_only"(只读) | "read_write"(沙箱根全域可读写)
    pub mode: String,
    /// 沙箱根目录(绝对路径);file 类工具路径一律相对该根解析,越界即拒
    pub sandbox_root: PathBuf,
    /// 边界内工具清单(语义门卫:顶层 tools 中的沙箱类工具必须列于此;
    /// mode=read_only 时不得含 file_write)
    pub tools: Vec<String>,
}

impl CapabilityBoundary {
    /// 是否只读模式
    pub fn is_read_only(&self) -> bool {
        self.mode == "read_only"
    }

    /// 序列化为会话事实形态(经 create_session initial_content 既有载体进链,
    /// 零新 Fact 类型)
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "capability_boundary": {
                "mode": self.mode,
                "sandbox_root": self.sandbox_root.display().to_string(),
                "tools": self.tools,
            }
        })
    }

    /// 系统级边界段文本(会话建立稳定位置追加到 system_prompt 尾部;
    /// 首要读者 = LLM 自知——缺输入就地编造是「一本正经胡说八道」的机理,
    /// 显式供给边界是机制解法而非 prompt 恳求)
    ///
    /// 段内第二块=默认会话交接协议(自主交接设计 PR-H1,映射 S4_boundary 槽位,
    /// 新注入面入表不游离):主动交接优于被动截断,交接档六要素+新会话首动作
    /// 判据。工具可用性由 agentTools.handover 开关与裁决面独立管;协议文本
    /// 默认在场(工具缺席时模型按纪律输出交接内容,不编造工具调用)。
    pub fn awareness_segment(&self) -> String {
        let mode_desc = if self.is_read_only() {
            "read_only(只读,你没有写文件能力)"
        } else {
            "read_write(沙箱根全域可读写)"
        };
        format!(
            "【能力边界声明】\n\
             - 访问模式:{}\n\
             - 沙箱根目录:{}(一切文件路径相对该根解析)\n\
             - 边界内工具:{}\n\
             越出沙箱根的路径不可访问;尝试越界的操作会被拒绝并告知边界。\
             若任务需要边界外的资源,如实说明边界限制,不要猜测或编造。\n\n\
             【会话交接协议】\n\
             - 主动交接优于被动截断:窗口余量紧张(见 [context] 余量信号)或\
             任务将跨会话延续时,先写结构化交接档,再经 session_spawn 开启\
             子会话续接(父子因果链入账,新会话按交接档起步)。\n\
             - 交接档六要素:①任务目标(复述,防漂移)②已完成项+证据锚点\
             (文件:行/commit/测试名)③下一步动作(按序,可执行粒度)④关键路径\
             与关键决策理由⑤踩坑清单⑥环境状态(HEAD/测试基线/在途改动)。\n\
             - 新会话首动作=读交接档并按其中的续接判据自检,确认续接成功后\
             再继续任务;跳过读档直接开工,审计面可见。\n\
             - 交接不是失败:任务半程主动交接是长程一致性的正规手段。",
            mode_desc,
            self.sandbox_root.display(),
            self.tools.join(", ")
        )
    }
}

/// Skill 声明条目(skills 装配 B2 批,agent_def 增量字段)
///
/// 声明时刻 = 人工把关(供应链闸口前移到 definition):声明者只管挑文件,
/// LLM 的装载判断依据 100% 来自 SKILL.md 本身——description 不手填,启动时
/// 从 frontmatter 自动解析(单一事实源,防两处维护漂移,见
/// [`resolve_skill_manifest_entries`])。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SkillEntry {
    /// 技能名(必填,全表唯一;LLM 用它调 read_skill)
    pub name: String,
    /// SKILL.md 文件路径(绝对,或相对 definition 文件——load_from_dir
    /// 解析为绝对路径;read_skill 按此表映射,不走 path_scope 沙箱:
    /// 路径来源=人工声明而非 LLM 参数,沙箱攻击面不存在)
    pub path: PathBuf,
}

/// Skill 解析产物(声明 name → 绝对路径 + frontmatter description;
/// manifest 注入与 read_skill 注册共用此形态)
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SkillManifestEntry {
    /// 技能名(= SkillEntry.name)
    pub name: String,
    /// 解析后的 SKILL.md 绝对路径
    pub path: PathBuf,
    /// frontmatter description(manifest 渲染用;缺失时为空串,加载期 warn)
    pub description: String,
}

/// SKILL.md frontmatter 解析产物(轻量:仅取围栏内平铺键值)
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SkillFrontmatter {
    /// frontmatter name(仅解析留档;生效名 = 声明表名,单一事实源防两处漂移)
    pub name: Option<String>,
    /// frontmatter description(manifest 渲染用;缺省 = 声明缺失,加载期 warn)
    pub description: Option<String>,
}

/// 轻量 frontmatter 解析(照 skill-adapter 正则行解析口径,不引 YAML 依赖:
/// 仅取 `---` 围栏内平铺 `key: value` 行;嵌套/列表不支持——显式未覆盖声明,
/// 市面 skill 实测遇解析失败按样本补)
pub fn parse_skill_frontmatter(content: &str) -> Result<SkillFrontmatter, String> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut lines = content.lines().map(str::trim_start);
    let first = lines
        .find(|l| !l.is_empty())
        .ok_or_else(|| "empty file, expected '---' frontmatter fence".to_string())?;
    if first.trim_end() != "---" {
        return Err("missing '---' frontmatter fence at file head".to_string());
    }
    let mut fm = SkillFrontmatter::default();
    let mut closed = false;
    for line in lines {
        let t = line.trim_end();
        if t == "---" {
            closed = true;
            break;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        // 平铺 `key: value`(无冒号行跳过,宽松口径与 adapter 一致)
        let Some((k, v)) = t.split_once(':') else {
            continue;
        };
        let value = v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        match k.trim() {
            "name" => fm.name = Some(value),
            "description" => fm.description = Some(value),
            _ => {}
        }
    }
    if !closed {
        return Err("frontmatter fence not closed (missing closing '---')".to_string());
    }
    Ok(fm)
}

/// 解析 skills 声明为生效清单(逐条验证:路径存在且为文件、frontmatter
/// 可解析;description 缺失 warn 不拦——描述质量决定 LLM 是否装载,是
/// 声明者的生态责任,机制只提醒)
///
/// 路径语义:绝对路径原样;相对路径相对 definition 文件目录(load_from_dir
/// 解析)或进程工作目录(直接构造 definition 的调用方,如测试)。Err =
/// fail-visible,调用方(加载期/runner 构造期)拒绝启动,不静默降级。
pub fn resolve_skill_manifest_entries(
    entries: &[SkillEntry],
) -> Result<Vec<SkillManifestEntry>, String> {
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let meta = std::fs::metadata(&e.path).map_err(|err| {
            format!(
                "skill '{}' path '{}' unreadable: {}",
                e.name,
                e.path.display(),
                err
            )
        })?;
        if !meta.is_file() {
            return Err(format!(
                "skill '{}' path '{}' is not a regular file",
                e.name,
                e.path.display()
            ));
        }
        let content = std::fs::read_to_string(&e.path)
            .map_err(|err| format!("skill '{}' read failed: {}", e.name, err))?;
        let fm = parse_skill_frontmatter(&content).map_err(|err| {
            format!(
                "skill '{}' frontmatter unparseable ({}): {}",
                e.name,
                e.path.display(),
                err
            )
        })?;
        let description = match fm.description {
            Some(d) if !d.is_empty() => d,
            _ => {
                tracing::warn!(
                    skill = %e.name,
                    "skill frontmatter has no description; manifest line will be empty \
                     (description quality drives the LLM's loading decision)"
                );
                String::new()
            }
        };
        out.push(SkillManifestEntry {
            name: e.name.clone(),
            path: e.path.clone(),
            description,
        });
    }
    Ok(out)
}

/// Agent definition
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentDefinition {
    /// Agent type identifier
    pub agent_type: String,
    /// Version number
    pub version: String,
    /// Description
    pub description: String,
    /// System prompt
    pub system_prompt: String,
    /// Model name to use
    pub model: String,
    /// Temperature parameter
    pub temperature: f32,
    /// Max execution steps
    pub max_steps: usize,
    /// Step timeout in seconds
    pub step_timeout_secs: u64,
    /// Available tool list
    pub tools: Vec<String>,
    #[serde(default)]
    /// Memory configuration
    pub memory: MemoryConfig,
    /// Output format configuration (optional)
    pub output_format: Option<OutputFormat>,
    /// G2:上下文窗口 token 数(可选,默认 8192)
    ///
    /// 设为 `None` 时使用 `AgentConfig` 默认值。设为 `Some(n)` 时,
    /// `ContextWindowManager` 会按 `n` 裁剪历史消息,保留 system + 最近若干轮。
    /// 预留 1/4 给响应,实际可用输入 = `n - n/4`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<usize>,
    /// 验收判据自检命令（可选；长程/TB 模式）
    ///
    /// 配置后，`task_done` 指令提交前 runner 强制在 shell 执行此命令：
    /// exit 0 = 判据通过（`params.acceptance_passed=true` 放行），
    /// 非 0 = 门禁拒绝提交（LLM 收到失败详情并被强制继续）——
    /// **判据不过不存在 done 退出路径**。未配置时不拦截（非长程运行零影响）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_command: Option<String>,
    /// G13:单轮内并行工具调用上限(可选,默认 1 = 串行)
    ///
    /// - `1`(默认):工具按顺序串行执行(向后兼容旧行为)
    /// - `>1`:同一 ReAct 步骤中多个 active 工具调用并行执行(用 `futures::future::join_all`)
    ///
    /// candidate 工具(需审批)始终串行执行,不受此参数影响。
    /// 设为 `0` 视为 `1`(防御性)。
    #[serde(
        default = "default_max_parallel_tools",
        skip_serializing_if = "is_max_parallel_tools_default"
    )]
    pub max_parallel_tools: usize,
    /// 能力边界声明(可选;agent_def v1.1 增量。None = 未声明,行为同 v1.0:
    /// serve 层按启动配置合成缺省边界)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_boundary: Option<CapabilityBoundary>,
    /// 人工审查开合(审批决策端模式;2026-09-28 立项,agent 定义级,D1 首版粒度)
    ///
    /// - 缺省 None(等价 "manual"):现状行为零变化(candidate 走交互审批)
    /// - "manual":人工审查开启(CLI stdin 交互 / HTTP 60s 审批窗)
    /// - "auto_policy":自动判定模式(无人值守)——candidate proposal 由
    ///   PolicyApproval 判定式决策端自动批准,判断逻辑照跑、理由逐笔留痕入链
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<String>,
    /// 组装配方(可选;元层先行批,agent_def 增量。None = 内置默认配方 =
    /// 现状行为逐字节等价;声明后配方版本/哈希进 effective_params 三阶落账)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assembly: Option<crate::agent::assembly::AssemblyRecipe>,
    /// skills 声明(可选;skills 装配 B2 批,agent_def 增量。None/空 = 不注入
    /// manifest 段、不注册 read_skill——零变化)。声明时刻 = 人工把关,详见
    /// [`SkillEntry`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<SkillEntry>>,
    /// F-101 身份资产段(可选;agent_def 增量。我是谁/服务谁/能力边界自述/
    /// 行为基调,规格成本 ≤300 token;来源=human,权威=指令级,置信=1.0)。
    /// None = S1 槽仅基底块原样(既有定义零变化);声明后 S1 槽内拼接序固定
    /// 为 基底块→身份段(槽位协议权威序),随 system_prompt 落链自然覆盖账面。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_segment: Option<String>,
    /// F-101 北极星锚(可选;agent_def 增量。任务对焦锚——聚焦论供锚面,
    /// 来源=human,权威=指令级)。None = 不注入;声明后 S1 槽内拼接序 =
    /// 基底块→身份段→北极星锚(槽位协议权威序)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub north_star: Option<String>,
    /// 交接底座包(可选;上下文延续归一件。确定性结构字段——上一程任务的
    /// 目标/里程碑/下一步/已验证事实/死路清单,由驱动纯查表生成,零 LLM;
    /// 注入=S3 槽内 "## Handoff Base" 结构化块,与滚动摘要(语义面)分层
    /// 配对、永不合并存储。None = 不注入(既有 agent 零影响)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<HandoffPackage>,
}

/// 交接底座包(上下文延续归一件:确定性包=底座,sidecar 摘要=语义面)。
///
/// 字段语义(与驱动侧 handoff 生成器对齐):
/// - `goal`:总目标(跨程不变);
/// - `milestone_current`:当前里程碑(每程推进后随包 generation 更新);
/// - `next_step`:下一动作;
/// - `verified_facts`:已验证事实逐条(可溯源到账面);
/// - `dead_ends`:死路清单逐条(失败教训强制回喂——只增不删,防重试)。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HandoffPackage {
    /// 总目标
    pub goal: String,
    /// 当前里程碑(可选)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone_current: Option<String>,
    /// 下一动作(可选)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    /// 已验证事实(逐条)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verified_facts: Vec<String>,
    /// 死路清单(逐条;只增不删)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dead_ends: Vec<String>,
}

/// G13:`max_parallel_tools` 的默认值(串行)
fn default_max_parallel_tools() -> usize {
    1
}

/// G13:序列化时若为默认值(1)则跳过
fn is_max_parallel_tools_default(v: &usize) -> bool {
    *v == 1
}

/// 宪法规则集版本(审查结论账面口径;文本面 R3-R5 + 配置面 R6-R9 为第二版
/// 规则集,初版三规则=第一版)。SessionCreated.constitution 落 "pass@<版本>"
/// ——运行期到达该事件的定义必为过审定义(违反在加载期 fail-fast,会话
/// 不会建立)。
pub const CONSTITUTION_RULESET_VERSION: &str = "2";

impl AgentDefinition {
    /// 审查结论账面值(过审凭据;"pass@" + 规则集版本)
    pub fn constitution_pass_mark() -> String {
        format!("pass@{CONSTITUTION_RULESET_VERSION}")
    }

    /// agent_type 标识符白名单:`[A-Za-z0-9_-]+`
    ///
    /// 门卫(P2-M7 前置补丁,2026-08-27):workflow JSON 等外部输入会以
    /// `agent_type` 拼接文件路径加载——含 `/` `\` `..` 的值构成路径穿越,
    /// 可读取盘上任意 .json。加载侧统一在此拒绝。
    pub fn validate_agent_type(agent_type: &str) -> Result<(), AgentDefinitionError> {
        if agent_type.is_empty()
            || !agent_type
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(AgentDefinitionError::InvalidDefinition(format!(
                "invalid agent_type '{}': must match [A-Za-z0-9_-]+ (path traversal guard)",
                agent_type
            )));
        }
        Ok(())
    }

    /// 定义级语义校验(反序列化后的第二道门卫)
    ///
    /// serde 只保证类型正确,不保证取值合理。此处拦截会在 LLM 调用时才爆出的
    /// 错配(如 temperature 越界、max_steps=0),加载期即失败并给出可读原因。
    pub fn validate(&self) -> Result<(), AgentDefinitionError> {
        Self::validate_agent_type(&self.agent_type)?;
        if !(0.0..=2.0).contains(&self.temperature) {
            return Err(AgentDefinitionError::InvalidDefinition(format!(
                "temperature {} out of range [0.0, 2.0]",
                self.temperature
            )));
        }
        if self.max_steps == 0 {
            return Err(AgentDefinitionError::InvalidDefinition(
                "max_steps must be >= 1".to_string(),
            ));
        }
        if self.step_timeout_secs == 0 {
            return Err(AgentDefinitionError::InvalidDefinition(
                "step_timeout_secs must be >= 1".to_string(),
            ));
        }
        // M5-a:capability_boundary 语义门卫(单一事实源 + 模式/工具一致性)
        if let Some(b) = &self.capability_boundary {
            if b.mode != "read_only" && b.mode != "read_write" {
                return Err(AgentDefinitionError::InvalidDefinition(format!(
                    "capability_boundary.mode '{}' must be 'read_only' or 'read_write'",
                    b.mode
                )));
            }
            if b.sandbox_root.as_os_str().is_empty() {
                return Err(AgentDefinitionError::InvalidDefinition(
                    "capability_boundary.sandbox_root must not be empty".to_string(),
                ));
            }
            if !b.sandbox_root.is_absolute() {
                return Err(AgentDefinitionError::InvalidDefinition(format!(
                    "capability_boundary.sandbox_root '{}' must be an absolute path",
                    b.sandbox_root.display()
                )));
            }
            // 单一事实源:顶层 tools 中的沙箱类工具必须被声明覆盖
            // (否则该工具沙箱绑定启动配置、声明形同虚设 → 双源漂移)
            for t in &self.tools {
                if SANDBOX_CAPABLE_TOOLS.contains(&t.as_str()) && !b.tools.contains(t) {
                    return Err(AgentDefinitionError::InvalidDefinition(format!(
                        "tool '{}' is sandbox-capable and must be declared in \
                         capability_boundary.tools (single source of truth)",
                        t
                    )));
                }
            }
            // 只读模式不得授予写工具
            if b.is_read_only() && b.tools.iter().any(|t| t == "file_write") {
                return Err(AgentDefinitionError::InvalidDefinition(
                    "capability_boundary mode 'read_only' cannot grant 'file_write'".to_string(),
                ));
            }
            // 死声明防护:声明的工具必须真实存在于顶层 tools
            for t in &b.tools {
                if !self.tools.contains(t) {
                    return Err(AgentDefinitionError::InvalidDefinition(format!(
                        "capability_boundary lists tool '{}' which is not in the agent's tools",
                        t
                    )));
                }
            }
        }
        // 人工审查开合:approval_mode 取值门卫(未知值加载期即拒,不给运行期惊喜)
        if let Some(m) = &self.approval_mode {
            if m != "manual" && m != "auto_policy" {
                return Err(AgentDefinitionError::InvalidDefinition(format!(
                    "approval_mode '{}' must be 'manual' or 'auto_policy'",
                    m
                )));
            }
        }
        // 元层先行批:assembly 配方语义门卫(槽位 id 唯一/来源白名单/ratio 越界/
        // 降级序/裁剪策略/骨架槽位/全局预算——加载期 fail-fast)
        if let Some(recipe) = &self.assembly {
            recipe.validate().map_err(|e| {
                AgentDefinitionError::InvalidDefinition(format!("assembly recipe invalid: {}", e))
            })?;
        }
        // B2:skills 声明门卫(name 非空 + 全表唯一——read_skill 查表键的合法性;
        // 路径存在性/frontmatter 可解析在 load_from_dir 有目录上下文时校验)
        if let Some(skills) = &self.skills {
            let mut seen = std::collections::HashSet::new();
            for s in skills {
                if s.name.trim().is_empty() {
                    return Err(AgentDefinitionError::InvalidDefinition(
                        "skills.name must not be empty".to_string(),
                    ));
                }
                if !seen.insert(s.name.as_str()) {
                    return Err(AgentDefinitionError::InvalidDefinition(format!(
                        "duplicate skill name '{}'",
                        s.name
                    )));
                }
            }
        }
        Ok(())
    }

    /// F-201:加载时静态宪法审查(06 权威序确定性子集;违反=拒载,错误明示
    /// 规则名)。三规则:
    /// - R1 骨架完整性(仅声明 assembly 配方时):骨架槽位来源绑定——S1_base
    ///   绑 definition.system_prompt、S7_history 绑 messages(id 在位/去重由
    ///   配方 validate 必填检查保证,此处防骨架槽位换源——换源=骨架失效);
    /// - R2 分区序=权威子集(仅声明 assembly 配方时):S1_base < recall <
    ///   awareness_segment < manifest 相对序,缺席合法,逆序拒载(治理对不进
    ///   definition 可声明集——配方层无治理槽,运行时 enforce 归 F-202);
    /// - R3 禁跨区内容混入(全量生效):system_prompt 基底文本禁含机制哨兵
    ///   短语(记忆分区标题/感知段/规范索引文案只允许由机制写入,定义文本
    ///   不得伪造——字面级判定,确定性)。
    ///
    /// 挂点分工(批次 D E2E 实测修订):本方法 = **文件供给侧关口**
    /// (load_from_dir 门卫 4)。运行时构造路径(from_definition)只查
    /// [`Self::validate_assembly_binding`](R1/R2)——serve 面 M1 规范索引/
    /// L2 前馈/进化信号注入先于 from_definition 修改 system_prompt,注入段
    /// 合法含机制哨兵短语,运行时重复 R3 会把机制注入误判为跨区伪造。
    pub fn validate_constitution(&self) -> Result<(), AgentDefinitionError> {
        // R3:机制哨兵短语集合(权威源=context_inspector::MECHANISM_SECTION_MARKERS
        // ——与 I2 检查器分区切分同源;新增机制分区须同步)
        for s in crate::agent::context_inspector::MECHANISM_SECTION_MARKERS {
            if self.system_prompt.contains(s) {
                return Err(AgentDefinitionError::InvalidDefinition(format!(
                    "[R3 cross-zone] system_prompt must not contain mechanism sentinel phrase '{}'",
                    s
                )));
            }
        }
        // R4/R5:身份资产段与北极星锚同族审查——同为定义声明文本(无缺省合成,
        // 声明即生效),机制分区标题只允许机制写入,定义文本不得伪造。
        // 文本面规则只挂文件口(与 R3 同分工:机制注入先于运行时构造,运行时
        // 复查会误判注入段)
        if let Some(seg) = &self.identity_segment {
            for s in crate::agent::context_inspector::MECHANISM_SECTION_MARKERS {
                if seg.contains(s) {
                    return Err(AgentDefinitionError::InvalidDefinition(format!(
                        "[R4 identity sentinel] identity_segment must not contain mechanism sentinel phrase '{}'",
                        s
                    )));
                }
            }
        }
        if let Some(star) = &self.north_star {
            for s in crate::agent::context_inspector::MECHANISM_SECTION_MARKERS {
                if star.contains(s) {
                    return Err(AgentDefinitionError::InvalidDefinition(format!(
                        "[R5 north-star sentinel] north_star must not contain mechanism sentinel phrase '{}'",
                        s
                    )));
                }
            }
        }
        self.validate_assembly_binding()
    }

    /// R6-R9 配置面审查(工具名单形态/记忆配置形态/自省工具矛盾前置)。
    ///
    /// 配置面无机制注入冲突(批次 D 误判教训只涉 system_prompt 文本面),
    /// 文件口与运行时口双查共享本判定(判定代码单一事实源):
    /// - 文件口:validate_constitution(加载期拦,错误早于部署);
    /// - 运行时口:validate_assembly_binding(from_definition 直构路径兜底)。
    fn validate_memory_and_tools(&self) -> Result<(), AgentDefinitionError> {
        // R6:工具名单形态——trim 后非空/不含空白/全表去重(重复声明=静默无效
        // 声明,显性化;保留判定与注册面解耦——存在性归运行时注册检查)
        let mut seen = std::collections::HashSet::new();
        for t in &self.tools {
            if t.trim().is_empty() {
                return Err(AgentDefinitionError::InvalidDefinition(
                    "[R6 tool-list] tool name must not be empty or whitespace-only".to_string(),
                ));
            }
            if t.chars().any(|c| c.is_whitespace()) {
                return Err(AgentDefinitionError::InvalidDefinition(format!(
                    "[R6 tool-list] tool name '{}' must not contain whitespace",
                    t
                )));
            }
            if !seen.insert(t.as_str()) {
                return Err(AgentDefinitionError::InvalidDefinition(format!(
                    "[R6 tool-list] duplicate tool name '{}'",
                    t
                )));
            }
        }
        // R6 续:记忆自省/笔记工具声明×记忆未启用 配置矛盾前置(运行时
        // register_memory_introspection_tools 同判——加载期拒绝更早更清晰)
        let memory_off = self.memory.memory_type.is_empty() || self.memory.memory_type == "none";
        if memory_off {
            if let Some(t) = self
                .tools
                .iter()
                .find(|t| crate::agent::memory_tool::is_registered_memory_tool(t))
            {
                return Err(AgentDefinitionError::InvalidDefinition(format!(
                    "[R6 tool-memory] tool '{}' declared but memory is disabled (requires memory.type=persistent)",
                    t
                )));
            }
        }
        // R7:记忆类型枚举(空串=未声明同 none;判定与运行时装配同源语义)
        let mt = self.memory.memory_type.as_str();
        if !mt.is_empty() && mt != "none" && mt != "persistent" {
            return Err(AgentDefinitionError::InvalidDefinition(format!(
                "[R7 memory-type] unsupported memory.type '{}' (allowed: none|persistent)",
                mt
            )));
        }
        // R8:记忆域形态——仅在记忆启用时有意义(未启用时空域合法:字段不被
        // 消费,存量定义存在此形态,强约束会误伤);shared. 前缀=跨代理显式
        // 共享声明,合法放行
        if !memory_off {
            let ns = self.memory.namespace.as_str();
            if ns.is_empty() {
                return Err(AgentDefinitionError::InvalidDefinition(
                    "[R8 memory-namespace] namespace must not be empty when memory is enabled"
                        .to_string(),
                ));
            }
            if !ns
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
            {
                return Err(AgentDefinitionError::InvalidDefinition(format!(
                    "[R8 memory-namespace] namespace '{}' contains characters outside [a-zA-Z0-9._-]",
                    ns
                )));
            }
        }
        // R9:检索缓存路径形态——声明时非空+禁穿越段(存在性/open 失败维持
        // 运行期 warn 降级语义,加载期只查可静态判定的形态)
        if let Some(db) = &self.memory.lex_store {
            if db.trim().is_empty() {
                return Err(AgentDefinitionError::InvalidDefinition(
                    "[R9 lex-store] lex_store path must not be empty when declared".to_string(),
                ));
            }
            if db.split(['/', '\\']).any(|seg| seg == "..") {
                return Err(AgentDefinitionError::InvalidDefinition(format!(
                    "[R9 lex-store] lex_store path '{}' must not contain '..' traversal segments",
                    db
                )));
            }
        }
        Ok(())
    }

    /// R1/R2 配方绑定审查(运行时构造路径挂点:from_definition)。
    ///
    /// 只查 assembly 配方声明(骨架槽位来源绑定 + 权威分区序),不查 R3——
    /// 机制注入(M1/L2/进化信号)先于运行时构造发生且不触配方,故此路径
    /// 不会误伤注入段;R3 由文件入口([`Self::load_from_dir`])单独把关。
    pub fn validate_assembly_binding(&self) -> Result<(), AgentDefinitionError> {
        // R1/R2:仅对声明配方的定义生效(未声明 = 内置默认配方,骨架/槽序
        // 由代码保证)
        if let Some(recipe) = &self.assembly {
            // R1:骨架完整性——来源绑定(id 在位/去重由配方 validate 保证)
            for (slot_id, required_source) in [
                ("S1_base", "definition.system_prompt"),
                ("S7_history", "messages"),
            ] {
                match recipe.slots.iter().find(|s| s.id == slot_id) {
                    Some(s) if s.source == required_source => {}
                    Some(s) => {
                        return Err(AgentDefinitionError::InvalidDefinition(format!(
                            "[R1 skeleton] slot '{}' must bind source '{}', found '{}'",
                            slot_id, required_source, s.source
                        )));
                    }
                    None => {
                        return Err(AgentDefinitionError::InvalidDefinition(format!(
                            "[R1 skeleton] required slot '{}' missing from assembly",
                            slot_id
                        )));
                    }
                }
            }
            // R2:分区序=权威序(first-occurrence 相对序,缺席合法,逆序拒载)
            let rank = |source: &str| match source {
                "definition.system_prompt" => Some(0u8),
                "recall" => Some(1),
                "definition.capability_boundary.awareness_segment" => Some(2),
                "manifest" => Some(3),
                _ => None,
            };
            let mut prev: Option<(usize, u8)> = None;
            for (i, slot) in recipe.slots.iter().enumerate() {
                if let Some(r) = rank(&slot.source) {
                    if let Some((prev_i, prev_r)) = prev {
                        if r < prev_r {
                            return Err(AgentDefinitionError::InvalidDefinition(format!(
                                "[R2 section order] slot '{}' (index {}) violates authority order S1_base < recall < awareness_segment < manifest (out-of-order slot at index {})",
                                slot.id, i, prev_i
                            )));
                        }
                    }
                    prev = Some((i, r));
                }
            }
        }
        // R6-R9 配置面(双口共享判定;运行时口兜底 from_definition 直构路径)
        self.validate_memory_and_tools()
    }

    /// Load Agent definition from directory
    pub fn load_from_dir(dir: &Path, agent_type: &str) -> Result<Self, AgentDefinitionError> {
        // 门卫 1:agent_type 标识符白名单(路径穿越防护)
        Self::validate_agent_type(agent_type)?;
        let path = dir.join(format!("{}.json", agent_type));
        if !path.exists() {
            return Err(AgentDefinitionError::NotFound(agent_type.to_string()));
        }
        let content = std::fs::read_to_string(&path)?;
        let mut value: serde_json::Value =
            serde_json::from_str(&content).map_err(AgentDefinitionError::Json)?;
        // 元层先行批:assembly $ref 外部配方引用解析(原位替换为内嵌形态,
        // 相对 definition 目录;穿越/绝对路径/混合形态加载期即拒)
        crate::agent::assembly::resolve_assembly_ref(&mut value, dir)
            .map_err(AgentDefinitionError::InvalidDefinition)?;
        // 门卫 2:宪法 jsonschema 全量校验(找不到 schema 时降级为仅门卫 3,tracing 留痕)
        crate::agent::constitution::validate_agent_def(&value).map_err(|errs| {
            AgentDefinitionError::InvalidDefinition(format!(
                "constitution schema violations: {}",
                errs.join("; ")
            ))
        })?;
        // 门卫 3:定义级语义校验(取值范围)
        let mut def: AgentDefinition =
            serde_json::from_value(value.clone()).map_err(AgentDefinitionError::Json)?;
        def.validate()?;
        // 门卫 4(F-201):加载时静态宪法审查(骨架/槽序/跨区哨兵;违反=拒载,
        // 错误明示规则名)
        def.validate_constitution()?;
        // B2:skills 声明 fail-fast(相对路径先按 definition 目录解析为绝对
        // 路径——单一解析点,后续消费方拿到的声明路径全部绝对;再逐条验证
        // 存在/为文件/frontmatter 可解析,启动期拦截不留运行期惊喜)
        if let Some(skills) = &mut def.skills {
            for s in skills.iter_mut() {
                if s.path.is_relative() {
                    s.path = dir.join(&s.path);
                }
            }
            crate::agent::definition::resolve_skill_manifest_entries(skills).map_err(|e| {
                AgentDefinitionError::InvalidDefinition(format!("skills invalid: {}", e))
            })?;
        }
        Ok(def)
    }

    /// List all available Agent types in directory
    pub fn list_available(dir: &Path) -> Result<Vec<String>, AgentDefinitionError> {
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut types = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    types.push(stem.to_string());
                }
            }
        }
        types.sort();
        Ok(types)
    }

    /// Convert to AgentConfig
    pub fn to_agent_config(&self) -> AgentConfig {
        AgentConfig {
            agent_type: self.agent_type.clone(),
            system_prompt: self.system_prompt.clone(),
            model: self.model.clone(),
            temperature: self.temperature,
            max_steps: self.max_steps,
            step_timeout: std::time::Duration::from_secs(self.step_timeout_secs),
            tool_names: self.tools.clone(),
            llm_retry_count: AgentConfig::default().llm_retry_count,
            output_format: self.output_format.clone(),
            // G13:0 视为 1(防御性);None 时用 AgentConfig 默认值
            max_parallel_tools: if self.max_parallel_tools == 0 {
                1
            } else {
                self.max_parallel_tools
            },
            // M5-a:边界声明不在 to_agent_config 复制——生效边界由 serve/CLI 层
            // wire_capability_boundary 统一合成注入(单一事实源,禁双源)
            capability_boundary: None,
            // 元层先行批:配方经 serve/CLI 层 from_definition 注入生效执行器
            // (此处 None = AgentConfig 默认配方语义,消费侧展开为内置默认)
            assembly: None,
            // B2:skills 生效清单不在 to_agent_config 复制——由 serve/CLI 层
            // wire_skills 统一解析注入(单一事实源,与边界同口径)
            skills: None,
            // F-101:身份资产段直拷(定义内容面,与 system_prompt 同族——
            // 无缺省合成,声明即生效)
            identity_segment: self.identity_segment.clone(),
            // F-101:北极星锚直拷(同身份段口径——无缺省合成,声明即生效)
            north_star: self.north_star.clone(),
            // 交接底座包直拷(同口径——声明即生效,S3 槽内渲染)
            handoff: self.handoff.clone(),
            // 治理门禁段不在此复制——serve 三路径构造期经
            // with_governance_segment 注入(非 definition 数据)
            governance_segment: None,
        }
    }
}

/// Agent 瀹氫箟绠＄悊鍣
#[derive(Debug, Clone)]
pub struct AgentDefinitionManager {
    /// Agent 瀹氫箟鏂囦欢鎵€鍦ㄧ洰褰
    agents_dir: PathBuf,
}

impl AgentDefinitionManager {
    /// Create new manager
    pub fn new(agents_dir: PathBuf) -> Self {
        Self { agents_dir }
    }

    /// Create manager with default directory (rules/agents)
    pub fn with_default_dir() -> Self {
        let dir = PathBuf::from("rules/agents");
        Self::new(dir)
    }

    /// Get agents directory path
    pub fn agents_dir(&self) -> &Path {
        &self.agents_dir
    }

    /// Load Agent definition of specified type
    pub fn load(&self, agent_type: &str) -> Result<AgentDefinition, AgentDefinitionError> {
        AgentDefinition::load_from_dir(&self.agents_dir, agent_type)
    }

    /// List all available Agent types
    pub fn list_types(&self) -> Result<Vec<String>, AgentDefinitionError> {
        AgentDefinition::list_available(&self.agents_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create tempdir")
    }

    fn write_json(dir: &Path, name: &str, json: &str) {
        let path = dir.join(format!("{}.json", name));
        let mut f = std::fs::File::create(&path).expect("create file");
        f.write_all(json.as_bytes()).expect("write file");
    }

    /// 组装配方测试的最小合法 definition JSON(骨架字段齐全)
    fn minimal_def_json() -> String {
        r#"{
            "agent_type": "recipe_test",
            "version": "1.0.0",
            "description": "assembly recipe test",
            "system_prompt": "you are a test agent",
            "model": "test-model",
            "temperature": 0.5,
            "max_steps": 5,
            "step_timeout_secs": 30,
            "tools": []
        }"#
        .to_string()
    }

    /// F-101:identity_segment 可选(旧定义 JSON 加载零变化)+声明后加载与
    /// to_agent_config 透传(definition JSON 样例验收)
    #[test]
    fn test_identity_segment_optional_and_propagates() {
        let dir = make_tmp_dir();
        // 无字段:既有定义形态
        write_json(dir.path(), "no_identity", &minimal_def_json());
        let def = AgentDefinition::load_from_dir(dir.path(), "no_identity").expect("load");
        assert!(def.identity_segment.is_none());
        assert!(def.to_agent_config().identity_segment.is_none());
        // 有字段:合法加载 + 直拷透传
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(r#", "identity_segment": "【身份资产】我是 general 助手:服务项目委托者,只读沙箱边界,直接简洁基调"}"#);
        write_json(dir.path(), "with_identity", &json);
        let def2 = AgentDefinition::load_from_dir(dir.path(), "with_identity").expect("load");
        assert_eq!(
            def2.identity_segment.as_deref(),
            Some("【身份资产】我是 general 助手:服务项目委托者,只读沙箱边界,直接简洁基调")
        );
        let cfg = def2.to_agent_config();
        assert_eq!(
            cfg.identity_segment, def2.identity_segment,
            "to_agent_config 直拷(声明即生效,无缺省合成)"
        );
        // C-4:north_star 同批——可选加载+直拷透传(声明即生效)
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(r#", "north_star": "【北极星】以最小上下文获得可靠任务完成"}"#);
        write_json(dir.path(), "with_north_star", &json);
        let def3 = AgentDefinition::load_from_dir(dir.path(), "with_north_star").expect("load");
        assert_eq!(
            def3.north_star.as_deref(),
            Some("【北极星】以最小上下文获得可靠任务完成")
        );
        let cfg3 = def3.to_agent_config();
        assert_eq!(
            cfg3.north_star, def3.north_star,
            "north_star 直拷同身份段口径"
        );
        // 交接底座包同批——可选加载+直拷透传(声明即生效)
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(
            r#", "handoff": {
                "goal": "完成数据管线迁移",
                "milestone_current": "阶段 2 完成",
                "next_step": "跑验收测试",
                "verified_facts": ["构建通过", "单测全绿"],
                "dead_ends": ["方案甲:内存缓存不可回放"]
            }}"#,
        );
        write_json(dir.path(), "with_handoff", &json);
        let def4 = AgentDefinition::load_from_dir(dir.path(), "with_handoff").expect("load");
        let h = def4.handoff.as_ref().expect("handoff present");
        assert_eq!(h.goal, "完成数据管线迁移");
        assert_eq!(h.milestone_current.as_deref(), Some("阶段 2 完成"));
        assert_eq!(h.next_step.as_deref(), Some("跑验收测试"));
        assert_eq!(h.verified_facts.len(), 2);
        assert_eq!(h.dead_ends.len(), 1);
        let cfg4 = def4.to_agent_config();
        assert_eq!(cfg4.handoff, def4.handoff, "handoff 直拷同口径");
    }

    /// 元层先行批:assembly 内嵌段加载(字段正确透传)
    #[test]
    fn test_assembly_inline_loading() {
        let dir = make_tmp_dir();
        let mut json = minimal_def_json();
        json.pop(); // 去掉尾部 '}'
        json.push_str(
            r#", "assembly": {
                "recipe_version": "recipe-v1.0",
                "slots": [
                    { "id": "S1_base", "source": "definition.system_prompt" },
                    { "id": "S5_task", "source": "goal", "role": "user" },
                    { "id": "S7_history", "source": "messages",
                      "trim": { "strategy": "KeepSystemKeepLast", "buffer_pct": 10, "hint_budget_tokens": 20 } }
                ],
                "budget": { "reserve_for_response_pct": 30, "tool_result_max_chars": 9000 }
            }}"#,
        );
        write_json(dir.path(), "recipe_test", &json);
        let def = AgentDefinition::load_from_dir(dir.path(), "recipe_test").expect("load");
        let recipe = def.assembly.expect("assembly present");
        assert_eq!(recipe.budget.reserve_for_response_pct, 30);
        assert_eq!(recipe.budget.tool_result_max_chars, 9000);
        assert_eq!(recipe.slots[2].trim.as_ref().unwrap().buffer_pct, 10);
    }

    /// 元层先行批:$ref 外部配方文件加载(相对 definition 目录解析后等价内嵌)
    #[test]
    fn test_assembly_ref_loading() {
        let dir = make_tmp_dir();
        let recipes = dir.path().join("recipes");
        std::fs::create_dir_all(&recipes).expect("create recipes dir");
        std::fs::write(
            recipes.join("assembly-v1.json"),
            r#"{"recipe_version":"recipe-v1.0","budget":{"reserve_for_response_pct":35}}"#,
        )
        .expect("write recipe file");
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(r#", "assembly": { "$ref": "recipes/assembly-v1.json" }}"#);
        write_json(dir.path(), "recipe_test", &json);
        let def = AgentDefinition::load_from_dir(dir.path(), "recipe_test").expect("load");
        let recipe = def.assembly.expect("assembly present via $ref");
        assert_eq!(recipe.recipe_version, "recipe-v1.0");
        assert_eq!(recipe.budget.reserve_for_response_pct, 35);
    }

    /// 元层先行批:未声明 assembly = None(None 在消费侧展开为内置默认配方)
    #[test]
    fn test_assembly_absent_is_none() {
        let dir = make_tmp_dir();
        write_json(dir.path(), "recipe_test", &minimal_def_json());
        let def = AgentDefinition::load_from_dir(dir.path(), "recipe_test").expect("load");
        assert!(def.assembly.is_none());
    }

    /// 元层先行批:非法配方加载期即拒(ratio 越声明 clamp 区间)
    #[test]
    fn test_assembly_invalid_rejected_at_load() {
        let dir = make_tmp_dir();
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(
            r#", "assembly": {
                "recipe_version": "recipe-v1.0",
                "slots": [
                    { "id": "S1_base", "source": "definition.system_prompt" },
                    { "id": "S5_task", "source": "goal" },
                    { "id": "S7_history", "source": "messages" }
                ]
            }}"#,
        );
        // 篡改 ratio 越界(0.9 越出 [0.1,0.5])
        let bad = json.replace(
            r#""slots": ["#,
            r#""slots": [ { "id": "S3_memory", "source": "recall",
                "budget": { "ratio": 0.9, "base": "total_window", "clamp": [0.1, 0.5] } }, "#,
        );
        write_json(dir.path(), "recipe_test", &bad);
        let err = AgentDefinition::load_from_dir(dir.path(), "recipe_test")
            .expect_err("invalid recipe must be rejected");
        assert!(
            err.to_string().contains("budget.ratio"),
            "unexpected error: {}",
            err
        );
    }

    /// F-201:加载时静态宪法审查——合法样本通过 + 三类违例样本拒载
    /// (错误明示规则名;I1 验收=加载违例拒载实测)
    #[test]
    fn test_constitution_static_review_rejects_and_passes() {
        let dir = make_tmp_dir();
        // 合法样本 1:无配方(旧定义形态,R1/R2 不适用,R3 通过)
        write_json(dir.path(), "plain_ok", &minimal_def_json());
        let def = AgentDefinition::load_from_dir(dir.path(), "plain_ok").expect("plain passes");
        assert!(def.validate_constitution().is_ok());
        // 合法样本 2:全槽配方且槽序合法(S1<recall<awareness<manifest)
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(
            r#", "assembly": {
                "recipe_version": "recipe-v1.0",
                "slots": [
                    { "id": "S1_base", "source": "definition.system_prompt" },
                    { "id": "S3_memory", "source": "recall",
                      "budget": { "ratio": 0.25, "base": "input", "clamp": [0.1, 0.5] } },
                    { "id": "S4_boundary", "source": "definition.capability_boundary.awareness_segment" },
                    { "id": "S4b_skills", "source": "manifest" },
                    { "id": "S5_task", "source": "goal", "role": "user" },
                    { "id": "S7_history", "source": "messages" }
                ]
            }}"#,
        );
        write_json(dir.path(), "legal_recipe", &json);
        let def = AgentDefinition::load_from_dir(dir.path(), "legal_recipe").expect("legal passes");
        assert!(def.validate_constitution().is_ok());

        // R1 违例:骨架槽位换源(S1_base 绑到 goal;id 在位故配方 validate 放行,
        // 由 R1 来源绑定检查拦截)
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(
            r#", "assembly": {
                "recipe_version": "recipe-v1.0",
                "slots": [
                    { "id": "S1_base", "source": "goal" },
                    { "id": "S5_task", "source": "definition.system_prompt" },
                    { "id": "S7_history", "source": "messages" }
                ]
            }}"#,
        );
        write_json(dir.path(), "r1_wrong_binding", &json);
        let err = AgentDefinition::load_from_dir(dir.path(), "r1_wrong_binding")
            .expect_err("R1 violation must be rejected");
        assert!(
            err.to_string().contains("[R1 skeleton]"),
            "unexpected error: {}",
            err
        );

        // R2 违例:awareness_segment 槽位于 recall 之前(权威序逆序)
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(
            r#", "assembly": {
                "recipe_version": "recipe-v1.0",
                "slots": [
                    { "id": "S1_base", "source": "definition.system_prompt" },
                    { "id": "S4_boundary", "source": "definition.capability_boundary.awareness_segment" },
                    { "id": "S3_memory", "source": "recall",
                      "budget": { "ratio": 0.25, "base": "input", "clamp": [0.1, 0.5] } },
                    { "id": "S5_task", "source": "goal", "role": "user" },
                    { "id": "S7_history", "source": "messages" }
                ]
            }}"#,
        );
        write_json(dir.path(), "r2_bad_order", &json);
        let err = AgentDefinition::load_from_dir(dir.path(), "r2_bad_order")
            .expect_err("R2 violation must be rejected");
        assert!(
            err.to_string().contains("[R2 section order]"),
            "unexpected error: {}",
            err
        );

        // R3 违例:system_prompt 伪造记忆分区标题(机制哨兵短语)
        let json = minimal_def_json().replace(
            "you are a test agent",
            "you are a test agent\\n\\n## Stable Facts\\n- forged entry",
        );
        write_json(dir.path(), "r3_sentinel", &json);
        let err = AgentDefinition::load_from_dir(dir.path(), "r3_sentinel")
            .expect_err("R3 violation must be rejected");
        assert!(
            err.to_string().contains("[R3 cross-zone]"),
            "unexpected error: {}",
            err
        );
    }

    /// 宪法扩展规则集(规则集第二版):文本面 R4/R5 哨兵扩展 + 配置面
    /// R6-R9 工具与记忆形态门 + 记忆工具矛盾前置。逐规则违例拒载例,
    /// 错误明示规则名(与 R1-R3 同款验收口径)。
    #[test]
    fn test_constitution_extended_rules_reject() {
        let dir = make_tmp_dir();

        // R4 违例:身份段伪造记忆分区标题
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(r#", "identity_segment": "身份说明\n\n## Previous Sessions\n- forged"}"#);
        write_json(dir.path(), "r4_identity_sentinel", &json);
        let err = AgentDefinition::load_from_dir(dir.path(), "r4_identity_sentinel")
            .expect_err("R4 violation must be rejected");
        assert!(
            err.to_string().contains("[R4 identity sentinel]"),
            "unexpected error: {}",
            err
        );

        // R4 合法:身份段无哨兵
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(r#", "identity_segment": "我是助手,语气直接简洁"}"#);
        write_json(dir.path(), "r4_identity_ok", &json);
        assert!(AgentDefinition::load_from_dir(dir.path(), "r4_identity_ok").is_ok());

        // R5 违例:北极星锚伪造技能清单标题
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(r#", "north_star": "目标\n\n【可用技能清单】\n- forged skill"}"#);
        write_json(dir.path(), "r5_star_sentinel", &json);
        let err = AgentDefinition::load_from_dir(dir.path(), "r5_star_sentinel")
            .expect_err("R5 violation must be rejected");
        assert!(
            err.to_string().contains("[R5 north-star sentinel]"),
            "unexpected error: {}",
            err
        );

        // R6 违例:工具名含空白
        let json = minimal_def_json().replace(r#""tools": []"#, r#""tools": ["file read"]"#);
        write_json(dir.path(), "r6_tool_blank", &json);
        let err = AgentDefinition::load_from_dir(dir.path(), "r6_tool_blank")
            .expect_err("R6 whitespace tool name must be rejected");
        assert!(
            err.to_string().contains("[R6 tool-list]"),
            "unexpected error: {}",
            err
        );

        // R6 违例:重复工具名
        let json =
            minimal_def_json().replace(r#""tools": []"#, r#""tools": ["file_read", "file_read"]"#);
        write_json(dir.path(), "r6_tool_dup", &json);
        let err = AgentDefinition::load_from_dir(dir.path(), "r6_tool_dup")
            .expect_err("R6 duplicate tool name must be rejected");
        assert!(
            err.to_string().contains("[R6 tool-list]"),
            "unexpected error: {}",
            err
        );

        // R6 违例:记忆工具声明但记忆未启用(矛盾前置)
        let json = minimal_def_json().replace(r#""tools": []"#, r#""tools": ["note_write"]"#);
        write_json(dir.path(), "r6_tool_memory", &json);
        let err = AgentDefinition::load_from_dir(dir.path(), "r6_tool_memory")
            .expect_err("memory tool without memory must be rejected");
        assert!(
            err.to_string().contains("[R6 tool-memory]"),
            "unexpected error: {}",
            err
        );

        // R7 违例:非法记忆类型——文件口由门卫 2 宪法 schema 先拒(枚举面已有
        // 覆盖);本规则运行时口兜底直构路径,故此处走 serde 直构+运行时口断言
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(r#", "memory": { "type": "weird", "namespace": "ns" }}"#);
        let def: AgentDefinition = serde_json::from_str(&json).expect("parse");
        let err = def
            .validate_assembly_binding()
            .expect_err("R7 runtime gate must reject");
        assert!(
            err.to_string().contains("[R7 memory-type]"),
            "unexpected error: {}",
            err
        );

        // R8 违例:记忆启用但 namespace 为空
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(r#", "memory": { "type": "persistent", "namespace": "" }}"#);
        write_json(dir.path(), "r8_ns_empty", &json);
        let err = AgentDefinition::load_from_dir(dir.path(), "r8_ns_empty")
            .expect_err("R8 empty namespace must be rejected");
        assert!(
            err.to_string().contains("[R8 memory-namespace]"),
            "unexpected error: {}",
            err
        );

        // R8 合法:记忆未启用时空域放行(存量定义形态,字段不被消费)
        write_json(dir.path(), "r8_ns_off_ok", &minimal_def_json());
        assert!(AgentDefinition::load_from_dir(dir.path(), "r8_ns_off_ok").is_ok());

        // R8 合法:shared. 前缀=跨代理显式共享声明
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(r#", "memory": { "type": "persistent", "namespace": "shared.team-facts" }}"#);
        write_json(dir.path(), "r8_ns_shared", &json);
        assert!(AgentDefinition::load_from_dir(dir.path(), "r8_ns_shared").is_ok());

        // R9 违例:检索缓存路径穿越段
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(
            r#", "memory": { "type": "persistent", "namespace": "ns", "lex_store": "data/../evil/x.db" }}"#,
        );
        write_json(dir.path(), "r9_lex_traversal", &json);
        let err = AgentDefinition::load_from_dir(dir.path(), "r9_lex_traversal")
            .expect_err("R9 violation must be rejected");
        assert!(
            err.to_string().contains("[R9 lex-store]"),
            "unexpected error: {}",
            err
        );
    }

    /// 双口一致性:from_definition 直构路径(运行时口)对配置面违规同判早失败
    /// (validate_assembly_binding 复用同一判定;文本面 R4/R5 不进运行时口)
    #[test]
    fn test_constitution_runtime_gate_memory_and_tools() {
        let mut def: AgentDefinition = serde_json::from_str(&minimal_def_json()).expect("parse");
        // 运行时口基线:合法定义过审
        assert!(def.validate_assembly_binding().is_ok());
        // 配置面违例:运行时口同判
        def.tools = vec!["note_write".to_string()];
        let err = def
            .validate_assembly_binding()
            .expect_err("runtime gate must reject memory tool without memory");
        assert!(
            err.to_string().contains("[R6 tool-memory]"),
            "unexpected error: {}",
            err
        );
        // 文本面不进运行时口:身份段含哨兵在运行时口放行(文件口把关)
        let mut def2: AgentDefinition = serde_json::from_str(&minimal_def_json()).expect("parse");
        def2.identity_segment = Some("x\n\n## Stable Facts\n- f".to_string());
        assert!(
            def2.validate_assembly_binding().is_ok(),
            "text-face rules stay file-side (mechanism-injection lesson)"
        );
        assert!(def2.validate_constitution().is_err());
    }

    /// 存量定义全过审(向后兼容机器证明):agents/ 目录现存定义逐个
    /// 加载+过审,新规则集零误伤
    #[test]
    fn test_constitution_all_stock_definitions_pass() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("agents");
        let types = std::fs::read_dir(&dir)
            .expect("agents dir present")
            .filter_map(|e| {
                let p = e.ok()?.path();
                (p.extension().and_then(|x| x.to_str()) == Some("json"))
                    .then(|| p.file_stem().unwrap().to_string_lossy().to_string())
            })
            .collect::<Vec<_>>();
        assert!(
            types.len() >= 6,
            "stock definitions expected, found: {:?}",
            types
        );
        for t in types {
            let def = AgentDefinition::load_from_dir(&dir, &t)
                .unwrap_or_else(|e| panic!("stock def '{}' must load: {}", t, e));
            assert!(
                def.validate_constitution().is_ok(),
                "stock def '{}' must pass constitution",
                t
            );
        }
    }

    /// 审查结论账面值口径:过审凭据 = "pass@" + 规则集版本
    #[test]
    fn test_constitution_pass_mark_format() {
        let mark = AgentDefinition::constitution_pass_mark();
        assert!(mark.starts_with("pass@"), "got: {}", mark);
    }

    #[test]
    fn test_agent_definition_deserialize() {
        let json = r#"{
            "agent_type": "researcher",
            "version": "1.0.0",
            "description": "Research agent",
            "system_prompt": "You are a research agent",
            "model": "gpt-4o-mini",
            "temperature": 0.3,
            "max_steps": 20,
            "step_timeout_secs": 60,
            "tools": ["search_web", "read_file"],
            "memory": { "type": "file", "namespace": "researcher" },
            "output_format": { "type": "json", "schema": {"summary": "string"} }
        }"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert_eq!(def.agent_type, "researcher");
        assert_eq!(def.version, "1.0.0");
        assert_eq!(def.model, "gpt-4o-mini");
        assert!((def.temperature - 0.3).abs() < 0.01);
        assert_eq!(def.max_steps, 20);
        assert_eq!(def.step_timeout_secs, 60);
        assert_eq!(def.tools, vec!["search_web", "read_file"]);
        assert_eq!(def.memory.memory_type, "file");
        assert_eq!(def.memory.namespace, "researcher");
        assert!(def.output_format.is_some());
        assert_eq!(def.output_format.as_ref().unwrap().format_type, "json");
    }

    #[test]
    fn test_agent_definition_default_memory() {
        let json = r#"{
            "agent_type": "simple",
            "version": "1.0.0",
            "description": "绠€鍗?Agent",
            "system_prompt": "浣犲ソ",
            "model": "gpt-4o-mini",
            "temperature": 0.7,
            "max_steps": 10,
            "step_timeout_secs": 30,
            "tools": [],
            "output_format": null
        }"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert_eq!(def.memory.memory_type, "none");
        assert!(def.output_format.is_none());
    }

    #[test]
    fn test_load_from_dir() {
        let dir = make_tmp_dir();
        let json = r#"{
            "agent_type": "test_agent",
            "version": "2.0.0",
            "description": "娴嬭瘯",
            "system_prompt": "test",
            "model": "gpt-4",
            "temperature": 0.5,
            "max_steps": 5,
            "step_timeout_secs": 10,
            "tools": ["echo"],
            "output_format": null
        }"#;
        write_json(dir.path(), "test_agent", json);

        let def = AgentDefinition::load_from_dir(dir.path(), "test_agent").expect("load");
        assert_eq!(def.agent_type, "test_agent");
        assert_eq!(def.version, "2.0.0");
    }

    #[test]
    fn test_load_from_dir_not_found() {
        let dir = make_tmp_dir();
        let result = AgentDefinition::load_from_dir(dir.path(), "nonexistent");
        assert!(matches!(result, Err(AgentDefinitionError::NotFound(_))));
    }

    // ===== 门卫负向用例(P2-M7 前置补丁,2026-08-27) =====

    #[test]
    fn test_load_rejects_path_traversal_agent_type() {
        let dir = make_tmp_dir();
        for bad in ["../general", "a/b", "a\\b", "..", ""] {
            let result = AgentDefinition::load_from_dir(dir.path(), bad);
            match result {
                Err(AgentDefinitionError::InvalidDefinition(msg)) => {
                    assert!(msg.contains("path traversal") || msg.contains("invalid agent_type"));
                }
                other => panic!(
                    "agent_type {:?} not rejected as InvalidDefinition: {:?}",
                    bad, other
                ),
            }
        }
    }

    #[test]
    fn test_validate_rejects_temperature_out_of_range() {
        let mk = |t: f32| {
            r#"{"agent_type":"x","version":"1","description":"","system_prompt":"","model":"m","temperature":"#
                .to_string()
                + &t.to_string()
                + r#","max_steps":1,"step_timeout_secs":1,"tools":[],"output_format":null}"#
        };
        for t in [-0.5_f32, 2.5_f32] {
            let def: AgentDefinition = serde_json::from_str(&mk(t)).expect("parse");
            let err = def.validate().unwrap_err();
            assert!(
                err.to_string().contains("temperature"),
                "temperature {} not rejected: {}",
                t,
                err
            );
        }
        // 边界值合法
        for t in [0.0_f32, 2.0_f32] {
            let def: AgentDefinition = serde_json::from_str(&mk(t)).expect("parse");
            assert!(def.validate().is_ok());
        }
    }

    #[test]
    fn test_validate_rejects_zero_max_steps_and_timeout() {
        let json = r#"{"agent_type":"x","version":"1","description":"","system_prompt":"","model":"m","temperature":0.5,"max_steps":0,"step_timeout_secs":1,"tools":[],"output_format":null}"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert!(def
            .validate()
            .unwrap_err()
            .to_string()
            .contains("max_steps"));

        let json = r#"{"agent_type":"x","version":"1","description":"","system_prompt":"","model":"m","temperature":0.5,"max_steps":1,"step_timeout_secs":0,"tools":[],"output_format":null}"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert!(def
            .validate()
            .unwrap_err()
            .to_string()
            .contains("step_timeout_secs"));
    }

    #[test]
    fn test_validate_accepts_production_agents() {
        // 三个生产 agent 定义必须通过门卫(防门卫误伤真资产)
        let dir = PathBuf::from("agents");
        if !dir.exists() {
            return; // 非仓根运行时跳过
        }
        for name in ["general", "researcher", "rule-copilot"] {
            let def = AgentDefinition::load_from_dir(&dir, name)
                .unwrap_or_else(|e| panic!("production agent '{}' rejected: {}", name, e));
            assert!(def.validate().is_ok());
        }
    }

    #[test]
    fn test_list_available() {
        let dir = make_tmp_dir();
        write_json(
            dir.path(),
            "alpha",
            r#"{"agent_type":"alpha","version":"1","description":"","system_prompt":"","model":"","temperature":0.5,"max_steps":1,"step_timeout_secs":1,"tools":[],"output_format":null}"#,
        );
        write_json(
            dir.path(),
            "beta",
            r#"{"agent_type":"beta","version":"1","description":"","system_prompt":"","model":"","temperature":0.5,"max_steps":1,"step_timeout_secs":1,"tools":[],"output_format":null}"#,
        );
        std::fs::write(dir.path().join("readme.txt"), "hello").unwrap();

        let types = AgentDefinition::list_available(dir.path()).expect("list");
        assert_eq!(types.len(), 2);
        assert_eq!(types[0], "alpha");
        assert_eq!(types[1], "beta");
    }

    #[test]
    fn test_list_available_empty_dir() {
        let dir = make_tmp_dir();
        let types = AgentDefinition::list_available(dir.path()).expect("list");
        assert!(types.is_empty());
    }

    #[test]
    fn test_list_available_nonexistent_dir() {
        let types = AgentDefinition::list_available(Path::new("/nonexistent/path/xyz"))
            .expect("nonexistent dir returns empty");
        assert!(types.is_empty());
    }

    #[test]
    fn test_to_agent_config() {
        let def = AgentDefinition {
            agent_type: "writer".to_string(),
            version: "1.0.0".to_string(),
            description: "Writer agent".to_string(),
            system_prompt: "You are a writing assistant".to_string(),
            model: "gpt-4o".to_string(),
            temperature: 0.8,
            max_steps: 15,
            step_timeout_secs: 45,
            tools: vec!["write_file".to_string()],
            memory: MemoryConfig::default(),
            output_format: None,
            context_window_tokens: None,
            acceptance_command: None,
            max_parallel_tools: 1,
            capability_boundary: None,
            approval_mode: None,
            assembly: None,
            skills: None,
            identity_segment: None,
            north_star: None,
            handoff: None,
        };
        let config = def.to_agent_config();
        assert_eq!(config.agent_type, "writer");
        assert_eq!(config.system_prompt, "You are a writing assistant");
        assert_eq!(config.model, "gpt-4o");
        assert!((config.temperature - 0.8).abs() < 0.01);
        assert_eq!(config.max_steps, 15);
        assert_eq!(config.step_timeout, std::time::Duration::from_secs(45));
        assert_eq!(config.tool_names, vec!["write_file"]);
        assert_eq!(config.max_parallel_tools, 1);
    }

    #[test]
    fn test_definition_manager() {
        let dir = make_tmp_dir();
        write_json(
            dir.path(),
            "researcher",
            r#"{"agent_type":"researcher","version":"1.0.0","description":"test researcher","system_prompt":"test","model":"gpt-4","temperature":0.3,"max_steps":10,"step_timeout_secs":30,"tools":[],"output_format":null}"#,
        );
        let mgr = AgentDefinitionManager::new(dir.path().to_path_buf());
        let types = mgr.list_types().expect("list");
        assert_eq!(types, vec!["researcher"]);
        let def = mgr.load("researcher").expect("load");
        assert_eq!(def.agent_type, "researcher");
    }

    #[test]
    fn test_definition_error_display() {
        let err = AgentDefinitionError::NotFound("foo".to_string());
        assert!(format!("{}", err).contains("foo"));

        let err = AgentDefinitionError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file missing",
        ));
        assert!(format!("{}", err).contains("IO error"));

        let err = AgentDefinitionError::Json(
            serde_json::from_str::<serde_json::Value>("bad").unwrap_err(),
        );
        assert!(format!("{}", err).contains("JSON parse error"));
    }

    // ===== P0: MessagePersistConfig 测试（用户决策 2）=====

    #[test]
    fn test_message_persist_config_default() {
        let cfg = MessagePersistConfig::default();
        assert_eq!(cfg.mode, "every_message");
        assert!(cfg.n.is_none());
    }

    #[test]
    fn test_message_persist_config_to_mode_every_message() {
        let cfg = MessagePersistConfig::default();
        let mode = cfg.to_mode().expect("every_message");
        assert_eq!(mode, crate::agent::memory::MessagePersistMode::EveryMessage);
    }

    #[test]
    fn test_message_persist_config_to_mode_every_n() {
        let cfg = MessagePersistConfig {
            mode: "every_n".to_string(),
            n: Some(5),
        };
        let mode = cfg.to_mode().expect("every_n");
        assert_eq!(mode, crate::agent::memory::MessagePersistMode::EveryN(5));
    }

    #[test]
    fn test_message_persist_config_to_mode_per_react_round() {
        let cfg = MessagePersistConfig {
            mode: "per_react_round".to_string(),
            n: None,
        };
        let mode = cfg.to_mode().expect("per_react_round");
        assert_eq!(
            mode,
            crate::agent::memory::MessagePersistMode::PerReactRound
        );
    }

    #[test]
    fn test_message_persist_config_to_mode_disabled() {
        let cfg = MessagePersistConfig {
            mode: "disabled".to_string(),
            n: None,
        };
        let mode = cfg.to_mode().expect("disabled");
        assert_eq!(mode, crate::agent::memory::MessagePersistMode::Disabled);
    }

    #[test]
    fn test_message_persist_config_to_mode_unknown_errors() {
        let cfg = MessagePersistConfig {
            mode: "magic".to_string(),
            n: None,
        };
        let result = cfg.to_mode();
        assert!(result.is_err());
        let err_msg = result.unwrap_err();
        assert!(err_msg.contains("unknown message_persist mode"));
        assert!(err_msg.contains("magic"));
    }

    #[test]
    fn test_message_persist_config_to_mode_every_n_zero_errors() {
        let cfg = MessagePersistConfig {
            mode: "every_n".to_string(),
            n: Some(0),
        };
        let result = cfg.to_mode();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be > 0"));
    }

    #[test]
    fn test_message_persist_config_to_mode_every_n_missing_n_errors() {
        let cfg = MessagePersistConfig {
            mode: "every_n".to_string(),
            n: None,
        };
        let result = cfg.to_mode();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be > 0"));
    }

    #[test]
    fn test_message_persist_config_deserialize_every_n() {
        let json = r#"{"mode":"every_n","n":10}"#;
        let cfg: MessagePersistConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(cfg.mode, "every_n");
        assert_eq!(cfg.n, Some(10));
    }

    #[test]
    fn test_message_persist_config_deserialize_skips_n_when_none() {
        let json = r#"{"mode":"per_react_round"}"#;
        let cfg: MessagePersistConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(cfg.mode, "per_react_round");
        assert!(cfg.n.is_none());

        // 序列化时也应跳过 n 字段（注意不能用 contains("n")，
        // 因为 "per_react_round" 字符串本身包含字母 n）
        let serialized = serde_json::to_string(&cfg).expect("serialize");
        assert!(!serialized.contains("\"n\""));
    }

    // ===== P0: MemoryConfig 扩展字段测试 =====

    #[test]
    fn test_memory_config_default_includes_new_fields() {
        let cfg = MemoryConfig::default();
        assert_eq!(cfg.memory_type, "none");
        assert_eq!(cfg.namespace, "");
        assert_eq!(cfg.message_persist.mode, "every_message");
        assert!(cfg.ttl_secs.is_none());
        assert!(cfg.summary_model.is_none());
    }

    #[test]
    fn test_memory_config_backward_compat_without_new_fields() {
        // 旧版 agent.json 不包含 message_persist/ttl_secs/summary_model
        // 应该能反序列化并用默认值填充
        let json = r#"{"type":"persistent","namespace":"researcher"}"#;
        let cfg: MemoryConfig = serde_json::from_str(json).expect("parse old format");
        assert_eq!(cfg.memory_type, "persistent");
        assert_eq!(cfg.namespace, "researcher");
        assert_eq!(cfg.message_persist.mode, "every_message");
        assert!(cfg.ttl_secs.is_none());
        assert!(cfg.summary_model.is_none());
    }

    #[test]
    fn test_memory_config_with_all_new_fields() {
        let json = r#"{
            "type": "persistent",
            "namespace": "researcher",
            "message_persist": { "mode": "every_n", "n": 3 },
            "ttl_secs": 3600,
            "summary_model": "gpt-4o-mini"
        }"#;
        let cfg: MemoryConfig = serde_json::from_str(json).expect("parse full");
        assert_eq!(cfg.memory_type, "persistent");
        assert_eq!(cfg.namespace, "researcher");
        assert_eq!(cfg.message_persist.mode, "every_n");
        assert_eq!(cfg.message_persist.n, Some(3));
        assert_eq!(cfg.ttl_secs, Some(3600));
        assert_eq!(cfg.summary_model, Some("gpt-4o-mini".to_string()));

        // 验证 to_mode 解析正确
        let mode = cfg.message_persist.to_mode().expect("to_mode");
        assert_eq!(mode, crate::agent::memory::MessagePersistMode::EveryN(3));
    }

    #[test]
    fn test_memory_config_skips_none_fields_when_serializing() {
        let cfg = MemoryConfig::default();
        let json = serde_json::to_string(&cfg).expect("serialize");
        // ttl_secs 和 summary_model 是 None，不应出现在序列化结果中
        assert!(!json.contains("ttl_secs"));
        assert!(!json.contains("summary_model"));
        // message_persist 有默认值，应该出现
        assert!(json.contains("message_persist"));
    }

    #[test]
    fn test_memory_config_ttl_user_decision_5() {
        // 用户决策 5：TTL 由 agent.json 配置
        let json = r#"{
            "type": "persistent",
            "namespace": "researcher",
            "ttl_secs": 86400
        }"#;
        let cfg: MemoryConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(cfg.ttl_secs, Some(86400));
    }

    #[test]
    fn test_memory_config_summary_model_user_decision_3() {
        // 用户决策 3：摘要模型单独配置
        let json = r#"{
            "type": "persistent",
            "namespace": "researcher",
            "summary_model": "minimax/abab6.5s"
        }"#;
        let cfg: MemoryConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(cfg.summary_model, Some("minimax/abab6.5s".to_string()));
    }

    #[test]
    fn test_agent_definition_with_extended_memory_config() {
        // 完整的 agent.json 集成测试：包含所有新字段
        let json = r#"{
            "agent_type": "researcher",
            "version": "1.0.0",
            "description": "Research agent with memory",
            "system_prompt": "You are a research agent",
            "model": "gpt-4o-mini",
            "temperature": 0.3,
            "max_steps": 20,
            "step_timeout_secs": 60,
            "tools": ["search_web"],
            "memory": {
                "type": "persistent",
                "namespace": "researcher",
                "message_persist": { "mode": "per_react_round" },
                "ttl_secs": 7200,
                "summary_model": "gpt-4o-mini"
            },
            "output_format": null
        }"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert_eq!(def.memory.memory_type, "persistent");
        assert_eq!(def.memory.message_persist.mode, "per_react_round");
        assert_eq!(def.memory.ttl_secs, Some(7200));
        assert_eq!(def.memory.summary_model, Some("gpt-4o-mini".to_string()));

        let mode = def.memory.message_persist.to_mode().expect("to_mode");
        assert_eq!(
            mode,
            crate::agent::memory::MessagePersistMode::PerReactRound
        );
    }

    // ===== M5-a: capability_boundary 测试 =====

    #[test]
    fn test_capability_boundary_serde_roundtrip_and_skip() {
        // None(缺省)时序列化不出现该键(v1.0 存量文档零迁移)
        let json = r#"{
            "agent_type": "x", "version": "1", "description": "",
            "system_prompt": "", "model": "m", "temperature": 0.5,
            "max_steps": 1, "step_timeout_secs": 1, "tools": [],
            "output_format": null
        }"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert!(def.capability_boundary.is_none());
        let ser = serde_json::to_string(&def).expect("serialize");
        assert!(
            !ser.contains("capability_boundary"),
            "None 应被 skip: {}",
            ser
        );

        // 声明存在时往返保真(sandbox_root 取平台合法绝对路径,门卫要求绝对路径)
        let abs_root = if cfg!(windows) {
            "D:/evo-agent"
        } else {
            "/tmp/evo-agent"
        };
        let json2 = format!(
            r#"{{
            "agent_type": "x", "version": "1", "description": "",
            "system_prompt": "", "model": "m", "temperature": 0.5,
            "max_steps": 1, "step_timeout_secs": 1,
            "tools": ["file_read"],
            "output_format": null,
            "capability_boundary": {{
                "mode": "read_only",
                "sandbox_root": "{}",
                "tools": ["file_read"]
            }}
        }}"#,
            abs_root
        );
        let def2: AgentDefinition = serde_json::from_str(&json2).expect("parse");
        let b = def2.capability_boundary.as_ref().expect("declared");
        assert!(b.is_read_only());
        assert_eq!(b.tools, vec!["file_read".to_string()]);
        assert_eq!(b.sandbox_root, PathBuf::from(abs_root));
        assert!(
            def2.validate().is_ok(),
            "合法声明应通过门卫: {:?}",
            def2.validate()
        );
    }

    #[test]
    fn test_capability_boundary_validate_rejections() {
        let mk = |mode: &str, root: &str, tools: Vec<&str>, btools: Vec<&str>| AgentDefinition {
            agent_type: "x".to_string(),
            version: "1".to_string(),
            description: String::new(),
            system_prompt: String::new(),
            model: "m".to_string(),
            temperature: 0.5,
            max_steps: 1,
            step_timeout_secs: 1,
            tools: tools.into_iter().map(String::from).collect(),
            memory: MemoryConfig::default(),
            output_format: None,
            context_window_tokens: None,
            acceptance_command: None,
            max_parallel_tools: 1,
            capability_boundary: Some(CapabilityBoundary {
                mode: mode.to_string(),
                sandbox_root: PathBuf::from(root),
                tools: btools.into_iter().map(String::from).collect(),
            }),
            approval_mode: None,
            assembly: None,
            skills: None,
            identity_segment: None,
            north_star: None,
            handoff: None,
        };
        // 平台合法绝对路径(Linux 上 "D:/x" 非绝对路径,门卫语义会被绝对路径检查劫持)
        let abs_root = if cfg!(windows) { "D:/x" } else { "/x" };
        // mode 取值越界
        let e = mk("read_all", abs_root, vec!["file_read"], vec!["file_read"]);
        assert!(e.validate().is_err());
        // sandbox_root 相对路径
        let e = mk(
            "read_only",
            "relative/dir",
            vec!["file_read"],
            vec!["file_read"],
        );
        assert!(e.validate().is_err());
        // 单一事实源:顶层 tools 有沙箱类工具但声明未覆盖
        let e = mk("read_only", abs_root, vec!["file_read"], vec![]);
        assert!(e.validate().is_err());
        // read_only 授予写工具
        let e = mk(
            "read_only",
            abs_root,
            vec!["file_write"],
            vec!["file_write"],
        );
        assert!(e.validate().is_err());
        // 死声明:声明的工具不在顶层 tools
        let e = mk(
            "read_only",
            abs_root,
            vec!["file_read"],
            vec!["file_read", "file_write"],
        );
        assert!(e.validate().is_err());
        // 合法:read_write 覆盖双沙箱工具
        let ok = mk(
            "read_write",
            abs_root,
            vec!["file_read", "file_write"],
            vec!["file_read", "file_write"],
        );
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn test_capability_boundary_to_json_and_segment() {
        let b = CapabilityBoundary {
            mode: "read_only".to_string(),
            sandbox_root: PathBuf::from("D:/evo-agent"),
            tools: vec!["file_read".to_string()],
        };
        // 会话事实形态:包裹键 capability_boundary(server initial_content 载体)
        let j = b.to_json();
        assert_eq!(j["capability_boundary"]["mode"], "read_only");
        assert_eq!(j["capability_boundary"]["sandbox_root"], "D:/evo-agent");
        // 系统级边界段:首要读者 LLM 自知——模式/根/工具三要素齐备;
        // 第二块=默认会话交接协议(自主交接设计 PR-H1,映射 S4_boundary 槽位)
        let seg = b.awareness_segment();
        assert!(seg.contains("能力边界声明"));
        assert!(seg.contains("read_only"));
        assert!(seg.contains("D:/evo-agent"));
        assert!(seg.contains("file_read"));
        assert!(seg.contains("会话交接协议"));
        assert!(seg.contains("session_spawn"));
        assert!(seg.contains("踩坑清单"));
        assert!(seg.contains("续接判据"));
    }

    // ===== G13: max_parallel_tools 测试 =====

    #[test]
    fn test_max_parallel_tools_defaults_to_1() {
        // 旧版 agent.json 不含 max_parallel_tools,应默认 1(串行)
        let json = r#"{
            "agent_type": "simple",
            "version": "1.0.0",
            "description": "",
            "system_prompt": "",
            "model": "gpt-4o-mini",
            "temperature": 0.5,
            "max_steps": 10,
            "step_timeout_secs": 30,
            "tools": [],
            "output_format": null
        }"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert_eq!(def.max_parallel_tools, 1, "default should be 1 (serial)");
    }

    #[test]
    fn test_max_parallel_tools_custom_value() {
        let json = r#"{
            "agent_type": "parallel",
            "version": "1.0.0",
            "description": "",
            "system_prompt": "",
            "model": "gpt-4o-mini",
            "temperature": 0.5,
            "max_steps": 10,
            "step_timeout_secs": 30,
            "tools": [],
            "output_format": null,
            "max_parallel_tools": 4
        }"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert_eq!(def.max_parallel_tools, 4);

        let config = def.to_agent_config();
        assert_eq!(config.max_parallel_tools, 4);
    }

    #[test]
    fn test_max_parallel_tools_zero_treated_as_one() {
        // 0 视为 1(防御性)
        let json = r#"{
            "agent_type": "zero",
            "version": "1.0.0",
            "description": "",
            "system_prompt": "",
            "model": "gpt-4o-mini",
            "temperature": 0.5,
            "max_steps": 10,
            "step_timeout_secs": 30,
            "tools": [],
            "output_format": null,
            "max_parallel_tools": 0
        }"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert_eq!(def.max_parallel_tools, 0, "raw value preserved as 0");
        // to_agent_config 应把 0 规范化为 1
        let config = def.to_agent_config();
        assert_eq!(config.max_parallel_tools, 1, "0 should be normalized to 1");
    }

    #[test]
    fn test_max_parallel_tools_skipped_when_default_in_serialization() {
        let def = AgentDefinition {
            agent_type: "x".to_string(),
            version: "1".to_string(),
            description: String::new(),
            system_prompt: String::new(),
            model: "m".to_string(),
            temperature: 0.5,
            max_steps: 1,
            step_timeout_secs: 1,
            tools: vec![],
            memory: MemoryConfig::default(),
            output_format: None,
            context_window_tokens: None,
            acceptance_command: None,
            max_parallel_tools: 1,
            capability_boundary: None,
            approval_mode: None,
            assembly: None,
            skills: None,
            identity_segment: None,
            north_star: None,
            handoff: None,
        };
        let json = serde_json::to_string(&def).expect("serialize");
        assert!(
            !json.contains("max_parallel_tools"),
            "default value should be skipped: {}",
            json
        );
    }

    // ===== C3/C4: MemoryConfig 新增字段默认值测试 =====

    #[test]
    fn test_memory_config_defaults() {
        let cfg = MemoryConfig::default();
        assert!((cfg.memory_budget_ratio - 0.25).abs() < 1e-6);
        assert_eq!(cfg.max_session_summaries, 3);
        assert_eq!(cfg.max_injected_events, 5);
        assert_eq!(cfg.summary_rollup_threshold, 10);
        assert!(cfg.enable_event_extraction);
    }

    #[test]
    fn test_memory_config_c3_c4_fields_backward_compat() {
        // 旧版 agent.json 不含 C3/C4 字段，反序列化应使用默认值
        let json = r#"{"type":"persistent","namespace":"researcher"}"#;
        let cfg: MemoryConfig = serde_json::from_str(json).expect("parse");
        assert!((cfg.memory_budget_ratio - 0.25).abs() < 1e-6);
        assert_eq!(cfg.max_session_summaries, 3);
        assert_eq!(cfg.max_injected_events, 5);
        assert_eq!(cfg.summary_rollup_threshold, 10);
        assert!(cfg.enable_event_extraction);
    }

    // ===== 人工审查开合:approval_mode 测试 =====

    #[test]
    fn test_approval_mode_defaults_to_none() {
        // 旧版 agent.json 不含 approval_mode → None(manual 态,现状行为零变化)
        let json = r#"{
            "agent_type": "x", "version": "1", "description": "",
            "system_prompt": "", "model": "m", "temperature": 0.5,
            "max_steps": 1, "step_timeout_secs": 1, "tools": [],
            "output_format": null
        }"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert!(def.approval_mode.is_none());
        // 缺省序列化不出现该键(存量定义零迁移)
        let ser = serde_json::to_string(&def).expect("serialize");
        assert!(!ser.contains("approval_mode"));
    }

    #[test]
    fn test_approval_mode_auto_policy_accepted_and_validated() {
        let json = r#"{
            "agent_type": "x", "version": "1", "description": "",
            "system_prompt": "", "model": "m", "temperature": 0.5,
            "max_steps": 1, "step_timeout_secs": 1, "tools": [],
            "output_format": null,
            "approval_mode": "auto_policy"
        }"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert_eq!(def.approval_mode.as_deref(), Some("auto_policy"));
        assert!(def.validate().is_ok());

        let mut manual = def.clone();
        manual.approval_mode = Some("manual".to_string());
        assert!(manual.validate().is_ok());

        // 未知值:加载期门卫即拒
        let mut bad = def;
        bad.approval_mode = Some("yolo".to_string());
        let err = bad.validate().unwrap_err();
        assert!(err.to_string().contains("approval_mode"));
    }

    // =========================================================================
    // skills 装配 B2 批(声明字段/frontmatter 解析/加载门卫)
    // =========================================================================

    fn write_skill_md(dir: &Path, rel: &str, body: &str) -> PathBuf {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
        let mut f = std::fs::File::create(&path).expect("create file");
        f.write_all(body.as_bytes()).expect("write file");
        path
    }

    const SAMPLE_SKILL_MD: &str = "---\n\
         name: git-discipline\n\
         description: 提交前先看 diff,身份旗标逐项检查\n\
         version: 1.0.0\n\
         ---\n\
         # 纪律正文\n\
         先 git status 再 git diff。\n";

    /// schema 向后兼容:旧 JSON 无 skills 字段 → 加载成功且为 None(零破坏)
    #[test]
    fn test_skills_absent_is_none_backward_compat() {
        let dir = make_tmp_dir();
        write_json(dir.path(), "old_agent", &minimal_def_json());
        let def = AgentDefinition::load_from_dir(dir.path(), "old_agent").expect("load");
        assert!(def.skills.is_none());
    }

    /// skills 声明加载正例:字段透传 + 相对路径按 definition 目录解析为绝对
    #[test]
    fn test_skills_loading_resolves_relative_paths() {
        let dir = make_tmp_dir();
        write_skill_md(
            dir.path(),
            "skills/git-discipline/SKILL.md",
            SAMPLE_SKILL_MD,
        );
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(
            r#", "skills": [
                { "name": "git-discipline", "path": "skills/git-discipline/SKILL.md" }
            ]}"#,
        );
        write_json(dir.path(), "skill_agent", &json);
        let def = AgentDefinition::load_from_dir(dir.path(), "skill_agent").expect("load");
        let skills = def.skills.expect("skills present");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "git-discipline");
        assert!(
            skills[0].path.is_absolute(),
            "relative path must be resolved"
        );
        assert!(skills[0].path.ends_with("SKILL.md"));
    }

    /// validate 门卫:name 为空拒绝;重名拒绝
    #[test]
    fn test_skills_validate_rejects_empty_and_duplicate_names() {
        let dir = make_tmp_dir();
        let skill_path = write_skill_md(dir.path(), "a/SKILL.md", SAMPLE_SKILL_MD);
        let empty_name = AgentDefinition {
            agent_type: "x".into(),
            version: "1".into(),
            description: "d".into(),
            system_prompt: "s".into(),
            model: "m".into(),
            temperature: 0.5,
            max_steps: 1,
            step_timeout_secs: 1,
            tools: vec![],
            memory: MemoryConfig::default(),
            output_format: None,
            context_window_tokens: None,
            acceptance_command: None,
            max_parallel_tools: 1,
            capability_boundary: None,
            approval_mode: None,
            assembly: None,
            skills: Some(vec![SkillEntry {
                name: "  ".into(),
                path: skill_path.clone(),
            }]),
            identity_segment: None,
            north_star: None,
            handoff: None,
        };
        assert!(empty_name
            .validate()
            .unwrap_err()
            .to_string()
            .contains("name"));

        let dup = AgentDefinition {
            skills: Some(vec![
                SkillEntry {
                    name: "a".into(),
                    path: skill_path.clone(),
                },
                SkillEntry {
                    name: "a".into(),
                    path: skill_path,
                },
            ]),
            ..empty_name
        };
        assert!(dup
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate skill name"));
    }

    /// load_from_dir fail-fast:路径不存在 → 加载即拒
    #[test]
    fn test_skills_missing_file_rejected_at_load() {
        let dir = make_tmp_dir();
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(
            r#", "skills": [
                { "name": "gone", "path": "skills/missing/SKILL.md" }
            ]}"#,
        );
        write_json(dir.path(), "skill_agent", &json);
        let err = AgentDefinition::load_from_dir(dir.path(), "skill_agent").unwrap_err();
        assert!(err.to_string().contains("skills invalid"), "got: {}", err);
    }

    /// frontmatter 不可解析(无围栏) → 加载即拒
    #[test]
    fn test_skills_unparseable_frontmatter_rejected_at_load() {
        let dir = make_tmp_dir();
        write_skill_md(dir.path(), "skills/bad/SKILL.md", "# 只有正文,没有围栏\n");
        let mut json = minimal_def_json();
        json.pop();
        json.push_str(
            r#", "skills": [
                { "name": "bad", "path": "skills/bad/SKILL.md" }
            ]}"#,
        );
        write_json(dir.path(), "skill_agent", &json);
        let err = AgentDefinition::load_from_dir(dir.path(), "skill_agent").unwrap_err();
        assert!(err.to_string().contains("frontmatter"), "got: {}", err);
    }

    /// frontmatter 解析正例:键值提取 + 引号剥离 + 围栏外正文不混入
    #[test]
    fn test_parse_skill_frontmatter_extracts_flat_keys() {
        let fm = parse_skill_frontmatter(SAMPLE_SKILL_MD).expect("parse");
        assert_eq!(fm.name.as_deref(), Some("git-discipline"));
        assert_eq!(
            fm.description.as_deref(),
            Some("提交前先看 diff,身份旗标逐项检查")
        );
    }

    /// frontmatter 解析反例:无围栏/围栏未闭合 → Err
    #[test]
    fn test_parse_skill_frontmatter_rejects_missing_or_unclosed_fence() {
        assert!(parse_skill_frontmatter("no fence here").is_err());
        assert!(parse_skill_frontmatter("---\nname: x\n").is_err());
        assert!(parse_skill_frontmatter("").is_err());
    }

    /// resolve 正例:description 提取;缺失 → Ok(空串) 且不 panic(warn 口径)
    #[test]
    fn test_resolve_skill_manifest_entries_description_semantics() {
        let dir = make_tmp_dir();
        let with_desc = write_skill_md(dir.path(), "a/SKILL.md", SAMPLE_SKILL_MD);
        let no_desc = write_skill_md(dir.path(), "b/SKILL.md", "---\nname: b\n---\nbody only\n");
        let entries = resolve_skill_manifest_entries(&[
            SkillEntry {
                name: "a".into(),
                path: with_desc,
            },
            SkillEntry {
                name: "b".into(),
                path: no_desc,
            },
        ])
        .expect("resolve");
        assert_eq!(entries[0].description, "提交前先看 diff,身份旗标逐项检查");
        assert_eq!(
            entries[1].description, "",
            "missing description = empty string"
        );
    }

    /// resolve 反例:路径是目录 → Err
    #[test]
    fn test_resolve_rejects_directory_path() {
        let dir = make_tmp_dir();
        let sub = dir.path().join("skills-dir");
        std::fs::create_dir_all(&sub).unwrap();
        let err = resolve_skill_manifest_entries(&[SkillEntry {
            name: "d".into(),
            path: sub,
        }])
        .unwrap_err();
        assert!(err.contains("not a regular file"), "got: {}", err);
    }
}
