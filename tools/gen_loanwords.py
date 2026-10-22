"""JMdict_e からカタカナ語 -> 英単語の表 (resources/loanwords.tsv) を作る。

使い方: python3 -I tools/gen_loanwords.py <JMdict_e のパス> > resources/loanwords.tsv

JMdict は EDRDG の成果物 (CC BY-SA 4.0, https://www.edrdg.org/edrdg/licence.html)。
"""

from __future__ import annotations

import re
import sys
import xml.etree.ElementTree as ET
from collections import defaultdict
from typing import Iterator

XML_LANG = "{http://www.w3.org/XML/1998/namespace}lang"
KATAKANA = re.compile(r"^[ァ-ヺー]+$")
WORD = re.compile(r"[A-Za-z][A-Za-z .'-]{0,30}")
MAX_PER_READING = 3

# カタカナ -> 子音骨格。母音・ヤ行・ワ行・ハ行(ほぼ無音) は落とす。
_ROWS = {
    "k": "カキクケコ",
    "g": "ガギグゲゴ",
    "s": "サシスセソ",
    "z": "ザジズゼゾ",
    "t": "タチツテト",
    "d": "ダヂヅデド",
    "n": "ナニヌネノンマミムメモ",
    "r": "ラリルレロ",
    "b": "バビブベボヴ",
    "p": "パピプペポ",
    "f": "フ",
}
_KANA_SKELETON = {ch: c for c, chars in _ROWS.items() for ch in chars}

# 英語側の書き換え。順序が意味を持つ (ch は c より先)。
_EN_REWRITES = (
    ("gh", ""), ("ph", "f"), ("tch", "t"), ("ch", "t"), ("sh", "s"),
    ("th", "s"), ("ck", "k"), ("c", "k"), ("q", "k"), ("x", "ks"),
    ("l", "r"), ("v", "b"), ("j", "z"), ("w", ""), ("m", "n"),
)
# 濁音と清音は同一視する (ブラウザ / browser の z と s など)。
_UNVOICE = str.maketrans("gzdb", "kstp")


def squeeze(s: str) -> str:
    return re.sub(r"(.)\1+", r"\1", s)


def kana_skeleton(katakana: str) -> str:
    return squeeze("".join(_KANA_SKELETON.get(ch, "") for ch in katakana).translate(_UNVOICE))


def english_skeleton(word: str) -> str:
    s = re.sub(r"[^a-z]", "", word.lower())
    for old, new in _EN_REWRITES:
        s = s.replace(old, new)
    return squeeze(re.sub(r"[aeiouyh]", "", s).translate(_UNVOICE))


def is_subsequence(needle: str, haystack: str) -> bool:
    it = iter(haystack)
    return all(ch in it for ch in needle)


def plausible(katakana: str, word: str) -> bool:
    """読みの子音骨格が英単語の骨格の部分列で、先頭の子音が一致する。"""
    ja = kana_skeleton(katakana)
    en = english_skeleton(word)
    return bool(ja) and bool(en) and ja[0] == en[0] and is_subsequence(ja, en)


def english_words(sense: ET.Element) -> Iterator[str]:
    """英語由来の語義から英単語を、原語表記 (lsource) を優先して返す。"""
    sources = sense.findall("lsource")
    if sources and not any(s.get(XML_LANG, "eng") == "eng" for s in sources):
        return
    for s in sources:
        if s.text:
            yield s.text.strip()
    for g in sense.findall("gloss"):
        if g.text:
            yield re.sub(r"\s*\(.*?\)", "", g.text).strip()


def entry_rows(entry: ET.Element) -> Iterator[tuple[str, str, bool]]:
    """(カタカナ読み, 英単語, 一般語か) を返す。エントリ・読みごとに最初の妥当な語だけ。"""
    if entry.find("k_ele") is not None:
        return
    senses = entry.findall("sense")
    if not senses:
        return
    words = [w for w in english_words(senses[0]) if WORD.fullmatch(w)]
    for r_ele in entry.findall("r_ele"):
        reading = r_ele.findtext("reb", "")
        if not KATAKANA.match(reading):
            continue
        common = r_ele.find("re_pri") is not None
        word = next((w for w in words if plausible(reading, w)), None)
        if word:
            yield reading, word, common


def build(path: str) -> dict[str, list[str]]:
    found: dict[str, list[tuple[bool, str]]] = defaultdict(list)
    for _, element in ET.iterparse(path, events=("end",)):
        if element.tag != "entry":
            continue
        for reading, word, common in entry_rows(element):
            found[reading].append((common, word))
        element.clear()
    table: dict[str, list[str]] = {}
    for reading, items in found.items():
        # 一般語を先に、同順位は JMdict の出現順 (sorted は安定)。
        ordered = [w for _, w in sorted(items, key=lambda x: not x[0])]
        table[reading] = list(dict.fromkeys(ordered))[:MAX_PER_READING]
    return table


def main(path: str) -> None:
    print("# JMdict (c) EDRDG, CC BY-SA 4.0 https://www.edrdg.org/edrdg/licence.html")
    print("# tools/gen_loanwords.py で生成。手で編集しない。")
    for reading, words in sorted(build(path).items()):
        print(reading + "\t" + "\t".join(words))


if __name__ == "__main__":
    main(sys.argv[1])
