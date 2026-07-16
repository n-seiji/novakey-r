# 投機変換ストリーム化 Implementation Plan (gpt-5.6-sol レビュー反映済み)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 入力中は生ひらがな表示のまま裏で LLM に投機変換させ、スペース確定時にはキャッシュ済み結果を即時表示、未完なら streamResponse で流し込む。

**Architecture:** composer に「reveal(開示)」状態を追加し、LLM 結果は届いても表示せずキャッシュする。スペースで reveal し、キャッシュヒットなら即置換、ミスならストリーミング partial を marked text に流す。Swift 側は controller ごとに「次の respond 用セッション」を1つ先行生成して固定 prefix で prewarm し、respond 1回ごとに使い捨てる(履歴蓄積なし + prewarm 有効の両立)。

**Tech Stack:** 既存構成(Rust objc2 0.5 / Swift 6 FoundationModels / build.rs)。新規依存なし。

## Global Constraints

- 既存の契約を維持: `novakey_llm_on_result` は必ず main queue 上で呼ぶ / 既存 C-ABI シグネチャ互換(status 追加のみ) / 検証ゲート `isPlausibleConversion` は final にのみ適用
- latest-wins(世代チェック)は維持。既存の `isLatest` は `@MainActor` のため Task 内からは `await isLatest(...)` で呼ぶ(同期呼び出しは Swift 6 の actor isolation エラー)
- セッションは respond 1回で使い捨て(transcript 蓄積禁止)。ただし次回用セッションの先行 prewarm は可
- スペース変換モードをデフォルトのまま維持。ライブモードは「常時 reveal 扱い」で従来動作
- テスト: `cargo test` 全通過。composer のロジックは全てモック LLM でユニットテスト
- コミット規約: emoji プレフィックス(`[:sparkles:]` 等)
- status コード: 0=final ok, 1=error(検証失敗・空出力含む), 2=contextOverflow, 3=partial(新規)
- ストリーム型(正確な API): 戻り値 `LanguageModelSession.ResponseStream<String>`、要素 `...ResponseStream<String>.Snapshot`、`snapshot.content: String.PartiallyGenerated`(String では `PartiallyGenerated == String`)

## File Structure

- Modify: `src/composer.rs` — reveal 状態・投機キャッシュ・partial/failure 受信・再要求経路
- Modify: `src/imk.rs` — Space モードでも tick を回す / スペース=reveal / partial・failure 表示 / モード切替時の再描画
- Modify: `src/llm_bridge.rs` — `STATUS_PARTIAL` / smoke_test の terminal 対応
- Modify: `swift/LLMBridge.swift` — セッション先行 prewarm プール / `streamResponse` 化 / 空出力の失敗扱い

---

### Task 1: composer.rs — reveal 状態機械と投機キャッシュ

**Files:**
- Modify: `src/composer.rs`

**Interfaces:**
- Produces:
  - `pub enum RevealOutcome { Shown(String), Pending, Empty }`
  - `Composer::reveal(&mut self) -> RevealOutcome` — キャッシュ済みなら `Shown(変換結果)`、未着なら `Pending`(未要求ならここで発行、**要求済みでも結果がなければ再発行**)、バッファ空なら `Empty`
  - `Composer::unreveal(&mut self)` — 2度目のスペースで生ひらがなへ戻す(世代は進めない: キャッシュ維持)
  - `Composer::is_revealed(&self) -> bool` — `revealed || live`
  - `Composer::on_llm_partial(&self, generation: u64, text: &str) -> Option<String>` — 現世代かつ reveal 済みのときのみ表示文字列を返す(キャッシュしない)
  - `Composer::on_llm_failure(&mut self, generation: u64) -> Option<String>` — 現世代の terminal 失敗。`requested_generation`/`requested_at_ms` を解除(再要求可能に)し、reveal 済みなら生ひらがな文字列を返す(表示ロールバック用)
  - `Composer::set_live(&mut self, live: bool)`
  - `Composer::display_hiragana(&self) -> &str` / `Composer::context(&self) -> &str` — 単純 getter
  - 変更: `on_llm_result` — 結果は常にキャッシュ。表示文字列を返すのは reveal 済みのときのみ
  - 変更: `display()` — reveal 済みかつ変換ありなら変換結果、それ以外は生ひらがな
  - 変更: `push_hiragana`/`backspace` — reveal 解除 + 世代前進。**変換結果を土台に追記する挙動は削除**(hiragana バッファが常に真実源)
  - 変更: `commit()`/`cancel()` — **両方で `revealed = false` にリセット**

