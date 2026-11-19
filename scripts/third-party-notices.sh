#!/usr/bin/env bash
# Writes THIRD-PARTY-NOTICES.txt for a Notedown release. Three variants:
#
#   macos  the notedown archive for macOS: the crates of crates/notedown with
#          default features (OCR) for aarch64-apple-darwin (about.toml), then
#          the AWS-LC license file (licenses/aws-lc.txt) and the code built
#          into it that the crate notices do not show: the C and C++ code of
#          llama.cpp and the Apache-2.0 parts of pagevision
#          (licenses/bundled-code.txt).
#   linux  the notedown archives and .deb packages for Linux: the crates of
#          crates/notedown with --no-default-features for both musl targets,
#          then licenses/aws-lc.txt. No llama.cpp, so no bundled-code.txt.
#   app    the Notedown app: the crates of app/src-tauri for
#          aarch64-apple-darwin (app/src-tauri/about.toml), then the whole
#          macos text (the notedown program inside the app), then the npm
#          packages built into app/dist.
#
# Usage: scripts/third-party-notices.sh <macos|linux|app> <out-file>
#
# Needs cargo-about on PATH and the crates fetched (cargo fetch --locked). The
# app variant also needs jq, app/node_modules (npm ci) and app/dist (npm run
# build).
#
# The npm set is the production packages of app/package-lock.json plus
# app/bundled-packages.txt (devDependencies that Vite compiles into dist).
# Every "/*! <name> v<version>" banner and every known runtime marker in dist
# must name a package of the set, or the script fails.
#
# Exit 2 on a usage error, 1 on any other failure.
set -euo pipefail

usage() {
  echo "usage: $(basename "$0") <macos|linux|app> <out-file>" >&2
  exit 2
}

die() {
  echo "error: $*" >&2
  exit 1
}

# Runtimes that leave no banner in the bundle: "<package> <fixed string>".
# Svelte 5 registers its major version on window.__svelte.
RUNTIME_MARKERS='svelte __svelte'

# filter_blocks <raw> <out> <count-file>
# Keeps a license block of the cargo-about output only when it names at least
# one crate, and turns the crate markers into list items. A block whose crates
# are all Notedown's own path crates comes out with no crate line and is left
# out. Writes the number of crate lines to <count-file>.
filter_blocks() {
  local raw="$1" out="$2" count="$3"
  awk -v count="$count" '
    $0 == "@@notedown-block" { inblock = 1; buf = ""; crates = 0; next }
    $0 == "@@notedown-end" {
      if (crates > 0) printf "%s", buf
      inblock = 0
      next
    }
    inblock && /^@@notedown-crate / {
      sub(/^@@notedown-crate /, "  - ")
      crates++
      total++
      buf = buf $0 "\n"
      next
    }
    inblock { buf = buf $0 "\n"; next }
    { print }
    END {
      if (inblock) { print "error: unterminated license block" > "/dev/stderr"; exit 1 }
      print total + 0 > count
    }
  ' "$raw" > "$out"
  if grep -q '^@@notedown' "$out"; then
    die "a template marker is left in the output"
  fi
}

# cargo_notices <manifest> <config> <out> [cargo-about args...]
# Runs cargo-about with about.hbs and writes the filtered license blocks.
cargo_notices() {
  local manifest="$1" config="$2" out="$3"
  shift 3
  local raw="$work/raw.txt" count="$work/count.txt"
  rm -f "$raw"
  # --locked: a Cargo.lock that would change fails. --fail: a crate whose
  # license cannot be read or clarified fails. -o, not a redirect.
  cargo about generate --locked --fail \
    --manifest-path "$manifest" \
    --config "$config" \
    --output-file "$raw" \
    "$@" \
    "$root/about.hbs"
  [ -s "$raw" ] || die "cargo-about wrote no output for $manifest"
  filter_blocks "$raw" "$out" "$count"
  local crates
  crates="$(cat "$count")"
  [ "$crates" -gt 0 ] || die "cargo-about output for $manifest lists no third-party crate"
  echo "$manifest: $crates crate entries" >&2
}

# bundled_code <file>: a notices file of licenses/ without its leading "#"
# lines (where the text comes from and how it was checked).
bundled_code() {
  local file="$1"
  [ -s "$file" ] || die "$file is missing or empty"
  awk 'body || !/^#/ { body = 1; print }' "$file"
}

