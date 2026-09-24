// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `evo-agent` —— EvoRule AI agent runner CLI
//!
//! 5 原则落地(详见 `EvoRule Design Principles` 文档):
//! - **透明**:每条子命令输出明确,`config` 展示完整合并后配置
//! - **可选**:`run` 有 `--agent` / `--workdir` 等选项;`tools show` 显示 active/candidate/blocked
//! - **可控**:candidate 工具需要 `--auto-approve-candidates` 才放行(0.1.0: 拒绝 + 提示如何启用)
//! - **可回放**:每个 run 输出一份 fact log(后续 0.2.0 实现完整 replay 模式)
//! - **可审计**:`run` 结果是结构化 JSON,适合归档
//!
//! ## 子命令
//!
//! ```text
//! evo-agent run <goal>           # 跑 agent
//! evo-agent list                  # 列出可用 agent 类型
//! evo-agent tools list            # 列出 6 个工具(active/candidate/blocked)
//! evo-agent tools show <name>     # 显示工具详情
//! evo-agent validate <agent>      # 校验 agent.json
//! evo-agent config                # 显示合并后的配置
//! ```
//!
//! ## 示例
//!
//! ```bash
//! # 1) 准备 agent.json
//! mkdir -p agents
//! cat > agents/researcher.json <<EOF
//! {
//!   "agent_type": "researcher",
//!   "version": "0.1.0",
//!   "description": "...",
//!   "system_prompt": "...",
//!   "model": "MiniMax-M2.5",
//!   "temperature": 0.3,
//!   "max_steps": 20,
//!   "step_timeout_secs": 60,
//!   "tools": ["file_read", "search_files"]
//! }
//! EOF
//!
//! # 2) 跑(用 evorule + LLM)
//! evo-agent run "summarize the README"
//!
//! # 3) 列出可用 agent
//! evo-agent list
//!
//! # 4) 看 6 工具的 3 层安全模型
//! evo-agent tools list
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueHint};

use evo_agent::agent::definition::AgentDefinitionManager;
use evo_agent::agent::delegate::DelegateContext;
use evo_agent::agent::replan::{
    lookup_agent_type, should_replan, BudgetCounters, BudgetThresholds, ReplanReason, ReplanState,
};
use evo_agent::agent::runner::AgentRunner;
use evo_agent::agent::workflow::WorkflowEngine;
use evo_agent::api::agent_api::AgentApiState;
use evo_agent::api::evorule_client::EvoruleApiClient;
use evo_agent::api::workspace_client::WorkspaceApiClient;
use evo_agent::builtin_tools::{default_tool_specs, http_get, shell_exec, ToolSpec};
use evo_agent::io_handlers::LlmHandler;
use evo_agent::Workflow;

// =============================================================================
// CLI 定义
// =============================================================================

#[derive(Parser, Debug)]
#[command(
    name = "evo-agent",
    version,
    about = "EvoRule AI agent runner — JSON rules, JSON facts, JSON everything",
    long_about = "Run AI agents that read user-written JSON rules, produce JSON fact logs, \
                  and stay within the EvoRule 5 design principles (transparent / optional / \
                  controllable / replayable / auditable)."
)]
struct Cli {
    /// 工作目录(默认当前目录)
    #[arg(long, global = true, value_hint = ValueHint::DirPath, default_value = ".")]
    workdir: PathBuf,

    /// 详细输出(debug logging 到 stderr)
    #[arg(long, global = true, short = 'v')]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 跑 agent(给一个 goal)
    Run {
        /// 任务描述(给 agent 的指令)
        goal: String,

        /// agent 类型(对应 `agents/<name>.json`)。不指定则用 config.agents.default
        #[arg(long, short = 'a')]
        agent: Option<String>,

        /// 自动批准 candidate 工具(0.1.0 暂不交互,默认拒绝;带此 flag 则允许)
        #[arg(long)]
        auto_approve_candidates: bool,

        /// G4:流式输出(token-by-token,实时显示 LLM 输出)
        #[arg(long)]
        stream: bool,
    },

    /// 列出所有可用的 agent 类型
    List {
        /// 覆盖 agents 目录(否则用 config.agents.dir)
        #[arg(long, short = 'd', value_hint = ValueHint::DirPath)]
        dir: Option<PathBuf>,
    },

    /// 工具管理(看 3 层安全模型)
    Tools {
        #[command(subcommand)]
        action: ToolsAction,
    },

    /// 校验 agent.json 是否合法
    Validate {
        /// agent 类型名(对应 `agents/<name>.json`)
        agent: String,
    },

    /// 显示合并后的配置(default + user + project + env)
    Config,

    /// G5:启动 HTTP server(对外提供 agent API + SSE 流式 + 取消端点)
    Serve {
        /// 监听地址(默认 127.0.0.1)
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        /// 监听端口(默认 8081)
        #[arg(long, default_value_t = 8081)]
        port: u16,

        /// G7:鉴权 token(覆盖配置文件,可多次指定)
        #[arg(long)]
        auth_token: Vec<String>,

        /// G7:禁用鉴权(覆盖配置文件的 enabled=true)
        #[arg(long)]
        no_auth: bool,
    },

    /// 进化巡视任务模式(一次性):信号探查 → 有信号则 agent 起草+提名 →
    /// 结构化 JSON 巡视报告;无信号零动作静默退出(报告仍输出一行供调度器消费)。
    /// 触发器在 agent 侧/外部调度(cron/运维脚本),server 零自治循环。
    Patrol {
        /// 要巡视的 evorule 会话 id(违规信号按会话归因,与信号端点同口径)
        #[arg(long)]
        session: u64,

        /// 起草与提名所用的治理工作空间 id
        #[arg(long)]
        workspace: String,

        /// agent 类型(默认 rule-copilot:提名工具在协作体档案白名单内)
        #[arg(long, short = 'a')]
        agent: Option<String>,

        /// 巡视报告 JSON 追加写入路径(缺省仅打印到 stdout)
        #[arg(long, value_hint = ValueHint::FilePath)]
        out: Option<PathBuf>,
    },

    /// G9:执行多 agent 工作流(DAG 编排,并行层 + 串行依赖)
    Workflow {
        /// 工作流 id(对应 `rules/workflows/<id>.json`)
        workflow_id: String,

        /// 覆盖 workflows 目录(否则用 `{workdir}/rules/workflows`)
        #[arg(long, short = 'd', value_hint = ValueHint::DirPath)]
        dir: Option<PathBuf>,

        /// 最大委托深度(默认 3,防无限递归)
        #[arg(long, default_value_t = 3)]
        max_depth: usize,

        /// 并行子 agent 并发上限(默认 5,0 = 不限流)
        #[arg(long, default_value_t = 5)]
        max_concurrent: usize,
    },

    /// G15:REPL 交互模式(对话式,复用同一 evorule session)
    ///
    /// 面向"则灵"消费者场景:逐条输入,逐条响应,保持上下文连续。
    /// 首次输入创建新 session,后续输入复用同一 session。
    ///
    /// 特殊命令:
    /// - `/exit` 退出 REPL
    /// - `/session` 显示当前 session ID
    /// - `/rewind <version>` 回滚到指定版本
    ///
    /// 跨进程恢复(Q14:B):session_id 持久化到 `{workdir}/.evo-agent/session`,
    /// `evo-agent repl --session <id>` 可恢复已有 session。
    Repl {
        /// agent 类型(对应 `agents/<name>.json`)。不指定则用 config.agents.default
        #[arg(long, short = 'a')]
        agent: Option<String>,

        /// 自动批准 candidate 工具(同 `run --auto-approve-candidates`)
        #[arg(long)]
        auto_approve_candidates: bool,

        /// G15:恢复已有 session(Q14:B 跨进程恢复)
        ///
        /// 不指定时:尝试从 `{workdir}/.evo-agent/session` 加载;
        /// 加载失败则首次输入创建新 session。
        #[arg(long)]
        session: Option<String>,
    },

    /// G14:回放指定 session 的记忆事件链("则灵"生活回放)
    ///
    /// 从 evorule 拉取结构化事件,按因果链排列,输出确定性时间线。
    /// 可选 LLM 包装为自然语言叙述(temperature=0,事实不变)。
    ///
    /// 用法:
    ///   evo-agent replay --session 123                    # 回放全部事件(按时间)
    ///   evo-agent replay --session 123 --event E005       # 从 E005 沿因果链回溯
    ///   evo-agent replay --session 123 --entity pet_doudou # 回放某实体的所有事件
    ///   evo-agent replay --session 123 --narrate           # LLM 自然语言叙述
    Replay {
        /// evorule session ID(必填)
        #[arg(long)]
        session: String,

        /// 从指定事件 ID 出发沿因果链回溯/前进
        #[arg(long)]
        event: Option<String>,

        /// 回放某实体的所有事件(如 "pet_doudou")
        #[arg(long)]
        entity: Option<String>,

        /// 回放方向:backward(默认,沿 cause 链回溯)或 forward(沿 effects 链前进)
        #[arg(long, default_value = "backward")]
        direction: String,

        /// LLM 自然语言叙述(temperature=0,事实不变;不加则输出结构化时间线)
        #[arg(long)]
        narrate: bool,

        /// 改进3：结构化时间线开启哈希链验证(默认关闭,逐事件 evidence_for_event 输出 ✓/✗)
        #[arg(long)]
        verify: bool,

        /// agent 类型(对应 `agents/<name>.json`,narrate 时用于 LLM 配置)
        #[arg(long, short = 'a')]
        agent: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ToolsAction {
    /// 列出所有 6 个工具 + 3 层分类
    List,
    /// 显示单个工具的详情(active / candidate / blocked)
    Show {
        /// 工具名
        name: String,
    },
}

// =============================================================================
// main
// =============================================================================

fn main() -> ExitCode {
    let cli = Cli::parse();

    // 初始化 logging(0.1.0 简化:用 env RUST_LOG,默认 info)
    init_logging(cli.verbose);

    match cli.command {
        Command::Run {
            goal,
            agent,
            auto_approve_candidates,
            stream,
        } => cmd_run(
            &cli.workdir,
            &goal,
            agent.as_deref(),
            auto_approve_candidates,
            stream,
        ),
        Command::List { dir } => cmd_list(&cli.workdir, dir.as_deref()),
        Command::Tools { action } => cmd_tools(&cli.workdir, action),
        Command::Validate { agent } => cmd_validate(&cli.workdir, &agent),
        Command::Config => cmd_config(&cli.workdir),
        Command::Serve {
            host,
            port,
            auth_token,
            no_auth,
        } => cmd_serve(&cli.workdir, &host, port, &auth_token, no_auth),
        Command::Patrol {
            session,
            workspace,
            agent,
            out,
        } => cmd_patrol(
            &cli.workdir,
            session,
            &workspace,
            agent.as_deref(),
            out.as_deref(),
        ),
        Command::Workflow {
            workflow_id,
            dir,
            max_depth,
            max_concurrent,
        } => cmd_workflow(
            &cli.workdir,
            &workflow_id,
            dir.as_deref(),
            max_depth,
            max_concurrent,
        ),
        Command::Repl {
            agent,
            auto_approve_candidates,
            session,
        } => cmd_repl(
            &cli.workdir,
            agent.as_deref(),
            auto_approve_candidates,
            session.as_deref(),
        ),
        Command::Replay {
            session,
            event,
            entity,
            direction,
            narrate,
            verify,
            agent,
        } => cmd_replay(
            &cli.workdir,
            &session,
            event.as_deref(),
            entity.as_deref(),
            &direction,
            narrate,
            verify,
            agent.as_deref(),
        ),
    }
}

fn init_logging(verbose: bool) {
    use tracing_subscriber::{fmt, EnvFilter};
    let level = if verbose { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

// =============================================================================
// run —— 跑 agent
// =============================================================================

fn cmd_run(
    workdir: &Path,
    goal: &str,
    agent: Option<&str>,
    auto_approve_candidates: bool,
    stream: bool,
) -> ExitCode {
    // 1. 加载配置
    let config = match evo_agent::config::Config::load(workdir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {}", e);
            return ExitCode::from(1);
        }
    };

    // 2. 决定 agent 类型
    let agent_name = agent.unwrap_or(&config.agents.default);
    // 相对 agents.dir 相对 workdir 解析(与 config 加载基准一致),不基于进程 cwd
    let agents_dir = if config.agents.dir.is_absolute() {
        config.agents.dir.clone()
    } else {
        workdir.join(&config.agents.dir)
    };
    let mgr = AgentDefinitionManager::new(agents_dir.clone());
    let def = match mgr.load(agent_name) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "failed to load agent '{}' from {}: {}",
                agent_name,
                agents_dir.display(),
                e
            );
            return ExitCode::from(1);
        }
    };

