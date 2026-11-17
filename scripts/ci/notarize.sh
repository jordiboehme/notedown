#!/usr/bin/env bash
# Usage: notarize.sh <file>
#
# Sends a zip, dmg or pkg to Apple's notary service and waits for the
# verdict. Exit 0 when Apple accepted it, 1 with an ::error:: line when not
# (with the notary log when there is a submission), 2 on a usage error. It
# does not staple: the caller staples what can carry a ticket.
#
# Environment (the App Store Connect API key, from the release environment):
#   APPLE_API_KEY_P8     the text of the .p8 key file
#   APPLE_API_KEY_ID     its key id
#   APPLE_API_ISSUER_ID  its issuer id
#
# notarytool --timeout only limits its own polling, so every call also gets a
# hard limit from perl's alarm (the alarm survives the exec). The upload runs
# once and is retried only when it fails; the wait runs on the submission id,
# so waiting again never queues a second upload. Per try, in seconds, with
# the defaults:
#   NOTARIZE_SUBMIT_TRIES=3  NOTARIZE_SUBMIT_CAP=300   upload
#   NOTARIZE_WAIT_TRIES=2    NOTARIZE_WAIT_CAP=780     wait (--timeout 12m)
#   NOTARIZE_LOG_CAP=120                               log on failure
# Worst case with the defaults: 3 x 300 + 5 + 10 (retry.sh's pauses) + 2 x
# 780 + 5 + 120 = 2600 s, 43.3 min. A caller's step timeout goes above the
# worst case of the values it uses, so this script's ::error:: comes first.
#
# The key is written to a private temp folder and removed on exit. The
# JSON answers are printed; they hold no secret.
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $(basename "$0") <file>" >&2
  exit 2
fi
file="$1"
if [ ! -f "$file" ]; then
  echo "notarize.sh: $file does not exist" >&2
  exit 2
fi
for var in APPLE_API_KEY_P8 APPLE_API_KEY_ID APPLE_API_ISSUER_ID; do
  if [ -z "${!var:-}" ]; then
    echo "notarize.sh: $var is not set" >&2
    exit 2
  fi
done

submit_tries="${NOTARIZE_SUBMIT_TRIES:-3}"
submit_cap="${NOTARIZE_SUBMIT_CAP:-300}"
wait_tries="${NOTARIZE_WAIT_TRIES:-2}"
wait_cap="${NOTARIZE_WAIT_CAP:-780}"
log_cap="${NOTARIZE_LOG_CAP:-120}"
for value in "$submit_tries" "$submit_cap" "$wait_tries" "$wait_cap" "$log_cap"; do
  case "$value" in
    '' | *[!0-9]* | 0)
      echo "notarize.sh: tries and caps must be positive integers, got '$value'" >&2
      exit 2
      ;;
  esac
done
# notarytool stops polling a minute before the alarm, so its own answer
# (a JSON object with a status) comes first when Apple is only slow.
wait_timeout="$(((wait_cap - 60) / 60))"
[ "$wait_timeout" -ge 1 ] || wait_timeout=1

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
work="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/notarize.XXXXXX")"
trap 'rm -rf "$work"' EXIT
key="$work/api-key.p8"
(umask 077 && printf '%s\n' "$APPLE_API_KEY_P8" >"$key")
sub="$work/submit.json"
result="$work/wait.json"
name="$(basename "$file")"

# The redirect sits inside bash -c, so the ::warning:: lines of retry.sh stay
# out of the JSON; each try overwrites the file.
echo "notarize: uploading $name"
set +e
# shellcheck disable=SC2016 # $1..$6 belong to the inner bash
bash "$here/retry.sh" "$submit_tries" bash -c \
  'perl -e "alarm shift; exec @ARGV" "$1" xcrun notarytool submit "$2" --key "$3" --key-id "$4" --issuer "$5" --no-wait --output-format json >"$6"' \
  _ "$submit_cap" "$file" "$key" "$APPLE_API_KEY_ID" "$APPLE_API_ISSUER_ID" "$sub"
submit_rc=$?
set -e
cat "$sub" 2>/dev/null && echo || true
if ! jq -e 'type == "object" and (.id | type == "string") and (.id | length > 0)' "$sub" >/dev/null 2>&1; then
  echo "::error::notarizing $name failed: the upload returned no submission id (exit $submit_rc)"
  exit 1
fi
id="$(jq -r '.id' "$sub")"

echo "notarize: waiting for submission $id ($name)"
set +e
# shellcheck disable=SC2016 # $1..$7 belong to the inner bash
bash "$here/retry.sh" "$wait_tries" bash -c \
  'perl -e "alarm shift; exec @ARGV" "$1" xcrun notarytool wait "$2" --key "$3" --key-id "$4" --issuer "$5" --timeout "$6" --output-format json >"$7"' \
  _ "$wait_cap" "$id" "$key" "$APPLE_API_KEY_ID" "$APPLE_API_ISSUER_ID" "${wait_timeout}m" "$result"
wait_rc=$?
set -e
cat "$result" 2>/dev/null && echo || true

# The last try can leave nothing or no JSON object (alarm, network, crash).
if ! jq -e 'type == "object"' "$result" >/dev/null 2>&1; then
  echo "::error::notarizing $name failed: notarytool gave no answer for submission $id (exit $wait_rc)"
  exit 1
fi
status="$(jq -r '.status // "unknown"' "$result")"
if [ "$wait_rc" -ne 0 ] || [ "$status" != Accepted ]; then
  echo "::error::notarizing $name failed: submission $id has the status $status (notarytool exit $wait_rc)"
  perl -e 'alarm shift; exec @ARGV' "$log_cap" xcrun notarytool log "$id" \
    --key "$key" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID" || true
  exit 1
fi
echo "notarize: $name accepted (submission $id)"
