// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! Windows deterministic desktop worker bridge.
//!
//! The desktop shell polls this loopback-only surface; serve never connects back.
//! A dedicated token protects these routes and grants no general workbench API
//! access. There is at most one queued or dispatched job in the process.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio::sync::{oneshot, Notify};
use tokio::time::{timeout, timeout_at, Instant};

pub(crate) const DESKTOP_PROTOCOL_VERSION: u32 = 1;
pub(crate) const DESKTOP_WORKER_TOKEN_ENV: &str = "EVO_AGENT_DESKTOP__WORKER_TOKEN";
const LONG_POLL: Duration = Duration::from_secs(20);
const OBSERVE_DEADLINE: Duration = Duration::from_secs(8);
const ACT_DEADLINE: Duration = Duration::from_secs(12);
const MAX_WORKER_EPOCH_CHARS: usize = 128;
const MAX_JOB_ID_CHARS: usize = 128;
const TERMINAL_HISTORY: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DesktopJobKind {
    Observe,
    Act,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DesktopOutcome {
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DesktopAction {
    Focus,
    Invoke,
    Toggle,
    Select,
    SetValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DesktopObserveRequest {
    #[serde(default)]
    pub include_screenshot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DesktopActionRequest {
    pub observation_id: String,
    pub target_id: String,
    pub action: DesktopAction,
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DesktopRequest {
    Observe(DesktopObserveRequest),
    Act(DesktopActionRequest),
}

impl DesktopRequest {
    fn kind(&self) -> DesktopJobKind {
        match self {
            Self::Observe(_) => DesktopJobKind::Observe,
            Self::Act(_) => DesktopJobKind::Act,
        }
    }

    fn deadline(&self) -> Duration {
        match self {
            Self::Observe(_) => OBSERVE_DEADLINE,
            Self::Act(_) => ACT_DEADLINE,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DesktopJob {
    pub protocol_version: u32,
    pub job_id: String,
    pub worker_epoch: String,
    pub kind: DesktopJobKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe: Option<DesktopObserveRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<DesktopActionRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DesktopWaitRequest {
    pub protocol_version: u32,
    pub worker_epoch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DesktopWorkerCompletion {
    pub protocol_version: u32,
    pub worker_epoch: String,
    pub outcome: DesktopOutcome,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub os_error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct DesktopCompletion {
    pub job_id: String,
    pub kind: DesktopJobKind,
    pub outcome: DesktopOutcome,
    pub result: Value,
    pub os_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DesktopBridgeError {
    pub code: &'static str,
    pub message: &'static str,
}

impl std::fmt::Display for DesktopBridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for DesktopBridgeError {}

/// Process-wide deterministic desktop job broker with a redacted debug form.
#[derive(Clone)]
pub struct DesktopBridge {
    inner: Arc<BridgeInner>,
}

impl std::fmt::Debug for DesktopBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopBridge").finish_non_exhaustive()
    }
}

struct BridgeInner {
    token: Arc<str>,
    state: Mutex<BridgeState>,
    notify: Notify,
}

#[derive(Default)]
struct BridgeState {
    current: Option<JobSlot>,
    active_worker_epoch: Option<String>,
    requires_observation: bool,
    next_job_id: u64,
    terminal: HashMap<String, TerminalJob>,
    terminal_order: VecDeque<String>,
}

struct JobSlot {
    job: DesktopJob,
    stage: JobStage,
    tx: Option<oneshot::Sender<DesktopCompletion>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobStage {
    Queued,
    Dispatched,
}

struct TerminalJob {
    worker_epoch: String,
    outcome: DesktopOutcome,
    accept_late_receipt: bool,
}

impl DesktopBridge {
    pub(crate) fn new(token: String) -> Result<Self, &'static str> {
        if token.is_empty() {
            return Err("desktop worker token must not be empty");
        }
        Ok(Self {
            inner: Arc::new(BridgeInner {
                token: Arc::from(token),
                state: Mutex::new(BridgeState::default()),
                notify: Notify::new(),
            }),
        })
    }

    /// Capture and remove the environment copy. No token value is logged or persisted.
    pub fn from_env() -> Option<Self> {
        let token = std::env::var_os(DESKTOP_WORKER_TOKEN_ENV)?;
        std::env::remove_var(DESKTOP_WORKER_TOKEN_ENV);
        let token = match token.into_string() {
            Ok(token) => token,
            Err(_) => {
                eprintln!("[desktop] worker bridge disabled: token is not valid Unicode");
                return None;
            }
        };
        match Self::new(token) {
            Ok(bridge) => Some(bridge),
            Err(e) => {
                eprintln!("[desktop] worker bridge disabled: {e}");
                None
            }
        }
    }

    pub(crate) async fn execute(
        &self,
        request: DesktopRequest,
    ) -> Result<DesktopCompletion, DesktopBridgeError> {
        let deadline = request.deadline();
        self.execute_with_deadline(request, deadline).await
    }

    async fn execute_with_deadline(
        &self,
        request: DesktopRequest,
        deadline: Duration,
    ) -> Result<DesktopCompletion, DesktopBridgeError> {
        if matches!(request, DesktopRequest::Act(_)) {
            let state = lock_state(&self.inner.state);
            if state.requires_observation {
                return Err(DesktopBridgeError {
                    code: "observation_required",
                    message:
                        "a successful fresh desktop observation is required before desktop_act",
                });
            }
        }

        let (tx, rx) = oneshot::channel();
        let job_id = {
            let mut state = lock_state(&self.inner.state);
            if state.current.is_some() {
                return Err(DesktopBridgeError {
                    code: "busy",
                    message: "another desktop observation or action is already in flight",
                });
            }
            if matches!(request, DesktopRequest::Act(_)) && state.requires_observation {
                return Err(DesktopBridgeError {
                    code: "observation_required",
                    message:
                        "a successful fresh desktop observation is required before desktop_act",
                });
            }
            state.next_job_id = state.next_job_id.wrapping_add(1);
            let job_id = format!("desktop-{}-{}", std::process::id(), state.next_job_id);
            let (observe, action) = match &request {
                DesktopRequest::Observe(params) => (Some(params.clone()), None),
                DesktopRequest::Act(params) => (None, Some(params.clone())),
            };
            state.current = Some(JobSlot {
                job: DesktopJob {
                    protocol_version: DESKTOP_PROTOCOL_VERSION,
                    job_id: job_id.clone(),
                    worker_epoch: String::new(),
                    kind: request.kind(),
                    observe,
                    action,
                },
                stage: JobStage::Queued,
                tx: Some(tx),
            });
            job_id
        };
        self.inner.notify.notify_waiters();

        let mut call = DesktopCall {
            inner: Arc::downgrade(&self.inner),
            job_id,
            kind: request.kind(),
            rx,
            finished: false,
        };
        match timeout(deadline, &mut call).await {
            Ok(completion) => Ok(completion),
            Err(_) => Ok(call.deadline_exceeded()),
        }
    }

    #[cfg(test)]
    pub(crate) fn requires_observation(&self) -> bool {
        lock_state(&self.inner.state).requires_observation
    }

    fn activate_worker(&self, worker_epoch: &str) {
        let completion = {
            let mut state = lock_state(&self.inner.state);
            if state.active_worker_epoch.as_deref() == Some(worker_epoch) {
                return;
            }
            let had_previous_worker = state.active_worker_epoch.is_some();
            state.active_worker_epoch = Some(worker_epoch.to_string());
            if !had_previous_worker {
                return;
            }
            state.requires_observation = true;
            let Some(slot) = state.current.take() else {
                return;
            };
            let (outcome, error) = match slot.stage {
                JobStage::Queued => (DesktopOutcome::Failed, "cancelled_before_dispatch"),
                JobStage::Dispatched => {
                    state.requires_observation = true;
                    (DesktopOutcome::Unknown, "worker_epoch_changed")
                }
            };
            let completion = DesktopCompletion {
                job_id: slot.job.job_id.clone(),
                kind: slot.job.kind,
                outcome,
                result: json!({"error": error}),
                os_error: Some(error.to_string()),
            };
            record_terminal(
                &mut state,
                slot.job.job_id,
                TerminalJob {
                    worker_epoch: slot.job.worker_epoch,
                    outcome,
                    accept_late_receipt: slot.stage == JobStage::Dispatched,
                },
            );
            slot.tx.map(|tx| (tx, completion))
        };
        if let Some((tx, completion)) = completion {
            let _ = tx.send(completion);
        }
    }

    fn take_job(&self, worker_epoch: &str) -> Option<DesktopJob> {
        let mut state = lock_state(&self.inner.state);
        if state.active_worker_epoch.as_deref() != Some(worker_epoch) {
            return None;
        }
        let slot = state.current.as_mut()?;
        if slot.stage != JobStage::Queued {
            return None;
        }
        slot.stage = JobStage::Dispatched;
        slot.job.worker_epoch = worker_epoch.to_string();
        Some(slot.job.clone())
    }

    fn complete(
        &self,
        job_id: &str,
        receipt: DesktopWorkerCompletion,
    ) -> Result<CompletionAck, (StatusCode, Json<Value>)> {
        let mut state = lock_state(&self.inner.state);
        if state.active_worker_epoch.as_deref() != Some(receipt.worker_epoch.as_str()) {
            return Err(protocol_error(StatusCode::CONFLICT, "stale_worker_epoch"));
        }

        let current_matches = state.current.as_ref().is_some_and(|slot| {
            slot.stage == JobStage::Dispatched
                && slot.job.job_id == job_id
                && slot.job.worker_epoch == receipt.worker_epoch
        });
        if current_matches {
            let mut slot = state.current.take().expect("current job checked above");
            if slot.job.kind == DesktopJobKind::Act || receipt.outcome == DesktopOutcome::Unknown {
                state.requires_observation = true;
            } else if slot.job.kind == DesktopJobKind::Observe
                && receipt.outcome == DesktopOutcome::Succeeded
            {
                state.requires_observation = false;
            }
            let completion = DesktopCompletion {
                job_id: job_id.to_string(),
                kind: slot.job.kind,
                outcome: receipt.outcome,
                result: receipt.result,
                os_error: receipt.os_error,
            };
            record_terminal(
                &mut state,
                job_id.to_string(),
                TerminalJob {
                    worker_epoch: receipt.worker_epoch,
                    outcome: receipt.outcome,
                    accept_late_receipt: false,
                },
            );
            if let Some(tx) = slot.tx.take() {
                if tx.send(completion).is_err() {
                    // The OS result is real, but its delivery is not. Never replay it.
                    state.requires_observation = true;
                    if let Some(terminal) = state.terminal.get_mut(job_id) {
                        terminal.outcome = DesktopOutcome::Unknown;
                        terminal.accept_late_receipt = false;
                    }
                }
            }
            return Ok(CompletionAck {
                accepted: true,
                late: false,
            });
        }

        let Some(terminal) = state.terminal.get_mut(job_id) else {
            return Err(protocol_error(StatusCode::CONFLICT, "stale_or_unknown_job"));
        };
        if terminal.worker_epoch != receipt.worker_epoch {
            return Err(protocol_error(StatusCode::CONFLICT, "stale_worker_epoch"));
        }
        if !terminal.accept_late_receipt {
            return Err(protocol_error(StatusCode::CONFLICT, "duplicate_completion"));
        }
        terminal.accept_late_receipt = false;
        // Deliberately do not clear requires_observation: the original caller
        // already received unknown and must obtain a fresh observation.
        Ok(CompletionAck {
            accepted: true,
            late: true,
        })
    }
}

#[derive(Debug, Serialize)]
struct CompletionAck {
    accepted: bool,
    late: bool,
}

struct DesktopCall {
    inner: std::sync::Weak<BridgeInner>,
    job_id: String,
    kind: DesktopJobKind,
    rx: oneshot::Receiver<DesktopCompletion>,
    finished: bool,
}

impl DesktopCall {
    fn deadline_exceeded(&mut self) -> DesktopCompletion {
        self.finished = true;
        let Some(inner) = self.inner.upgrade() else {
            return synthetic_completion(
                &self.job_id,
                self.kind,
                DesktopOutcome::Unknown,
                "bridge_unavailable",
            );
        };
        let mut state = lock_state(&inner.state);
        let Some(slot) = state.current.take_if(|slot| slot.job.job_id == self.job_id) else {
            return synthetic_completion(
                &self.job_id,
                self.kind,
                DesktopOutcome::Unknown,
                "result_lost",
            );
        };
        let (outcome, error) = match slot.stage {
            JobStage::Queued => (DesktopOutcome::Failed, "cancelled_before_dispatch"),
            JobStage::Dispatched => {
                state.requires_observation = true;
                (DesktopOutcome::Unknown, "desktop_job_deadline_exceeded")
            }
        };
        record_terminal(
            &mut state,
            self.job_id.clone(),
            TerminalJob {
                worker_epoch: slot.job.worker_epoch,
                outcome,
                accept_late_receipt: slot.stage == JobStage::Dispatched,
            },
        );
        DesktopCompletion {
            job_id: self.job_id.clone(),
            kind: slot.job.kind,
            outcome,
            result: json!({"error": error}),
            os_error: Some(error.to_string()),
        }
    }
}

impl Future for DesktopCall {
    type Output = DesktopCompletion;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.rx).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.finished = true;
                Poll::Ready(result.unwrap_or_else(|_| {
                    if let Some(inner) = self.inner.upgrade() {
                        abort_job(&inner, &self.job_id);
                    }
                    synthetic_completion(
                        &self.job_id,
                        self.kind,
                        DesktopOutcome::Unknown,
                        "result_lost",
                    )
                }))
            }
        }
    }
}

impl Drop for DesktopCall {
    fn drop(&mut self) {
        if !self.finished {
            if let Some(inner) = self.inner.upgrade() {
                abort_job(&inner, &self.job_id);
            }
        }
    }
}

fn abort_job(inner: &BridgeInner, job_id: &str) {
    let mut state = lock_state(&inner.state);
    let Some(slot) = state.current.take_if(|slot| slot.job.job_id == job_id) else {
        return;
    };
    let (outcome, accept_late_receipt) = match slot.stage {
        JobStage::Queued => (DesktopOutcome::Failed, false),
        JobStage::Dispatched => {
            state.requires_observation = true;
            (DesktopOutcome::Unknown, true)
        }
    };
    record_terminal(
        &mut state,
        job_id.to_string(),
        TerminalJob {
            worker_epoch: slot.job.worker_epoch,
            outcome,
            accept_late_receipt,
        },
    );
}

fn synthetic_completion(
    job_id: &str,
    kind: DesktopJobKind,
    outcome: DesktopOutcome,
    error: &'static str,
) -> DesktopCompletion {
    DesktopCompletion {
        job_id: job_id.to_string(),
        kind,
        outcome,
        result: json!({"error": error}),
        os_error: Some(error.to_string()),
    }
}

fn lock_state(state: &Mutex<BridgeState>) -> MutexGuard<'_, BridgeState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn record_terminal(state: &mut BridgeState, job_id: String, terminal: TerminalJob) {
    if state.terminal.insert(job_id.clone(), terminal).is_none() {
        state.terminal_order.push_back(job_id);
    }
    while state.terminal_order.len() > TERMINAL_HISTORY {
        if let Some(oldest) = state.terminal_order.pop_front() {
            state.terminal.remove(&oldest);
        }
    }
}

/// Internal routes with an independent Bearer middleware. Merge this router
/// after the general 1 MiB request limit; completion has its own 4 MiB limit.
pub fn router(bridge: Arc<DesktopBridge>) -> axum::Router {
    axum::Router::new()
        .route(
            "/internal/desktop/v1/jobs/wait",
            axum::routing::post(wait_for_job),
        )
        .route(
            "/internal/desktop/v1/jobs/{job_id}/complete",
            axum::routing::post(complete_job)
                .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024)),
        )
        .with_state(bridge.clone())
        .layer(axum::middleware::from_fn_with_state(
            bridge,
            authenticate_worker,
        ))
}

async fn authenticate_worker(
    State(bridge): State<Arc<DesktopBridge>>,
    request: Request,
    next: Next,
) -> Response {
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let Some(provided) = provided else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if constant_time_token_eq(provided, &bridge.inner.token) {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

fn constant_time_token_eq(provided: &str, expected: &str) -> bool {
    let a = provided.as_bytes();
    let b = expected.as_bytes();
    if a.len() == b.len() {
        bool::from(a.ct_eq(b))
    } else {
        let _ = a.ct_eq(a);
        false
    }
}

async fn wait_for_job(
    State(bridge): State<Arc<DesktopBridge>>,
    Json(request): Json<DesktopWaitRequest>,
) -> Response {
    if request.protocol_version != DESKTOP_PROTOCOL_VERSION {
        return protocol_error(StatusCode::BAD_REQUEST, "protocol_version_mismatch")
            .into_response();
    }
    if request.worker_epoch.is_empty()
        || request.worker_epoch.chars().count() > MAX_WORKER_EPOCH_CHARS
    {
        return protocol_error(StatusCode::BAD_REQUEST, "invalid_worker_epoch").into_response();
    }
    bridge.activate_worker(&request.worker_epoch);
    let deadline = Instant::now() + LONG_POLL;
    loop {
        let notified = bridge.inner.notify.notified();
        tokio::pin!(notified);
        let _ = notified.as_mut().enable();
        if let Some(job) = bridge.take_job(&request.worker_epoch) {
            return (StatusCode::OK, Json(job)).into_response();
        }
        let now = Instant::now();
        if now >= deadline {
            return StatusCode::NO_CONTENT.into_response();
        }
        if timeout_at(deadline, notified).await.is_err() {
            return StatusCode::NO_CONTENT.into_response();
        }
    }
}

async fn complete_job(
    State(bridge): State<Arc<DesktopBridge>>,
    Path(job_id): Path<String>,
    Json(receipt): Json<DesktopWorkerCompletion>,
) -> Result<Json<CompletionAck>, (StatusCode, Json<Value>)> {
    if receipt.protocol_version != DESKTOP_PROTOCOL_VERSION {
        return Err(protocol_error(
            StatusCode::BAD_REQUEST,
            "protocol_version_mismatch",
        ));
    }
    if job_id.is_empty() || job_id.chars().count() > MAX_JOB_ID_CHARS {
        return Err(protocol_error(StatusCode::BAD_REQUEST, "invalid_job_id"));
    }
    bridge.complete(&job_id, receipt).map(Json)
}

fn protocol_error(status: StatusCode, code: &'static str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": code})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn bridge() -> DesktopBridge {
        DesktopBridge::new("desktop-test-token".to_string()).unwrap()
    }

    fn observe() -> DesktopRequest {
        DesktopRequest::Observe(DesktopObserveRequest {
            include_screenshot: false,
        })
    }

    fn action() -> DesktopRequest {
        DesktopRequest::Act(DesktopActionRequest {
            observation_id: "observation-1".to_string(),
            target_id: "target-1".to_string(),
            action: DesktopAction::Invoke,
            value: None,
        })
    }

    fn receipt(epoch: &str, outcome: DesktopOutcome) -> DesktopWorkerCompletion {
        DesktopWorkerCompletion {
            protocol_version: DESKTOP_PROTOCOL_VERSION,
            worker_epoch: epoch.to_string(),
            outcome,
            result: json!({"observation_id": "observation-1"}),
            os_error: None,
        }
    }

    async fn dispatch(bridge: &DesktopBridge, epoch: &str) -> DesktopJob {
        for _ in 0..100 {
            bridge.activate_worker(epoch);
            if let Some(job) = bridge.take_job(epoch) {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("job must dispatch")
    }

    #[tokio::test]
    async fn observe_round_trip_clears_reobservation_gate() {
        let bridge = bridge();
        bridge.activate_worker("old-epoch");
        bridge.activate_worker("new-epoch");
        assert!(bridge.requires_observation());
        let execution = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.execute(observe()).await.unwrap() }
        });
        tokio::task::yield_now().await;
        let job = dispatch(&bridge, "new-epoch").await;
        let ack = bridge
            .complete(&job.job_id, receipt("new-epoch", DesktopOutcome::Succeeded))
            .unwrap();
        assert!(!ack.late);
        let completion = execution.await.unwrap();
        assert_eq!(completion.outcome, DesktopOutcome::Succeeded);
        assert!(!bridge.requires_observation());
    }

    #[tokio::test]
    async fn only_one_job_can_be_in_flight() {
        let bridge = bridge();
        let first = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.execute(observe()).await }
        });
        tokio::task::yield_now().await;
        let error = bridge.execute(observe()).await.unwrap_err();
        assert_eq!(error.code, "busy");
        let job = dispatch(&bridge, "worker-1").await;
        bridge
            .complete(&job.job_id, receipt("worker-1", DesktopOutcome::Succeeded))
            .unwrap();
        assert!(first.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn queued_deadline_is_failed_not_unknown() {
        let bridge = bridge();
        let completion = bridge
            .execute_with_deadline(observe(), Duration::from_millis(5))
            .await
            .unwrap();
        assert_eq!(completion.outcome, DesktopOutcome::Failed);
        assert_eq!(
            completion.os_error.as_deref(),
            Some("cancelled_before_dispatch")
        );
        assert!(!bridge.requires_observation());
    }

    #[tokio::test]
    async fn dispatched_deadline_is_unknown_and_late_receipt_does_not_clear_gate() {
        let bridge = bridge();
        let execution = tokio::spawn({
            let bridge = bridge.clone();
            async move {
                bridge
                    .execute_with_deadline(observe(), Duration::from_millis(30))
                    .await
                    .unwrap()
            }
        });
        tokio::task::yield_now().await;
        let job = dispatch(&bridge, "worker-1").await;
        let completion = execution.await.unwrap();
        assert_eq!(completion.outcome, DesktopOutcome::Unknown);
        assert!(bridge.requires_observation());
        let ack = bridge
            .complete(&job.job_id, receipt("worker-1", DesktopOutcome::Succeeded))
            .unwrap();
        assert!(ack.late);
        assert!(bridge.requires_observation());
        let error = bridge.execute(action()).await.unwrap_err();
        assert_eq!(error.code, "observation_required");
    }

    #[tokio::test]
    async fn duplicate_and_cross_epoch_receipts_are_rejected() {
        let bridge = bridge();
        let execution = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.execute(observe()).await.unwrap() }
        });
        tokio::task::yield_now().await;
        let job = dispatch(&bridge, "worker-1").await;
        bridge
            .complete(&job.job_id, receipt("worker-1", DesktopOutcome::Succeeded))
            .unwrap();
        assert!(bridge
            .complete(&job.job_id, receipt("worker-1", DesktopOutcome::Succeeded))
            .is_err());
        assert!(bridge
            .complete(&job.job_id, receipt("worker-2", DesktopOutcome::Succeeded))
            .is_err());
        assert_eq!(execution.await.unwrap().outcome, DesktopOutcome::Succeeded);
    }

    #[tokio::test]
    async fn worker_epoch_change_makes_dispatched_job_unknown_without_replay() {
        let bridge = bridge();
        let execution = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.execute(action()).await.unwrap() }
        });
        tokio::task::yield_now().await;
        let job = dispatch(&bridge, "worker-1").await;
        bridge.activate_worker("worker-2");
        let completion = execution.await.unwrap();
        assert_eq!(completion.outcome, DesktopOutcome::Unknown);
        assert!(bridge.requires_observation());
        assert!(bridge
            .complete(&job.job_id, receipt("worker-1", DesktopOutcome::Succeeded))
            .is_err());
        assert!(bridge.take_job("worker-2").is_none());
    }

    #[tokio::test]
    async fn any_action_completion_requires_fresh_observation() {
        let bridge = bridge();
        let execution = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.execute(action()).await.unwrap() }
        });
        tokio::task::yield_now().await;
        let job = dispatch(&bridge, "worker-1").await;
        bridge
            .complete(&job.job_id, receipt("worker-1", DesktopOutcome::Succeeded))
            .unwrap();
        assert_eq!(execution.await.unwrap().outcome, DesktopOutcome::Succeeded);
        assert!(bridge.requires_observation());
    }

    #[tokio::test]
    async fn dedicated_auth_rejects_missing_wrong_and_general_tokens() {
        let app = router(Arc::new(bridge()));
        for token in [None, Some("wrong"), Some("general-serve-token")] {
            let mut builder = HttpRequest::builder()
                .method("POST")
                .uri("/internal/desktop/v1/jobs/wait");
            if let Some(token) = token {
                builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            let request = builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&DesktopWaitRequest {
                        protocol_version: DESKTOP_PROTOCOL_VERSION,
                        worker_epoch: "worker-1".to_string(),
                    })
                    .unwrap(),
                ))
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn protocol_mismatch_is_rejected() {
        let app = router(Arc::new(bridge()));
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/internal/desktop/v1/jobs/wait")
            .header(header::AUTHORIZATION, "Bearer desktop-test-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&DesktopWaitRequest {
                    protocol_version: 2,
                    worker_epoch: "worker-1".to_string(),
                })
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn synthetic_completion_preserves_action_kind() {
        let completion = synthetic_completion(
            "job-1",
            DesktopJobKind::Act,
            DesktopOutcome::Unknown,
            "result_lost",
        );
        assert_eq!(completion.kind, DesktopJobKind::Act);
        assert_eq!(completion.outcome, DesktopOutcome::Unknown);
    }

    #[test]
    fn golden_job_json_has_no_ambiguous_action_shape() {
        let job = DesktopJob {
            protocol_version: 1,
            job_id: "desktop-1".to_string(),
            worker_epoch: "epoch-1".to_string(),
            kind: DesktopJobKind::Act,
            observe: None,
            action: Some(DesktopActionRequest {
                observation_id: "obs-1".to_string(),
                target_id: "target-1".to_string(),
                action: DesktopAction::SetValue,
                value: Some("raw text".to_string()),
            }),
        };
        assert_eq!(
            serde_json::to_value(job).unwrap(),
            json!({
                "protocol_version": 1,
                "job_id": "desktop-1",
                "worker_epoch": "epoch-1",
                "kind": "act",
                "action": {
                    "observation_id": "obs-1",
                    "target_id": "target-1",
                    "action": "set_value",
                    "value": "raw text"
                }
            })
        );
    }
}
