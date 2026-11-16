// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! `codegraph` —— 代码知识图谱只读查询面(工作区符号图谱反馈回路)
//!
//! 经本机 codegraph-bridge HTTP 服务(缺省 `http://127.0.0.1:9150`,
//! 契约 v1.3)查询任意已初始化仓库的符号图谱:按符号搜索、任务上下文
//! 合成、调用方/被调方、影响面、节点详情、源码分组、文件清单、索引
//! 健康。与 [`super::build_check`] 同族定位:模型主动查询,替代裸文本
//! 自行翻仓。
//!
//! ## 契约要点
//!
//! - **只读服务族**:工具只暴露 9 个只读查询服务(桥契约全部
//!   `sensitive=false`);`codegraph_ensure_index`(写索引面)不入本工具
//!   ——未索引仓库由服务端/壳侧执行首次索引,本工具对该场景 fail-visible
//!   报错并如实转述桥错误。
//! - **服务枚举封闭**:模型按 `service` 键从枚举表选择,不可组 URL/命令;
//!   每服务超时按桥契约分档(10-30s),工具级预算受 manifest 超时档约束。
//! - **payload 直通**:`params` 对象原样并入 `{projectPath, ...}` 载荷,
//!   校验语义归桥契约单源;`projectPath` 缺省=工具装配工作区根。
//! - **fail-visible**:桥不可达/非 200/非 JSON 一律显式报错并指认端点
//!   (`EVO_CODEGRAPH_URL` 可覆盖),绝不静默空结果。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::io_handler::IoResult;

/// 工具名(LLM 契约面/manifest/注册三处同键)
pub const CODEGRAPH_TOOL: &str = "codegraph";
/// 桥端点缺省值(`EVO_CODEGRAPH_URL` 可覆盖)
pub const BRIDGE_BASE_URL_DEFAULT: &str = "http://127.0.0.1:9150";

/// 只读查询服务(键 → 桥契约超时分档;线上名=`codegraph_{key}`)
struct QueryService {
    key: &'static str,
    timeout_ms: u64,
}

const SERVICES: &[QueryService] = &[
    QueryService {
        key: "search",
        timeout_ms: 15_000,
    },
    QueryService {
        key: "context",
        timeout_ms: 30_000,
    },
    QueryService {
        key: "callers",
        timeout_ms: 15_000,
    },
    QueryService {
        key: "callees",
        timeout_ms: 15_000,
    },
    QueryService {
        key: "impact",
        timeout_ms: 15_000,
    },
    QueryService {
        key: "node",
        timeout_ms: 15_000,
    },
    QueryService {
        key: "explore",
        timeout_ms: 30_000,
    },
    QueryService {
        key: "files",
        timeout_ms: 15_000,
    },
    QueryService {
        key: "status",
        timeout_ms: 10_000,
    },
];

/// 桥调用传输缝(生产=HTTP;测试=打桩)
#[allow(clippy::double_must_use)] // async_trait 装箱 future 与 Result 双 must_use 形态
#[async_trait::async_trait]
pub(crate) trait CodeGraphTransport: Send + Sync {
    /// POST `{base}/services/codegraph_{service}`,成功=结果 JSON 直通,
    /// 失败=已含指认信息的错误串
    async fn call(
        &self,
        service: &str,
        payload: &Value,
        timeout: Duration,
    ) -> Result<Value, String>;
}

/// 生产传输:HTTP POST(桥契约 v1.3;鉴权未武装时零额外头)
struct HttpTransport {
    base_url: String,
    client: reqwest::Client,
}

