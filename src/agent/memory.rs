// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Agent memory manager -- manages memory via evorule payload API
//!
//! # Namespace convention (used for accounting paths)
//! - shared memory: `__memory__.agent_{type}.shared.{key}`
//! - session memory: `__memory__.agent_{type}.session_{id}.{key}`
//! - short-term memory: `__memory__.agent_{type}.session_{id}.messages.{idx}`
//!
//! # Architecture change (P1-1):
//! Memory is no longer stored in local files, but written to the session payload
//! via evorule's `POST /api/sessions/{id}/payload` API, enabling cross-session
//! sharing and audit chain continuity.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::api::evorule_client::EvoruleApiClient;

/// 鍐呭瓨鎿嶄綔閿欒
#[derive(Debug)]
pub enum MemoryError {
    /// TODO: doc
    Io(std::io::Error),
    /// TODO: doc
    Json(serde_json::Error),
    /// TODO: doc
    EmptyKey,
    /// TODO: doc
    KeyTooLong(usize),
    /// TODO: doc
    EvoruleError(String),
    /// TODO: doc
    SessionNotSet,
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryError::Io(e) => write!(f, "IO error: {}", e),
            MemoryError::Json(e) => write!(f, "JSON error: {}", e),
            MemoryError::EmptyKey => write!(f, "memory key cannot be empty"),
            MemoryError::KeyTooLong(len) => write!(f, "memory key too long ({} chars)", len),
            MemoryError::EvoruleError(e) => write!(f, "Evorule API error: {}", e),
            MemoryError::SessionNotSet => write!(f, "session not set"),
        }
    }
}

impl std::error::Error for MemoryError {}

impl From<std::io::Error> for MemoryError {
    fn from(e: std::io::Error) -> Self {
        MemoryError::Io(e)
    }
}

impl From<serde_json::Error> for MemoryError {
    fn from(e: serde_json::Error) -> Self {
        MemoryError::Json(e)
    }
}

impl From<crate::api::evorule_client::EvoruleApiError> for MemoryError {
    fn from(e: crate::api::evorule_client::EvoruleApiError) -> Self {
        MemoryError::EvoruleError(e.to_string())
    }
}

/// 鍐呭瓨璁板綍
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// TODO: doc
    pub key: String,
    /// TODO: doc
    pub value: String,
    /// TODO: doc
    pub timestamp: u64,
}

/// 鍐呭瓨绠＄悊鍣紙閫氳繃 evorule payload API 瀹炵幇锛
#[derive(Clone)]
pub struct MemoryManager {
    namespace: String,
    evorule_client: EvoruleApiClient,
    session_id: Option<String>,
    cache: BTreeMap<String, MemoryRecord>,
}

impl MemoryManager {
    /// TODO: doc
    pub fn new(namespace: &str, evorule_client: EvoruleApiClient) -> Self {
        Self {
            namespace: namespace.to_string(),
            evorule_client,
            session_id: None,
            cache: BTreeMap::new(),
        }
    }

    /// TODO: doc
    pub fn with_session_id(mut self, session_id: &str) -> Self {
        self.session_id = Some(session_id.to_string());
        self
    }

    /// TODO: doc
    pub fn set_session_id(&mut self, session_id: &str) {
        self.session_id = Some(session_id.to_string());
    }

    /// TODO: doc
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    fn build_path(&self, key: &str) -> String {
        format!("__memory__.{}.{}", self.namespace, key)
    }

    /// TODO: doc
    pub async fn set(&mut self, key: &str, value: &str) -> Result<(), MemoryError> {
        if key.is_empty() {
            return Err(MemoryError::EmptyKey);
        }
        let max_key_len = 256;
        if key.len() > max_key_len {
            return Err(MemoryError::KeyTooLong(key.len()));
        }

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let record = MemoryRecord {
            key: key.to_string(),
            value: value.to_string(),
            timestamp,
        };
        self.cache.insert(key.to_string(), record.clone());

        if let Some(session_id) = &self.session_id {
            let path = self.build_path(key);
            let payload_value = serde_json::to_value(record)?;
            self.evorule_client.update_payload(session_id, &path, &payload_value).await?;
        }

        Ok(())
    }

    /// TODO: doc
    pub async fn get(&mut self, key: &str) -> Result<Option<MemoryRecord>, MemoryError> {
        if let Some(record) = self.cache.get(key) {
            return Ok(Some(record.clone()));
        }

        if let Some(session_id) = &self.session_id {
            let path = self.build_path(key);
            match self.evorule_client.get_facts(session_id, Some(&path)).await {
                Ok(facts) => {
                    for fact in facts {
                        if let Ok(record) = serde_json::from_value::<MemoryRecord>(fact.value) {
                            self.cache.insert(key.to_string(), record.clone());
                            return Ok(Some(record));
                        }
                    }
                }
                Err(_) => {}
            }
        }

        Ok(None)
    }

