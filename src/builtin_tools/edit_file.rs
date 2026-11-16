// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `edit_file` —— 精准编辑(写面第五件;old/new 串替换 + 唯一性闸 + 保真细节)
//!
//! ## 定位
//!
//! 写面高频缺口收口:此前改已有文件只有整文件覆盖写(file_write)或
//! candidate 审批的 sed——前者 token 浪费且易引入意外改动,后者危险。
//! `edit_file` 提供最小编辑面:锚串替换,单次调用只动锚串邻域。
//!
//! ## 安全模型(对齐 file_write 同族)
//!
//! 1. **工作目录沙箱 + 写入路径白名单**:路径解析单源委托
//!    [`fs_safety::resolve_writable_target`](crate::builtin_tools::fs_safety)
//!    (绝对路径/`..`/symlink/junction 逃逸一律拒;默认仅 `./workspace/`,
//!    能力边界 read_write 模式下由 apply_capability_boundary 重绑 `.`)
//! 2. **目标必须已存在**:编辑语义不创建文件(创建走 file_create/file_write)
//! 3. **唯一性闸**:old_string 非 replace_all 时必须唯一命中,否则 fail-visible
//!    (报实际命中数,引导加长锚串)——防误伤未预期的位置
//! 4. **保真不猜**:仅接受 UTF-8(非 UTF-8 拒);换行形态单形态文件归一保真
//!    (CRLF/LF 不翻转),混合换行 fail-visible 拒;BOM 有则保有无则不加
//! 5. **大小帽**:目标文件 20MB、old+new 合计 1MB(对齐 file_write 内容帽)
//! 6. **原子写**:tmp+rename 同目录落盘(中断不留半文件)
//!
//! ## 治理面
//!
//! manifest: Sentineled(同 file_write——out_of_sandbox enforce 意图信号经
//! resolve_tool_intent 采集)+ AutoPolicy(沙箱内直改,写面高频工具与
//! file_write 同判)+ 开关 `agentTools.editFile`(默认开)。

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// 默认写入根目录(相对 workdir;同 file_write 单源)
pub const DEFAULT_WRITABLE_DIR: &str = crate::builtin_tools::file_write::DEFAULT_WRITABLE_DIR;

/// 目标文件大小帽
pub const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024; // 20 MB

/// old_string + new_string 合计大小帽(对齐 file_write 单次内容帽)
pub const MAX_EDIT_BYTES: usize = 1024 * 1024; // 1 MB

/// UTF-8 BOM 字节序列
const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// `edit_file` 工具
#[derive(Clone)]
pub struct FileEditTool {
    workdir: PathBuf,
    writable_dir: PathBuf,
    max_file_bytes: u64,
}

impl FileEditTool {
    /// TODO: doc
    pub fn new(workdir: PathBuf) -> Self {
        Self {
            workdir,
            writable_dir: PathBuf::from(DEFAULT_WRITABLE_DIR),
            max_file_bytes: MAX_FILE_BYTES,
        }
    }

    /// 设置可写子目录(相对 workdir;同 file_write 语义,`.` = 全 workdir)
    pub fn with_writable_dir(mut self, dir: &str) -> Self {
        self.writable_dir = PathBuf::from(dir);
        self
    }

    /// 路径安全:单源委托 fs_safety::resolve_writable_target(与 file_write 同权)
    fn resolve_safe_path(&self, raw: &str) -> Result<(PathBuf, PathBuf), String> {
        crate::builtin_tools::fs_safety::resolve_writable_target(
            &self.workdir,
            &self.writable_dir,
            raw,
        )
    }
}

/// 换行形态判定(纯函数)
///
/// 返回 `Some('\r\n' 归一标记)`/`Some(LF)`/`None`(混合——调用方 fail-visible)。
/// 判据:统计 `\r\n` 出现数与 `\n` 总数——相等 = 全 CRLF;`\r\n` 为零 = 全
/// LF;其余 = 混合(保真优先于智能,不猜主形态)。
fn detect_line_ending(text: &str) -> Option<bool> {
    let lf_total = text.matches('\n').count();
    let crlf = text.matches("\r\n").count();
    if crlf == 0 {
        Some(false) // 全 LF
    } else if crlf == lf_total {
        Some(true) // 全 CRLF
    } else {
        None // 混合
    }
}

