// 进程编排：端口预分配（三口互斥）→ job 收编 → engine → 健康探测 →
// LLM 执行体（可选：exe+key 双条件，缺位降级不阻断）→ 健康探测 →
// serve（注入执行体端点与调用方令牌 env）→ 健康探测。
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
use std::path::{Path, PathBuf};
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
    /// LLM 执行体（可选组件：exe+key 双条件启用；未启用 = None）
    pub ai_plugin: Option<Child>,
    pub ai_plugin_port: Option<u16>,
}

/// 一次拉起所需的全部输入（job 由守护方持有，跨重启复用）。
pub struct LaunchContext<'a> {
    pub paths: &'a ResolvedPaths,
    pub console: &'a Console,
    pub engine_extra_args: &'a [String],
    pub serve_token: &'a str,
    pub engine_token: &'a str,
    /// 执行体调用方令牌（每次壳启动随机生成；仅 env 注入双方进程，
    /// 不落盘/不进命令行/不进日志——与 engine token 同款纪律）
    pub executor_token: &'a str,
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
/// exclude 列出已占定的兄弟端口（serve/engine/executor 三口互斥）。
fn pick_free_port(exclude: &[u16]) -> Result<u16, String> {
    for _ in 0..16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        drop(listener);
        if !exclude.contains(&port) {
            return Ok(port);
        }
    }
    Err("cannot pick a free port".into())
}

