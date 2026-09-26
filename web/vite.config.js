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
      '/admin': 'http://127.0.0.1:8081',
      '/health': 'http://127.0.0.1:8081',
    },
  },
});
