#!/usr/bin/env bash
# Usage: homebrew.sh formula|cask <version> <SHA256SUMS>
#
# Prints a file of the tap jordiboehme/tap to stdout for the release
# v<version>:
#
# - formula: Formula/notedown.rb, the CLI from notedown-v<version>-macos.tar.gz.
# - cask: Casks/notedown-app.rb, the app from notedown-app-v<version>-macos.dmg.
#
# Both are Apple silicon only (arm64) and need macOS 13 (Ventura). The sum
# comes from <SHA256SUMS> as collect writes it (`<sum>  <name>`, or
# `<sum> *<name>`).
#
# The version is the tag without the v: <major>.<minor>.<patch>, optionally
# with a pre-release suffix of letters, digits and dots (0.0.0-dev in the dry
# run). Anything else is refused: the version is written into Ruby source, so
# only these characters may reach it.
#
# The file needs exactly one line for the asset, and its sum must be 64
# lowercase hex characters, as sha256sum writes it. A missing line, a second
# line or a bad sum is an error and nothing is printed, so the caller never
# gets a file with an empty or wrong sum.
#
# The cask stops and removes the "Start at login" LaunchAgent. The app turns
# it on through tauri-plugin-autostart (MacosLauncher::LaunchAgent) without a
# custom app name, so auto-launch names the job after the product name:
# label "Notedown" in ~/Library/LaunchAgents/Notedown.plist. The zap removes
# what the app keeps under ~/Library, never the data folder ~/notedown-data.
#
# Exit 0 with the file on stdout. Exit 1 when SHA256SUMS lacks the one good
# sum, exit 2 on bad usage (mode, version, unreadable file). Errors go to
# stderr.
set -u
# Byte-wise ranges: [0-9a-f] must not match an upper-case letter.
export LC_ALL=C

usage() {
  echo "usage: homebrew.sh formula|cask <version> <SHA256SUMS>" >&2
  exit 2
}

[ "$#" -eq 3 ] || usage
mode="$1"
version="$2"
sums="$3"

case "$mode" in
  formula) asset="notedown-v${version}-macos.tar.gz" ;;
  cask) asset="notedown-app-v${version}-macos.dmg" ;;
  *) usage ;;
esac

version_re='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+(\.[0-9A-Za-z]+)*)?$'
if ! [[ "$version" =~ $version_re ]]; then
  echo "homebrew.sh: the version must look like 1.2.3 or 1.2.3-rc.1 (no v), got '$version'" >&2
  exit 2
fi

if [ ! -f "$sums" ] || [ ! -r "$sums" ]; then
  echo "homebrew.sh: cannot read the checksum file '$sums'" >&2
  exit 2
fi

# The name is compared as a whole string, never as a pattern. A line is
# `<sum>` and then two characters: a space, and a space or `*`.
found="$(awk -v name="$asset" '
  {
    sep = substr($0, length($1) + 1, 2)
    if ((sep == "  " || sep == " *") && substr($0, length($1) + 3) == name) {
      n++
      s = $1
    }
  }
  END { printf "%d %s\n", n, s }
' "$sums")"
count="${found%% *}"
sha="${found#* }"
if [ "$count" -eq 0 ]; then
  echo "homebrew.sh: $sums has no sum for $asset" >&2
  exit 1
fi
if [ "$count" -gt 1 ]; then
  echo "homebrew.sh: $sums has $count lines for $asset, expected one" >&2
  exit 1
fi
if ! [[ "$sha" =~ ^[0-9a-f]{64}$ ]]; then
  echo "homebrew.sh: the sum for $asset in $sums is not 64 lowercase hex characters: '$sha'" >&2
  exit 1
fi

repo=https://github.com/jordiboehme/notedown
bundle_id=io.github.jordiboehme.notedown

if [ "$mode" = formula ]; then
  # The url names the version as is, so the formula does not depend on the
  # order of `version` and `url`.
  cat <<FORMULA
class Notedown < Formula
  desc "Turn Supernote handwritten notes into Markdown"
  homepage "${repo}"
  url "${repo}/releases/download/v${version}/${asset}"
  version "${version}"
  sha256 "${sha}"
  license "AGPL-3.0-or-later"

  depends_on arch: :arm64
  depends_on macos: :ventura

  def install
    bin.install "notedown"
    prefix.install "THIRD-PARTY-NOTICES.txt"
  end

  test do
    assert_match "notedown", shell_output("#{bin}/notedown --version")
  end
end
FORMULA
  exit 0
fi

# #{version} is Ruby interpolation, written as is.
cat <<CASK
cask "notedown-app" do
  version "${version}"
  sha256 "${sha}"

  url "${repo}/releases/download/v#{version}/notedown-app-v#{version}-macos.dmg"
  name "Notedown"
  desc "Menu bar app that turns Supernote handwritten notes into Markdown"
  homepage "${repo}"

  auto_updates true
  depends_on arch: :arm64
  depends_on macos: :ventura

  app "Notedown.app"

  uninstall launchctl: "Notedown",
            quit:      "${bundle_id}"

  zap trash: [
    "~/Library/Application Support/${bundle_id}",
    "~/Library/Caches/${bundle_id}",
    "~/Library/LaunchAgents/Notedown.plist",
    "~/Library/Preferences/${bundle_id}.plist",
    "~/Library/Saved Application State/${bundle_id}.savedState",
    "~/Library/WebKit/${bundle_id}",
  ]
end
CASK