/// 端口可绑判定（bind 即释放；与 pick_free_port 同一释放-竞态窗语义）。
fn port_bindable(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// 读 runtime.json 记录的上次三口（粘滞候选；缺件/损坏一律 fail-soft 全空，
/// 不因状态文件问题改变选口行为）。
fn last_ports(paths: &ResolvedPaths) -> (Option<u16>, Option<u16>, Option<u16>) {
    let Ok(raw) = std::fs::read_to_string(paths.data_dir.join("runtime.json")) else {
        return (None, None, None);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return (None, None, None);
    };
    let port_of = |k: &str| {
        v.get(k)
            .and_then(|s| s.get("port"))
            .and_then(|p| p.as_u64())
            .and_then(|p| u16::try_from(p).ok())
    };
    (port_of("serve"), port_of("engine"), port_of("llm_executor"))
}

/// 粘滞选口（连接韧性核心）：上次端口未被兄弟口占用且可绑 → 复用；
/// 否则回落挑新口。组崩溃重启端口不变 = 壳跳过重导航、前端 WS 指数退避
/// 重连直接命中归位的 serve——用户可感「断」（整页刷新）的消点。
/// 被其他进程抢占时如实回落新口（此时壳会重导航，连接仍可恢复）。
fn sticky_or_free(preferred: Option<u16>, exclude: &[u16]) -> Result<u16, String> {
    match preferred {
        Some(p) if !exclude.contains(&p) && port_bindable(p) => Ok(p),
        _ => pick_free_port(exclude),
    }
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

    let base = [
        home.join("evo-agent.toml"),
        app_config_dir.join("evo-agent.toml"),
    ]
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
                writeln!(
                    log,
                    "[orch] console.enabled=true but console.dir empty; skipped"
                )
                .ok();
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
    // 安全令牌走环境变量注入(引擎支持 EVORULE_AUTH_TOKEN),不经进程命令行——
    // 命令行参数对本机所有进程可读,令牌上账面即暴露。
    engine_cmd
        .env("EVORULE_AUTH_TOKEN", ctx.engine_token)
        .args([
            "--profile",
            "development",
            "--addr",
            &format!("127.0.0.1:{engine_port}"),
            "--core-eval",
        ])
        .arg(
            paths
                .evorule_server_home
                .join("resources")
                .join("server_eval.json"),
        )
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

/// 用户工作文件夹偏好(38 号方案 X):向导/设置保存于壳配置目录的
/// `agent-workspace.txt`;None=用户未改过,用缺省(用户 Documents\evo-agent)。
pub(crate) fn workspace_pref(app_config_dir: &Path) -> Option<PathBuf> {
    let p = app_config_dir.join("agent-workspace.txt");
    let s = std::fs::read_to_string(p).ok()?;
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Some(PathBuf::from(s))
}

/// 写工作文件夹偏好(向导保存;空串=清除偏好回缺省)。
pub fn save_workspace_pref(app_config_dir: &Path, path: &str) -> Result<(), String> {
    let p = app_config_dir.join("agent-workspace.txt");
    std::fs::write(&p, path.trim()).map_err(|e| format!("write agent-workspace.txt: {e}"))
}

/// 缺省 agent 能力边界根:%USERPROFILE%\Documents\evo-agent。
/// (Documents 若被 OneDrive 等策略重定向,此处仍落本机 Documents——
/// 目录恒有效且用户可见,重定向场景差异可接受。)
pub(crate) fn default_agent_root() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join("Documents").join("evo-agent")
}

fn spawn_serve(
    ctx: &LaunchContext,
    serve_port: u16,
    engine_port: u16,
    ai_plugin_port: Option<u16>,
) -> Result<Child, String> {
    let paths = ctx.paths;
    let serve_log = open_append(&paths.logs_dir.join("serve.log"))?;
    let serve_log_err = open_append(&paths.logs_dir.join("serve.log"))?;
    let mut serve_cmd = Command::new(&paths.evo_agent_exe);
    // 38 号方案 X:agent 能力边界根(用户工作文件夹)与编排 workdir(runtime)
    // 解耦——用户文件夹缺省自动创建,用户在向导/设置里改过则以偏好为准
    let agent_root = workspace_pref(&paths.app_config_dir).unwrap_or_else(default_agent_root);
    let _ = std::fs::create_dir_all(&agent_root);
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
        .args(["--agent-root"])
        .arg(&agent_root)
        .env(
            "EVO_AGENT_EVORULE__BASE_URL",
            format!("http://127.0.0.1:{engine_port}"),
        )
        .env("EVO_AGENT_EVORULE__API_KEY", ctx.engine_token)
        .env("EVORULE_AUTH_TOKEN", ctx.engine_token)
        .stdout(Stdio::from(serve_log))
        .stderr(Stdio::from(serve_log_err))
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    // 执行体调用方令牌恒注入（无执行体时无害闲置）；端点仅在执行体在位时
    // 覆盖缺省 9130——serve 侧 env 优先链已就绪
    serve_cmd.env("EVORULE_LLM_EXECUTOR_TOKEN", ctx.executor_token);
    if let Some(p) = ai_plugin_port {
        serve_cmd.env(
            "EVORULE_LLM_EXECUTOR_URL",
            format!("http://127.0.0.1:{p}/llm/execute"),
        );
    }
    let serve = serve_cmd
        .spawn()
        .map_err(|e| format!("spawn evo-agent serve: {e}"))?;
    if !ctx.self_assigned {
        ctx.job.assign_pid(serve.id());
    }
    Ok(serve)
}

/// 已配置 key 的 provider 合并视图（内置目录+自定义；自定义清单损坏=留痕
/// 后仅按内置目录降级，绝不静默吞）。
fn configured_providers(paths: &ResolvedPaths) -> Vec<(crate::wizard::OwnedProvider, String)> {
    let all = match crate::wizard::read_all_providers(paths) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[orch] custom providers list corrupt, degrading to built-in only: {e}");
            crate::wizard::catalog_providers()
        }
    };
    all.into_iter()
        .filter_map(|p| crate::wizard::read_provider_key(paths, &p.name).map(|k| (p, k)))
        .collect()
}

/// 壳生成的执行体运行时配置（每次拉起覆写）。动态值——监听端口/引擎地址/
/// 白名单物料绝对路径——不允许静态模板承载；key 与调用方令牌一律走 env，
/// 不进本文件。server_auth_token 置空：本拓扑下执行体只被 serve 调用
/// （/llm/execute），不主动调引擎。
///
/// 多 provider 路由：已配置 key 的 provider（内置目录+自定义）生成为
/// `llm_providers` 节（按模型路由端点与凭据）；同时保留 `llm_execute_url`
/// 单端点形态——旧版捆绑插件忽略未知配置键时按单端点路径降级工作。
fn write_ai_plugin_config(
    paths: &ResolvedPaths,
    listen_port: u16,
    engine_port: u16,
) -> Result<(), String> {
    let allowlist = paths
        .evorule_server_home
        .join("agent-governance")
        .join("rules")
        .join("00_constraint_model_allowlist.json");
    let mut cfg = serde_json::json!({
        "listen_addr": format!("127.0.0.1:{listen_port}"),
        "server_base_url": format!("http://127.0.0.1:{engine_port}"),
        "server_auth_token": "",
        "llm_endpoint": "https://api.minimaxi.com/v1",
        "llm_execute_url": "https://api.minimaxi.com/v1/text/chatcompletion_v2",
        "llm_model": "MiniMax-M3",
        "llm_temperature": 0.2,
        "llm_timeout_ms": 60000,
        "model_allowlist_path": allowlist.display().to_string(),
    });
    let providers = configured_providers(paths);
    if !providers.is_empty() {
        cfg["llm_providers"] = serde_json::Value::Array(
            providers
                .iter()
                .map(|(p, _)| {
                    serde_json::json!({
                        "name": p.name,
                        "execute_url": p.execute_url,
                        "api_key_env": crate::wizard::provider_key_env(&p.name),
                        "models": p.models,
                    })
                })
                .collect(),
        );
    }
    let out = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    std::fs::write(paths.runtime_dir.join("ai-plugin.json"), out)
        .map_err(|e| format!("write ai-plugin.json: {e}"))
}

