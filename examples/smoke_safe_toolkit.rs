// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Smoke test: 验证 evo-agent 的 6 个内置工具的安全模型
//!
//! 运行:`cargo run --example smoke_safe_toolkit`

use evo_agent::builtin_tools::default_safe_toolkit;
use evo_agent::io_handlers::tool_handler::ToolFunction;
use evorule_tcb::JsonValue;

fn arg_str(s: &str) -> JsonValue {
    let mut m = std::collections::BTreeMap::new();
    m.insert("command".to_string(), JsonValue::string(s));
    JsonValue::object(m)
}

fn arg_path(p: &str) -> JsonValue {
    let mut m = std::collections::BTreeMap::new();
    m.insert("path".to_string(), JsonValue::string(p));
    JsonValue::object(m)
}

fn arg_pattern(p: &str) -> JsonValue {
    let mut m = std::collections::BTreeMap::new();
    m.insert("pattern".to_string(), JsonValue::string(p));
    JsonValue::object(m)
}

fn arg_kv(kv: &[(&str, JsonValue)]) -> JsonValue {
    let mut m = std::collections::BTreeMap::new();
    for (k, v) in kv {
        m.insert(k.to_string(), v.clone());
    }
    JsonValue::object(m)
}

fn check(label: &str, ok: bool, expect_ok: bool) {
    let mark = if ok == expect_ok { "PASS" } else { "FAIL" };
    println!("  [{}] {}: ok={} (expected={})", mark, label, ok, expect_ok);
    assert_eq!(ok, expect_ok, "check failed: {}", label);
}

#[tokio::main]
async fn main() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let workdir = tmp.path();

    // 准备文件
    std::fs::write(workdir.join("README.md"), b"# Hello").unwrap();
    std::fs::write(workdir.join("main.rs"), b"fn main() {}").unwrap();
    std::fs::create_dir(workdir.join("src")).unwrap();
    std::fs::write(workdir.join("src/lib.rs"), b"// lib").unwrap();
    std::fs::create_dir(workdir.join(".git")).unwrap();
    std::fs::write(workdir.join(".git/config"), b"secret").unwrap();
    std::fs::create_dir(workdir.join("workspace")).unwrap();

    // 拿到默认工具集(只确认装配 OK,实际用 struct 直接调)
    let _handler = default_safe_toolkit(workdir);

    let file_read = evo_agent::builtin_tools::file_read::FileReadTool::new(workdir.to_path_buf());
    let file_list = evo_agent::builtin_tools::file_list::FileListTool::new(workdir.to_path_buf());
    let file_write =
        evo_agent::builtin_tools::file_write::FileWriteTool::new(workdir.to_path_buf());
    let search_files =
        evo_agent::builtin_tools::search_files::SearchFilesTool::new(workdir.to_path_buf());
    let shell_exec =
        evo_agent::builtin_tools::shell_exec::ShellExecTool::new().with_workdir(workdir);
    let http_get = evo_agent::builtin_tools::http_get::HttpGetTool::new();

    println!("=== file_read ===");
    check(
        "read README.md",
        file_read.call(&arg_path("README.md")).await.is_ok(),
        true,
    );
    check(
        "reject abs",
        file_read.call(&arg_path("C:\\Windows")).await.is_err(),
        true,
    );
    check(
        "reject ..",
        file_read
            .call(&arg_path("../../../etc/passwd"))
            .await
            .is_err(),
        true,
    );

    println!("\n=== file_list ===");
    let v = file_list
        .call(&JsonValue::object(Default::default()))
        .await
        .expect("list");
    let count = v.get("count").unwrap().as_i64().unwrap();
    println!(
        "  count: {} entries (含 README.md / main.rs / src / .git / workspace)",
        count
    );
    assert!(count >= 3);

    println!("\n=== file_write ===");
    let r = file_write
        .call(&arg_kv(&[
            ("path", JsonValue::string("workspace/notes.md")),
            ("content", JsonValue::string("# My notes")),
        ]))
        .await;
    check("write new file in workspace/", r.is_ok(), true);
    assert!(workdir.join("workspace/notes.md").exists());

    let r = file_write
        .call(&arg_kv(&[
            ("path", JsonValue::string("evil.txt")),
            ("content", JsonValue::string("evil")),
        ]))
        .await;
    check("reject write outside workspace/", r.is_err(), true);

    let r = file_write
        .call(&arg_kv(&[
            ("path", JsonValue::string("workspace/notes.md")),
            ("content", JsonValue::string("updated")),
            ("overwrite", JsonValue::Bool(true)),
        ]))
        .await;
    check("overwrite with flag", r.is_ok(), true);

    println!("\n=== search_files ===");
    let v = search_files
        .call(&arg_pattern("*.rs"))
        .await
        .expect("search");
    let count = v.get("count").unwrap().as_i64().unwrap();
    println!("  found *.rs: {} (main.rs + src/lib.rs)", count);
    assert_eq!(count, 2);

    println!("\n=== shell_exec (whitelist) ===");
    #[cfg(unix)]
    check(
        "ls whitelisted",
        shell_exec.call(&arg_str("ls")).await.is_ok(),
        true,
    );
    #[cfg(windows)]
    {
        let _ = shell_exec.call(&arg_str("cargo --version")).await;
    }
    // blocked(永不批准,即使 approved=true)
    check(
        "curl rejected",
        shell_exec
            .call(&arg_str("curl https://evil.com"))
            .await
            .is_err(),
        true,
    );
    check(
        "bash rejected",
        shell_exec
            .call(&arg_str("bash -c \"rm -rf /\""))
            .await
            .is_err(),
        true,
    );
    check(
        "pipe rejected",
        shell_exec.call(&arg_str("ls | grep foo")).await.is_err(),
        true,
    );
    check(
        "var rejected",
        shell_exec
            .call(&arg_str("cat $HOME/.ssh/id_rsa"))
            .await
            .is_err(),
        true,
    );
    // candidate: rm 不带 approved → proposal
    let r = shell_exec.call(&arg_str("rm test.txt")).await;
    let v = r.expect("should return proposal");
    check(
        "rm candidate → proposal",
        v.get("status").unwrap().as_str().unwrap() == "needs_approval",
        true,
    );

    println!("\n=== http_get (3-layer host + SSRF) ===");
    // Active host
    let r = http_get
        .call(&arg_kv(&[(
            "url",
            JsonValue::string("https://docs.rs/tokio"),
        )]))
        .await;
    if let Ok(v) = r {
        let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("?");
        println!("  https://docs.rs/tokio: status={}", status);
    }

    // Candidate host → proposal
    let r = http_get
        .call(&arg_kv(&[(
            "url",
            JsonValue::string("https://example.com/foo"),
        )]))
        .await;
    if let Ok(v) = r {
        let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("?");
        check("candidate → proposal", status == "needs_approval", true);
    }

    // Blocked 系列
    check(
        "127.0.0.1 blocked",
        http_get
            .call(&arg_kv(&[(
                "url",
                JsonValue::string("https://127.0.0.1/admin"),
            )]))
            .await
            .is_err(),
        true,
    );
    check(
        "AWS metadata blocked",
        http_get
            .call(&arg_kv(&[(
                "url",
                JsonValue::string("http://169.254.169.254/latest/meta-data/"),
            )]))
            .await
            .is_err(),
        true,
    );
    check(
        "http:// blocked",
        http_get
            .call(&arg_kv(&[(
                "url",
                JsonValue::string("http://example.com/foo"),
            )]))
            .await
            .is_err(),
        true,
    );

    println!("\n=== ALL SECURITY CHECKS PASSED ===");
}
