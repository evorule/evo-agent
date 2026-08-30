<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# Evo-Agent — 声明

**版权所有 (c) 2026 EvoRule Project**

本项目（`evo-agent`）是 EvoRule 框架的 AI Agent 编排层：以 evorule 引擎为确定性内核，
将 LLM 调用、工具调用、记忆读写全部转译为 `IoRequest` 事实，落入审计/回滚/回放轨道。

## 协议

**本仓（Evo-Agent）所有代码采用 AGPL-3.0-or-later + 商业双许可。** 详见 [LICENSE](LICENSE)。

> 许可分层说明：Evo-Agent 与 evorule-server / evorule 主仓同层，均为 AGPL-3.0 +
> 商业双许可；客户端集成（SDK 等宽松许可组件）通过 HTTP API 交互，不触发本仓协议义务。
> 商业授权请联系 <evorulelab@gmail.com>。

## 依赖说明

### EvoRule 生态依赖（path 依赖，随主仓许可）

| 依赖 | 来源 | 说明 |
|---|---|---|
| evorule-tcb | path：`../evorule/evorule-tcb` | 确定性执行内核（数据模型 / JSON 值） |
| evorule-reactor | path：`../evorule/evorule-reactor` | 反应器（事实日志 / 状态机） |
| evorule-server | [evorule-server 仓](https://gitee.com/evorule/evorule-server) | 运行期 HTTP 依赖（sidecar 会话 / 审计链） |

### 第三方依赖（主要直接依赖）

| 依赖 | 协议 | 用途 |
|---|---|---|
| `tokio` / `tokio-util` | MIT | 异步运行时 / 取消信号 |
| `axum` / `tower` / `tower-http` | MIT | HTTP API / WebSocket |
| `reqwest` | MIT / Apache-2.0 | HTTP 客户端（LLM 调用 / server API） |
| `serde` / `serde_json` / `serde_with` | MIT / Apache-2.0 | 序列化 |
| `clap` | MIT / Apache-2.0 | CLI 参数解析 |
| `tracing` / `tracing-subscriber` | MIT | 结构化日志 |
| `prometheus` | MIT / Apache-2.0 | 指标导出 |
| `jsonschema` | MIT | 结构化输出校验（G11） |
| `rustyline` | MIT | REPL 行编辑（G15） |
| `regex` | MIT / Apache-2.0 | L2 SafetyAuditor 规则匹配 |
| `subtle` | BSD-3-Clause | 恒定时间比较（G7） |
| `rand` | MIT / Apache-2.0 | 重试退避抖动（G3） |
| `mockito` / `tokio-tungstenite`（dev） | MIT | 测试设施 |

## 联系信息

- **项目**: Evo-Agent — AI Agent 编排层
- **作者**: EvoRule Project
- **邮箱**: <evorulelab@gmail.com>
- **组织**: [EvoRule Lab](https://gitee.com/evorule)
- **Gitee**: <https://gitee.com/evorule/evo-agent>
