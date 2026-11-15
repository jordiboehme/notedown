#!/usr/bin/env python3
"""Build the notedown banner and icon SVGs ("Vector Party" design).

The text is outlined from Orbitron (SIL Open Font License 1.1), so the SVGs
need no web fonts. The font is not part of the repository. Fetch it first:

    mkdir -p /tmp/orbitron
    curl -sSLo /tmp/orbitron/Orbitron.ttf \
      "https://github.com/google/fonts/raw/main/ofl/orbitron/Orbitron%5Bwght%5D.ttf"

Then run (needs fontTools: pip install fonttools):

    python3 docs/assets/build.py /tmp/orbitron/Orbitron.ttf

It writes banner.svg, banner-static.svg, icon.svg and icon-menubar.svg next to
this file. The PNGs and the .icns come from icon.svg:

    cd docs/assets && mkdir -p icon.iconset
    for s in 16 32 128 256 512; do
      rsvg-convert -w $s -h $s icon.svg -o icon.iconset/icon_${s}x${s}.png
      rsvg-convert -w $((s*2)) -h $((s*2)) icon.svg -o icon.iconset/icon_${s}x${s}@2x.png
    done
    rsvg-convert -w 1024 -h 1024 icon.svg -o icon-1024.png
    iconutil -c icns icon.iconset -o notedown.icns
"""

import math
import sys
from pathlib import Path

from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont

OUT = Path(__file__).resolve().parent

BG = "#05040c"
GRID = "#ff2fd1"
LINE = "#47f3ff"
TAG = "#ff9df0"


def num(v):
    s = f"{v:.1f}"
    if s.endswith(".0"):
        s = s[:-2]
    return "0" if s == "-0" else s


# ---------------------------------------------------------------- text

class Font:
    def __init__(self, path, weight):
        self.font = instantiateVariableFont(TTFont(path), {"wght": weight})
        self.glyphs = self.font.getGlyphSet()
        self.cmap = self.font.getBestCmap()
        self.upm = self.font["head"].unitsPerEm

    def advance(self, ch):
        return self.font["hmtx"][self.cmap[ord(ch)]][0]

    def width(self, text, size, tracking=0.0):
        w = sum(self.advance(c) for c in text) * size / self.upm
        return w + tracking * (len(text) - 1)

    def path(self, text, x, y, size, tracking=0.0, mapper=None):
        """Outline `text` with its baseline at (x, y). `mapper` can bend each
        point afterwards (used to put the "#" on the tilted tablet screen)."""
        pen = SVGPathPen(self.glyphs, ntos=num)
        s = size / self.upm
        for ch in text:
            name = self.cmap[ord(ch)]
            if mapper is None:
                tp = TransformPen(pen, (s, 0, 0, -s, x, y))
            else:
                tp = MapPen(pen, lambda px, py, x=x: mapper(x + px * s, y - py * s))
            self.glyphs[name].draw(tp)
            x += self.advance(ch) * s + tracking
        return pen.getCommands()


class MapPen:
    """A pen that sends every point through a function."""

    def __init__(self, out, fn):
        self.out, self.fn = out, fn

    def moveTo(self, p):
        self.out.moveTo(self.fn(*p))

    def lineTo(self, p):
        self.out.lineTo(self.fn(*p))

    def qCurveTo(self, *pts):
        self.out.qCurveTo(*[None if p is None else self.fn(*p) for p in pts])

    def curveTo(self, *pts):
        self.out.curveTo(*[self.fn(*p) for p in pts])

    def closePath(self):
        self.out.closePath()

    def endPath(self):
        self.out.endPath()

    def addComponent(self, name, transform):
        raise NotImplementedError


# ---------------------------------------------------------------- 3D tablet

BOX = [(-1, -1.4, -.08), (1, -1.4, -.08), (1, 1.4, -.08), (-1, 1.4, -.08),
       (-1, -1.4, .08), (1, -1.4, .08), (1, 1.4, .08), (-1, 1.4, .08)]
SCREEN = [(-.82, -1.15, -.09), (.82, -1.15, -.09), (.82, 1.2, -.09), (-.82, 1.2, -.09)]