- [ ] **Step 1: 失敗するテストを書く**

tests モジュールに追加:

```rust
    #[test]
    fn 未revealならllm結果はキャッシュされるが表示されない() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        assert_eq!(c.on_llm_result(gen, "今日"), None);
        assert_eq!(c.display(), "きょう");
        assert!(c.is_converted());
    }

    #[test]
    fn revealでキャッシュヒットなら即shown() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        c.on_llm_result(gen, "今日");
        assert_eq!(c.reveal(), RevealOutcome::Shown("今日".to_string()));
        assert_eq!(c.display(), "今日");
        assert!(c.is_revealed());
    }

    #[test]
    fn revealでキャッシュミスならpendingで要求発行() {
        let (mut c, log) = composer_with_log();
        c.push_hiragana("きょう");
        assert_eq!(c.reveal(), RevealOutcome::Pending);
        assert_eq!(log.borrow().len(), 1);
        let gen = c.current_generation();
        assert_eq!(c.on_llm_result(gen, "今日"), Some("今日".to_string()));
        assert_eq!(c.display(), "今日");
    }

    #[test]
    fn 要求済みでも結果未着ならrevealで再要求する() {
        let (mut c, log) = composer_with_log();
        c.push_hiragana("きょう");
        c.note_input_time(1000);
        c.tick(1300); // 投機要求(1回目)
        assert_eq!(log.borrow().len(), 1);
        assert_eq!(c.reveal(), RevealOutcome::Pending); // 結果未着 → 再要求
        assert_eq!(log.borrow().len(), 2);
    }

    #[test]
    fn reveal時に空バッファならempty() {
        let (mut c, _) = composer_with_log();
        assert_eq!(c.reveal(), RevealOutcome::Empty);
    }

    #[test]
    fn unrevealで生ひらがな表示に戻るがキャッシュは維持() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        c.on_llm_result(gen, "今日");
        c.reveal();
        c.unreveal();
        assert_eq!(c.display(), "きょう");
        assert_eq!(c.reveal(), RevealOutcome::Shown("今日".to_string()));
    }

    #[test]
    fn unreveal中に届いたfinalはキャッシュされ再revealで即表示() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        c.reveal();
        c.unreveal();
        let gen = c.current_generation();
        assert_eq!(c.on_llm_result(gen, "今日"), None); // 非表示でキャッシュ
        assert_eq!(c.reveal(), RevealOutcome::Shown("今日".to_string()));
    }

    #[test]
    fn 追記でreveal解除かつ世代が進む() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        c.on_llm_result(gen, "今日");
        c.reveal();
        c.push_hiragana("は");
        assert!(!c.is_revealed());
        assert_eq!(c.display(), "きょうは");
        // 旧世代の partial/final は破棄される
        assert_eq!(c.on_llm_result(gen, "今日"), None);
        assert_eq!(c.on_llm_partial(gen, "今日"), None);
    }

    #[test]
    fn commitはreveal済みなら変換結果_未revealなら生ひらがな() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        c.on_llm_result(gen, "今日");
        assert_eq!(c.commit(), "きょう"); // 未 reveal: 生ひらがな確定

        c.push_hiragana("きょう");
        let gen = c.current_generation();
        c.on_llm_result(gen, "今日");
        c.reveal();
        assert_eq!(c.commit(), "今日");
        assert!(!c.is_revealed()); // commit で revealed リセット
    }

    #[test]
    fn cancelでもrevealedがリセットされる() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        c.on_llm_result(gen, "今日");
        c.reveal();
        c.cancel();
        assert!(!c.is_revealed());
        assert_eq!(c.display(), "きょう");
        // cancel 後の旧世代 final は無視され、次の reveal は Pending から始まる
        assert_eq!(c.on_llm_result(gen, "今日"), None);
        assert_eq!(c.reveal(), RevealOutcome::Pending);
    }

    #[test]
    fn partialはreveal済みかつ現世代のみ表示_キャッシュしない() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょうは");
        let gen = c.current_generation();
        assert_eq!(c.on_llm_partial(gen, "今日"), None); // 未 reveal
        c.reveal();
        assert_eq!(c.on_llm_partial(gen, "今日"), Some("今日".to_string()));
        assert!(!c.is_converted()); // partial はキャッシュされない
        assert_eq!(c.on_llm_partial(gen + 1, "今日は"), None); // 旧世代
    }

    #[test]
    fn failureでrequested解除とロールバック文字列() {
        let (mut c, log) = composer_with_log();
        c.push_hiragana("きょう");
        c.reveal(); // Pending + 要求1
        let gen = c.current_generation();
        // reveal 済みで失敗 → 生ひらがなへのロールバック文字列が返る
        assert_eq!(c.on_llm_failure(gen), Some("きょう".to_string()));
        // requested_generation が解除されるので再 reveal で再要求できる
        assert_eq!(c.reveal(), RevealOutcome::Pending);
        assert_eq!(log.borrow().len(), 2);
    }

    #[test]
    fn 未revealのfailureはロールバック不要() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        c.note_input_time(1000);
        c.tick(1300);
        let gen = c.current_generation();
        assert_eq!(c.on_llm_failure(gen), None); // 表示は元々ひらがな
    }

    #[test]
    fn 旧世代のfailureは無視される() {
        let (mut c, log) = composer_with_log();
        c.push_hiragana("きょう");
        c.reveal();
        let old = c.current_generation();
        c.push_hiragana("は"); // 世代前進
        let before = log.borrow().len();
        assert_eq!(c.on_llm_failure(old), None);
        assert_eq!(log.borrow().len(), before); // 副作用なし
    }

    #[test]
    fn live設定時は従来どおり結果到着で即表示() {
        let (mut c, _) = composer_with_log();
        c.set_live(true);
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        assert_eq!(c.on_llm_result(gen, "今日"), Some("今日".to_string()));
        assert_eq!(c.display(), "今日");
    }
```

