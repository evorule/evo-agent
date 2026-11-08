# THIRD-PARTY-NOTICES — EvoRule 生态第三方组件使用声明

> **本文件是 EvoRule 生态 13 个在役仓库的第三方组件使用声明（使用声明文件）。**
> 适用范围、义务边界、归档位置与复现方法见下文。
> 首次生成：2026-10-24 · 本版：2026-10-25（Wave 2 分发合规收口）· 审计基线：13 在役仓 · 依赖记录 3404 条

---

## 〇、一句话结论

**依赖许可证面无禁止级风险（GPL-2.0-only / SSPL / BUSL / 非商用限制 = 0 项）；分发义务已于 2026-10-25（Wave 2）收口——有分发链的仓全部随附五件套（LICENSE + NOTICE + THIRD-PARTY-NOTICES + LICENSES/ + SOURCE.md），缺件发布由 dist-gate 阻断。**

| 维度 | 结论 |
|---|---|
| 禁止级许可证（copyleft 不兼容 / 源码可见 / 非商用） | **0 项** |
| 需登记的宽松许可（不在原白名单） | 7 类（MIT-0 / CDLA-Permissive-2.0 / BSL-1.0 / BlueOak-1.0.0 / NCSA / OFL-1.1 / BSD-1-Clause）— **已于 2026-10-24 全部登记入 `deny.toml`**，复验 0 红项 |
| 需特别标注的 copyleft 依赖 | 1 类（MPL-2.0，dompurify，可选 Apache-2.0 分支） |
| 已发布 crate 缺许可证正文 | **0 / 9**（evorule-discipline、evorule-eval 已于 Wave 2 补齐） |
| 发行分发包缺 LICENSE/NOTICE/声明 | **0**（Wave 2 后有分发链的仓全部五件套随附 + dist-gate 阻断，详见第五节） |
| 依赖许可 CI 门禁覆盖 | **全生态**：license-gate（防漂移比对 + cargo-deny 四闸）+ dist-gate（发布阻断）自根仓 reusable workflow 钉 SHA 复用，10 仓 `deny.toml` 统一 |
| 自有许可七件套一致性 | **绿**（Wave 2 重写 17 件漂移件；核验覆盖面扩容至 16 仓进行中，详见第二节） |

---

## 一、适用范围：13 个在役仓

| # | 仓名 | 角色 | 依赖生态 | 依赖记录数 | 分发形态 |
|---|---|---|---|---|---|
| 1 | `evorule` | 核心引擎 / TCB 唯一版本源（8 crate） | cargo | 195 | crates.io ×6 + CLI 双平台二进制 |
| 2 | `evorule-server` | 官方 HTTP 服务入口（:18080） | cargo + npm | 513 | 零依赖分发包 win64.zip / linux64.tar.gz |
| 3 | `evorule-rule` | 规则资产治理层 | cargo | 335 | crates.io + 双平台双 bin |
| 4 | `evorule-system-rules` | 生态宪法 / schema 版本权威 | 无编译依赖 | 0 | 仓库内资产（schemas/migrations/signatures） |
| 5 | `evorule-bundle` | 治理↔执行唯一传递契约 | cargo | 147 | crates.io |
| 6 | `evorule-hash` | 审计链规范哈希 SSOT | cargo | 20 | crates.io |
| 7 | `evorule-console-cloud` | 云控制台 / 合规审计层 | npm | 213 | 静态站点 zip + GitHub Pages |
| 8 | `evorule-integration-tests` | 跨仓一致性门禁 | cargo | 252 | 不分发 |
| 9 | `evorule-context-inspector` | 上下文检查器 | cargo | 212 | crates.io |
| 10 | `evorule-wasm-demo` | WASM 演示 | cargo + npm | 74 | npm 包 + wasm 产物 |
| 11 | `evo-agent` | 应用层编排（可信 AI 工作站） | cargo + npm + pypi | 706 | npm 包 + 桌面端 |
| 12 | `evo-agent-desktop` | 桌面壳发行 / 自动更新通道 | pypi / 脚本 | 549 | 桌面安装包 |
| 13 | `sync-sentinel` | 双端一致性哨兵 | shell | 0 | 不分发（运维脚本） |
| | **合计** | | | **3404** | |

---

## 二、本生态自身的许可与双许可路径

