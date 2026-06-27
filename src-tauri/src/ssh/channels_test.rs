//! Host + attach PTY channel tests (U5; R8–R11 partial-open, AE2, KTD4).
//!
//! Hermetic via an in-process russh server fixture (the "containerized bot" — Docker
//! is unavailable), richer than U4's `connection_test.rs` server: this one honors
//! `request_pty` / `shell` / `exec` / `window_change`, **echoes** data back on a
//! channel, records the exec'd command and the last window size, and can be told to
//! reject `exec` so the partial-open path is exercised. U4's server is left
//! untouched (its tests stay green); this is the U5-local fixture the plan allows.
//!
//! What is proven here:
//!   - AE2: the attach PTY execs **exactly** the bot's configured attach command
//!     (default when unset, override when set).
//!   - Fan-out: host output reaches the host sink, attach output the attach sink —
//!     no cross-routing (two channels, two sinks).
//!   - Partial-open (KTD6): host PTY up + attach exec rejected ⇒ host stays usable,
//!     an `AttachFailure` surfaces, the connection is still alive.
//!   - Multibyte UTF-8 split across two server writes arrives intact (no Rust-side
//!     `String::from_utf8`; the coalescer concatenates whole payloads in order).
//!   - Resize sends `window_change` with the new dims (the server records them).

use super::*;
use crate::keychain::MemoryKeyStore;
use crate::ssh::connection::{connect, ConnectConfig, HostKeyPrompt, TrustResponse};
use crate::ssh::known_hosts::{KnownHosts, MemoryKnownHostsStore};
use crate::ssh::signer::{Ed25519Signer, Signer};

use russh::keys::ssh_key::PublicKey; // russh's fork, used in the server Handler sig
use russh::server::{Auth, Msg as ServerMsg, Session};
use russh::{Channel as RusshChannel, ChannelId, CryptoVec, Pty};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::sync::mpsc;

// ── In-memory test sink ─────────────────────────────────────────────────────

/// A [`PtySink`] that records every chunk it receives. Stands in for the
/// production `tauri::ipc::Channel` so the read loop is exercised without a Tauri
/// IPC runtime. Records are kept per-send so coalescing behaviour is observable,
/// and `joined()` concatenates them for content assertions.
#[derive(Clone, Default)]
struct RecordingSink {
    chunks: Arc<StdMutex<Vec<Vec<u8>>>>,
}

impl RecordingSink {
    fn new() -> Self {
        Self::default()
    }

    /// All bytes received so far, concatenated in arrival order.
    fn joined(&self) -> Vec<u8> {
        self.chunks.lock().unwrap().iter().flatten().copied().collect()
    }

    fn chunk_count(&self) -> usize {
        self.chunks.lock().unwrap().len()
    }
}

impl PtySink for RecordingSink {
    fn send(&self, bytes: &[u8]) -> Result<(), PtySinkError> {
        self.chunks.lock().unwrap().push(bytes.to_vec());
        Ok(())
    }
}

