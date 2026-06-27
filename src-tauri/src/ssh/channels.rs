//! Host-shell + Hermes-attach PTY channels (U5; R8, R9, R10, KTD4/KTD6).
//!
//! This module opens the two interactive PTY channels Botbox always shows, off the
//! **single** authenticated `russh` connection (R10) — both share the one
//! `Arc<Handle>` U4 exposes via [`Connection::handle`](crate::ssh::connection::Connection::handle).
//!
//! ## Independent outcomes (KTD6 partial-open)
//!
//! The host shell and the Hermes attach PTY open as **independent** results
//! ([`open_host_pty`] / [`open_attach_pty`]). A host-up/attach-failed outcome keeps
//! the connection up and surfaces an attach-specific
//! [`ConnectErrorKind::AttachFailure`](crate::ssh::pipeline::ConnectErrorKind::AttachFailure);
//! it never aborts the working host shell. The command layer ([`crate::commands`])
//! opens the host PTY first, then the attach PTY, and reports the attach failure
//! without tearing the connection down.
//!
//! ## Raw bytes, no Rust-side UTF-8 decode (KTD4)
//!
//! A **read task per channel** forwards `russh` `ChannelMsg::Data` /
//! `ChannelMsg::ExtendedData` straight to a per-terminal sink as **raw bytes**.
//! There is deliberately NO `String::from_utf8` anywhere in this path: a multibyte
//! UTF-8 sequence split across two `russh` reads is forwarded byte-for-byte and
//! reassembled by xterm.js on the frontend. The sink is the
//! [`PtySink`] trait so the read loop is testable without a Tauri IPC runtime; the
//! production sink is a `tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>` that
//! ships `InvokeResponseBody::Raw(bytes)`.
//!
//! ## Coalescing
//!
//! To avoid per-read (often per-byte, for interactive typing echo) IPC overhead,
//! the read task **coalesces** small reads into one sink send: it accumulates into
//! a buffer and flushes when either the buffer reaches [`COALESCE_FLUSH_BYTES`] or
//! [`COALESCE_FLUSH_WINDOW`] elapses since the first un-flushed byte. Coalescing
//! only ever *concatenates* whole `russh` payloads in arrival order — it never
//! splits a payload, so a multibyte sequence that arrives whole in one payload
//! stays whole, and one split across payloads is reassembled in order. The flush
//! window is short enough to stay interactive.
//!
//! ## Input + resize
//!
//! Input flows frontend `onData` → `pty_write` command → [`PtyChannel::write`] →
//! `russh` `channel.data(...)`. Resize flows `FitAddon`/`onResize` → `pty_resize`
//! command → [`PtyChannel::resize`] → `russh` `channel.window_change(...)`. The
//! initial size is passed in `request_pty`; the frontend debounces resizes and
//! delays the first one (the dropped-SIGWINCH pitfall) — see `terminal.ts`.

use std::sync::Arc;
use std::time::Duration;

use russh::client::Msg;
use russh::{Channel, ChannelMsg, ChannelReadHalf, ChannelWriteHalf};

use crate::ssh::connection::SharedHandle;
use crate::ssh::pipeline::{ConnectError, ConnectErrorKind, ConnectStage};

/// Flush the coalescing buffer once it reaches this many bytes. A bulk read (e.g.
/// `cat` of a big file, a screen redraw from `vim`/`htop`) flushes in chunks of
/// this size rather than one giant send.
pub const COALESCE_FLUSH_BYTES: usize = 16 * 1024;

/// Flush the coalescing buffer at most this long after the first un-flushed byte,
/// so interactive output (a keystroke echo) is never held back perceptibly while
/// still batching a burst that arrives within the window.
pub const COALESCE_FLUSH_WINDOW: Duration = Duration::from_millis(8);

/// Terminal type advertised to the remote PTY. `xterm-256color` matches the
/// xterm.js frontend renderer.
pub const PTY_TERM: &str = "xterm-256color";

/// The two terminal panes (R8/R9). Used only for diagnostics / error framing; the
/// routing to the right xterm instance is the per-channel sink, not this tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKind {
    /// Interactive host shell (R8).
    Host,
    /// Hermes attach session running the bot's configured attach command (R9).
    Attach,
}

impl PaneKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PaneKind::Host => "host",
            PaneKind::Attach => "attach",
        }
    }
}

