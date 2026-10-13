// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// xterm 主题:与工作台 evorule-dark token 同源取色(monaco-setup 同款色板),
// 背景透明融入面板;明暗切换随主题板块(B11)落地后扩展。

export const terminalTheme = {
  background: 'rgba(0,0,0,0)', // 透明,底色由面板容器提供(--bg-card 同源)
  foreground: '#f1f5f9',
  cursor: '#2496ed',
  cursorAccent: '#0d1117',
  selectionBackground: '#1d63ed40',
  // 16 色板:深色底可读性调校(对齐 monaco-setup 明度阶梯)
  black: '#484f58',
  red: '#ff7b72',
  green: '#3fb950',
  yellow: '#d29922',
  blue: '#58a6ff',
  magenta: '#bc8cff',
  cyan: '#39c5cf',
  white: '#b1bac4',
  brightBlack: '#6e7681',
  brightRed: '#ffa198',
  brightGreen: '#56d364',
  brightYellow: '#e3b341',
  brightBlue: '#79c0ff',
  brightMagenta: '#d2a8ff',
  brightCyan: '#56d4dd',
  brightWhite: '#f1f5f9',
};
