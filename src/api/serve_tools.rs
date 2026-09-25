// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! E1:serve 模式工具组装 —— union toolkit + 按白名单过滤
//!
//! serve 模式下 `cmd_serve` 在启动时调用 [`build_union_toolkit`] 一次,组装
//! 内置 6 + 规则 24 = 30 个工具的 union toolkit,存入 `AgentApiState.toolkit`。
//!
//! 每次 `/agents/{type}/run` 请求时,handler 调用 [`build_filtered_toolkit`]
//! 按 `def.tools` 白名单从 union 中过滤出该 agent 可用的工具,实现安全隔离。
//!
//! 本模块还提供 [`apply_l2_feed_forward`]——serve 三路径（WS / run / run-stream）
//! 共用的 L2 约束边界段前馈注入 helper。

use std::path::Path;

use crate::api::evorule_client::EvoruleApiClient;
use crate::api::workspace_client::WorkspaceApiClient;
use crate::builtin_tools::default_safe_toolkit;
use crate::io_handlers::tool_handler::ToolHandler;
use crate::rule_tools::full_rule_toolkit;

/// union toolkit 中包含的全部规则工具名(26 个)
///
/// workspace 2 + rule 12 + translate 3 + audit 3 + knowledge 3 + meta 1 + evolution 2 = 26
const RULE_TOOL_NAMES: &[&str] = &[
    // workspace_tools (2)
    "ws_list",
    "ws_create",
    // rule_tools (12)
    "rule_list",
    "rule_get",
    "rule_create",
    "rule_update",
    "rule_versions",
    "rule_version_get",
    "rule_submit",
    "rule_activate",
    "rule_block",
    "rule_archive",
    "rule_fork",
    "rule_reload",
    // translate_tools (3)
    "rule_to_transform",
    "rule_to_conditional",
    "rule_validate",
    // audit_tools (3)
    "audit_get",
    "audit_verify",
    "session_rewind",
    // knowledge_tools (3,只读消费面)
    "knowledge_datasets",
    "knowledge_search",
    "knowledge_entry_get",
    // meta_tools (1,L2 约束只读消费面)
    "meta_summary",
    // evolution_tools (2,进化信号只读消费面 + 约束层晋升提名)
    "evolution_signals",
    "rule_promote",
];

/// 构建 union toolkit(内置 6 + 规则 26 = 32 工具,启动时一次组装)
///
/// 在 `cmd_serve` 启动时调用一次,结果存入 `AgentApiState.toolkit`。
pub fn build_union_toolkit(
    workdir: &Path,
    ws: &WorkspaceApiClient,
    ev: &EvoruleApiClient,
) -> ToolHandler {
    let mut handler = default_safe_toolkit(workdir);
    let rule_handler = full_rule_toolkit(ws, ev);
    // 合并规则工具到 handler(按白名单逐个取出,保证只注册已知工具)
    for name in RULE_TOOL_NAMES {
        if let Some(tool) = rule_handler.get_tool(name) {
            handler.register_tool(name, tool);
        }
    }
    handler
}

// =============================================================================
// L2 约束边界段前馈注入（serve 三路径共用 helper）
// =============================================================================

/// 前馈触发工具集：白名单命中其一 = 具备规则生成/校验能力，才注入 L2 边界段
///
/// 纯消费 agent（如 researcher 类）不命中 → 不注入（对齐「仅规则草稿请求注入」语义）。
const L2_FEED_FORWARD_TRIGGER_TOOLS: &[&str] = &["rule_create", "rule_update", "rule_validate"];

/// 前馈触发条件判定（确定性）：`tools ∩ L2_FEED_FORWARD_TRIGGER_TOOLS ≠ ∅`
pub fn l2_feed_forward_triggered(tools: &[String]) -> bool {
    tools
        .iter()
        .any(|t| L2_FEED_FORWARD_TRIGGER_TOOLS.contains(&t.as_str()))
}

