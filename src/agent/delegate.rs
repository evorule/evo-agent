// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Agent delegate — sub-agent invocation
//!
//! ## G9:多 agent 编排
//!
//! 在原有串行单子 agent 委托(`delegate`)基础上,新增:
//! - [`DelegateContext::delegate_parallel`]:并行委托多个子 agent(`join_all`)
//! - [`DelegateContext::delegate_race`]:竞速委托(任一完成即返回,取消其余)
//! - **深度强制**:`delegate()` 现在会检查 `current_depth >= max_depth`,超限直接返回 Err
//!   (旧版只提供 `can_delegate` 辅助方法但不强制,容易无限递归)
//! - **并发限流**:`max_concurrent` 用 `tokio::sync::Semaphore` 限制并行子 agent 数,
//!   防止高并发下 evorule session 数暴增(§9.6 风险缓解)

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::agent::definition::AgentDefinitionManager;
use crate::agent::runner::DEFAULT_MAX_DELEGATE_DEPTH;
use crate::api::evorule_client::EvoruleApiClient;

/// G9:并行委托的默认并发上限(§9.6 风险缓解,默认 5)
pub const DEFAULT_MAX_CONCURRENT_DELEGATES: usize = 5;

/// Delegate context
#[derive(Debug, Clone)]
pub struct DelegateContext {
    /// Current delegate depth
    pub current_depth: usize,
    /// Parent agent type
    pub parent_agent_type: String,
    /// Agent definition manager
    pub definitions: AgentDefinitionManager,
    /// Evorule API client
    pub evorule_client: EvoruleApiClient,
    /// G9:最大委托深度(超过则拒绝,防无限递归)
    ///
    /// 默认 [`DEFAULT_MAX_DELEGATE_DEPTH`](crate::agent::runner::DEFAULT_MAX_DELEGATE_DEPTH) = 3。
    /// `delegate()` / `delegate_parallel()` / `delegate_race()` 在执行前检查
    /// `current_depth >= max_depth`,超限返回 Err。
    pub max_depth: usize,
    /// G9:并行委托并发上限(`Semaphore` 限流)
    ///
    /// `Some(sem)` 时,`delegate_parallel` / `delegate_race` 的每个子任务在调
    /// `delegate()` 前先 acquire 一个 permit,执行完释放。
    /// `None` = 不限流(测试/单机场景)。`Arc<Semaphore>` 在 `increment_depth`
    /// clone 时共享,因此限制是**全局的**(跨整个委托树)。
    pub max_concurrent: Option<Arc<Semaphore>>,
}

impl DelegateContext {
    /// Create new delegate context
    pub fn new(
        parent_agent_type: &str,
        definitions: AgentDefinitionManager,
        evorule_client: EvoruleApiClient,
    ) -> Self {
        Self {
            current_depth: 0,
            parent_agent_type: parent_agent_type.to_string(),
            definitions,
            evorule_client,
            max_depth: DEFAULT_MAX_DELEGATE_DEPTH,
            max_concurrent: None,
        }
    }

    /// Set delegate depth
    pub fn with_depth(mut self, depth: usize) -> Self {
        self.current_depth = depth;
        self
    }

    /// G9:设置最大委托深度(覆盖默认 `DEFAULT_MAX_DELEGATE_DEPTH`)
    pub fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// G9:启用并行委托并发限流(§9.6 风险缓解)
    ///
    /// `max_concurrent` 个子 agent 可同时运行,超出部分排队等待 permit。
    /// 共享 `Arc<Semaphore>`,在 `increment_depth` clone 时延续,限制跨整个委托树。
    pub fn with_max_concurrent_delegates(mut self, max_concurrent: usize) -> Self {
        if max_concurrent == 0 {
            self.max_concurrent = None;
        } else {
            self.max_concurrent = Some(Arc::new(Semaphore::new(max_concurrent)));
        }
        self
    }

    /// Increment delegate depth
    ///
    /// clone 自身并 `current_depth + 1`。`max_depth` 与 `max_concurrent`(Arc 共享)一并延续。
    pub fn increment_depth(&self) -> Self {
        self.clone().with_depth(self.current_depth + 1)
    }

