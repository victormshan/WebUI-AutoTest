#!/bin/sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/opencode-agent-test.XXXXXX")
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

fail() { echo "test-opencode-agent: FAIL: $*" >&2; exit 1; }

new_repo() {
    r=$tmp/repo-$1
    mkdir -p "$r"
    git -C "$r" init -q
    git -C "$r" config user.email test@example.com
    git -C "$r" config user.name test
    echo base >"$r/base.txt"
    git -C "$r" add base.txt
    git -C "$r" commit -qm base
    printf '%s\n' "$r"
}

fake=$tmp/fake-opencode
cat >"$fake" <<'FAKE'
#!/bin/sh
set -eu
printf '%s\n' "$*" >"$FAKE_ARGS"
printf '%s\0' "$@" >"${FAKE_ARGV:-/dev/null}"
prompt=
seen=0
for a do
    if [ "$seen" = 1 ]; then prompt=$a; fi
    if [ "$a" = "--" ]; then seen=1; fi
done
printf '%s' "$prompt" >"$FAKE_PROMPT"
[ "$seen" = 1 ] || { echo "no -- separator" >&2; exit 93; }
[ -n "$prompt" ] || { echo "empty prompt" >&2; exit 94; }
[ "$PWD" = "$FAKE_REPO" ] || { echo "bad cwd: $PWD" >&2; exit 91; }
[ -z "${REVIEW_GATE_TOKEN_FILE:-}" ] || { echo "gate token leaked" >&2; exit 90; }
[ -z "${REVIEW_GATE_TOKEN:-}" ] || { echo "gate token leaked" >&2; exit 90; }
[ -z "${REVIEW_GATE_URL:-}" ] || { echo "gate url leaked" >&2; exit 90; }
[ -z "${REVIEW_GATE_API_KEY:-}" ] || { echo "gate api key leaked" >&2; exit 90; }
case "${FAKE_MODE:-change}" in
    change) printf 'changed\n' >"$FAKE_REPO/out.txt" ;;
    stage-change)
        printf 'staged-change\n' >"$FAKE_REPO/base.txt"
        git -C "$FAKE_REPO" add base.txt
        ;;
    change-weird) printf 'weird-changed\n' >"$FAKE_REPO/$FAKE_WEIRD" ;;
    none) ;;
    commit)
        printf 'changed\n' >"$FAKE_REPO/out.txt"
        git -C "$FAKE_REPO" add -A
        git -C "$FAKE_REPO" commit -qm opencode-commit
        ;;
    move-tag)
        printf 'changed\n' >"$FAKE_REPO/out.txt"
        git -C "$FAKE_REPO" commit --allow-empty -qm tmp-tag-target
        git -C "$FAKE_REPO" tag -f v1 HEAD
        git -C "$FAKE_REPO" reset -q --hard HEAD^
        ;;
    fail) echo "fake failure" >&2; exit 7 ;;
    *) echo "unknown mode" >&2; exit 92 ;;
esac
echo "fake opencode output"
FAKE
chmod +x "$fake"

feedback=$tmp/feedback.txt
printf 'previous review says: fix the thing\n' >"$feedback"

run_wrapper() {
    r=$1
    mode=$2
    ev=$3
    set +e
    AUTO_ITERATE_REPO="$r" AUTO_ITERATE_ITERATION=1 AUTO_ITERATE_TASK='demo/task' \
    AUTO_ITERATE_GOAL='implement the demo' AUTO_ITERATE_ACCEPTANCE='tests pass' \
    AUTO_ITERATE_VERSION_ACCEPTANCE='version acceptance text' \
    AUTO_ITERATE_FEEDBACK="$feedback" AUTO_ITERATE_EVIDENCE="$ev" \
    REVIEW_GATE_TOKEN_FILE=/must/not/leak REVIEW_GATE_TOKEN=secret REVIEW_GATE_URL=http://leak \
    REVIEW_GATE_API_KEY=another-secret \
    OPENCODE_BIN="$fake" FAKE_REPO="$r" FAKE_MODE="$mode" \
    FAKE_ARGS="$tmp/args-$mode" FAKE_ARGV="$tmp/argv-$mode" FAKE_PROMPT="$tmp/prompt-$mode" \
    sh "$here/opencode-agent.sh" >"$tmp/out-$mode" 2>"$tmp/err-$mode"
    rc=$?
    set -e
    printf '%s\n' "$rc"
}

