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

近日公開: 最初の署名付きリリースを準備中です。

公開後は次のどちらかで入れます。

- Homebrew: `brew install --cask n-seiji/tap/novakey-r`
- または Releases の zip をブラウザでダウンロードし、Finder で展開して `NovakeyR.app` を `~/Library/Input Methods` に移します。

そのあと、システム設定 > キーボード > 入力ソース で NovakeyR を追加してください。先にログインし直す必要がある場合があります。

### アンインストール

先に入力ソースから NovakeyR を外し、そのあと `brew uninstall --zap --cask novakey-r` を実行します。

### ソースからビルド

[mise](https://mise.jdx.dev/) と Rust が必要です。並べ替えにはモデルファイルが必要で、リポジトリには入っていません。先に 1 度だけ作ります (Hugging Face から約 41 MB をダウンロード。[uv](https://docs.astral.sh/uv/) は mise が入れます)。作らなくても動き、そのときは変換エンジン本来の順です。

```sh
mise run judge-model
mise run install
```

`~/Library/Input Methods/NovakeyR.app` に入ります。ログインし直してから、システム設定 > キーボード > 入力ソース で NovakeyR を追加してください。

## 実行時にネットワークは使いません

NovakeyR はネットワーク接続を開きません。ソースと、リンクしているクレートの一覧 `Cargo.lock` を読めば、自分で確かめられます。

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
