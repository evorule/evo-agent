// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 工具面统一架构（一表）：ToolManifest 单一真相源
//!
//! 设计档：knowledge/agent-tools/evo-agent-工具面统一架构设计-20261005.md §3.1。
//!
//! 每个工具一条 manifest 记录，是工具全部**治理元数据**的唯一权威源：
//! 来源、能力域、裁决分级、审批策略、agentTools 开关绑定、沙箱域、超时档。
//!
//! **三条硬规则**（设计档 §3.1，对齐生态约束 H fail-fast）：
//! 1. 无 manifest 拒绝注册：[`crate::io_handlers::tool_handler::ToolHandler::register`]
//!    接收 `(manifest, handler)`；按名查不到静态条目且非动态源（服务代理/MCP）
//!    的注册在启动期 panic（fail-fast）。
//! 2. 静态层锁定：内置 36 + 规则 59 + delegate 1 + memory 6 的 manifest 为
//!    静态表（[`static_manifests`]，合计 102）；测试断言数量、名称无重叠、spec 可解析、
//!    派生视图与旧表快照一致——「union 42 vs 41」类注释漂移机制上不可能再现。
//! 3. 动态层注册期补全：服务代理与 MCP 在注册期必须产出 spec
//!    （[`SpecSource::Inline`]）；丢字段即注册失败。
//!
//! **spec 不复制**：LLM 契约文本（description/参数）仍由既有 spec 源函数产出
//! （`default_tool_specs` / `rule_tool_specs` / `memory_tool_specs` /
//! `delegate_tool_spec`），manifest 经 [`SpecSource`] 分派引用——单一真相源
//! （治理元数据在表、契约文本单源），静态层测试锁两侧名称集合相等。
//!
//! 分级口径（与现状行为等价，快照测试锁定）：
//! - [`AdjudicationClass::Sentineled`]：P2 事前意图裁决 + 规则正本 enforce 已在场
//!   （file_create/file_move/file_delete/file_write 的 out_of_sandbox enforce）；
//! - [`AdjudicationClass::Sensitive`]：P2 事前意图裁决在通道（意图必报，暂无
//!   enforce——git 写族 2 + 规则治理写族 19 + delegate 1 + session_spawn 1 +
//!   memory_tombstone 1）；
//! - [`AdjudicationClass::Standard`]：不走 P2 裁决（机制层治理或纯落链）。
//!   `is_governance_adjudication_tool` 改由 manifest 派生后，命中集合恰等于
//!   原 GOVERNANCE_ADJUDICATION_TOOLS 24 表；delegate 统一装配批兑现模块
//!   注释承诺：delegate 升 Sensitive 并接 P2 通道（设计档 §3.2 终态），
//!   派生集合演进为 25 条（重录快照须 diff 审；数量锁 67 不变——级别
//!   变化不增减条目数）。D1 兑现批（2026-10-07）：file_write 升 Sentineled
//!   派生集合演进为 26 条（快照重录 diff 审；数量锁 74 不变）。
//!   file_write 的意图信号与 M5-c R1 通道（pending_target_scope，机制层
//!   由实现持有）并存互不干扰。自主交接 PR-H3 批（2026-10-07）：session_spawn
//!   落地 Sensitive（双闸叠加），派生集合演进为 27 条（数量锁 76→77）。
//!
//! **D6 终态（2026-10-06，B1 收官批）——P2 派生集合语义扩展**：从纯静态表
//! 扩展为「静态表 ∪ 动态注册条目（class≠Standard）」，判定 =
//! [`is_p2_adjudicated_runtime`]——静态命中分级以静态表为准（防降级：冒名
//! 注册不改变治理分级），静态未命中按 runtime manifest（MCP/ServiceProxy
//! 默认 Sensitive 意图必报，人工分级降档走 settings 键
//! `agentTools.mcpAdjudication` / `agentTools.serviceProxyAdjudication`，
//! D7-A 方案一来源级）。`GOVERNANCE_SNAPSHOT` 31 条快照锁静态面不变；
//! 本扩展零静态表变更（数量锁现 74）。

use serde::{Deserialize, Serialize};

use crate::builtin_tools::ParameterSpec;

// =============================================================================
// 枚举（manifest 治理元数据的值域）
// =============================================================================

/// 工具来源（装配与治理语义按来源分派）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    /// 内置工具（宿主实现，default_safe_toolkit 注册）
    Builtin,
    /// 规则工具·透传（rule_tools adapter 表驱动，49 个）
    RuleTransparent,
    /// 规则工具·本地逻辑（rule_tools local_handlers，7 个）
    RuleLocal,
    /// 自省记忆工具（memory_tool，6 个）
    Memory,
    /// delegate 子代理派发
    Delegate,
    /// 技能面（read_skill，按 agent definition skills 声明注册）
    Skill,
    /// 服务代理（evorule-server 插件服务，发现期动态注册）
    ServiceProxy,
    /// MCP 远端工具（握手后 tools/list 动态注册）
    Mcp,
}

/// 能力域（聚焦语言：agent 定义 def.tools 是基础聚焦，能力域是聚焦的词汇表）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapDomain {
    /// 文件系统读
    FsRead,
    /// 文件系统写
    FsWrite,
    /// git 读（status/log/diff 等）
    GitRead,
    /// git 写（stage/commit）
    GitWrite,
    /// 进程执行（shell/命令）
    Process,
    /// 网络访问（http_get 等）
    Network,
    /// 治理写（规则/发布/生产面）
    Governance,
    /// 记忆系统读写
    Memory,
    /// 子代理委派
    Delegate,
    /// 技能加载
    Skill,
    /// 人在环交互（ask_user 澄清卡——会话面问答，无文件/网络触点）
    Interaction,
}

/// 裁决分级（P2 事前意图裁决的通道分级，见模块文档）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdjudicationClass {
    /// 哨兵：P2 裁决 + 规则正本 enforce 已在场
    Sentineled,
    /// 敏感：P2 裁决在通道，意图必报（每次调用必产出裁决提案并留痕）。
    /// 「必报」≠「必拦」——是否真正拦截取决于规则正本是否配置了匹配该
    /// 工具的 enforce 条目，以规则资产现状为准，勿将本分级当作拦截保证。
    Sensitive,
    /// 常规：免 P2 裁决（机制层治理或纯落链）
    Standard,
}

/// 审批策略（PR-2 管道阶段⑤分级审批的输入；现状如实盘点）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// 永远拒绝（逃逸出口/不可逆破坏类）
    AlwaysDeny,
    /// 缺省拒绝（candidate 审批模式：首次调用返回 needs_approval 提案）
    ManualDefault,
    /// 策略端自动（active 白名单即策略；默认宽+事后账全）
    AutoPolicy,
    /// 转人工（人工面专属）
    HumanOnly,
}

/// 沙箱域（PR-6 执行契约与 D3 容器策略的输入）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxScope {
    /// 宿主机制沙箱（fs_safety/net_guard 机制层围栏）
    HostSandboxed,
    /// 容器可执行（D3 策略选项，现状无消费）
    ContainerEligible,
    /// 无沙箱（仅人工面）
    NoSandbox,
}

/// 超时档（执行器生命周期契约：执行超时按档位分派）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutClass {
    /// 30s（与工具内部硬超时一致，如 grep_files）
    Fast,
    /// 60s（现状全局默认）
    Default,
    /// 600s（容器/构建类，容器域策略采纳后启用）
    Long,
}

impl TimeoutClass {
    /// 超时档 → 执行超时值（全仓唯一映射点，执行路径按此分派）
    pub fn duration(self) -> std::time::Duration {
        match self {
            TimeoutClass::Fast => std::time::Duration::from_secs(30),
            TimeoutClass::Default => std::time::Duration::from_secs(60),
            TimeoutClass::Long => std::time::Duration::from_secs(600),
        }
    }
}

/// agentTools.* 开关绑定（从 serve_tools TOOL_SWITCH_KEYS 迁移，快照测试锁等价）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchBinding {
    /// 设置键（agentTools.*，scope=application）
    pub key: String,
    /// 键缺失/值非法时的回落默认（开=在 agent 工具面）
    pub default_on: bool,
}

