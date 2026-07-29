# 辞書エンジン移行 + LLM リランキング Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** かな漢字変換を AzooKeyKanaKanjiConverter(辞書ラティス+Viterbi、数ms・n-best)に移行してライブ変換と候補ウィンドウを実現し、LLM は非同期リランキング(候補番号の選択)に役割変更する。

**Architecture:** `swift/AzooKeyBridge`(SPM ラッパー、type: .static)を build.rs で `swift build` し単一 .a をリンク。`kkc_bridge.rs` が同期 FFI(メインスレッド専用)。`composer.rs` は候補リスト状態機械に書き換え(同期 `CandidateEngine` trait 注入でテスト)。`imk.rs` はライブ表示・スペース候補送り・IMKCandidates・確定時学習を配線。既存 `llm_bridge.rs`/`LLMBridge.swift` はリランキング専用に縮小。

**Tech Stack:** AzooKeyKanaKanjiConverter v0.11.2 (`.upToNextMinor`)、Swift 6.1+(手元 6.2)、既存 objc2 0.5 構成。新規 Rust 依存なし。

**設計 doc:** `docs/superpowers/specs/2026-07-29-azookey-engine-design.md`

## Global Constraints

- `KanaKanjiConverter` と候補状態は**メインスレッド専用**。Swift の各 FFI 冒頭で `dispatchPrecondition(condition: .onQueue(.main))` を入れて契約を検証する
- 候補は Swift 側で **controller ごとに (candidates, revision) を分離保持**する。select は (controller_id, revision, engine_index) で照合し、不一致なら学習せず false(複数アプリ並行入力での学習誤爆防止)
- Rust 側の候補は `{text, engine_index}` のペアで扱い、**学習には常に元の engine_index を渡す**(リランキングで表示順を変えても学習対象がずれない)
- 依存は `.exact("0.11.2")` で完全固定し、`swift/AzooKeyBridge/Package.resolved` をコミットする
- IMKCandidates: panelType は 1(kIMKSingleColumnScrollingCandidatePanel)、show の location hint は **2(kIMKLocateCandidatesBelowHint)**。マジックナンバーは Rust 定数化
- `novakey_llm_on_result` は main queue 上で呼ばれる契約(既存)を維持
- 辞書リソースバンドル `AzooKeyKanaKanjiConverter_KanaKanjiConverterModuleWithDefaultDictionary.bundle` は**実行ファイルと同じディレクトリ**に必要(検証済み)。.app では `Contents/MacOS/`、cargo 直実行では `target/release/`
- 学習データ保存先: `~/Library/Application Support/NovakeyR/`(FFI 初期化時に作成)
- Swift ビルド検証済みリンクフラグ: `-L <bridge>/.build/release -lAzooKeyBridge`、`$(xcrun --show-sdk-path)/usr/lib/swift`、`/Library/Developer/CommandLineTools/usr/lib/swift/macosx`(compatibility libs、無いと undefined symbol)、`-Wl,-rpath,/usr/lib/swift`
- v0.11.2 の API 注意: `requireJapanesePrediction: Bool`(main ブランチ README の `.autoMix` は未リリース)。`shouldResetMemory` は deprecated だが README 例どおり `false` を渡す
- テスト: `cargo test` 全通過。composer はモックエンジンでユニットテスト
- コミット規約: emoji プレフィックス
- N_best = 9(候補ウィンドウ1画面分)

## File Structure

- Create: `swift/AzooKeyBridge/Package.swift` — ラッパー SPM パッケージ
- Create: `swift/AzooKeyBridge/Sources/AzooKeyBridge/Bridge.swift` — `@_cdecl` FFI 実装
- Create: `src/kkc_bridge.rs` — 同期 FFI 宣言 + Rust ラッパ + smoke test
- Rewrite: `src/composer.rs` — 候補リスト状態機械(旧 reveal/デバウンス/タイムアウト機構は削除)
- Modify: `src/imk.rs` — 配線全面変更、IMKCandidates
- Modify: `src/llm_bridge.rs` / `swift/LLMBridge.swift` — リランキング専用化
- Modify: `build.rs` — swift build 追加、バンドルコピー
- Modify: `src/main.rs` — `mod kkc_bridge;` + `--test-kkc`
- Modify: `mise.toml` — app タスクのバンドル同梱、`test-kkc` タスク
- Modify: `CLAUDE.md`

参考実装(調査エージェントの実機検証済み成果物、そのまま流用可):
`/private/tmp/claude-501/-Users-sijis-ghq-github-com-n-seiji-novakey-r/317ee42d-6620-498b-886b-57ef095c3d95/scratchpad/bridge/`(Package.swift + Bridge.swift)

---

### Task 1: AzooKeyBridge — SPM パッケージ・FFI・ビルド統合

**Files:**
- Create: `swift/AzooKeyBridge/Package.swift`
- Create: `swift/AzooKeyBridge/Sources/AzooKeyBridge/Bridge.swift`
- Create: `src/kkc_bridge.rs`
- Modify: `build.rs`, `src/main.rs`, `mise.toml`, `.gitignore`

**Interfaces:**
- Produces(C ABI):
  - `novakey_kkc_convert(reading_ptr, reading_len) -> *mut c_char` — `\u{1F}` 区切りの候補文字列(strdup 確保、呼び出し側が `free`)。候補ゼロなら空文字列
  - `novakey_kkc_select(index: Int)` — 学習更新 + 永続化 + セッション終了
  - `novakey_kkc_cancel()` — セッション終了のみ
- Produces(Rust):
  - `kkc_bridge::convert(reading: &str) -> Vec<String>`(メインスレッド専用)
  - `kkc_bridge::select(index: usize)` / `kkc_bridge::cancel()`
  - `kkc_bridge::smoke_test()`

- [ ] **Step 1: Package.swift を作成**

`swift/AzooKeyBridge/Package.swift`:

```swift
// swift-tools-version: 6.1
import PackageDescription

let package = Package(
    name: "AzooKeyBridge",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "AzooKeyBridge", type: .static, targets: ["AzooKeyBridge"])
    ],
    dependencies: [
        .package(url: "https://github.com/azooKey/AzooKeyKanaKanjiConverter", exact: "0.11.2")
    ],
    targets: [
        .target(name: "AzooKeyBridge", dependencies: [
            .product(name: "KanaKanjiConverterModuleWithDefaultDictionary", package: "AzooKeyKanaKanjiConverter")
        ])
    ]
)
```

`.gitignore` に `swift/AzooKeyBridge/.build/` を追加。

- [ ] **Step 2: Bridge.swift を作成**

`swift/AzooKeyBridge/Sources/AzooKeyBridge/Bridge.swift`:

