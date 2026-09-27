// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// Monaco 环境装配 + 工作台主题(与工作台深色 token 同源)
import * as monaco from 'monaco-editor';
import EditorWorker from 'monaco-editor/esm/vs/editor/editor.worker?worker';
import JsonWorker from 'monaco-editor/esm/vs/language/json/json.worker?worker';

let ready = false;

/**
 * JSON 诊断选项基线(语法级校验;schema 挂载期由编辑器覆写 schemas 字段)。
 * validate 由 problems.json.validate 设置键联动;隐私治理点:enableSchemaRequest
 * 恒关,不发起任何网络请求。
 */
export function jsonDiagnosticsOptions(validate = true) {
  return {
    validate,
    allowComments: false,
    trailingCommas: 'warning',
    enableSchemaRequest: false,
    schemas: [],
  };
}

export function setupMonaco() {
  if (ready) return;
  ready = true;
  self.MonacoEnvironment = {
    getWorker: (_workerId, label) => (label === 'json' ? new JsonWorker() : new EditorWorker()),
  };
  if (monaco.languages.json?.jsonDefaults) {
    monaco.languages.json.jsonDefaults.setDiagnosticsOptions(jsonDiagnosticsOptions(true));
  }
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
