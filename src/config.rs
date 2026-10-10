// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Configuration loading for evo-agent.
//!
//! 配置层级(优先级低→高,后者覆盖前者):
//! 1. **默认值** —— 代码中 `Config::default()`
//! 2. **用户配置** —— `~/.config/evo-agent/config.toml`(Linux/macOS)
//!    或 `%APPDATA%\evo-agent\config.toml`(Windows)
//! 3. **项目配置** —— `./evo-agent.toml`(由 `project_dir` 指定;字段级合并,
//!    文件中显式出现的字段才覆盖,未出现的字段保持上层值)
//! 4. **环境变量** —— `EVO_AGENT_*` 前缀,`__` 分隔 section/field
//!    (如 `EVO_AGENT_LLM__PROVIDER=openai`)
//!
//! LLM 配置的解析统一走 [`LlmConfig::resolve`](LlmConfig::resolve) 单一入口
//! (env 优先、toml 兜底),serve 与 CLI 共用同一语义:
//! - `EVO_AGENT_LLM__*` 显式环境变量优先;
//! - 其次裸 provider 环境变量(`MINIMAX_API_KEY` / `DEEPSEEK_API_KEY` /
//!   `OPENAI_API_KEY` 族,优先级同序,详见 `.env.example`);
//! - 都缺省时回落 toml 合并值或代码默认值。
//!
//! 配置文件支持 `${ENV:VAR_NAME}` 占位符(用于 `api_key` 字段),加载时展开为环境变量值。
//!
//! # 配置文件示例
//! ```toml
//! [llm]
//! provider = "minimax"
//! api_key = "${ENV:MINIMAX_API_KEY}"
//! model = "MiniMax-M2.5"
//!
//! [evorule]
//! base_url = "http://localhost:18080"
//!
//! [agents]
//! dir = "./agents"
//! default = "general"
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// LLM provider 配置
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmConfig {
    /// provider 名称:`minimax` | `deepseek` | `openai`
    ///
    /// 展示预留字段:当前仅随脱敏状态端点回显,不参与请求路由
    /// (实际请求端点由 `api_base`/`model` 决定)。未知值装载时出警告
    /// 日志但不拒绝(为未来多 provider 路由预留)。
    #[serde(default = "LlmConfig::default_provider")]
    pub provider: String,
    /// API key,支持 `${ENV:VAR_NAME}` 占位符
    #[serde(default = "LlmConfig::default_api_key")]
    pub api_key: String,
    /// 模型名
    #[serde(default = "LlmConfig::default_model")]
    pub model: String,
    /// API endpoint
    #[serde(default = "LlmConfig::default_api_base")]
    pub api_base: String,
    /// 请求超时(秒)
    #[serde(default = "default_llm_timeout")]
    pub timeout_secs: u64,
    /// 失败重试次数
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
}

impl LlmConfig {
    /// 默认 provider
    pub fn default_provider() -> String {
        "minimax".to_string()
    }
    /// 默认 api_key(占位符,加载时展开)
    pub fn default_api_key() -> String {
        "${ENV:MINIMAX_API_KEY}".to_string()
    }
    /// 默认 model
    pub fn default_model() -> String {
        "MiniMax-M2.5".to_string()
    }
    /// 默认 api_base(国际版;api.minimax.io 为国内版域名,sk-cp- 国际版 key 无效)
    pub fn default_api_base() -> String {
        "https://api.minimaxi.com/v1/text/chatcompletion_v2".to_string()
    }
}

fn default_llm_timeout() -> u64 {
    60
}

fn default_max_retries() -> usize {
    3
}

/// LLM 配置只读状态(脱敏快照)
///
/// 凭据可视化端点 `GET /admin/llm-status` 的响应体。设计铁律:**不携带任何
/// 密钥内容**——仅报告是否存在、末 4 位提示(长度 ≥8 才出具)与来源变量名。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LlmStatusSnapshot {
    /// LLM 是否可用(key 已配置)
    pub configured: bool,
    /// provider 名称
    pub provider: String,
    /// 模型名
    pub model: String,
    /// API 端点(去除 query/fragment,不含凭据)
    pub api_base: String,
    /// API key 脱敏状态
    pub api_key: ApiKeyStatus,
}

/// API key 脱敏状态
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ApiKeyStatus {
    /// 是否已配置
    pub present: bool,
    /// 末 4 位提示(长度 < 8 时不出具,防短 key 猜测)
    pub hint: Option<String>,
    /// 来源:环境变量名(如 `MINIMAX_API_KEY`)或 `"config"`
    pub source: Option<String>,
}

impl LlmStatusSnapshot {
    /// 未配置态快照(state 兼容路径的默认值)
    pub fn unconfigured() -> Self {
        Self {
            configured: false,
            provider: String::new(),
            model: String::new(),
            api_base: String::new(),
            api_key: ApiKeyStatus {
                present: false,
                hint: None,
                source: None,
            },
        }
    }
}

impl LlmConfig {
    /// LLM 配置统一解析入口(env 优先、toml 兜底)
    ///
    /// serve 与 CLI 共用此函数,消灭「一个入口只看裸环境变量、另一个只看
    /// toml」的双真相。`base` 为 toml 合并后的基线(user + project 配置,
    /// 或代码默认值)。字段独立判定优先级:
    /// 1. `EVO_AGENT_LLM__*` 显式环境变量(部署注入,最高优先);
    /// 2. 裸 provider 环境变量(`MINIMAX_API_KEY` / `DEEPSEEK_API_KEY` /
    ///    `OPENAI_API_KEY`,按此顺序取第一个设置者):key 兜底 `api_key`;
    ///    `model`/`api_base` 取对应伴随变量(`{P}_MODEL` / `{P}_API_BASE`),
    ///    伴随变量未设置且基线值仍为出厂默认时换用该 provider 的默认组
    ///    ——已被 toml 定制的值不会被裸环境变量冲掉;
    /// 3. 都缺省时保持 `base`(toml 合并值或代码默认)。
    pub fn resolve(base: LlmConfig) -> LlmConfig {
        Self::resolve_with_source(base).0
    }

    /// 同 [`resolve`](Self::resolve),附带返回 api_key 的来源环境变量名
    /// (供脱敏状态端点记录;未命中任何环境变量时为 `None`)。
    pub fn resolve_with_source(mut base: LlmConfig) -> (LlmConfig, Option<String>) {
        let explicit_key = apply_explicit_llm_env(&mut base);
        apply_bare_provider_env(&mut base, explicit_key.is_some());
        (base, explicit_key.or_else(bare_provider_key_source))
    }
}

/// 应用 `EVO_AGENT_LLM__*` 显式环境变量覆盖,返回 api_key 是否被其接管
///
/// 各字段独立判定(设置即覆盖,互不依赖);仅 api_key 的接管与否决定
/// 是否继续走裸 provider 环境变量兜底。
fn apply_explicit_llm_env(cfg: &mut LlmConfig) -> Option<String> {
    if let Ok(v) = std::env::var("EVO_AGENT_LLM__PROVIDER") {
        cfg.provider = v;
    }
    if let Ok(v) = std::env::var("EVO_AGENT_LLM__MODEL") {
        cfg.model = v;
    }
    if let Ok(v) = std::env::var("EVO_AGENT_LLM__API_BASE") {
        cfg.api_base = v;
    }
    if let Ok(v) = std::env::var("EVO_AGENT_LLM__TIMEOUT_SECS") {
        if let Ok(n) = v.parse() {
            cfg.timeout_secs = n;
        }
    }
    if let Ok(v) = std::env::var("EVO_AGENT_LLM__MAX_RETRIES") {
        if let Ok(n) = v.parse() {
            cfg.max_retries = n;
        }
    }
    match std::env::var("EVO_AGENT_LLM__API_KEY") {
        Ok(v) => {
            cfg.api_key = v;
            Some("EVO_AGENT_LLM__API_KEY".to_string())
        }
        Err(_) => None,
    }
}

