// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Evorule workspace API 客户端 —— 封装规则管理 + 转译 + 校验 + 热重载。
//!
//! 消费 evorule-server 的 `core/workspace` crate 暴露的 HTTP 端点。
//! 与 `EvoruleApiClient`（session/audit 相关）职责分离、独立模块；共享 `ApiCore` / `ApiError`。

use crate::api::api_core::{ApiCore, ApiError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// =============================================================================
// 客户端
// =============================================================================

/// 工作区治理域 API 客户端（工作区 / 规则 / 沙盒 / 发布全生命周期）。
#[derive(Debug, Clone)]
pub struct WorkspaceApiClient {
    /// 复用的 HTTP 核心客户端（共享 base_url 与认证头注入）。
    core: ApiCore,
}

impl WorkspaceApiClient {
    /// Create new workspace API client.
    /// Auth token read from `EVORULE_AUTH_TOKEN` env (same as EvoruleApiClient).
    pub fn new(base_url: &str) -> Self {
        Self {
            core: ApiCore::new(base_url),
        }
    }

    // ===== Workspace 管理 =====

    /// POST /api/workspaces — 创建 workspace
    pub async fn create_workspace(
        &self,
        req: CreateWorkspaceRequest,
    ) -> Result<WorkspaceRecord, ApiError> {
        let url = self.core.url("/api/workspaces");
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&req)
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: WorkspaceRecord = resp.json().await?;
        Ok(result)
    }

    /// GET /api/workspaces[?owner_id=xxx] — 列出所有 workspace（可按 owner 过滤）
    pub async fn list_workspaces(
        &self,
        owner_id: Option<&str>,
    ) -> Result<Vec<WorkspaceRecord>, ApiError> {
        let url = if let Some(oid) = owner_id {
            format!("{}/api/workspaces?owner_id={}", self.core.base_url(), oid)
        } else {
            self.core.url("/api/workspaces")
        };
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: Vec<WorkspaceRecord> = resp.json().await?;
        Ok(result)
    }

    // ===== 规则管理 =====

    /// POST /api/workspaces/{id}/rules — 创建规则
    pub async fn create_rule(
        &self,
        workspace_id: &str,
        req: CreateRuleRequest,
    ) -> Result<RuleRecord, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/rules",
            self.core.base_url(),
            workspace_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&req)
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: RuleRecord = resp.json().await?;
        Ok(result)
    }

    /// GET /api/workspaces/{id}/rules — 列出规则
    pub async fn list_rules(&self, workspace_id: &str) -> Result<Vec<RuleRecord>, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/rules",
            self.core.base_url(),
            workspace_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: Vec<RuleRecord> = resp.json().await?;
        Ok(result)
    }

    /// GET /api/workspaces/{id}/rules/{rule_id} — 获取规则详情
    pub async fn get_rule(
        &self,
        workspace_id: &str,
        rule_id: &str,
    ) -> Result<RuleRecord, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/rules/{}",
            self.core.base_url(),
            workspace_id,
            rule_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: RuleRecord = resp.json().await?;
        Ok(result)
    }

    /// PATCH /api/workspaces/{id}/rules/{rule_id} — 更新规则内容（仅 Draft 状态允许）
    pub async fn update_rule_content(
        &self,
        workspace_id: &str,
        rule_id: &str,
        req: UpdateRuleContentRequest,
    ) -> Result<RuleVersionRecord, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/rules/{}",
            self.core.base_url(),
            workspace_id,
            rule_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().patch(&url))
            .json(&req)
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: RuleVersionRecord = resp.json().await?;
        Ok(result)
    }

    /// POST /api/workspaces/{id}/rules/{rule_id}/activate — 激活规则
    pub async fn activate_rule(
        &self,
        workspace_id: &str,
        rule_id: &str,
    ) -> Result<RuleRecord, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/rules/{}/activate",
            self.core.base_url(),
            workspace_id,
            rule_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: RuleRecord = resp.json().await?;
        Ok(result)
    }

    /// POST /api/workspaces/{id}/rules/{rule_id}/submit — 提交候选（Draft → Candidate）
    pub async fn submit_rule(
        &self,
        workspace_id: &str,
        rule_id: &str,
    ) -> Result<RuleRecord, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/rules/{}/submit",
            self.core.base_url(),
            workspace_id,
            rule_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: RuleRecord = resp.json().await?;
        Ok(result)
    }

    /// POST /api/workspaces/{id}/rules/{rule_id}/block — 阻塞规则
    pub async fn block_rule(
        &self,
        workspace_id: &str,
        rule_id: &str,
    ) -> Result<RuleRecord, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/rules/{}/block",
            self.core.base_url(),
            workspace_id,
            rule_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: RuleRecord = resp.json().await?;
        Ok(result)
    }

    /// POST /api/workspaces/{id}/rules/{rule_id}/archive — 归档规则
    pub async fn archive_rule(
        &self,
        workspace_id: &str,
        rule_id: &str,
    ) -> Result<RuleRecord, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/rules/{}/archive",
            self.core.base_url(),
            workspace_id,
            rule_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: RuleRecord = resp.json().await?;
        Ok(result)
    }

    /// POST /api/workspaces/{id}/rules/{rule_id}/fork — fork 规则
    pub async fn fork_rule(
        &self,
        workspace_id: &str,
        rule_id: &str,
        req: ForkRuleRequest,
    ) -> Result<RuleRecord, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/rules/{}/fork",
            self.core.base_url(),
            workspace_id,
            rule_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&req)
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: RuleRecord = resp.json().await?;
        Ok(result)
    }

    /// GET /api/workspaces/{id}/rules/{rule_id}/versions — 列出版本（含 content，按 version 降序）
    pub async fn list_rule_versions(
        &self,
        workspace_id: &str,
        rule_id: &str,
    ) -> Result<Vec<RuleVersionRecord>, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/rules/{}/versions",
            self.core.base_url(),
            workspace_id,
            rule_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: Vec<RuleVersionRecord> = resp.json().await?;
        Ok(result)
    }

    /// GET /api/workspaces/{id}/rules/{rule_id}/versions/{version_id} — 获取版本
    pub async fn get_rule_version(
        &self,
        workspace_id: &str,
        rule_id: &str,
        version_id: &str,
    ) -> Result<RuleVersionRecord, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/rules/{}/versions/{}",
            self.core.base_url(),
            workspace_id,
            rule_id,
            version_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: RuleVersionRecord = resp.json().await?;
        Ok(result)
    }

    // ===== 规则转译 + 校验 + 热重载 =====

    /// POST /api/rules/translate/to_transform — 转译为 transform 格式（condition + action_set → transform）
    pub async fn translate_to_transform(&self, body: Value) -> Result<Value, ApiError> {
        let url = self.core.url("/api/rules/translate/to_transform");
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

    /// POST /api/rules/translate/to_conditional — 转译为可读视图（transform → condition + action_set，lossy）
    pub async fn translate_to_conditional(&self, body: Value) -> Result<Value, ApiError> {
        let url = self.core.url("/api/rules/translate/to_conditional");
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

    /// POST /api/rules/validate — 校验规则（G1-G7）
    ///
    /// body 需为 `{"rules": "..."}`（server.rs validate_rules_handler，rules 字段是 JSON 字符串）。
    /// 返回 200（passed=true）或 422（passed=false，含 errors 数组）。
    /// 422 时 server 返回非 2xx → check_response 报 ApiError，调用方需按需处理。
    pub async fn validate_rules(&self, body: Value) -> Result<Value, ApiError> {
        let url = self.core.url("/api/rules/validate");
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        // validate 的 422 是"校验未通过"而非 HTTP 错误，需要返回 body 给调用方判断
        if resp.status().as_u16() == 422 {
            let result: Value = resp.json().await?;
            return Ok(result);
        }
        self.core.check_response(&resp).await?;
        let result: Value = resp.json().await?;
        Ok(result)
    }

    /// POST /api/rules/reload — 热重载规则（从磁盘重新加载 TCB 宪法 + 业务规则）
    ///
    /// 请求体为空 `{}`。成功返回 `{"reload_ok":true,"previous_rules":N,"current_rules":M}`。
    pub async fn reload_rules(&self) -> Result<Value, ApiError> {
        let url = self.core.url("/api/rules/reload");
        let body = serde_json::json!({});
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

    // ===== 沙盒编排 (SANDBOX_ORCHESTRATION_DESIGN.md §6) =====

    /// POST /api/workspaces/{id}/sandboxes — 启动沙盒测试
    ///
    /// server 侧 StartSandboxHttpRequest = flatten(StartSandboxRequest) + started_by；
    /// 此处把 req 序列化后合并 started_by 字段，与 server flatten 契约对齐。
    pub async fn start_sandbox(
        &self,
        workspace_id: &str,
        req: StartSandboxRequest,
        started_by: &str,
    ) -> Result<StartSandboxResponse, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/sandboxes",
            self.core.base_url(),
            workspace_id
        );
        let mut body = serde_json::to_value(&req).unwrap_or_default();
        body["started_by"] = serde_json::Value::String(started_by.to_string());
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: StartSandboxResponse = resp.json().await?;
        Ok(result)
    }

    /// GET /api/workspaces/{id}/sandboxes?requester=xxx — 列出沙盒历史
    pub async fn list_sandboxes(
        &self,
        workspace_id: &str,
        requester: &str,
    ) -> Result<Vec<SandboxSession>, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/sandboxes?requester={}",
            self.core.base_url(),
            workspace_id,
            requester
        );
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: Vec<SandboxSession> = resp.json().await?;
        Ok(result)
    }

    /// GET /api/workspaces/{id}/sandboxes/{sandbox_id}?requester=xxx — 沙盒详情
    pub async fn get_sandbox(
        &self,
        workspace_id: &str,
        sandbox_id: i64,
        requester: &str,
    ) -> Result<SandboxSession, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/sandboxes/{}?requester={}",
            self.core.base_url(),
            workspace_id,
            sandbox_id,
            requester
        );
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: SandboxSession = resp.json().await?;
        Ok(result)
    }

    /// POST /api/workspaces/{id}/sandboxes/{sandbox_id}/close — 关闭沙盒
    pub async fn close_sandbox(
        &self,
        workspace_id: &str,
        sandbox_id: i64,
        closed_by: &str,
    ) -> Result<Value, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/sandboxes/{}/close",
            self.core.base_url(),
            workspace_id,
            sandbox_id
        );
        let body = serde_json::json!({ "closed_by": closed_by });
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

    /// GET /api/workspaces/{id}/sandboxes/{sandbox_id}/report — 测试报告
    pub async fn get_sandbox_report(
        &self,
        workspace_id: &str,
        sandbox_id: i64,
    ) -> Result<TestReport, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/sandboxes/{}/report",
            self.core.base_url(),
            workspace_id,
            sandbox_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: TestReport = resp.json().await?;
        Ok(result)
    }

    // ===== 测试数据集 (SANDBOX_ORCHESTRATION_DESIGN.md §3) =====

    /// POST /api/workspaces/{id}/test-datasets — 创建合成测试数据集
    pub async fn create_test_dataset(
        &self,
        workspace_id: &str,
        req: CreateTestDatasetRequest,
    ) -> Result<TestDatasetRecord, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/test-datasets",
            self.core.base_url(),
            workspace_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&req)
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: TestDatasetRecord = resp.json().await?;
        Ok(result)
    }

    /// GET /api/workspaces/{id}/test-datasets — 列出测试数据集
    pub async fn list_test_datasets(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<TestDatasetRecord>, ApiError> {
        let url = format!(
            "{}/api/workspaces/{}/test-datasets",
            self.core.base_url(),
            workspace_id
        );
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: Vec<TestDatasetRecord> = resp.json().await?;
        Ok(result)
    }

    // ===== 发布队列 (PUBLISH_QUEUE_DESIGN.md §6) =====

    /// POST /api/publish/queue — 提交到发布队列 (DepartmentHead 权限)
    ///
    /// server 侧 SubmitPublishHttpRequest = flatten(SubmitPublishRequest) + submitted_by + role。
    pub async fn submit_publish(
        &self,
        req: SubmitPublishRequest,
        submitted_by: &str,
        role: &str,
    ) -> Result<PublishQueueItem, ApiError> {
        let url = self.core.url("/api/publish/queue");
        let mut body = serde_json::to_value(&req).unwrap_or_default();
        body["submitted_by"] = serde_json::Value::String(submitted_by.to_string());
        body["role"] = serde_json::Value::String(role.to_string());
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: PublishQueueItem = resp.json().await?;
        Ok(result)
    }

    /// GET /api/publish/queue[?status=xxx] — 列出发布队列
    pub async fn list_publish_queue(
        &self,
        status: Option<&str>,
    ) -> Result<Vec<PublishQueueItem>, ApiError> {
        let url = match status {
            Some(s) => format!("{}/api/publish/queue?status={}", self.core.base_url(), s),
            None => self.core.url("/api/publish/queue"),
        };
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: Vec<PublishQueueItem> = resp.json().await?;
        Ok(result)
    }

    /// GET /api/publish/queue/{queue_id} — 队列项详情
    pub async fn get_publish_queue_item(
        &self,
        queue_id: i64,
    ) -> Result<PublishQueueItem, ApiError> {
        let url = format!("{}/api/publish/queue/{}", self.core.base_url(), queue_id);
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: PublishQueueItem = resp.json().await?;
        Ok(result)
    }

    /// POST /api/publish/queue/{queue_id}/review — 审批发布 (Admin 权限)
    ///
    /// server 侧 ReviewPublishHttpRequest = flatten(ReviewPublishRequest) + reviewed_by + role。
    pub async fn review_publish(
        &self,
        queue_id: i64,
        req: ReviewPublishRequest,
        reviewed_by: &str,
        role: &str,
    ) -> Result<PublishQueueItem, ApiError> {
        let url = format!(
            "{}/api/publish/queue/{}/review",
            self.core.base_url(),
            queue_id
        );
        let mut body = serde_json::to_value(&req).unwrap_or_default();
        body["reviewed_by"] = serde_json::Value::String(reviewed_by.to_string());
        body["role"] = serde_json::Value::String(role.to_string());
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: PublishQueueItem = resp.json().await?;
        Ok(result)
    }

    /// POST /api/publish/rollback — 紧急回滚 (Admin 权限)
    ///
    /// server 侧 RollbackHttpRequest = flatten(RollbackRequest) + operated_by + role。
    pub async fn emergency_rollback(
        &self,
        req: RollbackRequest,
        operated_by: &str,
        role: &str,
    ) -> Result<Value, ApiError> {
        let url = self.core.url("/api/publish/rollback");
        let mut body = serde_json::to_value(&req).unwrap_or_default();
        body["operated_by"] = serde_json::Value::String(operated_by.to_string());
        body["role"] = serde_json::Value::String(role.to_string());
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

    // ===== 生产状态 + 审计 (PUBLISH_QUEUE_DESIGN.md §6) =====

    /// GET /api/production/state — 查询当前生产状态
    pub async fn get_production_state(&self) -> Result<ProductionStateRecord, ApiError> {
        let url = self.core.url("/api/production/state");
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: ProductionStateRecord = resp.json().await?;
        Ok(result)
    }

    /// GET /api/production/audit[?limit=N] — 查询发布审计历史
    pub async fn list_production_audit(
        &self,
        limit: Option<i64>,
    ) -> Result<Vec<ProductionAuditRecord>, ApiError> {
        let url = match limit {
            Some(n) => format!("{}/api/production/audit?limit={}", self.core.base_url(), n),
            None => self.core.url("/api/production/audit"),
        };
        let resp = self
            .core
            .auth_header(self.core.client().get(&url))
            .send()
            .await?;
        self.core.check_response(&resp).await?;
        let result: Vec<ProductionAuditRecord> = resp.json().await?;
        Ok(result)
    }

    // ===== UV-084 W2：bundles 部署闭环（治理域导出，部署链上游） =====

    /// POST /bundles/export — 带真实闸门一证据的导出（T0 决策：POST 承载 tests 数组）
    ///
    /// 闭环链路：本方法导出 DatasetBundle → 执行域 `bundle_import_dry_run`
    /// 预检 → `bundle_import` 落盘激活。
    ///
    /// - `verdict="pass"` 时 `subset` 必须非空且每项以 `sandbox:<id>`（机器背书）
    ///   或 `human:<actor>`（人工降级）开头——治理域证据形状校验（UV-080 B1），
    ///   违反 → 400 显式错误；
    /// - `verdict="fail"` 为显式"未验证"导出（无伪造风险，无 subset 要求）；
    /// - `trim` 为可选裁剪视图语法（`tag:core` / `domain:tax` / `ids:id1,id2`，
    ///   多段以 `;` 分隔，交集）。
    ///
    /// 走 check_response_full：证据形状校验失败的修复指引透出。
    pub async fn export_bundle(
        &self,
        dataset_id: &str,
        version: &str,
        verdict: &str,
        subset: Vec<String>,
        trim: Option<&str>,
    ) -> Result<Value, ApiError> {
        let url = self.core.url("/bundles/export");
        let body = serde_json::json!({
            "dataset_id": dataset_id,
            "version": version,
            "tests": {
                "verdict": verdict,
                "subset": subset,
            },
            "subset": trim,
        });
        let resp = self
            .core
            .auth_header(self.core.client().post(&url))
            .json(&body)
            .send()
            .await?;
        let resp = self.core.check_response_full(resp).await?;
        Ok(resp.json().await?)
    }
}

// =============================================================================
// 请求/响应结构体（从 evorule-server core/workspace/src/models.rs 对齐）
// =============================================================================

/// 创建工作区请求（对齐 server models.rs CreateWorkspaceRequest）。
#[derive(Debug, Serialize)]
pub struct CreateWorkspaceRequest {
    /// 工作区名称。
    pub name: String,
    /// 所有者用户 ID。
    pub owner_id: String,
    /// 可选描述。
    #[serde(default)]
    pub description: Option<String>,
}

/// 工作区记录（对齐 server models.rs WorkspaceRecord）。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WorkspaceRecord {
    /// 工作区 ID。
    pub id: String,
    /// 工作区名称。
    pub name: String,
    /// 描述。
    pub description: Option<String>,
    /// 创建时间（RFC3339 字符串）。
    pub created_at: String,
    /// 所有者用户 ID。
    pub owner_id: String,
    /// server 侧是 WorkspaceState 枚举（active/archived，snake_case），非 `status`；client 用 String 兼容
    pub state: String,
    /// 最近更新时间。
    pub updated_at: String,
    /// 归档时间（未归档为 None）。
    pub archived_at: Option<String>,
}

