#!/bin/sh
set -eu

die() { echo "opencode-agent: $*" >&2; exit 1; }

repo=${AUTO_ITERATE_REPO:-}
iteration=${AUTO_ITERATE_ITERATION:-}
task=${AUTO_ITERATE_TASK:-unknown}
goal=${AUTO_ITERATE_GOAL:-}
acceptance=${AUTO_ITERATE_ACCEPTANCE:-}
version_acceptance=${AUTO_ITERATE_VERSION_ACCEPTANCE:-}
feedback=${AUTO_ITERATE_FEEDBACK:-}

[ -n "$repo" ] || die "AUTO_ITERATE_REPO is not set; run this through auto-iterate.sh"
[ -d "$repo" ] || die "repository does not exist: $repo"
case "$iteration" in
    ''|*[!0-9]*) die "AUTO_ITERATE_ITERATION is not an integer: $iteration" ;;
esac
git -C "$repo" rev-parse --is-inside-work-tree >/dev/null 2>&1 \
    || die "not a git work tree: $repo"

if [ -n "${OPENCODE_BIN:-}" ]; then
    bin=$(command -v "$OPENCODE_BIN" 2>/dev/null || printf '%s' "$OPENCODE_BIN")
else
    bin=
    if command -v opencode >/dev/null 2>&1; then
        bin=$(command -v opencode)
    elif [ -x "$HOME/.local/bin/opencode" ]; then
        bin=$HOME/.local/bin/opencode
    elif [ -x "$HOME/.opencode/bin/opencode" ]; then
        bin=$HOME/.opencode/bin/opencode
    fi
    [ -n "$bin" ] || die "opencode CLI not found; install it or set OPENCODE_BIN"
fi
[ -x "$bin" ] || die "OPENCODE_BIN is not executable: $bin"

case "$bin" in
    *.cmd|*.CMD|*.bat|*.BAT|*.ps1|*.PS1)
        die "OPENCODE_BIN must be an executable, not a Windows shell shim: $bin"
        ;;
    *.exe|*.EXE)
        case "$repo" in
            /mnt/[A-Za-z]/*) ;;
            *) die "Windows opencode cannot safely edit $repo; install the native Linux CLI or use a repository under /mnt/<drive>/" ;;
        esac
        ;;
esac

flags=${OPENCODE_FLAGS:---auto}
set -f
set -- $flags
set +f
if [ -n "${OPENCODE_MODEL:-}" ]; then set -- "$@" -m "$OPENCODE_MODEL"; fi
if [ -n "${OPENCODE_AGENT:-}" ]; then set -- "$@" --agent "$OPENCODE_AGENT"; fi

safe_task=$(printf '%s' "$task" | tr '/ ' '__')
ev=${AUTO_ITERATE_EVIDENCE:-${OPENCODE_EVIDENCE:-}}
if [ -z "$ev" ]; then
    ev=${TMPDIR:-/tmp}/auto-iterate-opencode-${safe_task}-v${iteration}.log
fi
case "$ev" in
    /*) ;;
    *) ev=$(pwd)/$ev ;;
esac
canon_repo=$(realpath "$repo" 2>/dev/null || readlink -f "$repo" 2>/dev/null || printf '%s' "$repo")
if command -v realpath >/dev/null 2>&1; then
    canon_ev=$(realpath -m "$ev" 2>/dev/null || printf '%s' "$ev")
else
    canon_ev=$(readlink -f "$ev" 2>/dev/null || printf '%s' "$ev")
fi
case "$canon_ev" in
    "$canon_repo"/*|"$canon_repo") die "evidence path must be outside the repository: $ev" ;;
esac
mkdir -p "$(dirname "$ev")" 2>/dev/null || die "cannot create evidence directory for $ev"

prompt=$(mktemp "${TMPDIR:-/tmp}/auto-iterate-opencode-prompt.XXXXXX")
cleanup() { rm -f "$prompt"; }
trap cleanup EXIT HUP INT TERM

{
    printf '%s\n' "You are the implementer for exactly one version of an automated iteration."
    printf '%s\n' ""
    printf 'Task: %s\n' "$task"
    printf 'Version: %s\n' "$iteration"
    printf 'Repository: %s\n' "$repo"
    printf '%s\n' ""
    printf '%s\n' "Goal:"
    printf '%s\n' "${goal:-(not provided)}"
    printf '%s\n' ""
    printf '%s\n' "Final acceptance:"
    printf '%s\n' "${acceptance:-(not provided)}"
    if [ -n "$version_acceptance" ]; then
        printf '%s\n' ""
        printf '%s\n' "Version acceptance:"
        printf '%s\n' "$version_acceptance"
    fi
    if [ -n "$feedback" ] && [ -s "$feedback" ]; then
        printf '%s\n' ""
        printf '%s\n' "Previous review feedback (address it in this version):"
        cat "$feedback"
    fi
    printf '%s\n' ""
    cat <<'RULES'
Rules:
1. Implement the smallest coherent version that moves toward the goal and final acceptance. Do one version only.
2. Do not commit, amend, tag, push, rebase, reset, checkout another branch, or change branches. Leave all changes uncommitted in the working tree; the driver owns git.
3. Do not modify .git, hooks, remotes, tags, or the index intentionally; the driver stages the tree.
4. Do not call review-gate, write or infer review verdicts, or read gate credentials. Do not set REVIEW_GATE_* variables.
5. Run the repository's existing tests or the most relevant checks. Report the exact commands and their results.
6. Do not leave caches, build artifacts, __pycache__, coverage files, or temporary files behind.
7. Do not claim tests you did not run. If you are blocked, make the smallest safe change and explain the limitation.
8. Do not modify files outside the repository or add secrets/credentials.
9. Finish with these sections: SUMMARY, TESTS, FILES, RISKS.
RULES
} >"$prompt"

for _rg_name in $(env | sed -n 's/^\(REVIEW_GATE_[A-Za-z0-9_]*\)=.*/\1/p'); do
    unset "$_rg_name"
