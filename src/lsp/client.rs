// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Agent 侧自持 LSP 客户端:语言服务器进程会话期缓存 + 文件级诊断/符号查询。
//!
//! 与 UI 的 WS 直通桥(单连接被前端占用)不同,本客户端由工具面自持:
//! 每 (语言, 工作区) 一个实例,进程级注册表缓存,空闲回收。生命周期:
//!
//! - spawn 复用 [`super::process`] 机械(no-shell/argv/workdir 锁定/Drop 兜底);
//! - initialize 握手后按需 didOpen/didChange(全量同步),等首条
//!   publishDiagnostics(有界等待,早退于目标 uri 首条发布);
//! - 服务器→客户端请求按最小应答集回复(configuration/registerCapability/
//!   workDoneProgress/workspaceFolders),不回应即服务器停摆;
//! - 空闲超时(300s)或实例超帽(6)时最旧者 kill 回收。
//!
//! 所有失败 fail-visible 返回字符串错误(含安装指引);建议性消费方
//! (编辑回执诊断)自行把错误降级为字段缺席。

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::time::timeout;

use super::process::LspServerProcess;
use super::transport::encode_frame;
use super::Lang;

/// 诊断等待上限(毫秒):服务器 spawn+initialize+didOpen+首条发布的最坏链路。
/// 早退于目标 uri 首条 publishDiagnostics;rust-analyzer 冷启动全仓加载可能
/// 超帽——返回已收集内容并在信封附 pending 提示,不无限等。
const DIAG_WAIT_MS: u64 = 10_000;
/// 空闲回收阈值(秒)。
const IDLE_EVICTION_SECS: u64 = 300;
/// 进程级实例上限(超帽回收最旧)。
const MAX_CLIENTS: usize = 6;
/// 信封诊断条数帽(超出截断并标注)。
pub const DIAG_CAP: usize = 20;

// =============================================================================
// 语言判定
// =============================================================================

/// 按扩展名判定语言门(不受支持=None,调用方 fail-visible)。
pub fn lang_for_path(path: &Path) -> Option<Lang> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "rs" => Some(Lang::Rust),
        "ts" | "tsx" | "mts" | "cts" | "jsx" => Some(Lang::TypeScript),
        "py" | "pyi" => Some(Lang::Python),
        _ => None,
    }
}

/// 磁盘绝对路径 → `file://` URI(反斜杠归一为正斜杠;空格/中文不编码,
/// 服务器侧以路径后缀匹配回读)。
fn to_uri(abs: &Path) -> String {
    let fwd = abs.to_string_lossy().replace('\\', "/");
    if fwd.starts_with('/') {
        format!("file://{fwd}")
    } else {
        format!("file:///{fwd}")
    }
}

/// 服务器回传 uri 是否指向目标磁盘路径(宽松后缀匹配:编码形态/盘符大小写
/// 差异兼容;诊断归属误配的最坏后果=信封带错文件诊断,advisory 面可容)。
#[allow(dead_code)] // v1 诊断匹配走自建 uri 精确回读;保留作 v2 位置级导航的宽松匹配面
fn uri_matches_path(uri: &str, abs: &Path) -> bool {
    let norm = uri
        .replace("%20", " ")
        .replace("%E2%80%8B", "") // 零宽空格(rust-analyzer 偶发)
        .to_ascii_lowercase();
    let suffix = abs
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let suffix = suffix.trim_start_matches('/');
    norm.trim_start_matches("file://")
        .trim_start_matches('/')
        .ends_with(suffix)
}

// =============================================================================
// 客户端本体
// =============================================================================

