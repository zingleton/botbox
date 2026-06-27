//! Tests for the bot inventory store (U3; R4/R5/R6 coverage).
//!
//! Logic tests run over the in-memory [`MemoryBotStore`] via the storage-trait
//! seam, so they never touch the real Tauri app-data dir. The persistence +
//! `0600` tests use a `tempfile::TempDir` and assert behaviour against the real
//! [`JsonBotStore`] file writer, including a re-instantiated store to simulate a
//! relaunch.

use super::*;

fn input(name: &str, host: &str) -> BotInput {
    BotInput {
        name: name.to_string(),
        host: host.to_string(),
        attach_command: None,
        dashboard_port: None,
    }
}

// ── Defaults (R6) ──────────────────────────────────────────────────────────

#[test]
fn add_applies_canonical_hermes_defaults_when_fields_blank() {
    let inv = Inventory::new(MemoryBotStore::new());
    let bot = inv.add(input("Hermes-A", "10.0.0.5")).unwrap();

    assert_eq!(bot.attach_command, "tmux attach -t hermes");
    assert_eq!(bot.dashboard_port, 9119);
    // The constants are the single source of truth.
    assert_eq!(bot.attach_command, DEFAULT_ATTACH_COMMAND);
    assert_eq!(bot.dashboard_port, DEFAULT_DASHBOARD_PORT);
}

#[test]
fn add_treats_whitespace_only_attach_command_as_blank() {
    let inv = Inventory::new(MemoryBotStore::new());
    let bot = inv
        .add(BotInput {
            name: "B".into(),
            host: "h".into(),
            attach_command: Some("   ".into()),
            dashboard_port: Some(0),
        })
        .unwrap();
    assert_eq!(bot.attach_command, DEFAULT_ATTACH_COMMAND);
    assert_eq!(bot.dashboard_port, DEFAULT_DASHBOARD_PORT);
}

#[test]
fn add_keeps_explicit_overrides() {
    let inv = Inventory::new(MemoryBotStore::new());
    let bot = inv
        .add(BotInput {
            name: "B".into(),
            host: "h".into(),
            attach_command: Some("docker exec -it hermes bash".into()),
            dashboard_port: Some(8443),
        })
        .unwrap();
    assert_eq!(bot.attach_command, "docker exec -it hermes bash");
    assert_eq!(bot.dashboard_port, 8443);
}

#[test]
fn add_requires_name_and_host() {
    let inv = Inventory::new(MemoryBotStore::new());
    assert!(matches!(
        inv.add(input("", "h")),
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        inv.add(input("n", "  ")),
        Err(StoreError::Invalid(_))
    ));
}

// ── Edit / remove target precisely (R4) ────────────────────────────────────

#[test]
fn update_changes_only_the_targeted_bot() {
    let inv = Inventory::new(MemoryBotStore::new());
    let a = inv.add(input("A", "1.1.1.1")).unwrap();
    let b = inv.add(input("B", "2.2.2.2")).unwrap();

    let updated = inv
        .update(
            &b.id,
            BotInput {
                name: "B2".into(),
                host: "9.9.9.9".into(),
                attach_command: Some("tmux attach -t other".into()),
                dashboard_port: Some(1234),
            },
        )
        .unwrap();
    assert_eq!(updated.name, "B2");
    assert_eq!(updated.host, "9.9.9.9");

    let bots = inv.list().unwrap();
    let a_after = bots.iter().find(|x| x.id == a.id).unwrap();
    let b_after = bots.iter().find(|x| x.id == b.id).unwrap();
    // Sibling untouched.
    assert_eq!(a_after, &a);
    // Target updated.
    assert_eq!(b_after.name, "B2");
    assert_eq!(b_after.attach_command, "tmux attach -t other");
    assert_eq!(b_after.dashboard_port, 1234);
}

