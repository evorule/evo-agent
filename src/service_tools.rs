// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 服务代理工具 —— 服务消费桥（发现 → 注册 → 直调）
//!
//! agent 启动时经 `GET /api/services` 发现 evorule-server 插件服务，
//! 按 `config.evorule.service_tools` 白名单将每个服务注册为代理工具；
//! 工具执行经 `POST /api/services/{name}/invoke` 直调 server 服务链
//! （与 io_request 同一执行路径，无第二实现）。
//!
//! 治理语义：
//! - 白名单为空 = 不注册任何服务工具（安全默认）；
//! - `sensitive=true` 的服务跳过注册（server 侧 invoke 对敏感服务 403，
//!   敏感操作必须走会话审计链）；
//! - 描述与参数契约随注册缓存（对账清单 `description`/`parameters`），
//!   供 runner 生成带参动态工具 schema（LLM 可带参真实调用）；
//! - 服务执行失败 fail-fast 透传（无静默降级）。

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use serde_json::Value;

use crate::api::api_core::ApiError;
use crate::api::evorule_client::EvoruleApiClient;
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};

/// 服务描述注册表：注册时缓存对账清单的 description。
///
/// 供 runner 组装 tools schema 的降级分支查询（动态注册的工具无静态 spec，
/// 无此表则 schema 的 description 为空，LLM 只能凭工具名猜用途）。
static SERVICE_DESCRIPTIONS: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();

fn descriptions() -> &'static RwLock<HashMap<String, String>> {
    SERVICE_DESCRIPTIONS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 查询已注册服务工具的描述（未注册返回 None）
pub fn service_description(name: &str) -> Option<String> {
    descriptions().read().ok()?.get(name).cloned()
}

/// 服务参数契约注册表：注册时缓存对账清单的 `parameters`
/// （OpenAI function parameters 子集；插件包 plugin.json 声明 →
/// server 对账透传 → 本表缓存）。
///
/// 供 runner 动态工具 schema 生成完整参数契约（LLM 可带参调用服务）；
/// 服务未声明（native/registry 来源缺省）返回 None，schema 降级空 object。
static SERVICE_PARAMETERS: OnceLock<RwLock<HashMap<String, serde_json::Value>>> = OnceLock::new();

fn parameters() -> &'static RwLock<HashMap<String, serde_json::Value>> {
    SERVICE_PARAMETERS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 查询已注册服务工具的参数契约（未注册/服务未声明返回 None）
pub fn service_parameters(name: &str) -> Option<serde_json::Value> {
    parameters().read().ok()?.get(name).cloned()
}

/// 服务能力目录域场景表（静态策展）：前缀 → (显示名, 典型触发场景)。
/// 命中规则：最长已知前缀优先（`numeric_unit_*` 命中「单位换算」而非
/// 「数值计算」）；无命中 → 服务名首 `_` 前段作组键、无场景文案
/// （前向兼容新族，零改码）。
const SERVICE_CATALOG_DOMAINS: &[(&str, &str, &str)] = &[
    (
        "numeric",
        "数值计算",
        "高精度十进制/有理数运算、数论、金融利息与摊销、量纲求值——禁止心算",
    ),
    ("numeric_unit", "单位换算", "度量衡换算（率显式提供）"),
    ("unit", "单位换算", "度量衡换算（率显式提供）"),
    (
        "inference",
        "规则推理与量纲",
        "规则读写集/环路/冲突/前向后向链/反事实/量纲验证",
    ),
    (
        "mechanics",
        "力学",
        "牛顿第二定律、匀速运动学、动量冲量、机械能、电路欧姆/电阻、PID",
    ),
    ("physics", "物理仿真", "有界重力带仿真、总机械能"),
    ("grav", "引力相对论", "史瓦西半径、近日点进动、光线偏折"),
    ("fluid", "流体力学", "伯努利、连续性、静水压"),
    ("quantum", "量子力学", "无限深势阱、谐振子"),
    ("gnss", "卫星导航", "钟差/相对论漂移/偏心率改正/出厂预偏置"),
    ("metrology", "计量", "测量不确定度传播"),
    (
        "geometry",
        "几何校核",
        "碰撞/间隙/遮挡/等距投影/管路走线/图样审计",
    ),
    ("checksum", "校验码", "Luhn/ISBN/IBAN 校验"),
    ("gov", "治理统计", "比例分摊、聚合、时间窗（时长/重叠）"),
    ("cny", "人民币大写", "金额转中文大写"),
    ("legal", "法务期限", "期限起算与届满"),
    ("indicator", "技术指标", "SMA/EMA/MACD/RSI"),
    ("export", "文档导出", "PDF/XLSX/DOCX 生成"),
    ("template", "模板渲染", "模板填充实例化"),
    (
        "browser",
        "浏览器操作",
        "网页导航/取文/截图/表单/网络观测（交互类，非计算）",
    ),
];

