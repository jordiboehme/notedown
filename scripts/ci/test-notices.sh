#!/usr/bin/env bash
# Fixture test of scripts/third-party-notices.sh without cargo-about: a stub
# cargo writes a rendered template, a fixture app has a lock file,
# node_modules and dist. Checks the block filter (a block with no registry
# crate is dropped), the bundled code (appended to macos and app, not to
# linux), the cargo-about arguments per variant and the npm set check (a
# banner or runtime outside the set fails).
#
# Usage: scripts/ci/test-notices.sh   (needs jq)
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1" >&2; fails=$((fails + 1)); }

# A copy of the repository parts the script reads, with fixture app files.
root="$tmp/root"
mkdir -p "$root/scripts" "$root/licenses" "$root/crates/notedown" "$root/app/src-tauri" "$tmp/bin"
cp "$repo/scripts/third-party-notices.sh" "$root/scripts/"
cp "$repo/about.toml" "$repo/about.hbs" "$root/"
cp "$repo/licenses/bundled-code.txt" "$repo/licenses/aws-lc.txt" "$root/licenses/"
cp "$repo/app/src-tauri/about.toml" "$root/app/src-tauri/"
touch "$root/crates/notedown/Cargo.toml" "$root/app/src-tauri/Cargo.toml"

# The rendered template as cargo-about would write it: an MIT block with a
# registry crate (the own path crate renders no line) and an AGPL block used
# only by the own crates. STUB_RAW overrides the text.
cat > "$tmp/raw-good.txt" <<'EOF'
@@notedown-block
========================================================================
MIT License (MIT)
========================================================================

Used by:
@@notedown-crate serde 1.0.229
@@notedown-crate anyhow 1.0.104

MIT text here

@@notedown-end
@@notedown-block
========================================================================
GNU Affero General Public License v3.0 or later (AGPL-3.0-or-later)
========================================================================

Used by:

AGPL text here

@@notedown-end
EOF

cat > "$tmp/bin/cargo" <<'EOF'
#!/usr/bin/env bash
# Stub: logs the arguments, writes $STUB_RAW to the --output-file path.
set -euo pipefail
echo "$*" >> "$STUB_LOG"
out=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output-file) out="$2"; shift 2 ;;
    *) shift ;;
  esac
done
[ -n "$out" ] || { echo "stub cargo: no --output-file" >&2; exit 1; }
cp "$STUB_RAW" "$out"
EOF
chmod +x "$tmp/bin/cargo"

# Fixture app: two production packages (one without a license file, as the
# Tauri plugins), one dev package that Vite bundles (listed), one dev package
# that is not bundled.
app="$root/app"
cat > "$app/package-lock.json" <<'EOF'
{
  "name": "fixture",
  "lockfileVersion": 3,
  "packages": {
    "": { "name": "fixture" },
    "node_modules/@scope/api": { "version": "2.0.0", "license": "Apache-2.0 OR MIT" },
    "node_modules/@scope/plugin": { "version": "1.0.0", "license": "MIT OR Apache-2.0" },
    "node_modules/@scope/plugin/node_modules/helper": { "version": "2.0.0", "license": "MIT" },
    "node_modules/helper": { "version": "1.0.0", "dev": true, "license": "MIT" },
    "node_modules/svelte": { "version": "5.0.0", "dev": true, "license": "MIT" },
    "node_modules/tailwindcss": { "version": "4.0.0", "dev": true, "license": "MIT" },
    "node_modules/vite": { "version": "8.0.0", "dev": true, "license": "MIT" }
  }
}
EOF
mkpkg() { # <name> <version> <license> [license-file-text]
  mkdir -p "$app/node_modules/$1"
  printf '{ "name": "%s", "version": "%s", "license": "%s" }\n' "$1" "$2" "$3" > "$app/node_modules/$1/package.json"
  if [ "$#" -ge 4 ]; then printf '%s\n' "$4" > "$app/node_modules/$1/LICENSE"; fi
}
mkpkg @scope/api 2.0.0 "Apache-2.0 OR MIT" "api license text"
mkpkg @scope/plugin 1.0.0 "MIT OR Apache-2.0"
# A nested copy at another version than the top-level dev copy: the notices
# must read the nested one.
mkpkg @scope/plugin/node_modules/helper 2.0.0 MIT "nested helper license text"
printf '{ "name": "helper", "version": "2.0.0", "license": "MIT" }\n' > "$app/node_modules/@scope/plugin/node_modules/helper/package.json"
mkpkg helper 1.0.0 MIT "top helper license text"
printf 'PackageCopyrightText: 2019-2022, Fixture Authors\n' > "$app/node_modules/@scope/plugin/LICENSE.spdx"
mkpkg svelte 5.0.0 MIT "svelte license text"
mkpkg tailwindcss 4.0.0 MIT "tailwind license text"
printf '# comment\nsvelte\ntailwindcss\n' > "$app/bundled-packages.txt"
mkdir -p "$app/dist/assets"
printf '/*! tailwindcss v4.0.0 | MIT License | https://tailwindcss.com */\n' > "$app/dist/assets/index.css"
# shellcheck disable=SC2016 # a literal JS template string
printf 'var a=((window.__svelte??={}).v??=new Set).add(`5`);\n' > "$app/dist/assets/index.js"

