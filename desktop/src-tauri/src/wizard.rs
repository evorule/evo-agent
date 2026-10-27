// 首启向导（批C）：LLM 凭据引导 + DPAPI 加密落盘 + 托盘轮换入口。
//
// 凭据安全边界（延续第三进程编排批的归边原则）：
// - key 只经 Tauri IPC（tauri:// 本地页向导窗，进程内桥）直达壳 Rust 层，
//   主工作台 WebView（serve dist 加载）全程零接触；
// - 落盘统一 DPAPI（CryptProtectData，用户作用域）→ 密文文件格式
//   `dpapi1:<hex>`（前缀自识别，杜绝密/明文歧义）；读侧无前缀按明文兼容
//   （第三进程编排批的存量 llm-key.txt 照常工作）；
// - key 不进日志/不进命令行/不进 runtime 产物。
//
// 生效链：向导保存 key → 「立即生效」调既有 restart_services（托盘同路径）
// → 服务组整组重启 → 执行体启用判定重读 key → LLM 面启用；全程无需重启壳。

use std::path::PathBuf;

use tauri::Manager;

use crate::shell_config::ResolvedPaths;
use crate::SupHandle;

/// LLM key 文件名（app_config_dir 下；与第三进程编排批同位）
pub const KEY_FILE: &str = "llm-key.txt";
/// 密文前缀（读侧自识别；无前缀 = 存量明文兼容）
const CIPHER_PREFIX: &str = "dpapi1:";
/// key 单行长度上限（防误粘整段日志/超长垃圾）
const MAX_KEY_LEN: usize = 4096;

pub fn key_file_path(paths: &ResolvedPaths) -> PathBuf {
    paths.app_config_dir.join(KEY_FILE)
}

pub fn wizard_done_path(paths: &ResolvedPaths) -> PathBuf {
    paths.app_config_dir.join("wizard_done.json")
}

/// 首启检测：key 未配置 且 向导从未完成 → 弹向导
pub fn first_run_pending(paths: &ResolvedPaths) -> bool {
    read_key_raw(paths).is_none() && !wizard_done(paths)
}

fn wizard_done(paths: &ResolvedPaths) -> bool {
    std::fs::read_to_string(wizard_done_path(paths))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("done").and_then(|d| d.as_bool()))
        .unwrap_or(false)
}

fn mark_wizard_done(paths: &ResolvedPaths) -> Result<(), String> {
    let p = wizard_done_path(paths);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::json!({ "done": true, "ts": now_secs() });
    std::fs::write(&p, json.to_string()).map_err(|e| format!("write wizard_done.json: {e}"))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---- DPAPI 封装（windows-sys FFI；非 Windows 编译为明文回落桩） ----

#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    fn blob_from(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_ptr() as *mut u8,
        }
    }

    /// 用户作用域加密（CRYPTPROTECT_UI_FORBIDDEN：服务/无界面上下文可用）
    pub fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob_from(plain);
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // 描述串与熵均不使用；失败码不携带 key 信息
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err("CryptProtectData failed".into());
        }
        let slice = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) };
        let data = slice.to_vec();
        unsafe {
            LocalFree(out.pbData as _);
        }
        Ok(data)
    }

    pub fn unprotect(cipher: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob_from(cipher);
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err("CryptUnprotectData failed".into());
        }
        let slice = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) };
        let data = slice.to_vec();
        unsafe {
            LocalFree(out.pbData as _);
        }
        Ok(data)
    }
}

// ---- 密文编解码（hex；`dpapi1:` 前缀自识别） ----

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// 保存 key：trim + 单行 + 长度校验 → DPAPI 加密落盘
pub fn save_key(paths: &ResolvedPaths, raw: &str) -> Result<(), String> {
    validate_and_store(&key_file_path(paths), raw)
}

/// 校验（trim 非空/单行/长度上限）后 DPAPI 加密落盘（save_key 与分 provider
/// key 共用一套形态：`dpapi1:<hex>` 前缀自识别，读侧明文兼容）
fn validate_and_store(path: &std::path::Path, raw: &str) -> Result<(), String> {
    let key = raw.trim();
    if key.is_empty() {
        return Err("key 不能为空".into());
    }
    if key.len() > MAX_KEY_LEN {
        return Err(format!("key 过长（上限 {MAX_KEY_LEN} 字符）"));
    }
    if key.contains('\n') || key.contains('\r') {
        return Err("key 须为单行".into());
    }
    #[cfg(windows)]
    let stored = format!(
        "{CIPHER_PREFIX}{}",
        hex_encode(&dpapi::protect(key.as_bytes())?)
    );
    #[cfg(not(windows))]
    let stored = key.to_string(); // 非 Windows（内测仅发 Windows）明文回落
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, stored).map_err(|e| format!("write key file: {e}"))
}

