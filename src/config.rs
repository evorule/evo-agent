// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Configuration loading for evo-agent.
//!
//! 配置层级(优先级低→高,后者覆盖前者):
//! 1. **默认值** —— 代码中 `Config::default()`
//! 2. **用户配置** —— `~/.config/evo-agent/config.toml`(Linux/macOS)
//!    或 `%APPDATA%\evo-agent\config.toml`(Windows)
//! 3. **项目配置** —— `./evo-agent.toml`(由 `project_dir` 指定)
//! 4. **环境变量** —— `EVO_AGENT_*` 前缀,`__` 分隔 section/field
//!    (如 `EVO_AGENT_LLM__PROVIDER=openai`)
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
    /// provider 名称: `minimax` | `deepseek` | `openai`
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
    /// G2:上下文窗口 token 数(默认 8192,按模型调整)
    ///
    /// `AgentRunner` 用此值构造 `ContextWindowManager`,
    /// 在发给 LLM 前裁剪历史消息,保留 system + 最近若干轮。
    #[serde(default = "default_context_window_tokens")]
    pub context_window_tokens: usize,
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

/// G2:默认上下文窗口 token 数
fn default_context_window_tokens() -> usize {
    8192
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
            context_window_tokens: default_context_window_tokens(),
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
    /// 字段值非法(如 provider 不在白名单、URL 协议错等)
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

    /// 从单个配置文件合并(只覆盖文件中存在的字段)
    fn merge_from_file(&mut self, path: &Path) -> Result<(), ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let value: toml::Value = content.parse().map_err(ConfigError::Parse)?;

        if let Some(table) = value.get("llm") {
            self.llm = table.clone().try_into().map_err(ConfigError::Parse)?;
        }
        if let Some(table) = value.get("evorule") {
            self.evorule = table.clone().try_into().map_err(ConfigError::Parse)?;
        }
        if let Some(table) = value.get("logging") {
            self.logging = table.clone().try_into().map_err(ConfigError::Parse)?;
        }
        if let Some(table) = value.get("agents") {
            self.agents = table.clone().try_into().map_err(ConfigError::Parse)?;
        }
        if let Some(table) = value.get("auth") {
            self.auth = table.clone().try_into().map_err(ConfigError::Parse)?;
        }
        if let Some(table) = value.get("mcp") {
            self.mcp = table.clone().try_into().map_err(ConfigError::Parse)?;
        }

        Ok(())
    }

    /// 应用环境变量覆盖
    ///
    /// 约定: `EVO_AGENT_<SECTION>__<FIELD>=value`
    /// 例如: `EVO_AGENT_LLM__PROVIDER=openai` → `config.llm.provider = "openai"`
    fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("EVO_AGENT_LLM__PROVIDER") {
            self.llm.provider = v;
        }
        if let Ok(v) = std::env::var("EVO_AGENT_LLM__API_KEY") {
            self.llm.api_key = v;
        }
        if let Ok(v) = std::env::var("EVO_AGENT_LLM__MODEL") {
            self.llm.model = v;
        }
        if let Ok(v) = std::env::var("EVO_AGENT_LLM__API_BASE") {
            self.llm.api_base = v;
        }
        if let Ok(v) = std::env::var("EVO_AGENT_LLM__TIMEOUT_SECS") {
            if let Ok(n) = v.parse() {
                self.llm.timeout_secs = n;
            }
        }
        if let Ok(v) = std::env::var("EVO_AGENT_LLM__MAX_RETRIES") {
            if let Ok(n) = v.parse() {
                self.llm.max_retries = n;
            }
        }
        if let Ok(v) = std::env::var("EVO_AGENT_LLM__CONTEXT_WINDOW_TOKENS") {
            if let Ok(n) = v.parse() {
                self.llm.context_window_tokens = n;
            }
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
        self.llm.api_key = resolve_env_placeholder(&self.llm.api_key)?;
        if !self.evorule.api_key.is_empty() {
            self.evorule.api_key = resolve_env_placeholder(&self.evorule.api_key)?;
        }
        Ok(())
    }

    /// 宽松解析 `${ENV:VAR_NAME}` 占位符
    ///
    /// 与 [`resolve_env_placeholders`](Self::resolve_env_placeholders) 的区别:
    /// 环境变量未设置时替换为空字符串,不报错。
    fn resolve_env_placeholders_lenient(&mut self) {
        self.llm.api_key = resolve_env_placeholder_lenient(&self.llm.api_key);
        if !self.evorule.api_key.is_empty() {
            self.evorule.api_key = resolve_env_placeholder_lenient(&self.evorule.api_key);
        }
    }

    /// 验证配置值的合法性
    fn validate(&self) -> Result<(), ConfigError> {
        // LLM provider 必须是已知值
        match self.llm.provider.as_str() {
            "minimax" | "deepseek" | "openai" => {}
            other => {
                return Err(ConfigError::InvalidValue {
                    field: "llm.provider".to_string(),
                    reason: format!(
                        "unknown provider '{}', expected one of: minimax, deepseek, openai",
                        other
                    ),
                });
            }
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
    /// - 仍然验证 provider/base_url/logging 等非 LLM 字段
    fn validate_lenient(&self) -> Result<(), ConfigError> {
        // LLM provider 必须是已知值
        match self.llm.provider.as_str() {
            "minimax" | "deepseek" | "openai" => {}
            other => {
                return Err(ConfigError::InvalidValue {
                    field: "llm.provider".to_string(),
                    reason: format!(
                        "unknown provider '{}', expected one of: minimax, deepseek, openai",
                        other
                    ),
                });
            }
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
    fn test_validate_provider_unknown() {
        let mut cfg = Config::default();
        cfg.llm.provider = "bogus".to_string();
        cfg.llm.api_key = "direct".to_string();
        let result = cfg.validate();
        assert!(matches!(
            result,
            Err(ConfigError::InvalidValue { ref field, .. }) if field == "llm.provider"
        ));
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
    }

    #[test]
    fn test_load_resolves_env_placeholder() {
        let _lock = ENV_LOCK.lock().unwrap();

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
}