```swift
// Bridge.swift - AzooKeyKanaKanjiConverter を C ABI で Rust に公開する。
// 契約: 全関数メインスレッド専用(dispatchPrecondition で検証)。
// 候補は controller ごとに (candidates, revision) で分離保持し、select は
// (controller_id, revision, engine_index) の照合に成功したときだけ学習する。
import Foundation
import KanaKanjiConverterModuleWithDefaultDictionary

private nonisolated(unsafe) var converter: KanaKanjiConverter?
private struct SessionState {
    var candidates: [Candidate]
    var revision: UInt64
}
private nonisolated(unsafe) var sessions: [UInt64: SessionState] = [:]

private func memoryDir() -> URL {
    let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        .appendingPathComponent("NovakeyR", isDirectory: true)
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    return dir
}

private func getConverter() -> KanaKanjiConverter {
    if let c = converter { return c }
    let c = KanaKanjiConverter.withDefaultDictionary()
    converter = c
    return c
}

private func makeOptions() -> ConvertRequestOptions {
    let dir = memoryDir()
    return .init(
        N_best: 9,
        requireJapanesePrediction: true,
        requireEnglishPrediction: false,
        keyboardLanguage: .ja_JP,
        englishCandidateInRoman2KanaInput: true,
        fullWidthRomanCandidate: false,
        halfWidthKanaCandidate: false,
        learningType: .inputAndOutput,
        maxMemoryCount: 65536,
        shouldResetMemory: false,
        memoryDirectoryURL: dir,
        sharedContainerURL: dir,
        textReplacer: .withDefaultEmojiDictionary(),
        specialCandidateProviders: KanaKanjiConverter.defaultSpecialCandidateProviders,
        metadata: .init(versionString: "NovakeyR 0.1")
    )
}

/// 変換して候補テキストを \u{1F} 区切りで返す(strdup 確保、Rust が free)。
/// 戻り値 nil はエラー、空文字列は候補ゼロ。候補テキスト中の \u{1F} は除去して整合を守る。
@_cdecl("novakey_kkc_convert")
public func novakey_kkc_convert(
    _ controllerId: UInt64, _ revision: UInt64,
    _ readingPtr: UnsafePointer<UInt8>?, _ readingLen: Int
) -> UnsafeMutablePointer<CChar>? {
    dispatchPrecondition(condition: .onQueue(.main))
    guard let readingPtr, readingLen > 0 else { return nil }
    let reading = String(decoding: UnsafeBufferPointer(start: readingPtr, count: readingLen), as: UTF8.self)
    var composing = ComposingText()
    composing.insertAtCursorPosition(reading, inputStyle: .direct)
    let result = getConverter().requestCandidates(composing, options: makeOptions())
    sessions[controllerId] = SessionState(candidates: result.mainResults, revision: revision)
    let joined = result.mainResults
        .map { $0.text.replacingOccurrences(of: "\u{1F}", with: "") }
        .joined(separator: "\u{1F}")
    return strdup(joined)
}

/// engine_index の候補を確定として学習。revision 不一致・範囲外は false(学習なし)。
/// 成否に関わらずセッションは終了する。
@_cdecl("novakey_kkc_select")
public func novakey_kkc_select(
    _ controllerId: UInt64, _ revision: UInt64, _ engineIndex: Int
) -> Bool {
    dispatchPrecondition(condition: .onQueue(.main))
    defer {
        converter?.stopComposition()
        sessions[controllerId] = nil
    }
    guard let session = sessions[controllerId],
          session.revision == revision,
          engineIndex >= 0, engineIndex < session.candidates.count
    else { return false }
    let c = getConverter()
    c.updateLearningData(session.candidates[engineIndex])
    c.commitUpdateLearningData()
    return true
}

@_cdecl("novakey_kkc_cancel")
public func novakey_kkc_cancel(_ controllerId: UInt64) {
    dispatchPrecondition(condition: .onQueue(.main))
    converter?.stopComposition()
    sessions[controllerId] = nil
}
```

- [ ] **Step 3: kkc_bridge.rs を作成**

`src/kkc_bridge.rs`:

```rust
// kkc_bridge.rs - AzooKeyBridge(辞書かな漢字変換)への同期 FFI。
// 契約: 全関数メインスレッド専用。convert は数ms で返る。
// select は (controller_id, revision, engine_index) 照合に成功したときだけ学習する。
use std::ffi::{c_char, CStr};

extern "C" {
    fn novakey_kkc_convert(
        controller_id: u64,
        revision: u64,
        reading_ptr: *const u8,
        reading_len: usize,
    ) -> *mut c_char;
    fn novakey_kkc_select(controller_id: u64, revision: u64, engine_index: isize) -> bool;
    fn novakey_kkc_cancel(controller_id: u64);
}

/// 読み(ひらがな)から n-best 候補テキストを得る。候補なし・エラーなら空 Vec。
pub fn convert(controller_id: u64, revision: u64, reading: &str) -> Vec<String> {
    if reading.is_empty() {
        return Vec::new();
    }
    let ptr = unsafe { novakey_kkc_convert(controller_id, revision, reading.as_ptr(), reading.len()) };
    if ptr.is_null() {
        return Vec::new();
    }
    let joined = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { libc::free(ptr as *mut libc::c_void) };
    if joined.is_empty() {
        return Vec::new();
    }
    joined.split('\u{1F}').map(str::to_string).collect()
}

/// engine_index の候補を確定として学習し、変換セッションを終える。
/// revision 不一致等で学習されなかった場合は false。
pub fn select(controller_id: u64, revision: u64, engine_index: usize) -> bool {
    unsafe { novakey_kkc_select(controller_id, revision, engine_index as isize) }
}

/// 確定せず変換セッションを終える(Esc・生ひらがな確定)。
pub fn cancel(controller_id: u64) {
    unsafe { novakey_kkc_cancel(controller_id) };
}

/// mise run test-kkc 用: 1回変換して候補を表示。
pub fn smoke_test() {
    let candidates = convert(0, 1, "きょうはいいてんきですね");
    println!("candidates: {}", candidates.len());
    for (i, c) in candidates.iter().enumerate() {
        println!("{}: {}", i, c);
    }
    // revision 照合の検証: 一致で true、不一致で false
    assert!(select(0, 1, 0), "select が revision 一致で失敗");
    let again = convert(0, 2, "きょう");
    assert!(!again.is_empty());
    assert!(!select(0, 99, 0), "select が revision 不一致なのに成功");
    cancel(0);
    assert!(!candidates.is_empty(), "候補が空");
    assert!(
        candidates.iter().any(|c| c.contains("天気")),
        "天気を含む候補がない: {:?}",
        candidates
    );
    println!("OK");
}
```

`src/main.rs` に `mod kkc_bridge;` と、`--test-llm` 分岐の隣に:

```rust
    if std::env::args().any(|a| a == "--test-kkc") {
        kkc_bridge::smoke_test();
        return;
    }
```

- [ ] **Step 4: build.rs に swift build とリンクを追加**

`build.rs` の swiftc(LLMBridge)ブロックの後に追加:

```rust
    // AzooKeyBridge: SPM パッケージを静的ライブラリとしてビルド
    let status = Command::new("swift")
        .args(["build", "-c", "release", "--package-path", "swift/AzooKeyBridge"])
        .status()
        .expect("swift build の起動に失敗");
    assert!(status.success(), "AzooKeyBridge のビルドに失敗");

    let bridge_out = PathBuf::from("swift/AzooKeyBridge/.build/release");
    println!(
        "cargo:rustc-link-search=native={}",
        bridge_out.canonicalize().unwrap().display()
    );
    println!("cargo:rustc-link-lib=static=AzooKeyBridge");

    // Swift ランタイム: SDK の stub と CLT の compatibility libs が必要
    let sdk_path = String::from_utf8(
        Command::new("xcrun")
            .args(["--show-sdk-path"])
            .output()
            .expect("xcrun の起動に失敗")
            .stdout,
    )
    .unwrap();
    println!("cargo:rustc-link-search=native={}/usr/lib/swift", sdk_path.trim());
    let clt_swift = "/Library/Developer/CommandLineTools/usr/lib/swift/macosx";
    if std::path::Path::new(clt_swift).exists() {
        println!("cargo:rustc-link-search=native={}", clt_swift);
    }

    // 辞書リソースバンドルを cargo 実行用に target/<profile>/ へコピー
    let profile = env::var("PROFILE").unwrap();
    let target_dir = PathBuf::from(format!("target/{}", profile));
    let bundle_name = "AzooKeyKanaKanjiConverter_KanaKanjiConverterModuleWithDefaultDictionary.bundle";
    let _ = Command::new("rm")
        .arg("-rf")
        .arg(target_dir.join(bundle_name))
        .status();
    let status = Command::new("cp")
        .arg("-R")
        .arg(bridge_out.join(bundle_name))
        .arg(&target_dir)
        .status()
        .expect("bundle コピーの起動に失敗");
    assert!(status.success(), "辞書バンドルのコピーに失敗");

    println!("cargo:rerun-if-changed=swift/AzooKeyBridge/Package.swift");
    println!("cargo:rerun-if-changed=swift/AzooKeyBridge/Sources");
```

注意: 既存の `/usr/lib/swift` link-search と rpath 行は維持。

- [ ] **Step 5: mise.toml — app タスクのバンドル同梱と test-kkc**

`[tasks.app]` の `cp target/release/novakey_r ...` の直後に追加:

```toml
cp -R swift/AzooKeyBridge/.build/release/AzooKeyKanaKanjiConverter_KanaKanjiConverterModuleWithDefaultDictionary.bundle output/NovakeyR.app/Contents/MacOS/
```

タスク追加:

```toml
[tasks.test-kkc]
description = "辞書かな漢字変換の1回変換スモークテスト"
run = "cargo run --release -- --test-kkc"
```

- [ ] **Step 6: ビルドとスモークテスト**

Run: `cargo build --release`(初回は SPM resolve で数分 + llama.cpp xcframework の DL が走るが正常)
Run: `mise run test-kkc`
Expected: `candidates: 9` 前後、`今日はいい天気ですね` を含む候補一覧、`OK`

- [ ] **Step 7: Commit**

```bash
git add swift/AzooKeyBridge/Package.swift swift/AzooKeyBridge/Package.resolved swift/AzooKeyBridge/Sources src/kkc_bridge.rs src/main.rs build.rs mise.toml .gitignore
git commit -m "[:sparkles:]AzooKeyKanaKanjiConverterのFFIブリッジとビルド統合を追加"
```

---

### Task 2: composer.rs — 候補リスト状態機械への書き換え

**Files:**
- Rewrite: `src/composer.rs`(全面書き換え。旧 reveal/デバウンス/タイムアウト/世代管理の非同期機構は削除)

**Interfaces:**
- Produces:
  - `pub struct Candidate { pub text: String, pub engine_index: usize }` — 表示テキストと**エンジン側の元 index**。リランキングで表示順を変えても engine_index は不変で、学習には常にこれを渡す
  - `pub trait CandidateEngine { fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String>; fn select(&mut self, revision: u64, engine_index: usize) -> bool; fn cancel(&mut self); }` — revision には composer の generation を渡す
  - `Composer::new(engine: E) -> Self` / `Composer::set_live(&mut self, live: bool)`(デフォルト live = true)
  - `Composer::push_hiragana(&mut self, kana: &str)` / `Composer::backspace(&mut self) -> bool` — バッファ更新のたびに `generation += 1` してから同期で `engine.convert(generation, ...)`。結果は enumerate して `Candidate` 化し、**同一テキストは先頭のみ残して dedupe**(候補ウィンドウの文字列一致選択を一意にするため)。選択解除
  - `Composer::display(&self) -> &str` — 選択中ならその候補 / live かつ候補ありなら第1候補 / それ以外(raw 含む)は生ひらがな
  - `Composer::cycle(&mut self) -> bool` — スペース候補送り。候補なしなら false。`None→0→1→…→wrap`(表示 index)
  - `Composer::candidates(&self) -> &[Candidate]` / `Composer::selected_index(&self) -> Option<usize>` / `Composer::display_hiragana(&self) -> &str`
  - `Composer::revert_raw(&mut self)` / `Composer::is_raw(&self) -> bool` — Esc: 選択解除 + raw 表示。**raw 中の再 Esc はアプリへパススルー**するため is_raw を公開
  - `Composer::commit(&mut self) -> String` — display を返す。候補を表示していた場合は `engine.select(generation, その候補の engine_index)`、生ひらがな確定なら `engine.cancel()`。全状態クリア
  - `Composer::is_empty(&self) -> bool`
  - `Composer::current_generation(&self) -> u64`(リランキング用、`#[cfg(test)]` にしない)
  - `Composer::apply_rerank(&mut self, generation: u64, display_index: usize) -> Option<String>` — 世代一致・ユーザー未操作(selected == None)・raw でない・display_index が 1..len のときのみ、その候補を表示順の先頭に移動(engine_index は保持)して新しい display を返す。それ以外 None

- [ ] **Step 1: 旧テストを削除し、新テストを書く**

