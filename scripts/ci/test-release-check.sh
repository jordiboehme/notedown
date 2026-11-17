#!/usr/bin/env bash
# Fixture test of scripts/release-check.sh: a temporary repository with a
# bare `origin`, a copy of the script and a Cargo.toml at 1.2.3. Checks the
# notes-as-switch rule of the default mode (no notes: release=false; notes
# with an H1 and no tag: release=true; notes without an H1: exit 1; the tag
# on the remote: release=false; a remote that does not answer: exit 1) and
# the tag-commit contract (the peeled commit, nothing and exit 0 for a
# missing tag, exit 1 on a lookup error).
#
# Usage: scripts/ci/test-release-check.sh   (needs git)
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# No global or system git settings (identity, signing, hooks) reach the
# fixture.
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
unset RELEASE_CHECK_REMOTE GITHUB_OUTPUT
g() {
  git -c user.name=test -c user.email=test@example.invalid \
    -c commit.gpgsign=false -c tag.gpgsign=false -c init.defaultBranch=main "$@"
}

fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1" >&2; fails=$((fails + 1)); }

# A fresh fixture: work tree $tmp/<name>/work, its origin $tmp/<name>/origin.git.
fixture() {
  local dir="$tmp/$1"
  mkdir -p "$dir/work/scripts"
  g init -q --bare "$dir/origin.git"
  g init -q "$dir/work"
  cp "$repo/scripts/release-check.sh" "$dir/work/scripts/"
  cat >"$dir/work/Cargo.toml" <<'TOML'
[workspace]
members = []

[workspace.package]
version = "1.2.3"
TOML
  g -C "$dir/work" add -A
  g -C "$dir/work" commit -q -m init
  g -C "$dir/work" remote add origin "$dir/origin.git"
  g -C "$dir/work" push -q origin main
  echo "$dir/work"
}

# Runs the default mode; sets rc, out and outputs (the $GITHUB_OUTPUT text).
check() {
  local work="$1"
  : >"$tmp/output"
  rc=0
  out="$(GITHUB_OUTPUT="$tmp/output" bash "$work/scripts/release-check.sh" 2>&1)" || rc=$?
  outputs="$(cat "$tmp/output")"
}

expect_outputs() {
  local name="$1" want="$2"
  if [ "$rc" = 0 ] && [ "$outputs" = "$want" ]; then
    pass "$name"
  else
    fail "$name: exit $rc, outputs '$outputs', want '$want'; output: $out"
  fi
}

# No notes file: no release, green, and no tag lookup needed.
w="$(fixture no-notes)"
check "$w"
expect_outputs "no notes file answers release=false" "$(printf 'tag=v1.2.3\nrelease=false')"

# Notes with an H1, no tag anywhere: release.
w="$(fixture notes-h1)"
mkdir -p "$w/release-notes"
printf '# A motto\n\nText.\n' >"$w/release-notes/v1.2.3.md"
check "$w"
expect_outputs "notes with an H1 and no tag answer release=true" "$(printf 'tag=v1.2.3\nrelease=true')"

# Notes without an H1: fails.
w="$(fixture notes-no-h1)"
mkdir -p "$w/release-notes"
printf 'A motto without a heading\n' >"$w/release-notes/v1.2.3.md"
check "$w"
if [ "$rc" = 1 ] && grep -q "H1" <<<"$out"; then
  pass "notes without an H1 fail"
else
  fail "notes without an H1: exit $rc, want 1; output: $out"
fi

# An empty notes file: fails as well (it has no H1).
w="$(fixture notes-empty)"
mkdir -p "$w/release-notes"
: >"$w/release-notes/v1.2.3.md"
check "$w"
if [ "$rc" = 1 ]; then
  pass "an empty notes file fails"
else
  fail "an empty notes file: exit $rc, want 1; output: $out"
fi

# The tag exists on the remote only (a CI checkout has no tags): no release.
w="$(fixture tag-remote)"
mkdir -p "$w/release-notes"
printf '# A motto\n' >"$w/release-notes/v1.2.3.md"
g -C "$w" tag -a v1.2.3 -m v1.2.3
g -C "$w" push -q origin v1.2.3
g -C "$w" tag -d v1.2.3 >/dev/null
check "$w"
expect_outputs "a tag on the remote answers release=false" "$(printf 'tag=v1.2.3\nrelease=false')"

# The tag exists here: no release.
w="$(fixture tag-local)"
mkdir -p "$w/release-notes"
printf '# A motto\n' >"$w/release-notes/v1.2.3.md"
g -C "$w" tag v1.2.3
check "$w"
expect_outputs "a local tag answers release=false" "$(printf 'tag=v1.2.3\nrelease=false')"

# Notes in place and a remote that does not answer: fails, never "no tag".
w="$(fixture unreachable)"
mkdir -p "$w/release-notes"
printf '# A motto\n' >"$w/release-notes/v1.2.3.md"
rc=0
out="$(RELEASE_CHECK_REMOTE="$tmp/nowhere.git" bash "$w/scripts/release-check.sh" 2>&1)" || rc=$?
if [ "$rc" = 1 ]; then
  pass "an unreachable remote fails the default mode"
else
  fail "unreachable remote, default mode: exit $rc, want 1; output: $out"
fi

# tag-commit: an annotated tag prints the commit, not the tag object.
w="$(fixture tag-commit)"
g -C "$w" tag -a v9.9.9 -m v9.9.9
g -C "$w" push -q origin v9.9.9
want="$(g -C "$w" rev-parse HEAD)"
rc=0
got="$(bash "$w/scripts/release-check.sh" tag-commit v9.9.9)" || rc=$?
if [ "$rc" = 0 ] && [ "$got" = "$want" ]; then
  pass "tag-commit prints the peeled commit of an annotated tag"
else
  fail "tag-commit annotated: exit $rc, got '$got', want '$want'"
fi

# tag-commit: a lightweight tag prints its commit.
g -C "$w" tag v9.9.8
g -C "$w" push -q origin v9.9.8
rc=0
got="$(bash "$w/scripts/release-check.sh" tag-commit v9.9.8)" || rc=$?
if [ "$rc" = 0 ] && [ "$got" = "$want" ]; then
  pass "tag-commit prints the commit of a lightweight tag"
else
  fail "tag-commit lightweight: exit $rc, got '$got', want '$want'"
fi

# tag-commit: a missing tag prints nothing and exits 0.
rc=0
got="$(bash "$w/scripts/release-check.sh" tag-commit v0.0.1)" || rc=$?
if [ "$rc" = 0 ] && [ -z "$got" ]; then
  pass "tag-commit of a missing tag prints nothing and exits 0"
else
  fail "tag-commit missing: exit $rc, got '$got', want '' and 0"
fi

# tag-commit: a remote that does not answer exits 1.
rc=0
got="$(RELEASE_CHECK_REMOTE="$tmp/nowhere.git" bash "$w/scripts/release-check.sh" tag-commit v9.9.9 2>/dev/null)" || rc=$?
if [ "$rc" = 1 ] && [ -z "$got" ]; then
  pass "tag-commit with an unreachable remote exits 1"
else
  fail "tag-commit unreachable: exit $rc, got '$got', want 1"
fi

if [ "$fails" -gt 0 ]; then
  echo "$fails test(s) failed" >&2
  exit 1
fi
echo "release-check: all tests passed"
