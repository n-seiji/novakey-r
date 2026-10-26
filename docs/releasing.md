# リリース手順

メンテナ向けです。リリースは `.github/workflows/release.yml` がビルド・署名・公証まで行います。`release` 環境を承認するまでは何も公開されません。

1. 初回のみ、次の順で行います (順序が大事です。環境と承認者を作る前に変数を設定すると、誰の承認もなくリリースが公開されてしまいます)。
   1. `scripts/setup-release-env.sh` を実行します。`signing` 環境 (シークレットを置く、main のみ) と `release` 環境 (承認者 1 人、main のみ) の両方を作ります。
   2. 7 つのシークレットを `signing` 環境に登録します (標準入力を使わないものは `gh` が値を尋ねます)。`cert.p12` は Developer ID Application 証明書 (アプリの署名)、`installer.p12` は Developer ID Installer 証明書 (パッケージの署名) で、どちらも秘密鍵を含めます。
      ```sh
      base64 -i cert.p12 | gh secret set DEVELOPER_ID_P12_BASE64 --env signing
      gh secret set DEVELOPER_ID_P12_PASSWORD --env signing
      base64 -i installer.p12 | gh secret set DEVELOPER_ID_INSTALLER_P12_BASE64 --env signing
      gh secret set DEVELOPER_ID_INSTALLER_P12_PASSWORD --env signing
      gh secret set ASC_KEY_ID --env signing
      gh secret set ASC_ISSUER_ID --env signing
      base64 -i AuthKey_XXXXXXXXXX.p8 | gh secret set ASC_KEY_P8_BASE64 --env signing
      ```
   3. リリースを有効にするリポジトリ変数を設定します: `gh variable set RELEASE_ENABLED --body true`。`true` になるまでワークフローは「release disabled」と報告するだけで何もビルドしないので、main へのマージは安全です。

   シークレットは `signing` 環境に紐づき、この環境を使えるのは main で動くジョブだけなので、他のブランチのワークフローからは読めません。公証には、このリポジトリ専用の App Store Connect API キー (Developer ロール) を使ってください。
2. `Cargo.toml` の `version` を上げて main にマージします。ワークフローがアプリをビルドして署名し、公証と staple を行って zip にします。続いて staple 済みのアプリから `Shizuku-<version>.pkg` を作り (`scripts/build-pkg.sh`)、Developer ID Installer で署名し、公証と staple を行い、`spctl --assess --type install` で確かめます。版なしの `Shizuku.pkg` にも同じものをコピーします (`releases/latest/download/Shizuku.pkg` が常に最新版を指すように)。最後に `.pkg`、`.app.zip` (副) とそれぞれの `.sha256` を載せた下書きリリースを作ります。
3. 承認する前に下書きを試します。
   - 下書きの `.pkg` を **Safari で** ダウンロードします (quarantine 属性を付けるため)。開いてパスワードを求められないことを確かめ、ログインし直して入力ソースを追加し、実際に打ちます。
   - 同じ `.pkg` をもう一度、入っている版の上から開き、Shizuku が入力ソースに残っていることを確かめます。
   - Gatekeeper がパッケージか IME を止めるなら、承認しません。
4. `release` 環境を承認します。publish ジョブが下書きをビルド時のチェックサムと照合し、`.pkg` と zip の attestation を作ってリリースを公開します。