既存テストの期待値変更: `現世代のllm結果はmarked_textとして返る` / `変換済みをcommitすると変換結果が返る` / `同一表示の結果は差分抑制でnone` / `cancelで生ひらがなに戻る` / `is_convertedは変換結果保持中のみtrue` は冒頭に `c.set_live(true);` を追加して従来挙動(即表示)の検証として維持する。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test composer`
Expected: コンパイルエラー(`reveal` / `RevealOutcome` 未定義)

- [ ] **Step 3: 実装**

```rust
#[derive(Debug, PartialEq, Eq)]
pub enum RevealOutcome {
    Shown(String),
    Pending,
    Empty,
}
```

`Composer` に `revealed: bool` / `live: bool` を追加(`new` で false 初期化)。

```rust
    pub fn set_live(&mut self, live: bool) {
        self.live = live;
    }

    pub fn is_revealed(&self) -> bool {
        self.revealed || self.live
    }

    pub fn display(&self) -> &str {
        if self.is_revealed() {
            self.converted.as_deref().unwrap_or(&self.hiragana)
        } else {
            &self.hiragana
        }
    }

    pub fn display_hiragana(&self) -> &str {
        &self.hiragana
    }

    pub fn context(&self) -> &str {
        &self.context
    }

    pub fn reveal(&mut self) -> RevealOutcome {
        if self.hiragana.is_empty() {
            return RevealOutcome::Empty;
        }
        self.revealed = true;
        match &self.converted {
            Some(text) => RevealOutcome::Shown(text.clone()),
            None => {
                // 要求済みでも結果が無ければ再要求(取りこぼし・失敗からの回復)
                self.requested_generation = None;
                self.request_now();
                RevealOutcome::Pending
            }
        }
    }

    pub fn unreveal(&mut self) {
        self.revealed = false;
    }

    pub fn on_llm_result(&mut self, generation: u64, text: &str) -> Option<String> {
        if generation != self.generation {
            return None;
        }
        let changed = self.converted.as_deref() != Some(text);
        self.converted = Some(text.to_string());
        if self.is_revealed() && changed {
            Some(text.to_string())
        } else {
            None
        }
    }

    pub fn on_llm_partial(&self, generation: u64, text: &str) -> Option<String> {
        if generation != self.generation || !self.is_revealed() {
            return None;
        }
        Some(text.to_string())
    }

    pub fn on_llm_failure(&mut self, generation: u64) -> Option<String> {
        if generation != self.generation {
            return None;
        }
        self.requested_generation = None;
        self.requested_at_ms = None;
        if self.is_revealed() && self.converted.is_none() {
            Some(self.hiragana.clone())
        } else {
            None
        }
    }
