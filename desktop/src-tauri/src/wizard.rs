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
    key.chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
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
        // coding 套餐端点（token plan key 在标准 /api/paas/v4 会报 1113 余额
        // 错误；本目录按套餐端点配置，标准计费 key 需自行改回标准端点）
        execute_url: "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions",
        models: &["glm-4.6", "glm-4.5-air"],
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

/// 分 provider 保存 key（provider 须在目录或自定义清单内；校验与 DPAPI 形态
/// 同 save_key）
pub fn save_provider_key(paths: &ResolvedPaths, name: &str, raw: &str) -> Result<(), String> {
    let path = provider_key_file(paths, name).ok_or_else(|| format!("未知模型服务: {name}"))?;
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

/// 编排层消费：全部已配置的 (name, key) 列表（内置目录+自定义，目录序优先）。
/// 自定义清单损坏 → Err（调用方须显式留痕后降级，不静默吞掉）。
pub fn read_provider_keys(paths: &ResolvedPaths) -> Result<Vec<(String, String)>, String> {
    let all = read_all_providers(paths)?;
    Ok(all
        .into_iter()
        .filter_map(|p| read_provider_key(paths, &p.name).map(|k| (p.name, k)))
        .collect())
}

// ---- 自定义 provider（OpenAI 兼容；与内置目录同 key 存储与执法面） ----

pub const CUSTOM_PROVIDERS_FILE: &str = "llm-custom-providers.json";

/// 自定义 provider 声明（`llm-custom-providers.json` 数组元素）。
///
/// 实际准入仍以白名单物料为正源（执行体启动执法「路由 ⊆ 物料」）；本清单
/// 只承载端点与凭据路由，新增/修改不触仓库。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CustomProvider {
    /// 稳定名（ascii 小写数字；决定 key 文件名与环境变量后缀）
    pub name: String,
    pub display: String,
    /// OpenAI 兼容 chat/completions 全 URL
    pub execute_url: String,
    pub models: Vec<String>,
}

pub fn custom_providers_path(paths: &ResolvedPaths) -> PathBuf {
    paths.app_config_dir.join(CUSTOM_PROVIDERS_FILE)
}

fn custom_providers_load(paths: &ResolvedPaths) -> Result<Vec<CustomProvider>, String> {
    let raw = match std::fs::read_to_string(custom_providers_path(paths)) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()), // 不存在 = 空清单（常态）
    };
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&raw).map_err(|e| {
        format!(
            "自定义模型服务清单解析失败（{}）: {e}",
            CUSTOM_PROVIDERS_FILE
        )
    })
}

fn custom_providers_store(paths: &ResolvedPaths, list: &[CustomProvider]) -> Result<(), String> {
    let p = custom_providers_path(paths);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    // tmp+rename 原子写（损坏清单=全部自定义服务失效，不容半写态）
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, json_pretty(&list.to_vec()).as_bytes())
        .map_err(|e| format!("write {}: {e}", CUSTOM_PROVIDERS_FILE))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("rename {}: {e}", CUSTOM_PROVIDERS_FILE))
}

fn json_pretty<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "[]".to_string())
}

fn validate_custom_provider(p: &CustomProvider) -> Result<(), String> {
    let ok_name = !p.name.is_empty()
        && p.name.len() <= 32
        && p.name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !ok_name {
        return Err("服务标识须为小写字母/数字（1-32 字符，用作 key 文件名与环境变量后缀）".into());
    }
    if catalog_entry(&p.name).is_some() {
        return Err(format!("服务标识 '{}' 与内置目录重名", p.name));
    }
    if p.display.trim().is_empty() {
        return Err("显示名不能为空".into());
    }
    if !p.execute_url.starts_with("http://") && !p.execute_url.starts_with("https://") {
        return Err("端点须为 http(s) 全 URL（OpenAI 兼容 chat/completions）".into());
    }
    if p.models.is_empty() || p.models.iter().any(|m| m.trim().is_empty()) {
        return Err("models 须为非空模型名列表".into());
    }
    Ok(())
}