/// 创建规则请求（对齐 server models.rs CreateRuleRequest）。
#[derive(Debug, Serialize)]
pub struct CreateRuleRequest {
    /// 规则名称。
    pub name: String,
    /// 规则内容（JSON 规则集文本）。
    pub content: String,
    /// 创建者用户 ID。
    pub created_by: String,
    /// 可选描述。
    #[serde(default)]
    pub description: Option<String>,
}

/// 规则记录（对齐 server models.rs RuleRecord）。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RuleRecord {
    /// 规则 ID。
    pub id: String,
    /// 所属工作区 ID。
    pub workspace_id: String,
    /// 规则名称。
    pub name: String,
    /// 当前版本 ID（尚无版本为 None）。
    pub current_version_id: Option<String>,
    /// server 侧是 RuleState 枚举（draft/candidate/active/blocked/archived，snake_case），client 用 String 兼容
    pub state: String,
    /// 描述。
    pub description: Option<String>,
    /// 创建者用户 ID。
    pub created_by: String,
    /// 创建时间。
    pub created_at: String,
    /// 最近更新时间。
    pub updated_at: String,
    /// 归档时间（未归档为 None）。
    pub archived_at: Option<String>,
    /// 扩展元数据（JSON 字符串，空时为 "{}"）；v3 schema 新增列
    pub metadata: String,
}

