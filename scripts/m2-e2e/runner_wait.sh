#!/usr/bin/env bash
# Bounded check that audit-runner has no audit in progress (RUNBOOK §4.5). Run as root:
#   sudo bash scripts/m2-e2e/runner_wait.sh [TOTAL_MINUTES]        (default 30)
# Exit 0: no codex process of the auditor user and no established connection on the runner socket.
# Exit 2: a check could not be read (pgrep or ss failed). Exit 3: deadline passed, still active.
# It never kills anything and never stops a unit.
set -euo pipefail
TOTAL_MIN=${1:-30}
POLL=${POLL_SECS:-30}
PGREP=${PGREP:-pgrep}
SS=${SS:-ss}
SOCK=${RUNNER_SOCK:-/run/audit-runner/runner.sock}
START=$(date +%s); DEADLINE=$((START + TOTAL_MIN * 60))
echo "runner wait from $(date -u -d "@$START" +%FT%TZ) until $(date -u -d "@$DEADLINE" +%FT%TZ)"
while :; do
    set +e
    PROCS=$("$PGREP" -u auditor -af codex); prc=$?
    SOCKS=$("$SS" -xp); src=$?
    set -e
    # pgrep: 0 found, 1 none, anything else an error.
    if [ "$prc" -gt 1 ] || [ "$src" -ne 0 ]; then
        echo "READ FAILED: pgrep exit $prc, ss exit $src" >&2; exit 2
    fi
    CONN=$(printf '%s\n' "$SOCKS" | grep -F "$SOCK" | grep -E 'ESTAB|CLOSE-WAIT' || true)
    if [ "$prc" = 1 ] && [ -z "$CONN" ]; then
        echo "IDLE at $(date -u +%FT%TZ): no codex process, no connection on $SOCK"; exit 0
    fi
    echo "$(date -u +%FT%TZ) still active:"; [ "$prc" = 0 ] && printf '  %s\n' "$PROCS"; [ -n "$CONN" ] && printf '  %s\n' "$CONN"
    if [ "$(date +%s)" -ge "$DEADLINE" ]; then
        echo "DEADLINE PASSED: stop and report; do not kill, do not continue" >&2; exit 3
    fi
    sleep "$POLL"
done
