// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// B16 MCP 工作台 store 测试:动作(权威回填/关闭态/失败横幅)、编辑器往返
// 解析、状态映射与徽标派生。api.js 全 mock(同 settings.test.js 模式)。
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { get } from 'svelte/store';

vi.mock('../src/lib/api.js', () => ({
  mcpListServers: vi.fn(),
  mcpPutServers: vi.fn(),
  mcpRestartServer: vi.fn(),
  mcpStatus: vi.fn(),
}));

import {
  mcpState,
  loadMcp,
  toggleServerDisabled,
  toggleToolAllow,
  restartServer,
  applyServersText,
  serversToPayload,
  serversToText,
  parseServersText,
  statusKind,
  statusLabel,
  badgeCounts,
} from '../src/lib/mcp.js';
import { mcpListServers, mcpPutServers, mcpRestartServer } from '../src/lib/api.js';

const VIEW_ENTRY = {
  name: 'fs',
  command: 'npx',
  args: ['-y', 'server-filesystem'],
  env_keys: ['API_KEY'],
  disabled: false,
  timeout_secs: 30,
  always_allow: ['read_file'],
  status: 'connected',
  error: null,
  tools: [
    { name: 'mcp_fs_read_file', server_tool: 'read_file', description: '读文件', allowed: true },
    { name: 'mcp_fs_write_file', server_tool: 'write_file', description: '写文件', allowed: false },
  ],
};
const MERGED = { servers: [VIEW_ENTRY], config_path: 'D:/w/.evo/mcp.toml' };

function errWith(status, msg = `HTTP ${status}`) {
  return Object.assign(new Error(msg), { status });
}

function resetStore() {
  mcpState.set({ loading: false, enabled: null, servers: [], configPath: '', error: '' });
}

beforeEach(() => {
  resetStore();
  vi.clearAllMocks();
});

describe('serversToPayload(视图 → PUT 载荷)', () => {
  it('剥离派生字段(status/error/tools/env_keys),env 一律省略', () => {
    const p = serversToPayload([VIEW_ENTRY]);
    expect(p).toEqual([
      {
        name: 'fs',
        command: 'npx',
        args: ['-y', 'server-filesystem'],
        disabled: false,
        timeout_secs: 30,
        always_allow: ['read_file'],
      },
    ]);
    expect('env' in p[0]).toBe(false);
    expect('status' in p[0]).toBe(false);
    expect('tools' in p[0]).toBe(false);
  });

  it('缺省字段补默认(args=[]/timeout_secs=60/always_allow=[]/disabled=false)', () => {
    const p = serversToPayload([{ name: 'x', command: 'echo' }]);
    expect(p[0]).toEqual({
      name: 'x',
      command: 'echo',
      args: [],
      disabled: false,
      timeout_secs: 60,
      always_allow: [],
    });
  });
});

describe('编辑器往返(serversToText / parseServersText)', () => {
  it('预填文本解析回同形载荷', () => {
    const text = serversToText([VIEW_ENTRY]);
    const r = parseServersText(text);
    expect(r.error).toBeUndefined();
    expect(r.servers).toEqual(serversToPayload([VIEW_ENTRY]));
  });

  it('JSON 语法错误 / 非 servers 形态 / 条目坏形逐档拦截', () => {
    expect(parseServersText('{oops')).toHaveProperty('error');
    expect(parseServersText('{"a":1}')).toHaveProperty('error');
    expect(parseServersText('{"servers": "x"}')).toHaveProperty('error');
    expect(parseServersText('{"servers":[{}]}').error).toContain('name');
    expect(parseServersText('{"servers":[{"name":"a"}]}').error).toContain('command');
    expect(
      parseServersText('{"servers":[{"name":"a","command":"e","timeout_secs":0}]}').error,
    ).toContain('timeout_secs');
    expect(
      parseServersText('{"servers":[{"name":"a","command":"e","args":"x"}]}').error,
    ).toContain('args');
    expect(
      parseServersText('{"servers":[{"name":"a","command":"e","always_allow":"x"}]}').error,
    ).toContain('always_allow');
    expect(parseServersText('{"servers":[{"name":"a","command":"e","env":[]}]}').error).toContain(
      'env',
    );
  });

  it('接受 {servers} 对象与裸数组两种形态;显式 env 对象合法(用户覆盖用)', () => {
    expect(parseServersText('{"servers":[{"name":"a","command":"e"}]}').servers).toHaveLength(1);
    expect(parseServersText('[{"name":"a","command":"e"}]').servers).toHaveLength(1);
    expect(parseServersText('{"servers":[{"name":"a","command":"e","env":{"K":"V"}}]}').servers[0].env).toEqual({ K: 'V' });
  });
});

