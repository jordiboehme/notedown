#!/usr/bin/env bash
# Fixture test of scripts/ci/homebrew.sh: both modes render from a fixture
# SHA256SUMS, a missing, doubled or bad sum fails and prints nothing, and
# the cask never trashes the data folder. `ruby -c` checks the rendered
# files when ruby is installed (it is on the CI runner).
#
# Usage: scripts/ci/test-homebrew.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1" >&2; fails=$((fails + 1)); }

cli_sum=1111111111111111111111111111111111111111111111111111111111111111
app_sum=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
other=2222222222222222222222222222222222222222222222222222222222222222

# sums <version>: a SHA256SUMS as collect writes it, with the decoys a
# loose match would pick up.
sums() {
  local v="$1"
  printf '%s  %s\n' \
    "$other" "notedown-v$v-linux-amd64.tar.gz" \
    "$cli_sum" "notedown-v$v-macos.tar.gz" \
    "$other" "notedown-v$v-macos.cdx.json" \
    "$app_sum" "notedown-app-v$v-macos.dmg" \
    "$other" "notedown-app-v$v-macos.app.tar.gz" \
    "$other" "latest.json"
}

# run <mode> <version> <sums file>: exit code in $rc, stdout in $out,
# stderr in $err
run() {
  rc=0
  out="$(bash "$here/homebrew.sh" "$1" "$2" "$3" 2>"$tmp/err")" || rc=$?
  err="$(cat "$tmp/err")"
}

ruby_check() {
  local name="$1" file="$2"
  if command -v ruby >/dev/null 2>&1; then
    if ruby -c "$file" >/dev/null 2>&1; then pass "$name: ruby -c"; else fail "$name: ruby -c"; fi
  else
    echo "skip $name: ruby -c (no ruby)"
  fi
}

has() {
  local name="$1" text="$2"
  if grep -qF -- "$text" <<<"$out"; then pass "$name"; else fail "$name: lacks '$text'"; printf '%s\n' "$out" >&2; fi
}

sums 0.1.0 >"$tmp/SHA256SUMS"

run formula 0.1.0 "$tmp/SHA256SUMS"
if [ "$rc" = 0 ]; then pass "formula renders"; else fail "formula renders (exit $rc: $err)"; fi
printf '%s\n' "$out" >"$tmp/notedown.rb"
has "formula class" 'class Notedown < Formula'
has "formula url" 'url "https://github.com/jordiboehme/notedown/releases/download/v0.1.0/notedown-v0.1.0-macos.tar.gz"'
if grep -q '^  version ' <<<"$out"; then fail "formula has no version line (the url names it)"; else pass "formula has no version line (the url names it)"; fi
has "formula sum" "sha256 \"$cli_sum\""
has "formula arm64 only" 'depends_on arch: :arm64'
has "formula macOS 13" 'depends_on macos: :ventura'
has "formula installs the binary" 'bin.install "notedown"'
has "formula test checks the version" 'assert_match version.to_s, shell_output("#{bin}/notedown --version")'
ruby_check formula "$tmp/notedown.rb"

run cask 0.1.0 "$tmp/SHA256SUMS"
if [ "$rc" = 0 ]; then pass "cask renders"; else fail "cask renders (exit $rc: $err)"; fi
printf '%s\n' "$out" >"$tmp/notedown-app.rb"
has "cask token" 'cask "notedown-app" do'
has "cask version" 'version "0.1.0"'
has "cask sum" "sha256 \"$app_sum\""
has "cask url" 'url "https://github.com/jordiboehme/notedown/releases/download/v#{version}/notedown-app-v#{version}-macos.dmg"'
has "cask app" 'app "Notedown.app"'
has "cask arm64 only" 'depends_on arch: :arm64'
has "cask macOS 13" 'depends_on macos: :ventura'
has "cask auto updates" 'auto_updates true'
has "cask stops the autostart agent" 'launchctl: "Notedown"'
has "cask quits the app" 'quit:      "io.github.jordiboehme.notedown"'
has "cask zaps the autostart plist" '"~/Library/LaunchAgents/Notedown.plist"'
has "cask zaps Application Support" '"~/Library/Application Support/io.github.jordiboehme.notedown"'
has "cask zaps HTTPStorages" '"~/Library/HTTPStorages/io.github.jordiboehme.notedown"'
if grep -qF notedown-data <<<"$out"; then fail "the cask never names notedown-data"; else pass "the cask never names notedown-data"; fi
ruby_check cask "$tmp/notedown-app.rb"

