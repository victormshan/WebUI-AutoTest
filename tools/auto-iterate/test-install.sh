#!/bin/sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/auto-iterate-install.XXXXXX")
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT
fail() { echo "test-install: FAIL: $*" >&2; exit 1; }

skills=$tmp/skills/auto-iterate
bin=$tmp/bin
"$here/install.sh" --skill-dir "$skills" --bin-dir "$bin" >"$tmp/out" ||
    fail "install failed"
[ -L "$skills/SKILL.md" ] || fail "SKILL.md is not a link"
[ -L "$bin/auto-iterate.sh" ] || fail "driver is not a link"
[ -L "$bin/codex-agent.sh" ] || fail "codex adapter is not a link"
[ "$(readlink "$skills/SKILL.md")" = "$here/SKILL.md" ] || fail "skill link target is wrong"
[ "$(readlink "$bin/auto-iterate.sh")" = "$here/auto-iterate.sh" ] || fail "driver link target is wrong"
[ "$(readlink "$bin/codex-agent.sh")" = "$here/codex-agent.sh" ] || fail "adapter link target is wrong"
"$here/install.sh" --skill-dir "$skills" --bin-dir "$bin" --check >"$tmp/check" ||
    fail "check failed"
grep -q "^ok: $skills/SKILL.md" "$tmp/check" || fail "check did not report the skill link"

conflict=$tmp/conflict
mkdir -p "$conflict"
echo keep >"$conflict/auto-iterate.sh"
set +e
"$here/install.sh" --bin-dir "$conflict" >"$tmp/conflict.out" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "regular-file conflict exited $rc"
grep -q "refusing to replace regular file" "$tmp/conflict.out" || fail "no conflict message"
[ "$(cat "$conflict/auto-iterate.sh")" = keep ] || fail "regular file was overwritten"
[ ! -e "$conflict/codex-agent.sh" ] || fail "installer continued past a conflict"

"$here/install.sh" --bin-dir "$conflict" --force >"$tmp/force" ||
    fail "force install failed"
[ -L "$conflict/auto-iterate.sh" ] || fail "force did not replace the regular file"
[ -L "$conflict/codex-agent.sh" ] || fail "force did not install the adapter"

dry=$tmp/dry
"$here/install.sh" --skill-dir "$dry" --dry-run >"$tmp/dry.out" || fail "dry-run failed"
[ ! -e "$dry/SKILL.md" ] || fail "dry-run created a link"
grep -q "would install: $dry/SKILL.md" "$tmp/dry.out" || fail "dry-run did not print the plan"
set +e
"$here/install.sh" --skill-dir "$dry" --check >"$tmp/dry.check" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "check on a missing link exited $rc"
grep -q "missing link" "$tmp/dry.check" || fail "check did not report a missing link"

echo "test-install: ok"
