//! The [`Signer`] abstraction and its v1 ed25519 implementation (U2).
//!
//! `Signer` is the seam U4 authenticates through and a future hardware-backed
//! P-256 / Secure-Enclave signer will replace (KTD2). It is deliberately
//! impl-agnostic: it exposes the OpenSSH public key, the SSH algorithm id, and a
//! raw-signature operation, and says nothing about *where* the private key lives
//! (Keychain, Enclave, file). Private-key material is never exposed through this
//! trait except via the explicit [`Ed25519Signer::export_openssh_private`] path
//! that backs the `export_key` command.
//!
//! ## Security invariants (R3)
//!
//! - In-memory private bytes live in [`SecretBytes`], which zeroizes on drop and
//!   has a **redacting `Debug`** (`SecretBytes([redacted])`) — bytes never reach
//!   a log or a panic message.
//! - No `Signer` method returns private material. `public_openssh` / `algorithm`
//!   return only public data; `sign` returns a signature, not the key.
//! - The only egress for private material is `export_openssh_private`, which the
//!   `export_key` command calls behind an operator confirmation.

use std::fmt;

use ed25519_dalek::{Signer as _, SigningKey, Verifier as _, VerifyingKey};
use rand_core::OsRng;
use ssh_key::private::{Ed25519Keypair, KeypairData, PrivateKey};
use ssh_key::public::KeyData;
use ssh_key::{Algorithm, LineEnding, PublicKey};
use zeroize::Zeroize;

use crate::keychain::{KeyStore, KeyStoreError};

/// SSH algorithm identifier for the v1 ed25519 signer.
pub const SSH_ED25519: &str = "ssh-ed25519";

/// A zeroize-on-drop, redacting wrapper around secret byte material.
///
/// `Debug` prints `SecretBytes([redacted])` and never the contents, so the type
/// is safe to embed in structs that derive `Debug` or get logged. The buffer is
/// zeroed when dropped so the OpenSSH private bytes do not linger in freed
/// memory. This is the only place raw private bytes are held in this unit.
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Borrow the secret bytes. Callers must not log or persist the result
    /// except through the explicit export path.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never emit the bytes — redact unconditionally.
        f.write_str("SecretBytes([redacted])")
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Errors a signer can surface. U4 maps these onto the connect pipeline's
/// error classes (KTD6): a `KeyStore` failure / `Sign` failure becomes the
/// `local-signer-failure` class, distinct from a remote auth rejection.
#[derive(Debug)]
pub enum SignerError {
    /// No key is provisioned yet (`generate` not called).
    NoKey,
    /// The key store (Keychain/fake) failed.
    Store(KeyStoreError),
    /// The stored bytes were not a valid OpenSSH private key, or the wrong
    /// algorithm. The message MUST NOT include key material.
    InvalidKey(String),
    /// Encoding the public/private OpenSSH form failed.
    Encode(String),
}

impl fmt::Display for SignerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignerError::NoKey => write!(f, "no signing key provisioned"),
            SignerError::Store(e) => write!(f, "key store error: {e}"),
            SignerError::InvalidKey(m) => write!(f, "invalid stored key: {m}"),
            SignerError::Encode(m) => write!(f, "key encoding error: {m}"),
        }
    }
}

impl std::error::Error for SignerError {}

impl From<KeyStoreError> for SignerError {
    fn from(e: KeyStoreError) -> Self {
        match e {
            KeyStoreError::NotFound => SignerError::NoKey,
            other => SignerError::Store(other),
        }
    }
}

/// The auth/signing abstraction U4 consumes and a future hardware signer
/// replaces (KTD2).
///
/// Implementations hold (or reach) exactly one signing key. All methods are
/// public-data or signature operations — none expose the private key.
pub trait Signer: Send + Sync {
    /// The SSH algorithm id, e.g. `"ssh-ed25519"`. U4 advertises this to the
    /// server during publickey auth; a future P-256 signer returns its own id
    /// (a key-rotation event per KTD2).
    fn algorithm(&self) -> &'static str;

    /// The OpenSSH-format public key string (`ssh-ed25519 AAAA... [comment]`),
    /// suitable for one-click copy and remote provisioning.
    fn public_openssh(&self) -> Result<String, SignerError>;

    /// Sign an arbitrary challenge/message, returning the raw signature bytes
    /// (for ed25519, the 64-byte detached signature). U4 wraps these in the SSH
    /// wire signature for publickey auth.
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, SignerError>;
}

/// v1 [`Signer`]: ed25519, private key held in a [`KeyStore`] (Keychain in
/// production, in-memory in tests).
///
/// The store owns the only persistent copy of the private bytes. Each operation
/// loads + decodes them into a [`SecretBytes`] for the duration of the call;
/// nothing keeps a long-lived plaintext copy in this struct.
pub struct Ed25519Signer {
    store: Box<dyn KeyStore>,
}

impl Ed25519Signer {
    /// Wrap a key store. Does not generate; call [`Ed25519Signer::generate`] (or
    /// the `generate_key` command) to provision a key.
    pub fn new(store: Box<dyn KeyStore>) -> Self {
        Self { store }
    }

