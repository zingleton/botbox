#!/usr/bin/env bash
# Delete the server (stops billing). Leaves the uploaded SSH key in place.
source "$(dirname "$0")/lib.sh"; require_token; load_state
[ -n "${SERVER_ID:-}" ] || die "no server in .state to delete"
info "Deleting server $SERVER_ID ($SERVER_IP)…"
RESP="$(api DELETE "/servers/$SERVER_ID")"; api_ok "$RESP"
rm -f "$STATE_FILE"
info "Deleted. SSH key '$SSH_KEY_NAME' left on Hetzner (remove in console if unwanted)."
