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
    task)
        if [ -f "$state.done" ]; then st=done
        elif [ -f "$state.paused" ]; then st=paused
        else st=running; fi
        if [ -n "${STUB_HISTORY:-}" ] && [ -f "$STUB_HISTORY" ]; then hist=$(cat "$STUB_HISTORY"); else hist='[]'; fi
        printf '{"status":"%s","current_iteration":%s,"repo":"%s","stop_reason":null,"goal":"g","final_acceptance":"a","history":%s}\n' "$st" "$((n+1))" "$STUB_REPO" "$hist"
        ;;
    stage)
        if [ "${STUB_FAIL_STAGE:-0}" = 1 ] && [ ! -f "$state.stage_failed" ]; then
            : > "$state.stage_failed"
            exit 1
        fi
        git -C "$STUB_REPO" add -A
        printf '{"tree":"t","base":"b"}\n'
        ;;
    pubkey) printf 'stub-pub\n' ;;
    verify-range)
        [ -z "${STUB_VERIFY_RANGE_ARGS:-}" ] || printf '%s\n' "$*" > "$STUB_VERIFY_RANGE_ARGS"
        [ "${STUB_FAIL_VERIFY_RANGE:-0}" != 1 ] || exit 1
        printf 'verified\n'
        ;;
    review)
        case "$2" in
            run)
                if [ "${STUB_RUN_NO_AI:-0}" = 1 ]; then exit 3; fi
                if [ "${STUB_SLOW_REVIEW:-0}" = 1 ]; then
                    [ -z "${STUB_REVIEW_PID:-}" ] || printf '%s\n' "$$" > "$STUB_REVIEW_PID"
                    exec sleep 5
                fi
                if [ -n "${STUB_ARGS:-}" ]; then printf '%s\n' "$*" > "$STUB_ARGS"; fi
                if [ -n "${STUB_EVIDENCE_COPY:-}" ]; then
                    prev=
                    for a in "$@"; do
                        if [ "$prev" = "--evidence" ]; then cp "$a" "$STUB_EVIDENCE_COPY"; fi
                        prev=$a
                    done
                fi
                if [ "${STUB_REJECT:-0}" = 1 ]; then v=rejected; t=no
                elif [ "${STUB_REJECT_FIRST:-0}" = 1 ] && [ ! -f "$state.rejected_once" ]; then
                    v=rejected; t=first-no; : > "$state.rejected_once"
                else v=approved; t=ok; fi
                printf '%s\n' "$v" > "$state.verdict"
                printf '{"record":{"id":"v%s-1","verdict":"%s","text":"%s","channel":"stub","provider":"stub-provider","model":"stub-model"}}\n' "$((n+1))" "$v" "$t"
                ;;
            submit)
                if [ -n "${STUB_SUBMIT_ARGS:-}" ]; then printf '%s\n' "$*" > "$STUB_SUBMIT_ARGS"; fi
                printf '{"id":"fallback-1","verdict":"approved","text":"ok","channel":"claude-subagent","provider":"claude-subagent","model":null}\n'
                ;;
            *) echo "stub: unsupported review $*" >&2; exit 1 ;;
        esac
        ;;
    record)
        if [ "${STUB_FAIL_RECORD:-0}" = 1 ] && [ ! -f "${STUB_FAIL_MARKER:-/nonexistent}" ]; then
            : > "${STUB_FAIL_MARKER:-/dev/null}"
            exit 1
        fi
        if [ "$(cat "$state.verdict" 2>/dev/null || true)" = rejected ]; then
            if [ "${STUB_PAUSE_REJECT:-0}" = 1 ]; then
                : > "$state.paused"
                printf '{"action":"pause"}\n'
            else
                printf '{"action":"retry_same_round"}\n'
            fi
            exit 0
        fi
        if [ "$((n+1))" -ge "${STUB_ITERATIONS:-2}" ]; then act=finalize; else act=start_round; fi
        printf '{"action":"%s"}\n' "$act"
        echo $((n+1)) > "$state"
        if [ "$act" = finalize ]; then : > "$state.done"; fi
        ;;
    *) echo "stub: unsupported $*" >&2; exit 1 ;;
esac
STUB
chmod +x "$bin/review-gate"

cat > "$tmp/fallback.sh" <<'AG'
#!/bin/sh
set -eu
printf 'VERDICT: approved\nfallback review\n' > "$AUTO_ITERATE_REVIEW_FILE"
[ -z "${STUB_FALLBACK_DIFF:-}" ] || cp "$AUTO_ITERATE_DIFF" "$STUB_FALLBACK_DIFF"
[ -z "${STUB_FALLBACK_ENV:-}" ] || printf '%s|%s|%s|%s\n' "$AUTO_ITERATE_GOAL" "$AUTO_ITERATE_ACCEPTANCE" "$AUTO_ITERATE_TASK" "$AUTO_ITERATE_ITERATION" > "$STUB_FALLBACK_ENV"
AG
chmod +x "$tmp/fallback.sh"

cat > "$tmp/agent.sh" <<'AG'
#!/bin/sh
set -eu
echo "v$AUTO_ITERATE_ITERATION" > "$AUTO_ITERATE_REPO/out.txt"
printf '%s\n' "${AUTO_ITERATE_GOAL:-}" > "$AUTO_ITERATE_REPO/goal.txt"
printf '%s\n' "${AUTO_ITERATE_ACCEPTANCE:-}" > "$AUTO_ITERATE_REPO/acceptance.txt"
printf '%s\n' "${AUTO_ITERATE_EVIDENCE:-}" > "$AUTO_ITERATE_REPO/evidence.txt"
AG
chmod +x "$tmp/agent.sh"

cat > "$tmp/notify.sh" <<'AG'
#!/bin/sh
set -eu
{
    printf 'event=%s\n' "${AUTO_ITERATE_EVENT:-}"
    printf 'exit_code=%s\n' "${AUTO_ITERATE_EXIT_CODE:-}"
    printf 'reason=%s\n' "${AUTO_ITERATE_REASON:-}"
    printf 'task=%s\n' "${AUTO_ITERATE_TASK:-}"
    printf 'iteration=%s\n' "${AUTO_ITERATE_ITERATION:-}"
    printf 'commit=%s\n' "${AUTO_ITERATE_COMMIT:-}"
    printf 'tag=%s\n' "${AUTO_ITERATE_TAG:-}"
    printf 'review=%s\n' "${AUTO_ITERATE_REVIEW:-}"
    printf 'verdict=%s\n' "${AUTO_ITERATE_VERDICT:-}"
    printf 'channel=%s\n' "${AUTO_ITERATE_CHANNEL:-}"
    printf 'message=%s\n' "${AUTO_ITERATE_MESSAGE:-}"
} > "${STUB_NOTIFY_OUT:?}"
AG
chmod +x "$tmp/notify.sh"

STUB_REPO=$repo STUB_STATE=$state STUB_ARGS=$tmp/args PATH="$bin:$PATH" \
    STUB_NOTIFY_OUT=$tmp/notify.out STUB_VERIFY_RANGE_ARGS=$tmp/vrargs \
    "$here/auto-iterate.sh" --task demo --agent-cmd "$tmp/agent.sh" \
    --message demo --max 3 --log "$tmp/run.log" --report "$tmp/report.md" \
    --events "$tmp/events.jsonl" --provider fake --evidence "$tmp/evidence" \
    --verify-cmd true --notify-cmd "$tmp/notify.sh" --verify-range >"$tmp/out" 2>&1 \
    || fail "driver exited $?"

grep -q "finalized: demo" "$tmp/out" || fail "driver did not finalize"
[ "$(git -C "$repo" rev-list --count HEAD)" -eq 3 ] || fail "expected base + 2 versions"
[ "$(git -C "$repo" tag | wc -l)" -eq 2 ] || fail "expected 2 tags"
[ "$(cat "$repo/out.txt")" = v2 ] || fail "agent output is not v2"
[ "$(cat "$repo/goal.txt")" = g ] || fail "goal was not injected"
[ "$(cat "$repo/acceptance.txt")" = a ] || fail "acceptance was not injected"
[ "$(cat "$repo/evidence.txt")" = "$tmp/evidence" ] || fail "evidence path was not injected"
grep -q "round=1 " "$tmp/run.log" || fail "log misses round 1"
grep -q "round=2 " "$tmp/run.log" || fail "log misses round 2"
grep -q "verdict=approved" "$tmp/run.log" || fail "log misses approved verdict"
grep -q "channel=stub" "$tmp/run.log" || fail "log misses the reviewer channel"
grep -q -- "--provider fake" "$tmp/args" || fail "provider was not forwarded"
grep -q "| stub |" "$tmp/report.md" || fail "report misses the reviewer channel"
grep -q "action=finalize" "$tmp/run.log" || fail "log misses finalize"
[ -s "$tmp/report.md" ] || fail "report is empty"
grep -q "^# auto-iterate report" "$tmp/report.md" || fail "report header missing"
grep -q "| 1 | v1-1 | approved | start_round |" "$tmp/report.md" || fail "report row 1 missing"
grep -q "| 2 | v2-1 | approved | finalize |" "$tmp/report.md" || fail "report row 2 missing"
grep -q "^- finished: " "$tmp/report.md" || fail "report footer missing"
grep -q "duration" "$tmp/report.md" || fail "report misses the duration column"
grep -Eq '\| [0-9]+s \|$' "$tmp/report.md" || fail "report row misses the duration"
python3 - "$tmp/events.jsonl" <<'PY' || fail "events are incomplete"
import json, sys
ev = [json.loads(line) for line in open(sys.argv[1])]
kinds = [e["event"] for e in ev]
for kind in ("start", "round", "verify", "review", "approved", "record", "finalized"):
    assert kind in kinds, (kind, kinds)
verify = next(e for e in ev if e["event"] == "verify")
assert verify["ok"] == "true", verify
review = next(e for e in ev if e["event"] == "review")
assert review["channel"] == "stub", review
assert review["provider"] == "stub-provider", review
done = next(e for e in ev if e["event"] == "round_done")
assert done["duration"].isdigit(), done
PY

gitdir_main=$(git -C "$repo" rev-parse --absolute-git-dir)
manifest_main=$gitdir_main/auto-iterate/versions.ndjson
python3 - "$manifest_main" <<'PY' || fail "versions.ndjson is incomplete"
import json, sys
rows = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
assert len(rows) == 2, rows
for i, row in enumerate(rows, 1):
    assert row["task"] == "demo", row
    assert row["version"] == i, row
    assert row["iteration"] == i, row
    assert row["review"] == f"v{i}-1", row
    assert row["verdict"] == "approved", row
    assert row["tag"] == f"auto-iterate/demo/v{i}", row
    assert row["channel"] == "stub", row
    assert row["provider"] == "stub-provider", row
    assert row["message"] == "demo", row
    assert len(row["commit"]) == 40, row
    assert row["at"].endswith("Z"), row
PY
grep -q '^event=finalized$' "$tmp/notify.out" || fail "notify event is not finalized"
grep -q '^exit_code=0$' "$tmp/notify.out" || fail "notify exit code is wrong"
grep -q '^task=demo$' "$tmp/notify.out" || fail "notify task is wrong"
grep -q '^iteration=2$' "$tmp/notify.out" || fail "notify iteration is wrong"
grep -q '^tag=auto-iterate/demo/v2$' "$tmp/notify.out" || fail "notify tag is wrong"
grep -q '^review=v2-1$' "$tmp/notify.out" || fail "notify review is wrong"
grep -q '^verdict=approved$' "$tmp/notify.out" || fail "notify verdict is wrong"
grep -q '^channel=stub$' "$tmp/notify.out" || fail "notify channel is wrong"
grep -q -- '--pubkey stub-pub' "$tmp/vrargs" || fail "verify-range pubkey was not forwarded"
grep -Eq '[0-9a-f]{40}\.\.HEAD$' "$tmp/vrargs" || fail "verify-range got no range"
root_main=$(git -C "$repo" rev-list --max-parents=0 HEAD | head -n 1)
grep -q " $root_main\.\.HEAD$" "$tmp/vrargs" || fail "verify-range used the wrong base"

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

repo5=$tmp/repo5
state5=$tmp/state5
mkdir -p "$repo5"
git -C "$repo5" init -q
git -C "$repo5" config user.email test@example.com
git -C "$repo5" config user.name test
echo base > "$repo5/f"
git -C "$repo5" add f
git -C "$repo5" commit -qm base
echo 0 > "$state5"

set +e
STUB_REPO=$repo5 STUB_STATE=$state5 STUB_REJECT=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task stop --agent-cmd "$tmp/agent.sh" \
    --stop-on-reject --max 2 >"$tmp/out5" 2>&1
rc=$?
set -e
[ "$rc" -eq 5 ] || fail "stop-on-reject exited $rc"
grep -q "stopped after rejected review" "$tmp/out5" || fail "no stop-on-reject message"
[ "$(git -C "$repo5" rev-list --count HEAD)" -eq 1 ] || fail "stop-on-reject committed"
[ "$(git -C "$repo5" tag | wc -l)" -eq 0 ] || fail "stop-on-reject tagged"

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

repo6=$tmp/repo6
state6=$tmp/state6
mkdir -p "$repo6"
git -C "$repo6" init -q
git -C "$repo6" config user.email test@example.com
git -C "$repo6" config user.name test
echo base > "$repo6/f"
git -C "$repo6" add f
git -C "$repo6" commit -qm base
echo dirty > "$repo6/dirty.txt"
echo 0 > "$state6"

set +e
STUB_REPO=$repo6 STUB_STATE=$state6 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --check --task check >"$tmp/out6" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "dirty check exited $rc"
grep -q "uncommitted changes" "$tmp/out6" || fail "no dirty check message"

out=$(STUB_REPO=$repo6 STUB_STATE=$state6 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --check --allow-dirty --task check 2>&1) || fail "allow-dirty check failed"
printf '%s' "$out" | grep -q "check ok" || fail "allow-dirty check output"

repo7=$tmp/repo7
state7=$tmp/state7
mkdir -p "$repo7"
git -C "$repo7" init -q
git -C "$repo7" config user.email test@example.com
git -C "$repo7" config user.name test
echo base > "$repo7/f"
git -C "$repo7" add f
git -C "$repo7" commit -qm base
echo 0 > "$state7"

set +e
STUB_REPO=$repo7 STUB_STATE=$state7 STUB_FAIL_RECORD=1 STUB_FAIL_MARKER=$tmp/marker \
    PATH="$bin:$PATH" "$here/auto-iterate.sh" --task resume7 --agent-cmd "$tmp/agent.sh" \
    --message "resume v1" --max 1 >"$tmp/out7" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "failed record exited $rc"