/// 更新规则内容请求（对齐 server models.rs UpdateRuleContentRequest）。
#[derive(Debug, Serialize)]
pub struct UpdateRuleContentRequest {
    /// 新的规则内容。
    pub content: String,
    /// 操作者用户 ID。
    pub updated_by: String,
}

/// 规则版本记录（对齐 server models.rs RuleVersionRecord）。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RuleVersionRecord {
    /// 版本记录 ID。
    pub id: String,
    /// 所属规则 ID。
    pub rule_id: String,
    /// 版本号（单调递增）。
    pub version: u64,
    /// 内容 SHA256 哈希。
    pub content_hash: String,
    /// 该版本的规则内容全文。
    pub content: String,
    /// server 侧是 RuleVersionState 枚举（current/superseded，snake_case），client 用 String 兼容
    pub state: String,
    /// 创建者用户 ID。
    pub created_by: String,
    /// 创建时间。
    pub created_at: String,
}

/// Fork 规则请求（对齐 server models.rs ForkRuleRequest）。
#[derive(Debug, Serialize)]
pub struct ForkRuleRequest {
    /// 新规则名称。
    pub new_name: String,
    /// 操作者用户 ID。
    pub created_by: String,
}

// =============================================================================
// 沙盒 + 测试数据集 + 发布队列 + 生产状态 DTO
// (对齐 evorule-server core/workspace/src/models.rs + test_report.rs)
// 时间字段用 String（与 D1 一致，server 侧 DateTime<Utc> 序列化为 RFC3339 字符串）。
// =============================================================================

