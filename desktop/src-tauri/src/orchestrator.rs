// 进程编排：端口预分配 → job 收编 → engine → 健康探测 → serve → 健康探测。
//
// 已核实的实况约定（实施期逐一对照源码验证）：
// - evorule-server：`--profile development` 跳过生产五条件门（缺省 production
//   漏配即拒启）；`--addr 127.0.0.1:P`；`--auth-token`；`--wal-dir` 配齐后
//   会话跨重启存活。
// - evo-agent serve：`serve --host --port --auth-token --workdir`；config 从
//   `{workdir}/evo-agent.toml` 读；agents.dir 支持绝对路径（agents 目录为空
//   即拒起，故壳生成的 toml 必须写绝对路径）；工作台静态资源硬编码相对
//   workdir（`{workdir}/web/dist`），以 junction 指向产品资源根。
// - evorule 连接：env 覆盖 `EVO_AGENT_EVORULE__BASE_URL`（必须带 scheme，
//   客户端按纯字符串拼接 path）+ `EVO_AGENT_EVORULE__API_KEY`；token 仅存
//   壳内存与子进程环境，不落盘。

use std::io::Write;
use std::net::TcpListener;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rand::Rng;

use crate::health;
use crate::job::JobHandle;
use crate::shell_config::{Console, ResolvedPaths};

/// 子进程不弹控制台窗口（GUI 进程 spawn 控制台程序必带）
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct Runtime {
    pub job: JobHandle,
    pub serve_port: u16,
    _serve: Child,
    _engine: Child,
}

impl Runtime {
    /// 退出序列主路径：立即终止整树（M1 用强杀；优雅停两段式留 M3 细化）。
    pub fn shutdown(&self) {
        self.job.terminate();
    }
}

/// 生成 128bit 随机 token（32 hex 字符）。
pub fn gen_token() -> String {
    let bytes: [u8; 16] = rand::thread_rng().gen();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// bind-test 选空闲端口：绑定 0 拿系统分配后立即释放，显式传给子进程。
fn pick_free_port(exclude: Option<u16>) -> Result<u16, String> {
    for _ in 0..16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        drop(listener);
        if Some(port) != exclude {
            return Ok(port);
        }
    }
    Err("cannot pick a free port".into())
}

fn open_append(path: &Path) -> Result<std::fs::File, String> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("open log {}: {e}", path.display()))
}

/// 确保 `link` 为指向 `target` 的目录 junction。
///
/// 每次启动幂等重建（毫秒级）：悬空（目标消失）或错向（指向他处）的旧
/// junction 一律清除，杜绝 mklink「已存在」失败与 serve 静态 404。
fn ensure_junction(link: &Path, target: &Path) -> Result<(), String> {
    if std::fs::symlink_metadata(link).is_ok() {
        let _ = std::fs::remove_dir(link);
    }
    let out = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "mklink /J {} -> {} failed: {}",
            link.display(),
            target.display(),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// 壳生成的 serve 工作配置。
///
/// 基座 = 用户 evo-agent.toml 透传（查找序：产品资源根 → 壳配置目录；LLM key
/// 等敏感配置随用户配置走，壳不感知其内容）；随后强制覆盖 [agents].dir 为
/// 绝对路径（serve agents 空目录拒起）；console 通道按开关注入（预留通路，
/// 缺省关闭）。用户配置解析失败时 fail-soft：告警后退回最小配置（M1 行为）。
fn write_runtime_config(
    runtime_dir: &Path,
    home: &Path,
    app_config_dir: &Path,
    console: &Console,
    log: &mut impl Write,
) -> Result<(), String> {
    let path = runtime_dir.join("evo-agent.toml");

    let base = [home.join("evo-agent.toml"), app_config_dir.join("evo-agent.toml")]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok());

    let mut table: toml::Table = match &base {
        Some(s) => match toml::from_str(s) {
            Ok(t) => t,
            Err(e) => {
                writeln!(
                    log,
                    "[orch] user evo-agent.toml parse failed ({e}); using minimal config"
                )
                .ok();
                toml::Table::new()
            }
        },
        None => toml::Table::new(),
    };

    // [agents].dir 一律覆盖为绝对路径（基座中的相对路径以壳解析结果为准）
    let agents = table
        .entry("agents")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let toml::Value::Table(t) = agents {
        t.insert(
            "dir".to_string(),
            toml::Value::String(home.join("agents").display().to_string()),
        );
    }

    if console.enabled {
        match &console.dir {
            Some(dir) => {
                let wb = table
                    .entry("workbench")
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                if let toml::Value::Table(t) = wb {
                    t.insert(
                        "console_dir".to_string(),
                        toml::Value::String(dir.display().to_string()),
                    );
                }
            }
            None => {
                writeln!(log, "[orch] console.enabled=true but console.dir empty; skipped").ok();
            }
        }
    }

    let out = toml::to_string(&table).map_err(|e| format!("serialize evo-agent.toml: {e}"))?;
    std::fs::write(&path, out).map_err(|e| format!("write {}: {e}", path.display()))
}

pub struct LaunchInput<'a> {
    pub paths: &'a ResolvedPaths,
    pub console: &'a Console,
    pub engine_extra_args: &'a [String],
    pub serve_token: &'a str,
    pub engine_token: &'a str,
    pub version: &'a str,
}