`src/composer.rs` を新規内容で書き始める。テストモジュール:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    struct Log {
        converts: Rc<RefCell<Vec<(u64, String)>>>,
        // (revision, engine_index)
        selects: Rc<RefCell<Vec<(u64, usize)>>>,
        cancels: Rc<RefCell<usize>>,
    }

    struct MockEngine {
        log: Log,
        // 読み→候補のスタブ。無ければ「読み + K1/K2」を返す
        table: Vec<(String, Vec<String>)>,
    }

    impl CandidateEngine for MockEngine {
        fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String> {
            self.log
                .converts
                .borrow_mut()
                .push((revision, hiragana.to_string()));
            for (k, v) in &self.table {
                if k == hiragana {
                    return v.clone();
                }
            }
            vec![format!("{}K1", hiragana), format!("{}K2", hiragana)]
        }
        fn select(&mut self, revision: u64, engine_index: usize) -> bool {
            self.log.selects.borrow_mut().push((revision, engine_index));
            true
        }
        fn cancel(&mut self) {
            *self.log.cancels.borrow_mut() += 1;
        }
    }

    fn composer() -> (Composer<MockEngine>, Log) {
        let log = Log::default();
        let engine = MockEngine {
            log: log.clone(),
            table: vec![(
                "きょう".to_string(),
                vec!["今日".to_string(), "京".to_string(), "教".to_string()],
            )],
        };
        (Composer::new(engine), log)
    }

    #[test]
    fn 入力なしでは空() {
        let (c, _) = composer();
        assert!(c.is_empty());
        assert_eq!(c.display(), "");
    }

    #[test]
    fn liveなら第1候補を表示() {
        let (mut c, log) = composer();
        c.push_hiragana("きょう");
        assert_eq!(c.display(), "今日");
        assert_eq!(log.converts.borrow().len(), 1);
    }

    #[test]
    fn live_offなら生ひらがな表示() {
        let (mut c, _) = composer();
        c.set_live(false);
        c.push_hiragana("きょう");
        assert_eq!(c.display(), "きょう");
    }

    #[test]
    fn cycleで候補を順送りしwrapする() {
        let (mut c, _) = composer();
        c.push_hiragana("きょう");
        assert!(c.cycle());
        assert_eq!(c.display(), "今日"); // None → 0
        c.cycle();
        assert_eq!(c.display(), "京"); // → 1
        c.cycle();
        c.cycle();
        assert_eq!(c.display(), "今日"); // 2 → wrap 0
        assert_eq!(c.selected_index(), Some(0));
    }

    #[test]
    fn 候補なしならcycleはfalse() {
        let (mut c, _) = composer();
        assert!(!c.cycle());
    }

    #[test]
    fn 入力すると選択が解除され候補が更新される() {
        let (mut c, log) = composer();
        c.push_hiragana("きょう");
        c.cycle();
        c.cycle(); // 「京」選択中
        c.push_hiragana("は");
        assert_eq!(c.selected_index(), None);
        assert_eq!(c.display(), "きょうはK1"); // live: 新候補の先頭
        assert_eq!(log.converts.borrow().last().unwrap().1, "きょうは");
    }

    #[test]
    fn 同一テキスト候補はdedupeされ先頭のengine_indexが残る() {
        let log = Log::default();
        let engine = MockEngine {
            log: log.clone(),
            table: vec![(
                "きょう".to_string(),
                vec![
                    "今日".to_string(),
                    "京".to_string(),
                    "今日".to_string(), // engine_index 2 の重複
                ],
            )],
        };
        let mut c = Composer::new(engine);
        c.push_hiragana("きょう");
        assert_eq!(c.candidates().len(), 2);
        assert_eq!(c.candidates()[0].text, "今日");
        assert_eq!(c.candidates()[0].engine_index, 0);
        assert_eq!(c.candidates()[1].text, "京");
        assert_eq!(c.candidates()[1].engine_index, 1);
    }

    #[test]
    fn backspaceで候補再計算_空になったら候補もクリア() {
        let (mut c, _) = composer();
        c.push_hiragana("きょう");
        assert!(c.backspace());
        assert_eq!(c.display(), "きょK1");
        c.backspace();
        c.backspace();
        assert!(c.is_empty());
        assert_eq!(c.display(), "");
        assert!(c.candidates().is_empty());
        assert!(!c.backspace());
    }

    #[test]
    fn revert_rawで生ひらがな表示_次の入力で解除() {
        let (mut c, _) = composer();
        c.push_hiragana("きょう");
        c.cycle();
        c.revert_raw();
        assert_eq!(c.display(), "きょう");
        assert_eq!(c.selected_index(), None);
        c.push_hiragana("は");
        assert_eq!(c.display(), "きょうはK1"); // raw 解除
    }

    #[test]
    fn commitは候補表示中ならselect_生ひらがなならcancel() {
        let (mut c, log) = composer();
        let indices = |log: &Log| -> Vec<usize> {
            log.selects.borrow().iter().map(|s| s.1).collect()
        };
        // live 第1候補のまま確定
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        assert_eq!(c.commit(), "今日");
        assert_eq!(*log.selects.borrow(), vec![(gen, 0)]); // revision = 変換時の世代
        assert!(c.is_empty());

        // 候補送りして確定
        c.push_hiragana("きょう");
        c.cycle();
        c.cycle();
        assert_eq!(c.commit(), "京");
        assert_eq!(indices(&log), vec![0, 1]);

        // raw(Esc 後)確定
        c.push_hiragana("きょう");
        c.revert_raw();
        assert!(c.is_raw());
        assert_eq!(c.commit(), "きょう");
        assert_eq!(*log.cancels.borrow(), 1);
    }

    #[test]
    fn apply_rerankは表示順のみ変えengine_indexで学習する() {
        let (mut c, log) = composer();
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        assert_eq!(c.apply_rerank(gen, 1), Some("京".to_string()));
        assert_eq!(c.display(), "京"); // 「京」が表示先頭に
        assert_eq!(c.candidates()[0].text, "京");
        assert_eq!(c.candidates()[0].engine_index, 1); // 元 index は保持
        assert_eq!(c.candidates()[1].text, "今日");
        // commit は表示先頭の engine_index(=1) を select する
        assert_eq!(c.commit(), "京");
        assert_eq!(log.selects.borrow().last().unwrap().1, 1);
    }

    #[test]
    fn apply_rerankは旧世代_ユーザー操作後_範囲外で無効() {
        let (mut c, _) = composer();
        c.push_hiragana("きょう");
        let old = c.current_generation();
        c.push_hiragana("は"); // 世代前進
        assert_eq!(c.apply_rerank(old, 1), None);

        let gen = c.current_generation();
        c.cycle(); // ユーザー操作
        assert_eq!(c.apply_rerank(gen, 1), None);

        let (mut c2, _) = composer();
        c2.push_hiragana("きょう");
        let gen2 = c2.current_generation();
        assert_eq!(c2.apply_rerank(gen2, 99), None); // 範囲外
        assert_eq!(c2.apply_rerank(gen2, 0), None); // 既に先頭(変化なし)
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test composer`
Expected: コンパイルエラー(`CandidateEngine` 未定義等)

- [ ] **Step 3: 実装**

```rust
// composer.rs - 未確定文の状態機械: ひらがなバッファ + 同期変換候補 + 選択状態。
// 変換エンジンは CandidateEngine trait として注入する(テストではモック)。
// 候補は {text, engine_index} で持ち、リランキングで表示順を変えても
// 学習(select)には常にエンジン側の元 index を渡す。

/// 表示候補。engine_index はエンジンが返した並びでの位置(学習用の不変 ID)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub text: String,
    pub engine_index: usize,
}

/// 同期かな漢字変換エンジン(辞書ラティス)。メインスレッド専用・数ms で返る前提。
/// revision には composer の generation を渡し、select 時に同じ値で照合する。
pub trait CandidateEngine {
    fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String>;
    /// engine_index の候補を確定した(学習に反映してセッション終了)。照合失敗なら false
    fn select(&mut self, revision: u64, engine_index: usize) -> bool;
    /// 確定せずセッション終了(生ひらがな確定・破棄)
    fn cancel(&mut self);
}

