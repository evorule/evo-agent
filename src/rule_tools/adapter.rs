// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 通用 OpenAPI→ToolFunction 适配器（rule_tools 透传工具的表驱动底座）
//!
//! 「取参 → 单次 client 调用 → 响应原样返回」的薄包装工具统一为表驱动：
//! [`EndpointBinding`] 声明一个工具的端点映射（client / method / path 模板 /
//! 参数绑定），[`PassthroughTool`] 泛型执行，[`specs_from`] 生成静态 spec，
//! [`register_bindings`] 注册执行器。
//!
//! 端点元数据权威来源 = evorule-server `/api/openapi.json`（utoipa 聚合面，
//! 运行时 merge workspace spec，聚合面对全部治理端点完整覆盖无缺口）。
//! 映射表按设计稿固化为本模块代码常量（当前无第二消费方，暂不外置配置文件）。
//!
//! 等价纪律（与原手写薄包装逐项对齐）：
//! - spec 文本（工具/参数 description）逐字沿用原手写 spec——LLM 契约零漂移；
//! - required 缺失错误文本 = `missing required parameter: {name}`；
//! - required 参数类型不符视同缺失（对齐原 `and_then(as_str)` 链路语义）；
//! - optional 参数类型不符时忽略（同上）；
//! - POST/PATCH 一律携带 JSON body（无 body 字段时为 `{}`，与 reload 等既有
//!   端点契约一致）；GET 不携带 body；
//! - 响应 JSON 原样返回，错误经 `check_response_full` 口径透出。

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::api::evorule_client::EvoruleApiClient;
use crate::api::workspace_client::WorkspaceApiClient;
use crate::builtin_tools::{ParameterSpec, ToolSpec};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};

/// 目标 API client（两 client 共享 ApiCore 鉴权与错误口径，按端点域划分）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClientKind {
    /// WorkspaceApiClient（工作区治理域：workspace/规则/沙盒/发布/生产/转译）
    Workspace,
    /// EvoruleApiClient（会话/审计/知识/bundle 执行域）
    Evorule,
}

/// HTTP 方法（透传面只需要三个）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HttpMethod {
    Get,
    Post,
    Patch,
}

impl From<HttpMethod> for reqwest::Method {
    fn from(m: HttpMethod) -> Self {
        match m {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Patch => reqwest::Method::PATCH,
        }
    }
}

/// 参数落点
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Loc {
    /// path 模板 `{name}` 占位替换
    Path,
    /// query string（`?k=v&…`，值字符串化拼接）
    Query,
    /// JSON 请求体字段（键 = 参数名）
    Body,
}

/// 参数取值口径（对齐原手写实现的 `and_then(as_xxx)` 链路语义：
/// required 类型不符 = 视同缺失报缺参错误；optional 类型不符 = 忽略）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParamKind {
    /// `as_str`
    Str,
    /// `as_i64`（`non_negative=true` 时负值报 `{name} must be non-negative`）
    Int,
    /// 原样对象（形状校验属 server 职责，本地不拦）
    Obj,
    /// 原样数组
    Arr,
    /// `as_bool`
    Bool,
}

impl ParamKind {
    /// OpenAI function schema 的参数 type 字面量
    fn schema_type(self) -> &'static str {
        match self {
            ParamKind::Str => "string",
            ParamKind::Int => "integer",
            ParamKind::Obj => "object",
            ParamKind::Arr => "array",
            ParamKind::Bool => "boolean",
        }
    }
}

/// 参数绑定声明（字段全字面量声明，便于逐字对照原手写 spec/实现）
pub struct ParamBinding {
    pub name: &'static str,
    pub kind: ParamKind,
    pub loc: Loc,
    pub required: bool,
    /// Int 专用：非负校验（负值报 `{name} must be non-negative`）
    pub non_negative: bool,
    /// 参数 spec description（逐字沿用原手写文本）
    pub description: &'static str,
}

