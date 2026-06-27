#!/usr/bin/env bash
# Shared config + Hetzner Cloud API helpers for the Hermes test-bot deploy.
# Sourced by the other scripts in this folder.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="$HERE/.env"
STATE_FILE="$HERE/.state"

# --- Load user config (.env) --------------------------------------------
if [ -f "$ENV_FILE" ]; then
  set -a; . "$ENV_FILE"; set +a
fi

# --- Defaults (override any of these in .env) ---------------------------
: "${SERVER_NAME:=hermes-bot-1}"
: "${SERVER_TYPE:=cpx21}"          # 3 vCPU AMD / 4 GB / 80 GB
: "${SERVER_LOCATION:=ash}"        # Ashburn, VA (US East)
: "${SERVER_IMAGE:=ubuntu-24.04}"
: "${SSH_KEY_NAME:=botbox-hermes}"
: "${SSH_KEY_PATH:=$HOME/.ssh/botbox_hermes}"
: "${REMOTE_USER:=hermes}"
: "${HERMES_MODEL:=anthropic/claude-sonnet-4.5}"
: "${DASHBOARD_PORT:=9119}"
: "${LOCAL_TUNNEL_PORT:=9119}"

API="https://api.hetzner.cloud/v1"
SSH_OPTS="-o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=$HERE/.known_hosts -o ConnectTimeout=10 -i $SSH_KEY_PATH"

die(){ echo "ERROR: $*" >&2; exit 1; }
info(){ echo ">> $*" >&2; }
need(){ command -v "$1" >/dev/null 2>&1 || die "missing required tool: $1"; }
require_token(){ [ -n "${HCLOUD_TOKEN:-}" ] || die "HCLOUD_TOKEN not set — edit $ENV_FILE"; }

# api METHOD PATH [json-body]  -> prints raw JSON response (no -f, so we can
# read Hetzner's {"error":...} bodies instead of just failing on HTTP status).
api(){
  local method="$1" path="$2" body="${3:-}"
  if [ -n "$body" ]; then
    curl -sS -X "$method" "$API$path" \
      -H "Authorization: Bearer $HCLOUD_TOKEN" \
      -H "Content-Type: application/json" \
      -d "$body"
  else
    curl -sS -X "$method" "$API$path" \
      -H "Authorization: Bearer $HCLOUD_TOKEN"
  fi
}

# Abort if a Hetzner response carries an error object.
api_ok(){ echo "$1" | jq -e 'has("error") and .error != null' >/dev/null 2>&1 \
  && die "Hetzner API: $(echo "$1" | jq -c '.error')" || true; }

load_state(){ [ -f "$STATE_FILE" ] && . "$STATE_FILE" || true; }
save_state(){ printf 'SERVER_ID=%s\nSERVER_IP=%s\n' "$1" "$2" > "$STATE_FILE"; }
