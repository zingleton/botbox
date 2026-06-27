//! TOFU known-hosts store tests (U4; KTD5, R16, KTD10 coverage).
//!
//! Decision-logic tests run over the in-memory [`MemoryKnownHostsStore`] via the
//! storage-trait seam (no filesystem). The persistence + `0600` test uses a
//! `tempfile::TempDir` against the real [`JsonKnownHostsStore`].

use super::*;
use ssh_key::private::{Ed25519Keypair, KeypairData, PrivateKey};
use ssh_key::LineEnding;
use rand_core::OsRng;

/// Generate a throwaway ed25519 public key for tests (distinct each call).
fn random_pubkey() -> PublicKey {
    let kp = Ed25519Keypair::random(&mut OsRng);
    let private = PrivateKey::new(KeypairData::Ed25519(kp), "test").unwrap();
    // Round-trip through OpenSSH to mirror how a real host key would be parsed.
    let openssh = private
        .public_key()
        .to_openssh()
        .unwrap();
    let _ = private.to_openssh(LineEnding::LF).unwrap();
    PublicKey::from_openssh(&openssh).unwrap()
}

#[test]
fn unknown_host_yields_prompt_with_sha256_fingerprint() {
    let kh = KnownHosts::new(MemoryKnownHostsStore::new());
    let key = random_pubkey();

    let decision = kh.decide("10.0.0.5", &key).unwrap();
    match decision {
        HostKeyDecision::Unknown { fingerprint } => {
            assert!(
                fingerprint.starts_with("SHA256:"),
                "fingerprint is OpenSSH SHA-256 form, got {fingerprint}"
            );
            assert_eq!(fingerprint, sha256_fingerprint(&key));
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn trust_persists_and_subsequent_decide_is_known() {
    let kh = KnownHosts::new(MemoryKnownHostsStore::new());
    let key = random_pubkey();

    // First contact: unknown.
    assert!(matches!(
        kh.decide("host-a", &key).unwrap(),
        HostKeyDecision::Unknown { .. }
    ));

    // Operator accepts: persist.
    kh.trust("host-a", &key).unwrap();
    assert!(kh.saved("host-a").unwrap().is_some());

    // Same key now reads as Known.
    assert_eq!(kh.decide("host-a", &key).unwrap(), HostKeyDecision::Known);
}

#[test]
fn known_host_with_changed_key_is_a_mismatch_hard_stop() {
    let kh = KnownHosts::new(MemoryKnownHostsStore::new());
    let original = random_pubkey();
    let imposter = random_pubkey();

    kh.trust("host-b", &original).unwrap();

    // A different key for a trusted host is a mismatch carrying both fingerprints.
    match kh.decide("host-b", &imposter).unwrap() {
        HostKeyDecision::Mismatch {
            saved_fingerprint,
            presented_fingerprint,
        } => {
            assert_eq!(saved_fingerprint, sha256_fingerprint(&original));
            assert_eq!(presented_fingerprint, sha256_fingerprint(&imposter));
            assert_ne!(saved_fingerprint, presented_fingerprint);
        }
        other => panic!("expected Mismatch, got {other:?}"),
    }
}

#[test]
fn trust_does_not_silently_replace_on_mismatch_until_removed() {
    // R16: re-trust after a mismatch requires the explicit remove step. We model
    // the connect path: decide -> Mismatch is a hard stop; only after remove does
    // the host go back to Unknown (re-promptable), and trusting the new key works.
    let kh = KnownHosts::new(MemoryKnownHostsStore::new());
    let original = random_pubkey();
    let rotated = random_pubkey();

    kh.trust("host-c", &original).unwrap();
    assert!(matches!(
        kh.decide("host-c", &rotated).unwrap(),
        HostKeyDecision::Mismatch { .. }
    ));

    // Without removal, the saved key is still the original (never silently bumped).
    assert_eq!(
        kh.saved("host-c").unwrap().unwrap(),
        original.to_openssh().unwrap()
    );

    // Explicit recovery: remove, then the host is promptable again.
    kh.remove("host-c").unwrap();
    assert!(matches!(
        kh.decide("host-c", &rotated).unwrap(),
        HostKeyDecision::Unknown { .. }
    ));

    // And re-trusting the rotated key now sticks.
    kh.trust("host-c", &rotated).unwrap();
    assert_eq!(kh.decide("host-c", &rotated).unwrap(), HostKeyDecision::Known);
}

#[test]
fn remove_absent_host_is_a_noop() {
    let kh = KnownHosts::new(MemoryKnownHostsStore::new());
    kh.remove("never-seen").unwrap(); // does not error
}

#[test]
fn distinct_hosts_are_independent() {
    let kh = KnownHosts::new(MemoryKnownHostsStore::new());
    let key_a = random_pubkey();
    let key_b = random_pubkey();
    kh.trust("a", &key_a).unwrap();
    kh.trust("b", &key_b).unwrap();

    assert_eq!(kh.decide("a", &key_a).unwrap(), HostKeyDecision::Known);
    assert_eq!(kh.decide("b", &key_b).unwrap(), HostKeyDecision::Known);
    // a's key presented for b is a mismatch.
    assert!(matches!(
        kh.decide("b", &key_a).unwrap(),
        HostKeyDecision::Mismatch { .. }
    ));
}

// ── Real file persistence + 0600 (KTD10) ──

#[test]
fn persists_to_disk_and_reloads_after_simulated_restart() {
    let dir = tempfile::tempdir().unwrap();
    let key = random_pubkey();

    {
        let kh = KnownHosts::new(JsonKnownHostsStore::new(dir.path()));
        kh.trust("203.0.113.7", &key).unwrap();
    }

    // Fresh store over the same path sees the trusted key (relaunch).
    let kh2 = KnownHosts::new(JsonKnownHostsStore::new(dir.path()));
    assert_eq!(kh2.decide("203.0.113.7", &key).unwrap(), HostKeyDecision::Known);
}

#[cfg(unix)]
#[test]
fn store_file_is_created_0600() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let store = JsonKnownHostsStore::new(dir.path());
    let kh = KnownHosts::new(JsonKnownHostsStore::new(dir.path()));
    kh.trust("h", &random_pubkey()).unwrap();

    let mode = std::fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "known-hosts store must be owner-only (KTD10)");
}
