// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! `apply_patch` —— 容错 patch 应用工具(工具族外延批,原方案锚残余缺口)。
//!
//! 把「落地 LLM 生成的 unified diff」从多轮 shell_exec 审批试错(--3way/
//! --reject/逐 hunk 手补)收敛为单工具容错链:能精确应用就精确、上下文漂移
//! 走三方归并、归并不了部分应用并回传逐 hunk 拒绝清单——fail-visible 贯穿。
//!
//! ## 安全栏
//!
//! - **argv 直调无 shell**:全部 git 调用为参数数组,无解释器面;
//! - **传输走临时文件**:patch 文本写系统临时目录(生成名,全路径用后即删,
//!   含失败路径)——防大 patch 的 argv 参数长度帽;
//! - **部分应用 fail-visible**:`--reject` 有拒绝 hunk 时如实报 partial+
//!   拒绝清单(.rej 伴生文件在盘可查),禁静默半态;
//! - **check 干跑零写入**:`git apply --check` 全程只读,先验后用。

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::io_handler::IoResult;

/// 工具名(LLM 契约面/manifest/注册三处同键)
pub const APPLY_PATCH_TOOL: &str = "apply_patch";

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// 工作区根(wired 注入;全部 git 调用的 cwd=patch 相对路径解析锚)
#[derive(Clone)]
pub struct ApplyPatchTool {
    workdir: PathBuf,
}

impl ApplyPatchTool {
    /// 生产构造(workdir=工作区根,与 shell_exec/git_tools 同基线)
    pub fn wired(workdir: &Path) -> Self {
        Self {
            workdir: workdir.to_path_buf(),
        }
    }

    /// 测试构造
    #[cfg(test)]
    pub(crate) fn with_workdir(workdir: &Path) -> Self {
        Self {
            workdir: workdir.to_path_buf(),
        }
    }

    /// 同步 git 调用(argv 直调;cwd=工作区根)
    fn git(&self, args: &[&str]) -> Result<std::process::Output, String> {
        Command::new("git")
            .args(args)
            .current_dir(&self.workdir)
            .output()
            .map_err(|e| format!("git spawn failed: {e} (is git installed?)"))
    }

