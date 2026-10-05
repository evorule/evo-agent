// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! budget-report——est/true 预算偏差报告独立入口（规格修正批交付物 A/K-06）。
//!
//! 用法：
//! ```text
//! budget-report <runs-dir> [more-dirs...]
//! ```
//!
//! 递归收集各目录下 *.jsonl journal，聚合 llm_called 事件的 est/true 配对
//! 样本，输出 per-session 表+全体 P50/P95+r 建议折算区间（确定性，同输入
//! 逐字节一致）。**结论只出建议档，不直接改默认值**；偏离 ±50% 标注另呈批。
//! 挂点说明：journal schema 唯一事实源在本仓，独立 bin 避免 schema 跨仓
//! 复制漂移（24 号批次一许可项）。
use evo_agent::budget_report::analyze_dirs;
use std::path::PathBuf;

fn main() {
    let dirs: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if dirs.is_empty() {
        eprintln!("用法: budget-report <runs-dir> [more-dirs...]");
        std::process::exit(2);
    }
    match analyze_dirs(&dirs) {
        Ok((sessions, skips)) => {
            let sources: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
            print!("{}", evo_agent::budget_report::render_report(&sessions, &skips, &sources));
        }
        Err(e) => {
            eprintln!("分析失败: {e}");
            std::process::exit(1);
        }
    }
}
