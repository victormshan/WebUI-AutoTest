#!/bin/sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/manifest-test.XXXXXX")
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

fail() { echo "test-manifest: FAIL: $*" >&2; exit 1; }
tool=$here/manifest-tool.sh
manifest=$tmp/versions.ndjson

python3 - "$manifest" <<'PY'
import json, sys
rows = [
    {"at": "2026-01-01T00:00:00Z", "task": "t", "version": 1, "iteration": 1,
     "review": "v1-1", "verdict": "approved", "commit": "0" * 40,
     "tag": "tag-1", "channel": "stub", "provider": "stub",
     "message": "first, with comma", "tree": "a", "acceptance": "a1",
     "verify_cmd": "true"},
    {"at": "2026-01-02T00:00:00Z", "task": "t", "version": 2, "iteration": 2,
     "review": "v2-1", "verdict": "approved", "commit": "1" * 40,
     "tag": "tag-2", "channel": "stub", "provider": "stub",
     "message": "second", "tree": "b", "acceptance": "a2",
     "verify_cmd": "true"},
]
with open(sys.argv[1], "w") as f:
    for row in rows:
        f.write(json.dumps(row) + "\n")
PY

out=$("$tool" validate "$manifest") || fail "valid manifest rejected"
[ "$out" = "ok 2 entries" ] || fail "validate output: $out"

out=$("$tool" export "$manifest" json) || fail "json export failed"
printf '%s' "$out" | python3 -c 'import json,sys; d=json.load(sys.stdin); assert len(d)==2; assert d[0]["message"]=="first, with comma"' \
    || fail "json export content"

out=$("$tool" export "$manifest" csv) || fail "csv export failed"
printf '%s' "$out" | python3 -c '
import csv, io, sys
rows = list(csv.DictReader(io.StringIO(sys.stdin.read())))
assert len(rows) == 2, rows
assert rows[0]["message"] == "first, with comma", rows[0]
' || fail "csv export content"

head -n 1 "$manifest" >> "$manifest"
set +e
out=$("$tool" validate "$manifest" 2>&1)
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "duplicate manifest exited $rc"
printf '%s' "$out" | grep -q "duplicate t v1" || fail "duplicate not reported"
"$tool" repair "$manifest" > "$tmp/repair1" || fail "duplicate repair failed"
grep -q "repaired 1 line" "$tmp/repair1" || fail "duplicate repair count"
[ "$(wc -l < "$manifest")" -eq 2 ] || fail "duplicate repair kept wrong line count"
"$tool" validate "$manifest" >/dev/null || fail "repaired duplicate manifest invalid"

printf 'not json\n' >> "$manifest"
set +e
"$tool" validate "$manifest" >/dev/null 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "corrupt manifest exited $rc"
"$tool" repair "$manifest" > "$tmp/repair2" || fail "corrupt repair failed"
grep -q "repaired 1 line" "$tmp/repair2" || fail "corrupt repair count"
[ "$(wc -l < "$manifest")" -eq 2 ] || fail "corrupt repair kept wrong line count"
"$tool" validate "$manifest" >/dev/null || fail "repaired corrupt manifest invalid"
quarantine=$(find "$tmp" -name 'versions.ndjson.corrupt.*' | wc -l)
[ "$quarantine" -eq 2 ] || fail "expected 2 quarantine files, got $quarantine"
grep -q "not json" "$tmp"/versions.ndjson.corrupt.* || fail "corrupt line not quarantined"

before=$(find "$tmp" -name 'versions.ndjson.corrupt.*' | wc -l)
"$tool" repair "$manifest" > "$tmp/repair3" || fail "clean repair failed"
[ "$(cat "$tmp/repair3")" = "ok 0 repaired" ] || fail "clean repair output"
after=$(find "$tmp" -name 'versions.ndjson.corrupt.*' | wc -l)
[ "$before" -eq "$after" ] || fail "clean repair created a quarantine file"

set +e
"$tool" validate "$tmp/missing.ndjson" >/dev/null 2>&1
rc=$?
set -e
[ "$rc" -eq 1 ] || fail "missing manifest exited $rc"

echo "test-manifest: ok"