// =============================================================================
// spec 分派（契约文本单源，manifest 引用不复制）
// =============================================================================

/// spec 契约的来源分派
///
/// 静态源 = 既有 spec 生成函数按名查找（文本单源零复制）；
/// 动态源（服务代理/MCP）= 注册期产出的内联契约（硬规则 3）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpecSource {
    /// 内置 spec（builtin_tools::default_tool_specs 按名查找）
    Builtin,
    /// 规则工具 spec（rule_tools::rule_tool_specs 按名查找）
    Rule,
    /// 自省记忆 spec（memory_tool_specs 按名查找）
    Memory,
    /// delegate spec（delegate_tool_spec，单例）
    Delegate,
    /// 动态内联契约（服务代理/MCP 注册期产出）
    Inline {
        /// 工具描述（动态源注册期必填，空 = 注册失败）
        description: String,
        /// 参数 schema（OpenAI object 形态）
        parameters: serde_json::Value,
    },
}

impl SpecSource {
    /// 解析出 LLM 契约文本（description + OpenAI parameters object 形态）
    ///
    /// 静态源查不到（表漂移）返回 None——由静态层测试锁兜住，运行期不该发生。
    pub fn resolve(&self, name: &str) -> Option<(String, serde_json::Value)> {
        match self {
            SpecSource::Builtin => crate::builtin_tools::default_tool_specs()
                .into_iter()
                .find(|s| s.name == name)
                .map(|s| (s.description, params_to_openai(&s.parameters))),
            SpecSource::Rule => crate::rule_tools::rule_tool_specs()
                .into_iter()
                .find(|s| s.name == name)
                .map(|s| (s.description, params_to_openai(&s.parameters))),
            SpecSource::Memory => crate::agent::memory_tool::memory_tool_specs()
                .into_iter()
                .find(|s| s.name == name)
                .map(|s| (s.description, params_to_openai(&s.parameters))),
            SpecSource::Delegate => {
                let s = crate::builtin_tools::delegate_tool::delegate_tool_spec();
                Some((s.description, params_to_openai(&s.parameters)))
            }
            SpecSource::Inline {
                description,
                parameters,
            } => Some((description.clone(), parameters.clone())),
        }
    }
}

/// ToolSpec 参数列表 → OpenAI function parameters object（与
/// runner::openai_function_schemas_for 同一形状，测试锁两侧一致）
fn params_to_openai(parameters: &[ParameterSpec]) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for p in parameters {
        properties.insert(
            p.name.clone(),
            serde_json::json!({ "type": p.r#type, "description": p.description }),
        );
        if p.required {
            required.push(p.name.clone());
        }
    }
    let mut object = serde_json::json!({ "type": "object", "properties": properties });
    if !required.is_empty() {
        object["required"] = serde_json::json!(required);
    }
    object
}

// =============================================================================
// ToolManifest
// =============================================================================

/// 工具治理元数据单一真相源（一表；spec 文本经 [`SpecSource`] 分派单源）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolManifest {
    /// 全局唯一工具名（LLM 面稳定标识）
    pub name: String,
    /// 来源
    pub source: ToolSource,
    /// spec 契约来源（硬规则 3：动态源必须 Inline 且字段齐全）
    pub spec: SpecSource,
    /// 能力域（主域；聚焦过滤与开关键派生的词汇表）
    pub capability_domains: Vec<CapDomain>,
    /// 裁决分级（P2 通道判定 = class != Standard）
    pub adjudication_class: AdjudicationClass,
    /// 审批策略（PR-2 管道阶段⑤消费；现状如实盘点）
    pub approval_policy: ApprovalPolicy,
    /// agentTools.* 开关绑定（无绑定 = 不受开关过滤，恒可用）
    pub default_switch: Option<SwitchBinding>,
    /// 沙箱域
    pub sandbox_scope: SandboxScope,
    /// 超时档（执行路径按此分派，见 [`TimeoutClass::duration`]）
    pub timeout_class: TimeoutClass,
}

impl ToolManifest {
    /// 解析 LLM 契约（description + OpenAI parameters object）
    pub fn resolve_spec(&self) -> Option<(String, serde_json::Value)> {
        self.spec.resolve(&self.name)
    }

    /// P2 事前意图裁决通道判定（`GOVERNANCE_ADJUDICATION_TOOLS` 的 manifest 派生）
    pub fn is_p2_adjudicated(&self) -> bool {
        self.adjudication_class != AdjudicationClass::Standard
    }
}

// =============================================================================
// 静态表（硬规则 2：内置 36 + 规则 59 + delegate 1 + memory 6 = 102）
// =============================================================================

/// agentTools.fileCreate（默认开）
const SWITCH_FILE_CREATE: &str = "agentTools.fileCreate";
/// agentTools.fileMove（默认关）
const SWITCH_FILE_MOVE: &str = "agentTools.fileMove";
/// agentTools.fileDelete（默认关）
const SWITCH_FILE_DELETE: &str = "agentTools.fileDelete";
/// agentTools.grep（默认开）
const SWITCH_GREP: &str = "agentTools.grep";
/// agentTools.gitRead（默认开）
const SWITCH_GIT_READ: &str = "agentTools.gitRead";
/// agentTools.gitWrite（默认关）
const SWITCH_GIT_WRITE: &str = "agentTools.gitWrite";
/// agentTools.governanceWrite（默认开；单键管规则治理写族全量）
///
/// 公开常量：serve settings schema 登记与 manifest 开关绑定共用此单一来源。
pub const AGENT_TOOLS_GOVERNANCE_WRITE: &str = "agentTools.governanceWrite";
const SWITCH_GOVERNANCE_WRITE: &str = AGENT_TOOLS_GOVERNANCE_WRITE;
/// agentTools.mcp（默认开；单键管 MCP 动态工具面全量）
///
/// 公开常量：serve settings schema 登记与 manifest 开关绑定共用此单一来源。
/// D6 裁定（2026-10-06 项目方批方案甲）：动态源绑定开关，关闭 = 该来源
/// 动态工具连 LLM 契约一起下线（与治理写开关同语义）。
pub const AGENT_TOOLS_MCP: &str = "agentTools.mcp";
/// agentTools.serviceProxy（默认开；单键管服务代理动态工具面全量）
///
/// 公开常量：serve settings schema 登记与 manifest 开关绑定共用此单一来源。
/// D6 裁定同上。
pub const AGENT_TOOLS_SERVICE_PROXY: &str = "agentTools.serviceProxy";
/// agentTools.mcpAdjudication（默认 sensitive；D7-A 方案一：MCP 动态工具
/// 来源级裁决分级——standard = 人工降档免意图裁决）
///
/// 公开常量：serve settings schema 登记与装配覆写共用此单一来源。
pub const AGENT_TOOLS_MCP_ADJUDICATION: &str = "agentTools.mcpAdjudication";
/// agentTools.serviceProxyAdjudication（默认 sensitive；D7-A 同上，作用域
/// = 服务代理来源）
pub const AGENT_TOOLS_SERVICE_PROXY_ADJUDICATION: &str = "agentTools.serviceProxyAdjudication";
/// agentTools.handover（默认开；单键管自主交接工具族全量——交接协议为整体）
///
/// 公开常量：serve settings schema 登记与 manifest 开关绑定共用此单一来源。
/// 交接设计 §四：三工具 manifest 各自绑定同键（对齐 governanceWrite 管族
/// =列表枚举先例），同开同关；application 作用域（工作区层不可覆盖）。
pub const AGENT_TOOLS_HANDOVER: &str = "agentTools.handover";
const SWITCH_HANDOVER: &str = AGENT_TOOLS_HANDOVER;

/// agentTools.memory_recall（默认开；记忆主动消费只读工具）
///
/// 公开常量：serve settings schema 登记与 manifest 开关绑定共用此单一来源。
pub const AGENT_TOOLS_MEMORY_RECALL: &str = "agentTools.memory_recall";
const SWITCH_MEMORY_RECALL: &str = AGENT_TOOLS_MEMORY_RECALL;