/// 主 key（legacy 单端点 env 兼容载体）：minimax 优先（单 key 时代语义），
/// 缺位取合并视图第一个已配置 provider。仅供旧版捆绑插件降级路径与
/// EVORULE_AI_PLUGIN_LLM_API_KEY env 使用；路由模式各 provider 用专属 env。
fn read_primary_llm_key(paths: &ResolvedPaths) -> Option<String> {
    match crate::wizard::read_provider_key(paths, "minimax") {
        Some(k) => Some(k),
        None => configured_providers(paths)
            .into_iter()
            .next()
            .map(|(_, k)| k),
    }
}

/// 执行体启用判定：exe 在位 且 任一 provider key 在位（对齐 console「产物
/// 在位即启用」先例）。缺任一 = 不拉起 + 一条日志指引，工作台其余功能照常。
fn llm_executor_ready(ctx: &LaunchContext, log: &mut impl Write) -> Option<String> {
    if !ctx.paths.ai_plugin_exe.is_file() {
        writeln!(
            log,
            "[orch] llm executor exe not found ({}); LLM disabled",
            ctx.paths.ai_plugin_exe.display()
        )
        .ok();
        return None;
    }
    match read_primary_llm_key(ctx.paths) {
        Some(k) => Some(k),
        None => {
            writeln!(
                log,
                "[orch] llm key file missing ({}); put your key there (single line) to enable LLM",
                ctx.paths.app_config_dir.join("llm-key.txt").display()
            )
            .ok();
            None
        }
    }
}

fn spawn_ai_plugin(
    ctx: &LaunchContext,
    listen_port: u16,
    engine_port: u16,
    llm_key: &str,
) -> Result<Child, String> {
    let paths = ctx.paths;
    write_ai_plugin_config(paths, listen_port, engine_port)?;
    let out_log = open_append(&paths.logs_dir.join("ai-plugin.log"))?;
    let err_log = open_append(&paths.logs_dir.join("ai-plugin.log"))?;
    let mut cmd = Command::new(&paths.ai_plugin_exe);
    cmd.arg("--config")
        .arg(paths.runtime_dir.join("ai-plugin.json"))
        .env("EVORULE_AI_PLUGIN_CALLER_TOKEN", ctx.executor_token)
        .env("EVORULE_AI_PLUGIN_LLM_API_KEY", llm_key);
    // 路由模式：各 provider 专属 key env（凭据只走 env，不落盘不进命令行）
    for (p, key) in configured_providers(paths) {
        cmd.env(crate::wizard::provider_key_env(&p.name), key);
    }
    cmd.stdout(Stdio::from(out_log))
        .stderr(Stdio::from(err_log))
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    let child = cmd.spawn().map_err(|e| format!("spawn ai-plugin: {e}"))?;
    if !ctx.self_assigned {
        ctx.job.assign_pid(child.id());
    }
    Ok(child)
}

/// 执行体单独重生（serve/engine 在位时执行体退出的轻量恢复）：复用原端口
/// ——serve 的执行体端点 env 不变——并按当前 engine 端口重生成配置。
pub fn restart_llm_executor(
    ctx: &LaunchContext,
    port: u16,
    engine_port: u16,
    log: &mut impl Write,
) -> Result<Child, String> {
    match llm_executor_ready(ctx, log) {
        Some(key) => spawn_ai_plugin(ctx, port, engine_port, &key),
        None => Err("llm executor prerequisites gone (exe/key)".into()),
    }
}

