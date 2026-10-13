// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 内置 problem matcher:v1 两组($rustc gcc 形 / $tsc 形)。
//!
//! 行解析→结构化诊断(path/line/col/severity/code/msg),由运行器增量喂给
//! 前端诊断单源(owner=`task.<label>`)。gcc 形单行通吃(社区惯例:cargo 配
//! `--message-format=short` 产出该形态);JSON 模式/多行 pattern=v2。

#![forbid(unsafe_code)]

use serde::Serialize;

/// 解析出的单条诊断(行列 1-based,与诊断单源契约对齐)
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MatchedDiag {
    pub path: String,
    pub line: u32,
    pub col: u32,
    /// 1=Hint 2=Info 4=Warning 8=Error(Monaco severity 值,免前端换算)
    pub severity: u8,
    pub code: String,
    pub msg: String,
}

/// 按任务声明的 matcher 解析一行输出;不匹配返回 None(零损失:纯输出)。
pub fn parse_line(matcher: &str, line: &str) -> Option<MatchedDiag> {
    let trimmed = line.trim_start();
    match matcher {
        "$rustc" => parse_gcc_form(trimmed),
        "$tsc" => parse_tsc_form(trimmed),
        _ => None,
    }
}

/// gcc 形:`path:line:col: severity[code]?: message`
/// (rustc --message-format=short / gcc / clang 同族)
fn parse_gcc_form(line: &str) -> Option<MatchedDiag> {
    // 手写解析(正则依赖已在,但路径含冒号[Windows 盘符]时手写更稳:从右往左切)
    let (msg_part, rest) = split_severity_tail(line)?;
    let (path, line_no, col_no) = split_path_line_col(rest)?;
    Some(MatchedDiag {
        path,
        line: line_no,
        col: col_no,
        severity: msg_part.severity,
        code: msg_part.code,
        msg: msg_part.message,
    })
}

struct SeverityTail {
    severity: u8,
    code: String,
    message: String,
}

/// 从右找 `: error|warning|note` 分段(路径先行,消息随后)
fn split_severity_tail(line: &str) -> Option<(SeverityTail, &str)> {
    let mut best: Option<(usize, u8, &str)> = None;
    for kw in [
        ("error", 8u8),
        ("warning", 4u8),
        ("note", 2u8),
        ("help", 2u8),
    ] {
        let mut from = 0;
        while let Some(idx) = line[from..].find(kw.0) {
            let abs = from + idx;
            // 词边界:前导为 ": " 或 ":"
            let head_ok = abs >= 2 && &line[abs - 1..abs] == " ";
            let tail_is_colon = line[abs..].starts_with(kw.0);
            if head_ok
                && tail_is_colon
                && (line[abs + kw.0.len()..].starts_with(':')
                    || line[abs + kw.0.len()..].starts_with('['))
            {
                match best {
                    // 取最后一次出现(消息内也可能含 error 字样)
                    Some((prev, _, _)) if prev > abs => {}
                    _ => best = Some((abs, kw.1, kw.0)),
                }
            }
            from = abs + kw.0.len();
        }
    }
    let (idx, severity, kw) = best?;
    let head = &line[..idx].trim_end();
    let after_kw = &line[idx + kw.len()..];
    // tail 两形:`[CODE]: message`(rustc short)或 `: message`(gcc 裸形)
    let (code, message) = if let Some(rest) = after_kw.strip_prefix('[') {
        let close = rest.find(']')?;
        (
            rest[..close].to_string(),
            rest[close + 1..].trim_start_matches(':').trim().to_string(),
        )
    } else {
        (
            String::new(),
            after_kw.trim_start_matches(':').trim().to_string(),
        )
    };
    if message.is_empty() {
        return None;
    }
    Some((
        SeverityTail {
            severity,
            code,
            message,
        },
        head,
    ))
}

/// 头部 `path:line:col` 三段(从右往左切两个数字段;Windows 盘符冒号不受扰)
fn split_path_line_col(head: &str) -> Option<(String, u32, u32)> {
    // 尾随 ': '(severity 段切割残迹)先清,否则 rsplit 解析到空串
    let head = head.trim_end().trim_end_matches(':');
    let (head, col) = rsplit_number(head)?;
    let (head, line_no) = rsplit_number(head)?;
    if line_no == 0 || col == 0 {
        return None;
    }
    let path = head.trim_end_matches(':').trim().to_string();
    if path.is_empty() {
        return None;
    }
    Some((path, line_no, col))
}