[ "$(git -C "$repo7" rev-list --count HEAD)" -eq 2 ] || fail "failed record did not commit"
[ "$(git -C "$repo7" tag | wc -l)" -eq 1 ] || fail "failed record did not tag"
gitdir7=$(git -C "$repo7" rev-parse --absolute-git-dir)
state7file=$gitdir7/auto-iterate/resume7.json
[ -f "$state7file" ] || fail "no pending state after failed record"
python3 - "$state7file" <<'PY' || fail "pending state is wrong"
import json, sys
d = json.load(open(sys.argv[1]))
assert d["phase"] == "committed", d
assert d["iteration"] == 1, d
assert d["review"] == "v1-1", d
PY
set +e
STUB_REPO=$repo7 STUB_STATE=$state7 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume7 --agent-cmd "$tmp/agent.sh" \
    --max 1 >"$tmp/out7b" 2>&1
rc=$?
set -e
[ "$rc" -eq 2 ] || fail "pending state without --resume exited $rc"
grep -q "pending state" "$tmp/out7b" || fail "no pending state message"
STUB_REPO=$repo7 STUB_STATE=$state7 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume7 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out7c" 2>&1 || fail "resume committed failed"
grep -q "task resume7 is done" "$tmp/out7c" || fail "resume did not observe done"
[ ! -f "$state7file" ] || fail "state not cleared after resume"
manifest7=$gitdir7/auto-iterate/versions.ndjson
python3 - "$manifest7" <<'PY' || fail "resume did not append versions.ndjson"
import json, sys
rows = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
assert len(rows) == 1, rows
assert rows[0]["task"] == "resume7", rows
assert rows[0]["version"] == 1, rows
assert rows[0]["review"] == "v1-1", rows
PY

repo8=$tmp/repo8
state8=$tmp/state8
mkdir -p "$repo8"
git -C "$repo8" init -q
git -C "$repo8" config user.email test@example.com
git -C "$repo8" config user.name test
echo base > "$repo8/f"
git -C "$repo8" add f
git -C "$repo8" commit -qm base
echo change > "$repo8/f2"
git -C "$repo8" add f2
echo 0 > "$state8"
gitdir8=$(git -C "$repo8" rev-parse --absolute-git-dir)
mkdir -p "$gitdir8/auto-iterate"
printf '{"phase":"reviewed","iteration":1,"review":"v1-1","commit":"","tag":"","message":"resume reviewed v1","channel":"stub"}\n' \
    > "$gitdir8/auto-iterate/resume8.json"
STUB_REPO=$repo8 STUB_STATE=$state8 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume8 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out8" 2>&1 || fail "resume reviewed failed"
[ "$(git -C "$repo8" rev-list --count HEAD)" -eq 2 ] || fail "resume reviewed did not commit"
[ "$(git -C "$repo8" show HEAD:f2)" = change ] || fail "resume reviewed lost the staged change"
[ "$(git -C "$repo8" tag)" = "auto-iterate/resume8/v1" ] || fail "resume reviewed tag missing"
[ ! -f "$gitdir8/auto-iterate/resume8.json" ] || fail "reviewed state not cleared"

repo9=$tmp/repo9
state9=$tmp/state9
mkdir -p "$repo9"
git -C "$repo9" init -q
git -C "$repo9" config user.email test@example.com
git -C "$repo9" config user.name test
echo base > "$repo9/f"
git -C "$repo9" add f
git -C "$repo9" commit -qm base
echo 0 > "$state9"

STUB_REPO=$repo9 STUB_STATE=$state9 STUB_ITERATIONS=1 STUB_RUN_NO_AI=1 \
    STUB_SUBMIT_ARGS=$tmp/subargs STUB_FALLBACK_DIFF=$tmp/fbdiff STUB_FALLBACK_ENV=$tmp/fbenv \
    PATH="$bin:$PATH" "$here/auto-iterate.sh" --task fallback9 --agent-cmd "$tmp/agent.sh" \
    --fallback-cmd "$tmp/fallback.sh" --fallback-channel claude-subagent \
    --log "$tmp/run9.log" --max 1 >"$tmp/out9" 2>&1 || fail "fallback run failed"
[ "$(git -C "$repo9" rev-list --count HEAD)" -eq 2 ] || fail "fallback did not commit"
[ "$(git -C "$repo9" tag)" = "auto-iterate/fallback9/v1" ] || fail "fallback tag missing"
grep -q -- "--channel claude-subagent" "$tmp/subargs" || fail "fallback channel not forwarded"
grep -q "out.txt" "$tmp/fbdiff" || fail "fallback did not receive the staged diff"
[ "$(cat "$tmp/fbenv")" = "g|a|fallback9|1" ] || fail "fallback context is wrong"
grep -q "fallback submitted" "$tmp/run9.log" || fail "fallback not logged"

repo10=$tmp/repo10
state10=$tmp/state10
mkdir -p "$repo10"
git -C "$repo10" init -q
git -C "$repo10" config user.email test@example.com
git -C "$repo10" config user.name test
echo base > "$repo10/f"
git -C "$repo10" add f
git -C "$repo10" commit -qm base
echo 0 > "$state10"
set +e
STUB_REPO=$repo10 STUB_STATE=$state10 STUB_RUN_NO_AI=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task noai10 --agent-cmd "$tmp/agent.sh" --max 1 >"$tmp/out10" 2>&1
rc=$?
set -e
[ "$rc" -eq 3 ] || fail "no-ai without fallback exited $rc"
grep -q "no external AI available" "$tmp/out10" || fail "no no-ai message"
[ "$(git -C "$repo10" rev-list --count HEAD)" -eq 1 ] || fail "no-ai run committed"

repo4=$tmp/repo4
state4=$tmp/state4
mkdir -p "$repo4"
git -C "$repo4" init -q
git -C "$repo4" config user.email test@example.com
git -C "$repo4" config user.name test
echo base > "$repo4/f"
git -C "$repo4" add f
git -C "$repo4" commit -qm base
echo 0 > "$state4"

out=$(STUB_REPO=$repo4 STUB_STATE=$state4 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --check --task check 2>&1) || fail "check mode failed"
printf '%s' "$out" | grep -q "check ok: task=check" || fail "no check output"
[ "$(git -C "$repo4" rev-list --count HEAD)" -eq 1 ] || fail "check mode changed the repo"

# --- v13: repo flock lease ---
repo11=$tmp/repo11
state11=$tmp/state11
mkdir -p "$repo11"
git -C "$repo11" init -q
git -C "$repo11" config user.email test@example.com
git -C "$repo11" config user.name test
echo base > "$repo11/f"
git -C "$repo11" add f
git -C "$repo11" commit -qm base
echo 0 > "$state11"
gitdir11=$(git -C "$repo11" rev-parse --absolute-git-dir)
lock11=$gitdir11/auto-iterate/lock
mkdir -p "$gitdir11/auto-iterate"
(
    flock -n 9 || exit 97
    sleep 5
) 9>"$lock11" &
holder11=$!
sleep 1
now11=$(date +%s)
printf '{"owner":"other:1","host":"%s","pid":%s,"task":"other","iteration":1,"started":%s,"updated":%s}\n' \
    "$(hostname)" "$holder11" "$now11" "$now11" > "$lock11.meta"
set +e
STUB_REPO=$repo11 STUB_STATE=$state11 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task lock11 --agent-cmd "$tmp/agent.sh" --max 1 >"$tmp/out11" 2>&1
rc=$?
set -e
[ "$rc" -eq 2 ] || fail "active lock exited $rc"
grep -q "repo is locked by other" "$tmp/out11" || fail "no active-lock message"
[ "$(git -C "$repo11" rev-list --count HEAD)" -eq 1 ] || fail "active lock still committed"
kill "$holder11" 2>/dev/null || true
wait "$holder11" 2>/dev/null || true
STUB_REPO=$repo11 STUB_STATE=$state11 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task lock11 --agent-cmd "$tmp/agent.sh" --max 1 >"$tmp/out11b" 2>&1 \
    || fail "post-lock run failed"
[ "$(git -C "$repo11" rev-list --count HEAD)" -eq 2 ] || fail "post-lock run did not commit"
[ ! -f "$lock11.meta" ] || fail "lock metadata not removed after run"

# --- v13: verify command ---
repo12=$tmp/repo12
state12=$tmp/state12
mkdir -p "$repo12"
git -C "$repo12" init -q
git -C "$repo12" config user.email test@example.com
git -C "$repo12" config user.name test
echo base > "$repo12/f"
git -C "$repo12" add f
git -C "$repo12" commit -qm base
echo 0 > "$state12"
STUB_REPO=$repo12 STUB_STATE=$state12 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task verify12 --agent-cmd "$tmp/agent.sh" \
    --verify-cmd 'test -f out.txt && echo verify-ok' --evidence "$tmp/evidence12" \
    --log "$tmp/run12.log" --max 1 >"$tmp/out12" 2>&1 || fail "verify success run failed"
[ "$(git -C "$repo12" rev-list --count HEAD)" -eq 2 ] || fail "verify success did not commit"
grep -q "verify ok" "$tmp/run12.log" || fail "verify success not logged"
grep -q "verify-ok" "$tmp/evidence12" || fail "verify output not in evidence"
grep -q "== verify (exit 0)" "$tmp/evidence12" || fail "verify status not in evidence"

repo13=$tmp/repo13
state13=$tmp/state13
mkdir -p "$repo13"
git -C "$repo13" init -q
git -C "$repo13" config user.email test@example.com
git -C "$repo13" config user.name test
echo base > "$repo13/f"
git -C "$repo13" add f
git -C "$repo13" commit -qm base
echo 0 > "$state13"
set +e
STUB_REPO=$repo13 STUB_STATE=$state13 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task verify13 --agent-cmd "$tmp/agent.sh" \
    --verify-cmd 'echo no >&2; exit 7' --max 1 >"$tmp/out13" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "verify failure exited $rc"
grep -q "verify command failed (exit 7)" "$tmp/out13" || fail "no verify failure message"
[ "$(git -C "$repo13" rev-list --count HEAD)" -eq 1 ] || fail "verify failure committed"
[ "$(git -C "$repo13" tag | wc -l)" -eq 0 ] || fail "verify failure tagged"
gitdir13=$(git -C "$repo13" rev-parse --absolute-git-dir)
[ -f "$gitdir13/auto-iterate/verify13.json" ] || fail "verify failure dropped state"
grep -q '"phase": "verify"' "$gitdir13/auto-iterate/verify13.json" || fail "verify failure state phase is not verify"

# --- v13: per-version acceptance plan ---
repo14=$tmp/repo14
state14=$tmp/state14
mkdir -p "$repo14"
git -C "$repo14" init -q
git -C "$repo14" config user.email test@example.com
git -C "$repo14" config user.name test
echo base > "$repo14/f"
git -C "$repo14" add f
git -C "$repo14" commit -qm base
echo 0 > "$state14"
cat > "$tmp/plan14.json" <<'JSON'
{"1":"first acceptance","2":"second acceptance"}
JSON
cat > "$tmp/agent14.sh" <<'AG'
#!/bin/sh
set -eu
echo "$AUTO_ITERATE_VERSION_ACCEPTANCE" > "$AUTO_ITERATE_REPO/va-$AUTO_ITERATE_ITERATION.txt"
echo "v$AUTO_ITERATE_ITERATION" > "$AUTO_ITERATE_REPO/out.txt"
AG
chmod +x "$tmp/agent14.sh"
STUB_REPO=$repo14 STUB_STATE=$state14 STUB_ITERATIONS=2 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task plan14 --agent-cmd "$tmp/agent14.sh" \
    --version-plan "$tmp/plan14.json" --evidence "$tmp/evidence14" --max 2 >"$tmp/out14" 2>&1 \
    || fail "version plan run failed"
[ "$(cat "$repo14/va-1.txt")" = "first acceptance" ] || fail "version 1 acceptance wrong"
[ "$(cat "$repo14/va-2.txt")" = "second acceptance" ] || fail "version 2 acceptance wrong"
grep -q "## Version 2 acceptance" "$tmp/evidence14" || fail "version acceptance not in evidence"
grep -q "second acceptance" "$tmp/evidence14" || fail "version acceptance text missing from evidence"

# --- v13: rejected untracked cleanup and feedback clearing ---
repo15=$tmp/repo15
state15=$tmp/state15
mkdir -p "$repo15"
git -C "$repo15" init -q
git -C "$repo15" config user.email test@example.com
git -C "$repo15" config user.name test
echo base > "$repo15/f"
git -C "$repo15" add f
git -C "$repo15" commit -qm base
echo 0 > "$state15"
cat > "$tmp/agent15.sh" <<'AG'
#!/bin/sh
set -eu
it=$AUTO_ITERATE_ITERATION
if [ -s "$AUTO_ITERATE_FEEDBACK" ]; then
    cp "$AUTO_ITERATE_FEEDBACK" "feedback-$it.txt"
else
    echo junk > "junk-$it.txt"
fi
echo "v$it" > out.txt
AG
chmod +x "$tmp/agent15.sh"
STUB_REPO=$repo15 STUB_STATE=$state15 STUB_REJECT_FIRST=1 STUB_ITERATIONS=2 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task reject15 --agent-cmd "$tmp/agent15.sh" --max 3 >"$tmp/out15" 2>&1 \
    || fail "reject/retry/finalize run failed"
[ ! -e "$repo15/junk-1.txt" ] || fail "junk-1 survived the same-version retry"
grep -q "first-no" "$repo15/feedback-1.txt" || fail "rejection feedback was not delivered on the same-version retry"
[ -e "$repo15/feedback-2.txt" ] && fail "feedback was not cleared after approval"
[ "$(git -C "$repo15" rev-list --count HEAD)" -eq 3 ] || fail "reject/retry did not commit 2 approved versions"
gitdir15=$(git -C "$repo15" rev-parse --absolute-git-dir)
[ ! -e "$gitdir15/auto-iterate/reject15.feedback" ] || fail "durable feedback survived approval"

# --- v13: resume a staged tree without rerunning the agent ---
repo16=$tmp/repo16
state16=$tmp/state16
mkdir -p "$repo16"
git -C "$repo16" init -q
git -C "$repo16" config user.email test@example.com
git -C "$repo16" config user.name test
echo base > "$repo16/f"
git -C "$repo16" add f
git -C "$repo16" commit -qm base
echo change > "$repo16/f2"
git -C "$repo16" add f2
echo 0 > "$state16"
gitdir16=$(git -C "$repo16" rev-parse --absolute-git-dir)
base16=$(git -C "$repo16" rev-parse HEAD)
mkdir -p "$gitdir16/auto-iterate"
printf '{"phase":"staged","iteration":1,"review":"","commit":"","tag":"","message":"staged v1","channel":"","head":"%s","clean":1}\n' \
    "$base16" > "$gitdir16/auto-iterate/resume16.json"
STUB_REPO=$repo16 STUB_STATE=$state16 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume16 --agent-cmd 'echo agent-ran > agent-ran.txt' \
    --resume --max 1 >"$tmp/out16" 2>&1 || fail "staged resume failed"
