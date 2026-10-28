# AGENTS.md — agent 会话入场须知（本仓：evo-agent）

> 任何 AI agent 会话在本仓内工作前，先读此文件。

1. **对外披露规矩 = [PUBLIC-FACE.md](PUBLIC-FACE.md)**：内部编号（字母前缀-数字的私有登记号/档号）、私有库路径、真实密钥、内部运营流程、会话痕迹——**禁止出现在任何文件与提交信息**。CI 棘轮闸在役：存量已冻结，新违规即红。
2. **提交信息**用中性表述（feat/fix/docs/chore + 摘要），不带内部登记号；本机钩子即时拦截。
3. **仓入口**：README.md（及 DOCS_INDEX.md 如有）；改动遵守仓内 CONTRIBUTING/CI 门禁。

## 写码词面红线（AI agent 写码时即时自查）

写任何**注释、字符串字面量、文档、提交信息**之前，对照词表。红线由 `scripts/scan_public_face.py` 定义，完整模式清单见快照 [scripts/public-face-words.txt](scripts/public-face-words.txt)（词表变更后用 `python scripts/scan_public_face.py --dump-words > scripts/public-face-words.txt` 刷新）。

**阻断类（提交即拦，逐字避开）：**
- A1 AI 署名：助手工具代号/厂商身份词、联合署名 trailer（Co-Authored- 形态）、AI 做主语 + 创作/编写类动词的作者性表述（字面形态见快照 A1 类，引用时拆开写）
- B 内部编号：字母前缀连字符 + 数字的登记号/档号形态（`O-` + 两位数即为一例）、批次词 + W + 数字、「两位数 + 号」类内部档引用形态
- C 私有路径/仓名：本机盘符路径（盘符冒号反斜杠开头）、私有仓名字面量

**复核类（不拦但尽量避开）：** 治理口吻词与 LLM 厂商名——完整清单见快照 D/A2 类（治理叙事在注释与文档里用行为语言替代）。

**机械兜底：** 本机 `pre-commit` 钩子在提交时跑 `python scripts/scan_public_face.py --staged`（秒级，只扫暂存新增行，存量不背锅），命中阻断类当场拒绝——修词后重新 `git add` 再提交。钩子为本地工具不入仓，新 clone 部署方法见钩子文件内注释。

**经验法则：** 注释里写行为与意图（"审计链照常记录"），不写快照 D 类治理口吻；引用编号用语义描述（"存量问题登记册主键"）不写字面量。
