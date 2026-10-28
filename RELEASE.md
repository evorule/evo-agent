# Desktop 发版手册（RELEASE.md）

本文是 `.github/workflows/desktop-release.yml` 人工环节的权威程序。CI 自动化到产物上传为止，最后一步人工下载并发布 GitHub Release。

## 1. 版本准备（四处版本号）

发版前将四处版本号同步为目标版本：

| # | 文件 | 说明 |
|---|---|---|
| 1 | `desktop/src-tauri/tauri.conf.json` | `version` 字段——**CI 版本闸检查项**（tag 事件） |
| 2 | `desktop/src-tauri/Cargo.toml` | `version = "x.y.z"` |
| 3 | `desktop/Cargo.lock` | `evo-agent-desktop` 包条目（注意：位于 `desktop/`，不在 `src-tauri/`） |
| 4 | `desktop/package.json` | `version` 字段 |

## 2. 触发构建

- **标准路径**：打 tag `evo-agent-desktop-vX.Y.Z` 并推送双端。tag 触发版本闸：tag 名去前缀后必须等于 `tauri.conf.json` 的 `version`，不等则构建失败。
- **工具路径**：`workflow_dispatch`（无输入）从 main HEAD 构建，无版本闸——用于 CI 修复重跑；产物若直接用于发版，须在台账注明产出路径。
- 同 SHA 重推 tag 不会重新触发（ref 无变化即无事件）。tag 需改指新 SHA 时用 `git tag -f` 后双端重推。

## 3. CI 产物

workflow 产物名 `evo-agent-desktop-release`，包含：

- 安装器 `setup.exe`（NSIS）
- 签名文件 `.sig`（TAURI_SIGNING 密钥缺失时 fail-closed，属配置缺失而非代码缺陷）
- `latest.json`（更新元数据）
- 源码 tarball

## 4. 人工发布（本手册核心环节）

1. 从 Actions run 页下载 artifact `evo-agent-desktop-release` 并解包。
2. 到 `github.com/evorule/evo-agent-desktop` 创建 release：
   - **release 名 = `evo-agent-desktop-vX.Y.Z`**（与 tag/版本严格一致）
   - tag 选择同名 tag（若该仓无此 tag，先从 evo-agent 仓同步过去）
   - 上传 `setup.exe` / `.sig` / `latest.json` 作为 assets
3. 发布后核对：release 页 assets 完整、版本号与 tag 一致。

## 5. 排障速查

- **`upload-pack: not our ref`**：workflow 第三方仓检出被钉死 SHA 后上游换基所致——第三方检出必须浮动 main（不写 `ref:`）。
- **freshness 门拒（`build freshness FAIL`）**：三仓（evo-agent / evorule-server / evorule-console-cloud）检出工作树不干净；2026-10-28 起诊断段会打印脏文件 `git diff --stat` 与 runner 行尾配置。`-SkipFreshness` 仅限本地调试。
- **解析错误/乱码**：仓内 PowerShell 脚本必须 UTF-8 with BOM——pwsh 对无 BOM 的非 ASCII 脚本按 ANSI 读取会撕裂字符串导致 ParserError。
- **lockfile 疑似脱节**：干净 worktree 跑 `cargo metadata --format-version 1 --locked --manifest-path <repo>\Cargo.toml`，秒级裁决；子仓（如 `plugins/ai-plugin`）有独立 lock 须各自探。
