// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Agent 濮旀墭 鈥斺€?瀛?Agent 璋冪敤

use std::future::Future;
use std::pin::Pin;

use crate::agent::definition::AgentDefinitionManager;
use crate::api::evorule_client::EvoruleApiClient;

/// 濮旀墭涓婁笅鏂
#[derive(Debug, Clone)]
pub struct DelegateContext {
    /// 褰撳墠濮旀墭娣卞害
    pub current_depth: usize,
    /// 鐖?Agent 绫诲瀷
    pub parent_agent_type: String,
    /// Agent 瀹氫箟绠＄悊鍣
    pub definitions: AgentDefinitionManager,
    /// Evorule API 瀹㈡埛绔
    pub evorule_client: EvoruleApiClient,
}

impl DelegateContext {
    /// 鍒涘缓鏂扮殑濮旀墭涓婁笅鏂
    pub fn new(parent_agent_type: &str, definitions: AgentDefinitionManager, evorule_client: EvoruleApiClient) -> Self {
        Self {
            current_depth: 0,
            parent_agent_type: parent_agent_type.to_string(),
            definitions,
            evorule_client,
        }
    }

    /// 璁剧疆濮旀墭娣卞害
    pub fn with_depth(mut self, depth: usize) -> Self {
        self.current_depth = depth;
        self
    }

    /// 澧炲姞濮旀墭娣卞害
    pub fn increment_depth(&self) -> Self {
        self.clone().with_depth(self.current_depth + 1)
    }

    /// 鍒ゆ柇鏄惁鍙互缁х画濮旀墭
    pub fn can_delegate(&self, max_depth: usize) -> bool {
        self.current_depth < max_depth
    }

    /// 濮旀墭鎵ц瀛?Agent
    pub fn delegate<'a>(
        &'a self,
        agent_type: &'a str,
        task: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            tracing::info!(
                agent_type,
                task = %task,
                depth = self.current_depth,
                "寮€濮嬪鎵樻墽琛屽瓙 Agent"
            );

            let def = self
                .definitions
                .load(agent_type)
                .map_err(|e| format!("鍔犺浇瀛?Agent 瀹氫箟澶辫触 ({}): {}", agent_type, e))?;

            let config = def.to_agent_config();

            let mut runner = crate::agent::runner::AgentRunner::new(config, self.evorule_client.clone());

            let result = runner.run(task).await;

            match result {
                Ok(r) => {
                    if r.success {
                        tracing::info!(agent_type, depth = self.current_depth, "瀛?Agent 鎵ц鎴愬姛");
                        Ok(r.content)
                    } else {
                        let err = r.error.unwrap_or_default();
                        tracing::warn!(
                            agent_type,
                            depth = self.current_depth,
                            "瀛?Agent 鎵ц澶辫触: {}",
                            err
                        );
                        Err(err)
                    }
                }
                Err(e) => {
                    let err = format!("瀛?Agent 杩愯閿欒: {}", e);
                    tracing::error!(agent_type, depth = self.current_depth, "{}", err);
                    Err(err)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_client() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:8080")
    }

    #[test]
    fn test_delegate_context_new() {
        let definitions = AgentDefinitionManager::with_default_dir();
        let ctx = DelegateContext::new("parent", definitions, make_test_client());
        assert_eq!(ctx.current_depth, 0);
        assert_eq!(ctx.parent_agent_type, "parent");
    }

    #[test]
    fn test_delegate_context_with_depth() {
        let definitions = AgentDefinitionManager::with_default_dir();
        let ctx = DelegateContext::new("parent", definitions, make_test_client()).with_depth(2);
        assert_eq!(ctx.current_depth, 2);
    }

    #[test]
    fn test_delegate_context_increment_depth() {
        let definitions = AgentDefinitionManager::with_default_dir();
        let ctx = DelegateContext::new("parent", definitions, make_test_client()).with_depth(1);
        let ctx2 = ctx.increment_depth();
        assert_eq!(ctx2.current_depth, 2);
        assert_eq!(ctx.current_depth, 1);
    }

    #[test]
    fn test_delegate_context_can_delegate() {
        let definitions = AgentDefinitionManager::with_default_dir();
        let ctx = DelegateContext::new("parent", definitions, make_test_client()).with_depth(2);
        assert!(ctx.can_delegate(3));
        assert!(!ctx.can_delegate(2));
        assert!(ctx.can_delegate(10));
    }
}