    // 3. evorule + workspace 客户端(规则管理工具依赖 workspace API)
    let client =
        EvoruleApiClient::with_auth_token(&config.evorule.base_url, Some(&config.evorule.api_key));
    let ws_client = WorkspaceApiClient::new(&config.evorule.base_url);

    // 4. 工具 handler:内置安全工具(6) + 规则管理工具(20) 的 union
    //    修复:rule-copilot 等规则角色在 run 路径也能使用 ws_*/rule_*/audit_* 工具
    let mut tool_handler =
        evo_agent::api::serve_tools::build_union_toolkit(workdir, &ws_client, &client);

    // 5. 桥接
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to build tokio runtime: {}", e);
            return ExitCode::from(1);
        }
    };

    let runner_result = runtime.block_on(async {
        // G3:把 config.llm 真正接到 LlmHandler,复活 max_retries 死代码
        let llm_handler = LlmHandler::from_config(&config.llm);

        // G12:连接配置的 MCP server,把它们的工具注册到 tool_handler
        // (async:每个 server spawn 子进程 + initialize 握手 + tools/list 发现工具)
        if !config.mcp.servers.is_empty() {
            let connected =
                evo_agent::mcp::register_mcp_tools(&mut tool_handler, &config.mcp).await;
            eprintln!(
                "[mcp] {}/{} server(s) connected",
                connected,
                config.mcp.servers.len()
            );
        }

        // 服务消费桥:按白名单把 server 插件服务注册为代理工具
        // (发现失败降级继续并明示影响,不阻塞 agent 主流程)
        if !config.evorule.service_tools.is_empty() {
            match evo_agent::service_tools::register_service_tools(
                &mut tool_handler,
                &client,
                &config.evorule.service_tools,
            )
            .await
            {
                Ok(n) => eprintln!("[service-tools] {n} service tool(s) registered"),
                Err(e) => {
                    eprintln!("[service-tools] 服务工具注册失败,本次运行无服务工具: {e}")
                }
            }
        }

        AgentRunner::from_definition(def, client, tool_handler, Some(llm_handler)).await
    });
    let runner = match runner_result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("bridge error: {}", e);
            return ExitCode::from(1);
        }
    };

    // G8:注入 CliApproval — candidate 工具返回 needs_approval 时走交互式审批
    // --auto-approve-candidates 时跳过交互直接批准(自动化场景)
    // 不带 flag 时走 stdin 交互(y/N),无 callback 则默认拒绝(安全优先)
    let mut runner = runner.with_approval_callback(std::sync::Arc::new(
        evo_agent::agent::approval::CliApproval {
            auto_approve: auto_approve_candidates,
        },
    ));

    // 6. 跑
    eprintln!(
        "running agent '{}' with goal: {}{}",
        runner.agent_type(),
        goal,
        if stream { " [stream]" } else { "" }
    );

    // G6:Ctrl+C → 触发 cancel_token,runner 在下一个 event/chunk 边界优雅退出
    let cancel_token = runner.cancel_token().clone();
    let cancel_handle = runtime.spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n[Ctrl+C received, cancelling agent...]");
            cancel_token.cancel();
        }
    });

    let exit = if stream {
        cmd_run_streaming(&runtime, runner, goal)
    } else {
        cmd_run_blocking(&runtime, &mut runner, goal)
    };
    // 任务已结束,停止监听 Ctrl+C
    cancel_handle.abort();
    exit
}

/// 解析 agent 产出中的围栏 JSON 代码块(取最后一个 ```json 或 ``` 围栏块)
fn extract_last_fenced_json(text: &str) -> Option<String> {
    let mut result = None;
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        // 跳过语言标记(如 json)
        let body_start_offset = after.find('\n').map(|i| i + 1).unwrap_or(0);
        let body = &after[body_start_offset..];
        let Some(end) = body.find("```") else {
            break;
        };
        result = Some(body[..end].trim().to_string());
        rest = &body[end + 3..];
    }
    result
}

/// 约束层草稿结构校验(提名前置,意图静默丢失防御)
///
/// 与 server schema 门禁同口径的条目级键白名单({type, params},未知键 fail-fast
/// 并列明键名)+ enforce 条目最小结构校验(params 必含 domain/reason 且 reason
/// 非空)。背景:转写产物曾把匹配条件写成条目级 condition 字段,引擎静默忽略
/// 导致约束对所有指令无条件触发——起草意图在提交期即校验,杜绝无效提名进
/// 人审队列;同时进化约束必须自带拦截原语(enforce),留痕型产物不构成生效约束。
fn validate_constraint_draft(draft: &str) -> Result<(), String> {
    let parsed: serde_json::Value =
        serde_json::from_str(draft).map_err(|e| format!("约束草稿不是合法 JSON: {e}"))?;
    let transforms = parsed
        .get("transform")
        .and_then(|t| t.as_array())
        .ok_or("约束草稿缺少 transform 数组")?;
    if transforms.is_empty() {
        return Err("约束草稿 transform 为空".to_string());
    }
    for (i, entry) in transforms.iter().enumerate() {
        let obj = entry
            .as_object()
            .ok_or(format!("transform[{i}] 不是对象"))?;
        let unknown: Vec<&str> = obj
            .keys()
            .map(String::as_str)
            .filter(|k| *k != "type" && *k != "params")
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "transform[{i}] 携带条目级未知键 {}(条目级只允许 type/params;拦截条件必须写在 params.domain 内)",
                unknown.join(",")
            ));
        }
        let ty = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if ty != "enforce" {
            return Err(format!(
                "transform[{i}] type={ty:?} 不是 enforce——进化约束必须自带拦截原语(留痕型产物不构成生效约束)"
            ));
        }
        let params = obj
            .get("params")
            .and_then(|p| p.as_object())
            .ok_or(format!("transform[{i}] 缺少 params 对象"))?;
        for req in ["domain", "reason"] {
            if !params.contains_key(req) {
                return Err(format!("transform[{i}] enforce 缺少 params.{req}"));
            }
        }
        if params
            .get("reason")
            .and_then(|r| r.as_str())
            .is_none_or(|s| s.trim().is_empty())
        {
            return Err(format!(
                "transform[{i}] enforce reason 须为非空字符串(无 reason 的拦截不可审计)"
            ));
        }
    }
    Ok(())
}

/// 产物值清洗:去两侧空白/markdown 修饰符/引号/尾随分隔标点
/// (真实 LLM 会把 prompt 列表里的分号一并照抄进值,如「VERSION_ID=<ulid>;」)。
fn clean_extracted(v: &str) -> String {
    v.trim()
        .trim_matches('`')
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches(|c: char| ";,，。、.：:；".contains(c))
        .trim()
        .to_string()
}

/// 字符边界安全的日志截断(参数/结果常含中文,按字节切会 panic)
fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// 解析 `KEY=<值>` 形式的行内产物(E2E 演练同款约定,容忍 markdown 修饰符)
fn extract_keyed_value(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let s = line
            .trim()
            .trim_start_matches("- ")
            .trim_start_matches("* ");
        let s = s
            .trim()
            .trim_matches('`')
            .trim_start_matches('*')
            .trim_end_matches('*')
            .trim();
        // 容忍 KEY=值 / KEY:值 / KEY：值 三种行式(真实 LLM 输出格式存在漂移)
        for sep in ['=', ':', '：'] {
            if let Some(v) = s.strip_prefix(&format!("{key}{sep}")) {
                let cleaned = clean_extracted(v);
                if !cleaned.is_empty() {
                    return Some(cleaned);
                }
            }
        }
        // 容忍编号前缀行式(真实 LLM 常按 prompt 的 a)/b)/c) 要求以
        // 「b) KEY=…」回填):key 可出现在行中,但其前缀须为纯装饰(序号/
        // 括号/标点/空白,且不得以字母数字结尾——防 `valueX=1` 单词粘连
        // 误命中),key 后须紧跟分隔符。
        if let Some(pos) = s.find(key) {
            let (before, after) = (&s[..pos], &s[pos + key.len()..]);
            let decorated = before.chars().all(|c| {
                c.is_whitespace() || c.is_ascii_alphanumeric() || ")]}.、·*-—：（(:\"".contains(c)
            }) && before
                .trim_end()
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric());
            if decorated {
                for sep in ['=', ':', '：'] {
                    if let Some(v) = after.strip_prefix(sep) {
                        let cleaned = clean_extracted(v);
                        if !cleaned.is_empty() {
                            return Some(cleaned);
                        }
                    }
                }
            }
        }
    }
    None
}

/// patrol 单轮 runner 组装(与 serve 同链:MCP + 服务工具 + 档案定义;
/// run_streaming 消费 runner,两轮制每轮重建;巡视属无人值守任务,candidate 自动放行)。
///
/// `allowed_tools` 收窄本轮工具面(轮A 4 个起草工具/轮B 1 个提名工具):
/// 宽工具面下真实 LLM 会绕路探索(file_read/ws_list 等),实测导致步数超限或
/// 上下文被无关内容污染;窄面让 LLM 只见任务必需工具,行为收敛。
async fn patrol_build_runner(
    config: &evo_agent::config::Config,
    workdir: &Path,
    mut def: evo_agent::agent::definition::AgentDefinition,
    client: &EvoruleApiClient,
    allowed_tools: &[&str],
) -> Result<AgentRunner, String> {
    let ws_client = WorkspaceApiClient::new(&config.evorule.base_url);
    let union = evo_agent::api::serve_tools::build_union_toolkit(workdir, &ws_client, client);
    let whitelist: Vec<String> = allowed_tools.iter().map(|s| s.to_string()).collect();
    let mut tool_handler = evo_agent::api::serve_tools::build_filtered_toolkit(&union, &whitelist);
    // def.tools 与本轮工具面同步收窄:from_definition 会 fail-fast 校验 def.tools
    // 每个名字都已在 tool_handler 注册,只窄 handler 不窄 def 会直接启动失败。
    def.tools.retain(|t| whitelist.contains(t));
    if !config.mcp.servers.is_empty() {
        let connected = evo_agent::mcp::register_mcp_tools(&mut tool_handler, &config.mcp).await;
        eprintln!(
            "[mcp] {connected}/{} server(s) connected",
            config.mcp.servers.len()
        );
    }
    if !config.evorule.service_tools.is_empty() {
        match evo_agent::service_tools::register_service_tools(
            &mut tool_handler,
            client,
            &config.evorule.service_tools,
        )
        .await
        {
            Ok(n) => eprintln!("[service-tools] {n} service tool(s) registered"),
            Err(e) => eprintln!("[service-tools] 服务工具注册失败,本次巡视无服务工具: {e}"),
        }
    }
    let llm_handler = LlmHandler::from_config(&config.llm);
    let runner = AgentRunner::from_definition(def, client.clone(), tool_handler, Some(llm_handler))
        .await
        .map_err(|e| format!("bridge error: {e}"))?;
    Ok(runner.with_approval_callback(std::sync::Arc::new(
        evo_agent::agent::approval::CliApproval { auto_approve: true },
    )))
}

/// patrol 单轮流式执行:消费事件流至 Done。
///
/// 必须走流式路径:server 宪法 v0.5.0 起只做单发桥接(io_response 提交后即
/// Stable),多轮工具回喂循环在应用层——仅 `run_streaming` 的本地 ReAct 循环
/// 会把 tool_calls 的工具结果回喂 LLM 续轮;非流式 `run()` 单轮即止,LLM 若在
/// 首响应携带 tool_calls + 前言 content,巡视将拿到前言当作最终产出。
///
/// `label`(轮A/轮B)用于 stderr 轨迹留痕:工具调用与参数摘要逐条打印,
/// 巡视无人值守,轨迹是行为诊断(步数超限/工具误用)的唯一观测口。
async fn patrol_consume(
    runner: AgentRunner,
    prompt: String,
    label: &str,
) -> Result<evo_agent::agent::runner::AgentResult, String> {
    use futures_util::StreamExt;
    let mut stream = runner.run_streaming(prompt);
    let mut final_result: Option<evo_agent::agent::runner::AgentResult> = None;
    let mut step = 0usize;
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(evo_agent::agent::runner::AgentEvent::Step { step: n }) => {
                step = n;
                eprintln!("[patrol] {label} step {n}");
            }
            Ok(evo_agent::agent::runner::AgentEvent::ToolCall { name, args }) => {
                let args = truncate_utf8(&args.to_string(), 200);
                eprintln!("[patrol] {label} step {step} tool_call {name} args={args}");
            }
            Ok(evo_agent::agent::runner::AgentEvent::ToolResult { name, result }) => {
                let r = truncate_utf8(&result.to_string(), 150);
                eprintln!("[patrol] {label} step {step} tool_result {name} = {r}");
            }
            Ok(evo_agent::agent::runner::AgentEvent::LlmDone { finish_reason, .. }) => {
                eprintln!("[patrol] {label} step {step} llm_done finish={finish_reason:?}");
            }
            Ok(evo_agent::agent::runner::AgentEvent::Error(e)) => {
                eprintln!("[patrol] {label} 事件流错误: {e}");
                return Err(e.to_string());
            }
            Ok(evo_agent::agent::runner::AgentEvent::Done(r)) => {
                final_result = Some(r);
                break;
            }
            Ok(_) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    final_result.ok_or_else(|| "事件流在 Done 之前结束".to_string())
}

