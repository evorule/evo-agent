// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Tool I/O Handler -- invokes registered tool functions
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tracing::debug;

use crate::agent::tool_manifest::ToolManifest;
use crate::io_handler::{IoHandler, IoResult};

/// G13:工具函数 trait(异步)
///
/// `call` 是 `async fn`,支持网络请求/IO 操作/子进程等异步操作。
/// 同步工具(如 `std::fs`)在实现中用 `tokio::task::spawn_blocking` 包装。
///
/// **破坏性变更**(G13):从 `fn call(&self, args) -> IoResult` 改为 `async fn call`。
/// 旧代码需给 `impl ToolFunction` 加 `#[async_trait::async_trait]` 并把 `fn call` 改 `async fn call`。
#[async_trait::async_trait]
pub trait ToolFunction: Send + Sync {
    /// 执行工具(异步)
    ///
    /// # 参数
    /// - `args`:工具参数(JSON)
    ///
    /// # 返回
    /// - `Ok(Value)`:工具执行结果
    /// - `Err(String)`:错误描述
    async fn call(&self, args: &Value) -> IoResult;
}

const TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// 注册条目：治理元数据(manifest) + 执行器(func)成对在场
///
/// 工具面统一架构(一表)硬规则 1:注册必须携带 manifest——只有执行器没有
/// 治理元数据的注册被签名拒绝(编译期),按名查不到静态条目且非动态源的
/// 注册由调用方启动期 fail-fast。
#[derive(Clone)]
pub struct ToolEntry {
    /// 工具治理元数据(单一真相源)
    pub manifest: ToolManifest,
    /// 执行器
    pub func: Arc<dyn ToolFunction>,
}

impl std::fmt::Debug for ToolEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolEntry")
            .field("manifest", &self.manifest.name)
            .field("source", &self.manifest.source)
            .finish()
    }
}

/// Tool I/O Handler -- invokes registered tool functions
#[derive(Clone)]
pub struct ToolHandler {
    tools: Arc<BTreeMap<String, ToolEntry>>,
}

impl std::fmt::Debug for ToolHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolHandler")
            .field("tool_count", &self.tools.len())
            .finish()
    }
}

impl ToolHandler {
    /// Create new tool handler
    pub fn new() -> Self {
        Self {
            tools: Arc::new(BTreeMap::new()),
        }
    }

    /// Create handler with existing tools
    pub fn with_tools(tools: BTreeMap<String, ToolEntry>) -> Self {
        Self {
            tools: Arc::new(tools),
        }
    }

    /// Create handler from bare tool functions（测试/外部装配便捷口）
    ///
    /// 每个函数按 Mcp 动态源补全 manifest（硬规则 3：动态源注册期产出
    /// Inline spec），治理元数据与执行器成对在场。
    pub fn with_functions(funcs: BTreeMap<String, Arc<dyn ToolFunction>>) -> Self {
        let tools: BTreeMap<String, ToolEntry> = funcs
            .into_iter()
            .map(|(name, func)| {
                let manifest = crate::agent::tool_manifest::dynamic_manifest(
                    &name,
                    crate::agent::tool_manifest::ToolSource::Mcp,
                    format!("tool {name}"),
                    serde_json::json!({"type": "object", "properties": {}}),
                )
                .unwrap_or_else(|e| panic!("with_functions: {e}"));
                (name, ToolEntry { manifest, func })
            })
            .collect();
        Self {
            tools: Arc::new(tools),
        }
    }

    /// 注册工具（manifest 硬规则 1：治理元数据与执行器成对在场）
    ///
    /// 同名重复注册 = 后者覆盖（装配期重绑场景：能力边界重建 file 实例、
    /// skills 清单刷新 read_skill——语义与旧 register_tool 一致）。
    pub fn register(&mut self, manifest: ToolManifest, func: Arc<dyn ToolFunction>) {
        let name = manifest.name.clone();
        Arc::make_mut(&mut self.tools).insert(name, ToolEntry { manifest, func });
    }

    /// 注册静态工具（按名查静态 manifest 表，硬规则 1 启动期 fail-fast）
    ///
    /// 查不到静态条目 = panic（启动期装配失败可见，禁止无契约工具上线）。
    pub fn register_static(&mut self, name: &str, func: Arc<dyn ToolFunction>) {
        let manifest = crate::agent::tool_manifest::lookup_static(name).unwrap_or_else(|| {
            panic!(
                "tool '{name}' has no static manifest — registration refused \
                     (manifest hard rule 1: fail-fast)"
            )
        });
        self.register(manifest, func);
    }

    /// 从另一 handler 整体复制注册条目（装配过滤/重绑共用）
    ///
    /// 返回是否注册成功（源无此工具 = false，静默跳过语义由调用方决定）。
    pub fn register_entry_from(&mut self, source: &ToolHandler, name: &str) -> bool {
        match source.tools.get(name) {
            Some(entry) => {
                self.register(entry.manifest.clone(), entry.func.clone());
                true
            }
            None => false,
        }
    }

    /// Check whether tool is registered
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// 已注册工具名列表(按字母序)
    ///
    /// 供 runner 组装 LLM 请求的 tools schema 时枚举执行器
    /// (schema 数据源 = manifest spec 视图 ∩ 本列表)。
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// 按名取出工具实现(供 serve 按白名单过滤复用)
    pub fn get_tool(&self, name: &str) -> Option<Arc<dyn ToolFunction>> {
        self.tools.get(name).map(|e| e.func.clone())
    }

