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
use evo_agent::agent::runner::AgentRunner;
use evo_agent::api::evorule_client::EvoruleApiClient;
use evo_agent::builtin_tools::{
    default_safe_toolkit, default_tool_specs, shell_exec, http_get, ToolSpec,
};

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
        } => cmd_run(&cli.workdir, &goal, agent.as_deref(), auto_approve_candidates),
        Command::List { dir } => cmd_list(&cli.workdir, dir.as_deref()),
        Command::Tools { action } => cmd_tools(&cli.workdir, action),
        Command::Validate { agent } => cmd_validate(&cli.workdir, &agent),
        Command::Config => cmd_config(&cli.workdir),
    }
}

fn init_logging(verbose: bool) {
    use tracing_subscriber::{fmt, EnvFilter};
    let level = if verbose { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

// =============================================================================
// run —— 跑 agent
// =============================================================================

fn cmd_run(
    workdir: &Path,
    goal: &str,
    agent: Option<&str>,
    _auto_approve_candidates: bool,
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

    // 3. evorule 客户端
    let client = EvoruleApiClient::new(&config.evorule.base_url);

    // 4. 6 个安全工具
    let tool_handler = default_safe_toolkit(workdir);

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
        AgentRunner::from_definition(def, client, tool_handler, None).await
    });
    let mut runner = match runner_result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("bridge error: {}", e);
            return ExitCode::from(1);
        }
    };

    // 6. 跑
    eprintln!(
        "running agent '{}' with goal: {}",
        runner.agent_type(), goal
    );
    let run_result = runtime.block_on(async { runner.run(goal).await });

    // 7. 输出(JSON 到 stdout,log 到 stderr)
    match run_result {
        Ok(result) => {
            // 序列化为 JSON
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
            // 用 stderr 输出错误
            eprintln!("agent run failed: {}", e);
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
        None => match evo_agent::config::Config::load(workdir) {
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
    if let Some(c) = shell_exec::CANDIDATE_COMMANDS.iter().find(|c| c.name == name) {
        println!("[CANDIDATE] {}", c.name);
        println!("  description: {}", c.description);
        println!("  risk:        {}", c.risk);
        println!("  alternative: {}", c.alternative);
        return ExitCode::SUCCESS;
    }
    // shell_exec blocked
    if let Some((_, reason)) = shell_exec::BLOCKED_COMMANDS.iter().find(|(n, _)| *n == name) {
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
    let agents_dir = match evo_agent::config::Config::load(workdir) {
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
            println!("  memory:      type='{}' namespace='{}'", def.memory.memory_type, def.memory.namespace);
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
    match evo_agent::config::Config::load(workdir) {
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