/// 进化巡视任务模式:一次性「信号 → 起草 → 证据 → 提名 → 报告」。
///
/// 编排复用真实 LLM 全链演练已验证的两轮制(轮A agent 起草三步,操作者组装
/// 闸门一沙盒证据,轮B agent 携证据提名);触发器在本进程,server 零自治循环。
/// 无信号时零动作静默退出(exit 0,报告 status=no_signal)。
fn cmd_patrol(
    workdir: &Path,
    session: u64,
    workspace: &str,
    agent: Option<&str>,
    out: Option<&Path>,
) -> ExitCode {
    // 巡视报告(全程填充;任何分支都以一行 JSON 收尾供调度器消费)
    let mut report = serde_json::json!({
        "task": "evolution_patrol",
        "session_id": session,
        "workspace_id": workspace,
        "status": "error",
        "actions": [],
    });
    let write_report = |report: &serde_json::Value, out: Option<&Path>| {
        let line = serde_json::to_string(report).unwrap_or_default();
        println!("{line}");
        if let Some(path) = out {
            // 追加写入:调度器可按时间序列归档每次巡视
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(f, "{line}");
            }
        }
    };

    // 1. 加载配置(巡视需要 LLM,用严格模式)
    let config = match evo_agent::config::Config::load(workdir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            report["error"] = serde_json::json!(format!("config error: {e}"));
            write_report(&report, out);
            return ExitCode::from(1);
        }
    };

    // 2. 加载 agent 档案(默认 rule-copilot:rule_promote 在协作体档案白名单)
    let agent_name = agent.unwrap_or("rule-copilot");
    let agents_dir = if config.agents.dir.is_absolute() {
        config.agents.dir.clone()
    } else {
        workdir.join(&config.agents.dir)
    };
    let mgr = AgentDefinitionManager::new(agents_dir.clone());
    let def = match mgr.load(agent_name) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "failed to load agent '{}' from {}: {}",
                agent_name,
                agents_dir.display(),
                e
            );
            report["error"] = serde_json::json!(format!("agent load failed: {e}"));
            write_report(&report, out);
            return ExitCode::from(1);
        }
    };

    // 3. 客户端(工具面按轮组装,见 patrol_build_runner:run_streaming 消费 runner)
    let client =
        EvoruleApiClient::with_auth_token(&config.evorule.base_url, Some(&config.evorule.api_key));
    let ws_client = WorkspaceApiClient::new(&config.evorule.base_url);

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to build tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    // 4. 信号探查(不调 LLM):无信号 → 零动作静默退出
    let signals_resp = runtime.block_on(client.get_evolution_signals(session, None));
    let signals_resp = match signals_resp {
        Ok(v) => v,
        Err(e) => {
            eprintln!("failed to fetch evolution signals for session {session}: {e}");
            report["error"] = serde_json::json!(format!("signals fetch failed: {e}"));
            write_report(&report, out);
            return ExitCode::from(1);
        }
    };
    let has_signals = signals_resp
        .get("signals")
        .and_then(|s| s.as_array())
        .is_some_and(|a| !a.is_empty());
    report["signals"] = signals_resp.clone();
    if !has_signals {
        // 无信号零动作:status=no_signal,exit 0(静默 = 不起草不提名)
        report["status"] = serde_json::json!("no_signal");
        write_report(&report, out);
        return ExitCode::SUCCESS;
    }

    // 5. 轮A:agent 拉信号 + 起草源规则三步 + 产出约束草稿/版本 id/测试用例
    let prompt_a = format!(
        "你正在执行一次自动化进化巡视,请严格按以下步骤执行,不要向用户提问或请求确认:\n\
         你只能使用以下 4 个工具:evolution_signals、rule_create、rule_submit、rule_versions;\n\
         其余工具一律不可用,禁止尝试调用它们。\n\
         1) 调用 evolution_signals 工具(session_id 参数传 {session})查看当前违规信号明细。\n\
         2) 针对排名第一的违规信号,设计一条源规则(普通规则,用于跟踪该违规模式涉及的\n\
            行为),并设计一个约束层规则集草稿 JSON。约束草稿要求:$schema 用 \
            https://evorule.org/schemas/rule_set/v1.0.json,kind 为 rule_set,\n\
            metadata.tier 为 \"constraint\" 且 metadata.title 必须为非空字符串(可用\n\
            「<违规模式>拦截约束」句式)。**transform 必须使用 enforce 元指令**(而非 set):\n\
            enforce 的 params 必含 domain(匹配即强制中断剩余 transform 并拒绝执行违规指令,\n\
            与 branch.domain 同构的 7 域类型)和 reason(非空字符串,命中时随系统独占 Violation\n\
            事实审计回显)。**严禁使用 set 留痕型——进化约束必须自带拦截原语,留痕型不构成\n\
            生效约束(违规指令仍被旧约束拦截、新约束不可达)。** 草稿必须是且仅是如下完整\n\
            JSON 对象:transform 数组每个元素直接就是 {{\"type\":...,\"params\":...}} 对象,\n\
            严禁条目级出现 name/condition/meta 等任何其他键,严禁把 enforce 包进 meta 等\n\
            包裹层数组。已验证可用的最小完整草稿示例:\n\
            {{\"$schema\":\"https://evorule.org/schemas/rule_set/v1.0.json\",\"kind\":\"rule_set\",\"metadata\":{{\"tier\":\"constraint\",\"title\":\"robot_move 拦截约束\"}},\"transform\":[{{\"type\":\"enforce\",\"params\":{{\"domain\":{{\"type\":\"instruction\",\"instruction_type\":\"robot_move\"}},\"reason\":\"robot_move 违规拦截\"}}}}]}}\n\
            其中 domain.type 取 \"instruction\" 时匹配指令类型(如 robot_move),也可用 eq/lt/exists\n\
            等域类型表达更精确的匹配条件(条件必须写在 params.domain 内)。输出前自查:\n\
            transform 每个条目是否只含 type 与 params 两个键——条目级未知键会被 schema gate\n\
            拒收,草稿整体作废。\n\
            源规则 content 必须是且仅是如下形态的 JSON 对象——顶层只含 type 与 params\n\
            两个键,type 取 \"set\",params 必含 attr(字符串)/operation(只能是 set/add/sub)/\n\
            value(字符串或数值)三键,可附 condition 对象表达触发条件。已验证可用的最小示例:\n\
            {{\"type\":\"set\",\"params\":{{\"attr\":\"safety_audit\",\"operation\":\"set\",\"value\":\"violation_pattern_recorded\",\"condition\":{{\"instruction\":\"robot_move\"}}}}}}\n\
            若 rule_create 返回校验错误,按错误信息修正参数后立即重试。\n\
         3) 调用 rule_create 在工作空间 {workspace} 创建该源规则(name 自拟但需含\n\
            \"巡视\" 字样,content 为第 2 步源规则的 JSON 字符串形式,created_by 用 \
            \"evo-agent-patrol\")。\n\
         4) 调用 rule_submit 把该规则提交为候选(workspace_id 为 {workspace},rule_id 用\n\
            上一步返回的规则 id)。\n\
         5) 调用 rule_versions 查询该规则版本列表,取最新版本的版本 id。\n\
         6) 最后在回复中输出以下三样内容(必须齐全,顺序不限):\n\
         a) 一个 ```json 围栏代码块,内容为第 2 步的约束层草稿 JSON;\n\
         b) 一行 VERSION_ID=<第 5 步取到的版本 id>(行尾不要附加分号等标点);\n\
         c) 一行 TEST_CASE=<单个 JSON 对象>,该对象能命中你源规则 transform 的 domain 条件。"
    );
    eprintln!("[patrol] 轮A:信号感知与起草(start)");
    let turn_a = runtime.block_on(async {
        let runner = patrol_build_runner(
            &config,
            workdir,
            def.clone(),
            &client,
            &[
                "evolution_signals",
                "rule_create",
                "rule_submit",
                "rule_versions",
            ],
        )
        .await?;
        patrol_consume(runner, prompt_a, "轮A").await
    });
    let turn_a = match turn_a {
        Ok(r) if r.success => r,
        Ok(r) => {
            eprintln!("[patrol] 轮A 失败: {}", r.error.unwrap_or_default());
            report["error"] = serde_json::json!("patrol turn A failed");
            write_report(&report, out);
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("[patrol] 轮A 失败: {e}");
            report["error"] = serde_json::json!(format!("patrol turn A failed: {e}"));
            write_report(&report, out);
            return ExitCode::from(1);
        }
    };
    report["actions"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"action": "draft", "tools": turn_a.tool_calls}));
    let Some(version_id) = extract_keyed_value(&turn_a.content, "VERSION_ID") else {
        eprintln!("[patrol] 轮A 产物解析失败: 未找到 VERSION_ID 行");
        report["error"] = serde_json::json!("turn A: VERSION_ID not found in agent output");
        report["turn_a_text"] = serde_json::json!(turn_a.content);
        write_report(&report, out);
        return ExitCode::from(1);
    };
    let Some(draft_json) = extract_last_fenced_json(&turn_a.content) else {
        eprintln!("[patrol] 轮A 产物解析失败: 未找到约束草稿围栏 JSON");
        report["error"] = serde_json::json!("turn A: constraint draft fenced json not found");
        report["turn_a_text"] = serde_json::json!(turn_a.content);
        write_report(&report, out);
        return ExitCode::from(1);
    };
    // 提交期结构校验（意图静默丢失防御）：transform 条目级只允许 {type,params}，
    // type 必须 enforce（拒绝 set 留痕型），params 必含 domain/reason。
    // 校验失败即 fail-fast 落报告退出——不带病进入轮B/提交链路。
    if let Err(reason) = validate_constraint_draft(&draft_json) {
        eprintln!("[patrol] 约束草稿结构校验失败: {reason}");
        report["error"] = serde_json::json!(format!(
            "turn A: constraint draft validation failed: {reason}"
        ));
        report["draft_json"] = serde_json::json!(draft_json);
        report["turn_a_text"] = serde_json::json!(turn_a.content);
        write_report(&report, out);
        return ExitCode::from(1);
    }
    let test_case = extract_keyed_value(&turn_a.content, "TEST_CASE")
        .unwrap_or_else(|| "{\"probe\": true}".to_string());
    report["version_id"] = serde_json::json!(version_id);
    eprintln!("[patrol] 轮A 完成: VERSION_ID={version_id}");

    // 7. 操作者组装闸门一证据(dataset → sandbox_start → close;与治理审批同范式)
    let evidence = runtime.block_on(async {
        let ds = ws_client
            .create_test_dataset(
                workspace,
                evo_agent::api::workspace_client::CreateTestDatasetRequest {
                    name: "进化巡视沙盒证据数据集".to_string(),
                    cases_json: format!("[{test_case}]"),
                    created_by: "evo-agent-patrol".to_string(),
                    workspace_id: Some(workspace.to_string()),
                    description: Some("进化巡视自动组装的闸门一证据".to_string()),
                },
            )
            .await?;
        let sb = ws_client
            .start_sandbox(
                workspace,
                evo_agent::api::workspace_client::StartSandboxRequest {
                    rule_version_ids: vec![version_id.clone()],
                    test_dataset_id: ds.id,
                    parent_version: None,
                },
                "evo-agent-patrol",
            )
            .await?;
        ws_client
            .close_sandbox(workspace, sb.sandbox_id, "evo-agent-patrol")
            .await?;
        Ok::<i64, evo_agent::api::ApiError>(sb.sandbox_id)
    });
    let sandbox_id = match evidence {
        Ok(id) => id,
        Err(e) => {
            eprintln!("[patrol] 闸门一证据组装失败: {e}");
            report["error"] = serde_json::json!(format!("gate-one evidence failed: {e}"));
            write_report(&report, out);
            return ExitCode::from(1);
        }
    };
    report["sandbox_id"] = serde_json::json!(sandbox_id);
    report["actions"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"action": "gate_one_evidence", "sandbox_id": sandbox_id}));
    eprintln!("[patrol] 闸门一证据就绪: sandbox_id={sandbox_id}");

    // 8. 轮B:agent 携证据 id 调 rule_promote 提名(进人审队列)
    let prompt_b = format!(
        "你只能使用 rule_promote 这一个工具,其余工具一律不可用,禁止尝试调用它们。\n\
         请调用 rule_promote 工具提交约束层晋升提名,参数如下(严格照传,不要修改内容):\n\
         - workspace_id: \"{workspace}\"\n\
         - rule_version_ids: [\"{version_id}\"]\n\
         - meta_rule_content: 下面围栏 JSON 的字符串形式:\n\
         ```json\n{draft_json}\n```\n\
         - test_report_sandbox_id: {sandbox_id}\n\
         - submitted_by: \"evo-agent-patrol\"\n\
         - role: \"department_head\"\n\
         - description: \"进化巡视自动提名\"\n\
         完成后用中文简述提名结果。若工具返回校验错误,按错误信息修正参数后立即重试,\n\
         不要向操作者询问或等待指示。"
    );
    eprintln!("[patrol] 轮B:治理链提名(start)");
    let turn_b = runtime.block_on(async {
        let runner = patrol_build_runner(&config, workdir, def, &client, &["rule_promote"]).await?;
        patrol_consume(runner, prompt_b, "轮B").await
    });
    let turn_b = match turn_b {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[patrol] 轮B 失败: {e}");
            report["error"] = serde_json::json!(format!("patrol turn B failed: {e}"));
            write_report(&report, out);
            return ExitCode::from(1);
        }
    };
    report["actions"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"action": "nominate", "tools": turn_b.tool_calls}));

    // 9. 轮询治理队列确认提名入队(带 workspace 过滤,kind=meta_promotion)
    let poll_result = runtime.block_on(async {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            match ws_client
                .list_publish_queue(Some("pending"), Some(workspace))
                .await
            {
                Ok(items) => {
                    if let Some(item) = items
                        .iter()
                        .find(|i| i.kind == "meta_promotion" && i.workspace_id == workspace)
                    {
                        return Ok(item.clone());
                    }
                }
                Err(e) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(e.to_string());
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err("timeout waiting for meta_promotion queue item".to_string());
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });
    match poll_result {
        Ok(item) => {
            report["status"] = serde_json::json!("nominated");
            report["queue_id"] = serde_json::json!(item.id);
            report["queue_status"] = serde_json::json!(item.status);
            write_report(&report, out);
            eprintln!("[patrol] 提名入队: queue_id={} (等待人工审批)", item.id);
            ExitCode::SUCCESS
        }
        Err(e) => {
            // 提名可能已被预算门禁拒绝(重复提名 409)或入队超时——按轮B结果区分
            let duplicate = turn_b.error.is_none() && !turn_b.success;
            report["turn_b_text"] = serde_json::json!(turn_b.content);
            if turn_b.error.is_none() && turn_b.content.contains("已存在") {
                report["status"] = serde_json::json!("duplicate_rejected");
            } else if duplicate {
                report["status"] = serde_json::json!("turn_b_failed");
            }
            report["error"] = serde_json::json!(format!("queue confirm failed: {e}"));
            write_report(&report, out);
            ExitCode::from(1)
        }
    }
}

