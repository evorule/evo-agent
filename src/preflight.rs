// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

//! 启动预检与 doctor 体检(B1)
//!
//! 职责:把「依赖缺失在深处以网络错误形态暴露」(fail-late:执行体不可达要吃
//! 3 次指数退避才报错)改为「启动期明确报错 + 修复指引 + 非零退出」(fail-visible)。
//! 共享实现分两档:
//!
//! - [`PreflightScope::Quick`]:启动期快检(①②③④;探活并行,单探 3s 超时,
//!   总预算 ≤5s),由 Run/Serve/Repl/Schedule/Patrol 五入口统一调用;
//! - [`PreflightScope::Full`]:doctor 七项全量体检(①-⑦,串行逐项)。
//!
//! 检查项(编号即契约):
//! ① config 四层合成+validate(`Config::load` 严格档强制;此处出脱敏摘要)
//! ② 依赖1 evorule-server health 探活(`GET {evorule.base_url}/api/health`,3s)
//! ③ 依赖2 LLM 执行体探活(任何 HTTP 状态含 404/405 记可达——探端点存在性
//!    非业务成功;`llm.mode` 于后续批次落地前恒 relay 档语义)
//! ④ LLM 凭据存在性(relay 档:凭据归执行体侧持有,agent 侧零凭据判据,
//!    恒 Pass 信息项;direct 档落地后按 mode 分语义)
//! ⑤ default agent 定义 load(Full)
//! ⑥ workdir 可写(Full)
//! ⑦ 流式端点形态(信息项 Warn 级,永不 Fail;流式不可用时执行体回落
//!    legacy 整包路径,行为兼容)
//!
//! 边界纪律:探活为本模块独立实现,不借用 `io_handlers::LlmHandler`(该文件
//! 零改动);凭据面只出「存在性」,零密钥内容(与 `LlmStatusSnapshot` 脱敏
//! 语义同源)。

use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::config::Config;

// ---------------------------------------------------------------------------
// 修复指引文案常量(模板唯一来源,入口间不得各写各的)
// ---------------------------------------------------------------------------

/// 配置装载失败指引(config error 时随错误行输出)
pub const HINT_CONFIG_LOAD: &str =
    "fix the config error above (evo-agent.toml in the workdir); \
     `evo-agent config` shows the merged result after fixing";

/// 依赖1(evorule-server)不可达指引
pub const HINT_EVORULE_SERVER: &str = "start the evorule server (default \
     http://127.0.0.1:18080) or point `evorule.base_url` at a reachable instance; \
     agent sessions and audit facts require it";

/// 依赖2(LLM 执行体)不可达指引
pub const HINT_LLM_EXECUTOR: &str = "start the LLM executor or point \
     `llm.executor_url` at a reachable endpoint (env override: \
     EVORULE_LLM_EXECUTOR_URL); LLM calls are relayed through it";

/// default agent 定义装载失败指引
pub const HINT_AGENT_DEF: &str = "create agents/<name>.json in the agents \
     directory or fix `agents.default`/`agents.dir` in the config; \
     `evo-agent list` shows what is loadable";

/// workdir 不可写指引
pub const HINT_WORKDIR: &str = "grant write permission on the workdir \
     (sessions and journals are written under it)";

/// `--no-preflight` 逃生口留痕(跳过本身必须可见)
pub const SKIP_NOTE: &str = "skipped by --no-preflight; dependency failures will \
     surface mid-run as network errors";

/// 单项探活超时(②③⑦同款;Quick 双探并行 → 总预算 ≤5s)
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// 报告结构
// ---------------------------------------------------------------------------

/// 单项检查结论
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreflightVerdict {
    /// 通过
    Pass,
    /// 阻断性失败(启动期 → 指引 + 非零退出)
    Fail,
    /// 非阻断提示(信息项;照常启动)
    Warn,
}

/// 单项检查(名称/结论/明细/修复指引;指引仅 Fail(及部分 Warn)非空)
#[derive(Debug, Clone, Serialize)]
pub struct PreflightCheck {
    /// 检查项名称(机器可读标识,如 "evorule-server")
    pub name: String,
    /// 结论
    pub verdict: PreflightVerdict,
    /// 人读明细(零密钥内容)
    pub detail: String,
    /// 修复指引(空 = 无)
    pub hint: String,
}

