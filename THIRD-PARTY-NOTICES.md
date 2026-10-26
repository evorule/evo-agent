# THIRD-PARTY-NOTICES — evo-agent-desktop 分发面第三方组件使用声明

> 本文件为 `evo-agent-desktop` 分发面的第三方组件使用声明（**仓集子集**，覆盖仓：`evo-agent`、`evorule-server`、`evorule-console-cloud`），由根仓 `scripts/license-audit/gen_notice.py --repos evo-agent,evorule-server,evorule-console-cloud` 从生态级审计清单取并集生成。
> 义务边界与归档口径的完整说明见根仓生态级总纲 `THIRD-PARTY-NOTICES.md`。
> 生成日期：2026-10-26 · 本分发面依赖记录 1405 条

## 一、第三方许可总览（本分发面）

复合表达式（如 `MIT OR Apache-2.0`）按其可选分支拆分统计，同一依赖可能计入多行。

| 许可证 | 依赖记录数 | 涉及组件 | 义务 | 正文归档 |
|---|---|---|---|---|
| `MIT` | 1253 | `@asamuzakjp/css-color`, `@asamuzakjp/dom-selector`, `@babel/code-frame`, `@babel/helper-validator-identifier`, `@babel/runtime`, `@bramus/specificity` 等 | 保留版权声明与许可证全文，随二进制分发。 | ✅ |
| `Apache-2.0` | 732 | `@playwright/test`, `@tauri-apps/cli`, `@tauri-apps/cli-darwin-arm64`, `@tauri-apps/cli-darwin-x64`, `@tauri-apps/cli-linux-arm-gnueabihf`, `@tauri-apps/cli-linux-arm64-gnu` 等 | 保留版权声明、许可证全文与变更说明；上游若有 NOTICE 须一并附带（§4(d)）。 | ✅ |
| `Unicode-3.0` | 38 | `icu_collections`, `icu_locale_core`, `icu_normalizer`, `icu_normalizer_data`, `icu_properties`, `icu_properties_data` 等 | 保留版权声明与许可文本。 | ✅ |
| `Zlib` | 23 | `bytemuck`, `bytemuck_derive`, `foldhash`, `lru-slab`, `macro_rules_attribute`, `macro_rules_attribute-proc_macro` 等 | 保留版权声明与许可文本。 | ✅ |
| `ISC` | 23 | `forwarded-header-value`, `hmac-sha256`, `hyper-rustls`, `inotify`, `inotify-sys`, `lru-cache` 等 | 保留版权声明与许可文本。 | ✅ |
| `BSD-3-Clause` | 21 | `avif-serialize`, `curve25519-dalek`, `ed25519-dalek`, `encoding_rs`, `exr`, `highlight.js` 等 | 保留版权声明、条款与免责声明。 | ✅ |
| `Unlicense` | 19 | `aho-corasick`, `byteorder`, `byteorder-lite`, `globset`, `grep-matcher`, `grep-regex` 等 | 无义务。 | ✅ |
| `BSD-2-Clause` | 13 | `arrayref`, `av1-grain`, `entities`, `rav1e`, `serial2`, `v_frame` 等 | 保留版权声明、条款与免责声明。 | ✅ |
| `Apache-2.0 WITH LLVM-exception` | 12 | `blake3`, `linux-raw-sys`, `rustix`, `wasi`, `wasip2`, `wit-bindgen` | 保留版权声明与许可正文。 | ✅ |
| `CC0-1.0` | 8 | `blake3`, `constant_time_eq`, `imgref`, `mdn-data`, `notify` | 公共领域贡献，无义务。 | ✅ |
| `MIT-0` | 7 | `@csstools/color-helpers`, `@csstools/css-syntax-patches-for-csstree`, `borrow-or-share`, `constant_time_eq` | 保留版权声明与许可证全文，随二进制分发。 | ✅ |
| `BSL-1.0` | 6 | `clipboard-win`, `error-code`, `ryu`, `wasite`, `whoami` | 宽松许可：保留版权声明与正文。 | ✅ |
| `AGPL-3.0-or-later` | 6 | `@evorule/console-cloud`, `@evorule/evo-agent-workbench`, `evorule-bundle`, `evorule-discipline`, `evorule-hash` | 网络 copyleft：以网络提供服务时须向使用者提供对应源码（§13）。 | ✅ |
| `MPL-2.0` | 4 | `colored`, `dompurify`, `option-ext` | 文件级 copyleft：被修改的 MPL 文件须以 MPL 公开；分发时保留声明与正文。 | ✅ |
| `LGPL-2.1-or-later` | 4 | `r-efi` | 弱 copyleft：本生态统一选择 MIT/Apache 分支，不触发 LGPL 条款。 | ✅ |
| `CDLA-Permissive-2.0` | 4 | `webpki-root-certs`, `webpki-roots` | 宽松许可：保留版权声明与正文。 | ✅ |
| `0BSD` | 2 | `adler2` | 无附加义务。 | ✅ |
| `NCSA` | 1 | `libfuzzer-sys` | 宽松许可：保留版权声明与正文。 | ✅ |
| `BlueOak-1.0.0` | 1 | `lru-cache` | 宽松但非 OSI 认证：保留版权声明与正文。 | ✅ |
| `BSD-1-Clause` | 1 | `fiat-crypto` | 保留版权声明与许可正文。 | ✅ |

## 二、需特别标注的组件（本分发面相关）

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


## 三、本分发面分发义务

| 仓 | 分发物 | 现状 | 结论 |
|---|---|---|---|
| `evorule-server` | 零依赖分发包 win64.zip / linux64.tar.gz | ✅ 包内五件套（LICENSE + NOTICE + THIRD-PARTY-NOTICES + LICENSES/ + SOURCE.md）+ 源码 tarball | dist-gate 已接线（@a3aa299） |
| `evorule-console-cloud` | 静态站点 zip + GitHub Pages | ✅ web 包内五件套 + 源码 tarball（GitHub Pages 在线 demo 同源构建） | dist-gate 已接线（@a3aa299） |
| `evo-agent` | npm 包 evo-agent-node | ✅ sdk/nodejs LICENSE + license 字段在位；npm publish 已 CI 化（sdk-nodejs-release.yml） | dist-gate 已接线（@a3aa299） |
| `evo-agent-desktop` | 桌面安装包 | ✅ 打包面（desktop-release.yml + desktop/build-installer.ps1）携带五件套 | dist-gate 已接线（@a3aa299） |

