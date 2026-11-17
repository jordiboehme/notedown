#!/usr/bin/env bash
# Fixture test of scripts/ci/permissions-fit.sh: small workflow folders
# written to a temp dir, one per case, then the real .github/workflows.
#
# Usage: scripts/ci/test-permissions-fit.sh   (needs python3 with PyYAML, or yq)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1" >&2; fails=$((fails + 1)); }

# case_dir <name>: a fresh folder for one case, printed.
case_dir() {
  mkdir -p "$tmp/$1"
  printf '%s\n' "$tmp/$1"
}

# A called release.yml. $1 is the extra job permissions block (YAML lines
# at job indent), empty for none.
release_yml() {
  cat <<EOF
name: Release
on:
  workflow_call: {}
permissions:
  contents: read
jobs:
  notes:
    runs-on: ubuntu-latest
    steps:
      - run: true
  publish:
    runs-on: ubuntu-latest
$1
    steps:
      - run: true
EOF
}

# A caller with job permissions $1 (YAML lines at job indent, may be empty)
# and top-level permissions $2 (YAML lines, may be empty).
caller_yml() {
  cat <<EOF
name: CI
on:
  push: {}
$2
jobs:
  release:
$1
    uses: ./.github/workflows/release.yml
EOF
}

grant='    permissions:
      contents: write
      packages: write
      id-token: write
      attestations: write'
publish_asks='    permissions:
      contents: write
      id-token: write
      attestations: write'

expect() {
  local name="$1" dir="$2" want="$3" text="${4:-}" rc=0 out
  out="$(bash "$here/permissions-fit.sh" "$dir" 2>&1)" || rc=$?
  if [ "$rc" != "$want" ]; then
    fail "$name: exit $rc, want $want; output: $out"
  elif [ -n "$text" ] && ! grep -qF -- "$text" <<<"$out"; then
    fail "$name: the output does not say '$text'; output: $out"
  else
    pass "$name"
  fi
}

d="$(case_dir fits)"
release_yml "$publish_asks" >"$d/release.yml"
caller_yml "$grant" "" >"$d/ci.yml"
expect "a call inside the grant passes" "$d" 0 "ci.yml job release -> release.yml"

d="$(case_dir actions)"
release_yml '    permissions:
      contents: read
      actions: read' >"$d/release.yml"
caller_yml "$grant" "" >"$d/ci.yml"
expect "release.yml asking for actions fails" "$d" 1 "release.yml asks for actions: read, but ci.yml job release grants none"

d="$(case_dir read-grant)"
release_yml "$publish_asks" >"$d/release.yml"
caller_yml '    permissions:
      contents: read
      id-token: write
      attestations: write' "" >"$d/ci.yml"
expect "write asked against a read grant fails" "$d" 1 "release.yml asks for contents: write, but ci.yml job release grants read"

d="$(case_dir write-all)"
release_yml "$publish_asks" >"$d/release.yml"
caller_yml '    permissions: write-all' "" >"$d/ci.yml"
expect "a write-all grant covers every scope" "$d" 0

d="$(case_dir read-all-ask)"
release_yml '    permissions: read-all' >"$d/release.yml"
caller_yml "$grant" "" >"$d/ci.yml"
expect "a read-all job asks for actions too and fails" "$d" 1 "release.yml asks for actions: read"

d="$(case_dir top-level-grant)"
release_yml "$publish_asks" >"$d/release.yml"
caller_yml "" 'permissions:
  contents: read' >"$d/ci.yml"
expect "a caller job without a block falls back to the top level" "$d" 1 "release.yml asks for contents: write, but ci.yml job release grants read"

d="$(case_dir no-block)"
release_yml "" >"$d/release.yml"
caller_yml "" "" >"$d/ci.yml"
expect "a caller with no permissions block at all fails" "$d" 1 "without a permissions block"

d="$(case_dir no-caller)"
release_yml "" >"$d/release.yml"
expect "a release.yml nobody calls fails" "$d" 1 "no workflow calls release.yml"

d="$(case_dir no-top)"
release_yml "" | sed '/^permissions:/,/^  contents: read/d' >"$d/release.yml"
caller_yml "$grant" "" >"$d/ci.yml"
expect "a called workflow without top-level permissions fails" "$d" 1 "has no top-level permissions block"

expect "the repository's own workflows fit" "$here/../../.github/workflows" 0

if [ "$fails" -gt 0 ]; then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "all permissions-fit checks passed"
