#!/usr/bin/env bash
# Usage: test-licence-headers.sh [root]
#
# The notedown-ocr crate is MPL-2.0 since 0.2.0 (the rest of the workspace is
# AGPL-3.0-or-later). Checks that the crate says so: every .rs file starts with
# the three-line MPL-2.0 header, the crate has the licence text, and its
# Cargo.toml names MPL-2.0.
set -uo pipefail

root="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
crate="$root/crates/notedown-ocr"
fails=0
fail() { echo "FAIL: $1" >&2; fails=$((fails + 1)); }
pass() { echo "ok: $1"; }

header='// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.'

count=0
while IFS= read -r -d '' file; do
  count=$((count + 1))
  if [ "$(head -n 3 "$file")" != "$header" ]; then
    fail "${file#"$root"/} does not start with the MPL-2.0 header"
  fi
done < <(find "$crate" -name '*.rs' -not -path '*/target/*' -print0)
if [ "$count" -eq 0 ]; then fail "no .rs file found under crates/notedown-ocr"; else pass "$count .rs files checked"; fi

if grep -qx 'license = "MPL-2.0"' "$crate/Cargo.toml"; then pass "Cargo.toml names MPL-2.0"; else fail "Cargo.toml does not name MPL-2.0"; fi
if grep -q 'Mozilla Public License Version 2.0' "$crate/LICENSE" 2>/dev/null; then pass "LICENSE holds the MPL-2.0 text"; else fail "crates/notedown-ocr/LICENSE is missing or not MPL-2.0"; fi

if [ "$fails" -ne 0 ]; then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "all licence header checks passed"
