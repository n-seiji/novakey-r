"""README のタイル (docs/assets/tile-*.svg) を書き出す。nuthatch と同じ 480x360 の暗い角丸、3 コマのアニメーション。"""
from pathlib import Path

JP = "Hiragino Sans, 'Hiragino Kaku Gothic ProN', 'Noto Sans JP', system-ui, sans-serif"
MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
FG = "#f5f5f7"
INK = "#121214"
BLUE = "#5b8cff"

HEAD = f'''<svg xmlns="http://www.w3.org/2000/svg" width="240" height="180" viewBox="0 0 480 360" fill="none" role="img" aria-label="{{label}}">
  <style>
    .f {{{{ opacity: 0; animation: show 4.8s infinite; }}}}
    @keyframes show {{{{ 0% {{{{ opacity: 0 }}}} 3% {{{{ opacity: 1 }}}} 33.33% {{{{ opacity: 1 }}}} 36.33% {{{{ opacity: 0 }}}} 100% {{{{ opacity: 0 }}}} }}}}
    @media (prefers-reduced-motion: reduce) {{{{ .f {{{{ animation: none }}}} .f{{last}} {{{{ opacity: 1 }}}} }}}}
    .f0 {{{{ animation-delay: -4.80s; }}}}
    .f1 {{{{ animation-delay: -3.20s; }}}}
    .f2 {{{{ animation-delay: -1.60s; }}}}
  </style>
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#1f1f23"/>
      <stop offset="1" stop-color="#121214"/>
    </linearGradient>
  </defs>
  <rect width="480" height="360" rx="44" fill="url(#bg)"/>
  <rect x="0.5" y="0.5" width="479" height="359" rx="43.5" stroke="#ffffff" stroke-opacity="0.08"/>
  <rect x="36" y="40" width="408" height="280" rx="18" fill="#151517" stroke="#ffffff" stroke-opacity="0.25" stroke-width="1.5"/>
'''


def head(label: str) -> str:
    return HEAD.format(label=label, last=2).replace("{{", "{").replace("}}", "}")


def eyebrow(text: str) -> str:
    return f'<text x="62" y="84" font-family="{MONO}" font-size="15" letter-spacing="2" fill="{FG}" fill-opacity="0.45">{text}</text>'


def line(text: str, y: int = 150, state: str = "composing", size: int = 30, width: int = 0) -> str:
    """state: composing (点線の下線) / converting (実線の下線) / done (下線なし + キャレット)。"""
    out = f'<text x="62" y="{y}" font-family="{JP}" font-size="{size}" fill="{FG}">{text}</text>'
    if state == "composing":
        out += f'<line x1="62" y1="{y + 10}" x2="{62 + width}" y2="{y + 10}" stroke="{FG}" stroke-opacity="0.6" stroke-width="2" stroke-dasharray="4 4"/>'
    elif state == "converting":
        out += f'<line x1="62" y1="{y + 10}" x2="{62 + width}" y2="{y + 10}" stroke="{FG}" stroke-width="2.5"/>'
    else:
        out += f'<rect x="{66 + width}" y="{y - size + 4}" width="2.5" height="{size + 2}" rx="1" fill="{BLUE}"/>'
    return out


def keys(typed: str, y: int = 290) -> str:
    return (f'<text x="62" y="{y}" font-family="{MONO}" font-size="17" fill="{FG}" fill-opacity="0.4">keys</text>'
            f'<text x="120" y="{y}" font-family="{MONO}" font-size="17" fill="{FG}" fill-opacity="0.85">{typed}</text>')


def candidates(rows: list[str], y: int = 186, selected: int = 0) -> str:
    out = f'<rect x="56" y="{y}" width="300" height="{len(rows) * 34 + 12}" rx="10" fill="#202024" stroke="#ffffff" stroke-opacity="0.18"/>'
    for i, text in enumerate(rows):
        ry = y + 6 + i * 34
        if i == selected:
            out += f'<rect x="62" y="{ry}" width="288" height="32" rx="7" fill="{FG}"/>'
        color = INK if i == selected else FG
        out += (f'<text x="76" y="{ry + 22}" font-family="{MONO}" font-size="15" fill="{color}" fill-opacity="0.55">{i + 1}</text>'
                f'<text x="100" y="{ry + 23}" font-family="{JP}" font-size="19" fill="{color}">{text}</text>')
    return out


def frame(i: int, *parts: str) -> str:
    return f'  <g class="f f{i}">\n    ' + "\n    ".join(parts) + "\n  </g>\n"


def tile(label: str, frames: list[str]) -> str:
    return head(label) + "".join(frames) + "</svg>\n"


TILES = {
    "tile-convert.svg": tile("Space で変換し、Enter で確定する", [
        frame(0, eyebrow("TYPING"), line("あしたははれです", width=240), keys("ashitahaharedesu")),
        frame(1, eyebrow("SPACE"), line("明日は晴れです", state="converting", width=210),
              candidates(["明日は晴れです", "ashitahaharedesu", "明日は腫れです"])),
        frame(2, eyebrow("ENTER"), line("明日は晴れです", state="done", width=210), keys("⏎")),
    ]),
    "tile-english.svg": tile("英語はモードを切り替えずにそのまま打てる", [
        frame(0, eyebrow("TYPING"), line("ぎ", width=30), keys("gi")),
        frame(1, eyebrow("TYPING"), line("git", width=44), keys("git")),
        frame(2, eyebrow("SPACE IS A SPACE"), line("git merge", width=136), keys("git merge")),
    ]),
    "tile-mixed.svg": tile("英単語のあとの助詞はかなになる", [
        frame(0, eyebrow("TYPING"), line("artifact", width=118), keys("artifact")),
        frame(1, eyebrow("TYPING"), line("artifactを", width=150), keys("artifactwo")),
        frame(2, eyebrow("SPACE"), line("artifactを使う", state="converting", width=210),
              candidates(["artifactを使う", "artifactwotsukau", "artifactヲ使う"])),
    ]),
}

out = Path(__file__).resolve().parent.parent / "docs" / "assets"
for name, svg in TILES.items():
    (out / name).write_text(svg, encoding="utf-8")
print("wrote", ", ".join(TILES))
