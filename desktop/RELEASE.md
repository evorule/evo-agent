# evo-agent-desktop 发版手册（人工流程）

> 推送人工化纪律（统帅令）同样适用于发版：构建、签名、上传全链人工执行，
> 不设自动 release workflow。本手册=发版唯一操作正本，改动须经批审。

## 版本与产物形态

- 产品版本唯一源：`desktop/src-tauri/tauri.conf.json` 的 `version`，与
  `desktop/src-tauri/Cargo.toml`、主仓 `Cargo.toml`、`web/package.json` 四处
  保持一致（发版前 bump 同一版本号，0.4.0 起统一）。
- 发布仓：`github.com/evorule/evo-agent-desktop`（PUBLIC，release & 自更新通道）。
- 发布 tag 形态：`evo-agent-desktop-v<版本>`（每版一 tag 一 release，原子发布；
  版本史可溯、无 clobber 竞态）。0.3.0 首发误用了滚动 `latest` tag——该 release
  留作历史不重挂；新 release 发布后 GitHub `releases/latest` 自动指向它，旧版
  壳的更新器端点随之翻页到新 latest.json，无需迁移动作。release 标题=`evo-agent <版本>`。
- 产物三件套：`evo-agent-desktop_<ver>_x64-setup.exe` + `.sig` + `latest.json`
  （由 build-installer.ps1 归集到 `desktop/target/release/bundle/updater/`）。

## 前置条件

1. 签名私钥：`TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`
   env（minisign 私钥；位置见发布留痕，不入仓不落公开面）。无 env 时
   build 产出 setup.exe 但 `.sig` 缺失，脚本报错终止——更新器签名是
   fail-closed 红线，不允许无签名发版。
2. 引擎与执行体 release 构建在位：
   - `<server-repo>\target\release\evorule-server.exe`（engine 仓，工作目录同级）
   - `<server-repo>\plugins\ai-plugin\target\release\evorule-ai-plugin.exe`
   - 本仓 `target\release\evo-agent.exe`（`cargo build --release`）
3. 前端产物在位：`npm --prefix web run build`（cargo build 不含前端构建——
   安装包内的 web/dist 以收集时刻磁盘实况为准，忘跑=装旧前端）。
4. **新鲜度与干净度由 build-installer 自动校验**（`scripts/check-build-freshness.ps1`）：
   每笔预构建产物必须**晚于**其源码树的最后提交（发行纪律=先定稿提交、再
   release 重建），相关仓不得有未提交的已跟踪改动。陈旧/脏树即构建红——
   这道门把「推了代码忘重建=安装包静默装旧 bits」从事后教训变成构建期拦截。

## 构建与发布步骤

```powershell
# 1. 三方 release 构建（engine 仓 + ai-plugin + 本仓）
# 2. 前端构建
npm --prefix web run build

# 3. 安装包构建（收集内嵌资源 → SSOT 校验 → tauri build → 更新三件套）
powershell -File desktop\build-installer.ps1 [-ReleaseNotes "..."]
#    本地源调试可加 -UpdateUrlBase http://127.0.0.1:18777

# 4. 人工上传（gh 或网页）：新建 evorule/evo-agent-desktop 的 release
#    （tag=evo-agent-desktop-v<版本>，与 build-installer 生成的 latest.json
#    url permalink 同形）；三件套必须同批上传（部分上传窗口=旧壳更新探针
#    拿到新版本号却下不到安装包）。
gh release create "evo-agent-desktop-v<版本>" desktop\target\release\bundle\updater\* `
  --title "evo-agent <版本>" --notes "<ReleaseNotes>"

# 5. 发布后验证
#    - 探针：GET releases/latest/download/latest.json 应返回新版本号
#    - 老版本壳（>=0.3.0）托盘「检查更新」应发现新版本并完成升级重启
```

## 红线

- latest.json 的 signature 字段由脚本从 `.sig` 原文注入并回读校验，禁止手抄。
- latest.json 内 url 钉本版 tag permalink（脚本缺省行为）——不要手工改回
  latest 动态路径，否则新版本发布后旧 latest.json 指向漂移。
- 安装包内不含任何凭据；LLM key 由用户首启向导录入（DPAPI 加密落本机）。
