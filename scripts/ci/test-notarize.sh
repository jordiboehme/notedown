#!/usr/bin/env bash
# Fixture test of scripts/ci/notarize.sh: a stub `xcrun` on PATH plays
# notarytool. STUB_SUBMIT and STUB_WAIT pick its answer, STUB_DIR collects
# what it was given.
#
# Usage: scripts/ci/test-notarize.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1" >&2; fails=$((fails + 1)); }

mkdir -p "$tmp/bin" "$tmp/runner"
cat >"$tmp/bin/xcrun" <<'STUB'
#!/usr/bin/env bash
# xcrun notarytool <submit|wait|log> <file|id> --key <path> ...
[ "$1" = notarytool ] || exit 64
cmd="$2"
key=""
prev=""
for arg in "$@"; do
  [ "$prev" = --key ] && key="$arg"
  prev="$arg"
done
echo "$cmd" >>"$STUB_DIR/calls"
[ -n "$key" ] && cp "$key" "$STUB_DIR/key-seen"
case "$cmd" in
  submit)
    n="$(grep -c '^submit$' "$STUB_DIR/calls")"
    case "$STUB_SUBMIT" in
      ok) echo '{"id":"abc-123","message":"Successfully uploaded file"}' ;;
      noid) echo '{"message":"something else"}' ;;
      fail) echo 'Error: HTTP status code 500' >&2; exit 1 ;;
      fail-once) [ "$n" -gt 1 ] || exit 1; echo '{"id":"abc-123"}' ;;
    esac
    ;;
  wait)
    case "$STUB_WAIT" in
      accepted) echo '{"id":"abc-123","status":"Accepted","message":"Processing complete"}' ;;
      invalid) echo '{"id":"abc-123","status":"Invalid","message":"Processing complete"}'; exit 1 ;;
      garbage) echo 'not json'; exit 1 ;;
      hang) exec sleep 10 ;;
    esac
    ;;
  log) echo '{"issues":[]}' ;;
esac
STUB
chmod +x "$tmp/bin/xcrun"
printf 'zip' >"$tmp/app.zip"

# expect <name> <exit> <submit> <wait> [text in the output]
expect() {
  local name="$1" want="$2" submit="$3" wait="$4" text="${5:-}" rc=0 out
  rm -rf "$tmp/stub"
  mkdir -p "$tmp/stub"
  out="$(PATH="$tmp/bin:$PATH" STUB_DIR="$tmp/stub" STUB_SUBMIT="$submit" STUB_WAIT="$wait" \
    RUNNER_TEMP="$tmp/runner" APPLE_API_KEY_P8=$'-----BEGIN PRIVATE KEY-----\nkey\n-----END PRIVATE KEY-----' \
    APPLE_API_KEY_ID=KEYID APPLE_API_ISSUER_ID=issuer \
    NOTARIZE_SUBMIT_TRIES="${TRIES:-1}" NOTARIZE_WAIT_TRIES=1 NOTARIZE_WAIT_CAP="${CAP:-120}" \
    bash "$here/notarize.sh" "$tmp/app.zip" 2>&1)" || rc=$?
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

expect "accepted" 0 ok accepted "accepted (submission abc-123)"
if [ "$(cat "$tmp/stub/key-seen")" = $'-----BEGIN PRIVATE KEY-----\nkey\n-----END PRIVATE KEY-----' ]; then
  pass "the key reaches notarytool unchanged"
else
  fail "the key reaches notarytool unchanged"
fi
if [ -z "$(ls -A "$tmp/runner")" ]; then
  pass "the key folder is gone after the run"
else
  fail "the key folder is gone after the run"
fi
expect "no submission id" 1 noid accepted "no submission id"
expect "upload fails" 1 fail accepted "no submission id"
TRIES=2 expect "upload fails once, then works" 0 fail-once accepted "accepted"
expect "invalid, with the log" 1 ok invalid "the status Invalid"
if grep -qx log "$tmp/stub/calls"; then pass "the log is fetched on failure"; else fail "the log is fetched on failure"; fi
expect "no JSON from wait" 1 ok garbage "gave no answer"
CAP=2 expect "wait hangs past the alarm" 1 ok hang "gave no answer"
if [ -z "$(ls -A "$tmp/runner")" ]; then
  pass "the key folder is gone after a failure"
else
  fail "the key folder is gone after a failure"
fi

rc=0
APPLE_API_KEY_P8=k APPLE_API_KEY_ID=i bash "$here/notarize.sh" "$tmp/app.zip" >/dev/null 2>&1 || rc=$?
if [ "$rc" = 2 ]; then pass "a missing issuer id is a usage error"; else fail "a missing issuer id is a usage error (exit $rc)"; fi
rc=0
APPLE_API_KEY_P8=k APPLE_API_KEY_ID=i APPLE_API_ISSUER_ID=s bash "$here/notarize.sh" "$tmp/none.zip" >/dev/null 2>&1 || rc=$?
if [ "$rc" = 2 ]; then pass "a missing file is a usage error"; else fail "a missing file is a usage error (exit $rc)"; fi

if [ "$fails" -ne 0 ]; then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "all notarize checks passed"
