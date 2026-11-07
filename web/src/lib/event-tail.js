// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
/** NDJSON 事件文件尾随器(制作进度实时通道的前端半)。
 *
 *  通道形态(零新宿主端点):宿主 WS 的 fs_events 帧触发 + /api/files/raw
 *  的 Range 游标增量拉取——
 *  - 206:content-range 解出新游标(end+1),追加文本按 '\n' 切行(不完整
 *    尾行留缓冲),逐行 JSON 解析回调 onEvents;
 *  - 200:服务端未按 Range 回(Range 被忽略的形态)→ 整文件重放(游标归
 *    零语义,幂等);
 *  - 416:content-range 的 total 分辨两态——total===cursor 为无新数据
 *    (干净 no-op),total<cursor 为文件被截断/重写(游标归零全量重读);
 *  - 404:文件暂不存在(engine 未跑或已清)→ waiting 态,游标归零,文件
 *    复建后的首个 fs 事件会重新拉起;
 *  - 连续 2 次失败 → offline 显态(大法第六条出口口径:显式,不静默);
 *    恢复由下一个 fs 事件或手动 retry() 驱动,空闲期零轮询。
 *
 *  调用方契约:fsEvents store 的批量到达时调 noteFsEvent(paths[])(路径
 *  为 workdir 相对、'/' 分隔,与 fs_watch 帧一致);stop() 释放。 */

/** 连续失败多少次判 offline(§七判据:≥2 次显式断链) */
export const TAIL_OFFLINE_THRESHOLD = 2;

export class EventTail {
  /**
   * @param {{
   *   path: string,
   *   fetchImpl: (path: string, headers: Record<string, string>) => Promise<Response>,
   *   onEvents: (events: object[]) => void,
   *   onState: (state: 'idle'|'following'|'waiting'|'offline') => void,
   *   onParseError?: (line: string) => void,
   * }} opts
   */
  constructor(opts) {
    this.path = opts.path;
    this.fetchImpl = opts.fetchImpl;
    this.onEvents = opts.onEvents;
    this.onState = opts.onState;
    this.onParseError = opts.onParseError || null;
    this.cursor = 0;
    this.pending = '';
    this.consecutiveFailures = 0;
    this.state = 'idle';
    this.stopped = false;
    this.pulling = false;
    this.pendingPull = false;
  }

  _setState(s) {
    if (this.state !== s) {
      this.state = s;
      this.onState(s);
    }
  }

  start() {
    this.stopped = false;
    return this.pull();
  }

  stop() {
    this.stopped = true;
    this._setState('idle');
  }

  /** 手动恢复(offline 态的出口之一) */
  retry() {
    if (!this.stopped) return this.pull();
  }

  /** fs_events 批量到达:命中本文件或其所在目录(覆盖建文件瞬间)即拉取 */
  noteFsEvent(paths) {
    if (this.stopped) return;
    const hit = (paths || []).some((p) => {
      if (!p) return false;
      return (
        p === this.path ||
        this.path.startsWith(p.replace(/\/+$/, '') + '/') ||
        p.startsWith(this.path)
      );
    });
    if (hit) return this.pull();
  }

  /** 拉取入口:并发触发合并(进行中的一轮结束后补一轮,防丢最后一次写入) */
  async pull() {
    if (this.stopped) return;
    if (this.pulling) {
      this.pendingPull = true;
      return;
    }
    this.pulling = true;
    try {
      await this._pullOnce();
    } finally {
      this.pulling = false;
      if (this.pendingPull && !this.stopped) {
        this.pendingPull = false;
        this.pull();
      }
    }
  }

  async _pullOnce() {
    let resp;
    try {
      resp = await this.fetchImpl(this.path, { Range: `bytes=${this.cursor}-` });
    } catch (e) {
      this._noteFailure();
      return;
    }
    if (this.stopped) return;

    if (resp.status === 404) {
      // 文件暂不存在:归零等待复建(engine 每次构建重建该文件)
      this.cursor = 0;
      this.pending = '';
      this.consecutiveFailures = 0;
      this._setState('waiting');
      return;
    }

    if (resp.status === 416) {
      const total = this._totalFrom416(resp.headers.get('content-range'));
      if (total !== null && total < this.cursor) {
        // 文件被截断/重写:归零全量重读
        this.cursor = 0;
        this.pending = '';
        return this._pullOnce();
      }
      // total === cursor(或无法解析):无新数据,干净 no-op
      this.consecutiveFailures = 0;
      this._setState('following');
      return;
    }

    if (!resp.ok) {
      this._noteFailure();
      return;
    }

    if (resp.status === 200) {
      // 整文件重放(游标归零语义,幂等)
      this.cursor = 0;
      this.pending = '';
    }

    const text = await resp.text();
    if (this.stopped) return;

    const joined = this.pending + text;
    const lines = joined.split('\n');
    this.pending = lines.pop() || '';

    const events = [];
    for (const line of lines) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      try {
        events.push(JSON.parse(trimmed));
      } catch {
        if (this.onParseError) this.onParseError(trimmed.slice(0, 200));
      }
    }
    if (events.length) this.onEvents(events);

    if (resp.status === 206) {
      const cr = resp.headers.get('content-range') || '';
      const m = /bytes \d+-(\d+)\/(\d+)/.exec(cr);
      // 新游标 = end+1(content-range 的 end 为含端字节下标)
      this.cursor = m ? Number(m[1]) + 1 : this.cursor + new TextEncoder().encode(text).length;
    } else {
      this.cursor += new TextEncoder().encode(text).length;
    }
    this.consecutiveFailures = 0;
    this._setState('following');
  }

  _noteFailure() {
    this.consecutiveFailures += 1;
    if (this.consecutiveFailures >= TAIL_OFFLINE_THRESHOLD) {
      this._setState('offline');
    }
  }

  _totalFrom416(cr) {
    if (!cr) return null;
    const m = /bytes \*\/(\d+)/.exec(cr);
    return m ? Number(m[1]) : null;
  }
}
