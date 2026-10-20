<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 EvoRule Project -->
<!-- 侧面板:文件树(真实目录浏览,懒加载;目录点击展开,文件点击进编辑器 tab)。
     渲染采用扁平行模型(展开目录按深度打平),避免递归组件。
     增量:顶部操作钮 / inline 命名(新建+重命名,支持 "/" 建层级)/
     右键菜单 / 删除确认 / 折叠持久化 / WS fs_events 增量刷新 / tabs 联动 /
     拖拽移动(目录节点与根区可放置)。 -->
<script>
  import { onMount, tick } from 'svelte';
  import { get } from 'svelte/store';
  import { listDir, createFile, moveFile, deleteFile } from '../lib/api.js';
  import { registerCommand, unregisterCommand } from '../lib/commands.js';
  import { reconnectCurrent } from '../lib/ws.js';
  import {
    validateWorkspaceInput,
    updateWorkspaceRecent,
    loadRecentWorkspaces,
    saveRecentWorkspaces,
    workspaceAnchor,
    workspaceLabel as wsLabelOf,
  } from '../lib/workspace.js';
  import {
    openFile,
    closeTab,
    renameTabPath,
    activePath,
    fsEvents,
    connStatus,
    gitStatus,
    pendingTreeReveal,
  } from '../lib/stores.js';
  import ContextMenu from './ContextMenu.svelte';
  import ConfirmDialog from './ConfirmDialog.svelte';
  import OutlinePanel from './OutlinePanel.svelte';

  // ---- git 状态字母装饰(B3) ----
  // 单状态源 $gitStatus;exact 命中文件条目,折叠目录条目(尾 `/`)前缀命中其
  // 目录行。staged 与 changes 同时存在时 changes 优先(工作区状态更醒目)。
  $: gitMap = buildGitMap($gitStatus);

  function buildGitMap(st) {
    const map = new Map();
    if (!st) return map;
    for (const e of st.changes || []) map.set(e.path, e.status);
    for (const e of st.staged || []) if (!map.has(e.path)) map.set(e.path, e.status);
    return map;
  }

  function gitLetter(node) {
    const hit = gitMap.get(node.path);
    if (hit) return hit;
    if (node.kind !== 'dir') return null;
    const prefix = `${node.path}/`;
    for (const [p, s] of gitMap) {
      if (p.endsWith('/') && p.startsWith(prefix)) return s; // 折叠目录(untracked)
    }
    return null;
  }

  function gitLetterClass(s) {
    if (s === 'A' || s === 'U') return 'added';
    if (s === 'D') return 'deleted';
    if (s === 'R') return 'renamed';
    return 'modified';
  }

  let rootName = 'evo-agent(工作区)';
  let rootChildren = [];
  let rows = []; // [{node, depth}]
  let loadError = '';

  // path('' = 根) → {loaded, expanded, children}
  const dirState = new Map();

  // ---- E1 工作区:文件树锚点 + 重连刷新 ----
  // REST 文件面以 serve workdir 为根(绝对路径拒绝,安全红线不松);工作区在
  // workdir 之下时树锚定其相对子目录(anchor),之外时保持 workdir 根(agent
  // 能力边界仍随新连接生效,提示条明示)。
  const IS_WIN = typeof navigator !== 'undefined' && /win/i.test(navigator.userAgent || '');
  let anchor = ''; // 树根的 workdir 相对路径('' = workdir 根)
  let lastRootDir = ''; // 最近一次 listDir 返回的 serve workdir(重连对比用)
  let lastWsAtLoad = ''; // 最近一次建树时的工作区(重连对比用)
  let didLoad = false; // 首树建好后重连才做按需刷新
  let wsMenu = null; // 工作区下拉 {x, y, items}
  let wsDialog = null; // 切换工作区对话框 {value, error, probe}
  let wsNotice = ''; // 工作区切换提示条(带「立即重连」)
  let recentWs = loadRecentWorkspaces();

  // 折叠状态持久化(localStorage;键位口径与工作台其他 localStorage 键一致)
  const STATE_KEY = 'evo_explorer_state';

  // ---- inline 命名状态 ----
  // {mode:'createFile'|'createDir', anchor: 目录路径('' = 根), depth, value, error}
  // {mode:'rename', target: node, depth, value, error}
  let naming = null;

  // ---- 右键菜单 / 删除确认 ----
  let menu = null; // {x, y, items}
  let confirmState = null; // {message, run()}

  // ---- 选中与拖拽(选中 = 命令面板节点命令的作用对象;DnD = 树内移动) ----
  let selected = null;
  let dragOverPath = null; // 拖拽悬停的目标目录路径('' = 根,null = 无)

  async function loadChildren(path) {
    const st = dirState.get(path);
    if (st?.loaded) return st.children;
    const res = await listDir(path || undefined);
    const children = (res.entries || []).map((e) => ({
      name: e.name,
      kind: e.kind,
      path: path ? `${path}/${e.name}` : e.name,
    }));
    dirState.set(path, { ...(st || {}), loaded: true, children });
    return children;
  }

  /** 强制重读目录(绕过 loaded 缓存;目录已不存在时失效其状态) */
  async function reloadDir(path) {
    try {
      const res = await listDir(path || undefined);
      const children = (res.entries || []).map((e) => ({
        name: e.name,
        kind: e.kind,
        path: path ? `${path}/${e.name}` : e.name,
      }));
      dirState.set(path, { ...(dirState.get(path) || {}), loaded: true, children });
    } catch {
      dirState.delete(path);
    }
  }

  function persistExpanded() {
    try {
      const arr = [...dirState.entries()].filter(([, st]) => st.expanded).map(([p]) => p);
      localStorage.setItem(STATE_KEY, JSON.stringify(arr));
    } catch {
      /* 存储不可用时折叠状态只存活于内存 */
    }
  }

  function restoreExpanded() {
    try {
      const arr = JSON.parse(localStorage.getItem(STATE_KEY) || '[]');
      return Array.isArray(arr) ? arr.filter((p) => typeof p === 'string') : [];
    } catch {
      return [];
    }
  }

  function rebuild() {
    const out = [];
    const walk = (children, depth) => {
      for (const n of children) {
        out.push({ node: n, depth });
        if (n.kind === 'dir' && dirState.get(n.path)?.expanded) {
          walk(dirState.get(n.path)?.children || [], depth + 1);
        }
      }
    };
    walk(rootChildren, 0);
    rows = out;
  }

  async function toggle(node) {
    if (node.kind !== 'dir') return;
    const st = dirState.get(node.path) || {};
    if (st.expanded) {
      dirState.set(node.path, { ...st, expanded: false });
      persistExpanded();
      rebuild();
      return;
    }
    try {
      await loadChildren(node.path);
      dirState.set(node.path, { ...dirState.get(node.path), expanded: true });
      persistExpanded();
      rebuild();
    } catch (e) {
      loadError = String(e?.message || e);
    }
  }

  function open(node) {
    selected = node;
    if (node.kind === 'dir') toggle(node);
    else openFile(node.path);
  }

  // =========================================================================
  // inline 命名(新建/重命名共用一行输入;Enter 提交,Esc 取消,失焦提交)
  // =========================================================================

  function parentDir(p) {
    const i = p.lastIndexOf('/');
    return i === -1 ? '' : p.slice(0, i);
  }

  async function startCreate(mode, dirPath, depth = 0) {
    // 目录锚点未展开时先展开(输入行渲染在该目录子层)
    if (dirPath && dirState.get(dirPath)?.loaded !== true) {
      const node = rows.find((r) => r.node.path === dirPath)?.node;
      if (node) await toggle(node);
    } else if (dirPath && !dirState.get(dirPath)?.expanded) {
      dirState.set(dirPath, { ...dirState.get(dirPath), expanded: true });
      persistExpanded();
      rebuild();
    }
    naming = { mode, anchor: dirPath, depth: depth || 0, value: '', error: '' };
    if (dirPath) rebuild();
  }

  function startRename(node, depth) {
    naming = { mode: 'rename', target: node, depth, value: node.name, error: '' };
  }

  function cancelNaming() {
    naming = null;
  }

  // Windows 保留名/非法字符(前端预校验提示,免一次必败请求;后端为一等校验面)
  const RESERVED_NAMES = new Set([
    'CON', 'PRN', 'AUX', 'NUL',
    ...['COM1', 'COM2', 'COM3', 'COM4', 'COM5', 'COM6', 'COM7', 'COM8', 'COM9'],
    ...['LPT1', 'LPT2', 'LPT3', 'LPT4', 'LPT5', 'LPT6', 'LPT7', 'LPT8', 'LPT9'],
  ]);
  const ILLEGAL_CHARS = /[<>:"|?*]/; // 路径分隔符已由规整处理,余下非法字符在此拦截

  /** 逐段校验路径;返回错误文案,合法返回空串 */
  function invalidSegmentError(raw) {
    for (const seg of raw.split('/')) {
      if (!seg || seg === '.' || seg === '..') return `非法路径段:「${seg}」`;
      if (ILLEGAL_CHARS.test(seg)) return `含非法字符:「${seg}」`;
      if (RESERVED_NAMES.has(seg.replace(/[. ]+$/, '').toUpperCase())) {
        return `Windows 保留名:「${seg}」`;
      }
      if (/[. ]$/.test(seg)) return `Windows 不允许尾部点/空格:「${seg}」`;
    }
    return '';
  }

  async function commitNaming() {
    const n = naming;
    if (!n) return;
    // 规整用户输入:反斜杠 → 正斜杠,去首尾分隔符与空白(支持 "/" 建层级)
    const raw = n.value.trim().replace(/\\/g, '/').replace(/^\/+|\/+$/g, '');
    if (!raw) {
      naming = null;
      return;
    }
    const segErr = invalidSegmentError(raw);
    if (segErr) {
      naming = { ...n, error: segErr };
      return;
    }
    if (n.mode === 'rename') {
      const parent = parentDir(n.target.path);
      const newPath = parent ? `${parent}/${raw}` : raw;
      if (newPath === n.target.path) {
        naming = null;
        return;
      }
      try {
        await moveFile(n.target.path, parent || '.', raw);
        naming = null;
        await reloadDir(parent);
        rebuild();
        persistExpanded();
        // tabs 联动:model 缓存随 lastRename 迁移,dirty 内容保留
        renameTabPath(n.target.path, newPath);
      } catch (e) {
        naming = { ...n, error: String(e?.message || e) };
      }
      return;
    }
    const dirPath = n.anchor;
    const path = dirPath ? `${dirPath}/${raw}` : raw;
    try {
      await createFile(path, n.mode === 'createDir' ? 'dir' : 'file');
      naming = null;
      await reloadDir(dirPath);
      dirState.set(dirPath, { ...dirState.get(dirPath), expanded: true });
      persistExpanded();
      rebuild();
      if (n.mode === 'createFile') openFile(path);
    } catch (e) {
      naming = { ...n, error: String(e?.message || e) };
    }
  }

  // =========================================================================
  // 右键菜单 / 删除确认
  // =========================================================================

  function copyPath(node) {
    try {
      navigator.clipboard.writeText(node.path);
    } catch {
      /* 剪贴板不可用(非安全上下文等)时静默跳过 */
    }
  }

  function menuFor(node) {
    const items = [];
    if (node.kind === 'dir') {
      items.push({ label: '新建文件', action: () => startCreate('createFile', node.path) });
      items.push({ label: '新建文件夹', action: () => startCreate('createDir', node.path) });
      items.push({ separator: true });
    } else {
      items.push({ label: '在编辑器打开', action: () => openFile(node.path) });
    }
    items.push({ label: '重命名', action: () => startRename(node) });
    items.push({ label: '复制路径', action: () => copyPath(node) });
    items.push({ label: '刷新', action: refreshAll }); // E3:节点菜单补刷新(根区已有)
    items.push({ separator: true });
    items.push({ label: '删除…', danger: true, action: () => askDelete(node) });
    return items;
  }

  function openMenu(e, node) {
    e.preventDefault();
    selected = node;
    menu = { x: e.clientX, y: e.clientY, items: menuFor(node) };
  }

  function openRootMenu(e) {
    e.preventDefault();
    menu = {
      x: e.clientX,
      y: e.clientY,
      items: [
        { label: '新建文件', action: () => startCreate('createFile', anchor) },
        { label: '新建文件夹', action: () => startCreate('createDir', anchor) },
        { label: '刷新', action: refreshAll },
      ],
    };
  }

  function askDelete(node) {
    confirmState = {
      message: `删除「${node.name}」?内容将移入回收目录(.evo-trash),可手动找回。`,
      run: async () => {
        try {
          await deleteFile(node.path);
          // tabs 联动:已打开的 tab 一并关闭(dirty 内容随确认放弃)
          closeTab(node.path);
          await reloadDir(parentDir(node.path));
          rebuild();
        } catch (e) {
          loadError = String(e?.message || e);
        }
      },
    };
  }

  // =========================================================================
  // 拖拽移动(HTML5 DnD):draggable 行携带相对路径,drop 到目录/根 → move
  // =========================================================================

  function startDrag(e, node) {
    if (e.target && e.target.tagName === 'INPUT') {
      e.preventDefault(); // inline 命名输入中的文本选择不触发节点拖拽
      return;
    }
    e.dataTransfer.setData('text/plain', node.path);
    e.dataTransfer.effectAllowed = 'move';
  }

  function dragOver(e, dirPath) {
    e.preventDefault(); // 允许 drop(规范要求)
    e.dataTransfer.dropEffect = 'move';
    dragOverPath = dirPath;
  }

  async function dropTo(e, targetDir) {
    e.preventDefault();
    dragOverPath = null;
    const from = e.dataTransfer.getData('text/plain');
    if (!from) return;
    const name = from.split('/').pop() || from;
    const newPath = targetDir ? `${targetDir}/${name}` : name;
    // 守卫:拖到自身/自身子树 = 无操作(原地移动 newPath === from 同样拦截)
    if (newPath === from) return;
    if (targetDir === from || (targetDir && targetDir.startsWith(`${from}/`))) return;
    try {
      await moveFile(from, targetDir || '.', null);
      dirState.delete(from); // 旧子树状态随移动失效(展开态不迁移,v1 从简)
      await reloadDir(parentDir(from));
      if (dirState.get(targetDir)?.loaded) await reloadDir(targetDir);
      rootChildren = dirState.get(anchor)?.children || [];
      persistExpanded();
      rebuild();
      // tab 打开时路径联动(dirty 保留;未打开时无 model 键,信号无害)
      renameTabPath(from, newPath);
    } catch (err) {
      loadError = String(err?.message || err);
    }
  }

  // =========================================================================
  // 命令面板作用域:菜单/操作钮与命令共用同一本地函数(一处定义两处消费)
  // =========================================================================

  /** 选中节点 → 新建动作的目录锚点(dir→自身,file→父目录,无选中→树根) */
  function anchorFor(sel) {
    if (!sel) return anchor;
    return sel.kind === 'dir' ? sel.path : parentDir(sel.path);
  }

  /** 选中节点的树深度(inline 输入行缩进对齐用;找不到按根层) */
  function depthOf(sel) {
    if (!sel) return 0;
    const row = rows.find((r) => r.node.path === sel.path);
    return row ? row.depth : 0;
  }

  // =========================================================================
  // WS fs_events 增量刷新(受影响目录重读 → rebuild,不全树 refetch)
  // =========================================================================

  async function applyFsEvents(events) {
    const toReload = new Set();
    const toInvalidate = new Set();
    const touchParent = (p) => toReload.add(parentDir(p));
    const invalidate = (p) => {
      toInvalidate.add(p);
      touchParent(p);
    };
    for (const p of events.added || []) touchParent(p);
    for (const p of events.updated || []) touchParent(p);
    for (const p of events.removed || []) invalidate(p);
    for (const m of events.moved || []) {
      invalidate(m.from);
      touchParent(m.to);
      // 移动目录:旧子树的展开/懒加载状态全部失效
      toInvalidate.add(m.to);
      // tabs 联动:外部/agent 侧移动时已打开 tab 跟随迁移(dirty 保留)
      renameTabPath(m.from, m.to);
    }
    // 失效:删除以失效路径为前缀的全部 dirState 条目
    if (toInvalidate.size) {
      for (const key of [...dirState.keys()]) {
        if (toInvalidate.has(key)) {
          dirState.delete(key);
          continue;
        }
        for (const p of toInvalidate) {
          if (key.startsWith(`${p}/`)) {
            dirState.delete(key);
            break;
          }
        }
      }
    }
    for (const dir of toReload) await reloadDir(dir);
    rootChildren = dirState.get(anchor)?.children || [];
    persistExpanded();
    rebuild();
  }

  /** 全量刷新:重读根与全部已加载目录(保留展开状态) */
  async function refreshAll() {
    const loaded = [...dirState.keys()].filter((p) => dirState.get(p)?.loaded && p !== anchor);
    for (const p of [anchor, ...loaded]) await reloadDir(p);
    rootChildren = dirState.get(anchor)?.children || [];
    rebuild();
  }

  // =========================================================================
  // H2 文件树定位(面包屑/命令共用的 pendingTreeReveal 一次性信号):
  // 逐级展开祖先目录 → 滚动定位 + 闪烁;树根可能锚定在工作区子目录(E1),
  // 树外路径不定位。消费即清,onMount 也探一次(组件未挂载时设下的信号)。
  // =========================================================================

  let treeEl = null;
  let flashPath = null; // 定位闪烁的节点路径(1.5s 自动清)
  let flashTimer = null;

  /** 工作区路径 → 树内路径(锚点换算);null = 树外 */
  function toTreePath(p) {
    if (!p) return '';
    if (!anchor) return p;
    if (p === anchor) return '';
    if (p.startsWith(`${anchor}/`)) return p.slice(anchor.length + 1);
    return null;
  }

  async function revealInTree(target) {
    try {
      const segs = target ? target.split('/').filter(Boolean) : [];
      let acc = '';
      for (const seg of segs) {
        acc = acc ? `${acc}/${seg}` : seg;
        const st = dirState.get(acc);
        if (!st?.loaded) await loadChildren(acc);
        dirState.set(acc, { ...dirState.get(acc), expanded: true });
      }
      rootChildren = dirState.get(anchor)?.children || [];
      persistExpanded();
      rebuild();
      await tick();
      const esc =
        typeof CSS !== 'undefined' && CSS.escape ? CSS.escape(target) : target.replace(/"/g, '');
      const el = treeEl?.querySelector(`[data-tree-path="${esc}"]`);
      if (el) {
        el.scrollIntoView({ block: 'center' });
        flashPath = target;
        clearTimeout(flashTimer);
        flashTimer = setTimeout(() => (flashPath = null), 1500);
      }
    } catch {
      /* 目录加载失败(树外/瞬态):树态保持,不阻断 */
    }
  }

  async function consumeTreeReveal(sig) {
    if (!sig?.path) return;
    pendingTreeReveal.set(null);
    const target = toTreePath(sig.path);
    if (target === null) return;
    await revealInTree(target);
  }

  $: consumeTreeReveal($pendingTreeReveal);

  // =========================================================================
  // E1 工作区:建树/重连刷新 + 下拉菜单 + 切换对话框
  // =========================================================================

  /** 建树(挂载与工作区变化后共用):重读根 → 换算锚点 → 全量重建 */
  async function initTree() {
    try {
      const res = await listDir();
      lastRootDir = res.dir || '';
      lastWsAtLoad = localStorage.getItem('evo_workspace') || '';
      anchor = workspaceAnchor(lastRootDir, lastWsAtLoad, { caseInsensitive: IS_WIN }) || '';
      dirState.clear(); // 锚点可能变化,旧展开/懒加载状态整树失效
      rootChildren = [];
      const segSrc = anchor || lastRootDir;
      const seg = segSrc.split(/[\\/]/).filter(Boolean).pop();
      if (seg) rootName = seg;
      await loadChildren(anchor);
      rootChildren = dirState.get(anchor)?.children || [];
      // 恢复上次折叠状态(目录已不存在的条目静默跳过)
      for (const p of restoreExpanded()) {
        try {
          await loadChildren(p);
          dirState.set(p, { ...dirState.get(p), expanded: true });
        } catch {
          /* 目录已不存在 */
        }
      }
      rebuild();
      loadError = '';
      didLoad = true;
    } catch (e) {
      loadError = String(e?.message || e);
    }
  }

  /** 重连后按需刷新:根目录或工作区未变则树不动(防被动重连闪树) */
  async function refreshOnReconnect() {
    if (!didLoad) return;
    try {
      const res = await listDir();
      const ws = localStorage.getItem('evo_workspace') || '';
      if (res.dir === lastRootDir && ws === lastWsAtLoad) return;
      await initTree();
    } catch {
      /* 闪断:树保留旧态,后续 fs_events/手动刷新兜底 */
    }
  }

  function currentWorkspace() {
    return localStorage.getItem('evo_workspace') || '';
  }

  /** 工作区下拉菜单:最近 5 个 + 切换对话框 + 恢复缺省 */
  function openWsMenu(e) {
    const el = e.currentTarget || e.target;
    const r = el.getBoundingClientRect ? el.getBoundingClientRect() : { left: e.clientX, bottom: e.clientY };
    const cur = currentWorkspace();
    const items = recentWs.map((p) => ({
      label: `${p === cur ? '✓ ' : ''}${p}`,
      action: () => applyWorkspace(p),
    }));
    if (items.length) items.push({ separator: true });
    items.push({ label: '切换工作区…', action: openWsDialog });
    items.push({ label: '恢复缺省(serve workdir)', action: () => applyWorkspace('') });
    wsMenu = { x: r.left, y: r.bottom + 2, items };
  }

  /** 应用工作区(切换/恢复缺省共用):写 localStorage → 提示条(重连生效);
   *  广播 evo:workspace-changed 供对话侧栏同步按钮态。 */
  function applyWorkspace(v) {
    const chk = validateWorkspaceInput(v);
    if (!chk.ok) {
      wsNotice = `工作区路径不合法:${chk.error}`;
      return;
    }
    if (chk.value) {
      localStorage.setItem('evo_workspace', chk.value);
      recentWs = updateWorkspaceRecent(recentWs, chk.value);
      saveRecentWorkspaces(recentWs);
      wsNotice = `工作区已设为:${wsLabelOf(chk.value) || chk.value}。对新连接生效(新建会话/换线/重连)。`;
    } else {
      localStorage.removeItem('evo_workspace');
      wsNotice = '已恢复缺省工作区(serve workdir)。对新连接生效。';
    }
    try {
      window.dispatchEvent(new CustomEvent('evo:workspace-changed'));
    } catch {
      /* 非浏览器环境(测试)静默 */
    }
  }

  function openWsDialog() {
    wsDialog = { value: currentWorkspace(), error: '', probe: '' };
  }

  function confirmWsDialog() {
    if (!wsDialog) return;
    const chk = validateWorkspaceInput(wsDialog.value);
    if (!chk.ok) {
      wsDialog = { ...wsDialog, error: chk.error };
      return;
    }
    applyWorkspace(chk.value);
    wsDialog = null;
  }

  /** 探测输入路径(尽力而为):REST 文件面 workdir 根限定,越界/外部路径
   *  探测必然失败——提示措辞明示「新边界属正常」,不阻断切换(serve 为
   *  一等校验面,连接失败可恢复缺省)。 */
  async function probeWsPath() {
    if (!wsDialog) return;
    const chk = validateWorkspaceInput(wsDialog.value);
    if (!chk.ok) {
      wsDialog = { ...wsDialog, error: chk.error, probe: '' };
      return;
    }
    wsDialog = { ...wsDialog, error: '', probe: chk.value ? '探测中…' : '' };
    if (!chk.value) return;
    try {
      await listDir(chk.value);
      wsDialog = { ...wsDialog, probe: '探测:该路径可从当前连接访问。' };
    } catch (e) {
      wsDialog = {
        ...wsDialog,
        probe: `探测:当前连接无法访问该目录(${String(e?.message || e)})——若为新能力边界属正常,以重连后实况为准。`,
      };
    }
  }

  function reconnectNow() {
    reconnectCurrent();
    wsNotice = '重连中,连接建立后文件树将按新工作区刷新…';
  }

  /** 对话框输入框自动聚焦(Svelte action) */
  function autofocus(node) {
    node.focus();
  }

  onMount(async () => {
    // 命令自注册(注册置于任何 await 之前,保证组件存活期内命令可用)
    const commands = [
      {
        id: 'explorer.newFile',
        title: '新建文件',
        category: '文件',
        run: () => startCreate('createFile', anchorFor(selected)),
      },
      {
        id: 'explorer.newFolder',
        title: '新建文件夹',
        category: '文件',
        run: () => startCreate('createDir', anchorFor(selected)),
      },
      {
        id: 'explorer.rename',
        title: '重命名',
        category: '文件',
        keybinding: 'f2',
        when: 'explorerFocus',
        run: () => selected && startRename(selected, depthOf(selected)),
      },
      {
        id: 'explorer.delete',
        title: '删除…',
        category: '文件',
        when: 'explorerFocus',
        run: () => selected && askDelete(selected),
      },
      {
        id: 'explorer.copyPath',
        title: '复制路径',
        category: '文件',
        when: 'explorerFocus',
        run: () => selected && copyPath(selected),
      },
      {
        id: 'explorer.open',
        title: '在编辑器打开',
        category: '文件',
        when: 'explorerFocus',
        run: () => selected && selected.kind === 'file' && openFile(selected.path),
      },
      {
        id: 'explorer.refresh',
        title: '刷新资源管理器',
        category: '文件',
        run: refreshAll,
      },
    ];
    for (const c of commands) registerCommand(c);
    await initTree();
    // H2:组件未挂载期间设下的定位信号(侧栏在别的视图时)挂载后补消费
    const staleReveal = get(pendingTreeReveal);
    if (staleReveal) consumeTreeReveal(staleReveal);
    // fs_events 一次性信号订阅:消费后置 null
    const unsubFs = fsEvents.subscribe((ev) => {
      if (!ev) return;
      fsEvents.set(null);
      applyFsEvents(ev);
    });
    // E1:重连(手动/自动)成功且根或工作区变化时按需重建(锚点换算在 initTree)
    let prevConn = null;
    const unsubConn = connStatus.subscribe((s) => {
      if (prevConn && prevConn !== 'online' && s === 'online') {
        wsNotice = ''; // 重连成功,提示条收起
        refreshOnReconnect();
      }
      prevConn = s;
    });
    return () => {
      for (const c of commands) unregisterCommand(c.id);
      unsubFs();
      unsubConn();
    };
  });
</script>

<div class="explorer" data-zone="explorer">
  <div class="panel-title">
    资源管理器
    {#if $connStatus === 'offline'}
      <span class="offline" title="连接离线,文件树实时刷新不可用(手动刷新兜底)">离线</span>
    {/if}
  </div>
  <div class="actions">
    <span
      class="workdir mono"
      class:drop-root={dragOverPath === anchor}
      role="button"
      tabindex="0"
      title={`树根:${anchor || 'serve workdir 根'}\n点击切换工作区(重连生效);拖动节点到此处移动到树根`}
      onclick={openWsMenu}
      onkeydown={(e) => (e.key === 'Enter' || e.key === ' ') && openWsMenu(e)}
      ondragover={(e) => dragOver(e, anchor)}
      ondragleave={() => (dragOverPath = null)}
      ondrop={(e) => dropTo(e, anchor)}
    >
      <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
        <path d="M4 5a1 1 0 0 1 1-1h5l2 2h7a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5Z" />
      </svg>
      {rootName}
      <svg class="ws-caret" viewBox="0 0 24 24" width="10" height="10" fill="none" stroke="currentColor" stroke-width="2.2">
        <path d="M6 9l6 6 6-6" />
      </svg>
    </span>
    <span class="spacer" />
    <button class="action-btn" title="新建文件" onclick={() => startCreate('createFile', anchor)}>
      <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
        <path d="M13 3H7a1 1 0 0 0-1 1v16a1 1 0 0 0 1 1h10a1 1 0 0 0 1-1V8l-5-5Z" />
        <path d="M12 11v6M9 14h6" />
      </svg>
    </button>
    <button class="action-btn" title="新建文件夹" onclick={() => startCreate('createDir', anchor)}>
      <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
        <path d="M4 5a1 1 0 0 1 1-1h5l2 2h7a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5Z" />
        <path d="M10 13h6" />
      </svg>
    </button>
    <button class="action-btn" title="刷新" onclick={refreshAll}>
      <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
        <path d="M20 12a8 8 0 1 1-2.34-5.66M20 4v4h-4" />
      </svg>
    </button>
  </div>
  {#if wsNotice}
    <!-- E1:工作区切换提示条(写面即刻持久化,生效在下次连接;立即重连可选) -->
    <div class="ws-notice" role="status">
      <span class="ws-notice-text">{wsNotice}</span>
      <button class="ws-notice-btn" onclick={reconnectNow}>立即重连</button>
      <button class="ws-notice-x" onclick={() => (wsNotice = '')} aria-label="关闭提示">×</button>
    </div>
  {/if}
  {#if loadError}
    <div class="error">加载失败:{loadError}</div>
  {:else}
    <div class="tree" role="tree" bind:this={treeEl}>
      {#if naming && naming.mode !== 'rename' && naming.anchor === anchor}
        <div class="node naming" style="padding-left: 10px">
          <input
            class="name-input"
            bind:value={naming.value}
            placeholder="文件名(支持 a/b/c 建层级)"
            aria-label="新建名称"
            onkeydown={(e) => {
              if (e.key === 'Enter') commitNaming();
              else if (e.key === 'Escape') cancelNaming();
            }}
            onblur={commitNaming}
          />
        </div>
      {/if}
      {#each rows as { node, depth } (node.path)}
        <div
          class="node {node.kind}"
          class:selected={selected && selected.path === node.path}
          class:drop-target={dragOverPath === node.path}
          class:flash={flashPath === node.path}
          data-tree-path={node.path}
          style="padding-left: {10 + depth * 14}px"
          role="treeitem"
          aria-selected={node.kind === 'file' && node.path === $activePath}
          aria-expanded={node.kind === 'dir' ? !!dirState.get(node.path)?.expanded : undefined}
          tabindex="0"
          draggable={!(naming && naming.target?.path === node.path)}
          onclick={() => open(node)}
          onkeydown={(e) => (e.key === 'Enter' || e.key === ' ') && open(node)}
          oncontextmenu={(e) => openMenu(e, node)}
          ondragstart={(e) => startDrag(e, node)}
          ondragover={(e) => node.kind === 'dir' && dragOver(e, node.path)}
          ondragleave={() => (dragOverPath = null)}
          ondrop={(e) => node.kind === 'dir' && dropTo(e, node.path)}
        >
          {#if node.kind === 'dir'}
            <svg
              class="caret"
              class:open={!!dirState.get(node.path)?.expanded}
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              stroke-width="2.2"
            >
              <path d="M9 6l6 6-6 6" />
            </svg>
            <svg class="icon dir" viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8">
              <path d="M4 5a1 1 0 0 1 1-1h5l2 2h7a1 1 0 0 1 1 1v11a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5Z" />
            </svg>
          {:else}
            <svg class="icon" viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.8">
              <path d="M7 3h7l4 4v14H7z" />
            </svg>
          {/if}
          {#if naming && naming.mode === 'rename' && naming.target.path === node.path}
            <input
              class="name-input"
              bind:value={naming.value}
              aria-label="重命名"
              onkeydown={(e) => {
                if (e.key === 'Enter') commitNaming();
                else if (e.key === 'Escape') cancelNaming();
              }}
              onblur={commitNaming}
            />
          {:else}
            <span class="name">{node.name}</span>
          {/if}
          {#if gitLetter(node)}
            <span class="git-letter {gitLetterClass(gitLetter(node))}"
              title="git:{gitLetter(node)}">{gitLetter(node)}</span>
          {/if}
        </div>
        {#if naming && naming.mode !== 'rename' && naming.anchor === node.path}
          <div class="node naming" style="padding-left: {10 + (depth + 1) * 14}px">
            <input
              class="name-input"
              bind:value={naming.value}
              placeholder="名称(支持 a/b/c 建层级)"
              aria-label="新建名称"
              onkeydown={(e) => {
                if (e.key === 'Enter') commitNaming();
                else if (e.key === 'Escape') cancelNaming();
              }}
              onblur={commitNaming}
            />
          </div>
        {/if}
      {/each}
      {#if naming?.error}
        <div class="naming-error">{naming.error}</div>
      {/if}
    </div>
  {/if}
  <!-- H3 大纲面板:视图下方折叠段(随 explorer 视图显隐) -->
  <OutlinePanel />
</div>

{#if menu}
  <ContextMenu x={menu.x} y={menu.y} items={menu.items} on:close={() => (menu = null)} />
{/if}

{#if wsMenu}
  <!-- E1:工作区下拉(最近列表 + 切换 + 恢复缺省) -->
  <ContextMenu x={wsMenu.x} y={wsMenu.y} items={wsMenu.items} on:close={() => (wsMenu = null)} />
{/if}

{#if wsDialog}
  <!-- E1:切换工作区对话框(输入框 + 校验 + 尽力而为探测;替代裸 prompt) -->
  <div
    class="ws-overlay"
    role="presentation"
    onclick={(e) => e.target === e.currentTarget && (wsDialog = null)}
  >
    <div class="ws-dialog" role="dialog" aria-modal="true" aria-label="切换工作区">
      <div class="ws-dialog-title">切换工作区</div>
      <div class="ws-dialog-hint">
        绝对路径,作为 agent 读写能力边界(重连后生效;serve workdir 之下的子目录将同时成为文件树根)。留空恢复缺省。
      </div>
      <input
        class="ws-input mono"
        bind:value={wsDialog.value}
        placeholder="如 D:\projects\demo"
        aria-label="工作区绝对路径"
        use:autofocus
        onchange={probeWsPath}
        onkeydown={(e) => {
          if (e.key === 'Enter') confirmWsDialog();
          else if (e.key === 'Escape') wsDialog = null;
        }}
      />
      {#if wsDialog.error}
        <div class="ws-dialog-err">{wsDialog.error}</div>
      {/if}
      {#if wsDialog.probe}
        <div class="ws-dialog-probe">{wsDialog.probe}</div>
      {/if}
      <div class="ws-dialog-actions">
        <button class="ws-btn-plain" onclick={() => (wsDialog = null)}>取消</button>
        <button class="ws-btn-primary" onclick={confirmWsDialog}>切换</button>
      </div>
    </div>
  </div>
{/if}

<ConfirmDialog
  open={!!confirmState}
  title="删除确认"
  message={confirmState?.message || ''}
  confirmText="删除"
  on:confirm={() => {
    const run = confirmState?.run;
    confirmState = null;
    run?.();
  }}
  on:cancel={() => (confirmState = null)}
/>

<style>
  .explorer {
    width: var(--sidebar-w, 220px); /* E2:divider 可调,根变量统一侧栏宽 */
    flex-shrink: 0;
    background: var(--sidebar-bg);
    border-right: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    overflow-y: auto;
  }
  .panel-title {
    font-size: 11px;
    font-weight: var(--fw-sb);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: rgba(255, 255, 255, 0.4);
    padding: var(--sp-sm) var(--sp-md);
    display: flex;
    align-items: center;
    gap: var(--sp-sm);
  }
  .offline {
    font-size: 10px;
    text-transform: none;
    letter-spacing: 0;
    color: var(--warning);
    border: 1px solid var(--warning);
    border-radius: 4px;
    padding: 0 4px;
  }
  .actions {
    display: flex;
    align-items: center;
    gap: 2px;
    padding: var(--sp-xs) var(--sp-sm);
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
  }
  .workdir {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: var(--fs-xs);
    color: var(--sidebar-text);
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
    min-width: 0;
    cursor: pointer;
    border-radius: var(--r-sm);
  }
  .workdir:hover {
    background: var(--sidebar-hover);
    color: var(--sidebar-text-active);
  }
  .ws-caret {
    flex-shrink: 0;
    color: var(--text-muted);
  }
  /* E1:工作区切换提示条(状态行,不占树区) */
  .ws-notice {
    display: flex;
    align-items: flex-start;
    gap: 6px;
    margin: var(--sp-xs) var(--sp-sm) 0;
    padding: 6px 8px;
    border: 1px solid var(--brand);
    border-radius: var(--r-sm);
    background: var(--brand-bg, rgba(88, 166, 255, 0.08));
    font-size: var(--fs-xs);
    line-height: var(--lh-normal);
  }
  .ws-notice-text {
    flex: 1;
    min-width: 0;
    word-break: break-all;
    color: var(--sidebar-text);
  }
  .ws-notice-btn {
    flex-shrink: 0;
    border: 1px solid var(--brand);
    border-radius: 4px;
    color: var(--brand);
    font-size: var(--fs-xs);
    padding: 1px 8px;
    white-space: nowrap;
  }
  .ws-notice-btn:hover {
    background: var(--brand-bg, rgba(88, 166, 255, 0.12));
  }
  .ws-notice-x {
    flex-shrink: 0;
    border: none;
    background: transparent;
    color: var(--text-muted);
    font-size: 13px;
    line-height: 1;
    padding: 0 2px;
  }
  /* E1:切换工作区对话框(替代裸 prompt) */
  .ws-overlay {
    position: fixed;
    inset: 0;
    z-index: 1100;
    background: rgba(0, 0, 0, 0.45);
    display: flex;
    align-items: flex-start;
    justify-content: center;
    padding-top: 18vh;
  }
  .ws-dialog {
    width: 460px;
    max-width: calc(100vw - 32px);
    background: var(--bg-card);
    border: 1px solid var(--border-strong);
    border-radius: 8px;
    box-shadow: var(--sh-modal);
    padding: var(--sp-md);
  }
  .ws-dialog-title {
    font-size: var(--fs-base);
    font-weight: var(--fw-sb);
    color: var(--text-primary);
    margin-bottom: 6px;
  }
  .ws-dialog-hint {
    font-size: var(--fs-xs);
    color: var(--text-muted);
    line-height: var(--lh-normal);
    margin-bottom: 10px;
  }
  .ws-input {
    width: 100%;
    box-sizing: border-box;
    background: var(--bg-input);
    border: 1px solid var(--border-strong);
    border-radius: 4px;
    color: var(--text-primary);
    font-size: var(--fs-sm);
    padding: 6px 8px;
    outline: none;
  }
  .ws-input:focus {
    border-color: var(--brand);
  }
  .ws-dialog-err {
    margin-top: 6px;
    font-size: var(--fs-xs);
    color: #f87171;
    word-break: break-all;
  }
  .ws-dialog-probe {
    margin-top: 6px;
    font-size: var(--fs-xs);
    color: var(--text-muted);
    line-height: var(--lh-normal);
    word-break: break-all;
  }
  .ws-dialog-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 12px;
  }
  .ws-btn-plain {
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-secondary);
    padding: 4px 14px;
  }
  .ws-btn-plain:hover {
    color: var(--text-primary);
    background: var(--bg-hover);
  }
  .ws-btn-primary {
    border: 1px solid var(--brand);
    border-radius: 4px;
    background: var(--brand);
    color: #fff;
    padding: 4px 14px;
  }
  .ws-btn-primary:hover {
    filter: brightness(1.1);
  }
  .spacer {
    flex: 1;
  }
  .action-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 22px;
    height: 22px;
    border-radius: 4px;
    color: var(--sidebar-text);
    flex-shrink: 0;
  }
  .action-btn:hover {
    background: var(--sidebar-hover);
    color: var(--sidebar-text-active);
  }
  .tree {
    padding: var(--sp-xs) 0;
  }
  .node {
    display: flex;
    align-items: center;
    gap: 5px;
    padding-right: var(--sp-md);
    height: 26px;
    font-size: var(--fs-sm);
    color: var(--sidebar-text);
    cursor: pointer;
    user-select: none;
    white-space: nowrap;
    overflow: hidden;
  }
  .node.naming {
    cursor: default;
  }
  .node:hover {
    background: rgba(255, 255, 255, 0.06);
  }
  .node.selected {
    background: var(--bg-active);
  }
  .node.drop-target {
    outline: 1px dashed var(--brand);
    outline-offset: -1px;
    background: var(--brand-bg);
  }
  /* H2:定位闪烁(面包屑/命令跳目录时 1.5s 渐隐) */
  .node.flash {
    animation: tree-flash 1.5s ease-out;
  }
  @keyframes tree-flash {
    0%,
    35% {
      background: var(--brand-bg);
      outline: 1px solid var(--brand);
      outline-offset: -1px;
    }
    100% {
      background: transparent;
      outline: none;
    }
  }
  .workdir.drop-root {
    outline: 1px dashed var(--brand);
    outline-offset: -1px;
    border-radius: var(--r-sm);
  }
  .node:focus-visible {
    outline: 1px solid var(--brand);
    outline-offset: -1px;
  }
  .caret {
    width: 10px;
    height: 10px;
    flex-shrink: 0;
    transition: transform 0.12s ease;
    color: var(--text-muted);
  }
  .caret.open {
    transform: rotate(90deg);
  }
  .icon {
    flex-shrink: 0;
    color: var(--text-muted);
  }
  .icon.dir {
    color: var(--brand);
    opacity: 0.85;
  }
  .name {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .name + .git-letter {
    margin-left: auto; /* 状态字母钉在行尾(VS Code 同款) */
  }
  .git-letter {
    font-family: var(--font-mono);
    font-size: 11px;
    font-weight: var(--fw-sb);
    flex-shrink: 0;
  }
  .git-letter.added {
    color: var(--git-added);
  }
  .git-letter.modified {
    color: var(--git-modified);
  }
  .git-letter.deleted {
    color: var(--git-deleted);
  }
  .git-letter.renamed {
    color: var(--git-renamed);
  }
  .name-input {
    flex: 1;
    min-width: 0;
    background: var(--bg-input);
    border: 1px solid var(--brand);
    border-radius: 4px;
    color: var(--text-primary);
    font-size: var(--fs-sm);
    padding: 2px 6px;
    outline: none;
  }
  .naming-error {
    margin: var(--sp-xs) var(--sp-md);
    font-size: var(--fs-xs);
    color: #f87171;
    line-height: var(--lh-normal);
    word-break: break-all;
  }
  .error {
    padding: var(--sp-md);
    font-size: var(--fs-xs);
    color: #f87171;
    line-height: var(--lh-normal);
  }
</style>
