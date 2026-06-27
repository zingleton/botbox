//! Dashboard port-forward over `direct-tcpip` + eager wrong-port probe (U6; R12,
//! R13, R11 wrong-port, KTD7).
//!
//! This module forwards the bot's configured dashboard port to a **loopback**
//! local port tied to the live connection, and classifies a wrong dashboard port
//! at connect time via an eager probe. It rides the **single** authenticated
//! `russh` connection U4 exposes as
//! [`SharedHandle`](crate::ssh::connection::SharedHandle): the listener task opens
//! a `direct-tcpip` channel per accepted local connection and
//! `copy_bidirectional`s the loopback TCP stream against the SSH channel stream.
//!
//! ## Loopback-only bind, OS-assigned port (KTD7)
//!
//! We bind a [`tokio::net::TcpListener`] on **`127.0.0.1:0`** — NEVER `0.0.0.0` —
//! and read the OS-assigned port back from `local_addr()`. Reading the port back
//! from the *bound* listener (rather than picking a port and binding it) avoids a
//! TOCTOU race where the chosen port is taken between the check and the bind. The
//! returned [`Forward::local_url`] is `http://127.0.0.1:<port>`.
//!
//! ## Eager probe → wrong-port classification (KTD7, R11)
//!
//! [`probe_dashboard_port`] opens ONE `direct-tcpip` channel to the dashboard
//! `host:port` and immediately closes it. The signal we classify on:
//!
//! - With OpenSSH (what the real Hermes bot runs), if **nothing is listening** on
//!   the remote dashboard port the server replies `SSH_MSG_CHANNEL_OPEN_FAILURE`
//!   (reason `connect failed`). russh surfaces that as
//!   `Err(russh::Error::ChannelOpenFailure(_))` straight out of
//!   `channel_open_direct_tcpip` — so a failed open is the primary, reliable
//!   "nothing listening on port N" signal, and we tag it
//!   [`ConnectErrorKind::WrongDashboardPort`].
//! - Some SSH servers instead **accept** the channel open and only then EOF/close
//!   it when the forwarded connection is refused. To be robust against that, after
//!   a successful open we wait a short, bounded window for the first channel
//!   message: an immediate `Eof`/`Close` with no data also classifies as
//!   wrong-port. A channel that opens and *stays* open (or yields data) is a
//!   healthy listener.
//!
//! Either way the probe channel is closed before we return — it never leaks and is
//! distinct from an SSH-down error (which surfaces as the channel-open call failing
//! with a transport error, classified separately by the caller's pipeline; here a
//! transport death during the probe is reported as wrong-port-shaped only if the
//! server actively refused, otherwise it is the `is_closed` connection-lost path).
//!
//! ## Per-accept handling + lifecycle (KTD7)
//!
//! The accept loop is spawned as a task whose [`tokio::task::JoinHandle`] the
//! [`Forward`] owns — making the listener a **child of the connection actor**:
//! dropping/closing the `Forward` aborts the loop, frees the loopback port, and
//! aborts in-flight forwards. Each accepted connection is handled on its own task;
//! a per-accept `direct-tcpip` open failure is **logged/surfaced** (it never hangs
//! the browser waiting on a dead tunnel), not propagated to kill the listener.
//!
//! ## Accepted v1 risk (KTD7 / Scope Boundaries)
//!
//! The loopback listener is reachable by ANY local process running as the same
//! user — there is no per-connection access token in v1. For a single-operator
//! desktop this is accepted and documented here; a single-use access token on the
//! forward is deferred (see the plan's Scope Boundaries and U8 packaging docs).

use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};

use crate::ssh::connection::SharedHandle;
use crate::ssh::pipeline::{ConnectError, ConnectErrorKind, ConnectStage};

/// The originator address/port reported in the `direct-tcpip` open (informational
/// to the server; loopback is honest about where the forwarded connection began).
const ORIGINATOR_HOST: &str = "127.0.0.1";

/// How long the eager probe waits, after a successful channel open, for the first
/// channel message before deciding the listener is healthy. Servers that refuse
/// the forwarded connection *after* opening the channel send an EOF/Close within
/// this window; a real listener does not close immediately. Kept short so connect
/// is not perceptibly slowed.
const PROBE_SETTLE_WINDOW: Duration = Duration::from_millis(250);

