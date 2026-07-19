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

# Contributing to EvoRule

**Project**: EvoRule — Reactive Execution Engine
**Version**: v6.0
**Last updated**: 2026-07-19

> 🇨🇳 **中文版贡献指南见 [CONTRIBUTING_ZH.md](CONTRIBUTING_ZH.md)。**
> For international contributors, the English version is the authoritative reference.

---

## 🎯 Core Principles

### Principle 1: TCB minimal, business on top

✅ **`tier0-tcb` is a Kani-verifiable minimal kernel — only addition, subtraction, and causal chains**
❌ **Do NOT push business logic into `tier0-tcb`**

**Why**:
- The larger the TCB, the harder it is to formally verify
- TCB changes = constitution changes — must pass build.rs gates + Kani
- Business logic should be JSON data loaded at runtime

### Principle 2: Mechanism vs. application separation

✅ **`evo-agent` is the application layer; it talks to `evorule` via HTTP API**
❌ **Do NOT embed LLM / business rules / workflows inside `evorule`**

**Why**:
- Mechanism layer is independently verifiable
- Application layer can evolve independently (LLM upgrade doesn't touch evorule)
- The HTTP + JSON contract is public, cross-language

### Principle 3: JSON is the only expression

✅ **Rules / state / events / I/O params / audit ledger = all JSON**
❌ **Do NOT introduce non-JSON data formats (binary, protobuf, msgpack) in the system**

**Why**:
- Transparency, explainability, and auditability all stem from JSON
- `git diff` = audit, `grep` = query, JSONL = time machine
- Business can be read, written, and version-controlled

### Principle 4: Causal chain integrity

✅ **Every Fact has a `cause` field; the chain is fully traceable**
❌ **Do NOT introduce "internal state changes without a cause"**

**Why**:
- `rewind` / `replay` / `diff` are built on the causal chain
- Auditing, debugging, and dispute resolution all depend on it

---

## 🐛 Reporting Bugs

Use [Gitee Issues](https://gitee.com/evorulelab/evorule/issues) (preferred) or
GitHub Issues for international contributors.

**Report template**:
```markdown
**Environment**:
- OS: [e.g. Windows 11 / Ubuntu 22.04]
- Rust: [e.g. 1.74]
- evorule version: [e.g. v6.0.0]

**Steps to reproduce**:
1. ...
2. ...

**Expected behavior**:
...

**Actual behavior**:
...

**Logs / screenshots**:
[Paste server startup log or curl output]
```

---

## 💡 Feature Requests

Also use Issues with the `enhancement` label.

**Template**:
```markdown
**Problem**: What's wrong with the current approach?
**Proposed solution**: Brief description
**Alternatives considered**: Other options you evaluated
**Impact scope**: Which tier / module is affected
```

---

## 🔧 Submitting a PR

### Workflow

1. **Fork the repo** → create a fork under your Gitee/GitHub account
2. **Create a branch**: `git checkout -b feature/your-feature-name`
3. **Write code + write tests** — coverage must not drop
4. **Local validation**:
   ```bash
   cargo check --workspace
   cargo test --workspace
   cargo clippy --workspace -- -D warnings   # 0 warnings required
   ```
5. **Push**: `git push origin feature/your-feature-name`
6. **Open PR** on Gitee (or GitHub), fill in the PR template
7. **Sign CLA** (see below)
8. **Wait for review** — maintainers reply within 7 days

### Commit message convention

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
feat(tier0): add Kani proof for set_integer_safety
fix(db): adapt to sqlx 0.8 API
docs(readme): distinguish constitution from business rules
chore(deps): upgrade tokio to 1.40
refactor(reactor): split stable_detector module
test(e2e): add core constitution smoke test
```

### Branch naming

- `feature/<name>` — new feature
- `fix/<name>` — bug fix
- `docs/<name>` — docs only
- `chore/<name>` — misc
- `refactor/<name>` — refactor

---

## 📜 CLA (Contributor License Agreement)

**All contributions must include a CLA**. The bot will check automatically on PR.

- Individual contributors: [CLA-individual.md](CLA-individual.md) (to be published)
- Corporate contributors: contact evorulelab@gmail.com

**Why CLA?**
- Enable commercial licensing (see [DUAL_LICENSE.md](DUAL_LICENSE.md))
- Avoid contributor copyright disputes
- AGPL-3.0 alone is not enough for commercial dual-licensing

---

## 🧪 Testing Requirements

### Unit + integration tests

- New features must have corresponding unit tests
- Integration tests go in `tests/`
- Coverage must not drop (current ~95%)

### End-to-end tests

Start `evorule-server` and verify 5 core scenarios:
1. Health check
2. Session lifecycle
3. `set` + `increment` + `state` (constitution core)
4. Time machine (`replay` / `rewind`)
5. Audit chain

See: `tests/e2e_smoke.py`

### Kani formal verification (tier0-tcb only)

When adding new tier0 meta-instructions or domain types, you must add a Kani proof:

```bash
cargo kani -p tier0-tcb --features kani
```

---

## 🛠 Coding Standards

### Style

- `cargo fmt` must pass
- `cargo clippy -- -D warnings` must pass
- Functions must have doc comments (`deny(missing_docs)`)
- Public API examples must be runnable

### File header

All `.rs` files must include the SPDX header:

```rust
// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
```

### Module layering

- `tier0-tcb`: **only** pure computation (`no_std` compatible)
- `tier1-reactor`: event loop + FactsLog + time machine
- `tier2-governance`: I/O + HTTP API + audit
- `evo-agent` (separate repo): LLM orchestration

### Immutability by default

- Prefer `fn` over `fn mut`
- Use `BTreeMap` (deterministic iteration) over `HashMap`
- Public APIs should accept `&T` over `T`

---

## 🚫 What NOT to do

- ❌ **Do NOT add I/O to `tier0-tcb`** (breaks `no_std`)
- ❌ **Do NOT embed LLM inside `evorule`** (mechanism layer must stay LLM-free)
- ❌ **Do NOT introduce non-JSON data formats** (breaks transparency)
- ❌ **Do NOT use `unsafe` outside FFI code** (violates `#![forbid(unsafe_code)]`)
- ❌ **Do NOT use `unwrap` / `expect` / `panic` in `tier0-tcb`** (breaks "never panic" invariant)
- ❌ **Do NOT modify the "constitution" outside `core_eval.json`** (constitution stability is EvoRule's core)
- ❌ **Do NOT commit secrets / API keys / personal info / internal addresses** (it's a public repo)

---

## 📞 Contact

- **Gitee**: https://gitee.com/evorulelab/evorule/issues
- **Email**: evorulelab@gmail.com
- **Org**: [EvoRule Lab](https://gitee.com/evorulelab)

---

## 🙏 Acknowledgments

Thanks to all contributors! Your name will appear in [AUTHORS.md](AUTHORS.md).

---

**Style follows `evorule-core-backup` + community best practices
([Keep a Changelog](https://keepachangelog.com/),
[Conventional Commits](https://www.conventionalcommits.org/),
[Contributor Covenant](https://www.contributor-covenant.org/)).**
