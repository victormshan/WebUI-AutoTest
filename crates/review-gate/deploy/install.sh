#!/usr/bin/env bash
# Installs review-gate as the `reviewgate` system user. Run once, as root:
#   sudo bash deploy/install.sh [--binary target/release/review-gate] [--implementer USER] [--repo-root DIR]
#
# Afterwards the implementer (Claude Code, running as USER) can talk to the gate with the client
# token but cannot read or write its state, its signing key, or its trace files.
set -euo pipefail

BIN=target/release/review-gate
IMPL=${SUDO_USER:-}
REPO_ROOT=
while [ $# -gt 0 ]; do
  case "$1" in
    --binary) BIN=$2; shift 2 ;;
    --implementer) IMPL=$2; shift 2 ;;
    --repo-root) REPO_ROOT=$2; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ "$(id -u)" = 0 ] || { echo "run as root (sudo)" >&2; exit 1; }
[ -n "$IMPL" ] || { echo "--implementer USER is required" >&2; exit 1; }
[ -x "$BIN" ] || { echo "binary $BIN not found; build with: cargo build --release -p review-gate" >&2; exit 1; }
IMPL_HOME=$(getent passwd "$IMPL" | cut -d: -f6)
REPO_ROOT=${REPO_ROOT:-$IMPL_HOME/projects}

# 1. The system user: no login shell, no home of interest, not in the implementer's groups.
if ! id reviewgate >/dev/null 2>&1; then
  useradd --system --home-dir /var/lib/reviewgate --shell /usr/sbin/nologin reviewgate
fi
if id -nG "$IMPL" | tr ' ' '\n' | grep -qx reviewgate; then
  echo "warning: $IMPL is in group reviewgate; remove it (gpasswd -d $IMPL reviewgate)" >&2
fi

# 2. Binary.
install -m 0755 -o root -g root "$BIN" /usr/local/bin/review-gate

# 3. State (private) and trace dir (world-readable, only the gate writes).
install -d -m 0755 -o reviewgate -g reviewgate /var/lib/reviewgate
install -d -m 0700 -o reviewgate -g reviewgate /var/lib/reviewgate/state
install -d -m 0755 -o reviewgate -g reviewgate /var/lib/reviewgate/relay /var/lib/reviewgate/relay/traces

# 4. Client token: readable by the gate and the implementer, writable by neither.
install -d -m 0755 /etc/review-gate
if [ ! -s /etc/review-gate/client.token ]; then
  head -c 48 /dev/urandom | base64 -w0 > /etc/review-gate/client.token
fi
chown root:reviewgate /etc/review-gate/client.token
chmod 0640 /etc/review-gate/client.token
# The implementer reads it through an ACL (no group membership needed).
if command -v setfacl >/dev/null; then
  setfacl -m "u:$IMPL:r" /etc/review-gate/client.token
else
  install -d -m 0700 -o "$IMPL" -g "$IMPL" "$IMPL_HOME/.config/review-gate"
  install -m 0600 -o "$IMPL" -g "$IMPL" /etc/review-gate/client.token "$IMPL_HOME/.config/review-gate/token"
fi

# 5. Reviewer credentials (API keys) for the gate only. Edit /etc/review-gate/env afterwards.
if [ ! -e /etc/review-gate/env ]; then
  cat > /etc/review-gate/env <<'ENV'
# Read by review-gate.service. At least one reviewer from another vendor:
#GEMINI_API_KEY=
#DEEPSEEK_API_KEY=
#REVIEW_GATE_BASE_URL= / REVIEW_GATE_API_KEY= / REVIEW_GATE_MODEL=
# web-gemini bridge (dsh-web-gemini-ext), default http://localhost:8899
#DSH_RELAY_BRIDGE=http://localhost:8899
ENV
fi
chown root:reviewgate /etc/review-gate/env
chmod 0640 /etc/review-gate/env

# 6. Read access to the implementer's repositories (diffs are computed read-only).
#    Directories need o+x on the path and o+r on the repo; most homes already allow this.
for d in "$IMPL_HOME" "$REPO_ROOT"; do
  [ -d "$d" ] && chmod o+x "$d"
done
echo "repositories under $REPO_ROOT must be readable by others (chmod -R o+rX <repo>) for the gate to diff them"

# 7. Service.
install -m 0644 "$(dirname "$0")/review-gate.service" /etc/systemd/system/review-gate.service
if [ -d /run/systemd/system ]; then
  systemctl daemon-reload
  systemctl enable --now review-gate
  sleep 1
  systemctl --no-pager --lines=5 status review-gate || true
else
  echo "systemd is not running (WSL without systemd?). Start manually with:"
  echo "  sudo -u reviewgate env \$(grep -v '^#' /etc/review-gate/env | xargs) /usr/local/bin/review-gate serve --relay-dir /var/lib/reviewgate/relay &"
fi

echo
echo "public key (pin it in .github/workflows/review-gate.yml):"
for _ in 1 2 3 4 5; do
  curl -fsS http://127.0.0.1:7878/pubkey 2>/dev/null && break
  sleep 1
done
echo
