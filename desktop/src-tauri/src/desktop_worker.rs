// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! Single-threaded Windows STA worker for raw UIA evidence and fixed actions.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use windows::core::BSTR;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetWindowDC,
    ReleaseDC, SelectObject, BITMAPINFO, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, SAFEARRAY,
};
use windows::Win32::System::Ole::{
    SafeArrayDestroy, SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    IUIAutomationSelectionItemPattern, IUIAutomationTogglePattern, IUIAutomationValuePattern,
    TreeScope_Children, UIA_InvokePatternId, UIA_SelectionItemPatternId, UIA_TogglePatternId,
    UIA_ValuePatternId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
    PW_RENDERFULLCONTENT,
};

const PROTOCOL_VERSION: u32 = 1;
const MAX_DEPTH: usize = 8;
const MAX_ELEMENTS: usize = 200;
const MAX_STRING_CHARS: usize = 1024;
const MAX_UIA_JSON_BYTES: usize = 256 * 1024;
const MAX_PNG_BYTES: usize = 1_500_000;
const MAX_SCREENSHOT_PIXEL_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JobKind {
    Observe,
    Act,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DesktopAction {
    Focus,
    Invoke,
    Toggle,
    Select,
    SetValue,
}

#[derive(Debug, Deserialize)]
struct ObserveRequest {
    #[serde(default)]
    include_screenshot: bool,
}

#[derive(Debug, Deserialize)]
struct ActionRequest {
    observation_id: String,
    target_id: String,
    action: DesktopAction,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DesktopJob {
    protocol_version: u32,
    job_id: String,
    kind: JobKind,
    observe: Option<ObserveRequest>,
    action: Option<ActionRequest>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum DesktopOutcome {
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Debug, Serialize)]
struct WorkerCompletion {
    protocol_version: u32,
    worker_epoch: String,
    outcome: DesktopOutcome,
    result: Value,
    os_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct WaitRequest<'a> {
    protocol_version: u32,
    worker_epoch: &'a str,
}

pub struct DesktopWorker {
    stop: Arc<AtomicBool>,
}

impl DesktopWorker {
    pub fn start(token: String, serve_port: Arc<RwLock<Option<u16>>>) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_epoch = crate::orchestrator::gen_token();
        std::thread::Builder::new()
            .name("desktop-uia-sta".to_string())
            .spawn(move || {
                let com = match unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok() {
                    Ok(()) => ComGuard,
                    Err(error) => {
                        eprintln!("[desktop] COM initialization failed: {}", os_error(&error));
                        return;
                    }
                };
                let _com = com;
                let client = match reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(23))
                    .build()
                {
                    Ok(client) => client,
                    Err(error) => {
                        eprintln!("[desktop] HTTP client initialization failed: {error}");
                        return;
                    }
                };
                let mut executor = Executor::new();
                while !worker_stop.load(Ordering::Acquire) {
                    let Some(port) = *serve_port.read().unwrap() else {
                        std::thread::sleep(Duration::from_millis(200));
                        continue;
                    };
                    let url = format!("http://127.0.0.1:{port}/internal/desktop/v1/jobs/wait");
                    let response = client
                        .post(url)
                        .bearer_auth(&token)
                        .json(&WaitRequest {
                            protocol_version: PROTOCOL_VERSION,
                            worker_epoch: &worker_epoch,
                        })
                        .send();
                    let job = match response {
                        Ok(response) if response.status() == reqwest::StatusCode::NO_CONTENT => {
                            continue;
                        }
                        Ok(response) if response.status() == reqwest::StatusCode::OK => {
                            match response.json::<DesktopJob>() {
                                Ok(job) => job,
                                Err(error) => {
                                    eprintln!("[desktop] invalid job response: {error}");
                                    continue;
                                }
                            }
                        }
                        Ok(response) => {
                            eprintln!("[desktop] job wait returned HTTP {}", response.status());
                            std::thread::sleep(Duration::from_millis(250));
                            continue;
                        }
                        Err(error) => {
                            if !worker_stop.load(Ordering::Acquire) {
                                eprintln!("[desktop] job wait failed: {error}");
                                std::thread::sleep(Duration::from_millis(250));
                            }
                            continue;
                        }
                    };
                    if job.protocol_version != PROTOCOL_VERSION {
                        eprintln!("[desktop] unsupported job protocol version");
                        continue;
                    }
                    let completion = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        || executor.execute(&job, &worker_epoch),
                    )) {
                        Ok(completion) => completion,
                        Err(_) => WorkerCompletion {
                            protocol_version: PROTOCOL_VERSION,
                            worker_epoch: worker_epoch.clone(),
                            outcome: DesktopOutcome::Unknown,
                            result: json!({"error": "desktop worker panicked while executing the job"}),
                            os_error: Some("desktop worker panicked while executing the job".into()),
                        },
                    };
                    let complete_url = format!(
                        "http://127.0.0.1:{port}/internal/desktop/v1/jobs/{}/complete",
                        job.job_id
                    );
                    if let Err(error) = client
                        .post(complete_url)
                        .bearer_auth(&token)
                        .json(&completion)
                        .send()
                    {
                        // Completion is never replayed. serve retains the unknown-result gate.
                        eprintln!("[desktop] job completion delivery failed: {error}");
                    }
                }
            })
            .map_err(|error| format!("spawn desktop UIA worker: {error}"))?;
        Ok(Self { stop })
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Drop for DesktopWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