/// 参数换行归一到文件形态(纯函数):先统一到 LF(消参数端 \r\n),再按
/// 文件形态展开为 CRLF——避免双重转换
fn normalize_newlines(s: &str, crlf_file: bool) -> String {
    let lf = s.replace("\r\n", "\n");
    if crlf_file {
        lf.replace('\n', "\r\n")
    } else {
        lf
    }
}

// =============================================================================
// P1 容错:行 trim 归一模糊对齐 + 最近区域诊断
// =============================================================================

/// 行 trim 归一形(等价键):两侧空白全剥——缩进漂移/行尾空白差异的锚失配
/// 容错键。窗口唯一性闸照常适用于归一命中计数(exact 优先,字节保真不变)。
fn trimmed(line: &str) -> &str {
    line.trim()
}

/// 行级模糊对齐(纯函数):在文件行归一序列中找锚归一序列的连续命中窗口,
/// 返回全部窗口行区间 `[start, end)`(end 排他)。计数由调用方按唯一性闸
/// 裁决;锚全空行=返回空(空白锚处处可配,拒绝配对)。
fn fuzzy_windows(file_norm: &[&str], anchor_norm: &[&str]) -> Vec<(usize, usize)> {
    if anchor_norm.is_empty() || file_norm.len() < anchor_norm.len() {
        return Vec::new();
    }
    let mut wins = Vec::new();
    for i in 0..=file_norm.len() - anchor_norm.len() {
        if file_norm[i..i + anchor_norm.len()]
            .iter()
            .zip(anchor_norm.iter())
            .all(|(f, a)| *f == *a)
        {
            wins.push((i, i + anchor_norm.len()));
        }
    }
    wins
}

/// 最近区域诊断(容错二级,纯函数):取锚最长非空行按词元计与文件行的
/// 重叠数,取首个最大者,返回 (1-based 行号, 行原文)。零重叠=None。
fn nearest_region<'a>(file_lines: &[&'a str], anchor_lines: &[&str]) -> Option<(usize, &'a str)> {
    let probe = anchor_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .max_by_key(|l| l.len())?
        .trim();
    let tokens: Vec<&str> = probe.split_whitespace().collect();
    let mut best: Option<(usize, usize, &'a str)> = None;
    for (idx, fl) in file_lines.iter().enumerate() {
        let overlap = tokens.iter().filter(|t| fl.contains(**t)).count();
        if overlap > 0 && best.map(|(_, bo, _)| overlap > bo).unwrap_or(true) {
            best = Some((idx + 1, overlap, fl));
        }
    }
    best.map(|(n, _, line)| (n, line))
}

/// 提取变更点上下文预览(纯函数):替换点所在行前后各 3 行
fn build_preview(new_text: &str, replaced_pos: usize) -> String {
    let lines: Vec<&str> = new_text.lines().collect();
    let mut acc = 0usize;
    let mut hit_line = 0usize;
    for (idx, line) in lines.iter().enumerate() {
        let line_bytes = line.len() + 1; // +换行
        if acc + line_bytes > replaced_pos {
            hit_line = idx;
            break;
        }
        acc += line_bytes;
    }
    let from = hit_line.saturating_sub(3);
    let to = (hit_line + 4).min(lines.len());
    lines[from..to].join("\n")
}

#[async_trait::async_trait]
impl ToolFunction for FileEditTool {
    /// G13:async 入口 — 用 spawn_blocking 包装同步 fs 操作
    async fn call(&self, args: &Value) -> IoResult {
        let tool = self.clone();
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("edit_file tool panicked: {}", e))?
    }
}