/// 预检报告
#[derive(Debug, Clone, Default, Serialize)]
pub struct PreflightReport {
    /// 逐项检查结果(顺序即检查项编号序)
    pub checks: Vec<PreflightCheck>,
}

impl PreflightReport {
    /// 是否存在阻断性失败(任一 Fail)
    pub fn has_fail(&self) -> bool {
        self.checks
            .iter()
            .any(|c| c.verdict == PreflightVerdict::Fail)
    }

    /// 是否存在非阻断提示(任一 Warn)
    pub fn has_warn(&self) -> bool {
        self.checks
            .iter()
            .any(|c| c.verdict == PreflightVerdict::Warn)
    }

    /// 人读渲染(stderr 输出;`--json` 机器读走 serde_json 序列化)
    pub fn render_human(&self) -> String {
        let fails = self
            .checks
            .iter()
            .filter(|c| c.verdict == PreflightVerdict::Fail)
            .count();
        let warns = self
            .checks
            .iter()
            .filter(|c| c.verdict == PreflightVerdict::Warn)
            .count();
        let passes = self.checks.len() - fails - warns;
        let mut out = format!("preflight: {fails} failed, {warns} warning(s), {passes} passed\n");
        for c in &self.checks {
            let tag = match c.verdict {
                PreflightVerdict::Pass => "PASS",
                PreflightVerdict::Fail => "FAIL",
                PreflightVerdict::Warn => "WARN",
            };
            out.push_str(&format!("  [{tag}] {} — {}\n", c.name, c.detail));
            if !c.hint.is_empty() {
                out.push_str(&format!("        hint: {}\n", c.hint));
            }
        }
        out
    }
}

/// 预检档位:Quick = 启动期快检(①②③④);Full = doctor 全量(①-⑦)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightScope {
    /// 启动期快检(≤5s 预算;探活并行)
    Quick,
    /// doctor 七项全量体检(串行逐项)
    Full,
}

/// 执行预检,产出报告(不直接退出;退出语义由入口层按 has_fail 决定)
///
/// - `cfg`:已完成四层合成与校验的配置(`Config::load` 严格档或宽松档产物)
/// - `workdir`:工作目录(⑤ agents 目录解析基准、⑥ 可写探测目标)
pub async fn run_preflight(cfg: &Config, workdir: &Path, scope: PreflightScope) -> PreflightReport {
    let mut checks = Vec::new();

    // ① config 摘要(合成+validate 已由装载强制)
    checks.push(check_config(cfg));

    // ②③ 依赖探活:Quick 并行(≤5s 预算),Full 串行逐项
    match scope {
        PreflightScope::Quick => {
            let (d1, d2) = tokio::join!(probe_evorule_health(cfg), probe_executor(cfg));
            checks.push(d1);
            checks.push(d2);
        }
        PreflightScope::Full => {
            checks.push(probe_evorule_health(cfg).await);
            checks.push(probe_executor(cfg).await);
        }
    }

    // ④ LLM 凭据存在性(按 mode 分语义;当前恒 relay 档)
    checks.push(check_llm_credentials(cfg));

    // ⑤⑥⑦ Full 独有
    if scope == PreflightScope::Full {
        checks.push(check_default_agent(cfg, workdir));
        checks.push(check_workdir_writable(workdir));
        checks.push(probe_stream_endpoint(cfg).await);
    }

    PreflightReport { checks }
}

// ---------------------------------------------------------------------------
// 检查项实现
// ---------------------------------------------------------------------------

/// ① config 摘要(key 脱敏:只出存在性,零密钥内容)
fn check_config(cfg: &Config) -> PreflightCheck {
    let key_state = if cfg.llm.api_key.is_empty() {
        "unset"
    } else {
        "set (not consumed; credentials held executor-side)"
    };
    let token_state = if cfg
        .llm
        .executor_token
        .as_deref()
        .is_some_and(|t| !t.is_empty())
    {
        "present"
    } else {
        "absent"
    };
    PreflightCheck {
        name: "config".to_string(),
        verdict: PreflightVerdict::Pass,
        detail: format!(
            "evorule.base_url={}; llm.executor_url={}; llm.model={}; llm.timeout_secs={}; \
             llm.max_retries={}; agents.default={}; agents.dir={}; llm.api_key={}; \
             llm.executor_token={token_state}",
            cfg.evorule.base_url,
            cfg.llm.executor_url,
            cfg.llm.model,
            cfg.llm.timeout_secs,
            cfg.llm.max_retries,
            cfg.agents.default,
            cfg.agents.dir.display(),
            key_state,
        ),
        hint: String::new(),
    }
}

