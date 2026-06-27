# Contributing to Botbox

Thanks for helping build Botbox — the companion desktop SSH client for AI Power
Guild bots. This guide covers dev setup, the project layout, and how to run the
tests.

## Dev setup

Botbox is a [Tauri 2](https://v2.tauri.app/) app: a Rust backend (`src-tauri/`)
and a vanilla-TypeScript / Vite frontend (`src/`). You need:

- **Rust** stable (MSRV **1.77.2**) via [rustup](https://rustup.rs/). The SSH stack
  (`russh`) and the Keychain backend (`security-framework`) link against macOS
  system frameworks, so build on macOS.
- **pnpm** + **Node.js** — `npm i -g pnpm`.
- **Xcode Command Line Tools** — `xcode-select --install`.

```bash
pnpm install        # frontend deps + Tauri CLI
pnpm tauri dev      # run the app
pnpm tauri build    # build the macOS bundle (.app + .dmg)
```

## Project layout

The codebase is organized around the implementation units U1–U8 (see
`docs/plans/2026-06-26-001-feat-botbox-ssh-client-plan.md` for the full plan).

```
botbox/
├── src-tauri/                     # Rust backend
│   ├── Cargo.toml
│   ├── tauri.conf.json            # bundle config + strict CSP (KTD8)
│   ├── capabilities/default.json  # permission allowlist — the webview↔backend boundary
│   └── src/
│       ├── main.rs / lib.rs       # app entry + invoke_handler wiring (U1)
│       ├── commands.rs            # the Tauri command surface (U2–U6)
│       ├── keychain.rs            # Keychain storage via security-framework (U2)
│       ├── store.rs               # bot inventory persistence, 0600 (U3)
│       └── ssh/
│           ├── signer.rs          # Signer trait (+ algorithm id) + ed25519 impl + export (U2)
│           ├── connection.rs      # connection actor + driver + loss detection (U4)
│           ├── pipeline.rs        # staged connect + per-stage error classes (U4)
│           ├── known_hosts.rs     # TOFU host-key store, 0600 (U4)
│           ├── channels.rs        # host + attach PTY read/write/resize (U5)
│           └── forward.rs         # loopback port-forward + eager probe (U6)
├── src/                           # frontend (vanilla TS + xterm.js)
│   ├── main.ts                    # bootstrap + command/event wiring
│   ├── state.ts                   # connection/terminal state model (KTD9)
│   ├── connection.ts              # connect flow + event dispatch (U4)
│   ├── terminals.ts / terminal.ts # xterm.js wiring (U5)
│   ├── bots.ts                    # bot list / add-edit UI (U3)
│   ├── render.ts                  # error-class + provisioning UI (U7)
│   └── styles.css                 # AI Power Guild design language
└── docs/
    └── extending-to-other-bots.md # config-driven path to non-Hermes bots (R15)
```

The webview↔backend trust boundary (KTD8 / R18) is enforced in
`src-tauri/capabilities/default.json`: only `core:default` plus the loopback-scoped
`opener:allow-open-url` are granted. App-defined commands reach the webview via
`core:default` and are listed in `lib.rs`'s `invoke_handler`; **plugin** commands
need an explicit scope entry in the capability file. A change that adds a plugin
scope must keep the capability smoke test (`lib.rs` tests) green.

## Running the tests

The backend tests are **hermetic** — they use in-memory fakes (a memory key store,
a memory bot store, an in-process SSH server) so they never touch the real Keychain
or the network:

```bash
cd src-tauri && cargo test          # backend unit + integration tests
pnpm test                           # frontend tests (vitest)
```

A few backend tests are `#[ignore]`-gated because they touch the **real login
Keychain** (and may pop an interactive OS prompt); run those manually:

```bash
cd src-tauri && cargo test -- --ignored real_keychain
```

### The gated real-bot test

`src-tauri/tests/real_bot.rs` drives the *real* connect pipeline against a live
Hermes bot — the U5/U6 "done" gate. It is `#[ignore]` and only runs when you point
it at a bot:

```bash
cd src-tauri
BOTBOX_REAL_BOT_IP=<bot-ip> cargo test --test real_bot -- --ignored --nocapture
```

Optional overrides: `BOTBOX_REAL_BOT_USER` (default `hermes`),
`BOTBOX_REAL_BOT_ATTACH` (default `tmux attach -t hermes`). The
`deploy/hetzner/` scripts can stand up a disposable Hermes bot to test against.

## Code style

Match the existing conventions:

- **Rust:** `cargo fmt` and `cargo clippy` clean; module-level `//!` docs that
  reference the unit/KTD they implement; errors carry a stage/kind so the frontend
  can render the right class. Never return or log private key material.
- **TypeScript:** the frontend is dependency-light vanilla TS — no framework. Keep
  the state model in `state.ts` as the single source of truth for connection state.
- Keep the **security boundary** tight: don't add a capability scope or relax the
  CSP without a clear reason and a passing smoke test.

## Pull requests

Keep PRs scoped to one unit/concern, include or update tests, and confirm both
`cargo test` and `pnpm test` are green before opening. Botbox is MIT-licensed; by
contributing you agree your contributions are licensed under the same terms.
