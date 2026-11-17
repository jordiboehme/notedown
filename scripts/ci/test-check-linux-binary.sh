#!/usr/bin/env bash
# Fixture test of scripts/ci/check-linux-binary.sh: a stub `file` on PATH
# prints the description of each case, a stub binary prints its --version.
#
# Usage: scripts/ci/test-check-linux-binary.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
version="$(bash "$here/workspace-version.sh")"

fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1" >&2; fails=$((fails + 1)); }

mkdir -p "$tmp/bin"
cat >"$tmp/bin/file" <<'STUB'
#!/bin/sh
printf '%s\n' "$STUB_FILE"
STUB
chmod +x "$tmp/bin/file"

# expect <name> <file output> <--version output> <exit> [text]
expect() {
  local name="$1" desc="$2" says="$3" want="$4" text="${5:-}" rc=0 out
  printf '#!/bin/sh\necho "%s"\n' "$says" >"$tmp/notedown"
  chmod +x "$tmp/notedown"
  out="$(PATH="$tmp/bin:$PATH" STUB_FILE="$desc" bash "$here/check-linux-binary.sh" "$tmp/notedown" 2>&1)" || rc=$?
  if [ "$rc" != "$want" ]; then
    fail "$name: exit $rc, want $want; output: $out"
  elif [ -n "$text" ] && ! grep -qF -- "$text" <<<"$out"; then
    fail "$name: the output does not say '$text'; output: $out"
  else
    pass "$name"
  fi
}

pie='ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV), static-pie linked, stripped'
old='ELF 64-bit LSB executable, ARM aarch64, version 1 (SYSV), statically linked, stripped'
dyn='ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV), dynamically linked, interpreter /lib64/ld-linux-x86-64.so.2'

expect "a static-pie binary with the right version passes" "$pie" "notedown $version" 0
expect "an older 'statically linked' wording passes" "$old" "notedown $version" 0
expect "a dynamic binary fails" "$dyn" "notedown $version" 1 "is not a static binary"
expect "a wrong version fails" "$pie" "notedown 0.0.0-dev" 1 "want 'notedown $version'"

rc=0
out="$(bash "$here/check-linux-binary.sh" "$tmp/missing" 2>&1)" || rc=$?
if [ "$rc" = 1 ] && grep -qF 'does not exist' <<<"$out"; then
  pass "a missing binary fails"
else
  fail "a missing binary fails: exit $rc; output: $out"
fi

if [ "$fails" -gt 0 ]; then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "all check-linux-binary checks passed"
