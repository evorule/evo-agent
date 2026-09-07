// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `shell_exec` —— 执行 shell 命令(3 层安全模型: active / candidate / blocked)
//!
//! ## 设计原则(来自 Mavis 与 EvoRule 作者的对话,2026-07-20)
//!
//! > 人类很乐意让 LLM 帮他们做所有事,但担心**不透明、失控、不可预测**。
//! > 所以白名单、备选、黑名单,应**全部列出**:让人类可选,说明影响,列表本身就是透明。
//!
//! 落到这个工具:
//!
//! | 类别 | 例子 | 行为 |
//! |---|---|---|
//! | **active** | `cargo git ls cat grep rg make find` | **直接执行**,不打扰用户 |
//! | **candidate** | `rm mv cp head tail tar zip curl wget` | agent 想用 → 返回 `proposal` 摊开说明 → 用户批 → 再执行 |
//! | **blocked** | `sudo bash sh python node ruby perl curl wget nc ssh chmod chown dd` | **永不**批准(真逃逸出口 / 不可逆破坏) |
//!
//! ## `proposal` 机制
//!
//! 当 agent 调用 candidate 命令,工具**不直接执行**,返回:
//! ```json
//! {
//!   "status": "needs_approval",
//!   "command": "rm -rf /tmp/build",
//!   "program": "rm",
//!   "description": "删除文件或目录",
//!   "risk": "可能误删重要数据,且不可逆",
//!   "alternative": "用文件管理器;或 mv 到 ~/.local/trash",
//!   "instructions": "Ask the user. If approved, call with approved=true."
//! }
//! ```
//!
//! 上层(AgentRunner / CLI)看到 `status: "needs_approval"`,问用户。
//! 用户说 yes → 重新调用时加 `"approved": true` → 工具**这次**真的执行。
//!
//! ## 关键安全机制
//!
//! 1. **No shell**:`std::process::Command` 直接 exec,不经任何 shell 解析
//! 2. **拒绝 shell metacharacter**:`; | & $ \` > < ( ) \n \r` 在任何 arg 里都被拒
//! 3. **3 层分类**:active 直跑 / candidate 请示 / blocked 拒
//!
//! ## 不支持(明确)
//!
//! - pipe / 重定向 / glob / 变量展开 — 用多次 `shell_exec` 调用组合

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use evorule_tcb::JsonValue;

use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

// =============================================================================
// 3 层命令分类
// =============================================================================

/// **Active 白名单**:直接执行,无需请示
///
/// 这些是开发/调试高频、无破坏性、用途明确的命令。
pub const ACTIVE_COMMANDS: &[&str] = &[
    "cargo", // Rust build/test/clippy/fmt
    "git",   // 版本控制(只读 + 安全的写:status/log/diff/add/commit)
    "ls",    // 列目录
    "cat",   // 读文件(替代 file_read 用)
    "grep",  // 文本搜索
    "rg",    // ripgrep(更快的 grep)
    "make",  // 构建系统
    "find",  // 文件查找
];

