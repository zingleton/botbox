//! Connection actor + transport driver + validate-before-swap manager (U4).
//!
//! This module owns the live `russh` connection and the machinery around it:
//!
//! - **Staged connect pipeline** ([`connect`]) — drives TCP-connect → host-key
//!   check → publickey auth, tagging every failure with its [`ConnectStage`] and a
//!   distinct [`ConnectErrorKind`] (KTD6). It stops at *authenticated*; opening
//!   PTY channels (U5) and probing the dashboard (U6) are clean seams the returned
//!   [`Connection`] exposes (`handle()`), so those units open channels without
//!   re-implementing the handshake.
//! - **Connection actor** ([`Connection`]) — owns the cloneable `russh`
//!   [`Handle`]. The `Handle` is `Clone + Send`, so concurrent channel reads (U5)
//!   and writes share it without a `&mut Session`; the actual transport I/O runs in
//!   russh's own session task (spawned inside `connect_stream`). We add a **driver
//!   task** that watches the session for mid-session death and emits
//!   [`ConnectionEvent::Lost`] (loss detection), plus keepalives configured on the
//!   russh `Config`.
//! - **Host-key TOFU** (KTD5) — the client [`Handler::check_server_key`] callback
//!   runs *inside* the handshake, so the trust-prompt plumbing (a oneshot the UI
//!   resolves Trust/Reject) is installed into the handler **before** `connect()` is
//!   driven. The prompt is bounded by a configurable timeout (default 60s) that
//!   auto-rejects, which fails the handshake and drops the socket — no leaked
//!   half-open connection.
//! - **Validate-before-swap** ([`ConnectionManager`], KTD3) — one active
//!   connection. Switching bots stages the NEW connection fully (host-key + auth)
//!   before the prior active one is torn down; if the new one fails, the prior is
//!   preserved.
//!
//! Concurrency note (Risks & Dependencies): we never share a `&mut Session`. The
//! `Handle` is the only thing cloned; the session task russh spawns is the single
//! reader/writer of the socket, and our driver task only *observes* it via
//! `is_closed()`. The client handler is **non-generic** (it holds an
//! `Arc<dyn HostKeyDecider>`), so `Handle<ClientHandler>` is one concrete type
//! across the handshake and live phases — no unsound handle re-keying.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Config, Handle, Handler};
use russh::keys::ssh_encoding::Encode;
use russh::keys::ssh_key::{self, HashAlg, PublicKey};
use russh::CryptoVec;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::ssh::known_hosts::{HostKeyDecider, HostKeyDecision};
use crate::ssh::pipeline::{ConnectError, ConnectErrorKind, ConnectStage};
use crate::ssh::signer::Signer;

/// Default trust-prompt timeout (KTD5). Injectable via [`ConnectConfig`] so tests
/// use a short value instead of waiting a real minute.
pub const DEFAULT_HOST_KEY_PROMPT_TIMEOUT: Duration = Duration::from_secs(60);

/// Default TCP connect timeout (the unreachable-host classifier; KTD6).
pub const DEFAULT_TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default keepalive interval for the driver/loss-detection path.
pub const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// How many unanswered keepalives before russh closes the transport (loss).
pub const DEFAULT_KEEPALIVE_MAX: usize = 3;

/// Tunables for a connect attempt. Defaults match the plan (60s prompt); tests
/// shrink the timeouts so the bounded-prompt and unreachable paths run fast.
#[derive(Debug, Clone)]
pub struct ConnectConfig {
    pub username: String,
    pub tcp_connect_timeout: Duration,
    pub host_key_prompt_timeout: Duration,
    pub keepalive_interval: Duration,
    pub keepalive_max: usize,
}

impl ConnectConfig {
    /// Config for `user@host`-style auth with the plan defaults.
    pub fn for_user(username: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            tcp_connect_timeout: DEFAULT_TCP_CONNECT_TIMEOUT,
            host_key_prompt_timeout: DEFAULT_HOST_KEY_PROMPT_TIMEOUT,
            keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
            keepalive_max: DEFAULT_KEEPALIVE_MAX,
        }
    }

    /// Shrink the connect + prompt timeouts for tests. Keepalive left as-is.
    pub fn with_short_timeouts(mut self) -> Self {
        self.tcp_connect_timeout = Duration::from_millis(500);
        self.host_key_prompt_timeout = Duration::from_millis(100);
        self
    }
}