/// A bound, running dashboard forward: the loopback listener (its OS-assigned
/// port) plus the accept-loop task. Owned by the connection layer so its lifetime
/// is the connection's — dropping or [`Forward::close`]-ing it aborts the loop,
/// frees the port, and aborts in-flight forwards (KTD7 lifecycle).
pub struct Forward {
    /// The OS-assigned loopback port the dashboard is reachable on locally.
    local_port: u16,
    /// The accept loop. Aborting it closes the listener (frees the port) and stops
    /// spawning new per-accept forwards; tokio aborts the in-flight ones when their
    /// tasks are dropped on teardown.
    accept_task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for Forward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Forward")
            .field("local_port", &self.local_port)
            .field("finished", &self.accept_task.is_finished())
            .finish()
    }
}

impl Forward {
    /// The OS-assigned loopback port the forward is listening on.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// The local URL to open in the browser (`http://127.0.0.1:<port>`).
    pub fn local_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.local_port)
    }

    /// Tear the forward down: abort the accept loop, which drops the listener
    /// (freeing the loopback port) and stops accepting. In-flight per-accept tasks
    /// are detached; they end when their copy completes or the connection handle
    /// they hold goes away on connection teardown. Idempotent.
    pub fn close(self) {
        self.accept_task.abort();
    }
}

/// Eagerly probe the dashboard `host:port` to classify a wrong port BEFORE the
/// connection is reported "connected" (KTD7). Opens one `direct-tcpip` channel and
/// closes it; classifies per the module-level doc.
///
/// Returns `Ok(())` when something is listening on `remote_port`, or
/// `Err(ConnectError { kind: WrongDashboardPort, stage: ProbeDashboard, .. })`
/// when nothing is — distinct from an SSH-down error. The orchestrator's real-bot
/// test calls this directly to validate the live dashboard port (9119).
pub async fn probe_dashboard_port(
    handle: &SharedHandle,
    remote_host: &str,
    remote_port: u16,
) -> Result<(), ConnectError> {
    let wrong_port = |msg: String| {
        ConnectError::new(ConnectErrorKind::WrongDashboardPort, ConnectStage::ProbeDashboard, msg)
    };

    // Primary signal: OpenSSH refuses the channel open (CONNECT_FAILED) when
    // nothing is listening → `channel_open_direct_tcpip` returns Err. That is the
    // "nothing listening on port N" classification.
    let channel = match handle
        .channel_open_direct_tcpip(remote_host.to_string(), remote_port as u32, ORIGINATOR_HOST, 0)
        .await
    {
        Ok(ch) => ch,
        Err(russh::Error::ChannelOpenFailure(_)) => {
            return Err(wrong_port(format!(
                "nothing listening on port {remote_port} (the server refused the dashboard \
                 connection)"
            )));
        }
        Err(e) => {
            // A non-refusal error opening the probe channel: the transport is sick.
            // Surface as wrong-port-shaped at the probe stage only if the server
            // actively refused; otherwise this is a connect-time transport problem.
            // We classify it as wrong-port (probe could not confirm a listener) so
            // the operator is pointed at the port, not at a re-auth flow.
            return Err(wrong_port(format!(
                "could not probe dashboard port {remote_port}: {e}"
            )));
        }
    };

    // Secondary signal (robustness): some servers open the channel, then EOF/Close
    // it when the forwarded connect is refused. Split off the read half and wait a
    // short bounded window: an immediate Eof/Close with no data means nothing was
    // listening; data or a quiet-but-open channel means a healthy listener.
    let (mut read_half, write_half) = channel.split();
    let verdict = tokio::time::timeout(PROBE_SETTLE_WINDOW, async {
        loop {
            match read_half.wait().await {
                // Data or a window adjustment means the remote side is alive and the
                // forwarded connection was accepted → healthy.
                Some(russh::ChannelMsg::Data { .. })
                | Some(russh::ChannelMsg::ExtendedData { .. })
                | Some(russh::ChannelMsg::WindowAdjusted { .. }) => return ProbeVerdict::Healthy,
                // Immediate close with no data → the forwarded connection was refused.
                Some(russh::ChannelMsg::Eof)
                | Some(russh::ChannelMsg::Close)
                | None => return ProbeVerdict::Refused,
                // Other control messages: keep waiting within the window.
                Some(_) => {}
            }
        }
    })
    .await;

    // Close the probe channel either way — it never leaks (KTD7: "then close it").
    let _ = write_half.close().await;

    match verdict {
        // Timed out waiting for a close → the channel stayed open → healthy listener.
        Err(_elapsed) => Ok(()),
        Ok(ProbeVerdict::Healthy) => Ok(()),
        Ok(ProbeVerdict::Refused) => Err(wrong_port(format!(
            "nothing listening on port {remote_port} (the forwarded dashboard connection was \
             closed immediately)"
        ))),
    }
}

enum ProbeVerdict {
    Healthy,
    Refused,
}