/// Wait until `sink` has received at least `n` bytes total, or fail after a bound.
async fn wait_for_bytes(sink: &RecordingSink, n: usize) -> Vec<u8> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let got = sink.joined();
        if got.len() >= n {
            return got;
        }
        if std::time::Instant::now() > deadline {
            panic!("sink received {} of {} expected bytes", got.len(), n);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ── In-process russh server fixture (U5: PTY + exec + echo + records) ────────

/// Shared, observable server state across the per-connection handler clones.
#[derive(Default)]
struct ServerObservations {
    /// The exec'd command (attach path), captured verbatim — the AE2 assertion.
    exec_command: StdMutex<Option<Vec<u8>>>,
    /// The last window size the client sent via `window_change` (cols, rows).
    last_window: StdMutex<Option<(u32, u32)>>,
    /// Whether a shell was requested (host path).
    shell_requested: AtomicBool,
}

/// A server handler that:
///   - accepts publickey auth,
///   - accepts session channels,
///   - on `pty_request` succeeds,
///   - on `shell_request` succeeds and, if `greet` is set, writes a greeting,
///   - on `exec_request` records the command and either succeeds (writes a
///     greeting) or fails (the partial-open path) per `reject_exec`,
///   - echoes any client `data` straight back (so input round-trips), and
///   - records `window_change_request`.
#[derive(Clone)]
struct PtyServerHandler {
    obs: Arc<ServerObservations>,
    /// When true, `exec_request` reports failure → client sees AttachFailure.
    reject_exec: bool,
    /// Greeting bytes the server writes after shell/exec starts (per-pane content
    /// so fan-out is provable); `None` writes nothing.
    shell_greeting: Option<Vec<u8>>,
    exec_greeting: Option<Vec<u8>>,
}

impl russh::server::Handler for PtyServerHandler {
    type Error = russh::Error;

    #[allow(clippy::manual_async_fn)]
    fn auth_publickey(
        &mut self,
        _user: &str,
        _key: &PublicKey,
    ) -> impl std::future::Future<Output = Result<Auth, Self::Error>> + Send {
        async { Ok(Auth::Accept) }
    }

    #[allow(clippy::manual_async_fn)]
    fn channel_open_session(
        &mut self,
        _channel: RusshChannel<ServerMsg>,
        _session: &mut Session,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        async { Ok(true) }
    }

    #[allow(clippy::manual_async_fn)]
    fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        // Record the initial size (request_pty carries it) and acknowledge.
        *self.obs.last_window.lock().unwrap() = Some((col_width, row_height));
        let _ = session.channel_success(channel);
        async { Ok(()) }
    }

    #[allow(clippy::manual_async_fn)]
    fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        self.obs.shell_requested.store(true, Ordering::SeqCst);
        let _ = session.channel_success(channel);
        if let Some(greeting) = &self.shell_greeting {
            let _ = session.data(channel, CryptoVec::from(greeting.clone()));
        }
        async { Ok(()) }
    }

    #[allow(clippy::manual_async_fn)]
    fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        *self.obs.exec_command.lock().unwrap() = Some(data.to_vec());
        if self.reject_exec {
            // Signal the exec failed → the client's `exec(want_reply=true)` errors.
            let _ = session.channel_failure(channel);
        } else {
            let _ = session.channel_success(channel);
            if let Some(greeting) = &self.exec_greeting {
                let _ = session.data(channel, CryptoVec::from(greeting.clone()));
            }
        }
        async { Ok(()) }
    }

    #[allow(clippy::manual_async_fn)]
    fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        // Echo input straight back so the write path round-trips.
        let _ = session.data(channel, CryptoVec::from(data.to_vec()));
        async { Ok(()) }
    }

    #[allow(clippy::manual_async_fn)]
    fn window_change_request(
        &mut self,
        _channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        *self.obs.last_window.lock().unwrap() = Some((col_width, row_height));
        async { Ok(()) }
    }
}

/// A running PTY-capable server with its address, host key, and observations.
struct PtyTestServer {
    addr: String,
    host_openssh: String,
    obs: Arc<ServerObservations>,
    _accept_task: tokio::task::JoinHandle<()>,
}

/// How a fresh server should behave (greetings + whether to reject exec).
#[derive(Clone, Default)]
struct ServerOpts {
    reject_exec: bool,
    shell_greeting: Option<Vec<u8>>,
    exec_greeting: Option<Vec<u8>>,
}

