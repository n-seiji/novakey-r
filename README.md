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

### 2. Install

NovakeyR.app を `/Library/Input Methods` 内にコピーします。

```sh
sudo cp -r output/NovakeyR.app "/Library/Input Methods"
```

再起動（または、ログアウト→再ログイン）することで有効になります。

環境設定 > キーボード > 入力ソース の英語から`NovakeyR`を追加してください。

日本語モードについては開発中です。
