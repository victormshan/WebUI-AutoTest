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

The agent command sees:
  AUTO_ITERATE_REPO, AUTO_ITERATE_ITERATION, AUTO_ITERATE_FEEDBACK
where AUTO_ITERATE_FEEDBACK is a file containing the previous review text
(empty on the first round).

Exit: 0 done/finalized, 3 no external AI, 4 paused by the gate,
      5 stopped on a rejection, 1 error.
TXT
}

TASK=; AGENT=; MESSAGE=; EVIDENCE=; MAX=10; TAG_PREFIX=auto-iterate; LOG=; AGENT_TIMEOUT=0; REPORT=; report_init=0; EVENTS=; REVIEW_TIMEOUT=0; CHECK=0; PROVIDER=; STOP_ON_REJECT=0; ALLOW_DIRTY=0; RESUME=0; state=
while [ $# -gt 0 ]; do
    case "$1" in
        --task) TASK=$2; shift 2 ;;
        --agent-cmd) AGENT=$2; shift 2 ;;
        --message) MESSAGE=$2; shift 2 ;;
        --evidence) EVIDENCE=$2; shift 2 ;;
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
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done
[ -n "$TASK" ] || { usage >&2; exit 2; }
if [ "$CHECK" -eq 0 ] && [ -z "$AGENT" ]; then
    usage >&2
    exit 2
fi

for c in review-gate git python3; do
    command -v "$c" >/dev/null 2>&1 || { echo "missing required command: $c" >&2; exit 1; }
done
case "$MAX" in ''|*[!0-9]*) echo "--max must be an integer" >&2; exit 2 ;; esac
[ "$MAX" -ge 1 ] || { echo "--max must be >= 1" >&2; exit 2; }
case "$AGENT_TIMEOUT" in ''|*[!0-9]*) echo "--agent-timeout must be an integer" >&2; exit 2 ;; esac
case "$REVIEW_TIMEOUT" in ''|*[!0-9]*) echo "--review-timeout must be an integer" >&2; exit 2 ;; esac
if [ "$AGENT_TIMEOUT" -gt 0 ] || [ "$REVIEW_TIMEOUT" -gt 0 ]; then
    command -v timeout >/dev/null 2>&1 || { echo "missing required command: timeout" >&2; exit 1; }
fi

jget() { python3 -c 'import json,sys
d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[k]
print(d if not isinstance(d, str) else d)' "$@" 2>/dev/null || true; }

if [ "$CHECK" -eq 1 ]; then
    for f in "$LOG" "$REPORT" "$EVENTS"; do
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

log() {
    [ -n "$LOG" ] || return 0
    printf '%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >>"$LOG"
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
    mkdir -p "$(dirname "$state")"
    python3 - "$state" "$_phase" "$_it" "$_rid" "$_commit" "$_tag" "$_msg" "$_chan" <<'PY'
import json, sys
p, phase, iteration, review, commit, tag, message, channel = sys.argv[1:]
with open(p, "w") as f:
    json.dump({"phase": phase, "iteration": int(iteration), "review": review,
               "commit": commit, "tag": tag, "message": message,
               "channel": channel}, f)
    f.write("\n")
PY
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
    log "resume phase=$rphase iteration=$rit review=$rrid"
    case "$rphase" in
        committed)
            [ -n "$rrid" ] && [ -n "$rcommit" ] && [ -n "$rtag" ] || { echo "resume: committed state is incomplete" >&2; return 1; }
            git -C "$repo" rev-parse "$rcommit^{commit}" >/dev/null 2>&1 || { echo "resume: commit $rcommit is missing" >&2; return 1; }
            out=$(review-gate record --id "$TASK" --review "$rrid" --commit "$rcommit" --tag "$rtag") || {
                echo "resume: record failed; state kept" >&2; return 1; }
            ;;
        reviewed)
            [ -n "$rrid" ] || { echo "resume: reviewed state has no review" >&2; return 1; }
            if git -C "$repo" diff --cached --quiet; then
                rcommit=$(git -C "$repo" rev-parse HEAD)
            else
                [ -n "$rmsg" ] || rmsg="auto-iterate: version $rit"
                git -C "$repo" commit -q -m "$rmsg" || { echo "resume: commit failed" >&2; return 1; }
                rcommit=$(git -C "$repo" rev-parse HEAD)
            fi
            rtag="$TAG_PREFIX/$TASK/v$rit"
            if ! git -C "$repo" rev-parse "refs/tags/$rtag" >/dev/null 2>&1; then
                git -C "$repo" tag "$rtag" || { echo "resume: tag failed" >&2; return 1; }
            fi
            out=$(review-gate record --id "$TASK" --review "$rrid" --commit "$rcommit" --tag "$rtag") || {
                echo "resume: record failed; state kept" >&2; return 1; }
            ;;
        *) echo "resume: unknown phase ${rphase:-none}" >&2; return 1 ;;
    esac
    raction=$(printf '%s' "$out" | jget action)
    rm -f "$state"
    log "resume recorded review=$rrid commit=$rcommit tag=$rtag action=$raction"
    emit resume review="$rrid" commit="$rcommit" tag="$rtag" action="$raction"
    if [ "$report_init" -eq 1 ]; then
        printf '| %s | %s | approved | %s | %s | %s | %s | - |\n' \
            "$rit" "$rrid" "$raction" "${rcommit:--}" "${rtag:--}" "${rchan:--}" >>"$REPORT"
    fi
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
    rm -f "$feedback"
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
log "start task=$TASK max=$MAX"
emit start task="$TASK" max="$MAX"

