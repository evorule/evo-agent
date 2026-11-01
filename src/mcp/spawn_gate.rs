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
//!    批准清单。清单来源优先级：①治理清单（治理数据集 `org.evorule.mcp.servers`
//!    生效快照，gov_source 拉取+TTL 缓存，许可走治理审批链）②降级：用户级
//!    白名单文件（`.evorule/mcp-allowlist.json`，与工作区物理分离，会话写不到；
//!    治理未配置时即权威、不可达时显式降级，降级原因与时长事件留痕）。
//!    缺省 enforce（未登记=拒载）；`warn`=显式迁移档（放行+留痕）。
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
/// **不允许**以无 args_pattern 的形态进入批准清单（否则清单形同虚设）。
/// 与治理侧录入校验镜像对齐；修改须双侧同步（黄金向量测试锚定）。
const BARE_INTERPRETERS: &[&str] = &["npx", "uvx", "node", "python", "python3", "deno", "bun"];

/// MCP 批准条目固定 schema_ref（治理数据集 `org.evorule.mcp.servers` 内条目
/// 的领域 schema 锚；与治理侧约定常量镜像对齐）
pub(crate) const MCP_SERVER_SCHEMA_REF: &str = "builtin:mcp/server";

/// MCP 拉起批准清单数据集约定 id（与治理侧约定常量镜像对齐；提案上行与
/// 生效清单拉取共用）
pub(crate) const MCP_SERVERS_DATASET_ID: &str = "org.evorule.mcp.servers";

/// 裸解释器判定（提案上行消费：裸解释器的提案必须携带本次观测参数段锁包）
pub(crate) fn is_bare_interpreter(command: &str) -> bool {
    BARE_INTERPRETERS
        .iter()
        .any(|i| command.eq_ignore_ascii_case(i))
}

/// 批准条目身份键：command+args_pattern 的规范哈希（治理资产域条目指纹：
/// serde_json canonical 紧凑输入 + `blake3:` 前缀 + evorule-hash
/// `json_digest`+`prefixed` 原语）。
///
/// 与治理侧录入校验**镜像实现**（语义权威在治理侧 `model::mcp_servers`）：
/// 规范序列 = JSON 数组 `[command, 段...]`（args_pattern 缺省 = 空数组），
/// command 不做归一化（匹配语义字面精确，规范序列同样字面精确）。两侧
/// 黄金向量测试逐字面量对齐，任一侧修改必须双侧同步。
pub(crate) fn canonical_server_key(command: &str, args_pattern: Option<&[String]>) -> String {
    let mut canon = Vec::with_capacity(1 + args_pattern.map_or(0, <[String]>::len));
    canon.push(serde_json::Value::String(command.to_string()));
    if let Some(segs) = args_pattern {
        canon.extend(segs.iter().map(|s| serde_json::Value::String(s.clone())));
    }
    evorule_hash::prefixed(&evorule_hash::json_digest(&canon))
}

/// 批准条目形状规则（command/args_pattern 语义面；治理侧镜像）。规则细则见
/// 治理侧同形函数——裸解释器必带非空 args_pattern；段内仅支持段尾 `*` 通配；
/// 空段/空数组/空命令拒绝。
pub(crate) fn validate_approval_entry(
    command: &str,
    args_pattern: Option<&[String]>,
) -> Result<(), String> {
    if command.trim().is_empty() {
        return Err("command 不能为空".to_string());
    }
    if let Some(segs) = args_pattern {
        if segs.is_empty() {
            return Err(
                "args_pattern 空数组无意义：请省略该字段（仅匹配 command），或填写实际段序列"
                    .to_string(),
            );
        }
        for s in segs {
            if s.is_empty() {
                return Err("args_pattern 含空段".to_string());
            }
            if s.contains('*') && !s.ends_with('*') {
                return Err(format!(
                    "args_pattern 段 `{s}` 含通配符但不在段尾（仅支持段尾 `*` 前缀通配，其余形态永不匹配，录入即拒）"
                ));
            }
        }
    }
    let bare = BARE_INTERPRETERS
        .iter()
        .any(|i| command.eq_ignore_ascii_case(i));
    if bare && args_pattern.is_none_or(<[String]>::is_empty) {
        return Err(format!(
            "裸解释器 `{command}` 必须登记非空 args_pattern（锁死包名/脚本参数，否则批准形同虚设）"
        ));
    }
    Ok(())
}

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

