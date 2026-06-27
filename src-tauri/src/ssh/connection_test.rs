//! Connection-actor + staged-pipeline integration tests (U4).
//!
//! These exercise the connect pipeline end-to-end against an **in-process russh
//! server** (the hermetic "containerized bot" — Docker is unavailable). The
//! server binds `127.0.0.1:0`, the client drives the real `russh` handshake +
//! publickey auth against it, and we assert the stage-tagged error classes,
//! TOFU behaviour, the bounded trust-prompt timeout, validate-before-swap, and
//! mid-session loss detection.
//!
//! All tests are hermetic and run under normal `cargo test` (no `#[ignore]`, no
//! real network). The unreachable-host test uses a bound-then-closed port so it
//! fails fast without waiting on a timeout.

use super::*;
use crate::keychain::MemoryKeyStore;
use crate::ssh::known_hosts::{KnownHosts, MemoryKnownHostsStore};
use crate::ssh::signer::{Ed25519Signer, Signer, SignerError};
// The store's direct `ssh-key` (U2's 0.6.7) is referenced as `::ssh_key`, distinct
// from russh's vendored fork (`russh::keys::ssh_key`). Server-side keys are made
// with russh's fork; we bridge to the store via OpenSSH strings.
use russh::keys::ssh_key::PublicKey; // russh's fork, used in the server Handler sig
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

// ── In-process russh server fixture ─────────────────────────────────────────

/// A server handler that accepts or rejects publickey auth based on a flag, and
/// accepts session channels so the happy path completes. This is the hermetic
/// stand-in for a real bot.
#[derive(Clone)]
struct TestServerHandler {
    accept_auth: Arc<AtomicBool>,
}

impl russh::server::Handler for TestServerHandler {
    type Error = russh::Error;

    fn auth_publickey(
        &mut self,
        _user: &str,
        _key: &PublicKey,
    ) -> impl std::future::Future<Output = Result<russh::server::Auth, Self::Error>> + Send {
        let accept = self.accept_auth.load(Ordering::SeqCst);
        async move {
            if accept {
                Ok(russh::server::Auth::Accept)
            } else {
                Ok(russh::server::Auth::reject())
            }
        }
    }

    // Matches russh's trait signature (`-> impl Future`); the body is a bare async
    // block, which clippy would rather see as `async fn` — but the trait dictates
    // the shape, so allow it here.
    #[allow(clippy::manual_async_fn)]
    fn channel_open_session(
        &mut self,
        _channel: russh::Channel<russh::server::Msg>,
        _session: &mut russh::server::Session,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        async { Ok(true) }
    }
}

/// A running in-process server: its loopback `host:port` address, the host key it
/// presents, and an auth toggle. Dropping it stops accepting (the accept loop is
/// a detached task that ends when the listener is dropped).
struct TestServer {
    addr: String,
    /// The host key as an OpenSSH string — the type-agnostic interop surface
    /// (russh's `PublicKey` is a forked `ssh-key` type, distinct from the store's).
    host_openssh: String,
    #[allow(dead_code)]
    accept_auth: Arc<AtomicBool>,
    _accept_task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    /// Spin up a server on `127.0.0.1:0` that accepts auth by default.
    async fn start() -> Self {
        Self::start_with_auth(true).await
    }

