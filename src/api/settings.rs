// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 工作台设置体系（IDE 设置板块）
//!
//! 用户/工作区两级设置模型（对齐 VS Code User→Workspace 合并序）：
//!
//! - **注册表（schema）单源在 serve 端**：键 ID/类型/默认值/枚举/范围/分类/描述；
//!   前端设置 UI 渲染、校验提示与 JSON 补全全部消费
//!   `GET /api/workbench/settings/schema` 下发的条目，前后端零双声明。
//! - **User 层** = `data/workbench_settings.json`（serve 原子写；PUT 端点唯一写入口）。
//! - **Workspace 层** = `<workdir>/.evo/settings.json`（只读合并源；人工经文件面编辑，
//!   保存后下一次 GET 合并生效）。
//! - **合并序** Default→User→Workspace（后层覆盖前层），`sources` 标注每键生效层；
//!   损坏文件跳过该层（降级缺省），workspace 层内非法键值跳过该键（fail-soft）。
//!
//! 数据面边界（治理接点声明）：
//! - serve 端读写范围限定上述**两个固定路径**（构造期由 workdir 派生并过
//!   [`ensure_fixed_relative`] 白名单门，拒绝绝对路径与父目录穿越）；无任意路径写入口。
//! - 磁盘文档为扁平键对象（`{"editor.fontSize": 18}`，键 ID 即 schema 键）。
//! - 不含任何密钥/服务端配置——那是 evo-agent.toml 启动期配置域；设置页只消费
//!   `/admin/llm-status` 脱敏快照，零新增泄露面。
//! - 设置读写不落审批链（与按钮点击同级；settings 属工作台数据面非规则面）。

use std::path::{Component, Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::api::agent_api::AgentApiState;

// =============================================================================
// 设置项注册表（schema；v1 共 7 键，默认值=现行为，零回归）
// =============================================================================

/// 设置值类型（schema 下发与校验共用）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingType {
    /// 布尔
    Boolean,
    /// 数值（可带 range 闭区间约束）
    Number,
    /// 字符串（v1 无此类型键，词汇表预留）
    String,
    /// 枚举（enum_values 白名单）
    Enum,
    /// 数组（v1 仅键位覆盖列表，元素形态另校验）
    Array,
}

/// 设置项注册表条目（声明即生效：UI 渲染/校验/默认值三驱动）
#[derive(Debug, Clone, Serialize)]
pub struct SettingEntry {
    /// 扁平键 ID（域前缀.名称，VS Code 事实标准）
    pub key: &'static str,
    /// 值类型（UI 按此渲染控件、校验按此判型）
    #[serde(rename = "type")]
    pub kind: SettingType,
    /// 默认值（= IDE 现行为，保证零回归）
    pub default: Value,
    /// enum 类型的候选值（其余类型为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<&'static str>>,
    /// number 类型的闭区间范围 `[min, max]`（其余类型为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<[i64; 2]>,
    /// UI 分组（编辑器/工作台/键位/AI）
    pub category: &'static str,
    /// 展示描述（设置页与 JSON 补全文档共用）
    pub description: &'static str,
    /// `window` | `application`（application 禁工作区覆盖；v1 无此类键，机制预留）
    pub scope: &'static str,
}

