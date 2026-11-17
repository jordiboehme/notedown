#!/usr/bin/env bash
# Usage: latest-json.sh <version> <app.tar.gz.sig> <notes.md>
#
# Prints latest.json, the file the app's updater reads from
# releases/latest/download/latest.json, for the release v<version>
# (https://v2.tauri.app/plugin/updater, "Static JSON File"):
#
#   version   <version>, the tag without the v (0.0.0-dev in the dry run)
#   notes     the release notes file without its H1 line, trimmed
#   pub_date  now in UTC as RFC 3339, or $PUB_DATE when set and not empty
#   platforms.darwin-aarch64.signature  the content of the .sig file
#   platforms.darwin-aarch64.url        notedown-app-v<version>-macos.app.tar.gz
#                                       of the release v<version>
#
# A broken latest.json strands every installed app on its version, so every
# field must be there and not empty: an empty .sig, a .sig that is not one
# line of base64, or notes with no text after the H1 fail, and nothing is
# printed then.
#
# Exit 0 with the JSON on stdout. Exit 1 when a field would be empty or
# broken, exit 2 on bad usage (arguments, version, PUB_DATE, unreadable
# file). Errors go to stderr.
set -u
export LC_ALL=C

usage() {
  echo "usage: latest-json.sh <version> <app.tar.gz.sig> <notes.md>" >&2
  exit 2
}

[ "$#" -eq 3 ] || usage
version="$1"
sig_file="$2"
notes_file="$3"

version_re='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+(\.[0-9A-Za-z]+)*)?$'
if ! [[ "$version" =~ $version_re ]]; then
  echo "latest-json.sh: the version must look like 1.2.3 or 1.2.3-rc.1 (no v), got '$version'" >&2
  exit 2
fi

for f in "$sig_file" "$notes_file"; do
  if [ ! -f "$f" ] || [ ! -r "$f" ]; then
    echo "latest-json.sh: cannot read '$f'" >&2
    exit 2
  fi
done

pub_date="${PUB_DATE:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"
if ! [[ "$pub_date" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]]; then
  echo "latest-json.sh: PUB_DATE must be UTC RFC 3339 (2026-11-17T12:00:00Z), got '$pub_date'" >&2
  exit 2
fi

# Tauri writes the .sig as one line of base64 without a newline; a newline
# at the end is dropped.
signature="$(tr -d '\n' <"$sig_file")"
if [ -z "$signature" ]; then
  echo "latest-json.sh: the signature file $sig_file is empty" >&2
  exit 1
fi
if ! [[ "$signature" =~ ^[A-Za-z0-9+/]+=*$ ]]; then
  echo "latest-json.sh: the signature file $sig_file is not one line of base64" >&2
  exit 1
fi

# The notes: the file without its first line when that is the H1, without
# blank lines at the start and the end, and without trailing spaces.
notes="$(awk '
  NR == 1 && /^# / { next }
  { sub(/[[:space:]]+$/, ""); lines[++n] = $0 }
  END {
    first = 1
    while (first <= n && lines[first] == "") first++
    last = n
    while (last >= first && lines[last] == "") last--
    for (i = first; i <= last; i++) print lines[i]
  }
' "$notes_file")"
if [ -z "$notes" ]; then
  echo "latest-json.sh: the notes file $notes_file has no text after its H1" >&2
  exit 1
fi

url="https://github.com/jordiboehme/notedown/releases/download/v${version}/notedown-app-v${version}-macos.app.tar.gz"

jq -n \
  --arg version "$version" \
  --arg notes "$notes" \
  --arg pub_date "$pub_date" \
  --arg signature "$signature" \
  --arg url "$url" \
  '{
    version: $version,
    notes: $notes,
    pub_date: $pub_date,
    platforms: { "darwin-aarch64": { signature: $signature, url: $url } }
  }'
