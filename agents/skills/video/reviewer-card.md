---
name: video-reviewer
description: 视频审评卡：CHAI 三性审评（准确/完整/建设性），机器谓词取证与人工判断分账。成片 verify 通过后、交付上游前，或任一阶段产物需要质量判断时使用。
---

# 视频审评卡

## 定位（何时用）

任一阶段产物需要质量判断时在岗：镜头候选挑选、成片验收、上游退回后的整改复核。本卡管**判断与意见**；机器取证由引擎 verify 与 qc 谓词管——审评意见必须引用机器证据，不做无证据断言。

## 前置依赖（声明化引用）

- 引擎 CLI：`engine.py verify <project_dir>`（交叉核对，审评的机器证据源）、`engine.py report <project_dir>`（报告）、`engine.py review <project_dir>`（候选拼图审阅面）
- 产物：build/manifest.json（durations/usage/cues）、build/review/（候选拼图）
- 技能：`video-producer`（阶段判断与闸呈现归其管，本卡只出质量意见）

## 作业流程

1. 先取机器证据：跑 verify 确认 rc=0；读 manifest 的 durations.mismatch 与 usage 逐条 ok。机器证据红时直接出「不通过」意见并附证据路径，不进入人工判断段。
2. CHAI 准确性：逐镜头对照 project.json 镜头表——画面内容、字幕文本、时长是否与声明一致；引用具体镜头 ID、manifest 字段、帧号，不写「大概没问题」。
3. CHAI 完整性：对照需求清单查漏项——缺字幕、缺交付物、漏镜头。漏检比误报更糟：宁可多列疑点交上游裁，不静默放过。
4. CHAI 建设性：每条 critical 意见必须附可执行的修法（改哪个文件、跑哪条命令、重生成哪个镜头），不出只有否定的意见。
5. 分账出意见：机器可查的引 verify/manifest 字段名；人工判断的（叙事节奏、画面美感、文案语气）明确标注「判断面」，与机器面分开列。
6. 意见落档：审评意见写入 review 记录，逐条带证据路径，供 producer 卡在闸呈现时引用。

## 质量判据

- 机器可查：verify rc=0；manifest durations.mismatch=false；usage 全 ok=true。
- 人工判断：意见逐条有证据路径；critical 全部附修法；判断面与机器面分账清晰无混写。

## 闸提示（何时停待人审）

- verify 报 mismatch=true；usage 有 ok=false；候选间差异大到需要人选（`engine.py pick` 之前）；意见与上游预期冲突。

## 边界（不做何事+降级路径）

- 不直接改产物与工程文件（意见归意见，施工归施工）；不替 producer 做阶段推进决定；不跳过机器证据直接出「凭感觉」意见。
- 引擎不可用时降级路径：只做纸面审评（工程文件与需求清单对照），明确标注「未经 verify 背书」，不伪造机器证据。