/// 一条归一化诊断(信封序列化形态)。
#[derive(Debug, Clone)]
pub struct Diag {
    /// 诊断起始行(0 基,LSP 协议口径)
    pub start_line: u64,
    /// 诊断起始列(0 基,UTF-16 码元口径随 LSP)
    pub start_char: u64,
    /// 诊断结束行(0 基)
    pub end_line: u64,
    /// 诊断结束列(0 基)
    pub end_char: u64,
    /// 严重度(1=error/2=warning/3=info/4=hint;服务器缺省时 Null)
    pub severity: Value,
    /// 诊断消息全文
    pub message: String,
    /// 发出方(服务器自报,如 "pyright";缺省 Null)
    pub source: Value,
}

/// Agent 侧 LSP 客户端(单实例单语言单工作区)。
pub struct LspClient {
    lang: Lang,
    #[allow(dead_code)]
    workdir: PathBuf,
    process: LspServerProcess,
    next_id: u64,
    version: u64,
    opened: std::collections::HashSet<String>,
    /// 各 uri 最新诊断(publishDiagnostics 覆盖式更新)
    latest: HashMap<String, Vec<Diag>>,
    last_used: Instant,
}

impl LspClient {
    /// spawn + initialize 握手 + initialized 通知。
    async fn start(lang: Lang, workdir: PathBuf) -> Result<Self, String> {
        let process = LspServerProcess::spawn(&lang.spec(), &workdir)
            .await
            .map_err(|e| {
                format!(
                    "language server '{}' not available ({}): {}",
                    lang.spec().program,
                    install_hint(lang),
                    e
                )
            })?;
        let mut c = Self {
            lang,
            workdir,
            process,
            next_id: 1,
            version: 0,
            opened: std::collections::HashSet::new(),
            latest: HashMap::new(),
            last_used: Instant::now(),
        };
        let root_uri = to_uri(&c.workdir);
        let params = json!({
            "processId": Value::Null,
            "rootUri": root_uri,
            "workspaceFolders": [{"uri": root_uri, "name": "workspace"}],
            "capabilities": {
                "textDocument": {
                    "synchronization": {"didSave": false},
                    "publishDiagnostics": {"relatedInformation": false},
                },
                "workspace": {"configuration": true, "workspaceFolders": false},
            },
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        c.raw_call("initialize", params, deadline).await?;
        c.notify("initialized", json!({})).await?;
        Ok(c)
    }

    fn take_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    async fn write_frame(&mut self, body: &Value) -> Result<(), String> {
        let bytes = encode_frame(body.to_string().as_bytes());
        use tokio::io::AsyncWriteExt;
        self.process
            .stdin
            .write_all(&bytes)
            .await
            .map_err(|e| format!("lsp stdin write failed: {e}"))?;
        self.process
            .stdin
            .flush()
            .await
            .map_err(|e| format!("lsp stdin flush failed: {e}"))
    }

    /// 发通知(无 id)。
    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let body = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.write_frame(&body).await
    }

    /// 读一帧并分派:服务器请求按最小应答集回复;publishDiagnostics 归档;
    /// 对上 `want_id` 的响应返回其 result/error。返回 `None` = 帧已消化,
    /// 继续读;`Some(Ok(v))` = 命中响应;`Some(Err(e))` = 服务器报错。
    async fn pump_once(
        &mut self,
        want_id: Option<u64>,
    ) -> Result<Option<Result<Value, String>>, String> {
        let frame = self
            .process
            .stdout
            .next_frame()
            .await
            .map_err(|e| format!("lsp stdout read failed: {e}"))?;
        let msg: Value =
            serde_json::from_slice(&frame).map_err(|e| format!("lsp frame not json: {e}"))?;
        let id = msg.get("id").and_then(|v| v.as_u64());
        match (id, msg.get("method").and_then(|m| m.as_str())) {
            (Some(i), Some(method)) => {
                // 服务器→客户端请求:最小应答集
                let result = match method {
                    "workspace/configuration" => {
                        let items = msg
                            .pointer("/params/items")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();
                        Value::Array(items.iter().map(|_| json!({})).collect())
                    }
                    "workspace/workspaceFolders" => {
                        json!([{"uri": to_uri(&self.workdir), "name": "workspace"}])
                    }
                    _ => Value::Null, // registerCapability / workDoneProgress / 其它
                };
                let body = json!({"jsonrpc": "2.0", "id": i, "result": result});
                self.write_frame(&body).await?;
                Ok(None)
            }
            (Some(i), None) if want_id == Some(i) => {
                if let Some(err) = msg.get("error") {
                    return Ok(Some(Err(format!(
                        "lsp server error: {}",
                        err.get("message").and_then(|m| m.as_str()).unwrap_or("?")
                    ))));
                }
                Ok(Some(Ok(msg.get("result").cloned().unwrap_or(Value::Null))))
            }
            (None, Some(_)) => {
                if msg.get("method").and_then(|m| m.as_str())
                    == Some("textDocument/publishDiagnostics")
                {
                    let uri = msg
                        .pointer("/params/uri")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let diags = parse_diagnostics(msg.pointer("/params/diagnostics"));
                    self.latest.insert(uri, diags);
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// 请求-响应(期间消化通知/回复服务器请求),整体有界等待。
    async fn raw_call(
        &mut self,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value, String> {
        let id = self.take_id();
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.write_frame(&body).await?;
        let wait = deadline.saturating_duration_since(Instant::now());
        match timeout(wait, async {
            loop {
                if let Some(res) = self.pump_once(Some(id)).await? {
                    return res;
                }
            }
        })
        .await
        {
            Ok(res) => res,
            Err(_) => Err(format!(
                "lsp call '{method}' timed out (server busy or stuck); retry later or use build_check"
            )),
        }
    }

    /// 确保目标文件已同步到服务器(didOpen 首次 / didChange 全量随后)。
    async fn ensure_open(&mut self, abs: &Path, text: Option<&str>) -> Result<String, String> {
        let uri = to_uri(abs);
        let text = match text {
            Some(t) => t.to_string(),
            None => {
                std::fs::read_to_string(abs).map_err(|e| format!("lsp read target failed: {e}"))?
            }
        };
        self.version += 1;
        if self.opened.insert(uri.clone()) {
            self.notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri, "languageId": self.lang.as_str(),
                        "version": self.version, "text": text,
                    }
                }),
            )
            .await?;
        } else {
            self.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": {"uri": uri, "version": self.version},
                    "contentChanges": [{"text": text}],
                }),
            )
            .await?;
        }
        Ok(uri)
    }

    /// 文件诊断:同步最新内容 → 等目标 uri 首条发布(有界,早退) → 序列化。
    async fn diagnostics(
        &mut self,
        abs: &Path,
        text: Option<&str>,
        wait: Duration,
    ) -> Result<Value, String> {
        self.ensure_open(abs, text).await?;
        let uri = to_uri(abs);
        let deadline = Instant::now() + wait;
        loop {
            if let Some(diags) = self.latest.get(&uri) {
                // 已有发布:返回最新(服务器持续更新,后续调用会再等新发布)
                let out = serialize_diags(diags, &uri, self.lang);
                self.latest.remove(&uri);
                return Ok(out);
            }
            let now = Instant::now();
            if now >= deadline {
                // 超帽:返回 pending 提示形态(不谎报零问题)
                return Ok(json!({
                    "diagnostics": [], "count": 0, "truncated": false,
                    "lang": self.lang.as_str(),
                    "pending": "no diagnostics published within wait window (server may still be indexing); rerun for updated results",
                }));
            }
            let wait = deadline.saturating_duration_since(now);
            match timeout(wait, self.pump_once(None)).await {
                Err(_) => {}
                Ok(Err(e)) => return Err(e),
                Ok(Ok(_)) => {}
            }
        }
    }

    /// 文档符号(文件级;先同步内容再拉取)。
    async fn document_symbols(&mut self, abs: &Path) -> Result<Value, String> {
        self.ensure_open(abs, None).await?;
        let uri = to_uri(abs);
        let deadline = Instant::now() + Duration::from_secs(15);
        let res = self
            .raw_call(
                "textDocument/documentSymbol",
                json!({"textDocument": {"uri": uri}}),
                deadline,
            )
            .await?;
        let arr = res.as_array().cloned().unwrap_or_default();
        let count = arr.len();
        let truncated = count > 100;
        Ok(json!({
            "symbols": if truncated { arr[..100].to_vec() } else { arr },
            "count": count,
            "truncated": truncated,
            "lang": self.lang.as_str(),
        }))
    }
}

