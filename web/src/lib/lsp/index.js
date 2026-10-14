// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// LSP 装配入口:语言门发现(status)/按需建连/monaco model 路由/诊断桥/
// 生命周期事件落输出通道/Failed 横幅与手动重启。
//
// 装配(initLsp)由编辑器宿主(EditorPane onMount)调用一次,返回清理函数。
// 连接策略:**按语言门懒建**——首个该语言 model 打开才连 WS(spawn 语言服务器
// 是重操作,启动期零开销);lsp.enabled=false 或 PATH 缺失的门不建连。

import { writable, get } from 'svelte/store';
import { monaco as monacoSingleton } from '../monaco-setup.js';
import { settingsState } from '../settings.js';
import { pushPanelEvent, openFile } from '../stores.js';
import { uriSpecFor, setDiagnostics, clearDiagnostics, pathFromUri } from '../diagnostics.js';
import { getLspStatus } from '../api.js';
import { createLspClient, pathToFileUri, LSP_STATES } from './client.js';
import {
  attachFeatures,
  diagnosticsToMarkers,
  makeKindResolver,
  setWorkdirBase,
  uriToRelativeForJump,
} from './features.js';

/** Failed 态横幅({lang, serverName, detail} | null):App.svelte 消费 */
export const lspBanner = writable(null);

/** 两语言门实时状态快照:{rust:{state,...}, typescript:{...}}(状态查询命令消费) */
export const lspGates = writable({});

/** documentSymbol 结果存档(Map<path, symbols>;v1 无 UI,上下文板块消费) */
export const symbolStore = writable(new Map());

// ---- 模块级装配态 ----

let inited = false;
let cleanupFns = [];
/** lang → client 实例(懒建) */
const clients = new Map();
/** lang → {available, version, hint}(status 探测结果) */
const gateStatus = new Map();
let workdirBase = '';
let enabled = true;

/** monaco 语言 id → 语言门(null = 不在门内) */
function gateOfLanguage(monacoLangId) {
  if (monacoLangId === 'rust') return 'rust';
  if (monacoLangId === 'typescript' || monacoLangId === 'javascript') return 'typescript';
  return null;
}

function ownerOf(lang) {
  return `lsp.${lang === 'rust' ? 'rust-analyzer' : 'typescript-language-server'}`;
}

function logLine(detail, level = 'log') {
  pushPanelEvent('sys', { label: '[lsp]', detail, level: level === 'error' ? 'error' : level === 'warning' ? 'warn' : undefined });
}

/** 诊断入口:清理信号 → clear;推送 → 版本比对 + 单源下发 */
function handleDiagnostics(lang, params) {
  const owner = ownerOf(lang);
  if (params && typeof params.evoOwnerReset === 'string') {
    clearDiagnostics(params.evoOwnerReset, owner);
    return;
  }
  const converted = diagnosticsToMarkers(params);
  if (!converted) return; // 工作区外 URI(evoPath=null):灰置不呈现
  const { path, markers } = converted;
  // 过期推送丢弃:版本落后于 model 实时版本 → 等新推送(防旧结果覆盖新态)
  if (params && params.version != null) {
    const model = monacoSingleton.editor.getModel(monacoSingleton.Uri.parse(uriSpecFor(path)));
    if (model && params.version < model.getVersionId()) return;
  }
  setDiagnostics(path, owner, markers);
}

/** 生命周期/日志事件:输出通道 + 横幅 */
function handleEvent(lang, serverName, event) {
  if (event.kind === 'log') {
    logLine(`${serverName}: ${event.message}`, event.level);
    return;
  }
  // state 事件
  updateGateSnapshot(lang);
  if (event.state === LSP_STATES.FAILED) {
    lspBanner.set({ lang, serverName, detail: event.detail || 'restart budget exhausted' });
    logLine(`${serverName}: failed — ${event.detail || ''}`, 'error');
  } else if (event.state === LSP_STATES.UNAVAILABLE) {
    logLine(`${serverName} 不可用:${event.detail || ''}`, 'warning');
    lspBanner.update((b) => (b && b.lang === lang ? null : b));
  } else if (event.state === LSP_STATES.READY) {
    logLine(`${serverName}: ${event.detail || 'ready'}`);
    lspBanner.update((b) => (b && b.lang === lang ? null : b));
  } else {
    logLine(`${serverName}: ${event.state}${event.detail ? ` — ${event.detail}` : ''}`);
  }
}

