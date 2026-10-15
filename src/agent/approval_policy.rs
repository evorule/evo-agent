// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 审批策略条件文件与红线豁免清单（审批三档制封存基座，2026-09-26 设计/2026-10-15 实施）
//!
//! 职责边界（SEAL 红线：判定依据 serve 侧封存，agent 上下文只有只读副本）：
//! - 条件文件 `.evo/approval-policy.toml` 由 serve 侧独占读取——人类预置审批条件
//!   按「每工具组 conditions + policy_limits 通用限制」组织；加载失败 fail-visible：
//!   调用方必须将 policy 档降级 manual 并显式留痕，不得静默按无条件放行处理
//! - 红线豁免清单（默认强制人工审批）：命中任何一行的操作在任何审批档位下强制
//!   回落 manual；`approval.exemptionOverride=true`（用户显式关闭）可解除强制，
//!   但判定留痕必须加 `exemption_overridden` 标记（由决策端消费方落账，本模块
//!   只提供判定原语）；deny 规则（blocked）优先于一切配置，override 亦不可关闭

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// 单工具组的审批条件清单（人类预置）
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ToolGroupPolicy {
    /// 条件字符串清单（v1 语法：可机械核验的表达式；自然语言条件判定稳定性弱，不放开）
    #[serde(default)]
    pub conditions: Vec<String>,
}

/// 通用限制（`[policy_limits]`，所有 policy 工具共享）
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PolicyLimits {
    /// 修正重查上限（同一操作 fail 后重查次数，超限降级 manual——防无限循环）
    #[serde(default = "default_max_corrections")]
    pub max_corrections: u32,
    /// 单次任务连续自动操作上限（超限暂停等人——Runaway 护栏）
    #[serde(default = "default_max_actions_per_run")]
    pub max_actions_per_run: u32,
}

fn default_max_corrections() -> u32 {
    3
}

fn default_max_actions_per_run() -> u32 {
    200
}

impl Default for PolicyLimits {
    fn default() -> Self {
        Self {
            max_corrections: default_max_corrections(),
            max_actions_per_run: default_max_actions_per_run(),
        }
    }
}

/// 条件文件整体（顶层动态工具组表 + 可选 policy_limits）
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct ApprovalPolicyFile {
    /// 动态工具组（键=工具名或组名，如 shell_exec）
    #[serde(flatten)]
    pub groups: BTreeMap<String, ToolGroupPolicy>,
    /// 通用限制（缺省 3 次/200 次）
    #[serde(default)]
    pub policy_limits: PolicyLimits,
}

impl ApprovalPolicyFile {
    /// 从给定路径加载条件文件（serve 侧封存入口）
    ///
    /// fail-visible：文件不存在返回 [`PolicyFileError::NotFound`]（调用方视为
    /// 「未预置条件」——policy 档同样降级 manual，因为空条件清单无法评审）；
    /// 解析/schema 失败返回对应错误并必须显式留痕。
    pub fn load(path: &Path) -> Result<Self, PolicyFileError> {
        let raw = std::fs::read_to_string(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => PolicyFileError::NotFound(e),
            _ => PolicyFileError::Io(e),
        })?;
        Self::from_toml(&raw)
    }

    /// 从 TOML 文本解析（校验入口，测试与试判定共用）
    pub fn from_toml(raw: &str) -> Result<Self, PolicyFileError> {
        let parsed: Self =
            toml::from_str(raw).map_err(|e| PolicyFileError::Schema(e.to_string()))?;
        Ok(parsed)
    }

    /// 取指定工具组的条件清单（无该组返回空切片）
    pub fn conditions_for(&self, tool: &str) -> &[String] {
        self.groups
            .get(tool)
            .map(|g| g.conditions.as_slice())
            .unwrap_or(&[])
    }
}

/// 条件文件加载错误（fail-visible 语义，禁止调用方静默吞掉）
#[derive(Debug)]
pub enum PolicyFileError {
    /// 文件不存在（未预置条件——policy 档降级 manual）
    NotFound(std::io::Error),
    /// 读失败（权限等 I/O 错误）
    Io(std::io::Error),
    /// TOML 解析或 schema 校验失败
    Schema(String),
}

impl fmt::Display for PolicyFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(e) => write!(f, "approval policy file not found: {e}"),
            Self::Io(e) => write!(f, "approval policy file read failed: {e}"),
            Self::Schema(e) => write!(f, "approval policy file invalid: {e}"),
        }
    }
}

impl std::error::Error for PolicyFileError {}