/// 服务名的目录组键：最长已知前缀命中 → (前缀, Some((显示名, 场景)))；
/// 无命中 → 首 `_` 前段 + None。
fn catalog_group_of(name: &str) -> (&str, Option<(&'static str, &'static str)>) {
    let mut best: Option<(&'static str, &'static str, &'static str)> = None;
    for (prefix, display, scenario) in SERVICE_CATALOG_DOMAINS {
        let hit = name == *prefix
            || (name.len() > prefix.len()
                && name.starts_with(prefix)
                && name.as_bytes()[prefix.len()] == b'_');
        if hit && best.map_or(true, |(bp, _, _)| prefix.len() > bp.len()) {
            best = Some((prefix, display, scenario));
        }
    }
    match best {
        Some((p, d, s)) => (p, Some((d, s))),
        None => (name.split('_').next().unwrap_or(name), None),
    }
}

/// 确定性纯渲染：紧凑两段制（域行 name-only + 成本纪律尾注）。
/// 三级全排序（组间组键序 + 组内服务名序，BTreeMap/字节序）⇒ 同输入
/// 逐字节同输出，跨轮前缀缓存成立。
fn render_service_catalog_from(names: &[&str]) -> Option<String> {
    use std::collections::BTreeMap;

    if names.is_empty() {
        return None;
    }
    let mut groups: BTreeMap<&str, (Option<(&'static str, &'static str)>, Vec<&str>)> =
        BTreeMap::new();
    for name in names {
        let (key, meta) = catalog_group_of(name);
        groups.entry(key).or_insert((meta, Vec::new())).1.push(name);
    }
    let mut out = format!(
        "## 服务能力目录（确定性计算与操作面，共 {} 件 / {} 域）\n",
        names.len(),
        groups.len()
    );
    out.push_str(
        "数值/公式/量纲/单位换算/期限/校验类问题：先查下目录调服务，不要心算，不要手算近似值。\n",
    );
    for (key, (meta, mut services)) in groups {
        services.sort_unstable();
        let list = services.join(", ");
        match meta {
            Some((display, scenario)) => out.push_str(&format!(
                "- {display}（{key}_*，{} 件，{scenario}）：{list}\n",
                services.len()
            )),
            None => out.push_str(&format!("- {key}（{} 件）：{list}\n", services.len())),
        }
    }
    out.push_str("## 成本纪律\n");
    // 外判禁令（外判立法随批）：与 server 侧输出门禁
    // deterministic_compute 域特征表配套——prompt 侧预防 + 运行时侧检测。
    out.push_str("- 外判禁令：数值/公式/量纲/日期时长类问题禁止心算，必须走确定性服务。\n");
    out.push_str("- 便宜信息源优先：目录/描述/本地计算可答的，不调外部服务；同类需求先小后大。\n");
    out.push_str("- 大结果守则：工具返回超限会被自动引用化（落盘+指纹+分页取回 tool_result_fetch）；预判大结果时直接带 offset/max_chars 分页取用，勿一次性全量拉取。");
    Some(out)
}

/// 渲染服务能力目录段（system_prompt 常驻能力锚 + 成本纪律尾注）。
///
/// 数据源 = 服务描述注册表（启动期注册面写定后只读）：只列实际注册可调用
/// 的服务（sensitive/缺失/重名服务已被注册面过滤，天然不入目录）。内容只取
/// 服务名——描述不重复进目录（tools array 已带完整 function schema 与描述）。
/// 空注册表（零服务部署/注册失败）→ None → 配方槽位静默跳过（fail-soft，
/// 目录缺失不阻塞会话）。
pub fn render_service_catalog() -> Option<String> {
    let map = descriptions().read().ok()?;
    if map.is_empty() {
        return None;
    }
    let names: Vec<&str> = map.keys().map(String::as_str).collect();
    render_service_catalog_from(&names)
}

/// 服务代理工具：`call` = `POST /api/services/{name}/invoke`(args 原样透传)
struct ServiceProxyTool {
    ev: EvoruleApiClient,
    service_name: String,
}

/// 服务发现瞬态重试退避表（毫秒）：覆盖 sidecar 启动竞态——agent 先于
/// server 完成监听起跑时，首次 GET /api/services 连接失败属就绪期瞬态，
/// 有限退避重试（总等待 ≤3.5s）后即达稳态；耗尽仍失败 = 真降级，走
/// 注册失败留痕路径。仅对连接/超时形态重试；HTTP 状态错误（401/403/
/// 500）= server 在但拒绝，重试无益，立即失败。
const DISCOVERY_RETRY_DELAYS_MS: [u64; 3] = [500, 1000, 2000];

/// GET /api/services 就绪感知发现：瞬态失败按退避表重试，其余即时失败。
async fn list_services_with_readiness_retry(
    ev: &EvoruleApiClient,
    delays_ms: &[u64],
) -> Result<Value, String> {
    let mut attempt = 0usize;
    loop {
        match ev.list_services().await {
            Ok(v) => return Ok(v),
            Err(ApiError::HttpError(e)) if e.is_connect() || e.is_timeout() => {
                let Some(delay) = delays_ms.get(attempt) else {
                    return Err(format!(
                        "服务发现失败（GET /api/services）: {e}（重试 {} 次后仍连接失败）",
                        delays_ms.len()
                    ));
                };
                attempt += 1;
                eprintln!(
                    "[service-tools] server 未就绪（连接失败），{delay}ms 后重试（{attempt}/{}）",
                    delays_ms.len()
                );
                tokio::time::sleep(std::time::Duration::from_millis(*delay)).await;
            }
            Err(e) => {
                return Err(format!("服务发现失败（GET /api/services）: {e}"));
            }
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for ServiceProxyTool {
    async fn call(&self, args: &Value) -> IoResult {
        let serde_args = args.clone();
        self.ev
            .invoke_service(&self.service_name, &serde_args)
            .await
            .map_err(|e| format!("service {} invoke failed: {e}", self.service_name))
    }
}

/// 服务发现 → 白名单过滤 → 注册代理工具。
///
/// - 白名单为空：直接返回 0（不发起任何网络请求）；
/// - 服务发现失败：返回 Err（fail-fast，调用方决定是否致命）；
/// - `sensitive=true` 的服务跳过（invoke 403 守卫，注册了也是坏工具面）；
/// - 白名单中不存在的服务名：跳过并 warn（配置拼写错误可见）；
/// - 与既有本地工具重名的服务：跳过（本地工具优先，防覆盖）。
///
/// 返回实际注册的工具数量。
pub async fn register_service_tools(
    handler: &mut ToolHandler,
    ev: &EvoruleApiClient,
    whitelist: &[String],
) -> Result<usize, String> {
    register_service_tools_with_delays(handler, ev, whitelist, &DISCOVERY_RETRY_DELAYS_MS).await
}

/// 注册主体（发现步重试退避表参数化：测试注入空表验证即时 fail-fast）。
async fn register_service_tools_with_delays(
    handler: &mut ToolHandler,
    ev: &EvoruleApiClient,
    whitelist: &[String],
    discovery_retry_delays_ms: &[u64],
) -> Result<usize, String> {
    if whitelist.is_empty() {
        return Ok(0);
    }
    let services = list_services_with_readiness_retry(ev, discovery_retry_delays_ms).await?;

    let mut registered = 0usize;
    // 启动点名探针：缺失/sensitive/重名逐类收集，循环后输出结构化汇总，
    // 供装机面 fresh-install 点名与 CI 冒烟断言（B-051）
    let mut missing: Vec<&str> = Vec::new();
    let mut skipped_sensitive: Vec<&str> = Vec::new();
    let mut skipped_duplicate: Vec<&str> = Vec::new();
    for want in whitelist {
        let Some(info) = services.as_array().and_then(|arr| {
            arr.iter()
                .find(|s| s["name"].as_str() == Some(want.as_str()))
        }) else {
            eprintln!(
                "[service-tools] 跳过白名单项 '{want}'：不在服务对账清单（GET /api/services）"
            );
            missing.push(want);
            continue;
        };
        if info["sensitive"].as_bool() == Some(true) {
            eprintln!(
                "[service-tools] 跳过服务 '{want}'：sensitive 服务禁止直调（须走会话审计链）"
            );
            skipped_sensitive.push(want);
            continue;
        }
        if handler.has_tool(want) {
            eprintln!("[service-tools] 跳过服务 '{want}'：与本地工具重名（本地工具优先）");
            skipped_duplicate.push(want);
            continue;
        }
        let description = info["description"].as_str().unwrap_or("").to_string();
        let parameters_decl = info
            .get("parameters")
            .filter(|p| !p.is_null())
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "type": "object", "properties": {} }));
        // 一表硬规则 3:动态源注册期产出 Inline 契约——丢字段(空描述/参数
        // 非 object)即注册失败(设计档 §3.1,消灭服务代理空 schema 降级)。
        // 校验先行，失败时不残留描述/参数缓存。
        let manifest = crate::agent::tool_manifest::dynamic_manifest(
            want,
            crate::agent::tool_manifest::ToolSource::ServiceProxy,
            description,
            parameters_decl,
        )?;
        descriptions()
            .write()
            .map(|mut m| {
                m.insert(
                    want.clone(),
                    info["description"].as_str().unwrap_or("").to_string(),
                );
            })
            .map_err(|_| "服务描述注册表写入失败".to_string())?;
        if let Some(params) = info.get("parameters").filter(|p| !p.is_null()) {
            parameters()
                .write()
                .map(|mut m| {
                    m.insert(want.clone(), params.clone());
                })
                .map_err(|_| "服务参数契约注册表写入失败".to_string())?;
        }
        handler.register(
            manifest,
            Arc::new(ServiceProxyTool {
                ev: ev.clone(),
                service_name: want.clone(),
            }),
        );
        registered += 1;
    }
    // 结构化点名摘要：无论绿红都输出一行，fresh-install/CI 可直接断言
    // 「注册 N/M」；缺失与跳过分类点名，装机面缺失即肉眼可见（消灭静默缺席）
    let mut summary = format!(
        "[service-tools] 点名：注册 {registered}/{}",
        whitelist.len()
    );
    if !missing.is_empty() {
        summary.push_str(&format!(
            "，缺失 {}（{}）",
            missing.len(),
            missing.join(", ")
        ));
    }
    if !skipped_sensitive.is_empty() {
        summary.push_str(&format!(
            "，sensitive 跳过 {}（{}）",
            skipped_sensitive.len(),
            skipped_sensitive.join(", ")
        ));
    }
    if !skipped_duplicate.is_empty() {
        summary.push_str(&format!(
            "，重名跳过 {}（{}）",
            skipped_duplicate.len(),
            skipped_duplicate.join(", ")
        ));
    }
    eprintln!("{summary}");
    Ok(registered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_handlers::tool_handler::ToolHandler;

    #[test]
    fn empty_whitelist_registers_nothing() {
        // 安全默认：空白名单零注册零网络请求
        let mut handler = ToolHandler::new();
        let ev = EvoruleApiClient::new("http://127.0.0.1:1");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let n = rt
            .block_on(register_service_tools(&mut handler, &ev, &[]))
            .unwrap();
        assert_eq!(n, 0);
        assert!(handler.tool_names().is_empty());
    }

    #[test]
    fn discovery_failure_is_fail_fast() {
        // 不可达 base_url → 服务发现显式失败（不静默吞掉）；注入空退避表
        // 验证重试耗尽后即时失败（缺省退避表的耗尽语义由就绪重试用例覆盖）
        let mut handler = ToolHandler::new();
        let ev = EvoruleApiClient::new("http://127.0.0.1:1");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(register_service_tools_with_delays(
                &mut handler,
                &ev,
                &["finance_config_get".to_string()],
                &[],
            ))
            .unwrap_err();
        assert!(err.contains("服务发现失败"), "应 fail-fast: {err}");
    }

    // —— 进程内顺序应答式 HTTP 假服务（仅替代远端 server 的传输层）——

    struct CapturedRequest {
        path: String,
        body: String,
    }

    /// 按顺序对每个请求回一个 (status, body)；响应带 Connection: close 强制
    /// 逐请求新建连接；捕获 (path, body) 供请求形状断言。
    fn spawn_http_fixture(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, Arc<std::sync::Mutex<Vec<CapturedRequest>>>) {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        std::thread::spawn(move || {
            accept_and_respond(listener, responses, &captured_clone);
        });
        (format!("http://{addr}"), captured)
    }

    /// 延迟监听 fixture：复现 sidecar 启动竞态——客户端先起跑连接（监听
    /// 不在场 → 连接拒绝），`delay_ms` 后监听就位。返回 base_url。
    fn spawn_delayed_http_fixture(delay_ms: u64, responses: Vec<(u16, &'static str)>) -> String {
        use std::net::TcpListener;

        // 先占坑得确定空闲端口再释放（重 bind 同端口模拟「尚未监听」窗口）
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            let listener =
                TcpListener::bind(addr).expect("delayed fixture re-bind failed (port taken over?)");
            accept_and_respond(listener, responses, &std::sync::Mutex::new(Vec::new()));
        });
        format!("http://{addr}")
    }

    /// 顺序应答循环（监听器在手后逐请求 accept+应答+捕获）
    fn accept_and_respond(
        listener: std::net::TcpListener,
        responses: Vec<(u16, &'static str)>,
        captured: &std::sync::Mutex<Vec<CapturedRequest>>,
    ) {
        use std::io::{BufRead, BufReader, Read, Write};
        for (status, body) in responses {
            let (stream, _) = listener.accept().unwrap();
            let mut stream = stream;
            let path;
            let mut req_body = Vec::new();
            {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                path = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let mut content_length = 0usize;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                if content_length > 0 {
                    req_body = vec![0u8; content_length];
                    reader.read_exact(&mut req_body).unwrap();
                }
            }
            captured.lock().unwrap().push(CapturedRequest {
                path,
                body: String::from_utf8_lossy(&req_body).into_owned(),
            });
            let reason = match status {
                200 => "OK",
                _ => "Internal Server Error",
            };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).unwrap();
            stream.flush().unwrap();
        }
    }

    #[test]
    fn discovery_readiness_retry_succeeds_after_delayed_listen() {
        // 启动竞态复现：server 延迟 300ms 才监听，首次 GET 连接失败（瞬态），
        // 就绪等待重试后注册成功（缺省退避表 500ms 首试即落在就绪窗口内）
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let base = spawn_delayed_http_fixture(
            300,
            vec![(
                200,
                r#"[{"name":"late_svc","source":"native","sensitive":false,"description":"延迟就绪服务"}]"#,
            )],
        );
        let mut handler = ToolHandler::new();
        let ev = EvoruleApiClient::new(&base);
        let n = rt
            .block_on(register_service_tools(
                &mut handler,
                &ev,
                &["late_svc".to_string()],
            ))
            .unwrap();
        assert_eq!(n, 1, "就绪等待后应注册成功");
        assert!(handler.has_tool("late_svc"), "服务工具应在场");
    }

    #[test]
    fn proxy_tool_invoke_request_shape_and_result() {
        // 直调契约：POST /api/services/{name}/invoke，body = args 原样透传，
        // 响应 JSON 原样返回；描述从对账清单进注册表供 schema 透出。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (base, captured) = spawn_http_fixture(vec![
            (
                200,
                r#"[{"name":"shape_svc","source":"native","description":"形状测试服务","sensitive":false}]"#,
            ),
            (200, r#"{"result":"ok"}"#),
        ]);
        let mut handler = ToolHandler::new();
        let ev = EvoruleApiClient::new(&base);
        let n = rt
            .block_on(register_service_tools(
                &mut handler,
                &ev,
                &["shape_svc".to_string()],
            ))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            service_description("shape_svc").as_deref(),
            Some("形状测试服务")
        );

        let out = rt
            .block_on(handler.execute_by_name("shape_svc", &serde_json::json!({"key": "demo"})))
            .unwrap();
        assert_eq!(out.clone(), serde_json::json!({"result": "ok"}));

        let cap = captured.lock().unwrap();
        assert_eq!(cap[0].path, "/api/services", "第一次请求应为服务发现");
        assert_eq!(
            cap[1].path, "/api/services/shape_svc/invoke",
            "第二次请求应为服务直调"
        );
        assert_eq!(cap[1].body, r#"{"key":"demo"}"#, "args 应原样透传为 body");
    }

    #[test]
    fn registration_caches_parameters_contract() {
        // 参数契约随注册缓存（消费端链路：对账 parameters → 注册表
        // → runner 动态 schema）；服务未声明参数时如实返回 None（不伪造契约）。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (base, _captured) = spawn_http_fixture(vec![(
            200,
            r#"[{"name":"params_svc","source":"plugin","sensitive":false,"description":"带参服务","parameters":{"type":"object","properties":{"key":{"type":"string","description":"配置键"}},"required":["key"]}},{"name":"nocontract_svc","source":"registry","sensitive":false,"description":"无参数契约的服务"}]"#,
        )]);
        let mut handler = ToolHandler::new();
        let ev = EvoruleApiClient::new(&base);
        let n = rt
            .block_on(register_service_tools(
                &mut handler,
                &ev,
                &["params_svc".to_string(), "nocontract_svc".to_string()],
            ))
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            service_parameters("params_svc"),
            Some(serde_json::json!({
                "type": "object",
                "properties": { "key": { "type": "string", "description": "配置键" } },
                "required": ["key"]
            }))
        );
        assert!(
            service_parameters("nocontract_svc").is_none(),
            "未声明参数的服务不应有伪造契约"
        );
    }

    #[test]
    fn proxy_tool_error_is_fail_fast_passthrough() {
        // 服务执行失败（HTTP 500）必须显式报错透传，不静默吞掉。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (base, _captured) = spawn_http_fixture(vec![
            (
                200,
                r#"[{"name":"err_svc","source":"native","sensitive":false,"description":"执行即报错的服务"}]"#,
            ),
            (500, r#"{"error":"boom"}"#),
        ]);
        let mut handler = ToolHandler::new();
        let ev = EvoruleApiClient::new(&base);
        let n = rt
            .block_on(register_service_tools(
                &mut handler,
                &ev,
                &["err_svc".to_string()],
            ))
            .unwrap();
        assert_eq!(n, 1);

        let err = rt
            .block_on(handler.execute_by_name("err_svc", &serde_json::json!({})))
            .unwrap_err();
        assert!(err.contains("500"), "应透传 HTTP 状态码: {err}");
        assert!(
            err.contains("service err_svc invoke failed"),
            "应带服务名上下文: {err}"
        );
    }

    #[test]
    fn registration_rejects_empty_description() {
        // 一表硬规则 3：服务对账清单缺 description = 注册失败（fail-fast，
        // 消灭空 schema 降级——行为变更点，原实现静默注册空描述工具）。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (base, _captured) = spawn_http_fixture(vec![(
            200,
            r#"[{"name":"nodesc_svc","source":"native","sensitive":false}]"#,
        )]);
        let mut handler = ToolHandler::new();
        let ev = EvoruleApiClient::new(&base);
        let err = rt
            .block_on(register_service_tools(
                &mut handler,
                &ev,
                &["nodesc_svc".to_string()],
            ))
            .unwrap_err();
        assert!(err.contains("empty description"), "应报空描述拒绝: {err}");
        assert!(!handler.has_tool("nodesc_svc"), "被拒服务不应注册为工具");
    }

    // —— 服务能力目录渲染（纯函数面，与全局注册表无关）——

    #[test]
    fn service_catalog_groups_sorted_and_known_domains_annotated() {
        let out = render_service_catalog_from(&[
            "checksum_luhn",
            "numeric_decimal_eval",
            "numeric_unit_convert",
            "numeric_fin_interest",
            "browser_navigate",
            "zzdemo_echo",
        ])
        .unwrap();
        // 头两行：件数/域数与先查目录指引
        assert!(
            out.starts_with("## 服务能力目录（确定性计算与操作面，共 6 件 / 5 域）\n"),
            "目录头形态不符: {out}"
        );
        // 组间字典序：browser < checksum < numeric < numeric_unit < zzdemo
        let b = out.find("- 浏览器操作").unwrap();
        let c = out.find("- 校验码").unwrap();
        let n = out.find("- 数值计算").unwrap();
        let u = out.find("- 单位换算").unwrap();
        let z = out.find("- zzdemo").unwrap();
        assert!(b < c && c < n && n < u && u < z, "组间应按组键字典序");
        // 最长已知前缀命中：numeric_unit_* 归「单位换算」而非「数值计算」
        assert!(out.contains("- 数值计算（numeric_*，2 件，高精度"));
        assert!(out.contains("- 单位换算（numeric_unit_*，1 件，度量衡换算"));
        // 未知前缀回落：组键原样、无场景文案
        assert!(out.contains("- zzdemo（1 件）：zzdemo_echo"));
        // 组内字典序
        let n_row = &out[n..u];
        assert!(
            n_row.find("numeric_decimal_eval").unwrap()
                < n_row.find("numeric_fin_interest").unwrap()
        );
        // 尾注成本纪律段（首条=外判禁令，外判立法 v0.8.0 随批）
        assert!(out.contains("## 成本纪律\n"));
        assert!(
            out.contains("- 外判禁令：数值/公式/量纲/日期时长类问题禁止心算，必须走确定性服务。\n"),
            "成本纪律段必须含外判禁令首条: {out}"
        );
        assert!(out.ends_with("勿一次性全量拉取。"));
    }

    #[test]
    fn service_catalog_byte_stability_two_calls_identical() {
        let names = ["gov_ratio", "gov_time_window", "indicator_sma", "qqx_y"];
        let a = render_service_catalog_from(&names).unwrap();
        let b = render_service_catalog_from(&names).unwrap();
        assert_eq!(a, b, "同输入必须逐字节同输出（跨轮前缀缓存前提）");
    }

    #[test]
    fn service_catalog_empty_names_is_none() {
        assert!(render_service_catalog_from(&[]).is_none());
    }

    #[test]
    fn service_catalog_global_registry_path_renders() {
        // 全局面：注册表非空即渲染且含注册服务名；两次调用逐字节一致
        // （每轮重渲染稳定性）。空表→None 语义由纯函数空输入用例覆盖
        // （全局表为进程级共享态，此处只验在场路径）。
        descriptions()
            .write()
            .unwrap()
            .insert("zzcat_only".to_string(), "目录渲染占位服务".to_string());
        let out = render_service_catalog().expect("注册表非空应渲染");
        assert!(out.contains("## 服务能力目录"));
        assert!(out.contains("zzcat_only"));
        assert_eq!(out, render_service_catalog().unwrap());
    }
}
