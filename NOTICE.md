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

# EvoRule — 声明

**版权所有 (c) 2026 EvoRule Project**

本项目包含由 EvoRule Project 开发的软件。

## 协议分离

| 资产 | 协议 | 说明 |
|---|---|---|
| **代码** | AGPL-3.0-or-later | 详见 [LICENSE](LICENSE) |
| **`core_eval.json`** | **CC0 1.0 公共领域** | EvoRule 宪法(解释器规范)—— 任何人都可以自由实现兼容的 EvoRule 引擎,无需保留版权声明 |

**协议分离的战略意义**:
- 代码(copyleft,AGPL-3.0):保护 EvoRule 当前实现,阻止大厂"白嫖 fork 后卖闭源 SaaS"
- 宪法(public domain,CC0-1.0):鼓励广泛采用,任何人都可以基于宪法实现兼容引擎
- 这把"标准"和"实现"分开,类似 HTTP 规范(W3C 公共)vs Apache HTTP Server(版权)

## 设计哲学来源

EvoRule 的设计原则继承自早期 Python 版本 **evorule-core v0.2.0-beta**(2026-05-18):
- 规则即数据(可读、可审计、可序列化)
- 自解释引擎(解释器本身也是可被审计的规则)
- 完全可追溯(每次状态变化留下因果链)
- 零隐藏逻辑(解释器可读 + 可审计)
- 不可变状态(基于 frozendict/不可变数据结构)
- 确定性执行(相同输入 = 永远相同输出)

## 联系信息

- **项目**: EvoRule — 反应式执行引擎
- **作者**: EvoRule Project
- **邮箱**: evorulelab@gmail.com
- **组织**: [EvoRule Lab](https://gitee.com/evorulelab)
- **Gitee**: https://gitee.com/evorulelab/evorule

---

本声明遵循 evorule-core-backup 的发布原则。