function updateGateSnapshot(lang) {
  const client = clients.get(lang);
  lspGates.update((g) => ({
    ...g,
    [lang]: {
      state: client ? client.state : 'idle',
      pathAvailable: gateStatus.get(lang)?.available ?? null,
      version: gateStatus.get(lang)?.version ?? null,
      docs: client ? client.trackedPaths.length : 0,
    },
  }));
}

/** 建(或重建)某语言门 client 并连接 */
function ensureClient(lang) {
  if (clients.has(lang) || !enabled || !workdirBase) return clients.get(lang);
  if (!gateStatus.get(lang)?.available) return null; // PATH 缺失:灰置,不建连
  const serverName = lang === 'rust' ? 'rust-analyzer' : 'typescript-language-server';
  const token = localStorage.getItem('evo_agent_token') || '';
  const wsUrl = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/api/lsp/ws?lang=${lang}${token ? `&token=${encodeURIComponent(token)}` : ''}`;
  const client = createLspClient({
    lang,
    serverName,
    wsUrl,
    workdirBase,
    onEvent: (event) => handleEvent(lang, serverName, event),
    onDiagnostics: (params) => handleDiagnostics(lang, params),
    pathFromModel: (model) => pathFromUri(model.uri),
  });
  clients.set(lang, client);
  client.connect();
  // 特性注册:resolveKind 用 monaco 枚举名表;languageIds 覆盖门内语言
  const kindTable = monacoSingleton.languages.CompletionItemKind;
  const languageIds = lang === 'rust' ? 'rust' : ['typescript', 'javascript'];
  const disposeFeatures = attachFeatures({
    monaco: monacoSingleton,
    client,
    languageIds,
    jump: (path, reveal) => openFile(path, reveal),
    resolveKind: makeKindResolver(kindTable, kindTable?.Text ?? 18),
  });
  cleanupFns.push(disposeFeatures);
  updateGateSnapshot(lang);
  // 既有 model 补注册:onDidCreateModel 只在创建时发一次,重启/重建 client
  // 后已打开的文档不会再次触发——不补注册则重启丢失全部文档跟踪(didOpen
  // 与后续 didChange 都无从谈起)
  for (const m of monacoSingleton.editor.getModels()) {
    const path = pathFromUri(m.uri);
    if (path && gateOfLanguage(m.getLanguageId()) === lang) client.trackModel(m);
  }
  return client;
}

/** 路由 model 到语言门(创建/关闭生命周期) */
function routeModelCreate(model) {
  if (!enabled) return;
  const path = pathFromUri(model.uri);
  if (!path) return; // 草稿/错误占位/git-diff 等非正式 model
  const lang = gateOfLanguage(model.getLanguageId());
  if (!lang) return;
  const client = ensureClient(lang);
  if (!client) return;
  client.trackModel(model);
}

function routeModelDispose(model) {
  const lang = gateOfLanguage(model.getLanguageId());
  if (!lang) return;
  clients.get(lang)?.untrackModel(model);
}

/** 关闭某门(设置关闭/重启前置):断连+清诊断+清特性注册 */
function teardownGate(lang) {
  const client = clients.get(lang);
  if (!client) return;
  const owner = ownerOf(lang);
  for (const path of client.trackedPaths) {
    clearDiagnostics(path, owner);
  }
  client.dispose();
  clients.delete(lang);
  updateGateSnapshot(lang);
}

/** 设置开关翻转入口 */
function applyEnabled(next) {
  if (next === enabled) return;
  enabled = next;
  if (!enabled) {
    for (const lang of [...clients.keys()]) teardownGate(lang);
    lspBanner.set(null);
    logLine('lsp.enabled=false — 全部语言门已关闭');
  } else {
    logLine('lsp.enabled=true — 语言门按需就绪(打开对应语言文件即连)');
  }
}

/**
 * 装配(幂等)。返回清理函数(断连/摘订阅/复位 store)。
 */
export async function initLsp() {
  if (inited) return () => {};
  inited = true;
  const monaco = monacoSingleton;

  enabled = get(settingsState).settings['lsp.enabled'] !== false;
  const unsubSettings = settingsState.subscribe((s) => {
    applyEnabled(s.settings['lsp.enabled'] !== false);
  });

  // 语言门探测(status 端点同时下发工作区根)
  try {
    const status = await getLspStatus();
    workdirBase = String(status.workdir || '').replace(/\/+$/, '');
    setWorkdirBase(workdirBase);
    gateStatus.set('rust', status.rust ?? { available: false });
    gateStatus.set('typescript', status.typescript ?? { available: false });
    if (!status.rust?.available) {
      logLine(`rust 门灰置:${status.rust?.hint || 'rust-analyzer 未安装'}`, 'warning');
    }
    if (!status.typescript?.available) {
      logLine(`typescript 门灰置:${status.typescript?.hint || 'typescript-language-server 未安装'}`, 'warning');
    }
  } catch (e) {
    logLine(`status 探测失败(${String(e?.message || e)});语言门按缺失处理`, 'warning');
    gateStatus.set('rust', { available: false });
    gateStatus.set('typescript', { available: false });
  }

  const d1 = monaco.editor.onDidCreateModel(routeModelCreate);
  const d2 = monaco.editor.onWillDisposeModel(routeModelDispose);
  for (const m of monaco.editor.getModels()) routeModelCreate(m); // 启动时序兜底
  cleanupFns.push(() => {
    d1.dispose();
    d2.dispose();
  });

  return function disposeLsp() {
    unsubSettings();
    for (const lang of [...clients.keys()]) teardownGate(lang);
    for (const fn of cleanupFns) {
      try {
        fn();
      } catch {
        /* 清理失败不阻断 */
      }
    }
    cleanupFns = [];
    gateStatus.clear();
    workdirBase = '';
    inited = false;
    lspBanner.set(null);
  };
}

/** 手动重启某语言门(Failed 横幅按钮/`LSP: 重启` 命令) */
export function restartGate(lang) {
  teardownGate(lang);
  // 延迟重连:serve 侧旧桥的「断连收尸→注册表释放」与关闭回包存在毫秒级
  // 竞态,立即重连会撞 409(连接即断,门卡死在 stopped);800ms 足够收尾
  setTimeout(() => {
    if (enabled) ensureClient(lang);
  }, 800);
}

/** 保存完成转发(didSave;flycheck 类服务器依赖)——saveActive 成功路径调用 */
export function notifySaved(path) {
  if (!enabled) return;
  for (const client of clients.values()) {
    if (client.trackedPaths.includes(path)) client.didSave(path);
  }
}

/** 当前门状态快照(状态命令消费) */
export function gateSnapshot() {
  return get(lspGates);
}

/** documentSymbol 按需请求入 store(上下文板块消费;v1 无 UI) */
export async function requestDocumentSymbol(path, model) {
  const lang = gateOfLanguage(model.getLanguageId());
  const client = lang && clients.get(lang);
  if (!client || client.state !== LSP_STATES.READY) return null;
  try {
    const symbols = await client.documentSymbol(model);
    symbolStore.update((m) => new Map(m).set(path, symbols));
    return symbols;
  } catch {
    return null;
  }
}

/**
 * workspace/symbol 按需查询(@上下文符号候选消费):遍历已就绪语言门并发
 * 查询,SymbolInformation 归一化为 {name, kind, path, line}(path 经
 * workdirBase 截取为工作区相对路径,line 1-based)。
 * 返回 null = 无可用门(lsp.enabled=false 或无就绪 client,调用方走
 * symbolStore 本地兜底);[] = 门就绪但无结果。
 */
export async function requestWorkspaceSymbol(query) {
  if (!enabled) return null;
  const ready = [...clients.values()].filter((c) => c.state === LSP_STATES.READY);
  if (!ready.length) return null;
  const out = [];
  for (const client of ready) {
    try {
      const syms = await client.rpc.request('workspace/symbol', { query }, 8_000);
      if (!Array.isArray(syms)) continue;
      for (const s of syms) {
        const path = uriToRelativeForJump(s?.location?.uri);
        const ln = s?.location?.range?.start?.line;
        if (!path || typeof s?.name !== 'string' || !Number.isFinite(ln)) continue;
        out.push({ name: s.name, kind: s.kind, path, line: ln + 1 });
      }
    } catch {
      /* 单门失败不阻断其他门 */
    }
  }
  return out;
}

export { LSP_STATES, pathToFileUri };
