#!/usr/bin/env bash
# Usage: check-linux-binary.sh <binary>
#
# The checks of a Linux musl build of notedown, shared by ci.yml's musl job
# and release.yml's build-linux:
#
# - the binary is static: `file` reports "static-pie linked" (Rust's musl
#   targets link static-pie) or, in older file versions, "statically
#   linked", and names neither "dynamically linked" nor an interpreter;
# - `<binary> --version` prints exactly "notedown <version>", where the
#   version is the workspace version (scripts/ci/workspace-version.sh). A
#   dry run names its files 0.0.0-dev; the binary still says the real one.
#
# Exit 0 when both hold, 1 with an ::error:: line when not, 2 on a usage
# error.
set -euo pipefail

[ "$#" -eq 1 ] || { echo "usage: $(basename "$0") <binary>" >&2; exit 2; }
bin="$1"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ ! -f "$bin" ]; then
  echo "::error::$bin does not exist"
  exit 1
fi
info="$(file -b "$bin")"
echo "file: $info"
if ! grep -qE 'static(ally|-pie) linked' <<<"$info" \
  || grep -qE 'dynamically linked|interpreter' <<<"$info"; then
  echo "::error::$bin is not a static binary"
  exit 1
fi
version="$(bash "$here/workspace-version.sh")"
got="$("$bin" --version)"
echo "--version: $got"
if [ "$got" != "notedown $version" ]; then
  echo "::error::$bin --version printed '$got', want 'notedown $version'"
  exit 1
fi
