// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 公告面纯函数(批 J1 段 2):排序 / 裁剪 / 徽标映射。
// 数据源 = GET /api/notices(服务端已排序;此处重排幂等,兼作离线防御)。

/** 缺省可见条数(「全部展开」前) */
export const DEFAULT_VISIBLE = 2;

/** 排序:pinned 置顶 → date 降序(ISO 字典序即时间序)→ 原序稳定。
 *  返回新数组,不改入参。 */
export function sortNotices(list) {
  return [...(Array.isArray(list) ? list : [])].sort(
    (a, b) => (b.pinned ? 1 : 0) - (a.pinned ? 1 : 0) || String(b.date).localeCompare(String(a.date)),
  );
}

/** 展示裁剪:缺省取最新 DEFAULT_VISIBLE 条,展开全部。 */
export function sliceForDisplay(list, expanded) {
  const arr = Array.isArray(list) ? list : [];
  return expanded ? arr : arr.slice(0, DEFAULT_VISIBLE);
}

/** level → 徽标 CSS 类;未知值兜底 info(与后端契约词表 info/warn/feature 对应)。 */
export function levelClass(level) {
  return level === 'feature' || level === 'warn' ? `lv-${level}` : 'lv-info';
}