/// G4:非流式运行(原逻辑)
fn cmd_run_blocking(
    runtime: &tokio::runtime::Runtime,
    runner: &mut AgentRunner,
    goal: &str,
) -> ExitCode {
    let run_result = runtime.block_on(async { runner.run(goal).await });

    match run_result {
        Ok(result) => {
            let json = match serde_json::to_string_pretty(&result) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("failed to serialize result: {}", e);
                    return ExitCode::from(1);
                }
            };
            println!("{}", json);
            if result.success {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("agent run failed: {}", e);
            ExitCode::from(1)
        }
    }
}

/// G4:流式运行 — 逐 token 打印 LLM 输出,事件实时显示
fn cmd_run_streaming(
    runtime: &tokio::runtime::Runtime,
    runner: AgentRunner,
    goal: &str,
) -> ExitCode {
    use evo_agent::AgentEvent;
    use futures_util::StreamExt;
    use std::io::Write;

    let final_result = runtime.block_on(async move {
        let mut event_stream = runner.run_streaming(goal.to_string());

        let mut final_result: Option<evo_agent::AgentResult> = None;

        while let Some(event) = event_stream.next().await {
            match event {
                Ok(AgentEvent::SessionCreated { session_id, .. }) => {
                    eprintln!("[session: {}]", session_id);
                }
                Ok(AgentEvent::Step { step }) => {
                    eprintln!("\n--- step {} ---", step);
                }
                Ok(AgentEvent::LlmDelta { text }) => {
                    // LLM 增量文本 → stdout(不换行,实时显示)
                    print!("{}", text);
                    let _ = std::io::stdout().flush();
                }
                Ok(AgentEvent::LlmDone {
                    content: _,
                    finish_reason,
                }) => {
                    // 本轮 LLM 输出结束
                    println!();
                    eprintln!(
                        "[llm done: {}]",
                        finish_reason.unwrap_or_else(|| "?".to_string())
                    );
                }
                Ok(AgentEvent::ToolCall { name, args }) => {
                    eprintln!("[tool call: {}] {}", name, args);
                }
                Ok(AgentEvent::ToolResult { name, result }) => {
                    eprintln!("[tool result: {}] {}", name, result);
                }
                Ok(AgentEvent::ApprovalRequired {
                    tool_name,
                    command,
                    risk,
                    alternative,
                    ..
                }) => {
                    eprintln!("\n[⚠️ approval required] tool: {}", tool_name);
                    eprintln!("  command: {}", command);
                    eprintln!("  risk: {}", risk);
                    if !alternative.is_empty() {
                        eprintln!("  alternative: {}", alternative);
                    }
                }
                Ok(AgentEvent::ApprovalResult {
                    tool_name,
                    approved,
                    ..
                }) => {
                    if approved {
                        eprintln!("[✓ approved: {}] re-executing...", tool_name);
                    } else {
                        eprintln!("[✗ denied: {}] returning rejected", tool_name);
                    }
                }
                Ok(AgentEvent::Error(e)) => {
                    eprintln!("[error: {}]", e);
                }
                Ok(AgentEvent::Info(msg)) => {
                    eprintln!("[info: {}]", msg);
                }
                Ok(AgentEvent::Done(result)) => {
                    final_result = Some(result);
                    break;
                }
                Err(e) => {
                    eprintln!("[fatal: {}]", e);
                    final_result = Some(evo_agent::AgentResult::error(e.to_string(), 0, 0));
                    break;
                }
            }
        }

        final_result
    });

    match final_result {
        Some(result) => {
            // 最终结果以 JSON 输出到 stdout
            let json = match serde_json::to_string_pretty(&result) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("failed to serialize result: {}", e);
                    return ExitCode::from(1);
                }
            };
            println!("{}", json);
            if result.success {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        None => {
            eprintln!("stream ended without result");
            ExitCode::from(1)
        }
    }
}

// =============================================================================
// list —— 列 agent
// =============================================================================

