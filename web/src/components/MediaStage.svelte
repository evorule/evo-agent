<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<script>
  import { MEDIA_BLOB_LIMIT, isOversize, mediaCategoryFor, resolvePlaybackTarget } from '../lib/media-stage.js';
  import { fetchRaw } from '../lib/api.js';

  /** 媒体视图主体:按扩展名分类消费 /api/files/raw 的原始字节。
   *  鉴权两档制的 v1=fetch+Blob(带 Bearer 头→objectURL,全量缓冲后
   *  seek 完整可用,丢的只是渐进流式);超限走显式降级卡不静默。 */
  let { path } = $props();

  let status = $state('loading'); // loading | ready | error
  let category = $state(null);
  let objectUrl = $state('');
  let errorText = $state('');
  let errorStatus = $state(0);
  let sizeBytes = $state(0);
  let zoom = $state(1);
  let panX = $state(0);
  let panY = $state(0);
  let dragFrom = $state(null);

  // 旧 objectURL 的回收:effect 清理在 URL 更新与组件卸载两个时机都会跑
  $effect(() => {
    const url = objectUrl;
    return () => {
      if (url) URL.revokeObjectURL(url);
    };
  });

  $effect(() => {
    load(path);
  });

  async function load(p) {
    status = 'loading';
    errorText = '';
    errorStatus = 0;
    sizeBytes = 0;
    zoom = 1;
    panX = 0;
    panY = 0;
    category = mediaCategoryFor(p);
    if (!category) {
      status = 'error';
      errorText = 'unsupported media type';
      return;
    }
    try {
      // 播放目标裁定:mov/mkv/avi 先 HEAD 探 .preview.mp4 代理,有则切,无则显态
      const target = await resolvePlaybackTarget(p, async (probePath) => {
        const head = await fetchRaw(probePath, {}, 'HEAD');
        const ct = (head.headers.get('content-type') || '').toLowerCase();
        return { ok: head.ok, playable: ct === 'video/mp4' };
      });
      if (target.target === 'error') {
        status = 'error';
        errorText =
          target.reason === 'proxy-not-playable'
            ? `transcoded proxy ${target.proxy} exists but is not a playable video`
            : `${p} can't be decoded in-editor (mov/mkv/avi) and no ${target.proxy} proxy found — generate one to enable preview`;
        return;
      }
      const resp = await fetchRaw(target.path);
      if (!resp.ok) {
        let msg = '';
        try {
          msg = (await resp.text()).slice(0, 300);
        } catch {
          /* body 读取失败时保留状态码语义 */
        }
        status = 'error';
        errorStatus = resp.status;
        errorText = msg || `HTTP ${resp.status}`;
        return;
      }
      const declared = Number(resp.headers.get('content-length') || '0');
      if (isOversize(declared)) {
        status = 'error';
        errorText = `file is ${Math.round(declared / 1048576)} MB - over the ${Math.round(
          MEDIA_BLOB_LIMIT / 1048576
        )} MB in-editor preview limit; open it with an external player`;
        return;
      }
      const blob = await resp.blob();
      if (isOversize(blob.size)) {
        status = 'error';
        errorText = `file is ${Math.round(blob.size / 1048576)} MB - over the ${Math.round(
          MEDIA_BLOB_LIMIT / 1048576
        )} MB in-editor preview limit; open it with an external player`;
        return;
      }
      sizeBytes = blob.size;
      objectUrl = URL.createObjectURL(blob);
      status = 'ready';
    } catch (e) {
      status = 'error';
      errorText = String(e?.message || e);
    }
  }

  function onWheel(e) {
    e.preventDefault();
    const factor = e.deltaY < 0 ? 1.1 : 0.9;
    zoom = Math.min(8, Math.max(0.1, zoom * factor));
  }

  function onDown(e) {
    dragFrom = { x: e.clientX - panX, y: e.clientY - panY };
    e.currentTarget.setPointerCapture(e.pointerId);
  }

  function onMove(e) {
    if (!dragFrom) return;
    panX = e.clientX - dragFrom.x;
    panY = e.clientY - dragFrom.y;
  }

  function onUp() {
    dragFrom = null;
  }

  function resetView() {
    zoom = 1;
    panX = 0;
    panY = 0;
  }
</script>

<div class="media-stage">
  {#if status === 'loading'}
    <div class="ms-card ms-note">Loading {path} …</div>
  {:else if status === 'error'}
    <div class="ms-card ms-error">
      <div class="ms-title">Cannot preview {path}</div>
      {#if errorStatus}
        <div class="ms-sub">HTTP {errorStatus}</div>
      {/if}
      <div class="ms-sub">{errorText}</div>
    </div>
  {:else if category === 'video'}
    <!-- prettier-ignore -->
    <video controls preload="metadata" src={objectUrl}></video>
  {:else if category === 'audio'}
    <!-- prettier-ignore -->
    <audio controls src={objectUrl}></audio>
  {:else if category === 'image'}
    <img
      src={objectUrl}
      alt={path}
      draggable="false"
      style="transform: translate({panX}px, {panY}px) scale({zoom});"
      onwheel={onWheel}
      onpointerdown={onDown}
      onpointermove={onMove}
      onpointerup={onUp}
      onpointerleave={onUp}
      ondblclick={resetView}
    />
    <div class="ms-hint">scroll = zoom · drag = pan · double-click = reset</div>
  {:else if category === 'model'}
    <div class="ms-card ms-error">
      <div class="ms-title">3D model preview is not built in yet</div>
      <div class="ms-sub">
        {path} · {sizeBytes} bytes · rendering glTF assets needs a three.js
        dependency introduction (queued as a separate batch); use an external
        viewer meanwhile.
      </div>
    </div>
  {/if}
</div>

<style>
  .media-stage {
    display: flex;
    flex: 1;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 8px;
    min-height: 0;
    overflow: auto;
    padding: 16px;
    background: var(--bg-page, #141414);
  }
  .ms-card {
    max-width: 560px;
    padding: 16px 20px;
    border: 1px solid var(--border, #333);
    border-radius: 8px;
    background: var(--bg-card, #1d1d1d);
  }
  .ms-note {
    color: var(--fg-dim, #999);
  }
  .ms-error .ms-title {
    margin-bottom: 8px;
    font-weight: 600;
    color: var(--fg, #ddd);
    overflow-wrap: anywhere;
  }
  .ms-sub {
    color: var(--fg-dim, #999);
    font-size: 12px;
    overflow-wrap: anywhere;
  }
  video {
    max-width: 100%;
    max-height: 100%;
    outline: none;
  }
  audio {
    width: min(520px, 90%);
  }
  img {
    max-width: none;
    cursor: grab;
    transform-origin: center center;
    user-select: none;
  }
  img:active {
    cursor: grabbing;
  }
  .ms-hint {
    color: var(--fg-dim, #777);
    font-size: 11px;
  }
</style>
