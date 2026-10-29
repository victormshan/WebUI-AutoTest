#!/usr/bin/env bash
# Bounded check that audit-runner has no audit in progress (RUNBOOK §4.5). Run as root:
#   sudo bash scripts/m2-e2e/runner_wait.sh [TOTAL_MINUTES]        (default 30)
# Exit 0: no codex process of the auditor user other than the runner itself, and no established
#         connection on the runner socket. The runner's own argv carries `--codex <path>`, so
#         pgrep matches it too; its main PID (systemctl MainPID) is not counted.
# Exit 2: a check could not be read (pgrep, ss or systemctl failed, or MainPID is not a number). Exit 3: deadline passed, still active.
# It never kills anything and never stops a unit.
set -euo pipefail
TOTAL_MIN=${1:-30}
POLL=${POLL_SECS:-30}
PGREP=${PGREP:-pgrep}
SS=${SS:-ss}
SYSTEMCTL=${SYSTEMCTL:-systemctl}
UNIT=${RUNNER_UNIT:-audit-runner.service}
SOCK=${RUNNER_SOCK:-/run/audit-runner/runner.sock}
START=$(date +%s); DEADLINE=$((START + TOTAL_MIN * 60))
echo "runner wait from $(date -u -d "@$START" +%FT%TZ) until $(date -u -d "@$DEADLINE" +%FT%TZ)"
while :; do
    set +e
    RPID=$("$SYSTEMCTL" show -p MainPID --value "$UNIT"); syc=$?
    PROCS=$("$PGREP" -u auditor -af codex); prc=$?
    SOCKS=$("$SS" -xp); src=$?
    set -e
    # pgrep: 0 found, 1 none, anything else an error. MainPID is 0 when the runner is not running.
    if [ "$prc" -gt 1 ] || [ "$src" -ne 0 ] || [ "$syc" -ne 0 ] || ! [[ "$RPID" =~ ^[0-9]+$ ]]; then
        echo "READ FAILED: pgrep exit $prc, ss exit $src, systemctl exit $syc (MainPID '$RPID')" >&2; exit 2
    fi
    PROCS=$(printf '%s\n' "$PROCS" | awk -v p="$RPID" 'NF && $1 != p')
    CONN=$(printf '%s\n' "$SOCKS" | grep -F "$SOCK" | grep -E 'ESTAB|CLOSE-WAIT' || true)
    if [ -z "$PROCS" ] && [ -z "$CONN" ]; then
        echo "IDLE at $(date -u +%FT%TZ): no codex process, no connection on $SOCK"; exit 0
    fi
    echo "$(date -u +%FT%TZ) still active:"; [ -n "$PROCS" ] && printf '  %s\n' "$PROCS"; [ -n "$CONN" ] && printf '  %s\n' "$CONN"
    if [ "$(date +%s)" -ge "$DEADLINE" ]; then
        echo "DEADLINE PASSED: stop and report; do not kill, do not continue" >&2; exit 3
    fi
    sleep "$POLL"
done
