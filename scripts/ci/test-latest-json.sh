#!/usr/bin/env bash
# Fixture test of scripts/ci/latest-json.sh: the updater file from a
# fixture .sig and notes file, and every empty or broken field refused.
#
# Usage: scripts/ci/test-latest-json.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1" >&2; fails=$((fails + 1)); }

# A .sig as Tauri writes it: one line of base64, no newline at the end.
sig='dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVSUz0='
printf '%s' "$sig" >"$tmp/app.sig"
cat >"$tmp/notes.md" <<'EOF'
# Notes in, Markdown out

The first release of Notedown.

## New & Noteworthy

- Everything.

EOF

# run <version> <sig> <notes>: exit code in $rc, stdout in $out, stderr in $err
run() {
  rc=0
  out="$(PUB_DATE="${PUB_DATE-2026-11-17T12:00:00Z}" bash "$here/latest-json.sh" "$@" 2>"$tmp/err")" || rc=$?
  err="$(cat "$tmp/err")"
}

check() {
  local name="$1" filter="$2"
  if jq -e "$filter" >/dev/null 2>&1 <<<"$out"; then pass "$name"; else fail "$name: $filter"; printf '%s\n' "$out" >&2; fi
}

run 0.1.0 "$tmp/app.sig" "$tmp/notes.md"
if [ "$rc" = 0 ]; then pass "renders"; else fail "renders (exit $rc: $err)"; fi
check "version" '.version == "0.1.0"'
check "pub_date" '.pub_date == "2026-11-17T12:00:00Z"'
check "notes are the body without the H1" '.notes == "The first release of Notedown.\n\n## New & Noteworthy\n\n- Everything."'
check "signature is the .sig" ".platforms[\"darwin-aarch64\"].signature == \"$sig\""
check "url" '.platforms["darwin-aarch64"].url == "https://github.com/jordiboehme/notedown/releases/download/v0.1.0/notedown-app-v0.1.0-macos.app.tar.gz"'
check "only darwin-aarch64" '(.platforms | keys) == ["darwin-aarch64"]'
check "four keys" '(keys | sort) == ["notes", "platforms", "pub_date", "version"]'

# A .sig with a newline at the end reads the same.
printf '%s\n' "$sig" >"$tmp/nl.sig"
run 0.1.0 "$tmp/nl.sig" "$tmp/notes.md"
check "a .sig with a newline" ".platforms[\"darwin-aarch64\"].signature == \"$sig\""

# The dry run's version.
run 0.0.0-dev "$tmp/app.sig" "$tmp/notes.md"
check "a dry run version" '.version == "0.0.0-dev" and (.platforms["darwin-aarch64"].url | endswith("/v0.0.0-dev/notedown-app-v0.0.0-dev-macos.app.tar.gz"))'

PUB_DATE='' run 0.1.0 "$tmp/app.sig" "$tmp/notes.md"
check "pub_date from the clock" '.pub_date | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")'

# expect_fail <name> <exit> <text> <args...>
expect_fail() {
  local name="$1" want="$2" text="$3"
  shift 3
  run "$@"
  if [ "$rc" != "$want" ]; then fail "$name: exit $rc, want $want"; return; fi
  if [ -n "$out" ]; then fail "$name: printed a file anyway"; return; fi
  if ! grep -qF -- "$text" <<<"$err"; then fail "$name: stderr lacks '$text': $err"; return; fi
  pass "$name"
}

: >"$tmp/empty.sig"
expect_fail "an empty signature is refused" 1 "is empty" 0.1.0 "$tmp/empty.sig" "$tmp/notes.md"
printf '\n\n' >"$tmp/blank.sig"
expect_fail "a blank signature is refused" 1 "is empty" 0.1.0 "$tmp/blank.sig" "$tmp/notes.md"
printf 'abc def' >"$tmp/space.sig"
expect_fail "a signature with a space is refused" 1 "is not one line of base64" 0.1.0 "$tmp/space.sig" "$tmp/notes.md"
expect_fail "a missing signature file is refused" 2 "cannot read" 0.1.0 "$tmp/nope.sig" "$tmp/notes.md"

printf '# Only a motto\n\n' >"$tmp/h1-only.md"
expect_fail "notes with only the H1 are refused" 1 "has no text after its H1" 0.1.0 "$tmp/app.sig" "$tmp/h1-only.md"
: >"$tmp/empty.md"
expect_fail "empty notes are refused" 1 "has no text after its H1" 0.1.0 "$tmp/app.sig" "$tmp/empty.md"

expect_fail "an empty version is refused" 2 "the version must look like" "" "$tmp/app.sig" "$tmp/notes.md"
expect_fail "a version with a v is refused" 2 "the version must look like" v0.1.0 "$tmp/app.sig" "$tmp/notes.md"
PUB_DATE=yesterday expect_fail "a bad pub_date is refused" 2 "PUB_DATE must be" 0.1.0 "$tmp/app.sig" "$tmp/notes.md"
expect_fail "two arguments are a usage error" 2 "usage:" 0.1.0 "$tmp/app.sig"

if [ "$fails" -ne 0 ]; then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "all latest.json checks passed"
