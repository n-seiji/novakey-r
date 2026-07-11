# LLMかな漢字変換 + 自動句読点 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** NovakeyR に Apple FoundationModels(オンデバイスLLM)を使ったひらがな→漢字変換+自動句読点挿入を、ライブ変換/スペース変換の2モードで追加する。

**Architecture:** 純Rustの `composer.rs`(状態機械: バッファ・世代管理・差分抑制)を中核に、`llm_bridge.rs` + `swift/LLMBridge.swift`(C-ABI FFI、結果は必ず main queue 上でコールバック)で FoundationModels を呼ぶ。`imk.rs` はメインスレッド専用の controller レジストリで非同期結果を安全に受ける。

**Tech Stack:** Rust (objc2 0.5 系, 既存構成), Swift 6 + FoundationModels.framework, build.rs で swiftc -emit-library, mise tasks。

## Global Constraints

- macOS 26 以降のみ LLM 有効。availability が `available` 以外は生ひらがな確定へフォールバック(spec 参照)
- FoundationModels コンテキスト 4096 トークン: プロンプトに含める確定済み文脈は直近 200 文字まで、未確定 500 文字超はライブ変換停止
- IMK API・controller ivars はメインスレッド専用。Rust コールバックは Swift 側が `DispatchQueue.main.async` 経由で呼ぶ契約
- コールバックが運ぶのは `(controller_id: u64, generation: u64, status: i32, text)` のみ。client ポインタは保持しない
- 確定/Esc/`commitComposition:` で世代を進めて in-flight を無効化
- コミットは既存規約の emoji プレフィックス(`[:sparkles:]` 等)
- テスト: `mise run test`(= `cargo test`)。composer は LLM を trait 注入でモック

## File Structure

- Create: `src/composer.rs` — 未確定文バッファ+世代管理+差分抑制の純Rust状態機械(LLM/時計は trait 注入)
- Create: `src/llm_bridge.rs` — Swift shim への FFI 宣言、Rust 側コールバックエントリ、controller レジストリ
- Create: `swift/LLMBridge.swift` — FoundationModels 呼び出し、`@_cdecl` エクスポート
- Create: `build.rs` — swiftc で LLMBridge をコンパイル・リンク
- Modify: `src/imk.rs` — 2モード配線、非同期結果の受信、編集キー仕様
- Modify: `src/main.rs` — モジュール追加
- Modify: `mise.toml` — `test-llm` タスク追加
- Modify: `Cargo.toml` — build.rs 有効化

---

### Task 1: composer.rs — 状態機械のコア(世代管理・確定・無効化)

**Files:**
- Create: `src/composer.rs`
- Modify: `src/main.rs`(`mod composer;` 追加)

**Interfaces:**
- Produces:
  - `pub trait KanjiConverter { fn request(&mut self, generation: u64, hiragana: &str, context: &str); }`
  - `pub struct Composer<C: KanjiConverter>`
  - `Composer::new(converter: C) -> Self`
  - `Composer::push_hiragana(&mut self, kana: &str)` — 未確定バッファへ追記
  - `Composer::backspace(&mut self) -> bool` — 末尾1文字削除、バッファ空なら false
  - `Composer::on_llm_result(&mut self, generation: u64, text: &str) -> Option<String>` — 現世代なら表示すべき marked text を返す。旧世代/同一表示なら None(差分抑制)
  - `Composer::commit(&mut self) -> String` — 現表示(変換済みがあればそれ、なければ生ひらがな)を返して全状態クリア+世代インクリメント
  - `Composer::cancel(&mut self) -> ()` — 生ひらがなに戻す(変換結果破棄+世代インクリメント)
  - `Composer::display(&self) -> &str` — 現在の marked text(変換結果 or 生ひらがな)
  - `Composer::is_empty(&self) -> bool`

- [ ] **Step 1: 失敗するテストを書く**

`src/composer.rs` の末尾に:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct MockLlm {
        requests: Rc<RefCell<Vec<(u64, String, String)>>>,
    }
    impl KanjiConverter for MockLlm {
        fn request(&mut self, generation: u64, hiragana: &str, context: &str) {
            self.requests
                .borrow_mut()
                .push((generation, hiragana.to_string(), context.to_string()));
        }
    }

    fn composer_with_log() -> (Composer<MockLlm>, Rc<RefCell<Vec<(u64, String, String)>>>) {
        let log = Rc::new(RefCell::new(Vec::new()));
        (Composer::new(MockLlm { requests: log.clone() }), log)
    }

    #[test]
    fn 入力なしでは空() {
        let (c, _) = composer_with_log();
        assert!(c.is_empty());
        assert_eq!(c.display(), "");
    }

    #[test]
    fn ひらがな追記でdisplayは生ひらがな() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        assert_eq!(c.display(), "きょう");
    }

    #[test]
    fn 現世代のllm結果はmarked_textとして返る() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょうはいいてんき");
        let gen = c.current_generation();
        assert_eq!(
            c.on_llm_result(gen, "今日はいい天気"),
            Some("今日はいい天気".to_string())
        );
        assert_eq!(c.display(), "今日はいい天気");
    }

    #[test]
    fn 旧世代のllm結果は破棄される() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        let old_gen = c.current_generation();
        c.push_hiragana("は"); // 入力で世代が進む
        assert_eq!(c.on_llm_result(old_gen, "今日"), None);
        assert_eq!(c.display(), "きょうは");
    }

    #[test]
    fn 同一表示の結果は差分抑制でnone() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        assert_eq!(c.on_llm_result(gen, "今日"), Some("今日".to_string()));
        // 同じ世代・同じ文字列の再着信(理論上)や再設定は None
        assert_eq!(c.on_llm_result(gen, "今日"), None);
    }

    #[test]
    fn commitで全状態クリアとin_flight無効化() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        assert_eq!(c.commit(), "きょう");
        assert!(c.is_empty());
        // commit 後に旧世代の結果が届いても無視される
        assert_eq!(c.on_llm_result(gen, "今日"), None);
    }

    #[test]
    fn 変換済みをcommitすると変換結果が返る() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        c.on_llm_result(gen, "今日");
        assert_eq!(c.commit(), "今日");
    }

    #[test]
    fn cancelで生ひらがなに戻る() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        c.on_llm_result(gen, "今日");
        c.cancel();
        assert_eq!(c.display(), "きょう");
        // cancel も in-flight を無効化
        assert_eq!(c.on_llm_result(gen, "今日"), None);
    }

    #[test]
    fn backspaceはひらがなバッファ末尾を削る() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        assert!(c.backspace());
        assert_eq!(c.display(), "きょ");
        c.backspace();
        c.backspace();
        assert!(!c.backspace()); // 空なら false
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test composer`
Expected: コンパイルエラー(`Composer` 未定義)

- [ ] **Step 3: 最小実装**

`src/composer.rs` の先頭に:

```rust
// composer.rs - 未確定文の状態機械: バッファ・世代管理・差分抑制
// LLM は KanjiConverter trait として注入する(テストではモック)。