/// A terminal size in character cells. Pixel dimensions are sent as 0 (the SSH PTY
/// convention when only the character grid is known — the remote uses the cells).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub cols: u32,
    pub rows: u32,
}

impl PtySize {
    pub fn new(cols: u32, rows: u32) -> Self {
        Self { cols, rows }
    }
}

impl Default for PtySize {
    /// A sane default until the frontend's first `fit()` resize lands.
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

/// A sink for raw PTY bytes — the seam KTD4's `tauri::ipc::Channel` plugs into.
///
/// Implemented for the production `tauri::ipc::Channel<InvokeResponseBody>` (sends
/// `InvokeResponseBody::Raw`) and for an in-memory test fake. The contract: `send`
/// forwards the bytes **verbatim** in order, with no decoding or re-chunking
/// expected by the receiver beyond what coalescing already did. It is `Send +
/// Sync` so the read task can own it.
pub trait PtySink: Send + Sync + 'static {
    /// Forward one coalesced chunk of raw PTY bytes. Returns `Err` only if the sink
    /// is permanently gone (the read task then stops).
    fn send(&self, bytes: &[u8]) -> Result<(), PtySinkError>;
}

/// A sink-send failure (the receiver/webview channel is gone).
#[derive(Debug)]
pub struct PtySinkError(pub String);

impl std::fmt::Display for PtySinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pty sink send failed: {}", self.0)
    }
}

impl std::error::Error for PtySinkError {}

/// A live PTY channel: the `russh` channel's **write half** (for input + resize)
/// plus the read task that owns the **read half**, plus the pane tag.
///
/// `russh`'s [`Channel`] read side (`wait`) needs `&mut self`, while writes
/// (`data`) take `&self`. Sharing one `Channel` behind a `Mutex` deadlocks: the read
/// task would hold the lock across its `wait().await` and starve `write`. Instead we
/// [`Channel::split`] the channel into a [`ChannelReadHalf`] (owned exclusively by
/// the read task) and a [`ChannelWriteHalf`] (owned here) — the two run concurrently
/// with no shared lock (the Risks-and-Dependencies concurrency note, realised).
pub struct PtyChannel {
    pane: PaneKind,
    write: ChannelWriteHalf<Msg>,
    reader: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for PtyChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyChannel")
            .field("pane", &self.pane.as_str())
            .finish()
    }
}

impl PtyChannel {
    pub fn pane(&self) -> PaneKind {
        self.pane
    }

    /// Forward operator keystrokes to the remote PTY (input path). The bytes are
    /// the raw `onData` payload from xterm.js; we never interpret them.
    pub async fn write(&self, data: &[u8]) -> Result<(), russh::Error> {
        self.write.data(data).await
    }

    /// Inform the remote PTY of a new size (resize path → `window_change`). Pixel
    /// dimensions are 0 (cell-grid only).
    pub async fn resize(&self, size: PtySize) -> Result<(), russh::Error> {
        self.write.window_change(size.cols, size.rows, 0, 0).await
    }

    /// Stop the read task and close the channel. Called on teardown.
    pub async fn close(self) {
        self.reader.abort();
        let _ = self.write.close().await;
    }
}

/// Open the host-shell PTY (R8): a session channel + `request_pty` + `request_shell`.
///
/// Opens off the shared `Arc<Handle>` (clone `conn.handle()`), so both PTYs ride
/// the single SSH connection (R10). The read task is spawned forwarding raw bytes
/// to `sink`. Independent of the attach PTY (KTD6): the caller opens this first; if
/// it fails the whole terminal surface is down, but it does not touch the attach
/// outcome.
pub async fn open_host_pty(
    handle: SharedHandle,
    size: PtySize,
    sink: Arc<dyn PtySink>,
) -> Result<PtyChannel, ConnectError> {
    let mut channel = open_session_with_pty(&handle, size, PaneKind::Host).await?;

    // `want_reply: true` so the server's Success/Failure for the shell request is
    // emitted; we await it before declaring the shell live (KTD6: a refused shell
    // is an AttachFailure, not a silently dead pane).
    channel.request_shell(true).await.map_err(|e| {
        ConnectError::new(
            ConnectErrorKind::AttachFailure,
            ConnectStage::OpenChannels,
            format!("host shell request failed: {e}"),
        )
    })?;
    let prelude = await_request_reply(&mut channel, PaneKind::Host, "shell").await?;

    Ok(spawn_channel(PaneKind::Host, channel, prelude, sink))
}

