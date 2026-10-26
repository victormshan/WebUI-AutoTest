#!/usr/bin/env bash
# E4 preflight: read-only raw facts. No sudo, no writes, no network.
#
# Prints the full 40-hex base/head, the task book's full sha256, commit/merge counts, every
# verify-range line (raw), the full public key and where it came from, and the diff size.
# The final audit input size can only be known when a request is made: the gate assembles it
# from fixed objects and refuses it if it is over audit-runner's 4 MiB limit.
set -euo pipefail

REPO=${REPO:-/home/administrator/projects/Chrome-MCP}
BASE=${BASE:-3cc549109c700409b8069dbe920a96759cb47faa}
HEAD=${HEAD:-b423a579139fb5e7f47e64e976755a900ade547c}
TASK_BOOK=${TASK_BOOK:-plans/M2-design.md}
EVIDENCE_DIR=${EVIDENCE_DIR:-plans/M2-evidence}
NOTES_REF=${NOTES_REF:-refs/notes/review-gate}

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

section() { printf '\n===== %s =====\n' "$1"; }

section "identity"
BASE_FULL=$(git -C "$REPO" rev-parse --verify "$BASE^{commit}")
HEAD_FULL=$(git -C "$REPO" rev-parse --verify "$HEAD^{commit}")
printf 'repo=%s\n' "$REPO"
printf 'base=%s\n' "$BASE_FULL"
printf 'head=%s\n' "$HEAD_FULL"
printf 'base_short=%s head_short=%s\n' "${BASE_FULL:0:12}" "${HEAD_FULL:0:12}"
printf 'binary=%s\n' "$BIN"

section "task book at base"
git -C "$REPO" cat-file -e "$BASE_FULL:$TASK_BOOK" && echo "cat-file -e: ok"
printf 'size_bytes='
git -C "$REPO" cat-file blob "$BASE_FULL:$TASK_BOOK" | wc -c
printf 'sha256='
git -C "$REPO" cat-file blob "$BASE_FULL:$TASK_BOOK" | sha256sum | awk '{print $1}'
echo "evidence directory present at head:"
git -C "$REPO" cat-file -e "$HEAD_FULL:$EVIDENCE_DIR" && echo "  yes: $EVIDENCE_DIR"
git -C "$REPO" ls-tree -r --name-only "$HEAD_FULL" "$EVIDENCE_DIR"

section "commits in base..head"
printf 'rev_list_count='
git -C "$REPO" rev-list --count "$BASE_FULL..$HEAD_FULL"
printf 'merge_count='
git -C "$REPO" rev-list --merges --count "$BASE_FULL..$HEAD_FULL"
git -C "$REPO" log --reverse --format='%H %an <%ae> %cI %s' "$BASE_FULL..$HEAD_FULL"

section "diff size"
printf 'git_diff_bytes='
git -C "$REPO" diff "$BASE_FULL" "$HEAD_FULL" | wc -c
printf 'git_diff_stat_tail:\n'
git -C "$REPO" diff --stat "$BASE_FULL" "$HEAD_FULL" | tail -3

section "public key and its source"
PUBKEY=${PUBKEY:-}
if [ -n "$PUBKEY" ]; then
  PUBKEY_SOURCE="environment REVIEW_GATE_PUBKEY/PUBKEY"
else
  WORKFLOW=$REPO/.github/workflows/review-gate.yml
  if [ -f "$WORKFLOW" ] && grep -q 'REVIEW_GATE_PUBKEY:' "$WORKFLOW"; then
    PUBKEY=$(sed -n 's/^[[:space:]]*REVIEW_GATE_PUBKEY:[[:space:]]*"\(.*\)".*/\1/p' "$WORKFLOW" | head -1)
    PUBKEY_SOURCE="$WORKFLOW (the pinned value in the workflow)"
  elif curl -fsS --max-time 3 http://127.0.0.1:7878/pubkey >/dev/null 2>&1; then
    PUBKEY=$(curl -fsS --max-time 3 http://127.0.0.1:7878/pubkey | sed -n 's/.*"key"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
    PUBKEY_SOURCE="live GET http://127.0.0.1:7878/pubkey"
  else
    echo "no pinned public key found (set PUBKEY)" >&2
    exit 2
  fi
fi
printf 'pubkey=%s\n' "$PUBKEY"
printf 'pubkey_len=%s\n' "${#PUBKEY}"
printf 'pubkey_source=%s\n' "$PUBKEY_SOURCE"
printf 'live_service_pubkey='
if curl -fsS --max-time 3 http://127.0.0.1:7878/pubkey 2>/dev/null; then echo; else echo '(service not reachable or no key; not required for preflight)'; fi

section "notes ref and verify-range (raw)"
if git -C "$REPO" rev-parse --verify --quiet "$NOTES_REF" >/dev/null; then
  printf 'notes_ref=%s %s\n' "$NOTES_REF" "$(git -C "$REPO" rev-parse "$NOTES_REF")"
else
  echo "MISSING $NOTES_REF in $REPO: fetch it before E4 (do not proceed on a guess):" >&2
  echo "  git -C $REPO fetch origin '+refs/notes/review-gate:refs/notes/review-gate'" >&2
  exit 1
fi

set +e
"$BIN" verify-range --repo "$REPO" --pubkey "$PUBKEY" "$BASE_FULL..$HEAD_FULL"
VR=$?
set -e
printf 'verify_range_exit=%s\n' "$VR"

section "caveat"
cat <<'EOF'
The final audit input size is only known when the gate assembles it for a request; it is not
the diff size printed above. The gate refuses an over-limit input before writing anything
(audit-runner MAX_INPUT = 4 MiB), so do not trim input to make it pass.
EOF
exit "$VR"
