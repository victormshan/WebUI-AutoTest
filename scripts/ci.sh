#!/usr/bin/env bash
# Regression run used by CI (and runnable locally):
#   build -> start the demo shop -> replay all flows -> JUnit + Markdown reports.
#
# Env:
#   WEBTEST_REPORT_DIR   output directory (default: reports)
#   ANTHROPIC_API_KEY    if set, broken locators are healed by the LLM;
#                        otherwise replay is fully deterministic (--no-heal)
#   WEBTEST_FLOWS        flow globs (default: flows/*.yaml flows/explored/*.yaml)
set -euo pipefail
cd "$(dirname "$0")/.."

out="${WEBTEST_REPORT_DIR:-reports}"
mkdir -p "$out"

# rustup installs into ~/.cargo; non-login shells may not have it on PATH.
# shellcheck source=/dev/null
command -v cargo >/dev/null || . "$HOME/.cargo/env"
cargo build --release --locked -p webtest
bin=target/release/webtest

python3 fixtures/shop/server.py 8765 >"$out/server.log" 2>&1 &
server=$!
trap 'kill "$server" 2>/dev/null || true' EXIT
for _ in $(seq 50); do
  curl -sf -o /dev/null http://127.0.0.1:8765/ && break
  sleep 0.2
done

heal=(--no-heal)
if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
  heal=()
fi

# shellcheck disable=SC2206 # globs are meant to expand
flows=(${WEBTEST_FLOWS:-flows/*.yaml flows/explored/*.yaml})

status=0
"$bin" replay "${flows[@]}" "${heal[@]}" \
  --junit "$out/junit.xml" --markdown "$out/summary.md" || status=$?

if [ -n "${GITHUB_STEP_SUMMARY:-}" ] && [ -f "$out/summary.md" ]; then
  cat "$out/summary.md" >>"$GITHUB_STEP_SUMMARY"
fi
# Failure snapshots and healed flows help debugging; never ship login state.
[ -d runs ] && cp -r runs "$out/" || true
find flows -name '*.healed.yaml' -exec cp {} "$out/" \; 2>/dev/null || true

exit "$status"