/// Open the Hermes-attach PTY (R9, AE2): a session channel + `request_pty` +
/// `exec` of the bot's configured attach command (default `tmux attach -t hermes`,
/// or the per-bot override). The exec'd command is exactly `attach_command` — the
/// AE2 contract.
///
/// Independent outcome (KTD6): a failure here returns
/// [`ConnectErrorKind::AttachFailure`] and the caller keeps the host shell usable.
pub async fn open_attach_pty(
    handle: SharedHandle,
    attach_command: &str,
    size: PtySize,
    sink: Arc<dyn PtySink>,
) -> Result<PtyChannel, ConnectError> {
    let mut channel = open_session_with_pty(&handle, size, PaneKind::Attach).await?;

    // exec the configured attach command verbatim (AE2). `want_reply: true` so the
    // server replies Success/Failure; we await it so a server that REFUSES the exec
    // (e.g. no such tmux session) surfaces as an AttachFailure rather than a
    // silently dead pane. The send itself only queues the request — the accept /
    // reject decision arrives as a channel reply, which `await_request_reply` reads.
    channel
        .exec(true, attach_command.as_bytes())
        .await
        .map_err(|e| {
            ConnectError::new(
                ConnectErrorKind::AttachFailure,
                ConnectStage::OpenChannels,
                format!("attach command exec failed: {e}"),
            )
        })?;
    let prelude = await_request_reply(&mut channel, PaneKind::Attach, "attach exec").await?;

    Ok(spawn_channel(PaneKind::Attach, channel, prelude, sink))
}

/// Open a session channel and request a PTY of `size` on it. Shared by host +
/// attach; tags failures `AttachFailure` at the `open-channels` stage.
async fn open_session_with_pty(
    handle: &SharedHandle,
    size: PtySize,
    pane: PaneKind,
) -> Result<Channel<Msg>, ConnectError> {
    let channel = handle.channel_open_session().await.map_err(|e| {
        ConnectError::new(
            ConnectErrorKind::AttachFailure,
            ConnectStage::OpenChannels,
            format!("{} session channel open failed: {e}", pane.as_str()),
        )
    })?;

    // `want_reply: false` for the PTY request so its accept/reject does NOT add a
    // Success/Failure to the channel stream — that keeps the FIRST reply we await
    // (in `await_request_reply`) unambiguously the shell/exec reply. A bad PTY would
    // surface anyway when the shell/exec is refused.
    channel
        .request_pty(false, PTY_TERM, size.cols, size.rows, 0, 0, &[])
        .await
        .map_err(|e| {
            ConnectError::new(
                ConnectErrorKind::AttachFailure,
                ConnectStage::OpenChannels,
                format!("{} request_pty failed: {e}", pane.as_str()),
            )
        })?;

    Ok(channel)
}

/// Await the server's Success/Failure reply to a `want_reply: true` shell/exec
/// request, so a refused start surfaces as an [`ConnectErrorKind::AttachFailure`]
/// (KTD6) before the pane is declared live.
///
/// A well-behaved server sends the reply before any channel data, but to be safe we
/// buffer any `Data`/`ExtendedData` that arrives *before* the reply and return it so
/// the read task can emit it first (no lost greeting, no out-of-order bytes). Eof /
/// Close before a reply is treated as a failure (the start did not take).
async fn await_request_reply(
    channel: &mut Channel<Msg>,
    pane: PaneKind,
    what: &str,
) -> Result<Vec<u8>, ConnectError> {
    let mut early: Vec<u8> = Vec::new();
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => return Ok(early),
            Some(ChannelMsg::Failure) => {
                return Err(ConnectError::new(
                    ConnectErrorKind::AttachFailure,
                    ConnectStage::OpenChannels,
                    format!("{} {what} request was refused by the server", pane.as_str()),
                ));
            }
            Some(ChannelMsg::Data { data }) => early.extend_from_slice(&data),
            Some(ChannelMsg::ExtendedData { data, .. }) => early.extend_from_slice(&data),
            Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                return Err(ConnectError::new(
                    ConnectErrorKind::AttachFailure,
                    ConnectStage::OpenChannels,
                    format!("{} {what} channel closed before it started", pane.as_str()),
                ));
            }
            // Window adjustments etc. before the reply — keep waiting.
            Some(_) => {}
        }
    }
}

