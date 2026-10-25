#!/bin/bash
# Isolated real-binary check of the integrated bridge's observer path (bridge integration,
# Codex option B, condition 2): an observer reads task metadata over HTTP; an observer is refused
# everything else; an agent is refused the observe routes; no token is 401. Temporary HOME, its
# own port; nothing shared with the live bridge.
set -euo pipefail
BIN=${1:?path to the agent-bridge binary}
H=$(mktemp -d); trap 'kill $PID 2>/dev/null || true; rm -rf "$H"' EXIT
mkdir -p "$H/.config/agent-bridge"
sha() { printf '%s' "$1" | sha256sum | cut -d' ' -f1; }
printf '{"claude":"%s","dsh":"%s"}' "$(sha claude-token-0123456789abcdef0123)" "$(sha dsh-token-0123456789abcdef01234567)" > "$H/.config/agent-bridge/agents.json"
printf '{"review-gate":"%s"}' "$(sha observer-token-0123456789abcdef0123)" > "$H/.config/agent-bridge/observers.json"
PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
HOME="$H" "$BIN" serve --listen 127.0.0.1:$PORT > "$H/serve.log" 2>&1 & PID=$!
for i in $(seq 100); do curl -fs "http://127.0.0.1:$PORT/v1/health" >/dev/null 2>&1 && break; sleep 0.1; done
echo "binary sha256: $(sha256sum "$BIN" | cut -d' ' -f1)"
grep -o 'read-only observers.*' "$H/serve.log" || grep -i observer "$H/serve.log" || true
U="http://127.0.0.1:$PORT"
call() { curl -s -o "$H/out.json" -w '%{http_code}' -X "$1" ${3:+-H "Authorization: Bearer $3"} -H 'Content-Type: application/json' ${4:+-d "$4"} "$U$2"; }
fail=0; want() { local got=$1 exp=$2 what=$3; if [ "$got" = "$exp" ]; then echo "ok   $exp $what"; else echo "FAIL got $got want $exp: $what"; fail=1; fi; }
AG=claude-token-0123456789abcdef0123; OB=observer-token-0123456789abcdef0123
want "$(call POST /v1/tasks $AG '{"id":"obs-e2e-1","kind":"task","protocol":"2","body":"secret body text","meta":{"to":"dsh","title":"observer e2e"}}')" 200 "agent creates a task"
want "$(call GET /v1/observe/tasks $OB)" 200 "observer lists tasks"
grep -q '"obs-e2e-1"' "$H/out.json" && echo "ok   the list names the task" || { echo "FAIL the list does not name the task"; fail=1; }
want "$(call GET /v1/observe/tasks/obs-e2e-1 $OB)" 200 "observer reads one task"
grep -q 'secret body text' "$H/out.json" && { echo "FAIL the observer view contains the message body"; fail=1; } || echo "ok   metadata only: no message body in the observer view"
for r in "GET /v1/tasks" "GET /v1/tasks/obs-e2e-1" "GET /v1/identity" "GET /v1/inbox" "GET /v1/health" "POST /v1/tasks/obs-e2e-1/messages"; do set -- $r; want "$(call $1 $2 $OB '{}')" 403 "observer refused: $1 $2"; done
want "$(call GET /v1/observe/tasks $AG)" 403 "agent refused the observe route"
want "$(call GET /v1/observe/tasks '' )" 401 "no token on the observe route"
want "$(call GET /v1/observe/tasks wrong-token-0123456789abcdef01234567)" 401 "wrong token on the observe route"
[ $fail = 0 ] && echo "ALL OK" || { echo "SOME FAILED"; exit 1; }