/// 原始文件内容（None = 未配置）
fn read_key_raw(paths: &ResolvedPaths) -> Option<String> {
    std::fs::read_to_string(key_file_path(paths))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 供编排层消费：密文前缀走 DPAPI 解密，无前缀按明文兼容返回。
/// 失败（解不开）返回 None 并由调用方记日志——fail-visible 于日志面。
pub fn read_llm_key(paths: &ResolvedPaths) -> Option<String> {
    read_key_file(&key_file_path(paths))
}

fn key_tail4(key: &str) -> String {
    key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect()
}

// ---- 多 provider 目录与分 provider key 存储（模型路由批跟随） ----

/// 内置 provider 目录（OpenAI 兼容 chat/completions 全 URL 形态）。
///
/// `models` 为出厂建议集：**实际准入以白名单物料为准**——执行体启动执法
/// 「路由 ⊆ 物料」，物料未收的模型对应 provider 不装载（启动日志留诊断），
/// 属预期渐进态；扩展 provider/模型以物料扩表为正源，目录只承载端点与凭据。
pub struct ProviderEntry {
    pub name: &'static str,
    pub display: &'static str,
    pub execute_url: &'static str,
    pub models: &'static [&'static str],
}

pub const PROVIDERS: &[ProviderEntry] = &[
    ProviderEntry {
        name: "minimax",
        display: "MiniMax",
        execute_url: "https://api.minimaxi.com/v1/text/chatcompletion_v2",
        models: &["MiniMax-M2.5", "MiniMax-M3"],
    },
    ProviderEntry {
        name: "deepseek",
        display: "DeepSeek",
        execute_url: "https://api.deepseek.com/v1/chat/completions",
        models: &["deepseek-chat", "deepseek-reasoner"],
    },
    ProviderEntry {
        name: "kimi",
        display: "Kimi",
        execute_url: "https://api.moonshot.cn/v1/chat/completions",
        models: &["kimi-k2-0905-preview", "kimi-latest"],
    },
    ProviderEntry {
        name: "zhipu",
        display: "智谱 GLM",
        execute_url: "https://open.bigmodel.cn/api/paas/v4/chat/completions",
        models: &["glm-4.6", "glm-4.5"],
    },
    ProviderEntry {
        name: "qwen",
        display: "通义千问",
        execute_url: "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions",
        models: &["qwen3-max", "qwen-plus"],
    },
];

/// 目录内查找（编排层生成 llm_providers 时消费）
pub fn catalog_entry(name: &str) -> Option<&'static ProviderEntry> {
    PROVIDERS.iter().find(|p| p.name == name)
}

/// provider key 的执行体环境变量名（编排层注入进程；插件 api_key_env 消费
/// 同名义——凭据只走 env，不落盘）
pub fn provider_key_env(name: &str) -> String {
    format!("EVORULE_AI_PLUGIN_LLM_KEY_{}", name.to_ascii_uppercase())
}

fn provider_key_file(paths: &ResolvedPaths, name: &str) -> Option<PathBuf> {
    catalog_entry(name)?;
    Some(
        paths
            .app_config_dir
            .join("llm-keys")
            .join(format!("{name}.txt")),
    )
}

/// 分 provider 保存 key（provider 须在目录内；校验与 DPAPI 形态同 save_key）
pub fn save_provider_key(paths: &ResolvedPaths, name: &str, raw: &str) -> Result<(), String> {
    let path = provider_key_file(paths, name)
        .ok_or_else(|| format!("未知模型服务: {name}"))?;
    validate_and_store(&path, raw)
}

/// 读分 provider key：`llm-keys/<name>.txt`；minimax 回落存量 `llm-key.txt`
/// （单 key 时代部署零迁移）。密文前缀走 DPAPI 解密，无前缀明文兼容；
/// 失败（解不开）None，由调用方记日志。
pub fn read_provider_key(paths: &ResolvedPaths, name: &str) -> Option<String> {
    if let Some(path) = provider_key_file(paths, name) {
        if let Some(k) = read_key_file(&path) {
            return Some(k);
        }
    }
    if name == "minimax" {
        return read_llm_key(paths);
    }
    None
}

/// 编排层消费：目录序返回全部已配置的 (name, key) 列表
pub fn read_provider_keys(paths: &ResolvedPaths) -> Vec<(&'static str, String)> {
    PROVIDERS
        .iter()
        .filter_map(|p| read_provider_key(paths, p.name).map(|k| (p.name, k)))
        .collect()
}

