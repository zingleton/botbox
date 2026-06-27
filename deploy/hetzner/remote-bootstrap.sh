#!/usr/bin/env bash
# Runs ON the Hetzner box as the `hermes` user (piped in by bootstrap.sh).
#   $1 = default model (e.g. anthropic/claude-sonnet-4.5)
# Assumes ~/.hermes/.env already holds OPENROUTER_API_KEY (written separately).
set -euo pipefail
MODEL="${1:-anthropic/claude-sonnet-4.5}"

echo "[hermes-bootstrap] waiting for cloud-init to finish…"
sudo cloud-init status --wait 2>/dev/null || true

echo "[hermes-bootstrap] installing Hermes Agent (non-interactive)…"
curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh \
  | bash -s -- --non-interactive

# Make the freshly-installed `hermes` command resolvable in this shell.
export PATH="$HOME/.local/bin:$PATH"
# shellcheck disable=SC1090
[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc" || true
hash -r
command -v hermes >/dev/null || { echo "ERROR: hermes not on PATH after install"; exit 1; }

echo "[hermes-bootstrap] setting default model -> $MODEL"
hermes config set model.default "$MODEL" \
  || echo "WARN: 'hermes config set' failed; set the model later with 'hermes model'"

# Launcher: loads OPENROUTER_API_KEY from ~/.hermes/.env, then execs hermes.
cat > "$HOME/start-hermes.sh" <<'EOF'
#!/usr/bin/env bash
set -a; [ -f "$HOME/.hermes/.env" ] && . "$HOME/.hermes/.env"; set +a
export PATH="$HOME/.local/bin:$PATH"
exec hermes "$@"
EOF
chmod +x "$HOME/start-hermes.sh"

echo "[hermes-bootstrap] (re)starting tmux sessions…"
tmux kill-session -t hermes    2>/dev/null || true
tmux kill-session -t dashboard 2>/dev/null || true
# 'exec bash -l' keeps the tmux window alive even if the inner process exits,
# so `tmux attach` always lands you somewhere useful.
tmux new-session -d -s hermes    "$HOME/start-hermes.sh; exec bash -l"
tmux new-session -d -s dashboard "$HOME/start-hermes.sh dashboard --host 127.0.0.1 --no-open; exec bash -l"

echo "[hermes-bootstrap] done. Active sessions:"
tmux ls
