//! Bot inventory persistence (U3).
//!
//! Saved bots (name, host/IP, Hermes attach command, dashboard port) are
//! persisted to a JSON file in the Tauri app-data dir, written with `0600`
//! permissions at creation (KTD10 — the local-tamper threat: an attacker who can
//! write this file could point the operator at a malicious host). HMAC integrity
//! protection over a Keychain key is noted as a follow-up in the plan; v1 ships
//! `0600` + the documented threat.
//!
//! ## Storage-trait seam (mirrors `keychain::KeyStore`)
//!
//! All file I/O goes through the [`BotStore`] trait. The inventory logic
//! (add/edit/remove/select + default-application) lives in [`Inventory`] and
//! operates on an in-memory [`BotInventory`], so it is pure and trivially
//! testable. The real [`JsonBotStore`] is **path-injected**: the command layer
//! resolves the Tauri app-data dir and constructs it with that path, while tests
//! point it at a `tempfile::TempDir` (or use the in-memory [`MemoryBotStore`])
//! and never touch the real app-data dir.
//!
//! ## Defaults — single source of truth (R6)
//!
//! [`DEFAULT_ATTACH_COMMAND`] and [`DEFAULT_DASHBOARD_PORT`] are the canonical
//! Hermes defaults, taken from the live Hetzner deploy (`deploy/hetzner`): the
//! Hermes session is `tmux attach -t hermes` and the dashboard binds
//! `127.0.0.1:9119`. Applied on add when the operator leaves a field blank. U4
//! (connect) / U5 (attach PTY) / U6 (dashboard forward) read these off the
//! persisted [`Bot`], so they consume already-defaulted values — they MUST NOT
//! re-derive defaults.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Canonical default Hermes attach command (R6). Source of truth: the live
/// Hetzner deploy runs the agent in `tmux` session `hermes`
/// (`deploy/hetzner/README.md`, `attach.sh`).
pub const DEFAULT_ATTACH_COMMAND: &str = "tmux attach -t hermes";

/// Canonical default dashboard remote port (R6). Source of truth: the Hermes
/// dashboard binds `127.0.0.1:9119` on the live deploy
/// (`deploy/hetzner/README.md`, `dashboard.sh`).
pub const DEFAULT_DASHBOARD_PORT: u16 = 9119;

/// Canonical default SSH username (R6). Source of truth: the live Hetzner deploy
/// runs the Hermes agent under the Unix user `hermes` and its tmux session lives
/// there (`deploy/hetzner/cloud-init.yaml`, `lib.sh` `REMOTE_USER`). Connecting
/// as the right user is what lets `tmux attach -t hermes` find the session.
pub const DEFAULT_SSH_USERNAME: &str = "hermes";

/// Inventory file name within the app-data dir.
const BOTS_FILE: &str = "bots.json";

/// A single saved bot.
///
/// Field names are serialized in `camelCase` to match the frontend `Bot` type in
/// `src/state.ts` (`{ id, name, host, attachCommand, dashboardPort }`), so the
/// commands return shapes the webview consumes directly with no re-mapping. The
/// `host` field is the IP/hostname (R4 frames a bot as "name + IP").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bot {
    /// Stable id (uuid v4) assigned on add; update/remove/select target by id.
    pub id: String,
    pub name: String,
    pub host: String,
    /// SSH login user. Always concrete on a persisted bot — the default
    /// ([`DEFAULT_SSH_USERNAME`]) is baked in at add time (R6). The serde default
    /// covers a `bots.json` written before this field existed (the field was added
    /// after the initial schema): an old file with no `username` key deserializes
    /// to the canonical default user rather than failing the whole load and losing
    /// the saved inventory on upgrade.
    #[serde(default = "default_username")]
    pub username: String,
    /// Hermes attach command. Always concrete on a persisted bot — the default
    /// is baked in at add time, never left blank (R6).
    pub attach_command: String,
    /// Remote dashboard port U6 forwards to a loopback port.
    pub dashboard_port: u16,
}

/// Serde default for [`Bot::username`]: the canonical default SSH user. Used when
/// deserializing a `bots.json` written before the `username` field existed, so an
/// upgrade does not lose the saved inventory.
fn default_username() -> String {
    DEFAULT_SSH_USERNAME.to_string()
}

