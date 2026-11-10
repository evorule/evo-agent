// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
  plugins: [svelte()],
  // 相对 base:工作台由 evo-agent serve 托管在根路径,也兼容反代子路径
  base: './',
  build: {
    outDir: 'dist',
    // Monaco 体积较大,放宽分块告警
    chunkSizeWarningLimit: 4000,
  },
  server: {
    port: 5175,
    // 开发态代理到本地 evo-agent serve(API 与 WebSocket 同源)
    proxy: {
      '/api': { target: 'http://127.0.0.1:8081', ws: true },
      // G8 审批/问答回传/取消走 REST /agents/*(api.js 相对路径 fetch)——
      // 缺此行时 dev 态打到 vite 本身=404「提交失败」(D3 根因,生产同源无此面)
      '/agents': 'http://127.0.0.1:8081',
      '/admin': 'http://127.0.0.1:8081',
      '/health': 'http://127.0.0.1:8081',
    },
  },
});
