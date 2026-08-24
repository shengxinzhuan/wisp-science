//! Loopback bridge to a user-installed Chrome/Chromium extension.
//!
//! The extension runs inside the user's existing browser profile, so page
//! execution keeps that profile's cookies, login state, extensions, GPU, and
//! browser fingerprint. Wisp never launches a separate automation browser.
//!
//! Design acknowledgement: this bridge is inspired by GenericAgent's GA Web /
//! TMWebDriver real-browser architecture and compatible loopback protocol:
//! https://github.com/lsdefine/GenericAgent (MIT, Copyright 2025 lsdefine).
//! This module is Wisp's independent Rust implementation; see
//! `browser-extension/NOTICE.md` for provenance details.

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_hdr_async, WebSocketStream};
use uuid::Uuid;
use wisp_llm::ToolSchema;
use wisp_store::Store;
use wisp_tools::{Approval, ImageData, Tool, ToolEnv, ToolResult};

use crate::browser_url_filters::{self, BrowserUrlFilters};

mod chatgpt;
mod errors;
mod workspace;

const BRIDGE_ADDR: &str = "127.0.0.1:18765";
const WORKSPACE_ADDR: &str = "127.0.0.1:18766";
const REQUIRED_PROTOCOL: i64 = 2;
const EXTENSION_ORIGIN: &str = "chrome-extension://gnkjgagleagkgdlkkcianolobfdoocnp";
const BROWSER_DISCONNECTED_CODE: &str = "browser_extension_disconnected";
const BROWSER_DISCONNECTED_MARKER: &str = "WISP_BROWSER_DISCONNECTED";
const DISCONNECTED_ASSISTANT_INSTRUCTION: &str = "Live web retrieval is unavailable. Do not answer live, latest, current, or URL-specific questions from prior knowledge. Tell the user this turn contains no live web retrieval, relay the install steps, and wait until status is connected. Only continue from memory if they explicitly ask for a knowledge-only answer.";
const STALE_ASSISTANT_INSTRUCTION: &str = "A connected extension is older than the protocol this build needs, so parts of the browser toolset will fail. Tell the user which session needs it, have them open chrome://extensions and Reload Wisp Real Browser Bridge from extension_path, and do not claim the newer tools exist.";
const DEFAULT_TIMEOUT_MS: u64 = 15_000;
const MAX_TIMEOUT_MS: u64 = 60_000;
const AUTO_LAUNCH_WAIT: Duration = Duration::from_secs(15);
/// How long `start_workspace` waits for the launched browser's extension to
/// reach the workspace port before declaring the window unusable.
const WORKSPACE_CONNECT_WAIT: Duration = Duration::from_secs(20);
const MAX_SCRIPT_BYTES: usize = 64 * 1024;
const MAX_RESULT_CHARS: usize = 200_000;
/// Base64 payload ceiling for one screenshot, matching the shared image path's
/// 5 MB decoded limit (base64 inflates by 4/3).
const MAX_SCREENSHOT_B64: usize = 7 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct BrowserTab {
    id: i64,
    url: String,
    title: String,
    active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    window_id: Option<i64>,
}

#[derive(Clone)]
struct BridgeClient {
    connection_id: u64,
    session: String,
    tx: mpsc::UnboundedSender<Message>,
}

#[derive(Clone, Debug, Default)]
struct SessionMeta {
    protocol_version: i64,
    extension_version: String,
    capabilities: Vec<String>,
    paused: bool,
}

#[derive(Default)]
struct SessionState {
    client: Option<BridgeClient>,
    tabs: BTreeMap<i64, BrowserTab>,
    selected_tab: Option<i64>,
    pending: HashMap<String, oneshot::Sender<Result<BridgeReply, String>>>,
    meta: SessionMeta,
}

/// A client that reached a bridge port but was never claimed as the Wisp
/// extension. Kept so `browser_setup` can say *why* the extension popup shows
/// "Connected to Wisp" while Wisp reports `connected=false`.
#[derive(Clone, Debug)]
struct RefusedConnection {
    session: String,
    origin: Option<String>,
    reason: String,
}

#[derive(Default)]
struct BridgeState {
    sessions: HashMap<String, SessionState>,
    last_session: Option<String>,
    startup_error: Option<String>,
    workspace_pid: Option<u32>,
    last_refusal: Option<RefusedConnection>,
}

pub struct BrowserBridge {
    state: Mutex<BridgeState>,
    next_connection_id: AtomicU64,
    extension_dir: PathBuf,
    store: Option<Store>,
    /// Production `start()` only. Tests must not spawn a real browser.
    can_launch: bool,
    launch_lock: Mutex<()>,
}

#[derive(Clone, Debug)]
struct BridgeReply {
    value: Value,
    ready: Option<bool>,
    wait: Option<Value>,
}

struct BrowserExecution {
    tab_id: i64,
    value: Value,
    ready: Option<bool>,
    wait: Option<Value>,
}

fn session_slot<'a>(state: &'a mut BridgeState, name: &str) -> &'a mut SessionState {
    state.sessions.entry(name.to_string()).or_default()
}

fn session_summary(state: &BridgeState, name: &str) -> Value {
    let slot = state.sessions.get(name);
    let connected = slot.and_then(|session| session.client.as_ref()).is_some();
    let meta = slot.map(|session| &session.meta);
    let protocol_version = meta.map(|meta| meta.protocol_version).unwrap_or(0);
    let capabilities = meta
        .map(|meta| meta.capabilities.clone())
        .unwrap_or_default();
    let reload_required =
        connected && (protocol_version < REQUIRED_PROTOCOL || capabilities.is_empty());
    json!({
        "connected": connected,
        "extension_version": meta.map(|meta| meta.extension_version.clone()).unwrap_or_default(),
        "protocol_version": protocol_version,
        "capabilities": capabilities,
        "paused": meta.map(|meta| meta.paused).unwrap_or(false),
        "tabs": slot.map(|session| session.tabs.len()).unwrap_or(0),
        "reload_required": reload_required
    })
}

/// Explain a refused connection in the terms the user sees: the popup says
/// connected, Wisp says it is not.
fn refusal_summary(refusal: Option<&RefusedConnection>) -> Value {
    let Some(refusal) = refusal else {
        return Value::Null;
    };
    let session = &refusal.session;
    let explanation = match refusal.origin.as_deref() {
        Some(origin) if !origin.is_empty() => format!(
            "An extension at {origin} reached the {session} port and Wisp refused it, because this bridge only accepts {EXTENSION_ORIGIN}. Its own popup can still read Connected to Wisp while Wisp reports connected=false. Load the bundled extension from extension_path (a repacked or third-party copy gets a different id), and remove any other loopback bridge extension."
        ),
        _ => format!(
            "A client reached the {session} port without completing the Wisp extension handshake, so no session was claimed and Wisp reports connected=false. Another browser bridge or a plain HTTP client is probably using this port; stop it, then reload the Wisp extension."
        ),
    };
    json!({
        "session": session,
        "origin": refusal.origin,
        "expected_origin": EXTENSION_ORIGIN,
        "reason": refusal.reason,
        "explanation": explanation
    })
}