/// 保存（新增或覆盖）自定义 provider
pub fn save_custom_provider(paths: &ResolvedPaths, p: &CustomProvider) -> Result<(), String> {
    validate_custom_provider(p)?;
    let mut list = custom_providers_load(paths)?;
    list.retain(|e| e.name != p.name);
    list.push(p.clone());
    custom_providers_store(paths, &list)
}

/// 移除自定义 provider（key 文件保留——轮换重配时免重贴；显式清理走文件删除）
pub fn remove_custom_provider(paths: &ResolvedPaths, name: &str) -> Result<(), String> {
    let mut list = custom_providers_load(paths)?;
    let before = list.len();
    list.retain(|e| e.name != name);
    if list.len() == before {
        return Err(format!("自定义服务 '{}' 不存在", name));
    }
    custom_providers_store(paths, &list)
}

/// 编排层合并视图条目（内置 + 自定义；目录序在前）
#[derive(Clone, Debug)]
pub struct OwnedProvider {
    pub name: String,
    pub execute_url: String,
    pub models: Vec<String>,
}

/// 仅内置目录视图（自定义清单损坏时的编排层降级面）
pub fn catalog_providers() -> Vec<OwnedProvider> {
    PROVIDERS
        .iter()
        .map(|p| OwnedProvider {
            name: p.name.to_string(),
            execute_url: p.execute_url.to_string(),
            models: p.models.iter().map(|s| s.to_string()).collect(),
        })
        .collect()
}

/// 合并视图：内置目录 + 自定义清单。自定义清单损坏 → Err（编排层显式留痕
/// 后按仅内置降级，绝不静默）。
pub fn read_all_providers(paths: &ResolvedPaths) -> Result<Vec<OwnedProvider>, String> {
    let mut out: Vec<OwnedProvider> = catalog_providers();
    for c in custom_providers_load(paths)? {
        out.push(OwnedProvider {
            name: c.name,
            execute_url: c.execute_url,
            models: c.models,
        });
    }
    Ok(out)
}

/// provider key 文件路径（内置或自定义均在 `llm-keys/<name>.txt`；须为已知
/// provider——内置目录或自定义清单成员）
fn provider_key_file(paths: &ResolvedPaths, name: &str) -> Option<PathBuf> {
    let known = catalog_entry(name).is_some()
        || custom_providers_load(paths)
            .ok()
            .map(|list| list.iter().any(|c| c.name == name))
            .unwrap_or(false);
    if !known {
        return None;
    }
    Some(
        paths
            .app_config_dir
            .join("llm-keys")
            .join(format!("{name}.txt")),
    )
}

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
    pub execute_url: &'static str,
    pub models: &'static [&'static str],
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
                execute_url: p.execute_url,
                models: p.models,
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

/// 自定义服务清单（向导自定义区消费；损坏显式报错不静默）
#[tauri::command]
pub fn wizard_custom_providers(
    sup: tauri::State<'_, SupHandle>,
) -> Result<Vec<CustomProvider>, String> {
    custom_providers_load(&sup.paths)
}

/// 保存（新增或覆盖）自定义服务
#[tauri::command]
pub fn wizard_save_custom_provider(
    sup: tauri::State<'_, SupHandle>,
    provider: CustomProvider,
) -> Result<(), String> {
    save_custom_provider(&sup.paths, &provider)
}

/// 移除自定义服务
#[tauri::command]
pub fn wizard_remove_custom_provider(
    sup: tauri::State<'_, SupHandle>,
    name: String,
) -> Result<(), String> {
    remove_custom_provider(&sup.paths, &name)
}

// ---- provider 连通性测试（向导「测试」按钮；把验收黄金回合的第一轮产品化） ----

/// 测试请求：端点 + 模型 + key（输入框现值优先；空则按 name 读已存 key）。
/// key 仅本次请求内存使用，不落盘不进日志。
#[derive(Debug, serde::Deserialize)]
pub struct TestProviderRequest {
    pub execute_url: String,
    #[serde(default)]
    pub key: String,
    pub model: String,
    /// 已保存 provider 的名字（key 为空时按它读已存凭据，密钥不过 JS）
    #[serde(default)]
    pub name: Option<String>,
}

