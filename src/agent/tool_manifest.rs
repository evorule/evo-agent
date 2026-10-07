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
//! 2. 静态层锁定：内置 16 + 规则 46 + delegate 1 + memory 4 的 manifest 为
//!    静态表（[`static_manifests`]）；测试断言数量、名称无重叠、spec 可解析、
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
//!   enforce——git 写族 2 + 规则治理写族 19 + delegate 1）；
//! - [`AdjudicationClass::Standard`]：不走 P2 裁决（机制层治理或纯落链）。
//!   `is_governance_adjudication_tool` 改由 manifest 派生后，命中集合恰等于
//!   原 GOVERNANCE_ADJUDICATION_TOOLS 24 表；delegate 统一装配批兑现模块
//!   注释承诺：delegate 升 Sensitive 并接 P2 通道（设计档 §3.2 终态），
//!   派生集合演进为 25 条（重录快照须 diff 审；数量锁 67 不变——级别
//!   变化不增减条目数）。D1 兑现批（2026-10-07）：file_write 升 Sentineled
//!   派生集合演进为 26 条（快照重录 diff 审；数量锁 74 不变）。
//!   file_write 的意图信号与 M5-c R1 通道（pending_target_scope，机制层
//!   由实现持有）并存互不干扰。
//!
//! **D6 终态（2026-10-06，B1 收官批）——P2 派生集合语义扩展**：从纯静态表
//! 扩展为「静态表 ∪ 动态注册条目（class≠Standard）」，判定 =
//! [`is_p2_adjudicated_runtime`]——静态命中分级以静态表为准（防降级：冒名
//! 注册不改变治理分级），静态未命中按 runtime manifest（MCP/ServiceProxy
//! 默认 Sensitive 意图必报，人工分级降档走 settings 键
//! `agentTools.mcpAdjudication` / `agentTools.serviceProxyAdjudication`，
//! D7-A 方案一来源级）。`GOVERNANCE_SNAPSHOT` 26 条快照锁静态面不变；
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
    /// 规则工具·透传（rule_tools adapter 表驱动，40 个）
    RuleTransparent,
    /// 规则工具·本地逻辑（rule_tools local_handlers，6 个）
    RuleLocal,
    /// 自省记忆工具（memory_tool，4 个）
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
}

/// 裁决分级（P2 事前意图裁决的通道分级，见模块文档）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdjudicationClass {
    /// 哨兵：P2 裁决 + 规则正本 enforce 已在场
    Sentineled,
    /// 敏感：P2 裁决在通道（意图必报，暂无 enforce）
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
// 静态表（硬规则 2：内置 20 + 规则 46 + delegate 1 + memory 4 = 71）
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

/// 内置工具 manifest（20 条，与 default_tool_specs 名称集合相等——测试锁）
fn builtin_manifests() -> Vec<ToolManifest> {
    use CapDomain::*;
    let b = |name: &str, domains: Vec<CapDomain>| -> ToolManifest {
        base(name, ToolSource::Builtin, SpecSource::Builtin, domains)
    };
    vec![
        b("file_read", vec![FsRead]),
        b("file_list", vec![FsRead]),
        b("file_write", vec![FsWrite]),
        b("file_create", vec![FsWrite]),
        b("file_move", vec![FsWrite]),
        b("file_delete", vec![FsWrite]),
        b("search_files", vec![FsRead]),
        b("grep_files", vec![FsRead]),
        b("shell_exec", vec![Process]),
        b("http_get", vec![Network]),
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
            // file_write：D1 兑现——规则正本第 4 条 enforce（out_of_sandbox）
            // 在场，升 Sentineled；审批面不动（写面高频工具维持既有审批策略，
            // 由 handler 内联沙箱检查 + 规则层 enforce 双层把守）
            "file_write" => {
                m.adjudication_class = AdjudicationClass::Sentineled;
            }
            // git 写族：P2 裁决在通道；D1 建议上 enforce（PR-5 规则正本入库时
            // 追加，升级 Sentineled），manifest 先如实标 Sensitive；
            // 同为 candidate 审批模式
            "git_stage" | "git_commit" => {
                m.adjudication_class = AdjudicationClass::Sensitive;
                m.approval_policy = ApprovalPolicy::ManualDefault;
            }
            // 命令/网络 candidate 族：分类器判入 candidate 即出 needs_approval
            // 提案（PR-4 起经管道⑤评估臂单源），如实补标 ManualDefault
            // （此前默认 AutoPolicy 属盘点遗漏，行为等价修正）
            "shell_exec" | "http_get" => {
                m.approval_policy = ApprovalPolicy::ManualDefault;
            }
            // grep_files 内部 30s 硬超时（与全局 60s 双层，如实标注）
            "grep_files" => m.timeout_class = TimeoutClass::Fast,
            _ => {}
        }
        // agentTools.* 开关绑定（原 TOOL_SWITCH_KEYS 内置段，快照测试锁等价）
        m.default_switch = match m.name.as_str() {
            "file_create" => sw(SWITCH_FILE_CREATE, true),
            "file_move" => sw(SWITCH_FILE_MOVE, false),
            "file_delete" => sw(SWITCH_FILE_DELETE, false),
            "grep_files" => sw(SWITCH_GREP, true),
            "git_status" | "git_diff" | "git_log" => sw(SWITCH_GIT_READ, true),
            "git_stage" | "git_commit" => sw(SWITCH_GIT_WRITE, false),
            _ => None,
        };
        m
    })
    .collect()
}

/// 规则治理写族（agentTools.governanceWrite 单键管全量，21 个）
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

/// 规则工具 manifest（49 条 = 透传 40 + 本地逻辑 6 + why/order 3，名称集合与
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
    // 本地逻辑 6（rule_tools::local_handlers::register 的注册名）
    for name in [
        "audit_verify",
        "bundle_export",
        "skill_pack_to_bundle",
        "meta_summary",
        "evolution_signals",
        "rule_promote",
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
        MEMORY_GET_TOOL, MEMORY_LINK_TOOL, MEMORY_PROPOSE_TOOL, MEMORY_SEARCH_TOOL,
        NOTE_WRITE_TOOL,
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

/// 全部静态 manifest（71 条；测试锁数量/唯一名/双侧集合相等）
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
    let (domains, default_switch) = match source {
        ToolSource::ServiceProxy => (
            vec![CapDomain::Governance],
            sw(AGENT_TOOLS_SERVICE_PROXY, true),
        ),
        ToolSource::Mcp => (vec![CapDomain::Process], sw(AGENT_TOOLS_MCP, true)),
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
        approval_policy: ApprovalPolicy::AutoPolicy,
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
        // 内置 20 + 规则 49 + delegate 1 + memory 6 = 76
        // （PR-11a 查账工具族 +4：16→20；PR-11b why/order 三工具 +3：
        //   规则 46→49，数量锁 71→74；memory_link +1：74→75；
        //   memory_forget +1：75→76）
        assert_eq!(builtin_manifests().len(), 20);
        assert_eq!(rule_manifests().len(), 49);
        assert_eq!(all().len(), 76);
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
    /// 后演进为 26 条（同款 diff 审纪律）。
    const GOVERNANCE_SNAPSHOT: &[&str] = &[
        "file_create",
        "file_move",
        "file_delete",
        "file_write",
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
            vec!["file_create", "file_delete", "file_move", "file_write"]
        );
    }

    /// 原 TOOL_SWITCH_KEYS 30 条快照（tool, key, default_on）
    const SWITCH_SNAPSHOT: &[(&str, &str, bool)] = &[
        ("file_create", "agentTools.fileCreate", true),
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
