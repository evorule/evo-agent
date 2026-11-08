// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! run 面自审自检器（总体设计 P-B 期：def 开关字段，缺省关 = 既有定义零变化）
//!
//! # 形态与边界
//! - **只读**：不写状态、不改配置、不产出 LLM 可调用工具——触发为轮间
//!   自动执行（Step 事件 = 步界安全点，工具执行间隙），LLM 不参与结论
//!   计算（判据全部确定性解析，经 evorule-server 原生服务执行）；
//! - **触发**：双源——阶段锚精确为主（`stages[].at_step` 预声明步号，
//!   D1 工件批机械锚：步界 = Step 事件序，确定性可判，无需语义探测）+
//!   周期 N 步兜底（`every_n_steps`，既有不动）；同步双命中 = 一次自审
//!   （trigger 记 stage）。阶段语义边界探测留后续迭代；
//! - **数据源**：进程内事件帧缓冲（`AgentEvent` 流的 JSON 帧同构面，与
//!   CLI `run --json` / WS 帧逐字节同构，单一事实源 `agent_event_to_json`）；
//! - **消费**：trace-audit 只读服务族五件（熵 w4/w10、密度、分代、哨兵、
//!   范围外写入），经 runner 既有 evorule_client 泛化直调服务 invoke 端点
//!   （`/api/services/` 前缀）；
//! - **落账**：结论装配方 self_audit journal 事件（确定性摘要，大数组
//!   截断标记）；审计只读非关键路径——任何服务失败降级落账，不杀 run；
//! - **回喂**：缺省不回喂（观测面）；def `self_audit.inject` 显式开启后
//!   触发时把固定模板报告经系统消息分区注入对话（同结论去重 + 大小上限
//!   锁死，报告带 detour≠drift 定性、只描述差口不替决策）；
//! - **缺省不变式**：`self_audit` 字段缺省 None = 帧缓冲零采集、零触发、
//!   零事件（序列化面无字段）。
//!
//! # 已知边界（诚实登记）
//! - 非流式 `run()`（delegate 子代理路径）未接线——v1 覆盖流式主循环
//!   （CLI run 无人值守场景即此路径），delegate 短任务接线候后续批；
//! - scope 缺省不检范围外写入（def 显式声明才启用，正本 CWD 语义在
//!   服务面诚实化为 base_dir=scope，见服务文档）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::runner::AgentEvent;
use crate::api::evorule_client::EvoruleApiClient;

/// 自审配置（def `self_audit` 字段；None = 自检器不存在，既有定义零影响）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SelfAuditConfig {
    /// 周期触发步距：每 N 步（步界 = Step 事件）触发一次自审。必须 >0
    /// （装载期 fail-fast 校验，0 = 定义非法拒绝启动）。None = 不启用
    /// 周期触发（阶段锚见 `stages`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_n_steps: Option<u32>,
    /// 任务范围根（范围外写入检出的 scope 入参；None = 该判据跳过并在
    /// 结论中标注 skipped）。显式声明激活——正本 CWD 语义在服务面诚实化
    /// 为 base_dir=scope（执行体帧写路径为字面量，非链接形态）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// 结论注入开关（P-C 注入机制面；None/false[缺省] = 只落账不回喂 =
    /// P-B 行为零变化；true = 触发时把固定模板报告经系统消息分区注入对话，
    /// 同结论去重 + 报告体大小上限锁死）。注入文本模板固定（LLM 不可参与
    /// 结论计算，报告只描述差口不替决策，带 detour≠drift 定性）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject: Option<bool>,
    /// 阶段锚声明（D1 阶段锚机械面；缺省空 = 无阶段锚 = 触发形态与既有
    /// 周期面零变化）。`at_step` 预声明步号 = 确定性阶段锚：Step 事件序
    /// 精确命中触发一次自审（无需语义探测），优先于 `every_n_steps` 周期
    /// 兜底；同步双命中 = 一次自审（trigger 记 stage）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<StageSpec>,
}

/// 阶段锚声明（def `self_audit.stages` 数组项）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StageSpec {
    /// 阶段 id（journal `stage` 字段与报告头引用；非空）
    pub id: String,
    /// 阶段步号（Step 事件序，从 1 起；精确命中触发一次自审。装载期
    /// fail-fast：0 / 重复 at_step 拒——每步至多一个阶段锚）
    pub at_step: u32,
    /// 阶段目标原文（报告头引用；可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_text: Option<String>,
}

