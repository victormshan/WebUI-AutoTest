#!/usr/bin/env bash
# E4 driver: register -> (the user runs admin confirm --yes) -> request -> poll -> [--publish] -> status.
#
# Every step's raw JSON is kept under $E4_OUT (default: ./e4-out/<UTC timestamp>).
# It never retries a side effect after a timeout: on an uncertain result it stops and tells
# you to inspect `status` first.
#
# This script does not run root commands. `admin milestone confirm --yes` is printed for the
# user, because that decision is the user's. `--publish` writes public notes; it is opt-in.
#
# Required for the implementer side: the same environment as any review-gate client
# (REVIEW_GATE_URL, REVIEW_GATE_TOKEN or REVIEW_GATE_TOKEN_FILE).
set -euo pipefail

PHASE=${PHASE:-m2}
REPO=${REPO:-/home/administrator/projects/Chrome-MCP}
BASE=${BASE:-3cc549109c700409b8069dbe920a96759cb47faa}
HEAD=${HEAD:-bbc5aeb74005d76ad8fde20b89a5e9337f5c7aec}
TASK_BOOK=${TASK_BOOK:-plans/M2-design.md}
EVIDENCE=${EVIDENCE:-plans/M2-evidence}
REPO_ID=${REPO_ID:-github.com/victormshan/WebUI-AutoTest}
REMOTE=${REMOTE:-https://github.com/victormshan/WebUI-AutoTest.git}
PUBKEY=${PUBKEY:-ITU9Oc3AbiC/Q/LYPwld2b+OC2rxlqHbvCO7IQcGdXw=}
CONDITIONS_FILE=${CONDITIONS_FILE:-}
POLL_TIMEOUT=${E4_POLL_TIMEOUT:-1900}
POLL_INTERVAL=${E4_POLL_INTERVAL:-15}

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
if [ -n "${REVIEW_GATE_BIN:-}" ]; then
  BIN=$REVIEW_GATE_BIN
elif [ -x "$ROOT/target/release/review-gate" ]; then
  BIN=$ROOT/target/release/review-gate
else
  BIN=/usr/local/bin/review-gate
fi
[ -x "$BIN" ] || { echo "no review-gate binary (set REVIEW_GATE_BIN)" >&2; exit 2; }
command -v git >/dev/null || { echo "git is required" >&2; exit 2; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }

BASE_FULL=$(git -C "$REPO" rev-parse --verify "$BASE^{commit}")
HEAD_FULL=$(git -C "$REPO" rev-parse --verify "$HEAD^{commit}")

STAMP=$(date -u +%Y%m%dT%H%M%SZ)
OUT=${E4_OUT:-$PWD/e4-out/$STAMP}
mkdir -p "$OUT"
echo "E4 output: $OUT"

GATE_URL=${REVIEW_GATE_URL:-http://127.0.0.1:7878}
export REVIEW_GATE_URL=$GATE_URL

get_status() { # out; prints the CLI exit status (1 = not released yet)
  local rc
  set +e
  "$BIN" milestone status --phase "$PHASE" >"$1" 2>"$1.err"
  rc=$?
  set -e
  [ -s "$1.err" ] && cat "$1.err" >&2
  printf '%s\n' "$rc"
}

state_of() { # status-json path
  python3 - "$1" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
s = d["state"]
c = s.get("current")
print("registered=%s released=%s current=%s" % (
    s["registered"], d["released"],
    "-" if c is None else "%s/%s" % (c["request_id"], c["state"])))
PY
}

current_state() { # status-json path -> prints state or none
  python3 - "$1" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
c = d["state"].get("current")
print("none" if c is None else c["state"])
PY
}

registered() { # status-json path
  python3 - "$1" <<'PY'
import json, sys
print("yes" if json.load(open(sys.argv[1]))["state"]["registered"] else "no")
PY
}

request_id() {
  python3 - "$1" <<'PY'
import json, sys
c = json.load(open(sys.argv[1]))["state"].get("current")
print("" if c is None else c["request_id"])
PY
}

cmd_register() {
  local args=(milestone register --phase "$PHASE" --base "$BASE_FULL"
              --task-book "$TASK_BOOK" --evidence-dir "$EVIDENCE")
  if [ -n "$CONDITIONS_FILE" ]; then
    while IFS= read -r line || [ -n "$line" ]; do
      [ -z "$line" ] && continue
      args+=(--condition "$line")
    done < "$CONDITIONS_FILE"
  fi
  set +e
  "$BIN" "${args[@]}" >"$OUT/01-register.json" 2>"$OUT/01-register.err"
  local rc=$?
  set -e
  cat "$OUT/01-register.json"
  [ -s "$OUT/01-register.err" ] && cat "$OUT/01-register.err" >&2
  [ "$rc" = 0 ] || { echo "register failed (exit $rc); not retrying" >&2; exit "$rc"; }

  local code
  code=$(get_status "$OUT/02-status-after-register.json")
  cat "$OUT/02-status-after-register.json"
  echo
  echo "status_exit=$code $(state_of "$OUT/02-status-after-register.json")"
  if ! grep -q '"phase"' "$OUT/01-register.json"; then
    echo "register output is not a JSON application; stop and inspect" >&2
    exit 1
  fi
  cat <<EOF

Registration application written. The user must confirm it (this is the user's decision):
  preview: sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json confirm $PHASE
  execute: sudo -u reviewgate /usr/local/bin/review-gate admin milestone --state /var/lib/reviewgate/state --config /etc/review-gate/milestones.json --yes confirm $PHASE

Then run:
  $0 continue
EOF
}

cmd_continue() {
  local code
  code=$(get_status "$OUT/03-status-before-request.json")
  cat "$OUT/03-status-before-request.json"
  echo
  echo "status_exit=$code $(state_of "$OUT/03-status-before-request.json")"
  [ "$(registered "$OUT/03-status-before-request.json")" = yes ] || {
    cat <<EOF
Phase $PHASE is still not registered (the pending application is not in the signed log).
Ask the user to run the confirm command printed by '$0 register', then rerun '$0 continue'.
EOF
    exit 3
  }

  local cur
  cur=$(current_state "$OUT/03-status-before-request.json")
  if [ "$cur" != none ] && [ "${E4_ALLOW_NEW_REQUEST:-0}" != 1 ]; then
    cat <<EOF
Phase $PHASE already has a request ($(request_id "$OUT/03-status-before-request.json"), state $cur).
Not issuing another request: a second one could spend another audit and must never be used to
fish for APPROVE. Inspect status; if a new request is genuinely intended, set E4_ALLOW_NEW_REQUEST=1.
EOF
    exit 4
  fi

  set +e
  "$BIN" milestone request --phase "$PHASE" --head "$HEAD_FULL" >"$OUT/04-request.json" 2>"$OUT/04-request.err"
  local rc=$?
  set -e
  cat "$OUT/04-request.json"
  [ -s "$OUT/04-request.err" ] && cat "$OUT/04-request.err" >&2
  [ "$rc" = 0 ] || {
    echo "request failed or its outcome is uncertain (exit $rc): do NOT retry blindly; run status first" >&2
    get_status "$OUT/04-status-after-request.json" >/dev/null || true
    exit "$rc"
  }
  local ID
  ID=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["request_id"])' "$OUT/04-request.json")
  echo "request_id=$ID"

  local deadline=$((SECONDS + POLL_TIMEOUT)) n=0
  while :; do
    n=$((n + 1))
    local f
    f=$(printf '%s/05-status-%03d.json' "$OUT" "$n")
    code=$(get_status "$f")
    echo "$(date -u +%H:%M:%SZ) status_exit=$code $(state_of "$f")"
    local cs
    cs=$(current_state "$f")
    if [ "$cs" != Pending ]; then
      cp "$f" "$OUT/05-status-final.json"
      break
    fi
    [ "$SECONDS" -lt "$deadline" ] || {
      echo "polling cap reached ($POLL_TIMEOUT s); not cancelling or retrying. Inspect status $f." >&2
      exit 5
    }
    sleep "$POLL_INTERVAL"
  done

  # Extract the kernel-attested sender of the job separately from the signed log.
  # Read the token from REVIEW_GATE_TOKEN[_FILE] or the client's default files; never from argv.
  python3 - "$OUT" "$ID" "$GATE_URL" <<'PY'
import json, os, sys, urllib.request
out, rid, url = sys.argv[1:4]

def read_token():
    t = os.environ.get("REVIEW_GATE_TOKEN")
    if t and t.strip():
        return t.strip()
    candidates = [
        os.environ.get("REVIEW_GATE_TOKEN_FILE"),
        "/etc/review-gate/client.token",
        os.path.expanduser("~/.config/review-gate/token"),
    ]
    for p in candidates:
        if p and os.path.isfile(p):
            with open(p) as f:
                t = f.read().strip()
            if t:
                return t
    raise SystemExit("no review-gate token")

token = read_token()
req = urllib.request.Request(url + "/milestones/log", headers={"Authorization": "Bearer " + token})
text = urllib.request.urlopen(req, timeout=30).read().decode()
open(out + "/06-log.jsonl", "w").write(text)
job = None
for line in text.splitlines():
    if not line.strip():
        continue
    r = json.loads(line)
    if r.get("kind") == "job" and r.get("body", {}).get("request_id") == rid:
        job = r
if job is None:
    print("no job record yet for %s; keep the raw log" % rid, file=sys.stderr)
    sys.exit(0)
json.dump({"seq": job["seq"], "request_id": rid,
           "sender": job["body"].get("sender"),
           "response_status": job["body"].get("response", {}).get("status"),
           "error": job["body"].get("error")},
          open(out + "/06-job-sender.json", "w"), indent=2)
print(json.dumps(json.load(open(out + "/06-job-sender.json")), indent=2))
PY

  cat "$OUT/05-status-final.json"
  echo
  echo "final: $(state_of "$OUT/05-status-final.json")"
  python3 - "$OUT/05-status-final.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
print("released:", d["released"])
for r in d["reasons"]:
    print("reason:", r)
PY

  if [ "${E4_PUBLISH:-0}" = 1 ]; then
    cmd_publish
    return $?
  else
    cat <<EOF

Not publishing. To publish the signed log (ordinary push, never forced):
  E4_PUBLISH=1 $0 publish

The audit verdict is whatever the runner returned; REWORK is recorded, never retried.
EOF
  fi
}

cmd_publish() {
  local code
  code=$(get_status "$OUT/03-status-before-publish.json")
  cat "$OUT/03-status-before-publish.json"
  echo
  echo "status_exit=$code $(state_of "$OUT/03-status-before-publish.json")"
  [ "$(registered "$OUT/03-status-before-publish.json")" = yes ] || {
    echo "phase $PHASE is not registered; refusing to publish" >&2
    exit 3
  }
  local cs
  cs=$(current_state "$OUT/03-status-before-publish.json")
  if [ "$cs" = Pending ] || [ "$cs" = none ]; then
    echo "phase $PHASE has no finished request (current=$cs); refusing to publish now" >&2
    exit 4
  fi
  set +e
  "$BIN" milestone publish --repo "$REPO" --remote "$REMOTE" --pubkey "$PUBKEY" \
    --repo-id "$REPO_ID" >"$OUT/07-publish.json" 2>"$OUT/07-publish.err"
  local prc=$?
  set -e
  cat "$OUT/07-publish.json"
  [ -s "$OUT/07-publish.err" ] && cat "$OUT/07-publish.err" >&2
  code=$(get_status "$OUT/08-status-after-publish.json")
  echo "status_exit=$code $(state_of "$OUT/08-status-after-publish.json")"
  cat "$OUT/08-status-after-publish.json"
  echo
  [ "$prc" = 0 ] || echo "publish exit $prc: published but not yet archived is possible; do not push again, rerun status" >&2
  return "$prc"
}

case "${1:-}" in
  register) cmd_register ;;
  continue) cmd_continue ;;
  publish) cmd_publish ;;
  *) echo "usage: $0 register|continue|publish" >&2; exit 2 ;;
esac
