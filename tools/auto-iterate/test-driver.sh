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
    review) printf '{"record":{"id":"v%s-1","verdict":"approved","text":"ok"}}\n' "$((n+1))" ;;
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
    --message demo --max 3 --log "$tmp/run.log" >"$tmp/out" 2>&1 \
    || fail "driver exited $?"

grep -q "finalized: demo" "$tmp/out" || fail "driver did not finalize"
[ "$(git -C "$repo" rev-list --count HEAD)" -eq 3 ] || fail "expected base + 2 versions"
[ "$(git -C "$repo" tag | wc -l)" -eq 2 ] || fail "expected 2 tags"
[ "$(cat "$repo/out.txt")" = v2 ] || fail "agent output is not v2"
grep -q "round=1 " "$tmp/run.log" || fail "log misses round 1"
grep -q "round=2 " "$tmp/run.log" || fail "log misses round 2"
grep -q "verdict=approved" "$tmp/run.log" || fail "log misses approved verdict"
grep -q "action=finalize" "$tmp/run.log" || fail "log misses finalize"

echo "test-driver: ok"
