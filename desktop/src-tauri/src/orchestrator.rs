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
//
// M3 扩展：launch/restart_services 共用 spawn 助手（崩溃守护整组重启）；
// graceful_stop 两段式（Ctrl+C 按进程组尽力送达 → 5s 超时强杀）。

use std::io::Write;
use std::net::TcpListener;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::Rng;

use crate::health;
use crate::job::JobHandle;
use crate::shell_config::{Console, ResolvedPaths};

/// 子进程不弹控制台窗口（GUI 进程 spawn 控制台程序必带）
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// 子进程各立进程组：graceful 停止可按组送达 Ctrl+C（组 id = 子进程 pid）
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// 运行中的一组子进程（崩溃守护与退出序列的操作对象）。
pub struct Runtime {
    pub serve_port: u16,
    pub engine_port: u16,
    pub serve: Option<Child>,
    pub engine: Option<Child>,
}

/// 一次拉起所需的全部输入（job 由守护方持有，跨重启复用）。
pub struct LaunchContext<'a> {
    pub paths: &'a ResolvedPaths,
    pub console: &'a Console,
    pub engine_extra_args: &'a [String],
    pub serve_token: &'a str,
    pub engine_token: &'a str,
    pub version: &'a str,
    pub job: &'a Arc<JobHandle>,
    /// 自收编是否成功（决定子进程要不要逐个收编）
    pub self_assigned: bool,
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

/// 目录/配置/静态资源预备（幂等；初始启动与重启均可安全调用）。
pub fn prepare_workspace(ctx: &LaunchContext, log: &mut impl Write) -> Result<(), String> {
    let paths = ctx.paths;
    for dir in [&paths.data_dir, &paths.runtime_dir, &paths.logs_dir] {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    }
    std::fs::create_dir_all(paths.runtime_dir.join("web")).map_err(|e| e.to_string())?;
    ensure_junction(
        &paths.runtime_dir.join("web").join("dist"),
        &paths.evo_agent_home.join("web").join("dist"),
    )?;
    write_runtime_config(
        &paths.runtime_dir,
        &paths.evo_agent_home,
        &paths.app_config_dir,
        ctx.console,
        log,
    )
}

fn spawn_engine(ctx: &LaunchContext, engine_port: u16) -> Result<Child, String> {
    let paths = ctx.paths;
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
            ctx.engine_token,
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
        .args(ctx.engine_extra_args)
        .stdout(Stdio::from(engine_log))
        .stderr(Stdio::from(engine_log_err))
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    let engine = engine_cmd
        .spawn()
        .map_err(|e| format!("spawn evorule-server: {e}"))?;
    if !ctx.self_assigned {
        ctx.job.assign_pid(engine.id());
    }
    Ok(engine)
}

fn spawn_serve(ctx: &LaunchContext, serve_port: u16, engine_port: u16) -> Result<Child, String> {
    let paths = ctx.paths;
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
            ctx.serve_token,
            "--workdir",
        ])
        .arg(&paths.runtime_dir)
        .env(
            "EVO_AGENT_EVORULE__BASE_URL",
            format!("http://127.0.0.1:{engine_port}"),
        )
        .env("EVO_AGENT_EVORULE__API_KEY", ctx.engine_token)
        .env("EVORULE_AUTH_TOKEN", ctx.engine_token)
        .stdout(Stdio::from(serve_log))
        .stderr(Stdio::from(serve_log_err))
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    let serve = serve_cmd
        .spawn()
        .map_err(|e| format!("spawn evo-agent serve: {e}"))?;
    if !ctx.self_assigned {
        ctx.job.assign_pid(serve.id());
    }
    Ok(serve)
}

fn write_runtime_file(
    paths: &ResolvedPaths,
    version: &str,
    rt: &Runtime,
    serve_pid: u32,
    engine_pid: u32,
) -> Result<(), String> {
    let runtime_json = serde_json::json!({
        "version": version,
        "started_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "serve": { "port": rt.serve_port, "pid": serve_pid },
        "engine": { "port": rt.engine_port, "pid": engine_pid },
    });
    std::fs::write(
        paths.data_dir.join("runtime.json"),
        serde_json::to_string_pretty(&runtime_json).unwrap(),
    )
    .map_err(|e| format!("write runtime.json: {e}"))
}

