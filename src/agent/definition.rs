// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Agent 瀹氫箟 鈥斺€?浠?agent.json 鍔犺浇 Agent 閰嶇疆

use std::path::{Path, PathBuf};

use crate::agent::runner::AgentConfig;

/// Agent 瀹氫箟鍔犺浇閿欒
#[derive(Debug)]
pub enum AgentDefinitionError {
    /// IO 閿欒
    Io(std::io::Error),
    /// JSON 瑙ｆ瀽閿欒
    Json(serde_json::Error),
    /// Agent 绫诲瀷鏈壘鍒
    NotFound(String),
    /// 定义非法(门卫:取值越界/标识符不合法)
    InvalidDefinition(String),
}

impl std::fmt::Display for AgentDefinitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentDefinitionError::Io(e) => write!(f, "IO error: {}", e),
            AgentDefinitionError::Json(e) => write!(f, "JSON parse error: {}", e),
            AgentDefinitionError::NotFound(t) => write!(f, "Agent type not found: {}", t),
            AgentDefinitionError::InvalidDefinition(msg) => {
                write!(f, "Invalid agent definition: {}", msg)
            }
        }
    }
}

impl std::error::Error for AgentDefinitionError {}

impl From<std::io::Error> for AgentDefinitionError {
    fn from(e: std::io::Error) -> Self {
        AgentDefinitionError::Io(e)
    }
}

impl From<serde_json::Error> for AgentDefinitionError {
    fn from(e: serde_json::Error) -> Self {
        AgentDefinitionError::Json(e)
    }
}

/// 消息持久化配置（031 设计文档 P0，用户决策 2：可选开关）
///
/// 控制 `AgentRunner` 何时把对话消息写入 evorule payload。
/// 在 agent.json 中按如下配置：
/// ```json
/// "memory": {
///   "type": "persistent",
///   "namespace": "researcher",
///   "message_persist": { "mode": "every_n", "n": 5 }
/// }
/// ```
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MessagePersistConfig {
    /// 持久化模式：
    /// - `"every_message"`: 每条消息立即写入（默认，最安全）
    /// - `"every_n"`: 每 N 条消息批量写入
    /// - `"per_react_round"`: 每轮 ReAct 结束时写入
    /// - `"disabled"`: 不持久化消息（向后兼容旧行为）
    pub mode: String,
    /// 每 N 条消息刷写一次（仅 `mode == "every_n"` 时生效，必须 > 0）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<usize>,
}

impl Default for MessagePersistConfig {
    fn default() -> Self {
        Self {
            mode: "every_message".to_string(),
            n: None,
        }
    }
}

impl MessagePersistConfig {
    /// 解析为 `MemoryManager` 内部使用的 `MessagePersistMode` 枚举
    ///
    /// # 错误
    /// - 未知 mode 字符串
    /// - `every_n` 模式下 `n` 为 0 或缺失
    pub fn to_mode(&self) -> Result<crate::agent::memory::MessagePersistMode, String> {
        match self.mode.as_str() {
            "every_message" => Ok(crate::agent::memory::MessagePersistMode::EveryMessage),
            "every_n" => {
                let n = self.n.unwrap_or(0);
                if n == 0 {
                    return Err("message_persist.n must be > 0 when mode == 'every_n'".to_string());
                }
                Ok(crate::agent::memory::MessagePersistMode::EveryN(n))
            }
            "per_react_round" => Ok(crate::agent::memory::MessagePersistMode::PerReactRound),
            "disabled" => Ok(crate::agent::memory::MessagePersistMode::Disabled),
            other => Err(format!("unknown message_persist mode: {}", other)),
        }
    }
}