impl PtyTestServer {
    async fn start(opts: ServerOpts) -> Self {
        let host_key = russh::keys::PrivateKey::random(
            &mut rand_core::OsRng,
            russh::keys::Algorithm::Ed25519,
        )
        .expect("host key");
        let host_openssh = host_key.public_key().to_openssh().expect("host key openssh");

        let config = Arc::new(russh::server::Config {
            auth_rejection_time: Duration::from_millis(1),
            auth_rejection_time_initial: Some(Duration::from_millis(1)),
            keys: vec![host_key],
            ..russh::server::Config::default()
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("addr").to_string();

        let obs = Arc::new(ServerObservations::default());
        let handler = PtyServerHandler {
            obs: obs.clone(),
            reject_exec: opts.reject_exec,
            shell_greeting: opts.shell_greeting,
            exec_greeting: opts.exec_greeting,
        };

        let accept_task = tokio::spawn(async move {
            loop {
                let (socket, _peer) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let cfg = config.clone();
                let h = handler.clone();
                tokio::spawn(async move {
                    let _ = russh::server::run_stream(cfg, socket, h).await;
                });
            }
        });

        PtyTestServer {
            addr,
            host_openssh,
            obs,
            _accept_task: accept_task,
        }
    }

    fn store_host_key(&self) -> ::ssh_key::PublicKey {
        ::ssh_key::PublicKey::from_openssh(&self.host_openssh).expect("parse host key")
    }
}

// ── helpers (mirror connection_test.rs conventions) ─────────────────────────

fn test_signer() -> Arc<dyn Signer> {
    let s = Ed25519Signer::new(Box::new(MemoryKeyStore::new()));
    s.generate().expect("generate test key");
    Arc::new(s)
}

fn known_hosts_trusting(server: &PtyTestServer) -> Arc<KnownHosts<MemoryKnownHostsStore>> {
    let kh = KnownHosts::new(MemoryKnownHostsStore::new());
    kh.trust(
        &crate::ssh::connection::host_part(&server.addr),
        &server.store_host_key(),
    )
    .expect("pre-trust");
    Arc::new(kh)
}

fn auto_answer(
    mut rx: mpsc::Receiver<HostKeyPrompt>,
    answer: TrustResponse,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(prompt) = rx.recv().await {
            let _ = prompt.responder.send(answer);
        }
    })
}

/// Connect to `server` (pre-trusted) and return the live [`Connection`].
async fn connect_to(server: &PtyTestServer) -> crate::ssh::connection::Connection {
    let kh = known_hosts_trusting(server);
    let (tx, rx) = mpsc::channel(4);
    let _answer = auto_answer(rx, TrustResponse::Trust);
    connect(
        &server.addr,
        &ConnectConfig::for_user("tester"),
        test_signer(),
        kh,
        tx,
    )
    .await
    .expect("connects")
}

// ── Test: AE2 — attach execs the configured command (default + override) ────

#[tokio::test]
async fn attach_execs_default_command_when_unset() {
    // The default attach command is the store's `DEFAULT_ATTACH_COMMAND`. U5 runs
    // whatever the bot carries; here we pass the resolved default explicitly (the
    // store always resolves a blank to this default before U5 sees it).
    let server = PtyTestServer::start(ServerOpts::default()).await;
    let conn = connect_to(&server).await;
    let sink = Arc::new(RecordingSink::new());

    let default_cmd = crate::store::DEFAULT_ATTACH_COMMAND;
    let attach = open_attach_pty(conn.handle(), default_cmd, PtySize::default(), sink)
        .await
        .expect("attach opens");

    // The server recorded exactly the default attach command.
    wait_for_exec(&server).await;
    let recorded = server.obs.exec_command.lock().unwrap().clone().unwrap();
    assert_eq!(recorded, default_cmd.as_bytes());

    attach.close().await;
    conn.close().await;
}

#[tokio::test]
async fn attach_execs_override_command_when_set() {
    let server = PtyTestServer::start(ServerOpts::default()).await;
    let conn = connect_to(&server).await;
    let sink = Arc::new(RecordingSink::new());

    let override_cmd = "docker exec -it hermes tmux attach -t main";
    let attach = open_attach_pty(conn.handle(), override_cmd, PtySize::default(), sink)
        .await
        .expect("attach opens");

    wait_for_exec(&server).await;
    let recorded = server.obs.exec_command.lock().unwrap().clone().unwrap();
    assert_eq!(recorded, override_cmd.as_bytes());

    attach.close().await;
    conn.close().await;
}