# The dry run's version.
sums 0.0.0-dev >"$tmp/dev"
run cask 0.0.0-dev "$tmp/dev"
if [ "$rc" = 0 ] && grep -qF 'version "0.0.0-dev"' <<<"$out"; then pass "a dry run version renders"; else fail "a dry run version renders (exit $rc: $err)"; fi

# expect_fail <name> <mode> <version> <sums file> <exit> <text>
expect_fail() {
  run "$2" "$3" "$4"
  if [ "$rc" != "$5" ]; then fail "$1: exit $rc, want $5"; return; fi
  if [ -n "$out" ]; then fail "$1: printed a file anyway"; return; fi
  if ! grep -qF -- "$6" <<<"$err"; then fail "$1: stderr lacks '$6': $err"; return; fi
  pass "$1"
}

grep -v ' notedown-v0.1.0-macos.tar.gz$' "$tmp/SHA256SUMS" >"$tmp/no-cli"
expect_fail "formula: a missing line fails" formula 0.1.0 "$tmp/no-cli" 1 "has no sum for notedown-v0.1.0-macos.tar.gz"
grep -v ' notedown-app-v0.1.0-macos.dmg$' "$tmp/SHA256SUMS" >"$tmp/no-dmg"
expect_fail "cask: a missing line fails" cask 0.1.0 "$tmp/no-dmg" 1 "has no sum for notedown-app-v0.1.0-macos.dmg"

{ cat "$tmp/SHA256SUMS"; printf '%s  %s\n' "$other" notedown-app-v0.1.0-macos.dmg; } >"$tmp/twice"
expect_fail "cask: a second line fails" cask 0.1.0 "$tmp/twice" 1 "has 2 lines for notedown-app-v0.1.0-macos.dmg"

sed "s/^$cli_sum /${cli_sum:0:63} /" "$tmp/SHA256SUMS" >"$tmp/short"
expect_fail "formula: a 63 hex sum fails" formula 0.1.0 "$tmp/short" 1 "is not 64 lowercase hex characters"
sed "s/^$app_sum /$(tr a A <<<"$app_sum") /" "$tmp/SHA256SUMS" >"$tmp/upper"
expect_fail "cask: an upper case sum fails" cask 0.1.0 "$tmp/upper" 1 "is not 64 lowercase hex characters"
sed "s/^$app_sum /${app_sum:0:63}g /" "$tmp/SHA256SUMS" >"$tmp/nonhex"
expect_fail "cask: a non-hex sum fails" cask 0.1.0 "$tmp/nonhex" 1 "is not 64 lowercase hex characters"

# The binary mode marker of sha256sum is accepted.
sed 's/  notedown-app-/ *notedown-app-/' "$tmp/SHA256SUMS" >"$tmp/binary"
run cask 0.1.0 "$tmp/binary"
if [ "$rc" = 0 ]; then pass "a ' *' line is read"; else fail "a ' *' line is read (exit $rc: $err)"; fi

expect_fail "the sums of another version fail" formula 0.2.0 "$tmp/SHA256SUMS" 1 "has no sum for notedown-v0.2.0-macos.tar.gz"
expect_fail "a version with a v is refused" formula v0.1.0 "$tmp/SHA256SUMS" 2 "the version must look like"
expect_fail "a version with a quote is refused" cask '0.1.0"' "$tmp/SHA256SUMS" 2 "the version must look like"
expect_fail "an unknown mode is refused" bottle 0.1.0 "$tmp/SHA256SUMS" 2 "usage:"
expect_fail "a missing sums file is refused" cask 0.1.0 "$tmp/nope" 2 "cannot read"

if [ "$fails" -ne 0 ]; then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "all homebrew checks passed"