export PATH="$tmp/bin:$PATH" STUB_LOG="$tmp/args.log" STUB_RAW="$tmp/raw-good.txt"

run() { # <variant> <out>; output in $tmp/run.log, status in $status
  : > "$STUB_LOG"
  status=0
  bash "$root/scripts/third-party-notices.sh" "$@" > "$tmp/run.log" 2>&1 || status=$?
}

# --- macos
run macos "$tmp/macos.txt"
if [ "$status" -eq 0 ]; then pass "macos runs"; else fail "macos runs"; cat "$tmp/run.log" >&2; fi
if grep -qxF '  - serde 1.0.229' "$tmp/macos.txt"; then pass "macos lists the registry crates"; else fail "macos lists the registry crates"; fi
if grep -qF 'AGPL text here' "$tmp/macos.txt"; then fail "macos drops the block without a registry crate"; else pass "macos drops the block without a registry crate"; fi
if grep -qF 'Copyright (c) 2023-2026 The ggml authors' "$tmp/macos.txt"; then pass "macos appends the bundled code"; else fail "macos appends the bundled code"; fi
for want in 'Eric Young' 'The OpenSSL Project'; do
  if grep -qF -- "$want" "$tmp/macos.txt"; then pass "macos has aws-lc '$want'"; else fail "macos has aws-lc '$want'"; fi
done
if grep -q '^#' "$tmp/macos.txt"; then fail "macos drops the comment header of bundled-code.txt"; else pass "macos drops the comment header of bundled-code.txt"; fi
if grep -q '^@@' "$tmp/macos.txt"; then fail "macos leaves no marker"; else pass "macos leaves no marker"; fi
if grep -qF -- '--target aarch64-apple-darwin' "$STUB_LOG" && ! grep -qF -- '--no-default-features' "$STUB_LOG" \
  && grep -qF -- "--manifest-path $root/crates/notedown/Cargo.toml" "$STUB_LOG"; then pass "macos cargo-about arguments"; else fail "macos cargo-about arguments"; fi

# --- linux
run linux "$tmp/linux.txt"
if [ "$status" -eq 0 ]; then pass "linux runs"; else fail "linux runs"; cat "$tmp/run.log" >&2; fi
if grep -qxF '  - anyhow 1.0.104' "$tmp/linux.txt"; then pass "linux lists the registry crates"; else fail "linux lists the registry crates"; fi
for want in 'Eric Young' 'The OpenSSL Project'; do
  if grep -qF -- "$want" "$tmp/linux.txt"; then pass "linux has aws-lc '$want'"; else fail "linux has aws-lc '$want'"; fi
done
if grep -qF 'ggml authors' "$tmp/linux.txt"; then fail "linux has no bundled code"; else pass "linux has no bundled code"; fi
if grep -qF -- '--no-default-features --target x86_64-unknown-linux-musl --target aarch64-unknown-linux-musl' "$STUB_LOG"; then pass "linux cargo-about arguments"; else fail "linux cargo-about arguments"; fi

# --- app
run app "$tmp/app.txt"
if [ "$status" -eq 0 ]; then pass "app runs"; else fail "app runs"; cat "$tmp/run.log" >&2; fi
for want in 'svelte 5.0.0 (MIT)' 'tailwindcss 4.0.0 (MIT)' '@scope/api 2.0.0 (Apache-2.0 OR MIT)' \
  'svelte license text' 'Copyright 2019-2022, Fixture Authors' 'Copyright (c) 2023-2026 The ggml authors' \
  'Part 2: the notedown program inside the app'; do
  if grep -qF -- "$want" "$tmp/app.txt"; then pass "app has '$want'"; else fail "app has '$want'"; fi