/// 单文件读取（trim 空判 + DPAPI 前缀自识别；read_llm_key 的文件参数化形态）
fn read_key_file(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    if let Some(hexpart) = raw.strip_prefix(CIPHER_PREFIX) {
        let bytes = hex_decode(hexpart)?;
        #[cfg(windows)]
        {
            let plain = dpapi::unprotect(&bytes).ok()?;
            String::from_utf8(plain).ok().map(|s| s.trim().to_string())
        }
        #[cfg(not(windows))]
        String::from_utf8(bytes).ok().map(|s| s.trim().to_string())
    } else {
        Some(raw) // 存量明文兼容
    }
}

// ---- Tauri commands（向导页 IPC 面） ----

#[derive(serde::Serialize)]
pub struct KeyStatus {
    pub configured: bool,
    /// 已配置时的尾 4 位（轮换核对用；不泄全量）
    pub tail4: Option<String>,
    pub key_file: String,
}

#[tauri::command]
pub fn wizard_key_status(sup: tauri::State<'_, SupHandle>) -> KeyStatus {
    let paths = &sup.paths;
    let key = read_llm_key(paths);
    KeyStatus {
        configured: key.is_some(),
        tail4: key.as_ref().map(|k| {
            k.chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<String>()
        }),
        key_file: key_file_path(paths).display().to_string(),
    }
}

#[tauri::command]
pub fn wizard_save_key(sup: tauri::State<'_, SupHandle>, key: String) -> Result<(), String> {
    save_key(&sup.paths, &key)
}

/// 分 provider key 状态（向导「更多模型服务」列表消费）
#[derive(serde::Serialize)]
pub struct ProviderKeyStatus {
    pub name: &'static str,
    pub display: &'static str,
    pub configured: bool,
    /// 已配置时的尾 4 位（轮换核对用；不泄全量）
    pub tail4: Option<String>,
}

#[tauri::command]
pub fn wizard_provider_status(sup: tauri::State<'_, SupHandle>) -> Vec<ProviderKeyStatus> {
    PROVIDERS
        .iter()
        .map(|p| {
            let key = read_provider_key(&sup.paths, p.name);
            ProviderKeyStatus {
                name: p.name,
                display: p.display,
                configured: key.is_some(),
                tail4: key.as_deref().map(key_tail4),
            }
        })
        .collect()
}

/// 分 provider 保存 key（provider 须在内置目录内；DPAPI 加密落 llm-keys/）
#[tauri::command]
pub fn wizard_save_provider_key(
    sup: tauri::State<'_, SupHandle>,
    name: String,
    key: String,
) -> Result<(), String> {
    save_provider_key(&sup.paths, &name, &key)
}

#[tauri::command]
pub fn wizard_finish(sup: tauri::State<'_, SupHandle>) -> Result<(), String> {
    mark_wizard_done(&sup.paths)
}

/// 工作文件夹状态(向导工作区步):缺省根+用户已存偏好(38 号方案 X)。
#[derive(Debug, serde::Serialize)]
pub struct WorkspaceStatus {
    /// 缺省边界根(%USERPROFILE%\Documents\evo-agent)
    pub default_root: String,
    /// 用户已保存的工作文件夹(None=未改过,用缺省)
    pub saved: Option<String>,
}

#[tauri::command]
pub fn wizard_workspace_status(sup: tauri::State<'_, SupHandle>) -> WorkspaceStatus {
    let saved = crate::orchestrator::workspace_pref(&sup.paths.app_config_dir)
        .map(|p| p.display().to_string());
    let default_root = crate::orchestrator::default_agent_root()
        .display()
        .to_string();
    WorkspaceStatus {
        default_root,
        saved,
    }
}

#[tauri::command]
pub fn wizard_save_workspace(sup: tauri::State<'_, SupHandle>, path: String) -> Result<(), String> {
    crate::orchestrator::save_workspace_pref(&sup.paths.app_config_dir, &path)
}

