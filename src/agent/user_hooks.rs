// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! user_hooks —— 可编程 hooks（交互批 第二批）——用户定制挂点，治理之后、执行前后。
//!
//! 配置 `{workdir}/.evo-agent/hooks.json`：`pre_tool` / `post_tool` / `turn_end`
//! 三类挂点，各为有序规则列表（工具名匹配 `name` / `prefix*` + 命令行 + 超时）。
//!
//! ## 顺序契约（39 号 §三）
//!
//! 治理 enforce（硬闸）→ hooks（用户定制）→ 工具执行。hooks **不可绕过治理**：
//! pre_tool 挂点在管道内位于治理裁决与分级审批之后、执行（⑦）之前；治理拒绝
//! 的调用永远不会到达 hooks。
//!
//! ## 执行语义
//!
//! - 命中规则以平台 shell 子进程执行用户命令，stdin 传 JSON 上下文
//!   `{hook, tool, args, result?, session_id?}`；
//! - 子进程环境=**最小白名单**（系统运行必需键）+ 规则 `env_keys` 显式追加
//!   （声明式增量；业务键/凭据键不经声明不可达子进程）；
//! - `pre_tool`：stdout 可返回 `{"decision":"deny","reason":…}`（拒绝执行，
//!   拒绝文案回喂 LLM）或 `{"decision":"rewrite","args":{…}}`（参数改写）；
//!   两者皆无（或 stdout 非法）=放行；规则依序执行，**deny/rewrite 即停**
//!   （first-wins）；stdout 允许夹带日志行——逐行回退提取裁决 JSON；
//! - `post_tool` / `turn_end`：只读不可阻断——结局/退出码只留痕；
//! - 超时缺省 10s 可配（钳 1-120s）；超时**终止子进程**（kill_on_drop）；
//!   超时与失败按 **pre fail-open**（放行+留痕）/ **post·turn_end 忽略**处理；
//! - 单次调用全部命中规则的聚合墙钟 `HOOK_TOTAL_BUDGET_SECS`（超预算后续
//!   规则跳过并留痕，防规则数×超时上限线性放大拖慢工具调用）；
//! - 工作目录 = 会话 workdir（hooks 在边界内执行）。
//!
//! ## 信任域与装载闸（指纹钉版）
//!
//! hooks.json 位于 workdir（工作区内容域）——**该域可被不可信内容写入**
//! （克隆含恶意 hooks.json 的仓库、shell/解释器重定向写文件）。hooks 命令
//! 以用户级 shell 执行，若静默自动装载即构成「workspace 内容 → 持久化
//! 命令执行」的植入链。因此装载以**进程环境指纹钉版**为信任根：
//! - `EVO_HOOKS_TRUST_FINGERPRINT` 未设置 → hooks.json **不装载**（warn 留痕，
//!   给出当前指纹与设置指引）——缺省关；
//! - 设置且与文件指纹（blake3 前缀形态，与 journal digest 同口径）一致 →
//!   装载；支持逗号分隔指纹列表（hooks 轮换期并存）；
//! - 信任根=进程环境（部署方/启动者控制）——agent 改环境只影响子进程，
//!   无法自批；变更 hooks.json 必须经人重新钉版。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

/// 单挂点规则数上限（防配置膨胀拖慢每次调用）
pub const HOOK_MAX_RULES: usize = 20;
/// 命令行字符上限
pub const HOOK_MAX_COMMAND_CHARS: usize = 2000;
/// 工具名匹配式字符上限
pub const HOOK_MAX_TOOL_PATTERN_CHARS: usize = 100;
/// 子进程 stdout 读取上限（防 hook 输出爆炸）
pub const HOOK_MAX_STDOUT_BYTES: usize = 64 * 1024;
/// 超时缺省（秒）
pub const HOOK_DEFAULT_TIMEOUT_SECS: u64 = 10;
/// 超时钳位下限（秒）
pub const HOOK_MIN_TIMEOUT_SECS: u64 = 1;
/// 超时钳位上限（秒）
pub const HOOK_MAX_TIMEOUT_SECS: u64 = 120;
/// 单次调用全部命中规则的聚合墙钟（秒）——规则数×单规则超时上限的线性
/// 放大不再无界（20 规则×120s 最坏 40 分钟 → 60s 封顶，超预算跳过+留痕）
pub const HOOK_TOTAL_BUDGET_SECS: u64 = 60;
/// hooks.json 指纹钉版信任根（进程环境；逗号分隔指纹列表；未设置=hooks 不装载）
pub const HOOK_TRUST_ENV: &str = "EVO_HOOKS_TRUST_FINGERPRINT";
/// hook 子进程环境白名单基线（仅系统运行必需键；对齐 MCP spawn 闸最小集纪律——
/// 业务键/凭据键必须经规则 env_keys 显式声明才可达子进程）
const HOOK_MINIMAL_ENV_KEYS: &[&str] = &[
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
];

