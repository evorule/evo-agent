// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
/** 媒体视图路由与限额的纯函数面(stores 路由与 MediaStage 组件共用)。
 *
 *  分类表与 serve 端 /api/files/raw 的媒体 MIME 族一一对应;文本类
 *  (svg/json/md 等)刻意不在表内——可编辑文本仍走编辑器,媒体视图只
 *  接管无编辑语义的二进制。未知扩展回落 null(编辑器照旧)。 */

const CATEGORY_BY_EXT = {
  mp4: 'video',
  m4v: 'video',
  webm: 'video',
  mov: 'video',
  mkv: 'video',
  avi: 'video',
  mp3: 'audio',
  wav: 'audio',
  flac: 'audio',
  ogg: 'audio',
  m4a: 'audio',
  aac: 'audio',
  png: 'image',
  jpg: 'image',
  jpeg: 'image',
  gif: 'image',
  webp: 'image',
  bmp: 'image',
  ico: 'image',
};

/** 媒体预览 blob 化的体积上限(字节):超过走显式降级卡,不静默截断 */
export const MEDIA_BLOB_LIMIT = 64 * 1024 * 1024;

/** 路径 → 媒体类别('video'|'audio'|'image'),非媒体返回 null */
export function mediaCategoryFor(path) {
  const name = String(path || '').split(/[\\/]/).pop() || '';
  const dot = name.lastIndexOf('.');
  if (dot <= 0) return null;
  const ext = name.slice(dot + 1).toLowerCase();
  return CATEGORY_BY_EXT[ext] || null;
}

/** 该路径是否应在打开时路由到媒体视图(而非文本编辑器) */
export function shouldRouteToMediaStage(path) {
  return mediaCategoryFor(path) !== null;
}

/** 体积是否超出 blob 化上限(非数值输入按未超处理,交由后续流程) */
export function isOversize(sizeBytes) {
  return typeof sizeBytes === 'number' && sizeBytes > MEDIA_BLOB_LIMIT;
}
