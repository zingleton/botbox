//! Dashboard port-forward + eager-probe tests (U6; R12, R13, R11 wrong-port,
//! KTD7, AE4).
//!
//! Hermetic via an in-process russh server fixture that honors `direct-tcpip`
//! channel opens (the "containerized bot" — Docker is unavailable), mirroring the
//! U4/U5 fixtures. This server, on a `direct-tcpip` open:
//!   - if the requested port is its configured **dashboard port**, accepts the
//!     channel and proxies it to a tiny **local test HTTP server** (so a client
//!     hitting the loopback forward port gets a known response through the tunnel),
//!   - otherwise **refuses** the channel open (returns `Ok(false)`), which russh
//!     surfaces to the client as `ChannelOpenFailure` — the "nothing listening on
//!     port N" wrong-port signal the eager probe classifies (AE4).
//!
//! What is proven here:
//!   - The forward binds to 127.0.0.1 (never 0.0.0.0) and reports the OS-assigned
//!     port.
//!   - A request to the local port reaches the remote dashboard and returns the
//!     known response through the tunnel.
//!   - AE4: a wrong dashboard port is classified at connect via the eager probe as
//!     `WrongDashboardPort` ("nothing listening on port N"), distinct from an
//!     SSH-down error.
//!   - A per-accept `direct-tcpip` open failure after a healthy connect is surfaced
//!     (the local connection is closed, not left hanging).
//!   - Tearing down the forward stops the listener (the port no longer accepts) and
//!     aborts in-flight forwards.

use super::*;
use crate::keychain::MemoryKeyStore;
use crate::ssh::connection::{connect, host_part, ConnectConfig, HostKeyPrompt, TrustResponse};
use crate::ssh::known_hosts::{KnownHosts, MemoryKnownHostsStore};
use crate::ssh::pipeline::{ConnectErrorKind, ConnectStage};
use crate::ssh::signer::{Ed25519Signer, Signer};

use russh::keys::ssh_key::PublicKey; // russh's fork, used in the server Handler sig
use russh::server::{Auth, Msg as ServerMsg, Session};
use russh::Channel as RusshChannel;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

// ── A tiny local test HTTP server the bot "dashboard" stands in for ─────────

/// The fixed response the in-process dashboard returns to any request — the
/// known body a client hitting the loopback forward must receive through the
/// tunnel.
const DASHBOARD_BODY: &str = "BOTBOX-DASHBOARD-OK";

/// Start a minimal HTTP server on `127.0.0.1:0` that replies to any inbound bytes
/// with a fixed 200 + [`DASHBOARD_BODY`], then closes. Returns its `host:port`.
/// This is the remote dashboard the SSH server proxies `direct-tcpip` channels to.
async fn start_dashboard_http() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind dashboard");
    let addr = listener.local_addr().expect("dashboard addr").to_string();
    let task = tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            tokio::spawn(async move {
                // Read (and discard) the request bytes until we have a request line.
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    DASHBOARD_BODY.len(),
                    DASHBOARD_BODY
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });
    (addr, task)
}

// ── In-process russh server honoring direct-tcpip ───────────────────────────

/// A server handler that accepts publickey auth and, on a `direct-tcpip` open,
/// proxies to the configured dashboard `host:port` when the requested port matches
/// `dashboard_port` — otherwise refuses the open (the wrong-port signal).
#[derive(Clone)]
struct ForwardServerHandler {
    /// The remote dashboard the server proxies accepted channels to.
    dashboard_addr: String,
    /// The port the server treats as "the dashboard"; opens to other ports are
    /// refused. `AtomicU16` so a test can point the client at a *wrong* port while
    /// the server's real dashboard is on the right one.
    dashboard_port: Arc<AtomicU16>,
}

impl russh::server::Handler for ForwardServerHandler {
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
    fn channel_open_direct_tcpip(
        &mut self,
        channel: RusshChannel<ServerMsg>,
        _host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        _session: &mut Session,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        let dashboard_port = self.dashboard_port.load(Ordering::SeqCst);
        let dashboard_addr = self.dashboard_addr.clone();
        async move {
            // Wrong port → refuse the channel open. russh sends CHANNEL_OPEN_FAILURE,
            // which the client classifies as WrongDashboardPort (nothing listening).
            if port_to_connect as u16 != dashboard_port {
                return Ok(false);
            }

            // Right port → accept and proxy the channel to the local dashboard HTTP
            // server: copy bytes both ways between the SSH channel and a fresh TCP
            // connection to the dashboard.
            match TcpStream::connect(&dashboard_addr).await {
                Ok(mut upstream) => {
                    tokio::spawn(async move {
                        let mut chan_stream = channel.into_stream();
                        let _ = tokio::io::copy_bidirectional(&mut chan_stream, &mut upstream).await;
                    });
                    Ok(true)
                }
                // Dashboard unreachable from the server side → refuse.
                Err(_) => Ok(false),
            }
        }
    }
}

