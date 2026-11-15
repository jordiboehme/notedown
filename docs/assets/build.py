#!/usr/bin/env python3
"""Build the notedown banner and icons ("Paper Vector" design).

Paper and ink, the look of an e-ink panel, with a C64 font for the small text.

Run it from anywhere:

    python3 docs/assets/build.py

It needs Pillow 11 and fontTools (pip install pillow fonttools), plus
rsvg-convert (brew install librsvg) and iconutil (part of macOS) for the
icon PNGs and the .icns file.

The fonts are not part of the repository. The script downloads them from the
google/fonts repository on GitHub into a temporary folder:

    Orbitron (variable, used at weight 900)
      https://github.com/google/fonts/raw/main/ofl/orbitron/Orbitron%5Bwght%5D.ttf
    Press Start 2P
      https://github.com/google/fonts/raw/main/ofl/pressstart2p/PressStart2P-Regular.ttf

Both are under the SIL Open Font License 1.1. To use fonts you already have,
pass their folder (with Orbitron.ttf and PressStart2P.ttf in it):

    python3 docs/assets/build.py /path/to/fonts

It writes next to this file:

    banner.gif          animated banner, 1280x320, 2 colours, 9 s loop
    banner-static.png   first frame of the banner (for reduced motion)
    icon.svg            app icon source, all text as paths
    icon-menubar.svg    menu bar template icon
    icon.iconset/       all 10 macOS icon sizes
    icon-1024.png
    notedown.icns

How the banner is drawn: every frame is painted in greys at 640x160 (4x
oversampled, then reduced, which gives the anti-aliasing of a canvas),
then an 8x8 Bayer matrix turns the greys into paper or ink. The result is
scaled up 2x with nearest neighbour. The loop is 9 s at 15 frames per
second: the tablet makes one whole turn, the grid moves by 8 lines and the
cursor blinks 9 times, so the last frame leads straight into the first.
"""

import math
import shutil
import subprocess
import sys
import tempfile
import urllib.request
from pathlib import Path

from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont
from PIL import Image, ImageChops, ImageDraw, ImageFilter, ImageFont

OUT = Path(__file__).resolve().parent

FONTS = {
    "Orbitron.ttf": "https://github.com/google/fonts/raw/main/ofl/orbitron/Orbitron%5Bwght%5D.ttf",
    "PressStart2P.ttf": "https://github.com/google/fonts/raw/main/ofl/pressstart2p/PressStart2P-Regular.ttf",
}

PAPER = "#e9e6dc"
INK = "#1d1c1a"
PAPER_RGB = (0xE9, 0xE6, 0xDC)
INK_RGB = (0x1D, 0x1C, 0x1A)

# Banner geometry (canvas pixels before the 2x scale).
W, H = 640, 160
HZ = 106            # horizon
SS = 4              # oversampling factor
LOOP = 9            # seconds
FPS = 15
FRAMES = LOOP * FPS
START = 18          # first frame is t = 18 / 15 = 1.2 s
OMEGA = 2 * math.pi / LOOP      # one turn per loop (about 0.7 rad/s)
GRID_LINES_PER_LOOP = 8         # grid speed 8/9 lines per second

B8 = [
    0, 32, 8, 40, 2, 34, 10, 42,
    48, 16, 56, 24, 50, 18, 58, 26,
    12, 44, 4, 36, 14, 46, 6, 38,
    60, 28, 52, 20, 62, 30, 54, 22,
    3, 35, 11, 43, 1, 33, 9, 41,
    51, 19, 59, 27, 49, 17, 57, 25,
    15, 47, 7, 39, 13, 45, 5, 37,
    63, 31, 55, 23, 61, 29, 53, 21,
]

BOX = [(-1, -1.4, -.1), (1, -1.4, -.1), (1, 1.4, -.1), (-1, 1.4, -.1),
       (-1, -1.4, .1), (1, -1.4, .1), (1, 1.4, .1), (-1, 1.4, .1)]
EDGES = [(0, 1), (1, 2), (2, 3), (3, 0), (4, 5), (5, 6), (6, 7), (7, 4),
         (0, 4), (1, 5), (2, 6), (3, 7)]


