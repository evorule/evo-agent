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

# Evo-Agent 更新日志

`evo-agent`(EvoRule 生态的 Agent 编排层)的所有重要变更都记录在此。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/) v1.0,
本项目遵循 [语义化版本控制](https://semver.org/lang/zh-CN/) v2.0。

徽章说明:
- 🆕 新增
- 🔄 变更
- 🐛 修复
- 🗑 弃用
- ⚠️ Breaking Change
- 🔒 安全

---

## [Unreleased]

### 🆕 新增

#### IDE 工作台（Web IDE 前端，Trae 式布局）
- **内置 IDE 工作台**（`web/`，Svelte 5 + Vite 5 + Monaco Editor）— serve 直接托管（API 路由外层 fallback 到 `web/dist`，SPA 缺省回退 index.html）：左活动栏+文件树面板（S0 静态占位）、中编辑器群（Monaco 欢迎页验证内核与主题集成）、右对话侧栏（WS 双向流直连 agent 会话，真实模型流式回复 + 工具调用摘要卡 + 审批卡展示）、底部审计抽屉（默认收起占位）。设计 token 全量提取自 console-cloud 设计系统 v3.0（Docker 风格深色主题，字体/色板/间距/圆角/阴影逐项一致）；未构建前端时自动降级纯 API 模式
- **`tower-http` 新增 `fs` feature**（ServeDir/ServeFile 静态托管依赖）；工作台静态资源不经过 API 面鉴权中间件（页面须无 token 可打开），API 与 WS 面鉴权口径不变
- **工作台文件面（`src/api/file_api.rs`）** — 三端点 `GET /api/files/list` / `GET /api/files/read` / `PUT /api/files/write`，全部委托内置 file 工具实现（同一 workdir 沙箱与路径校验，不在 API 层重复安全逻辑）：list/read 复用 union toolkit 内同一工具实例（与 agent 完全同语义）；write 为人工编辑语义（`writable_dir="."` 使写面为 workdir 全域、固定 overwrite + create_parents，沙箱边界与 1 MB 上限原样保留）。受 G7 鉴权中间件保护；语义边界：本面服务人的直接操作，不构造 agent 会话事实（不进 agent 会话审计链），agent 路径 `file_write` 的 workspace 白名单与审批语义不变
- **文件树与编辑器真实化（前端）** — 文件树改为真实目录懒加载浏览（点击目录展开/收起、文件点击进编辑器）；编辑器多 tab（每文件独立 Monaco model 保留 undo 栈、dirty 角标、关闭切换）、Ctrl+S 保存落盘（走文件 REST 面，同 agent 写文件安全通道）、打开失败占位与保存失败提示
- **会话索引与历史恢复** — 新增 `GET /api/sessions`（会话列表：serve 本地 JSONL 索引，WS 面在会话创建/轮次结束时记录，读时去重合并，按最近活跃降序）与 `GET /api/sessions/{id}/transcript`（消息历史：从 evorule payload 投影 agent 持久化消息，零写入、同 idx 后写覆盖）；对话侧栏新增「历史」面板（点击恢复完整消息记录，含工具调用卡；继续对话自动接续同一会话），离开工作台再回来历史会话与内容完整可见；新增探针 `tests/ws_session_probe.mjs`（真实 LLM 会话级 E2E）
- **会话消息本地快照与保留期管理** — evorule 会话有 30 分钟闲置 TTL 自动回收，回收后历史投影报 `Session not found` 导致「历史恢复失败」；本轮落地：① WS 面 `TurnEnd` 时同批落本地消息快照（与 transcript 端点同一投影路径，保证同源同构；目录 `data/snapshots/YYYY/MM/DD/session_{sid}.json` 按日期分片，同 UTC 日原子覆盖）；② `GET .../transcript` 改为活投影优先 → 会话已回收时回落本地快照（响应带 `source: "snapshot"` + `authoritative: false`，前端恢复时明示「引擎侧会话已过期」）；③ 快照保留期配置 `GET/PUT /api/workbench/config`（`1d|1m|3m|6m|1y|forever`，缺省 3 个月，原子写 `data/workbench_config.json`），历史面板顶部下拉可改；④ serve 启动清扫 + 每日周期清理（每次现读配置，按日期分片目录整体删除）。**快照为展示层非权威副本**：审计真相源仍是 evorule FactsLog（append-only 永不删），删除策略只作用于快照目录，永不越界；回放/时光机器走引擎自审计链重建，零依赖快照
- **工作台治理叠加（evorule 功能落位）** — ① 底部面板从单审计抽屉升级为多 tab 面板（中栏底部、sash 拖拽调高/键盘 ↑↓ 微调、可折叠；标配 tabs：终端/输出/问题/控制台日志 + evorule tabs：审计/时光机器/记忆；实装「输出」= 系统事件流水与「审计」= 当前会话治理事件流 + 「在审计页查看」深链跳 console 审计页 `?session=<id>`；终端 PTY/问题/控制台日志/时光机器/记忆为占位待后续阶段）；② 审批卡交互：审批卡可直接「批准/拒绝」，走既有 `POST /agents/{t}/approve` 通道（60s 超时自动拒绝；无提案 ID/已失效/不匹配等情形在卡上明示）；③ 顶栏治理徽标：白名单（agent 工具数）+ 信号（当前会话违规信号累计，fail-soft）；④ 新增 `GET /api/sessions/{id}/evolution-signals` 只读代理（信号徽标数据源，透传 evorule-server 既有聚合端点，零写入、evorule-server 零改动）。**流式审批帧时序修复**：原实现 `ApprovalRequired` 在审批决策完成后才补发——帧到达前端时 60s 审批窗口已过，HTTP 审批在 WS/SSE 路径上结构性不可用（真实 LLM E2E 实测暴露）；修复为两阶段（先发 `ApprovalRequired` 开启窗口、决定后发 `ApprovalResult`），非流式路径与 CLI 交互审批不受影响；新增探针 `tests/ws_approval_probe.mjs`（真实 LLM 审批路径 E2E，批准/拒绝两路）
- **console 审计页 sidecar 自动拉起（可选配置）** — 首验实测发现审计深链指向的 console dev server（5174）未运行时为死链；新增 `[workbench] console_dir`/`console_port` 配置（缺省空 = 零副作用）：serve 启动期端口探测未监听则子进程拉起 vite dev（fail-soft 不阻断主功能、输出落 `data/console_sidecar.log`、独立存活幂等）；前端深链点击前探活，不可达时就地明示引导不跳死链；修复中连带发现 `Config::merge_from_file` 逐段合并漏新段（新配置段静默不生效）并修复

#### 进化巡视任务模式（一次性自进化编排）
- **`evo-agent patrol` 子命令** — 一次性进化巡视：信号探查（不调 LLM）→ 有信号则两轮制编排（轮A agent 拉信号明细 + 起草约束层草稿三步，草稿为 **enforce 拦截型**（`type=enforce` + `params.domain/reason`），提取后经提交期结构校验（条目级键白名单 `{type,params}`、拒绝 set 留痕型、`reason` 非空——与 server schema 门禁同口径），失败 fail-fast 落报告退出；进程侧组装闸门一沙盒证据：数据集 → 沙盒 → 关闭；轮B agent 携证据调 `rule_promote` 提名）→ 结构化 JSON 巡视报告（一行 JSON 到 stdout，`--out` 可追加归档）；**无信号零动作静默退出**（`status=no_signal`，exit 0）。触发器在本进程/外部调度，server 零自治循环；重复提名被服务端双态去重门禁拒绝时报告 `status=duplicate_rejected`。默认 `rule-copilot` 档案（提名工具在协作体白名单）
- **`serve` 启动期档案预载校验** — 启动时预载 agents 目录全部档案：缺失/坏 JSON/语义非法 fail-fast 并逐项列明；相对 `agents.dir` 改为相对 `--workdir` 解析（与 config 加载基准一致，不再受进程 cwd 影响，报错含实际解析基准）——消灭「会话期才报 agent not found」的延迟故障
- **`publish_list` 工具 `workspace_id` 过滤参数** — 与 server 侧同名查询参数对齐，治理队列跨工作空间污染根治（消费侧透传）

#### 档案与测试
- **`agents/general.json` 白名单收录 `evolution_signals`**（19 → 20 工具）— general 会话可感知违规态势（只读）；`rule_promote` 维持协作体档案独占（提名权收敛）
- **README 工具计数门禁测试** — 单测断言 README 规则工具集计数与 `rule_tool_specs()` 实际数量锁定，防文档漂移

#### 进化信号（自进化信号消费 + 约束层晋升提名）
- **`evolution_signals` 工具** — 拉取指定会话的违规信号聚合摘要（`GET /api/sessions/{id}/evolution-signals`，只读）：按规则归因聚合 enforce 拦截记录（计数降序→规则引用升序确定性排序）＋治理队列现状（待审普通规则/待审约束层晋升计数）；无信号返回「当前无进化信号」明示文本。规则工具集 43 → 45（serve union 30 → 32）；`agents/rule-copilot.json` 白名单加入 `evolution_signals` / `rule_promote`（版本号随动 0.2.0 → 0.3.0）
- **`rule_promote` 工具** — 约束层晋升提名：经治理链发布队列（`POST /api/publish/queue`）提名，`kind` 在工具实现内硬编码为 `meta_promotion`（LLM 无改道普通通道的口子）；`promoted_from` 等溯源字段由服务端权威预填（防伪造）；进入人审队列后由治理方审批，agent 面不提供任何审批通道
- **进化信号感知段前馈注入** — serve 三路径共用：与 L2 边界段同触发条件（起草族工具命中），静态文本注入 `evolution_signals` 工具感知（构造期会话未创建、无法预判活跃信号，活跃信号由 LLM 运行期经工具实时拉取——会话口径）；纯文本追加，fail-soft 天然满足

#### 元规则（L2 约束）接入
- **`meta_summary` 工具** — 查询当前生效的 L2 约束（元规则）清单摘要（`GET /api/rules/l2-inventory`，只读）；渲染为人类可读摘要文本（含守卫边界声明、禁项清单、路径读写约定、守卫指令类型），无 L2 时返回「当前无 L2 约束规则」明示文本。规则工具集 42 → 43（serve union 29 → 30）；`agents/general.json` / `agents/rule-copilot.json` 白名单各加入 `meta_summary`（版本号随动 0.3.0 / 0.2.0）
- **L2 约束边界段前馈注入** — serve 三路径（WebSocket / 同步 run / SSE run-stream）共用 helper：agent 工具白名单命中 `rule_create` / `rule_update` / `rule_validate` 之一时，runner 构造期实时拉取 L2 清单并把边界段追加到 system_prompt 尾部（memory recall 包装在外层，既有语义顺序不变）——LLM 生成规则草稿前先知道约束边界在哪，降低「生成即被拒」的无效消耗。纯消费 agent 不注入；拉取失败或清单为空 fail-soft 不注入（warn 留痕，绝不阻断会话）

#### 生态公共设施化（公共契约面 + 契约测试 + 治理文档）
- **公共契约面显式登记** — crate 级文档新增"公共契约面"章节：`LlmHandler`/`StreamChunk`/`ToolHandler` +
  `AuditedLlm` + `EvoruleApiClient`/`ApiError` + `IoHandler` + `config` 受 semver 约束
  （0.x 内 breaking 必须升 minor）；内部模块（builtin_tools/mcp/rule_tools/io_dispatcher/json_convert/metrics）
  标注 `#[doc(hidden)]`，真收窄留待 0.2.0
- **审计桥契约测试 +3** — 协议常量锁定（90s 超时/建链重试 1 次，防无声变更）、
  `is_transient_setup_error` 全分支（真实错误实例）、
  LLM 失败 → 错误回写 io_response 不留悬空 IoRequest（回归锁定）
- **`LlmHandler::with_max_retries`** — builder 风格重试次数覆盖（测试用 0 关闭退避延迟）
- **`NOTICE.md`** — AGPL-3.0 + 商业双许可声明 + 生态/第三方依赖清单（对齐生态范式）
- **`docs/RELEASE_PROCESS.md`** — 发布操作手册（git tag 形态 + crates.io 前置条件如实声明）
- **`verify.ps1`** — 一键验证：build → test → 防泄漏扫描 → 依赖契约断言
- **README"依赖契约"章节** — 引擎 crate 走 crates.io 版本依赖，仓外直接构建；path 依赖回流由 verify.ps1 断言拦截

#### 服务消费桥（执行侧服务能力对账与消费）
- **服务发现消费** — 启动期经服务对账端点拉取执行侧服务清单,按白名单过滤注册为本地工具(本地重名跳过,单测锁定);服务发现失败 fail-fast,不静默空跑
- **参数契约缓存与查询** — 服务参数契约查询能力落地(契约由服务提供方声明为唯一权威,server 只透传不解释)
- **动态工具 schema 生成** — 向 LLM 注入工具描述时,从参数契约生成 function schema,LLM 可带参真实调用服务(无契约声明时降级空 schema)

#### 本地运维脚本族（服务稳定化：看门狗 + 开机自启）
- **`scripts/ops/` 运维脚本族** — 解决本地三服务（evorule-server 18080 / serve 8081 / console 5174）「进程挂在终端/会话后台作业下、关终端或重启后消失、崩溃无人拉起、重启漏带密钥」四类不稳定：① 各服务幂等启动脚本（HTTP/TCP 探活通过即跳过，轮询等待启动完成，手动重跑即「一键全启」）；② `start-evo-agent-serve.ps1` 自动注入 `.env` 环境变量（LLM 密钥等，日志只回显键名绝不回显值）；③ `watchdog-check.ps1` 单次巡检（不通则拉起，命名互斥防重叠）；④ `install-watchdog-task.ps1` 注册计划任务 `EvoruleOpsWatchdog`（用户登录触发 + 每 1 小时巡检自愈，服务以分离进程独立于终端/会话存活）。本机真实路径走 gitignored `ops.local.json`（模板 `ops.local.example.json` 以中性路径入库）；纯运维层工具，不触碰引擎执行面（哈希链/Fact/审计语义零依赖）
- **serve 启动期 `.env` 自动加载 + 密钥缺失告警** — 新增零依赖最小实现 `src/dotenv.rs`（workdir/exe 目录候选、已设环境变量优先不覆盖、支持引号/`export ` 前缀解析，含单测），serve 裸启动也能携带 LLM 密钥；三个 provider 密钥全缺失时启动期打印醒目 WARNING 指引配置——根除「重启漏注入导致 LLM 登录失败」复发点（登记册 O-095 建议①③闭环，运维脚本注入退居双保险第二层）
- **看门狗巡检间隔调整** — 每 1 分钟 → 每 1 小时（项目方裁定「没必要太频繁」），开机自启仍由登录触发保证，进程死亡后最长 1 小时内自愈

### 🔄 变更

- **宪法 schema 校验收编共享组件 `evorule-constitution`** — `src/agent/constitution.rs` 由本地实现（目录探测/跨文件 `$ref` 内联/校验执行）改为薄封装（公共 API 签名不变，消费点零改动）：判定逻辑与 schema 数据（编译期内嵌，运行时零磁盘依赖）委托统一 crate（git 依赖 + rev 钉版），判定代码单一化；`jsonschema` 0.18 → 0.21（`Validator` API，跨文件 `$ref` 经 `$id` 解析，内联 hack 退役）；CI「检出宪法仓」步骤退役（cargo 自动拉取 git 依赖）；依赖契约断言精确化（主仓 crates 拦截保留，独立仓治理组件显式 allowlist）
- **`list_publish_queue` 客户端方法** — 新增可选 `workspace_id` 过滤参数（None 保留旧行为）；`PublishQueueItem` 补齐 `kind` 字段（对齐 server 模型）
- **移除 `blake3` 直接依赖** — 零代码调用（仅文档注释提及概念），死依赖删除；
  evorule-reactor 自身对 blake3 的依赖不受影响
- **移除 `evorule-tcb` / `evorule-reactor` 依赖** — Agent 编排层与 evorule 仓 TCB/Reactor 代码解耦；
  `evorule_tcb::JsonValue` 全面替换为 `serde_json::Value`（IoHandler trait 签名与工具参数同步），
  专用转换模块 `json_convert` 不再需要，随之删除

### 🐛 修复

- **工作台对话侧栏交互失效(Svelte 5 runes 响应性)** — 对话侧栏组件混用 `$effect`(触发
  runes 模式)与普通 `let` 顶层状态,runes 模式下普通 `let` 不具备响应性:点「历史」面板
  不展开、输入文字后发送按钮不点亮;现改为 `$state()` 声明并在源码处注明语义防复发
- **会话索引幽灵条目** — 页面残留不存在的 session id 重连后,失败的续用轮次也会写入会话
  索引,历史面板出现点击即失败的「无标题」死条目;现仅当会话真实建立过(`SessionCreated`)
  或本轮执行成功时才记录索引
- **lib.rs crate 级文档 mojibake** — 门面文档乱码修复为正常中文
- **工具循环在部分 LLM 供应商下静默不执行工具** — 工具描述(schema)此前未随 LLM 请求注入,
  模型只能凭训练先验盲猜工具名或输出供应商私有格式,循环一步即止且表现为假成功;
  现请求携带 OpenAI function 形状的工具描述(指令面与实际 LLM 请求面双接线),
  模型返回标准工具调用,多轮工具循环真实执行(新增集成测试锁定两条请求面)
- **demo 成功判据补业务落盘校验** — 原"退出码 + 审计链验证"双判据可被
  "什么都没做但审计如实记录"的假成功绕过;现要求业务文件真实写入(含金额记录)
  才宣告完成,失败时审计报告照常输出供排查(demo.ps1 / demo.sh 同步)

### 2026-09-07 变更

- **`verify.ps1` 布局断言 → 依赖契约断言** — 引擎 crate 已 crates.io 化，原"并排检出主仓"断言对外部用户必失败；
  现断言 Cargo.toml 中不存在 path 依赖（防回流），仓外 clone → verify 全 PASS
- **README"源码布局契约"章节改写为"依赖契约"** — 移除过时的并排检出要求
- **CHANGELOG 历史条目勘误** — `[1.0.0]` 为内部首发代号（未发布公开 tag），补注记澄清；修正失实链接与"首个公开发布"表述
- **`evorum.api_key` 配置接线** — 配置文件声明的 evorule API key 现已真实生效(Bearer 鉴权);优先级:显式声明 > 环境变量(`EVORULE_SERVICE_TOKEN`/`EVORULE_AUTH_TOKEN`)> 无认证;空声明回落环境变量,与 server 密钥优先级语义对齐(单测锁定优先级全表)
- **相对 agents 目录按工作目录解析** — 修复非仓根目录运行时 agent 定义加载失败;相对 agents 目录以 workdir 为基准解析(与配置加载基准一致),不再依赖进程当前目录

## [0.1.0] - 2026-07-20

`evo-agent` 与 EvoRule 主线同步从 0.1.0 开始(原 1.0.0 版本号退役)。

### 🆕 新增

- **AgentRunner 真实 LLM 调用** — `execute_llm_request` 通过 `LlmHandler` 走真实 HTTP API(替换原 stub)
- **tests/llm_real_smoke.py** — 4 场景真实 MiniMax 端到端测试

### 🔄 变更

- **协议统一为 AGPL-3.0-or-later**
- **.gitee/PULL_REQUEST_TEMPLATE.md** — 复用 evorule 的人类审查 checklist
- **65 个 .rs 文件 SPDX header** — 含 evo-agent 全部 17 个 + evorule-reactor/src/ffi.rs

### 🐛 修复

- **版本号 1.0.0 → 0.1.0** — 与 evorule 主线对齐
- **PR 模板 + Gitee CI** — 本仓 `.gitee/` 目录配置

### ⚠️ Breaking Changes

- **API 不承诺后向兼容**(SemVer 0.x 阶段)
- **execute_tool_request 仍是 stub** — 0.2.0 修

---

## [1.0.0] - 2026-07-19（内部首发代号）

> **注记（2026-09-07 勘误）**：1.0.0 为内部开发期原始版本号，**未发布为公开 git tag**，已被 0.1.0（与 EvoRule 主线对齐）退役——首个公开版本是 `[0.1.0]`。下文为内部时期快照，历史原貌保留；其中"首个公开发布"及引擎版本号等表述以本注记为准。

内部时期功能基线。

### ⚠️ 已知限制(v1.0)

- ⚠️ **LLM Handler 是 stub**(`io_handlers/llm_handler.rs::execute_llm_request` 返回 `"Simulated LLM response"`)
- ⚠️ **Tool Handler 是 stub**(`io_handlers/tool_handler.rs::execute_tool_request` 返回 `"Tool execution result: ..."`)
- ⚠️ 124 warnings(主要是 `missing_docs`,可通过 `cargo fix --lib -p evo-agent` 一键补)
- ⚠️ 1 unused import(`serde_json::Value` at `src/agent/memory.rs:15`)

> **生产环境使用前,请实现真实的 LLM / Tool Handler**。详见 README 的"接入真实 LLM"章节。

### 🆕 新增

#### 核心模块(`src/agent/`)

- **`AgentRunner`**(~1042 行) — ReAct 主循环
  - 事件驱动架构
  - 完整 Fact 闭环:Command → IoRequest → 外部调用 → IoResponse → Stable
  - `AgentConfig` 配置(agent_type / system_prompt / model / temperature / max_steps / step_timeout / tool_names / llm_retry_count)
  - `AgentResult` 结果(success / content / steps / duration_ms / tool_calls / error)
  - 8 种错误类型:`LlmError` / `ToolError` / `Timeout` / `MaxStepsExceeded` / `DelegateError` / `MemoryError` / `Internal` / `EvoruleError`

- **`MemoryManager`**(~460 行) — 三层记忆管理
  - 命名空间约定:
    - 共享:`__memory__.agent_{type}.shared.{key}`
    - 会话:`__memory__.agent_{type}.session_{id}.{key}`
    - 短期:`__memory__.agent_{type}.session_{id}.messages.{idx}`
  - 通过 evorule `POST /api/sessions/{id}/payload` API 写入
  - 6 种错误:`Io` / `Json` / `EmptyKey` / `KeyTooLong` / `EvoruleError` / `SessionNotSet`
  - 自动重试 + 跨调用 ID 跟踪

- **`AgentDefinitionManager`**(~365 行) — 从 `agent.json` 加载 Agent 配置
  - `MemoryConfig` 字段(shared_keys 等)
  - `OutputFormat` 枚举
  - 3 种错误:`Io` / `Json` / `NotFound`

- **`ToolRegistry`**(~280 行) — 工具注册中心
  - `ToolFunction` async trait
  - `ToolSpec` / `ParameterSpec` 工具规格
  - 参数验证(必填检查)
  - 动态注册 / 注销

- **`Translator`**(~175 行) — LLM 响应解析
  - `Message` 枚举(System / User / Assistant / Tool)
  - `LlmResponse` 包装
  - `ToolCall` 解析

- **`Delegate`**(~135 行) — Agent 嵌套
  - `DelegateContext` 跨调用状态
  - 默认最大嵌套深度:`DEFAULT_MAX_DELEGATE_DEPTH = 3`

#### HTTP API(`src/api/`)

- **`EvoruleApiClient`**(~530 行) — 透传 19 个 evorule 端点
  - 会话管理:`create_session` / `fork_session` / `list_sessions`
  - 命令:`command` / `update_payload` / `state`
  - 时间机器:`replay` / `rewind` / `diff`
  - 审计:`audit` / `audit_verify`
  - 共享 Fact:`shared_facts` / `shared_fact_source` / `shared_fact_used_by`
  - 集群:`join` / `leave` / `cluster_status`
  - I/O 提交:`submit_io_response` / `record_used_at_startup`

- **`AgentApi`**(~240 行) — Evo-Agent 自有 HTTP 服务
  - `POST /api/agent/run` — 启动 Agent 运行
  - `GET /api/agent/list` — 列出已注册 Agent
  - `GET /api/agent/{type}` — 查看 Agent 详细定义
  - `AgentRunRequest` / `AgentRunResponse` / `AgentInfo` / `AgentDefinitionResponse` / `AgentListResponse`
  - axum 路由 + State 注入

#### I/O 抽象(`src/io_handlers/`)

- **`LlmHandler`** trait — LLM 调用抽象
  - `execute_llm_request(model, system_prompt, messages, tools) -> Result<JsonValue, String>`
  - **v1.0 状态**:`src/io_handlers/llm_handler.rs::execute_llm_request` 返回 `"Simulated LLM response"`
  - **接入真实 LLM** — 实现 trait,替换默认 handler

- **`ToolHandler`** trait — 工具调用抽象
  - `execute_tool_request(tool_name, args) -> Result<JsonValue, String>`
  - **v1.0 状态**:`src/io_handlers/tool_handler.rs::execute_tool_request` 返回 `"Tool execution result: ..."`

#### 工具模块

- **`io_dispatcher`**(~80 行) — I/O 派发
- **`io_handler`**(~15 行) — `IoHandler` trait + `IoResult`
- **`json_convert`**(~110 行) — `serde_json::Value` ↔ `evorule_tcb::JsonValue` 转换

### 🔄 变更

- **依赖**:
  - `evorule-tcb`（monorepo path 依赖，历史形态）
  - `evorule-reactor`（monorepo path 依赖，历史形态）
  - `tokio` (full features)
  - `reqwest` 0.12(异步 HTTP 客户端)
  - `axum` 0.8(HTTP 服务)
  - `serde` / `serde_json`
  - `prometheus` 0.13
  - `tracing` 0.1
  - `blake3`(审计链哈希)
  - `clap` 4(CLI)
- **dev-deps**:`tempfile` / `mockito` / `tokio-test`
- **Lints**:`#![forbid(unsafe_code)]` / `#![warn(unused_imports)]` / `#![warn(unused_variables)]` / `#![warn(missing_docs)]`

### 🔧 工程

- **Rust >= 1.74**
- **Cargo workspace**:独立 crate(`evo-agent` 在自己 `Cargo.toml` 里)
- **2 层架构**:
  - **机制层** = evorule (Rust,tier0/tier1/tier2)
  - **应用层** = evo-agent (本项目)
  - **通信**:HTTP + JSON(不是直接 import)
- **测试**:`tests/integration_test.rs`(260 行,mockito mock evorule-server)

### 🔒 安全

- **Bearer 认证**:通过 `EvoruleApiClient` 转发
- **输入验证**:JSON 指令 schema 校验
- **错误处理**:`AgentError` 8 种,显式 `From` 转换
- **限流**:通过 evorule-server `tower_governor` 实现

### 📜 协议

- **evo-agent 代码**:AGPL-3.0-or-later(与 evorule 主项目同步)
- **依赖的 evorule-server 协议**:HTTP + JSON
- **`core_eval.json` 宪法**:CC0 1.0 公共领域(由 evorule 维护)

---

## 兼容性矩阵

| evo-agent | evorule-server | 状态 |
|---|---|---|
| 1.0.x | >= 6.0.0 | ✅ 当前 |
| 0.x | (未发布) | ❌ 计划从 1.0 起步 |

---

## 升级指南

### 0.x → 1.0.0(无 0.x 版本,跳过)

### 1.0.x → 1.0.y(patch)

无 breaking change,直接升级:
```bash
cargo update evo-agent
```

### 1.x → 2.x(未来 major)

待定。

---

## 与 EvoRule 主项目的关系

```
┌────────────────────────────────────────┐
│  应用层:evo-agent (本项目)              │
│  - LLM 编排                            │
│  - 工具注册                            │
│  - 记忆管理                            │
│  - HTTP API                            │
└────────────────┬───────────────────────┘
                 │ HTTP + JSON
                 │ (不直接 import)
┌────────────────┴───────────────────────┐
│  机制层:evorule (独立仓库)              │
│  https://gitee.com/evorulelab/evorule  │
│  - evorule-tcb (核心)                    │
│  - evorule-reactor (反应器)              │
│  - evorule-governance (HTTP/SSE)         │
└────────────────────────────────────────┘
```

**关键设计**:**机制 vs 应用分离**。evo-agent 是**应用层**,通过 HTTP API 与 evorule(机制层)通信。**不**共享内存,**不**嵌入 evorule 进程。

这个分层是 EvoRule 的核心原则 — 让**机制可独立验证**(无业务污染),让**应用可独立演化**(LLM 升级不需改 evorule)。

---

## 与 SDK 的关系

| 项目 | 语言 | 角色 |
|---|---|---|
| **EvoRule 主项目** | Rust | 反应式执行引擎 |
| **evo-agent** | Rust | Agent 编排层(本项目) |
| **TypeScript SDK** | TypeScript | JS/TS 应用集成 evorule |
| **Python SDK** | Python | Python 应用集成 evorule |
| **Go SDK** | Go | (计划中) |
| **Java SDK** | Java | (计划中) |
| **Web SDK** | Web | (计划中) |

**evo-agent 和 SDK 的区别**:
- **evo-agent** — 内置 LLM 编排,直接跑 Agent
- **SDK** — 仅 HTTP API 客户端,需要你自己写 Agent 逻辑

---

## 历史背景

`evo-agent` 早期是 evorule 项目内部的一部分(在 `evorule/evorule-governance/` 中),后按"机制 vs 应用分离"原则拆出到独立仓库。

**为什么拆出**:
- 机制层(确定性 / 可审计 / 可形式化验证)应独立
- 应用层(LLM / 业务 / 工作流)应可独立演化
- 边界清晰 = 各自可独立测试

**拆出后**:
- `evorule-governance` 只剩纯机制(无 agent 逻辑)
- `evo-agent` 自成项目,通过 HTTP API 与 evorule 通信
- 各自的测试 / CI / 发布独立

---

**作者**: EvoRule Project
**邮箱**: evorulelab@gmail.com
**Gitee**: https://gitee.com/evorulelab/evo-agent
**主项目**: https://gitee.com/evorulelab/evorule

---

**本变更日志遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/) v1.0 格式。**
