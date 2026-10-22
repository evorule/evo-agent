// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 主题唯一真相源:颜色类 token 明暗两值——CSS 变量(data-theme)/Monaco
// defineTheme/xterm theme 三通道全部从此派生(消除 monaco-setup 手工双源)。
//
// 纪律:dark 值=app.css :root 现值原样(不改动现有深色视觉);
// light 为独立重设计(非反转;品牌色不动;语义色 hue 一致;WCAG 校验)。
// Monaco 永不读 DOM CSS 变量(colors 仅收 hex)。

/** 主题模式:'auto'(跟随系统) | 'evorule-dark' | 'evorule-light' */
export const THEME_MODES = ['auto', 'evorule-dark', 'evorule-light'];

/** 颜色类 token 明暗两值(dark=app.css :root 现值原样) */
export const COLOR_TOKENS = {
  dark: {
    'bg-page': '#0b1929',
    'bg-card': '#0d1117',
    'bg-header': '#09101f',
    'bg-hover': 'rgba(255, 255, 255, 0.06)',
    'bg-active': 'rgba(29, 99, 237, 0.15)',
    'bg-input': '#0d1117',
    border: 'rgba(255, 255, 255, 0.08)',
    'border-strong': 'rgba(255, 255, 255, 0.15)',
    'text-primary': '#f1f5f9',
    'text-secondary': '#94a3b8',
    'text-muted': '#64748b',
    'text-inverse': '#0f172a',
    success: '#2ecc71',
    danger: '#e74c3c',
    warning: '#f39c12',
    info: '#3b82f6',
    brand: '#1d63ed',
    'brand-hover': '#1557d0',
    navy: '#09101f',
    'sidebar-bg': '#09101f',
    'sidebar-text': 'rgba(255, 255, 255, 0.65)',
    'sidebar-text-active': '#ffffff',
    'git-added': '#2ecc71',
    'git-modified': '#f39c12',
    'git-deleted': '#e74c3c',
    'git-renamed': '#3b82f6',
    'editor-bg': '#0d1117',
    'editor-line-number': '#64748b',
    'editor-selection': '#1d63ed40',
    'editor-cursor': '#2496ed',
  },
  light: {
    'bg-page': '#f6f8fa',
    'bg-card': '#ffffff',
    'bg-header': '#ffffff',
    'bg-hover': 'rgba(9, 16, 31, 0.05)',
    'bg-active': 'rgba(29, 99, 237, 0.10)',
    'bg-input': '#ffffff',
    border: 'rgba(9, 16, 31, 0.10)',
    'border-strong': 'rgba(9, 16, 31, 0.20)',
    'text-primary': '#0f172a',
    'text-secondary': '#475569',
    'text-muted': '#94a3b8',
    'text-inverse': '#f1f5f9',
    success: '#1e8e4e',
    danger: '#c0392b',
    warning: '#b9770e',
    info: '#1d63ed',
    brand: '#1d63ed',
    'brand-hover': '#1557d0',
    navy: '#09101f',
    'sidebar-bg': '#f0f4f8',
    'sidebar-text': 'rgba(9, 16, 31, 0.65)',
    'sidebar-text-active': '#0f172a',
    'git-added': '#1e8e4e',
    'git-modified': '#b9770e',
    'git-deleted': '#c0392b',
    'git-renamed': '#1d63ed',
    'editor-bg': '#ffffff',
    'editor-line-number': '#94a3b8',
    'editor-selection': '#1d63ed26',
    'editor-cursor': '#1d63ed',
  },
};

/** Monaco colors 键 → token 名(MONACO_MAP 引用的 token 必须存在于两套色板——单测守卫) */
export const MONACO_MAP = {
  'editor.background': 'editor-bg',
  'editor.foreground': 'text-primary',
  'editorLineNumber.foreground': 'editor-line-number',
  'editor.selectionBackground': 'editor-selection',
  'editorCursor.foreground': 'editor-cursor',
  'editorGutter.background': 'editor-bg',
  'editorIndentGuide.background': 'border',
  'scrollbarSlider.background': 'border-strong',
  'scrollbarSlider.hoverBackground': 'bg-hover',
  'editorWidget.background': 'bg-card',
  'editorSuggestWidget.background': 'bg-card',
  'editorHoverWidget.background': 'bg-card',
  'input.background': 'bg-input',
  'dropdown.background': 'bg-card',
  'focusBorder': 'brand',
};

/** 由 token 表拼 Monaco IStandaloneThemeData.colors(按主题名) */
export function monacoColorsFor(mode) {
  const resolved = resolveMode(mode);
  const tokens = COLOR_TOKENS[resolved];
  const colors = {};
  for (const [monacoKey, tokenName] of Object.entries(MONACO_MAP)) {
    const v = tokens[tokenName];
    if (v != null) colors[monacoKey] = v;
  }
  return colors;
}

/** Monaco rules:dark 走 base 默认(现视觉零变化);light 补核心 6 类 */
export function monacoRulesFor(mode) {
  const resolved = resolveMode(mode);
  if (resolved === 'dark') return [];
  const t = COLOR_TOKENS.light;
  return [
    { token: 'comment', foreground: t['text-muted'], fontStyle: 'italic' },
    { token: 'string', foreground: '#0a7d3c' },
    { token: 'keyword', foreground: '#9b1fe8' },
    { token: 'number', foreground: '#0b6fb8' },
    { token: 'type', foreground: '#0b6fb8' },
    { token: 'identifier', foreground: t['text-primary'] },
  ];
}

/** Monaco base 主题名 */
export function monacoBaseFor(mode) {
  return resolveMode(mode) === 'light' ? 'vs' : 'vs-dark';
}

