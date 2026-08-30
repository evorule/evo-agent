<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# Evo-Agent 发布流程

> **文档性质**：evo-agent 仓的发布操作手册。
> **适用范围**：evo-agent 仓（AI Agent 编排层，单一 Rust crate）。
> **各仓独立发布原则**：与生态各仓一致——只管好本仓真实情况，不追求生态版本同步 bump。

## 发布形态（如实声明）

- **当前发布形态 = git tag**（`v{MAJOR}.{MINOR}.{PATCH}`，如 `v0.1.0`）。
- **crates.io 发布暂不可行**：本仓以 path 依赖引用 `../evorule/evorule-tcb`、
  `../evorule/evorule-reactor`（见 README"源码布局契约"）。待主仓核心库发布
  crates.io 后切换为版本依赖，届时补 crates.io 发布章节。
- 下游使用方式：git 依赖 / 并排检出源码构建 / 预编译产物。

## 0. 前置条件

发布执行人需具备：

- Gitee 源仓库的 push 权限
- Rust 工具链（1.74+）
- 与主仓并排检出的源码布局（见 README）

## 1. 发布前就绪检查

### 1.1 一键验证

```powershell
pwsh verify.ps1
```

覆盖 4 项：`cargo build` → `cargo test`（全量）→ 防泄漏扫描（密钥模式）→
源码布局断言（path 依赖目标存在且可解析）。全部通过（exit 0）才可继续。

### 1.2 手工确认

- [ ] `CHANGELOG.md` 版本章节完整、填入实际发布日期、无遗留 `[Unreleased]`
      （发布时转为版本段或清空）
- [ ] `NOTICE.md` / `README.md` 与当前版本一致
- [ ] 公共契约面（lib.rs crate doc 所列）无未声明的 breaking 变更；
      0.x 阶段 breaking 必须升 minor

## 2. 创建 Git Tag

```bash
git status   # 必须无未提交变更
git tag -a v0.1.0 -m "Evo-Agent v0.1.0"
git push origin main --tags
```

> 版本纪律：0.x 阶段任何 breaking 变更升 minor（0.1.0 → 0.2.0）；
> 首个 1.0 之前不承诺 API 稳定（README 如实声明）。

## 3. 发布后验证

```bash
git ls-remote --tags origin v0.1.0
```

- [ ] tag 在远端存在
- [ ] 干净环境按 README 布局契约并排检出后 `cargo build && cargo test` 通过

## 附录：紧急回滚

```bash
git tag -d v0.1.0
git push origin :refs/tags/v0.1.0
# 修复后以新版本号重新发布
```

> 撤回 tag 是最后手段，仅当源码本身有严重缺陷时使用。