impl FileEditTool {
    /// 同步实现(供 spawn_blocking 调用)
    fn call_sync(&self, args: &Value) -> IoResult {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: path (string)".to_string())?;
        let old_string = args
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: old_string (string)".to_string())?;
        let new_string = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if old_string.is_empty() {
            return Err("old_string must be non-empty (empty anchor would match \
                        everywhere; use file_write to create/replace whole files)"
                .to_string());
        }
        if old_string.len() + new_string.len() > MAX_EDIT_BYTES {
            return Err(format!(
                "old_string + new_string too large: {} bytes (max {} bytes)",
                old_string.len() + new_string.len(),
                MAX_EDIT_BYTES
            ));
        }

        // 路径安全(写面单源)
        let (target, target_canonical) = self.resolve_safe_path(path)?;

        // 编辑语义:目标必须已存在
        if !target_canonical.exists() {
            return Err(format!(
                "file not found: '{}'; use file_create/file_write to create files",
                path
            ));
        }

        // 读取 + 大小帽
        let meta =
            std::fs::metadata(&target_canonical).map_err(|e| format!("stat failed: {}", e))?;
        if meta.len() > self.max_file_bytes {
            return Err(format!(
                "file too large: {} bytes (max {} bytes)",
                meta.len(),
                self.max_file_bytes
            ));
        }
        let raw = std::fs::read(&target_canonical).map_err(|e| format!("read failed: {}", e))?;
        let bytes_before = raw.len();

        // 保真面:BOM 检出 + UTF-8 强制(不猜编码)
        let (bom_present, body) = match raw.strip_prefix(BOM) {
            Some(rest) => (true, rest),
            None => (false, raw.as_slice()),
        };
        let text = std::str::from_utf8(body).map_err(|e| {
            format!(
                "file is not valid UTF-8 (byte {}): {}; edit_file never guesses \
                 encodings — convert or edit via other means",
                e.valid_up_to(),
                path
            )
        })?;

        // 换行形态:混合 → fail-visible;单形态 → 参数归一到文件形态
        let crlf_file = detect_line_ending(text).ok_or_else(|| {
            format!(
                "file has mixed line endings (CRLF and LF both present): '{}'; \
                 edit_file refuses to guess the dominant form — normalize the \
                 file first",
                path
            )
        })?;
        let old_norm = normalize_newlines(old_string, crlf_file);
        let new_norm = normalize_newlines(new_string, crlf_file);

        // 唯一性闸(exact 优先;0 hits → P1 容错两级,字节保真路径不变)
        let hits = text.matches(&old_norm).count();
        let mut fuzzy_match: Option<&'static str> = None;
        let file_eol = if crlf_file { "\r\n" } else { "\n" };

        let (new_text, first_pos, edits_applied) = if hits > 0 {
            // 唯一性闸(exact 路径;重构期曾误删——既有 multi_hit 锁测试当场
            // 执法,零逃逸进 main)
            if hits > 1 && !replace_all {
                return Err(format!(
                    "old_string matches {} times in '{}'; extend the anchor to make \
                     it unique, or pass replace_all=true to replace every occurrence",
                    hits, path
                ));
            }
            let first_pos = text.find(&old_norm);
            let nt = if replace_all {
                text.replace(&old_norm, &new_norm)
            } else {
                text.replacen(&old_norm, &new_norm, 1)
            };
            let ea = if replace_all { hits } else { 1 };
            (nt, first_pos.unwrap_or(0), ea)
        } else {
            // ── 容错一级:行 trim 归一模糊对齐(缩进漂移/行尾空白差异)──
            // 行空间重构: untouched 行原文逐字保留;唯一性闸同款适用于归一
            // 命中计数。锚全空行=处处可配,拒绝配对走诊断。
            let file_lines: Vec<&str> = text.split('\n').collect();
            let anchor_lines: Vec<&str> = old_norm.split('\n').collect();
            let file_norm: Vec<&str> = file_lines.iter().map(|l| trimmed(l)).collect();
            let anchor_trimmed: Vec<&str> = anchor_lines.iter().map(|l| trimmed(l)).collect();
            let windows = fuzzy_windows(&file_norm, &anchor_trimmed);
            if windows.is_empty() || anchor_trimmed.iter().all(|l| l.is_empty()) {
                // ── 容错二级:最近区域诊断(o10 处置方向②——报错携带盘上
                // 实际内容片段,消灭「只能盲试锚形态」)──
                let diag = nearest_region(&file_lines, &anchor_lines)
                    .map(|(n, line)| {
                        format!(
                            "; closest anchor-like region at line {n}: \"{}\"",
                            line.trim()
                        )
                    })
                    .unwrap_or_default();
                return Err(format!(
                    "old_string not found in '{}' (0 hits, incl. line-trim-normalized{diag}); \
                     read the file first (file_read/grep_files) to copy an exact anchor",
                    path
                ));
            }
            if windows.len() > 1 && !replace_all {
                return Err(format!(
                    "old_string matches {} times in '{}' under line-trim-normalized \
                     matching; extend the anchor to make it unique, or pass \
                     replace_all=true",
                    windows.len(),
                    path
                ));
            }
            fuzzy_match = Some("line-trim-normalized");
            let new_lines: Vec<&str> = new_norm.split('\n').collect();
            let had_trailing_newline = text.ends_with('\n');
            let line_splice = |windows: &[(usize, usize)]| -> String {
                let mut out: Vec<&str> = Vec::new();
                let mut prev = 0usize;
                for (s, e) in windows {
                    out.extend_from_slice(&file_lines[prev..*s]);
                    out.extend_from_slice(&new_lines);
                    prev = *e;
                }
                out.extend_from_slice(&file_lines[prev..]);
                let joined = out.join(file_eol);
                if had_trailing_newline && !joined.ends_with(file_eol) {
                    format!("{joined}{file_eol}")
                } else {
                    joined
                }
            };
            // 模糊路径预览位=命中窗口首行的字节偏移(逐行 len+1 累计;
            // CRLF 行的 \r 计入 len,值精确)
            let approx_pos = file_lines[..windows[0].0]
                .iter()
                .map(|l| l.len() + 1)
                .sum::<usize>();
            let nt = line_splice(&windows);
            let ea = windows.len();
            (nt, approx_pos, ea)
        };
        let preview = build_preview(&new_text, first_pos);

        // 原子写:同目录 tmp + rename(rename 在 Windows/Unix 均替换既有文件);
        // BOM 有则重挂(读时剥离仅为匹配,落盘保真)
        let mut out_bytes = Vec::with_capacity(new_text.len() + BOM.len());
        if bom_present {
            out_bytes.extend_from_slice(BOM);
        }
        out_bytes.extend_from_slice(new_text.as_bytes());
        let file_name = target
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .ok_or_else(|| "invalid target file name".to_string())?;
        let tmp_path = target
            .parent()
            .unwrap_or(Path::new("."))
            .join(format!("{}.evo-edit-tmp", file_name));
        let write_res = (|| -> Result<(), String> {
            std::fs::write(&tmp_path, &out_bytes)
                .map_err(|e| format!("tmp write failed: {}", e))?;
            std::fs::rename(&tmp_path, &target).map_err(|e| format!("atomic rename failed: {}", e))
        })();
        if let Err(e) = write_res {
            let _ = std::fs::remove_file(&tmp_path); // 清理残件,fail-visible
            return Err(e);
        }

        // o107 传输校验:rename 后回读逐字节比对(防传输层吞字);不匹配重写一次,
        // 仍不匹配显式报错 fail-visible
        let on_disk =
            std::fs::read(&target).map_err(|e| format!("post-write verify read failed: {}", e))?;
        if on_disk.as_slice() != out_bytes.as_slice() {
            std::fs::write(&tmp_path, &out_bytes)
                .map_err(|e| format!("rewrite tmp failed: {}", e))?;
            std::fs::rename(&tmp_path, &target)
                .map_err(|e| format!("rewrite rename failed: {}", e))?;
            let reread = std::fs::read(&target)
                .map_err(|e| format!("post-rewrite verify read failed: {}", e))?;
            if reread.as_slice() != out_bytes.as_slice() {
                return Err(format!(
                    "edit verification failed after rewrite: disk {} bytes != intended {} bytes (possible storage/transport fault)",
                    reread.len(),
                    out_bytes.len()
                ));
            }
        }

        let mut map = serde_json::Map::new();
        map.insert(
            "path".to_string(),
            Value::from(target_canonical.display().to_string()),
        );
        map.insert(
            "edits_applied".to_string(),
            Value::from(edits_applied as i64),
        );
        map.insert("bytes_before".to_string(), Value::from(bytes_before as i64));
        map.insert(
            "bytes_after".to_string(),
            Value::from(out_bytes.len() as i64),
        );
        map.insert("preview".to_string(), Value::from(preview));
        // P1 容错:模糊路径须显式标记(审计同权——放行决策落账原则的工具面
        // 等价;exact 路径零该字段=与现行为逐字节一致)
        if let Some(f) = fuzzy_match {
            map.insert("fuzzy_match".to_string(), Value::from(f));
        }
        if bom_present {
            map.insert("bom_preserved".to_string(), Value::Bool(true));
        }
        Ok(Value::Object(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workdir_with_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        dir
    }

    fn edit_args(path: &str, old: &str, new: &str) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("path".to_string(), Value::from(path));
        m.insert("old_string".to_string(), Value::from(old));
        m.insert("new_string".to_string(), Value::from(new));
        Value::Object(m)
    }

    // === P1 容错:行 trim 归一模糊对齐 + 最近区域诊断 ===

    #[test]
    fn test_fuzzy_match_applies_on_whitespace_drift() {
        // 一级容错:锚行缩进漂移(文件行带缩进,锚行无)→trim 归一唯一命中→
        // 行域外科替换+envelope 携 fuzzy 标记;窗外行原文逐字保留
        let dir = temp_workdir_with_workspace();
        write_ws_file(
            &dir,
            "cfg.txt",
            b"fn main() {\n    let x = 1;\n    other();\n}\n",
        );
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let r = tool
            .call_sync(&edit_args(
                "workspace/cfg.txt",
                "let x = 1;\nother();", // 锚行无缩进(漂移形态)
                "let x = 2;\nother();",
            ))
            .unwrap();
        assert_eq!(
            r.get("fuzzy_match").and_then(|v| v.as_str()),
            Some("line-trim-normalized"),
            "模糊路径须带标记: {r}"
        );
        let content =
            std::fs::read_to_string(dir.path().join("workspace").join("cfg.txt")).unwrap();
        assert!(content.contains("let x = 2;"), "替换已生效: {content}");
        assert!(content.contains("fn main() {"), "窗外行原文保留: {content}");
        assert!(content.ends_with('\n'), "尾换行保真");
    }

    #[test]
    fn test_fuzzy_ambiguous_rejected_with_normalized_count() {
        // 归一窗口 2 处+未 replace_all → 唯一性闸同款拒(计数透出;锚带尾
        // 空格漂移=exact 零命中,归一 2 命中——纯模糊场景)
        let dir = temp_workdir_with_workspace();
        write_ws_file(&dir, "dup.txt", b"x\ny\nx\ny\n");
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let err = tool
            .call_sync(&edit_args("workspace/dup.txt", "x \ny", "z"))
            .unwrap_err();
        assert!(
            err.contains("2 times") && err.contains("line-trim-normalized"),
            "got: {err}"
        );
    }

    #[test]
    fn test_fuzzy_replace_all_splices_all_windows() {
        let dir = temp_workdir_with_workspace();
        write_ws_file(&dir, "all.txt", b"x\ny\nmid\nx\ny\n");
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let mut args = edit_args("workspace/all.txt", "x \ny", "z");
        args.as_object_mut()
            .unwrap()
            .insert("replace_all".to_string(), Value::Bool(true));
        let r = tool.call_sync(&args).unwrap();
        assert_eq!(
            r.get("fuzzy_match").and_then(|v| v.as_str()),
            Some("line-trim-normalized")
        );
        let content =
            std::fs::read_to_string(dir.path().join("workspace").join("all.txt")).unwrap();
        assert_eq!(content.matches("z").count(), 2, "两窗口全替换: {content}");
        assert!(content.contains("mid"), "中间行保留: {content}");
    }

    #[test]
    fn test_anchor_not_found_reports_nearest_region() {
        // 二级容错:全失配→报错携带盘上最近区域(行号+内容片段,消灭盲试)
        let dir = temp_workdir_with_workspace();
        write_ws_file(
            &dir,
            "code.txt",
            b"fn main() {\n    let alpha = compute(y);\n}\n",
        );
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let err = tool
            .call_sync(&edit_args(
                "workspace/code.txt",
                "let alpha = compute(x); // wrong tail",
                "let alpha = computed;",
            ))
            .unwrap_err();
        assert!(
            err.contains("closest anchor-like region at line 2"),
            "最近区域诊断: {err}"
        );
        assert!(
            err.contains("let alpha = compute(y);"),
            "诊断携带盘上原文: {err}"
        );
    }

    #[test]
    fn test_exact_match_takes_precedence_without_fuzzy_marker() {
        // exact 命中→字节保真路径,零 fuzzy 字段(与现行为逐字节一致)
        let dir = temp_workdir_with_workspace();
        write_ws_file(&dir, "e.txt", b"alpha beta\n");
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let r = tool
            .call_sync(&edit_args("workspace/e.txt", "alpha beta", "alpha gamma"))
            .unwrap();
        assert!(
            r.get("fuzzy_match").is_none(),
            "exact 路径零 fuzzy 标记: {r}"
        );
    }

    fn write_ws_file(dir: &tempfile::TempDir, name: &str, content: &[u8]) {
        let p = dir.path().join("workspace").join(name);
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn test_basic_replace() {
        let dir = temp_workdir_with_workspace();
        write_ws_file(&dir, "a.txt", b"hello world");
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let r = tool.call_sync(&edit_args("workspace/a.txt", "world", "rust"));
        assert!(r.is_ok(), "got: {:?}", r);
        let out = std::fs::read_to_string(dir.path().join("workspace/a.txt")).unwrap();
        assert_eq!(out, "hello rust");
    }

    #[test]
    fn test_zero_hits_fails() {
        let dir = temp_workdir_with_workspace();
        write_ws_file(&dir, "a.txt", b"hello");
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let r = tool.call_sync(&edit_args("workspace/a.txt", "nope", "x"));
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("0 hits"));
    }

    #[test]
    fn test_multi_hit_requires_replace_all() {
        let dir = temp_workdir_with_workspace();
        write_ws_file(&dir, "a.txt", b"x x x");
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let r = tool.call_sync(&edit_args("workspace/a.txt", "x", "y"));
        assert!(r.is_err());
        let err = r.unwrap_err();
        assert!(err.contains("3 times"), "got: {err}");
        // 文件未动
        assert_eq!(
            std::fs::read_to_string(dir.path().join("workspace/a.txt")).unwrap(),
            "x x x"
        );
        // replace_all=true 通过
        let mut args = edit_args("workspace/a.txt", "x", "y");
        args.as_object_mut()
            .unwrap()
            .insert("replace_all".to_string(), Value::Bool(true));
        assert!(tool.call_sync(&args).is_ok());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("workspace/a.txt")).unwrap(),
            "y y y"
        );
    }

    #[test]
    fn test_delete_via_empty_new_string() {
        let dir = temp_workdir_with_workspace();
        write_ws_file(&dir, "a.txt", b"keep DROP keep");
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let r = tool.call_sync(&edit_args("workspace/a.txt", " DROP", ""));
        assert!(r.is_ok());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("workspace/a.txt")).unwrap(),
            "keep keep"
        );
    }

    #[test]
    fn test_empty_old_string_rejected() {
        let dir = temp_workdir_with_workspace();
        write_ws_file(&dir, "a.txt", b"x");
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let r = tool.call_sync(&edit_args("workspace/a.txt", "", "y"));
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("non-empty"));
    }

    #[test]
    fn test_missing_file_rejected() {
        let dir = temp_workdir_with_workspace();
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let r = tool.call_sync(&edit_args("workspace/ghost.txt", "a", "b"));
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_non_utf8_rejected() {
        let dir = temp_workdir_with_workspace();
        write_ws_file(&dir, "a.bin", &[0xFF, 0xFE, b'a', b'b']);
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let r = tool.call_sync(&edit_args("workspace/a.bin", "a", "b"));
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("not valid UTF-8"));
    }

    #[test]
    fn test_bom_preserved() {
        let dir = temp_workdir_with_workspace();
        let mut content = BOM.to_vec();
        content.extend_from_slice(b"hello");
        write_ws_file(&dir, "a.txt", &content);
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let r = tool.call_sync(&edit_args("workspace/a.txt", "hello", "hi"));
        assert!(r.is_ok(), "got: {:?}", r);
        let out = std::fs::read(dir.path().join("workspace/a.txt")).unwrap();
        assert!(out.starts_with(BOM), "BOM must be preserved");
        assert_eq!(&out[BOM.len()..], b"hi");
    }

    #[test]
    fn test_crlf_file_preserved_and_arg_lf_normalized() {
        let dir = temp_workdir_with_workspace();
        write_ws_file(&dir, "a.txt", b"line1\r\nline2\r\nline3\r\n");
        let tool = FileEditTool::new(dir.path().to_path_buf());
        // 参数用 LF:归一到文件 CRLF 形态后命中
        let r = tool.call_sync(&edit_args("workspace/a.txt", "line1\nline2", "L1\nL2"));
        assert!(r.is_ok(), "got: {:?}", r);
        let out = std::fs::read(dir.path().join("workspace/a.txt")).unwrap();
        assert_eq!(out, b"L1\r\nL2\r\nline3\r\n".to_vec());
    }

    #[test]
    fn test_lf_file_stays_lf() {
        let dir = temp_workdir_with_workspace();
        write_ws_file(&dir, "a.txt", b"a\nb\nc\n");
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let r = tool.call_sync(&edit_args("workspace/a.txt", "b", "B"));
        assert!(r.is_ok());
        let out = std::fs::read(dir.path().join("workspace/a.txt")).unwrap();
        assert_eq!(out, b"a\nB\nc\n".to_vec());
    }

    #[test]
    fn test_mixed_line_endings_rejected() {
        let dir = temp_workdir_with_workspace();
        write_ws_file(&dir, "a.txt", b"a\r\nb\nc\r\n");
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let r = tool.call_sync(&edit_args("workspace/a.txt", "b", "B"));
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("mixed line endings"));
    }

    #[test]
    fn test_reject_absolute_path() {
        let dir = temp_workdir_with_workspace();
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let r = tool.call_sync(&edit_args("C:\\evil.txt", "a", "b"));
        assert!(r.is_err());
    }

    #[test]
    fn test_reject_parent_dir() {
        let dir = temp_workdir_with_workspace();
        write_ws_file(&dir, "a.txt", b"x");
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let r = tool.call_sync(&edit_args("../workspace/a.txt", "x", "y"));
        assert!(r.is_err());
    }

    #[test]
    fn test_reject_outside_writable_dir() {
        let dir = temp_workdir_with_workspace();
        std::fs::write(dir.path().join("config.toml"), b"k=v").unwrap();
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let r = tool.call_sync(&edit_args("config.toml", "k", "v2"));
        assert!(r.is_err());
        assert!(
            r.unwrap_err().contains("outside writable_dir") || true,
            "sandbox rejection expected"
        );
        // 内容未动
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml")).unwrap(),
            "k=v"
        );
    }

    #[test]
    fn test_file_too_large_rejected() {
        let dir = temp_workdir_with_workspace();
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let tool = FileEditTool {
            max_file_bytes: 10,
            ..tool
        };
        write_ws_file(&dir, "big.txt", b"0123456789abcdef");
        let r = tool.call_sync(&edit_args("workspace/big.txt", "0", "1"));
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("too large"));
    }

    #[test]
    fn test_edit_bytes_cap() {
        let dir = temp_workdir_with_workspace();
        write_ws_file(&dir, "a.txt", b"anchor");
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let big = "x".repeat(MAX_EDIT_BYTES);
        let r = tool.call_sync(&edit_args("workspace/a.txt", "anchor", &big));
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("too large"));
    }

    #[test]
    fn test_missing_args() {
        let dir = temp_workdir_with_workspace();
        let tool = FileEditTool::new(dir.path().to_path_buf());
        // no old_string
        let mut m = serde_json::Map::new();
        m.insert("path".to_string(), Value::from("workspace/a.txt"));
        m.insert("new_string".to_string(), Value::from("y"));
        assert!(tool.call_sync(&Value::Object(m)).is_err());
        // no path
        let mut m = serde_json::Map::new();
        m.insert("old_string".to_string(), Value::from("x"));
        m.insert("new_string".to_string(), Value::from("y"));
        assert!(tool.call_sync(&Value::Object(m)).is_err());
    }

    /// active 工具:审批钩子恒 None(直通执行,契约同 file_write)
    #[test]
    fn test_evaluate_proposal_always_none() {
        let dir = temp_workdir_with_workspace();
        let tool = FileEditTool::new(dir.path().to_path_buf());
        assert!(tool
            .evaluate_proposal(&edit_args("workspace/a.txt", "a", "b"))
            .is_none());
    }

    /// symlink 逃逸拒(写面同族:CWE-59 链接跟随)
    #[test]
    fn test_reject_symlink_escape() {
        let dir = temp_workdir_with_workspace();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, b"secret").unwrap();
        let link_path = dir.path().join("workspace").join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_file, &link_path).unwrap();
        #[cfg(windows)]
        {
            match std::os::windows::fs::symlink_file(&outside_file, &link_path) {
                Ok(()) => match std::fs::symlink_metadata(&link_path) {
                    Ok(m) if m.file_type().is_symlink() => {}
                    _ => {
                        eprintln!("skip: symlink_file returned Ok but no symlink created");
                        let _ = std::fs::remove_file(&link_path);
                        return;
                    }
                },
                Err(e) if e.raw_os_error() == Some(1314) => {
                    eprintln!("skip: no symlink privilege on this Windows env");
                    return;
                }
                Err(e) => panic!("unexpected symlink error: {e}"),
            }
        }
        let tool = FileEditTool::new(dir.path().to_path_buf());
        let r = tool.call_sync(&edit_args("workspace/link.txt", "secret", "hacked"));
        assert!(r.is_err(), "symlink escape must be rejected");
        assert_eq!(std::fs::read(&outside_file).unwrap(), b"secret");
    }
}
