//! Keychain-backed private-key storage (U2).
//!
//! The private key (OpenSSH-encoded bytes) lives in the macOS Keychain as a
//! generic-password item scoped to the app bundle id, accessible
//! `WhenUnlockedThisDeviceOnly` (KTD2). It is **never** returned over IPC except
//! through the explicit `export_key` path; no command, log, or `Debug` emits it.
//!
//! ## Storage-trait seam
//!
//! All persistence goes through the [`KeyStore`] trait. The signer (see
//! `ssh::signer`) depends on `KeyStore`, never on `security-framework` directly,
//! so unit tests run against an in-memory [`MemoryKeyStore`] with no Keychain
//! access (no OS prompts, no flakiness). The real
//! [`KeychainKeyStore`] is exercised by an `#[ignore]`d integration test that
//! must be run manually (`cargo test -- --ignored`) because it may trigger an
//! interactive Keychain prompt on first write/read.
//!
//! Stored bytes are wrapped in [`SecretBytes`](crate::ssh::signer::SecretBytes)
//! at the signer boundary; the store itself moves opaque byte vectors.

use std::fmt;

/// Keychain service/account naming. Scoped to the app bundle id so the item is
/// namespaced to Botbox and distinct from any other app's Keychain entries.
///
/// Mirrors `tauri.conf.json`'s `identifier`. Kept here (not read from the conf
/// at runtime) so the storage layer has no dependency on Tauri config loading;
/// a drift test could assert these match if the identifier ever changes.
pub const KEYCHAIN_SERVICE: &str = "ai.aipowerguild.botbox";
/// Account/key within the service for the single v1 signing key.
pub const KEYCHAIN_ACCOUNT: &str = "ssh-signing-key-ed25519";

/// Errors from the storage layer. Variants are coarse on purpose: U4 maps a
/// `KeyStoreError` from the signer onto the `local-signer-failure` error class
/// (distinct from a remote auth rejection — see KTD6).
#[derive(Debug)]
pub enum KeyStoreError {
    /// No item exists for the configured service/account.
    NotFound,
    /// The underlying Keychain (or fake) failed. The string is a
    /// human-readable cause; it MUST NOT contain key material.
    Backend(String),
}

impl fmt::Display for KeyStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyStoreError::NotFound => write!(f, "no key found in store"),
            KeyStoreError::Backend(msg) => write!(f, "key store backend error: {msg}"),
        }
    }
}

impl std::error::Error for KeyStoreError {}

/// Storage seam for the private signing key.
///
/// Implementors persist raw OpenSSH-private-key bytes. The trait is intentionally
/// minimal — get / put-if-absent / exists — so a future Secure-Enclave-backed
/// store (which would hold a key handle rather than bytes) can be slotted behind
/// the same `Signer` without reworking the seam (KTD2 notes that swap is a
/// key-rotation event, but the storage boundary stays stable).
pub trait KeyStore: Send + Sync {
    /// Return the stored OpenSSH private-key bytes, or `NotFound`.
    fn load(&self) -> Result<Vec<u8>, KeyStoreError>;

    /// Store bytes only if no item exists yet. Returns `true` if it wrote a new
    /// item, `false` if one already existed (idempotent generate — KTD2). Never
    /// overwrites: callers rely on this to preserve an existing key.
    fn store_if_absent(&self, bytes: &[u8]) -> Result<bool, KeyStoreError>;