    /// 按名查治理元数据（一表派生面：裁决分级/审批/开关/能力域）
    pub fn manifest(&self, name: &str) -> Option<ToolManifest> {
        self.tools.get(name).map(|e| e.manifest.clone())
    }

    /// 全部条目治理元数据（按名字序）
    pub fn manifests(&self) -> Vec<ToolManifest> {
        self.tools.values().map(|e| e.manifest.clone()).collect()
    }

    /// G13:直接按名称执行工具(不经 IoHandler::execute 的 params 解包)
    ///
    /// 供 runner 的并行执行路径(`execute_single_tool`)直接调用,
    /// 跳过 `params.get("tool_name")` 解包步骤。
    /// 包含 60s 超时(同 `IoHandler::execute`)。
    pub async fn execute_by_name(&self, tool_name: &str, args: &Value) -> IoResult {
        let func = self
            .tools
            .get(tool_name)
            .ok_or_else(|| format!("tool not found: {tool_name}"))?
            .func
            .clone();

        debug!(tool_name = tool_name, "ready to invoke tool (async)");

        tokio::time::timeout(TOOL_TIMEOUT, func.call(args))
            .await
            .map_err(|_| {
                format!(
                    "tool '{tool_name}' timed out after {}s. Hint: for reading a specific file use 'file_read'; \
                     for listing a directory use 'file_list'; full-tree search ('search_files' / 'grep_files') \
                     may be slow on large workspaces — pass 'dir' to scope it or 'exclude' to skip big directories \
                     (default excludes: target, node_modules, .git, .evo-trash, data)",
                    TOOL_TIMEOUT.as_secs()
                )
            })?
    }
}

impl Default for ToolHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl IoHandler for ToolHandler {
    /// Execute tool invocation
    async fn execute(&self, params: &Value) -> IoResult {
        let tool_name = params
            .get("tool_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required param: tool_name".to_string())?;

        let args = params.get("args").cloned().unwrap_or(Value::Null);

        self.execute_by_name(tool_name, &args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool_manifest::{dynamic_manifest, lookup_static, ToolSource};

    struct EchoTool;

    #[async_trait::async_trait]
    impl ToolFunction for EchoTool {
        async fn call(&self, _args: &Value) -> IoResult {
            Ok(Value::from("result"))
        }
    }

    fn register_static(handler: &mut ToolHandler, name: &str) {
        let manifest =
            lookup_static(name).unwrap_or_else(|| panic!("static manifest missing: {name}"));
        handler.register(manifest, Arc::new(EchoTool));
    }

    #[test]
    fn test_tool_handler_new() {
        let handler = ToolHandler::new();
        assert!(!handler.has_tool("test"));
    }

    #[test]
    fn test_tool_handler_register_and_has() {
        let mut handler = ToolHandler::new();
        let manifest = lookup_static("file_read").unwrap();
        handler.register(manifest, Arc::new(EchoTool));
        assert!(handler.has_tool("file_read"));
        assert!(!handler.has_tool("other"));
    }

    #[test]
    fn test_tool_handler_manifest_accessor() {
        let mut handler = ToolHandler::new();
        register_static(&mut handler, "file_create");
        let m = handler.manifest("file_create").unwrap();
        assert_eq!(m.name, "file_create");
        assert!(m.is_p2_adjudicated());
        assert!(handler.manifest("nope").is_none());
    }

    #[test]
    fn test_tool_handler_register_entry_from() {
        let mut union = ToolHandler::new();
        register_static(&mut union, "file_read");

        let mut filtered = ToolHandler::new();
        assert!(filtered.register_entry_from(&union, "file_read"));
        assert!(filtered.has_tool("file_read"));
        // 源无此工具 = false
        assert!(!filtered.register_entry_from(&union, "nope"));
    }

    #[tokio::test]
    async fn test_tool_handler_execute_by_name() {
        let mut handler = ToolHandler::new();
        register_static(&mut handler, "file_read");
        let result = handler.execute_by_name("file_read", &Value::Null).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().to_string(), "\"result\"");
    }

    #[tokio::test]
    async fn test_tool_handler_execute_not_found() {
        let handler = ToolHandler::new();
        let result = handler.execute_by_name("nonexistent", &Value::Null).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("tool not found"));
    }

    #[tokio::test]
    async fn test_tool_handler_execute_via_io_handler() {
        // 通过 IoHandler::execute 路径(params 含 tool_name + args)
        let mut handler = ToolHandler::new();
        register_static(&mut handler, "file_read");
        let params = Value::Object(
            std::iter::once(("tool_name".to_string(), Value::from("file_read"))).collect(),
        );
        let result = handler.execute(&params).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_dynamic_manifest_into_handler() {
        // 动态源（服务代理/MCP）注册路径：Inline 契约成对注册
        let mut handler = ToolHandler::new();
        let manifest = dynamic_manifest(
            "svc_demo",
            ToolSource::ServiceProxy,
            "demo service tool".to_string(),
            serde_json::json!({"type": "object", "properties": {}}),
        )
        .unwrap();
        handler.register(manifest, Arc::new(EchoTool));
        assert!(handler.has_tool("svc_demo"));
        let m = handler.manifest("svc_demo").unwrap();
        assert!(matches!(
            m.spec,
            crate::agent::tool_manifest::SpecSource::Inline { .. }
        ));
    }
}
