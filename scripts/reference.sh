#!/usr/bin/env bash
# Builds a local supernotelib 0.7.3 venv (the reference implementation the
# parser is ported from) and regenerates the golden renderings.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
venv="$root/target/reference-venv"
[ -x "$venv/bin/python" ] || python3 -m venv "$venv"
"$venv/bin/pip" -q install "supernotelib==0.7.3"
mkdir -p "$root/tests/fixtures/golden"
"$venv/bin/python" - "$root" <<'EOF'
import sys
from supernotelib import load_notebook
from supernotelib.converter import ImageConverter, VisibilityOverlay
root = sys.argv[1]
import os
for n, path, gold in [
    ("test1", f"{root}/tests/fixtures/notes/test1.note", f"{root}/tests/fixtures/golden"),
    ("test2", f"{root}/tests/fixtures/notes/test2.note", f"{root}/tests/fixtures/golden"),
    ("bike", f"{root}/tests/fixtures/local/bike.note", f"{root}/tests/fixtures/local/golden"),
    ("content", f"{root}/tests/fixtures/local/content.note", f"{root}/tests/fixtures/local/golden"),
]:
    if not os.path.exists(path):
        continue
    os.makedirs(gold, exist_ok=True)
    nb = load_notebook(path)
    conv = ImageConverter(nb)
    for p in range(nb.get_total_pages()):
        conv.convert(p).convert("RGBA").save(f"{gold}/{n}-p{p+1}-bg.png")
        conv.convert(p, visibility_overlay={"BGLAYER": VisibilityOverlay.INVISIBLE}).convert("RGBA").save(
            f"{gold}/{n}-p{p+1}-nobg.png")
EOF
echo "reference sources: $(ls -d "$venv"/lib/python*/site-packages/supernotelib)"