fn bundled_manifest_version(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("manifest.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.get("version")?.as_str().map(str::to_string)
}

fn session_name_locked(state: &BridgeState, requested: Option<&str>) -> Result<String, String> {
    if let Some(name) = requested {
        if name != "shared" && name != "workspace" {
            return Err(errors::structured(
                errors::SESSION_REQUIRED,
                "session must be shared or workspace",
                false,
            ));
        }
        return Ok(name.to_string());
    }
    let connected: Vec<&str> = ["shared", "workspace"]
        .into_iter()
        .filter(|name| {
            state
                .sessions
                .get(*name)
                .and_then(|session| session.client.as_ref())
                .is_some()
        })
        .collect();
    if connected.is_empty() {
        return Ok("shared".into());
    }
    if connected.len() == 1 {
        return Ok(connected[0].to_string());
    }
    if let Some(last) = state.last_session.as_deref() {
        if connected.iter().any(|name| *name == last) {
            return Ok(last.to_string());
        }
    }
    Err(errors::structured(
        errors::SESSION_REQUIRED,
        "shared and workspace are both connected; pass session=shared or session=workspace",
        false,
    ))
}

#[allow(dead_code)]
impl BrowserBridge {
    fn new(extension_dir: PathBuf) -> Self {
        Self {
            state: Mutex::new(BridgeState::default()),
            next_connection_id: AtomicU64::new(1),
            extension_dir,
            store: None,
            can_launch: false,
            launch_lock: Mutex::new(()),
        }
    }

    pub async fn start(extension_dir: PathBuf, store: Store) -> Arc<Self> {
        let bridge = Arc::new(Self {
            state: Mutex::new(BridgeState::default()),
            next_connection_id: AtomicU64::new(1),
            extension_dir,
            store: Some(store),
            can_launch: true,
            launch_lock: Mutex::new(()),
        });
        match TcpListener::bind(BRIDGE_ADDR).await {
            Ok(listener) => {
                let task_bridge = bridge.clone();
                tokio::spawn(async move { task_bridge.accept_loop(listener).await });
            }
            Err(error) => {
                bridge.state.lock().await.startup_error = Some(format!(
                    "cannot listen on {BRIDGE_ADDR}: {error}; stop any other TMWebDriver/Wisp browser bridge using this port"
                ));
            }
        }
        match TcpListener::bind(WORKSPACE_ADDR).await {
            Ok(listener) => {
                let task_bridge = bridge.clone();
                tokio::spawn(async move {
                    task_bridge
                        .accept_loop_on(listener, "workspace".into())
                        .await
                });
            }
            Err(error) => {
                tracing::warn!(target: "wisp", "workspace bridge not listening on {WORKSPACE_ADDR}: {error}");
            }
        }
        bridge
    }

    async fn setup_info(&self) -> Value {
        let auto_launch = match &self.store {
            Some(store) => browser_url_filters::auto_launch_enabled(store).await,
            None => false,
        };
        let state = self.state.lock().await;
        let extension_path = self.verified_extension_path();
        let extension_ready = extension_path.is_some();
        // A live extension connection is the only proof that live retrieval
        // works. It outranks an unverifiable bundled copy: a user who loaded the
        // extension from another folder still browses fine, and reporting
        // extension_missing there told the model and the UI "no live retrieval"
        // on every turn (#921).
        let any_connected = ["shared", "workspace"].into_iter().any(|name| {
            state
                .sessions
                .get(name)
                .and_then(|session| session.client.as_ref())
                .is_some()
        });
        let status = if any_connected {
            "connected"
        } else if state.startup_error.is_some() {
            "error"
        } else if !extension_ready {
            "extension_missing"
        } else {
            "disconnected"
        };
        let live_retrieval = status == "connected";
        let steps = extension_path.as_ref().map_or_else(Vec::new, |path| {
            vec![
                "Start Wisp Science and keep it running.".to_string(),
                "Open chrome://extensions in the Chrome/Chromium profile Wisp should control."
                    .to_string(),
                "Enable Developer mode.".to_string(),
                format!("Click Load unpacked and select this exact folder: {path}"),
                "Open the Wisp Real Browser Bridge extension popup and confirm Connected to Wisp."
                    .to_string(),
            ]
        });
        let path_instruction = if extension_ready {
            "Copy extension_path character-for-character. Never translate, infer, normalize, or replace any path segment."
        } else {
            "The running Wisp build has no verified bundled extension path. Do not invent a path or claim the extension exists."
        };
        let connected_tabs = ["shared", "workspace"]
            .into_iter()
            .filter_map(|name| state.sessions.get(name))
            .map(|session| session.tabs.len())
            .sum::<usize>();
        let shared = session_summary(&state, "shared");
        let workspace = session_summary(&state, "workspace");
        let reload_required = [&shared, &workspace]
            .into_iter()
            .any(|session| session["reload_required"] == Value::Bool(true));
        let assistant_instruction = match (live_retrieval, reload_required) {
            (true, false) => path_instruction.to_string(),
            (true, true) => format!("{STALE_ASSISTANT_INSTRUCTION} {path_instruction}"),
            (false, _) => format!("{DISCONNECTED_ASSISTANT_INSTRUCTION} {path_instruction}"),
        };
        let bundled_extension_version = bundled_manifest_version(&self.extension_dir);
        let reported_extension_version = state
            .sessions
            .get("shared")
            .map(|session| session.meta.extension_version.clone())
            .filter(|version| !version.is_empty())
            .or_else(|| bundled_extension_version.clone());
        let workspace_running = state.workspace_pid.is_some();
        let workspace_connected = workspace
            .get("connected")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        json!({
            "status": status,
            "live_retrieval": live_retrieval,
            "code": if live_retrieval { Value::Null } else { json!(BROWSER_DISCONNECTED_CODE) },
            "connected_tabs": connected_tabs,
            "extension_version": reported_extension_version,
            "bundled_extension_version": bundled_extension_version,
            "workspace_endpoint": format!("ws://{WORKSPACE_ADDR}"),
            "required_protocol": REQUIRED_PROTOCOL,
            "reload_required": reload_required,
            "refused_connection": refusal_summary(state.last_refusal.as_ref()),
            "workspace": workspace::status_json(workspace_connected, workspace_running),
            "sessions": {
                "shared": shared,
                "workspace": workspace
            },
            "runtime_os": std::env::consts::OS,
            "path_source": "wisp_tauri_resource_dir",
            "extension_path": extension_path,
            "extension_path_verified": extension_ready,
            "extension_id": EXTENSION_ORIGIN.trim_start_matches("chrome-extension://"),
            "bridge_endpoint": format!("ws://{BRIDGE_ADDR}"),
            "install_scope": "once_per_browser_profile",
            "assistant_instruction": assistant_instruction,
            "steps": steps,
            "download_automation": {
                "limitation": "GA Web controls web-page tabs. It cannot operate Chrome/Edge toolbar download bubbles or native operating-system Open, Save, and Save As dialogs.",
                "manual_setup_required": true,
                "chrome_settings_url": "chrome://settings/downloads",
                "edge_settings_url": "edge://settings/downloads",
                "setting_to_disable": "Ask where to save each file before downloading",
                "multiple_downloads": {
                    "chrome_settings_url": "chrome://settings/content/automaticDownloads",
                    "edge_settings_url": "edge://settings/content/automaticDownloads",
                    "agent_gate": "Before triggering multiple file downloads, show these browser settings and wait for the user to confirm configuration. Until confirmed, download at most one file.",
                    "recommended_action": "Add only the trusted target site to Allowed to automatically download multiple files. If the browser asks on the site's first batch, choose Allow.",
                    "security_note": "Do not allow automatic multiple downloads for untrusted sites."
                },
                "effect": "Downloads save to the browser's configured default download directory without opening a native location prompt. Authorized filesystem tools may process the saved file afterward."
            },
            "auto_launch_browser": auto_launch,
            "error": state.startup_error
        })
    }

    async fn accept_loop(self: Arc<Self>, listener: TcpListener) {
        self.accept_loop_on(listener, "shared".into()).await;
    }

    async fn accept_loop_on(self: Arc<Self>, listener: TcpListener, session: String) {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let bridge = self.clone();
                    let session = session.clone();
                    tokio::spawn(async move {
                        if let Err(error) = bridge.accept_connection_on(stream, session).await {
                            tracing::warn!(target: "wisp", "browser bridge connection rejected: {error}");
                        }
                    });
                }
                Err(error) => {
                    tracing::warn!(target: "wisp", "browser bridge accept failed: {error}");
                }
            }
        }
    }

    async fn accept_connection(
        self: Arc<Self>,
        stream: TcpStream,
    ) -> Result<(), tokio_tungstenite::tungstenite::Error> {
        self.accept_connection_on(stream, "shared".into()).await
    }

    async fn accept_connection_on(
        self: Arc<Self>,
        stream: TcpStream,
        session: String,
    ) -> Result<(), tokio_tungstenite::tungstenite::Error> {
        // The handshake callback is synchronous, so the refused origin is parked
        // here and folded into bridge state once the await returns. Without it a
        // rejected extension is only a log line and the user is told nothing but
        // `connected=false` (#952).
        let refused: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
        let seen = refused.clone();
        let handshake = accept_hdr_async(stream, move |request: &Request, response: Response| {
            let origin = request
                .headers()
                .get("origin")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            if allowed_extension_origin(origin.as_deref()) {
                Ok(response)
            } else {
                if let Ok(mut slot) = seen.lock() {
                    *slot = Some(origin.unwrap_or_default());
                }
                Err(forbidden_response())
            }
        })
        .await;
        let socket = match handshake {
            Ok(socket) => socket,
            Err(error) => {
                let origin = refused.lock().ok().and_then(|slot| slot.clone());
                self.state.lock().await.last_refusal = Some(RefusedConnection {
                    session,
                    origin,
                    reason: error.to_string(),
                });
                return Err(error);
            }
        };
        self.serve_connection_on(socket, session).await;
        Ok(())
    }

    async fn serve_connection(self: Arc<Self>, socket: WebSocketStream<TcpStream>) {
        self.serve_connection_on(socket, "shared".into()).await;
    }

    async fn serve_connection_on(
        self: Arc<Self>,
        socket: WebSocketStream<TcpStream>,
        session: String,
    ) {
        let connection_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        let (mut writer, mut reader) = socket.split();
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.install_client_on(connection_id, tx.clone(), &session)
            .await;
        let writer_task = tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if writer.send(message).await.is_err() {
                    break;
                }
            }
        });

        while let Some(message) = reader.next().await {
            match message {
                Ok(Message::Text(text)) => {
                    self.handle_text_on(connection_id, &session, text.as_str())
                        .await
                }
                Ok(Message::Ping(payload)) => {
                    let _ = tx.send(Message::Pong(payload));
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
        writer_task.abort();
        self.disconnect_client_on(connection_id, &session).await;
    }

    async fn install_client(&self, connection_id: u64, tx: mpsc::UnboundedSender<Message>) {
        self.install_client_on(connection_id, tx, "shared").await;
    }

    async fn install_client_on(
        &self,
        connection_id: u64,
        tx: mpsc::UnboundedSender<Message>,
        session: &str,
    ) {
        let mut state = self.state.lock().await;
        let slot = session_slot(&mut state, session);
        fail_pending(slot, "browser extension connection was replaced");
        slot.client = Some(BridgeClient {
            connection_id,
            session: session.to_string(),
            tx,
        });
        slot.tabs.clear();
        slot.selected_tab = None;
    }

    async fn disconnect_client(&self, connection_id: u64) {
        self.disconnect_client_on(connection_id, "shared").await;
    }

    async fn disconnect_client_on(&self, connection_id: u64, session: &str) {
        let mut state = self.state.lock().await;
        if let Some(slot) = state.sessions.get_mut(session) {
            if slot
                .client
                .as_ref()
                .is_some_and(|client| client.connection_id == connection_id)
            {
                slot.client = None;
                slot.tabs.clear();
                slot.selected_tab = None;
                fail_pending(slot, "browser extension disconnected");
            }
        }
    }

    async fn handle_text(&self, connection_id: u64, text: &str) {
        self.handle_text_on(connection_id, "shared", text).await;
    }

    async fn handle_text_on(&self, connection_id: u64, session: &str, text: &str) {
        let Ok(message) = serde_json::from_str::<Value>(text) else {
            return;
        };
        let message_type = message.get("type").and_then(Value::as_str).unwrap_or("");
        let mut state = self.state.lock().await;
        let Some(slot) = state.sessions.get_mut(session) else {
            return;
        };
        if !slot
            .client
            .as_ref()
            .is_some_and(|client| client.connection_id == connection_id)
        {
            return;
        }
        match message_type {
            "ext_ready" | "tabs_update" => {
                if message_type == "ext_ready" {
                    slot.meta.protocol_version = message
                        .get("protocol_version")
                        .and_then(Value::as_i64)
                        .unwrap_or(1);
                    slot.meta.extension_version = message
                        .get("extension_version")
                        .and_then(Value::as_str)
                        .unwrap_or("0.2.1")
                        .to_string();
                    slot.meta.capabilities = message
                        .get("capabilities")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    slot.meta.paused = message
                        .get("paused")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                }
                replace_tabs(slot, &message);
            }
            "result" | "error" => {
                let Some(id) = message.get("id").and_then(Value::as_str) else {
                    return;
                };
                let Some(sender) = slot.pending.remove(id) else {
                    return;
                };
                let result = if message_type == "result" {
                    Ok(parse_bridge_reply(&message))
                } else {
                    Err(errors::from_value(message.get("error")))
                };
                let _ = sender.send(result);
            }
            _ => {}
        }
    }

    async fn execute(
        &self,
        requested_tab: Option<i64>,
        code: &str,
        timeout: Duration,
    ) -> Result<BrowserExecution, String> {
        self.execute_on(None, requested_tab, code, timeout).await
    }

    async fn execute_on(
        &self,
        session: Option<&str>,
        requested_tab: Option<i64>,
        code: &str,
        timeout: Duration,
    ) -> Result<BrowserExecution, String> {
        let id = Uuid::new_v4().to_string();
        let (response_tx, response_rx) = oneshot::channel();
        self.ensure_extension().await;
        let tab_id = {
            let mut state = self.state.lock().await;
            if let Some(error) = &state.startup_error {
                return Err(self.unavailable_message(error));
            }
            let session_name = session_name_locked(&state, session)?;
            let slot = session_slot(&mut state, &session_name);
            if slot.meta.paused {
                return Err(errors::structured(
                    errors::USER_CONTROLLING,
                    "user paused browser control from the extension popup",
                    false,
                ));
            }
            let Some(client) = slot.client.clone() else {
                return Err(self.unavailable_message("browser extension is not connected"));
            };
            let tab_id = select_tab(slot, requested_tab)?;
            slot.selected_tab = Some(tab_id);
            slot.pending.insert(id.clone(), response_tx);
            state.last_session = Some(session_name);
            let payload = request_payload(&id, Some(tab_id), code, timeout);
            if client.tx.send(Message::Text(payload.into())).is_err() {
                if let Some(slot) = state.sessions.get_mut(client.session.as_str()) {
                    slot.pending.remove(&id);
                }
                return Err("browser extension disconnected before the request was sent".into());
            }
            tab_id
        };

        match tokio::time::timeout(timeout, response_rx).await {
            Ok(Ok(Ok(reply))) => Ok(BrowserExecution {
                tab_id,
                value: reply.value,
                ready: reply.ready,
                wait: reply.wait,
            }),
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(_)) => Err("browser extension disconnected before returning a result".into()),
            Err(_) => {
                self.state
                    .lock()
                    .await
                    .sessions
                    .values_mut()
                    .for_each(|slot| {
                        slot.pending.remove(&id);
                    });
                Err(format!(
                    "browser execution timed out after {} ms",
                    timeout.as_millis()
                ))
            }
        }
    }

    /// Send a control command that does not target an existing tab (e.g. open a
    /// new tab). Unlike `execute`, this never requires an HTTP(S) tab to exist,
    /// so it can bootstrap browsing from an empty profile.
    async fn send_command(&self, code: String, timeout: Duration) -> Result<BridgeReply, String> {
        self.send_command_on(None, code, timeout).await
    }

    async fn send_command_on(
        &self,
        session: Option<&str>,
        code: String,
        timeout: Duration,
    ) -> Result<BridgeReply, String> {
        self.ensure_extension().await;
        let id = Uuid::new_v4().to_string();
        let (response_tx, response_rx) = oneshot::channel();
        {
            let mut state = self.state.lock().await;
            if let Some(error) = &state.startup_error {
                return Err(self.unavailable_message(error));
            }
            let session_name = session_name_locked(&state, session)?;
            let slot = session_slot(&mut state, &session_name);
            if slot.meta.paused {
                return Err(errors::structured(
                    errors::USER_CONTROLLING,
                    "user paused browser control from the extension popup",
                    false,
                ));
            }
            let Some(client) = slot.client.clone() else {
                return Err(self.unavailable_message("browser extension is not connected"));
            };
            slot.pending.insert(id.clone(), response_tx);
            state.last_session = Some(session_name);
            let payload = request_payload(&id, None, &code, timeout);
            if client.tx.send(Message::Text(payload.into())).is_err() {
                if let Some(slot) = state.sessions.get_mut(client.session.as_str()) {
                    slot.pending.remove(&id);
                }
                return Err("browser extension disconnected before the request was sent".into());
            }
        }

        match tokio::time::timeout(timeout, response_rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(_)) => Err("browser extension disconnected before returning a result".into()),
            Err(_) => {
                self.state
                    .lock()
                    .await
                    .sessions
                    .values_mut()
                    .for_each(|slot| {
                        slot.pending.remove(&id);
                    });
                Err(format!(
                    "browser command timed out after {} ms",
                    timeout.as_millis()
                ))
            }
        }
    }

    async fn open_tab(&self, url: &str, active: bool) -> Result<BridgeReply, String> {
        let code =
            json!({ "cmd": "tabs", "method": "create", "url": url, "active": active }).to_string();
        self.send_command_on(None, code, Duration::from_millis(DEFAULT_TIMEOUT_MS))
            .await
    }

    async fn open_tab_on(
        &self,
        session: Option<&str>,
        url: &str,
        active: bool,
    ) -> Result<BridgeReply, String> {
        let code =
            json!({ "cmd": "tabs", "method": "create", "url": url, "active": active }).to_string();
        self.send_command_on(session, code, Duration::from_millis(DEFAULT_TIMEOUT_MS))
            .await
    }

    async fn require_capability(
        &self,
        session: Option<&str>,
        capability: &str,
    ) -> Result<String, String> {
        let state = self.state.lock().await;
        if let Some(error) = &state.startup_error {
            return Err(self.unavailable_message(error));
        }
        let session_name = session_name_locked(&state, session)?;
        let Some(slot) = state.sessions.get(&session_name) else {
            return Err(self.unavailable_message("browser extension is not connected"));
        };
        if slot.client.is_none() {
            return Err(self.unavailable_message("browser extension is not connected"));
        }
        let has_capability = slot.meta.capabilities.iter().any(|item| item == capability);
        if slot.meta.protocol_version < REQUIRED_PROTOCOL || !has_capability {
            let version = if slot.meta.extension_version.is_empty() {
                "unknown".to_string()
            } else {
                slot.meta.extension_version.clone()
            };
            return Err(errors::structured(
                errors::EXTENSION_STALE,
                &format!(
                    "connected extension {version} (protocol {}) does not provide '{capability}'. Open chrome://extensions and Reload Wisp Real Browser Bridge 0.3.0 from extension_path. Do not pretend the new tool exists.",
                    slot.meta.protocol_version
                ),
                false,
            ));
        }
        Ok(session_name)
    }

    async fn start_workspace(&self) -> Result<Value, String> {
        let extension_path = self.verified_extension_path().ok_or_else(|| {
            "bundled browser-extension path is not available; cannot materialize workspace copy"
                .to_string()
        })?;
        let extension = workspace::materialize_extension(Path::new(&extension_path))?;
        self.start_workspace_with(
            workspace::resolve_browser()?,
            &extension_path,
            &extension,
            WORKSPACE_CONNECT_WAIT,
            |browser, profile, extension| {
                let child = workspace::launch_browser(browser, profile, extension)?;
                let pid = child.id();
                std::mem::forget(child);
                Ok(pid)
            },
            workspace::terminate,
        )
        .await
    }

    /// Launch the workspace browser and only report success once its extension
    /// has actually connected. Chrome 137+ ignores `--load-extension`, so
    /// reporting the spawned process as ready left the agent driving a blank
    /// window that could never answer (#952).
    async fn start_workspace_with<L, T>(
        &self,
        browser: workspace::WorkspaceBrowser,
        extension_path: &str,
        extension: &Path,
        wait: Duration,
        launch: L,
        terminate: T,
    ) -> Result<Value, String>
    where
        L: FnOnce(&workspace::WorkspaceBrowser, &Path, &Path) -> Result<u32, String>,
        T: FnOnce(u32),
    {
        let pid = launch(&browser, &workspace::profile_dir(), extension)?;
        self.state.lock().await.workspace_pid = Some(pid);
        if self.wait_for_session("workspace", wait).await {
            return Ok(workspace::status_json(true, true));
        }
        self.state.lock().await.workspace_pid = None;
        terminate(pid);
        Err(errors::structured(
            errors::WORKSPACE_EXTENSION_BLOCKED,
            &workspace::extension_blocked_message(&browser, extension_path, wait),
            false,
        ))
    }

    async fn stop_workspace(&self) -> Result<Value, String> {
        let pid = self.state.lock().await.workspace_pid.take();
        if let Some(pid) = pid {
            workspace::terminate(pid);
        }
        Ok(workspace::status_json(false, false))
    }

    async fn wait_for_session(&self, session: &str, wait: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if self.session_connected(session).await {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn session_connected(&self, session: &str) -> bool {
        self.state
            .lock()
            .await
            .sessions
            .get(session)
            .and_then(|slot| slot.client.as_ref())
            .is_some()
    }

    async fn tabs(&self) -> Result<Vec<BrowserTab>, String> {
        self.ensure_extension().await;
        let state = self.state.lock().await;
        if let Some(error) = &state.startup_error {
            return Err(self.unavailable_message(error));
        }
        let session_name = session_name_locked(&state, None)?;
        let Some(slot) = state.sessions.get(&session_name) else {
            return Err(self.unavailable_message("browser extension is not connected"));
        };
        if slot.client.is_none() {
            return Err(self.unavailable_message("browser extension is not connected"));
        }
        Ok(slot.tabs.values().cloned().collect())
    }

    async fn tabs_on(&self, session: Option<&str>) -> Result<Vec<BrowserTab>, String> {
        let state = self.state.lock().await;
        if let Some(error) = &state.startup_error {
            return Err(self.unavailable_message(error));
        }
        let session_name = session_name_locked(&state, session)?;
        let Some(slot) = state.sessions.get(&session_name) else {
            return Err(self.unavailable_message("browser extension is not connected"));
        };
        if slot.client.is_none() {
            return Err(self.unavailable_message("browser extension is not connected"));
        }
        Ok(slot.tabs.values().cloned().collect())
    }

    async fn client_connected(&self) -> bool {
        self.state
            .lock()
            .await
            .sessions
            .values()
            .any(|slot| slot.client.is_some())
    }

    /// If the extension is down and auto-launch is on, start the user's
    /// Chrome/Chromium/Edge so the already-installed unpacked extension can
    /// reconnect. Never used from tests (`can_launch` is false on `new()`).
    async fn ensure_extension(&self) {
        if self.client_connected().await {
            return;
        }
        if !self.can_launch {
            return;
        }
        if self.state.lock().await.startup_error.is_some() {
            return;
        }
        let enabled = match &self.store {
            Some(store) => browser_url_filters::auto_launch_enabled(store).await,
            None => false,
        };
        if !enabled {
            return;
        }
        let _guard = self.launch_lock.lock().await;
        if self.client_connected().await {
            return;
        }
        if let Err(error) = spawn_user_browser() {
            tracing::warn!(target: "wisp", "browser auto-launch failed: {error}");
            return;
        }
        let deadline = tokio::time::Instant::now() + AUTO_LAUNCH_WAIT;
        while tokio::time::Instant::now() < deadline {
            if self.client_connected().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    fn unavailable_message(&self, reason: &str) -> String {
        let setup = match self.verified_extension_path() {
            Some(path) => format!(
                "real-browser bridge unavailable: {reason}. In Chrome/Chromium open chrome://extensions, enable Developer mode, and Load unpacked from this exact native {} path: '{path}'. Keep Wisp running; the extension connects only to {BRIDGE_ADDR}.",
                std::env::consts::OS
            ),
            None => format!(
                "real-browser bridge unavailable: {reason}. This Wisp build has no verified bundled browser extension; do not infer an installation path."
            ),
        };
        format!("{setup} {BROWSER_DISCONNECTED_MARKER}. {DISCONNECTED_ASSISTANT_INSTRUCTION}")
    }

    fn verified_extension_path(&self) -> Option<String> {
        let dir = dunce::canonicalize(&self.extension_dir).ok()?;
        (dir.join("manifest.json").is_file() && dir.join("wait_tab.js").is_file())
            .then(|| dir.display().to_string())
    }

    /// Open the first real browser on its extension manager page so the
    /// banner's setup button can cut the manual install down to a paste.
    /// `opened` is false when no usable browser executable was found or the
    /// launch failed; the UI then falls back to manual instructions. Never
    /// spawns a browser from tests (`can_launch` is false on `new()`).
    pub fn open_extension_setup(&self) -> BrowserExtensionSetup {
        let extension_path = self.verified_extension_path();
        let opened = self.can_launch
            && browser_extension_page_plan().is_some_and(|(program, args)| {
                spawn_browser_with_url(&program, &args)
                    .map_err(|error| {
                        tracing::warn!(target: "wisp", "extension page launch failed: {error}");
                    })
                    .is_ok()
            });
        BrowserExtensionSetup {
            extension_path,
            opened,
        }
    }
}

/// Reply of the `open_browser_extension_page` command: the verified bundled
/// extension path (null when this build ships no complete extension) and
/// whether a browser was actually launched on its extension manager page.
#[derive(Serialize, Clone)]
pub struct BrowserExtensionSetup {
    pub extension_path: Option<String>,
    pub opened: bool,
}

/// Start the user's existing Chrome/Chromium/Edge so the unpacked Wisp
/// extension can reconnect. Does not use a temporary automation profile.
fn spawn_user_browser() -> Result<(), String> {
    let (program, args) = first_available_browser()
        .ok_or_else(|| "no Chrome, Chromium, or Edge browser was found".to_string())?;
    let mut command = std::process::Command::new(&program);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00000008 | 0x00000200);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to start {}: {error}", program.display()))
}

fn first_available_browser() -> Option<(PathBuf, Vec<String>)> {
    browser_launch_candidates()
        .into_iter()
        .find(|(program, args)| launch_plan_available(program, args))
}

/// Launch plan for the first real browser executable, pointed at its
/// extension manager page. The `cmd /C start` fallback is skipped: it cannot
/// carry the `*://extensions` URL.
fn browser_extension_page_plan() -> Option<(PathBuf, Vec<String>)> {
    browser_launch_candidates()
        .into_iter()
        .filter(|(program, _)| program.file_name().and_then(|name| name.to_str()) != Some("cmd"))
        .find(|(program, args)| launch_plan_available(program, args))
        .map(|(program, mut args)| {
            let url = extensions_page_url(&program, &args);
            args.push(url.to_string());
            (program, args)
        })
}

/// Each Chromium fork only understands its own `*://extensions` scheme.
fn extensions_page_url(program: &Path, args: &[String]) -> &'static str {
    let name = if program.ends_with("open") {
        // macOS `open -a "<App Name>"`: the browser identity sits in the args.
        args.get(1).map(String::as_str).unwrap_or_default()
    } else {
        program
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
    }
    .to_lowercase();
    if name.contains("edge") {
        "edge://extensions"
    } else if name.contains("brave") {
        "brave://extensions"
    } else {
        "chrome://extensions"
    }
}

/// Start `program` detached like `spawn_user_browser`, with caller-chosen args.
fn spawn_browser_with_url(program: &Path, args: &[String]) -> Result<(), String> {
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00000008 | 0x00000200);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to start {}: {error}", program.display()))
}