/// 红线豁免类别（默认强制人工审批的操作面）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedLine {
    /// 不可逆删除（file_delete 含批量——.evo-trash 兜底仍在，但自删+自审批叠加风险不可接受）
    IrreversibleDelete,
    /// 推送远端 / force push（影响超出本地工作区）
    RemotePush,
    /// 数据外发（网络外发命令 / http_get 抓取——数据出境红线）
    DataExfiltration,
    /// 改治理配置文件本身（自我扩权防护——条件文件/设置文件任何档位下必经人工）
    GovernanceConfigWrite,
}

impl RedLine {
    /// 审计留痕标记名（决策端落账用）
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::IrreversibleDelete => "red_line.delete",
            Self::RemotePush => "red_line.remote_push",
            Self::DataExfiltration => "red_line.data_exfiltration",
            Self::GovernanceConfigWrite => "red_line.governance_config_write",
        }
    }
}

/// 网络外发命令词法名单（v1 机械口径：宁可误报多一次人工审批，不可漏报自动放行）
const EXFIL_COMMANDS: &[&str] = &[
    "curl",
    "wget",
    "nc",
    "ncat",
    "netcat",
    "ssh",
    "scp",
    "sftp",
    "ftp",
    "telnet",
    "rsync",
    "invoke-webrequest",
    "invoke-restmethod",
    "iwr",
    "irm",
];

/// 治理配置文件路径匹配（\.evo\ 审批条件/设置文件——agent 改写即自我扩权）
fn is_governance_config_path(raw: &str) -> bool {
    let norm = raw.replace('\\', "/").to_ascii_lowercase();
    norm.contains(".evo/approval-policy.toml") || norm.contains(".evo/settings.json")
}

/// 从操作参数中收集全部叶子字符串（递归——覆盖批量路径数组等形状）
fn collect_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => {
            for it in items {
                collect_strings(it, out);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_strings(v, out);
            }
        }
        _ => {}
    }
}

/// shell 命令中是否命中外发命令词（按词匹配可执行名，含管道/串联后的段）
fn command_hits_exfil(command: &str) -> bool {
    command
        .split(|c: char| c.is_whitespace() || matches!(c, '|' | ';' | '&' | '(' | ')'))
        .filter(|w| !w.is_empty())
        .any(|w| {
            let name = w
                .trim_matches(|c: char| matches!(c, '"' | '\''))
                .to_ascii_lowercase();
            let base = name.rsplit(['/', '\\']).next().unwrap_or(&name);
            EXFIL_COMMANDS.contains(&base)
        })
}

/// shell 命令中是否命中推送远端模式（git push 含 force push）
fn command_hits_push(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains("git") && lower.split_whitespace().any(|w| w == "push")
}