def proj(p, a, b, cx, cy, s):
    x, y, z = p
    x1 = x * math.cos(a) + z * math.sin(a)
    z1 = -x * math.sin(a) + z * math.cos(a)
    y1 = y * math.cos(b) - z1 * math.sin(b)
    z2 = y * math.sin(b) + z1 * math.cos(b)
    f = s / (z2 + 4.5)
    return cx + x1 * f, cy + y1 * f


def poly(points, close=True):
    d = "M" + " ".join(f"{num(x)} {num(y)}" for x, y in points[:1])
    d += "".join(f"L{num(x)} {num(y)}" for x, y in points[1:])
    return d + ("Z" if close else "")


def body_path(a, b, cx, cy, s):
    """Front face, back face and the four side edges. The command structure
    is the same for every angle, so SMIL can interpolate between frames."""
    P = [proj(p, a, b, cx, cy, s) for p in BOX]
    d = poly(P[0:4]) + poly(P[4:8])
    for i in range(4):
        d += poly([P[i], P[i + 4]], close=False)
    return d


def screen_path(a, b, cx, cy, s):
    """Screen frame and five lines of "handwriting"."""
    d = poly([proj(p, a, b, cx, cy, s) for p in SCREEN])
    for r in range(5):
        yy = -0.8 + r * 0.4
        right = .6 - (r % 2) * .3
        d += poly([proj((-.6, yy, -.09), a, b, cx, cy, s),
                   proj((right, yy, -.09), a, b, cx, cy, s)], close=False)
    return d


# ---------------------------------------------------------------- banner

W, H = 1280, 320
HZ = 212                      # horizon
TAB = (1040, 150, 330)        # tablet centre and scale
PERIOD = 9                    # seconds for one turn
FRAMES = 72                   # keyframes per turn
A0 = 0.55                     # start angle, also the still frame
GRID_PERIOD = 0.9             # seconds per grid line
GRID_FRAMES = 16


def angles(i, n):
    t = i / n
    return A0 + 2 * math.pi * t, 0.35 + 0.2 * math.sin(2 * math.pi * t)


def grid_vertical():
    d = ""
    for i in range(-24, 25):
        d += f"M{num(640 + i * 14)} {HZ}L{num(640 + i * 120)} {H}"
    return d


def grid_horizontal(shift):
    d = ""
    for k in range(10):
        q = (k + shift) / 9
        y = HZ + (H - HZ) * q * q
        d += f"M0 {num(y)}H{W}"
    return d


def anim(attr, values, dur):
    keys = ";".join(values)
    return (f'<animate attributeName="{attr}" dur="{dur}s" repeatCount="indefinite" '
            f'calcMode="linear" values="{keys}"/>')


def banner(logo_font, tag_font, animated):
    logo_size = 104
    logo_x, logo_y = 72, 160
    logo_w = logo_font.width("NOTEDOWN", logo_size)
    tagline = "YOUR HANDWRITING, READABLE BY MACHINES."
    tag_size = 20
    tracking = (logo_w - tag_font.width(tagline, tag_size)) / (len(tagline) - 1)
    logo = logo_font.path("NOTEDOWN", logo_x, logo_y, logo_size)
    tag = tag_font.path(tagline, logo_x + 3, 202, tag_size, tracking)

    a, b = angles(0, 1)
    body = body_path(a, b, *TAB)
    scr = screen_path(a, b, *TAB)
    hlines = grid_horizontal(0)
    body_anim = scr_anim = h_anim = ""
    if animated:
        frames = [angles(i, FRAMES) for i in range(FRAMES + 1)]
        body_anim = anim("d", [body_path(a, b, *TAB) for a, b in frames], PERIOD)
        scr_anim = anim("d", [screen_path(a, b, *TAB) for a, b in frames], PERIOD)
        h_anim = anim("d", [grid_horizontal(i / GRID_FRAMES) for i in range(GRID_FRAMES + 1)],
                      GRID_PERIOD)

    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" role="img" aria-label="notedown: your handwriting, readable by machines">
