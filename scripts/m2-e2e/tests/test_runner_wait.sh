#!/usr/bin/env bash
# Fixture test of runner_wait.sh with stand-in pgrep/ss (no root).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SCRIPT=$HERE/../runner_wait.sh
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
fake() { # name exit-code output
  printf '#!/bin/sh\nprintf "%%s" "%s"\nexit %s\n' "$3" "$2" > "$TMP/$1"; chmod +x "$TMP/$1"
}
case_() { # name want total
  set +e; POLL_SECS=1 PGREP=$TMP/pgrep SS=$TMP/ss timeout 30 bash "$SCRIPT" "$3" > "$TMP/$1.out" 2>&1; rc=$?; set -e
  [ "$rc" = "$2" ] && echo "ok - $1: exit $rc" || { echo "FAIL - $1: exit $rc, want $2"; cat "$TMP/$1.out"; exit 1; }
}
fake pgrep 1 ""; fake ss 0 "u_str LISTEN 0 4096 /run/audit-runner/runner.sock 1 * 0"
case_ idle 0 5
fake pgrep 0 "4242 codex exec"; case_ codex-running-deadline 3 0
fake pgrep 1 ""; fake ss 0 "u_str ESTAB 0 0 /run/audit-runner/runner.sock 7 * 8"; case_ connection-deadline 3 0
fake pgrep 2 ""; case_ pgrep-error 2 5
fake pgrep 1 ""; fake ss 1 ""; case_ ss-error 2 5
echo "ALL OK"
