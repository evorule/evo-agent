// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! MCP server 拉起装载门（来源认证面）
//!
//! ## 威胁面
//!
//! mcp.toml 位于工作区（agent 会话可写），被篡改 = 拉起任意二进制（绕过全部
//! 规则治理——server 本体进程不受工具调用治理约束）；且子进程缺省继承父进程
//! 全量环境变量（凭据整体外泄）。
//!
//! ## 两道防线
//!
//! 1. **命令装载门**（本模块）：stdio server 拉起前校验 command+args 是否在
//!    用户级白名单（`.evorule/mcp-allowlist.json`，位于用户主目录——与工作区
//!    物理分离，会话写不到）。缺省 enforce（未登记=拒载）；`warn`=显式迁移档
//!    （放行+留痕）。
//! 2. **最小环境**（transport）：子进程环境 = 最小安全集 + mcp.toml env 显式
//!    增量（env_clear 后注入），凭据外泄面从「全量继承」收窄为「显式声明项」。
//!
//! ## 语义裁决（与内核约束前置门同构）
//!
//! 安全校验必须在**拉起之前**独立完成（前置门），不参与与连接逻辑的执行竞争；
//! 解析规则与匹配规则均为纯函数（不触 env），可无耦合单测。
//!
//! ## 诚实边界
//!
//! 白名单防的是误配与经 mcp.toml 一点的投毒路径；本机同 OS 用户恶意进程不在
//! 防御范围（强隔离=容器/独立用户，属部署形态议题）。白名单粒度为 server 级
//! （server 内部行为边界=server 质量问题；建议登记指向自审/自建的 server）。

use serde::{Deserialize, Serialize};

/// 裸解释器名清单：这类 command 是包管理器/运行时（npx 拉任意包=任意代码执行），
/// **不允许**以无 args_pattern 的形态进入白名单（否则白名单形同虚设）。
const BARE_INTERPRETERS: &[&str] = &["npx", "uvx", "node", "python", "python3", "deno", "bun"];

/// MCP server 子进程的最小安全环境键集
///
/// 仅系统运行必需键（进程创建/shell 解析/临时目录/本地化）；业务键与凭据键
/// 必须经 mcp.toml env 显式声明。清单入代码可审（对齐内核终止性防线的
/// 「每维上界显式化」哲学）。
const MINIMAL_SPAWN_ENV_KEYS: &[&str] = &[
    // 跨平台：进程拉起与运行
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "TERM",
    "TMPDIR",
    // Windows：进程创建与 shell 解析必需
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMFILES",
    "PROGRAMDATA",
    "HOMEDRIVE",
    "HOMEPATH",
    "TMP",
    "TEMP",
    "ALLUSERSPROFILE",
];

