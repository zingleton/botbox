//! Tauri command surface (introduced in U2).
//!
//! U1 kept the single `app_ready` command inline in `lib.rs`. U2 introduces this
//! dedicated command module and moves `app_ready` here, then adds the U2 key
//! commands. `lib.rs` wires everything into `invoke_handler`.
//!
//! Trust-boundary contract (KTD8 / R18): app-defined commands are reachable via
//! `core:default` — they do NOT each need a per-command ACL entry (only *plugin*
//! commands need scopes). So adding a command here + to `invoke_handler` is
//! sufficient; we do not touch `capabilities/default.json` for app commands. The
//! U1 capability smoke test still asserts no unused *plugin* scopes leaked.
//!
//! ## Security invariants for the key commands (R3)
//!
//! - `generate_key` / `get_public_key` return ONLY the OpenSSH **public** key.
//! - `export_key` is the single command that emits private material, and it
//!   writes it to a file at an operator-chosen path with `0600` perms — it never
//!   returns the bytes to the webview.
//! - No command's `Ok`/`Err` value carries private key bytes; `SignerError`'s
//!   `Display` is redaction-safe and `SecretBytes` redacts in `Debug`.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use tauri::{Emitter, Manager};
use tokio::sync::Mutex as AsyncMutex;

use crate::keychain::default_key_store;
use crate::ssh::channels::{
    open_attach_pty, open_host_pty, IpcChannelSink, PaneKind, PtyChannel, PtySize,
};
use crate::ssh::connection::{
    self, host_part, ConnectConfig, ConnectionManager, HostKeyPrompt, TrustResponse,
};
use crate::ssh::forward::{self, Forward};
use crate::ssh::known_hosts::{JsonKnownHostsStore, KnownHosts};
use crate::ssh::signer::{Ed25519Signer, Signer};
use crate::store::{Bot, BotInput, BotInventory, Inventory, JsonBotStore};

/// Minimal "the webview booted" handshake (moved from `lib.rs` in U2). The
/// frontend calls it once on startup; the response is informational.
#[derive(Debug, Serialize)]
pub struct AppInfo {
    pub name: String,
    pub version: String,
}

