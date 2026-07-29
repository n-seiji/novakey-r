# 辞書エンジン移行 + LLM リランキング 設計

## 背景と決定

LLM(FoundationModels)単独のかな漢字変換は、レイテンシ(〜1秒)・候補数(1個)・読み保存の非保証という構造的限界が実験で確認された。ユーザー提案のパイプライン「単語に区切る→変換予測→LLMでスコアリング」を、以下の形で実現する:

```
読み(ひらがな)
  → [AzooKeyKanaKanjiConverter]  辞書ラティス+Viterbi。区切りと n-best 候補を同時に解く(数ms・同期)
  → [LLM リランキング]            n-best から文脈に合う候補番号を選ばせる(非同期・失敗しても無害)
```

- 区切り(分かち書き)は単独工程にしない。読みだけでは区切りが一意に決まらないため(「きょうはいしゃ」問題)、辞書ラティス探索が区切りと候補を同時に解く
- LLM は生成せず**選択のみ**(出力1トークン)。表示をブロックしない
- 既存の FoundationModels ブリッジは維持し、役割をリランキング(将来: 句読点添削)に変更

## エンジン: AzooKeyKanaKanjiConverter v0.11.2 (実機検証済み)

- `.upToNextMinor(from: "0.11.2")` 固定(v1.0 未満はマイナーで破壊的変更あり)
- `KanaKanjiConverterModuleWithDefaultDictionary` を使用。辞書は SPM リソースバンドルとして同梱される
- ラッパー SPM パッケージ(`swift/AzooKeyBridge`)を `type: .static` で作ると全依存込みの単一 `libAzooKeyBridge.a`(約28MB)が得られ、Rust から静的リンクできる(検証済み)
- リソースバンドル `AzooKeyKanaKanjiConverter_KanaKanjiConverterModuleWithDefaultDictionary.bundle`(38MB)を .app に同梱する必要あり
- 学習(ユーザー履歴): `learningType: .inputAndOutput` + `updateLearningData(candidate)` / `commitUpdateLearningData()`
- ライセンス: 本体 MIT / 辞書 Apache-2.0
- Zenzai(ニューラル検証)は同パッケージの trait で後から有効化可能(本設計のスコープ外)

## UX

- **ライブ変換がデフォルト**: 打鍵ごとに同期変換(数ms)し、marked text に第1候補 + 未変換ローマ字を表示
- **スペース**: 候補送り(1→2→…→wrap)。IMKCandidates の候補ウィンドウを表示
- **Enter**: 表示中の候補を確定 + 学習更新
- **Esc**: 生ひらがな表示に戻す(もう一度 Esc でアプリへパススルー)
- **Backspace**: ローマ字バッファ優先、次にひらがな末尾1文字(従来どおり)
- メニュー: ライブ変換オン/オフ(オフ時はスペースまでひらがな表示)
- LLM リランキング: 変換のたびに非同期で候補番号を選ばせ、**ユーザーがまだ候補送りしていなければ**第1候補を差し替える

## 廃止するもの

- composer の非同期 LLM 変換機構(reveal/投機キャッシュ/デバウンス/タイムアウト/prewarm/streamResponse partial 表示)
- LLMBridge.swift の変換用プロンプト(リランキング用プロンプトに置換)
- 変換用の検証ゲート(辞書エンジンは読み保存が保証されるため不要)

## リスクと対策

- ビルド時間/サイズ増(初回 resolve で llama.cpp xcframework ~600MB DL、.a 28MB + bundle 38MB): 開発機のみの問題。CI なし
- `KanaKanjiConverter` はメインスレッド専用として扱う(IMK イベントは全てメインスレッドなので同期呼び出しで整合)
- 候補ウィンドウ(IMKCandidates)の挙動が不安定な場合: v1 は表示+クリック選択のみの informational とし、キー操作は自前の候補送りで完結させる