| 资产 | 许可 | 义务要点 |
|---|---|---|
| 代码（全部 13 仓） | AGPL-3.0-or-later | 分发须附许可证副本与修改说明（§4/§5(d)）；以网络提供服务须提供对应源码（§13） |
| `core_eval.json`（宪法/解释器规范） | CC0-1.0 公共领域 | 无义务，鼓励第三方实现兼容引擎 |
| 贡献物通道 D | 局部 Apache-2.0 再授权 | 见 `CONTRIBUTION_GRANT.md` |

**商业许可路径的差异**：持有商业许可（`COMMERCIAL_LICENSE.md`）的用户，其使用与分发不再受 AGPL 源码提供与 copyleft 传染约束；第三方组件的归属义务（MIT/BSD/Apache 的版权声明与正文）**不因商业许可而免除**——本文件所列第三方义务对两条路径同样成立。

### 与既有「许可文件七件套」的关系

生态已有一套自有许可治理机制：根仓 `scripts/sync-licenses.ps1` 以主仓为权威源，把 **CLA-corporate / CLA-individual / COMMERCIAL_LICENSE / DUAL_LICENSE / FREE_COMMERCIAL_LICENSE / LICENSE / NOTICE.md** 七件套同步到各副本仓并做一致性核验；`scripts/validate-license.ps1` 校验 LICENSE 存在性、AGPL/CC0 标识、`.rs` SPDX 头与 SDK 许可字段。

两者是**不同层面，互不替代**：

| 层面 | 管什么 | 载体 |
|---|---|---|
| 自有许可层（七件套） | 本生态代码的授权路径、CLA、双许可文本在各仓的一致性 | 各仓根 7 个文件 |
| 第三方归属层（本文件） | 依赖的第三方组件的许可、归属义务与正文 | 根仓本文件 + `LICENSES/third-party/` |

本文件**不纳入七件套**：第三方依赖逐仓不同，七件套的「各仓内容一致」语义不适用于此；它是生态级（跨 13 仓）的汇总声明。若需单仓声明，可从 `LICENSES/third-party/manifest.json` 的 `dependencies[].repo` 字段按仓筛选取子集生成。

**七件套状态**（2026-10-25 更新）：

1. **内容一致性：已收口**——2026-10-24 实测的 17 项不一致（`NOTICE.md` × 10 仓、`DUAL_LICENSE.md` × 7 仓）已于 Wave 2 全部重写对齐；`sync-licenses.ps1 -Check` 复验通过。
2. **覆盖面：扩容中**——原核验 11 仓含 4 个已退役私有仓；扩容批次将核验面调整为 13 在役仓 + 3 退役私有仓 = 16 仓（`sync-licenses.ps1` 仓名/目录映射同步重构）。

---

## 三、第三方许可总览

下表按许可证标识聚合。复合表达式（如 `MIT OR Apache-2.0`）按其可选分支拆分统计，同一依赖可能计入多行。

