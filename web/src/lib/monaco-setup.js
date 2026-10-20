// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// Monaco 环境装配。主题 defineTheme/应用移至 lib/theme.js applyTheme 统一
// 驱动(token 表唯一真相源,消除本模块手工双源);本模块保留 worker 装配与
// JSON 诊断选项基线。
import * as monaco from 'monaco-editor';
// 全量编辑器贡献:monaco-editor 0.52 的包入口 editor.main 只含核心编辑器
// (edcore)+语言——inline completions 控制器/查找/右键菜单/undo-redo API 等
// 富贡献在 editor.all.js,不导入则 ghost text 永不触发、编辑器退化为裸核心
import 'monaco-editor/esm/vs/editor/editor.all.js';
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
  // H4 整档格式化:Monaco 内建面已注册 json/css/html 的 DocumentFormatting;
  // JS/TS 仅有 range 格式化,此处补注册整档 provider(委托 ts worker,规则
  // 与内建 FormatHelper 一致)。注册后内建「格式化文档」右键菜单项与
  // Shift+Alt+F(formatActions,仅编辑器聚焦时)随之生效。
  registerTsDocumentFormatting();
  // 双套主题预注册 + 按设置应用(colors/rules 从 theme.js token 表派生)
  const mode = readStoredMode();
  applyTheme('evorule-dark', monaco);
  applyTheme('evorule-light', monaco);
  applyTheme(mode, monaco);
}

/** JS/TS 整档格式化 provider(worker getFormattingEditsForDocument;
 *  options 换算与内建 FormatHelper._convertOptions 同表) */
function registerTsDocumentFormatting() {
  const ts = monaco.languages.typescript;
  if (!ts?.getTypeScriptWorker) return; // worker 面缺失时退化为无整档格式化
  const convertOptions = (o) => ({
    ConvertTabsToSpaces: o.insertSpaces,
    TabSize: o.tabSize,
    IndentSize: o.tabSize,
    IndentStyle: 2, // Smart
    NewLineCharacter: '\n',
    InsertSpaceAfterCommaDelimiter: true,
    InsertSpaceAfterSemicolonInForStatements: true,
    InsertSpaceBeforeAndAfterBinaryOperators: true,
    InsertSpaceAfterKeywordsInControlFlowStatements: true,
    InsertSpaceAfterFunctionKeywordForAnonymousFunctions: true,
    InsertSpaceAfterOpeningAndBeforeClosingNonemptyParenthesis: false,
    InsertSpaceAfterOpeningAndBeforeClosingNonemptyBrackets: false,
    InsertSpaceAfterOpeningAndBeforeClosingTemplateStringBraces: false,
    PlaceOpenBraceOnNewLineForControlBlocks: false,
    PlaceOpenBraceOnNewLineForFunctions: false,
  });
  const provider = {
    async provideDocumentFormattingEdits(model, options) {
      const getWorker = model.getLanguageId() === 'javascript' ? ts.getJavaScriptWorker() : ts.getTypeScriptWorker();
      const client = await (await getWorker)(model.uri);
      if (model.isDisposed()) return [];
      const changes = await client.getFormattingEditsForDocument(
        model.uri.toString(),
        convertOptions(options),
      );
      if (model.isDisposed() || !Array.isArray(changes)) return [];
      return changes.map((c) => {
        const p1 = model.getPositionAt(c.span.start);
        const p2 = model.getPositionAt(c.span.start + c.span.length);
        return {
          range: {
            startLineNumber: p1.lineNumber,
            startColumn: p1.column,
            endLineNumber: p2.lineNumber,
            endColumn: p2.column,
          },
          text: c.newText,
        };
      });
    },
  };
  for (const lang of ['javascript', 'typescript']) {
    monaco.languages.registerDocumentFormattingEditProvider(lang, provider);
  }
}

export { monaco };