fn parse_diagnostics(v: Option<&Value>) -> Vec<Diag> {
    let arr = match v.and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .map(|d| Diag {
            start_line: d
                .pointer("/range/start/line")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            start_char: d
                .pointer("/range/start/character")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            end_line: d
                .pointer("/range/end/line")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            end_char: d
                .pointer("/range/end/character")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            severity: d.get("severity").cloned().unwrap_or(Value::Null),
            message: d
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string(),
            source: d.get("source").cloned().unwrap_or(Value::Null),
        })
        .collect()
}

fn serialize_diags(diags: &[Diag], uri: &str, lang: Lang) -> Value {
    let count = diags.len();
    let truncated = count > DIAG_CAP;
    let items: Vec<Value> = diags
        .iter()
        .take(DIAG_CAP)
        .map(|d| {
            json!({
                "range": {
                    "start": {"line": d.start_line, "character": d.start_char},
                    "end": {"line": d.end_line, "character": d.end_char},
                },
                "severity": d.severity,
                "message": d.message,
                "source": d.source,
            })
        })
        .collect();
    json!({
        "uri": uri,
        "diagnostics": items,
        "count": count,
        "truncated": truncated,
        "lang": lang.as_str(),
    })
}

fn install_hint(lang: Lang) -> &'static str {
    match lang {
        Lang::Rust => "install via `rustup component add rust-analyzer`",
        Lang::TypeScript => "install via `npm install -g typescript typescript-language-server`",
        Lang::Python => "install via `npm install -g pyright`",
    }
}

