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
use crate::ssh::connection::{
    self, host_part, ConnectConfig, ConnectionManager, HostKeyPrompt, TrustResponse,
};
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
    write_private_key_0600(&path, secret.expose()).map_err(|e| e.to_string())
}

/// Write `bytes` to `path` creating the file with `0600` (owner read/write
/// only) from the start — the perms are part of the open, so the file is never
/// momentarily group/world-readable.
fn write_private_key_0600(path: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        f.flush()?;
        // If the file pre-existed with looser perms, `mode()` does not tighten
        // it; enforce 0600 explicitly.
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        // Non-Unix (not a v1 target) — write without the Unix perm guarantee.
        let mut f = std::fs::File::create(path)?;
        f.write_all(bytes)?;
        f.flush()?;
        Ok(())
    }
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

/// Default SSH username for bot connections. The live Hermes deploy logs in as
/// `root` (see `deploy/hetzner`); per-bot username override is a later refinement.
const DEFAULT_SSH_USERNAME: &str = "root";
/// Default SSH port appended to a bot `host` that carries no explicit port.
const DEFAULT_SSH_PORT: u16 = 22;

/// Tauri-managed connection state (one per app): the active-connection manager and
/// the map of pending host-key trust prompts (host → responder).
pub struct SshState {
    manager: Arc<ConnectionManager>,
    /// Pending trust prompts awaiting the operator's `trust_host` answer.
    pending_trust: Arc<AsyncMutex<HashMap<String, tokio::sync::oneshot::Sender<TrustResponse>>>>,
}

impl Default for SshState {
    fn default() -> Self {
        Self {
            manager: Arc::new(ConnectionManager::new()),
            pending_trust: Arc::new(AsyncMutex::new(HashMap::new())),
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

    let cfg = ConnectConfig::for_user(DEFAULT_SSH_USERNAME);
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
                tokio::spawn(async move {
                    if let Some(connection::ConnectionEvent::Lost) = rx.recv().await {
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
            // tears down any prior active connection.
            manager.install(conn).await;

            let _ = app.emit("connected", BotIdPayload { bot_id: bot.id.clone() });
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
/// no-op when nothing is connected.
#[tauri::command]
pub async fn disconnect(state: tauri::State<'_, SshState>) -> Result<(), String> {
    state.manager.disconnect().await;
    Ok(())
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
        let path_str = path.to_str().unwrap().to_string();

        write_private_key_0600(&path_str, secret.expose()).unwrap();

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
        let path_str = path.to_str().unwrap().to_string();

        // Pre-create world-readable.
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private_key_0600(&path_str, b"BEGIN OPENSSH PRIVATE KEY").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let _ = std::fs::remove_file(&path);
    }
}
