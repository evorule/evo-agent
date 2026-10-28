// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 工作台设置体系（IDE 设置板块）
//!
//! 用户/工作区两级设置模型（对齐 VS Code User→Workspace 合并序）：
//!
//! - **注册表（schema）单源在 serve 端**：键 ID/类型/默认值/枚举/范围/分类/描述；
//!   前端设置 UI 渲染、校验提示与 JSON 补全全部消费
//!   `GET /api/workbench/settings/schema` 下发的条目，前后端零双声明。
//! - **User 层** = `data/workbench_settings.json`（serve 原子写；PUT 端点唯一写入口）。
//! - **Workspace 层** = `<workdir>/.evo/settings.json`（只读合并源；人工经文件面编辑，
//!   保存后下一次 GET 合并生效）。
//! - **合并序** Default→User→Workspace（后层覆盖前层），`sources` 标注每键生效层；
//!   损坏文件跳过该层（降级缺省），workspace 层内非法键值跳过该键（fail-soft）。
//!
//! 数据面边界（治理接点声明）：
//! - serve 端读写范围限定上述**两个固定路径**（构造期由 workdir 派生并过
//!   [`ensure_fixed_relative`] 白名单门，拒绝绝对路径与父目录穿越）；无任意路径写入口。
//! - 磁盘文档为扁平键对象（`{"editor.fontSize": 18}`，键 ID 即 schema 键）。
//! - 不含任何密钥/服务端配置——那是 evo-agent.toml 启动期配置域；设置页只消费
//!   `/admin/llm-status` 脱敏快照，零新增泄露面。
//! - 设置读写不落审批链（与按钮点击同级；settings 属工作台数据面非规则面）。

use std::path::{Component, Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tracing::warn;

use crate::api::agent_api::AgentApiState;

// =============================================================================
// 设置项注册表（schema；v1 共 7 键，默认值=现行为，零回归）
// =============================================================================

/// 设置值类型（schema 下发与校验共用）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingType {
    /// 布尔
    Boolean,
    /// 数值（可带 range 闭区间约束）
    Number,
    /// 字符串（v1 无此类型键，词汇表预留）
    String,
    /// 枚举（enum_values 白名单）
    Enum,
    /// 数组（v1 仅键位覆盖列表，元素形态另校验）
    Array,
}

/// 设置项注册表条目（声明即生效：UI 渲染/校验/默认值三驱动）
#[derive(Debug, Clone, Serialize)]
pub struct SettingEntry {
    /// 扁平键 ID（域前缀.名称，VS Code 事实标准）
    pub key: &'static str,
    /// 值类型（UI 按此渲染控件、校验按此判型）
    #[serde(rename = "type")]
    pub kind: SettingType,
    /// 默认值（= IDE 现行为，保证零回归）
    pub default: Value,
    /// enum 类型的候选值（其余类型为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<&'static str>>,
    /// number 类型的闭区间范围 `[min, max]`（其余类型为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<[i64; 2]>,
    /// UI 分组（编辑器/工作台/键位/AI）
    pub category: &'static str,
    /// 展示描述（设置页与 JSON 补全文档共用）
    pub description: &'static str,
    /// `window` | `application`（application 禁工作区覆盖；v1 无此类键，机制预留）
    pub scope: &'static str,
}

