//! Bridge the limux control socket onto the GTK host state.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use gtk::glib;
use gtk4 as gtk;
use limux_control::auth::{self, SocketControlMode};
use limux_control::request_io::{self, read_request_frame};
use limux_control::socket_path::{bind_listener, resolve_socket_path, SocketMode};
use limux_protocol::{parse_v1_command_envelope, V2Request, V2Response};
use serde_json::{json, Map, Value};

const METHODS: &[&str] = &[
    "system.ping",
    "system.identify",
    "system.capabilities",
    "workspace.current",
    "workspace.list",
    "workspace.create",
    "workspace.select",
    "workspace.rename",
    "workspace.close",
    "pane.list",
    "pane.surfaces",
    "pane.create",
    "surface.list",
    "surface.health",
    "surface.read_text",
    "surface.send_text",
    "surface.send_key",
    "browser.open_split",
    "browser.navigate",
    "browser.url.get",
    "browser.get.title",
    "browser.get.text",
    "browser.get.html",
    "browser.get.value",
    "browser.get.attr",
    "browser.get.count",
    "browser.get.box",
    "browser.get.styles",
    "browser.wait",
    "browser.eval",
    "browser.snapshot",
    "browser.find.text",
    "browser.find.label",
    "browser.find.placeholder",
    "browser.find.title",
    "browser.find.testid",
    "browser.find.alt",
    "browser.find.role",
    "browser.find.first",
    "browser.find.last",
    "browser.find.nth",
    "browser.click",
    "browser.fill",
    "browser.type",
    "browser.check",
    "browser.uncheck",
    "browser.select",
    "browser.focus",
    "browser.hover",
    "browser.dblclick",
    "browser.scroll",
    "browser.scroll_into_view",
    "browser.press",
    "browser.keydown",
    "browser.keyup",
    "browser.cookies.get",
    "browser.cookies.set",
    "browser.cookies.clear",
    "browser.storage.get",
    "browser.storage.set",
    "browser.storage.clear",
    "notification.create",
];

const PARSE_ERROR_CODE: i64 = -32700;
const INVALID_PARAMS_CODE: i64 = -32602;
const UNKNOWN_METHOD_CODE: i64 = -32601;
const INTERNAL_ERROR_CODE: i64 = -32603;
const NOT_FOUND_CODE: i64 = -32004;
const CONFLICT_CODE: i64 = -32009;