/// agentTools.audit_query（默认开；审计查询只读工具）
///
/// 公开常量：serve settings schema 登记与 manifest 开关绑定共用此单一来源。
pub const AGENT_TOOLS_AUDIT_QUERY: &str = "agentTools.audit_query";
const SWITCH_AUDIT_QUERY: &str = AGENT_TOOLS_AUDIT_QUERY;

/// agentTools.memory_tombstone（默认关；记忆墓碑写入工具——沉淀污染环
/// 存量出口，显式启用后仍走 Sensitive 意图裁决 + candidate 审批）
///
/// 公开常量：serve settings schema 登记与 manifest 开关绑定共用此单一来源。
pub const AGENT_TOOLS_MEMORY_TOMBSTONE: &str = "agentTools.memory_tombstone";
const SWITCH_MEMORY_TOMBSTONE: &str = AGENT_TOOLS_MEMORY_TOMBSTONE;

/// agentTools.editFile（默认开；写面精准编辑工具——file_write 同族治理）
///
/// 公开常量：serve settings schema 登记与 manifest 开关绑定共用此单一来源。
pub const AGENT_TOOLS_EDIT_FILE: &str = "agentTools.editFile";
const SWITCH_EDIT_FILE: &str = AGENT_TOOLS_EDIT_FILE;

/// agentTools.workspaceUndo（默认开；写面撤销——弹栈回退安全网，写面撤销批）
///
/// 公开常量：serve settings schema 登记与 manifest 开关绑定共用此单一来源。
pub const AGENT_TOOLS_WORKSPACE_UNDO: &str = "agentTools.workspaceUndo";
const SWITCH_WORKSPACE_UNDO: &str = AGENT_TOOLS_WORKSPACE_UNDO;

/// agentTools.webFetch（默认开；网页抓取取读——http_get 防线同源复用，批K 段2）
///
/// 公开常量：serve settings schema 登记与 manifest 开关绑定共用此单一来源。
pub const AGENT_TOOLS_WEB_FETCH: &str = "agentTools.webFetch";
const SWITCH_WEB_FETCH: &str = AGENT_TOOLS_WEB_FETCH;

/// agentTools.webSearch（默认开；互联网搜索——ai-plugin 中继，Brave key
/// 归边 ai-plugin 侧，批K 段2）
///
/// 公开常量：serve settings schema 登记与 manifest 开关绑定共用此单一来源。
pub const AGENT_TOOLS_WEB_SEARCH: &str = "agentTools.webSearch";
const SWITCH_WEB_SEARCH: &str = AGENT_TOOLS_WEB_SEARCH;

/// agentTools.codeSearch（默认开；工作区符号图谱检索——B14 结构面 agent
/// 工具化,纯读零模型依赖,批K 段3）
///
/// 公开常量：serve settings schema 登记与 manifest 开关绑定共用此单一来源。
pub const AGENT_TOOLS_CODE_SEARCH: &str = "agentTools.codeSearch";
const SWITCH_CODE_SEARCH: &str = AGENT_TOOLS_CODE_SEARCH;

/// agentTools.todo（默认开；todo 任务清单族单键双绑——工具契约面过滤 +
/// runner 装配注入面同键判定，批K 段4）
///
/// 公开常量：serve settings schema 登记与 manifest 开关绑定共用此单一来源。
pub const AGENT_TOOLS_TODO: &str = "agentTools.todo";
const SWITCH_TODO: &str = AGENT_TOOLS_TODO;

/// agentTools.httpRequest（默认关；HTTP 写动词 POST/PUT/PATCH/DELETE——
/// 双闸 Sensitive+ManualDefault，Active host 亦审批，批K 段5）
///
/// 公开常量：serve settings schema 登记与 manifest 开关绑定共用此单一来源。
pub const AGENT_TOOLS_HTTP_REQUEST: &str = "agentTools.httpRequest";
const SWITCH_HTTP_REQUEST: &str = AGENT_TOOLS_HTTP_REQUEST;

/// agentTools.askUser（默认开；人在环澄清卡——agent 向用户发结构化问题
/// 阻塞等回答，超时 fail-visible，批K 段7）
///
/// 公开常量：serve settings schema 登记与 manifest 开关绑定共用此单一来源。
pub const AGENT_TOOLS_ASK_USER: &str = "agentTools.askUser";
const SWITCH_ASK_USER: &str = AGENT_TOOLS_ASK_USER;

/// 开关绑定便捷构造（静态键 → String 归一）
fn sw(key: &'static str, default_on: bool) -> Option<SwitchBinding> {
    Some(SwitchBinding {
        key: key.to_string(),
        default_on,
    })
}

/// 构造静态条目的便捷底版（name/spec/分级/开关/域逐项覆写）
fn base(name: &str, source: ToolSource, spec: SpecSource, domains: Vec<CapDomain>) -> ToolManifest {
    ToolManifest {
        name: name.to_string(),
        source,
        spec,
        capability_domains: domains,
        adjudication_class: AdjudicationClass::Standard,
        approval_policy: ApprovalPolicy::AutoPolicy,
        default_switch: None,
        sandbox_scope: SandboxScope::HostSandboxed,
        timeout_class: TimeoutClass::Default,
    }
}

