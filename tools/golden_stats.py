"""Swift 版 KKC の golden を測定する (Rust 版移植 P0)。

    python3 -I tools/golden_stats.py [--runs N] [--no-runs] [--limit N]

測るもの:
  (a) fresh と typed で top3 が違う行 (増分経路の影響)。f32 の bit が等しい候補どうしは集合として比べる。
  (b) SWIFT_DETERMINISTIC_HASHING なしで fresh を N 回 (既定 5) 走らせ、結果が揺れる行 (同点の不安定さ)。
      tools/golden/unstable.txt に書く。
  (c) special 候補らしい候補と wise 辞書エントリ由来の候補が top9 に入る頻度 (fresh golden)。

候補の判定 (すべて data の形と text から。出どころは公開 API から取れないため推定):
  special (単独の data 1 つ):
    version   text が 'NovakeyR' で始まる (バージョン専用)
    email     lcid == rcid == 0 かつ text と読みに '@'
    unicode   lcid == rcid == 0 かつ読みが u/U(+)16進 (u+3042 系)
    time      text が H:MM / HH:MM で value == -10
    comma     text が桁区切りの数 (1,234 / -1,234.5) で value == -10
    calendar  text に年号/年 (令和・平成・昭和・NNNN年・元年) かつ value が -15 (cid 固有名詞) または -18/-19 (cid 一般名詞, mid 年)
  wise (data のいずれかの要素が該当。getWiseDicdata / getJapaneseNumberDicdata の出力):
    number_kana  lcid == 数(1295) かつ mid == 数(452)   いち -> 1 / 一
    number_int   lcid == 数(1295) かつ mid == 小さい数字(361)  Int(読み) と漢数字
    single_kana  lcid == rcid == 固有名詞(1288) かつ mid == 一般(501)、ruby 1 文字、value -13/-14
    symbol       lcid == 記号(5) かつ mid == 一般(501)  全半角・異体字
"""

import argparse
import gzip
import json
import os
import re
import subprocess
import sys
import tempfile
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any, Iterable, Iterator

ROOT = Path(__file__).resolve().parent.parent
GOLDEN = ROOT / "tools" / "golden"
CORPUS = ROOT / "tools" / "kkc_corpus.txt"
BINARY = ROOT / "target" / "release" / "novakey_r"

Row = dict[str, Any]
RowKey = tuple[str, tuple[str, ...]]

CID_SYMBOL = 5
CID_COMMON_NOUN = 1285
CID_PROPER_NOUN = 1288
CID_NUMBER = 1295
MID_YEAR = 237
MID_SMALL_NUMBER = 361
MID_NUMBER = 452
MID_GENERAL = 501


def bits_to_float(bits: str) -> float:
    import struct

    return struct.unpack(">f", bytes.fromhex(bits))[0]


def open_text(path: Path) -> Iterator[str]:
    opener = gzip.open if path.suffix == ".gz" else open
    with opener(path, "rt", encoding="utf-8") as f:
        yield from f


def load_rows(path: Path) -> list[Row]:
    return [json.loads(line) for line in open_text(path)]


def row_key(row: Row) -> RowKey:
    return (row["reading"], tuple(row.get("seq", ())))


def top_groups(cands: list[Row], k: int = 3) -> list[tuple[str, frozenset[str]]]:
    """先頭から k 件を覆うところまでの、value の bit が等しい連続区間の列。区間は集合で持つ。"""
    groups: list[tuple[str, frozenset[str]]] = []
    covered = 0
    i = 0
    while covered < k and i < len(cands):
        value = cands[i]["value"]
        texts: list[str] = []
        while i < len(cands) and cands[i]["value"] == value:
            texts.append(cands[i]["text"])
            i += 1
        groups.append((value, frozenset(texts)))
        covered += len(texts)
    return groups


def top_texts(row: Row, n: int) -> list[str]:
    return [c["text"] for c in row["main"][:n]]


def label(key: RowKey) -> str:
    reading, seq = key
    return f"{reading} [seq {'|'.join(seq)}]" if seq else reading


def report_fresh_vs_typed(limit: int) -> None:
    fresh = {row_key(r): r for r in reversed(load_rows(GOLDEN / "fresh.jsonl.gz"))}
    typed_rows = load_rows(GOLDEN / "typed.jsonl.gz")
    compared = 0
    top3_diff: list[tuple[RowKey, Row, Row]] = []
    top9_diff = 0
    first_clause_diff = 0
    for t in typed_rows:
        f = fresh.get(row_key(t))
        if f is None:
            continue
        compared += 1
        if top_groups(f["main"]) != top_groups(t["main"]):
            top3_diff.append((row_key(t), f, t))
        if top_texts(f, 9) != top_texts(t, 9):
            top9_diff += 1
        if top_groups(f["first_clause"], 1) != top_groups(t["first_clause"], 1):
            first_clause_diff += 1
    print("== (a) fresh vs typed ==")
    print(f"compared rows: {compared}")
    print(f"top3 differ (bit-equal ties as sets): {len(top3_diff)} ({100 * len(top3_diff) / compared:.2f}%)")
    print(f"top9 text list differs (order-exact): {top9_diff} ({100 * top9_diff / compared:.2f}%)")
    print(f"first clause top1 differs: {first_clause_diff}")
    for key, f, t in top3_diff[:limit]:
        print(f"  {label(key)}")
        print(f"    fresh: {top_texts(f, 5)}")
        print(f"    typed: {top_texts(t, 5)}")
    if len(top3_diff) > limit:
        print(f"  ... {len(top3_diff) - limit} more (--limit で増やす)")
    print()


def run_fresh_nondeterministic(out: Path) -> None:
    env = {k: v for k, v in os.environ.items() if k != "SWIFT_DETERMINISTIC_HASHING"}
    subprocess.run([str(BINARY), "--kkc-dump", str(CORPUS), str(out)], check=True, env=env)


