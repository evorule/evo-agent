// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Evorule HTTP API 客户端 —— 封装 session/audit 相关 API 调用。
//! 持有共享 ApiCore（base_url + client + auth），与 WorkspaceApiClient 职责分离。

use crate::api::api_core::{ApiCore, ApiError};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::pin::Pin;

/// evorule server HTTP API 客户端（会话 / Fact / 审计 / bundle 部署）。
#[derive(Debug, Clone)]
pub struct EvoruleApiClient {
    /// 复用的 HTTP 核心客户端（共享 base_url 与认证头注入）。
    core: ApiCore,
}

/// 单会话元数据（GET /api/sessions/{id} 响应的客户端投影，只取会话链
/// 因果面所需字段；完整响应含 idle_secs/phase 等运行态字段，按需再扩）。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionMetadata {
    /// 会话 ID
    pub session_id: String,
    /// 父会话 ID（派生会话才有；None = 根会话）
    pub parent_session_id: Option<String>,
    /// 反应器是否已结束
    pub is_finished: bool,
}

impl EvoruleApiClient {
    /// Create new API client.
    /// If `EVORULE_AUTH_TOKEN` env var is set, use it as Bearer token.
    /// Otherwise, send no auth header (server must be in dev mode / no auth).
    pub fn new(base_url: &str) -> Self {
        Self::with_auth_token(base_url, None)
    }

    /// Create API client with an explicit auth token from the config file
    /// (`evorum.api_key` declaration). Priority: explicit declaration > env
    /// vars (`EVORULE_SERVICE_TOKEN` / `EVORULE_AUTH_TOKEN`) > no auth;
    /// an empty declaration falls back to env resolution.
    pub fn with_auth_token(base_url: &str, auth_token: Option<&str>) -> Self {
        Self {
            core: ApiCore::with_auth_token(base_url, auth_token),
        }
    }

    /// 返回 evorule server 的 base_url(供 serve 模式构造 WorkspaceApiClient 等)
    pub fn base_url(&self) -> &str {
        self.core.base_url()
    }