/// 内置工具 manifest（37 条，与 default_tool_specs 名称集合相等——测试锁）
fn builtin_manifests() -> Vec<ToolManifest> {
    use CapDomain::*;
    let b = |name: &str, domains: Vec<CapDomain>| -> ToolManifest {
        base(name, ToolSource::Builtin, SpecSource::Builtin, domains)
    };
    vec![
        b("file_read", vec![FsRead]),
        // B1 多模态上行触发源:读面图片装载(沙箱/口径同 file_read,FsRead 域)
        b("image_load", vec![FsRead]),
        b("file_list", vec![FsRead]),
        b("file_write", vec![FsWrite]),
        // 批K 段1:写面第五件——精准编辑(old/new 串替换+唯一性闸+保真细节),
        // 治理同 file_write(Sentineled+AutoPolicy,见下方分级段)
        b("edit_file", vec![FsWrite]),
        // 写面撤销批:弹栈回退安全网(Standard+AutoPolicy 免裁决——不引入新
        // 内容只回退;FsWrite 域,恢复路径复验写面白名单;红线名单含本工具)
        b("workspace_undo", vec![FsWrite]),
        b("file_create", vec![FsWrite]),
        b("file_move", vec![FsWrite]),
        b("file_delete", vec![FsWrite]),
        b("search_files", vec![FsRead]),
        b("grep_files", vec![FsRead]),
        // 批K 段3:符号图谱检索(B14 结构面 agent 工具化)——读面 FsRead 域,
        // 占位/serve 期重绑双态(见 mod.rs 注册注释);Standard+AutoPolicy
        // 免裁决(纯读),不进 GOVERNANCE_SNAPSHOT
        b("code_search", vec![FsRead]),
        b("shell_exec", vec![Process]),
        b("http_get", vec![Network]),
        // 批K 段2:网络面第二件——网页抓取取读(复用 http_get host 分类/
        // SSRF/重定向/负面域全部防线;candidate host 同 http_get 审批语义)
        b("web_fetch", vec![Network]),
        // 批K 段2:网络面第三件——互联网搜索(ai-plugin 中继,Brave key 归边
        // ai-plugin 侧;查询词出站面由 exfil_scan 词形打标管辖)
        b("web_search", vec![Network]),
        // 批K 段5:网络面第四件——HTTP 写动词(POST/PUT/PATCH/DELETE)。
        // 防线组合继承 http_get(SSRF/三层 host/net_guard/超时/帽);redirect
        // Policy::none 禁跟随(唯一分叉)。治理全批最严:双闸 Sensitive+
        // ManualDefault(见下方分级段),默认关(写动词不进缺省工具面)
        b("http_request", vec![Network]),
        // python_exec（收口修复：执行体已注册但静态表缺条目，register_static
        // fail-fast 传染 toolkit 构造型测试）。任意代码执行（无命令白名单，
        // 危险等级高于 shell_exec）——Process 域 + candidate 全审批（见分级
        // 段），spec 源与审批钩子同步补齐。
        b("python_exec", vec![Process]),
        b("git_status", vec![GitRead]),
        b("git_diff", vec![GitRead]),
        b("git_log", vec![GitRead]),
        b("git_stage", vec![GitWrite]),
        b("git_commit", vec![GitWrite]),
        // 查账工具族（工具面统一架构 PR-11a）：会话账面只读查询——journal
        // 事件流/工具轨迹/文件现势+staleness/两段文本比对。全部 Standard
        // （免裁决但落账：查账自身入账）+ AutoPolicy（查账是减错动作本身，
        // 不加审批摩擦，设计档 §11.1）；无写面，不绑开关（与 file_read 同列）。
        b("query_journal", vec![Governance]),
        b("query_trace", vec![Governance]),
        b("read_back", vec![FsRead, Governance]),
        b("diff_runs", vec![Governance]),
        // read_skill 按 agent definition skills 声明注册（非 default_safe_toolkit
        // 静态注册），spec 常驻 default_tool_specs；来源标 Skill。
        base(
            "read_skill",
            ToolSource::Skill,
            SpecSource::Builtin,
            vec![Skill],
        ),
        // 自主交接双工具（自主交接设计 PR-H2）：会话间状态的结构化读写。
        // 占位执行体启动期注册（default_safe_toolkit），runner 会话期重绑
        // （wire_accounting 同构）；Standard 免裁决（自用 Draft 性质，对齐
        // note_write）+ AutoPolicy；开关=agentTools.handover 单键同绑（见
        // default_switch 段）。spec 常驻 default_tool_specs（Builtin 源）。
        b("handover_write", vec![Memory]),
        b("handover_read", vec![Memory]),
        // 记忆主动消费：只读检索工具（stable/events 两
        // 分区账本读取）。占位执行体启动期注册（default_safe_toolkit），runner
        // 会话期重绑（handover 同款）；Standard 免裁决 + AutoPolicy（只读面）。
        // spec 常驻 default_tool_specs（Builtin 源）。
        b("memory_recall", vec![Memory]),
        // 审计查询：三 scope 只读入口（gov/session/
        // journal）。注册/重绑/审批面同 memory_recall（只读 Standard+AutoPolicy）。
        b("audit_query", vec![Memory]),
        // 记忆墓碑（沉淀污染环存量出口）：null 墓碑写入（path+reason，
        // 存在性核对在执行体内）。注册/重绑同 memory_recall；Sensitive +
        // ManualDefault（双闸同 session_spawn——写面治理意图必报 + 人工
        // 默认否决点），默认关（agentTools.memory_tombstone）。
        b("memory_tombstone", vec![Memory]),
        // 自主开会话（自主交接设计 PR-H3）：子会话派生（fork，因果链权威在
        // server 侧 parent 链）+ 首轮 goal 自动驱动。占位执行体启动期注册
        // （default_safe_toolkit），runner 会话期接线重绑（handover 双工具
        // 同款）。能力域=Delegate（语义最近：创建执行上下文）。
        b("session_spawn", vec![CapDomain::Delegate]),
        // todo 任务清单族（批K 段4）：会话内结构化任务清单全量重写读写。
        // 占位执行体启动期注册（default_safe_toolkit），runner 会话期重绑
        // （handover 同款，state 在手）；Standard 免裁决 + AutoPolicy（进程
        // 内内存态读写，无沙箱/网络/子进程面）。能力域=Memory（handover
        // 先例：会话自管理状态面）。spec 常驻 default_tool_specs（Builtin 源）。
        b("todo_write", vec![Memory]),
        b("todo_read", vec![Memory]),
        // ask_user 人在环澄清卡（批K 段7）：agent 向用户发结构化问题阻塞
        // 等回答（两阶段协议：首调出 pending 形态 → runner 推帧等回答 →
        // 带 answer 重调）。能力域=Interaction（新增：人在环交互，无文件/
        // 网络/子进程触点）；Standard 免裁决 + AutoPolicy（交互零风险），
        // 不进 EXFIL_SCAN_TOOLS（question 发给会话数据有权方，非网络外发）。
        // spec 常驻 default_tool_specs（Builtin 源）。
        b("ask_user", vec![Interaction]),
    ]
    .into_iter()
    .map(|mut m| {
        // 分级/审批/开关/超时的现状如实盘点（设计档 §3.1 派生关系）
        match m.name.as_str() {
            // 哨兵：规则正本 00_constraint_tool_intent_adjudication 已有
            // out_of_sandbox enforce（核查报告活体实测 BLOCKED）；
            // 同时是 candidate 审批模式（handler 内嵌 needs_approval 提案机制）
            "file_create" | "file_move" | "file_delete" => {
                m.adjudication_class = AdjudicationClass::Sentineled;
                m.approval_policy = ApprovalPolicy::ManualDefault;
            }
            // file_write / edit_file：D1 兑现——规则正本第 4 条 enforce
            // （out_of_sandbox）在场，升 Sentineled；审批面不动（写面高频
            // 工具维持既有审批策略，由 handler 内联沙箱检查 + 规则层
            // enforce 双层把守）。edit_file 同族：写面精准编辑（批K 段1），
            // 意图信号经 resolve_tool_intent 采集（path 字段快筛同族）
            "file_write" | "edit_file" => {
                m.adjudication_class = AdjudicationClass::Sentineled;
            }
            // git 写族：P2 裁决在通道；D1 建议上 enforce（PR-5 规则正本入库时
            // 追加，升级 Sentineled），manifest 先如实标 Sensitive；
            // 同为 candidate 审批模式
            "git_stage" | "git_commit" => {
                m.adjudication_class = AdjudicationClass::Sensitive;
                m.approval_policy = ApprovalPolicy::ManualDefault;
            }
            // session_spawn（自主交接设计 PR-H3）：双闸叠加——Sensitive（P2
            // 事前意图裁决：自主开会话意图必报）+ ManualDefault（candidate
            // 审批模式首调出 needs_approval 提案）。职责不同非冗余：前者
            // 治理意图留痕，后者给人工面默认否决点（自主交接设计 §四）
            "session_spawn" => {
                m.adjudication_class = AdjudicationClass::Sensitive;
                m.approval_policy = ApprovalPolicy::ManualDefault;
            }
            // memory_tombstone（沉淀污染环存量出口）：双闸同 session_spawn——
            // Sensitive（写面治理意图必报）+ ManualDefault（candidate 审批
            // 模式首调出 needs_approval 提案；默认关，显式启用才可达）
            "memory_tombstone" => {
                m.adjudication_class = AdjudicationClass::Sensitive;
                m.approval_policy = ApprovalPolicy::ManualDefault;
            }
            // http_request（批K 段5，写动词）：双闸同 session_spawn 先例——
            // Sensitive（写动词意图必报，Active host 亦出 proposal——写面
            // 无 AutoPolicy 直跑特权）+ ManualDefault（每次调用人工默认
            // 否决点）。全批最严治理面：红线 + SSRF + exfil 横切 + 双闸
            // 四层叠加（见 http_request.rs 模块文档）
            "http_request" => {
                m.adjudication_class = AdjudicationClass::Sensitive;
                m.approval_policy = ApprovalPolicy::ManualDefault;
            }
            // 命令/网络 candidate 族：分类器判入 candidate 即出 needs_approval
            // 提案（PR-4 起经管道⑤评估臂单源），如实补标 ManualDefault
            // （此前默认 AutoPolicy 属盘点遗漏，行为等价修正）。
            // python_exec：任意代码执行无白名单可用，任何调用都出提案
            // （执行体 evaluate_proposal 全审批，无 active 名单）
            "shell_exec" | "http_get" | "web_fetch" => {
                m.approval_policy = ApprovalPolicy::ManualDefault;
            }
            "python_exec" => {
                m.approval_policy = ApprovalPolicy::ManualDefault;
            }
            // grep_files 内部 30s 硬超时（与全局 60s 双层，如实标注）
            "grep_files" => m.timeout_class = TimeoutClass::Fast,
            _ => {}
        }
        // agentTools.* 开关绑定（原 TOOL_SWITCH_KEYS 内置段，快照测试锁等价）
        m.default_switch = match m.name.as_str() {
            "file_create" => sw(SWITCH_FILE_CREATE, true),
            // 批K 段1:写面精准编辑,默认开(低风险高频,同 fileCreate 先例)
            "edit_file" => sw(SWITCH_EDIT_FILE, true),
            // 写面撤销批:弹栈回退安全网,默认开(不引入新内容只回退;
            // 关=agent 改坏后无自助退出面,回退到用户兜底)
            "workspace_undo" => sw(SWITCH_WORKSPACE_UNDO, true),
            // 批K 段2:网络读取/检索面,默认开
            "web_fetch" => sw(SWITCH_WEB_FETCH, true),
            "web_search" => sw(SWITCH_WEB_SEARCH, true),
            "file_move" => sw(SWITCH_FILE_MOVE, false),
            "file_delete" => sw(SWITCH_FILE_DELETE, false),
            "grep_files" => sw(SWITCH_GREP, true),
            // 批K 段3:符号图谱检索,默认开(纯读面,同 grep 先例)
            "code_search" => sw(SWITCH_CODE_SEARCH, true),
            "git_status" | "git_diff" | "git_log" => sw(SWITCH_GIT_READ, true),
            "git_stage" | "git_commit" => sw(SWITCH_GIT_WRITE, false),
            // 自主交接族：单键同绑同开同关（自主交接设计 §四；PR-H3 起
            // session_spawn 第三行同键接入）
            "handover_write" | "handover_read" | "session_spawn" => sw(SWITCH_HANDOVER, true),
            // 记忆主动消费：只读检索，默认开
            "memory_recall" => sw(SWITCH_MEMORY_RECALL, true),
            // 审计查询：只读入口，默认开
            "audit_query" => sw(SWITCH_AUDIT_QUERY, true),
            // 记忆墓碑：写入面默认关（显式启用才进 LLM 契约）
            "memory_tombstone" => sw(SWITCH_MEMORY_TOMBSTONE, false),
            // todo 任务清单族（批K 段4）：单键双绑（工具契约面过滤 +
            // runner 装配注入面同键判定），默认开（进程内内存态，零外溢面）
            "todo_write" | "todo_read" => sw(SWITCH_TODO, true),
            // 批K 段5:HTTP 写动词,默认关(写动词不进缺省工具面,显式启用
            // 才可达;启用后仍走双闸全流审批)
            "http_request" => sw(SWITCH_HTTP_REQUEST, false),
            // 批K 段7:人在环澄清卡,默认开(交互零风险;关=agent 无人可问,
            // resolve 侧立即返回无人态 fail-visible)
            "ask_user" => sw(SWITCH_ASK_USER, true),
            _ => None,
        };
        m
    })
    .collect()
}

