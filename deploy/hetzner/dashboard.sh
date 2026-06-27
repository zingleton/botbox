#!/usr/bin/env bash
# Forward the bot's localhost-only Hermes dashboard to your machine and open it.
source "$(dirname "$0")/lib.sh"; load_state
[ -n "${SERVER_IP:-}" ] || die "no server — run ./provision.sh first"
URL="http://localhost:$LOCAL_TUNNEL_PORT"
info "Tunneling $URL  ->  $SERVER_IP:$DASHBOARD_PORT   (Ctrl-C to stop)"
( sleep 2
  if   command -v powershell.exe >/dev/null 2>&1; then powershell.exe -NoProfile -Command "Start-Process '$URL'" >/dev/null 2>&1
  elif command -v xdg-open       >/dev/null 2>&1; then xdg-open "$URL" >/dev/null 2>&1
  elif command -v open           >/dev/null 2>&1; then open "$URL" >/dev/null 2>&1
  fi ) &
exec ssh $SSH_OPTS -N -L "$LOCAL_TUNNEL_PORT:127.0.0.1:$DASHBOARD_PORT" "$REMOTE_USER@$SERVER_IP"