/// 鍐呭瓨閰嶇疆
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryConfig {
    #[serde(rename = "type")]
    /// 鍐呭瓨绫诲瀷锛堝涓 "none", "persistent"锛
    pub memory_type: String,
    /// 鍐呭瓨鍛藉悕绌洪棿
    pub namespace: String,
    /// 消息持久化配置（用户决策 2：可选开关，默认 `every_message`）
    #[serde(default)]
    pub message_persist: MessagePersistConfig,
    /// 记忆过期时间（秒，可选，用户决策 5：TTL）
    ///
    /// 设置后记忆条目在 `ttl_secs` 秒后过期，由 `MemoryManager` 惰性清理。
    /// 未设置（`None`）时记忆永久保留。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
    /// 摘要模型名称（可选，用户决策 3：单独配置 `summary_model`）
    ///
    /// 设置后 P4 记忆压缩使用此模型生成 summary，否则 fallback 到主 `model`。
    /// P0+P1 阶段不使用此字段，仅占位。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_model: Option<String>,
    /// G14/Q18:事件提取模型名称（可选，单独配置 `extraction_model`）
    ///
    /// 设置后 032 EventExtractor 使用此模型从对话中提取结构化事件字段，
    /// 否则 fallback 到主 `model`。可用便宜模型(如 GPT-4o-mini)降低成本。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction_model: Option<String>,
    /// C3: 记忆区占窗口的比例（默认 0.25）
    #[serde(default = "default_memory_budget_ratio")]
    pub memory_budget_ratio: f32,
    /// C2: 最近 N 个会话摘要（默认 3）
    #[serde(default = "default_max_session_summaries")]
    pub max_session_summaries: usize,
    /// C2: top-K 事件（默认 5）
    #[serde(default = "default_max_injected_events")]
    pub max_injected_events: usize,
    /// C4: L1 摘要 rollup 阈值（默认 10）
    #[serde(default = "default_summary_rollup_threshold")]
    pub summary_rollup_threshold: usize,
    /// C1: 是否启用事件提取（默认 true）
    #[serde(default = "default_true")]
    pub enable_event_extraction: bool,
}

fn default_memory_budget_ratio() -> f32 {
    0.25
}
fn default_max_session_summaries() -> usize {
    3
}
fn default_max_injected_events() -> usize {
    5
}
fn default_summary_rollup_threshold() -> usize {
    10
}
fn default_true() -> bool {
    true
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            memory_type: "none".to_string(),
            namespace: String::new(),
            message_persist: MessagePersistConfig::default(),
            ttl_secs: None,
            summary_model: None,
            extraction_model: None,
            memory_budget_ratio: default_memory_budget_ratio(),
            max_session_summaries: default_max_session_summaries(),
            max_injected_events: default_max_injected_events(),
            summary_rollup_threshold: default_summary_rollup_threshold(),
            enable_event_extraction: default_true(),
        }
    }
}

/// Output format configuration
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OutputFormat {
    #[serde(rename = "type")]
    /// Format type (e.g. "json", "text")
    pub format_type: String,
    /// Output schema (optional)
    pub schema: Option<serde_json::Value>,
    /// G11:校验失败时的最大重试次数(可选,默认 2)
    ///
    /// LLM 输出不符合 schema 时,runner 注入校正消息让 LLM 重试。
    /// 超过 `max_retries` 次后降级为接受原输出(避免死循环)。
    /// 设为 `Some(0)` 表示不重试,首次失败即降级接受。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<usize>,
}

/// Agent definition
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentDefinition {
    /// Agent type identifier
    pub agent_type: String,
    /// Version number
    pub version: String,
    /// Description
    pub description: String,
    /// System prompt
    pub system_prompt: String,
    /// Model name to use
    pub model: String,
    /// Temperature parameter
    pub temperature: f32,
    /// Max execution steps
    pub max_steps: usize,
    /// Step timeout in seconds
    pub step_timeout_secs: u64,
    /// Available tool list
    pub tools: Vec<String>,
    #[serde(default)]
    /// Memory configuration
    pub memory: MemoryConfig,
    /// Output format configuration (optional)
    pub output_format: Option<OutputFormat>,
    /// G2:上下文窗口 token 数(可选,默认 8192)
    ///
    /// 设为 `None` 时使用 `AgentConfig` 默认值。设为 `Some(n)` 时,
    /// `ContextWindowManager` 会按 `n` 裁剪历史消息,保留 system + 最近若干轮。
    /// 预留 1/4 给响应,实际可用输入 = `n - n/4`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<usize>,
    /// G13:单轮内并行工具调用上限(可选,默认 1 = 串行)
    ///
    /// - `1`(默认):工具按顺序串行执行(向后兼容旧行为)
    /// - `>1`:同一 ReAct 步骤中多个 active 工具调用并行执行(用 `futures::future::join_all`)
    ///
    /// candidate 工具(需审批)始终串行执行,不受此参数影响。
    /// 设为 `0` 视为 `1`(防御性)。
    #[serde(
        default = "default_max_parallel_tools",
        skip_serializing_if = "is_max_parallel_tools_default"
    )]
    pub max_parallel_tools: usize,
}

