---
name: order-audit
version: 1.0.0
description: 订单审核纪律与输出约束合成样本
---

## 纪律
- 禁止跳过审批：→ 用 approve_gate 校验退款单必须走审批链
- 金额字段修改后必须复核留痕

## 输出格式
```json
{"instruction_type": "audit_report", "summary": "审核结论与依据"}
```

## 验证清单
- 检查 status 字段与审批链一致
- 使用 audit_tool 校验操作日志完整性

## 何时使用
处理订单审核、退款审批相关任务时。

## 背景
本 skill 为适配器集成回归的合成样本，不含真实业务语义。