    /// TODO: doc
    pub async fn remove(&mut self, key: &str) -> Result<Option<MemoryRecord>, MemoryError> {
        let removed = self.cache.remove(key);

        if let Some(session_id) = &self.session_id {
            let path = self.build_path(key);
            let null_value = serde_json::json!(null);
            self.evorule_client.update_payload(session_id, &path, &null_value).await?;
        }

        Ok(removed)
    }

    /// TODO: doc
    pub async fn clear(&mut self) -> Result<(), MemoryError> {
        let keys: Vec<String> = self.cache.keys().cloned().collect();
        for key in keys {
            self.remove(&key).await?;
        }
        self.cache.clear();
        Ok(())
    }

    /// TODO: doc
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.cache.keys()
    }

    /// TODO: doc
    pub async fn sync_from_evorule(&mut self) -> Result<(), MemoryError> {
        if let Some(session_id) = &self.session_id {
            let prefix = format!("__memory__.{}", self.namespace);
            match self.evorule_client.get_facts(session_id, Some(&prefix)).await {
                Ok(facts) => {
                    for fact in facts {
                        if let Ok(record) = serde_json::from_value::<MemoryRecord>(fact.value) {
                            self.cache.insert(record.key.clone(), record);
                        }
                    }
                }
                Err(_) => {}
            }
        }
        Ok(())
    }

    /// TODO: doc
    pub fn build_system_prompt(&self, base_prompt: &str) -> String {
        if self.cache.is_empty() {
            return base_prompt.to_string();
        }

        let mut memory_lines = Vec::new();
        memory_lines.push("=== AGENT MEMORY ===".to_string());
        memory_lines.push(format!("Namespace: {}", self.namespace));
        memory_lines.push("".to_string());

        for key in self.cache.keys() {
            if let Some(record) = self.cache.get(key) {
                memory_lines.push(format!("{}: {}", record.key, record.value));
            }
        }
        memory_lines.push("".to_string());
        memory_lines.push("=== END MEMORY ===".to_string());

        format!("{}\n\n{}", base_prompt, memory_lines.join("\n"))
    }

    /// TODO: doc
    pub fn save_to_file(&self, path: &std::path::Path) -> Result<(), MemoryError> {
        let content = serde_json::to_string_pretty(&self.cache)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// TODO: doc
    pub fn load_from_file(path: &std::path::Path, namespace: &str, evorule_client: EvoruleApiClient) -> Result<Self, MemoryError> {
        let mut manager = Self::new(namespace, evorule_client);
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            manager.cache = serde_json::from_str(&content)?;
        }
        Ok(manager)
    }

    /// TODO: doc
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// TODO: doc
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

