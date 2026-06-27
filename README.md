# Botbox

**Botbox is the companion desktop SSH client for [AI Power Guild](https://aipowerguild.com) bots.**

It is a Tauri 2 macOS app with a Rust backend that holds a single embedded
[`russh`](https://github.com/Eugeny/russh) connection to a remote bot and
multiplexes three things over it at once:

- an interactive **host-shell** terminal,
- a **Hermes-attach** terminal (the live agent session), and
- a loopback **dashboard port-forward** (open in your browser).

Reaching a remote agent normally means hand-rolling SSH: generate a key, get it
onto the box, remember IPs, open a shell, attach to the running session, and set
up a port-forward before the dashboard is reachable. Botbox collapses that into a
few clicks — and because the SSH client is *embedded* (not a shell-out to system
`ssh`), the same key and the same codepath are designed to carry forward to other
platforms later.

Botbox shares the AI Power Guild design language with the Guild web app — they
are the same product family. **Hermes** is the supported bot type in v1; other
bot types are reachable by configuration, not by forking core logic (see
[Extending to other bots](docs/extending-to-other-bots.md)).

> **Status:** v1, macOS only. Bot inventory and key provisioning are **manual**
> (copy/paste). Deeper integration with the AI Power Guild web app — pulling bot
> IPs from the Guild's inventory and pushing your public key for provisioning — is
> the planned next step and is **not** built yet. See [Roadmap](#roadmap).

---

## Features

- **One embedded SSH connection, multiplexed** — host shell, Hermes attach, and
  the dashboard tunnel all ride a single `russh` connection (ed25519).
- **Keys live in the macOS Keychain** — an ed25519 keypair is generated on demand
  and the private key is stored as a Keychain item. You copy the *public* key to
  provision a bot; an explicit, opt-in **export** writes a `0600` OpenSSH private
  key if you ever need it elsewhere.
- **Trust-on-first-use host keys** — the host key is verified on first connect
  (you accept a fingerprint), persisted, and a later **mismatch** hard-stops with
  a distinct, recoverable warning.
- **Validate-before-swap connecting** — switching to a new bot only tears down the
  current connection *after* the new one authenticates.
- **Distinct error classes** — unreachable host, untrusted/changed host key, remote
  auth failure (unprovisioned key), local Keychain/signer failure, wrong dashboard
  port, and mid-session connection loss are each surfaced differently, with the
  right next action (e.g. auth failure shows your public key to paste onto the bot).
- **Loopback-only dashboard tunnel** — the dashboard port is forwarded to
  `127.0.0.1:<os-assigned-port>` (never `0.0.0.0`), with an eager wrong-port probe,
  and opened in your default browser.
- **Strict security boundary** — a strict CSP (no `unsafe-inline`/`unsafe-eval`) and
  a capability allowlist that grants only `core:default` plus a loopback-scoped
  browser-open permission.

---

## Install / build

Botbox is built with [Tauri 2](https://v2.tauri.app/). You need:

- **Rust** (stable; the project's MSRV is 1.77.2) — install via [rustup](https://rustup.rs/).
- **pnpm** and **Node.js** — `npm i -g pnpm`.
- **macOS** with Xcode Command Line Tools (`xcode-select --install`) for the
  system frameworks Tauri and the Keychain backend link against.

```bash
pnpm install          # frontend deps (xterm.js, fonts, Tauri CLI)

pnpm tauri dev        # run the app in development
pnpm tauri build      # produce an installable macOS bundle (.app + .dmg)
```

`pnpm tauri build` writes the bundle under
`src-tauri/target/release/bundle/` (a `Botbox.app` and a `.dmg`).

### Code-signing & the Keychain (distribution)

For local development the unsigned build works. **For distribution you must
code-sign the app**, and signing is also what gives the Keychain item a stable
identity:

- The private key is stored under the app's bundle identifier
  (`ai.aipowerguild.botbox`, in `src-tauri/tauri.conf.json`). For the Keychain item
  to reliably persist and be readable across launches under a **stable signed
  identity**, the distributed app should be signed with an Apple Developer ID and
  carry a `keychain-access-groups` entitlement matching the bundle id.
- An *unsigned* dev build still works, but its Keychain access is tied to the
  ad-hoc signature; a signed release build is the supported distribution path.
- Botbox does not ship signing credentials. To produce a signed, notarized DMG
  you supply your own Developer ID certificate and configure Tauri's macOS signing
  (`signingIdentity` / notarization). See the
  [Tauri macOS code-signing guide](https://v2.tauri.app/distribute/sign/macos/).

---

## How it works

### 1. Key flow (generate → copy → provision → connect)

1. **Generate** your ed25519 key in Botbox (one action; idempotent — if a key
   already exists it is reused). The private key goes into the Keychain; it is
   never shown.
2. **Copy** the public key from the always-available public-key surface.
3. **Provision** it on the bot — append it to the bot user's
   `~/.ssh/authorized_keys` (for a Guild Hermes bot the login user is `hermes`).
4. **Connect.** Botbox authenticates with the Keychain-held key.

If you ever need the private key elsewhere, use the explicit **export** action: it
writes an OpenSSH private key with `0600` permissions to a path you choose, behind
a confirmation that the key is leaving the Keychain.

### 2. Provisioning on auth failure

When a connection reaches the bot but the key is rejected (the key isn't yet in
`authorized_keys`), Botbox does **not** show a generic error. It renders the
**provisioning surface**: your public key inline with a copy action, so you can
paste it onto the bot and retry. This is distinct from a **local Keychain/signer
failure** (Keychain locked, OS prompt cancelled), which shows unlock guidance
instead — you are never sent to re-paste a key that was already correct.

### 3. Adding a bot

A bot is **name + host**, plus three fields that default to the live Guild Hermes
deploy:

| Field            | Default               | Meaning                                            |
| ---------------- | --------------------- | -------------------------------------------------- |
| `username`       | `hermes`              | SSH login user                                     |
| `attach_command` | `tmux attach -t hermes` | command run in the attach terminal                |
| `dashboard_port` | `9119`                | the dashboard port on the bot (forwarded to loopback) |

Leave any of the three blank and Botbox applies the default. Bots are persisted to
the app-data dir as JSON with `0600` permissions and survive relaunch. You can
add/edit/remove/select bots; switching while connected confirms before tearing the
active connection down.

### 4. Dashboard tunnel

On connect (after auth), Botbox eagerly probes the bot's dashboard port. If
something is listening, it binds a loopback listener on `127.0.0.1:0` (OS-assigned
port), forwards it to the bot's dashboard port over the SSH connection, reports the
local URL, and opens your browser there. If nothing is listening it surfaces
"nothing listening on port N" — distinct from an SSH failure — and leaves your
terminals usable. The tunnel is a child of the connection: it goes inactive on
disconnect, bot-switch, or connection loss.

---

## Security notes

- **Keychain key storage.** The ed25519 private key is a Keychain item scoped to
  the app bundle (`kSecAttrAccessibleWhenUnlockedThisDeviceOnly`). No command
  returns private key material except the explicit `export` path; nothing logs it.
- **Loopback-only forward.** The dashboard listener binds `127.0.0.1`, never
  `0.0.0.0`.
- **TOFU host keys.** First-contact host keys are verified by fingerprint and
  persisted; a changed key hard-stops and requires an explicit *remove saved key*
  before re-trust — Botbox never silently updates a host key.
- **Strict webview↔backend boundary.** A strict CSP (no `unsafe-inline`/
  `unsafe-eval`) plus a capability allowlist (`src-tauri/capabilities/default.json`)
  that grants only `core:default` and a **loopback-scoped** browser-open
  permission. The terminal renders untrusted remote bytes, so this boundary is a
  deliberate, tested control, not a default.

### Accepted v1 risks

These are accepted and documented for the single-operator v1; mitigations are on
the roadmap:

- **Loopback forward is reachable by same-user local processes.** Any process
  running as the same macOS user can reach the tunneled dashboard on the loopback
  port. Accepted for a single-operator desktop; a single-use access token on the
  forward is deferred.
- **Local data-dir tamper can defeat TOFU.** An attacker who can *write* the
  known-hosts store or the bot inventory could pre-trust a malicious host and
  defeat trust-on-first-use silently. Both files are created `0600` (owner-only) to
  raise the bar; HMAC-over-a-Keychain-key integrity protection is a documented
  follow-up.

---

## Roadmap

Botbox is built so these extend it rather than rewrite it:

- **AI Power Guild integration.** v1 is manual copy/paste. The planned next step is
  to pull bot IPs from the Guild's inventory and push your public key for
  provisioning **through the AI Power Guild web app**, so adding a bot and
  provisioning your key become one click instead of copy/paste.
- **More bot types** (e.g. OpenClaw) via per-bot config — already supported; see
  [Extending to other bots](docs/extending-to-other-bots.md).
- **Hardware-backed keys** behind the existing `Signer` trait (Secure Enclave
  P-256). This is a key-rotation event, not a transparent swap — see the
  extensibility doc.
- **Guild-distributed known host keys**, multiple concurrent connections,
  auto-reconnect with backoff, and the loopback access token / data-dir HMAC noted
  above.

Out of scope for the product: Botbox is not a general-purpose SSH manager or a full
terminal-emulator replacement, has no multi-user/team model, and does not provision
or manage the bots themselves — that is AI Power Guild's job.

---

## Contributing & license

See [CONTRIBUTING.md](CONTRIBUTING.md) for dev setup, the module map, and how to
run the tests. Botbox is open source under the [MIT license](LICENSE).
