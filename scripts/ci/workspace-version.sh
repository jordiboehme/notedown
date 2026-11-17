#!/usr/bin/env bash
# Usage: workspace-version.sh [root]
#
# Prints the release version: `version` in [workspace.package] of the root
# Cargo.toml. The one place that reads it, for scripts/release-check.sh,
# scripts/ci/version-pins.sh and the CI jobs. root defaults to the repository
# this script is in. Exit 1 with an ::error:: line when there is none.
set -euo pipefail

root="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"

# `version = "x.y.z"` inside [workspace.package], up to the next section.
version="$(awk '
  /^\[/ { inside = ($0 ~ /^\[workspace\.package\][[:space:]]*$/) ; next }
  inside && /^version[[:space:]]*=/ {
    sub(/^version[[:space:]]*=[[:space:]]*"/, ""); sub(/".*$/, ""); print; exit
  }' "$root/Cargo.toml")"
if [ -z "$version" ]; then
  echo "::error file=Cargo.toml::Cargo.toml has no version in [workspace.package]." >&2
  exit 1
fi
printf '%s\n' "$version"
