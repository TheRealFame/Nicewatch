#!/usr/bin/env python3
"""Generate the Nicewatch app icons: a single bold 'N' on a dark rounded
square, downscaled to the full size set.  Classic DejaVu Sans Bold — no
gradients, no chrome, matching the GUI's flat aesthetic."""
from PIL import Image, ImageDraw, ImageFont
import os

SIZES = [16, 32, 48, 64, 128, 256, 512]
BG = (26, 29, 35)          # #1a1d23 sidebar
FG = (244, 244, 246)       # #f4f4f6 near-white
MASTER = 512
RADIUS = 104
OUT = os.path.normpath(os.path.join(os.path.dirname(__file__), "..", "gui", "src-tauri", "icons"))
FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"

img = Image.new("RGBA", (MASTER, MASTER), (0, 0, 0, 0))
d = ImageDraw.Draw(img)
d.rounded_rectangle(
    [28, 28, MASTER - 28, MASTER - 28],
    radius=RADIUS,
    fill=BG,
)
font = ImageFont.truetype(FONT, 430)
bbox = d.textbbox((0, 0), "N", font=font)
w, h = bbox[2] - bbox[0], bbox[3] - bbox[1]
d.text(
    ((MASTER - w) / 2 - bbox[0], (MASTER - h) / 2 - bbox[1]),
    "N",
    font=font,
    fill=FG,
)

os.makedirs(OUT, exist_ok=True)
for s in SIZES:
    img.resize((s, s), Image.LANCZOS).save(os.path.join(OUT, f"icon-{s}.png"))
img.save(os.path.join(OUT, "icon.png"))
img.resize((32, 32), Image.LANCZOS).save(os.path.join(OUT, "32x32.png"))
img.resize((128, 128), Image.LANCZOS).save(os.path.join(OUT, "128x128.png"))
img.resize((256, 256), Image.LANCZOS).save(os.path.join(OUT, "128x128@2x.png"))
img.resize((256, 256), Image.LANCZOS).save(
    os.path.join(OUT, "icon.ico"), sizes=[(16, 16), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]
)
print("wrote", OUT)