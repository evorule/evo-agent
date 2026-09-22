<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 侧面板:文件树(S0 静态占位;目录浏览在标准 IDE 基础阶段接入) -->
<div class="explorer">
  <div class="panel-title">资源管理器</div>
  <div class="workdir mono">
    <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
      <path d="M4 5a1 1 0 0 1 1-1h5l2 2h7a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5Z" />
    </svg>
    evo-agent(工作区)
  </div>
  <div class="tree" aria-hidden="true">
    <div class="node dir">agents</div>
    <div class="node dir">rules</div>
    <div class="node dir">src</div>
    <div class="node dir">tests</div>
    <div class="node file">README.md</div>
    <div class="node file">Cargo.toml</div>
  </div>
  <div class="placeholder">文件树目录浏览将在标准 IDE 基础阶段接入</div>
</div>

<style>
  .explorer {
    width: 220px;
    flex-shrink: 0;
    background: var(--sidebar-bg);
    border-right: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    overflow-y: auto;
  }
  .panel-title {
    font-size: 11px;
    font-weight: var(--fw-sb);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: rgba(255, 255, 255, 0.4);
    padding: var(--sp-sm) var(--sp-md);
  }
  .workdir {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: var(--sp-xs) var(--sp-md);
    font-size: var(--fs-xs);
    color: var(--sidebar-text);
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
  }
  .tree {
    padding: var(--sp-xs) 0;
  }
  .node {
    display: flex;
    align-items: center;
    padding: 0 var(--sp-md) 0 var(--sp-lg);
    height: 28px;
    font-size: var(--fs-sm);
    color: var(--sidebar-text);
    opacity: 0.75;
  }
  .node.dir::before,
  .node.file::before {
    content: '';
    width: 12px;
    height: 12px;
    margin-right: 6px;
    border-radius: 2px;
    flex-shrink: 0;
  }
  .node.dir::before {
    background: var(--brand);
    opacity: 0.55;
  }
  .node.file::before {
    border: 1.5px solid var(--text-muted);
  }
  .placeholder {
    margin-top: auto;
    padding: var(--sp-md);
    font-size: var(--fs-xs);
    color: var(--text-muted);
    line-height: var(--lh-normal);
  }
</style>
