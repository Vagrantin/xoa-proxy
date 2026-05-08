#!/usr/bin/env bash
# deploy-proxy.sh
# ════════════════════════════════════════════════════════════════════════════
# Installs xoa-proxy on the XCP-ng Dom0 host and (optionally) builds +
# deploys the updated XO-Lite bundle.
#
# Usage:
#   ./deploy-proxy.sh                          # uses HOST from environment
#   ./deploy-proxy.sh root@192.168.0.70        # explicit host
#   ./deploy-proxy.sh root@192.168.0.70 --app  # also build & deploy XO-Lite
#
# Requirements on your dev machine: ssh, rsync, (npx yarn if using --app)
# ════════════════════════════════════════════════════════════════════════════
set -euo pipefail

# ── Config ────────────────────────────────────────────────────────────────────
HOST="${1:-${XO_HOST:-root@192.168.0.70}}"
DEPLOY_APP=false

# Parse optional flags
for arg in "$@"; do
  [[ "$arg" == "--app" ]] && DEPLOY_APP=true
done

XOLITE_DIR="$(cd "$(dirname "$0")/.." && pwd)"  # project root (one level up from xoa-proxy/)
PROXY_BIN="$(dirname "$0")/xoa-proxy"
SERVICE_FILE="$(dirname "$0")/xoa-proxy.service"
LOG_ROTATE="$(dirname "$0")/logrotate.d/xoa-proxy"
REMOTE_WWW="/opt/xensource/www"
REMOTE_BIN="/opt/xensource/bin"
REMOTE_SERVICE="/etc/systemd/system/xoa-proxy.service"
REMOTE_LOGROTATE="/etc/logrotate.d/xoa-proxy"

echo "▶ Target host : $HOST"
echo "▶ Deploy app  : $DEPLOY_APP"
echo ""

# ── Step 1: Install the proxy script on Dom0 ──────────────────────────────────
echo "── [1/4] Copying xoa-proxy to $HOST:$REMOTE_BIN/"
scp "$PROXY_BIN" "$HOST:$REMOTE_BIN/"

# ── Step 2: Install and enable the systemd service ────────────────────────────
echo "── [2/4] Installing systemd unit"
scp "$SERVICE_FILE" "$HOST:$REMOTE_SERVICE"

ssh "$HOST" bash <<'REMOTE'
  set -e
  systemctl daemon-reload
  systemctl enable xoa-proxy
  # Restart if already running, start if not
  if systemctl is-active --quiet xoa-proxy; then
    systemctl restart xoa-proxy
    echo "  ✔ xoa-proxy restarted"
  else
    systemctl start xoa-proxy
    echo "  ✔ xoa-proxy started"
  fi
  # Verify it came up
  sleep 1
  systemctl is-active xoa-proxy && echo "  ✔ xoa-proxy is running" || echo "  ✘ xoa-proxy failed to start — check: journalctl -u xoa-proxy"
REMOTE

# ── Step 3: Install and enable the logrotate  ────────────────────────────
echo "── [3/4] pushing logrotate config"
scp "$LOG_ROTATE" "$HOST:$REMOTE_LOGROTATE"

# ── Step 4 (optional): Build and deploy XO-Lite ───────────────────────────────
if [[ "$DEPLOY_APP" == "true" ]]; then
  echo "── [4/4] Building XO-Lite"
  cd "$XOLITE_DIR" && npx yarn build

  echo "      Deploying dist/ to $HOST:$REMOTE_WWW/"
  cd "$XOLITE_DIR/dist"
  scp -r * "$HOST:$REMOTE_WWW/"
  echo "  ✔ XO-Lite deployed"
else
  echo "── [4/4] Skipping XO-Lite build (pass --app to include)"
fi

echo ""
echo "✔ Done. Proxy endpoint: http://127.0.0.1:9001/image.xva?src=<https-url>"
echo "  Logs: ssh $HOST tail -f /var/log/xoa-proxy.log"
