#!/usr/bin/env bash
# Fixture test of drain_wait.sh: a stand-in /health that answers a scripted sequence.
# No root, no gate. Each case asserts drain_wait.sh's exit status.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SCRIPT=$HERE/../drain_wait.sh
TMP=$(mktemp -d); trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$TMP"' EXIT

serve() { # sequence-file -> prints the port
  python3 - "$1" > /dev/null 2>&1 <<'PY' &
import http.server, json, socket, sys
seq = json.load(open(sys.argv[1]))
state = {"i": 0}
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        step = seq[min(state["i"], len(seq) - 1)]; state["i"] += 1
        body = step["body"].encode()
        self.send_response(step["status"]); self.send_header("Content-Length", str(len(body)))
        self.end_headers(); self.wfile.write(body)
s = http.server.HTTPServer(("127.0.0.1", 0), H)
open(sys.argv[1] + ".port", "w").write(str(s.server_address[1]))
s.serve_forever()
PY
  SERVER=$!
  for _ in $(seq 50); do [ -s "$1.port" ] && break; sleep 0.1; done
}

health() { # draining idle active failures pending(json)
  python3 -c 'import json,sys; d,i,a,f,p=sys.argv[1:]; print(json.dumps({"ok":True,"drain":{"configured":True,"draining":d=="1","reason":"x","active_ops":int(a),"failure_count":int(f),"failures":[],"idle":i=="1"},"m1_jobs_running":0,"m2_pending":json.loads(p)}))' "$@"
}

case_() { # name want-exit total-min sequence-json
  local name=$1 want=$2 total=$3 seqf=$TMP/$1.json port rc
  printf '%s' "$4" > "$seqf"
  serve "$seqf"; port=$(cat "$seqf.port")
  set +e; POLL_SECS=1 timeout 60 bash "$SCRIPT" "http://127.0.0.1:$port" "$total" > "$TMP/$name.out" 2>&1; rc=$?; set -e
  kill "$SERVER" 2>/dev/null || true; wait "$SERVER" 2>/dev/null || true
  if [ "$rc" = "$want" ]; then echo "ok - $name: exit $rc"; else echo "FAIL - $name: exit $rc, want $want"; cat "$TMP/$name.out"; exit 1; fi
}

NOT_IDLE=$(health 1 0 1 0 0); IDLE=$(health 1 1 0 0 0)
j() { python3 -c 'import json,sys; print(json.dumps([{"status":int(s),"body":b} for s,b in zip(sys.argv[1::2],sys.argv[2::2])]))' "$@"; }

case_ idle-after-two-not-idle 0 5 "$(j 200 "$NOT_IDLE" 200 "$NOT_IDLE" 200 "$IDLE")"
case_ http-500 2 5 "$(j 500 "oops")"
case_ bad-json 2 5 "$(j 200 "{not json")"
case_ pending-null 2 5 "$(j 200 "$(health 1 1 0 0 null)")"
case_ field-missing 2 5 "$(j 200 '{"ok":true,"m2_pending":0}')"
case_ bool-as-int 2 5 "$(j 200 "$(python3 -c 'import json; print(json.dumps({"drain":{"draining":True,"idle":True,"active_ops":False,"failure_count":0},"m2_pending":0}))')")"
case_ idle-but-a-failure 3 0 "$(j 200 "$(health 1 0 0 1 0)")"
case_ never-idle-deadline 3 0 "$(j 200 "$NOT_IDLE")"
# Nothing listening: a port that was free a moment ago.
FREE=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
set +e; bash "$SCRIPT" "http://127.0.0.1:$FREE" 5 > "$TMP/refused.out" 2>&1; rc=$?; set -e
[ "$rc" = 2 ] && echo "ok - connection-refused: exit 2" || { echo "FAIL - connection-refused: exit $rc"; cat "$TMP/refused.out"; exit 1; }
echo "ALL OK"