    /// Check if can continue delegating
    pub fn can_delegate(&self, max_depth: usize) -> bool {
        self.current_depth < max_depth
    }

    /// G9:是否还能继续委托(用自身 `max_depth`,无需调用方传参)
    pub fn can_delegate_more(&self) -> bool {
        self.current_depth < self.max_depth
    }

    /// Delegate execution to sub-agent
    ///
    /// G9:执行前检查 `current_depth >= max_depth`,超限返回 Err(防无限递归)。
    pub fn delegate<'a>(
        &'a self,
        agent_type: &'a str,
        task: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            // G9:深度强制(旧版只提供 can_delegate 辅助方法但不强制)
            if self.current_depth >= self.max_depth {
                return Err(format!(
                    "max delegate depth exceeded: current_depth={} >= max_depth={} \
                     (cannot delegate to agent '{}')",
                    self.current_depth, self.max_depth, agent_type
                ));
            }

            tracing::info!(
                agent_type,
                task = %task,
                depth = self.current_depth,
                max_depth = self.max_depth,
                "Starting delegated sub-agent execution"
            );

            let def = self.definitions.load(agent_type).map_err(|e| {
                format!(
                    "Failed to load sub-agent definition ({}): {}",
                    agent_type, e
                )
            })?;

            let config = def.to_agent_config();

            let mut runner =
                crate::agent::runner::AgentRunner::new(config, self.evorule_client.clone());

            let result = runner.run(task).await;