// =============================================================================
// 进程级注册表(缓存 + 空闲回收 + 超帽驱逐)
// =============================================================================

fn clients() -> &'static Mutex<HashMap<String, Arc<tokio::sync::Mutex<LspClient>>>> {
    static CLIENTS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<LspClient>>>>> =
        OnceLock::new();
    CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn registry_key(lang: Lang, workdir: &Path) -> String {
    format!(
        "{}|{}",
        lang.as_str(),
        workdir
            .canonicalize()
            .unwrap_or_else(|_| workdir.to_path_buf())
            .display()
    )
}

/// 取缓存实例或冷启动;顺带回收空闲/超帽实例。
async fn acquire(lang: Lang, workdir: &Path) -> Result<Arc<tokio::sync::Mutex<LspClient>>, String> {
    let key = registry_key(lang, workdir);
    {
        let mut map = clients().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = map.get(&key) {
            let mut guard = c.try_lock();
            if let Ok(g) = guard.as_mut() {
                g.last_used = Instant::now();
            }
            drop(guard);
            return Ok(Arc::clone(c));
        }
        // 空闲回收 + 超帽驱逐(锁内只做判定与摘除;kill 落在 Drop,不阻塞)
        let idle_cut = Instant::now() - Duration::from_secs(IDLE_EVICTION_SECS);
        let mut stale: Vec<String> = Vec::new();
        for (k, v) in map.iter() {
            if let Ok(g) = v.try_lock() {
                if g.last_used < idle_cut {
                    stale.push(k.clone());
                }
            }
        }
        for k in &stale {
            map.remove(k);
        }
        if map.len() >= MAX_CLIENTS {
            let mut order: Vec<(String, Instant)> = map
                .iter()
                .filter_map(|(k, v)| v.try_lock().ok().map(|g| (k.clone(), g.last_used)))
                .collect();
            order.sort_by_key(|(_, t)| *t);
            for (k, _) in order {
                if map.len() < MAX_CLIENTS {
                    break;
                }
                // 判定与摘除分离:守卫先落(Err 变体借 mutex,不可跨 remove 存活)
                let idle = map
                    .get(&k)
                    .and_then(|c| c.try_lock().ok())
                    .map(|g| g.last_used < Instant::now())
                    .unwrap_or(false);
                if idle {
                    map.remove(&k);
                }
            }
        }
    }
    let client = Arc::new(tokio::sync::Mutex::new(
        LspClient::start(lang, workdir.to_path_buf()).await?,
    ));
    let mut map = clients().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(prev) = map.insert(key, Arc::clone(&client)) {
        // 竞态兜底:他任务先插同键——旧实例交 Drop 杀进程
        drop(prev);
    }
    Ok(client)
}

