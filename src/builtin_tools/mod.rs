// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 6 个内置工具(0.1.0:file_read / file_list / file_write / search_files / shell_exec / http_get)
//!
//! ## 设计原则(来自 Mavis 与 EvoRule 作者的对话,2026-07-20)
//!
//! > 人类很乐意让 LLM 帮他们做所有事,但担心**不透明、失控、不可预测**。
//! > 所以白名单、备选、黑名单,应**全部列出**:让人类可选,说明影响,列表本身就是透明。
//!
//! 3 层模型(每个工具都遵守):
//!
//! | 类别 | 行为 |
//! |---|---|
//! | **active** | 白名单,直接执行,无需请示 |
//! | **candidate** | 备选,agent 想用 → 返回 proposal 摊开说明 → 用户批 → 再执行 |
//! | **blocked** | 永不批准(逃逸出口 / 不可逆破坏) |
//!
//! ## 通用安全机制
//!
//! 1. **白名单 + 默认 deny**:`shell_exec` 只允许列出的命令;`http_get` 只允许列出的 host
//! 2. **工作目录沙箱**:`file_*` / `search_files` 所有路径必须 relative to workdir
//!    - 拒绝绝对路径
//!    - 拒绝 `..` 路径段
//!    - canonicalize 后必须仍在 workdir 内(防 symlink 逃逸)
//! 3. **资源限制**:size limit / max_results / timeout
//! 4. **No shell**:`shell_exec` 走 `std::process::Command` 直接 exec,不经任何 shell 解析
//! 5. **SSRF 防护**:`http_get` 硬编码黑名单(localhost/内网/cloud metadata)
//!
//! ## 修改这些工具 = 修改 evo-agent 的安全模型
//!
//! 改之前要明确:
//! - 增加了什么能力?
//! - 谁会受影响?(圈 2 合规用户)
//! - 能否绕过白名单/沙箱?
//!
//! 默认是"宁可功能少,也不可被滥用"。

pub mod file_list;
pub mod file_read;
pub mod file_write;
pub mod http_get;
pub mod search_files;
pub mod shell_exec;

use std::path::Path;
use std::sync::Arc;

use crate::io_handlers::tool_handler::ToolHandler;

/// 构造一个"安全默认工具集"
///
/// 包含 6 个工具(0.1.0 阶段):
/// - `file_read`:读文件(工作目录沙箱 + size limit)
/// - `file_list`:列目录(工作目录沙箱 + 跳过隐藏)
/// - `file_write`:写文件(只能写 `./workspace/` + overwrite 保护)
/// - `search_files`:glob 找文件(工作目录沙箱 + max_results 限制)
/// - `shell_exec`:执行白名单命令(8 active + 20 candidate + 28 blocked)
/// - `http_get`:HTTP GET(6 active host + SSRF 防护 + 任何其他 host 需批准)
///
/// 用法:
/// ```ignore
/// let tool_handler = builtin_tools::default_safe_toolkit(Path::new("."));
/// let runner = AgentRunner::new(config, client).with_tool_handler(tool_handler);
/// ```
pub fn default_safe_toolkit(workdir: &Path) -> ToolHandler {
    let workdir_buf = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());

    let mut handler = ToolHandler::new();
    handler.register_tool(
        "file_read",
        Arc::new(file_read::FileReadTool::new(workdir_buf.clone())),
    );
    handler.register_tool(
        "file_list",
        Arc::new(file_list::FileListTool::new(workdir_buf.clone())),
    );
    handler.register_tool(
        "file_write",
        Arc::new(file_write::FileWriteTool::new(workdir_buf.clone())),
    );
    handler.register_tool(
        "search_files",
        Arc::new(search_files::SearchFilesTool::new(workdir_buf.clone())),
    );
    handler.register_tool(
        "shell_exec",
        Arc::new(shell_exec::ShellExecTool::new().with_workdir(&workdir_buf)),
    );
    handler.register_tool("http_get", Arc::new(http_get::HttpGetTool::new()));
    handler
}