```

`push_hiragana`/`backspace`: 「converted を土台に取り込む」ロジックを削除し、`self.revealed = false;` を追加した上で既存 invalidate(世代前進 + converted クリア)を通す。
`commit`: 既存処理の後に `self.revealed = false;` を追加。
`cancel`: `self.revealed = false;` を追加。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test composer`
Expected: 全 PASS

- [ ] **Step 5: Commit**

```bash
git add src/composer.rs
git commit -m "[:sparkles:]composer: reveal状態機械・投機キャッシュ・failure回復を追加"
```

---

### Task 2: Swift — セッション先行 prewarm プールと streamResponse 化

**Files:**
- Modify: `swift/LLMBridge.swift`
- Modify: `src/llm_bridge.rs`

**Interfaces:**
- Produces(Rust):
  - `pub const STATUS_PARTIAL: i32 = 3;`
  - `llm_bridge::prewarm_async(controller_id: u64, context: &str)` — 次回 respond 用セッションを先行生成し固定 prefix で prewarm(入力ごとではなく **composition 開始時/文脈変化時に1回** 呼ぶ)
- Produces(C ABI / Swift):
  - `novakey_llm_prewarm(controller_id: UInt64, ctx_ptr, ctx_len)`
  - `novakey_llm_convert` は respond 中の各 snapshot を `status=3` で deliver し、最後に final(0/1/2) を deliver する

**設計(レビュー指摘 HIGH-1 対応):** prewarm はセッション単位でしか効かないため、controller ごとに「次の respond で使うセッション」を1つだけプールする。prewarm prefix は入力に依存しない `instructions` + `"直前の文脈: <ctx>\n入力: "`(ctx 空なら `"入力: "`)までとする(入力全体を prefix にすると次の打鍵で prefix 不一致になり無意味なため)。convert はプール済みセッションがあればそれを取り出して respond し(1回で使い捨て)、なければその場で生成する。respond 開始直後に次回用セッションを補充 prewarm する。

- [ ] **Step 1: llm_bridge.rs に追加**

```rust
pub const STATUS_PARTIAL: i32 = 3;

extern "C" {
    // 既存 extern ブロックに追記
    fn novakey_llm_prewarm(controller_id: u64, ctx_ptr: *const u8, ctx_len: usize);
}

pub fn prewarm_async(controller_id: u64, context: &str) {
    unsafe { novakey_llm_prewarm(controller_id, context.as_ptr(), context.len()) };
}
```

`smoke_test` の handler を terminal status 対応に修正(LOW-10):

```rust
    set_result_handler(|_, _, status, text| {
        println!("status={} text={}", status, text);
        if status != STATUS_PARTIAL {
            std::process::exit(if status == STATUS_OK { 0 } else { 1 });
        }
    });
```

さらに `smoke_test` 冒頭の `convert_async` 呼び出し前に 15 秒のウォッチドッグを追加:

```rust
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(15));
        eprintln!("timeout: no terminal status within 15s");
        std::process::exit(2);
    });
```

- [ ] **Step 2: LLMBridge.swift — セッションプールと prewarm**