    /// 通用透传请求（rule_tools OpenAPI 适配器专用）：method + path（可含
    /// query）+ 可选 JSON body → 响应 JSON 原样返回。
    ///
    /// 错误口径 = check_response_full（非 2xx 时 server 错误详情透出，LLM 可自
    /// 诊断修复）；`accept_statuses` 中的非 2xx 状态视为成功、body 原样返回
    /// （如 validate 类端点的 422=校验未通过业务结果）。
    pub async fn passthrough_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        accept_statuses: &[u16],
    ) -> Result<Value, ApiError> {
        let url = self.core.url(path);
        let mut req = self
            .core
            .auth_header(self.core.client().request(method, &url));
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await?;
        let resp = self
            .core
            .check_response_full_accept(resp, accept_statuses)
            .await?;
        resp.json().await.map_err(|_| ApiError::InvalidResponse)
    }

    /// GET /api/io-contract —— evorule-server IO 形状契约拉取（E-8 协商，34 号档）。
    ///
    /// 错误分类给 [`crate::api::io_contract::negotiate_io_contract`] 消费：
    /// 404 → `NotFound`（旧 server，warn 通过）；其余传输/解析错误 →
    /// `Unavailable`（与 404 同判 warn 通过——部署期网络未起是常态）。
    pub async fn fetch_io_contract(
        &self,
    ) -> Result<serde_json::Value, crate::api::io_contract::NegotiationFetchError> {
        let url = self.core.url("/api/io-contract");
        let resp = match self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Err(crate::api::io_contract::NegotiationFetchError::Unavailable(
                    e.to_string(),
                ))
            }
        };
        if resp.status().as_u16() == 404 {
            return Err(crate::api::io_contract::NegotiationFetchError::NotFound);
        }
        match resp.json::<serde_json::Value>().await {
            Ok(v) => Ok(v),
            Err(e) => Err(crate::api::io_contract::NegotiationFetchError::Unavailable(
                e.to_string(),
            )),
        }
    }

    /// GET /api/rules/l2-inventory —— L2 约束（元规则）只读清单投影
    ///
    /// 返回 `{count, files:[{path,title,guard_for,promoted_from,promoted_at,
    /// promoted_by}]}`（服务端 fail-soft：任何扫描/解析失败均跳过，全失败返回
    /// 空清单）。供 meta_summary 工具与前馈注入共用；晋升条目
    /// （`00_constraint_promoted_*`）的 `promoted_*` 三字段即晋升账，
    /// `promoted_from` 形如 `rule_version:<版本id>`，与 workspace 规则版本链
    /// 对账（lineage_of 第二账数据源）。
    pub async fn get_l2_inventory(&self) -> Result<Value, ApiError> {
        let url = self.core.url("/api/rules/l2-inventory");
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        resp.json().await.map_err(|_| ApiError::InvalidResponse)
    }

    /// GET /api/sessions/{id}/evolution-signals —— 进化信号只读聚合（自进化）
    ///
    /// 返回 `{session_id, total_violations, signals:[{kind,rule_ref,reason_summary,
    /// count,last_version,last_instr_type}], queue:{pending_normal,pending_meta_promotion}}`
    /// （服务端 fail-soft：空会话/不可读 → 200 + 空信号）。供 evolution_signals
    /// 工具与前馈注入共用。`limit` 为 Some 时携带 `?limit=`（0 = 不限）。
    pub async fn get_evolution_signals(
        &self,
        session_id: u64,
        limit: Option<usize>,
    ) -> Result<Value, ApiError> {
        let mut url = self
            .core
            .url(&format!("/api/sessions/{session_id}/evolution-signals"));
        if let Some(n) = limit {
            url.push_str(&format!("?limit={n}"));
        }
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        resp.json().await.map_err(|_| ApiError::InvalidResponse)
    }

    /// GET /api/services —— 执行侧服务能力对账(服务消费契约的发现端)
    ///
    /// 返回 `[{name, source, version?, description?, plugin?, sensitive}, ...]`。
    pub async fn list_services(&self) -> Result<Value, ApiError> {
        let url = self.core.url("/api/services");
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        resp.json().await.map_err(|_| ApiError::InvalidResponse)
    }

    /// POST /api/services/{name}/invoke —— 插件服务直调(服务消费契约的调用端)
    ///
    /// body = 服务 args;响应 = 服务执行结果。服务侧错误以 HTTP 状态透传
    /// (401 未认证 / 403 敏感守卫 / 404 未知服务 / 502 执行失败),经 check_response fail-fast。
    pub async fn invoke_service(
        &self,
        service_name: &str,
        args: &Value,
    ) -> Result<Value, ApiError> {
        let url = self
            .core
            .url(&format!("/api/services/{service_name}/invoke"));
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(args)
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        resp.json().await.map_err(|_| ApiError::InvalidResponse)
    }

    /// 提议知识条目入账 Draft（治理语义封装，非裸 passthrough）
    ///
    /// 经服务编排层服务注册桥（`knowledge-propose`）→ 规则引擎服务端闸：
    /// `governance.llm_generated.flag` 由服务端强制构造（请求值不可生效）、
    /// `trust_level` 强制 `llm`（冒充显式拒绝）、`cause` 必填入溯源锚，
    /// 入账复用规则引擎既有全闸链，一律 Draft 落账（行权在生命周期迁移闸）。
    ///
    /// - `entry`：与规则引擎 knowledge 条目入账请求同构（entry_id/version/payload/
    ///   schema_ref/...；其内混入的 governance/trust_level 声明不采信）；
    /// - `source_session_id`：来源会话锚（提取候选所在会话，可审计回放），可缺省。
    ///
    /// 服务名固定白名单（不开放任意服务直调封装）；服务侧 4xx（冒充拒绝/契约闸）
    /// 经 `check_response` fail-fast 显式上抛。
    pub async fn propose_knowledge_entry(
        &self,
        dataset_id: &str,
        entry: &Value,
        cause: &str,
        source_session_id: Option<&str>,
    ) -> Result<Value, ApiError> {
        let args = serde_json::json!({
            "dataset_id": dataset_id,
            "entry": entry,
            "cause": cause,
            "source_session_id": source_session_id,
        });
        self.invoke_service("knowledge-propose", &args).await
    }

    /// 机器行权知识候选（治理语义封装，对称 `propose_knowledge_entry`）
    ///
    /// 经服务编排层服务注册桥（`knowledge-transition`）→ 规则引擎服务端机器闸：
    /// 现场六检执行 → 非全过 422 附 MachineGateReport 全文（fail-visible）；
    /// 全过按状态机合法路径逐跳放行至 Active（gate=machine+tier 审计留痕，
    /// T1 进人工追认队列——「机器行权，人工追认」）。行权上限=Active。
    ///
    /// - `entry_id`：候选条目 ID（propose 回执的 entry_id）；
    /// - `cause`：行权 cause（必填溯源锚）。
    pub async fn transition_knowledge_entry(
        &self,
        dataset_id: &str,
        entry_id: &str,
        cause: &str,
    ) -> Result<Value, ApiError> {
        let args = serde_json::json!({
            "dataset_id": dataset_id,
            "entry_id": entry_id,
            "to": "active",
            "cause": cause,
        });
        self.invoke_service("knowledge-transition", &args).await
    }

    /// 平台用户令牌校验 —— GET /api/platform/auth/me
    ///
    /// 用调用者提交的 Bearer 令牌直接请求认证端点，换取平台用户名。
    /// 注意：此处必须显式携带被校验的令牌本身，不可复用核心客户端的
    /// 服务级认证头（二者身份不同）。校验失败（网络 / 状态码 / 响应
    /// 形状）一律返回 `ApiError`，由调用方降级为未验证身份，不阻断审批。
    pub async fn verify_platform_token(&self, token: &str) -> Result<String, ApiError> {
        let url = self.core.url("/api/platform/auth/me");
        let resp = self
            .core
            .client()
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await.map_err(|_| ApiError::InvalidResponse)?;
        let username = result["user"]["username"]
            .as_str()
            .or_else(|| result["username"].as_str())
            .ok_or(ApiError::InvalidResponse)?;
        Ok(username.to_string())
    }

    /// 创建新会话，返回会话 ID。`initial_content` 为可选的初始 payload 内容。
    ///
    /// `caller_role` 为可选的主体声明（`Some("llm")` = LLM agent 会话；
    /// 服务端登记后命令入口注入 `__meta__.caller_role`，权限门按声明判定；
    /// None 不声明 → 服务端 fail-closed Unknown → Deny，与既有口径一致）。
    pub async fn create_session(
        &self,
        initial_content: Option<&Value>,
        caller_role: Option<&str>,
    ) -> Result<String, ApiError> {
        let url = self.core.url("/api/sessions");

        let mut body = serde_json::Map::new();
        if let Some(content) = initial_content {
            body.insert("initial_content".to_string(), content.clone());
        }
        if let Some(role) = caller_role {
            body.insert("caller_role".to_string(), serde_json::json!(role));
        }

        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        let session_id = result["session_id"]
            .as_u64()
            .ok_or(ApiError::InvalidResponse)?
            .to_string();

        Ok(session_id)
    }

    /// 从父会话 fork 出新会话，可选指定继承到的版本号。
    ///
    /// PR-H4 验收修复:改走 `POST /api/sessions/from/{parent_id}`——该端点
    /// version 可选,缺省=父会话最新快照(governance `facts_log.snapshot()`
    /// 口径,session_spawn 无版本 spawn 语义正需如此),且带声明继承与
    /// IoSubscriber 完整接线。原 `/api/sessions/fork/` 端点的 handler 强制
    /// version 必填(缺失即 400 空拒绝体),不适用无版本 spawn 调用。
    pub async fn create_session_fork(
        &self,
        parent_id: &str,
        version: Option<u64>,
    ) -> Result<String, ApiError> {
        let url = match version {
            Some(v) => format!(
                "{}/api/sessions/from/{}?version={}",
                self.core.base_url(),
                parent_id,
                v
            ),
            None => format!("{}/api/sessions/from/{}", self.core.base_url(), parent_id),
        };

        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        let session_id = result["session_id"]
            .as_u64()
            .ok_or(ApiError::InvalidResponse)?
            .to_string();

        Ok(session_id)
    }

    /// 读单会话元数据（GET /api/sessions/{id}）。
    ///
    /// 会话链因果面（自主交接设计）：`parent_session_id` 为 `None` = 根会话；
    /// 深度上溯链的权威在 server 侧（机制层 Session 持 parent_session_id 链）。
    pub async fn get_session_metadata(
        &self,
        session_id: &str,
    ) -> Result<SessionMetadata, ApiError> {
        let url = format!("{}/api/sessions/{}", self.core.base_url(), session_id);
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await.map_err(|_| ApiError::InvalidResponse)?;
        let sid = result["session_id"]
            .as_u64()
            .ok_or(ApiError::InvalidResponse)?
            .to_string();
        let parent = result["parent_session_id"].as_u64().map(|p| p.to_string());
        let is_finished = result["is_finished"].as_bool().unwrap_or(false);
        Ok(SessionMetadata {
            session_id: sid,
            parent_session_id: parent,
            is_finished,
        })
    }

    /// 向会话提交一条业务指令（POST command）。
    pub async fn submit_command(&self, session_id: &str, command: &Value) -> Result<(), ApiError> {
        let url = format!(
            "{}/api/sessions/{}/command",
            self.core.base_url(),
            session_id
        );

        let body = serde_json::json!({ "instruction": command });
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        Ok(())
    }

    /// 提交指令并同步等待结论（POST command?wait=true）。
    ///
    /// 响应 JSON 形态：`{success, fact_id, accepted: Option<bool>, violation?, code?}`——
    /// `accepted=true/false` = 结论已落链（受理/被 enforce 拦截）；
    /// `accepted` 缺失或 null = 等待窗口内结论未落定（WAIT_TIMEOUT 降级，
    /// 或旧版 server 无 wait 支持），调用方应回退既有轮询判据。
    pub async fn submit_command_wait(
        &self,
        session_id: &str,
        command: &Value,
    ) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/command?wait=true",
            self.core.base_url(),
            session_id
        );

        let body = serde_json::json!({ "instruction": command });
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    /// 回应会话的 IO 请求（POST io_response），`error` 非空表示 IO 执行失败。
    pub async fn submit_io_response(
        &self,
        session_id: &str,
        request_id: u64,
        result: &Value,
        error: Option<&str>,
    ) -> Result<(), ApiError> {
        let url = format!(
            "{}/api/sessions/{}/io_response",
            self.core.base_url(),
            session_id
        );

        let body = serde_json::json!({
            "request_id": request_id,
            "result": result,
            "error": error,
        });

        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        Ok(())
    }

    /// 按 path 更新会话 payload 中的指定值（挂账一闭环：返回 server 侧 fact_id）
    ///
    /// server `/api/sessions/{id}/payload` 成功响应携带 `fact_id`
    /// （ApiResponse.fact_id）——证据链上游锚点。旧版丢弃响应体恒返回 `()`。
    /// server 未返回 fact_id 时为 `None`（不视为错误，兼容旧版 server）。
    pub async fn update_payload(
        &self,
        session_id: &str,
        path: &str,
        value: &Value,
    ) -> Result<Option<u64>, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/payload",
            self.core.base_url(),
            session_id
        );

        let body = serde_json::json!({
            "path": path,
            "value": value,
        });

        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        Ok(body.get("fact_id").and_then(|f| f.as_u64()))
    }

    /// 批量更新会话 payload（跨仓挂账二随动：一次 HTTP 写入多条）
    ///
    /// server `/api/sessions/{id}/payloads`——预校验整批拒绝
    /// （空批次/受保护域身份不足/会话不存在）、执行期逐条上报。
    /// 返回逐条 fact_id（失败条为 None）。
    /// `success=false`（部分失败）按 [`ApiError`] 上浮，调用方由 B3 对账兜底。
    pub async fn update_payloads_batch(
        &self,
        session_id: &str,
        updates: &[(String, Value)],
    ) -> Result<Vec<Option<u64>>, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/payloads",
            self.core.base_url(),
            session_id
        );
        let items: Vec<serde_json::Value> = updates
            .iter()
            .map(|(path, value)| serde_json::json!({ "path": path, "value": value }))
            .collect();
        let body = serde_json::json!({ "updates": items });

        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        let success = body
            .get("success")
            .and_then(|s| s.as_bool())
            .unwrap_or(false);
        if !success {
            let message = body
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string();
            return Err(ApiError::ApiError {
                status: 200,
                message: format!("batch payload partial failure: {message}"),
            });
        }
        let fact_ids = body
            .get("results")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|r| r.get("fact_id").and_then(|f| f.as_u64()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(fact_ids)
    }

    /// 获取会话当前状态（GET state，返回完整 state JSON）。
    pub async fn get_state(&self, session_id: &str) -> Result<Value, ApiError> {
        let url = format!("{}/api/sessions/{}/state", self.core.base_url(), session_id);

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    /// 获取会话 Fact 列表，`prefix` 可选按 path 前缀过滤。
    pub async fn get_facts(
        &self,
        session_id: &str,
        prefix: Option<&str>,
    ) -> Result<Vec<FactEntry>, ApiError> {
        let url = if let Some(p) = prefix {
            format!(
                "{}/api/sessions/{}/facts?prefix={}",
                self.core.base_url(),
                session_id,
                p
            )
        } else {
            format!("{}/api/sessions/{}/facts", self.core.base_url(), session_id)
        };

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Vec<FactEntry> = resp.json().await?;
        Ok(result)
    }

    /// 获取跨会话共享 Fact 列表，`prefix` 可选按 path 前缀过滤。
    pub async fn get_shared_facts(
        &self,
        prefix: Option<&str>,
    ) -> Result<Vec<SharedFactEntry>, ApiError> {
        let url = if let Some(p) = prefix {
            format!("{}/api/shared/facts?prefix={}", self.core.base_url(), p)
        } else {
            format!("{}/api/shared/facts", self.core.base_url())
        };

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Vec<SharedFactEntry> = resp.json().await?;
        Ok(result)
    }

    /// 读共享事实版本号与历史长度（GET /api/shared/facts/version，并发核对用）。
    /// 返回 `{version, history_len}`。
    pub async fn get_shared_facts_version(&self) -> Result<Value, ApiError> {
        let url = format!("{}/api/shared/facts/version", self.core.base_url());
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        resp.json().await.map_err(|_| ApiError::InvalidResponse)
    }

    /// `POST /api/shared/facts/rollup` — 标记一批共享事实为已 rollup
    ///
    /// 用于 C1 sediment 的 C4 rollup：把被合并的旧会话摘要标记成 `rolled_up`，
    /// 使它们在 server 端 `facts_by_path_prefix` 查询中被过滤，避免下次仍计入
    /// 阈值、反复 rollup 导致共享空间膨胀（L-3 修复）。
    /// 标记后仍可通过 `fact_by_id` 访问，保留审计可追溯性。
    pub async fn mark_shared_facts_rollup(&self, fact_ids: &[u64]) -> Result<(), ApiError> {
        let url = format!("{}/api/shared/facts/rollup", self.core.base_url());
        let body = serde_json::json!({
            "fact_ids": fact_ids,
        });
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        // R11：不得走通用 check_response——它把 404 映射为
        // SessionNotFound（对 rollup 是误导语义），且错误 message 恒为
        // 空串（吞掉 server 的"未知 fact_id / 两 ID 空间不通用"诊断）。
        // 此处显式读取 ApiResponse 格式的 message 透出。
        let status = resp.status();
        if !status.is_success() {
            let status_u16 = status.as_u16();
            let err_body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
            let message = err_body
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string();
            return Err(ApiError::ApiError {
                status: status_u16,
                message,
            });
        }
        Ok(())
    }

    /// 查询共享 Fact 的来源会话信息。
    pub async fn get_shared_fact_source(&self, fact_id: u64) -> Result<SharedFactEntry, ApiError> {
        let url = format!(
            "{}/api/shared/facts/{}/source",
            self.core.base_url(),
            fact_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: SharedFactEntry = resp.json().await?;
        Ok(result)
    }

    /// 记录本会话启动时使用了哪些共享 Fact（回写 used_at_startup）。
    pub async fn record_used_at_startup(
        &self,
        session_id: &str,
        fact_ids: &[u64],
    ) -> Result<(), ApiError> {
        let url = format!(
            "{}/api/sessions/{}/used_at_startup",
            self.core.base_url(),
            session_id
        );

        let body = serde_json::json!({
            "fact_ids": fact_ids,
        });

        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        Ok(())
    }

    /// 查询本会话启动时使用过的共享 Fact ID 列表。
    pub async fn get_used_at_startup(&self, session_id: &str) -> Result<Vec<u64>, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/used_at_startup",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        let fact_ids: Vec<u64> = result["fact_ids"]
            .as_array()
            .ok_or(ApiError::InvalidResponse)?
            .iter()
            .filter_map(|v| v.as_u64())
            .collect();

        Ok(fact_ids)
    }

    /// 查询哪些会话使用过指定共享 Fact，返回会话 ID 列表。
    pub async fn get_sessions_using_fact(&self, fact_id: u64) -> Result<Vec<u64>, ApiError> {
        let url = format!(
            "{}/api/shared/facts/{}/used_by",
            self.core.base_url(),
            fact_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        let sessions: Vec<u64> = result["sessions"]
            .as_array()
            .ok_or(ApiError::InvalidResponse)?
            .iter()
            .filter_map(|v| v.as_u64())
            .collect();

        Ok(sessions)
    }

    /// 获取会话审计报告（GET audit，返回完整报告 JSON）。
    pub async fn get_audit_report(&self, session_id: &str) -> Result<Value, ApiError> {
        let url = format!("{}/api/sessions/{}/audit", self.core.base_url(), session_id);

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    /// 获取会话审计报告（含完整 Fact 内容，F5）。
    ///
    /// `GET /api/sessions/{id}/audit?include_content=true` —— 每条审计条目
    /// 附加 `content_json`（完整 Fact 内容，含 IoRequest params / IoResponse
    /// result）。查账 why 族（explain_denial）用：拒因解释需要被拒命令的
    /// 原始参数与 Violation 事实的 rule_index/reason/cause 同框。
    pub async fn get_audit_report_with_content(&self, session_id: &str) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/audit?include_content=true",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    /// 获取当前生效的 core_eval 规则正本（GET /api/rules，014 合法 API #2）。
    ///
    /// 返回 `{count, core_eval: [...], tiers}` —— `core_eval` 数组按引擎
    /// 求值序排列，`rule_index` 即该数组下标（Violation 事实归因锚）。
    /// 注意：core_eval 节点为引擎侧执行语义投影，不携带规则 id/metadata
    /// （晋升账改经 [`Self::get_l2_inventory`] 透出）。
    pub async fn get_rules(&self) -> Result<Value, ApiError> {
        let url = self.core.url("/api/rules");
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    /// GET /api/rules/hit-stats —— 规则命中统计清单（装备代谢数据源，39 号批 B3）
    ///
    /// 返回 `{ruleset_version, total_rules, generated_at_ms,
    /// entries:[{source,index,instr_type,hit_count,first_hit_seq,last_hit_seq,
    /// last_hit_at_ms}], zero_hits:[{source,index,instr_type}]}`。
    /// `filter`：None=all（默认，命中清单+零命中清单）| Some("hits")=仅命中 |
    /// Some("zero")=仅零命中（死规则候选）；非法值 → 400。`version` 缺省 =
    /// 当前版本，超出保留窗口 → 404。
    pub async fn get_hit_stats(
        &self,
        version: Option<&str>,
        filter: Option<&str>,
    ) -> Result<Value, ApiError> {
        let mut url = self.core.url("/api/rules/hit-stats");
        let mut params: Vec<String> = Vec::new();
        if let Some(v) = version {
            params.push(format!("version={}", urlencode(v)));
        }
        if let Some(f) = filter {
            params.push(format!("filter={}", urlencode(f)));
        }
        if !params.is_empty() {
            url.push('?');
            url.push_str(&params.join("&"));
        }
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        resp.json().await.map_err(|_| ApiError::InvalidResponse)
    }

    /// GET /api/rules/hit-stats/{rule_key} —— 单规则跨版本命中切片（39 号批 B3）
    ///
    /// `rule_key` 形如 `{index}@{source}`：`index` 为合并规则列表下标，`source`
    /// 为来源标签（宪法规则集为 `core_eval`，业务规则为 rules_dir 相对路径）。
    /// 本方法对 rule_key 整体百分号编码（`/` → `%2F`），调用方传原始形态即可。
    /// 返回 `{source,index,instr_type,hit_total,series:[{ruleset_version,
    /// hit_count,...}]}`（无统计的已知版本计 0）。
    pub async fn get_hit_stats_series(&self, rule_key: &str) -> Result<Value, ApiError> {
        let url = self
            .core
            .url(&format!("/api/rules/hit-stats/{}", urlencode(rule_key)));
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        resp.json().await.map_err(|_| ApiError::InvalidResponse)
    }

    /// **已废弃**：使用 `verify_audit_typed` 替代。此方法字段名已修正（valid→verified）。
    pub async fn verify_audit(&self, session_id: &str) -> Result<bool, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/audit/verify",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        let valid = result["verified"]
            .as_bool()
            .ok_or(ApiError::InvalidResponse)?;

        Ok(valid)
    }

    /// **已废弃**：使用 `get_causal_chain_typed` 替代。此方法已修正为按对象解析。
    pub async fn get_causal_chain(
        &self,
        session_id: &str,
        fact_id: u64,
    ) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/audit/causal/{}",
            self.core.base_url(),
            session_id,
            fact_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    /// B2/B4：类型化整链验证 —— GET /api/sessions/{id}/audit/verify → AuditVerify
    pub async fn verify_audit_typed(
        &self,
        session_id: &str,
    ) -> Result<crate::agent::memory_event::evidence::AuditVerify, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/audit/verify",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result = resp.json().await?;
        Ok(result)
    }

    /// B2/B4：类型化因果链 —— GET /api/sessions/{id}/audit/causal/{fact_id} → CausalChain
    pub async fn get_causal_chain_typed(
        &self,
        session_id: &str,
        fact_id: u64,
    ) -> Result<crate::agent::memory_event::evidence::CausalChain, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/audit/causal/{}",
            self.core.base_url(),
            session_id,
            fact_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result = resp.json().await?;
        Ok(result)
    }

    // =========================================================================
    // 审计链导出/导入（39 号批 B4）—— V-1/V-2 抗篡改线「接导出非造轮子」对接面。
    // 导出只读；导入破坏性（完全覆盖目标会话审计链），由调用方把守授权。
    // =========================================================================

    /// GET /api/sessions/{id}/audit/export —— 审计链导出（JSON 全文）
    ///
    /// 返回审计链 JSON（导出前 server 先审计新事实，含最新条目）。
    /// 404 = 会话不存在。
    pub async fn export_audit_chain(&self, session_id: &str) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/audit/export",
            self.core.base_url(),
            session_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        resp.json().await.map_err(|_| ApiError::InvalidResponse)
    }

    /// POST /api/sessions/{id}/audit/import —— 审计链导入（**破坏性**：覆盖现有审计链）
    ///
    /// `data` 为 [`Self::export_audit_chain`] 导出的审计链 JSON 原样。
    /// 返回 `{session_id, imported, verify_ok, status}`（status: "ok" |
    /// "verify_failed"——导入成功但链验证失败时如实上报，不视为传输错误）。
    /// 400 = 数据解析失败；404 = 会话不存在。
    pub async fn import_audit_chain(
        &self,
        session_id: &str,
        data: &Value,
    ) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/audit/import",
            self.core.base_url(),
            session_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(data)
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        resp.json().await.map_err(|_| ApiError::InvalidResponse)
    }

    /// GET /api/sessions/{id}/audit/export/compressed —— 审计链压缩导出（gzip 二进制）
    ///
    /// 返回 `application/gzip` 原始字节（体积通常为 JSON 的 5-10%），落盘/传输由
    /// 调用方处置。404 = 会话不存在；500 = server 压缩失败。
    pub async fn export_audit_chain_compressed(
        &self,
        session_id: &str,
    ) -> Result<bytes::Bytes, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/audit/export/compressed",
            self.core.base_url(),
            session_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        resp.bytes().await.map_err(|_| ApiError::InvalidResponse)
    }

    /// POST /api/sessions/{id}/audit/import/compressed —— 审计链压缩导入（**破坏性**）
    ///
    /// `gz` 为 gzip 二进制（`Content-Type: application/gzip`），解压后语义同
    /// [`Self::import_audit_chain`]，导入成功后 server 自动 verify。
    /// 返回 `{session_id, imported, verify_ok, status, format:"gzip"}`。
    pub async fn import_audit_chain_compressed(
        &self,
        session_id: &str,
        gz: &[u8],
    ) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/audit/import/compressed",
            self.core.base_url(),
            session_id
        );
        let resp = self
            .core
            .auth_header(
                self.core
                    .client()
                    .post(&url)
                    .header(reqwest::header::CONTENT_TYPE, "application/gzip"),
            )
            .body(gz.to_vec())
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        resp.json().await.map_err(|_| ApiError::InvalidResponse)
    }

    /// 回退会话到指定版本（GET rewind）。
    pub async fn rewind(&self, session_id: &str, version: u64) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/rewind?version={}",
            self.core.base_url(),
            session_id,
            version
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    /// 中断会话（POST interrupt，server 常挂载）——请求反应器在下一个
    /// checkpoint 停止。返回 `{session_id, success, message}`；404 = 会话不存在。
    pub async fn interrupt_session(&self, session_id: &str) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/interrupt",
            self.core.base_url(),
            session_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        resp.json().await.map_err(|_| ApiError::InvalidResponse)
    }

    /// 强制中止会话（POST abort，**破坏性**：直接中止反应器任务，不等 checkpoint）。
    ///
    /// 双保险：server 未开 `--allow-abort`/`EVORULE_ALLOW_ABORT=1` 时该端点
    /// 不挂载（404）——错误原样上抛，由调用方判定是否为开关未开。
    pub async fn abort_session(&self, session_id: &str) -> Result<Value, ApiError> {
        let url = format!("{}/api/sessions/{}/abort", self.core.base_url(), session_id);
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        resp.json().await.map_err(|_| ApiError::InvalidResponse)
    }

    /// 读会话结构不变式自检计数（GET invariants，语义不变量三性面健康声明）。
    /// 返回 `{session_id, structural_invariant_violations}`；404 = 会话不存在。
    pub async fn get_session_invariants(&self, session_id: &str) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/invariants",
            self.core.base_url(),
            session_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        resp.json().await.map_err(|_| ApiError::InvalidResponse)
    }

    /// 全量重放会话 Fact 流（GET replay，按版本序返回 Fact JSON 列表）。
    pub async fn replay(&self, session_id: &str) -> Result<Vec<Value>, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/replay",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Vec<Value> = resp.json().await?;
        Ok(result)
    }

    /// B3：版本区间 Fact 流重放（read_from 等价）
    /// GET /api/sessions/{id}/replay?from={from}&to={to}
    pub async fn replay_range(
        &self,
        session_id: &str,
        from: Option<u64>,
        to: Option<u64>,
    ) -> Result<Vec<FactLogEntry>, ApiError> {
        let mut url = format!(
            "{}/api/sessions/{}/replay",
            self.core.base_url(),
            session_id
        );
        let mut qs = Vec::new();
        if let Some(f) = from {
            qs.push(format!("from={}", f));
        }
        if let Some(t) = to {
            qs.push(format!("to={}", t));
        }
        if !qs.is_empty() {
            url.push('?');
            url.push_str(&qs.join("&"));
        }
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        Ok(resp.json().await?)
    }

    /// 对比会话两个版本的 payload 差异（GET diff）。
    pub async fn diff(&self, session_id: &str, a: u64, b: u64) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/diff?a={}&b={}",
            self.core.base_url(),
            session_id,
            a,
            b
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    /// 订阅会话 SSE 事件流，返回可逐事件拉取的流对象。
    pub async fn subscribe_events(&self, session_id: &str) -> Result<EvoruleEventStream, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/events",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.stream_client().get(&url))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(ApiError::ApiError {
                status: resp.status().as_u16(),
                message: resp.text().await.unwrap_or_default(),
            });
        }

        Ok(EvoruleEventStream {
            buffer: Vec::new(),
            stream: Box::pin(resp.bytes_stream()),
        })
    }

    /// 获取调试信息：当前执行阶段（GET debug/phase）。
    pub async fn get_debug_phase(&self, session_id: &str) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/debug/phase",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    /// 获取调试信息：指令队列（GET debug/queue）。
    pub async fn get_debug_queue(&self, session_id: &str) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/debug/queue",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    /// 获取调试信息：待回应的 IO 请求（GET debug/pending_io）。
    pub async fn get_debug_pending_io(&self, session_id: &str) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/sessions/{}/debug/pending_io",
            self.core.base_url(),
            session_id
        );

        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;

        let result: Value = resp.json().await?;
        Ok(result)
    }

    // =========================================================================
    // bundles 部署闭环（执行域 4 端点）
    // 全部走 check_response_full——校验失败(400)的 {"error": "..."} 详情透出，
    // LLM agent 可自诊断修复；404 带 body 不误判为会话不存在。
    // =========================================================================

    /// POST /api/bundles/import/dry-run —— 导入预检（校验链全跑，不落盘不 reload）
    ///
    /// `bundle` 为治理域导出的 DatasetBundle JSON 原样对象。
    /// 返回 `{valid, bundle_id, dataset_id, source_version, selection_mode,
    /// resolved_version, entry_count, verdict, missing_services}`。
    pub async fn bundle_import_dry_run(&self, bundle: &Value) -> Result<Value, ApiError> {
        let url = self.core.url("/api/bundles/import/dry-run");
        let body = serde_json::json!({ "bundle": bundle });
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        let resp = self.core.check_response_full(resp).await?;
        Ok(resp.json().await?)
    }

    /// POST /api/bundles/import —— 导入快照包并激活（破坏性：落盘 rules/bundles/ + reload）
    ///
    /// `bundle` 为治理域导出的 DatasetBundle JSON 原样对象。8 项校验任一失败
    /// → 400 显式错误（message 含校验失败详情）；成功 → 201 导入结果。
    pub async fn bundle_import(&self, bundle: &Value) -> Result<Value, ApiError> {
        let url = self.core.url("/api/bundles/import");
        let body = serde_json::json!({ "bundle": bundle });
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        let resp = self.core.check_response_full(resp).await?;
        Ok(resp.json().await?)
    }

    /// GET /api/bundles/active —— 当前激活 bundle 列表（rules/bundles/*/manifest 视图）
    pub async fn bundle_active_list(&self) -> Result<Value, ApiError> {
        let url = self.core.url("/api/bundles/active");
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        let resp = self.core.check_response_full(resp).await?;
        Ok(resp.json().await?)
    }

    /// GET /api/bundles/imports —— bundle 导入溯源记录（bundle_imports 表，只读审计）
    pub async fn bundle_imports_list(&self) -> Result<Value, ApiError> {
        let url = self.core.url("/api/bundles/imports");
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        let resp = self.core.check_response_full(resp).await?;
        Ok(resp.json().await?)
    }

    // =========================================================================
    // knowledge 执行侧数据面（3 端点，只读）
    // =========================================================================

    /// GET /api/knowledge —— 已承载数据资产的数据集清单
    pub async fn knowledge_datasets(&self) -> Result<Value, ApiError> {
        let url = self.core.url("/api/knowledge");
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        let resp = self.core.check_response_full(resp).await?;
        Ok(resp.json().await?)
    }

    /// GET /api/knowledge/{ds}/entries?q=&domain=&tags= —— 数据集条目检索
    ///
    /// - `q`：包含匹配（entry_id/schema_ref/bundle_id/payload）；
    /// - `domain`：领域精确匹配（忽略大小写）；
    /// - `tags`：逗号分隔标签（任一命中）。
    ///
    /// 数据集未承载 → 404 显式（区分"不存在"与"过滤后为空"）。
    pub async fn knowledge_entries(
        &self,
        dataset: &str,
        q: Option<&str>,
        domain: Option<&str>,
        tags: Option<&str>,
    ) -> Result<Value, ApiError> {
        let mut url = format!("{}/api/knowledge/{}/entries", self.core.base_url(), dataset);
        let mut params: Vec<String> = Vec::new();
        if let Some(v) = q {
            params.push(format!("q={}", urlencode(v)));
        }
        if let Some(v) = domain {
            params.push(format!("domain={}", urlencode(v)));
        }
        if let Some(v) = tags {
            params.push(format!("tags={}", urlencode(v)));
        }
        if !params.is_empty() {
            url.push('?');
            url.push_str(&params.join("&"));
        }
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        let resp = self.core.check_response_full(resp).await?;
        Ok(resp.json().await?)
    }

    /// GET /api/knowledge/{ds}/entries/{entry_id} —— 单条直取（payload 零转译原样）
    pub async fn knowledge_entry(&self, dataset: &str, entry_id: &str) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/knowledge/{}/entries/{}",
            self.core.base_url(),
            dataset,
            entry_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        let resp = self.core.check_response_full(resp).await?;
        Ok(resp.json().await?)
    }
}