#[tauri::command]
pub fn app_ready() -> AppInfo {
    AppInfo {
        name: env!("CARGO_PKG_NAME").to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Build the v1 signer over the platform key store (Keychain on Apple targets,
/// in-memory elsewhere). The signer is cheap and stateless apart from the store,
/// so we construct it per command rather than holding global state.
fn signer() -> Ed25519Signer {
    Ed25519Signer::new(default_key_store())
}

/// Generate the signing key if absent and return its OpenSSH public string.
///
/// Idempotent: if a key already exists, returns the existing public key without
/// overwriting (KTD2). Never returns private material.
#[tauri::command]
pub fn generate_key() -> Result<String, String> {
    signer().generate().map_err(|e| e.to_string())
}

/// Return the OpenSSH public key, or an error if no key is provisioned yet.
///
/// The frontend's always-available "public key" surface calls this; if it
/// returns the no-key error, the UI offers the generate flow.
#[tauri::command]
pub fn get_public_key() -> Result<String, String> {
    signer().public_openssh().map_err(|e| e.to_string())
}

/// Export the OpenSSH **private** key to `path` with `0600` permissions (R17).
///
/// The path comes from the frontend (v1 accepts a path argument rather than
/// opening a native save dialog — see the module note and U2 report: keeping the
/// capability allowlist minimal, no `dialog` plugin/scope added). The frontend
/// gates this behind a confirmation warning that the key is leaving the Keychain.
///
/// The private bytes are written to the file and never returned to the webview;
/// the in-memory copy is zeroized on drop (`SecretBytes`).
#[tauri::command]
pub fn export_key(path: String) -> Result<(), String> {
    let secret = signer()
        .export_openssh_private()
        .map_err(|e| e.to_string())?;
    crate::fs::write_0600(std::path::Path::new(&path), secret.expose()).map_err(|e| e.to_string())
}

// ── Bot inventory commands (U3 / R4, R5, R6) ───────────────────────────────
//
// The inventory persists to `bots.json` in the Tauri app-data dir, written
// `0600` (KTD10). The path is resolved per call from the `AppHandle` and the
// store is path-injected into `JsonBotStore`, mirroring U2's storage-trait seam
// (`store.rs` holds the `BotStore` trait + the `MemoryBotStore` fake the unit
// tests use, so tests never write to the real app-data dir).
//
// Defaults for a blank attach command / dashboard port are applied inside
// `Inventory::add`/`update` from the single source of truth in `store.rs`
// (`DEFAULT_ATTACH_COMMAND` = `tmux attach -t hermes`, `DEFAULT_DASHBOARD_PORT`
// = 9119). U4/U5/U6 read the already-defaulted values off the persisted `Bot`.

/// Build a path-injected inventory over the Tauri app-data dir. The directory is
/// created lazily on first save by `JsonBotStore`.
fn inventory(app: &tauri::AppHandle) -> Result<Inventory<JsonBotStore>, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?;
    Ok(Inventory::new(JsonBotStore::new(dir)))
}

/// List saved bots for selection (R5).
#[tauri::command]
pub fn list_bots(app: tauri::AppHandle) -> Result<Vec<Bot>, String> {
    inventory(&app)?.list().map_err(|e| e.to_string())
}

/// Return the full inventory document (bots + the persisted selection). The
/// connection layer (U4) reads `selected_bot_id` to know which bot to connect.
#[tauri::command]
pub fn get_inventory(app: tauri::AppHandle) -> Result<BotInventory, String> {
    inventory(&app)?.inventory().map_err(|e| e.to_string())
}

/// Add a bot (name + host required; blank attach/port get the Hermes defaults).
/// Returns the stored bot with its assigned id and resolved fields (R4, R6).
#[tauri::command]
pub fn add_bot(app: tauri::AppHandle, input: BotInput) -> Result<Bot, String> {
    inventory(&app)?.add(input).map_err(|e| e.to_string())
}

/// Edit an existing bot in place (R4). Only the targeted bot changes.
#[tauri::command]
pub fn update_bot(app: tauri::AppHandle, id: String, input: BotInput) -> Result<Bot, String> {
    inventory(&app)?.update(&id, input).map_err(|e| e.to_string())
}

/// Remove a bot (R4). Clears the selection if it pointed at the removed bot.
#[tauri::command]
pub fn remove_bot(app: tauri::AppHandle, id: String) -> Result<(), String> {
    inventory(&app)?.remove(&id).map_err(|e| e.to_string())
}

/// Record the active-bot selection (R5). `None` clears it. U4 reads this pointer.
#[tauri::command]
pub fn select_bot(app: tauri::AppHandle, id: Option<String>) -> Result<(), String> {
    inventory(&app)?
        .select(id.as_deref())
        .map_err(|e| e.to_string())
}

// ── Connection commands (U4 / R7, R16, R11) ─────────────────────────────────
//
// The connection layer holds one active connection (validate-before-swap) and the
// TOFU known-hosts store. State lives in [`SshState`], registered via Tauri
// `manage` in `lib.rs`. Connect runs the staged pipeline (`ssh::connection`),
// emitting frontend events for stage progress, the host-key trust prompt, and the
// terminal outcome; U7 renders each error class.
//
// Frontend event contract (consumed by `state.ts` dispatchers):
//   - `connect-stage`     { stage }                     → connecting progress
//   - `host-key-prompt`   { host, fingerprint }         → trust modal (await answer)
//   - `connected`         { botId }                     → live
//   - `connect-failed`    { kind, stage, message }      → idle + error (U7)
//   - `connection-lost`   { botId, kind, message }      → connection-lost (U7)
//
// The host-key prompt is answered out-of-band by the `trust_host` command, which
// resolves the pending oneshot keyed by host.
/// Default SSH port appended to a bot `host` that carries no explicit port.
const DEFAULT_SSH_PORT: u16 = 22;

/// The two live PTY channels (U5). Held in [`SshState`] so `pty_write` /
/// `pty_resize` can address each by pane after `open_terminals` opens them. The
/// attach slot is `Option` because the attach PTY can fail to open while the host
/// shell stays usable (KTD6 partial-open).
#[derive(Default)]
pub struct ActiveTerminals {
    host: Option<PtyChannel>,
    attach: Option<PtyChannel>,
}

/// Tauri-managed connection state (one per app): the active-connection manager, the
/// map of pending host-key trust prompts (host → responder), and the live PTY
/// channels (U5).
pub struct SshState {
    manager: Arc<ConnectionManager>,
    /// Pending trust prompts awaiting the operator's `trust_host` answer.
    pending_trust: Arc<AsyncMutex<HashMap<String, tokio::sync::oneshot::Sender<TrustResponse>>>>,
    /// The host + attach PTY channels for the active connection (U5).
    terminals: Arc<AsyncMutex<ActiveTerminals>>,
    /// The active dashboard port-forward (U6). A child of the active connection:
    /// it is bound after auth, torn down on disconnect / swap (flipping the tunnel
    /// badge inactive), and replaced when a new connection's tunnel comes up.
    forward: Arc<AsyncMutex<Option<Forward>>>,
}

impl Default for SshState {
    fn default() -> Self {
        Self {
            manager: Arc::new(ConnectionManager::new()),
            pending_trust: Arc::new(AsyncMutex::new(HashMap::new())),
            terminals: Arc::new(AsyncMutex::new(ActiveTerminals::default())),
            forward: Arc::new(AsyncMutex::new(None)),
        }
    }
}

impl SshState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Serializable connect failure for the `connect-failed` / `connection-lost`
/// events — exactly the frontend `ConnectionError` shape (kind + stage + message).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectFailedPayload {
    kind: &'static str,
    stage: &'static str,
    message: String,
}

/// `connect-stage` event payload.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct StagePayload {
    stage: &'static str,
}

