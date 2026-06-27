#!/usr/bin/env bash
# Open a host shell on the bot.
source "$(dirname "$0")/lib.sh"; load_state
[ -n "${SERVER_IP:-}" ] || die "no server — run ./provision.sh first"
exec ssh $SSH_OPTS "$REMOTE_USER@$SERVER_IP"
