#!/usr/bin/env bash
# E2: anonymous read of the public remote by the gate's own user, in the real sandbox.
#
# This script only PRINTS commands. Run them yourself; they need sudo. It never touches
# /etc, the binaries or the runner.
#
# What the gate actually does (crates/review-gate/src/milestone/sync.rs, fetch_remote):
#   git -c safe.directory=* -C <state>/milestones/remote.git ls-remote <remote_url> <notes ref>
#   git -c safe.directory=* -C ... fetch -q --no-tags <remote_url> +refs/notes/review-gate-milestones:...
# It sets GIT_TERMINAL_PROMPT=0 and runs entirely in its private bare `remote.git`; the
# implementer's repo-local config is not read for the fetch. It does NOT clear the system or
# global git config or other inherited environment, so the checks below clear them explicitly
# (GIT_CONFIG_NOSYSTEM, GIT_CONFIG_GLOBAL=/dev/null, empty HOME/XDG) and also clear
# credential.helper/http.extraHeader, to prove the fetch needs no credential at all.
set -euo pipefail

GATE_USER=${GATE_USER:-reviewgate}
GATE_STATE=${GATE_STATE:-/var/lib/reviewgate/state}
GATE_HOME=${GATE_HOME:-/var/lib/reviewgate}
CACHE=$GATE_STATE/milestones/remote.git
REPO=${REPO:-/home/administrator/projects/Chrome-MCP}
BASE=${BASE:-3cc549109c700409b8069dbe920a96759cb47faa}
HEAD=${HEAD:-bbc5aeb74005d76ad8fde20b89a5e9337f5c7aec}
REMOTE=${REMOTE:-https://github.com/victormshan/WebUI-AutoTest.git}
NOTES_REF=refs/notes/review-gate-milestones
PUBKEY=${PUBKEY:-ITU9Oc3AbiC/Q/LYPwld2b+OC2rxlqHbvCO7IQcGdXw=}
TASK_BOOK=plans/M2-design.md
EVIDENCE_DIR=plans/M2-evidence

cat <<EOF
# E2 anonymous remote read
# gate user=$GATE_USER state=$GATE_STATE
# cache=$CACHE
# remote=$REMOTE
# base=$BASE head=$HEAD
#
# 0) The gate's effective settings, read-only. fetch_remote inherits the service environment
#    (/etc/review-gate/env plus systemd), sets GIT_TERMINAL_PROMPT=0, and reads the private
#    cache only. There is no credential setup anywhere in this path.
#    Print variable NAMES only; /etc/review-gate/env contains API keys and must never be echoed.
sudo sed -n 's/^\([A-Za-z_][A-Za-z0-9_]*\)=.*/\1/p' /etc/review-gate/env | grep -E 'GIT_|CREDENTIAL|SSH_|HTTP' || echo '(no git credential entries in /etc/review-gate/env)'
systemctl show review-gate.service -p Environment -p EnvironmentFiles -p User -p ProtectHome -p ProtectSystem -p ReadWritePaths

# 1) Evidence BEFORE: the private cache's fetch time and notes ref.
sudo stat -c '%y %n' $CACHE/FETCH_HEAD
sudo git -C $CACHE for-each-ref $NOTES_REF --format='%(objectname) %(refname)'
sudo git -C $CACHE log -1 --format='%H %cI %s' $NOTES_REF

# 2) Anonymous fetch in a sandbox matching the unit (User/ProtectHome/ProtectSystem/PrivateTmp).
#    Empty credentials and no prompt; if this succeeds, no credential was used.
sudo systemd-run --pipe --wait --collect -p User=$GATE_USER -p Group=$GATE_USER -p ProtectSystem=strict -p ProtectHome=read-only -p PrivateTmp=yes -p ReadWritePaths=/var/lib/reviewgate -p "Environment=HOME=$GATE_HOME XDG_CONFIG_HOME=$GATE_HOME/.config GIT_TERMINAL_PROMPT=0 GIT_ASKPASS=/bin/false SSH_ASKPASS=/bin/false GIT_SSH_COMMAND=/bin/false GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null" git -c credential.helper= -c http.extraHeader= -C $CACHE fetch --no-tags $REMOTE "+$NOTES_REF:$NOTES_REF"

# Expected: exit 0, no username/password prompt, no "could not read Username".
# If it fails with an auth prompt, STOP: do not add a credential. Report the error; the
# gate would be non-anonymous and must not be deployed for this phase.

# 3) Evidence AFTER: FETCH_HEAD must be newer, and the ref must exist.
sudo stat -c '%y %n' $CACHE/FETCH_HEAD
sudo git -C $CACHE for-each-ref $NOTES_REF --format='%(objectname) %(refname)'

# 4) Read the task book at base, the evidence directory, and the review note at head, as the
#    gate user, with an empty environment. safe.directory is the EXACT repository path only.
#    (Never use safe.directory=*.)
sudo -u $GATE_USER env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=$GATE_HOME XDG_CONFIG_HOME=$GATE_HOME/.config GIT_TERMINAL_PROMPT=0 GIT_ASKPASS=/bin/false  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null  git -C $REPO -c safe.directory=$REPO cat-file -t $BASE

sudo -u $GATE_USER env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=$GATE_HOME XDG_CONFIG_HOME=$GATE_HOME/.config GIT_TERMINAL_PROMPT=0 GIT_ASKPASS=/bin/false  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null  git -C $REPO -c safe.directory=$REPO cat-file blob $BASE:$TASK_BOOK | sha256sum

# Expected sha256 of the task book at base:
#   571e53f1063f62f85f941182b0222f5eb7571442026d23face6d2515b26ecce6

sudo -u $GATE_USER env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=$GATE_HOME XDG_CONFIG_HOME=$GATE_HOME/.config GIT_TERMINAL_PROMPT=0 GIT_ASKPASS=/bin/false  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null  git -C $REPO -c safe.directory=$REPO ls-tree -r --name-only $HEAD $EVIDENCE_DIR

sudo -u $GATE_USER env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=$GATE_HOME XDG_CONFIG_HOME=$GATE_HOME/.config GIT_TERMINAL_PROMPT=0 GIT_ASKPASS=/bin/false  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null  git -C $REPO -c safe.directory=$REPO log -1 --format='%H %an <%ae> %cI %s' $HEAD

# The review-gate note on head (the commit attestation), and its verification with the pinned key.
sudo git -C $REPO notes --ref=refs/notes/review-gate list $HEAD
sudo git -C $REPO notes --ref=refs/notes/review-gate show $HEAD
review-gate verify --repo $REPO --pubkey "$PUBKEY" $HEAD

# 5) Only if git reports a dubious-ownership error: add the exact repository path, never '*'.
#    sudo -u $GATE_USER git config --global --add safe.directory $REPO
#    (and remove it afterwards with: --unset-all safe.directory $REPO)

# Record all raw output (including errors) in the evidence directory. The gate's fetch leaves
# FETCH_HEAD in its private cache; that timestamp plus the ref above is the evidence that the
# fetch came from this remote.
EOF
