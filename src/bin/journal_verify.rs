// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! journal-verify：journal 链只读验证工具（审计哨兵可调度）
//!
//! 退出码（与 evorule scan-wal-dir 协议对齐）：0=验证通过（含 unhashed 如实
//! 计数）；3=链破坏（内容/链接/链值失配或 seq 断裂）；5=半行/损坏拒收。
//! `--expect-tail`：与引擎审计链锚值比对（从 evorule payload `__journal__.anchor`
//! 读出 chain_tail 传入——本地整链重算后与锚不符即 rc=3，防重算实证口径）。

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(about = "journal 链只读验证（seq 连续 + 逐行哈希三查 + 可选锚比对）")]
struct Args {
    /// journal 文件路径（`{dir}/{session_id}.jsonl`）
    file: PathBuf,
    /// 期望链尾（引擎链锚值；提供时做防整链重算比对）
    #[arg(long)]
    expect_tail: Option<String>,
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    match evo_agent::agent::journal::verify_journal_file(&args.file) {
        Ok(r) => {
            println!(
                "journal: {}（unhashed {} / {} 行），chain_tail={}..{}",
                if r.verified { "PASS" } else { "FAIL" },
                r.unhashed_lines,
                r.total_lines,
                &r.chain_tail[..8.min(r.chain_tail.len())],
                &r.chain_tail[r.chain_tail.len().saturating_sub(8)..]
            );
            if let Some(f) = &r.first_failure {
                println!("  首失配: {f}");
            }
            if let Some(expect) = &args.expect_tail {
                if r.chain_tail != *expect {
                    println!("  锚比对失配: 本地 {} vs 引擎锚 {}", r.chain_tail, expect);
                    return std::process::ExitCode::from(3);
                }
                println!("  锚比对通过（防整链重算实证口径）");
            }
            if r.verified {
                std::process::ExitCode::SUCCESS
            } else if r
                .first_failure
                .as_deref()
                .map(|f| f.contains("partial tail"))
                .unwrap_or(false)
            {
                std::process::ExitCode::from(5)
            } else {
                std::process::ExitCode::from(3)
            }
        }
        Err(e) => {
            eprintln!("读取失败: {e}");
            std::process::ExitCode::from(3)
        }
    }
}