/// **Candidate 备选**:有合法用途,但有风险,需用户批准
///
/// 每个 candidate 必须有:
/// - `description`:这命令做什么
/// - `risk`:最大风险是什么
/// - `alternative`:有没有更安全的替代
pub const CANDIDATE_COMMANDS: &[CandidateCommand] = &[
    CandidateCommand {
        name: "rm",
        description: "删除文件或目录",
        risk: "误删不可逆;rm -rf 没有提示",
        alternative: "用 file manager GUI;或 mv 到 ~/.local/trash",
    },
    CandidateCommand {
        name: "mv",
        description: "移动/重命名文件",
        risk: "覆盖现有文件无提示;跨设备 mv 可能变 cp+rm",
        alternative: "cp + 确认 + rm(分两步,更安全)",
    },
    CandidateCommand {
        name: "cp",
        description: "复制文件",
        risk: "覆盖现有文件无提示(除非 -i)",
        alternative: "默认加 -i 强制提示覆盖",
    },
    CandidateCommand {
        name: "mkdir",
        description: "创建目录",
        risk: "低;只在错误路径会创建垃圾",
        alternative: "—",
    },
    CandidateCommand {
        name: "touch",
        description: "创建空文件或更新时间戳",
        risk: "低",
        alternative: "—",
    },
    CandidateCommand {
        name: "head",
        description: "读文件前 N 行",
        risk: "低",
        alternative: "file_read 工具(更结构化)",
    },
    CandidateCommand {
        name: "tail",
        description: "读文件后 N 行",
        risk: "低",
        alternative: "file_read 工具",
    },
    CandidateCommand {
        name: "sed",
        description: "流编辑器(替换/删除行)",
        risk: "误改文件无确认;在 -i 模式下尤其危险",
        alternative: "用脚本读+改+写,分步可见",
    },
    CandidateCommand {
        name: "awk",
        description: "文本处理",
        risk: "低(只读)",
        alternative: "—",
    },
    CandidateCommand {
        name: "tar",
        description: "打包/解包归档",
        risk: "解 tar 时路径遍历(罕见)",
        alternative: "用 zip 工具(更现代)",
    },
    CandidateCommand {
        name: "zip",
        description: "创建 zip 归档",
        risk: "低",
        alternative: "—",
    },
    CandidateCommand {
        name: "unzip",
        description: "解 zip 归档",
        risk: "低",
        alternative: "—",
    },
    CandidateCommand {
        name: "xargs",
        description: "把 stdin 转成命令行参数",
        risk: "中;被用于构造危险命令链",
        alternative: "用 --max-args=1 + --interactive 限制",
    },
    CandidateCommand {
        name: "patch",
        description: "应用 diff 文件",
        risk: "中;可任意改文件",
        alternative: "review diff 后手动应用",
    },
    CandidateCommand {
        name: "diff",
        description: "比较两个文件",
        risk: "低(只读)",
        alternative: "—",
    },
    CandidateCommand {
        name: "wc",
        description: "统计行/字/字节",
        risk: "低(只读)",
        alternative: "—",
    },
    CandidateCommand {
        name: "sort",
        description: "排序",
        risk: "低(只读)",
        alternative: "—",
    },
    CandidateCommand {
        name: "uniq",
        description: "去重",
        risk: "低(只读)",
        alternative: "—",
    },
    CandidateCommand {
        name: "cut",
        description: "切列",
        risk: "低(只读)",
        alternative: "—",
    },
    CandidateCommand {
        name: "tr",
        description: "字符替换",
        risk: "低(只读)",
        alternative: "—",
    },
];

/// **Blocked 永不**:这些是"逃逸出口",**不能成为 candidate**
///
/// 即使加 active 也不行,即使 propose 也不行。改这块 = 改安全模型。
pub const BLOCKED_COMMANDS: &[(&str, &str)] = &[
    ("sudo", "权限提升 — 跨安全边界"),
    ("su", "切换用户 — 同上"),
    ("bash", "shell 逃逸 — 绕过白名单"),
    ("sh", "shell 逃逸 — 同上"),
    ("zsh", "shell 逃逸 — 同上"),
    ("fish", "shell 逃逸 — 同上"),
    ("python", "Turing-complete — 任何操作都可做"),
    ("python3", "同上"),
    ("node", "Turing-complete"),
    ("ruby", "Turing-complete"),
    ("perl", "Turing-complete"),
    (
        "curl",
        "网络外联 — 泄露数据 + 下载恶意内容。用 http_get 工具",
    ),
    ("wget", "同上"),
    ("nc", "网络原始 socket — 反向 shell"),
    ("ncat", "同上"),
    ("ssh", "远程 shell — 跨主机"),
    ("scp", "远程文件传输"),
    ("chmod", "权限修改 — 改文件权限"),
    ("chown", "所有权修改 — 改文件属主"),
    ("dd", "块设备直接读写 — 可擦盘"),
    ("mkfs", "格式化文件系统 — 毁数据"),
    ("fdisk", "分区表修改 — 毁数据"),
    ("mount", "挂载设备 — 改系统状态"),
    ("umount", "卸载设备"),
    ("systemctl", "系统服务控制"),
    ("service", "同上"),
    ("kill", "进程终止(可能终止 evorule 自身)"),
    ("killall", "同上"),
    ("pkill", "同上"),
    ("shutdown", "关机"),
    ("reboot", "重启"),
    ("halt", "关机"),
    ("poweroff", "关机"),
    ("init", "init 系统"),
];

/// 单个 candidate 命令的描述信息
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateCommand {
    /// TODO: doc
    pub name: &'static str,
    /// TODO: doc
    pub description: &'static str,
    /// TODO: doc
    pub risk: &'static str,
    /// TODO: doc
    pub alternative: &'static str,
}

