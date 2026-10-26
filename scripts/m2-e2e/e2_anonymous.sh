#!/usr/bin/env bash
# E2: the gate reads the public remote anonymously, in the real sandbox of its unit.
#
# Run once, as root, after the deployment (RUNBOOK §9):
#   set -o pipefail; sudo bash scripts/m2-e2e/e2_anonymous.sh <code revision X> 2>&1 | tee -a <log>
#   (pipefail, so a failure of the script is not hidden by tee)
# (the head is an argument, not an environment variable, so sudo's environment handling does
# not matter)
# Read only. It changes nothing in /etc, the binaries, the unit or the gate's state, except what
# the gate itself does when asked for a status (its own fetch into its private cache).
#
# What it shows (Codex review of b7a3a91, n8 R2-R4, n10 §5-6):
#  1. Names, never values: the environment the unit and the running process carry
#     (Environment= assignments are only counted; PassEnvironment names; EnvironmentFiles
#     paths; /proc/<MainPID>/environ split on NUL, names only). Since fd34eae the gate's git runs
#     with a cleared environment, so none of these reach it; this is the record of what exists.
#  2. The gate's OWN fetch: one read-only status call makes the gate run fetch_remote; its `sync`
#     answer is the result of that anonymous read (Equal/RemoteBehind/RemoteAhead/Missing, or
#     Stop("remote unreachable: <git's error>")). The private cache's FETCH_HEAD and refs, before and after.
#  3. Reads under the unit's own sandbox: the properties are read from the running unit and passed
#     to systemd-run as parsed values (no shell string splicing); every command's exit status is
#     checked (set -euo pipefail); the deployed binary verifies the head's attestation with the
#     pinned key.
set -euo pipefail
umask 077

[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 2; }

UNIT=${UNIT:-review-gate.service}
GATE_STATE=${GATE_STATE:-/var/lib/reviewgate/state}
CACHE=$GATE_STATE/milestones/remote.git
GATE_BIN=${GATE_BIN:-/usr/local/bin/review-gate}
REPO=${REPO:-/home/administrator/projects/Chrome-MCP}
BASE=${BASE:-3cc549109c700409b8069dbe920a96759cb47faa}
HEAD=${1:?usage: e2_anonymous.sh <E4 head (the code revision X)>}
PHASE=${PHASE:-m2}
PUBKEY=${PUBKEY:-ITU9Oc3AbiC/Q/LYPwld2b+OC2rxlqHbvCO7IQcGdXw=}
TOKEN_FILE=${TOKEN_FILE:-/etc/review-gate/client.token}
URL=${URL:-http://127.0.0.1:7878}
TASK_BOOK=plans/M2-design.md
TASK_BOOK_SHA256=571e53f1063f62f85f941182b0222f5eb7571442026d23face6d2515b26ecce6
EVIDENCE_DIR=plans/M2-evidence
NOTES=refs/notes/review-gate

step() { printf '\n== %s\n' "$*"; }

step "0. unit and binary"
systemctl is-active "$UNIT"
MAINPID=$(systemctl show "$UNIT" -p MainPID --value)
[ "$MAINPID" -gt 0 ]
echo "MainPID $MAINPID started $(ps -o lstart= -p "$MAINPID")"
sha256sum "$GATE_BIN" "/proc/$MAINPID/exe"

step "1. environment: names only"
# Environment= values can hold secrets: only how many assignments there are.
systemctl show "$UNIT" -p Environment --value | python3 -c '
import shlex, sys
try:
    n = len(shlex.split(sys.stdin.read()))
except ValueError:
    n = "unparsable (not shown)"
print("Environment= assignments:", n)'
echo "PassEnvironment names: [$(systemctl show "$UNIT" -p PassEnvironment --value)]"
echo "EnvironmentFiles: $(systemctl show "$UNIT" -p EnvironmentFiles --value)"
python3 - "$MAINPID" <<'PY'
import re, sys
raw = open(f"/proc/{sys.argv[1]}/environ", "rb").read()
names = []
for item in raw.split(b"\0"):
    if item:
        name = item.split(b"=", 1)[0].decode("ascii", "replace")
        names.append(name if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name) else "<not a name>")
print("process environment names:", " ".join(sorted(names)))
PY