/// 三类挂点
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    /// 工具执行前（可 deny/改写）
    PreTool,
    /// 工具执行后（只读）
    PostTool,
    /// 回合结束（只读）
    TurnEnd,
}

impl HookKind {
    /// 配置节名（hooks.json 顶层键）
    pub fn config_key(self) -> &'static str {
        match self {
            HookKind::PreTool => "pre_tool",
            HookKind::PostTool => "post_tool",
            HookKind::TurnEnd => "turn_end",
        }
    }
}

/// 单条 hook 规则（hooks.json 解析形态）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookRule {
    /// 工具名匹配：精确名或 `prefix*` 通配（turn_end 挂点忽略此字段）
    #[serde(default)]
    pub tool: String,
    /// 用户命令行（平台 shell 执行）
    pub command: String,
    /// 超时（秒；缺省 10，钳 1-120）
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// 环境变量白名单显式增量：子进程环境=最小系统集+本清单声明的键
    /// （值取自父进程；不存在则跳过）。业务键/凭据键不经声明不可达子进程。
    #[serde(default)]
    pub env_keys: Vec<String>,
}

impl HookRule {
    /// 配置校验（构造期一次，fail-visible 列明全部问题）
    fn validate(&self, kind: HookKind) -> Result<(), String> {
        let mut problems = Vec::new();
        if self.command.is_empty() {
            problems.push("command is required".to_string());
        }
        if self.command.chars().count() > HOOK_MAX_COMMAND_CHARS {
            problems.push(format!("command exceeds {HOOK_MAX_COMMAND_CHARS} chars"));
        }
        if self.env_keys.len() > 32 {
            problems.push("env_keys exceeds 32 entries".to_string());
        }
        for k in &self.env_keys {
            if k.is_empty() || k.chars().count() > 128 {
                problems.push("env_keys entry invalid (empty or >128 chars)".to_string());
            } else if !k
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                problems.push(format!(
                    "env_keys entry '{k}' invalid: expected ASCII UPPER_CASE env key"
                ));
            }
        }
        if kind != HookKind::TurnEnd {
            if self.tool.is_empty() {
                problems.push("tool match pattern is required".to_string());
            }
            if self.tool.chars().count() > HOOK_MAX_TOOL_PATTERN_CHARS {
                problems.push(format!(
                    "tool pattern exceeds {HOOK_MAX_TOOL_PATTERN_CHARS} chars"
                ));
            }
        }
        if let Some(t) = self.timeout_secs {
            if !(HOOK_MIN_TIMEOUT_SECS..=HOOK_MAX_TIMEOUT_SECS).contains(&t) {
                problems.push(format!(
                    "timeout_secs must be {HOOK_MIN_TIMEOUT_SECS}-{HOOK_MAX_TIMEOUT_SECS}"
                ));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    /// 工具名匹配（精确或 `prefix*` 通配；与 serve_tools 允许面同法）
    fn matches_tool(&self, tool_name: &str) -> bool {
        match self.tool.strip_suffix('*') {
            Some(prefix) => tool_name.starts_with(prefix),
            None => self.tool == tool_name,
        }
    }

    /// 生效超时
    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.timeout_secs
                .unwrap_or(HOOK_DEFAULT_TIMEOUT_SECS)
                .clamp(HOOK_MIN_TIMEOUT_SECS, HOOK_MAX_TIMEOUT_SECS),
        )
    }
}

/// hooks 配置（三类挂点规则表）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserHooks {
    /// 工具执行前（可 deny/改写）
    #[serde(default)]
    pub pre_tool: Vec<HookRule>,
    /// 工具执行后（只读）
    #[serde(default)]
    pub post_tool: Vec<HookRule>,
    /// 回合结束（只读）
    #[serde(default)]
    pub turn_end: Vec<HookRule>,
}

impl UserHooks {
    /// 解析 + 校验（文件缺失=None；schema 非法=Err 由调用方 fail-visible）
    pub fn load(path: &std::path::Path) -> Result<Option<UserHooks>, String> {
        let s = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("hooks.json unreadable: {e}")),
        };
        Self::parse_str(&s)
    }

    /// 从已读文本解析 + 校验（指纹闸复用：避免双读文件）
    fn parse_str(s: &str) -> Result<Option<UserHooks>, String> {
        if s.trim().is_empty() {
            return Ok(None);
        }
        let hooks: UserHooks =
            serde_json::from_str(s).map_err(|e| format!("hooks.json invalid: {e}"))?;
        for (kind, rules) in [
            (HookKind::PreTool, &hooks.pre_tool),
            (HookKind::PostTool, &hooks.post_tool),
            (HookKind::TurnEnd, &hooks.turn_end),
        ] {
            if rules.len() > HOOK_MAX_RULES {
                return Err(format!(
                    "{} exceeds {HOOK_MAX_RULES} rules",
                    kind.config_key()
                ));
            }
            for (i, r) in rules.iter().enumerate() {
                r.validate(kind)
                    .map_err(|e| format!("{}[{i}]: {e}", kind.config_key()))?;
            }
        }
        Ok(Some(hooks))
    }

    /// 规则数（三类合计；装配期日志用）
    pub fn total_rules(&self) -> usize {
        self.pre_tool.len() + self.post_tool.len() + self.turn_end.len()
    }
}

