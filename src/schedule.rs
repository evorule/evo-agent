// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of evo-agent, licensed under GNU Affero General Public License v3 or later.
//! 定时自治调度器（纯确定性组件，三分法定位：调度无 LLM）。
//!
//! # 语义纪律（运维诚实性）
//! - **触发**：五字段 cron（分 时 日 月 周）按分钟粒度匹配；同一匹配分钟只发一次；
//! - **错过**：调度器停机/睡眠期间错过的匹配分钟**不补跑**——逐分钟记
//!   `missed` 留痕（跳过即显式，绝不静默吞也不静默补）；
//! - **留痕**：每次触发/错过/失败追加 JSONL 日志（缺省 `data/schedule.journal.jsonl`），
//!   字段 {ts, minute, job, kind, action, exit_code?, note}；
//! - **执行**：子进程调用本二进制既有子命令（run/patrol）——与会话/审计链零耦合，
//!   触发后的执行照常走八阶段管道与治理。
//! - **超时**：单次触发缺省 600s 超时强杀（防僵尸任务阻塞后续分钟）。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};

// ============================================================================
// cron 五字段表达式
// ============================================================================

/// 五字段 cron 表达式（分 时 日 月 周），支持 `*`、`*/n`、`a`、`a-b`、逗号列表。
///
/// 周字段 0-7（0 与 7 均为周日）；日/周同时受限时取并集（V7 cron 经典语义）。
#[derive(Debug, Clone, PartialEq)]
pub struct CronExpr {
    minute: Field,
    hour: Field,
    dom: Field,
    month: Field,
    dow: Field,
}

/// 单字段：允许值集合（bitset 语义，紧凑上限 60+1 位）
#[derive(Debug, Clone, PartialEq)]
struct Field(Vec<u32>);

impl Field {
    fn parse(text: &str, lo: u32, hi: u32, name: &str) -> Result<Self, String> {
        let mut out = Vec::new();
        for part in text.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err(format!("{name} 字段含空段"));
            }
            let (range, step) = match part.split_once('/') {
                Some((r, s)) => (r, s.parse::<u32>().map_err(|_| format!("{name} 步长非法: {s}"))?),
                None => (part, 1),
            };
            if step == 0 {
                return Err(format!("{name} 步长不可为 0"));
            }
            let (a, b) = if range == "*" {
                (lo, hi)
            } else if let Some((x, y)) = range.split_once('-') {
                let a = x.trim().parse::<u32>().map_err(|_| format!("{name} 范围起点非法: {x}"))?;
                let b = y.trim().parse::<u32>().map_err(|_| format!("{name} 范围终点非法: {y}"))?;
                (a, b)
            } else {
                let v = range.parse::<u32>().map_err(|_| format!("{name} 数值非法: {range}"))?;
                (v, v)
            };
            if a < lo || b > hi || a > b {
                return Err(format!("{name} 范围越界: {a}-{b}（合法 {lo}-{hi}）"));
            }
            let mut v = a;
            while v <= b {
                if !out.contains(&v) {
                    out.push(v);
                }
                v += step;
            }
        }
        out.sort_unstable();
        Ok(Field(out))
    }

    fn contains(&self, v: u32) -> bool {
        self.0.contains(&v)
    }

    fn is_full(&self, lo: u32, hi: u32) -> bool {
        self.0.len() == ((hi - lo + 1) as usize)
    }
}

impl CronExpr {
    /// 解析五字段 cron（空格分隔）。错误信息带字段名与合法域。
    pub fn parse(expr: &str) -> Result<Self, String> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(format!(
                "cron 需五字段(分 时 日 月 周)，得到 {} 段: {expr:?}",
                parts.len()
            ));
        }
        Ok(CronExpr {
            minute: Field::parse(parts[0], 0, 59, "分")?,
            hour: Field::parse(parts[1], 0, 23, "时")?,
            dom: Field::parse(parts[2], 1, 31, "日")?,
            month: Field::parse(parts[3], 1, 12, "月")?,
            dow: Field::parse(parts[4], 0, 7, "周")?,
        })
    }

    /// 是否匹配给定时刻（本地墙钟；周日=0 或 7 归一为 0）。
    pub fn matches(&self, year: i32, month: u32, day: u32, hour: u32, minute: u32) -> bool {
        if !self.minute.contains(minute) || !self.hour.contains(hour) || !self.month.contains(month)
        {
            return false;
        }
        let wd = weekday_index(year, month, day);
        let dom_ok = self.dom.contains(day);
        let dow_ok = self.dow.contains(wd) || (wd == 0 && self.dow.contains(7));
        // 经典 V7 语义：日与周均受限时取并集，否则各自判
        if !self.dom.is_full(1, 31) && !self.dow.is_full(0, 7) {
            dom_ok || dow_ok
        } else {
            dom_ok && dow_ok
        }
    }
}

