# 审批策略指南（审批三档制）

本文档说明工作台审批三档模型（auto / manual / policy）的语义、policy 档的开启步骤、
agent 的自我修正机制、红线豁免清单与关闭风险、审计回放用法，以及典型场景的推荐配置。

## 一、审批三档说明

每个工具/工具组的审批档位由设置键 `approval.mode.<toolGroup>` 控制（v1 注册了
`approval.mode.shellExec`），三值枚举：

| 档位 | 行为 | 适用 |
|---|---|---|
| `auto` | 白名单直跑，不经审批 | 只读、无副作用操作 |
| `manual` | 每次弹出人工审批卡（60s 无响应自动拒绝） | 缺省值；写操作标配 |
| `policy` | 对照人类预置条件由独立评审自动判定：全部条件通过则放行；有不符合则退回修正指令，agent 修正后重查 | 长程任务进阶选项 |

要点：

- **policy 是进阶选项**：需要先预置条件文件（见下节），文件缺失、无效或该工具
  未预置条件时，policy 档自动回落人工（manual），绝不静默放行。
- **policy 不改变工具面三层框架**：运行时仍是 active / candidate / blocked 判定，
  policy 只是 candidate 的自动化执行路径；blocked 级拒绝任何档位下都生效，不可配置绕过。
- 三档均可在 设置 → 审批策略（或「Agent 工具」分类）中调整。

## 二、policy 开启步骤

### 1. 预置条件文件

条件文件路径：工作区 `.evo/approval-policy.toml`，由 serve 侧独占读取（agent 上下文
只有只读副本，agent 修改条件文件的操作本身命中豁免清单强制人工）。

条件条目支持**字符串|表两种形态**（见下节「条件类型」），模板：

```toml
# 每工具/工具组一节,conditions 为条件条目清单(字符串=自然语言,表=机械/验证条件)
[shell_exec]
conditions = [
  # 自然语言条件:交独立评审模型逐条判定(兜底层)
  "命令限定为只读:cargo build / cargo test / cargo check / git status / ls / cat / grep",
  # 白名单条件(机械层):操作详情命中任一 glob 才符合,零命中=不符——零 LLM
  { text = "只放行 cargo 构建类命令", match = ["cargo build*", "cargo check*", "cargo test*"] },
  # 词法排除条件(机械层):操作详情任一叶子串含排除词即不符
  { text = "不得外发代码", exclude = ["git push", "curl", "wget", "ssh"] },
  # 路径前缀白名单条件(机械层):args 中路径语义键(path/file_path/file/target/
  # new_path/old_path/dest/dest/dir/directory/root/workspace/folder 等)识别出的
  # 路径值须全部落在白名单前缀内;任一越界=不符,零识别路径=不符(fail-closed)
  { text = "仅允许工作区内文件写入", path_prefix = ["D:/work", "./out"] },
  # 外部验证条件(验证层):serve 侧执行验证命令,不经 shell;exit 0=符合,
  # 非 0=不符(输出尾段进修正指令),超时/spawn 失败=判定不可信降级人工
  { text = "工作区构建通过", verify = { cmd = "cargo check", timeout_secs = 120 } },
]

# 文件写族组(file_write/file_create/file_move/file_edit/write_file/edit_file):
# 组档位键 approval.mode.fileEdit 设为 policy 时走本组条件评审
[file_edit]
conditions = [
  { text = "写入目标须在工作区内", path_prefix = ["D:/work", "./"] },
]

# 文件删除组:注意不可逆删除是红线操作——无论组档位如何设置,file_delete 在
# 任何档位下默认强制人工审批,仅在豁免覆盖显式开启后由条件评审接管
[file_delete]
conditions = [
  { text = "仅允许删除输出目录内文件", path_prefix = ["D:/work/out", "./out"] },
]

[policy_limits]
max_corrections = 3      # 修正重查上限(超限降级人工)
max_actions_per_run = 200 # 单任务自动操作配额(超出暂停等人,Runaway 护栏)
```

### 1.1 条件类型

| 形态 | TOML 写法 | 判定层 | 语义 |
|---|---|---|---|
| 自然语言 | `"字符串"` | LLM 兜底 | 交独立评审模型判定（条件类型扩展前的行为，完全兼容） |
| 白名单 | `{ text, match = [glob...] }` | 机械（零 LLM） | 操作详情叶子串须命中至少一个模式（小写归一 + `*` 通配）；零命中=不符 |
| 词法排除 | `{ text, exclude = [词...] }` | 机械（零 LLM） | 任一叶子串含排除词=不符；零命中=符合 |
| 路径前缀 | `{ text, path_prefix = [前缀...] }` | 机械（零 LLM） | args 中路径语义键识别出的路径值（分隔符/大小写归一）须全部命中至少一个前缀；任一越界=不符；**零识别路径=不符**（fail-closed 防参数键名漂移静默放行） |
| 外部验证 | `{ text, verify = { cmd, timeout_secs } }` | serve 侧执行 | exit 0=符合；exit 非 0=不符（输出尾段进修正指令）；超时/spawn 失败=判定不可信降级人工 |