| 许可证 | 依赖记录数 | 涉及组件（示例） | 义务 | 正文归档 |
|---|---|---|---|---|
| `MIT` | 3029 | `@asamuzakjp/css-color`, `@asamuzakjp/dom-selector`, `@babel/code-frame`, `@babel/helper-validator-identifier` 等 | 保留版权声明与许可证全文，随二进制分发。 | ✅ |
| `Apache-2.0` | 2197 | `@playwright/test`, `@tauri-apps/cli`, `@tauri-apps/cli-darwin-arm64`, `@tauri-apps/cli-darwin-x64` 等 | 保留版权声明、许可证全文与变更说明；上游若有 NOTICE 须一并附带（§4(d)）。 | ✅ |
| `Unicode-3.0` | 137 | `icu_collections`, `icu_locale_core`, `icu_normalizer`, `icu_normalizer_data` 等 | 保留版权声明与许可文本。 | ✅ |
| `Apache-2.0 WITH LLVM-exception` | 85 | `blake3`, `cranelift-assembler-x64`, `cranelift-assembler-x64-meta`, `cranelift-bforest` 等 | 保留版权声明与许可正文。 | ✅ |
| `Zlib` | 64 | `bytemuck`, `bytemuck_derive`, `dispatch2`, `foldhash` 等 | 保留版权声明与许可文本。 | ✅ |
| `Unlicense` | 51 | `aho-corasick`, `byteorder`, `byteorder-lite`, `globset` 等 | 无义务。 | ✅ |
| `BSD-3-Clause` | 51 | `alloc-no-stdlib`, `alloc-stdlib`, `avif-serialize`, `brotli` 等 | 保留版权声明、条款与免责声明。 | ✅ |
| `ISC` | 46 | `forwarded-header-value`, `hmac-sha256`, `hyper-rustls`, `inotify` 等 | 保留版权声明与许可文本。 | ✅ |
| `CC0-1.0` | 25 | `blake3`, `constant_time_eq`, `dunce`, `imgref` 等 | 公共领域贡献，无义务。 | ✅ |
| `BSD-2-Clause` | 24 | `arrayref`, `av1-grain`, `entities`, `mach2` 等 | 保留版权声明、条款与免责声明。 | ✅ |
| `MIT-0` | 19 | `@csstools/color-helpers`, `@csstools/css-syntax-patches-for-csstree`, `borrow-or-share`, `constant_time_eq` 等 | 保留版权声明与许可证全文，随二进制分发。 | ✅ |
| `LGPL-2.1-or-later` | 14 | `r-efi` | 弱 copyleft：本生态统一选择 MIT/Apache 分支，不触发 LGPL 条款。 | ✅ |
| `AGPL-3.0-or-later` | 13 | `@evorule/console-cloud`, `@evorule/evo-agent-workbench`, `evorule-audit-chain`, `evorule-bundle` 等 | 网络 copyleft：以网络提供服务时须向使用者提供对应源码（§13）。 | ✅ |
| `BSL-1.0` | 13 | `clipboard-win`, `error-code`, `ryu`, `wasite` 等 | 宽松许可：保留版权声明与正文。 | ✅ |
| `MPL-2.0` | 10 | `colored`, `cssparser`, `cssparser-macros`, `dompurify` 等 | 文件级 copyleft：被修改的 MPL 文件须以 MPL 公开；分发时保留声明与正文。 | ✅ |
| `0BSD` | 8 | `adler2` | 无附加义务。 | ✅ |
| `CDLA-Permissive-2.0` | 8 | `webpki-root-certs`, `webpki-roots` | 宽松许可：保留版权声明与正文。 | ✅ |
| `BSD-1-Clause` | 6 | `fiat-crypto` | 保留版权声明与许可正文。 | ✅ |
| `NCSA` | 1 | `libfuzzer-sys` | 宽松许可：保留版权声明与正文。 | ✅ |
| `BlueOak-1.0.0` | 1 | `lru-cache` | 宽松但非 OSI 认证：保留版权声明与正文。 | ✅ |

机器可读全量清单见 [`LICENSES/third-party/manifest.json`](LICENSES/third-party/manifest.json)（3404 条依赖记录，含每条的许可标识与来源）。

---

## 四、需特别标注的组件

### `MPL-2.0` — dompurify（evo-agent 前端）

MPL-2.0 为文件级 copyleft。dompurify 的表达式为 `MPL-2.0 OR Apache-2.0`，可选择 Apache-2.0 分支规避 copyleft；但一旦修改其源码并分发该修改文件，须以 MPL-2.0 公开该文件。当前为原样依赖，义务为零。

### `BlueOak-1.0.0` — lru-cache（evo-agent）

BlueOak-1.0.0 是宽松许可但**未经 OSI 认证**。法律上可用，但若下游客户要求「全部依赖为 OSI 认证许可」，此项会成为审计问询点。已列入 deny.toml 白名单并附正文。

### `CDLA-Permissive-2.0` — webpki-roots / webpki-root-certs（evorule-server、evo-agent）

根证书库采用 CDLA-Permissive-2.0。宽松许可，仅要求保留声明。注意：根证书内容本身由各 CA 分发，其内容的独立授权不由本仓库承担。

### `NCSA` — libfuzzer-sys（evo-agent，仅 dev 依赖）

表达式为 `(MIT OR Apache-2.0) AND NCSA`——NCSA 部分是 **AND 强制项**，不可通过 OR 选择规避，须随附 NCSA 正文。

### `BSL-1.0` — clipboard-win / error-code（evo-agent）

Boost Software License 1.0，宽松，保留声明即可。

### `MIT-0` — borrow-or-share（evorule-server）、@csstools/*（evo-agent、console-cloud）

MIT-0 是 MIT 的零归属变体，义务低于 MIT。