/// `host-key-prompt` event payload.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct HostKeyPromptPayload {
    host: String,
    fingerprint: String,
}

/// `connected` carries the bot id.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BotIdPayload {
    bot_id: String,
}

/// `connection-lost` carries the bot id + the error class/message (U7 renders the
/// reconnect affordance).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionLostPayload {
    bot_id: String,
    kind: &'static str,
    message: String,
}

/// `tunnel-status` event payload (U6). Drives the tunnel UI: the active/inactive
/// badge, the copyable local URL, and (on wrong-port) the error to surface.
///
/// `active` true carries the loopback `url`; `active` false (teardown or a wrong
/// dashboard port) carries no url and, for a wrong port, the `errorKind` /
/// `message` so U7 can render "nothing listening on port N".
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TunnelStatusPayload {
    bot_id: String,
    active: bool,
    url: Option<String>,
    error_kind: Option<&'static str>,
    message: Option<String>,
}

/// Resolve the bot to connect: the selected bot id must match a saved bot.
fn selected_bot(app: &tauri::AppHandle) -> Result<Bot, String> {
    let inv = inventory(app)?.inventory().map_err(|e| e.to_string())?;
    let id = inv
        .selected_bot_id
        .ok_or_else(|| "no bot is selected".to_string())?;
    inv.bots
        .into_iter()
        .find(|b| b.id == id)
        .ok_or_else(|| "selected bot no longer exists".to_string())
}

/// Build the `host:port` dial string from a bot `host` (appends the default SSH
/// port when the host has none; brackets bare IPv6).
fn dial_addr(host: &str) -> String {
    if host.starts_with('[') || host.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        // Already has a port (or is a bracketed literal with one).
        host.to_string()
    } else if host.contains(':') {
        // Bare IPv6 literal — bracket it and add the port.
        format!("[{host}]:{DEFAULT_SSH_PORT}")
    } else {
        format!("{host}:{DEFAULT_SSH_PORT}")
    }
}

/// The known-hosts store over the Tauri app-data dir (0600), mirroring the bot
/// inventory path resolution.
fn known_hosts(app: &tauri::AppHandle) -> Result<Arc<KnownHosts<JsonKnownHostsStore>>, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?;
    Ok(Arc::new(KnownHosts::new(JsonKnownHostsStore::new(dir))))
}

