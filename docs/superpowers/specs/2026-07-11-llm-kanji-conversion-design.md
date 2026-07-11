# NovakeyR: LLMによるかな漢字変換 + 自動句読点挿入 設計

日付: 2026-07-11
ステータス: レビュー反映済み (Sonnet レビュー + web調査)

## 目的

現状の NovakeyR はローマ字→ひらがな変換まで。これを拡張し:

1. ひらがな列を文脈に応じた自然な日本語表記(漢字・カタカナ・英字)に変換する
2. 句読点(。、)を自動挿入する。文脈がつながって不要になった句読点は自動で消える
3. すべてローカル(オンデバイス)で完結させる

## 前提・環境

- macOS 26.2 / `FoundationModels.framework`(Apple Intelligence オンデバイスLLM)が利用可能
  - 日本語対応済み。レイテンシは第一トークン ~0.6ms/プロンプトトークン・生成 ~30 tok/s、`prewarm()` で初回 ~150ms 程度まで短縮可能
  - **コンテキストウィンドウは 4096 トークン固定**(超過で `.exceededContextWindowSize`)
- 既存構成: `src/main.rs`(エントリ)、`src/imk.rs`(IMK統合)、`src/romaji_converter.rs`(ローマ字→ひらがな、純Rust・テスト済み)
- FoundationModels は Swift 専用 API のため Swift shim が必要。Rust 統合の先行事例あり(fm-bindings, rusty_foundationmodels — いずれも build.rs で swiftc を呼び C-ABI ライブラリをリンクする方式)
- 先行事例: azooKey-Desktop の Zenzai(90M GPT-2 によるニューラルかな漢字変換、ローカル動作)が LLM かな漢字変換の実績。同音異義語・長文で辞書式を大きく上回る品質

## 全体アーキテクチャ

```
キー入力
  → imk.rs (IMKInputController, メインスレッド)
  → romaji_converter.rs (ローマ字→ひらがな、既存)
  → composer.rs (新規: 文バッファ・変換セッション管理、純Rust)
  → llm_bridge.rs (新規: Rust↔Swift FFI)
  → swift/LLMBridge.swift (新規: FoundationModels 呼び出し)
```

## スレッド/ライフタイム契約(最重要)

IMK の API はすべてメインスレッド(MainActor)前提であり、controller の ivar(`RefCell`)は `!Sync`。よって:

1. **LLM 呼び出しは Swift 側で非同期実行**し、**結果は必ず Swift 側で `DispatchQueue.main.async` にホップしてから** Rust コールバックを呼ぶ。Rust コールバックは常にメインスレッドで発火する契約とする
2. **コールバックは `client` ポインタを保持しない**。コールバックが運ぶのは `(controller_id, generation, text)` のみ。controller はグローバルレジストリ(`main` スレッド専用の `HashMap<ControllerId, Weak<...>>` 相当)経由で引き直し、解放済みなら結果を破棄する(use-after-free 防止)
3. **世代カウンタ**: 変換要求ごとに世代番号を発行。到着した結果の世代が現世代より古ければ破棄
4. **確定・キャンセル・フォーカス喪失(`commitComposition:`)時は現世代を進めて全 in-flight 要求を無効化**する。遅延到着した旧結果が次の文に混入しない

## コンポーネント

### 1. swift/LLMBridge.swift(新規)

- `LanguageModelSession` は **controller ごとに1つ**保持(複数アプリ同時入力で文脈が混ざるのを防ぐ)。起動時に `prewarm()`
- availability は `SystemLanguageModel.default.availability` の case 別に判定し、理由コードを Rust に返す(`available` / `deviceNotEligible` / `appleIntelligenceNotEnabled` / `modelNotReady`)
- `@_cdecl` エクスポート(C型のみ: UTF-8 バイトポインタ+長さ):
  - `novakey_llm_convert(controller_id, generation, input_ptr, input_len, context_ptr, context_len)` — 非同期。完了時に main queue 上で Rust の登録済みコールバック `novakey_llm_on_result(controller_id, generation, status, text_ptr, text_len)` を呼ぶ
  - `novakey_llm_availability() -> i32`
- `respond(to:)` の throws はステータスコードにマップしてコールバックへ。Swift 側の fatalError はプロセスごと落ちるため、throw されるエラーのみ扱える点をログ方針に明記
- ビルド: `build.rs` から `swiftc -emit-library -static`(先行事例 fm-bindings 方式)。mise の `build`/`app`/`install` タスクはそのまま `cargo build` 経由で動く