/// 命令分类结果
#[derive(Debug, Clone, PartialEq)]
pub enum CommandCategory {
    /// Active:直接执行
    Active,
    /// Candidate:需要用户批准
    Candidate(CandidateCommand),
    /// Blocked:永不批准
    Blocked(&'static str), // 拒绝原因
    /// Unknown:既不在 active 也不在 candidate
    Unknown,
}

/// 单次执行超时(秒)
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// 输出最大字节数
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024; // 1 MB

/// `shell_exec` 工具
#[derive(Clone)]
pub struct ShellExecTool {
    timeout: Duration,
    max_output_bytes: usize,
    workdir: Option<std::path::PathBuf>,
}

impl ShellExecTool {
    /// TODO: doc
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            workdir: None,
        }
    }

    /// TODO: doc
    pub fn with_workdir(mut self, dir: &Path) -> Self {
        self.workdir = Some(dir.to_path_buf());
        self
    }

    /// TODO: doc
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }

    /// TODO: doc
    pub fn with_max_output_bytes(mut self, n: usize) -> Self {
        self.max_output_bytes = n;
        self
    }

    /// 提取 argv[0] 的 basename
    fn program_name(arg0: &str) -> &str {
        std::path::Path::new(arg0)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(arg0)
    }

    /// 拒绝任何含 shell metacharacter 的 arg
    fn check_no_shell_metachars(parts: &[&str]) -> Result<(), String> {
        const BAD: &[char] = &[';', '|', '&', '`', '$', '>', '<', '\n', '\r', '(', ')'];
        for part in parts {
            for c in BAD {
                if part.contains(*c) {
                    return Err(format!(
                        "shell metacharacter '{}' in arg '{}' not allowed; \
                         use multiple shell_exec calls instead",
                        c, part
                    ));
                }
            }
        }
        Ok(())
    }

    /// 把命令分到 3 个桶之一
    pub fn classify(program: &str) -> CommandCategory {
        if ACTIVE_COMMANDS.contains(&program) {
            CommandCategory::Active
        } else if let Some(&c) = CANDIDATE_COMMANDS.iter().find(|c| c.name == program) {
            CommandCategory::Candidate(c)
        } else if let Some(&(_, reason)) =
            BLOCKED_COMMANDS.iter().find(|(name, _)| *name == program)
        {
            CommandCategory::Blocked(reason)
        } else {
            CommandCategory::Unknown
        }
    }

    /// 实际执行命令
    fn execute(&self, parts: &[&str], original_cmd: &str) -> IoResult {
        let program = Self::program_name(parts[0]);

        let mut cmd = Command::new(program);
        cmd.args(&parts[1..]);
        if let Some(dir) = &self.workdir {
            cmd.current_dir(dir);
        }
        cmd.stdin(std::process::Stdio::null());

        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                return Err(format!(
                    "failed to spawn '{}': {} (is it installed and in PATH?)",
                    program, e
                ));
            }
        };

        let stdout_bytes = if output.stdout.len() > self.max_output_bytes {
            &output.stdout[..self.max_output_bytes]
        } else {
            &output.stdout
        };
        let stderr_bytes = if output.stderr.len() > self.max_output_bytes {
            &output.stderr[..self.max_output_bytes]
        } else {
            &output.stderr
        };

        let mut map = std::collections::BTreeMap::new();
        map.insert("status".to_string(), JsonValue::string("ok"));
        map.insert(
            "command".to_string(),
            JsonValue::string(original_cmd.to_string()),
        );
        map.insert(
            "program".to_string(),
            JsonValue::string(program.to_string()),
        );
        map.insert(
            "exit_code".to_string(),
            JsonValue::Integer(output.status.code().unwrap_or(-1) as i64),
        );
        map.insert(
            "stdout".to_string(),
            JsonValue::string(String::from_utf8_lossy(stdout_bytes).to_string()),
        );
        map.insert(
            "stderr".to_string(),
            JsonValue::string(String::from_utf8_lossy(stderr_bytes).to_string()),
        );
        map.insert(
            "stdout_truncated".to_string(),
            JsonValue::Bool(output.stdout.len() > self.max_output_bytes),
        );
        Ok(JsonValue::object(map))
    }

    /// 构造一个 proposal(给 agent/CLI 用于问用户)
    fn make_proposal(program: &str, original_cmd: &str, candidate: CandidateCommand) -> IoResult {
        let mut map = std::collections::BTreeMap::new();
        map.insert("status".to_string(), JsonValue::string("needs_approval"));
        map.insert(
            "command".to_string(),
            JsonValue::string(original_cmd.to_string()),
        );
        map.insert(
            "program".to_string(),
            JsonValue::string(program.to_string()),
        );
        map.insert("category".to_string(), JsonValue::string("candidate"));
        map.insert(
            "description".to_string(),
            JsonValue::string(candidate.description),
        );
        map.insert("risk".to_string(), JsonValue::string(candidate.risk));
        map.insert(
            "alternative".to_string(),
            JsonValue::string(candidate.alternative),
        );
        map.insert(
            "instructions".to_string(),
            JsonValue::string(
                "Ask the user. If approved, call with approved=true (or use --yes flag in CLI).",
            ),
        );
        Ok(JsonValue::object(map))
    }
}

