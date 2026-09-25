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
- **REST run/stream 会话进工作台索引（O-125 修复：run 会话不可见）+ 响应携带 `session_id`（O-086 收口）** — `consume_to_done` 增捕获流中 `SessionCreated` 会话 ID；`POST /agents/{type}/run` 完成后与 `run/stream` SSE 的 `SessionCreated` 臂均按 WS 面同口径挂本地会话索引（title=goal 截 60 字符，record fail-soft）；`AgentRunResponse` 增可选 `session_id` 字段（`skip_serializing_if` 省略 None，旧消费者零影响）——消费者凭此直查 18080 权威面或工作台回放。真实 MiniMax E2E 十二断言全 PASS（run 会话与 stream 会话均入 8081 索引、title 截断、transcript 投影）；证据 `D:\knowledge\audits\evidence\o125o123-e2e-20260925\`
- **运行体身份查询端点 `GET /version`（O-116 收口：版本查询接口工作台可见）** — serve 新增只读元信息端点（鉴权内，workdir 绝对路径不出无鉴权面），返回 `{version, exe_mtime_epoch, workdir}`；采集逻辑上移为 `serve_tools::runtime_identity` 共享正本——`cmd_serve` 启动横幅与 HTTP 接口单一事实源（消除横幅内联实现的双源漂移），排障时核对「运行中的 exe」与「源码 HEAD」一致性不再留盲区。O-120 批已落启动横幅（`fd785ea`），本批收口接口面；E2E 实测接口三字段与横幅一致
- **REST run 端点工具执行回路（O-124 修复：工具能力面假象消解）** — `POST /agents/{type}/run` 由非流式 `run()` 单发桥接（LLM 返 tool_calls 即返、工具不执行，实测 s113 `success=true + content=""`）改为消费 `run_streaming` 至 `Done`（O-114 delegate 同款修法 `bb172b2` 先例）：多轮工具回喂在服务端真实执行完毕后聚合返回，对外 `AgentRunResponse` 结构零变化。聚合逻辑提取为 `consume_to_done` 纯函数（流式终止契约「正常/LLM 中断/rewind 失败/max_steps 超限均以 Done 收尾」权威结果 + Err 项/无 Done 防御性失败组装，单测 4 条）；approval 不注入（candidate 缺省拒绝，与 delegate 同款）。真实 MiniMax E2E 十三断言全 PASS：1952 字符真实 README 总结、file_read 调用与回喂经 18080 state 落链、M5-c 意图裁决伴生会话顺带实证裁决通道在 run 路径工作。证据 `D:\knowledge\audits\evidence\o124-e2e-20260925\`
- **ARCHITECTURE §9 自有 API 清单重写（O-126 顺手收口：文档漂移）** — 原清单仍列早期 `/api/agent/*` 3 端点假想形态，与现行 20+ 路由面严重脱节；重写为现行分组概览（运维/凭据可视化/Agent 执行/会话与对话/工作台/文件面，含 `/version`），并注明会话链上权威与本地索引覆盖边界（O-125）
- **规范入口索引感知段（BACKLOG M1：知识层「什么问题查什么档」）** — serve 三路径（WS `construct_runner` / HTTP `run_agent` / `run_agent_stream`）构造 runner 时在 system_prompt 尾部全量注入【规范入口索引】段（`apply_regulation_index_awareness`，`serve_tools.rs`，静态 const 文本、无网络调用、无触发条件——规范程序是所有 serve 会话的通用素养）。段内容 = `INDEX-规范入口-给agent.md` v1.0 的产品端投影，三原则：**速查自足**（能力缺口三问全文+【三问留痕】格式在段内，agent 不依赖外档即可执行）、**知识地图**（宪法/design/登记册等规范域索引）、**边界如实**（规范原档在宿主侧知识库、沙箱内不可达，如实声明请宿主提供——对齐 M5-a「不猜测不编造」）。第一案 P1/P2/P3 三 FAIL 全翻转：真实 MiniMax WS E2E 九断言全 PASS（agent 逐字引用索引段、三问实质执行、完整【三问留痕】留痕）。范围裁决：仅 serve 三路径；CLI/driver/delegate 不注入（token 形态零变化）。权威源同步纪律：索引档变更必须同步本投影（serve_tools.rs `REGULATION_INDEX_AWARENESS_SEGMENT`）。证据 `D:\knowledge\audits\evidence\m1-regindex-e2e-20260925\`
- **协作验收规则三哨兵（M5-c 全局观机制第三刀：约束层 enforce）** — `00_constraint` 治理集三条 enforce 规则经 design-09 治理链发布（`rules/governance/00_constraint_collab_acceptance.json` v1.0.1）：R1 边界强制（`meta_tool.pending_target_scope=out_of_sandbox` → Halted）、R2 实施前置（workflow `meta_workflow.phase=implementation` 要求 `exists(payload.meta_task.due_diligence_done)`）、R3 核收前置（`phase=closure` 要求 `exists(payload.meta_task.implemented)`）。机制层配合两处：①runner 对 file 类工具调用先解析目标落点为规范字段 `meta_tool.pending_target_scope`（意图裁决规则化，机制层内联沙箱检查保留为兜底防线，双层各司其职）；②workflow 引擎对每个 LLM 节点 delegate 前向标记会话提交阶段信号 `set meta_workflow.phase=<node_id>`、节点成功后即时提交完成信号打标（`mark_node_done`，门/打标严格配对），driver drain 保留幂等兜底。域谓词状态路径遵循 exec 相对路径约定（`payload.*`/`instruction.*` 自动补 `__exec__.` 前缀）。真实 LLM E2E 四象限：越界企图/未尽责调均拦截且有 Violation 归因（rule_index+reason），合规链路逐级放行，误伤=0/漏放=0；`EVORULE_DISCIPLINE_GATE` 维持缺省 warn（flip enforce 另批）
- **workflow 任务标记（M5-b 全局观机制第二刀：协作留痕通道）** — `workflow` 执行时驱动创建标记会话（`initial_content={"kind":"workflow_run","workflow_id":...}`），每个节点执行成功后向其提交中性完成信号 `set meta_signal.node_done=<node_id>`（`run_plan_loop` 增参 `marks_session: Option<String>`，None=既有行为零变更；信号提交失败 fail-fast——留痕是硬义务）。信号不含任务语义，「节点→标记」裁决完全在规则层：新增业务规则资产 `rules/governance/collab_task_marks.json`（rule_set v1.0，tier="business"，三条 branch 壳+set：`due_diligence→meta_task.due_diligence_done`、`implementation→meta_task.implemented`、`closure→meta_task.closed`；部署到 server 规则目录生效），机制层生产信号、规则层裁决标记，改协作纪律=改规则零发版。新增示例工作流 `rules/workflows/collab_dd_impl_close.json`（尽调→实施→核收三节点线性协作，workflow_dag v1.2）。真实 LLM E2E：三节点走通（tokens_used=1036）+ 标记会话链上三标记全真（branch 壳条件触发与 on_true set 落链均实测确认）
- **能力边界声明 `capability_boundary`(M5-a 全局观机制,agent_def v1.1 增量字段)** — agent.json 可声明 `{mode: "read_only"|"read_write", sandbox_root, tools}`,成为 file 类工具沙箱检查的单一事实源;未声明时按启动配置合成缺省边界(行为与既往完全一致)。声明生效后:①会话建立时在 system_prompt 尾部注入系统级边界段(LLM 自知边界,越界请求可自述边界而非误报「文件不存在」);②边界事实经 create_session 的 initial_content 载体进链(零新 Fact 类型);③file_read/file_write 越界错误改为回报「不可访问 + 边界路径」。语义门卫:mode 取值白名单/sandbox_root 必须为绝对路径/顶层 tools 中的沙箱类工具必须列于 boundary.tools/read_only 不得授予 file_write。Schema 权威:evorule-system-rules `agent_def/v1.1.json`(evorule-constitution 0.3.1,rev pin 同步 bump)
- **`workflow` 子命令新增 `--max-tokens <N>` flag** — token 预算阈值接线：累计 `tokens_used` 达到 N 即触发 replan Budget 分支（交付物 7 tokens 埋点的阈值消费闭环）；不指定 = 不限（缺省 None 维度不参与判定，行为不变）。`DriverLimits`/`BudgetThresholds.max_tokens` 逐版接线（driver.rs 阈值重算处）
- **D-01 二次保险 + 降级兜底（runner.rs）** — SSE 流关闭路径新增 enforce 兜底：断流可能吞掉 `Violation` 帧（违规表现为「静默成功后流关闭」），流关闭时 best-effort 查 evolution-signals，`total_violations > 0` 即以 `enforce violation: rule_ref=..., reason=...` 固定前缀上抛（归因取链上最新违规信号 `last_version` 最大者；workflow 层凭前缀判别终止不 replan）；兜底查询不可用（网络断/会话被 TTL 收割/server 不可达）时**降级**——warn 留痕 + 返回携带本地上下文的原流关闭错误，不掩盖不阻塞不重试。同批补齐**流式消费面（REPL/工作台 `run_streaming`）的 `Violation` 分支**（此前流式路径违规事件落 `_` 静默丢弃）
- **R2-T04 链体积监控（runner.rs）** — 长 会话 facts_log 体积增长观测告警：workflow 链与流式两消费面的 `Stable` 收尾路径 best-effort 查审计报告 `entry_count`（BLAKE3 审计链长 = 链体积权威只读投影），达到 `CHAIN_SIZE_WARN_ENTRIES`（10,000 条）warn 告警、低于阈值 debug 观测；查询失败静默降级。只读观测不干预执行——不写链、不拦截、不改变控制流（零红线风险）
- **R3-T03 replan 链上因果断言（E2E 场景 B 增强）** — 运行前后 `GET /api/sessions` 列表 diff 圈定新建会话，从 server 链上唯一推断 replan 因果并断言四条：①恰一个 v2 planner 会话且链上 IoRequest 含 v1 失败摘要（ghost_agent）；②链上 IoResponse.content 提取出 PlanFact JSON；③PlanFact 结构合法且已修复失败（无 ghost_agent）；④裸 PlanFact 不含注入组字段（plan_source/plan_version/parent_plan_hash 为驱动内存态由外层权威注入——注入正确性由 driver UT 覆盖）；planner 会话全链证据落盘 evidence_dir
- **R6-T01 性能测试 PT（`#[ignore]` 手动跑，不进 CI）** — 两个可观测基线：①`pt_materialize_512_boundary_timing`（materializer.rs）：512 节点上限展开 ×10 次计时（实测 avg ≈ 7.6ms），防物化复杂度回归；②`pt_compute_only_512_node_chain_timing`（workflow.rs）：512 节点 compute-only 最深单链执行计时（实测 ≈ 58ms，strcmp 引用前驱的最压迫拓扑，compute 不经 delegate 确定性可重复），防执行层复杂度回归。手动跑：`cargo test -p evo-agent --lib pt_ -- --ignored --nocapture`
- **R1-T03 IT 真实化：planner 重试链路 E2E 场景 E（tests/e2e_plan_execute.py）** — 真实 LLM「非法 JSON→错误反馈→重试成功」全链路：`retry_drill` 演练工作流（planner 节点 task 含 `__FLAKY_FIRST__` marker）触发 planner 按条件协议首答非法 JSON（固定回复 NOT_JSON_YET）→ `call_planner_with_retry` 提取失败 → 原任务附 IMPORTANT 错误反馈重试 → 反馈分支输出合法 PlanFact → v1 物化执行成功。断言分两层：进程层（EXIT=0、`plan v1 materialized`、`plan_versions=1 replans=0`、tokens_used 含两次 planner 调用）+ 链上层（恰两个 marker planner 会话：首调 P1 的 IoResponse 提取不出 PlanFact、重试 P2 的 IoRequest 用户消息含 driver 固定反馈文案、P2 产物结构合法且无 ghost）。planner.json 增补响应条件协议（IMPORTANT 反馈 > FLAKY marker > 标准规则，按序求值；无 marker 的标准任务走原路径零行为变化，场景 A/B/D 回归验证 PASS）；脚本新增 `--only <场景>` 单场景调试参数

#### plan-execute Phase 2：enforce 判别 / planner 重试 / 静态拦截 / 成本埋点
- **D-01 enforce 一票否决（runner.rs + driver.rs）** — `AgentRunner` 事件循环新增 `Violation` 分支：TCB 约束前置门拒绝违规指令时，runner 不 rewind 不重试，flush/sediment 后以固定前缀 `enforce violation: rule_index=..., reason=...` 上抛 `AgentResult::error`；外层驱动 `is_enforce_violation` 凭前缀判别后**终止整个循环且不 replan**（纲领 §9.5.1 选项 B——宪法违规是系统性错误，不开「换计划再试」通道）。 Violation 消费零新增 Fact 类型（消费既有 SSE `Violation` 事件的 `rule_index`/`reason` 字段）
- **planner 重试面（driver.rs，R1-T03/T04）** — `call_planner_with_retry`：PlanFact JSON 提取失败 → 原任务附提取错误反馈重试 1 次（全程硬上限 2 次 planner 调用），仍失败整体终止；probe 站点复用已产出的 probe 输出（`Some(first_output)` 不重复调用），replan 站点传 `None` 函数内先补首次调用
- **静态拦截 R8-T03（driver.rs）** — v(n+1) 物化后提交前双重闸：① `find_non_idempotent_writes` 递归扫描 PlanFact（含 loops body 嵌套），声明 `file_write`/`shell_exec` 写类工具的 tool 节点即**拒绝提交**（schema J3 幂等读白名单之外的防御深度二道闸）；② `detect_repeated_nodes` 与已执行注册表（跨版本累积 `(node_id, agent_type)`，workflow.rs `take_executed_node_ids` drain 语义）比对，幂等重复 warn + 计数放行（丢弃式 D-02 接受重复执行成本，§9.4.2）
- **计划体检面 M3（materializer.rs）** — `find_orphan_computes`：孤立 compute 节点（id 不在任何 `depends_on` 且非 `output_node`）物化时 warn 非拒载（交付物 4 §5.3 建议形态，步骤 5.5）
- **replan 成本埋点（交付物 7）** — tokens 埋点链路贯通：llm_handler 解析的 provider `usage.total_tokens` → runner `IoRequest` 提交后经共享 `Arc<AtomicU64>` 累加（`DelegateContext::with_token_counter` 透传，随 Clone 延续到每个子 runner）→ driver 维护 `tokens_used`（总量）/`replan_tokens`（v2+ 各版执行差值 + replan planner 调用自身消耗）/`repeated_nodes` 三计数；统计行扩展为 `plan_versions/replans/nodes_executed/wall_ms/repeated_nodes/tokens_used/replan_tokens`。阈值判定 `max_tokens` 维度结构就绪（缺省 None 不参与，不改变控制流）
- **D-01 演练素材** — `agents/probe_violator.json`（model 非白名单，指令必被 TCB 门拦截）、`rules/workflows/enforce_drill.json`（Dsl 单 violator 节点，验证终止不 replan）、`rules/workflows/ok_then_fail_drill.json`（合规节点后接 ghost_agent，配 `--max-replan 0` 验证硬上限判定序第 1 步终止）
- **E2E 扩至四场景（tests/e2e_plan_execute.py）** — 新增场景 C（enforce 终止：EXIT=1 + `halted by enforce violation` + 全程无 replan）与场景 D（硬上限防抖：EXIT=1 + `replan budget exhausted`）；场景 A 补断言 `tokens_used>0`、场景 B 补断言 `replan_tokens>0`（交付物 7 真实生效证据）；统计行正则同步 7 字段

#### plan-execute 外层驱动循环与 planner 节点（Phase 1-B）
- **外层驱动循环（`src/agent/driver.rs`）** — `run_plan_loop` 编排「计划 → 执行 → replan 重跑」主循环，双模式：**Dsl**（手写 workflow 即 v1 计划直接执行，失败/预算触发 replan）与 **PlanExecute**（`--plan-execute`：载入工作流作为 planning probe 单 planner 节点 DAG，其输出解析为 PlanFact v1 → 注入元数据 → 物化 → 执行）。replan 为丢弃式（纲领 D-02）：v(n) 失败/预算耗尽 → 失败摘要 + 计划结构构造 replan 任务 → planner（走 delegate 既有路径，IoRequest sidecar 入链，零新增审计通道）产 PlanFact v(n+1) → 物化 → 全新 execute（已执行结果不注入）；`should_replan` 到硬上限仍 Err 时显式传播失败（含版本/replan 统计）不静默。注入组（`plan_source`/`plan_version`/`parent_plan_hash`）外层权威注入（LLM 不产出）；`parent_plan_hash` = BLAKE3 64-hex，锚 = 注入后 PlanFact canonical JSON（Dsl v1 锚 = workflow 文件原文 hash，seed_hash 传入）
- **`workflow` 子命令 CLI 扩展** — 新增 `--plan-execute`（bool）、`--max-replan`（默认 3）、`--max-wall-ms`（默认 1,800,000 = 30 分钟）三 flag；执行成功打印统计行 `plan_versions/replans/nodes_executed/wall_ms`
- **引擎节点计数（workflow.rs）** — `WorkflowEngine` 新增 `executed_nodes()`（Arc<AtomicU64>，compute 成功与 LLM 成功两处累加，Clone 共享同一计数器），供驱动预算判定消费
- **C9 防自指递归守卫（materializer.rs）** — PlanFact 节点引用 `agent_type="planner"` 即拒（planner 是计划生产者非执行者）；golden 示例 fixture 同步对齐
- **planner agent 档案（`agents/planner.json`）** — PlanFact 产出者：temperature 0.2 / max_steps 2 / 无工具；system prompt 含 PlanFact schema 严格规则（节点 id 文法、agent_type 白名单 researcher|general 且禁 planner、单汇点、loops `prev.` 前缀、冻结限额）+ few-shot 示例
- **planning probe 工作流（`rules/workflows/research_plan.json`）** — 单 planner 节点，task 为研究目标 + PlanFact 产出指令，`--plan-execute` 模式入口
- **replan 演练工作流（`rules/workflows/replan_drill.json`）** — Dsl 模式 replan 触发演练：boom 节点引用不存在的 `ghost_agent` 必失败 → Failure replan → planner 产 v2 修复重跑
- **真实 LLM E2E（`tests/e2e_plan_execute.py`，IT 级不进 CI）** — 两场景断言：场景 A（probe → PlanFact v1 → 物化 → 执行，`plan_versions=1 replans=0`）与场景 B（v1 失败 → Failure replan → v2 成功，`plan_versions=2 replans=1`）；`.env` 进程环境注入（O-095 口径）；`--evidence-dir` 证据落盘
- **`blake3` 依赖重新引入** — `parent_plan_hash`/`failed_plan_hash` 锚计算需要（与生态 BLAKE3 哈希纪律一致；此前 0.x 曾以零调用移除，本批恢复为真实消费）

#### workflow_dag v1.2 动态循环基座（plan-execute 方案 D Phase 1-A）
- **workflow_dag v1.2 物化器（`src/agent/materializer.rs`）** — 纯函数物化器：把 v1.2 文档（手写 DSL 形态与 PlanFact 形态归一化）静态展开为线性 DAG——顶层 `loops` 循环体按 `{loop_id}_iter{k}_{node_id}` 展开为副本链（`max_iterations` 1..=32、`body` 1..=8、loops ≤ 8、展开前 ≤ 64、展开后 ≤ 512，冻结限额校验）；跨迭代引用文法 R1–R4 三消费面（模板占位符/compute inputs/run_when 观察）共用同一分类器，iter0 的 `prev.X` 统一消解为空串语义；跨迭代隐式依赖逐节点全连；展开后自检（id 唯一/引用存在性/拓扑层序/无环）；同输入必同输出（字节级确定性测试锁定）
- **`constitution::load_workflow` 统一加载入口** — schema 校验 → v1.2 物化 / v1.0-v1.1 直接反序列化；v1.2 防呆拒载门翻转为「schema + 物化」双门（引擎 loop/compute 能力已落地，物化门保证 v1.2 增量语义被真正消费而非被 serde 静默忽略）；`workflow` 子命令切换至该入口
- **compute 纯函数节点（workflow.rs）** — 封闭目录三函数 `strcmp`（equal|different / contained|not_contained）/ `numeric_cmp`（true|false，IEEE 754，解析失败 = 节点失败无静默回退）/ `regex_match`（match|no_match，pattern 加载期校验）；execute 层循环内同步内联求值——不经 delegate（不占并发槽/不耗 max_depth/无会话/无 IoRequest），结果与 LLM 节点同表同构（run_when 观察/占位符渲染/下游 compute 级联三面消费）；validate 层封闭目录代码校验（输入数量/threshold 互斥/pattern 编译/引用存在性）+ 层序检查（输入仅可引用更早拓扑层）
- **replan 触发判定（`src/agent/replan.rs`，骨架）** — 纯函数 `should_replan` 判定序写死（replan 硬上限默认 3 → 失败优先 → 预算任一维度达到阈值）；`WorkflowFailureRecord` 失败摘要（Err 文本纯函数解析失败节点 id）+ `BudgetCounters` 预算计数器（tokens_used MVP 恒 0）+ `BudgetThresholds` 阈值（来自驱动配置禁止进 PlanFact，动态默认 max_nodes = 展开后节点数 × 2）；`workflow` 子命令接线失败/预算触发路径，外层驱动循环（重调 planner 产 v2）属 Phase 1-B；零新增 Fact 类型
- **workflow_dag v1.1 `run_when` 条件分支补记** — v1.1 节点级条件分支（求值为假跳过、豁免级联、空观察源语义）此前未入 CHANGELOG，随本批一并补记

#### IDE 工作台（Web IDE 前端，Trae 式布局）
- **内置 IDE 工作台**（`web/`，Svelte 5 + Vite 5 + Monaco Editor）— serve 直接托管（API 路由外层 fallback 到 `web/dist`，SPA 缺省回退 index.html）：左活动栏+文件树面板（S0 静态占位）、中编辑器群（Monaco 欢迎页验证内核与主题集成）、右对话侧栏（WS 双向流直连 agent 会话，真实模型流式回复 + 工具调用摘要卡 + 审批卡展示）、底部审计抽屉（默认收起占位）。设计 token 全量提取自 console-cloud 设计系统 v3.0（Docker 风格深色主题，字体/色板/间距/圆角/阴影逐项一致）；未构建前端时自动降级纯 API 模式
- **`tower-http` 新增 `fs` feature**（ServeDir/ServeFile 静态托管依赖）；工作台静态资源不经过 API 面鉴权中间件（页面须无 token 可打开），API 与 WS 面鉴权口径不变
- **工作台文件面（`src/api/file_api.rs`）** — 三端点 `GET /api/files/list` / `GET /api/files/read` / `PUT /api/files/write`，全部委托内置 file 工具实现（同一 workdir 沙箱与路径校验，不在 API 层重复安全逻辑）：list/read 复用 union toolkit 内同一工具实例（与 agent 完全同语义）；write 为人工编辑语义（`writable_dir="."` 使写面为 workdir 全域、固定 overwrite + create_parents，沙箱边界与 1 MB 上限原样保留）。受 G7 鉴权中间件保护；语义边界：本面服务人的直接操作，不构造 agent 会话事实（不进 agent 会话审计链），agent 路径 `file_write` 的 workspace 白名单与审批语义不变
- **文件树与编辑器真实化（前端）** — 文件树改为真实目录懒加载浏览（点击目录展开/收起、文件点击进编辑器）；编辑器多 tab（每文件独立 Monaco model 保留 undo 栈、dirty 角标、关闭切换）、Ctrl+S 保存落盘（走文件 REST 面，同 agent 写文件安全通道）、打开失败占位与保存失败提示
- **会话索引与历史恢复** — 新增 `GET /api/sessions`（会话列表：serve 本地 JSONL 索引，WS 面在会话创建/轮次结束时记录，读时去重合并，按最近活跃降序）与 `GET /api/sessions/{id}/transcript`（消息历史：从 evorule payload 投影 agent 持久化消息，零写入、同 idx 后写覆盖）；对话侧栏新增「历史」面板（点击恢复完整消息记录，含工具调用卡；继续对话自动接续同一会话），离开工作台再回来历史会话与内容完整可见；新增探针 `tests/ws_session_probe.mjs`（真实 LLM 会话级 E2E）
- **会话消息本地快照与保留期管理** — evorule 会话有 30 分钟闲置 TTL 自动回收，回收后历史投影报 `Session not found` 导致「历史恢复失败」；本轮落地：① WS 面 `TurnEnd` 时同批落本地消息快照（与 transcript 端点同一投影路径，保证同源同构；目录 `data/snapshots/YYYY/MM/DD/session_{sid}.json` 按日期分片，同 UTC 日原子覆盖）；② `GET .../transcript` 改为活投影优先 → 会话已回收时回落本地快照（响应带 `source: "snapshot"` + `authoritative: false`，前端恢复时明示「引擎侧会话已过期」）；③ 快照保留期配置 `GET/PUT /api/workbench/config`（`1d|1m|3m|6m|1y|forever`，缺省 3 个月，原子写 `data/workbench_config.json`），历史面板顶部下拉可改；④ serve 启动清扫 + 每日周期清理（每次现读配置，按日期分片目录整体删除）。**快照为展示层非权威副本**：审计真相源仍是 evorule FactsLog（append-only 永不删），删除策略只作用于快照目录，永不越界；回放/时光机器走引擎自审计链重建，零依赖快照
- **工作台治理叠加（evorule 功能落位）** — ① 底部面板从单审计抽屉升级为多 tab 面板（中栏底部、sash 拖拽调高/键盘 ↑↓ 微调、可折叠；标配 tabs：终端/输出/问题/控制台日志 + evorule tabs：审计/时光机器/记忆；实装「输出」= 系统事件流水与「审计」= 当前会话治理事件流 + 「在审计页查看」深链跳 console 审计页 `?session=<id>`；终端 PTY/问题/控制台日志/时光机器/记忆为占位待后续阶段）；② 审批卡交互：审批卡可直接「批准/拒绝」，走既有 `POST /agents/{t}/approve` 通道（60s 超时自动拒绝；无提案 ID/已失效/不匹配等情形在卡上明示）；③ 顶栏治理徽标：白名单（agent 工具数）+ 信号（当前会话违规信号累计，fail-soft）；④ 新增 `GET /api/sessions/{id}/evolution-signals` 只读代理（信号徽标数据源，透传 evorule-server 既有聚合端点，零写入、evorule-server 零改动）。**流式审批帧时序修复**：原实现 `ApprovalRequired` 在审批决策完成后才补发——帧到达前端时 60s 审批窗口已过，HTTP 审批在 WS/SSE 路径上结构性不可用（真实 LLM E2E 实测暴露）；修复为两阶段（先发 `ApprovalRequired` 开启窗口、决定后发 `ApprovalResult`），非流式路径与 CLI 交互审批不受影响；新增探针 `tests/ws_approval_probe.mjs`（真实 LLM 审批路径 E2E，批准/拒绝两路）
- **console 审计页 sidecar 自动拉起（可选配置）** — 首验实测发现审计深链指向的 console dev server（5174）未运行时为死链；新增 `[workbench] console_dir`/`console_port` 配置（缺省空 = 零副作用）：serve 启动期端口探测未监听则子进程拉起 vite dev（fail-soft 不阻断主功能、输出落 `data/console_sidecar.log`、独立存活幂等）；前端深链点击前探活，不可达时就地明示引导不跳死链；修复中连带发现 `Config::merge_from_file` 逐段合并漏新段（新配置段静默不生效）并修复
- **agent 产物协作编辑（agent 草稿 → 人定稿 → 落盘）** — 补齐「agent 产内容 → 人审改 → 定稿落盘」协作回路（纯前端，零后端改动）：agent `file_write` 成功后产物自动在编辑器打开并登记，对话侧栏新增「agent 产物」卡（路径/字节数/草稿-定稿状态/一键回编辑器）；编辑器顶部产物条提供「查看与草稿差异」（Monaco diff：左=agent 写入时的草稿基线快照，右=当前可编辑内容）与实时状态（草稿 · 编辑后 Ctrl+S 保存即定稿 / 已定稿）；保存即定稿（复用既有文件 REST 写通道）；草稿基线与定稿留痕为工作台展示层记录（localStorage 按会话持久化、总量 2MB 上限尽力持久化），不入 agent 会话审计链（语义边界见 README）；同路径被 agent 再次写入 = 刷新草稿基线重开协作轮

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

- **serve 启动横幅打运行体身份** — 启动即打印 `版本 | exe mtime (epoch s) | workdir 绝对形态`，排障时可直接核对「运行中的 exe」与「源码 HEAD」是否一致，不留常驻进程二进制过期的盲区（无 build.rs 依赖）
- **宪法 schema 校验收编共享组件 `evorule-constitution`** — `src/agent/constitution.rs` 由本地实现（目录探测/跨文件 `$ref` 内联/校验执行）改为薄封装（公共 API 签名不变，消费点零改动）：判定逻辑与 schema 数据（编译期内嵌，运行时零磁盘依赖）委托统一 crate（git 依赖 + rev 钉版），判定代码单一化；`jsonschema` 0.18 → 0.21（`Validator` API，跨文件 `$ref` 经 `$id` 解析，内联 hack 退役）；CI「检出宪法仓」步骤退役（cargo 自动拉取 git 依赖）；依赖契约断言精确化（主仓 crates 拦截保留，独立仓治理组件显式 allowlist）
- **`list_publish_queue` 客户端方法** — 新增可选 `workspace_id` 过滤参数（None 保留旧行为）；`PublishQueueItem` 补齐 `kind` 字段（对齐 server 模型）
- **移除 `blake3` 直接依赖** — 零代码调用（仅文档注释提及概念），死依赖删除；
  evorule-reactor 自身对 blake3 的依赖不受影响
- **移除 `evorule-tcb` / `evorule-reactor` 依赖** — Agent 编排层与 evorule 仓 TCB/Reactor 代码解耦；
  `evorule_tcb::JsonValue` 全面替换为 `serde_json::Value`（IoHandler trait 签名与工具参数同步），
  专用转换模块 `json_convert` 不再需要，随之删除

### 🐛 修复
- **核收节点「首行二元裁决」契约被 LLM 措辞摇摆穿透（O-123）** — closure 节点原契约要求「回复第一行为核收通过/核收不通过」：实测 LLM 输出首行裁决与文内差口表/自我复验结论相反，下游按首行解析即得误判。模板 v1.0.1 重写为两段强结构：第一段逐项核对清单（每个 agent 一行「覆盖=已覆盖/遗漏；工具清单=一致/差异」，不得合并省略）+ 末行独占裁决（当且仅当清单全部已覆盖且一致才可「核收通过」，裁决词不得出现在清单结束之前）——首行不再承载裁决，「首行解析误判」穿透面结构性消除。真实 MiniMax driver 正例 E2E：三节点全过，closure 输出 5 agent 逐项清单→末行「核收通过」结构一致零矛盾
- **LLM 业务层错误体被静默判成功（workflow 节点假绿）** — OpenAI 兼容端点对业务层失败（无效密钥/额度不足）返回 HTTP 200 + `{"base_resp":{"status_code":1004,...}}` 错误体（无 choices）：流式路径已有显式检测，非流式 `parse_success_response` 却照常解析并 `unwrap_or("")` 出空 content → 引擎 Stable 后节点被判成功——空产出沿工作流下游传播形成全链假绿（假绿指纹：tokens_used=0 + 亚秒墙钟 + content_len=0）。非流式路径现与流式对称：`base_resp` 错误体与 `choices` 缺失均显式转错误（附响应摘要）；`run()` Stable 臂补最近 LLM 输出回捞（与流式路径对称，消除不对称）；空产出 warn 观测（不改判合法空响应）。突变验证测试两条锁定（HTTP 401 与 HTTP 200+错误体均如实传播为 run 错误且 error io_response 回写链上）
- **委托子代理无工具执行回路（单轮即止+零工具契约）** — workflow 节点委托的子代理原以非流式 `run()` 单发执行且无任何工具契约：LLM 无 tools 可知只能凭训练先验输出供应商原生 `<minimax:tool_call>` XML 死文本，无执行也无结果回喂，节点产出=意图声明而非任务实质。现 `DelegateContext` 支持 union toolkit+工作目录成对注入（`workflow` 子命令接线），子 runner 按档案 `tools` 白名单过滤挂载并接能力边界（与 serve 面/CLI 巡视同款模式；过滤后为空不挂载=纯规划型子代理行为不变）；执行改走流式消费至 Done——本地 ReAct 循环多轮执行工具并回喂直至最终产出，`delegate()` 对外签名不变（driver/标记层零改动），token 埋点随流式路径累加；委托场景不新增审批通道（candidate 工具默认拒绝，fail-safe）
- **IoRequest 处理失败后引擎在途请求悬挂（链实不一致）** — 非流式 `run()` 主循环对 IoRequest 处理失败（60s step 超时 / LLM 错误 / 工具错误 / 内部错误）一律直接上抛，不回写 io_response：server 侧 io_request 永久挂起，链上留下幽灵在途请求，链记录与 runner 实际执行态脱节；流式路径另有 4 处 persist_message 失败早退同病。统一为「失败也回写 error io_response 再终止」——与 audited_llm 既有「LLM 失败不留悬空 IoRequest」契约、流式路径取消/错误分支同一语义，保证引擎状态机必能收尾；突变验证回归测试锁定（无修复红 / 有修复绿）
- **两处 O-119 回归测试在 Linux 误用 Windows 绝对路径样本（CI ubuntu 红灯）** — `file_list`/`search_files` 的越界文案测试以 `"C:\Windows"` 作绝对路径样本：Unix 上该形态**不是**绝对路径，走不到拒绝分支（落到不存在目录的 io 错误），`sandbox boundary` 文案断言失败（M5-a `ebe54da` 同族教训二犯，`file_read.rs` 测试已有 `cfg!(windows)` 条件化先例）；两测试改为平台各自真实绝对路径样本（Windows `C:\Windows` / Unix `/etc`）。顺带修复 `verify.ps1` 只留 cargo test 汇总行的问题：失败明细随 stderr 走，原 `2>$null` 丢弃导致 CI 日志无失败定位信息（本次只能本地 WSL 复现取证）；改为并流保留，失败时输出末 80 行明细
- **工具意图裁决假拦（独立裁决会话通道）** — 意图裁决原在主会话内「提交+1s 轮询 version」：主会话 `call_external` 在途（io_request 包装流）时引擎命令串行评估，意图指令仅在 IoResponse 后才被评估，轮询恒超时→一律判 blocked——工作台第二轮对话起相对路径合法文件操作也被假拦（首轮因新建分支漏设 session_id 恰好跳过裁决而正常）。重构为独立裁决会话通道（`agent/adjudicator.rs` `AdjudicationChannel`）：每 runner 一条 evorule 裁决会话（惰性创建、轮内复用，initial_content 自述身份供审计关联），不受主会话 io 在途影响；fail-closed 语义不变（version 未推进=拦截），传输错误失效重建重试一次仍败 fail-fast；首轮/续轮统一走裁决（删 `if let Some(session_id)` 守卫），新建分支同步回填 `runner.session_id`。四不变式：意图指令形态不变（中性 `set meta_tool.pending_target_scope`）/R1 规则资产零改动/workflow 场景零改动（phase 门仍走主会话）/裁决会话历史即审计证据。真实 LLM E2E 三场景验证：WS 多轮首轮/续轮相对路径放行、越界绝对路径拦截且 agent 自述边界；driver 正例三节点零误伤；driver 负例 R2 拦截 halt 零 LLM
- **search_files 中文文件名 panic** — glob `*` 分支按字节索引切片（`0..=len`），索引落在多字节字符内部即 panic（byte index not a char boundary），中文/日文等文件名检索必触发、会话该轮崩溃；改为 `char_indices` 迭代+末尾空后缀补测（`?` 分支本就 char-safe 不动）；新增多字节 glob 断言与中文文件名端到端搜索单测
- **file_list/search_files 越界文案缺边界路径** — 两工具绝对路径/父目录/越界错误仍是旧格式（无 M5-a 边界后缀），agent 被拒后无法自知边界；三条文案对齐 file_read/file_write 同款（回报「不可访问 + 沙箱边界绝对路径」）
- **缺省沙箱根以相对「.」呈现** — 未声明 `capability_boundary` 时合成根直接用启动 workdir（相对路径形态），边界展示面（系统提示段/拦截文案）出现「outside the sandbox boundary '.'」，LLM 与用户均不可读；合成根改为 canonicalize 绝对化（目录缺失时 fallback cwd 拼接，相对尾段保留），Windows `\\?\` verbatim 前缀在展示面简化为常规形态（UNC 还原 `\\server\share`）
- **workflow phase 门与节点完成信号时序倒挂（M5-c 实测修正）** — `node_done` 完成信号原由外层驱动在节点执行返回后 drain 提交，晚于引擎内 phase 门求值：带约束规则的 workflow 第二节点 delegate 前置门必被误拦（前置标记尚未落链，`exists` 判 false）。修正为 LLM 节点成功分支即时打标（workflow.rs `mark_node_done`：向标记会话提交完成信号并 version 感知等待落链），门/打标严格配对；driver drain 保留作幂等兜底，未注入 phase_gate 形态零变更
- **`.env` 仅 serve 子命令加载（workflow/run 等子命令 LLM 密钥失联）** — O-095 结构性修复当时只接了
  `cmd_serve`，`workflow`/`run`/`patrol` 等子命令路径不加载 `.env`：真实 LLM 运行在未显式注入环境变量时
  密钥缺失，LLM 调用失败且节点表现为空产出假绿。现前置到 `main` 入口统一加载（已设置的环境变量优先，
  serve 路径行为不变），所有子命令裸启动均可携带 LLM 密钥
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
