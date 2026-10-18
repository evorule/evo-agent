// 壳自有配置（shell.toml）与路径解析。
//
// 配置位置：%APPDATA%\evo-agent-desktop\shell.toml；文件不存在时全部缺省。
// M1 开发态通过显式绝对路径指向各仓产物；M2 打包后三项缺省值（exe 同目录）
// 即为安装目录布局，无需配置文件。

use std::path::PathBuf;

#[derive(serde::Deserialize, Default)]
pub struct ShellConfig {
    #[serde(default)]
    pub paths: Paths,
    #[serde(default)]
    pub console: Console,
    #[serde(default)]
    pub engine: Engine,
    #[serde(default)]
    pub ui: Ui,
    #[serde(default)]
    pub update: Update,
}

/// 壳层 UI 行为（M3）：关窗缺省最小化到托盘，close_directly=true 直接退出。
#[derive(serde::Deserialize, Default)]
pub struct Ui {
    #[serde(default)]
    pub close_directly: bool,
}

/// 自动更新链（M4）：缺省全开。endpoints 空 = 用 tauri.conf.json 内置端点；
/// 非空 = 完全覆盖（高级项，本地源调试用）。
#[derive(serde::Deserialize, Clone)]
pub struct Update {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_check_interval_hours")]
    pub check_interval_hours: u64,
    #[serde(default)]
    pub endpoints: Vec<String>,
}

impl Default for Update {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_hours: 24,
            endpoints: Vec::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_check_interval_hours() -> u64 {
    24
}

#[derive(serde::Deserialize, Default)]
pub struct Paths {
    /// evo-agent 可执行文件（缺省 = 壳 exe 同目录 \evo-agent.exe）
    pub evo_agent_exe: Option<PathBuf>,
    /// 产品资源根（含 agents\、web\dist\；缺省 = 壳 exe 同目录）
    pub evo_agent_home: Option<PathBuf>,
    /// evorule-server 可执行文件（缺省 = 壳 exe 同目录 \evorule-server.exe）
    pub evorule_server_exe: Option<PathBuf>,
    /// evorule-server 资源根（含 resources\、rules\；缺省 = 壳 exe 同目录）
    pub evorule_server_home: Option<PathBuf>,
}

/// console 审计页通道（预留）：enabled=true 时壳把 console_dir 写入 serve
/// 工作目录的 evo-agent.toml，由 serve 既有 fail-soft sidecar 机制拉起。
/// 缺省关闭（M1 决策 R4）；console 集成批次开启并指向 console 产物目录。
#[derive(serde::Deserialize, Default, Clone)]
pub struct Console {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub dir: Option<PathBuf>,
}

#[derive(serde::Deserialize, Default)]
pub struct Engine {
    /// 透传给 evorule-server 的额外参数（高级项）
    #[serde(default)]
    pub extra_args: Vec<String>,
}

/// 解析后的全部路径（launch 输入）。
#[derive(Clone)]
pub struct ResolvedPaths {
    pub evo_agent_exe: PathBuf,
    pub evo_agent_home: PathBuf,
    pub evorule_server_exe: PathBuf,
    /// evorule-server 资源根（宪法/规则文件；显式传 flag，不依赖 CWD）
    pub evorule_server_home: PathBuf,
    /// 壳数据根（%LOCALAPPDATA%\evo-agent-desktop）
    pub data_dir: PathBuf,
    /// serve 工作目录（data_dir\runtime）：壳生成的 evo-agent.toml、
    /// web\dist junction、serve 会话/快照数据都在这里
    pub runtime_dir: PathBuf,
    /// engine 数据目录（db/memory；wal 单列）
    pub engine_data_dir: PathBuf,
    /// 日志目录（data_dir\logs）
    pub logs_dir: PathBuf,
    /// 壳配置目录（%APPDATA%\evo-agent-desktop；用户 evo-agent.toml 透传查找位）
    pub app_config_dir: PathBuf,
}

fn shell_config_path() -> Option<PathBuf> {
    std::env::var("APPDATA").ok().map(|appdata| {
        PathBuf::from(appdata)
            .join("evo-agent-desktop")
            .join("shell.toml")
    })
}

pub fn load() -> ShellConfig {
    let path = match shell_config_path() {
        Some(p) => p,
        None => return ShellConfig::default(),
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return ShellConfig::default(),
    };
    match toml::from_str(&raw) {
        Ok(cfg) => cfg,
        Err(e) => {
            // 配置非法不阻断启动：告警后走全缺省（fail-soft）
            eprintln!("[shell] shell.toml parse failed ({}), using defaults", e);
            ShellConfig::default()
        }
    }
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

fn app_config_dir() -> Option<PathBuf> {
    std::env::var("APPDATA").ok().map(|d| PathBuf::from(d).join("evo-agent-desktop"))
}

/// 安装态优先：NSIS 资源布局（exe_dir\resources\...）存在即用之，否则回落
/// 开发态布局（exe 同目录直接放二进制/资源根，由 shell.toml 显式指定）。
fn installed_or(installed: PathBuf, fallback: PathBuf) -> PathBuf {
    if installed.exists() {
        installed
    } else {
        fallback
    }
}

pub fn resolve(cfg: &ShellConfig) -> Result<ResolvedPaths, String> {
    let exe_dir = exe_dir().ok_or("cannot resolve current exe dir")?;
    let evo_agent_exe = cfg
        .paths
        .evo_agent_exe
        .clone()
        .unwrap_or_else(|| {
            installed_or(
                exe_dir.join("resources").join("bin").join("evo-agent.exe"),
                exe_dir.join("evo-agent.exe"),
            )
        });
    let evo_agent_home = cfg
        .paths
        .evo_agent_home
        .clone()
        .unwrap_or_else(|| installed_or(exe_dir.join("resources").join("evo-agent"), exe_dir.clone()));
    let evorule_server_exe = cfg
        .paths
        .evorule_server_exe
        .clone()
        .unwrap_or_else(|| {
            installed_or(
                exe_dir.join("resources").join("bin").join("evorule-server.exe"),
                exe_dir.join("evorule-server.exe"),
            )
        });
    let evorule_server_home = cfg
        .paths
        .evorule_server_home
        .clone()
        .unwrap_or_else(|| installed_or(
            exe_dir.join("resources").join("evorule-server"),
            exe_dir.clone(),
        ));

    // 数据根用 identifier（与 NSIS currentUser 安装目录 %LOCALAPPDATA%\{productName} 隔离，
    // 防卸载危及用户数据）
    let data_dir = std::env::var("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("com.evorule.evo-agent-desktop"))
        .map_err(|_| "LOCALAPPDATA not set".to_string())?;
    let app_config = app_config_dir().unwrap_or_else(|| data_dir.join("config"));

    Ok(ResolvedPaths {
        evo_agent_exe,
        evo_agent_home,
        evorule_server_exe,
        evorule_server_home,
        runtime_dir: data_dir.join("runtime"),
        logs_dir: data_dir.join("logs"),
        engine_data_dir: data_dir.join("engine-data"),
        data_dir,
        app_config_dir: app_config,
    })
}