struct ComGuard;

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct TargetEntry {
    element: IUIAutomationElement,
    runtime_id: Vec<i32>,
    control_type: i32,
    hwnd: isize,
    pid: u32,
}

struct ObservationCache {
    observation_id: String,
    targets: HashMap<String, TargetEntry>,
}

struct Executor {
    automation: IUIAutomation,
    generation: u64,
    cache: Option<ObservationCache>,
}

impl Executor {
    fn new() -> Self {
        let automation = unsafe {
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                .expect("UI Automation COM server is required on Windows")
        };
        Self {
            automation,
            generation: 0,
            cache: None,
        }
    }

    fn execute(&mut self, job: &DesktopJob, worker_epoch: &str) -> WorkerCompletion {
        let (outcome, result, os_error) = match job.kind {
            JobKind::Observe => match job.observe.as_ref() {
                Some(request) => match self.observe(request) {
                    Ok(result) => (DesktopOutcome::Succeeded, result, None),
                    Err(error) => failed(error),
                },
                None => failed("observe request is missing".into()),
            },
            JobKind::Act => match job.action.as_ref() {
                Some(request) => match self.act(request) {
                    Ok(result) => (DesktopOutcome::Succeeded, result, None),
                    Err(error) => failed(error),
                },
                None => failed("action request is missing".into()),
            },
        };
        WorkerCompletion {
            protocol_version: PROTOCOL_VERSION,
            worker_epoch: worker_epoch.to_string(),
            outcome,
            result,
            os_error,
        }
    }

    fn observe(&mut self, request: &ObserveRequest) -> Result<Value, String> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or("generation exhausted")?;
        self.cache = None;
        let generation = self.generation;
        let observation_id = format!("obs-{generation}-{}", crate::orchestrator::gen_token());
        let foreground = foreground_window()?;
        let root = unsafe { self.automation.ElementFromHandle(foreground.hwnd) }
            .map_err(|error| format!("ElementFromHandle: {}", os_error(&error)))?;
        let condition = unsafe { self.automation.RawViewCondition() }
            .map_err(|error| format!("RawViewCondition: {}", os_error(&error)))?;
        let mut elements = Vec::new();
        let mut targets = HashMap::new();
        let mut reasons = Vec::new();
        let mut string_truncated = false;
        self.capture_element(
            &root,
            0,
            &condition,
            &foreground,
            &observation_id,
            &mut elements,
            &mut targets,
            &mut reasons,
            &mut string_truncated,
        )?;
        if string_truncated {
            reasons.push("string_limit".into());
        }

        let captured_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mut result = json!({
            "protocol_version": PROTOCOL_VERSION,
            "observation_id": observation_id,
            "generation": generation,
            "captured_at": captured_at,
            "window": foreground.to_json(),
            "uia": {
                "elements": elements,
                "depth_limit": MAX_DEPTH,
                "element_limit": MAX_ELEMENTS,
            },
            "truncated": !reasons.is_empty(),
            "truncation_reasons": reasons,
        });

        let mut removed_for_json_budget = false;
        while serde_json::to_vec(&result)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_UIA_JSON_BYTES
        {
            let Some(elements) = result["uia"]["elements"].as_array_mut() else {
                return Err("UIA elements are not an array".into());
            };
            if elements.pop().is_none() {
                return Err("UIA observation metadata exceeds 256 KiB".into());
            }
            removed_for_json_budget = true;
        }
        if removed_for_json_budget {
            result["truncated"] = Value::Bool(true);
            result["truncation_reasons"]
                .as_array_mut()
                .ok_or("truncation reasons are not an array")?
                .push(Value::String("uia_json_limit".into()));
            let retained: HashSet<String> = result["uia"]["elements"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|element| element["target_id"].as_str().map(str::to_string))
                .collect();
            targets.retain(|target_id, _| retained.contains(target_id));
        }