/// 关闭(缓存摘除+kill)指定实例(测试用;生产走空闲回收)。
pub fn shutdown(lang: Lang, workdir: &Path) {
    let key = registry_key(lang, workdir);
    if let Ok(mut map) = clients().lock() {
        map.remove(&key);
    }
}

// =============================================================================
// 工具面公共 API
// =============================================================================

fn resolve_abs(workdir: &Path, path: &str) -> Result<PathBuf, String> {
    let trimmed = path.trim_start_matches(['/', '\\']);
    let p = Path::new(trimmed);
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    Ok(workdir.join(p))
}

/// 工具面:文件诊断(fail-visible;错误串含安装指引)。
pub async fn diagnostics_for_tool(
    workdir: &Path,
    path: &str,
    wait_ms: Option<u64>,
) -> Result<Value, String> {
    let abs = resolve_abs(workdir, path)?;
    let lang = lang_for_path(&abs).ok_or_else(|| {
        "unsupported language for lsp (supported: .rs/.ts/.tsx/.jsx/.py/.pyi)".to_string()
    })?;
    if !abs.is_file() {
        return Err(format!("file not found: {}", abs.display()));
    }
    let wait = Duration::from_millis(wait_ms.unwrap_or(DIAG_WAIT_MS).min(DIAG_WAIT_MS));
    let client = acquire(lang, workdir).await?;
    let mut guard = client.lock().await;
    let res = guard.diagnostics(&abs, None, wait).await;
    if res.is_err() {
        drop(guard);
        shutdown(lang, workdir); // 进程态可疑:摘缓存,下次调用冷启
    }
    res
}

/// 工具面:文档符号(fail-visible)。
pub async fn document_symbols_for_tool(workdir: &Path, path: &str) -> Result<Value, String> {
    let abs = resolve_abs(workdir, path)?;
    let lang = lang_for_path(&abs).ok_or_else(|| {
        "unsupported language for lsp (supported: .rs/.ts/.tsx/.jsx/.py/.pyi)".to_string()
    })?;
    if !abs.is_file() {
        return Err(format!("file not found: {}", abs.display()));
    }
    let client = acquire(lang, workdir).await?;
    let mut guard = client.lock().await;
    let res = guard.document_symbols(&abs).await;
    if res.is_err() {
        drop(guard);
        shutdown(lang, workdir);
    }
    res
}

/// 编辑回执诊断(建议性):门控(coding 档+lsp 开)+语言支持全部通过才拉;
/// 任何失败返回 None(编辑本身已成功,诊断缺席不得反噬)。
pub async fn envelope_diagnostics(
    workdir: &Path,
    abs: &Path,
    new_text: Option<&str>,
) -> Option<Value> {
    if !crate::api::settings::lsp_agent_gate(workdir) {
        return None;
    }
    let lang = lang_for_path(abs)?;
    let client = acquire(lang, workdir).await.ok()?;
    let mut guard = client.lock().await;
    guard
        .diagnostics(abs, new_text, Duration::from_millis(DIAG_WAIT_MS))
        .await
        .ok()
}