/// 接线形态：hooks 规则表 + 执行工作目录（会话 workdir；hooks 在边界内执行）。
/// 管道依赖经 Arc 共享（每回合构造 fresh runner，构造期一次性装载）。
#[derive(Debug, Clone)]
pub struct WiredHooks {
    /// 解析后的规则表
    pub hooks: UserHooks,
    /// 执行工作目录（会话 workdir）
    pub workdir: std::path::PathBuf,
}

impl WiredHooks {
    /// 从 workdir 装载 `.evo-agent/hooks.json`（缺失=None；解析失败=Err
    /// 由调用方 fail-visible 决定降级或拒绝）。
    ///
    /// **指纹钉版装载闸**：hooks.json 位于工作区内容域（可被不可信内容写入），
    /// 生效必须以进程环境 [`HOOK_TRUST_ENV`] 的指纹钉版为信任根——未设置=
    /// 不装载（Ok(None)+warn 留痕，会话照常）；指纹不符=不装载+warn（给出
    /// 当前指纹与钉版指引）。解析失败仍 Err（fail-visible：坏的 hooks 文件
    /// 静默跳过会让用户误以为在防）。
    pub fn load_from_workdir(workdir: &std::path::Path) -> Result<Option<WiredHooks>, String> {
        Self::load_from_workdir_with_trust(workdir, std::env::var(HOOK_TRUST_ENV).ok().as_deref())
    }

    /// 装载主体（trust 可注入——测试不经进程环境；生产=HOOK_TRUST_ENV 读进程环境）
    pub fn load_from_workdir_with_trust(
        workdir: &std::path::Path,
        trust: Option<&str>,
    ) -> Result<Option<WiredHooks>, String> {
        let path = workdir.join(".evo-agent").join("hooks.json");
        let s = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("hooks.json unreadable: {e}")),
        };
        let fingerprint = hooks_file_fingerprint(s.as_bytes());
        match trust {
            None => {
                if s.trim().is_empty() {
                    return Ok(None);
                }
                tracing::warn!(
                    path = %path.display(),
                    fingerprint = %fingerprint,
                    "hooks.json present but not trusted: set {HOOK_TRUST_ENV} to the \
                     fingerprint below to pin this file (workspace content is untrusted; \
                     hooks auto-execution requires an explicit trust anchor)"
                );
                return Ok(None);
            }
            Some(listed) => {
                let pinned = listed
                    .split(',')
                    .map(|f| f.trim())
                    .filter(|f| !f.is_empty())
                    .any(|f| f == fingerprint);
                if !pinned {
                    if s.trim().is_empty() {
                        return Ok(None);
                    }
                    tracing::warn!(
                        path = %path.display(),
                        fingerprint = %fingerprint,
                        pinned = %listed,
                        "hooks.json fingerprint mismatch: not loading (re-pin \
                         {HOOK_TRUST_ENV} to the current fingerprint to trust it)"
                    );
                    return Ok(None);
                }
            }
        }
        match UserHooks::parse_str(&s)? {
            Some(hooks) => Ok(Some(WiredHooks {
                hooks,
                workdir: workdir.to_path_buf(),
            })),
            None => Ok(None),
        }
    }
}

/// hooks.json 内容指纹（blake3 前缀自描述形态，与 journal digest 同口径；
/// 哈希一律 evorule-hash，禁自写 blake3）
pub fn hooks_file_fingerprint(bytes: &[u8]) -> String {
    evorule_hash::prefixed(&evorule_hash::digest(bytes))
}

/// pre_tool 裁决（管道消费）
#[derive(Debug, Clone, PartialEq)]
pub enum PreToolVerdict {
    /// 放行（无 deny/rewrite；args 保持原样）
    Allow,
    /// 拒绝执行（reason 回喂 LLM）
    Deny(String),
    /// 参数改写（替换本次调用的 args）
    Rewrite(Value),
}

/// hook 执行结局（留痕用）
#[derive(Debug, Clone)]
pub struct HookOutcome {
    /// 规则序号（0 起，配置顺序）
    pub rule_index: usize,
    /// exited_ok = 退出码 0 且未超时
    pub exited_ok: bool,
    /// 超时/生成失败/读失败等运行性故障（与退出码非 0 区分）
    pub error: Option<String>,
    /// stdout（截到 HOOK_MAX_STDOUT_BYTES；pre_tool 裁决解析源）
    pub stdout: String,
}