/// LLM かな漢字変換への非同期リクエスト発行口。
/// 結果は Composer::on_llm_result に (generation, text) で戻す契約。
pub trait KanjiConverter {
    fn request(&mut self, generation: u64, hiragana: &str, context: &str);
}

pub struct Composer<C: KanjiConverter> {
    converter: C,
    /// 未確定のひらがなバッファ(常に入力の真実源)
    hiragana: String,
    /// 現世代の LLM 変換結果(あれば display はこちら)
    converted: Option<String>,
    /// 変換要求の世代。入力変更・確定・キャンセルで進む
    generation: u64,
}

impl<C: KanjiConverter> Composer<C> {
    pub fn new(converter: C) -> Self {
        Self { converter, hiragana: String::new(), converted: None, generation: 0 }
    }

    pub fn current_generation(&self) -> u64 {
        self.generation
    }

    pub fn is_empty(&self) -> bool {
        self.hiragana.is_empty()
    }

    pub fn display(&self) -> &str {
        self.converted.as_deref().unwrap_or(&self.hiragana)
    }

    fn invalidate(&mut self) {
        self.generation += 1;
        self.converted = None;
    }

    pub fn push_hiragana(&mut self, kana: &str) {
        // 変換結果が表示中なら、それを土台に追記する(スペース変換後の追加入力仕様)
        if let Some(converted) = self.converted.take() {
            self.hiragana = converted;
        }
        self.hiragana.push_str(kana);
        self.invalidate_keep_hiragana();
    }

    fn invalidate_keep_hiragana(&mut self) {
        self.generation += 1;
        self.converted = None;
    }

    pub fn backspace(&mut self) -> bool {
        if let Some(converted) = self.converted.take() {
            self.hiragana = converted;
        }
        if self.hiragana.pop().is_none() {
            return false;
        }
        self.invalidate_keep_hiragana();
        true
    }

    pub fn on_llm_result(&mut self, generation: u64, text: &str) -> Option<String> {
        if generation != self.generation {
            return None; // 旧世代: 破棄
        }
        if self.converted.as_deref() == Some(text) {
            return None; // 差分なし: setMarkedText を呼ばせない
        }
        self.converted = Some(text.to_string());
        Some(text.to_string())
    }

    pub fn commit(&mut self) -> String {
        let out = self.display().to_string();
        self.hiragana.clear();
        self.invalidate();
        out
    }

    pub fn cancel(&mut self) {
        self.converted = None;
        self.generation += 1;
    }
}
```

`src/main.rs` に `mod composer;` を追加(既存の `mod` 宣言の並びに合わせる)。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test composer`
Expected: 全テスト PASS(`converter` フィールド未使用の warning は Task 2 で解消)

- [ ] **Step 5: Commit**

```bash
git add src/composer.rs src/main.rs
git commit -m "[:sparkles:]composer: 世代管理・差分抑制の状態機械を追加"
```

---

### Task 2: composer.rs — デバウンスと変換要求の発行