pub fn launch(input: LaunchInput) -> Result<Runtime, String> {
    let paths = input.paths;

    // 目录先行（日志 open_append 依赖 logs 目录已存在）
    for dir in [&paths.data_dir, &paths.runtime_dir, &paths.logs_dir] {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    }
    let mut shell_log = open_append(&paths.logs_dir.join("shell.log"))?;

    writeln!(shell_log, "[orch] launch begin (shell v{})", input.version).ok();
    std::fs::create_dir_all(paths.runtime_dir.join("web"))
        .map_err(|e| e.to_string())?;

    // 工作台静态资源：serve 硬编码读 {workdir}/web/dist，junction 指向产品资源根
    ensure_junction(
        &paths.runtime_dir.join("web").join("dist"),
        &paths.evo_agent_home.join("web").join("dist"),
    )?;

    write_runtime_config(
        &paths.runtime_dir,
        &paths.evo_agent_home,
        &paths.app_config_dir,
        input.console,
        &mut shell_log,
    )?;

    let serve_port = pick_free_port(None)?;
    let engine_port = pick_free_port(Some(serve_port))?;

    // job 收编优先自收编（此后子进程自动隶属）；失败退化为逐个收编
    let job = JobHandle::create()?;
    let self_assigned = job.assign_self();
    writeln!(
        shell_log,
        "[orch] job created (self_assigned={self_assigned})"
    )
    .ok();

    // engine：evorule-server（development 档 + 回环 + token + 全路径显式，
    // 不依赖 CWD：宪法/规则取资源根，db/memory 归壳数据目录，wal 常驻）
    let engine_wal = paths.data_dir.join("engine-wal");
    std::fs::create_dir_all(&engine_wal).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&paths.engine_data_dir).map_err(|e| e.to_string())?;
    let engine_log = open_append(&paths.logs_dir.join("engine.log"))?;
    let engine_log_err = open_append(&paths.logs_dir.join("engine.log"))?;
    let mut engine_cmd = Command::new(&paths.evorule_server_exe);
    engine_cmd
        .args([
            "--profile",
            "development",
            "--addr",
            &format!("127.0.0.1:{engine_port}"),
            "--auth-token",
            input.engine_token,
            "--core-eval",
        ])
        .arg(paths.evorule_server_home.join("resources").join("server_eval.json"))
        .args(["--rules-dir"])
        .arg(paths.evorule_server_home.join("rules"))
        .args(["--db-path"])
        .arg(paths.engine_data_dir.join("evorule.db"))
        .args(["--memory-dir"])
        .arg(paths.engine_data_dir.join("memory"))
        .args(["--wal-dir"])
        .arg(&engine_wal)
        .args(input.engine_extra_args)
        .stdout(Stdio::from(engine_log))
        .stderr(Stdio::from(engine_log_err))
        .creation_flags(CREATE_NO_WINDOW);
    writeln!(shell_log, "[orch] spawn engine: {}", paths.evorule_server_exe.display()).ok();
    let mut engine = engine_cmd
        .spawn()
        .map_err(|e| format!("spawn evorule-server: {e}"))?;
    if !self_assigned {
        job.assign_pid(engine.id());
    }

    if !health::wait_healthy(engine_port, "/api/health", Duration::from_secs(30)) {
        let _ = engine.kill();
        return Err(format!(
            "engine health check timeout (port {engine_port}); logs: {}",
            paths.logs_dir.display()
        ));
    }
    writeln!(shell_log, "[orch] engine healthy on port {engine_port}").ok();

    // serve：evo-agent serve（workdir=壳 runtime 目录，evorule 指向 engine 走 env）
    let serve_log = open_append(&paths.logs_dir.join("serve.log"))?;
    let serve_log_err = open_append(&paths.logs_dir.join("serve.log"))?;
    let mut serve_cmd = Command::new(&paths.evo_agent_exe);
    serve_cmd
        .args([
            "serve",
            "--host",
            "127.0.0.1",
            "--port",
            &serve_port.to_string(),
            "--auth-token",
            input.serve_token,
            "--workdir",
        ])
        .arg(&paths.runtime_dir)
        .env("EVO_AGENT_EVORULE__BASE_URL", format!("http://127.0.0.1:{engine_port}"))
        .env("EVO_AGENT_EVORULE__API_KEY", input.engine_token)
        .env("EVORULE_AUTH_TOKEN", input.engine_token)
        .stdout(Stdio::from(serve_log))
        .stderr(Stdio::from(serve_log_err))
        .creation_flags(CREATE_NO_WINDOW);
    writeln!(shell_log, "[orch] spawn serve: {}", paths.evo_agent_exe.display()).ok();
    let mut serve = serve_cmd
        .spawn()
        .map_err(|e| format!("spawn evo-agent serve: {e}"))?;
    if !self_assigned {
        job.assign_pid(serve.id());
    }

    if !health::wait_healthy(serve_port, "/health", Duration::from_secs(30)) {
        let _ = serve.kill();
        return Err(format!(
            "serve health check timeout (port {serve_port}); logs: {}",
            paths.logs_dir.display()
        ));
    }
    writeln!(shell_log, "[orch] serve healthy on port {serve_port}").ok();

    // 运行时握手文件（诊断用；不含 token）
    let runtime_json = serde_json::json!({
        "version": input.version,
        "started_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "serve": { "port": serve_port, "pid": serve.id() },
        "engine": { "port": engine_port, "pid": engine.id() },
    });
    std::fs::write(
        paths.data_dir.join("runtime.json"),
        serde_json::to_string_pretty(&runtime_json).unwrap(),
    )
    .map_err(|e| format!("write runtime.json: {e}"))?;

    writeln!(shell_log, "[orch] launch complete").ok();
    Ok(Runtime {
        job,
        serve_port,
        _serve: serve,
        _engine: engine,
    })
}