describe('statusKind / statusLabel / badgeCounts', () => {
  it('状态点三色映射(connected 绿/starting|unconnected 黄/failed 红/disabled 灰)', () => {
    expect(statusKind('connected')).toBe('success');
    expect(statusKind('starting')).toBe('warning');
    expect(statusKind('unconnected')).toBe('warning');
    expect(statusKind('failed')).toBe('danger');
    expect(statusKind('disabled')).toBe('muted');
    expect(statusKind('whatever')).toBe('warning');
  });

  it('中文标签;未知状态原样透出', () => {
    expect(statusLabel('connected')).toBe('已连接');
    expect(statusLabel('disabled')).toBe('已停用');
    expect(statusLabel('mystery')).toBe('mystery');
  });

  it('徽标计数:工具数只累计 connected server', () => {
    const servers = [
      { status: 'connected', disabled: false, tools: [{}, {}] },
      { status: 'failed', disabled: false, tools: [{}, {}] },
      { status: 'connected', disabled: false, tools: [] },
      { status: 'disabled', disabled: true, tools: [] },
    ];
    expect(badgeCounts(servers)).toEqual({ total: 4, connected: 2, failed: 1, disabled: 1, tools: 2 });
    expect(badgeCounts(undefined)).toEqual({ total: 0, connected: 0, failed: 0, disabled: 0, tools: 0 });
  });
});

describe('loadMcp', () => {
  it('成功 → enabled=true + servers/configPath 回填', async () => {
    mcpListServers.mockResolvedValue(MERGED);
    await loadMcp();
    const s = get(mcpState);
    expect(s.enabled).toBe(true);
    expect(s.servers).toEqual(MERGED.servers);
    expect(s.configPath).toBe(MERGED.config_path);
    expect(s.loading).toBe(false);
    expect(s.error).toBe('');
  });

  it('503 → enabled=false 关闭态,无错误横幅', async () => {
    mcpListServers.mockRejectedValue(errWith(503));
    await loadMcp();
    const s = get(mcpState);
    expect(s.enabled).toBe(false);
    expect(s.error).toBe('');
  });

  it('其他失败 → error 横幅,enabled 不翻转', async () => {
    mcpState.set({ loading: false, enabled: true, servers: [], configPath: '', error: '' });
    mcpListServers.mockRejectedValue(new Error('网络中断'));
    await loadMcp();
    const s = get(mcpState);
    expect(s.enabled).toBe(true);
    expect(s.error).toBe('网络中断');
  });
});

