#!/bin/sh
# Vendor-neutral L2 driver: implement -> review -> commit/tag/record -> next version.
# The review-gate remains the only authority; this script never writes a verdict.
set -eu

usage() {
    cat <<'TXT'
usage: auto-iterate.sh --task ID --agent-cmd CMD [options]

  --task ID          review-gate task id (must already exist and be running)
  --agent-cmd CMD    shell command that implements one version (run in the repo)
  --message MSG      commit message (default: "auto-iterate: version <n>")
  --evidence FILE    optional evidence file passed to `review run`
  --max N            safety limit on rounds (default 10)
  --tag-prefix P     tag prefix (default auto-iterate)
  --log FILE         append one timestamped line per round to FILE
  --agent-timeout S  kill the agent command after S seconds (0 = off, default 0)
  --report FILE      write a Markdown run report to FILE
  --events FILE      append one JSON object per line to FILE
  --review-timeout S kill the external review client after S seconds (0 = off, default 0)
  --check            validate prerequisites and exit without changing anything
  --provider NAME    reviewer provider passed to `review run` (default: gate auto)
  --stop-on-reject   stop after a rejected review instead of retrying (exit 5)
  --allow-dirty      start even if the repository has uncommitted changes
  --resume           finish a pending record/commit from a crashed run
  --fallback-cmd CMD explicit fallback reviewer when `review run` has no external AI
  --fallback-channel C weaker channel for the fallback (default claude-subagent)
  --lock-timeout S   wait up to S seconds for the repo lock (default 0 = fail fast)
  --verify-cmd CMD   run CMD in the repo after the agent; failure stops the round
  --verify-timeout S kill --verify-cmd after S seconds (0 = off, default 0)
  --version-plan F   JSON object mapping version number to acceptance text
  --notify-cmd CMD   run CMD once when the driver exits (notification hook)
  --verify-range     after finalize, verify every task commit with review-gate

The notify command sees AUTO_ITERATE_EVENT (finalized/paused/failed/stopped/
done/blocked), AUTO_ITERATE_EXIT_CODE, AUTO_ITERATE_REASON, AUTO_ITERATE_TASK,
AUTO_ITERATE_REPO, AUTO_ITERATE_ITERATION, AUTO_ITERATE_COMMIT,
AUTO_ITERATE_TAG, AUTO_ITERATE_REVIEW, AUTO_ITERATE_VERDICT,
AUTO_ITERATE_CHANNEL and AUTO_ITERATE_MESSAGE. A notification failure is
reported but never changes the driver's exit status.

The agent command sees:
  AUTO_ITERATE_REPO, AUTO_ITERATE_ITERATION, AUTO_ITERATE_FEEDBACK,
  AUTO_ITERATE_GOAL, AUTO_ITERATE_ACCEPTANCE, AUTO_ITERATE_TASK,
  AUTO_ITERATE_EVIDENCE, AUTO_ITERATE_VERSION_ACCEPTANCE
where AUTO_ITERATE_FEEDBACK is a file containing the previous review text
(cleared after an approved version) and AUTO_ITERATE_EVIDENCE is the --evidence
path (empty when the driver was not given one). AUTO_ITERATE_VERSION_ACCEPTANCE
is the per-version text from --version-plan (empty when the plan has no entry).

The driver holds a kernel (flock) lock on the worktree's git dir while it runs.
A second driver fails fast with exit 2 unless --lock-timeout is set. The kernel
releases the lock when the driver exits, so a crashed run never leaves a stale
lease behind.

Exit: 0 done/finalized, 2 lock held or pending state, 3 no external AI,
      4 paused by the gate, 5 stopped on a rejection, 1 error.
TXT
}

TASK=; AGENT=; MESSAGE=; EVIDENCE=; EVIDENCE_SET=0; MAX=10; TAG_PREFIX=auto-iterate; LOG=; AGENT_TIMEOUT=0; REPORT=; report_init=0; EVENTS=; REVIEW_TIMEOUT=0; CHECK=0; PROVIDER=; STOP_ON_REJECT=0; ALLOW_DIRTY=0; RESUME=0; FALLBACK=; FALLBACK_CHANNEL=claude-subagent; state=
LOCK_TIMEOUT=0; lockfile=; lock_owner=; VERIFY_CMD=; VERIFY_TIMEOUT=0; VERIFY_CMD_SET=0; VERIFY_TIMEOUT_SET=0; VERSION_PLAN=; resume_review=0; resume_allow_dirty=0; keep_state=0; started_clean=0; durable_feedback=
NOTIFY=; VERIFY_RANGE=0; notify_event=; notify_reason=; manifest=; commit=; tag=; rid=; verdict=; channel=; msg=; full=
while [ $# -gt 0 ]; do
    case "$1" in
        --task) TASK=$2; shift 2 ;;
        --agent-cmd) AGENT=$2; shift 2 ;;
        --message) MESSAGE=$2; shift 2 ;;
        --evidence) EVIDENCE=$2; EVIDENCE_SET=1; shift 2 ;;
        --max) MAX=$2; shift 2 ;;
        --tag-prefix) TAG_PREFIX=$2; shift 2 ;;
        --log) LOG=$2; shift 2 ;;
        --agent-timeout) AGENT_TIMEOUT=$2; shift 2 ;;
        --report) REPORT=$2; shift 2 ;;
        --events) EVENTS=$2; shift 2 ;;
        --review-timeout) REVIEW_TIMEOUT=$2; shift 2 ;;
        --check) CHECK=1; shift ;;
        --provider) PROVIDER=$2; shift 2 ;;
        --stop-on-reject) STOP_ON_REJECT=1; shift ;;
        --allow-dirty) ALLOW_DIRTY=1; shift ;;
        --resume) RESUME=1; shift ;;
        --fallback-cmd) FALLBACK=$2; shift 2 ;;
        --fallback-channel) FALLBACK_CHANNEL=$2; shift 2 ;;
        --lock-timeout) LOCK_TIMEOUT=$2; shift 2 ;;
        --verify-cmd) VERIFY_CMD=$2; VERIFY_CMD_SET=1; shift 2 ;;
        --verify-timeout) VERIFY_TIMEOUT=$2; VERIFY_TIMEOUT_SET=1; shift 2 ;;
        --version-plan) VERSION_PLAN=$2; shift 2 ;;
        --notify-cmd) NOTIFY=$2; shift 2 ;;
        --verify-range) VERIFY_RANGE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done
[ -n "$TASK" ] || { usage >&2; exit 2; }
if [ "$CHECK" -eq 0 ] && [ -z "$AGENT" ]; then
    usage >&2
    exit 2
fi

for c in review-gate git python3 flock; do
    command -v "$c" >/dev/null 2>&1 || { echo "missing required command: $c" >&2; exit 1; }
done
case "$MAX" in ''|*[!0-9]*) echo "--max must be an integer" >&2; exit 2 ;; esac
[ "$MAX" -ge 1 ] || { echo "--max must be >= 1" >&2; exit 2; }
case "$AGENT_TIMEOUT" in ''|*[!0-9]*) echo "--agent-timeout must be an integer" >&2; exit 2 ;; esac
case "$REVIEW_TIMEOUT" in ''|*[!0-9]*) echo "--review-timeout must be an integer" >&2; exit 2 ;; esac
case "$LOCK_TIMEOUT" in ''|*[!0-9]*) echo "--lock-timeout must be an integer" >&2; exit 2 ;; esac
case "$VERIFY_TIMEOUT" in ''|*[!0-9]*) echo "--verify-timeout must be an integer" >&2; exit 2 ;; esac
case "$FALLBACK_CHANNEL" in claude-subagent|self-review) ;; *) echo "--fallback-channel must be claude-subagent or self-review" >&2; exit 2 ;; esac
if [ "$AGENT_TIMEOUT" -gt 0 ] || [ "$REVIEW_TIMEOUT" -gt 0 ] || [ "$VERIFY_TIMEOUT" -gt 0 ]; then
    command -v timeout >/dev/null 2>&1 || { echo "missing required command: timeout" >&2; exit 1; }