/// 启动沙盒测试请求（对齐 server models.rs StartSandboxRequest）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartSandboxRequest {
    /// 参与本次沙盒测试的规则版本 ID 列表。
    pub rule_version_ids: Vec<String>,
    /// 用于验收的测试数据集 ID。
    pub test_dataset_id: i64,
    /// 父版本号（增量沙盒时指定，首测为 None）。
    #[serde(default)]
    pub parent_version: Option<u64>,
}

/// 启动沙盒测试响应（对齐 server models.rs StartSandboxResponse）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartSandboxResponse {
    /// 沙盒会话 ID。
    pub sandbox_id: i64,
    /// 引擎侧 TCB 会话 ID。
    pub tcb_session_id: u64,
    /// 草稿规则集哈希。
    pub draft_ruleset_hash: String,
    /// 测试用例总数。
    pub test_case_count: usize,
}

/// 沙盒会话记录（对齐 server models.rs SandboxSession；
/// status 为 SandboxStatus 枚举 snake_case 字符串，client 用 String 兼容）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSession {
    /// 沙盒会话 ID。
    pub id: i64,
    /// 所属工作区 ID。
    pub workspace_id: String,
    /// 引擎侧 TCB 会话 ID（未运行为 None）。
    pub tcb_session_id: Option<i64>,
    /// 父沙盒会话 ID（增量沙盒链）。
    pub parent_session_id: i64,
    /// 草稿规则集哈希。
    pub draft_ruleset_hash: Option<String>,
    /// 测试数据集 ID。
    pub test_dataset_id: i64,
    /// 沙盒状态（running/closed 等 snake_case 字符串）。
    pub status: String,
    /// 启动时间。
    pub started_at: String,
    /// 关闭时间（未关闭为 None）。
    pub closed_at: Option<String>,
    /// 启动者用户 ID。
    pub started_by: String,
    /// 审计导出文件路径。
    pub export_path: Option<String>,
}

