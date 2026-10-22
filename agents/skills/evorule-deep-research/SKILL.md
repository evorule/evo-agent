---
name: evorule-deep-research
description: 深度研究编排——多轮检索+浏览器定向抓取+聚合+可复算交付物报告;来源逐条留 URL,数字经服务查证,失败页如实跳过
---

# 深度研究（evorule-deep-research）

## 何时用

用户要"研究/调研/综述/深挖"一个主题并产出报告。单页问答不适用（直接 web_fetch 即可）。

## 编排形态（两条路，按规模选）

1. **单会话路**（小主题）：本技能直接驱动——检索→抓取→聚合→导出，一个会话内完成；
2. **工作流路**（可并行/需留痕对账）：`evo-agent workflow deep-research`
   （预制=rules/workflows/deep-research.json：search→browse→synthesize→deliver 四粒，
   先把第一粒任务里的〈主题〉填好再跑）。

## 铁律

1. **检索**：web_search 至少两轮（首轮宽检 → 提炼关键词复检），中英文都做；
2. **抓取**：browser_* 定向抓取检索命中的页面正文；**单页失败如实记"跳过+原因"，
   绝不编造页面内容**；抓取页数与跳过页数在纪要中显式计数；
3. **来源纪律**：纪要/报告中每个事实性论断逐条给来源 URL；查证不了的显式标
   "未核实"——不装作核实过；
4. **数字纪律**：报告里出现的、经你方运算得到的数字（合计/均值/折算等），
   先调确定性服务（numeric_*/gov_* 等）取得结果再入文，并进 provenance
   （同 evorule-deliverable 技能）；**页面上原文引用的数字不算运算**，标来源即可；
5. **交付**：纪要落 docx（export_document_docx）或按用户要的形态走 export_* 族，
   回报 {path, bytes, blake3} 信封。

## 标准动作序列（单会话路）

1. web_search 两轮 → 要点清单 + 待抓 URL 清单（3-8 条）；
2. browser_launch → 逐页 navigate+get_text → 每页摘要点（失败页记录跳过）；
3. 聚合纪要（要点/分歧/证据缺口/来源表）；
4. export_document_docx（来源表=URL 列；运算数字入 provenance）→ 回报信封。