/// 最小百分号编码（query 参数安全；仅编码保留字与空格，字母数字与常见符号直通）
pub(crate) fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// 会话 Fact 条目（GET facts 返回元素）。
#[derive(Debug, Deserialize, Clone)]
pub struct FactEntry {
    /// Fact 版本号。
    pub version: u64,
    /// server 返回 "fact_id"；映射到 id 保持现有引用不变（B1 契约修复）
    #[serde(rename = "fact_id", default)]
    pub id: u64,
    /// Fact 路径。
    pub path: String,
    /// Fact 值（任意 JSON）。
    pub value: Value,
    /// server 不返回 "type"；default 兜底避免反序列化失败（B1 契约修复）
    #[serde(rename = "type", default)]
    pub fact_type: String,
}

/// /replay 返回的单个 Fact（fact.to_json() + version）
#[derive(Debug, Clone, Deserialize)]
pub struct FactLogEntry {
    /// Fact 版本号。
    pub version: u64,
    /// Fact 类型（扁平捕获自原始 JSON）。
    #[serde(rename = "type", default)]
    pub fact_type: String,
    /// Fact ID（server 返回 "fact_id"，缺失时为 0）。
    #[serde(rename = "fact_id", default)]
    pub id: u64,
    /// 因果父 Fact 版本号。
    #[serde(default)]
    pub cause: Option<u64>,
    /// 变体字段扁平捕获（path/value/instruction/io_type/params/result/...）
    #[serde(flatten)]
    pub payload: Value,
}