impl UserHooks {
    /// 执行一类挂点的全部命中规则（顺序执行；上下文 JSON 随 stdin 下发）
    ///
    /// 返回各规则结局（含失败与超时——留痕由调用方做，本函数不吞不炸）。
    pub async fn run_kind(
        &self,
        kind: HookKind,
        tool_name: &str,
        ctx: &Value,
        workdir: &std::path::Path,
    ) -> Vec<HookOutcome> {
        let rules: &[HookRule] = match kind {
            HookKind::PreTool => &self.pre_tool,
            HookKind::PostTool => &self.post_tool,
            HookKind::TurnEnd => &self.turn_end,
        };
        let mut outcomes = Vec::new();
        for (i, rule) in rules.iter().enumerate() {
            if kind != HookKind::TurnEnd && !rule.matches_tool(tool_name) {
                continue;
            }
            outcomes.push(run_hook_rule(i, rule, kind, ctx, workdir).await);
        }
        outcomes
    }

    /// pre_tool 裁决：依序执行命中规则，首个显式 Deny/Rewrite 生效即返回
    /// （first-wins——文档语义「生效即返回」的实现口径，后续规则命令不再执行）；
    /// 全部放行 = [`PreToolVerdict::Allow`]。运行性故障（超时/生成失败）按
    /// fail-open 放行并随 [`HookOutcome`] 留痕。聚合墙钟
    /// [`HOOK_TOTAL_BUDGET_SECS`]：超预算后剩余规则跳过并留痕。
    pub async fn pre_tool_verdict(
        &self,
        tool_name: &str,
        args: &Value,
        session_id: &str,
        workdir: &std::path::Path,
    ) -> (PreToolVerdict, Vec<HookOutcome>) {
        let ctx = json!({
            "hook": "pre_tool",
            "tool": tool_name,
            "args": args,
            "session_id": session_id,
        });
        // 逐条执行逐条解析:deny/rewrite 即停(后续规则命令不执行)——ubuntu CI
        // 实证缺陷纠正:此前 run_kind 全量先跑、解析循环事后找 deny,
        // 「deny 即停」文档语义从未真正落地;rewrite 分支同停（first-wins）
        let started = std::time::Instant::now();
        let mut outcomes: Vec<HookOutcome> = Vec::new();
        for (i, rule) in self.pre_tool.iter().enumerate() {
            if !rule.matches_tool(tool_name) {
                continue;
            }
            // 聚合墙钟：超预算跳过剩余规则（防规则数×单规则超时上限线性放大）
            if started.elapsed() >= std::time::Duration::from_secs(HOOK_TOTAL_BUDGET_SECS) {
                tracing::warn!(
                    tool = %tool_name,
                    budget_secs = HOOK_TOTAL_BUDGET_SECS,
                    "user hooks total budget exceeded; skipping remaining pre_tool rules"
                );
                outcomes.push(HookOutcome {
                    rule_index: i,
                    exited_ok: false,
                    error: Some(format!(
                        "skipped: aggregate hooks budget of {HOOK_TOTAL_BUDGET_SECS}s exceeded"
                    )),
                    stdout: String::new(),
                });
                break;
            }
            let out = run_hook_rule(i, rule, HookKind::PreTool, &ctx, workdir).await;
            match parse_pre_decision(&out.stdout) {
                Some(PreToolVerdict::Deny(reason)) => {
                    outcomes.push(out);
                    return (PreToolVerdict::Deny(reason), outcomes);
                }
                Some(PreToolVerdict::Rewrite(args)) => {
                    outcomes.push(out);
                    return (PreToolVerdict::Rewrite(args), outcomes);
                }
                Some(PreToolVerdict::Allow) | None => {}
            }
            outcomes.push(out);
        }
        (PreToolVerdict::Allow, outcomes)
    }

    /// post_tool 只读通知（结局随 stdin 下发；失败只留痕不阻断）
    pub async fn post_tool_notify(
        &self,
        tool_name: &str,
        args: &Value,
        result: &str,
        session_id: &str,
        workdir: &std::path::Path,
    ) -> Vec<HookOutcome> {
        let ctx = json!({
            "hook": "post_tool",
            "tool": tool_name,
            "args": args,
            "result": result,
            "session_id": session_id,
        });
        self.run_kind(HookKind::PostTool, tool_name, &ctx, workdir)
            .await
    }

    /// turn_end 只读通知（工具无关；失败只留痕不阻断）
    pub async fn turn_end_notify(
        &self,
        session_id: &str,
        workdir: &std::path::Path,
    ) -> Vec<HookOutcome> {
        let ctx = json!({
            "hook": "turn_end",
            "session_id": session_id,
        });
        self.run_kind(HookKind::TurnEnd, "", &ctx, workdir).await
    }
}