done
if grep -qxF 'helper 2.0.0 (MIT)' "$tmp/app.txt" && grep -qF 'nested helper license text' "$tmp/app.txt" \
  && ! grep -qF 'top helper license text' "$tmp/app.txt"; then
  pass "app reads a nested package at its lock path"
else
  fail "app reads a nested package at its lock path"
fi
if grep -qF 'vite 8.0.0' "$tmp/app.txt"; then fail "app leaves out unbundled dev packages"; else pass "app leaves out unbundled dev packages"; fi
if grep -qF -- "--manifest-path $root/app/src-tauri/Cargo.toml --config $root/app/src-tauri/about.toml" "$STUB_LOG" \
  && grep -qF -- "--manifest-path $root/crates/notedown/Cargo.toml --config $root/about.toml" "$STUB_LOG"; then pass "app runs cargo-about on the app and on notedown"; else fail "app runs cargo-about on the app and on notedown"; fi

# A banner of a package outside the set fails.
printf '/*! leftpad v1.0.0 | MIT */\n' > "$app/dist/assets/extra.js"
run app "$tmp/app-bad.txt"
if [ "$status" -ne 0 ] && grep -qF 'contains leftpad' "$tmp/run.log" && [ ! -e "$tmp/app-bad.txt" ]; then
  pass "app fails on a banner outside the set"
else
  fail "app fails on a banner outside the set"
fi
rm "$app/dist/assets/extra.js"

# The svelte runtime without svelte in the list fails.
printf 'tailwindcss\n' > "$app/bundled-packages.txt"
run app "$tmp/app-bad.txt"
if [ "$status" -ne 0 ] && grep -qF 'contains svelte' "$tmp/run.log"; then
  pass "app fails on the svelte runtime outside the set"
else
  fail "app fails on the svelte runtime outside the set"
fi
printf 'svelte\ntailwindcss\n' > "$app/bundled-packages.txt"

# A package without a license file and a license other than MIT/Apache fails.
mkpkg @scope/plugin 1.0.0 "BSD-2-Clause"
run app "$tmp/app-bad.txt"
if [ "$status" -ne 0 ] && grep -qF '@scope/plugin has no license file and its license BSD-2-Clause does not offer MIT' "$tmp/run.log"; then
  pass "app fails on a package without license text"
else
  fail "app fails on a package without license text"
fi
mkpkg @scope/plugin 1.0.0 "MIT OR Apache-2.0"

# The fallback for a package without a license file needs an MIT text in part 1.
printf '@@notedown-block\nApache License 2.0 (Apache-2.0)\nUsed by:\n@@notedown-crate b 1.0.0\n\ntext\n@@notedown-end\n' > "$tmp/raw-apache.txt"
STUB_RAW="$tmp/raw-apache.txt" run app "$tmp/app-bad.txt"
if [ "$status" -ne 0 ] && grep -qF 'part 1 has none' "$tmp/run.log"; then
  pass "app fails when the license-file fallback has no MIT text"
else
  fail "app fails when the license-file fallback has no MIT text"
fi

# No dist: a clear failure.
mv "$app/dist" "$app/dist.off"
run app "$tmp/app-bad.txt"
if [ "$status" -ne 0 ] && grep -qF 'does not exist' "$tmp/run.log"; then pass "app fails without dist"; else fail "app fails without dist"; fi
mv "$app/dist.off" "$app/dist"

# --- cargo-about output edge cases
printf '@@notedown-block\nX (X)\nUsed by:\n\ntext\n@@notedown-end\n' > "$tmp/raw-none.txt"
STUB_RAW="$tmp/raw-none.txt" run macos "$tmp/none.txt"
if [ "$status" -ne 0 ] && grep -qF 'lists no third-party crate' "$tmp/run.log"; then pass "no crate at all fails"; else fail "no crate at all fails"; fi

printf '@@notedown-block\nX (X)\n@@notedown-crate a 1.0.0\n' > "$tmp/raw-open.txt"
STUB_RAW="$tmp/raw-open.txt" run macos "$tmp/open.txt"
if [ "$status" -ne 0 ] && grep -qF 'unterminated license block' "$tmp/run.log"; then pass "unterminated block fails"; else fail "unterminated block fails"; fi

# --- usage
run windows "$tmp/x.txt"
if [ "$status" -eq 2 ]; then pass "unknown variant is a usage error"; else fail "unknown variant is a usage error"; fi
run macos
if [ "$status" -eq 2 ]; then pass "missing output is a usage error"; else fail "missing output is a usage error"; fi

if [ "$fails" -ne 0 ]; then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "all notices checks passed"
