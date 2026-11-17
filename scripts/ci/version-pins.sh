#!/usr/bin/env bash
# Usage: version-pins.sh [root]
#
# Checks that every file which names the release version agrees with it, so
# a release pull request fails here and not at the tag. The version is
# `version` in [workspace.package] of the root Cargo.toml. It must also be:
#
# - the `version` of every internal path dependency in the root Cargo.toml
#   ([workspace.dependencies] `{ path = "crates/...", version = "..." }`)
# - taken by every crate in crates/ with `version.workspace = true`
# - the version of each of those crates in Cargo.lock
# - `version` of the Mac app crate in app/src-tauri/Cargo.toml (its own
#   workspace, not linked to the root) and of notedown-app in
#   app/src-tauri/Cargo.lock. The app bundle and its updater take it
# - `version` in app/package.json, and both places in app/package-lock.json
#   (the top `version` and `packages[""].version`)
#
# Reads the files only: no cargo, no network. root defaults to the repository
# this script is in. Exit 1 with an ::error:: line for each problem. Needs jq.
set -euo pipefail

root="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"

version="$(bash "$(dirname "${BASH_SOURCE[0]}")/workspace-version.sh" "$root")"

bad=0
problem() {
  echo "::error file=$1::$2" >&2
  bad=1
}

# Internal path dependencies of the root Cargo.toml.
deps="$(grep -E '^[A-Za-z0-9_-]+[[:space:]]*=[[:space:]]*\{.*path[[:space:]]*=[[:space:]]*"crates/' "$root/Cargo.toml" || true)"
if [ -z "$deps" ]; then
  problem Cargo.toml "Cargo.toml lists no internal path dependency (path = \"crates/...\"), so there is nothing to check against $version."
fi
while IFS= read -r line; do
  [ -n "$line" ] || continue
  dep="${line%%[[:space:]=]*}"
  pinned="$(sed -nE 's/.*version[[:space:]]*=[[:space:]]*"([^"]*)".*/\1/p' <<<"$line")"
  if [ "$pinned" != "$version" ]; then
    problem Cargo.toml "the path dependency $dep in Cargo.toml has version '$pinned' but the workspace version is $version."
  fi
done <<<"$deps"

# The crates in crates/ and their entries in Cargo.lock.
lock_version() {
  # The version line right after `name = "<crate>"`.
  awk -v n="name = \"$2\"" '
    $0 == n { getline; if ($0 ~ /^version = "/) { sub(/^version = "/, ""); sub(/"$/, ""); print }; exit }
  ' "$1"
}
crates=0
for manifest in "$root"/crates/*/Cargo.toml; do
  [ -f "$manifest" ] || continue
  rel="${manifest#"$root"/}"
  name="$(sed -nE 's/^name[[:space:]]*=[[:space:]]*"([^"]*)".*/\1/p' "$manifest" | head -1)"
  if [ -z "$name" ]; then
    problem "$rel" "$rel has no package name."
    continue
  fi
  crates=$((crates + 1))
  if ! grep -qE '^version\.workspace[[:space:]]*=[[:space:]]*true' "$manifest"; then
    problem "$rel" "$rel must take its version from the workspace (version.workspace = true), so the binary, the tag and the release notes name the same version $version."
  fi
  locked="$(lock_version "$root/Cargo.lock" "$name")"
  if [ "$locked" != "$version" ]; then
    problem Cargo.lock "Cargo.lock has $name at version '$locked' but the workspace version is $version. Run cargo update --workspace."
  fi
done
if [ "$crates" = 0 ]; then
  problem Cargo.toml "no crate found in crates/, so there is nothing to check against $version."
fi

# The Mac app crate.
app_manifest="$root/app/src-tauri/Cargo.toml"
app_version="$(awk '
  /^\[/ { inside = ($0 ~ /^\[package\][[:space:]]*$/) ; next }
  inside && /^version[[:space:]]*=/ {
    sub(/^version[[:space:]]*=[[:space:]]*"/, ""); sub(/".*$/, ""); print; exit
  }' "$app_manifest")"
if [ "$app_version" != "$version" ]; then
  problem app/src-tauri/Cargo.toml "app/src-tauri/Cargo.toml has version '$app_version' but the workspace version is $version."
fi
app_name="$(awk '
  /^\[/ { inside = ($0 ~ /^\[package\][[:space:]]*$/) ; next }
  inside && /^name[[:space:]]*=/ {
    sub(/^name[[:space:]]*=[[:space:]]*"/, ""); sub(/".*$/, ""); print; exit
  }' "$app_manifest")"
locked="$(lock_version "$root/app/src-tauri/Cargo.lock" "${app_name:-notedown-app}")"
if [ "$locked" != "$version" ]; then
  problem app/src-tauri/Cargo.lock "app/src-tauri/Cargo.lock has ${app_name:-notedown-app} at version '$locked' but the workspace version is $version. Run cargo update --workspace in app/src-tauri."
fi

# The frontend.
pinned="$(jq -r '.version // ""' "$root/app/package.json")"
if [ "$pinned" != "$version" ]; then
  problem app/package.json "app/package.json has version '$pinned' but the workspace version is $version."
fi
top="$(jq -r '.version // ""' "$root/app/package-lock.json")"
inner="$(jq -r '.packages[""].version // ""' "$root/app/package-lock.json")"
if [ "$top" != "$version" ] || [ "$inner" != "$version" ]; then
  problem app/package-lock.json "app/package-lock.json has version '$top' and packages[\"\"] version '$inner' but the workspace version is $version. Run npm install in app."
fi

[ "$bad" = "0" ] || exit 1
echo "version pins: $version (Cargo.toml, $crates crates, Cargo.lock, app/src-tauri, app/package.json, app/package-lock.json)"
