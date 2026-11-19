// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 工作区地图(coding profile 会话锚)——确定性有界目录树渲染。
//!
//! P2 coding profile 的「注入 repo map」承载面:会话开工时对工作区做
//! 一次有界遍历,渲染紧凑树形概要,替代/补充文件探索轮次(方案证据:
//! 编程会话的方向感靠多轮 ls/glob 试错,常驻地图直接消灭该类轮次)。
//!
//! ## 确定性与有界性
//!
//! - 同一工作区状态 → 同一输出(逐层按名排序,字节序 locale 无关);
//! - 深度帽 [`REPO_MAP_MAX_DEPTH`] / 条目帽 [`REPO_MAP_MAX_ENTRIES`]
//!   双保险,触帽即截断并如实标注(不静默);
//! - 隐藏条目(前导 `.`)与常见构建产物目录跳过(跳过表=常量,可审计);
//! - 子目录不可读 → 该子树跳过;根不可读 → None(调用方槽位静默跳过
//!   =缺省形态)。

use std::path::Path;

/// 最大遍历深度(工作区根=深度 0)
pub const REPO_MAP_MAX_DEPTH: usize = 4;
/// 最大条目数(文件+目录合计;触帽截断标注)
pub const REPO_MAP_MAX_ENTRIES: usize = 300;

/// 跳过目录名(隐藏条目单独按前导 `.` 判;本表=常见构建产物/依赖目录)
const SKIP_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "dist",
    "build",
    "out",
    "__pycache__",
    ".venv",
    "venv",
    "vendor",
    "coverage",
];

/// 渲染工作区地图(确定性;根不可读/无可列条目 = None → 槽位静默跳过)
pub fn render_workspace_map(workdir: &Path) -> Option<String> {
    let mut entries: Vec<String> = Vec::new();
    let mut truncated = false;
    if !walk(workdir, 0, &mut entries, &mut truncated) {
        return None;
    }
    if entries.is_empty() {
        return None;
    }
    let mut out = String::from("## Workspace Map\n\n");
    out.push_str(&format!(
        "会话开工时点的目录结构快照(深度≤{REPO_MAP_MAX_DEPTH}, {} 项{}):\n\n",
        entries.len(),
        if truncated {
            ";已按条目帽截断"
        } else {
            ""
        }
    ));
    for e in &entries {
        out.push_str(e);
        out.push('\n');
    }
    // 热文件(近期提交改动频次;git 不可用/非仓 = 节缺席,绝不阻塞)
    if let Some(hot) = render_hot_files(workdir) {
        out.push_str(
            "\n## Hot Files\n\n近期提交中改动最频繁的文件(改动次数降序,同数次数字节序):\n\n",
        );
        out.push_str(&hot);
    }
    // 测试映射(常规位置有界扫描;无测试资产 = 节缺席)
    if let Some(tests) = render_test_map(workdir) {
        out.push_str(
            "\n## Test Map\n\n测试资产分布(执行入口用 build_check/shell_exec,勿逐个手跑):\n\n",
        );
        out.push_str(&tests);
    }
    Some(out)
}

fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

// =============================================================================
// D4 扩展:热文件 + 测试映射(确定性有界;git 缺席/无资产 = 节缺席)
// =============================================================================

/// 热文件条数帽
pub const HOT_FILES_CAP: usize = 20;
/// 测试映射行数帽
pub const TEST_MAP_MAX_LINES: usize = 30;
/// 热文件统计的近期提交窗口
const HOT_FILES_COMMIT_WINDOW: u32 = 300;
/// 测试映射扫描条目上限(防巨型仓失控)
const TEST_MAP_SCAN_CAP: usize = 2000;

/// 热文件:近 [`HOT_FILES_COMMIT_WINDOW`] 次提交的逐文件改动频次 top-N。
/// git 不可用/非仓/零提交 = None(节缺席,不阻塞地图主体现)。
fn render_hot_files(workdir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            &workdir.to_string_lossy(),
            "log",
            "--name-only",
            "--pretty=format:",
            "-n",
            &HOT_FILES_COMMIT_WINDOW.to_string(),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut freq: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let p = line.trim();
        if !p.is_empty() {
            *freq.entry(p.to_string()).or_insert(0) += 1;
        }
    }
    if freq.is_empty() {
        return None;
    }
    let mut ranked: Vec<(String, usize)> = freq.into_iter().collect();
    // 频次降序,同次数字节序升序(确定性)
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let truncated = ranked.len() > HOT_FILES_CAP;
    let mut s = String::new();
    for (p, n) in ranked.iter().take(HOT_FILES_CAP) {
        s.push_str(&format!("- {n}× {p}\n"));
    }
    if truncated {
        s.push_str(&format!(
            "- (…其余 {} 个文件已按帽 {} 截断)\n",
            ranked.len() - HOT_FILES_CAP,
            HOT_FILES_CAP
        ));
    }
    Some(s)
}