/// 规则治理写族（agentTools.governanceWrite 单键管全量，22 个）
const GOVERNANCE_WRITE_TOOLS: &[&str] = &[
    "rule_create",
    "rule_update",
    "rule_submit",
    "rule_activate",
    "rule_block",
    "rule_archive",
    "rule_fork",
    "rule_reload",
    "rule_promote",
    "verdict_submit",
    "ws_create",
    "sandbox_start",
    "sandbox_close",
    "dataset_create",
    "publish_submit",
    "publish_list",
    "publish_queue_get",
    "publish_review",
    "publish_rollback",
    "bundle_export",
    "bundle_import_dry_run",
    "bundle_import",
];

/// P2 裁决在通道的规则工具（原 GOVERNANCE_ADJUDICATION_TOOLS 规则段 19 个；
/// publish_list/publish_queue_get 有写权开关但不走 P2 裁决——读面语义）
const RULE_P2_ADJUDICATED_TOOLS: &[&str] = &[
    "rule_create",
    "rule_update",
    "rule_submit",
    "rule_activate",
    "rule_block",
    "rule_archive",
    "rule_fork",
    "rule_reload",
    "rule_promote",
    "verdict_submit",
    "ws_create",
    "sandbox_start",
    "sandbox_close",
    "dataset_create",
    "publish_submit",
    "publish_review",
    "publish_rollback",
    "bundle_export",
    "bundle_import_dry_run",
    "bundle_import",
];

/// 规则工具 manifest（59 条 = 透传 49 + 本地逻辑 7 + why/order 3，名称集合与
/// rule_tool_specs() 相等——测试锁）
fn rule_manifests() -> Vec<ToolManifest> {
    let mut manifests: Vec<ToolManifest> = crate::rule_tools::adapter::ALL_TRANSPARENT_BINDINGS
        .iter()
        .map(|binding| {
            let name = binding.name;
            let mut m = base(
                name,
                ToolSource::RuleTransparent,
                SpecSource::Rule,
                vec![CapDomain::Governance],
            );
            if RULE_P2_ADJUDICATED_TOOLS.contains(&name) {
                m.adjudication_class = AdjudicationClass::Sensitive;
            }
            if GOVERNANCE_WRITE_TOOLS.contains(&name) {
                m.default_switch = sw(SWITCH_GOVERNANCE_WRITE, true);
            }
            m
        })
        .collect();
    // 本地逻辑 7（rule_tools::local_handlers::register 的注册名）
    for name in [
        "audit_verify",
        "bundle_export",
        "skill_pack_to_bundle",
        "meta_summary",
        "evolution_signals",
        "rule_promote",
        "verdict_submit",
    ] {
        let mut m = base(
            name,
            ToolSource::RuleLocal,
            SpecSource::Rule,
            vec![CapDomain::Governance],
        );
        if RULE_P2_ADJUDICATED_TOOLS.contains(&name) {
            m.adjudication_class = AdjudicationClass::Sensitive;
        }
        if GOVERNANCE_WRITE_TOOLS.contains(&name) {
            m.default_switch = sw(SWITCH_GOVERNANCE_WRITE, true);
        }
        manifests.push(m);
    }
    // why/order 3（rule_tools::why_tools::register 的注册名，PR-11b 查账
    // 因果查询面）——全部只读，Standard（免裁决但落账）+AutoPolicy（免审）
    // +无开关（不进 GOVERNANCE_SNAPSHOT/SWITCH_SNAPSHOT，快照面零变化）
    for name in ["explain_denial", "causal_order", "lineage_of"] {
        let m = base(
            name,
            ToolSource::RuleLocal,
            SpecSource::Rule,
            vec![CapDomain::Governance],
        );
        manifests.push(m);
    }
    manifests
}

/// delegate manifest（1 条）
///
/// delegate 统一装配批起 adjudication_class=Sensitive（接 P2 通道，设计档
/// §3.2 终态；PR-1 行为等价期的 Standard 暂标已随快照重录一并演进）。
fn delegate_manifest() -> ToolManifest {
    // delegate 统一装配批：级别演进 Standard→Sensitive（模块文档承诺兑现）——
    // delegate 自身调用接 P2 事前意图裁决通道（意图必报）。条目数不变
    // （test_static_manifest_count_locked 锁守），仅级别字段演进；P2 派生
    // 集合 24→25（快照重录）。
    let mut m = base(
        "delegate",
        ToolSource::Delegate,
        SpecSource::Delegate,
        vec![CapDomain::Delegate],
    );
    m.adjudication_class = AdjudicationClass::Sensitive;
    m
}

/// 自省记忆工具 manifest（6 条，与 memory_tool_specs 名称集合相等——测试锁）
fn memory_manifests() -> Vec<ToolManifest> {
    use crate::agent::memory_tool::{
        MEMORY_GET_TOOL, MEMORY_LINK_TOOL, MEMORY_PROPOSE_TOOL, MEMORY_SEARCH_TOOL, NOTE_WRITE_TOOL,
    };
    vec![
        base(
            MEMORY_SEARCH_TOOL,
            ToolSource::Memory,
            SpecSource::Memory,
            vec![CapDomain::Memory],
        ),
        base(
            MEMORY_GET_TOOL,
            ToolSource::Memory,
            SpecSource::Memory,
            vec![CapDomain::Memory],
        ),
        base(
            MEMORY_PROPOSE_TOOL,
            ToolSource::Memory,
            SpecSource::Memory,
            vec![CapDomain::Memory],
        ),
        base(
            NOTE_WRITE_TOOL,
            ToolSource::Memory,
            SpecSource::Memory,
            vec![CapDomain::Memory],
        ),
        base(
            MEMORY_LINK_TOOL,
            ToolSource::Memory,
            SpecSource::Memory,
            vec![CapDomain::Memory],
        ),
        base(
            crate::agent::memory_tool::MEMORY_FORGET_TOOL,
            ToolSource::Memory,
            SpecSource::Memory,
            vec![CapDomain::Memory],
        ),
    ]
}