r=$(new_repo ok)
ev=$tmp/evidence-ok.log
printf 'driver preface\n' >"$ev"
rc=$(run_wrapper "$r" change "$ev")
[ "$rc" -eq 0 ] || fail "success exited $rc: $(cat "$tmp/err-change")"
[ -f "$r/out.txt" ] || fail "opencode change was not made"
grep -q "fake opencode output" "$ev" || fail "evidence does not contain opencode output"
grep -q "implement the demo" "$tmp/prompt-change" || fail "goal missing from prompt"
grep -q "tests pass" "$tmp/prompt-change" || fail "acceptance missing from prompt"
grep -q "version acceptance text" "$tmp/prompt-change" || fail "version acceptance missing from prompt"
grep -q "driver preface" "$ev" || fail "driver evidence was truncated"
grep -q "fix the thing" "$tmp/prompt-change" || fail "feedback missing from prompt"
grep -q "Task: demo/task" "$tmp/prompt-change" || fail "task missing from prompt"
grep -q "^run --auto -- " "$tmp/args-change" \
    || fail "default opencode flags missing: $(cat "$tmp/args-change")"
[ "$(git -C "$r" rev-list --count HEAD)" -eq 1 ] || fail "wrapper created a commit"
[ "$(git -C "$r" tag | wc -l)" -eq 0 ] || fail "wrapper created a tag"

r=$(new_repo model)
rc=$(AUTO_ITERATE_REPO="$r" AUTO_ITERATE_ITERATION=2 OPENCODE_BIN="$fake" \
    OPENCODE_MODEL=test/model OPENCODE_AGENT=build OPENCODE_FLAGS='--pure --format json' \
    AUTO_ITERATE_EVIDENCE="$tmp/evidence-model.log" \
    FAKE_REPO="$r" FAKE_ARGS="$tmp/args-model" FAKE_PROMPT="$tmp/prompt-model" \
    sh "$here/opencode-agent.sh" >/dev/null 2>"$tmp/err-model"; printf '%s\n' "$?")
[ "$rc" -eq 0 ] || fail "model flags exited $rc: $(cat "$tmp/err-model")"
grep -q -- "--pure --format json -m test/model --agent build -- " "$tmp/args-model" \
    || fail "model/agent flags missing: $(cat "$tmp/args-model")"

r=$(new_repo none)
rc=$(run_wrapper "$r" none "$tmp/evidence-none.log")
[ "$rc" -eq 1 ] || fail "no-change exited $rc"
grep -q "made no changes" "$tmp/err-none" || fail "no-change message missing"

r=$(new_repo dirty-none)
echo dirty >"$r/base.txt"
rc=$(run_wrapper "$r" none "$tmp/evidence-dirty-none.log")
[ "$rc" -eq 1 ] || fail "dirty no-change exited $rc"
grep -q "made no changes" "$tmp/err-none" || fail "dirty no-change message missing"

r=$(new_repo staged-change)
echo staged >"$r/base.txt"
git -C "$r" add base.txt
rc=$(run_wrapper "$r" stage-change "$tmp/evidence-stage-change.log")
[ "$rc" -eq 0 ] || fail "staged change exited $rc: $(cat "$tmp/err-none")"
[ "$(git -C "$r" show :base.txt)" = staged-change ] || fail "staged change was not made"

r=$(new_repo weird-name)
weird=$(printf 'weird\tname.txt')
printf 'weird-old\n' >"$r/$weird"
export FAKE_WEIRD="$weird"
rc=$(run_wrapper "$r" change-weird "$tmp/evidence-weird.log")
[ "$rc" -eq 0 ] || fail "quoted filename change exited $rc: $(cat "$tmp/err-none")"
[ "$(cat "$r/$weird")" = weird-changed ] || fail "quoted filename change was not made"
unset FAKE_WEIRD

r=$(new_repo commit)
rc=$(run_wrapper "$r" commit "$tmp/evidence-commit.log")
[ "$rc" -eq 1 ] || fail "commit exited $rc"
grep -q "changed HEAD" "$tmp/err-commit" || fail "commit guard did not fire"
[ "$(git -C "$r" rev-list --count HEAD)" -eq 2 ] || fail "fake commit did not happen"

r=$(new_repo move-tag)
git -C "$r" tag v1
rc=$(run_wrapper "$r" move-tag "$tmp/evidence-move-tag.log")
[ "$rc" -eq 1 ] || fail "moved-tag exited $rc"
grep -q "changed tags" "$tmp/err-move-tag" || fail "moved-tag guard missing"

r=$(new_repo fail)
rc=$(run_wrapper "$r" fail "$tmp/evidence-fail.log")
[ "$rc" -eq 7 ] || fail "failure exited $rc"
grep -q "opencode run failed" "$tmp/err-fail" || fail "failure message missing"

