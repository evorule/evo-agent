# evo-agent-desktop 发版手册（唯一操作正本）

> 本手册 = desktop 发版**唯一操作正本**（2026-11-07 归一：根目录 `RELEASE.md`
> 改为指向本文件的指针，消除双正本漂移）。发版纪律（2026-10-24 批审修订）：
> 构建+签名由 CI workflow（`desktop-release.yml`，触发 `evo-agent-desktop-v*` tag）
> 执行，**上传 Release 与发布确认保持人工执行**——人在环保留在上传环节。
> CI 不可用时按「本地构建路径」人工执行。改动须经批审。

## 版本与产物形态

- 产品版本唯一源：`desktop/src-tauri/tauri.conf.json` 的 `version`。发版前
  bump **五处版本面并保持一致**（四处受 CI 版本闸逐项检查 + Cargo.lock 由
  cargo 构建核对、版本闸亦查）：

| # | 文件 | 说明 | CI 版本闸 |
|---|---|---|---|
| 1 | `desktop/src-tauri/tauri.conf.json` | tag 比对基准 | ✓ |
| 2 | `desktop/src-tauri/Cargo.toml` | 壳 crate 版本 | ✓ |
| 3 | `desktop/Cargo.lock`（在 `desktop/`，不在 `src-tauri/`） | cargo 真实构建输入，漏 bump 时 CI 仍按旧版构建 | ✓ |
| 4 | `desktop/package.json` | desktop npm 包版本 | ✓ |
| 5 | `web/package.json` | 工作台浏览器形态版本回落源 | ✓ |

  主仓根 `Cargo.toml` 的版本是**独立版本面**（serve/API 版本演进），与桌面
  发版版本无关，勿混入本表。
- 发布仓：`github.com/evorule/evo-agent-desktop`（PUBLIC，release & 自更新通道）。
- 发布 tag 形态：`evo-agent-desktop-v<版本>`（每版一 tag 一 release，原子发布）。
  0.3.0 首发误用了滚动 `latest` tag——该 release 留作历史不重挂；新 release
  发布后 GitHub `releases/latest` 自动指向它，旧版壳的更新器端点随之翻页。
- 发布产物**四件**：`evo-agent-desktop_<ver>_x64-setup.exe` + `.sig` +
  `latest.json` + `evo-agent-desktop-<ver>-source.tar.gz`（AGPL §13 源码归档；
  许可证五件套已内嵌安装包 `resources\license\`）。均由 build-installer.ps1
  归集到 `desktop/target/release/bundle/updater/`。
- **内嵌组件溯源**（2026-11-07 起）：`resources\license\SOURCE.md` 的内嵌
  组件表记录 server/inspector/console 及 inspector 编译依赖检出（evorule、
  evorule-hash）的浮动 main 检出 HEAD——「这包装的是哪天的引擎」以它为准，
  §P 禁单换配套的核对锚点。

## 触发构建

- **标准路径**：打 tag `evo-agent-desktop-vX.Y.Z` 并推送到 github（单端；
  双端镜像时代已退役）。tag 触发版本闸：tag 名去前缀必须等于
  `tauri.conf.json` 的 `version`，且与上表其余版本面一致，否则构建失败。
- **工具路径**：`workflow_dispatch`（无输入）从 main HEAD 构建，**无版本闸**
  ——用于 CI 修复重跑/工具链验证；产物若直接用于发版，须在台账注明产出路径。
- 同 SHA 重推 tag 不会重新触发（ref 无变化即无事件）。tag 需改指新 SHA 时用
  `git tag -f` 后单端重推。

## 构建与发布步骤

### 缺省通道：CI 构建（desktop-release.yml）

1. 推送 tag（或 Actions 页 workflow_dispatch）。workflow 顺序：dist-gate →
   版本面核对 → 三方 release 构建（server/ai-plugin/wasm-host/inspector/
   evo-agent）→ web/console 构建 → 安装包构建（**freshness 门全量生效** →
   收集 → SSOT 校验 → 许可证五件套+SOURCE.md → tauri build → 更新三件套）→
   **装机冒烟验收** → artifact 上传（冒烟不绿不出包）。
2. 下载 run 的 artifact `evo-agent-desktop-release`（发布产物四件）。
3. 人工上传到 `github.com/evorule/evo-agent-desktop` 的 release
   `evo-agent-desktop-v<版本>`：上传四件（部分上传窗口=旧壳更新探针拿到新
   版本号却下不到安装包，三件套+tarball 必须同批）。
4. **滚动 `latest` 通道刷新（发版收口必做，2026-10-29 定型——「新 release
   已建」≠滚动通道到位，GH 端曾连漏两批）**：把 `latest` tag release 的附件
   刷成当前版——删旧 asset 三件（setup.exe/.sig/latest.json）→
   `gh release upload latest <三件> --clobber` → PATCH name/body 对齐本版。
5. **发布后验证**：Actions 手动触发 `desktop-release-probe` 工作流（自动
   校验 latest.json 结构/版本形态/资产可达/签名配对）；辅以人工两步——
   `GET releases/latest/download/latest.json` 应返回新版本号；老版本壳
   （>=0.3.0）托盘「检查更新」应发现新版本并完成升级重启。

### 本地构建路径（CI 不可用时的备选）

```powershell
# 1. 三方 release 构建（engine 仓 + ai-plugin + wasm-host + inspector + 本仓）
# 2. 前端构建
npm --prefix web run build

