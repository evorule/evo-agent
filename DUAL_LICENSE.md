<!--
  Copyright 2026 EvoRule Project

  This program is free software: you can redistribute it and/or modify
  it under the terms of the GNU Affero General Public License as published by
  the Free Software Foundation, either version 3 of the License, or
  (at your option) any later version.

  This program is distributed in the hope that it will be useful,
  but WITHOUT ANY WARRANTY; without even the implied warranty of
  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
  GNU Affero General Public License for more details.

  You should have received a copy of the GNU Affero General Public License
  along with this program.  If not, see <https://www.gnu.org/licenses/>.

  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# EvoRule 双重许可说明

**版本**: 1.0
**生效日期**: 2026-07-19
**适用范围**: EvoRule 反应式执行引擎及其生态(evo-agent / SDK)

---

## 概述

EvoRule 采用**双轨许可模式**,为不同用户提供灵活选择:

1. **AGPL-3.0-or-later 开源许可** — 免费使用,适合开源项目和个人开发者
2. **商业许可** — 付费使用,适合企业闭源产品和商业应用

`core_eval.json`(EvoRule 宪法)采用 **CC0 1.0 公共领域**,独立于代码协议。

---

## AGPL-3.0-or-later 开源许可

### 适用场景

- ✅ 开源项目(必须同样采用 AGPL-3.0 或兼容许可证)
- ✅ 个人学习和研究
- ✅ 内部工具(不对外提供服务)
- ✅ 教育用途
- ✅ 非营利公益项目

### 主要义务

根据 AGPL-3.0-or-later 许可证,如果您:

- 修改了 EvoRule 代码
- 通过网络向用户提供服务

则您必须:

- 📄 公开您的源代码(包括修改部分)
- 🔗 提供获取源代码的方式
- 📝 保留原始版权声明和许可证

### 限制

- ❌ 不能将 EvoRule 用于闭源商业产品
- ❌ 不能在 SaaS 服务中使用而不公开源代码
- ❌ 不能移除或修改版权声明

---

## 商业许可

### 适用场景

- ✅ 企业闭源产品
- ✅ SaaS 服务(无需公开源代码)
- ✅ 商业软件集成
- ✅ 专有系统开发
- ✅ 需要技术支持和 SLA 保障

### 主要优势

- 🔒 **无需公开源代码** — 您可以在闭源产品中使用
- 🚀 **无 AGPL 传染性** — 您的代码不受 AGPL 约束
- 💼 **商业友好** — 适合企业级应用
- 🛡 **法律保护** — 获得明确的商业使用授权
- 🤝 **技术支持** — 可选的技术支持和咨询服务

### 定价方案

| 方案 | 价格 | 适用对象 |
|---|---|---|
| 初创企业 | 联系询价 | 年收入 < $1M 的公司 |
| 中小企业 | 联系询价 | 年收入 $1M-$10M 的公司 |
| 大型企业 | 联系询价 | 年收入 > $10M 的公司 |
| 教育机构 | 优惠价格 | 学校和科研机构 |
| 非营利组织 | **免费**(申请) | 见 [FREE_COMMERCIAL_LICENSE.md](FREE_COMMERCIAL_LICENSE.md) |

**联系方式**: evorulelab@gmail.com

---

## 协议分离(关键)

EvoRule 的"代码"和"宪法"采用**不同协议**:

| 资产 | 协议 | 说明 |
|---|---|---|
| EvoRule 代码(Rust) | AGPL-3.0-or-later | copyleft,保护当前实现 |
| `core_eval.json`(宪法) | **CC0 1.0 公共领域** | 解释器规范,任何人都可自由实现 |

**战略意义**:
- 代码 AGPL-3.0:阻止大厂"白嫖 fork 后卖闭源 SaaS"
- 宪法 CC0:鼓励广泛采用,任何人都可以基于宪法实现兼容的 EvoRule 引擎
- 这把"标准"和"实现"分开,类似 HTTP 规范(W3C 公共)vs Apache HTTP Server(版权)

---

## 常见问题

### Q1: 我可以在公司内部使用 AGPL 版本吗?

**A**: 可以。如果您的内部工具不对外部用户提供服务,可以使用 AGPL 版本而无需公开代码。但如果通过 Web 界面向员工提供服务,从严格的 AGPL 解释角度,可能需要公开代码。建议企业内部使用选择商业许可以避免法律风险。

### Q2: 商业许可是否包含技术支持?

**A**: 基础商业许可不包含技术支持,但可以购买额外的支持套餐。详情请咨询销售团队。

### Q3: 我可以从 AGPL 升级到商业许可吗?

**A**: 可以。您可以随时从 AGPL 切换到商业许可,只需联系销售团队即可。

### Q4: 商业许可是永久的还是订阅制?

**A**: 我们提供两种选项:
- **永久许可** — 一次性付费,永久使用该版本
- **订阅许可** — 年费制,包含所有更新和技术支持

### Q5: EvoRule 跟传统规则引擎有什么区别?

**A**: 关键区别是**透明性 + 可审计性 + JSON 唯一表达**。详见 [README.md](README.md)。AGPL-3.0 是这个定位的天然选择 — 因为可审计的前提是源码可读。

### Q6: 我可以基于 `core_eval.json` 实现自己的 EvoRule 兼容引擎吗?

**A**: **可以,这是 CC0 公共领域的目的**。您的实现只需在自己的代码上选自己的许可证(可以是 AGPL、商业、闭源),不需要回馈给本项目。但您的实现**不是** "EvoRule",只是 "EvoRule 兼容"。

### Q7: 自由职业者 / 个人开发者需要商业许可吗?

**A**: 不需要。如果您是个人使用、学习、内部工具,AGPL-3.0 即可。如果您把基于 EvoRule 的服务卖给客户,**才**需要商业许可。

---

## 联系方式

- **销售咨询**: evorulelab@gmail.com
- **技术支持**: evorulelab@gmail.com(同邮箱)
- **官方网站**: https://gitee.com/evorulelab/evorule
- **Gitee 组织**: https://gitee.com/evorulelab

---

## 法律声明

本文档**不构成法律建议**。如有法律疑问,请咨询专业律师。

EvoRule 的知识产权归 EvoRule Project 所有。

---

## 版本历史

| 版本 | 日期 | 变更说明 |
|---|---|---|
| 1.0 | 2026-07-19 | 初版,基于 evorule-core-backup v0.2.0-beta 的 DUAL_LICENSE 适配 |

---

**最后更新**: 2026-07-19
**文档版本**: 1.0

*本协议遵循 evorule-core-backup 的发布原则。*