// ── Host-key trust prompt plumbing ──────────────────────────────────────────
//
// `check_server_key` runs inside the handshake task. On an *unknown* host it must
// (a) hand the fingerprint to the UI and (b) block until the operator answers —
// without holding a lock that would wedge the app. Two channels, installed into
// the handler BEFORE `connect()` drives the handshake:
//   - `prompt_tx`: the handler sends a `HostKeyPrompt` (fingerprint + a oneshot
//     responder) to whoever drives the connect (command layer / test).
//   - the responder oneshot: the UI resolves it Trust/Reject. A bounded timeout on
//     the handler side auto-rejects → `check_server_key` returns `false` → the
//     handshake fails and the socket is dropped (no leaked connection).

/// The operator's answer to a host-key trust prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustResponse {
    Trust,
    Reject,
}

/// A surfaced trust prompt: the SHA-256 fingerprint to show + the responder the
/// UI resolves. Sent out of the handler over the prompt channel.
#[derive(Debug)]
pub struct HostKeyPrompt {
    pub host: String,
    pub fingerprint: String,
    pub responder: oneshot::Sender<TrustResponse>,
}

// ── russh `auth::Signer` adapter over our `Signer` ──────────────────────────

/// Adapts our Keychain-backed [`Signer`] to russh's `auth::Signer`, wrapping the
/// raw 64-byte ed25519 detached signature into the SSH wire signature blob russh
/// appends to the to-sign buffer (matching russh's own agent path: an outer
/// `string` wrapping `string(algo) || string(sig)`).
///
/// A signing failure here is the **local-signer-failure** class — it surfaces as
/// [`SignerAuthError::LocalSigner`], which the pipeline maps to
/// [`ConnectErrorKind::LocalSignerFailure`], NOT a remote rejection.
struct SignerAdapter {
    signer: Arc<dyn Signer>,
}

/// Error from the signing adapter. `From<russh::SendError>` is required by the
/// `auth::Signer` trait.
#[derive(Debug)]
enum SignerAuthError {
    /// Our local signer (Keychain) failed to produce a signature.
    LocalSigner(String),
    /// Wire-encoding the signature failed.
    Encode(String),
    /// russh's internal send failed (transport went away).
    Send,
}

impl std::fmt::Display for SignerAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignerAuthError::LocalSigner(m) => write!(f, "local signer failure: {m}"),
            SignerAuthError::Encode(m) => write!(f, "signature encoding failure: {m}"),
            SignerAuthError::Send => write!(f, "transport send failure during auth"),
        }
    }
}

impl std::error::Error for SignerAuthError {}

impl From<russh::SendError> for SignerAuthError {
    fn from(_: russh::SendError) -> Self {
        SignerAuthError::Send
    }
}

impl russh::Signer for SignerAdapter {
    type Error = SignerAuthError;

    fn auth_publickey_sign(
        &mut self,
        _key: &PublicKey,
        _hash_alg: Option<HashAlg>,
        to_sign: CryptoVec,
    ) -> impl std::future::Future<Output = Result<CryptoVec, Self::Error>> + Send {
        let signer = self.signer.clone();
        async move {
            // Sign the to-sign payload with the Keychain key (raw 64-byte sig).
            let raw = signer
                .sign(&to_sign)
                .map_err(|e| SignerAuthError::LocalSigner(e.to_string()))?;

            // Inner blob `string(algo) || string(sig)` via ssh-key's Signature
            // encoder, appended to the to-sign buffer wrapped in an outer `string`
            // (4-byte length prefix) — exactly what russh's agent path produces.
            let sig = ssh_key::Signature::new(ssh_key::Algorithm::Ed25519, raw)
                .map_err(|e| SignerAuthError::Encode(e.to_string()))?;
            let mut inner: Vec<u8> = Vec::new();
            sig.encode(&mut inner)
                .map_err(|e| SignerAuthError::Encode(e.to_string()))?;

            let mut out = to_sign;
            out.extend(&(inner.len() as u32).to_be_bytes());
            out.extend(&inner);
            Ok(out)
        }
    }
}

// ── Client handler (TOFU + host-key prompt), non-generic ────────────────────

/// What the host-key check resolved to, reported out of the handler so the
/// pipeline can tag the precise error class even though the callback only returns
/// a bool.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostKeyOutcome {
    Known,
    Trusted,
    PromptRejected,
    PromptTimedOut,
    Mismatch {
        saved_fingerprint: String,
        presented_fingerprint: String,
    },
    StoreError(String),
}

