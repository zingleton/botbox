//! Trust-on-first-use (TOFU) known-hosts store (U4; KTD5, R16, KTD10).
//!
//! On first contact with a host, `check_server_key` consults this store and finds
//! the host *unknown* — the connect pipeline surfaces the SHA-256 fingerprint to
//! the UI and awaits a Trust/Reject decision, persisting the key on accept. On a
//! later connect to a *known* host, the presented key must match the saved key
//! byte-for-byte; a *mismatch* is a hard stop (a possible MITM) that the operator
//! can only recover from by explicitly removing the saved key first
//! (`remove_known_host`) — we NEVER silently update a saved key.
//!
//! ## Storage-trait seam (mirrors `store::BotStore` / `keychain::KeyStore`)
//!
//! All persistence goes through the [`KnownHostsStore`] trait. The decision logic
//! ([`KnownHosts`]) is pure over the trait, so unit tests run against the in-memory
//! [`MemoryKnownHostsStore`] with no filesystem. The real [`JsonKnownHostsStore`]
//! is **path-injected** (the command layer resolves the Tauri app-data dir; tests
//! point it at a `tempfile::TempDir`) and is written `0600` at creation (KTD10) —
//! the local-tamper threat (an attacker who can write this file pre-trusts a
//! malicious host and defeats TOFU) is documented in the plan; HMAC integrity is
//! a deferred follow-up.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use ssh_key::{HashAlg, PublicKey};

/// Known-hosts file name within the app-data dir.
const KNOWN_HOSTS_FILE: &str = "known_hosts.json";

/// The persisted known-hosts document: host → saved OpenSSH public key string.
///
/// Keyed by the *host* the operator connects to (the bot's `host` field). The
/// value is the canonical OpenSSH public key string (`ssh-ed25519 AAAA...`), which
/// round-trips back into a `ssh_key::PublicKey` for the byte-for-byte comparison.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownHostsDoc {
    /// `BTreeMap` so the serialized file has a stable key order (deterministic
    /// diffs / tests).
    #[serde(default)]
    pub hosts: BTreeMap<String, String>,
}

/// Errors from the known-hosts layer.
#[derive(Debug)]
pub enum KnownHostsError {
    /// The underlying store (file or fake) failed.
    Backend(String),
    /// A stored or presented key was not a parseable OpenSSH public key. The
    /// message MUST NOT include private material (public keys are not secret, but
    /// we keep messages terse).
    InvalidKey(String),
}

impl std::fmt::Display for KnownHostsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KnownHostsError::Backend(m) => write!(f, "known-hosts store error: {m}"),
            KnownHostsError::InvalidKey(m) => write!(f, "invalid host key: {m}"),
        }
    }
}

impl std::error::Error for KnownHostsError {}

/// The TOFU decision for a presented host key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyDecision {
    /// The host is known and the presented key matches the saved key — proceed.
    Known,
    /// The host is unknown (first contact) — prompt the operator with the
    /// fingerprint. The carried string is the SHA-256 fingerprint (`SHA256:...`).
    Unknown { fingerprint: String },
    /// The host is known but the presented key DIFFERS from the saved key — a
    /// hard-stop mismatch (possible MITM). Carries both fingerprints for the UI.
    Mismatch {
        saved_fingerprint: String,
        presented_fingerprint: String,
    },
}

/// Storage seam for the known-hosts document. Implementors persist a whole
/// [`KnownHostsDoc`]; the document is small so whole-file read/write keeps the
/// seam trivial and atomic-per-op.
pub trait KnownHostsStore: Send + Sync {
    /// Load the persisted document. A missing store reads as empty (first run),
    /// NOT an error.
    fn load(&self) -> Result<KnownHostsDoc, KnownHostsError>;
    /// Persist the document, replacing prior contents. Created `0600` (KTD10).
    fn save(&self, doc: &KnownHostsDoc) -> Result<(), KnownHostsError>;
}

/// SHA-256 fingerprint (`SHA256:base64`) of a public key, in the OpenSSH format
/// shown by `ssh-keygen -lf`. This is what the operator sees in the trust prompt.
pub fn sha256_fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// TOFU decision logic layered over a [`KnownHostsStore`]. The connect pipeline
/// calls [`KnownHosts::decide`] from inside `check_server_key`; on an accepted
/// prompt it calls [`KnownHosts::trust`]; the `remove_known_host` command calls
/// [`KnownHosts::remove`].
pub struct KnownHosts<S: KnownHostsStore> {
    store: S,
}

