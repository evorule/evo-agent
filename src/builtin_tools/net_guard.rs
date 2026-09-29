// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `net_guard` —— 网络负面域守卫：benchmark 基础设施访问硬拦截
//!
//! ## 语义
//!
//! shell_exec（local / docker-exec 两后端）与 http_get 的命令串/URL 命中
//! [`DENIED_NETWORK_PATTERNS`] 即拒绝。此为**完整性红线拦截**：
//!
//! - 不属于审批链语义判断——审批开合（approval_mode / approved 参数）对它
//!   无影响，任何模式下都拒绝；
//! - **不设配置口**——红线没有绕过开关；
//! - 拒绝事件以 Err 文本进入工具观察面，随审计链留痕。
//!
//! ## 负面域集合
//!
//! benchmark 官方站点（含全部子域）+ benchmark GitHub 组织路径（含
//! raw/codeload 镜像形态）+ HF 基准仓库路径。匹配 = 小写化子串。
//!
//! 边界裁定：`pip install terminal-bench`（PyPI 公共工具包安装）不在拦截面
//! ——完整性红线指向「获取任务答案/访问基准仓库」，安装公共 CLI 工具不是
//! 答案通道；域/组织路径形态已覆盖全部仓库访问向量（git clone / curl /
//! wget / HF 直链），按误伤最小化取精确形态。

/// 网络负面域模式（小写；子串匹配）
pub const DENIED_NETWORK_PATTERNS: &[&str] = &[
    // benchmark 官方站点（含全部子域）
    "tbench.ai",
    // benchmark GitHub 组织（github.com 与 raw/codeload 等镜像形态统一命中）
    "laude-institute",
    // benchmark GitHub 组织路径
    "github.com/terminal-bench",
    // HF 基准仓库（数据集/榜单仓路径；不影响 HF 上模型与无关数据集的正常下载）
    "huggingface.co/terminal-bench",
    "huggingface.co/harborframework",
    "huggingface.co/datasets/terminal-bench",
    "hf.co/terminal-bench",
    "hf.co/harborframework",
];

/// 检查命令串/URL 是否命中网络负面域；命中即 Err（硬拒绝，无审批通道）
pub fn check_denied_network_target(text: &str) -> Result<(), String> {
    let lowered = text.to_lowercase();
    for pattern in DENIED_NETWORK_PATTERNS {
        if lowered.contains(pattern) {
            return Err(format!(
                "network access matching '{}' is denied by policy: \
                 benchmark infrastructure domains are off-limits; \
                 obtain data from task-appropriate sources only",
                pattern
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_benchmark_site_denied() {
        // 主域与子域形态
        assert!(check_denied_network_target("curl https://tbench.ai/leaderboard").is_err());
        assert!(check_denied_network_target("wget https://docs.tbench.ai/integrity").is_err());
        assert!(check_denied_network_target("TBCHECK=1 curl https://www.tbench.ai/").is_err());
    }

    #[test]
    fn test_benchmark_github_repos_denied() {
        // 组织路径与镜像形态
        assert!(check_denied_network_target(
            "git clone https://github.com/laude-institute/terminal-bench"
        )
        .is_err());
        assert!(check_denied_network_target(
            "curl https://raw.githubusercontent.com/laude-institute/terminal-bench/main/x.sh"
        )
        .is_err());
        assert!(
            check_denied_network_target("git clone https://github.com/terminal-bench/tasks")
                .is_err()
        );
    }

    #[test]
    fn test_hf_benchmark_repos_denied() {
        assert!(check_denied_network_target(
            "wget https://huggingface.co/datasets/terminal-bench/terminal-bench-2"
        )
        .is_err());
        assert!(check_denied_network_target(
            "curl https://huggingface.co/harborframework/terminal-bench-2-leaderboard"
        )
        .is_err());
        assert!(check_denied_network_target("curl https://hf.co/terminal-bench/x").is_err());
    }

    #[test]
    fn test_legitimate_targets_pass() {
        // pypi / 正常 GitHub 仓 / HF 模型与无关数据集 / 公共域全部放行
        assert!(
            check_denied_network_target("pip3 install pandas -i https://pypi.org/simple").is_ok()
        );
        assert!(
            check_denied_network_target("git clone https://github.com/libgit2/libgit2").is_ok()
        );
        assert!(check_denied_network_target(
            "curl https://huggingface.co/meta-llama/Meta-Llama-3-8B/resolve/main/config.json"
        )
        .is_ok());
        assert!(check_denied_network_target("wget https://example.com/data.csv").is_ok());
        assert!(check_denied_network_target("git status && git log").is_ok());
    }

    #[test]
    fn test_case_insensitive() {
        assert!(check_denied_network_target("curl https://TBENCH.AI/x").is_err());
        assert!(
            check_denied_network_target("git clone https://GITHUB.COM/LAUDE-INSTITUTE/tb").is_err()
        );
    }
}
