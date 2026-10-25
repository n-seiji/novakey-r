<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/wordmark-dark.svg">
  <img src="docs/assets/wordmark-light.svg" alt="NovakeyR" width="320">
</picture>

[English](README.md) · **日本語**

**英語も日本語も、モードを切り替えずに打てる macOS の日本語入力。**

かな漢字変換はすべて Rust で書かれ、手元の Mac の中だけで動きます。打った文字は外に送りません。

<table>
  <tr>
    <td align="center" valign="top" width="33%">
      <img src="docs/assets/tile-convert.svg" width="200" alt="あしたははれです が Space で 明日は晴れです になり、Enter で確定する"><br>
      <b>変換</b><br>
      <sub>Space で変換、Enter で確定。</sub>
    </td>
    <td align="center" valign="top" width="33%">
      <img src="docs/assets/tile-english.svg" width="200" alt="ぎ が git になり、Space を挟んで git merge と打てる"><br>
      <b>英語はそのまま</b><br>
      <sub>英単語は英字で。Space は空白。</sub>
    </td>
    <td align="center" valign="top" width="33%">
      <img src="docs/assets/tile-mixed.svg" width="200" alt="artifact のあとの wo が を になり、artifactを使う に変換される"><br>
      <b>英語まじり</b><br>
      <sub>artifactを使う も、続けて打てる。</sub>
    </td>
  </tr>
</table>

- **変換。** Space で変換し、もう一度押すと次の候補です。数字キーで選び、Enter で表示しているとおりに確定します。ローマ字の表は Google 日本語入力 (Mozc) と同じです。
- **英語。** かなにならない英単語は英字のまま表示します (enter, merge)。英語の途中の Space は空白として入ります。一度英語で確定した語は、次から打った時点で英字になります (git)。
- **候補。** カタカナ語の英単語 (デスクトップ → desktop) と絵文字 (ねこ → 🐈) も候補に出ます。
- **学習。** 選んだ候補を覚えて、次から先に出します。パスワード欄とパスワード管理アプリでは覚えません。
- **並べ替え。** 小さな日本語言語モデル (rinna japanese-gpt2-xsmall) を Mac の中で動かし、文全体の候補を自然な順に並べ替えます。通信はしません。メモリは約 140 MB 増え (長い入力を headless で流し `/usr/bin/time -l` で測った最大値が 41 MB から約 181 MB)、1 回の判定は中央値で約 8 ms です。モデルファイルが必要です (下記)。無いときは変換エンジン本来の順のままです。
- **ライブ変換 (任意)。** 打ちながら変換結果を表示します。入力メニューから切り替えます。

## インストール

動作環境: macOS 26 以降、Apple Silicon (M1 以降)。Intel Mac には対応していません。

次のどちらかで入れます。

- Homebrew: 最初に一度 `brew trust n-seiji/tap` を実行します (Homebrew は信頼した公式以外の tap の cask しか読み込みません)。そのあと `brew install --cask n-seiji/tap/novakey-r` を実行します
- または Releases の zip をブラウザでダウンロードし、Finder で展開して `NovakeyR.app` を `~/Library/Input Methods` に移します。

そのあと、システム設定 > キーボード > 入力ソース で NovakeyR を追加してください。先にログインし直す必要がある場合があります。

`brew upgrade --cask novakey-r` で更新すると、macOS が入力ソースから NovakeyR を外すので、もう一度追加してください。Homebrew はアプリを丸ごと置き直しますが、中身だけの入れ替えは macOS の App 管理の保護で止められるためです。

### アンインストール

先に入力ソースから NovakeyR を外します。そのあと、Homebrew で入れた場合は `brew uninstall --zap --cask novakey-r` を実行します。zip から入れた場合は `~/Library/Input Methods/NovakeyR.app` を削除します。

### ソースからビルド