/// serve 三路径共用：L2 约束边界段前馈注入（拉取 + 渲染 + 追加 system_prompt 尾部）
///
/// - 触发条件：`tools ∩ {rule_create, rule_update, rule_validate} ≠ ∅`；
/// - 注入位置：`system_prompt` 尾部追加（memory recall 在 runner 内层包装，
///   既有语义顺序不变）；
/// - fail-soft：拉取失败/端点不可达 → warn 留痕 + 不注入，绝不阻断会话；
///   L2 清单为空 → 不注入。
///
/// 时效：每次 runner 构造实时拉取（无缓存）——元规则增删下一轮即反映。
pub async fn apply_l2_feed_forward(
    ev: &EvoruleApiClient,
    tools: &[String],
    system_prompt: &mut String,
) {
    if !l2_feed_forward_triggered(tools) {
        return;
    }
    let inv = match ev.get_l2_inventory().await {
        Ok(inv) => inv,
        Err(e) => {
            tracing::warn!(error = %e, "L2 feed-forward fetch failed; continuing without injection");
            return;
        }
    };
    if let Some(segment) = crate::rule_tools::meta_tools::render_l2_inventory_summary(&inv) {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(&segment);
    }
}

/// 进化信号感知段（serve 三路径共用,静态文本,无网络调用——确定性注入）
///
/// 构造期 runner 的进化会话尚未创建（runner.run 时才 create_session）,
/// 无法评估"是否有活跃信号"；活跃信号由 LLM 运行期经 evolution_signals
/// 工具实时拉取（会话口径）,本段只负责工具感知。触发条件与 L2 前馈一致
/// （命中起草族）；纯文本追加,fail-soft 天然满足。
pub async fn apply_evolution_signals_awareness(
    _ev: &EvoruleApiClient,
    tools: &[String],
    system_prompt: &mut String,
) {
    if !l2_feed_forward_triggered(tools) {
        return;
    }
    system_prompt.push_str("\n\n");
    system_prompt.push_str(EVOLUTION_AWARENESS_SEGMENT);
}

/// 进化信号感知段文本（与 evolution_tools 渲染模板同属展示层,禁内部编号字样）
const EVOLUTION_AWARENESS_SEGMENT: &str = "【进化信号感知】\n\
你具备 evolution_signals 工具（只读）。若任务涉及一个已有会话且其存在反复被强制拦截的违规,\
先用该工具拉取该会话的进化信号摘要,再围绕高频违规起草改进规则；无信号或与任务无关时跳过。";

// =============================================================================
// M1 规范入口索引感知段（serve 三路径共用,静态文本,无网络调用——确定性注入）
// =============================================================================

/// 规范入口索引感知段（BACKLOG M1,2026-09-25）
///
/// 内容源 = `D:\knowledge\7-Expert\INDEX-规范入口-给agent.md` v1.0 的**产品端
/// 投影**（立项-M1 §二方案 A 裁决）。投影三原则：
/// - 速查自足：核心程序（三问/留痕格式）全文在段内,agent 不依赖外档即可执行；
/// - 知识地图：知道什么规范存在、归哪个域；
/// - 边界如实：规范原档在宿主侧知识库、沙箱内不可达,如实声明（对齐 M5-a
///   「不要猜测或编造」）——第一案 P4 实测原档 `D:\knowledge\7-Expert` 物理
///   不可达,原「search_files 定向检索原档」设想按实测修表转译。
///
/// 权威源与同步纪律：索引 v1.0 变更（新增规范档位/核心条款修订）必须同步
/// 本投影,变更留痕于索引档头部与 CHANGELOG。
const REGULATION_INDEX_AWARENESS_SEGMENT: &str = "【规范入口索引】(什么问题查什么档;遇到「做不到/缺能力/不可达/访问被拒」等受限情形,先过规范程序再行动,禁止臆答)\n\
一、能力缺口三问——凡遇受限情形(做不到/缺能力/不可达/访问被拒),禁止直接出补齐方案或只作边界说明,必须先依次回答:\n\
①缺陷还是特性?evorule 是反缺省架构,「做不到」更可能是刻意边界;\n\
②该补在哪层?由外向内:外层预结构化喂字段→治理程序→视图→上下文;「扩内核」永远是最后选项;\n\
③确需动内核→过红线核验:停止并上报,不擅自执行。\n\
二、留痕纪律——修复必登记、变更必同步文档、发现非本任务范围的存量问题→登记册留痕、关键决策必留痕。\n\
三、确定性红线——确定/可回放/可审计不可触碰;改动只落展示层,永不进入被哈希/被重放的机器事实与协议标识符。\n\
四、宿主唯一——一切行为通道以 evorule 引擎为唯一入口,禁止绕引擎建第二执行通道。\n\
五、规则判定一句话——违规机器可判→规则层;纯知识风格→上下文,永不规则化。\n\
受限情形的答复必须以【三问留痕】段显式留痕,格式:【三问留痕】①缺陷/特性判定:<结论+依据>;②归属层:<层+方案>;③红线核验:<不需/已过程序>。\n\
知识地图(规范原档在宿主侧知识库,你的沙箱内不可达;需要深读时如实说明并请宿主提供,不要猜测或编造):\n\
能力缺口三问程序与案例→宪法档;规则落成/分层/治理→design-07/08/09;产品方向取舍→evo-agent 产品化指导;既有问题→OBSERVATIONS 登记册。";

