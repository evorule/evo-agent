// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `env_doctor` —— 编程环境预检(只读探针矩阵)。
//!
//! 固定探针表逐一 spawn `--version`(argv 直调,禁 shell),返回在位性/
//! 版本行/缺口指引矩阵——环境缺口在动手前显式暴露(缺失启动器的
//! "毫秒级 spawn 死"形态在诊断前即现形,而非任务中途撞墙)。
//!
//! 只读语义:探针全部为版本查询,零工作区写面;进程短命(spawn 后即收)。

#![forbid(unsafe_code)]

use std::path::PathBuf;

// Windows GUI 子系统:CREATE_NO_WINDOW 需要该扩展 trait
#[cfg(windows)]
use std::os::windows::process::CommandExt;

use serde_json::{json, Value};

use crate::io_handler::IoResult;

use crate::io_handlers::tool_handler::ToolFunction;

/// 工具名(LLM 契约面/manifest/注册三处同键)
pub const ENV_DOCTOR_TOOL: &str = "env_doctor";

/// 固定探针表(名称, argv, 用途说明/缺口指引)
const PROBES: &[(&str, &[&str], &str)] = &[
    (
        "cargo",
        &["--version"],
        "Rust build tool (build_check cargo profiles)",
    ),
    ("rustc", &["--version"], "Rust compiler"),
    ("git", &["--version"], "version control (git_* tools)"),
    ("python", &["--version"], "Python interpreter"),
    (
        "pytest",
        &["--version"],
        "Python test runner (build_check pytest profile); if missing but python \
         is present, try `python -m pip install --force-reinstall pytest`",
    ),
    (
        "node",
        &["--version"],
        "Node.js runtime (LSP pyright / typescript-language-server via npm)",
    ),
];

/// 工具本体(工作区根锚定;探针为版本查询,与 workdir 无关——字段保留
/// 与工具族构造形态一致)
pub struct EnvDoctorTool {
    #[allow(dead_code)]
    workdir: PathBuf,
}

impl EnvDoctorTool {
    /// 构造(workdir=装配工作区根)
    pub fn new(workdir: PathBuf) -> Self {
        Self { workdir }
    }

    /// 单探针:spawn `--version`,返回 (在位, 版本首行)
    fn run_probe(program: &str, argv: &[&str]) -> (bool, String) {
        let mut cmd = std::process::Command::new(program);
        cmd.args(argv);
        // Windows GUI 子系统下抑制控制台窗口
        #[cfg(windows)]
        {
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        match cmd.output() {
            Ok(o) if o.status.success() => {
                let first = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let first = if first.is_empty() {
                    // python 老版本把 --version 打到 stderr
                    String::from_utf8_lossy(&o.stderr)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string()
                } else {
                    first
                };
                (true, first)
            }
            _ => (false, String::new()),
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for EnvDoctorTool {
    async fn call(&self, _args: &Value) -> IoResult {
        // 探针为快速子进程查询:spawn_blocking 包装(工具族 async 契约)
        let probes = tokio::task::spawn_blocking(|| {
            PROBES
                .iter()
                .map(|(name, argv, purpose)| {
                    let (ok, version) = Self::run_probe(name, argv);
                    json!({
                        "name": name,
                        "ok": ok,
                        "version": if ok { Value::from(version) } else { Value::Null },
                        "purpose": purpose,
                    })
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| format!("env_doctor probe task failed: {e}"))?;
        let missing: Vec<&str> = probes
            .iter()
            .filter(|p| p["ok"].as_bool() != Some(true))
            .map(|p| p["name"].as_str().unwrap_or("?"))
            .collect();
        let all_ok = missing.is_empty();
        let mut map = json!({
            "action": "env_probe",
            "probes": probes,
            "missing": missing,
            "all_ok": all_ok,
        });
        if !all_ok {
            if let Some(o) = map.as_object_mut() {
                o.insert(
                    "note".to_string(),
                    Value::String(format!(
                        "missing tools: {} — install them before relying on the \
                         matching build_check profiles or LSP queries",
                        missing.join(", ")
                    )),
                );
            }
        }
        Ok(map)
    }
}

/// LLM 契约面参数 schema(零参数;探针表封闭)
pub fn env_doctor_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "required": [],
    })
}

/// LLM 契约面描述(英文;只读定位)
pub fn env_doctor_description() -> String {
    "Probe the local coding toolchain (read-only): checks cargo, rustc, git, \
     python, pytest, and node presence + versions via --version queries and \
     returns a matrix with install hints for anything missing. Use this \
     before relying on build_check profiles or LSP queries, to surface \
     environment gaps early instead of mid-task. Read-only: version queries \
     only, no workspace writes."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_handlers::tool_handler::ToolFunction;

    #[test]
    fn schema_has_no_required_params() {
        let s = env_doctor_schema();
        assert_eq!(s["required"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn probe_table_covers_core_toolchain() {
        let names: Vec<&str> = PROBES.iter().map(|(n, _, _)| *n).collect();
        for expected in ["cargo", "rustc", "git", "python", "pytest", "node"] {
            assert!(names.contains(&expected), "{expected} probe missing");
        }
    }

    #[tokio::test]
    async fn matrix_reports_and_never_fails() {
        // git 在任意开发机必在位;矩阵整体不得 Err(fail-soft 形态)
        let tool = EnvDoctorTool::new(std::env::temp_dir());
        let out = tool.call(&json!({})).await.expect("matrix returned");
        assert!(out["all_ok"].is_boolean());
        assert!(out["probes"].as_array().unwrap().len() == PROBES.len());
    }

    #[tokio::test]
    async fn missing_tool_reports_with_hint() {
        // 不在 PATH 的探针:ok=false 且 version 为 Null(矩阵面如实)
        let (ok, _) = EnvDoctorTool::run_probe("definitely-not-here-b462", &["--version"]);
        assert!(!ok);
    }
}
