// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `code_search` —— 工作区符号图谱检索（B14 结构面 agent 工具化,批K 段3）
//!
//! ## 定位（与既有搜索工具分工,防重复建设）
//! - `search_files`:按文件名 glob 找文件;
//! - `grep_files`:按内容串/正则做**行级**全文匹配（已知串找出现处）;
//! - `code_search`:符号名**图谱检索**——tree-sitter 抽取的符号集上做子串
//!   匹配 + PageRank 排位（找「哪些文件定义/引用了该符号」,代码入口/影响面
//!   场景比行级 grep 噪声低得多）。
//! - evorule-server 侧另有 codegraph-bridge 插件（跨仓库、外部 Node
//!   sidecar）——本工具面向 evo-agent 当前工作区、零外部依赖,两者不重复。
//!
//! ## 范围裁剪（v1:只做 lexical 结构面）
//! B14 的 semantic/hybrid 模式**不进 agent 工具面**:语义面装配 fastembed
//! 本地嵌入模型（`index.semantic.enabled` 显式开启,部署面默认关）,本机无
//! GPU 场景内存吃紧属硬约束;hybrid 的词法侧即 grep_core 文件聚合,与
//! `grep_files` 重复。工具恒走符号图谱（纯 CPU、零模型依赖、零外发）。
//!
//! ## 接线双态（audit_query_tool 同款占位/重绑惯例）
//! - 启动期 `default_safe_toolkit` 注册占位（`unwired()`）:调用 fail-visible
//!   如实报「索引服务未启用」,不静默返回空结果;
//! - serve 启动 `index.enabled` 开启并建好 [`IndexService`] 后,经
//!   `AgentApiState.toolkit` 写锁重绑 `wired()`（B16 MCP 运行期重绑同款手法）;
//!   CLI 面与关闭态恒占位。开关 agentTools.codeSearch 过滤三面自动工作
//!   （启动期注册+开关过滤后在场才重绑——开关关=占位未进面,重绑跳过）。
//!
//! ## 输出契约
//! 与 REST `GET /api/index/search?mode=lexical` 同源数据、零方言漂移:
//! `results: [{file, kind, name, line_start, line_end, score}]`（score 为
//! PageRank 融合排位分,降序）;`semantic: "structure-only"` 恒标注,消费者
//! 可据其区分本工具与语义面输出。

use std::sync::Arc;

use serde_json::{json, Value};

use crate::index::IndexService;
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::ToolFunction;

/// 结果数缺省值（与 REST search 端点同域）
pub const DEFAULT_LIMIT: usize = 20;

/// 结果数硬上限（与 REST search 端点同域;防误传巨值拖长 PageRank 排位）
pub const MAX_LIMIT: usize = 100;

/// `code_search` 执行器（占位/接线双态）
pub struct CodeSearchTool {
    index: Option<Arc<IndexService>>,
}

impl CodeSearchTool {
    /// 占位构造（启动期 default_safe_toolkit 注册;调用 fail-visible 报错）
    pub fn unwired() -> Self {
        Self { index: None }
    }

    /// 接线构造（serve 启动期 index.enabled 建好服务后重绑）
    pub fn wired(index: Arc<IndexService>) -> Self {
        Self { index: Some(index) }
    }

    fn call_sync(&self, args: &Value) -> Result<Value, String> {
        let Some(svc) = &self.index else {
            return Err(
                "code_search: index service not enabled (set index.enabled=true in \
                 workbench settings and restart serve; or POST /api/index/rebuild to \
                 build the index first)"
                    .to_string(),
            );
        };
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required arg: query (string)".to_string())?
            .trim();
        if query.is_empty() {
            return Err("query must not be empty".to_string());
        }
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n.max(1) as usize).min(MAX_LIMIT))
            .unwrap_or(DEFAULT_LIMIT);
        // PageRank 排位为 CPU 密集:call 已在 spawn_blocking（G13 惯例）,此处直调
        let graph = svc.graph();
        if graph.is_empty() {
            return Err(
                "code_search: symbol graph not ready yet (index empty or still \
                 building — check GET /api/index/status; first build via POST \
                 /api/index/rebuild)"
                    .to_string(),
            );
        }
        let hits = graph.match_symbols(query, limit);
        Ok(json!({
            "query": query,
            "mode": "lexical",
            "semantic": "structure-only",
            "resultCount": hits.len(),
            "results": hits
                .iter()
                .map(|(s, score)| json!({
                    "file": s.file,
                    "kind": s.kind,
                    "name": s.name,
                    "line_start": s.line_start,
                    "line_end": s.line_end,
                    "score": score,
                }))
                .collect::<Vec<_>>(),
        }))
    }
}