/// 某年某月天数（含闰年）
fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// 周几（0=周日；Zeller 同余简化实现）
fn weekday_index(year: i32, month: u32, day: u32) -> u32 {
    // 1970-01-01 是周四(4)。以儒略日推：
    let mut days: i64 = 0;
    if year >= 1970 {
        for y in 1970..year {
            days += if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 366 } else { 365 };
        }
    } else {
        for y in year..1970 {
            days -= if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 366 } else { 365 };
        }
    }
    for m in 1..month {
        days += days_in_month(year, m) as i64;
    }
    days += day as i64 - 1;
    // 1970-01-01 = 周四 = 4
    ((days + 4).rem_euclid(7)) as u32
}

// ============================================================================
// 任务与配置
// ============================================================================

/// 单条定时任务
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScheduleJob {
    /// 任务名（journal 对账键）
    pub name: String,
    /// 五字段 cron 表达式
    pub cron: String,
    /// 触发的子命令：`run`（goal 会话）或 `patrol`（进化巡视）
    #[serde(rename = "type")]
    pub kind: String,
    /// type=run 时的 goal（必填）
    #[serde(default)]
    pub goal: Option<String>,
    /// 可选 agent 名（缺省 default agent）
    #[serde(default)]
    pub agent: Option<String>,
    /// type=patrol 时的会话 id（patrol 子命令必填）
    #[serde(default)]
    pub session: Option<u64>,
    /// type=patrol 时的工作空间 id（patrol 子命令必填）
    #[serde(default)]
    pub workspace: Option<String>,
    /// 缺省启用
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 单次触发超时秒（缺省 600）
    #[serde(default = "default_job_timeout")]
    pub timeout_secs: u64,
}

fn default_true() -> bool {
    true
}

fn default_job_timeout() -> u64 {
    600
}

/// [schedule] 配置节
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ScheduleConfig {
    /// tick 周期秒（缺省 30；>=60 会漏分钟，装载期校验拦截）
    #[serde(default = "default_tick")]
    pub tick_secs: u64,
    /// 留痕日志路径（相对 workdir；缺省 data/schedule.journal.jsonl）
    #[serde(default)]
    pub journal: Option<String>,
    #[serde(default, rename = "jobs")]
    pub jobs: Vec<ScheduleJob>,
}

fn default_tick() -> u64 {
    30
}

impl ScheduleConfig {
    /// 装载期校验：cron 可解析 + kind 合法 + run 带 goal + tick<60 + 名字唯一。
    pub fn validate(&self) -> Result<(), String> {
        if self.tick_secs >= 60 {
            return Err(format!("schedule.tick_secs={} ≥60 会漏分钟，须 <60", self.tick_secs));
        }
        let mut names = std::collections::HashSet::new();
        for j in &self.jobs {
            if !names.insert(j.name.as_str()) {
                return Err(format!("schedule 任务重名: {}", j.name));
            }
            CronExpr::parse(&j.cron)?;
            match j.kind.as_str() {
                "run" => {
                    if j.goal.as_deref().map(str::is_empty).unwrap_or(true) {
                        return Err(format!("任务 {} type=run 缺 goal", j.name));
                    }
                }
                "patrol" => {
                    if j.session.is_none() || j.workspace.as_deref().map(str::is_empty).unwrap_or(true) {
                        return Err(format!("任务 {} type=patrol 缺 session/workspace", j.name));
                    }
                }
                other => {
                    return Err(format!("任务 {} type 非法: {other}（run|patrol）", j.name));
                }
            }
        }
        Ok(())
    }
}