<title>notedown</title>
<defs>
<filter id="glow" filterUnits="userSpaceOnUse" x="0" y="0" width="{W}" height="{H}">
<feGaussianBlur in="SourceGraphic" stdDeviation="5" result="wide"/>
<feGaussianBlur in="SourceGraphic" stdDeviation="1.6" result="near"/>
<feMerge><feMergeNode in="wide"/><feMergeNode in="near"/><feMergeNode in="SourceGraphic"/></feMerge>
</filter>
<filter id="soft" filterUnits="userSpaceOnUse" x="0" y="0" width="{W}" height="{H}">
<feGaussianBlur in="SourceGraphic" stdDeviation="3" result="b"/>
<feMerge><feMergeNode in="b"/><feMergeNode in="SourceGraphic"/></feMerge>
</filter>
<linearGradient id="sky" x1="0" y1="0" x2="0" y2="1">
<stop offset="0" stop-color="{GRID}" stop-opacity="0"/>
<stop offset="1" stop-color="{GRID}" stop-opacity=".35"/>
</linearGradient>
<linearGradient id="fade" x1="0" y1="{HZ}" x2="0" y2="{H}" gradientUnits="userSpaceOnUse">
<stop offset="0" stop-color="#fff" stop-opacity=".25"/>
<stop offset=".35" stop-color="#fff" stop-opacity=".8"/>
<stop offset="1" stop-color="#fff"/>
</linearGradient>
<mask id="floor"><rect y="{HZ}" width="{W}" height="{H - HZ}" fill="url(#fade)"/></mask>
</defs>
<rect width="{W}" height="{H}" fill="{BG}"/>
<rect y="150" width="{W}" height="{HZ - 150}" fill="url(#sky)"/>
<g fill="none" stroke="{GRID}" stroke-width="1.5" stroke-opacity=".8" mask="url(#floor)">
<path d="{grid_vertical()}"/>
<path d="{hlines}">{h_anim}</path>
</g>
<path d="M0 {HZ}H{W}" stroke="{GRID}" stroke-width="2" filter="url(#soft)"/>
<g fill="none" stroke="{LINE}" stroke-linejoin="round" stroke-linecap="round" filter="url(#glow)">
<path stroke-width="2.5" d="{body}">{body_anim}</path>
<path stroke-width="1.4" d="{scr}">{scr_anim}</path>
</g>
<path d="{logo}" fill="{LINE}" fill-opacity=".08" stroke="{LINE}" stroke-width="2.5" stroke-linejoin="round" filter="url(#glow)"/>
<path d="{tag}" fill="{TAG}" filter="url(#soft)"/>
</svg>
'''


# ---------------------------------------------------------------- icon

def icon(hash_font):
    # Apple icon grid: 824 x 824 body with a ~185 px corner radius in 1024.
    x0, size, r = 100, 824, 185
    hz = 690
    cx, cy, s = 512, 480, 840
    a, b = -0.42, 0.28

    body = body_path(a, b, cx, cy, s)
    scr = poly([proj(p, a, b, cx, cy, s) for p in SCREEN])

    # "#" drawn in screen coordinates (units of the tablet), then projected.
    hs = 1.25
    hw = hash_font.advance("#") / hash_font.upm * hs

    def on_screen(px, py):
        return proj((px, py, -.09), a, b, cx, cy, s)

    hash_d = hash_font.path("#", -hw / 2, 0.05 + 0.72 * hs / 2, hs, mapper=on_screen)

    vp = 512
    grid = ""
    for i in range(-9, 10):
        grid += f"M{num(vp + i * 30)} {hz}L{num(vp + i * 150)} {x0 + size}"
    for k in range(1, 6):
        q = k / 5
        y = hz + (x0 + size - hz) * q * q
        grid += f"M{x0} {num(y)}H{x0 + size}"

    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024" width="1024" height="1024">
<title>notedown</title>
<defs>
<clipPath id="squircle"><rect x="{x0}" y="{x0}" width="{size}" height="{size}" rx="{r}"/></clipPath>
<filter id="glow" filterUnits="userSpaceOnUse" x="0" y="0" width="1024" height="1024">
<feGaussianBlur in="SourceGraphic" stdDeviation="16" result="wide"/>
<feGaussianBlur in="SourceGraphic" stdDeviation="5" result="near"/>
<feMerge><feMergeNode in="wide"/><feMergeNode in="near"/><feMergeNode in="SourceGraphic"/></feMerge>
</filter>
<filter id="soft" filterUnits="userSpaceOnUse" x="0" y="0" width="1024" height="1024">
<feGaussianBlur in="SourceGraphic" stdDeviation="8" result="b"/>
<feMerge><feMergeNode in="b"/><feMergeNode in="SourceGraphic"/></feMerge>
</filter>
<linearGradient id="sky" x1="0" y1="{x0}" x2="0" y2="{hz}" gradientUnits="userSpaceOnUse">
<stop offset="0" stop-color="#0b0820"/>
<stop offset=".6" stop-color="{BG}"/>
<stop offset="1" stop-color="{GRID}" stop-opacity=".45"/>
</linearGradient>
<linearGradient id="fade" x1="0" y1="{hz}" x2="0" y2="{x0 + size}" gradientUnits="userSpaceOnUse">
<stop offset="0" stop-color="#fff" stop-opacity=".3"/>
<stop offset=".4" stop-color="#fff" stop-opacity=".85"/>
<stop offset="1" stop-color="#fff"/>
</linearGradient>
<mask id="floor"><rect x="{x0}" y="{hz}" width="{size}" height="{x0 + size - hz}" fill="url(#fade)"/></mask>
</defs>
<g clip-path="url(#squircle)">
<rect x="{x0}" y="{x0}" width="{size}" height="{size}" fill="{BG}"/>
<rect x="{x0}" y="{x0}" width="{size}" height="{hz - x0}" fill="url(#sky)"/>
<path d="{grid}" fill="none" stroke="{GRID}" stroke-width="7" stroke-opacity=".85" mask="url(#floor)"/>
<path d="M{x0} {hz}H{x0 + size}" stroke="{GRID}" stroke-width="8" filter="url(#soft)"/>
<g fill="none" stroke="{LINE}" stroke-linejoin="round" stroke-linecap="round" filter="url(#glow)">
<path stroke-width="22" d="{body}"/>
<path stroke-width="9" d="{scr}"/>
</g>
<path d="{hash_d}" fill="{LINE}" filter="url(#glow)"/>
</g>
<rect x="{x0 + 1.5}" y="{x0 + 1.5}" width="{size - 3}" height="{size - 3}" rx="{r - 1.5}" fill="none" stroke="#fff" stroke-opacity=".12" stroke-width="3"/>
</svg>
'''