impl SelfAuditConfig {
    /// 装载期校验（fail-fast：非法值拒绝启动，不静默回落）
    pub fn validate(&self) -> Result<(), String> {
        if let Some(n) = self.every_n_steps {
            if n == 0 {
                return Err(
                    "self_audit.every_n_steps 必须为正整数（0 = 非法；不启用周期触发\
                     应省略该字段）"
                        .to_string(),
                );
            }
        }
        if let Some(scope) = &self.scope {
            if scope.trim().is_empty() {
                return Err(
                    "self_audit.scope 不可为空字符串（不检范围外写入应省略该字段）".to_string(),
                );
            }
        }
        let mut seen = std::collections::HashSet::new();
        for stage in &self.stages {
            if stage.at_step == 0 {
                return Err(
                    "self_audit.stages[].at_step 必须为正整数（步号从 1 起，0 = 非法；\
                     不启用阶段锚应省略 stages）"
                        .to_string(),
                );
            }
            if !seen.insert(stage.at_step) {
                return Err(format!(
                    "self_audit.stages[].at_step 重复（{} 步只能声明一个阶段锚）",
                    stage.at_step
                ));
            }
        }
        Ok(())
    }

    /// def 对账锚：`self_audit` 配置序列化体的 evorule-hash 口径 digest
    /// （journal SelfAudit.plan_hash 落账值；配置任何字段变化 = 锚变，
    /// def 字节级对账——报告/journal 与定义版本可离线互证）。
    pub fn plan_hash(&self) -> String {
        let body = serde_json::to_vec(self).unwrap_or_default();
        evorule_hash::prefixed(&evorule_hash::digest(&body))
    }
}

/// 阶段锚命中判定（纯函数，可测）：step 与某声明的 at_step 精确相等 =
/// 命中该阶段（validate 拒重复 at_step，每步至多命中一个）。
pub fn stage_hit(cfg: &SelfAuditConfig, step: usize) -> Option<&StageSpec> {
    cfg.stages.iter().find(|s| s.at_step as usize == step)
}

/// 触发形态（双源判定结果；阶段锚精确优先，周期兜底）。
#[derive(Debug, Clone, PartialEq)]
pub enum TriggerKind<'a> {
    /// 阶段锚精确命中（trigger 落账记 "stage"）
    Stage(&'a StageSpec),
    /// 周期兜底命中（trigger 落账记 "periodic"，既有形态）
    Periodic,
}

/// 步界触发判定（双源合一，纯函数，可测）：阶段锚命中优先于周期兜底；
/// 都未命中 = None（该步不自审）。
pub fn trigger_kind<'a>(cfg: &'a SelfAuditConfig, step: usize) -> Option<TriggerKind<'a>> {
    if let Some(stage) = stage_hit(cfg, step) {
        return Some(TriggerKind::Stage(stage));
    }
    if should_trigger(cfg, step) {
        return Some(TriggerKind::Periodic);
    }
    None
}

/// 周期触发判定（纯函数，可测）：step 从 1 起，每 N 步命中。
pub fn should_trigger(cfg: &SelfAuditConfig, step: usize) -> bool {
    match cfg.every_n_steps {
        Some(n) if n > 0 => step > 0 && step.is_multiple_of(n as usize),
        _ => false,
    }
}

/// 进程内事件帧缓冲（wrapper 采集 / inner 触发消费，同任务串行访问）。
/// cfg 为 None（缺省）时 `capture` 零开销直返——缺省不变式守门。
pub struct SelfAuditFrameBuffer {
    cfg: Option<SelfAuditConfig>,
    frames: std::sync::Mutex<Vec<Value>>,
    /// 上次渲染报告（P-C 注入去重状态；None = 尚未渲染过）
    last_report: std::sync::Mutex<Option<String>>,
}

impl SelfAuditFrameBuffer {
    /// 构造（run_streaming 装配点；cfg 缺省 None = 零采集）
    pub fn new(cfg: Option<SelfAuditConfig>) -> Self {
        Self {
            cfg,
            frames: std::sync::Mutex::new(Vec::new()),
            last_report: std::sync::Mutex::new(None),
        }
    }