/// ② 依赖1 evorule-server health 探活(`GET /api/health`,3s 超时)
async fn probe_evorule_health(cfg: &Config) -> PreflightCheck {
    const NAME: &str = "evorule-server";
    let url = format!(
        "{}/api/health",
        cfg.evorule.base_url.trim_end_matches('/')
    );
    let client = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            return fail(NAME, format!("probe client build error: {e}"), HINT_EVORULE_SERVER)
        }
    };
    match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                pass(NAME, format!("{url} -> {status}"))
            } else {
                fail(
                    NAME,
                    format!("{url} -> {status} (reachable, but health did not answer OK)"),
                    HINT_EVORULE_SERVER,
                )
            }
        }
        Err(e) => fail(NAME, format!("{url} unreachable: {e}"), HINT_EVORULE_SERVER),
    }
}

/// ③ 依赖2 LLM 执行体探活(任何 HTTP 状态记可达;传输层不可达才 Fail)
///
/// `llm.mode` 于后续批次落地前恒 relay 档语义(§3.4:仅 relay 档探执行体;
/// direct 档落地后本项按 mode 分语义)。
async fn probe_executor(cfg: &Config) -> PreflightCheck {
    const NAME: &str = "llm-executor";
    let url = &cfg.llm.executor_url;
    let client = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => return fail(NAME, format!("probe client build error: {e}"), HINT_LLM_EXECUTOR),
    };
    match client.get(url).send().await {
        Ok(resp) => pass(
            NAME,
            format!(
                "{url} reachable (HTTP {}; endpoint-existence probe, business success not required)",
                resp.status()
            ),
        ),
        Err(e) => fail(NAME, format!("{url} unreachable: {e}"), HINT_LLM_EXECUTOR),
    }
}

/// ④ LLM 凭据存在性(relay 档:凭据归执行体侧,agent 侧恒 Pass 信息项)
fn check_llm_credentials(cfg: &Config) -> PreflightCheck {
    let token = if cfg
        .llm
        .executor_token
        .as_deref()
        .is_some_and(|t| !t.is_empty())
    {
        "present"
    } else {
        "absent"
    };
    pass(
        "llm-credentials",
        format!(
            "relay mode: credentials held executor-side (key escrow); \
             agent-side llm.api_key not consumed; llm.executor_token={token}"
        ),
    )
}

/// ⑤ default agent 定义 load(Full)
fn check_default_agent(cfg: &Config, workdir: &Path) -> PreflightCheck {
    const NAME: &str = "agent-default";
    // agents 目录解析与 cmd_run 同款:相对路径相对 workdir(与 config 装载基准一致)
    let agents_dir = if cfg.agents.dir.is_absolute() {
        cfg.agents.dir.clone()
    } else {
        workdir.join(&cfg.agents.dir)
    };
    let mgr = crate::agent::definition::AgentDefinitionManager::new(agents_dir.clone());
    match mgr.load(&cfg.agents.default) {
        Ok(_) => pass(
            NAME,
            format!(
                "default agent '{}' loaded from {}",
                cfg.agents.default,
                agents_dir.display()
            ),
        ),
        Err(e) => fail(
            NAME,
            format!(
                "failed to load default agent '{}' from {}: {e}",
                cfg.agents.default,
                agents_dir.display()
            ),
            HINT_AGENT_DEF,
        ),
    }
}

/// ⑥ workdir 可写(Full;写入+删除探测文件,零残留)
fn check_workdir_writable(workdir: &Path) -> PreflightCheck {
    const NAME: &str = "workdir-writable";
    let probe = workdir.join(format!(".evo-agent-preflight-{}", std::process::id()));
    match std::fs::write(&probe, b"ok").and_then(|()| std::fs::remove_file(&probe)) {
        Ok(()) => pass(NAME, format!("{} writable", workdir.display())),
        Err(e) => fail(
            NAME,
            format!("{} not writable: {e}", workdir.display()),
            HINT_WORKDIR,
        ),
    }
}

