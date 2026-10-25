#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.12.*"
# dependencies = [
#   "fonttools",
#   "uharfbuzz",
#   "cairosvg",
#   "pillow",
# ]
#
# [tool.uv]
# # Supply-chain guard: refuse package versions published less than 7 days ago.
# exclude-newer = "7 days"
# ///
"""Shizuku のロゴ一式を書き出す。

  docs/assets/mark.svg                 マーク単体 (ライト色)
  docs/assets/wordmark-{light,dark}.svg  マーク + "shizuku" (文字はアウトライン化済み、フォント非依存)
  resources/app.icns                   アプリアイコン (角丸の正方形に白いしずく)
  resources/logo.tiff                  入力メニューのアイコン (黒のテンプレート画像、16px と 32px @2x)

マークは水滴 1 つと縦棒 1 本。縦棒は水滴のハイライトであり入力カーソルでもあり、
背景色で塗らずに水滴から抜く (even-odd の複合パス)。
文字は Quicksand (SIL OFL 1.1) の wght=500 を HarfBuzz でシェイプ (カーニング込み) して
パスにする。フォントは google/fonts の固定コミットから取り、sha256 を確かめる。
iconutil と tiffutil (macOS 付属) が要る。
"""
import hashlib
import io
import os
import subprocess
import sys
import tempfile
import urllib.request
from pathlib import Path

# cairosvg は libcairo を動的に探す。Homebrew の cairo を見つけられるよう、
# 起動時に fallback パスを足して自分を起動し直す。
_BREW = "/opt/homebrew/lib"
if sys.platform == "darwin" and os.path.isdir(_BREW) and _BREW not in os.environ.get("DYLD_FALLBACK_LIBRARY_PATH", ""):
    env = dict(os.environ)
    env["DYLD_FALLBACK_LIBRARY_PATH"] = _BREW + ":" + env.get("DYLD_FALLBACK_LIBRARY_PATH", "/usr/lib")
    os.execve(sys.executable, [sys.executable, *sys.argv], env)

import cairosvg  # noqa: E402
import uharfbuzz as hb  # noqa: E402
from fontTools.pens.boundsPen import BoundsPen  # noqa: E402
from fontTools.pens.svgPathPen import SVGPathPen  # noqa: E402
from fontTools.pens.transformPen import TransformPen  # noqa: E402
from fontTools.ttLib import TTFont  # noqa: E402
from fontTools.varLib import instancer  # noqa: E402
from PIL import Image  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent

FONT_COMMIT = "e7db6ca603fd4862010383ff17ffd6cc4ffac1c0"  # google/fonts
FONT_URL = f"https://raw.githubusercontent.com/google/fonts/{FONT_COMMIT}/ofl/quicksand/Quicksand%5Bwght%5D.ttf"
FONT_SHA256 = "39c9b64223561f56aaff6062a6f04063c4fc86809ad6768722c06614d977e1cc"
WEIGHT = 500
WORD = "shizuku"

# --- the mark (viewBox 0 0 100 125) ---
DROP = "M47.6 13.4 Q50 9.8 52.4 13.4 L82.8 57.1 A40 40 0 1 1 17.2 57.1 Z"
BAR = (58.4, 67.2, 11.2, 33.6, 5.6)  # x y w h r
MARK_W, MARK_H = 100, 125
# しずくの見える範囲 (先端 y=9.8 〜 円の底 y=120、円の左右 x=10〜90)
DROP_BOX = (10.0, 9.8, 90.0, 120.0)

BLUE = "#2F7FD0"
BLUE_DARK = "#6FB2FF"
INK_LIGHT = "#2A2925"
INK_DARK = "#EEEBE4"


def bar_path() -> str:
    x, y, w, h, r = BAR
    return (
        f"M{x + r:g} {y:g}H{x + w - r:g}A{r:g} {r:g} 0 0 1 {x + w:g} {y + r:g}V{y + h - r:g}"
        f"A{r:g} {r:g} 0 0 1 {x + w - r:g} {y + h:g}H{x + r:g}A{r:g} {r:g} 0 0 1 {x:g} {y + h - r:g}"
        f"V{y + r:g}A{r:g} {r:g} 0 0 1 {x + r:g} {y:g}Z"
    )


MARK_D = DROP + bar_path()  # fill-rule="evenodd" で縦棒が抜ける


def num(v: float) -> str:
    s = f"{v:.2f}".rstrip("0").rstrip(".")
    return "0" if s in ("-0", "") else s


def fetch_font() -> bytes:
    with urllib.request.urlopen(FONT_URL, timeout=60) as r:
        data = r.read()
    got = hashlib.sha256(data).hexdigest()
    if got != FONT_SHA256:
        sys.exit(f"Quicksand sha256 mismatch: got {got}, want {FONT_SHA256}")
    return data


def outline_word(font_bytes: bytes, font_size: float):
    """WORD を baseline y=0、pen 原点 x=0 のパスにする。(d, ink_bbox) を返す。"""
    tt = instancer.instantiateVariableFont(TTFont(io.BytesIO(font_bytes)), {"wght": WEIGHT})
    buf = io.BytesIO()
    tt.save(buf)
    raw = buf.getvalue()
    tt = TTFont(io.BytesIO(raw))
    upem = tt["head"].unitsPerEm
    hbfont = hb.Font(hb.Face(hb.Blob(raw)))
    hbbuf = hb.Buffer()
    hbbuf.add_str(WORD)
    hbbuf.guess_segment_properties()
    hb.shape(hbfont, hbbuf, {"kern": True, "liga": True})
    order = tt.getGlyphOrder()
    gs = tt.getGlyphSet()
    k = font_size / upem
    pen = SVGPathPen(gs, ntos=num)
    bounds = BoundsPen(gs)
    x = 0.0
    for info, pos in zip(hbbuf.glyph_infos, hbbuf.glyph_positions):
        name = order[info.codepoint]
        t = (k, 0, 0, -k, (x + pos.x_offset) * k, -pos.y_offset * k)
        gs[name].draw(TransformPen(pen, t))
        gs[name].draw(TransformPen(bounds, t))
        x += pos.x_advance
    return pen.getCommands(), bounds.bounds