### `LGPL-2.1-or-later` — r-efi 等 11 项（多仓）

表达式均为 `MIT OR Apache-2.0 OR LGPL-2.1-or-later`。本生态**统一选择 MIT/Apache 分支**，因此不触发 LGPL 的动态链接与可替换性要求；已附 LGPL 正文以备下游自行选择。

---

## 五、分发义务矩阵（Wave 2 后已收口）

AGPL-3.0 的分发义务：§4（随附许可证副本 + 保留声明 + 修改版加「Appropriate Legal Notices」）、§6（分发对象代码须附 §4 要求的通知）、§13（网络服务须提供对应源码）。同时 MIT/BSD/ISC/Apache 等宽松许可要求**随分发物保留其版权声明与许可正文**。

| 仓 | 分发物 | 现状 | 结论 |
|---|---|---|---|
| `evorule` | crates.io 包（6 个 crate）+ CLI 双平台二进制 | ✅ 6 crate LICENSE 全部在位（cargo publish 自动附带）；CLI 合规归档随 Release（五件套 + SBOM） | dist-gate 已接线 |
| `evorule-server` | 零依赖分发包 win64.zip / linux64.tar.gz | ✅ 包内五件套（LICENSE + NOTICE + THIRD-PARTY-NOTICES + LICENSES/ + SOURCE.md）+ 源码 tarball | dist-gate 已接线（@a3aa299） |
| `evorule-rule` | 双平台双 bin | ✅ 独立合规归档 zip 随 Release（五件套）+ 源码 tarball | dist-gate 已接线（@a3aa299） |
| `evorule-bundle` | crates.io 包 | ✅ 已随包（LICENSE + 双许可文件 + NOTICE） | crate 闸：LICENSE 在位 |
| `evorule-hash` | crates.io 包 | ✅ 已随包（LICENSE + 双许可文件 + NOTICE） | crate 闸：LICENSE 在位 |
| `evorule-console-cloud` | 静态站点 zip + GitHub Pages | ✅ web 包内五件套 + 源码 tarball（GitHub Pages 在线 demo 同源构建） | dist-gate 已接线（@a3aa299） |
| `evorule-wasm-demo` | npm 包 evorule-wasm-demo | ⚠️ npm 发布无 CI 发布链（合规扫描器归类 none）；2026-10-24 实测包内含 LICENSE | 发布链 CI 化与第三方声明随包待后续批次 |
| `evo-agent` | npm 包 evo-agent-node | ✅ sdk/nodejs LICENSE + license 字段在位；npm publish 已 CI 化（sdk-nodejs-release.yml） | dist-gate 已接线（@a3aa299） |
| `evo-agent-desktop` | 桌面安装包 | ✅ 打包面（desktop-release.yml + desktop/build-installer.ps1）携带五件套 | dist-gate 已接线（@a3aa299） |
| `evorule-system-rules` | 仓库资产 | ✅ 仓根有 LICENSE | — |
| `evorule-context-inspector` | crates.io 包 | ✅ 已随包（LICENSE 在位） | 发布为手工操作，无 CI 发布链（合规扫描器归类 none） |
| `evorule-integration-tests` | 不分发 | — | — |
| `sync-sentinel` | 不分发 | — | — |

> 上表「现状」为 2026-10-25 对 Wave 2 门禁接线后各仓 release 链的复核（`check-dist-compliance.ps1` + `release.yml` 逐行核验）；2026-10-24 首轮审计的 6/13 分发缺口已全部收口。首轮对 crates.io 9 个 `.crate` 包的下载实测结论仍有效。

---

## 六、声明文件的放置规范

| 文件 | 位置 | 作用 |
|---|---|---|
| 本文件 | 根仓 `THIRD-PARTY-NOTICES.md` | 生态级第三方使用声明（总纲） |
| 许可证正文 | 根仓 `LICENSES/third-party/<许可标识>/` | 各许可证的逐字正文，供随分发物附带的 `LICENSES/` 目录使用 |
| 机器清单 | 根仓 `LICENSES/third-party/manifest.json` | 全量依赖-许可映射（CI 可校验） |
| 自身许可 | 各仓根 `LICENSE` | AGPL-3.0-or-later 全文 |
| 自身声明 | 各仓根 `NOTICE.md` | 该仓自身的版权/许可指引 |
| 依赖门禁 | 各仓根 `deny.toml` + CI `cargo deny check` | 许可白名单门禁 |
| 审计管线 | 根仓 `scripts/license-audit/` | 本文件的生成脚本（可复现） |