/// Connect to the selected bot over `russh` (R7). Runs the staged pipeline with
/// validate-before-swap: a successful connect tears down any prior active
/// connection; a failure leaves it intact (the pipeline never reaches `install`).
///
/// Returns the connected bot id on success; on failure returns the
/// frontend-shaped error (the same payload also rides the `connect-failed` event).
#[tauri::command]
pub async fn connect(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
) -> Result<String, ConnectFailedString> {
    let bot = selected_bot(&app).map_err(ConnectFailedString::plain)?;
    let addr = dial_addr(&bot.host);
    let host = host_part(&addr);

    let signer: Arc<dyn Signer> = Arc::new(signer());
    let decider = known_hosts(&app).map_err(ConnectFailedString::plain)?;
    let manager = state.manager.clone();
    let pending = state.pending_trust.clone();

    // Begin-connect: the frontend already dispatched `begin-connect`; emit the
    // initial stage so the UI shows tcp-connect progress.
    emit_stage(&app, "tcp-connect");

    // Prompt bridge: forward host-key prompts to the frontend and park the
    // responder under the host so `trust_host` can resolve it.
    let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<HostKeyPrompt>(4);
    let app_for_prompt = app.clone();
    let prompt_task = tokio::spawn(async move {
        while let Some(prompt) = prompt_rx.recv().await {
            pending
                .lock()
                .await
                .insert(prompt.host.clone(), prompt.responder);
            let _ = app_for_prompt.emit(
                "host-key-prompt",
                HostKeyPromptPayload {
                    host: prompt.host,
                    fingerprint: prompt.fingerprint,
                },
            );
        }
    });

    let cfg = ConnectConfig::for_user(&bot.username);
    let result = connection::connect(&addr, &cfg, signer, decider, prompt_tx).await;
    prompt_task.abort();
    // Clear any leftover pending prompt for this host (rejected/timed out).
    state.pending_trust.lock().await.remove(&host);

    match result {
        Ok(conn) => {
            // Take the loss-event stream BEFORE installing, so the watcher does not
            // need to reach back into the manager's lock. When the driver reports
            // Lost, emit `connection-lost` so the UI offers reconnect rather than a
            // frozen terminal.
            let events = conn.take_events().await;
            let bot_id = bot.id.clone();
            if let Some(mut rx) = events {
                let app_for_loss = app.clone();
                let forward_for_loss = state.forward.clone();
                tokio::spawn(async move {
                    if let Some(connection::ConnectionEvent::Lost) = rx.recv().await {
                        // Mid-session loss: tear the dashboard forward down (frees the
                        // port, aborts in-flight forwards) and flip the tunnel badge
                        // inactive, then surface connection-lost for reconnect.
                        close_forward(&forward_for_loss).await;
                        let _ = app_for_loss.emit(
                            "tunnel-status",
                            TunnelStatusPayload {
                                bot_id: bot_id.clone(),
                                active: false,
                                url: None,
                                error_kind: None,
                                message: None,
                            },
                        );
                        let _ = app_for_loss.emit(
                            "connection-lost",
                            ConnectionLostPayload {
                                bot_id: bot_id.clone(),
                                kind: "connection-lost",
                                message: format!("connection to bot {bot_id} was lost"),
                            },
                        );
                    }
                });
            }

            // Validate-before-swap: install only now that auth succeeded — this
            // tears down any prior active connection. Close the prior connection's
            // PTY channels AND the prior dashboard forward first (their tasks
            // reference the old handle); this flips the prior tunnel badge inactive.
            close_terminals(&state.terminals).await;
            close_forward(&state.forward).await;
            manager.install(conn).await;

            let _ = app.emit("connected", BotIdPayload { bot_id: bot.id.clone() });

            // U6: eager dashboard probe → bind loopback forward → open browser. The
            // probe classifies a wrong port (KTD7) WITHOUT tearing the connection
            // down (the host/attach terminals stay usable); a wrong port surfaces as
            // an inactive tunnel carrying the wrong-port error (U7 renders it).
            establish_dashboard_tunnel(&app, &state, &bot).await;

            Ok(bot.id)
        }
        Err(e) => {
            let payload = ConnectFailedPayload {
                kind: e.kind.as_kind(),
                stage: e.stage.as_str(),
                message: e.message.clone(),
            };
            let _ = app.emit("connect-failed", payload.clone());
            Err(ConnectFailedString::from_payload(payload))
        }
    }
}

/// Answer an open host-key trust prompt (the operator clicked Trust/Reject). Keyed
/// by host; resolves the parked responder so the handshake proceeds or aborts.
#[tauri::command]
pub async fn trust_host(
    state: tauri::State<'_, SshState>,
    host: String,
    trust: bool,
) -> Result<(), String> {
    let responder = state.pending_trust.lock().await.remove(&host);
    match responder {
        Some(tx) => {
            let answer = if trust {
                TrustResponse::Trust
            } else {
                TrustResponse::Reject
            };
            tx.send(answer)
                .map_err(|_| "trust prompt is no longer awaiting an answer".to_string())
        }
        None => Err(format!("no pending host-key prompt for {host}")),
    }
}

/// Remove the saved key for a host so a changed-key mismatch can be re-trusted
/// (R16). This is the explicit recovery step a mismatch requires; it never
/// auto-trusts the new key — the next connect re-prompts.
#[tauri::command]
pub async fn remove_known_host(app: tauri::AppHandle, host: String) -> Result<(), String> {
    let kh = known_hosts(&app)?;
    kh.remove(&host).map_err(|e| e.to_string())
}

/// Tear down the active connection (operator Disconnect; R7/KTD9). Idempotent: a
/// no-op when nothing is connected. Closes the PTY channels first (stops their read
/// tasks) and then the connection.
#[tauri::command]
pub async fn disconnect(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
) -> Result<(), String> {
    // Tear down the dashboard forward first (frees the loopback port, aborts
    // in-flight forwards) and flip the tunnel badge inactive (KTD7 lifecycle).
    let had_forward = state.forward.lock().await.is_some();
    close_forward(&state.forward).await;
    if had_forward {
        let bot_id = selected_bot(&app).map(|b| b.id).unwrap_or_default();
        let _ = app.emit(
            "tunnel-status",
            TunnelStatusPayload {
                bot_id,
                active: false,
                url: None,
                error_kind: None,
                message: None,
            },
        );
    }
    close_terminals(&state.terminals).await;
    state.manager.disconnect().await;
    Ok(())
}

/// Close + clear both PTY channels (stops their read tasks). Shared by the explicit
/// Disconnect and the validate-before-swap teardown inside `connect`.
async fn close_terminals(terminals: &AsyncMutex<ActiveTerminals>) {
    let (host, attach) = {
        let mut guard = terminals.lock().await;
        (guard.host.take(), guard.attach.take())
    };
    if let Some(h) = host {
        h.close().await;
    }
    if let Some(a) = attach {
        a.close().await;
    }
}