/// ⑦ 流式端点形态(Full;信息项 Warn 级,永不 Fail)
async fn probe_stream_endpoint(cfg: &Config) -> PreflightCheck {
    const NAME: &str = "llm-stream-endpoint";
    let stream_url = match stream_endpoint_url(&cfg.llm.executor_url) {
        Some(u) => u,
        None => {
            return warn(
                NAME,
                format!(
                    "executor url {} has no /llm/execute suffix (custom path); \
                     no stream twin endpoint to probe, non-streaming path applies",
                    cfg.llm.executor_url
                ),
            )
        }
    };
    let client = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => return warn(NAME, format!("probe client build error: {e}")),
    };
    match client.get(&stream_url).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.as_u16() == 404 {
                warn(
                    NAME,
                    format!("{stream_url} -> 404 (stream endpoint missing; execution falls back to non-streaming)"),
                )
            } else {
                pass(NAME, format!("{stream_url} routed (HTTP {status})"))
            }
        }
        Err(e) => warn(
            NAME,
            format!("{stream_url} unreachable: {e} (informational; execution falls back to non-streaming)"),
        ),
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 执行体端点 → 流式端点 URL(与 llm_handler 执行体→流式推导同形:
/// 剥 `/llm/execute` 后缀拼 `/llm/execute-stream`;不以该后缀收尾返回 None)。
/// 独立实现——llm_handler 零改动纪律。
fn stream_endpoint_url(executor_url: &str) -> Option<String> {
    executor_url
        .strip_suffix("/llm/execute")
        .map(|base| format!("{base}/llm/execute-stream"))
}

fn pass(name: &str, detail: String) -> PreflightCheck {
    PreflightCheck {
        name: name.to_string(),
        verdict: PreflightVerdict::Pass,
        detail,
        hint: String::new(),
    }
}

fn fail(name: &str, detail: String, hint: &str) -> PreflightCheck {
    PreflightCheck {
        name: name.to_string(),
        verdict: PreflightVerdict::Fail,
        detail,
        hint: hint.to_string(),
    }
}

fn warn(name: &str, detail: String) -> PreflightCheck {
    PreflightCheck {
        name: name.to_string(),
        verdict: PreflightVerdict::Warn,
        detail,
        hint: String::new(),
    }
}

// ---------------------------------------------------------------------------
// 测试(随实现同文件;形态先例 = llm_handler 测试族:#[tokio::test] + mockito)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn base_cfg() -> Config {
        Config::default()
    }

    fn dead_url(port: u16, path: &str) -> String {
        format!("http://127.0.0.1:{port}{path}")
    }

    #[test]
    fn test_report_json_shape() {
        let report = PreflightReport {
            checks: vec![
                pass("a", "ok".to_string()),
                fail("b", "bad".to_string(), HINT_WORKDIR),
                warn("c", "meh".to_string()),
            ],
        };
        let v = serde_json::to_value(&report).unwrap();
        let arr = v["checks"].as_array().unwrap();
        assert_eq!(arr[0]["verdict"], "pass");
        assert_eq!(arr[1]["verdict"], "fail");
        assert_eq!(arr[2]["verdict"], "warn");
        assert_eq!(arr[1]["hint"], HINT_WORKDIR);
    }

    #[test]
    fn test_render_human_contains_fail_and_hint() {
        let report = PreflightReport {
            checks: vec![
                pass("config", "ok".to_string()),
                fail("evorule-server", "unreachable".to_string(), HINT_EVORULE_SERVER),
            ],
        };
        let s = report.render_human();
        assert!(s.contains("[FAIL] evorule-server"), "render: {s}");
        assert!(s.contains(HINT_EVORULE_SERVER), "render: {s}");
        assert!(s.contains("1 failed"), "render: {s}");
    }

    #[tokio::test]
    async fn test_quick_four_checks_dead_deps() {
        let mut cfg = base_cfg();
        cfg.evorule.base_url = dead_url(1, "");
        cfg.llm.executor_url = dead_url(1, "/llm/execute");
        let started = std::time::Instant::now();
        let report = run_preflight(&cfg, Path::new("."), PreflightScope::Quick).await;
        let elapsed = started.elapsed();
        let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["config", "evorule-server", "llm-executor", "llm-credentials"]);
        assert!(report.has_fail());
        let d1 = &report.checks[1];
        assert_eq!(d1.verdict, PreflightVerdict::Fail);
        assert!(!d1.hint.is_empty());
        let d2 = &report.checks[2];
        assert_eq!(d2.verdict, PreflightVerdict::Fail);
        // Quick 预算:双探并行,断依赖(连接瞬拒)应在 5s 内收场
        assert!(elapsed.as_secs() < 5, "quick preflight took {elapsed:?}");
    }

    #[tokio::test]
    async fn test_quick_pass_with_mock_servers() {
        let mut health = mockito::Server::new_async().await;
        let _h = health
            .mock("GET", "/api/health")
            .with_status(200)
            .with_body("ok")
            .create_async()
            .await;
        let mut executor = mockito::Server::new_async().await;
        // 404/405 记可达:探端点存在性非业务成功(§3.4)
        let _e = executor
            .mock("GET", "/llm/execute")
            .with_status(405)
            .create_async()
            .await;

        let mut cfg = base_cfg();
        cfg.evorule.base_url = health.url();
        cfg.llm.executor_url = format!("{}/llm/execute", executor.url());
        let report = run_preflight(&cfg, Path::new("."), PreflightScope::Quick).await;
        assert!(!report.has_fail(), "render: {}", report.render_human());
        assert_eq!(report.checks[1].verdict, PreflightVerdict::Pass);
        assert_eq!(report.checks[2].verdict, PreflightVerdict::Pass);
    }

    #[tokio::test]
    async fn test_full_seven_checks_order() {
        let mut cfg = base_cfg();
        cfg.evorule.base_url = dead_url(1, "");
        cfg.llm.executor_url = dead_url(1, "/llm/execute");
        let workdir = std::env::temp_dir();
        let report = run_preflight(&cfg, &workdir, PreflightScope::Full).await;
        let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "config",
                "evorule-server",
                "llm-executor",
                "llm-credentials",
                "agent-default",
                "workdir-writable",
                "llm-stream-endpoint",
            ]
        );
        assert!(report.has_fail());
        // ⑥ workdir 可写:临时目录应过
        assert_eq!(report.checks[5].verdict, PreflightVerdict::Pass);
    }

    #[tokio::test]
    async fn test_full_stream_endpoint_warn_on_404() {
        let mut executor = mockito::Server::new_async().await;
        // 主端点 GET 405(可达);流式孪生端点 404(缺失 → Warn 信息项)
        let _e = executor
            .mock("GET", "/llm/execute")
            .with_status(405)
            .create_async()
            .await;
        let _s = executor
            .mock("GET", "/llm/execute-stream")
            .with_status(404)
            .create_async()
            .await;

        let mut cfg = base_cfg();
        cfg.evorule.base_url = dead_url(1, "");
        cfg.llm.executor_url = format!("{}/llm/execute", executor.url());
        let workdir = std::env::temp_dir();
        let report = run_preflight(&cfg, &workdir, PreflightScope::Full).await;
        assert_eq!(report.checks[2].verdict, PreflightVerdict::Pass);
        assert_eq!(report.checks[6].name, "llm-stream-endpoint");
        assert_eq!(report.checks[6].verdict, PreflightVerdict::Warn);
    }

    #[test]
    fn test_stream_endpoint_url_derivation() {
        assert_eq!(
            stream_endpoint_url("http://127.0.0.1:9130/llm/execute"),
            Some("http://127.0.0.1:9130/llm/execute-stream".to_string())
        );
        // 自托管改路径:无后缀 → None(非流式路径)
        assert_eq!(stream_endpoint_url("http://127.0.0.1:9130/custom"), None);
    }

    #[tokio::test]
    async fn test_workdir_writable_fail_on_file_path() {
        // 以「已存在文件」当 workdir:写入探测必败(ENOTDIR/NotADirectory 族)
        let dir = std::env::temp_dir();
        let file_path = dir.join(format!(".evo-preflight-test-{}", std::process::id()));
        std::fs::write(&file_path, b"x").unwrap();
        let cfg = base_cfg();
        let report = run_preflight(&cfg, &file_path, PreflightScope::Full).await;
        std::fs::remove_file(&file_path).ok();
        assert_eq!(report.checks[5].name, "workdir-writable");
        assert_eq!(report.checks[5].verdict, PreflightVerdict::Fail);
        assert_eq!(report.checks[5].hint, HINT_WORKDIR);
    }
}