def fetch_fonts(folder):
    folder = Path(folder)
    for name, url in FONTS.items():
        target = folder / name
        if not target.exists():
            print(f"downloading {name}")
            with urllib.request.urlopen(url) as r, open(target, "wb") as f:
                shutil.copyfileobj(r, f)
    return folder


def g(v):
    """Grey level like the canvas g(): 0 is ink, 1 is paper."""
    return round(max(0.0, min(1.0, v)) * 255)


# ---------------------------------------------------------------- banner

def rot(p, a, b):
    x, y, z = p
    x1 = x * math.cos(a) + z * math.sin(a)
    z1 = -x * math.sin(a) + z * math.cos(a)
    y1 = y * math.cos(b) - z1 * math.sin(b)
    z2 = y * math.sin(b) + z1 * math.cos(b)
    return (x1, y1, z2)


def pj(r, cx=520, cy=70, s=160):
    f = s / (r[2] + 4.5)
    return (cx + r[0] * f, cy + r[1] * f)


def S(p):
    """Canvas point to oversampled pixel space."""
    return (p[0] * SS, p[1] * SS)


class Banner:
    def __init__(self, fonts):
        self.logo = ImageFont.truetype(str(fonts / "Orbitron.ttf"), 46 * SS)
        self.logo.set_variation_by_axes([900])
        self.c64 = ImageFont.truetype(str(fonts / "PressStart2P.ttf"), 8 * SS)
        self.base = self._base()
        self.threshold = self._threshold()

    def _base(self):
        """Paper, horizon haze and the fan of floor lines: the same in every frame."""
        w, h = W * SS, H * SS
        img = Image.new("L", (w, h), 255)
        d = ImageDraw.Draw(img)
        top = HZ - 29
        for yy in range(top * SS, HZ * SS):
            y = (yy + 0.5) / SS
            d.line([(0, yy), (w, yy)], fill=g(1 + (0.55 - 1) * (y - top) / 29))
        # floor lines toward the vanishing point, stroked with a vertical gradient
        grad = Image.new("L", (w, h), 0)
        gd = ImageDraw.Draw(grad)
        for yy in range(HZ * SS - SS, h):
            y = (yy + 0.5) / SS
            q = max(0.0, min(1.0, (y - HZ) / (H - HZ)))
            gd.line([(0, yy), (w, yy)], fill=round(g(0.6) * (1 - q)))
        mask = Image.new("L", (w, h), 0)
        md = ImageDraw.Draw(mask)
        for i in range(-26, 27):
            md.line([S((W / 2 + i * 7, HZ)), S((W / 2 + i * 61, H))], fill=255, width=SS)
        img.paste(grad, (0, 0), mask)
        return img

    def _threshold(self):
        """Paper where grey > (B8 + 0.5) * 255 / 64, the same test as the canvas."""
        cell = Image.new("L", (8, 8))
        cell.putdata([math.floor((v + 0.5) * 255 / 64) for v in B8])
        t = Image.new("L", (W, H))
        for y in range(0, H, 8):
            for x in range(0, W, 8):
                t.paste(cell, (x, y))
        return t

    def grey(self, k):
        """Frame k of the loop in greys at SS x size. t = (START + k) / FPS."""
        n = START + k
        t = n / FPS
        img = self.base.copy()
        d = ImageDraw.Draw(img)
        # grid lines across, moving toward the viewer
        sh = (n * GRID_LINES_PER_LOOP % FRAMES) / FRAMES
        for i in range(9):
            q = (i + sh) / 9
            y = HZ + (H - HZ) * q * q
            d.line([S((0, y)), S((W, y))], fill=g(0.6 * (1 - q)), width=SS)
        d.line([S((0, HZ)), S((W, HZ))], fill=0, width=SS)
        # the turning wireframe tablet
        a = OMEGA * t
        b = 0.35 + math.sin(OMEGA * t) * 0.18
        P = [pj(rot(p, a, b)) for p in BOX]
        for i, j in EDGES:
            d.line([S(P[i]), S(P[j])], fill=0, width=round(1.5 * SS))
        zf = -.11 if rot((0, 0, -1), a, b)[2] < 0 else .11
        for r in range(5):
            yy = -0.8 + r * 0.4
            A = pj(rot((-.6, yy, zf), a, b))
            B = pj(rot((.6 - (r % 2) * .3, yy, zf), a, b))
            d.line([S(A), S(B)], fill=0, width=SS)
        # the logo block
        d.rectangle([28 * SS, 26 * SS, (28 + 318) * SS - 1, (26 + 74) * SS - 1], fill=255)
        org = (36 * SS, 70 * SS)
        # canvas: fill at 72% grey, then a 1.6 px ink stroke centred on the outline
        fill = Image.new("L", img.size, 0)
        ImageDraw.Draw(fill).text(org, "NOTEDOWN", font=self.logo, fill=255, anchor="ls")
        outer = Image.new("L", img.size, 0)
        ImageDraw.Draw(outer).text(org, "NOTEDOWN", font=self.logo, fill=255, anchor="ls",
                                   stroke_width=3, stroke_fill=255)
        inner = fill.filter(ImageFilter.MinFilter(7))
        img.paste(0, (0, 0), outer)
        img.paste(g(0.72), (0, 0), inner)
        d.text((38 * SS, 86 * SS), "YOUR HANDWRITING, READABLE BY MACHINES.",
               font=self.c64, fill=0, anchor="ls")
        d.text((38 * SS, 98 * SS), "READY.", font=self.c64, fill=0, anchor="ls")
        if (2 * n // FPS) % 2 == 0:
            d.rectangle([38 * SS, 100 * SS, 46 * SS - 1, 108 * SS - 1], fill=0)
        return img.reduce(SS)

    def frame(self, k):
        """Frame k dithered to paper (index 0) and ink (index 1), scaled 2x."""
        diff = ImageChops.subtract(self.grey(k), self.threshold)
        idx = diff.point(lambda v: 0 if v > 0 else 1)
        p = Image.frombytes("P", (W, H), idx.tobytes())
        p.putpalette(list(PAPER_RGB) + list(INK_RGB))
        return p.resize((W * 2, H * 2), Image.NEAREST)


def build_banner(fonts):
    banner = Banner(fonts)
    frames = [banner.frame(k) for k in range(FRAMES)]
    # GIF delays are in 1/100 s: 70, 60, 70 ms make exactly 15 frames per second
    durations = [(70, 60, 70)[k % 3] for k in range(FRAMES)]
    frames[0].save(OUT / "banner.gif", save_all=True, append_images=frames[1:],
                   duration=durations, loop=0, optimize=True)
    frames[0].save(OUT / "banner-static.png", optimize=True)
    # the frame after the last one must be the first one again
    seam = ImageChops.difference(banner.frame(FRAMES).convert("L"), frames[0].convert("L"))
    if seam.getbbox() is not None:
        sys.exit("banner loop has a seam")


# ---------------------------------------------------------------- icon

def glyph_path(font_path, ch, x, y, size):
    font = TTFont(str(font_path))
    gs = font.getGlyphSet()
    name = font.getBestCmap()[ord(ch)]
    scale = size / font["head"].unitsPerEm
    pen = SVGPathPen(gs, ntos=lambda v: ("%.2f" % v).rstrip("0").rstrip("."))
    gs[name].draw(TransformPen(pen, (scale, 0, 0, -scale, x, y)))
    return pen.getCommands()


def icon_svg(hash_d, small=False):
    """App icon "READY.": a frontal tablet with a hash and a solid block cursor.

    small=True is the variant for 16 and 32 px: no dither (it turns into a grey
    smear there), thicker lines, two solid text lines and the floor as two plain lines.
    """
    hz = 790
    grid = ""
    for i in range(-9, 10):
        grid += f"M{512 + i * 40} {hz} L{512 + i * 170} 1024 "
    if small:
        grid = ""
    for q in ((0.4,) if small else (0.18, 0.45, 0.8)):
        grid += f"M0 {round(hz + (1024 - hz) * q)} H1024 "
    grid = grid.strip()
    if small:
        floor = ""
        grid_paths = f'<path d="{grid}" stroke="{INK}" stroke-width="24" fill="none"/>\n'
        bar = 32
        edge = 40
        screen = ""
        lines = (f'<rect x="372" y="548" width="250" height="36" fill="{INK}"/>'
                 f'<rect x="372" y="636" width="190" height="36" fill="{INK}"/>\n')
    else:
        floor = (f'<rect x="0" y="{hz}" width="1024" height="60" fill="url(#a)"/>'
                 f'<rect x="0" y="{hz + 60}" width="1024" height="180" fill="url(#b)"/>\n')
        grid_paths = (f'<path d="{grid}" stroke="{PAPER}" stroke-width="10" fill="none"/>\n'
                      f'<path d="{grid}" stroke="{INK}" stroke-width="5" fill="none"/>\n')
        bar = 12
        edge = 24
        screen = (f'<rect x="342" y="232" width="360" height="520" rx="10" fill="none" '
                  f'stroke="{INK}" stroke-width="10"/>\n')
        lines = ('<rect x="372" y="540" width="250" height="16" fill="url(#b)"/>'
                 '<rect x="372" y="590" width="190" height="16" fill="url(#b)"/>'
                 '<rect x="372" y="640" width="230" height="16" fill="url(#b)"/>\n')
    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024" width="1024" height="1024">
<defs>
<clipPath id="k"><rect x="100" y="100" width="824" height="824" rx="185"/></clipPath>
<pattern id="a" width="16" height="16" patternUnits="userSpaceOnUse"><rect width="8" height="8" fill="{INK}"/></pattern>
<pattern id="b" width="16" height="16" patternUnits="userSpaceOnUse"><rect width="8" height="8" fill="{INK}"/><rect x="8" y="8" width="8" height="8" fill="{INK}"/></pattern>
</defs>
<g clip-path="url(#k)">
<rect width="1024" height="1024" fill="{PAPER}"/>
{floor}{grid_paths}<rect x="0" y="{hz - bar // 2}" width="1024" height="{bar}" fill="{INK}"/>
<path d="M300 188 L752 188 L772 208 L772 822 L320 822 L300 802 Z" fill="{INK}"/>
<rect x="292" y="176" width="460" height="634" rx="34" fill="{PAPER}" stroke="{INK}" stroke-width="{edge}"/>
{screen}<path d="{hash_d}" fill="{INK}"/>
<rect x="520" y="356" width="108" height="114" fill="{INK}"/>
{lines}</g>
<rect x="100" y="100" width="824" height="824" rx="185" fill="none" stroke="#c9c5b8" stroke-width="3"/>
</svg>
'''


MENUBAR = '''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 18 18" width="18" height="18">
<rect x="3.2" y="1.6" width="11.6" height="14.8" rx="2" fill="none" stroke="#000" stroke-width="1.6"/>
<path d="M7.4 5.2L6.6 12.8M11.2 5.2L10.4 12.8M5.4 7.6H12.6M5.2 10.4H12.4" fill="none" stroke="#000" stroke-width="1.3" stroke-linecap="round"/>
</svg>
'''


def rsvg(svg, size, out):
    subprocess.run(["rsvg-convert", "-w", str(size), "-h", str(size), str(svg), "-o", str(out)],
                   check=True)


def build_icons(fonts, tmp):
    hash_d = glyph_path(fonts / "PressStart2P.ttf", "#", 372, 470, 120)
    (OUT / "icon.svg").write_text(icon_svg(hash_d))
    small = Path(tmp) / "icon-small.svg"
    small.write_text(icon_svg(hash_d, small=True))
    (OUT / "icon-menubar.svg").write_text(MENUBAR)
    iconset = OUT / "icon.iconset"
    if iconset.exists():
        shutil.rmtree(iconset)
    iconset.mkdir()
    for s in (16, 32, 128, 256, 512):
        for scale, suffix in ((1, ""), (2, "@2x")):
            px = s * scale
            src = small if px <= 32 else OUT / "icon.svg"
            rsvg(src, px, iconset / f"icon_{s}x{s}{suffix}.png")
    rsvg(OUT / "icon.svg", 1024, OUT / "icon-1024.png")
    subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(OUT / "notedown.icns")],
                   check=True)


def main():
    with tempfile.TemporaryDirectory() as tmp:
        fonts = Path(sys.argv[1]) if len(sys.argv) > 1 else fetch_fonts(tmp)
        build_banner(fonts)
        build_icons(fonts, tmp)
    for name in ("banner.gif", "banner-static.png", "icon.svg", "icon-menubar.svg",
                 "icon-1024.png", "notedown.icns"):
        print(f"{name}: {(OUT / name).stat().st_size} bytes")


if __name__ == "__main__":
    main()