/// 端点绑定声明（一个透传工具 = 一条）
pub struct EndpointBinding {
    /// 工具名（LLM 面稳定标识，白名单锚点以此为准）
    pub name: &'static str,
    pub client: ClientKind,
    pub method: HttpMethod,
    /// path 模板（`{param}` 占位对应 Loc::Path 参数名；query 运行期追加）。
    /// 权威来源 = /api/openapi.json paths。
    pub path: &'static str,
    /// 工具 spec description（逐字沿用原手写文本，LLM 契约零漂移）
    pub description: &'static str,
    pub params: &'static [ParamBinding],
}

impl EndpointBinding {
    /// 校验 path 模板占位符与 Loc::Path 参数绑定双向一致（表配置错误早暴露）
    fn path_placeholders_consistent(&self) -> bool {
        let mut placeholders: Vec<&str> = Vec::new();
        let rest = self.path;
        let mut scan = rest;
        while let Some(start) = scan.find('{') {
            let after = &scan[start + 1..];
            if let Some(end) = after.find('}') {
                placeholders.push(&after[..end]);
                scan = &after[end + 1..];
            } else {
                return false;
            }
        }
        let path_params: Vec<&str> = self
            .params
            .iter()
            .filter(|p| p.loc == Loc::Path)
            .map(|p| p.name)
            .collect();
        placeholders.len() == path_params.len()
            && placeholders.iter().all(|ph| path_params.contains(ph))
            && path_params.iter().all(|pp| placeholders.contains(pp))
    }
}

/// path/query 参数值字符串化（Str/Int/Bool；Obj/Arr 出现在 Path/Query 属表配置
/// 错误，按 missing 处理）。错误文本与原手写链路一致：`missing required parameter: {name}`。
fn coerce_scalar(raw: &Value, kind: ParamKind, name: &str) -> Result<String, String> {
    let s = match kind {
        ParamKind::Str => raw.as_str().map(str::to_string),
        ParamKind::Int => raw.as_i64().map(|v| v.to_string()),
        ParamKind::Bool => raw.as_bool().map(|b| b.to_string()),
        _ => None,
    };
    s.ok_or_else(|| format!("missing required parameter: {name}"))
}

/// 泛型透传执行器：按 binding 取参 → 组装请求 → 单次 client 调用 → 响应原样返回
#[derive(Clone)]
pub struct PassthroughTool {
    ws: WorkspaceApiClient,
    ev: EvoruleApiClient,
    binding: &'static EndpointBinding,
}

