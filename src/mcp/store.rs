// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! B16 PR-2:MCP 配置存储(`<workdir>/.evo/mcp.toml`)
//!
//! ## 职责(配置面;C-1a 裁定:独立 mcp.toml)
//!
//! - **load**:`.evo/mcp.toml` 存在则优先(单一事实源),不存在回落
//!   `evo-agent.toml [mcp]` 段(启动时注入的 fallback 快照)——用户从
//!   evo-agent.toml 拷来的段落在工作台保存后即迁移为独立文件
//! - **save**:全量序列化 `[[mcp.servers]]` 段 + 原子写(tmp + rename),
//!   工作台 PUT 端点写回后即时生效(端点负责重载连接)
//!
//! ## 文件定位
//!
//! mcp.toml 是**程序管理文件**(工作台编辑器读写,无注释保护需求,
//! toml::to_string_pretty 全量写回);手改后经 PUT 端点或重启生效。

use crate::config::McpConfig;

/// mcp.toml 文件形态包装:输出/解析 `[mcp]` 段(与 evo-agent.toml [mcp]
/// 段完全同构,用户可把该段整段拷入 mcp.toml 零改动生效)
#[derive(serde::Serialize, serde::Deserialize)]
struct McpTomlFile {
    #[serde(default)]
    mcp: McpConfig,
}

/// MCP 配置存储(mcp.toml 读写 + evo-agent.toml 回落)
#[derive(Debug)]
pub struct McpConfigStore {
    /// 配置文件路径(`<workdir>/.evo/mcp.toml`)
    path: std::path::PathBuf,
    /// 回落配置(启动时 evo-agent.toml [mcp] 段快照;mcp.toml 不存在时生效)
    fallback: McpConfig,
}

impl McpConfigStore {
    /// 构造存储(workdir 沙箱基目录 + 启动期回落配置)
    pub fn new(workdir: &std::path::Path, fallback: McpConfig) -> Self {
        Self {
            path: workdir.join(".evo").join("mcp.toml"),
            fallback,
        }
    }

    /// 配置文件路径(状态端点展示用)
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// 读取生效配置(mcp.toml 存在优先,损坏 warn 落空;不存在回落 fallback)
    pub fn load(&self) -> McpConfig {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => toml::from_str::<McpTomlFile>(&text)
                .map(|f| f.mcp)
                .unwrap_or_else(|e| {
                    tracing::warn!(path = %self.path.display(), error = %e, "mcp.toml parse failed, using empty config");
                    McpConfig::default()
                }),
            Err(_) => self.fallback.clone(),
        }
    }

    /// 全量写回配置(原子写:tmp + rename;空 servers = 显式清空)
    pub fn save(&self, cfg: &McpConfig) -> Result<(), String> {
        let content = toml::to_string_pretty(&McpTomlFile { mcp: cfg.clone() })
            .map_err(|e| format!("serialize mcp config failed: {e}"))?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("create mcp config dir failed: {e}"))?;
        }
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, content).map_err(|e| format!("write mcp config tmp failed: {e}"))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("rename mcp config failed: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerConfig;
    use std::collections::HashMap;

    fn server(name: &str, command: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: command.to_string(),
            args: vec!["-y".to_string()],
            env: HashMap::new(),
            disabled: false,
            timeout_secs: 60,
            always_allow: vec![],
            transport: None,
            url: None,
            headers: None,
            ..Default::default()
        }
    }

    fn store(dir: &std::path::Path, fallback: McpConfig) -> McpConfigStore {
        McpConfigStore::new(dir, fallback)
    }

    #[test]
    fn test_load_falls_back_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let fallback = McpConfig {
            servers: vec![server("fb", "echo")],
        };
        let s = store(tmp.path(), fallback);
        let loaded = s.load();
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.servers[0].name, "fb");
    }

    #[test]
    fn test_save_then_load_roundtrip_and_file_priority() {
        let tmp = tempfile::tempdir().unwrap();
        let fallback = McpConfig {
            servers: vec![server("fb", "echo")],
        };
        let s = store(tmp.path(), fallback);

        let mut fs_server = server("fs", "npx");
        fs_server.disabled = true;
        fs_server.always_allow = vec!["read_file".to_string()];
        let cfg = McpConfig {
            servers: vec![fs_server],
        };
        s.save(&cfg).unwrap();

        assert!(s.path().exists(), "mcp.toml 应写回 .evo/ 下");
        // 文件存在 → 优先于 fallback(mcp.toml 单一事实源)
        let loaded = s.load();
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.servers[0].name, "fs");
        assert!(loaded.servers[0].disabled);
        assert_eq!(loaded.servers[0].always_allow, vec!["read_file"]);
        // 原子写:tmp 文件不残留
        assert!(!s.path().with_extension("toml.tmp").exists());
    }

    #[test]
    fn test_load_corrupt_file_yields_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let fallback = McpConfig {
            servers: vec![server("fb", "echo")],
        };
        let s = store(tmp.path(), fallback);
        std::fs::create_dir_all(s.path().parent().unwrap()).unwrap();
        std::fs::write(s.path(), "not [ valid toml {{{{").unwrap();
        let loaded = s.load();
        assert!(loaded.servers.is_empty(), "损坏文件 warn 落空,不回落");
    }

    #[test]
    fn test_save_empty_servers_writes_explicit_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path(), McpConfig::default());
        s.save(&McpConfig::default()).unwrap();
        // 显式空文件存在 = 用户清空语义(不再回落 fallback)
        assert!(s.path().exists());
        assert!(s.load().servers.is_empty());
    }

    #[test]
    fn test_saved_format_uses_mcp_servers_table() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path(), McpConfig::default());
        let cfg = McpConfig {
            servers: vec![server("x", "node")],
        };
        s.save(&cfg).unwrap();
        let text = std::fs::read_to_string(s.path()).unwrap();
        // 与 evo-agent.toml [mcp] 段同构:用户可整段拷贝
        assert!(
            text.contains("[[mcp.servers]]"),
            "应为 [[mcp.servers]] 段: {text}"
        );
    }
}
