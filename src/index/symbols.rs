// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 符号抽取（tree-sitter，PR-2）—— rust/ts 两门 grammar（与 08 号 LSP 同语言范围）
//!
//! 单文件一遍解析产出三面（15 号 D2/§3.1）：
//! - **def**：顶层与 impl/class 内符号（fn/struct/enum/trait/impl/method/
//!   class/interface/type）→ [`SymbolRow`]（入库 symbols 表）；
//! - **ref**：标识符出现计数（name 级，Aider repomap 口径）→ 跨库两遍编排时
//!   以「全局 def 名集合」过滤成 refs 边（weight=√次数，PR-3）；
//! - **chunk**：符号边界切分 + 富化头（文件路径/imports/父签名）→ 检索结果
//!   的上下文组装（D2；chunk 内容不落库，展示时按行区间回读源文件——D8）。
//!
//! 解析失败/无语法树 → 空抽取（该文件零符号零计数，索引尽力而为，不致命）。

use std::collections::HashMap;

use tree_sitter::{Node, Parser};

use crate::index::store::SymbolRow;

/// chunk 估算 token 上限（≈4 字符/token，与前端 estimateTokens 同口径）
pub const MAX_CHUNK_TOKENS: usize = 500;

/// 符号抽取语言（v1 两门：与 08 号 LSP 范围一致）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// Rust（.rs）
    Rust,
    /// TypeScript / TSX（.ts/.tsx/.mts/.cts）
    TypeScript,
}

/// 按扩展名路由语言（其余扩展名不建符号索引）
pub fn lang_for_path(rel_path: &str) -> Option<Lang> {
    let ext = rel_path.rsplit('.').next()?;
    match ext {
        "rs" => Some(Lang::Rust),
        "ts" | "tsx" | "mts" | "cts" => Some(Lang::TypeScript),
        _ => None,
    }
}

/// 带父上下文的符号条目（`parent` 供 chunk 富化头；入库取 `.row`）
#[derive(Debug, Clone, PartialEq)]
pub struct SymEntry {
    /// 库表行（kind/name/行区间）
    pub row: SymbolRow,
    /// 父符号签名（impl 类型 / 所属类名；顶层为 None）
    pub parent: Option<String>,
}

/// 单文件抽取产物
#[derive(Debug, Default, PartialEq)]
pub struct Extracted {
    /// def 符号（源码出现序）
    pub symbols: Vec<SymEntry>,
    /// 标识符出现计数（name 级；升序排序，PR-3 过滤成 refs 边）
    pub ident_counts: Vec<(String, u32)>,
    /// import/use 语句原文（chunk 富化头用；声明序，最多 10 条）
    pub imports: Vec<String>,
}

/// 解析并抽取（解析器不可用/无语法树 → 空抽取）
pub fn extract(lang: Lang, src: &str) -> Extracted {
    let Some(mut parser) = parser_for(lang) else {
        return Extracted::default();
    };
    let Some(tree) = parser.parse(src, None) else {
        return Extracted::default();
    };
    let mut out = Extracted::default();
    let mut counts: HashMap<String, u32> = HashMap::new();
    walk(tree.root_node(), src, lang, &mut out, &mut counts, None, 0);
    out.ident_counts = counts.into_iter().filter(|(_, n)| *n > 0).collect();
    out.ident_counts.sort();
    out
}

fn parser_for(lang: Lang) -> Option<Parser> {
    let mut p = Parser::new();
    let ok = match lang {
        Lang::Rust => p.set_language(&tree_sitter_rust::LANGUAGE.into()).is_ok(),
        Lang::TypeScript => p
            .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            .is_ok(),
    };
    ok.then_some(p)
}

/// def 节点判定（两语言的符号承载节点）
fn is_def(lang: Lang, kind: &str) -> bool {
    matches!(
        (lang, kind),
        (
            Lang::Rust,
            "function_item"
                | "function_signature_item"
                | "struct_item"
                | "enum_item"
                | "trait_item"
        ) | (
            Lang::TypeScript,
            "function_declaration"
                | "class_declaration"
                | "abstract_class_declaration"
                | "method_definition"
                | "interface_declaration"
                | "type_alias_declaration"
        )
    )
}

/// node kind → 库表 kind 口径
fn def_kind(kind: &str) -> &'static str {
    match kind {
        "function_item" | "function_declaration" => "fn",
        "function_signature_item" => "method", // rust trait 方法签名（parent=trait 名）
        "struct_item" => "struct",
        "enum_item" => "enum",
        "trait_item" => "trait",
        "class_declaration" | "abstract_class_declaration" => "class",
        "method_definition" => "method",
        "interface_declaration" => "interface",
        "type_alias_declaration" => "type",
        _ => "fn",
    }
}

