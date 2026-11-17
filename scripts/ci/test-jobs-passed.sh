#!/usr/bin/env bash
# Fixture test of scripts/ci/jobs-passed.sh: jobs JSON as
# `gh api repos/R/actions/runs/N/jobs?filter=latest --paginate` prints it, in
# scripts/ci/fixtures/jobs-*.json.
#
# Usage: scripts/ci/test-jobs-passed.sh   (needs jq)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fx="$here/fixtures"

fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1" >&2; fails=$((fails + 1)); }

run() {
  rc=0
  out="$(bash "$here/jobs-passed.sh" <"$fx/$1" 2>&1)" || rc=$?
}

expect() {
  local name="$1" fixture="$2" want="$3" names="${4:-}"
  run "$fixture"
  if [ "$rc" != "$want" ]; then
    fail "$name: exit $rc, want $want; output: $out"
  elif [ -n "$names" ] && ! grep -qF "$names" <<<"$out"; then
    fail "$name: the output does not name '$names'; output: $out"
  else
    pass "$name"
  fi
}

expect "all success or skipped passes" jobs-all-success.json 0
expect "a completed cancelled leg fails" jobs-cancelled-leg.json 1 "musl (arm64): cancelled"
expect "a queued release-check with conclusion null passes" jobs-queued-release-check.json 0
expect "a failure on the second page fails" jobs-two-pages.json 1 "clippy (macos-latest): failure"
expect "two green pages pass" jobs-two-pages-green.json 0
expect "a timed out leg fails" jobs-timed-out.json 1 "test (macos-latest): timed_out"
expect "no completed job at all fails" jobs-none.json 1
expect "broken JSON fails" jobs-broken.json 1

if [ "$fails" -gt 0 ]; then
  echo "$fails test(s) failed" >&2
  exit 1
fi
echo "jobs-passed: all tests passed"