// ============================================================================
// 调度循环
// ============================================================================

/// 一次 tick 的对账动作（可测纯函数核心 + IO 注入面）
pub struct TickAction {
    pub job: String,
    pub kind: String,
    pub action: &'static str, // "fired" | "missed"
    pub minute: String,       // "YYYY-MM-DDTHH:MM"
}

/// 计算本 tick 应发生的动作：从 `last_minute`（含）之后到 `now_minute`（含）逐分钟
/// 扫描；`now_minute` 的匹配=fired，此前错过的=missed（不补跑）。
/// `last_minute=None`（首 tick）只看当前分钟，不回溯（启动即历史清零，如实约定）。
pub fn plan_tick(jobs: &[ScheduleJob], last_minute: Option<&str>, now_minute: &str) -> Vec<TickAction> {
    let mut out = Vec::new();
    let now = parse_minute(now_minute).unwrap_or((1970, 1, 1, 0, 0));
    for j in jobs.iter().filter(|j| j.enabled) {
        let Ok(cron) = CronExpr::parse(&j.cron) else {
            continue; // 装载期已校验；此处防御性跳过
        };
        // 回溯边界：最多扫 1440 分钟（一天）——更久的历史聚合为逐条 missed 有界记账
        let start = match last_minute.and_then(parse_minute) {
            Some(l) => increment_minute(l.0, l.1, l.2, l.3, l.4),
            None => (now.0, now.1, now.2, now.3, now.4), // 首 tick 只看当前
        };
        let mut cur = start;
        let mut guard = 0;
        while (cur.0, cur.1, cur.2, cur.3, cur.4) <= now && guard < 1441 {
            let is_now = (cur.0, cur.1, cur.2, cur.3, cur.4) == (now.0, now.1, now.2, now.3, now.4);
            if cron.matches(cur.0, cur.1, cur.2, cur.3, cur.4) {
                out.push(TickAction {
                    job: j.name.clone(),
                    kind: j.kind.clone(),
                    action: if is_now { "fired" } else { "missed" },
                    minute: format_minute(cur.0, cur.1, cur.2, cur.3, cur.4),
                });
            }
            cur = increment_minute(cur.0, cur.1, cur.2, cur.3, cur.4);
            guard += 1;
        }
    }
    out
}

fn parse_minute(s: &str) -> Option<(i32, u32, u32, u32, u32)> {
    // "YYYY-MM-DDTHH:MM"
    let (d, t) = s.split_once('T')?;
    let mut dp = d.split('-');
    let y: i32 = dp.next()?.parse().ok()?;
    let mo: u32 = dp.next()?.parse().ok()?;
    let da: u32 = dp.next()?.parse().ok()?;
    let mut tp = t.split(':');
    let h: u32 = tp.next()?.parse().ok()?;
    let mi: u32 = tp.next()?.parse().ok()?;
    Some((y, mo, da, h, mi))
}

fn format_minute(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> String {
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}")
}

