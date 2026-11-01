// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! MCP 拉起批准提案上行（治理原生线提案回流）
//!
//! 「第一次使用即产生提案，运营方批准即成为长期许可」：装载门判定出的未登记
//! server（enforce 拒载 / warn 迁移档放行）自动转提案写入治理数据集 Draft。
//!
//! ## fail-soft 红线（不变式）
//!
//! 提案失败**绝不影响拉起判定本身**——提案在独立线程 fire-and-forget，判定
//! 路径零等待零依赖；网络/鉴权/形状任何失败只留事件，不重试不阻塞。
//!
//! ## 去重（双保险）
//!
//! - 进程内：`attempted` 集，同一 server_key 本进程至多提案一次（先占位再
//!   发送，并发重试不重复提案）；
//! - 治理侧：entry_id=server_key+version 1 唯一性，重复提案收到
//!   `already_present` 回执如实留痕（跨进程/重启的重复由治理侧吸收）。
//!
//! ## 提案形态（最窄诚实）
//!
//! - 非裸解释器：仅 command 级（与装载门自身语义一致——固定二进制的身份即
//!   命令本身，args 是其业务参数）；
//! - 裸解释器：逐字复制本次观测参数段（锁死本次观测，最窄提案；运营方可
//!   编辑草稿放宽为通配）；
//! - 裸解释器且无参数 = 无法构造合规批准条目，跳过并留痕；
//! - 提案不含批准归因（归因在人工批准时按审批者身份补全——提案是请求，
//!   不是批准）。
//!
//! ## 配置
//!
//! `EVORULE_MCP_GOV_PROPOSE_KEY`（entries:propose scope；缺省未配置=提案关闭，
//! 零开销）；服务地址与数据集复用拉取源同款 env（`EVORULE_MCP_GOV_URL`/
//! `EVORULE_MCP_GOV_DATASET`）。

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use super::spawn_gate::{
    canonical_server_key, is_bare_interpreter, validate_approval_entry, MCP_SERVERS_DATASET_ID,
};

/// 提案通路配置（URL+PROPOSE_KEY 齐全才启用）
struct ProposalConfig {
    base_url: String,
    propose_key: String,
    dataset_id: String,
}

fn proposal_config_from_env() -> Option<ProposalConfig> {
    let base_url = std::env::var("EVORULE_MCP_GOV_URL").unwrap_or_default();
    let propose_key = std::env::var("EVORULE_MCP_GOV_PROPOSE_KEY").unwrap_or_default();
    if base_url.trim().is_empty() || propose_key.trim().is_empty() {
        return None;
    }
    let dataset_id = std::env::var("EVORULE_MCP_GOV_DATASET")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| MCP_SERVERS_DATASET_ID.to_string());
    Some(ProposalConfig {
        base_url,
        propose_key,
        dataset_id,
    })
}

/// 进程内去重集（server_key 一次一提）
fn attempted() -> &'static Mutex<HashSet<String>> {
    static ATTEMPTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ATTEMPTED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 装载门钩子入口：未登记 server 观测 → 治理提案（fire-and-forget）。
/// `context` = 判定语境（enforce 拒载 / warn 迁移档放行），入提案事由供审批面回放。
pub(crate) fn maybe_propose(server_name: &str, command: &str, args: &[String], context: &str) {
    let Some(cfg) = proposal_config_from_env() else {
        return;
    };
    // 提案载荷构造（最窄诚实，见模块文档）
    let args_pattern: Option<Vec<String>> = if is_bare_interpreter(command) {
        if args.is_empty() {
            tracing::debug!(
                target: "mcp_spawn_gate",
                server = %server_name,
                command = %command,
                "MCP 提案跳过：裸解释器无参数无法构造合规批准条目"
            );
            return;
        }
        Some(args.to_vec())
    } else {
        None
    };
    if let Err(reason) = validate_approval_entry(command, args_pattern.as_deref()) {
        tracing::debug!(
            target: "mcp_spawn_gate",
            server = %server_name,
            command = %command,
            reason = %reason,
            "MCP 提案跳过：观测不满足批准条目形状规则"
        );
        return;
    }
    let server_key = canonical_server_key(command, args_pattern.as_deref());
    // 进程内去重：先占位再发送
    let first = {
        let mut set = attempted().lock().unwrap();
        set.insert(server_key.clone())
    };
    if !first {
        tracing::debug!(
            target: "mcp_spawn_gate",
            server = %server_name,
            "MCP 提案跳过：本进程已提案在案（server_key 去重）"
        );
        return;
    }
    let server_name_owned = server_name.to_string();
    let command = command.to_string();
    let cause = format!("未登记 server 自动提案（{context}）");
    let spawned = std::thread::Builder::new()
        .name("mcp-gov-proposal".into())
        .spawn(move || {
            send_proposal(
                &cfg,
                &server_name_owned,
                &command,
                args_pattern,
                &server_key,
                &cause,
            )
        });
    if spawned.is_err() {
        tracing::warn!(
            target: "mcp_spawn_gate",
            server = %server_name,
            "MCP 提案线程启动失败（fail-soft：不影响判定，后续观测不再重试本进程提案）"
        );
    }
}