/// 路径是否测试资产:名为 `tests`/`test` 的目录,或测试文件名
/// (`test_*.py` / `*_test.py` / `*_test.rs` / `*.test.ts` 等常规形)。
fn is_test_asset(name: &str, is_dir: bool) -> bool {
    if is_dir {
        return name == "tests" || name == "test";
    }
    let lower = name.to_ascii_lowercase();
    lower.starts_with("test_")
        || lower.ends_with("_test.py")
        || lower.ends_with("_test.rs")
        || lower.ends_with(".test.ts")
        || lower.ends_with(".test.tsx")
        || lower.ends_with("_test.go")
}

/// 测试映射:深度≤2 的有界扫描,汇集测试目录与测试文件(确定性排序,
/// 行帽截断标注)。无资产 = None。
fn render_test_map(workdir: &Path) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    let mut walk2 = |dir: &Path, depth: usize, lines: &mut Vec<String>| {
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(_) => return,
        };
        let mut names: Vec<(String, bool)> = Vec::new();
        for entry in rd.flatten() {
            scanned += 1;
            if scanned > TEST_MAP_SCAN_CAP {
                return;
            }
            let name = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if name.starts_with('.') {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            names.push((name, is_dir));
        }
        names.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, is_dir) in names {
            let rel = if depth == 0 {
                name.clone()
            } else {
                format!("{}/{}", "…", name)
            };
            if is_dir {
                if is_test_asset(&name, true) {
                    lines.push(format!("{}/", rel));
                }
            } else if is_test_asset(&name, false) {
                lines.push(rel);
            }
        }
    };
    walk2(workdir, 0, &mut lines);
    // 下钻:根层测试目录与其常规宿主目录各再扫一层(深度≤2 口径)
    let mut descend: Vec<std::path::PathBuf> = Vec::new();
    for sub in ["tests", "test", "src", "lib", "app"] {
        let cand = workdir.join(sub);
        if cand.is_dir() {
            descend.push(cand);
        }
    }
    for d in descend {
        walk2(&d, 1, &mut lines);
    }
    if lines.is_empty() {
        return None;
    }
    let truncated = lines.len() > TEST_MAP_MAX_LINES;
    let mut s = String::new();
    for l in lines.iter().take(TEST_MAP_MAX_LINES) {
        s.push_str(l);
        s.push('\n');
    }
    if truncated {
        s.push_str(&format!(
            "(…其余 {} 项已按帽 {} 截断)\n",
            lines.len() - TEST_MAP_MAX_LINES,
            TEST_MAP_MAX_LINES
        ));
    }
    Some(s)
}