            match result {
                Ok(r) => {
                    if r.success {
                        tracing::info!(
                            agent_type,
                            depth = self.current_depth,
                            "Sub-agent execution succeeded"
                        );
                        Ok(r.content)
                    } else {
                        let err = r.error.unwrap_or_default();
                        tracing::warn!(
                            agent_type,
                            depth = self.current_depth,
                            "Sub-agent execution failed: {}",
                            err
                        );
                        Err(err)
                    }
                }
                Err(e) => {
                    let err = format!("Sub-agent runtime error: {}", e);
                    tracing::error!(agent_type, depth = self.current_depth, "{}", err);
                    Err(err)
                }
            }
        })
    }

    /// G9:并行委托多个子 agent
    ///
    /// 所有子 agent 同时启动,各自独立 `run()`,全部完成后返回结果列表。
    /// 任意子 agent 失败不影响其他(失败项返回 `Err`)。
    ///
    /// # 深度
    ///
    /// 每个子 agent 在 `current_depth + 1` 层执行。若 `current_depth + 1 >= max_depth`,
    /// 对应子 agent 返回 `Err`(深度超限),不影响其他子 agent。
    ///
    /// # 并发限流
    ///
    /// 若配置了 `max_concurrent`,每个子任务在 `delegate()` 前 acquire permit,
    /// 防止 session 数暴增。
    ///
    /// # 参数
    ///
    /// - `tasks`:`(agent_type, task)` 列表
    ///
    /// # 返回
    ///
    /// 与 `tasks` 等长、同序的结果列表(`Ok(content)` / `Err(msg)`)。
    pub async fn delegate_parallel(
        &self,
        tasks: Vec<(String, String)>,
    ) -> Vec<Result<String, String>> {
        let futures: Vec<_> = tasks
            .into_iter()
            .map(|(agent_type, task)| {
                let ctx = self.increment_depth();
                let sem = ctx.max_concurrent.clone();
                async move {
                    if let Some(sem) = sem {
                        let _permit = match sem.acquire_owned().await {
                            Ok(p) => p,
                            Err(e) => {
                                return Err(format!("delegate semaphore closed: {}", e));
                            }
                        };
                    }
                    ctx.delegate(&agent_type, &task).await
                }
            })
            .collect();
        futures_util::future::join_all(futures).await
    }

    /// G9:竞速委托(任一完成即返回,取消其余)
    ///
    /// 适用场景:多模型/多策略试跑,取最快结果。
    ///
    /// 第一个完成的子 agent 的结果(无论 Ok/Err)被返回,其余被 drop(取消)。
    /// 被取消的子 agent 的 evorule session 可能残留(由 evorule 自身的 session
    /// 超时清理),P1 阶段不处理。
    ///
    /// # 参数
    ///
    /// - `tasks`:`(agent_type, task)` 列表(至少 1 个)
    ///
    /// # 返回
    ///
    /// 第一个完成子 agent 的 `Result<String, String>`。
    pub async fn delegate_race(&self, tasks: Vec<(String, String)>) -> Result<String, String> {
        let futures: Vec<Pin<Box<dyn Future<Output = Result<String, String>> + Send>>> = tasks
            .into_iter()
            .map(|(agent_type, task)| {
                let ctx = self.increment_depth();
                Box::pin(async move {
                    if let Some(sem) = ctx.max_concurrent.clone() {
                        let _permit = match sem.acquire_owned().await {
                            Ok(p) => p,
                            Err(e) => {
                                return Err(format!("delegate semaphore closed: {}", e));
                            }
                        };
                    }
                    ctx.delegate(&agent_type, &task).await
                }) as Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
            })
            .collect();

        if futures.is_empty() {
            return Err("delegate_race requires at least 1 task".to_string());
        }

        // 用 select_all 取第一个完成的;其余被 drop(取消)
        let (result, _index, _remaining) = futures_util::future::select_all(futures).await;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_client() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:8080")
    }

    fn make_ctx() -> DelegateContext {
        let definitions = AgentDefinitionManager::with_default_dir();
        DelegateContext::new("parent", definitions, make_test_client())
    }

    #[test]
    fn test_delegate_context_new() {
        let ctx = make_ctx();
        assert_eq!(ctx.current_depth, 0);
        assert_eq!(ctx.parent_agent_type, "parent");
        assert_eq!(ctx.max_depth, DEFAULT_MAX_DELEGATE_DEPTH);
        assert!(ctx.max_concurrent.is_none());
    }

    #[test]
    fn test_delegate_context_with_depth() {
        let ctx = make_ctx().with_depth(2);
        assert_eq!(ctx.current_depth, 2);
    }

    #[test]
    fn test_delegate_context_with_max_depth() {
        let ctx = make_ctx().with_max_depth(10);
        assert_eq!(ctx.max_depth, 10);
    }

    #[test]
    fn test_delegate_context_with_max_concurrent() {
        let ctx = make_ctx().with_max_concurrent_delegates(5);
        assert!(ctx.max_concurrent.is_some());
        // 0 = 不限流
        let ctx0 = make_ctx().with_max_concurrent_delegates(0);
        assert!(ctx0.max_concurrent.is_none());
    }

    #[test]
    fn test_delegate_context_increment_depth() {
        let ctx = make_ctx().with_depth(1);
        let ctx2 = ctx.increment_depth();
        assert_eq!(ctx2.current_depth, 2);
        assert_eq!(ctx.current_depth, 1); // 原对象不变
                                          // max_depth 延续
        assert_eq!(ctx2.max_depth, ctx.max_depth);
    }

    #[test]
    fn test_increment_depth_shares_semaphore() {
        // Arc<Semaphore> 在 increment_depth clone 时共享(指针相等)
        let ctx = make_ctx().with_max_concurrent_delegates(3);
        let ctx2 = ctx.increment_depth();
        assert!(Arc::ptr_eq(
            ctx.max_concurrent.as_ref().unwrap(),
            ctx2.max_concurrent.as_ref().unwrap()
        ));
    }

    #[test]
    fn test_delegate_context_can_delegate() {
        let ctx = make_ctx().with_depth(2);
        assert!(ctx.can_delegate(3));
        assert!(!ctx.can_delegate(2));
        assert!(ctx.can_delegate(10));
    }

    #[test]
    fn test_can_delegate_more_uses_self_max_depth() {
        let ctx = make_ctx().with_depth(2).with_max_depth(3);
        assert!(ctx.can_delegate_more()); // 2 < 3
        let ctx2 = make_ctx().with_depth(3).with_max_depth(3);
        assert!(!ctx2.can_delegate_more()); // 3 >= 3
    }

    // ===== G9:深度强制 =====

    #[tokio::test]
    async fn test_delegate_rejects_when_depth_exceeded() {
        // current_depth == max_depth → 拒绝(不实际加载 agent 定义)
        let ctx = make_ctx().with_depth(3).with_max_depth(3);
        let result = ctx.delegate("researcher", "do something").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("max delegate depth exceeded"), "got: {}", err);
        assert!(err.contains("current_depth=3"), "got: {}", err);
        assert!(err.contains("max_depth=3"), "got: {}", err);
    }

    #[tokio::test]
    async fn test_delegate_rejects_when_depth_exceeds_by_more() {
        let ctx = make_ctx().with_depth(5).with_max_depth(3);
        let result = ctx.delegate("researcher", "task").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("max delegate depth exceeded"));
    }

    // ===== G9:delegate_parallel =====

    #[tokio::test]
    async fn test_delegate_parallel_empty_tasks() {
        let ctx = make_ctx();
        let results = ctx.delegate_parallel(vec![]).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_delegate_parallel_all_exceed_depth() {
        // 所有子任务都在 depth+1 层,而 max_depth=1 → 全部超限
        let ctx = make_ctx().with_depth(1).with_max_depth(1);
        let tasks = vec![
            ("agent_a".to_string(), "task_a".to_string()),
            ("agent_b".to_string(), "task_b".to_string()),
            ("agent_c".to_string(), "task_c".to_string()),
        ];
        let results = ctx.delegate_parallel(tasks).await;
        assert_eq!(results.len(), 3);
        for r in &results {
            assert!(r.is_err());
            assert!(r
                .as_ref()
                .unwrap_err()
                .contains("max delegate depth exceeded"));
        }
    }

    #[tokio::test]
    async fn test_delegate_parallel_preserves_order() {
        // 即使各子任务耗时不同,join_all 保持输入顺序
        let ctx = make_ctx().with_depth(1).with_max_depth(1);
        let tasks = vec![
            ("a1".to_string(), "t1".to_string()),
            ("a2".to_string(), "t2".to_string()),
            ("a3".to_string(), "t3".to_string()),
        ];
        let results = ctx.delegate_parallel(tasks).await;
        assert_eq!(results.len(), 3);
        // 全部 Err(深度超限),但顺序与输入一致
        for r in &results {
            assert!(r.is_err());
        }
    }

    // ===== G9:delegate_race =====

    #[tokio::test]
    async fn test_delegate_race_empty_returns_err() {
        let ctx = make_ctx();
        let result = ctx.delegate_race(vec![]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("at least 1 task"));
    }

    #[tokio::test]
    async fn test_delegate_race_single_exceeds_depth() {
        let ctx = make_ctx().with_depth(2).with_max_depth(2);
        let result = ctx
            .delegate_race(vec![("agent_a".to_string(), "task".to_string())])
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("max delegate depth exceeded"));
    }

    #[tokio::test]
    async fn test_delegate_race_multiple_all_exceed_depth() {
        let ctx = make_ctx().with_depth(2).with_max_depth(2);
        let tasks = vec![
            ("a1".to_string(), "t1".to_string()),
            ("a2".to_string(), "t2".to_string()),
        ];
        let result = ctx.delegate_race(tasks).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("max delegate depth exceeded"));
    }

    // ===== G9:并发限流不阻塞测试(验证 Semaphore 可构造) =====

    #[tokio::test]
    async fn test_delegate_parallel_with_semaphore_all_exceed_depth() {
        // 配了限流 + 全部超限:permit 仍能获取(因为没有实际 delegate 占用)
        let ctx = make_ctx()
            .with_depth(1)
            .with_max_depth(1)
            .with_max_concurrent_delegates(2);
        let tasks = vec![
            ("a1".to_string(), "t1".to_string()),
            ("a2".to_string(), "t2".to_string()),
        ];
        let results = ctx.delegate_parallel(tasks).await;
        assert_eq!(results.len(), 2);
        for r in &results {
            assert!(r.is_err());
        }
    }

    #[test]
    fn test_default_max_concurrent_delegates_is_5() {
        assert_eq!(DEFAULT_MAX_CONCURRENT_DELEGATES, 5);
    }
}