セッション管理を置き換え(メインスレッド専用):

```swift
private func promptPrefix(ctx: String) -> String {
    ctx.isEmpty ? "入力: " : "直前の文脈: \(ctx)\n入力: "
}

private func buildPrompt(input: String, ctx: String) -> String {
    promptPrefix(ctx: ctx) + "\(input) →"
}

// controller ごとの「次の respond 用」先行 prewarm 済みセッション。
// respond 1回で必ず取り出して使い捨てる(transcript を持ち越さない)。
private var pooledSessions: [UInt64: (session: LanguageModelSession, prefix: String)] = [:]

@MainActor
private func refillPool(_ controllerId: UInt64, ctx: String) {
    let prefix = promptPrefix(ctx: ctx)
    if pooledSessions[controllerId]?.prefix == prefix { return } // 温め直し不要
    let session = LanguageModelSession(instructions: instructions)
    session.prewarm(promptPrefix: Prompt(prefix))
    pooledSessions[controllerId] = (session, prefix)
}

@MainActor
private func takeSession(_ controllerId: UInt64, ctx: String) -> LanguageModelSession {
    let prefix = promptPrefix(ctx: ctx)
    if let pooled = pooledSessions.removeValue(forKey: controllerId), pooled.prefix == prefix {
        return pooled.session
    }
    return LanguageModelSession(instructions: instructions)
}

@_cdecl("novakey_llm_prewarm")
public func novakey_llm_prewarm(
    _ controllerId: UInt64,
    _ ctxPtr: UnsafePointer<UInt8>, _ ctxLen: Int
) {
    let ctx = String(decoding: UnsafeBufferPointer(start: ctxPtr, count: ctxLen), as: UTF8.self)
    DispatchQueue.main.async {
        refillPool(controllerId, ctx: ctx)
    }
}
```

注意: `prewarm(promptPrefix:)`(引数型 `Prompt?`)が SDK に無い場合は引数なし `prewarm()` にフォールバックし、コメントを残す。

- [ ] **Step 3: LLMBridge.swift — convert を streamResponse 化**

既存 `novakey_llm_convert` の main.async 内を変更(latest-wins は既存の `@MainActor` な latestGenerations を `await` で参照):

```swift
    DispatchQueue.main.async {
        registerLatest(controllerId, generation) // 既存 latest-wins 更新 + 旧 Task cancel
        let session = takeSession(controllerId, ctx: ctx)
        refillPool(controllerId, ctx: ctx) // 次回用を補充
        let prompt = buildPrompt(input: input, ctx: ctx)
        let task = Task {
            do {
                let stream = session.streamResponse(to: prompt)
                var latest = ""
                for try await snapshot in stream {
                    latest = snapshot.content // String.PartiallyGenerated == String
                    guard await isLatest(controllerId, generation) else { return }
                    deliver(controllerId, generation, 3, latest)
                }
                guard await isLatest(controllerId, generation) else { return }
                let output = latest.trimmingCharacters(in: .whitespacesAndNewlines)
                if !output.isEmpty, isPlausibleConversion(input: input, output: output) {
                    deliver(controllerId, generation, 0, output)
                } else {
                    deliver(controllerId, generation, 1, "") // 空出力・検証失敗は明示的に error
                }
            } catch let e as LanguageModelSession.GenerationError {
                if case .exceededContextWindowSize = e {
                    deliver(controllerId, generation, 2, "")
                } else {
                    deliver(controllerId, generation, 1, "\(e)")
                }
            } catch {
                deliver(controllerId, generation, 1, "\(error)")
            }
        }
        trackTask(controllerId, generation, task) // 既存の旧 Task cancel 機構に登録
    }
```

`registerLatest` / `isLatest` / `trackTask` は既存の latest-wins 実装の関数名に合わせて統合すること(存在しない場合は同等の `@MainActor` 状態として実装)。`isPlausibleConversion` の guard 節も「output の読みが空 → return false」に変更する(MEDIUM-7: fail-open は正規化不能時のみに限定)。

- [ ] **Step 4: ビルドとスモークテスト**

