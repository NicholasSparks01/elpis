#!/usr/bin/env python3
"""Regenerate the app icons from the master in assets/elpis-icon.png.

    python scripts/make-icons.py        (needs Pillow: pip install Pillow)

The master is a 512 px RGBA tile (an amphora in a hexagon on olive, rounded corners, transparent outside).
This writes the sizes Tauri bundles (crates/elpis-gui/icons/*, including a multi-size icon.ico whose entries
are PNG-compressed, which Windows Vista and later read) and the GUI's logo (crates/elpis-gui/ui/logo.png).
Only downscales; small sizes get a little sharpening so the thin hexagon line stays readable.
"""
import io
import struct
from pathlib import Path

from PIL import Image, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
MASTER = ROOT / "assets" / "elpis-icon.png"
ICONS = ROOT / "crates" / "elpis-gui" / "icons"
UI = ROOT / "crates" / "elpis-gui" / "ui"

master = Image.open(MASTER).convert("RGBA")
if master.width != master.height or master.width < 256:
    raise SystemExit(f"{MASTER} should be a square image of at least 256 px (it is {master.size})")


def sized(n: int) -> Image.Image:
    if n == master.width:
        return master.copy()
    out = master.resize((n, n), Image.LANCZOS)
    if n <= 48:
        out = out.filter(ImageFilter.UnsharpMask(radius=0.8, percent=60, threshold=0))
    return out


ICONS.mkdir(parents=True, exist_ok=True)
for name, n in [("32x32.png", 32), ("128x128.png", 128), ("128x128@2x.png", 256), ("256x256.png", 256), ("icon.png", 512)]:
    sized(n).save(ICONS / name, optimize=True)

sizes = [16, 24, 32, 48, 64, 128, 256]
blobs = []
for n in sizes:
    buf = io.BytesIO()
    sized(n).save(buf, "PNG", optimize=True)
    blobs.append(buf.getvalue())
header = struct.pack("<HHH", 0, 1, len(sizes))
offset = 6 + 16 * len(sizes)
entries = b""
for n, blob in zip(sizes, blobs):
    entries += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(blob), offset)  # 256 is stored as 0
    offset += len(blob)
(ICONS / "icon.ico").write_bytes(header + entries + b"".join(blobs))

sized(128).save(UI / "logo.png", optimize=True)
print("wrote", ICONS.relative_to(ROOT), "and", (UI / "logo.png").relative_to(ROOT))