impl std::fmt::Debug for MemoryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryManager")
            .field("namespace", &self.namespace)
            .field("record_count", &self.cache.len())
            .field("session_id", &self.session_id)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_client() -> EvoruleApiClient {
        EvoruleApiClient::new("http://localhost:8080")
    }

    fn make_tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create tempdir")
    }

    #[test]
    fn test_memory_manager_new() {
        let mgr = MemoryManager::new("test", make_test_client());
        assert_eq!(mgr.namespace(), "test");
        assert!(mgr.is_empty());
        assert_eq!(mgr.len(), 0);
        assert!(mgr.session_id.is_none());
    }

    #[test]
    fn test_memory_manager_with_session_id() {
        let mgr = MemoryManager::new("test", make_test_client()).with_session_id("123");
        assert_eq!(mgr.session_id, Some("123".to_string()));
    }

    #[test]
    fn test_memory_manager_set_and_get_cached() {
        let mut mgr = MemoryManager::new("research", make_test_client());
        tokio_test::block_on(async {
            mgr.set("topic", "AI safety").await.expect("set");
            mgr.set("source", "arXiv").await.expect("set");

            assert!(!mgr.is_empty());
            assert_eq!(mgr.len(), 2);

            let record = mgr.get("topic").await.expect("get").expect("record");
            assert_eq!(record.key, "topic");
            assert_eq!(record.value, "AI safety");

            let record = mgr.get("source").await.expect("get").expect("record");
            assert_eq!(record.value, "arXiv");

            assert!(mgr.get("nonexistent").await.expect("get").is_none());
        });
    }

    #[test]
    fn test_memory_manager_set_empty_key() {
        let mut mgr = MemoryManager::new("test", make_test_client());
        let result = tokio_test::block_on(async { mgr.set("", "value").await });
        assert!(matches!(result, Err(MemoryError::EmptyKey)));
    }

    #[test]
    fn test_memory_manager_set_key_too_long() {
        let mut mgr = MemoryManager::new("test", make_test_client());
        let long_key = "a".repeat(500);
        let result = tokio_test::block_on(async { mgr.set(&long_key, "value").await });
        assert!(matches!(result, Err(MemoryError::KeyTooLong(500))));
    }

    #[test]
    fn test_memory_manager_remove() {
        let mut mgr = MemoryManager::new("test", make_test_client());
        tokio_test::block_on(async {
            mgr.set("key1", "val1").await.expect("set");
            mgr.set("key2", "val2").await.expect("set");

            let removed = mgr.remove("key1").await.expect("remove").expect("record");
            assert_eq!(removed.key, "key1");
            assert_eq!(mgr.len(), 1);

            assert!(mgr.remove("nonexistent").await.expect("remove").is_none());
        });
    }

    #[test]
    fn test_memory_manager_clear() {
        let mut mgr = MemoryManager::new("test", make_test_client());
        tokio_test::block_on(async {
            mgr.set("key1", "val1").await.expect("set");
            mgr.set("key2", "val2").await.expect("set");

            mgr.clear().await.expect("clear");
            assert!(mgr.is_empty());
            assert_eq!(mgr.len(), 0);
        });
    }

    #[test]
    fn test_memory_manager_keys() {
        let mut mgr = MemoryManager::new("test", make_test_client());
        tokio_test::block_on(async {
            mgr.set("zebra", "z").await.expect("set");
            mgr.set("alpha", "a").await.expect("set");
            mgr.set("beta", "b").await.expect("set");

            let mut keys: Vec<&String> = mgr.keys().collect();
            keys.sort();
            assert_eq!(keys.len(), 3);
            assert_eq!(keys, vec!["alpha", "beta", "zebra"]);
        });
    }

    #[test]
    fn test_memory_manager_build_system_prompt_empty() {
        let mgr = MemoryManager::new("test", make_test_client());
        let prompt = mgr.build_system_prompt("You are a helpful assistant");
        assert_eq!(prompt, "You are a helpful assistant");
    }

    #[test]
    fn test_memory_manager_build_system_prompt_with_memory() {
        let mut mgr = MemoryManager::new("research", make_test_client());
        tokio_test::block_on(async {
            mgr.set("topic", "quantum computing").await.expect("set");
            mgr.set("author", "John Doe").await.expect("set");

            let prompt = mgr.build_system_prompt("You are a research assistant");
            assert!(prompt.contains("=== AGENT MEMORY ==="));
            assert!(prompt.contains("Namespace: research"));
            assert!(prompt.contains("topic: quantum computing"));
            assert!(prompt.contains("author: John Doe"));
            assert!(prompt.contains("=== END MEMORY ==="));
            assert!(prompt.starts_with("You are a research assistant"));
        });
    }

    #[test]
    fn test_memory_manager_save_and_load() {
        let dir = make_tmp_dir();
        let path = dir.path().join("memory.json");

        let mut mgr1 = MemoryManager::new("test", make_test_client());
        tokio_test::block_on(async {
            mgr1.set("key1", "val1").await.expect("set");
            mgr1.set("key2", "val2").await.expect("set");
        });
        mgr1.save_to_file(&path).expect("save");

        let mut mgr2 = MemoryManager::load_from_file(&path, "test", make_test_client()).expect("load");
        assert_eq!(mgr2.namespace(), "test");
        assert_eq!(mgr2.len(), 2);
        tokio_test::block_on(async {
            assert_eq!(mgr2.get("key1").await.expect("get").unwrap().value, "val1");
            assert_eq!(mgr2.get("key2").await.expect("get").unwrap().value, "val2");
        });
    }

    #[test]
    fn test_memory_manager_load_nonexistent() {
        let mgr = MemoryManager::load_from_file(
            std::path::Path::new("/nonexistent/path/memory.json"),
            "test",
            make_test_client(),
        ).expect("load");
        assert_eq!(mgr.namespace(), "test");
        assert!(mgr.is_empty());
    }

    #[test]
    fn test_memory_manager_overwrite() {
        let mut mgr = MemoryManager::new("test", make_test_client());
        tokio_test::block_on(async {
            mgr.set("key", "v1").await.expect("set");
            mgr.set("key", "v2").await.expect("set");

            assert_eq!(mgr.len(), 1);
            assert_eq!(mgr.get("key").await.expect("get").unwrap().value, "v2");
        });
    }

    #[test]
    fn test_memory_error_display() {
        let err = MemoryError::EmptyKey;
        assert!(format!("{}", err).contains("cannot be empty"));

        let err = MemoryError::KeyTooLong(300);
        assert!(format!("{}", err).contains("300"));

        let err = MemoryError::EvoruleError("connection failed".to_string());
        assert!(format!("{}", err).contains("Evorule API error"));

        let err = MemoryError::SessionNotSet;
        assert!(format!("{}", err).contains("session not set"));
    }

    #[test]
    fn test_memory_manager_debug_format() {
        let mgr = MemoryManager::new("test", make_test_client());
        let debug = format!("{:?}", mgr);
        assert!(debug.contains("MemoryManager"));
        assert!(debug.contains("test"));
        assert!(debug.contains("record_count: 0"));
    }

    #[test]
    fn test_build_path() {
        let mgr = MemoryManager::new("agent_research", make_test_client());
        assert_eq!(mgr.build_path("topic"), "__memory__.agent_research.topic");
        assert_eq!(mgr.build_path("author"), "__memory__.agent_research.author");
    }
}
