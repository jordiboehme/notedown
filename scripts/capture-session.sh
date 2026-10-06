#!/usr/bin/env bash
# Start the notedown capture server for a Nomad session.
# Usage: scripts/capture-session.sh <data dir>
set -euo pipefail

if [ $# -ne 1 ]; then
  echo "usage: scripts/capture-session.sh <data dir>" >&2
  exit 2
fi
data="$1"
root="$(cd "$(dirname "$0")/.." && pwd)"
bin="$root/target/release/notedown"
cd "$root"

if ! command -v cargo >/dev/null 2>&1; then
  tc="$HOME/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin"
  export PATH="$tc:$PATH"
fi

stale() {
  [ ! -x "$bin" ] && return 0
  [ -n "$(find crates Cargo.toml Cargo.lock \
    -type f \( -name '*.rs' -o -name '*.toml' -o -name Cargo.lock \) \
    -newer "$bin" -print -quit)" ]
}

if stale; then
  cargo build --release -p notedown
fi

accounts="$("$bin" cloud account --data "$data" list)"
if [ -z "$accounts" ]; then
  echo "No account in $data yet. In $root run:" >&2
  echo "" >&2
  echo "  d=\"$data\"" >&2
  echo "  target/release/notedown cloud account --data \"\$d\" \\" >&2
  echo "    add jordi --email you@example.org" >&2
  exit 1
fi

exec "$bin" serve --data "$data" --capture
