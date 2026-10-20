//! 学習の Swift 版との比較 (`--kkc-learn-compare`)。
//!
//! 同じ「変換 → 下位の候補を確定 → 同じ読みをもう一度変換」の列を、Swift 版 (学習 on・空の一時メモリ) と
//! Rust 版 (学習 on・空の一時ディレクトリ) の両方に流して、学習後の上位 3 件が一致するか数える。
//! 2 つの学習は同じ `NOVAKEY_MEMORY_DIR` を使う (Swift は直下のファイル、Rust は `rust-learning/` の中)。
//! 事例は 1 つの記憶に積み上げる (実際の使い方と同じ。事例どうしで読みが重ならないようにしてある)。
#![forbid(unsafe_code)]

use std::rc::Rc;

use super::learning::Learning;
use super::{KkcCore, RustEngine};
use crate::composer::CandidateEngine;
use crate::kkc_bridge::KkcEngine;

/// (読み, 確定する候補の順位 (1 始まり、変換前の一覧での位置), 確定する回数)
const CASES: &[(&str, usize, usize)] = &[
    ("かんじ", 3, 1),
    ("きかい", 2, 1),
    ("こうえん", 3, 1),
    ("せいかく", 2, 1),
    ("はし", 4, 1),
    ("かみ", 3, 1),
    ("あめ", 2, 1),
    ("いし", 3, 1),
    ("きょうはいいてんきですね", 3, 1),
    ("わたしのなまえはなかのです", 2, 1),
    ("あしたかいしゃにいきます", 2, 1),
    ("けんきゅうしつ", 3, 1),
    ("ふくざつなもんだい", 2, 1),
    ("おんがくをきく", 3, 1),
    ("しゅうまつにかいものにいく", 2, 1),
    ("はなをみる", 3, 2),
    ("せいふのほうしん", 2, 1),
    ("さくじつのかいぎ", 3, 1),
    ("しょうがっこう", 4, 1),
    ("そうだんにのってください", 2, 2),
    ("ほんをよむ", 3, 1),
    ("かてい", 3, 1),
];

pub fn run(args: &[String]) -> i32 {
    let verbose = args.iter().any(|a| a == "--show");
    let dir = std::env::temp_dir().join(format!("novakey-r-learn-compare-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::env::remove_var("NOVAKEY_DISABLE_LEARNING");
    std::env::set_var("NOVAKEY_MEMORY_DIR", &dir);

    let core = match KkcCore::open_with_learning(Learning::open(&dir.join("rust-learning"))) {
        Ok(c) => Rc::new(c),
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let mut rust = RustEngine::with_core(core);
    let mut swift = KkcEngine::new(0);
    let mut rev = 0u64;
    let mut top3_match = 0;
    let mut top1_match = 0;
    let mut all_match = 0;
    let mut baseline_match = 0;
    let mut skipped = 0;
    for &(reading, rank, times) in CASES {
        rev += 1;
        let sw0 = swift.convert(rev, reading);
        let ru0 = rust.convert(rev, reading);
        baseline_match += usize::from(sw0.iter().take(3).eq(ru0.iter().take(3)));
        let Some(picked) = sw0.get(rank - 1).cloned() else {
            skipped += 1;
            continue;
        };
        let mut ok = true;
        for t in 0..times {
            let sw = if t == 0 { sw0.clone() } else { rev += 1; swift.convert(rev, reading) };
            let ru = if t == 0 { ru0.clone() } else { rust.convert(rev, reading) };
            let si = sw.iter().position(|c| *c == picked);
            let ri = ru.iter().position(|c| *c == picked);
            // 2 回目以降は、その時点の一覧にあればそれを、なければ飛ばす
            if let Some(i) = si {
                swift.select(rev, i);
            } else {
                swift.cancel();
            }
            if let Some(i) = ri {
                rust.select(rev, i);
            } else {
                rust.cancel();
            }
            ok &= si.is_some() && ri.is_some();
        }
        rev += 1;
        let sw1 = swift.convert(rev, reading);
        let ru1 = rust.convert(rev, reading);
        swift.cancel();
        rust.cancel();
        let t3 = sw1.iter().take(3).eq(ru1.iter().take(3));
        let t1 = sw1.first() == ru1.first();
        let same = sw1 == ru1;
        top3_match += usize::from(t3);
        top1_match += usize::from(t1);
        all_match += usize::from(same);
        let status = if t3 { "ok  " } else { "DIFF" };
        println!("{status} {reading} (確定: {picked} x{times}{})", if ok { "" } else { " ※一覧に無く確定できなかった回あり" });
        if verbose || !t3 {
            println!("      before swift: {}", sw0.iter().take(5).cloned().collect::<Vec<_>>().join(" / "));
            println!("      after  swift: {}", sw1.iter().take(5).cloned().collect::<Vec<_>>().join(" / "));
            println!("      after  rust : {}", ru1.iter().take(5).cloned().collect::<Vec<_>>().join(" / "));
        }
    }
    let n = CASES.len() - skipped;
    println!("学習前の top3 一致 {baseline_match}/{}", CASES.len());
    println!("学習後 top1 一致 {top1_match}/{n}, top3 一致 {top3_match}/{n}, 上位 9 件すべて一致 {all_match}/{n}");
    // 全事例を覚えた後で、同じ読みと、覚えた語を含む別の読みをもう一度変換して比べる
    let extras = ["かんじをかく", "こうえんにいく", "きかいをつかう", "かみをきる", "あめがふる", "いしになる", "かていのじじょう", "けんきゅうしつにいく"];
    let mut final_match = 0;
    let mut final_total = 0;
    for reading in CASES.iter().map(|c| c.0).chain(extras) {
        rev += 1;
        let sw = swift.convert(rev, reading);
        let ru = rust.convert(rev, reading);
        swift.cancel();
        rust.cancel();
        final_total += 1;
        if sw.iter().take(3).eq(ru.iter().take(3)) {
            final_match += 1;
        } else {
            println!("DIFF(累積) {reading}\n      swift: {}\n      rust : {}", sw.iter().take(5).cloned().collect::<Vec<_>>().join(" / "), ru.iter().take(5).cloned().collect::<Vec<_>>().join(" / "));
        }
    }
    println!("全事例を覚えた後 (事例の読み + 追加の読み) top3 一致 {final_match}/{final_total}");
    let _ = std::fs::remove_dir_all(&dir);
    i32::from(top3_match != n || final_match != final_total)
}
