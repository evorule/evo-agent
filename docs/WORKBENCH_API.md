# evo-agent Workbench HTTP API 调用文档

工作台（IDE workbench）端点的完整参考。本文覆盖 `docs/API.md` 未涉及的工作台面端点——文件、搜索、Git、LSP、终端、任务、AI 补全、语义索引、MCP 工作台面与工作台设置。Agent 执行面端点（`/agents/*`、`/api/sessions/*`）见 [API.md](API.md)。

> 端点清单以 `src/api/agent_api.rs` 路由装配为单一事实源（SSOT）；本文与其余端点文档随代码变更同步维护。

## 目录

- [1. 鉴权与通用约定](#1-鉴权与通用约定)
- [2. 端点一览](#2-端点一览)
- [3. 文件面](#3-文件面)
- [4. 搜索与替换](#4-搜索与替换)
- [5. Git 面](#5-git-面)
- [6. LSP 面](#6-lsp-面)
- [7. 终端面（PTY）](#7-终端面pty)
- [8. 任务面](#8-任务面)
- [9. AI 补全面](#9-ai-补全面)
- [10. 语义索引面](#10-语义索引面)
- [11. MCP 工作台面](#11-mcp-工作台面)
- [12. 工作台设置面](#12-工作台设置面)

---

## 1. 鉴权与通用约定

- 除 `/health`、`/metrics` 外，**全部端点受鉴权中间件保护**（token 缺失/无效 → `401`）。
- WS 端点在连接握手时受同一中间件覆盖，并在 WS 连接内做二次 token 校验。
- 文件/Git 路径参数一律为**相对 workdir 的路径**；越界路径（workdir 之外）一律拒绝。
- 树写操作（文件写/创建/移动/删除/替换 apply/Git 暂存区写）受全局树写互斥保护。
- 人为 UI 写操作不进 agent 审计链（与 agent 工具面的审计语义分离）。

## 2. 端点一览

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/files/list?dir=` | 目录列表 |
| GET | `/api/files/read?path=` | 读文件 |
| PUT | `/api/files/write` | 写文件（整体覆盖） |
| POST | `/api/files/create` | 创建文件/目录 |
| POST | `/api/files/move` | 移动/重命名 |
| DELETE | `/api/files?path=` | 删除（软删除进 `.evo-trash`） |
| POST | `/api/files/search` | 全局搜索 |
| POST | `/api/files/replace` | 全局搜索替换（预览/应用两段） |
| GET | `/api/git/status` | 双态 status（暂存区 vs HEAD + 工作区 vs 暂存区） |
| GET | `/api/git/diff?path=` | 文件 diff（HEAD blob vs 工作区） |
| POST | `/api/git/stage` | 暂存 |
| POST | `/api/git/unstage` | 取消暂存 |
| POST | `/api/git/discard` | 丢弃工作区改动 |
| POST | `/api/git/commit` | 提交 |
| GET | `/api/git/identity` | 查询 Git 身份（缺失只引导不代写） |
| POST | `/api/git/commit-message/generate` | AI 生成提交消息（只读不提交） |
| GET | `/api/lsp/status` | LSP 连接状态 |
| GET | `/api/lsp/ws` | LSP WebSocket（透明转发） |
| GET | `/api/pty/ws` | 终端 WebSocket |
| GET | `/api/tasks` | 列任务（读 `.evo/tasks.json`） |
| POST | `/api/tasks/run` | 运行任务 |
| POST | `/api/tasks/{run_id}/cancel` | 取消任务（两段式：优雅→5s→树杀） |
| GET | `/api/tasks/ws` | 任务输出 WebSocket |
| POST | `/api/completion` | AI 补全（ghost text） |
| POST | `/api/inline-chat` | AI Inline Chat（选中代码改写） |
| GET | `/api/index/search?query=` | 语义索引符号检索 |
| GET | `/api/index/status` | 语义索引就绪度 |
| POST | `/api/index/rebuild` | 全量重建索引（人工触发） |
| GET / PUT | `/api/mcp/servers` | MCP server 配置清单（GET 视图 / PUT 全量写回） |
| POST | `/api/mcp/servers/{name}/restart` | 重启单个 MCP server |
| GET | `/api/mcp/status` | MCP 连接态摘要 |
| GET / PUT | `/api/workbench/config` | 工作台配置（快照保留期） |
| GET / PUT | `/api/workbench/settings` | 工作台设置（两级合并读写） |
| GET | `/api/workbench/settings/schema` | 设置 schema（前后端零双声明） |

## 3. 文件面

全部委托 builtin file 工具实现（同一沙箱与校验）；写面为人工编辑语义。

### GET `/api/files/list?dir=<相对目录>`

`dir` 缺省 = workdir 根。返回目录条目列表。

### GET `/api/files/read?path=<相对路径>`

返回文件内容。路径不存在 → `404`。

### PUT `/api/files/write`

```json
{ "path": "src/main.rs", "content": "……完整文件内容……" }
```

整体覆盖写。

### POST `/api/files/create`

```json
{ "path": "src/new_mod.rs", "kind": "file" }
```

`kind`: `file`（缺省）| `dir`。目标必须不存在（已存在 → `409`）。

### POST `/api/files/move`

```json
{ "path": "a.txt", "target_dir": "docs", "new_name": "b.txt" }
```

`new_name` 缺省 = 保留原名；`target_dir` 必须已存在。

### DELETE `/api/files?path=<相对路径>`

软删除：条目移入 `.evo-trash`，不直接销毁。

## 4. 搜索与替换

### POST `/api/files/search`

```json
{
  "query": "TODO",
  "is_regex": false,
  "case_sensitive": false,
  "whole_word": false,
  "smart_case": true,
  "dir": ".",
  "include_globs": ["*.rs"],
  "exclude_globs": [],
  "use_ignore_files": true,
  "max_results": 200
}
```

仅 `query` 必填，其余缺省如上。30s 硬超时返回已收集部分（`truncated` + `timedOut` 标注）。

### POST `/api/files/replace`

搜索参数同上，另加：

```json
{ "replacement": "新文本", "apply": false, "paths": ["a.rs", "b.rs"] }
```

- `apply=false`（缺省）：预览 `{preview, fileCount, matchCount}`，零写盘。
- `apply=true`：**重新匹配**（不信任预览快照）后逐文件 tmp+rename 原子写，返回 `{appliedFiles, appliedMatches, failed}`。属写面，anonymous 身份 `403`。

## 5. Git 面

薄委托 git 核心层；身份缺失返回结构化引导（不代写 config）。非 git 仓库 / worktree 形态 → `400`。

| 端点 | 请求体 |
|------|--------|
| `POST /api/git/stage` / `unstage` / `discard` | `{ "paths": ["a.rs", "dir/"] }`（untracked 目录折叠条目带尾 `/`） |
| `POST /api/git/commit` | `{ "message": "……" }`（空消息拒绝） |
| `GET /api/git/diff?path=` | — |
| `GET /api/git/identity` | — |

### POST `/api/git/commit-message/generate`

staged diff → 三级截断 → LLM 生成提交消息。请求体 `{}`，**只读不提交**。

## 6. LSP 面

人工编辑辅助，零 agent 工具面。每语言单连接单实例；服务异常自动退避重启（60s×3 → Failed）。

- `GET /api/lsp/status` — 各语言连接状态（pid、就绪度、重启计数）。
- `GET /api/lsp/ws` — WebSocket 透明转发；前端 JSON-RPC 消息层 + Full 同步去抖。

## 7. 终端面（PTY）

`GET /api/pty/ws` — WebSocket。人的终端（会话留痕 + 会话内零拦截治理分级）；并发上限 8；控制帧白名单；断连树杀。

## 8. 任务面

任务定义读工作区 `.evo/tasks.json`（`tasks: [{label, type: process|shell, command, args, options, group, problem_matcher}]`）。治理红线：**任务永不自动运行**（人的显式触发）。

- `GET /api/tasks` — 列任务定义。
- `POST /api/tasks/run` — `{ "label": "build" }`。并发护栏：总数上限 + 同 label 互斥（冲突 → `409`）。
- `POST /api/tasks/{run_id}/cancel` — 两段式取消：优雅终止 → 5s → 树杀。
- `GET /api/tasks/ws?run_id=<id>&token=<token>` — 任务输出流（stdout/stderr 合流行 + 生命周期事件；诊断桥 owner=`task.<label>`）。

## 9. AI 补全面

LLM 外发面；prompt 组装与 `.evoignore` 排除在前端层执行。

### POST `/api/completion`

```json
{ "prefix": "光标前代码", "suffix": "光标后代码", "lang": "rust" }
```

`suffix`/`lang` 可选（serve 侧对 prefix/suffix 再做一次预算裁剪兜底）。返回续写建议（ghost text 消费）。

### POST `/api/inline-chat`

```json
{ "instruction": "给函数加 doc 注释", "code": "选中的代码", "path": "src/main.rs", "lang": "rust" }
```

`path`/`lang` 可选。返回改写后的代码（前端 computeDiff 渲染红删绿增预览；Accept = applyEdits 落盘）。

## 10. 语义索引面

结构面 v1。`index.enabled=false`（索引服务未注入）时三个端点一律 `503`。

- `GET /api/index/search?query=<符号>` — 符号检索。
- `GET /api/index/status` — 就绪度。
- `POST /api/index/rebuild` — 全量重建（人工触发写面）。

## 11. MCP 工作台面

未启用态（MCP 关闭）一律 `503`。

- `GET /api/mcp/servers` — 配置清单视图。**env 值不出现在 GET 视图**（只回 `env_keys`，防泄露）。
- `PUT /api/mcp/servers` — `{"servers":[{name, command, args, env?, disabled?, timeout_secs?, always_allow?}]}`。校验 → 写回 `mcp.toml` → 全量重载。**env 省略 = 继承现有值**（前端写回不误清密钥）；显式提供（含空对象）= 覆盖。
- `POST /api/mcp/servers/{name}/restart` — 重启单个 server。
- `GET /api/mcp/status` — 连接态摘要（ok/failed/disabled）。

## 12. 工作台设置面

- `GET /api/workbench/config` / `PUT` — 快照保留期配置（`retention` 标签，如 `"3m"`；删除只作用于 `data/snapshots/`，永不越界）。
- `GET /api/workbench/settings` — 两级（默认层 + 用户层 `data/workbench_settings.json`）合并后全量 + 每键生效层标注。
- `PUT /api/workbench/settings` — 写用户层单键（`{ "key": "editor.fontSize", "value": 14 }`；`value: null` = 重置该键；非法键/值 → `400`；原子写）。
- `GET /api/workbench/settings/schema` — 全部设置键 schema（SSOT，前端选择器据此渲染，前后端零双声明）。