// ── Dashboard port-forward commands (U6 / R12, R13, R11 wrong-port) ──────────
//
// The dashboard forward is a CHILD of the active connection (KTD7): it is bound
// after auth, stored in [`SshState::forward`], and torn down on disconnect / swap
// (flipping the tunnel badge inactive). `establish_dashboard_tunnel` runs the eager
// probe then binds; `open_tunnel` re-establishes it on demand; `open_dashboard`
// opens the browser at the loopback URL via the scoped `opener` plugin (R13).
//
// Frontend event contract (consumed by `connection.ts`/`state.ts`):
//   - `tunnel-status` { botId, active, url?, errorKind?, message? }
//
// `active:true` + `url` → badge active, copyable URL; `active:false` → badge
// inactive (teardown) or, with `errorKind: "wrong-dashboard-port"`, the wrong-port
// surface (U7).

/// Tear down + clear the active dashboard forward (stops the accept loop, frees the
/// loopback port, aborts in-flight forwards). Shared by disconnect, swap, and
/// re-establish. Does NOT emit `tunnel-status` — the caller decides what to emit.
async fn close_forward(forward: &AsyncMutex<Option<Forward>>) {
    let prior = forward.lock().await.take();
    if let Some(f) = prior {
        f.close();
    }
}

/// Run the eager dashboard probe and, on success, bind the loopback forward and
/// emit the active tunnel status (+ auto-open the browser). A wrong port emits an
/// inactive tunnel carrying the wrong-port error WITHOUT tearing the connection
/// down (KTD7: the host/attach terminals stay usable). Best-effort: a failure to
/// bind/probe never fails the connect — the connection is already live.
async fn establish_dashboard_tunnel(app: &tauri::AppHandle, state: &SshState, bot: &Bot) {
    // Emit the probe-dashboard stage so the connecting UI shows progress (KTD6).
    emit_stage(app, "probe-dashboard");

    let handle = match state.manager.with_active(|conn| conn.handle()).await {
        Some(h) => h,
        // Connection vanished between install and here — nothing to forward.
        None => return,
    };
    let dashboard_host = forward_dashboard_host(&bot.host);

    // Eager probe BEFORE binding the listener / opening the browser (KTD7).
    if let Err(e) = forward::probe_dashboard_port(&handle, &dashboard_host, bot.dashboard_port).await
    {
        let _ = app.emit(
            "tunnel-status",
            TunnelStatusPayload {
                bot_id: bot.id.clone(),
                active: false,
                url: None,
                error_kind: Some(e.kind.as_kind()),
                message: Some(e.message),
            },
        );
        return;
    }

    // Probe OK → bind the loopback forward and store it as a connection child.
    match forward::bind_and_forward(handle, &dashboard_host, bot.dashboard_port).await {
        Ok(fwd) => {
            let url = fwd.local_url();
            *state.forward.lock().await = Some(fwd);
            let _ = app.emit(
                "tunnel-status",
                TunnelStatusPayload {
                    bot_id: bot.id.clone(),
                    active: true,
                    url: Some(url.clone()),
                    error_kind: None,
                    message: None,
                },
            );
            // Open the browser only now that the listener is bound AND the
            // connection is authenticated (KTD7 / R13).
            open_url_via_opener(app, &url);
        }
        Err(e) => {
            // Binding the loopback listener failed (rare). Surface as an inactive
            // tunnel; the connection itself stays up.
            let _ = app.emit(
                "tunnel-status",
                TunnelStatusPayload {
                    bot_id: bot.id.clone(),
                    active: false,
                    url: None,
                    error_kind: Some(e.kind.as_kind()),
                    message: Some(e.message),
                },
            );
        }
    }
}

/// The remote host to dial the dashboard on, derived from the bot `host`. The
/// dashboard is reached *from the bot itself*, so we forward to `127.0.0.1` on the
/// remote (the dashboard binds loopback on the bot) rather than re-dialing the
/// bot's public IP from inside the SSH session.
fn forward_dashboard_host(_bot_host: &str) -> String {
    "127.0.0.1".to_string()
}

/// Open `url` in the default browser via the scoped `opener` plugin (R13). The
/// capability allowlist scopes `open_url` to loopback, so only the dashboard URL is
/// openable. Best-effort: a failure is logged, not surfaced as a connect error.
fn open_url_via_opener(app: &tauri::AppHandle, url: &str) {
    use tauri_plugin_opener::OpenerExt;
    if let Err(e) = app.opener().open_url(url.to_string(), None::<String>) {
        eprintln!("botbox: could not open dashboard URL {url}: {e}");
    }
}