/// 解析 pre_tool stdout 裁决（非法/空 = None；宽容 JSON 解析）
///
/// 双回退：整体解析失败后 ① Windows cmd echo 反转义重试（参数传递层转义，
/// 真机实证）；② **逐行倒序提取**可解析为裁决对象的 JSON 行——hook 向 stdout
/// 打日志（调试输出）不再吞掉裁决（日志行非单一 JSON 导致整体解析失败=
/// 静默 fail-open 的历史形态消灭）。
fn parse_pre_decision(stdout: &str) -> Option<PreToolVerdict> {
    let s = stdout.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(v) = parse_decision_value(s) {
        return Some(v);
    }
    // 回退 ①:cmd echo 引号反转义重试
    if let Some(v) = parse_decision_value(&s.replace("\\\"", "\"")) {
        return Some(v);
    }
    // 回退 ②:逐行倒序找「能解析为含 decision 键的 JSON 对象」的行
    for line in stdout.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Some(v) = parse_decision_value(line) {
            return Some(v);
        }
    }
    None
}

/// 单个 JSON 文本 → 裁决值（None=非裁决对象）
fn parse_decision_value(s: &str) -> Option<PreToolVerdict> {
    let v: Value = serde_json::from_str(s.trim()).ok()?;
    match v.get("decision").and_then(|d| d.as_str())? {
        "deny" => Some(PreToolVerdict::Deny(
            v.get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("blocked by user hook")
                .to_string(),
        )),
        "rewrite" => {
            let args = v.get("args").filter(|a| a.is_object()).cloned()?;
            Some(PreToolVerdict::Rewrite(args))
        }
        "allow" => Some(PreToolVerdict::Allow),
        _ => None,
    }
}