/// 容器型 def 判定（下钻时把自身名传为子项父签名）
fn is_container(lang: Lang, kind: &str) -> bool {
    matches!(
        (lang, kind),
        (Lang::Rust, "trait_item")
            | (
                Lang::TypeScript,
                "class_declaration" | "abstract_class_declaration"
            )
    )
}

/// cursor 迭代遍历（递归深度护栏 256，防病态深树栈溢出）
fn walk(
    node: Node<'_>,
    src: &str,
    lang: Lang,
    out: &mut Extracted,
    counts: &mut HashMap<String, u32>,
    parent: Option<&str>,
    depth: usize,
) {
    if depth > 256 {
        return;
    }
    let kind = node.kind();

    // impl 块：本体记符号（类型名），块内 fn 升格为 method 并带父签名
    if lang == Lang::Rust && kind == "impl_item" {
        let type_name = field_text(node, "type", src);
        if let Some(t) = &type_name {
            let (s, e) = span(node);
            out.push_symbol("impl", t.clone(), s, e, None);
        }
        descend(node, src, lang, out, counts, type_name.as_deref(), depth);
        return;
    }

    // def 节点：记符号（带父）后继续下钻（mod/class 内子项逐个入账）
    if is_def(lang, kind) {
        let name = field_text(node, "name", src).unwrap_or_default();
        let container = is_container(lang, kind);
        if !name.is_empty() {
            let (s, e) = span(node);
            // rust impl 块内 fn / trait 方法签名语义上是 method（父签名已带上）
            let dkind = match (lang, kind, parent.is_some()) {
                (Lang::Rust, "function_item", true) => "method",
                (Lang::Rust, "function_signature_item", _) => "method",
                _ => def_kind(kind),
            };
            out.push_symbol(dkind, name.clone(), s, e, parent.map(str::to_owned));
        }
        // 容器型 def（rust trait / ts class）下钻时把自身名传为子项父签名
        let child_parent = if container && !name.is_empty() {
            Some(name.as_str())
        } else {
            parent
        };
        descend(node, src, lang, out, counts, child_parent, depth);
        return;
    }

    // import/use 语句原文（富化头；数量护栏 10）
    let import_kind = match lang {
        Lang::Rust => "use_declaration",
        Lang::TypeScript => "import_statement",
    };
    if kind == import_kind && out.imports.len() < 10 {
        let text = text_of(node, src);
        let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !one_line.is_empty() {
            out.imports.push(one_line.chars().take(80).collect());
        }
    }

    // 标识符计数（rust identifier；ts identifier+type_identifier）
    let countable = match lang {
        Lang::Rust => kind == "identifier",
        Lang::TypeScript => matches!(kind, "identifier" | "type_identifier"),
    };
    if countable {
        let name = text_of(node, src);
        if !name.is_empty() {
            *counts.entry(name).or_insert(0) += 1;
        }
    }

    descend(node, src, lang, out, counts, parent, depth);
}

fn descend(
    node: Node<'_>,
    src: &str,
    lang: Lang,
    out: &mut Extracted,
    counts: &mut HashMap<String, u32>,
    parent: Option<&str>,
    depth: usize,
) {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            walk(child, src, lang, out, counts, parent, depth + 1);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

impl Extracted {
    fn push_symbol(
        &mut self,
        kind: &str,
        name: String,
        start: usize,
        end: usize,
        parent: Option<String>,
    ) {
        self.symbols.push(SymEntry {
            row: SymbolRow {
                kind: kind.to_string(),
                name,
                line_start: start as u32 + 1, // 0-based → 1-based
                line_end: end as u32 + 1,
            },
            parent,
        });
    }
}

fn span(node: Node<'_>) -> (usize, usize) {
    (node.start_position().row, node.end_position().row)
}

fn text_of(node: Node<'_>, src: &str) -> String {
    node.utf8_text(src.as_bytes()).unwrap_or("").to_string()
}

/// 取字段文本（压平空白——类型名/方法名跨行换行时保持单行）
fn field_text(node: Node<'_>, field: &'static str, src: &str) -> Option<String> {
    let child = node.child_by_field_name(field)?;
    Some(
        text_of(child, src)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(""),
    )
}

// =============================================================================
// 富化 chunk（D2：符号边界切分 + 前置「文件路径/imports/父签名」富化）
// =============================================================================

/// 富化 chunk（不落库——展示时按行区间回读源文件，D8）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeChunk {
    /// 相对路径（`/` 分隔）
    pub file: String,
    /// 所属符号名（顶层文件级 chunk 为空串）
    pub symbol: String,
    /// 符号类别
    pub kind: String,
    /// 起始行（1 起，含）
    pub start_line: u32,
    /// 结束行（含）
    pub end_line: u32,
    /// 富化文本（头 + 源码行区间）
    pub text: String,
}

