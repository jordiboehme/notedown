#!/usr/bin/env bash
# Fixture test of scripts/ci/app-sbom.sh: two small cargo SBOMs with a shared
# crate, an npm SBOM with a production package, a bundled devDependency, a
# build tool and a second version of the bundled package, and the lock file
# and list that pick the npm set.
#
# Usage: scripts/ci/test-app-sbom.sh   (needs jq)
#
# The jq filters are single-quoted on purpose.
# shellcheck disable=SC2016
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fails=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1" >&2; fails=$((fails + 1)); }

cat >"$tmp/app.json" <<'EOF'
{
  "bomFormat": "CycloneDX",
  "specVersion": "1.5",
  "metadata": {"component": {"name": "notedown-app", "bom-ref": "path+file:///w/app/src-tauri#notedown-app@0.1.0"}},
  "components": [
    {"name": "serde", "version": "1.0.0", "bom-ref": "registry#serde@1.0.0", "purl": "pkg:cargo/serde@1.0.0"},
    {"name": "tauri", "version": "2.12.5", "bom-ref": "registry#tauri@2.12.5", "purl": "pkg:cargo/tauri@2.12.5"},
    {"name": "bare-a", "version": "1.0.0"},
    {"name": "bare-b", "version": "1.0.0"},
    {"name": "bare-a", "version": "1.0.0"}
  ],
  "dependencies": [
    {"ref": "path+file:///w/app/src-tauri#notedown-app@0.1.0", "dependsOn": ["registry#tauri@2.12.5"]},
    {"ref": "registry#tauri@2.12.5", "dependsOn": ["registry#serde@1.0.0"]},
    {"ref": "registry#serde@1.0.0", "dependsOn": []}
  ]
}
EOF

cat >"$tmp/cli.json" <<'EOF'
{
  "bomFormat": "CycloneDX",
  "specVersion": "1.5",
  "metadata": {"component": {"name": "notedown", "version": "0.1.0", "bom-ref": "path+file:///w/crates/notedown#0.1.0", "purl": "pkg:cargo/notedown@0.1.0?download_url=file://."}},
  "components": [
    {"name": "serde", "version": "1.0.0", "bom-ref": "registry#serde@1.0.0", "purl": "pkg:cargo/serde@1.0.0"},
    {"name": "llama-cpp-sys-2", "version": "0.1.161", "bom-ref": "registry#llama-cpp-sys-2@0.1.161", "purl": "pkg:cargo/llama-cpp-sys-2@0.1.161"},
    {"name": "candle-core", "version": "0.11.1", "bom-ref": "registry#candle-core@0.11.1", "purl": "pkg:cargo/candle-core@0.11.1"},
    {"name": "pagevision", "version": "0.1.0", "bom-ref": "git+https://github.com/jordiboehme/rezzer?rev=74188f8dc0a39053e29ba42a5b04b893bec2bd34#pagevision@0.1.0", "purl": "pkg:cargo/pagevision@0.1.0?vcs_url=git%2Bhttps://github.com/jordiboehme/rezzer%4074188f8dc0a39053e29ba42a5b04b893bec2bd34"}
  ],
  "dependencies": [
    {"ref": "path+file:///w/crates/notedown#0.1.0", "dependsOn": ["git+https://github.com/jordiboehme/rezzer?rev=74188f8dc0a39053e29ba42a5b04b893bec2bd34#pagevision@0.1.0", "registry#llama-cpp-sys-2@0.1.161", "registry#serde@1.0.0"]},
    {"ref": "git+https://github.com/jordiboehme/rezzer?rev=74188f8dc0a39053e29ba42a5b04b893bec2bd34#pagevision@0.1.0", "dependsOn": ["registry#candle-core@0.11.1", "registry#llama-cpp-sys-2@0.1.161"]},
    {"ref": "registry#serde@1.0.0", "dependsOn": []},
    {"ref": "registry#candle-core@0.11.1", "dependsOn": []},
    {"ref": "registry#llama-cpp-sys-2@0.1.161", "dependsOn": []}
  ]
}
EOF

cat >"$tmp/npm.json" <<'EOF'
{
  "bomFormat": "CycloneDX",
  "specVersion": "1.5",
  "metadata": {"component": {"name": "app", "bom-ref": "notedown-app@0.1.0"}},
  "components": [
    {"name": "@tauri-apps/api", "version": "2.12.4", "bom-ref": "@tauri-apps/api@2.12.4", "purl": "pkg:npm/%40tauri-apps/api@2.12.4"},
    {"name": "svelte", "version": "5.57.3", "bom-ref": "svelte@5.57.3", "purl": "pkg:npm/svelte@5.57.3"},
    {"name": "svelte", "version": "4.0.0", "bom-ref": "svelte@4.0.0", "purl": "pkg:npm/svelte@4.0.0"},
    {"name": "vite", "version": "7.0.0", "bom-ref": "vite@7.0.0", "purl": "pkg:npm/vite@7.0.0"}
  ],
  "dependencies": [
    {"ref": "notedown-app@0.1.0", "dependsOn": ["vite@7.0.0"]}
  ]
}
EOF

