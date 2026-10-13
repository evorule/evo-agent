// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// LSP ↔ Monaco 特性映射层:5 特性(completion/hover/definition/documentSymbol/
// diagnostics)的协议形状转换 + Monaco provider 注册。
//
// 纯函数(lspSeverityToMonaco/hoverToMarkdown/normalizeLocations/mapCompletionItems/
// diagnosticsToMarkers)零 monaco 静态依赖,可独立单测;注册入口 attachFeatures
// 由装配层传入 monaco 单例。

/** LSP DiagnosticSeverity(1=Error 2=Warning 3=Info 4=Hint)→ Monaco marker severity(8/4/2/1) */
export function lspSeverityToMonaco(severity) {
  switch (severity) {
    case 1:
      return 8; // Error
    case 2:
      return 4; // Warning
    case 3:
      return 2; // Info
    case 4:
      return 1; // Hint
    default:
      return 8; // 缺省按最严重呈现(不静默吞错)
  }
}

/**
 * LSP Hover → Monaco IMarkdownString[]。
 * contents 形态(MarkupContent | MarkedString | MarkedString[])统一归一为 markdown。
 */
export function hoverToMarkdown(hover) {
  if (!hover || hover.contents == null) return { contents: [] };
  const c = hover.contents;
  const toMarkdown = (piece) => {
    if (typeof piece === 'string') return { value: piece };
    if (piece && typeof piece === 'object' && 'language' in piece) {
      return { value: `\`\`\`${piece.language}\n${piece.value ?? ''}\n\`\`\`` };
    }
    if (piece && typeof piece === 'object' && 'value' in piece) {
      return { value: piece.value }; // MarkupContent(kindmarkdown/plaintext 同形呈现)
    }
    return { value: String(piece ?? '') };
  };
  const contents = Array.isArray(c) ? c.map(toMarkdown) : [toMarkdown(c)];
  return { contents, range: hover.range ? lspRangeToMonacoRange(hover.range) : undefined };
}

/** LSP range(0-based)→ Monaco range(1-based) */
export function lspRangeToMonacoRange(range) {
  return {
    startLineNumber: (range.start?.line ?? 0) + 1,
    startColumn: (range.start?.character ?? 0) + 1,
    endLineNumber: (range.end?.line ?? range.start?.line ?? 0) + 1,
    endColumn: (range.end?.character ?? range.start?.character ?? 0) + 1,
  };
}

/**
 * LSP definition 响应归一为 Location[](Location | Location[] | LocationLink[] | null)。
 * 返回 [{uri, range}];LocationLink 取 targetUri/targetSelectionRange(缺省 targetRange)。
 */
export function normalizeLocations(result) {
  if (!result) return [];
  const list = Array.isArray(result) ? result : [result];
  const out = [];
  for (const item of list) {
    if (!item || typeof item !== 'object') continue;
    if ('targetUri' in item) {
      out.push({ uri: item.targetUri, range: item.targetSelectionRange || item.targetRange });
    } else if ('uri' in item) {
      out.push({ uri: item.uri, range: item.range });
    }
  }
  return out;
}

/**
 * LSP CompletionItemKind(1-25)→ Monaco kind 解析器工厂。
 * kindTable 传 monaco.languages.CompletionItemKind 形状(名字→数值);
 * 未知 kind 回落 Text/Plain。
 */
export function makeKindResolver(kindTable, fallback) {
  // LSP kind 序(tab 顺序)与 Monaco 枚举名对齐;数值由运行时表解析
  const LSP_KIND_NAMES = [
    null, 'Text', 'Method', 'Function', 'Constructor', 'Field', 'Variable', 'Class',
    'Interface', 'Module', 'Property', 'Unit', 'Value', 'Enum', 'Keyword', 'Snippet',
    'Color', 'File', 'Reference', 'Folder', 'EnumMember', 'Constant', 'Struct', 'Event',
    'Operator', 'TypeParameter',
  ];
  return function resolveKind(lspKind) {
    const name = LSP_KIND_NAMES[lspKind] || 'Text';
    return (kindTable && kindTable[name] != null ? kindTable[name] : fallback);
  };
}

/**
 * LSP CompletionItem[] → Monaco CompletionItem[](纯映射;range 由调用方给词边界)。
 * insertText:取 insertText 或 label;snippet 格式(insertTextFormat=2)v1 按原文插入。
 */
export function mapCompletionItems(items, resolveKind, range) {
  const list = Array.isArray(items) ? items : (items?.items ?? []);
  return list
    .filter((it) => it && (it.label != null))
    .map((it) => {
      const insert = it.insertText ?? (typeof it.label === 'string' ? it.label : String(it.label.text ?? it.label));
      const out = {
        label: typeof it.label === 'string' ? it.label : (it.label.label ?? ''),
        kind: resolveKind(it.kind ?? 1),
        insertText: insert,
        range,
      };
      if (it.detail != null) out.detail = it.detail;
      if (it.documentation != null) {
        out.documentation =
          typeof it.documentation === 'string'
            ? { value: it.documentation }
            : { value: it.documentation?.value ?? '' };
      }
      if (it.filterText != null) out.filterText = it.filterText;
      if (it.sortText != null) out.sortText = it.sortText;
      if (it.deprecated) out.tags = [1]; // monaco CompletionItemTag.Deprecated
      return out;
    });
}

