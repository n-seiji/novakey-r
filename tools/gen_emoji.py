"""azooKey の絵文字辞書から 読み -> 絵文字 の表 (resources/emoji.tsv) を作る。

使い方: python3 -I tools/gen_emoji.py tools/data/emoji_all_E16.0.txt > resources/emoji.tsv

入力は `絵文字<TAB>読み,読み,...<TAB>` の行。読みにはひらがなと漢字が混ざるので、
ひらがな (と長音) だけの読みを残す。読みの数が少ない絵文字ほどその読みに固有なので先に並べる
(ねこ なら 😺 より 🐈)。入力の絵文字には生成器が必ず U+FE0F を1つ足して
あるため、末尾の U+FE0F を1つだけ外す。

データは Unicode の絵文字データと CLDR の日本語注釈 (Unicode License v3) 由来で、
azooKey_emoji_dictionary_storage (commit 08f82252fb90ef8f0949a7e3c554e9e1787ce121) 経由。
"""

from __future__ import annotations

import re
import sys
from collections import defaultdict

HIRAGANA = re.compile(r"^[ぁ-ゖー]+$")
VS16 = "️"

HEADER = (
    "# Unicode, Inc. CLDR / emoji data (Unicode License v3 "
    "https://www.unicode.org/license.txt),\n"
    "# azooKey_emoji_dictionary_storage commit "
    "08f82252fb90ef8f0949a7e3c554e9e1787ce121 経由。\n"
    "# tools/gen_emoji.py で生成。手で編集しない。\n"
)


def strip_one_vs16(emoji: str) -> str:
    """末尾の U+FE0F を1つだけ外す。"""
    return emoji[:-1] if emoji.endswith(VS16) else emoji


def build_table(lines: list[str]) -> dict[str, list[str]]:
    """読み -> 絵文字 (読みの数が少ない順、同数は入力の出現順・重複なし)。"""
    ranked: dict[str, list[tuple[int, str]]] = defaultdict(list)
    for line in lines:
        cols = line.rstrip("\n").split("\t")
        if len(cols) < 2 or not cols[0]:
            continue
        emoji = strip_one_vs16(cols[0])
        readings = cols[1].split(",")
        for reading in readings:
            if HIRAGANA.match(reading) and all(e != emoji for _, e in ranked[reading]):
                ranked[reading].append((len(readings), emoji))
    return {
        r: [e for _, e in sorted(es, key=lambda x: x[0])] for r, es in ranked.items()
    }


def render(table: dict[str, list[str]]) -> str:
    rows = (f"{r}\t" + "\t".join(table[r]) for r in sorted(table))
    return HEADER + "\n".join(rows) + "\n"


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    with open(argv[1], encoding="utf-8") as f:
        table = build_table(f.readlines())
    sys.stdout.write(render(table))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
