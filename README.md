# [WIP]NovakeyR

NovakeyR は、LLMと単語辞書をかけ合わせた macOS 向け IME です。すべて Rust で書かれており、Swift ランタイムには依存しません。

## 動作例

comming soon.

## 動作環境

macOS　26

## 使い方

### 1. ビルド ・ パッケージの出力

```sh
$ mise run app
```

`output/NovakeyR.app` が出力されます

### 2. Cloudflare 認証情報の登録

候補のリランキングに Cloudflare Workers AI (`@cf/cloudflare/clef-flash`) を使います。
API トークン(Workers AI の権限が必要)とアカウント ID を macOS の Keychain に登録してください。
`-w` の後ろに値を書かないと、対話プロンプトで入力できます(コマンド履歴に残りません)。

```sh
$ security add-generic-password -U -s me.sijis.inputmethod.NovakeyR.cloudflare -a api-token -w
$ security add-generic-password -U -s me.sijis.inputmethod.NovakeyR.cloudflare -a account-id -w
```

未登録の場合、リランキングは無効になり辞書変換のみで動作します。登録後は IME の再起動が必要です。

#### Cloudflare に送られる情報とガード

リランキングのたびに、変換候補・読み(ひらがな)・直前の文(最大100文字)を Cloudflare Workers AI に送信します。
次の場合は送信を制限します(`src/outbound.rs`)。

1. セキュア入力中(パスワード欄など)は、リランキング自体を行いません。
2. パスワードマネージャ・ターミナル系アプリ(1Password、Bitwarden、キーチェーンアクセス、Terminal、iTerm2、Warp、Ghostty、kitty、Alacritty 等)では、リランキング自体を行いません。
3. 直前の文に API キー、秘密鍵、JWT、メールアドレス、長い数字列、長いトークン風の文字列、「パスワード」などの語が含まれる場合は、直前の文を送らず候補と読みだけで行います。

#### 学習について

確定した候補は端末内の `~/Library/Application Support/NovakeyR/rust-learning/learning.tsv` に記録され、次の変換に反映されます(外部には送りません)。
セキュア入力中、およびパスワードマネージャ(1Password、Bitwarden、キーチェーンアクセス、パスワード)では学習しません。

### 3. Install

NovakeyR.app を `/Library/Input Methods` 内にコピーします。

```sh
sudo cp -r output/NovakeyR.app "/Library/Input Methods"
```

再起動（または、ログアウト→再ログイン）することで有効になります。

環境設定 > キーボード > 入力ソース の英語から`NovakeyR`を追加してください。

日本語モードについては開発中です。

## クレジット

カタカナ語の英単語候補(`resources/loanwords.tsv`)は、[EDRDG](https://www.edrdg.org/) の JMdict から `tools/gen_loanwords.py` で生成しています。JMdict は [CC BY-SA 4.0](https://www.edrdg.org/edrdg/licence.html) で提供されています。

絵文字の候補(`resources/emoji.tsv`)は、Unicode, Inc. の絵文字データと CLDR の日本語注釈([Unicode License v3](https://www.unicode.org/license.txt))を、[azooKey_emoji_dictionary_storage](https://github.com/ensan-hcl/azooKey_emoji_dictionary_storage)(commit 08f82252fb90ef8f0949a7e3c554e9e1787ce121)経由で取り込み、`tools/gen_emoji.py` で生成しています。

ローマ字→ひらがなの変換表(`src/romaji_table.tsv`)は、[Mozc](https://github.com/google/mozc) の `romanji-hiragana.tsv`(BSD 3-Clause, Copyright Google Inc.)を元にしています。

かな漢字変換(`src/kkc/`)は、[AzooKeyKanaKanjiConverter](https://github.com/azooKey/AzooKeyKanaKanjiConverter)(MIT License, Copyright (c) 2023 Miwa / Ensan)を Rust に移植したものです。ライセンス全文は `NOTICE` を参照してください。

変換辞書(`dictionary/`)は、[azooKey_dictionary_storage](https://github.com/ensan-hcl/azooKey_dictionary_storage)(Apache License 2.0)のデータを、予測用の `p/` を除いてそのまま収録しています。詳細は `dictionary/NOTICE` を参照してください。