/// 平台 shell 命令形态（用户自持配置，信任级与 shell_exec 同源）
fn shell_command(command: &str) -> (std::process::Command, String) {
    #[cfg(target_os = "windows")]
    {
        let mut c = std::process::Command::new("cmd");
        c.arg("/S").arg("/C").arg(command);
        (c, "cmd /S /C".to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(command);
        (c, "sh -c".to_string())
    }
}

/// hook 子进程环境：最小白名单基线（[`HOOK_MINIMAL_ENV_KEYS`]）+ 规则
/// `env_keys` 显式增量（值取自父进程，不存在跳过）——业务键/凭据键不经
/// 声明不可达子进程（凭据外带面收闸；与 MCP spawn 闸最小集纪律对齐）
fn build_hook_env(rule_env_keys: &[String]) -> Vec<(String, String)> {
    let parent: std::collections::HashMap<String, String> = std::env::vars().collect();
    build_hook_env_from(&parent, rule_env_keys)
}

/// 纯函数变体（parent 可注入——测试不经进程环境，杜绝并行测试的
/// 进程级 environ 竞争）
fn build_hook_env_from(
    parent: &std::collections::HashMap<String, String>,
    rule_env_keys: &[String],
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for key in HOOK_MINIMAL_ENV_KEYS {
        if let Some(v) = parent.get(*key) {
            out.push(((*key).to_string(), v.clone()));
        }
    }
    for k in rule_env_keys {
        if let Some(v) = parent.get(k) {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

/// 单规则执行（超时杀子进程；一切运行性故障记入 outcome，不上抛）
async fn run_hook_rule(
    rule_index: usize,
    rule: &HookRule,
    kind: HookKind,
    ctx: &Value,
    workdir: &std::path::Path,
) -> HookOutcome {
    let _ = kind; // 预留:turn_end 无工具名匹配,形态参数留位(当前调用方已滤)
    let (mut cmd, _label) = shell_command(&rule.command);
    cmd.current_dir(workdir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    // 环境收敛：env_clear 后注入白名单基线 + 规则 env_keys 显式增量
    // （此前全量继承宿主环境=凭据可达 hook 子进程，与「最小环境」注释相悖）
    let env = build_hook_env(&rule.env_keys);
    let mut tcmd = tokio::process::Command::from(cmd);
    tcmd.env_clear();
    for (k, v) in env {
        tcmd.env(k, v);
    }
    // 超时=终止：kill_on_drop 保证 timeout 分支 drop child 时进程树被杀
    // （此前仅放弃等待——hook 进程泄漏且事后仍可完成副作用）
    tcmd.kill_on_drop(true);
    let mut child = match tcmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return HookOutcome {
                rule_index,
                exited_ok: false,
                error: Some(format!("spawn failed: {e}")),
                stdout: String::new(),
            };
        }
    };
    let payload = serde_json::to_vec(ctx).unwrap_or_default();
    // stdin 写入与等待/排空并发：stdin 句柄移交独立任务写入（慢子进程不读
    // stdin 时任务挂起，进程被超时杀死后管道断开、任务自然退出——无泄漏）；
    // wait_with_output 并发读 stdout/stderr——大 payload 与不排 stdout 的
    // 子进程互不再阻塞（此前顺序写先于等待，post_tool 大结果互锁至「超时」）
    if let Some(mut stdin) = child.stdin.take() {
        tokio::spawn(async move {
            if let Err(e) = stdin.write_all(&payload).await {
                // 管道关闭（进程早退）不算致命——继续等退出码
                let _ = e;
            }
            let _ = stdin.shutdown().await;
        });
    }
    let timed = tokio::time::timeout(rule.timeout(), child.wait_with_output()).await;
    match timed {
        Err(_) => HookOutcome {
            rule_index,
            exited_ok: false,
            error: Some(format!(
                "timeout after {}s (killed)",
                rule.timeout().as_secs()
            )),
            stdout: String::new(),
        },
        Ok(Err(e)) => HookOutcome {
            rule_index,
            exited_ok: false,
            error: Some(format!("wait failed: {e}")),
            stdout: String::new(),
        },
        Ok(Ok(output)) => {
            let mut stdout = String::from_utf8_lossy(&output.stdout)
                .chars()
                .take(HOOK_MAX_STDOUT_BYTES)
                .collect::<String>();
            if output.stdout.len() > HOOK_MAX_STDOUT_BYTES {
                stdout.push_str("\n…[hook stdout truncated]");
            }
            HookOutcome {
                rule_index,
                exited_ok: output.status.success(),
                error: None,
                stdout,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_hooks(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hooks.json");
        std::fs::write(&p, body).unwrap();
        (dir, p)
    }

    #[test]
    fn missing_and_empty_files_are_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(UserHooks::load(&dir.path().join("absent.json"))
            .unwrap()
            .is_none());
        let (d, p) = tmp_hooks("   ");
        let _ = d;
        assert!(UserHooks::load(&p).unwrap().is_none());
    }

    #[test]
    fn invalid_schema_fails_visible() {
        let (d, p) = tmp_hooks(r#"{"pre_tool": [{"tool": "shell_exec"}]}"#);
        let _ = d;
        // serde 先拦缺失字段;无论走 serde 还是 validate,都必须 fail-visible
        let err = UserHooks::load(&p).unwrap_err();
        assert!(
            err.contains("hooks.json invalid") || err.contains("command is required"),
            "{err}"
        );
    }

    #[test]
    fn tool_pattern_matching_exact_and_wildcard() {
        let r = HookRule {
            tool: "shell_exec".into(),
            command: "echo hi".into(),
            timeout_secs: None,
            env_keys: Vec::new(),
        };
        assert!(r.matches_tool("shell_exec"));
        assert!(!r.matches_tool("shell_execx"));
        let w = HookRule {
            tool: "file_*".into(),
            command: "echo hi".into(),
            timeout_secs: None,
            env_keys: Vec::new(),
        };
        assert!(w.matches_tool("file_write"));
        assert!(!w.matches_tool("git_status"));
    }

    #[test]
    fn parse_pre_decision_forms() {
        assert_eq!(
            parse_pre_decision(r#"{"decision":"deny","reason":"no rm"}"#),
            Some(PreToolVerdict::Deny("no rm".into()))
        );
        assert_eq!(
            parse_pre_decision(r#"{"decision":"deny"}"#),
            Some(PreToolVerdict::Deny("blocked by user hook".into()))
        );
        assert_eq!(
            parse_pre_decision(r#"{"decision":"rewrite","args":{"path":"b.txt"}}"#),
            Some(PreToolVerdict::Rewrite(json!({"path": "b.txt"})))
        );
        // rewrite 非 object args = 不采纳
        assert_eq!(
            parse_pre_decision(r#"{"decision":"rewrite","args":"x"}"#),
            None
        );
        // 放行/噪声/垃圾
        assert_eq!(
            parse_pre_decision(r#"{"decision":"allow"}"#),
            Some(PreToolVerdict::Allow)
        );
        assert_eq!(parse_pre_decision("plain text"), None);
        assert_eq!(parse_pre_decision("   "), None);
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn pre_tool_deny_stops_execution() {
        // 决策 JSON 经文件零引用下发(cat 直读;shell 引号在各平台形态不同,
        // 不进测试依赖——Windows 侧由 cmd 转义回退路径覆盖)
        let dir = tempfile::tempdir().unwrap();
        let decision_file = dir.path().join("decision.json");
        std::fs::write(&decision_file, r#"{"decision":"deny","reason":"no rm"}"#).unwrap();
        let hooks = UserHooks {
            pre_tool: vec![
                HookRule {
                    tool: "shell_exec".into(),
                    command: format!("cat {}", decision_file.display()),
                    timeout_secs: Some(15),
                    env_keys: Vec::new(),
                },
                HookRule {
                    tool: "shell_exec".into(),
                    command: "exit 3".into(),
                    timeout_secs: None,
                    env_keys: Vec::new(),
                },
            ],
            ..Default::default()
        };
        let (v, outs) = hooks
            .pre_tool_verdict(
                "shell_exec",
                &json!({"command": "rm -rf /"}),
                "s1",
                dir.path(),
            )
            .await;
        assert_eq!(
            v,
            PreToolVerdict::Deny("no rm".into()),
            "outcomes: {outs:?}"
        );
        assert_eq!(
            outs.len(),
            1,
            "deny must stop later rules; outcomes: {outs:?}"
        );
    }

    #[tokio::test]
    async fn pre_tool_timeout_fails_open() {
        let hooks = UserHooks {
            pre_tool: vec![HookRule {
                tool: "*".into(),
                command: if cfg!(target_os = "windows") {
                    // timeout.exe 拒绝重定向 stdin;ping 是 Windows 惯用睡眠
                    "ping -n 30 127.0.0.1".into()
                } else {
                    // exec 替换 shell 进程为本体——超时杀死的就是延迟本体
                    "exec sleep 30".into()
                },
                timeout_secs: Some(1),
                env_keys: Vec::new(),
            }],
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let (v, outs) = hooks
            .pre_tool_verdict("file_write", &json!({}), "s1", dir.path())
            .await;
        assert_eq!(v, PreToolVerdict::Allow, "timeout must fail open");
        assert!(outs[0].error.as_deref().unwrap_or("").contains("timeout"));
    }

    #[tokio::test]
    async fn post_tool_is_fire_and_forget() {
        let hooks = UserHooks {
            post_tool: vec![HookRule {
                tool: "file_read".into(),
                command: "exit 5".into(), // 非零退出也只留痕
                timeout_secs: None,
                env_keys: Vec::new(),
            }],
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let outs = hooks
            .post_tool_notify("file_read", &json!({}), "ok", "s1", dir.path())
            .await;
        assert_eq!(outs.len(), 1);
        assert!(!outs[0].exited_ok);
    }

    #[tokio::test]
    async fn stdin_receives_context_json() {
        // hook 把 stdin 原样回显到 stdout;校验上下文形状
        let hooks = UserHooks {
            post_tool: vec![HookRule {
                tool: "*".into(),
                command: if cfg!(target_os = "windows") {
                    r#"findstr ".*"more"#.into()
                } else {
                    "cat".into()
                },
                timeout_secs: None,
                env_keys: Vec::new(),
            }],
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let outs = hooks
            .post_tool_notify(
                "file_read",
                &json!({"path": "a.txt"}),
                "RES",
                "sid-9",
                dir.path(),
            )
            .await;
        if cfg!(not(target_os = "windows")) {
            let ctx: Value = serde_json::from_str(&outs[0].stdout).unwrap();
            assert_eq!(ctx["hook"], "post_tool");
            assert_eq!(ctx["tool"], "file_read");
            assert_eq!(ctx["args"]["path"], "a.txt");
            assert_eq!(ctx["result"], "RES");
            assert_eq!(ctx["session_id"], "sid-9");
        } else {
            // Windows findstr 回显形态不保证逐字节,只要求不炸
            assert!(!outs.is_empty());
        }
    }

    #[test]
    fn rule_count_cap_enforced() {
        let rules: Vec<HookRule> = (0..=HOOK_MAX_RULES)
            .map(|_| HookRule {
                tool: "*".into(),
                command: "echo x".into(),
                timeout_secs: None,
                env_keys: Vec::new(),
            })
            .collect();
        let body = serde_json::to_string(&json!({ "turn_end": rules })).unwrap();
        let (d, p) = tmp_hooks(&body);
        let _ = d;
        let err = UserHooks::load(&p).unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
    }
}

#[cfg(test)]
mod batch_a_tickets {
    //! 批A 入场券（先红后绿）：first-wins rewrite 语义红验（现行为 last-wins）
    use super::*;

    #[tokio::test]
    async fn first_rewrite_wins_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = UserHooks {
            pre_tool: vec![
                HookRule {
                    tool: "file_read".into(),
                    command: r#"echo {"decision":"rewrite","args":{"path":"first.txt"}}"#.into(),
                    timeout_secs: Some(5),
                    env_keys: Vec::new(),
                },
                HookRule {
                    tool: "file_read".into(),
                    command: r#"echo {"decision":"rewrite","args":{"path":"second.txt"}}"#.into(),
                    timeout_secs: Some(5),
                    env_keys: Vec::new(),
                },
            ],
            ..Default::default()
        };
        let (verdict, outcomes) = hooks
            .pre_tool_verdict(
                "file_read",
                &serde_json::json!({"path":"a.txt"}),
                "s1",
                dir.path(),
            )
            .await;
        match verdict {
            PreToolVerdict::Rewrite(args) => {
                assert_eq!(
                    args["path"], "first.txt",
                    "首个 Rewrite 生效即返回（first-wins）"
                );
            }
            other => panic!("expected rewrite, got {other:?}"),
        }
        // 首个 Rewrite 生效即停：第二条规则命令不执行
        assert_eq!(outcomes.len(), 1, "rewrite 后不得继续执行后续规则");
    }
}

#[cfg(test)]
mod batch_a_behavior {
    //! 批A 行为锁：指纹闸（信任根=注入，不经进程 env）/env 白名单/超时杀进程
    use super::*;

    fn hooks_body() -> String {
        r#"{"pre_tool":[{"tool":"file_read","command":"echo allow"}]}"#.to_string()
    }

    #[test]
    fn fingerprint_gate_unset_trust_never_loads() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join(".evo-agent");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("hooks.json"), hooks_body()).unwrap();
        let loaded = WiredHooks::load_from_workdir_with_trust(dir.path(), None)
            .expect("unset trust must not error");
        assert!(
            loaded.is_none(),
            "未钉版指纹的 hooks.json 不得装载（植入链闸）"
        );
    }

    #[test]
    fn fingerprint_gate_mismatch_never_loads() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join(".evo-agent");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("hooks.json"), hooks_body()).unwrap();
        let loaded = WiredHooks::load_from_workdir_with_trust(
            dir.path(),
            Some("blake3:0000000000000000000000000000000000000000000000000000000000000000"),
        )
        .expect("mismatched trust must not error");
        assert!(loaded.is_none(), "指纹不符不得装载");
    }

    #[test]
    fn fingerprint_gate_pinned_list_loads() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join(".evo-agent");
        std::fs::create_dir_all(&d).unwrap();
        let body = hooks_body();
        std::fs::write(d.join("hooks.json"), &body).unwrap();
        let fp = hooks_file_fingerprint(body.as_bytes());
        // 逗号分隔列表中的第二项命中也应放行（轮换期并存语义）
        let listed = format!("blake3:deadbeef, {fp}");
        let loaded = WiredHooks::load_from_workdir_with_trust(dir.path(), Some(&listed))
            .expect("pinned trust must not error");
        assert!(loaded.is_some(), "钉版指纹命中必须装载");
        assert_eq!(loaded.unwrap().hooks.total_rules(), 1);
    }

    #[test]
    fn hook_env_is_minimal_baseline_plus_declared_keys() {
        // 注入式：父环境为合成 HashMap（不经进程环境——并行测试下
        // set_var 与他人 vars()/spawn 的 environ 竞争是 Linux UB 面）
        let mut parent = std::collections::HashMap::new();
        parent.insert("PATH".to_string(), "/usr/bin".to_string());
        parent.insert("EVO_TEST_HOOK_ENV_KEY".to_string(), "sentinel".to_string());
        let rule_env_keys = ["EVO_TEST_HOOK_ENV_KEY".to_string()];
        let env = build_hook_env_from(&parent, &rule_env_keys);
        assert!(
            env.iter().any(|(k, _)| k == "PATH"),
            "PATH 基线必须在场（进程拉起必需）"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "EVO_TEST_HOOK_ENV_KEY" && v == "sentinel"),
            "env_keys 显式声明的键必须注入"
        );
        let bare = build_hook_env_from(&parent, &[]);
        assert!(
            !bare.iter().any(|(k, _)| k == "EVO_TEST_HOOK_ENV_KEY"),
            "未声明的业务键不得注入（凭据外带面收闸）"
        );
        assert!(bare
            .iter()
            .all(|(k, _)| HOOK_MINIMAL_ENV_KEYS.contains(&k.as_str())));
    }

    #[tokio::test]
    async fn timeout_kills_hook_process_before_side_effect() {
        // hook = 延迟后写标记文件；超时（1s）远小于延迟（6s）——若子进程未被杀，
        // 标记文件会在延迟后出现。等待超过延迟窗口后验证标记不存在=进程真死。
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker.txt");
        let delay_then_write = if cfg!(target_os = "windows") {
            format!(
                r#"ping -n 7 127.0.0.1 >nul & echo done > {}"#,
                marker.display()
            )
        } else {
            // exec 替换 shell 进程为本体——超时杀死的就是延迟本体（普通
            // 形态下 sh 死而 sleep 存活，marker 仍会被孤儿写出）
            format!("exec sleep 6; echo done > {}", marker.display())
        };
        let hooks = UserHooks {
            pre_tool: vec![HookRule {
                tool: "file_read".into(),
                command: delay_then_write,
                timeout_secs: Some(1),
                env_keys: Vec::new(),
            }],
            ..Default::default()
        };
        let t0 = std::time::Instant::now();
        let (verdict, outcomes) = hooks
            .pre_tool_verdict("file_read", &serde_json::json!({}), "s1", dir.path())
            .await;
        assert_eq!(verdict, PreToolVerdict::Allow, "超时 fail-open 放行");
        assert!(outcomes[0]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("timeout"));
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(20),
            "超时分支必须即时返回（kill_on_drop），不得干等子进程自然退出（CI 负载余量）"
        );
        // 越过延迟窗口：进程若仍存活则标记已写
        tokio::time::sleep(std::time::Duration::from_secs(7)).await;
        assert!(!marker.exists(), "超时后子进程必须已死（未写出标记文件）");
    }
}