/// 规范入口索引感知段注入（serve 三路径共用,静态文本,无网络调用）
///
/// 与 [`apply_evolution_signals_awareness`] 同族但**无触发条件**——规范程序
/// 是所有 serve 会话的通用素养（第一案失败场景 = general agent 纯文件任务
/// 同样需要）,全量注入。纯文本追加,fail-soft 天然满足。
/// 范围裁决（立项-M1 §3.2）：仅 serve 三路径;CLI/driver/delegate 不注入
/// （开发工具链自有规范通道,token 形态零变化）。
pub fn apply_regulation_index_awareness(system_prompt: &mut String) {
    system_prompt.push_str("\n\n");
    system_prompt.push_str(REGULATION_INDEX_AWARENESS_SEGMENT);
}

/// 按 agent 白名单过滤 toolkit(serve 模式安全隔离)
///
/// 从 union toolkit 中只取出 `whitelist` 中列出的工具,构造一个新的
/// `ToolHandler`。未注册的工具名会被静默跳过(防御性:agent.json 写了
/// 不存在的工具名不应 500,`from_definition` 会兜底报错)。
pub fn build_filtered_toolkit(union: &ToolHandler, whitelist: &[String]) -> ToolHandler {
    let mut filtered = ToolHandler::new();
    for name in whitelist {
        if let Some(tool) = union.get_tool(name) {
            filtered.register_tool(name, tool);
        }
    }
    filtered
}

// =============================================================================
// M5-a 能力边界:生效边界合成 + 声明绑定(serve 三路径与 CLI 共用)
// =============================================================================

/// Windows canonicalize 产物带 `\\?\` verbatim 前缀,边界展示面(LLM 自知
/// 边界/系统提示段)用常规形态;`\\?\UNC\` 还原为 `\\server\share`。
/// 仅影响展示与纯路径运算的自洽形态,不含 fs 语义变更。
fn simplify_verbatim(p: std::path::PathBuf) -> std::path::PathBuf {
    if let Some(s) = p.to_str() {
        if let Some(stripped) = s.strip_prefix(r"\\?\UNC\") {
            return std::path::PathBuf::from(format!(r"\\{}", stripped));
        }
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            return std::path::PathBuf::from(stripped);
        }
    }
    p
}

/// 合成生效能力边界:显式声明优先;未声明时按启动配置合成缺省(行为同 v1.0)
///
/// 单一事实源纪律(宪法 §七 反模式④):`capability_boundary` 是边界值唯一
/// 权威,启动配置仅作为「未声明时」的缺省来源——两态合一处产出,禁两处
/// 各存一份 root。
pub fn effective_capability_boundary(
    def: &crate::agent::definition::AgentDefinition,
    startup_workdir: &Path,
) -> crate::agent::definition::CapabilityBoundary {
    use crate::agent::definition::{CapabilityBoundary, SANDBOX_CAPABLE_TOOLS};
    if let Some(b) = &def.capability_boundary {
        return b.clone();
    }
    let sandbox_tools: Vec<String> = def
        .tools
        .iter()
        .filter(|t| SANDBOX_CAPABLE_TOOLS.contains(&t.as_str()))
        .cloned()
        .collect();
    let mode = if sandbox_tools.iter().any(|t| t == "file_write") {
        "read_write"
    } else {
        "read_only"
    };
    // 缺省合成:沙箱根 = 启动 workdir;模式按是否含写类工具如实判定;
    // 边界内工具 = 顶层 tools ∩ 沙箱类工具
    // O-119①:合成根必须为绝对路径——相对 workdir 启动时若原样入边界,
    // LLM 面展示的沙箱边界是相对路径,agent 无法自知绝对边界(瞎子摸象)。
    // canonicalize 失败(目录尚不存在等)时 fallback cwd 拼接——Path::join
    // 遇绝对路径自动替换基准,相对/绝对两态皆正确。
    let sandbox_root = simplify_verbatim(startup_workdir.canonicalize().unwrap_or_else(|_| {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(startup_workdir)
    }));
    CapabilityBoundary {
        mode: mode.to_string(),
        sandbox_root,
        tools: sandbox_tools,
    }
}

