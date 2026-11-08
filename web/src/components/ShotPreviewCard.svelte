<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<script>
  import { fetchRaw } from '../lib/api.js';

  /** 镜头预览卡:制作进度 done 相产物(asset.path)的即时预览。
   *  取流同 MediaStage(fetch+Blob 带 Bearer 头);超限/失败显态不静默。
   *  video/image 两形态,其余种类不进本卡(见 progress-assets.js)。 */
  let { path, kind = 'video', bytes = 0 } = $props();

  let status = $state('loading'); // loading | ready | error
  let objectUrl = $state('');
  let errorText = $state('');

  // 旧 objectURL 回收:effect 清理在 URL 更新与组件卸载两个时机都会跑
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
    if (!p) {
      status = 'error';
      errorText = 'missing artifact path';
      return;
    }
    try {
      const resp = await fetchRaw(p);
      if (!resp.ok) {
        let msg = '';
        try {
          msg = (await resp.text()).slice(0, 200);
        } catch {
          /* body 读取失败时保留状态码语义 */
        }
        status = 'error';
        errorText = msg || `HTTP ${resp.status}`;
        return;
      }
      const blob = await resp.blob();
      // 卡内限额从宽:常规产物全预览;超大产物引导走文件树全视图
      if (blob.size > 256 * 1024 * 1024) {
        status = 'error';
        errorText = `artifact is ${Math.round(blob.size / 1048576)} MB — open it from the file tree`;
        return;
      }
      objectUrl = URL.createObjectURL(blob);
      status = 'ready';
    } catch (e) {
      status = 'error';
      errorText = String(e?.message || e);
    }
  }
</script>

<div class="spc-root">
  <div class="spc-head">
    <span class="spc-label">artifact preview</span>
    <span class="spc-path" title={path}>{path}</span>
    {#if typeof bytes === 'number' && bytes > 0}
      <span class="spc-bytes">{(bytes / 1048576).toFixed(1)} MB</span>
    {/if}
  </div>
  {#if status === 'loading'}
    <div class="spc-note">loading …</div>
  {:else if status === 'error'}
    <div class="spc-note spc-err">{errorText}</div>
  {:else if kind === 'image'}
    <img src={objectUrl} alt={path} />
  {:else}
    <!-- prettier-ignore -->
    <video controls preload="metadata" src={objectUrl}></video>
  {/if}
</div>

<style>
  .spc-root {
    margin: 6px 10px;
    padding: 8px 10px;
    border: 1px solid var(--border, #333);
    border-radius: 6px;
    background: var(--bg-card, #1d1d1d);
  }
  .spc-head {
    display: flex;
    align-items: baseline;
    gap: 8px;
    margin-bottom: 6px;
    min-width: 0;
  }
  .spc-label {
    color: var(--fg-dim, #777);
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    white-space: nowrap;
  }
  .spc-path {
    color: var(--fg-dim, #999);
    font-size: 11px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .spc-bytes {
    color: var(--fg-dim, #777);
    font-size: 11px;
    white-space: nowrap;
  }
  .spc-note {
    color: var(--fg-dim, #999);
    font-size: 11px;
    overflow-wrap: anywhere;
  }
  .spc-err {
    color: var(--err, #e07a7a);
  }
  video {
    max-width: 100%;
    max-height: 240px;
    outline: none;
  }
  img {
    max-width: 100%;
    max-height: 240px;
    object-fit: contain;
  }
</style>
