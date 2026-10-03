//! 16 号档 D2:验收判据自检门禁(TB/长程模式)。
//!
//! `task_done` 提交前 runner 强制执行验收命令(`acceptance_command`,
//! definition 配置):exit 0=判据通过放行,非 0=门禁拒绝——**判据不过
//! 不存在 done 退出路径**(与北极星 lh-r1 规则门串联:规则=决策数据,
//! 本模块=runner 侧机制)。`task_blocked` 需非空 reason(fail-closed)。
//! 其余指令原样放行;未配置命令时 task_done 也放行(非长程运行零影响)。
//!
//! 属性契约(与 D1 规则包 08-northstar-pack-longhorizon-d1 对接):
//! - 注入 `params.acceptance_passed`(强制由 runner 填写,不采信 LLM 自报,
//!   G8 同款决策权归 runner)+ 失败时 `params.acceptance_detail`;
//! - 拒绝时 `GateOutcome::Reject(detail)` 由调用方作为 tool_result 回喂
//!   LLM,指令不提交引擎。

use serde_json::Value;

/// 门禁结果
pub enum GateOutcome {
    /// 放行(args 可能已被注入 acceptance_passed/acceptance_detail)
    Allow(Value),
    /// 拒绝(携带详情;调用方作为 tool_result 回喂 LLM,不提交指令)
    Reject(String),
}

/// 判据自检门禁入口
pub async fn apply_acceptance_gate(acceptance_command: Option<&str>, args: &Value) -> GateOutcome {
    let itype = args
        .get("instruction_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match itype {
        "task_done" => {
            let Some(cmd) = acceptance_command else {
                return GateOutcome::Allow(args.clone());
            };
            let outcome = run_acceptance_command(cmd).await;
            let passed = matches!(&outcome, Ok(o) if o.status.success());
            let detail = match &outcome {
                Err(e) => e.clone(),
                Ok(o) => format!(
                    "exit={:?} stdout_tail={} stderr_tail={}",
                    o.status.code(),
                    tail_str(&String::from_utf8_lossy(&o.stdout), 400),
                    tail_str(&String::from_utf8_lossy(&o.stderr), 400),
                ),
            };
            let mut args = args.clone();
            if let Some(obj) = args.as_object_mut() {
                let params = obj.entry("params").or_insert_with(|| serde_json::json!({}));
                if let Some(p) = params.as_object_mut() {
                    // G8 同款:验收结果由 runner 强制填写,不采信 LLM 自报
                    p.insert("acceptance_passed".to_string(), serde_json::json!(passed));
                    if !passed {
                        p.insert("acceptance_detail".to_string(), serde_json::json!(detail));
                    }
                }
            }
            if passed {
                GateOutcome::Allow(args)
            } else {
                GateOutcome::Reject(format!(
                    "验收判据未通过({detail})——task_done 已被门禁拒绝:判据不过不存在 done 退出路径"
                ))
            }
        }
        "task_blocked" => {
            let reason = args
                .get("params")
                .and_then(|p| p.get("reason"))
                .and_then(|r| r.as_str())
                .map(str::trim)
                .unwrap_or("");
            if reason.is_empty() {
                GateOutcome::Reject(
                    "task_blocked 缺少必填 params.reason(fail-closed):诚实退出也必须留下可审计的原因分类"
                        .to_string(),
                )
            } else {
                GateOutcome::Allow(args.clone())
            }
        }
        _ => GateOutcome::Allow(args.clone()),
    }
}

/// 执行验收命令(windows=cmd /C,其余=sh -c;tokio 异步执行不阻塞执行器线程)。
/// spawn 失败按"未通过"处理(Err 携带原因)。
async fn run_acceptance_command(cmd: &str) -> Result<std::process::Output, String> {
    #[cfg(windows)]
    let mut command = {
        let mut c = tokio::process::Command::new("cmd");
        c.args(["/C", cmd]);
        c
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    command
        .output()
        .await
        .map_err(|e| format!("acceptance command spawn failed: {e}"))
}

/// 取字符串尾部 n 个字符(字符边界安全)
fn tail_str(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut start = s.len() - n;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_gate_passes_on_exit_zero() {
        let args = serde_json::json!({
            "instruction_type": "task_done",
            "params": { "milestone_current": 3 }
        });
        match apply_acceptance_gate(Some("exit 0"), &args).await {
            GateOutcome::Allow(a) => {
                assert_eq!(a["params"]["acceptance_passed"], serde_json::json!(true));
                // 原有 params 字段保留
                assert_eq!(a["params"]["milestone_current"], serde_json::json!(3));
            }
            GateOutcome::Reject(d) => panic!("should allow: {d}"),
        }
    }

    #[tokio::test]
    async fn test_gate_rejects_on_nonzero_exit() {
        let args = serde_json::json!({
            "instruction_type": "task_done",
            "params": {}
        });
        match apply_acceptance_gate(Some("exit 3"), &args).await {
            GateOutcome::Reject(d) => {
                assert!(d.contains("验收判据未通过"));
                // 拒绝不改写原 args 的语义:注入发生在 Allow 路径,此处 detail 已含原因
            }
            GateOutcome::Allow(_) => panic!("non-zero exit must reject"),
        }
    }

    #[tokio::test]
    async fn test_gate_unconfigured_allows_task_done() {
        let args = serde_json::json!({ "instruction_type": "task_done", "params": {} });
        assert!(matches!(
            apply_acceptance_gate(None, &args).await,
            GateOutcome::Allow(_)
        ));
    }

    #[tokio::test]
    async fn test_gate_task_blocked_requires_reason() {
        let no_reason = serde_json::json!({ "instruction_type": "task_blocked", "params": {} });
        assert!(matches!(
            apply_acceptance_gate(None, &no_reason).await,
            GateOutcome::Reject(_)
        ));
        let with_reason = serde_json::json!({
            "instruction_type": "task_blocked",
            "params": { "reason": "deps unavailable, fail-visible" }
        });
        assert!(matches!(
            apply_acceptance_gate(None, &with_reason).await,
            GateOutcome::Allow(_)
        ));
    }

    #[tokio::test]
    async fn test_gate_passthrough_other_instructions() {
        let args = serde_json::json!({ "instruction_type": "task_start", "params": {} });
        assert!(matches!(
            apply_acceptance_gate(Some("exit 1"), &args).await,
            GateOutcome::Allow(_)
        ));
    }
}