/// 按「显式声明」重建 file 类工具沙箱绑定(声明缺省 → no-op,union 实例
/// 已绑定启动 workdir,单一事实源成立)
///
/// 显式声明 = 唯一权威:`file_read` 按 `sandbox_root` 重建;`file_write` 在
/// `read_write` 模式下按 `sandbox_root` 全域可写(writable_dir = ".")。
/// 声明合法性(模式/工具一致性)由 `AgentDefinition::validate` 门卫先行保证。
pub fn apply_capability_boundary(
    handler: &mut ToolHandler,
    declared: Option<&crate::agent::definition::CapabilityBoundary>,
) {
    let Some(b) = declared else {
        return;
    };
    let root = b.sandbox_root.clone();
    if b.tools.iter().any(|t| t == "file_read") {
        handler.register_tool(
            "file_read",
            std::sync::Arc::new(crate::builtin_tools::file_read::FileReadTool::new(
                root.clone(),
            )),
        );
    }
    if !b.is_read_only() && b.tools.iter().any(|t| t == "file_write") {
        handler.register_tool(
            "file_write",
            std::sync::Arc::new(
                crate::builtin_tools::file_write::FileWriteTool::new(root).with_writable_dir("."),
            ),
        );
    }
}

/// M5-a:serve/CLI 共用一步接线 —— 合成生效边界 + (显式声明时)重绑工具面
///
/// 返回生效边界供调用方传给 `AgentRunner::with_capability_boundary`。
pub fn wire_capability_boundary(
    handler: &mut ToolHandler,
    def: &crate::agent::definition::AgentDefinition,
    startup_workdir: &Path,
) -> crate::agent::definition::CapabilityBoundary {
    let declared = def.capability_boundary.clone();
    let effective = effective_capability_boundary(def, startup_workdir);
    apply_capability_boundary(handler, declared.as_ref());
    effective
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_clients() -> (WorkspaceApiClient, EvoruleApiClient) {
        (
            WorkspaceApiClient::new("http://localhost:0"),
            EvoruleApiClient::new("http://localhost:0"),
        )
    }

    #[test]
    fn test_build_union_toolkit_registers_30_tools() {
        let (ws, ev) = make_clients();
        let handler = build_union_toolkit(Path::new("."), &ws, &ev);

        // 6 个内置工具
        for name in [
            "file_read",
            "file_list",
            "file_write",
            "search_files",
            "shell_exec",
            "http_get",
        ] {
            assert!(
                handler.has_tool(name),
                "builtin tool {} should be registered",
                name
            );
        }

        // 23 个规则工具
        for name in RULE_TOOL_NAMES {
            assert!(
                handler.has_tool(name),
                "rule tool {} should be registered",
                name
            );
        }

        // 总数 = 6 + 25 = 31(逐个验证所有预期工具都在)
        let all_names: Vec<&str> = [
            "file_read",
            "file_list",
            "file_write",
            "search_files",
            "shell_exec",
            "http_get",
        ]
        .iter()
        .copied()
        .chain(RULE_TOOL_NAMES.iter().copied())
        .collect();
        assert_eq!(all_names.len(), 32, "expected 32 total tool names");
        for name in &all_names {
            assert!(
                handler.has_tool(name),
                "tool {} missing from union toolkit",
                name
            );
        }
    }

    #[test]
    fn test_agents_definitions_whitelist_resolvable_in_union() {
        // agents/*.json 的 tools 白名单必须全部能从 union toolkit 解析
        // (agent 白名单声明了 union 中不存在的工具名 = 该能力实际不可用)
        let (ws, ev) = make_clients();
        let union = build_union_toolkit(Path::new("."), &ws, &ev);

        let defs = [
            ("general.json", include_str!("../../agents/general.json")),
            (
                "rule-copilot.json",
                include_str!("../../agents/rule-copilot.json"),
            ),
            (
                "researcher.json",
                include_str!("../../agents/researcher.json"),
            ),
        ];
        for (file, raw) in defs {
            let def: serde_json::Value = serde_json::from_str(raw)
                .unwrap_or_else(|e| panic!("{file} is not valid JSON: {e}"));
            let tools = def["tools"]
                .as_array()
                .unwrap_or_else(|| panic!("{file} missing tools array"));
            assert!(!tools.is_empty(), "{file} has empty tools whitelist");
            for tool in tools {
                let name = tool.as_str().unwrap();
                assert!(
                    union.has_tool(name),
                    "{file} whitelists tool '{name}' which is not in the serve union toolkit"
                );
            }
        }
    }

    #[test]
    fn test_build_filtered_toolkit_whitelist_subset() {
        let (ws, ev) = make_clients();
        let union = build_union_toolkit(Path::new("."), &ws, &ev);

        // 只取 3 个工具的白名单
        let whitelist: Vec<String> = vec![
            "file_read".to_string(),
            "rule_list".to_string(),
            "audit_get".to_string(),
        ];
        let filtered = build_filtered_toolkit(&union, &whitelist);

        assert!(filtered.has_tool("file_read"));
        assert!(filtered.has_tool("rule_list"));
        assert!(filtered.has_tool("audit_get"));
        // 白名单外的工具不应存在
        assert!(!filtered.has_tool("file_write"));
        assert!(!filtered.has_tool("shell_exec"));
        assert!(!filtered.has_tool("ws_create"));
    }

    #[test]
    fn test_build_filtered_toolkit_unknown_name_skipped() {
        let (ws, ev) = make_clients();
        let union = build_union_toolkit(Path::new("."), &ws, &ev);

        // 白名单含不存在的工具名 —— 应被静默跳过,不 panic
        let whitelist: Vec<String> = vec!["file_read".to_string(), "nonexistent_tool".to_string()];
        let filtered = build_filtered_toolkit(&union, &whitelist);

        assert!(filtered.has_tool("file_read"));
        assert!(!filtered.has_tool("nonexistent_tool"));
    }

    #[test]
    fn test_build_filtered_toolkit_empty_whitelist() {
        let (ws, ev) = make_clients();
        let union = build_union_toolkit(Path::new("."), &ws, &ev);

        let filtered = build_filtered_toolkit(&union, &[]);
        // 空白名单 → 空 toolkit(所有工具都不存在)
        assert!(!filtered.has_tool("file_read"));
        assert!(!filtered.has_tool("rule_list"));
    }

    #[test]
    fn test_get_tool_returns_some_for_registered() {
        let (ws, ev) = make_clients();
        let union = build_union_toolkit(Path::new("."), &ws, &ev);

        assert!(union.get_tool("file_read").is_some());
        assert!(union.get_tool("rule_list").is_some());
    }

    #[test]
    fn test_get_tool_returns_none_for_unregistered() {
        let (ws, ev) = make_clients();
        let union = build_union_toolkit(Path::new("."), &ws, &ev);

        assert!(union.get_tool("nonexistent").is_none());
    }

    // ===== L2 约束前馈注入 helper 测试 =====

    #[test]
    fn test_l2_feed_forward_triggered_by_rule_tools() {
        // 命中触发工具之一 → 注入
        assert!(l2_feed_forward_triggered(&["rule_create".to_string()]));
        assert!(l2_feed_forward_triggered(&[
            "rule_list".to_string(),
            "rule_update".to_string()
        ]));
        assert!(l2_feed_forward_triggered(&["rule_validate".to_string()]));
        // 纯消费白名单 → 不注入
        assert!(!l2_feed_forward_triggered(&[]));
        assert!(!l2_feed_forward_triggered(&[
            "file_read".to_string(),
            "rule_list".to_string(),
            "knowledge_search".to_string()
        ]));
    }

    #[tokio::test]
    async fn test_apply_l2_feed_forward_fail_soft() {
        // 端点不可达（localhost:0 连接失败）→ 不注入不 panic；未命中触发 → 不发请求直接返回
        let ev = EvoruleApiClient::new("http://localhost:0");
        let mut prompt = "base prompt".to_string();
        apply_l2_feed_forward(&ev, &["rule_create".to_string()], &mut prompt).await;
        assert_eq!(prompt, "base prompt", "拉取失败不得改动 system_prompt");

        // 未命中触发条件 → 不注入
        let mut prompt2 = "base prompt".to_string();
        apply_l2_feed_forward(
            &ev,
            &["file_read".to_string(), "rule_list".to_string()],
            &mut prompt2,
        )
        .await;
        assert_eq!(prompt2, "base prompt");
    }

    #[tokio::test]
    async fn test_evolution_signals_awareness_static_and_gated() {
        // 感知段是纯静态文本（无网络调用）：命中起草族 → 追加；未命中 → 不动
        let ev = EvoruleApiClient::new("http://localhost:0");
        let mut prompt = "base prompt".to_string();
        apply_evolution_signals_awareness(&ev, &["rule_create".to_string()], &mut prompt).await;
        assert!(prompt.contains("进化信号感知"), "命中起草族应追加感知段");
        assert!(prompt.starts_with("base prompt"));

        let mut prompt2 = "base prompt".to_string();
        apply_evolution_signals_awareness(&ev, &["knowledge_search".to_string()], &mut prompt2)
            .await;
        assert_eq!(prompt2, "base prompt", "未命中起草族不得改动 system_prompt");
    }

    #[test]
    fn test_union_toolkit_contains_evolution_signals() {
        let (ws, ev) = make_clients();
        let union = build_union_toolkit(Path::new("."), &ws, &ev);
        assert!(union.get_tool("evolution_signals").is_some());
        assert!(union.get_tool("meta_summary").is_some());
    }

    // ===== M1 规范入口索引感知段测试 =====

    #[test]
    fn test_regulation_index_segment_anchors() {
        // 关键锚点:三问程序/留痕格式/边界如实/知识地图——第一案 P1'/P2'/P3'
        // 检验点的机制载体,缺失任一即投影不完整
        let seg = REGULATION_INDEX_AWARENESS_SEGMENT;
        assert!(seg.contains("规范入口索引"), "段名锚点");
        assert!(seg.contains("能力缺口三问"), "三问程序锚点(P2')");
        assert!(seg.contains("缺陷还是特性"), "三问①锚点");
        assert!(seg.contains("归属层"), "三问②锚点");
        assert!(seg.contains("红线核验"), "三问③锚点");
        assert!(seg.contains("【三问留痕】"), "留痕格式锚点(P3')");
        assert!(seg.contains("禁止臆答"), "索引使用指令锚点(P1')");
        assert!(
            seg.contains("沙箱内不可达"),
            "边界如实锚点(对齐 M5-a 不编造)"
        );
        assert!(seg.contains("知识地图"), "知识地图锚点");
        // 权威源同步纪律的最低护栏:登记册/宪法/design 关键域在地图中可发现
        assert!(seg.contains("登记册"));
        assert!(seg.contains("宪法档"));
        assert!(seg.contains("design-07/08/09"));
    }

    #[test]
    fn test_apply_regulation_index_awareness_appends() {
        // 追加于尾部、原内容前缀保持、无条件注入(纯消费白名单同样注入)
        let mut prompt = "base prompt".to_string();
        apply_regulation_index_awareness(&mut prompt);
        assert!(prompt.starts_with("base prompt"));
        assert!(prompt.contains("【规范入口索引】"));
        assert!(prompt.ends_with("登记册。"), "段必须位于 prompt 尾部");

        // 无触发条件:任何工具清单形态都注入(helper 不接收 tools 参数,此处
        // 断言的是静态语义——第二次调用同样生效,幂等性由调用方保证)
        let mut prompt2 = String::new();
        apply_regulation_index_awareness(&mut prompt2);
        assert!(prompt2.starts_with("\n\n【规范入口索引】"));
    }

    // ===== M5-a 能力边界 helper 测试 =====

    use std::path::PathBuf;

    use crate::agent::definition::{AgentDefinition, CapabilityBoundary};

    fn make_boundary(mode: &str, root: PathBuf, tools: &[&str]) -> CapabilityBoundary {
        CapabilityBoundary {
            mode: mode.to_string(),
            sandbox_root: root,
            tools: tools.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn make_def(tools: &[&str], boundary: Option<CapabilityBoundary>) -> AgentDefinition {
        AgentDefinition {
            agent_type: "tester".to_string(),
            version: "1.0.0".to_string(),
            description: "test".to_string(),
            system_prompt: "p".to_string(),
            model: "test-model".to_string(),
            temperature: 0.7,
            max_steps: 5,
            step_timeout_secs: 30,
            tools: tools.iter().map(|s| s.to_string()).collect(),
            memory: Default::default(),
            output_format: None,
            context_window_tokens: None,
            max_parallel_tools: 1,
            capability_boundary: boundary,
        }
    }

    #[test]
    fn test_effective_boundary_declared_wins() {
        // 显式声明 = 唯一权威:启动 workdir 不参与合成(单一事实源)
        let declared_root = if cfg!(windows) {
            PathBuf::from("D:\\declared-root")
        } else {
            PathBuf::from("/tmp/declared-root")
        };
        let def = make_def(
            &["file_read", "file_list"],
            Some(make_boundary(
                "read_only",
                declared_root.clone(),
                &["file_read"],
            )),
        );
        let eff = effective_capability_boundary(&def, Path::new("."));
        assert!(eff.is_read_only());
        assert_eq!(eff.sandbox_root, declared_root);
        assert_eq!(eff.tools, vec!["file_read".to_string()]);
    }

    #[test]
    fn test_effective_boundary_default_synthesis() {
        // 未声明:沙箱根 = 启动 workdir 合成**绝对路径**(O-119①);含 file_write → read_write;工具取沙箱类交集
        let def = make_def(&["file_read", "file_write", "rule_list"], None);
        let eff = effective_capability_boundary(&def, Path::new("."));
        assert!(!eff.is_read_only());
        assert!(
            eff.sandbox_root.is_absolute(),
            "O-119①: synthesized sandbox_root must be absolute, got: {}",
            eff.sandbox_root.display()
        );
        assert_eq!(eff.tools.len(), 2);

        // 纯只读白名单 → read_only,tools 只含 file_read
        let def2 = make_def(&["file_read", "file_list", "knowledge_search"], None);
        let eff2 = effective_capability_boundary(&def2, Path::new("."));
        assert!(eff2.is_read_only());
        assert_eq!(eff2.tools, vec!["file_read".to_string()]);
    }

    #[test]
    fn test_effective_boundary_default_root_absolute_o119() {
        // O-119①:相对启动 workdir → 合成绝对沙箱根;目录不存在时 fallback
        // cwd 拼接(join 绝对路径自动替换基准),相对尾段保留
        let def = make_def(&["file_read"], None);
        let eff = effective_capability_boundary(&def, Path::new("some/relative/dir"));
        assert!(
            eff.sandbox_root.is_absolute(),
            "synthesized sandbox_root must be absolute, got: {}",
            eff.sandbox_root.display()
        );
        assert!(
            eff.sandbox_root
                .ends_with(Path::new("some").join("relative").join("dir")),
            "fallback join must preserve the relative tail, got: {}",
            eff.sandbox_root.display()
        );
    }

    #[test]
    fn test_simplify_verbatim_strips_windows_prefix() {
        // Windows canonicalize 产物去 \\?\ 前缀(展示面常规形态);UNC 还原
        assert_eq!(
            super::simplify_verbatim(std::path::PathBuf::from(r"\\?\D:\work")),
            std::path::PathBuf::from(r"D:\work")
        );
        assert_eq!(
            super::simplify_verbatim(std::path::PathBuf::from(r"\\?\UNC\srv\share")),
            std::path::PathBuf::from(r"\\srv\share")
        );
        // 无前缀(Unix 形态/相对路径)原样返回
        assert_eq!(
            super::simplify_verbatim(std::path::PathBuf::from("/tmp/x")),
            std::path::PathBuf::from("/tmp/x")
        );
    }

    #[tokio::test]
    async fn test_wire_no_declaration_is_noop() {
        // 未声明:union 实例维持启动 workdir 绑定(缺省合成复现现状)
        let startup = tempfile::tempdir().unwrap();
        let root = startup.path().canonicalize().unwrap();
        std::fs::write(root.join("startup_only.txt"), b"here").unwrap();

        let (ws, ev) = make_clients();
        let mut handler = build_union_toolkit(&root, &ws, &ev);
        let def = make_def(&["file_read", "file_list"], None);
        let eff = wire_capability_boundary(&mut handler, &def, &root);

        // O-119①:合成根 = 启动 workdir 的绝对化形态(去 \\?\ verbatim 前缀)
        assert_eq!(eff.sandbox_root, super::simplify_verbatim(root.clone()));
        let res = handler
            .execute_by_name(
                "file_read",
                &serde_json::json!({"path": "startup_only.txt"}),
            )
            .await;
        assert!(
            res.is_ok(),
            "no-declaration must keep startup binding: {:?}",
            res.err()
        );
    }

    #[tokio::test]
    async fn test_wire_declared_root_rebinds_file_read() {
        // 声明根 ≠ 启动 workdir:wire 后 file_read 按声明根解析,越界错误回报边界路径
        let declared_dir = tempfile::tempdir().unwrap();
        let startup_dir = tempfile::tempdir().unwrap();
        let declared_root = declared_dir.path().canonicalize().unwrap();
        let startup_root = startup_dir.path().canonicalize().unwrap();
        std::fs::write(declared_root.join("in_boundary.txt"), b"inside").unwrap();
        std::fs::write(startup_root.join("outside.txt"), b"outside").unwrap();

        let (ws, ev) = make_clients();
        let mut handler = build_union_toolkit(&startup_root, &ws, &ev);

        // 对照:wire 前 union 实例按启动 workdir 绑定,相对路径可读
        let pre = handler
            .execute_by_name("file_read", &serde_json::json!({"path": "outside.txt"}))
            .await;
        assert!(
            pre.is_ok(),
            "startup binding should read outside.txt before wire"
        );

        let def = make_def(
            &["file_read"],
            Some(make_boundary(
                "read_only",
                declared_root.clone(),
                &["file_read"],
            )),
        );
        let eff = wire_capability_boundary(&mut handler, &def, &startup_root);
        assert_eq!(eff.sandbox_root, declared_root);

        // 声明根内文件可读(相对路径按声明根解析)
        let ok = handler
            .execute_by_name("file_read", &serde_json::json!({"path": "in_boundary.txt"}))
            .await;
        assert!(
            ok.is_ok(),
            "boundary-root file must be readable after rebind: {:?}",
            ok.err()
        );

        // 原 workdir 独有文件(绝对路径) → 拒,且错误回报边界路径
        let outside_abs = startup_root.join("outside.txt");
        let err = handler
            .execute_by_name(
                "file_read",
                &serde_json::json!({"path": outside_abs.display().to_string()}),
            )
            .await;
        assert!(
            err.is_err(),
            "startup-only file must be rejected after rebind"
        );
        let msg = err.unwrap_err();
        assert!(
            msg.contains(&declared_root.display().to_string()),
            "error must report the boundary path, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_apply_read_write_declaration_enables_full_domain_write() {
        // read_write 声明 → writable_dir="."(沙箱根全域可写,不再限 workspace/ 子目录)
        let declared_dir = tempfile::tempdir().unwrap();
        let root = declared_dir.path().canonicalize().unwrap();

        // 对照:缺省实例(writable_dir=workspace)根下直写被拒
        let mut default_handler = ToolHandler::new();
        default_handler.register_tool(
            "file_write",
            std::sync::Arc::new(crate::builtin_tools::file_write::FileWriteTool::new(
                root.clone(),
            )),
        );
        let denied = default_handler
            .execute_by_name(
                "file_write",
                &serde_json::json!({"path": "direct.txt", "content": "x"}),
            )
            .await;
        assert!(
            denied.is_err(),
            "default binding must confine writes to workspace/"
        );

        // read_write 声明 apply 后:根下直写成功
        let mut handler = ToolHandler::new();
        handler.register_tool(
            "file_write",
            std::sync::Arc::new(crate::builtin_tools::file_write::FileWriteTool::new(
                root.clone(),
            )),
        );
        let b = make_boundary("read_write", root.clone(), &["file_write"]);
        apply_capability_boundary(&mut handler, Some(&b));
        let ok = handler
            .execute_by_name(
                "file_write",
                &serde_json::json!({"path": "direct.txt", "content": "x"}),
            )
            .await;
        assert!(
            ok.is_ok(),
            "read_write declaration implies full-domain write: {:?}",
            ok.err()
        );
        assert!(root.join("direct.txt").exists());
    }
}