/// OS-specific launch plans. The first available program is used at runtime.
fn browser_launch_candidates() -> Vec<(PathBuf, Vec<String>)> {
    #[cfg(windows)]
    {
        let mut candidates = Vec::new();
        for root in [
            std::env::var_os("LOCALAPPDATA"),
            std::env::var_os("PROGRAMFILES"),
            std::env::var_os("PROGRAMFILES(X86)"),
        ]
        .into_iter()
        .flatten()
        {
            let root = PathBuf::from(root);
            candidates.push((
                root.join("Google/Chrome/Application/chrome.exe"),
                Vec::new(),
            ));
            candidates.push((
                root.join("Microsoft/Edge/Application/msedge.exe"),
                Vec::new(),
            ));
            candidates.push((
                root.join("BraveSoftware/Brave-Browser/Application/brave.exe"),
                Vec::new(),
            ));
        }
        candidates.push((
            PathBuf::from("cmd"),
            vec!["/C".into(), "start".into(), String::new(), "chrome".into()],
        ));
        candidates
    }
    #[cfg(target_os = "macos")]
    {
        [
            "Google Chrome",
            "Microsoft Edge",
            "Chromium",
            "Brave Browser",
        ]
        .into_iter()
        .map(|name| {
            (
                PathBuf::from("/usr/bin/open"),
                vec!["-a".into(), name.into()],
            )
        })
        .collect()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        [
            "google-chrome-stable",
            "google-chrome",
            "chromium-browser",
            "chromium",
            "microsoft-edge-stable",
            "microsoft-edge",
            "brave-browser",
        ]
        .into_iter()
        .map(|name| (PathBuf::from(name), Vec::new()))
        .collect()
    }
}

fn launch_plan_available(
    program: &Path,
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] args: &[String],
) -> bool {
    #[cfg(target_os = "macos")]
    {
        if program.ends_with("open") && args.first().map(String::as_str) == Some("-a") {
            return args.get(1).is_some_and(|name| {
                Path::new("/Applications")
                    .join(format!("{name}.app"))
                    .is_dir()
            });
        }
    }
    if program.components().count() > 1 {
        return program.is_file();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(program);
        candidate.is_file()
            || cfg!(windows) && dir.join(format!("{}.exe", program.display())).is_file()
    })
}

fn allowed_extension_origin(origin: Option<&str>) -> bool {
    origin == Some(EXTENSION_ORIGIN)
}

fn forbidden_response() -> ErrorResponse {
    tokio_tungstenite::tungstenite::http::Response::builder()
        .status(StatusCode::FORBIDDEN)
        .body(Some(
            "Wisp browser bridge accepts Chrome extensions only".into(),
        ))
        .expect("static browser bridge rejection response")
}

fn fail_pending(state: &mut SessionState, reason: &str) {
    for (_, sender) in state.pending.drain() {
        let _ = sender.send(Err(reason.to_string()));
    }
}

fn replace_tabs(state: &mut SessionState, message: &Value) {
    let Some(tabs) = message.get("tabs").and_then(Value::as_array) else {
        return;
    };
    state.tabs = tabs
        .iter()
        .filter_map(parse_tab)
        .map(|tab| (tab.id, tab))
        .collect();
    if !state
        .selected_tab
        .is_some_and(|tab_id| state.tabs.contains_key(&tab_id))
    {
        state.selected_tab = state
            .tabs
            .values()
            .find(|tab| tab.active)
            .or_else(|| state.tabs.values().next())
            .map(|tab| tab.id);
    }
}

fn parse_tab(value: &Value) -> Option<BrowserTab> {
    let id = value.get("id").and_then(|id| {
        id.as_i64()
            .or_else(|| id.as_str().and_then(|id| id.parse().ok()))
    })?;
    Some(BrowserTab {
        id,
        url: value
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        active: value
            .get("active")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        window_id: value.get("windowId").and_then(Value::as_i64),
    })
}

fn select_tab(state: &SessionState, requested: Option<i64>) -> Result<i64, String> {
    if state.tabs.is_empty() {
        return Err("browser extension is connected, but no HTTP(S) tabs are available".into());
    }
    if let Some(tab_id) = requested {
        return state
            .tabs
            .contains_key(&tab_id)
            .then_some(tab_id)
            .ok_or_else(|| {
                format!("browser tab {tab_id} is not available; call web_scan with tabs_only=true")
            });
    }
    state
        .selected_tab
        .filter(|tab_id| state.tabs.contains_key(tab_id))
        .or_else(|| state.tabs.values().find(|tab| tab.active).map(|tab| tab.id))
        .or_else(|| state.tabs.keys().next().copied())
        .ok_or_else(|| "no browser tab is selected".into())
}

fn request_payload(id: &str, tab_id: Option<i64>, code: &str, timeout: Duration) -> String {
    let mut payload = json!({
        "id": id,
        "code": code,
        "timeoutMs": timeout.as_millis() as u64,
    });
    if let Some(tab_id) = tab_id {
        payload["tabId"] = json!(tab_id);
    }
    payload.to_string()
}

fn parse_bridge_reply(message: &Value) -> BridgeReply {
    BridgeReply {
        value: message
            .get("result")
            .or_else(|| message.get("data"))
            .cloned()
            .unwrap_or(Value::Null),
        ready: message.get("ready").and_then(Value::as_bool),
        wait: message
            .get("wait")
            .cloned()
            .filter(|value| !value.is_null()),
    }
}