#[test]
fn update_errors_on_stale_id() {
    let inv = Inventory::new(MemoryBotStore::new());
    inv.add(input("A", "1.1.1.1")).unwrap();
    assert!(matches!(
        inv.update("does-not-exist", input("X", "y")),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn remove_deletes_only_the_targeted_bot() {
    let inv = Inventory::new(MemoryBotStore::new());
    let a = inv.add(input("A", "1.1.1.1")).unwrap();
    let b = inv.add(input("B", "2.2.2.2")).unwrap();
    let c = inv.add(input("C", "3.3.3.3")).unwrap();

    inv.remove(&b.id).unwrap();

    let ids: Vec<String> = inv.list().unwrap().into_iter().map(|x| x.id).collect();
    assert_eq!(ids, vec![a.id, c.id]);
}

#[test]
fn remove_errors_on_stale_id() {
    let inv = Inventory::new(MemoryBotStore::new());
    inv.add(input("A", "1.1.1.1")).unwrap();
    assert!(matches!(inv.remove("nope"), Err(StoreError::NotFound(_))));
}

// ── Selection pointer (R5; what U4 reads) ──────────────────────────────────

#[test]
fn select_records_and_clears_the_active_bot() {
    let inv = Inventory::new(MemoryBotStore::new());
    let a = inv.add(input("A", "1.1.1.1")).unwrap();

    inv.select(Some(&a.id)).unwrap();
    assert_eq!(inv.inventory().unwrap().selected_bot_id.as_deref(), Some(a.id.as_str()));

    inv.select(None).unwrap();
    assert_eq!(inv.inventory().unwrap().selected_bot_id, None);
}

#[test]
fn select_rejects_unknown_id() {
    let inv = Inventory::new(MemoryBotStore::new());
    assert!(matches!(
        inv.select(Some("ghost")),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn removing_the_selected_bot_clears_the_selection() {
    let inv = Inventory::new(MemoryBotStore::new());
    let a = inv.add(input("A", "1.1.1.1")).unwrap();
    inv.select(Some(&a.id)).unwrap();
    inv.remove(&a.id).unwrap();
    assert_eq!(inv.inventory().unwrap().selected_bot_id, None);
}

// ── Real file persistence + 0600 (R5; KTD10) ───────────────────────────────

#[test]
fn persists_to_disk_and_reloads_after_simulated_restart() {
    let dir = tempfile::tempdir().unwrap();

    // First "launch": add a bot.
    let added = {
        let inv = Inventory::new(JsonBotStore::new(dir.path()));
        let bot = inv.add(input("Hermes-A", "203.0.113.7")).unwrap();
        inv.select(Some(&bot.id)).unwrap();
        bot
    };

    // Simulated relaunch: a brand-new store over the same path must see it.
    let inv2 = Inventory::new(JsonBotStore::new(dir.path()));
    let reloaded = inv2.inventory().unwrap();
    assert_eq!(reloaded.bots.len(), 1);
    assert_eq!(reloaded.bots[0], added);
    assert_eq!(reloaded.selected_bot_id.as_deref(), Some(added.id.as_str()));
}

#[test]
fn missing_store_reads_as_empty_inventory() {
    let dir = tempfile::tempdir().unwrap();
    let inv = Inventory::new(JsonBotStore::new(dir.path()));
    // No file written yet.
    assert!(inv.list().unwrap().is_empty());
    assert_eq!(inv.inventory().unwrap().selected_bot_id, None);
}

#[cfg(unix)]
#[test]
fn store_file_is_created_0600() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let store = JsonBotStore::new(dir.path());
    let inv = Inventory::new(JsonBotStore::new(dir.path()));
    inv.add(input("A", "1.1.1.1")).unwrap();

    let mode = std::fs::metadata(store.path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "bot store must be owner-only (KTD10)");
}

#[cfg(unix)]
#[test]
fn save_tightens_preexisting_loose_perms() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let store = JsonBotStore::new(dir.path());

    // Pre-create the file world-readable.
    std::fs::write(store.path(), b"{}").unwrap();
    std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o644)).unwrap();

    Inventory::new(JsonBotStore::new(dir.path()))
        .add(input("A", "1.1.1.1"))
        .unwrap();

    let mode = std::fs::metadata(store.path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

// ── JSON-encoding correctness: surprising characters round-trip (R4) ────────

#[test]
fn surprising_characters_round_trip_without_corrupting_the_store() {
    let dir = tempfile::tempdir().unwrap();

    let gnarly_name = "He\"llo 🤖\n; rm -rf /\t世界 'quote' \\back/slash";
    let gnarly_host = "2001:db8::1%eth0\nspoof";
    let gnarly_attach = "tmux attach -t \"weird; name\" && echo \u{1F4A9}";

    let saved = {
        let inv = Inventory::new(JsonBotStore::new(dir.path()));
        inv.add(BotInput {
            name: gnarly_name.into(),
            host: gnarly_host.into(),
            attach_command: Some(gnarly_attach.into()),
            dashboard_port: Some(65535),
        })
        .unwrap()
    };

    // Reload from a fresh store: JSON must have escaped/round-tripped intact.
    let reloaded = Inventory::new(JsonBotStore::new(dir.path()))
        .list()
        .unwrap();
    assert_eq!(reloaded.len(), 1);
    let b = &reloaded[0];
    assert_eq!(b.name, gnarly_name);
    assert_eq!(b.host, gnarly_host);
    assert_eq!(b.attach_command, gnarly_attach);
    assert_eq!(b.dashboard_port, 65535);
    assert_eq!(b, &saved);

    // And a second bot added afterward proves the file wasn't corrupted.
    let inv = Inventory::new(JsonBotStore::new(dir.path()));
    inv.add(input("plain", "127.0.0.1")).unwrap();
    assert_eq!(inv.list().unwrap().len(), 2);
}
