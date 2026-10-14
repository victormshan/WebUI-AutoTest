#!/bin/sh
# Wrapper used as the runner's --codex: runs the real Codex with the runner's exact arguments,
# only redirecting the model provider to the local capture server (port in CAP_PORT file next to
# this script). The provider flags go before the final "-".
here=$(dirname "$0"); real=$(cat "$here/real-codex"); port=$(cat "$here/cap-port")
last=""; set -- "$@"; n=$#; i=0; args=""
for a in "$@"; do i=$((i+1)); if [ $i -lt $n ]; then set -- "$@" "$a"; fi; done
shift $n
exec env CAP_KEY=dummy "$real" "$@" -c 'model_providers.cap.name="cap"' -c "model_providers.cap.base_url=\"http://127.0.0.1:$port/v1\"" -c 'model_providers.cap.wire_api="responses"' -c 'model_providers.cap.env_key="CAP_KEY"' -c 'model_provider="cap"' -