step "2. the gate's own anonymous fetch (one read-only status call)"
cache_state() {
    stat -c '%y %n' "$CACHE/FETCH_HEAD" 2>/dev/null || echo "no FETCH_HEAD yet"
    git -c "safe.directory=$CACHE" -C "$CACHE" for-each-ref refs/notes --format='%(objectname) %(refname)'
}
cache_state
TMP=$(mktemp -d /root/.e2-XXXXXX)
trap 'rm -rf "$TMP"' EXIT
python3 - "$TOKEN_FILE" "$TMP/h" <<'PY'
import os, sys
token = open(sys.argv[1]).read().strip()          # the client's normalization (trim)
fd = os.open(sys.argv[2], os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
os.write(fd, f"Authorization: Bearer {token}\n".encode())
os.close(fd)
PY
T0=$(date -u +%FT%T.%NZ)
CODE=$(curl -sS --connect-timeout 5 --max-time 300 -o "$TMP/status.json" -w '%{http_code}' -H @"$TMP/h" "$URL/milestones/$PHASE")
T1=$(date -u +%FT%T.%NZ)
echo "status call $T0 .. $T1 -> HTTP $CODE"
[ "$CODE" = 200 ]
python3 - "$TMP/status.json" <<'PY'
import json, sys
s = json.load(open(sys.argv[1]))
sync = s.get("sync")
# The status protocol (flow.rs Status.sync): the Debug form of Sync, a string. Success is Equal,
# RemoteBehind {..}, RemoteAhead {..} or Missing; a failed read is Stop("…"). Anything else, a
# missing field or another type stops the script.
if not isinstance(sync, str):
    sys.exit(f"status has no string sync field ({sync!r}): stop and report")
print("sync (the gate's own read of the remote):", sync)
print("released:", s.get("released"), " reasons:", len(s.get("reasons", [])))
if sync.startswith("Stop"):
    sys.exit("the gate's own read of the remote did not succeed: stop and report")
if not (sync == "Equal" or sync == "Missing" or sync.startswith(("RemoteBehind {", "RemoteAhead {"))):
    sys.exit(f"unexpected sync value {sync[:80]!r}: stop and report")
PY
cache_state

step "3. the unit's sandbox, as configured on the running unit"
# Captured first, then split: a failure of systemctl or of the parser stops the script
# (mapfile < <(...) would not report the producer's exit status).
PROPS_TXT=$(systemctl show "$UNIT" --property=User,Group,ProtectSystem,ProtectHome,PrivateTmp,NoNewPrivileges,ReadWritePaths,ReadOnlyPaths,InaccessiblePaths,ProtectKernelTunables,ProtectControlGroups,RestrictAddressFamilies,UMask)
mapfile -t PROPS <<< "$PROPS_TXT"
printf '%s\n' "${PROPS[@]}"
# systemd-run arguments from the parsed values. Paths are space-separated and have no spaces in
# this deployment; any entry that is not an absolute path stops the script.
RUNARGS_TXT=$(python3 - "${PROPS[@]}" <<'PY'
import sys
props = dict(p.split("=", 1) for p in sys.argv[1:])
for required in ["User", "Group", "ProtectSystem", "ProtectHome", "ReadWritePaths"]:
    if not props.get(required):
        sys.exit(f"the unit has no {required}: refusing to build a partial sandbox")
out = []
for k in ["User", "Group", "ProtectSystem", "ProtectHome", "PrivateTmp", "NoNewPrivileges",
          "ProtectKernelTunables", "ProtectControlGroups", "UMask"]:
    if props.get(k, ""):
        out.append(f"--property={k}={props[k]}")
for k in ["ReadWritePaths", "ReadOnlyPaths", "InaccessiblePaths"]:
    for path in props.get(k, "").split():
        if not path.lstrip("-").startswith("/"):
            sys.exit(f"unexpected {k} entry {path!r}: stop")
        out.append(f"--property={k}={path}")
print("\n".join(out))
PY
)
mapfile -t RUNARGS <<< "$RUNARGS_TXT"
[ "${#RUNARGS[@]}" -ge 5 ]
echo "mirrored: exactly the properties listed above as --property; not mirrored (and not claimed):"
echo "  RestrictAddressFamilies, and every other unit setting not in that list"
in_sandbox() {
    systemd-run --quiet --pipe --wait --collect "${RUNARGS[@]}" \
        --setenv=HOME=/nonexistent --setenv=GIT_CONFIG_NOSYSTEM=1 \
        --setenv=GIT_CONFIG_GLOBAL=/dev/null --setenv=GIT_TERMINAL_PROMPT=0 "$@"
}
G=(/usr/bin/git -c "safe.directory=$REPO" -C "$REPO")

step "4. reads in that sandbox (each exit status checked)"
in_sandbox "${G[@]}" cat-file -t "$BASE"
in_sandbox "${G[@]}" cat-file -t "$HEAD"
in_sandbox "${G[@]}" cat-file blob "$BASE:$TASK_BOOK" > "$TMP/book"
GOT=$(sha256sum "$TMP/book" | cut -d' ' -f1)
echo "task book at base: $GOT"
[ "$GOT" = "$TASK_BOOK_SHA256" ] || { echo "task book sha256 differs: stop" >&2; exit 1; }
in_sandbox "${G[@]}" ls-tree -r --name-only "$HEAD" "$EVIDENCE_DIR"
in_sandbox "${G[@]}" log -1 --format='%H %an <%ae> %cI %s' "$HEAD"
in_sandbox "${G[@]}" notes --ref="$NOTES" list "$HEAD"
in_sandbox "${G[@]}" notes --ref="$NOTES" show "$HEAD"
in_sandbox "$GATE_BIN" verify --repo "$REPO" --pubkey "$PUBKEY" "$HEAD"

step "E2 done: every command above succeeded"