        if request.include_screenshot {
            match capture_png(foreground.hwnd, foreground.rect) {
                Ok(png) => {
                    result["screenshot"] = json!({
                        "status": "captured",
                        "mime_type": "image/png",
                        "bytes": png.len(),
                    });
                    result["images"] = json!([{
                        "mime_type": "image/png",
                        "data_base64": base64::engine::general_purpose::STANDARD.encode(png),
                    }]);
                }
                Err(error) => {
                    result["screenshot"] = json!({"status": "failed", "error": error});
                }
            }
        }
        self.cache = Some(ObservationCache {
            observation_id,
            targets,
        });
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn capture_element(
        &self,
        element: &IUIAutomationElement,
        depth: usize,
        condition: &windows::Win32::UI::Accessibility::IUIAutomationCondition,
        foreground: &ForegroundWindow,
        observation_id: &str,
        elements: &mut Vec<Value>,
        targets: &mut HashMap<String, TargetEntry>,
        reasons: &mut Vec<String>,
        string_truncated: &mut bool,
    ) -> Result<(), String> {
        if elements.len() >= MAX_ELEMENTS {
            if !reasons.iter().any(|reason| reason == "element_limit") {
                reasons.push("element_limit".into());
            }
            return Ok(());
        }
        let (value, target) = self.capture_properties(
            element,
            foreground,
            observation_id,
            elements.len(),
            string_truncated,
        )?;
        elements.push(value);
        targets.insert(target.runtime_key.clone(), target.entry);

        if depth >= MAX_DEPTH {
            let children = unsafe { element.FindAll(TreeScope_Children, condition) }
                .map_err(|error| format!("FindAll children: {}", os_error(&error)))?;
            if unsafe { children.Length() }.map_err(|error| os_error(&error))? > 0
                && !reasons.iter().any(|reason| reason == "depth_limit")
            {
                reasons.push("depth_limit".into());
            }
            return Ok(());
        }
        let children = unsafe { element.FindAll(TreeScope_Children, condition) }
            .map_err(|error| format!("FindAll children: {}", os_error(&error)))?;
        let count = unsafe { children.Length() }.map_err(|error| os_error(&error))?;
        for index in 0..count {
            if elements.len() >= MAX_ELEMENTS {
                if !reasons.iter().any(|reason| reason == "element_limit") {
                    reasons.push("element_limit".into());
                }
                break;
            }
            let child = unsafe { children.GetElement(index) }
                .map_err(|error| format!("GetElement child: {}", os_error(&error)))?;
            self.capture_element(
                &child,
                depth + 1,
                condition,
                foreground,
                observation_id,
                elements,
                targets,
                reasons,
                string_truncated,
            )?;
        }
        Ok(())
    }

