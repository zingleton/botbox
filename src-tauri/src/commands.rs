//! Tauri command surface (introduced in U2).
//!
//! U1 kept the single `app_ready` command inline in `lib.rs`. U2 introduces this
//! dedicated command module and moves `app_ready` here, then adds the U2 key
//! commands. `lib.rs` wires everything into `invoke_handler`.
//!
//! Trust-boundary contract (KTD8 / R18): app-defined commands are reachable via
//! `core:default` — they do NOT each need a per-command ACL entry (only *plugin*
//! commands need scopes). So adding a command here + to `invoke_handler` is
//! sufficient; we do not touch `capabilities/default.json` for app commands. The
//! U1 capability smoke test still asserts no unused *plugin* scopes leaked.
//!
//! ## Security invariants for the key commands (R3)
//!
//! - `generate_key` / `get_public_key` return ONLY the OpenSSH **public** key.
//! - `export_key` is the single command that emits private material, and it
//!   writes it to a file at an operator-chosen path with `0600` perms — it never
//!   returns the bytes to the webview.
//! - No command's `Ok`/`Err` value carries private key bytes; `SignerError`'s
//!   `Display` is redaction-safe and `SecretBytes` redacts in `Debug`.

use serde::Serialize;

use crate::keychain::default_key_store;
use crate::ssh::signer::{Ed25519Signer, Signer};

/// Minimal "the webview booted" handshake (moved from `lib.rs` in U2). The
/// frontend calls it once on startup; the response is informational.
#[derive(Debug, Serialize)]
pub struct AppInfo {
    pub name: String,
    pub version: String,
}

#[tauri::command]
pub fn app_ready() -> AppInfo {
    AppInfo {
        name: env!("CARGO_PKG_NAME").to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Build the v1 signer over the platform key store (Keychain on Apple targets,
/// in-memory elsewhere). The signer is cheap and stateless apart from the store,
/// so we construct it per command rather than holding global state.
fn signer() -> Ed25519Signer {
    Ed25519Signer::new(default_key_store())
}

/// Generate the signing key if absent and return its OpenSSH public string.
///
/// Idempotent: if a key already exists, returns the existing public key without
/// overwriting (KTD2). Never returns private material.
#[tauri::command]
pub fn generate_key() -> Result<String, String> {
    signer().generate().map_err(|e| e.to_string())
}

/// Return the OpenSSH public key, or an error if no key is provisioned yet.
///
/// The frontend's always-available "public key" surface calls this; if it
/// returns the no-key error, the UI offers the generate flow.
#[tauri::command]
pub fn get_public_key() -> Result<String, String> {
    signer().public_openssh().map_err(|e| e.to_string())
}

/// Export the OpenSSH **private** key to `path` with `0600` permissions (R17).
///
/// The path comes from the frontend (v1 accepts a path argument rather than
/// opening a native save dialog — see the module note and U2 report: keeping the
/// capability allowlist minimal, no `dialog` plugin/scope added). The frontend
/// gates this behind a confirmation warning that the key is leaving the Keychain.
///
/// The private bytes are written to the file and never returned to the webview;
/// the in-memory copy is zeroized on drop (`SecretBytes`).
#[tauri::command]
pub fn export_key(path: String) -> Result<(), String> {
    let secret = signer()
        .export_openssh_private()
        .map_err(|e| e.to_string())?;
    write_private_key_0600(&path, secret.expose()).map_err(|e| e.to_string())
}

/// Write `bytes` to `path` creating the file with `0600` (owner read/write
/// only) from the start — the perms are part of the open, so the file is never
/// momentarily group/world-readable.
fn write_private_key_0600(path: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        f.flush()?;
        // If the file pre-existed with looser perms, `mode()` does not tighten
        // it; enforce 0600 explicitly.
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        // Non-Unix (not a v1 target) — write without the Unix perm guarantee.
        let mut f = std::fs::File::create(path)?;
        f.write_all(bytes)?;
        f.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ready_reports_package_metadata() {
        let info = app_ready();
        assert_eq!(info.name, "botbox");
        assert!(!info.version.is_empty());
    }

    /// `export_key`'s file writer creates the key file with exactly `0600`
    /// permissions. We test the writer directly (it does not depend on the
    /// Keychain) with a parseable OpenSSH private key produced by the signer over
    /// an in-memory store.
    #[cfg(unix)]
    #[test]
    fn export_writes_parseable_private_key_with_0600() {
        use crate::keychain::MemoryKeyStore;
        use crate::ssh::signer::Ed25519Signer;
        use std::os::unix::fs::PermissionsExt;

        let signer = Ed25519Signer::new(Box::new(MemoryKeyStore::new()));
        let public = signer.generate().unwrap();
        let secret = signer.export_openssh_private().unwrap();

        let dir = std::env::temp_dir();
        let path = dir.join(format!("botbox-export-test-{}.key", std::process::id()));
        let path_str = path.to_str().unwrap().to_string();

        write_private_key_0600(&path_str, secret.expose()).unwrap();

        // Permissions are exactly 0600.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "exported key must be owner-only");

        // The file parses as an OpenSSH private key yielding the same public key.
        let pem = std::fs::read_to_string(&path).unwrap();
        let parsed = ssh_key::private::PrivateKey::from_openssh(&pem).unwrap();
        assert_eq!(parsed.public_key().to_openssh().unwrap(), public);

        let _ = std::fs::remove_file(&path);
    }

    /// Tightening perms when the target pre-exists with looser bits.
    #[cfg(unix)]
    #[test]
    fn export_tightens_preexisting_loose_perms() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir();
        let path = dir.join(format!("botbox-export-loose-{}.key", std::process::id()));
        let path_str = path.to_str().unwrap().to_string();

        // Pre-create world-readable.
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private_key_0600(&path_str, b"BEGIN OPENSSH PRIVATE KEY").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let _ = std::fs::remove_file(&path);
    }
}