/// The russh client [`Handler`]. Non-generic: it holds an `Arc<dyn
/// HostKeyDecider>` so `Handle<ClientHandler>` is one concrete type for both the
/// handshake and the live phase. Its only real job is `check_server_key` (TOFU);
/// everything else uses trait defaults. After auth the handler is dormant — the
/// cloned `Handle` carries channel I/O for U5/U6.
pub struct ClientHandler {
    host: String,
    decider: Arc<dyn HostKeyDecider>,
    prompt_tx: mpsc::Sender<HostKeyPrompt>,
    prompt_timeout: Duration,
    decision_tx: mpsc::Sender<HostKeyOutcome>,
}

impl Handler for ClientHandler {
    type Error = russh::Error;

    fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        let host = self.host.clone();
        let decider = self.decider.clone();
        let prompt_tx = self.prompt_tx.clone();
        let decision_tx = self.decision_tx.clone();
        let timeout = self.prompt_timeout;
        // Cross the known-hosts seam as an OpenSSH string: russh's `PublicKey` is a
        // distinct Rust type from the store's `ssh-key` (russh vendors a fork), but
        // the OpenSSH wire string interops (KTD2 / U2).
        let presented_openssh = server_public_key.to_openssh();
        async move {
            let presented_openssh = match presented_openssh {
                Ok(s) => s,
                Err(e) => {
                    let _ = decision_tx
                        .send(HostKeyOutcome::StoreError(format!(
                            "could not encode presented host key: {e}"
                        )))
                        .await;
                    return Ok(false);
                }
            };
            match decider.decide_openssh(&host, &presented_openssh) {
                Ok(HostKeyDecision::Known) => {
                    let _ = decision_tx.send(HostKeyOutcome::Known).await;
                    Ok(true)
                }
                Ok(HostKeyDecision::Unknown { fingerprint }) => {
                    let (responder, answer) = oneshot::channel();
                    if prompt_tx
                        .send(HostKeyPrompt {
                            host: host.clone(),
                            fingerprint,
                            responder,
                        })
                        .await
                        .is_err()
                    {
                        // No one draining prompts → fail closed.
                        let _ = decision_tx.send(HostKeyOutcome::PromptRejected).await;
                        return Ok(false);
                    }
                    match tokio::time::timeout(timeout, answer).await {
                        Ok(Ok(TrustResponse::Trust)) => {
                            if let Err(e) = decider.trust_openssh(&host, &presented_openssh) {
                                let _ = decision_tx
                                    .send(HostKeyOutcome::StoreError(e.to_string()))
                                    .await;
                                return Ok(false);
                            }
                            let _ = decision_tx.send(HostKeyOutcome::Trusted).await;
                            Ok(true)
                        }
                        Ok(Ok(TrustResponse::Reject)) | Ok(Err(_)) => {
                            let _ = decision_tx.send(HostKeyOutcome::PromptRejected).await;
                            Ok(false)
                        }
                        Err(_elapsed) => {
                            // Bounded timeout → auto-reject; handshake fails, socket
                            // dropped, no leaked half-open connection.
                            let _ = decision_tx.send(HostKeyOutcome::PromptTimedOut).await;
                            Ok(false)
                        }
                    }
                }
                Ok(HostKeyDecision::Mismatch {
                    saved_fingerprint,
                    presented_fingerprint,
                }) => {
                    // Hard stop. Never silently update; recovery is remove_known_host.
                    let _ = decision_tx
                        .send(HostKeyOutcome::Mismatch {
                            saved_fingerprint,
                            presented_fingerprint,
                        })
                        .await;
                    Ok(false)
                }
                Err(e) => {
                    let _ = decision_tx
                        .send(HostKeyOutcome::StoreError(e.to_string()))
                        .await;
                    Ok(false)
                }
            }
        }
    }
}

// ── Connection actor + driver ───────────────────────────────────────────────

/// The shared `russh` client handle U5/U6 open channels through. The `Handle`'s
/// channel-opening + I/O methods take `&self` (they only touch the internal mpsc to
/// the session task), so an `Arc` is enough to share it across the driver task and
/// the PTY/forward channels without a `&mut Session` (KTD3).
pub type SharedHandle = Arc<Handle<ClientHandler>>;

/// Events the connection actor emits to the command/UI layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionEvent {
    /// The transport died mid-session (loss detection) — offer manual reconnect.
    Lost,
}