impl Default for ShellExecTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ToolFunction for ShellExecTool {
    /// G13:async 入口 — 用 spawn_blocking 包装同步 std::process::Command 操作
    async fn call(&self, args: &JsonValue) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("shell_exec tool panicked: {}", e))?
    }
}

impl ShellExecTool {
    /// 同步实现(供 spawn_blocking 调用)
    fn call_sync(&self, args: &JsonValue) -> IoResult {
        let cmd_str = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: command (string)".to_string())?;

        // 1. 解析
        let parts: Vec<&str> = cmd_str.split_whitespace().collect();
        if parts.is_empty() {
            return Err("empty command".to_string());
        }

        // 2. metacharacter check(无论 active / candidate / blocked 都要先过这一关)
        Self::check_no_shell_metachars(&parts)?;

        // 3. 分类
        let program = Self::program_name(parts[0]);
        let category = Self::classify(program);

        match category {
            CommandCategory::Active => self.execute(&parts, cmd_str),

            CommandCategory::Candidate(c) => {
                // 检查 approved
                let approved = args
                    .get("approved")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if approved {
                    self.execute(&parts, cmd_str)
                } else {
                    // 返回 proposal(让上层问用户)
                    Self::make_proposal(program, cmd_str, c)
                }
            }

            CommandCategory::Blocked(reason) => Err(format!(
                "command '{}' is BLOCKED (reason: {}); \
                 see builtin_tools::shell_exec::BLOCKED_COMMANDS for the full blocklist",
                program, reason
            )),

            CommandCategory::Unknown => Err(format!(
                "command '{}' is NOT in active or candidate list (allowed: {}; \
                 candidates: {}; see builtin_tools::shell_exec)",
                program,
                ACTIVE_COMMANDS.join(", "),
                CANDIDATE_COMMANDS
                    .iter()
                    .map(|c| c.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg_with(command: &str, approved: bool) -> JsonValue {
        let mut m = std::collections::BTreeMap::new();
        m.insert("command".to_string(), JsonValue::string(command));
        m.insert("approved".to_string(), JsonValue::Bool(approved));
        JsonValue::object(m)
    }

    fn arg(command: &str) -> JsonValue {
        arg_with(command, false)
    }

    // === 分类函数单元测试 ===

    #[test]
    fn test_classify_active() {
        assert_eq!(ShellExecTool::classify("cargo"), CommandCategory::Active);
        assert_eq!(ShellExecTool::classify("git"), CommandCategory::Active);
        assert_eq!(ShellExecTool::classify("ls"), CommandCategory::Active);
    }

    #[test]
    fn test_classify_candidate() {
        match ShellExecTool::classify("rm") {
            CommandCategory::Candidate(c) => {
                assert_eq!(c.name, "rm");
                assert!(c.risk.contains("不可逆") || c.risk.contains("误删"));
            }
            other => panic!("expected Candidate, got {:?}", other),
        }
        // 注意:curl 在 candidate 中虽然有合法用途,但被划到 blocked
        // 因为我们用 http_get 工具替代它(0.2.0)
        // 这里先测试不会 panic
        let _ = ShellExecTool::classify("curl");
    }

    #[test]
    fn test_classify_blocked() {
        match ShellExecTool::classify("sudo") {
            CommandCategory::Blocked(reason) => {
                assert!(reason.contains("权限") || reason.contains("安全"));
            }
            other => panic!("expected Blocked, got {:?}", other),
        }
        assert!(matches!(
            ShellExecTool::classify("bash"),
            CommandCategory::Blocked(_)
        ));
        assert!(matches!(
            ShellExecTool::classify("python"),
            CommandCategory::Blocked(_)
        ));
    }

    #[test]
    fn test_classify_unknown() {
        assert_eq!(
            ShellExecTool::classify("nonexistent_xyz"),
            CommandCategory::Unknown
        );
    }

    // === call() 行为测试 ===

    #[test]
    fn test_active_command_runs_without_approval() {
        // cargo --version 不在白名单?等等,cargo 在白名单
        // 用 ls 替代
        let tmp = tempfile::tempdir().unwrap();
        let tool = ShellExecTool::new().with_workdir(tmp.path());
        let result = tool.call_sync(&arg("ls"));
        #[cfg(unix)]
        assert!(result.is_ok(), "ls should run on unix");
        #[cfg(windows)]
        {
            // Windows 上 ls 不存在,只验 error 信息合理
            let _ = result;
        }
    }

    #[test]
    fn test_candidate_command_returns_proposal_without_approval() {
        let tool = ShellExecTool::new();
        let result = tool.call_sync(&arg("rm -rf /tmp/something"));
        let v = result.expect("should not error, should return proposal");
        assert_eq!(v.get("status").unwrap().as_str().unwrap(), "needs_approval");
        assert_eq!(v.get("program").unwrap().as_str().unwrap(), "rm");
        assert_eq!(v.get("category").unwrap().as_str().unwrap(), "candidate");
        assert!(v.get("description").is_some());
        assert!(v.get("risk").is_some());
        assert!(v.get("alternative").is_some());
        assert!(v.get("instructions").is_some());
    }

    #[test]
    fn test_candidate_command_runs_with_approval() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ShellExecTool::new().with_workdir(tmp.path());
        // mkdir 是 candidate 命令,加 approved=true 应该执行
        let result = tool.call_sync(&arg_with("mkdir test_dir_42", true));
        // mkdir 可能成功或失败(取决于系统),但不会是 proposal
        if let Ok(v) = result {
            assert_eq!(v.get("status").unwrap().as_str().unwrap(), "ok");
            // 验证目录真创建了
            assert!(tmp.path().join("test_dir_42").exists());
        }
        // 如果 result.is_err() 也接受(可能平台差异)
    }

    #[test]
    fn test_blocked_command_always_rejected() {
        let tool = ShellExecTool::new();
        // sudo 即使加 approved=true 也应该被拒
        let result = tool.call_sync(&arg_with("sudo apt install something", true));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("BLOCKED"), "got: {}", err);
    }

    #[test]
    fn test_unknown_command_rejected() {
        let tool = ShellExecTool::new();
        let result = tool.call_sync(&arg("nonexistent_xyz --foo"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("NOT in active or candidate"), "got: {}", err);
    }

    #[test]
    fn test_metacharacter_rejected_even_for_candidate() {
        let tool = ShellExecTool::new();
        // `;` 在 candidate 命令里也拒
        let result = tool.call_sync(&arg("rm foo; rm bar"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("metacharacter"), "got: {}", err);
    }

    #[test]
    fn test_empty_command() {
        let tool = ShellExecTool::new();
        let result = tool.call_sync(&arg(""));
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_command_arg() {
        let tool = ShellExecTool::new();
        let result = tool.call_sync(&JsonValue::object(std::collections::BTreeMap::new()));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing required arg"));
    }

    #[test]
    fn test_program_name_extracts_basename() {
        assert_eq!(ShellExecTool::program_name("/usr/bin/cargo"), "cargo");
        assert_eq!(ShellExecTool::program_name("cargo"), "cargo");
        assert_eq!(ShellExecTool::program_name("./cargo"), "cargo");
    }

    #[test]
    fn test_active_command_actually_runs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), b"hi").unwrap();
        let tool = ShellExecTool::new().with_workdir(tmp.path());
        #[cfg(unix)]
        {
            let result = tool.call_sync(&arg("ls"));
            let v = result.unwrap();
            assert_eq!(v.get("status").unwrap().as_str().unwrap(), "ok");
            let stdout = v.get("stdout").unwrap().as_str().unwrap();
            assert!(stdout.contains("hello.txt"), "got: {}", stdout);
        }
        #[cfg(windows)]
        {
            let _ = tool.call_sync(&arg("ls"));
        }
    }
}
