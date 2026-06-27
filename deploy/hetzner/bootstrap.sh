#!/usr/bin/env bash
# Install + start Hermes on the already-provisioned box, over SSH.
source "$(dirname "$0")/lib.sh"
need ssh; need curl
load_state
[ -n "${SERVER_IP:-}" ] || die "no server IP in .state — run ./provision.sh first"
[ -n "${OPENROUTER_API_KEY:-}" ] || die "OPENROUTER_API_KEY not set — edit $ENV_FILE"

info "Waiting for SSH on $REMOTE_USER@$SERVER_IP…"
ok=""
for _ in $(seq 1 60); do
  if ssh $SSH_OPTS "$REMOTE_USER@$SERVER_IP" true 2>/dev/null; then ok=1; break; fi
  sleep 5
done
[ -n "$ok" ] || die "could not SSH in (key not provisioned yet, or host unreachable)"

info "Writing OpenRouter key to ~/.hermes/.env (encrypted channel, mode 0600)…"
printf 'OPENROUTER_API_KEY=%s\n' "$OPENROUTER_API_KEY" \
  | ssh $SSH_OPTS "$REMOTE_USER@$SERVER_IP" 'umask 077; mkdir -p ~/.hermes && cat > ~/.hermes/.env'

info "Running remote bootstrap (install + configure + start Hermes)…"
ssh $SSH_OPTS "$REMOTE_USER@$SERVER_IP" 'bash -s' -- "$HERMES_MODEL" \
  < "$(dirname "$0")/remote-bootstrap.sh"

echo
info "Hermes is up on $SERVER_IP"
echo "  Host shell:        ./connect.sh"
echo "  Hermes session:    ./attach.sh        (tmux attach -t hermes)"
echo "  Dashboard tunnel:  ./dashboard.sh     (http://localhost:$LOCAL_TUNNEL_PORT)"