/// 设置项注册表（声明即生效；计数锁见测试 merged_defaults_when_no_files）。
///
/// - `editor.minimap` 默认 **false**：EditorPane 创建参数实态即显式关闭
///   （设计初稿记 true 为 Monaco 缺省口径，实施期按「默认值=现行为」原则修正）。
/// - `workbench.theme` v1 单值（消费方 EditorPane 两处创建参数原值）；主题明暗切换随主题板块扩展。
/// - `keybindings.overrides` 数据格式沿用命令面板覆盖层 `[{key, command, when?}]`，
///   仅存储点由浏览器 localStorage 迁入（读取端过滤非法条目，写入端同口径校验）。
/// - `ai.autocomplete.enabled` 为后续 AI 补全板块的预留键（落地前无消费方）。
pub fn schema() -> Vec<SettingEntry> {
    vec![
        SettingEntry {
            key: "editor.fontSize",
            kind: SettingType::Number,
            default: json!(14),
            enum_values: None,
            range: Some([9, 28]),
            category: "编辑器",
            description: "编辑器字体大小",
            scope: "window",
        },
        SettingEntry {
            key: "editor.tabSize",
            kind: SettingType::Number,
            default: json!(4),
            enum_values: None,
            range: Some([1, 8]),
            category: "编辑器",
            description: "一个制表符等效的空格数",
            scope: "window",
        },
        SettingEntry {
            key: "editor.wordWrap",
            kind: SettingType::Enum,
            default: json!("off"),
            enum_values: Some(vec!["on", "off"]),
            range: None,
            category: "编辑器",
            description: "控制折行方式：on 按视口宽度折行，off 不折行",
            scope: "window",
        },
        SettingEntry {
            key: "editor.minimap",
            kind: SettingType::Boolean,
            default: json!(false),
            enum_values: None,
            range: None,
            category: "编辑器",
            description: "是否显示小地图（缩略图）",
            scope: "window",
        },
        SettingEntry {
            key: "workbench.theme",
            kind: SettingType::Enum,
            default: json!("evorule-dark"),
            enum_values: Some(vec!["auto", "evorule-dark", "evorule-light"]),
            range: None,
            category: "工作台",
            description: "工作台配色主题（auto=跟随系统深浅；evorule-dark=深色；evorule-light=浅色）",
            scope: "window",
        },
        SettingEntry {
            key: "workbench.uiFontSize",
            kind: SettingType::Number,
            default: json!(13),
            enum_values: None,
            range: Some([11, 18]),
            category: "工作台",
            description: "界面基准字号（px）：联动全部 UI 令牌面（侧栏/面板/状态栏/对话等）；编辑器与终端字号独立设置（editor.fontSize / terminal.fontSize），不随本键变动",
            scope: "window",
        },
        SettingEntry {
            key: "keybindings.overrides",
            kind: SettingType::Array,
            default: json!([]),
            enum_values: None,
            range: None,
            category: "键位",
            description: "自定义键位覆盖列表，元素形如 {\"key\": \"ctrl+shift+p\", \"command\": \"命令 ID\"}（可选 when 子句）",
            scope: "window",
        },
        SettingEntry {
            key: "ai.autocomplete.enabled",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "AI",
            description: "AI 内联补全开关（补全能力上线后生效）",
            scope: "window",
        },
        SettingEntry {
            key: "aiCommit.enabled",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "AI",
            description: "源代码管理提交框的 AI 生成提交消息（✨）开关：staged 变更交给所配 LLM 起草提交消息写入输入框供编辑；生成只读不提交，提交动作始终在人",
            scope: "window",
        },
        SettingEntry {
            key: "aiCommit.excludePaths",
            kind: SettingType::Array,
            default: json!([
                "**/package-lock.json",
                "**/pnpm-lock.yaml",
                "**/yarn.lock",
                "**/Cargo.lock",
                "**/poetry.lock",
                "**/*.min.js",
                "**/*.min.css",
                "**/dist/**",
                "**/build/**"
            ]),
            enum_values: None,
            range: None,
            category: "AI",
            description: "AI 生成提交消息时从 diff 输入排除的路径 glob（lockfile/构建产物等噪音文件；命中文件只留摘要行，不进 prompt）",
            scope: "window",
        },
        SettingEntry {
            key: "agentTools.fileCreate",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 创建文件/目录（file_create；开启后每次调用默认需人工审批）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.editFile",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 精准编辑已有文件（edit_file，锚串替换+唯一性闸，UTF-8/CRLF 保真；沙箱与审计同 file_write 写面，随开随用）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.workspaceUndo",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 回退最近一次工作区写面操作（workspace_undo，写前快照弹栈恢复——覆盖写字节级写回/新建文件删除；恢复路径复验写面白名单并全程落账；安全网能力随开随用）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.webFetch",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 抓取网页并转为可读文本（web_fetch，http_get 防线同源复用：active host 直取/candidate host 审批/SSRF 永拒；随开随用）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.webSearch",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 互联网搜索（web_search，经本机 ai-plugin 中继调用 Brave Search API，搜索凭据仅存 ai-plugin 侧；未配置时 fail-visible 自诊断）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.codeSearch",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 符号图谱检索（code_search，基于工作区索引的符号级代码搜索，tree-sitter + PageRank 排位，纯结构面无本地模型；需 index.enabled 开启索引服务）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.fileMove",
            kind: SettingType::Boolean,
            default: json!(false),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 移动/重命名文件（file_move；开启后每次调用默认需人工审批）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.httpRequest",
            kind: SettingType::Boolean,
            default: json!(false),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 发 HTTP 写请求（http_request，POST/PUT/PATCH/DELETE；全批最严治理面：SSRF 防线 + 敏感意图裁决 + 每次调用人工审批双闸，Active 域亦不直跑，redirect 禁跟随；默认关——显式启用才进 LLM 契约，启用后仍走双闸全流审批）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.fileDelete",
            kind: SettingType::Boolean,
            default: json!(false),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 删除文件（file_delete，软删除进回收目录；开启后每次调用默认需人工审批）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.grep",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 全项目内容搜索（grep_files，只读；随开随用）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.gitRead",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 读取 git 状态（git_status/git_diff/git_log，只读；随开随用）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.gitWrite",
            kind: SettingType::Boolean,
            default: json!(false),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 执行 git 写操作（git_stage/git_commit；开启后每次调用默认需人工审批）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.governanceWrite",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 执行治理写操作（rule 写 8 + rule_promote + ws_create + sandbox_start/close + dataset_create + publish ×5 + bundle export/import ×3，共 21 个工具；关闭 = 敏感部署只读模式：写工具连 LLM 契约一起下线，LLM 仍可查询与辅助起草，提交/晋升/发布权回到人）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.handover",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "自主交接工具（handover_write/handover_read）：会话收尾写结构化交接档、续接会话读档核对（关闭 = 交接族连 LLM 契约一起下线，长任务仅靠压缩兜底）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.memory_recall",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "记忆主动消费工具（memory_recall，只读）：agent 会话中按需检索稳定事实/事件账本（此前仅启动期被动回注；关闭 = 工具连 LLM 契约一起下线，记忆回流退回被动回注单通道）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.audit_query",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "审计查询工具（audit_query，只读）：四 scope 入口——gov 治理留痕（含批准先例卡）/shared 共享账本前缀检索（含 /mcp 事件轨道）/session 会话审计与因果链/journal 事件流（关闭 = 工具连 LLM 契约一起下线，agent 无法主动查询治理留痕与会话审计）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.memory_tombstone",
            kind: SettingType::Boolean,
            default: json!(false),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "记忆墓碑工具（memory_tombstone，写入面）：对已固化的沉淀错误事实写 null 墓碑使其视图不可见（append-only 原值永在账；仅本命名空间 stable/events 两分区；reason 强制且永久入审计链；默认关——显式启用才进 LLM 契约，启用后仍走敏感意图裁决与人工审批）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.todo",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "todo 任务清单族（todo_write/todo_read，单键双绑）：会话内结构化任务清单的全量重写读写——关闭 = 两工具连 LLM 契约一起下线，且 runner 装配注入面同键判定同步缺席（清单摘要不再注入 system prompt，防「工具在、注入断」的单腿态）；进程内内存态零外溢面，默认开",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.askUser",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "人在环澄清卡（ask_user）：agent 在关键分歧点向用户发结构化问题并阻塞等回答（单选/多选/自由文本，超时缺省 300s）——关闭 = 工具连 LLM 契约一起下线，等待面同步缺席；交互零风险（无文件/网络/子进程触点），问题只发给会话数据有权方（用户本人），默认开",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.eventLedger",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "事件型记忆轨道：工具完成/轮里程碑/审批决定/会话终态四类结构化事件写入记忆账本（每轮工具事件限 5 条防膨胀；关闭 = 事件轨道停写，sediment 叙事沉淀与治理留痕不受影响）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.mcp",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 调用 MCP 远端工具（mcp.servers 配置握手后注册的动态工具全量；关闭 = 该来源工具连 LLM 契约一起下线，能力面不存在，非\"调用被拒\"）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.serviceProxy",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 调用服务代理工具（治理服务对账清单注册的动态工具全量；关闭 = 该来源工具连 LLM 契约一起下线，能力面不存在，非\"调用被拒\"）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.mcpAdjudication",
            kind: SettingType::Enum,
            default: json!("sensitive"),
            enum_values: Some(vec!["sensitive", "standard"]),
            range: None,
            category: "Agent 工具",
            description: "MCP 动态工具的裁决分级（D6 方案甲：默认 sensitive = 每次调用事前意图裁决留账；standard = 免意图裁决直执行。人工分级后可降档，分级不影响开关——agentTools.mcp 管在场性，本键管裁决性）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.serviceProxyAdjudication",
            kind: SettingType::Enum,
            default: json!("sensitive"),
            enum_values: Some(vec!["sensitive", "standard"]),
            range: None,
            category: "Agent 工具",
            description: "服务代理动态工具的裁决分级（语义同 agentTools.mcpAdjudication，作用于服务代理来源）",
            scope: "application",
        },
        SettingEntry {
            key: "mcpServer.enabled",
            kind: SettingType::Boolean,
            default: json!(false),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "把 serve 工具面以 MCP 协议暴露为 /mcp 端点（B23；缺省关闭=fail-closed。开启后外部 MCP 客户端可接入，工具面与 LLM 完全一致【治理等价】，审批链共用同一执行体；鉴权复用 serve Bearer token，重启 serve 生效）",
            scope: "application",
        },
        SettingEntry {
            key: "approval.mode.shellExec",
            kind: SettingType::Enum,
            default: json!("manual"),
            enum_values: Some(vec!["manual", "auto_policy", "policy"]),
            range: None,
            category: "Agent 工具",
            description: "shell_exec 的审批档位（agent 定义未显式指定 approval_mode 时的缺省档位：manual=每次人工审批；auto_policy=判定式无人值守放行；policy=对照 .evo/approval-policy.toml 预置条件由独立评审自动判定，全过放行、不过退回修正，重查超限自动回落人工。policy 是进阶选项：需先在条件文件预置审批条件，文件缺失或无效时该档自动回落人工，绝不静默放行。重启会话生效）",
            scope: "application",
        },
        SettingEntry {
            key: "dap.adapters",
            kind: SettingType::String,
            default: json!("{}"),
            enum_values: None,
            range: None,
            category: "运行和调试",
            description: "调试适配器白名单（运行和调试板块，fail-closed：缺省空=拒绝任何适配器 spawn）。JSON 对象形态：{\"适配器名\": {\"command\": \"可执行文件\", \"args\": [\"启动参数\"], \"langs\": [\"语言标签\"]}}，如 {\"rust\": {\"command\": \"lldb-dap\"}}。仅白名单内的适配器可经 /api/dap/ws 启动调试会话；重启 serve 生效",
            scope: "application",
        },
        SettingEntry {
            key: "approval.exemptionOverride",
            kind: SettingType::Boolean,
            default: json!(false),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "关闭红线豁免强制人工审批（默认关闭=file_delete/push 远端/数据外发/改治理配置文件四类操作在任何档位下都强制人工审批）。开启后上述操作将由审批评审依条件自动判定，判定留痕携带 exemption_overridden 标记。请确认理解风险后再开启；此开关不影响 blocked 级拒绝（不可配置绕过）",
            scope: "application",
        },
        SettingEntry {
            key: "search.maxResults",
            kind: SettingType::Number,
            default: json!(1000),
            enum_values: None,
            range: Some([100, 20000]),
            category: "搜索",
            description: "全局搜索单次返回的匹配数上限（超出部分截断并提示）",
            scope: "window",
        },
        SettingEntry {
            key: "search.smartCase",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "搜索",
            description: "智能大小写：查询含大写字母时才区分大小写（全小写查询不敏感）",
            scope: "window",
        },
        SettingEntry {
            key: "search.useIgnoreFiles",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "搜索",
            description: "全局搜索尊重 .gitignore 等忽略文件",
            scope: "window",
        },
        SettingEntry {
            key: "search.excludeGlobs",
            kind: SettingType::Array,
            default: json!([
                ".git/**",
                "target/**",
                "node_modules/**",
                ".evo-trash/**",
                "data/**"
            ]),
            enum_values: None,
            range: None,
            category: "搜索",
            description: "全局搜索排除的路径 glob 列表（serve 端另有恒叠加工集）",
            scope: "window",
        },
        SettingEntry {
            key: "problems.json.validate",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "问题",
            description: "JSON 文件语法校验（编辑器内实时波浪线与问题面板条目；关闭即清除现有标记）",
            scope: "window",
        },
        SettingEntry {
            key: "problems.markdown.lint",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "问题",
            description: "Markdown 轻量校验（标题跳级/重复、代码围栏配对、行尾空白；关闭即清除现有标记）",
            scope: "window",
        },
        SettingEntry {
            key: "completion.enabled",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "AI",
            description: "AI 行内补全（ghost text；打字停顿自动触发，内容外发至当前 LLM 配置端点；.evoignore 命中文件零外发）",
            scope: "window",
        },
        SettingEntry {
            key: "inlineChat.enabled",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "AI",
            description: "AI Inline Chat（编辑器内 Ctrl+K 显式触发改写；每次调用留痕治理日志；.evoignore 命中文件零外发）",
            scope: "window",
        },
        SettingEntry {
            key: "task.autoRevealProblems",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "任务",
            description: "任务结束时自动打开问题面板（扫出问题/非零退出码时；跟随 onProblem 语义）",
            scope: "window",
        },
        SettingEntry {
            key: "terminal.shell",
            kind: SettingType::String,
            default: json!("auto"),
            enum_values: None,
            range: None,
            category: "终端",
            description: "终端 shell 可执行路径（auto = 按平台发现序：Windows PowerShell→CMD→Git Bash，unix $SHELL；指定路径则直接采用）",
            scope: "window",
        },
        SettingEntry {
            key: "terminal.fontSize",
            kind: SettingType::Number,
            default: json!(14),
            enum_values: None,
            range: Some([8, 32]),
            category: "终端",
            description: "终端字体大小",
            scope: "window",
        },
        SettingEntry {
            key: "terminal.scrollback",
            kind: SettingType::Number,
            default: json!(1000),
            enum_values: None,
            range: Some([100, 10000]),
            category: "终端",
            description: "终端回滚行数上限（内存护栏）",
            scope: "window",
        },
        SettingEntry {
            key: "lsp.enabled",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "语言服务器",
            description: "LSP 语言服务器接入（rust-analyzer / typescript-language-server，PATH 检测，缺失自动灰置；关闭即断开连接并清除全部 lsp.* 诊断）",
            scope: "window",
        },
        SettingEntry {
            key: "longSession.compaction.enabled",
            kind: SettingType::Boolean,
            default: json!(false),
            enum_values: None,
            range: None,
            category: "长会话",
            description: "长会话主动压缩：上下文用量达到阈值比例时，把近期轮次之外的早期对话生成滚动摘要，并清除区内大块工具结果原文（被清原文以 [cleared: 工具名] 引用替代，完整留存于会话 journal 可查；被动裁剪仍为最后防线）。默认关闭——被动裁剪（机器确定性面）为缺省路径，需要 LLM 摘要压缩时在此显式开启",
            scope: "window",
        },
        SettingEntry {
            key: "longSession.compaction.thresholdPct",
            kind: SettingType::Number,
            default: json!(70),
            enum_values: None,
            range: Some([50, 95]),
            category: "长会话",
            description: "触发主动压缩的上下文用量阈值（占上下文窗口的百分比，默认 70%；宁可早压缩，避免逼近窗口上限才被动裁剪）",
            scope: "window",
        },
        SettingEntry {
            key: "longSession.compaction.maxClearToolResults",
            kind: SettingType::Number,
            default: json!(20),
            enum_values: None,
            range: Some([0, 200]),
            category: "长会话",
            description: "单次主动压缩最多清除的工具结果条数（按原文长度从大到小清除，0 = 只生成摘要不清除原文）",
            scope: "window",
        },
        SettingEntry {
            key: "mentions.maxFileLines",
            kind: SettingType::Number,
            default: json!(600),
            enum_values: None,
            range: Some([50, 2000]),
            category: "对话",
            description: "@引用文件携带的全文行数上限（未打开且超限的文件降级为符号大纲并标注截断）",
            scope: "window",
        },
        SettingEntry {
            key: "mentions.terminalLines",
            kind: SettingType::Number,
            default: json!(200),
            enum_values: None,
            range: Some([20, 1000]),
            category: "对话",
            description: "@引用终端时携带的输出尾部行数",
            scope: "window",
        },
        SettingEntry {
            key: "index.enabled",
            kind: SettingType::Boolean,
            default: json!(false),
            enum_values: None,
            range: None,
            category: "工作台",
            description: "代码库语义索引（符号图谱检索，/api/index/*；默认关——开启后 serve 启动时对工作区建索引并持续增量维护，重启生效）",
            scope: "window",
        },
        SettingEntry {
            key: "index.semantic.enabled",
            kind: SettingType::Boolean,
            default: json!(false),
            enum_values: None,
            range: None,
            category: "工作台",
            description: "语义面向量检索（mode=semantic/hybrid，依赖 index.enabled）。需本地嵌入模型 all-MiniLM-L6-v2 q8（首次启用下载约 24MB，来源 HuggingFace，缓存于 ~/.evo-agent/models，之后离线运行）；仅含 semantic feature 的构建可用，缺省构建开启时明示降级纯结构面；重启生效",
            scope: "window",
        },
    ]
}