fn cmd_list(workdir: &Path, dir: Option<&Path>) -> ExitCode {
    let agents_dir = match dir {
        Some(d) => d.to_path_buf(),
        None => match evo_agent::config::Config::load_lenient(workdir) {
            Ok(c) => c.agents.dir,
            Err(e) => {
                eprintln!("config error: {}", e);
                return ExitCode::from(1);
            }
        },
    };
    let mgr = AgentDefinitionManager::new(agents_dir.clone());
    match mgr.list_types() {
        Ok(types) => {
            if types.is_empty() {
                println!("(no agent definitions found in {})", agents_dir.display());
            } else {
                println!("Available agents in {}:", agents_dir.display());
                for t in types {
                    // 尝试加载详细
                    let detail = mgr.load(&t).ok();
                    let (version, desc) = match detail {
                        Some(d) => (d.version, d.description),
                        None => ("?".to_string(), "?".to_string()),
                    };
                    println!("  - {} (v{}): {}", t, version, desc);
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed to list agents: {}", e);
            ExitCode::from(1)
        }
    }
}

// =============================================================================
// tools —— 工具(3 层)
// =============================================================================

fn cmd_tools(_workdir: &Path, action: ToolsAction) -> ExitCode {
    match action {
        ToolsAction::List => tools_list(),
        ToolsAction::Show { name } => tools_show(&name),
    }
}

fn tools_list() -> ExitCode {
    println!("=== 6 Built-in Tools (3-layer security model) ===\n");

    println!("[ACTIVE] 直接执行(无需请示):");
    for spec in default_tool_specs() {
        println!("  - {}", spec.name);
    }
    println!();

    println!("[CANDIDATE] 备选(LLM 想用 → 摊开 proposal 给你看 → 你批 → 再执行):");
    for c in shell_exec::CANDIDATE_COMMANDS {
        println!("  - {} — {} (risk: {})", c.name, c.description, c.risk);
    }
    println!("  (http_get candidate host:任何不在 active 列表的公开 host)");
    println!();

    println!("[BLOCKED] 永不批准(逃逸出口 / 不可逆破坏):");
    for (name, reason) in shell_exec::BLOCKED_COMMANDS {
        println!("  - {} — {}", name, reason);
    }
    println!(
        "  - (http_get blocked) 127.0.0.0/8, 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, \
         169.254.0.0/16 (cloud metadata!), http://"
    );
    println!();

    println!("=== 5 设计原则(详见 DESIGN_PRINCIPLES.md) ===");
    println!("  active 白名单 = 透明 + 可选(LLM 可以直接用)");
    println!("  candidate 备选 = 可控(必须 user 批准,展开 proposal)");
    println!("  blocked 黑名单 = 不可越权(永不执行)");
    println!("  fact log    = 可回放 + 可审计(每个决定留痕)");

    ExitCode::SUCCESS
}

fn tools_show(name: &str) -> ExitCode {
    // 先在 active 找
    if let Some(spec) = default_tool_specs().into_iter().find(|s| s.name == name) {
        print_spec(spec, "ACTIVE");
        return ExitCode::SUCCESS;
    }
    // shell_exec candidate
    if let Some(c) = shell_exec::CANDIDATE_COMMANDS
        .iter()
        .find(|c| c.name == name)
    {
        println!("[CANDIDATE] {}", c.name);
        println!("  description: {}", c.description);
        println!("  risk:        {}", c.risk);
        println!("  alternative: {}", c.alternative);
        return ExitCode::SUCCESS;
    }
    // shell_exec blocked
    if let Some((_, reason)) = shell_exec::BLOCKED_COMMANDS
        .iter()
        .find(|(n, _)| *n == name)
    {
        println!("[BLOCKED] {}", name);
        println!("  reason: {}", reason);
        return ExitCode::SUCCESS;
    }
    // http_get host
    if http_get::ACTIVE_HOSTS.contains(&name) {
        println!("[ACTIVE http_get] {}", name);
        println!("  直接请求,无需请示");
        return ExitCode::SUCCESS;
    }
    eprintln!("unknown tool or command: '{}'", name);
    ExitCode::from(1)
}

fn print_spec(spec: ToolSpec, layer: &str) {
    println!("[{}] {}", layer, spec.name);
    println!("  description: {}", spec.description);
    println!("  parameters:");
    for p in spec.parameters {
        let req = if p.required { " (required)" } else { "" };
        println!(
            "    - {}{} : {}  — {}",
            p.name, req, p.r#type, p.description
        );
    }
}

// =============================================================================
// validate —— 校验 agent.json
// =============================================================================

fn cmd_validate(workdir: &Path, agent: &str) -> ExitCode {
    let agents_dir = match evo_agent::config::Config::load_lenient(workdir) {
        Ok(c) => c.agents.dir,
        Err(e) => {
            eprintln!("config error: {}", e);
            return ExitCode::from(1);
        }
    };
    let mgr = AgentDefinitionManager::new(agents_dir);
    match mgr.load(agent) {
        Ok(def) => {
            println!("OK: agent '{}' v{}", def.agent_type, def.version);
            println!("  description: {}", def.description);
            println!("  model:       {} (temp {})", def.model, def.temperature);
            println!("  max_steps:   {}", def.max_steps);
            println!("  tools:       {:?}", def.tools);
            println!(
                "  memory:      type='{}' namespace='{}'",
                def.memory.memory_type, def.memory.namespace
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("INVALID: agent '{}': {}", agent, e);
            ExitCode::from(1)
        }
    }
}

// =============================================================================
// config —— 显示合并后的配置
// =============================================================================

fn cmd_config(workdir: &Path) -> ExitCode {
    match evo_agent::config::Config::load_lenient(workdir) {
        Ok(c) => match serde_json::to_string_pretty(&c) {
            Ok(s) => {
                println!("{}", s);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("failed to serialize config: {}", e);
                ExitCode::from(1)
            }
        },
        Err(e) => {
            eprintln!("config error: {}", e);
            ExitCode::from(1)
        }
    }
}

// =============================================================================
// serve —— G5:启动 HTTP server
// =============================================================================

/// G5:启动 axum HTTP server,对外提供 agent API
///
/// 端点一览(详见 `agent_api::router_with_auth`):
/// - `GET  /health` — 健康检查(免鉴权)
/// - `GET  /agents` — 列出可用 agent(G7:需鉴权)
/// - `GET  /agents/{t}` — 查看 agent 定义(G7:需鉴权)
/// - `POST /agents/{t}/run` — 同步执行(G7:需鉴权)
/// - `POST /agents/{t}/run/stream` — SSE 流式执行(G4,G7:需鉴权)
/// - `POST /agents/{t}/cancel?session_id=xxx` — 取消正在运行的 session(G6,G7:需鉴权)
/// - `POST /agents/{t}/approve` — 审批 candidate 工具调用(G8,G7:需鉴权)
///
/// G7 鉴权配置优先级:CLI `--auth-token`/`--no-auth` > 环境变量 > 配置文件 > 默认(disabled)
///
/// 优雅关闭:Ctrl+C / SIGTERM → 停止接受新连接,等待在途请求完成。
fn cmd_serve(
    workdir: &Path,
    host: &str,
    port: u16,
    auth_tokens: &[String],
    no_auth: bool,
) -> ExitCode {
    use evo_agent::api::agent_api;
    use evo_agent::api::auth::AuthConfig;
    use tower_http::cors::CorsLayer;
    use tower_http::limit::RequestBodyLimitLayer;

    // 0. 加载 .env(若存在;已设置的环境变量优先,不覆盖)——O-095 结构性修复,
    //    裸启动 serve 也能带 LLM 密钥,不再依赖外部注入
    if let Some((path, applied)) = evo_agent::dotenv::load_dotenv_for(workdir) {
        eprintln!("[dotenv] loaded {} key(s) from {}", applied, path.display());
    }

    // 1. 加载配置(宽松模式:server 启动不需要 LLM API key,只在 run 时才需要)
    let config = match evo_agent::config::Config::load_lenient(workdir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {}", e);
            return ExitCode::from(1);
        }
    };

    // 2. 初始化 logging(尊重 config.logging)
    init_logging_for_serve(&config);

    // 2.5 O-095 启动期告警:三个 provider 密钥全缺失时显式提示,避免 LLM 失联假故障被误判
    let has_llm_key = ["MINIMAX_API_KEY", "DEEPSEEK_API_KEY", "OPENAI_API_KEY"]
        .iter()
        .any(|k| {
            std::env::var(k)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
        });
    if !has_llm_key {
        eprintln!(
            "[llm] WARNING: no LLM API key env found (MINIMAX_API_KEY / DEEPSEEK_API_KEY / OPENAI_API_KEY)"
        );
        eprintln!(
            "[llm] WARNING: agent LLM calls will fail; set env vars or place a .env in the serve workdir (auto-loaded at startup)"
        );
    }

    // 3. G7:构造鉴权配置
    //    优先级:--no-auth > --auth-token > 配置文件/环境变量
    let auth_config = if no_auth {
        AuthConfig::disabled()
    } else if !auth_tokens.is_empty() {
        AuthConfig::new(auth_tokens.to_vec(), true)
    } else {
        config.auth.to_auth_config()
    };

    if auth_config.enabled() {
        eprintln!(
            "[auth] HTTP API authentication enabled ({} token(s))",
            auth_tokens.len().max(config.auth.tokens.len())
        );
    } else {
        eprintln!("[auth] HTTP API authentication disabled");
    }

    // 4. 构造 API state
    // 相对 agents.dir 相对 workdir 解析(与 config 加载基准一致),不基于进程 cwd
    let agents_dir = if config.agents.dir.is_absolute() {
        config.agents.dir.clone()
    } else {
        workdir.join(&config.agents.dir)
    };
    let definitions = AgentDefinitionManager::new(agents_dir.clone());

    // 启动期档案预载校验:缺失/坏 JSON/语义非法 fail-fast 并逐项列明,
    // 消灭「会话期才报 agent not found」的延迟故障(含相对路径解析基准显式化)
    match definitions.list_types() {
        Ok(types) if types.is_empty() => {
            eprintln!(
                "agent definition preload failed: no agent definitions found in {} \
                 (resolve base = workdir {:?})",
                agents_dir.display(),
                workdir
            );
            return ExitCode::from(1);
        }
        Ok(types) => {
            let failures: Vec<String> = types
                .iter()
                .filter_map(|t| definitions.load(t).err().map(|e| format!("  - {t}: {e}")))
                .collect();
            if !failures.is_empty() {
                eprintln!(
                    "agent definition preload failed: {} problem(s) in {}:\n{}",
                    failures.len(),
                    agents_dir.display(),
                    failures.join("\n")
                );
                return ExitCode::from(1);
            }
            eprintln!(
                "[agents] {} definition(s) preloaded and validated from {}",
                types.len(),
                agents_dir.display()
            );
        }
        Err(e) => {
            eprintln!(
                "agent definition preload failed: cannot list {}: {e} (resolve base = workdir {:?})",
                agents_dir.display(),
                workdir
            );
            return ExitCode::from(1);
        }
    }

    let evorule_client =
        EvoruleApiClient::with_auth_token(&config.evorule.base_url, Some(&config.evorule.api_key));
    // E1:构造 workspace_client + union toolkit(启动时一次组装 26 个工具)
    let workspace_client = std::sync::Arc::new(WorkspaceApiClient::new(evorule_client.base_url()));
    let mut toolkit = evo_agent::api::serve_tools::build_union_toolkit(
        workdir,
        &workspace_client,
        &evorule_client,
    );
    // 服务消费桥:按白名单把 server 插件服务注册为代理工具
    // (注册失败降级继续并明示影响,不阻塞 server 启动)
    if !config.evorule.service_tools.is_empty() {
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => match rt.block_on(evo_agent::service_tools::register_service_tools(
                &mut toolkit,
                &evorule_client,
                &config.evorule.service_tools,
            )) {
                Ok(n) => eprintln!("[service-tools] {n} service tool(s) registered"),
                Err(e) => {
                    eprintln!("[service-tools] 服务工具注册失败,serve 面无服务工具: {e}")
                }
            },
            Err(e) => eprintln!("[service-tools] 临时 runtime 构建失败: {e}"),
        }
    }
    let toolkit_tool_count = toolkit.tool_names().len();
    let toolkit = std::sync::Arc::new(toolkit);
    eprintln!(
        "[tools] union toolkit assembled: {} tool(s)",
        toolkit_tool_count
    );
    // G17:构造共享 metrics(供 /metrics 端点 + runner 插桩共用)
    let metrics = match evo_agent::Metrics::new() {
        Ok(m) => std::sync::Arc::new(m),
        Err(e) => {
            eprintln!("failed to create metrics registry: {}", e);
            return ExitCode::from(1);
        }
    };
    // P2-V3 止血:安装审计链旁路调用的指标回调(Summarizer 影子调用可见化)
    {
        let m = metrics.clone();
        let _ = evo_agent::metrics::set_bypass_audit_hook(move |purpose| {
            evo_agent::Metrics::inc_llm_bypass_audit(&m, purpose);
        });
    }
    // P5-A3 指标:安装 L2 SafetyAuditor 命中上报回调(召回污染态势可见化)
    {
        let m = metrics.clone();
        let _ = evo_agent::metrics::set_safety_hit_hook(move |rule| {
            evo_agent::Metrics::inc_safety_audit_hit(&m, rule);
        });
    }
    let state = AgentApiState::new_with_metrics(
        definitions,
        evorule_client,
        metrics,
        workdir.to_path_buf(),
        workspace_client,
        toolkit,
    )
    // 凭据可视化:注入 LLM 配置脱敏快照(响应体不含任何密钥内容)
    .with_llm_status(std::sync::Arc::new(
        config
            .llm
            .status_snapshot(config.llm_api_key_source.as_deref()),
    ));

    // G12:MCP 工具注册(P1 边界:只在 `run` 子命令生效)
    //
    // `serve` 模式已接入完整工具面(`src/api/serve_tools.rs`:union toolkit =
    // 内置 6 + 规则 23 共 29 工具,存 `AgentApiState.toolkit`,按 agent 白名单
    // `build_filtered_toolkit` 过滤后注入每请求 runner)。MCP 适配器需要共享
    // 长生命周期的 McpClient(子进程),按请求 spawn 代价过高,故未入 serve
    // toolkit。因此 P1 阶段 MCP 工具仅在 `evo-agent run` 中生效;serve 模式
    // 的 MCP 接入留待后续迭代。
    if !config.mcp.servers.is_empty() {
        eprintln!(
            "[mcp] {} server(s) configured, but MCP tools are only active in `evo-agent run` mode (P1)",
            config.mcp.servers.len()
        );
    }

    // 5. 构造 router + 中间件(G7 鉴权 + CORS + 1MB body limit)
    //
    // 工作台静态托管:API 路由外层 fallback 到 web/dist(SPA,缺省回退 index.html)。
    // fallback 挂在外层 Router 上,不经过 API 面的 auth 中间件 —— 工作台页面必须
    // 无 token 可打开;API 与 WS 面仍全部走 G7 鉴权(auth 启用时前端以 ?token= 接入)。
    let dist_dir = workdir.join("web").join("dist");
    if dist_dir.join("index.html").exists() {
        eprintln!(
            "[workbench] serving workbench UI from {} at /",
            dist_dir.display()
        );
    } else {
        eprintln!(
            "[workbench] web/dist not found ({}); workbench disabled, API-only mode. \
             Build: npm --prefix web install && npm --prefix web run build",
            dist_dir.display()
        );
    }
    let workbench = tower_http::services::ServeDir::new(&dist_dir).not_found_service(
        tower_http::services::ServeFile::new(dist_dir.join("index.html")),
    );
    let app = axum::Router::new()
        .merge(agent_api::router_with_auth(state, auth_config))
        .fallback_service(workbench)
        .layer(CorsLayer::permissive())
        .layer(RequestBodyLimitLayer::new(1024 * 1024));

    // 6. 多线程 runtime(server 需要并发处理请求)
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to build tokio runtime: {}", e);
            return ExitCode::from(1);
        }
    };

    // O-094:console 审计页 sidecar 自动拉起(配置化,fail-soft)。
    // workbench.console_dir 配置 console-cloud 仓目录后,serve 启动期探测端口,
    // 未监听则拉起 vite dev(审计深链数据源);未配置/目录无效/拉起失败仅告警。
    if let Some(console_dir) = config.workbench.console_dir.clone() {
        let port = config
            .workbench
            .console_port
            .unwrap_or(evo_agent::api::console_sidecar::DEFAULT_CONSOLE_PORT);
        let log_file = workdir.join("data").join("console_sidecar.log");
        runtime.spawn(async move {
            let (spawned, msg) = evo_agent::api::console_sidecar::ensure_console_dev(
                &console_dir,
                port,
                Some(&log_file),
            )
            .await;
            eprintln!("[console-sidecar] {}", msg);
            if spawned {
                eprintln!(
                    "[console-sidecar] audit deep-link target: http://localhost:{}/audit",
                    port
                );
            }
        });
    }

    // O-093:快照目录自建 + 启动清扫 + 每日周期清理。
    // 保留期每次清理时从 workbench_config.json 现读(改配置下次清理即生效);
    // 删除只作用于 data/snapshots/ 目录,永不越界(展示层副本,非审计链)。
    // 注:此处按同路径新建独立实例(state 已移入 router;两 store 仅持路径,无状态)。
    {
        let snapshots =
            evo_agent::api::snapshots::SnapshotStore::new(workdir.join("data").join("snapshots"));
        let config_store = evo_agent::api::snapshots::WorkbenchConfigStore::new(
            workdir.join("data").join("workbench_config.json"),
        );
        snapshots.ensure_dir();
        let policy =
            evo_agent::api::snapshots::RetentionPolicy::from_label(&config_store.load().retention)
                .unwrap_or(evo_agent::api::snapshots::RetentionPolicy::Month3);
        let removed = snapshots.cleanup_expired(policy);
        if removed > 0 {
            eprintln!(
                "[snapshots] startup cleanup: removed {} expired day-dir(s) (retention={})",
                removed,
                policy.label()
            );
        }
        runtime.spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await; // 首次 tick 立即返回(启动清扫已做,消费掉)
            loop {
                interval.tick().await;
                let policy = evo_agent::api::snapshots::RetentionPolicy::from_label(
                    &config_store.load().retention,
                )
                .unwrap_or(evo_agent::api::snapshots::RetentionPolicy::Month3);
                let removed = snapshots.cleanup_expired(policy);
                if removed > 0 {
                    eprintln!(
                        "[snapshots] periodic cleanup: removed {} expired day-dir(s) (retention={})",
                        removed,
                        policy.label()
                    );
                }
            }
        });
    }

    // 7. 绑定 + 启动
    let addr = format!("{}:{}", host, port);
    let bind_result = runtime.block_on(async { tokio::net::TcpListener::bind(&addr).await });

    let listener = match bind_result {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bind {} failed: {}", addr, e);
            return ExitCode::from(1);
        }
    };

    eprintln!("evo-agent HTTP server listening on http://{}", addr);
    eprintln!("  GET  /                       (workbench UI, web/dist)");
    eprintln!("  GET  /health                  (no auth)");
    eprintln!("  GET  /metrics                 (no auth, Prometheus G17)");
    eprintln!("  GET  /admin/llm-status        (auth, masked LLM config status)");
    eprintln!("  GET  /agents                  (auth)");
    eprintln!("  POST /agents/{{type}}/run          (auth)");
    eprintln!("  POST /agents/{{type}}/run/stream   (auth, SSE)");
    eprintln!("  POST /agents/{{type}}/cancel       (auth)");
    eprintln!("  POST /agents/{{type}}/approve      (auth, G8)");
    eprintln!("press Ctrl+C to shut down");

    runtime.block_on(async {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .expect("server error");
    });

    eprintln!("server shut down gracefully");
    ExitCode::SUCCESS
}

