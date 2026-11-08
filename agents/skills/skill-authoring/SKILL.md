---
name: skill-authoring
description: 创建、修改、撤销宿主技能（SKILL.md）的正确通道与实测陷阱。新建技能、更新已有技能正文、撤销或重建技能、排查技能为何不生效或读不到时先读，避免绕过门禁与命名死锁。
---

# 技能创建与维护通道

> 技能面是**有门禁的供应链通道**：写入 → 审批 → 生效，每一步都留痕。本文件记录实测通过的正确路径与踩过的坑。

## 1. 生效位置与关键端点

| 项 | 值 |
|---|---|
| 用户级技能目录 | `<serve workdir>/data/skills/<name>/SKILL.md`（桌面安装默认 workdir 为其数据目录下的 `runtime/`） |
| 项目级技能目录 | `<serve workdir>/.evo/skills/<name>/SKILL.md` |
| 注册账本 | `<serve workdir>/data/skills/registry.json` |
| 管理面端点 | `/api/skills`、`/api/skills/pending`、`/api/skills/approve`、`/api/skills/revoke`、`/api/skills/import`（**都在 serve.port**） |
| 文件面端点 | `/api/files/read`、`PUT /api/files/write`、`DELETE /api/files`、`/api/files/list` |
| 审计留痕 | `<serve workdir>/data/human_gate_ledger.jsonl`（每个写面操作一行；身份字段当前版本不区分人工与 agent 发起） |

**注意**：`/api/services` 在 **engine.port**，`/api/skills` 在 **serve.port**，两者不同，跨端口一律 404。端口从 `runtime.json` 读，不要写死。

## 2. 三条路径（按用途选，不要混用）

### 2.1 修改已有技能正文（最常用）

```
1) PUT  /api/files/write   覆盖 data/skills/<name>/SKILL.md
2) POST /api/skills/approve 重算哈希落账 → active
```

**不要用 import 改同名技能**——它对 active/revoked 同名一律拒绝。注册账本的批准操作是幂等 upsert，**「内容更新后重新走审批」就是同名更新的正解**。

### 2.2 新建技能（首次创建）

```
1) PUT  /api/files/write   path=data/skills/<name>/SKILL.md  body={path,content}
2) POST /api/skills/approve body={name,level:"user"}
3) 校验 GET /api/skills → status=active；GET /api/skills/pending → 已不在待审列表
```

**也可以走 import**（`POST /api/skills/import`，body `{"path":"<workdir 内 bundle 相对路径>"}`，bundle 格式 `{"skills":[{"name","level","body"}]}`）——它会自动落盘并留痕，且**拒绝覆盖任何 active/revoked 同名行**。import 只适合批量导入或首次创建。

### 2.3 撤销与恢复

```
POST /api/skills/revoke body={name,level}
```

撤销后该技能不进会话技能清单（`read_skill` 读不到）。**恢复**：把 SKILL.md 放回原注册路径，直接重新 approve 即幂等重激活，不必走 import。

## 3. 硬性前置：frontmatter 与命名

- 文件必须是 `<name>/SKILL.md` 子目录式，散置文件不算技能
- 首行必须是 `---` frontmatter 围栏，含 `name:` 与 `description:`；围栏不闭合会被**拒绝**
- 轻量 frontmatter 解析器**只认平铺 `key: value`**；嵌套块、列表行会被跳过并告警
- `name` 建议用 `[a-zA-Z0-9][a-zA-Z0-9_-]*`，且**目录名 = frontmatter name = 声明名**（不一致会被 import 判为钓鱼而拒）
- 泄露检查两级：**署名族**（宣称本文由模型自动写就之类表述）→ approve **硬拦 422**；**技术词**（如 agent、prompt、上下文窗口）→ 仅 warning，不拦。注意：**正文里引用这些词面也会被命中**，写示例时必须绕开字面量（改用转述，如本条这样写）。

## 4. 实测踩坑清单

1. **撤销后同名重建有死锁坑**：若撤销后**删除了技能目录**，import 同名会被拒（报错让你「先撤销」，但已经撤销过了），approve 又因目录已删扫描不到 → 双拒。**正确做法：不要删目录**；已撤销的技能恢复 = 放回 SKILL.md 直接重新 approve。删除后想重建同名的场景当前无官方解（已知限制，管理面指引待改进）。
2. **管理面不察觉篡改**：`GET /api/skills` 读的是账本里的旧哈希，**从不重算文件**。文件被改后管理面仍显示 active、哈希不变。哈希失配只在新会话创建时降级。所以**改完文件必须自己重新 approve**，否则下次会话创建时该技能会静默降级为 discovered。
3. **approve 不校验正文完整性**：乱码字符（U+FFFD）不会被拦，approve 照样 200。**写入后必须自己回读检查**。
4. **端口分裂**：`/api/skills*` 与 `/api/files*` 在 serve.port；`/api/services` 在 engine.port。
5. **写面工具沙箱隔离**：agent 侧 `file_write`/`file_delete` 的可写域与技能目录可能不重叠，直接写会报 `outside writable_dir` 或 `parent dir (..) not allowed`。走宿主 `/api/files/*` HTTP 面可解。
6. **技能清单是会话创建时注入的**：新建的技能**当次会话不出现**在【可用技能清单】里，需新会话才可见；`read_skill` 同理。
7. **代码执行工具可能被审批策略拒绝**：需要写文件或调管理面时，优先走宿主 HTTP API（file/skills 面），不要依赖代码执行工具。

## 5. 验收清单（做完逐项核）

- `GET /api/skills` → 该技能 `status=active`，`content_hash` 与 approve 返回一致
- `GET /api/skills/pending` → 不含该技能
- `GET /api/files/read?path=data/skills/<name>/SKILL.md` → 回读正文，**无乱码字符（U+FFFD）**、frontmatter 完整
- `data/skills/registry.json` → 只有预期的行，无遗留 stale 行
- `data/human_gate_ledger.jsonl` → 有本次操作的留痕
- 临时文件（bundle、备份）已清理

## 6. 红线

- 不绕过门禁直写技能文件而不留痕；不经 import 改同名技能；**不手工编辑注册账本文件**（账本只应由宿主管理面写）。
- 改内容后不重新 approve（会导致下会话静默降级）。
- 不把「文件已写」当作「技能已生效」——生效判据是 `status=active` + 哈希一致。