/// 测试报告（对齐 server test_report.rs TestReport）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestReport {
    /// 沙盒会话 ID。
    pub sandbox_id: String,
    /// 所属工作区 ID。
    pub workspace_id: String,
    /// 引擎侧 TCB 会话 ID。
    pub tcb_session_id: u64,
    /// 父沙盒会话 ID。
    pub parent_session_id: Option<u64>,
    /// 草稿规则集哈希。
    pub draft_ruleset_hash: String,
    /// 测试统计摘要。
    pub summary: TestSummary,
    /// 逐用例结果。
    pub cases: Vec<TestCaseResult>,
    /// 测试异常列表。
    pub anomalies: Vec<TestAnomaly>,
    /// 审计链信息。
    pub audit_info: AuditInfo,
    /// 报告整体哈希（防篡改）。
    pub report_hash: String,
    /// 报告生成时间。
    pub generated_at: String,
}

/// 测试统计摘要
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestSummary {
    /// 用例总数。
    pub total_cases: usize,
    /// 通过数。
    pub passed: usize,
    /// 失败数。
    pub failed: usize,
    /// 跳过数。
    pub skipped: usize,
    /// 通过率（0.0-1.0）。
    pub pass_rate: f64,
    /// 总耗时（毫秒）。
    pub total_duration_ms: u64,
    /// 产生的引擎 Fact 总数。
    pub fact_count: usize,
}

/// 单个测试 case 结果（status 为 CaseStatus 枚举 snake_case 字符串，client 用 String 兼容）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCaseResult {
    /// 用例 ID。
    pub case_id: String,
    /// 用例名称。
    pub case_name: String,
    /// 执行状态（passed/failed/skipped 等）。
    pub status: String,
    /// 产生的 Fact ID（未产生为 None）。
    pub fact_id: Option<u64>,
    /// 失败时的错误信息。
    pub error_message: Option<String>,
    /// 该用例耗时（毫秒）。
    pub duration_ms: u64,
}

