#!/bin/sh
# Fake Codex for the M1 stop-rule test: leaves a process behind outside its own process group
# (setsid), then reports a whole event stream and an answer.
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
cat >/dev/null
setsid sleep 4790 </dev/null >/dev/null 2>&1 &
left=$!
# Wait until it has its own session (left the group), else the group kill still reaches it.
i=0
while [ "$(awk '{print $6}' /proc/$left/stat 2>/dev/null)" != "$left" ] && [ $i -lt 100 ]; do
  sleep 0.02; i=$((i+1))
done
printf '%s\n' '{"type":"thread.started"}' '{"type":"turn.completed"}'
printf "VERDICT: APPROVE\nleft-pid=%s\n" "$left" > "$out"
