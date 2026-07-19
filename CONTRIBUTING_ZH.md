<!--
  Copyright 2026 EvoRule Project

  This program is free software: you can redistribute it and/or modify
  it under the terms of the GNU Affero General Public License as published by
  the Free Software Foundation, either version 3 of the License, or
  (at your option) any later version.

  This program is distributed in the hope that it will be useful,
  but WITHOUT ANY WARRANTY; without even the implied warranty of
  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
  GNU Affero General Public License for more details.

  You should have received a copy of the GNU Affero General Public License
  along with this program.  If not, see <https://www.gnu.org/licenses/>.

  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# EvoRule 贡献指南（中文版）

**项目**: EvoRule — 反应式执行引擎
**版本**: v6.0
**最后更新**: 2026-07-19

---

## 🎯 核心原则

### 原则 1: TCB 极简,业务上浮

✅ **tier0-tcb 是 Kani 可验证的极简内核,只做加减与因果链**
❌ **不要把业务逻辑塞进 tier0-tcb**

**理由**:
- TCB 越大,越难形式化验证
- TCB 变更 = 宪法变更,需要重新过门禁
- 业务逻辑应作为 JSON 数据加载,通过 tier1/tier2 处理

### 原则 2: 机制与应用分离

✅ **evo-agent 是应用层,通过 HTTP API 与 evorule 通信**
❌ **不要在 evorule 内嵌 LLM / 业务规则 / 工作流**

**理由**:
- 机制层可独立验证(无业务污染)
- 应用层可独立演化(LLM 升级不需改 evorule)
- 契约通过 HTTP + JSON 公开,可跨语言调用

### 原则 3: JSON 是唯一表达

✅ **规则 / 状态 / 事件 / I/O 参数 / 审计账本 = 全部 JSON**
❌ **不要在系统内部引入非 JSON 数据结构(如 binary / protobuf / msgpack)**

**理由**:
- 透明性、可解释性、可审计性的源头是 JSON
- git diff = 审计,grep = 查询,JSONL = 时间机器
- 业务可读、可写、可版本控制

### 原则 4: 因果链贯通

✅ **每个 Fact 都有 cause 字段,因果链可追溯**
❌ **不要引入"无 cause 的内部状态变更"**

**理由**:
- rewind / replay / diff 的基础是因果链
- 审计、调试、争议解决全部依赖因果链

---

## 🐛 报告 Bug

