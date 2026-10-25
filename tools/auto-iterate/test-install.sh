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
[ -L "$bin/opencode-agent.sh" ] || fail "opencode adapter is not a link"
[ "$(readlink "$skills/SKILL.md")" = "$here/SKILL.md" ] || fail "skill link target is wrong"
[ "$(readlink "$bin/auto-iterate.sh")" = "$here/auto-iterate.sh" ] || fail "driver link target is wrong"
[ "$(readlink "$bin/codex-agent.sh")" = "$here/codex-agent.sh" ] || fail "codex adapter link target is wrong"
[ "$(readlink "$bin/opencode-agent.sh")" = "$here/opencode-agent.sh" ] || fail "opencode adapter link target is wrong"
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
[ ! -e "$conflict/opencode-agent.sh" ] || fail "installer continued past a conflict"

"$here/install.sh" --bin-dir "$conflict" --force >"$tmp/force" ||
    fail "force install failed"
[ -L "$conflict/auto-iterate.sh" ] || fail "force did not replace the regular file"
[ -L "$conflict/codex-agent.sh" ] || fail "force did not install the codex adapter"
[ -L "$conflict/opencode-agent.sh" ] || fail "force did not install the opencode adapter"

upgrade=$tmp/upgrade
mkdir -p "$tmp/old/tools/auto-iterate" "$upgrade"
echo old > "$tmp/old/tools/auto-iterate/SKILL.md"
ln -s "$tmp/old/tools/auto-iterate/SKILL.md" "$upgrade/SKILL.md"
"$here/install.sh" --skill-dir "$upgrade" --upgrade >"$tmp/upgrade.out" ||
    fail "upgrade failed"
[ "$(readlink "$upgrade/SKILL.md")" = "$here/SKILL.md" ] || fail "upgrade did not migrate the link"
grep -q "^upgrade ok$" "$tmp/upgrade.out" || fail "upgrade did not report success"
echo keep > "$upgrade/user.conf"
set +e
"$here/install.sh" --skill-dir "$upgrade" --upgrade >"$tmp/upgrade2.out" 2>&1
rc=$?
set -e
[ "$rc" -eq 0 ] || fail "idempotent upgrade exited $rc"
[ "$(cat "$upgrade/user.conf")" = keep ] || fail "upgrade disturbed user data"
upbin=$tmp/upgrade-bin
mkdir -p "$tmp/old2/tools/auto-iterate" "$upbin"
echo old > "$tmp/old2/tools/auto-iterate/opencode-agent.sh"
ln -s "$tmp/old2/tools/auto-iterate/opencode-agent.sh" "$upbin/opencode-agent.sh"
"$here/install.sh" --bin-dir "$upbin" --upgrade >"$tmp/upgrade-bin.out" ||
    fail "adapter upgrade failed"
[ "$(readlink "$upbin/opencode-agent.sh")" = "$here/opencode-agent.sh" ] ||
    fail "upgrade did not migrate the opencode adapter link"
[ -L "$upbin/codex-agent.sh" ] || fail "upgrade did not install the missing codex adapter"

un=$tmp/uninstall
"$here/install.sh" --skill-dir "$un/skills" --bin-dir "$un/bin" >/dev/null || fail "uninstall setup failed"
echo keep > "$un/bin/user-tool"
"$here/install.sh" --skill-dir "$un/skills" --bin-dir "$un/bin" --uninstall >"$tmp/un.out" ||
    fail "uninstall failed"
[ ! -e "$un/skills/SKILL.md" ] || fail "uninstall kept the skill link"
[ ! -e "$un/bin/auto-iterate.sh" ] || fail "uninstall kept the driver link"
[ ! -e "$un/bin/codex-agent.sh" ] || fail "uninstall kept the codex adapter link"
[ ! -e "$un/bin/opencode-agent.sh" ] || fail "uninstall kept the opencode adapter link"
[ "$(cat "$un/bin/user-tool")" = keep ] || fail "uninstall removed a foreign file"
"$here/install.sh" --skill-dir "$un/skills" --bin-dir "$un/bin" --uninstall >"$tmp/un2.out" ||
    fail "idempotent uninstall failed"
grep -q "^absent: $un/bin/auto-iterate.sh$" "$tmp/un2.out" || fail "idempotent uninstall output"

foreign=$tmp/foreign
mkdir -p "$foreign"
echo keep > "$foreign/auto-iterate.sh"
set +e
"$here/install.sh" --bin-dir "$foreign" --uninstall >"$tmp/foreign.out" 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "foreign uninstall exited $rc"
grep -q "not ours" "$tmp/foreign.out" || fail "foreign uninstall did not warn"
[ "$(cat "$foreign/auto-iterate.sh")" = keep ] || fail "foreign uninstall removed a regular file"

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
