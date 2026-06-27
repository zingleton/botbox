//! Pipeline error-class mapping tests (U4; test-first per the execution note).
//!
//! These are pure-data tests written BEFORE any `russh` wiring: they pin the
//! `ConnectStage` → `ConnectErrorKind` default mapping and the **1:1**
//! correspondence between `ConnectErrorKind` and the frontend
//! `ConnectionErrorKind` discriminants in `src/state.ts`. If either drifts, U7's
//! per-class UI silently mis-renders, so these assertions are load-bearing.

use super::*;

/// Every stage has a *default* failure class — the class a bare failure at that
/// stage maps to when the pipeline cannot classify it more specifically (a TCP
/// failure is always unreachable; a host-key failure defaults to untrusted; auth
/// defaults to remote rejection; channels → attach; probe → wrong port). The
/// pipeline overrides the default for the cases it CAN distinguish (e.g. a signer
/// failure during authenticate, or a mismatch during host-key-check).
#[test]
fn each_stage_has_a_default_error_class() {
    assert_eq!(
        default_kind_for_stage(ConnectStage::TcpConnect),
        ConnectErrorKind::Unreachable
    );
    assert_eq!(
        default_kind_for_stage(ConnectStage::HostKeyCheck),
        ConnectErrorKind::UntrustedHostKey
    );
    assert_eq!(
        default_kind_for_stage(ConnectStage::Authenticate),
        ConnectErrorKind::RemoteAuthFailure
    );
    assert_eq!(
        default_kind_for_stage(ConnectStage::OpenChannels),
        ConnectErrorKind::AttachFailure
    );
    assert_eq!(
        default_kind_for_stage(ConnectStage::ProbeDashboard),
        ConnectErrorKind::WrongDashboardPort
    );
}

/// The signer-failure class is DISTINCT from the remote-auth-failure class even
/// though both are tagged at the `authenticate` stage (KTD6): a Keychain/signer
/// failure must not route the operator to re-paste a correct key.
#[test]
fn signer_failure_is_distinct_from_remote_auth_failure() {
    assert_ne!(
        ConnectErrorKind::LocalSignerFailure,
        ConnectErrorKind::RemoteAuthFailure
    );
    // Both are authenticate-stage failures, but the signer one is the override,
    // not the default.
    assert_eq!(
        default_kind_for_stage(ConnectStage::Authenticate),
        ConnectErrorKind::RemoteAuthFailure
    );
    assert_eq!(
        ConnectErrorKind::LocalSignerFailure.as_kind(),
        "local-signer-failure"
    );
}

/// Stage discriminants are exactly the frontend `ConnectStage` union strings.
#[test]
fn stage_strings_match_frontend() {
    assert_eq!(ConnectStage::TcpConnect.as_str(), "tcp-connect");
    assert_eq!(ConnectStage::HostKeyCheck.as_str(), "host-key-check");
    assert_eq!(ConnectStage::Authenticate.as_str(), "authenticate");
    assert_eq!(ConnectStage::OpenChannels.as_str(), "open-channels");
    assert_eq!(ConnectStage::ProbeDashboard.as_str(), "probe-dashboard");
}

/// Error-class discriminants are exactly the frontend `ConnectionErrorKind`
/// union strings — the 1:1 mapping U7 relies on. All eight classes covered.
#[test]
fn error_kind_strings_match_frontend_one_to_one() {
    let pairs = [
        (ConnectErrorKind::Unreachable, "unreachable-host"),
        (ConnectErrorKind::UntrustedHostKey, "untrusted-host-key"),
        (ConnectErrorKind::HostKeyMismatch, "host-key-mismatch"),
        (ConnectErrorKind::RemoteAuthFailure, "remote-auth-failure"),
        (ConnectErrorKind::LocalSignerFailure, "local-signer-failure"),
        (ConnectErrorKind::WrongDashboardPort, "wrong-dashboard-port"),
        (ConnectErrorKind::AttachFailure, "attach-failure"),
        (ConnectErrorKind::ConnectionLost, "connection-lost"),
    ];
    for (kind, expected) in pairs {
        assert_eq!(kind.as_kind(), expected, "drift for {kind:?}");
    }
    // No two classes share a discriminant string (injectivity).
    let mut seen = std::collections::HashSet::new();
    for (kind, _) in pairs {
        assert!(seen.insert(kind.as_kind()), "duplicate kind for {kind:?}");
    }
}

/// A stage-tagged `ConnectError` carries both the class and the stage, and its
/// Display is redaction-safe (carries the message we put in, nothing else).
#[test]
fn connect_error_carries_stage_and_kind() {
    let e = ConnectError::new(
        ConnectErrorKind::Unreachable,
        ConnectStage::TcpConnect,
        "connection refused",
    );
    assert_eq!(e.kind, ConnectErrorKind::Unreachable);
    assert_eq!(e.stage, ConnectStage::TcpConnect);
    assert_eq!(e.to_string(), "[tcp-connect] connection refused");
}
