// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
//
// EventEmitter 模式示例 — 用 .on() 注册事件监听器
//
// 用法:
//   1. 启动 server:  evo-agent serve --port 8081
//   2. 运行示例:     node examples/event-driven.js
//
// 展示:
//   - EventEmitter 模式(.on + consume())
//   - 多个独立监听器(log + 统计 + 实时输出)
//   - stream.collect() 便捷方法

'use strict';

const { EvoAgentClient } = require('../');

const SERVER = process.env.EVO_AGENT_URL || 'http://127.0.0.1:8081';
const AGENT_TYPE = process.env.AGENT_TYPE || 'general';
const GOAL = process.env.GOAL || 'What is 2+2? Explain briefly.';

// ---------------------------------------------------------------------------
// 示例 1: EventEmitter 模式
// ---------------------------------------------------------------------------

async function eventEmitterMode() {
  console.log('=== EventEmitter Mode ===\n');

  const client = new EvoAgentClient(SERVER);
  const stream = await client.runStream(AGENT_TYPE, { agent_type: AGENT_TYPE, goal: GOAL });

  // 统计收集器
  const stats = {
    deltas: 0,
    toolCalls: 0,
    steps: 0,
    startTime: Date.now(),
  };

  // 注册各类事件监听器
  stream.on('session_created', (data) => {
    console.log(`[session] ${data.session_id}`);
  });

  stream.on('step', (data) => {
    stats.steps = data.step;
    console.log(`\n--- step ${data.step} ---`);
  });

  stream.on('llm_delta', (data) => {
    stats.deltas++;
    process.stdout.write(data.text);
  });

  stream.on('llm_done', (data) => {
    console.log(`\n  [done: ${data.finish_reason || '?'}]`);
  });

  stream.on('tool_call', (data) => {
    stats.toolCalls++;
    console.log(`  [tool] ${data.name}`);
  });

  stream.on('tool_result', (data) => {
    console.log(`  [result] ${data.name}: ${JSON.stringify(data.result).slice(0, 100)}`);
  });

  stream.on('error', (data) => {
    console.error(`  [error] ${data.error}`);
  });

  stream.on('info', (data) => {
    console.log(`  [info] ${data.message}`);
  });

  // catch-all 监听器(调试用)
  if (process.env.DEBUG) {
    stream.on('event', (event) => {
      console.error(`  [debug] ${event.type}: ${JSON.stringify(event.data).slice(0, 120)}`);
    });
  }

  // done 事件
  stream.on('done', (result) => {
    const elapsed = Date.now() - stats.startTime;
    console.log(`\n\n${'='.repeat(50)}`);
    console.log(`  Result:`);
    console.log(`    success:  ${result.success}`);
    console.log(`    steps:    ${result.steps}`);
    console.log(`    duration: ${result.duration_ms}ms (client-side: ${elapsed}ms)`);
    console.log(`    tokens:   ${stats.deltas} delta events`);
    console.log(`    tools:    ${stats.toolCalls} calls`);
    if (result.error) {
      console.log(`    error:    ${result.error}`);
    }
    console.log(`${'='.repeat(50)}`);
  });

  // 开始消费(阻塞直到流结束)
  await stream.consume();
  console.log('\n[stream ended]');
}

// ---------------------------------------------------------------------------
// 示例 2: collect() 便捷方法(只要最终结果,不要中间事件)
// ---------------------------------------------------------------------------

async function collectMode() {
  console.log('\n=== Collect Mode (fire-and-forget result) ===\n');

  const client = new EvoAgentClient(SERVER);
  const stream = await client.runStream(AGENT_TYPE, { agent_type: AGENT_TYPE, goal: GOAL });

  // collect() 内部消费所有事件,只返回最终的 AgentResult
  // 适合"我只要结果,不在乎过程"的场景
  const result = await stream.collect();

  console.log(`success:  ${result.success}`);
  console.log(`steps:    ${result.steps}`);
  console.log(`content:  ${result.content}`);
  if (result.error) {
    console.log(`error:    ${result.error}`);
  }
}

// ---------------------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------------------

async function main() {
  // 跑 EventEmitter 模式
  await eventEmitterMode();

  // collect 模式(取消注释来运行)
  // 注意: collect 会发起第二个独立请求
  // await collectMode();
}

main().catch((err) => {
  console.error('Error:', err.message);
  process.exit(1);
});