fn merge_ready_wait(mut payload: Value, ready: Option<bool>, wait: Option<Value>) -> Value {
    if let Some(ready) = ready {
        payload["ready"] = json!(ready);
    }
    if let Some(wait) = wait {
        payload["wait"] = wait;
    }
    payload
}

#[allow(dead_code)]
fn render_bridge_error(error: Option<&Value>) -> String {
    match error {
        Some(Value::String(error)) => error.clone(),
        Some(error) => serde_json::to_string_pretty(error).unwrap_or_else(|_| error.to_string()),
        None => "browser extension returned an unknown error".into(),
    }
}

fn session_arg(args: &Value) -> Result<Option<String>, String> {
    let Some(value) = args.get("session") else {
        return Ok(None);
    };
    let Some(name) = value.as_str() else {
        return Err(errors::structured(
            errors::SESSION_REQUIRED,
            "session must be a string",
            false,
        ));
    };
    if name != "shared" && name != "workspace" {
        return Err(errors::structured(
            errors::SESSION_REQUIRED,
            "session must be shared or workspace",
            false,
        ));
    }
    Ok(Some(name.to_string()))
}

fn tab_id_arg(args: &Value) -> Result<Option<i64>, String> {
    let Some(value) = args.get("switch_tab_id") else {
        return Ok(None);
    };
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .map(Some)
        .ok_or_else(|| "switch_tab_id must be an integer tab id returned by web_scan".into())
}

fn open_tab_result(reply: BridgeReply, url: &str, filters: &BrowserUrlFilters) -> Value {
    let mut result = json!({ "tab": reply.value });
    if !filters.prefer.is_empty() {
        let preferred = filters.is_preferred(url);
        result["preferred"] = json!(preferred);
        if !preferred {
            result["prefer_hosts"] = json!(filters
                .prefer
                .iter()
                .map(|rule| rule.host.clone())
                .collect::<Vec<_>>());
        }
    }
    merge_ready_wait(result, reply.ready, reply.wait)
}

fn render_json(value: &Value) -> String {
    let rendered = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    if rendered.chars().count() <= MAX_RESULT_CHARS {
        return rendered;
    }
    let mut clipped: String = rendered.chars().take(MAX_RESULT_CHARS).collect();
    clipped.push_str("\n... browser result truncated");
    clipped
}

/// Exact-host check so `https://chatgpt.com.evil.com/` or a `chatgpt.com`
/// substring in the path/query never passes as an official ChatGPT tab.
fn is_chatgpt_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    matches!(host, "chatgpt.com" | "chat.openai.com")
}

/// Resolve a user-supplied project-relative path. Rejects absolute paths and
/// any `..`/prefix component, so tool arguments cannot escape the project
/// root ("path traversal").
fn project_relative_path(root: &Path, rel: &str) -> Result<PathBuf, String> {
    use std::path::Component;
    let trimmed = rel.trim();
    if trimmed.is_empty() {
        return Err("path must be a non-empty project-relative path".into());
    }
    let path = Path::new(trimmed);
    let inside_root = !path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
    if !inside_root {
        return Err(format!(
            "path must stay inside the project (no absolute paths or '..' segments): {trimmed}"
        ));
    }
    Ok(root.join(path))
}

fn chatgpt_tab_error(tabs: &[BrowserTab], requested: Option<i64>) -> Result<i64, String> {
    if tabs.is_empty() {
        return Err("no HTTP(S) tabs are available for ChatGPT one-shot".into());
    }
    let tab = if let Some(tab_id) = requested {
        tabs.iter().find(|tab| tab.id == tab_id).ok_or_else(|| {
            format!("browser tab {tab_id} is not available; call web_scan with tabs_only=true")
        })?
    } else {
        tabs.iter()
            .find(|tab| is_chatgpt_url(&tab.url))
            .or_else(|| tabs.iter().find(|tab| tab.active))
            .or_else(|| tabs.first())
            .ok_or_else(|| "no browser tab is selected".to_string())?
    };
    if !is_chatgpt_url(&tab.url) {
        return Err("web_agent_* currently supports only an already-open chatgpt.com tab".into());
    }
    Ok(tab.id)
}

fn human_verification_handoff(page: &Value) -> Option<Value> {
    let title = page.get("title").and_then(Value::as_str).unwrap_or("");
    let text = page.get("text").and_then(Value::as_str).unwrap_or("");
    let content = format!("{title}\n{text}").to_ascii_lowercase();
    if !content.contains("are you a robot")
        || !(content.contains("confirm you are a human") || content.contains("captcha challenge"))
    {
        return None;
    }
    Some(json!({
        "required": true,
        "reason": "captcha_challenge",
        "instruction": "Stop browser automation and ask the user to complete the human-verification challenge manually in this current visible browser tab. Wait for the user to confirm completion before scanning the same tab again.",
        "resume": "After the user confirms, call web_scan on the same tab and continue only when the challenge is no longer detected."
    }))
}

const SCAN_SCRIPT: &str = r##"(() => {
  const visible = (el) => {
    const s = getComputedStyle(el), r = el.getBoundingClientRect();
    return s.display !== 'none' && s.visibility !== 'hidden' && Number(s.opacity) > 0 && r.width > 0 && r.height > 0;
  };
  const selector = (el) => {
    if (el.id) {
      const id = '#' + CSS.escape(el.id);
      if (document.querySelectorAll(id).length === 1) return id;
    }
    const parts = [];
    for (let node = el; node && node.nodeType === 1 && parts.length < 6; node = node.parentElement) {
      let part = node.tagName.toLowerCase();
      const siblings = node.parentElement ? [...node.parentElement.children].filter(x => x.tagName === node.tagName) : [];
      if (siblings.length > 1) part += `:nth-of-type(${siblings.indexOf(node) + 1})`;
      parts.unshift(part);
      const candidate = parts.join(' > ');
      if (document.querySelectorAll(candidate).length === 1) return candidate;
    }
    return parts.join(' > ');
  };
  const query = 'a,button,input,textarea,select,summary,[role],[contenteditable=true],h1,h2,h3,label';
  const elements = [...document.querySelectorAll(query)].filter(visible).slice(0, 400).map((el) => {
    const r = el.getBoundingClientRect(), type = el.getAttribute('type') || '';
    return {
      selector: selector(el), tag: el.tagName.toLowerCase(), role: el.getAttribute('role') || undefined,
      text: (el.innerText || el.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 500) || undefined,
      aria_label: el.getAttribute('aria-label') || undefined, href: el.href || undefined, type: type || undefined,
      value: type.toLowerCase() === 'password' ? undefined : (el.value || undefined), disabled: !!el.disabled,
      rect: [Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height)]
    };
  });
  return { url: location.href, title: document.title, viewport: [innerWidth, innerHeight],
    ready_state: document.readyState,
    text: (document.body?.innerText || '').slice(0, 30000), elements };
})()"##;

const TEXT_SCAN_SCRIPT: &str = r#"(() => ({
  url: location.href,
  title: document.title,
  ready_state: document.readyState,
  text: (document.body?.innerText || '').slice(0, 50000)
}))()"#;

pub struct BrowserSetupTool {
    bridge: Arc<BrowserBridge>,
    store: Store,
}

impl BrowserSetupTool {
    pub fn new(bridge: Arc<BrowserBridge>, store: Store) -> Self {
        Self { bridge, store }
    }
}

#[async_trait]
impl Tool for BrowserSetupTool {
    fn name(&self) -> &str {
        "browser_setup"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            "Call when the user asks to configure, install, set up, or connect the real browser, and before any live page retrieval. The result is derived from the running Wisp binary's native Tauri resource directory and includes the manual settings required for unattended single and multiple downloads. Copy extension_path character-for-character and never convert it between Windows, WSL, macOS, or Linux. If status is not connected, live_retrieval is false: do not answer live, latest, current, or URL-specific questions from prior knowledge; relay the steps and wait. If refused_connection is present, relay its explanation: the extension popup can read Connected to Wisp while Wisp refuses that socket. If reload_required is true, have the user reload the extension. If extension_path_verified is false, report the missing bundled extension and never invent a path.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "description": "Optional: start_workspace (returns only once the workspace extension connects, and fails with WORKSPACE_EXTENSION_BLOCKED when the launched browser cannot load it) or stop_workspace" }
                },
                "additionalProperties": false
            }),
        )
    }

    fn preview(&self, _args: &Value) -> String {
        "show real-browser setup status and extension path".into()
    }

    async fn run(&self, args: &Value, _env: &dyn ToolEnv) -> ToolResult {
        if let Some(action) = args.get("action").and_then(Value::as_str) {
            let result = match action {
                "start_workspace" => self.bridge.start_workspace().await,
                "stop_workspace" => self.bridge.stop_workspace().await,
                _ => Err("action must be start_workspace or stop_workspace".into()),
            };
            return match result {
                Ok(value) => ToolResult::ok(render_json(&value)),
                Err(error) => ToolResult::fail(error),
            };
        }
        self.bridge.ensure_extension().await;
        let mut info = self.bridge.setup_info().await;
        let filters = browser_url_filters::load(&self.store).await;
        info["url_filters"] = json!({
            "block": filters.block,
            "prefer": filters.prefer,
            "matching": "host and subdomains; block is enforced; prefer is advisory for literature and similar tasks"
        });
        ToolResult::ok(render_json(&info))
    }
}

pub struct WebScanTool {
    bridge: Arc<BrowserBridge>,
}

impl WebScanTool {
    pub fn new(bridge: Arc<BrowserBridge>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl Tool for WebScanTool {
    fn name(&self) -> &str {
        "web_scan"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            "Read visible content and actionable elements from the user's real, persistent Chrome/Chromium session. The browser keeps its existing cookies, login state, extensions, GPU/WebGL behavior, and normal profile fingerprint. Waits until the tab's document is complete before reading (or until timeout). The result includes ready and page.ready_state; if ready is false, scan again instead of clicking a partial page. Use tabs_only first when the target tab is unclear. If the result contains human_intervention.required=true, stop browser automation, ask the user to complete the challenge in the current visible tab, and wait for confirmation before scanning again.",
            json!({
                "type": "object",
                "properties": {
                    "tabs_only": { "type": "boolean", "description": "List connected HTTP(S) tabs without reading page content" },
                    "switch_tab_id": { "type": ["integer", "string"], "description": "Tab id returned by this tool; selects that tab for this and later calls" },
                    "text_only": { "type": "boolean", "description": "Return page text without the actionable-element snapshot" },
                    "mode": { "type": "string", "description": "default | text | article. article adds images[], figures[], code_blocks[]" },
                    "session": { "type": "string", "description": "shared or workspace" }
                }
            }),
        )
    }

    fn minimum_approval(&self) -> Approval {
        Approval::Ask
    }

    fn preview(&self, args: &Value) -> String {
        if args
            .get("tabs_only")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            "list real-browser tabs".into()
        } else {
            args.get("switch_tab_id")
                .map(|tab| format!("scan real-browser tab {tab}"))
                .unwrap_or_else(|| "scan selected real-browser tab".into())
        }
    }

    async fn run(&self, args: &Value, _env: &dyn ToolEnv) -> ToolResult {
        let session = match session_arg(args) {
            Ok(session) => session,
            Err(error) => return ToolResult::fail(error),
        };
        if args
            .get("tabs_only")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return match self.bridge.tabs_on(session.as_deref()).await {
                Ok(tabs) => ToolResult::ok(render_json(&json!({ "tabs": tabs }))),
                Err(error) => ToolResult::fail(error),
            };
        }
        let tab_id = match tab_id_arg(args) {
            Ok(tab_id) => tab_id,
            Err(error) => return ToolResult::fail(error),
        };
        let text_only = args
            .get("text_only")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mode = args
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or(if text_only { "text" } else { "default" });
        if mode == "article" {
            if let Err(error) = self
                .bridge
                .require_capability(session.as_deref(), "article_scan")
                .await
            {
                return ToolResult::fail(error);
            }
        }
        let script = if mode == "article" {
            json!({ "cmd": "scan", "mode": "article" }).to_string()
        } else if mode == "text" || text_only {
            TEXT_SCAN_SCRIPT.to_string()
        } else {
            SCAN_SCRIPT.to_string()
        };
        match self
            .bridge
            .execute_on(
                session.as_deref(),
                tab_id,
                &script,
                Duration::from_millis(DEFAULT_TIMEOUT_MS),
            )
            .await
        {
            Ok(execution) => {
                let handoff = human_verification_handoff(&execution.value);
                ToolResult::ok(render_json(&merge_ready_wait(
                    json!({
                        "human_intervention": handoff,
                        "tab_id": execution.tab_id,
                        "page": execution.value
                    }),
                    execution.ready,
                    execution.wait,
                )))
            }
            Err(error) => ToolResult::fail(error),
        }
    }
}

pub struct WebExecuteJsTool {
    bridge: Arc<BrowserBridge>,
    store: Store,
}

impl WebExecuteJsTool {
    pub fn new(bridge: Arc<BrowserBridge>, store: Store) -> Self {
        Self { bridge, store }
    }
}