/// 红线豁免判定原语：操作是否命中默认强制人工审批清单
///
/// 纯函数、零 IO；命中即「任何审批档位下强制回落 manual」（deny 优先级更高，
/// 由 manifest blocked 分支先行拒绝，不经本函数）。`approval.exemptionOverride`
/// 的解除语义由决策端消费方实现——本函数恒返回客观命中结果。
pub fn hits_red_line(tool: &str, args: &serde_json::Value) -> Option<RedLine> {
    match tool {
        "file_delete" => return Some(RedLine::IrreversibleDelete),
        "http_get" => return Some(RedLine::DataExfiltration),
        "shell_exec" => {
            let mut strings = Vec::new();
            collect_strings(args, &mut strings);
            if strings.iter().any(|s| command_hits_exfil(s)) {
                return Some(RedLine::DataExfiltration);
            }
            if strings.iter().any(|s| command_hits_push(s)) {
                return Some(RedLine::RemotePush);
            }
        }
        "file_write" | "file_create" | "file_move" | "file_edit" | "write_file" => {
            let mut strings = Vec::new();
            collect_strings(args, &mut strings);
            if strings.iter().any(|s| is_governance_config_path(s)) {
                return Some(RedLine::GovernanceConfigWrite);
            }
        }
        _ => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SAMPLE: &str = r#"
[shell_exec]
conditions = [
  "command 匹配白名单模式 ^cargo|^git status|^git diff",
  "仅工作区内路径"
]
[file_read]
conditions = ["仅工作区内路径"]
[policy_limits]
max_corrections = 3
max_actions_per_run = 200
"#;

    #[test]
    fn parse_full_sample() {
        let f = ApprovalPolicyFile::from_toml(SAMPLE).unwrap();
        assert_eq!(f.groups.len(), 2);
        assert_eq!(f.conditions_for("shell_exec").len(), 2);
        assert_eq!(f.conditions_for("file_read").len(), 1);
        assert_eq!(f.conditions_for("absent"), &[] as &[String]);
        assert_eq!(f.policy_limits.max_corrections, 3);
        assert_eq!(f.policy_limits.max_actions_per_run, 200);
    }

    #[test]
    fn limits_default_when_absent() {
        let f = ApprovalPolicyFile::from_toml("[shell_exec]\nconditions = [\"x\"]").unwrap();
        assert_eq!(f.policy_limits, PolicyLimits::default());
        assert_eq!(f.policy_limits.max_corrections, 3);
        assert_eq!(f.policy_limits.max_actions_per_run, 200);
    }

    #[test]
    fn bad_toml_is_visible_error() {
        let err = ApprovalPolicyFile::from_toml("not [valid toml").unwrap_err();
        assert!(matches!(err, PolicyFileError::Schema(_)));
        assert!(err.to_string().contains("invalid"));
    }

    #[test]
    fn load_missing_file_reports_not_found() {
        let err = ApprovalPolicyFile::load(Path::new("Z:/definitely/absent/x.toml")).unwrap_err();
        assert!(matches!(err, PolicyFileError::NotFound(_)));
    }

    #[test]
    fn red_line_delete_always_hits() {
        assert_eq!(
            hits_red_line("file_delete", &json!({"path": "a.txt"})),
            Some(RedLine::IrreversibleDelete)
        );
    }

    #[test]
    fn red_line_http_get_is_exfiltration() {
        assert_eq!(
            hits_red_line("http_get", &json!({"url": "https://example.com"})),
            Some(RedLine::DataExfiltration)
        );
    }

    #[test]
    fn red_line_shell_exfil_commands() {
        assert_eq!(
            hits_red_line(
                "shell_exec",
                &json!({"command": "curl https://evil.example"})
            ),
            Some(RedLine::DataExfiltration)
        );
        assert_eq!(
            hits_red_line(
                "shell_exec",
                &json!({"command": "echo hi | wget -q http://x"})
            ),
            Some(RedLine::DataExfiltration)
        );
        // 全路径形态的可执行名同样命中
        assert_eq!(
            hits_red_line("shell_exec", &json!({"command": "/usr/bin/nc -l 8080"})),
            Some(RedLine::DataExfiltration)
        );
    }

    #[test]
    fn red_line_shell_push() {
        assert_eq!(
            hits_red_line("shell_exec", &json!({"command": "git push origin main"})),
            Some(RedLine::RemotePush)
        );
        assert_eq!(
            hits_red_line(
                "shell_exec",
                &json!({"command": "git push --force origin main"})
            ),
            Some(RedLine::RemotePush)
        );
        // 非推送的 git 操作不命中
        assert_eq!(
            hits_red_line("shell_exec", &json!({"command": "git status"})),
            None
        );
        assert_eq!(
            hits_red_line("shell_exec", &json!({"command": "cargo build"})),
            None
        );
    }

    #[test]
    fn red_line_governance_config_write() {
        assert_eq!(
            hits_red_line(
                "file_write",
                &json!({"path": ".evo/approval-policy.toml", "content": "x"})
            ),
            Some(RedLine::GovernanceConfigWrite)
        );
        // Windows 反斜杠路径归一后同样命中
        assert_eq!(
            hits_red_line(
                "file_create",
                &json!({"path": "D:\\work\\.evo\\settings.json"})
            ),
            Some(RedLine::GovernanceConfigWrite)
        );
        // 普通文件写不命中
        assert_eq!(
            hits_red_line("file_write", &json!({"path": "src/main.rs"})),
            None
        );
    }

    #[test]
    fn red_line_unrelated_tools_pass() {
        assert_eq!(hits_red_line("file_read", &json!({"path": "a.rs"})), None);
        assert_eq!(hits_red_line("grep_files", &json!({"pattern": "x"})), None);
    }

    #[test]
    fn red_line_marker_names_stable() {
        assert_eq!(RedLine::IrreversibleDelete.as_str(), "red_line.delete");
        assert_eq!(RedLine::RemotePush.as_str(), "red_line.remote_push");
        assert_eq!(
            RedLine::DataExfiltration.as_str(),
            "red_line.data_exfiltration"
        );
        assert_eq!(
            RedLine::GovernanceConfigWrite.as_str(),
            "red_line.governance_config_write"
        );
    }
}