    fn capture_properties(
        &self,
        element: &IUIAutomationElement,
        foreground: &ForegroundWindow,
        observation_id: &str,
        index: usize,
        string_truncated: &mut bool,
    ) -> Result<(Value, CapturedTarget), String> {
        let runtime_id = runtime_id(element)?;
        let control_type = unsafe { element.CurrentControlType() }
            .map_err(|error| format!("CurrentControlType: {}", os_error(&error)))?
            .0;
        let name = bounded_string(
            unsafe { element.CurrentName() }
                .map_err(|error| os_error(&error))?
                .to_string(),
            string_truncated,
        );
        let automation_id = bounded_string(
            unsafe { element.CurrentAutomationId() }
                .map_err(|error| os_error(&error))?
                .to_string(),
            string_truncated,
        );
        let class_name = bounded_string(
            unsafe { element.CurrentClassName() }
                .map_err(|error| os_error(&error))?
                .to_string(),
            string_truncated,
        );
        let rect =
            unsafe { element.CurrentBoundingRectangle() }.map_err(|error| os_error(&error))?;
        let enabled = unsafe { element.CurrentIsEnabled() }
            .map_err(|error| os_error(&error))?
            .as_bool();
        let offscreen = unsafe { element.CurrentIsOffscreen() }
            .map_err(|error| os_error(&error))?
            .as_bool();
        let password = unsafe { element.CurrentIsPassword() }
            .map_err(|error| os_error(&error))?
            .as_bool();
        let focusable = unsafe { element.CurrentIsKeyboardFocusable() }
            .map_err(|error| os_error(&error))?
            .as_bool();
        let invoke = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
        }
        .is_ok();
        let value_pattern =
            unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
                .ok();
        let toggle_pattern = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
        }
        .ok();
        let selection_pattern = unsafe {
            element.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                UIA_SelectionItemPatternId,
            )
        }
        .ok();
        let mut patterns = Vec::new();
        if invoke {
            patterns.push("invoke");
        }
        if value_pattern.is_some() {
            patterns.push("value");
        }
        if toggle_pattern.is_some() {
            patterns.push("toggle");
        }
        if selection_pattern.is_some() {
            patterns.push("selection_item");
        }
        let value_state = value_pattern.as_ref().map(|pattern| {
            let read_only = unsafe { pattern.CurrentIsReadOnly() }
                .map(|value| value.as_bool())
                .ok();
            let value = if password {
                Value::Null
            } else {
                unsafe { pattern.CurrentValue() }
                    .map(|value| Value::String(bounded_string(value.to_string(), string_truncated)))
                    .unwrap_or(Value::Null)
            };
            json!({"value": value, "is_read_only": read_only})
        });
        let toggle_state = toggle_pattern.as_ref().and_then(|pattern| {
            unsafe { pattern.CurrentToggleState() }
                .ok()
                .map(|state| state.0)
        });
        let selected = selection_pattern.as_ref().and_then(|pattern| {
            unsafe { pattern.CurrentIsSelected() }
                .ok()
                .map(|state| state.as_bool())
        });
        let target_id = format!("{observation_id}:target-{index}");
        let json_value = json!({
            "target_id": target_id,
            "runtime_id": runtime_id,
            "name": name,
            "automation_id": automation_id,
            "control_type": control_type,
            "class_name": class_name,
            "bounding_rectangle": rect_json(rect),
            "is_enabled": enabled,
            "is_offscreen": offscreen,
            "is_password": password,
            "is_keyboard_focusable": focusable,
            "patterns": patterns,
            "value": value_state,
            "toggle_state": toggle_state,
            "is_selected": selected,
        });
        Ok((
            json_value,
            CapturedTarget {
                runtime_key: target_id,
                entry: TargetEntry {
                    element: element.clone(),
                    runtime_id,
                    control_type,
                    hwnd: foreground.hwnd.0 as isize,
                    pid: foreground.pid,
                },
            },
        ))
    }

    fn act(&mut self, request: &ActionRequest) -> Result<Value, String> {
        // Any action attempt consumes the whole observation, including validation failures.
        let cache = self.cache.take();
        let mut cache = cache.ok_or("a fresh desktop observation is required")?;
        if cache.observation_id != request.observation_id {
            return Err("observation_id is not the latest observation".into());
        }
        let target = cache
            .targets
            .remove(request.target_id.as_str())
            .ok_or("target_id is stale, unknown, or already consumed")?;
        let foreground = foreground_window()?;
        if foreground.hwnd.0 as isize != target.hwnd || foreground.pid != target.pid {
            return Err("the observed window is no longer in the foreground".into());
        }
        if runtime_id(&target.element)? != target.runtime_id {
            return Err("target RuntimeId changed or is no longer valid".into());
        }
        let control_type = unsafe { target.element.CurrentControlType() }
            .map_err(|error| os_error(&error))?
            .0;
        if control_type != target.control_type {
            return Err("target ControlType changed".into());
        }
        if !unsafe { target.element.CurrentIsEnabled() }
            .map_err(|error| os_error(&error))?
            .as_bool()
        {
            return Err("target is disabled".into());
        }
        if unsafe { target.element.CurrentIsOffscreen() }
            .map_err(|error| os_error(&error))?
            .as_bool()
        {
            return Err("target is offscreen".into());
        }
        match request.action {
            DesktopAction::Focus => {
                if !unsafe { target.element.CurrentIsKeyboardFocusable() }
                    .map_err(|error| os_error(&error))?
                    .as_bool()
                {
                    return Err("target is not keyboard focusable".into());
                }
                unsafe { target.element.SetFocus() }.map_err(|error| os_error(&error))?;
            }
            DesktopAction::Invoke => unsafe {
                target
                    .element
                    .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                    .map_err(|error| os_error(&error))?
                    .Invoke()
                    .map_err(|error| os_error(&error))?;
            },
            DesktopAction::Toggle => unsafe {
                target
                    .element
                    .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                    .map_err(|error| os_error(&error))?
                    .Toggle()
                    .map_err(|error| os_error(&error))?;
            },
            DesktopAction::Select => unsafe {
                target
                    .element
                    .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                        UIA_SelectionItemPatternId,
                    )
                    .map_err(|error| os_error(&error))?
                    .Select()
                    .map_err(|error| os_error(&error))?;
            },
            DesktopAction::SetValue => {
                if unsafe { target.element.CurrentIsPassword() }
                    .map_err(|error| os_error(&error))?
                    .as_bool()
                {
                    return Err("set_value is forbidden for password elements".into());
                }
                let pattern = unsafe {
                    target
                        .element
                        .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                }
                .map_err(|error| os_error(&error))?;
                if unsafe { pattern.CurrentIsReadOnly() }
                    .map_err(|error| os_error(&error))?
                    .as_bool()
                {
                    return Err("target ValuePattern is read-only".into());
                }
                let value = request
                    .value
                    .as_deref()
                    .ok_or("set_value requires a value")?;
                unsafe { pattern.SetValue(&BSTR::from(value)) }
                    .map_err(|error| os_error(&error))?;
            }
        }
        Ok(json!({
            "action": request.action,
            "target_id": request.target_id,
            "observation_id": request.observation_id,
        }))
    }
}