type BridgeResult = Result<Value, BridgeError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceTarget {
    Active,
    Handle(String),
    Name(String),
    Index(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PaneCreateDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PaneCreateType {
    Terminal,
    Browser,
}

/// Parser-level contract for the live-GTK `pane.create` route.
///
/// Request fields accepted by the bridge:
/// - `workspace_id`/`id`, `name`, or `index` target the workspace. Raw
///   handles and `workspace:<id>` refs are accepted and preserved for the GTK
///   layer to resolve.
/// - `surface_id` and `pane_id` identify the source pane. Raw handles and
///   `surface:<id>`/`pane:<id>` refs are accepted. Later GTK work resolves
///   precedence as explicit surface, explicit pane, then safe workspace-local
///   fallback.
/// - `direction` is one of `left|right|up|down`, defaulting to `right`.
/// - `type` is one of `terminal|browser`, defaulting to `terminal`.
/// - `command` is a terminal-only host extension: the host injects it into the
///   newly-created surface after creation. The standalone core dispatcher may
///   accept the field for compatibility but does not launch a process.
///
/// `pane.create` intentionally implements live-GTK terminal panes only. Browser
/// split creation is exposed through `browser.open_split`, so `type=browser` and
/// `url` on `pane.create` fail at parse time before any GTK work is scheduled.
/// Responses must keep the existing core/CLI field
/// names: `pane_id`, `pane_ref`, `surface_id`, and `surface_ref`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatePaneRequest {
    pub target: WorkspaceTarget,
    pub source_pane_id: Option<String>,
    pub source_surface_id: Option<String>,
    pub direction: PaneCreateDirection,
    pub pane_type: PaneCreateType,
    pub command: Option<String>,
}

#[derive(Debug)]
pub enum ControlCommand {
    Identify {
        caller: Option<Value>,
        reply: mpsc::Sender<BridgeResult>,
    },
    CurrentWorkspace {
        reply: mpsc::Sender<BridgeResult>,
    },
    ListWorkspaces {
        reply: mpsc::Sender<BridgeResult>,
    },
    ListPanes {
        target: WorkspaceTarget,
        reply: mpsc::Sender<BridgeResult>,
    },
    ListPaneSurfaces {
        target: WorkspaceTarget,
        pane_id: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    CreatePane {
        request: CreatePaneRequest,
        reply: mpsc::Sender<BridgeResult>,
    },
    BrowserOpenSplit {
        target: WorkspaceTarget,
        source_surface_id: Option<String>,
        url: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    BrowserNavigate {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        url: String,
        reply: mpsc::Sender<BridgeResult>,
    },
    BrowserUrlGet {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    BrowserTitleGet {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    BrowserGet {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        kind: String,
        selector: Option<String>,
        name: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    BrowserWait {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        selector: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    BrowserEval {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        script: String,
        reply: mpsc::Sender<BridgeResult>,
    },
    BrowserSnapshot {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    BrowserFind {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        locator: String,
        value: String,
        index: Option<usize>,
        reply: mpsc::Sender<BridgeResult>,
    },
    BrowserClick {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        selector: String,
        reply: mpsc::Sender<BridgeResult>,
    },
    BrowserFill {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        selector: String,
        text: String,
        reply: mpsc::Sender<BridgeResult>,
    },
    BrowserAction {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        action: String,
        selector: Option<String>,
        text: Option<String>,
        value: Option<String>,
        key: Option<String>,
        dy: Option<u64>,
        reply: mpsc::Sender<BridgeResult>,
    },
    BrowserData {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        action: String,
        name: Option<String>,
        key: Option<String>,
        value: Option<String>,
        storage_type: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    ListSurfaces {
        target: WorkspaceTarget,
        reply: mpsc::Sender<BridgeResult>,
    },
    SurfaceHealth {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    ReadSurfaceText {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    CreateWorkspace {
        name: Option<String>,
        cwd: Option<String>,
        command: Option<String>,
        reply: mpsc::Sender<BridgeResult>,
    },
    SelectWorkspace {
        target: WorkspaceTarget,
        reply: mpsc::Sender<BridgeResult>,
    },
    RenameWorkspace {
        target: WorkspaceTarget,
        title: String,
        reply: mpsc::Sender<BridgeResult>,
    },
    CloseWorkspace {
        target: WorkspaceTarget,
        reply: mpsc::Sender<BridgeResult>,
    },
    SendText {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        text: String,
        reply: mpsc::Sender<BridgeResult>,
    },
    SendKey {
        target: WorkspaceTarget,
        surface_hint: Option<String>,
        key: String,
        reply: mpsc::Sender<BridgeResult>,
    },
    /// Post a desktop-style notification into the sidebar + toast overlay.
    /// `target` chooses the workspace to flag as unread; if not provided,
    /// the currently-active workspace is used.
    CreateNotification {
        target: WorkspaceTarget,
        title: String,
        subtitle: String,
        body: String,
        reply: mpsc::Sender<BridgeResult>,
    },
}

impl ControlCommand {
    pub fn respond(self, result: BridgeResult) {
        match self {
            Self::Identify { reply, .. }
            | Self::CurrentWorkspace { reply }
            | Self::ListWorkspaces { reply }
            | Self::ListPanes { reply, .. }
            | Self::ListPaneSurfaces { reply, .. }
            | Self::CreatePane { reply, .. }
            | Self::BrowserOpenSplit { reply, .. }
            | Self::BrowserNavigate { reply, .. }
            | Self::BrowserUrlGet { reply, .. }
            | Self::BrowserTitleGet { reply, .. }
            | Self::BrowserGet { reply, .. }
            | Self::BrowserWait { reply, .. }
            | Self::BrowserEval { reply, .. }
            | Self::BrowserSnapshot { reply, .. }
            | Self::BrowserFind { reply, .. }
            | Self::BrowserClick { reply, .. }
            | Self::BrowserFill { reply, .. }
            | Self::BrowserAction { reply, .. }
            | Self::BrowserData { reply, .. }
            | Self::ListSurfaces { reply, .. }
            | Self::SurfaceHealth { reply, .. }
            | Self::ReadSurfaceText { reply, .. }
            | Self::CreateWorkspace { reply, .. }
            | Self::SelectWorkspace { reply, .. }
            | Self::RenameWorkspace { reply, .. }
            | Self::CloseWorkspace { reply, .. }
            | Self::SendText { reply, .. }
            | Self::SendKey { reply, .. }
            | Self::CreateNotification { reply, .. } => {
                let _ = reply.send(result);
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl BridgeError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS_CODE, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(NOT_FOUND_CODE, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(CONFLICT_CODE, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR_CODE, message)
    }
}

fn parse_request(input: &str) -> Result<V2Request, BridgeError> {
    if let Ok(request) = serde_json::from_str::<V2Request>(input) {
        return Ok(request);
    }

    match parse_v1_command_envelope(input) {
        Ok(v1) => Ok(v1.into_v2_request(None)),
        Err(error) => Err(BridgeError::new(
            PARSE_ERROR_CODE,
            format!("invalid request payload: {error}"),
        )
        .with_data(json!({ "raw": input }))),
    }
}

fn params_object(params: &Value) -> Result<&Map<String, Value>, BridgeError> {
    params
        .as_object()
        .ok_or_else(|| BridgeError::invalid_params("params must be a JSON object"))
}

fn optional_string(params: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        params
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn optional_raw_string(params: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        params
            .get(*key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn optional_handle(
    params: &Map<String, Value>,
    keys: &[&str],
) -> Result<Option<String>, BridgeError> {
    for key in keys {
        let Some(value) = params.get(*key) else {
            continue;
        };
        match value {
            Value::Null => {}
            Value::String(raw) => {
                let handle = raw.trim();
                if !handle.is_empty() {
                    return Ok(Some(handle.to_string()));
                }
            }
            Value::Number(number) => {
                let id = number.as_u64().ok_or_else(|| {
                    BridgeError::invalid_params(format!(
                        "{key} must be a non-negative integer or ref handle"
                    ))
                })?;
                return Ok(Some(id.to_string()));
            }
            _ => {
                return Err(BridgeError::invalid_params(format!(
                    "{key} must be a non-negative integer or ref handle"
                )));
            }
        }
    }
    Ok(None)
}

fn optional_ref_handle(
    params: &Map<String, Value>,
    keys: &[&str],
    prefix: &str,
) -> Result<Option<String>, BridgeError> {
    optional_handle(params, keys).map(|handle| {
        handle.map(|handle| {
            handle
                .strip_prefix(prefix)
                .unwrap_or(handle.as_str())
                .to_string()
        })
    })
}

fn optional_index(params: &Map<String, Value>, key: &str) -> Result<Option<usize>, BridgeError> {
    let Some(value) = params.get(key) else {
        return Ok(None);
    };

    if let Some(index) = value.as_u64() {
        return Ok(Some(index as usize));
    }

    Err(BridgeError::invalid_params(format!(
        "{key} must be a non-negative integer"
    )))
}

fn looks_like_workspace_handle(raw: &str) -> bool {
    let raw = raw.trim();
    if raw.starts_with("workspace:") {
        return true;
    }
    let value = raw;
    uuid::Uuid::parse_str(value).is_ok() || value.chars().all(|ch| ch.is_ascii_digit())
}

fn parse_optional_workspace_target(
    params: &Map<String, Value>,
    allow_name: bool,
) -> Result<WorkspaceTarget, BridgeError> {
    if let Some(handle) = optional_handle(params, &["workspace_id", "id"])? {
        if allow_name && !looks_like_workspace_handle(&handle) {
            return Ok(WorkspaceTarget::Name(handle));
        }
        return Ok(WorkspaceTarget::Handle(handle));
    }
    if allow_name {
        if let Some(name) = optional_string(params, &["name"]) {
            return Ok(WorkspaceTarget::Name(name));
        }
    }
    if let Some(index) = optional_index(params, "index")? {
        return Ok(WorkspaceTarget::Index(index));
    }
    Ok(WorkspaceTarget::Active)
}

#[cfg_attr(not(test), allow(dead_code))]
fn parse_create_pane_request(
    params: &Map<String, Value>,
) -> Result<CreatePaneRequest, BridgeError> {
    let direction = match optional_string(params, &["direction"])
        .unwrap_or_else(|| "right".to_string())
        .as_str()
    {
        "left" => PaneCreateDirection::Left,
        "right" => PaneCreateDirection::Right,
        "up" => PaneCreateDirection::Up,
        "down" => PaneCreateDirection::Down,
        _ => {
            return Err(BridgeError::invalid_params(
                "pane.create direction must be one of left|right|up|down",
            ));
        }
    };

    let pane_type = match optional_string(params, &["type"])
        .unwrap_or_else(|| "terminal".to_string())
        .as_str()
    {
        "terminal" => PaneCreateType::Terminal,
        "browser" => PaneCreateType::Browser,
        _ => {
            return Err(BridgeError::invalid_params(
                "pane.create type must be one of terminal|browser",
            ));
        }
    };

    if matches!(pane_type, PaneCreateType::Browser) {
        return Err(BridgeError::invalid_params(
            "pane.create live GTK bridge supports type=terminal only",
        ));
    }
    if optional_string(params, &["url"]).is_some() {
        return Err(BridgeError::invalid_params(
            "pane.create url is only supported for browser panes",
        ));
    }

    Ok(CreatePaneRequest {
        target: parse_optional_workspace_target(params, true)?,
        source_pane_id: optional_ref_handle(params, &["pane_id"], "pane:")?,
        source_surface_id: optional_ref_handle(params, &["surface_id"], "surface:")?,
        direction,
        pane_type,
        command: optional_string(params, &["command"]),
    })
}

fn parse_required_workspace_target(
    params: &Map<String, Value>,
    allow_name: bool,
    method: &str,
) -> Result<WorkspaceTarget, BridgeError> {
    let target = parse_optional_workspace_target(params, allow_name)?;
    if matches!(target, WorkspaceTarget::Active) {
        Err(BridgeError::invalid_params(format!(
            "{method} requires workspace_id/id, name, or index"
        )))
    } else {
        Ok(target)
    }
}

fn handle_method(
    id: Option<Value>,
    method: &str,
    params: Value,
    dispatch: &dyn Fn(ControlCommand),
) -> V2Response {
    let params = match params_object(&params) {
        Ok(params) => params,
        Err(error) => return error_response(id, error),
    };

    let queued = match method {
        "system.ping" | "ping" => return V2Response::success(id, json!({ "pong": true })),
        "system.capabilities" => {
            return V2Response::success(id, json!({ "commands": METHODS, "methods": METHODS }));
        }
        "system.identify" => {
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::Identify {
                    caller: params.get("caller").cloned(),
                    reply,
                },
                rx,
            )
        }
        "workspace.current" => {
            let (reply, rx) = mpsc::channel();
            (ControlCommand::CurrentWorkspace { reply }, rx)
        }
        "workspace.list" | "list-workspaces" => {
            let (reply, rx) = mpsc::channel();
            (ControlCommand::ListWorkspaces { reply }, rx)
        }
        "pane.list" | "list-panes" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (ControlCommand::ListPanes { target, reply }, rx)
        }
        "pane.surfaces" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::ListPaneSurfaces {
                    target,
                    pane_id: optional_string(params, &["pane_id", "id"]),
                    reply,
                },
                rx,
            )
        }
        "pane.create" | "new-pane" => {
            let request = match parse_create_pane_request(params) {
                Ok(request) => request,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (ControlCommand::CreatePane { request, reply }, rx)
        }
        "browser.open_split" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let source_surface_id =
                match optional_ref_handle(params, &["surface_id", "id"], "surface:") {
                    Ok(surface_hint) => surface_hint,
                    Err(error) => return error_response(id, error),
                };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::BrowserOpenSplit {
                    target,
                    source_surface_id,
                    url: optional_string(params, &["url"]),
                    reply,
                },
                rx,
            )
        }
        "browser.navigate" => {
            let Some(url) = optional_string(params, &["url"]) else {
                return error_response(
                    id,
                    BridgeError::invalid_params("browser.navigate requires url"),
                );
            };
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "id"], "surface:") {
                    Ok(surface_hint) => surface_hint,
                    Err(error) => return error_response(id, error),
                };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::BrowserNavigate {
                    target,
                    surface_hint,
                    url,
                    reply,
                },
                rx,
            )
        }
        "browser.url.get" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "id"], "surface:") {
                    Ok(surface_hint) => surface_hint,
                    Err(error) => return error_response(id, error),
                };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::BrowserUrlGet {
                    target,
                    surface_hint,
                    reply,
                },
                rx,
            )
        }
        "browser.get.title" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "id"], "surface:") {
                    Ok(surface_hint) => surface_hint,
                    Err(error) => return error_response(id, error),
                };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::BrowserTitleGet {
                    target,
                    surface_hint,
                    reply,
                },
                rx,
            )
        }
        method if method.starts_with("browser.get.") => {
            let kind = method.trim_start_matches("browser.get.").trim().to_string();
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "id"], "surface:") {
                    Ok(surface_hint) => surface_hint,
                    Err(error) => return error_response(id, error),
                };
            let selector = optional_string(params, &["selector"]);
            let name = optional_string(params, &["name", "property"]);
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::BrowserGet {
                    target,
                    surface_hint,
                    kind,
                    selector,
                    name,
                    reply,
                },
                rx,
            )
        }
        "browser.wait" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "id"], "surface:") {
                    Ok(surface_hint) => surface_hint,
                    Err(error) => return error_response(id, error),
                };
            let selector = optional_string(params, &["selector"]);
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::BrowserWait {
                    target,
                    surface_hint,
                    selector,
                    reply,
                },
                rx,
            )
        }
        "browser.eval" => {
            let Some(script) = optional_string(params, &["script"]) else {
                return error_response(
                    id,
                    BridgeError::invalid_params("browser.eval requires script"),
                );
            };
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "id"], "surface:") {
                    Ok(surface_hint) => surface_hint,
                    Err(error) => return error_response(id, error),
                };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::BrowserEval {
                    target,
                    surface_hint,
                    script,
                    reply,
                },
                rx,
            )
        }
        "browser.snapshot" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "id"], "surface:") {
                    Ok(surface_hint) => surface_hint,
                    Err(error) => return error_response(id, error),
                };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::BrowserSnapshot {
                    target,
                    surface_hint,
                    reply,
                },
                rx,
            )
        }
        method if method.starts_with("browser.find.") => {
            let locator = method
                .trim_start_matches("browser.find.")
                .trim()
                .to_string();
            let value = match locator.as_str() {
                "role" => optional_string(params, &["role", "name"]),
                "first" | "last" | "nth" => optional_string(params, &["selector"]),
                other => optional_string(params, &[other]),
            };
            let Some(value) = value else {
                return error_response(
                    id,
                    BridgeError::invalid_params(format!(
                        "browser.find.{locator} requires a locator value"
                    )),
                );
            };
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "id"], "surface:") {
                    Ok(surface_hint) => surface_hint,
                    Err(error) => return error_response(id, error),
                };
            let index = match optional_index(params, "index") {
                Ok(index) => index,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::BrowserFind {
                    target,
                    surface_hint,
                    locator,
                    value,
                    index,
                    reply,
                },
                rx,
            )
        }
        "browser.click" => {
            let Some(selector) = optional_string(params, &["selector"]) else {
                return error_response(
                    id,
                    BridgeError::invalid_params("browser.click requires selector"),
                );
            };
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "id"], "surface:") {
                    Ok(surface_hint) => surface_hint,
                    Err(error) => return error_response(id, error),
                };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::BrowserClick {
                    target,
                    surface_hint,
                    selector,
                    reply,
                },
                rx,
            )
        }
        "browser.fill" => {
            let Some(selector) = optional_string(params, &["selector"]) else {
                return error_response(
                    id,
                    BridgeError::invalid_params("browser.fill requires selector"),
                );
            };
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "id"], "surface:") {
                    Ok(surface_hint) => surface_hint,
                    Err(error) => return error_response(id, error),
                };
            let text = optional_raw_string(params, &["value", "text"]).unwrap_or_default();
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::BrowserFill {
                    target,
                    surface_hint,
                    selector,
                    text,
                    reply,
                },
                rx,
            )
        }
        "browser.type"
        | "browser.check"
        | "browser.uncheck"
        | "browser.select"
        | "browser.focus"
        | "browser.hover"
        | "browser.dblclick"
        | "browser.scroll"
        | "browser.scroll_into_view"
        | "browser.press"
        | "browser.keydown"
        | "browser.keyup" => {
            let action = method.trim_start_matches("browser.").to_string();
            let selector = optional_string(params, &["selector"]);
            let key = optional_string(params, &["key"]);
            let text = optional_raw_string(params, &["text"]);
            let value = optional_raw_string(params, &["value"]);
            let dy = match optional_index(params, "dy") {
                Ok(Some(value)) => Some(value as u64),
                Ok(None) => match optional_index(params, "amount") {
                    Ok(amount) => amount.map(|value| value as u64),
                    Err(error) => return error_response(id, error),
                },
                Err(error) => return error_response(id, error),
            };
            let needs_selector = matches!(
                action.as_str(),
                "type"
                    | "check"
                    | "uncheck"
                    | "select"
                    | "focus"
                    | "hover"
                    | "dblclick"
                    | "scroll_into_view"
            );
            if needs_selector && selector.is_none() {
                return error_response(
                    id,
                    BridgeError::invalid_params(format!("browser.{action} requires selector")),
                );
            }
            if action == "type" && text.is_none() {
                return error_response(id, BridgeError::invalid_params("browser.type requires text"));
            }
            if action == "select" && value.is_none() {
                return error_response(
                    id,
                    BridgeError::invalid_params("browser.select requires value"),
                );
            }
            if matches!(action.as_str(), "press" | "keydown" | "keyup") && key.is_none() {
                return error_response(
                    id,
                    BridgeError::invalid_params(format!("browser.{action} requires key")),
                );
            }
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "id"], "surface:") {
                    Ok(surface_hint) => surface_hint,
                    Err(error) => return error_response(id, error),
                };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::BrowserAction {
                    target,
                    surface_hint,
                    action,
                    selector,
                    text,
                    value,
                    key,
                    dy,
                    reply,
                },
                rx,
            )
        }
        "browser.cookies.get"
        | "browser.cookies.set"
        | "browser.cookies.clear"
        | "browser.storage.get"
        | "browser.storage.set"
        | "browser.storage.clear" => {
            let action = method.trim_start_matches("browser.").to_string();
            let name = optional_string(params, &["name"]);
            let key = optional_string(params, &["key"]);
            let value = optional_raw_string(params, &["value"]);
            let storage_type = optional_string(params, &["type", "storage_type"]);
            if action == "cookies.set" && name.is_none() {
                return error_response(
                    id,
                    BridgeError::invalid_params("browser.cookies.set requires name"),
                );
            }
            if action == "cookies.set" && value.is_none() {
                return error_response(
                    id,
                    BridgeError::invalid_params("browser.cookies.set requires value"),
                );
            }
            if matches!(action.as_str(), "storage.get" | "storage.set") && key.is_none() {
                return error_response(
                    id,
                    BridgeError::invalid_params(format!("browser.{action} requires key")),
                );
            }
            if action == "storage.set" && value.is_none() {
                return error_response(
                    id,
                    BridgeError::invalid_params("browser.storage.set requires value"),
                );
            }
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint =
                match optional_ref_handle(params, &["surface_id", "id"], "surface:") {
                    Ok(surface_hint) => surface_hint,
                    Err(error) => return error_response(id, error),
                };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::BrowserData {
                    target,
                    surface_hint,
                    action,
                    name,
                    key,
                    value,
                    storage_type,
                    reply,
                },
                rx,
            )
        }
        "surface.list" | "list-panels" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (ControlCommand::ListSurfaces { target, reply }, rx)
        }
        "surface.health" | "surface-health" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint = match optional_ref_handle(params, &["surface_id", "id"], "surface:")
            {
                Ok(surface_hint) => surface_hint,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::SurfaceHealth {
                    target,
                    surface_hint,
                    reply,
                },
                rx,
            )
        }
        "surface.read_text" | "read-screen" | "capture-pane" => {
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let surface_hint = match optional_ref_handle(params, &["surface_id", "id"], "surface:")
            {
                Ok(surface_hint) => surface_hint,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::ReadSurfaceText {
                    target,
                    surface_hint,
                    reply,
                },
                rx,
            )
        }
        "workspace.create" | "new-workspace" => {
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::CreateWorkspace {
                    name: optional_string(params, &["name", "title"]),
                    cwd: optional_string(params, &["cwd"]),
                    command: optional_string(params, &["command"]),
                    reply,
                },
                rx,
            )
        }
        "workspace.select" | "workspace.activate" | "activate-workspace" => {
            let target = match parse_required_workspace_target(params, true, method) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (ControlCommand::SelectWorkspace { target, reply }, rx)
        }
        "workspace.rename" | "rename-workspace" => {
            let Some(title) = optional_string(params, &["title", "name"]) else {
                return error_response(
                    id,
                    BridgeError::invalid_params("workspace.rename requires title/name"),
                );
            };
            let target = match parse_optional_workspace_target(params, false) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::RenameWorkspace {
                    target,
                    title,
                    reply,
                },
                rx,
            )
        }
        "workspace.close" | "close-workspace" => {
            let target = match parse_optional_workspace_target(params, false) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (ControlCommand::CloseWorkspace { target, reply }, rx)
        }
        "surface.send_text" | "send-text" | "send" => {
            let Some(text) = optional_string(params, &["text"]) else {
                return error_response(
                    id,
                    BridgeError::invalid_params("surface.send_text requires text"),
                );
            };
            // allow_name = true: lets agent-team peers address each other by
            // workspace name (e.g. `--workspace codex`) instead of UUID.
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::SendText {
                    target,
                    surface_hint: optional_string(params, &["surface_id"]),
                    text,
                    reply,
                },
                rx,
            )
        }
        "surface.send_key" | "send-key" => {
            let Some(key) = optional_string(params, &["key"]) else {
                return error_response(
                    id,
                    BridgeError::invalid_params("surface.send_key requires key"),
                );
            };
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::SendKey {
                    target,
                    surface_hint: optional_string(params, &["surface_id"]),
                    key,
                    reply,
                },
                rx,
            )
        }
        "notification.create" | "notify" => {
            // Title is required; subtitle and body are optional. This mirrors
            // cmux notify's shape (title/subtitle/body) and maps onto the
            // existing sidebar unread pipeline.
            let Some(title) = optional_string(params, &["title"]) else {
                return error_response(
                    id,
                    BridgeError::invalid_params("notification.create requires title"),
                );
            };
            let subtitle = optional_string(params, &["subtitle"]).unwrap_or_default();
            let body = optional_string(params, &["body", "message"]).unwrap_or_default();
            // allow_name = true: lets agent hooks target a peer by name.
            let target = match parse_optional_workspace_target(params, true) {
                Ok(target) => target,
                Err(error) => return error_response(id, error),
            };
            let (reply, rx) = mpsc::channel();
            (
                ControlCommand::CreateNotification {
                    target,
                    title,
                    subtitle,
                    body,
                    reply,
                },
                rx,
            )
        }
        _ => {
            return error_response(
                id,
                BridgeError::new(UNKNOWN_METHOD_CODE, format!("unknown method: {method}")),
            );
        }
    };

    let (command, reply_rx) = queued;

    dispatch(command);

    match reply_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(result)) => V2Response::success(id, result),
        Ok(Err(error)) => error_response(id, error),
        Err(_) => error_response(id, BridgeError::internal("control command timed out")),
    }
}

