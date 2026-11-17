#!/usr/bin/env bash
# Usage: updater-key-id.sh <file.sig> <tauri.conf.json>
#
# Checks that an updater signature was made with the key whose public half
# is `plugins.updater.pubkey` in tauri.conf.json. The released app trusts
# only that public key; a signature of another key passes every other check
# and then no installed app can update.
#
# Both values are minisign files in base64 (Tauri's form): decoded, line 2
# is base64 again, and its bytes 3 to 10 are the key id (bytes 1 and 2 name
# the algorithm). The ids must be equal.
#
# Exit 0 when they match, 1 with an ::error:: line when not or when a value
# cannot be read, 2 on a usage error.
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $(basename "$0") <file.sig> <tauri.conf.json>" >&2
  exit 2
fi
sig="$1"
conf="$2"
for f in "$sig" "$conf"; do
  if [ ! -f "$f" ]; then
    echo "updater-key-id.sh: $f does not exist" >&2
    exit 2
  fi
done

# key_id <base64 minisign file text>: the key id as hex, or nothing.
key_id() {
  local line2
  line2="$(printf '%s' "$1" | base64 --decode 2>/dev/null | sed -n 2p)" || true
  [ -n "$line2" ] || return 0
  printf '%s' "$line2" | base64 --decode 2>/dev/null | od -An -tx1 -j2 -N8 | tr -d ' \n' || true
}

pubkey="$(jq -r '.plugins.updater.pubkey // empty' "$conf")"
if [ -z "$pubkey" ]; then
  echo "::error::$conf has no plugins.updater.pubkey"
  exit 1
fi
want="$(key_id "$pubkey")"
got="$(key_id "$(cat "$sig")")"
if [ "${#want}" -ne 16 ]; then
  echo "::error::cannot read the key id of plugins.updater.pubkey in $conf"
  exit 1
fi
if [ "${#got}" -ne 16 ]; then
  echo "::error::cannot read the key id of $sig"
  exit 1
fi
if [ "$got" != "$want" ]; then
  echo "::error::$sig is signed by the updater key $got, but the app trusts only $want (plugins.updater.pubkey in $conf). TAURI_SIGNING_PRIVATE_KEY does not belong to the public key: no installed app could install this update."
  exit 1
fi
echo "updater key id $got matches plugins.updater.pubkey"
