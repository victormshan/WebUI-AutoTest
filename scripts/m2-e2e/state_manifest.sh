#!/usr/bin/env bash
# Full sha256 manifest of a directory (the gate's state), written by the shell that runs this
# script, so `sudo bash state_manifest.sh …` writes as root (a redirect outside sudo would not).
#
#   sudo bash scripts/m2-e2e/state_manifest.sh /var/lib/reviewgate /root/m2-id/state-M0.sha256
#
# Every regular file is hashed, *.tmp included (an interrupted write is state too); the *.tmp files
# are also listed on stdout with size and time. Any unreadable file, or any failing step, exits
# non-zero and leaves no output file (written to a temporary name, renamed only on success). An
# existing output file is never overwritten.
set -euo pipefail
umask 077
DIR=${1:?usage: state_manifest.sh <dir> <out>}
OUT=${2:?usage: state_manifest.sh <dir> <out>}
[ -d "$DIR" ] || { echo "not a directory: $DIR" >&2; exit 2; }
[ ! -e "$OUT" ] || { echo "refusing to overwrite $OUT" >&2; exit 2; }
PART=$(mktemp "$OUT.part.XXXXXX")
trap 'rm -f "$PART"' EXIT
(cd "$DIR" && find . -type f -print0 | sort -z | xargs -0 --no-run-if-empty sha256sum) > "$PART"
echo "temporary files (kept as they are):"
(cd "$DIR" && find . -type f -name '*.tmp' -printf '  %TY-%Tm-%Td %TH:%TM:%TS %s %p\n')
mv "$PART" "$OUT"
trap - EXIT
echo "manifest: $(wc -l < "$OUT") files, sha256 $(sha256sum "$OUT" | cut -d' ' -f1), $OUT"