/// 批准清单条目
///
/// `comment` 必填（谁为何登记此条目的最小审计——不允许无归因的放行）；
/// `args_pattern` 缺省 = 仅匹配 command（适用于固定二进制）；给出 = args 须
/// 前缀匹配该段序列（适用于 npx/uvx 等包管理器拉起，把包名一并锁死；
/// 段内支持尾缀 `*` 通配）。
///
/// `server_key`/`approval_ref` 为治理清单条目随行字段（批准条目身份键/审批
/// 锚，判定命中时随事件出证）；本地文件条目可省略（serde 缺省 None）。
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_ref: Option<String>,
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

    /// 条目匹配：command 字面精确 + args 前缀逐段匹配（段内支持尾缀 `*` 通配）。
    /// 返回命中的条目本体（事件面随行 server_key/approval_ref 出证）。
    pub fn matches(&self, command: &str, args: &[String]) -> Option<&AllowlistEntry> {
        self.allow.iter().find(|e| {
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

/// 单 server 装载门总入口（调用方一行接入）：治理清单优先 → 降级本地文件 →
/// 按 env 策略判定。每一步来源选择显式留痕（降级原因/时长不静默）；
/// 清单解析失败 = 两种档位下均连接失败（配置损坏必须大声）。
/// 未登记 server 的观测在判定后转治理提案（提案回流，fail-soft：独立线程
/// fire-and-forget，判定路径零等待零依赖）。
pub(crate) fn ensure_spawn_allowed(
    server_name: &str,
    command: &str,
    args: &[String],
) -> Result<(), String> {
    let policy = spawn_policy();
    let result = match super::gov_source::availability() {
        super::gov_source::Availability::Governance(snapshot) => gate(
            server_name,
            command,
            args,
            &policy,
            Some(&snapshot.allow),
            "governance",
        ),
        super::gov_source::Availability::Fallback {
            reason,
            degraded_for,
        } => {
            tracing::warn!(
                target: "mcp_spawn_gate",
                server = %server_name,
                reason = reason,
                degraded_secs = degraded_for.map(|d| d.as_secs()).unwrap_or(0),
                "MCP spawn gate: 治理清单不可用，本次判定降级本地白名单文件（显式降级）"
            );
            let list = SpawnAllowlist::load()?;
            gate(
                server_name,
                command,
                args,
                &policy,
                Some(&list),
                "local_fallback",
            )
        }
        super::gov_source::Availability::NotConfigured => {
            let list = SpawnAllowlist::load()?;
            gate(server_name, command, args, &policy, Some(&list), "local")
        }
    };
    // 提案回流钩子：warn 放行的未登记 / enforce 拒载的未登记，都是「第一次
    // 使用即产生提案」的观测点（治理清单命中放行则无提案语义）。
    match &result {
        Err(_) => super::gov_proposal::maybe_propose(server_name, command, args, "enforce 拒载"),
        Ok(GateOutcome::AllowedUnlistedWarn) => {
            super::gov_proposal::maybe_propose(server_name, command, args, "warn 迁移档放行")
        }
        Ok(GateOutcome::Allowed) => {}
    }
    // 调用方契约保持 Result<(), _>：判定结论仅供提案钩子消费
    result.map(|_| ())
}

/// 装载门判定结论（Ok 内载荷，提案回流钩子消费）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateOutcome {
    /// 名单命中放行
    Allowed,
    /// warn 迁移档下未登记放行（留痕 + 提案上行钩子）
    AllowedUnlistedWarn,
}

/// 装载门判定（单 server）：Ok(结论)=放行；Err=拒载（消息含原因与处置指引）。
/// 每次判定发结构化事件（治理事实挂点：来源/条目身份键/审批锚/判定/理由）。
pub(crate) fn gate(
    server_name: &str,
    command: &str,
    args: &[String],
    policy: &str,
    allowlist: Option<&SpawnAllowlist>,
    origin: &str,
) -> Result<GateOutcome, String> {
    let matched = allowlist.and_then(|list| list.matches(command, args));
    let allowed = matched.is_some();
    let args_json = serde_json::to_string(args).unwrap_or_default();
    // 治理事实出证（P2c：判定落定后 fire-and-forget——不参与判定，fail-soft
    // 同提案回流红线；未初始化=no-op）。verdict 三态：命中/warn 未登记/拒载。
    let governance_facts = crate::governance::hooks::McpSpawnFacts {
        server_name: server_name.to_string(),
        command: command.to_string(),
        args_prefix: Some(args.iter().take(8).map(|s| s.to_string()).collect()),
        server_key: matched.and_then(|e| e.server_key.clone()),
        approval_ref: matched.and_then(|e| e.approval_ref.clone()),
        source: Some(origin.to_string()),
        policy: Some(policy.to_string()),
    };
    let server_key = matched
        .and_then(|e| e.server_key.clone())
        .unwrap_or_default();
    let approval_ref = matched
        .and_then(|e| e.approval_ref.clone())
        .unwrap_or_default();
    if allowed {
        tracing::info!(
            target: "mcp_spawn_gate",
            server = %server_name,
            command = %command,
            args = %args_json,
            policy = %policy,
            source = origin,
            server_key = %server_key,
            approval_ref = %approval_ref,
            verdict = "allow",
            "MCP spawn gate: approved"
        );
        Ok(GateOutcome::Allowed)
    } else if policy == "warn" {
        tracing::warn!(
            target: "mcp_spawn_gate",
            server = %server_name,
            command = %command,
            args = %args_json,
            policy = %policy,
            source = origin,
            verdict = "allow_warn",
            "MCP spawn gate: 未登记 server（warn 迁移档放行并留痕）"
        );
        crate::governance::hooks::mcp_spawn(
            &governance_facts,
            crate::governance::hooks::McpSpawnVerdict::AllowWarn,
        );
        Ok(GateOutcome::AllowedUnlistedWarn)
    } else {
        tracing::warn!(
            target: "mcp_spawn_gate",
            server = %server_name,
            command = %command,
            args = %args_json,
            policy = %policy,
            source = origin,
            verdict = "reject",
            "MCP spawn gate: 未登记 server 拒载（enforce；登记走治理批准清单 \
             org.evorule.mcp.servers，或用户级 .evorule/mcp-allowlist.json \
             本地兜底，或显式切换 warn 迁移档）"
        );
        crate::governance::hooks::mcp_spawn(
            &governance_facts,
            crate::governance::hooks::McpSpawnVerdict::Reject,
        );
        Err(format!(
            "MCP server `{server_name}` 未在拉起批准清单（command `{command}`，来源 {origin}）；\
             来源认证面拒绝未登记的本地进程拉起。处置：在治理批准清单 \
             org.evorule.mcp.servers 登记该 server（走治理审批链生效），或在用户级 \
             .evorule/mcp-allowlist.json 本地登记，或将 EVORULE_MCP_SPAWN_POLICY \
             显式切换为 warn 迁移档"
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
            server_key: None,
            approval_ref: None,
        }
    }

    // ===== 黄金向量（与治理侧录入校验逐字面量对齐；任一侧修改必须双侧同步） =====

    #[test]
    fn canonical_server_key_golden_vectors() {
        let segs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // 固定二进制（无 args_pattern）
        assert_eq!(
            canonical_server_key("my-fixed-server.exe", None),
            "blake3:d5c3446a9fd2ae9dc68e6ffbaeb619e0e675f0572bffd04de9a9afce2d28ed87"
        );
        // 包管理器 + 通配段
        assert_eq!(
            canonical_server_key(
                "npx",
                Some(&segs(&["-y", "@modelcontextprotocol/server-*"]))
            ),
            "blake3:f890f2b86546855e48b14db28ab97d7ef06e411ed0a21fb003911a3e1a36c953"
        );
        // 字面精确（不做归一化）：前导空格 = 不同键
        assert_ne!(
            canonical_server_key("npx", Some(&segs(&["-y"]))),
            canonical_server_key("npx ", Some(&segs(&["-y"])))
        );
    }

    #[test]
    fn approval_entry_shape_rules_mirror_governance_side() {
        assert!(validate_approval_entry("npx", None).is_err());
        assert!(validate_approval_entry(
            "npx",
            Some(&["@modelcontextprotocol/server-*".to_string()])
        )
        .is_ok());
        assert!(validate_approval_entry("my-server.exe", None).is_ok());
        assert!(validate_approval_entry("foo", Some(&[] as &[String])).is_err());
        assert!(validate_approval_entry("foo", Some(&["a*b".to_string()])).is_err());
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
        assert!(list
            .matches("my-fixed-server.exe", &["--x".to_string()])
            .is_some());
        // npx + 白名单包名前缀
        assert!(list
            .matches(
                "npx",
                &[
                    "-y".to_string(),
                    "@modelcontextprotocol/server-fs".to_string()
                ]
            )
            .is_some());
        // npx + 未登记包名 → 不匹配
        assert!(list
            .matches("npx", &["-y".to_string(), "@evil/pkg".to_string()])
            .is_none());
        // 前缀必须按段对齐：第二段才有意义，第一段不同即不匹配
        assert!(list
            .matches("npx", &["@modelcontextprotocol/server-fs".to_string()])
            .is_none());
    }

    // ===== 策略门 =====

    #[test]
    fn gate_enforce_rejects_unlisted_and_warn_allows() {
        let list = SpawnAllowlist {
            allow: vec![entry("known", None)],
        };
        // enforce：未登记拒载
        let r = gate("s", "unknown-cmd", &[], "enforce", Some(&list), "local");
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("未在拉起批准清单"));
        // warn：放行
        assert!(gate("s", "unknown-cmd", &[], "warn", Some(&list), "local").is_ok());
        // 名单内：放行
        assert!(gate("s", "known", &[], "enforce", Some(&list), "local").is_ok());
    }

    #[test]
    fn gate_missing_allowlist_enforce_rejects_all() {
        // None = 名单缺失：enforce 下全部 stdio 拒载（fail-closed），warn 放行
        let r = gate("s", "known", &[], "enforce", None, "governance");
        assert!(r.is_err());
        assert!(gate("s", "known", &[], "warn", None, "local_fallback").is_ok());
    }

    // ===== 双源门（来源选择语义） =====

    #[test]
    fn gate_origin_flows_through_verdicts() {
        // 事件面来源标签贯穿三种来源（governance / local_fallback / local）；
        // 判定语义与来源无关——同一名单同一结果
        let mut listed = entry("known", None);
        listed.server_key = Some(canonical_server_key("known", None));
        listed.approval_ref = Some("org.evorule.mcp.servers@v1".to_string());
        let list = SpawnAllowlist {
            allow: vec![listed],
        };
        for origin in ["governance", "local_fallback", "local"] {
            assert!(gate("s", "known", &[], "enforce", Some(&list), origin).is_ok());
            assert!(gate("s", "other", &[], "enforce", Some(&list), origin).is_err());
        }
    }
}