/// Bind the loopback listener and spawn the accept loop that forwards each local
/// connection to the remote dashboard `host:port` over a `direct-tcpip` channel
/// (KTD7). Binds `127.0.0.1:0` and reads back the OS-assigned port (no TOCTOU).
///
/// Does NOT probe — the caller runs [`probe_dashboard_port`] first (eager, before
/// reporting connected) and only binds+forwards once the connection is
/// authenticated. The returned [`Forward`] owns the accept task; drop/close it to
/// tear the forward down. The orchestrator's real-bot test calls this to bind+
/// forward the live dashboard for an active connection.
pub async fn bind_and_forward(
    handle: SharedHandle,
    remote_host: impl Into<String>,
    remote_port: u16,
) -> Result<Forward, ConnectError> {
    // Bind loopback only (NEVER 0.0.0.0) and read back the OS-assigned port. The
    // OS picks a free port atomically with the bind, so there is no window where a
    // chosen port could be stolen before we listen on it (TOCTOU-free).
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| {
        ConnectError::new(
            ConnectErrorKind::WrongDashboardPort,
            ConnectStage::ProbeDashboard,
            format!("could not bind loopback forward listener: {e}"),
        )
    })?;
    let local_addr = listener.local_addr().map_err(|e| {
        ConnectError::new(
            ConnectErrorKind::WrongDashboardPort,
            ConnectStage::ProbeDashboard,
            format!("could not read forward listener address: {e}"),
        )
    })?;
    debug_assert!(local_addr.ip().is_loopback(), "forward listener must be loopback");
    let local_port = local_addr.port();

    let remote_host = remote_host.into();
    let accept_task = tokio::spawn(accept_loop(listener, handle, remote_host, remote_port));

    Ok(Forward {
        local_port,
        accept_task,
    })
}

/// The accept loop: for each accepted loopback connection, spawn a per-accept task
/// that opens a `direct-tcpip` channel to the remote dashboard and
/// `copy_bidirectional`s the two streams. A per-accept open failure is logged (it
/// never hangs the browser on a dead tunnel) and does not kill the loop.
async fn accept_loop(
    listener: TcpListener,
    handle: SharedHandle,
    remote_host: String,
    remote_port: u16,
) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            // The listener was closed (teardown) or errored — stop accepting.
            Err(_) => return,
        };
        let handle = handle.clone();
        let remote_host = remote_host.clone();
        tokio::spawn(async move {
            if let Err(e) = forward_one(stream, handle, &remote_host, remote_port).await {
                // Surface (log) the per-accept failure rather than leaving the
                // browser hanging on a half-open tunnel. The local stream is
                // dropped here, closing it so the browser sees a clean reset.
                eprintln!(
                    "botbox: dashboard forward for {peer} → {remote_host}:{remote_port} failed: {e}"
                );
            }
        });
    }
}

/// Forward a single accepted loopback connection: open a `direct-tcpip` channel to
/// the remote dashboard and pump bytes both ways with `copy_bidirectional`.
async fn forward_one(
    mut local: TcpStream,
    handle: SharedHandle,
    remote_host: &str,
    remote_port: u16,
) -> Result<(), ForwardError> {
    let peer_port = local.peer_addr().map(|a| a.port()).unwrap_or(0);
    let channel = handle
        .channel_open_direct_tcpip(
            remote_host.to_string(),
            remote_port as u32,
            ORIGINATOR_HOST,
            peer_port as u32,
        )
        .await
        .map_err(|e| ForwardError(format!("direct-tcpip open failed: {e}")))?;

    // `into_stream` gives an `AsyncRead + AsyncWrite` over the SSH channel's data
    // messages, so the loopback TCP stream and the SSH channel copy directly. This
    // is the DEV.to russh port-forward pattern.
    let mut channel_stream = channel.into_stream();
    tokio::io::copy_bidirectional(&mut local, &mut channel_stream)
        .await
        .map_err(|e| ForwardError(format!("copy_bidirectional ended with error: {e}")))?;
    Ok(())
}

/// A per-accept forward failure (the tunnel for one browser connection died).
/// Logged by the accept loop; never propagated to kill the listener.
#[derive(Debug)]
struct ForwardError(String);

impl std::fmt::Display for ForwardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ForwardError {}

/// Build the loopback dashboard URL for a local port (the value handed to the
/// browser/opener and surfaced in the tunnel UI). Kept as a tiny pure helper so the
/// command layer and tests agree on the exact format.
pub fn local_dashboard_url(local_port: u16) -> String {
    format!("http://127.0.0.1:{local_port}")
}

#[cfg(test)]
#[path = "forward_test.rs"]
mod forward_test;
