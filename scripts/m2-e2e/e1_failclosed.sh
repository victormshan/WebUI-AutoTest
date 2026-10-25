#!/usr/bin/env bash
# E1: fail-closed startup + the actual status answers for an unregistered phase.
#
# Isolated: own state directory, own port, own tokens, a throwaway git repo and a
# file:// bare remote. It never contacts the production runner or bridge.
#
# Asserts, for each broken configuration, that `review-gate serve` exits non-zero and
# binds no port. Then starts one good isolated instance and records the real answers:
#   - a phase that was never applied for;
#   - a phase applied for but not confirmed by the user (the pending file is not in
#     the signed log, so status is the same shaped answer, never 404/200-released).
#
# It needs no sudo. It refuses to run as root (to keep the owner check meaningful).
set -euo pipefail

if [ "$(id -u)" = 0 ]; then
  echo "run as a normal user, not root: the token owner check must be exercised" >&2
  exit 2
fi

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
BIN=${REVIEW_GATE_BIN:-$ROOT/target/release/review-gate}
[ -x "$BIN" ] || { echo "no review-gate binary at $BIN (build or set REVIEW_GATE_BIN)" >&2; exit 2; }

command -v git >/dev/null || { echo "git is required" >&2; exit 2; }
command -v curl >/dev/null || { echo "curl is required" >&2; exit 2; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }

TMP=$(mktemp -d "${TMPDIR:-/tmp}/m2-e1.XXXXXX")
PIDS=()
cleanup() {
  for p in "${PIDS[@]:-}"; do
    [ -n "$p" ] && kill "$p" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  rm -rf "$TMP"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok - $*"; }

# ---- fixtures -----------------------------------------------------------------

REPO=$TMP/repo
mkdir -p "$REPO/plans" "$REPO/ev"
git -C "$REPO" init -q
git -C "$REPO" config user.name e1
git -C "$REPO" config user.email e1@example.invalid
printf 'E1 task book\n' > "$REPO/plans/task.md"
printf 'evidence for E1\n' > "$REPO/ev/a.txt"
git -C "$REPO" add -A
git -C "$REPO" commit -qm 'E1 fixture'
BASE=$(git -C "$REPO" rev-parse HEAD)

REMOTE=$TMP/remote.git
git init -q --bare "$REMOTE"
REMOTE_URL="file://$REMOTE"

STATE=$TMP/state
mkdir -p "$STATE"

CLIENT=$TMP/client.token
head -c 48 /dev/urandom | base64 -w0 > "$CLIENT"
chmod 0600 "$CLIENT"
BRIDGE=$TMP/bridge.token
head -c 48 /dev/urandom | base64 -w0 > "$BRIDGE"
chmod 0600 "$BRIDGE"

free_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

mkcfg() { # out repo remote token
  python3 - "$1" "$2" "$3" "$4" <<'PY'
import json, sys
out, repo, remote, token = sys.argv[1:5]
json.dump({
    "repo_id": "e1-fixture",
    "repo": repo,
    "remote_url": remote,
    "runner_socket": "/nonexistent/e1-runner.sock",
    "runner_uid": 4242,
    "runner_cgroup": "/system.slice/e1-never.service",
    "job_timeout_secs": 1800,
    "auditor": "openai",
    "implementers": ["claude", "opencode"],
    "bridge_url": "http://127.0.0.1:1",
    "bridge_token_file": token,
}, open(out, "w"))
PY
}

mutate() { # file python-expression over d
  python3 - "$1" "$2" <<'PY'
import json, sys
p, expr = sys.argv[1], sys.argv[2]
d = json.load(open(p))
exec(expr)
json.dump(d, open(p, "w"))
PY
}

no_listener() { # port
  python3 - "$1" <<'PY'
import socket, sys
s = socket.socket()
s.settimeout(0.5)
try:
    s.connect(("127.0.0.1", int(sys.argv[1])))
except OSError:
    print("no-listener")
    raise SystemExit(0)
print("LISTENING")
raise SystemExit(1)
PY
}

wait_health() { # port
  for _ in $(seq 1 100); do
    if curl -fsS "http://127.0.0.1:$1/health" >/dev/null 2>&1; then return 0; fi
    sleep 0.1
  done
  return 1
}

# ---- 1. broken configurations: non-zero exit, no port -------------------------

GOOD_CFG=$TMP/good.json
mkcfg "$GOOD_CFG" "$REPO" "$REMOTE_URL" "$BRIDGE"

case_bad() { # name expected-message cfg [token]
  local name=$1 want=$2 cfg=$3 token=${4:-$CLIENT}
  local port state log rc
  port=$(free_port)
  state=$TMP/state-$name
  mkdir -p "$state"
  log=$TMP/$name.log
  set +e
  timeout 15 "$BIN" serve --state "$state" --listen "127.0.0.1:$port" \
    --token-file "$token" --milestones "$cfg" >"$log" 2>&1
  rc=$?
  set -e
  [ "$rc" -ne 0 ] || fail "$name: serve exited 0 for a broken configuration"
  [ "$rc" -ne 124 ] || fail "$name: serve did not exit (timeout)"
  grep -q -- "$want" "$log" || { cat "$log" >&2; fail "$name: expected $want in stderr"; }
  no_listener "$port" >/dev/null || fail "$name: a port was bound"
  ok "$name: exit $rc, no listener, stderr matched"
}

BAD=$TMP/relative.json; mkcfg "$BAD" "$REPO" "$REMOTE_URL" "$BRIDGE"; mutate "$BAD" 'd["repo"]="repo"'
case_bad relative-path "must be an absolute path" "$BAD"

BAD=$TMP/empty.json; mkcfg "$BAD" "$REPO" "$REMOTE_URL" "$BRIDGE"; mutate "$BAD" 'd["auditor"]=" "'
case_bad empty-field "every field must be set" "$BAD"

BAD=$TMP/unknown.json; mkcfg "$BAD" "$REPO" "$REMOTE_URL" "$BRIDGE"; mutate "$BAD" 'd["unknown_field"]=1'
case_bad unknown-field "unknown field" "$BAD"

BAD=$TMP/empty-impl.json; mkcfg "$BAD" "$REPO" "$REMOTE_URL" "$BRIDGE"; mutate "$BAD" 'd["implementers"]=[]'
case_bad empty-implementers "every field must be set" "$BAD"

BADTOKEN=$TMP/token-0644
head -c 48 /dev/urandom | base64 -w0 > "$BADTOKEN"
chmod 0644 "$BADTOKEN"
BADCFG=$TMP/token-mode.json; mkcfg "$BADCFG" "$REPO" "$REMOTE_URL" "$BADTOKEN"
case_bad token-mode-0644 "group or others" "$BADCFG"

SHORTTOKEN=$TMP/token-short
printf 'short\n' > "$SHORTTOKEN"
chmod 0600 "$SHORTTOKEN"
BADCFG=$TMP/token-short.json; mkcfg "$BADCFG" "$REPO" "$REMOTE_URL" "$SHORTTOKEN"
case_bad token-too-short "too short" "$BADCFG"

echo "SKIP owner-mismatch: constructing a token file owned by another uid needs root;"
echo "     the check is read_token_owned_by (crates/review-gate/src/milestone/serve.rs) and is"
echo "     covered by its unit test. No sudo is used here."

# ---- 2. one good isolated instance: the actual status answers -----------------

PORT=$(free_port)
GOOD_STATE=$TMP/state-good
mkdir -p "$GOOD_STATE"
"$BIN" serve --state "$GOOD_STATE" --listen "127.0.0.1:$PORT" \
  --token-file "$CLIENT" --milestones "$GOOD_CFG" >"$TMP/good.log" 2>&1 &
PIDS+=("$!")
wait_health "$PORT" || { cat "$TMP/good.log" >&2; fail "good instance did not become healthy"; }
ok "good isolated instance is healthy on 127.0.0.1:$PORT"

api() { # path out
  curl -sS -o "$2" -w '%{http_code}' -H "Authorization: Bearer $(cat "$CLIENT")" \
    "http://127.0.0.1:$PORT$1"
}

CODE=$(api /milestones/ghost "$TMP/ghost.json")
[ "$CODE" = 200 ] || { cat "$TMP/ghost.json" >&2; fail "unregistered phase: HTTP $CODE, expected 200"; }
python3 - "$TMP/ghost.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
assert d["state"]["registered"] is False, d
assert d["released"] is False, d
assert any("phase not registered" in r for r in d["reasons"]), d["reasons"]
print("ok - never-applied phase: 200, registered=false, released=false, reason says unregistered")
PY

# Apply (not confirm): pending/<phase>.json exists, but the signed log does not change.
python3 - "$TMP/apply.json" "$BASE" <<'PY'
import json, sys
json.dump({"phase": "pending-one", "base": sys.argv[2], "task_book": "plans/task.md",
           "evidence_dir": "ev", "conditions": []}, open(sys.argv[1], "w"))
PY
CODE=$(curl -sS -o "$TMP/apply.out" -w '%{http_code}' -X POST \
  -H "Authorization: Bearer $(cat "$CLIENT")" -H 'Content-Type: application/json' \
  --data-binary "@$TMP/apply.json" "http://127.0.0.1:$PORT/milestones")
[ "$CODE" = 200 ] || { cat "$TMP/apply.out" >&2; fail "apply: HTTP $CODE"; }
[ -f "$GOOD_STATE/milestones/pending/pending-one.json" ] || fail "apply did not write the pending file"

CODE=$(api /milestones/pending-one "$TMP/pending.json")
[ "$CODE" = 200 ] || { cat "$TMP/pending.json" >&2; fail "applied-not-confirmed: HTTP $CODE"; }
python3 - "$TMP/pending.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
assert d["state"]["registered"] is False, d
assert d["released"] is False, d
assert d["state"]["current"] is None, d["state"]["current"]
assert any("phase not registered (or not confirmed by the user)" in r for r in d["reasons"]), d["reasons"]
print("ok - applied but not confirmed: same 200/registered=false shape; pending is not in the signed log")
PY

kill "${PIDS[-1]}" 2>/dev/null || true
wait "${PIDS[-1]}" 2>/dev/null || true
unset 'PIDS[-1]'

echo "E1 PASS"
