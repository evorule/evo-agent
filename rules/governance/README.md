# rules/governance/ —— 资产源目录（非加载路径）

本目录是治理规则资产的**编辑源/声明面**，供治理仓与 server 侧副本同步、
评审与人工引用。**evo-agent 运行时不从本目录装载任何规则**——本仓的规则
加载路径只认 `rules/workflows/`（工作流资产，经装载门卫与全量资产回归门）。

各文件效力口径（如实）：

| 文件 | 性质 |
|---|---|
| `00_constraint_model_allowlist.json` | 会话模型准入白名单（enforce：拦截白名单外模型的 `call_external`）。本仓内为编辑源；**生效执行体**是 server 侧 ai-plugin 的网络入口白名单（物料为本文件副本） |
| `00_constraint_tool_trace_shell_guard.json` | shell 工具危险命令守卫的声明源；运行时拦截由内置工具 `shell_exec` 的危险名单与引擎 enforce 共同承担 |
| `00_constraint_tool_intent_adjudication.json` | 工具意图裁决（P2 通道）的声明面 |
| `00_constraint_collab_acceptance.json` | 协作验收约束声明面 |
| `00_constraint_container_tooling_permit.json` | **声明型许可清单**（`metadata.permits`）：transform 仅含空动作分支，运行时行为与缺省一致，不新增任何拦截——列入 `00_constraint_` 命名族是为与同族约束并列管理，**不代表有执行语义** |
| `collab_task_marks.json` / `judge_acceptance_marks.json` | 标记/判据资产 |

引用与部署方请注意：判断某条规则是否会在运行时拦截行为，以**执行体
（server enforce / 内置工具守卫）的实际接线**为准，不以本目录文件存在为准。