/// Split an opened+started channel into a [`PtyChannel`] (write half) + spawned read
/// task (read half). `prelude` is any data that arrived before the start reply
/// (emitted first, in order).
fn spawn_channel(
    pane: PaneKind,
    channel: Channel<Msg>,
    prelude: Vec<u8>,
    sink: Arc<dyn PtySink>,
) -> PtyChannel {
    let (read_half, write) = channel.split();
    let reader = spawn_reader(read_half, prelude, sink);
    PtyChannel {
        pane,
        write,
        reader,
    }
}

/// Spawn the per-channel read task (KTD4): pump `ChannelMsg::Data` /
/// `ChannelMsg::ExtendedData` into `sink` as raw bytes, coalescing small reads.
///
/// Coalescing: accumulate payloads into `buf`; flush when `buf` reaches
/// [`COALESCE_FLUSH_BYTES`] or [`COALESCE_FLUSH_WINDOW`] has passed since the first
/// un-flushed byte. We model the window with a `tokio::time::sleep` armed only
/// while `buf` is non-empty, racing it against the next channel message. On Eof /
/// Close we flush whatever is buffered and stop. No `String::from_utf8` anywhere —
/// bytes pass through verbatim, so a multibyte sequence split across `wait()`
/// reads is reassembled downstream by xterm.js.
fn spawn_reader(
    mut read_half: ChannelReadHalf,
    prelude: Vec<u8>,
    sink: Arc<dyn PtySink>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Emit any data that arrived before the start reply, in order, first.
        if !prelude.is_empty() && sink.send(&prelude).is_err() {
            return;
        }

        let mut buf: Vec<u8> = Vec::with_capacity(COALESCE_FLUSH_BYTES);

        // Flush helper: send `buf` to the sink and clear it; break on a gone sink.
        macro_rules! flush {
            () => {{
                if !buf.is_empty() {
                    if sink.send(&buf).is_err() {
                        break;
                    }
                    buf.clear();
                }
            }};
        }

        loop {
            // The read half is owned exclusively here (no lock), so awaiting the
            // next message never blocks `write`/`resize` on the write half. Arm the
            // flush timer only when bytes are buffered.
            let next = if buf.is_empty() {
                read_half.wait().await
            } else {
                tokio::select! {
                    msg = read_half.wait() => msg,
                    _ = tokio::time::sleep(COALESCE_FLUSH_WINDOW) => {
                        flush!();
                        continue;
                    }
                }
            };

            match next {
                Some(ChannelMsg::Data { data }) => {
                    buf.extend_from_slice(&data);
                    if buf.len() >= COALESCE_FLUSH_BYTES {
                        flush!();
                    }
                }
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    // Route stderr (ext=1) to the same terminal as stdout — a PTY
                    // interleaves them; xterm renders the combined stream.
                    buf.extend_from_slice(&data);
                    if buf.len() >= COALESCE_FLUSH_BYTES {
                        flush!();
                    }
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                    flush!();
                    break;
                }
                // ExitStatus, Success, Failure, WindowAdjusted, … — nothing to
                // forward to the terminal; keep reading.
                Some(_) => {}
            }
        }
    })
}

/// Production [`PtySink`] over a Tauri `ipc::Channel` (KTD4): each coalesced chunk
/// is shipped as `InvokeResponseBody::Raw(bytes)` — ordered binary, never JSON,
/// never UTF-8 decoded. The frontend creates the `Channel<ArrayBuffer>` and passes
/// it to the open-terminals command; the backend's read task sends raw bytes into
/// it (the `Tnze/tauri-plugin-pty` shape).
pub struct IpcChannelSink {
    channel: tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>,
}

impl IpcChannelSink {
    pub fn new(channel: tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>) -> Self {
        Self { channel }
    }
}

impl PtySink for IpcChannelSink {
    fn send(&self, bytes: &[u8]) -> Result<(), PtySinkError> {
        self.channel
            .send(tauri::ipc::InvokeResponseBody::Raw(bytes.to_vec()))
            .map_err(|e| PtySinkError(e.to_string()))
    }
}

#[cfg(test)]
#[path = "channels_test.rs"]
mod channels_test;
