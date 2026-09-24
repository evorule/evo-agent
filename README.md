# Evo-Agent

> 可信 AI 工作站 —— 在 evorule 确定性执行引擎之上，为开发者和企业提供基于规则约束的 AI Agent 编排层。

[![CI](https://github.com/evorule/evo-agent/actions/workflows/ci.yml/badge.svg)](https://github.com/evorule/evo-agent/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-1.74%2B-orange.svg)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)
[![Version](https://img.shields.io/badge/version-0.1.0-green.svg)](Cargo.toml)

---

## 定位

**Evo-Agent = LLM 大脑 + 工具手脚 + 持久记忆 + 规则约束，执行过程通过 evorule 引擎留下可审计的 Fact 链。**

它不重新发明状态机，也不内嵌 LLM 客户端。所有 LLM 调用、工具调用、记忆读写都转成 evorule 的 `IoRequest` 事件，由 evorule 反应器负责执行、回滚、审计 —— Agent 层只关心"下一步该干什么"。

### 三种 AI 角色

| 角色 | 说明 | 对应 Agent |
|------|------|------------|
| **规则创建助手** | 自然语言 → JSON 规则（LLM 生成 + G1-G7 校验 + 热重载） | `rule-copilot` |
| **AI 执行器** | 通过 `call_external` 执行 LLM/工具调用，受规则约束 | `general` / `researcher` |
| **对话管理入口** | 自然语言管理规则生命周期（创建/提交/激活/归档） | `rule-copilot` |

### 30 秒看一眼

![evo-agent CLI 概览](docs/evo-agent-cli.gif)

*真实终端录制：`evo-agent list` / `tools show` / `validate` / `tools list` —— 6 个内置工具的 3 层安全模型（active 白名单 / candidate 待批 / blocked 永不）。*

---

## 核心特性

| 特性 | 说明 |
|------|------|
| **完整 Fact 闭环** | 每次 LLM 调用、工具调用、记忆读写都生成可审计 Fact，支持 rewind / replay / diff |
| **三层记忆** | 共享记忆（`shared.{ns}.{key}`）/ 会话记忆 / 短期消息，跨会话可追溯 |
| **跨会话共享事实** | `SharedFactsLog` WAL 持久化 + rollup 标记 + 按 ID 审计回溯 |
| **记忆事件链** | 结构化事件提取 + 因果链 + 确定性回放（`replay` 命令） |
| **会话沉淀** | 会话结束时自动写入摘要 + 稳定事实到共享空间 |
| **工具注册中心** | `ToolRegistry` + `ToolFunction` trait，任何 `async fn(JsonValue) -> Result<JsonValue, String>` 都能注册 |
| **3 层安全模型** | active（白名单）/ candidate（待批）/ blocked（永不），含 SSRF 防护 + 工作目录沙箱 |
| **规则管理工具集** | 45 个工具：workspace 2 + rule 12 + translate 3 + audit 3 + sandbox 5 + dataset 2 + publish 5 + production 2 + bundles 5 + knowledge 3 + meta 1 + evolution 2 |
| **工作流引擎** | DAG 拓扑编排多 Agent，同层并行 + 跨层串行 + 模板渲染 |
| **MCP 客户端** | 接入 Model Context Protocol 工具生态（stdio 传输） |
| **上下文窗口管理** | 按 token 数裁剪历史消息，保留 system + 最近若干轮 |
| **审批系统** | CLI 交互审批 / HTTP 回调审批 / 自动批准三种模式 |
| **HTTP + WebSocket + SSE** | REST API 启动 Agent，SSE 流式输出，WebSocket 双向通信 |
| **REPL 交互模式** | 对话式复用同一 session，支持 `/rewind` 回滚 |
| **可插拔 LLM/工具** | `LlmHandler` / `ToolHandler` trait，接入 OpenAI / MiniMax / DeepSeek 等 |
| **零 unsafe** | `#![forbid(unsafe_code)]` 全栈适用 |

---

## 架构：2-Loop 解耦

Evo-Agent 是独立的应用层，通过 HTTP API 与 evorule 引擎对话：

```
┌──────────────────────────────┐         ┌──────────────────────────────┐
│  Evo-Agent (应用层)           │         │  evorule-server (机制层)      │
│                              │         │                              │
│  ┌──────────────────────┐    │         │  ┌──────────────────────┐    │
│  │  Application Loop    │    │         │  │  Reactor Loop        │    │
│  │                      │    │         │  │                      │    │
│  │  for step in 0..N:   │    │         │  │  drain command       │    │
│  │    LLM call  ───┐    │    │  HTTP   │    │  stable detect       │    │
│  │    parse resp   │    │    │ ──────> │    │  block on cmd_rx     │    │
│  │    handle io_   │    │    │ <────── │    │  while pending_io=0: │    │
│  │      request    │    │    │   SSE   │    │    execute_transition│    │
│  │    POST io_resp │    │    │         │    │                      │    │
│  └──────────────────────┘    │         │  └──────────────────────┘    │
│           ▲                  │         │           │                  │
│           │                  │         │           ▼                  │
│  ┌────────┴─────────┐        │         │  ┌──────────────────────┐    │
│  │ MemoryManager    │        │         │  │ FactsLog (append)    │    │
│  │ ToolRegistry     │        │         │  │ + causal chain       │    │
│  │ AgentDefinition  │        │         │  │ + WAL + SharedFacts  │    │
│  │ ContextWindow    │        │         │  └──────────────────────┘    │
│  │ WorkflowEngine   │        │         │                              │
│  │ McpClient        │        │         │                              │
│  └──────────────────┘        │         │                              │
└──────────────────────────────┘         └──────────────────────────────┘
```

**为什么分开？** 把"机制"和"应用"塞进同一个进程，会导致确定性、可审计性、可形式化验证的边界污染。EvoRule 的 TCB 永远只做加减与因果链，EvoAgent 的所有业务逻辑都在它之外。HTTP 是它们的唯一契约。

---

## 快速开始

### 前置条件

- Rust 1.74+
- 运行中的 evorule-server（默认 `http://127.0.0.1:18080`）
- LLM provider 的 API key

### 5 分钟 Demo（一条命令）

准备一个 MiniMax API key（或 DeepSeek），然后：

```bash
# Windows (PowerShell)
$env:MINIMAX_API_KEY = "your-api-key"
powershell -ExecutionPolicy Bypass -File demo.ps1

# Linux / macOS
export MINIMAX_API_KEY=your-api-key
./demo.sh
```

脚本自动完成：下载并启动 evorule-server（Gitee Release 整包，端口 18080，随机 token 认证）→ 写入项目配置 → 构建 → 跑一笔费用登记会话（LLM + 工具调用全部转为可审计 Fact）→ 调用 `audit/verify` 验证 Fact 哈希链并打印完整审计报告。完成后浏览器打开 http://localhost:18080 可查看审计页。重置：删除 `.demo/` 目录即可。

### 依赖契约

evo-agent 自 O-044（2026-09-20）起与 evorule 仓完全解耦：Cargo 依赖面仅第三方 crates，
不引用任何 `evorule-*` crate，与 evorule-server 的交互只走 HTTP/WS 协议——clone 本仓后直接构建：

```bash
git clone https://gitee.com/evorule/evo-agent.git
cd evo-agent
cargo build          # 依赖自动从 crates.io 解析
```

- **禁止 `evorule-*` 依赖**：本仓是 Agent 编排层，不依赖 evorule 仓机制层代码（TCB / Reactor）；
  `verify.ps1` 第 4 步（依赖契约断言）会在 Cargo.toml 出现任何 `evorule-*` 依赖时判 FAIL。
- 运行时仍需一个可达的 evorule-server 实例（见下节）。

### 启动 evorule-server

```bash
cd ../evorule-server
cargo build --release

# 基础启动（审计 / 记忆 / 时间机器 / 规则热重载）
./target/release/evorule-server --addr 127.0.0.1:18080 --wal-dir ./data/wal

# 若需 rule-copilot / general / researcher 通过 call_external 调用外部服务，
# 必须额外挂载服务注册表并放行本机回环（详见 evorule-server/README.md）：
./target/release/evorule-server --addr 127.0.0.1:18080 --wal-dir ./data/wal \
  --service-registry ./service_registry.json --allow-loopback
```

> 角色 1/3（`call_external`）在 evorule-server 侧已就绪：挂载 `service_registry.json` 后即可跑通。仓库内置 `echo_server.py` + `dev-start.sh` 演示环境，参考 evorule-server 实战指南。

> **外部插件包生态**：evo-agent 可消费的服务来自三种来源——server 内置（native）、外部插件包（独立服务进程 + `plugin.json` 声明清单，规范见 evorule-server 仓 `docs/PLUGIN_GUIDE.md`）、注册表绑定（registry）。agent 启动期经服务对账端点发现可用服务，按配置白名单注册为本地工具；声明了参数契约的服务会自动生成工具 schema，LLM 可带参真实调用。

### 启动 Evo-Agent HTTP API

```bash
cargo run --release -- serve --port 8081
```

### 打开工作台（Web IDE）

serve 同时托管内置的 IDE 工作台（Trae 式布局：左文件树 / 中编辑器 / 右对话侧栏 / 底部审计抽屉），风格与 evorule 设计体系一致：

```bash
# 首次使用先构建前端（产物在 web/dist，已 gitignore）
npm --prefix web install && npm --prefix web run build

cargo run --release -- serve --port 8081
# 浏览器打开 http://127.0.0.1:8081/ 即工作台
```

- 右侧对话侧栏直连 agent 会话（WS 双向流，见 API.md §6），真实模型流式回复
- 对话历史：「历史」面板列出近期会话（serve 本地索引），点击恢复完整消息记录（见 API.md §11），离开工作台再回来历史完整可见；每轮结束自动落本地消息快照（`data/snapshots/YYYY/MM/DD/`），引擎会话闲置 30 分钟被 TTL 回收后恢复自动回落快照并明示来源；「历史」面板顶部可配置快照保留期（1 天/1 个月/3 个月/半年/1 年/长期，缺省 3 个月，见 API.md §11.3）。快照为展示层非权威副本，审计真相源仍是 evorule FactsLog（永不删除）
- 左侧文件树浏览工作目录（懒加载展开），点击文件在中栏编辑器打开
- 编辑器多 tab（Monaco 内核），`Ctrl+S` 保存落盘——与 agent 写文件走同一 file 工具实现（同 workdir 沙箱与安全校验，见 API.md §7）
- 底部多 tab 面板（中栏底部、拖拽调高、可折叠）：「输出」呈现系统事件流水、「审计」呈现当前会话治理事件流（工具调用/审批/错误）并提供「在审计页查看」深链跳转 console 审计页（`?session=<id>` 定位；默认 `http://localhost:5174`，localStorage `evo_console_origin` 可改）；终端（PTY）/问题/控制台日志/时光机器/记忆为占位待后续阶段。深链点击前自动探活——console 未运行时就地明示引导，不跳死链
- **console 审计页自动拉起（可选配置）**：在 `evo-agent.toml` 配置 `[workbench] console_dir = "<console-cloud 仓目录>"`（可选 `console_port`，缺省 5174）后，serve 启动期检测该端口未监听时自动以子进程拉起 console dev server（fail-soft：目录无效/依赖缺失/端口占用仅告警；子进程输出落 `data/console_sidecar.log`）。缺省不配置 = 无任何副作用
- 审批卡交互：agent 触发候选工具审批时，对话侧栏审批卡可直接「批准/拒绝」（走 §4.4 既有审批通道，60s 超时自动拒绝；WS 审批帧两阶段时序见 API.md §12.2）
- **agent 产物协作编辑**：agent `file_write` 写文件成功后，产物自动在编辑器打开并登记（对话侧栏出现「agent 产物」卡，可随时点回编辑器）；编辑器顶部出现产物条——「查看与草稿差异」切 diff 视图（左=agent 草稿基线、右=当前可编辑），直接增删改，`Ctrl+S` 保存即定稿（产物条与产物卡实时显示草稿/已定稿状态与时间）；草稿基线与定稿留痕为工作台展示层记录（localStorage，不入审计链），按会话持久化、切会话自动载入
- 顶栏治理徽标：白名单（agent 工具数）+ 信号（当前会话违规信号累计，见 API.md §12.1），均 fail-soft
- 未构建前端时 serve 自动降级为纯 API 模式，不影响既有用法

### 跑一个 Agent

```bash
curl -X POST http://127.0.0.1:8081/api/agent/run \
  -H "Content-Type: application/json" \
  -d '{"agent_type": "researcher", "goal": "总结当前目录的 README"}'
```

---

## CLI 用法

```text
evo-agent run <goal>                    # 跑 agent（给一个 goal + 可选 agent 类型）
evo-agent list                           # 列出 agents/ 目录下的所有 agent
evo-agent tools list                     # 列出 6 个内置工具（3 层安全模型）
evo-agent tools show <name>              # 显示单个工具的 active/candidate/blocked 详情
evo-agent validate <agent>               # 校验 agent.json 是否合法
evo-agent config                         # 显示合并后的配置
evo-agent serve --port 8081              # 启动 HTTP server（启动期预载校验全部 agent 档案，坏档案 fail-fast）
evo-agent patrol --session <id> --workspace <ws_id>  # 进化巡视（一次性任务，见下文）
evo-agent workflow <workflow_id>         # 执行多 agent DAG 工作流（--plan-execute 走 plan-execute 全链路）
evo-agent repl                           # REPL 交互模式（复用同一 session）
evo-agent replay --session <id>          # 回放 session 的记忆事件链
```

### patrol（进化巡视任务模式）

一次性自进化巡视：**信号探查 → agent 起草 → 闸门一证据组装 → 治理链提名 → 结构化 JSON 巡视报告**。触发器在本进程（可由 cron/运维脚本按需调起），server 侧零自治循环——每次调用只执行一次。

```bash
# 巡视指定会话的进化信号,有信号则自动完成起草与提名(默认 rule-copilot 档案)
evo-agent patrol --session 42 --workspace 01ABC...

# 巡视报告追加写入文件(调度器按时间序列归档)
evo-agent patrol --session 42 --workspace 01ABC... --out patrol-report.jsonl
```

行为语义：

- **无信号**：零动作静默退出（exit 0），报告 `status=no_signal`（一行 JSON 打印到 stdout，供调度器消费）。
- **有信号**：轮A agent 拉取信号明细并起草约束层草稿（rule_create → rule_submit → rule_versions），进程侧组装闸门一沙盒证据（数据集 → 沙盒 → 关闭），轮B agent 携证据调用 `rule_promote` 提名进入人审队列；报告 `status=nominated` 含 `queue_id`。草稿必须是 **enforce 拦截型**（`transform` 条目 `type=enforce`，`params` 必含 `domain` 与非空 `reason`，拦截条件写在 `params.domain` 内，严禁条目级 `condition` 等未知键）——提取后先经提交期结构校验（与 server schema 门禁同口径），失败即 fail-fast 落报告退出（报告 `error` 字段携带校验错误、`draft_json` 留存原始草稿），不带病进入轮B与提交链路。
- **重复提名**：同一目标规则的提名已存在（pending 待审或 published 已发布）时，server 侧双态去重门禁拒绝（409），报告 `status=duplicate_rejected`——提名权仍在人审闭环内，agent 面无审批通道。

### run

```bash
# 用 researcher agent 跑任务
MINIMAX_API_KEY=sk-... \
  evo-agent run "总结一下 README" -a researcher

# 流式输出（token-by-token）
evo-agent run "分析代码结构" -a researcher --stream

# 自动批准 candidate 工具
evo-agent run "执行构建脚本" -a general --auto-approve-candidates
```

### repl

```bash
evo-agent repl -a general
# > 帮我查看当前目录结构
# > /session        # 显示当前 session ID
# > /rewind 3       # 回滚到版本 3
# > /exit
```

### replay

```bash
# 回放全部事件（按时间线）
evo-agent replay --session 123

# 从 E005 沿因果链回溯
evo-agent replay --session 123 --event E005

# 回放某实体的所有事件
evo-agent replay --session 123 --entity pet_doudou

# LLM 自然语言叙述（temperature=0，事实不变）
evo-agent replay --session 123 --narrate
```

### workflow

```json
// rules/workflows/research_and_write.json
{
  "workflow_id": "research_and_write",
  "nodes": [
    { "id": "research", "agent_type": "researcher", "task": "调研 Rust 异步生态", "depends_on": [] },
    { "id": "write", "agent_type": "general", "task_template": "基于调研结果写报告：\n{research}", "depends_on": ["research"] }
  ],
  "output_node": "write"
}
```

```bash
evo-agent workflow research_and_write
```

工作流 DSL 支持 workflow_dag v1.0/v1.1/v1.2 三版本并存（按文档形态自动分派）：
v1.1 节点级条件分支 `run_when`（求值为假跳过，豁免级联）；v1.2 新增有界循环
`loops`（加载时静态展开为线性副本链，展开后仍是纯 DAG）与纯函数节点 `compute`
（封闭目录 `strcmp`/`numeric_cmp`/`regex_match`，不经 LLM、无 IO、无副作用，
典型用法为循环收敛门控：结果与上一轮一致即提前退出）。

执行失败或预算耗尽时由外层驱动循环（`src/agent/driver.rs`）按 replan 判定函数
决定是否触发重规划：丢弃式重规划——失败摘要 + 计划结构喂给 planner 产出下一版
PlanFact，物化后全新执行（已执行结果不注入），replan 硬上限默认 3 次
（`--max-replan`/`--max-wall-ms` 可调）。plan-execute 全链路（planner 先产计划
再执行）加 `--plan-execute`：

```bash
# PlanExecute 模式：载入工作流作为 planning probe（单 planner 节点），
# 其输出解析为 PlanFact v1 → 物化 → 执行
evo-agent workflow research_plan --plan-execute

# Dsl 模式：手写 workflow 即 v1 计划直接执行；失败触发 replan 时
# 由 planner 产出 PlanFact v2 修复重跑（见 rules/workflows/replan_drill.json）
evo-agent workflow replan_drill

# D-01 enforce 演练：节点 model 非白名单 → TCB 门产生 Violation →
# 驱动判别后终止整个循环且不 replan（宪法违规一票否决，§9.5.1-B）
evo-agent workflow enforce_drill

# replan 硬上限防抖演练：合规节点成功后接必败节点，--max-replan 0
# 使判定序第 1 步（硬上限）直接终止并显式传播错误
evo-agent workflow ok_then_fail_drill --max-replan 0
```

执行成功时输出统计行（Phase 2 起含成本埋点）：

```text
=== workflow 'research_plan' done (plan_versions=1 replans=0 nodes_executed=3
wall_ms=81234 repeated_nodes=0 tokens_used=18745 replan_tokens=0) ===
```

- `repeated_nodes`：replan 产物中与已执行节点（同 id 同 agent_type）的幂等重复
  计数——warn 放行（丢弃式接受重复成本）；PlanFact 中声明 `file_write`/`shell_exec`
  写类工具的节点则直接拒绝提交（防御深度二道闸）
- `tokens_used`：全程 LLM token 消耗（provider `usage` 汇总，仅供观测）
- `replan_tokens`：v2+ 各版执行 + replan planner 调用自身消耗（重复执行成本上界，
  为增量式 replan 策略积累判定数据）

planner 产出的 PlanFact 若无法解析为 JSON，驱动会带提取错误反馈重试 1 次
（全程硬上限 2 次 planner 调用），仍失败则整体终止，不猜测不静默。

---

## Agent 定义

Agent 配置从 `agents/{type}.json` 加载：

```json
{
  "agent_type": "researcher",
  "version": "0.1.0",
  "description": "研究型 Agent",
  "system_prompt": "You are a careful research assistant.",
  "model": "MiniMax-M2.5",
  "temperature": 0.3,
  "max_steps": 20,
  "step_timeout_secs": 60,
  "tools": ["file_read", "search_files", "file_list"],
  "memory": {
    "type": "persistent",
    "namespace": "researcher",
    "message_persist": { "mode": "every_message" },
    "max_session_summaries": 3,
    "max_injected_events": 5,
    "enable_event_extraction": true
  },
  "context_window_tokens": 8192,
  "parallel_tools": 1
}
```

### 内置 Agent

| Agent | 说明 | 工具 |
|-------|------|------|
| `general` | 通用 Agent — 文件操作 + Shell + Web + 规则消费与起草（白名单 20 工具，含 `evolution_signals` 只读信号感知） | file_*, search_files, shell_exec, http_get, rule_list/get/versions/version_get/validate/create/update/submit, knowledge_*, audit_get, meta_summary, evolution_signals |
| `researcher` | 研究 Agent — 只读搜索 | file_read, search_files, file_list |
| `rule-copilot` | 规则协作 Agent — 23 个规则管理工具（白名单） | ws_*, rule_*, audit_*, translate_*, sandbox_*, dataset_*, publish_*, knowledge_*, meta_*, evolution_signals, rule_promote |

---

## 记忆系统

### 三层记忆

| 层级 | 路径格式 | 用途 | 生命周期 |
|------|----------|------|----------|
| 共享记忆 | `shared.{ns}.{key}` | 跨会话共享知识 | 永久（WAL 持久化） |
| 会话记忆 | `{ns}.sessions.{sid}.{key}` | 单会话私有状态 | 会话期间 |
| 短期消息 | `{ns}.sessions.{sid}.messages.{idx}` | 对话历史 | 会话期间 |

所有记忆通过 `POST /api/sessions/{id}/payload` 写入 evorule，记忆本身就是 Fact，可回放、可审计。

### 跨会话共享事实

当 session 写入 `shared.*` 路径的 PayloadUpdate 时，evorule-server 同步广播到 `SharedFactsLog`：

- **WAL 持久化**：重启后自动恢复历史共享事实 + 元数据
- **Rollup 标记**：已合并的旧事实从 prefix 查询中过滤，但 `fact_by_id` 仍可访问（审计可追溯）
- **来源追踪**：每条共享事实记录 `source_session_id`

### 记忆事件链

会话运行期间，`EventExtractor` 从对话中提取结构化事件：

- 实体（Entity）：人物、地点、物品等
- 事件（Event）：带因果链的 `cause_fact_id` 锚点
- 叙事（Narrative）：LLM 生成的自然语言描述

会话结束时通过 `replay` 命令回放，支持按事件 ID、实体、方向（backward/forward）筛选。

### 会话沉淀

会话结束（Stable/Error 分支）时自动完成三级沉淀：

1. **当前级**：messages 已由 `MessagePersistMode` 在运行时逐条写入
2. **中期级**：整会话摘要写入 `shared.{ns}.sessions.{sid}.summary`
3. **长期级**：稳定事实写入 `shared.{ns}.stable.{key}`

---

## 工具系统

### 3 层安全模型

| 层级 | 行为 | 示例 |
|------|------|------|
| **ACTIVE** | 直接执行，无需审批 | file_read, file_list, file_write, search_files, shell_exec（8 命令白名单）, http_get（6 主机白名单） |
| **CANDIDATE** | LLM 想用 → 返回 proposal → 用户审批 → 执行 | rm, mv, curl 等 20+ shell 命令，任意公开 HTTP host |
| **BLOCKED** | 永不批准 | sudo, python, bash 等 28+ 逃逸命令，SSRF 黑名单 IP 段 |

### 内置工具（6 个）

| 工具 | 说明 |
|------|------|
| `file_read` | 读取文件（工作目录沙箱，拒绝 `..` / 绝对路径 / symlink 逃逸） |
| `file_list` | 列出目录内容 |
| `file_write` | 写入文件 |
| `search_files` | 按内容搜索文件 |
| `shell_exec` | 执行 Shell 命令（白名单 + candidate 审批） |
| `http_get` | HTTP GET 请求（主机白名单 + SSRF 防护） |

### 规则管理工具集（45 个）

通过 `rule_management_toolkit` / `full_rule_toolkit` 组装，用于 `rule-copilot` Agent：

| 类别 | 工具数 | 说明 |
|------|--------|------|
| workspace | 2 | ws_list, ws_create |
| rule | 12 | rule_list, rule_get, rule_create, rule_update, rule_versions, rule_version_get, rule_submit, rule_activate, rule_block, rule_archive, rule_fork, rule_reload |
| translate | 3 | rule_to_transform, rule_to_conditional, rule_validate |
| audit | 3 | audit_get, audit_verify, session_rewind |
| sandbox | 5 | 沙盒编排（fork + 合成数据 + 测试报告） |
| dataset | 2 | 数据集管理 |
| publish | 5 | 发布队列 + 三级权限 |
| production | 2 | 生产环境管理 |
| bundles | 5 | 规则包导入/列出/回滚 |
| knowledge | 3 | 知识库检索 |
| meta | 1 | meta_summary（L2 约束清单摘要） |
| evolution | 2 | evolution_signals（进化信号拉取）+ rule_promote（约束层晋升提名） |

### MCP 工具接入

通过 MCP 客户端接入外部工具生态：

```toml
# evo-agent.toml
[[mcp.servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
```

MCP 工具自动注册为 `mcp_{server}_{tool}` 前缀，纳入 ToolHandler 统一管理。

---

## 配置

4 层配置加载（优先级低 → 高，后者覆盖前者）：

1. **默认值** — 代码中 `Config::default()`
2. **用户配置** — `~/.config/evo-agent/config.toml`（Linux/macOS）或 `%APPDATA%\evo-agent\config.toml`（Windows）
3. **项目配置** — `./evo-agent.toml`
4. **环境变量** — `EVO_AGENT_*` 前缀，`__` 分隔 section/field

```toml
# evo-agent.toml 示例
[llm]
provider = "minimax"
api_key = "${ENV:MINIMAX_API_KEY}"
model = "MiniMax-M2.5"
api_base = "https://api.minimax.io/v1/text/chatcompletion_v2"
timeout_secs = 30
max_retries = 3
context_window_tokens = 8192

[evorule]
base_url = "http://127.0.0.1:18080"

[agents]
dir = "./agents"
default = "general"

[serve]
host = "127.0.0.1"
port = 8081
```

`api_key` 字段支持 `${ENV:VAR_NAME}` 占位符，加载时展开为环境变量值。

---

## HTTP API

### Evo-Agent 自有 API

| 方法 | 路径 | 说明 |
|------|------|------|
| `GET` | `/api/agent/list` | 列出所有已注册 Agent 类型 |
| `GET` | `/api/agent/{type}` | 查看指定 Agent 详细定义 |
| `POST` | `/api/agent/run` | 启动一个 Agent 运行 |
| `GET` | `/api/health` | 健康检查 |

### 通过 ApiCore 透传到 evorule-server

两个 client（`EvoruleApiClient` + `WorkspaceApiClient`）共享 `ApiCore`（base_url + reqwest Client + Bearer auth），统一错误为 `ApiError`。

认证 token 构造时读取：`EVORULE_SERVICE_TOKEN`（service 身份，可写受保护域 `stable.llm`/`stable.system`）优先，缺省回退 `EVORULE_AUTH_TOKEN`（user 身份）；均缺失时不发 auth header（server 须为 dev mode）。

---

## 接入真实 LLM

实现 `LlmHandler` trait 即可接入任意 LLM provider：

```rust
use evo_agent::io_handlers::LlmHandler;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct OpenAIHandler {
    api_key: String,
    base_url: String,
}

#[async_trait]
impl LlmHandler for OpenAIHandler {
    async fn call(
        &self,
        model: &str,
        system_prompt: &str,
        messages: Vec<Value>,
        tools: Vec<Value>,
    ) -> Result<Value, String> {
        let client = reqwest::Client::new();
        let body = json!({
            "model": model,
            "messages": [
                { "role": "system", "content": system_prompt },
                ...messages,
            ],
            "tools": tools,
        });
        let resp = client
            .post(&format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json::<Value>()
            .await
            .map_err(|e| e.to_string())?;
        Ok(resp)
    }
}
```

内置支持 OpenAI 兼容 API（MiniMax / DeepSeek / OpenAI），通过 `LlmConfig` 配置 provider / api_key / model / api_base。

---

## 测试

```bash
# 单元 + 集成测试
cargo test --workspace

# 只跑集成（mockito mock evorule-server）
cargo test --test integration_test

# 真实 evorule-server 端到端
# 1. 启动 evorule-server
# 2. cargo test -- --ignored --test-threads=1
```

集成测试用 `mockito` mock evorule-server，覆盖：
- `auto_recall`（启动时拉取 shared facts）
- ReAct 主循环的 SSE 事件驱动
- 工具调用闭环
- 错误处理路径

---

## 本地运维：服务稳定化（看门狗 + 开机自启）

evorule 生态本地开发涉及三个常驻服务：evorule-server（18080）、evo-agent serve（8081）、console 审计页 dev server（5174）。`scripts/ops/` 提供一套与仓库一同演进的运维脚本族，解决四类常见不稳定：进程挂在终端/会话后台作业下（会话结束服务陪葬）、关终端或重启电脑后服务消失、进程崩溃无人拉起、重启时漏带 LLM 密钥导致假性「LLM 失联」。

| 文件 | 作用 |
|------|------|
| `_common.ps1` | 共享工具：配置加载 / HTTP+TCP 探活 / 轮询等待 / 分离启动 / 日志 |
| `start-evorule-server.ps1` | 幂等拉起 evorule-server（探活通过即跳过） |
| `start-evo-agent-serve.ps1` | 幂等拉起 serve，自动注入 `.env` 环境变量（LLM 密钥等，日志只回显键名不回显值） |
| `start-console.ps1` | 幂等拉起 console dev server |
| `watchdog-check.ps1` | 单次巡检：三服务探活，不通则拉起（互斥锁防重叠）；手动运行即为「一键全启」 |
| `install-watchdog-task.ps1` | 注册 Windows 计划任务 `EvoruleOpsWatchdog`（用户登录触发 + 每 1 小时巡检自愈） |
| `ops.local.example.json` | 本机配置模板（入库，中性路径示例） |

### 首次启用

```powershell
# 1. 复制模板为本机配置（已被 .gitignore 忽略，不入库），按本机实际路径修改 exe/args/workdir/env_file
Copy-Item scripts\ops\ops.local.example.json scripts\ops\ops.local.json

# 2. 注册计划任务（登录自启 + 每分钟巡检；重复注册用 -Force 覆盖，卸载见下）
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\ops\install-watchdog-task.ps1

# 3. 卸载看门狗
Unregister-ScheduledTask -TaskName EvoruleOpsWatchdog -Confirm:$false
```

设计要点：
- **分离启动**：服务以脱离调用方的独立进程运行（隐藏窗口），不再挂在终端/会话后台作业下
- **幂等**：所有启动脚本探活通过即跳过，可随时手动重跑；`watchdog-check.ps1` 即「一键全启」
- **密钥双保险**：serve 自身启动时自动加载 workdir（或 exe 目录）下的 `.env`（已设置的环境变量优先，不覆盖；`src/dotenv.rs` 零依赖最小实现），密钥全缺失时启动期打印醒目 WARNING——即使绕过运维脚本裸启动也不会「LLM 失联」；脚本侧 `.env` 注入作为第二层保障
- **密钥安全**：`.env` 注入只回显键名；`ops.local.json` 与日志目录均在 gitignore 内，密钥不进 git
- **日志**：每次启动的 stdout/stderr 落 `log_dir`（上一份轮转为 `*.prev`），巡检动作落 `watchdog.log`
- **边界**：纯运维层工具，不触碰 evorule 引擎执行面（哈希链 / Fact / 审计语义零依赖）

---

## 目录结构

```
evo-agent/
├── Cargo.toml
├── README.md
├── CHANGELOG.md
├── LICENSE
├── NOTICE.md                        # 许可与依赖声明
├── verify.ps1                       # 一键验证（build/test/防泄漏/布局断言）
├── agents/                          # Agent 定义
│   ├── general.json
│   ├── researcher.json
│   └── rule-copilot.json
├── config-examples/                 # 配置示例
├── docs/
│   ├── API.md
│   ├── RELEASE_PROCESS.md           # 发布流程
│   └── security/                    # 安全设计文档
├── src/
│   ├── lib.rs                       # 入口 + 公共契约面登记
│   ├── config.rs                    # 4 层配置加载
│   ├── json_convert.rs              # serde ↔ tcb::JsonValue 转换
│   ├── io_handler.rs                # I/O handler 基类 trait
│   ├── io_dispatcher.rs             # I/O 分发器
│   ├── agent/
│   │   ├── runner.rs                # ReAct 主循环
│   │   ├── audited_llm.rs           # 审计链内 LLM 执行桥（sidecar 会话协议）
│   │   ├── memory.rs                # 三层记忆管理
│   │   ├── memory_event/            # 结构化记忆事件 + 因果链 + 回放
│   │   │   ├── entity.rs            #   实体定义
│   │   │   ├── event.rs             #   事件定义
│   │   │   ├── evidence.rs          #   证据伴随
│   │   │   ├── extraction.rs        #   事件提取
│   │   │   ├── replay.rs            #   确定性回放
│   │   │   └── store.rs             #   事件存储
│   │   ├── definition.rs            # Agent 配置加载
│   │   ├── tool_registry.rs         # 工具注册中心
│   │   ├── translator.rs            # LLM 响应解析
│   │   ├── delegate.rs              # Agent 嵌套上下文
│   │   ├── workflow.rs              # DAG 工作流引擎(compute 纯函数节点内联求值)
│   │   ├── materializer.rs          # workflow_dag v1.2 物化器(loop 静态展开)
│   │   ├── replan.rs                # replan 触发判定纯函数 + 预算结构
│   │   ├── driver.rs                # plan-execute 外层驱动循环(计划→执行→replan 重跑)
│   │   ├── context_window.rs        # 上下文窗口裁剪
│   │   ├── summarizer.rs            # 会话摘要
│   │   ├── sediment.rs              # 会话沉淀通道
│   │   ├── approval.rs              # 工具审批系统
│   │   ├── callback.rs              # 事件回调链
│   │   ├── output_validator.rs      # JSON Schema 输出校验
│   │   └── mod.rs
│   ├── api/
│   │   ├── api_core.rs              # 共享 HTTP 基建（ApiCore + ApiError）
│   │   ├── evorule_client.rs        # evorule-server 端点客户端
│   │   ├── workspace_client.rs      # workspace 服务客户端
│   │   ├── agent_api.rs             # /api/agent/* 路由
│   │   ├── serve_tools.rs           # serve 模式工具注册
│   │   ├── ws_handler.rs            # WebSocket 双向流
│   │   ├── auth.rs                  # Bearer 认证
│   │   ├── metrics.rs               # Prometheus 指标
│   │   └── mod.rs
│   ├── builtin_tools/               # 6 个内置工具
│   │   ├── file_read.rs
│   │   ├── file_list.rs
│   │   ├── file_write.rs
│   │   ├── search_files.rs
│   │   ├── shell_exec.rs
│   │   ├── http_get.rs
│   │   ├── delegate_tool.rs         # Agent 委托工具
│   │   └── mod.rs
│   ├── rule_tools/                  # 45 个规则管理工具
│   │   ├── workspace_tools.rs       #   workspace 2 个
│   │   ├── rule_tools.rs            #   rule CRUD 12 个
│   │   ├── translate_tools.rs       #   规则转换 3 个
│   │   ├── audit_tools.rs           #   审计 3 个
│   │   ├── sandbox_tools.rs         #   沙盒 5 个
│   │   ├── dataset_tools.rs         #   数据集 2 个
│   │   ├── publish_tools.rs         #   发布 5 个
│   │   ├── production_tools.rs      #   生产 2 个
│   │   └── mod.rs
│   ├── mcp/                         # MCP 客户端
│   │   ├── client.rs                #   JSON-RPC 2.0 客户端
│   │   ├── transport.rs             #   stdio 传输
│   │   ├── tool_adapter.rs          #   MCP → ToolFunction 适配
│   │   └── mod.rs
│   ├── io_handlers/                 # LLM/Tool 抽象层
│   │   ├── llm_handler.rs
│   │   ├── tool_handler.rs
│   │   └── mod.rs
│   └── bin/
│       └── evo-agent.rs             # CLI 入口
├── scripts/
│   └── ops/                         # 本地运维脚本族（看门狗/自启/一键拉起，见「本地运维」章节）
└── tests/
    ├── integration_test.rs          # mockito 端到端测试
    └── llm_real_smoke.py            # 真实 LLM 冒烟脚本
```

---

## 当前状态 / 已知限制

基于 2026-08 完成度核查：

**已就绪（曾被报告误判为"未修"的项）**

| 项 | 状态 | 说明 |
|----|------|------|
| 共享事实广播（原 L-1） | ✅ | evorule-server `session_payload` handler 已实现 `shared.*` → `SharedFactsLog::append` 广播 |
| rollup 标记（原 L-3） | ✅ | evo-agent `mark_shared_facts_rollup` + server 端点均就绪，`facts_by_path_prefix` 过滤 `rolled_up` |
| 共享路径格式（L-2）/ 上下文窗口字段（L-4）/ sediment 写入前缀（L-6） | ✅ | evo-agent 侧均已修复，与 recall 三前缀完全匹配 |
| 记忆召回顺序（C2） | ✅ | `runner.rs` 已修复：recall 在 `build_system_prompt` 之前 |
| 角色 1/3 `call_external` | ✅ | evorule-server 侧就绪，挂载 `service_registry.json` + `--allow-loopback` 即可跑通（已端到端验证） |

**仍未完成 / 设计取舍**

| 项 | 状态 | 说明 |
|----|------|------|
| 集群协作（E2 / cluster） | ❌ 设计移除 | 多 reactor 协作原语已移出机制层，定位为应用层功能；evorule-server 路由已无 cluster 端点 |
| Runner 拆分（Phase 2） | ⏳ | `runner.rs` 仍为约 2600 行单文件，未拆为子模块 |
| UI 联调 | ⏳ | 无前端联调，本轮仅后端 + CLI 验证 |
| 编译告警 | ⚠️ | 主体为 `missing_docs`；另有少量 clippy 代码质量 lint 待清理 |

> 规则管理工具集总数为 **45 个**（workspace 2 + rule 12 + translate 3 + audit 3 + sandbox 5 + dataset 2 + publish 5 + production 2 + bundles 5 + knowledge 3 + meta 1 + evolution 2），上文[核心特性](#核心特性)与[工具系统](#工具系统)的拆分表已据实校正。

## 依赖关系

```toml
reqwest = "0.12"              # HTTP 客户端
axum = "0.8"                  # HTTP 服务（含 WebSocket）
tokio = "1"                   # 异步运行时
serde / serde_json = "1"      # JSON 序列化
clap = "4"                    # CLI 参数解析
tracing = "0.1"               # 结构化日志
prometheus = "0.13"           # 指标
jsonschema = "0.18"           # JSON Schema 校验
rustyline = "14"              # REPL 行编辑
```

依赖面仅第三方 crates（自 O-044 起，2026-09-20，不含任何 `evorule-*` 依赖）；
与 evorule-server 之间只有 HTTP/WS 协议契约。

**零 unsafe**：`#![forbid(unsafe_code)]` 在所有 module 强制。

---

## 相关项目

- [evorule](https://gitee.com/evorule/evorule) — 反应式执行引擎（tier0/tier1/tier2）
- [evorule-server](https://gitee.com/evorule/evorule-server) — HTTP 服务 + Workspace + 沙盒 + 发布队列

---

## License

[AGPL-3.0](LICENSE) — EvoRule dual-license: closed-source use via [DUAL_LICENSE.md](DUAL_LICENSE.md) / [FREE_COMMERCIAL_LICENSE.md](FREE_COMMERCIAL_LICENSE.md) (free for eligible entities) / [COMMERCIAL_LICENSE.md](COMMERCIAL_LICENSE.md) (paid). Constitution `core_eval.json` is CC0-1.0. Commercial inquiries: evorulelab@gmail.com.


