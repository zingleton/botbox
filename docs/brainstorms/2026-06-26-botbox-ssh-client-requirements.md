---
date: 2026-06-26
topic: botbox-ssh-client
---

# Botbox — Requirements

## Summary

Botbox is an open-source Tauri app that makes it easy to reach a remote Hermes
agent over SSH from any of your computers and phones. It generates and holds an
SSH key, saves your bots by name and IP, opens an in-app host shell and a
Hermes-session terminal, and tunnels the Hermes web dashboard to localhost so you
can open it in a browser. v1 targets macOS desktop and one operator managing
several bots.

## Problem Frame

A Hermes agent runs on a remote host and exposes a terminal and a web dashboard.
Reaching it today means hand-rolling SSH: generating a key, getting it onto the
box, remembering IPs, opening a shell, attaching to the running agent session,
and setting up a port-forward before the dashboard is reachable. That is a chore
to do once and worse to repeat from a second laptop or a phone, where a usable
SSH client and `ssh` binary may not even exist. Botbox collapses that setup into
a few clicks and makes the same bots reachable from every device the operator owns.

## Key Decisions

- **Embedded SSH client in Rust, not system `ssh`.** Botbox speaks SSH itself
  over one connection per bot and multiplexes both terminals and the dashboard
  tunnel as channels on it. This is the only transport that uses a
  Keychain-held key directly, stays identical across macOS/Windows/iOS, and
  survives onto iOS — which has no `ssh` binary to shell out to.
- **Private key lives in the macOS Keychain.** Botbox generates a dedicated key
  and stores the private half in the Keychain (Secure Enclave where available).
  The key is Botbox-only by design; the operator provisions the public half onto
  bots manually. A consequence the operator accepts: this key is not usable from
  their normal terminal `ssh` without an explicit export.
- **AI Power Guild integration is manual copy/paste for v1.** Botbox shows the
  public key to copy and lets the operator type a bot's name and IP. No calls to
  the `aisupply` app yet — `aisupply` has no endpoint today for registering a
  public key or returning a bot IP, so that integration is a later milestone on
  both sides.
- **The Hermes terminal attaches to a running session.** Hermes runs as a
  long-lived process on the box; the Hermes terminal runs a fixed attach command
  (e.g. a `tmux` attach or `docker exec`) over SSH. The attach command is a
  per-bot config field, not hardcoded, so other bot types (OpenClaw, etc.) can be
  supported later by changing the command.
- **macOS first, then Windows.** The first coding run and build target macOS for
  tooling reliability. A Windows build follows later; iOS is a further target the
  embedded-SSH choice keeps open.

## Actors

- A1. **Operator** — the human running Botbox. Generates the key, saves bots,
  opens terminals and the dashboard. Single-operator app; no multi-user or
  sharing model in v1.
- A2. **Hermes bot** — the remote host running the Hermes agent, an SSH server,
  the long-lived Hermes session, and the web dashboard on some port.
- A3. **AI Power Guild (`aisupply`)** — the system that provisions skills and API
  keys onto the Hermes bot. In v1 Botbox does not call it; it is the manual
  source of bot IPs and the manual destination for the public key.

## Key Flows

- F1. **First-run key setup.** Operator opens Botbox → generates an SSH key →
  Botbox stores the private key in the Keychain and shows the public key →
  operator copies it and provisions it onto the bot(s) out of band.
- F2. **Add and select a bot.** Operator enters a name and IP, saves it to the
  bot list → selects a saved bot to act on. The list persists across launches.
- F3. **Connect and open terminals.** Operator connects to the selected bot over
  the embedded SSH client → Botbox opens a host-shell terminal and a
  Hermes-attach terminal in the UI, both over the one connection.
- F4. **Tunnel and open the dashboard.** Operator starts the dashboard tunnel →
  Botbox forwards the bot's dashboard port to a localhost port → Botbox opens the
  default browser at that local URL.

## Requirements

**Key management**

- R1. Botbox generates an SSH keypair on demand and stores the private key in the
  macOS Keychain, using the Secure Enclave where the platform supports it.
- R2. Botbox displays the public key in a form the operator can copy in one
  action, and provides a path to send it to AI Power Guild later (manual copy for
  v1).
- R3. Botbox does not require the operator to handle the private key file
  directly during normal use, but provides an explicit export action for an
  operator who wants the private key on disk (e.g. to reuse it from system
  `ssh` or move it to another machine).