[ "$(git -C "$repo16" rev-list --count HEAD)" -eq 2 ] || fail "staged resume did not commit"
[ "$(git -C "$repo16" show HEAD:f2)" = change ] || fail "staged resume lost the change"
[ "$(git -C "$repo16" tag)" = "auto-iterate/resume16/v1" ] || fail "staged resume tag missing"
[ ! -e "$repo16/agent-ran.txt" ] || fail "staged resume reran the agent"
[ ! -f "$gitdir16/auto-iterate/resume16.json" ] || fail "staged resume state not cleared"

repo17=$tmp/repo17
state17=$tmp/state17
mkdir -p "$repo17"
git -C "$repo17" init -q
git -C "$repo17" config user.email test@example.com
git -C "$repo17" config user.name test
echo base > "$repo17/f"
git -C "$repo17" add f
git -C "$repo17" commit -qm base
echo change > "$repo17/f2"
git -C "$repo17" add f2
echo 0 > "$state17"
gitdir17=$(git -C "$repo17" rev-parse --absolute-git-dir)
base17=$(git -C "$repo17" rev-parse HEAD)
mkdir -p "$gitdir17/auto-iterate"
printf '{"phase":"reviewing","iteration":1,"review":"","commit":"","tag":"","message":"reviewing v1","channel":"","head":"%s","clean":1}\n' \
    "$base17" > "$gitdir17/auto-iterate/resume17.json"
STUB_REPO=$repo17 STUB_STATE=$state17 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume17 --agent-cmd 'echo agent-ran > agent-ran.txt' \
    --resume --max 1 >"$tmp/out17" 2>&1 || fail "reviewing resume failed"
[ "$(git -C "$repo17" rev-list --count HEAD)" -eq 2 ] || fail "reviewing resume did not commit"
[ "$(git -C "$repo17" show HEAD:f2)" = change ] || fail "reviewing resume lost the change"
[ ! -e "$repo17/agent-ran.txt" ] || fail "reviewing resume reran the agent"

repo18=$tmp/repo18
state18=$tmp/state18
mkdir -p "$repo18"
git -C "$repo18" init -q
git -C "$repo18" config user.email test@example.com
git -C "$repo18" config user.name test
echo base > "$repo18/f"
git -C "$repo18" add f
git -C "$repo18" commit -qm base
echo change > "$repo18/f2"
git -C "$repo18" add f2
echo 0 > "$state18"
gitdir18=$(git -C "$repo18" rev-parse --absolute-git-dir)
mkdir -p "$gitdir18/auto-iterate"
printf '{"phase":"staged","iteration":1,"review":"","commit":"","tag":"","message":"staged v1","channel":"","head":"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef","clean":1}\n' \
    > "$gitdir18/auto-iterate/resume18.json"
set +e
STUB_REPO=$repo18 STUB_STATE=$state18 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume18 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out18" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "changed-HEAD resume exited $rc"
grep -q "saved HEAD" "$tmp/out18" || fail "changed-HEAD guard message missing"
[ "$(git -C "$repo18" rev-list --count HEAD)" -eq 1 ] || fail "changed-HEAD resume committed"
[ -f "$gitdir18/auto-iterate/resume18.json" ] || fail "changed-HEAD state was removed"

# A record that succeeded just before the state removal must not be replayed.
repo19=$tmp/repo19
state19=$tmp/state19
mkdir -p "$repo19"
git -C "$repo19" init -q
git -C "$repo19" config user.email test@example.com
git -C "$repo19" config user.name test
echo base > "$repo19/f"
git -C "$repo19" add f
git -C "$repo19" commit -qm base
base19=$(git -C "$repo19" rev-parse HEAD)
echo one > "$repo19/f"
git -C "$repo19" add f
git -C "$repo19" commit -qm v1
commit19=$(git -C "$repo19" rev-parse HEAD)
tag19=auto-iterate/resume19/v1
git -C "$repo19" tag "$tag19"
gitdir19=$(git -C "$repo19" rev-parse --absolute-git-dir)
mkdir -p "$gitdir19/auto-iterate"
echo 1 > "$state19"
printf '[{"review":"v1-9","outcome":"approved"}]\n' > "$tmp/history19.json"
printf '{"phase":"committed","iteration":1,"review":"v1-9","commit":"%s","tag":"%s","message":"v1","channel":"stub","head":"%s","clean":1,"feedback":""}\n' \
    "$commit19" "$tag19" "$base19" > "$gitdir19/auto-iterate/resume19.json"
printf 'stale durable\n' > "$gitdir19/auto-iterate/resume19.feedback"
STUB_REPO=$repo19 STUB_STATE=$state19 STUB_HISTORY=$tmp/history19.json STUB_ITERATIONS=2 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume19 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 --log "$tmp/run19.log" >"$tmp/out19" 2>&1 || fail "already-recorded committed resume failed"
grep -q "already recorded" "$tmp/run19.log" || fail "committed record was replayed"
[ "$(git -C "$repo19" rev-list --count HEAD)" -eq 3 ] || fail "committed resume did not continue to v2"
[ "$(git -C "$repo19" tag | wc -l)" -eq 2 ] || fail "committed resume did not tag v2"
[ ! -e "$gitdir19/auto-iterate/resume19.feedback" ] || fail "committed resume kept stale durable feedback"

# A rejected record that succeeded must be finalized without replay, keeping feedback.
repo20=$tmp/repo20
state20=$tmp/state20
mkdir -p "$repo20"
git -C "$repo20" init -q
git -C "$repo20" config user.email test@example.com
git -C "$repo20" config user.name test
echo base > "$repo20/f"
git -C "$repo20" add f
git -C "$repo20" commit -qm base
base20=$(git -C "$repo20" rev-parse HEAD)
echo tracked > "$repo20/f"
gitdir20=$(git -C "$repo20" rev-parse --absolute-git-dir)
mkdir -p "$gitdir20/auto-iterate"
echo 0 > "$state20"
printf '[{"review":"v1-8","outcome":"rejected"}]\n' > "$tmp/history20.json"
python3 - "$gitdir20/auto-iterate/resume20.json" "$base20" <<'PY'
import json, sys
json.dump({"phase":"rejected","iteration":1,"review":"v1-8","commit":"","tag":"",
           "message":"v1","channel":"stub","head":sys.argv[2],"clean":1,
           "feedback":"fix from previous review"}, open(sys.argv[1],"w"))
PY
cat > "$tmp/agent20.sh" <<'AG'
#!/bin/sh
set -eu
cat "$AUTO_ITERATE_FEEDBACK" > "$AUTO_ITERATE_REPO/feedback.txt"
echo v1 > "$AUTO_ITERATE_REPO/out.txt"
AG
chmod +x "$tmp/agent20.sh"
STUB_REPO=$repo20 STUB_STATE=$state20 STUB_HISTORY=$tmp/history20.json STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume20 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 --log "$tmp/run20.log" >"$tmp/out20" 2>&1 || fail "already-recorded rejected resume failed"
grep -q "already recorded" "$tmp/run20.log" || fail "rejected record was replayed"
grep -q "fix from previous review" "$repo20/feedback.txt" || fail "rejection feedback was not restored"
[ "$(git -C "$repo20" rev-list --count HEAD)" -eq 2 ] || fail "rejected resume did not continue to v1"
[ "$(git -C "$repo20" show HEAD:f)" = tracked ] || fail "recovered tracked edit was not committed"

# A pending state from another task blocks the repository under the lock.
repo21=$tmp/repo21
state21=$tmp/state21
mkdir -p "$repo21"
git -C "$repo21" init -q
git -C "$repo21" config user.email test@example.com
git -C "$repo21" config user.name test
echo base > "$repo21/f"
git -C "$repo21" add f
git -C "$repo21" commit -qm base
gitdir21=$(git -C "$repo21" rev-parse --absolute-git-dir)
mkdir -p "$gitdir21/auto-iterate"
echo 0 > "$state21"
printf '{"phase":"staged"}\n' > "$gitdir21/auto-iterate/other-task.json"
set +e
STUB_REPO=$repo21 STUB_STATE=$state21 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume21 --agent-cmd "$tmp/agent.sh" --max 1 >"$tmp/out21" 2>&1
rc=$?
set -e
[ "$rc" -eq 2 ] || fail "other-task pending state exited $rc"
grep -q "pending state for another task" "$tmp/out21" || fail "other-task pending guard missing"
[ "$(git -C "$repo21" rev-list --count HEAD)" -eq 1 ] || fail "other-task pending state was ignored"

# A record that finalized the task still needs cleanup on --resume.
repo22=$tmp/repo22
state22=$tmp/state22
mkdir -p "$repo22"
git -C "$repo22" init -q
git -C "$repo22" config user.email test@example.com
git -C "$repo22" config user.name test
echo base > "$repo22/f"
git -C "$repo22" add f
git -C "$repo22" commit -qm base
base22=$(git -C "$repo22" rev-parse HEAD)
echo one > "$repo22/f"
git -C "$repo22" add f
git -C "$repo22" commit -qm v1
commit22=$(git -C "$repo22" rev-parse HEAD)
tag22=auto-iterate/resume22/v1
git -C "$repo22" tag "$tag22"
gitdir22=$(git -C "$repo22" rev-parse --absolute-git-dir)
mkdir -p "$gitdir22/auto-iterate"
echo 1 > "$state22"
: > "$state22.done"
printf '[{"review":"v1-7","outcome":"approved"}]\n' > "$tmp/history22.json"
printf '{"phase":"committed","iteration":1,"review":"v1-7","commit":"%s","tag":"%s","message":"v1","channel":"stub","head":"%s","clean":1,"feedback":""}\n' \
    "$commit22" "$tag22" "$base22" > "$gitdir22/auto-iterate/resume22.json"
STUB_REPO=$repo22 STUB_STATE=$state22 STUB_HISTORY=$tmp/history22.json PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume22 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out22" 2>&1 || fail "finalized pending resume failed"
[ ! -f "$gitdir22/auto-iterate/resume22.json" ] || fail "finalized pending state was not removed"
[ "$(git -C "$repo22" rev-list --count HEAD)" -eq 2 ] || fail "finalized pending resume changed history"

# Reviewed recovery must persist committed before record, so a record crash is replayable.
repo24=$tmp/repo24
state24=$tmp/state24
mkdir -p "$repo24"
git -C "$repo24" init -q
git -C "$repo24" config user.email test@example.com
git -C "$repo24" config user.name test
echo base > "$repo24/f"
git -C "$repo24" add f
git -C "$repo24" commit -qm base
base24=$(git -C "$repo24" rev-parse HEAD)
echo staged > "$repo24/f2"
git -C "$repo24" add f2
gitdir24=$(git -C "$repo24" rev-parse --absolute-git-dir)
mkdir -p "$gitdir24/auto-iterate"
echo 0 > "$state24"
printf '{"phase":"reviewed","iteration":1,"review":"v1-5","commit":"","tag":"","message":"v1","channel":"stub","head":"%s","clean":1,"feedback":""}\n' \
    "$base24" > "$gitdir24/auto-iterate/resume24.json"
set +e
STUB_REPO=$repo24 STUB_STATE=$state24 STUB_ITERATIONS=1 STUB_FAIL_RECORD=1 STUB_FAIL_MARKER=$tmp/fail24 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume24 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out24a" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "reviewed record failure exited $rc"
grep -q '"phase": "committed"' "$gitdir24/auto-iterate/resume24.json" || fail "reviewed recovery did not persist committed"
[ "$(git -C "$repo24" rev-list --count HEAD)" -eq 2 ] || fail "reviewed recovery did not commit"
STUB_REPO=$repo24 STUB_STATE=$state24 STUB_ITERATIONS=1 STUB_FAIL_RECORD=1 STUB_FAIL_MARKER=$tmp/fail24 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume24 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out24b" 2>&1 || fail "reviewed record replay failed"
[ ! -f "$gitdir24/auto-iterate/resume24.json" ] || fail "reviewed replay state was not removed"
[ "$(git -C "$repo24" tag | wc -l)" -eq 1 ] || fail "reviewed replay did not keep one tag"

# A killed driver must not let --resume race a still-running agent.
repo25=$tmp/repo25
state25=$tmp/state25
mkdir -p "$repo25"
git -C "$repo25" init -q
git -C "$repo25" config user.email test@example.com
git -C "$repo25" config user.name test
echo base > "$repo25/f"
git -C "$repo25" add f
git -C "$repo25" commit -qm base
echo 0 > "$state25"
cat > "$tmp/kill25.sh" <<'AG'
#!/bin/sh
set -eu
printf 'started\n' > "$KILL_MARKER"
sleep 2
printf 'late\n' > "$KILL_MARKER.late"
AG
chmod +x "$tmp/kill25.sh"
KILL_MARKER=$tmp/kill25.marker
export KILL_MARKER
STUB_REPO=$repo25 STUB_STATE=$state25 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task kill25 --agent-cmd "$tmp/kill25.sh" --max 1 >"$tmp/out25a" 2>&1 &
pid25=$!
i=0
while [ ! -f "$KILL_MARKER" ] && [ "$i" -lt 50 ]; do
    sleep 0.1
    i=$((i + 1))
done
[ -f "$KILL_MARKER" ] || fail "kill test agent did not start"
kill -9 "$pid25" 2>/dev/null || true
wait "$pid25" 2>/dev/null || true
set +e
STUB_REPO=$repo25 STUB_STATE=$state25 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task kill25 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out25b" 2>&1
rc=$?
set -e
[ "$rc" -eq 2 ] || fail "resume raced the surviving agent (exit $rc)"
grep -q "previous agent/verifier is still running" "$tmp/out25b" || fail "surviving-agent guard missing"
i=0
while [ ! -f "$KILL_MARKER.late" ] && [ "$i" -lt 50 ]; do
    sleep 0.1
    i=$((i + 1))
done
[ -f "$KILL_MARKER.late" ] || fail "surviving agent did not finish its delayed write"
STUB_REPO=$repo25 STUB_STATE=$state25 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task kill25 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out25c" 2>&1 || fail "resume failed after the surviving agent exited"
[ "$(git -C "$repo25" rev-list --count HEAD)" -eq 2 ] || fail "post-kill driver did not commit"

# Resumed staged trees rerun --verify-cmd before staging again.
repo26=$tmp/repo26
state26=$tmp/state26
mkdir -p "$repo26"
git -C "$repo26" init -q
git -C "$repo26" config user.email test@example.com
git -C "$repo26" config user.name test
echo base > "$repo26/f"
git -C "$repo26" add f
git -C "$repo26" commit -qm base
base26=$(git -C "$repo26" rev-parse HEAD)
echo staged > "$repo26/f2"
git -C "$repo26" add f2
gitdir26=$(git -C "$repo26" rev-parse --absolute-git-dir)
mkdir -p "$gitdir26/auto-iterate"
echo 0 > "$state26"
printf '{"phase":"staged","iteration":1,"review":"","commit":"","tag":"","message":"v1","channel":"","head":"%s","clean":1,"feedback":"","verify_cmd":"exit 1","verify_timeout":0}\n' \
    "$base26" > "$gitdir26/auto-iterate/resume26.json"