pub struct Composer<E: CandidateEngine> {
    engine: E,
    /// 未確定のひらがなバッファ(常に入力の真実源)
    hiragana: String,
    /// 現バッファに対する n-best 候補(表示順。dedupe 済み)
    candidates: Vec<Candidate>,
    /// スペース候補送りで選択中の表示 index。None = 未操作
    selected: Option<usize>,
    /// Esc で生ひらがな表示に固定中か(次の入力で解除)
    raw: bool,
    /// ライブ変換(未選択でも第1候補を表示)
    live: bool,
    /// 入力変更のたびに進む。エンジンの revision 兼リランキング鮮度判定
    generation: u64,
}

impl<E: CandidateEngine> Composer<E> {
    pub fn new(engine: E) -> Self {
        Self {
            engine,
            hiragana: String::new(),
            candidates: Vec::new(),
            selected: None,
            raw: false,
            live: true,
            generation: 0,
        }
    }

    pub fn set_live(&mut self, live: bool) {
        self.live = live;
    }

    pub fn is_empty(&self) -> bool {
        self.hiragana.is_empty()
    }

    pub fn current_generation(&self) -> u64 {
        self.generation
    }

    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    pub fn display_hiragana(&self) -> &str {
        &self.hiragana
    }

    pub fn is_raw(&self) -> bool {
        self.raw
    }

    pub fn display(&self) -> &str {
        if self.raw {
            return &self.hiragana;
        }
        if let Some(i) = self.selected {
            return &self.candidates[i].text;
        }
        if self.live {
            if let Some(first) = self.candidates.first() {
                return &first.text;
            }
        }
        &self.hiragana
    }

    fn refresh(&mut self) {
        self.generation += 1;
        self.selected = None;
        self.raw = false;
        self.candidates.clear();
        if self.hiragana.is_empty() {
            return;
        }
        // 同一テキストは先頭のみ残す(候補ウィンドウの文字列選択を一意にする)
        let texts = self.engine.convert(self.generation, &self.hiragana);
        for (engine_index, text) in texts.into_iter().enumerate() {
            if !self.candidates.iter().any(|c| c.text == text) {
                self.candidates.push(Candidate { text, engine_index });
            }
        }
    }

    pub fn push_hiragana(&mut self, kana: &str) {
        self.hiragana.push_str(kana);
        self.refresh();
    }

    pub fn backspace(&mut self) -> bool {
        if self.hiragana.pop().is_none() {
            return false;
        }
        self.refresh();
        true
    }

    pub fn cycle(&mut self) -> bool {
        if self.candidates.is_empty() {
            return false;
        }
        self.raw = false;
        self.selected = Some(match self.selected {
            None => 0,
            Some(i) => (i + 1) % self.candidates.len(),
        });
        true
    }

    pub fn revert_raw(&mut self) {
        self.selected = None;
        self.raw = true;
    }

    pub fn commit(&mut self) -> String {
        let out = self.display().to_string();
        let committed_display_index = if self.raw {
            None
        } else {
            self.selected
                .or(if self.live && !self.candidates.is_empty() {
                    Some(0)
                } else {
                    None
                })
        };
        match committed_display_index {
            // 表示位置ではなくエンジン側の元 index を渡す(リランキング後も正しく学習)
            Some(i) => {
                let _ = self
                    .engine
                    .select(self.generation, self.candidates[i].engine_index);
            }
            None => self.engine.cancel(),
        }
        self.hiragana.clear();
        self.candidates.clear();
        self.selected = None;
        self.raw = false;
        self.generation += 1;
        out
    }

