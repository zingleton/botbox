# Hermes bot on Hetzner — automated deploy

Scripted, no-web-UI provisioning of a [Nous Research Hermes Agent](https://github.com/NousResearch/hermes-agent)
bot on a Hetzner Cloud VPS, talking to an LLM through **OpenRouter**. This is the
"real test bot" the Botbox brainstorm calls for: it runs Hermes in a `tmux`
session and serves its dashboard on `127.0.0.1:9119`, so Botbox can attach with
`tmux attach -t hermes` and tunnel the dashboard over SSH.

Everything is driven by the Hetzner Cloud REST API (`curl` + `jq`) — no `hcloud`
CLI or web console required.

## What it builds

- **Server:** `cpx21` (3 vCPU AMD / 4 GB / 80 GB) in **Ashburn, VA (`ash`)`,
  Ubuntu 24.04. ~$0.013/hr (~$8/mo). No GPU — all inference is on OpenRouter.
- **User `hermes`** with your SSH public key; passwordless sudo.
- **Hermes** installed via the official installer, default model
  `anthropic/claude-sonnet-4.5`, running in two tmux sessions:
  - `hermes` — the interactive agent session (attach to talk to it)
  - `dashboard` — `hermes dashboard` bound to `127.0.0.1:9119`

## Prerequisites

Run from **Git Bash** (already have `curl`, `jq`, `ssh`, `ssh-keygen`).

## Setup

```bash
cd deploy/hetzner
cp .env.example .env
# edit .env: paste HCLOUD_TOKEN and OPENROUTER_API_KEY (both required)
```

- **HCLOUD_TOKEN** — Hetzner Cloud Console → project → Security → API Tokens (Read & Write).
- **OPENROUTER_API_KEY** — https://openrouter.ai/keys

The SSH keypair at `~/.ssh/botbox_hermes` is generated automatically if absent.
Already have a key you want to use? Set `SSH_KEY_PATH` in `.env` to its path
(the script expects `$SSH_KEY_PATH.pub` to exist).

## Deploy

```bash
./up.sh            # provision + install + start (one shot)
```

or step by step:

```bash
./provision.sh     # create the server, wait for boot, record IP in .state
./bootstrap.sh     # install + configure + start Hermes over SSH
```

## Use it

```bash
./connect.sh       # host shell on the bot
./attach.sh        # attach to the live Hermes session (tmux attach -t hermes)
./dashboard.sh     # forward 9119 -> localhost and open the browser
```

## Tear down

```bash
./destroy.sh       # delete the server (stops billing); keeps the SSH key on Hetzner
```

## Botbox mapping

Fills the deferred values in the Botbox requirements doc:

| Botbox per-bot config | Value here                |
|-----------------------|---------------------------|
| Hermes attach command | `tmux attach -t hermes`   |
| Dashboard remote port | `9119`                    |
| SSH user / IP         | `hermes@<.state IP>`      |

## Notes & hardening

- **Secrets:** `.env`, `.state`, `.known_hosts` are gitignored. The OpenRouter
  key is sent over SSH into `~/.hermes/.env` (mode 0600) — it is *not* baked into
  Hetzner cloud-init metadata.
- **Dashboard auth:** the dashboard stores API keys and has no auth, so it binds
  to localhost only and is reached exclusively through the SSH tunnel. Don't
  expose it on `0.0.0.0`.
- **Persistence:** tmux sessions survive disconnects but not a reboot. For an
  always-on bot, convert the two launches into systemd services (next step).
- **Firewall:** only port 22 is reachable from the internet (dashboard is
  localhost-bound, gateway API is off). Attach a Hetzner firewall limiting 22 to
  your IP if you want defense in depth.
- **Reusing across runs:** `provision.sh` and the key upload are idempotent —
  rerunning reuses an existing server of the same `SERVER_NAME`.
```
