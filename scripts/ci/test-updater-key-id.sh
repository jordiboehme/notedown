#!/usr/bin/env bash
# Fixture test of scripts/ci/updater-key-id.sh. The fixtures are minisign
# files built by hand: a comment line, then base64 of the algorithm (2
# bytes), the key id (8 bytes) and the key or signature bytes, all of it
# base64 again as Tauri stores it.
#
# Usage: scripts/ci/test-updater-key-id.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1" >&2; fails=$((fails + 1)); }

# zeros <n>: n zero bytes as printf escapes
zeros() { local i out=""; for ((i = 0; i < $1; i++)); do out="$out\\x00"; done; printf '%s' "$out"; }
# minisign <comment> <alg escapes> <id escapes> <payload length>
minisign() {
  local line2
  # shellcheck disable=SC2059 # the escapes are the format on purpose
  line2="$(printf "$2$3$(zeros "$4")" | base64 | tr -d '\n')"
  printf 'untrusted comment: %s\n%s\n' "$1" "$line2" | base64 | tr -d '\n'
}
id_a='\x01\x02\x03\x04\x05\x06\x07\x08'
id_b='\xda\x34\x9c\x4f\xa0\x47\x41\xf0'

conf() { printf '{"plugins":{"updater":{"pubkey":"%s"}}}\n' "$1" >"$tmp/conf.json"; }

# expect <name> <exit> [text]
expect() {
  local name="$1" want="$2" text="${3:-}" rc=0 out
  out="$(bash "$here/updater-key-id.sh" "$tmp/app.sig" "$tmp/conf.json" 2>&1)" || rc=$?
  if [ "$rc" != "$want" ]; then
    fail "$name: exit $rc, want $want"
    printf '%s\n' "$out" >&2
    return
  fi
  if [ -n "$text" ] && ! grep -qF -- "$text" <<<"$out"; then
    fail "$name: the output lacks '$text'"
    printf '%s\n' "$out" >&2
    return
  fi
  pass "$name"
}

conf "$(minisign 'minisign public key: 0807060504030201' 'Ed' "$id_a" 32)"
minisign 'signature from tauri secret key' 'ED' "$id_a" 64 >"$tmp/app.sig"
expect "same key" 0 "0102030405060708 matches"

minisign 'signature from tauri secret key' 'ED' "$id_b" 64 >"$tmp/app.sig"
expect "another key" 1 "signed by the updater key da349c4fa04741f0, but the app trusts only 0102030405060708"

printf 'not base64 at all' >"$tmp/app.sig"
expect "a broken signature" 1 "cannot read the key id of"

minisign 'signature from tauri secret key' 'ED' "$id_a" 64 >"$tmp/app.sig"
printf '{"plugins":{}}\n' >"$tmp/conf.json"
expect "no pubkey" 1 "has no plugins.updater.pubkey"

conf "$(printf 'one line only' | base64)"
expect "a pubkey without a key line" 1 "cannot read the key id of plugins.updater.pubkey"

# The real pubkey: its comment names the id, little endian.
cp "$here/../../app/src-tauri/tauri.conf.json" "$tmp/conf.json"
minisign 'signature from tauri secret key' 'ED' "$id_b" 64 >"$tmp/app.sig"
expect "the committed pubkey (id F04147A04F9C34DA)" 0 "da349c4fa04741f0 matches"

rc=0
bash "$here/updater-key-id.sh" "$tmp/app.sig" >/dev/null 2>&1 || rc=$?
if [ "$rc" = 2 ]; then pass "one argument is a usage error"; else fail "one argument is a usage error (exit $rc)"; fi

if [ "$fails" -ne 0 ]; then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "all updater key id checks passed"
