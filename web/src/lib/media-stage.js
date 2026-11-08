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
  glb: 'model',
  gltf: 'model',
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

/** 浏览器原生解不了的容器黑名单(mov/mkv/avi):Blob URL 给到 <video> 会
 *  静默黑屏——显态裁定,不许走「能播就播」的侥幸路径。mp4/webm 直连。 */
export const UNPLAYABLE_VIDEO_EXTS = ['mov', 'mkv', 'avi'];

/** 该路径是否为黑名单容器(大小写不敏感,兼 / \ 分隔) */
export function isUnplayableVideo(path) {
  const name = String(path || '').split(/[\\/]/).pop() || '';
  const dot = name.lastIndexOf('.');
  if (dot <= 0) return false;
  return UNPLAYABLE_VIDEO_EXTS.includes(name.slice(dot + 1).toLowerCase());
}

/** 黑名单容器的代理播放路径:<stem>.preview.mp4(与源文件同目录) */
export function proxyPathFor(path) {
  const p = String(path || '').replace(/\\/g, '/');
  const dot = p.lastIndexOf('.');
  const stem = dot > p.lastIndexOf('/') ? p.slice(0, dot) : p;
  return `${stem}.preview.mp4`;
}

/** 播放目标裁定(纯函数,探测经注入的 checker 可测)。
 *  返回 {target:'original', path} 直连 |
 *        {target:'proxy', path, originalPath} 切代理 |
 *        {target:'error', path, proxy, reason} 显态报错。
 *  checker(path) → {ok: HTTP 2xx?, playable: Content-Type 为 video/mp4?}。 */
export async function resolvePlaybackTarget(path, checker) {
  if (!isUnplayableVideo(path)) {
    return { target: 'original', path };
  }
  const proxy = proxyPathFor(path);
  let probe;
  try {
    probe = await checker(proxy);
  } catch {
    probe = { ok: false };
  }
  if (!probe || !probe.ok) {
    return { target: 'error', path, proxy, reason: 'no-proxy' };
  }
  if (!probe.playable) {
    return { target: 'error', path, proxy, reason: 'proxy-not-playable' };
  }
  return { target: 'proxy', path: proxy, originalPath: path };
}
