#!/usr/bin/env bash
#
# Decides whether a push to main releases, and looks up where a tag points.
#
# The release notes file is the switch. A release is the pull request that
# bumps the version in every pinned place and adds release-notes/v<version>.md;
# merging it releases. A version bump without its notes file does not release.
# CI's `release-check` job on main runs this after `ci-passed`:
#
#   bash scripts/release-check.sh
#
# reads the workspace version from [workspace.package] in Cargo.toml and
# answers:
#
# - release-notes/v<version>.md does not exist: nothing to release. One line,
#   release=false, exit 0. No tag lookup.
# - the notes file exists but is empty or does not open with an H1: exit 1.
# - the notes file is in place and the tag v<version> exists, here or on the
#   remote: the version was released already. release=false, exit 0.
# - the notes file is in place and the tag is new: release=true, exit 0.
#
# A tag lookup that fails (network, a remote that does not answer) exits 1.
# It never reads as "no tag", which would start a release.
#
# On GitHub Actions the answer goes to $GITHUB_OUTPUT as `tag=v<version>` and
# `release=true|false`. Locally it is only printed.
#
#   bash scripts/release-check.sh tag-commit v1.2.3
#
# prints the commit the tag points to (peeled, so an annotated tag gives its
# commit), or nothing when the tag does not exist; exit 1 on a lookup error.
#
# The remote is `origin`, or $RELEASE_CHECK_REMOTE when set.
#
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
remote="${RELEASE_CHECK_REMOTE:-origin}"

# The commit a tag points to, from the remote, peeled. Empty when the remote
# has no such tag. Returns 1 on a lookup error.
tag_commit() {
  local tag="$1" out rc=0
  out="$(git -C "$root" ls-remote --exit-code --tags "$remote" \
    "refs/tags/$tag" "refs/tags/$tag^{}")" || rc=$?
  case "$rc" in
    0) ;;
    2) return 0 ;;
    *)
      echo "::error::could not look up the tag $tag on the remote $remote (git ls-remote exit $rc)" >&2
      return 1
      ;;
  esac
  # An annotated tag lists the tag object and then the peeled commit (^{});
  # a lightweight tag lists the commit alone.
  local peeled plain
  peeled="$(awk -v r="refs/tags/$tag^{}" '$2 == r {print $1}' <<<"$out")"
  plain="$(awk -v r="refs/tags/$tag" '$2 == r {print $1}' <<<"$out")"
  printf '%s\n' "${peeled:-$plain}"
}

output() {
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    printf '%s=%s\n' "$1" "$2" >>"$GITHUB_OUTPUT"
  fi
}

if [ "${1:-}" = "tag-commit" ]; then
  [ -n "${2:-}" ] || { echo "usage: $0 tag-commit <tag>" >&2; exit 2; }
  tag_commit "$2"
  exit 0
fi
[ "$#" -eq 0 ] || { echo "usage: $0 [tag-commit <tag>]" >&2; exit 2; }

version="$(bash "$root/scripts/ci/workspace-version.sh" "$root")"
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "::error file=Cargo.toml::no workspace version of the form X.Y.Z in [workspace.package] of Cargo.toml (found '$version')" >&2
  exit 1
fi
tag="v$version"
output tag "$tag"

notes="release-notes/$tag.md"
if [ ! -f "$root/$notes" ]; then
  echo "release: $notes does not exist, so this commit does not release (the notes file is the switch)"
  output release false
  exit 0
fi
if ! head -1 "$root/$notes" | grep -qE '^# .*[^[:space:]]'; then
  echo "::error file=$notes::$notes must open with an H1 (a line starting with '# ' followed by text), the release's motto." >&2
  exit 1
fi

if git -C "$root" rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  echo "release: $tag is already tagged here, nothing to release"
  output release false
  exit 0
fi
commit="$(tag_commit "$tag")"
if [ -n "$commit" ]; then
  echo "release: $tag is already tagged on $remote (at $commit), nothing to release"
  output release false
  exit 0
fi

echo "release: $tag is new and $notes is in place, so this commit releases $tag"
output release true