/// A live, authenticated connection: the shared `russh` [`Handle`] plus the driver
/// task that watches for mid-session loss.
///
/// `russh`'s `Handle` is **not** `Clone` (it owns the auth-reply receiver), but all
/// the methods U5/U6 need — `channel_open_session`, `channel_open_direct_tcpip`,
/// `data`, `disconnect`, `is_closed` — take `&self` and only touch the internal
/// `Sender`, which is itself an mpsc to the session task. So we share the handle
/// behind an `Arc` (KTD3's cloned-handle concurrency, realised as a shared handle):
/// the driver task and U5/U6 hold `Arc<Handle>` and call `&self` methods
/// concurrently; we never share a `&mut Session`.
pub struct Connection {
    handle: Arc<Handle<ClientHandler>>,
    driver: tokio::task::JoinHandle<()>,
    events: Mutex<Option<mpsc::Receiver<ConnectionEvent>>>,
    /// Set true the instant an INTENTIONAL teardown begins (operator Disconnect /
    /// validate-before-swap). The driver checks it before emitting
    /// [`ConnectionEvent::Lost`], so a `is_closed()` flip caused by our own
    /// `handle.disconnect()` during `close()` is never surfaced as a phantom
    /// `connection-lost` to the UI (Fix: clean disconnect must not look like loss).
    closing: Arc<AtomicBool>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("closed", &self.handle.is_closed())
            .finish()
    }
}

impl Connection {
    /// The shared handle U5/U6 open channels through (clone the `Arc`).
    pub fn handle(&self) -> SharedHandle {
        self.handle.clone()
    }

    /// Whether the underlying session is closed (transport gone). The driver task
    /// turns this into a [`ConnectionEvent::Lost`]; callers can also poll it.
    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    /// Await the next connection event (loss). Used by tests to assert loss
    /// detection fires rather than hanging. Returns `None` once the event stream
    /// has been taken via [`Connection::take_events`].
    pub async fn next_event(&self) -> Option<ConnectionEvent> {
        let mut guard = self.events.lock().await;
        match guard.as_mut() {
            Some(rx) => rx.recv().await,
            None => None,
        }
    }

    /// Take ownership of the loss-event stream (once). The command layer takes it
    /// right after a successful connect to spawn the `connection-lost` emitter
    /// *before* the connection is handed to the manager, so loss detection does
    /// not need to reach back into the manager's lock.
    pub async fn take_events(&self) -> Option<mpsc::Receiver<ConnectionEvent>> {
        self.events.lock().await.take()
    }

    /// Clean teardown (operator Disconnect / swap): mark the connection
    /// intentionally-closing FIRST (so the driver suppresses any `Lost` raised by
    /// our own disconnect), abort the driver, then send the SSH disconnect.
    ///
    /// Ordering matters: setting `closing` before `disconnect()` means that even if
    /// the driver wakes between the abort request landing and the task actually
    /// stopping, it sees `closing == true` and does NOT emit `Lost`. The abort is
    /// requested before the disconnect so the polling task is already being torn
    /// down rather than racing the `is_closed()` flip.
    pub async fn close(self) {
        self.closing.store(true, Ordering::SeqCst);
        self.driver.abort();
        let _ = self
            .handle
            .disconnect(russh::Disconnect::ByApplication, "", "")
            .await;
    }
}