/// G5:根据 config.logging 初始化日志
fn init_logging_for_serve(config: &evo_agent::config::Config) {
    use tracing_subscriber::{fmt, EnvFilter};
    let level = &config.logging.level;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    if config.logging.format == "json" {
        let _ = fmt().with_env_filter(filter).json().try_init();
    } else {
        let _ = fmt().with_env_filter(filter).with_target(false).try_init();
    }
}

/// G5:优雅关闭信号监听
///
/// 收到 Ctrl+C(Unix+Windows)或 SIGTERM(Unix)后返回,
/// axum 停止接受新连接并等待在途请求完成。
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => eprintln!("\n[Ctrl+C received, graceful shutdown...]"),
        _ = terminate => eprintln!("\n[SIGTERM received, graceful shutdown...]"),
    }
}

// =============================================================================
// workflow —— G9:多 agent 工作流(DAG 编排)
// =============================================================================

/// G9:执行多 agent 工作流
///
/// 从 `rules/workflows/<id>.json` 加载工作流定义,拓扑排序后逐层并行执行,
/// 把上游节点结果填入下游 `task_template`,最终输出 `output_node` 的结果。
///
/// # 子 agent 配置
///
/// 子 agent 通过 `DelegateContext::delegate()` 执行,内部用 `AgentRunner::new`
/// + `LlmHandler::with_defaults()`(读环境变量 API key)。因此执行前需确保:
/// - evorule server 可达(`config.evorule.base_url`)
/// - LLM API key 环境变量已设置(`MINIMAX_API_KEY` / `DEEPSEEK_API_KEY` / `OPENAI_API_KEY`)
/// - 各 `agent_type` 对应的 `agents/<type>.json` 已定义
fn cmd_workflow(
    workdir: &Path,
    workflow_id: &str,
    dir: Option<&Path>,
    max_depth: usize,
    max_concurrent: usize,
) -> ExitCode {
    // 1. 加载配置(宽松模式:workflow 子命令需要 evorule base_url + agents dir)
    let config = match evo_agent::config::Config::load_lenient(workdir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {}", e);
            return ExitCode::from(1);
        }
    };

    // 2. 定位 workflow 文件
    let workflows_dir = match dir {
        Some(d) => d.to_path_buf(),
        None => workdir.join("rules").join("workflows"),
    };
    let wf_path = workflows_dir.join(format!("{}.json", workflow_id));
    let wf_content = match std::fs::read_to_string(&wf_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "failed to load workflow '{}' from {}: {}",
                workflow_id,
                wf_path.display(),
                e
            );
            return ExitCode::from(1);
        }
    };

    // 3. 解析 workflow JSON
    let wf_value: serde_json::Value = match serde_json::from_str(&wf_content) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("failed to parse workflow '{}': {}", workflow_id, e);
            return ExitCode::from(1);
        }
    };

    // 3.5 校验并加载(宪法 jsonschema 全量校验 + v1.2 物化 / v1.0-v1.1 反序列化;
    //     workflow_dag v1.0/v1.1/v1.2 按文档形态分派,失败 fail-fast 拒载)
    let wf: Workflow = match evo_agent::agent::constitution::load_workflow(&wf_value) {
        Ok(w) => w,
        Err(violations) => {
            eprintln!(
                "workflow '{}' failed constitution validation/materialization (workflow_dag): {}",
                workflow_id,
                violations.join("; ")
            );
            return ExitCode::from(1);
        }
    };

    eprintln!(
        "workflow '{}' ({} nodes, output='{}') from {}",
        wf.workflow_id,
        wf.nodes.len(),
        wf.output_node,
        wf_path.display()
    );
    for n in &wf.nodes {
        let deps = if n.depends_on.is_empty() {
            "(no deps)".to_string()
        } else {
            format!("depends_on: {:?}", n.depends_on)
        };
        eprintln!("  - {} [{}] {}", n.id, n.agent_type, deps);
    }

    // 4. 构造 DelegateContext
    let definitions = AgentDefinitionManager::new(config.agents.dir.clone());
    let client =
        EvoruleApiClient::with_auth_token(&config.evorule.base_url, Some(&config.evorule.api_key));
    let mut ctx =
        DelegateContext::new("workflow_root", definitions, client).with_max_depth(max_depth);
    if max_concurrent > 0 {
        ctx = ctx.with_max_concurrent_delegates(max_concurrent);
    }

    // 5. 执行(current_thread runtime:async I/O 并发足够,与 cmd_run 一致)
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to build tokio runtime: {}", e);
            return ExitCode::from(1);
        }
    };

    let engine = WorkflowEngine::new(ctx);
    let started = std::time::Instant::now();
    let result = runtime.block_on(engine.execute(&wf));
    let wall_ms = started.elapsed().as_millis() as u64;

    // replan 触发判定骨架(交付物 6 §2;T4 骨架:失败/预算触发路径先通,
    // 外层驱动循环重调 planner 产 v2 属 Phase 1-B——骨架阶段输出决策并以非零码退出)
    // nodes_executed 真实累加来源 = 外层驱动循环的节点完成事件(Phase 1-B),骨架置 0;
    // tokens_used MVP 恒 0(交付物 6 §4.1,Phase 2 埋点)
    let counters = BudgetCounters {
        nodes_executed: 0,
        wall_ms,
        tokens_used: 0,
    };
    let thresholds = BudgetThresholds::defaults(wf.nodes.len());
    let replan_state = ReplanState {
        current_version: 1,
        replan_count: 0,
    };
    if let Some(decision) = should_replan(&result, &counters, &thresholds, &replan_state) {
        match decision.reason {
            ReplanReason::Failure => {
                let mut record = decision
                    .failure_record
                    .expect("failure decision carries record");
                if let Some(node_id) = &record.failed_node_id {
                    record.agent_type = lookup_agent_type(&wf.nodes, node_id);
                }
                eprintln!(
                    "\nreplan triggered (failure): node={:?} agent={:?} plan_version={} \
                     (replan driver loop lands in Phase 1-B)",
                    record.failed_node_id, record.agent_type, record.failed_plan_version
                );
            }
            ReplanReason::Budget => {
                let snap = decision
                    .budget_snapshot
                    .expect("budget decision carries snapshot");
                eprintln!(
                    "\nreplan triggered (budget): nodes={} wall_ms={} tokens={} \
                     (replan driver loop lands in Phase 1-B)",
                    snap.nodes_executed, snap.wall_ms, snap.tokens_used
                );
            }
        }
        return ExitCode::from(1);
    }

    match result {
        Ok(content) => {
            eprintln!("\n=== workflow '{}' done ===", wf.workflow_id);
            println!("{}", content);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nworkflow '{}' failed: {}", wf.workflow_id, e);
            ExitCode::from(1)
        }
    }
}

// =============================================================================
// repl —— G15:REPL 交互模式(对话式,复用同一 evorule session)
// =============================================================================

/// G15:REPL 交互模式入口
///
/// 面向"则灵"消费者场景:逐条输入,逐条响应,保持上下文连续。
///
/// # 工作流
///
/// 1. 加载 agent 定义 + evorule client + tool_handler(同 `cmd_run`)
/// 2. session 恢复优先级:`--session <id>` > `{workdir}/.evo-agent/session` 文件 > 首次输入新建
/// 3. rustyline 读一行输入
/// 4. 特殊命令:`/exit` `/session` `/rewind <version>`
/// 5. 首次输入:调 `runner.run_streaming(input)`,捕获 `session_id`,持久化到文件
/// 6. 后续输入:构造新 runner + `runner.run_continuation(session_id, input)`
/// 7. 实时打印 `AgentEvent`(LlmDelta 逐 token、ToolCall、ToolResult、Done)
///
/// # 跨进程恢复(Q14:B)
///
/// session_id 持久化到 `{workdir}/.evo-agent/session`(纯文本,单行)。
/// `evo-agent repl --session <id>` 或重启 REPL 时自动加载。
fn cmd_repl(
    workdir: &Path,
    agent: Option<&str>,
    auto_approve_candidates: bool,
    session: Option<&str>,
) -> ExitCode {
    use rustyline::error::ReadlineError;
    use rustyline::{DefaultEditor, Result as RlResult};

    // 1. 加载配置
    let config = match evo_agent::config::Config::load(workdir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {}", e);
            return ExitCode::from(1);
        }
    };

    let agent_name = agent.unwrap_or(&config.agents.default);
    let agents_dir = &config.agents.dir;
    let mgr = AgentDefinitionManager::new(agents_dir.clone());
    let def = match mgr.load(agent_name) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "failed to load agent '{}' from {}: {}",
                agent_name,
                agents_dir.display(),
                e
            );
            return ExitCode::from(1);
        }
    };

    let client =
        EvoruleApiClient::with_auth_token(&config.evorule.base_url, Some(&config.evorule.api_key));

    // session 文件路径(Q14:B 跨进程恢复)
    let session_file = workdir.join(".evo-agent").join("session");

    // 2. session 恢复优先级:--session > 文件 > None(首次输入新建)
    let resumed_session_id: Option<String> = if let Some(id) = session {
        Some(id.to_string())
    } else if session_file.exists() {
        match std::fs::read_to_string(&session_file) {
            Ok(content) => {
                let id = content.trim().to_string();
                if id.is_empty() {
                    None
                } else {
                    eprintln!(
                        "[repl] resumed session from {}: {}",
                        session_file.display(),
                        id
                    );
                    Some(id)
                }
            }
            Err(e) => {
                eprintln!(
                    "[repl] warning: failed to read session file {}: {}",
                    session_file.display(),
                    e
                );
                None
            }
        }
    } else {
        None
    };

    // 3. rustyline Editor
    let mut rl = match DefaultEditor::new() {
        Ok(editor) => editor,
        Err(e) => {
            eprintln!("failed to initialize readline: {}", e);
            return ExitCode::from(1);
        }
    };

    // 4. tokio runtime
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to build tokio runtime: {}", e);
            return ExitCode::from(1);
        }
    };

    eprintln!("evo-agent REPL (agent: '{}')", agent_name);
    eprintln!("type /exit to quit, /session to show session ID, /rewind <version> to rollback");
    if resumed_session_id.is_some() {
        eprintln!("[repl] continuing from existing session");
    } else {
        eprintln!("[repl] first input will create a new session");
    }

    let mut current_session: Option<String> = resumed_session_id;

    // REPL 共享环境(每轮稳定参数打包)
    let env = ReplEnv {
        runtime: &runtime,
        config: &config,
        def: &def,
        client: &client,
        workdir,
        auto_approve_candidates,
        session_file: &session_file,
    };

    // 5. REPL 主循环
    loop {
        let prompt = if current_session.is_some() {
            ">>> "
        } else {
            "[new] >>> "
        };
        let line: RlResult<String> = rl.readline(prompt);
        match line {
            Ok(input) => {
                let input = input.trim().to_string();
                if input.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(&input);

                // 特殊命令
                if input == "/exit" || input == "/quit" {
                    eprintln!("[repl] bye");
                    break;
                }
                if input == "/session" {
                    match &current_session {
                        Some(id) => eprintln!("current session: {}", id),
                        None => eprintln!("no active session (first input will create one)"),
                    }
                    continue;
                }
                if let Some(rest) = input.strip_prefix("/rewind ") {
                    let version_str = rest.trim();
                    match version_str.parse::<u64>() {
                        Ok(version) => {
                            if let Some(sid) = &current_session {
                                match runtime.block_on(client.rewind(sid, version)) {
                                    Ok(result) => {
                                        eprintln!("[rewind] ok: {}", result);
                                    }
                                    Err(e) => {
                                        eprintln!("[rewind] failed: {}", e);
                                    }
                                }
                            } else {
                                eprintln!("[rewind] no active session");
                            }
                        }
                        Err(_) => {
                            eprintln!("[rewind] invalid version: {}", version_str);
                        }
                    }
                    continue;
                }
                if input == "/help" {
                    eprintln!("commands:");
                    eprintln!("  /exit          quit REPL");
                    eprintln!("  /session       show current session ID");
                    eprintln!("  /rewind <ver>  rollback to version");
                    eprintln!("  (anything else is sent to the agent)");
                    continue;
                }

                // 普通输入:调 agent
                let exit = run_repl_turn(&env, &mut current_session, input);
                if let Some(code) = exit {
                    return code;
                }
            }
            Err(ReadlineError::Interrupted) => {
                eprintln!("[Ctrl+C] type /exit to quit");
                continue;
            }
            Err(ReadlineError::Eof) => {
                eprintln!("[EOF] bye");
                break;
            }
            Err(e) => {
                eprintln!("[repl] readline error: {}", e);
                break;
            }
        }
    }

    ExitCode::SUCCESS
}

