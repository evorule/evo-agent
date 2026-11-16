// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 治理包（governance pack）——会话治理机制独立模块
//!
//! 以独立模块形态提供治理能力，不嵌入 definition/config struct：
//! 零级联初始化器改动、零 serde 冲突、装配层按需调用。
//!
//! ## 机制
//!
//! - [`GovernancePack::handover_check`]: 长会话退出时检查 handover 文件是否已写
//! - [`GovernancePack::pit_inject`]: 按触及面过滤注入坑账本条目（预算帽内）
//! - [`GovernancePack::antidote_check`]: 稳定事实裁剪前校验解毒剂覆盖
//!
//! ## 激活形态（D6 裁定 A）
//!
//! 工作区内放 `governance.json` 即激活（缺省不存在=关闭=现状行为）。
//! 本机装机在 workspace 下放一份即可，公开仓零影响。
//!
//! ## 边界
//!
//! 本模块为运行时独立插件，不走 definition/config struct 管道——
//! 零级联初始化器改动、零公开面触发。

use std::path::{Path, PathBuf};

/// 治理包配置（从工作区 `governance.json` 读取）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GovernancePack {
    /// 收尾拦截：会话退出时检查 handover 文件是否已写
    #[serde(default = "default_true")]
    pub handover_gate: bool,
    /// 坑账本注入：开工时按触及面过滤注入
    #[serde(default = "default_true")]
    pub pit_injection: bool,
    /// 解毒剂覆盖校验：稳定事实裁剪前校验
    #[serde(default = "default_true")]
    pub antidote_check: bool,
    /// 坑账本文件路径（相对工作区）
    #[serde(default = "default_pit_path")]
    pub pit_ledger_path: String,
    /// 注入预算帽（最大注入行数；PD-9 参数包）
    #[serde(default = "default_budget")]
    pub pit_budget_lines: usize,
}

fn default_true() -> bool {
    true
}

fn default_pit_path() -> String {
    "skills/PITS.md".to_string()
}

fn default_budget() -> usize {
    50
}

impl Default for GovernancePack {
    fn default() -> Self {
        Self {
            handover_gate: true,
            pit_injection: true,
            antidote_check: true,
            pit_ledger_path: default_pit_path(),
            pit_budget_lines: 50,
        }
    }
}

impl GovernancePack {
    /// 从工作区 `governance.json` 读取（不存在=None=治理包关闭）
    pub fn from_workspace(workdir: &Path) -> Option<Self> {
        let path = workdir.join("governance.json");
        let data = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&data).ok()
    }

    /// 收尾拦截：handover 文件存在性检查
    ///
    /// 返回 Some(warning) = 需要 agent 注意（未写 handover）
    /// 返回 None = 通过（handover 已写或 gate 关闭）
    pub fn handover_check(&self, workdir: &Path, turns: usize) -> Option<String> {
        if !self.handover_gate || turns == 0 {
            return None;
        }
        let handover = workdir.join("handover.md");
        if handover.exists() {
            return None;
        }
        Some(format!(
            "governance: session ended ({turns} turns) without handover — \
             write handover.md for cross-session continuity"
        ))
    }

    /// 坑账本触及面注入：从 PITS 文件读取条目、按关键词过滤、预算帽截断
    ///
    /// 返回注入行列表（空 = 无匹配或 gate 关闭）
    pub fn pit_inject(&self, workdir: &Path, surface_keywords: &[&str]) -> Vec<String> {
        if !self.pit_injection {
            return Vec::new();
        }
        let pit_path = workdir.join(&self.pit_ledger_path);
        let data = match std::fs::read_to_string(&pit_path) {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };
        let budget = self.pit_budget_lines;
        let mut result = Vec::new();
        for line in data.lines() {
            if result.len() >= budget {
                break;
            }
            let l = line.trim();
            if l.is_empty() || l.starts_with('#') || l.starts_with("//") {
                continue;
            }
            for kw in surface_keywords {
                if l.contains(kw) {
                    result.push(l.to_string());
                    break;
                }
            }
        }
        result
    }

    /// 解毒剂覆盖校验：检查给定事实条目是否有解毒剂标记
    ///
    /// 返回缺乏解毒剂覆盖的可裁事实条目列表
    pub fn antidote_check<'a>(&self, facts: &'a [String]) -> Vec<&'a String> {
        if !self.antidote_check {
            return Vec::new();
        }
        // 有解毒剂标记 = 行内含 "antidote" 或 "解毒"
        // 无标记 = 需要补充覆盖
        facts
            .iter()
            .filter(|f| !f.contains("antidote") && !f.contains("解毒"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_from_workspace_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(GovernancePack::from_workspace(dir.path()).is_none());
    }

    #[test]
    fn test_from_workspace_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("governance.json");
        fs::write(&cfg_path, r#"{"handover_gate": true, "pit_budget_lines": 20}"#).unwrap();
        let pack = GovernancePack::from_workspace(dir.path()).unwrap();
        assert!(pack.handover_gate);
        assert_eq!(pack.pit_budget_lines, 20);
    }

    #[test]
    fn test_handover_check_no_file_warns() {
        let dir = tempfile::tempdir().unwrap();
        let pack = GovernancePack::default();
        let warning = pack.handover_check(dir.path(), 10);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("handover"));
    }

    #[test]
    fn test_handover_check_file_exists_passes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("handover.md"), "# handover").unwrap();
        let pack = GovernancePack::default();
        assert!(pack.handover_check(dir.path(), 10).is_none());
    }

    #[test]
    fn test_pit_inject_filters_by_keyword() {
        let dir = tempfile::tempdir().unwrap();
        let pit = dir.path().join("PITS.md");
        fs::write(
            &pit,
            "- git add -A 卷杂物\n- 审批超时勿降级\n- 不相关条目\n",
        )
        .unwrap();
        let pack = GovernancePack {
            pit_ledger_path: pit.to_string_lossy().to_string(),
            ..Default::default()
        };
        let hits = pack.pit_inject(dir.path(), &["git add", "审批"]);
        assert_eq!(hits.len(), 2);
        assert!(hits[0].contains("git add"));
        assert!(hits[1].contains("审批"));
    }

    #[test]
    fn test_pit_inject_respects_budget() {
        let dir = tempfile::tempdir().unwrap();
        let pit = dir.path().join("PITS.md");
        let content: String = (0..100)
            .map(|i| format!("- git 坑 {}\n", i))
            .collect();
        fs::write(&pit, &content).unwrap();
        let pack = GovernancePack {
            pit_ledger_path: pit.to_string_lossy().to_string(),
            pit_budget_lines: 5,
            ..Default::default()
        };
        let hits = pack.pit_inject(dir.path(), &["git"]);
        assert_eq!(hits.len(), 5);
    }

    #[test]
    fn test_antidote_check_flags_uncovered_facts() {
        let pack = GovernancePack::default();
        let facts = vec![
            "fact with antidote coverage".to_string(),
            "fact without coverage".to_string(),
            "another uncovered fact".to_string(),
        ];
        let uncovered = pack.antidote_check(&facts);
        assert_eq!(uncovered.len(), 2);
    }
}