impl<S: KnownHostsStore> KnownHosts<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    /// Classify a presented host key for `host` as Known / Unknown / Mismatch.
    ///
    /// Comparison is byte-for-byte on the parsed key data (not string equality),
    /// so a re-encoded-but-identical key still reads as Known. A saved value that
    /// no longer parses is treated as a mismatch (defensive: never silently trust).
    pub fn decide(&self, host: &str, presented: &PublicKey) -> Result<HostKeyDecision, KnownHostsError> {
        let doc = self.store.load()?;
        match doc.hosts.get(host) {
            None => Ok(HostKeyDecision::Unknown {
                fingerprint: sha256_fingerprint(presented),
            }),
            Some(saved_openssh) => {
                let saved = PublicKey::from_openssh(saved_openssh)
                    .map_err(|e| KnownHostsError::InvalidKey(e.to_string()))?;
                if saved.key_data() == presented.key_data() {
                    Ok(HostKeyDecision::Known)
                } else {
                    Ok(HostKeyDecision::Mismatch {
                        saved_fingerprint: sha256_fingerprint(&saved),
                        presented_fingerprint: sha256_fingerprint(presented),
                    })
                }
            }
        }
    }

    /// Persist `key` as the trusted key for `host` (on an accepted trust prompt).
    ///
    /// This is the ONLY way a host key enters the store. It overwrites any prior
    /// entry — but the connect path only ever calls it after `decide` returned
    /// `Unknown`, and a `Mismatch` requires an explicit [`KnownHosts::remove`]
    /// first, so a saved key is never silently replaced (R16).
    pub fn trust(&self, host: &str, key: &PublicKey) -> Result<(), KnownHostsError> {
        let openssh = key
            .to_openssh()
            .map_err(|e| KnownHostsError::InvalidKey(e.to_string()))?;
        let mut doc = self.store.load()?;
        doc.hosts.insert(host.to_string(), openssh);
        self.store.save(&doc)
    }

    /// Remove the saved key for `host` (the explicit mismatch-recovery step that
    /// must precede any re-trust; R16). Removing an absent host is a no-op.
    pub fn remove(&self, host: &str) -> Result<(), KnownHostsError> {
        let mut doc = self.store.load()?;
        if doc.hosts.remove(host).is_some() {
            self.store.save(&doc)?;
        }
        Ok(())
    }

    /// The saved OpenSSH public key string for `host`, if any (diagnostics/tests).
    pub fn saved(&self, host: &str) -> Result<Option<String>, KnownHostsError> {
        Ok(self.store.load()?.hosts.get(host).cloned())
    }
}

/// Object-safe decider seam so the connection handler can be **non-generic** over
/// the store type (the handler holds an `Arc<dyn HostKeyDecider>`). [`KnownHosts`]
/// implements it for any [`KnownHostsStore`]. This keeps `Handle<ClientHandler>` a
/// single concrete type across the handshake and live phases (U4) and out of
/// U5/U6's way.
///
/// Keys cross this seam as **OpenSSH public-key strings**, not typed `PublicKey`
/// values, deliberately: `russh` vendors a *fork* of `ssh-key`
/// (`internal-russh-forked-ssh-key`), so russh's `PublicKey` is a distinct Rust
/// type from U2's `ssh_key::PublicKey` even though both encode the identical
/// OpenSSH wire format (KTD2 / U2's interop note). The string is the type-agnostic
/// interop surface; this store parses it with its own `ssh-key` to compute
/// fingerprints and compare key data.
pub trait HostKeyDecider: Send + Sync {
    /// Classify a presented key (given as its OpenSSH string) for `host`.
    fn decide_openssh(&self, host: &str, presented_openssh: &str) -> Result<HostKeyDecision, KnownHostsError>;
    /// Persist a trusted key (OpenSSH string) for `host`.
    fn trust_openssh(&self, host: &str, key_openssh: &str) -> Result<(), KnownHostsError>;
}

impl<S: KnownHostsStore> HostKeyDecider for KnownHosts<S> {
    fn decide_openssh(&self, host: &str, presented_openssh: &str) -> Result<HostKeyDecision, KnownHostsError> {
        let presented = PublicKey::from_openssh(presented_openssh)
            .map_err(|e| KnownHostsError::InvalidKey(e.to_string()))?;
        KnownHosts::decide(self, host, &presented)
    }
    fn trust_openssh(&self, host: &str, key_openssh: &str) -> Result<(), KnownHostsError> {
        let key = PublicKey::from_openssh(key_openssh)
            .map_err(|e| KnownHostsError::InvalidKey(e.to_string()))?;
        KnownHosts::trust(self, host, &key)
    }
}

// ── In-memory fake (tests) ──

/// In-memory [`KnownHostsStore`] for tests; holds the document behind a mutex.
#[derive(Default)]
pub struct MemoryKnownHostsStore {
    inner: std::sync::Mutex<KnownHostsDoc>,
}

impl MemoryKnownHostsStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl KnownHostsStore for MemoryKnownHostsStore {
    fn load(&self) -> Result<KnownHostsDoc, KnownHostsError> {
        Ok(self.inner.lock().expect("MemoryKnownHostsStore poisoned").clone())
    }

    fn save(&self, doc: &KnownHostsDoc) -> Result<(), KnownHostsError> {
        *self.inner.lock().expect("MemoryKnownHostsStore poisoned") = doc.clone();
        Ok(())
    }
}

// ── Real JSON-file store (0600) ──

/// File-backed [`KnownHostsStore`]: a single `known_hosts.json` in a caller-
/// supplied directory (Tauri app-data dir in production; a temp dir in tests),
/// created `0600` and re-tightened on every save (KTD10). Mirrors
/// `store::JsonBotStore`.
pub struct JsonKnownHostsStore {
    path: PathBuf,
}

impl JsonKnownHostsStore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join(KNOWN_HOSTS_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl KnownHostsStore for JsonKnownHostsStore {
    fn load(&self) -> Result<KnownHostsDoc, KnownHostsError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| KnownHostsError::Backend(format!("parse {}: {e}", self.path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(KnownHostsDoc::default()),
            Err(e) => Err(KnownHostsError::Backend(format!(
                "read {}: {e}",
                self.path.display()
            ))),
        }
    }

    fn save(&self, doc: &KnownHostsDoc) -> Result<(), KnownHostsError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| KnownHostsError::Backend(format!("mkdir {}: {e}", parent.display())))?;
        }
        let json = serde_json::to_vec_pretty(doc)
            .map_err(|e| KnownHostsError::Backend(format!("serialize: {e}")))?;
        crate::fs::write_0600(&self.path, &json)
            .map_err(|e| KnownHostsError::Backend(format!("write {}: {e}", self.path.display())))
    }
}

#[cfg(test)]
#[path = "known_hosts_test.rs"]
mod known_hosts_test;