/// G13:`max_parallel_tools` 的默认值(串行)
fn default_max_parallel_tools() -> usize {
    1
}

/// G13:序列化时若为默认值(1)则跳过
fn is_max_parallel_tools_default(v: &usize) -> bool {
    *v == 1
}

impl AgentDefinition {
    /// agent_type 标识符白名单:`[A-Za-z0-9_-]+`
    ///
    /// 门卫(P2-M7 前置补丁,2026-08-27):workflow JSON 等外部输入会以
    /// `agent_type` 拼接文件路径加载——含 `/` `\` `..` 的值构成路径穿越,
    /// 可读取盘上任意 .json。加载侧统一在此拒绝。
    pub fn validate_agent_type(agent_type: &str) -> Result<(), AgentDefinitionError> {
        if agent_type.is_empty()
            || !agent_type
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(AgentDefinitionError::InvalidDefinition(format!(
                "invalid agent_type '{}': must match [A-Za-z0-9_-]+ (path traversal guard)",
                agent_type
            )));
        }
        Ok(())
    }

    /// 定义级语义校验(反序列化后的第二道门卫)
    ///
    /// serde 只保证类型正确,不保证取值合理。此处拦截会在 LLM 调用时才爆出的
    /// 错配(如 temperature 越界、max_steps=0),加载期即失败并给出可读原因。
    pub fn validate(&self) -> Result<(), AgentDefinitionError> {
        Self::validate_agent_type(&self.agent_type)?;
        if !(0.0..=2.0).contains(&self.temperature) {
            return Err(AgentDefinitionError::InvalidDefinition(format!(
                "temperature {} out of range [0.0, 2.0]",
                self.temperature
            )));
        }
        if self.max_steps == 0 {
            return Err(AgentDefinitionError::InvalidDefinition(
                "max_steps must be >= 1".to_string(),
            ));
        }
        if self.step_timeout_secs == 0 {
            return Err(AgentDefinitionError::InvalidDefinition(
                "step_timeout_secs must be >= 1".to_string(),
            ));
        }
        Ok(())
    }

    /// Load Agent definition from directory
    pub fn load_from_dir(dir: &Path, agent_type: &str) -> Result<Self, AgentDefinitionError> {
        // 门卫 1:agent_type 标识符白名单(路径穿越防护)
        Self::validate_agent_type(agent_type)?;
        let path = dir.join(format!("{}.json", agent_type));
        if !path.exists() {
            return Err(AgentDefinitionError::NotFound(agent_type.to_string()));
        }
        let content = std::fs::read_to_string(&path)?;
        let value: serde_json::Value = serde_json::from_str(&content)
            .map_err(AgentDefinitionError::Json)?;
        // 门卫 2:宪法 jsonschema 全量校验(找不到 schema 时降级为仅门卫 3,tracing 留痕)
        crate::agent::constitution::validate_agent_def(&value).map_err(|errs| {
            AgentDefinitionError::InvalidDefinition(format!(
                "constitution schema violations: {}",
                errs.join("; ")
            ))
        })?;
        // 门卫 3:定义级语义校验(取值范围)
        let def: AgentDefinition = serde_json::from_value(value.clone())
            .map_err(AgentDefinitionError::Json)?;
        def.validate()?;
        Ok(def)
    }

    /// List all available Agent types in directory
    pub fn list_available(dir: &Path) -> Result<Vec<String>, AgentDefinitionError> {
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut types = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    types.push(stem.to_string());
                }
            }
        }
        types.sort();
        Ok(types)
    }

    /// Convert to AgentConfig
    pub fn to_agent_config(&self) -> AgentConfig {
        AgentConfig {
            agent_type: self.agent_type.clone(),
            system_prompt: self.system_prompt.clone(),
            model: self.model.clone(),
            temperature: self.temperature,
            max_steps: self.max_steps,
            step_timeout: std::time::Duration::from_secs(self.step_timeout_secs),
            tool_names: self.tools.clone(),
            llm_retry_count: AgentConfig::default().llm_retry_count,
            output_format: self.output_format.clone(),
            // G13:0 视为 1(防御性);None 时用 AgentConfig 默认值
            max_parallel_tools: if self.max_parallel_tools == 0 {
                1
            } else {
                self.max_parallel_tools
            },
        }
    }
}