# npm_set <package-lock.json> <bundled-packages.txt>
# Prints the package paths below node_modules, sorted: every non-dev package
# of the lock file with its full lock path (a nested package stays
# "a/node_modules/b") plus the names of the list (comments and blank lines
# ignored).
npm_set() {
  local lock="$1" list="$2"
  [ -s "$lock" ] || die "$lock is missing or empty"
  [ -s "$list" ] || die "$list is missing or empty"
  {
    jq -r '
      .packages // error("no packages in the lock file (lockfileVersion 2 or 3 needed)")
      | to_entries[]
      | select(.key | startswith("node_modules/"))
      | select(.value.dev != true)
      | .key | sub("^node_modules/"; "")
    ' "$lock"
    sed -e 's/#.*//' -e 's/[[:space:]]//g' "$list" | grep -v '^$' || true
  } | LC_ALL=C sort -u
}

# check_dist <set-file> <dist-dir>
# Fails when dist shows a package banner or runtime marker whose package is
# not in the set. A set path counts by its package name (the part after the
# last node_modules/).
check_dist() {
  local set="$1" dist="$2" found="$work/found.txt" names="$work/names.txt" name marker missing=0
  sed -E "s|^.*node_modules/||" "$set" | LC_ALL=C sort -u > "$names"
  [ -d "$dist" ] || die "$dist does not exist (run npm ci and npm run build in app first)"
  : > "$found"
  grep -rhoE '/\*! (@[A-Za-z0-9._-]+/)?[A-Za-z0-9._-]+ v[0-9]' "$dist" \
    | sed -E 's|^/\*! ||; s| v[0-9]$||' >> "$found" || true
  while read -r name marker; do
    if grep -rqF -- "$marker" "$dist"; then
      echo "$name" >> "$found"
    fi
  done <<< "$RUNTIME_MARKERS"
  [ -s "$found" ] || die "$dist shows no package banner or runtime marker, the check would prove nothing"
  while read -r name; do
    if ! grep -qxF -- "$name" "$names"; then
      echo "error: $dist contains $name, which is neither a production dependency nor in app/bundled-packages.txt" >&2
      missing=1
    fi
  done < <(LC_ALL=C sort -u "$found")
  [ "$missing" -eq 0 ] || exit 1
}

# npm_notices <set-file> <node_modules>
# Prints one block per package: name, version, declared license, then the
# license files of the package. A package without a license file is allowed
# only when its license offers MIT (MIT, or MIT OR Apache-2.0): the block then
# says it is used under MIT, names the copyright from LICENSE.spdx and sets
# fallback=1, and main checks that part 1 has an MIT text.
npm_notices() {
  local set="$1" modules="$2" path name dir version license files copyright
  fallback=0
  [ -d "$modules" ] || die "$modules does not exist (run npm ci in app first)"
  while read -r path; do
    dir="$modules/$path"
    [ -s "$dir/package.json" ] || die "$path is in the npm set but not in $modules"
    name="$(jq -r '.name // empty' "$dir/package.json")"
    [ -n "$name" ] || name="${path##*node_modules/}"
    version="$(jq -r '.version // empty' "$dir/package.json")"
    license="$(jq -r '(.license | if type == "object" then .type else . end) // empty' "$dir/package.json")"
    [ -n "$version" ] || die "$dir/package.json has no version"
    [ -n "$license" ] || die "$dir/package.json has no license"
    echo "========================================================================"
    echo "$name $version ($license)"
    echo "========================================================================"
    echo
    files="$(find "$dir" -maxdepth 1 -type f \( -iname 'licen[cs]e*' -o -iname 'copying*' \) ! -iname '*.spdx' | LC_ALL=C sort)"
    if [ -n "$files" ]; then
      while IFS= read -r f; do
        cat "$f"
        echo
      done <<< "$files"
    else
      if ! printf '%s\n' "$license" | grep -qE '^\(?(MIT|Apache-2\.0)( OR (MIT|Apache-2\.0))*\)?$' \
        || ! printf '%s\n' "$license" | grep -qw MIT; then
        die "$name has no license file and its license $license does not offer MIT"
      fi
      copyright=""
      if [ -s "$dir/LICENSE.spdx" ]; then
        copyright="$(sed -n 's/^PackageCopyrightText: //p' "$dir/LICENSE.spdx" | head -n 1)"
      fi
      echo "The package has no license file. Its license is $license. Notedown"
      echo "uses it under the MIT license. The MIT text is in part 1 above."
      fallback=1
      if [ -n "$copyright" ]; then
        echo "Copyright $copyright"
      fi
      echo
    fi
  done < "$set"
}

