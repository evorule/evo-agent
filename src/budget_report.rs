// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! budget-report——est/true 预算偏差报告（规格修正批交付物 A/K-06）。
//!
//! 数据源：journal `llm_called` 事件——provider 真值 usage（`tokens.total`）
//! 对照估算值（`tokens_est`），两账天然同源（真值优先入账已实现），报告件
//! 只做聚合。挂点=独立 bin（`src/bin/budget_report.rs`）：journal schema
//! 唯一事实源在本仓，避免跨仓复制漂移（"独立 bin"设计许可项）。
//!
//! 输出（确定性，同输入逐字节一致）：per-session 样本表 + 全体 P50/P95
//! （nearest-rank）+ r 建议折算区间（附样本量）；偏离 ±50% 自动标注
//! "调参需另呈批"——**结论只出建议档，不直接改默认值**。

use crate::agent::journal::{read_all, JournalEvent};
use std::path::{Path, PathBuf};

/// r 默认值（配方 memory_budget_ratio 现行缺省口径；仅用于建议折算）
const R_DEFAULT: f64 = 0.25;

/// 单会话样本集（ratio = tokens_est / true_total）
#[derive(Debug, Clone)]
pub struct SessionStats {
    /// 会话 journal 文件名（去扩展名）
    pub session: String,
    /// 有效样本（est 与真值齐备且真值>0）的比值，写入序
    pub ratios: Vec<f64>,
}

/// 单会话分析：读 journal，抽取有效样本；无 llm 样本=Ok(None)
pub fn analyze_journal_file(path: &Path) -> Result<Option<SessionStats>, String> {
    let session = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let lines = read_all(path).map_err(|e| e.to_string())?;
    let mut ratios = Vec::new();
    for line in lines {
        if let JournalEvent::LlmCalled {
            tokens: Some(truth),
            tokens_est: Some(est),
            ..
        } = line.event
        {
            if truth.total > 0 {
                ratios.push(est as f64 / truth.total as f64);
            }
        }
    }
    if ratios.is_empty() {
        return Ok(None);
    }
    Ok(Some(SessionStats { session, ratios }))
}

/// 多目录分析：递归收集 *.jsonl（路径排序=确定性）；无法解析的文件跳过
/// 并计入 skips（报告中如实列出）。
pub fn analyze_dirs(dirs: &[PathBuf]) -> Result<(Vec<SessionStats>, Vec<String>), String> {
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        collect_jsonl(dir, &mut files)?;
    }
    files.sort();
    let mut sessions = Vec::new();
    let mut skips = Vec::new();
    for f in &files {
        match analyze_journal_file(f) {
            Ok(Some(st)) => sessions.push(st),
            Ok(None) => {}
            Err(e) => skips.push(format!("{}: {}", f.display(), e)),
        }
    }
    Ok((sessions, skips))
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("entry: {e}"))?;
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, out)?;
        } else if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
            out.push(path);
        }
    }
    Ok(())
}

/// nearest-rank 百分位（升序样本；文档化取位法=确定性）
fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let n = sorted.len();
    let idx = ((p * n as f64).ceil() as usize).clamp(1, n) - 1;
    Some(sorted[idx])
}

fn fmt4(v: f64) -> String {
    format!("{v:.4}")
}