fn error_response(id: Option<Value>, error: BridgeError) -> V2Response {
    V2Response::error(id, error.code, error.message, error.data)
}

fn dispatch_request(input: &str, dispatch: &dyn Fn(ControlCommand)) -> V2Response {
    match parse_request(input) {
        Ok(request) => handle_method(request.id, &request.method, request.params, dispatch),
        Err(error) => error_response(None, error),
    }
}

fn handle_client(
    stream: UnixStream,
    dispatch: &(dyn Fn(ControlCommand) + Send + Sync + 'static),
) -> io::Result<()> {
    stream.set_read_timeout(Some(request_io::CLIENT_IDLE_TIMEOUT))?;
    let reader_stream = stream.try_clone()?;
    reader_stream.set_read_timeout(Some(request_io::CLIENT_IDLE_TIMEOUT))?;
    let mut reader = io::BufReader::new(reader_stream);
    let mut writer = stream;
    let mut line_buf = Vec::with_capacity(4096);

    loop {
        if !read_request_frame(&mut reader, &mut line_buf)? {
            return Ok(());
        }

        let input = std::str::from_utf8(&line_buf)
            .map(|line| line.trim_end_matches(['\n', '\r']))
            .unwrap_or("");
        if input.is_empty() {
            continue;
        }

        let response = dispatch_request(input, dispatch);
        let mut payload = serde_json::to_string(&response)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        payload.push('\n');
        writer.write_all(payload.as_bytes())?;
        writer.flush()?;
    }
}

struct ConnectionSlot {
    active_connections: Arc<AtomicUsize>,
}

impl ConnectionSlot {
    fn try_acquire(active_connections: Arc<AtomicUsize>) -> Option<Self> {
        active_connections
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < request_io::MAX_CONNECTIONS).then_some(current + 1)
            })
            .ok()?;
        Some(Self { active_connections })
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.active_connections.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Start the control socket server in a background thread and dispatch each
/// command onto the GTK main context.
pub fn start(dispatch: fn(ControlCommand)) {
    let context = glib::MainContext::default();
    let dispatch = std::sync::Arc::new(move |command: ControlCommand| {
        context.invoke(move || dispatch(command));
    });

    std::thread::Builder::new()
        .name("limux-control".into())
        .spawn(move || {
            let path = resolve_socket_path(None, SocketMode::Runtime);
            let control_mode = SocketControlMode::from_env();
            let listener = match bind_listener(
                &path,
                SocketMode::Runtime,
                control_mode.requires_owner_only_socket(),
            ) {
                Ok(listener) => listener,
                Err(error) => {
                    eprintln!(
                        "limux: control socket bind failed ({}): {error}",
                        path.display()
                    );
                    return;
                }
            };

            eprintln!("limux: control socket at {}", path.display());
            let active_connections = Arc::new(AtomicUsize::new(0));

            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let Some(slot) = ConnectionSlot::try_acquire(active_connections.clone()) else {
                            eprintln!("limux: rejecting control client, too many active connections");
                            continue;
                        };
                        let peer = match auth::authorize_peer(&stream, control_mode) {
                            Ok(peer) => peer,
                            Err(error) => {
                                eprintln!("limux: rejected control client: {error}");
                                continue;
                            }
                        };
                        let dispatch = dispatch.clone();
                        std::thread::Builder::new()
                            .name("limux-ctrl-conn".into())
                            .spawn(move || {
                                let _slot = slot;
                                if let Err(error) = handle_client(stream, dispatch.as_ref()) {
                                    eprintln!(
                                        "limux: control connection error for pid={} uid={}: {error}",
                                        peer.pid, peer.uid
                                    );
                                }
                            })
                            .ok();
                    }
                    Err(error) => {
                        eprintln!("limux: control accept error: {error}");
                    }
                }
            }
        })
        .expect("failed to spawn control server thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v2_request_directly() {
        let request = parse_request(r#"{"id":"1","method":"system.ping","params":{}}"#)
            .expect("v2 request should parse");
        assert_eq!(request.id, Some(Value::String("1".to_string())));
        assert_eq!(request.method, "system.ping");
    }

    #[test]
    fn parses_v1_request_envelope() {
        let request = parse_request(r#"{"command":"workspace.create","args":{"cwd":"/tmp"}}"#)
            .expect("v1 request should parse");
        assert_eq!(request.method, "workspace.create");
        assert_eq!(request.params["cwd"], "/tmp");
    }

    #[test]
    fn workspace_target_prefers_handle_over_index() {
        let params = json!({
            "workspace_id": "workspace:abc",
            "index": 2
        });
        let target =
            parse_optional_workspace_target(params.as_object().expect("object params"), true)
                .expect("target should parse");
        assert_eq!(target, WorkspaceTarget::Handle("workspace:abc".to_string()));
    }

    #[test]
    fn workspace_target_treats_cli_workspace_id_as_name_when_allowed() {
        let params = json!({
            "workspace_id": "claude"
        });
        let target =
            parse_optional_workspace_target(params.as_object().expect("object params"), true)
                .expect("target should parse");
        assert_eq!(target, WorkspaceTarget::Name("claude".to_string()));
    }

    #[test]
    fn workspace_target_preserves_raw_uuid_workspace_ids_when_names_are_allowed() {
        let workspace_id = "2b8b5ca4-0200-4433-9f7c-d5c9f725be50";
        let params = json!({
            "workspace_id": workspace_id
        });
        let target =
            parse_optional_workspace_target(params.as_object().expect("object params"), true)
                .expect("target should parse");
        assert_eq!(target, WorkspaceTarget::Handle(workspace_id.to_string()));
    }

    #[test]
    fn workspace_select_requires_explicit_target() {
        let params = Map::new();
        let error = parse_required_workspace_target(&params, true, "workspace.select")
            .expect_err("workspace.select should require a target");
        assert_eq!(error.code, INVALID_PARAMS_CODE);
    }

    #[test]
    fn pane_create_contract_accepts_raw_and_ref_targets() {
        let params = json!({
            "workspace_id": 7,
            "surface_id": "surface:11",
            "pane_id": "pane:12",
            "direction": "left",
            "type": "terminal",
            "command": "claude"
        });
        let request = parse_create_pane_request(params.as_object().expect("object params"))
            .expect("pane.create request should parse");

        assert_eq!(request.target, WorkspaceTarget::Handle("7".to_string()));
        assert_eq!(request.source_surface_id, Some("11".to_string()));
        assert_eq!(request.source_pane_id, Some("12".to_string()));
        assert_eq!(request.direction, PaneCreateDirection::Left);
        assert_eq!(request.pane_type, PaneCreateType::Terminal);
        assert_eq!(request.command, Some("claude".to_string()));
    }

    #[test]
    fn pane_create_contract_rejects_invalid_direction_and_type() {
        let bad_direction = json!({ "direction": "diagonal" });
        let error = parse_create_pane_request(bad_direction.as_object().expect("object params"))
            .expect_err("invalid direction should fail");
        assert_eq!(error.code, INVALID_PARAMS_CODE);

        let bad_type = json!({ "type": "webview" });
        let error = parse_create_pane_request(bad_type.as_object().expect("object params"))
            .expect_err("invalid type should fail");
        assert_eq!(error.code, INVALID_PARAMS_CODE);
    }

    #[test]
    fn pane_create_contract_rejects_deferred_browser_fields() {
        let browser = json!({ "type": "browser" });
        let error = parse_create_pane_request(browser.as_object().expect("object params"))
            .expect_err("browser panes are deferred");
        assert_eq!(error.code, INVALID_PARAMS_CODE);

        let url = json!({ "url": "https://example.com" });
        let error = parse_create_pane_request(url.as_object().expect("object params"))
            .expect_err("url is browser-only");
        assert_eq!(error.code, INVALID_PARAMS_CODE);
    }

    #[test]
    fn pane_create_route_queues_create_pane_command() {
        let response = dispatch_request(
            r#"{"id":1,"method":"pane.create","params":{"name":"claude","surface_id":"surface:4:tab","direction":"down","command":"codex"}}"#,
            &|command| match command {
                ControlCommand::CreatePane { request, reply } => {
                    assert_eq!(request.target, WorkspaceTarget::Name("claude".to_string()));
                    assert_eq!(request.source_surface_id, Some("4:tab".to_string()));
                    assert_eq!(request.direction, PaneCreateDirection::Down);
                    assert_eq!(request.command, Some("codex".to_string()));
                    let _ = reply.send(Ok(json!({
                        "pane_id": "9",
                        "pane_ref": "pane:9",
                        "surface_id": "9:tab",
                        "surface_ref": "surface:9:tab"
                    })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        let result = response.result.expect("pane.create should return a result");
        assert_eq!(result["pane_ref"], "pane:9");
        assert_eq!(result["surface_ref"], "surface:9:tab");
    }

    #[test]
    fn pane_create_route_rejects_invalid_params_before_dispatch() {
        let response = dispatch_request(
            r#"{"id":1,"method":"new-pane","params":{"direction":"diagonal"}}"#,
            &|command| panic!("invalid pane.create should not dispatch: {command:?}"),
        );

        assert_eq!(response.result, None);
        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    fn browser_routes_queue_live_bridge_commands() {
        let open_response = dispatch_request(
            r#"{"id":1,"method":"browser.open_split","params":{"workspace_id":"codex","surface_id":"surface:4:tab","url":"https://example.com"}}"#,
            &|command| match command {
                ControlCommand::BrowserOpenSplit {
                    target,
                    source_surface_id,
                    url,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(source_surface_id, Some("4:tab".to_string()));
                    assert_eq!(url, Some("https://example.com".to_string()));
                    let _ = reply.send(Ok(json!({ "ok": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(open_response.error, None);

        let navigate_response = dispatch_request(
            r#"{"id":2,"method":"browser.navigate","params":{"surface_id":"surface:9:tab","url":"https://cmux.dev"}}"#,
            &|command| match command {
                ControlCommand::BrowserNavigate {
                    target,
                    surface_hint,
                    url,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Active);
                    assert_eq!(surface_hint, Some("9:tab".to_string()));
                    assert_eq!(url, "https://cmux.dev");
                    let _ = reply.send(Ok(json!({ "ok": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(navigate_response.error, None);
    }

    #[test]
    fn browser_navigate_requires_url_before_dispatch() {
        let response = dispatch_request(
            r#"{"id":1,"method":"browser.navigate","params":{"surface_id":"surface:9:tab"}}"#,
            &|command| panic!("invalid browser.navigate should not dispatch: {command:?}"),
        );

        assert_eq!(response.result, None);
        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );
    }

    #[test]
    fn browser_get_and_wait_routes_accept_surface_refs() {
        let get_response = dispatch_request(
            r#"{"id":1,"method":"browser.get.attr","params":{"surface_id":"surface:9:tab","selector":"#name","name":"aria-label"}}"#,
            &|command| match command {
                ControlCommand::BrowserGet {
                    target,
                    surface_hint,
                    kind,
                    selector,
                    name,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Active);
                    assert_eq!(surface_hint, Some("9:tab".to_string()));
                    assert_eq!(kind, "attr");
                    assert_eq!(selector, Some("#name".to_string()));
                    assert_eq!(name, Some("aria-label".to_string()));
                    let _ = reply.send(Ok(json!({ "value": "Name" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(get_response.error, None);
        assert_eq!(get_response.result.expect("result")["value"], "Name");

        let wait_response = dispatch_request(
            r#"{"id":2,"method":"browser.wait","params":{"surface_id":"surface:9:tab","selector":"#ready"}}"#,
            &|command| match command {
                ControlCommand::BrowserWait {
                    target,
                    surface_hint,
                    selector,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Active);
                    assert_eq!(surface_hint, Some("9:tab".to_string()));
                    assert_eq!(selector, Some("#ready".to_string()));
                    let _ = reply.send(Ok(json!({ "ok": true, "ready": true })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(wait_response.error, None);
        assert_eq!(wait_response.result.expect("result")["ready"], true);
    }

    #[test]
    fn browser_eval_route_requires_script_and_accepts_surface_refs() {
        let missing_script = dispatch_request(
            r#"{"id":1,"method":"browser.eval","params":{"surface_id":"surface:9:tab"}}"#,
            &|command| panic!("invalid browser.eval should not dispatch: {command:?}"),
        );
        assert_eq!(missing_script.result, None);
        assert_eq!(
            missing_script.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );

        let response = dispatch_request(
            r#"{"id":2,"method":"browser.eval","params":{"surface_id":"surface:9:tab","script":"document.title"}}"#,
            &|command| match command {
                ControlCommand::BrowserEval {
                    target,
                    surface_hint,
                    script,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Active);
                    assert_eq!(surface_hint, Some("9:tab".to_string()));
                    assert_eq!(script, "document.title");
                    let _ = reply.send(Ok(json!({ "value": "Limux" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(response.result.expect("result")["value"], "Limux");
    }

    #[test]
    fn browser_snapshot_route_accepts_surface_refs() {
        let response = dispatch_request(
            r#"{"id":1,"method":"browser.snapshot","params":{"surface_id":"surface:9:tab"}}"#,
            &|command| match command {
                ControlCommand::BrowserSnapshot {
                    target,
                    surface_hint,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Active);
                    assert_eq!(surface_hint, Some("9:tab".to_string()));
                    let _ = reply.send(Ok(json!({
                        "snapshot": "- document \"Limux\"",
                        "text": "Limux",
                    })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(response.result.expect("result")["text"], "Limux");
    }

    #[test]
    fn browser_find_routes_require_locator_values_and_accept_surface_refs() {
        let missing_value = dispatch_request(
            r#"{"id":1,"method":"browser.find.text","params":{"surface_id":"surface:9:tab"}}"#,
            &|command| panic!("invalid browser.find should not dispatch: {command:?}"),
        );
        assert_eq!(missing_value.result, None);
        assert_eq!(
            missing_value.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );

        let response = dispatch_request(
            r#"{"id":2,"method":"browser.find.nth","params":{"surface_id":"surface:9:tab","selector":"button","index":2}}"#,
            &|command| match command {
                ControlCommand::BrowserFind {
                    target,
                    surface_hint,
                    locator,
                    value,
                    index,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Active);
                    assert_eq!(surface_hint, Some("9:tab".to_string()));
                    assert_eq!(locator, "nth");
                    assert_eq!(value, "button");
                    assert_eq!(index, Some(2));
                    let _ = reply.send(Ok(json!({ "element_ref": "@e2", "selector": "button:nth-of-type(3)" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(response.result.expect("result")["element_ref"], "@e2");
    }

    #[test]
    fn browser_action_routes_require_selectors_and_accept_surface_refs() {
        let missing_selector = dispatch_request(
            r#"{"id":1,"method":"browser.click","params":{"surface_id":"surface:9:tab"}}"#,
            &|command| panic!("invalid browser.click should not dispatch: {command:?}"),
        );
        assert_eq!(missing_selector.result, None);
        assert_eq!(
            missing_selector.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );

        let click_response = dispatch_request(
            r#"{"id":2,"method":"browser.click","params":{"surface_id":"surface:9:tab","selector":"#submit"}}"#,
            &|command| match command {
                ControlCommand::BrowserClick {
                    target,
                    surface_hint,
                    selector,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Active);
                    assert_eq!(surface_hint, Some("9:tab".to_string()));
                    assert_eq!(selector, "#submit");
                    let _ = reply.send(Ok(json!({ "ok": true, "selector": selector })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(click_response.error, None);
        assert_eq!(click_response.result.expect("result")["selector"], "#submit");

        let fill_response = dispatch_request(
            r#"{"id":3,"method":"browser.fill","params":{"surface_id":"surface:9:tab","selector":"#name","text":"Ada"}}"#,
            &|command| match command {
                ControlCommand::BrowserFill {
                    target,
                    surface_hint,
                    selector,
                    text,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Active);
                    assert_eq!(surface_hint, Some("9:tab".to_string()));
                    assert_eq!(selector, "#name");
                    assert_eq!(text, "Ada");
                    let _ = reply.send(Ok(json!({ "ok": true, "selector": selector, "value": text })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(fill_response.error, None);
        assert_eq!(fill_response.result.expect("result")["value"], "Ada");
    }

    #[test]
    fn browser_action_routes_require_required_fields_and_accept_surface_refs() {
        let missing_text = dispatch_request(
            r#"{"id":1,"method":"browser.type","params":{"surface_id":"surface:9:tab","selector":"#name"}}"#,
            &|command| panic!("invalid browser.type should not dispatch: {command:?}"),
        );
        assert_eq!(missing_text.result, None);
        assert_eq!(
            missing_text.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );

        let type_response = dispatch_request(
            r#"{"id":2,"method":"browser.type","params":{"surface_id":"surface:9:tab","selector":"#name","text":" Ada"}}"#,
            &|command| match command {
                ControlCommand::BrowserAction {
                    target,
                    surface_hint,
                    action,
                    selector,
                    text,
                    value,
                    key,
                    dy,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Active);
                    assert_eq!(surface_hint, Some("9:tab".to_string()));
                    assert_eq!(action, "type");
                    assert_eq!(selector, Some("#name".to_string()));
                    assert_eq!(text, Some(" Ada".to_string()));
                    assert_eq!(value, None);
                    assert_eq!(key, None);
                    assert_eq!(dy, None);
                    let _ = reply.send(Ok(json!({ "ok": true, "selector": selector, "text": text })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(type_response.error, None);
        assert_eq!(type_response.result.expect("result")["text"], " Ada");

        let key_response = dispatch_request(
            r#"{"id":3,"method":"browser.press","params":{"surface_id":"surface:9:tab","key":"Enter"}}"#,
            &|command| match command {
                ControlCommand::BrowserAction { action, key, reply, .. } => {
                    assert_eq!(action, "press");
                    assert_eq!(key, Some("Enter".to_string()));
                    let _ = reply.send(Ok(json!({ "ok": true, "key": key })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(key_response.error, None);
        assert_eq!(key_response.result.expect("result")["key"], "Enter");
    }

    #[test]
    fn browser_data_routes_require_required_fields_and_accept_surface_refs() {
        let missing_key = dispatch_request(
            r#"{"id":1,"method":"browser.storage.get","params":{"surface_id":"surface:9:tab"}}"#,
            &|command| panic!("invalid browser.storage.get should not dispatch: {command:?}"),
        );
        assert_eq!(missing_key.result, None);
        assert_eq!(
            missing_key.error.as_ref().map(|error| error.code),
            Some(INVALID_PARAMS_CODE)
        );

        let cookie_response = dispatch_request(
            r#"{"id":2,"method":"browser.cookies.set","params":{"surface_id":"surface:9:tab","name":"sid","value":"abc 123"}}"#,
            &|command| match command {
                ControlCommand::BrowserData {
                    target,
                    surface_hint,
                    action,
                    name,
                    key,
                    value,
                    storage_type,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Active);
                    assert_eq!(surface_hint, Some("9:tab".to_string()));
                    assert_eq!(action, "cookies.set");
                    assert_eq!(name, Some("sid".to_string()));
                    assert_eq!(key, None);
                    assert_eq!(value, Some("abc 123".to_string()));
                    assert_eq!(storage_type, None);
                    let _ = reply.send(Ok(json!({ "ok": true, "name": name, "value": value })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(cookie_response.error, None);
        assert_eq!(cookie_response.result.expect("result")["name"], "sid");

        let storage_response = dispatch_request(
            r#"{"id":3,"method":"browser.storage.get","params":{"surface_id":"surface:9:tab","type":"session","key":"token"}}"#,
            &|command| match command {
                ControlCommand::BrowserData {
                    action,
                    key,
                    storage_type,
                    reply,
                    ..
                } => {
                    assert_eq!(action, "storage.get");
                    assert_eq!(key, Some("token".to_string()));
                    assert_eq!(storage_type, Some("session".to_string()));
                    let _ = reply.send(Ok(json!({ "value": "secret" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );
        assert_eq!(storage_response.error, None);
        assert_eq!(storage_response.result.expect("result")["value"], "secret");
    }

    #[test]
    fn browser_get_routes_accept_surface_refs() {
        let response = dispatch_request(
            r#"{"id":1,"method":"browser.url.get","params":{"surface_id":"surface:9:tab"}}"#,
            &|command| match command {
                ControlCommand::BrowserUrlGet {
                    target,
                    surface_hint,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Active);
                    assert_eq!(surface_hint, Some("9:tab".to_string()));
                    let _ = reply.send(Ok(json!({ "url": "https://example.com" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(response.result.expect("result")["url"], "https://example.com");
    }

    #[test]
    fn surface_health_route_accepts_surface_refs() {
        let response = dispatch_request(
            r#"{"id":1,"method":"surface.health","params":{"workspace_id":"codex","surface_id":"surface:4:tab"}}"#,
            &|command| match command {
                ControlCommand::SurfaceHealth {
                    target,
                    surface_hint,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Name("codex".to_string()));
                    assert_eq!(surface_hint, Some("4:tab".to_string()));
                    let _ = reply.send(Ok(json!({ "surfaces": [] })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert!(response.result.is_some());
    }

    #[test]
    fn read_text_route_accepts_capture_alias_and_surface_refs() {
        let response = dispatch_request(
            r#"{"id":1,"method":"capture-pane","params":{"surface_id":"surface:9:tab"}}"#,
            &|command| match command {
                ControlCommand::ReadSurfaceText {
                    target,
                    surface_hint,
                    reply,
                } => {
                    assert_eq!(target, WorkspaceTarget::Active);
                    assert_eq!(surface_hint, Some("9:tab".to_string()));
                    let _ = reply.send(Ok(json!({ "text": "ready" })));
                }
                other => panic!("unexpected command: {other:?}"),
            },
        );

        assert_eq!(response.error, None);
        assert_eq!(response.result.expect("result")["text"], "ready");
    }
}
