// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

'use strict';

/**
 * evo-agent Node.js SDK — 零依赖 HTTP/SSE 客户端
 *
 * @example
 * const { EvoAgentClient } = require('evo-agent-node');
 *
 * const client = new EvoAgentClient('http://127.0.0.1:8081');
 *
 * // 同步执行
 * const result = await client.run('general', 'hello');
 *
 * // 流式执行
 * const stream = await client.runStream('general', 'write a haiku');
 * for await (const event of stream) {
 *   if (event.type === 'llm_delta') process.stdout.write(event.data.text);
 *   if (event.type === 'done') console.log(event.data);
 * }
 */

const { EvoAgentClient, AgentEventStream } = require('./lib/client');

module.exports = { EvoAgentClient, AgentEventStream };