装载期 fail-fast 校验（任一命中即拒载，运行时 fail-visible 降级人工）：

- 条件表未知字段、`text` 缺失、四机械键（match/exclude/path_prefix/verify）全缺——拒绝；
- 同一条件表 `verify` 与 `match`/`exclude`/`path_prefix` 混用、或机械键互相混用——拒绝
  （单条条件只声明一种机械/验证形态）；
- `cmd` 字符串形态含 shell 元字符（`|` `;` `&` `>` `<`）——拒绝（验证命令不经
  shell 执行，需要管道/重定向请显式写包装脚本再以 argv 数组引用）；
- `timeout_secs` 超上限 600——拒绝（缺省 120）；
- `policy_limits` 未知键——拒绝。

判定管线三层（逐层短路）：①机械层（match/exclude/path_prefix 零 LLM，任一不符→
拒绝并回喂修正指令，跳过后层）→ ②验证层（verify 逐条执行）→ ③LLM 兜底层（仅存
自然语言条件时调用评审模型）。**条件清单全部为机械/验证条件且全过=零 LLM 快路径**
（评审模型零调用，判定留痕标 `mechanical`）。验证器只能收紧判定（不符→拒绝），永远不
能放宽——白名单/排除/验证器都不是红线的旁路：红线闸先于一切机械判定。

### 1.1.1 每工具组档位（组级扩面）

除会话缺省键 `approval.mode.shellExec` 外，文件工具组有独立档位键（设置 → Agent
工具分类；留空=跟随会话缺省档位，行为零变化）：

| 设置键 | 覆盖工具 | 说明 |
|---|---|---|
| `approval.mode.fileEdit` | file_write / file_create / file_move / file_edit / write_file / edit_file | 文件写族组档位（policy 档走条件文件 `[file_edit]` 等组评审，配 path_prefix 可实现工作区白名单零 LLM 放行） |
| `approval.mode.fileDelete` | file_delete | 文件删除组档位。**注意**：不可逆删除是红线操作——任何档位下默认强制人工审批，仅在豁免覆盖显式开启后由条件评审接管 |

来源优先序：定义级 `approval_mode` > 组档位键 > `approval.mode.shellExec` 会话缺省
键。定义未指定且组键与会话缺省出现差异时装配每工具组路由器（per-request 按工具
归组路由到该组独立审批栈，路由决策留痕）；全同=单栈直连（装配与行为零变化）。
serve 三消费面（WS/HTTP run/plan-workspace 强制 policy 面）与 CLI run/REPL 面同
口径接线；CLI run 面无人值守预检对组键=policy 同样拒启（评审调用无界挂起风险）。

### 1.2 编辑入口

编辑入口：设置 → 审批策略 → 编辑条件文件。保存即整体覆盖并自动校验；语法或
schema 校验失败会显式标红（运行时对该文件 fail-visible 降级人工）。

### 2. 试判定

设置 → 审批策略 → 按条件试判定：输入工具名与操作参数 JSON，点击「试判定」。
该预检为纯机械判定（红线豁免 → 条件清单 → 限额 → match/exclude 实判），零 LLM
调用、不执行 verify 命令、不落账，结果三色：

- 红「强制人工审批」：命中红线豁免清单且豁免覆盖未开启——评审不会发生；
- 黄「进入评审判定」：交由评审按条件逐条判定；
- 灰「降级人工(不可评审) / 条件文件未预置 / 条件文件无效」：运行时会回落人工。

响应逐条给出 `condition_results`（match/exclude 对请求参数实判红/绿；verify 标注
「试判定不执行命令」；自然语言标注「运行时交评审」）；存在机械不符条目时注记
「运行时将拒绝并回喂修正指令」。

### 3. 切换档位

把目标工具组档位设为 `policy`（如会话缺省键 `approval.mode.shellExec`，或组级键
`approval.mode.fileEdit` / `approval.mode.fileDelete`，见「每工具组档位」一节）。
从 manual 切到 policy 后，该组工具的候选操作不再弹人工审批卡，改走评审判定链。

## 三、修正机制（agent 会怎么自我修正）

policy 档下每次候选操作触发一次判定（三层管线，见「条件类型」一节；机械/验证
条件先行裁决，评审模型只在存自然语言条件时介入）：