/// A running forward-capable server: its loopback address, host key, and the
/// dashboard port it honors.
struct ForwardTestServer {
    addr: String,
    host_openssh: String,
    dashboard_port: Arc<AtomicU16>,
    _accept_task: tokio::task::JoinHandle<()>,
    _dashboard_task: tokio::task::JoinHandle<()>,
}

impl ForwardTestServer {
    /// Start a server whose dashboard is a real local HTTP server; the server
    /// honors `direct-tcpip` to that dashboard's port and refuses others.
    async fn start() -> Self {
        let (dashboard_addr, dashboard_task) = start_dashboard_http().await;
        let dashboard_port: u16 = dashboard_addr
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse().ok())
            .expect("dashboard port");
        let dashboard_port = Arc::new(AtomicU16::new(dashboard_port));

        let host_key =
            russh::keys::PrivateKey::random(&mut rand_core::OsRng, russh::keys::Algorithm::Ed25519)
                .expect("host key");
        let host_openssh = host_key.public_key().to_openssh().expect("host openssh");

        let config = Arc::new(russh::server::Config {
            auth_rejection_time: Duration::from_millis(1),
            auth_rejection_time_initial: Some(Duration::from_millis(1)),
            keys: vec![host_key],
            ..russh::server::Config::default()
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
        let addr = listener.local_addr().expect("addr").to_string();

        let handler = ForwardServerHandler {
            dashboard_addr,
            dashboard_port: dashboard_port.clone(),
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

        ForwardTestServer {
            addr,
            host_openssh,
            dashboard_port,
            _accept_task: accept_task,
            _dashboard_task: dashboard_task,
        }
    }

    /// The dashboard port the server proxies to (the "right" port to probe/forward).
    fn dashboard_port(&self) -> u16 {
        self.dashboard_port.load(Ordering::SeqCst)
    }

    fn store_host_key(&self) -> ::ssh_key::PublicKey {
        ::ssh_key::PublicKey::from_openssh(&self.host_openssh).expect("parse host key")
    }
}

// ── helpers (mirror connection_test.rs / channels_test.rs) ──────────────────

fn test_signer() -> Arc<dyn Signer> {
    let s = Ed25519Signer::new(Box::new(MemoryKeyStore::new()));
    s.generate().expect("generate test key");
    Arc::new(s)
}

fn known_hosts_trusting(server: &ForwardTestServer) -> Arc<KnownHosts<MemoryKnownHostsStore>> {
    let kh = KnownHosts::new(MemoryKnownHostsStore::new());
    kh.trust(&host_part(&server.addr), &server.store_host_key())
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

async fn connect_to(server: &ForwardTestServer) -> crate::ssh::connection::Connection {
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

/// Make an HTTP/1.1 GET to `127.0.0.1:<port>` and return the full response bytes.
async fn http_get(port: u16) -> Vec<u8> {
    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.expect("connect local");
    sock.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("send request");
    sock.flush().await.expect("flush");
    let mut out = Vec::new();
    let _ = sock.read_to_end(&mut out).await;
    out
}

// ── Test: binds 127.0.0.1, reports OS-assigned port ─────────────────────────

#[tokio::test]
async fn forward_binds_loopback_and_reports_os_port() {
    let server = ForwardTestServer::start().await;
    let conn = connect_to(&server).await;

    let forward = bind_and_forward(conn.handle(), "127.0.0.1", server.dashboard_port())
        .await
        .expect("forward binds");

    // The OS assigned a real, non-zero port.
    let port = forward.local_port();
    assert_ne!(port, 0, "OS-assigned port must be reported");
    assert_eq!(forward.local_url(), format!("http://127.0.0.1:{port}"));

    // The listener is reachable on loopback specifically: connecting to it on
    // 127.0.0.1 succeeds (proves it bound loopback, not nowhere).
    let probe = TcpStream::connect(("127.0.0.1", port)).await;
    assert!(probe.is_ok(), "loopback forward port must accept connections");

    forward.close();
    conn.close().await;
}

// ── Test: a request to the local port reaches the remote dashboard ──────────

#[tokio::test]
async fn request_to_local_port_reaches_remote_dashboard() {
    let server = ForwardTestServer::start().await;
    let conn = connect_to(&server).await;

    let forward = bind_and_forward(conn.handle(), "127.0.0.1", server.dashboard_port())
        .await
        .expect("forward binds");
    let port = forward.local_port();

    // Hit the loopback port; the bytes are tunneled over direct-tcpip to the remote
    // dashboard HTTP server, whose known response comes back through the tunnel.
    let response = http_get(port).await;
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.contains(DASHBOARD_BODY),
        "tunneled response must carry the dashboard body, got: {text}"
    );
    assert!(text.starts_with("HTTP/1.1 200"), "tunneled a real HTTP response");

    forward.close();
    conn.close().await;
}

// ── Test (AE4): wrong dashboard port → WrongDashboardPort at probe ──────────

#[tokio::test]
async fn wrong_dashboard_port_is_classified_at_probe() {
    let server = ForwardTestServer::start().await;
    let conn = connect_to(&server).await;

    // Probe a port the server does NOT proxy (its dashboard is elsewhere). The
    // server refuses the channel open → russh ChannelOpenFailure → wrong-port.
    let wrong_port = server.dashboard_port().wrapping_add(1).max(1);
    let err = probe_dashboard_port(&conn.handle(), "127.0.0.1", wrong_port)
        .await
        .expect_err("a wrong dashboard port must be classified as wrong-port");

    assert_eq!(err.kind, ConnectErrorKind::WrongDashboardPort);
    assert_eq!(err.stage, ConnectStage::ProbeDashboard);
    // Distinct from an SSH-down/connection-lost error.
    assert_ne!(err.kind, ConnectErrorKind::ConnectionLost);
    assert_ne!(err.kind, ConnectErrorKind::Unreachable);
    assert!(
        err.message.contains(&wrong_port.to_string()),
        "message names the port: {}",
        err.message
    );

    // And the connection is still alive (the probe did not tear it down).
    assert!(!conn.is_closed(), "probe leaves the connection up");

    conn.close().await;
}

// ── Test: the eager probe passes for the RIGHT port ─────────────────────────

#[tokio::test]
async fn probe_succeeds_for_listening_dashboard_port() {
    let server = ForwardTestServer::start().await;
    let conn = connect_to(&server).await;

    probe_dashboard_port(&conn.handle(), "127.0.0.1", server.dashboard_port())
        .await
        .expect("probe of a listening dashboard port succeeds");

    conn.close().await;
}

// ── Test: per-accept open failure after a healthy connect is surfaced ───────

#[tokio::test]
async fn per_accept_open_failure_is_surfaced_not_hung() {
    let server = ForwardTestServer::start().await;
    let conn = connect_to(&server).await;

    // Bind a forward pointed at a WRONG remote port. The probe would have caught
    // this, but here we deliberately bind+forward to a refused port to exercise the
    // per-accept open-failure path: each accepted local connection tries to open a
    // direct-tcpip channel, the server refuses it, and the per-accept task logs the
    // failure and closes the local stream (no silent browser hang).
    let wrong_port = server.dashboard_port().wrapping_add(1).max(1);
    let forward = bind_and_forward(conn.handle(), "127.0.0.1", wrong_port)
        .await
        .expect("forward binds even toward a wrong port");
    let port = forward.local_port();

    // Connect to the loopback port and read to EOF: the per-accept open fails, so
    // the local stream is dropped/closed promptly rather than hanging open forever.
    let mut sock = tokio::time::timeout(
        Duration::from_secs(3),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .expect("local accept does not hang")
    .expect("local connect");

    // The read returns (EOF) within a bounded time — the browser would see a reset,
    // not an indefinite hang.
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(3), sock.read_to_end(&mut buf)).await;
    assert!(read.is_ok(), "a failed forward closes the local stream (no hang)");

    forward.close();
    conn.close().await;
}

// ── Test: teardown stops the listener + aborts in-flight forwards ───────────

#[tokio::test]
async fn teardown_stops_listener_and_frees_port() {
    let server = ForwardTestServer::start().await;
    let conn = connect_to(&server).await;

    let forward = bind_and_forward(conn.handle(), "127.0.0.1", server.dashboard_port())
        .await
        .expect("forward binds");
    let port = forward.local_port();

    // Before teardown: the port accepts.
    assert!(
        TcpStream::connect(("127.0.0.1", port)).await.is_ok(),
        "port accepts before teardown"
    );

    // Tear down (the connection-child lifecycle): the accept loop aborts, the
    // listener is dropped, and the port is freed.
    forward.close();

    // After teardown the port no longer accepts. Give the aborted task a moment to
    // drop the listener, then assert connects are refused.
    let freed = wait_until_refused(port, Duration::from_secs(3)).await;
    assert!(freed, "loopback port must stop accepting after teardown");

    conn.close().await;
}

/// Poll a loopback `port` until a fresh connect is refused (the listener is gone)
/// or the bound elapses. Returns whether it became refused.
async fn wait_until_refused(port: u16, within: Duration) -> bool {
    let deadline = std::time::Instant::now() + within;
    loop {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(_) => {
                if std::time::Instant::now() > deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(_) => return true,
        }
    }
}