fn find_entry(key: &str) -> Option<SettingEntry> {
    schema().into_iter().find(|e| e.key == key)
}

// =============================================================================
// 校验（键白名单 + 类型 + range/enum + 数组元素形态）
// =============================================================================

/// 校验「键存在 + 值合法」。`value=None` 表示重置（只查键白名单）。
pub fn validate_put(key: &str, value: Option<&Value>) -> Result<(), String> {
    let entry = find_entry(key)
        .ok_or_else(|| format!("unknown setting key: {key} (not in the settings schema)"))?;
    let Some(v) = value else {
        return Ok(()); // 重置：移除用户层覆盖，回落默认/工作区值
    };
    match entry.kind {
        SettingType::Boolean => {
            if !v.is_boolean() {
                return Err(format!("setting '{key}' expects a boolean"));
            }
        }
        SettingType::Number => {
            let Some(n) = v.as_f64() else {
                return Err(format!("setting '{key}' expects a number"));
            };
            if !n.is_finite() {
                return Err(format!("setting '{key}' expects a finite number"));
            }
            if let Some([lo, hi]) = entry.range {
                let (lo, hi) = (lo as f64, hi as f64);
                if !(n >= lo && n <= hi) {
                    return Err(format!(
                        "setting '{key}' out of range: {n} (allowed {lo}..={hi})"
                    ));
                }
            }
        }
        SettingType::String => {
            if !v.is_string() {
                return Err(format!("setting '{key}' expects a string"));
            }
        }
        SettingType::Enum => {
            let Some(s) = v.as_str() else {
                return Err(format!(
                    "setting '{key}' expects one of: {}",
                    entry.enum_values.clone().unwrap_or_default().join("|")
                ));
            };
            let allowed = entry.enum_values.unwrap_or_default();
            if !allowed.contains(&s) {
                return Err(format!(
                    "setting '{key}' invalid value: {s} (allowed: {})",
                    allowed.join("|")
                ));
            }
        }
        SettingType::Array => {
            if !v.is_array() {
                return Err(format!("setting '{key}' expects an array"));
            }
            // v1 唯一数组键的元素契约：{key, command} 非空字符串对（when 可选），
            // 与键位覆盖层读取端的过滤口径一致，防脏数据入库。
            if key == "keybindings.overrides" {
                if let Some(arr) = v.as_array() {
                    for (i, el) in arr.iter().enumerate() {
                        let ok = el
                            .get("key")
                            .and_then(Value::as_str)
                            .is_some_and(|s| !s.trim().is_empty())
                            && el
                                .get("command")
                                .and_then(Value::as_str)
                                .is_some_and(|s| !s.trim().is_empty())
                            && el.get("when").map(|w| w.is_string()).unwrap_or(true);
                        if !ok {
                            return Err(format!(
                                "setting '{key}' element #{i} must be {{key, command}} strings (optional when)"
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// workspace 层键级守卫：application scope 拒工作区覆盖 + 值须过 schema 校验
fn accept_workspace_value(entry: &SettingEntry, v: &Value) -> bool {
    entry.scope != "application" && validate_put(entry.key, Some(v)).is_ok()
}

// =============================================================================
// 存储路径白名单（防目录穿越）
// =============================================================================

/// 路径白名单门：只接受「相对 base 的普通相对路径」，拒绝绝对路径/根相对路径
/// （`Path::join` 对根相对与绝对输入会整体替换 base，均构成越界面）与父目录穿越。
/// 两个存储路径构造期过此门；未来新增存储路径必须同门（回归护栏）。
pub fn ensure_fixed_relative(base: &Path, rel: &str) -> Result<PathBuf, String> {
    let p = Path::new(rel);
    if p.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(format!(
            "path not allowed (must be plain relative): '{rel}'"
        ));
    }
    Ok(base.join(p))
}

// =============================================================================
// 存储（User 层原子写；Workspace 层只读合并源）
// =============================================================================

/// 读一层 JSON 文档（缺文件/损坏 → 空层降级；非对象 → 空层）
fn load_layer(path: &Path) -> Map<String, Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Map::new();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(), // 损坏/非对象 → 空层（下次 PUT 原子写覆盖修复）
    }
}

/// 工作台设置存储（两级）
#[derive(Debug)]
pub struct WorkbenchSettingsStore {
    user_path: PathBuf,
    workspace_path: PathBuf,
}

impl WorkbenchSettingsStore {
    /// 构造（两个固定路径由 workdir 派生并过白名单门；常量天然合规，
    /// 门是防常量演化为动态输入的回归护栏）
    pub fn new(workdir: &Path) -> Result<Self, String> {
        Ok(Self {
            user_path: ensure_fixed_relative(workdir, "data/workbench_settings.json")?,
            workspace_path: ensure_fixed_relative(workdir, ".evo/settings.json")?,
        })
    }

    /// user 层文件路径（单测与诊断用）
    pub fn user_path(&self) -> &Path {
        &self.user_path
    }

    /// 合并读取：Default→User→Workspace（后层覆盖前层）。
    /// 返回 `(settings, sources)`；sources 每键标注 `default|user|workspace`。
    /// workspace 层未知键忽略、非法值/application scope 键跳过（fail-soft）。
    pub fn merged(&self) -> (Map<String, Value>, Map<String, Value>) {
        let user = load_layer(&self.user_path);
        let ws_raw = load_layer(&self.workspace_path);
        let mut workspace = Map::new();
        for (k, v) in ws_raw {
            if let Some(e) = find_entry(&k) {
                if accept_workspace_value(&e, &v) {
                    workspace.insert(k, v);
                }
            }
        }
        let mut settings = Map::new();
        let mut sources = Map::new();
        for e in schema() {
            let (v, src) = if let Some(w) = workspace.get(e.key) {
                (w.clone(), "workspace")
            } else if let Some(u) = user.get(e.key) {
                (u.clone(), "user")
            } else {
                (e.default.clone(), "default")
            };
            settings.insert(e.key.to_string(), v);
            sources.insert(e.key.to_string(), json!(src));
        }
        (settings, sources)
    }

    /// 写 user 层单键（`value=None` 表示重置=从 user 层移除该键；纯 IO 无校验，
    /// 校验由 [`validate_put`] 在端点层先行）。
    pub fn write_user_layer_key(&self, key: &str, value: Option<&Value>) -> Result<(), String> {
        let mut layer = load_layer(&self.user_path);
        match value {
            Some(v) => {
                layer.insert(key.to_string(), v.clone());
            }
            None => {
                layer.remove(key);
            }
        }
        self.save_atomic(&self.user_path, &layer)
    }

    /// 原子写（tmp + rename；父目录确保存在；Windows rename 覆盖已存在文件）
    fn save_atomic(&self, path: &Path, doc: &Map<String, Value>) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create settings dir failed: {e}"))?;
        }
        let tmp = path.with_extension("json.tmp");
        {
            let text = serde_json::to_string_pretty(doc)
                .map_err(|e| format!("serialize settings failed: {e}"))?;
            let mut f =
                std::fs::File::create(&tmp).map_err(|e| format!("write tmp failed: {e}"))?;
            use std::io::Write as _;
            f.write_all(text.as_bytes())
                .map_err(|e| format!("write tmp failed: {e}"))?;
        }
        std::fs::rename(&tmp, path).map_err(|e| format!("rename failed: {e}"))?;
        Ok(())
    }
}

// =============================================================================
// 端点（三件；G16 鉴权中间件自动覆盖）
// =============================================================================

/// `GET /api/workbench/settings` —— 合并后全量 + 每键生效层标注
pub async fn get_settings(State(state): State<AgentApiState>) -> Json<Value> {
    let (settings, sources) = state.workbench_settings().merged();
    Json(json!({ "settings": settings, "sources": sources }))
}

/// `PUT /api/workbench/settings` 请求体（`value: null` = 重置该键）
#[derive(Debug, Deserialize)]
pub struct PutSettingBody {
    /// 扁平键 ID（须在 schema 白名单内）
    pub key: String,
    /// 新值（null 表示重置=移除用户层覆盖）
    pub value: Value,
}

/// `PUT /api/workbench/settings` —— 写 user 层单键（校验非法 400；IO 失败 500；原子写）。
/// 返回写后合并值与生效层，前端可直取更新。
pub async fn put_settings(
    State(state): State<AgentApiState>,
    Json(body): Json<PutSettingBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let store = state.workbench_settings();
    let v = if body.value.is_null() {
        None
    } else {
        Some(&body.value)
    };
    validate_put(&body.key, v).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    store
        .write_user_layer_key(&body.key, v)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let (settings, sources) = store.merged();
    // 红线豁免覆盖开启动作留痕（审批三档制：exemptionOverride=true = 用户显式
    // 关闭红线强制人工审批面）——warning 级落日志；回落 false/重置不另记
    if body.key == "approval.exemptionOverride"
        && settings
            .get(&body.key)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    {
        warn!(
            target: "approval_audit",
            "红线豁免覆盖开启：approval.exemptionOverride=true，命中豁免清单的操作不再强制人工审批（判定留痕携带 exemption_overridden 标记）"
        );
    }
    let source = sources.get(&body.key).cloned().unwrap_or(json!("default"));
    Ok(Json(json!({
        "key": body.key,
        "value": settings.get(&body.key).cloned().unwrap_or(Value::Null),
        "source": source,
    })))
}

/// `GET /api/workbench/settings/schema` —— 注册表下发（UI 渲染/校验/JSON 补全单源）
pub async fn get_settings_schema() -> Json<Value> {
    Json(json!({ "entries": schema() }))
}

// =============================================================================
// 单测（合并序/校验/降级/白名单/原子写）
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立临时目录（测试间不共享）
    fn temp_store(tag: &str) -> (PathBuf, WorkbenchSettingsStore) {
        let dir = std::env::temp_dir().join(format!(
            "evo-settings-test-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let store = WorkbenchSettingsStore::new(&dir).unwrap();
        (dir, store)
    }

    fn write_file(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    fn user_file(dir: &Path) -> PathBuf {
        dir.join("data").join("workbench_settings.json")
    }

    fn ws_file(dir: &Path) -> PathBuf {
        dir.join(".evo").join("settings.json")
    }

    #[test]
    fn merged_defaults_when_no_files() {
        let (_d, store) = temp_store("defaults");
        let (settings, sources) = store.merged();
        assert_eq!(settings.len(), 58); // 计数锁:新增键须同步(scheme 单源纪律;B22 memory_recall/audit_query/eventLedger;B23 mcpServer.enabled;O-487② memory_tombstone;批K editFile;批K 段2 webFetch/webSearch;批K 段3 codeSearch;批K 段4 todo;批K 段5 httpRequest;批K 段7 askUser;workbench.uiFontSize;M4 workspaceUndo)
        assert_eq!(settings["editor.fontSize"], json!(14));
        assert_eq!(settings["editor.minimap"], json!(false)); // DC-1
        assert_eq!(settings["editor.wordWrap"], json!("off"));
        assert_eq!(settings["keybindings.overrides"], json!([]));
        // agentTools.* 开关键:出厂默认(fileCreate/editFile/grep/gitRead 开 / fileMove、fileDelete、gitWrite 关)
        assert_eq!(settings["agentTools.fileCreate"], json!(true));
        assert_eq!(settings["agentTools.editFile"], json!(true));
        assert_eq!(settings["agentTools.workspaceUndo"], json!(true)); // M4 写面撤销默认开
        assert_eq!(settings["agentTools.webFetch"], json!(true));
        assert_eq!(settings["agentTools.webSearch"], json!(true));
        assert_eq!(settings["agentTools.codeSearch"], json!(true));
        assert_eq!(settings["agentTools.fileMove"], json!(false));
        assert_eq!(settings["agentTools.fileDelete"], json!(false));
        // http_request(批K 段5):写动词默认关——显式启用才进 LLM 契约
        assert_eq!(settings["agentTools.httpRequest"], json!(false));
        assert_eq!(settings["agentTools.grep"], json!(true));
        assert_eq!(settings["agentTools.gitRead"], json!(true));
        assert_eq!(settings["agentTools.gitWrite"], json!(false));
        // 治理写权开关:单键默认开 = 45 工具全量,零行为变化
        assert_eq!(settings["agentTools.governanceWrite"], json!(true));
        // 动态源开关(D6 裁定方案甲):默认开 = 向后兼容,现状零行为变化
        assert_eq!(settings["agentTools.mcp"], json!(true));
        assert_eq!(settings["agentTools.serviceProxy"], json!(true));
        // 动态源裁决分级(D6 终态+D7-A 方案一):默认 sensitive = 意图必报
        assert_eq!(settings["agentTools.mcpAdjudication"], json!("sensitive"));
        assert_eq!(
            settings["agentTools.serviceProxyAdjudication"],
            json!("sensitive")
        );
        // 自主交接开关(交接设计 PR-H2):默认开 = 交接族在面,零行为变化
        assert_eq!(settings["agentTools.handover"], json!(true));
        // todo 任务清单族(批K 段4):默认开 = 进程内内存态零外溢面
        assert_eq!(settings["agentTools.todo"], json!(true));
        // search.* 全局搜索 4 键(B2 消费)
        assert_eq!(settings["search.maxResults"], json!(1000));
        assert_eq!(settings["search.smartCase"], json!(true));
        assert_eq!(settings["search.useIgnoreFiles"], json!(true));
        assert_eq!(
            settings["search.excludeGlobs"],
            json!([
                ".git/**",
                "target/**",
                "node_modules/**",
                ".evo-trash/**",
                "data/**"
            ])
        );
        // problems.* 诊断面 2 键(问题面板/编辑器校验开关)
        assert_eq!(settings["problems.json.validate"], json!(true));
        assert_eq!(settings["problems.markdown.lint"], json!(true));
        // longSession.compaction.* 3 键:默认关+70%+20 条(启用属 LLM
        // 精力开销,缺省走被动裁剪)
        assert_eq!(settings["longSession.compaction.enabled"], json!(false));
        assert_eq!(settings["longSession.compaction.thresholdPct"], json!(70));
        assert_eq!(
            settings["longSession.compaction.maxClearToolResults"],
            json!(20)
        );
        // mentions.* @上下文引用 2 键(B13 消费:文件降级大纲/终端尾部行数)
        assert_eq!(settings["mentions.maxFileLines"], json!(600));
        assert_eq!(settings["mentions.terminalLines"], json!(200));
        // index.enabled 默认关:后台索引为重资源行为,显式开启(重启生效)
        assert_eq!(settings["index.enabled"], json!(false));
        // index.semantic.enabled 默认关:首次启用触发约 24MB 模型下载(明示),
        // 依赖 index.enabled;缺省关 = 零外发零行为变化
        assert_eq!(settings["index.semantic.enabled"], json!(false));
        for (k, v) in &sources {
            assert_eq!(v, &json!("default"), "key {k} should be default");
        }
    }

    #[test]
    fn merge_order_default_user_workspace() {
        let (d, store) = temp_store("order");
        write_file(
            &user_file(&d),
            r#"{"editor.fontSize": 18, "editor.wordWrap": "on"}"#,
        );
        write_file(
            &ws_file(&d),
            r#"{"editor.fontSize": 20, "editor.tabSize": 2, "unknown.key": 1, "editor.minimap": "yes"}"#,
        );
        let (settings, sources) = store.merged();
        // workspace 覆盖 user 覆盖 default
        assert_eq!(settings["editor.fontSize"], json!(20));
        assert_eq!(sources["editor.fontSize"], json!("workspace"));
        // user 生效
        assert_eq!(settings["editor.wordWrap"], json!("on"));
        assert_eq!(sources["editor.wordWrap"], json!("user"));
        // workspace 独有键生效
        assert_eq!(settings["editor.tabSize"], json!(2));
        assert_eq!(sources["editor.tabSize"], json!("workspace"));
        // workspace 未知键忽略；非法值键跳过（回落 default）
        assert_eq!(settings["editor.minimap"], json!(false));
        assert_eq!(sources["editor.minimap"], json!("default"));
    }

    #[test]
    fn corrupt_layers_degrade_to_default() {
        let (d, store) = temp_store("corrupt");
        write_file(&user_file(&d), "{ not json !!!");
        write_file(&ws_file(&d), "[]"); // 非对象
        let (settings, sources) = store.merged();
        assert_eq!(settings["editor.fontSize"], json!(14));
        assert_eq!(sources["editor.fontSize"], json!("default"));
        // 损坏层不阻塞 PUT（原子写覆盖修复）
        store
            .write_user_layer_key("editor.fontSize", Some(&json!(21)))
            .unwrap();
        let (settings, sources) = store.merged();
        assert_eq!(settings["editor.fontSize"], json!(21));
        assert_eq!(sources["editor.fontSize"], json!("user"));
    }

    #[test]
    fn put_and_reset_roundtrip() {
        let (_d, store) = temp_store("roundtrip");
        store
            .write_user_layer_key("editor.fontSize", Some(&json!(24)))
            .unwrap();
        store
            .write_user_layer_key("editor.wordWrap", Some(&json!("on")))
            .unwrap();
        let (settings, sources) = store.merged();
        assert_eq!(settings["editor.fontSize"], json!(24));
        assert_eq!(sources["editor.fontSize"], json!("user"));
        // 重置 = 从 user 层移除，回落默认
        store.write_user_layer_key("editor.fontSize", None).unwrap();
        let (settings, sources) = store.merged();
        assert_eq!(settings["editor.fontSize"], json!(14));
        assert_eq!(sources["editor.fontSize"], json!("default"));
        // 其余键不受影响
        assert_eq!(settings["editor.wordWrap"], json!("on"));
        // 磁盘形态：扁平键对象
        let text = std::fs::read_to_string(user_file(&_d)).unwrap();
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["editor.wordWrap"], json!("on"));
        assert!(doc.get("editor.fontSize").is_none());
    }

    #[test]
    fn atomic_write_pretty_and_loadable() {
        let (_d, store) = temp_store("atomic");
        store
            .write_user_layer_key("editor.fontSize", Some(&json!(9)))
            .unwrap();
        // 直接重读（原子写后文件可解析）
        let text = std::fs::read_to_string(store.user_path()).unwrap();
        assert!(text.contains("\"editor.fontSize\""));
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["editor.fontSize"], json!(9));
        // 无 .tmp 残留
        let tmp = store.user_path().with_extension("json.tmp");
        assert!(!tmp.exists());
    }

    #[test]
    fn validate_rejects_unknown_and_invalid() {
        // 键白名单
        assert!(validate_put("no.such.key", Some(&json!(1))).is_err());
        assert!(validate_put("no.such.key", None).is_err());
        // number 范围
        assert!(validate_put("editor.fontSize", Some(&json!(8))).is_err());
        assert!(validate_put("editor.fontSize", Some(&json!(29))).is_err());
        assert!(validate_put("editor.fontSize", Some(&json!("14"))).is_err());
        assert!(validate_put("editor.fontSize", Some(&json!(9))).is_ok());
        assert!(validate_put("editor.fontSize", Some(&json!(28))).is_ok());
        assert!(validate_put("editor.tabSize", Some(&json!(0))).is_err());
        assert!(validate_put("editor.tabSize", Some(&json!(1))).is_ok());
        // enum
        assert!(validate_put("editor.wordWrap", Some(&json!("auto"))).is_err());
        assert!(validate_put("editor.wordWrap", Some(&json!("on"))).is_ok());
        assert!(validate_put("workbench.theme", Some(&json!("light"))).is_err());
        assert!(validate_put("workbench.theme", Some(&json!("auto"))).is_ok());
        assert!(validate_put("workbench.theme", Some(&json!("evorule-light"))).is_ok());
        // boolean
        assert!(validate_put("editor.minimap", Some(&json!("yes"))).is_err());
        assert!(validate_put("editor.minimap", Some(&json!(true))).is_ok());
        // array 元素形态
        assert!(validate_put("keybindings.overrides", Some(&json!({}))).is_err());
        assert!(validate_put("keybindings.overrides", Some(&json!([{"key": "ctrl+k"}]))).is_err());
        assert!(validate_put(
            "keybindings.overrides",
            Some(&json!([{"key": "ctrl+k", "command": "workbench.action.file.save"}]))
        )
        .is_ok());
        assert!(validate_put(
            "keybindings.overrides",
            Some(&json!([{"key": "ctrl+k", "command": "x.y", "when": "tabsOpen"}]))
        )
        .is_ok());
        // 重置只查键白名单
        assert!(validate_put("editor.fontSize", None).is_ok());
    }

    #[test]
    fn workspace_application_scope_guard() {
        let mut e = find_entry("editor.fontSize").unwrap();
        // 同值：window scope 放行
        assert!(accept_workspace_value(&e, &json!(16)));
        // application scope 拒工作区覆盖（机制预留）
        e.scope = "application";
        assert!(!accept_workspace_value(&e, &json!(16)));
        // 非法值拒绝
        e.scope = "window";
        assert!(!accept_workspace_value(&e, &json!("big")));
    }

    #[test]
    fn path_whitelist_rejects_traversal() {
        let base = Path::new("/w");
        // 父目录穿越
        assert!(ensure_fixed_relative(base, "../evil.json").is_err());
        assert!(ensure_fixed_relative(base, "data/../../evil.json").is_err());
        // 根相对路径(join 会替换 base,同属越界面;Windows/Unix 双态都拦)
        assert!(ensure_fixed_relative(base, "/evil.json").is_err());
        // 盘符绝对路径(Windows 形态;Unix 上为普通相对名,不构成越界)
        if cfg!(windows) {
            assert!(ensure_fixed_relative(base, "C:/evil.json").is_err());
        }
        // 合规相对路径
        assert!(ensure_fixed_relative(base, "data/settings.json").is_ok());
        assert!(ensure_fixed_relative(base, ".evo/settings.json").is_ok());
    }
}