def menubar():
    # 18 x 18 pt template: black on transparent, strokes only, no text.
    return '''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 18 18" width="18" height="18">
<g fill="none" stroke="#000" stroke-linecap="round" stroke-linejoin="round">
<path stroke-width="1.5" d="M5.25 1.75H12.75Q14.25 1.75 14.25 3.25V14.75Q14.25 16.25 12.75 16.25H5.25Q3.75 16.25 3.75 14.75V3.25Q3.75 1.75 5.25 1.75Z"/>
<path stroke-width="1.4" d="M7.6 5.5L7.2 12.5M10.8 5.5L10.4 12.5M6 7.4H12.3M5.7 10.6H12"/>
</g>
</svg>
'''


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: build.py path/to/Orbitron[wght].ttf")
    font_path = sys.argv[1]
    black = Font(font_path, 900)
    bold = Font(font_path, 800)
    (OUT / "banner.svg").write_text(banner(black, bold, animated=True))
    (OUT / "banner-static.svg").write_text(banner(black, bold, animated=False))
    (OUT / "icon.svg").write_text(icon(black))
    (OUT / "icon-menubar.svg").write_text(menubar())
    for name in ("banner.svg", "banner-static.svg", "icon.svg", "icon-menubar.svg"):
        print(f"{name}: {(OUT / name).stat().st_size} bytes")


if __name__ == "__main__":
    main()