set +e
STUB_REPO=$repo26 STUB_STATE=$state26 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume26 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out26" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "staged resume verify failure exited $rc"
[ -f "$gitdir26/auto-iterate/resume26.json" ] || fail "staged resume dropped state after verify failure"
[ "$(git -C "$repo26" rev-list --count HEAD)" -eq 1 ] || fail "staged resume verified failure still committed"

# A killed driver must not race a verifier that is still running.
repo27=$tmp/repo27
state27=$tmp/state27
mkdir -p "$repo27"
git -C "$repo27" init -q
git -C "$repo27" config user.email test@example.com
git -C "$repo27" config user.name test
echo base > "$repo27/f"
git -C "$repo27" add f
git -C "$repo27" commit -qm base
echo 0 > "$state27"
cat > "$tmp/verify27.sh" <<'AG'
#!/bin/sh
set -eu
printf 'started\n' > "$VERIFY_MARKER"
sleep 2
printf 'late\n' > "$VERIFY_MARKER.late"
AG
chmod +x "$tmp/verify27.sh"
VERIFY_MARKER=$tmp/verify27.marker
export VERIFY_MARKER
STUB_REPO=$repo27 STUB_STATE=$state27 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task verify27 --agent-cmd "$tmp/agent.sh" --verify-cmd "sh $tmp/verify27.sh" --max 1 >"$tmp/out27a" 2>&1 &
pid27=$!
i=0
while [ ! -f "$VERIFY_MARKER" ] && [ "$i" -lt 50 ]; do
    sleep 0.1
    i=$((i + 1))
done
[ -f "$VERIFY_MARKER" ] || fail "verifier did not start"
kill -9 "$pid27" 2>/dev/null || true
wait "$pid27" 2>/dev/null || true
set +e
STUB_REPO=$repo27 STUB_STATE=$state27 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task verify27 --agent-cmd "$tmp/agent.sh" \
    --resume --verify-cmd true --max 1 >"$tmp/out27b" 2>&1
rc=$?
set -e
[ "$rc" -eq 2 ] || fail "resume raced the surviving verifier (exit $rc)"
grep -q "previous agent/verifier is still running" "$tmp/out27b" || fail "surviving-verifier guard missing"
i=0
while [ ! -f "$VERIFY_MARKER.late" ] && [ "$i" -lt 50 ]; do
    sleep 0.1
    i=$((i + 1))
done
[ -f "$VERIFY_MARKER.late" ] || fail "verifier did not finish its delayed write"
STUB_REPO=$repo27 STUB_STATE=$state27 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task verify27 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out27c" 2>&1 || fail "resume failed after the verifier exited"
[ "$(git -C "$repo27" rev-list --count HEAD)" -eq 2 ] || fail "post-verify-kill driver did not commit"

# A relative --evidence path is resolved from the launch directory, not the repo.
repo28=$tmp/repo28
state28=$tmp/state28
mkdir -p "$repo28"
git -C "$repo28" init -q
git -C "$repo28" config user.email test@example.com
git -C "$repo28" config user.name test
echo base > "$repo28/f"
git -C "$repo28" add f
git -C "$repo28" commit -qm base
echo 0 > "$state28"
(
    cd "$tmp"
    STUB_REPO=$repo28 STUB_STATE=$state28 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
        "$here/auto-iterate.sh" --task evidence28 --agent-cmd "$tmp/agent.sh" \
        --evidence rel28.log --max 1 >"$tmp/out28" 2>&1
) || fail "relative evidence run failed"
[ "$(cat "$repo28/evidence.txt")" = "$tmp/rel28.log" ] || fail "relative evidence resolved from the repo"
[ -f "$tmp/rel28.log" ] || fail "relative evidence file was not written from the launch directory"
[ ! -e "$repo28/rel28.log" ] || fail "relative evidence leaked into the repo"

# A failed agent keeps the round state and its feedback for a retry.
repo29=$tmp/repo29
state29=$tmp/state29
mkdir -p "$repo29"
git -C "$repo29" init -q
git -C "$repo29" config user.email test@example.com
git -C "$repo29" config user.name test
echo base > "$repo29/f"
git -C "$repo29" add f
git -C "$repo29" commit -qm base
base29=$(git -C "$repo29" rev-parse HEAD)
gitdir29=$(git -C "$repo29" rev-parse --absolute-git-dir)
mkdir -p "$gitdir29/auto-iterate"
echo 0 > "$state29"
printf '{"phase":"agent","iteration":1,"review":"","commit":"","tag":"","message":"v1","channel":"","head":"%s","clean":1,"feedback":"keep agent feedback"}\n' \
    "$base29" > "$gitdir29/auto-iterate/resume29.json"
set +e
STUB_REPO=$repo29 STUB_STATE=$state29 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume29 --agent-cmd 'exit 1' \
    --resume --max 1 --log "$tmp/run29a.log" >"$tmp/out29a" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "agent failure exited $rc"
[ -f "$gitdir29/auto-iterate/resume29.json" ] || fail "agent failure dropped state"
STUB_REPO=$repo29 STUB_STATE=$state29 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume29 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out29b" 2>&1 || fail "agent retry failed"
grep -q "keep agent feedback" "$repo29/feedback.txt" || fail "agent retry lost feedback"

# A failed verifier keeps state and feedback for a retry.
repo30=$tmp/repo30
state30=$tmp/state30
mkdir -p "$repo30"
git -C "$repo30" init -q
git -C "$repo30" config user.email test@example.com
git -C "$repo30" config user.name test
echo base > "$repo30/f"
git -C "$repo30" add f
git -C "$repo30" commit -qm base
base30=$(git -C "$repo30" rev-parse HEAD)
cat > "$tmp/verify30.sh" <<'AG'
#!/bin/sh
if [ -f "$VERIFY30_MARK" ]; then exit 0; fi
: > "$VERIFY30_MARK"
exit 1
AG
chmod +x "$tmp/verify30.sh"
gitdir30=$(git -C "$repo30" rev-parse --absolute-git-dir)
mkdir -p "$gitdir30/auto-iterate"
echo 0 > "$state30"
printf '{"phase":"agent","iteration":1,"review":"","commit":"","tag":"","message":"v1","channel":"","head":"%s","clean":1,"feedback":"keep verify feedback","verify_cmd":"sh %s","verify_timeout":0}\n' \
    "$base30" "$tmp/verify30.sh" > "$gitdir30/auto-iterate/resume30.json"
set +e
VERIFY30_MARK=$tmp/verify30.mark STUB_REPO=$repo30 STUB_STATE=$state30 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume30 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out30a" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "verify failure exited $rc"
[ -f "$gitdir30/auto-iterate/resume30.json" ] || fail "verify failure dropped state"
grep -q '"phase": "verify"' "$gitdir30/auto-iterate/resume30.json" || fail "verify failure state phase is not verify"
VERIFY30_MARK=$tmp/verify30.mark STUB_REPO=$repo30 STUB_STATE=$state30 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume30 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out30b" 2>&1 || fail "verify retry failed"
grep -q "keep verify feedback" "$repo30/feedback.txt" || fail "verify retry lost feedback"

# A failed stage keeps state and feedback for a retry.
repo31=$tmp/repo31
state31=$tmp/state31
mkdir -p "$repo31"
git -C "$repo31" init -q
git -C "$repo31" config user.email test@example.com
git -C "$repo31" config user.name test
echo base > "$repo31/f"
git -C "$repo31" add f
git -C "$repo31" commit -qm base
base31=$(git -C "$repo31" rev-parse HEAD)
gitdir31=$(git -C "$repo31" rev-parse --absolute-git-dir)
mkdir -p "$gitdir31/auto-iterate"
echo 0 > "$state31"
printf '{"phase":"agent","iteration":1,"review":"","commit":"","tag":"","message":"v1","channel":"","head":"%s","clean":1,"feedback":"keep stage feedback"}\n' \
    "$base31" > "$gitdir31/auto-iterate/resume31.json"
set +e
STUB_REPO=$repo31 STUB_STATE=$state31 STUB_ITERATIONS=1 STUB_FAIL_STAGE=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume31 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out31a" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "stage failure exited $rc"
[ -f "$gitdir31/auto-iterate/resume31.json" ] || fail "stage failure dropped state"
grep -q '"phase": "staged"' "$gitdir31/auto-iterate/resume31.json" || fail "stage failure state phase is not staged"
STUB_REPO=$repo31 STUB_STATE=$state31 STUB_ITERATIONS=1 STUB_FAIL_STAGE=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume31 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out31b" 2>&1 || fail "stage retry failed"
grep -q "keep stage feedback" "$repo31/feedback.txt" || fail "stage retry lost feedback"

# A held worker lease without a PID marker still blocks a new driver.
repo32=$tmp/repo32
state32=$tmp/state32
mkdir -p "$repo32"
git -C "$repo32" init -q
git -C "$repo32" config user.email test@example.com
git -C "$repo32" config user.name test
echo base > "$repo32/f"
git -C "$repo32" add f
git -C "$repo32" commit -qm base
gitdir32=$(git -C "$repo32" rev-parse --absolute-git-dir)
mkdir -p "$gitdir32/auto-iterate"
echo 0 > "$state32"
lease32=$gitdir32/auto-iterate/worker.lock
: > "$lease32"
(
    flock -n 8 || exit 97
    : > "$tmp/lease32.ready"
    sleep 2
) 8>"$lease32" &
holder32=$!
i=0
while [ ! -f "$tmp/lease32.ready" ] && [ "$i" -lt 50 ]; do
    sleep 0.1
    i=$((i + 1))
done
[ -f "$tmp/lease32.ready" ] || fail "worker lease holder did not start"
set +e
STUB_REPO=$repo32 STUB_STATE=$state32 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task lease32 --agent-cmd "$tmp/agent.sh" --max 1 >"$tmp/out32a" 2>&1
rc=$?
set -e
[ "$rc" -eq 2 ] || fail "held worker lease exited $rc"
grep -q "previous agent/verifier is still running" "$tmp/out32a" || fail "worker lease guard missing"
kill "$holder32" 2>/dev/null || true
wait "$holder32" 2>/dev/null || true
STUB_REPO=$repo32 STUB_STATE=$state32 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task lease32 --agent-cmd "$tmp/agent.sh" --max 1 >"$tmp/out32b" 2>&1 \
    || fail "run after worker lease release failed"
[ "$(git -C "$repo32" rev-list --count HEAD)" -eq 2 ] || fail "run after worker lease release did not commit"

# Reviewed recovery refuses a mutated index.
repo33=$tmp/repo33
state33=$tmp/state33
mkdir -p "$repo33"
git -C "$repo33" init -q
git -C "$repo33" config user.email test@example.com
git -C "$repo33" config user.name test
echo base > "$repo33/f"
git -C "$repo33" add f
git -C "$repo33" commit -qm base
base33=$(git -C "$repo33" rev-parse HEAD)
echo reviewed > "$repo33/f"
git -C "$repo33" add f
tree33=$(git -C "$repo33" write-tree)
gitdir33=$(git -C "$repo33" rev-parse --absolute-git-dir)
mkdir -p "$gitdir33/auto-iterate"
echo 0 > "$state33"
printf '{"phase":"reviewed","iteration":1,"review":"v1-1","commit":"","tag":"","message":"v1","channel":"stub","head":"%s","clean":1,"feedback":"","tree":"%s"}\n' \
    "$base33" "$tree33" > "$gitdir33/auto-iterate/review33.json"
echo mutated > "$repo33/f"
git -C "$repo33" add f
set +e
STUB_REPO=$repo33 STUB_STATE=$state33 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task review33 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out33" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "mutated reviewed index exited $rc"
grep -q "reviewed tree changed" "$tmp/out33" || fail "mutated reviewed index was not rejected"
[ "$(git -C "$repo33" rev-list --count HEAD)" -eq 1 ] || fail "mutated reviewed index was committed"

# Reviewed recovery refuses an unrelated child commit with a different tree.
repo34=$tmp/repo34
state34=$tmp/state34
mkdir -p "$repo34"
git -C "$repo34" init -q
git -C "$repo34" config user.email test@example.com
git -C "$repo34" config user.name test
echo base > "$repo34/f"
git -C "$repo34" add f
git -C "$repo34" commit -qm base
base34=$(git -C "$repo34" rev-parse HEAD)
echo reviewed > "$repo34/f"
git -C "$repo34" add f
tree34=$(git -C "$repo34" write-tree)
git -C "$repo34" reset -q --hard
echo unrelated > "$repo34/g"
git -C "$repo34" add g
git -C "$repo34" commit -qm child
gitdir34=$(git -C "$repo34" rev-parse --absolute-git-dir)
mkdir -p "$gitdir34/auto-iterate"
echo 0 > "$state34"
printf '{"phase":"reviewed","iteration":1,"review":"v1-1","commit":"","tag":"","message":"v1","channel":"stub","head":"%s","clean":1,"feedback":"","tree":"%s"}\n' \
    "$base34" "$tree34" > "$gitdir34/auto-iterate/review34.json"
set +e
STUB_REPO=$repo34 STUB_STATE=$state34 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task review34 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out34" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "unrelated child commit exited $rc"
grep -q "committed tree does not match reviewed tree" "$tmp/out34" || fail "unrelated child commit was not rejected"
[ "$(git -C "$repo34" rev-list --count HEAD)" -eq 2 ] || fail "unrelated child check created a commit"

# A paused rejection leaves no pending JSON and another task can start; feedback survives.
repo35=$tmp/repo35
state35=$tmp/state35
mkdir -p "$repo35"
git -C "$repo35" init -q
git -C "$repo35" config user.email test@example.com
git -C "$repo35" config user.name test
echo base > "$repo35/f"
git -C "$repo35" add f
git -C "$repo35" commit -qm base
gitdir35=$(git -C "$repo35" rev-parse --absolute-git-dir)
mkdir -p "$gitdir35/auto-iterate"
echo 0 > "$state35"
set +e
STUB_REPO=$repo35 STUB_STATE=$state35 STUB_REJECT_FIRST=1 STUB_PAUSE_REJECT=1 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task pause35a --agent-cmd "$tmp/agent.sh" --max 1 >"$tmp/out35a" 2>&1
rc=$?
set -e
[ "$rc" -eq 4 ] || fail "paused rejection exited $rc"
[ ! -f "$gitdir35/auto-iterate/pause35a.json" ] || fail "paused rejection left pending state"
[ -s "$gitdir35/auto-iterate/pause35a.feedback" ] || fail "paused rejection lost durable feedback"
grep -q "first-no" "$gitdir35/auto-iterate/pause35a.feedback" || fail "durable feedback content missing"
STUB_REPO=$repo35 STUB_STATE=$state35 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task pause35a --agent-cmd "$tmp/agent.sh" --resume --max 1 >"$tmp/out35b" 2>&1 \
    || fail "resume of paused rejection failed"