describe('toggleServerDisabled / toggleToolAllow(PUT 权威回填)', () => {
  beforeEach(() => {
    mcpState.set({ loading: false, enabled: true, servers: [VIEW_ENTRY], configPath: 'p', error: '' });
  });

  it('启停翻转进 PUT 载荷,响应合并视图回填 store', async () => {
    mcpPutServers.mockResolvedValue(MERGED);
    await toggleServerDisabled('fs');
    expect(mcpPutServers).toHaveBeenCalledTimes(1);
    const sent = mcpPutServers.mock.calls[0][0];
    expect(sent[0].disabled).toBe(true);
    expect('env' in sent[0]).toBe(false, '程序化写回必须省略 env 字段');
    expect(get(mcpState).servers).toEqual(MERGED.servers);
  });

  it('未知名翻转无效果(仍调用 PUT,载荷原样)', async () => {
    mcpPutServers.mockResolvedValue(MERGED);
    await toggleServerDisabled('ghost');
    expect(mcpPutServers.mock.calls[0][0][0].disabled).toBe(false);
  });

  it('PUT 失败 → error 横幅且 store servers 不变', async () => {
    mcpPutServers.mockRejectedValue(Object.assign(new Error('HTTP 400'), { status: 400 }));
    const ok = await toggleServerDisabled('fs');
    expect(ok).toBe(false);
    const s = get(mcpState);
    expect(s.error).toBe('HTTP 400');
    expect(s.servers[0].disabled).toBe(false);
  });

  it('alwaysAllow 勾选加入白名单,再勾取消移除', async () => {
    // 响应即权威回填:第一次 PUT 的响应视图应已含 write_file(模拟 serve 重载后状态)
    const MERGED_ALLOWED = {
      servers: [{ ...VIEW_ENTRY, always_allow: ['read_file', 'write_file'] }],
      config_path: 'p',
    };
    mcpPutServers.mockResolvedValueOnce(MERGED_ALLOWED).mockResolvedValueOnce(MERGED);
    await toggleToolAllow('fs', 'write_file');
    expect(mcpPutServers.mock.calls[0][0][0].always_allow).toEqual(['read_file', 'write_file']);
    // 回填后 store 白名单含 write_file,再勾 = 移除
    await toggleToolAllow('fs', 'write_file');
    expect(mcpPutServers.mock.calls[1][0][0].always_allow).toEqual(['read_file']);
  });
});

describe('restartServer', () => {
  beforeEach(() => {
    mcpState.set({ loading: false, enabled: true, servers: [VIEW_ENTRY], configPath: 'p', error: '' });
  });

  it('成功 → POST 后重拉合并视图', async () => {
    mcpRestartServer.mockResolvedValue({ name: 'fs', status: 'connected', tools_registered: 2 });
    mcpListServers.mockResolvedValue(MERGED);
    const ok = await restartServer('fs');
    expect(ok).toBe(true);
    expect(mcpRestartServer).toHaveBeenCalledWith('fs');
    expect(mcpListServers).toHaveBeenCalledTimes(1);
    expect(get(mcpState).error).toBe('');
  });

  it('502 重连失败 → 重拉呈现 failed 态 + 错误横幅', async () => {
    mcpRestartServer.mockRejectedValue(Object.assign(new Error('reconnect failed'), { status: 502 }));
    mcpListServers.mockResolvedValue(MERGED);
    const ok = await restartServer('fs');
    expect(ok).toBe(false);
    expect(mcpListServers).toHaveBeenCalledTimes(1);
    expect(get(mcpState).error).toBe('reconnect failed');
  });
});

describe('applyServersText', () => {
  it('解析失败返回错误消息且不触发 PUT', async () => {
    const msg = await applyServersText('{bad');
    expect(msg).toContain('JSON 解析失败');
    expect(mcpPutServers).not.toHaveBeenCalled();
  });

  it('合法文本 → PUT + store 回填,返回 null', async () => {
    mcpPutServers.mockResolvedValue(MERGED);
    const msg = await applyServersText('{"servers":[{"name":"a","command":"e"}]}');
    expect(msg).toBeNull();
    expect(mcpPutServers).toHaveBeenCalledTimes(1);
    expect(get(mcpState).servers).toEqual(MERGED.servers);
  });
});

describe('loadMcp 关闭态后动作恢复探测', () => {
  it('PUT 成功响应即权威,enabled 翻回 true', async () => {
    mcpState.set({ loading: false, enabled: false, servers: [], configPath: '', error: '' });
    mcpPutServers.mockResolvedValue(MERGED);
    await applyServersText('{"servers":[{"name":"a","command":"e"}]}');
    expect(get(mcpState).enabled).toBe(true);
  });
});
