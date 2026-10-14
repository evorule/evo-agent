// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// Monaco 环境装配。主题 defineTheme/应用移至 lib/theme.js applyTheme 统一
// 驱动(token 表唯一真相源,消除本模块手工双源);本模块保留 worker 装配与
// JSON 诊断选项基线。
import * as monaco from 'monaco-editor';
import EditorWorker from 'monaco-editor/esm/vs/editor/editor.worker?worker';
import JsonWorker from 'monaco-editor/esm/vs/language/json/json.worker?worker';
import { applyTheme, readStoredMode } from './theme.js';

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
  // 双套主题预注册 + 按设置应用(colors/rules 从 theme.js token 表派生)
  const mode = readStoredMode();
  applyTheme('evorule-dark', monaco);
  applyTheme('evorule-light', monaco);
  applyTheme(mode, monaco);
}

export { monaco };