Run: `cargo build --release && ./target/release/novakey_r --test-llm`
Expected: status=3 の partial が複数回出た後、status=0 の final が出て exit 0

- [ ] **Step 5: Commit**

```bash
git add src/llm_bridge.rs swift/LLMBridge.swift
git commit -m "[:sparkles:]FFI: セッション先行prewarmとstreamResponse(partial配信)を追加"
```

---

### Task 3: imk.rs — 投機変換の配線(reveal・partial/failure 表示・prewarm)

**Files:**
- Modify: `src/imk.rs`

**Interfaces:**
- Consumes: Task 1 の `reveal()/unreveal()/on_llm_partial()/on_llm_failure()/set_live()/context()`, Task 2 の `prewarm_async` / `STATUS_PARTIAL`

- [ ] **Step 1: Space モードでも投機 tick を回す**

`schedule_tick_if_live` を `schedule_speculative_tick` にリネームし、モード条件を撤廃(両モードで裏変換。Space モードでは reveal まで表示されない)。呼び出し箇所(alphabetic/非 alphabet/backspace 分岐)もリネースに追従。

```rust
    fn schedule_speculative_tick(&self) {
        schedule_tick(self.ivars().controller_id);
    }
```

- [ ] **Step 2: prewarm は composition 開始時と確定時に発行(MEDIUM-8 対応)**

打鍵ごとではなく、prefix(文脈)が変わるタイミングだけ:

1. `input_text` alphabetic 分岐で、`composer.push_hiragana(&kana)` の直前に「バッファが空だったか」を記録し、空→非空になった打鍵(composition 開始)でのみ:

```rust
                let starting = composer.is_empty();
                if !kana.is_empty() {
                    composer.push_hiragana(&kana);
                }
                if starting && !composer.is_empty() && llm_enabled() {
                    llm_bridge::prewarm_async(self.ivars().controller_id, composer.context());
                }
```

2. Enter 確定処理(`insertNewline:`)の `composer.set_context(&text)` の直後に(次 composition 用の温め直し):

```rust
                if llm_enabled() {
                    llm_bridge::prewarm_async(self.ivars().controller_id, composer.context());
                }
```

- [ ] **Step 3: スペースキーを reveal ベースに書き換え**

`input_text` のスペース分岐の match を置き換え:

```rust
                match *self.ivars().mode.borrow() {
                    ConversionMode::Space => {
                        if composer.is_revealed() {
                            composer.unreveal();
                        } else if llm_enabled() {
                            if composer.reveal() == crate::composer::RevealOutcome::Empty {
                                return Bool::NO;
                            }
                        }
                        let marked = composer.display().to_string();
                        unsafe { set_marked_str(client, &marked) };
                    }
                    ConversionMode::Live => {
                        if llm_enabled() {
                            composer.request_now();
                        }
                        let marked = composer.display().to_string();
                        unsafe { set_marked_str(client, &marked) };
                    }
                }
```

Pending の場合は生ひらがな表示のまま、直後に届く partial/final が marked text を更新する。

- [ ] **Step 4: handle_llm_result で partial / failure を処理(HIGH-2, HIGH-3 対応)**

```rust
    fn handle_llm_result(&self, generation: u64, status: i32, text: &str) {
        let client = *self.ivars().last_client.borrow();
        if client.is_null() {
            return;
        }
        let romaji_pending = !self.ivars().converter.borrow().buffer().is_empty();
        let mut composer = self.ivars().composer.borrow_mut();
        let marked = match status {
            llm_bridge::STATUS_PARTIAL => {
                if romaji_pending {
                    None // partial は打鍵途中なら破棄(使い捨て)
                } else {
                    composer.on_llm_partial(generation, text)
                }
            }
            llm_bridge::STATUS_OK => {
                // final は打鍵途中でも必ずキャッシュし、UI 更新だけ抑止する
                let marked = composer.on_llm_result(generation, text);
                if romaji_pending {
                    None
                } else {
                    marked
                }
            }
            _ => {
                unsafe { NSLog!("LLM error status=%d", status) };
                let rollback = composer.on_llm_failure(generation);
                if romaji_pending {
                    None
                } else {
                    rollback // partial 表示済みなら生ひらがなへ戻す
                }
            }
        };
        if let Some(marked) = marked {
            unsafe { set_marked_str(client, &marked) };
        }
    }
```

