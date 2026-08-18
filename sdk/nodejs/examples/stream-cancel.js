// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
//
// SSE 流式执行 + 异步取消示例
//
// 用法:
//   1. 启动 server:  evo-agent serve --port 8081
//   2. 运行示例:     node examples/stream-cancel.js
//   3. (可选) AUTO_CANCEL_MS=3000 node examples/stream-cancel.js
//      → 3 秒后自动取消
//
// 展示:
//   - AsyncIterator 模式消费 SSE 流(for await...of)
//   - 实时打印 LLM token 增量
//   - session_id 获取
//   - 流式取消(client.cancel)
//   - 取消后的 error + done 事件处理

'use strict';

const { EvoAgentClient } = require('../');

const SERVER = process.env.EVO_AGENT_URL || 'http://127.0.0.1:8081';
const AGENT_TYPE = process.env.AGENT_TYPE || 'general';
const GOAL = process.env.GOAL || 'Write a detailed essay about the history of computing, from Babbage to modern AI.';
const AUTO_CANCEL_MS = parseInt(process.env.AUTO_CANCEL_MS || '0', 10);

async function main() {
  const client = new EvoAgentClient(SERVER, { streamTimeout: 0 }); // 0 = 无超时

  console.log(`Starting streaming: agent=${AGENT_TYPE}`);
  console.log(`Goal: ${GOAL}`);
  console.log('---');

  const stream = await client.runStream(AGENT_TYPE, { agent_type: AGENT_TYPE, goal: GOAL });

  // 自动取消定时器(如果设置了 AUTO_CANCEL_MS)
  let cancelTimer = null;
  if (AUTO_CANCEL_MS > 0) {
    cancelTimer = setTimeout(async () => {
      if (stream.sessionId) {
        console.log(`\n\n[Auto-cancel after ${AUTO_CANCEL_MS}ms] session=${stream.sessionId}`);
        try {
          await stream.cancel(client);
          console.log('[Cancel sent]');
        } catch (err) {
          console.error('[Cancel failed]', err.message);
        }
      }
    }, AUTO_CANCEL_MS);
  }

  // 手动取消(按 Enter 触发,仅交互模式)
  let manualCancelListener = null;
  if (process.stdin.isTTY && AUTO_CANCEL_MS === 0) {
    console.log('[Press Enter to cancel]');
    manualCancelListener = async () => {
      if (stream.sessionId) {
        console.log(`\n[Manual cancel] session=${stream.sessionId}`);
        try {
          await stream.cancel(client);
        } catch (err) {
          console.error('[Cancel failed]', err.message);
        }
      }
    };
    process.stdin.once('data', manualCancelListener);
  }

  // 消费 SSE 流
  let stepCount = 0;
  let toolCallCount = 0;

  try {
    for await (const event of stream) {
      switch (event.type) {
        case 'session_created':
          console.log(`[session] ${event.data.session_id}`);
          break;

        case 'step':
          stepCount = event.data.step;
          console.log(`\n--- step ${stepCount} ---`);
          break;

        case 'llm_delta':
          // 实时打印 LLM token(不换行)
          process.stdout.write(event.data.text);
          break;

        case 'llm_done':
          console.log(`\n  [llm done: ${event.data.finish_reason || '?'}]`);
          break;

        case 'tool_call':
          toolCallCount++;
          console.log(`  [tool call] ${event.data.name}(${JSON.stringify(event.data.args)})`);
          break;

        case 'tool_result':
          console.log(`  [tool result] ${event.data.name} → ${JSON.stringify(event.data.result)}`);
          break;

        case 'info':
          console.log(`  [info] ${event.data.message}`);
          break;

        case 'error':
          console.log(`\n  [error] ${event.data.error}`);
          break;

        case 'done':
          console.log(`\n\n${'='.repeat(50)}`);
          console.log(`  success:    ${event.data.success}`);
          console.log(`  steps:      ${event.data.steps}`);
          console.log(`  duration:   ${event.data.duration_ms}ms`);
          console.log(`  tool_calls: ${event.data.tool_calls.length}`);
          if (event.data.error) {
            console.log(`  error:      ${event.data.error}`);
          }
          console.log(`${'='.repeat(50)}`);
          break;
      }
    }
  } catch (err) {
    if (err.code === 'ECONNRESET' || err.message.includes('aborted')) {
      // 连接被取消时断开,正常
      console.log('\n[connection closed]');
    } else {
      throw err;
    }
  }

  // 清理
  if (cancelTimer) clearTimeout(cancelTimer);
  if (manualCancelListener) {
    process.stdin.removeListener('data', manualCancelListener);
    process.stdin.pause();
  }

  console.log(`\nFinished: ${stepCount} steps, ${toolCallCount} tool calls`);
}

main().catch((err) => {
  console.error('Error:', err.message);
  process.exit(1);
});