impl HttpTransport {
    /// 端点解析:env `EVO_CODEGRAPH_URL` 优先(空串视同未设),缺省本机桥
    fn from_env() -> Self {
        let base_url = std::env::var("EVO_CODEGRAPH_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| BRIDGE_BASE_URL_DEFAULT.to_string());
        Self {
            base_url,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl CodeGraphTransport for HttpTransport {
    async fn call(
        &self,
        service: &str,
        payload: &Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let url = format!("{}/services/codegraph_{service}", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(payload)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| {
                format!(
                    "codegraph bridge unreachable at {} ({e}) — is the bridge service \
                     running? override the endpoint via EVO_CODEGRAPH_URL",
                    self.base_url
                )
            })?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("codegraph bridge returned a non-JSON body: {e}"))?;
        if !status.is_success() {
            let msg = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown bridge error");
            return Err(format!("codegraph bridge returned {status}: {msg}"));
        }
        Ok(body)
    }
}

/// `codegraph` 工具(工作区根=projectPath 缺省;传输缝注入)
pub struct CodeGraphTool {
    workdir: PathBuf,
    transport: Arc<dyn CodeGraphTransport>,
}

impl CodeGraphTool {
    /// 生产构造(workdir=查询缺省根,与 shell_exec 同基线)
    pub fn wired(workdir: &Path) -> Self {
        Self {
            workdir: workdir.to_path_buf(),
            transport: Arc::new(HttpTransport::from_env()),
        }
    }

    /// 测试缝(注入打桩传输;仅测试构建编译,生产路径走 [`wired`])
    #[cfg(test)]
    pub(crate) fn with_transport(workdir: &Path, transport: Arc<dyn CodeGraphTransport>) -> Self {
        Self {
            workdir: workdir.to_path_buf(),
            transport,
        }
    }
}

#[async_trait::async_trait]
impl crate::io_handlers::tool_handler::ToolFunction for CodeGraphTool {
    async fn call(&self, args: &Value) -> IoResult {
        let spec = match args.get("service") {
            Some(Value::String(s)) => SERVICES
                .iter()
                .find(|svc| svc.key == s.as_str())
                .ok_or_else(|| {
                    format!(
                        "codegraph: unknown service \"{s}\" (valid services: {})",
                        SERVICES
                            .iter()
                            .map(|svc| svc.key)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?,
            Some(_) => return Err("codegraph: service must be a string".to_string()),
            None => return Err("codegraph: missing required argument \"service\"".to_string()),
        };
        let project_path = match args.get("projectPath") {
            None | Some(Value::Null) => self.workdir.to_string_lossy().to_string(),
            Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
            Some(_) => return Err("codegraph: projectPath must be a non-empty string".to_string()),
        };
        let mut payload = serde_json::Map::new();
        payload.insert("projectPath".to_string(), Value::String(project_path));
        match args.get("params") {
            None | Some(Value::Null) => {}
            Some(Value::Object(m)) => {
                for (k, v) in m {
                    payload.insert(k.clone(), v.clone());
                }
            }
            Some(_) => return Err("codegraph: params must be an object".to_string()),
        }
        self.transport
            .call(
                spec.key,
                &Value::Object(payload),
                Duration::from_millis(spec.timeout_ms),
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_handlers::tool_handler::ToolFunction;
    use std::sync::Mutex;

    /// 捕获桩:记录服务名/载荷/超时,按队列回放结果
    struct StubTransport {
        calls: Mutex<Vec<(String, Value, u64)>>,
        results: Mutex<Vec<Result<Value, String>>>,
    }

    #[async_trait::async_trait]
    impl CodeGraphTransport for StubTransport {
        async fn call(
            &self,
            service: &str,
            payload: &Value,
            timeout: Duration,
        ) -> Result<Value, String> {
            self.calls.lock().unwrap().push((
                service.to_string(),
                payload.clone(),
                timeout.as_millis() as u64,
            ));
            self.results
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| Ok(serde_json::json!({"ok": true})))
        }
    }

    fn tool_with(results: Vec<Result<Value, String>>) -> (CodeGraphTool, Arc<StubTransport>) {
        let stub = Arc::new(StubTransport {
            calls: Mutex::new(Vec::new()),
            results: Mutex::new(results.into_iter().rev().collect()),
        });
        (
            CodeGraphTool::with_transport(Path::new("D:/ws/repo"), stub.clone()),
            stub,
        )
    }

    #[tokio::test]
    async fn missing_service_is_fail_visible() {
        let (tool, _stub) = tool_with(Vec::new());
        let err = tool.call(&Value::Null).await.unwrap_err();
        assert!(
            err.contains("missing required argument \"service\""),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn unknown_service_lists_valid_set() {
        let (tool, _stub) = tool_with(Vec::new());
        let err = tool
            .call(&serde_json::json!({"service": "reindex"}))
            .await
            .unwrap_err();
        assert!(err.contains("unknown service \"reindex\""), "got: {err}");
        assert!(
            err.contains("search, context, callers, callees, impact, node, explore, files, status"),
            "错误须列出只读服务全集: {err}"
        );
    }

    #[tokio::test]
    async fn project_path_defaults_to_workdir_and_params_merge() {
        let (tool, stub) = tool_with(Vec::new());
        tool.call(&serde_json::json!({
            "service": "search",
            "params": {"query": "loginUser", "limit": 5}
        }))
        .await
        .unwrap();
        let calls = stub.calls.lock().unwrap();
        let (service, payload, timeout) = &calls[0];
        assert_eq!(service, "search", "线上服务名=键(桥侧加 codegraph_ 前缀)");
        assert_eq!(
            payload.get("projectPath").and_then(|v| v.as_str()),
            Some("D:/ws/repo"),
            "projectPath 缺省=装配工作区根"
        );
        assert_eq!(
            payload.get("query").and_then(|v| v.as_str()),
            Some("loginUser")
        );
        assert_eq!(payload.get("limit").and_then(|v| v.as_i64()), Some(5));
        assert_eq!(*timeout, 15_000, "search 档=15s");
    }

    #[tokio::test]
    async fn explicit_project_path_overrides_default() {
        let (tool, stub) = tool_with(Vec::new());
        tool.call(&serde_json::json!({
            "service": "status",
            "projectPath": "D:/other/repo"
        }))
        .await
        .unwrap();
        let calls = stub.calls.lock().unwrap();
        assert_eq!(
            calls[0].1.get("projectPath").and_then(|v| v.as_str()),
            Some("D:/other/repo")
        );
        assert_eq!(calls[0].2, 10_000, "status 档=10s");
    }

    #[tokio::test]
    async fn non_object_params_is_rejected() {
        let (tool, _stub) = tool_with(Vec::new());
        let err = tool
            .call(&serde_json::json!({"service": "search", "params": "query"}))
            .await
            .unwrap_err();
        assert!(err.contains("params must be an object"), "got: {err}");
    }

    #[tokio::test]
    async fn bridge_error_passes_through_fail_visible() {
        // 桥侧业务错误(500+error 字段由传输层提取)须如实转述,不得空结果假绿
        let (tool, _stub) = tool_with(vec![Err(
            "codegraph bridge returned 500: index locked".to_string()
        )]);
        let err = tool
            .call(&serde_json::json!({"service": "search"}))
            .await
            .unwrap_err();
        assert!(err.contains("index locked"), "桥错误须如实转述: {err}");
    }

    #[tokio::test]
    async fn transport_error_surfaces_verbatim() {
        // 不可达类错误须携带端点指认与覆盖面提示(EVO_CODEGRAPH_URL)
        let (tool, _stub) = tool_with(vec![Err(
            "codegraph bridge unreachable at http://127.0.0.1:9150 (conn refused) — is \
             the bridge service running? override the endpoint via EVO_CODEGRAPH_URL"
                .to_string(),
        )]);
        let err = tool
            .call(&serde_json::json!({"service": "context"}))
            .await
            .unwrap_err();
        assert!(err.contains("bridge unreachable"), "got: {err}");
        assert!(
            err.contains("EVO_CODEGRAPH_URL"),
            "不可达错误须指认端点覆盖面: {err}"
        );
    }

    #[tokio::test]
    async fn http_transport_posts_to_service_route_and_extracts_errors() {
        // HttpTransport 契约:POST /services/codegraph_{service} 200=JSON 直通,
        // 非 200=提取 error 字段为 fail-visible 错误串
        use mockito::Matcher;
        let mut server = mockito::Server::new_async().await;
        let payload = serde_json::json!({"projectPath": "D:/ws"});
        let ok_mock = server
            .mock("POST", "/services/codegraph_search")
            .match_body(Matcher::Json(payload.clone()))
            .with_status(200)
            .with_body("{\"symbols\": []}")
            .create_async()
            .await;
        let transport = HttpTransport {
            base_url: server.url(),
            client: reqwest::Client::new(),
        };
        let out = transport
            .call("search", &payload, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            out.get("symbols").and_then(|v| v.as_array()).unwrap().len(),
            0
        );
        ok_mock.assert();

        let err_mock = server
            .mock("POST", "/services/codegraph_search")
            .with_status(500)
            .with_body("{\"error\": \"boom\"}")
            .create_async()
            .await;
        let err = transport
            .call("search", &payload, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.contains("500") && err.contains("boom"), "got: {err}");
        err_mock.assert();
    }
}
