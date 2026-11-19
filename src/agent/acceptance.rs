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
//!
//! Write-off conjunction extension (long-horizon mode): when the caller
//! hands in the journal-projected pieces
//! ([`crate::agent::writeoff::JournalPieces`]), the `task_done` admission
//! condition upgrades from the criteria piece alone to the three-piece
//! conjunction (all criteria passed AND window evidence committed AND
//! breakpoint fresh). The three-valued verdict Pass/Reject/Unknown comes
//! from the pure functions in [`crate::agent::writeoff`]; both Reject and
//! Unknown refuse the submission (Unknown never pretends to pass, same
//! discipline as the verify gate's `unknown` verdict), with every missing
//! piece itemized in the detail. Passing None (non-long-horizon mode)
//! keeps the legacy criteria-only behavior byte-identical (default
//! invariant).
//!
//! Acceptance evidence extension (long-horizon mode): when the caller
//! hands in the runner-projected touch report
//! ([`crate::agent::acceptance_evidence::TouchReport`]), two more
//! machine faces join the admission decision. First, the evidence
//! spot-check runs before the criteria command: recorded digests are
//! recomputed against the extant checkpoint blob bytes, and any mismatch
//! or missing blob refuses the submission without spending the criteria
//! budget (a tampered ledger face is a refusal, not a pass). Second, the
//! criteria command is executed twice and both stdout bytes and exit
//! code must be bit-identical — a divergence means the criterion is not
//! repeatable and the submission is honestly refused (discipline, not
//! defect). Every `task_done` refusal additionally names the unreferenced
//! touch paths when the model's anchors leave part of the projected touch
//! surface uncovered (report credibility signal). Passing a None report
//! (non-long-horizon mode) keeps behavior byte-identical.

use serde_json::Value;

use crate::agent::acceptance_evidence::{self, IntegrityVerdict, TouchReport};
use crate::agent::writeoff::{self, CriteriaState, JournalPieces, WriteoffVerdict};

/// 门禁结果
pub enum GateOutcome {
    /// 放行(args 可能已被注入 acceptance_passed/acceptance_detail)
    Allow(Value),
    /// 拒绝(携带详情;调用方作为 tool_result 回喂 LLM,不提交指令)
    Reject(String),
}

