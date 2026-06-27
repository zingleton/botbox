#!/usr/bin/env bash
# Attach to the live Hermes session (the Botbox "Hermes terminal" equivalent).
source "$(dirname "$0")/lib.sh"; load_state
[ -n "${SERVER_IP:-}" ] || die "no server — run ./provision.sh first"
exec ssh $SSH_OPTS -t "$REMOTE_USER@$SERVER_IP" 'tmux attach -t hermes'