/// 提案发送（独立线程；任何失败只留事件）
fn send_proposal(
    cfg: &ProposalConfig,
    server_name: &str,
    command: &str,
    args_pattern: Option<Vec<String>>,
    server_key: &str,
    cause: &str,
) {
    let url = format!(
        "{}/v1/invoke/propose_mcp_server",
        cfg.base_url.trim_end_matches('/')
    );
    let payload = serde_json::json!({
        "dataset_id": cfg.dataset_id,
        "command": command,
        "args_pattern": args_pattern,
        "server_key": server_key,
        "source_session_id": server_name,
        "cause": cause,
    });
    let client = match reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "mcp_spawn_gate", server = %server_name, error = %e, "MCP 提案失败：HTTP client 构建失败（fail-soft）");
            return;
        }
    };
    let result = client
        .post(&url)
        .header("X-Api-Key", &cfg.propose_key)
        .json(&payload)
        .send();
    match result {
        Ok(resp) => {
            let status = resp.status();
            if status.as_u16() == 201 {
                tracing::info!(
                    target: "mcp_spawn_gate",
                    server = %server_name,
                    command = %command,
                    server_key = %server_key,
                    "MCP 提案已入治理库（Draft 待审；批准后自动进入生效清单）"
                );
            } else if status.as_u16() == 200 {
                // already_present：治理侧去重回执（同键提案已在库，任何状态）
                tracing::info!(
                    target: "mcp_spawn_gate",
                    server = %server_name,
                    command = %command,
                    server_key = %server_key,
                    "MCP 提案在案（治理侧去重：同键提案已存在，不再重复）"
                );
            } else {
                let body = resp.text().unwrap_or_default();
                tracing::warn!(
                    target: "mcp_spawn_gate",
                    server = %server_name,
                    command = %command,
                    status = %status,
                    body = %body,
                    "MCP 提案被治理服务拒绝（fail-soft，不影响判定）"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "mcp_spawn_gate",
                server = %server_name,
                command = %command,
                error = %e,
                "MCP 提案发送失败（fail-soft：治理服务不可达，不影响判定）"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::spawn_gate::{canonical_server_key, is_bare_interpreter};

    #[test]
    fn proposal_payload_shape_for_both_command_kinds() {
        // 非裸解释器：command 级提案（无 args_pattern）
        assert!(!is_bare_interpreter("my-server.exe"));
        let key = canonical_server_key("my-server.exe", None);
        assert!(key.starts_with("blake3:"));
        // 裸解释器：逐字复制观测参数段；形状闸接受
        assert!(is_bare_interpreter("NPX"));
        let observed = vec!["-y".to_string(), "@scope/srv-demo".to_string()];
        let pattern = Some(observed.clone());
        assert!(
            super::super::spawn_gate::validate_approval_entry("npx", pattern.as_deref()).is_ok()
        );
        let keyed = canonical_server_key("npx", pattern.as_deref());
        assert_ne!(key, keyed);
        // 裸解释器无参数：不可提案（调用侧跳过）
        assert!(super::super::spawn_gate::validate_approval_entry("npx", None).is_err());
    }
}