done
unset _rg_name

snapshot_repo() {
    _snap=$(mktemp "${TMPDIR:-/tmp}/opencode-snapshot.XXXXXX") || return 1
    if ! (
        git -C "$repo" status --porcelain=v1 -z || exit 1
        git -C "$repo" write-tree || exit 1
        git -C "$repo" diff --binary || exit 1
        git -C "$repo" ls-files --others --exclude-standard -z |
            REPO=$repo xargs -0 -r sh -c '
                for f do
                    printf "U %s\0" "$f" || exit 1
                    git -C "$REPO" hash-object -- "$REPO/$f" || exit 1
                done
            ' sh || exit 1
    ) >"$_snap"; then
        rm -f "$_snap"
        return 1
    fi
    sha256sum "$_snap" | awk '{print $1}'
    rm -f "$_snap"
}

head_before=$(git -C "$repo" rev-parse HEAD)
tags_before=$(git -C "$repo" for-each-ref --format='%(refname) %(objectname)' refs/tags | LC_ALL=C sort)
changes_before=$(snapshot_repo)

printf 'opencode-agent: task=%s v%s repo=%s bin=%s evidence=%s\n' \
    "$task" "$iteration" "$repo" "$bin" "$ev" >&2

prompt_text=$(cat "$prompt")
set +e
(
    cd "$repo"
    exec "$bin" run "$@" -- "$prompt_text"
) >>"$ev" 2>&1
rc=$?
set -e

if [ "$rc" -ne 0 ]; then
    cat "$ev" >&2
    echo "opencode-agent: opencode run failed (exit $rc); see $ev" >&2
    exit "$rc"
fi

head_after=$(git -C "$repo" rev-parse HEAD)
if [ "$head_after" != "$head_before" ]; then
    cat "$ev" >&2
    die "opencode changed HEAD ($head_before -> $head_after); refusing to hand a committed tree to the driver"
fi
tags_after=$(git -C "$repo" for-each-ref --format='%(refname) %(objectname)' refs/tags | LC_ALL=C sort)
if [ "$tags_after" != "$tags_before" ]; then
    cat "$ev" >&2
    die "opencode changed tags; refusing to hand the tree to the driver"
fi
changes_after=$(snapshot_repo)
if [ "$changes_after" = "$changes_before" ]; then
    cat "$ev" >&2
    die "opencode made no changes; nothing to review"
fi

cat "$ev"