fn write_runtime_file(
    paths: &ResolvedPaths,
    version: &str,
    rt: &Runtime,
    serve_pid: u32,
    engine_pid: u32,
    ai_plugin_pid: Option<u32>,
) -> Result<(), String> {
    let runtime_json = serde_json::json!({
        "version": version,
        "started_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "serve": { "port": rt.serve_port, "pid": serve_pid },
        "engine": { "port": rt.engine_port, "pid": engine_pid },
        // null = 执行体未启用（exe/key 缺位）；令牌永不入本文件
        "llm_executor": rt.ai_plugin_port.map(|p| serde_json::json!({
            "port": p,
            "pid": ai_plugin_pid.unwrap_or(0),
        })),
    });
    std::fs::write(
        paths.data_dir.join("runtime.json"),
        serde_json::to_string_pretty(&runtime_json).unwrap(),
    )
    .map_err(|e| format!("write runtime.json: {e}"))
}

/// 全链拉起（初始启动与整组重启共用）：engine → 探测 → 执行体（可选）→
/// 探测 → serve → 探测 → runtime.json。执行体为可选组件：exe/key 缺位或
/// 健康失败一律降级不阻断（LLM 不可用，工作台其余功能照常）。
fn start_group(ctx: &LaunchContext, log: &mut impl Write) -> Result<Runtime, String> {
    let (last_serve, last_engine, last_exec) = last_ports(ctx.paths);
    let serve_port = sticky_or_free(last_serve, &[])?;
    let engine_port = sticky_or_free(last_engine, &[serve_port])?;
    let executor_port = sticky_or_free(last_exec, &[serve_port, engine_port])?;

    let mut engine = spawn_engine(ctx, engine_port)?;
    if !health::wait_healthy(engine_port, "/api/health", Duration::from_secs(30)) {
        let _ = engine.kill();
        return Err(format!("engine health check timeout (port {engine_port})"));
    }
    writeln!(log, "[orch] engine healthy on port {engine_port}").ok();

    let mut ai_plugin = None;
    let mut ai_plugin_port = None;
    if let Some(key) = llm_executor_ready(ctx, log) {
        match spawn_ai_plugin(ctx, executor_port, engine_port, &key) {
            Ok(mut child) => {
                if health::wait_healthy(executor_port, "/health", Duration::from_secs(15)) {
                    writeln!(log, "[orch] llm executor healthy on port {executor_port}").ok();
                    ai_plugin = Some(child);
                    ai_plugin_port = Some(executor_port);
                } else {
                    writeln!(
                        log,
                        "[orch] llm executor health timeout (port {executor_port}); LLM disabled"
                    )
                    .ok();
                    let _ = child.kill();
                }
            }
            Err(e) => {
                writeln!(log, "[orch] llm executor spawn failed: {e}").ok();
            }
        }
    }

    let mut serve = spawn_serve(ctx, serve_port, engine_port, ai_plugin_port)?;
    if !health::wait_healthy(serve_port, "/health", Duration::from_secs(30)) {
        let _ = serve.kill();
        return Err(format!("serve health check timeout (port {serve_port})"));
    }
    writeln!(log, "[orch] serve healthy on port {serve_port}").ok();

    let rt = Runtime {
        serve_port,
        engine_port,
        serve: Some(serve),
        engine: Some(engine),
        ai_plugin,
        ai_plugin_port,
    };
    write_runtime_file(
        ctx.paths,
        ctx.version,
        &rt,
        rt.serve.as_ref().map(|c| c.id()).unwrap_or(0),
        rt.engine.as_ref().map(|c| c.id()).unwrap_or(0),
        rt.ai_plugin.as_ref().map(|c| c.id()),
    )?;
    Ok(rt)
}

/// 初始启动：预备工作区 → 全链拉起（日志目录随错误提示，便于排障）。
pub fn launch(ctx: &LaunchContext, log: &mut impl Write) -> Result<Runtime, String> {
    writeln!(log, "[orch] launch begin (shell v{})", ctx.version).ok();
    prepare_workspace(ctx, log)?;
    let rt = start_group(ctx, log)?;
    writeln!(log, "[orch] launch complete").ok();
    Ok(rt)
}