/// 全部静态 manifest（102 条；测试锁数量/唯一名/双侧集合相等）
///
/// C1 性能收口（PR-3 顺手）：静态表在首次访问后经 OnceLock 缓存——
/// 构造函数为纯函数（无常量/无环境依赖），行为零变化（调用面拿到的
/// 仍是克隆值，外部无法扰动缓存）；消除热路径（逐工具调用查表/聚焦
/// 构面）的每次全量重建。
pub fn static_manifests() -> Vec<ToolManifest> {
    static_manifest_table().values().cloned().collect()
}

/// 按名查静态 manifest（注册点 fail-fast 查询入口）
pub fn lookup_static(name: &str) -> Option<ToolManifest> {
    static_manifest_table().get(name).cloned()
}

/// 静态表 OnceLock 缓存单点（BTreeMap：按名 O(log n) 查找 + 名序稳定）
fn static_manifest_table() -> &'static std::collections::BTreeMap<String, ToolManifest> {
    static TABLE: std::sync::OnceLock<std::collections::BTreeMap<String, ToolManifest>> =
        std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut all = builtin_manifests();
        all.extend(rule_manifests());
        all.push(delegate_manifest());
        all.extend(memory_manifests());
        all.into_iter().map(|m| (m.name.clone(), m)).collect()
    })
}

/// 动态源 manifest 构造（服务代理/MCP 注册期；硬规则 3）
///
/// - 服务代理：description/parameters 来自服务对账清单（注册期已校验非敏感）；
/// - MCP：description/inputSchema 来自远端 tools/list 透传；
/// - `description` 为空视为丢字段（fail-fast，调用方拒注册）。
///
/// **D6 裁定（2026-10-06 项目方批方案甲）**：动态源绑定 `agentTools.mcp` /
/// `agentTools.serviceProxy` 开关（默认开，向后兼容；消费点 =
/// `build_filtered_toolkit_with_switches`，关闭 = 该来源工具面整体下线）。
/// **D6 终态已落地**：动态源默认 `Sensitive`（意图必报）——P2 派生判定 =
/// [`is_p2_adjudicated_runtime`]（静态优先防降级 ∪ 动态按本 manifest 分级）；
/// 人工分级降 Standard 走 settings 键 `agentTools.mcpAdjudication` /
/// `agentTools.serviceProxyAdjudication`（来源级，D7-A 方案一，装配层覆写）。
pub fn dynamic_manifest(
    name: &str,
    source: ToolSource,
    description: String,
    parameters: serde_json::Value,
) -> Result<ToolManifest, String> {
    if description.trim().is_empty() {
        return Err(format!(
            "tool '{name}' ({source:?}) rejected: empty description at registration \
             (manifest hard rule 3: dynamic sources must produce a spec)"
        ));
    }
    if !parameters.is_object() {
        return Err(format!(
            "tool '{name}' ({source:?}) rejected: parameters is not an object at \
             registration (manifest hard rule 3)"
        ));
    }
    let (domains, default_switch, approval_policy) = match source {
        ToolSource::ServiceProxy => (
            vec![CapDomain::Governance],
            sw(AGENT_TOOLS_SERVICE_PROXY, true),
            ApprovalPolicy::AutoPolicy,
        ),
        // G8 接合：MCP 工具缺省 candidate 全审批
        // （ManualDefault——首次调用出 needs_approval 提案过 G8）；白名单直跑
        // 由注册层按 `mcp.servers[].always_allow` 覆写为 AutoPolicy（见
        // tool_adapter::register_client_tools，alwaysAllow=用户显式授权）。
        ToolSource::Mcp => (
            vec![CapDomain::Process],
            sw(AGENT_TOOLS_MCP, true),
            ApprovalPolicy::ManualDefault,
        ),
        _ => return Err(format!("unsupported dynamic tool source: {source:?}")),
    };
    Ok(ToolManifest {
        name: name.to_string(),
        source,
        spec: SpecSource::Inline {
            description,
            parameters,
        },
        capability_domains: domains,
        adjudication_class: AdjudicationClass::Sensitive,
        approval_policy,
        default_switch,
        sandbox_scope: SandboxScope::HostSandboxed,
        timeout_class: TimeoutClass::Default,
    })
}

/// P2 派生判定（运行时形态，D6 终态）：静态优先防降级 ∪ 动态按 runtime manifest
///
/// 判定序（防降级锚，设计档 §12.2）：
/// - `lookup_static` 命中 → 分级以**静态表**为准——冒名注册（静态名下挂
///   动态 manifest）不改变治理分级（runner_tests 冒名测试的判据前提）；
/// - 静态未命中 → 按 runtime manifest（MCP/ServiceProxy 注册期产出的
///   分级，D6 终态默认 Sensitive，人工可降 Standard）。
///
/// 与 [`is_governance_adjudication_tool`]（runner，纯静态查询）的分工：本
/// 函数消费管道阶段①查得的 runtime manifest（`PipelineDeps.manifest_of`
/// 为 handler 优先+静态兜底——直接消费会遮蔽静态表，故本函数显式以
/// `lookup_static` 先行）。`runtime = None` 退化为纯静态语义（与旧查询
/// 等价）。
pub fn is_p2_adjudicated_runtime(name: &str, runtime: Option<&ToolManifest>) -> bool {
    match lookup_static(name) {
        Some(static_m) => static_m.is_p2_adjudicated(),
        None => runtime.map(|m| m.is_p2_adjudicated()).unwrap_or(false),
    }
}

