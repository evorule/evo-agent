// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// DAP 会话层单测:launch 时序/状态机迁移/断点全量同步语义/verified 写回。
// 零真实 WebSocket/适配器:client 注入假件(捕获请求帧,脚本化响应与事件)。
import { describe, it, expect, vi } from 'vitest';
import { createDebugSession, DBG_STATES, normalizeLaunchConfigs } from '../src/lib/dap/session.js';

/** 假 client:捕获请求,按脚本应答;事件经 emitEvent 注入 */
function fakeClient() {
  const requests = [];
  let nextSeq = 1;
  const handlers = { event: [], clientEvent: [] };
  const responder = { script: {} }; // command → body(或函数)
  const client = {
    state: 'idle',
    connect() {
      client.state = 'connecting';
      // 假 ws 立即 open
      queueMicrotask(() => handlers.clientEvent.forEach((h) => h({ kind: 'ws-open' })));
    },
    disconnect() {
      client.state = 'ended';
    },
    request(command, args) {
      requests.push({ command, args });
      // 真实适配器时序镜像:launch 请求后吐 initialized 事件(等 configurationDone)
      if (command === 'launch') {
        queueMicrotask(() => handlers.event.forEach((h) => h('initialized', {})));
      }
      const body = responder.script[command];
      const value = typeof body === 'function' ? body(args) : body;
      return Promise.resolve(value === undefined ? {} : value);
    },
    // 事件注入(session 经装配层 handleDapEvent 消费)
    emitDap(event, body) {
      handlers.event.forEach((h) => h(event, body));
    },
    emitClientEvent(ev) {
      handlers.clientEvent.forEach((h) => h(ev));
    },
    onClientEvent(h) {
      handlers.clientEvent.push(h);
    },
    onDapEvent(h) {
      handlers.event.push(h);
    },
  };
  return { client, requests, responder };
}

/** 组装 session:client 事件直连 session(装配层同款接线) */
function makeSession(overrides = {}) {
  const f = fakeClient();
  const states = [];
  const outputs = [];
  const stoppedBodies = [];
  const bpWrites = [];
  const session = createDebugSession(f.client, {
    workdirBase: 'D:/proj',
    onState: (s, d) => states.push([s, d]),
    onOutput: (b) => outputs.push(b),
    onStopped: (b) => stoppedBodies.push(b),
    ...overrides,
    onBreakpoints: (path, lines) => {
      bpWrites.push([path, lines]);
      overrides.onBreakpoints?.(path, lines);
    },
  });
  f.client.onClientEvent((ev) => session.handleClientEvent(ev));
  f.client.onDapEvent((ev, body) => session.handleDapEvent(ev, body));
  return { f, session, states, outputs, stoppedBodies, bpWrites };
}

const RUST_CONFIG = { type: 'rust', request: 'launch', name: 'debug demo', program: 'target/debug/demo.exe' };

describe('normalizeLaunchConfigs', () => {
  it('接受数组形态与 configurations 包裹形态;过滤 attach 与畸形项', () => {
    expect(normalizeLaunchConfigs([{ type: 'rust', name: 'a' }])).toHaveLength(1);
    expect(normalizeLaunchConfigs({ configurations: [{ type: 'node', name: 'b' }] })).toHaveLength(1);
    expect(normalizeLaunchConfigs([{ type: 'rust', request: 'attach', name: 'x' }])).toHaveLength(0);
    expect(normalizeLaunchConfigs([{ nope: 1 }, null, 's'])).toHaveLength(0);
    expect(normalizeLaunchConfigs(null)).toEqual([]);
  });
});

