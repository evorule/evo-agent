// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 通用 OpenAPI→ToolFunction 适配器（rule_tools 透传工具的表驱动底座）
//!
//! 「取参 → 单次 client 调用 → 响应原样返回」的薄包装工具统一为表驱动：
//! [`EndpointBinding`] 声明一个工具的端点映射（client / method / path 模板 /
//! 参数绑定），[`build_request_parts`] 纯函数组装请求，[`PassthroughTool`]
//! 泛型执行，[`specs_from`] 生成静态 spec，[`register_bindings`] 注册执行器。
//!
//! 端点元数据权威来源 = evorule-server `/api/openapi.json`（utoipa 聚合面，
//! 运行时 merge workspace spec，聚合面对全部治理端点完整覆盖无缺口——D-2 实测）。
//! 设计演化说明：原 PR1-1 方案「运行时 fetch openapi.json」不可行——
//! `rule_tool_specs()` 是同步函数（runner 注入点契约）且白名单单测须离线运行，
//! 故端点映射表于设计期从 openapi.json 提取后固化为代码常量（D-4 建议形态）；
//! openapi.json 仍是设计期权威来源，映射表漂移由 spec 等价测试锁定。
//!
//! 等价纪律（与原手写薄包装逐项对齐，LLM 契约零漂移）：
//! - spec 文本（工具/参数 description）逐字沿用原手写 spec；
//! - required 缺失错误文本 = `missing required parameter: {name}`；
//! - required 参数类型不符视同缺失（对齐原 `and_then(as_str)` 链路语义）；
//! - optional 参数类型不符时忽略（同上）；
//! - POST/PATCH 一律携带 JSON body（无 body 字段时为 `{}`，与 reload 等既有
//!   端点契约一致）；GET 不携带 body；
//! - Query 值百分号编码仅在原实现编码处启用（knowledge_search 三参数，对齐
//!   原手写 `urlencode` 调用点；path 段与其余 query 原样拼接）；
//! - rule_validate 的 422（校验未通过业务结果）经 `accept_statuses` 视为成功；
//! - 响应 JSON 原样返回，错误经 `check_response_full` 口径透出（原手写个别
//!   端点走 `check_response` 不透出 server 详情，此处统一为 full 口径——错误
//!   信息更详细，成功路径行为不变）；
//! - 原手写 typed struct 中 `Option` 字段为 `None` 时序列化为 `"k": null`，
//!   本表缺席即不含键——server 端 `Option` 反序列化语义下 null 与缺席等价。

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::api::evorule_client::{urlencode, EvoruleApiClient};
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
    /// 原样对象（`require_object=true` 时非 object 报缺参错误，对齐 bundle
    /// 二工具原实现的 `is_object` 前置校验）
    Obj,
    /// 原样数组
    Arr,
    /// `as_bool`
    Bool,
    /// 字符串数组：`as_array` → 逐元素 `as_str` 过滤收集（对齐 sandbox_start /
    /// publish_submit 原实现的 `filter_map(as_str)` 链路；非字符串元素静默丢弃）
    StrArr,
}

impl ParamKind {
    /// OpenAI function schema 的参数 type 字面量
    fn schema_type(self) -> &'static str {
        match self {
            ParamKind::Str => "string",
            ParamKind::Int => "integer",
            ParamKind::Obj => "object",
            ParamKind::Arr | ParamKind::StrArr => "array",
            ParamKind::Bool => "boolean",
        }
    }
}

/// 参数绑定声明
pub struct ParamBinding {
    pub name: &'static str,
    pub kind: ParamKind,
    pub loc: Loc,
    pub required: bool,
    /// Int 专用：非负校验（负值报 `{name} must be non-negative`）
    pub non_negative: bool,
    /// Query 专用：值百分号编码（对齐原手写 urlencode 调用点）
    pub url_encode: bool,
    /// Obj 专用：非 object 视同缺失（bundle 二工具）
    pub require_object: bool,
    /// 参数 spec description（逐字沿用原手写文本）
    pub description: &'static str,
}

/// ParamBinding 默认值底版（声明处用结构体更新语法 `..PARAM_DEFAULTS` 省略布尔开关）
const PARAM_DEFAULTS: ParamBinding = ParamBinding {
    name: "",
    kind: ParamKind::Str,
    loc: Loc::Body,
    required: false,
    non_negative: false,
    url_encode: false,
    require_object: false,
    description: "",
};

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
    /// 非 2xx 但视为成功的状态码（如 rule_validate 的 422 = 校验未通过业务
    /// 结果，响应原样返回）；空 = 严格 2xx 口径。
    pub accept_statuses: &'static [u16],
}

/// EndpointBinding 默认值底版（声明处用 `..BINDING_DEFAULTS` 省略 accept_statuses）
const BINDING_DEFAULTS: EndpointBinding = EndpointBinding {
    name: "",
    client: ClientKind::Workspace,
    method: HttpMethod::Get,
    path: "",
    description: "",
    params: &[],
    accept_statuses: &[],
};