/// 测试异常
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestAnomaly {
    /// 异常类型标识。
    pub anomaly_type: String,
    /// 异常描述。
    pub description: String,
    /// 关联 Fact ID（无关联为 None）。
    pub fact_id: Option<u64>,
    /// 严重级别。
    pub severity: String,
}

/// 审计链信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditInfo {
    /// 审计链长度（Fact 节点数）。
    pub audit_chain_length: usize,
    /// 审计链哈希校验是否通过。
    pub audit_chain_verified: bool,
    /// 审计导出文件路径。
    pub audit_export_path: Option<String>,
}

/// 合成测试数据集记录（对齐 server models.rs TestDatasetRecord）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestDatasetRecord {
    /// 数据集 ID。
    pub id: i64,
    /// 数据集名称。
    pub name: String,
    /// 所属工作区 ID（全局共享数据集为 None）。
    pub workspace_id: Option<String>,
    /// 用例定义（JSON 字符串）。
    pub cases_json: String,
    /// 用例数量。
    pub case_count: i64,
    /// 创建时间。
    pub created_at: String,
    /// 创建者用户 ID。
    pub created_by: String,
    /// 描述。
    pub description: Option<String>,
}

/// 创建测试数据集请求（对齐 server models.rs CreateTestDatasetRequest）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTestDatasetRequest {
    /// 数据集名称。
    pub name: String,
    /// 用例定义（JSON 字符串）。
    pub cases_json: String,
    /// 创建者用户 ID。
    pub created_by: String,
    /// 所属工作区 ID（缺省为全局）。
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// 描述。
    #[serde(default)]
    pub description: Option<String>,
}

/// 发布队列记录（对齐 server models.rs PublishQueueItem；
/// status 为 PublishStatus 枚举 snake_case 字符串，client 用 String 兼容）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishQueueItem {
    /// 队列条目 ID。
    pub id: i64,
    /// 所属工作区 ID。
    pub workspace_id: String,
    /// 最终候选规则集（JSON 字符串）。
    pub final_candidate_rules: String,
    /// 规则集哈希。
    pub ruleset_hash: String,
    /// 验收依据的测试报告沙盒 ID。
    pub test_report_sandbox_id: Option<i64>,
    /// 提交者用户 ID。
    pub submitted_by: String,
    /// 提交时间。
    pub submitted_at: String,
    /// 审批者用户 ID（未审批为 None）。
    pub reviewed_by: Option<String>,
    /// 审批时间。
    pub reviewed_at: Option<String>,
    /// 审批意见。
    pub review_comment: Option<String>,
    /// 发布产生的生产版本号。
    pub published_version: Option<i64>,
    /// 发布时间。
    pub published_at: Option<String>,
    /// 队列状态（pending/approved/rejected/published 等）。
    pub status: String,
    /// 发布说明。
    pub description: Option<String>,
}

/// 提交发布请求（对齐 server models.rs SubmitPublishRequest）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitPublishRequest {
    /// 所属工作区 ID。
    pub workspace_id: String,
    /// 进入发布的规则版本 ID 列表。
    pub rule_version_ids: Vec<String>,
    /// 验收依据的测试报告沙盒 ID。
    #[serde(default)]
    pub test_report_sandbox_id: Option<i64>,
    /// 发布说明。
    #[serde(default)]
    pub description: Option<String>,
}

/// 审批发布请求（对齐 server models.rs ReviewPublishRequest）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewPublishRequest {
    /// approved / rejected
    pub decision: String,
    /// 审批意见。
    #[serde(default)]
    pub comment: Option<String>,
}

/// 紧急回滚请求（对齐 server models.rs RollbackRequest）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackRequest {
    /// 回滚目标生产版本号。
    pub target_version: i64,
    /// 回滚原因。
    pub reason: String,
}

/// 生产状态记录（对齐 server models.rs ProductionStateRecord，单行表）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductionStateRecord {
    /// 记录 ID。
    pub id: i64,
    /// 当前生产 TCB 会话 ID。
    pub current_session_id: Option<i64>,
    /// 当前规则集版本号。
    pub ruleset_version: i64,
    /// 当前规则集哈希。
    pub ruleset_hash: Option<String>,
    /// 最近操作者用户 ID。
    pub last_operated_by: Option<String>,
    /// 最近更新时间。
    pub updated_at: String,
}