[mise](https://mise.jdx.dev/) と Rust が必要です。並べ替えにはモデルファイルが必要で、リポジトリには入っていません。先に 1 度だけ作ります (Hugging Face から約 41 MB をダウンロード。[uv](https://docs.astral.sh/uv/) は mise が入れます)。作らなくても動き、そのときは変換エンジン本来の順です。

```sh
mise run judge-model
mise run install
```

`~/Library/Input Methods/NovakeyR.app` に入ります。ログインし直してから、システム設定 > キーボード > 入力ソース で NovakeyR を追加してください。

### 依存パッケージ

公開から 7 日未満のバージョンは使いません。`mise run release-age` (`scripts/check-release-age.sh`) は、`Cargo.lock` に crates.io の 7 日未満のバージョンがあると失敗します (`MIN_AGE_DAYS` で変更可)。CI でも push と pull request ごとに実行します。審判モデルのスクリプトは uv の `exclude-newer = "7 days"`、`mise.toml` は `minimum_release_age = "7d"` を設定しています。緊急で例外が要るときは `tools/release-age-allow.txt` に `name version # 理由` の行を足し、期間が過ぎたら消してください。

## 実行時にネットワークは使いません

NovakeyR はネットワーク接続を開きません。`scripts/check-no-network.sh` がソースとビルドしたバイナリのネットワーク利用を調べ、CI が push と pull request のたびに実行します。これは実行時の保証です。審判モデルはビルド時に 1 度だけ取得します。ソースと、リンクしているクレートの一覧 `Cargo.lock` を読んで自分で確かめることもできます。

## キー

| キー | 動作 |
|---|---|
| Space | 変換 / 次の候補。英語の途中では空白 |
| Shift+Space | 前の候補。英語の途中では変換 |
| ↑ ↓ | 候補を移動 |
| 1〜9 | 候補を選んで確定 |
| Enter | 表示しているとおりに確定 |
| Esc / Backspace | 変換を取り消す |
| Ctrl+J / K / L / ; / : | ひらがな / カタカナ / 全角英数 / 半角カナ / 半角英数 |
| F6〜F10 | 同上 |

## データの置き場所

学習は `~/Library/Application Support/NovakeyR/rust-learning/` に保存されます (`learning.tsv` と `english.txt`)。消すときは `mise run reset-memory` です。キー入力のトレースは初期状態でオフで、打った文字は記録しません。

## リリース手順

メンテナ向けです。リリースは `.github/workflows/release.yml` がビルド・署名・公証まで行います。`release` 環境を承認するまでは何も公開されません。

1. 初回のみ、次の順で行います (順序が大事です。環境と承認者を作る前に変数を設定すると、誰の承認もなくリリースが公開されてしまいます)。
   1. `scripts/setup-release-env.sh` を実行します。`signing` 環境 (シークレットを置く、main のみ) と `release` 環境 (承認者 1 人、main のみ) の両方を作ります。
   2. 5 つのシークレットを `signing` 環境に登録します (標準入力を使わないものは `gh` が値を尋ねます)。
      ```sh
      base64 -i cert.p12 | gh secret set DEVELOPER_ID_P12_BASE64 --env signing
      gh secret set DEVELOPER_ID_P12_PASSWORD --env signing
      gh secret set ASC_KEY_ID --env signing
      gh secret set ASC_ISSUER_ID --env signing
      base64 -i AuthKey_XXXXXXXXXX.p8 | gh secret set ASC_KEY_P8_BASE64 --env signing
      ```
   3. リリースを有効にするリポジトリ変数を設定します: `gh variable set RELEASE_ENABLED --body true`。`true` になるまでワークフローは「release disabled」と報告するだけで何もビルドしないので、main へのマージは安全です。

   シークレットは `signing` 環境に紐づき、この環境を使えるのは main で動くジョブだけなので、他のブランチのワークフローからは読めません。公証には、このリポジトリ専用の App Store Connect API キー (Developer ロール) を使ってください。
2. `Cargo.toml` の `version` を上げて main にマージします。ワークフローがアプリをビルドして署名し、公証と staple を行い、`NovakeyR-<version>.app.zip` とその `.sha256` を載せた下書きリリースを作ります。
3. 承認する前に下書きを試します。
   - 下書きの zip を **Safari で** ダウンロードします (quarantine 属性を付けるため)。Finder で展開し、`NovakeyR.app` を `~/Library/Input Methods` に移し、ログインし直して入力ソースを追加し、実際に打ちます。
   - Homebrew でも同じことをします。下書きの URL は公開されていないので、`packaging/homebrew/novakey-r.rb` をコピーして `url "file:///<ダウンロードした zip のパス>"` とその zip の sha256 に書き換え、ローカルの tap (`brew tap-new --no-git <you>/local` の `Casks/`) に置きます。Homebrew は tap にある cask しか入れないためです。`brew install --cask <you>/local/novakey-r` で入れ、`xattr -p com.apple.quarantine ~/Library/Input\ Methods/NovakeyR.app` の結果を確かめます。
   - Gatekeeper が IME を止めるなら、承認しません。
4. `release` 環境を承認します。publish ジョブが下書きをビルド時のチェックサムと照合し、zip の attestation を作ってリリースを公開します。
5. `scripts/bump-cask.sh <version> <tap>/Casks/novakey-r.rb` を実行し (チェックサムと attestation を検証します)、tap を push します。

## クレジット

- かな漢字変換 (`src/kkc/`) は [AzooKeyKanaKanjiConverter](https://github.com/azooKey/AzooKeyKanaKanjiConverter) (MIT, © 2023 Miwa / Ensan) を Rust に移植したものです。
- 変換辞書 (`dictionary/`) は [azooKey_dictionary_storage](https://github.com/ensan-hcl/azooKey_dictionary_storage) (Apache-2.0) のデータです。
- ローマ字の表 (`src/romaji_table.tsv`) は [Mozc](https://github.com/google/mozc) の `romanji-hiragana.tsv` (BSD-3-Clause, © Google Inc.) です。
- カタカナ語の英単語 (`resources/loanwords.tsv`) は [EDRDG](https://www.edrdg.org/) の JMdict (CC BY-SA 4.0) から作っています。
- 絵文字 (`resources/emoji.tsv`) は Unicode の絵文字データと CLDR の日本語注釈 (Unicode License v3) から、[azooKey_emoji_dictionary_storage](https://github.com/ensan-hcl/azooKey_emoji_dictionary_storage) 経由で作っています。
- 並べ替えは [rinna/japanese-gpt2-xsmall](https://huggingface.co/rinna/japanese-gpt2-xsmall) (MIT, © rinna Co., Ltd.) を使います。`tools/gen_judge_model.py` で変換して使い、このリポジトリには含みません。
- ロゴの文字は [Newsreader](https://github.com/productiontype/Newsreader) (SIL Open Font License 1.1) をアウトライン化したものです。

ライセンス全文は [`NOTICE`](NOTICE) と [`dictionary/NOTICE`](dictionary/NOTICE) にあります。

## ライセンス

GPL-3.0-or-later です。[`LICENSE`](LICENSE) を参照してください。サードパーティの表示は [`NOTICE`](NOTICE)、Rust クレートのライセンスは [`THIRD_PARTY_LICENSES`](THIRD_PARTY_LICENSES) にあります。