/// 应用裸 provider 环境变量兜底(MiniMax > DeepSeek > OpenAI,取第一个设置者)
///
/// `explicit_key_taken` 为 `true`(api_key 已被 `EVO_AGENT_LLM__API_KEY`
/// 接管)时整体跳过——显式配置环境变量是部署方的完整意志,不需要裸环境
/// 变量再补语义。`model`/`api_base` 仅在基线仍为出厂默认(未被 toml 或
/// 显式环境变量定制)时才切到该 provider 的默认组,已定制的值不覆盖。
fn apply_bare_provider_env(cfg: &mut LlmConfig, explicit_key_taken: bool) {
    if explicit_key_taken {
        return;
    }
    let (key, model_var, base_var, def_model, def_base) =
        if let Ok(k) = std::env::var("MINIMAX_API_KEY") {
            (
                k,
                "MINIMAX_MODEL",
                "MINIMAX_API_BASE",
                "MiniMax-M2.5",
                "https://api.minimaxi.com/v1/text/chatcompletion_v2",
            )
        } else if let Ok(k) = std::env::var("DEEPSEEK_API_KEY") {
            (
                k,
                "DEEPSEEK_MODEL",
                "DEEPSEEK_API_BASE",
                "deepseek-chat",
                "https://api.deepseek.com/v1/chat/completions",
            )
        } else if let Ok(k) = std::env::var("OPENAI_API_KEY") {
            (
                k,
                "OPENAI_MODEL",
                "OPENAI_API_BASE",
                "gpt-4o-mini",
                "https://api.openai.com/v1/chat/completions",
            )
        } else {
            return;
        };
    cfg.api_key = key;
    // model/api_base:基线已被显式 env 或 toml 定制(≠出厂默认)时不动,
    // 保持「环境变量只补缺省、不冲掉显式定制」的兜底语义
    if let Ok(v) = std::env::var(model_var) {
        cfg.model = v;
    } else if cfg.model == LlmConfig::default_model() {
        cfg.model = def_model.to_string();
    }
    if let Ok(v) = std::env::var(base_var) {
        cfg.api_base = v;
    } else if cfg.api_base == LlmConfig::default_api_base() {
        cfg.api_base = def_base.to_string();
    }
}

/// 裸 provider 环境变量命中时的 api_key 来源名(脱敏状态端点用)
fn bare_provider_key_source() -> Option<String> {
    for name in ["MINIMAX_API_KEY", "DEEPSEEK_API_KEY", "OPENAI_API_KEY"] {
        if std::env::var(name).is_ok() {
            return Some(name.to_string());
        }
    }
    None
}

impl LlmConfig {
    /// 生成脱敏状态快照
    ///
    /// `key_source` 为配置加载期记录的密钥来源
    /// (见 `Config::llm_api_key_source`),透传不加工。
    pub fn status_snapshot(&self, key_source: Option<&str>) -> LlmStatusSnapshot {
        let present = !self.api_key.is_empty();
        let hint = if self.api_key.chars().count() >= 8 {
            let tail: String = self
                .api_key
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            Some(tail)
        } else {
            None
        };
        // 端点仅保留协议+主机+路径,去掉 query/fragment(防 URL 携带凭据参数)
        let api_base = self
            .api_base
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .to_string();
        LlmStatusSnapshot {
            configured: present,
            provider: self.provider.clone(),
            model: self.model.clone(),
            api_base,
            api_key: ApiKeyStatus {
                present,
                hint,
                source: key_source.map(|s| s.to_string()),
            },
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: Self::default_provider(),
            api_key: Self::default_api_key(),
            model: Self::default_model(),
            api_base: Self::default_api_base(),
            timeout_secs: default_llm_timeout(),
            max_retries: default_max_retries(),
        }
    }
}

/// evorule HTTP 服务连接配置
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvoruleConfig {
    /// evorule base URL
    #[serde(default = "EvoruleConfig::default_base_url")]
    pub base_url: String,
    /// evorule API key(留空 = 假定 evorule 本身无鉴权)
    #[serde(default = "EvoruleConfig::default_api_key")]
    pub api_key: String,
    /// HTTP 请求超时(秒)
    #[serde(default = "default_evorule_timeout")]
    pub timeout_secs: u64,
    /// 服务工具白名单:启动时经 GET /api/services 发现,仅注册本列表内的
    /// server 插件服务为 agent 代理工具(执行经 POST /api/services/{name}/invoke)。
    /// 空列表 = 不注册任何服务工具(安全默认)。
    #[serde(default)]
    pub service_tools: Vec<String>,
}

fn default_evorule_timeout() -> u64 {
    30
}

impl EvoruleConfig {
    /// TODO: doc
    pub fn default_base_url() -> String {
        "http://localhost:18080".to_string()
    }
    /// TODO: doc
    pub fn default_api_key() -> String {
        String::new()
    }
}

impl Default for EvoruleConfig {
    fn default() -> Self {
        Self {
            base_url: Self::default_base_url(),
            api_key: Self::default_api_key(),
            timeout_secs: default_evorule_timeout(),
            service_tools: Vec::new(),
        }
    }
}

/// 日志配置
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LoggingConfig {
    /// 日志级别: `trace` | `debug` | `info` | `warn` | `error`
    #[serde(default = "LoggingConfig::default_level")]
    pub level: String,
    /// 日志格式: `json` | `pretty`
    #[serde(default = "LoggingConfig::default_format")]
    pub format: String,
}

impl LoggingConfig {
    /// TODO: doc
    pub fn default_level() -> String {
        "info".to_string()
    }
    /// TODO: doc
    pub fn default_format() -> String {
        "pretty".to_string()
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: Self::default_level(),
            format: Self::default_format(),
        }
    }
}

/// G7:HTTP API 鉴权配置
///
/// ```toml
/// [auth]
/// enabled = true
/// tokens = ["secret-token-1", "secret-token-2"]
/// ```
///
/// 环境变量:`EVO_AGENT_AUTH__ENABLED=true`、`EVO_AGENT_AUTH__TOKENS=t1,t2`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AuthConfigFile {
    /// 是否启用鉴权(默认 false,开发模式)
    #[serde(default)]
    pub enabled: bool,
    /// 合法 token 列表
    #[serde(default)]
    pub tokens: Vec<String>,
}

impl AuthConfigFile {
    /// 转换为 API 层的 `AuthConfig`
    pub fn to_auth_config(&self) -> crate::api::auth::AuthConfig {
        crate::api::auth::AuthConfig::new(self.tokens.clone(), self.enabled)
    }
}

/// AgentDefinition 配置
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentsConfig {
    /// AgentDefinition 文件目录(查找 `<dir>/<agent_type>.json`)
    #[serde(default = "AgentsConfig::default_dir")]
    pub dir: PathBuf,
    /// 默认 agent(不指定 --agent 时使用)
    #[serde(default = "AgentsConfig::default_default")]
    pub default: String,
}