/// 从右切一个 `:<数字>` 段
fn rsplit_number(s: &str) -> Option<(&str, u32)> {
    let idx = s.rfind(':')?;
    let num: u32 = s[idx + 1..].trim().parse().ok()?;
    Some((&s[..idx], num))
}

/// tsc 形:`path(line,col): severity CODE: message`
fn parse_tsc_form(line: &str) -> Option<MatchedDiag> {
    let open = line.find('(')?;
    let close = line[open..].find(')')? + open;
    let path = line[..open].trim().to_string();
    if path.is_empty() || !line[close..].starts_with("): ") {
        return None;
    }
    let mut dims = line[open + 1..close].split(',');
    let line_no: u32 = dims.next()?.trim().parse().ok()?;
    let col: u32 = dims.next()?.trim().parse().ok()?;
    if line_no == 0 || col == 0 {
        return None;
    }
    let rest = line[close + 3..].trim();
    let mut parts = rest.splitn(3, ' ');
    let severity_word = parts.next()?;
    let severity = match severity_word {
        "error" => 8u8,
        "warning" => 4u8,
        _ => return None,
    };
    let code = parts.next()?.trim_end_matches(':').to_string();
    let msg = parts.next()?.to_string();
    if msg.is_empty() {
        return None;
    }
    Some(MatchedDiag {
        path,
        line: line_no,
        col,
        severity,
        code,
        msg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rustc_short_form() {
        let d = parse_line(
            "$rustc",
            "src/lsp/bridge.rs:86:24: error[E0308]: mismatched types",
        )
        .unwrap();
        assert_eq!(d.path, "src/lsp/bridge.rs");
        assert_eq!(d.line, 86);
        assert_eq!(d.col, 24);
        assert_eq!(d.severity, 8);
        assert_eq!(d.code, "E0308");
        assert_eq!(d.msg, "mismatched types");
    }

    #[test]
    fn rustc_warning_without_code() {
        let d = parse_line("$rustc", "src/x.rs:12:5: warning: unused variable: `y`").unwrap();
        assert_eq!(d.severity, 4);
        assert_eq!(d.code, "");
        assert_eq!(d.msg, "unused variable: `y`");
    }

    #[test]
    fn gcc_windows_path_with_drive_colon() {
        let d = parse_line(
            "$rustc",
            r"D:\work\my proj\src\main.rs:3:1: error: expected `;`, found `}`",
        )
        .unwrap();
        assert_eq!(d.path, r"D:\work\my proj\src\main.rs");
        assert_eq!(d.line, 3);
        assert_eq!(d.severity, 8);
    }

    #[test]
    fn gcc_message_containing_error_word() {
        // 消息正文含 "error" 字样:应取最后一次合法分段
        let d = parse_line("$rustc", "src/a.rs:9:1: error: type error in expression").unwrap();
        assert_eq!(d.line, 9);
        assert_eq!(d.msg, "type error in expression");
    }

    #[test]
    fn gcc_non_diag_line_rejected() {
        assert!(parse_line("$rustc", "Compiling evo-agent v0.2.0").is_none());
        assert!(parse_line("$rustc", "warning: build failed").is_none()); // 无 path:line:col 头
        assert!(parse_line("$rustc", "").is_none());
    }

    #[test]
    fn tsc_form() {
        let d = parse_line(
            "$tsc",
            "web/src/main.ts(3,7): error TS2322: Type 'string' is not assignable to type 'number'.",
        )
        .unwrap();
        assert_eq!(d.path, "web/src/main.ts");
        assert_eq!(d.line, 3);
        assert_eq!(d.col, 7);
        assert_eq!(d.severity, 8);
        assert_eq!(d.code, "TS2322");
        assert_eq!(d.msg, "Type 'string' is not assignable to type 'number'.");
    }

    #[test]
    fn tsc_warning_form() {
        let d = parse_line(
            "$tsc",
            "a.ts(10,1): warning TS6133: 'x' is declared but never used.",
        )
        .unwrap();
        assert_eq!(d.severity, 4);
        assert_eq!(d.code, "TS6133");
    }

    #[test]
    fn tsc_non_diag_rejected() {
        assert!(parse_line("$tsc", "[11:22:33] Found 3 errors.").is_none());
        assert!(parse_line("$tsc", "Watching for file changes.").is_none());
    }

    #[test]
    fn unknown_matcher_passthrough_none() {
        assert!(parse_line("$go", "x.go:1:1: boom").is_none());
    }
}