/// REPL 每轮共享环境（把 run_repl_turn 的稳定参数打包，避免超长参数列表）。
struct ReplEnv<'a> {
    runtime: &'a tokio::runtime::Runtime,
    config: &'a evo_agent::config::Config,
    def: &'a evo_agent::agent::definition::AgentDefinition,
    client: &'a EvoruleApiClient,
    workdir: &'a Path,
    auto_approve_candidates: bool,
    session_file: &'a Path,
}

/// G15:执行一轮 REPL 对话
///
/// 首次输入(`current_session == None`):构造 runner → `run_streaming` → 捕获 session_id
/// 后续输入(`current_session == Some(id)`):构造 runner → `run_continuation(id, input)`
///
/// 返回 `Some(ExitCode)` 表示致命错误(应退出 REPL),`None` 表示继续下一轮。
fn run_repl_turn(
    env: &ReplEnv<'_>,
    current_session: &mut Option<String>,
    input: String,
) -> Option<ExitCode> {
    use evo_agent::AgentEvent;
    use futures_util::StreamExt;
    use std::io::Write;

    // 解构共享环境(全部 Copy,函数体引用方式不变)
    let ReplEnv {
        runtime,
        config,
        def,
        client,
        workdir,
        auto_approve_candidates,
        session_file,
    } = *env;

    // 构造 tool_handler + llm_handler + runner(每轮重建,因为 run_streaming/run_continuation 消费 self)
    // 与 cmd_run 一致:内置安全工具 + 规则管理工具 union,保证 rule-copilot 可用
    let ws_client =
        evo_agent::api::workspace_client::WorkspaceApiClient::new(&config.evorule.base_url);
    let mut tool_handler =
        evo_agent::api::serve_tools::build_union_toolkit(workdir, &ws_client, client);

    let runner_result = runtime.block_on(async {
        let llm_handler = evo_agent::io_handlers::LlmHandler::from_config(&config.llm);

        // G12:MCP 工具注册
        if !config.mcp.servers.is_empty() {
            let _ = evo_agent::mcp::register_mcp_tools(&mut tool_handler, &config.mcp).await;
        }

        // 服务消费桥:按白名单把 server 插件服务注册为代理工具(失败降级继续)
        if !config.evorule.service_tools.is_empty() {
            match evo_agent::service_tools::register_service_tools(
                &mut tool_handler,
                client,
                &config.evorule.service_tools,
            )
            .await
            {
                Ok(n) => eprintln!("[service-tools] {n} service tool(s) registered"),
                Err(e) => {
                    eprintln!("[service-tools] 服务工具注册失败,本次会话无服务工具: {e}")
                }
            }
        }

        evo_agent::agent::runner::AgentRunner::from_definition(
            def.clone(),
            client.clone(),
            tool_handler,
            Some(llm_handler),
        )
        .await
    });

    let runner = match runner_result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[repl] bridge error: {}", e);
            return Some(ExitCode::from(1));
        }
    };

    let runner = runner.with_approval_callback(std::sync::Arc::new(
        evo_agent::agent::approval::CliApproval {
            auto_approve: auto_approve_candidates,
        },
    ));

    // G6:Ctrl+C → cancel_token
    let cancel_token = runner.cancel_token().clone();
    let cancel_handle = runtime.spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n[Ctrl+C received, cancelling current turn...]");
            cancel_token.cancel();
        }
    });

    let event_stream = if let Some(sid) = current_session.clone() {
        // G15:continuation — 复用已有 session
        runner.run_continuation(sid, input)
    } else {
        // 首次输入 — 创建新 session
        runner.run_streaming(input)
    };

    let turn_result = runtime.block_on(async move {
        let mut event_stream = event_stream;
        let mut got_session = None;
        let mut had_error = false;

        while let Some(event) = event_stream.next().await {
            match event {
                Ok(AgentEvent::SessionCreated { session_id, .. }) => {
                    eprintln!("[session: {}]", session_id);
                    got_session = Some(session_id);
                }
                Ok(AgentEvent::Step { step }) => {
                    eprintln!("\n--- step {} ---", step);
                }
                Ok(AgentEvent::LlmDelta { text }) => {
                    print!("{}", text);
                    let _ = std::io::stdout().flush();
                }
                Ok(AgentEvent::LlmDone {
                    content: _,
                    finish_reason,
                }) => {
                    println!();
                    eprintln!(
                        "[llm done: {}]",
                        finish_reason.unwrap_or_else(|| "?".to_string())
                    );
                }
                Ok(AgentEvent::ToolCall { name, args }) => {
                    eprintln!("[tool call: {}] {}", name, args);
                }
                Ok(AgentEvent::ToolResult { name, result }) => {
                    eprintln!("[tool result: {}] {}", name, result);
                }
                Ok(AgentEvent::ApprovalRequired {
                    tool_name,
                    command,
                    risk,
                    alternative,
                    ..
                }) => {
                    eprintln!("\n[approval required] tool: {}", tool_name);
                    eprintln!("  command: {}", command);
                    eprintln!("  risk: {}", risk);
                    if !alternative.is_empty() {
                        eprintln!("  alternative: {}", alternative);
                    }
                }
                Ok(AgentEvent::ApprovalResult {
                    tool_name,
                    approved,
                    ..
                }) => {
                    if approved {
                        eprintln!("[approved: {}]", tool_name);
                    } else {
                        eprintln!("[denied: {}]", tool_name);
                    }
                }
                Ok(AgentEvent::Error(e)) => {
                    eprintln!("[error: {}]", e);
                    had_error = true;
                }
                Ok(AgentEvent::Info(msg)) => {
                    eprintln!("[info: {}]", msg);
                }
                Ok(AgentEvent::Done(_result)) => {
                    break;
                }
                Err(e) => {
                    eprintln!("[fatal: {}]", e);
                    had_error = true;
                    break;
                }
            }
        }

        (got_session, had_error)
    });

    cancel_handle.abort();

    let (got_session, had_error) = turn_result;

    // 首次输入:保存 session_id
    if let Some(sid) = &got_session {
        *current_session = Some(sid.clone());
        // 持久化到文件(Q14:B 跨进程恢复)
        if let Some(parent) = session_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(session_file, sid) {
            eprintln!(
                "[repl] warning: failed to persist session to {}: {}",
                session_file.display(),
                e
            );
        }
    }

    if had_error {
        eprintln!("[repl] turn ended with errors");
    }

    None
}

// =============================================================================
// G14:cmd_replay — 回放记忆事件链("则灵"生活回放)
// =============================================================================

#[allow(clippy::too_many_arguments)]
fn cmd_replay(
    workdir: &Path,
    session: &str,
    event: Option<&str>,
    entity: Option<&str>,
    direction: &str,
    narrate: bool,
    verify: bool,
    agent: Option<&str>,
) -> ExitCode {
    use evo_agent::{MemoryEventStore, ReplayDirection, ReplayEngine};

    // 1. 加载配置 + agent 定义(narrate 时需要 LLM 配置)
    let config = match evo_agent::config::Config::load(workdir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {}", e);
            return ExitCode::from(1);
        }
    };

    let llm_handler = if narrate {
        Some(evo_agent::io_handlers::LlmHandler::from_config(&config.llm))
    } else {
        None
    };

    // agent 定义(narrate 时用于 extraction_model 配置)
    let _def = if narrate {
        let agent_name = agent.unwrap_or(&config.agents.default);
        let mgr = AgentDefinitionManager::new(config.agents.dir.clone());
        match mgr.load(agent_name) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!(
                    "[replay] warning: failed to load agent '{}': {} (narration will use default model)",
                    agent_name, e
                );
                None
            }
        }
    } else {
        None
    };

    // 2. 构造 evorule client + MemoryEventStore
    let client =
        EvoruleApiClient::with_auth_token(&config.evorule.base_url, Some(&config.evorule.api_key));
    let namespace = agent.unwrap_or(&config.agents.default);
    let mut store = MemoryEventStore::new(namespace, client);
    store.set_session_id(session);

    // 3. 同步事件(best-effort,HTTP 失败不阻塞)
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[replay] failed to create runtime: {}", e);
            return ExitCode::from(1);
        }
    };

    let _sync_count: usize =
        runtime.block_on(async { store.sync_from_evorule().await.unwrap_or(0) });

    let event_count = store.event_count();
    eprintln!(
        "[replay] synced {} events from evorule (session: {})",
        event_count, session
    );

    if event_count == 0 {
        eprintln!(
            "[replay] no events found. Run some conversations first to generate memory events."
        );
        return ExitCode::from(0);
    }

    // 4. 构造 ReplayEngine
    let mut engine = ReplayEngine::new(store);
    if let Some(llm) = llm_handler {
        engine = engine.with_llm(llm);
    }

    // 5. 根据参数选择回放模式
    let events_result: Result<Vec<evo_agent::MemoryEvent>, String> = runtime.block_on(async {
        if let Some(event_id) = event {
            // 从指定事件出发,沿因果链回溯/前进
            let dir = if direction == "forward" {
                ReplayDirection::Forward
            } else {
                ReplayDirection::Backward
            };
            engine
                .replay_from(event_id, dir)
                .await
                .map_err(|e| e.to_string())
        } else if let Some(entity_id) = entity {
            // 按实体回放
            engine
                .replay_by_entity(entity_id)
                .await
                .map_err(|e| e.to_string())
        } else {
            // 全部事件(按时间排序)
            Ok(engine.store().list_events_sorted())
        }
    });

    let events: Vec<evo_agent::MemoryEvent> = match events_result {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[replay] error: {}", e);
            return ExitCode::from(1);
        }
    };

    if events.is_empty() {
        eprintln!("[replay] no events matched the criteria.");
        return ExitCode::from(0);
    }

    // 6. 输出
    if narrate {
        // 改进3：narrate_with_evidence — 叙述 + 逐事件证据标记
        let narrative = runtime.block_on(async { engine.narrate_with_evidence(&events).await });
        match narrative {
            Ok(n) => {
                println!("{}", n.text);
                // 改进3：打印逐事件证据标记
                if !n.evidence.is_empty() {
                    println!("\n[本回放的证据标记]");
                    for (event_id, mark) in &n.evidence {
                        println!("  {}  {}", event_id, mark);
                    }
                }
                eprintln!(
                    "\n[cited {} events, {} facts]",
                    n.cited_events.len(),
                    n.cited_facts.len()
                );
            }
            Err(e) => {
                eprintln!(
                    "[replay] narration failed: {}, falling back to structured output",
                    e
                );
                // 改进3：fallback 到结构化时间线时,verify 语义保留
                let evidence_map = if verify {
                    runtime.block_on(build_evidence_map(engine.store_mut(), &events))
                } else {
                    std::collections::BTreeMap::new()
                };
                print_structured_timeline(&events, &evidence_map);
            }
        }
    } else {
        // 改进3：结构化时间线 — 源锚点 [fact#N] 恒显示;--verify 时附加 ✓/✗
        let evidence_map = if verify {
            runtime.block_on(build_evidence_map(engine.store_mut(), &events))
        } else {
            std::collections::BTreeMap::new()
        };
        print_structured_timeline(&events, &evidence_map);
    }

    ExitCode::from(0)
}

