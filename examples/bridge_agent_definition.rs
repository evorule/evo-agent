// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Smoke test: 演示 AgentDefinition → AgentRunner 桥接
//!
//! 流程:
//! 1. 加载 agent.json(从临时目录)
//! 2. 构造 evorule HTTP 客户端
//! 3. 用 `builtin_tools::default_safe_toolkit(workdir)` 装 6 个工具
//! 4. `AgentRunner::from_definition()` 一步组装
//! 5. 验证 runner 配置正确
//!
//! 运行:`cargo run --example bridge_agent_definition`

use std::io::Write;
use std::path::PathBuf;

use evo_agent::agent::definition::AgentDefinitionManager;
use evo_agent::agent::runner::AgentRunner;
use evo_agent::api::evorule_client::EvoruleApiClient;
use evo_agent::builtin_tools::default_safe_toolkit;

fn write_agent_json(dir: &PathBuf) {
    let path = dir.join("researcher.json");
    let mut f = std::fs::File::create(&path).expect("create file");
    f.write_all(
        br#"{
            "agent_type": "researcher",
            "version": "0.1.0",
            "description": "Research-style agent that can read files and search",
            "system_prompt": "You are a careful research assistant. Read files before answering.",
            "model": "MiniMax-M2.5",
            "temperature": 0.3,
            "max_steps": 20,
            "step_timeout_secs": 60,
            "tools": ["file_read", "search_files", "file_list"],
            "memory": { "type": "none", "namespace": "" }
        }"#,
    )
    .expect("write file");
}

#[tokio::main]
async fn main() {
    // 1. 准备一个临时项目目录
    let tmp = tempfile::tempdir().expect("create tempdir");
    let workdir = tmp.path();
    let agents_dir = workdir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create agents dir");

    // 2. 写一个 agent.json
    write_agent_json(&agents_dir);
    println!("Wrote agent.json to {}", agents_dir.display());

    // 3. 用 AgentDefinitionManager 加载
    let mgr = AgentDefinitionManager::new(agents_dir.clone());
    let def = mgr.load("researcher").expect("load researcher.json");
    println!("Loaded agent: type={}, version={}", def.agent_type, def.version);
    println!("  model: {}", def.model);
    println!("  tools: {:?}", def.tools);
    println!("  memory.type: {}", def.memory.memory_type);

    // 4. 构造 evorule 客户端(假地址,实际不会发请求因为 memory=none)
    let client = EvoruleApiClient::new("http://localhost:8080");

    // 5. 装 6 个安全工具
    let tool_handler = default_safe_toolkit(workdir);
    println!("\nTool handler registered {} tools", {
        // 用 has_tool 测试每个,或干脆只确认这几个在
        let mut count = 0;
        for n in &["file_read", "file_list", "file_write", "search_files", "shell_exec", "http_get"] {
            if tool_handler.has_tool(n) {
                count += 1;
            } else {
                eprintln!("  WARNING: {} not registered", n);
            }
        }
        count
    });

    // 6. 桥接:AgentDefinition + client + tools → 完整可跑 runner
    let _runner = AgentRunner::from_definition(def, client, tool_handler, None)
        .await
        .expect("from_definition failed");
    println!("\n=== Bridge OK ===");
    println!("  Runner created (config + tool_handler + memory all wired).");
    println!("  0.1.0 阶段不演示 runner.run(),留给 CLI (#2) 集成。");
    println!("\n=== Bridge 完整流程: agent.json + 6 工具 → 可跑 AgentRunner ===");
}
