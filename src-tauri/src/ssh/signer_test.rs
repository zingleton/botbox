//! Tests for the ed25519 [`Signer`] and the export path (U2; AE1 coverage).
//!
//! Unit tests run against the in-memory [`MemoryKeyStore`] via the storage-trait
//! seam, so they need no real Keychain (no OS prompts, no flakiness). One real
//! Keychain test lives at the bottom, gated behind `#[ignore]`; run it manually
//! with `cargo test -- --ignored` on macOS (it may trigger an interactive
//! Keychain prompt).

use super::*;
use crate::keychain::{KeyStore, KeyStoreError, MemoryKeyStore, MemoryPublicKeyCache};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn signer() -> Ed25519Signer {
    Ed25519Signer::new(Box::new(MemoryKeyStore::new()))
}

/// A `KeyStore` wrapper that counts `load()` calls, so a test can prove the
/// public-key cache avoids private-key reads (and thus the Keychain prompt).
struct CountingKeyStore {
    inner: MemoryKeyStore,
    loads: Arc<AtomicUsize>,
}

impl KeyStore for CountingKeyStore {
    fn load(&self) -> Result<Vec<u8>, KeyStoreError> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.inner.load()
    }
    fn store_if_absent(&self, bytes: &[u8]) -> Result<bool, KeyStoreError> {
        self.inner.store_if_absent(bytes)
    }
}

#[test]
fn public_cache_avoids_private_key_reads_for_public_openssh() {
    let loads = Arc::new(AtomicUsize::new(0));
    let store = CountingKeyStore {
        inner: MemoryKeyStore::new(),
        loads: loads.clone(),
    };
    let s = Ed25519Signer::new(Box::new(store))
        .with_public_cache(Box::new(MemoryPublicKeyCache::new()));

    // generate() decodes the key it just made — it must NOT re-read the store to
    // derive the public key, and it back-fills the cache.
    let pubkey = s.generate().expect("generate");
    let after_generate = loads.load(Ordering::SeqCst);

    // Subsequent public_openssh() calls answer from the cache: zero new reads.
    assert_eq!(s.public_openssh().unwrap(), pubkey);
    assert_eq!(s.public_openssh().unwrap(), pubkey);
    assert_eq!(
        loads.load(Ordering::SeqCst),
        after_generate,
        "public_openssh must not read the key store when the cache is warm"
    );

    // sign() still needs the private key (this is the only path that prompts).
    let _ = s.sign(b"challenge").expect("sign");
    assert!(
        loads.load(Ordering::SeqCst) > after_generate,
        "sign() reads the private key"
    );
}

#[test]
fn public_cache_miss_backfills_from_private_key() {
    // Pre-provision a key, attach an EMPTY cache: the first public_openssh
    // derives from the private key (one read) and back-fills, so the second
    // call is a cache hit (no further read). Models an existing key from before
    // the cache existed.
    let seed = signer();
    let seed_bytes = {
        seed.generate().expect("seed generate");
        seed.export_openssh_private().expect("export seed")
    };

    let loads = Arc::new(AtomicUsize::new(0));
    let inner = MemoryKeyStore::new();
    inner
        .store_if_absent(seed_bytes.expose())
        .expect("seed store"); // store_if_absent does not count as a load
    let store = CountingKeyStore {
        inner,
        loads: loads.clone(),
    };
    let s = Ed25519Signer::new(Box::new(store))
        .with_public_cache(Box::new(MemoryPublicKeyCache::new()));

    let first = s.public_openssh().expect("derive");
    let after_first = loads.load(Ordering::SeqCst);
    assert!(after_first >= 1, "first call reads the private key");
    let second = s.public_openssh().expect("cache hit");
    assert_eq!(first, second);
    assert_eq!(
        loads.load(Ordering::SeqCst),
        after_first,
        "second call is a cache hit — no further private-key read"
    );
}

#[test]
fn generate_when_absent_stores_and_returns_valid_public_key() {
    let s = signer();
    let public = s.generate().expect("generate succeeds");

    // Valid OpenSSH ed25519 public string.
    assert!(public.starts_with("ssh-ed25519 "), "got: {public}");
    let parsed = PublicKey::from_openssh(&public).expect("public key parses");
    assert_eq!(parsed.algorithm(), Algorithm::Ed25519);

    // Signer reports the ed25519 algorithm id.
    assert_eq!(s.algorithm(), "ssh-ed25519");
}