/// 从父进程环境中提取最小安全集并合并显式增量（纯函数，可测）
///
/// `parent`=父进程全量环境；`extra`=mcp.toml env 显式声明（覆盖最小集同名键）。
pub(crate) fn build_minimal_env(
    parent: &std::collections::HashMap<String, String>,
    extra: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for key in MINIMAL_SPAWN_ENV_KEYS {
        if let Some(v) = parent.get(*key) {
            out.insert((*key).to_string(), v.clone());
        }
    }
    for (k, v) in extra {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// 从当前进程环境构建 MCP server 子进程环境（最小安全集 + extra 增量）
pub(crate) fn minimal_spawn_env(
    extra: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    let parent: std::collections::HashMap<String, String> = std::env::vars().collect();
    build_minimal_env(&parent, extra)
}

/// 白名单条目
///
/// `comment` 必填（谁为何登记此条目的最小审计——不允许无归因的放行）；
/// `args_pattern` 缺省 = 仅匹配 command（适用于固定二进制）；给出 = args 须
/// 前缀匹配该段序列（适用于 npx/uvx 等包管理器拉起，把包名一并锁死；
/// 段内支持尾缀 `*` 通配）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllowlistEntry {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_pattern: Option<Vec<String>>,
    #[serde(default)]
    pub comment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<String>,
}

/// 用户级 spawn 白名单（批准清单）
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SpawnAllowlist {
    #[serde(default)]
    pub allow: Vec<AllowlistEntry>,
}

impl SpawnAllowlist {
    /// 白名单文件路径：env `EVORULE_MCP_ALLOWLIST` 覆盖，缺省用户主目录下
    /// `.evorule/mcp-allowlist.json`（用户级配置，与项目工作区物理分离——
    /// 会话写得到 mcp.toml，写不到这里）。
    pub fn default_path() -> std::path::PathBuf {
        if let Ok(p) = std::env::var("EVORULE_MCP_ALLOWLIST") {
            return p.into();
        }
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default();
        std::path::Path::new(&home)
            .join(".evorule")
            .join("mcp-allowlist.json")
    }

    /// 读取白名单：文件缺失 = 空名单（enforce 下全部 stdio 拒载，错误信息
    /// 携带指引）；文件存在但解析失败 = **显式错误**（配置损坏必须大声，
    /// 不静默当空名单）。
    pub fn load() -> Result<Self, String> {
        let path = Self::default_path();
        match std::fs::read_to_string(&path) {
            Ok(raw) => Self::parse(&path.display().to_string(), &raw),
            Err(_) => {
                tracing::warn!(
                    path = %path.display(),
                    "MCP spawn allowlist 未找到（enforce 下全部 stdio server 拒载；\
                     运营方请创建该文件登记批准的 server，或显式切换 warn 迁移档）"
                );
                Ok(Self::default())
            }
        }
    }

    /// 解析白名单文本（严格：comment 必填；裸解释器必须带 args_pattern）
    pub fn parse(path_label: &str, raw: &str) -> Result<Self, String> {
        let list: SpawnAllowlist = serde_json::from_str(raw)
            .map_err(|e| format!("MCP spawn 白名单 {path_label} 非法 JSON: {e}"))?;
        for e in &list.allow {
            if e.comment.trim().is_empty() {
                return Err(format!(
                    "MCP spawn 白名单 {path_label} 条目 `{}` 缺 comment（登记必须带归因）",
                    e.command
                ));
            }
            let bare = BARE_INTERPRETERS
                .iter()
                .any(|i| e.command.eq_ignore_ascii_case(i));
            if bare && e.args_pattern.is_none() {
                return Err(format!(
                    "MCP spawn 白名单条目 `{}`：裸解释器必须登记 args_pattern \
                     （锁死包名/脚本参数，否则白名单形同虚设）",
                    e.command
                ));
            }
        }
        Ok(list)
    }

    /// 条目匹配：command 字面精确 + args 前缀逐段匹配（段内支持尾缀 `*` 通配）
    pub fn matches(&self, command: &str, args: &[String]) -> bool {
        self.allow.iter().any(|e| {
            if e.command != command {
                return false;
            }
            match &e.args_pattern {
                None => true,
                Some(pat) => {
                    args.len() >= pat.len()
                        && pat.iter().zip(args.iter()).all(|(p, a)| {
                            p.ends_with('*') && a.starts_with(p.trim_end_matches('*')) || p == a
                        })
                }
            }
        })
    }
}

/// 拉起策略（`EVORULE_MCP_SPAWN_POLICY`）：`enforce`（缺省，未登记=拒载）/
/// `warn`（显式迁移档：放行+warn 留痕）。未知值按 enforce 处理（fail-closed）。
/// 测试编译面缺省 warn：既有 MCP 测试语料未登记白名单，enforce 语义由纯函数
/// 测试专测（显式传档位，不依赖 env）。
pub(crate) fn spawn_policy() -> String {
    static POLICY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    POLICY
        .get_or_init(|| {
            if cfg!(test) {
                return "warn".to_string();
            }
            match std::env::var("EVORULE_MCP_SPAWN_POLICY").as_deref() {
                Ok("warn") => "warn".to_string(),
                _ => "enforce".to_string(),
            }
        })
        .clone()
}

/// 单 server 装载门总入口（调用方一行接入）：加载白名单 → 按 env 策略判定。
/// 白名单文件解析失败 = 两种档位下均连接失败（配置损坏必须大声）。
pub(crate) fn ensure_spawn_allowed(
    server_name: &str,
    command: &str,
    args: &[String],
) -> Result<(), String> {
    let policy = spawn_policy();
    let allowlist = SpawnAllowlist::load()?;
    gate(server_name, command, args, &policy, Some(&allowlist))
}

/// 装载门判定（单 server）：Ok=放行；Err=拒载（消息含原因与处置指引）。
/// 每次判定发结构化事件（治理事实挂点：server/command/args/判定/理由）。
pub(crate) fn gate(
    server_name: &str,
    command: &str,
    args: &[String],
    policy: &str,
    allowlist: Option<&SpawnAllowlist>,
) -> Result<(), String> {
    let allowed = allowlist
        .map(|list| list.matches(command, args))
        .unwrap_or(false);
    let args_json = serde_json::to_string(args).unwrap_or_default();
    if allowed {
        tracing::info!(
            target: "mcp_spawn_gate",
            server = %server_name,
            command = %command,
            args = %args_json,
            policy = %policy,
            verdict = "allow",
            "MCP spawn gate: allowlisted"
        );
        Ok(())
    } else if policy == "warn" {
        tracing::warn!(
            target: "mcp_spawn_gate",
            server = %server_name,
            command = %command,
            args = %args_json,
            policy = %policy,
            verdict = "allow_warn",
            "MCP spawn gate: 未登记 server（warn 迁移档放行并留痕）"
        );
        Ok(())
    } else {
        tracing::warn!(
            target: "mcp_spawn_gate",
            server = %server_name,
            command = %command,
            args = %args_json,
            policy = %policy,
            verdict = "reject",
            "MCP spawn gate: 未登记 server 拒载（enforce；登记见用户级 \
             .evorule/mcp-allowlist.json，或显式切换 warn 迁移档）"
        );
        Err(format!(
            "MCP server `{server_name}` 未在拉起白名单（command `{command}`）；\
             来源认证面拒绝未登记的本地进程拉起。处置：在用户级 \
             .evorule/mcp-allowlist.json 登记该 server，或将 \
             EVORULE_MCP_SPAWN_POLICY 显式切换为 warn 迁移档"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn entry(command: &str, args_pattern: Option<Vec<String>>) -> AllowlistEntry {
        AllowlistEntry {
            command: command.to_string(),
            args_pattern,
            comment: "测试登记".to_string(),
            approved_by: None,
            approved_at: None,
        }
    }

    // ===== 最小环境 =====

    #[test]
    fn minimal_env_keeps_only_safe_keys_plus_extras() {
        let parent: HashMap<String, String> = [
            ("PATH", "/usr/bin"),
            ("SYSTEMROOT", r"C:\Windows"),
            ("EVORULE_SERVICE_TOKEN", "secret"), // 凭据键：必须被剔除
            ("ANTHROPIC_API_KEY", "sk-x"),       // 凭据键：必须被剔除
            ("RANDOM_BUSINESS_VAR", "v"),        // 业务键：必须被剔除
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let mut extra = HashMap::new();
        extra.insert("MCP_EXTRA".to_string(), "1".to_string());
        let env = build_minimal_env(&parent, &extra);
        assert!(env.contains_key("PATH"));
        assert!(env.contains_key("SYSTEMROOT"));
        assert!(env.contains_key("MCP_EXTRA"));
        assert!(!env.contains_key("EVORULE_SERVICE_TOKEN"));
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
        assert!(!env.contains_key("RANDOM_BUSINESS_VAR"));
    }

    // ===== 白名单解析 =====

    #[test]
    fn parse_rejects_missing_comment() {
        let raw = r#"{"allow":[{"command":"foo"}]}"#;
        let r = SpawnAllowlist::parse("t", raw);
        assert!(r.is_err(), "缺 comment 必须拒载");
        assert!(r.unwrap_err().contains("缺 comment"));
    }

    #[test]
    fn parse_rejects_bare_interpreter_without_args_pattern() {
        // 裸解释器（无 args_pattern）拒载；补 args_pattern 后合法
        let r = SpawnAllowlist::parse("t", r#"{"allow":[{"command":"npx","comment":"x"}]}"#);
        assert!(r.is_err(), "裸 npx 无 args_pattern 必须拒载");
        let ok = SpawnAllowlist::parse(
            "t",
            r#"{"allow":[{"command":"npx","args_pattern":["-y","@modelcontextprotocol/*"],"comment":"x"}]}"#,
        );
        assert!(ok.is_ok());
    }

    // ===== 匹配 =====

    #[test]
    fn matches_command_exact_and_args_prefix() {
        let list = SpawnAllowlist {
            allow: vec![
                entry("my-fixed-server.exe", None),
                entry(
                    "npx",
                    Some(vec![
                        "-y".to_string(),
                        "@modelcontextprotocol/*".to_string(),
                    ]),
                ),
            ],
        };
        // 固定二进制：仅 command 匹配，args 任意
        assert!(list.matches("my-fixed-server.exe", &["--x".to_string()]));
        // npx + 白名单包名前缀
        assert!(list.matches(
            "npx",
            &[
                "-y".to_string(),
                "@modelcontextprotocol/server-fs".to_string()
            ]
        ));
        // npx + 未登记包名 → 不匹配
        assert!(!list.matches("npx", &["-y".to_string(), "@evil/pkg".to_string()]));
        // 前缀必须按段对齐：第二段才有意义，第一段不同即不匹配
        assert!(!list.matches("npx", &["@modelcontextprotocol/server-fs".to_string()]));
    }

    // ===== 策略门 =====

    #[test]
    fn gate_enforce_rejects_unlisted_and_warn_allows() {
        let list = SpawnAllowlist {
            allow: vec![entry("known", None)],
        };
        // enforce：未登记拒载
        let r = gate("s", "unknown-cmd", &[], "enforce", Some(&list));
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("未在拉起白名单"));
        // warn：放行
        assert!(gate("s", "unknown-cmd", &[], "warn", Some(&list)).is_ok());
        // 名单内：放行
        assert!(gate("s", "known", &[], "enforce", Some(&list)).is_ok());
    }

    #[test]
    fn gate_missing_allowlist_enforce_rejects_all() {
        // None = 文件缺失：enforce 下全部 stdio 拒载（fail-closed），warn 放行
        let r = gate("s", "known", &[], "enforce", None);
        assert!(r.is_err());
        assert!(gate("s", "known", &[], "warn", None).is_ok());
    }
}