/// Fields the operator supplies when adding/editing a bot. The attach command
/// and dashboard port are optional/blank-able: blank => the default is applied
/// at add time (R6). `name` + `host` are the required core (R4).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotInput {
    pub name: String,
    pub host: String,
    /// Blank/absent => [`DEFAULT_SSH_USERNAME`].
    #[serde(default)]
    pub username: Option<String>,
    /// Blank/absent => [`DEFAULT_ATTACH_COMMAND`].
    #[serde(default)]
    pub attach_command: Option<String>,
    /// Absent (or 0) => [`DEFAULT_DASHBOARD_PORT`].
    #[serde(default)]
    pub dashboard_port: Option<u16>,
}

impl BotInput {
    /// Resolve the SSH username, applying the default when blank (trimmed).
    fn resolved_username(&self) -> String {
        match self.username.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => DEFAULT_SSH_USERNAME.to_string(),
        }
    }

    /// Resolve the attach command, applying the default when blank (trimmed).
    fn resolved_attach_command(&self) -> String {
        match self.attach_command.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => DEFAULT_ATTACH_COMMAND.to_string(),
        }
    }

    /// Resolve the dashboard port, applying the default when absent or 0.
    fn resolved_dashboard_port(&self) -> u16 {
        match self.dashboard_port {
            Some(p) if p != 0 => p,
            _ => DEFAULT_DASHBOARD_PORT,
        }
    }
}

/// The persisted inventory document.
///
/// `selected_bot_id` is the active-bot pointer the connection layer (U4) reads to
/// know which bot to connect. Persisting it means the selection survives relaunch
/// alongside the list (R5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotInventory {
    #[serde(default)]
    pub bots: Vec<Bot>,
    #[serde(default)]
    pub selected_bot_id: Option<String>,
}