    /// Idempotently ensure a key exists, returning its OpenSSH public string.
    ///
    /// If a key already exists, returns the existing public key and does NOT
    /// overwrite (KTD2 — `store_if_absent` enforces this). If none exists,
    /// generates an ed25519 keypair, encodes it to the OpenSSH private format,
    /// and stores those bytes.
    pub fn generate(&self) -> Result<String, SignerError> {
        if self.store.exists()? {
            return self.public_openssh();
        }

        // Generate from the OS CSPRNG.
        let keypair = Ed25519Keypair::random(&mut OsRng);
        let private = PrivateKey::new(KeypairData::Ed25519(keypair), "botbox")
            .map_err(|e| SignerError::Encode(e.to_string()))?;

        // Encode to OpenSSH private format. `to_openssh` returns a
        // `Zeroizing<String>` — wrap the bytes in `SecretBytes` so the copy we
        // hand to the store is zeroized on drop too.
        let pem = private
            .to_openssh(LineEnding::LF)
            .map_err(|e| SignerError::Encode(e.to_string()))?;
        let secret = SecretBytes::new(pem.as_bytes().to_vec());

        // `store_if_absent` returns false if a key raced in between the check and
        // the write; in that case we keep the existing one (idempotent).
        self.store.store_if_absent(secret.expose())?;
        self.public_openssh()
    }

    /// Load + decode the stored private key into an `ssh-key` `PrivateKey`.
    /// The decoded key is held only for the duration of the caller's use.
    fn load_private(&self) -> Result<PrivateKey, SignerError> {
        let bytes = SecretBytes::new(self.store.load()?);
        PrivateKey::from_openssh(bytes.expose())
            .map_err(|e| SignerError::InvalidKey(e.to_string()))
    }

    /// Extract the dalek signing key from the stored ed25519 private key.
    fn signing_key(&self) -> Result<SigningKey, SignerError> {
        let private = self.load_private()?;
        match private.key_data() {
            KeypairData::Ed25519(kp) => SigningKey::try_from(kp)
                .map_err(|e| SignerError::InvalidKey(e.to_string())),
            _ => Err(SignerError::InvalidKey("stored key is not ed25519".into())),
        }
    }

    /// The OpenSSH **private** key string. The ONLY private-material egress in
    /// this unit; `export_key` calls this behind an operator confirmation (R17).
    /// Returns a [`SecretBytes`] so the caller (the command) zeroizes it after
    /// writing the file.
    pub fn export_openssh_private(&self) -> Result<SecretBytes, SignerError> {
        let private = self.load_private()?;
        let pem = private
            .to_openssh(LineEnding::LF)
            .map_err(|e| SignerError::Encode(e.to_string()))?;
        Ok(SecretBytes::new(pem.as_bytes().to_vec()))
    }

    /// The public verifying key, for tests/verification (proves the stored key
    /// is usable for auth — KTD2 / U4).
    pub fn verifying_key(&self) -> Result<VerifyingKey, SignerError> {
        let public = self.public_key_data()?;
        match public {
            KeyData::Ed25519(pk) => {
                VerifyingKey::try_from(&pk).map_err(|e| SignerError::InvalidKey(e.to_string()))
            }
            _ => Err(SignerError::InvalidKey("stored key is not ed25519".into())),
        }
    }

    fn public_key_data(&self) -> Result<KeyData, SignerError> {
        let private = self.load_private()?;
        Ok(private.public_key().key_data().clone())
    }
}

impl Signer for Ed25519Signer {
    fn algorithm(&self) -> &'static str {
        // Guard against a stored non-ed25519 key — but v1 only ever writes
        // ed25519, so this is the static id. (A future signer overrides this.)
        debug_assert_eq!(Algorithm::Ed25519.as_str(), SSH_ED25519);
        SSH_ED25519
    }

    fn public_openssh(&self) -> Result<String, SignerError> {
        let private = self.load_private()?;
        let public: &PublicKey = private.public_key();
        public
            .to_openssh()
            .map_err(|e| SignerError::Encode(e.to_string()))
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, SignerError> {
        let signing = self.signing_key()?;
        let sig = signing.sign(message);
        Ok(sig.to_vec())
    }
}

/// Verify a raw ed25519 signature against an OpenSSH public key string. Used by
/// tests to prove a `Signer` signature round-trips against the published key.
pub fn verify_openssh_ed25519(
    public_openssh: &str,
    message: &[u8],
    signature: &[u8],
) -> Result<(), SignerError> {
    let public = PublicKey::from_openssh(public_openssh)
        .map_err(|e| SignerError::InvalidKey(e.to_string()))?;
    let pk = match public.key_data() {
        KeyData::Ed25519(pk) => pk,
        _ => return Err(SignerError::InvalidKey("not an ed25519 public key".into())),
    };
    let verifying =
        VerifyingKey::try_from(pk).map_err(|e| SignerError::InvalidKey(e.to_string()))?;
    let sig = ed25519_dalek::Signature::try_from(signature)
        .map_err(|e| SignerError::InvalidKey(e.to_string()))?;
    verifying
        .verify(message, &sig)
        .map_err(|e| SignerError::InvalidKey(e.to_string()))
}

#[cfg(test)]
#[path = "signer_test.rs"]
mod signer_test;