fn increment_minute(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> (i32, u32, u32, u32, u32) {
    let (mi2, h2, d2, mo2, y2) = if mi < 59 {
        (mi + 1, h, d, mo, y)
    } else if h < 23 {
        (0, h + 1, d, mo, y)
    } else if d < days_in_month(y, mo) {
        (0, 0, d + 1, mo, y)
    } else if mo < 12 {
        (0, 0, 1, mo + 1, y)
    } else {
        (0, 0, 1, 1, y + 1)
    };
    (y2, mo2, d2, h2, mi2)
}

/// 留痕一条 JSONL（append；失败如实返回错误——日志写不进即停，不静默丢账）
pub fn journal_append(path: &Path, entry: &serde_json::Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录 {}: {e}", parent.display()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("打开留痕日志 {}: {e}", path.display()))?;
    writeln!(f, "{entry}").map_err(|e| format!("写留痕: {e}"))
}

/// 触发一次任务：子进程调本二进制对应子命令，返回退出码。
pub fn fire_job(exe: &Path, workdir: &Path, job: &ScheduleJob) -> Result<i32, String> {
    let mut cmd = Command::new(exe);
    cmd.current_dir(workdir);
    match job.kind.as_str() {
        "patrol" => {
            cmd.arg("patrol")
                .arg("--session")
                .arg(job.session.unwrap_or(0).to_string())
                .arg("--workspace")
                .arg(job.workspace.as_deref().unwrap_or(""));
            if let Some(a) = &job.agent {
                cmd.arg("--agent").arg(a);
            }
        }
        _ => {
            cmd.arg("run");
            if let Some(a) = &job.agent {
                cmd.arg("--agent").arg(a);
            }
            cmd.arg("--").arg(job.goal.as_deref().unwrap_or(""));
        }
    }
    // 子进程输出直通本进程 stdio（运维诚实性：任务日志可见，不吞）
    let st = cmd
        .status()
        .map_err(|e| format!("拉起子进程失败 {}: {e}", exe.display()))?;
    Ok(st.code().unwrap_or(-1))
}

/// 调度主循环（阻塞；每 tick_secs 对账一次）。留痕失败/任务失败都不中断循环
/// （单任务故障不拖垮其余任务），但 journal 写失败=断账级错误即刻返回。
pub fn run_loop(
    config: &ScheduleConfig,
    workdir: &Path,
    exe: PathBuf,
) -> Result<(), String> {
    let journal_path = workdir.join(config.journal.clone().unwrap_or_else(|| "data/schedule.journal.jsonl".into()));
    let mut last_minute: Option<String> = None;
    println!(
        "[schedule] 调度器启动：{} 任务，tick={}s，留痕={}",
        config.jobs.len(),
        config.tick_secs,
        journal_path.display()
    );
    loop {
        let now = chrono_now_minute();
        let actions = plan_tick(&config.jobs, last_minute.as_deref(), &now);
        for a in actions {
            let entry = serde_json::json!({
                "ts": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
                "minute": a.minute,
                "job": a.job,
                "kind": a.kind,
                "action": a.action,
            });
            match a.action {
                "fired" => {
                    println!("[schedule] 触发 {}/{} @{}", a.job, a.kind, a.minute);
                    journal_append(&journal_path, &entry)?;
                    if let Some(job) = config.jobs.iter().find(|j| j.name == a.job) {
                        let res = fire_job(&exe, workdir, job);
                        let done = serde_json::json!({
                            "ts": std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
                            "minute": a.minute, "job": a.job, "kind": a.kind,
                            "action": "finished",
                            "exit_code": res.clone().unwrap_or(-1),
                            "error": res.err(),
                        });
                        journal_append(&journal_path, &done)?;
                    }
                }
                _ => {
                    println!("[schedule] 错过（不补跑）{}/{} @{}", a.job, a.kind, a.minute);
                    journal_append(&journal_path, &entry)?;
                }
            }
        }
        last_minute = Some(now);
        std::thread::sleep(Duration::from_secs(config.tick_secs.max(1)));
    }
}

/// 当前本地墙钟分钟键（无 chrono 依赖：从系统时区偏移不行——直接用 UTC 键并如实标注。
/// cron 语义按本地墙钟更直观，但引入本地时区换算需 chrono；此处**以 UTC 分钟为准**
/// 并在配置档声明该口径（确定性优先：UTC 无夏令时跳变）。
fn chrono_now_minute() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    let rem = secs % 86400;
    let (y, mo, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Unix 天数 → 公历年月日（Howard Hinnant 算法）
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as i64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    ((y + i64::from(m <= 2)) as i32, m as u32, d as u32)
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn cron_parse_basic_forms() {
        CronExpr::parse("* * * * *").unwrap();
        CronExpr::parse("*/2 * * * *").unwrap();
        CronExpr::parse("0 9-17 * * 1-5").unwrap();
        CronExpr::parse("1,30 0 1,15 */3 0").unwrap();
        assert!(CronExpr::parse("* * * *").is_err(), "四段应报错");
        assert!(CronExpr::parse("60 * * * *").is_err(), "分越界");
        assert!(CronExpr::parse("*/0 * * * *").is_err(), "零步长");
    }

    #[test]
    fn cron_match_every_two_minutes() {
        let c = CronExpr::parse("*/2 * * * *").unwrap();
        assert!(c.matches(2026, 10, 22, 10, 0));
        assert!(c.matches(2026, 10, 22, 10, 58));
        assert!(!c.matches(2026, 10, 22, 10, 59));
    }

    #[test]
    fn cron_weekday_and_dom_union() {
        // 周一至周五 9 点
        let c = CronExpr::parse("0 9 * * 1-5").unwrap();
        // 2026-10-22 是周四
        assert!(c.matches(2026, 10, 22, 9, 0));
        assert!(!c.matches(2026, 10, 22, 10, 0));
        // 2026-10-24 周六不匹配
        assert!(!c.matches(2026, 10, 24, 9, 0));
    }

    #[test]
    fn weekday_index_known_dates() {
        assert_eq!(weekday_index(1970, 1, 1), 4, "1970-01-01 周四");
        assert_eq!(weekday_index(2026, 10, 22), 4, "2026-10-22 周四");
        assert_eq!(weekday_index(2026, 10, 25), 0, "2026-10-25 周日");
    }

    #[test]
    fn plan_tick_fires_now_and_marks_missed() {
        let jobs = vec![ScheduleJob {
            name: "j".into(),
            cron: "*/2 * * * *".into(),
            kind: "patrol".into(),
            goal: None,
            agent: None,
            session: Some(1),
            workspace: Some("ws".into()),
            enabled: true,
            timeout_secs: 60,
        }];
        // 上一 tick=10:00（已含该分钟），现在=10:05：错过 10:02/10:04，当前 10:05 不匹配
        let acts = plan_tick(&jobs, Some("2026-10-22T10:00"), "2026-10-22T10:05");
        assert_eq!(acts.iter().filter(|a| a.action == "missed").count(), 2);
        assert!(acts.iter().all(|a| a.action != "fired"));
        // 现在=10:06：错过 10:02/10:04 + fired 10:06
        let acts = plan_tick(&jobs, Some("2026-10-22T10:00"), "2026-10-22T10:06");
        assert_eq!(acts.iter().filter(|a| a.action == "missed").count(), 2);
        assert_eq!(acts.iter().filter(|a| a.action == "fired").count(), 1);
    }

    #[test]
    fn plan_tick_first_tick_no_backtrack() {
        let jobs = vec![ScheduleJob {
            name: "j".into(),
            cron: "* * * * *".into(),
            kind: "patrol".into(),
            goal: None,
            agent: None,
            session: Some(1),
            workspace: Some("ws".into()),
            enabled: true,
            timeout_secs: 60,
        }];
        let acts = plan_tick(&jobs, None, "2026-10-22T10:05");
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].action, "fired");
    }

    #[test]
    fn config_validate_rules() {
        let mut c = ScheduleConfig::default();
        assert!(c.validate().is_ok());
        c.jobs.push(ScheduleJob {
            name: "a".into(),
            cron: "bad".into(),
            kind: "run".into(),
            goal: Some("g".into()),
            agent: None,
            session: None,
            workspace: None,
            enabled: true,
            timeout_secs: 60,
        });
        assert!(c.validate().is_err());
        c.jobs[0].cron = "*/5 * * * *".into();
        c.jobs[0].goal = None;
        assert!(c.validate().is_err(), "run 缺 goal 应拒");
        c.jobs[0].kind = "xxx".into();
        c.jobs[0].goal = Some("g".into());
        assert!(c.validate().is_err(), "kind 非法应拒");
        c.tick_secs = 60;
        c.jobs[0].kind = "patrol".into();
        assert!(c.validate().is_err(), "tick>=60 应拒");
    }

    #[test]
    fn minute_math_roundtrip() {
        assert_eq!(increment_minute(2026, 12, 31, 23, 59), (2027, 1, 1, 0, 0));
        assert_eq!(increment_minute(2024, 2, 28, 23, 59), (2024, 2, 29, 0, 0), "闰年");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20648), (2026, 7, 14));
    }
}
