#!/bin/sh
# Fake Codex for the M1 fd inventory: lists the descriptors this process holds, as Codex would.
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
cat >/dev/null
(for f in /proc/$$/fd/*; do echo "${f##*/} $(readlink $f)"; done) > "$out"
