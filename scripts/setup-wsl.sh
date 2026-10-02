#!/usr/bin/env bash
# Root-free toolchain setup for WSL/Ubuntu without sudo:
#   - Rust (rustup) with zig as the C compiler/linker (no gcc needed)
#   - Chrome for Testing + the few shared libs it lacks, under ~/.local
set -euo pipefail

ZIG_VERSION=0.15.2
BIN="$HOME/.local/bin"
mkdir -p "$BIN" "$HOME/.local/opt" "$HOME/.local/chrome-libs" "$HOME/.cache/chrome"

# --- Rust ---------------------------------------------------------------
if ! command -v "$HOME/.cargo/bin/cargo" >/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
"$HOME/.cargo/bin/rustup" component add rustfmt clippy

# --- zig as cc ----------------------------------------------------------
if ! command -v cc >/dev/null; then
  if [ ! -x "$BIN/zig" ]; then
    curl -sSfL "https://ziglang.org/download/$ZIG_VERSION/zig-x86_64-linux-$ZIG_VERSION.tar.xz" \
      | tar xJ -C "$HOME/.local/opt"
    ln -sf "$HOME/.local/opt/zig-x86_64-linux-$ZIG_VERSION/zig" "$BIN/zig"
  fi
  cat >"$BIN/zigcc" <<'EOF'
#!/bin/sh
# zig as a root-free C compiler/linker for Rust (no gcc on this host).
# Drops rustc/cc-style --target flags, which zig cannot parse.
for a in "$@"; do
  shift
  case "$a" in --target=*) ;; *) set -- "$@" "$a" ;; esac
done
exec "$HOME/.local/bin/zig" cc -target x86_64-linux-gnu "$@"
EOF
  # shellcheck disable=SC2016 # $HOME/$@ must stay literal in the generated script
  printf '#!/bin/sh\nexec "$HOME/.local/bin/zig" ar "$@"\n' >"$BIN/zigar"
  chmod +x "$BIN/zigcc" "$BIN/zigar"
  if ! grep -q zigcc "$HOME/.cargo/config.toml" 2>/dev/null; then
    cat >>"$HOME/.cargo/config.toml" <<EOF
[target.x86_64-unknown-linux-gnu]
linker = "$BIN/zigcc"

[env]
CC_x86_64_unknown_linux_gnu = "$BIN/zigcc"
AR_x86_64_unknown_linux_gnu = "$BIN/zigar"
EOF
  fi
fi

# --- Chrome for Testing -------------------------------------------------
if [ ! -x "$HOME/.cache/chrome/chrome-linux64/chrome" ]; then
  url=$(python3 -c "import json,urllib.request;d=json.load(urllib.request.urlopen('https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json'));print([x['url'] for x in d['channels']['Stable']['downloads']['chrome'] if x['platform']=='linux64'][0])")
  curl -sSfL -o /tmp/chrome-linux64.zip "$url"
  python3 - <<'EOF'
import os, zipfile
z = zipfile.ZipFile('/tmp/chrome-linux64.zip')
for i in z.infolist():
    p = z.extract(i, os.path.expanduser('~/.cache/chrome'))
    m = (i.external_attr >> 16) & 0o777
    if m:
        os.chmod(p, m)
EOF
fi

missing=$(LD_LIBRARY_PATH="$HOME/.local/chrome-libs" ldd "$HOME/.cache/chrome/chrome-linux64/chrome" | grep -c 'not found' || true)
if [ "$missing" != 0 ]; then
  tmp=$(mktemp -d)
  (cd "$tmp" && apt-get download libnspr4 libnss3 libasound2t64 && for f in *.deb; do dpkg-deb -x "$f" x; done)
  find "$tmp/x" -name '*.so*' -exec cp -a {} "$HOME/.local/chrome-libs/" \;
  rm -rf "$tmp"
fi

cat >"$BIN/chrome-wsl" <<'EOF'
#!/bin/sh
# Chrome for Testing wrapper: supplies user-local NSS/ALSA libs (no root install).
export LD_LIBRARY_PATH="$HOME/.local/chrome-libs${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
exec "$HOME/.cache/chrome/chrome-linux64/chrome" "$@"
EOF
chmod +x "$BIN/chrome-wsl"

"$BIN/chrome-wsl" --headless --no-sandbox --dump-dom 'data:text/html,<p>ok</p>' >/dev/null
echo "setup complete: $("$HOME/.cargo/bin/cargo" --version), $("$BIN/chrome-wsl" --version)"
