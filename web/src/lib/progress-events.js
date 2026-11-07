// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
/** 制作进度事件的合并面(v2 WS typed 帧 × v1 Range 尾随 双通道冗余)。
 *
 *  两条通道(宿主解析帧/前端尾随)会对同一构建产生重复事件——去重键=
 *  事件格式 events/1.0 的单调 seq;无 seq 的事件(异源/手填文件)按到达
 *  序保留不去重(如实呈现)。新到置顶,容量裁剪。 */

/** 合并入站事件:按 seq 去重、新到置顶、容量裁剪 */
export function mergeProgressEvents(existing, incoming, cap = 50) {
  const seen = new Set(
    existing
      .map((e) => (e && typeof e.seq === 'number' ? e.seq : null))
      .filter((s) => s !== null)
  );
  const merged = [...existing];
  for (const e of incoming || []) {
    if (!e || typeof e !== 'object') continue;
    if (typeof e.seq === 'number') {
      if (seen.has(e.seq)) continue;
      seen.add(e.seq);
    }
    merged.unshift(e);
  }
  return merged.slice(0, cap);
}