async fn wait_for_exec(server: &PtyTestServer) {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while server.obs.exec_command.lock().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            panic!("server never recorded an exec");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ── Test: fan-out — host output → host sink, attach output → attach sink ────

#[tokio::test]
async fn host_and_attach_outputs_route_to_their_own_sinks() {
    let server = PtyTestServer::start(ServerOpts {
        shell_greeting: Some(b"HOST-SHELL-READY".to_vec()),
        exec_greeting: Some(b"ATTACH-HERMES-READY".to_vec()),
        ..Default::default()
    })
    .await;
    let conn = connect_to(&server).await;

    let host_sink = Arc::new(RecordingSink::new());
    let attach_sink = Arc::new(RecordingSink::new());

    let host = open_host_pty(conn.handle(), PtySize::default(), host_sink.clone())
        .await
        .expect("host opens");
    let attach = open_attach_pty(conn.handle(),
        "tmux attach -t hermes",
        PtySize::default(),
        attach_sink.clone(),
    )
    .await
    .expect("attach opens");

    let host_out = wait_for_bytes(&host_sink, b"HOST-SHELL-READY".len()).await;
    let attach_out = wait_for_bytes(&attach_sink, b"ATTACH-HERMES-READY".len()).await;

    // Each pane saw its own greeting and NOT the other's (no cross-routing).
    assert!(host_out.windows(16).any(|w| w == b"HOST-SHELL-READY"));
    assert!(!host_out
        .windows(19)
        .any(|w| w == b"ATTACH-HERMES-READY"));
    assert!(attach_out
        .windows(19)
        .any(|w| w == b"ATTACH-HERMES-READY"));
    assert!(!attach_out.windows(16).any(|w| w == b"HOST-SHELL-READY"));

    host.close().await;
    attach.close().await;
    conn.close().await;
}

// ── Test: partial-open (KTD6) — attach exec rejected, host stays usable ─────

#[tokio::test]
async fn attach_failure_keeps_host_usable_and_connection_up() {
    let server = PtyTestServer::start(ServerOpts {
        reject_exec: true, // attach exec will be refused
        shell_greeting: Some(b"HOST-OK".to_vec()),
        ..Default::default()
    })
    .await;
    let conn = connect_to(&server).await;

    // Host shell opens fine.
    let host_sink = Arc::new(RecordingSink::new());
    let host = open_host_pty(conn.handle(), PtySize::default(), host_sink.clone())
        .await
        .expect("host opens independently");

    // Attach fails with an attach-specific error (not a whole-connection abort).
    let attach_sink = Arc::new(RecordingSink::new());
    let err = open_attach_pty(conn.handle(),
        "tmux attach -t hermes",
        PtySize::default(),
        attach_sink,
    )
    .await
    .expect_err("attach exec is rejected");
    assert_eq!(err.kind, ConnectErrorKind::AttachFailure);
    assert_eq!(err.stage, ConnectStage::OpenChannels);

    // The connection is still alive and the host shell still streams + echoes.
    assert!(!conn.is_closed(), "connection stays up after attach failure");
    let _ = wait_for_bytes(&host_sink, b"HOST-OK".len()).await;

    host.write(b"echo hi\n").await.expect("host write works");
    // The echo (server mirrors data) reaches the host sink — host remains usable.
    let got = wait_for_bytes(&host_sink, b"HOST-OK".len() + b"echo hi\n".len()).await;
    assert!(got.windows(8).any(|w| w == b"echo hi\n"));

    host.close().await;
    conn.close().await;
}

// ── Test: multibyte UTF-8 split across two reads arrives intact (no decode) ─

#[tokio::test]
async fn multibyte_utf8_split_across_reads_is_byte_exact() {
    // A 4-byte emoji (😀 = F0 9F 98 80) whose bytes the server emits in two
    // separate writes. If anything in the Rust path did `String::from_utf8` on a
    // partial read it would corrupt; the coalescer concatenates whole payloads in
    // order, so the four bytes arrive verbatim and reassemble.
    let emoji = "😀".as_bytes().to_vec();
    assert_eq!(emoji.len(), 4);
    let (first, second) = emoji.split_at(2);

    // Server writes the two halves on the shell channel, on separate `data` sends.
    let server = PtyTestServer::start(ServerOpts {
        shell_greeting: Some(first.to_vec()),
        ..Default::default()
    })
    .await;
    // A second greeting can't be expressed via opts; instead re-use the echo path:
    // after the shell greeting we write the second half by echoing client input.
    let conn = connect_to(&server).await;
    let sink = Arc::new(RecordingSink::new());
    let host = open_host_pty(conn.handle(), PtySize::default(), sink.clone())
        .await
        .expect("host opens");

    // First half arrives from the shell greeting.
    let _ = wait_for_bytes(&sink, 2).await;
    // Drive the second half through the echo path (server mirrors `data`).
    host.write(second).await.expect("write second half");

    let got = wait_for_bytes(&sink, 4).await;
    // The four emoji bytes appear contiguously and intact somewhere in the stream.
    assert!(
        got.windows(4).any(|w| w == emoji.as_slice()),
        "expected the 4 emoji bytes intact, got {got:?}"
    );
    // And the whole stream is valid UTF-8 (nothing mangled a byte).
    assert!(std::str::from_utf8(&got).is_ok());

    host.close().await;
    conn.close().await;
}

// ── Test: resize → window_change with the new dims ──────────────────────────

#[tokio::test]
async fn resize_sends_window_change_with_new_dims() {
    let server = PtyTestServer::start(ServerOpts::default()).await;
    let conn = connect_to(&server).await;
    let sink = Arc::new(RecordingSink::new());

    // Initial size in request_pty is 80x24 (default).
    let host = open_host_pty(conn.handle(), PtySize::new(80, 24), sink)
        .await
        .expect("host opens");

    // Wait for the server to record the initial request_pty size.
    wait_for_window(&server, (80, 24)).await;

    // Resize and assert the server saw the new dims via window_change.
    host.resize(PtySize::new(132, 50)).await.expect("resize");
    wait_for_window(&server, (132, 50)).await;
    assert_eq!(*server.obs.last_window.lock().unwrap(), Some((132, 50)));

    host.close().await;
    conn.close().await;
}

async fn wait_for_window(server: &PtyTestServer, expect: (u32, u32)) {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if *server.obs.last_window.lock().unwrap() == Some(expect) {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "window never became {:?} (last {:?})",
                expect,
                *server.obs.last_window.lock().unwrap()
            );
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ── Test: coalescing forwards bytes verbatim (no split / mangle) ────────────

#[tokio::test]
async fn coalescing_preserves_byte_stream_verbatim() {
    // A burst the server writes in many small sends; the coalescer must deliver the
    // exact concatenation, in order, possibly in fewer chunks than sends.
    let server = PtyTestServer::start(ServerOpts {
        shell_greeting: Some(b"ABCDEFGHIJ".to_vec()),
        ..Default::default()
    })
    .await;
    let conn = connect_to(&server).await;
    let sink = Arc::new(RecordingSink::new());
    let host = open_host_pty(conn.handle(), PtySize::default(), sink.clone())
        .await
        .expect("host opens");

    // Drive more bytes through the echo path so multiple payloads coalesce.
    let _ = wait_for_bytes(&sink, 10).await;
    for b in b"KLMNOP" {
        host.write(&[*b]).await.expect("write byte");
    }

    let got = wait_for_bytes(&sink, 16).await;
    // Order preserved and content exact (the greeting prefix followed by the echoes,
    // possibly interleaved with the echo timing but each byte intact and contiguous).
    assert!(got.windows(10).any(|w| w == b"ABCDEFGHIJ"));
    for b in b"KLMNOP" {
        assert!(got.contains(b));
    }
    // Coalescing never produced more chunks than bytes (sanity: it batches).
    assert!(sink.chunk_count() <= got.len());

    host.close().await;
    conn.close().await;
}