/// Agent 瀹氫箟绠＄悊鍣
#[derive(Debug, Clone)]
pub struct AgentDefinitionManager {
    /// Agent 瀹氫箟鏂囦欢鎵€鍦ㄧ洰褰
    agents_dir: PathBuf,
}

impl AgentDefinitionManager {
    /// Create new manager
    pub fn new(agents_dir: PathBuf) -> Self {
        Self { agents_dir }
    }

    /// Create manager with default directory (rules/agents)
    pub fn with_default_dir() -> Self {
        let dir = PathBuf::from("rules/agents");
        Self::new(dir)
    }

    /// Get agents directory path
    pub fn agents_dir(&self) -> &Path {
        &self.agents_dir
    }

    /// Load Agent definition of specified type
    pub fn load(&self, agent_type: &str) -> Result<AgentDefinition, AgentDefinitionError> {
        AgentDefinition::load_from_dir(&self.agents_dir, agent_type)
    }

    /// List all available Agent types
    pub fn list_types(&self) -> Result<Vec<String>, AgentDefinitionError> {
        AgentDefinition::list_available(&self.agents_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create tempdir")
    }

    fn write_json(dir: &Path, name: &str, json: &str) {
        let path = dir.join(format!("{}.json", name));
        let mut f = std::fs::File::create(&path).expect("create file");
        f.write_all(json.as_bytes()).expect("write file");
    }

    #[test]
    fn test_agent_definition_deserialize() {
        let json = r#"{
            "agent_type": "researcher",
            "version": "1.0.0",
            "description": "Research agent",
            "system_prompt": "You are a research agent",
            "model": "gpt-4o-mini",
            "temperature": 0.3,
            "max_steps": 20,
            "step_timeout_secs": 60,
            "tools": ["search_web", "read_file"],
            "memory": { "type": "file", "namespace": "researcher" },
            "output_format": { "type": "json", "schema": {"summary": "string"} }
        }"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert_eq!(def.agent_type, "researcher");
        assert_eq!(def.version, "1.0.0");
        assert_eq!(def.model, "gpt-4o-mini");
        assert!((def.temperature - 0.3).abs() < 0.01);
        assert_eq!(def.max_steps, 20);
        assert_eq!(def.step_timeout_secs, 60);
        assert_eq!(def.tools, vec!["search_web", "read_file"]);
        assert_eq!(def.memory.memory_type, "file");
        assert_eq!(def.memory.namespace, "researcher");
        assert!(def.output_format.is_some());
        assert_eq!(def.output_format.as_ref().unwrap().format_type, "json");
    }