/// 测试结果：ok + 延迟；失败时归类（network/auth/provider_error/shape）与
/// 上游归因透传（错误体 message，不含 key）。
#[derive(Debug, serde::Serialize)]
pub struct TestProviderResult {
    pub ok: bool,
    pub latency_ms: u64,
    pub reply: Option<String>,
    pub error_class: Option<String>,
    pub error_detail: Option<String>,
}

/// 响应分类（纯函数，单测锚定）：HTTP 状态门 → 业务错误体门 → 文本门。
pub(crate) fn classify_provider_test(
    status: u16,
    body: &str,
) -> (bool, Option<String>, Option<String>, Option<String>) {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let err_msg = parsed.get("error").map(|e| match e {
        serde_json::Value::String(s) => s.clone(),
        _ => e
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| e.to_string()),
    });
    if !(200..300).contains(&status) {
        let class = match status {
            401 | 403 => "auth",
            _ => "provider_error",
        };
        return (
            false,
            Some(class.to_string()),
            Some(err_msg.unwrap_or_else(|| format!("HTTP {status}"))),
            None,
        );
    }
    if let Some(e) = err_msg {
        return (false, Some("provider_error".to_string()), Some(e), None);
    }
    let reply = parsed
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|s| s.as_str())
        .map(str::to_string);
    match reply {
        Some(t) if !t.trim().is_empty() => {
            let head: String = t.chars().take(60).collect();
            (true, None, None, Some(head))
        }
        _ => (
            false,
            Some("shape".to_string()),
            Some("响应无 choices[0].message.content 文本".to_string()),
            None,
        ),
    }
}