#[async_trait::async_trait]
impl ToolFunction for CodeSearchTool {
    /// G13:async 入口 — 用 spawn_blocking 包装同步图谱检索（PageRank CPU 密集）
    async fn call(&self, args: &Value) -> IoResult {
        // IndexService 无 &self 可变态且 graph() 快照廉价克隆;Arc 双份跨线程
        // 安全（Service 全 Sync）。self 借用跨 await 不行——克隆 Arc 进闭包。
        let tool = CodeSearchTool {
            index: self.index.clone(),
        };
        let args = args.clone();
        tokio::task::spawn_blocking(move || tool.call_sync(&args))
            .await
            .map_err(|e| format!("code_search tool panicked: {}", e))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wired_with_files(root: &std::path::Path) -> Arc<IndexService> {
        std::fs::write(root.join("a.rs"), "fn alpha_lookup() { beta(); }").unwrap();
        std::fs::write(root.join("b.rs"), "fn beta() {}").unwrap();
        let svc = IndexService::open(root).unwrap();
        match svc.rebuild_sync() {
            crate::index::RebuildOutcome::Completed(_) => {}
            other => panic!("expected completed, got {other:?}"),
        }
        Arc::new(svc)
    }

    #[test]
    fn unwired_fails_visible_with_guidance() {
        let err = CodeSearchTool::unwired()
            .call_sync(&json!({ "query": "alpha" }))
            .unwrap_err();
        assert!(err.contains("index service not enabled"), "got: {err}");
        assert!(err.contains("index.enabled"), "got: {err}");
    }

    #[test]
    fn wired_searches_symbols_with_scores() {
        let dir = tempfile::tempdir().unwrap();
        let svc = wired_with_files(dir.path());
        let out = CodeSearchTool::wired(svc)
            .call_sync(&json!({ "query": "alpha" }))
            .unwrap();
        assert_eq!(out["mode"], "lexical");
        assert_eq!(out["semantic"], "structure-only");
        assert_eq!(out["resultCount"], 1);
        let hit = &out["results"][0];
        assert_eq!(hit["file"], "a.rs");
        assert_eq!(hit["name"], "alpha_lookup");
        assert_eq!(hit["line_start"], 1);
    }

    #[test]
    fn empty_query_and_missing_query_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let svc = wired_with_files(dir.path());
        let tool = CodeSearchTool::wired(svc);
        let err = tool.call_sync(&json!({ "query": "   " })).unwrap_err();
        assert!(err.contains("must not be empty"), "got: {err}");
        let err = tool.call_sync(&json!({})).unwrap_err();
        assert!(err.contains("missing required arg: query"), "got: {err}");
    }

    #[test]
    fn limit_clamped_to_max() {
        let dir = tempfile::tempdir().unwrap();
        let svc = wired_with_files(dir.path());
        // 巨值封顶不 panic（MAX_LIMIT=100;小工作区结果本就少,封顶语义不炸即可）
        let out = CodeSearchTool::wired(svc)
            .call_sync(&json!({ "query": "a", "limit": 1_000_000 }))
            .unwrap();
        assert!(out["resultCount"].as_u64().unwrap() <= 100);
    }

    #[test]
    fn empty_graph_fails_visible_with_rebuild_guidance() {
        let dir = tempfile::tempdir().unwrap();
        // 未重建（open 后 graph 恒空快照）→ fail-visible 引导 rebuild
        let svc = Arc::new(IndexService::open(dir.path()).unwrap());
        let err = CodeSearchTool::wired(svc)
            .call_sync(&json!({ "query": "alpha" }))
            .unwrap_err();
        assert!(err.contains("not ready"), "got: {err}");
        assert!(err.contains("rebuild"), "got: {err}");
    }

    #[tokio::test]
    async fn async_call_wraps_blocking_search() {
        let dir = tempfile::tempdir().unwrap();
        let svc = wired_with_files(dir.path());
        let out = CodeSearchTool::wired(svc)
            .call(&json!({ "query": "beta" }))
            .await
            .unwrap();
        assert_eq!(out["results"][0]["file"], "b.rs");
    }
}
