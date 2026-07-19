<!--
  EvoRule / Evo-Agent Pull Request Template
  对应 VERSION_STRATEGY.md 的人类审查项
  机械可验的项已交给 CI,本模板只覆盖人审项
-->

## 关联

- [ ] 关联 Issue: #
- [ ] 关联 PR: #

## 版本变更

- [ ] **MAJOR** 升级 (含破坏性变更)
- [ ] **MINOR** 升级 (新功能,无破坏)
- [ ] **PATCH** 升级 (仅 bug 修复)
- [ ] **预发布** (alpha / beta / rc)

**目标版本**:`v` &nbsp;&nbsp;&nbsp;&nbsp; **对应策略章节**:VERSION_STRATEGY §

---

## 🤖 自动化检查(由 CI 完成)

> 以下项 CI 自动跑,作者**不需要**勾选,留给 reviewer 看绿勾

- [ ] `validate-version.ps1` 通过(§2.1 / §3.2)
- [ ] `validate-changelog.ps1` 通过(§4.5)
- [ ] `validate-license.ps1` 通过(AGPL + SPDX + CC0)
- [ ] `validate-cargolock.ps1` 通过(§8)
- [ ] `validate-release.ps1` 通过(§10.1)
- [ ] `cargo check --workspace` 通过
- [ ] `cargo test --workspace` 通过
- [ ] `cargo clippy --workspace -- -D warnings` 通过

---

## 👤 人类审查清单(作者必填,Reviewer 必看)

### 通用项(所有 PR)

- [ ] **MINOR 升级是一次明显的功能里程碑**(§4.2,非"加了一行")
- [ ] **API 稳定性**:公开 API 没有"偷偷改"
- [ ] **CHANGELOG 章节完整**,无 [Unreleased] 残留
- [ ] **破坏性变更已标 `⚠️ BREAKING CHANGES`**(§6.2)
- [ ] **弃用 API 走完流程**:`#[deprecated(since=...)]` + 至少 1 个 MINOR 共存(§5.2)
- [ ] **Cargo.lock 策略一致**:binary commit, lib 不 commit(§8)
- [ ] **git tag 带 `v` 前缀**(§10.1)
- [ ] **Conventional Commits 格式**:`feat:` / `fix:` / `chore:` / `feat!:` / `BREAKING CHANGE:`

### SDK 变更(任一 SDK 改动时勾)

- [ ] TypeScript SDK 版本号已同步(§7.1)
- [ ] Python SDK 版本号已同步
- [ ] SDK CHANGELOG 已更新
- [ ] SDK 公共 API 文档已更新

### 升 1.0 时(§4.4)—— 任意 1 项不满足 = 不发 1.0

- [ ] LLM handler 真实(非 stub)
- [ ] tool handler 真实(非 stub)
- [ ] `cargo build` 0 warnings(含 missing_docs / clippy)
- [ ] E2E 测试通过(真实 LLM + 真实 server + Agent run)
- [ ] Kani 形式化验证(tier0 不变式 ≥ 5 个 proof)
- [ ] TECHNICAL_MANUAL / USER_GUIDE / API_REFERENCE 三件齐
- [ ] PERFORMANCE_BENCHMARK.md
- [ ] SECURITY_AUDIT.md + THREAT_MODEL.md
- [ ] 1 个 reference 实现跑通(`examples/reactive_researcher`)
- [ ] 1 名独立 reviewer(reviewer 不等于 author)已 review 本 PR

### 升 1.0 之后触发第三方审计时(§4.5)

- [ ] 满足 6 个触发条件之一(已写在 PR description)
- [ ] 选定审计供应商(Cure53 / Trail of Bits / NCC / 国内)
- [ ] 报告摘要准备放 `docs/security/`

---

## ✅ 测试

- [ ] 单元测试覆盖新代码(目标:行覆盖 ≥ 80%)
- [ ] 集成测试(如改 API 必填)
- [ ] E2E 测试(改 LLM/Server/Agent 路径必填)
- [ ] 性能影响(改热路径必填,无明显退化)

## 📚 文档

- [ ] README 更新(新增 / 弃用 API)
- [ ] examples 更新
- [ ] CHANGELOG 写完
- [ ] 协议影响(改 LICENSE 字段 / SPDX header 必填)

## 🔒 安全

- [ ] 没有引入新的 unsafe(`#![forbid(unsafe_code)]` 守住)
- [ ] 没有引入新的外部依赖(除非 PR description 写明理由)
- [ ] 没有 hardcode 密钥 / API key
- [ ] 没有引入新 I/O 路径(文件 / 网络 / 子进程)

---

**Reviewer**:@evorulelab/maintainers

**说明**:
- 普通 bug 修复:勾"通用项"中相关行 + "测试"行
- 版本号变更:**所有**"通用项"必填
- 升 1.0:额外勾"升 1.0 时"全部
- 触发第三方审计:额外勾"升 1.0 之后"全部