**分发时的落地做法**：每个分发包根目录放 `LICENSE`（AGPL 全文）+ `NOTICE.md` + `THIRD-PARTY-NOTICES.md`（本文件副本）+ `LICENSES/third-party/`（许可证正文目录）。AGPL §13 的源码提供入口在包的 `README` 或 `NOTICE.md` 中给出对应仓库地址与 commit。

---

## 七、复现与更新

```bash
# 审计管线位于 scripts/license-audit/（随本仓版本化，可复现）
cd scripts/license-audit

# 1. 提取各仓依赖与许可（cargo 缓存优先，crates.io 兜底）
python extract_licenses.py repos.json inv_raw.json
python resolve_licenses.py inv_raw.json inv_final.json

# 2. SPDX 语义求值与风险分级
python spdx_eval.py inv_final.json spdx_verdicts.json

# 3. 归档许可证正文（逐字取自 cargo 缓存 / npm registry）
python gen_third_party.py inv_final.json
python fetch_missing_licenses.py   # NCSA / LGPL-2.1：上游 crate 未分发正文
python finalize_manifest.py

# 4. 生成本文件（生态级总纲；--inv/--out 可改清单与输出路径）
python gen_notice.py

# 4b. 单仓子集声明（供分发包精简附带；缺省输出 THIRD-PARTY-NOTICES-<repo>.md）
python gen_notice.py --repo evorule-server
```

**更新 SOP**：新增依赖后重跑上述流程；若引入新许可标识，需同时
①在对应仓 `deny.toml` 的 `[licenses].allow` 登记并写明理由（根仓 `deny.toml` 第 4-5 行已定此纪律），②在本文档第三节表格登记，③在 `LICENSES/third-party/` 归档正文。

---

## 八、遗留与已收口事项

以下为 2026-10-24 首轮审计的待确认项及其 2026-10-25 状态：

1. **[已收口] `evo-agent-node`（npm）发布包缺 LICENSE** — `sdk/nodejs/` 的 LICENSE 文件与 package.json `license` 字段已在位；npm publish 已 CI 化（`sdk-nodejs-release.yml`）并接 dist-gate。
2. **[已收口] `evorule-discipline` / `evorule-eval`（crates.io）发布包缺 LICENSE** — 两 crate 根目录 LICENSE 已于 Wave 2 补齐，cargo publish 自动附带。
3. **[已收口] 两个 npm 工作区包未声明 license 字段** — `evo-agent-desktop`、`evorule-codegraph-bridge`（均为 private，不发布）均已补 `AGPL-3.0-or-later`。
4. **[已收口] `@colbymchenry/codegraph` 为 `file:` 仓外本地依赖** — 已以 vendoring 消除：构建产物 dist 入仓（`evorule-server/plugins/codegraph-bridge/vendor/`，MIT 正文随附），上游版本同版钉 0.9.4，可复现可溯源。
5. **[已收口] 依赖许可 CI 门禁覆盖** — Wave 2 已全生态接线：license-gate（防漂移比对 + cargo-deny 四闸）+ dist-gate（发布阻断）自根仓 reusable workflow 钉 SHA 复用，10 仓 deny.toml 统一。
6. **[已收口] 七件套一致性门禁红（17 项不一致）** — Wave 2 已重写对齐（见第二节）；核验覆盖面扩容至 16 仓进行中。
7. **[维持观察] `evorule-law-packs` 不在在役仓清单内** — 4 个判据包基于官方原文自行编写、未使用第三方译文（见各包 MAPPING.md 声明），法律风险低；维持条件触发：若未来随产品分发，需补其自身的许可声明（当前无 git 仓、无 LICENSE）。

---

*本文件由 evorule 许可证合规审计流程生成；数据来源为各仓 `Cargo.lock`、`package-lock.json`、`requirements*.txt`，许可正文逐字取自 cargo 本地 registry 缓存与 npm registry tarball（仅行尾按仓库约定统一为 LF 存储）。NCSA / LGPL-2.1 两份正文因上游 crate 不分发，另从 OSI 与 gnu.org 取，来源与理由见对应目录的 `SOURCE.txt`。*