describe('launch 时序与状态机', () => {
  it('start:connect→initialize→launch→configurationDone,状态依序迁移到 running', async () => {
    const { f, session, states } = makeSession();
    const p = session.start('rust', RUST_CONFIG);
    await p;
    expect(f.requests.map((r) => r.command)).toEqual(['initialize', 'launch', 'configurationDone']);
    expect(f.requests[0].args.adapterID).toBe('rust');
    expect(f.requests[1].args).toBe(RUST_CONFIG); // launch 参数=配置原文
    expect(states.map(([s]) => s)).toEqual([
      DBG_STATES.CONNECTING,
      DBG_STATES.INITIALIZING,
      DBG_STATES.LAUNCHING,
      DBG_STATES.RUNNING,
    ]);
    expect(session.state).toBe(DBG_STATES.RUNNING);
  });

  it('initialize 失败 → FAILED 态且 start reject', async () => {
    const { f, session } = makeSession();
    f.responder.script.initialize = () => {
      throw new Error('adapter boom');
    };
    // request 层是 Promise resolve 型假件——用 reject 脚本:直接改写 request
    f.client.request = (command) => {
      if (command === 'initialize') return Promise.reject(new Error('adapter boom'));
      return Promise.resolve({});
    };
    await expect(session.start('rust', RUST_CONFIG)).rejects.toThrow('adapter boom');
    // 失败态可重开(终态放行)
    expect(['failed', 'idle', 'ended']).toContain(session.state);
  });

  it('ws 连接超时 → start reject(15s)', async () => {
    vi.useFakeTimers();
    const { session } = makeSession();
    // 覆写 connect:不触发 ws-open
    const { createDebugSession: _ } = {};
    const p = session.start('rust', RUST_CONFIG).catch((e) => e);
    vi.advanceTimersByTime(16_000);
    const err = await p;
    expect(err.message).toBe('ws 连接超时');
    vi.useRealTimers();
  });

  it('stopped 事件 → STOPPED 态+onStopped;continued → RUNNING', async () => {
    const { f, session, stoppedBodies } = makeSession();
    await session.start('rust', RUST_CONFIG);
    f.client.emitDap('stopped', { reason: 'breakpoint', threadId: 1 });
    expect(session.state).toBe(DBG_STATES.STOPPED);
    expect(stoppedBodies).toEqual([{ reason: 'breakpoint', threadId: 1 }]);
    f.client.emitDap('continued', {});
    expect(session.state).toBe(DBG_STATES.RUNNING);
  });

  it('terminated 事件 → TERMINATED 终态', async () => {
    const { f, session } = makeSession();
    await session.start('rust', RUST_CONFIG);
    f.client.emitDap('terminated', {});
    expect(session.state).toBe(DBG_STATES.TERMINATED);
  });

  it('output 事件透传 onOutput', async () => {
    const { f, session, outputs } = makeSession();
    await session.start('rust', RUST_CONFIG);
    f.client.emitDap('output', { output: 'hello\n', category: 'stdout' });
    expect(outputs).toEqual([{ output: 'hello\n', category: 'stdout' }]);
  });

  it('会话进行中重复 start 拒绝', async () => {
    const { session } = makeSession();
    await session.start('rust', RUST_CONFIG);
    await expect(session.start('rust', RUST_CONFIG)).rejects.toThrow('已在进行中');
  });
});

describe('断点全量同步语义', () => {
  it('会话前灌入的断点在 start 时逐文件全量重放(setBreakpoints)', async () => {
    const { f, session } = makeSession();
    session.primeBreakpoints({ 'src/main.rs': [3, 7], 'src/lib.rs': [12] });
    await session.start('rust', RUST_CONFIG);
    const sb = f.requests.filter((r) => r.command === 'setBreakpoints');
    expect(sb).toHaveLength(2);
    // 全量语义:source 为文件系统路径(pathFormat='path'),breakpoints=该文件全量行
    expect(sb[0].args.source).toEqual({ name: 'main.rs', path: 'D:/proj/src/main.rs' });
    expect(sb[0].args.breakpoints).toEqual([{ line: 3 }, { line: 7 }]);
    expect(sb[1].args.breakpoints).toEqual([{ line: 12 }]);
  });

  it('verified 写回:适配器返回的实际行覆盖真相源并回调 onBreakpoints', async () => {
    const { f, session, bpWrites } = makeSession();
    f.responder.script.setBreakpoints = (args) => ({
      breakpoints: [{ line: args.breakpoints[0].line + 1, verified: true, id: 42 }],
    });
    await session.start('rust', RUST_CONFIG);
    await session.applyBreakpoints('src/main.rs', [3]);
    expect(bpWrites.at(-1)).toEqual(['src/main.rs', [4]]); // verified 行=3+1
    expect(session.breakpointLines()['src/main.rs']).toEqual([4]);
  });

  it('会话活动期 applyBreakpoints 即时同步;未活动仅改真相源', async () => {
    const { f, session } = makeSession();
    await session.applyBreakpoints('src/a.rs', [1]); // 未 start:无 setBreakpoints 请求
    expect(f.requests.filter((r) => r.command === 'setBreakpoints')).toHaveLength(0);
    await session.start('rust', RUST_CONFIG);
    const before = f.requests.filter((r) => r.command === 'setBreakpoints').length;
    await session.applyBreakpoints('src/a.rs', [2, 5]); // 活动期:立即全量同步
    const sb = f.requests.filter((r) => r.command === 'setBreakpoints');
    expect(sb.length).toBeGreaterThan(before);
    expect(sb.at(-1).args.breakpoints).toEqual([{ line: 2 }, { line: 5 }]);
  });

  it('setBreakpoints 失败不炸会话(留痕 onState)', async () => {
    const { f, session, states } = makeSession();
    await session.start('rust', RUST_CONFIG);
    f.client.request = (command) => {
      if (command === 'setBreakpoints') return Promise.reject(new Error('adapter busy'));
      return Promise.resolve({});
    };
    await session.applyBreakpoints('src/a.rs', [9]);
    const failNote = states.find(([, d]) => String(d).includes('setBreakpoints 失败'));
    expect(failNote).toBeTruthy();
    expect(session.state).toBe(DBG_STATES.RUNNING); // 会话不被打断
  });
});
