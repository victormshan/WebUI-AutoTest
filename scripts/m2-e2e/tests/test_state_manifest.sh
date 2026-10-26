#!/usr/bin/env bash
# Fixture test of state_manifest.sh (no root): complete manifest including *.tmp; an unreadable
# file fails with no output; an existing output is not overwritten; output unwritable fails.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SCRIPT=$HERE/../state_manifest.sh
TMP=$(mktemp -d); trap 'chmod -R u+rwx "$TMP" 2>/dev/null || true; rm -rf "$TMP"' EXIT
mkdir -p "$TMP/state/tasks" "$TMP/state/reviews/t" "$TMP/out"
echo '{"id":"t"}' > "$TMP/state/tasks/t.json"
echo 'partial' > "$TMP/state/tasks/u.json.tmp"
echo '{"id":"v1-1"}' > "$TMP/state/reviews/t/v1-1.json"

bash "$SCRIPT" "$TMP/state" "$TMP/out/M0" > "$TMP/run1.txt"
[ "$(wc -l < "$TMP/out/M0")" = 3 ] && echo "ok - 3 files hashed" || { cat "$TMP/out/M0"; exit 1; }
grep -q 'u.json.tmp$' "$TMP/out/M0" && echo "ok - the .tmp file is in the manifest"
grep -q 'u.json.tmp' "$TMP/run1.txt" && echo "ok - the .tmp file is listed"
(cd "$TMP/state" && sha256sum -c "$TMP/out/M0" > /dev/null) && echo "ok - the manifest verifies"

set +e; bash "$SCRIPT" "$TMP/state" "$TMP/out/M0" > /dev/null 2>&1; rc=$?; set -e
[ "$rc" != 0 ] && echo "ok - an existing output is not overwritten (exit $rc)"

chmod 000 "$TMP/state/reviews/t/v1-1.json"
set +e; bash "$SCRIPT" "$TMP/state" "$TMP/out/M1" > "$TMP/run2.txt" 2>&1; rc=$?; set -e
[ "$rc" != 0 ] && [ ! -e "$TMP/out/M1" ] && ! ls "$TMP/out" | grep -q part \
  && echo "ok - an unreadable file fails (exit $rc), no output, no partial file" || { cat "$TMP/run2.txt"; exit 1; }
chmod 644 "$TMP/state/reviews/t/v1-1.json"

chmod 500 "$TMP/out"
set +e; bash "$SCRIPT" "$TMP/state" "$TMP/out/M2" > /dev/null 2>&1; rc=$?; set -e
[ "$rc" != 0 ] && echo "ok - an output directory the caller cannot write fails (exit $rc)"
echo "ALL OK"
