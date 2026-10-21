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

The agent command sees:
  AUTO_ITERATE_REPO, AUTO_ITERATE_ITERATION, AUTO_ITERATE_FEEDBACK
where AUTO_ITERATE_FEEDBACK is a file containing the previous review text
(empty on the first round).

Exit: 0 done/finalized, 3 no external AI, 4 paused by the gate, 1 error.
TXT
}

TASK=; AGENT=; MESSAGE=; EVIDENCE=; MAX=10; TAG_PREFIX=auto-iterate
while [ $# -gt 0 ]; do
    case "$1" in
        --task) TASK=$2; shift 2 ;;
        --agent-cmd) AGENT=$2; shift 2 ;;
        --message) MESSAGE=$2; shift 2 ;;
        --evidence) EVIDENCE=$2; shift 2 ;;
        --max) MAX=$2; shift 2 ;;
        --tag-prefix) TAG_PREFIX=$2; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done
[ -n "$TASK" ] && [ -n "$AGENT" ] || { usage >&2; exit 2; }

jget() { python3 -c 'import json,sys
d=json.load(sys.stdin)
for k in sys.argv[1:]:
    d = d[k]
print(d if not isinstance(d, str) else d)' "$@" 2>/dev/null || true; }

feedback=$(mktemp "${TMPDIR:-/tmp}/auto-iterate-feedback.XXXXXX")
trap 'rm -f "$feedback"' EXIT
: > "$feedback"
round=0

while :; do
    show=$(review-gate task show --id "$TASK")
    status=$(printf '%s' "$show" | jget status)
    [ "$status" = running ] || { echo "task $TASK is ${status:-unknown}; stopping"; exit 0; }
    iteration=$(printf '%s' "$show" | jget current_iteration)
    repo=$(printf '%s' "$show" | jget repo)
    [ -n "$repo" ] || { echo "task $TASK has no repo" >&2; exit 1; }

    round=$((round + 1))
    if [ "$round" -gt "$MAX" ]; then
        echo "reached --max $MAX; stopping with the task still running" >&2
        exit 0
    fi
    echo "== round $round (version $iteration) =="

    export AUTO_ITERATE_REPO="$repo" AUTO_ITERATE_ITERATION="$iteration" \
        AUTO_ITERATE_FEEDBACK="$feedback"
    ( cd "$repo" && sh -c "$AGENT" )

    review-gate stage --id "$TASK" >/dev/null

    set +e
    if [ -n "$EVIDENCE" ]; then
        job=$(review-gate review run --id "$TASK" --evidence "$EVIDENCE")
    else
        job=$(review-gate review run --id "$TASK")
    fi
    rc=$?
    set -e
    case "$rc" in
        0) ;;
        3) echo "no external AI available; stopping (staged tree kept)" >&2; exit 3 ;;
        *) echo "review failed (exit $rc)" >&2; exit 1 ;;
    esac

    rid=$(printf '%s' "$job" | jget record id)
    verdict=$(printf '%s' "$job" | jget record verdict)
    text=$(printf '%s' "$job" | jget record text)
    [ -n "$rid" ] || { echo "review job returned no record id" >&2; exit 1; }

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
        git -C "$repo" commit -q -m "$msg"
        tag="$TAG_PREFIX/$TASK/v$iteration"
        git -C "$repo" tag "$tag"
        out=$(review-gate record --id "$TASK" --review "$rid" --commit HEAD --tag "$tag")
    else
        printf '%s\n' "$text" > "$feedback"
        out=$(review-gate record --id "$TASK" --review "$rid")
        git -C "$repo" reset -q
    fi

    action=$(printf '%s' "$out" | jget action)
    case "$action" in
        start_round|retry_same_round) continue ;;
        finalize) echo "finalized: $TASK"; exit 0 ;;
        pause)
            reason=$(review-gate task show --id "$TASK" | jget stop_reason)
            echo "paused by the gate: ${reason:-no reason}" >&2
            exit 4
            ;;
        *) echo "unexpected record action: ${action:-none}" >&2; exit 1 ;;
    esac
done