#[async_trait]
impl Tool for WebExecuteJsTool {
    fn name(&self) -> &str {
        "web_execute_js"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            "Execute JavaScript in a tab from the user's real, persistent Chrome/Chromium session. The extension waits until the tab's document is complete before running the script, and waits again if the script navigates. The result includes ready; if ready is false, scan again before clicking. Call web_scan first and do not guess selectors. To close tabs, never call window.close(); send {\"cmd\":\"tabs\",\"method\":\"close\",\"tabIds\":[...]} using ids returned by web_open_tab/web_scan. If web_scan reports human_intervention.required=true, do not automate the challenge; wait for the user to complete it and confirm before continuing. For a task that will trigger multiple file downloads, first tell the user how to allow automatic multiple downloads for the trusted target site at chrome://settings/content/automaticDownloads or edge://settings/content/automaticDownloads, then wait for confirmation; until confirmed, trigger at most one file download. A JSON script with cmd='cdp' may call one Chrome DevTools Protocol method for trusted input or other advanced browser actions.",
            json!({
                "type": "object",
                "properties": {
                    "script": { "type": "string", "description": "JavaScript, or a JSON command such as {\"cmd\":\"cdp\",\"method\":\"Input.dispatchMouseEvent\",\"params\":{...}}" },
                    "switch_tab_id": { "type": ["integer", "string"], "description": "Tab id returned by web_scan" },
                    "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 60000, "description": "Execution timeout in milliseconds (default 15000)" },
                    "session": { "type": "string", "description": "shared or workspace" }
                },
                "required": ["script"]
            }),
        )
    }

    fn minimum_approval(&self) -> Approval {
        Approval::Ask
    }

    fn preview(&self, args: &Value) -> String {
        let script = args.get("script").and_then(Value::as_str).unwrap_or("");
        let mut preview: String = script.chars().take(240).collect();
        if script.chars().count() > 240 {
            preview.push('…');
        }
        preview
    }

    async fn run(&self, args: &Value, _env: &dyn ToolEnv) -> ToolResult {
        let Some(script) = args
            .get("script")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|script| !script.is_empty())
        else {
            return ToolResult::fail("missing required argument 'script'");
        };
        if script.len() > MAX_SCRIPT_BYTES {
            return ToolResult::fail(format!(
                "browser script is {} bytes (maximum {MAX_SCRIPT_BYTES})",
                script.len()
            ));
        }
        if script.starts_with("window.close()") {
            return ToolResult::fail(
                "window.close() cannot close ordinary browser tabs. Use the browser-use skill's tab command: {\"cmd\":\"tabs\",\"method\":\"close\",\"tabIds\":[...]} with tab ids returned by web_open_tab/web_scan.",
            );
        }
        let filters = browser_url_filters::load(&self.store).await;
        if let Some((url, rule)) = filters.blocked_navigation(script) {
            return ToolResult::fail(browser_url_filters::block_message(&url, rule));
        }
        let tab_id = match tab_id_arg(args) {
            Ok(tab_id) => tab_id,
            Err(error) => return ToolResult::fail(error),
        };
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);
        let session = match session_arg(args) {
            Ok(session) => session,
            Err(error) => return ToolResult::fail(error),
        };
        match self
            .bridge
            .execute_on(
                session.as_deref(),
                tab_id,
                script,
                Duration::from_millis(timeout_ms),
            )
            .await
        {
            Ok(execution) => ToolResult::ok(render_json(&merge_ready_wait(
                json!({
                    "tab_id": execution.tab_id,
                    "result": execution.value
                }),
                execution.ready,
                execution.wait,
            ))),
            Err(error) => ToolResult::fail(error),
        }
    }
}

pub struct WebOpenTabTool {
    bridge: Arc<BrowserBridge>,
    store: Store,
}

impl WebOpenTabTool {
    pub fn new(bridge: Arc<BrowserBridge>, store: Store) -> Self {
        Self { bridge, store }
    }
}

#[async_trait]
impl Tool for WebOpenTabTool {
    fn name(&self) -> &str {
        "web_open_tab"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            "Open a new tab at an http(s) URL in the user's real, persistent Chrome/Chromium session. Works even when no tab is open yet, so use this to start browsing. Waits until the new tab's document is complete (or until timeout). The result includes the new tab id plus ready; if ready is false, call web_scan before acting. Pass the tab id as switch_tab_id to web_scan or web_execute_js. User-defined blocked hosts from Settings → Browser are refused before the tab opens. When url_filters.prefer is non-empty, prefer those hosts for literature and similar retrieval.",
            json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Absolute http:// or https:// URL to open" },
                    "active": { "type": "boolean", "description": "Focus the new tab (default false)" },
                    "session": { "type": "string", "description": "shared or workspace" }
                },
                "required": ["url"]
            }),
        )
    }

    fn minimum_approval(&self) -> Approval {
        Approval::Ask
    }

    fn preview(&self, args: &Value) -> String {
        let url = args.get("url").and_then(Value::as_str).unwrap_or("");
        format!("open real-browser tab at {url}")
    }

    async fn run(&self, args: &Value, _env: &dyn ToolEnv) -> ToolResult {
        let Some(url) = args
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
        else {
            return ToolResult::fail("missing required argument 'url'");
        };
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return ToolResult::fail("url must be an absolute http:// or https:// address");
        }
        let filters = browser_url_filters::load(&self.store).await;
        if let Some(rule) = filters.blocked(url) {
            return ToolResult::fail(browser_url_filters::block_message(url, rule));
        }
        let active = args.get("active").and_then(Value::as_bool).unwrap_or(false);
        let session = match session_arg(args) {
            Ok(session) => session,
            Err(error) => return ToolResult::fail(error),
        };
        match self
            .bridge
            .open_tab_on(session.as_deref(), url, active)
            .await
        {
            Ok(reply) => ToolResult::ok(render_json(&open_tab_result(reply, url, &filters))),
            Err(error) => ToolResult::fail(error),
        }
    }
}

pub struct WebScreenshotTool {
    bridge: Arc<BrowserBridge>,
}

impl WebScreenshotTool {
    pub fn new(bridge: Arc<BrowserBridge>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl Tool for WebScreenshotTool {
    fn name(&self) -> &str {
        "web_screenshot"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            "Look at what a tab in the user's real Chrome/Chromium session is showing. Waits until the tab's document is complete before capturing. Use it when web_scan's text and element snapshot is not enough: rendered layout, a chart or diagram, a canvas/WebGL page, a QR code, a PDF or image viewer, or a page that looks wrong and needs eyes. Captures the visible viewport of the tab; to reach content below the fold, scroll with web_execute_js first and capture again. Pass 'question' to say what should be read out of the screenshot.",
            json!({
                "type": "object",
                "properties": {
                    "switch_tab_id": { "type": ["integer", "string"], "description": "Tab id returned by web_scan; selects that tab for this and later calls" },
                    "question": { "type": "string", "description": "What to look for in the screenshot, e.g. 'is the login QR code visible and not expired?'" },
                    "full_page": { "type": "boolean" },
                    "selector": { "type": "string" },
                    "save_path": { "type": "string", "description": "Optional project-relative PNG path. Screenshots are not original figures." },
                    "session": { "type": "string" }
                }
            }),
        )
    }

    fn minimum_approval(&self) -> Approval {
        Approval::Ask
    }

    fn preview(&self, args: &Value) -> String {
        args.get("switch_tab_id")
            .map(|tab| format!("screenshot real-browser tab {tab}"))
            .unwrap_or_else(|| "screenshot selected real-browser tab".into())
    }

    async fn run(&self, args: &Value, env: &dyn ToolEnv) -> ToolResult {
        let tab_id = match tab_id_arg(args) {
            Ok(tab_id) => tab_id,
            Err(error) => return ToolResult::fail(error),
        };
        // Viewport JPEG stays on the vision path. full_page / selector / save_path
        // write a PNG to the project and must not be treated as an original figure.
        let session = match session_arg(args) {
            Ok(session) => session,
            Err(error) => return ToolResult::fail(error),
        };
        let full_page = args
            .get("full_page")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let selector = args.get("selector").and_then(Value::as_str);
        let save_path = args.get("save_path").and_then(Value::as_str);
        let persist_png = full_page || selector.is_some() || save_path.is_some();
        if persist_png {
            let capability = if selector.is_some() {
                "selector_screenshot"
            } else {
                "full_page_screenshot"
            };
            if let Err(error) = self
                .bridge
                .require_capability(session.as_deref(), capability)
                .await
            {
                return ToolResult::fail(error);
            }
        }
        let code = if persist_png {
            json!({ "cmd": "capture", "full_page": full_page, "selector": selector, "format": "png" }).to_string()
        } else {
            json!({
                "cmd": "cdp",
                "method": "Page.captureScreenshot",
                "params": { "format": "jpeg", "quality": 80 }
            })
            .to_string()
        };
        let execution = match self
            .bridge
            .execute_on(
                session.as_deref(),
                tab_id,
                &code,
                Duration::from_millis(DEFAULT_TIMEOUT_MS),
            )
            .await
        {
            Ok(execution) => execution,
            Err(error) => return ToolResult::fail(error),
        };
        let Some(data) = execution
            .value
            .get("data")
            .and_then(Value::as_str)
            .filter(|data| !data.is_empty())
        else {
            return ToolResult::fail("browser screenshot returned no image data");
        };
        if data.len() > MAX_SCREENSHOT_B64 {
            return ToolResult::fail(format!(
                "browser screenshot is too large ({} bytes of base64); reduce the browser window size and retry",
                data.len()
            ));
        }
        if persist_png {
            let bytes =
                match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data) {
                    Ok(bytes) => bytes,
                    Err(error) => return ToolResult::fail(format!("decode screenshot: {error}")),
                };
            let target = match save_path.map(str::trim).filter(|rel| !rel.is_empty()) {
                Some(rel) => match project_relative_path(env.project_root(), rel) {
                    Ok(path) => path,
                    Err(error) => return ToolResult::fail(format!("save_path: {error}")),
                },
                _ => {
                    let dir = env
                        .project_root()
                        .join("browser-assets")
                        .join("screenshots");
                    if let Err(error) = std::fs::create_dir_all(&dir) {
                        return ToolResult::fail(format!("create screenshot dir: {error}"));
                    }
                    dir.join(format!("shot-{}.png", Uuid::new_v4()))
                }
            };
            if let Some(parent) = target.parent() {
                if let Err(error) = std::fs::create_dir_all(parent) {
                    return ToolResult::fail(format!("create screenshot parent: {error}"));
                }
            }
            if let Err(error) = std::fs::write(&target, &bytes) {
                return ToolResult::fail(format!("write screenshot: {error}"));
            }
            let sha = hex::encode(Sha256::digest(&bytes));
            return ToolResult::ok(render_json(&json!({
                "tab_id": execution.tab_id,
                "path": target.display().to_string(),
                "bytes": bytes.len(),
                "sha256": sha,
                "note": "screenshot PNG is not an original figure"
            })));
        }
        ToolResult::image(ImageData {
            mime: "image/jpeg".into(),
            data_url: format!("data:image/jpeg;base64,{data}"),
            label: format!(
                "Screenshot of real browser tab {} ({} KB)",
                execution.tab_id,
                data.len() / 1024
            ),
        })
    }
}

pub struct WebSaveAssetsTool {
    bridge: Arc<BrowserBridge>,
}