/// Spawn the driver over a shared `Arc<Handle>`. The `closing` flag lets an
/// intentional [`Connection::close`] suppress the `Lost` event that our own
/// `handle.disconnect()` would otherwise trip.
fn spawn_driver_arc(
    handle: Arc<Handle<ClientHandler>>,
    closing: Arc<AtomicBool>,
) -> (tokio::task::JoinHandle<()>, mpsc::Receiver<ConnectionEvent>) {
    let (tx, rx) = mpsc::channel(4);
    let driver = tokio::spawn(async move {
        loop {
            if handle.is_closed() {
                // Only a real, unintentional transport death is a loss. If an
                // intentional teardown set `closing`, the closure is expected —
                // stay silent so the UI never sees a phantom `connection-lost`.
                if !closing.load(Ordering::SeqCst) {
                    let _ = tx.send(ConnectionEvent::Lost).await;
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    (driver, rx)
}

// The driver task watches `is_closed()` on a short interval and emits
// [`ConnectionEvent::Lost`] the moment the session task ends (keepalive failure or
// transport close), so terminals are never left frozen. russh sends the keepalives
// itself via `Config::keepalive_interval`; the driver only observes the resulting
// closure. See `spawn_driver_arc` above.

// ── Staged connect pipeline ─────────────────────────────────────────────────

/// Run the staged connect against an already-resolved `addr` (`host:port`),
/// driving TCP-connect → host-key check → publickey auth and returning a live
/// [`Connection`] on success or a stage-tagged [`ConnectError`] on failure.
///
/// `prompt_tx` is the channel the host-key trust prompt is surfaced on; the caller
/// must be draining it concurrently and resolving the responder, or the bounded
/// timeout auto-rejects.
///
/// Heart of KTD6: each branch tags its stage so the surfaced error is
/// distinguishable. U4 stops at authenticated; `open-channels` / `probe-dashboard`
/// are left to U5/U6 via [`Connection::handle`].
pub async fn connect(
    addr: &str,
    cfg: &ConnectConfig,
    signer: Arc<dyn Signer>,
    decider: Arc<dyn HostKeyDecider>,
    prompt_tx: mpsc::Sender<HostKeyPrompt>,
) -> Result<Connection, ConnectError> {
    // Stage 1: TCP connect (with timeout) → unreachable on failure/timeout.
    let stream = match tokio::time::timeout(
        cfg.tcp_connect_timeout,
        tokio::net::TcpStream::connect(addr),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(ConnectError::new(
                ConnectErrorKind::Unreachable,
                ConnectStage::TcpConnect,
                format!("TCP connect to {addr} failed: {e}"),
            ));
        }
        Err(_elapsed) => {
            return Err(ConnectError::new(
                ConnectErrorKind::Unreachable,
                ConnectStage::TcpConnect,
                format!("TCP connect to {addr} timed out"),
            ));
        }
    };
    let _ = stream.set_nodelay(true);

    // Host-key decision is reported out-of-band so we can classify the failure.
    let (decision_tx, mut decision_rx) = mpsc::channel(2);

    let config = Arc::new(Config {
        // Keepalives drive loss detection: russh sends them and closes the
        // transport after `keepalive_max` go unanswered; the driver observes the
        // closure (KTD6).
        keepalive_interval: Some(cfg.keepalive_interval),
        keepalive_max: cfg.keepalive_max,
        ..Config::default()
    });

    let host = host_part(addr);
    let handler = ClientHandler {
        host: host.clone(),
        decider,
        prompt_tx,
        prompt_timeout: cfg.host_key_prompt_timeout,
        decision_tx,
    };

    // Stage 2: host-key check happens *inside* the handshake driven by
    // `connect_stream`. The handler was constructed with the prompt/decision
    // plumbing installed BEFORE the handshake runs (KTD5).
    let mut handle = match client::connect_stream(config, stream, handler).await {
        Ok(h) => h,
        Err(e) => return Err(classify_handshake_failure(&mut decision_rx, e)),
    };

    // Stage 3: publickey auth via our Keychain signer. A signer failure here is
    // the DISTINCT local-signer class; a server rejection is remote-auth-failure.
    let public_openssh = signer.public_openssh().map_err(|e| {
        ConnectError::new(
            ConnectErrorKind::LocalSignerFailure,
            ConnectStage::Authenticate,
            format!("could not read public key from signer: {e}"),
        )
    })?;
    let public_key = PublicKey::from_openssh(&public_openssh).map_err(|e| {
        ConnectError::new(
            ConnectErrorKind::LocalSignerFailure,
            ConnectStage::Authenticate,
            format!("signer public key did not parse: {e}"),
        )
    })?;

    let mut adapter = SignerAdapter { signer };
    let auth = handle
        .authenticate_publickey_with(&cfg.username, public_key, None, &mut adapter)
        .await;

    match auth {
        Ok(result) if result.success() => {
            let handle = Arc::new(handle);
            let closing = Arc::new(AtomicBool::new(false));
            let (driver, rx) = spawn_driver_arc(handle.clone(), closing.clone());
            Ok(Connection {
                handle,
                driver,
                events: Mutex::new(Some(rx)),
                closing,
            })
        }
        Ok(_failure) => Err(ConnectError::new(
            ConnectErrorKind::RemoteAuthFailure,
            ConnectStage::Authenticate,
            "server rejected the public key".to_string(),
        )),
        Err(SignerAuthError::LocalSigner(m)) => Err(ConnectError::new(
            ConnectErrorKind::LocalSignerFailure,
            ConnectStage::Authenticate,
            format!("local signer could not sign: {m}"),
        )),
        Err(SignerAuthError::Encode(m)) => Err(ConnectError::new(
            ConnectErrorKind::LocalSignerFailure,
            ConnectStage::Authenticate,
            format!("signature encoding failed: {m}"),
        )),
        Err(SignerAuthError::Send) => Err(ConnectError::new(
            ConnectErrorKind::RemoteAuthFailure,
            ConnectStage::Authenticate,
            "transport closed during auth".to_string(),
        )),
    }
}

/// Drain the host-key decision (if the handshake produced one) and map it to the
/// right stage-tagged error. A handshake can also fail for non-host-key reasons
/// (kex/protocol), which fall through to the socket-level (unreachable) tag.
fn classify_handshake_failure(
    decision_rx: &mut mpsc::Receiver<HostKeyOutcome>,
    err: russh::Error,
) -> ConnectError {
    match decision_rx.try_recv() {
        Ok(HostKeyOutcome::Mismatch {
            saved_fingerprint,
            presented_fingerprint,
        }) => ConnectError::new(
            ConnectErrorKind::HostKeyMismatch,
            ConnectStage::HostKeyCheck,
            format!(
                "host key changed: saved {saved_fingerprint}, presented {presented_fingerprint}"
            ),
        ),
        Ok(HostKeyOutcome::PromptRejected) => ConnectError::new(
            ConnectErrorKind::UntrustedHostKey,
            ConnectStage::HostKeyCheck,
            "host key trust prompt was rejected".to_string(),
        ),
        Ok(HostKeyOutcome::PromptTimedOut) => ConnectError::new(
            ConnectErrorKind::UntrustedHostKey,
            ConnectStage::HostKeyCheck,
            "host key trust prompt timed out".to_string(),
        ),
        Ok(HostKeyOutcome::StoreError(m)) => ConnectError::new(
            ConnectErrorKind::UntrustedHostKey,
            ConnectStage::HostKeyCheck,
            format!("known-hosts store error during host-key check: {m}"),
        ),
        // Known/Trusted but the handshake still failed, or no decision recorded →
        // a transport/protocol failure during the handshake (not a trust call).
        Ok(HostKeyOutcome::Known) | Ok(HostKeyOutcome::Trusted) | Err(_) => ConnectError::new(
            ConnectErrorKind::Unreachable,
            ConnectStage::TcpConnect,
            format!("handshake failed: {err}"),
        ),
    }
}

// ── Validate-before-swap manager ────────────────────────────────────────────

/// Holds the single active connection and performs validate-before-swap (KTD3).
///
/// The new connection is staged fully (TCP → host-key → auth) by [`connect`]
/// *before* [`ConnectionManager::install`] is called, and the prior active
/// connection is torn down only inside `install` — so a failed connect (which
/// never reaches `install`) leaves the prior connection intact and never strands
/// the operator.
pub struct ConnectionManager {
    active: Mutex<Option<Connection>>,
}

impl Default for ConnectionManager {
    fn default() -> Self {
        Self {
            active: Mutex::new(None),
        }
    }
}

impl ConnectionManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a connection is currently active.
    pub async fn is_active(&self) -> bool {
        self.active.lock().await.is_some()
    }

    /// Install `new_conn` as the active connection, tearing down any prior one
    /// only after the new one is in hand (it already authenticated before this is
    /// called — validate-before-swap). The prior is closed cleanly.
    pub async fn install(&self, new_conn: Connection) {
        let mut guard = self.active.lock().await;
        if let Some(prior) = guard.take() {
            prior.close().await;
        }
        *guard = Some(new_conn);
    }

    /// Tear down the active connection (operator Disconnect), if any.
    pub async fn disconnect(&self) {
        if let Some(conn) = self.active.lock().await.take() {
            conn.close().await;
        }
    }

    /// Run `f` against the active connection (e.g. to clone its handle for
    /// U5/U6), returning `None` if there is none.
    pub async fn with_active<R>(&self, f: impl FnOnce(&Connection) -> R) -> Option<R> {
        self.active.lock().await.as_ref().map(f)
    }
}

/// Extract the host part of a `host:port` (or bare host) address for known-hosts
/// keying. IPv6 literals in brackets are handled.
pub fn host_part(addr: &str) -> String {
    if let Some(rest) = addr.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return rest[..end].to_string();
        }
    }
    match addr.rsplit_once(':') {
        Some((h, _port)) => h.to_string(),
        None => addr.to_string(),
    }
}

#[cfg(test)]
#[path = "connection_test.rs"]
mod connection_test;