/// 渲染报告（确定性；同输入逐字节一致）
pub fn render_report(sessions: &[SessionStats], skips: &[String], sources: &[String]) -> String {
    let mut out = String::new();
    out.push_str("=== budget-report（est/true 预算偏差报告） ===\n");
    out.push_str(&format!("source: {}\n", sources.join(", ")));
    let mut all: Vec<f64> = Vec::new();
    for st in sessions {
        all.extend_from_slice(&st.ratios);
        let mut sorted = st.ratios.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let vals = sorted
            .iter()
            .map(|r| fmt4(*r))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "session {} samples={} ratios=[{}]\n",
            st.session,
            st.ratios.len(),
            vals
        ));
    }
    all.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if all.is_empty() {
        out.push_str("samples: 0（无有效 est/true 配对样本；报告仅骨架）\n");
        return out;
    }
    let p50 = percentile(&all, 0.50).unwrap_or(0.0);
    let p95 = percentile(&all, 0.95).unwrap_or(0.0);
    out.push_str(&format!(
        "全体: samples={} P50={} P95={}\n",
        all.len(),
        fmt4(p50),
        fmt4(p95)
    ));
    if p50 > 0.0 && p95 > 0.0 {
        let lo = R_DEFAULT / p95;
        let hi = R_DEFAULT / p50;
        out.push_str(&format!(
            "r 建议折算区间(基 {R_DEFAULT}): [{}, {}]（est/true<1=估算低估真实消耗偏高需上调;>1=估算偏高可下调;附样本量 {}）\n",
            fmt4(lo),
            fmt4(hi),
            all.len()
        ));
    }
    if !(0.5..=1.5).contains(&p50) {
        out.push_str("⚠ est/true 偏离超 ±50%——调参需另呈批,本报告不改默认值\n");
    }
    if !skips.is_empty() {
        out.push_str(&format!("skipped: {}\n", skips.join("; ")));
    }
    out.push_str(
        "(注) 结论为建议档,不改默认值;token 为 provider 真值口径,估算为 tokens_est 口径\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::journal::{JournalWriter, TokenRecord};

    fn write_fixture_journal(dir: &Path, session: &str, rows: &[(Option<u64>, Option<u64>)]) {
        let w = JournalWriter::open(dir, session).unwrap();
        for (truth_total, est) in rows {
            let tokens = truth_total.map(|t| TokenRecord {
                prompt: t,
                completion: 0,
                total: t,
            });
            w.llm_called_react("m", None, tokens, *est, 3, "r").unwrap();
        }
    }

    #[test]
    fn test_render_report_byte_level() {
        // 合成 fixture journal → 期望报告逐字节（验收①）
        let dir = std::env::temp_dir().join(format!("budget-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_fixture_journal(
            &dir,
            "sess-a",
            &[
                (Some(100), Some(90)),
                (Some(200), Some(220)),
                (None, Some(50)),
            ],
        );
        write_fixture_journal(&dir, "sess-b", &[(Some(400), Some(380))]);

        let (sessions, skips) = analyze_dirs(std::slice::from_ref(&dir)).unwrap();
        assert_eq!(skips.len(), 0);
        assert_eq!(sessions.len(), 2);
        let report = render_report(&sessions, &skips, &[dir.display().to_string()]);

        let lines = report.lines().collect::<Vec<_>>();
        // 会话行按路径排序——与本机序无关地校验业务字段行(逐字节)
        let header = lines[0];
        assert_eq!(header, "=== budget-report（est/true 预算偏差报告） ===");
        let all_line = lines.iter().find(|l| l.starts_with("全体: ")).unwrap();
        assert_eq!(
            *all_line, "全体: samples=3 P50=0.9500 P95=1.1000",
            "P50/P95 nearest-rank 逐字节"
        );
        let r_line = lines
            .iter()
            .find(|l| l.starts_with("r 建议折算区间"))
            .unwrap();
        assert_eq!(
            *r_line,
            "r 建议折算区间(基 0.25): [0.2273, 0.2632]（est/true<1=估算低估真实消耗偏高需上调;>1=估算偏高可下调;附样本量 3）"
        );
        // sess-a:est 缺失行跳过 → 恰 2 样本(0.9/1.1);sess-b 恰 1 样本(0.95)
        let sa = lines
            .iter()
            .find(|l| l.starts_with("session sess-a"))
            .unwrap();
        assert_eq!(*sa, "session sess-a samples=2 ratios=[0.9000, 1.1000]");
        let sb = lines
            .iter()
            .find(|l| l.starts_with("session sess-b"))
            .unwrap();
        assert_eq!(*sb, "session sess-b samples=1 ratios=[0.9500]");
        // 同输入两次运行逐字节一致（确定性自证）
        let report2 = render_report(&sessions, &skips, &[dir.display().to_string()]);
        assert_eq!(report, report2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_deviation_annotation_and_empty() {
        // 偏离 ±50% → 标注行;空样本 → 骨架报告
        let biased = vec![SessionStats {
            session: "s".into(),
            ratios: vec![0.3, 0.4],
        }];
        let rep = render_report(&biased, &[], &["src".into()]);
        assert!(rep.contains("偏离超 ±50%"), "{rep}");
        let empty = render_report(&[], &[], &["src".into()]);
        assert!(empty.contains("samples: 0"), "{empty}");
    }
}