// =============================================================================
// 测试
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_detection_by_extension() {
        assert_eq!(lang_for_path(Path::new("a/b.rs")), Some(Lang::Rust));
        assert_eq!(lang_for_path(Path::new("x.py")), Some(Lang::Python));
        assert_eq!(lang_for_path(Path::new("x.PY")), Some(Lang::Python));
        assert_eq!(lang_for_path(Path::new("c.ts")), Some(Lang::TypeScript));
        assert_eq!(lang_for_path(Path::new("c.tsx")), Some(Lang::TypeScript));
        assert_eq!(lang_for_path(Path::new("readme.md")), None);
        assert_eq!(lang_for_path(Path::new("noext")), None);
    }

    #[test]
    fn uri_shape_windows_and_posix() {
        let win = to_uri(Path::new("D:\\repo\\src\\a.rs"));
        assert_eq!(win, "file:///D:/repo/src/a.rs");
        let posix = to_uri(Path::new("/home/u/a.py"));
        assert_eq!(posix, "file:///home/u/a.py");
    }

    #[test]
    fn uri_match_tolerates_encoding_and_case() {
        let abs = Path::new("D:\\Repo\\Src\\My File.py");
        assert!(uri_matches_path("file:///d:/repo/src/My%20File.py", abs));
        assert!(!uri_matches_path("file:///d:/repo/src/Other.py", abs));
    }

    #[test]
    fn parse_and_cap_diagnostics() {
        let v = json!([
            {"range": {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 7}},
             "severity": 1, "message": "boom", "source": "pyright"},
            {"range": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 1}},
             "message": "no severity"}
        ]);
        let diags = parse_diagnostics(Some(&v));
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].message, "boom");
        assert_eq!(diags[1].severity, Value::Null);
        let out = serialize_diags(&diags, "file:///x.py", Lang::Python);
        assert_eq!(out["count"], 2);
        assert_eq!(out["truncated"], false);
        assert_eq!(out["diagnostics"][0]["range"]["start"]["line"], 1);
    }

    #[test]
    fn unsupported_extension_fails_visible() {
        // lang_for_path=None 的文件走 fail-visible(不静默吞)
        assert!(lang_for_path(Path::new("a.java")).is_none());
    }

    /// 真实服务器红→绿(环境无 pyright 时跳过;有则全链计时取证)。
    #[tokio::test]
    async fn diagnostics_red_to_green_with_pyright() {
        let probe = std::process::Command::new("pyright-langserver")
            .arg("--version")
            .output();
        if probe.is_err() {
            // Windows `.cmd` shim:where 探测
            if std::process::Command::new("where")
                .arg("pyright-langserver")
                .output()
                .map(|o| !o.status.success())
                .unwrap_or(true)
            {
                eprintln!("SKIP: pyright-langserver not on PATH");
                return;
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir(&ws).unwrap();
        let bad = ws.join("bad.py");
        std::fs::write(&bad, b"def f(:\n    pass\n").unwrap();
        let t0 = Instant::now();
        let v = diagnostics_for_tool(&ws, "workspace/bad.py", None)
            .await
            .expect("diagnostics on bad file");
        let elapsed = t0.elapsed();
        // 红:至少一条 error(severity 1)
        let red = v.to_string().contains("\"severity\":1");
        assert!(red, "expected an error diagnostic, got {v}");
        // 绿:修复后零 error
        std::fs::write(&bad, b"def f():\n    pass\n").unwrap();
        let v2 = diagnostics_for_tool(&ws, "workspace/bad.py", None)
            .await
            .expect("diagnostics on fixed file");
        let green = !v2.to_string().contains("\"severity\":1");
        assert!(green, "expected no error after fix, got {v2}");
        eprintln!("red-to-green elapsed: {}ms", elapsed.as_millis());
        shutdown(Lang::Python, &ws);
    }
}