impl AgentsConfig {
    /// TODO: doc
    pub fn default_dir() -> PathBuf {
        PathBuf::from("./agents")
    }
    /// TODO: doc
    pub fn default_default() -> String {
        "general".to_string()
    }
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            dir: Self::default_dir(),
            default: Self::default_default(),
        }
    }
}

/// G12:MCP server 配置(单个 server)
///
/// ```toml
/// [[mcp.servers]]
/// name = "filesystem"
/// command = "npx"
/// args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
/// env = { GITHUB_TOKEN = "ghp_..." }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServerConfig {
    /// server 名称(用于工具名前缀 `mcp_{name}_{tool}`)
    pub name: String,
    /// 启动命令(如 `npx` / `node` / `python`)
    pub command: String,
    /// 命令参数
    #[serde(default)]
    pub args: Vec<String>,
    /// 环境变量(注入子进程,用于 token 等)
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// G12:MCP 配置(包含多个 server)
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct McpConfig {
    /// MCP server 列表
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// 完整配置
///
/// 任何字段都可缺失(用 `#[serde(default)]`),分层合并时只覆盖有值的字段。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Config {
    #[serde(default)]
    /// TODO: doc
    pub llm: LlmConfig,
    #[serde(default)]
    /// TODO: doc
    pub evorule: EvoruleConfig,
    #[serde(default)]
    /// TODO: doc
    pub logging: LoggingConfig,
    #[serde(default)]
    /// TODO: doc
    pub agents: AgentsConfig,
    #[serde(default)]
    /// G7:HTTP API 鉴权配置
    pub auth: AuthConfigFile,
    #[serde(default)]
    /// G12:MCP server 配置(可配置多个 stdio MCP server)
    pub mcp: McpConfig,
    #[serde(default)]
    /// 工作台附加配置(console 审计页自动拉起等,纯展示层)
    pub workbench: WorkbenchConfig,
    /// LLM API key 的来源记录(脱敏状态端点用;不携带任何密钥内容)
    ///
    /// 取值:`Some(环境变量名)`(如 `MINIMAX_API_KEY` / `EVO_AGENT_LLM__API_KEY`)
    /// 或 `Some("config")`(配置文件字面量)。`None` = 未配置。
    /// 由 `apply_env_overrides` / `resolve_env_placeholders*` 在加载期填充。
    #[serde(skip)]
    pub llm_api_key_source: Option<String>,
}

/// 工作台附加配置(纯展示层,零引擎/协议触碰)
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct WorkbenchConfig {
    /// console-cloud 仓目录(本机路径,如 `C:\path\to\console-cloud`)。
    /// 配置后 serve 启动期自动拉起其 dev server(工作台审计深链的数据源);
    /// 缺省空 = 不启用(公开部署零耦合、零副作用)。
    #[serde(default)]
    pub console_dir: Option<String>,
    /// console dev server 端口(缺省 5174,与 console-cloud vite 配置一致)
    #[serde(default)]
    pub console_port: Option<u16>,
}

