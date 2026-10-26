#!/usr/bin/env bash
# Rehearsal of RUNBOOK §3.2 on fixtures with the live permissions, using the binary to deploy
# (Codex review of v4, item 1): four 0600 token files, a 0640 legacy client.token and a 0640
# agents.json in a 0755 directory. No root (the converter trusts files owned by its own user).
#   REVIEW_GATE_BIN=<the review-gate to deploy> bash scripts/m2-e2e/tests/test_identity_migration.sh
set -euo pipefail
BIN=${REVIEW_GATE_BIN:?set REVIEW_GATE_BIN to the review-gate binary to deploy}
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
ETC=$TMP/etc; TOK=$TMP/tokens; WORK=$TMP/root-m2-id
mkdir -p "$ETC" "$TOK" "$WORK"; chmod 755 "$ETC"; chmod 700 "$WORK"
declare -A T
for p in claude opencode codex dsh; do
  T[$p]=$(head -c 48 /dev/urandom | base64 -w0)
  printf '%s\n' "${T[$p]}" > "$TOK/$p.token"; chmod 600 "$TOK/$p.token"
done
LEGACY=$(head -c 48 /dev/urandom | base64 -w0)
printf '%s\n' "$LEGACY" > "$ETC/client.token"; chmod 640 "$ETC/client.token"
python3 - "$ETC/agents.json" "${T[claude]}" "${T[opencode]}" "${T[codex]}" "${T[dsh]}" <<'PY'
import json, sys
out, c, o, x, d = sys.argv[1:]
json.dump({c: "claude", o: "deepseek", x: "openai", d: "deepseek"}, open(out, "w"))
PY
chmod 640 "$ETC/agents.json"
before=$(sha256sum "$ETC/client.token" "$ETC/agents.json"; stat -c '%a %n' "$ETC/client.token")

convert() { # legacy-file out
  "$BIN" admin identity-from-agents \
    --credential claude=claude="$TOK/claude.token" --credential opencode=deepseek="$TOK/opencode.token" \
    --credential codex=openai="$TOK/codex.token" --credential dsh=deepseek="$TOK/dsh.token" \
    --legacy "$1" --agents "$ETC/agents.json" --out "$2"
}

# 1. The live 0640 client.token as --legacy: refused, nothing written, the original unchanged.
set +e; convert "$ETC/client.token" "$WORK/identity-direct.json" > "$TMP/direct.out" 2>&1; rc=$?; set -e
[ "$rc" != 0 ] && grep -q "mode 0600 or stricter" "$TMP/direct.out" && [ ! -e "$WORK/identity-direct.json" ] \
  && echo "ok - a 0640 legacy token is refused (exit $rc), no output" || { cat "$TMP/direct.out"; exit 1; }

# 2. RUNBOOK §3.2: a 0600 copy in the private work directory, then the conversion.
install -m 0600 "$ETC/client.token" "$WORK/client.token.copy"
convert "$WORK/client.token.copy" "$WORK/identity.json" > "$TMP/convert.out" 2>&1
echo "ok - the conversion with the 0600 copy succeeds"; sed 's/^/     /' "$TMP/convert.out"
rm -f "$WORK/client.token.copy"
[ ! -e "$WORK/client.token.copy" ] && echo "ok - the plaintext copy is removed"

# 3. The result: hashes of what clients send (trimmed), no plaintext token, the original unchanged.
python3 - "$WORK/identity.json" "$TOK" "$ETC/client.token" <<'PY'
import hashlib, json, sys, os
ids = json.load(open(sys.argv[1]))
text = open(sys.argv[1]).read()
got = {p["id"]: (p["family"], p["credentials"][0]["sha256"]) for p in ids["principals"]}
want = {"claude": "claude", "opencode": "deepseek", "codex": "openai", "dsh": "deepseek", "legacy-claude": "claude"}
files = {p: os.path.join(sys.argv[2], p + ".token") for p in ["claude", "opencode", "codex", "dsh"]}
files["legacy-claude"] = sys.argv[3]
for p, fam in want.items():
    tok = open(files[p]).read().strip()
    assert got[p] == (fam, hashlib.sha256(tok.encode()).hexdigest()), p
    assert tok not in text, p
print("ok - 5 principals, each hash = sha256(trimmed token), families as expected, no token in the file")
PY
after=$(sha256sum "$ETC/client.token" "$ETC/agents.json"; stat -c '%a %n' "$ETC/client.token")
[ "$before" = "$after" ] && echo "ok - client.token and agents.json unchanged (content and mode)"
echo "ALL OK"