- [ ] **Step 5: ASCII 入力での reveal 解除(HIGH-2 対応)**

alphabetic 分岐の先頭(`converter.process_input` ループの前)に追加 — かなが生成されない打鍵(子音1文字目)でも reveal 済み表示を維持しない:

```rust
                {
                    let mut composer = self.ivars().composer.borrow_mut();
                    if composer.is_revealed() {
                        composer.unreveal();
                    }
                }
```

注: このあと既存フローで `composer.display() + romaji` を marked 表示するため、表示は「生ひらがな + 打鍵中ローマ字」に自然に戻る。

- [ ] **Step 6: モード切替時の同期と再描画(MEDIUM-6 対応)**

`toggle_conversion_mode` を置き換え:

```rust
        #[method(toggleConversionMode:)]
        fn toggle_conversion_mode(&self, _sender: *mut AnyObject) {
            let new_mode = {
                let mut mode = self.ivars().mode.borrow_mut();
                *mode = match *mode {
                    ConversionMode::Live => ConversionMode::Space,
                    ConversionMode::Space => ConversionMode::Live,
                };
                *mode
            };
            let mut composer = self.ivars().composer.borrow_mut();
            composer.set_live(new_mode == ConversionMode::Live);
            if new_mode == ConversionMode::Space {
                composer.unreveal(); // Space 移行時はひらがな表示から
            }
            // 未確定があれば表示を現状態に合わせて再描画
            let client = *self.ivars().last_client.borrow();
            if !client.is_null() && !composer.is_empty() {
                let marked = composer.display().to_string();
                unsafe { set_marked_str(client, &marked) };
            }
        }
```

- [ ] **Step 7: ビルド+テスト+実機確認**

Run: `cargo test && mise run install`
Expected: 全テスト PASS。実機シナリオ:
- 「kyouhaiitenki」→ 入力中ひらがなのまま → スペースで即時 or ストリーミング表示 → スペースでひらがな復帰 → Enter 確定
- Pending 中に h → Backspace → スペース: 再要求されて変換が出る(ハングしない)
- 変換失敗時(長文で検証ゲートに落とすなど): ひらがなに戻る

- [ ] **Step 8: Commit**

```bash
git add src/imk.rs
git commit -m "[:sparkles:]投機変換: reveal・partial/failure表示・prewarmを配線"
```

---

### Task 4: チューニングとドキュメント

**Files:**
- Modify: `src/composer.rs`(DEBOUNCE_MS)
- Modify: `CLAUDE.md`

- [ ] **Step 1: デバウンスを投機向けに短縮**

`DEBOUNCE_MS` を 300 → 150 に変更(表示に出ないため短くしてキャッシュヒット率を上げる)。DEBOUNCE_MS 依存のテスト時刻はリテラルをやめて `1000 + DEBOUNCE_MS - 100` / `1000 + DEBOUNCE_MS` のように定数参照に書き換える。

Run: `cargo test composer`
Expected: PASS

- [ ] **Step 2: CLAUDE.md 更新**

Architecture 節の変換フロー記述を更新:

```markdown
- 変換フロー: 入力中は生ひらがな表示のまま裏で投機変換(セッション先行prewarm + キャッシュ)。
  スペースで開示(キャッシュヒットなら即時、ミスなら streamResponse で流し込み)。
  もう一度スペースでひらがなに戻る。Enter で確定。変換失敗時はひらがな表示へ自動復帰
```

- [ ] **Step 3: 全体確認とコミット**

Run: `cargo test && mise run install` → 実機で両モード・連続入力・モード切替(未確定中の切替含む)・フォーカス切替を手動確認

```bash
git add src/composer.rs CLAUDE.md
git commit -m "[:sparkles:]投機変換のチューニングとドキュメント更新"
```
