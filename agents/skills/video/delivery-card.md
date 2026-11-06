---
name: video-delivery
description: 视频发布交付卡：交付物齐套检查、台账核对、交付面呈现。verify 全绿后、正式交付上游/客户前使用；交付后归档也归本卡。
---

# 视频发布交付卡

## 定位（何时用）

成片 verify 全绿之后、交付动作之前，以及交付后归档。本卡管**交付齐套与呈现**；质量判断归 reviewer 卡，范围与闸判断归 producer 卡——三卡不得互相代行。

## 前置依赖（声明化引用）

- 引擎 CLI：`engine.py verify <project_dir>`（交付前置门——rc=0 才进交付）、`engine.py report <project_dir>`（报告生成）、`engine.py events <project_dir>`（事件流只读——交付前全程留痕核对）
- 产物契约：build/final.mp4、manifest.json、subs.srt/subs.ass、storyboard.xlsx、delivery.docx、report.pdf
- 技能：`video-producer`（闸呈现归其管）、`video-reviewer`（质量意见）、`evorule-deliverable`（交付纪律通则）

## 作业流程

1. 交付前置门：verify rc=0 且 durations.mismatch=false 才允许进交付段——红了不交付，回施工面修，不「带瑕交付再补」。
2. 齐套清单逐项点验：对照 project.json 需求基线，逐项核对七件套（成片/manifest/字幕双格式/分镜表/交付文档/报告）在 build/ 下存在且非空；缺件先补齐再谈交付。
3. AI 台账核对：五字段（模型版本/提示词摘要/输入来源/生成日期/修改说明）逐镜头与 manifest usage 互证——台账与 manifest 对不上即停，回查生成链，不凭印象签字。
4. 溯源完整性：AI 生成镜头逐条核对 usage.detail（provider/seed/comfy_prompt_id 等留痕字段）在案；provenance_only 标注在场——交付物里溯源面不全即红牌。
5. 交付包呈现：按上游要求的形态打包（网盘/邮件/仓内路径），交付说明写清：成片指纹（blake3/bytes）、时长、交付物清单、台账位置——三件齐呈（闸点、判据、证据路径），交 producer 卡走闸。
6. 归档：交付完成后核对 events 事件流全程在案，工程目录与 build/ 产物路径登记进交付记录，供后续回溯。

## 质量判据

- 机器可查：verify rc=0；七件套逐项存在且非空（文件大小>0）；manifest 可解析且 usage 全 ok=true。
- 人工判断：台账五字段与 manifest 互证一致；交付说明三件齐；归档路径可达。

## 闸提示（何时停待人审）

- 正式交付动作本身（producer 卡闸呈现后由上游签收）；台账互证失败；上游临时追加交付物（=范围变更，回摄取卡不私接）。

## 边界（不做何事+降级路径）

- 不跳过 verify 直接交付；不代 reviewer 出质量结论；不替上游签收；发现 build/ 产物被手工改动即停（产物只由引擎生成），报告 producer 卡处置。
- 上游渠道不可达时降级路径：交付包与说明备齐在仓内路径，标注「待送达」，不假装已交付、不删本地产物。
