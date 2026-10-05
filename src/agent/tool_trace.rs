// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! P1:工具调用轨迹采集器 —— 工具级行为治理的事实地基
//!
//! ## 背景与治理语义
//!
//! agent 会话每轮仅 1 条 `call_external` 过引擎,
//! 工具调用全部在 runner 本地执行——引擎审计链对工具级操作零感知,「规则
//! 约束 agent 真实操作」对工具级不可达。本模块在 G17 插桩点采集每次工具
//! 调用的完整轨迹,会话收尾时以 `tool_trace` 指令(宪法 core_eval v0.5.0
//! 新增规则)批量提交进引擎:
//!
//! - 合法轨迹 → StateTransition 事实落审计链(全文可回放);
//! - 违规轨迹 → rules_dir enforce 在约束门拦截(Halted→Violation 留痕);
//! - 两类留痕互补,审计面完整。
//!
//! ## 纪律
//!
//! - fail-soft:提交失败仅计数+warn,绝不阻断会话收尾(与 L2 前馈同纪律);
//! - args 全文进链(裁定口径)前做敏感键脱敏 + 单条体积上限截断(防 payload
//!   膨胀炸链),截断为显式标记不静默丢弃。

use serde_json::{Map, Value};
use std::sync::Mutex;

/// 单条轨迹 args 序列化后的体积上限(字节)。超出即截断并标记 `truncated`,
/// 防巨型 file_write/http_get 响应把 payload 撑爆审计链。
const MAX_ARGS_BYTES: usize = 32 * 1024;

/// 超时终止真实结局登记表上限:采集点缺席时(CLI 直调等无轨迹面的路径)
/// 丢弃最旧条目,防登记表无界堆积。
const MAX_KILLED_OUTCOMES: usize = 64;

/// 超时终止真实结局(执行器生命周期契约)
///
/// 进程族回收方(工具实现,spawn_blocking 内)与轨迹采集点(管道/G13 并行
/// 路径)无共享句柄,以「工具名+参数标识」关联:回收方在终止进程族后登记
/// 真实结局(exit_code/信号),采集点 [`ToolTraceCollector::record`] 取走
/// 匹配项,该次调用轨迹 status 标 `killed` 并附加真实结局——消灭「超时
/// 放弃后结局永不可知」。同名同命令并发超时的极端交错可能互换条目
/// (两方均为真实 killed 结局,仅 exit_code 可能错配,登记表按此取舍)。
#[derive(Debug, Clone)]
pub struct KilledOutcome {
    /// 工具名(与 record 的 tool_name 匹配)
    pub tool: String,
    /// 参数标识(shell_exec = command 原文)
    pub ident: String,
    /// 终止后回收到的退出码(信号终止时为 None)
    pub exit_code: Option<i32>,
    /// 终止信号(Unix SIGKILL=9;Windows 强制终止无信号 = None)
    pub signal: Option<i32>,
    /// 击杀后核验:true = 按所用核验方法未发现进程族存活者(覆盖面见
    /// 回收方注释;Unix 进程组全组 / Windows 击杀前快照集合,回退路径为
    /// 直接子代);false = 发现存活或核验不可用——不确定性显式入账,
    /// 不静默宣称已净。
    pub verified: bool,
}

static KILLED_OUTCOMES: Mutex<Vec<KilledOutcome>> = Mutex::new(Vec::new());

/// 登记超时终止真实结局(进程族回收方调用;fail-soft:锁中毒即放弃)
pub fn register_killed_outcome(outcome: KilledOutcome) {
    if let Ok(mut queue) = KILLED_OUTCOMES.lock() {
        if queue.len() >= MAX_KILLED_OUTCOMES {
            queue.remove(0);
        }
        queue.push(outcome);
    }
}

/// 取走与 (tool_name, ident) 匹配的登记(FIFO 首条;无匹配不动登记表)
fn take_killed_outcome(tool_name: &str, ident: &str) -> Option<KilledOutcome> {
    let mut queue = KILLED_OUTCOMES.lock().ok()?;
    let idx = queue
        .iter()
        .position(|o| o.tool == tool_name && o.ident == ident)?;
    Some(queue.remove(idx))
}