/// 列出默认工具集中所有工具的 spec(给 LLM 看)
pub fn default_tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "file_read".to_string(),
            description: "Read a text file's content. Path is relative to workdir; \
                          absolute paths and `..` are rejected. Files larger than 10MB are rejected."
                .to_string(),
            parameters: vec![ParameterSpec {
                name: "path".to_string(),
                r#type: "string".to_string(),
                description: "File path, relative to workdir (e.g. \"src/main.rs\")".to_string(),
                required: true,
            }],
        },
        ToolSpec {
            name: "file_list".to_string(),
            description: "List directory entries. Path is relative to workdir; \
                          absolute paths and `..` are rejected. \
                          Hidden files (starting with `.`) are skipped by default."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "dir".to_string(),
                    r#type: "string".to_string(),
                    description: "Subdirectory to list, relative to workdir (default: \".\")".to_string(),
                    required: false,
                },
                ParameterSpec {
                    name: "include_hidden".to_string(),
                    r#type: "boolean".to_string(),
                    description: "Set true to include hidden files (default: false)".to_string(),
                    required: false,
                },
                ParameterSpec {
                    name: "max_entries".to_string(),
                    r#type: "integer".to_string(),
                    description: "Max number of entries (default 1000)".to_string(),
                    required: false,
                },
            ],
        },
        ToolSpec {
            name: "file_write".to_string(),
            description: "Write content to a file in the writable subdir. \
                          **ONLY writes to ./workspace/** (configurable). \
                          Cannot overwrite existing files unless `overwrite=true`. \
                          Cannot create parent dirs unless `create_parents=true`. \
                          Max content size 1MB."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "path".to_string(),
                    r#type: "string".to_string(),
                    description: "File path, relative to writable_dir (e.g. \"workspace/notes.md\")".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "content".to_string(),
                    r#type: "string".to_string(),
                    description: "File content to write".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "overwrite".to_string(),
                    r#type: "boolean".to_string(),
                    description: "Set true to overwrite existing file (default: false)".to_string(),
                    required: false,
                },
                ParameterSpec {
                    name: "create_parents".to_string(),
                    r#type: "boolean".to_string(),
                    description: "Set true to auto-create parent directories (default: false)".to_string(),
                    required: false,
                },
            ],
        },
        ToolSpec {
            name: "search_files".to_string(),
            description: "Find files by glob pattern in workdir. \
                          Supports `*` (any chars) and `?` (single char). \
                          Hidden files (starting with `.`) are skipped. \
                          Max results capped to prevent OOM."
                .to_string(),
            parameters: vec![
                ParameterSpec {
                    name: "pattern".to_string(),
                    r#type: "string".to_string(),
                    description: "Glob pattern (e.g. \"*.rs\", \"test_*.py\")".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "dir".to_string(),
                    r#type: "string".to_string(),
                    description: "Subdirectory to search in, relative to workdir (default: \".\")".to_string(),
                    required: false,
                },
                ParameterSpec {
                    name: "max_results".to_string(),
                    r#type: "integer".to_string(),
                    description: "Max number of results (default 1000)".to_string(),
                    required: false,
                },
            ],
        },
        ToolSpec {
            name: "shell_exec".to_string(),
            description: shell_exec_description(),
            parameters: vec![
                ParameterSpec {
                    name: "command".to_string(),
                    r#type: "string".to_string(),
                    description: "Command to execute, e.g. \"cargo test --release\" or \"ls -la\"".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "approved".to_string(),
                    r#type: "boolean".to_string(),
                    description: "Set to true ONLY after the user has approved a candidate-command proposal. \
                                  Active commands ignore this flag. Blocked commands reject regardless."
                        .to_string(),
                    required: false,
                },
            ],
        },
        ToolSpec {
            name: "http_get".to_string(),
            description: http_get_description(),
            parameters: vec![
                ParameterSpec {
                    name: "url".to_string(),
                    r#type: "string".to_string(),
                    description: "HTTPS URL to GET. Must be in active allowlist OR approved by user.".to_string(),
                    required: true,
                },
                ParameterSpec {
                    name: "approved".to_string(),
                    r#type: "boolean".to_string(),
                    description: "Set to true ONLY after the user has approved a candidate-host proposal. \
                                  Blocked hosts (private IP, localhost) reject regardless."
                        .to_string(),
                    required: false,
                },
            ],
        },
    ]
}

/// 构造 shell_exec 的 description(给 LLM 看)
fn shell_exec_description() -> String {
    use crate::builtin_tools::shell_exec::{ACTIVE_COMMANDS, BLOCKED_COMMANDS, CANDIDATE_COMMANDS};

    let active: Vec<String> = ACTIVE_COMMANDS.iter().map(|s| format!("`{}`", s)).collect();
    let candidates: Vec<String> = CANDIDATE_COMMANDS
        .iter()
        .map(|c| format!("`{}` ({}: {})", c.name, c.description, c.risk))
        .collect();
    let blocked: Vec<String> = BLOCKED_COMMANDS
        .iter()
        .map(|(name, reason)| format!("`{}` ({})", name, reason))
        .collect();

    format!(
        "Execute a shell command with a 3-layer safety model.\n\n\
         **Active (run directly, no approval needed):** {}\n\n\
         **Candidate (require user approval via approved=true):** {}\n\n\
         **Blocked (always rejected, no candidate path):** {}\n\n\
         **Rules:**\n\
         - No shell — args passed directly (no pipe, no redirect, no glob expansion)\n\
         - Shell metacharacters rejected: ; | & $ ` > < ( )\n\
         - To use a candidate command: FIRST show user the proposal (status=needs_approval), \
         THEN call again with approved=true after they confirm",
        active.join(", "),
        candidates.join("; "),
        blocked.join("; "),
    )
}

/// 构造 http_get 的 description(给 LLM 看)
fn http_get_description() -> String {
    use crate::builtin_tools::http_get::ACTIVE_HOSTS;

    let active: Vec<String> = ACTIVE_HOSTS.iter().map(|h| format!("`{}`", h)).collect();

    format!(
        "HTTP GET with a 3-layer host safety model.\n\n\
         **Active (request directly, no approval needed):** {}\n\n\
         **Candidate (require user approval via approved=true):** any other public host\n\n\
         **Blocked (ALWAYS rejected, even with approved=true):**\n\
         - `http://` (only `https://` allowed by default)\n\
         - `127.0.0.0/8`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16` (private IPs / SSRF)\n\
         - `169.254.0.0/16` (link-local, **especially cloud metadata 169.254.169.254**)\n\
         - IPv6 `::1`, `fe80::/10`, `fc00::/7`\n\n\
         **Other limits:** timeout 10s, max response 1MB, max 3 redirects",
        active.join(", "),
    )
}

/// 工具 spec(给 LLM 看,跟 ToolRegistry 里的 spec 兼容)
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// TODO: doc
    pub name: String,
    /// TODO: doc
    pub description: String,
    /// TODO: doc
    pub parameters: Vec<ParameterSpec>,
}

#[derive(Debug, Clone)]
/// TODO: doc
pub struct ParameterSpec {
    /// TODO: doc
    pub name: String,
    /// TODO: doc
    pub r#type: String,
    /// TODO: doc
    pub description: String,
    /// TODO: doc
    pub required: bool,
}