/// 初始启动：预备工作区 → engine → 探测 → serve → 探测 → runtime.json。
pub fn launch(ctx: &LaunchContext, log: &mut impl Write) -> Result<Runtime, String> {
    writeln!(log, "[orch] launch begin (shell v{})", ctx.version).ok();
    prepare_workspace(ctx, log)?;

    let serve_port = pick_free_port(None)?;
    let engine_port = pick_free_port(Some(serve_port))?;

    let mut engine = spawn_engine(ctx, engine_port)?;
    if !health::wait_healthy(engine_port, "/api/health", Duration::from_secs(30)) {
        let _ = engine.kill();
        return Err(format!(
            "engine health check timeout (port {engine_port}); logs: {}",
            ctx.paths.logs_dir.display()
        ));
    }
    writeln!(log, "[orch] engine healthy on port {engine_port}").ok();

    let mut serve = spawn_serve(ctx, serve_port, engine_port)?;
    if !health::wait_healthy(serve_port, "/health", Duration::from_secs(30)) {
        let _ = serve.kill();
        return Err(format!(
            "serve health check timeout (port {serve_port}); logs: {}",
            ctx.paths.logs_dir.display()
        ));
    }
    writeln!(log, "[orch] serve healthy on port {serve_port}").ok();

    let rt = Runtime {
        serve_port,
        engine_port,
        serve: Some(serve),
        engine: Some(engine),
    };
    write_runtime_file(
        ctx.paths,
        ctx.version,
        &rt,
        rt.serve.as_ref().map(|c| c.id()).unwrap_or(0),
        rt.engine.as_ref().map(|c| c.id()).unwrap_or(0),
    )?;
    writeln!(log, "[orch] launch complete").ok();
    Ok(rt)
}

/// 整组重启（崩溃退避 / 手动触发共用）：复用工作区，端口重选，全链重拉。
pub fn restart_services(ctx: &LaunchContext, log: &mut impl Write) -> Result<Runtime, String> {
    writeln!(log, "[orch] group restart begin").ok();
    let serve_port = pick_free_port(None)?;
    let engine_port = pick_free_port(Some(serve_port))?;

    let mut engine = spawn_engine(ctx, engine_port)?;
    if !health::wait_healthy(engine_port, "/api/health", Duration::from_secs(30)) {
        let _ = engine.kill();
        return Err(format!("engine health check timeout (port {engine_port})"));
    }
    let mut serve = spawn_serve(ctx, serve_port, engine_port)?;
    if !health::wait_healthy(serve_port, "/health", Duration::from_secs(30)) {
        let _ = serve.kill();
        return Err(format!("serve health check timeout (port {serve_port})"));
    }
    let rt = Runtime {
        serve_port,
        engine_port,
        serve: Some(serve),
        engine: Some(engine),
    };
    write_runtime_file(
        ctx.paths,
        ctx.version,
        &rt,
        rt.serve.as_ref().map(|c| c.id()).unwrap_or(0),
        rt.engine.as_ref().map(|c| c.id()).unwrap_or(0),
    )?;
    writeln!(log, "[orch] group restart complete").ok();
    Ok(rt)
}

/// 尽力向子进程组送达 Ctrl+C（GUI 壳无控制台，AttachConsole 后按组发送）。
fn send_ctrl_c(pid: u32) {
    use windows_sys::Win32::System::Console::{
        AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, CTRL_C_EVENT,
    };
    unsafe {
        if AttachConsole(pid) != 0 {
            GenerateConsoleCtrlEvent(CTRL_C_EVENT, pid);
            FreeConsole();
        }
    }
}

/// 两段式停止：Ctrl+C 尽力送达 → 轮询至超时（5s）→ 强杀。数据面由
/// WAL 保障（engine 跨 kill -9 全存活已实证），强杀仅影响优雅收尾。
pub fn graceful_stop(
    engine: &mut Option<Child>,
    serve: &mut Option<Child>,
    timeout: Duration,
) {
    for slot in [engine.as_ref(), serve.as_ref()].into_iter().flatten() {
        send_ctrl_c(slot.id());
    }
    let start = Instant::now();
    loop {
        let engine_gone = engine
            .as_mut()
            .map(|c| c.try_wait().map(|s| s.is_some()).unwrap_or(true))
            .unwrap_or(true);
        let serve_gone = serve
            .as_mut()
            .map(|c| c.try_wait().map(|s| s.is_some()).unwrap_or(true))
            .unwrap_or(true);
        if engine_gone && serve_gone {
            return;
        }
        if start.elapsed() >= timeout {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if let Some(c) = engine { let _ = c.kill(); }
    if let Some(c) = serve { let _ = c.kill(); }
}