fi
if [ -n "$VERSION_PLAN" ]; then
    [ -f "$VERSION_PLAN" ] || { echo "version plan not found: $VERSION_PLAN" >&2; exit 1; }
    python3 - "$VERSION_PLAN" <<'PY' || { echo "version plan is not valid JSON: $VERSION_PLAN" >&2; exit 1; }
import json, sys
d = json.load(open(sys.argv[1]))
if not isinstance(d, (dict, list)):
    raise SystemExit("plan must be an object or array")
PY
fi
if [ -n "$EVIDENCE" ]; then
    case "$EVIDENCE" in
        /*) ;;
        *) EVIDENCE=$(pwd)/$EVIDENCE ;;
    esac
fi
if [ -n "$VERSION_PLAN" ]; then
    case "$VERSION_PLAN" in
        /*) ;;
        *) VERSION_PLAN=$(pwd)/$VERSION_PLAN ;;
    esac
fi

jget() { python3 -c 'import json,sys
d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[k]
print(d if not isinstance(d, str) else d)' "$@" 2>/dev/null || true; }

if [ "$CHECK" -eq 1 ]; then
    for f in "$LOG" "$REPORT" "$EVENTS" "$EVIDENCE"; do
        [ -n "$f" ] || continue
        if [ -e "$f" ]; then
            [ -w "$f" ] || { echo "check: $f is not writable" >&2; exit 1; }
        else
            d=$(dirname "$f")
            { [ -d "$d" ] && [ -w "$d" ]; } || { echo "check: cannot create $f" >&2; exit 1; }
        fi
    done
    show=$(review-gate task show --id "$TASK")
    status=$(printf '%s' "$show" | jget status)
    repo=$(printf '%s' "$show" | jget repo)
    iteration=$(printf '%s' "$show" | jget current_iteration)
    [ "$status" = running ] || { echo "check: task $TASK is ${status:-unknown}" >&2; exit 1; }
    [ -n "$repo" ] || { echo "check: task $TASK has no repo" >&2; exit 1; }
    [ -d "$repo" ] || { echo "check: repo $repo does not exist" >&2; exit 1; }
    git -C "$repo" rev-parse --is-inside-work-tree >/dev/null 2>&1 || {
        echo "check: $repo is not a git work tree" >&2
        exit 1
    }
    if [ "$ALLOW_DIRTY" -eq 0 ]; then
        dirty=$(git -C "$repo" status --porcelain)
        [ -z "$dirty" ] || { printf 'check: %s has uncommitted changes:\n%s\n' "$repo" "$dirty" >&2; exit 1; }
    fi
    echo "check ok: task=$TASK status=$status iteration=$iteration repo=$repo"
    exit 0
fi
if [ -n "$LOG" ]; then
    : >>"$LOG" 2>/dev/null || { echo "cannot write log: $LOG" >&2; exit 1; }
fi
if [ -n "$REPORT" ]; then
    : >"$REPORT" 2>/dev/null || { echo "cannot write report: $REPORT" >&2; exit 1; }
fi
if [ -n "$EVENTS" ]; then
    : >"$EVENTS" 2>/dev/null || { echo "cannot write events: $EVENTS" >&2; exit 1; }
fi
if [ -n "$EVIDENCE" ]; then
    : >>"$EVIDENCE" 2>/dev/null || { echo "cannot write evidence: $EVIDENCE" >&2; exit 1; }
fi

log() {
    case "$*" in
        stop\ *) _logline=$*; notify_reason=${_logline#stop } ;;
        finalized) notify_event=finalized ;;
        paused\ *) notify_event=paused ;;
    esac
    [ -n "$LOG" ] || return 0
    printf '%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >>"$LOG"
}

notify_exit() {
    _nrc=$1
    [ -n "$NOTIFY" ] || return 0
    _ne=$notify_event
    if [ -z "$_ne" ]; then
        case "$_nrc" in
            0) _ne=done ;;
            2) _ne=blocked ;;
            3) _ne=stopped ;;
            4) _ne=paused ;;
            5) _ne=stopped ;;
            *) _ne=failed ;;
        esac
    fi
    _nr=${notify_reason:-exit-$_nrc}
    _ndir=${repo:-$(pwd)}
    if ! (
        cd "$_ndir" 2>/dev/null || cd /
        AUTO_ITERATE_EVENT="$_ne" \
        AUTO_ITERATE_EXIT_CODE="$_nrc" \
        AUTO_ITERATE_REASON="$_nr" \
        AUTO_ITERATE_TASK="$TASK" \
        AUTO_ITERATE_REPO="${repo:-}" \
        AUTO_ITERATE_ITERATION="${iteration:-}" \
        AUTO_ITERATE_COMMIT="${full:-${commit:-}}" \
        AUTO_ITERATE_TAG="${tag:-}" \
        AUTO_ITERATE_REVIEW="${rid:-}" \
        AUTO_ITERATE_VERDICT="${verdict:-}" \
        AUTO_ITERATE_CHANNEL="${channel:-}" \
        AUTO_ITERATE_MESSAGE="${msg:-}" \
        sh -c "$NOTIFY"
    ) 9>&- 8>&-; then
        echo "notify command failed" >&2
    fi
    return 0
}

emit() {
    [ -n "$EVENTS" ] || return 0
    python3 -c 'import datetime,json,sys
d={"ts":datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),"event":sys.argv[1]}
for a in sys.argv[2:]:
    k,v=a.split("=",1)
    d[k]=v
print(json.dumps(d,separators=(",",":")))' "$@" >>"$EVENTS"
}

write_state() {
    _phase=$1; _it=$2; _rid=$3; _commit=$4; _tag=$5; _msg=$6; _chan=$7
    _head=${8:-}; _clean=${9:-0}; _fb=${10:-}; _tree=${11:-}; _untracked=${12-$preserve_untracked}
    _vcmd=${VERIFY_CMD:-}; _vtimeout=${VERIFY_TIMEOUT:-0}
    _vplan=${VERSION_PLAN:-}; _vacc=${version_acceptance:-}; _ev=${EVIDENCE:-}
    mkdir -p "$(dirname "$state")"
    python3 - "$state" "$_phase" "$_it" "$_rid" "$_commit" "$_tag" "$_msg" "$_chan" "$_head" "$_clean" "$_fb" "$_tree" "$_untracked" "$_vcmd" "$_vtimeout" "$_vplan" "$_vacc" "$_ev" <<'PY'
import json, os, sys
p, phase, iteration, review, commit, tag, message, channel, head, clean, feedback, tree, untracked, vcmd, vtimeout, vplan, vacc, evidence = sys.argv[1:19]
tmp = f"{p}.tmp.{os.getpid()}"
with open(tmp, "w") as f:
    json.dump({"phase": phase, "iteration": int(iteration), "review": review,
               "commit": commit, "tag": tag, "message": message,
               "channel": channel, "head": head, "clean": int(clean or 0),
               "feedback": feedback, "tree": tree, "untracked": untracked,
               "verify_cmd": vcmd, "verify_timeout": int(vtimeout or 0),
               "version_plan": vplan, "version_acceptance": vacc,
               "evidence": evidence}, f)
    f.write("\n")
    f.flush()
    os.fsync(f.fileno())
os.replace(tmp, p)
PY
}

write_retry_meta() {
    [ -n "$retry_meta" ] || return 0
    python3 - "$retry_meta" "$iteration" "$1" "$started_clean" "${2:-}" \
        "${VERIFY_CMD:-}" "${VERIFY_TIMEOUT:-0}" "${VERSION_PLAN:-}" \
        "${version_acceptance:-}" "${EVIDENCE:-}" <<'PY'
import json, os, sys
p, iteration, head, clean, untracked, vcmd, vtimeout, vplan, vacc, evidence = sys.argv[1:11]
tmp = f"{p}.tmp.{os.getpid()}"
with open(tmp, "w") as f:
    json.dump({"iteration": int(iteration), "head": head,
               "clean": int(clean or 0), "untracked": untracked,
               "verify_cmd": vcmd, "verify_timeout": int(vtimeout or 0),
               "version_plan": vplan, "version_acceptance": vacc,
               "evidence": evidence}, f)
    f.write("\n")
os.replace(tmp, p)
PY
}

version_acceptance_for() {
    [ -n "$VERSION_PLAN" ] || return 0
    python3 - "$VERSION_PLAN" "$1" <<'PY'
import json, sys
plan, iteration = sys.argv[1], int(sys.argv[2])
d = json.load(open(plan))
v = ""
if isinstance(d, dict):
    v = d.get(str(iteration), d.get(iteration, ""))
elif isinstance(d, list):
    for e in d:
        if isinstance(e, dict) and int(e.get("version", e.get("iteration", -1))) == iteration:
            v = e.get("acceptance", "")
            break
if isinstance(v, dict):
    v = v.get("acceptance", "")
print("" if v is None else v)
PY
}

apply_saved_round_settings() {
    _as_it=$1; _vc=$2; _vt=$3; _vp=$4; _va=$5; _ev=$6
    if [ -n "$_vc" ]; then
        if [ "$VERIFY_CMD_SET" -eq 0 ]; then
            VERIFY_CMD=$_vc
            if [ "$VERIFY_TIMEOUT_SET" -eq 1 ] && [ "$VERIFY_TIMEOUT" != "${_vt:-0}" ]; then
                echo "resume: verify timeout differs from the saved round" >&2
                return 1
            fi
            VERIFY_TIMEOUT=${_vt:-0}
        else
            [ "$VERIFY_CMD" = "$_vc" ] || {
                echo "resume: verify command differs from the saved round" >&2; return 1; }
            if [ "$VERIFY_TIMEOUT_SET" -eq 1 ]; then
                [ "$VERIFY_TIMEOUT" = "${_vt:-0}" ] || {
                    echo "resume: verify timeout differs from the saved round" >&2; return 1; }
            else
                VERIFY_TIMEOUT=${_vt:-0}
            fi
        fi
    elif [ "$VERIFY_CMD_SET" -eq 1 ] || [ "$VERIFY_TIMEOUT_SET" -eq 1 ]; then
        echo "resume: verify settings were not part of the saved round" >&2
        return 1
    fi
    if [ -n "$_ev" ]; then
        if [ -z "$EVIDENCE" ]; then
            EVIDENCE=$_ev
        elif [ "$EVIDENCE" != "$_ev" ]; then
            echo "resume: evidence path differs from the saved round" >&2
            return 1
        fi
    elif [ "$EVIDENCE_SET" -eq 1 ]; then
        echo "resume: evidence path was not part of the saved round" >&2
        return 1
    fi
    if [ -n "$_vp" ]; then
        if [ -z "$VERSION_PLAN" ]; then
            VERSION_PLAN=$_vp
        elif [ "$VERSION_PLAN" != "$_vp" ]; then
            echo "resume: version plan differs from the saved round" >&2
            return 1
        fi
        _saved_va=$(version_acceptance_for "$_as_it") || {
            echo "resume: cannot read saved version acceptance" >&2; return 1; }
        [ "$_saved_va" = "$_va" ] || {
            echo "resume: version acceptance differs from the saved round" >&2; return 1; }
        version_acceptance=$_va
    elif [ -n "$VERSION_PLAN" ]; then
        echo "resume: version plan was not part of the saved round" >&2
        return 1
    fi
}

write_feedback_file() {
    [ "${AUTO_ITERATE_FAIL_FEEDBACK:-0}" = 1 ] && return 1
    printf '%s\n' "$1" >"$2"
}

append_evidence() {
    [ "${AUTO_ITERATE_FAIL_EVIDENCE:-0}" = 1 ] && return 1
    cat >>"$EVIDENCE"
}

collect_rejection_untracked() {
    { git -C "$repo" diff --cached --name-only --diff-filter=A -z
      git -C "$repo" ls-files --others --exclude-standard -z
    } | base64 -w0
}

merge_untracked() {
    python3 - "$1" "$2" <<'PY'
import base64, sys
seen = []
for raw in sys.argv[1:3]:
    for p in base64.b64decode(raw or "").split(b"\0"):
        if p and p not in seen:
            seen.append(p)
sys.stdout.write(base64.b64encode(b"\0".join(seen)).decode())
PY
}

subtract_untracked() {
    python3 - "$1" "$2" <<'PY'
import base64, sys
pre = set(filter(None, base64.b64decode(sys.argv[1] or "").split(b"\0")))
cur = [p for p in base64.b64decode(sys.argv[2] or "").split(b"\0") if p and p not in pre]
sys.stdout.write(base64.b64encode(b"\0".join(cur)).decode())
PY
}

clean_untracked() {
    [ -n "$1" ] || return 0
    printf '%s' "$1" | base64 -d | ( cd "$repo" || exit 1; xargs -0 -r rm -rf -- )
}

write_manifest() {
    _mi=$1; _mrid=$2; _mcommit=$3; _mtag=$4; _mchan=$5; _mmsg=$6
    _mprov=$7; _mtree=$8; _macc=$9; _mvcmd=${10:-}
    [ -n "$manifest" ] || return 1
    mkdir -p "$(dirname "$manifest")" || return 1
    python3 - "$manifest" "$TASK" "$_mi" "$_mrid" "$_mcommit" "$_mtag" \
        "$_mchan" "$_mprov" "$_mmsg" "$_mtree" "$_macc" "$_mvcmd" <<'PY'
import datetime, json, os, sys
p, task, iteration, review, commit, tag, channel, provider, message, tree, acceptance, verify_cmd = sys.argv[1:13]
entry = {
    "at": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "task": task,
    "version": int(iteration),
    "iteration": int(iteration),
    "review": review,
    "verdict": "approved",
    "commit": commit,
    "tag": tag,
    "channel": channel,
    "provider": provider,
    "message": message,
    "tree": tree,
    "acceptance": acceptance,
    "verify_cmd": verify_cmd,
}
entries = []
if os.path.exists(p):
    with open(p) as f:
        for line in f:
            if not line.strip():
                continue
            try:
                entries.append(json.loads(line))
            except ValueError:
                raise SystemExit(f"invalid manifest line in {p}")
for e in entries:
    if e.get("task") == task and int(e.get("iteration", -1)) == int(iteration):
        if e.get("review") != review or e.get("commit") != commit or e.get("tag") != tag:
            raise SystemExit(f"manifest entry for {task} v{iteration} differs")
        raise SystemExit(0)
entries.append(entry)
tmp = f"{p}.tmp.{os.getpid()}"
with open(tmp, "w") as f:
    for e in entries:
        f.write(json.dumps(e, ensure_ascii=False, separators=(",", ":")) + "\n")
    f.flush()
    os.fsync(f.fileno())
os.replace(tmp, p)
try:
    dfd = os.open(os.path.dirname(p) or ".", os.O_RDONLY)
    try:
        os.fsync(dfd)
    finally:
        os.close(dfd)
except OSError:
    pass
PY
}

task_range() {
    _show=$(review-gate task show --id "$TASK") || return 1
    _first=$(printf '%s' "$_show" | python3 -c 'import json,sys
d=json.load(sys.stdin)
for h in d.get("history") or []:
    if h.get("commit"):
        print(h["commit"]); break' 2>/dev/null) || return 1
    if [ -n "$_first" ]; then
        if _base=$(git -C "$repo" rev-parse "$_first^" 2>/dev/null); then
            :
        else
            _base=$_first
        fi
    else
        _base=$(git -C "$repo" rev-list --max-parents=0 HEAD | head -n 1) || return 1
    fi
    [ -n "$_base" ] || return 1
    printf '%s..HEAD\n' "$_base"
}

verify_range() {
    [ "$VERIFY_RANGE" -eq 1 ] || return 0
    _range=$(task_range) || { echo "verify-range: cannot determine task range" >&2; return 1; }
    _pub=$(review-gate pubkey 2>/dev/null | tr -d '\r\n') || { echo "verify-range: cannot read gate pubkey" >&2; return 1; }
    [ -n "$_pub" ] || { echo "verify-range: empty gate pubkey" >&2; return 1; }
    if ! review-gate verify-range --repo "$repo" --pubkey "$_pub" "$_range"; then
        echo "verify-range failed for $_range" >&2
        return 1
    fi
    log "verify-range ok range=$_range"
    emit verify_range range="$_range"
    return 0
}

host_name() { hostname 2>/dev/null || uname -n 2>/dev/null || echo unknown; }

write_lock_meta() {
    _owner=$1; _started=$2
    mkdir -p "$(dirname "$lockfile")"
    python3 - "$lockfile.meta" "$_owner" "$(host_name)" "$$" "$TASK" "$iteration" "$_started" <<'PY'
import datetime, json, os, sys
p, owner, host, pid, task, iteration, started = sys.argv[1:]
tmp = f"{p}.tmp.{os.getpid()}"
now = int(datetime.datetime.now().timestamp())
with open(tmp, "w") as f:
    json.dump({"owner": owner, "host": host, "pid": int(pid), "task": task,
               "iteration": iteration, "started": int(started),
               "updated": now}, f)
    f.write("\n")
    f.flush()
    os.fsync(f.fileno())
os.replace(tmp, p)
PY
}

acquire_lock() {
    mkdir -p "$(dirname "$lockfile")"
    exec 9>"$lockfile" || { echo "cannot open lock file: $lockfile" >&2; return 1; }
    if [ "$LOCK_TIMEOUT" -eq 0 ]; then
        if ! flock -n 9; then
            _lowner=; _ltask=
            [ ! -f "$lockfile.meta" ] || _lowner=$(jget owner <"$lockfile.meta" 2>/dev/null)
            [ ! -f "$lockfile.meta" ] || _ltask=$(jget task <"$lockfile.meta" 2>/dev/null)
            { exec 9>&-; } 2>/dev/null || true
            echo "repo is locked by ${_lowner:-unknown} (task ${_ltask:-unknown}); use --lock-timeout or wait" >&2
            return 1
        fi
    elif ! flock -w "$LOCK_TIMEOUT" 9; then
        _lowner=
        [ ! -f "$lockfile.meta" ] || _lowner=$(jget owner <"$lockfile.meta" 2>/dev/null)
        { exec 9>&-; } 2>/dev/null || true
        echo "repo lock still held after ${LOCK_TIMEOUT}s by ${_lowner:-unknown}" >&2
        return 1
    fi
    lock_owner="${AUTO_ITERATE_OWNER:-$(host_name):$$}"
    if ! write_lock_meta "$lock_owner" "$(date +%s)"; then
        flock -u 9 2>/dev/null || true
        { exec 9>&-; } 2>/dev/null || true
        echo "cannot write lock metadata" >&2
        return 1
    fi
    log "lock acquired owner=$lock_owner"
    emit lock action="acquired" owner="$lock_owner"
}

renew_lock() {
    [ -n "$lockfile" ] || return 0
    [ -f "$lockfile.meta" ] || { echo "repo lock metadata disappeared: $lockfile.meta" >&2; return 1; }
    _lowner=$(jget owner <"$lockfile.meta" 2>/dev/null)
    [ "$_lowner" = "$lock_owner" ] || { echo "repo lock was taken over by ${_lowner:-unknown}" >&2; return 1; }
    _started=$(jget started <"$lockfile.meta" 2>/dev/null)
    write_lock_meta "$lock_owner" "${_started:-$(date +%s)}" || { echo "cannot renew lock metadata" >&2; return 1; }
}

release_lock() {
    [ -n "$lockfile" ] || return 0
    if [ -n "$lock_owner" ] && [ -f "$lockfile.meta" ] && [ "$(jget owner <"$lockfile.meta" 2>/dev/null)" = "$lock_owner" ]; then
        rm -f "$lockfile.meta"
    fi
    flock -u 9 2>/dev/null || true
    { exec 9>&-; } 2>/dev/null || true
    log "lock released owner=$lock_owner"
    emit lock action="released" owner="$lock_owner"
    lockfile=; lock_owner=
}

review_outcome() {
    _rid=$1
    [ -n "$_rid" ] || { printf ''; return 0; }
    _show=$(review-gate task show --id "$TASK" 2>/dev/null) || return 1
    printf '%s' "$_show" | python3 -c 'import json,sys
rid = sys.argv[1]
try:
    d = json.load(sys.stdin)
except Exception:
    raise SystemExit(1)
for h in d.get("history", []) or []:
    if isinstance(h, dict) and h.get("review") == rid:
        print(h.get("outcome", "") or "")
        raise SystemExit(0)
print("")' "$_rid"
}

worker_lease_acquire() {
    [ -n "$worker_lock" ] || return 1
    exec 8>"$worker_lock" || { echo "cannot open worker lease: $worker_lock" >&2; return 1; }
    if ! flock -n 8; then
        { exec 8>&-; } 2>/dev/null || true
        return 1
    fi
}

worker_lease_release() {
    { exec 8>&-; } 2>/dev/null || true
    [ -n "$worker_lock" ] || return 0
    exec 8>"$worker_lock" || return 1
    if ! flock -n 8; then
        { exec 8>&-; } 2>/dev/null || true
        return 1
    fi
    { exec 8>&-; } 2>/dev/null || true
}

check_worker() {
    [ -n "$worker_lock" ] || return 0
    if [ -f "$worker_lock" ]; then
        exec 8>"$worker_lock" || { echo "cannot open worker lease: $worker_lock" >&2; return 1; }
        if ! flock -n 8; then
            _wpid=$(cat "$worker_pidfile" 2>/dev/null || true)
            { exec 8>&-; } 2>/dev/null || true
            echo "a previous agent/verifier is still running${_wpid:+ (pid $_wpid)}; wait for it to exit before resuming" >&2
            return 1
        fi
        { exec 8>&-; } 2>/dev/null || true
    fi
    rm -f "$worker_pidfile"
}

check_pending_states() {
    _dir=$(dirname "$state")
    [ -d "$_dir" ] || return 0
    for _f in "$_dir"/*.json; do
        [ -e "$_f" ] || continue
        if [ "$_f" = "$state" ]; then
            continue
        fi
        printf 'pending state for another task: %s; resolve it with the owning task and --resume first\n' "$_f" >&2
        return 1
    done
}

resume_pending() {
    raw=$(cat "$state")
    rphase=$(printf '%s' "$raw" | jget phase)
    rit=$(printf '%s' "$raw" | jget iteration)
    rrid=$(printf '%s' "$raw" | jget review)
    rcommit=$(printf '%s' "$raw" | jget commit)
    rtag=$(printf '%s' "$raw" | jget tag)
    rmsg=$(printf '%s' "$raw" | jget message)
    rchan=$(printf '%s' "$raw" | jget channel)
    rhead=$(printf '%s' "$raw" | jget head)
    rclean=$(printf '%s' "$raw" | jget clean)
    rfb=$(printf '%s' "$raw" | jget feedback)
    rtree=$(printf '%s' "$raw" | jget tree)
    rvc=$(printf '%s' "$raw" | jget verify_cmd)
    rvt=$(printf '%s' "$raw" | jget verify_timeout)
    rvp=$(printf '%s' "$raw" | jget version_plan)
    rva=$(printf '%s' "$raw" | jget version_acceptance)
    rev=$(printf '%s' "$raw" | jget evidence)
    runtracked=$(printf '%s' "$raw" | jget untracked)
    case "$rphase" in
        agent|verify|staged|reviewing)
            _now_untracked=$(collect_rejection_untracked)
            preserve_untracked=$(merge_untracked "$runtracked" "$_now_untracked")
            ;;
    esac
    apply_saved_round_settings "$rit" "$rvc" "$rvt" "$rvp" "$rva" "$rev" || return 1
    case "$rclean" in 1) started_clean=1 ;; *) started_clean=0 ;; esac
    recorded=
    case "$rphase" in
        committed|rejected) recorded=$(review_outcome "$rrid") || {
            echo "resume: cannot read task history" >&2; return 1; } ;;
    esac
    log "resume phase=$rphase iteration=$rit review=$rrid recorded=${recorded:-none}"
    emit resume phase="$rphase" iteration="$rit" review="$rrid" recorded="${recorded:-none}"
    if [ "$rphase" = committed ] && { [ "$recorded" = approved ] || [ "$recorded" = insufficient-reviewer ]; }; then
        [ -n "$rit" ] && [ "$iteration" -ge "$rit" ] || {
            echo "resume: task iteration $iteration is behind committed state $rit" >&2; return 1; }
    elif [ -n "$rit" ] && [ "$rit" != "$iteration" ]; then
        echo "resume: state iteration $rit does not match task iteration $iteration" >&2
        return 1
    fi
    cur_head=$(git -C "$repo" rev-parse HEAD)
    raction=
    case "$rphase" in
        committed)
            [ -n "$rrid" ] && [ -n "$rcommit" ] && [ -n "$rtag" ] || { echo "resume: committed state is incomplete" >&2; return 1; }
            git -C "$repo" rev-parse "$rcommit^{commit}" >/dev/null 2>&1 || { echo "resume: commit $rcommit is missing" >&2; return 1; }
            if [ -n "$rtree" ]; then
                [ "$(git -C "$repo" rev-parse "$rcommit^{tree}")" = "$rtree" ] || {
                    echo "resume: commit tree does not match reviewed tree $rtree" >&2; return 1; }
            fi
            if [ -n "$rhead" ] && [ "$rhead" != "$rcommit" ] && [ "$(git -C "$repo" rev-parse "$rcommit^")" != "$rhead" ]; then
                echo "resume: commit $rcommit is not based on saved HEAD $rhead" >&2
                return 1
            fi
            tag_commit=$(git -C "$repo" rev-parse "refs/tags/$rtag^{commit}" 2>/dev/null) || {
                echo "resume: tag $rtag is missing" >&2; return 1; }
            [ "$tag_commit" = "$rcommit" ] || { echo "resume: tag $rtag does not point to $rcommit" >&2; return 1; }
            if [ "$recorded" = approved ] || [ "$recorded" = insufficient-reviewer ]; then
                raction=already-recorded
                log "resume committed review=$rrid already recorded as $recorded"
            else
                [ -z "$recorded" ] || { echo "resume: review $rrid was recorded as $recorded" >&2; return 1; }
                out=$(review-gate record --id "$TASK" --review "$rrid" --commit "$rcommit" --tag "$rtag") || {
                    echo "resume: record failed; state kept" >&2; return 1; }
                raction=$(printf '%s' "$out" | jget action)
            fi
            ;;
        reviewed)
            [ -n "$rrid" ] || { echo "resume: reviewed state has no review" >&2; return 1; }
            if [ -n "$rtree" ]; then
                if [ "$cur_head" = "$rhead" ]; then
                    [ "$(git -C "$repo" write-tree)" = "$rtree" ] || {
                        echo "resume: reviewed tree changed; rerun review" >&2; return 1; }
                else
                    [ "$(git -C "$repo" rev-parse "$cur_head^{tree}")" = "$rtree" ] || {
                        echo "resume: committed tree does not match reviewed tree" >&2; return 1; }
                fi
            fi
            if [ -n "$rhead" ]; then
                if [ "$cur_head" = "$rhead" ]; then
                    git -C "$repo" diff --cached --quiet && { echo "resume: reviewed state has no staged changes" >&2; return 1; }
                    [ -n "$rmsg" ] || rmsg="auto-iterate: version $rit"
                    git -C "$repo" commit -q -m "$rmsg" || { echo "resume: commit failed" >&2; return 1; }
                    rcommit=$(git -C "$repo" rev-parse HEAD)
                else
                    [ "$(git -C "$repo" rev-parse "$cur_head^")" = "$rhead" ] || {
                        echo "resume: HEAD moved from $rhead to $cur_head" >&2; return 1; }
                    git -C "$repo" diff --cached --quiet || { echo "resume: staged changes remain after the commit" >&2; return 1; }
                    rcommit=$cur_head
                fi
            else
                if git -C "$repo" diff --cached --quiet; then
                    rcommit=$(git -C "$repo" rev-parse HEAD)
                else
                    [ -n "$rmsg" ] || rmsg="auto-iterate: version $rit"
                    git -C "$repo" commit -q -m "$rmsg" || { echo "resume: commit failed" >&2; return 1; }
                    rcommit=$(git -C "$repo" rev-parse HEAD)
                fi
            fi
            [ -n "$rtag" ] || rtag="$TAG_PREFIX/$TASK/v$rit"
            if tag_commit=$(git -C "$repo" rev-parse "refs/tags/$rtag^{commit}" 2>/dev/null); then
                [ "$tag_commit" = "$rcommit" ] || { echo "resume: tag $rtag points to $tag_commit, not $rcommit" >&2; return 1; }
            else
                git -C "$repo" tag "$rtag" || { echo "resume: tag failed" >&2; return 1; }
            fi
            write_state committed "$rit" "$rrid" "$rcommit" "$rtag" "$rmsg" "$rchan" "$rhead" "$started_clean" "" "$rtree" || {
                echo "resume: cannot persist committed state" >&2; return 1; }
            out=$(review-gate record --id "$TASK" --review "$rrid" --commit "$rcommit" --tag "$rtag") || {
                echo "resume: record failed; state kept" >&2; return 1; }
            ;;
        staged|reviewing|verify)
            if [ -n "$rhead" ] && [ "$cur_head" != "$rhead" ]; then
                echo "resume: saved HEAD $rhead does not match current $cur_head" >&2
                return 1
            fi
            if [ -n "$rfb" ]; then
                write_feedback_file "$rfb" "$feedback" || {
                    echo "resume: cannot restore feedback" >&2; return 1; }
                write_feedback_file "$rfb" "$durable_feedback" || {
                    echo "resume: cannot restore durable feedback" >&2; return 1; }
            fi
            resume_review=1
            log "resume $rphase -> review iteration=$rit"
            emit resume_review phase="$rphase" iteration="$rit"
            return 0
            ;;
        agent)
            if [ -n "$rhead" ] && [ "$cur_head" != "$rhead" ]; then
                echo "resume: saved HEAD $rhead does not match current $cur_head" >&2
                return 1
            fi
            dirty=$(git -C "$repo" status --porcelain)
            if [ -n "$dirty" ]; then
                log "resume agent dirty; preserving partial edits"
                resume_allow_dirty=1
            fi
            _now_untracked=$(collect_rejection_untracked)
            preserve_untracked=$(merge_untracked "$runtracked" "$_now_untracked")
            if [ -n "$rfb" ]; then
                write_feedback_file "$rfb" "$feedback" || {
                    echo "resume: cannot restore feedback" >&2; return 1; }
            fi
            keep_state=1
            log "resume agent -> restart iteration=$rit"
            emit resume_agent iteration="$rit"
            return 0
            ;;
        rejected)
            [ -n "$rrid" ] || { echo "resume: rejected state has no review" >&2; return 1; }
            if [ -n "$rhead" ] && [ "$cur_head" != "$rhead" ]; then
                echo "resume: HEAD moved from $rhead to $cur_head after rejection" >&2
                return 1
            fi
            if [ -n "$rfb" ]; then
                write_feedback_file "$rfb" "$feedback" || {
                    echo "resume: cannot restore feedback" >&2; return 1; }
                write_feedback_file "$rfb" "$durable_feedback" || {
                    echo "resume: cannot restore durable feedback" >&2; return 1; }
            fi
            write_retry_meta "$rhead" "$runtracked" || {
                echo "resume: cannot persist retry metadata" >&2; return 1; }
            case "$recorded" in
                rejected|insufficient-reviewer)
                    raction=already-recorded
                    log "resume rejected review=$rrid already recorded as $recorded" ;;
                "")
                    out=$(review-gate record --id "$TASK" --review "$rrid") || {
                        echo "resume: record failed; state kept" >&2; return 1; }
                    raction=$(printf '%s' "$out" | jget action) ;;
                *)
                    echo "resume: review $rrid was recorded as $recorded" >&2; return 1 ;;
            esac
            resume_allow_dirty=1
            git -C "$repo" reset -q || { echo "resume: reset failed; state kept" >&2; return 1; }
            if [ "$started_clean" -eq 1 ]; then
                clean_untracked "$runtracked" || { echo "resume: clean failed; state kept" >&2; return 1; }
                [ -z "$runtracked" ] || log "resume cleaned recorded untracked files iteration=$rit"
            fi
            ;;
        *) echo "resume: unknown phase ${rphase:-none}" >&2; return 1 ;;
    esac
    [ -n "$raction" ] || raction=$(printf '%s' "$out" | jget action)
    if [ "$rphase" = committed ] || [ "$rphase" = reviewed ]; then
        write_manifest "$rit" "$rrid" "$rcommit" "$rtag" "$rchan" "$rmsg" "" "$rtree" "$rva" "$rvc" \
            || { echo "resume: cannot append approved version to $manifest" >&2; return 1; }
        if [ "$raction" = finalize ] && [ "$VERIFY_RANGE" -eq 1 ]; then
            verify_range || { echo "resume: verify-range failed" >&2; return 1; }
        fi
        rm -f "$durable_feedback" || { echo "resume: cannot remove durable feedback" >&2; return 1; }
        rm -f "$retry_meta" || { echo "resume: cannot remove retry metadata" >&2; return 1; }
        : > "$feedback" || { echo "resume: cannot clear feedback" >&2; return 1; }
    fi
    if [ "$keep_state" -eq 0 ]; then
        rm -f "$state" || { echo "resume: cannot remove state" >&2; return 1; }
    fi
    log "resume recorded phase=$rphase review=$rrid commit=${rcommit:--} tag=${rtag:--} action=$raction"
    emit resume_done phase="$rphase" review="$rrid" commit="${rcommit:--}" tag="${rtag:--}" action="$raction"
    if [ "$report_init" -eq 1 ]; then
        if [ "$rphase" = rejected ]; then
            printf '| %s | %s | rejected | %s | - | - | %s | - |\n' \
                "$rit" "$rrid" "$raction" "${rchan:--}" >>"$REPORT"
        else
            printf '| %s | %s | approved | %s | %s | %s | %s | - |\n' \
                "$rit" "$rrid" "$raction" "${rcommit:--}" "${rtag:--}" "${rchan:--}" >>"$REPORT"
        fi
    fi
    return 0
}

verify_worktree() {
    [ -n "$VERIFY_CMD" ] || return 0
    _vlog=$(mktemp "${TMPDIR:-/tmp}/auto-iterate-verify.XXXXXX")
    worker_lease_acquire || { echo "another worker is active; refusing to start verification" >&2; return 1; }
    set +e
    (
        cd "$repo" || exit 1
        if [ "$VERIFY_TIMEOUT" -gt 0 ]; then
            exec sh -c 'printf "%s\n" "$$" >"$1" || exit 1; shift; exec "$@"' sh "$worker_pidfile" timeout "$VERIFY_TIMEOUT" sh -c "$VERIFY_CMD"
        else
            exec sh -c 'printf "%s\n" "$$" >"$1" || exit 1; shift; exec "$@"' sh "$worker_pidfile" sh -c "$VERIFY_CMD"
        fi
    ) 9>&- >"$_vlog" 2>&1 &
    _vpid=$!
    wait "$_vpid"
    _vrc=$?
    set -e
    if ! worker_lease_release; then
        echo "verifier left a background process holding the worker lease" >&2
        return 1
    fi
    rm -f "$worker_pidfile"
    cat "$_vlog"
    if [ -n "$EVIDENCE" ]; then
        { printf '\n== verify (exit %s): %s ==\n' "$_vrc" "$VERIFY_CMD"; cat "$_vlog"; } | append_evidence \
            || { echo "cannot append verify output to evidence $EVIDENCE" >&2; rm -f "$_vlog"; return 1; }
    fi
    rm -f "$_vlog"
    if [ "$_vrc" -ne 0 ]; then
        log "stop verify-failed rc=$_vrc"
        emit stop reason="verify-failed" rc="$_vrc"
        echo "verify command failed (exit $_vrc); changes kept, task still running" >&2
        return 1
    fi
    log "verify ok"
    emit verify ok
    return 0
}

report_header() {
    [ -n "$REPORT" ] && [ "$report_init" -eq 0 ] || return 0
    {
        printf '# auto-iterate report\n\n'
        printf '%s\n' "- task: \`$TASK\`"
        printf '%s\n' "- repo: \`$repo\`"
        printf '%s\n' "- started: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
        printf '\n| version | review | verdict | action | commit | tag | channel | duration |\n'
        printf '|---|---|---|---|---|---|---|---|\n'
    } >"$REPORT"
    report_init=1
}


feedback=$(mktemp "${TMPDIR:-/tmp}/auto-iterate-feedback.XXXXXX")
finish() {
    rc=$?
    release_lock
    rm -f "$feedback"
    notify_exit "$rc" || true
    if [ -n "$REPORT" ]; then
        {
            printf '\n'
            printf '%s\n' "- finished: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
            printf '%s\n' "- exit: $rc"
        } >>"$REPORT" 2>/dev/null || true
    fi
}
trap finish EXIT
: > "$feedback"
round=0
preserve_untracked=
log "start task=$TASK max=$MAX"
emit start task="$TASK" max="$MAX"

while :; do
    show=$(review-gate task show --id "$TASK")
    status=$(printf '%s' "$show" | jget status)
    iteration=$(printf '%s' "$show" | jget current_iteration)
    repo=$(printf '%s' "$show" | jget repo)
    [ -n "$repo" ] || { echo "task $TASK has no repo" >&2; exit 1; }
    if [ -z "$state" ]; then
        gitdir=$(git -C "$repo" rev-parse --absolute-git-dir)
        state="$gitdir/auto-iterate/$(printf '%s' "$TASK" | tr '/ ' '__').json"
        durable_feedback="$gitdir/auto-iterate/$(printf '%s' "$TASK" | tr '/ ' '__').feedback"
        retry_meta="$gitdir/auto-iterate/$(printf '%s' "$TASK" | tr '/ ' '__').retry"
        worker_pidfile="$gitdir/auto-iterate/worker.pid"
        worker_lock="$gitdir/auto-iterate/worker.lock"
        manifest="$gitdir/auto-iterate/versions.ndjson"
    fi
    if [ "$status" != running ]; then
        if [ "$RESUME" -eq 1 ] && [ "$round" -eq 0 ] && [ -f "$state" ]; then
            :
        else
            [ "$status" = paused ] && notify_event=paused
            log "stop status=${status:-unknown}"
            emit stop reason="status" status="${status:-unknown}"
            echo "task $TASK is ${status:-unknown}; stopping"
            exit 0
        fi
    fi
    report_header
    if [ -z "$lockfile" ]; then
        lockfile="$gitdir/auto-iterate/lock"
        acquire_lock || exit 2
        show=$(review-gate task show --id "$TASK")
        status=$(printf '%s' "$show" | jget status)
        if [ "$status" != running ]; then
            [ "$RESUME" -eq 1 ] && [ "$round" -eq 0 ] && [ -f "$state" ] || {
                [ "$status" = paused ] && notify_event=paused
                log "stop status=${status:-unknown}"
                emit stop reason="status" status="${status:-unknown}"
                echo "task $TASK is ${status:-unknown}; stopping"
                exit 0
            }
        fi
        iteration=$(printf '%s' "$show" | jget current_iteration)
        repo_after=$(printf '%s' "$show" | jget repo)
        [ "$repo_after" = "$repo" ] || {
            echo "task repo changed while waiting for the lock: $repo -> ${repo_after:-empty}" >&2; exit 1; }
    fi
    if [ -s "$durable_feedback" ]; then
        cat "$durable_feedback" >"$feedback"
    else
        : > "$feedback"
    fi
    check_worker || exit 2
    check_pending_states || exit 2
    if [ "$round" -eq 0 ] && [ -f "$state" ]; then
        if [ "$RESUME" -eq 0 ]; then
            log "stop pending-state"
            emit stop reason="pending-state"
            echo "pending state $state; rerun with --resume" >&2
            exit 2
        fi
        resume_pending || exit 1
        show=$(review-gate task show --id "$TASK")
        status=$(printf '%s' "$show" | jget status)
        [ "$status" = running ] || { log "stop status=${status:-unknown}"; echo "task $TASK is ${status:-unknown}; stopping"; exit 0; }
        iteration=$(printf '%s' "$show" | jget current_iteration)
        repo=$(printf '%s' "$show" | jget repo)
    fi
    if [ "$round" -eq 0 ] && [ "$RESUME" -eq 1 ] && [ ! -f "$state" ] && [ -f "$retry_meta" ]; then
        _rtm_raw=$(cat "$retry_meta")
        rtm_it=$(printf '%s' "$_rtm_raw" | jget iteration)
        rtm_head=$(printf '%s' "$_rtm_raw" | jget head)
        rtm_clean=$(printf '%s' "$_rtm_raw" | jget clean)
        rtm_vc=$(printf '%s' "$_rtm_raw" | jget verify_cmd)
        rtm_vt=$(printf '%s' "$_rtm_raw" | jget verify_timeout)
        rtm_vp=$(printf '%s' "$_rtm_raw" | jget version_plan)
        rtm_va=$(printf '%s' "$_rtm_raw" | jget version_acceptance)
        rtm_ev=$(printf '%s' "$_rtm_raw" | jget evidence)
        if [ "$rtm_it" = "$iteration" ] && [ "$rtm_head" = "$(git -C "$repo" rev-parse HEAD)" ]; then
            apply_saved_round_settings "$iteration" "$rtm_vc" "$rtm_vt" "$rtm_vp" "$rtm_va" "$rtm_ev" || exit 1
            _now_untracked=$(collect_rejection_untracked)
            preserve_untracked=$(merge_untracked "$preserve_untracked" "$_now_untracked")
            started_clean=0
            [ "$rtm_clean" = 1 ] && started_clean=1
            resume_allow_dirty=1
            log "resume rejected-retry iteration=$iteration allow-dirty"
        else
            log "ignore stale retry metadata"
        fi
    fi
    if [ "$round" -eq 0 ]; then
        dirty=$(git -C "$repo" status --porcelain)
        if [ -z "$dirty" ]; then
            started_clean=1
        elif [ "$ALLOW_DIRTY" -eq 0 ] && [ "$resume_review" -eq 0 ] && [ "$resume_allow_dirty" -eq 0 ]; then
            log "stop dirty-worktree"
            emit stop reason="dirty-worktree"
            printf 'worktree %s is dirty; commit/stash first or pass --allow-dirty:\n%s\n' "$repo" "$dirty" >&2
            exit 1
        fi
    fi

    if { [ -n "$VERSION_PLAN" ] || [ -n "$VERIFY_CMD" ]; } && [ -z "$EVIDENCE" ]; then
        EVIDENCE=$(mktemp "${TMPDIR:-/tmp}/auto-iterate-evidence.XXXXXX")
    fi
    goal=$(printf '%s' "$show" | jget goal)
    acceptance=$(printf '%s' "$show" | jget final_acceptance)
    version_acceptance=$(version_acceptance_for "$iteration") || { echo "cannot read version acceptance for $iteration" >&2; exit 1; }
    commit=-
    tag=-
    channel=-
    round=$((round + 1))
    if [ "$round" -gt "$MAX" ]; then
        notify_event=stopped
        log "stop max-rounds"
        emit stop reason="max-rounds"
        echo "reached --max $MAX; stopping with the task still running" >&2
        exit 0
    fi
    echo "== round $round (version $iteration) =="
    log "round=$round iteration=$iteration repo=$repo"
    emit round round="$round" iteration="$iteration"
    round_start=$(date +%s)
    renew_lock || exit 2

    export AUTO_ITERATE_REPO="$repo" AUTO_ITERATE_ITERATION="$iteration" \
        AUTO_ITERATE_FEEDBACK="$feedback" AUTO_ITERATE_GOAL="$goal" \
        AUTO_ITERATE_ACCEPTANCE="$acceptance" AUTO_ITERATE_TASK="$TASK" \
        AUTO_ITERATE_EVIDENCE="${EVIDENCE:-}" \
        AUTO_ITERATE_VERSION_ACCEPTANCE="$version_acceptance"
    msg=${MESSAGE:-"auto-iterate: version $iteration"}
    head_base=$(git -C "$repo" rev-parse HEAD)
    if [ -n "$version_acceptance" ] && [ -n "$EVIDENCE" ]; then
        { printf '\n## Version %s acceptance\n%s\n' "$iteration" "$version_acceptance"; } | append_evidence \
            || { echo "cannot append version acceptance to evidence $EVIDENCE" >&2; exit 1; }
    fi

    if [ "$resume_review" -eq 0 ]; then
        _fbtext=$(cat "$feedback" 2>/dev/null || true)
        write_state agent "$iteration" "" "" "" "$msg" "" "$head_base" "$started_clean" "$_fbtext"
        worker_lease_acquire || { echo "another worker is active; refusing to start the agent" >&2; exit 2; }
        set +e
        (
            cd "$repo" || exit 1
            if [ "$AGENT_TIMEOUT" -gt 0 ]; then
                exec sh -c 'printf "%s\n" "$$" >"$1" || exit 1; shift; exec "$@"' sh "$worker_pidfile" timeout "$AGENT_TIMEOUT" sh -c "$AGENT"
            else
                exec sh -c 'printf "%s\n" "$$" >"$1" || exit 1; shift; exec "$@"' sh "$worker_pidfile" sh -c "$AGENT"
            fi
        ) 9>&- &
        agent_pid=$!
        wait "$agent_pid"
        arc=$?
        set -e
        if [ "$arc" -eq 0 ]; then
            _fbtext=$(cat "$feedback" 2>/dev/null || true)
            write_state verify "$iteration" "" "" "" "$msg" "" "$head_base" "$started_clean" "$_fbtext"
        fi
        if ! worker_lease_release; then
            echo "agent left a background process holding the worker lease" >&2
            log "stop agent-descendant"
            emit stop reason="agent-descendant"
            exit 2
        fi
        rm -f "$worker_pidfile"
        if [ "$arc" -ne 0 ]; then
            if [ "$arc" -eq 124 ]; then
                log "stop agent-timeout seconds=$AGENT_TIMEOUT"
                emit stop reason="agent-timeout"
                echo "agent timed out after ${AGENT_TIMEOUT}s" >&2
            else
                log "stop agent-failed rc=$arc"
                emit stop reason="agent-failed"
                echo "agent failed (exit $arc)" >&2
            fi
            exit 1
        fi

        if ! verify_worktree; then
            exit 1
        fi

        _fbtext=$(cat "$feedback" 2>/dev/null || true)
        write_state staged "$iteration" "" "" "" "$msg" "" "$head_base" "$started_clean" "$_fbtext"
        if ! review-gate stage --id "$TASK" >/dev/null; then
            log "stop stage-failed"
            emit stop reason="stage-failed"
            echo "review-gate stage failed; changes kept" >&2
            exit 1
        fi
    else
        log "resume review iteration=$iteration"
        if ! verify_worktree; then
            exit 1
        fi
        _fbtext=$(cat "$feedback" 2>/dev/null || true)
        write_state staged "$iteration" "" "" "" "$msg" "" "$head_base" "$started_clean" "$_fbtext"
        if ! review-gate stage --id "$TASK" >/dev/null; then
            log "stop stage-failed"
            emit stop reason="stage-failed"
            echo "review-gate stage failed; changes kept" >&2
            exit 1
        fi
        resume_review=0
    fi
    _fbtext=$(cat "$feedback" 2>/dev/null || true)
    write_state reviewing "$iteration" "" "" "" "$msg" "" "$head_base" "$started_clean" "$_fbtext"

    worker_lease_acquire || { echo "another worker is active; refusing to start the review" >&2; exit 2; }
    set +e
    set -- review-gate review run --id "$TASK"
    if [ -n "$EVIDENCE" ]; then set -- "$@" --evidence "$EVIDENCE"; fi
    if [ -n "$PROVIDER" ]; then set -- "$@" --provider "$PROVIDER"; fi
    if [ "$REVIEW_TIMEOUT" -gt 0 ]; then
        job=$(timeout "$REVIEW_TIMEOUT" "$@" 9>&-)
    else
        job=$("$@" 9>&-)
    fi
    rc=$?
    set -e
    if ! worker_lease_release; then
        echo "review left a background process holding the worker lease" >&2
        log "stop review-descendant"
        emit stop reason="review-descendant"
        exit 2
    fi
    if [ "$rc" -eq 124 ] && [ "$REVIEW_TIMEOUT" -gt 0 ]; then
        log "stop review-timeout seconds=$REVIEW_TIMEOUT"
        emit stop reason="review-timeout"
        echo "review timed out after ${REVIEW_TIMEOUT}s; the task may have a pending review" >&2
        exit 3
    fi
    if [ "$rc" -eq 3 ] && [ -n "$FALLBACK" ]; then
        log "fallback review-unavailable channel=$FALLBACK_CHANNEL"
        emit fallback channel="$FALLBACK_CHANNEL"
        fb_diff=$(mktemp "${TMPDIR:-/tmp}/auto-iterate-diff.XXXXXX")
        fb_file=$(mktemp "${TMPDIR:-/tmp}/auto-iterate-review.XXXXXX")
        git -C "$repo" diff --cached >"$fb_diff"
        worker_lease_acquire || { echo "another worker is active; refusing to start the fallback" >&2; exit 2; }
        set +e
        (
            cd "$repo"
            AUTO_ITERATE_REPO="$repo" AUTO_ITERATE_TASK="$TASK" \
            AUTO_ITERATE_ITERATION="$iteration" AUTO_ITERATE_GOAL="$goal" \
            AUTO_ITERATE_ACCEPTANCE="$acceptance" AUTO_ITERATE_EVIDENCE="$EVIDENCE" \
            AUTO_ITERATE_VERSION_ACCEPTANCE="$version_acceptance" \
            AUTO_ITERATE_DIFF="$fb_diff" AUTO_ITERATE_REVIEW_FILE="$fb_file" \
            sh -c "$FALLBACK"
        ) 9>&- &
        fbpid=$!
        printf '%s\n' "$fbpid" > "$worker_pidfile"
        wait "$fbpid"
        frc=$?
        set -e
        if ! worker_lease_release; then
            echo "fallback reviewer left a background process holding the worker lease" >&2
            log "stop fallback-descendant"
            emit stop reason="fallback-descendant"
            exit 2
        fi
        rm -f "$worker_pidfile"
        if [ "$frc" -ne 0 ]; then
            log "stop fallback-failed rc=$frc"
            emit stop reason="fallback-failed"
            echo "fallback reviewer failed (exit $frc); staged tree kept" >&2
            exit 1
        fi
        if [ ! -s "$fb_file" ]; then
            log "stop fallback-empty"
            emit stop reason="fallback-empty"
            echo "fallback reviewer wrote no review text" >&2
            exit 1
        fi
        if ! job=$(review-gate review submit --id "$TASK" --channel "$FALLBACK_CHANNEL" --file "$fb_file"); then
            log "stop fallback-submit-failed"
            emit stop reason="fallback-submit-failed"
            echo "review submit failed; staged tree kept" >&2
            exit 1
        fi
        job=$(printf '{"record":%s}\n' "$job")
        rm -f "$fb_diff" "$fb_file"
        log "fallback submitted channel=$FALLBACK_CHANNEL"
        emit fallback_submitted channel="$FALLBACK_CHANNEL"
        rc=0
    fi
    case "$rc" in
        0) ;;
        3) log "stop no-external-ai"; emit stop reason="no-external-ai"; echo "no external AI available; stopping (staged tree kept)" >&2; exit 3 ;;
        *) log "stop review-failed rc=$rc"; emit stop reason="review-failed"; echo "review failed (exit $rc)" >&2; exit 1 ;;
    esac

    rid=$(printf '%s' "$job" | jget record id)
    verdict=$(printf '%s' "$job" | jget record verdict)
    text=$(printf '%s' "$job" | jget record text)
    channel=$(printf '%s' "$job" | jget record channel)
    rprovider=$(printf '%s' "$job" | jget record provider)
    [ -n "$rid" ] || { log "error no-review-id"; echo "review job returned no record id" >&2; exit 1; }
    log "review id=$rid verdict=$verdict channel=${channel:-unknown} provider=${rprovider:-unknown}"
    emit review id="$rid" verdict="$verdict" channel="${channel:-unknown}" provider="${rprovider:-unknown}"

    case "$verdict" in
        approved) echo "review $rid: approved" ;;
        rejected)
            echo "review $rid: rejected" >&2
            printf '%s\n' "$text" | sed 's/^/  /' >&2
            ;;
        *) echo "review $rid: ${verdict:-unknown}" >&2 ;;
    esac

    renew_lock || exit 2
    if [ "$verdict" = approved ]; then
        reviewed_tree=$(git -C "$repo" write-tree)
        tag="$TAG_PREFIX/$TASK/v$iteration"
        write_state reviewed "$iteration" "$rid" "" "$tag" "$msg" "$channel" "$head_base" "$started_clean" "" "$reviewed_tree"
        git -C "$repo" commit -q -m "$msg"
        git -C "$repo" tag "$tag"
        full=$(git -C "$repo" rev-parse HEAD)
        commit=$(git -C "$repo" rev-parse --short HEAD)
        write_state committed "$iteration" "$rid" "$full" "$tag" "$msg" "$channel" "$head_base" "$started_clean" "" "$reviewed_tree"
        log "approved review=$rid commit=$commit tag=$tag"
        emit approved review="$rid" commit="$commit" tag="$tag"
        out=$(review-gate record --id "$TASK" --review "$rid" --commit HEAD --tag "$tag")
        write_manifest "$iteration" "$rid" "$full" "$tag" "$channel" "$msg" "$rprovider" "$reviewed_tree" "$version_acceptance" "$VERIFY_CMD" \
            || { echo "cannot append approved version to $manifest; state kept" >&2; exit 1; }
        rm -f "$durable_feedback"
        rm -f "$retry_meta"
        : > "$feedback"
        rm -f "$state"
    else
        _all_untracked=$(collect_rejection_untracked)
        untracked_b64=$(subtract_untracked "$preserve_untracked" "$_all_untracked")
        write_state rejected "$iteration" "$rid" "" "" "$msg" "$channel" "$head_base" "$started_clean" "$text" "" "$untracked_b64"
        printf '%s\n' "$text" > "$feedback"
        printf '%s\n' "$text" > "$durable_feedback"
        write_retry_meta "$head_base" "$untracked_b64"
        log "rejected review=$rid"
        emit rejected review="$rid"
        out=$(review-gate record --id "$TASK" --review "$rid")
        git -C "$repo" reset -q
        if [ "$started_clean" -eq 1 ]; then
            clean_untracked "$untracked_b64"
            log "cleaned agent untracked files iteration=$iteration"
            emit cleaned iteration="$iteration"
        fi
        rm -f "$state"
    fi

    action=$(printf '%s' "$out" | jget action)
    log "record action=$action"
    emit record action="$action"
    duration=$(( $(date +%s) - round_start ))
    log "round-done duration=${duration}s"
    emit round_done duration="$duration"
    if [ "$report_init" -eq 1 ]; then
        printf '| %s | %s | %s | %s | %s | %s | %s | %ss |\n' \
            "$iteration" "$rid" "$verdict" "$action" "${commit:--}" "${tag:--}" "${channel:--}" "$duration" >>"$REPORT"
    fi
    if [ "$STOP_ON_REJECT" -eq 1 ] && [ "$verdict" != approved ]; then
        case "$action" in
            start_round|retry_same_round)
                log "stop on-reject verdict=$verdict"
                emit stop reason="stop-on-reject"
                echo "stopped after rejected review $rid; the task is still running" >&2
                exit 5
                ;;
        esac
    fi
    case "$action" in
        start_round|retry_same_round) continue ;;
        finalize)
            if [ "$VERIFY_RANGE" -eq 1 ]; then
                verify_range || exit 1
            fi
            log "finalized"; emit finalized; echo "finalized: $TASK"; exit 0 ;;
        pause)
            reason=$(review-gate task show --id "$TASK" | jget stop_reason)
            notify_event=paused
            log "paused reason=${reason:-none}"
            echo "paused by the gate: ${reason:-no reason}" >&2
            exit 4
            ;;
        *) log "stop unexpected-action=${action:-none}"; echo "unexpected record action: ${action:-none}" >&2; exit 1 ;;
    esac
done