cat >"$tmp/package-lock.json" <<'EOF'
{
  "name": "notedown-app",
  "lockfileVersion": 3,
  "packages": {
    "": {"name": "notedown-app"},
    "node_modules/@tauri-apps/api": {"version": "2.12.4"},
    "node_modules/svelte": {"version": "5.57.3", "dev": true},
    "node_modules/some-tool/node_modules/svelte": {"version": "4.0.0", "dev": true},
    "node_modules/vite": {"version": "7.0.0", "dev": true}
  }
}
EOF
printf '# bundled\nsvelte\n' >"$tmp/bundled.txt"

run() {
  rc=0
  out="$(bash "$here/app-sbom.sh" "$tmp/app.json" "$tmp/cli.json" "$tmp/npm.json" \
    "$tmp/package-lock.json" "$tmp/bundled.txt" "$tmp/out.json" 2>&1)" || rc=$?
}

check() {
  local name="$1" filter="$2"
  if jq -e "$filter" "$tmp/out.json" >/dev/null; then
    pass "$name"
  else
    fail "$name"
  fi
}

run
if [ "$rc" != 0 ]; then
  fail "the merge runs: exit $rc; output: $out"
else
  pass "the merge runs"
  check "the app stays the root" '.metadata.component.name == "notedown-app"'
  check "a crate in both trees is listed once" '[.components[] | select(.name == "serde")] | length == 1'
  check "components without purl and bom-ref stay apart, by name@version" \
    '[.components[] | select(.name | startswith("bare-")) | .name] == ["bare-a", "bare-b"]'
  check "the crates of the program are in" 'any(.components[]; .name == "llama-cpp-sys-2")'
  check "the OCR crates of the program are in, the git one too" '
    any(.components[]; .name == "candle-core") and any(.components[]; .name == "pagevision")'
  check "the git crate keeps its dependencies" '
    .dependencies[] | select(.ref | startswith("git+https://github.com/jordiboehme/rezzer?"))
    | .dependsOn | index("registry#candle-core@0.11.1") != null'
  check "the program itself is a component" 'any(.components[]; .name == "notedown")'
  check "the production npm package is in" 'any(.components[]; .name == "@tauri-apps/api")'
  check "the bundled devDependency is in, at its top-level version" \
    '[.components[] | select(.name == "svelte") | .version] == ["5.57.3"]'
  check "a build tool is left out" 'all(.components[]; .name != "vite")'
  check "the app root names the program and the npm packages" '
    (.dependencies[] | select(.ref == "path+file:///w/app/src-tauri#notedown-app@0.1.0") | .dependsOn) as $d
    | ($d | index("path+file:///w/crates/notedown#0.1.0")) != null
      and ($d | index("svelte@5.57.3")) != null
      and ($d | index("registry#tauri@2.12.5")) != null'
  check "a shared dependency entry is listed once" '[.dependencies[] | select(.ref == "registry#serde@1.0.0")] | length == 1'
  check "the program keeps its own dependencies" '
    .dependencies[] | select(.ref == "path+file:///w/crates/notedown#0.1.0") | .dependsOn | index("registry#llama-cpp-sys-2@0.1.161") != null'
  check "no npm dev graph is taken" 'all(.dependencies[]; .ref != "notedown-app@0.1.0")'
fi

# A bundled package the npm SBOM does not have fails.
printf 'svelte\nmissing-pkg\n' >"$tmp/bundled.txt"
jq '.packages["node_modules/missing-pkg"] = {"version": "1.0.0", "dev": true}' \
  "$tmp/package-lock.json" >"$tmp/lock2.json"
mv "$tmp/lock2.json" "$tmp/package-lock.json"
run
if [ "$rc" != 0 ] && grep -qF 'missing-pkg@1.0.0' <<<"$out"; then
  pass "a package of the set missing from the npm SBOM fails"
else
  fail "a package of the set missing from the npm SBOM fails: exit $rc; output: $out"
fi

# A package of the list that is not in the lock file fails.
printf 'svelte\nnot-installed\n' >"$tmp/bundled.txt"
run
if [ "$rc" != 0 ] && grep -qF 'not-installed has no version' <<<"$out"; then
  pass "a listed package missing from the lock file fails"
else
  fail "a listed package missing from the lock file fails: exit $rc; output: $out"
fi

# An input that is not CycloneDX fails.
printf 'svelte\n' >"$tmp/bundled.txt"
echo '{}' >"$tmp/cli.json"
run
if [ "$rc" != 0 ] && grep -qF 'is not a CycloneDX document' <<<"$out"; then
  pass "an input that is not CycloneDX fails"
else
  fail "an input that is not CycloneDX fails: exit $rc; output: $out"
fi

if [ "$fails" -gt 0 ]; then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "all app-sbom checks passed"
