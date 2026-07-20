// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Smoke test: 加载 examples/config.toml 并打印结果。
//!
//! 运行:`cargo run --example smoke_config`

use evo_agent::config::Config;
use std::path::Path;

fn main() {
    // 隔离 HOME,避免真实用户配置干扰
    let original_home = std::env::var("HOME").ok();
    let original_appdata = std::env::var("APPDATA").ok();
    std::env::set_var("HOME", "/tmp/nonexistent-home");
    std::env::set_var("APPDATA", "/tmp/nonexistent-appdata");

    // 给一个假 API key 让占位符解析能成功
    std::env::set_var("MINIMAX_API_KEY", "smoke-test-fake-key");

    // 清掉可能影响 smoke test 的 env override
    let saved_env_overrides: Vec<(String, Option<String>)> = [
        "EVO_AGENT_LLM__PROVIDER",
        "EVO_AGENT_LLM__MODEL",
        "EVO_AGENT_LLM__API_KEY",
        "EVO_AGENT_EVORULE__BASE_URL",
    ]
    .iter()
    .map(|k| (k.to_string(), std::env::var(k).ok()))
    .collect();
    for (k, _) in &saved_env_overrides {
        std::env::remove_var(k);
    }

    let config_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let cfg = Config::load(&config_path).expect("failed to load config");

    // 还原所有 env
    for (k, v) in &saved_env_overrides {
        if let Some(val) = v {
            std::env::set_var(k, val);
        }
    }
    if let Some(h) = original_home {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(a) = original_appdata {
        std::env::set_var("APPDATA", a);
    } else {
        std::env::remove_var("APPDATA");
    }

    println!("=== Loaded config from {} ===", config_path.display());
    println!("llm.provider       = {}", cfg.llm.provider);
    println!("llm.api_key        = {}", cfg.llm.api_key);
    println!("llm.model          = {}", cfg.llm.model);
    println!("llm.api_base       = {}", cfg.llm.api_base);
    println!("llm.timeout_secs   = {}", cfg.llm.timeout_secs);
    println!("llm.max_retries    = {}", cfg.llm.max_retries);
    println!("evorule.base_url   = {}", cfg.evorule.base_url);
    println!("evorule.api_key    = '{}'", cfg.evorule.api_key);
    println!("evorule.timeout    = {}", cfg.evorule.timeout_secs);
    println!("logging.level      = {}", cfg.logging.level);
    println!("logging.format     = {}", cfg.logging.format);
    println!("agents.dir         = {}", cfg.agents.dir.display());
    println!("agents.default     = {}", cfg.agents.default);

    assert_eq!(cfg.llm.provider, "minimax");
    assert_eq!(cfg.llm.api_key, "smoke-test-fake-key");
    assert_eq!(cfg.llm.model, "MiniMax-M2.5");
    assert_eq!(cfg.evorule.base_url, "http://localhost:8080");
    assert_eq!(cfg.agents.dir.to_str().unwrap(), "./agents");
    assert_eq!(cfg.agents.default, "general");

    println!("\n✓ All assertions passed");
}