state35b=$tmp/state35b
echo 0 > "$state35b"
STUB_REPO=$repo35 STUB_STATE=$state35b STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task pause35b --agent-cmd "$tmp/agent.sh" --max 1 >"$tmp/out35c" 2>&1 \
    || fail "another task could not start after a paused rejection"
[ "$(git -C "$repo35" rev-list --count HEAD)" -eq 2 ] || fail "another task did not commit after a paused rejection"

# An insufficient-reviewer rejection that was already recorded must not block resume.
repo36=$tmp/repo36
state36=$tmp/state36
mkdir -p "$repo36"
git -C "$repo36" init -q
git -C "$repo36" config user.email test@example.com
git -C "$repo36" config user.name test
echo base > "$repo36/f"
git -C "$repo36" add f
git -C "$repo36" commit -qm base
base36=$(git -C "$repo36" rev-parse HEAD)
gitdir36=$(git -C "$repo36" rev-parse --absolute-git-dir)
mkdir -p "$gitdir36/auto-iterate"
echo 0 > "$state36"
printf '[{"review":"v1-6","outcome":"insufficient-reviewer"}]\n' > "$tmp/history36.json"
python3 - "$gitdir36/auto-iterate/resume36.json" "$base36" <<'PY'
import json, sys
json.dump({"phase":"rejected","iteration":1,"review":"v1-6","commit":"","tag":"",
           "message":"v1","channel":"stub","head":sys.argv[2],"clean":1,
           "feedback":"reviewer unavailable"}, open(sys.argv[1],"w"))
PY
: > "$state36.paused"
STUB_REPO=$repo36 STUB_STATE=$state36 STUB_HISTORY=$tmp/history36.json PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume36 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out36" 2>&1 || fail "insufficient-reviewer resume failed"
[ ! -f "$gitdir36/auto-iterate/resume36.json" ] || fail "insufficient-reviewer resume kept pending state"
grep -q "reviewer unavailable" "$gitdir36/auto-iterate/resume36.feedback" || fail "insufficient-reviewer feedback was lost"

# Feedback is reloaded under the lock, so stale text is dropped across lock contention.
repo37=$tmp/repo37
state37=$tmp/state37
mkdir -p "$repo37"
git -C "$repo37" init -q
git -C "$repo37" config user.email test@example.com
git -C "$repo37" config user.name test
echo base > "$repo37/f"
git -C "$repo37" add f
git -C "$repo37" commit -qm base
gitdir37=$(git -C "$repo37" rev-parse --absolute-git-dir)
mkdir -p "$gitdir37/auto-iterate"
echo 0 > "$state37"
printf 'stale feedback\n' > "$gitdir37/auto-iterate/contention37.feedback"
lock37=$gitdir37/auto-iterate/lock
: > "$lock37"
(
    flock -n 8 || exit 97
    : > "$tmp/lock37.ready"
    sleep 3
) 8>"$lock37" &
holder37=$!
i=0
while [ ! -f "$tmp/lock37.ready" ] && [ "$i" -lt 50 ]; do
    sleep 0.1
    i=$((i + 1))
done
[ -f "$tmp/lock37.ready" ] || fail "lock holder did not start"
STUB_REPO=$repo37 STUB_STATE=$state37 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task contention37 --agent-cmd "$tmp/agent20.sh" \
    --max 1 --lock-timeout 5 >"$tmp/out37" 2>&1 &
waiter37=$!
sleep 1
rm -f "$gitdir37/auto-iterate/contention37.feedback"
kill "$holder37" 2>/dev/null || true
wait "$holder37" 2>/dev/null || true
wait "$waiter37" || fail "lock waiter failed"
[ ! -s "$repo37/feedback.txt" ] || fail "stale feedback survived lock contention"

# A background agent descendant keeps the worker lease and blocks staging.
repo38=$tmp/repo38
state38=$tmp/state38
mkdir -p "$repo38"
git -C "$repo38" init -q
git -C "$repo38" config user.email test@example.com
git -C "$repo38" config user.name test
echo base > "$repo38/f"
git -C "$repo38" add f
git -C "$repo38" commit -qm base
cat > "$tmp/agent38.sh" <<'AG'
#!/bin/sh
set -eu
echo v1 > "$AUTO_ITERATE_REPO/out.txt"
sleep 3 &
echo $! > "$AUTO_ITERATE_REPO/bg.pid"
AG
chmod +x "$tmp/agent38.sh"
echo 0 > "$state38"
set +e
STUB_REPO=$repo38 STUB_STATE=$state38 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task agent38 --agent-cmd "$tmp/agent38.sh" --max 1 >"$tmp/out38" 2>&1
rc=$?
set -e
[ "$rc" -eq 2 ] || fail "agent descendant exited $rc"
grep -q "agent left a background process" "$tmp/out38" || fail "agent descendant guard missing"
[ "$(git -C "$repo38" rev-list --count HEAD)" -eq 1 ] || fail "agent descendant was committed"
[ -f "$repo38/bg.pid" ] && kill "$(cat "$repo38/bg.pid")" 2>/dev/null || true

# A background verifier descendant keeps the worker lease and blocks staging.
repo39=$tmp/repo39
state39=$tmp/state39
mkdir -p "$repo39"
git -C "$repo39" init -q
git -C "$repo39" config user.email test@example.com
git -C "$repo39" config user.name test
echo base > "$repo39/f"
git -C "$repo39" add f
git -C "$repo39" commit -qm base
cat > "$tmp/verify39.sh" <<'AG'
#!/bin/sh
set -eu
sleep 3 &
echo $! > "$VERIFY_BG"
AG
chmod +x "$tmp/verify39.sh"
echo 0 > "$state39"
set +e
VERIFY_BG=$tmp/bg39.pid STUB_REPO=$repo39 STUB_STATE=$state39 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task verify39 --agent-cmd "$tmp/agent.sh" \
    --verify-cmd "sh $tmp/verify39.sh" --max 1 >"$tmp/out39" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "verifier descendant exited $rc"
grep -q "verifier left a background process" "$tmp/out39" || fail "verifier descendant guard missing"
[ "$(git -C "$repo39" rev-list --count HEAD)" -eq 1 ] || fail "verifier descendant was committed"
[ -f "$tmp/bg39.pid" ] && kill "$(cat "$tmp/bg39.pid")" 2>/dev/null || true

# A killed driver leaves the review process holding the worker lease.
repo40=$tmp/repo40
state40=$tmp/state40
mkdir -p "$repo40"
git -C "$repo40" init -q
git -C "$repo40" config user.email test@example.com
git -C "$repo40" config user.name test
echo base > "$repo40/f"
git -C "$repo40" add f
git -C "$repo40" commit -qm base
echo 0 > "$state40"
STUB_REPO=$repo40 STUB_STATE=$state40 STUB_SLOW_REVIEW=1 STUB_REVIEW_PID=$tmp/review40.pid PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task review40 --agent-cmd "$tmp/agent.sh" --max 1 >"$tmp/out40a" 2>&1 &
driver40=$!
i=0
while [ ! -f "$tmp/review40.pid" ] && [ "$i" -lt 50 ]; do
    sleep 0.1
    i=$((i + 1))
done
[ -f "$tmp/review40.pid" ] || fail "review worker did not start"
kill -9 "$driver40" 2>/dev/null || true
wait "$driver40" 2>/dev/null || true
set +e
STUB_REPO=$repo40 STUB_STATE=$state40 STUB_SLOW_REVIEW=1 STUB_REVIEW_PID=$tmp/review40.pid PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task review40 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out40b" 2>&1
rc=$?
set -e
[ "$rc" -eq 2 ] || fail "resume raced the surviving review (exit $rc)"
grep -q "previous agent/verifier is still running" "$tmp/out40b" || fail "surviving-review guard missing"
kill "$(cat "$tmp/review40.pid")" 2>/dev/null || true
sleep 0.3
STUB_REPO=$repo40 STUB_STATE=$state40 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task review40 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out40c" 2>&1 || fail "resume failed after the review exited"
[ "$(git -C "$repo40" rev-list --count HEAD)" -eq 2 ] || fail "post-review-kill driver did not commit"

# A killed driver leaves the fallback reviewer holding the worker lease.
repo41=$tmp/repo41
state41=$tmp/state41
mkdir -p "$repo41"
git -C "$repo41" init -q
git -C "$repo41" config user.email test@example.com
git -C "$repo41" config user.name test
echo base > "$repo41/f"
git -C "$repo41" add f
git -C "$repo41" commit -qm base
cat > "$tmp/fallback41.sh" <<'AG'
#!/bin/sh
set -eu
printf '%s\n' "$$" > "$FB_PID"
exec sleep 5
AG
chmod +x "$tmp/fallback41.sh"
echo 0 > "$state41"
FB_PID=$tmp/fallback41.pid STUB_REPO=$repo41 STUB_STATE=$state41 STUB_RUN_NO_AI=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task fallback41 --agent-cmd "$tmp/agent.sh" \
    --fallback-cmd "sh $tmp/fallback41.sh" --fallback-channel claude-subagent \
    --max 1 >"$tmp/out41a" 2>&1 &
driver41=$!
i=0
while [ ! -f "$tmp/fallback41.pid" ] && [ "$i" -lt 50 ]; do
    sleep 0.1
    i=$((i + 1))
done
[ -f "$tmp/fallback41.pid" ] || fail "fallback worker did not start"
kill -9 "$driver41" 2>/dev/null || true
wait "$driver41" 2>/dev/null || true
set +e
FB_PID=$tmp/fallback41.pid STUB_REPO=$repo41 STUB_STATE=$state41 STUB_RUN_NO_AI=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task fallback41 --agent-cmd "$tmp/agent.sh" \
    --fallback-cmd "sh $tmp/fallback41.sh" --fallback-channel claude-subagent \
    --resume --max 1 >"$tmp/out41b" 2>&1
rc=$?
set -e
[ "$rc" -eq 2 ] || fail "resume raced the surviving fallback (exit $rc)"
grep -q "previous agent/verifier is still running" "$tmp/out41b" || fail "surviving-fallback guard missing"
kill "$(cat "$tmp/fallback41.pid")" 2>/dev/null || true
sleep 0.3
STUB_REPO=$repo41 STUB_STATE=$state41 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task fallback41 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out41c" 2>&1 || fail "resume failed after the fallback exited"
[ "$(git -C "$repo41" rev-list --count HEAD)" -eq 2 ] || fail "post-fallback-kill driver did not commit"

# Resume rejects a verify command that differs from the saved round.
repo42=$tmp/repo42
state42=$tmp/state42
mkdir -p "$repo42"
git -C "$repo42" init -q
git -C "$repo42" config user.email test@example.com
git -C "$repo42" config user.name test
echo base > "$repo42/f"
git -C "$repo42" add f
git -C "$repo42" commit -qm base
base42=$(git -C "$repo42" rev-parse HEAD)
gitdir42=$(git -C "$repo42" rev-parse --absolute-git-dir)
mkdir -p "$gitdir42/auto-iterate"
echo 0 > "$state42"
printf '{"phase":"verify","iteration":1,"review":"","commit":"","tag":"","message":"v1","channel":"","head":"%s","clean":1,"feedback":"","tree":"","verify_cmd":"exit 1","verify_timeout":0}\n' \
    "$base42" > "$gitdir42/auto-iterate/resume42.json"
set +e
STUB_REPO=$repo42 STUB_STATE=$state42 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume42 --agent-cmd "$tmp/agent.sh" \
    --resume --verify-cmd true --max 1 >"$tmp/out42" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "verify command mismatch exited $rc"
grep -q "verify command differs" "$tmp/out42" || fail "verify command mismatch was not rejected"
[ "$(git -C "$repo42" rev-list --count HEAD)" -eq 1 ] || fail "verify command mismatch committed"

# Saved per-version acceptance is restored on resume.
repo43=$tmp/repo43
state43=$tmp/state43
mkdir -p "$repo43"
git -C "$repo43" init -q
git -C "$repo43" config user.email test@example.com
git -C "$repo43" config user.name test
echo base > "$repo43/f"
git -C "$repo43" add f
git -C "$repo43" commit -qm base
base43=$(git -C "$repo43" rev-parse HEAD)
echo staged > "$repo43/f2"
git -C "$repo43" add f2
printf '{"1":"accept 43"}\n' > "$tmp/plan43.json"
gitdir43=$(git -C "$repo43" rev-parse --absolute-git-dir)
mkdir -p "$gitdir43/auto-iterate"
echo 0 > "$state43"
printf '{"phase":"staged","iteration":1,"review":"","commit":"","tag":"","message":"v1","channel":"","head":"%s","clean":1,"feedback":"","tree":"","verify_cmd":"","verify_timeout":0,"version_plan":"%s","version_acceptance":"accept 43","evidence":"%s"}\n' \
    "$base43" "$tmp/plan43.json" "$tmp/evidence43" > "$gitdir43/auto-iterate/resume43.json"
STUB_REPO=$repo43 STUB_STATE=$state43 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume43 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out43" 2>&1 || fail "saved acceptance resume failed"
grep -q "accept 43" "$tmp/evidence43" || fail "saved version acceptance was not restored"
[ "$(git -C "$repo43" rev-list --count HEAD)" -eq 2 ] || fail "saved acceptance resume did not commit"

# An edited version plan conflicts with the saved acceptance.
repo44=$tmp/repo44
state44=$tmp/state44
mkdir -p "$repo44"
git -C "$repo44" init -q
git -C "$repo44" config user.email test@example.com
git -C "$repo44" config user.name test
echo base > "$repo44/f"
git -C "$repo44" add f
git -C "$repo44" commit -qm base
base44=$(git -C "$repo44" rev-parse HEAD)
echo staged > "$repo44/f2"
git -C "$repo44" add f2
printf '{"1":"changed acceptance"}\n' > "$tmp/plan44.json"
gitdir44=$(git -C "$repo44" rev-parse --absolute-git-dir)
mkdir -p "$gitdir44/auto-iterate"
echo 0 > "$state44"
printf '{"phase":"staged","iteration":1,"review":"","commit":"","tag":"","message":"v1","channel":"","head":"%s","clean":1,"feedback":"","tree":"","verify_cmd":"","verify_timeout":0,"version_plan":"%s","version_acceptance":"accept 44"}\n' \
    "$base44" "$tmp/plan44.json" > "$gitdir44/auto-iterate/resume44.json"
set +e
STUB_REPO=$repo44 STUB_STATE=$state44 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume44 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out44" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "edited acceptance exited $rc"
grep -q "version acceptance differs" "$tmp/out44" || fail "edited acceptance was not rejected"
[ -f "$gitdir44/auto-iterate/resume44.json" ] || fail "edited acceptance dropped state"
[ "$(git -C "$repo44" rev-list --count HEAD)" -eq 1 ] || fail "edited acceptance was committed"
[ -z "$(git -C "$repo44" tag)" ] || fail "edited acceptance tagged"

