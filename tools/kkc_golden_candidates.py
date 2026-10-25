"""fresh golden から辞書リーダー (Rust P1) の検証用 TSV を標準出力に出す。

    python3 -I tools/kkc_golden_candidates.py

対象: main / first_clause の候補のうち data が 1 要素で、その ruby (カタカナ) が読み (カタカナ化) の prefix のもの。
列: reading_katakana, ruby, word, lcid, rcid, mid, value_bits, adjust_bits, kind
kind は辞書エントリではなく動的に作られる候補の推定種別。`-` なら辞書エントリのはず。
  wise 系: number_kana / number_int / single_kana / symbol
  special 系: version / email / unicode / time / comma / calendar
同じ行は 1 度だけ出す。
"""

import gzip
import json
import re
import struct
import sys
from pathlib import Path
from typing import Any

sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parent.parent

Row = dict[str, Any]

CID_SYMBOL = 5
CID_COMMON_NOUN = 1285
CID_PROPER_NOUN = 1288
CID_NUMBER = 1295
MID_YEAR = 237
MID_SMALL_NUMBER = 361
MID_NUMBER = 452
MID_GENERAL = 501


def bits_to_float(bits: str) -> float:
    return struct.unpack(">f", bytes.fromhex(bits))[0]


ERA = re.compile(r"令和|平成|昭和|大正|明治|西暦|^\d+年$|元年")
UNICODE_READING = re.compile(r"^[uU](\+)?[0-9a-fA-F]+$")


def special_kinds(c: Row, reading: str) -> set[str]:
    data = c["data"]
    if len(data) != 1:
        return set()
    d = data[0]
    text = c["text"]
    value = bits_to_float(d["value"])
    kinds: set[str] = set()
    # The golden data was captured before the rename to Shizuku; its version candidate still reads "NovakeyR 0.1".
    if text.startswith("NovakeyR"):
        kinds.add("version")
    if d["lcid"] == 0 and d["rcid"] == 0:
        if "@" in text and "@" in reading:
            kinds.add("email")
        elif UNICODE_READING.match(reading):
            kinds.add("unicode")
    if re.fullmatch(r"\d{1,2}:\d{2}", text) and value == -10:
        kinds.add("time")
    if re.fullmatch(r"-?\d{1,3}(,\d{3})+(\.\d+)?", text) and value == -10:
        kinds.add("comma")
    if ERA.search(text):
        if value == -15 and d["lcid"] == CID_PROPER_NOUN:
            kinds.add("calendar")
        if value in (-18, -19) and d["lcid"] == CID_COMMON_NOUN and d["mid"] == MID_YEAR:
            kinds.add("calendar")
    return kinds


def wise_kinds_of_element(d: Row) -> set[str]:
    kinds: set[str] = set()
    value = bits_to_float(d["value"])
    if d["lcid"] == CID_NUMBER and d["mid"] == MID_NUMBER:
        kinds.add("number_kana")
    if d["lcid"] == CID_NUMBER and d["mid"] == MID_SMALL_NUMBER:
        kinds.add("number_int")
    if (
        d["lcid"] == CID_PROPER_NOUN
        and d["rcid"] == CID_PROPER_NOUN
        and d["mid"] == MID_GENERAL
        and len(d["ruby"]) == 1
        and value in (-13, -14)
    ):
        kinds.add("single_kana")
    if d["lcid"] == CID_SYMBOL and d["mid"] == MID_GENERAL:
        kinds.add("symbol")
    return kinds



def to_katakana(s: str) -> str:
    return "".join(chr(ord(c) + 0x60) if 0x3041 <= ord(c) <= 0x3096 else c for c in s)


def main() -> None:
    seen = set()
    with gzip.open(ROOT / "tools" / "golden" / "fresh.jsonl.gz", "rt", encoding="utf-8") as f:
        for line in f:
            row = json.loads(line)
            reading = to_katakana(row["reading"])
            for cand in row["main"] + row["first_clause"]:
                if len(cand["data"]) != 1:
                    continue
                d = cand["data"][0]
                if not reading.startswith(d["ruby"]) or d["ruby"] == "":
                    continue
                kinds = wise_kinds_of_element(d) | special_kinds(cand, row["reading"])
                kind = "+".join(sorted(kinds)) if kinds else "-"
                fields = [reading, d["ruby"], d["word"], str(d["lcid"]), str(d["rcid"]), str(d["mid"]), d["value"], d["adjust"], kind]
                if any(("\t" in x or "\n" in x) for x in fields):
                    continue
                key = tuple(fields)
                if key in seen:
                    continue
                seen.add(key)
                print("\t".join(fields))


if __name__ == "__main__":
    main()
    sys.stdout.flush()
