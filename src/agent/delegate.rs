// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Agent delegate - sub-agent invocation

use std::future::Future;
use std::pin::Pin;

use crate::agent::definition::AgentDefinitionManager;
use crate::api::evorule_client::EvoruleApiClient;

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
}

impl DelegateContext {
    /// Create new delegate context
    pub fn new(parent_agent_type: &str, definitions: AgentDefinitionManager, evorule_client: EvoruleApiClient) -> Self {
        Self {
            current_depth: 0,
            parent_agent_type: parent_agent_type.to_string(),
            definitions,
            evorule_client,
        }
    }

    /// Set delegate depth
    pub fn with_depth(mut self, depth: usize) -> Self {
        self.current_depth = depth;
        self
    }

    /// Increment delegate depth
    pub fn increment_depth(&self) -> Self {
        self.clone().with_depth(self.current_depth + 1)
    }

    /// Check if can continue delegating
    pub fn can_delegate(&self, max_depth: usize) -> bool {
        self.current_depth < max_depth
    }

    /// Delegate execution to sub-agent
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
                "Starting delegated sub-agent execution"
            );

            let def = self
                .definitions
                .load(agent_type)
                .map_err(|e| format!("Failed to load sub-agent definition ({}): {}", agent_type, e))?;

            let config = def.to_agent_config();

            let mut runner = crate::agent::runner::AgentRunner::new(config, self.evorule_client.clone());

            let result = runner.run(task).await;

            match result {
                Ok(r) => {
                    if r.success {
                        tracing::info!(agent_type, depth = self.current_depth, "Sub-agent execution succeeded");
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
