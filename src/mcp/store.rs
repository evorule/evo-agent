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

    /// 读取生效配置(mcp.toml 存在优先;不存在回落 fallback)。
    ///
    /// 文件面校验（与 API PUT 面同一套 validate 语义在此处的镜像=结构级
    /// 约束：server 名唯一+timeout>0——重复名整组覆盖与 timeout=0 不限时
    /// 的入场路径封堵；API PUT 的完整校验仍在端点层）。解析失败不再静默
    /// 落空配置：**错误态显式上报**（坏档与没配可区分），由调用方决定
    /// fail-visible 呈现；文件损坏时回落 fallback 而非空配置（可用性保底）。
    pub fn load_checked(&self) -> Result<McpConfig, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => {
                let cfg: McpConfig = toml::from_str::<McpTomlFile>(&text)
                    .map(|f| f.mcp)
                    .map_err(|e| {
                        format!(
                            "mcp.toml parse failed ({}): {e} — fix or remove the file;                              the MCP surface stays on the fallback config meanwhile",
                            self.path.display()
                        )
                    })?;
                // 结构级约束镜像（与 PUT 面同语义的最小子集）
                let mut seen = std::collections::HashSet::new();
                for srv in &cfg.servers {
                    if !seen.insert(srv.name.clone()) {
                        return Err(format!(
                            "mcp.toml has duplicate server name '{}' — remove the                              duplicate entry (later duplicates silently override earlier                              ones, including their trust settings)",
                            srv.name
                        ));
                    }
                    if srv.timeout_secs == 0 {
                        return Err(format!(
                            "mcp.toml server '{}' has timeout_secs = 0 (unbounded);                              set a positive timeout",
                            srv.name
                        ));
                    }
                }
                Ok(cfg)
            }
            Err(_) => Ok(self.fallback.clone()),
        }
    }

    /// 读取生效配置(兼容壳：校验失败 warn+回落 fallback——既有调用方语义)
    pub fn load(&self) -> McpConfig {
        match self.load_checked() {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(error = %e, "mcp.toml invalid; falling back");
                self.fallback.clone()
            }
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
    fn test_load_corrupt_file_is_visible_and_falls_back() {
        // 行为变更：损坏文件不再静默落空配置（坏档与没配可区分）——
        // load_checked 显式报错；兼容壳 load() warn 后回落 fallback（可用性保底）
        let tmp = tempfile::tempdir().unwrap();
        let fallback = McpConfig {
            servers: vec![server("fb", "echo")],
        };
        let s = store(tmp.path(), fallback);
        std::fs::create_dir_all(s.path().parent().unwrap()).unwrap();
        std::fs::write(s.path(), "not [ valid toml {{{{").unwrap();
        let err = s
            .load_checked()
            .expect_err("损坏文件必须显式报错（坏档与没配可区分）");
        assert!(err.contains("parse failed"), "{err}");
        let loaded = s.load();
        assert_eq!(loaded.servers.len(), 1, "兼容壳回落 fallback 保底");
        assert_eq!(loaded.servers[0].name, "fb");
    }

    #[test]
    fn test_load_duplicate_server_and_zero_timeout_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path(), McpConfig::default());
        std::fs::create_dir_all(s.path().parent().unwrap()).unwrap();
        let body = "[[mcp.servers]]\n\
                    name = \"a\"\n\
                    command = \"x\"\n\
                    [[mcp.servers]]\n\
                    name = \"a\"\n\
                    command = \"y\"\n";
        std::fs::write(s.path(), body).unwrap();
        let err = s.load_checked().expect_err("重复 server 名必须拒绝");
        assert!(err.contains("duplicate"), "{err}");
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