/// 生产审计记录（对齐 server models.rs ProductionAuditRecord）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductionAuditRecord {
    /// 审计记录 ID。
    pub id: i64,
    /// 事件类型（publish/rollback 等）。
    pub event_type: String,
    /// 事件后的规则集版本号。
    pub ruleset_version: i64,
    /// 事件前的规则集版本号。
    pub previous_version: Option<i64>,
    /// 事件后的规则集哈希。
    pub ruleset_hash: String,
    /// 关联的 TCB 会话 ID。
    pub tcb_session_id: i64,
    /// 来源工作区 ID 列表（JSON 字符串）。
    pub source_workspace_ids: String,
    /// 操作者用户 ID。
    pub operated_by: String,
    /// 操作时间。
    pub operated_at: String,
    /// 操作原因。
    pub reason: Option<String>,
    /// 测试报告路径列表（JSON 字符串）。
    pub test_report_paths: Option<String>,
    /// 规则集快照全文。
    pub ruleset_snapshot: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_client(server_url: &str) -> WorkspaceApiClient {
        WorkspaceApiClient::new(server_url)
    }

    #[test]
    fn test_client_new() {
        let client = make_client("http://localhost:8080");
        assert_eq!(client.core.base_url(), "http://localhost:8080");
    }

    #[test]
    fn test_workspace_record_deserialize() {
        let json = r#"{
            "id": "01JTEST",
            "name": "test-ws",
            "description": "test workspace",
            "created_at": "2026-01-01T00:00:00Z",
            "owner_id": "user1",
            "state": "active",
            "updated_at": "2026-01-01T00:00:00Z",
            "archived_at": null
        }"#;
        let record: WorkspaceRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.id, "01JTEST");
        assert_eq!(record.state, "active");
        assert!(record.archived_at.is_none());
    }

    #[test]
    fn test_rule_record_deserialize() {
        let json = r#"{
            "id": "01JRULE",
            "workspace_id": "01JTEST",
            "name": "test-rule",
            "current_version_id": null,
            "state": "draft",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "archived_at": null,
            "description": "a test rule",
            "created_by": "user1",
            "metadata": "{}"
        }"#;
        let record: RuleRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.id, "01JRULE");
        assert_eq!(record.state, "draft");
        assert_eq!(record.metadata, "{}");
        assert!(record.archived_at.is_none());
    }

    #[test]
    fn test_rule_version_record_deserialize() {
        let json = r#"{
            "id": "01JVER",
            "rule_id": "01JRULE",
            "version": 1,
            "content_hash": "abc123",
            "content": "{\"transform\":[]}",
            "state": "current",
            "created_by": "user1",
            "created_at": "2026-01-01T00:00:00Z"
        }"#;
        let record: RuleVersionRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.version, 1);
        assert_eq!(record.state, "current");
        assert_eq!(record.content_hash, "abc123");
    }

    #[test]
    fn test_create_workspace_request_serialize() {
        let req = CreateWorkspaceRequest {
            name: "test".to_string(),
            owner_id: "user1".to_string(),
            description: Some("desc".to_string()),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["name"], "test");
        assert_eq!(json["owner_id"], "user1");
        assert_eq!(json["description"], "desc");
    }

    #[test]
    fn test_create_rule_request_serialize() {
        let req = CreateRuleRequest {
            name: "rule1".to_string(),
            content: r#"{"transform":[]}"#.to_string(),
            created_by: "user1".to_string(),
            description: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["name"], "rule1");
        assert_eq!(json["content"], r#"{"transform":[]}"#);
        assert!(json.get("description").is_some());
        assert!(json["description"].is_null());
    }

    #[test]
    fn test_fork_rule_request_serialize() {
        let req = ForkRuleRequest {
            new_name: "forked-rule".to_string(),
            created_by: "user1".to_string(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["new_name"], "forked-rule");
        assert_eq!(json["created_by"], "user1");
    }

    // ===== D4 沙盒/发布 DTO 反序列化 + 请求序列化测试 =====

    #[test]
    fn test_start_sandbox_request_serialize() {
        let req = StartSandboxRequest {
            rule_version_ids: vec!["01JVER".to_string()],
            test_dataset_id: 42,
            parent_version: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["rule_version_ids"][0], "01JVER");
        assert_eq!(json["test_dataset_id"], 42);
    }

    #[test]
    fn test_start_sandbox_response_deserialize() {
        let json = r#"{
            "sandbox_id": 1,
            "tcb_session_id": 500,
            "draft_ruleset_hash": "abc123",
            "test_case_count": 3
        }"#;
        let resp: StartSandboxResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.sandbox_id, 1);
        assert_eq!(resp.tcb_session_id, 500);
        assert_eq!(resp.draft_ruleset_hash, "abc123");
        assert_eq!(resp.test_case_count, 3);
    }

    #[test]
    fn test_sandbox_session_deserialize() {
        let json = r#"{
            "id": 1,
            "workspace_id": "01JTEST",
            "tcb_session_id": 500,
            "parent_session_id": 100,
            "draft_ruleset_hash": "hash1",
            "test_dataset_id": 7,
            "status": "running",
            "started_at": "2026-01-01T00:00:00Z",
            "closed_at": null,
            "started_by": "user1",
            "export_path": null
        }"#;
        let rec: SandboxSession = serde_json::from_str(json).unwrap();
        assert_eq!(rec.id, 1);
        assert_eq!(rec.status, "running");
        assert_eq!(rec.parent_session_id, 100);
        assert_eq!(rec.tcb_session_id, Some(500));
        assert!(rec.closed_at.is_none());
        assert!(rec.export_path.is_none());
    }

    #[test]
    fn test_test_report_deserialize() {
        let json = r#"{
            "sandbox_id": "1",
            "workspace_id": "01JTEST",
            "tcb_session_id": 500,
            "parent_session_id": 100,
            "draft_ruleset_hash": "abc",
            "summary": {
                "total_cases": 3, "passed": 2, "failed": 1, "skipped": 0,
                "pass_rate": 0.667, "total_duration_ms": 0, "fact_count": 3
            },
            "cases": [],
            "anomalies": [],
            "audit_info": {
                "audit_chain_length": 5, "audit_chain_verified": true,
                "audit_export_path": null
            },
            "report_hash": "deadbeef",
            "generated_at": "2026-01-01T00:00:00Z"
        }"#;
        let report: TestReport = serde_json::from_str(json).unwrap();
        assert_eq!(report.tcb_session_id, 500);
        assert_eq!(report.summary.passed, 2);
        assert_eq!(report.summary.failed, 1);
        assert!(report.audit_info.audit_chain_verified);
        assert_eq!(report.report_hash, "deadbeef");
    }

    #[test]
    fn test_test_dataset_record_deserialize() {
        let json = r#"{
            "id": 1,
            "name": "ds-1",
            "workspace_id": "01JTEST",
            "cases_json": "[{\"event\":\"test\"}]",
            "case_count": 1,
            "created_at": "2026-01-01T00:00:00Z",
            "created_by": "user1",
            "description": "a dataset"
        }"#;
        let rec: TestDatasetRecord = serde_json::from_str(json).unwrap();
        assert_eq!(rec.id, 1);
        assert_eq!(rec.case_count, 1);
        assert_eq!(rec.workspace_id.as_deref(), Some("01JTEST"));
    }

    #[test]
    fn test_publish_queue_item_deserialize() {
        let json = r#"{
            "id": 1,
            "workspace_id": "01JTEST",
            "final_candidate_rules": "[]",
            "ruleset_hash": "hash",
            "test_report_sandbox_id": 9,
            "submitted_by": "head1",
            "submitted_at": "2026-01-01T00:00:00Z",
            "reviewed_by": null,
            "reviewed_at": null,
            "review_comment": null,
            "published_version": null,
            "published_at": null,
            "status": "pending",
            "description": "release v1"
        }"#;
        let item: PublishQueueItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.id, 1);
        assert_eq!(item.status, "pending");
        assert_eq!(item.submitted_by, "head1");
        assert_eq!(item.test_report_sandbox_id, Some(9));
        assert!(item.reviewed_by.is_none());
        assert!(item.published_version.is_none());
    }

    #[test]
    fn test_production_state_record_deserialize() {
        let json = r#"{
            "id": 1,
            "current_session_id": 100,
            "ruleset_version": 3,
            "ruleset_hash": "hash",
            "last_operated_by": "admin1",
            "updated_at": "2026-01-01T00:00:00Z"
        }"#;
        let rec: ProductionStateRecord = serde_json::from_str(json).unwrap();
        assert_eq!(rec.id, 1);
        assert_eq!(rec.ruleset_version, 3);
        assert_eq!(rec.current_session_id, Some(100));
    }

    #[test]
    fn test_production_audit_record_deserialize() {
        let json = r#"{
            "id": 1,
            "event_type": "ruleset_published",
            "ruleset_version": 4,
            "previous_version": 3,
            "ruleset_hash": "hash",
            "tcb_session_id": 200,
            "source_workspace_ids": "[\"01JTEST\"]",
            "operated_by": "admin1",
            "operated_at": "2026-01-01T00:00:00Z",
            "reason": null,
            "test_report_paths": null,
            "ruleset_snapshot": null
        }"#;
        let rec: ProductionAuditRecord = serde_json::from_str(json).unwrap();
        assert_eq!(rec.id, 1);
        assert_eq!(rec.event_type, "ruleset_published");
        assert_eq!(rec.previous_version, Some(3));
        assert!(rec.reason.is_none());
    }

    #[test]
    fn test_submit_publish_request_serialize() {
        let req = SubmitPublishRequest {
            workspace_id: "01JTEST".to_string(),
            rule_version_ids: vec!["rv1".to_string(), "rv2".to_string()],
            test_report_sandbox_id: Some(5),
            description: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["workspace_id"], "01JTEST");
        assert_eq!(json["rule_version_ids"][1], "rv2");
        assert_eq!(json["test_report_sandbox_id"], 5);
        assert!(json["description"].is_null());
    }

    #[test]
    fn test_rollback_request_serialize() {
        let req = RollbackRequest {
            target_version: 2,
            reason: "bad release".to_string(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["target_version"], 2);
        assert_eq!(json["reason"], "bad release");
    }

    #[test]
    fn test_create_test_dataset_request_serialize() {
        let req = CreateTestDatasetRequest {
            name: "ds".to_string(),
            cases_json: "[]".to_string(),
            created_by: "user1".to_string(),
            workspace_id: None,
            description: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["name"], "ds");
        assert_eq!(json["cases_json"], "[]");
        assert_eq!(json["created_by"], "user1");
    }
}
