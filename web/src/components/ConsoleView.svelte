<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- console 审计页同源嵌入(B1 治理叠加 I):serve 静态托管 /console,中栏全宽
     iframe。宿主(App.svelte center 区)常挂载本组件、以 display 切换显示——
     iframe 销毁会丢 console 登录态(同 BottomPanel TerminalView 先例)。 -->

<div class="console-view">
  <iframe class="console-frame" src="/console/audit" title="console 审计页"></iframe>
</div>

<style>
  .console-view {
    flex: 1;
    min-height: 0;
    display: flex;
  }
  .console-frame {
    flex: 1;
    width: 100%;
    border: none;
    background: var(--bg);
  }
</style>