使用 [Gitee Issues](https://gitee.com/evorulelab/evorule/issues)。

**报告模板**:
```markdown
**环境**:
- OS: [e.g. Windows 11 / Ubuntu 22.04]
- Rust: [e.g. 1.74]
- evorule 版本: [e.g. v6.0.0]

**复现步骤**:
1. ...
2. ...

**期望行为**:
...

**实际行为**:
...

**日志/截图**:
[粘贴 server 启动日志或 curl 输出]
```

---

## 💡 功能建议

也用 [Gitee Issues](https://gitee.com/evorulelab/evorule/issues),标签 `enhancement`。

**建议模板**:
```markdown
**问题**: 现有方案的不足
**建议方案**: 简要描述
**替代方案**: 你考虑过的其他选项
**影响范围**: 涉及哪些 tier / 模块
```

---

## 🔧 提交 PR

### 流程

1. **Fork 仓库** → 在你的 Gitee 账号下创建 fork
2. **创建分支**:`git checkout -b feature/your-feature-name`
3. **写代码 + 写测试** — 测试覆盖率不掉
4. **本地验证**:
   ```bash
   cargo check --workspace
   cargo test --workspace
   cargo clippy --workspace -- -D warnings   # 0 warnings 才能合
   ```
5. **推送**:`git push origin feature/your-feature-name`
6. **创建 PR** 到 Gitee,填写 PR 模板
7. **CLA 签署**(见下)
8. **等 review** — 维护者会在 7 天内回复

### 提交消息规范

使用 [Conventional Commits](https://www.conventionalcommits.org/):

```
feat(tier0): 新增 Kani proof for set_integer_safety
fix(db): 修复 sqlx 0.8 API 适配
docs(readme): 区分宪法 vs 业务规则
chore(deps): 升级 tokio 到 1.40
refactor(reactor): 拆分 stable_detector 模块
test(e2e): 添加宪法核心冒烟测试
```

### 分支命名

- `feature/<name>` — 新功能
- `fix/<name>` — Bug 修复
- `docs/<name>` — 仅文档
- `chore/<name>` — 杂项
- `refactor/<name>` — 重构

---

## 📜 CLA(贡献者协议)

**所有贡献必须签署 CLA**。提交 PR 时,机器人会自动检查。

- 个人贡献者:[CLA-individual.md](CLA-individual.md)(待发布)
- 企业贡献者:请联系 evorulelab@gmail.com

**为什么需要 CLA?**
- 保护项目可商业化(参考 [DUAL_LICENSE.md](DUAL_LICENSE.md))
- 避免贡献者版权纠纷
- AGPL-3.0 之外的商业许可需要 CLA 配合

---

## 🧪 测试要求

### 单元 + 集成测试

- 新功能必须有对应单元测试
- 集成测试放在 `tests/` 目录
- 覆盖率不下降(当前 ~95%)

### 端到端测试

启动 evorule-server,跑通 5 个核心场景:
1. 健康检查
2. 会话生命周期
3. set + increment + state(宪法核心)
4. 时间机器(replay / rewind)
5. 审计链

参考:`tests/e2e_smoke.py`

### Kani 形式化验证(仅 tier0-tcb)

新增 tier0 元指令 / 域类型时,必须配 Kani proof:

```bash
cargo kani -p tier0-tcb --features kani
```

---

## 🛠 代码规范

### 风格

- `cargo fmt` 必须过
- `cargo clippy -- -D warnings` 必须过
- 函数必须有 doc comment(`deny(missing_docs)`)
- 公开 API 例子必须跑通

### 文件头

所有 `.rs` 文件必须有 SPDX header:

```rust
// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
```

### 模块化

- `tier0-tcb`:**只**包含纯计算(`no_std` 兼容)
- `tier1-reactor`:事件循环 + FactsLog + 时间机器
- `tier2-governance`:I/O + HTTP API + 审计
- `evo-agent`(独立仓库):LLM 编排

### 不可变默认

- 函数默认 `fn` 而非 `fn mut`,避免隐式状态
- 数据结构用 `BTreeMap`(确定性迭代)而非 `HashMap`
- 所有公开 API 优先接收 `&T` 而非 `T`

---

## 🚫 不要做的事

- ❌ **不要在 tier0-tcb 加 I/O**(破坏 no_std)
- ❌ **不要在 evorule 内嵌 LLM**(机制层与 LLM 分离)
- ❌ **不要引入非 JSON 数据格式**(破坏透明性)
- ❌ **不要用 `unsafe` 在非 FFI 代码中**(违反 `#![forbid(unsafe_code)]`)
- ❌ **不要用 `unwrap` / `expect` / `panic` 在 tier0-tcb**(破坏"永不 panic"约束)
- ❌ **不要直接修改 `core_eval.json` 之外的"宪法"**(宪法稳定是 EvoRule 的核心)
- ❌ **不要 commit secrets / API key / 真名 / 内网地址**(公开仓库)

---

## 📞 联系方式

- **Gitee**: https://gitee.com/evorulelab/evorule/issues
- **邮箱**: evorulelab@gmail.com
- **组织**: [EvoRule Lab](https://gitee.com/evorulelab)

---

## 🙏 致谢

感谢所有贡献者!你的名字会出现在 [AUTHORS.md](AUTHORS.md) 中。

---

**遵循 evorule-core-backup 风格的贡献指南。**
**参考了 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/)、[Conventional Commits](https://www.conventionalcommits.org/)、[Contributor Covenant](https://www.contributor-covenant.org/) 等社区最佳实践。**