    #[test]
    fn test_agent_definition_default_memory() {
        let json = r#"{
            "agent_type": "simple",
            "version": "1.0.0",
            "description": "绠€鍗?Agent",
            "system_prompt": "浣犲ソ",
            "model": "gpt-4o-mini",
            "temperature": 0.7,
            "max_steps": 10,
            "step_timeout_secs": 30,
            "tools": [],
            "output_format": null
        }"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert_eq!(def.memory.memory_type, "none");
        assert!(def.output_format.is_none());
    }

    #[test]
    fn test_load_from_dir() {
        let dir = make_tmp_dir();
        let json = r#"{
            "agent_type": "test_agent",
            "version": "2.0.0",
            "description": "娴嬭瘯",
            "system_prompt": "test",
            "model": "gpt-4",
            "temperature": 0.5,
            "max_steps": 5,
            "step_timeout_secs": 10,
            "tools": ["echo"],
            "output_format": null
        }"#;
        write_json(dir.path(), "test_agent", json);

        let def = AgentDefinition::load_from_dir(dir.path(), "test_agent").expect("load");
        assert_eq!(def.agent_type, "test_agent");
        assert_eq!(def.version, "2.0.0");
    }

    #[test]
    fn test_load_from_dir_not_found() {
        let dir = make_tmp_dir();
        let result = AgentDefinition::load_from_dir(dir.path(), "nonexistent");
        assert!(matches!(result, Err(AgentDefinitionError::NotFound(_))));
    }

    // ===== 门卫负向用例(P2-M7 前置补丁,2026-08-27) =====

    #[test]
    fn test_load_rejects_path_traversal_agent_type() {
        let dir = make_tmp_dir();
        for bad in ["../general", "a/b", "a\\b", "..", ""] {
            let result = AgentDefinition::load_from_dir(dir.path(), bad);
            match result {
                Err(AgentDefinitionError::InvalidDefinition(msg)) => {
                    assert!(msg.contains("path traversal") || msg.contains("invalid agent_type"));
                }
                other => panic!("agent_type {:?} not rejected as InvalidDefinition: {:?}", bad, other),
            }
        }
    }

    #[test]
    fn test_validate_rejects_temperature_out_of_range() {
        let mk = |t: f32| {
            r#"{"agent_type":"x","version":"1","description":"","system_prompt":"","model":"m","temperature":"#
                .to_string()
                + &t.to_string()
                + r#","max_steps":1,"step_timeout_secs":1,"tools":[],"output_format":null}"#
        };
        for t in [-0.5_f32, 2.5_f32] {
            let def: AgentDefinition = serde_json::from_str(&mk(t)).expect("parse");
            let err = def.validate().unwrap_err();
            assert!(
                err.to_string().contains("temperature"),
                "temperature {} not rejected: {}",
                t,
                err
            );
        }
        // 边界值合法
        for t in [0.0_f32, 2.0_f32] {
            let def: AgentDefinition = serde_json::from_str(&mk(t)).expect("parse");
            assert!(def.validate().is_ok());
        }
    }

    #[test]
    fn test_validate_rejects_zero_max_steps_and_timeout() {
        let json = r#"{"agent_type":"x","version":"1","description":"","system_prompt":"","model":"m","temperature":0.5,"max_steps":0,"step_timeout_secs":1,"tools":[],"output_format":null}"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert!(def.validate().unwrap_err().to_string().contains("max_steps"));

        let json = r#"{"agent_type":"x","version":"1","description":"","system_prompt":"","model":"m","temperature":0.5,"max_steps":1,"step_timeout_secs":0,"tools":[],"output_format":null}"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert!(def
            .validate()
            .unwrap_err()
            .to_string()
            .contains("step_timeout_secs"));
    }

    #[test]
    fn test_validate_accepts_production_agents() {
        // 三个生产 agent 定义必须通过门卫(防门卫误伤真资产)
        let dir = PathBuf::from("agents");
        if !dir.exists() {
            return; // 非仓根运行时跳过
        }
        for name in ["general", "researcher", "rule-copilot"] {
            let def = AgentDefinition::load_from_dir(&dir, name)
                .unwrap_or_else(|e| panic!("production agent '{}' rejected: {}", name, e));
            assert!(def.validate().is_ok());
        }
    }

    #[test]
    fn test_list_available() {
        let dir = make_tmp_dir();
        write_json(
            dir.path(),
            "alpha",
            r#"{"agent_type":"alpha","version":"1","description":"","system_prompt":"","model":"","temperature":0.5,"max_steps":1,"step_timeout_secs":1,"tools":[],"output_format":null}"#,
        );
        write_json(
            dir.path(),
            "beta",
            r#"{"agent_type":"beta","version":"1","description":"","system_prompt":"","model":"","temperature":0.5,"max_steps":1,"step_timeout_secs":1,"tools":[],"output_format":null}"#,
        );
        std::fs::write(dir.path().join("readme.txt"), "hello").unwrap();

        let types = AgentDefinition::list_available(dir.path()).expect("list");
        assert_eq!(types.len(), 2);
        assert_eq!(types[0], "alpha");
        assert_eq!(types[1], "beta");
    }

    #[test]
    fn test_list_available_empty_dir() {
        let dir = make_tmp_dir();
        let types = AgentDefinition::list_available(dir.path()).expect("list");
        assert!(types.is_empty());
    }

    #[test]
    fn test_list_available_nonexistent_dir() {
        let types = AgentDefinition::list_available(Path::new("/nonexistent/path/xyz"))
            .expect("nonexistent dir returns empty");
        assert!(types.is_empty());
    }

    #[test]
    fn test_to_agent_config() {
        let def = AgentDefinition {
            agent_type: "writer".to_string(),
            version: "1.0.0".to_string(),
            description: "Writer agent".to_string(),
            system_prompt: "You are a writing assistant".to_string(),
            model: "gpt-4o".to_string(),
            temperature: 0.8,
            max_steps: 15,
            step_timeout_secs: 45,
            tools: vec!["write_file".to_string()],
            memory: MemoryConfig::default(),
            output_format: None,
            context_window_tokens: None,
            max_parallel_tools: 1,
        };
        let config = def.to_agent_config();
        assert_eq!(config.agent_type, "writer");
        assert_eq!(config.system_prompt, "You are a writing assistant");
        assert_eq!(config.model, "gpt-4o");
        assert!((config.temperature - 0.8).abs() < 0.01);
        assert_eq!(config.max_steps, 15);
        assert_eq!(config.step_timeout, std::time::Duration::from_secs(45));
        assert_eq!(config.tool_names, vec!["write_file"]);
        assert_eq!(config.max_parallel_tools, 1);
    }

    #[test]
    fn test_definition_manager() {
        let dir = make_tmp_dir();
        write_json(
            dir.path(),
            "researcher",
            r#"{"agent_type":"researcher","version":"1.0.0","description":"test researcher","system_prompt":"test","model":"gpt-4","temperature":0.3,"max_steps":10,"step_timeout_secs":30,"tools":[],"output_format":null}"#,
        );
        let mgr = AgentDefinitionManager::new(dir.path().to_path_buf());
        let types = mgr.list_types().expect("list");
        assert_eq!(types, vec!["researcher"]);
        let def = mgr.load("researcher").expect("load");
        assert_eq!(def.agent_type, "researcher");
    }

    #[test]
    fn test_definition_error_display() {
        let err = AgentDefinitionError::NotFound("foo".to_string());
        assert!(format!("{}", err).contains("foo"));

        let err = AgentDefinitionError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file missing",
        ));
        assert!(format!("{}", err).contains("IO error"));

        let err = AgentDefinitionError::Json(
            serde_json::from_str::<serde_json::Value>("bad").unwrap_err(),
        );
        assert!(format!("{}", err).contains("JSON parse error"));
    }

    // ===== P0: MessagePersistConfig 测试（用户决策 2）=====

    #[test]
    fn test_message_persist_config_default() {
        let cfg = MessagePersistConfig::default();
        assert_eq!(cfg.mode, "every_message");
        assert!(cfg.n.is_none());
    }

    #[test]
    fn test_message_persist_config_to_mode_every_message() {
        let cfg = MessagePersistConfig::default();
        let mode = cfg.to_mode().expect("every_message");
        assert_eq!(mode, crate::agent::memory::MessagePersistMode::EveryMessage);
    }

    #[test]
    fn test_message_persist_config_to_mode_every_n() {
        let cfg = MessagePersistConfig {
            mode: "every_n".to_string(),
            n: Some(5),
        };
        let mode = cfg.to_mode().expect("every_n");
        assert_eq!(mode, crate::agent::memory::MessagePersistMode::EveryN(5));
    }

    #[test]
    fn test_message_persist_config_to_mode_per_react_round() {
        let cfg = MessagePersistConfig {
            mode: "per_react_round".to_string(),
            n: None,
        };
        let mode = cfg.to_mode().expect("per_react_round");
        assert_eq!(
            mode,
            crate::agent::memory::MessagePersistMode::PerReactRound
        );
    }

    #[test]
    fn test_message_persist_config_to_mode_disabled() {
        let cfg = MessagePersistConfig {
            mode: "disabled".to_string(),
            n: None,
        };
        let mode = cfg.to_mode().expect("disabled");
        assert_eq!(mode, crate::agent::memory::MessagePersistMode::Disabled);
    }

    #[test]
    fn test_message_persist_config_to_mode_unknown_errors() {
        let cfg = MessagePersistConfig {
            mode: "magic".to_string(),
            n: None,
        };
        let result = cfg.to_mode();
        assert!(result.is_err());
        let err_msg = result.unwrap_err();
        assert!(err_msg.contains("unknown message_persist mode"));
        assert!(err_msg.contains("magic"));
    }

    #[test]
    fn test_message_persist_config_to_mode_every_n_zero_errors() {
        let cfg = MessagePersistConfig {
            mode: "every_n".to_string(),
            n: Some(0),
        };
        let result = cfg.to_mode();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be > 0"));
    }

    #[test]
    fn test_message_persist_config_to_mode_every_n_missing_n_errors() {
        let cfg = MessagePersistConfig {
            mode: "every_n".to_string(),
            n: None,
        };
        let result = cfg.to_mode();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be > 0"));
    }

    #[test]
    fn test_message_persist_config_deserialize_every_n() {
        let json = r#"{"mode":"every_n","n":10}"#;
        let cfg: MessagePersistConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(cfg.mode, "every_n");
        assert_eq!(cfg.n, Some(10));
    }

    #[test]
    fn test_message_persist_config_deserialize_skips_n_when_none() {
        let json = r#"{"mode":"per_react_round"}"#;
        let cfg: MessagePersistConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(cfg.mode, "per_react_round");
        assert!(cfg.n.is_none());

        // 序列化时也应跳过 n 字段（注意不能用 contains("n")，
        // 因为 "per_react_round" 字符串本身包含字母 n）
        let serialized = serde_json::to_string(&cfg).expect("serialize");
        assert!(!serialized.contains("\"n\""));
    }

    // ===== P0: MemoryConfig 扩展字段测试 =====

    #[test]
    fn test_memory_config_default_includes_new_fields() {
        let cfg = MemoryConfig::default();
        assert_eq!(cfg.memory_type, "none");
        assert_eq!(cfg.namespace, "");
        assert_eq!(cfg.message_persist.mode, "every_message");
        assert!(cfg.ttl_secs.is_none());
        assert!(cfg.summary_model.is_none());
    }

    #[test]
    fn test_memory_config_backward_compat_without_new_fields() {
        // 旧版 agent.json 不包含 message_persist/ttl_secs/summary_model
        // 应该能反序列化并用默认值填充
        let json = r#"{"type":"persistent","namespace":"researcher"}"#;
        let cfg: MemoryConfig = serde_json::from_str(json).expect("parse old format");
        assert_eq!(cfg.memory_type, "persistent");
        assert_eq!(cfg.namespace, "researcher");
        assert_eq!(cfg.message_persist.mode, "every_message");
        assert!(cfg.ttl_secs.is_none());
        assert!(cfg.summary_model.is_none());
    }

    #[test]
    fn test_memory_config_with_all_new_fields() {
        let json = r#"{
            "type": "persistent",
            "namespace": "researcher",
            "message_persist": { "mode": "every_n", "n": 3 },
            "ttl_secs": 3600,
            "summary_model": "gpt-4o-mini"
        }"#;
        let cfg: MemoryConfig = serde_json::from_str(json).expect("parse full");
        assert_eq!(cfg.memory_type, "persistent");
        assert_eq!(cfg.namespace, "researcher");
        assert_eq!(cfg.message_persist.mode, "every_n");
        assert_eq!(cfg.message_persist.n, Some(3));
        assert_eq!(cfg.ttl_secs, Some(3600));
        assert_eq!(cfg.summary_model, Some("gpt-4o-mini".to_string()));

        // 验证 to_mode 解析正确
        let mode = cfg.message_persist.to_mode().expect("to_mode");
        assert_eq!(mode, crate::agent::memory::MessagePersistMode::EveryN(3));
    }

    #[test]
    fn test_memory_config_skips_none_fields_when_serializing() {
        let cfg = MemoryConfig::default();
        let json = serde_json::to_string(&cfg).expect("serialize");
        // ttl_secs 和 summary_model 是 None，不应出现在序列化结果中
        assert!(!json.contains("ttl_secs"));
        assert!(!json.contains("summary_model"));
        // message_persist 有默认值，应该出现
        assert!(json.contains("message_persist"));
    }

    #[test]
    fn test_memory_config_ttl_user_decision_5() {
        // 用户决策 5：TTL 由 agent.json 配置
        let json = r#"{
            "type": "persistent",
            "namespace": "researcher",
            "ttl_secs": 86400
        }"#;
        let cfg: MemoryConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(cfg.ttl_secs, Some(86400));
    }

    #[test]
    fn test_memory_config_summary_model_user_decision_3() {
        // 用户决策 3：摘要模型单独配置
        let json = r#"{
            "type": "persistent",
            "namespace": "researcher",
            "summary_model": "minimax/abab6.5s"
        }"#;
        let cfg: MemoryConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(cfg.summary_model, Some("minimax/abab6.5s".to_string()));
    }

    #[test]
    fn test_agent_definition_with_extended_memory_config() {
        // 完整的 agent.json 集成测试：包含所有新字段
        let json = r#"{
            "agent_type": "researcher",
            "version": "1.0.0",
            "description": "Research agent with memory",
            "system_prompt": "You are a research agent",
            "model": "gpt-4o-mini",
            "temperature": 0.3,
            "max_steps": 20,
            "step_timeout_secs": 60,
            "tools": ["search_web"],
            "memory": {
                "type": "persistent",
                "namespace": "researcher",
                "message_persist": { "mode": "per_react_round" },
                "ttl_secs": 7200,
                "summary_model": "gpt-4o-mini"
            },
            "output_format": null
        }"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert_eq!(def.memory.memory_type, "persistent");
        assert_eq!(def.memory.message_persist.mode, "per_react_round");
        assert_eq!(def.memory.ttl_secs, Some(7200));
        assert_eq!(def.memory.summary_model, Some("gpt-4o-mini".to_string()));

        let mode = def.memory.message_persist.to_mode().expect("to_mode");
        assert_eq!(
            mode,
            crate::agent::memory::MessagePersistMode::PerReactRound
        );
    }

    // ===== G13: max_parallel_tools 测试 =====

    #[test]
    fn test_max_parallel_tools_defaults_to_1() {
        // 旧版 agent.json 不含 max_parallel_tools,应默认 1(串行)
        let json = r#"{
            "agent_type": "simple",
            "version": "1.0.0",
            "description": "",
            "system_prompt": "",
            "model": "gpt-4o-mini",
            "temperature": 0.5,
            "max_steps": 10,
            "step_timeout_secs": 30,
            "tools": [],
            "output_format": null
        }"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert_eq!(def.max_parallel_tools, 1, "default should be 1 (serial)");
    }

    #[test]
    fn test_max_parallel_tools_custom_value() {
        let json = r#"{
            "agent_type": "parallel",
            "version": "1.0.0",
            "description": "",
            "system_prompt": "",
            "model": "gpt-4o-mini",
            "temperature": 0.5,
            "max_steps": 10,
            "step_timeout_secs": 30,
            "tools": [],
            "output_format": null,
            "max_parallel_tools": 4
        }"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert_eq!(def.max_parallel_tools, 4);

        let config = def.to_agent_config();
        assert_eq!(config.max_parallel_tools, 4);
    }

    #[test]
    fn test_max_parallel_tools_zero_treated_as_one() {
        // 0 视为 1(防御性)
        let json = r#"{
            "agent_type": "zero",
            "version": "1.0.0",
            "description": "",
            "system_prompt": "",
            "model": "gpt-4o-mini",
            "temperature": 0.5,
            "max_steps": 10,
            "step_timeout_secs": 30,
            "tools": [],
            "output_format": null,
            "max_parallel_tools": 0
        }"#;
        let def: AgentDefinition = serde_json::from_str(json).expect("parse");
        assert_eq!(def.max_parallel_tools, 0, "raw value preserved as 0");
        // to_agent_config 应把 0 规范化为 1
        let config = def.to_agent_config();
        assert_eq!(config.max_parallel_tools, 1, "0 should be normalized to 1");
    }

    #[test]
    fn test_max_parallel_tools_skipped_when_default_in_serialization() {
        let def = AgentDefinition {
            agent_type: "x".to_string(),
            version: "1".to_string(),
            description: String::new(),
            system_prompt: String::new(),
            model: "m".to_string(),
            temperature: 0.5,
            max_steps: 1,
            step_timeout_secs: 1,
            tools: vec![],
            memory: MemoryConfig::default(),
            output_format: None,
            context_window_tokens: None,
            max_parallel_tools: 1,
        };
        let json = serde_json::to_string(&def).expect("serialize");
        assert!(
            !json.contains("max_parallel_tools"),
            "default value should be skipped: {}",
            json
        );
    }

    // ===== C3/C4: MemoryConfig 新增字段默认值测试 =====

    #[test]
    fn test_memory_config_defaults() {
        let cfg = MemoryConfig::default();
        assert!((cfg.memory_budget_ratio - 0.25).abs() < 1e-6);
        assert_eq!(cfg.max_session_summaries, 3);
        assert_eq!(cfg.max_injected_events, 5);
        assert_eq!(cfg.summary_rollup_threshold, 10);
        assert!(cfg.enable_event_extraction);
    }

    #[test]
    fn test_memory_config_c3_c4_fields_backward_compat() {
        // 旧版 agent.json 不含 C3/C4 字段，反序列化应使用默认值
        let json = r#"{"type":"persistent","namespace":"researcher"}"#;
        let cfg: MemoryConfig = serde_json::from_str(json).expect("parse");
        assert!((cfg.memory_budget_ratio - 0.25).abs() < 1e-6);
        assert_eq!(cfg.max_session_summaries, 3);
        assert_eq!(cfg.max_injected_events, 5);
        assert_eq!(cfg.summary_rollup_threshold, 10);
        assert!(cfg.enable_event_extraction);
    }
}
