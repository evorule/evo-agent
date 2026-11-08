// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! run 面自审自检器（总体设计 P-B 期：def 开关字段，缺省关 = 既有定义零变化）
//!
//! # 形态与边界
//! - **只读**：不写状态、不改配置、不产出 LLM 可调用工具——触发为轮间
//!   自动执行（Step 事件 = 步界安全点，工具执行间隙），LLM 不参与结论
//!   计算（判据全部确定性解析，经 evorule-server 原生服务执行）；
//! - **触发**：周期 N 步（`every_n_steps`，def 可配）。设计稿的「阶段边界
//!   为主」触发在 v1 无机械锚可依（阶段语义在 goal 原文，执行体不可语义
//!   判定——LLM 判阶段违反确定性铁律），候规划工件批次接锚后补齐；
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
    /// 周期触发（阶段锚候规划工件批次——v1 无机械阶段面）。
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
        Ok(())
    }
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
pub fn render_report(step: usize, services: &Value) -> String {
    let body = serde_json::to_string_pretty(services).unwrap_or_else(|_| "{}".to_string());
    let body: String = if body.chars().count() > REPORT_CAP_CHARS {
        let cut: String = body.chars().take(REPORT_CAP_CHARS).collect();
        format!("{cut}\n__truncated__")
    } else {
        body
    };
    format!(
        "[self-audit] periodic self-audit report (step {step})\n\
         Read-only observation: the findings below are deterministic trace-audit\n\
         results, not LLM-generated. detour != drift: benign detours are not\n\
         defects. Findings describe gaps only; the decision remains yours.\n{body}"
    )
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
        };
        let err = bad.validate().expect_err("空 scope 必须拒");
        assert!(err.contains("scope"));
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
        };
        let s2 = serde_json::to_string(&c_inject).unwrap();
        assert_eq!(s2, r#"{"every_n_steps":7,"inject":true}"#);
    }

    #[test]
    fn test_render_report_fixed_template_and_cap() {
        let services = serde_json::json!({ "entropy_w4": { "value": 0.5 } });
        let r1 = render_report(5, &services);
        let r2 = render_report(5, &services);
        assert_eq!(r1, r2, "同输入同输出（模板锁死）");
        assert!(r1.contains("detour != drift"), "定性行必须存在");
        assert!(r1.contains("step 5"));
        assert!(r1.contains("entropy_w4"));
        // 超限截断：大数组撑爆报告体
        let big: Vec<Value> = (0..5000)
            .map(|i| serde_json::json!({ "p": format!("D:/x/{i}") }))
            .collect();
        let huge = serde_json::json!({ "out_of_scope_writes": { "writes": big } });
        let r = render_report(1, &huge);
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
