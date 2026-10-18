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

#### Cloudflare に送られる情報とガード

リランキングのたびに、変換候補・読み(ひらがな)・直前の文(最大100文字)を Cloudflare Workers AI に送信します。
次の場合は送信を制限します(`src/outbound.rs`)。

1. セキュア入力中(パスワード欄など)は、リランキング自体を行いません。
2. パスワードマネージャ・ターミナル系アプリ(1Password、Bitwarden、キーチェーンアクセス、Terminal、iTerm2、Warp、Ghostty、kitty、Alacritty 等)では、リランキング自体を行いません。
3. 直前の文に API キー、秘密鍵、JWT、メールアドレス、長い数字列、長いトークン風の文字列、「パスワード」などの語が含まれる場合は、直前の文を送らず候補と読みだけで行います。

### 3. Install

NovakeyR.app を `/Library/Input Methods` 内にコピーします。

```sh
sudo cp -r output/NovakeyR.app "/Library/Input Methods"
```

再起動（または、ログアウト→再ログイン）することで有効になります。

環境設定 > キーボード > 入力ソース の英語から`NovakeyR`を追加してください。

日本語モードについては開発中です。