def canonical(row: Row) -> str:
    return json.dumps(row, sort_keys=True, ensure_ascii=False)


def report_instability(runs: int) -> None:
    golden = load_rows(GOLDEN / "fresh.jsonl.gz")
    variants: list[dict[int, Row]] = [dict(enumerate(golden))]
    with tempfile.TemporaryDirectory() as tmp:
        for n in range(runs):
            out = Path(tmp) / f"run{n}.jsonl"
            run_fresh_nondeterministic(out)
            rows = load_rows(out)
            if len(rows) != len(golden):
                sys.exit(f"run {n}: 行数が golden と違う ({len(rows)} != {len(golden)})")
            variants.append(dict(enumerate(rows)))
    unstable: list[tuple[int, int, bool, bool]] = []
    for i, base in enumerate(golden):
        rows = [v[i] for v in variants]
        distinct = len({canonical(r) for r in rows})
        top3_changes = len({tuple(top_groups(r["main"])) for r in rows}) > 1
        top9_changes = len({tuple(top_texts(r, 9)) for r in rows}) > 1
        if distinct > 1:
            unstable.append((i, distinct, top3_changes, top9_changes))
    print(f"== (b) nondeterministic hashing, {runs} runs + deterministic golden ==")
    print(f"rows: {len(golden)}; rows whose result varies at all: {len(unstable)}")
    print(f"  top3 varies (ties as sets): {sum(1 for u in unstable if u[2])}")
    print(f"  top9 text order varies: {sum(1 for u in unstable if u[3])}")
    lines = [
        "# SWIFT_DETERMINISTIC_HASHING なしで fresh を複数回走らせたとき結果が揺れる行 (tools/golden_stats.py が生成)。",
        "# 列: 読み<TAB>seq<TAB>異なる結果の数<TAB>top3が揺れる(0/1)<TAB>top9の文字列順が揺れる(0/1)",
    ]
    for i, distinct, top3, top9 in unstable:
        reading, seq = row_key(golden[i])
        lines.append(f"{reading}\t{'|'.join(seq)}\t{distinct}\t{int(top3)}\t{int(top9)}")
    (GOLDEN / "unstable.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")
    shown = [u for u in unstable if u[2]][:10] or unstable[:10]
    for i, distinct, top3, top9 in shown:
        print(f"  {label(row_key(golden[i]))}: {distinct} variants, top3={top3}, top9={top9}")
    print(f"written: {GOLDEN / 'unstable.txt'}")
    print()


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


def wise_kinds(c: Row) -> tuple[set[str], bool]:
    """(含まれる wise 種別, 全要素が wise か)"""
    per_element = [wise_kinds_of_element(d) for d in c["data"]]
    kinds = set().union(*per_element) if per_element else set()
    return kinds, bool(per_element) and all(per_element)


def report_top9_frequency() -> None:
    rows = load_rows(GOLDEN / "fresh.jsonl.gz")
    total = len(rows)
    special_top9: Counter[str] = Counter()
    special_any: Counter[str] = Counter()
    special_examples: dict[str, list[str]] = defaultdict(list)
    wise_involved: Counter[str] = Counter()
    wise_only: Counter[str] = Counter()
    wise_examples: dict[str, list[str]] = defaultdict(list)
    for row in rows:
        seen_top9: set[str] = set()
        seen_any: set[str] = set()
        for rank, c in enumerate(row["main"]):
            for kind in special_kinds(c, row["reading"]):
                seen_any.add(kind)
                if rank < 9:
                    seen_top9.add(kind)
                    if len(special_examples[kind]) < 6:
                        special_examples[kind].append(f"{row['reading']} -> {c['text']} (#{rank + 1})")
        for kind in seen_any:
            special_any[kind] += 1
        for kind in seen_top9:
            special_top9[kind] += 1
        involved: set[str] = set()
        only: set[str] = set()
        for rank, c in enumerate(row["main"][:9]):
            kinds, all_wise = wise_kinds(c)
            involved |= kinds
            if all_wise:
                only |= kinds
            for kind in kinds:
                if len(wise_examples[kind]) < 6 and (all_wise or kind not in only):
                    wise_examples[kind].append(f"{row['reading']} -> {c['text']} (#{rank + 1})")
        for kind in involved:
            wise_involved[kind] += 1
        for kind in only:
            wise_only[kind] += 1
    print("== (c) frequency in main[:9] of the fresh golden ==")
    print(f"rows: {total}")
    print("special candidates (rows with >=1 in top9 / rows with >=1 anywhere in main):")
    for kind in ["version", "email", "unicode", "time", "comma", "calendar"]:
        print(f"  {kind:9s} top9 {special_top9[kind]:4d}  anywhere {special_any[kind]:4d}")
        for ex in special_examples[kind][:4]:
            print(f"      {ex}")
    print("wise entries (rows where a top9 candidate contains one / is made only of wise entries):")
    for kind in ["number_kana", "number_int", "single_kana", "symbol"]:
        print(f"  {kind:11s} involved {wise_involved[kind]:4d}  wise-only {wise_only[kind]:4d}")
        for ex in wise_examples[kind][:4]:
            print(f"      {ex}")
    print()


def main(argv: Iterable[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--runs", type=int, default=5, help="(b) の非決定的実行の回数")
    parser.add_argument("--no-runs", action="store_true", help="(b) を飛ばす")
    parser.add_argument("--limit", type=int, default=30, help="(a) で表示する行数")
    args = parser.parse_args(argv)
    report_fresh_vs_typed(args.limit)
    if not args.no_runs:
        report_instability(args.runs)
    report_top9_frequency()


if __name__ == "__main__":
    main()
