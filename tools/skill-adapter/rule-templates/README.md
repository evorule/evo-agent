# evorule 规则模板库

> **用途**：现成可复制的规则模板（params 契约 + 宪法补集已内置）。**复制模板 JSON 到流程实例的 `vars.rules` 数组即可**（替换占位符，或留空由执行器自动生成唯一 ID）。
> **契约纪律**：所有模板遵循系统数据契约（入参 `instruction.params.*`、set 纯覆盖、相对路径补全 `__exec__.`）；不与宪法桥接规则（call_external/call_service 全捕获）语义冲突；enforce 依据 `evorule-tcb/discipline/core_eval.json` DC 纪律正本。
> **验证**：各模板均可过 s0 预检（宪法冲突/契约/命名检查）。

## 模板清单

| 模板 | 类 | 业务语义 | 关键结构 |
|---|---|---|---|
| `rule-a-expense-limit`（流程模板内置） | A 类 | 金额 <5000 自动批准，否则转人工 | branch + lt 判定 + set 双分支 |
| `rule-c-multi-condition.json` | C 类 | 多条件 AND 组合判定（金额<5000 **且** 部门=finance） | branch + **all 组合**多个域条件 |
| `rule-gate-enforce-role.json` | 门禁类 | 仅 admin 可执行 approve_purchase，否则 **Violation** | branch + **enforce 顶层声明**（domain 违规条件 + reason 必填） |
| `rule-d-state-machine.json` | D 类 | 状态机节点迁移（from=pending → status=processing + push 后续） | branch + eq 状态 + **set + push** |

## 使用方式

1. 复制模板 JSON → 粘贴为流程实例 `vars.rules` 新条目
2. 替换 `{{rule_xx_id}}`（可留空 → 执行器自动生成 `rule-xx-{时间戳}-{随机}`）
3. 调整业务参数：阈值 `value`、部门值、角色值、状态名、reason 文案
4. 运行 `python flow_executor.py --flow my-flow.json`（s0 预检自动校验）

## 模板扩展原则（新增模板时遵守）

- 指令入参一律 `instruction.params.*`（禁 payload）
- 不与宪法 index 7/8/9 桥接语义重叠：call_external 禁止重复实现；call_service 必须补 `not exists(service_name)`
- enforce 必须顶层声明（DC-02）+ 携带 reason（DC-03）
- io_request 最多一个（DC-08）、必须叶子（DC-09）
- set 必须 attr+value（DC-06）；branch 必须 domain（DC-07）