while :; do
    show=$(review-gate task show --id "$TASK")
    status=$(printf '%s' "$show" | jget status)
    [ "$status" = running ] || { log "stop status=${status:-unknown}"; emit stop reason="status" status="${status:-unknown}"; echo "task $TASK is ${status:-unknown}; stopping"; exit 0; }
    iteration=$(printf '%s' "$show" | jget current_iteration)
    repo=$(printf '%s' "$show" | jget repo)
    [ -n "$repo" ] || { echo "task $TASK has no repo" >&2; exit 1; }
    report_header
    if [ -z "$state" ]; then
        gitdir=$(git -C "$repo" rev-parse --absolute-git-dir)
        state="$gitdir/auto-iterate/$(printf '%s' "$TASK" | tr '/ ' '__').json"
    fi
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
    if [ "$round" -eq 0 ] && [ "$ALLOW_DIRTY" -eq 0 ]; then
        dirty=$(git -C "$repo" status --porcelain)
        if [ -n "$dirty" ]; then
            log "stop dirty-worktree"
            emit stop reason="dirty-worktree"
            printf 'worktree %s is dirty; commit/stash first or pass --allow-dirty:\n%s\n' "$repo" "$dirty" >&2
            exit 1
        fi
    fi

    goal=$(printf '%s' "$show" | jget goal)
    acceptance=$(printf '%s' "$show" | jget final_acceptance)
    commit=-
    tag=-
    channel=-
    round=$((round + 1))
    if [ "$round" -gt "$MAX" ]; then
        log "stop max-rounds"
        emit stop reason="max-rounds"
        echo "reached --max $MAX; stopping with the task still running" >&2
        exit 0
    fi
    echo "== round $round (version $iteration) =="
    log "round=$round iteration=$iteration repo=$repo"
    emit round round="$round" iteration="$iteration"
    round_start=$(date +%s)

    export AUTO_ITERATE_REPO="$repo" AUTO_ITERATE_ITERATION="$iteration" \
        AUTO_ITERATE_FEEDBACK="$feedback" AUTO_ITERATE_GOAL="$goal" \
        AUTO_ITERATE_ACCEPTANCE="$acceptance" AUTO_ITERATE_TASK="$TASK"
    set +e
    if [ "$AGENT_TIMEOUT" -gt 0 ]; then
        ( cd "$repo" && timeout "$AGENT_TIMEOUT" sh -c "$AGENT" )
    else
        ( cd "$repo" && sh -c "$AGENT" )
    fi
    arc=$?
    set -e
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

    review-gate stage --id "$TASK" >/dev/null

    set +e
    set -- review-gate review run --id "$TASK"
    if [ -n "$EVIDENCE" ]; then set -- "$@" --evidence "$EVIDENCE"; fi
    if [ -n "$PROVIDER" ]; then set -- "$@" --provider "$PROVIDER"; fi
    if [ "$REVIEW_TIMEOUT" -gt 0 ]; then
        job=$(timeout "$REVIEW_TIMEOUT" "$@")
    else
        job=$("$@")
    fi
    rc=$?
    set -e
    if [ "$rc" -eq 124 ] && [ "$REVIEW_TIMEOUT" -gt 0 ]; then
        log "stop review-timeout seconds=$REVIEW_TIMEOUT"
        emit stop reason="review-timeout"
        echo "review timed out after ${REVIEW_TIMEOUT}s; the task may have a pending review" >&2
        exit 3
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

    if [ "$verdict" = approved ]; then
        msg=${MESSAGE:-"auto-iterate: version $iteration"}
        write_state reviewed "$iteration" "$rid" "" "" "$msg" "$channel"
        git -C "$repo" commit -q -m "$msg"
        tag="$TAG_PREFIX/$TASK/v$iteration"
        git -C "$repo" tag "$tag"
        full=$(git -C "$repo" rev-parse HEAD)
        commit=$(git -C "$repo" rev-parse --short HEAD)
        write_state committed "$iteration" "$rid" "$full" "$tag" "$msg" "$channel"
        log "approved review=$rid commit=$commit tag=$tag"
        emit approved review="$rid" commit="$commit" tag="$tag"
        out=$(review-gate record --id "$TASK" --review "$rid" --commit HEAD --tag "$tag")
        rm -f "$state"
    else
        printf '%s\n' "$text" > "$feedback"
        log "rejected review=$rid"
        emit rejected review="$rid"
        out=$(review-gate record --id "$TASK" --review "$rid")
        git -C "$repo" reset -q
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
        finalize) log "finalized"; emit finalized; echo "finalized: $TASK"; exit 0 ;;
        pause)
            reason=$(review-gate task show --id "$TASK" | jget stop_reason)
            log "paused reason=${reason:-none}"
            echo "paused by the gate: ${reason:-no reason}" >&2
            exit 4
            ;;
        *) log "stop unexpected-action=${action:-none}"; echo "unexpected record action: ${action:-none}" >&2; exit 1 ;;
    esac
done