- R16. Verify the host key on first connection (trust-on-first-use), persist
  accepted host keys, and hard-stop with a distinct warning if a known host's
  key later changes.

**Bot inventory**

- R4. Botbox lets the operator add a bot as a name plus IP address, save it, and
  edit or remove it later.
- R5. Saved bots persist across app launches and are listed for selection.
- R6. Each bot carries per-bot configuration for the Hermes attach command and
  the dashboard remote port, each with a sensible default.

**Connection and terminals**

- R7. Botbox connects to a selected bot over its own embedded SSH client using the
  Keychain-held key, authenticating once per bot connection.
- R8. Botbox shows an interactive host-shell terminal for the bot inside the UI.
- R9. Botbox shows a second interactive terminal that runs the bot's configured
  attach command to reach the live Hermes session.
- R10. Both terminals operate over the single SSH connection to the bot.
- R11. Botbox surfaces connection and authentication failures clearly enough that
  the operator can tell a wrong IP, an unprovisioned key, and an unreachable host
  apart.

**Dashboard tunnel**

- R12. Botbox forwards the bot's configured dashboard port to a localhost port and
  reports the resulting local URL.
- R13. Botbox opens the operator's default browser at the tunneled local URL.

**Project shape**

- R14. Botbox is built with Tauri and structured to produce installable builds for
  macOS (v1), with Windows and iOS as later targets.
- R15. The project is open source with documented code and an explicit path to
  extend it to other bot types (e.g. OpenClaw) by configuring connection and
  attach behavior rather than forking core logic.

## Acceptance Examples

- AE1. **Covers R1, R3.** When the operator generates a key, the private key is
  written to the Keychain and never shown or written to a plaintext file by
  Botbox in the normal flow.
- AE2. **Covers R9, R6.** When the operator opens the Hermes terminal on a bot
  whose attach command is the default, Botbox runs that default command; when the
  operator has overridden the attach command for that bot, Botbox runs the
  override.
- AE3. **Covers R11.** When the operator connects to a bot whose public key has
  not been provisioned, Botbox reports an authentication failure distinct from an
  unreachable-host error.
- AE4. **Covers R12, R13.** When the operator starts the dashboard tunnel, Botbox
  forwards the configured remote port to a free localhost port and opens the
  browser at that local URL; if the remote port is wrong, the browser open
  surfaces the failed connection rather than failing silently.

## Scope Boundaries

**Deferred for later**

- AI Power Guild API integration: pulling bot IPs from `aisupply` and pushing the
  public key to it for provisioning. v1 is manual copy/paste.
- Windows and iOS builds. The architecture keeps them open; v1 ships macOS.
- Multiple simultaneous dashboard tunnels or multiple concurrent bot connections,
  if v1 finds one-at-a-time sufficient.

**Outside this product's identity**

- Botbox is not a general-purpose SSH manager or a replacement for a full
  terminal emulator; it optimizes the Hermes-bot path, not arbitrary SSH
  administration.
- No multi-user, team-sharing, or fleet-orchestration model. One operator, their
  own bots.
- Botbox does not provision or manage the bots themselves (that is AI Power
  Guild's job); it connects to bots that already exist.

## Dependencies / Assumptions

- The embedded Rust SSH library supports interactive PTY channels and local
  port-forward channels over a single connection. This is load-bearing for R7,
  R10, and R12.
- The operator can provision the public key onto each bot out of band (paste into
  `authorized_keys` or via AI Power Guild manually).
- The Hermes session is reachable by a stable attach command, and the dashboard
  listens on a knowable per-bot port.
- macOS Keychain / Secure Enclave is available on the operator's first-target
  machine.

## Outstanding Questions

**Resolve before planning**

- None blocking. The transport, key storage, integration depth, and platform
  order are decided above.

**Deferred to planning / fill from a real bot**

- The default Hermes attach command (exact `tmux`/`docker exec` form) and the
  default dashboard remote port — fill these from a real test bot.
- Whether v1 allows more than one bot connected at once, or strictly one active
  connection at a time.
- Choice of embedded SSH Rust library and terminal-rendering component — a
  planning/architecture decision constrained by R7 and R10.

## Sources / Research

- `botbox.md` — original user goal and feature list (the spec this doc refines).
- `aisupply/` (AI Power Guild app) — Next.js app; has `app/api/account/git-access`
  and `app/api/account/memory-access` routes but no SSH-public-key or bot-IP
  endpoint today. Confirms the Guild integration is net-new on both sides.