def mark_svg_body(fill: str, scale: float, tx: float, ty: float) -> str:
    return (
        f'<path transform="translate({num(tx)} {num(ty)}) scale({num(scale)})" '
        f'fill="{fill}" fill-rule="evenodd" d="{MARK_D}"/>'
    )


def wordmark(font_bytes: bytes, mark_fill: str, ink: str) -> str:
    fs = 56.0
    s = 32 / MARK_W  # 56px の文字で幅 32、高さ 40
    gap = 14.0
    word_d, (wx0, wy0, wx1, wy1) = outline_word(font_bytes, fs)
    # マークはしずくの底を文字のベースライン (y=0) に置く。丸い字と同じく 0.4 だけはみ出させる。
    mark_ty = 0.4 - DROP_BOX[3] * s
    text_x = MARK_W * s + gap
    # 見えている範囲に合わせた tight な viewBox (アンチエイリアス用に 1 だけ余白)
    x0 = min(DROP_BOX[0] * s, text_x + wx0) - 1
    x1 = text_x + wx1 + 1
    y0 = min(mark_ty + DROP_BOX[1] * s, wy0) - 1
    y1 = max(0.4, wy1) + 1
    vb = f"{num(x0)} {num(y0)} {num(x1 - x0)} {num(y1 - y0)}"
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vb}" role="img" aria-label="Shizuku">'
        f"<title>Shizuku</title>"
        f"{mark_svg_body(mark_fill, s, 0, mark_ty)}"
        f'<path transform="translate({num(text_x)} 0)" fill="{ink}" d="{word_d}"/>'
        f"</svg>\n"
    )


def mark_only(fill: str) -> str:
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {MARK_W} {MARK_H}" role="img" aria-label="Shizuku">'
        f'<title>Shizuku</title><path fill="{fill}" fill-rule="evenodd" d="{MARK_D}"/></svg>\n'
    )


def drop_in_box(fill: str, box_x: float, box_y: float, box_h: float, canvas: int, extra: str = "") -> str:
    """しずくの見える範囲の高さが box_h になるよう、(box_x, box_y) を中心に置く。"""
    x0, y0, x1, y1 = DROP_BOX
    k = box_h / (y1 - y0)
    tx = box_x - (x0 + x1) / 2 * k
    ty = box_y - (y0 + y1) / 2 * k
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{canvas}" height="{canvas}" viewBox="0 0 {canvas} {canvas}">'
        f"{extra}{mark_svg_body(fill, k, tx, ty)}</svg>"
    )


def app_icon_svg() -> str:
    c = 1024
    body = 824  # macOS のアイコンは周囲に 100px 余白
    m = (c - body) / 2
    r = body * 0.2237
    bg = f'<rect x="{m}" y="{m}" width="{body}" height="{body}" rx="{r:.1f}" fill="{BLUE}"/>'
    return drop_in_box("#FFFFFF", c / 2, c / 2, body * 0.55, c, bg)


def menu_icon_svg(px: int) -> str:
    return drop_in_box("#000000", px / 2, px / 2, px * 0.875, px)


def png(svg: str, size: int) -> bytes:
    return cairosvg.svg2png(bytestring=svg.encode(), output_width=size, output_height=size)


def write_icns(path: Path) -> None:
    svg = app_icon_svg()
    with tempfile.TemporaryDirectory() as td:
        iconset = Path(td) / "app.iconset"
        iconset.mkdir()
        for base in (16, 32, 128, 256, 512):
            (iconset / f"icon_{base}x{base}.png").write_bytes(png(svg, base))
            (iconset / f"icon_{base}x{base}@2x.png").write_bytes(png(svg, base * 2))
        subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(path)], check=True)


def write_menu_tiff(path: Path) -> None:
    with tempfile.TemporaryDirectory() as td:
        one, two = Path(td) / "1x.tiff", Path(td) / "2x.tiff"
        for out, px, dpi in ((one, 16, 72), (two, 32, 144)):
            img = Image.open(io.BytesIO(png(menu_icon_svg(px), px))).convert("RGBA")
            img.save(out, dpi=(dpi, dpi))
        subprocess.run(["tiffutil", "-cathidpicheck", str(one), str(two), "-out", str(path)], check=True)


def main() -> None:
    font = fetch_font()
    assets = ROOT / "docs" / "assets"
    assets.mkdir(parents=True, exist_ok=True)
    (assets / "wordmark-light.svg").write_text(wordmark(font, BLUE, INK_LIGHT))
    (assets / "wordmark-dark.svg").write_text(wordmark(font, BLUE_DARK, INK_DARK))
    (assets / "mark.svg").write_text(mark_only(BLUE))
    write_icns(ROOT / "resources" / "app.icns")
    write_menu_tiff(ROOT / "resources" / "logo.tiff")
    print("wrote docs/assets/{mark,wordmark-light,wordmark-dark}.svg resources/{app.icns,logo.tiff}")


if __name__ == "__main__":
    main()