    /// 采集一帧（帧同构面 = `agent_event_to_json` 单一事实源；cfg None 时零开销）
    pub fn capture(&self, event: &AgentEvent) {
        if self.cfg.is_none() {
            return;
        }
        let frame = crate::api::ws_handler::agent_event_to_json(Ok(event.clone()));
        if let Ok(mut g) = self.frames.lock() {
            g.push(frame);
        }
    }

    /// 是否启用（trigger 侧快速门）
    pub fn enabled(&self) -> bool {
        self.cfg.is_some()
    }

    /// 配置引用（触发判定用）
    pub fn config(&self) -> Option<&SelfAuditConfig> {
        self.cfg.as_ref()
    }

    /// 当前帧快照（触发时克隆；采集与触发同任务串行，锁无争用）
    pub fn snapshot(&self) -> Vec<Value> {
        self.frames
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// P-C 注入去重判定：报告与上次渲染不同（或首次）= true 并记录；
    /// 相同 = false（同结论不重复注入，频控面）。仅注入开启时调用。
    pub fn report_changed(&self, report: &str) -> bool {
        let mut g = self.last_report.lock().unwrap_or_else(|p| p.into_inner());
        let changed = g.as_deref() != Some(report);
        if changed {
            *g = Some(report.to_string());
        }
        changed
    }
}

/// 帧同构面单一事实源在 `crate::api::ws_handler::agent_event_to_json`
/// （CLI run --json 与 WS 帧共用此映射，同构断言测试锁定）——采集面直接
/// 复用，不另设第二序列化路径。
///
/// journal 摘要截断阈值（超限数组只留头部，总长计入截断标记；
/// 结论为确定性摘要，全量可由帧流离线重算——审计可回放性由帧面+journal
/// 双面保证）
const SUMMARY_ARRAY_CAP: usize = 100;

/// 服务出参压缩：大数组截断（journal 落账尺寸纪律），标量全保留。
pub fn compact_for_summary(value: &Value) -> Value {
    match value {
        Value::Array(items) => {
            if items.len() > SUMMARY_ARRAY_CAP {
                let mut head: Vec<Value> = items.iter().take(SUMMARY_ARRAY_CAP).cloned().collect();
                head.push(Value::String(format!(
                    "__truncated_{}_of_{}__",
                    SUMMARY_ARRAY_CAP,
                    items.len()
                )));
                Value::Array(head)
            } else {
                value.clone()
            }
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k.clone(), compact_for_summary(v));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// 执行一次自审：五件只读服务顺序直调（熵 w4/w10、密度、分代、哨兵、
/// 范围外写入），逐件降级（单件失败记 error 不中断）。
/// 返回 services 装配方（journal 落账载荷的 `services` 字段）。
pub async fn invoke_audit_services(
    client: &EvoruleApiClient,
    cfg: &SelfAuditConfig,
    frames: &[Value],
) -> Value {
    let mut out = serde_json::Map::new();
    // 熵 w4/w10（window 入参；缺省 10）
    for w in [4usize, 10] {
        let params = serde_json::json!({ "events": frames, "window": w });
        match client
            .invoke_service("trace_behavior_entropy", &params)
            .await
        {
            Ok(v) => {
                out.insert(format!("entropy_w{w}"), compact_for_summary(&v));
            }
            Err(e) => {
                out.insert(
                    format!("entropy_w{w}"),
                    serde_json::json!({ "error": e.to_string() }),
                );
            }
        }
    }
    // 审批密度 + 分代切分 + 时代混杂哨兵（单入参）
    for (key, service) in [
        ("density", "trace_approval_density"),
        ("era_split", "trace_era_split"),
        ("era_mixing_sentinel", "trace_era_mixing_sentinel"),
    ] {
        let params = serde_json::json!({ "events": frames });
        match client.invoke_service(service, &params).await {
            Ok(v) => {
                out.insert(key.to_string(), compact_for_summary(&v));
            }
            Err(e) => {
                out.insert(
                    key.to_string(),
                    serde_json::json!({ "error": e.to_string() }),
                );
            }
        }
    }
    // 范围外写入（scope 显式声明才启用；None = skipped）
    match &cfg.scope {
        Some(scope) => {
            let params = serde_json::json!({
                "events": frames,
                "scope": scope,
                "base_dir": scope,
            });
            match client
                .invoke_service("trace_out_of_scope_writes", &params)
                .await
            {
                Ok(v) => {
                    out.insert("out_of_scope_writes".to_string(), compact_for_summary(&v));
                }
                Err(e) => {
                    out.insert(
                        "out_of_scope_writes".to_string(),
                        serde_json::json!({ "error": e.to_string() }),
                    );
                }
            }
        }
        None => {
            out.insert(
                "out_of_scope_writes".to_string(),
                serde_json::json!({ "skipped": true }),
            );
        }
    }
    Value::Object(out)
}

/// 注入报告体字符上限（模板锁死纪律：报告为确定性摘要，全量可由帧流
/// 离线重算；超限截断标记收尾）
const REPORT_CAP_CHARS: usize = 4000;

/// P-C 注入报告渲染（模板固定，LLM 不可参与结论计算）：头部定性
/// （只读观测面 + detour≠drift + 只描述差口不替决策）+ services 摘要体。
/// 阶段锚命中时头部带阶段 id/goal_text（periodic 兜底 = 既有形态不变）。
pub fn render_report(step: usize, services: &Value, stage: Option<&StageSpec>) -> String {
    let body = serde_json::to_string_pretty(services).unwrap_or_else(|_| "{}".to_string());
    let body: String = if body.chars().count() > REPORT_CAP_CHARS {
        let cut: String = body.chars().take(REPORT_CAP_CHARS).collect();
        format!("{cut}\n__truncated__")
    } else {
        body
    };
    let head = match stage {
        Some(s) => match &s.goal_text {
            Some(goal) => format!(
                "[self-audit] stage self-audit report (step {step}, stage \"{}\": {goal})",
                s.id
            ),
            None => format!(
                "[self-audit] stage self-audit report (step {step}, stage \"{}\")",
                s.id
            ),
        },
        None => format!("[self-audit] periodic self-audit report (step {step})"),
    };
    format!(
        "{head}\n\
         Read-only observation: the findings below are deterministic trace-audit\n\
         results, not LLM-generated. detour != drift: benign detours are not\n\
         defects. Findings describe gaps only; the decision remains yours.\n{body}"
    )
}

/// 步界自审检查点（run_streaming 步界统一入口；引擎 IoRequest 步界与本地
/// ReAct 回喂步界共用）：触发判定（阶段锚精确优先，周期兜底）→五件只读
/// 服务直调→注入判定（P-C，缺省关）→journal 落账。全链 best-effort：
/// 任何失败降级不杀 run。
#[allow(clippy::too_many_arguments)]
pub async fn audit_checkpoint(
    client: &EvoruleApiClient,
    frame_buffer: &std::sync::Arc<SelfAuditFrameBuffer>,
    messages: &mut Vec<crate::agent::translator::Message>,
    journal: &Option<std::sync::Arc<crate::agent::journal::JournalWriter>>,
    session_id: &str,
    step_count: usize,
) {
    if !frame_buffer.enabled() {
        return;
    }
    let Some(sa_cfg) = frame_buffer.config() else {
        return;
    };
    let Some(trigger) = trigger_kind(sa_cfg, step_count) else {
        return;
    };
    let (trigger_name, stage) = match &trigger {
        TriggerKind::Stage(s) => ("stage", Some(*s)),
        TriggerKind::Periodic => ("periodic", None),
    };
    let frames = frame_buffer.snapshot();
    let frames_count = frames.len();
    let audit_started = std::time::Instant::now();
    let services = invoke_audit_services(client, sa_cfg, &frames).await;
    let audit_ms = audit_started.elapsed().as_millis() as u64;
    // P-C 注入机制面（缺省关=只落账不回喂）：模板渲染+同结论去重后经
    // 系统消息分区注入对话，LLM 下轮可见。
    let mut injected = false;
    if sa_cfg.inject == Some(true) {
        let report = render_report(step_count, &services, stage);
        if frame_buffer.report_changed(&report) {
            messages.push(crate::agent::translator::Message::System { content: report });
            injected = true;
        }
    }
    if let Some(j) = journal {
        if let Err(e) = j.record_self_audit(
            trigger_name,
            step_count as u64,
            frames_count,
            audit_ms,
            injected,
            stage.map(|s| s.id.as_str()),
            Some(&sa_cfg.plan_hash()),
            services,
        ) {
            tracing::warn!(session_id = %session_id, error = %e, "self_audit journal write failed (ignored)");
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn cfg(n: Option<u32>) -> SelfAuditConfig {
        SelfAuditConfig {
            every_n_steps: n,
            scope: None,
            inject: None,
            stages: Vec::new(),
        }
    }

    fn stage(id: &str, at: u32, goal: Option<&str>) -> StageSpec {
        StageSpec {
            id: id.to_string(),
            at_step: at,
            goal_text: goal.map(str::to_string),
        }
    }

    #[test]
    fn test_should_trigger_periodic() {
        let c = cfg(Some(5));
        assert!(!should_trigger(&c, 0));
        assert!(should_trigger(&c, 5));
        assert!(!should_trigger(&c, 7));
        assert!(should_trigger(&c, 10));
        assert!(!should_trigger(&cfg(None), 5), "None = 不触发");
        assert!(
            !should_trigger(&cfg(Some(0)), 5),
            "0 视为关闭（装载期已拒，此处防御）"
        );
    }

    #[test]
    fn test_validate_fail_fast() {
        assert!(cfg(Some(5)).validate().is_ok());
        assert!(cfg(None).validate().is_ok());
        let err = cfg(Some(0)).validate().expect_err("0 必须拒");
        assert!(err.contains("every_n_steps"));
        let bad = SelfAuditConfig {
            every_n_steps: Some(1),
            scope: Some("  ".to_string()),
            inject: None,
            stages: Vec::new(),
        };
        let err = bad.validate().expect_err("空 scope 必须拒");
        assert!(err.contains("scope"));
    }

    #[test]
    fn test_validate_stage_rules() {
        // at_step=0 拒（步号从 1 起）
        let mut c = cfg(None);
        c.stages = vec![stage("setup", 0, None)];
        let err = c.validate().expect_err("at_step=0 必须拒");
        assert!(err.contains("at_step"));
        // 重复 at_step 拒（每步至多一个阶段锚）
        let mut c = cfg(None);
        c.stages = vec![stage("a", 3, None), stage("b", 3, None)];
        let err = c.validate().expect_err("重复 at_step 必须拒");
        assert!(err.contains("重复"));
        // 合法声明放行
        let mut c = cfg(None);
        c.stages = vec![stage("setup", 2, Some("install")), stage("verify", 9, None)];
        assert!(c.validate().is_ok());
    }

    #[test]
    fn test_stage_hit_and_trigger_kind_priority() {
        // 无阶段锚 = stage_hit 永不命中（既有周期面零变化）
        let c = cfg(Some(5));
        assert!(stage_hit(&c, 5).is_none());
        assert_eq!(trigger_kind(&c, 5), Some(TriggerKind::Periodic));
        assert_eq!(trigger_kind(&c, 7), None);
        // 阶段锚精确命中
        let mut only = cfg(None);
        only.stages = vec![stage("setup", 3, None)];
        assert_eq!(stage_hit(&only, 3).map(|s| s.id.as_str()), Some("setup"));
        assert!(stage_hit(&only, 2).is_none());
        assert!(stage_hit(&only, 4).is_none());
        assert!(matches!(
            trigger_kind(&only, 3),
            Some(TriggerKind::Stage(_))
        ));
        // 同步双命中 = 阶段锚优先（精确为主，周期兜底）
        let mut both = cfg(Some(3));
        both.stages = vec![stage("setup", 3, None)];
        match trigger_kind(&both, 3) {
            Some(TriggerKind::Stage(s)) => assert_eq!(s.id, "setup"),
            other => panic!("阶段锚必须优先，got {other:?}"),
        }
        // 非锚步走周期兜底
        assert_eq!(trigger_kind(&both, 6), Some(TriggerKind::Periodic));
    }

    #[test]
    fn test_plan_hash_stable_and_sensitive() {
        let a = cfg(Some(7));
        let b = cfg(Some(7));
        assert_eq!(a.plan_hash(), b.plan_hash(), "同配置同锚（确定性）");
        let mut c = a.clone();
        c.stages = vec![stage("setup", 2, None)];
        assert_ne!(a.plan_hash(), c.plan_hash(), "配置变化 = 锚变");
        // 锚形态 = evorule-hash 口径前缀
        assert!(a.plan_hash().starts_with("blake3:"));
    }

    #[test]
    fn test_serde_default_zero_change() {
        // 缺省 None：无 self_audit 字段的定义 JSON 解析后无字段、序列化无字段
        let v: Value = serde_json::from_str("{}").unwrap();
        let parsed: Option<SelfAuditConfig> =
            serde_json::from_value(v.get("self_audit").cloned().unwrap_or(Value::Null))
                .unwrap_or(None);
        assert!(parsed.is_none());
        let c = cfg(Some(7));
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(s, r#"{"every_n_steps":7}"#);
        // inject 缺省 None：序列化无字段
        assert!(!s.contains("inject"));
        let c_inject = SelfAuditConfig {
            every_n_steps: Some(7),
            scope: None,
            inject: Some(true),
            stages: Vec::new(),
        };
        let s2 = serde_json::to_string(&c_inject).unwrap();
        assert_eq!(s2, r#"{"every_n_steps":7,"inject":true}"#);
        // stages 缺省空：序列化无字段；声明后可回环（goal_text None 不落字段）
        assert!(!s2.contains("stages"));
        let c_stage = SelfAuditConfig {
            every_n_steps: None,
            scope: None,
            inject: None,
            stages: vec![
                stage("setup", 2, Some("install deps")),
                stage("verify", 9, None),
            ],
        };
        let s3 = serde_json::to_string(&c_stage).unwrap();
        assert_eq!(
            s3,
            r#"{"stages":[{"id":"setup","at_step":2,"goal_text":"install deps"},{"id":"verify","at_step":9}]}"#
        );
        let back: SelfAuditConfig = serde_json::from_str(&s3).unwrap();
        assert_eq!(back, c_stage, "stages 声明回环无损");
    }

    #[test]
    fn test_render_report_fixed_template_and_cap() {
        let services = serde_json::json!({ "entropy_w4": { "value": 0.5 } });
        let r1 = render_report(5, &services, None);
        let r2 = render_report(5, &services, None);
        assert_eq!(r1, r2, "同输入同输出（模板锁死）");
        assert!(r1.contains("detour != drift"), "定性行必须存在");
        assert!(r1.contains("periodic"), "periodic 兜底 = 既有形态不变");
        assert!(r1.contains("step 5"));
        assert!(r1.contains("entropy_w4"));
        // 阶段锚命中头部：带阶段 id / goal_text
        let s = stage("setup", 3, Some("install deps"));
        let rs = render_report(3, &services, Some(&s));
        assert!(rs.contains("stage self-audit report"), "阶段头部形态");
        assert!(rs.contains("stage \"setup\": install deps"));
        assert!(!rs.contains("periodic"));
        let s_no_goal = stage("verify", 9, None);
        let rn = render_report(9, &services, Some(&s_no_goal));
        assert!(rn.contains("stage \"verify\""));
        // 超限截断：大数组撑爆报告体
        let big: Vec<Value> = (0..5000)
            .map(|i| serde_json::json!({ "p": format!("D:/x/{i}") }))
            .collect();
        let huge = serde_json::json!({ "out_of_scope_writes": { "writes": big } });
        let r = render_report(1, &huge, None);
        // 报告体=头部模板+截断到 4000 的 JSON 体+截断标记
        assert!(r.chars().count() < REPORT_CAP_CHARS + 512, "截断生效");
        assert!(r.ends_with("__truncated__"));
        assert!(r.starts_with("[self-audit]"), "头部模板保留");
    }

    #[test]
    fn test_report_changed_dedup() {
        let buf = SelfAuditFrameBuffer::new(Some(cfg(Some(5))));
        assert!(buf.report_changed("A"), "首次 = 变更");
        assert!(!buf.report_changed("A"), "同结论去重");
        assert!(buf.report_changed("B"), "结论变化 = 再注入");
        assert!(!buf.report_changed("B"));
    }

    #[test]
    fn test_compact_truncates_large_arrays() {
        let big: Vec<Value> = (0..250).map(Value::from).collect();
        let input = serde_json::json!({ "a": 1, "list": big, "nested": { "list2": big } });
        let out = compact_for_summary(&input);
        let list = out.get("list").and_then(Value::as_array).unwrap();
        assert_eq!(list.len(), 101, "100 头部 + 1 截断标记");
        assert!(list[100]
            .as_str()
            .unwrap()
            .starts_with("__truncated_100_of_250__"));
        let list2 = out
            .get("nested")
            .unwrap()
            .get("list2")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(list2.len(), 101);
        assert_eq!(out.get("a").and_then(Value::as_i64), Some(1));
        // 小数组不截断
        let small = serde_json::json!({ "l": [1, 2, 3] });
        assert_eq!(compact_for_summary(&small), small);
    }
}