/// 清空登记表(测试面:用例间隔离)
#[cfg(test)]
pub(crate) fn clear_killed_outcomes() {
    if let Ok(mut queue) = KILLED_OUTCOMES.lock() {
        queue.clear();
    }
}

/// 登记表当前条数(测试面)
#[cfg(test)]
pub(crate) fn killed_outcome_len() -> usize {
    KILLED_OUTCOMES.lock().map(|q| q.len()).unwrap_or(0)
}

/// 登记表相关用例的跨模块串行锁(测试面):登记/清空为进程级全局态,
/// 本模块与 shell_exec 的「登记→采集点消费」集成用例须互斥,防并行测试
/// 的 clear 清掉他模块刚登记的条目
#[cfg(test)]
pub(crate) static KILLED_OUTCOME_TEST_LOCK: Mutex<()> = Mutex::new(());

/// 敏感键名表(小写精确/前后缀匹配):命中值替换为 `[REDACTED]`。
const SENSITIVE_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "token",
    "password",
    "passwd",
    "secret",
    "authorization",
    "credential",
    "private_key",
    "passphrase",
    "auth",
];

/// 工具调用轨迹采集器(会话内累积,AgentRunner 持有)
#[derive(Debug, Default)]
pub struct ToolTraceCollector {
    entries: Vec<Value>,
    submit_failures: usize,
    /// 执行桥后端上下文:Some(容器名)=shell_exec 走 docker-exec 后端
    /// (容器名经 serve run 请求扩展字段传入,LLM 不可控);None=宿主后端。
    /// 影响 shell_exec 轨迹成形(后端感知分流,分流≠删检),见 [`ToolTraceCollector::record`]
    exec_backend: Option<String>,
}

/// rm 递归强删旗标(词级匹配):`rm` 本身是 candidate(需审批),
/// 但携带递归旗标的 rm 属破坏性操作,轨迹打 `rm:<flag>` 旗标。
const RM_DANGEROUS_FLAGS: &[&str] = &["-rf", "-fr", "-r"];

/// 违禁域名单单一事实源 = [`crate::builtin_tools::net_guard::DENIED_NETWORK_PATTERNS`]
/// （执行前防线与审计链防线共用同一名单，禁经 benchmark 基础设施网络取答案）。
/// 子串级检测只在应用采集侧(零子串谓词纪律
/// 仅约束规则层),命中打 `domain:<域名>` 旗标,由 server 层规则以
/// `exists(danger_hits)` 判定 enforce——与危险程序打标同链路。
/// 危险命令检测:程序/旗标为词级 token 匹配(非裸子串——防
/// `cat shutdown.log` 类误伤),违禁域为子串级扫描(域名串特异性高,
/// 且违禁域为合规红线,fail-closed 方向误伤只影响轨迹入链形态)。
///
/// 单一事实源 = [`crate::builtin_tools::shell_exec::BLOCKED_COMMANDS`]
/// (执行前防线Blocked 永不名单);本检测是审计链防线:轨迹条目附加
/// `danger_hits` 字段,server 层规则文件以 `exists(value.danger_hits)`
/// 判定并 enforce 拦截(违规轨迹→Violation 留痕)。零子串谓词纪律下,
/// 子串级检测不可入规则层(7 基础域无 contains),故打标在应用采集侧。
///
/// 返回命中描述(如 `program:dd` / `rm:-rf` / `domain:tbench.ai`);无命中
/// 返回空 Vec——**空结果不写字段**(规则层 exists 语义:字段不存在=false)。
fn detect_danger_hits(command: &str) -> Vec<String> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let mut hits = Vec::new();
    for (i, tok) in tokens.iter().enumerate() {
        let bare = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        let is_blocked = crate::builtin_tools::shell_exec::BLOCKED_COMMANDS
            .iter()
            .any(|(name, _)| *name == bare);
        if is_blocked {
            hits.push(format!("program:{bare}"));
        }
        if bare == "rm" {
            if let Some(flag) = tokens[i + 1..]
                .iter()
                .take_while(|t| t.starts_with('-'))
                .find(|t| RM_DANGEROUS_FLAGS.contains(t))
            {
                hits.push(format!("rm:{flag}"));
            }
        }
    }
    hits.extend(detect_domain_hits(command));
    hits
}