    /// Whether an item currently exists.
    fn exists(&self) -> Result<bool, KeyStoreError> {
        match self.load() {
            Ok(_) => Ok(true),
            Err(KeyStoreError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

// ── In-memory fake (tests + non-Apple builds) ──

/// In-memory [`KeyStore`] used by unit tests and as the default on platforms
/// without a Keychain. Holds bytes behind a mutex; never touches the OS.
#[derive(Default)]
pub struct MemoryKeyStore {
    inner: std::sync::Mutex<Option<Vec<u8>>>,
}

impl MemoryKeyStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl KeyStore for MemoryKeyStore {
    fn load(&self) -> Result<Vec<u8>, KeyStoreError> {
        let guard = self.inner.lock().expect("MemoryKeyStore mutex poisoned");
        guard.clone().ok_or(KeyStoreError::NotFound)
    }

    fn store_if_absent(&self, bytes: &[u8]) -> Result<bool, KeyStoreError> {
        let mut guard = self.inner.lock().expect("MemoryKeyStore mutex poisoned");
        if guard.is_some() {
            return Ok(false);
        }
        *guard = Some(bytes.to_vec());
        Ok(true)
    }
}

// ── Real macOS/iOS Keychain implementation ──

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod sys {
    use super::{KeyStore, KeyStoreError, KEYCHAIN_ACCOUNT, KEYCHAIN_SERVICE};
    use core_foundation::base::{TCFType, ToVoid};
    use core_foundation::data::CFData;
    use core_foundation::dictionary::CFMutableDictionary;
    use core_foundation::string::{CFString, CFStringRef};
    use security_framework::item::{add_item, ItemAddOptions, ItemAddValue, ItemClass};
    use security_framework::passwords::{delete_generic_password, get_generic_password};
    use security_framework_sys::access_control::kSecAttrAccessibleWhenUnlockedThisDeviceOnly;

    /// `errSecItemNotFound`.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

    // The `kSecAttrAccessible` *key* constant is not re-exported by
    // `security-framework-sys`, so we link it directly from Security.framework.
    // The matching *value* (`...WhenUnlockedThisDeviceOnly`) IS re-exported and
    // is imported above.
    #[link(name = "Security", kind = "framework")]
    extern "C" {
        static kSecAttrAccessible: CFStringRef;
    }

    /// The real macOS/iOS Keychain-backed store.
    ///
    /// Stores the OpenSSH private-key bytes as a generic-password item scoped to
    /// the bundle id (`service`/`account`), with
    /// `kSecAttrAccessibleWhenUnlockedThisDeviceOnly` accessibility (KTD2). Reads
    /// go through `security-framework`'s convenience `get_generic_password`;
    /// writes go through `add_item` so we can set the accessibility attribute the
    /// convenience `set_generic_password` does not expose.
    #[derive(Default)]
    pub struct KeychainKeyStore;

    impl KeychainKeyStore {
        pub fn new() -> Self {
            Self
        }
    }

    impl KeyStore for KeychainKeyStore {
        fn load(&self) -> Result<Vec<u8>, KeyStoreError> {
            match get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT) {
                Ok(bytes) => Ok(bytes),
                Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Err(KeyStoreError::NotFound),
                Err(e) => Err(KeyStoreError::Backend(e.to_string())),
            }
        }

        fn store_if_absent(&self, bytes: &[u8]) -> Result<bool, KeyStoreError> {
            // Idempotent: never overwrite an existing key (KTD2).
            match self.load() {
                Ok(_) => return Ok(false),
                Err(KeyStoreError::NotFound) => {}
                Err(e) => return Err(e),
            }
            add_with_accessibility(bytes)?;
            Ok(true)
        }
    }

    /// Add a generic-password item carrying our service/account and
    /// `kSecAttrAccessibleWhenUnlockedThisDeviceOnly`.
    ///
    /// We build the base add-dictionary via `ItemAddOptions` (maintained API),
    /// promote it to a mutable dictionary, and inject the accessibility
    /// attribute the builder does not expose, then hand it to `add_item`
    /// (`SecItemAdd`).
    fn add_with_accessibility(bytes: &[u8]) -> Result<(), KeyStoreError> {
        let value = ItemAddValue::Data {
            class: ItemClass::generic_password(),
            data: CFData::from_buffer(bytes),
        };
        let mut opts = ItemAddOptions::new(value);
        opts.set_service(KEYCHAIN_SERVICE)
            .set_account_name(KEYCHAIN_ACCOUNT)
            .set_label("Botbox SSH signing key");

        let base = opts.to_dictionary();
        let mut dict = CFMutableDictionary::from(&base);
        // SAFETY: the two CFString constants are framework globals (get-rule);
        // `add` into a CFType dictionary retains key+value, so they may drop
        // after this block.
        unsafe {
            let key = CFString::wrap_under_get_rule(kSecAttrAccessible);
            let val = CFString::wrap_under_get_rule(kSecAttrAccessibleWhenUnlockedThisDeviceOnly);
            // `to_dictionary()` produces a `*const c_void`-keyed dictionary, so
            // add via raw void pointers.
            dict.add(&key.to_void(), &val.to_void());
        }

        add_item(dict.to_immutable()).map_err(|e| KeyStoreError::Backend(e.to_string()))
    }

    /// Test/cleanup helper: remove the stored item. Only used by the ignored
    /// integration test so it leaves the Keychain clean.
    pub fn delete_for_tests() -> Result<(), KeyStoreError> {
        match delete_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            Err(e) => Err(KeyStoreError::Backend(e.to_string())),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use sys::KeychainKeyStore;

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use sys::delete_for_tests as keychain_delete_for_tests;

/// The production [`KeyStore`] for the current platform: the real Keychain on
/// Apple targets, the in-memory store elsewhere (so the crate still builds and
/// the signer is exercisable on non-Apple CI).
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub fn default_key_store() -> Box<dyn KeyStore> {
    Box::new(KeychainKeyStore::new())
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
pub fn default_key_store() -> Box<dyn KeyStore> {
    Box::new(MemoryKeyStore::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trips_and_is_idempotent() {
        let store = MemoryKeyStore::new();
        assert!(!store.exists().unwrap());
        assert!(matches!(store.load(), Err(KeyStoreError::NotFound)));

        let first = b"openssh-private-bytes-v1".to_vec();
        assert!(store.store_if_absent(&first).unwrap(), "first write happens");
        assert!(store.exists().unwrap());
        assert_eq!(store.load().unwrap(), first);

        // store_if_absent must NOT overwrite (idempotent generate).
        let second = b"different-bytes".to_vec();
        assert!(
            !store.store_if_absent(&second).unwrap(),
            "second write is a no-op"
        );
        assert_eq!(store.load().unwrap(), first, "existing key preserved");
    }
}