/** 模式解析:auto→跟随系统 */
export function resolveMode(mode) {
  if (mode === 'evorule-light') return 'light';
  if (mode === 'evorule-dark') return 'dark';
  // auto / 未知值:跟随系统
  if (typeof window !== 'undefined' && window.matchMedia && window.matchMedia('(prefers-color-scheme: light)').matches) {
    return 'light';
  }
  return 'dark';
}

/** CSS 变量注入文本(data-theme 由调用方设) */
export function cssVariablesFor(mode) {
  const resolved = resolveMode(mode);
  const tokens = COLOR_TOKENS[resolved];
  return Object.entries(tokens)
    .map(([k, v]) => `  --${k}: ${v};`)
    .join('\n');
}

/** xterm 主题(集成终端挂点;从 token 表派生)。mode 传已解析的 'dark'|'light' */
export function xtermThemeFor(resolved) {
  const t = COLOR_TOKENS[resolved] || COLOR_TOKENS.dark;
  return {
    background: 'rgba(0,0,0,0)',
    foreground: t['text-primary'],
    cursor: t['editor-cursor'],
    cursorAccent: t['bg-card'],
    selectionBackground: t['editor-selection'],
  };
}

/** localStorage 镜像键(index.html FOUC 内联脚本同键) */
export const THEME_MIRROR_KEY = 'evo_theme_mode';

export function readStoredMode() {
  try {
    const v = localStorage.getItem(THEME_MIRROR_KEY);
    return THEME_MODES.includes(v) ? v : 'auto';
  } catch {
    return 'auto';
  }
}

export function storeMode(mode) {
  try {
    localStorage.setItem(THEME_MIRROR_KEY, mode);
  } catch {
    /* 不可用:会话内 */
  }
}

/** xterm 主题变化订阅者(集成终端挂点预留) */
const themeChangeSubscribers = new Set();

export function onThemeChange(fn) {
  themeChangeSubscribers.add(fn);
  return () => themeChangeSubscribers.delete(fn);
}

/** light 语法 rules 核心 6 类(设计档 §一) */
function lightRules() {
  return [
    { token: 'comment', foreground: '#6a737d', fontStyle: 'italic' },
    { token: 'string', foreground: '#0a7d3c' },
    { token: 'keyword', foreground: '#7c3aed' },
    { token: 'number', foreground: '#0b6fb8' },
    { token: 'type', foreground: '#0e7490' },
    { token: 'identifier', foreground: '#0f172a' },
  ];
}

// 暗色 token 配色(Atom One Dark 族,贴 DeepSeek 网页版代码块观感):
// 注释斜体灰蓝/关键字紫/字符串绿/数字橙/类型黄/函数蓝/操作符青。
// 仅 token 色,编辑器背景仍由 monacoColorsFor(dark) 派生,保持工作台整体协调。
function darkRules() {
  return [
    { token: 'comment', foreground: '5c637b', fontStyle: 'italic' },
    { token: 'string', foreground: '98c379' },
    { token: 'string.escape', foreground: '56b6c2' },
    { token: 'keyword', foreground: 'c678dd' },
    { token: 'number', foreground: 'd19a66' },
    { token: 'regexp', foreground: '98c379' },
    { token: 'type', foreground: 'e5c07b' },
    { token: 'type.identifier', foreground: 'e5c07b' },
    { token: 'function', foreground: '61afef' },
    { token: 'operator', foreground: '56b6c2' },
    { token: 'tag', foreground: 'e06c75' },
    { token: 'attribute.name', foreground: 'd19a66' },
    { token: 'attribute.value', foreground: '98c379' },
  ];
}

let themesRegistered = false;

/**
 * 应用主题(同一 tick 三通道):data-theme 属性 + Monaco setTheme + 订阅者回调。
 * mode='auto' 时经 matchMedia 解析实际深浅。monaco 模块由调用方传入
 * (避免与 monaco-setup 循环导入)。
 */
export function applyTheme(mode, monaco) {
  const NL = String.fromCharCode(10);
  const resolved = resolveMode(mode);
  // 1) data-theme + color-scheme(原生控件跟随)
  if (typeof document !== 'undefined') {
    document.documentElement.dataset.theme = resolved;
    document.documentElement.style.colorScheme = resolved;
    // 2) CSS 变量注入(覆盖 :root 同名变量;scoped 到 [data-theme])
    const tokens = COLOR_TOKENS[resolved];
    let style = document.getElementById('evo-theme-vars');
    if (!style) {
      style = document.createElement('style');
      style.id = 'evo-theme-vars';
      document.head.appendChild(style);
    }
    style.textContent =
      "[data-theme='" +
      resolved +
      "']{" +
      NL +
      Object.entries(tokens)
        .map(function (kv) {
          return '  --' + kv[0] + ': ' + kv[1] + ';';
        })
        .join(NL) +
      NL +
      '}';
  }
  // 3) Monaco(双套预注册一次;setTheme 切 base/colors)
  if (monaco) {
    if (!themesRegistered) {
      monaco.editor.defineTheme('evorule-dark', {
        base: 'vs-dark',
        inherit: true,
        rules: darkRules(),
        colors: monacoColorsFor('dark'),
      });
      monaco.editor.defineTheme('evorule-light', {
        base: 'vs',
        inherit: true,
        rules: lightRules(),
        colors: monacoColorsFor('light'),
      });
      themesRegistered = true;
    }
    monaco.editor.setTheme(resolved === 'light' ? 'evorule-light' : 'evorule-dark');
  }
  // 4) 订阅者(xterm 等)
  for (const fn of themeChangeSubscribers) {
    try {
      fn(resolved);
    } catch {
      /* 订阅者异常不阻断 */
    }
  }
  return resolved;
}