/// 改进3：逐事件构建 event_id → 紧凑证据标记 映射（`render_compact`）。
/// server 不可用/无 fact_id 的事件自动跳过（fail-open,不影响时间线输出）。
async fn build_evidence_map(
    store: &mut evo_agent::MemoryEventStore,
    events: &[evo_agent::MemoryEvent],
) -> std::collections::BTreeMap<String, String> {
    let mut m = std::collections::BTreeMap::new();
    for ev in events {
        if let Ok(Some(evid)) = store.evidence_for_event(&ev.event_id).await {
            m.insert(ev.event_id.clone(), evid.render_compact());
        }
    }
    m
}

/// 打印结构化事件时间线(确定性,无 LLM)
///
/// 改进3：`evidence_map`（event_id → 紧凑证据标记）非空时在每行附加验证标记 `[fact#N ✓ chain=K]`；
/// 为空时仍显示本地源锚点 `[fact#N]`（event.fact_id，零依赖）。
fn print_structured_timeline(
    events: &[evo_agent::MemoryEvent],
    evidence_map: &std::collections::BTreeMap<String, String>,
) {
    use evo_agent::EventType;

    for event in events {
        let type_str = match &event.event_type {
            EventType::Conversation(_) => "对话",
            EventType::Relationship(_) => "关系",
            EventType::Milestone(_) => "里程碑",
            EventType::Habit(_) => "习惯",
            EventType::Health(_) => "健康",
            EventType::EmotionEvent => "情感",
            EventType::Location(_) => "位置",
            EventType::Item(_) => "物品",
            EventType::IOTrigger(_) => "I/O",
            EventType::SystemObservation => "系统",
            EventType::Custom(s) => s,
        };

        let summary = event
            .content
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("(无摘要)");

        let emotion_str = event
            .emotion
            .as_ref()
            .map(|e| {
                if e.labels.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", e.labels.join(","))
                }
            })
            .unwrap_or_default();

        let entities_str = if event.entities.is_empty() {
            String::new()
        } else {
            let ents: Vec<String> = event
                .entities
                .iter()
                .map(|e| format!("{}({})", e.entity_id, e.role))
                .collect();
            format!(" {{{}}}", ents.join(", "))
        };

        let cause_str = event
            .cause
            .map(|c| format!(" ←cause={}", c))
            .unwrap_or_default();

        let effects_str = if event.effects.is_empty() {
            String::new()
        } else {
            // 改进2：effects 为 EventRef（event_id + 引擎级 FactId），格式化展示
            let refs: Vec<String> = event
                .effects
                .iter()
                .map(|r| match r.fact_id {
                    Some(fid) => format!("{} (fact#{})", r.event_id, fid),
                    None => r.event_id.clone(),
                })
                .collect();
            format!(" →effects=[{}]", refs.join(", "))
        };

        // 改进3：证据标记（--verify 提供完整 render_compact；否则本地源锚点 [fact#N]）
        let evidence_mark = evidence_mark(event, evidence_map);

        println!(
            "[{}] {} {}{}{}{}{} — {}",
            event.timestamp,
            type_str,
            event.event_id,
            evidence_mark,
            emotion_str,
            entities_str,
            cause_str,
            summary
        );

        if !effects_str.is_empty() {
            println!("       {}", effects_str);
        }
    }

    eprintln!("\n[{} events]", events.len());
}

/// 改进3：计算单事件的证据标记字符串。
///
/// - `evidence_map` 命中（`--verify`）→ `" [fact#N ✓ chain=K]"`（render_compact 原文）
/// - 否则本地源锚点 → `" [fact#N]"`（零依赖，离线可用）；无 fact_id → 空串
fn evidence_mark(
    event: &evo_agent::MemoryEvent,
    evidence_map: &std::collections::BTreeMap<String, String>,
) -> String {
    match evidence_map.get(&event.event_id) {
        Some(mark) => format!(" {}", mark),
        None => event
            .fact_id
            .filter(|&f| f > 0)
            .map(|f| format!(" [fact#{}]", f))
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_agent::{EventSource, EventType, MemoryEvent};

    fn make_event(id: &str, fact_id: Option<u64>) -> MemoryEvent {
        let mut e =
            MemoryEvent::new_root(id, EventType::EmotionEvent, 1000, EventSource::UserInput)
                .with_content(serde_json::json!({"summary": "t"}));
        e.fact_id = fact_id;
        e
    }

    /// 改进3：无 --verify 时,本地源锚点 [fact#N] 恒显示(零依赖)
    #[test]
    fn test_evidence_mark_local_anchor() {
        let e = make_event("E001", Some(42));
        let map = std::collections::BTreeMap::new();
        assert_eq!(evidence_mark(&e, &map), " [fact#42]");
    }

    /// 改进3：无 fact_id → 无锚点
    #[test]
    fn test_evidence_mark_no_fact_id() {
        let e = make_event("E001", None);
        let map = std::collections::BTreeMap::new();
        assert_eq!(evidence_mark(&e, &map), "");
    }

    /// 改进3：--verify 时,evidence_map 命中优先于本地锚点(完整 render_compact)
    #[test]
    fn test_evidence_mark_verify_precedence() {
        let e = make_event("E001", Some(42));
        let mut map = std::collections::BTreeMap::new();
        map.insert("E001".to_string(), "[fact#42 ✓ chain=3]".to_string());
        assert_eq!(evidence_mark(&e, &map), " [fact#42 ✓ chain=3]");
    }

    /// 改进3：--verify 未命中(离线/无 fact_id)→ 回退本地锚点
    #[test]
    fn test_evidence_mark_verify_miss_falls_back() {
        let e = make_event("E002", Some(7));
        let map = std::collections::BTreeMap::new();
        assert_eq!(evidence_mark(&e, &map), " [fact#7]");
    }
}

#[cfg(test)]
mod patrol_parse_tests {
    use super::*;

    #[test]
    fn test_extract_last_fenced_json_takes_last_block() {
        let text = "说明文字\n```json\n{\"a\": 1}\n```\n中间文字\n```json\n{\"tier\": \"constraint\"}\n```\n收尾";
        let got = extract_last_fenced_json(text).expect("应取到最后一个围栏块");
        assert!(got.contains("\"tier\""), "实际: {got}");
        assert!(!got.contains("\"a\""), "不应取第一个块: {got}");
    }

    #[test]
    fn test_extract_last_fenced_json_without_lang_tag() {
        let text = "前言\n```\n{\"b\": 2}\n```";
        assert_eq!(
            extract_last_fenced_json(text).as_deref(),
            Some("{\"b\": 2}")
        );
    }

    #[test]
    fn test_extract_last_fenced_json_none_when_absent() {
        assert!(extract_last_fenced_json("没有任何围栏块").is_none());
        assert!(extract_last_fenced_json("```json\n未闭合").is_none());
    }

    /// 合法 enforce 草稿(含域内条件)应通过校验
    #[test]
    fn test_validate_constraint_draft_accepts_enforce() {
        let draft = r#"{
            "tier": "constraint",
            "transform": [
                {
                    "type": "enforce",
                    "params": {
                        "domain": {"type": "instruction", "instruction_type": "robot_move"},
                        "reason": "robot_move 违规拦截"
                    }
                }
            ]
        }"#;
        assert!(validate_constraint_draft(draft).is_ok());
    }

    /// 条目级 condition(转写静默丢失形态)必须 fail-fast 并列明键名
    #[test]
    fn test_validate_constraint_draft_rejects_entry_level_condition() {
        let draft = r#"{
            "transform": [
                {
                    "type": "set",
                    "condition": "__exec__.instruction.type == \"robot_move\"",
                    "params": {"attr": "meta_guard.mark", "operation": "set", "value": true}
                }
            ]
        }"#;
        let err = validate_constraint_draft(draft).expect_err("条目级 condition 应被拒绝");
        assert!(err.contains("condition"), "报错应列明未知键: {err}");
        assert!(err.contains("type/params"), "报错应说明白名单: {err}");
    }

    /// 留痕型 set 条目不构成生效约束,必须拒绝
    #[test]
    fn test_validate_constraint_draft_rejects_set_placeholder() {
        let draft = r#"{
            "transform": [
                {
                    "type": "set",
                    "params": {"attr": "meta_guard.mark", "operation": "set", "value": true}
                }
            ]
        }"#;
        let err = validate_constraint_draft(draft).expect_err("留痕型 set 应被拒绝");
        assert!(err.contains("enforce"), "报错应说明必须 enforce: {err}");
    }

    /// enforce 缺 reason 拒绝(无 reason 的拦截不可审计)
    #[test]
    fn test_validate_constraint_draft_rejects_missing_reason() {
        let draft = r#"{
            "transform": [
                {
                    "type": "enforce",
                    "params": {"domain": {"type": "instruction", "instruction_type": "robot_move"}}
                }
            ]
        }"#;
        let err = validate_constraint_draft(draft).expect_err("缺 reason 应被拒绝");
        assert!(err.contains("reason"), "报错应指明缺失字段: {err}");
    }

    /// 非法 JSON / 缺 transform / 空 transform 均拒绝
    #[test]
    fn test_validate_constraint_draft_rejects_malformed() {
        assert!(validate_constraint_draft("不是 JSON").is_err());
        assert!(validate_constraint_draft(r#"{"tier": "constraint"}"#).is_err());
        assert!(validate_constraint_draft(r#"{"transform": []}"#).is_err());
    }

    #[test]
    fn test_extract_keyed_value_tolerates_markdown_decorations() {
        let text = "step1 完成\n- **VERSION_ID=`01ABC`**\nTEST_CASE={\"motion\": \"forward\"}";
        assert_eq!(
            extract_keyed_value(text, "VERSION_ID").as_deref(),
            Some("01ABC")
        );
        assert_eq!(
            extract_keyed_value(text, "TEST_CASE").as_deref(),
            Some("{\"motion\": \"forward\"}")
        );
    }

    #[test]
    fn test_extract_keyed_value_missing() {
        assert!(extract_keyed_value("无产物", "VERSION_ID").is_none());
    }

    #[test]
    fn test_extract_keyed_value_tolerates_colon_separators() {
        // 真实 LLM 输出格式漂移:等号之外的冒号/全角冒号行式
        assert_eq!(
            extract_keyed_value("VERSION_ID: 01ABC", "VERSION_ID").as_deref(),
            Some("01ABC")
        );
        assert_eq!(
            extract_keyed_value("- **VERSION_ID：`01ABC`**", "VERSION_ID").as_deref(),
            Some("01ABC")
        );
        // TEST_CASE 的 JSON 值内含冒号,不应被冒号分隔误切
        let tc = "TEST_CASE:{\"probe\": true}";
        assert_eq!(
            extract_keyed_value(tc, "TEST_CASE").as_deref(),
            Some("{\"probe\": true}")
        );
    }

    #[test]
    fn test_extract_keyed_value_tolerates_numbered_prefix() {
        // 真实 LLM 按 prompt 的 a)/b)/c) 要求回填,编号前缀须被剥离
        // (实测失败形态:turn_a_text 明明含「b) VERSION_ID=…」却解析失败)
        assert_eq!(
            extract_keyed_value("b) VERSION_ID=01M3361CKBX9BYWX654PZ7Z3K1", "VERSION_ID")
                .as_deref(),
            Some("01M3361CKBX9BYWX654PZ7Z3K1")
        );
        assert_eq!(
            extract_keyed_value("1. VERSION_ID: 01ABC", "VERSION_ID").as_deref(),
            Some("01ABC")
        );
        assert_eq!(
            extract_keyed_value("(c) TEST_CASE={\"a\": \"b:c\"}", "TEST_CASE").as_deref(),
            Some("{\"a\": \"b:c\"}")
        );
        // 防误命中:单词粘连(key 前缀以字母数字/下划线结尾)不得命中
        assert_eq!(
            extract_keyed_value("MY_VERSION_ID=01ABC", "VERSION_ID"),
            None
        );
        assert_eq!(
            extract_keyed_value("text VERSION_ID=01ABC", "VERSION_ID"),
            None
        );
    }

    #[test]
    fn test_extract_keyed_value_trims_trailing_punctuation() {
        // 实测失败形态:LLM 照抄 prompt 列表分号,值带尾标点导致按 id 查版本 404
        assert_eq!(
            extract_keyed_value("b) VERSION_ID=01M3366Z21DS57NZR9DQ0NEBS2;", "VERSION_ID")
                .as_deref(),
            Some("01M3366Z21DS57NZR9DQ0NEBS2")
        );
        // JSON 值两端为花括号,标点清洗不得伤及内容
        assert_eq!(
            extract_keyed_value("TEST_CASE={\"a\": \"b\"};", "TEST_CASE").as_deref(),
            Some("{\"a\": \"b\"}")
        );
    }
}
