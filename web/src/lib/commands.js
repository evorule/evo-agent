// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 命令注册表:工作台全部可命令化动作的唯一入口(对齐 contributes.commands 模型,裁剪到最小)。
// 命令层无状态、组件自注册:依赖组件内状态的命令(EditorPane 保存/差异视图)由组件
// onMount 注册、onDestroy 注销;executeCommand 是全局执行入口,listCommands 供面板枚举。

const registry = new Map();

/**
 * 注册命令。同 id 重复注册 = 覆盖(幂等,组件热替换/重挂载安全)。
 * @param {{id: string, title: string, category?: string, keybinding?: string,
 *           when?: string, run: Function}} cmd
 */
export function registerCommand(cmd) {
  if (!cmd || typeof cmd.id !== 'string' || !cmd.id) {
    throw new Error('registerCommand: id 必须为非空字符串');
  }
  if (typeof cmd.title !== 'string' || !cmd.title) {
    throw new Error(`registerCommand(${cmd.id}): title 必须为非空字符串`);
  }
  if (typeof cmd.run !== 'function') {
    throw new Error(`registerCommand(${cmd.id}): run 必须为函数`);
  }
  registry.set(cmd.id, {
    id: cmd.id,
    title: cmd.title,
    category: cmd.category || '',
    keybinding: cmd.keybinding || null, // 默认键位提示(实际生效键以键位规则表为准)
    when: cmd.when || '',
    run: cmd.run,
  });
}

/** 注销命令(组件销毁时自清理)。返回是否确有注销。 */
export function unregisterCommand(id) {
  return registry.delete(id);
}

/** 全局执行入口。命令不存在抛错(调用方面板层决定如何呈现)。 */
export function executeCommand(id, ...args) {
  const cmd = registry.get(id);
  if (!cmd) throw new Error(`未知命令:${id}`);
  return cmd.run(...args);
}

export function getCommand(id) {
  return registry.get(id) || null;
}

/** 面板枚举:按 分类→标题 排序(确定性次序,导航稳定)。 */
export function listCommands() {
  const cmp = new Intl.Collator('zh-Hans-CN').compare;
  return [...registry.values()].sort(
    (a, b) => cmp(a.category, b.category) || cmp(a.title, b.title),
  );
}
