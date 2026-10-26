#!/usr/bin/env bash
# Waits, bounded, until a gate with a drain (code revision fd34eae or later) is safe to stop.
#
#   bash scripts/m2-e2e/drain_wait.sh [URL] [TOTAL_MINUTES]      (defaults: http://127.0.0.1:7878, 60)
#
# Exit 0: /health shows drain.draining = true, drain.idle = true, drain.active_ops = 0,
#         drain.failure_count = 0 and m2_pending = 0 (integers, exact types).
# Exit 2: a read failed (connection, timeout, HTTP status not 200, not JSON, a field missing or of
#         the wrong type, m2_pending null). Nothing is retried after such an error: report it.
# Exit 3: the total deadline passed with the gate still not idle: report drain.failures and stop.
# It never touches the drain file and never stops the service.
set -euo pipefail

URL=${1:-http://127.0.0.1:7878}
TOTAL_MIN=${2:-60}
POLL=${POLL_SECS:-30}
START=$(date +%s)
DEADLINE=$((START + TOTAL_MIN * 60))
echo "drain wait: $URL, from $(date -u -d "@$START" +%FT%TZ) until $(date -u -d "@$DEADLINE" +%FT%TZ)"

while :; do
    BODY=$(mktemp)
    set +e
    CODE=$(curl -sS --connect-timeout 5 --max-time 15 -o "$BODY" -w '%{http_code}' "$URL/health")
    RC=$?
    set -e
    if [ "$RC" -ne 0 ] || [ "$CODE" != 200 ]; then
        echo "READ FAILED: curl exit $RC, HTTP $CODE" >&2
        rm -f "$BODY"
        exit 2
    fi
    set +e
    python3 - "$BODY" <<'PY'
import json, sys
try:
    h = json.load(open(sys.argv[1]))
    d = h["drain"]
    checks = {
        "draining": (d["draining"], bool), "idle": (d["idle"], bool),
        "active_ops": (d["active_ops"], int), "failure_count": (d["failure_count"], int),
        "m2_pending": (h["m2_pending"], int),
    }
    for name, (v, t) in checks.items():
        if type(v) is not t:            # exact type: bool is not accepted as int, null is refused
            raise TypeError(f"{name} is {v!r}, not {t.__name__}")
except Exception as e:                   # missing field, wrong type, not JSON
    print(f"BAD HEALTH ANSWER: {e}", file=sys.stderr)
    sys.exit(2)
print(f"draining={d['draining']} idle={d['idle']} active_ops={d['active_ops']} "
      f"failure_count={d['failure_count']} m2_pending={h['m2_pending']} failures={d.get('failures')}")
ok = d["draining"] and d["idle"] and d["active_ops"] == 0 and d["failure_count"] == 0 and h["m2_pending"] == 0
sys.exit(0 if ok else 1)
PY
    RC=$?
    set -e
    rm -f "$BODY"
    case $RC in
        0) echo "IDLE at $(date -u +%FT%TZ): safe to stop"; exit 0 ;;
        1) ;;
        *) exit 2 ;;
    esac
    if [ "$(date +%s)" -ge "$DEADLINE" ]; then
        echo "DEADLINE PASSED at $(date -u +%FT%TZ): not idle; stop and report (do not stop the service)" >&2
        exit 3
    fi
    sleep "$POLL"
done