impl FactLogEntry {
    /// 读取扁平捕获中的 path 字段。
    pub fn path(&self) -> Option<&str> {
        self.payload.get("path").and_then(|v| v.as_str())
    }
    /// 读取扁平捕获中的 value 字段。
    pub fn value(&self) -> Option<&Value> {
        self.payload.get("value")
    }
    /// 读取扁平捕获中的 instruction 字段。
    pub fn instruction(&self) -> Option<&Value> {
        self.payload.get("instruction")
    }
}

/// 跨会话共享 Fact 条目。
#[derive(Debug, Deserialize, Clone)]
pub struct SharedFactEntry {
    /// Fact ID。
    pub fact_id: u64,
    /// Fact 路径。
    pub path: String,
    /// Fact 值（任意 JSON）。
    pub value: Value,
    /// 来源会话 ID。
    pub source_session_id: u64,
    /// Fact 版本号。
    pub version: u64,
    /// 会话侧源头 fact_id（N6 链路统一，R10）。
    ///
    /// `None` = 对接旧 server（无此字段）或该条目先于 R10 写入。
    /// `Option` 缺省反序列化为 `None`，新旧 server 双兼容。
    #[serde(default)]
    pub origin_fact_id: Option<u64>,
}

/// 会话 SSE 事件流（按行缓冲解析 `data:` 帧）。
pub struct EvoruleEventStream {
    /// 未消费的字节缓冲。
    buffer: Vec<u8>,
    /// 底层字节流。
    stream: Pin<Box<dyn futures_core::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
}