struct CapturedTarget {
    runtime_key: String,
    entry: TargetEntry,
}

fn failed(error: String) -> (DesktopOutcome, Value, Option<String>) {
    (DesktopOutcome::Failed, json!({"error": error}), Some(error))
}

fn os_error(error: &windows::core::Error) -> String {
    format!("HRESULT 0x{:08X}: {error}", error.code().0 as u32)
}

fn bounded_string(value: String, truncated: &mut bool) -> String {
    if value.chars().count() <= MAX_STRING_CHARS {
        return value;
    }
    *truncated = true;
    value.chars().take(MAX_STRING_CHARS).collect()
}

fn runtime_id(element: &IUIAutomationElement) -> Result<Vec<i32>, String> {
    let array = unsafe { element.GetRuntimeId() }.map_err(|error| os_error(&error))?;
    let guard = SafeArrayGuard(array);
    let lower = unsafe { SafeArrayGetLBound(guard.0, 1) }.map_err(|error| os_error(&error))?;
    let upper = unsafe { SafeArrayGetUBound(guard.0, 1) }.map_err(|error| os_error(&error))?;
    let mut values = Vec::new();
    for index in lower..=upper {
        let mut value = 0i32;
        unsafe { SafeArrayGetElement(guard.0, &index, &mut value as *mut i32 as *mut c_void) }
            .map_err(|error| os_error(&error))?;
        values.push(value);
    }
    Ok(values)
}

struct SafeArrayGuard(*mut SAFEARRAY);

impl Drop for SafeArrayGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            let _ = unsafe { SafeArrayDestroy(self.0) };
        }
    }
}

struct ForegroundWindow {
    hwnd: HWND,
    pid: u32,
    title: String,
    class_name: String,
    rect: RECT,
}

impl ForegroundWindow {
    fn to_json(&self) -> Value {
        json!({
            "hwnd": (self.hwnd.0 as isize).to_string(),
            "pid": self.pid,
            "title": self.title,
            "class_name": self.class_name,
            "rectangle": rect_json(self.rect),
        })
    }
}

fn foreground_window() -> Result<ForegroundWindow, String> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return Err("GetForegroundWindow returned no window".into());
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return Err("GetWindowThreadProcessId returned PID 0".into());
    }
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.map_err(|error| os_error(&error))?;
    Ok(ForegroundWindow {
        hwnd,
        pid,
        title: window_string(hwnd, true),
        class_name: window_string(hwnd, false),
        rect,
    })
}

fn window_string(hwnd: HWND, title: bool) -> String {
    let mut buffer = [0u16; 1025];
    let length = unsafe {
        if title {
            GetWindowTextW(hwnd, &mut buffer)
        } else {
            GetClassNameW(hwnd, &mut buffer)
        }
    };
    String::from_utf16_lossy(&buffer[..length.max(0) as usize])
}

fn rect_json(rect: RECT) -> Value {
    json!({
        "left": rect.left,
        "top": rect.top,
        "right": rect.right,
        "bottom": rect.bottom,
    })
}

struct WindowDc(HWND, HDC);

impl Drop for WindowDc {
    fn drop(&mut self) {
        if !self.1.is_invalid() {
            unsafe { ReleaseDC(Some(self.0), self.1) };
        }
    }
}

struct MemoryDc(HDC);

impl Drop for MemoryDc {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { DeleteDC(self.0) };
        }
    }
}

struct Bitmap(HBITMAP);

impl Drop for Bitmap {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { DeleteObject(HGDIOBJ(self.0 .0)) };
        }
    }
}

