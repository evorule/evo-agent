//! 验收判据自检门禁(TB/长程模式)。
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

/// 取字符串尾部 n 个字符(字符边界安全;workflow 判据门禁复用)
pub(crate) fn tail_str(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut start = s.len() - n;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

// ===== G4 判据生成(先写后锁/外部锚优先/生成执行解耦;执行半边=本文件既有机制层) =====

/// 生成判据的提示模板(外部锚优先:强制 shell 命令形态,禁自由文本自评)
pub(crate) fn build_criteria_prompt(goal: &str, tools: &[String]) -> String {
    let tool_list = if tools.is_empty() {
        "(工具清单不可用)".to_string()
    } else {
        tools.join(", ")
    };
    format!(
        "你是验收判据设计器。为下述任务产出**一条 shell 验收命令**,供自动化门禁在 \
         agent 声称完成时执行:exit 0 = 验收通过,非 0 = 拒绝。\n\n\
         任务目标:\n{goal}\n\n\
         可用工具面(判据只应使用 shell 可观测的外部事实,而非重新执行任务):\n{tool_list}\n\n\
         硬性要求:\n\
         1. 只输出一条命令,单行,不换行,不用 markdown 代码块,无任何解释文字;\n\
         2. 优先外部锚:产物文件存在性/内容断言(test/grep)、退出码、HTTP 探针 \
            (curl -fsS)、输出断言——不要让判据依赖 agent 的自我报告;\n\
         3. 判据必须可重复执行且无副作用(只读检查;不得修改/删除任务产物);\n\
         4. 判据不要重新执行任务本身,只验证其产物;\n\
         5. 无法设计可执行判据时,输出以 echo 开头的最诚实降级判据 \
            (如 `echo PENDING-HUMAN-REVIEW && exit 1`),保持单行。\n\n\
         示例形态:test -f report.md && grep -q \"总销售额\" report.md"
    )
}

/// 校验生成的判据(外部锚优先的结构保证:非命令形态/超长/多行一律拒绝)
pub fn validate_generated_criteria(cmd: &str) -> Result<(), String> {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return Err("判据为空".to_string());
    }
    if trimmed.contains('\n') || trimmed.contains('\r') {
        return Err("判据必须单行(多语句用 && 串联)".to_string());
    }
    if trimmed.len() > 2048 {
        return Err(format!("判据超长({} 字节 > 2048)", trimmed.len()));
    }
    Ok(())
}

/// 从 LLM 响应中提取判据文本(剥 markdown 围栏与语言标签行;多行时取首个
/// 非空行——容错模型惯性,后续仍过 validate 单行校验)
pub(crate) fn extract_criteria(raw: &str) -> String {
    let mut out = raw.trim().to_string();
    if let Some(rest) = out.strip_prefix("```") {
        let rest = rest.trim_start_matches(['\r', '\n']);
        out = match rest.rfind("```") {
            Some(end) => rest[..end].trim().to_string(),
            None => rest.trim().to_string(),
        };
    }
    let mut lines = out.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = match lines.next() {
        Some(l) => l,
        None => return String::new(),
    };
    // 裸语言标签行(如 ```sh 残留的 "sh"):真实 shell 判据必然含空格/路径/操作符
    if first.len() <= 10 && first.chars().all(|c| c.is_ascii_alphabetic()) {
        return lines.next().unwrap_or("").to_string();
    }
    first.to_string()
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

    // ===== G4 判据生成纯函数 =====

    #[test]
    fn test_validate_criteria_accepts_command() {
        assert!(validate_generated_criteria("test -f report.md").is_ok());
        assert!(
            validate_generated_criteria("  exit 0  ").is_ok(),
            "首尾空白放行(trim 语义)"
        );
        let edge = "x".repeat(2048);
        assert!(validate_generated_criteria(&edge).is_ok(), "2048 边界放行");
    }

    #[test]
    fn test_validate_criteria_rejects_bad_shapes() {
        assert!(validate_generated_criteria("").is_err(), "空判据拒绝");
        assert!(validate_generated_criteria("   \t ").is_err(), "纯空白拒绝");
        assert!(validate_generated_criteria("a\nb").is_err(), "多行拒绝");
        assert!(validate_generated_criteria("a\r\nb").is_err(), "CRLF 拒绝");
        let long = "x".repeat(2049);
        assert!(validate_generated_criteria(&long).is_err(), "超长拒绝");
    }

    #[test]
    fn test_extract_criteria_variants() {
        assert_eq!(extract_criteria("test -f a.md"), "test -f a.md");
        // 围栏与语言标签行剥离
        assert_eq!(extract_criteria("```\ntest -f a.md\n```"), "test -f a.md");
        assert_eq!(extract_criteria("```sh\ntest -f a.md\n```"), "test -f a.md");
        assert_eq!(extract_criteria("```rust\ncargo build\n```"), "cargo build");
        assert_eq!(
            extract_criteria("```sh\ntest -f a.md"),
            "test -f a.md",
            "未闭合围栏容错"
        );
        // 多行:取首个非空行(容错;整体仍受单行校验约束)
        assert_eq!(extract_criteria("test -f a.md\n这是解释"), "test -f a.md");
        assert_eq!(
            extract_criteria("  \n  \n"),
            "",
            "全空白返回空串(交由 validate 拒绝)"
        );
    }

    #[test]
    fn test_criteria_prompt_contains_goal_and_anchors() {
        let p = build_criteria_prompt(
            "生成月报",
            &["file_read".to_string(), "shell_exec".to_string()],
        );
        assert!(p.contains("生成月报"));
        assert!(p.contains("file_read"));
        assert!(p.contains("单行"));
        assert!(p.contains("外部锚"));
        // 空工具面也成立
        assert!(build_criteria_prompt("g", &[]).contains("工具清单不可用"));
    }
}
