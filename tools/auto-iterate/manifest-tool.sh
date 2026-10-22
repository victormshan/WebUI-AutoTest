#!/bin/sh
set -eu

usage() {
    cat >&2 <<'EOF'
usage:
  manifest-tool.sh validate FILE
  manifest-tool.sh repair FILE
  manifest-tool.sh export FILE [json|csv]

validate  check JSONL, required fields and duplicate task/version pairs
repair    atomically keep valid lines and quarantine bad/duplicate lines
export    print the manifest as a JSON array (default) or CSV
EOF
    exit 2
}

[ $# -ge 2 ] || usage
cmd=$1
file=$2
shift 2

common='
import json, os, sys

REQUIRED = ("at", "task", "version", "iteration", "review", "verdict",
            "commit", "tag", "channel", "provider")

def load(path):
    entries, bad = [], []
    with open(path, encoding="utf-8") as f:
        for lineno, raw in enumerate(f, 1):
            if not raw.strip():
                continue
            try:
                e = json.loads(raw)
            except ValueError as exc:
                bad.append((lineno, raw, "invalid JSON: %s" % exc))
                continue
            errors = []
            if not isinstance(e, dict):
                errors.append("record is not an object")
            else:
                for key in REQUIRED:
                    if key not in e:
                        errors.append("missing %s" % key)
                try:
                    version = int(e.get("version"))
                    iteration = int(e.get("iteration"))
                except (TypeError, ValueError):
                    errors.append("version/iteration is not an integer")
                else:
                    if version < 1:
                        errors.append("version must be positive")
                    if iteration != version:
                        errors.append("iteration does not match version")
                commit = str(e.get("commit", ""))
                if len(commit) != 40 or any(c not in "0123456789abcdef" for c in commit):
                    errors.append("commit is not a full sha1")
                if e.get("verdict") != "approved":
                    errors.append("verdict is not approved")
            if errors:
                bad.append((lineno, raw, "; ".join(errors)))
            else:
                entries.append((lineno, raw, e))
    return entries, bad

def duplicates(entries):
    seen, dup = set(), []
    for lineno, raw, e in entries:
        key = (str(e["task"]), int(e["version"]))
        if key in seen:
            dup.append((lineno, raw, "duplicate %s v%s" % key))
        else:
            seen.add(key)
    return dup
'

case "$cmd" in
    validate)
        [ $# -eq 0 ] || usage
        [ -f "$file" ] || { echo "manifest not found: $file" >&2; exit 1; }
        python3 - "$file" <<PY
$common
entries, bad = load(sys.argv[1])
for lineno, raw, why in bad:
    print("line %d: %s" % (lineno, why), file=sys.stderr)
for lineno, raw, why in duplicates(entries):
    print("line %d: %s" % (lineno, why), file=sys.stderr)
if bad or duplicates(entries):
    sys.exit(1)
print("ok %d entries" % len(entries))
PY
        ;;
    repair)
        [ $# -eq 0 ] || usage
        [ -f "$file" ] || { echo "manifest not found: $file" >&2; exit 1; }
        python3 - "$file" <<PY
$common
import datetime
path = sys.argv[1]
entries, bad = load(path)
bad += duplicates(entries)
if not bad:
    print("ok 0 repaired")
    sys.exit(0)
bad_lines = {lineno for lineno, _, _ in bad}
stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
quarantine = "%s.corrupt.%s.%d" % (path, stamp, os.getpid())
with open(quarantine, "w", encoding="utf-8") as out:
    for lineno, raw, _ in bad:
        out.write(raw if raw.endswith("\n") else raw + "\n")
    out.flush()
    os.fsync(out.fileno())
tmp = "%s.tmp.%d" % (path, os.getpid())
with open(tmp, "w", encoding="utf-8") as out:
    for lineno, raw, _ in entries:
        if lineno in bad_lines:
            continue
        out.write(raw if raw.endswith("\n") else raw + "\n")
    out.flush()
    os.fsync(out.fileno())
os.replace(tmp, path)
dfd = os.open(os.path.dirname(path) or ".", os.O_RDONLY)
try:
    os.fsync(dfd)
finally:
    os.close(dfd)
print("repaired %d line(s) -> %s" % (len(bad), quarantine))
PY
        ;;
    export)
        fmt=json
        if [ $# -eq 1 ]; then fmt=$1; shift; fi
        [ $# -eq 0 ] || usage
        case "$fmt" in json|csv) ;; *) usage ;; esac
        [ -f "$file" ] || { echo "manifest not found: $file" >&2; exit 1; }
        python3 - "$file" "$fmt" <<PY
$common
import csv
path, fmt = sys.argv[1:3]
entries, bad = load(path)
bad += duplicates(entries)
if bad:
    for lineno, raw, why in bad:
        print("line %d: %s" % (lineno, why), file=sys.stderr)
    sys.exit(1)
rows = [e for _, _, e in entries]
if fmt == "json":
    print(json.dumps(rows, ensure_ascii=False, indent=2))
else:
    cols = ["at", "task", "version", "review", "commit", "tag", "channel",
            "provider", "message", "acceptance", "verify_cmd"]
    w = csv.DictWriter(sys.stdout, fieldnames=cols, extrasaction="ignore")
    w.writeheader()
    for row in rows:
        w.writerow(row)
PY
        ;;
    *) usage ;;
esac