/// Errors from the inventory layer.
#[derive(Debug)]
pub enum StoreError {
    /// No bot with the given id (update/remove/select on a stale id).
    NotFound(String),
    /// Required field (name/host) was blank.
    Invalid(String),
    /// The underlying store (file or fake) failed. The string is a
    /// human-readable cause.
    Backend(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::NotFound(id) => write!(f, "no bot with id `{id}`"),
            StoreError::Invalid(msg) => write!(f, "invalid bot: {msg}"),
            StoreError::Backend(msg) => write!(f, "bot store backend error: {msg}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Storage seam for the bot inventory.
///
/// Implementors persist a whole [`BotInventory`] document (the inventory is small;
/// whole-file read/write keeps the seam trivial and atomic-per-op). Mirrors the
/// load/save shape of `keychain::KeyStore`. Tests use [`MemoryBotStore`]; the
/// real impl is [`JsonBotStore`].
pub trait BotStore: Send + Sync {
    /// Load the persisted inventory. A missing store reads as an empty inventory
    /// (first run), NOT an error.
    fn load(&self) -> Result<BotInventory, StoreError>;

    /// Persist the inventory, replacing any prior contents. The file is created
    /// with `0600` permissions on first write (KTD10).
    fn save(&self, inventory: &BotInventory) -> Result<(), StoreError>;
}

// ── Inventory operations (pure over a `BotStore`) ──

/// High-level inventory operations layered over a [`BotStore`]. Each op does a
/// load → mutate → save round-trip so a crash between calls never leaves a
/// half-written list (the save is the commit point). The commands in
/// `commands.rs` are thin wrappers over these.
pub struct Inventory<S: BotStore> {
    store: S,
}

impl<S: BotStore> Inventory<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    /// All saved bots (R5: list for selection).
    pub fn list(&self) -> Result<Vec<Bot>, StoreError> {
        Ok(self.store.load()?.bots)
    }

    /// The full document (bots + current selection). U4 reads the selection.
    pub fn inventory(&self) -> Result<BotInventory, StoreError> {
        self.store.load()
    }

    /// Add a bot, applying defaults to blank fields (R4, R6). Returns the stored
    /// bot (with its assigned id + resolved defaults).
    pub fn add(&self, input: BotInput) -> Result<Bot, StoreError> {
        let name = input.name.trim();
        let host = input.host.trim();
        if name.is_empty() {
            return Err(StoreError::Invalid("name is required".into()));
        }
        if host.is_empty() {
            return Err(StoreError::Invalid("host/IP is required".into()));
        }

        let bot = Bot {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            host: host.to_string(),
            username: input.resolved_username(),
            attach_command: input.resolved_attach_command(),
            dashboard_port: input.resolved_dashboard_port(),
        };

        let mut inv = self.store.load()?;
        inv.bots.push(bot.clone());
        self.store.save(&inv)?;
        Ok(bot)
    }

    /// Edit an existing bot in place. Blank attach/port re-apply the defaults
    /// (consistent with add). Only the targeted bot changes; siblings are
    /// untouched. Errors `NotFound` on a stale id.
    pub fn update(&self, id: &str, input: BotInput) -> Result<Bot, StoreError> {
        let name = input.name.trim();
        let host = input.host.trim();
        if name.is_empty() {
            return Err(StoreError::Invalid("name is required".into()));
        }
        if host.is_empty() {
            return Err(StoreError::Invalid("host/IP is required".into()));
        }

        let mut inv = self.store.load()?;
        let slot = inv
            .bots
            .iter_mut()
            .find(|b| b.id == id)
            .ok_or_else(|| StoreError::NotFound(id.to_string()))?;

        slot.name = name.to_string();
        slot.host = host.to_string();
        slot.username = input.resolved_username();
        slot.attach_command = input.resolved_attach_command();
        slot.dashboard_port = input.resolved_dashboard_port();
        let updated = slot.clone();

        self.store.save(&inv)?;
        Ok(updated)
    }

    /// Remove a bot by id (R4). Clears the selection if it pointed at the removed
    /// bot so U4 never reads a dangling pointer. Errors `NotFound` on a stale id.
    pub fn remove(&self, id: &str) -> Result<(), StoreError> {
        let mut inv = self.store.load()?;
        let before = inv.bots.len();
        inv.bots.retain(|b| b.id != id);
        if inv.bots.len() == before {
            return Err(StoreError::NotFound(id.to_string()));
        }
        if inv.selected_bot_id.as_deref() == Some(id) {
            inv.selected_bot_id = None;
        }
        self.store.save(&inv)
    }

    /// Record the active-bot selection (R5). `None` clears it. A non-`None` id
    /// must reference an existing bot. U4's connect layer reads this pointer.
    pub fn select(&self, id: Option<&str>) -> Result<(), StoreError> {
        let mut inv = self.store.load()?;
        if let Some(id) = id {
            if !inv.bots.iter().any(|b| b.id == id) {
                return Err(StoreError::NotFound(id.to_string()));
            }
        }
        inv.selected_bot_id = id.map(str::to_string);
        self.store.save(&inv)
    }
}

// ── In-memory fake (tests) ──

/// In-memory [`BotStore`] for tests that exercise the inventory logic without any
/// filesystem. Holds the document behind a mutex.
#[derive(Default)]
pub struct MemoryBotStore {
    inner: std::sync::Mutex<BotInventory>,
}

impl MemoryBotStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl BotStore for MemoryBotStore {
    fn load(&self) -> Result<BotInventory, StoreError> {
        Ok(self.inner.lock().expect("MemoryBotStore poisoned").clone())
    }

    fn save(&self, inventory: &BotInventory) -> Result<(), StoreError> {
        *self.inner.lock().expect("MemoryBotStore poisoned") = inventory.clone();
        Ok(())
    }
}

// ── Real JSON-file store (0600) ──

/// File-backed [`BotStore`]: a single `bots.json` in a directory the caller
/// supplies (the Tauri app-data dir in production; a temp dir in tests). The file
/// is created with `0600` permissions and the perms are re-asserted on every save
/// so a pre-existing looser file is tightened (KTD10).
pub struct JsonBotStore {
    path: PathBuf,
}

impl JsonBotStore {
    /// Construct a store whose JSON file lives at `dir/bots.json`. The directory
    /// is created (recursively) on the first save if it does not exist.
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join(BOTS_FILE),
        }
    }

    /// Path of the backing JSON file (for diagnostics/tests).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl BotStore for JsonBotStore {
    fn load(&self) -> Result<BotInventory, StoreError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Backend(format!("parse {}: {e}", self.path.display()))),
            // Missing file == first run == empty inventory.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BotInventory::default()),
            Err(e) => Err(StoreError::Backend(format!(
                "read {}: {e}",
                self.path.display()
            ))),
        }
    }

    fn save(&self, inventory: &BotInventory) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StoreError::Backend(format!("mkdir {}: {e}", parent.display())))?;
        }
        let json = serde_json::to_vec_pretty(inventory)
            .map_err(|e| StoreError::Backend(format!("serialize: {e}")))?;
        crate::fs::write_0600(&self.path, &json)
            .map_err(|e| StoreError::Backend(format!("write {}: {e}", self.path.display())))
    }
}

#[cfg(test)]
#[path = "store_test.rs"]
mod store_test;
