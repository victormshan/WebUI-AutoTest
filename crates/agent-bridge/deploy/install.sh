#!/usr/bin/env bash
# Installs agent-bridge for the current user (WSL), once. No sudo needed. Run by the user:
#   bash crates/agent-bridge/deploy/install.sh [--binary target/release/agent-bridge] [--distro Ubuntu]
#
# What it does (idempotent; existing tokens are kept):
#  1. installs the binary to ~/.local/bin/agent-bridge
#  2. creates three random tokens (CSPRNG) in ~/.config/agent-bridge (0700 dir, 0600 files):
#       token            Claude's token for agent-bridge
#       dsh.token        DSH's token for agent-bridge
#       dsh-notify.token what agent-bridge presents to DSH's notify endpoint
#     and writes agents.json (only the SHA-256 of the two agent tokens) and notify.json
#  3. sets Windows user environment variables for the DSH host (values go through stdin, never
#     the command line): DSH_AGENT_BRIDGE_URL, DSH_AGENT_BRIDGE_TOKEN, DSH_RELAY_BRIDGE_NOTIFY_TOKEN
#  4. installs and starts the systemd user service agent-bridge.service (127.0.0.1:7879)
#  5. registers a Windows logon task that starts WSL, so the services come up after a reboot
#
# The DSH host only sees the new environment variables after it restarts. That restart rotates
# the phone link token, so it is NOT done here: decide when, and tell Claude/DSH.
set -euo pipefail

BIN=target/release/agent-bridge
DISTRO=${WSL_DISTRO_NAME:-Ubuntu}
while [ $# -gt 0 ]; do
  case "$1" in
    --binary) BIN=$2; shift 2 ;;
    --distro) DISTRO=$2; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ "$(id -u)" != 0 ] || { echo "run as your normal user, not root" >&2; exit 1; }
[ -x "$BIN" ] || { echo "binary $BIN not found; build with: cargo build --release -p agent-bridge" >&2; exit 1; }
PS=${AGENT_BRIDGE_PS:-/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe}
[ -x "$PS" ] || { echo "Windows PowerShell not found at $PS" >&2; exit 1; }

# 1. binary
install -d -m 0755 "$HOME/.local/bin"
install -m 0755 "$BIN" "$HOME/.local/bin/agent-bridge.new"
mv -f "$HOME/.local/bin/agent-bridge.new" "$HOME/.local/bin/agent-bridge"
AB="$HOME/.local/bin/agent-bridge"

# 2. tokens and config
CFG="$HOME/.config/agent-bridge"
install -d -m 0700 "$CFG"
umask 077
for f in token dsh.token dsh-notify.token; do
  if [ ! -s "$CFG/$f" ]; then
    head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$CFG/$f"
    echo "created $CFG/$f"
  fi
  chmod 0600 "$CFG/$f"
done
printf '{"claude":"%s","dsh":"%s"}\n' \
  "$("$AB" hash-token < "$CFG/token")" "$("$AB" hash-token < "$CFG/dsh.token")" > "$CFG/agents.json"
cat > "$CFG/notify.json" <<JSON
{"dsh": {"url": "http://127.0.0.1:3080/dsh-web-relay/bridge/notify", "token_file": "$CFG/dsh-notify.token", "transport": "auto"}}
JSON
chmod 0600 "$CFG/agents.json" "$CFG/notify.json"

# 3. Windows user environment for the DSH host (read from stdin, nothing on argv)
{
  echo "http://127.0.0.1:7879"
  cat "$CFG/dsh.token"; echo
  cat "$CFG/dsh-notify.token"; echo
} | "$PS" -NoProfile -NonInteractive -Command '
  $url = [Console]::In.ReadLine(); $tok = [Console]::In.ReadLine(); $notify = [Console]::In.ReadLine()
  foreach ($p in @(@("DSH_AGENT_BRIDGE_URL", $url), @("DSH_AGENT_BRIDGE_TOKEN", $tok), @("DSH_RELAY_BRIDGE_NOTIFY_TOKEN", $notify))) {
    if (-not $p[1]) { throw "empty value for $($p[0])" }
    [Environment]::SetEnvironmentVariable($p[0], $p[1].Trim(), "User")
  }
  Write-Host "Windows user environment: DSH_AGENT_BRIDGE_URL, DSH_AGENT_BRIDGE_TOKEN, DSH_RELAY_BRIDGE_NOTIFY_TOKEN set"' | tr -d '\r'

# Migration (design b6): finished tasks of the old file protocol become closed archives.
# Unfinished ones are refused and listed — finish them in the file protocol first.
if [ -z "${AGENT_BRIDGE_NO_SYSTEMD:-}" ]; then systemctl --user stop agent-bridge.service 2>/dev/null || true; fi
if [ -d /mnt/d/cc-tasks/claude-bridge/tasks ] || [ -n "${AGENT_BRIDGE_IMPORT_DIR:-}" ]; then
  "$AB" import --dir "${AGENT_BRIDGE_IMPORT_DIR:-/mnt/d/cc-tasks/claude-bridge/tasks}" || echo "(some file-protocol tasks were not imported; see above)"
fi

if [ -n "${AGENT_BRIDGE_NO_SYSTEMD:-}" ]; then echo "(test mode: skipping systemd and the Windows logon task)"; exit 0; fi

# 4. systemd user service
UNIT_DIR="$HOME/.config/systemd/user"
install -d -m 0755 "$UNIT_DIR"
cat > "$UNIT_DIR/agent-bridge.service" <<UNIT
[Unit]
Description=agent-bridge: two-way task dispatch between Claude Code and the DSH main agent
After=default.target

[Service]
Type=simple
# curl.exe (to reach DSH's notify on Windows loopback) is found here; the binary also falls back to it.
Environment=PATH=/usr/local/bin:/usr/bin:/bin:/mnt/c/Windows/System32
ExecStart=$AB serve --listen 127.0.0.1:7879 --mirror /mnt/d/cc-tasks/claude-bridge/tasks --step-relay-dir $HOME/.claude/step-relay
Restart=on-failure
RestartSec=3
UMask=0077

[Install]
WantedBy=default.target
UNIT
systemctl --user daemon-reload
systemctl --user enable --now agent-bridge.service
systemctl --user restart agent-bridge.service
for _ in 1 2 3 4 5 6 7 8 9 10; do
  if curl -fsS http://127.0.0.1:7879/v1/health >/dev/null 2>&1; then break; fi
  sleep 1
done
curl -fsS http://127.0.0.1:7879/v1/health && echo

# 5. start WSL at Windows logon (same kind of task DSH already uses: logon trigger, no elevation)
"$PS" -NoProfile -NonInteractive -Command "
  \$action = New-ScheduledTaskAction -Execute 'wsl.exe' -Argument '-d $DISTRO --exec /bin/true'
  \$trigger = New-ScheduledTaskTrigger -AtLogOn -User \$env:USERNAME
  \$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 5)
  Register-ScheduledTask -TaskName 'WSL-Autostart-$DISTRO' -Action \$action -Trigger \$trigger -Settings \$settings -Description 'Start WSL at logon so review-gate, cc-watchdog and agent-bridge run' -Force | Out-Null
  Write-Host 'Windows logon task WSL-Autostart-$DISTRO registered'" | tr -d '\r'

echo
echo "Done. Next (your decision): restart the DSH host so it picks up the new environment"
echo "variables — this rotates the phone link token. Tell Claude when, in your own words."
