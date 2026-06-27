//! Staged connect pipeline + per-stage error classes (U4; KTD6, R11/R16).
//!
//! This module is the *heart* of U4: it runs a connect as an ordered sequence of
//! stages and tags every failure with the stage it occurred at, mapping that to a
//! distinct [`ConnectionErrorKind`]. The error-class enum maps **1:1** onto the
//! frontend `ConnectionErrorKind` union in `src/state.ts`, so U7 can render each
//! class as a distinct, actionable surface.
//!
//! ## Stages (mirrors `ConnectStage` in `state.ts`)
//!
//! ```text
//! tcp-connect → host-key-check → authenticate → open-channels → probe-dashboard
//! ```
//!
//! U4 owns the pipeline through **authenticate** and exposes a clean seam for U5
//! (`open-channels`, the PTYs) and U6 (`probe-dashboard`). The error classes for
//! those later stages already exist here so the enum is complete and the frontend
//! mapping is stable; U4's pipeline simply does not *reach* them yet.
//!
//! ## Test-first
//!
//! Per the U4 execution note, the [`ConnectStage`] → [`ConnectErrorKind`] mapping
//! and its 1:1 correspondence to the frontend `kind` strings are proven by
//! `pipeline_test.rs` *before* any `russh` wiring. The mapping is pure data, so it
//! is tested without a socket.

use std::fmt;

/// The ordered connect stages. 1:1 with the frontend `ConnectStage` union.
///
/// The string forms returned by [`ConnectStage::as_str`] are exactly the frontend
/// discriminants, so a stage emitted here drives the `connect-stage` action
/// without re-mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectStage {
    /// Open the TCP socket (with a connect timeout).
    TcpConnect,
    /// TOFU host-key verification (`check_server_key`): known / unknown / mismatch.
    HostKeyCheck,
    /// publickey auth via the [`Signer`](crate::ssh::signer::Signer).
    Authenticate,
    /// Open the host + attach PTY channels (U5 owns the PTYs).
    OpenChannels,
    /// Eager dashboard-port probe (U6 owns the forward).
    ProbeDashboard,
}

impl ConnectStage {
    /// The frontend discriminant string (matches `ConnectStage` in `state.ts`).
    pub fn as_str(self) -> &'static str {
        match self {
            ConnectStage::TcpConnect => "tcp-connect",
            ConnectStage::HostKeyCheck => "host-key-check",
            ConnectStage::Authenticate => "authenticate",
            ConnectStage::OpenChannels => "open-channels",
            ConnectStage::ProbeDashboard => "probe-dashboard",
        }
    }
}

impl fmt::Display for ConnectStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The connect error classes (KTD6 / R11). Maps **1:1** onto the frontend
/// `ConnectionErrorKind` union in `src/state.ts` — [`ConnectErrorKind::as_kind`]
/// returns exactly the frontend discriminant string for each variant.
///
/// The mapping from a *stage* to its class is what makes the surfaced errors
/// distinguishable: a TCP failure is `Unreachable`, a server key rejection is
/// `RemoteAuthFailure`, a Keychain/signer failure is the **distinct**
/// `LocalSignerFailure` (so the operator is NOT sent to re-paste a correct key),
/// and so on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectErrorKind {
    /// TCP connect failed or timed out (stage: tcp-connect).
    Unreachable,
    /// Host key is unknown — the TOFU trust prompt is surfaced (stage:
    /// host-key-check). Recoverable by accepting the prompt.
    UntrustedHostKey,
    /// Host key changed vs the saved key — hard stop, recoverable only via an
    /// explicit `remove_known_host` (stage: host-key-check).
    HostKeyMismatch,
    /// The server rejected our key → route to the provision-your-key surface
    /// (stage: authenticate).
    RemoteAuthFailure,
    /// The local [`Signer`](crate::ssh::signer::Signer) could not produce a
    /// signature (locked Keychain, cancelled OS prompt, missing item). DISTINCT
    /// from a remote rejection — must NOT route to provision-your-key (stage:
    /// authenticate).
    LocalSignerFailure,
    /// Nothing listening on the dashboard port (U6's eager probe; stage:
    /// probe-dashboard).
    WrongDashboardPort,
    /// A PTY channel (host or attach) failed to open (U5; stage: open-channels).
    AttachFailure,
    /// The transport died mid-session — surfaced by the driver task with a
    /// manual-reconnect affordance.
    ConnectionLost,
}

impl ConnectErrorKind {
    /// The frontend `ConnectionErrorKind` discriminant string. 1:1 mapping.
    pub fn as_kind(self) -> &'static str {
        match self {
            ConnectErrorKind::Unreachable => "unreachable-host",
            ConnectErrorKind::UntrustedHostKey => "untrusted-host-key",
            ConnectErrorKind::HostKeyMismatch => "host-key-mismatch",
            ConnectErrorKind::RemoteAuthFailure => "remote-auth-failure",
            ConnectErrorKind::LocalSignerFailure => "local-signer-failure",
            ConnectErrorKind::WrongDashboardPort => "wrong-dashboard-port",
            ConnectErrorKind::AttachFailure => "attach-failure",
            ConnectErrorKind::ConnectionLost => "connection-lost",
        }
    }
}

/// The *default* error class for a stage: the class a bare, otherwise-unclassified
/// failure at that stage maps to. The pipeline overrides this for the cases it can
/// distinguish more precisely (a host-key *mismatch* vs unknown, a *signer* failure
/// vs a remote rejection during authenticate). This keeps the stage→class fallback
/// in one place so a new stage cannot be added without choosing its class.
pub fn default_kind_for_stage(stage: ConnectStage) -> ConnectErrorKind {
    match stage {
        ConnectStage::TcpConnect => ConnectErrorKind::Unreachable,
        ConnectStage::HostKeyCheck => ConnectErrorKind::UntrustedHostKey,
        ConnectStage::Authenticate => ConnectErrorKind::RemoteAuthFailure,
        ConnectStage::OpenChannels => ConnectErrorKind::AttachFailure,
        ConnectStage::ProbeDashboard => ConnectErrorKind::WrongDashboardPort,
    }
}

/// A stage-tagged connect failure carrying the error class, the stage it was
/// tagged at, and a redaction-safe human message. This is the value the connect
/// pipeline surfaces to the command layer, which maps it onto the frontend
/// `connect-failed` / `connection-lost` actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectError {
    pub kind: ConnectErrorKind,
    pub stage: ConnectStage,
    pub message: String,
}

impl ConnectError {
    pub fn new(kind: ConnectErrorKind, stage: ConnectStage, message: impl Into<String>) -> Self {
        Self {
            kind,
            stage,
            message: message.into(),
        }
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.stage, self.message)
    }
}

impl std::error::Error for ConnectError {}

#[cfg(test)]
#[path = "pipeline_test.rs"]
mod pipeline_test;
