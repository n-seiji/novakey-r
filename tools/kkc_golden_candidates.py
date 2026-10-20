"""fresh golden から辞書リーダー (Rust P1) の検証用 TSV を標準出力に出す。

    python3 -I tools/kkc_golden_candidates.py

対象: main / first_clause の候補のうち data が 1 要素で、その ruby (カタカナ) が読み (カタカナ化) の prefix のもの。
列: reading_katakana, ruby, word, lcid, rcid, mid, value_bits, adjust_bits, kind
kind は辞書エントリではなく動的に作られる候補の推定種別 (golden_stats.py と同じ判定)。`-` なら辞書エントリのはず。
  wise 系: number_kana / number_int / single_kana / symbol
  special 系: version / email / unicode / time / comma / calendar
同じ行は 1 度だけ出す。
"""

import gzip
import importlib.util
import json
import sys
from pathlib import Path

sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parent.parent

spec = importlib.util.spec_from_file_location("golden_stats", ROOT / "tools" / "golden_stats.py")
gs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gs)


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
                kinds = gs.wise_kinds_of_element(d) | gs.special_kinds(cand, row["reading"])
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
