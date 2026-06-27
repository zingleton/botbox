# Extending Botbox to other bot types

Botbox ships with **Hermes** as the supported bot type, but it is **not**
Hermes-specific. Supporting another bot type (OpenClaw, or anything reachable over
SSH that runs an attachable session and serves a local dashboard) is a matter of
**configuration per bot**, not a fork of the connection core. This is requirement
**R15**.

## Why it's config, not a fork

The connection pipeline is bot-agnostic. When you connect, Botbox runs the same
staged pipeline for every bot:

1. TCP connect to `host`
2. verify the host key (trust-on-first-use)
3. authenticate with your Keychain-held ed25519 key as `username`
4. open the **host-shell** PTY and the **attach** PTY (running `attach_command`)
5. probe + forward `dashboard_port` to a loopback port

Nothing in steps 1–5 knows or cares that the bot is "Hermes". The only
Hermes-specific facts are three per-bot fields with defaults. Point them at a
different bot's user, attach command, and dashboard port and the same codepath
reaches that bot.

## The three per-bot fields

Each saved bot carries these, alongside its `name` and `host`. They are defined in
`src-tauri/src/store.rs` and resolved when you add/edit a bot; leaving one blank
applies the Hermes default.

| Field            | Hermes default          | What a non-Hermes bot needs                                   |
| ---------------- | ----------------------- | ------------------------------------------------------------- |
| `username`       | `hermes`                | the SSH **login user** the bot's agent runs under             |
| `attach_command` | `tmux attach -t hermes` | the command that drops you into the bot's **live session**    |
| `dashboard_port` | `9119`                  | the **port the bot's dashboard listens on** (on the bot's loopback) |

These defaults come from the live AI Power Guild Hermes deploy in
`deploy/hetzner/` (the Hermes agent runs as the Unix user `hermes`, its session is
a tmux session named `hermes`, and its dashboard binds `127.0.0.1:9119`).

## Walk-through: adding a non-Hermes bot

Say you have an **OpenClaw** bot. Figure out three things about it:

1. **Login user** — which SSH user runs the agent? Suppose it's `openclaw`.
   → set `username` = `openclaw` (and put that user's `authorized_keys` in place
   when you provision your public key).

2. **Attach command** — how do you reach its live session once you're SSH'd in?
   - a tmux session named `claw`? → `tmux attach -t claw`
   - a screen session? → `screen -r claw`
   - a container? → `docker exec -it openclaw /bin/bash` (or the agent's own
     attach subcommand)
   - it just runs in the foreground on login? → a plain shell like `bash -l`
   → set `attach_command` to whichever matches.

3. **Dashboard port** — what port does its dashboard serve on the bot's loopback?
   Suppose `8080`. → set `dashboard_port` = `8080`.

Add the bot in Botbox with `name`, `host`, and those three fields. Connect. The
host shell, the attach terminal (running your `attach_command`), and the dashboard
tunnel (forwarding your `dashboard_port`) all work the same way they do for Hermes.

If a bot has **no dashboard**, point `dashboard_port` at whatever it does serve, or
expect the eager probe to report "nothing listening on port N" — the host and
attach terminals still work; the tunnel just stays inactive.

## What you do **not** touch

You should not need to edit the SSH pipeline, the connection actor, the PTY
channels, the forwarder, or the error classes to support a new bot type. If you
find yourself wanting to, that's a signal the new behavior should be expressed as
**configuration** (a new per-bot field with a default) rather than a branch in the
core. The whole point of R15 is that bot types are data, not code paths.

## The `Signer` seam (future hardware-backed keys)

Authentication goes through a `Signer` trait (`src-tauri/src/ssh/signer.rs`) that
exposes the public key, the **SSH algorithm id** (`ssh-ed25519` today), and a
`sign` method. The v1 implementation is ed25519 with the private key in the
Keychain. A future hardware-backed signer (Secure Enclave P-256) slots in behind
the same trait.

Note this is **not** a transparent swap: the Secure Enclave only does P-256 ECDSA,
so a hardware-backed signer changes the wire algorithm **and** the public key.
Every bot would have to be **re-provisioned** with the new public key — it's a
key-rotation event. The trait keeps that migration confined to the signer module,
but it is not invisible to operators. This is why the trait advertises its
algorithm id: the connection layer learns the right algorithm from the signer
rather than assuming ed25519.

## Roadmap context

Today, adding a bot and provisioning your key are **manual**. The planned AI Power
Guild integration will pull bot inventory and push your public key through the
Guild web app — at which point new bot types arrive as Guild-managed inventory
entries carrying these same per-bot fields. The config-driven model here is what
makes that integration additive rather than a rewrite.
