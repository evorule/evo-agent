// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 技能两段式确认 web 桥单测：壳 invoke 通道 / 拒绝呈现 / IPC 失败回落轮询。
import { describe, expect, it } from 'vitest';
import { resolveChallenge } from '../src/lib/skillsConfirm.js';

describe('resolveChallenge', () => {
  it('shell invoke path returns the shell verdict', async () => {
    const stub = {
      invoke: async (cmd, args) => {
        expect(cmd).toBe('skills_native_confirm');
        expect(args.challengeId).toBe('chg-1');
        expect(args.action).toBe('approve');
        return { executed: true, state: 'executed', result: { status: 'active' } };
      },
    };
    const out = await resolveChallenge({ challenge_id: 'chg-1', action: 'approve' }, stub);
    expect(out.executed).toBe(true);
    expect(out.result.status).toBe('active');
  });

  it('invoke denial surfaces denied without polling', async () => {
    const stub = { invoke: async () => ({ executed: false, state: 'denied' }) };
    const out = await resolveChallenge({ challenge_id: 'chg-2', action: 'revoke' }, stub);
    expect(out.executed).toBe(false);
    expect(out.state).toBe('denied');
  });

  it('invoke failure falls back to polling until terminal', async () => {
    const orig = global.fetch;
    global.fetch = async () => ({ ok: true, json: async () => ({ state: 'denied' }) });
    try {
      const out = await resolveChallenge(
        { challenge_id: 'chg-3', action: 'approve' },
        {
          invoke: async () => {
            throw new Error('permission missing');
          },
        },
      );
      expect(out.executed).toBe(false);
      expect(out.state).toBe('denied');
    } finally {
      global.fetch = orig;
    }
  });
});
