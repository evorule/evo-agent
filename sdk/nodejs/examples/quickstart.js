// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
//
// 快速开始示例 — 基础 API 调用(非流式)
//
// 用法:
//   1. 启动 server:  evo-agent serve --port 8081
//   2. 运行示例:     node examples/quickstart.js

'use strict';

const { EvoAgentClient } = require('../');

const SERVER = process.env.EVO_AGENT_URL || 'http://127.0.0.1:8081';

async function main() {
  const client = new EvoAgentClient(SERVER);

  // 1. 健康检查
  console.log('=== 1. Health Check ===');
  const health = await client.health();
  console.log(`  ${health}`);

  // 2. 列出可用 agent
  console.log('\n=== 2. List Agents ===');
  const { agents } = await client.listAgents();
  for (const a of agents) {
    console.log(`  - ${a.agent_type} (v${a.version}): ${a.description}`);
    console.log(`    tools: ${a.tools.join(', ')}`);
  }

  // 3. 查看 agent 定义
  if (agents.length > 0) {
    const type = agents[0].agent_type;
    console.log(`\n=== 3. Get Agent Definition: ${type} ===`);
    const def = await client.getAgent(type);
    console.log(`  model: ${def.model}`);
    console.log(`  temperature: ${def.temperature}`);
    console.log(`  max_steps: ${def.max_steps}`);
    console.log(`  system_prompt: ${def.system_prompt.slice(0, 80)}...`);
    if (def.memory_config) {
      console.log(`  memory: type=${def.memory_config.type}, namespace=${def.memory_config.namespace}`);
    }
  }

  // 4. 同步执行(需要 evorule + LLM 后端在线)
  //    取消注释下面的代码来实际执行
  //
  // console.log('\n=== 4. Run Agent (sync) ===');
  // const result = await client.run('general', {
  //   agent_type: 'general',
  //   goal: 'What files are in the current directory?',
  //   max_steps: 5,
  // });
  // console.log(`  success: ${result.success}`);
  // console.log(`  steps: ${result.steps}`);
  // console.log(`  duration: ${result.duration_ms}ms`);
  // console.log(`  content: ${result.content}`);
  // if (result.error) console.log(`  error: ${result.error}`);

  console.log('\n=== Done ===');
}

main().catch((err) => {
  console.error('Error:', err.message);
  process.exit(1);
});
