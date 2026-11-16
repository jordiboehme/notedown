#!/bin/sh
# Renders the menu bar (tray) icons from docs/assets/icon-menubar.svg.
#
# They are template images: black on transparent, macOS colours them for the
# menu bar. The attention variant has a small dot in the top right corner,
# with a gap around it, so it reads apart from the outline.
#
# Needs rsvg-convert (brew install librsvg). Run it from anywhere:
#
#     sh app/scripts/tray-icons.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
src="$here/../../docs/assets/icon-menubar.svg"
out="$here/../src-tauri/icons/tray"
mkdir -p "$out"

# The attention variant: the same drawing, cut out around the dot, plus the dot.
body=$(sed -e '1d' -e '$d' "$src")
attention="$out/attention.svg"
cat > "$attention" <<SVG
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 18 18" width="18" height="18">
<mask id="gap"><rect width="18" height="18" fill="#fff"/><circle cx="14.2" cy="3.8" r="4.2" fill="#000"/></mask>
<g mask="url(#gap)">
$body
</g>
<circle cx="14.2" cy="3.8" r="2.8" fill="#000"/>
</svg>
SVG

rsvg-convert -w 18 -h 18 "$src" -o "$out/icon.png"
rsvg-convert -w 36 -h 36 "$src" -o "$out/icon@2x.png"
rsvg-convert -w 18 -h 18 "$attention" -o "$out/attention.png"
rsvg-convert -w 36 -h 36 "$attention" -o "$out/attention@2x.png"
