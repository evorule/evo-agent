# Desktop 发版手册

**发版唯一操作正本 = [`desktop/RELEASE.md`](desktop/RELEASE.md)**（2026-11-07
归一：本文件原为独立操作手册，与 desktop/ 版双正本漂移，现改为指针）。

速览：

- 发版链 = `desktop-release.yml`（tag `evo-agent-desktop-v*` / workflow_dispatch）
  → dist-gate → 版本面核对 → 三方构建 → freshness 门 → 装机冒烟 → artifact；
  人工上传 Release + 滚动 `latest` 通道刷新 + 发布后探针（`desktop-release-probe`）。
- 版本面五处（四处受 CI 版本闸检查 + desktop/Cargo.lock）、产物四件、
  前置条件、排障速查、红线——全部见 [`desktop/RELEASE.md`](desktop/RELEASE.md)。
- 发版链作业法沉淀（滚动通道史/签名接线/编码铁律）：
  knowledge 侧 `evorule-desktop-release` skill。