    async fn start_with_auth(accept_auth: bool) -> Self {
        // Server host key (ed25519) — what the client TOFU-verifies.
        let host_key = russh::keys::PrivateKey::random(
            &mut rand_core::OsRng,
            russh::keys::Algorithm::Ed25519,
        )
        .expect("host key");
        let host_openssh = host_key.public_key().to_openssh().expect("host key openssh");

        let config = Arc::new(russh::server::Config {
            // Fast rejections so the auth-reject test does not wait a real second.
            auth_rejection_time: std::time::Duration::from_millis(1),
            auth_rejection_time_initial: Some(std::time::Duration::from_millis(1)),
            keys: vec![host_key],
            ..russh::server::Config::default()
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr").to_string();

        let accept_flag = Arc::new(AtomicBool::new(accept_auth));
        let handler = TestServerHandler {
            accept_auth: accept_flag.clone(),
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

        TestServer {
            addr,
            host_openssh,
            accept_auth: accept_flag,
            _accept_task: accept_task,
        }
    }

    /// The host key parsed into the *store's* `ssh-key` type (for direct
    /// `KnownHosts` calls in tests).
    fn store_host_key(&self) -> ::ssh_key::PublicKey {
        ::ssh_key::PublicKey::from_openssh(&self.host_openssh).expect("parse host key")
    }
}

/// Build a real ed25519 signer over an in-memory store with a freshly generated
/// key (no Keychain).
fn test_signer() -> Arc<dyn Signer> {
    let s = Ed25519Signer::new(Box::new(MemoryKeyStore::new()));
    s.generate().expect("generate test key");
    Arc::new(s)
}

/// A signer that always fails to sign / read its public key — models a locked
/// Keychain or a cancelled OS prompt (the local-signer-failure class).
struct FailingSigner;

impl Signer for FailingSigner {
    fn algorithm(&self) -> &'static str {
        "ssh-ed25519"
    }
    fn public_openssh(&self) -> Result<String, SignerError> {
        Err(SignerError::NoKey)
    }
    fn sign(&self, _message: &[u8]) -> Result<Vec<u8>, SignerError> {
        Err(SignerError::NoKey)
    }
}

/// Drain trust prompts in the background, answering every prompt with `answer`.
/// Returns the join handle (kept alive for the duration of the connect).
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

/// Pre-trust `server`'s host key in a fresh known-hosts store so connects skip the
/// prompt.
fn known_hosts_trusting(server: &TestServer) -> Arc<KnownHosts<MemoryKnownHostsStore>> {
    let kh = KnownHosts::new(MemoryKnownHostsStore::new());
    kh.trust(&host_part(&server.addr), &server.store_host_key())
        .expect("pre-trust");
    Arc::new(kh)
}

fn empty_known_hosts() -> Arc<KnownHosts<MemoryKnownHostsStore>> {
    Arc::new(KnownHosts::new(MemoryKnownHostsStore::new()))
}

// ── Test 1: unreachable host → unreachable class ────────────────────────────

#[tokio::test]
async fn unreachable_host_is_classified_unreachable() {
    // Bind then immediately drop the listener so the port is closed → connect
    // refused fast (no timeout wait).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);

    let (tx, rx) = mpsc::channel(4);
    let _answer = auto_answer(rx, TrustResponse::Trust);

    let err = connect(
        &addr,
        &ConnectConfig::for_user("tester").with_short_timeouts(),
        test_signer(),
        empty_known_hosts(),
        tx,
    )
    .await
    .expect_err("connect to a closed port must fail");

    assert_eq!(err.kind, ConnectErrorKind::Unreachable);
    assert_eq!(err.stage, ConnectStage::TcpConnect);
}

#[tokio::test]
async fn unroutable_ip_times_out_as_unreachable() {
    // TEST-NET-1 (192.0.2.0/24) is reserved-unroutable → connect blocks until the
    // short timeout, classified unreachable (not a refused-connection error).
    let (tx, rx) = mpsc::channel(4);
    let _answer = auto_answer(rx, TrustResponse::Trust);

    let err = connect(
        "192.0.2.1:22",
        &ConnectConfig::for_user("tester").with_short_timeouts(),
        test_signer(),
        empty_known_hosts(),
        tx,
    )
    .await
    .expect_err("unroutable IP must fail");

    assert_eq!(err.kind, ConnectErrorKind::Unreachable);
    assert_eq!(err.stage, ConnectStage::TcpConnect);
}

// ── Test 2 (AE3): reachable host rejects the key → remote-auth-failure ───────

#[tokio::test]
async fn reachable_host_rejecting_key_is_remote_auth_failure() {
    let server = TestServer::start_with_auth(false).await;
    let kh = known_hosts_trusting(&server);

    let (tx, rx) = mpsc::channel(4);
    let _answer = auto_answer(rx, TrustResponse::Trust);

    let err = connect(
        &server.addr,
        &ConnectConfig::for_user("tester"),
        test_signer(),
        kh,
        tx,
    )
    .await
    .expect_err("server rejects the key");

    // Distinct from unreachable: it reached auth and got rejected.
    assert_eq!(err.kind, ConnectErrorKind::RemoteAuthFailure);
    assert_eq!(err.stage, ConnectStage::Authenticate);
    assert_ne!(err.kind, ConnectErrorKind::Unreachable);
}

// ── Test 3: local signer failure → signer-failure class (NOT provisioning) ──

#[tokio::test]
async fn local_signer_failure_is_distinct_from_remote_auth() {
    let server = TestServer::start().await; // accepts auth, but we never get there
    let kh = known_hosts_trusting(&server);

    let (tx, rx) = mpsc::channel(4);
    let _answer = auto_answer(rx, TrustResponse::Trust);

    let err = connect(
        &server.addr,
        &ConnectConfig::for_user("tester"),
        Arc::new(FailingSigner),
        kh,
        tx,
    )
    .await
    .expect_err("signer cannot sign");

    assert_eq!(err.kind, ConnectErrorKind::LocalSignerFailure);
    assert_eq!(err.stage, ConnectStage::Authenticate);
    // The whole point: NOT routed to the provision-your-key (remote-auth) surface.
    assert_ne!(err.kind, ConnectErrorKind::RemoteAuthFailure);
}

// ── Test 4: first contact → trust prompt surfaced; on accept, key persisted ──

#[tokio::test]
async fn first_contact_prompts_and_persists_on_accept() {
    let server = TestServer::start().await;
    let kh = empty_known_hosts(); // unknown host → prompt

    let (tx, mut rx) = mpsc::channel::<HostKeyPrompt>(4);

    // Capture that a prompt was surfaced (with a SHA-256 fingerprint), answer Trust.
    let prompted = Arc::new(AtomicBool::new(false));
    let prompted2 = prompted.clone();
    let kh_for_assert = kh.clone();
    let host = host_part(&server.addr);
    let answer = tokio::spawn(async move {
        if let Some(prompt) = rx.recv().await {
            assert!(prompt.fingerprint.starts_with("SHA256:"));
            assert_eq!(prompt.host, host);
            prompted2.store(true, Ordering::SeqCst);
            let _ = prompt.responder.send(TrustResponse::Trust);
        }
    });

    let conn = connect(
        &server.addr,
        &ConnectConfig::for_user("tester"),
        test_signer(),
        kh,
        tx,
    )
    .await
    .expect("connect succeeds after trusting the host");

    answer.await.unwrap();
    assert!(prompted.load(Ordering::SeqCst), "a trust prompt was surfaced");

    // The host key was persisted: a subsequent decide reads Known.
    let saved = kh_for_assert
        .saved(&host_part(&server.addr))
        .unwrap();
    assert!(saved.is_some(), "host key persisted on accept");

    conn.close().await;
}

// ── Test 5: unresolved prompt times out → socket closed, no leak ────────────

#[tokio::test]
async fn unresolved_trust_prompt_times_out_and_aborts_cleanly() {
    let server = TestServer::start().await;
    let kh = empty_known_hosts();

    // Drain prompts but NEVER answer → the bounded 100ms timeout must fire.
    let (tx, mut rx) = mpsc::channel(4);
    let _never_answer = tokio::spawn(async move {
        // Hold the prompt (and its responder) without resolving it.
        let _held = rx.recv().await;
        // Park so the responder is not dropped early (which would also reject).
        std::future::pending::<()>().await;
    });

    let start = std::time::Instant::now();
    let err = connect(
        &server.addr,
        &ConnectConfig::for_user("tester").with_short_timeouts(),
        test_signer(),
        kh,
        tx,
    )
    .await
    .expect_err("an unanswered prompt must auto-reject");

    let elapsed = start.elapsed();
    assert_eq!(err.kind, ConnectErrorKind::UntrustedHostKey);
    assert_eq!(err.stage, ConnectStage::HostKeyCheck);
    // It returned promptly (well under a real 60s; bounded by the ~100ms timeout
    // plus handshake), proving the parked handshake aborted rather than hanging.
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "timed out cleanly in {elapsed:?}"
    );
}

// ── Test 6: known host, changed key → recoverable mismatch hard-stop ────────

#[tokio::test]
async fn changed_host_key_is_recoverable_mismatch() {
    let server = TestServer::start().await;
    let host = host_part(&server.addr);

    // Pre-trust a DIFFERENT (imposter) key for this host → presented key mismatches.
    let imposter = russh::keys::PrivateKey::random(
        &mut rand_core::OsRng,
        russh::keys::Algorithm::Ed25519,
    )
    .unwrap();
    let imposter_openssh = imposter.public_key().to_openssh().unwrap();
    let imposter_store_key = ::ssh_key::PublicKey::from_openssh(&imposter_openssh).unwrap();
    let kh = KnownHosts::new(MemoryKnownHostsStore::new());
    kh.trust(&host, &imposter_store_key).unwrap();
    let kh = Arc::new(kh);

    let (tx, rx) = mpsc::channel(4);
    let _answer = auto_answer(rx, TrustResponse::Trust); // won't matter; mismatch hard-stops

    // Connect fails with a mismatch and does NOT silently update.
    let err = connect(
        &server.addr,
        &ConnectConfig::for_user("tester"),
        test_signer(),
        kh.clone(),
        tx,
    )
    .await
    .expect_err("mismatch hard-stops");
    assert_eq!(err.kind, ConnectErrorKind::HostKeyMismatch);
    assert_eq!(err.stage, ConnectStage::HostKeyCheck);

    // Saved key is still the imposter (never silently bumped).
    assert_eq!(
        kh.saved(&host).unwrap().unwrap(),
        imposter_store_key.to_openssh().unwrap()
    );

    // Recovery: explicit remove, then a fresh connect trusts the real key and
    // succeeds.
    kh.remove(&host).unwrap();
    let (tx2, rx2) = mpsc::channel(4);
    let _answer2 = auto_answer(rx2, TrustResponse::Trust);
    let conn = connect(
        &server.addr,
        &ConnectConfig::for_user("tester"),
        test_signer(),
        kh.clone(),
        tx2,
    )
    .await
    .expect("after remove + re-trust, connect succeeds");
    assert_eq!(
        kh.decide(&host, &server.store_host_key()).unwrap(),
        crate::ssh::known_hosts::HostKeyDecision::Known
    );
    conn.close().await;
}

// ── Test 7: validate-before-swap (A intact when B fails; swap on B success) ──

#[tokio::test]
async fn validate_before_swap_preserves_prior_on_failure_and_swaps_on_success() {
    let mgr = ConnectionManager::new();

    // Server A: accepts. Connect and install as active.
    let server_a = TestServer::start().await;
    let kh_a = known_hosts_trusting(&server_a);
    let (tx_a, rx_a) = mpsc::channel(4);
    let _ans_a = auto_answer(rx_a, TrustResponse::Trust);
    let conn_a = connect(
        &server_a.addr,
        &ConnectConfig::for_user("tester"),
        test_signer(),
        kh_a,
        tx_a,
    )
    .await
    .expect("A connects");
    mgr.install(conn_a).await;
    assert!(mgr.is_active().await, "A is active");

    // Server B: REJECTS auth. The staged connect to B fails BEFORE any swap, so we
    // never call install → A stays active.
    let server_b = TestServer::start_with_auth(false).await;
    let kh_b = known_hosts_trusting(&server_b);
    let (tx_b, rx_b) = mpsc::channel(4);
    let _ans_b = auto_answer(rx_b, TrustResponse::Trust);
    let b_result = connect(
        &server_b.addr,
        &ConnectConfig::for_user("tester"),
        test_signer(),
        kh_b,
        tx_b,
    )
    .await;
    assert!(b_result.is_err(), "B fails auth");
    assert!(mgr.is_active().await, "A still active after B's failed switch");

    // Server C: accepts. A successful connect, then install tears A down and makes
    // C active (swap only after C authenticated).
    let server_c = TestServer::start().await;
    let kh_c = known_hosts_trusting(&server_c);
    let (tx_c, rx_c) = mpsc::channel(4);
    let _ans_c = auto_answer(rx_c, TrustResponse::Trust);
    let conn_c = connect(
        &server_c.addr,
        &ConnectConfig::for_user("tester"),
        test_signer(),
        kh_c,
        tx_c,
    )
    .await
    .expect("C connects");
    mgr.install(conn_c).await;
    assert!(mgr.is_active().await, "C is active after swap");

    mgr.disconnect().await;
    assert!(!mgr.is_active().await, "clean disconnect clears active");
}

// ── Test 8: transport dies mid-session → connection-lost (no frozen actor) ──

#[tokio::test]
async fn mid_session_transport_death_emits_connection_lost() {
    let server = TestServer::start().await;
    let kh = known_hosts_trusting(&server);
    let (tx, rx) = mpsc::channel(4);
    let _ans = auto_answer(rx, TrustResponse::Trust);

    let conn = connect(
        &server.addr,
        &ConnectConfig::for_user("tester"),
        test_signer(),
        kh,
        tx,
    )
    .await
    .expect("connects");

    // Kill the server side: dropping the accept task's listener won't close the
    // established session, so instead we drop the whole server (its accept loop)
    // and force the session shut by disconnecting from our side to simulate the
    // transport going away from under the driver. To exercise *loss detection*
    // specifically, abort the server so the peer half-closes.
    drop(server); // stops accepting; existing session peer task may linger

    // Disconnect at the transport level to make the session task end, which flips
    // is_closed() and must make the driver emit Lost (not hang).
    conn.handle()
        .disconnect(russh::Disconnect::ByApplication, "", "")
        .await
        .ok();

    // The driver must surface Lost within a bounded wait — proving the actor is
    // not frozen.
    let event = tokio::time::timeout(std::time::Duration::from_secs(3), conn.next_event())
        .await
        .expect("driver emits an event (not frozen)");
    assert_eq!(event, Some(ConnectionEvent::Lost));
}

// ── Test 9: a clean close() does NOT surface as connection-lost ─────────────
//
// `close()` calls `handle.disconnect()`, which flips `is_closed()` true. The
// driver polls `is_closed()`, so without the intentional-close flag a clean
// operator Disconnect / bot-swap could emit a phantom `Lost` BEFORE the driver is
// aborted — surfacing `connection-lost` to the UI after the user deliberately
// disconnected. This proves the flag suppresses that.
#[tokio::test]
async fn clean_close_does_not_emit_connection_lost() {
    let server = TestServer::start().await;
    let kh = known_hosts_trusting(&server);
    let (tx, rx) = mpsc::channel(4);
    let _ans = auto_answer(rx, TrustResponse::Trust);

    let conn = connect(
        &server.addr,
        &ConnectConfig::for_user("tester"),
        test_signer(),
        kh,
        tx,
    )
    .await
    .expect("connects");

    // Take the loss-event stream the way the command layer does, so we can observe
    // whether the driver emits anything across an intentional teardown.
    let mut events = conn.take_events().await.expect("events stream available");

    // Intentional teardown: this must NOT produce a Lost event.
    conn.close().await;

    // Give the driver ample time to (wrongly) emit if the flag did not suppress it.
    // The receiver should instead close cleanly (channel dropped) with no Lost.
    match tokio::time::timeout(std::time::Duration::from_millis(500), events.recv()).await {
        Ok(Some(ConnectionEvent::Lost)) => {
            panic!("clean close() must not emit connection-lost (phantom loss)")
        }
        // `None` == the driver ended without sending (correct) — the sender dropped.
        Ok(None) => {}
        // Timed out with nothing sent — also correct (no phantom Lost).
        Err(_elapsed) => {}
    }
}
