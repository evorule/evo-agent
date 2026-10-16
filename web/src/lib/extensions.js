// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// 扩展面板 API 消费封装(形制补全 F2):技能包审批链/代理定义列表/MCP 摘要。
// 全部为既有 serve 端点的只读/审批消费,零新后端。API 包装器在 api.js。

export { listSkills, listPendingSkills, approveSkill, revokeSkill } from './api.js';
export { getOrchestrationAgents as listAgentDefs } from './api.js';
export { mcpStatus as mcpSummary } from './api.js';
