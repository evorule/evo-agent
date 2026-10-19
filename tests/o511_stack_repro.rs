// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 复现探针:delegate 子代理栈溢出(长 task 文本场景)
//!
//! 复现口径:delegate 携带约 3KB 长 task(含嵌套代码块/XML 示例)下探 researcher
//! 子代理,进程 `thread 'main' has overflowed its stack`(0xC00000FD)。
//!
//! 探针设计:evorule client 指向死端口(18999)——create_session 必败,若探针进程
//! 栈溢出,则崩点锁定在 create_session 之前的同步构造路径(定义装载/runner 构造/
//! toolkit 过滤/能力边界/组装执行器);若探针正常返回 Err,则崩点在会话创建之后的
//! LLM 流式面,需另行起服复现。

use evo_agent::agent::definition::AgentDefinitionManager;
use evo_agent::agent::delegate::DelegateContext;
use evo_agent::api::evorule_client::EvoruleApiClient;

/// 实录等价 task 文本(约 3KB,含嵌套代码块/XML 标签/JSON 示例)
const LONG_TASK: &str = r#"Read the file rules/22_djbh_rules.json (it's a JSON rule_set document with a top-level "transform" array of 4 branch entries).

Then call the tool `inference_rw_extract` with EXACTLY these parameters:
- rules: a JSON array of 4 elements, where each element is ONE branch entry from the file's "transform" array (a JSON object with "type":"branch" and "params"). Pass them VERBATIM from the file - do not modify, do not wrap, just the 4 branch objects in their original order.
- sources: a JSON array of 4 strings, in order: ["core_eval", "rules/22_djbh_rules.json", "rules/22_djbh_rules.json", "rules/22_djbh_rules.json"]

The tool parameters in this environment are passed via XML-style function call tags. To avoid the XML wrapper mangling nested arrays of objects, the safest path is:

1. Use shell_exec to write a small JSON file containing the 4 branch objects (read the file with cat, then extract just the transform array with a one-liner, e.g. `cat rules/22_djbh_rules.json | python3 -c "import json,sys; d=json.load(sys.stdin); print(json.dumps(d['transform']))"` - but note python3 is BLOCKED in shell, so use a different approach).

2. Instead, use shell_exec with `cat rules/22_djbh_rules.json` to dump the file, then carefully construct the rules array in your next call by hand from the 4 branch objects you can see in the output. The rules parameter MUST be passed as a proper JSON string per the tool's JSON-Schema signature, and the system will pass it through as-is.

3. The critical issue: when you invoke `inference_rw_extract`, the `rules` parameter is declared as a JSON array. In the function-call XML format, nested arrays of complex objects tend to get re-interpreted. The way to make it work is: pass `rules` as a SINGLE string containing a JSON-encoded array, OR find another way to serialize it.

Actually, look at the function-call schema. The parameter `rules` is described as: "items: {type: object}, type: array". So the system expects an actual JSON array. The XML wrapper should serialize nested items. But the previous attempt (a 4-element list of branches) became 7 items, suggesting the wrapper expanded nested children.

Try this approach: serialize the entire `rules` and `sources` payloads as JSON strings INSIDE the function call, by passing them as strings - but the schema says array. So we cannot pass as string.

Alternative: use `shell_exec` to issue the HTTP call directly. The inference service is on a local port. Try `curl http://localhost:PORT/inference/rw_extract -d @-` - but curl is blocked.

Alternative: write a small wrapper. Use the workspace to write a JSON file with the request body, then... there's no `http_post` tool.

The CORRECT approach: each top-level child of the `rules` array in the XML function-call tag becomes one element of the array. So you must put EXACTLY 4 `<item>...</item>` children under `<rules>`. But the content of each `<item>` is a JSON OBJECT (not an array), and that object must be passed as a single stringified JSON value? Or as nested XML tags?

Look at how it worked: in the previous attempt, a `<rules>` tag with 4 `<item>` children, where each item had nested XML tags for the branch object, produced "rules(7)" - meaning 7 elements. So nested items inside an `<item>` are being flattened into top-level elements. This means the XML wrapper is treating every `<item>` at any depth as a top-level array element.

CONCLUSION: To pass complex nested objects, you must put the JSON-encoded object as the TEXT CONTENT of a single `<item>` tag. The system should then parse that text as the array element value.

Try:
```xml
<rules>
  <item>{ "type":"branch", "params":{"condition":"a","then":"b"} }</item>
  <item>{ "type":"branch", "params":{"condition":"c","then":"d"} }</item>
  <item>{ "type":"branch", "params":{"condition":"e","then":"f"} }</item>
  <item>{ "type":"branch", "params":{"condition":"g","then":"h"} }</item>
</rules>
```

Each item's text is a JSON object as a string. If the system accepts that and parses the strings, we get 4 elements. If it complains about the strings not being objects, we'll need another approach.

Task:
1. Read rules/22_djbh_rules.json (use file_read tool).
2. Construct the 4-branch JSON array and 4-source JSON array.
3. Call inference_rw_extract with rules/sources as 4-item arrays of JSON-stringified objects (test the approach above).
4. If that fails, report the error verbatim.
5. If it succeeds, report the FULL response verbatim, especially the `status` and `determinism` fields.
6. If neither approach works in your environment, report clearly what was tried and what happened, so the parent session can decide next steps."#;

/// 死端口探针:崩点若在 create_session 之前,本测试进程栈溢出死亡(失败即复现);
/// 若正常返回 Err(网络失败),则崩点在会话创建之后的 LLM 流式面。
#[tokio::test]
async fn o511_dead_port_probe_no_stack_overflow() {
    let definitions = AgentDefinitionManager::new(std::path::PathBuf::from("agents"));
    let client = EvoruleApiClient::new("http://127.0.0.1:18999");
    let ctx = DelegateContext::new("general", definitions, client);
    let result = ctx.delegate("researcher", LONG_TASK).await;
    assert!(
        result.is_err(),
        "死端口下 delegate 应返回 Err;Ok 意味着连接了意外服务"
    );
    let err = result.unwrap_err();
    eprintln!(
        "delegate returned Err as expected (len={}): {}",
        err.len(),
        &err[..err.len().min(200)]
    );
}

/// 短 task 对照组:同构造路径短文本应一直绿(隔离 task 长度变量)
#[tokio::test]
async fn o511_short_task_control_no_stack_overflow() {
    let definitions = AgentDefinitionManager::new(std::path::PathBuf::from("agents"));
    let client = EvoruleApiClient::new("http://127.0.0.1:18999");
    let ctx = DelegateContext::new("general", definitions, client);
    let result = ctx.delegate("researcher", "read the file and report").await;
    assert!(result.is_err());
}