/// 整组重启（崩溃退避 / 手动触发共用）：复用工作区，端口重选，全链重拉。
pub fn restart_services(ctx: &LaunchContext, log: &mut impl Write) -> Result<Runtime, String> {
    writeln!(log, "[orch] group restart begin").ok();
    let rt = start_group(ctx, log)?;
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
    ai_plugin: &mut Option<Child>,
    timeout: Duration,
) {
    for slot in [engine.as_ref(), serve.as_ref(), ai_plugin.as_ref()]
        .into_iter()
        .flatten()
    {
        send_ctrl_c(slot.id());
    }
    let start = Instant::now();
    loop {
        let all_gone = [engine.as_mut(), serve.as_mut(), ai_plugin.as_mut()]
            .into_iter()
            .flatten()
            .all(|c| c.try_wait().map(|s| s.is_some()).unwrap_or(true));
        if all_gone {
            return;
        }
        if start.elapsed() >= timeout {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if let Some(c) = engine {
        let _ = c.kill();
    }
    if let Some(c) = serve {
        let _ = c.kill();
    }
    if let Some(c) = ai_plugin {
        let _ = c.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_paths(tag: &str) -> ResolvedPaths {
        let root =
            std::env::temp_dir().join(format!("evo-shell-orch-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(root.join("runtime")).unwrap();
        std::fs::create_dir_all(root.join("app-config")).unwrap();
        ResolvedPaths {
            evo_agent_exe: root.join("evo-agent.exe"),
            evo_agent_home: root.clone(),
            evorule_server_exe: root.join("evorule-server.exe"),
            evorule_server_home: root.join("server-home"),
            ai_plugin_exe: root.join("evorule-ai-plugin.exe"),
            data_dir: root.clone(),
            runtime_dir: root.join("runtime"),
            engine_data_dir: root.join("engine-data"),
            logs_dir: root.join("logs"),
            app_config_dir: root.join("app-config"),
            console: crate::shell_config::Console::default(),
        }
    }

    /// 生成的执行体配置是跨进程契约（消费方 = server 仓 ai-plugin）：
    /// 动态值正确落位，令牌/key 绝不入文件（零落盘纪律）。
    #[test]
    fn ai_plugin_config_carries_dynamic_values_and_no_secrets() {
        let paths = scratch_paths("cfg");
        write_ai_plugin_config(&paths, 19131, 18080).unwrap();
        let raw = std::fs::read_to_string(paths.runtime_dir.join("ai-plugin.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["listen_addr"], "127.0.0.1:19131");
        assert_eq!(v["server_base_url"], "http://127.0.0.1:18080");
        assert_eq!(v["server_auth_token"], "");
        assert!(v["model_allowlist_path"]
            .as_str()
            .unwrap()
            .ends_with("00_constraint_model_allowlist.json"));
        // 零落盘红线：调用方令牌与 LLM key 的载体是 env，本文件不得出现键名
        assert!(v.get("caller_token").is_none());
        assert!(v.get("llm_api_key").is_none());
        assert!(v.get("EVORULE_AI_PLUGIN_CALLER_TOKEN").is_none());
        assert!(v.get("EVORULE_AI_PLUGIN_LLM_API_KEY").is_none());
        std::fs::remove_dir_all(&paths.data_dir).ok();
    }

    #[test]
    fn llm_key_file_trimmed_and_blank_rejected() {
        let paths = scratch_paths("key");
        // 缺文件 → None
        assert!(crate::wizard::read_llm_key(&paths).is_none());
        let key_file = paths.app_config_dir.join("llm-key.txt");
        // 空白 → None（视为未配置，不误启用）
        std::fs::write(&key_file, "   \n").unwrap();
        assert!(crate::wizard::read_llm_key(&paths).is_none());
        // 正常值 → trim 后返回
        std::fs::write(&key_file, "  sk-live-example \n").unwrap();
        assert_eq!(
            crate::wizard::read_llm_key(&paths).as_deref(),
            Some("sk-live-example")
        );
        std::fs::remove_dir_all(&paths.data_dir).ok();
    }

    #[test]
    fn ai_plugin_config_emits_llm_providers_for_configured_keys() {
        let paths = scratch_paths("prov-cfg");
        // 两家 provider 配 key（明文直写=读侧兼容形态；不经 DPAPI 依赖）
        let keys_dir = paths.app_config_dir.join("llm-keys");
        std::fs::create_dir_all(&keys_dir).unwrap();
        std::fs::write(keys_dir.join("deepseek.txt"), "sk-ds-test\n").unwrap();
        std::fs::write(keys_dir.join("kimi.txt"), "sk-kimi-test\n").unwrap();
        write_ai_plugin_config(&paths, 19132, 18080).unwrap();
        let raw = std::fs::read_to_string(paths.runtime_dir.join("ai-plugin.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let providers = v["llm_providers"].as_array().expect("providers emitted");
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0]["name"], "deepseek");
        assert_eq!(
            providers[0]["api_key_env"],
            "EVORULE_AI_PLUGIN_LLM_KEY_DEEPSEEK"
        );
        assert!(providers[0]["models"].as_array().unwrap().len() >= 1);
        assert_eq!(providers[1]["name"], "kimi");
        // 零落盘红线延伸：路由节同样不得出现 key 本体
        assert!(raw.contains("api_key_env") && !raw.contains("sk-ds-test"));
        // 单端点形态保留（旧版捆绑插件降级路径）
        assert!(v["llm_execute_url"]
            .as_str()
            .unwrap()
            .starts_with("https://"));
        // 主 key 兼容载体：无 minimax key 时回落目录序第一个
        let primary = read_primary_llm_key(&paths).unwrap();
        assert_eq!(primary, "sk-ds-test");
        std::fs::remove_dir_all(&paths.data_dir).ok();
    }

    #[test]
    fn ai_plugin_config_has_no_providers_section_when_no_keys() {
        let paths = scratch_paths("prov-empty");
        write_ai_plugin_config(&paths, 19133, 18080).unwrap();
        let raw = std::fs::read_to_string(paths.runtime_dir.join("ai-plugin.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(v.get("llm_providers").is_none(), "无 key 不生成路由节");
        assert!(read_primary_llm_key(&paths).is_none());
        std::fs::remove_dir_all(&paths.data_dir).ok();
    }

    #[test]
    fn ai_plugin_config_includes_custom_providers() {
        let paths = scratch_paths("custom-cfg");
        let custom = serde_json::json!([{
            "name": "myprov",
            "display": "My Provider",
            "execute_url": "https://api.example.com/v1/chat/completions",
            "models": ["model-a"]
        }]);
        std::fs::write(
            paths.app_config_dir.join("llm-custom-providers.json"),
            custom.to_string(),
        )
        .unwrap();
        let keys_dir = paths.app_config_dir.join("llm-keys");
        std::fs::create_dir_all(&keys_dir).unwrap();
        std::fs::write(keys_dir.join("myprov.txt"), "sk-custom\n").unwrap();
        write_ai_plugin_config(&paths, 19134, 18080).unwrap();
        let raw = std::fs::read_to_string(paths.runtime_dir.join("ai-plugin.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let providers = v["llm_providers"].as_array().expect("providers emitted");
        let mine = providers
            .iter()
            .find(|p| p["name"] == "myprov")
            .expect("custom provider routed");
        assert_eq!(
            mine["execute_url"],
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(mine["api_key_env"], "EVORULE_AI_PLUGIN_LLM_KEY_MYPROV");
        // 零落盘红线：自定义 key 本体不入配置文件
        assert!(!raw.contains("sk-custom"));
        std::fs::remove_dir_all(&paths.data_dir).ok();
    }

    // === 粘滞选口（连接韧性：同口重启=前端零刷新） ===

    #[test]
    fn sticky_or_free_prefers_free_port_and_falls_back() {
        // 候选口空闲 → 原样复用
        let free = pick_free_port(&[]).unwrap();
        assert_eq!(sticky_or_free(Some(free), &[]).unwrap(), free);
        // 候选口被排除 → 回落且不撞排除集
        let picked = sticky_or_free(Some(free), &[free]).unwrap();
        assert_ne!(picked, free);
        // 候选口被他人绑定 → 回落
        let held = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let held_port = held.local_addr().unwrap().port();
        assert_ne!(sticky_or_free(Some(held_port), &[]).unwrap(), held_port);
        // 无候选 → 挑新口
        assert_ne!(sticky_or_free(None, &[]).unwrap(), held_port);
    }

    #[test]
    fn last_ports_reads_runtime_json_fail_soft() {
        let paths = scratch_paths("ports");
        // 缺文件 → 全空
        assert_eq!(last_ports(&paths), (None, None, None));
        // 正常记录 → 三口读出（llm_executor 为 null）
        std::fs::write(
            paths.data_dir.join("runtime.json"),
            r#"{"serve":{"port":8081,"pid":1},"engine":{"port":18080,"pid":2},"llm_executor":null}"#,
        )
        .unwrap();
        assert_eq!(last_ports(&paths), (Some(8081), Some(18080), None));
        // 损坏 → 全空（不改变选口行为）
        std::fs::write(paths.data_dir.join("runtime.json"), "{not json").unwrap();
        assert_eq!(last_ports(&paths), (None, None, None));
        std::fs::remove_dir_all(&paths.data_dir).ok();
    }
}
