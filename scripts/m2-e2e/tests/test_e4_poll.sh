#!/usr/bin/env bash
# Fixture test of e4_run.sh's polling with a stand-in review-gate (no service, no root).
# The gate serialises request states in lowercase; the driver must keep polling while the
# state is "pending" and stop at the first other state (2026-10-26: it compared to "Pending"
# and stopped at the first poll).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SCRIPT=$HERE/../e4_run.sh
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
REPO=$(cd "$HERE/../../.." && pwd)
HEAD_ID=$(git -C "$REPO" rev-parse HEAD)

cat > "$TMP/review-gate" <<'EOF'
#!/usr/bin/env bash
# stand-in: `milestone status` answers from a counter; `milestone request` accepts.
set -euo pipefail
n_file=$FAKE_DIR/polls
case "$1 $2" in
  "milestone status")
    n=$(cat "$n_file" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$n_file"
    if [ "$n" -le 1 ]; then cur=null
    elif [ "$n" -le 3 ]; then cur='{"request_id":"m2-q1","state":"pending"}'
    else cur='{"request_id":"m2-q1","state":"failed"}'; fi
    printf '{"hash":"","reasons":[],"released":false,"seq":1,"state":{"registered":true,"current":%s},"sync":"Equal"}\n' "$cur"
    exit 1 ;;
  "milestone request")
    echo '{"request_id":"m2-q1"}' ;;
  *) echo "unexpected: $*" >&2; exit 9 ;;
esac
EOF
chmod +x "$TMP/review-gate"

set +e
FAKE_DIR=$TMP REVIEW_GATE_BIN=$TMP/review-gate REPO=$REPO BASE=$HEAD_ID HEAD=$HEAD_ID \
  E4_OUT=$TMP/out E4_POLL_INTERVAL=0 E4_POLL_TIMEOUT=30 REVIEW_GATE_URL=http://127.0.0.1:9 \
  REVIEW_GATE_TOKEN=fixture-token-not-used-by-any-service \
  bash "$SCRIPT" continue > "$TMP/run.out" 2>&1
set -e

polls=$(cat "$TMP/polls")
[ "$polls" = 4 ] && echo "ok - polled while pending, stopped at the first other state (4 status calls)" \
  || { echo "FAIL - $polls status calls"; cat "$TMP/run.out"; exit 1; }
grep -q '"state":"failed"' "$TMP/out/05-status-final.json" && echo "ok - the final status is the failed one" \
  || { cat "$TMP/run.out"; exit 1; }
echo "ALL OK"