impl EndpointBinding {
    /// 校验 path 模板占位符与 Loc::Path 参数绑定双向一致（表配置错误早暴露）
    fn path_placeholders_consistent(&self) -> bool {
        let mut placeholders: Vec<&str> = Vec::new();
        let mut scan = self.path;
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

/// 类型口径（对齐原手写 and_then(as_xxx) 链路；Obj 的 require_object 变体）
fn param_type_ok(p: &ParamBinding, raw: &Value) -> bool {
    match p.kind {
        ParamKind::Str => raw.is_string(),
        ParamKind::Int => raw.is_i64(),
        ParamKind::Obj => !p.require_object || raw.is_object(),
        ParamKind::Arr | ParamKind::StrArr => raw.is_array(),
        ParamKind::Bool => raw.is_boolean(),
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

/// 纯函数：按 binding 从 args 取参 → 组装 (path[?query], body)。
///
/// 错误 = 取参/校验失败（缺参、类型不符、负值、占位符未解析）；
/// 成功 = 请求形态（GET 的 body 恒为 `None`，POST/PATCH 恒为 `Some`——
/// 无 body 字段时为 `{}`）。单独抽出以便离线断言请求形状。
pub(crate) fn build_request_parts(
    binding: &EndpointBinding,
    args: &Value,
) -> Result<(String, Option<Value>), String> {
    let mut path = binding.path.to_string();
    let mut query = String::new();
    let mut body = Map::new();
    for p in binding.params {
        let raw = match args.get(p.name) {
            Some(v) if !v.is_null() => v,
            _ => {
                if p.required {
                    return Err(format!("missing required parameter: {}", p.name));
                }
                continue;
            }
        };
        // 类型口径：required 类型不符 = 视同缺失，optional 类型不符 = 忽略
        if !param_type_ok(p, raw) {
            if p.required {
                return Err(format!("missing required parameter: {}", p.name));
            }
            continue;
        }
        if p.kind == ParamKind::Int && p.non_negative && raw.as_i64().is_some_and(|v| v < 0) {
            return Err(format!("{} must be non-negative", p.name));
        }
        match p.loc {
            Loc::Path => {
                let s = coerce_scalar(raw, p.kind, p.name)?;
                path = path.replace(&format!("{{{}}}", p.name), &s);
            }
            Loc::Query => {
                let s = coerce_scalar(raw, p.kind, p.name)?;
                let val = if p.url_encode { urlencode(&s) } else { s };
                query.push(if query.is_empty() { '?' } else { '&' });
                query.push_str(p.name);
                query.push('=');
                query.push_str(&val);
            }
            Loc::Body => {
                let v = if p.kind == ParamKind::StrArr {
                    // as_array → 逐元素 as_str 过滤收集（type_ok 已保证 is_array）
                    Value::Array(
                        raw.as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|e| e.as_str().map(String::from))
                            .map(Value::String)
                            .collect(),
                    )
                } else {
                    raw.clone()
                };
                body.insert(p.name.to_string(), v);
            }
        }
    }
    if path.contains('{') {
        return Err(format!(
            "tool '{}' binding error: unresolved path placeholder",
            binding.name
        ));
    }
    let body_arg = match binding.method {
        HttpMethod::Get => None,
        _ => Some(Value::Object(body)),
    };
    Ok((format!("{path}{query}"), body_arg))
}

/// 泛型透传执行器：按 binding 组装请求 → 单次 client 调用 → 响应原样返回
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
        let (url_path, body_arg) = build_request_parts(self.binding, args)?;
        let result = match self.binding.client {
            ClientKind::Workspace => {
                self.ws
                    .passthrough_request(
                        self.binding.method.into(),
                        &url_path,
                        body_arg.as_ref(),
                        self.binding.accept_statuses,
                    )
                    .await
            }
            ClientKind::Evorule => {
                self.ev
                    .passthrough_request(
                        self.binding.method.into(),
                        &url_path,
                        body_arg.as_ref(),
                        self.binding.accept_statuses,
                    )
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
        h.register_static(b.name, Arc::new(PassthroughTool::new(ws, ev, b)));
    }
}

// =============================================================================
// workspace 族（2）—— ws_list / ws_create
// =============================================================================

pub static WS_LIST: EndpointBinding = EndpointBinding {
    name: "ws_list",
    path: "/api/workspaces",
    description: "List all workspaces, optionally filtered by owner_id.",
    params: &[ParamBinding {
        name: "owner_id",
        loc: Loc::Query,
        description: "Optional owner id to filter workspaces.",
        ..PARAM_DEFAULTS
    }],
    ..BINDING_DEFAULTS
};

pub static WS_CREATE: EndpointBinding = EndpointBinding {
    name: "ws_create",
    method: HttpMethod::Post,
    path: "/api/workspaces",
    description: "Create a new workspace.",
    params: &[
        ParamBinding {
            name: "name",
            required: true,
            description: "Workspace name.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "owner_id",
            required: true,
            description: "Owner id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "description",
            description: "Optional workspace description.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

// =============================================================================
// rule 族（12）—— rule_crud 1:1 映射 A1 端点
// =============================================================================

pub static RULE_LIST: EndpointBinding = EndpointBinding {
    name: "rule_list",
    path: "/api/workspaces/{workspace_id}/rules",
    description: "List all rules in a workspace.",
    params: &[ParamBinding {
        name: "workspace_id",
        loc: Loc::Path,
        required: true,
        description: "Workspace id.",
        ..PARAM_DEFAULTS
    }],
    ..BINDING_DEFAULTS
};

pub static RULE_GET: EndpointBinding = EndpointBinding {
    name: "rule_get",
    path: "/api/workspaces/{workspace_id}/rules/{rule_id}",
    description: "Get a rule's details.",
    params: &[
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Path,
            required: true,
            description: "Workspace id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "rule_id",
            loc: Loc::Path,
            required: true,
            description: "Rule id.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static RULE_CREATE: EndpointBinding = EndpointBinding {
    name: "rule_create",
    method: HttpMethod::Post,
    path: "/api/workspaces/{workspace_id}/rules",
    description: "Create a new rule in a workspace.",
    params: &[
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Path,
            required: true,
            description: "Workspace id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "name",
            required: true,
            description: "Rule name.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "content",
            required: true,
            description: "Rule content (JSON string).",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "created_by",
            required: true,
            description: "Creator id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "description",
            description: "Optional rule description.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static RULE_UPDATE: EndpointBinding = EndpointBinding {
    name: "rule_update",
    method: HttpMethod::Patch,
    path: "/api/workspaces/{workspace_id}/rules/{rule_id}",
    description: "Update rule content (only allowed in Draft state).",
    params: &[
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Path,
            required: true,
            description: "Workspace id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "rule_id",
            loc: Loc::Path,
            required: true,
            description: "Rule id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "content",
            required: true,
            description: "New rule content (JSON string).",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "updated_by",
            required: true,
            description: "Updater id.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static RULE_VERSIONS: EndpointBinding = EndpointBinding {
    name: "rule_versions",
    path: "/api/workspaces/{workspace_id}/rules/{rule_id}/versions",
    description: "List all versions of a rule (descending by version).",
    params: &[
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Path,
            required: true,
            description: "Workspace id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "rule_id",
            loc: Loc::Path,
            required: true,
            description: "Rule id.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static RULE_VERSION_GET: EndpointBinding = EndpointBinding {
    name: "rule_version_get",
    path: "/api/workspaces/{workspace_id}/rules/{rule_id}/versions/{version_id}",
    description: "Get a specific rule version.",
    params: &[
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Path,
            required: true,
            description: "Workspace id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "rule_id",
            loc: Loc::Path,
            required: true,
            description: "Rule id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "version_id",
            loc: Loc::Path,
            required: true,
            description: "Version id.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static RULE_SUBMIT: EndpointBinding = EndpointBinding {
    name: "rule_submit",
    method: HttpMethod::Post,
    path: "/api/workspaces/{workspace_id}/rules/{rule_id}/submit",
    description: "Submit a rule (Draft -> Candidate).",
    params: &[
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Path,
            required: true,
            description: "Workspace id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "rule_id",
            loc: Loc::Path,
            required: true,
            description: "Rule id.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static RULE_ACTIVATE: EndpointBinding = EndpointBinding {
    name: "rule_activate",
    method: HttpMethod::Post,
    path: "/api/workspaces/{workspace_id}/rules/{rule_id}/activate",
    description: "Activate a rule.",
    params: &[
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Path,
            required: true,
            description: "Workspace id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "rule_id",
            loc: Loc::Path,
            required: true,
            description: "Rule id.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static RULE_BLOCK: EndpointBinding = EndpointBinding {
    name: "rule_block",
    method: HttpMethod::Post,
    path: "/api/workspaces/{workspace_id}/rules/{rule_id}/block",
    description: "Block a rule.",
    params: &[
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Path,
            required: true,
            description: "Workspace id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "rule_id",
            loc: Loc::Path,
            required: true,
            description: "Rule id.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static RULE_ARCHIVE: EndpointBinding = EndpointBinding {
    name: "rule_archive",
    method: HttpMethod::Post,
    path: "/api/workspaces/{workspace_id}/rules/{rule_id}/archive",
    description: "Archive a rule.",
    params: &[
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Path,
            required: true,
            description: "Workspace id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "rule_id",
            loc: Loc::Path,
            required: true,
            description: "Rule id.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static RULE_FORK: EndpointBinding = EndpointBinding {
    name: "rule_fork",
    method: HttpMethod::Post,
    path: "/api/workspaces/{workspace_id}/rules/{rule_id}/fork",
    description: "Fork a rule into a new rule.",
    params: &[
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Path,
            required: true,
            description: "Workspace id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "rule_id",
            loc: Loc::Path,
            required: true,
            description: "Rule id to fork from.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "new_name",
            required: true,
            description: "Name for the forked rule.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "created_by",
            required: true,
            description: "Creator id.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static RULE_RELOAD: EndpointBinding = EndpointBinding {
    name: "rule_reload",
    method: HttpMethod::Post,
    path: "/api/rules/reload",
    description: "Hot-reload all rules from disk (TCB constitution + business rules).",
    params: &[],
    ..BINDING_DEFAULTS
};

// =============================================================================
// translate 族（3）—— rule_to_transform / rule_to_conditional / rule_validate
// =============================================================================

pub static RULE_TO_TRANSFORM: EndpointBinding = EndpointBinding {
    name: "rule_to_transform",
    method: HttpMethod::Post,
    path: "/api/rules/translate/to_transform",
    description: "Translate a rule (condition + action_set) into transform format.",
    params: &[ParamBinding {
        name: "body",
        kind: ParamKind::Obj,
        required: true,
        // 原实现仅检查存在性（任意 JSON 类型透传），不做 is_object 校验
        description: "Rule body to translate (JSON object).",
        ..PARAM_DEFAULTS
    }],
    ..BINDING_DEFAULTS
};

pub static RULE_TO_CONDITIONAL: EndpointBinding = EndpointBinding {
    name: "rule_to_conditional",
    method: HttpMethod::Post,
    path: "/api/rules/translate/to_conditional",
    description: "Translate a transform-format rule into a readable conditional view (lossy).",
    params: &[ParamBinding {
        name: "body",
        kind: ParamKind::Obj,
        required: true,
        description: "Rule body to translate (JSON object).",
        ..PARAM_DEFAULTS
    }],
    ..BINDING_DEFAULTS
};

pub static RULE_VALIDATE: EndpointBinding = EndpointBinding {
    name: "rule_validate",
    method: HttpMethod::Post,
    path: "/api/rules/validate",
    description: "Validate rules against G1-G7 constraints.",
    params: &[ParamBinding {
        name: "rules",
        required: true,
        description: "Rules to validate (JSON string).",
        ..PARAM_DEFAULTS
    }],
    // 422 = 校验未通过业务结果（非 HTTP 错误），响应原样返回
    accept_statuses: &[422],
    ..BINDING_DEFAULTS
};

// =============================================================================
// audit 族（2 透传）—— audit_get / session_rewind（audit_verify 为本地逻辑，
// 驻 audit_tools.rs）
// =============================================================================

pub static AUDIT_GET: EndpointBinding = EndpointBinding {
    name: "audit_get",
    client: ClientKind::Evorule,
    path: "/api/sessions/{session_id}/audit",
    description: "Get the audit report for a session.",
    params: &[ParamBinding {
        name: "session_id",
        loc: Loc::Path,
        required: true,
        description: "Session id.",
        ..PARAM_DEFAULTS
    }],
    ..BINDING_DEFAULTS
};

pub static SESSION_REWIND: EndpointBinding = EndpointBinding {
    name: "session_rewind",
    client: ClientKind::Evorule,
    path: "/api/sessions/{session_id}/rewind",
    description: "Rewind a session to a specific version.",
    params: &[
        ParamBinding {
            name: "session_id",
            loc: Loc::Path,
            required: true,
            description: "Session id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "version",
            kind: ParamKind::Int,
            loc: Loc::Query,
            required: true,
            non_negative: true,
            description: "Target version to rewind to (non-negative integer).",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

// =============================================================================
// sandbox 族（5）
// =============================================================================

pub static SANDBOX_START: EndpointBinding = EndpointBinding {
    name: "sandbox_start",
    method: HttpMethod::Post,
    path: "/api/workspaces/{workspace_id}/sandboxes",
    description:
        "Start a sandbox test session (fork production session + load draft rules + inject test cases).",
    params: &[
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Path,
            required: true,
            description: "Workspace id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "rule_version_ids",
            kind: ParamKind::StrArr,
            required: true,
            description: "Rule version ids to test (array of strings).",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "test_dataset_id",
            kind: ParamKind::Int,
            required: true,
            description: "Synthetic test dataset id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "started_by",
            required: true,
            description: "User id starting the sandbox (must be a workspace member).",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "parent_version",
            kind: ParamKind::Int,
            description: "Optional production session version to fork from (default: latest).",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static SANDBOX_LIST: EndpointBinding = EndpointBinding {
    name: "sandbox_list",
    path: "/api/workspaces/{workspace_id}/sandboxes",
    description: "List sandbox test history for a workspace.",
    params: &[
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Path,
            required: true,
            description: "Workspace id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "requester",
            loc: Loc::Query,
            required: true,
            description: "Requester user id (for workspace member check).",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static SANDBOX_GET: EndpointBinding = EndpointBinding {
    name: "sandbox_get",
    path: "/api/workspaces/{workspace_id}/sandboxes/{sandbox_id}",
    description: "Get a sandbox session's details.",
    params: &[
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Path,
            required: true,
            description: "Workspace id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "sandbox_id",
            kind: ParamKind::Int,
            loc: Loc::Path,
            required: true,
            description: "Sandbox id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "requester",
            loc: Loc::Query,
            required: true,
            description: "Requester user id (for workspace member check).",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static SANDBOX_CLOSE: EndpointBinding = EndpointBinding {
    name: "sandbox_close",
    method: HttpMethod::Post,
    path: "/api/workspaces/{workspace_id}/sandboxes/{sandbox_id}/close",
    description: "Close a sandbox session (export test facts + close session).",
    params: &[
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Path,
            required: true,
            description: "Workspace id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "sandbox_id",
            kind: ParamKind::Int,
            loc: Loc::Path,
            required: true,
            description: "Sandbox id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "closed_by",
            required: true,
            description: "User id closing the sandbox (must be a workspace member).",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static SANDBOX_REPORT: EndpointBinding = EndpointBinding {
    name: "sandbox_report",
    path: "/api/workspaces/{workspace_id}/sandboxes/{sandbox_id}/report",
    description: "Get the test report (BLAKE3-signed) for a closed/running sandbox.",
    params: &[
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Path,
            required: true,
            description: "Workspace id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "sandbox_id",
            kind: ParamKind::Int,
            loc: Loc::Path,
            required: true,
            description: "Sandbox id.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

// =============================================================================
// dataset 族（2）
// =============================================================================

pub static DATASET_CREATE: EndpointBinding = EndpointBinding {
    name: "dataset_create",
    method: HttpMethod::Post,
    path: "/api/workspaces/{workspace_id}/test-datasets",
    description: "Create a synthetic test dataset for sandbox testing.",
    params: &[
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Path,
            required: true,
            description: "Workspace id (path).",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "name",
            required: true,
            description: "Dataset name.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "cases_json",
            required: true,
            description: "Test cases as a JSON array string.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "created_by",
            required: true,
            description: "Creator user id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "description",
            description: "Optional dataset description.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static DATASET_LIST: EndpointBinding = EndpointBinding {
    name: "dataset_list",
    path: "/api/workspaces/{workspace_id}/test-datasets",
    description: "List test datasets for a workspace.",
    params: &[ParamBinding {
        name: "workspace_id",
        loc: Loc::Path,
        required: true,
        description: "Workspace id.",
        ..PARAM_DEFAULTS
    }],
    ..BINDING_DEFAULTS
};

// =============================================================================
// publish 族（5）
// =============================================================================

pub static PUBLISH_SUBMIT: EndpointBinding = EndpointBinding {
    name: "publish_submit",
    method: HttpMethod::Post,
    path: "/api/publish/queue",
    description: "Submit rules to the publish queue (requires DepartmentHead role).",
    params: &[
        ParamBinding {
            name: "workspace_id",
            required: true,
            description: "Source workspace id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "rule_version_ids",
            kind: ParamKind::StrArr,
            required: true,
            description: "Candidate rule version ids to publish (array of strings).",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "submitted_by",
            required: true,
            description: "Submitter user id (department head).",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "role",
            required: true,
            description: "Publish role (doctor / department_head / admin).",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "test_report_sandbox_id",
            kind: ParamKind::Int,
            description: "Optional attached test report sandbox id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "description",
            description: "Optional release description.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static PUBLISH_LIST: EndpointBinding = EndpointBinding {
    name: "publish_list",
    path: "/api/publish/queue",
    description: "List the publish queue, optionally filtered by status and workspace.",
    params: &[
        ParamBinding {
            name: "status",
            loc: Loc::Query,
            description: "Optional status filter (pending/approved/published/rejected/cancelled).",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "workspace_id",
            loc: Loc::Query,
            description: "Optional workspace filter; omit to list across all workspaces.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static PUBLISH_QUEUE_GET: EndpointBinding = EndpointBinding {
    name: "publish_queue_get",
    path: "/api/publish/queue/{queue_id}",
    description: "Get a publish queue item's details.",
    params: &[ParamBinding {
        name: "queue_id",
        kind: ParamKind::Int,
        loc: Loc::Path,
        required: true,
        description: "Queue item id.",
        ..PARAM_DEFAULTS
    }],
    ..BINDING_DEFAULTS
};

pub static PUBLISH_REVIEW: EndpointBinding = EndpointBinding {
    name: "publish_review",
    method: HttpMethod::Post,
    path: "/api/publish/queue/{queue_id}/review",
    description: "Review (approve/reject) a publish queue item (requires Admin role).",
    params: &[
        ParamBinding {
            name: "queue_id",
            kind: ParamKind::Int,
            loc: Loc::Path,
            required: true,
            description: "Queue item id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "decision",
            required: true,
            description: "Review decision (approved / rejected).",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "reviewed_by",
            required: true,
            description: "Reviewer user id (admin).",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "role",
            required: true,
            description: "Publish role (must be admin).",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "comment",
            description: "Optional review comment.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static PUBLISH_ROLLBACK: EndpointBinding = EndpointBinding {
    name: "publish_rollback",
    method: HttpMethod::Post,
    path: "/api/publish/rollback",
    description: "Emergency rollback to a target ruleset version (requires Admin role).",
    params: &[
        ParamBinding {
            name: "target_version",
            kind: ParamKind::Int,
            required: true,
            description: "Target ruleset version to roll back to.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "reason",
            required: true,
            description: "Rollback reason.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "operated_by",
            required: true,
            description: "Operator user id (admin).",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "role",
            required: true,
            description: "Publish role (must be admin).",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

// =============================================================================
// production 族（2）
// =============================================================================

pub static PROD_STATE: EndpointBinding = EndpointBinding {
    name: "prod_state",
    path: "/api/production/state",
    description: "Get the current production state (active session + ruleset version).",
    params: &[],
    ..BINDING_DEFAULTS
};

pub static PROD_AUDIT: EndpointBinding = EndpointBinding {
    name: "prod_audit",
    path: "/api/production/audit",
    description: "List publish/rollback audit history (most recent first).",
    params: &[ParamBinding {
        name: "limit",
        kind: ParamKind::Int,
        loc: Loc::Query,
        description: "Optional max number of records to return (default 50).",
        ..PARAM_DEFAULTS
    }],
    ..BINDING_DEFAULTS
};

// =============================================================================
// bundles 族（4 透传）—— bundle_export 为本地逻辑（带证据校验），驻
// bundle_tools.rs
// =============================================================================

pub static BUNDLE_IMPORT_DRY_RUN: EndpointBinding = EndpointBinding {
    name: "bundle_import_dry_run",
    client: ClientKind::Evorule,
    method: HttpMethod::Post,
    path: "/api/bundles/import/dry-run",
    description: "Pre-check a bundle import in the execution domain (all 8 \
                  validations run, no disk write, no reload). Pass the bundle object \
                  returned by bundle_export.",
    params: &[ParamBinding {
        name: "bundle",
        kind: ParamKind::Obj,
        required: true,
        require_object: true,
        description: "DatasetBundle JSON object (as returned by bundle_export).",
        ..PARAM_DEFAULTS
    }],
    ..BINDING_DEFAULTS
};

pub static BUNDLE_IMPORT: EndpointBinding = EndpointBinding {
    name: "bundle_import",
    client: ClientKind::Evorule,
    method: HttpMethod::Post,
    path: "/api/bundles/import",
    description: "Import and activate a bundle in the execution domain (destructive: \
                  atomic write to rules/bundles/ + reload; new sessions pick up the \
                  new ruleset). Run bundle_import_dry_run first.",
    params: &[ParamBinding {
        name: "bundle",
        kind: ParamKind::Obj,
        required: true,
        require_object: true,
        description: "DatasetBundle JSON object (as returned by bundle_export).",
        ..PARAM_DEFAULTS
    }],
    ..BINDING_DEFAULTS
};

pub static BUNDLE_ACTIVE_LIST: EndpointBinding = EndpointBinding {
    name: "bundle_active_list",
    client: ClientKind::Evorule,
    path: "/api/bundles/active",
    description: "List currently active bundles in the execution domain (from \
                  rules/bundles/*/bundle_manifest.json).",
    params: &[],
    ..BINDING_DEFAULTS
};

pub static BUNDLE_IMPORTS_LIST: EndpointBinding = EndpointBinding {
    name: "bundle_imports_list",
    client: ClientKind::Evorule,
    path: "/api/bundles/imports",
    description: "List bundle import provenance records in the execution domain \
                  (read-only audit trail).",
    params: &[],
    ..BINDING_DEFAULTS
};

// =============================================================================
// knowledge 族（3，只读数据面）—— q/domain/tags 走百分号编码（对齐原手写）
// =============================================================================

pub static KNOWLEDGE_DATASETS: EndpointBinding = EndpointBinding {
    name: "knowledge_datasets",
    client: ClientKind::Evorule,
    path: "/api/knowledge",
    description: "List knowledge datasets hosted in the execution domain (read-only \
                  data plane, GET /api/knowledge).",
    params: &[],
    ..BINDING_DEFAULTS
};

pub static KNOWLEDGE_SEARCH: EndpointBinding = EndpointBinding {
    name: "knowledge_search",
    client: ClientKind::Evorule,
    path: "/api/knowledge/{dataset}/entries",
    description: "Search entries in a knowledge dataset (GET \
                  /api/knowledge/{ds}/entries). Dataset not hosted → explicit 404.",
    params: &[
        ParamBinding {
            name: "dataset",
            loc: Loc::Path,
            required: true,
            description: "Dataset id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "q",
            loc: Loc::Query,
            url_encode: true,
            description: "Substring match across entry_id / schema_ref / bundle_id / \
                          payload.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "domain",
            loc: Loc::Query,
            url_encode: true,
            description: "Domain exact match (case-insensitive).",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "tags",
            loc: Loc::Query,
            url_encode: true,
            description: "Comma-separated tags (any-match).",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static KNOWLEDGE_ENTRY_GET: EndpointBinding = EndpointBinding {
    name: "knowledge_entry_get",
    client: ClientKind::Evorule,
    path: "/api/knowledge/{dataset}/entries/{entry_id}",
    description: "Fetch a single knowledge entry by id (payload returned verbatim, \
                  zero translation).",
    params: &[
        ParamBinding {
            name: "dataset",
            loc: Loc::Path,
            required: true,
            description: "Dataset id.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "entry_id",
            loc: Loc::Path,
            required: true,
            description: "Entry id.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

// =============================================================================
// hit-stats 族（2，只读观测面）—— 装备代谢数据源（39 号批 B3）
// =============================================================================

pub static HIT_STATS: EndpointBinding = EndpointBinding {
    name: "hit_stats",
    client: ClientKind::Evorule,
    path: "/api/rules/hit-stats",
    description: "List rule hit statistics for the current (or a historical) ruleset \
                  version (GET /api/rules/hit-stats). filter=hits returns hit \
                  entries only, filter=zero returns zero-hit dead-rule candidates.",
    params: &[
        ParamBinding {
            name: "version",
            loc: Loc::Query,
            url_encode: true,
            description: "Ruleset version (default: current); versions outside the \
                          retention window are rejected with 404.",
            ..PARAM_DEFAULTS
        },
        ParamBinding {
            name: "filter",
            loc: Loc::Query,
            url_encode: true,
            description: "all (default) | hits | zero.",
            ..PARAM_DEFAULTS
        },
    ],
    ..BINDING_DEFAULTS
};

pub static HIT_STATS_SERIES: EndpointBinding = EndpointBinding {
    name: "hit_stats_series",
    client: ClientKind::Evorule,
    path: "/api/rules/hit-stats/{rule_key}",
    description: "Per-rule cross-version hit series (GET \
                  /api/rules/hit-stats/{rule_key}). rule_key format: {index}@{source}, \
                  e.g. 0@core_eval or 2@rules%2Fbundles%2Fexpenses.json.",
    params: &[ParamBinding {
        name: "rule_key",
        loc: Loc::Path,
        required: true,
        description: "Rule key: {index}@{source} (source URL-encoded).",
        ..PARAM_DEFAULTS
    }],
    ..BINDING_DEFAULTS
};

// =============================================================================
// 全量透传 binding 表（42 个 = D-1 分类表纯透传族全集 + hit-stats 2）
// =============================================================================

/// 全部透传工具的端点绑定（顺序 = 原手写 specs 组装顺序，便于对照）。
/// 本地逻辑工具（audit_verify / bundle_export / meta_summary / evolution_signals /
/// rule_promote）不在此表，仍驻独立文件。
pub static ALL_TRANSPARENT_BINDINGS: &[&EndpointBinding] = &[
    // workspace 2
    &WS_LIST,
    &WS_CREATE,
    // rule 12
    &RULE_LIST,
    &RULE_GET,
    &RULE_CREATE,
    &RULE_UPDATE,
    &RULE_VERSIONS,
    &RULE_VERSION_GET,
    &RULE_SUBMIT,
    &RULE_ACTIVATE,
    &RULE_BLOCK,
    &RULE_ARCHIVE,
    &RULE_FORK,
    &RULE_RELOAD,
    // translate 3
    &RULE_TO_TRANSFORM,
    &RULE_TO_CONDITIONAL,
    &RULE_VALIDATE,
    // audit 2（audit_verify 本地逻辑除外）
    &AUDIT_GET,
    &SESSION_REWIND,
    // sandbox 5
    &SANDBOX_START,
    &SANDBOX_LIST,
    &SANDBOX_GET,
    &SANDBOX_CLOSE,
    &SANDBOX_REPORT,
    // dataset 2
    &DATASET_CREATE,
    &DATASET_LIST,
    // publish 5
    &PUBLISH_SUBMIT,
    &PUBLISH_LIST,
    &PUBLISH_QUEUE_GET,
    &PUBLISH_REVIEW,
    &PUBLISH_ROLLBACK,
    // production 2
    &PROD_STATE,
    &PROD_AUDIT,
    // bundles 4（bundle_export 本地逻辑除外）
    &BUNDLE_IMPORT_DRY_RUN,
    &BUNDLE_IMPORT,
    &BUNDLE_ACTIVE_LIST,
    &BUNDLE_IMPORTS_LIST,
    // knowledge 3
    &KNOWLEDGE_DATASETS,
    &KNOWLEDGE_SEARCH,
    &KNOWLEDGE_ENTRY_GET,
    // hit-stats 2（39 号批 B3，只读观测面）
    &HIT_STATS,
    &HIT_STATS_SERIES,
];

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

    fn dummy_value(p: &ParamBinding) -> Value {
        match p.kind {
            ParamKind::Str => Value::from("x"),
            ParamKind::Int => Value::from(1),
            ParamKind::Obj if p.require_object => serde_json::json!({}),
            // 原实现仅查存在性：任意 JSON 类型均可透传
            ParamKind::Obj => Value::from("any"),
            ParamKind::Arr | ParamKind::StrArr => Value::Array(vec![]),
            ParamKind::Bool => Value::from(true),
        }
    }

    fn wrong_typed_value(p: &ParamBinding) -> Value {
        match p.kind {
            ParamKind::Str => Value::from(42),
            _ => Value::from("not-the-right-type"),
        }
    }

    #[test]
    fn test_all_bindings_count_and_names_unique() {
        assert_eq!(ALL_TRANSPARENT_BINDINGS.len(), 42);
        let mut names: Vec<&str> = ALL_TRANSPARENT_BINDINGS.iter().map(|b| b.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ALL_TRANSPARENT_BINDINGS.len());
    }

    #[test]
    fn test_path_placeholders_consistent() {
        for b in ALL_TRANSPARENT_BINDINGS {
            assert!(
                b.path_placeholders_consistent(),
                "{} path placeholders inconsistent with Path params",
                b.name
            );
        }
    }

    #[test]
    fn test_accept_statuses_only_rule_validate() {
        for b in ALL_TRANSPARENT_BINDINGS {
            if b.name == "rule_validate" {
                assert_eq!(b.accept_statuses, &[422]);
            } else {
                assert!(
                    b.accept_statuses.is_empty(),
                    "{} has accept_statuses",
                    b.name
                );
            }
        }
    }

    #[test]
    fn test_register_bindings_all() {
        let mut h = ToolHandler::new();
        register_bindings(&mut h, &make_ws(), &make_ev(), ALL_TRANSPARENT_BINDINGS);
        for b in ALL_TRANSPARENT_BINDINGS {
            assert!(h.has_tool(b.name), "tool {} should be registered", b.name);
        }
    }

    // =========================================================================
    // 请求形状不变式（build_request_parts 纯函数离线断言）
    // =========================================================================

    #[test]
    fn test_ws_create_request_shape() {
        let args = serde_json::json!({ "name": "ws", "owner_id": "u1", "description": "d" });
        let (path, body) = build_request_parts(&WS_CREATE, &args).unwrap();
        assert_eq!(path, "/api/workspaces");
        assert_eq!(
            body,
            Some(serde_json::json!({ "name": "ws", "owner_id": "u1", "description": "d" }))
        );
        // optional 缺席 → body 不含该键
        let args = serde_json::json!({ "name": "ws", "owner_id": "u1" });
        let (_, body) = build_request_parts(&WS_CREATE, &args).unwrap();
        assert_eq!(
            body,
            Some(serde_json::json!({ "name": "ws", "owner_id": "u1" }))
        );
    }

    #[test]
    fn test_get_carries_no_body() {
        let args = serde_json::json!({ "session_id": "s1" });
        let (path, body) = build_request_parts(&AUDIT_GET, &args).unwrap();
        assert_eq!(path, "/api/sessions/s1/audit");
        assert_eq!(body, None);
    }

    #[test]
    fn test_post_without_body_params_carries_empty_object() {
        // POST/PATCH 恒带 body：无 body 字段时为 {}（reload 契约）
        let (path, body) = build_request_parts(&RULE_RELOAD, &Value::Object(Map::new())).unwrap();
        assert_eq!(path, "/api/rules/reload");
        assert_eq!(body, Some(serde_json::json!({})));
    }

    #[test]
    fn test_session_rewind_query_version() {
        let args = serde_json::json!({ "session_id": "s1", "version": 5 });
        let (path, body) = build_request_parts(&SESSION_REWIND, &args).unwrap();
        assert_eq!(path, "/api/sessions/s1/rewind?version=5");
        assert_eq!(body, None);
    }

    #[tokio::test]
    async fn test_session_rewind_negative_version_rejected() {
        let tool = PassthroughTool::new(&make_ws(), &make_ev(), &SESSION_REWIND);
        let args = serde_json::json!({ "session_id": "s1", "version": -1 });
        let err = tool.call(&args).await.unwrap_err();
        assert_eq!(err, "version must be non-negative");
    }

    #[test]
    fn test_sandbox_list_query() {
        let args = serde_json::json!({ "workspace_id": "ws1", "requester": "u1" });
        let (path, body) = build_request_parts(&SANDBOX_LIST, &args).unwrap();
        assert_eq!(path, "/api/workspaces/ws1/sandboxes?requester=u1");
        assert_eq!(body, None);
    }

    #[test]
    fn test_publish_list_no_args() {
        let (path, body) = build_request_parts(&PUBLISH_LIST, &Value::Object(Map::new())).unwrap();
        assert_eq!(path, "/api/publish/queue");
        assert_eq!(body, None);
    }

    #[test]
    fn test_prod_audit_optional_limit() {
        let (path, _) =
            build_request_parts(&PROD_AUDIT, &serde_json::json!({ "limit": 5 })).unwrap();
        assert_eq!(path, "/api/production/audit?limit=5");
        let (path, _) = build_request_parts(&PROD_AUDIT, &Value::Object(Map::new())).unwrap();
        assert_eq!(path, "/api/production/audit");
    }

    #[test]
    fn test_sandbox_start_strarr_filter() {
        let args = serde_json::json!({
            "workspace_id": "ws1",
            "rule_version_ids": [1, "v2", null],
            "test_dataset_id": 3,
            "started_by": "u1",
            "parent_version": 2
        });
        let (path, body) = build_request_parts(&SANDBOX_START, &args).unwrap();
        assert_eq!(path, "/api/workspaces/ws1/sandboxes");
        // filter_map(as_str)：非字符串元素静默丢弃（对齐原实现）
        assert_eq!(
            body,
            Some(serde_json::json!({
                "rule_version_ids": ["v2"],
                "test_dataset_id": 3,
                "started_by": "u1",
                "parent_version": 2
            }))
        );
        // optional 缺席 → 不含键
        let args = serde_json::json!({
            "workspace_id": "ws1",
            "rule_version_ids": ["v1"],
            "test_dataset_id": 3,
            "started_by": "u1"
        });
        let (_, body) = build_request_parts(&SANDBOX_START, &args).unwrap();
        assert_eq!(
            body,
            Some(serde_json::json!({
                "rule_version_ids": ["v1"],
                "test_dataset_id": 3,
                "started_by": "u1"
            }))
        );
    }

    #[test]
    fn test_publish_submit_body_shape() {
        let args = serde_json::json!({
            "workspace_id": "ws1",
            "rule_version_ids": ["v1", "v2"],
            "submitted_by": "u1",
            "role": "department_head"
        });
        let (path, body) = build_request_parts(&PUBLISH_SUBMIT, &args).unwrap();
        assert_eq!(path, "/api/publish/queue");
        // 仅声明参数入 body：kind/meta_rule_content 键永不出现（等价原实现 None 语义）
        assert_eq!(
            body,
            Some(serde_json::json!({
                "workspace_id": "ws1",
                "rule_version_ids": ["v1", "v2"],
                "submitted_by": "u1",
                "role": "department_head"
            }))
        );
    }

    #[test]
    fn test_rule_validate_body_and_accept() {
        let args = serde_json::json!({ "rules": "[{\"id\":1}]" });
        let (path, body) = build_request_parts(&RULE_VALIDATE, &args).unwrap();
        assert_eq!(path, "/api/rules/validate");
        assert_eq!(body, Some(serde_json::json!({ "rules": "[{\"id\":1}]" })));
    }

    #[test]
    fn test_bundle_import_require_object() {
        let bundle = serde_json::json!({ "dataset_id": "ds1", "entries": [] });
        let args = serde_json::json!({ "bundle": bundle });
        let (path, body) = build_request_parts(&BUNDLE_IMPORT, &args).unwrap();
        assert_eq!(path, "/api/bundles/import");
        assert_eq!(body, Some(serde_json::json!({ "bundle": bundle })));
        // 非 object → 缺参错误（原实现 is_object 前置校验）
        let args = serde_json::json!({ "bundle": "not-an-object" });
        let err = build_request_parts(&BUNDLE_IMPORT, &args).unwrap_err();
        assert_eq!(err, "missing required parameter: bundle");
    }

    #[test]
    fn test_rule_to_transform_accepts_any_json_type() {
        // require_object=false：原实现仅查存在性，字符串也透传
        let args = serde_json::json!({ "body": "any" });
        let (_, body) = build_request_parts(&RULE_TO_TRANSFORM, &args).unwrap();
        assert_eq!(body, Some(serde_json::json!({ "body": "any" })));
    }

    #[test]
    fn test_knowledge_search_urlencode() {
        let args = serde_json::json!({
            "dataset": "ds1",
            "q": "a b&c",
            "domain": "tax",
            "tags": "x,y"
        });
        let (path, body) = build_request_parts(&KNOWLEDGE_SEARCH, &args).unwrap();
        assert_eq!(
            path,
            "/api/knowledge/ds1/entries?q=a%20b%26c&domain=tax&tags=x%2Cy"
        );
        assert_eq!(body, None);
    }

    // =========================================================================
    // 全表循环不变式：required 缺失 / required 类型不符 → 缺参错误
    // =========================================================================

    #[test]
    fn test_all_required_missing_error_messages() {
        for b in ALL_TRANSPARENT_BINDINGS {
            for target in b.params.iter().filter(|p| p.required) {
                let mut m = Map::new();
                for p in b.params {
                    if p.required && p.name != target.name {
                        m.insert(p.name.to_string(), dummy_value(p));
                    }
                }
                let err = build_request_parts(b, &Value::Object(m)).unwrap_err();
                assert_eq!(
                    err,
                    format!("missing required parameter: {}", target.name),
                    "binding {}",
                    b.name
                );
            }
        }
    }

    #[test]
    fn test_all_required_wrong_type_treated_as_missing() {
        // Obj 且 require_object=false 的参数接受任意类型（原实现仅查存在性），排除
        for b in ALL_TRANSPARENT_BINDINGS {
            for target in b
                .params
                .iter()
                .filter(|p| p.required && !(p.kind == ParamKind::Obj && !p.require_object))
            {
                let mut m = Map::new();
                for p in b.params {
                    if p.required {
                        let v = if p.name == target.name {
                            wrong_typed_value(p)
                        } else {
                            dummy_value(p)
                        };
                        m.insert(p.name.to_string(), v);
                    }
                }
                let err = build_request_parts(b, &Value::Object(m)).unwrap_err();
                assert_eq!(
                    err,
                    format!("missing required parameter: {}", target.name),
                    "binding {}:{}",
                    b.name,
                    target.name
                );
            }
        }
    }
}
