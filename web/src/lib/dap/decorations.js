// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 编辑器断点装饰:gutter 断点圆点 + stopped 命中行高亮 + gutter 点击增删。
//
// 形态:createDecorationsCollection(0.52 新 API)双集合——断点集合(断点 store
// 驱动)与 stopped 集合(命中帧驱动);model 切换即重算(装饰挂在 editor 上,
// 跟随当前 model 的 path 过滤)。装饰类名样式落 app.css(主题 token;B11 三通道)。
//
// 实测注记(2026-10-17 真机):
// - monaco 0.52 ESM 命名空间未导出 TrackedRangeStickiness/MouseTargetType——
//   取字面量(stickiness=1;gutter target type 2/3/4);
// - modelChange 事件内即时 set 的装饰偶发不渲染——同步延后一帧(rAF)。

import { get } from 'svelte/store';
import { debugBreakpoints, debugStopped, toggleBreakpoint } from './index.js';

export function attachDebugDecorations({ monaco, editor, getModelForPath, pathFromUri }) {
  if (!editor) return () => {};
  editor.updateOptions({ glyphMargin: true }); // 断点圆点宿主(默认关)

  const bpCollection = editor.createDecorationsCollection();
  const stopCollection = editor.createDecorationsCollection();

  /** 当前 model 的工作区相对路径(草稿/evo:// 等返回 null) */
  function currentPath() {
    const model = editor.getModel();
    if (!model) return null;
    const p = pathFromUri(model.uri);
    return p && !p.startsWith('evo://') ? p : null;
  }

  function syncBreakpoints() {
    // 延后一帧:modelChange 事件内即时 set 的装饰在 0.52 上偶发不渲染(实测)
    requestAnimationFrame(() => {
      const path = currentPath();
      const lines = path ? get(debugBreakpoints)[path] || [] : [];
      bpCollection.set(
        lines.map((line) => ({
          range: new monaco.Range(line, 1, line, 1),
          options: {
            isWholeLine: true,
            glyphMarginClassName: 'dap-bp-glyph',
            glyphMarginHoverMessage: { value: '断点(点击 gutter 移除)' },
            stickiness: 1, // NeverGrowsWhenTypingAtEdges(0.52 命名空间无此枚举,取字面量)
          },
        })),
      );
    });
  }

  function syncStopped() {
    requestAnimationFrame(() => {
      const hit = get(debugStopped);
      const path = currentPath();
      if (!hit || !path || hit.path !== path) {
        stopCollection.set([]);
        return;
      }
      stopCollection.set([
        {
          range: new monaco.Range(hit.line, 1, hit.line, 1),
          options: {
            isWholeLine: true,
            className: 'dap-stopped-line',
            linesDecorationsClassName: 'dap-stopped-arrow',
            stickiness: 1,
          },
        },
      ]);
    });
  }

  // gutter 点击:字形边距/行号区 → 增删断点。
  // MouseTargetType 枚举在 monaco 0.52 ESM 命名空间未导出(实测 undefined),
  // 取字面量:2=GUTTER_GLYPH_MARGIN,3=GUTTER_LINE_NUMBERS,4=GUTTER_LINE_DECORATIONS
  const GUTTER_TYPES = [2, 3, 4];
  const mouseDown = editor.onMouseDown((e) => {
    const t = e?.target;
    if (!t) return;
    if (!GUTTER_TYPES.includes(t.type)) return;
    const path = currentPath();
    if (!path) return;
    const line = t.position?.lineNumber;
    if (!Number.isFinite(line)) return;
    toggleBreakpoint(path, line);
  });

  const modelChange = editor.onDidChangeModel(() => {
    syncBreakpoints();
    syncStopped();
  });

  const un1 = debugBreakpoints.subscribe(syncBreakpoints);
  const un2 = debugStopped.subscribe(syncStopped);
  syncBreakpoints();
  syncStopped();

  return function disposeDebugDecorations() {
    mouseDown.dispose();
    modelChange.dispose();
    un1();
    un2();
    bpCollection.clear();
    stopCollection.clear();
  };
}
