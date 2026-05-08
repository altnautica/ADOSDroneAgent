#!/usr/bin/env bash
# Render all mockup pages to PNG at exact 480x320 px.
#
# Headless Chrome on macOS has a quirk where --window-size produces a
# viewport with some bottom-edge clipping when the body is exactly the
# requested height. We work around that by rendering at 480x500
# (giving Chrome plenty of room) and cropping the result to the
# canonical 480x320 with sips.
#
# Usage:  bash render.sh
# Override Chrome path with:  CHROME=/path/to/chromium bash render.sh

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PAGES_DIR="$HERE/pages"
OUT_DIR="$HERE/output"
TMP_DIR="$HERE/.render-tmp"

CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
if [[ ! -x "$CHROME" ]]; then
  echo "error: Chrome not found at $CHROME" >&2
  echo "set CHROME env var to a chromium binary" >&2
  exit 1
fi

if ! command -v sips >/dev/null 2>&1; then
  echo "error: sips not found (need macOS image tools)" >&2
  exit 1
fi

mkdir -p "$OUT_DIR" "$TMP_DIR"

shopt -s nullglob
for page in "$PAGES_DIR"/*.html; do
  base="$(basename "$page" .html)"
  raw="$TMP_DIR/$base.png"
  out="$OUT_DIR/$base.png"
  echo "→ $base.png"
  "$CHROME" \
    --headless \
    --disable-gpu \
    --hide-scrollbars \
    --force-device-scale-factor=1 \
    --window-size=480,500 \
    --screenshot="$raw" \
    "file://$page" \
    >/dev/null 2>&1
  # Crop top-left 480x320 from the larger render.
  python3 -c "
from PIL import Image
img = Image.open('$raw')
img.crop((0, 0, 480, 320)).save('$out')
"
done

rm -rf "$TMP_DIR"

echo
echo "rendered $(ls -1 "$OUT_DIR"/*.png 2>/dev/null | wc -l | tr -d ' ') PNGs into $OUT_DIR/"
