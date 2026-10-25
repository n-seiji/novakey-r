# Releasing

For maintainers. Releases are built, signed and notarized by `.github/workflows/release.yml`; nothing is published until the `release` environment is approved.

1. Once, in this order (the order matters: with the variable set before the environment and its reviewer exist, a release would be published without anyone approving it):
   1. Run `scripts/setup-release-env.sh`. It creates both environments, `signing` (holds the secrets, main only) and `release` (one required reviewer, main only).
   2. Set the seven secrets in the `signing` environment (`gh` prompts for the value of the ones without stdin). `cert.p12` holds the Developer ID Application certificate (signs the app) and `installer.p12` the Developer ID Installer certificate (signs the package), each with its private key:
      ```sh
      base64 -i cert.p12 | gh secret set DEVELOPER_ID_P12_BASE64 --env signing
      gh secret set DEVELOPER_ID_P12_PASSWORD --env signing
      base64 -i installer.p12 | gh secret set DEVELOPER_ID_INSTALLER_P12_BASE64 --env signing
      gh secret set DEVELOPER_ID_INSTALLER_P12_PASSWORD --env signing
      gh secret set ASC_KEY_ID --env signing
      gh secret set ASC_ISSUER_ID --env signing
      base64 -i AuthKey_XXXXXXXXXX.p8 | gh secret set ASC_KEY_P8_BASE64 --env signing
      ```
   3. Set the repository variable that turns releases on: `gh variable set RELEASE_ENABLED --body true`. Until it is `true`, the workflow only reports "release disabled" and builds nothing, so merging to main is safe.

   The secrets are scoped to the `signing` environment, which only jobs running on main can use, so workflows on other branches cannot read them. Use a dedicated App Store Connect API key for this repository, with the Developer role, for notarization.
2. Bump `version` in `Cargo.toml` and merge to main. The workflow builds the app, signs, notarizes and staples it, and zips it. It then builds `Shizuku-<version>.pkg` from the stapled app (`scripts/build-pkg.sh`), signs the package with the Developer ID Installer identity, notarizes and staples the package, and checks it with `spctl --assess --type install`. Finally it creates a draft release with the `.pkg`, the `.app.zip` (secondary) and their `.sha256` files.
3. Test the draft before approving it:
   - Download the `.pkg` from the draft **in Safari**, so it gets the quarantine attribute. Open it, check that the installer does not ask for a password, log out and back in, add the input source and type.
   - Open the same `.pkg` again over the installed one and check that Shizuku is still in Input Sources.
   - If Gatekeeper blocks the package or the IME, do not approve.
4. Approve the `release` environment. The publish job checks the draft against the build's checksums, attests the `.pkg` and the zip and publishes the release.
