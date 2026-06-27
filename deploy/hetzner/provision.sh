#!/usr/bin/env bash
# Create (or reuse) the Hetzner server that will run Hermes.
#   - generates an SSH keypair if needed
#   - uploads the public key to Hetzner (idempotent)
#   - creates the server with cloud-init base provisioning
#   - waits until it is running and records IP in .state
source "$(dirname "$0")/lib.sh"
need curl; need jq; need ssh; need ssh-keygen
require_token

# 1. Local SSH keypair -------------------------------------------------------
PUB="$SSH_KEY_PATH.pub"
if [ ! -f "$PUB" ]; then
  info "No public key at $PUB — generating an ed25519 keypair"
  mkdir -p "$(dirname "$SSH_KEY_PATH")"
  ssh-keygen -t ed25519 -N "" -f "$SSH_KEY_PATH" -C "$SSH_KEY_NAME" >/dev/null
fi
PUBKEY="$(cat "$PUB")"

# 2. Upload SSH key (idempotent by name) ------------------------------------
RESP="$(api GET "/ssh_keys?name=$SSH_KEY_NAME")"; api_ok "$RESP"
KEY_ID="$(echo "$RESP" | jq -r '.ssh_keys[0].id // empty')"
if [ -n "$KEY_ID" ]; then
  info "SSH key '$SSH_KEY_NAME' already on Hetzner (id $KEY_ID)"
else
  info "Uploading SSH key '$SSH_KEY_NAME'"
  RESP="$(api POST /ssh_keys "$(jq -n --arg n "$SSH_KEY_NAME" --arg k "$PUBKEY" '{name:$n,public_key:$k}')")"
  api_ok "$RESP"
  KEY_ID="$(echo "$RESP" | jq -r '.ssh_key.id')"
fi

# 3. Reuse an existing server with this name --------------------------------
RESP="$(api GET "/servers?name=$SERVER_NAME")"; api_ok "$RESP"
SID="$(echo "$RESP" | jq -r '.servers[0].id // empty')"
if [ -n "$SID" ]; then
  IP="$(echo "$RESP" | jq -r '.servers[0].public_net.ipv4.ip')"
  info "Server '$SERVER_NAME' already exists (id $SID, ip $IP) — reusing"
  save_state "$SID" "$IP"
  exit 0
fi

# 4. Render cloud-init with the public key ----------------------------------
# '|' is a safe sed delimiter: base64 SSH keys never contain it.
USERDATA="$(sed "s|__SSH_PUBLIC_KEY__|$PUBKEY|" "$(dirname "$0")/cloud-init.yaml")"

# 5. Create the server ------------------------------------------------------
info "Creating $SERVER_TYPE '$SERVER_NAME' in $SERVER_LOCATION ($SERVER_IMAGE)…"
BODY="$(jq -n \
  --arg name "$SERVER_NAME" --arg type "$SERVER_TYPE" \
  --arg loc "$SERVER_LOCATION" --arg img "$SERVER_IMAGE" \
  --arg key "$SSH_KEY_NAME" --arg ud "$USERDATA" \
  '{name:$name, server_type:$type, location:$loc, image:$img,
    ssh_keys:[$key], user_data:$ud,
    labels:{app:"hermes","managed-by":"botbox"},
    public_net:{enable_ipv4:true, enable_ipv6:true}}')"
RESP="$(api POST /servers "$BODY")"; api_ok "$RESP"
SID="$(echo "$RESP" | jq -r '.server.id // empty')"
[ -n "$SID" ] || die "server create failed: $RESP"

# 6. Poll until running -----------------------------------------------------
info "Server $SID created — waiting for it to boot…"
ST=""; IP=""
for _ in $(seq 1 60); do
  RESP="$(api GET "/servers/$SID")"; api_ok "$RESP"
  ST="$(echo "$RESP" | jq -r '.server.status')"
  IP="$(echo "$RESP" | jq -r '.server.public_net.ipv4.ip')"
  [ "$ST" = running ] && break
  sleep 5
done
[ "$ST" = running ] || die "server did not reach 'running' (last status: $ST)"
save_state "$SID" "$IP"

info "Server running at $IP (id $SID)"
echo
echo "Next:  ./bootstrap.sh   # installs + starts Hermes over SSH"
echo "  (or run ./up.sh to do provision + bootstrap in one go)"
