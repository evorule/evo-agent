// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 全局 ModelRegistry:path → Monaco model 单源(编辑器分屏共享同一 model 的
// 落地载体,model 挂 evofile:/// 规约 Uri)。同文件多编辑器实例挂同一
// model——内容/undo 栈自动同步。
//
// 职责边界:本模块只持「存活 model 的 path 键映射」与 baseline(编辑基线,
// dirty 判定用);model 的创建参数(uriSpecFor/langOf)与 dispose 时机归调用方
// (EditorPane 主组/GroupPane 副组)——键一致即共享。

const models = new Map();
const baselines = new Map();

/** 取 path 对应的存活 model(无则 null) */
export function getModelForPath(path) {
  return models.get(path) || null;
}

export function hasModel(path) {
  return models.has(path);
}

/** 注册存活 model(创建后调用;同 path 重复注册=覆盖) */
export function registerModel(path, model) {
  models.set(path, model);
}

/** 释放 path 的 model 登记(返回被释放的 model 或 null;dispose 由调用方执行) */
export function releaseModel(path) {
  const m = models.get(path) || null;
  models.delete(path);
  return m;
}

/** 全部存活 [path, model](设置同步/markdown lint 遍历/销毁清理) */
export function allModels() {
  return [...models.entries()];
}

/** model 存活键重命名(文件重命名/移动:undo 栈随 model 保留) */
export function renameModelKey(from, to) {
  const m = models.get(from);
  if (m) {
    models.delete(from);
    models.set(to, m);
  }
}

// ---- 编辑基线(dirty 判定的「已保存内容」快照;跨组共享) ----

export function getBaseline(path) {
  return baselines.get(path) ?? '';
}

export function setBaseline(path, content) {
  baselines.set(path, content);
}

export function deleteBaseline(path) {
  baselines.delete(path);
}

export function renameBaseline(from, to) {
  const b = baselines.get(from);
  if (b !== undefined) {
    baselines.delete(from);
    baselines.set(to, b);
  }
}