**Files:**
- Modify: `src/composer.rs`
- Test: 同ファイル `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 1 の `Composer` / `KanjiConverter`
- Produces:
  - `Composer::tick(&mut self, now_ms: u64)` — 呼び出し側(imk.rs のタイマー)が現在時刻を渡す。入力停止 300ms 経過かつ未変換なら `converter.request(...)` を発行
  - `Composer::note_input_time(&mut self, now_ms: u64)` — push_hiragana/backspace 後に呼ぶ
  - `Composer::set_context(&mut self, ctx: &str)` — 確定済み文脈(直近200文字に内部で切り詰め)
  - `Composer::request_now(&mut self)` — スペース変換モード用: 即時に変換要求
  - `pub const DEBOUNCE_MS: u64 = 300;`
  - `pub const MAX_PENDING_CHARS: usize = 500;`(超過時 tick は要求を出さない)
  - `pub const MAX_CONTEXT_CHARS: usize = 200;`

- [ ] **Step 1: 失敗するテストを書く**

tests モジュールに追加:

```rust
    #[test]
    fn 入力後300ms経過のtickで変換要求が飛ぶ() {
        let (mut c, log) = composer_with_log();
        c.push_hiragana("きょう");
        c.note_input_time(1000);
        c.tick(1200); // まだ 200ms
        assert!(log.borrow().is_empty());
        c.tick(1300); // 300ms 経過
        let reqs = log.borrow();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].1, "きょう");
        assert_eq!(reqs[0].0, c.current_generation());
    }

    #[test]
    fn 同一世代で二重要求しない() {
        let (mut c, log) = composer_with_log();
        c.push_hiragana("きょう");
        c.note_input_time(1000);
        c.tick(1300);
        c.tick(1400);
        assert_eq!(log.borrow().len(), 1);
    }

    #[test]
    fn 変換済みなら再要求しない() {
        let (mut c, log) = composer_with_log();
        c.push_hiragana("きょう");
        c.note_input_time(1000);
        c.tick(1300);
        let gen = c.current_generation();
        c.on_llm_result(gen, "今日");
        c.tick(2000);
        assert_eq!(log.borrow().len(), 1);
    }

    #[test]
    fn 文脈は200文字に切り詰められる() {
        let (mut c, log) = composer_with_log();
        c.set_context(&"あ".repeat(300));
        c.push_hiragana("きょう");
        c.request_now();
        assert_eq!(log.borrow()[0].2.chars().count(), 200);
    }

    #[test]
    fn 未確定500文字超はtickで要求しない() {
        let (mut c, log) = composer_with_log();
        c.push_hiragana(&"か".repeat(501));
        c.note_input_time(1000);
        c.tick(2000);
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn request_nowは即時要求() {
        let (mut c, log) = composer_with_log();
        c.push_hiragana("きょう");
        c.request_now();
        assert_eq!(log.borrow().len(), 1);
    }

    #[test]
    fn 空バッファでは要求しない() {
        let (mut c, log) = composer_with_log();
        c.request_now();
        c.note_input_time(0);
        c.tick(1000);
        assert!(log.borrow().is_empty());
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test composer`
Expected: コンパイルエラー(`tick` 未定義)

- [ ] **Step 3: 実装**

`Composer` にフィールド追加(`new` も初期化を追記):

```rust
pub const DEBOUNCE_MS: u64 = 300;
pub const MAX_PENDING_CHARS: usize = 500;
pub const MAX_CONTEXT_CHARS: usize = 200;

// struct Composer に追加するフィールド:
//   last_input_ms: Option<u64>,
//   requested_generation: Option<u64>,
//   context: String,
```

```rust
    pub fn note_input_time(&mut self, now_ms: u64) {
        self.last_input_ms = Some(now_ms);
    }

    pub fn set_context(&mut self, ctx: &str) {
        // 直近 MAX_CONTEXT_CHARS 文字のみ保持(4096トークン制約対策)
        let chars: Vec<char> = ctx.chars().collect();
        let start = chars.len().saturating_sub(MAX_CONTEXT_CHARS);
        self.context = chars[start..].iter().collect();
    }

    fn should_request(&self) -> bool {
        !self.hiragana.is_empty()
            && self.converted.is_none()
            && self.requested_generation != Some(self.generation)
            && self.hiragana.chars().count() <= MAX_PENDING_CHARS
    }

    pub fn tick(&mut self, now_ms: u64) {
        let Some(last) = self.last_input_ms else { return };
        if now_ms.saturating_sub(last) < DEBOUNCE_MS {
            return;
        }
        if self.should_request() {
            self.requested_generation = Some(self.generation);
            self.converter.request(self.generation, &self.hiragana, &self.context);
        }
    }

    pub fn request_now(&mut self) {
        if !self.hiragana.is_empty() && self.requested_generation != Some(self.generation) {
            self.requested_generation = Some(self.generation);
            self.converter.request(self.generation, &self.hiragana, &self.context);
        }
    }
```

注意: `request_now` は `MAX_PENDING_CHARS` を超えていても発行する(スペース変換=明示操作のため)。`invalidate`/`invalidate_keep_hiragana` では `requested_generation` はリセット不要(世代不一致で自然に再要求可能になる)。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test composer`
Expected: 全 PASS

- [ ] **Step 5: Commit**

```bash
git add src/composer.rs
git commit -m "[:sparkles:]composer: デバウンスと変換要求発行を追加"
```

---

### Task 3: Swift shim + build.rs + FFI(llm_bridge.rs)

**Files:**
- Create: `swift/LLMBridge.swift`
- Create: `build.rs`
- Create: `src/llm_bridge.rs`
- Modify: `Cargo.toml`(`build = "build.rs"`)
- Modify: `src/main.rs`(`mod llm_bridge;`)
- Modify: `mise.toml`(`test-llm` タスク)

**Interfaces:**
- Consumes: なし(独立レイヤー)
- Produces(Rust 側):
  - `llm_bridge::availability() -> Availability`(enum: `Available` / `DeviceNotEligible` / `NotEnabled` / `ModelNotReady` / `Unknown`)
  - `llm_bridge::convert_async(controller_id: u64, generation: u64, hiragana: &str, context: &str)`
  - `llm_bridge::set_result_handler(f: fn(controller_id: u64, generation: u64, status: i32, text: &str))` — **メインスレッドで呼ばれる契約**
- Produces(C ABI、Swift 側実装):
  - `novakey_llm_availability() -> Int32`(0=available, 1=deviceNotEligible, 2=notEnabled, 3=modelNotReady, -1=unknown)
  - `novakey_llm_convert(controller_id: UInt64, generation: UInt64, input_ptr, input_len, ctx_ptr, ctx_len)`
  - Rust 側エクスポート `novakey_llm_on_result(controller_id: u64, generation: u64, status: i32, text_ptr: *const u8, text_len: usize)`(status: 0=ok, 1=error, 2=contextOverflow)

- [ ] **Step 1: Swift shim を書く**

`swift/LLMBridge.swift`:

```swift
// LLMBridge.swift - FoundationModels を C ABI で Rust に公開する。
// 契約: novakey_llm_on_result は必ず main queue 上で呼ぶ。
import Foundation
import FoundationModels

@_silgen_name("novakey_llm_on_result")
func novakey_llm_on_result(
    _ controllerId: UInt64, _ generation: UInt64,
    _ status: Int32, _ textPtr: UnsafePointer<UInt8>?, _ textLen: Int)

private let instructions = """
あなたは日本語入力システムの変換エンジンである。入力されたひらがな読み列を、\
文脈に合った自然な日本語表記(漢字・カタカナ・英字を適切に使用)に変換し、\
適切な句読点(。、)を補って出力せよ。読みに存在しない内容を追加しない。\
変換結果の文字列のみを返す。

例:
入力: きょうはいいてんきですね → 今日はいい天気ですね。
入力: あっぷるのはっぴょうかいをみた → Appleの発表会を見た。
入力: それでかえったあとにねた → それで、帰った後に寝た。
"""

// controller ごとにセッションを分離(文脈の混線防止)。メインスレッドからのみ触る。
private var sessions: [UInt64: LanguageModelSession] = [:]

private func session(for id: UInt64) -> LanguageModelSession {
    if let s = sessions[id] { return s }
    let s = LanguageModelSession(instructions: instructions)
    s.prewarm()
    sessions[id] = s
    return s
}

@_cdecl("novakey_llm_availability")
public func novakey_llm_availability() -> Int32 {
    switch SystemLanguageModel.default.availability {
    case .available: return 0
    case .unavailable(.deviceNotEligible): return 1
    case .unavailable(.appleIntelligenceNotEnabled): return 2
    case .unavailable(.modelNotReady): return 3
    default: return -1
    }
}

private func deliver(_ id: UInt64, _ gen: UInt64, _ status: Int32, _ text: String) {
    DispatchQueue.main.async {
        var bytes = Array(text.utf8)
        bytes.withUnsafeBufferPointer { buf in
            novakey_llm_on_result(id, gen, status, buf.baseAddress, buf.count)
        }
    }
}

@_cdecl("novakey_llm_convert")
public func novakey_llm_convert(
    _ controllerId: UInt64, _ generation: UInt64,
    _ inputPtr: UnsafePointer<UInt8>, _ inputLen: Int,
    _ ctxPtr: UnsafePointer<UInt8>, _ ctxLen: Int
) {
    let input = String(decoding: UnsafeBufferPointer(start: inputPtr, count: inputLen), as: UTF8.self)
    let ctx = String(decoding: UnsafeBufferPointer(start: ctxPtr, count: ctxLen), as: UTF8.self)
    // セッション辞書はメインスレッド専用なので、取得を main で行ってから Task へ
    DispatchQueue.main.async {
        let s = session(for: controllerId)
        let prompt = ctx.isEmpty ? "入力: \(input) →" : "直前の文脈: \(ctx)\n入力: \(input) →"
        Task {
            do {
                let response = try await s.respond(to: prompt)
                deliver(controllerId, generation, 0, response.content.trimmingCharacters(in: .whitespacesAndNewlines))
            } catch let e as LanguageModelSession.GenerationError {
                // コンテキスト超過は専用コード。セッションを作り直して次回に備える
                DispatchQueue.main.async { sessions[controllerId] = nil }
                if case .exceededContextWindowSize = e {
                    deliver(controllerId, generation, 2, "")
                } else {
                    deliver(controllerId, generation, 1, "\(e)")
                }
            } catch {
                deliver(controllerId, generation, 1, "\(error)")
            }
        }
    }
}
```

- [ ] **Step 2: build.rs を書く**

`build.rs`:

```rust
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let status = Command::new("swiftc")
        .args([
            "swift/LLMBridge.swift",
            "-emit-library",
            "-static",
            "-o",
        ])
        .arg(out_dir.join("libLLMBridge.a"))
        .args(["-target", "arm64-apple-macos26.0"])
        .status()
        .expect("swiftc の起動に失敗");
    assert!(status.success(), "LLMBridge.swift のコンパイルに失敗");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=LLMBridge");
    println!("cargo:rustc-link-lib=framework=FoundationModels");
    // swiftc static lib は Swift ランタイムのリンクパスが必要
    println!("cargo:rustc-link-search=native=/usr/lib/swift");
    println!("cargo:rerun-if-changed=swift/LLMBridge.swift");
}
```

`Cargo.toml` の `[package]` に `build = "build.rs"` を追加。

- [ ] **Step 3: llm_bridge.rs を書く**

`src/llm_bridge.rs`:

```rust
// llm_bridge.rs - Swift shim (LLMBridge.swift) への FFI。
// 契約: novakey_llm_on_result は Swift 側が main queue 上で呼ぶ。
use std::slice;
use std::str;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Available,
    DeviceNotEligible,
    NotEnabled,
    ModelNotReady,
    Unknown,
}

pub const STATUS_OK: i32 = 0;
pub const STATUS_ERROR: i32 = 1;
pub const STATUS_CONTEXT_OVERFLOW: i32 = 2;

extern "C" {
    fn novakey_llm_availability() -> i32;
    fn novakey_llm_convert(
        controller_id: u64,
        generation: u64,
        input_ptr: *const u8,
        input_len: usize,
        ctx_ptr: *const u8,
        ctx_len: usize,
    );
}

pub fn availability() -> Availability {
    match unsafe { novakey_llm_availability() } {
        0 => Availability::Available,
        1 => Availability::DeviceNotEligible,
        2 => Availability::NotEnabled,
        3 => Availability::ModelNotReady,
        _ => Availability::Unknown,
    }
}

pub fn convert_async(controller_id: u64, generation: u64, hiragana: &str, context: &str) {
    unsafe {
        novakey_llm_convert(
            controller_id,
            generation,
            hiragana.as_ptr(),
            hiragana.len(),
            context.as_ptr(),
            context.len(),
        );
    }
}

type ResultHandler = fn(controller_id: u64, generation: u64, status: i32, text: &str);

// fn ポインタを usize で保持(メインスレッドでのみ読み書きされるが、
// 型として Sync が必要なため atomic を使う)
static RESULT_HANDLER: AtomicUsize = AtomicUsize::new(0);

pub fn set_result_handler(f: ResultHandler) {
    RESULT_HANDLER.store(f as usize, Ordering::SeqCst);
}

/// Swift 側から main queue 上で呼ばれる。
#[no_mangle]
pub extern "C" fn novakey_llm_on_result(
    controller_id: u64,
    generation: u64,
    status: i32,
    text_ptr: *const u8,
    text_len: usize,
) {
    let handler = RESULT_HANDLER.load(Ordering::SeqCst);
    if handler == 0 {
        return;
    }
    let text = if text_ptr.is_null() || text_len == 0 {
        ""
    } else {
        let bytes = unsafe { slice::from_raw_parts(text_ptr, text_len) };
        str::from_utf8(bytes).unwrap_or("")
    };
    let f: ResultHandler = unsafe { std::mem::transmute(handler) };
    f(controller_id, generation, status, text);
}
```

`src/main.rs` に `mod llm_bridge;` を追加。

- [ ] **Step 4: ビルドが通ることを確認**

Run: `cargo build --release`
Expected: リンクまで成功(FoundationModels / Swift ランタイム解決を含む)。失敗時は `-target` のバージョンや `swiftc` の SDK 指定(`-sdk $(xcrun --show-sdk-path)`)を調整

- [ ] **Step 5: test-llm スモークテストを追加**

`mise.toml` に追加(既存タスクの書式に合わせる):

```toml
[tasks.test-llm]
description = "FoundationModels availability + 1回変換のスモークテスト"
run = "cargo run --release -- --test-llm"
```

`src/main.rs` の冒頭で `--test-llm` 引数を処理:

```rust
    if std::env::args().any(|a| a == "--test-llm") {
        llm_bridge::smoke_test();
        return;
    }
```

`src/llm_bridge.rs` に追加:

```rust
/// mise run test-llm 用: availability を表示し1回変換して終了。
pub fn smoke_test() {
    println!("availability: {:?}", availability());
    if availability() != Availability::Available {
        return;
    }
    set_result_handler(|_, _, status, text| {
        println!("status={} text={}", status, text);
        std::process::exit(0);
    });
    convert_async(0, 0, "きょうはいいてんきですね", "");
    // main queue のコールバックを受けるため run loop を回す
    unsafe {
        use objc2::{class, msg_send};
        use objc2::runtime::AnyObject;
        let run_loop: *mut AnyObject = msg_send![class!(NSRunLoop), mainRunLoop];
        let _: () = msg_send![run_loop, run];
    }
}
```

- [ ] **Step 6: スモークテスト実行**

Run: `mise run test-llm`
Expected: `availability: Available` と変換結果(例: `今日はいい天気ですね。`)が出力される

- [ ] **Step 7: Commit**

```bash
git add swift/LLMBridge.swift build.rs src/llm_bridge.rs src/main.rs Cargo.toml mise.toml
git commit -m "[:sparkles:]FoundationModels への Swift shim と FFI を追加"
```

---

### Task 4: imk.rs — controller レジストリと非同期結果の受信

**Files:**
- Modify: `src/imk.rs`
- Modify: `src/main.rs`(初期化時に handler 登録)

**Interfaces:**
- Consumes: `llm_bridge::set_result_handler`, `Composer`(Task 1-2)
- Produces:
  - `imk::init_llm_dispatch()` — main.rs から一度呼ぶ。`set_result_handler(dispatch_llm_result)` を登録
  - controller は `Ivars` に `controller_id: u64` と `composer: RefCell<Composer<LlmKanjiConverter>>` と `last_client: RefCell<*mut AnyObject>` を持つ
  - メインスレッド専用レジストリ: `thread_local! { static CONTROLLERS: RefCell<HashMap<u64, Id<NovakeyRInputController>>> }`
  - `LlmKanjiConverter`(`KanjiConverter` 実装、`llm_bridge::convert_async` を呼ぶだけの薄いアダプタ)

- [ ] **Step 1: LlmKanjiConverter とレジストリを実装**

`src/imk.rs` に追加:

```rust
use crate::composer::{Composer, KanjiConverter};
use crate::llm_bridge;

/// composer からの変換要求を FFI に流すアダプタ。controller_id を焼き込む。
pub struct LlmKanjiConverter {
    controller_id: u64,
}

impl KanjiConverter for LlmKanjiConverter {
    fn request(&mut self, generation: u64, hiragana: &str, context: &str) {
        llm_bridge::convert_async(self.controller_id, generation, hiragana, context);
    }
}

use std::sync::atomic::{AtomicU64, Ordering};
static NEXT_CONTROLLER_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    // メインスレッド専用。callback は client ポインタを持たず、
    // ここから controller を引き直す(use-after-free 防止)。
    static CONTROLLERS: RefCell<HashMap<u64, Id<NovakeyRInputController>>> =
        RefCell::new(HashMap::new());
}

/// Swift 側から main queue 上で呼ばれる結果ディスパッチャ。
fn dispatch_llm_result(controller_id: u64, generation: u64, status: i32, text: &str) {
    CONTROLLERS.with(|c| {
        let controllers = c.borrow();
        let Some(controller) = controllers.get(&controller_id) else {
            return; // controller は解放済み: 結果を破棄
        };
        controller.handle_llm_result(generation, status, text);
    });
}

pub fn init_llm_dispatch() {
    llm_bridge::set_result_handler(dispatch_llm_result);
}
```

- [ ] **Step 2: Ivars 拡張と登録/解除**

`Ivars` を拡張し、`init_with_server` で id 発行+レジストリ登録。`handle_llm_result` は `declare_class!` の外の `impl NovakeyRInputController` に書く:

```rust
pub struct Ivars {
    converter: RefCell<RomajiConverter>,
    composer: RefCell<Composer<LlmKanjiConverter>>,
    controller_id: u64,
    // 直近の inputText/didCommand で受け取った client。
    // handle_llm_result はメインスレッドで同期的に呼ばれるため、
    // 直近のイベントサイクル内でのみ有効な弱い参照として扱う。
    last_client: RefCell<*mut AnyObject>,
}
```

`init_with_server` 内:

```rust
            let controller_id = NEXT_CONTROLLER_ID.fetch_add(1, Ordering::SeqCst);
            let this = this.set_ivars(Ivars {
                converter: RefCell::new(RomajiConverter::new()),
                composer: RefCell::new(Composer::new(LlmKanjiConverter { controller_id })),
                controller_id,
                last_client: RefCell::new(std::ptr::null_mut()),
            });
            let this: Option<Id<Self>> = unsafe {
                msg_send_id![super(this), initWithServer: server, delegate: delegate, client: client]
            };
            if let Some(ref obj) = this {
                CONTROLLERS.with(|c| c.borrow_mut().insert(controller_id, obj.clone()));
            }
            this
```

`declare_class!` の外に:

```rust
impl NovakeyRInputController {
    fn handle_llm_result(&self, generation: u64, status: i32, text: &str) {
        if status != llm_bridge::STATUS_OK {
            unsafe { NSLog!("LLM error status=%d", status) };
            return; // ひらがな表示を維持(フォールバック)
        }
        let client = *self.ivars().last_client.borrow();
        if client.is_null() {
            return;
        }
        let mut composer = self.ivars().composer.borrow_mut();
        if let Some(marked) = composer.on_llm_result(generation, text) {
            unsafe { set_marked_str(client, &marked) };
        }
    }
}
```

注意: `Id<NovakeyRInputController>` のレジストリ保持は controller を延命させる(強参照)。IMK は controller をクライアントごとに再利用するため実害は小さいが、`deallocate` 時のレジストリ解除は行わない(v1 の割り切り。メモリは controller 数 × 小サイズ)。

- [ ] **Step 3: main.rs で初期化**

`src/main.rs` の IMKServer 接続前に `imk::init_llm_dispatch();` を追加。

- [ ] **Step 4: ビルド確認**

Run: `cargo build --release && cargo test`
Expected: ビルド成功、既存テスト PASS

- [ ] **Step 5: Commit**

```bash
git add src/imk.rs src/main.rs
git commit -m "[:sparkles:]imk: controllerレジストリとLLM結果ディスパッチを追加"
```

---

### Task 5: imk.rs — ライブ変換モード配線(デバウンスタイマー含む)

**Files:**
- Modify: `src/imk.rs`

**Interfaces:**
- Consumes: Task 1-4 全部
- Produces: ライブ変換の完全なキーイベントフロー

**設計メモ:** デバウンスは「入力から 300ms 後に一度だけ発火するワンショット」を、Swift 側に頼らず `dispatch_after`(libdispatch)で実装する。`now_ms` は `mach_absolute_time` ではなく `std::time::Instant` を起点とした経過 ms を使う。

- [ ] **Step 1: 時刻ヘルパとデバウンス発火を実装**

```rust
use std::time::Instant;
use once_cell::sync::Lazy;

static EPOCH: Lazy<Instant> = Lazy::new(Instant::now);

fn now_ms() -> u64 {
    EPOCH.elapsed().as_millis() as u64
}

/// 300ms 後にメインスレッドで composer.tick を打つワンショット。
/// dispatch_after(main queue) を使う。controller_id 経由で引き直す。
fn schedule_tick(controller_id: u64) {
    use dispatch2::{DispatchQueue, DispatchTime};
    let delay = DispatchTime::now() + std::time::Duration::from_millis(crate::composer::DEBOUNCE_MS + 10);
    DispatchQueue::main().after(delay, move || {
        CONTROLLERS.with(|c| {
            if let Some(controller) = c.borrow().get(&controller_id) {
                controller.on_debounce_tick();
            }
        });
    });
}
```

依存追加: `Cargo.toml` に `dispatch2 = "0.1"`(objc2 ファミリーの libdispatch バインディング)。もし API が合わない場合は `unsafe extern "C" { fn dispatch_after(...) }` の生 FFI で同等を実装。

```rust
impl NovakeyRInputController {
    fn on_debounce_tick(&self) {
        self.ivars().composer.borrow_mut().tick(now_ms());
    }
}
```

- [ ] **Step 2: inputText を composer 経由に書き換える**

`input_text` のアルファベット分岐を変更。かなは commit せず composer に積む:

```rust
            // last_client を更新(handle_llm_result 用)
            *self.ivars().last_client.borrow_mut() = client;

            if !input.is_empty() && input.chars().all(|c| c.is_ascii_alphabetic()) {
                let mut kana = String::new();
                for ch in input.chars() {
                    if let Some(output) = converter.process_input(ch) {
                        kana.push_str(&output);
                    }
                }
                drop(converter);
                let mut composer = self.ivars().composer.borrow_mut();
                if !kana.is_empty() {
                    composer.push_hiragana(&kana);
                }
                composer.note_input_time(now_ms());
                // 表示 = 変換結果orひらがな + 未変換romaji
                let romaji = self.ivars().converter.borrow().buffer().to_string();
                let marked = format!("{}{}", composer.display(), romaji);
                unsafe { set_marked_str(client, &marked) };
                schedule_tick(self.ivars().controller_id);
                return Bool::YES;
            }
```

非アルファベット入力(数字・記号)は、pending romaji を flush して composer に積んだ上で composer.push_hiragana(その文字) し、同様に marked 表示(playful_convert は composer が空のときのみ従来動作)。

- [ ] **Step 3: 編集キーを spec 仕様に合わせる**

`did_command_by_selector` を書き換え:

```rust
            // Backspace: romaji バッファ優先、次に composer 末尾1文字
            if selector == sel!(deleteBackward:) {
                if !converter.buffer().is_empty() {
                    converter.handle_backspace();
                } else {
                    drop(converter);
                    let mut composer = self.ivars().composer.borrow_mut();
                    if !composer.backspace() {
                        return Bool::NO; // 未確定なし: アプリへパススルー
                    }
                    composer.note_input_time(now_ms());
                    let marked = composer.display().to_string();
                    unsafe { set_marked_str(client, &marked) };
                    schedule_tick(self.ivars().controller_id);
                    return Bool::YES;
                }
                // romaji を削った場合の再表示
                let romaji = converter.buffer().to_string();
                drop(converter);
                let composer = self.ivars().composer.borrow();
                let marked = format!("{}{}", composer.display(), romaji);
                unsafe { set_marked_str(client, &marked) };
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
                    return Bool::NO; // 未確定なし: 改行をアプリへ
                }
                let text = composer.commit(); // in-flight 無効化込み
                composer.set_context(&text);
                unsafe {
                    set_marked_str(client, "");
                    insert_str(client, &text);
                }
                return Bool::YES; // 改行自体は挿入しない(確定のみ)
            }

            // Esc: 生ひらがなに戻す
            if selector == sel!(cancelOperation:) {
                converter.clear();
                drop(converter);
                let mut composer = self.ivars().composer.borrow_mut();
                if composer.is_empty() {
                    return Bool::NO;
                }
                composer.cancel();
                let marked = composer.display().to_string();
                unsafe { set_marked_str(client, &marked) };
                return Bool::YES;
            }

            // 左右矢印: 未確定中は無効(v1 はカーソル移動非対応)
            if selector == sel!(moveLeft:) || selector == sel!(moveRight:) {
                let has_pending = !converter.buffer().is_empty()
                    || !self.ivars().composer.borrow().is_empty();
                return if has_pending { Bool::YES } else { Bool::NO };
            }
```

`commit_composition` も composer 対応(現表示をそのまま確定+in-flight 無効化):

```rust
        #[method(commitComposition:)]
        fn commit_composition(&self, client: *mut AnyObject) {
            let mut converter = self.ivars().converter.borrow_mut();
            let romaji_out = converter.flush();
            drop(converter);
            let mut composer = self.ivars().composer.borrow_mut();
            if let Some(out) = romaji_out {
                composer.push_hiragana(&out);
            }
            if !composer.is_empty() {
                let text = composer.commit();
                unsafe { insert_str(client, &text) };
            }
        }
```

- [ ] **Step 4: ビルド+ユニットテスト**

Run: `cargo build --release && cargo test`
Expected: 成功

- [ ] **Step 5: 実機確認**

Run: `mise run install` → 再ログインまたは入力ソース再追加 → テキストエディタで「kyouhaiiotenki」等を入力
Expected: 下線付きひらがな表示 → 約300ms 後に「今日はいいお天気」等に置換 → Enter で確定。Esc で生ひらがな復帰。`mise run logs` でエラーがないこと

- [ ] **Step 6: Commit**

```bash
git add src/imk.rs Cargo.toml Cargo.lock
git commit -m "[:sparkles:]ライブ変換モード: LLM変換とデバウンスを配線"
```

---

### Task 6: スペース変換モードとモード切替メニュー

**Files:**
- Modify: `src/imk.rs`

**Interfaces:**
- Consumes: Task 5 のフロー
- Produces:
  - `Ivars` に `mode: RefCell<ConversionMode>`(`enum ConversionMode { Live, Space }`、デフォルト `Live`)
  - IMK メニュー(`menu` メソッド)に「ライブ変換」トグル項目

- [ ] **Step 1: モード enum とスペースキー処理**

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConversionMode {
    Live,
    Space,
}
```

`input_text` の先頭付近(alphabetic 分岐の前)にスペース処理を追加:

```rust
            if input == " " {
                let romaji_out = converter.flush();
                drop(converter);
                let mut composer = self.ivars().composer.borrow_mut();
                if let Some(out) = romaji_out {
                    composer.push_hiragana(&out);
                }
                if composer.is_empty() {
                    // 未確定なし: playful space にフォールバック
                    if let Some(converted) = playful_convert(" ") {
                        unsafe { insert_str(client, &converted) };
                        return Bool::YES;
                    }
                    return Bool::NO;
                }
                match *self.ivars().mode.borrow() {
                    ConversionMode::Space => {
                        if composer.is_converted() {
                            // 2度目のスペース: 生ひらがなへトグル
                            composer.cancel();
                        } else {
                            composer.request_now();
                        }
                        let marked = composer.display().to_string();
                        unsafe { set_marked_str(client, &marked) };
                    }
                    ConversionMode::Live => {
                        // ライブ変換中のスペースは即時変換のショートカット
                        composer.request_now();
                    }
                }
                return Bool::YES;
            }
```

composer に補助メソッド追加(テスト込み):

```rust
    pub fn is_converted(&self) -> bool {
        self.converted.is_some()
    }
```

```rust
    #[test]
    fn is_convertedは変換結果保持中のみtrue() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        assert!(!c.is_converted());
        let gen = c.current_generation();
        c.on_llm_result(gen, "今日");
        assert!(c.is_converted());
    }
```

ライブモードでは `schedule_tick` が回っているため通常スペース不要だが、即時変換ショートカットとして扱う。

- [ ] **Step 2: モード切替メニュー**

`declare_class!` に `menu` と action を追加:

```rust
        #[method(menu)]
        fn menu(&self) -> *mut AnyObject {
            unsafe {
                let menu: *mut AnyObject = msg_send![class!(NSMenu), new];
                let title = NSString::from_str(match *self.ivars().mode.borrow() {
                    ConversionMode::Live => "ライブ変換: オン",
                    ConversionMode::Space => "ライブ変換: オフ(スペース変換)",
                });
                let item: *mut AnyObject = msg_send![class!(NSMenuItem), alloc];
                let item: *mut AnyObject = msg_send![
                    item,
                    initWithTitle: &*title,
                    action: sel!(toggleConversionMode:),
                    keyEquivalent: &*NSString::from_str("")
                ];
                let _: () = msg_send![menu, addItem: item];
                menu
            }
        }

        #[method(toggleConversionMode:)]
        fn toggle_conversion_mode(&self, _sender: *mut AnyObject) {
            let mut mode = self.ivars().mode.borrow_mut();
            *mode = match *mode {
                ConversionMode::Live => ConversionMode::Space,
                ConversionMode::Space => ConversionMode::Live,
            };
        }
```

Space モード時は `input_text` で `schedule_tick` を呼ばない(デバウンス変換を止める)条件分岐を追加:

```rust
                if *self.ivars().mode.borrow() == ConversionMode::Live {
                    schedule_tick(self.ivars().controller_id);
                }
```

- [ ] **Step 3: 起動時 availability チェックとフォールバック**

`init_llm_dispatch` 後に一度 `llm_bridge::availability()` を確認し、`Available` 以外なら `LLM_ENABLED`(main-thread 用 `thread_local` bool)を false に。false の場合:
- `schedule_tick` / `request_now` を呼ばない(composer はひらがな表示のまま)
- メニュー項目に「LLM利用不可: <理由>」を disabled 表示で追加

```rust
thread_local! {
    static LLM_ENABLED: std::cell::Cell<bool> = std::cell::Cell::new(true);
}
```

- [ ] **Step 4: ビルド+テスト+実機確認**

Run: `cargo test && mise run install`
Expected: メニューからモード切替でき、スペース変換モードで「ひらがな→スペース→変換→スペース→ひらがな」のトグルが動く

- [ ] **Step 5: Commit**

```bash
git add src/imk.rs src/composer.rs
git commit -m "[:sparkles:]スペース変換モードとモード切替メニューを追加"
```

---

### Task 7: タイムアウト・仕上げ・ドキュメント

**Files:**
- Modify: `src/composer.rs`(タイムアウト)
- Modify: `CLAUDE.md`(アーキテクチャ節の更新)

**Interfaces:**
- Consumes: 全タスク
- Produces: `Composer::tick` がタイムアウト再送も担う

- [ ] **Step 1: タイムアウトのテストを書く**

```rust
    #[test]
    fn 要求後3秒応答がなければ再要求できる() {
        let (mut c, log) = composer_with_log();
        c.push_hiragana("きょう");
        c.note_input_time(1000);
        c.tick(1300); // 要求1
        c.tick(2000); // まだ待つ
        assert_eq!(log.borrow().len(), 1);
        c.tick(4400); // 3s 超 → 世代を進めて再要求
        assert_eq!(log.borrow().len(), 2);
        let reqs = log.borrow();
        assert!(reqs[1].0 > reqs[0].0); // 新世代
    }
```

- [ ] **Step 2: 実装**

`pub const TIMEOUT_MS: u64 = 3000;` を追加。`tick` に要求時刻(`requested_at_ms: Option<u64>`)を記録し、`now - requested_at >= TIMEOUT_MS` かつ未変換なら `generation += 1` して再要求(再要求も `requested_at_ms` を更新)。`request_now`/`should_request` 成立時に `requested_at_ms = Some(now)`(`tick` は now を持つので記録可能。`request_now` は時刻なしのため `requested_at_ms = None` のままとし、タイムアウト再送はライブ変換のみの機能とする)。

Run: `cargo test composer`
Expected: PASS

- [ ] **Step 3: CLAUDE.md 更新**

`CLAUDE.md` の Architecture / Core Components に追記:

```markdown
- `src/composer.rs` - 未確定文の状態機械(世代管理・デバウンス・差分抑制、unit tested)
- `src/llm_bridge.rs` - FoundationModels への FFI(結果は main queue 経由で戻る契約)
- `swift/LLMBridge.swift` - FoundationModels 呼び出し(build.rs で swiftc コンパイル)
- 変換モード: ライブ変換(デフォルト)/ スペース変換。入力ソースメニューで切替
```

- [ ] **Step 4: 全体確認とコミット**

Run: `cargo test && mise run install` → 両モード・フォーカス切替(アプリ間移動で確定されること)・Apple Intelligence オフ時のフォールバックを手動確認

```bash
git add src/composer.rs CLAUDE.md
git commit -m "[:sparkles:]LLMタイムアウト再送とドキュメント更新"
```