/// v1 设置项清单（7 键）。
///
/// - `editor.minimap` 默认 **false**：EditorPane 创建参数实态即显式关闭
///   （设计初稿记 true 为 Monaco 缺省口径，实施期按「默认值=现行为」原则修正）。
/// - `workbench.theme` v1 单值（消费方 EditorPane 两处创建参数原值）；主题明暗切换随主题板块扩展。
/// - `keybindings.overrides` 数据格式沿用命令面板覆盖层 `[{key, command, when?}]`，
///   仅存储点由浏览器 localStorage 迁入（读取端过滤非法条目，写入端同口径校验）。
/// - `ai.autocomplete.enabled` 为后续 AI 补全板块的预留键（落地前无消费方）。
pub fn schema() -> Vec<SettingEntry> {
    vec![
        SettingEntry {
            key: "editor.fontSize",
            kind: SettingType::Number,
            default: json!(14),
            enum_values: None,
            range: Some([9, 28]),
            category: "编辑器",
            description: "编辑器字体大小",
            scope: "window",
        },
        SettingEntry {
            key: "editor.tabSize",
            kind: SettingType::Number,
            default: json!(4),
            enum_values: None,
            range: Some([1, 8]),
            category: "编辑器",
            description: "一个制表符等效的空格数",
            scope: "window",
        },
        SettingEntry {
            key: "editor.wordWrap",
            kind: SettingType::Enum,
            default: json!("off"),
            enum_values: Some(vec!["on", "off"]),
            range: None,
            category: "编辑器",
            description: "控制折行方式：on 按视口宽度折行，off 不折行",
            scope: "window",
        },
        SettingEntry {
            key: "editor.minimap",
            kind: SettingType::Boolean,
            default: json!(false),
            enum_values: None,
            range: None,
            category: "编辑器",
            description: "是否显示小地图（缩略图）",
            scope: "window",
        },
        SettingEntry {
            key: "workbench.theme",
            kind: SettingType::Enum,
            default: json!("evorule-dark"),
            enum_values: Some(vec!["evorule-dark"]),
            range: None,
            category: "工作台",
            description: "工作台配色主题（当前仅深色；明暗切换随主题板块扩展）",
            scope: "window",
        },
        SettingEntry {
            key: "keybindings.overrides",
            kind: SettingType::Array,
            default: json!([]),
            enum_values: None,
            range: None,
            category: "键位",
            description: "自定义键位覆盖列表，元素形如 {\"key\": \"ctrl+shift+p\", \"command\": \"命令 ID\"}（可选 when 子句）",
            scope: "window",
        },
        SettingEntry {
            key: "ai.autocomplete.enabled",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "AI",
            description: "AI 内联补全开关（补全能力上线后生效）",
            scope: "window",
        },
        SettingEntry {
            key: "agentTools.fileCreate",
            kind: SettingType::Boolean,
            default: json!(true),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 创建文件/目录（file_create；开启后每次调用默认需人工审批）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.fileMove",
            kind: SettingType::Boolean,
            default: json!(false),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 移动/重命名文件（file_move；开启后每次调用默认需人工审批）",
            scope: "application",
        },
        SettingEntry {
            key: "agentTools.fileDelete",
            kind: SettingType::Boolean,
            default: json!(false),
            enum_values: None,
            range: None,
            category: "Agent 工具",
            description: "允许 agent 删除文件（file_delete，软删除进回收目录；开启后每次调用默认需人工审批）",
            scope: "application",
        },
    ]
}

fn find_entry(key: &str) -> Option<SettingEntry> {
    schema().into_iter().find(|e| e.key == key)
}

// =============================================================================
// 校验（键白名单 + 类型 + range/enum + 数组元素形态）
// =============================================================================