impl WebSaveAssetsTool {
    pub fn new(bridge: Arc<BrowserBridge>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl Tool for WebSaveAssetsTool {
    fn name(&self) -> &str {
        "web_save_assets"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            "Download page assets (images, PDFs, zips) through the connected Chrome session using the extension host permission. Files are staged under Downloads/WispBrowserStaging then copied into the project. Never uses page fetch/CORS. Pass session when both browsers are connected.",
            json!({
                "type": "object",
                "properties": {
                    "urls": { "type": "array", "items": { "type": "string" }, "description": "http(s) asset URLs" },
                    "referrer": { "type": "string" },
                    "dest_dir": { "type": "string", "description": "Project-relative destination, default browser-assets" },
                    "session": { "type": "string" },
                    "switch_tab_id": { "type": ["integer", "string"] }
                },
                "required": ["urls"]
            }),
        )
    }

    fn minimum_approval(&self) -> Approval {
        Approval::Ask
    }

    fn preview(&self, args: &Value) -> String {
        let n = args
            .get("urls")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        format!("save {n} browser asset(s)")
    }

    async fn run(&self, args: &Value, env: &dyn ToolEnv) -> ToolResult {
        let session = match session_arg(args) {
            Ok(session) => session,
            Err(error) => return ToolResult::fail(error),
        };
        let Some(urls) = args.get("urls").and_then(Value::as_array) else {
            return ToolResult::fail("urls is required");
        };
        if urls.is_empty() {
            return ToolResult::fail("urls is empty");
        }
        if let Err(error) = self
            .bridge
            .require_capability(session.as_deref(), "asset_download")
            .await
        {
            return ToolResult::fail(error);
        }
        let dest_rel = args
            .get("dest_dir")
            .and_then(Value::as_str)
            .unwrap_or("browser-assets");
        let dest = match project_relative_path(env.project_root(), dest_rel) {
            Ok(dest) => dest,
            Err(error) => return ToolResult::fail(format!("dest_dir: {error}")),
        };
        if let Err(error) = std::fs::create_dir_all(&dest) {
            return ToolResult::fail(format!("create dest dir: {error}"));
        }
        let referrer = args.get("referrer").and_then(Value::as_str).unwrap_or("");
        let tab_id = match tab_id_arg(args) {
            Ok(tab_id) => tab_id,
            Err(error) => return ToolResult::fail(error),
        };
        let mut saved = Vec::new();
        for url_value in urls {
            let Some(url) = url_value.as_str() else {
                continue;
            };
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return ToolResult::fail(errors::structured(
                    errors::ASSET_BLOCKED,
                    "only http(s) asset URLs are allowed",
                    false,
                ));
            }
            let command =
                json!({ "cmd": "download", "url": url, "referrer": referrer }).to_string();
            match self
                .bridge
                .execute_on(
                    session.as_deref(),
                    tab_id,
                    &command,
                    Duration::from_millis(MAX_TIMEOUT_MS),
                )
                .await
            {
                Ok(execution) => {
                    let filename = execution
                        .value
                        .get("filename")
                        .and_then(Value::as_str)
                        .unwrap_or("WispBrowserStaging/download.bin");
                    let staged = execution
                        .value
                        .get("absolute_path")
                        .and_then(Value::as_str)
                        .filter(|path| !path.is_empty())
                        .map(PathBuf::from)
                        .unwrap_or_else(|| {
                            dirs::download_dir()
                                .unwrap_or_else(|| env.project_root().join("Downloads"))
                                .join(filename)
                        });
                    let name = staged
                        .file_name()
                        .or_else(|| Path::new(filename).file_name())
                        .unwrap_or_default();
                    let target = dest.join(name);
                    let mut copied = None;
                    for _ in 0..20 {
                        if staged.is_file() {
                            match std::fs::copy(&staged, &target) {
                                Ok(bytes) => copied = Some(bytes),
                                Err(error) => {
                                    return ToolResult::fail(format!("copy staged asset: {error}"))
                                }
                            }
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    let sha = if target.is_file() {
                        match std::fs::read(&target) {
                            Ok(bytes) => hex::encode(Sha256::digest(&bytes)),
                            Err(error) => {
                                return ToolResult::fail(format!("read copied asset: {error}"))
                            }
                        }
                    } else {
                        String::new()
                    };
                    saved.push(json!({
                        "source_url": url,
                        "path": target.display().to_string(),
                        "staged": staged.display().to_string(),
                        "copied_bytes": copied,
                        "sha256": sha,
                        "download": execution.value,
                        "ready": execution.ready
                    }));
                }
                Err(error) => return ToolResult::fail(error),
            }
        }
        ToolResult::ok(render_json(
            &json!({ "saved": saved, "dest_dir": dest.display().to_string() }),
        ))
    }
}

pub struct WebAgentSendTool {
    bridge: Arc<BrowserBridge>,
}

impl WebAgentSendTool {
    pub fn new(bridge: Arc<BrowserBridge>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl Tool for WebAgentSendTool {
    fn name(&self) -> &str {
        "web_agent_send"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(self.name(), "Send one prompt to the ChatGPT web composer in the selected real-browser session. Requires an already-logged-in chatgpt.com tab. Does not type passwords. session is required when both browsers are connected.", json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string" },
                "session": { "type": "string" },
                "switch_tab_id": { "type": ["integer", "string"] }
            },
            "required": ["prompt"]
        }))
    }
    fn minimum_approval(&self) -> Approval {
        Approval::Ask
    }
    fn preview(&self, args: &Value) -> String {
        format!(
            "send ChatGPT prompt: {}",
            args.get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(80)
                .collect::<String>()
        )
    }
    async fn run(&self, args: &Value, _env: &dyn ToolEnv) -> ToolResult {
        let session = match session_arg(args) {
            Ok(v) => v,
            Err(e) => return ToolResult::fail(e),
        };
        if let Err(error) = self
            .bridge
            .require_capability(session.as_deref(), "chatgpt_turn")
            .await
        {
            return ToolResult::fail(error);
        }
        let Some(prompt) = args
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return ToolResult::fail("prompt is required");
        };
        let requested = match tab_id_arg(args) {
            Ok(v) => v,
            Err(e) => return ToolResult::fail(e),
        };
        let tabs = match self.bridge.tabs_on(session.as_deref()).await {
            Ok(tabs) => tabs,
            Err(error) => return ToolResult::fail(error),
        };
        let tab_id = match chatgpt_tab_error(&tabs, requested) {
            Ok(tab_id) => Some(tab_id),
            Err(error) => return ToolResult::fail(error),
        };
        let ready = match self
            .bridge
            .execute_on(
                session.as_deref(),
                tab_id,
                &chatgpt::ready_script(),
                Duration::from_millis(DEFAULT_TIMEOUT_MS),
            )
            .await
        {
            Ok(ready) => ready,
            Err(error) => return ToolResult::fail(error),
        };
        if let Some(blocked) = ready.value.get("blocked").and_then(Value::as_str) {
            return ToolResult::fail(format!(
                "ChatGPT page is blocked ({blocked}). Complete login or captcha in the visible tab, then retry."
            ));
        }
        let fill = match self
            .bridge
            .execute_on(
                session.as_deref(),
                tab_id,
                &chatgpt::send_script(prompt),
                Duration::from_millis(DEFAULT_TIMEOUT_MS),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => return ToolResult::fail(e),
        };
        let sent = match self
            .bridge
            .execute_on(
                session.as_deref(),
                tab_id,
                &chatgpt::click_send_script(),
                Duration::from_millis(DEFAULT_TIMEOUT_MS),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => return ToolResult::fail(e),
        };
        ToolResult::ok(render_json(
            &json!({ "prompt": prompt, "filled": fill.value, "sent": sent.value, "tab_id": sent.tab_id }),
        ))
    }
}

pub struct WebAgentWaitTool {
    bridge: Arc<BrowserBridge>,
}

