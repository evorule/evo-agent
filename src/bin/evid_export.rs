// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! evid-export：从 evo-agent journal 导出标准化 EVID 证据文档。
//!
//! journal 的逐行链哈希三元组即 EVID 投影的事实哈希源——纯本地操作，
//! 不需要访问 evorule-server。投影逻辑在 lib（`agent::evid::project_journal`），
//! 链三元组校验 fail-closed：journal 链已断时不产出证据文档（退出码 3）。
//!
//! 投影器全程无密钥——签名由部署侧锚点密钥体系附加（见 evid_verify.py）。

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(about = "EVID 证据投影：从 evo-agent journal 导出标准化证据文档")]
struct Args {
    /// journal 文件路径（data/sessions/{session_id}.jsonl）
    #[arg(long)]
    journal: PathBuf,

    /// 输出文件路径
    #[arg(long, short = 'o')]
    output: PathBuf,

    /// 链标识（默认 = journal 文件名去扩展名）
    #[arg(long)]
    chain_id: Option<String>,
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();

    let content = match std::fs::read_to_string(&args.journal) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ERROR: journal 读取失败: {e}");
            return std::process::ExitCode::from(3);
        }
    };

    let chain_id = args.chain_id.clone().unwrap_or_else(|| {
        args.journal
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    });

    let proj = match evo_agent::agent::evid::project_journal(&content, &chain_id) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ERROR: {e}");
            return std::process::ExitCode::from(3);
        }
    };

    let output = match serde_json::to_string_pretty(&proj.chain) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("ERROR: serialize: {e}");
            return std::process::ExitCode::from(1);
        }
    };

    if let Err(e) = std::fs::write(&args.output, output) {
        eprintln!("ERROR: write: {e}");
        return std::process::ExitCode::from(1);
    }

    println!(
        "EVID exported: {} records, chain_root={}",
        proj.chain.chain_length,
        &proj.chain.chain_root[..24.min(proj.chain.chain_root.len())]
    );
    if proj.skipped_unhashed > 0 {
        println!(
            "note: {} legacy unhashed lines skipped (not projected)",
            proj.skipped_unhashed
        );
    }
    std::process::ExitCode::SUCCESS
}