/// 校验「键存在 + 值合法」。`value=None` 表示重置（只查键白名单）。
pub fn validate_put(key: &str, value: Option<&Value>) -> Result<(), String> {
    let entry = find_entry(key)
        .ok_or_else(|| format!("unknown setting key: {key} (not in the settings schema)"))?;
    let Some(v) = value else {
        return Ok(()); // 重置：移除用户层覆盖，回落默认/工作区值
    };
    match entry.kind {
        SettingType::Boolean => {
            if !v.is_boolean() {
                return Err(format!("setting '{key}' expects a boolean"));
            }
        }
        SettingType::Number => {
            let Some(n) = v.as_f64() else {
                return Err(format!("setting '{key}' expects a number"));
            };
            if !n.is_finite() {
                return Err(format!("setting '{key}' expects a finite number"));
            }
            if let Some([lo, hi]) = entry.range {
                let (lo, hi) = (lo as f64, hi as f64);
                if !(n >= lo && n <= hi) {
                    return Err(format!(
                        "setting '{key}' out of range: {n} (allowed {lo}..={hi})"
                    ));
                }
            }
        }
        SettingType::String => {
            if !v.is_string() {
                return Err(format!("setting '{key}' expects a string"));
            }
        }
        SettingType::Enum => {
            let Some(s) = v.as_str() else {
                return Err(format!(
                    "setting '{key}' expects one of: {}",
                    entry.enum_values.clone().unwrap_or_default().join("|")
                ));
            };
            let allowed = entry.enum_values.unwrap_or_default();
            if !allowed.contains(&s) {
                return Err(format!(
                    "setting '{key}' invalid value: {s} (allowed: {})",
                    allowed.join("|")
                ));
            }
        }
        SettingType::Array => {
            if !v.is_array() {
                return Err(format!("setting '{key}' expects an array"));
            }
            // v1 唯一数组键的元素契约：{key, command} 非空字符串对（when 可选），
            // 与键位覆盖层读取端的过滤口径一致，防脏数据入库。
            if key == "keybindings.overrides" {
                if let Some(arr) = v.as_array() {
                    for (i, el) in arr.iter().enumerate() {
                        let ok = el
                            .get("key")
                            .and_then(Value::as_str)
                            .is_some_and(|s| !s.trim().is_empty())
                            && el
                                .get("command")
                                .and_then(Value::as_str)
                                .is_some_and(|s| !s.trim().is_empty())
                            && el.get("when").map(|w| w.is_string()).unwrap_or(true);
                        if !ok {
                            return Err(format!(
                                "setting '{key}' element #{i} must be {{key, command}} strings (optional when)"
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// workspace 层键级守卫：application scope 拒工作区覆盖 + 值须过 schema 校验
fn accept_workspace_value(entry: &SettingEntry, v: &Value) -> bool {
    entry.scope != "application" && validate_put(entry.key, Some(v)).is_ok()
}

// =============================================================================
// 存储路径白名单（防目录穿越）
// =============================================================================

/// 路径白名单门：只接受「相对 base 的普通相对路径」，拒绝绝对路径/根相对路径
/// （`Path::join` 对根相对与绝对输入会整体替换 base，均构成越界面）与父目录穿越。
/// 两个存储路径构造期过此门；未来新增存储路径必须同门（回归护栏）。
pub fn ensure_fixed_relative(base: &Path, rel: &str) -> Result<PathBuf, String> {
    let p = Path::new(rel);
    if p.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(format!(
            "path not allowed (must be plain relative): '{rel}'"
        ));
    }
    Ok(base.join(p))
}

// =============================================================================
// 存储（User 层原子写；Workspace 层只读合并源）
// =============================================================================

/// 读一层 JSON 文档（缺文件/损坏 → 空层降级；非对象 → 空层）
fn load_layer(path: &Path) -> Map<String, Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Map::new();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(), // 损坏/非对象 → 空层（下次 PUT 原子写覆盖修复）
    }
}

/// 工作台设置存储（两级）
#[derive(Debug)]
pub struct WorkbenchSettingsStore {
    user_path: PathBuf,
    workspace_path: PathBuf,
}

impl WorkbenchSettingsStore {
    /// 构造（两个固定路径由 workdir 派生并过白名单门；常量天然合规，
    /// 门是防常量演化为动态输入的回归护栏）
    pub fn new(workdir: &Path) -> Result<Self, String> {
        Ok(Self {
            user_path: ensure_fixed_relative(workdir, "data/workbench_settings.json")?,
            workspace_path: ensure_fixed_relative(workdir, ".evo/settings.json")?,
        })
    }

    /// user 层文件路径（单测与诊断用）
    pub fn user_path(&self) -> &Path {
        &self.user_path
    }

    /// 合并读取：Default→User→Workspace（后层覆盖前层）。
    /// 返回 `(settings, sources)`；sources 每键标注 `default|user|workspace`。
    /// workspace 层未知键忽略、非法值/application scope 键跳过（fail-soft）。
    pub fn merged(&self) -> (Map<String, Value>, Map<String, Value>) {
        let user = load_layer(&self.user_path);
        let ws_raw = load_layer(&self.workspace_path);
        let mut workspace = Map::new();
        for (k, v) in ws_raw {
            if let Some(e) = find_entry(&k) {
                if accept_workspace_value(&e, &v) {
                    workspace.insert(k, v);
                }
            }
        }
        let mut settings = Map::new();
        let mut sources = Map::new();
        for e in schema() {
            let (v, src) = if let Some(w) = workspace.get(e.key) {
                (w.clone(), "workspace")
            } else if let Some(u) = user.get(e.key) {
                (u.clone(), "user")
            } else {
                (e.default.clone(), "default")
            };
            settings.insert(e.key.to_string(), v);
            sources.insert(e.key.to_string(), json!(src));
        }
        (settings, sources)
    }

    /// 写 user 层单键（`value=None` 表示重置=从 user 层移除该键；纯 IO 无校验，
    /// 校验由 [`validate_put`] 在端点层先行）。
    pub fn write_user_layer_key(&self, key: &str, value: Option<&Value>) -> Result<(), String> {
        let mut layer = load_layer(&self.user_path);
        match value {
            Some(v) => {
                layer.insert(key.to_string(), v.clone());
            }
            None => {
                layer.remove(key);
            }
        }
        self.save_atomic(&self.user_path, &layer)
    }

    /// 原子写（tmp + rename；父目录确保存在；Windows rename 覆盖已存在文件）
    fn save_atomic(&self, path: &Path, doc: &Map<String, Value>) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create settings dir failed: {e}"))?;
        }
        let tmp = path.with_extension("json.tmp");
        {
            let text = serde_json::to_string_pretty(doc)
                .map_err(|e| format!("serialize settings failed: {e}"))?;
            let mut f =
                std::fs::File::create(&tmp).map_err(|e| format!("write tmp failed: {e}"))?;
            use std::io::Write as _;
            f.write_all(text.as_bytes())
                .map_err(|e| format!("write tmp failed: {e}"))?;
        }
        std::fs::rename(&tmp, path).map_err(|e| format!("rename failed: {e}"))?;
        Ok(())
    }
}

// =============================================================================
// 端点（三件；G16 鉴权中间件自动覆盖）
// =============================================================================

/// `GET /api/workbench/settings` —— 合并后全量 + 每键生效层标注
pub async fn get_settings(State(state): State<AgentApiState>) -> Json<Value> {
    let (settings, sources) = state.workbench_settings().merged();
    Json(json!({ "settings": settings, "sources": sources }))
}

/// `PUT /api/workbench/settings` 请求体（`value: null` = 重置该键）
#[derive(Debug, Deserialize)]
pub struct PutSettingBody {
    /// 扁平键 ID（须在 schema 白名单内）
    pub key: String,
    /// 新值（null 表示重置=移除用户层覆盖）
    pub value: Value,
}

/// `PUT /api/workbench/settings` —— 写 user 层单键（校验非法 400；IO 失败 500；原子写）。
/// 返回写后合并值与生效层，前端可直取更新。
pub async fn put_settings(
    State(state): State<AgentApiState>,
    Json(body): Json<PutSettingBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let store = state.workbench_settings();
    let v = if body.value.is_null() {
        None
    } else {
        Some(&body.value)
    };
    validate_put(&body.key, v).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    store
        .write_user_layer_key(&body.key, v)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let (settings, sources) = store.merged();
    let source = sources.get(&body.key).cloned().unwrap_or(json!("default"));
    Ok(Json(json!({
        "key": body.key,
        "value": settings.get(&body.key).cloned().unwrap_or(Value::Null),
        "source": source,
    })))
}

/// `GET /api/workbench/settings/schema` —— 注册表下发（UI 渲染/校验/JSON 补全单源）
pub async fn get_settings_schema() -> Json<Value> {
    Json(json!({ "entries": schema() }))
}

// =============================================================================
// 单测（合并序/校验/降级/白名单/原子写）
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立临时目录（测试间不共享）
    fn temp_store(tag: &str) -> (PathBuf, WorkbenchSettingsStore) {
        let dir = std::env::temp_dir().join(format!(
            "evo-settings-test-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let store = WorkbenchSettingsStore::new(&dir).unwrap();
        (dir, store)
    }

    fn write_file(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    fn user_file(dir: &Path) -> PathBuf {
        dir.join("data").join("workbench_settings.json")
    }

    fn ws_file(dir: &Path) -> PathBuf {
        dir.join(".evo").join("settings.json")
    }

    #[test]
    fn merged_defaults_when_no_files() {
        let (_d, store) = temp_store("defaults");
        let (settings, sources) = store.merged();
        assert_eq!(settings.len(), 10);
        assert_eq!(settings["editor.fontSize"], json!(14));
        assert_eq!(settings["editor.minimap"], json!(false)); // DC-1
        assert_eq!(settings["editor.wordWrap"], json!("off"));
        assert_eq!(settings["keybindings.overrides"], json!([]));
        // agentTools.* 开关键:出厂默认(fileCreate 开 / fileMove、fileDelete 关)
        assert_eq!(settings["agentTools.fileCreate"], json!(true));
        assert_eq!(settings["agentTools.fileMove"], json!(false));
        assert_eq!(settings["agentTools.fileDelete"], json!(false));
        for (k, v) in &sources {
            assert_eq!(v, &json!("default"), "key {k} should be default");
        }
    }

    #[test]
    fn merge_order_default_user_workspace() {
        let (d, store) = temp_store("order");
        write_file(
            &user_file(&d),
            r#"{"editor.fontSize": 18, "editor.wordWrap": "on"}"#,
        );
        write_file(
            &ws_file(&d),
            r#"{"editor.fontSize": 20, "editor.tabSize": 2, "unknown.key": 1, "editor.minimap": "yes"}"#,
        );
        let (settings, sources) = store.merged();
        // workspace 覆盖 user 覆盖 default
        assert_eq!(settings["editor.fontSize"], json!(20));
        assert_eq!(sources["editor.fontSize"], json!("workspace"));
        // user 生效
        assert_eq!(settings["editor.wordWrap"], json!("on"));
        assert_eq!(sources["editor.wordWrap"], json!("user"));
        // workspace 独有键生效
        assert_eq!(settings["editor.tabSize"], json!(2));
        assert_eq!(sources["editor.tabSize"], json!("workspace"));
        // workspace 未知键忽略；非法值键跳过（回落 default）
        assert_eq!(settings["editor.minimap"], json!(false));
        assert_eq!(sources["editor.minimap"], json!("default"));
    }

    #[test]
    fn corrupt_layers_degrade_to_default() {
        let (d, store) = temp_store("corrupt");
        write_file(&user_file(&d), "{ not json !!!");
        write_file(&ws_file(&d), "[]"); // 非对象
        let (settings, sources) = store.merged();
        assert_eq!(settings["editor.fontSize"], json!(14));
        assert_eq!(sources["editor.fontSize"], json!("default"));
        // 损坏层不阻塞 PUT（原子写覆盖修复）
        store
            .write_user_layer_key("editor.fontSize", Some(&json!(21)))
            .unwrap();
        let (settings, sources) = store.merged();
        assert_eq!(settings["editor.fontSize"], json!(21));
        assert_eq!(sources["editor.fontSize"], json!("user"));
    }

    #[test]
    fn put_and_reset_roundtrip() {
        let (_d, store) = temp_store("roundtrip");
        store
            .write_user_layer_key("editor.fontSize", Some(&json!(24)))
            .unwrap();
        store
            .write_user_layer_key("editor.wordWrap", Some(&json!("on")))
            .unwrap();
        let (settings, sources) = store.merged();
        assert_eq!(settings["editor.fontSize"], json!(24));
        assert_eq!(sources["editor.fontSize"], json!("user"));
        // 重置 = 从 user 层移除，回落默认
        store.write_user_layer_key("editor.fontSize", None).unwrap();
        let (settings, sources) = store.merged();
        assert_eq!(settings["editor.fontSize"], json!(14));
        assert_eq!(sources["editor.fontSize"], json!("default"));
        // 其余键不受影响
        assert_eq!(settings["editor.wordWrap"], json!("on"));
        // 磁盘形态：扁平键对象
        let text = std::fs::read_to_string(user_file(&_d)).unwrap();
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["editor.wordWrap"], json!("on"));
        assert!(doc.get("editor.fontSize").is_none());
    }

    #[test]
    fn atomic_write_pretty_and_loadable() {
        let (_d, store) = temp_store("atomic");
        store
            .write_user_layer_key("editor.fontSize", Some(&json!(9)))
            .unwrap();
        // 直接重读（原子写后文件可解析）
        let text = std::fs::read_to_string(store.user_path()).unwrap();
        assert!(text.contains("\"editor.fontSize\""));
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["editor.fontSize"], json!(9));
        // 无 .tmp 残留
        let tmp = store.user_path().with_extension("json.tmp");
        assert!(!tmp.exists());
    }

    #[test]
    fn validate_rejects_unknown_and_invalid() {
        // 键白名单
        assert!(validate_put("no.such.key", Some(&json!(1))).is_err());
        assert!(validate_put("no.such.key", None).is_err());
        // number 范围
        assert!(validate_put("editor.fontSize", Some(&json!(8))).is_err());
        assert!(validate_put("editor.fontSize", Some(&json!(29))).is_err());
        assert!(validate_put("editor.fontSize", Some(&json!("14"))).is_err());
        assert!(validate_put("editor.fontSize", Some(&json!(9))).is_ok());
        assert!(validate_put("editor.fontSize", Some(&json!(28))).is_ok());
        assert!(validate_put("editor.tabSize", Some(&json!(0))).is_err());
        assert!(validate_put("editor.tabSize", Some(&json!(1))).is_ok());
        // enum
        assert!(validate_put("editor.wordWrap", Some(&json!("auto"))).is_err());
        assert!(validate_put("editor.wordWrap", Some(&json!("on"))).is_ok());
        assert!(validate_put("workbench.theme", Some(&json!("light"))).is_err());
        // boolean
        assert!(validate_put("editor.minimap", Some(&json!("yes"))).is_err());
        assert!(validate_put("editor.minimap", Some(&json!(true))).is_ok());
        // array 元素形态
        assert!(validate_put("keybindings.overrides", Some(&json!({}))).is_err());
        assert!(validate_put("keybindings.overrides", Some(&json!([{"key": "ctrl+k"}]))).is_err());
        assert!(validate_put(
            "keybindings.overrides",
            Some(&json!([{"key": "ctrl+k", "command": "workbench.action.file.save"}]))
        )
        .is_ok());
        assert!(validate_put(
            "keybindings.overrides",
            Some(&json!([{"key": "ctrl+k", "command": "x.y", "when": "tabsOpen"}]))
        )
        .is_ok());
        // 重置只查键白名单
        assert!(validate_put("editor.fontSize", None).is_ok());
    }

    #[test]
    fn workspace_application_scope_guard() {
        let mut e = find_entry("editor.fontSize").unwrap();
        // 同值：window scope 放行
        assert!(accept_workspace_value(&e, &json!(16)));
        // application scope 拒工作区覆盖（机制预留）
        e.scope = "application";
        assert!(!accept_workspace_value(&e, &json!(16)));
        // 非法值拒绝
        e.scope = "window";
        assert!(!accept_workspace_value(&e, &json!("big")));
    }

    #[test]
    fn path_whitelist_rejects_traversal() {
        let base = Path::new("/w");
        // 父目录穿越
        assert!(ensure_fixed_relative(base, "../evil.json").is_err());
        assert!(ensure_fixed_relative(base, "data/../../evil.json").is_err());
        // 根相对路径(join 会替换 base,同属越界面;Windows/Unix 双态都拦)
        assert!(ensure_fixed_relative(base, "/evil.json").is_err());
        // 盘符绝对路径(Windows 形态;Unix 上为普通相对名,不构成越界)
        if cfg!(windows) {
            assert!(ensure_fixed_relative(base, "C:/evil.json").is_err());
        }
        // 合规相对路径
        assert!(ensure_fixed_relative(base, "data/settings.json").is_ok());
        assert!(ensure_fixed_relative(base, ".evo/settings.json").is_ok());
    }
}
