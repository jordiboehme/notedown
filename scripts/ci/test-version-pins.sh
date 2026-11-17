#!/usr/bin/env bash
# Fixture test of scripts/ci/version-pins.sh: copies the files the script
# reads into a temporary root, checks that this repository passes, then moves
# one pin at a time away from the workspace version and checks that the
# script fails and names the file.
#
# Usage: scripts/ci/test-version-pins.sh   (needs jq and perl)
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1" >&2; fails=$((fails + 1)); }

# A fresh copy of the pinned files in $tmp/<name>.
fixture() {
  local root="$tmp/$1" c
  mkdir -p "$root/app/src-tauri"
  cp "$repo/Cargo.toml" "$repo/Cargo.lock" "$root/"
  for c in "$repo"/crates/*/; do
    mkdir -p "$root/crates/$(basename "$c")"
    cp "$c/Cargo.toml" "$root/crates/$(basename "$c")/"
  done
  cp "$repo/app/package.json" "$repo/app/package-lock.json" "$root/app/"
  cp "$repo/app/src-tauri/Cargo.toml" "$repo/app/src-tauri/Cargo.lock" "$root/app/src-tauri/"
  echo "$root"
}

run() {
  rc=0
  out="$(bash "$repo/scripts/ci/version-pins.sh" "$1" 2>&1)" || rc=$?
}

# Expects a failure that names the file.
expect_fail() {
  local name="$1" file="$2"
  if [ "$rc" = 1 ] && grep -qF "file=$file::" <<<"$out"; then
    pass "$name"
  else
    fail "$name: exit $rc, want 1 naming $file; output: $out"
  fi
}

run "$repo"
if [ "$rc" = 0 ]; then
  pass "this repository passes"
else
  fail "this repository: exit $rc; output: $out"
fi

root="$(fixture baseline)"
run "$root"
if [ "$rc" = 0 ]; then
  pass "an untouched copy passes"
else
  fail "untouched copy: exit $rc; output: $out"
fi

ver="$(bash "$repo/scripts/ci/workspace-version.sh" "$repo")"
other="9.8.7"

deps="$(sed -nE 's/^([A-Za-z0-9_-]+)[[:space:]]*=[[:space:]]*\{.*path[[:space:]]*=[[:space:]]*"crates\/.*/\1/p' "$repo/Cargo.toml")"
if [ "$(grep -c . <<<"$deps")" -lt 5 ]; then
  fail "expected at least five path dependencies in Cargo.toml, found: $deps"
fi
for dep in $deps; do
  root="$(fixture "path-dep-$dep")"
  perl -pi -e 's/^(\Q'"$dep"'\E = \{.*\bversion = )"[^"]*"/$1"'"$other"'"/' "$root/Cargo.toml"
  if cmp -s "$repo/Cargo.toml" "$root/Cargo.toml"; then
    fail "the mutation of the path dependency $dep changed nothing"
    continue
  fi
  run "$root"
  expect_fail "the path dependency $dep off the version fails" "Cargo.toml"
done

root="$(fixture crate-own-version)"
perl -pi -e 's/^version\.workspace = true$/version = "'"$other"'"/' "$root/crates/notedown-ocr/Cargo.toml"
run "$root"
expect_fail "a crate with its own version fails" "crates/notedown-ocr/Cargo.toml"

root="$(fixture app-crate)"
# The version inside [package] only.
perl -pi -e '$in = /^\[package\]\s*$/ if /^\[/; s/^version = "[^"]*"$/version = "'"$other"'"/ if $in' "$root/app/src-tauri/Cargo.toml"
run "$root"
expect_fail "the app crate off the version fails" "app/src-tauri/Cargo.toml"

root="$(fixture package-json)"
jq --arg v "$other" '.version = $v' "$root/app/package.json" >"$tmp/x.json"
mv "$tmp/x.json" "$root/app/package.json"
run "$root"
expect_fail "app/package.json off the version fails" "app/package.json"

root="$(fixture lock-top)"
jq --arg v "$other" '.version = $v' "$root/app/package-lock.json" >"$tmp/x.json"
mv "$tmp/x.json" "$root/app/package-lock.json"
run "$root"
expect_fail "the top version of app/package-lock.json off the version fails" "app/package-lock.json"

root="$(fixture lock-root-package)"
jq --arg v "$other" '.packages[""].version = $v' "$root/app/package-lock.json" >"$tmp/x.json"
mv "$tmp/x.json" "$root/app/package-lock.json"
run "$root"
expect_fail "the root package of app/package-lock.json off the version fails" "app/package-lock.json"

for crate in notedown notedown-ocr notedown-pipeline; do
  root="$(fixture "cargo-lock-$crate")"
  perl -0pi -e 's/(\nname = "'"$crate"'"\nversion = )"[^"]*"/$1"'"$other"'"/' "$root/Cargo.lock"
  run "$root"
  expect_fail "$crate in Cargo.lock off the version fails" "Cargo.lock"
done

root="$(fixture cargo-lock-missing)"
perl -0pi -e 's/\nname = "notedown-convert"\n/\nname = "notedown-convert-gone"\n/' "$root/Cargo.lock"
run "$root"
expect_fail "a crate missing from Cargo.lock fails" "Cargo.lock"

root="$(fixture app-lock)"
perl -0pi -e 's/(\nname = "notedown-app"\nversion = )"[^"]*"/$1"'"$other"'"/' "$root/app/src-tauri/Cargo.lock"
run "$root"
expect_fail "the app crate in app/src-tauri/Cargo.lock off the version fails" "app/src-tauri/Cargo.lock"

# Moving the workspace version alone leaves every pin behind.
root="$(fixture workspace-only)"
perl -pi -e 's/^version = "'"$ver"'"$/version = "'"$other"'"/' "$root/Cargo.toml"
run "$root"
expect_fail "a workspace version bump without the pins fails" "app/package.json"

if [ "$fails" -gt 0 ]; then
  echo "$fails test(s) failed" >&2
  exit 1
fi
echo "version-pins: all tests passed"
