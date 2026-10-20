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
    let p = key_file_path(paths);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&p, stored).map_err(|e| format!("write llm-key.txt: {e}"))
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
    let raw = read_key_raw(paths)?;
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
        Some(raw) // 存量明文兼容（第三进程编排批格式）
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

#[tauri::command]
pub fn wizard_finish(sup: tauri::State<'_, SupHandle>) -> Result<(), String> {
    mark_wizard_done(&sup.paths)
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
}