/// SSE 事件流中的单个事件（type 字段 + 扁平捕获的其余字段）。
#[derive(Debug, Deserialize)]
pub struct EvoruleEvent {
    /// 事件类型（如 IoRequest / Stable）。
    #[serde(rename = "type")]
    pub event_type: String,
    /// 事件其余字段扁平捕获。
    #[serde(flatten)]
    pub payload: Value,
}
impl EvoruleEventStream {
    /// 拉取下一个事件；流结束或解析错误时返回 None。
    pub async fn next(&mut self) -> Option<EvoruleEvent> {
        let mut data = String::new();

        loop {
            loop {
                let (line_end, line_len) =
                    if let Some(pos) = self.buffer.windows(2).position(|w| w == b"\r\n") {
                        (pos, 2)
                    } else if let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
                        (pos, 1)
                    } else {
                        break;
                    };

                let line = String::from_utf8_lossy(&self.buffer[..line_end])
                    .trim_end_matches('\r')
                    .to_string();
                self.buffer.drain(..line_end + line_len);

                if line.starts_with("data: ") {
                    let json_part = line.strip_prefix("data: ").unwrap_or(&line);
                    data.push_str(json_part);
                } else if line.is_empty() && !data.is_empty() {
                    match serde_json::from_str::<EvoruleEvent>(&data) {
                        Ok(ev) => return Some(ev),
                        Err(e) => {
                            tracing::warn!("Failed to parse event: {}", e);
                            data.clear();
                        }
                    }
                }
            }

            match self.stream.next().await {
                Some(Ok(chunk)) => {
                    self.buffer.extend_from_slice(&chunk);
                }
                Some(Err(e)) => {
                    tracing::warn!("SSE stream error: {}", e);
                    return None;
                }
                None => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_client() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:8080")
    }

    #[test]
    fn test_client_new() {
        let client = make_test_client();
        assert_eq!(client.core.base_url(), "http://localhost:8080");
    }

    // ===== B1 FactEntry 契约修复测试 =====

    #[test]
    fn test_fact_entry_deserialize_server_format() {
        // server 返回 "fact_id"（非 "id"），不返回 "type"
        let json = r#"{"fact_id": 42, "version": 7, "path": "__memory__.x.session_1.k", "value": {"key":"k","value":"v","timestamp":1700000000}}"#;
        let fact: FactEntry = serde_json::from_str(json).unwrap();
        assert_eq!(fact.id, 42); // rename 生效：fact_id → id
        assert_eq!(fact.version, 7);
        assert_eq!(fact.path, "__memory__.x.session_1.k");
        assert_eq!(fact.fact_type, ""); // server 不返回 type，default 兜底
    }

    #[test]
    fn test_fact_entry_deserialize_with_type() {
        // 如果 server 返回了 type，也能正确解析
        let json =
            r#"{"fact_id": 1, "version": 1, "path": "test", "value": {}, "type": "PayloadUpdate"}"#;
        let fact: FactEntry = serde_json::from_str(json).unwrap();
        assert_eq!(fact.fact_type, "PayloadUpdate");
    }

    #[test]
    fn test_fact_entry_deserialize_missing_fact_id() {
        // fact_id 缺失时 default 兜底为 0
        let json = r#"{"version": 1, "path": "test", "value": {}}"#;
        let fact: FactEntry = serde_json::from_str(json).unwrap();
        assert_eq!(fact.id, 0);
    }

    // ===== B3 FactLogEntry 反序列化测试 =====

    #[test]
    fn test_fact_log_entry_deserialize() {
        // Command 变体
        let json = r#"{"version": 1, "type": "Command", "fact_id": 10, "cause": null, "instruction": {"type": "call_external", "params": {"goal": "hello"}}}"#;
        let entry: FactLogEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.version, 1);
        assert_eq!(entry.fact_type, "Command");
        assert_eq!(entry.id, 10);
        assert!(entry.cause.is_none());
        assert!(entry.instruction().is_some());
        assert_eq!(entry.instruction().unwrap()["params"]["goal"], "hello");

        // PayloadUpdate 变体
        let json = r#"{"version": 2, "type": "PayloadUpdate", "fact_id": 20, "path": "__memory__.x.session_1.k", "value": {"key": "k", "value": "v", "timestamp": 1700000000}}"#;
        let entry: FactLogEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.fact_type, "PayloadUpdate");
        assert_eq!(entry.id, 20);
        assert_eq!(entry.path(), Some("__memory__.x.session_1.k"));
        assert!(entry.value().is_some());
        assert_eq!(entry.value().unwrap()["key"], "k");

