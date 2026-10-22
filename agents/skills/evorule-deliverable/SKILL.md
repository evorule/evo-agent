---
name: evorule-deliverable
description: 可复算交付物组装纪律——产出报告/表格/文档时,数字一律出自确定性服务并留溯源清单,禁模型心算入文;导出走 export_* 服务族落盘信封
---

# 可复算交付物（evorule-deliverable）

## 何时用

用户要"报告/报表/文档/交付物"且其中含**数字**时（对账表、分配方案、审计报告、分析文档）。
纯文字创作不适用本技能。

## 铁律（违反即废品）

1. **数字只准来自服务**：任何将进入交付物的数值，必须先调用对应确定性服务
   （numeric_*/gov_*/indicator_*/gnss_* 等）取得结果，把服务返回值原样入文。
   **禁止把模型心算/记忆里的数写进交付物**——算错无人能查，也复算不出。
2. **溯源清单随行**：每个数字记一条 `{claim, service, args(压缩JSON), digest(信封 blake3)}`，
   作为 `provenance` 参数传给导出服务——它会渲染成文档内"溯源"表，读者可逐项重算。
3. **导出走 export_* 服务族**（落盘信封，不把文件内容搬进对话）：
   - 表格/对账/多表 → `export_workbook_xlsx`：`{title, sheets:[{name,header,rows}], provenance, filename}`
   - 报告/文档（段落+表格）→ `export_document_docx`：`{title, paragraphs, tables, provenance, filename}`
   - 审计链/会话类摘要报告 → `export_report_pdf`：`{content_type, raw_data, integrity, options, filename}`
   - 单元格值域=字符串|整数；小数先经 numeric 服务转字符串（如 `"33.33"`），**不带浮点入表**。
4. **回报信封不回报文件**：导出服务返回 `{path, bytes, blake3}`——把路径与 blake3 报给用户，
   不要 file_read 整个文件回显。

## 标准动作序列

1. 先问清/推断交付物形态（表格 or 文档 or PDF）与字段；
2. 逐个数字调服务（同一服务可复用；args 原样进溯源）；
3. 组装 args（含 provenance 四元组列表）一次性调导出服务；
4. 向用户报告：文件绝对路径 + 字节数 + blake3 + 溯源条数（"每个数都可重算"）。

## 真机黄金示例（照此形态组装）

```
gov_ratio_split {"total":100,"weights":[1,1,1]}          → [34,33,33]
export_workbook_xlsx {
  "title":"比例分配对账",
  "sheets":[{"name":"对账表","header":["份额方","金额(分)"],
             "rows":[["甲",34],["乙",33],["丙",33]]}],
  "provenance":[{"claim":"三份均分100","service":"gov_ratio_split",
                 "args":"{\"total\":100,\"weights\":[1,1,1]}","digest":"<信封blake3>"}],
  "filename":"ratio_split.xlsx"}
→ {"status":"determined","determinism":"bitwise_replayable",
   "path":"…/ratio_split.xlsx","bytes":…,"blake3":"blake3:…"}
```

同输入重跑必得同 blake3（幂等覆写）——这就是"可复算交付物"。
