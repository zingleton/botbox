//! Botbox Rust backend (U1 scaffold).
//!
//! U1 stands up a buildable Tauri 2 app with the webview<->backend trust
//! boundary in place (strict CSP + capability allowlist; see
//! `tauri.conf.json` and `capabilities/default.json`). It deliberately ships
//! **no SSH logic** — only the command surface skeleton the frontend needs so
//! the capability allowlist has concrete commands to enumerate.
//!
//! Later units extend the command set (see the plan's Output Structure):
//!   - U2: `generate_key`, `get_public_key`, `export_key`
//!   - U3: `list_bots`, `add_bot`, `update_bot`, `remove_bot`, `select_bot`
//!     (+ `get_inventory`, returning bots + the persisted selection U4 reads)
//!   - U4: `connect`, `disconnect`, `trust_host`, `remove_known_host`
//!   - U5: `pty_write`, `pty_resize` (+ `ipc::Channel` raw byte streams)
//!   - U6: `open_tunnel`, `open_dashboard`
//!
//! Each new app-defined command MUST be added to the `invoke_handler` below
//! (app commands are reachable via `core:default`; only *plugin* commands need a
//! scope entry in `capabilities/default.json`). KTD8 / R18.
//!
//! U2 introduces `commands.rs` (the command surface, including the relocated
//! `app_ready`) and the `ssh` + `keychain` modules.

pub mod commands;
pub mod keychain;
pub mod ssh;
pub mod store;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // `opener` is the only plugin we register; its scope is locked down in
        // the capability file (`opener:allow-open-url` scoped to loopback) so it
        // can open ONLY the `http://127.0.0.1:<port>` dashboard URL and nothing
        // else. U6 uses it (R13) both from the Rust `open_dashboard` command and
        // directly from the webview "Open Dashboard" button.
        .plugin(tauri_plugin_opener::init())
        // U4 connection state: one active connection (validate-before-swap) +
        // the pending host-key trust prompts. App-defined commands reach it via
        // `tauri::State`; no plugin scope is involved (KTD8 boundary unchanged).
        .manage(commands::SshState::new())
        .invoke_handler(tauri::generate_handler![
            commands::app_ready,
            commands::generate_key,
            commands::get_public_key,
            commands::export_key,
            commands::list_bots,
            commands::get_inventory,
            commands::add_bot,
            commands::update_bot,
            commands::remove_bot,
            commands::select_bot,
            commands::connect,
            commands::disconnect,
            commands::trust_host,
            commands::remove_known_host,
            commands::open_terminals,
            commands::pty_write,
            commands::pty_resize,
            commands::open_tunnel,
            commands::open_dashboard,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Botbox");
}

#[cfg(test)]
mod tests {
    /// Smoke test for KTD8 / R18: the capability allowlist is the
    /// webview<->backend trust boundary. It must grant only what the app needs
    /// and MUST NOT grant unused plugin scopes. App-defined commands (e.g.
    /// `app_ready`) are reachable via `core:default`; we don't enumerate them
    /// here (Tauri gates *plugin* commands through this ACL).
    ///
    /// As of **U6** the ONE allowed plugin capability is the `opener` plugin's
    /// `open_url`, scoped to the loopback dashboard URLs (opening
    /// `http://127.0.0.1:<port>` is the U6 dashboard capability — R13). The test
    /// asserts:
    ///   - `core:default` is granted,
    ///   - `opener:allow-open-url` IS granted AND is narrowly scoped to loopback
    ///     (`http://127.0.0.1:*` / `http://localhost:*`) — never an unscoped
    ///     open-url grant or a wider host,
    ///   - no OTHER `opener:*` command scope leaked (e.g. `open_path`,
    ///     `reveal_item_in_dir`),
    ///   - no other later-unit plugin scope leaked.
    #[test]
    fn capability_allowlist_grants_only_scoped_opener_for_dashboard() {
        let caps = include_str!("../capabilities/default.json");
        let caps: serde_json::Value =
            serde_json::from_str(caps).expect("capabilities/default.json is valid JSON");

        let entries = caps["permissions"]
            .as_array()
            .expect("capabilities has a permissions array");

        // Permission entries are either a bare string identifier or an object
        // `{ "identifier": "...", "allow": [...] }` (the scoped form). Collect
        // each entry's identifier for the prefix checks.
        let identifier = |p: &serde_json::Value| -> Option<String> {
            p.as_str()
                .map(str::to_string)
                .or_else(|| p["identifier"].as_str().map(str::to_string))
        };
        let identifiers: Vec<String> = entries.iter().filter_map(identifier).collect();

        // Core defaults are granted.
        assert!(
            identifiers.iter().any(|p| p == "core:default"),
            "expected `core:default` to be granted"
        );

        // Exactly the scoped open-url opener capability is granted (U6 dashboard).
        let opener_entries: Vec<&serde_json::Value> = entries
            .iter()
            .filter(|p| identifier(p).as_deref() == Some("opener:allow-open-url"))
            .collect();
        assert_eq!(
            opener_entries.len(),
            1,
            "expected exactly one `opener:allow-open-url` grant (the U6 dashboard capability)"
        );

        // It must be the SCOPED object form (carry an `allow` list), never the bare
        // unscoped identifier — an unscoped open-url grant would let the webview
        // open arbitrary URLs.
        let opener = opener_entries[0];
        let allow = opener["allow"]
            .as_array()
            .expect("opener:allow-open-url must be scoped with an `allow` list (not unscoped)");
        assert!(!allow.is_empty(), "opener scope `allow` list must not be empty");

        // Every allowed URL pattern is loopback-only.
        for entry in allow {
            let url = entry["url"]
                .as_str()
                .expect("each opener scope entry is a `{ url }` object");
            assert!(
                url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:"),
                "opener scope must be loopback-only, found `{url}`"
            );
        }

        // No OTHER opener command scope leaked (only open_url is allowed).
        for p in &identifiers {
            assert!(
                p == "opener:allow-open-url" || !p.starts_with("opener:"),
                "capability allowlist leaked a non-dashboard opener scope: `{p}`"
            );
        }

        // No other later-unit plugin scopes should be present (defense in depth).
        for forbidden_prefix in ["shell:", "fs:", "http:", "dialog:", "process:"] {
            for p in &identifiers {
                assert!(
                    !p.starts_with(forbidden_prefix),
                    "capability allowlist leaked an unused plugin scope: `{p}`"
                );
            }
        }
    }
}