    /// patch 文本落地临时文件(系统临时目录+生成名;调用方负责删除)
    fn write_patch_temp(patch: &str) -> Result<(PathBuf, String), String> {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("evo-apply-patch-{now}-{seq}.diff"));
        // 尾换行兜底:git apply 对缺尾换行的 patch 报 corrupt(实测);LLM 产
        // patch 高发此形态,补一个换行是纯增量的鲁棒性修复
        let mut text = patch.to_string();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        std::fs::write(&p, text).map_err(|e| format!("patch temp file write failed: {e}"))?;
        Ok((p.clone(), p.to_string_lossy().to_string()))
    }

    /// 单档尝试:返回 Ok( Some(stdout) )=该档全量成功;Ok(None)=该档失败
    /// (但 --reject 例外:退出 1=部分应用,由调用方按档语义判定);Err=spawn 失败
    fn try_tier(&self, git_args: &[&str]) -> Result<Option<String>, String> {
        let out = self.git(git_args)?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if out.status.success() {
            return Ok(Some(stdout));
        }
        // --reject 语义:退出 1 且有 "Applied patch"/"Rejected hunk" 痕迹=
        // 部分应用(由调用方按档位识别,此处统一回传 None+痕迹在 stderr)
        let _ = stderr;
        Ok(None)
    }

    /// 容错链执行(check=false 实写;返回档位与详情)
    fn run_chain(
        &self,
        patch_file: &str,
        three_way: bool,
        reject_fallback: bool,
        dry_run: bool,
    ) -> Result<(String, String), String> {
        let check: &[&str] = if dry_run { &["--check"] } else { &[] };
        // 档一:精确应用
        let mut a1: Vec<&str> = vec!["apply"];
        a1.extend_from_slice(check);
        a1.push(patch_file);
        if let Some(stdout) = self.try_tier(&a1)? {
            return Ok(("exact".to_string(), stdout.trim().to_string()));
        }
        // 档二:三方归并(--3way 依赖 index blobs;缺基线自动失败落下一档)
        let mut a2: Vec<&str> = vec!["apply"];
        a2.extend_from_slice(check);
        a2.push("--3way");
        a2.push("--whitespace=nowarn");
        a2.push(patch_file);
        if three_way {
            if let Some(stdout) = self.try_tier(&a2)? {
                return Ok(("three_way".to_string(), stdout.trim().to_string()));
            }
        }
        // 档三:部分应用(--reject;干跑态不适用——check 只报可达档位)
        if !dry_run && reject_fallback {
            let mut a3: Vec<&str> = vec!["apply", "--reject", "--whitespace=nowarn"];
            a3.push(patch_file);
            let out = self.git(&a3)?;
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            let detail = format!("{}{}", stdout.trim(), stderr.trim());
            if out.status.success() {
                return Ok(("applied_reject_clean".to_string(), detail));
            }
            // 退出非零但有应用痕迹=部分应用(partial);无任何痕迹=全败 fail-visible
            let has_reject_trace = detail.contains("Rejected hunk")
                || detail.contains("Applied patch")
                || detail.contains("with reject");
            if has_reject_trace {
                return Ok(("partial".to_string(), detail));
            }
            return Err(format!(
                "patch failed at all tolerance tiers; last attempt (git apply \
                 --reject) stderr/stdout tail: {}",
                detail.trim()
            ));
        }
        Err(format!(
            "patch failed at enabled tolerance tiers (exact{}{}); use check for \
             details or adjust the patch context",
            if three_way { ", three_way" } else { "" },
            if reject_fallback { ", reject" } else { "" }
        ))
    }

    /// 从链详情提取 .rej 拒绝清单行(逐 hunk 拒绝 fail-visible 面)
    fn reject_lines(detail: &str) -> Vec<String> {
        detail
            .lines()
            .filter(|l| {
                l.contains("Rejected hunk") || l.contains(".rej") || l.contains("Applied patch")
            })
            .map(str::to_string)
            .collect()
    }
}

#[async_trait::async_trait]
impl crate::io_handlers::tool_handler::ToolFunction for ApplyPatchTool {
    async fn call(&self, args: &Value) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("apply_patch tool panicked: {e}"))?
    }
}

impl ApplyPatchTool {
    /// 同步实现(供 spawn_blocking 调用)
    fn call_sync(&self, args: &Value) -> IoResult {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: action (check | apply)".to_string())?;
        match action {
            "check" => self.action_apply(args, true),
            "apply" => self.action_apply(args, false),
            other => Err(format!(
                "unknown action \"{other}\" (valid actions: check, apply)"
            )),
        }
    }

