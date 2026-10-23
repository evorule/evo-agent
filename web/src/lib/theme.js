// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 主题唯一真相源:颜色类 token 明暗两值——CSS 变量(data-theme)/Monaco
// defineTheme/xterm theme 三通道全部从此派生(消除 monaco-setup 手工双源)。
//
// 纪律:dark/light 值=app.css :root/[data-theme] 现值原样(同源换血);
// 基线=GitHub Primer primitives(2026-10-23 排版与配色重设计冻结)。
// Monaco 永不读 DOM CSS 变量(colors 仅收 hex)。

/** 主题模式:'auto'(跟随系统) | 'evorule-dark' | 'evorule-light' */
export const THEME_MODES = ['auto', 'evorule-dark', 'evorule-light'];

/** 颜色类 token 明暗两值(dark=app.css :root 现值原样) */
export const COLOR_TOKENS = {
  dark: {
    'bg-page': '#0d1117',
    'bg-card': '#161b22',
    'bg-header': '#010409',
    'bg-hover': 'rgba(177, 186, 196, 0.12)',
    'bg-active': 'rgba(56, 139, 253, 0.15)',
    'bg-input': '#161b22',
    border: '#30363d',
    'border-strong': '#484f58',
    'text-primary': '#e6edf3',
    'text-secondary': '#9198a1',
    'text-muted': '#6e7681',
    'text-inverse': '#ffffff',
    success: '#3fb950',
    danger: '#f85149',
    warning: '#d29922',
    info: '#4493f8',
    brand: '#1f6feb',
    'brand-hover': '#2a5db0',
    'sidebar-bg': '#010409',
    'sidebar-text': '#9198a1',
    'sidebar-text-active': '#e6edf3',
    'git-added': '#3fb950',
    'git-modified': '#d29922',
    'git-deleted': '#f85149',
    'git-renamed': '#4493f8',
    'editor-bg': '#0d1117',
    'editor-line-number': '#6e7681',
    'editor-selection': '#1f6feb40',
    'editor-cursor': '#4493f8',
  },
  light: {
    'bg-page': '#ffffff',
    'bg-card': '#f6f8fa',
    'bg-header': '#ffffff',
    'bg-hover': 'rgba(31, 35, 40, 0.05)',
    'bg-active': 'rgba(9, 105, 218, 0.10)',
    'bg-input': '#ffffff',
    border: '#d1d9e0',
    'border-strong': '#8c959f',
    'text-primary': '#1f2328',
    'text-secondary': '#656d76',
    'text-muted': '#6e7781',
    'text-inverse': '#ffffff',
    success: '#1a7f37',
    danger: '#d1242f',
    warning: '#9a6700',
    info: '#0969da',
    brand: '#0969da',
    'brand-hover': '#0757ba',
    'sidebar-bg': '#f6f8fa',
    'sidebar-text': '#656d76',
    'sidebar-text-active': '#1f2328',
    'git-added': '#1a7f37',
    'git-modified': '#9a6700',
    'git-deleted': '#d1242f',
    'git-renamed': '#0969da',
    'editor-bg': '#ffffff',
    'editor-line-number': '#6e7781',
    'editor-selection': '#0969da26',
    'editor-cursor': '#0969da',
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