/// 配置加载/解析错误
#[derive(Debug)]
pub enum ConfigError {
    /// IO 错误(读配置文件)
    Io(std::io::Error),
    /// TOML 解析错误
    Parse(toml::de::Error),
    /// `${ENV:VAR_NAME}` 引用的环境变量未设置
    EnvVarNotFound(String),
    /// 字段值非法(如 URL 协议错、日志级别未知等)
    InvalidValue {
        /// 字段路径(如 "llm.provider" / "evorule.base_url")
        field: String,
        /// 详细原因
        reason: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "config IO error: {}", e),
            ConfigError::Parse(e) => write!(f, "config TOML parse error: {}", e),
            ConfigError::EnvVarNotFound(v) => {
                write!(f, "config references ${{ENV:{}}} but it is not set", v)
            }
            ConfigError::InvalidValue { field, reason } => {
                write!(f, "config invalid value at {}: {}", field, reason)
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(e) => Some(e),
            ConfigError::Parse(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl Config {
    /// 加载配置,合并所有层级
    ///
    /// `project_dir` 是项目根目录,会在该目录下查找 `evo-agent.toml`。
    ///
    /// 此方法为严格模式:LLM API key 必须存在,否则报错。
    /// 适用于 `run` 命令(实际需要调用 LLM)。
    pub fn load(project_dir: &Path) -> Result<Self, ConfigError> {
        let mut config = Config::default();

        // Layer 2: user config
        if let Some(user_path) = user_config_path() {
            if user_path.exists() {
                config.merge_from_file(&user_path)?;
            }
        }

        // Layer 3: project config
        let project_path = project_dir.join("evo-agent.toml");
        if project_path.exists() {
            config.merge_from_file(&project_path)?;
        }

        // Layer 4: env vars(最高优先,覆盖文件)
        config.apply_env_overrides();

        // 解析 ${ENV:VAR} 占位符
        config.resolve_env_placeholders()?;

        // 验证
        config.validate()?;

        Ok(config)
    }

    /// 加载配置(宽松模式,用于 list/tools/config 等只读命令)
    ///
    /// 与 [`load`](Self::load) 的区别:
    /// - `${ENV:VAR}` 占位符未设置时替换为空字符串(不报错)
    /// - 跳过 LLM API key 非空验证
    ///
    /// 这样 `list`/`tools list`/`config` 等只读命令不依赖 LLM API key,
    /// 用户可以在未配置 LLM 的情况下浏览 agents 和工具。
    pub fn load_lenient(project_dir: &Path) -> Result<Self, ConfigError> {
        let mut config = Config::default();

        if let Some(user_path) = user_config_path() {
            if user_path.exists() {
                config.merge_from_file(&user_path)?;
            }
        }

        let project_path = project_dir.join("evo-agent.toml");
        if project_path.exists() {
            config.merge_from_file(&project_path)?;
        }

        config.apply_env_overrides();

        // 宽松解析:占位符未设置时替换为空字符串
        config.resolve_env_placeholders_lenient();

        // 宽松验证:跳过 LLM API key 非空检查
        config.validate_lenient()?;

        Ok(config)
    }

    /// 从单个配置文件合并(字段级深合并)
    ///
    /// 文件中**显式出现**的字段覆盖当前值(默认值或上层配置文件的值),
    /// 未出现的字段保持当前值——多层配置(user + project)叠加时各自只
    /// 贡献自己声明的字段,不会整段互覆。数组字段(如 `mcp.servers`)按
    /// 字段整体覆盖(数组元素不做逐项合并)。
    fn merge_from_file(&mut self, path: &Path) -> Result<(), ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let value: toml::Value = content.parse().map_err(ConfigError::Parse)?;

        if let Some(table) = value.get("llm").and_then(|v| v.as_table()) {
            // 迁移提示:该字段从未接入实际消费,上下文窗口由 agent 定义的
            // context_window_tokens 决定;旧配置文件里出现时提示但不拒绝装载
            if table.contains_key("context_window_tokens") {
                tracing::warn!(
                    "{}: llm.context_window_tokens 已移除(从未接入实际消费,实际窗口由 agent 定义的 context_window_tokens 决定);该键被忽略",
                    path.display()
                );
            }
            merge_field(table, "provider", &mut self.llm.provider)?;
            merge_field(table, "api_key", &mut self.llm.api_key)?;
            merge_field(table, "model", &mut self.llm.model)?;
            merge_field(table, "api_base", &mut self.llm.api_base)?;
            merge_field(table, "timeout_secs", &mut self.llm.timeout_secs)?;
            merge_field(table, "max_retries", &mut self.llm.max_retries)?;
        }
        if let Some(table) = value.get("evorule").and_then(|v| v.as_table()) {
            merge_field(table, "base_url", &mut self.evorule.base_url)?;
            merge_field(table, "api_key", &mut self.evorule.api_key)?;
            merge_field(table, "timeout_secs", &mut self.evorule.timeout_secs)?;
            merge_field(table, "service_tools", &mut self.evorule.service_tools)?;
        }
        if let Some(table) = value.get("logging").and_then(|v| v.as_table()) {
            merge_field(table, "level", &mut self.logging.level)?;
            merge_field(table, "format", &mut self.logging.format)?;
        }
        if let Some(table) = value.get("agents").and_then(|v| v.as_table()) {
            merge_field(table, "dir", &mut self.agents.dir)?;
            merge_field(table, "default", &mut self.agents.default)?;
        }
        if let Some(table) = value.get("auth").and_then(|v| v.as_table()) {
            merge_field(table, "enabled", &mut self.auth.enabled)?;
            merge_field(table, "tokens", &mut self.auth.tokens)?;
        }
        if let Some(table) = value.get("mcp").and_then(|v| v.as_table()) {
            merge_field(table, "servers", &mut self.mcp.servers)?;
        }
        if let Some(table) = value.get("workbench").and_then(|v| v.as_table()) {
            merge_field(table, "console_dir", &mut self.workbench.console_dir)?;
            merge_field(table, "console_port", &mut self.workbench.console_port)?;
        }

        Ok(())
    }

    /// 应用环境变量覆盖
    ///
    /// 约定: `EVO_AGENT_<SECTION>__<FIELD>=value`
    /// 例如: `EVO_AGENT_LLM__PROVIDER=openai` → `config.llm.provider = "openai"`
    ///
    /// llm 段统一走 [`LlmConfig::resolve`](LlmConfig::resolve) 单一入口
    /// (env 优先、toml 兜底;含裸 provider 环境变量兜底),serve 与 CLI 共用。
    fn apply_env_overrides(&mut self) {
        // llm 段:统一解析入口(覆盖当前 toml 合并基线)
        let base = std::mem::take(&mut self.llm);
        let (resolved, key_source) = LlmConfig::resolve_with_source(base);
        self.llm = resolved;
        if key_source.is_some() {
            self.llm_api_key_source = key_source;
        }

        if let Ok(v) = std::env::var("EVO_AGENT_EVORULE__BASE_URL") {
            self.evorule.base_url = v;
        }
        if let Ok(v) = std::env::var("EVO_AGENT_EVORULE__API_KEY") {
            self.evorule.api_key = v;
        }
        if let Ok(v) = std::env::var("EVO_AGENT_EVORULE__TIMEOUT_SECS") {
            if let Ok(n) = v.parse() {
                self.evorule.timeout_secs = n;
            }
        }
        // 服务工具白名单(逗号分隔;容器化部署经环境变量注入)
        if let Ok(v) = std::env::var("EVO_AGENT_EVORULE__SERVICE_TOOLS") {
            self.evorule.service_tools = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }

        if let Ok(v) = std::env::var("EVO_AGENT_LOGGING__LEVEL") {
            self.logging.level = v;
        }
        if let Ok(v) = std::env::var("EVO_AGENT_LOGGING__FORMAT") {
            self.logging.format = v;
        }

        if let Ok(v) = std::env::var("EVO_AGENT_AGENTS__DIR") {
            self.agents.dir = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("EVO_AGENT_AGENTS__DEFAULT") {
            self.agents.default = v;
        }

        // MCP server 配置(JSON 数组;容器化部署经环境变量注入)。
        // 解析失败仅告警不拒绝装载(与整段配置容忍语义一致,避免单变量
        // 拼写错误让整个进程起不来)。
        if let Ok(v) = std::env::var("EVO_AGENT_MCP__SERVERS") {
            match serde_json::from_str::<Vec<McpServerConfig>>(&v) {
                Ok(servers) => self.mcp.servers = servers,
                Err(e) => {
                    tracing::warn!(
                        "EVO_AGENT_MCP__SERVERS 不是合法的 MCP server JSON 数组,已忽略: {e}"
                    );
                }
            }
        }

        // G7:鉴权配置
        if let Ok(v) = std::env::var("EVO_AGENT_AUTH__ENABLED") {
            self.auth.enabled = v == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("EVO_AGENT_AUTH__TOKENS") {
            self.auth.tokens = v.split(',').map(|s| s.trim().to_string()).collect();
        }
    }

    /// 解析 `${ENV:VAR_NAME}` 占位符
    fn resolve_env_placeholders(&mut self) -> Result<(), ConfigError> {
        self.record_llm_api_key_source();
        self.llm.api_key = resolve_env_placeholder(&self.llm.api_key)?;
        if !self.evorule.api_key.is_empty() {
            self.evorule.api_key = resolve_env_placeholder(&self.evorule.api_key)?;
        }
        Ok(())
    }

    /// 在占位符解析前记录 API key 的来源(脱敏状态端点用,不含密钥内容)
    ///
    /// 优先级低于 `EVO_AGENT_LLM__API_KEY` 覆盖分支(该分支已先行记录)。
    fn record_llm_api_key_source(&mut self) {
        if self.llm_api_key_source.is_some() || self.llm.api_key.is_empty() {
            return;
        }
        let source = self
            .llm
            .api_key
            .strip_prefix("${ENV:")
            .and_then(|s| s.strip_suffix('}'))
            .map(|var| var.to_string())
            .unwrap_or_else(|| "config".to_string());
        self.llm_api_key_source = Some(source);
    }

    /// 宽松解析 `${ENV:VAR_NAME}` 占位符
    ///
    /// 与 [`resolve_env_placeholders`](Self::resolve_env_placeholders) 的区别:
    /// 环境变量未设置时替换为空字符串,不报错。
    fn resolve_env_placeholders_lenient(&mut self) {
        self.record_llm_api_key_source();
        self.llm.api_key = resolve_env_placeholder_lenient(&self.llm.api_key);
        if !self.evorule.api_key.is_empty() {
            self.evorule.api_key = resolve_env_placeholder_lenient(&self.evorule.api_key);
        }
    }

    /// 验证配置值的合法性
    fn validate(&self) -> Result<(), ConfigError> {
        // provider 为展示预留字段:未知值仅警告不拒绝(实际请求端点由
        // api_base/model 决定,旧配置文件里的自定义值装载不受影响)
        match self.llm.provider.as_str() {
            "minimax" | "deepseek" | "openai" => {}
            other => tracing::warn!(
                "llm.provider '{}' 当前仅作展示预留,不参与请求路由(实际端点由 api_base/model 决定);未来多 provider 路由接入时将消费此字段",
                other
            ),
        }

        // LLM API key 不能为空
        if self.llm.api_key.is_empty() {
            return Err(ConfigError::InvalidValue {
                field: "llm.api_key".to_string(),
                reason: "must not be empty (set directly or via ${{ENV:VAR}})".to_string(),
            });
        }

        // evorule base_url 必须是 http:// 或 https:// 开头
        if !self.evorule.base_url.starts_with("http://")
            && !self.evorule.base_url.starts_with("https://")
        {
            return Err(ConfigError::InvalidValue {
                field: "evorule.base_url".to_string(),
                reason: format!(
                    "must start with http:// or https://, got '{}'",
                    self.evorule.base_url
                ),
            });
        }

        // logging.level 必须是已知值
        match self.logging.level.as_str() {
            "trace" | "debug" | "info" | "warn" | "error" => {}
            other => {
                return Err(ConfigError::InvalidValue {
                    field: "logging.level".to_string(),
                    reason: format!(
                        "unknown level '{}', expected one of: trace, debug, info, warn, error",
                        other
                    ),
                });
            }
        }

        // logging.format 必须是已知值
        match self.logging.format.as_str() {
            "json" | "pretty" => {}
            other => {
                return Err(ConfigError::InvalidValue {
                    field: "logging.format".to_string(),
                    reason: format!("unknown format '{}', expected one of: json, pretty", other),
                });
            }
        }

        Ok(())
    }

    /// 宽松验证(用于 list/tools/config 等只读命令)
    ///
    /// 与 [`validate`](Self::validate) 的区别:
    /// - 跳过 LLM API key 非空验证
    /// - 仍然验证 base_url/logging 等非 LLM 字段
    fn validate_lenient(&self) -> Result<(), ConfigError> {
        // provider 为展示预留字段:未知值仅警告不拒绝(同严格模式口径)
        match self.llm.provider.as_str() {
            "minimax" | "deepseek" | "openai" => {}
            other => tracing::warn!(
                "llm.provider '{}' 当前仅作展示预留,不参与请求路由(实际端点由 api_base/model 决定);未来多 provider 路由接入时将消费此字段",
                other
            ),
        }

        // 注意:宽松模式跳过 LLM API key 非空检查

        // evorule base_url 必须是 http:// 或 https:// 开头
        if !self.evorule.base_url.starts_with("http://")
            && !self.evorule.base_url.starts_with("https://")
        {
            return Err(ConfigError::InvalidValue {
                field: "evorule.base_url".to_string(),
                reason: format!(
                    "must start with http:// or https://, got '{}'",
                    self.evorule.base_url
                ),
            });
        }

        // logging.level 必须是已知值
        match self.logging.level.as_str() {
            "trace" | "debug" | "info" | "warn" | "error" => {}
            other => {
                return Err(ConfigError::InvalidValue {
                    field: "logging.level".to_string(),
                    reason: format!(
                        "unknown level '{}', expected one of: trace, debug, info, warn, error",
                        other
                    ),
                });
            }
        }

        // logging.format 必须是已知值
        match self.logging.format.as_str() {
            "json" | "pretty" => {}
            other => {
                return Err(ConfigError::InvalidValue {
                    field: "logging.format".to_string(),
                    reason: format!("unknown format '{}', expected one of: json, pretty", other),
                });
            }
        }

        Ok(())
    }
}

/// 解析 `${ENV:VAR_NAME}` 占位符
///
/// 如果字符串不是占位符,原样返回。
/// 如果是占位符,读取环境变量;不存在则报错。
fn resolve_env_placeholder(s: &str) -> Result<String, ConfigError> {
    if let Some(var_name) = s.strip_prefix("${ENV:").and_then(|s| s.strip_suffix('}')) {
        std::env::var(var_name).map_err(|_| ConfigError::EnvVarNotFound(var_name.to_string()))
    } else {
        Ok(s.to_string())
    }
}

/// 宽松解析 `${ENV:VAR_NAME}` 占位符
///
/// 与 [`resolve_env_placeholder`] 的区别:
/// 环境变量未设置时返回空字符串,不报错。
fn resolve_env_placeholder_lenient(s: &str) -> String {
    if let Some(var_name) = s.strip_prefix("${ENV:").and_then(|s| s.strip_suffix('}')) {
        std::env::var(var_name).unwrap_or_default()
    } else {
        s.to_string()
    }
}

/// 字段级合并辅助:toml 表中显式出现的字段覆盖目标值,未出现则保持原值
///
/// 类型不符按配置解析错误处理(与整段反序列化的错误语义一致)。
fn merge_field<T>(table: &toml::value::Table, key: &str, target: &mut T) -> Result<(), ConfigError>
where
    T: serde::de::DeserializeOwned,
{
    match table.get(key) {
        Some(v) => {
            *target = v.clone().try_into().map_err(ConfigError::Parse)?;
            Ok(())
        }
        None => Ok(()),
    }
}

/// 用户级配置文件路径
///
/// - Unix: `$XDG_CONFIG_HOME/evo-agent/config.toml` 或 `$HOME/.config/evo-agent/config.toml`
/// - Windows: `%APPDATA%\evo-agent\config.toml`
fn user_config_path() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            return Some(PathBuf::from(xdg).join("evo-agent/config.toml"));
        }
        if let Ok(home) = std::env::var("HOME") {
            return Some(PathBuf::from(home).join(".config/evo-agent/config.toml"));
        }
        None
    }
    #[cfg(windows)]
    {
        if let Ok(roaming) = std::env::var("APPDATA") {
            return Some(PathBuf::from(roaming).join("evo-agent\\config.toml"));
        }
        if let Ok(home) = std::env::var("USERPROFILE") {
            return Some(PathBuf::from(home).join("evo-agent\\config.toml"));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // 防止 env 变量测试并发干扰
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 配置解析涉及的全部环境变量名(测试隔离用)
    const LLM_ENV_NAMES: [&str; 17] = [
        "MINIMAX_API_KEY",
        "MINIMAX_MODEL",
        "MINIMAX_API_BASE",
        "DEEPSEEK_API_KEY",
        "DEEPSEEK_MODEL",
        "DEEPSEEK_API_BASE",
        "OPENAI_API_KEY",
        "OPENAI_MODEL",
        "OPENAI_API_BASE",
        "EVO_AGENT_LLM__PROVIDER",
        "EVO_AGENT_LLM__API_KEY",
        "EVO_AGENT_LLM__MODEL",
        "EVO_AGENT_LLM__API_BASE",
        "EVO_AGENT_LLM__TIMEOUT_SECS",
        "EVO_AGENT_LLM__MAX_RETRIES",
        "EVO_AGENT_EVORULE__SERVICE_TOOLS",
        "EVO_AGENT_MCP__SERVERS",
    ];

    /// 删除全部 LLM 相关环境变量(防本机环境泄漏进测试),返回原值快照供恢复
    fn clear_llm_env() -> Vec<(&'static str, Option<String>)> {
        let snap: Vec<_> = LLM_ENV_NAMES
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect();
        for (k, _) in &snap {
            std::env::remove_var(k);
        }
        snap
    }

    /// 隔离用户级配置目录(防真实 user config 混入),返回快照供恢复
    fn isolate_user_config_dirs(baseline: &Path) -> Vec<(&'static str, Option<String>)> {
        let names = ["HOME", "APPDATA", "USERPROFILE", "XDG_CONFIG_HOME"];
        let snap: Vec<_> = names.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for k in names {
            std::env::set_var(k, baseline.join("no-such-home"));
        }
        snap
    }

    /// 按快照恢复环境变量(与 clear_llm_env / isolate_user_config_dirs 配对)
    fn restore_env(snap: Vec<(&'static str, Option<String>)>) {
        for (k, v) in snap {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn test_default_config() {
        let cfg = Config::default();
        assert_eq!(cfg.llm.provider, "minimax");
        assert_eq!(cfg.evorule.base_url, "http://localhost:18080");
        assert_eq!(cfg.agents.dir, PathBuf::from("./agents"));
        assert_eq!(cfg.agents.default, "general");
    }

    #[test]
    fn test_resolve_env_placeholder_passthrough() {
        let result = resolve_env_placeholder("plain-value").unwrap();
        assert_eq!(result, "plain-value");
    }

    #[test]
    fn test_resolve_env_placeholder_with_var() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::set_var("TEST_EVO_VAR_42", "secret123");
        let result = resolve_env_placeholder("${ENV:TEST_EVO_VAR_42}").unwrap();
        assert_eq!(result, "secret123");
        std::env::remove_var("TEST_EVO_VAR_42");
    }

    #[test]
    fn test_resolve_env_placeholder_missing() {
        let result = resolve_env_placeholder("${ENV:DEFINITELY_NOT_SET_X9K2}");
        assert!(matches!(result, Err(ConfigError::EnvVarNotFound(_))));
    }

    #[test]
    fn test_validate_provider_unknown_warns_but_loads() {
        // provider 为展示预留字段:未知值仅警告不拒绝装载(旧配置兼容)
        let mut cfg = Config::default();
        cfg.llm.provider = "some-future-provider".to_string();
        cfg.llm.api_key = "direct".to_string();
        assert!(cfg.validate().is_ok());
        assert!(cfg.validate_lenient().is_ok());
    }

    #[test]
    fn test_validate_empty_api_key() {
        let mut cfg = Config::default();
        cfg.llm.api_key = "".to_string();
        let result = cfg.validate();
        assert!(matches!(
            result,
            Err(ConfigError::InvalidValue { ref field, .. }) if field == "llm.api_key"
        ));
    }

    #[test]
    fn test_validate_evorule_url_must_be_http() {
        let mut cfg = Config::default();
        cfg.llm.api_key = "direct".to_string();
        cfg.evorule.base_url = "ftp://nope".to_string();
        let result = cfg.validate();
        assert!(matches!(
            result,
            Err(ConfigError::InvalidValue { ref field, .. }) if field == "evorule.base_url"
        ));
    }

    #[test]
    fn test_validate_logging_level() {
        let mut cfg = Config::default();
        cfg.llm.api_key = "direct".to_string();
        cfg.logging.level = "verbose".to_string();
        let result = cfg.validate();
        assert!(matches!(
            result,
            Err(ConfigError::InvalidValue { ref field, .. }) if field == "logging.level"
        ));
    }

    #[test]
    fn test_merge_from_file_partial() {
        let _lock = ENV_LOCK.lock().unwrap();
        // HOME 指向临时目录,避免污染真实 user config
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("evo-agent.toml");
        std::fs::write(
            &toml_path,
            r#"
[llm]
provider = "openai"
model = "gpt-4o"
"#,
        )
        .unwrap();

        let mut cfg = Config::default();
        cfg.merge_from_file(&toml_path).unwrap();
        // 被覆盖
        assert_eq!(cfg.llm.provider, "openai");
        assert_eq!(cfg.llm.model, "gpt-4o");
        // 未被覆盖(保留 default)
        assert_eq!(cfg.evorule.base_url, "http://localhost:18080");
    }

    #[test]
    fn test_env_override_takes_precedence() {
        let _lock = ENV_LOCK.lock().unwrap();
        // 先用普通值
        std::env::set_var("EVO_AGENT_LLM__PROVIDER", "deepseek");
        std::env::set_var("EVO_AGENT_LLM__MODEL", "deepseek-chat");
        std::env::set_var("EVO_AGENT_EVORULE__BASE_URL", "https://evorule.example.com");
        // 关掉 api_key 占位符的解析路径,直接给字面值
        std::env::set_var("EVO_AGENT_LLM__API_KEY", "literal-key");

        let _tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert_eq!(cfg.llm.provider, "deepseek");
        assert_eq!(cfg.llm.model, "deepseek-chat");
        assert_eq!(cfg.evorule.base_url, "https://evorule.example.com");
        assert_eq!(cfg.llm.api_key, "literal-key");

        // 清理
        std::env::remove_var("EVO_AGENT_LLM__PROVIDER");
        std::env::remove_var("EVO_AGENT_LLM__MODEL");
        std::env::remove_var("EVO_AGENT_EVORULE__BASE_URL");
        std::env::remove_var("EVO_AGENT_LLM__API_KEY");
    }

    #[test]
    fn test_load_full_flow_with_project_config() {
        let _lock = ENV_LOCK.lock().unwrap();
        // 隔离 LLM env:toml 字面量 api_key 不应被本机裸 provider env 覆盖
        let env_snap = clear_llm_env();

        // 准备:临时项目目录,放 evo-agent.toml
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path();
        std::fs::write(
            project_dir.join("evo-agent.toml"),
            r#"
[llm]
provider = "openai"
api_key = "literal-key-123"
model = "gpt-4o"

[evorule]
base_url = "https://evorule.example.com:8443"

[agents]
dir = "./my-agents"
default = "coder"
"#,
        )
        .unwrap();

        // 隔离 HOME 避免加载用户配置
        let saved_home = std::env::var("HOME").ok();
        let saved_appdata = std::env::var("APPDATA").ok();
        let saved_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("HOME", tmp.path().join("nonexistent-home"));

        // 关键:把 API key 解析成字面值(不是占位符)
        let cfg = Config::load(project_dir).expect("load should succeed");

        // 还原
        if let Some(h) = saved_home {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }
        if let Some(a) = saved_appdata {
            std::env::set_var("APPDATA", a);
        } else {
            std::env::remove_var("APPDATA");
        }
        if let Some(x) = saved_xdg {
            std::env::set_var("XDG_CONFIG_HOME", x);
        } else {
            std::env::remove_var("XDG_CONFIG_HOME");
        }

        assert_eq!(cfg.llm.provider, "openai");
        assert_eq!(cfg.llm.api_key, "literal-key-123");
        assert_eq!(cfg.llm.model, "gpt-4o");
        assert_eq!(cfg.evorule.base_url, "https://evorule.example.com:8443");
        assert_eq!(cfg.agents.dir, PathBuf::from("./my-agents"));
        assert_eq!(cfg.agents.default, "coder");
        restore_env(env_snap);
    }

    #[test]
    fn test_load_resolves_env_placeholder() {
        let _lock = ENV_LOCK.lock().unwrap();
        // 隔离 LLM env:本机裸 provider env 不应干扰占位符展开断言
        let env_snap = clear_llm_env();

        // 设置 env
        std::env::set_var("TEST_EVO_API_KEY_99", "real-secret-from-env");

        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("evo-agent.toml"),
            r#"
[llm]
api_key = "${ENV:TEST_EVO_API_KEY_99}"
"#,
        )
        .unwrap();

        // 隔离 HOME
        let saved_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", tmp.path().join("nonexistent-home"));

        let cfg = Config::load(tmp.path()).expect("load should succeed");

        if let Some(h) = saved_home {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }
        std::env::remove_var("TEST_EVO_API_KEY_99");

        assert_eq!(cfg.llm.api_key, "real-secret-from-env");
        restore_env(env_snap);
    }

    #[test]
    fn test_mcp_config_default_empty() {
        let cfg = Config::default();
        assert!(cfg.mcp.servers.is_empty());
    }

    #[test]
    fn test_mcp_config_merge_from_file() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("evo-agent.toml");
        std::fs::write(
            &toml_path,
            r#"
[llm]
api_key = "literal"

[[mcp.servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[[mcp.servers]]
name = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "ghp_secret" }
"#,
        )
        .unwrap();

        let mut cfg = Config::default();
        cfg.merge_from_file(&toml_path).unwrap();

        assert_eq!(cfg.mcp.servers.len(), 2);
        assert_eq!(cfg.mcp.servers[0].name, "filesystem");
        assert_eq!(cfg.mcp.servers[0].command, "npx");
        assert_eq!(cfg.mcp.servers[0].args.len(), 3);
        assert!(cfg.mcp.servers[0].env.is_empty());

        assert_eq!(cfg.mcp.servers[1].name, "github");
        assert_eq!(
            cfg.mcp.servers[1].env.get("GITHUB_TOKEN"),
            Some(&"ghp_secret".to_string())
        );
    }

    #[test]
    fn test_mcp_config_optional_fields_default() {
        let _lock = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("evo-agent.toml");
        // 只给 name + command,args/env 应默认为空
        std::fs::write(
            &toml_path,
            r#"
[llm]
api_key = "k"

[[mcp.servers]]
name = "minimal"
command = "echo"
"#,
        )
        .unwrap();

        let mut cfg = Config::default();
        cfg.merge_from_file(&toml_path).unwrap();

        assert_eq!(cfg.mcp.servers.len(), 1);
        assert_eq!(cfg.mcp.servers[0].name, "minimal");
        assert_eq!(cfg.mcp.servers[0].command, "echo");
        assert!(cfg.mcp.servers[0].args.is_empty());
        assert!(cfg.mcp.servers[0].env.is_empty());
    }

    // =========================================================================
    // LLM 配置统一解析(env 优先、toml 兜底)
    // =========================================================================

    #[test]
    fn test_llm_resolve_env_takes_precedence_over_toml() {
        // env 设置 → env 值生效(优先于 toml 基线)
        let _lock = ENV_LOCK.lock().unwrap();
        let env_snap = clear_llm_env();
        std::env::set_var("EVO_AGENT_LLM__API_KEY", "env-key");
        std::env::set_var("EVO_AGENT_LLM__MODEL", "env-model");

        let base = LlmConfig {
            api_key: "toml-key".to_string(),
            model: "toml-model".to_string(),
            ..Default::default()
        };

        let resolved = LlmConfig::resolve(base);
        assert_eq!(resolved.api_key, "env-key");
        assert_eq!(resolved.model, "env-model");
        restore_env(env_snap);
    }

    #[test]
    fn test_llm_resolve_falls_back_to_toml() {
        // env 缺 → toml 基线值生效
        let _lock = ENV_LOCK.lock().unwrap();
        let env_snap = clear_llm_env();

        let base = LlmConfig {
            api_key: "toml-key".to_string(),
            model: "toml-model".to_string(),
            ..Default::default()
        };

        let resolved = LlmConfig::resolve(base);
        assert_eq!(resolved.api_key, "toml-key");
        assert_eq!(resolved.model, "toml-model");
        restore_env(env_snap);
    }

    #[test]
    fn test_llm_resolve_defaults_when_env_and_toml_missing() {
        // 两者都缺 → 代码默认值
        let _lock = ENV_LOCK.lock().unwrap();
        let env_snap = clear_llm_env();

        let resolved = LlmConfig::resolve(LlmConfig::default());
        assert_eq!(resolved.api_key, LlmConfig::default_api_key());
        assert_eq!(resolved.model, LlmConfig::default_model());
        assert_eq!(resolved.api_base, LlmConfig::default_api_base());
        assert_eq!(resolved.timeout_secs, default_llm_timeout());
        assert_eq!(resolved.max_retries, default_max_retries());
        restore_env(env_snap);
    }

    #[test]
    fn test_llm_resolve_bare_provider_env_switches_provider_defaults() {
        // 裸 provider env 兜底:DEEPSEEK_API_KEY 命中且基线为出厂默认时,
        // model/api_base 切到 deepseek 默认组(与裸 env 时代 serve 行为对齐)
        let _lock = ENV_LOCK.lock().unwrap();
        let env_snap = clear_llm_env();
        std::env::set_var("DEEPSEEK_API_KEY", "sk-bare");

        let resolved = LlmConfig::resolve(LlmConfig::default());
        assert_eq!(resolved.api_key, "sk-bare");
        assert_eq!(resolved.model, "deepseek-chat");
        assert_eq!(
            resolved.api_base,
            "https://api.deepseek.com/v1/chat/completions"
        );
        restore_env(env_snap);
    }

    #[test]
    fn test_llm_resolve_bare_env_respects_toml_customization() {
        // 裸 key 只兜底 api_key;toml 已定制的 model 不被 provider 默认组冲掉
        let _lock = ENV_LOCK.lock().unwrap();
        let env_snap = clear_llm_env();
        std::env::set_var("MINIMAX_API_KEY", "sk-bare");

        let base = LlmConfig {
            model: "custom-model".to_string(),
            ..Default::default()
        };
        let resolved = LlmConfig::resolve(base);
        assert_eq!(resolved.api_key, "sk-bare");
        assert_eq!(resolved.model, "custom-model");
        restore_env(env_snap);
    }

    #[test]
    fn test_llm_resolve_explicit_key_blocks_bare_env() {
        // EVO_AGENT_LLM__API_KEY 接管 api_key 后,裸 provider env 整体跳过
        let _lock = ENV_LOCK.lock().unwrap();
        let env_snap = clear_llm_env();
        std::env::set_var("MINIMAX_API_KEY", "sk-bare");
        std::env::set_var("EVO_AGENT_LLM__API_KEY", "env-key");

        let resolved = LlmConfig::resolve(LlmConfig::default());
        assert_eq!(resolved.api_key, "env-key");
        assert_eq!(resolved.model, LlmConfig::default_model());
        assert_eq!(resolved.api_base, LlmConfig::default_api_base());
        restore_env(env_snap);
    }

    #[test]
    fn test_llm_resolve_records_key_source() {
        // 裸 provider env 命中时来源记录为该环境变量名(脱敏状态端点用)
        let _lock = ENV_LOCK.lock().unwrap();
        let env_snap = clear_llm_env();
        std::env::set_var("DEEPSEEK_API_KEY", "sk-bare");

        let (_, source) = LlmConfig::resolve_with_source(LlmConfig::default());
        assert_eq!(source, Some("DEEPSEEK_API_KEY".to_string()));

        std::env::set_var("EVO_AGENT_LLM__API_KEY", "env-key");
        let (_, source) = LlmConfig::resolve_with_source(LlmConfig::default());
        assert_eq!(source, Some("EVO_AGENT_LLM__API_KEY".to_string()));
        restore_env(env_snap);
    }

    #[test]
    fn test_dual_path_llm_config_consistency() {
        // 双路径一致性:同一组 env(+无 toml 定制)输入下,
        // CLI 路径(Config::load 产物,run 命令直接消费)与 serve 路径
        // (cmd_serve 注入 state 的解析产物)得出相同的 LLM 字段——
        // 两条入口共用同一解析链后,一致性由同源保证,本测试锁住该事实
        let _lock = ENV_LOCK.lock().unwrap();
        let env_snap = clear_llm_env();
        std::env::set_var("MINIMAX_API_KEY", "sk-both-paths");
        std::env::set_var("MINIMAX_MODEL", "MiniMax-Path-Test");

        let tmp = tempfile::tempdir().unwrap();
        let dir_snap = isolate_user_config_dirs(tmp.path());

        let cfg = Config::load(tmp.path()).expect("load should succeed");
        // serve 侧独立按同一解析入口重算(无 toml 基线),应与 CLI 路径一致
        let serve_side = LlmConfig::resolve(LlmConfig::default());

        restore_env(dir_snap);
        assert_eq!(cfg.llm.api_key, serve_side.api_key);
        assert_eq!(cfg.llm.model, serve_side.model);
        assert_eq!(cfg.llm.api_base, serve_side.api_base);
        assert_eq!(cfg.llm.api_key, "sk-both-paths");
        assert_eq!(cfg.llm.model, "MiniMax-Path-Test");
        restore_env(env_snap);
    }

    // =========================================================================
    // 字段级深合并
    // =========================================================================

    #[test]
    fn test_merge_from_file_field_level_across_layers() {
        // 跨层叠加:user 层声明 api_key/model,project 层只声明 provider——
        // 字段级合并下 project 层未声明的字段保留 user 层值(整段覆盖时代
        // 会被 project 层缺失字段的默认值冲掉)
        let _lock = ENV_LOCK.lock().unwrap();
        let env_snap = clear_llm_env();
        let tmp = tempfile::tempdir().unwrap();
        let user_path = tmp.path().join("user.toml");
        let project_path = tmp.path().join("project.toml");
        std::fs::write(
            &user_path,
            r#"
[llm]
api_key = "user-key"
model = "user-model"
"#,
        )
        .unwrap();
        std::fs::write(
            &project_path,
            r#"
[llm]
provider = "openai"
"#,
        )
        .unwrap();

        let mut cfg = Config::default();
        cfg.merge_from_file(&user_path).unwrap();
        cfg.merge_from_file(&project_path).unwrap();

        assert_eq!(cfg.llm.api_key, "user-key");
        assert_eq!(cfg.llm.model, "user-model");
        assert_eq!(cfg.llm.provider, "openai");
        // project 层未声明的 llm 字段保持默认
        assert_eq!(cfg.llm.timeout_secs, default_llm_timeout());
        // 未涉及的 section 保持默认
        assert_eq!(cfg.evorule.base_url, "http://localhost:18080");
        restore_env(env_snap);
    }

    #[test]
    fn test_merge_from_file_single_section_partial_fields() {
        // 单文件内部分字段:显式字段覆盖,未声明字段保持父值(同 section)
        let _lock = ENV_LOCK.lock().unwrap();
        let env_snap = clear_llm_env();
        let tmp = tempfile::tempdir().unwrap();
        let toml_path = tmp.path().join("evo-agent.toml");
        std::fs::write(
            &toml_path,
            r#"
[evorule]
base_url = "https://override.example.com"
"#,
        )
        .unwrap();

        let mut cfg = Config::default();
        cfg.evorule.api_key = "parent-key".to_string();
        cfg.merge_from_file(&toml_path).unwrap();

        assert_eq!(cfg.evorule.base_url, "https://override.example.com");
        assert_eq!(cfg.evorule.api_key, "parent-key");
        assert_eq!(cfg.evorule.timeout_secs, default_evorule_timeout());
        restore_env(env_snap);
    }

    // =========================================================================
    // 旧配置装载兼容(死字段移除 + provider 展示预留)
    // =========================================================================

    #[test]
    fn test_legacy_toml_with_dead_field_and_unknown_provider_loads() {
        // 旧配置兼容:llm.context_window_tokens 已移除(警告后忽略,不破装载);
        // provider 未知值仅警告不拒绝
        let _lock = ENV_LOCK.lock().unwrap();
        let env_snap = clear_llm_env();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("evo-agent.toml"),
            r#"
[llm]
provider = "legacy-provider"
api_key = "literal-key"
context_window_tokens = 32768
"#,
        )
        .unwrap();
        let dir_snap = isolate_user_config_dirs(tmp.path());

        let cfg = Config::load(tmp.path()).expect("legacy config should still load");

        restore_env(dir_snap);
        // 死字段被忽略,其余字段正常装载
        assert_eq!(cfg.llm.provider, "legacy-provider");
        assert_eq!(cfg.llm.api_key, "literal-key");
        restore_env(env_snap);
    }

    // =========================================================================
    // 环境变量白名单
    // =========================================================================

    #[test]
    fn test_env_whitelist_service_tools_and_mcp_servers() {
        // 白名单内 key 可读:服务工具白名单(逗号分隔)与 MCP server(JSON 数组)
        let _lock = ENV_LOCK.lock().unwrap();
        let env_snap = clear_llm_env();
        std::env::set_var("EVO_AGENT_EVORULE__SERVICE_TOOLS", "svc_a, svc_b");
        std::env::set_var(
            "EVO_AGENT_MCP__SERVERS",
            r#"[{"name":"fs","command":"npx","args":["-y","server-fs"]}]"#,
        );

        let mut cfg = Config::default();
        cfg.apply_env_overrides();

        assert_eq!(
            cfg.evorule.service_tools,
            vec!["svc_a".to_string(), "svc_b".to_string()]
        );
        assert_eq!(cfg.mcp.servers.len(), 1);
        assert_eq!(cfg.mcp.servers[0].name, "fs");
        assert_eq!(cfg.mcp.servers[0].command, "npx");
        assert_eq!(
            cfg.mcp.servers[0].args,
            vec!["-y".to_string(), "server-fs".to_string()]
        );
        restore_env(env_snap);
    }

    #[test]
    fn test_env_whitelist_mcp_malformed_json_ignored() {
        // EVO_AGENT_MCP__SERVERS 非法 JSON:告警后忽略,装载不破
        let _lock = ENV_LOCK.lock().unwrap();
        let env_snap = clear_llm_env();
        std::env::set_var("EVO_AGENT_MCP__SERVERS", "not-a-json-array");

        let mut cfg = Config::default();
        cfg.apply_env_overrides();

        assert!(cfg.mcp.servers.is_empty());
        restore_env(env_snap);
    }

    #[test]
    fn test_env_whitelist_unknown_keys_filtered() {
        // 白名单外的 EVO_AGENT_* 变量被过滤:不进配置、不报错
        let _lock = ENV_LOCK.lock().unwrap();
        let env_snap = clear_llm_env();
        std::env::set_var("EVO_AGENT_MCP__NOT_A_FIELD", "zzz");
        std::env::set_var("EVO_AGENT_UNKNOWN_SECTION__X", "1");

        let mut cfg = Config::default();
        cfg.apply_env_overrides();

        assert!(cfg.mcp.servers.is_empty());
        assert_eq!(cfg.evorule.base_url, EvoruleConfig::default_base_url());
        assert_eq!(cfg.llm.provider, LlmConfig::default_provider());
        std::env::remove_var("EVO_AGENT_MCP__NOT_A_FIELD");
        std::env::remove_var("EVO_AGENT_UNKNOWN_SECTION__X");
        restore_env(env_snap);
    }
}
