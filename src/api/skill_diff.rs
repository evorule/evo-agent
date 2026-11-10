// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 待审批 diff 工具（更新审批复核面）：已批准原文 → 盘上待审文本的
//! unified diff。
//!
//! - 行级 LCS 对齐（确定性；同输入恒同输出，逐字节可复算——审批面
//!   展示的数据必须可复算，禁启发式）；
//! - unified 形态（`---`/`+++`/`@@ -a,b +c,d @@`，3 行上下文）；
//! - 零新依赖（标准库实现）；体量帽防超大 diff 撑爆审批视图（截断
//!   显式标注，不静默）。

/// diff 体量帽（字符）：超过即截断并加标注行——审批面永远展示完整意图
/// 或明确告知被截断，不给"看似完整实则截断"的中间态。
const DIFF_CAP_CHARS: usize = 256 * 1024;

/// 行级 LCS 长度表（O(n·m)；技能正文量级下无性能压力）
fn lcs_table(a: &[&str], b: &[&str]) -> Vec<usize> {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![0usize; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * (m + 1) + j] = if a[i] == b[j] {
                dp[(i + 1) * (m + 1) + j + 1] + 1
            } else {
                dp[(i + 1) * (m + 1) + j].max(dp[i * (m + 1) + j + 1])
            };
        }
    }
    dp
}

/// LCS 对齐产出的编辑脚本：`(tag, old_line_no, new_line_no, line)`，
/// tag ∈ {ctx(0), del(-1), add(1)}；行号 1-based。
fn edit_script<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<(i8, usize, usize, &'a str)> {
    let dp = lcs_table(a, b);
    let (n, m) = (a.len(), b.len());
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((0, i + 1, j + 1, a[i]));
            i += 1;
            j += 1;
        } else if dp[(i + 1) * (m + 1) + j] >= dp[i * (m + 1) + j + 1] {
            out.push((-1, i + 1, j + 1, a[i]));
            i += 1;
        } else {
            out.push((1, i + 1, j + 1, b[j]));
            j += 1;
        }
    }
    while i < n {
        out.push((-1, i + 1, j + 1, a[i]));
        i += 1;
    }
    while j < m {
        out.push((1, i + 1, j + 1, b[j]));
        j += 1;
    }
    out
}

/// unified diff（3 行上下文；确定性；超过 256KiB 截断加标注）
pub fn unified_diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a == b {
        return String::new();
    }
    let script = edit_script(&a, &b);
    let mut out = String::new();
    out.push_str(&format!("--- {old_label}\n+++ {new_label}\n"));

    // hunk 切分：相邻编辑点窗口 ≤ 2*context 合并
    const CTX: usize = 3;
    let mut idx = 0usize;
    while idx < script.len() {
        // 跳过前导上下文，找到下一个编辑点
        while idx < script.len() && script[idx].0 == 0 {
            idx += 1;
        }
        if idx >= script.len() {
            break;
        }
        let start = idx.saturating_sub(CTX);
        // 向后扩展 hunk：编辑点间距 ≤ 2*CTX 连续合并
        let mut end = idx;
        let mut last_edit = idx;
        for (k, item) in script.iter().enumerate().skip(idx) {
            if item.0 != 0 {
                last_edit = k;
                end = k;
            } else if k - last_edit <= 2 * CTX {
                end = k;
            }
        }
        end = (end + CTX).min(script.len() - 1);

        let old_start = script[start].1;
        let new_start = script[start].2;
        let old_count = script[start..=end].iter().filter(|e| e.0 != 1).count();
        let new_count = script[start..=end].iter().filter(|e| e.0 != -1).count();
        // 纯插入/纯删除 hunk 的行号约定：count=0 时 start 指向「前一行」
        // （标准 unified 语义；空旧版顶头插入 → -0,0）
        let (old_start, new_start) = (
            if old_count == 0 {
                old_start.saturating_sub(1)
            } else {
                old_start
            },
            if new_count == 0 {
                new_start.saturating_sub(1)
            } else {
                new_start
            },
        );
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            old_start, old_count, new_start, new_count
        ));
        for e in &script[start..=end] {
            let prefix = match e.0 {
                0 => ' ',
                -1 => '-',
                _ => '+',
            };
            out.push(prefix);
            out.push_str(e.3);
            out.push('\n');
        }
        idx = end + 1;
    }

    if out.len() > DIFF_CAP_CHARS {
        out.truncate(DIFF_CAP_CHARS);
        out.push_str(&format!(
            "\n[diff truncated at {} chars — review the full pending text above]\n",
            DIFF_CAP_CHARS
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn identical_inputs_empty_diff() {
        assert_eq!(unified_diff("a\nb\n", "a\nb\n", "o", "n"), "");
    }

    #[test]
    fn single_line_change_produces_hunk() {
        let d = unified_diff(
            "line1\nline2\nline3\n",
            "line1\nCHANGED\nline3\n",
            "old",
            "new",
        );
        assert!(d.starts_with("--- old\n+++ new\n"));
        assert!(d.contains("@@ -1,3 +1,3 @@"));
        assert!(d.contains("-line2"));
        assert!(d.contains("+CHANGED"));
        assert!(d.contains(" line1"), "上下文行保留");
    }

    #[test]
    fn addition_and_removal_tags() {
        let d = unified_diff("keep\n", "keep\nadded\n", "o", "n");
        assert!(d.contains("+added"));
        assert!(
            !d.lines()
                .any(|l| l.starts_with('-') && !l.starts_with("---")),
            "纯新增无删除行（头部 --- 不算）: {d}"
        );
        let d2 = unified_diff("keep\nold-line\n", "keep\n", "o", "n");
        assert!(d2.contains("-old-line"));
    }

    #[test]
    fn deterministic_double_run() {
        let old = (0..50).map(|i| format!("line-{i}")).collect::<Vec<_>>();
        let mut new = old.clone();
        new[10] = "CHANGED".into();
        new.push("tail".into());
        let (o, n) = (old.join("\n").to_string(), new.join("\n").to_string());
        let d1 = unified_diff(&o, &n, "a", "b");
        let d2 = unified_diff(&o, &n, "a", "b");
        assert_eq!(d1, d2, "同输入双跑逐位一致");
        assert!(d1.contains("@@"));
    }

    #[test]
    fn cap_truncates_with_marker() {
        let big = "x".repeat(DIFF_CAP_CHARS + 1000);
        let d = unified_diff("", &big, "o", "n");
        assert!(d.len() <= DIFF_CAP_CHARS + 200, "截断后不超帽+标注余量");
        assert!(d.contains("[diff truncated"), "截断必须显式标注");
    }

    #[test]
    fn empty_old_all_added() {
        let d = unified_diff("", "a\nb\n", "o", "n");
        assert!(d.contains("@@ -0,0 +1,2 @@"));
        assert!(d.contains("+a") && d.contains("+b"));
    }
}