/// 连通性测试（真实最小 chat 调用）：连接 10s/总 30s 超时，回复 60 字符截头。
#[tauri::command]
pub async fn wizard_test_provider(
    sup: tauri::State<'_, SupHandle>,
    req: TestProviderRequest,
) -> Result<TestProviderResult, String> {
    let started = std::time::Instant::now();
    // key 解析：输入框现值优先（测未保存的新 key），空则按 name 读已存凭据
    // （密钥不出 Rust 侧，不过 JS）
    let key = if !req.key.trim().is_empty() {
        req.key.trim().to_string()
    } else if let Some(name) = req.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        read_provider_key(&sup.paths, name).unwrap_or_default()
    } else {
        String::new()
    };
    if key.trim().is_empty() {
        return Ok(TestProviderResult {
            ok: false,
            latency_ms: 0,
            reply: None,
            error_class: Some("no_key".to_string()),
            error_detail: Some("未提供 key 且该服务尚未保存凭据".to_string()),
        });
    }
    let body = serde_json::json!({
        "model": req.model.trim(),
        "messages": [{ "role": "user", "content": "Reply with exactly: OK" }],
        "max_tokens": 16,
        "stream": false,
    });
    let client = match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Ok(TestProviderResult {
                ok: false,
                latency_ms: 0,
                reply: None,
                error_class: Some("network".to_string()),
                error_detail: Some(format!("client build: {e}")),
            })
        }
    };
    let resp = client
        .post(req.execute_url.trim())
        .bearer_auth(&key)
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await;
    let latency = started.elapsed().as_millis() as u64;
    Ok(match resp {
        Err(e) => TestProviderResult {
            ok: false,
            latency_ms: latency,
            reply: None,
            error_class: Some("network".to_string()),
            error_detail: Some(e.to_string()),
        },
        Ok(r) => {
            let status = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            let (ok, class, detail, reply) = classify_provider_test(status, &text);
            TestProviderResult {
                ok,
                latency_ms: latency,
                reply,
                error_class: class,
                error_detail: detail,
            }
        }
    })
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
        let raw = std::fs::read_to_string(provider_key_file(&paths, "deepseek").unwrap()).unwrap();
        assert!(
            raw.starts_with(CIPHER_PREFIX),
            "provider key stored as cipher"
        );
        assert_eq!(
            read_provider_key(&paths, "deepseek").as_deref(),
            Some("sk-ds-abc123")
        );
        // minimax：独立文件缺位时回落存量 llm-key.txt（单 key 时代零迁移）
        assert!(read_provider_key(&paths, "minimax").is_none());
        std::fs::write(key_file_path(&paths), "sk-legacy-minimax\n").unwrap();
        assert_eq!(
            read_provider_key(&paths, "minimax").as_deref(),
            Some("sk-legacy-minimax")
        );
        // 编排层清单：目录序、含回落项
        let keys = read_provider_keys(&paths).unwrap();
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

    #[test]
    fn custom_provider_save_load_remove_roundtrip() {
        let paths = scratch_paths("custom");
        let p = CustomProvider {
            name: "myprov".into(),
            display: "My Provider".into(),
            execute_url: "https://api.example.com/v1/chat/completions".into(),
            models: vec!["model-a".into(), "model-b".into()],
        };
        save_custom_provider(&paths, &p).unwrap();
        let list = custom_providers_load(&paths).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "myprov");
        // key 文件路径对自定义成员可用
        save_provider_key(&paths, "myprov", "sk-custom-1").unwrap();
        assert_eq!(
            read_provider_key(&paths, "myprov").as_deref(),
            Some("sk-custom-1")
        );
        // 合并视图：内置在前，自定义在尾
        let all = read_all_providers(&paths).unwrap();
        assert_eq!(all.last().unwrap().name, "myprov");
        assert_eq!(all.len(), PROVIDERS.len() + 1);
        // 移除
        remove_custom_provider(&paths, "myprov").unwrap();
        assert!(custom_providers_load(&paths).unwrap().is_empty());
        std::fs::remove_dir_all(&paths.data_dir).ok();
    }

    #[test]
    fn custom_provider_validation_rejects() {
        let paths = scratch_paths("customrej");
        let base = |name: &str| CustomProvider {
            name: name.into(),
            display: "d".into(),
            execute_url: "https://x/v1/chat/completions".into(),
            models: vec!["m".into()],
        };
        assert!(
            save_custom_provider(&paths, &base("Bad-Name")).is_err(),
            "非小写"
        );
        assert!(
            save_custom_provider(&paths, &base("minimax")).is_err(),
            "与内置重名"
        );
        let mut bad_url = base("okname");
        bad_url.execute_url = "ftp://x".into();
        assert!(
            save_custom_provider(&paths, &bad_url).is_err(),
            "非 http(s)"
        );
        let mut no_models = base("okname");
        no_models.models = vec![];
        assert!(
            save_custom_provider(&paths, &no_models).is_err(),
            "空 models"
        );
        std::fs::remove_dir_all(&paths.data_dir).ok();
    }

    #[test]
    fn classify_provider_test_covers_main_paths() {
        // 2xx + 文本 → ok
        let (ok, class, _, reply) =
            classify_provider_test(200, r#"{"choices":[{"message":{"content":"OK"}}]}"#);
        assert!(ok && class.is_none() && reply.as_deref() == Some("OK"));
        // 401 → auth + 透传 message
        let (ok, class, detail, _) =
            classify_provider_test(401, r#"{"error":{"message":"invalid key"}}"#);
        assert!(
            !ok && class.as_deref() == Some("auth") && detail.as_deref() == Some("invalid key")
        );
        // 200 + 业务错误体 → provider_error（余额类/伪 200 错误形态）
        let (ok, class, detail, _) =
            classify_provider_test(200, r#"{"error":{"code":"1113","message":"余额不足"}}"#);
        assert!(
            !ok && class.as_deref() == Some("provider_error")
                && detail.as_deref() == Some("余额不足")
        );
        // 200 + 无文本 → shape
        let (ok, class, _, _) = classify_provider_test(200, r#"{"choices":[]}"#);
        assert!(!ok && class.as_deref() == Some("shape"));
    }
}
