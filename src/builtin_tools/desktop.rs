// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Deterministic desktop observation and fixed UIA action tools.
//!
//! These tools forward validated requests to the desktop bridge. They do not
//! interpret screen content, choose targets, retry operations, or decide whether
//! a user's task is complete.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Map, Value};

#[cfg(test)]
use crate::api::desktop_bridge::DesktopOutcome;
use crate::api::desktop_bridge::{
    DesktopAction, DesktopActionRequest, DesktopBridge, DesktopCompletion, DesktopJobKind,
    DesktopObserveRequest, DesktopRequest,
};
use crate::io_handler::IoResult;
use crate::io_handlers::tool_handler::{ToolFunction, ToolHandler};

const BRIDGE_UNAVAILABLE: &str = "desktop bridge unavailable";
const MAX_SET_VALUE_CHARS: usize = 4096;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObserveArgs {
    #[serde(default)]
    include_screenshot: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActArgs {
    observation_id: String,
    target_id: String,
    action: DesktopAction,
    #[serde(default)]
    value: Option<String>,
}

fn parse_observe_args(args: &Value) -> Result<DesktopObserveRequest, String> {
    let args: ObserveArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid desktop_observe args: {e}"))?;
    Ok(DesktopObserveRequest {
        include_screenshot: args.include_screenshot,
    })
}

fn parse_act_args(args: &Value) -> Result<DesktopActionRequest, String> {
    let args: ActArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid desktop_act args: {e}"))?;
    if args.observation_id.is_empty() {
        return Err("invalid desktop_act args: observation_id must not be empty".to_string());
    }
    if args.target_id.is_empty() {
        return Err("invalid desktop_act args: target_id must not be empty".to_string());
    }
    match args.action {
        DesktopAction::SetValue => {
            let value = args.value.as_deref().ok_or_else(|| {
                "invalid desktop_act args: set_value requires a string value".to_string()
            })?;
            if value.chars().count() > MAX_SET_VALUE_CHARS {
                return Err(format!(
                    "invalid desktop_act args: value exceeds {MAX_SET_VALUE_CHARS} characters"
                ));
            }
        }
        _ if args.value.is_some() => {
            return Err(
                "invalid desktop_act args: value is only accepted for set_value".to_string(),
            );
        }
        _ => {}
    }
    Ok(DesktopActionRequest {
        observation_id: args.observation_id,
        target_id: args.target_id,
        action: args.action,
        value: args.value,
    })
}

fn format_completion(completion: DesktopCompletion) -> Value {
    let mut map = match completion.result {
        Value::Object(map) => map,
        Value::Null => Map::new(),
        result => {
            let mut map = Map::new();
            map.insert("result".to_string(), result);
            map
        }
    };
    map.insert("status".to_string(), json!("ok"));
    map.insert(
        "outcome".to_string(),
        serde_json::to_value(completion.outcome).expect("desktop outcome serializes"),
    );
    map.insert("job_id".to_string(), json!(completion.job_id));
    map.insert(
        "os_error".to_string(),
        completion
            .os_error
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    if completion.kind == DesktopJobKind::Act {
        map.insert("next_observation_required".to_string(), json!(true));
    }
    Value::Object(map)
}

async fn execute(bridge: &Option<Arc<DesktopBridge>>, request: DesktopRequest) -> IoResult {
    let bridge = bridge
        .as_ref()
        .ok_or_else(|| BRIDGE_UNAVAILABLE.to_string())?;
    bridge
        .execute(request)
        .await
        .map(format_completion)
        .map_err(|e| format!("desktop bridge error {}: {}", e.code, e.message))
}

/// Foreground-window UIA observation tool.
pub struct DesktopObserveTool {
    bridge: Option<Arc<DesktopBridge>>,
}

impl DesktopObserveTool {
    /// Construct an unavailable tool that fails visibly when called.
    pub fn unwired() -> Self {
        Self { bridge: None }
    }

    /// Bind the tool to the process-wide desktop bridge.
    pub fn wired(bridge: Arc<DesktopBridge>) -> Self {
        Self {
            bridge: Some(bridge),
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for DesktopObserveTool {
    async fn call(&self, args: &Value) -> IoResult {
        let request = parse_observe_args(args)?;
        execute(&self.bridge, DesktopRequest::Observe(request)).await
    }
}

/// Fixed UIA action tool. Every valid call requires a separate manual approval.
pub struct DesktopActTool {
    bridge: Option<Arc<DesktopBridge>>,
}

impl DesktopActTool {
    /// Construct an unavailable tool that fails visibly when called.
    pub fn unwired() -> Self {
        Self { bridge: None }
    }

    /// Bind the tool to the process-wide desktop bridge.
    pub fn wired(bridge: Arc<DesktopBridge>) -> Self {
        Self {
            bridge: Some(bridge),
        }
    }
}

#[async_trait::async_trait]
impl ToolFunction for DesktopActTool {
    async fn call(&self, args: &Value) -> IoResult {
        let request = parse_act_args(args)?;
        execute(&self.bridge, DesktopRequest::Act(request)).await
    }

    fn evaluate_proposal(&self, args: &Value) -> Option<Value> {
        let request = parse_act_args(args).ok()?;
        let action = serde_json::to_value(request.action)
            .ok()?
            .as_str()?
            .to_string();
        Some(json!({
            "status": "needs_approval",
            "category": "candidate",
            "description": format!(
                "desktop {action} on target '{}' from observation '{}'",
                request.target_id, request.observation_id
            ),
            "risk": "performs one fixed UI Automation action in the foreground Windows application",
            "alternative": "perform the action manually in the foreground application",
        }))
    }
}

/// Rebind only desktop tools already present in a manifest-backed toolkit.
/// Absence is expected until the static manifest entries are assembled.
pub fn rebind_desktop_tools(toolkit: &mut ToolHandler, bridge: Arc<DesktopBridge>) {
    if toolkit.has_tool("desktop_observe") {
        toolkit.register_static(
            "desktop_observe",
            Arc::new(DesktopObserveTool::wired(bridge.clone())),
        );
    }
    if toolkit.has_tool("desktop_act") {
        toolkit.register_static("desktop_act", Arc::new(DesktopActTool::wired(bridge)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_args_are_strict_and_screenshot_defaults_off() {
        let request = parse_observe_args(&json!({})).unwrap();
        assert!(!request.include_screenshot);
        let request = parse_observe_args(&json!({"include_screenshot": true})).unwrap();
        assert!(request.include_screenshot);
        assert!(parse_observe_args(&json!({"depth": 9})).is_err());
        assert!(parse_observe_args(&json!(null)).is_err());
    }

    #[test]
    fn action_args_enforce_fixed_value_contract() {
        let base = json!({
            "observation_id": "obs-1",
            "target_id": "target-1",
            "action": "invoke",
            "value": "not allowed"
        });
        assert!(parse_act_args(&base).is_err());

        let missing = json!({
            "observation_id": "obs-1",
            "target_id": "target-1",
            "action": "set_value",
            "value": null
        });
        assert!(parse_act_args(&missing).is_err());

        let too_long = json!({
            "observation_id": "obs-1",
            "target_id": "target-1",
            "action": "set_value",
            "value": "x".repeat(MAX_SET_VALUE_CHARS + 1)
        });
        assert!(parse_act_args(&too_long).is_err());

        let valid = json!({
            "observation_id": "obs-1",
            "target_id": "target-1",
            "action": "set_value",
            "value": "raw text"
        });
        assert_eq!(
            parse_act_args(&valid).unwrap().value.as_deref(),
            Some("raw text")
        );
    }

    #[test]
    fn action_args_reject_empty_ids_extra_fields_and_unknown_actions() {
        for args in [
            json!({"observation_id": "", "target_id": "t", "action": "focus"}),
            json!({"observation_id": "o", "target_id": "", "action": "focus"}),
            json!({"observation_id": "o", "target_id": "t", "action": "click"}),
            json!({"observation_id": "o", "target_id": "t", "action": "focus", "x": 1}),
        ] {
            assert!(parse_act_args(&args).is_err(), "accepted: {args}");
        }
    }

    #[test]
    fn every_valid_action_has_manual_proposal_without_value_echo() {
        let tool = DesktopActTool::unwired();
        let proposal = tool
            .evaluate_proposal(&json!({
                "observation_id": "obs-secret-context",
                "target_id": "target-7",
                "action": "set_value",
                "value": "do-not-echo-this-value"
            }))
            .unwrap();
        assert_eq!(proposal["status"], json!("needs_approval"));
        assert_eq!(proposal["category"], json!("candidate"));
        assert!(!proposal.to_string().contains("do-not-echo-this-value"));
        assert!(tool
            .evaluate_proposal(&json!({"action": "invoke"}))
            .is_none());
    }

    #[tokio::test]
    async fn unwired_tools_fail_visibly() {
        let observe = DesktopObserveTool::unwired()
            .call(&json!({}))
            .await
            .unwrap_err();
        assert!(observe.contains(BRIDGE_UNAVAILABLE), "got: {observe}");
        let act = DesktopActTool::unwired()
            .call(&json!({
                "observation_id": "obs-1",
                "target_id": "target-1",
                "action": "invoke",
                "value": null
            }))
            .await
            .unwrap_err();
        assert!(act.contains(BRIDGE_UNAVAILABLE), "got: {act}");
    }

    #[test]
    fn completion_format_preserves_raw_fields_and_marks_action_reobservation() {
        let completion = DesktopCompletion {
            job_id: "job-1".to_string(),
            kind: DesktopJobKind::Act,
            outcome: DesktopOutcome::Unknown,
            result: json!({"raw": {"os": "evidence"}}),
            os_error: Some("deadline".to_string()),
        };
        let value = format_completion(completion);
        assert_eq!(value["status"], json!("ok"));
        assert_eq!(value["outcome"], json!("unknown"));
        assert_eq!(value["raw"]["os"], json!("evidence"));
        assert_eq!(value["next_observation_required"], json!(true));
    }

    #[test]
    fn observation_images_remain_at_the_top_level_for_runner_extraction() {
        let completion = DesktopCompletion {
            job_id: "job-1".to_string(),
            kind: DesktopJobKind::Observe,
            outcome: DesktopOutcome::Succeeded,
            result: json!({
                "observation_id": "obs-1",
                "images": [{"mime_type": "image/png", "data_base64": "raw"}]
            }),
            os_error: None,
        };
        let value = format_completion(completion);
        assert_eq!(value["images"][0]["mime_type"], json!("image/png"));
        assert!(value.get("result").is_none());
    }

    #[test]
    fn rebind_without_registered_manifest_entries_is_a_noop() {
        let mut toolkit = ToolHandler::new();
        let bridge = Arc::new(DesktopBridge::new("test-token".to_string()).unwrap());
        rebind_desktop_tools(&mut toolkit, bridge);
        assert!(toolkit.tool_names().is_empty());
    }
}