    fn action_apply(&self, args: &Value, dry_run: bool) -> IoResult {
        let patch = args
            .get("patch")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "missing required arg: patch (unified diff text, non-empty)".to_string()
            })?;
        let three_way = args
            .get("three_way")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let reject_fallback = args
            .get("reject_fallback")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        // 传输走临时文件(防大 patch argv 长度帽);全路径用后即删(含失败路径)
        let (tmp_path, tmp_display) = Self::write_patch_temp(patch)?;
        let result = self.run_chain(&tmp_display, three_way, reject_fallback, dry_run);
        let _ = std::fs::remove_file(&tmp_path);
        let (tier, detail) = result?;
        let mut payload = serde_json::json!({
            "action": if dry_run { "check" } else { "apply" },
            "tier": tier,
            "detail": detail,
        });
        if !dry_run {
            payload["status"] = Value::from(if tier == "partial" {
                "partial_applied_rejected_hunks_listed_in_detail"
            } else {
                "applied"
            });
            let rejects = Self::reject_lines(&detail);
            if !rejects.is_empty() {
                payload["rejected"] = serde_json::json!(rejects);
                payload["note"] = Value::from(
                    "some hunks were rejected and .rej companion files were written; \
                     resolve them with edit_file (the .rej files themselves are NOT \
                     deleted by this tool)",
                );
            }
        }
        Ok(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_handlers::tool_handler::ToolFunction;

    /// 真 git 临时仓:基线提交(文件 base.txt 内容 a/b/c 三行)
    fn repo_with_base(content: &str) -> (tempfile::TempDir, ApplyPatchTool) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git spawn");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-b", "main"]);
        std::fs::write(root.join("base.txt"), content).unwrap();
        run(&["add", "base.txt"]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-m",
            "base",
        ]);
        let tool = ApplyPatchTool::with_workdir(&root);
        (dir, tool)
    }

    fn ap_args(action: &str, patch: &str, extra: Value) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("action".to_string(), Value::from(action));
        m.insert("patch".to_string(), Value::from(patch));
        if let Value::Object(o) = extra {
            for (k, v) in o {
                m.insert(k, v);
            }
        }
        Value::Object(m)
    }

    #[tokio::test]
    async fn exact_apply_updates_file() {
        let (d, tool) = repo_with_base("a\nb\nc\n");
        let root = d.path().to_path_buf();
        let patch = "\
diff --git a/base.txt b/base.txt
--- a/base.txt
+++ b/base.txt
@@ -1,3 +1,3 @@
 a
-b
+B
 c
";
        let r = tool
            .call(&ap_args("apply", patch, Value::Null))
            .await
            .unwrap();
        assert_eq!(r.get("tier").and_then(|v| v.as_str()), Some("exact"));
        let now = std::fs::read_to_string(root.join("base.txt"))
            .unwrap()
            .replace('\r', "");
        assert_eq!(now, "a\nB\nc\n");
    }

    #[tokio::test]
    async fn three_way_merges_stale_patch_against_evolved_code() {
        // --3way 真实语义(实测三条硬约束):①patch 必须带 index 行(无行必败)
        // ②工作树文件必须与 index 一致(否则 "does not match index")③邻近行
        // 异改=冲突(ours 改行须与 patch 改行非邻)——真用例=补丁对已提交演进
        // 的代码过期:base 五行,v1 造补丁改 d 行,v2 提交改 a 行后回放:
        // 基=v1 blob,ours=v2 提交,patch 改 d → 三方归并出 A2/b/c/D/e
        let (d, tool) = repo_with_base("a\nb\nc\nd\ne\n");
        let root = d.path().to_path_buf();
        let hash = |content: &[u8]| -> String {
            let tmp = root.join("hashobj.tmp");
            std::fs::write(&tmp, content).unwrap();
            let sha = String::from_utf8_lossy(
                &Command::new("git")
                    .args(["hash-object", tmp.to_str().unwrap()])
                    .current_dir(&root)
                    .output()
                    .expect("git hash-object")
                    .stdout,
            )
            .trim()
            .to_string();
            let _ = std::fs::remove_file(&tmp);
            sha
        };
        // v1 blob 基哈希(提交内容)与 patch postimage 哈希
        let old_sha = hash(b"a\nb\nc\nd\ne\n");
        let new_sha = hash(b"a\nb\nc\nD\ne\n");
        // 演进:v2 提交把 a 行改 A2(已提交态=工作树与 index 一致;行 1 与
        // patch 改动的行 4 非邻=归并可清)
        std::fs::write(root.join("base.txt"), "A2\nb\nc\nd\ne\n").unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git spawn");
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["add", "base.txt"]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-m",
            "v2 evolve",
        ]);
        let patch = format!(
            "\
diff --git a/base.txt b/base.txt
index {old_sha}..{new_sha} 100644
--- a/base.txt
+++ b/base.txt
@@ -1,5 +1,5 @@
 a
 b
 c
-d
+D
 e
"
        );
        let r = tool
            .call(&ap_args(
                "apply",
                &patch,
                serde_json::json!({"reject_fallback": false}),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.get("tier").and_then(|v| v.as_str()),
            Some("three_way"),
            "过期补丁对演进提交应走三方归并: {r:?}"
        );
        let now = std::fs::read_to_string(root.join("base.txt"))
            .unwrap()
            .replace('\r', "");
        assert_eq!(
            now, "A2\nb\nc\nD\ne\n",
            "归并应同时保留 v2 演进(a→A2)与 patch 改动(d→D)"
        );
    }

    #[tokio::test]
    async fn partial_applies_good_hunks_and_lists_rejects() {
        let (d, tool) = repo_with_base("a\nb\nc\n");
        let root = d.path().to_path_buf();
        // hunk1 可应用;b/hunk2 引用不存在上下文(zzz 行)必拒
        let patch = "\
diff --git a/base.txt b/base.txt
--- a/base.txt
+++ b/base.txt
@@ -1,2 +1,3 @@
 a
+a2
 b
@@ -1,2 +1,3 @@
 zzz-nonexistent
 c
+tail
";
        let r = tool
            .call(&ap_args("apply", patch, Value::Null))
            .await
            .unwrap();
        assert_eq!(
            r.get("tier").and_then(|v| v.as_str()),
            Some("partial"),
            "不可归并 hunk 应部分应用: {r:?}"
        );
        assert!(r.get("rejected").is_some(), "拒绝清单在回传");
        let now = std::fs::read_to_string(root.join("base.txt"))
            .unwrap()
            .replace('\r', "");
        assert!(now.contains("a2"), "可应用 hunk 已落盘: {now:?}");
        // .rej 伴生文件在盘(fail-visible,处置归 agent)
        let rej = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".rej"));
        assert!(rej, ".rej 伴生文件在盘");
    }

    #[tokio::test]
    async fn check_dry_run_reports_tier_without_writing() {
        let (d, tool) = repo_with_base("a\nb\nc\n");
        let root = d.path().to_path_buf();
        let before = std::fs::read_to_string(root.join("base.txt")).unwrap();
        let patch = "\
diff --git a/base.txt b/base.txt
--- a/base.txt
+++ b/base.txt
@@ -1,3 +1,3 @@
 a
-b
+B
 c
";
        let r = tool
            .call(&ap_args("check", patch, Value::Null))
            .await
            .unwrap();
        assert_eq!(r.get("tier").and_then(|v| v.as_str()), Some("exact"));
        assert_eq!(
            std::fs::read_to_string(root.join("base.txt")).unwrap(),
            before,
            "check 干跑零写入"
        );
    }

    #[tokio::test]
    async fn malformed_and_empty_patch_fail_visible() {
        let (_d, tool) = repo_with_base("a\nb\nc\n");
        let no_patch = serde_json::json!({"action": "apply"});
        let err = tool.call(&no_patch).await.unwrap_err();
        assert!(err.contains("missing required arg: patch"), "got: {err}");
        let err = tool
            .call(&ap_args("apply", "", Value::Null))
            .await
            .unwrap_err();
        assert!(err.contains("non-empty"), "空 patch 拒: {err}");
        let err = tool
            .call(&ap_args("apply", "this is not a diff", Value::Null))
            .await
            .unwrap_err();
        assert!(
            err.contains("failed at all tolerance tiers"),
            "malformed fail-visible: {err}"
        );
    }

    #[tokio::test]
    async fn unknown_action_rejected() {
        let (_d, tool) = repo_with_base("a\nb\nc\n");
        let err = tool
            .call(&ap_args("revert", "x", Value::Null))
            .await
            .unwrap_err();
        assert!(err.contains("valid actions: check, apply"), "got: {err}");
    }
}