#[test]
fn generate_is_idempotent_and_does_not_overwrite() {
    let s = signer();
    let first = s.generate().expect("first generate");
    let second = s.generate().expect("second generate (idempotent)");
    assert_eq!(first, second, "public key unchanged across two generate calls");

    // get_public_key (via public_openssh) returns the same key.
    assert_eq!(s.public_openssh().unwrap(), first);
}

#[test]
fn public_key_round_trips_as_parseable_ed25519() {
    let s = signer();
    let public = s.generate().unwrap();
    let parsed = PublicKey::from_openssh(&public).expect("round-trip parse");
    assert!(parsed.key_data().ed25519().is_some(), "is an ed25519 key");
}

#[test]
fn signature_verifies_against_public_key() {
    // Proves the stored key is usable for auth (U4): a signature from the
    // Signer verifies under the published public key.
    let s = signer();
    let public = s.generate().unwrap();

    let challenge = b"botbox-auth-challenge-\x00\x01\x02 random bytes";
    let sig = s.sign(challenge).expect("sign");
    assert_eq!(sig.len(), 64, "ed25519 detached signature is 64 bytes");

    verify_openssh_ed25519(&public, challenge, &sig).expect("signature verifies");

    // A tampered message must NOT verify.
    let mut bad = challenge.to_vec();
    bad[0] ^= 0xFF;
    assert!(
        verify_openssh_ed25519(&public, &bad, &sig).is_err(),
        "tampered message must fail verification"
    );
}

#[test]
fn secret_bytes_debug_is_redacted() {
    // The private-key wrapper never emits bytes via Debug.
    let secret = SecretBytes::new(b"super-secret-private-key-bytes".to_vec());
    let rendered = format!("{secret:?}");
    assert_eq!(rendered, "SecretBytes([redacted])");
    assert!(!rendered.contains("secret"), "must not leak content");
    assert!(!rendered.contains("private"), "must not leak content");
}

#[test]
fn signer_error_display_never_leaks_key_material() {
    // SignerError is the type that could surface in a log/IPC error; ensure its
    // Display carries no key bytes (only NoKey/store/encoding context).
    let e = SignerError::NoKey;
    assert_eq!(e.to_string(), "no signing key provisioned");
}

#[test]
fn export_produces_parseable_openssh_private_key() {
    // The export path is the ONLY private-material egress. Assert the exported
    // bytes parse back as the same ed25519 private key.
    let s = signer();
    let public = s.generate().unwrap();

    let exported = s.export_openssh_private().expect("export");
    let pem = std::str::from_utf8(exported.expose()).expect("utf8 pem");
    assert!(
        pem.contains("BEGIN OPENSSH PRIVATE KEY"),
        "exported bytes are an OpenSSH private key"
    );

    let reparsed = PrivateKey::from_openssh(pem).expect("private key parses");
    // The reparsed private key yields the same public key.
    assert_eq!(reparsed.public_key().to_openssh().unwrap(), public);
}

#[test]
fn export_before_generate_yields_no_key() {
    let s = signer();
    assert!(matches!(
        s.export_openssh_private(),
        Err(SignerError::NoKey)
    ));
    assert!(matches!(s.public_openssh(), Err(SignerError::NoKey)));
    assert!(matches!(s.sign(b"x"), Err(SignerError::NoKey)));
}

// ── Real Keychain integration test (manual) ─────────────────────────────────
//
// Exercises the real `security-framework` path end to end: generate → store in
// the login Keychain → read back → verify a signature. Gated `#[ignore]` because
// an unsigned `cargo test` binary may trigger an interactive Keychain prompt and
// the item persists in the login keychain. Run manually:
//
//     cargo test -- --ignored real_keychain
//
// It cleans up the item it creates on the way in and out.
#[cfg(any(target_os = "macos", target_os = "ios"))]
#[test]
#[ignore = "touches the real login Keychain; run manually with --ignored"]
fn real_keychain_round_trip() {
    use crate::keychain::{keychain_delete_for_tests, KeychainKeyStore};

    // Start clean so a stale item from a prior run doesn't make generate a no-op.
    keychain_delete_for_tests().expect("pre-clean");

    let s = Ed25519Signer::new(Box::new(KeychainKeyStore::new()));
    let public = s.generate().expect("generate into real Keychain");
    assert!(public.starts_with("ssh-ed25519 "));

    // Idempotent against the real store.
    assert_eq!(s.generate().unwrap(), public, "idempotent on real Keychain");

    // The key read back from the Keychain signs and verifies.
    let msg = b"real-keychain-challenge";
    let sig = s.sign(msg).expect("sign with Keychain key");
    verify_openssh_ed25519(&public, msg, &sig).expect("verify");

    keychain_delete_for_tests().expect("post-clean");
}