impl PassthroughTool {
    pub fn new(
        ws: &WorkspaceApiClient,
        ev: &EvoruleApiClient,
        binding: &'static EndpointBinding,
    ) -> Self {
        Self {
            ws: ws.clone(),
            ev: ev.clone(),
            binding,
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for PassthroughTool {
    async fn call(&self, args: &Value) -> IoResult {
        let mut path = self.binding.path.to_string();
        let mut query = String::new();
        let mut body = Map::new();
        for p in self.binding.params {
            let raw = match args.get(p.name) {
                Some(v) if !v.is_null() => v,
                _ => {
                    if p.required {
                        return Err(format!("missing required parameter: {}", p.name));
                    }
                    continue;
                }
            };
            match p.loc {
                Loc::Path => {
                    let s = coerce_scalar(raw, p.kind, p.name)?;
                    path = path.replace(&format!("{{{}}}", p.name), &s);
                }
                Loc::Query => {
                    let s = coerce_scalar(raw, p.kind, p.name)?;
                    query.push(if query.is_empty() { '?' } else { '&' });
                    query.push_str(&format!("{}={}", p.name, s));
                }
                Loc::Body => {
                    // 类型口径对齐原手写 and_then(as_xxx) 链路：required 类型不符 =
                    // 视同缺失，optional 类型不符 = 忽略；Obj 存在即透传（形状校验
                    // 属 server 职责，对齐 rule_to_transform 等既有语义）
                    let type_ok = match p.kind {
                        ParamKind::Obj => true,
                        ParamKind::Str => raw.is_string(),
                        ParamKind::Int => raw.is_i64(),
                        ParamKind::Arr => raw.is_array(),
                        ParamKind::Bool => raw.is_boolean(),
                    };
                    if !type_ok {
                        if p.required {
                            return Err(format!("missing required parameter: {}", p.name));
                        }
                        continue;
                    }
                    if p.kind == ParamKind::Int && p.non_negative {
                        match raw.as_i64() {
                            Some(v) if v < 0 => {
                                return Err(format!("{} must be non-negative", p.name));
                            }
                            Some(_) => {}
                            None => return Err(format!("missing required parameter: {}", p.name)),
                        }
                    }
                    body.insert(p.name.to_string(), raw.clone());
                }
            }
        }
        if path.contains('{') {
            return Err(format!(
                "tool '{}' binding error: unresolved path placeholder",
                self.binding.name
            ));
        }
        let body_arg = match self.binding.method {
            HttpMethod::Get => None,
            _ => Some(Value::Object(body)),
        };
        let url_path = format!("{path}{query}");
        let result = match self.binding.client {
            ClientKind::Workspace => {
                self.ws
                    .passthrough_request(self.binding.method.into(), &url_path, body_arg.as_ref())
                    .await
            }
            ClientKind::Evorule => {
                self.ev
                    .passthrough_request(self.binding.method.into(), &url_path, body_arg.as_ref())
                    .await
            }
        }
        .map_err(|e| e.to_string())?;
        Ok(result)
    }
}

/// 从一条 binding 生成工具 spec（description/参数逐字沿用 binding 声明）
pub fn spec_of(b: &EndpointBinding) -> ToolSpec {
    ToolSpec {
        name: b.name.to_string(),
        description: b.description.to_string(),
        parameters: b
            .params
            .iter()
            .map(|p| ParameterSpec {
                name: p.name.to_string(),
                r#type: p.kind.schema_type().to_string(),
                description: p.description.to_string(),
                required: p.required,
            })
            .collect(),
    }
}

/// 从 binding 列表批量生成 spec
pub fn specs_from(bindings: &[&'static EndpointBinding]) -> Vec<ToolSpec> {
    bindings.iter().map(|b| spec_of(b)).collect()
}

/// 按 binding 列表批量注册透传执行器
pub fn register_bindings(
    h: &mut ToolHandler,
    ws: &WorkspaceApiClient,
    ev: &EvoruleApiClient,
    bindings: &[&'static EndpointBinding],
) {
    for b in bindings {
        debug_assert!(
            b.path_placeholders_consistent(),
            "{} path placeholders inconsistent with Path params",
            b.name
        );
        h.register_tool(b.name, Arc::new(PassthroughTool::new(ws, ev, b)));
    }
}

// =============================================================================
// workspace 族（2 个）—— ws_list / ws_create
// =============================================================================

pub static WS_LIST: EndpointBinding = EndpointBinding {
    name: "ws_list",
    client: ClientKind::Workspace,
    method: HttpMethod::Get,
    path: "/api/workspaces",
    description: "List all workspaces, optionally filtered by owner_id.",
    params: &[ParamBinding {
        name: "owner_id",
        kind: ParamKind::Str,
        loc: Loc::Query,
        required: false,
        non_negative: false,
        description: "Optional owner id to filter workspaces.",
    }],
};

pub static WS_CREATE: EndpointBinding = EndpointBinding {
    name: "ws_create",
    client: ClientKind::Workspace,
    method: HttpMethod::Post,
    path: "/api/workspaces",
    description: "Create a new workspace.",
    params: &[
        ParamBinding {
            name: "name",
            kind: ParamKind::Str,
            loc: Loc::Body,
            required: true,
            non_negative: false,
            description: "Workspace name.",
        },
        ParamBinding {
            name: "owner_id",
            kind: ParamKind::Str,
            loc: Loc::Body,
            required: true,
            non_negative: false,
            description: "Owner id.",
        },
        ParamBinding {
            name: "description",
            kind: ParamKind::Str,
            loc: Loc::Body,
            required: false,
            non_negative: false,
            description: "Optional workspace description.",
        },
    ],
};

/// workspace 族端点绑定（D-2 实测：聚合面完整覆盖，直接走适配器无需补充表）
pub static WORKSPACE_BINDINGS: &[&EndpointBinding] = &[&WS_LIST, &WS_CREATE];

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn make_ws() -> WorkspaceApiClient {
        WorkspaceApiClient::new("http://localhost:0")
    }

    fn make_ev() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:0")
    }

    #[test]
    fn test_ws_bindings_specs_equal_handwritten() {
        // 等价锚点：适配器生成的 spec 与原手写 spec 逐字段全等（LLM 契约零漂移）
        let adapted = specs_from(WORKSPACE_BINDINGS);
        let handwritten = crate::rule_tools::workspace_tools::specs();
        assert_eq!(adapted.len(), handwritten.len());
        for (a, h) in adapted.iter().zip(handwritten.iter()) {
            assert_eq!(a.name, h.name, "tool name mismatch");
            assert_eq!(
                a.description, h.description,
                "{} description mismatch",
                h.name
            );
            assert_eq!(
                a.parameters.len(),
                h.parameters.len(),
                "{} parameter count mismatch",
                h.name
            );
            for (ap, hp) in a.parameters.iter().zip(h.parameters.iter()) {
                assert_eq!(ap.name, hp.name, "{} param name mismatch", h.name);
                assert_eq!(ap.r#type, hp.r#type, "{}:{} type mismatch", h.name, hp.name);
                assert_eq!(
                    ap.description, hp.description,
                    "{}:{} description mismatch",
                    h.name, hp.name
                );
                assert_eq!(
                    ap.required, hp.required,
                    "{}:{} required mismatch",
                    h.name, hp.name
                );
            }
        }
    }

    #[test]
    fn test_bindings_names_unique() {
        let mut names: Vec<&str> = WORKSPACE_BINDINGS.iter().map(|b| b.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            WORKSPACE_BINDINGS.len(),
            "binding names must be unique"
        );
    }

    #[test]
    fn test_path_placeholders_consistent() {
        for b in WORKSPACE_BINDINGS {
            assert!(
                b.path_placeholders_consistent(),
                "{} path placeholders inconsistent with Path params",
                b.name
            );
        }
    }

    #[tokio::test]
    async fn test_ws_create_missing_name() {
        let h = ToolHandler::new();
        let tool = PassthroughTool::new(&make_ws(), &make_ev(), &WS_CREATE);
        let args = Value::Object(Map::new());
        let err = tool.call(&args).await.unwrap_err();
        assert_eq!(err, "missing required parameter: name");
        drop(h);
    }

    #[tokio::test]
    async fn test_ws_create_missing_owner_id() {
        let tool = PassthroughTool::new(&make_ws(), &make_ev(), &WS_CREATE);
        let args = serde_json::json!({ "name": "ws" });
        let err = tool.call(&args).await.unwrap_err();
        assert_eq!(err, "missing required parameter: owner_id");
    }

    #[tokio::test]
    async fn test_required_wrong_type_treated_as_missing() {
        // 等价语义：required 参数类型不符 = 视同缺失（对齐原 and_then(as_str) 链路）
        let tool = PassthroughTool::new(&make_ws(), &make_ev(), &WS_CREATE);
        let args = serde_json::json!({ "name": 42, "owner_id": "u1" });
        let err = tool.call(&args).await.unwrap_err();
        assert_eq!(err, "missing required parameter: name");
    }

    #[tokio::test]
    async fn test_optional_wrong_type_ignored() {
        // optional 类型不符 → 忽略该参数，走到网络层失败（localhost:0 连接拒绝）
        let tool = PassthroughTool::new(&make_ws(), &make_ev(), &WS_CREATE);
        let args = serde_json::json!({ "name": "ws", "owner_id": "u1", "description": 42 });
        let result = tool.call(&args).await;
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(!msg.contains("missing required parameter"), "实际: {msg}");
    }

    #[test]
    fn test_register_bindings() {
        let mut h = ToolHandler::new();
        register_bindings(&mut h, &make_ws(), &make_ev(), WORKSPACE_BINDINGS);
        assert!(h.has_tool("ws_list"));
        assert!(h.has_tool("ws_create"));
    }
}