/// 组装富化 chunk：每个符号一个 chunk，超 [`MAX_CHUNK_TOKENS`] 按行均分为多段
///
/// `imports` 来自 [`extract`]；源码行按 1-based 行号区间回读自 `src`。
pub fn build_chunks(
    file: &str,
    src: &str,
    entries: &[SymEntry],
    imports: &[String],
) -> Vec<CodeChunk> {
    let lines: Vec<&str> = src.lines().collect();
    let mut chunks = Vec::new();
    for e in entries {
        let s = (e.row.line_start as usize).max(1);
        let end = (e.row.line_end as usize).min(lines.len()).max(s);
        let head = match &e.parent {
            Some(p) => {
                let mut h = format!("// file: {file}\n");
                if !imports.is_empty() {
                    h.push_str(&format!("// imports: {}\n", imports.join("; ")));
                }
                h.push_str(&format!("// {} {} (parent: {p})\n", e.row.kind, e.row.name));
                h
            }
            None => {
                let mut h = format!("// file: {file}\n");
                if !imports.is_empty() {
                    h.push_str(&format!("// imports: {}\n", imports.join("; ")));
                }
                h.push_str(&format!("// {} {}\n", e.row.kind, e.row.name));
                h
            }
        };
        let budget = MAX_CHUNK_TOKENS
            .saturating_mul(4)
            .saturating_sub(head.len() / 4);
        if budget == 0 {
            continue;
        }
        let body = &lines[s - 1..end];
        // 按字符预算切行段（行边界切分，段间不重叠不遗漏；空体保底一段）
        let mut segs: Vec<Vec<&str>> = Vec::new();
        let mut cur: Vec<&str> = Vec::new();
        let mut cur_len = 0usize;
        for line in body {
            let l = line.len() + 1;
            if !cur.is_empty() && cur_len + l > budget {
                segs.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            cur.push(line);
            cur_len += l;
        }
        if !cur.is_empty() {
            segs.push(cur);
        }
        if segs.is_empty() {
            segs.push(Vec::new());
        }
        for seg in &segs {
            let (start_line, end_line) = if seg.is_empty() {
                (s as u32, s as u32)
            } else {
                // 段首/段尾行回定位（行内容在区间内唯一性不足时按首/尾匹配）
                let first = seg[0];
                let off_head = body
                    .iter()
                    .position(|l| std::ptr::eq(*l, first))
                    .unwrap_or(0);
                let last = *seg.last().unwrap();
                let off_tail = body
                    .iter()
                    .rev()
                    .position(|l| std::ptr::eq(*l, last))
                    .unwrap_or(0);
                ((s + off_head) as u32, (end - off_tail) as u32)
            };
            chunks.push(CodeChunk {
                file: file.to_string(),
                symbol: e.row.name.clone(),
                kind: e.row.kind.clone(),
                start_line,
                end_line,
                text: format!("{head}{}", seg.join("\n")),
            });
        }
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_SAMPLE: &str = "\
use std::collections::HashMap;
use crate::agent::lexstore::LexStore;

pub struct Cache {
    map: HashMap<String, String>,
}

pub enum Mode {
    Fast,
    Safe,
}

pub trait Lookup {
    fn find(&self, key: &str) -> Option<String>;
}

impl Cache {
    pub fn new() -> Self {
        Self { map: HashMap::new() }
    }

    pub fn put(&mut self, key: &str, val: &str) {
        self.map.insert(key.to_string(), val.to_string());
    }
}

pub fn top_level(cache: &Cache) -> usize {
    cache.map.len()
}
";

    const TS_SAMPLE: &str = "\
import { readFile } from 'fs/promises';
import helper from './helper';

export class Loader {
    constructor(private root: string) {}

    async load(name: string): Promise<string> {
        return readFile(name, 'utf8');
    }
}

export interface Options {
    root: string;
    verbose: boolean;
}

export type Mode = 'fast' | 'safe';

export function boot(opts: Options): Loader {
    return new Loader(opts.root);
}
";

    #[test]
    fn rust_symbols_golden() {
        let ex = extract(Lang::Rust, RUST_SAMPLE);
        let sig: Vec<(String, String)> = ex
            .symbols
            .iter()
            .map(|e| (e.row.kind.clone(), e.row.name.clone()))
            .collect();
        assert_eq!(
            sig,
            vec![
                ("struct".into(), "Cache".into()),
                ("enum".into(), "Mode".into()),
                ("trait".into(), "Lookup".into()),
                ("method".into(), "find".into()),
                ("impl".into(), "Cache".into()),
                ("method".into(), "new".into()),
                ("method".into(), "put".into()),
                ("fn".into(), "top_level".into()),
            ]
        );
        // trait 方法签名 / impl 方法都带父签名
        let find = ex.symbols.iter().find(|e| e.row.name == "find").unwrap();
        assert_eq!(find.parent.as_deref(), Some("Lookup"));
        // method 带父签名（富化头用；入库行不含）
        let put = ex.symbols.iter().find(|e| e.row.name == "put").unwrap();
        assert_eq!(put.parent.as_deref(), Some("Cache"));
        assert_eq!(put.row.line_start, 22);
        // 标识符计数：def 名自身入账（跨文件 ref 由 PR-3 编排过滤成边）
        let n = ex.ident_counts.iter().find(|(k, _)| k == "put").unwrap().1;
        assert!(n >= 1);
        // imports 收集
        assert_eq!(ex.imports.len(), 2);
        assert!(ex.imports[0].starts_with("use std::collections::HashMap"));
    }

    #[test]
    fn ts_symbols_golden() {
        let ex = extract(Lang::TypeScript, TS_SAMPLE);
        let sig: Vec<(String, String)> = ex
            .symbols
            .iter()
            .map(|e| (e.row.kind.clone(), e.row.name.clone()))
            .collect();
        assert_eq!(
            sig,
            vec![
                ("class".into(), "Loader".into()),
                ("method".into(), "constructor".into()),
                ("method".into(), "load".into()),
                ("interface".into(), "Options".into()),
                ("type".into(), "Mode".into()),
                ("fn".into(), "boot".into()),
            ]
        );
        let load = ex.symbols.iter().find(|e| e.row.name == "load").unwrap();
        assert_eq!(load.parent.as_deref(), Some("Loader"));
        assert!(ex.ident_counts.iter().any(|(k, _)| k == "Loader"));
    }

    #[test]
    fn lang_routing_by_extension() {
        assert_eq!(lang_for_path("a/b.rs"), Some(Lang::Rust));
        assert_eq!(lang_for_path("x.tsx"), Some(Lang::TypeScript));
        assert_eq!(lang_for_path("x.mts"), Some(Lang::TypeScript));
        assert_eq!(lang_for_path("x.py"), None);
        assert_eq!(lang_for_path("noext"), None);
    }

    #[test]
    fn broken_source_does_not_panic() {
        let ex = extract(Lang::Rust, "}}} fn ( ( ( garbage");
        // 错误恢复下可能部分抽取——只断言不 panic 且无 imports
        assert!(ex.imports.is_empty());
    }

    #[test]
    fn oversized_symbol_splits_into_contiguous_segments() {
        let body: String = (0..300)
            .map(|i| format!("    let v{i} = {i}; // pad\n"))
            .collect();
        let src = format!("pub fn big() {{\n{body}}}\n");
        let ex = extract(Lang::Rust, &src);
        assert_eq!(ex.symbols.len(), 1);
        let chunks = build_chunks("big.rs", &src, &ex.symbols, &[]);
        assert!(chunks.len() > 1, "expected split, got {}", chunks.len());
        // 行区间连续无缝且覆盖整个符号
        for w in chunks.windows(2) {
            assert_eq!(w[0].end_line + 1, w[1].start_line);
        }
        assert_eq!(chunks[0].start_line, ex.symbols[0].row.line_start);
        assert_eq!(chunks.last().unwrap().end_line, ex.symbols[0].row.line_end);
    }

    #[test]
    fn chunk_header_carries_file_and_parent() {
        let src = "impl Cache {\n    pub fn put(&mut self) {}\n}\n";
        let ex = extract(Lang::Rust, src);
        let chunks = build_chunks("lib.rs", src, &ex.symbols, &["use a::b".to_string()]);
        let put = chunks.iter().find(|c| c.symbol == "put").unwrap();
        assert!(put.text.contains("// file: lib.rs"));
        assert!(put.text.contains("// imports: use a::b"));
        assert!(put.text.contains("(parent: Cache)"));
    }
}
