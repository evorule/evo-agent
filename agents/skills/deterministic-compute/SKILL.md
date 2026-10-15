---
name: deterministic-compute
description: 确定性计算卸载——把可形式化的计算交给 evorule-server 原生推理/几何服务（TCB 确定性内核），LLM 只保留语义判断与调度；含 17 服务路由表、plan-then-verify 纪律与诚实边界
---

# 确定性计算卸载（TCB 推理服务）

你是概率系统：同样的输入，两次回答可能不同，算多了会错。你的宿主环境连着
evorule-server，其中运行着**确定性推理层**——同样的输入永远同样的输出，逐位一致，
全程可审计。凡是"答案唯一、可形式化"的计算，交出去：更快、更省、不会错。

## 核心路由表（任务类型 → 服务）

你的工具列表中可能出现以下服务工具（名字精确如下）。按任务对号入座：

| 你正要做的事 | 交给谁 |
|---|---|
| 校验公式量纲 / 单位换算 / 单位比较 | `inference_dimension_verify_formula` / `inference_dimension_from_unit` / `inference_dimension_compare_units` |
| 预演"这个计划跑下去会发生什么" | `inference_forward_simulate` |
| 反推"达成目标缺哪些前提" | `inference_backward_plan` |
| 评估"这一步动作会影响什么" | `inference_effect_predict` |
| 检查计划是否成环 / 能否收敛 | `inference_cycle_detect` / `inference_convergence_check` |
| 检查方案间读写冲突 / 提取读写集 | `inference_conflict_detect` / `inference_rw_extract` |
| 比较两套规则集的差异 | `inference_differential_diff` |
| 定位"哪个决策引入了差异"（失败归因） | `inference_counterfactual_analyze` |
| 扫描参数取值面（哪些值安全） | `inference_counterfactual_sweep` |
| 判断图形是否碰撞 / 间距够不够 / 层次对不对 | `geometry_collision_check` / `geometry_clearance_check` / `geometry_occlusion_check` |

## 什么留给自己

语义理解、需求对错、自然语言生成、多方案权衡——服务不懂"对不对"，只懂
"合不合法"。**判断交给你的话，校验交给服务**：你先说方案，服务说这个方案
在规则面上是否成立。

## 纪律（违反即劣化）

1. **工具名以实际工具列表为准**。列表里没有 → 白名单未开或 server 不在线，
   如实报告并降级为普通做法；**绝不编造服务调用或假装已调用**。
2. **结果原样转述**。服务返回错误（fail-fast 语义）→ 如实上报错误本身，
   不粉饰、不脑补一个"大概没问题"。
3. **先预演后执行（plan-then-verify）**。重要动作前先让 `inference_forward_simulate`
   预演一遍；被拦下就改计划，不要硬闯。这一步花的是毫秒，救的是整盘。
4. **拒绝是语义的一部分**。服务显式拒绝某个参数（如不支持的能力）→
   改参数重试或换路由，不要绕过。
5. **参数契约以工具 schema 为准**，schema 没写的参数不要发明。

## 收益的诚实账

计算密集任务（公式、仿真、扫描、几何判定）收益最大；纯对话与写作任务
没有卸载空间，别为了用而用。省下的是你的步数与出错率——每一次确定性
计算都同时生成审计证据，出错时可回放归因。
