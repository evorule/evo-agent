---
name: video-screendemo
description: 录屏演示卡（M3）：演示脚本→录屏采集→合成检查。screen-demo 线 capture 段使用；synthetic 子路线（纯 CPU 确定性合成）同卡覆盖。
---

# 录屏演示卡

## 定位（何时用）

M3 screen-demo 线 capture_prep→capture_edit 段：演示脚本落位、录屏采集质量把关、合成检查。本卡管**演示呈现判断**（可见性/节奏/聚焦）；纯 synthetic 确定性合成子路线（无真录屏）共用本卡的合成检查面。

## 前置依赖（声明化引用）

- 引擎 CLI：`engine.py plan <project_dir>`（预演）、`engine.py build <project_dir>`（合成）、`engine.py timeline <project_dir>`（时间轴对位）
- 阶段链：brief→demo_script→shot_plan→capture_prep→capture_edit→demo_assembly→delivery
- 技能：`video-enhance-chain`（可见性增强判断同构——光标高亮/缩放属增强件）、`video-editing`（剪辑计划共用）、`video-reviewer`（复查）
- 谓词面：qc_resolution（录屏分辨率达标）/qc_text_legibility（关键 UI 文本可读性）——机器初筛引谓词名

## 作业流程

1. 演示脚本逐屏分解：操作流程拆成「每屏一动作」清单（点击什么/预期看到什么/停留几秒）——清单粒度即剪辑粒度，脚本含糊=录屏返工。
2. 采集前环境三定：分辨率定（1080p 起，目标平台规格）、缩放定（UI 字号在 720p 下仍可读）、干扰定（通知关闭/桌面清理/无关窗口关）——三定清单过才有资格按录制键。
3. 采集质量五查（每次录完即查，不攒批）：分辨率达标、帧率稳定（掉帧段标红）、鼠标轨迹可跟随、关键文本可读（qc_text_legibility 初筛）、无隐私泄露面（通知弹窗/地址栏用户名）。
4. 演示节奏判断：操作停留时长与观众理解速度匹配（新界面多留、熟练操作快进）——节奏判断写进剪辑输入包供剪辑卡参照。
5. synthetic 子路线合规：确定性合成（程序化产出画面而非真录屏）必须逐帧同源可复现——合成参数进台账，产物标注 synthetic（诚实呈现，不冒充实拍录屏）。
6. 隐私面终查：交付前全片扫一遍隐私面（用户名/邮箱/token 弹窗/内网地址）——命中即打码或重录，隐私事故零容忍。

## 质量判据

- 机器可查：qc_resolution/qc_text_legibility 全过；时间轴与 demo_script 逐屏对位；synthetic 产物可复现（同参数重跑同结果）。
- 人工判断：五查逐项有结论；节奏判断在案；隐私面终查通过记录。

## 闸提示（何时停待人审）

- 采集环境三定清单确认（首次录制前）；隐私面命中处置（打码 vs 重录成本归上游）；synthetic vs 实拍的路线确认（诚实呈现义务）。

## 边界（不做何事+降级路径）

- 不录未过三定清单的屏（返工比等待贵）；不私自决定隐私面打码标准（交上游裁）；synthetic 产物不标注实拍。
- 屏幕环境不可控（无法清干扰）时降级路径：列干扰清单交上游裁「接受干扰录制 vs 换环境」，不硬录。