r=$(new_repo inside)
rc=$(run_wrapper "$r" change "$r/evidence.log")
[ "$rc" -eq 1 ] || fail "inside-evidence exited $rc"
grep -q "outside the repository" "$tmp/err-change" || fail "inside-evidence guard missing"

ln -s "$r/evidence-link.log" "$tmp/evidence-link.log"
rc=$(run_wrapper "$r" change "$tmp/evidence-link.log")
[ "$rc" -eq 1 ] || fail "symlink-evidence exited $rc"
grep -q "outside the repository" "$tmp/err-change" || fail "symlink-evidence guard missing"

r=$(new_repo windows)
cp "$fake" "$tmp/fake.exe"
set +e
AUTO_ITERATE_REPO="$r" AUTO_ITERATE_ITERATION=1 OPENCODE_BIN="$tmp/fake.exe" \
    sh "$here/opencode-agent.sh" >"$tmp/out-win" 2>"$tmp/err-win"
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "windows guard exited $rc"
grep -q "Windows opencode cannot safely edit" "$tmp/err-win" || fail "windows guard message missing"

cp "$fake" "$tmp/fake.cmd"
set +e
AUTO_ITERATE_REPO="$r" AUTO_ITERATE_ITERATION=1 OPENCODE_BIN="$tmp/fake.cmd" \
    sh "$here/opencode-agent.sh" >"$tmp/out-cmd" 2>"$tmp/err-cmd"
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "windows shim guard exited $rc"
grep -q "Windows shell shim" "$tmp/err-cmd" || fail "windows shim guard message missing"

mkdir -p "$tmp/home/.local/bin"
cp "$fake" "$tmp/home/.local/bin/opencode"
rm -f "$tmp/args-default-path" "$tmp/marker-default-path"
r=$(new_repo default-path)
set +e
HOME="$tmp/home" PATH=/usr/bin:/bin \
AUTO_ITERATE_REPO="$r" AUTO_ITERATE_ITERATION=1 AUTO_ITERATE_EVIDENCE="$tmp/evidence-default-path.log" \
FAKE_REPO="$r" FAKE_ARGS="$tmp/args-default-path" FAKE_PROMPT="$tmp/prompt-default-path" \
sh "$here/opencode-agent.sh" >"$tmp/out-default-path" 2>"$tmp/err-default-path"
rc=$?
set -e
[ "$rc" -eq 0 ] || fail "HOME/.local/bin selection exited $rc: $(cat "$tmp/err-default-path")"
[ -f "$r/out.txt" ] || fail "HOME/.local/bin fake did not run"

r=$(new_repo default-evidence)
mkdir -p "$tmp/evdir"
set +e
TMPDIR="$tmp/evdir" \
AUTO_ITERATE_REPO="$r" AUTO_ITERATE_ITERATION=3 AUTO_ITERATE_TASK='t/3' \
OPENCODE_BIN="$fake" FAKE_REPO="$r" FAKE_ARGS="$tmp/args-default-evidence" \
FAKE_PROMPT="$tmp/prompt-default-evidence" \
sh "$here/opencode-agent.sh" >"$tmp/out-default-evidence" 2>"$tmp/err-default-evidence"
rc=$?
set -e
[ "$rc" -eq 0 ] || fail "default evidence exited $rc: $(cat "$tmp/err-default-evidence")"
ev=$tmp/evdir/auto-iterate-opencode-t_3-v3.log
[ -f "$ev" ] || fail "default evidence file missing: $ev"
grep -q "fake opencode output" "$ev" || fail "default evidence lacks output"

# driver --adapter selection (no gate call: these fail before any review-gate use)
driver=$here/auto-iterate.sh
set +e
sh "$driver" --task demo --adapter nope >"$tmp/driver-bad.out" 2>"$tmp/driver-bad.err"
rc=$?
set -e
[ "$rc" -eq 2 ] || fail "--adapter nope exited $rc"
grep -q "unknown adapter: nope (expected codex or opencode)" "$tmp/driver-bad.err" \
    || fail "unknown adapter message missing"

set +e
sh "$driver" --task demo --adapter opencode --agent-cmd true \
    >"$tmp/driver-both.out" 2>"$tmp/driver-both.err"
rc=$?
set -e
[ "$rc" -eq 2 ] || fail "--adapter with --agent-cmd exited $rc"
grep -q "exclusive" "$tmp/driver-both.err" || fail "exclusive message missing"

sh "$driver" --help | grep -q -- '--adapter NAME' || fail "driver help lacks --adapter"

set +e
sh -n "$here/opencode-agent.sh"
rc=$?
set -e
[ "$rc" -eq 0 ] || fail "shell syntax"

echo "test-opencode-agent: ok"