# 3. 安装包构建（freshness 门 → 收集内嵌资源 → SSOT 校验 → 许可证五件套 →
#    tauri build → 更新三件套 + 源码 tarball）
powershell -File desktop\build-installer.ps1 [-ReleaseNotes "..."]
#    本地源调试可加 -UpdateUrlBase http://127.0.0.1:18777
#    -SkipFreshness 仅限本地调试，发版禁用

# 4-5. 人工上传 + 滚动通道刷新 + 发布后验证：同缺省通道 3-5 步
```

## 前置条件

1. 签名私钥：`TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`
   env（minisign 私钥；位置见发布留痕，不入仓不落公开面）。CI 通道取自
   evo-agent 仓 Actions secrets（同名）；无 env 时 build 产出 setup.exe 但
   `.sig` 缺失，脚本报错终止——更新器签名是 fail-closed 红线，不允许无签名发版。
2. `latest.json` 的 notes：缺省由脚本注入一行版本标识（空 notes = 更新确认
   弹窗说明段空白）；自定义说明经 `-ReleaseNotes` 传入。
3. **freshness 门**：每笔预构建产物必须**晚于**其源码树的最后提交（先定稿
   提交、再 release 重建），evo-agent / evorule-server / evorule-console-cloud /
   evorule-context-inspector / evorule（内核，inspector 编译依赖检出）五仓
   不得有未提交的已跟踪改动。陈旧/脏树即构建红。历史注记：2026-10-28 CI 曾
   出现 diff --quiet 幻影判脏（evo-agent 册 o15 观察项），当时以 -SkipFreshness
   旁路补偿；行尾根因已随 workflow `autocrlf=false` 根治，**2026-11-07 旁路
   已回收、门在 CI 全量生效**——若幻影复现，构建红的 `[freshness-diag]`
   诊断段会打印 diff/stat 与行尾配置取证，凭证据裁定，勿再无证旁路。

## 排障速查

- **`upload-pack: not our ref`**：workflow 第三方仓检出被钉死 SHA 后上游换基
  所致——第三方代码检出必须浮动 main（不写 `ref:`）。唯一例外=dist-gate
  复用工作流钉 SHA（供应链惯例），其维护锚注释见 workflow 内该 job 上方。
- **freshness 门拒（`build freshness FAIL`）**：见前置条件 3；诊断段打印脏
  文件 `git diff --stat` 与 runner 行尾配置。
- **解析错误/乱码**：仓内 PowerShell 脚本必须 UTF-8 with BOM——pwsh 对无 BOM
  的非 ASCII 脚本按 ANSI 读取会撕裂字符串导致 ParserError；PS 5.1 读 BOM-less
  UTF-8 数据文件按 ANSI 误读，读外部 JSON 用 `[System.IO.File]::ReadAllText`。
- **lockfile 疑似脱节**：干净 worktree 跑
  `cargo metadata --format-version 1 --locked --manifest-path <repo>\Cargo.toml`，
  秒级裁决；子仓（如 `plugins/ai-plugin`）有独立 lock 须各自探。

## 红线

- latest.json 的 signature 字段由脚本从 `.sig` 原文注入并回读校验，禁止手抄。
- latest.json 内 url 钉本版 tag permalink（脚本缺省行为）——不要手工改回
  latest 动态路径，否则新版本发布后旧 latest.json 指向漂移。
- 安装包内不含任何凭据；LLM key 由用户首启向导录入（DPAPI 加密落本机）。
- 发版模板 `desktop/templates/evo-agent.toml` 不携带开发者本机路径（公开
  部署零耦合；动态值一律运行期注入）。