// =============================================================================
// 测试（硬规则 2 静态层锁 + 派生等价快照）
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn all() -> Vec<ToolManifest> {
        static_manifests()
    }

    #[test]
    fn test_static_manifest_count_locked() {
        // 内置 24 + 规则 58 + delegate 1 + memory 6 = 90
        // （A 类断头线 5 工具：透传 44→49，规则 53→58，数量锁 83→88；
        // （facts_version +1：透传 43→44，规则 52→53，数量锁 82→83；
        // （audit_export +1：透传 42→43，规则 51→52，数量锁 81→82；
        // （hit-stats 双工具 +2：透传 40→42，规则 49→51，数量锁 79→81；
        // （PR-11a 查账工具族 +4：16→20；PR-11b why/order 三工具 +3：
        //   规则 46→49；自主交接双工具 +2：内置 20→22，数量锁 74→76；
        //   自主交接 PR-H3 session_spawn +1：内置 22→23，数量锁 76→77；
        //   账本记忆收口（ai/queue14 合并）：memory_link +1、memory_forget +1
        //   → memory 4→6，数量锁 77→79）
        //   （判定事实治理登记批：verdict_submit +1，规则 58→59，数量锁 88→89）
        //   （python_exec 收口 +1：内置 23→24，数量锁 89→90）
        //   （B22 记忆主动消费 memory_recall +1：内置 24→25，数量锁 90→91）
        //   （B22 审计查询 audit_query +1：内置 25→26，数量锁 91→92）
        //   （B1 多模态上行 image_load +1：内置 26→27，数量锁 92→93）
        //   （O-487② 记忆墓碑 memory_tombstone +1：内置 27→28，数量锁 93→94）
        //   （批K 段1 精准编辑 edit_file +1：内置 28→29，数量锁 94→95）
        //   （批K 段2 网络双件 web_fetch/web_search +2：内置 29→31，数量锁 95→97）
        //   （批K 段3 符号图谱检索 code_search +1：内置 31→32，数量锁 97→98）
        //   （批K 段4 todo 双件 todo_write/todo_read +2：内置 32→34，数量锁 98→100）
        //   （批K 段5 HTTP 写动词 http_request +1：内置 34→35，数量锁 100→101）
        //   （批K 段7 人在环 ask_user +1：内置 35→36，数量锁 101→102）
        //   （写面撤销批 workspace_undo +1：内置 36→37，数量锁 102→103）
        assert_eq!(builtin_manifests().len(), 37);
        assert_eq!(rule_manifests().len(), 59);
        assert_eq!(all().len(), 103);
    }

    #[test]
    fn test_static_manifest_names_unique() {
        let manifests = all();
        let names: Vec<&str> = manifests.iter().map(|m| m.name.as_str()).collect();
        let set: HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), set.len(), "manifest names must be unique");
    }

    #[test]
    fn test_names_match_spec_sources_both_ways() {
        // manifest 表 ↔ spec 源函数 双向名称集合相等（消灭 spec/handler 两张皮）
        let manifest_names: HashSet<String> = all().iter().map(|m| m.name.clone()).collect();

        let mut spec_names: HashSet<String> = crate::builtin_tools::default_tool_specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        spec_names.extend(
            crate::rule_tools::rule_tool_specs()
                .into_iter()
                .map(|s| s.name),
        );
        spec_names.extend(
            crate::agent::memory_tool::memory_tool_specs()
                .into_iter()
                .map(|s| s.name),
        );
        spec_names.insert(crate::builtin_tools::delegate_tool::delegate_tool_spec().name);

        let missing_in_manifest: Vec<&String> = spec_names.difference(&manifest_names).collect();
        assert!(
            missing_in_manifest.is_empty(),
            "spec sources not in manifest: {missing_in_manifest:?}"
        );
        let orphan_manifests: Vec<&String> = manifest_names.difference(&spec_names).collect();
        assert!(
            orphan_manifests.is_empty(),
            "manifests without spec source: {orphan_manifests:?}"
        );
    }

    #[test]
    fn test_every_manifest_resolves_serializable_spec() {
        for m in all() {
            let (description, parameters) = m
                .resolve_spec()
                .unwrap_or_else(|| panic!("manifest '{}' spec source must resolve", m.name));
            assert!(
                !description.trim().is_empty(),
                "{}: empty description",
                m.name
            );
            assert!(
                parameters.is_object(),
                "{}: parameters must be an object",
                m.name
            );
            assert!(
                parameters.get("type").and_then(|v| v.as_str()) == Some("object"),
                "{}: parameters.type must be object",
                m.name
            );
        }
    }

    #[test]
    fn test_capability_domains_within_enum() {
        for m in all() {
            assert!(
                !m.capability_domains.is_empty(),
                "{}: empty domains",
                m.name
            );
        }
    }

    #[test]
    fn test_timeout_class_duration_mapping() {
        // 超时档 → 执行超时值映射锁(与枚举变体文档一致)
        assert_eq!(
            TimeoutClass::Fast.duration(),
            std::time::Duration::from_secs(30)
        );
        assert_eq!(
            TimeoutClass::Default.duration(),
            std::time::Duration::from_secs(60)
        );
        assert_eq!(
            TimeoutClass::Long.duration(),
            std::time::Duration::from_secs(600)
        );
    }

    #[test]
    fn test_manifest_serialization_roundtrip() {
        for m in all() {
            let json =
                serde_json::to_string(&m).unwrap_or_else(|e| panic!("{} serialize: {e}", m.name));
            let back: ToolManifest =
                serde_json::from_str(&json).unwrap_or_else(|e| panic!("{}: {e}", m.name));
            assert_eq!(back, m, "{}: roundtrip mismatch", m.name);
        }
    }

    // ---------- 派生等价快照（防迁移丢失） ----------

    /// 原 GOVERNANCE_ADJUDICATION_TOOLS 24 表快照（runner.rs 已改派生，
    /// 快照在此固化防漂移）；delegate 统一装配批 delegate 升 Sensitive 后
    /// 演进为 25 条（delegate 加入治理裁决集——快照重录须 diff 审，数量锁
    /// 不变，级别变化不增减条目数）。D1 兑现批 file_write 升 Sentineled
    /// 后演进为 26 条（同款 diff 审纪律）。自主交接 PR-H3 批 session_spawn
    /// 落地 Sensitive（双闸叠加），演进为 27 条（数量锁 76→77）。
    /// 判定事实治理登记批 verdict_submit +1 → 28 条；O-487② 记忆墓碑
    /// memory_tombstone 落地 Sensitive（双闸同 session_spawn），演进为 29 条
    /// （默认关——开关不在场不进 LLM 契约，裁决集仍含，防绕道）。
    /// 批K 段1 edit_file 落地 Sentineled（file_write 同族写面）→ 30 条
    /// （快照重录 diff 审）。批K 段5 http_request 落地 Sensitive（写动词
    /// 双闸，Active host 亦审批）→ 31 条（快照重录 diff 审）。
    const GOVERNANCE_SNAPSHOT: &[&str] = &[
        "file_create",
        "file_move",
        "file_delete",
        "file_write",
        "edit_file",
        "git_stage",
        "git_commit",
        "rule_create",
        "rule_update",
        "rule_submit",
        "rule_activate",
        "rule_block",
        "rule_archive",
        "rule_fork",
        "rule_reload",
        "rule_promote",
        "verdict_submit",
        "ws_create",
        "sandbox_start",
        "sandbox_close",
        "dataset_create",
        "publish_submit",
        "publish_review",
        "publish_rollback",
        "bundle_export",
        "bundle_import_dry_run",
        "bundle_import",
        "delegate",
        "session_spawn",
        "memory_tombstone",
        // 批K 段5:HTTP 写动词（双闸 Sensitive+ManualDefault——写动词意图必报）
        "http_request",
    ];

    #[test]
    fn test_p2_adjudicated_set_matches_governance_snapshot() {
        let manifests = all();
        let derived: HashSet<&str> = manifests
            .iter()
            .filter(|m| m.is_p2_adjudicated())
            .map(|m| m.name.as_str())
            .collect();
        let snapshot: HashSet<&str> = GOVERNANCE_SNAPSHOT.iter().copied().collect();
        assert_eq!(
            derived, snapshot,
            "P2 派生集合必须与 GOVERNANCE_ADJUDICATION_TOOLS 快照完全一致"
        );
    }

    #[test]
    fn test_delegate_manifest_is_sensitive_p2_adjudicated() {
        // delegate 统一装配批：级别演进 Standard→Sensitive（模块文档承诺
        // 兑现）——delegate 自身调用接 P2 意图必报通道。条目数不变
        // （test_static_manifest_count_locked 锁守），仅级别演进。
        let manifests = all();
        let m = manifests
            .iter()
            .find(|m| m.name == "delegate")
            .expect("delegate manifest must exist in static table");
        assert_eq!(m.adjudication_class, AdjudicationClass::Sensitive);
        assert!(m.is_p2_adjudicated());
    }

    #[test]
    fn test_http_request_manifest_dual_gate() {
        // 批K 段5:HTTP 写动词双闸断言——Sensitive(P2 意图必报,写动词
        // 无直跑特权)+ ManualDefault(每次调用人工默认否决点);Network 域;
        // 默认关(写动词不进缺省工具面)
        let manifests = all();
        let m = manifests
            .iter()
            .find(|m| m.name == "http_request")
            .expect("http_request manifest must exist in static table");
        assert_eq!(m.adjudication_class, AdjudicationClass::Sensitive);
        assert_eq!(m.approval_policy, ApprovalPolicy::ManualDefault);
        assert!(m.is_p2_adjudicated());
        assert!(m
            .capability_domains
            .contains(&crate::agent::tool_manifest::CapDomain::Network));
        let sw = m.default_switch.as_ref().expect("switch binding required");
        assert_eq!(sw.key, "agentTools.httpRequest");
        assert!(!sw.default_on, "write verb must default OFF");
    }

    #[test]
    fn test_sentineled_is_exactly_file_break_family() {
        let manifests = all();
        // 名序化表(C1 OnceLock BTreeMap)遍历序=字典序;断言按集合语义排序比较
        let mut sentineled: Vec<&str> = manifests
            .iter()
            .filter(|m| m.adjudication_class == AdjudicationClass::Sentineled)
            .map(|m| m.name.as_str())
            .collect();
        sentineled.sort_unstable();
        assert_eq!(
            sentineled,
            vec![
                "edit_file",
                "file_create",
                "file_delete",
                "file_move",
                "file_write"
            ]
        );
    }

    /// 原 TOOL_SWITCH_KEYS 快照（tool, key, default_on）；批K 段1 edit_file
    /// 接入 → 32 条（memory_recall/audit_query/memory_tombstone 后续接入，
    /// 历史批注曾滞后于实际行数，20261021 批K 段4 修正口径）；批K 段4
    /// todo_write/todo_read 接入 → 43 条；批K 段5 http_request 接入
    /// （默认关）→ 44 条；批K 段7 ask_user 接入 → 45 条；写面撤销批
    /// workspace_undo 接入（默认开）→ 46 条
    const SWITCH_SNAPSHOT: &[(&str, &str, bool)] = &[
        ("file_create", "agentTools.fileCreate", true),
        ("edit_file", "agentTools.editFile", true),
        ("web_fetch", "agentTools.webFetch", true),
        ("web_search", "agentTools.webSearch", true),
        ("code_search", "agentTools.codeSearch", true),
        ("file_move", "agentTools.fileMove", false),
        ("file_delete", "agentTools.fileDelete", false),
        ("grep_files", "agentTools.grep", true),
        ("git_status", "agentTools.gitRead", true),
        ("git_diff", "agentTools.gitRead", true),
        ("git_log", "agentTools.gitRead", true),
        ("git_stage", "agentTools.gitWrite", false),
        ("git_commit", "agentTools.gitWrite", false),
        ("rule_create", "agentTools.governanceWrite", true),
        ("rule_update", "agentTools.governanceWrite", true),
        ("rule_submit", "agentTools.governanceWrite", true),
        ("rule_activate", "agentTools.governanceWrite", true),
        ("rule_block", "agentTools.governanceWrite", true),
        ("rule_archive", "agentTools.governanceWrite", true),
        ("rule_fork", "agentTools.governanceWrite", true),
        ("rule_reload", "agentTools.governanceWrite", true),
        ("rule_promote", "agentTools.governanceWrite", true),
        ("verdict_submit", "agentTools.governanceWrite", true),
        ("ws_create", "agentTools.governanceWrite", true),
        ("sandbox_start", "agentTools.governanceWrite", true),
        ("sandbox_close", "agentTools.governanceWrite", true),
        ("dataset_create", "agentTools.governanceWrite", true),
        ("publish_submit", "agentTools.governanceWrite", true),
        ("publish_list", "agentTools.governanceWrite", true),
        ("publish_queue_get", "agentTools.governanceWrite", true),
        ("publish_review", "agentTools.governanceWrite", true),
        ("publish_rollback", "agentTools.governanceWrite", true),
        ("bundle_export", "agentTools.governanceWrite", true),
        ("bundle_import_dry_run", "agentTools.governanceWrite", true),
        ("bundle_import", "agentTools.governanceWrite", true),
        // 自主交接族（自主交接设计 PR-H2）：单键同绑；PR-H3 起 session_spawn
        // 第三行同键
        ("handover_write", "agentTools.handover", true),
        ("handover_read", "agentTools.handover", true),
        ("session_spawn", "agentTools.handover", true),
        // 记忆主动消费
        ("memory_recall", "agentTools.memory_recall", true),
        // 审计查询
        ("audit_query", "agentTools.audit_query", true),
        // 记忆墓碑（默认关）
        ("memory_tombstone", "agentTools.memory_tombstone", false),
        // todo 任务清单族（批K 段4）：单键双绑同列
        ("todo_write", "agentTools.todo", true),
        ("todo_read", "agentTools.todo", true),
        // HTTP 写动词（批K 段5）：默认关（写动词不进缺省工具面）
        ("http_request", "agentTools.httpRequest", false),
        // 人在环澄清卡（批K 段7）：默认开（交互零风险，无文件/网络/子进程触点）
        ("ask_user", "agentTools.askUser", true),
        // 写面撤销（写面撤销批）：默认开（安全网能力——弹栈回退,不引入新内容）
        ("workspace_undo", "agentTools.workspaceUndo", true),
    ];

    #[test]
    fn test_switch_bindings_match_snapshot() {
        let manifests = all();
        let derived: HashMap<&str, (&str, bool)> = manifests
            .iter()
            .filter_map(|m| {
                m.default_switch
                    .as_ref()
                    .map(|s| (m.name.as_str(), (s.key.as_str(), s.default_on)))
            })
            .collect();
        assert_eq!(
            derived.len(),
            SWITCH_SNAPSHOT.len(),
            "开关绑定条数必须与 TOOL_SWITCH_KEYS 快照一致"
        );
        for (tool, key, default_on) in SWITCH_SNAPSHOT {
            let got = derived
                .get(tool)
                .unwrap_or_else(|| panic!("switch binding missing for '{tool}'"));
            assert_eq!(
                got,
                &(*key, *default_on),
                "switch binding drift for '{tool}'"
            );
        }
    }

    // ---------- 动态源（硬规则 3） ----------

    #[test]
    fn test_dynamic_manifest_rejects_empty_description() {
        let err = dynamic_manifest(
            "svc_x",
            ToolSource::ServiceProxy,
            String::new(),
            serde_json::json!({}),
        )
        .unwrap_err();
        assert!(err.contains("empty description"), "{err}");
    }

    #[test]
    fn test_dynamic_manifest_rejects_non_object_parameters() {
        let err = dynamic_manifest(
            "svc_x",
            ToolSource::ServiceProxy,
            "desc".to_string(),
            serde_json::json!("not-object"),
        )
        .unwrap_err();
        assert!(err.contains("not an object"), "{err}");
    }

    #[test]
    fn test_dynamic_manifest_inline_spec_resolves() {
        let m = dynamic_manifest(
            "svc_x",
            ToolSource::ServiceProxy,
            "a service tool".to_string(),
            serde_json::json!({"type": "object", "properties": {}}),
        )
        .unwrap();
        let (description, parameters) = m.resolve_spec().unwrap();
        assert_eq!(description, "a service tool");
        assert!(parameters.is_object());
    }

    #[test]
    fn test_dynamic_manifest_approval_policy_by_source() {
        // G8 接合：MCP 缺省 candidate 全审批
        // （ManualDefault）；ServiceProxy 维持 AutoPolicy（既有审批行为不变）
        let mcp = dynamic_manifest(
            "mcp_x",
            ToolSource::Mcp,
            "an mcp tool".to_string(),
            serde_json::json!({"type": "object", "properties": {}}),
        )
        .unwrap();
        assert_eq!(mcp.approval_policy, ApprovalPolicy::ManualDefault);

        let svc = dynamic_manifest(
            "svc_x",
            ToolSource::ServiceProxy,
            "a service tool".to_string(),
            serde_json::json!({"type": "object", "properties": {}}),
        )
        .unwrap();
        assert_eq!(svc.approval_policy, ApprovalPolicy::AutoPolicy);
    }

    #[test]
    fn test_dynamic_manifest_binds_source_switches() {
        // D6 裁定（2026-10-06 项目方批方案甲）：动态源绑定来源级开关，
        // 默认开（向后兼容）；消费点 = build_filtered_toolkit_with_switches。
        let mcp = dynamic_manifest(
            "mcp_x",
            ToolSource::Mcp,
            "an mcp tool".to_string(),
            serde_json::json!({"type": "object", "properties": {}}),
        )
        .unwrap();
        let binding = mcp.default_switch.expect("mcp manifest must bind a switch");
        assert_eq!(binding.key, AGENT_TOOLS_MCP);
        assert!(binding.default_on, "mcp switch must default ON");

        let svc = dynamic_manifest(
            "svc_x",
            ToolSource::ServiceProxy,
            "a service tool".to_string(),
            serde_json::json!({"type": "object", "properties": {}}),
        )
        .unwrap();
        let binding = svc
            .default_switch
            .expect("service proxy manifest must bind a switch");
        assert_eq!(binding.key, AGENT_TOOLS_SERVICE_PROXY);
        assert!(binding.default_on, "service proxy switch must default ON");
    }
}
