---
date: 2026-06-28
topic: guild-ssh-cert-auth
---

# Guild-Brokered SSH Access for Botbox — Requirements

## Summary

Make the AI Power Guild an **SSH certificate authority** so a member can reach
their Hermes bot from any device they are signed into, with no manual key
handling. A botbox client obtains a short-lived, Guild-signed certificate using
its existing login and connects to the bot over public SSH; each bot is
provisioned once to trust the Guild's CA and never changes as devices come and
go. This design is the contract the Guild's future bot-provisioning feature
implements, and it is built so a later no-public-SSH overlay can slot in without
rework.

---

## Problem Frame

A Hermes bot runs on a public-IP Hetzner VPS reachable only by SSH key. Today the
bot is born with exactly one public key baked into cloud-init
(`deploy/hetzner/cloud-init.yaml` substitutes a single `__SSH_PUBLIC_KEY__` into
`hermes`'s `authorized_keys`). There is no path to authorize a *second* device
after boot without hand-editing `authorized_keys` over an already-working SSH
session — which is exactly the case that fails: one member, several devices (a
Mac, an iPhone), added over time and often from a device that has never reached
the bot before.

The intuitive fixes make every bot into mutable per-device state. Pushing each
new key onto each bot needs a Guild-held admin key to every box; having bots poll
the Guild for an allow-list adds latency and an on-bot agent. Both scale as
devices × bots and must be kept in sync forever. Meanwhile the bot's port 22 is
continuously scanned — pubkey-only auth keeps scanners out, but the member still
wants device access controlled centrally by the system that already authenticates
them and creates their bots. The missing piece is a trust path from "signed into
the Guild" to "allowed onto my bot" that scales to many devices without touching
the bot each time.

---

## Key Decisions

- **The Guild is an SSH certificate authority, not a key distributor.** The bot
  trusts the Guild's CA; the Guild vouches for a device by signing its key into a
  certificate. This replaces per-bot key fan-out with sign-at-issuance, so a bot
  is immutable after provisioning and adding a device never touches the bot. This
  is the load-bearing reframe: the original problem ("get each client's public
  key onto the bot") dissolves.

- **Public SSH is retained for v1; the overlay is the next hardening phase.**
  Keeping direct SSH solves multi-device access now without a WireGuard relay and
  its iOS Packet-Tunnel cost. The cert layer is kept transport-independent so a
  later overlay can move the transport beneath unchanged auth (see R18).

- **Login is enrollment; the roster is the revocation surface.** Any device
  signed into the Guild can obtain certificates for the member's bots
  immediately — the existing session/bearer token is the device's
  authentication. The Guild records each device's key so a single lost device can
  be revoked without disturbing the others. Chosen over per-device approval for
  zero-friction add; chosen over no-roster for granular revoke.

- **Certificates are short-lived and re-requested per session; the Guild must be
  reachable at connect time.** Revocation latency is bounded by TTL rather than a
  revocation list in v1. If the Guild is unreachable and the client holds no
  unexpired certificate, the connection fails — accepted, since the Guild is
  needed to do almost anything with a bot anyway.

- **The Guild also signs the bot's host key.** A Guild-signed *host* certificate,
  trusted by botbox, removes the trust-on-first-use prompt the client shows today
  — so a brand-new device connects cleanly the first time. This supersedes the
  TOFU-prompt path in the prior brainstorm's R16.

- **This doc is the provisioning contract, not the provisioning build.** The Guild
  has no bot creation/management today. The bot-side and Guild-side requirements
  here define what the Guild's next-step provisioning feature must implement;
  building Hetzner provisioning is out of scope.

## Actors

- A1. **Member (operator)** — owns the bots; manages their device roster in the
  Guild web app. One owner per bot; no sharing.
- A2. **botbox client (device)** — holds a local ed25519 key, fetches a
  certificate, connects. Several per member (macOS desktop, iOS phone, second
  laptop).
- A3. **AI Power Guild** — the certificate authority plus the owner/bot registry
  and device roster. Authenticates devices via the existing Supabase session
  token; signs user and host certificates.
- A4. **Hermes bot** — trusts the Guild CA, admits any valid Guild-signed
  certificate for its owner's principal, and stores no per-client keys.

## Key Flows

```mermaid
sequenceDiagram
    participant D as botbox client (device)
    participant G as AI Power Guild (CA)
    participant B as Hermes bot
    Note over B: Provisioned once — trusts Guild CA,<br/>carries Guild-signed host cert
    D->>G: authenticated request: my public key + which bot (bearer token)
    G->>G: verify member owns bot; device on roster
    G-->>D: short-lived user certificate (principal = owner)
    D->>B: SSH publickey auth — key + certificate
    B->>B: validate cert vs trusted Guild CA + principal
    B-->>D: session opens (host cert → no trust-on-first-use prompt)
```

- F1. **Provision trust (the contract).** When the Guild creates a bot (future
  work), it injects the Guild CA public key and restricts it to the owner's
  principal, and installs a Guild-signed host certificate. The bot is now
  reachable by any of the owner's current and future devices.
- F2. **Enroll a device.** A botbox client signs into the Guild; on its first
  certificate request its public key is recorded on the owner's roster. No
  separate approval step.
- F3. **Connect.** The client requests a certificate (bearer token + its public
  key + target bot); the Guild verifies ownership and signs a short-lived
  certificate; the client presents key + certificate and the bot admits it.
- F4. **Revoke a device.** The member removes a device from the roster; the Guild
  stops issuing certificates for it; the device's last certificate lapses at TTL.

## Requirements

**Guild certificate authority**

- R1. The Guild holds an SSH user CA keypair; the private signing key is a
  server-side secret guarded like other Guild admin secrets and never leaves the
  Guild.
- R2. The Guild exposes an authenticated endpoint that, given a caller's existing
  Guild bearer token, a botbox client public key, and a target bot, returns a
  short-lived Guild-signed SSH user certificate.
- R3. Certificates are short-lived and re-requestable per session, and are scoped
  so a certificate authenticates only to bots the requesting member owns
  (principal-based scoping; exact principal structure deferred to planning).
- R4. The Guild signs only for bots the authenticated member owns; a request
  naming a bot the member does not own is refused without issuing a certificate.

**Bot trust configuration (the provisioning contract)**

- R5. Each bot is provisioned to trust the Guild CA public key for user
  authentication (e.g. sshd `TrustedUserCAKeys`), restricted to the principal the
  Guild issues for that bot's owner.
- R6. After provisioning, a bot needs no per-device key changes as the owner adds
  or removes devices, and stores no per-client public keys.
- R7. The bot keeps certificate/pubkey-only SSH (no passwords); public port 22
  stays reachable but admits only valid Guild-signed certificates.
- R8. The Guild signs the bot's SSH host key into a host certificate at
  provisioning, and botbox trusts the Guild host CA, so a new device's first
  connection shows no trust-on-first-use prompt.
- R9. R5–R8 define the contract the Guild's future bot-provisioning feature must
  implement; building that provisioning is out of scope here.

**Client certificate flow**

- R10. A botbox client keeps its existing locally-held ed25519 key
  (Keychain/in-memory) and obtains a certificate for that key from the Guild
  before connecting.
- R11. The client presents key + certificate during SSH publickey auth and, on
  certificate expiry, requests a fresh certificate rather than failing
  permanently.
- R12. Certificate acquisition uses the device's existing Guild session/bearer
  token as the device's authentication — no separate per-bot credential.
- R13. When the Guild is unreachable and the client holds no unexpired
  certificate, the connection attempt fails with an error distinct from a
  wrong-address, unreachable-bot, or auth-rejection failure.

**Device enrollment and revocation**

- R14. Any device signed into the Guild can obtain certificates for the owner's
  bots immediately; there is no separate per-device approval step.
- R15. The Guild records each enrolled device's public key as a roster entry that
  identifies the device, so devices are individually visible to the owner.
- R16. The owner can revoke a single device from the roster; a revoked device can
  no longer obtain new certificates, and other devices are unaffected.
- R17. Revocation takes effect within one certificate TTL — a revoked device's
  last certificate remains valid until it expires; instant global revocation (a
  published KRL) is deferred.

**Composition with later hardening**

- R18. SSH certificate auth, enrollment, and revocation are kept independent of
  the transport, so a later overlay / no-public-SSH phase can move the transport
  beneath them without changing the CA, the roster, or the cert flow.

## Acceptance Examples

- AE1. **Covers R5, R6, R14.** The member signs into the Guild on a brand-new iOS
  phone and connects to a bot provisioned weeks earlier; the bot's configuration
  is unchanged from provisioning and admits the phone.
- AE2. **Covers R4.** A member requests a certificate naming a bot they do not
  own; the Guild refuses and issues no certificate.
- AE3. **Covers R16, R17.** The member revokes a lost laptop from the roster; the
  laptop can no longer obtain a new certificate, its last certificate still works
  until it expires, and the member's other devices keep connecting.
- AE4. **Covers R8.** The first connection from a new device to a bot carrying a
  Guild-signed host certificate shows no trust-on-first-use prompt.
- AE5. **Covers R13.** The Guild is unreachable and the client holds no unexpired
  certificate; the connection fails with an error the operator can tell apart
  from a wrong IP or a rejected key.
- AE6. **Covers R11.** A client whose certificate has expired requests a fresh one
  on the next connect and succeeds, rather than reporting a permanent auth
  failure.

## Success Criteria

- Adding a device to a bot requires zero changes on the bot.
- No operator ever copies a public key into `authorized_keys` by hand.
- A revoked device cannot start a new session within one certificate TTL.
- A new device's first connection to an owned bot shows no trust-on-first-use
  prompt.

## Scope Boundaries

**Deferred for later**

- The no-public-internet overlay (a Guild-run WireGuard relay routing all
  connections through the Guild) — the planned hardening phase this design is
  built to accept (R18).
- Instant/global certificate revocation via a published KRL — v1 relies on short
  TTL plus roster revocation.
- Offline / cached-certificate connections when the Guild is unreachable — v1
  accepts that the connection fails.
- Implementation of the Guild's bot creation, management, and Hetzner
  provisioning — the next build, designed against this doc's contract (R9).

**Outside this product's identity**

- Multi-user or team sharing of a single bot — still one owner per bot.
- A general-purpose SSH certificate authority or device-management product — this
  is scoped to botbox ↔ Hermes-bot access.

## Dependencies / Assumptions

- botbox's `russh` client (pinned 0.54.5) supports OpenSSH client certificate auth
  via `authenticate_openssh_cert(user, Arc<PrivateKey>, Certificate)` — **verified
  in source**. That method signs with an **in-memory `PrivateKey`**; the external
  `Signer` path botbox uses today (`authenticate_publickey_with` →
  `FuturePublicKey`) only sends a plain public key and cannot carry a certificate.
  The v1 ed25519 signer already loads the Keychain private key into memory to sign
  (`Ed25519Signer::load_private` in `src-tauri/src/ssh/signer.rs`), so the cert path
  is feasible for v1 with no russh fork — load-bearing for R10/R11.
- The Guild will store each bot's address and owner and serve them to authorize
  and target a certificate request. This is net-new and part of the provisioning
  build that follows.
- The device's Supabase session/bearer token is an acceptable proxy for device
  authentication (R12, R14), reusing the existing per-device bounded-credential
  pattern (`app/api/account/git-access` in `aisupply`).
- Client keys stay ed25519 with the private key extractable into memory
  (Keychain-held). A future hardware-backed P-256 / Secure-Enclave signer **cannot**
  export a `PrivateKey`, so it cannot use russh 0.54.5's certificate path —
  reconciling certificates with a non-extractable key needs a newer or forked russh
  that signs certs through an external signer, or accepting that certificate auth
  implies a software-held key. This narrows the prior "hardware signer is a clean
  future swap" assumption (the `Signer` seam in `src-tauri/src/ssh/signer.rs`).
- The Guild can safeguard the CA private signing key (server-side secret or KMS).

## Outstanding Questions

**Resolve before planning**

- None blocking. The `russh` certificate-auth feasibility gate is resolved (see
  Dependencies): v1's ed25519 path can present a certificate via
  `authenticate_openssh_cert` because the Keychain key is loaded into memory to
  sign.

**Deferred to planning**

- How the certificate path threads through the `Signer` seam. The cert auth method
  needs an in-memory `Arc<PrivateKey>`, which the current `Signer` trait
  deliberately does not expose. Planning decides whether to add a narrow cert-auth
  method to the trait or pass the private key into the connection layer for the
  cert case only.
- Whether a newer/forked `russh` offers external-signer certificate auth, which is
  the prerequisite for ever pairing certificates with a Secure-Enclave signer.

- Exact certificate TTL, and whether the client fetches per connect or
  pre-fetches and refreshes.
- The principal structure that scopes a certificate to a member's bots (per-member
  vs per-bot principal) and how the bot's `TrustedUserCAKeys` principal
  restriction is expressed.
- Where the CA private key lives (KMS vs server secret) and its rotation
  procedure.
- Host-certificate renewal cadence (host certs also expire) and how botbox pins
  the Guild host CA.
- The roster data model and how revocation propagates (deny future issuance only
  in v1; KRL later).

## Sources / Research

- `docs/brainstorms/2026-06-26-botbox-ssh-client-requirements.md` — the client-app
  brainstorm this extends; it deferred the Guild integration (its R2) and used a
  TOFU host-key prompt (its R16) that R8 here replaces.
- `src-tauri/src/ssh/connection.rs` — the staged connect pipeline and the TOFU
  host-key handler that a Guild host cert (R8) makes unnecessary.
- `src-tauri/src/ssh/signer.rs` — the `Signer` seam the client key and certificate
  presentation build on.
- `deploy/hetzner/cloud-init.yaml` — today's single-key injection; R5/R8 change
  this to CA-trust plus a host cert when provisioning moves into the Guild.
- `aisupply`: `lib/auth/bearer.ts` (bearer-token validation) and
  `app/api/account/git-access/route.ts` (per-device bounded-credential issuance) —
  the existing patterns R2/R12/R14 reuse.