    pub fn apply_rerank(&mut self, generation: u64, display_index: usize) -> Option<String> {
        if generation != self.generation
            || self.selected.is_some()
            || self.raw
            || display_index == 0
            || display_index >= self.candidates.len()
        {
            return None;
        }
        let chosen = self.candidates.remove(display_index);
        self.candidates.insert(0, chosen); // engine_index は Candidate ごと移動するので不変
        Some(self.display().to_string())
    }
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test composer`
Expected: 全 PASS(imk.rs がまだ旧 API を参照してコンパイルエラーになる場合は `cargo test --lib` ではなく Task 3 と合わせて確認でもよいが、原則ここで `cargo test` が通るよう imk.rs 側の参照エラーが出るなら Task 3 を先に着手せず、このコミットは composer 単体のビルドが通る最小修正(imk.rs の一時的な `todo!()` 化はしない)まで含めない。実際には Task 3 と同一コミットにせず、コンパイルを通すため Task 3 の書き換えと連続で作業し、テスト確認後にファイル単位でコミットを分ける)

- [ ] **Step 5: Commit**

```bash
git add src/composer.rs
git commit -m "[:sparkles:]composer: 同期候補リスト状態機械に書き換え"
```

---

### Task 3: imk.rs — 配線の全面書き換え

**Files:**
- Modify: `src/imk.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: Task 1 の `kkc_bridge`、Task 2 の `Composer`/`CandidateEngine`
- Produces: `KkcEngine`(CandidateEngine 実装)、新しいキーイベントフロー

- [ ] **Step 1: 旧機構の削除**

`src/imk.rs` から以下を削除:
- `LlmKanjiConverter` と `KanjiConverter` 実装(use ごと)
- `EPOCH`/`now_ms`/`libdispatch` モジュール/`debounce_fired`/`schedule_tick`/`schedule_speculative_tick`/`on_debounce_tick`
- `handle_llm_result` の変換適用ロジック(Task 4 でリランキング用に作り直すため、いったん本体を空にせず**メソッドごと削除**し、`dispatch_llm_result` からの呼び出しもコメントアウトではなく削除。`dispatch_llm_result`/`CONTROLLERS`/`init_llm_dispatch` 自体は Task 4 で使うので残す)
- prewarm 呼び出し(`llm_bridge::prewarm_async`)

- [ ] **Step 2: KkcEngine と Ivars 変更**

```rust
use crate::composer::{CandidateEngine, Composer};
use crate::kkc_bridge;

/// 辞書エンジンへの薄いアダプタ。メインスレッド専用。controller_id を焼き込む。
pub struct KkcEngine {
    controller_id: u64,
}

impl CandidateEngine for KkcEngine {
    fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String> {
        kkc_bridge::convert(self.controller_id, revision, hiragana)
    }
    fn select(&mut self, revision: u64, engine_index: usize) -> bool {
        kkc_bridge::select(self.controller_id, revision, engine_index)
    }
    fn cancel(&mut self) {
        kkc_bridge::cancel(self.controller_id);
    }
}
```

`Ivars`:

```rust
pub struct Ivars {
    converter: RefCell<RomajiConverter>,
    composer: RefCell<Composer<KkcEngine>>,
    mode: RefCell<ConversionMode>,
    controller_id: u64,
    last_client: RefCell<*mut AnyObject>,
}
```

`init_with_server` の初期化を `Composer::new(KkcEngine { controller_id })` に変更。`ConversionMode` は `Live`(デフォルト)/`Space` を維持し、`Live` ⇔ `composer.set_live(true)`、`Space` ⇔ `set_live(false)`。初期値 `ConversionMode::Live`。

- [ ] **Step 3: input_text の書き換え**

```rust
        #[method(inputText:client:)]
        fn input_text(&self, text: *mut AnyObject, client: *mut AnyObject) -> Bool {
            let Some(input) = to_s(text) else {
                return Bool::NO;
            };
            *self.ivars().last_client.borrow_mut() = client;

            let mut converter = self.ivars().converter.borrow_mut();

            // スペース: 候補送り(未確定がなければ従来の playful space)
            if input == " " {
                let romaji_out = converter.flush();
                drop(converter);
                let mut composer = self.ivars().composer.borrow_mut();
                if let Some(out) = romaji_out {
                    composer.push_hiragana(&out);
                }
                if composer.is_empty() {
                    if let Some(converted) = playful_convert(" ") {
                        unsafe { insert_str(client, &converted) };
                        return Bool::YES;
                    }
                    return Bool::NO;
                }
                if composer.cycle() {
                    let marked = composer.display().to_string();
                    unsafe { set_marked_str(client, &marked) };
                    self.update_candidates_window(&composer);
                } else {
                    // 候補ゼロ(記号のみ等): ひらがな表示のまま
                    let marked = composer.display().to_string();
                    unsafe { set_marked_str(client, &marked) };
                }
                return Bool::YES;
            }

            // アルファベット: ローマ字合成 → composer 更新(同期変換)
            if !input.is_empty() && input.chars().all(|c| c.is_ascii_alphabetic()) {
                let mut kana = String::new();
                for ch in input.chars() {
                    if let Some(output) = converter.process_input(ch) {
                        kana.push_str(&output);
                    }
                }
                let romaji = converter.buffer().to_string();
                drop(converter);
                let mut composer = self.ivars().composer.borrow_mut();
                if !kana.is_empty() {
                    composer.push_hiragana(&kana);
                    self.request_rerank(&composer);
                }
                let marked = format!("{}{}", composer.display(), romaji);
                unsafe { set_marked_str(client, &marked) };
                self.hide_candidates_window();
                return Bool::YES;
            }

            // 非アルファベット(数字・記号): 未確定に取り込む
            let romaji_out = converter.flush();
            drop(converter);
            let mut composer = self.ivars().composer.borrow_mut();
            if let Some(out) = romaji_out {
                composer.push_hiragana(&out);
            }
            if composer.is_empty() {
                if let Some(converted) = playful_convert(input) {
                    unsafe { insert_str(client, &converted) };
                    return Bool::YES;
                }
                return Bool::NO;
            }
            composer.push_hiragana(input);
            self.request_rerank(&composer);
            let marked = composer.display().to_string();
            unsafe { set_marked_str(client, &marked) };
            self.hide_candidates_window();
            Bool::YES
        }
```

`request_rerank` は Task 4 で実装するため、このタスクでは空実装を置く:

```rust
impl NovakeyRInputController {
    fn request_rerank(&self, _composer: &Composer<KkcEngine>) {}
    fn update_candidates_window(&self, _composer: &Composer<KkcEngine>) {}
    fn hide_candidates_window(&self) {}
}
```

- [ ] **Step 4: did_command_by_selector の書き換え**

```rust
            // Backspace: romaji 優先、次に composer 末尾1文字
            if selector == sel!(deleteBackward:) {
                if !converter.buffer().is_empty() {
                    converter.handle_backspace();
                    let romaji = converter.buffer().to_string();
                    drop(converter);
                    let composer = self.ivars().composer.borrow();
                    let marked = format!("{}{}", composer.display(), romaji);
                    unsafe { set_marked_str(client, &marked) };
                    return Bool::YES;
                }
                drop(converter);
                let mut composer = self.ivars().composer.borrow_mut();
                if !composer.backspace() {
                    return Bool::NO;
                }
                let marked = composer.display().to_string();
                unsafe { set_marked_str(client, &marked) };
                self.hide_candidates_window();
                return Bool::YES;
            }

            // Enter: 確定
            if selector == sel!(insertNewline:) {
                if let Some(output) = converter.flush() {
                    drop(converter);
                    self.ivars().composer.borrow_mut().push_hiragana(&output);
                } else {
                    drop(converter);
                }
                let mut composer = self.ivars().composer.borrow_mut();
                if composer.is_empty() {
                    return Bool::NO;
                }
                let text = composer.commit();
                unsafe {
                    set_marked_str(client, "");
                    insert_str(client, &text);
                }
                self.hide_candidates_window();
                return Bool::YES;
            }

            // Esc: 1回目は生ひらがな表示、raw 中の2回目はアプリへパススルー
            if selector == sel!(cancelOperation:) {
                converter.clear();
                drop(converter);
                let mut composer = self.ivars().composer.borrow_mut();
                if composer.is_empty() {
                    return Bool::NO;
                }
                if composer.is_raw() {
                    return Bool::NO; // 2回目の Esc: 状態変更せずアプリへ
                }
                composer.revert_raw();
                let marked = composer.display().to_string();
                unsafe { set_marked_str(client, &marked) };
                self.hide_candidates_window();
                return Bool::YES;
            }

            // 左右矢印: 未確定中は無効
            if selector == sel!(moveLeft:) || selector == sel!(moveRight:) {
                let has_pending = !converter.buffer().is_empty()
                    || !self.ivars().composer.borrow().is_empty();
                return if has_pending { Bool::YES } else { Bool::NO };
            }
```

`commit_composition` は既存の形を維持(flush → push → commit → insert)し、最後に `self.hide_candidates_window();` を追加。

- [ ] **Step 5: モードメニューの live 同期**

`toggle_conversion_mode` の既存構造を維持し、`composer.set_live(new_mode == ConversionMode::Live)` と、未確定があれば `composer.display()` で marked 再描画(既存実装のまま)。`menu` のタイトルは「ライブ変換: オン」「ライブ変換: オフ(スペースで変換)」に変更。LLM 利用不可の disabled 項目は「LLMリランキング無効(Apple Intelligence を確認)」に文言変更。

- [ ] **Step 6: main.rs**

`init_llm_dispatch()` 呼び出しは維持(Task 4 のリランキング配線で使う)。

- [ ] **Step 7: ビルド+テスト+実機確認**

Run: `cargo test && mise run install`
Expected: 全 PASS。実機: 「kyouhaiitenki」→ 打鍵中からライブで「今日はいい天気」表示(数ms・ガタつきなし)→ スペースで候補送り → Enter 確定。Esc でひらがな。変換履歴が効くこと(同じ語の再変換で学習順)

- [ ] **Step 8: Commit**

```bash
git add src/imk.rs src/main.rs
git commit -m "[:sparkles:]imk: 辞書エンジンによるライブ変換と候補送りに配線変更"
```

---

### Task 4: IMKCandidates 候補ウィンドウ + LLM リランキング

**Files:**
- Modify: `src/imk.rs`
- Modify: `src/llm_bridge.rs`
- Modify: `swift/LLMBridge.swift`

**Interfaces:**
- Produces(C ABI): `novakey_llm_rerank(controller_id, generation, reading_ptr, reading_len, cands_ptr, cands_len)` — 候補(`\u{1F}` 区切り)から最適な番号を選び、既存 `novakey_llm_on_result` に `status=0, text="<番号>"` で返す
- Produces(Rust): `llm_bridge::rerank_async(controller_id, generation, reading: &str, candidates: &[String])`
- 削除: `convert_async` / `prewarm_async` / `STATUS_PARTIAL` / smoke_test の変換プロンプト(リランキングの smoke に置換)

- [ ] **Step 1: IMKCandidates の生成と表示**

`connect_imkserver` でサーバを保持するよう変更:

```rust
// IMKCandidates 定数(InputMethodKit ヘッダより)
const K_IMK_SINGLE_COLUMN_SCROLLING_PANEL: usize = 1; // kIMKSingleColumnScrollingCandidatePanel
const K_IMK_LOCATE_CANDIDATES_BELOW_HINT: usize = 2; // kIMKLocateCandidatesBelowHint

thread_local! {
    static SERVER: std::cell::Cell<*mut AnyObject> = const { std::cell::Cell::new(std::ptr::null_mut()) };
    static CANDIDATES_PANEL: std::cell::Cell<*mut AnyObject> = const { std::cell::Cell::new(std::ptr::null_mut()) };
}

pub unsafe fn connect_imkserver(name: *mut AnyObject, identifer: *mut AnyObject) {
    let server_class = IMKServer::class();
    let server_alloc: *mut AnyObject = msg_send![server_class, alloc];
    let server: *mut AnyObject =
        msg_send![server_alloc, initWithName: name, bundleIdentifier: identifer];
    SERVER.with(|s| s.set(server));
    NSLog!("IMKServer connection established");
}

/// 候補ウィンドウ(単一列スクロール)。遅延生成。
fn candidates_panel() -> *mut AnyObject {
    CANDIDATES_PANEL.with(|c| {
        if !c.get().is_null() {
            return c.get();
        }
        let server = SERVER.with(|s| s.get());
        if server.is_null() {
            return std::ptr::null_mut();
        }
        let panel: *mut AnyObject = unsafe {
            let alloc: *mut AnyObject = msg_send![objc2::class!(IMKCandidates), alloc];
            msg_send![alloc, initWithServer: server, panelType: K_IMK_SINGLE_COLUMN_SCROLLING_PANEL]
        };
        c.set(panel);
        panel
    })
}
```

controller 側の実装(Task 3 の空実装を置換):

```rust
impl NovakeyRInputController {
    fn update_candidates_window(&self, composer: &Composer<KkcEngine>) {
        let panel = candidates_panel();
        if panel.is_null() || composer.candidates().is_empty() {
            return;
        }
        unsafe {
            let _: () = msg_send![panel, updateCandidates];
            let _: () = msg_send![panel, show: K_IMK_LOCATE_CANDIDATES_BELOW_HINT];
        }
    }

    fn hide_candidates_window(&self) {
        let panel = candidates_panel();
        if panel.is_null() {
            return;
        }
        unsafe {
            let visible: Bool = msg_send![panel, isVisible];
            if visible.as_bool() {
                let _: () = msg_send![panel, hide];
            }
        }
    }
}
```

`declare_class!` に IMKCandidates が呼ぶメソッドを追加:

```rust
        // IMKCandidates が候補一覧を要求してくる
        #[method(candidates:)]
        fn candidates(&self, _sender: *mut AnyObject) -> *mut AnyObject {
            let composer = self.ivars().composer.borrow();
            unsafe {
                let array: *mut AnyObject = msg_send![objc2::class!(NSMutableArray), new];
                for cand in composer.candidates() {
                    let ns = NSString::from_str(&cand.text);
                    let _: () = msg_send![array, addObject: &*ns];
                }
                // +1 で作った array を +0 規約(autorelease)で返す
                msg_send![array, autorelease]
            }
        }

        // 候補ウィンドウ内でハイライトが移動した(矢印キー等)。marked text を同期する
        #[method(candidateSelectionChanged:)]
        fn candidate_selection_changed(&self, candidate_string: *mut AnyObject) {
            let client = *self.ivars().last_client.borrow();
            if client.is_null() {
                return;
            }
            let text: *mut AnyObject = unsafe { msg_send![candidate_string, string] };
            let Some(text) = to_s(text) else { return };
            let mut composer = self.ivars().composer.borrow_mut();
            // 候補は dedupe 済みなので文字列一致で表示 index が一意に決まる
            if let Some(idx) = composer.candidates().iter().position(|c| c.text == text) {
                while composer.selected_index() != Some(idx) {
                    if !composer.cycle() {
                        return;
                    }
                }
                let marked = composer.display().to_string();
                unsafe { set_marked_str(client, &marked) };
            }
        }

        // 候補ウィンドウでの選択確定(クリック/Enter)
        #[method(candidateSelected:)]
        fn candidate_selected(&self, candidate_string: *mut AnyObject) {
            let client = *self.ivars().last_client.borrow();
            if client.is_null() {
                return;
            }
            let text: *mut AnyObject = unsafe { msg_send![candidate_string, string] };
            let Some(text) = to_s(text) else { return };
            let mut composer = self.ivars().composer.borrow_mut();
            // 候補は dedupe 済みなので文字列一致で表示 index が一意に決まる
            if let Some(idx) = composer.candidates().iter().position(|c| c.text == text) {
                while composer.selected_index() != Some(idx) {
                    if !composer.cycle() {
                        break;
                    }
                }
            }
            let out = composer.commit();
            unsafe {
                set_marked_str(client, "");
                insert_str(client, &out);
            }
            self.hide_candidates_window();
        }
```

- [ ] **Step 2: LLMBridge.swift をリランキング専用に書き換え**

`novakey_llm_convert`/prewarm/セッションプール/streamResponse/isPlausibleConversion を削除し、以下に置換(instructions も置換):

```swift
private let rerankInstructions = """
あなたは日本語入力の候補選択器である。読みと変換候補の番号付きリストが与えられる。
文脈と読みに最も適合する候補の番号だけを出力せよ。説明・記号は一切付けない。
"""

@_cdecl("novakey_llm_rerank")
public func novakey_llm_rerank(
    _ controllerId: UInt64, _ generation: UInt64,
    _ readingPtr: UnsafePointer<UInt8>, _ readingLen: Int,
    _ candsPtr: UnsafePointer<UInt8>, _ candsLen: Int
) {
    let reading = String(decoding: UnsafeBufferPointer(start: readingPtr, count: readingLen), as: UTF8.self)
    let joined = String(decoding: UnsafeBufferPointer(start: candsPtr, count: candsLen), as: UTF8.self)
    let candidates = joined.split(separator: "\u{1F}").map(String.init)
    guard candidates.count > 1 else { return }
    DispatchQueue.main.async {
        registerLatest(controllerId, generation) // 既存 latest-wins 機構を流用
        let listing = candidates.enumerated().map { "\($0.offset): \($0.element)" }.joined(separator: "\n")
        let prompt = "読み: \(reading)\n候補:\n\(listing)\n番号:"
        let task = Task {
            do {
                let session = LanguageModelSession(instructions: rerankInstructions)
                let response = try await session.respond(
                    to: prompt,
                    options: GenerationOptions(sampling: .greedy, maximumResponseTokens: 4)
                )
                guard await isLatest(controllerId, generation) else { return }
                let digits = response.content.trimmingCharacters(in: .whitespacesAndNewlines)
                if let idx = Int(digits), idx >= 0, idx < candidates.count {
                    deliver(controllerId, generation, 0, "\(idx)")
                }
                // パース不能・範囲外は黙って捨てる(辞書の並びが残る)
            } catch {
                // リランキング失敗は無害: 何も送らない
            }
        }
        trackTask(controllerId, generation, task)
    }
}
```

注意: `GenerationOptions(sampling: .greedy, ...)` のイニシャライザが合わない場合は `GenerationOptions()` にフォールバックし `maximumResponseTokens` のみ設定する形を試す。`deliver`/availability 関数は既存のまま維持。

- [ ] **Step 3: llm_bridge.rs をリランキング専用に縮小**

`convert_async`/`prewarm_async`/`STATUS_PARTIAL` と対応 extern 宣言を削除し、追加:

```rust
extern "C" {
    fn novakey_llm_rerank(
        controller_id: u64,
        generation: u64,
        reading_ptr: *const u8,
        reading_len: usize,
        cands_ptr: *const u8,
        cands_len: usize,
    );
}

pub fn rerank_async(controller_id: u64, generation: u64, reading: &str, candidates: &[String]) {
    let joined = candidates.join("\u{1F}");
    unsafe {
        novakey_llm_rerank(
            controller_id,
            generation,
            reading.as_ptr(),
            reading.len(),
            joined.as_ptr(),
            joined.len(),
        );
    }
}
```

`smoke_test` を書き換え:

```rust
/// mise run test-llm 用: リランキングを1回実行して番号を表示。
pub fn smoke_test() {
    println!("availability: {:?}", availability());
    if availability() != Availability::Available {
        return;
    }
    set_result_handler(|_, _, status, text| {
        println!("status={} selected_index={}", status, text);
        std::process::exit(0);
    });
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(15));
        eprintln!("timeout: no result within 15s");
        std::process::exit(2);
    });
    let candidates = vec![
        "橋を渡る".to_string(),
        "箸を渡る".to_string(),
        "端を渡る".to_string(),
    ];
    rerank_async(0, 0, "はしをわたる", &candidates);
    unsafe {
        use objc2::runtime::AnyObject;
        use objc2::{class, msg_send};
        let run_loop: *mut AnyObject = msg_send![class!(NSRunLoop), mainRunLoop];
        let _: () = msg_send![run_loop, run];
    }
}
```

- [ ] **Step 4: imk.rs — リランキングの発行と適用**

Task 3 の空実装 `request_rerank` を置換:

```rust
    fn request_rerank(&self, composer: &Composer<KkcEngine>) {
        if !llm_enabled() || composer.candidates().len() < 2 {
            return;
        }
        let texts: Vec<String> = composer
            .candidates()
            .iter()
            .map(|c| c.text.clone())
            .collect();
        llm_bridge::rerank_async(
            self.ivars().controller_id,
            composer.current_generation(),
            composer.display_hiragana(),
            &texts,
        );
    }
```

`handle_llm_result` を新設(`dispatch_llm_result` から呼ぶ):

```rust
    fn handle_llm_result(&self, generation: u64, status: i32, text: &str) {
        if status != llm_bridge::STATUS_OK {
            return;
        }
        let Ok(index) = text.parse::<usize>() else {
            return;
        };
        let client = *self.ivars().last_client.borrow();
        if client.is_null() {
            return;
        }
        // 打鍵途中(ローマ字バッファ非空)は表示を触らない
        if !self.ivars().converter.borrow().buffer().is_empty() {
            return;
        }
        let mut composer = self.ivars().composer.borrow_mut();
        if let Some(marked) = composer.apply_rerank(generation, index) {
            unsafe { set_marked_str(client, &marked) };
        }
    }
```

- [ ] **Step 5: ビルド+テスト+スモーク**

Run: `cargo test && cargo build --release && mise run test-kkc && ./target/release/novakey_r --test-llm`
Expected: テスト全 PASS / kkc 候補一覧 OK / rerank が `selected_index=0`(橋)を返す

- [ ] **Step 6: 実機確認**

Run: `mise run install`
Expected: スペースで候補ウィンドウが出てクリック選択できる。ライブ表示中に(まれに)第1候補が文脈で入れ替わる。候補ウィンドウが不安定な場合は `update_candidates_window` の呼び出しをスペース時のみに限定して再確認

- [ ] **Step 7: Commit**

```bash
git add src/imk.rs src/llm_bridge.rs src/composer.rs swift/LLMBridge.swift
git commit -m "[:sparkles:]候補ウィンドウとLLMリランキングを追加"
```

---

### Task 5: クリーンアップとドキュメント

**Files:**
- Modify: `CLAUDE.md`, `mise.toml`(test-llm の説明更新)

- [ ] **Step 1: dead code 確認**

Run: `cargo build --release 2>&1 | grep warning`
Expected: 警告ゼロ。残っていれば削除(旧 `DEBOUNCE_MS` 等の未使用定数・関数)

- [ ] **Step 2: CLAUDE.md 更新**

Core Components / 変換フローを書き換え:

```markdown
- `src/composer.rs` - 未確定文の状態機械(ひらがなバッファ+同期n-best候補+選択状態、unit tested)
- `src/kkc_bridge.rs` + `swift/AzooKeyBridge/` - AzooKeyKanaKanjiConverter(辞書ラティス)への同期FFI
- `src/llm_bridge.rs` + `swift/LLMBridge.swift` - FoundationModels によるLLM候補リランキング(非同期・失敗無害)
- 変換フロー: 打鍵ごとに辞書エンジンで同期変換(数ms)しライブ表示。スペースで候補送り+候補ウィンドウ。
  Enter 確定(学習更新)。Esc で生ひらがな。LLM は裏で候補番号を選び未操作なら第1候補を差し替える
- 辞書リソースバンドルを .app の Contents/MacOS/ に同梱(mise run app が実施)
```

`[tasks.test-llm]` の description を「LLM候補リランキングのスモークテスト」に変更。

- [ ] **Step 3: 全体確認とコミット**

Run: `cargo test && mise run install` → 実機で入力・候補送り・確定・学習・モード切替・フォーカス切替を確認

```bash
git add CLAUDE.md mise.toml
git commit -m "[:books:]辞書エンジン移行のドキュメント更新"
```