/// 违禁域检测(子串级,小写化全文扫描):shell_exec 命令串与 http_get url
/// 共用。命中打 `domain:<域名>` 旗标(如 `domain:tbench.ai`)。
fn detect_domain_hits(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    crate::builtin_tools::net_guard::DENIED_NETWORK_PATTERNS
        .iter()
        .filter(|d| lower.contains(&d.to_lowercase()))
        .map(|d| format!("domain:{d}"))
        .collect()
}

impl ToolTraceCollector {
    /// 设置后端上下文(runner 构造后、运行前调用一次;不注入=宿主后端)
    pub fn set_exec_backend(&mut self, container: Option<&str>) {
        self.exec_backend = container.map(str::to_string);
    }

    /// 记录一次工具调用(含被治理拦截的调用——拦截也是真实执行史)
    ///
    /// `status`: `ok` / `error` / `blocked_by_governance`(裁决拦截) /
    /// `killed`(执行超时,进程族终止后的真实结局,见 [`KilledOutcome`])。
    /// shell_exec 调用附带危险命令打标:命中则轨迹条目附加 `danger_hits`
    /// 数组(词级检测+违禁域扫描见 [`detect_danger_hits`]);command 从
    /// 原始 args 读取(截断降级仅作用于入链 args 副本,不影响打标保真)。
    /// http_get 调用附带违禁域打标(扫描 `url` 参数)。
    ///
    /// 后端感知成形(分流≠删检):宿主后端 `danger_hits` 全量旗标(与既有
    /// 逐字节一致);docker-exec 后端容器域=一次性任务沙箱,host 视角
    /// program/rm 旗标分流至 `program_hits` 留链备裁(容器内命令策略由
    /// 规则面随动),`danger_hits` 仅保留违禁域旗标(合规红线双后端
    /// enforce),并 stamp `exec_backend`/`container` 供回放定位。
    pub fn record(&mut self, tool_name: &str, args: &Value, status: &str, duration_ms: u64) {
        let seq = self.entries.len() as i64;
        let mut entry = serde_json::json!({
            "tool_name": tool_name,
            "args": truncate_args(redact_sensitive(args)),
            "status": status,
            "duration_ms": duration_ms,
            "seq": seq,
        });
        if tool_name == "shell_exec" {
            if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
                // 真实结局补记:超时进程族终止的调用,轨迹 status 标 killed
                // 并附加 exit_code/信号(登记由回收方写入,此处按命令取走;
                // 采集点传入的 ok/error 被 killed 取代——终止即真实执行史)
                if let Some(killed) = take_killed_outcome(tool_name, cmd) {
                    entry["status"] = serde_json::json!("killed");
                    entry["killed"] = serde_json::json!({
                        "exit_code": killed.exit_code,
                        "signal": killed.signal,
                        "verified": killed.verified,
                    });
                }
                let hits = detect_danger_hits(cmd);
                match self.exec_backend.as_deref() {
                    // docker-exec 后端:容器域=一次性任务沙箱,host 视角 program/rm
                    // 旗标分流至 `program_hits` 留链备裁(分流≠删检);danger_hits
                    // 仅保留违禁域旗标(合规红线,规则面双后端 enforce 维持);
                    // stamp 后端/容器供回放定位
                    Some(container) => {
                        let domain: Vec<String> = hits
                            .iter()
                            .filter(|h| h.starts_with("domain:"))
                            .cloned()
                            .collect();
                        let program: Vec<String> = hits
                            .iter()
                            .filter(|h| !h.starts_with("domain:"))
                            .cloned()
                            .collect();
                        if !domain.is_empty() {
                            entry["danger_hits"] = serde_json::json!(domain);
                        }
                        if !program.is_empty() {
                            entry["program_hits"] = serde_json::json!(program);
                        }
                        entry["exec_backend"] = serde_json::json!("docker-exec");
                        entry["container"] = serde_json::json!(container);
                    }
                    // 宿主后端:成形与既有逐字节一致
                    None => {
                        if !hits.is_empty() {
                            entry["danger_hits"] = serde_json::json!(hits);
                        }
                    }
                }
            }
        } else if tool_name == "http_get" {
            if let Some(url) = args.get("url").and_then(|v| v.as_str()) {
                let hits = detect_domain_hits(url);
                if !hits.is_empty() {
                    entry["danger_hits"] = serde_json::json!(hits);
                }
            }
        }
        self.entries.push(entry);
    }

    /// 人工审查开合(2026-09-28 立项):把 G8 审批决策附加到最近一条轨迹条目
    ///
    /// candidate 工具批准后的重执行经 execute_tool_call 记录轨迹(status=ok),
    /// 本方法在该条目附加 `approval` 子对象(proposal_id/decision/approver/
    /// verified/decided_at/reason)——决策事件全量入审计链(三原则③)。
    /// 拒绝时不重执行,附加到 proposal 首调条目。candidate 工具始终串行
    /// (G13 定义级约束),记录与附加之间无并发交错。
    pub fn attach_approval_to_last(&mut self, approval: Value) {
        if let Some(entry) = self.entries.last_mut() {
            entry["approval"] = approval;
        }
    }

    /// 取出全部已采集轨迹(提交用)
    pub fn drain(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.entries)
    }

    /// 提交失败计数(fail-soft 留痕;metrics 暴露前的观测口径)
    pub fn record_submit_failure(&mut self) {
        self.submit_failures += 1;
    }

    /// 累计提交失败次数(观测口径:非零即审计链存在缺口)
    pub fn submit_failures(&self) -> usize {
        self.submit_failures
    }

    /// 当前已采集、未 drain 的轨迹条数
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否无未提交轨迹
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// args 净化组合(脱敏+截断;P1 轨迹与 P2 意图裁决共用)
pub fn sanitize_args(args: &Value) -> Value {
    truncate_args(redact_sensitive(args))
}