/// (Re-)establish the dashboard tunnel for the active connection on demand (U6).
/// Idempotent-ish: tears down any prior forward first, then probes + binds. Returns
/// the loopback URL on success, or the wrong-port / bind error message on failure.
///
/// The connect flow already establishes the tunnel automatically; this command
/// lets the frontend retry after a transient wrong-port (e.g. the dashboard came up
/// late) without reconnecting, and is the agent-native entry point the orchestrator
/// can drive against the real bot.
#[tauri::command]
pub async fn open_tunnel(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
) -> Result<String, String> {
    let bot = selected_bot(&app)?;
    let handle = state
        .manager
        .with_active(|conn| conn.handle())
        .await
        .ok_or_else(|| "no active connection".to_string())?;
    let dashboard_host = forward_dashboard_host(&bot.host);

    // Eager probe first; a wrong port is a distinct, actionable error.
    forward::probe_dashboard_port(&handle, &dashboard_host, bot.dashboard_port)
        .await
        .map_err(|e| e.message)?;

    // Replace any prior forward (frees its port) before binding the fresh one.
    close_forward(&state.forward).await;
    let fwd = forward::bind_and_forward(handle, &dashboard_host, bot.dashboard_port)
        .await
        .map_err(|e| e.message)?;
    let url = fwd.local_url();
    *state.forward.lock().await = Some(fwd);

    let _ = app.emit(
        "tunnel-status",
        TunnelStatusPayload {
            bot_id: bot.id,
            active: true,
            url: Some(url.clone()),
            error_kind: None,
            message: None,
        },
    );
    Ok(url)
}

/// Open the dashboard's loopback URL in the default browser (R13). `url` must be the
/// loopback URL the tunnel reported; the scoped `opener` plugin rejects anything
/// outside `http://127.0.0.1:*` / `http://localhost:*`, so this cannot open
/// arbitrary URLs even though it is an app command.
#[tauri::command]
pub async fn open_dashboard(app: tauri::AppHandle, url: String) -> Result<(), String> {
    // Defense in depth: even though the opener scope is loopback-only, refuse a
    // non-loopback URL here so the app command matches the plugin scope exactly.
    // A real parse (not a prefix check) — `http://127.0.0.1:1@evil.com/` has a
    // *userinfo* of `127.0.0.1:1`; its real host is `evil.com`, which a prefix
    // match would wrongly accept.
    if !is_loopback_http_url(&url) {
        return Err(format!("refusing to open non-loopback dashboard URL: {url}"));
    }
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(url, None::<String>)
        .map_err(|e| e.to_string())
}

/// Whether `url` is an `http://` URL whose authority host is exactly a loopback
/// literal (`127.0.0.1`, `localhost`, or `[::1]`).
///
/// Parses the authority rather than prefix-matching the string, so userinfo
/// tricks like `http://127.0.0.1:1@evil.com/` (real host `evil.com`) are rejected.
/// The scheme must be exactly `http` (the dashboard is plain-HTTP loopback); any
/// `@` userinfo is rejected outright; the host must equal a loopback literal after
/// stripping an optional `:port`.
fn is_loopback_http_url(url: &str) -> bool {
    // Scheme must be exactly `http://` (case-sensitive is fine for our own URLs).
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };

    // The authority runs up to the first `/`, `?`, or `#`.
    let authority_end = rest
        .find(['/', '?', '#'])
        .unwrap_or(rest.len());
    let authority = &rest[..authority_end];

    // Any userinfo (`user[:pass]@host`) means the host is NOT the literal before
    // the `@` — reject rather than try to interpret it.
    if authority.contains('@') {
        return false;
    }

    // Strip an optional `:port`. IPv6 literals are bracketed (`[::1]:port`), so
    // for a bracketed host the port (if any) follows the closing `]`.
    let host = if authority.starts_with('[') {
        match authority.find(']') {
            // Host is the bracketed literal, INCLUDING the brackets (`[::1]`). What
            // follows the `]` must be empty or a valid `:port`; else it's malformed.
            Some(close) => {
                if !is_empty_or_port(&authority[close + 1..]) {
                    return false;
                }
                &authority[..=close]
            }
            None => return false, // malformed bracket
        }
    } else {
        match authority.rsplit_once(':') {
            Some((h, port)) => {
                // The part after `:` must be a non-empty all-digit port; otherwise
                // this `:` is not a port separator and the authority is malformed.
                if !is_port(port) {
                    return false;
                }
                h
            }
            None => authority,
        }
    };

    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