1. serve 组装判定：机械层（match/exclude 零 LLM）→ 验证层（verify 执行）→
   LLM 兜底层——评审请求只含工具名、操作详情（argv/路径/内容摘要）、**自然语言
   条件清单**与已通过条目的证据摘要，**不含 agent 对话上下文与执行意图**
   （独立性保证，杜绝自我审批偏袒）；
2. 独立 LLM 评审调用（评审专用 system prompt，非流式，30s 超时；评审判定依据
   收缩为两源：拒绝规则 > 预置条件——机械可核验条件已由系统先行裁决，评审只判
   自然语言条件）；红线类操作在评审前置闸即拒绝，评审不发生；
3. 判定结果机械重算（评审自报结论不作数）：
   - 全部条件通过 → 放行执行；
   - 存在不符合条件 → 拒绝，未通过条目与理由组装为**修正指令**回喂 agent，
     agent 修正方案后重查（同一条件清单全量重判），重查上限 `max_corrections`
     次（缺省 3），超限降级人工——防无限循环；
   - 任一条件无法判定 / 条件清单为空 / 评审输出解析失败 / 超时 → 判定不可信，
     直接降级人工。

**Runaway 护栏**：单任务内自动操作（工具调用）累计计数，不随成功重置——配额
次数（`max_actions_per_run`，缺省 200）内正常放行，第 N+1 次尝试触发暂停：当前
操作不执行，任务以「等人确认」收尾，计数经事件日志跨重启回填。每轮发给模型的
上下文尾部还会注入「任务护栏复述块」（任务目标+未满足条件+剩余配额），对抗
长程目标漂移。

## 四、豁免清单与关闭风险

以下五类操作命中**红线豁免清单**，默认在任何审批档位下强制人工审批，评审不发生：

| 红线 | 留痕标记 | 覆盖面 |
|---|---|---|
| 不可逆删除 | `red_line.delete` | `file_delete`（含批量） |
| 推送远端 | `red_line.remote_push` | git push / force push 等影响超出本地工作区的命令 |
| 数据外发 | `red_line.data_exfiltration` | 网络外发命令、`http_get` 抓取 |
| 改治理配置 | `red_line.governance_config_write` | 审批策略/设置等治理配置文件写入（自我扩权防护） |
| 宿主任意代码执行 | `red_line.host_code_execution` | `python_exec` 未装配 workspace 沙箱（落宿主解释器任意执行，静态逃逸检测可被动态构造绕过，按执行面兜底） |

**豁免覆盖**（`approval.exemptionOverride`，默认关闭）：开启后上述操作不再强制
人工，改由评审按条件自动判定，判定留痕携带 `exemption_overridden` 标记。开启动作
在设置页需二次确认，界面持续展示标红警示条，服务端对该次设置写入落 warning 级
日志留痕。

关闭风险须知：

- 豁免覆盖开启后，agent 理论上可在无人参与下完成「删文件→自我审批」叠加动作；
  工作台的删除有回收站（.evo-trash）兜底，但**这是风险兜底而非安全边界**；
- 该开关不影响 blocked 级拒绝（不可配置绕过），也不影响评审自身的拒绝规则优先级；
- 建议仅在理解上述风险、且任务环境可控（隔离沙箱/可回滚工作区）时开启。

## 五、审计回放用法

policy 档的全部判定过程落会话事件日志（append-only）：

- 每次 policy 判定产生 `policy_judged` 事件，携带条件清单、逐条判定
  （pass/evidence）、最终结论与总体理由全文；
- 操作的工具轨迹（tool_trace）中，审批子对象记录判定来源（`policy-review`）、
  结论与豁免覆盖标记（如适用）；
- 治理事件流同步推送 PolicyApproved / PolicyRejected / PolicyFallback 三类展示事件，
  可在工作台事件轨道查看。

回放方法：打开对应会话 → 事件/轨迹视图按 `policy_judged` 或红线标记
（`red_line.*`、`exemption_overridden`）筛选，即可核对「哪个操作、按哪些条件、
以什么证据」被放行或拒绝。

## 六、场景建议

**挂机重构**（长程代码任务，人在附近可接管）：

- 写操作工具设 `policy`，预置「限定改动范围+必须通过构建/测试」类可机械核验条件；
- 豁免覆盖保持默认（关闭）——删除/推远端等操作仍停下来等人；
- `max_actions_per_run` 保持默认 200 作为失控护栏，`max_corrections` 缺省 3。

**无人值守 CI 助手**（彻底挂机，无人实时响应审批）：

- 目标工具全量设 `policy`，条件收敛到可机械核验的白名单式表达；
- `approval.exemptionOverride` **保持关闭**——红线操作触发即暂停/拒绝，宁可
  任务中断也不无人化高危操作；
- 条件清单从紧起步，经审计回放抽查后再逐步放宽。