プロンプト方針:

> 以下のひらがな読み列を、自然な日本語表記(漢字・カタカナ・英字を適切に使用)に変換し、適切な句読点を補って出力せよ。変換結果のみを返す。

- few-shot 例を数個添付、temperature ~0
- **毎回、未確定文全体を再生成**する。文脈が変わって不要になった句読点は次回の再生成で自然に消える
- **コンテキスト上限**: 4096 トークン制約に収めるため、プロンプトに含める確定済み文脈は直近 ~200 文字までに制限。未確定部分がそれ自体で長すぎる場合(~500 文字超)はライブ変換を停止しひらがな表示のまま(確定時に一括変換を試みる)

### 2. composer.rs(新規・純Rust)

- 未確定文全体のひらがなバッファと直近の変換結果を保持
- **デバウンス**: ライブ変換モードでは入力停止 約300ms でLLM変換をトリガー
- **世代管理**: 上記契約の発行・照合・無効化を担う
- **差分抑制**: 新しい変換結果が現在表示中の marked text と同一なら `setMarkedText` を呼ばない(ちらつき防止)
- LLM は trait(`KanjiConverter`)として抽象化し、テストではモック実装を注入

### 3. imk.rs 拡張 — 2つの変換モード

**ライブ変換モード(デフォルト)**
- 未確定文全体を marked text として保持
- LLM結果到着時(メインスレッド)に差分があれば `setMarkedText:` で差し替え。選択範囲は常に末尾に置く
- Enter で確定(`insertText:`)+全 in-flight 無効化、Esc で生ひらがなに戻す

**スペース変換モード(従来型)**
- ひらがなのまま marked text に入力 → スペースでLLM変換 → marked text を置換
- もう一度スペースで生ひらがなにトグル、Enter で確定
- **変換後にさらに文字を入力した場合**: 変換結果を生ひらがなに巻き戻さず、変換結果+新規ひらがなを連結した未確定文として扱い、次回スペースで全体を再変換

**編集キーの仕様**
- バックスペース: 未確定中は「ひらがなバッファの末尾1文字削除」(表示は生ひらがな相当に部分的に戻る。次のデバウンスで再変換)
- 左右矢印キー: 未確定中は**無効**(v1 ではカーソル移動をサポートしない。全文再生成モデルと矛盾するため)。未確定文がなければアプリへパススルー
- `commitComposition:`(フォーカス喪失): 現在の表示内容をそのまま確定し、in-flight を無効化

- モード切替: IMK の入力ソースメニューに項目を追加
- フォールバック: availability が `available` 以外なら、ひらがなのまま確定する現行動作に落とす(メニューに状態表示)

### 4. 既存機能との関係

- `playful_convert()`(1/0/space のお遊び変換)は確定時処理として維持
- `romaji_converter.rs` は変更なし

## エラーハンドリング

- LLM 失敗/タイムアウト(3s)/コンテキスト超過: その世代を破棄しひらがな表示を維持。確定時に未変換ならひらがなのまま確定
- Swift の thrown error はステータスコードで Rust に返し、NSLog に出力(握りつぶさない)

## テスト戦略

- `composer.rs`: モック `KanjiConverter` で、デバウンス・世代破棄・確定時無効化・差分抑制・フォールバックを `cargo test`
- `romaji_converter.rs`: 既存テスト維持
- Swift shim: `mise run test-llm`(実機で availability 確認+1回変換のスモークテスト)
- 手動: `mise run install` → 実アプリで両モード・フォーカス切替・複数アプリ同時入力を確認

## リスク

| リスク | 対策 |
|--------|------|
| LLMレイテンシ | prewarm + デバウンス + 世代管理。実測 ~30 tok/s なら短文は体感数百ms |
| use-after-free(client) | コールバックは id のみ運び、メインスレッドのレジストリで引き直す |
| 旧結果の混入 | 確定/キャンセル/フォーカス喪失で全 in-flight 無効化 |
| 読みと異なる漢字 | temperature 0・few-shot・Esc で生ひらがな復帰 |
| 4096トークン上限 | 文脈 ~200 文字・未確定 ~500 文字で打ち切り |
| marked text ちらつき | 差分がなければ setMarkedText を呼ばない |
| FoundationModels 不可環境 | ひらがな確定へフォールバック+メニューに理由表示 |