/// A non-empty all-ASCII-digit port string.
fn is_port(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Empty, or a `:port` suffix (the part after a bracketed IPv6 host's `]`).
fn is_empty_or_port(s: &str) -> bool {
    s.is_empty() || s.strip_prefix(':').is_some_and(is_port)
}

// ── PTY terminal commands (U5 / R8, R9, R10, R11 partial-open) ──────────────
//
// `open_terminals` opens BOTH PTYs off the single authenticated connection (R10),
// as independent outcomes (KTD6): the host shell and the Hermes attach PTY are
// opened separately, and an attach failure is reported without tearing the
// connection down or losing the working host shell. The frontend passes one
// `ipc::Channel<ArrayBuffer>` per pane; the per-channel read task ships raw PTY
// bytes into it (KTD4). `pty_write` / `pty_resize` then address each pane by kind.

/// The result of opening the two terminals: the host shell is required (a host
/// failure is a hard error returned from the command), and the attach outcome is
/// reported separately so the frontend can surface an attach-specific error while
/// keeping the host shell live (KTD6).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenTerminalsResult {
    /// `true` when the Hermes attach PTY opened; `false` + `attach_error` when it
    /// failed but the host shell is still usable.
    attach_ok: bool,
    /// The attach-specific error (kind + message) when `attach_ok` is false. Its
    /// `kind` is `attach-failure` (1:1 with the frontend `ConnectionErrorKind`).
    attach_error: Option<AttachErrorPayload>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AttachErrorPayload {
    kind: &'static str,
    message: String,
}

/// Open the host-shell + Hermes-attach PTYs for the active connection (U5).
///
/// `host_channel` / `attach_channel` are the per-pane `ipc::Channel`s the frontend
/// created; raw PTY bytes stream into them. `cols`/`rows` are the panes' initial
/// sizes (passed to `request_pty`). The host shell must open or the command errors;
/// the attach PTY runs the bot's configured `attach_command` (AE2) and its failure
/// is reported in [`OpenTerminalsResult`] without aborting (KTD6).
#[tauri::command]
pub async fn open_terminals(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
    host_channel: tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>,
    attach_channel: tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>,
    cols: u32,
    rows: u32,
) -> Result<OpenTerminalsResult, String> {
    let bot = selected_bot(&app)?;
    let size = PtySize::new(cols.max(1), rows.max(1));

    // Replace any prior terminals (e.g. reconnect) before opening fresh ones.
    close_terminals(&state.terminals).await;

    let host_sink = Arc::new(IpcChannelSink::new(host_channel));
    let attach_sink = Arc::new(IpcChannelSink::new(attach_channel));

    // Clone the shared `Arc<Handle>` out of the manager so the async PTY opens run
    // after the manager lock is released (the `with_active` closure cannot be async).
    // Both clones point at the SAME connection handle — the two PTYs ride one SSH
    // connection (R10).
    let handle = state
        .manager
        .with_active(|conn| conn.handle())
        .await
        .ok_or_else(|| "no active connection".to_string())?;

    // Open the host shell first. A host failure means the whole terminal surface is
    // down — return it as a hard error (no partial state stored).
    let host = open_host_pty(handle.clone(), size, host_sink)
        .await
        .map_err(|e| e.message)?;

    // Open the attach PTY independently (KTD6). Its failure does NOT abort: the host
    // shell stays usable and the attach error is reported in the result.
    let attach_result =
        open_attach_pty(handle.clone(), &bot.attach_command, size, attach_sink).await;

    let mut guard = state.terminals.lock().await;
    guard.host = Some(host);

    match attach_result {
        Ok(attach) => {
            guard.attach = Some(attach);
            Ok(OpenTerminalsResult {
                attach_ok: true,
                attach_error: None,
            })
        }
        Err(e) => Ok(OpenTerminalsResult {
            attach_ok: false,
            attach_error: Some(AttachErrorPayload {
                kind: e.kind.as_kind(),
                message: e.message,
            }),
        }),
    }
}

/// Forward operator keystrokes to a pane's remote PTY (input path; KTD4).
/// `pane` is `"host"` or `"attach"`. `data` is the raw `onData` payload.
#[tauri::command]
pub async fn pty_write(
    state: tauri::State<'_, SshState>,
    pane: String,
    data: Vec<u8>,
) -> Result<(), String> {
    let kind = parse_pane(&pane)?;
    let guard = state.terminals.lock().await;
    let channel = pane_channel(&guard, kind).ok_or_else(|| format!("{pane} terminal is not open"))?;
    channel.write(&data).await.map_err(|e| e.to_string())
}

/// Inform a pane's remote PTY of a new size (resize path → `window_change`; KTD4).
#[tauri::command]
pub async fn pty_resize(
    state: tauri::State<'_, SshState>,
    pane: String,
    cols: u32,
    rows: u32,
) -> Result<(), String> {
    let kind = parse_pane(&pane)?;
    let guard = state.terminals.lock().await;
    let channel = pane_channel(&guard, kind).ok_or_else(|| format!("{pane} terminal is not open"))?;
    channel
        .resize(PtySize::new(cols.max(1), rows.max(1)))
        .await
        .map_err(|e| e.to_string())
}

fn parse_pane(pane: &str) -> Result<PaneKind, String> {
    match pane {
        "host" => Ok(PaneKind::Host),
        "attach" => Ok(PaneKind::Attach),
        other => Err(format!("unknown pane `{other}` (expected host|attach)")),
    }
}

fn pane_channel(terminals: &ActiveTerminals, kind: PaneKind) -> Option<&PtyChannel> {
    match kind {
        PaneKind::Host => terminals.host.as_ref(),
        PaneKind::Attach => terminals.attach.as_ref(),
    }
}

fn emit_stage(app: &tauri::AppHandle, stage: &'static str) {
    let _ = app.emit("connect-stage", StagePayload { stage });
}

/// A connect failure rendered as a JSON string for the command `Err` channel,
/// carrying the same kind/stage/message the `connect-failed` event does (the
/// frontend can use either path). For a non-pipeline failure (e.g. no bot
/// selected) it degrades to a plain message with no stage.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectFailedString {
    kind: String,
    stage: Option<String>,
    message: String,
}

impl ConnectFailedString {
    fn plain(message: String) -> Self {
        Self {
            kind: "local-signer-failure".to_string(),
            stage: None,
            message,
        }
    }
    fn from_payload(p: ConnectFailedPayload) -> Self {
        Self {
            kind: p.kind.to_string(),
            stage: Some(p.stage.to_string()),
            message: p.message,
        }
    }
}

impl std::fmt::Display for ConnectFailedString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ready_reports_package_metadata() {
        let info = app_ready();
        assert_eq!(info.name, "botbox");
        assert!(!info.version.is_empty());
    }

    /// The loopback guard parses the authority instead of prefix-matching, so a
    /// userinfo trick whose real host is non-loopback is rejected, while a genuine
    /// loopback URL is accepted (defense-in-depth for `open_dashboard`).
    #[test]
    fn open_dashboard_loopback_guard_parses_authority() {
        // The exploit: `127.0.0.1:1` is USERINFO; the real host is `evil.com`.
        assert!(
            !is_loopback_http_url("http://127.0.0.1:1@evil.com/"),
            "userinfo trick must be rejected (real host is evil.com)"
        );
        assert!(!is_loopback_http_url("http://localhost@evil.com/"));
        assert!(!is_loopback_http_url("http://evil.com/"));
        assert!(!is_loopback_http_url("http://127.0.0.1.evil.com/"));
        // Non-http schemes are rejected (no https/file/etc).
        assert!(!is_loopback_http_url("https://127.0.0.1:54321"));
        assert!(!is_loopback_http_url("file:///etc/passwd"));

        // Genuine loopback URLs are accepted.
        assert!(is_loopback_http_url("http://127.0.0.1:54321"));
        assert!(is_loopback_http_url("http://127.0.0.1:54321/"));
        assert!(is_loopback_http_url("http://127.0.0.1:54321/path?q=1#f"));
        assert!(is_loopback_http_url("http://localhost:9119"));
        assert!(is_loopback_http_url("http://localhost"));
        assert!(is_loopback_http_url("http://[::1]:8080"));
        assert!(is_loopback_http_url("http://[::1]"));
    }

    /// `export_key`'s file writer creates the key file with exactly `0600`
    /// permissions. We test the writer directly (it does not depend on the
    /// Keychain) with a parseable OpenSSH private key produced by the signer over
    /// an in-memory store.
    #[cfg(unix)]
    #[test]
    fn export_writes_parseable_private_key_with_0600() {
        use crate::keychain::MemoryKeyStore;
        use crate::ssh::signer::Ed25519Signer;
        use std::os::unix::fs::PermissionsExt;

        let signer = Ed25519Signer::new(Box::new(MemoryKeyStore::new()));
        let public = signer.generate().unwrap();
        let secret = signer.export_openssh_private().unwrap();

        let dir = std::env::temp_dir();
        let path = dir.join(format!("botbox-export-test-{}.key", std::process::id()));

        crate::fs::write_0600(&path, secret.expose()).unwrap();

        // Permissions are exactly 0600.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "exported key must be owner-only");

        // The file parses as an OpenSSH private key yielding the same public key.
        let pem = std::fs::read_to_string(&path).unwrap();
        let parsed = ssh_key::private::PrivateKey::from_openssh(&pem).unwrap();
        assert_eq!(parsed.public_key().to_openssh().unwrap(), public);

        let _ = std::fs::remove_file(&path);
    }

    /// Tightening perms when the target pre-exists with looser bits.
    #[cfg(unix)]
    #[test]
    fn export_tightens_preexisting_loose_perms() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir();
        let path = dir.join(format!("botbox-export-loose-{}.key", std::process::id()));

        // Pre-create world-readable.
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        crate::fs::write_0600(&path, b"BEGIN OPENSSH PRIVATE KEY").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let _ = std::fs::remove_file(&path);
    }
}
