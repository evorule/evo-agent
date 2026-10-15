//! 治理留痕入链与记忆消费面（B22，23 号档板块设计）
//!
//! A 项 治理留痕入链：工作台治理事件（审批请求/决定、policy 三态判定、
//! S4 产物定稿、terminal PTY 会话生命周期、B21 journal 里程碑镜像）以
//! Fact 形式写入 evorule 共享账本 `shared.gov.*`——「人的操作」进入记忆，
//! 可回放、可因果绑定、跨会话可查。
//!
//! - **会话载体**：serve 进程生命周期单 sidecar 会话（23 号档 §3.1 裁决：
//!   隔离不混 agent 事实流+单会话链内时序天然有序+重启自然分区）。
//!   serve 启动时 [`init_ledger`] 安装进程级单例；CLI 模式不初始化
//!   （治理事件=工作台 serve 面，调用面以 [`current_ledger`] 判空 no-op）。
//! - **写入策略**：best-effort——evorule 不可达时不阻断治理操作（前端
//!   govEvents 展示流照常），但必须留痕（[`tracing::warn`]，与 memory
//!   store「不静默」纪律一致）。
//! - **命名空间**：`ns=gov` 固定（23 号档 §八风险 3），与 `__memory__` /
//!   `shared.{agent_ns}` 隔离；路径 `shared.gov.{ns}.{event_kind}.{ts}.{seq}`。
//! - **隐私边界**：gov value 为摘要+引用级，不存指令/产物全文（全文已在
//!   journal 与会话事实，gov 只存治理摘要+指针，19 号口径）。
//!
//! 事件源接线（hooks）沿 PR2 交付；本模块（PR1）交付骨架：事件 schema、
//! seq 分配、路径构造、best-effort 写入与会话管理。

pub mod hooks;
pub mod ledger;

pub use hooks::GovAuditedApproval;
pub use ledger::{
    current_ledger, gov_event_id, gov_event_path, init_ledger, GovActor, GovEvent, GovEventKind,
    GovLedger, GovRefs, TerminalAction, GOV_CALLER_ROLE, GOV_NAMESPACE, GOV_SHARED_PREFIX,
};