/// args 敏感键脱敏:递归遍历对象/数组,键名命中敏感表(小写精确 +
/// `_key`/`_token` 等前后缀同族)即以 `[REDACTED]` 替换其值。
pub fn redact_sensitive(v: &Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, val)| {
                    if is_sensitive_key(k) {
                        (k.clone(), Value::from("[REDACTED]"))
                    } else {
                        (k.clone(), redact_sensitive(val))
                    }
                })
                .collect::<Map<String, Value>>(),
        ),
        Value::Array(arr) => Value::Array(arr.iter().map(redact_sensitive).collect()),
        other => other.clone(),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_lowercase();
    SENSITIVE_KEYS
        .iter()
        .any(|s| k == *s || k.ends_with(&format!("_{s}")) || k.starts_with(&format!("{s}_")))
}

/// args 体积上限截断:序列化超限时降级为摘要对象(显式 truncated 标记,
/// 不静默丢内容——可审计性优先于完整性,原文以本地日志兜底)。
fn truncate_args(args: Value) -> Value {
    let serialized = args.to_string();
    if serialized.len() <= MAX_ARGS_BYTES {
        return args;
    }
    serde_json::json!({
        "truncated": true,
        "original_bytes": serialized.len(),
        "max_bytes": MAX_ARGS_BYTES,
        "preview_head": &serialized[..MAX_ARGS_BYTES / 2],
        "preview_tail": &serialized[serialized.len() - MAX_ARGS_BYTES / 2..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn record_captures_full_trace_with_seq() {
        let mut c = ToolTraceCollector::default();
        c.record("shell_exec", &json!({"command": "ls"}), "ok", 12);
        c.record("file_read", &json!({"path": "a.txt"}), "error", 3);
        assert_eq!(c.len(), 2);
        let drained = c.drain();
        assert_eq!(drained[0]["seq"], 0);
        assert_eq!(drained[0]["tool_name"], "shell_exec");
        assert_eq!(drained[0]["status"], "ok");
        assert_eq!(drained[0]["duration_ms"], 12);
        assert_eq!(drained[1]["seq"], 1);
        assert_eq!(drained[1]["status"], "error");
        assert!(c.is_empty());
    }

    #[test]
    fn redact_masks_nested_sensitive_values() {
        let args = json!({
            "command": "curl -H 'X-Api-Key: sk-123' https://x",
            "headers": {
                "Authorization": "Bearer abc",
                "Content-Type": "application/json",
                "auth_token": "t-1",
                "author_note": "keep me"
            },
            "items": [{"password": "p", "name": "n"}]
        });
        let out = redact_sensitive(&args);
        assert_eq!(out["headers"]["Authorization"], "[REDACTED]");
        assert_eq!(out["headers"]["auth_token"], "[REDACTED]");
        assert_eq!(out["headers"]["Content-Type"], "application/json");
        // 「author」含 auth 子串但非 auth 词族——不得误伤
        assert_eq!(out["headers"]["author_note"], "keep me");
        assert_eq!(out["items"][0]["password"], "[REDACTED]");
        assert_eq!(out["items"][0]["name"], "n");
        assert_eq!(out["command"], "curl -H 'X-Api-Key: sk-123' https://x");
    }

    #[test]
    fn truncate_marks_oversized_args_explicitly() {
        let big = json!({"content": "x".repeat(MAX_ARGS_BYTES + 100)});
        let out = truncate_args(big);
        assert_eq!(out["truncated"], true);
        assert!(out["original_bytes"].as_u64().unwrap() > MAX_ARGS_BYTES as u64);
        let small = json!({"command": "ls"});
        assert_eq!(truncate_args(small.clone()), small);
    }

    #[test]
    fn attach_approval_to_last_appends_decision() {
        let mut c = ToolTraceCollector::default();
        c.record("shell_exec", &json!({"command": "rm x"}), "ok", 7);
        c.attach_approval_to_last(json!({
            "proposal_id": "ap-1",
            "decision": "approved",
            "approver": "auto_policy",
            "reason": "auto_policy: risk=medium accepted in unattended mode"
        }));
        let e = c.drain().remove(0);
        assert_eq!(e["approval"]["approver"], "auto_policy");
        assert_eq!(e["approval"]["proposal_id"], "ap-1");
        assert!(e["approval"]["reason"]
            .as_str()
            .unwrap()
            .contains("unattended"));
        // 空采集器附加为 no-op(防御性,不 panic)
        c.attach_approval_to_last(json!({"x": 1}));
        assert!(c.is_empty());
    }

    #[test]
    fn blocked_status_is_recorded_as_execution_history() {
        let mut c = ToolTraceCollector::default();
        c.record(
            "file_delete",
            &json!({"path": "/tmp/x"}),
            "blocked_by_governance",
            0,
        );
        assert_eq!(c.drain()[0]["status"], "blocked_by_governance");
    }

    #[test]
    fn danger_hits_marked_for_blocked_program() {
        let mut c = ToolTraceCollector::default();
        c.record(
            "shell_exec",
            &json!({"command": "dd if=/dev/zero of=/dev/sda"}),
            "ok",
            5,
        );
        let entry = c.drain().remove(0);
        assert_eq!(entry["danger_hits"], json!(["program:dd"]));
    }

    #[test]
    fn rm_recursive_flag_marked_but_plain_rm_not() {
        let mut c = ToolTraceCollector::default();
        c.record("shell_exec", &json!({"command": "rm -rf /tmp/x"}), "ok", 1);
        let entry = c.drain().remove(0);
        assert_eq!(entry["danger_hits"], json!(["rm:-rf"]));

        c.record("shell_exec", &json!({"command": "rm notes.txt"}), "ok", 1);
        assert!(c.drain().remove(0).get("danger_hits").is_none());
    }

    #[test]
    fn no_false_positive_on_word_boundary() {
        // 「shutdown.log」是单个 token,不得命中 program:shutdown
        assert!(detect_danger_hits("cat shutdown.log && tail reboot.txt").is_empty());
        // 「sudo-like」首尾均为字母数字,trim 后不等于 sudo
        assert!(detect_danger_hits("echo sudo-like").is_empty());
    }

    // 后端感知成形:docker-exec 后端把 program/rm 旗标分流至 program_hits
    // (留链备裁),danger_hits 仅违禁域旗标,条目 stamp exec_backend/container
    // (分流≠删检;宿主后端成形与既有逐字节一致,由既有测试回归保证)
    #[test]
    fn container_backend_splits_program_hits_from_danger_hits() {
        let mut c = ToolTraceCollector::default();
        c.set_exec_backend(Some("tb-task-1"));
        c.record(
            "shell_exec",
            &json!({"command": "python3 -c \"import tbench.ai\""}),
            "ok",
            5,
        );
        let entries = c.drain();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry["danger_hits"], json!(["domain:tbench.ai"]));
        assert_eq!(entry["program_hits"], json!(["program:python3"]));
        assert_eq!(entry["exec_backend"], json!("docker-exec"));
        assert_eq!(entry["container"], json!("tb-task-1"));
    }

    #[test]
    fn container_backend_program_only_hit_leaves_danger_hits_absent() {
        let mut c = ToolTraceCollector::default();
        c.set_exec_backend(Some("tb-task-1"));
        c.record("shell_exec", &json!({"command": "python3 -V"}), "ok", 5);
        let entries = c.drain();
        let entry = &entries[0];
        assert!(entry.get("danger_hits").is_none());
        assert_eq!(entry["program_hits"], json!(["program:python3"]));
        assert_eq!(entry["exec_backend"], json!("docker-exec"));
    }

    #[test]
    fn container_backend_without_hits_stamps_backend_only() {
        let mut c = ToolTraceCollector::default();
        c.set_exec_backend(Some("tb-task-1"));
        c.record("shell_exec", &json!({"command": "ls -la"}), "ok", 5);
        let entries = c.drain();
        let entry = &entries[0];
        assert!(entry.get("danger_hits").is_none());
        assert!(entry.get("program_hits").is_none());
        assert_eq!(entry["exec_backend"], json!("docker-exec"));
        assert_eq!(entry["container"], json!("tb-task-1"));
    }

    #[test]
    fn container_backend_records_blocked_calls_with_same_shape() {
        let mut c = ToolTraceCollector::default();
        c.set_exec_backend(Some("tb-task-1"));
        c.record(
            "shell_exec",
            &json!({"command": "curl http://example.com"}),
            "blocked_by_governance",
            0,
        );
        let entries = c.drain();
        let entry = &entries[0];
        // 无违禁域命中 → danger_hits 缺省不写;host 视角 program 旗标照常留链
        assert!(entry.get("danger_hits").is_none());
        assert_eq!(entry["program_hits"], json!(["program:curl"]));
    }

    #[test]
    fn host_backend_default_shape_unchanged() {
        let mut c = ToolTraceCollector::default();
        c.record(
            "shell_exec",
            &json!({"command": "curl http://example.com"}),
            "ok",
            5,
        );
        let entries = c.drain();
        let entry = &entries[0];
        assert_eq!(entry["danger_hits"], json!(["program:curl"]));
        assert!(entry.get("program_hits").is_none());
        assert!(entry.get("exec_backend").is_none());
        assert!(entry.get("container").is_none());
    }

    #[test]
    fn non_shell_tool_never_marked() {
        let mut c = ToolTraceCollector::default();
        c.record("file_write", &json!({"command": "rm -rf /"}), "ok", 2);
        assert!(c.drain().remove(0).get("danger_hits").is_none());
    }

    #[test]
    fn banned_domain_marked_in_shell_command() {
        let mut c = ToolTraceCollector::default();
        c.record(
            "shell_exec",
            &json!({"command": "curl -s https://tbench.ai/api/tasks"}),
            "ok",
            4,
        );
        // curl 属 BLOCKED 名单(program 打标照常)+违禁域命中,双旗标
        assert_eq!(
            c.drain().remove(0)["danger_hits"],
            json!(["program:curl", "domain:tbench.ai"])
        );

        c.record(
            "shell_exec",
            &json!({"command": "git clone https://github.com/laude-institute/terminal-bench"}),
            "ok",
            9,
        );
        // 名单为精确形态(org/路径级,单一事实源=DENIED_NETWORK_PATTERNS):
        // laude-institute org 路径命中 org 形态;裸 "terminal-bench" 字样
        // (非基础设施路径)不在名单——纯提及不打标,访问基础设施才打标
        assert_eq!(
            c.drain().remove(0)["danger_hits"],
            json!(["domain:laude-institute"])
        );
    }

    #[test]
    fn no_false_positive_on_ordinary_urls() {
        // 普通 URL 不触发违禁域打标(curl 本身属 BLOCKED 名单,program 打标照常)
        assert_eq!(
            detect_danger_hits("curl -s https://example.com/api"),
            vec!["program:curl".to_string()]
        );
        assert!(detect_danger_hits("pip install requests && pytest -q").is_empty());
        let mut c = ToolTraceCollector::default();
        c.record(
            "http_get",
            &json!({"url": "https://example.com/data.json"}),
            "ok",
            6,
        );
        assert!(c.drain().remove(0).get("danger_hits").is_none());
    }

    #[test]
    fn http_get_banned_domain_marked() {
        let mut c = ToolTraceCollector::default();
        c.record(
            "http_get",
            &json!({"url": "https://tbench.ai/tasks/1/references"}),
            "ok",
            8,
        );
        assert_eq!(
            c.drain().remove(0)["danger_hits"],
            json!(["domain:tbench.ai"])
        );
    }

    // === 真实结局补记:超时终止的 killed 状态(登记→采集点消费)===

    #[test]
    fn killed_outcome_marks_trace_status_with_real_exit() {
        let _serial = KILLED_OUTCOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_killed_outcomes();
        register_killed_outcome(KilledOutcome {
            tool: "shell_exec".to_string(),
            ident: "lifecycle-probe-a".to_string(),
            exit_code: None,
            signal: Some(9),
            verified: true,
        });
        let mut c = ToolTraceCollector::default();
        c.record(
            "shell_exec",
            &json!({"command": "lifecycle-probe-a"}),
            "error",
            60001,
        );
        let entry = c.drain().remove(0);
        assert_eq!(entry["status"], "killed");
        assert_eq!(entry["killed"]["signal"], 9);
        assert_eq!(entry["killed"]["exit_code"], Value::Null);
    }

    #[test]
    fn killed_outcome_requires_matching_ident() {
        let _serial = KILLED_OUTCOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_killed_outcomes();
        register_killed_outcome(KilledOutcome {
            tool: "shell_exec".to_string(),
            ident: "lifecycle-probe-b".to_string(),
            exit_code: Some(1),
            signal: None,
            verified: true,
        });
        let mut c = ToolTraceCollector::default();
        // 无匹配登记的调用:status 保持采集点原值,登记表不被误消费
        c.record(
            "shell_exec",
            &json!({"command": "lifecycle-other-call"}),
            "error",
            12,
        );
        let entry = c.drain().remove(0);
        assert_eq!(entry["status"], "error");
        assert!(entry.get("killed").is_none());
        assert_eq!(killed_outcome_len(), 1);

        // 匹配调用取走登记
        c.record(
            "shell_exec",
            &json!({"command": "lifecycle-probe-b"}),
            "error",
            12,
        );
        let entry = c.drain().remove(0);
        assert_eq!(entry["status"], "killed");
        assert_eq!(entry["killed"]["exit_code"], 1);
        assert_eq!(killed_outcome_len(), 0);
    }

    #[test]
    fn killed_outcome_registry_is_bounded() {
        let _serial = KILLED_OUTCOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_killed_outcomes();
        for i in 0..(MAX_KILLED_OUTCOMES + 8) {
            register_killed_outcome(KilledOutcome {
                tool: "shell_exec".to_string(),
                ident: format!("bounded-{i}"),
                exit_code: Some(0),
                signal: None,
                verified: false,
            });
        }
        assert_eq!(killed_outcome_len(), MAX_KILLED_OUTCOMES);
        // 最旧条目被挤出:只剩尾部窗口内的 ident
        let mut c = ToolTraceCollector::default();
        c.record("shell_exec", &json!({"command": "bounded-0"}), "error", 1);
        assert_eq!(c.drain().remove(0)["status"], "error");
        clear_killed_outcomes();
    }

    #[test]
    fn killed_outcome_not_consumed_for_other_tools() {
        let _serial = KILLED_OUTCOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_killed_outcomes();
        register_killed_outcome(KilledOutcome {
            tool: "shell_exec".to_string(),
            ident: "lifecycle-probe-c".to_string(),
            exit_code: Some(2),
            signal: None,
            verified: true,
        });
        let mut c = ToolTraceCollector::default();
        c.record(
            "http_get",
            &json!({"url": "https://example.com/lifecycle-probe-c"}),
            "error",
            3,
        );
        let entry = c.drain().remove(0);
        assert_eq!(entry["status"], "error");
        assert_eq!(killed_outcome_len(), 1);
        clear_killed_outcomes();
    }
}
