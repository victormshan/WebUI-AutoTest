#!/bin/sh
# Self-contained driver test: a stub review-gate, two approved rounds, no network.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/auto-iterate-test.XXXXXX")
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

fail() { echo "test-driver: FAIL: $*" >&2; exit 1; }

repo=$tmp/repo
bin=$tmp/bin
state=$tmp/state
mkdir -p "$repo" "$bin"
git -C "$repo" init -q
git -C "$repo" config user.email test@example.com
git -C "$repo" config user.name test
echo base > "$repo/f"
git -C "$repo" add f
git -C "$repo" commit -qm base
echo 0 > "$state"

cat > "$bin/review-gate" <<'STUB'
#!/bin/sh
set -eu
state=${STUB_STATE:?}
n=$(cat "$state" 2>/dev/null || echo 0)
[ -n "$n" ] || n=0
case "$1" in
    task) printf '{"status":"running","current_iteration":%s,"repo":"%s","stop_reason":null}\n' "$((n+1))" "$STUB_REPO" ;;
    stage) git -C "$STUB_REPO" add -A; printf '{"tree":"t","base":"b"}\n' ;;
    review)
        if [ "${STUB_SLOW_REVIEW:-0}" = 1 ]; then sleep 5; fi
        printf '{"record":{"id":"v%s-1","verdict":"approved","text":"ok"}}\n' "$((n+1))"
        ;;
    record)
        if [ "$n" -eq 0 ]; then printf '{"action":"start_round"}\n'; else printf '{"action":"finalize"}\n'; fi
        echo $((n+1)) > "$state"
        ;;
    *) echo "stub: unsupported $*" >&2; exit 1 ;;
esac
STUB
chmod +x "$bin/review-gate"

cat > "$tmp/agent.sh" <<'AG'
#!/bin/sh
set -eu
echo "v$AUTO_ITERATE_ITERATION" > "$AUTO_ITERATE_REPO/out.txt"
AG
chmod +x "$tmp/agent.sh"

STUB_REPO=$repo STUB_STATE=$state PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task demo --agent-cmd "$tmp/agent.sh" \
    --message demo --max 3 --log "$tmp/run.log" --report "$tmp/report.md" \
    --events "$tmp/events.jsonl" >"$tmp/out" 2>&1 \
    || fail "driver exited $?"

grep -q "finalized: demo" "$tmp/out" || fail "driver did not finalize"
[ "$(git -C "$repo" rev-list --count HEAD)" -eq 3 ] || fail "expected base + 2 versions"
[ "$(git -C "$repo" tag | wc -l)" -eq 2 ] || fail "expected 2 tags"
[ "$(cat "$repo/out.txt")" = v2 ] || fail "agent output is not v2"
grep -q "round=1 " "$tmp/run.log" || fail "log misses round 1"
grep -q "round=2 " "$tmp/run.log" || fail "log misses round 2"
grep -q "verdict=approved" "$tmp/run.log" || fail "log misses approved verdict"
grep -q "action=finalize" "$tmp/run.log" || fail "log misses finalize"
[ -s "$tmp/report.md" ] || fail "report is empty"
grep -q "^# auto-iterate report" "$tmp/report.md" || fail "report header missing"
grep -q "| 1 | v1-1 | approved | start_round |" "$tmp/report.md" || fail "report row 1 missing"
grep -q "| 2 | v2-1 | approved | finalize |" "$tmp/report.md" || fail "report row 2 missing"
grep -q "^- finished: " "$tmp/report.md" || fail "report footer missing"
python3 - "$tmp/events.jsonl" <<'PY' || fail "events are incomplete"
import json, sys
ev = [json.loads(line) for line in open(sys.argv[1])]
kinds = [e["event"] for e in ev]
for kind in ("start", "round", "review", "approved", "record", "finalized"):
    assert kind in kinds, (kind, kinds)
PY

repo2=$tmp/repo2
state2=$tmp/state2
mkdir -p "$repo2"
git -C "$repo2" init -q
git -C "$repo2" config user.email test@example.com
git -C "$repo2" config user.name test
echo base > "$repo2/f"
git -C "$repo2" add f
git -C "$repo2" commit -qm base
echo 0 > "$state2"

cat > "$tmp/hang.sh" <<'AG'
#!/bin/sh
sleep 5
AG
chmod +x "$tmp/hang.sh"

start=$(date +%s)
set +e
STUB_REPO=$repo2 STUB_STATE=$state2 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task hang --agent-cmd "$tmp/hang.sh" \
    --agent-timeout 1 --max 1 >"$tmp/out2" 2>&1
rc=$?
set -e
elapsed=$(( $(date +%s) - start ))
[ "$rc" -eq 1 ] || fail "timeout case exited $rc"
[ "$elapsed" -le 3 ] || fail "timeout case took ${elapsed}s"
grep -q "agent timed out after 1s" "$tmp/out2" || fail "no timeout message"

repo3=$tmp/repo3
state3=$tmp/state3
mkdir -p "$repo3"
git -C "$repo3" init -q
git -C "$repo3" config user.email test@example.com
git -C "$repo3" config user.name test
echo base > "$repo3/f"
git -C "$repo3" add f
git -C "$repo3" commit -qm base
echo 0 > "$state3"

start=$(date +%s)
set +e
STUB_REPO=$repo3 STUB_STATE=$state3 STUB_SLOW_REVIEW=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task slow --agent-cmd "$tmp/agent.sh" \
    --review-timeout 1 --max 1 >"$tmp/out3" 2>&1
rc=$?
set -e
elapsed=$(( $(date +%s) - start ))
[ "$rc" -eq 3 ] || fail "review timeout case exited $rc"
[ "$elapsed" -le 3 ] || fail "review timeout case took ${elapsed}s"
grep -q "review timed out after 1s" "$tmp/out3" || fail "no review timeout message"
[ "$(git -C "$repo3" rev-list --count HEAD)" -eq 1 ] || fail "review timeout committed"

echo "test-driver: ok"