/**
 * publishDiagnostics params → (markers, path) 或 null(不可消费:无 evoPath)。
 * version 比对由调用方做(需 model 实时版本)。
 */
export function diagnosticsToMarkers(params) {
  const path = params?.evoPath;
  if (typeof path !== 'string' || !path) return null;
  const diags = Array.isArray(params.diagnostics) ? params.diagnostics : [];
  const markers = diags.map((d) => ({
    message: d?.message ?? '',
    severity: lspSeverityToMonaco(d?.severity),
    startLineNumber: (d?.range?.start?.line ?? 0) + 1,
    startColumn: (d?.range?.start?.character ?? 0) + 1,
    endLineNumber: (d?.range?.end?.line ?? d?.range?.start?.line ?? 0) + 1,
    endColumn: (d?.range?.end?.character ?? d?.range?.start?.character ?? 0) + 1,
    source: d?.source ?? undefined,
    code: d?.code ?? undefined,
  }));
  return { path, markers };
}

/**
 * 注册本语言门的 5 特性 Monaco providers。
 *
 * @returns dispose():注销全部注册
 */
export function attachFeatures({ monaco, client, languageIds, jump, resolveKind }) {
  const disposers = [];
  const wordRangeFor = (model, position) => {
    const word = model.getWordUntilPosition(position);
    return {
      startLineNumber: position.lineNumber,
      endLineNumber: position.lineNumber,
      startColumn: word.startColumn,
      endColumn: word.endColumn,
    };
  };

  // completion:触发字符随 server capabilities;静态注册(v1 忽略 dynamic registration)
  const completionTriggers = client.capabilities?.completionProvider?.triggerCharacters?.length
    ? client.capabilities.completionProvider.triggerCharacters
    : ['.', ':', '/', '"'];
  disposers.push(
    monaco.languages.registerCompletionItemProvider(languageIds, {
      triggerCharacters: completionTriggers,
      async provideCompletionItems(model, position) {
        if (client.state !== 'ready') return { suggestions: [] };
        try {
          const result = await client.completion(model, position);
          const items = mapCompletionItems(result, resolveKind, wordRangeFor(model, position));
          return { suggestions: items };
        } catch {
          return { suggestions: [] }; // 查询失败静默(补全是增强面)
        }
      },
    }),
  );

  // hover
  disposers.push(
    monaco.languages.registerHoverProvider(languageIds, {
      async provideHover(model, position) {
        if (client.state !== 'ready') return null;
        try {
          const hover = await client.hover(model, position);
          return hoverToMarkdown(hover);
        } catch {
          return null;
        }
      },
    }),
  );

  // definition:Location[] → jump(path, reveal)
  disposers.push(
    monaco.languages.registerDefinitionProvider(languageIds, {
      async provideDefinition(model, position) {
        if (client.state !== 'ready') return null;
        try {
          const locations = normalizeLocations(await client.definition(model, position));
          for (const loc of locations) {
            const path = uriToRelativeForJump(loc.uri);
            if (path) {
              jump(path, {
                line: (loc.range.start?.line ?? 0) + 1,
                col: loc.range.start?.character ?? 0,
                endCol: loc.range.end?.character ?? (loc.range.start?.character ?? 0),
              });
              return null; // 跳转已发起(monaco 侧无需再渲染)
            }
          }
          return null;
        } catch {
          return null;
        }
      },
    }),
  );

  return () => {
    for (const d of disposers) {
      try {
        d.dispose();
      } catch {
        /* 清理失败不阻断 */
      }
    }
  };
}

/** Location uri → 工作区相对路径(诊断桥同款换算在前端侧的跳转场景):
 *  evoPath 已由 serve 附加在推送上;definition 的 Location.uri 是原始 file://,
 *  此处按 workdir 前缀截取(装配层注入 workdirBase 后覆写本函数)。 */
let workdirBaseRef = '';
export function setWorkdirBase(base) {
  workdirBaseRef = String(base).replace(/\/+$/, '');
}
export function uriToRelativeForJump(uri) {
  if (typeof uri !== 'string' || !uri.startsWith('file:///')) return null;
  let p;
  try {
    p = decodeURIComponent(uri.slice('file:///'.length));
  } catch {
    return null;
  }
  const base = workdirBaseRef.replace(/^\/+/, '');
  if (base && p.startsWith(base + '/')) return p.slice(base.length + 1);
  return p; // 工作区外:返回解码绝对路径(jump 层按未知路径处理)
}
