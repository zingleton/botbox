//! Real-bot integration test — the U5 "done" gate (and a U6 dashboard probe).
//!
//! These tests are `#[ignore]` and only meaningful against a live Hermes bot.
//! They drive the *real* connection pipeline (KTD6) with the *real* Keychain
//! signer (U2) against a real SSH server, open the host + Hermes-attach PTYs
//! (U5), and forward the dashboard port (U6) — the paths the hermetic
//! in-process-server tests can only approximate.
//!
//! Run manually:
//!   BOTBOX_REAL_BOT_IP=178.156.179.126 \
//!     cargo test --test real_bot -- --ignored --nocapture
//!
//! Optional env:
//!   BOTBOX_REAL_BOT_USER     SSH user (default: hermes)
//!   BOTBOX_REAL_BOT_ATTACH   attach command (default: tmux attach -t hermes)
//!   BOTBOX_REAL_BOT_DASH     dashboard remote port (default: 9119)
//!
//! `print_app_public_key` prints the Keychain public key so it can be
//! provisioned onto the bot's authorized_keys before the connect test runs.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use botbox_lib::keychain::default_key_store;
use botbox_lib::ssh::channels::{open_attach_pty, open_host_pty, PtySink, PtySinkError, PtySize};
use botbox_lib::ssh::connection::{connect, ConnectConfig, HostKeyPrompt, TrustResponse};
use botbox_lib::ssh::known_hosts::{JsonKnownHostsStore, KnownHosts};
use botbox_lib::ssh::signer::{Ed25519Signer, Signer};

/// A `PtySink` that accumulates every byte for assertion.
struct CollectingSink(Arc<Mutex<Vec<u8>>>);

impl PtySink for CollectingSink {
    fn send(&self, bytes: &[u8]) -> Result<(), PtySinkError> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }
}

fn collected(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&buf.lock().unwrap()).to_string()
}

/// Print the app's Keychain public key — provision this on the bot before the
/// connect test (`echo '<key>' >> ~hermes/.ssh/authorized_keys`).
#[test]
#[ignore = "prints Keychain public key; run manually"]
fn print_app_public_key() {
    let signer = Ed25519Signer::new(default_key_store());
    signer.generate().expect("generate keypair");
    println!(
        "APP_PUBLIC_KEY={}",
        signer.public_openssh().expect("public key")
    );
}

/// The U5 gate: authenticate to the real bot with the Keychain key as the
/// configured user, open the host shell (assert we are that user), then open
/// the Hermes-attach PTY and assert we reach the live Hermes session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a live bot; set BOTBOX_REAL_BOT_IP"]
async fn real_bot_host_shell_and_hermes_attach() {
    let Ok(ip) = std::env::var("BOTBOX_REAL_BOT_IP") else {
        eprintln!("BOTBOX_REAL_BOT_IP unset — skipping");
        return;
    };
    let user = std::env::var("BOTBOX_REAL_BOT_USER").unwrap_or_else(|_| "hermes".into());
    let attach =
        std::env::var("BOTBOX_REAL_BOT_ATTACH").unwrap_or_else(|_| "tmux attach -t hermes".into());
    let addr = format!("{ip}:22");

    // Real Keychain signer (the key provisioned via print_app_public_key).
    let signer: Arc<dyn Signer> = Arc::new(Ed25519Signer::new(default_key_store()));

    // TOFU store in a temp dir; auto-Trust the first prompt.
    let tmp = tempfile::tempdir().unwrap();
    let decider = Arc::new(KnownHosts::new(JsonKnownHostsStore::new(tmp.path())));
    let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<HostKeyPrompt>(4);
    tokio::spawn(async move {
        while let Some(p) = prompt_rx.recv().await {
            eprintln!("[trust] {} fingerprint {}", p.host, p.fingerprint);
            let _ = p.responder.send(TrustResponse::Trust);
        }
    });

    let cfg = ConnectConfig::for_user(&user);
    let conn = connect(&addr, &cfg, signer, decider, prompt_tx)
        .await
        .expect("connect to real bot");
    let handle = conn.handle();

    // ── Host shell: prove auth + identity ───────────────────────────────────
    let host_buf = Arc::new(Mutex::new(Vec::new()));
    let host_pty = open_host_pty(
        handle.clone(),
        PtySize::new(120, 40),
        Arc::new(CollectingSink(host_buf.clone())),
    )
    .await
    .expect("open host PTY");

    host_pty.write(b"whoami\n").await.expect("write whoami");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let host_out = collected(&host_buf);
    eprintln!("─── host shell ───\n{host_out}\n──────────────────");
    assert!(
        host_out.contains(&user),
        "host shell should report the login user `{user}`; got:\n{host_out}"
    );

    // ── Hermes attach: prove we reach the live agent session ────────────────
    let attach_buf = Arc::new(Mutex::new(Vec::new()));
    let attach_pty = open_attach_pty(
        handle.clone(),
        &attach,
        PtySize::new(120, 40),
        Arc::new(CollectingSink(attach_buf.clone())),
    )
    .await
    .expect("open attach PTY");

    // Give tmux a moment to attach and Hermes to repaint.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let attach_out = collected(&attach_buf);
    eprintln!("─── hermes attach ───\n{attach_out}\n─────────────────────");
    // The Hermes TUI renders its model line / prompt; any of these markers
    // confirms we reached the live session rather than an empty shell.
    let markers = ["hermes", "Hermes", "claude", "❯", "tmux"];
    assert!(
        markers.iter().any(|m| attach_out.contains(m)),
        "attach output should show the live Hermes session; got:\n{attach_out}"
    );

    host_pty.close().await;
    attach_pty.close().await;
    conn.close().await;
}