/// 打开向导窗（首启 / 托盘「LLM 凭据…」共用）；已存在则聚焦
pub fn open_wizard(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(existing) = app.get_webview_window("wizard") {
        let _ = existing.show();
        let _ = existing.set_focus();
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(app, "wizard", tauri::WebviewUrl::App("wizard.html".into()))
        .title("evo-agent 首次配置")
        .inner_size(540.0, 460.0)
        .min_inner_size(540.0, 460.0)
        .resizable(false)
        .always_on_top(true)
        .center()
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::shell_config;

    fn scratch_paths(tag: &str) -> ResolvedPaths {
        // 与 orchestrator 测试同口径：std temp_dir + 进程号自清式一次性根
        let root = std::env::temp_dir().join(format!(
            "evo-shell-wizard-test-{tag}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("app-config")).unwrap();
        let mut paths = shell_config::resolve(&shell_config::load()).unwrap();
        paths.app_config_dir = root.join("app-config");
        paths.data_dir = root;
        paths
    }

    #[test]
    fn test_save_and_read_roundtrip() {
        let paths = scratch_paths("roundtrip");
        save_key(&paths, "  sk-live-abcdef123456  ").unwrap();
        // 落盘非明文（dpapi1: 前缀 + hex）
        let raw = read_key_raw(&paths).unwrap();
        assert!(raw.starts_with(CIPHER_PREFIX), "stored form must be cipher");
        // 读出 = trim 后原值
        assert_eq!(
            read_llm_key(&paths).as_deref(),
            Some("sk-live-abcdef123456")
        );
        std::fs::remove_dir_all(&paths.data_dir).ok();
    }

    #[test]
    fn test_legacy_plain_text_still_readable() {
        let paths = scratch_paths("legacy");
        std::fs::write(key_file_path(&paths), "sk-plain-legacy\n").unwrap();
        assert_eq!(read_llm_key(&paths).as_deref(), Some("sk-plain-legacy"));
        std::fs::remove_dir_all(&paths.data_dir).ok();
    }

    #[test]
    fn test_save_rejects_blank_multiline_and_overlong() {
        let paths = scratch_paths("rejects");
        assert!(save_key(&paths, "   ").is_err());
        assert!(save_key(&paths, "a\nb").is_err());
        assert!(save_key(&paths, &"x".repeat(MAX_KEY_LEN + 1)).is_err());
        std::fs::remove_dir_all(&paths.data_dir).ok();
    }

    #[test]
    fn test_first_run_and_done_mark() {
        let paths = scratch_paths("firstrun");
        assert!(first_run_pending(&paths), "fresh config dir = first run");
        mark_wizard_done(&paths).unwrap();
        assert!(!first_run_pending(&paths), "done mark suppresses wizard");
        std::fs::remove_dir_all(&paths.data_dir).ok();
    }

    #[test]
    fn provider_catalog_entries_are_well_formed() {
        // 名字须 ASCII 小写（env 变量名映射与文件名的契约前提）且全局唯一
        let mut names = std::collections::BTreeSet::new();
        for p in PROVIDERS {
            assert!(!p.name.is_empty(), "empty provider name");
            assert!(
                p.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
                "provider name {} must be lowercase ascii",
                p.name
            );
            assert!(names.insert(p.name), "duplicate provider name {}", p.name);
            assert!(p.execute_url.starts_with("https://"), "{} url", p.name);
            assert!(!p.models.is_empty(), "{} models", p.name);
            assert!(!p.display.is_empty(), "{} display", p.name);
        }
    }

    #[test]
    fn provider_key_save_read_roundtrip_and_legacy_fallback() {
        let paths = scratch_paths("provkey");
        // deepseek：独立文件落盘（密文形态），读回一致
        save_provider_key(&paths, "deepseek", "  sk-ds-abc123  ").unwrap();
        let raw = std::fs::read_to_string(provider_key_file(&paths, "deepseek").unwrap())
            .unwrap();
        assert!(raw.starts_with(CIPHER_PREFIX), "provider key stored as cipher");
        assert_eq!(read_provider_key(&paths, "deepseek").as_deref(), Some("sk-ds-abc123"));
        // minimax：独立文件缺位时回落存量 llm-key.txt（单 key 时代零迁移）
        assert!(read_provider_key(&paths, "minimax").is_none());
        std::fs::write(key_file_path(&paths), "sk-legacy-minimax\n").unwrap();
        assert_eq!(
            read_provider_key(&paths, "minimax").as_deref(),
            Some("sk-legacy-minimax")
        );
        // 编排层清单：目录序、含回落项
        let keys = read_provider_keys(&paths);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].0, "minimax");
        assert_eq!(keys[1].0, "deepseek");
        std::fs::remove_dir_all(&paths.data_dir).ok();
    }

    #[test]
    fn provider_key_rejects_unknown_provider_and_blank() {
        let paths = scratch_paths("provreject");
        assert!(save_provider_key(&paths, "no-such-provider", "sk-x").is_err());
        assert!(save_provider_key(&paths, "deepseek", "   ").is_err());
        assert!(read_provider_key(&paths, "no-such-provider").is_none());
        std::fs::remove_dir_all(&paths.data_dir).ok();
    }

    #[test]
    fn provider_key_env_naming_is_stable() {
        assert_eq!(
            provider_key_env("deepseek"),
            "EVORULE_AI_PLUGIN_LLM_KEY_DEEPSEEK"
        );
    }
}
