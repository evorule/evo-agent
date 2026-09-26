// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// Monaco 环境装配 + 工作台主题(与工作台深色 token 同源)
import * as monaco from 'monaco-editor';
import EditorWorker from 'monaco-editor/esm/vs/editor/editor.worker?worker';
import JsonWorker from 'monaco-editor/esm/vs/language/json/json.worker?worker';

let ready = false;

export function setupMonaco() {
  if (ready) return;
  ready = true;
  self.MonacoEnvironment = {
    getWorker: (_workerId, label) => (label === 'json' ? new JsonWorker() : new EditorWorker()),
  };
  monaco.editor.defineTheme('evorule-dark', {
    base: 'vs-dark',
    inherit: true,
    rules: [],
    colors: {
      'editor.background': '#0d1117',
      'editor.foreground': '#f1f5f9',
      'editorLineNumber.foreground': '#64748b',
      'editorLineNumber.activeForeground': '#94a3b8',
      'editor.selectionBackground': '#1d63ed40',
      'editorCursor.foreground': '#2496ed',
      'editorGutter.background': '#0d1117',
      'editorIndentGuide.background': '#ffffff10',
      'scrollbarSlider.background': '#ffffff15',
      'scrollbarSlider.hoverBackground': '#ffffff25',
    },
  });
}

export { monaco };
