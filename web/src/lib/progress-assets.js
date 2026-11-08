// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
/** 制作进度事件 → 产物资产派生面(镜头预览卡数据源,纯函数零副作用)。
 *
 *  events/1.0 契约:产物由 done 相事件的 `asset` 节携带——
 *  `{path, kind, bytes, blake3}`;path 是相对项目 build/ 目录的文件名
 *  (engine 硬契约:正式产物恒落 build/;事件写在项目根的
 *  build.progress.ndjson)。本模块把 (ndjson 路径, asset.path) 归一成
 *  工作区相对路径,供前端走 /api/files/raw 取原始字节。 */

/** 前端预览直接可播/可显的资产种类;其余(glb/ass/srt/docx…)显态不预览 */
const PREVIEWABLE_KINDS = {
  mp4: 'video',
  webm: 'video',
  png: 'image',
  jpg: 'image',
  jpeg: 'image',
  webp: 'image',
  gif: 'image',
};

/** asset.kind → 预览种类('video'|'image'),不可预览回落 null */
export function previewKindFor(kind) {
  return PREVIEWABLE_KINDS[String(kind || '').toLowerCase()] || null;
}

/** 事件列表 → 最新一条带可预览产物的 done 事件(events 新到置顶,首个即最新)。
 *  无产物/产物种类不可预览 → null(调用方不渲染预览卡)。 */
export function extractAssetEvent(events) {
  for (const e of events || []) {
    if (!e || typeof e !== 'object') continue;
    const a = e.asset;
    if (!a || typeof a !== 'object') continue;
    if (typeof a.path !== 'string' || !a.path.trim()) continue;
    if (previewKindFor(a.kind) === null) continue;
    return e;
  }
  return null;
}

/** (ndjson 路径, asset.path) → 工作区相对产物路径。
 *  归一规则:ndjson 所在目录 + `build/` + asset.path 文件名——
 *  事件写在项目根(`projects/x/build.progress.ndjson`)与写在 build/
 *  内(`projects/x/build/build.progress.ndjson`)两种布局都归一到
 *  `projects/x/build/<file>`。任一输入为空 → null(调用方显态)。 */
export function buildAssetPath(ndjsonPath, assetPath) {
  if (!ndjsonPath || !assetPath) return null;
  const nd = String(ndjsonPath).replace(/\\/g, '/');
  let dir = nd.slice(0, nd.lastIndexOf('/') + 1);
  if (!dir) return null;
  // ndjson 已在 build/ 内的布局不再追加一层(build/build 会指空)
  if (!dir.endsWith('build/')) dir += 'build/';
  const name = String(assetPath).split(/[\\/]/).pop();
  if (!name) return null;
  return `${dir}${name}`;
}