impl WebAgentWaitTool {
    pub fn new(bridge: Arc<BrowserBridge>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl Tool for WebAgentWaitTool {
    fn name(&self) -> &str {
        "web_agent_wait"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(self.name(), "Wait until the ChatGPT web turn looks complete (stop control gone / assistant text stable). Uses the Wait Engine, not document.complete.", json!({
            "type": "object",
            "properties": {
                "session": { "type": "string" },
                "switch_tab_id": { "type": ["integer", "string"] },
                "timeout_ms": { "type": "integer" }
            }
        }))
    }
    fn minimum_approval(&self) -> Approval {
        Approval::Ask
    }
    fn preview(&self, _args: &Value) -> String {
        "wait for ChatGPT web reply".into()
    }
    async fn run(&self, args: &Value, _env: &dyn ToolEnv) -> ToolResult {
        let session = match session_arg(args) {
            Ok(v) => v,
            Err(e) => return ToolResult::fail(e),
        };
        if let Err(error) = self
            .bridge
            .require_capability(session.as_deref(), "chatgpt_turn")
            .await
        {
            return ToolResult::fail(error);
        }
        let requested = match tab_id_arg(args) {
            Ok(v) => v,
            Err(e) => return ToolResult::fail(e),
        };
        let tabs = match self.bridge.tabs_on(session.as_deref()).await {
            Ok(tabs) => tabs,
            Err(error) => return ToolResult::fail(error),
        };
        let tab_id = match chatgpt_tab_error(&tabs, requested) {
            Ok(tab_id) => Some(tab_id),
            Err(error) => return ToolResult::fail(error),
        };
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(45_000)
            .clamp(1, MAX_TIMEOUT_MS);
        let started = std::time::Instant::now();
        let command =
            json!({ "cmd": "wait", "spec": chatgpt::wait_spec(), "timeoutMs": timeout_ms })
                .to_string();
        match self
            .bridge
            .execute_on(
                session.as_deref(),
                tab_id,
                &command,
                Duration::from_millis(timeout_ms),
            )
            .await
        {
            Ok(v) => ToolResult::ok(render_json(&merge_ready_wait(
                json!({
                    "tab_id": v.tab_id,
                    "result": v.value,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                    "status": if v.ready.unwrap_or(false) { "complete" } else { "waiting" }
                }),
                v.ready,
                v.wait,
            ))),
            Err(e) => ToolResult::fail(e),
        }
    }
}

pub struct WebAgentReadTool {
    bridge: Arc<BrowserBridge>,
}

impl WebAgentReadTool {
    pub fn new(bridge: Arc<BrowserBridge>) -> Self {
        Self { bridge }
    }
}

#[async_trait]
impl Tool for WebAgentReadTool {
    fn name(&self) -> &str {
        "web_agent_read"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            "Read the latest ChatGPT web assistant turn as {answer_text, citations, status}.",
            json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string" },
                    "switch_tab_id": { "type": ["integer", "string"] }
                }
            }),
        )
    }
    fn minimum_approval(&self) -> Approval {
        Approval::Ask
    }
    fn preview(&self, _args: &Value) -> String {
        "read last ChatGPT web answer".into()
    }
    async fn run(&self, args: &Value, _env: &dyn ToolEnv) -> ToolResult {
        let session = match session_arg(args) {
            Ok(v) => v,
            Err(e) => return ToolResult::fail(e),
        };
        if let Err(error) = self
            .bridge
            .require_capability(session.as_deref(), "chatgpt_turn")
            .await
        {
            return ToolResult::fail(error);
        }
        let requested = match tab_id_arg(args) {
            Ok(v) => v,
            Err(e) => return ToolResult::fail(e),
        };
        let tabs = match self.bridge.tabs_on(session.as_deref()).await {
            Ok(tabs) => tabs,
            Err(error) => return ToolResult::fail(error),
        };
        let tab_id = match chatgpt_tab_error(&tabs, requested) {
            Ok(tab_id) => Some(tab_id),
            Err(error) => return ToolResult::fail(error),
        };
        match self
            .bridge
            .execute_on(
                session.as_deref(),
                tab_id,
                &chatgpt::read_script(),
                Duration::from_millis(DEFAULT_TIMEOUT_MS),
            )
            .await
        {
            Ok(v) => {
                let mut parsed = chatgpt::parse_read(&v.value);
                parsed["tab_id"] = json!(v.tab_id);
                parsed["prompt"] = Value::Null;
                parsed["elapsed_ms"] = json!(0);
                ToolResult::ok(render_json(&merge_ready_wait(parsed, v.ready, v.wait)))
            }
            Err(e) => ToolResult::fail(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use sha2::{Digest, Sha256};

    struct NoEnv(PathBuf);

    #[async_trait]
    impl ToolEnv for NoEnv {
        fn project_root(&self) -> &std::path::Path {
            &self.0
        }

        async fn confirm(&self, _message: &str) -> bool {
            true
        }

        async fn emit(&self, _event: wisp_tools::ToolEvent) {}
    }

    async fn empty_store() -> (Store, PathBuf) {
        let tmp =
            std::env::temp_dir().join(format!("wisp_browser_tool_{}.sqlite", uuid::Uuid::new_v4()));
        (Store::open(&tmp).await.unwrap(), tmp)
    }

    #[test]
    fn manifest_key_matches_the_only_accepted_extension_origin() {
        let manifest_path = wisp_paths::browser_extension_dir()
            .unwrap()
            .join("manifest.json");
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(manifest_path).unwrap()).unwrap();
        let key = manifest["key"].as_str().unwrap();
        let der = base64::engine::general_purpose::STANDARD
            .decode(key)
            .unwrap();
        let digest = Sha256::digest(der);
        let id: String = digest[..16]
            .iter()
            .flat_map(|byte| [byte >> 4, byte & 0x0f])
            .map(|nibble| char::from(b'a' + nibble))
            .collect();
        assert_eq!(EXTENSION_ORIGIN, format!("chrome-extension://{id}"));
    }

    #[test]
    fn bridge_accepts_extension_origins_only() {
        assert!(allowed_extension_origin(Some(
            "chrome-extension://gnkjgagleagkgdlkkcianolobfdoocnp"
        )));
        assert!(!allowed_extension_origin(Some(
            "chrome-extension://abcdefghijklmnop"
        )));
        assert!(!allowed_extension_origin(Some("https://example.com")));
        assert!(!allowed_extension_origin(Some("null")));
        assert!(!allowed_extension_origin(None));
    }

    #[tokio::test]
    async fn page_access_tools_always_require_approval() {
        let bridge = Arc::new(BrowserBridge::new(PathBuf::from("extension")));
        let (store, tmp) = empty_store().await;
        assert_eq!(
            WebScanTool::new(bridge.clone()).minimum_approval(),
            Approval::Ask
        );
        assert_eq!(
            WebExecuteJsTool::new(bridge.clone(), store).minimum_approval(),
            Approval::Ask
        );
        assert_eq!(
            WebScreenshotTool::new(bridge).minimum_approval(),
            Approval::Ask
        );
        let _ = std::fs::remove_file(tmp);
    }

    #[tokio::test]
    async fn execute_js_rejects_window_close_with_the_tab_command() {
        let bridge = Arc::new(BrowserBridge::new(PathBuf::from("extension")));
        let (store, tmp) = empty_store().await;
        let result = WebExecuteJsTool::new(bridge, store)
            .run(
                &json!({ "script": "window.close(); 'close-requested'" }),
                &NoEnv(PathBuf::from(".")),
            )
            .await;
        assert!(!result.success);
        assert!(result.content.contains("\"method\":\"close\""));
        assert!(result.content.contains("tabIds"));
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn are_you_a_robot_page_requires_manual_handoff() {
        let handoff = human_verification_handoff(&json!({
            "title": "Are you a robot?",
            "text": "Please confirm you are a human by completing the captcha challenge below."
        }))
        .unwrap();

        assert_eq!(handoff["required"], true);
        assert_eq!(handoff["reason"], "captcha_challenge");
        assert!(handoff["instruction"]
            .as_str()
            .unwrap()
            .contains("Wait for the user to confirm"));
        assert!(human_verification_handoff(&json!({
            "title": "Browser automation article",
            "text": "This article asks: Are you a robot?"
        }))
        .is_none());
    }

    #[tokio::test]
    async fn setup_reports_the_extension_folder_without_requiring_approval() {
        let extension_dir = wisp_paths::browser_extension_dir().unwrap();
        let bridge = Arc::new(BrowserBridge::new(extension_dir.clone()));
        let (store, tmp) = empty_store().await;
        let info = bridge.setup_info().await;
        let expected_path = dunce::canonicalize(extension_dir).unwrap();

        assert_eq!(info["status"], "disconnected");
        assert_eq!(info["live_retrieval"], false);
        assert_eq!(info["code"], BROWSER_DISCONNECTED_CODE);
        assert_eq!(info["required_protocol"], 2);
        assert_eq!(info["bundled_extension_version"], "0.3.0");
        assert_eq!(info["extension_version"], "0.3.0");
        assert!(info["assistant_instruction"]
            .as_str()
            .unwrap()
            .contains("Do not answer live, latest, current, or URL-specific questions"));
        assert_eq!(info["runtime_os"], std::env::consts::OS);
        assert_eq!(info["path_source"], "wisp_tauri_resource_dir");
        assert_eq!(info["extension_path"], expected_path.display().to_string());
        assert_eq!(info["extension_path_verified"], true);
        assert_eq!(info["install_scope"], "once_per_browser_profile");
        assert_eq!(info["auto_launch_browser"], false);
        assert_eq!(
            info["download_automation"]["chrome_settings_url"],
            "chrome://settings/downloads"
        );
        assert_eq!(
            info["download_automation"]["setting_to_disable"],
            "Ask where to save each file before downloading"
        );
        assert_eq!(
            info["download_automation"]["multiple_downloads"]["chrome_settings_url"],
            "chrome://settings/content/automaticDownloads"
        );
        assert!(
            info["download_automation"]["multiple_downloads"]["recommended_action"]
                .as_str()
                .unwrap()
                .contains("trusted target site")
        );
        assert!(
            info["download_automation"]["multiple_downloads"]["agent_gate"]
                .as_str()
                .unwrap()
                .contains("wait for the user to confirm")
        );
        assert!(WebExecuteJsTool::new(bridge.clone(), store.clone())
            .schema()
            .function
            .description
            .contains("until confirmed, trigger at most one file download"));
        assert!(info["steps"].as_array().unwrap().iter().any(|step| step
            .as_str()
            .unwrap()
            .contains(info["extension_path"].as_str().unwrap())));
        let unavailable = bridge.unavailable_message("not connected");
        assert!(unavailable.contains(info["extension_path"].as_str().unwrap()));
        assert!(unavailable.contains(BROWSER_DISCONNECTED_MARKER));
        assert!(
            unavailable.contains("Do not answer live, latest, current, or URL-specific questions")
        );
        assert_eq!(
            BrowserSetupTool::new(bridge, store).minimum_approval(),
            Approval::Allow
        );
        let _ = std::fs::remove_file(tmp);
    }

    #[tokio::test]
    async fn setup_never_offers_an_unverified_extension_path() {
        let missing = std::env::temp_dir().join(format!(
            "wisp-browser-extension-missing-{}",
            std::process::id()
        ));
        let bridge = BrowserBridge::new(missing.clone());
        let info = bridge.setup_info().await;

        assert_eq!(info["status"], "extension_missing");
        assert_eq!(info["live_retrieval"], false);
        assert_eq!(info["code"], BROWSER_DISCONNECTED_CODE);
        assert_eq!(info["extension_path_verified"], false);
        assert!(info["extension_path"].is_null());
        assert!(info["steps"].as_array().unwrap().is_empty());
        assert!(!bridge
            .unavailable_message("not connected")
            .contains(&missing.display().to_string()));

        let incomplete = std::env::temp_dir().join(format!(
            "wisp-browser-extension-incomplete-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&incomplete).unwrap();
        std::fs::write(incomplete.join("manifest.json"), "{}").unwrap();
        let incomplete_bridge = BrowserBridge::new(incomplete.clone());
        let incomplete_info = incomplete_bridge.setup_info().await;
        assert_eq!(incomplete_info["status"], "extension_missing");
        assert!(incomplete_info["extension_path"].is_null());
        let _ = std::fs::remove_dir_all(&incomplete);
    }

    #[test]
    fn extension_page_url_matches_the_browser_scheme() {
        // Windows/Linux executables identify the browser by file stem.
        assert_eq!(
            extensions_page_url(Path::new("chrome"), &[]),
            "chrome://extensions"
        );
        assert_eq!(
            extensions_page_url(Path::new("chromium"), &[]),
            "chrome://extensions"
        );
        assert_eq!(
            extensions_page_url(Path::new("msedge"), &[]),
            "edge://extensions"
        );
        assert_eq!(
            extensions_page_url(Path::new("microsoft-edge-stable"), &[]),
            "edge://extensions"
        );
        assert_eq!(
            extensions_page_url(Path::new("brave"), &[]),
            "brave://extensions"
        );
        // macOS goes through `open -a "<App Name>"`, so the identity is an arg.
        let open = Path::new("/usr/bin/open");
        assert_eq!(
            extensions_page_url(open, &["-a".into(), "Google Chrome".into()]),
            "chrome://extensions"
        );
        assert_eq!(
            extensions_page_url(open, &["-a".into(), "Microsoft Edge".into()]),
            "edge://extensions"
        );
        assert_eq!(
            extensions_page_url(open, &["-a".into(), "Brave Browser".into()]),
            "brave://extensions"
        );
    }

    #[test]
    fn extension_setup_never_launches_a_browser_from_tests() {
        let missing = std::env::temp_dir().join(format!(
            "wisp-browser-extension-setup-missing-{}",
            std::process::id()
        ));
        let bridge = BrowserBridge::new(missing);
        let setup = bridge.open_extension_setup();

        assert!(!setup.opened);
        assert!(setup.extension_path.is_none());
    }

    #[test]
    fn tab_parser_accepts_generic_agent_numeric_and_string_ids() {
        let numeric =
            parse_tab(&json!({ "id": 7, "url": "https://a", "title": "A", "active": true }))
                .unwrap();
        let string = parse_tab(&json!({ "id": "8", "url": "https://b", "title": "B" })).unwrap();
        assert_eq!(numeric.id, 7);
        assert!(numeric.active);
        assert_eq!(string.id, 8);
    }

    #[tokio::test]
    async fn routes_execution_to_the_live_extension_and_correlates_result() {
        let bridge = Arc::new(BrowserBridge::new(PathBuf::from("extension")));
        let (tx, mut rx) = mpsc::unbounded_channel();
        bridge.install_client(1, tx).await;
        bridge
            .handle_text(
                1,
                r#"{"type":"ext_ready","tabs":[{"id":42,"url":"https://example.com","title":"Example","active":true}]}"#,
            )
            .await;

        let running = {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                bridge
                    .execute(None, "document.title", Duration::from_secs(1))
                    .await
            })
        };
        let outbound = rx.recv().await.unwrap().into_text().unwrap();
        let outbound: Value = serde_json::from_str(&outbound).unwrap();
        assert_eq!(outbound["tabId"], 42);
        assert_eq!(outbound["code"], "document.title");
        assert_eq!(outbound["timeoutMs"], 1000);
        let id = outbound["id"].as_str().unwrap();
        bridge
            .handle_text(
                1,
                &json!({ "type": "result", "id": id, "result": "Example" }).to_string(),
            )
            .await;

        let result = running.await.unwrap().unwrap();
        assert_eq!(result.tab_id, 42);
        assert_eq!(result.value, "Example");
    }

    #[tokio::test]
    async fn open_tab_creates_a_tab_without_any_existing_tab() {
        let bridge = Arc::new(BrowserBridge::new(PathBuf::from("extension")));
        let (tx, mut rx) = mpsc::unbounded_channel();
        bridge.install_client(1, tx).await;
        // No tabs_update sent: state.tabs is empty, yet open_tab must still work.

        let running = {
            let bridge = bridge.clone();
            tokio::spawn(async move { bridge.open_tab("https://example.com", true).await })
        };
        let outbound = rx.recv().await.unwrap().into_text().unwrap();
        let outbound: Value = serde_json::from_str(&outbound).unwrap();
        assert!(outbound.get("tabId").is_none());
        assert_eq!(outbound["timeoutMs"], DEFAULT_TIMEOUT_MS);
        let command: Value = serde_json::from_str(outbound["code"].as_str().unwrap()).unwrap();
        assert_eq!(command["cmd"], "tabs");
        assert_eq!(command["method"], "create");
        assert_eq!(command["url"], "https://example.com");
        assert_eq!(command["active"], true);

        let id = outbound["id"].as_str().unwrap();
        bridge
            .handle_text(
                1,
                &json!({ "type": "result", "id": id, "result": { "id": 99, "url": "https://example.com" } })
                    .to_string(),
            )
            .await;

        let reply = running.await.unwrap().unwrap();
        assert_eq!(reply.value["id"], 99);
        assert!(reply.ready.is_none());
        assert!(reply.wait.is_none());
    }

    #[tokio::test]
    async fn open_tab_and_execute_js_refuse_blocked_hosts() {
        let bridge = Arc::new(BrowserBridge::new(PathBuf::from("extension")));
        let (store, tmp) = empty_store().await;
        crate::browser_url_filters::save(
            &store,
            BrowserUrlFilters {
                block: vec![crate::browser_url_filters::BrowserUrlFilterRule {
                    host: "blocked.test".into(),
                    reason: "hijacked".into(),
                }],
                prefer: vec![crate::browser_url_filters::BrowserUrlFilterRule {
                    host: "pubmed.ncbi.nlm.nih.gov".into(),
                    reason: String::new(),
                }],
            },
        )
        .await
        .unwrap();

        let opened = WebOpenTabTool::new(bridge.clone(), store.clone())
            .run(
                &json!({ "url": "https://www.blocked.test/paper" }),
                &NoEnv(PathBuf::from(".")),
            )
            .await;
        assert!(!opened.success);
        assert!(opened.content.contains("hijacked"));
        assert!(opened.content.contains("blocked.test"));

        let navigated = WebExecuteJsTool::new(bridge.clone(), store.clone())
            .run(
                &json!({ "script": "location.href='https://blocked.test/js'" }),
                &NoEnv(PathBuf::from(".")),
            )
            .await;
        assert!(!navigated.success);
        assert!(navigated.content.contains("blocked by user URL filter"));

        let setup = BrowserSetupTool::new(bridge, store)
            .run(&json!({}), &NoEnv(PathBuf::from(".")))
            .await;
        assert!(setup.success);
        assert!(setup.content.contains("blocked.test"));
        assert!(setup.content.contains("pubmed.ncbi.nlm.nih.gov"));
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn open_tab_result_flags_non_preferred_hosts() {
        let filters = BrowserUrlFilters {
            prefer: vec![crate::browser_url_filters::BrowserUrlFilterRule {
                host: "pubmed.ncbi.nlm.nih.gov".into(),
                reason: String::new(),
            }],
            ..BrowserUrlFilters::default()
        };
        let flagged = open_tab_result(
            BridgeReply {
                value: json!({ "id": 1 }),
                ready: None,
                wait: None,
            },
            "https://scholar.google.com",
            &filters,
        );
        assert_eq!(flagged["preferred"], false);
        assert_eq!(flagged["prefer_hosts"][0], "pubmed.ncbi.nlm.nih.gov");
        let preferred = open_tab_result(
            BridgeReply {
                value: json!({ "id": 1 }),
                ready: Some(true),
                wait: Some(json!({ "until": "complete", "waited_ms": 12 })),
            },
            "https://pubmed.ncbi.nlm.nih.gov/1",
            &filters,
        );
        assert_eq!(preferred["ready"], true);
        assert_eq!(preferred["wait"]["waited_ms"], 12);
        assert_eq!(preferred["preferred"], true);
        assert!(preferred.get("prefer_hosts").is_none());
    }

    #[tokio::test]
    async fn scan_and_open_surface_ready_wait_from_the_extension() {
        let bridge = Arc::new(BrowserBridge::new(PathBuf::from("extension")));
        let (tx, mut rx) = mpsc::unbounded_channel();
        bridge.install_client(1, tx).await;
        bridge
            .handle_text(
                1,
                r#"{"type":"ext_ready","tabs":[{"id":7,"url":"https://example.com","title":"Example","active":true}]}"#,
            )
            .await;

        let scanning = {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                WebScanTool::new(bridge)
                    .run(&json!({}), &NoEnv(PathBuf::from(".")))
                    .await
            })
        };
        let outbound = rx.recv().await.unwrap().into_text().unwrap();
        let outbound: Value = serde_json::from_str(&outbound).unwrap();
        assert_eq!(outbound["timeoutMs"], DEFAULT_TIMEOUT_MS);
        let id = outbound["id"].as_str().unwrap();
        bridge
            .handle_text(
                1,
                &json!({
                    "type": "result",
                    "id": id,
                    "result": {
                        "url": "https://example.com",
                        "title": "Example",
                        "ready_state": "complete",
                        "text": "hello",
                        "elements": []
                    },
                    "ready": true,
                    "wait": { "until": "complete", "waited_ms": 80, "status": "complete" }
                })
                .to_string(),
            )
            .await;
        let scanned = scanning.await.unwrap();
        assert!(scanned.success);
        let body: Value = serde_json::from_str(&scanned.content).unwrap();
        assert_eq!(body["ready"], true);
        assert_eq!(body["wait"]["waited_ms"], 80);
        assert_eq!(body["page"]["ready_state"], "complete");

        let opening = {
            let bridge = bridge.clone();
            tokio::spawn(async move { bridge.open_tab("https://example.com/paper", false).await })
        };
        let outbound = rx.recv().await.unwrap().into_text().unwrap();
        let outbound: Value = serde_json::from_str(&outbound).unwrap();
        let id = outbound["id"].as_str().unwrap();
        bridge
            .handle_text(
                1,
                &json!({
                    "type": "result",
                    "id": id,
                    "result": { "id": 8, "url": "https://example.com/paper", "title": "Paper", "status": "loading" },
                    "ready": false,
                    "wait": { "until": "complete", "waited_ms": 14500, "timed_out": true, "status": "loading" }
                })
                .to_string(),
            )
            .await;
        let opened = opening.await.unwrap().unwrap();
        assert_eq!(opened.ready, Some(false));
        assert_eq!(opened.wait.as_ref().unwrap()["timed_out"], true);
        let rendered = open_tab_result(
            opened,
            "https://example.com/paper",
            &BrowserUrlFilters::default(),
        );
        assert_eq!(rendered["ready"], false);
        assert_eq!(rendered["tab"]["id"], 8);
    }

    #[test]
    fn scan_scripts_report_document_ready_state() {
        assert!(SCAN_SCRIPT.contains("ready_state: document.readyState"));
        assert!(TEXT_SCAN_SCRIPT.contains("ready_state: document.readyState"));
    }

    #[test]
    fn wait_tab_complete_matches_documented_contract() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../browser-extension");
        let output = std::process::Command::new("node")
            .args([
                "--test",
                "wait_tab.test.mjs",
                "protocol.test.mjs",
                "scan_page.test.mjs",
                "tab_ops.test.mjs",
                "wait_engine.test.mjs",
            ])
            .current_dir(&dir)
            .output()
            .expect("node --test should run the extension waiter contract");
        assert!(
            output.status.success(),
            "node --test wait_tab.test.mjs failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn browser_results_are_bounded_before_entering_model_context() {
        let rendered = render_json(&json!({ "text": "x".repeat(MAX_RESULT_CHARS * 2) }));
        assert!(rendered.chars().count() <= MAX_RESULT_CHARS + 40);
        assert!(rendered.ends_with("browser result truncated"));
    }

    #[tokio::test]
    async fn only_bridge_unavailability_carries_the_disconnect_marker() {
        let bridge = Arc::new(BrowserBridge::new(PathBuf::from("extension")));
        assert!(bridge
            .unavailable_message("browser extension is not connected")
            .contains(BROWSER_DISCONNECTED_MARKER));
        // A tab-level failure is not a disconnect and must not raise the
        // "no live retrieval" banner.
        let (tx, _rx) = mpsc::unbounded_channel();
        bridge.install_client(1, tx).await;
        let result = WebScanTool::new(bridge)
            .run(&json!({ "switch_tab_id": 9 }), &NoEnv(PathBuf::from(".")))
            .await;
        assert!(!result.success);
        assert!(!result.content.contains(BROWSER_DISCONNECTED_MARKER));
    }

    struct RecordingEnv {
        root: PathBuf,
        events: std::sync::Mutex<Vec<wisp_tools::ToolEvent>>,
    }

    #[async_trait]
    impl ToolEnv for RecordingEnv {
        fn project_root(&self) -> &std::path::Path {
            &self.root
        }

        async fn confirm(&self, _message: &str) -> bool {
            true
        }

        async fn emit(&self, event: wisp_tools::ToolEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    /// The tool result itself is the live-retrieval record the UI reads. A
    /// separate presentation event outlived the turn it described and revived a
    /// stale "no live retrieval" banner (#887, #921), so browser tools emit none.
    #[tokio::test]
    async fn disconnected_tools_mark_the_result_without_a_presentation() {
        let bridge = Arc::new(BrowserBridge::new(PathBuf::from("extension")));
        let env = RecordingEnv {
            root: PathBuf::from("."),
            events: std::sync::Mutex::new(Vec::new()),
        };
        let result = WebScanTool::new(bridge)
            .run(&json!({ "tabs_only": true }), &env)
            .await;
        assert!(!result.success);
        assert!(result.content.contains(BROWSER_DISCONNECTED_MARKER));
        assert!(env.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn both_sessions_require_an_explicit_session_argument() {
        let bridge = Arc::new(BrowserBridge::new(PathBuf::from("extension")));
        let (tx_a, _rx_a) = mpsc::unbounded_channel();
        let (tx_b, _rx_b) = mpsc::unbounded_channel();
        bridge.install_client_on(1, tx_a, "shared").await;
        bridge.install_client_on(2, tx_b, "workspace").await;
        let err = WebScanTool::new(bridge)
            .run(&json!({ "tabs_only": true }), &NoEnv(PathBuf::from(".")))
            .await;
        assert!(!err.success);
        assert!(err.content.contains("SESSION_REQUIRED"));
    }

    #[test]
    fn article_scan_is_not_inlined_in_the_legacy_scan_script() {
        assert!(!SCAN_SCRIPT.contains("code_blocks"));
        let scan_js = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../browser-extension/scan_page.js"),
        )
        .unwrap();
        assert!(scan_js.contains("code_blocks"));
        assert!(scan_js.contains("images"));
    }

    #[tokio::test]
    async fn article_scan_rejects_a_stale_extension() {
        let bridge = Arc::new(BrowserBridge::new(PathBuf::from("extension")));
        let (tx, _rx) = mpsc::unbounded_channel();
        bridge.install_client(1, tx).await;
        bridge
            .handle_text(
                1,
                r#"{"type":"ext_ready","tabs":[{"id":1,"url":"https://example.com","title":"E","active":true}]}"#,
            )
            .await;
        let result = WebScanTool::new(bridge)
            .run(&json!({ "mode": "article" }), &NoEnv(PathBuf::from(".")))
            .await;
        assert!(!result.success);
        assert!(result.content.contains("EXTENSION_STALE"));
    }

    #[test]
    fn chatgpt_url_helper_accepts_official_hosts_only() {
        assert!(is_chatgpt_url("https://chatgpt.com/"));
        assert!(is_chatgpt_url("https://www.chatgpt.com/"));
        assert!(is_chatgpt_url("https://chat.openai.com/c/abc"));
        assert!(is_chatgpt_url("https://CHATGPT.com/c/abc"));
        assert!(!is_chatgpt_url("https://example.com/chatgpt"));
        // Host suffix/lookalike bypasses must fail: web_agent_* would otherwise
        // fill prompts into a phishing page.
        assert!(!is_chatgpt_url("https://chatgpt.com.evil.com/"));
        assert!(!is_chatgpt_url("https://evilchatgpt.com/"));
        assert!(!is_chatgpt_url("https://evil.com/?next=chatgpt.com"));
        assert!(!is_chatgpt_url("https://evil.com/chat.openai.com"));
        assert!(!is_chatgpt_url("http://chatgpt.com/"));
        assert!(!is_chatgpt_url("javascript:alert('chatgpt.com')"));
        assert!(!is_chatgpt_url("not a url chatgpt.com"));
    }

    #[test]
    fn project_relative_path_rejects_traversal_and_absolute_paths() {
        let root = Path::new("/project");
        assert_eq!(
            project_relative_path(root, "browser-assets/shot.png").unwrap(),
            root.join("browser-assets/shot.png")
        );
        assert_eq!(
            project_relative_path(root, "./figures/a.png").unwrap(),
            root.join("./figures/a.png")
        );
        assert!(project_relative_path(root, "../outside.png").is_err());
        assert!(project_relative_path(root, "a/../../outside.png").is_err());
        assert!(project_relative_path(root, "/etc/passwd").is_err());
        assert!(project_relative_path(root, "").is_err());
        assert!(project_relative_path(root, "   ").is_err());
    }

    #[test]
    fn paused_check_parses_the_command_instead_of_sniffing_the_raw_string() {
        let background_js = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../browser-extension/background.js"),
        )
        .unwrap();
        assert!(!background_js.contains("indexOf('\"cmd\":\"control\"')"));
        assert!(background_js.contains("isControlCommand(command)"));
    }

    #[tokio::test]
    async fn a_connected_extension_outranks_an_unverifiable_bundled_copy() {
        let missing = std::env::temp_dir().join(format!(
            "wisp-browser-extension-connected-{}",
            uuid::Uuid::new_v4()
        ));
        let bridge = Arc::new(BrowserBridge::new(missing));
        let (tx, _rx) = mpsc::unbounded_channel();
        bridge.install_client(1, tx).await;
        let info = bridge.setup_info().await;

        assert_eq!(info["status"], "connected");
        assert_eq!(info["live_retrieval"], true);
        assert!(info["code"].is_null());
        assert_eq!(info["extension_path_verified"], false);
        assert_eq!(info["auto_launch_browser"], false);
    }

    fn fake_browser(loads_unpacked_extensions: bool) -> workspace::WorkspaceBrowser {
        workspace::WorkspaceBrowser {
            name: "Google Chrome".into(),
            path: PathBuf::from("/opt/chrome"),
            loads_unpacked_extensions,
        }
    }

    #[tokio::test]
    async fn workspace_start_closes_a_window_whose_extension_never_connects() {
        let bridge = Arc::new(BrowserBridge::new(PathBuf::from("extension")));
        let terminated = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = terminated.clone();

        let error = bridge
            .start_workspace_with(
                fake_browser(false),
                "/opt/wisp/browser-extension",
                Path::new("/tmp/workspace-extension"),
                Duration::from_millis(10),
                |_, _, _| Ok(4242),
                move |pid| recorder.lock().unwrap().push(pid),
            )
            .await
            .unwrap_err();

        assert!(error.contains(errors::WORKSPACE_EXTENSION_BLOCKED));
        assert!(error.contains("--load-extension"));
        assert!(error.contains("chrome://extensions"));
        // The doomed about:blank window is closed and forgotten, so a later
        // stop_workspace cannot kill an unrelated process id.
        assert_eq!(*terminated.lock().unwrap(), vec![4242]);
        assert!(bridge.state.lock().await.workspace_pid.is_none());
    }

    #[tokio::test]
    async fn workspace_start_reports_ready_only_after_the_extension_connects() {
        let bridge = Arc::new(BrowserBridge::new(PathBuf::from("extension")));
        let (tx, _rx) = mpsc::unbounded_channel();
        bridge.install_client_on(1, tx, "workspace").await;

        let status = bridge
            .start_workspace_with(
                fake_browser(true),
                "/opt/wisp/browser-extension",
                Path::new("/tmp/workspace-extension"),
                Duration::from_millis(10),
                |_, _, _| Ok(4242),
                |pid| panic!("a connected workspace must not be terminated, got {pid}"),
            )
            .await
            .unwrap();

        assert_eq!(status["connected"], true);
        assert_eq!(status["process_running"], true);
        assert_eq!(bridge.state.lock().await.workspace_pid, Some(4242));
    }

    #[test]
    fn refused_connection_explains_why_a_connected_popup_is_not_claimed() {
        assert!(refusal_summary(None).is_null());

        let foreign = refusal_summary(Some(&RefusedConnection {
            session: "shared".into(),
            origin: Some("chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
            reason: "HTTP error: 403 Forbidden".into(),
        }));
        assert_eq!(foreign["session"], "shared");
        assert_eq!(foreign["expected_origin"], EXTENSION_ORIGIN);
        assert_eq!(foreign["reason"], "HTTP error: 403 Forbidden");
        let explanation = foreign["explanation"].as_str().unwrap();
        assert!(explanation.contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(explanation.contains("Connected to Wisp"));

        let unfinished = refusal_summary(Some(&RefusedConnection {
            session: "workspace".into(),
            origin: None,
            reason: "WebSocket protocol error: Handshake not finished".into(),
        }));
        assert!(unfinished["explanation"]
            .as_str()
            .unwrap()
            .contains("without completing the Wisp extension handshake"));
    }

    #[tokio::test]
    async fn setup_names_the_refusal_and_the_stale_reload_instead_of_only_connected_false() {
        let bridge = Arc::new(BrowserBridge::new(PathBuf::from("extension")));
        bridge.state.lock().await.last_refusal = Some(RefusedConnection {
            session: "shared".into(),
            origin: Some("chrome-extension://other".into()),
            reason: "HTTP error: 403 Forbidden".into(),
        });
        let info = bridge.setup_info().await;
        assert_eq!(
            info["refused_connection"]["origin"],
            "chrome-extension://other"
        );
        assert_eq!(info["reload_required"], false);

        // A protocol-1 extension connects and its popup reads Connected, so the
        // status must name the reload rather than looking healthy.
        let (tx, _rx) = mpsc::unbounded_channel();
        bridge.install_client(1, tx).await;
        bridge
            .handle_text(1, r#"{"type":"ext_ready","protocol_version":1,"tabs":[]}"#)
            .await;
        let info = bridge.setup_info().await;
        assert_eq!(info["reload_required"], true);
        assert_eq!(info["sessions"]["shared"]["reload_required"], true);
        assert!(info["assistant_instruction"]
            .as_str()
            .unwrap()
            .contains("Reload Wisp Real Browser Bridge"));
    }

    #[test]
    fn launch_candidates_name_a_real_chrome_family_browser() {
        let candidates = browser_launch_candidates();
        assert!(!candidates.is_empty());
        let rendered: Vec<String> = candidates
            .iter()
            .map(|(program, args)| format!("{} {}", program.display(), args.join(" ")))
            .collect();
        let blob = rendered.join("\n").to_lowercase();
        assert!(
            blob.contains("chrome") || blob.contains("chromium") || blob.contains("msedge"),
            "expected a Chrome-family launch plan, got {blob}"
        );
        #[cfg(windows)]
        assert!(blob.contains("chrome.exe") || blob.contains("start"));
        #[cfg(target_os = "macos")]
        assert!(blob.contains("open") && blob.contains("google chrome"));
    }
}
