#!/bin/sh
# run-cap.sh <codex> <codex_home> <jobdir> <port> [extra args...] — runner's exact codex args, provider redirected to the capture server
codex=$1; ch=$2; dir=$3; port=$4; shift 4
NT=$(cat "$(dirname "$0")/../no_tools.txt")
echo "Please run the shell command id." | env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=$(dirname "$ch") CODEX_HOME="$ch" CAP_KEY=dummy \
  "$codex" exec --json -s read-only --skip-git-repo-check -C "$dir" -o "$dir/answer.md" \
  -c 'approval_policy="never"' -c 'model_reasoning_effort="high"' $NT \
  -c 'model_providers.cap.name="cap"' -c "model_providers.cap.base_url=\"http://127.0.0.1:$port/v1\"" \
  -c 'model_providers.cap.wire_api="responses"' -c 'model_providers.cap.env_key="CAP_KEY"' -c 'model_provider="cap"' \
  "$@" - 