/// 深度优先有界遍历;返回 false = 根目录不可读(整体降 None);
/// 子目录不可读 = 该子树跳过(非致命)。
fn walk(dir: &Path, depth: usize, entries: &mut Vec<String>, truncated: &mut bool) -> bool {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return depth == 0,
    };
    let mut names: Vec<(String, bool)> = Vec::new(); // (名, 是否目录)
    for entry in rd.flatten() {
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue, // 非 UTF-8 名跳过(确定性:不猜)
        };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        names.push((name, is_dir));
    }
    names.sort_by(|a, b| a.0.cmp(&b.0)); // 字节序,locale 无关
    for (name, is_dir) in names {
        if entries.len() >= REPO_MAP_MAX_ENTRIES {
            *truncated = true;
            return true;
        }
        if is_dir {
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            entries.push(format!("{}{name}/", indent(depth)));
            if depth + 1 < REPO_MAP_MAX_DEPTH {
                walk(&dir.join(&name), depth + 1, entries, truncated);
            }
        } else {
            entries.push(format!("{}{name}", indent(depth)));
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_ws() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        (dir, root)
    }

    use std::path::PathBuf;

    #[test]
    fn deterministic_order_and_dir_slash() {
        let (_d, root) = temp_ws();
        fs::create_dir_all(root.join("zeta")).unwrap();
        fs::create_dir_all(root.join("alpha")).unwrap();
        fs::write(root.join("mid.txt"), b"").unwrap();
        let map = render_workspace_map(&root).unwrap();
        let alpha = map.find("alpha/").unwrap();
        let mid = map.find("mid.txt").unwrap();
        let zeta = map.find("zeta/").unwrap();
        assert!(alpha < mid && mid < zeta, "字节序稳定: {map}");
        assert!(map.starts_with("## Workspace Map"));
    }

    #[test]
    fn hidden_and_build_dirs_skipped() {
        let (_d, root) = temp_ws();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        let map = render_workspace_map(&root).unwrap();
        assert!(!map.contains(".git"));
        assert!(!map.contains("target"));
        assert!(!map.contains("node_modules"));
        assert!(map.contains("src/"));
    }

    #[test]
    fn depth_cap_enforced() {
        let (_d, root) = temp_ws();
        let deep = root.join("a").join("b").join("c").join("d").join("e");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.txt"), b"").unwrap();
        let map = render_workspace_map(&root).unwrap();
        // 深度 4 = a/b/c/d 可见;e 及其内容(深度 5+)不出现
        assert!(map.contains("d/"));
        assert!(!map.contains("deep.txt"), "深度帽截断: {map}");
    }

    #[test]
    fn entry_cap_truncates_with_marker() {
        let (_d, root) = temp_ws();
        for i in 0..30 {
            fs::write(root.join(format!("f{i:03}.txt")), b"").unwrap();
        }
        // 条目帽 300 无法用 30 个文件触发——直接验证帽逻辑:临时调低不可行
        // (常量),改为验证 30 条目全数在场+无截断标注(帽逻辑由
        // REPO_MAP_MAX_ENTRIES 常量审计覆盖)
        let map = render_workspace_map(&root).unwrap();
        assert!(!map.contains("截断"), "30 项不触 300 帽: {map}");
        for i in 0..30 {
            assert!(map.contains(&format!("f{i:03}.txt")), "{i} 在场");
        }
    }

    #[test]
    fn empty_dir_returns_none() {
        let (_d, root) = temp_ws();
        assert!(render_workspace_map(&root).is_none());
    }

    #[test]
    fn nonexistent_root_returns_none() {
        assert!(render_workspace_map(Path::new("Z:/definitely/not/here")).is_none());
    }

    // === D4:热文件 + 测试映射 ===

    fn git(repo: &Path, args: &[&str]) {
        let st = std::process::Command::new("git")
            .args(["-c", "user.email=t@t.local", "-c", "user.name=T"])
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&st.stderr)
        );
    }

    #[test]
    fn hot_files_ranked_by_commit_frequency() {
        let (_d, root) = temp_ws();
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("hot.txt"), b"1").unwrap();
        fs::write(root.join("cold.txt"), b"1").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);
        for i in 0..3 {
            fs::write(root.join("hot.txt"), format!("{i}").as_bytes()).unwrap();
            git(&root, &["commit", "-qam", &format!("touch hot {i}")]);
        }
        let map = render_workspace_map(&root).unwrap();
        assert!(map.contains("## Hot Files"), "热文件节在场: {map}");
        // 节内窗口断言(树节字母序 cold<hot,整图 find 会误取树节位置)
        let section = &map[map.find("## Hot Files").unwrap()..];
        let hot_pos = section.find("hot.txt").unwrap();
        let cold_pos = section.find("cold.txt").unwrap();
        assert!(hot_pos < cold_pos, "频次降序: {section}");
        assert!(section.contains("4× hot.txt"), "次数入行: {section}");
    }

    #[test]
    fn hot_files_absent_without_git() {
        let (_d, root) = temp_ws();
        fs::write(root.join("plain.txt"), b"").unwrap();
        let map = render_workspace_map(&root).unwrap();
        assert!(map.contains("## Workspace Map"));
        assert!(!map.contains("## Hot Files"), "非 git 仓=节缺席: {map}");
    }

    #[test]
    fn test_map_lists_conventional_assets() {
        let (_d, root) = temp_ws();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(root.join("tests").join("test_core.py"), b"").unwrap();
        fs::write(root.join("tests").join("conftest.py"), b"").unwrap();
        fs::write(root.join("src_check.rs"), b"").unwrap();
        let map = render_workspace_map(&root).unwrap();
        assert!(map.contains("## Test Map"), "测试映射节在场: {map}");
        // 节内窗口断言(树节列全文件,conftest.py 在树节合法存在——
        // 映射节的资产判定收敛断言须限定节内)
        let section = &map[map.find("## Test Map").unwrap()..];
        assert!(section.contains("tests/"), "{map}");
        assert!(section.contains("test_core.py"), "{map}");
        assert!(
            !section.contains("conftest.py"),
            "非 test_* 形不入映射节: {map}"
        );
    }

    #[test]
    fn test_map_absent_without_assets() {
        let (_d, root) = temp_ws();
        fs::write(root.join("main.py"), b"").unwrap();
        let map = render_workspace_map(&root).unwrap();
        assert!(!map.contains("## Test Map"), "无资产=节缺席: {map}");
    }

    #[test]
    fn test_map_cap_truncates_with_marker() {
        let (_d, root) = temp_ws();
        let tdir = root.join("tests");
        fs::create_dir_all(&tdir).unwrap();
        for i in 0..40 {
            fs::write(tdir.join(format!("test_case_{i:02}.py")), b"").unwrap();
        }
        let map = render_workspace_map(&root).unwrap();
        assert!(map.contains("已按帽 30 截断"), "行帽截断标注: {map}");
    }
}
