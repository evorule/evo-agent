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
    let agents_dir = &config.agents.dir;
    if !agents_dir.is_absolute() {
        // 相对路径 → 相对 workdir
    }
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
    let client = EvoruleApiClient::new(&config.evorule.base_url);
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
                Ok(AgentEvent::SessionCreated { session_id }) => {
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
    let agents_dir = config.agents.dir.clone();
    let definitions = AgentDefinitionManager::new(agents_dir);
    let evorule_client = EvoruleApiClient::new(&config.evorule.base_url);
    // E1:构造 workspace_client + union toolkit(启动时一次组装 26 个工具)
    let workspace_client = std::sync::Arc::new(WorkspaceApiClient::new(evorule_client.base_url()));
    let toolkit = std::sync::Arc::new(evo_agent::api::serve_tools::build_union_toolkit(
        workdir,
        &workspace_client,
        &evorule_client,
    ));
    eprintln!(
        "[tools] union toolkit assembled: {} tool(s) (6 builtin + 20 rule)",
        26 // 6 builtin + 20 rule
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
    );

    // G12:MCP 工具注册(P1 边界:只在 `run` 子命令生效)
    //
    // `serve` 模式下,每个 /agents/{type}/run 请求用 `AgentRunner::new` 构造 runner,
    // 其 tool_handler 为空(连 6 个内置工具都未注册 —— 这是 serve 路径的既有架构缺口,
    // 非 G12 引入)。MCP 适配器需要共享长生命周期的 McpClient(子进程),按请求 spawn
    // 代价过高。因此 P1 阶段 MCP 工具仅在 `evo-agent run` 中生效;serve 模式的工具
    // 架构改造(含 MCP + 内置工具)留待后续迭代。
    if !config.mcp.servers.is_empty() {
        eprintln!(
            "[mcp] {} server(s) configured, but MCP tools are only active in `evo-agent run` mode (P1)",
            config.mcp.servers.len()
        );
    }

    // 5. 构造 router + 中间件(G7 鉴权 + CORS + 1MB body limit)
    let app = agent_api::router_with_auth(state, auth_config)
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
    eprintln!("  GET  /health                  (no auth)");
    eprintln!("  GET  /metrics                 (no auth, Prometheus G17)");
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

    // 3.5 宪法 jsonschema 全量校验(M7-B2;找不到 schema 时降级为仅结构门卫,tracing 留痕)
    if let Err(violations) = evo_agent::agent::constitution::validate_workflow_dag(&wf_value) {
        eprintln!(
            "workflow '{}' violates constitution schema (workflow_dag/v1.0): {}",
            workflow_id,
            violations.join("; ")
        );
        return ExitCode::from(1);
    }

    let wf: Workflow = match serde_json::from_value(wf_value) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("failed to parse workflow '{}': {}", workflow_id, e);
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
    let client = EvoruleApiClient::new(&config.evorule.base_url);
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
    let result = runtime.block_on(engine.execute(&wf));

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

    let client = EvoruleApiClient::new(&config.evorule.base_url);

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
                let exit = run_repl_turn(
                    &runtime,
                    &config,
                    &def,
                    &client,
                    workdir,
                    auto_approve_candidates,
                    &mut current_session,
                    &session_file,
                    input,
                );
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

/// G15:执行一轮 REPL 对话
///
/// 首次输入(`current_session == None`):构造 runner → `run_streaming` → 捕获 session_id
/// 后续输入(`current_session == Some(id)`):构造 runner → `run_continuation(id, input)`
///
/// 返回 `Some(ExitCode)` 表示致命错误(应退出 REPL),`None` 表示继续下一轮。
fn run_repl_turn(
    runtime: &tokio::runtime::Runtime,
    config: &evo_agent::config::Config,
    def: &evo_agent::agent::definition::AgentDefinition,
    client: &EvoruleApiClient,
    workdir: &Path,
    auto_approve_candidates: bool,
    current_session: &mut Option<String>,
    session_file: &Path,
    input: String,
) -> Option<ExitCode> {
    use evo_agent::AgentEvent;
    use futures_util::StreamExt;
    use std::io::Write;

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
                Ok(AgentEvent::SessionCreated { session_id }) => {
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
    let client = EvoruleApiClient::new(&config.evorule.base_url);
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
        let mut e = MemoryEvent::new_root(id, EventType::EmotionEvent, 1000, EventSource::UserInput)
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
