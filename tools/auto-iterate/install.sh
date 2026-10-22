#!/bin/sh
# Install the auto-iterate skill and helper links without copying files.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
skill_src=$here/SKILL.md
driver_src=$here/auto-iterate.sh
codex_src=$here/codex-agent.sh

usage() {
    cat <<'TXT'
usage: install.sh [options]

  --skill-dir DIR  link SKILL.md as DIR/SKILL.md
  --bin-dir DIR    link auto-iterate.sh and codex-agent.sh into DIR
  --check          verify the requested links, change nothing
  --dry-run        print planned links, change nothing
  --force          replace an existing regular file or different symlink
  --upgrade        idempotent install; never replaces foreign files
  --uninstall      remove only links that point at this checkout

At least one of --skill-dir or --bin-dir is required. By default an existing
regular file is never overwritten. uninstall never removes a regular file or a
symlink owned by another installation.
TXT
}

SKILL_DIR=; BIN_DIR=; CHECK=0; DRY=0; FORCE=0; UNINSTALL=0; UPGRADE=0
while [ $# -gt 0 ]; do
    case "$1" in
        --skill-dir) SKILL_DIR=$2; shift 2 ;;
        --bin-dir) BIN_DIR=$2; shift 2 ;;
        --check) CHECK=1; shift ;;
        --dry-run) DRY=1; shift ;;
        --force) FORCE=1; shift ;;
        --upgrade) UPGRADE=1; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done
[ -n "$SKILL_DIR" ] || [ -n "$BIN_DIR" ] || { usage >&2; exit 2; }
[ "$UNINSTALL" -eq 0 ] || [ "$UPGRADE" -eq 0 ] || { echo "--uninstall and --upgrade are exclusive" >&2; exit 2; }
[ "$UNINSTALL" -eq 0 ] || [ "$CHECK" -eq 0 ] || { echo "--uninstall cannot be combined with --check" >&2; exit 2; }
if [ "$UNINSTALL" -eq 0 ]; then
    [ -f "$skill_src" ] || { echo "missing $skill_src" >&2; exit 1; }
    [ -f "$driver_src" ] || { echo "missing $driver_src" >&2; exit 1; }
    [ -f "$codex_src" ] || { echo "missing $codex_src" >&2; exit 1; }
fi

install_link() {
    _src=$1; _dest=$2
    if [ -L "$_dest" ]; then
        _current=$(readlink "$_dest") || { echo "cannot read link $_dest" >&2; return 1; }
        if [ "$_current" = "$_src" ]; then
            [ "$CHECK" -eq 1 ] && echo "ok: $_dest -> $_src" || true
            return 0
        fi
        if [ "$CHECK" -eq 1 ]; then
            echo "check: $_dest points to $_current, not $_src" >&2
            return 1
        fi
        _upgrade_replace=0
        if [ "$UPGRADE" -eq 1 ]; then
            case "$_current" in
                */tools/auto-iterate/"$(basename "$_src")") _upgrade_replace=1 ;;
            esac
        fi
        if [ "$FORCE" -eq 0 ] && [ "$_upgrade_replace" -eq 0 ]; then
            echo "refusing to replace symlink $_dest -> $_current (use --force)" >&2
            return 1
        fi
    elif [ -e "$_dest" ]; then
        if [ "$CHECK" -eq 1 ]; then
            echo "check: $_dest exists and is not a symlink" >&2
            return 1
        fi
        if [ "$FORCE" -eq 0 ]; then
            echo "refusing to replace regular file $_dest (use --force)" >&2
            return 1
        fi
    elif [ "$CHECK" -eq 1 ]; then
        echo "check: missing link $_dest -> $_src" >&2
        return 1
    fi
    if [ "$DRY" -eq 1 ]; then
        echo "would install: $_dest -> $_src"
        return 0
    fi
    mkdir -p "$(dirname "$_dest")" || return 1
    [ ! -e "$_dest" ] || rm -f "$_dest" || return 1
    ln -s "$_src" "$_dest" || return 1
    echo "installed: $_dest -> $_src"
}

uninstall_link() {
    _src=$1; _dest=$2
    if [ -L "$_dest" ]; then
        _current=$(readlink "$_dest") || { echo "cannot read link $_dest" >&2; return 1; }
        if [ "$_current" != "$_src" ]; then
            echo "skip: $_dest -> $_current (not ours)" >&2
            return 1
        fi
        if [ "$DRY" -eq 1 ]; then
            echo "would remove: $_dest -> $_src"
            return 0
        fi
        rm -f "$_dest" || return 1
        echo "removed: $_dest"
    elif [ -e "$_dest" ]; then
        echo "skip: regular file $_dest (not ours)" >&2
        return 1
    else
        echo "absent: $_dest"
    fi
    return 0
}

if [ "$UNINSTALL" -eq 1 ]; then
    _rc=0
    if [ -n "$SKILL_DIR" ]; then
        uninstall_link "$skill_src" "$SKILL_DIR/SKILL.md" || _rc=1
    fi
    if [ -n "$BIN_DIR" ]; then
        uninstall_link "$driver_src" "$BIN_DIR/auto-iterate.sh" || _rc=1
        uninstall_link "$codex_src" "$BIN_DIR/codex-agent.sh" || _rc=1
    fi
    exit "$_rc"
fi

if [ -n "$SKILL_DIR" ]; then
    install_link "$skill_src" "$SKILL_DIR/SKILL.md" || exit 1
fi
if [ -n "$BIN_DIR" ]; then
    install_link "$driver_src" "$BIN_DIR/auto-iterate.sh" || exit 1
    install_link "$codex_src" "$BIN_DIR/codex-agent.sh" || exit 1
fi
if [ "$UPGRADE" -eq 1 ]; then
    echo "upgrade ok"
fi
exit 0
