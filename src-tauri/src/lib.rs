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
        // the capability file so it cannot open arbitrary URLs/paths. U6 uses
        // it to open the loopback dashboard URL.
        .plugin(tauri_plugin_opener::init())
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running Botbox");
}

#[cfg(test)]
mod tests {
    /// Smoke test for KTD8 / R18: the capability allowlist is the
    /// webview<->backend trust boundary. It must grant only what U1 needs and
    /// MUST NOT grant unused plugin scopes. App-defined commands (e.g.
    /// `app_ready`) are reachable via `core:default`; we don't enumerate them
    /// here (Tauri gates *plugin* commands through this ACL). The control we
    /// assert is that the `opener` plugin — registered in `run()` so U6 can
    /// use it — has NONE of its command scopes granted yet, and that no other
    /// later-unit plugin scope has leaked in.
    #[test]
    fn capability_allowlist_excludes_unused_plugin_scopes() {
        let caps = include_str!("../capabilities/default.json");
        let caps: serde_json::Value =
            serde_json::from_str(caps).expect("capabilities/default.json is valid JSON");

        let perms: Vec<String> = caps["permissions"]
            .as_array()
            .expect("capabilities has a permissions array")
            .iter()
            .filter_map(|p| p.as_str().map(str::to_string))
            .collect();

        // U1 grants exactly the core defaults — nothing else.
        assert!(
            perms.iter().any(|p| p == "core:default"),
            "expected `core:default` to be granted"
        );

        // The opener plugin is registered but its scopes are NOT granted in U1;
        // opening the dashboard URL is a U6 capability. Any `opener:*` grant
        // here is a trust-boundary regression.
        for p in &perms {
            assert!(
                !p.starts_with("opener:"),
                "capability allowlist leaked an opener scope before U6: `{p}`"
            );
        }

        // No later-unit plugin scopes should be present yet (defense in depth).
        for forbidden_prefix in ["shell:", "fs:", "http:", "dialog:", "process:"] {
            for p in &perms {
                assert!(
                    !p.starts_with(forbidden_prefix),
                    "capability allowlist leaked an unused plugin scope: `{p}`"
                );
            }
        }
    }
}
