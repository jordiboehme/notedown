#!/usr/bin/env bash
# Usage: app-sbom.sh <app-crates.cdx.json> <cli.cdx.json> <npm.cdx.json> \
#          <package-lock.json> <bundled-packages.txt> <out>
#
# Writes the CycloneDX SBOM of the Notedown app (design note 6 of the release
# plan) from three SBOMs of the release's sbom job:
#
#   app-crates  cargo cyclonedx of app/src-tauri for aarch64-apple-darwin
#   cli         cargo cyclonedx of crates/notedown for aarch64-apple-darwin
#               (the notedown program inside the app, the sidecar)
#   npm         npm sbom --sbom-format cyclonedx of app/ (every installed
#               package, dev tools included)
#
# The result is the app-crates SBOM (its metadata and root component) with
# these components, deduplicated by purl (by bom-ref when a component has no
# purl, by name@version when it has neither):
#
#   - the crates of the app,
#   - the notedown program itself (the root component of the cli SBOM) and
#     its crates,
#   - the npm packages built into app/dist: the same set as the app notices
#     (npm_set of scripts/third-party-notices.sh: the production packages of
#     package-lock.json plus bundled-packages.txt). The npm SBOM's other
#     packages (build tools) are left out.
#
# The dependencies of both cargo SBOMs are merged by ref; the app's root
# entry also names the notedown program and the npm packages. The npm SBOM's
# own dependency graph is not taken (it is the graph of the dev install).
#
# Fails when a package of the npm set is not in the npm SBOM, or when an
# input is not a CycloneDX document. Needs jq.
set -euo pipefail

[ "$#" -eq 6 ] || {
  echo "usage: $(basename "$0") <app-crates.cdx.json> <cli.cdx.json> <npm.cdx.json> <package-lock.json> <bundled-packages.txt> <out>" >&2
  exit 2
}
app="$1" cli="$2" npm="$3" lock="$4" list="$5" out="$6"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# npm_set and die; main of the notices script does not run when sourced.
# shellcheck source=SCRIPTDIR/../third-party-notices.sh
source "$here/../third-party-notices.sh"

for f in "$app" "$cli" "$npm"; do
  jq -e '.bomFormat == "CycloneDX" and (.components | type == "array")' "$f" >/dev/null \
    || die "$f is not a CycloneDX document with components"
done

set_file="$(mktemp)"
trap 'rm -f "$set_file"' EXIT
npm_set "$lock" "$list" >"$set_file"
[ -s "$set_file" ] || die "the npm set is empty"

# name@version of every package of the set, from the lock file. The npm
# SBOM's bom-ref has the same form.
pairs="$(jq -n --slurpfile lock "$lock" --rawfile set "$set_file" '
  [ $set | split("\n")[] | select(length > 0) | . as $p
    | ($lock[0].packages["node_modules/" + $p] // {}) as $e
    | if ($e.version // "") == "" then error("\($p) has no version in the lock file") else . end
    | {name: ($e.name // ($p | sub("^.*node_modules/"; ""))), version: $e.version} ]')"

jq -n \
  --slurpfile a "$app" --slurpfile c "$cli" --slurpfile n "$npm" \
  --argjson pairs "$pairs" '
  $a[0] as $app | $c[0] as $cli | $n[0] as $npm
  | ($pairs | map(. as $p
      | [$npm.components[] | select(.name == $p.name and .version == $p.version)]
      | if length == 0 then error("the npm SBOM has no \($p.name)@\($p.version)") else .[0] end)) as $packages
  | ($cli.metadata.component // error("the cli SBOM has no root component")) as $program
  | ($app.metadata.component["bom-ref"] // error("the app SBOM has no root bom-ref")) as $root
  | $app
  | .components = ([$app.components[], $program, $cli.components[], $packages[]]
      | unique_by(.purl // .["bom-ref"] // "\(.name)@\(.version)"))
  | (.components | map(select(.["bom-ref"] != null) | {(.["bom-ref"]): true}) | add // {}) as $known
  | .dependencies = (
      [ ($app.dependencies // [])[], ($cli.dependencies // [])[],
        {ref: $root, dependsOn: ([$program["bom-ref"]] + ($packages | map(.["bom-ref"])))} ]
      | group_by(.ref)
      | map({ref: .[0].ref,
             dependsOn: ([.[].dependsOn // [] | .[]] | unique | map(select($known[.] == true)))}))
' >"$out"

echo "$out: $(jq '.components | length' "$out") components, $(jq 'length' <<<"$pairs") of them npm packages"