fn capture_png(hwnd: HWND, rect: RECT) -> Result<Vec<u8>, String> {
    let width = rect
        .right
        .checked_sub(rect.left)
        .ok_or("invalid window width")?;
    let height = rect
        .bottom
        .checked_sub(rect.top)
        .ok_or("invalid window height")?;
    if width <= 0 || height <= 0 {
        return Err("foreground window has an empty rectangle".into());
    }
    let pixel_bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("screenshot dimensions overflow")?;
    if pixel_bytes > MAX_SCREENSHOT_PIXEL_BYTES {
        return Err("screenshot raw pixel buffer exceeds 64 MiB".into());
    }
    let window_dc = WindowDc(hwnd, unsafe { GetWindowDC(Some(hwnd)) });
    if window_dc.1.is_invalid() {
        return Err("GetWindowDC failed".into());
    }
    let memory_dc = MemoryDc(unsafe { CreateCompatibleDC(Some(window_dc.1)) });
    if memory_dc.0.is_invalid() {
        return Err("CreateCompatibleDC failed".into());
    }
    let bitmap = Bitmap(unsafe { CreateCompatibleBitmap(window_dc.1, width, height) });
    if bitmap.0.is_invalid() {
        return Err("CreateCompatibleBitmap failed".into());
    }
    let old = unsafe { SelectObject(memory_dc.0, HGDIOBJ(bitmap.0 .0)) };
    if old.is_invalid() {
        return Err("SelectObject failed".into());
    }
    let capture_result = (|| -> Result<Vec<u8>, String> {
        let printed =
            unsafe { PrintWindow(hwnd, memory_dc.0, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)) }
                .as_bool();
        if !printed {
            unsafe {
                windows::Win32::Graphics::Gdi::BitBlt(
                    memory_dc.0,
                    0,
                    0,
                    width,
                    height,
                    Some(window_dc.1),
                    0,
                    0,
                    windows::Win32::Graphics::Gdi::SRCCOPY,
                )
            }
            .map_err(|error| format!("PrintWindow and BitBlt failed: {}", os_error(&error)))?;
        }
        let mut pixels = vec![0u8; pixel_bytes];
        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = std::mem::size_of_val(&info.bmiHeader) as u32;
        info.bmiHeader.biWidth = width;
        info.bmiHeader.biHeight = -height;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;
        let rows = unsafe {
            GetDIBits(
                memory_dc.0,
                bitmap.0,
                0,
                height as u32,
                Some(pixels.as_mut_ptr() as *mut c_void),
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        if rows != height {
            return Err(format!("GetDIBits returned {rows} rows, expected {height}"));
        }
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }
        let mut png = Cursor::new(Vec::new());
        let mut encoder = png::Encoder::new(&mut png, width as u32, height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
        writer
            .write_image_data(&pixels)
            .map_err(|error| error.to_string())?;
        drop(writer);
        let png = png.into_inner();
        if png.len() > MAX_PNG_BYTES {
            return Err(format!(
                "screenshot PNG is {} bytes, exceeding the 1500000-byte limit",
                png.len()
            ));
        }
        Ok(png)
    })();
    unsafe { SelectObject(memory_dc.0, old) };
    capture_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_strings_count_unicode_characters() {
        let mut truncated = false;
        let value = bounded_string("界".repeat(MAX_STRING_CHARS + 1), &mut truncated);
        assert!(truncated);
        assert_eq!(value.chars().count(), MAX_STRING_CHARS);
    }

    #[test]
    fn action_names_match_the_wire_protocol() {
        let action: DesktopAction = serde_json::from_str("\"set_value\"").unwrap();
        assert_eq!(action, DesktopAction::SetValue);
        assert!(serde_json::from_str::<DesktopAction>("\"click\"").is_err());
    }

    #[test]
    fn completion_uses_the_assigned_worker_epoch() {
        let completion = WorkerCompletion {
            protocol_version: PROTOCOL_VERSION,
            worker_epoch: "epoch-test".into(),
            outcome: DesktopOutcome::Failed,
            result: json!({"error": "raw"}),
            os_error: Some("raw".into()),
        };
        let value = serde_json::to_value(completion).unwrap();
        assert_eq!(value["worker_epoch"], "epoch-test");
        assert_eq!(value["outcome"], "failed");
    }

    use std::process::{Child, Command};
    use std::time::Instant;

    use windows::core::PCWSTR;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    const MAIN_TITLE: &str = "EvoAgent Desktop UIA Fixture";
    const OTHER_TITLE: &str = "EvoAgent Desktop UIA Fixture Other";

    struct FixtureChild(Child);

    impl Drop for FixtureChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn wait_for_window(title: &str) -> HWND {
        let title_wide = wide(title);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(hwnd) = unsafe { FindWindowW(None, PCWSTR(title_wide.as_ptr())) } {
                if !hwnd.is_invalid() {
                    return hwnd;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for fixture window {title}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn activate(title: &str) {
        let hwnd = wait_for_window(title);
        let current_thread = unsafe { GetCurrentThreadId() };
        let foreground_thread = unsafe { GetWindowThreadProcessId(GetForegroundWindow(), None) };
        let target_thread = unsafe { GetWindowThreadProcessId(hwnd, None) };
        let attach =
            |from, to| from != to && unsafe { AttachThreadInput(from, to, true) }.as_bool();
        let attached_foreground = attach(current_thread, foreground_thread);
        let attached_target = attach(current_thread, target_thread);
        let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
        let _ = unsafe { BringWindowToTop(hwnd) };
        let _ = unsafe { SetForegroundWindow(hwnd) };
        if attached_target {
            let _ = unsafe { AttachThreadInput(current_thread, target_thread, false) };
        }
        if attached_foreground {
            let _ = unsafe { AttachThreadInput(current_thread, foreground_thread, false) };
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while unsafe { GetForegroundWindow() } != hwnd {
            assert!(
                Instant::now() < deadline,
                "fixture window did not become foreground: {title}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn observe(executor: &mut Executor, include_screenshot: bool) -> Value {
        executor
            .observe(&ObserveRequest { include_screenshot })
            .unwrap()
    }

    fn element<'a>(observation: &'a Value, automation_id: &str) -> &'a Value {
        observation["uia"]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|element| element["automation_id"] == automation_id)
            .unwrap_or_else(|| panic!("missing UIA element {automation_id}"))
    }

    fn request(
        observation: &Value,
        automation_id: &str,
        action: DesktopAction,
        value: Option<&str>,
    ) -> ActionRequest {
        ActionRequest {
            observation_id: observation["observation_id"].as_str().unwrap().to_string(),
            target_id: element(observation, automation_id)["target_id"]
                .as_str()
                .unwrap()
                .to_string(),
            action,
            value: value.map(str::to_string),
        }
    }

    fn assert_requires_fresh_observation(executor: &mut Executor, request: &ActionRequest) {
        assert_eq!(
            executor.act(request).unwrap_err(),
            "a fresh desktop observation is required"
        );
    }

    #[test]
    fn windows_uia_fixture_covers_actions_and_target_freshness() {
        let Some(fixture) = std::env::var_os("EVO_AGENT_DESKTOP_UIA_FIXTURE") else {
            eprintln!(
                "EVO_AGENT_DESKTOP_UIA_FIXTURE is unset; skipping interactive UIA acceptance"
            );
            return;
        };
        let _com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .map(|()| ComGuard)
            .unwrap();
        let _child = FixtureChild(
            Command::new(fixture)
                .spawn()
                .expect("spawn the WPF UIA fixture"),
        );
        activate(MAIN_TITLE);
        let mut executor = Executor::new();

        let observation = observe(&mut executor, true);
        assert_eq!(observation["window"]["title"], MAIN_TITLE);
        assert_eq!(observation["screenshot"]["status"], "captured");
        let screenshot = &observation["images"][0];
        assert_eq!(screenshot["mime_type"], "image/png");
        let png = base64::engine::general_purpose::STANDARD
            .decode(screenshot["data_base64"].as_str().unwrap())
            .unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(observation["screenshot"]["bytes"], png.len());
        assert_eq!(element(&observation, "InvokeStatus")["name"], "not-invoked");
        assert_eq!(element(&observation, "ToggleTarget")["toggle_state"], 0);
        assert_eq!(element(&observation, "SelectTarget")["is_selected"], false);
        assert_eq!(
            element(&observation, "ValueTarget")["value"]["value"],
            "initial"
        );

        let focus = request(&observation, "FocusTarget", DesktopAction::Focus, None);
        let consumed_target = request(&observation, "InvokeTarget", DesktopAction::Invoke, None);
        executor.act(&focus).unwrap();
        assert_requires_fresh_observation(&mut executor, &focus);
        assert_requires_fresh_observation(&mut executor, &consumed_target);

        let observation = observe(&mut executor, false);
        let focus = request(&observation, "FocusTarget", DesktopAction::Focus, None);
        let has_focus = {
            let cache = executor.cache.as_ref().unwrap();
            let target = cache.targets.get(&focus.target_id).unwrap();
            unsafe { target.element.CurrentHasKeyboardFocus() }
                .unwrap()
                .as_bool()
        };
        assert!(has_focus, "focus action did not move keyboard focus");

        let invoke = request(&observation, "InvokeTarget", DesktopAction::Invoke, None);
        executor.act(&invoke).unwrap();
        let observation = observe(&mut executor, false);
        assert_eq!(element(&observation, "InvokeStatus")["name"], "invoked");

        let toggle = request(&observation, "ToggleTarget", DesktopAction::Toggle, None);
        executor.act(&toggle).unwrap();
        let observation = observe(&mut executor, false);
        assert_eq!(element(&observation, "ToggleTarget")["toggle_state"], 1);

        let select = request(&observation, "SelectTarget", DesktopAction::Select, None);
        executor.act(&select).unwrap();
        let observation = observe(&mut executor, false);
        assert_eq!(element(&observation, "SelectTarget")["is_selected"], true);

        let set_value = request(
            &observation,
            "ValueTarget",
            DesktopAction::SetValue,
            Some("updated 界"),
        );
        executor.act(&set_value).unwrap();
        let observation = observe(&mut executor, false);
        assert_eq!(
            element(&observation, "ValueTarget")["value"]["value"],
            "updated 界"
        );

        let stale = request(&observation, "FocusTarget", DesktopAction::Focus, None);
        let _latest = observe(&mut executor, false);
        assert_eq!(
            executor.act(&stale).unwrap_err(),
            "observation_id is not the latest observation"
        );
        assert_requires_fresh_observation(&mut executor, &stale);

        let observation = observe(&mut executor, false);
        let mut unknown = request(&observation, "FocusTarget", DesktopAction::Focus, None);
        unknown.target_id = format!("{}:unknown", unknown.observation_id);
        assert_eq!(
            executor.act(&unknown).unwrap_err(),
            "target_id is stale, unknown, or already consumed"
        );
        assert_requires_fresh_observation(&mut executor, &unknown);

        let observation = observe(&mut executor, false);
        assert_eq!(
            element(&observation, "ReadOnlyTarget")["value"]["is_read_only"],
            true
        );
        let read_only = request(
            &observation,
            "ReadOnlyTarget",
            DesktopAction::SetValue,
            Some("forbidden"),
        );
        assert_eq!(
            executor.act(&read_only).unwrap_err(),
            "target ValuePattern is read-only"
        );
        assert_requires_fresh_observation(&mut executor, &read_only);

        let observation = observe(&mut executor, false);
        let password_element = element(&observation, "PasswordTarget");
        assert_eq!(password_element["is_password"], true);
        assert_eq!(password_element["value"]["value"], Value::Null);
        let password = request(
            &observation,
            "PasswordTarget",
            DesktopAction::SetValue,
            Some("forbidden"),
        );
        assert_eq!(
            executor.act(&password).unwrap_err(),
            "set_value is forbidden for password elements"
        );
        assert_requires_fresh_observation(&mut executor, &password);

        let observation = observe(&mut executor, false);
        let wrong_foreground = request(&observation, "FocusTarget", DesktopAction::Focus, None);
        activate(OTHER_TITLE);
        assert_eq!(
            executor.act(&wrong_foreground).unwrap_err(),
            "the observed window is no longer in the foreground"
        );
        assert_requires_fresh_observation(&mut executor, &wrong_foreground);
        activate(MAIN_TITLE);

        let observation = observe(&mut executor, false);
        let runtime_mismatch = request(&observation, "FocusTarget", DesktopAction::Focus, None);
        executor
            .cache
            .as_mut()
            .unwrap()
            .targets
            .get_mut(&runtime_mismatch.target_id)
            .unwrap()
            .runtime_id
            .push(-1);
        assert_eq!(
            executor.act(&runtime_mismatch).unwrap_err(),
            "target RuntimeId changed or is no longer valid"
        );
        assert_requires_fresh_observation(&mut executor, &runtime_mismatch);

        let observation = observe(&mut executor, false);
        let control_type_mismatch =
            request(&observation, "FocusTarget", DesktopAction::Focus, None);
        executor
            .cache
            .as_mut()
            .unwrap()
            .targets
            .get_mut(&control_type_mismatch.target_id)
            .unwrap()
            .control_type += 1;
        assert_eq!(
            executor.act(&control_type_mismatch).unwrap_err(),
            "target ControlType changed"
        );
        assert_requires_fresh_observation(&mut executor, &control_type_mismatch);
    }
}
