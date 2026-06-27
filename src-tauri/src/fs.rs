//! Shared filesystem primitives.
//!
//! [`write_0600`] is the single owner-only file writer used by every place that
//! persists sensitive material to the local disk: the exported private key
//! (`commands.rs`), the bot inventory (`store.rs`), and the TOFU known-hosts store
//! (`ssh/known_hosts.rs`). Centralising it keeps the `0600` invariant (KTD10 — the
//! local-tamper threat) in one audited place rather than triplicated.

use std::io;
use std::path::Path;

/// Write `bytes` to `path`, creating the file with `0600` (owner read/write only)
/// from the start so it is never momentarily group/world-readable, and tightening
/// the perms if the file pre-existed with looser bits.
///
/// On non-Unix targets (not a v1 target) the file is written without the Unix
/// permission guarantee.
pub fn write_0600(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;

    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        f.flush()?;
        // `mode()` only applies when the file is created; if it pre-existed with
        // looser perms, tighten it explicitly.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
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