/// 判据自检门禁入口
///
/// `writeoff_pieces`: journal-projected conjunction pieces (computed by
/// the runner for long-horizon mode; the model never supplies them). None
/// = non-long-horizon mode, legacy criteria-only behavior. When Some, the
/// `task_done` admission upgrades to the three-piece conjunction and both
/// Reject and Unknown refuse the submission with the itemized detail.
///
/// `runner_report`: runner-projected acceptance evidence (touch surface,
/// coverage, journal-anchor coverage, evidence spot-check verdict; also
/// runner-computed, model never supplies it). None = the faces were not
/// built (non-long-horizon mode) — no dual run, no spot-check, byte
/// identical legacy behavior. When Some on a `task_done`, the spot-check
/// verdict joins the admission decision, the criteria command runs twice
/// under the bit-identical discipline, and every refusal names the
/// unreferenced touch paths when coverage is incomplete.
pub async fn apply_acceptance_gate(
    acceptance_command: Option<&str>,
    args: &Value,
    writeoff_pieces: Option<&JournalPieces>,
    runner_report: Option<&TouchReport>,
) -> GateOutcome {
    let itype = args
        .get("instruction_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match itype {
        "task_done" => {
            // Evidence spot-check first (cheap read-only IO): recorded
            // digests are recomputed against the extant checkpoint blobs,
            // so a tampered ledger face refuses admission without
            // spending the criteria command budget. The model cannot
            // influence this face (runner-projected from the manifest
            // SSOT), and the refusal names every failing artifact.
            if let Some(report) = runner_report {
                if let IntegrityVerdict::Reject { faces } = &report.integrity {
                    return GateOutcome::Reject(format!(
                        "task_done refused by evidence recomputation (fail-closed): {} — recorded ledger digests no longer match the extant artifacts",
                        faces.join("; ")
                    ) + &acceptance_evidence::coverage_rejection_line(report));
                }
            }
            let Some(cmd) = acceptance_command else {
                // Conjunction mode without a configured command: the
                // criteria piece cannot be established, so the conjunction
                // evaluation reports it honestly (unknown is not pass).
                // Non-conjunction mode keeps the legacy allow.
                if let Some(pieces) = writeoff_pieces {
                    let verdict =
                        writeoff::evaluate_conjunction(CriteriaState::Unconfigured, pieces);
                    return GateOutcome::Reject(append_coverage(
                        writeoff_refusal(&verdict),
                        runner_report,
                    ));
                }
                return GateOutcome::Allow(args.clone());
            };
            // Long-horizon mode runs the criterion command twice: stdout
            // bytes + exit code must be bit-identical (a repeatable,
            // side-effect-free criterion produces identical faces; a
            // divergence means the criterion is not deterministic and the
            // submission is honestly refused — discipline, not defect).
            // Non-long-horizon mode keeps the single legacy run.
            let (passed, detail) = if writeoff_pieces.is_some() {
                let first = run_acceptance_command(cmd).await;
                let second = run_acceptance_command(cmd).await;
                match (first, second) {
                    (Ok(a), Ok(b)) => {
                        let describe = |o: &std::process::Output| {
                            format!(
                                "exit={:?} stdout_tail={}",
                                o.status.code(),
                                tail_str(&String::from_utf8_lossy(&o.stdout), 400)
                            )
                        };
                        if acceptance_evidence::dual_run_bit_identical(
                            a.status.code(),
                            &a.stdout,
                            b.status.code(),
                            &b.stdout,
                        ) {
                            let passed = a.status.success();
                            let detail = if passed {
                                String::new()
                            } else {
                                format!("exit={:?}", a.status.code())
                            };
                            (passed, detail)
                        } else {
                            (
                                false,
                                format!(
                                    "acceptance criteria non-deterministic: dual-run outputs diverged (run1: {} | run2: {}) — a done-admission criterion must be repeatable; this is discipline, not a defect",
                                    describe(&a),
                                    describe(&b)
                                ),
                            )
                        }
                    }
                    (Err(e1), Err(e2)) => (
                        false,
                        format!("acceptance command failed on both runs: {e1} | {e2}"),
                    ),
                    (Err(e), _) | (_, Err(e)) => (false, format!("acceptance command failed: {e}")),
                }
            } else {
                let outcome = run_acceptance_command(cmd).await;
                match &outcome {
                    Err(e) => (false, e.clone()),
                    Ok(o) => {
                        let passed = o.status.success();
                        let detail = format!(
                            "exit={:?} stdout_tail={} stderr_tail={}",
                            o.status.code(),
                            tail_str(&String::from_utf8_lossy(&o.stdout), 400),
                            tail_str(&String::from_utf8_lossy(&o.stderr), 400),
                        );
                        (passed, detail)
                    }
                }
            };
            if let Some(pieces) = writeoff_pieces {
                // Conjunction mode: the criteria result enters the
                // three-piece verdict; both Reject and Unknown refuse the
                // submission without injecting the pass flag (unknown is
                // not pass). Injection happens only on the Allow path
                // after the full conjunction passed.
                let criteria = if passed {
                    CriteriaState::Passed
                } else {
                    CriteriaState::Failed(detail.clone())
                };
                let verdict = writeoff::evaluate_conjunction(criteria, pieces);
                if !matches!(verdict, WriteoffVerdict::Pass) {
                    return GateOutcome::Reject(append_coverage(
                        writeoff_refusal(&verdict),
                        runner_report,
                    ));
                }
            }
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
                GateOutcome::Reject(append_coverage(
                    format!(
                        "验收判据未通过({detail})——task_done 已被门禁拒绝:判据不过不存在 done 退出路径"
                    ),
                    runner_report,
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

/// Build the refusal detail for a non-Pass write-off verdict. Both verdict
/// classes refuse the submission; the wording states which (a plain reject
/// vs an unknowable face that is not treated as pass) and the verdict
/// payload already itemizes every failing/unknowable piece.
fn writeoff_refusal(verdict: &WriteoffVerdict) -> String {
    match verdict {
        WriteoffVerdict::Pass => String::new(), // unreachable: caller guards on non-Pass
        WriteoffVerdict::Reject(detail) => format!(
            "task_done 已被合取门禁拒绝(缺项明细:{detail})——判据通过、证据落盘、断点块新鲜三件未全过,不存在 done 退出路径"
        ),
        WriteoffVerdict::Unknown(detail) => format!(
            "task_done 已被合取门禁拒绝(unknown 不假装通过:{detail})——无法确证的合取件按拒绝处理,不存在 done 退出路径"
        ),
    }
}

/// Append the unreferenced-touch-paths line to a `task_done` refusal when
/// the runner report shows a coverage deficit (report credibility signal:
/// the acceptance's own claims are cross-checked against what was actually
/// touched). Empty report or complete coverage → unchanged text.
fn append_coverage(rejection: String, runner_report: Option<&TouchReport>) -> String {
    match runner_report {
        Some(report) => format!(
            "{rejection}{}",
            acceptance_evidence::coverage_rejection_line(report)
        ),
        None => rejection,
    }
}

/// 验收命令整体预算(秒):挂起判据 = 判据不返回即不存在 done 路径,task_done
/// 被永久楔死。预算到期 kill_on_drop 终止子进程并报超时(按不过形态归因,
/// fail-visible)
const ACCEPTANCE_COMMAND_TIMEOUT_SECS: u64 = 120;

/// 执行验收命令(windows=cmd /C,其余=sh -c;tokio 异步执行不阻塞执行器线程)。
/// spawn 失败按"未通过"处理(Err 携带原因)。整体受
/// [`ACCEPTANCE_COMMAND_TIMEOUT_SECS`] 预算约束(超时 = 终止,非放弃)。
async fn run_acceptance_command(cmd: &str) -> Result<std::process::Output, String> {
    run_acceptance_command_with_budget(
        cmd,
        std::time::Duration::from_secs(ACCEPTANCE_COMMAND_TIMEOUT_SECS),
    )
    .await
}

/// 预算参数化主体(测试注入短预算实测超时/终止路径)
async fn run_acceptance_command_with_budget(
    cmd: &str,
    budget: std::time::Duration,
) -> Result<std::process::Output, String> {
    #[cfg(windows)]
    let mut command = {
        let mut c = tokio::process::Command::new("cmd");
        // raw_arg 直传(与 workflow 判据执行同语义):arg() 按 MSVCRT 约定对
        // 内层引号做反斜杠转义而 cmd 不认转义——含引号判据串经 arg() 必被
        // 搅碎(实锤:python -c "exit(7)" 退出码偏离);直传让 cmd 收到与
        // 声明一致的串
        c.raw_arg("/C").raw_arg(cmd);
        c
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    // 预算=终止(非放弃):Unix 独立进程组 + 到期进程族终止(shell_exec 同款
    // 原语)——windows cmd /C 中间层下的孙进程一并回收,kill_on_drop 仅作
    // 直杀兜底
    #[cfg(unix)]
    command.process_group(0);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    command.kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| format!("acceptance command spawn failed: {e}"))?;
    let child_pid = child.id();
    // 管道并发排空(防满管阻塞子进程);内容收进缓冲,自然退出臂组装 Output。
    // 树级/组级终止必须在树存活时进行(kill_on_drop 的 drop 直杀会先断树,
    // taskkill /T 即扑空),故用借用式 wait + 前置 pid 快照
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let out_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(s) = stdout_pipe.as_mut() {
            let _ = tokio::io::AsyncReadExt::read_to_end(s, &mut buf).await;
        }
        buf
    });
    let err_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(s) = stderr_pipe.as_mut() {
            let _ = tokio::io::AsyncReadExt::read_to_end(s, &mut buf).await;
        }
        buf
    });
    match tokio::time::timeout(budget, child.wait()).await {
        Ok(Ok(status)) => {
            let stdout = out_task.await.unwrap_or_default();
            let stderr = err_task.await.unwrap_or_default();
            Ok(std::process::Output {
                status,
                stdout,
                stderr,
            })
        }
        Ok(Err(e)) => Err(format!("acceptance command spawn failed: {e}")),
        Err(_) => {
            // 进程族终止(树级/组级,树存活时链条完整)+ 直杀兜底 + reap
            if let Some(pid) = child_pid {
                crate::builtin_tools::shell_exec::terminate_family_by_pid(pid);
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = out_task.await;
            let _ = err_task.await;
            Err(format!(
                "acceptance command timed out after {}s (process tree terminated)",
                budget.as_secs()
            ))
        }
    }
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

    // ===== 验收命令整体预算与引号保真(o85) =====

    #[cfg(windows)]
    const HUNG_ACCEPTANCE_CMD: &str = "ping -n 120 127.0.0.1 >nul";
    #[cfg(not(windows))]
    const HUNG_ACCEPTANCE_CMD: &str = "sleep 120";

    #[tokio::test]
    async fn test_acceptance_command_budget_bounds_hung_command() {
        // 验收命令挂起原为无界 await——判据不返回即不存在 done 路径,task_done
        // 被永久楔死。预算到期必须报超时(fail-visible,按不过形态归因)
        let t0 = std::time::Instant::now();
        let err = run_acceptance_command_with_budget(
            HUNG_ACCEPTANCE_CMD,
            std::time::Duration::from_secs(1),
        )
        .await
        .expect_err("hung acceptance command must hit the budget");
        assert!(err.contains("timed out"), "{err}");
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(10),
            "budget must fire near 1s, took {:?}",
            t0.elapsed()
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_acceptance_command_windows_quote_preserved() {
        // Windows /C 引号保真回归(workflow 判据同型缺陷在此复发过):/C 参数
        // 未用 raw_arg 时 arg() 按 MSVCRT 约定转义内层引号而 cmd 不认——
        // python -c "exit(7)" 被搅碎后退出码偏离;直传后必得真实退出码 7
        let out = run_acceptance_command("python -c \"exit(7)\"")
            .await
            .expect("python 须成功 spawn");
        assert_eq!(out.status.code(), Some(7));
    }

    #[tokio::test]
    async fn test_gate_passes_on_exit_zero() {
        let args = serde_json::json!({
            "instruction_type": "task_done",
            "params": { "milestone_current": 3 }
        });
        match apply_acceptance_gate(Some("exit 0"), &args, None, None).await {
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
        match apply_acceptance_gate(Some("exit 3"), &args, None, None).await {
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
            apply_acceptance_gate(None, &args, None, None).await,
            GateOutcome::Allow(_)
        ));
    }

    #[tokio::test]
    async fn test_gate_task_blocked_requires_reason() {
        let no_reason = serde_json::json!({ "instruction_type": "task_blocked", "params": {} });
        assert!(matches!(
            apply_acceptance_gate(None, &no_reason, None, None).await,
            GateOutcome::Reject(_)
        ));
        let with_reason = serde_json::json!({
            "instruction_type": "task_blocked",
            "params": { "reason": "deps unavailable, fail-visible" }
        });
        assert!(matches!(
            apply_acceptance_gate(None, &with_reason, None, None).await,
            GateOutcome::Allow(_)
        ));
    }

    #[tokio::test]
    async fn test_gate_passthrough_other_instructions() {
        let args = serde_json::json!({ "instruction_type": "task_start", "params": {} });
        assert!(matches!(
            apply_acceptance_gate(Some("exit 1"), &args, None, None).await,
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

    // ===== Write-off conjunction gate (long-horizon mode) =====

    use crate::agent::writeoff::{EvidenceState, FreshnessState};

    /// Piece-state shorthand for the conjunction gate tests.
    fn pieces(evidence: EvidenceState, freshness: FreshnessState) -> JournalPieces {
        JournalPieces {
            evidence,
            freshness,
        }
    }

    fn done_args() -> serde_json::Value {
        serde_json::json!({ "instruction_type": "task_done", "params": {} })
    }

    #[tokio::test]
    async fn test_writeoff_allows_when_conjunction_passes() {
        // All three pieces satisfied: admission granted and the runner
        // truth injected exactly as in legacy mode.
        let p = pieces(
            EvidenceState::Present {
                digest: "d".to_string(),
            },
            FreshnessState::Fresh,
        );
        match apply_acceptance_gate(Some("exit 0"), &done_args(), Some(&p), None).await {
            GateOutcome::Allow(a) => {
                assert_eq!(a["params"]["acceptance_passed"], serde_json::json!(true));
            }
            GateOutcome::Reject(d) => panic!("satisfied conjunction must allow: {d}"),
        }
    }

    #[tokio::test]
    async fn test_writeoff_forged_pass_flag_cannot_grant_admission() {
        // G8 negative: a model-forged params.acceptance_passed=true is
        // never consulted — the criteria truth comes from the command
        // exit, and the conjunction refuses regardless of the flag.
        let args = serde_json::json!({
            "instruction_type": "task_done",
            "params": { "acceptance_passed": true }
        });
        let p = pieces(
            EvidenceState::Present {
                digest: "d".to_string(),
            },
            FreshnessState::Fresh,
        );
        match apply_acceptance_gate(Some("exit 7"), &args, Some(&p), None).await {
            GateOutcome::Reject(d) => {
                assert!(d.contains("缺项明细"), "{d}");
                assert!(d.contains("acceptance criteria not met"), "{d}");
            }
            GateOutcome::Allow(_) => panic!("forged flag must not grant admission"),
        }
    }

    #[tokio::test]
    async fn test_writeoff_missing_one_matrix_refuses_done() {
        let ok_evidence = EvidenceState::Present {
            digest: "d".to_string(),
        };
        // Single-missing cases.
        let cases = [
            (
                EvidenceState::Missing,
                FreshnessState::Fresh,
                "committed evidence missing",
            ),
            (
                ok_evidence.clone(),
                FreshnessState::Stale,
                "breakpoint stale",
            ),
            (
                ok_evidence.clone(),
                FreshnessState::Missing,
                "breakpoint missing",
            ),
        ];
        for (evidence, freshness, needle) in cases {
            let p = pieces(evidence, freshness);
            match apply_acceptance_gate(Some("exit 0"), &done_args(), Some(&p), None).await {
                GateOutcome::Reject(d) => {
                    assert!(d.contains(needle), "case `{needle}` detail: {d}");
                }
                GateOutcome::Allow(_) => {
                    panic!("single-missing case `{needle}` must refuse done")
                }
            }
        }
        // All-missing case: every failing piece listed in one detail.
        let p = pieces(EvidenceState::Missing, FreshnessState::Missing);
        match apply_acceptance_gate(Some("exit 3"), &done_args(), Some(&p), None).await {
            GateOutcome::Reject(d) => {
                assert!(d.contains("acceptance criteria not met"), "{d}");
                assert!(d.contains("committed evidence missing"), "{d}");
                assert!(d.contains("breakpoint missing"), "{d}");
            }
            GateOutcome::Allow(_) => panic!("all-missing case must refuse done"),
        }
    }

    #[tokio::test]
    async fn test_writeoff_unknown_journal_refuses_done_not_pass() {
        // Journal disabled/unreadable: both journal pieces are Unknown.
        // Unknown must refuse done — it must never degrade to a pass.
        let p = pieces(EvidenceState::Unknown, FreshnessState::Unknown);
        match apply_acceptance_gate(Some("exit 0"), &done_args(), Some(&p), None).await {
            GateOutcome::Reject(d) => {
                assert!(d.contains("unknown 不假装通过"), "{d}");
                assert!(d.contains("cannot be established"), "{d}");
            }
            GateOutcome::Allow(_) => panic!("unknown must not pass"),
        }
    }

    #[tokio::test]
    async fn test_writeoff_unconfigured_command_is_unknown_refusal() {
        // Long-horizon mode demands a criteria command: without one, the
        // legacy unconditional allow must NOT happen — the gate refuses
        // with the unknowable-criteria face.
        let p = pieces(
            EvidenceState::Present {
                digest: "d".to_string(),
            },
            FreshnessState::Fresh,
        );
        match apply_acceptance_gate(None, &done_args(), Some(&p), None).await {
            GateOutcome::Reject(d) => {
                assert!(d.contains("cannot be established"), "{d}");
            }
            GateOutcome::Allow(_) => {
                panic!("missing criteria command must refuse in conjunction mode")
            }
        }
    }

    #[tokio::test]
    async fn test_non_longhorizon_mode_behavior_unchanged() {
        // Default invariant (writeoff_pieces = None): criteria-only
        // admission. A passing command allows with no journal pieces at
        // all, and an unconfigured command still allows — the pieces that
        // would reject in conjunction mode are simply not consulted.
        assert!(matches!(
            apply_acceptance_gate(Some("exit 0"), &done_args(), None, None).await,
            GateOutcome::Allow(_)
        ));
        assert!(matches!(
            apply_acceptance_gate(None, &done_args(), None, None).await,
            GateOutcome::Allow(_)
        ));
        // Legacy forged-flag negative still holds without the conjunction.
        let args = serde_json::json!({
            "instruction_type": "task_done",
            "params": { "acceptance_passed": true }
        });
        assert!(matches!(
            apply_acceptance_gate(Some("exit 3"), &args, None, None).await,
            GateOutcome::Reject(_)
        ));
    }

    #[tokio::test]
    async fn test_writeoff_mode_leaves_other_instructions_untouched() {
        // The conjunction only governs task_done: task_blocked reason
        // check and passthrough instructions behave identically in
        // conjunction mode.
        let p = pieces(EvidenceState::Unknown, FreshnessState::Unknown);
        let blocked = serde_json::json!({ "instruction_type": "task_blocked", "params": {} });
        assert!(matches!(
            apply_acceptance_gate(None, &blocked, Some(&p), None).await,
            GateOutcome::Reject(_)
        ));
        let blocked_ok = serde_json::json!({
            "instruction_type": "task_blocked",
            "params": { "reason": "honest exit" }
        });
        assert!(matches!(
            apply_acceptance_gate(None, &blocked_ok, Some(&p), None).await,
            GateOutcome::Allow(_)
        ));
        let start = serde_json::json!({ "instruction_type": "task_start", "params": {} });
        assert!(matches!(
            apply_acceptance_gate(Some("exit 1"), &start, Some(&p), None).await,
            GateOutcome::Allow(_)
        ));
    }

    // ===== Acceptance evidence faces (touch report, dual run, spot-check) =====

    use crate::agent::acceptance_evidence::{
        compute_touch_coverage, project_touch_surface, read_manifest_entries, spot_check_evidence,
        TouchReport, TouchSurfaceEntry, DEFAULT_SPOT_CHECK_LIMIT,
    };
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    /// Digest in the shared ledger convention (blake3:<hex>).
    fn ev_digest(bytes: &[u8]) -> String {
        format!("blake3:{}", blake3::hash(bytes).to_hex())
    }

    /// Fixture checkpoint store: manifest SSOT + content blob under a
    /// temp workspace root (the gate consumes the projection read-only).
    fn evidence_fixture(name: &str, body: &[u8], path: &str) -> (PathBuf, Vec<TouchSurfaceEntry>) {
        let dir =
            std::env::temp_dir().join(format!("d4-accept-gate-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".evo-checkpoint").join("blobs")).unwrap();
        let hash = ev_digest(body);
        let line = format!(
            "{{\"seq\":1,\"ts\":1001,\"tool\":\"file_write\",\"path\":\"{path}\",\"kind\":\"content\",\"hash\":\"{hash}\",\"bytes\":{}}}",
            body.len()
        );
        std::fs::write(
            dir.join(".evo-checkpoint").join("manifest.jsonl"),
            line + "\n",
        )
        .unwrap();
        let blob = dir
            .join(".evo-checkpoint")
            .join("blobs")
            .join(hash.strip_prefix("blake3:").unwrap());
        std::fs::write(blob, body).unwrap();
        let entries = read_manifest_entries(&dir);
        (dir, project_touch_surface(&entries))
    }

    fn report_for(
        dir: &std::path::Path,
        surface: &[TouchSurfaceEntry],
        referenced: &[&str],
    ) -> TouchReport {
        let refs: BTreeSet<String> = referenced.iter().map(|s| s.to_string()).collect();
        TouchReport {
            surface: surface.to_vec(),
            coverage: compute_touch_coverage(surface, &refs),
            journal_anchor_coverage: None,
            integrity: spot_check_evidence(dir, surface, DEFAULT_SPOT_CHECK_LIMIT),
        }
    }

    #[cfg(windows)]
    fn append_counter_cmd(counter: &std::path::Path) -> String {
        format!("@echo x>>{}", counter.display())
    }

    #[cfg(not(windows))]
    fn append_counter_cmd(counter: &std::path::Path) -> String {
        format!("echo x >> {}", counter.display())
    }

    #[tokio::test]
    async fn test_dual_run_identical_outputs_allow_in_longhorizon_mode() {
        // Mode-on with a repeatable criterion: two runs, identical exit
        // and stdout — admission granted exactly as in legacy mode.
        let p = pieces(
            EvidenceState::Present {
                digest: "d".to_string(),
            },
            FreshnessState::Fresh,
        );
        match apply_acceptance_gate(Some("exit 0"), &done_args(), Some(&p), None).await {
            GateOutcome::Allow(a) => {
                assert_eq!(a["params"]["acceptance_passed"], serde_json::json!(true));
            }
            GateOutcome::Reject(d) => panic!("repeatable criterion must allow: {d}"),
        }
    }

    #[tokio::test]
    async fn test_dual_run_divergence_captured_as_non_determinism() {
        // Negative: a criterion embedding varying output diverges across
        // the two runs; the dual run must capture it and refuse honestly
        // (discipline, not defect). This probe can only pass if the
        // command genuinely executed twice with diverging output. The
        // probe command is the exported assembly one (a GUID print — a
        // time-based %RANDOM% collides when both runs spawn within the
        // same system tick, measured on this host).
        use crate::agent::acceptance_evidence::NONDETERMINISTIC_PROBE_CMD;
        let nondet = NONDETERMINISTIC_PROBE_CMD;
        let p = pieces(
            EvidenceState::Present {
                digest: "d".to_string(),
            },
            FreshnessState::Fresh,
        );
        match apply_acceptance_gate(Some(nondet), &done_args(), Some(&p), None).await {
            GateOutcome::Reject(d) => {
                assert!(d.contains("non-deterministic"), "{d}");
                assert!(d.contains("dual-run"), "{d}");
            }
            GateOutcome::Allow(_) => panic!("a non-deterministic criterion must be refused"),
        }
    }

    #[tokio::test]
    async fn test_dual_run_executes_twice_only_in_longhorizon_mode() {
        // Execution-count proof through a file side effect: legacy mode
        // runs the criterion once; long-horizon mode runs it twice.
        let dir = std::env::temp_dir().join(format!("d4-accept-cnt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let counter = dir.join("counter.txt");
        let cmd = append_counter_cmd(&counter);
        let t0 = std::time::Instant::now();
        match apply_acceptance_gate(Some(&cmd), &done_args(), None, None).await {
            GateOutcome::Allow(a) => {
                assert_eq!(a["params"]["acceptance_passed"], serde_json::json!(true));
            }
            GateOutcome::Reject(d) => panic!("deterministic criterion must allow: {d}"),
        }
        assert!(
            t0.elapsed() >= std::time::Duration::from_millis(1),
            "criterion must really run (elapsed {:?})",
            t0.elapsed()
        );
        let content = std::fs::read_to_string(&counter).unwrap();
        let runs = content.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(
            runs, 1,
            "non-long-horizon mode must run the criterion exactly once, got {content:?}"
        );
        // Long-horizon mode: the same style of criterion appends one line
        // per execution — the dual run must leave exactly two.
        let counter2 = dir.join("counter2.txt");
        let cmd2 = append_counter_cmd(&counter2);
        let p = pieces(
            EvidenceState::Present {
                digest: "d".to_string(),
            },
            FreshnessState::Fresh,
        );
        match apply_acceptance_gate(Some(&cmd2), &done_args(), Some(&p), None).await {
            GateOutcome::Allow(_) => {}
            GateOutcome::Reject(d) => {
                panic!("deterministic criterion must allow in mode-on too: {d}")
            }
        }
        let content2 = std::fs::read_to_string(&counter2).unwrap();
        let runs2 = content2.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(
            runs2, 2,
            "long-horizon mode must run the criterion twice (dual run), got {content2:?}"
        );
    }

    #[tokio::test]
    async fn test_evidence_spot_check_blocks_on_tampered_blob() {
        // Negative: after-capture blob tampering is detected by
        // recomputation and refuses admission even though the criteria
        // command would pass — recorded digests are never trusted.
        let (dir, surface) = evidence_fixture("tamper", b"artifact-bytes", "a.txt");
        let p = pieces(
            EvidenceState::Present {
                digest: "d".to_string(),
            },
            FreshnessState::Fresh,
        );
        // Honest store: evidence face ok, admission granted.
        let report = report_for(&dir, &surface, &["a.txt"]);
        assert_eq!(report.coverage.ratio_pct, 100);
        match apply_acceptance_gate(Some("exit 0"), &done_args(), Some(&p), Some(&report)).await {
            GateOutcome::Allow(_) => {}
            GateOutcome::Reject(d) => panic!("honest evidence must allow: {d}"),
        }
        // Tamper one byte of the blob on disk.
        let hash = ev_digest(b"artifact-bytes");
        let blob = dir
            .join(".evo-checkpoint")
            .join("blobs")
            .join(hash.strip_prefix("blake3:").unwrap());
        std::fs::write(&blob, b"artifact-byteZ").unwrap();
        let tampered = report_for(&dir, &surface, &["a.txt"]);
        match apply_acceptance_gate(Some("exit 0"), &done_args(), Some(&p), Some(&tampered)).await {
            GateOutcome::Reject(d) => {
                assert!(d.contains("evidence recomputation"), "{d}");
                assert!(d.contains("a.txt"), "{d}");
            }
            GateOutcome::Allow(_) => panic!("tampered evidence must refuse"),
        }
    }

    #[tokio::test]
    async fn test_rejection_names_unreferenced_touch_paths() {
        // Design fixture: 5 touched / 3 referenced = 60%; every task_done
        // refusal must name the unreferenced paths (credibility signal).
        let dir = std::env::temp_dir().join(format!("d4-accept-cov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".evo-checkpoint").join("blobs")).unwrap();
        let hash = ev_digest(b"x");
        let lines: Vec<String> = ["p1", "p2", "p3", "p4", "p5"]
            .iter()
            .enumerate()
            .map(|(i, path)| {
                format!(
                    "{{\"seq\":{},\"ts\":{},\"tool\":\"file_write\",\"path\":\"{path}\",\"kind\":\"content\",\"hash\":\"{hash}\",\"bytes\":1}}",
                    i + 1,
                    1000 + i,
                )
            })
            .collect();
        std::fs::write(
            dir.join(".evo-checkpoint").join("manifest.jsonl"),
            lines.join("\n") + "\n",
        )
        .unwrap();
        let blob = dir
            .join(".evo-checkpoint")
            .join("blobs")
            .join(hash.strip_prefix("blake3:").unwrap());
        std::fs::write(blob, b"x").unwrap();
        let surface = project_touch_surface(&read_manifest_entries(&dir));
        assert_eq!(surface.len(), 5);
        let report = report_for(&dir, &surface, &["p1", "p2", "p3"]);
        assert_eq!(report.coverage.ratio_pct, 60, "5 touched / 3 referenced");
        let p = pieces(
            EvidenceState::Present {
                digest: "d".to_string(),
            },
            FreshnessState::Fresh,
        );
        match apply_acceptance_gate(Some("exit 1"), &done_args(), Some(&p), Some(&report)).await {
            GateOutcome::Reject(d) => {
                assert!(d.contains("acceptance criteria not met"), "{d}");
                assert!(d.contains("3/5"), "{d}");
                assert!(d.contains("60%"), "{d}");
                assert!(d.contains("p4") && d.contains("p5"), "{d}");
            }
            GateOutcome::Allow(_) => panic!("failing criterion must refuse"),
        }
    }
}