# A reviewed v1 can advance to v2 after validating v1's saved acceptance.
repo45=$tmp/repo45
state45=$tmp/state45
mkdir -p "$repo45"
git -C "$repo45" init -q
git -C "$repo45" config user.email test@example.com
git -C "$repo45" config user.name test
echo base > "$repo45/f"
git -C "$repo45" add f
git -C "$repo45" commit -qm base
base45=$(git -C "$repo45" rev-parse HEAD)
echo reviewed > "$repo45/f2"
git -C "$repo45" add f2
printf '{"1":"accept 45 v1","2":"accept 45 v2"}\n' > "$tmp/plan45.json"
gitdir45=$(git -C "$repo45" rev-parse --absolute-git-dir)
mkdir -p "$gitdir45/auto-iterate"
echo 0 > "$state45"
printf '{"phase":"reviewed","iteration":1,"review":"v1-1","commit":"","tag":"","message":"v1","channel":"stub","head":"%s","clean":1,"feedback":"","tree":"%s","verify_cmd":"","verify_timeout":0,"version_plan":"%s","version_acceptance":"accept 45 v1"}\n' \
    "$base45" "$(git -C "$repo45" write-tree)" "$tmp/plan45.json" > "$gitdir45/auto-iterate/resume45.json"
STUB_REPO=$repo45 STUB_STATE=$state45 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume45 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out45" 2>&1 || fail "reviewed v1 acceptance resume failed"
grep -q "version acceptance differs" "$tmp/out45" && fail "v2 acceptance was compared to v1"
[ "$(git -C "$repo45" rev-list --count HEAD)" -eq 2 ] || fail "reviewed v1 acceptance resume did not commit"

# An already-approved committed v1 can advance to v2 after the same check.
repo46=$tmp/repo46
state46=$tmp/state46
mkdir -p "$repo46"
git -C "$repo46" init -q
git -C "$repo46" config user.email test@example.com
git -C "$repo46" config user.name test
echo base > "$repo46/f"
git -C "$repo46" add f
git -C "$repo46" commit -qm base
base46=$(git -C "$repo46" rev-parse HEAD)
echo one > "$repo46/f"
git -C "$repo46" add f
git -C "$repo46" commit -qm v1
commit46=$(git -C "$repo46" rev-parse HEAD)
tag46=auto-iterate/resume46/v1
git -C "$repo46" tag "$tag46"
printf '[{"review":"v1-9","outcome":"approved"}]\n' > "$tmp/history46.json"
gitdir46=$(git -C "$repo46" rev-parse --absolute-git-dir)
mkdir -p "$gitdir46/auto-iterate"
echo 1 > "$state46"
printf '{"phase":"committed","iteration":1,"review":"v1-9","commit":"%s","tag":"%s","message":"v1","channel":"stub","head":"%s","clean":1,"feedback":"","tree":"%s","verify_cmd":"","verify_timeout":0,"version_plan":"%s","version_acceptance":"accept 46 v1"}\n' \
    "$commit46" "$tag46" "$base46" "$(git -C "$repo46" rev-parse "$commit46^{tree}")" "$tmp/plan46.json" > "$gitdir46/auto-iterate/resume46.json"
printf '{"1":"accept 46 v1","2":"accept 46 v2"}\n' > "$tmp/plan46.json"
STUB_REPO=$repo46 STUB_STATE=$state46 STUB_HISTORY=$tmp/history46.json STUB_ITERATIONS=2 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume46 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out46" 2>&1 || fail "committed v1 acceptance resume failed"
grep -q "version acceptance differs" "$tmp/out46" && fail "v2 acceptance was compared to committed v1"
[ "$(git -C "$repo46" rev-list --count HEAD)" -eq 3 ] || fail "committed v1 acceptance resume did not continue to v2"

# Resume cannot add a verifier to a round that was saved without one.
repo47=$tmp/repo47
state47=$tmp/state47
mkdir -p "$repo47"
git -C "$repo47" init -q
git -C "$repo47" config user.email test@example.com
git -C "$repo47" config user.name test
echo base > "$repo47/f"
git -C "$repo47" add f
git -C "$repo47" commit -qm base
base47=$(git -C "$repo47" rev-parse HEAD)
echo reviewed > "$repo47/f2"
git -C "$repo47" add f2
gitdir47=$(git -C "$repo47" rev-parse --absolute-git-dir)
mkdir -p "$gitdir47/auto-iterate"
echo 0 > "$state47"
printf '{"phase":"reviewed","iteration":1,"review":"v1-1","commit":"","tag":"","message":"v1","channel":"stub","head":"%s","clean":1,"feedback":"","tree":"%s","verify_cmd":"","verify_timeout":0}\n' \
    "$base47" "$(git -C "$repo47" write-tree)" > "$gitdir47/auto-iterate/resume47.json"
set +e
STUB_REPO=$repo47 STUB_STATE=$state47 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume47 --agent-cmd "$tmp/agent.sh" \
    --resume --verify-cmd 'exit 1' --max 1 >"$tmp/out47" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "added verifier exited $rc"
grep -q "verify settings were not part of the saved round" "$tmp/out47" || fail "added verifier was not rejected"
[ -f "$gitdir47/auto-iterate/resume47.json" ] || fail "added verifier dropped state"
[ "$(git -C "$repo47" rev-list --count HEAD)" -eq 1 ] || fail "added verifier committed"

# A timeout-only override cannot silently replace the saved timeout.
repo48=$tmp/repo48
state48=$tmp/state48
mkdir -p "$repo48"
git -C "$repo48" init -q
git -C "$repo48" config user.email test@example.com
git -C "$repo48" config user.name test
echo base > "$repo48/f"
git -C "$repo48" add f
git -C "$repo48" commit -qm base
base48=$(git -C "$repo48" rev-parse HEAD)
gitdir48=$(git -C "$repo48" rev-parse --absolute-git-dir)
mkdir -p "$gitdir48/auto-iterate"
echo 0 > "$state48"
printf '{"phase":"verify","iteration":1,"review":"","commit":"","tag":"","message":"v1","channel":"","head":"%s","clean":1,"feedback":"","tree":"","verify_cmd":"exit 1","verify_timeout":0}\n' \
    "$base48" > "$gitdir48/auto-iterate/resume48.json"
set +e
STUB_REPO=$repo48 STUB_STATE=$state48 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume48 --agent-cmd "$tmp/agent.sh" \
    --resume --verify-timeout 5 --max 1 >"$tmp/out48" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "timeout-only override exited $rc"
grep -q "verify timeout differs" "$tmp/out48" || fail "timeout-only override was not rejected"
[ -f "$gitdir48/auto-iterate/resume48.json" ] || fail "timeout-only override dropped state"
[ "$(git -C "$repo48" rev-list --count HEAD)" -eq 1 ] || fail "timeout-only override committed"

# A saved evidence path is restored on resume.
repo49=$tmp/repo49
state49=$tmp/state49
mkdir -p "$repo49"
git -C "$repo49" init -q
git -C "$repo49" config user.email test@example.com
git -C "$repo49" config user.name test
echo base > "$repo49/f"
git -C "$repo49" add f
git -C "$repo49" commit -qm base
base49=$(git -C "$repo49" rev-parse HEAD)
echo staged > "$repo49/f2"
git -C "$repo49" add f2
gitdir49=$(git -C "$repo49" rev-parse --absolute-git-dir)
mkdir -p "$gitdir49/auto-iterate"
echo 0 > "$state49"
printf '{"phase":"verify","iteration":1,"review":"","commit":"","tag":"","message":"v1","channel":"","head":"%s","clean":1,"feedback":"","tree":"","verify_cmd":"echo evidence-ok","verify_timeout":0,"version_plan":"","version_acceptance":"","evidence":"%s"}\n' \
    "$base49" "$tmp/evidence49" > "$gitdir49/auto-iterate/resume49.json"
STUB_REPO=$repo49 STUB_STATE=$state49 STUB_ITERATIONS=1 STUB_ARGS=$tmp/args49 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume49 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out49" 2>&1 || fail "saved evidence resume failed"
grep -q "evidence-ok" "$tmp/evidence49" || fail "saved evidence was not used for verify"
grep -q -- "--evidence $tmp/evidence49" "$tmp/args49" || fail "saved evidence was not passed to review"

# A custom tag saved with reviewed state is reused on recovery.
repo50=$tmp/repo50
state50=$tmp/state50
mkdir -p "$repo50"
git -C "$repo50" init -q
git -C "$repo50" config user.email test@example.com
git -C "$repo50" config user.name test
echo base > "$repo50/f"
git -C "$repo50" add f
git -C "$repo50" commit -qm base
base50=$(git -C "$repo50" rev-parse HEAD)
echo reviewed > "$repo50/f2"
git -C "$repo50" add f2
gitdir50=$(git -C "$repo50" rev-parse --absolute-git-dir)
mkdir -p "$gitdir50/auto-iterate"
echo 0 > "$state50"
printf '{"phase":"reviewed","iteration":1,"review":"v1-1","commit":"","tag":"custom/resume50/v1","message":"v1","channel":"stub","head":"%s","clean":1,"feedback":"","tree":"%s","verify_cmd":"","verify_timeout":0,"version_plan":"","version_acceptance":"","evidence":""}\n' \
    "$base50" "$(git -C "$repo50" write-tree)" > "$gitdir50/auto-iterate/resume50.json"
STUB_REPO=$repo50 STUB_STATE=$state50 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume50 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out50" 2>&1 || fail "custom tag resume failed"
[ "$(git -C "$repo50" tag)" = "custom/resume50/v1" ] || fail "custom tag was not reused"
[ "$(git -C "$repo50" rev-list --count HEAD)" -eq 2 ] || fail "custom tag resume did not commit"

# A version plan without --evidence still gives the reviewer the acceptance text.
repo51=$tmp/repo51
state51=$tmp/state51
mkdir -p "$repo51"
git -C "$repo51" init -q
git -C "$repo51" config user.email test@example.com
git -C "$repo51" config user.name test
echo base > "$repo51/f"
git -C "$repo51" add f
git -C "$repo51" commit -qm base
printf '{"1":"review accept 51"}\n' > "$tmp/plan51.json"
echo 0 > "$state51"
STUB_REPO=$repo51 STUB_STATE=$state51 STUB_ITERATIONS=1 STUB_ARGS=$tmp/args51 STUB_EVIDENCE_COPY=$tmp/evidence51.copy PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task review51 --agent-cmd "$tmp/agent.sh" \
    --version-plan "$tmp/plan51.json" --max 1 >"$tmp/out51" 2>&1 || fail "version plan without evidence failed"
grep -q "review accept 51" "$tmp/evidence51.copy" || fail "reviewer did not receive version acceptance"
grep -q -- "--evidence" "$tmp/args51" || fail "reviewer was not given an evidence path"

