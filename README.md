# [WIP]NovakeyR

NovakeyR は、LLMと単語辞書をかけ合わせた macOS 向け IME です。

## 動作例

comming soon.

## 動作環境

macOS　26

## 使い方

### 1. ビルド ・ パッケージの出力

```sh
$ make app
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

### 3. Install

NovakeyR.app を `/Library/Input Methods` 内にコピーします。

```sh
sudo cp -r output/NovakeyR.app "/Library/Input Methods"
```

再起動（または、ログアウト→再ログイン）することで有効になります。

環境設定 > キーボード > 入力ソース の英語から`NovakeyR`を追加してください。

日本語モードについては開発中です。