header() {
  echo "Third-party notices for Notedown"
  echo "================================"
  echo
  echo "Notedown is licensed under the GNU Affero General Public License v3.0 or"
  echo "later (AGPL-3.0-or-later), see LICENSE in the source repository:"
  echo "https://github.com/jordiboehme/notedown"
  echo
}

# macos_body <out>: the crates and bundled code of the macOS notedown binary.
macos_body() {
  local out="$1" blocks="$work/macos-blocks.txt"
  cargo_notices "$root/crates/notedown/Cargo.toml" "$root/about.toml" "$blocks" \
    --target aarch64-apple-darwin
  {
    echo "The notedown program is built from the Rust crates listed below. For each"
    echo "license, this file names the crates that use it and gives the license"
    echo "text. The C and C++ code built into the program follows at the end."
    echo
    cat "$blocks"
    echo
    bundled_code "$root/licenses/aws-lc.txt"
    echo
    bundled_code "$root/licenses/bundled-code.txt"
  } > "$out"
}

main() {
  [ "$#" -eq 2 ] || usage
  local variant="$1" out="$2"
  [ -n "$out" ] || usage
  case "$variant" in
    macos | linux | app) ;;
    *) usage ;;
  esac

  root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
  local body="$work/body.txt"

  case "$variant" in
    macos)
      macos_body "$work/macos.txt"
      { header; cat "$work/macos.txt"; } > "$body"
      ;;
    linux)
      cargo_notices "$root/crates/notedown/Cargo.toml" "$root/about.toml" "$work/linux.txt" \
        --no-default-features \
        --target x86_64-unknown-linux-musl \
        --target aarch64-unknown-linux-musl
      {
        header
        echo "The notedown program is built from the Rust crates listed below. For each"
        echo "license, this file names the crates that use it and gives the license"
        echo "text. The license file of AWS-LC (C code in the aws-lc-sys crate)"
        echo "follows at the end."
        echo
        cat "$work/linux.txt"
        echo
        bundled_code "$root/licenses/aws-lc.txt"
      } > "$body"
      ;;
    app)
      local app="$root/app"
      local dist="$app/dist"
      local modules="$app/node_modules"
      local set="$work/npm-set.txt"
      npm_set "$app/package-lock.json" "$app/bundled-packages.txt" > "$set"
      [ -s "$set" ] || die "the npm set is empty"
      check_dist "$set" "$dist"
      npm_notices "$set" "$modules" > "$work/npm.txt"
      cargo_notices "$app/src-tauri/Cargo.toml" "$app/src-tauri/about.toml" "$work/app.txt" \
        --target aarch64-apple-darwin
      if [ "$fallback" -eq 1 ] && ! grep -qE '^.* \(MIT\)$' "$work/app.txt"; then
        die "an npm package without a license file points to the MIT text, but part 1 has none"
      fi
      macos_body "$work/macos.txt"
      {
        header
        echo "The Notedown app has three parts, each listed below with the licenses"
        echo "of the third-party code in it:"
        echo
        echo "  1. The app itself, built from Rust crates."
        echo "  2. The notedown program inside the app (Contents/MacOS/notedown)."
        echo "  3. The JavaScript and CSS packages of the app's window."
        echo
        echo
        echo "Part 1: the Notedown app"
        echo "########################"
        echo
        echo "The app is built from the Rust crates listed below. For each license,"
        echo "this file names the crates that use it and gives the license text."
        echo
        cat "$work/app.txt"
        echo
        echo "Part 2: the notedown program inside the app"
        echo "###########################################"
        echo
        cat "$work/macos.txt"
        echo
        echo "Part 3: JavaScript and CSS packages"
        echo "###################################"
        echo
        echo "The app's window is built from these npm packages."
        echo
        cat "$work/npm.txt"
      } > "$body"
      ;;
  esac

  mv "$body" "$out"
  echo "wrote $out ($variant)"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