# The version plan path is stored absolutely for recovery.
repo52=$tmp/repo52
state52=$tmp/state52
mkdir -p "$repo52"
git -C "$repo52" init -q
git -C "$repo52" config user.email test@example.com
git -C "$repo52" config user.name test
echo base > "$repo52/f"
git -C "$repo52" add f
git -C "$repo52" commit -qm base
printf '{"1":"accept 52"}\n' > "$tmp/plan52.json"
cat > "$tmp/verify52.sh" <<'AG'
#!/bin/sh
if [ -f "$VERIFY52_MARK" ]; then exit 0; fi
: > "$VERIFY52_MARK"
exit 1
AG
chmod +x "$tmp/verify52.sh"
echo 0 > "$state52"
set +e
(cd "$tmp" && VERIFY52_MARK=$tmp/verify52.mark STUB_REPO=$repo52 STUB_STATE=$state52 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task plan52 --agent-cmd "$tmp/agent.sh" \
    --version-plan plan52.json --verify-cmd "sh $tmp/verify52.sh" --max 1 >"$tmp/out52a" 2>&1)
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "relative plan setup exited $rc"
grep -Fq "\"version_plan\": \"$tmp/plan52.json\"" "$repo52/.git/auto-iterate/plan52.json" || fail "version plan was not stored absolutely"
(cd / && VERIFY52_MARK=$tmp/verify52.mark STUB_REPO=$repo52 STUB_STATE=$state52 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task plan52 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out52b" 2>&1) || fail "resume from another directory failed"

# Repeating the same plan options after an interruption keeps the saved evidence.
repo53=$tmp/repo53
state53=$tmp/state53
mkdir -p "$repo53"
git -C "$repo53" init -q
git -C "$repo53" config user.email test@example.com
git -C "$repo53" config user.name test
echo base > "$repo53/f"
git -C "$repo53" add f
git -C "$repo53" commit -qm base
printf '{"1":"accept 53"}\n' > "$tmp/plan53.json"
cat > "$tmp/verify53.sh" <<'AG'
#!/bin/sh
if [ -f "$VERIFY53_MARK" ]; then exit 0; fi
: > "$VERIFY53_MARK"
exit 1
AG
chmod +x "$tmp/verify53.sh"
echo 0 > "$state53"
set +e
(cd "$tmp" && VERIFY53_MARK=$tmp/verify53.mark STUB_REPO=$repo53 STUB_STATE=$state53 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task plan53 --agent-cmd "$tmp/agent.sh" \
    --version-plan plan53.json --verify-cmd "sh $tmp/verify53.sh" --max 1 >"$tmp/out53a" 2>&1)
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "repeated-plan setup exited $rc"
VERIFY53_MARK=$tmp/verify53.mark STUB_REPO=$repo53 STUB_STATE=$state53 STUB_ITERATIONS=1 STUB_EVIDENCE_COPY=$tmp/evidence53.copy PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task plan53 --agent-cmd "$tmp/agent.sh" \
    --resume --version-plan "$tmp/plan53.json" --max 1 >"$tmp/out53b" 2>&1 \
    || fail "resume with repeated plan options failed"
grep -q "accept 53" "$tmp/evidence53.copy" || fail "repeated-plan resume lost acceptance evidence"

# A failed agent that left partial edits can resume without discarding them.
repo54=$tmp/repo54
state54=$tmp/state54
mkdir -p "$repo54"
git -C "$repo54" init -q
git -C "$repo54" config user.email test@example.com
git -C "$repo54" config user.name test
echo base > "$repo54/f"
git -C "$repo54" add f
git -C "$repo54" commit -qm base
cat > "$tmp/agent54.sh" <<'AG'
#!/bin/sh
set -eu
if [ -f "$AUTO_ITERATE_REPO/partial.marker" ]; then
    echo done > "$AUTO_ITERATE_REPO/out.txt"
else
    echo partial > "$AUTO_ITERATE_REPO/partial.txt"
    : > "$AUTO_ITERATE_REPO/partial.marker"
    exit 1
fi
AG
chmod +x "$tmp/agent54.sh"
echo 0 > "$state54"
set +e
STUB_REPO=$repo54 STUB_STATE=$state54 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task agent54 --agent-cmd "$tmp/agent54.sh" --max 1 >"$tmp/out54a" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "partial agent exited $rc"
[ -f "$repo54/partial.txt" ] || fail "partial edit was not kept"
STUB_REPO=$repo54 STUB_STATE=$state54 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task agent54 --agent-cmd "$tmp/agent54.sh" \
    --resume --max 1 >"$tmp/out54b" 2>&1 || fail "partial agent resume failed"
grep -q "preserving partial edits" "$tmp/run.log" 2>/dev/null || true
[ "$(git -C "$repo54" show HEAD:partial.txt)" = partial ] || fail "partial edit was not committed"
[ "$(git -C "$repo54" rev-list --count HEAD)" -eq 2 ] || fail "partial agent resume did not commit"

# An agent failure during a dirty rejection retry keeps and resumes the tracked edits.
repo55=$tmp/repo55
state55=$tmp/state55
mkdir -p "$repo55"
git -C "$repo55" init -q
git -C "$repo55" config user.email test@example.com
git -C "$repo55" config user.name test
echo base > "$repo55/f"
git -C "$repo55" add f
git -C "$repo55" commit -qm base
base55=$(git -C "$repo55" rev-parse HEAD)
echo edited > "$repo55/f"
gitdir55=$(git -C "$repo55" rev-parse --absolute-git-dir)
mkdir -p "$gitdir55/auto-iterate"
echo 0 > "$state55"
printf '{"phase":"agent","iteration":1,"review":"","commit":"","tag":"","message":"v1","channel":"","head":"%s","clean":1,"feedback":"retry feedback"}\n' \
    "$base55" > "$gitdir55/auto-iterate/resume55.json"
STUB_REPO=$repo55 STUB_STATE=$state55 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume55 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out55" 2>&1 || fail "dirty rejection retry resume failed"
[ "$(git -C "$repo55" show HEAD:f)" = edited ] || fail "tracked rejection edit was lost"
grep -q "retry feedback" "$repo55/feedback.txt" || fail "dirty rejection retry lost feedback"

# Rejection -> stop-on-reject -> resume keeps tracked edits and retry metadata.
repo56=$tmp/repo56
state56=$tmp/state56
mkdir -p "$repo56"
git -C "$repo56" init -q
git -C "$repo56" config user.email test@example.com
git -C "$repo56" config user.name test
echo base > "$repo56/f"
git -C "$repo56" add f
git -C "$repo56" commit -qm base
cat > "$tmp/agent56.sh" <<'AG'
#!/bin/sh
set -eu
echo tracked-change > "$AUTO_ITERATE_REPO/f"
echo junk > "$AUTO_ITERATE_REPO/junk56.txt"
AG
cat > "$tmp/verify56.sh" <<AG
#!/bin/sh
set -eu
echo verify >> "$tmp/verify56.marker"
AG
cat > "$tmp/plan56.json" <<'JSON'
{"1":"retry acceptance"}
JSON
chmod +x "$tmp/agent56.sh" "$tmp/verify56.sh"
: > "$tmp/verify56.marker"
echo 0 > "$state56"
set +e
STUB_REPO=$repo56 STUB_STATE=$state56 STUB_REJECT=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task reject56 --agent-cmd "$tmp/agent56.sh" \
    --verify-cmd "sh $tmp/verify56.sh" --version-plan "$tmp/plan56.json" \
    --evidence "$tmp/evidence56" --stop-on-reject --max 1 >"$tmp/out56a" 2>&1
rc=$?
set -e
[ "$rc" -eq 5 ] || fail "stop-on-reject exited $rc"
[ "$(git -C "$repo56" status --porcelain f)" = " M f" ] || fail "rejected tracked edit was not kept"
[ -f "$repo56/.git/auto-iterate/reject56.retry" ] || fail "retry metadata missing"
grep -q '"verify_cmd"' "$repo56/.git/auto-iterate/reject56.retry" || fail "retry metadata lost verify settings"
grep -q '"version_plan"' "$repo56/.git/auto-iterate/reject56.retry" || fail "retry metadata lost version plan"
STUB_REPO=$repo56 STUB_STATE=$state56 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task reject56 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out56b" 2>&1 || fail "rejection stop resume failed"
[ "$(git -C "$repo56" show HEAD:f)" = tracked-change ] || fail "rejection resume lost tracked edit"
[ "$(git -C "$repo56" rev-list --count HEAD)" -eq 2 ] || fail "rejection resume did not commit"
grep -q "no" "$repo56/feedback.txt" || fail "rejection resume lost feedback"
[ "$(grep -c verify "$tmp/verify56.marker")" -eq 2 ] || fail "rejection resume lost verify command"
grep -q "retry acceptance" "$tmp/evidence56" || fail "rejection resume lost version acceptance"

# A committed approved version recorded as insufficient-reviewer is idempotent.
repo57=$tmp/repo57
state57=$tmp/state57
mkdir -p "$repo57"
git -C "$repo57" init -q
git -C "$repo57" config user.email test@example.com
git -C "$repo57" config user.name test
echo base > "$repo57/f"
git -C "$repo57" add f
git -C "$repo57" commit -qm base
base57=$(git -C "$repo57" rev-parse HEAD)
echo one > "$repo57/f"
git -C "$repo57" add f
git -C "$repo57" commit -qm v1
commit57=$(git -C "$repo57" rev-parse HEAD)
tag57=auto-iterate/resume57/v1
git -C "$repo57" tag "$tag57"
printf '[{"review":"v1-5","outcome":"insufficient-reviewer"}]\n' > "$tmp/history57.json"
gitdir57=$(git -C "$repo57" rev-parse --absolute-git-dir)
mkdir -p "$gitdir57/auto-iterate"
echo 1 > "$state57"
printf '{"phase":"committed","iteration":1,"review":"v1-5","commit":"%s","tag":"%s","message":"v1","channel":"stub","head":"%s","clean":1,"feedback":"","tree":"%s","verify_cmd":"","verify_timeout":0,"version_plan":"","version_acceptance":"","evidence":""}\n' \
    "$commit57" "$tag57" "$base57" "$(git -C "$repo57" rev-parse "$commit57^{tree}")" > "$gitdir57/auto-iterate/resume57.json"
: > "$state57.paused"
STUB_REPO=$repo57 STUB_STATE=$state57 STUB_HISTORY=$tmp/history57.json PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume57 --agent-cmd "$tmp/agent.sh" \
    --resume --max 1 >"$tmp/out57" 2>&1 || fail "insufficient committed resume failed"
[ ! -f "$gitdir57/auto-iterate/resume57.json" ] || fail "insufficient committed resume kept state"
[ "$(git -C "$repo57" rev-list --count HEAD)" -eq 2 ] || fail "insufficient committed resume changed history"

# Recovery must stop before recording or cleanup if persisting committed state fails.
repo58=$tmp/repo58
state58=$tmp/state58
mkdir -p "$repo58"
git -C "$repo58" init -q
git -C "$repo58" config user.email test@example.com
git -C "$repo58" config user.name test
echo base > "$repo58/f"
git -C "$repo58" add f
git -C "$repo58" commit -qm base
base58=$(git -C "$repo58" rev-parse HEAD)
echo one > "$repo58/f"
git -C "$repo58" add f
tree58=$(git -C "$repo58" write-tree)
gitdir58=$(git -C "$repo58" rev-parse --absolute-git-dir)
mkdir -p "$gitdir58/auto-iterate" "$tmp/bin-fail58"
echo 0 > "$state58"
printf '{"phase":"reviewed","iteration":1,"review":"v1-6","commit":"","tag":"","message":"v1","channel":"stub","head":"%s","clean":1,"feedback":"","tree":"%s","verify_cmd":"","verify_timeout":0,"version_plan":"","version_acceptance":"","evidence":""}\n' \
    "$base58" "$tree58" > "$gitdir58/auto-iterate/resume58.json"
printf '[]\n' > "$tmp/history58.json"
: > "$state58.paused"
cat > "$tmp/bin-fail58/python3" <<'PYFAIL'
#!/bin/sh
[ "$2" = "$FAIL_STATE" ] && exit 1
exec /usr/bin/python3 "$@"
PYFAIL
chmod +x "$tmp/bin-fail58/python3"
set +e
STUB_REPO=$repo58 STUB_STATE=$state58 STUB_HISTORY=$tmp/history58.json \
    PATH="$tmp/bin-fail58:$bin:$PATH" FAIL_STATE=$gitdir58/auto-iterate/resume58.json \
    "$here/auto-iterate.sh" --task resume58 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out58" 2>&1
rc=$?
set -e
[ "$rc" -ne 0 ] || fail "failed state persistence was ignored"
[ -f "$gitdir58/auto-iterate/resume58.json" ] || fail "failed state persistence removed the state"
grep -q "cannot persist committed state" "$tmp/out58" || fail "state persistence failure was not reported"

# Feedback persistence failures stop staged/agent recovery.
repo59=$tmp/repo59
state59=$tmp/state59
mkdir -p "$repo59"
git -C "$repo59" init -q
git -C "$repo59" config user.email test@example.com
git -C "$repo59" config user.name test
echo base > "$repo59/f"
git -C "$repo59" add f
git -C "$repo59" commit -qm base
base59=$(git -C "$repo59" rev-parse HEAD)
echo staged > "$repo59/f"
git -C "$repo59" add f
tree59=$(git -C "$repo59" write-tree)
gitdir59=$(git -C "$repo59" rev-parse --absolute-git-dir)
mkdir -p "$gitdir59/auto-iterate"
echo 0 > "$state59"
printf '{"phase":"staged","iteration":1,"review":"","commit":"","tag":"","message":"v1","channel":"","head":"%s","clean":1,"feedback":"keep-stage","tree":"%s","verify_cmd":"","verify_timeout":0,"version_plan":"","version_acceptance":"","evidence":""}\n' \
    "$base59" "$tree59" > "$gitdir59/auto-iterate/resume59.json"
: > "$state59.paused"
set +e
STUB_REPO=$repo59 STUB_STATE=$state59 AUTO_ITERATE_FAIL_FEEDBACK=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume59 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out59" 2>&1
rc=$?
set -e
[ "$rc" -ne 0 ] || fail "staged feedback persistence failure was ignored"
[ -f "$gitdir59/auto-iterate/resume59.json" ] || fail "staged feedback failure removed state"
grep -q "cannot restore feedback" "$tmp/out59" || fail "staged feedback failure was not reported"

repo60=$tmp/repo60
state60=$tmp/state60
mkdir -p "$repo60"
git -C "$repo60" init -q
git -C "$repo60" config user.email test@example.com
git -C "$repo60" config user.name test
echo base > "$repo60/f"
git -C "$repo60" add f
git -C "$repo60" commit -qm base
base60=$(git -C "$repo60" rev-parse HEAD)
gitdir60=$(git -C "$repo60" rev-parse --absolute-git-dir)
mkdir -p "$gitdir60/auto-iterate"
echo 0 > "$state60"
printf '{"phase":"agent","iteration":1,"review":"","commit":"","tag":"","message":"v1","channel":"","head":"%s","clean":1,"feedback":"keep-agent","tree":"%s","verify_cmd":"","verify_timeout":0,"version_plan":"","version_acceptance":"","evidence":""}\n' \
    "$base60" "$(git -C "$repo60" rev-parse HEAD^{tree})" > "$gitdir60/auto-iterate/resume60.json"
: > "$state60.paused"
set +e
STUB_REPO=$repo60 STUB_STATE=$state60 AUTO_ITERATE_FAIL_FEEDBACK=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume60 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out60" 2>&1
rc=$?
set -e
[ "$rc" -ne 0 ] || fail "agent feedback persistence failure was ignored"
[ -f "$gitdir60/auto-iterate/resume60.json" ] || fail "agent feedback failure removed state"

# Acceptance/verification evidence append failures stop the round.
repo61=$tmp/repo61
state61=$tmp/state61
mkdir -p "$repo61"
git -C "$repo61" init -q
git -C "$repo61" config user.email test@example.com
git -C "$repo61" config user.name test
echo base > "$repo61/f"
git -C "$repo61" add f
git -C "$repo61" commit -qm base
printf '{"1":"must fail"}\n' > "$tmp/plan61.json"
echo 0 > "$state61"
set +e
STUB_REPO=$repo61 STUB_STATE=$state61 AUTO_ITERATE_FAIL_EVIDENCE=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task accept61 --agent-cmd "$tmp/agent20.sh" \
    --version-plan "$tmp/plan61.json" --evidence "$tmp/evidence61" \
    --max 1 >"$tmp/out61" 2>&1
rc=$?
set -e
[ "$rc" -ne 0 ] || fail "acceptance append failure was ignored"
[ "$(git -C "$repo61" rev-list --count HEAD)" -eq 1 ] || fail "acceptance append failure committed"

repo62=$tmp/repo62
state62=$tmp/state62
mkdir -p "$repo62"
git -C "$repo62" init -q
git -C "$repo62" config user.email test@example.com
git -C "$repo62" config user.name test
echo base > "$repo62/f"
git -C "$repo62" add f
git -C "$repo62" commit -qm base
echo 0 > "$state62"
set +e
STUB_REPO=$repo62 STUB_STATE=$state62 AUTO_ITERATE_FAIL_EVIDENCE=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task verify62 --agent-cmd "$tmp/agent20.sh" \
    --verify-cmd true --evidence "$tmp/evidence62" --max 1 >"$tmp/out62" 2>&1
rc=$?
set -e
[ "$rc" -ne 0 ] || fail "verify evidence append failure was ignored"
[ "$(git -C "$repo62" rev-list --count HEAD)" -eq 1 ] || fail "verify evidence append failure committed"
gitdir62=$(git -C "$repo62" rev-parse --absolute-git-dir)
[ -f "$gitdir62/auto-iterate/verify62.json" ] || fail "verify evidence failure did not preserve state"

# Rejected recovery cleans only the untracked paths saved with the round.
repo63=$tmp/repo63
state63=$tmp/state63
mkdir -p "$repo63"
git -C "$repo63" init -q
git -C "$repo63" config user.email test@example.com
git -C "$repo63" config user.name test
echo base > "$repo63/f"
git -C "$repo63" add f
git -C "$repo63" commit -qm base
base63=$(git -C "$repo63" rev-parse HEAD)
gitdir63=$(git -C "$repo63" rev-parse --absolute-git-dir)
mkdir -p "$gitdir63/auto-iterate"
echo junk > "$repo63/agent-junk.txt"
echo keep > "$repo63/user-created.txt"
untracked63=$(printf 'agent-junk.txt\0' | base64 -w0)
echo 0 > "$state63"
printf '[{"review":"v1-9","outcome":"rejected"}]\n' > "$tmp/history63.json"
printf '{"phase":"rejected","iteration":1,"review":"v1-9","commit":"","tag":"","message":"v1","channel":"stub","head":"%s","clean":1,"feedback":"","tree":"","untracked":"%s","verify_cmd":"","verify_timeout":0,"version_plan":"","version_acceptance":"","evidence":""}\n' \
    "$base63" "$untracked63" > "$gitdir63/auto-iterate/resume63.json"
: > "$state63.paused"
STUB_REPO=$repo63 STUB_STATE=$state63 STUB_HISTORY=$tmp/history63.json PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume63 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out63" 2>&1 || fail "targeted cleaned resume failed"
[ ! -e "$repo63/agent-junk.txt" ] || fail "recorded agent junk survived"
[ -e "$repo63/user-created.txt" ] || fail "unrelated user file was deleted"
[ ! -f "$gitdir63/auto-iterate/resume63.json" ] || fail "targeted cleanup kept state"

# A feedback persistence failure keeps the rejected state recoverable.
repo64=$tmp/repo64
state64=$tmp/state64
mkdir -p "$repo64"
git -C "$repo64" init -q
git -C "$repo64" config user.email test@example.com
git -C "$repo64" config user.name test
echo base > "$repo64/f"
git -C "$repo64" add f
git -C "$repo64" commit -qm base
base64=$(git -C "$repo64" rev-parse HEAD)
gitdir64=$(git -C "$repo64" rev-parse --absolute-git-dir)
mkdir -p "$gitdir64/auto-iterate"
echo 0 > "$state64"
printf '[]\n' > "$tmp/history64.json"
printf '{"phase":"rejected","iteration":1,"review":"v1-7","commit":"","tag":"","message":"v1","channel":"stub","head":"%s","clean":1,"feedback":"persist me","tree":"","untracked":"","verify_cmd":"","verify_timeout":0,"version_plan":"","version_acceptance":"","evidence":""}\n' \
    "$base64" > "$gitdir64/auto-iterate/resume64.json"
: > "$state64.paused"
set +e
STUB_REPO=$repo64 STUB_STATE=$state64 STUB_HISTORY=$tmp/history64.json AUTO_ITERATE_FAIL_FEEDBACK=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume64 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out64" 2>&1
rc=$?
set -e
[ "$rc" -ne 0 ] || fail "rejected feedback persistence failure was ignored"
[ "$(cat "$state64")" = 0 ] || fail "record ran before feedback persistence"
[ -f "$gitdir64/auto-iterate/resume64.json" ] || fail "rejected feedback failure removed state"

# A rejected state written by the driver keeps its cleanup list across a record failure.
repo65=$tmp/repo65
state65=$tmp/state65
mkdir -p "$repo65"
git -C "$repo65" init -q
git -C "$repo65" config user.email test@example.com
git -C "$repo65" config user.name test
echo base > "$repo65/f"
git -C "$repo65" add f
git -C "$repo65" commit -qm base
cat > "$tmp/agent65.sh" <<'AG'
#!/bin/sh
set -eu
echo tracked > "$AUTO_ITERATE_REPO/f"
echo junk > "$AUTO_ITERATE_REPO/agent-junk65.txt"
AG
chmod +x "$tmp/agent65.sh"
echo 0 > "$state65"
set +e
STUB_REPO=$repo65 STUB_STATE=$state65 STUB_REJECT=1 STUB_FAIL_RECORD=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task reject65 --agent-cmd "$tmp/agent65.sh" \
    --max 1 >"$tmp/out65a" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "record failure exited $rc"
gitdir65=$(git -C "$repo65" rev-parse --absolute-git-dir)
[ -f "$gitdir65/auto-iterate/reject65.json" ] || fail "record failure did not keep rejected state"
[ -f "$repo65/agent-junk65.txt" ] || fail "record failure cleaned junk too early"
STUB_REPO=$repo65 STUB_STATE=$state65 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task reject65 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out65b" 2>&1 || fail "rejected record retry failed"
[ ! -e "$repo65/agent-junk65.txt" ] || fail "driver-written rejected state lost cleanup list"
[ "$(git -C "$repo65" show HEAD:f)" = tracked ] || fail "rejected record retry lost tracked edit"

# Files created between an interrupted agent and --resume survive a later rejection.
repo66=$tmp/repo66
state66=$tmp/state66
mkdir -p "$repo66"
git -C "$repo66" init -q
git -C "$repo66" config user.email test@example.com
git -C "$repo66" config user.name test
echo base > "$repo66/f"
git -C "$repo66" add f
git -C "$repo66" commit -qm base
cat > "$tmp/agent66.sh" <<'AG'
#!/bin/sh
set -eu
echo junk > "$AUTO_ITERATE_REPO/agent-junk66.txt"
exit 1
AG
chmod +x "$tmp/agent66.sh"
echo 0 > "$state66"
set +e
STUB_REPO=$repo66 STUB_STATE=$state66 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume66 --agent-cmd "$tmp/agent66.sh" \
    --max 1 >"$tmp/out66a" 2>&1
rc=$?
set -e
[ "$rc" -ne 0 ] || fail "interrupted agent did not fail"
echo keep > "$repo66/user-created66.txt"
STUB_REPO=$repo66 STUB_STATE=$state66 STUB_REJECT=1 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume66 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out66b" 2>&1 || fail "resumed rejection failed"
[ -e "$repo66/user-created66.txt" ] || fail "user file created before resume was deleted"

# --verify-cmd alone still gives the reviewer verification evidence.
repo67=$tmp/repo67
state67=$tmp/state67
mkdir -p "$repo67"
git -C "$repo67" init -q
git -C "$repo67" config user.email test@example.com
git -C "$repo67" config user.name test
echo base > "$repo67/f"
git -C "$repo67" add f
git -C "$repo67" commit -qm base
echo 0 > "$state67"
STUB_REPO=$repo67 STUB_STATE=$state67 STUB_EVIDENCE_COPY=$tmp/copy67 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task verify67 --agent-cmd "$tmp/agent20.sh" \
    --verify-cmd 'echo verify-marker' --max 1 >"$tmp/out67" 2>&1 || fail "verify evidence run failed"
[ -f "$tmp/copy67" ] || fail "reviewer received no evidence file"
grep -q "verify-marker" "$tmp/copy67" || fail "reviewer evidence missed verify output"
grep -q "exit 0" "$tmp/copy67" || fail "reviewer evidence missed verify exit status"

# Files created between a rejected round and --resume survive a second rejection.
repo68=$tmp/repo68
state68=$tmp/state68
mkdir -p "$repo68"
git -C "$repo68" init -q
git -C "$repo68" config user.email test@example.com
git -C "$repo68" config user.name test
echo base > "$repo68/f"
git -C "$repo68" add f
git -C "$repo68" commit -qm base
echo 0 > "$state68"
set +e
STUB_REPO=$repo68 STUB_STATE=$state68 STUB_REJECT=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task reject68 --agent-cmd "$tmp/agent20.sh" \
    --stop-on-reject --max 1 >"$tmp/out68a" 2>&1
rc=$?
set -e
[ "$rc" -eq 5 ] || fail "first rejection exited $rc"
echo keep > "$repo68/user-created68.txt"
STUB_REPO=$repo68 STUB_STATE=$state68 STUB_REJECT=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task reject68 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out68b" 2>&1 || fail "retry rejection failed"
[ -e "$repo68/user-created68.txt" ] || fail "user file created before retry resume was deleted"

# Untracked user files survive rejection recovery from verify/staged/reviewing.
check_resume_preserve() {
    _phase=$1
    _repo=$tmp/repo-preserve-$_phase
    _state=$tmp/state-preserve-$_phase
    mkdir -p "$_repo"
    git -C "$_repo" init -q
    git -C "$_repo" config user.email test@example.com
    git -C "$_repo" config user.name test
    echo base > "$_repo/f"
    git -C "$_repo" add f
    git -C "$_repo" commit -qm base
    _base=$(git -C "$_repo" rev-parse HEAD)
    _gitdir=$(git -C "$_repo" rev-parse --absolute-git-dir)
    mkdir -p "$_gitdir/auto-iterate"
    echo 0 > "$_state"
    _vcmd=
    [ "$_phase" = verify ] && _vcmd=true
    printf '{"phase":"%s","iteration":1,"review":"","commit":"","tag":"","message":"v1","channel":"","head":"%s","clean":1,"feedback":"","tree":"","untracked":"","verify_cmd":"%s","verify_timeout":0,"version_plan":"","version_acceptance":"","evidence":""}\n' \
        "$_phase" "$_base" "$_vcmd" > "$_gitdir/auto-iterate/resume-$_phase.json"
    echo keep > "$_repo/user-$_phase.txt"
    STUB_REPO=$_repo STUB_STATE=$_state STUB_REJECT=1 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
        "$here/auto-iterate.sh" --task "resume-$_phase" --agent-cmd "$tmp/agent20.sh" \
        --resume --max 1 >"$tmp/out-preserve-$_phase" 2>&1 || fail "$_phase resume failed"
    [ -e "$_repo/user-$_phase.txt" ] || fail "$_phase recovery deleted a user file"
}
check_resume_preserve verify
check_resume_preserve staged
check_resume_preserve reviewing

# An explicit empty cleanup list must not fall back to the protected paths.
repo69=$tmp/repo69
state69=$tmp/state69
mkdir -p "$repo69"
git -C "$repo69" init -q
git -C "$repo69" config user.email test@example.com
git -C "$repo69" config user.name test
echo base > "$repo69/f"
git -C "$repo69" add f
git -C "$repo69" commit -qm base
cat > "$tmp/agent69.sh" <<'AG'
#!/bin/sh
set -eu
exit 1
AG
chmod +x "$tmp/agent69.sh"
echo 0 > "$state69"
set +e
STUB_REPO=$repo69 STUB_STATE=$state69 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume69 --agent-cmd "$tmp/agent69.sh" \
    --max 1 >"$tmp/out69a" 2>&1
rc=$?
set -e
[ "$rc" -ne 0 ] || fail "interrupted agent did not fail"
echo keep > "$repo69/user-created69.txt"
set +e
STUB_REPO=$repo69 STUB_STATE=$state69 STUB_REJECT=1 STUB_FAIL_RECORD=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume69 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out69b" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "rejection record failure exited $rc"
STUB_REPO=$repo69 STUB_STATE=$state69 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task resume69 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out69c" 2>&1 || fail "rejected record retry failed"
[ -e "$repo69/user-created69.txt" ] || fail "empty cleanup list deleted a protected user file"

# Saved protections survive a second resume after the protected file was staged.
repo70=$tmp/repo70
state70=$tmp/state70
mkdir -p "$repo70"
git -C "$repo70" init -q
git -C "$repo70" config user.email test@example.com
git -C "$repo70" config user.name test
echo base > "$repo70/f"
git -C "$repo70" add f
git -C "$repo70" commit -qm base
base70=$(git -C "$repo70" rev-parse HEAD)
echo keep > "$repo70/user-protected70.txt"
git -C "$repo70" add user-protected70.txt
gitdir70=$(git -C "$repo70" rev-parse --absolute-git-dir)
mkdir -p "$gitdir70/auto-iterate"
protected70=$(printf 'user-protected70.txt\0' | base64 -w0)
printf '[]\n' > "$tmp/history70.json"
echo 0 > "$state70"
printf '{"phase":"reviewing","iteration":1,"review":"v1-1","commit":"","tag":"","message":"v1","channel":"","head":"%s","clean":1,"feedback":"","tree":"","untracked":"%s","verify_cmd":"","verify_timeout":0,"version_plan":"","version_acceptance":"","evidence":""}\n' \
    "$base70" "$protected70" > "$gitdir70/auto-iterate/review70.json"
: > "$state70.paused"
STUB_REPO=$repo70 STUB_STATE=$state70 STUB_HISTORY=$tmp/history70.json STUB_REJECT=1 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task review70 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out70" 2>&1 || fail "protected reviewing resume failed"
[ -e "$repo70/user-protected70.txt" ] || fail "saved protection was dropped after staging"

# A user file created and staged after interruption is protected on resume.
repo71=$tmp/repo71
state71=$tmp/state71
mkdir -p "$repo71"
git -C "$repo71" init -q
git -C "$repo71" config user.email test@example.com
git -C "$repo71" config user.name test
echo base > "$repo71/f"
git -C "$repo71" add f
git -C "$repo71" commit -qm base
base71=$(git -C "$repo71" rev-parse HEAD)
gitdir71=$(git -C "$repo71" rev-parse --absolute-git-dir)
mkdir -p "$gitdir71/auto-iterate"
printf '[]\n' > "$tmp/history71.json"
echo 0 > "$state71"
printf '{"phase":"reviewing","iteration":1,"review":"v1-1","commit":"","tag":"","message":"v1","channel":"","head":"%s","clean":1,"feedback":"","tree":"","untracked":"","verify_cmd":"","verify_timeout":0,"version_plan":"","version_acceptance":"","evidence":""}\n' \
    "$base71" > "$gitdir71/auto-iterate/review71.json"
echo keep > "$repo71/user-staged71.txt"
git -C "$repo71" add user-staged71.txt
: > "$state71.paused"
STUB_REPO=$repo71 STUB_STATE=$state71 STUB_HISTORY=$tmp/history71.json STUB_REJECT=1 STUB_ITERATIONS=1 PATH="$bin:$PATH" \
    "$here/auto-iterate.sh" --task review71 --agent-cmd "$tmp/agent20.sh" \
    --resume --max 1 >"$tmp/out71" 2>&1 || fail "staged user file resume failed"
[ -e "$repo71/user-staged71.txt" ] || fail "newly staged user file was deleted"

# Finalize verify-range failure is reported without undoing the approved commit.
repo72=$tmp/repo72
state72=$tmp/state72
mkdir -p "$repo72"
git -C "$repo72" init -q
git -C "$repo72" config user.email test@example.com
git -C "$repo72" config user.name test
echo base > "$repo72/f"
git -C "$repo72" add f
git -C "$repo72" commit -qm base
echo 0 > "$state72"
set +e
STUB_REPO=$repo72 STUB_STATE=$state72 STUB_ITERATIONS=1 STUB_FAIL_VERIFY_RANGE=1 PATH="$bin:$PATH" \
    STUB_NOTIFY_OUT=$tmp/notify72.out \
    "$here/auto-iterate.sh" --task vr72 --agent-cmd "$tmp/agent.sh" \
    --verify-range --notify-cmd "$tmp/notify.sh" --max 1 >"$tmp/out72" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "failed verify-range exited $rc"
grep -q "verify-range failed" "$tmp/out72" || fail "no verify-range failure message"
[ "$(git -C "$repo72" rev-list --count HEAD)" -eq 2 ] || fail "verify-range failure rolled back the commit"
[ "$(git -C "$repo72" tag | wc -l)" -eq 1 ] || fail "verify-range failure lost the tag"
grep -q '^event=failed$' "$tmp/notify72.out" || fail "failed run did not notify failure"
grep -q '^exit_code=1$' "$tmp/notify72.out" || fail "failure notification has the wrong exit code"

echo "test-driver: ok"