        // Stable 变体
        let json = r#"{"version": 3, "type": "Stable", "fact_id": 30, "final_snapshot": {"payload": "done"}}"#;
        let entry: FactLogEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.fact_type, "Stable");
        assert_eq!(entry.id, 30);
        assert_eq!(entry.payload["final_snapshot"]["payload"], "done");

        // cause 字段
        let json = r#"{"version": 4, "type": "IoRequest", "fact_id": 40, "cause": 10, "io_type": "call_external", "params": {"model": "test"}}"#;
        let entry: FactLogEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.fact_type, "IoRequest");
        assert_eq!(entry.cause, Some(10));
        assert_eq!(entry.payload["io_type"], "call_external");
        assert_eq!(entry.payload["params"]["model"], "test");
    }

    // ===== R10 SharedFactEntry origin_fact_id 双兼容 =====

    #[test]
    fn test_shared_fact_entry_origin_dual_compat() {
        // 新 server（R10+）：携带 origin_fact_id
        let json = r#"{"fact_id":30066,"path":"shared.test.topic","value":{"key":"topic"},"source_session_id":9,"version":1,"origin_fact_id":53}"#;
        let entry: SharedFactEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.origin_fact_id, Some(53));

        // 新 server：origin 为 null（旧签名写入/历史条目）
        let json = r#"{"fact_id":30066,"path":"shared.test.topic","value":{},"source_session_id":9,"version":1,"origin_fact_id":null}"#;
        let entry: SharedFactEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.origin_fact_id, None);

        // 旧 server（R10 前）：无该字段 → 缺省 None，不报错
        let json = r#"{"fact_id":30066,"path":"shared.test.topic","value":{},"source_session_id":9,"version":1}"#;
        let entry: SharedFactEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.origin_fact_id, None);
    }

    #[test]
    fn test_build_call_external_command() {
        let command = serde_json::json!({
            "type": "call_external",
            "params": {
                "model": "gpt-4o-mini",
                "temperature": 0.7,
                "system_prompt": "You are a helpful assistant",
                "goal": "Test task",
                "tool_names": ["search", "calculator"],
            }
        });

        assert_eq!(command["type"], "call_external");
        assert_eq!(command["params"]["model"], "gpt-4o-mini");
        assert_eq!(command["params"]["goal"], "Test task");
    }

    #[test]
    fn test_evorule_event_deserialize() {
        let json = r#"{"type":"IoRequest","id":2,"cause":1,"io_type":"call_external","params":{"model":"test"}}"#;
        let event: EvoruleEvent = serde_json::from_str(json).unwrap();

        assert_eq!(event.event_type, "IoRequest");
        assert_eq!(event.payload["io_type"], "call_external");
        assert_eq!(event.payload["id"], 2);
        assert_eq!(event.payload["params"]["model"], "test");
    }

    #[test]
    fn test_evorule_event_deserialize_stable() {
        let json = r#"{"type":"Stable","id":3,"final_snapshot":{"payload":"task completed"}}"#;
        let event: EvoruleEvent = serde_json::from_str(json).unwrap();

        assert_eq!(event.event_type, "Stable");
        assert_eq!(event.payload["id"], 3);
        assert_eq!(event.payload["final_snapshot"]["payload"], "task completed");
    }

    #[tokio::test]
    async fn test_fact_closed_loop_workflow() {
        let _client = make_test_client();

        let goal = "What is the weather today?";
        let system_prompt = "You are a weather assistant";

        let command = serde_json::json!({
            "type": "call_external",
            "params": {
                "model": "gpt-4o-mini",
                "temperature": 0.7,
                "system_prompt": system_prompt,
                "goal": goal,
                "tool_names": ["weather_api"],
            }
        });

        assert_eq!(command["type"], "call_external");
        assert_eq!(command["params"]["goal"], goal);
        assert_eq!(command["params"]["system_prompt"], system_prompt);

        let io_response = serde_json::json!({
            "content": "The weather is sunny today",
            "tool_calls": null,
            "is_finished": true,
        });

        assert_eq!(io_response["content"], "The weather is sunny today");
        assert!(io_response["is_finished"].as_bool().unwrap());
    }

    #[test]
    fn test_io_response_payload_structure() {
        let request_id: u64 = 123;
        let content = "Tool execution result";
        let used_facts = vec!["fact1".to_string(), "fact2".to_string()];

        let io_response_body = serde_json::json!({
            "request_id": request_id,
            "content": content,
            "used_facts": used_facts,
        });

        assert_eq!(io_response_body["request_id"], request_id);
        assert_eq!(io_response_body["content"], content);
        assert_eq!(io_response_body["used_facts"].as_array().unwrap().len(), 2);
    }

    // ===== 平台用户令牌校验 =====

    /// 校验成功:认证端点返回嵌套 user.username → 解析出用户名
    #[tokio::test]
    async fn test_verify_platform_token_success() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/platform/auth/me")
            .match_header("authorization", "Bearer user-token-1")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"success":true,"user":{"username":"alice","displayName":"Alice",
                    "email":"a@x.com","department":"sec","role":"user"},
                    "permissions":[],"permissions_version":1}"#,
            )
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        let name = client.verify_platform_token("user-token-1").await;
        assert_eq!(name.unwrap(), "alice");
    }

    /// 校验失败:非 2xx(如 401)→ 返回 Err,由调用方降级为未验证身份
    #[tokio::test]
    async fn test_verify_platform_token_rejected() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/platform/auth/me")
            .with_status(401)
            .with_body(r#"{"success":false}"#)
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        assert!(client.verify_platform_token("bad-token").await.is_err());
    }

    /// 校验失败:2xx 但响应缺少用户名 → InvalidResponse
    #[tokio::test]
    async fn test_verify_platform_token_missing_username() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/platform/auth/me")
            .with_status(200)
            .with_body(r#"{"success":true,"user":{}}"#)
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        assert!(matches!(
            client.verify_platform_token("t").await,
            Err(ApiError::InvalidResponse)
        ));
    }

    // ===== 治理写通路：propose_knowledge_entry 封装 =====

    /// 请求形状：服务名固定 knowledge-propose，body = {dataset_id, entry, cause,
    /// source_session_id}；响应透传（proposed 回执）
    #[tokio::test]
    async fn test_propose_knowledge_entry_request_shape() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/services/knowledge-propose/invoke")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "dataset_id": "ds-a23",
                "cause": "E2E：候选入账",
                "source_session_id": "sess-1"
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"status":"proposed","entry_id":"k-1","version":1,"lifecycle":"Draft"}"#)
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        let entry = serde_json::json!({
            "entry_id": "k-1", "version": 1,
            "payload": {"statement": "x"}, "schema_ref": "builtin:knowledge/fact"
        });
        let resp = client
            .propose_knowledge_entry("ds-a23", &entry, "E2E：候选入账", Some("sess-1"))
            .await
            .unwrap();
        assert_eq!(resp["status"], "proposed");
        assert_eq!(resp["lifecycle"], "Draft");
        mock.assert_async().await;
    }

    /// 请求形状：source_session_id 缺省时字段在 body 内显式 null（形状稳定，服务端 serde 可选）
    #[tokio::test]
    async fn test_propose_knowledge_entry_session_none_shape() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/services/knowledge-propose/invoke")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "dataset_id": "ds-a23",
                "cause": "c",
                "source_session_id": serde_json::Value::Null
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"status":"proposed"}"#)
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        let entry = serde_json::json!({"entry_id": "k-2", "version": 1,
            "payload": {}, "schema_ref": "builtin:knowledge/fact"});
        client
            .propose_knowledge_entry("ds-a23", &entry, "c", None)
            .await
            .unwrap();
        mock.assert_async().await;
    }

    /// 错误翻译：服务侧 4xx（冒充拒绝/契约闸）fail-fast 显式上抛，不静默
    #[tokio::test]
    async fn test_propose_knowledge_entry_error_fail_fast() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/services/knowledge-propose/invoke")
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(r#"{"error":{"code":"bad_request","message":"trust_level 冒充拒绝"}}"#)
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        let entry = serde_json::json!({"entry_id": "k-3", "version": 1,
            "payload": {}, "schema_ref": "builtin:knowledge/fact"});
        assert!(client
            .propose_knowledge_entry("ds-a23", &entry, "c", None)
            .await
            .is_err());
    }

    // ===== 治理写通路：transition_knowledge_entry 封装（A2-4） =====

    /// 请求形状：服务名固定 knowledge-transition，body = {dataset_id, entry_id,
    /// to:"active", cause}；响应透传（transitioned 回执含 tier/report）
    #[tokio::test]
    async fn test_transition_knowledge_entry_request_shape() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/services/knowledge-transition/invoke")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "dataset_id": "ds-a24",
                "entry_id": "k-1",
                "to": "active",
                "cause": "E2E：机器行权"
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"status":"transitioned","entry_id":"k-1","lifecycle":"Active","tier":"T0","post_review_required":false}"#,
            )
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        let resp = client
            .transition_knowledge_entry("ds-a24", "k-1", "E2E：机器行权")
            .await
            .unwrap();
        assert_eq!(resp["status"], "transitioned");
        assert_eq!(resp["lifecycle"], "Active");
        assert_eq!(resp["tier"], "T0");
        mock.assert_async().await;
    }

    /// 错误翻译：机器闸未放行 422（MachineGateReport 全文）fail-fast 显式上抛
    #[tokio::test]
    async fn test_transition_knowledge_entry_gate_fail_fail_fast() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/services/knowledge-transition/invoke")
            .with_status(422)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"error":{"code":"unprocessable_entity","message":"机器闸未放行（T2）: tier=T2 M1_schema=pass"}}"#,
            )
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        assert!(client
            .transition_knowledge_entry("ds-a24", "k-2", "c")
            .await
            .is_err());
    }

    // ===== B4：审计链导出/导入（39 号批）=====

    /// 请求形状：GET export → JSON 透传
    #[tokio::test]
    async fn test_export_audit_chain_shape() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/api/sessions/42/audit/export")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"session_id":42,"entries":[{"seq":1}],"verified":true}"#)
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        let chain = client.export_audit_chain("42").await.unwrap();
        assert_eq!(chain["session_id"], 42);
        assert_eq!(chain["verified"], true);
        mock.assert_async().await;
    }

    /// 请求形状：POST import → body=审计链 JSON 原样；verify_failed 如实透传（200）
    #[tokio::test]
    async fn test_import_audit_chain_shape() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/sessions/42/audit/import")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "entries": [{"seq": 1}]
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"session_id":42,"imported":true,"verify_ok":false,"status":"verify_failed"}"#,
            )
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        let data = serde_json::json!({"entries": [{"seq": 1}]});
        let resp = client.import_audit_chain("42", &data).await.unwrap();
        assert_eq!(resp["imported"], true);
        assert_eq!(resp["status"], "verify_failed");
        mock.assert_async().await;
    }

    /// 错误透出：导入数据非法 → 400 上抛
    #[tokio::test]
    async fn test_import_audit_chain_bad_request() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/sessions/42/audit/import")
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(r#"{"message":"bad request"}"#)
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        assert!(client
            .import_audit_chain("42", &serde_json::json!({}))
            .await
            .is_err());
    }

    /// 压缩导出：返回 gzip 二进制原样（非 JSON 通路）
    #[tokio::test]
    async fn test_export_audit_chain_compressed_bytes() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/api/sessions/42/audit/export/compressed")
            .with_status(200)
            .with_header("content-type", "application/gzip")
            .with_body(b"GZIP-FAKE-PAYLOAD")
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        let gz = client.export_audit_chain_compressed("42").await.unwrap();
        assert_eq!(&gz[..], b"GZIP-FAKE-PAYLOAD");
        mock.assert_async().await;
    }

    /// 压缩导入：body=gzip 二进制 + Content-Type: application/gzip；响应 JSON
    #[tokio::test]
    async fn test_import_audit_chain_compressed_shape() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/sessions/42/audit/import/compressed")
            .match_header("content-type", "application/gzip")
            .match_body(mockito::Matcher::Exact(
                "GZIP-FAKE-PAYLOAD".to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"session_id":42,"imported":true,"verify_ok":true,"status":"ok","format":"gzip"}"#,
            )
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        let resp = client
            .import_audit_chain_compressed("42", b"GZIP-FAKE-PAYLOAD")
            .await
            .unwrap();
        assert_eq!(resp["status"], "ok");
        assert_eq!(resp["format"], "gzip");
        mock.assert_async().await;
    }

    // ===== B3：规则命中统计（39 号批）=====

    /// 请求形状：query 带 version+filter（urlencode）；响应透传
    #[tokio::test]
    async fn test_get_hit_stats_request_shape() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/api/rules/hit-stats")
            .match_query("version=v-abc123&filter=zero")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"ruleset_version":"v-abc123","total_rules":3,"generated_at_ms":1700000000000,
                    "entries":[],"zero_hits":[{"source":"core_eval","index":2,"instr_type":"branch"}]}"#,
            )
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        let resp = client
            .get_hit_stats(Some("v-abc123"), Some("zero"))
            .await
            .unwrap();
        assert_eq!(resp["ruleset_version"], "v-abc123");
        assert_eq!(resp["zero_hits"][0]["index"], 2);
        mock.assert_async().await;
    }

    /// 缺省形状：version/filter 均为 None 时不带 query
    #[tokio::test]
    async fn test_get_hit_stats_default_no_query() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/api/rules/hit-stats")
            .match_query(mockito::Matcher::Missing)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"ruleset_version":"v1","total_rules":0,"entries":[],"zero_hits":[]}"#)
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        client.get_hit_stats(None, None).await.unwrap();
        mock.assert_async().await;
    }

    /// 错误透出：filter 非法 → 400 上抛（check_response 口径）
    #[tokio::test]
    async fn test_get_hit_stats_invalid_filter_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/rules/hit-stats")
            .match_query("filter=bogus")
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(r#"{"message":"invalid filter"}"#)
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        assert!(client.get_hit_stats(None, Some("bogus")).await.is_err());
    }

    /// 请求形状：rule_key 整体百分号编码（`/` → %2F）落到 path
    #[tokio::test]
    async fn test_get_hit_stats_series_url_encoded() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock(
                "GET",
                // 线上形态断言：@→%40、/→%2F（urlencode 全保留字编码口径）
                mockito::Matcher::Regex(
                    r"^/api/rules/hit-stats/2%40rules%2Fbundles%2Fexpenses\.json$".to_string(),
                ),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"source":"rules/bundles/expenses.json","index":2,"instr_type":"io_request",
                    "hit_total":7,"series":[{"ruleset_version":"v1","hit_count":7}]}"#,
            )
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        let resp = client
            .get_hit_stats_series("2@rules/bundles/expenses.json")
            .await
            .unwrap();
        assert_eq!(resp["hit_total"], 7);
        assert_eq!(resp["series"][0]["hit_count"], 7);
        mock.assert_async().await;
    }

    // ===== B1：会话即时终止面（39 号批）=====

    /// interrupt 请求形状：POST 无 body；响应 `{session_id,success,message}` 透传
    #[tokio::test]
    async fn test_interrupt_session_shape() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/sessions/42/interrupt")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"session_id":42,"success":true,"message":"Interrupt requested, reactor will respond at next checkpoint"}"#,
            )
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        let resp = client.interrupt_session("42").await.unwrap();
        assert_eq!(resp["session_id"], 42);
        assert_eq!(resp["success"], true);
        mock.assert_async().await;
    }

    /// abort 成功形状（server 开 `--allow-abort` 时可达）
    #[tokio::test]
    async fn test_abort_session_shape() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/api/sessions/42/abort")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"session_id":42,"success":true,"message":"aborted"}"#)
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        let resp = client.abort_session("42").await.unwrap();
        assert_eq!(resp["success"], true);
        mock.assert_async().await;
    }

    /// 双保险口径：server 未开 `--allow-abort` → 端点不挂载（404）原样上抛
    #[tokio::test]
    async fn test_abort_session_gated_404_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/api/sessions/42/abort")
            .with_status(404)
            .with_header("content-type", "application/json")
            .with_body(r#"{"message":"not found"}"#)
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        assert!(client.abort_session("42").await.is_err());
    }

    /// invariants 响应形状：`{session_id, structural_invariant_violations}` 透传
    #[tokio::test]
    async fn test_get_session_invariants_shape() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/api/sessions/42/invariants")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"session_id":42,"structural_invariant_violations":0}"#)
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        let resp = client.get_session_invariants("42").await.unwrap();
        assert_eq!(resp["structural_invariant_violations"], 0);
        mock.assert_async().await;
    }

    /// 404（会话不存在）按错误上抛——fail-soft 判定在消费挂点侧
    #[tokio::test]
    async fn test_get_session_invariants_404_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/sessions/42/invariants")
            .with_status(404)
            .with_header("content-type", "application/json")
            .with_body(r#"{"message":"not found"}"#)
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        assert!(client.get_session_invariants("42").await.is_err());
    }

    /// facts/version 响应形状：`{version, history_len}` 透传（39 号批 B5）
    #[tokio::test]
    async fn test_get_shared_facts_version_shape() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/api/shared/facts/version")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"version":7,"history_len":42}"#)
            .create_async()
            .await;

        let client = EvoruleApiClient::new(&server.url());
        let resp = client.get_shared_facts_version().await.unwrap();
        assert_eq!(resp["version"], 7);
        assert_eq!(resp["history_len"], 42);
        mock.assert_async().await;
    }
}
