<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/wordmark-dark.svg">
  <img src="docs/assets/wordmark-light.svg" alt="Shizuku" width="320">
</picture>

**English** · [日本語](README.ja.md)

**A Japanese input method for macOS that lets you type English without switching modes.**

Kana-kanji conversion is written in Rust and runs entirely on your Mac. What you type stays there.

<table>
  <tr>
    <td align="center" valign="top" width="33%">
      <img src="docs/assets/tile-convert.svg" width="200" alt="あしたははれです becomes 明日は晴れです with Space and is committed with Enter"><br>
      <b>Convert</b><br>
      <sub>Space to convert, Enter to commit.</sub>
    </td>
    <td align="center" valign="top" width="33%">
      <img src="docs/assets/tile-english.svg" width="200" alt="ぎ turns into git, and git merge is typed with a plain space"><br>
      <b>English as you type it</b><br>
      <sub>English words stay English. Space is a space.</sub>
    </td>
    <td align="center" valign="top" width="33%">
      <img src="docs/assets/tile-mixed.svg" width="200" alt="wo after artifact becomes を and converts to artifactを使う"><br>
      <b>Mixed</b><br>
      <sub>Type artifactを使う in one go.</sub>
    </td>
  </tr>
</table>

- **Conversion.** Space converts, and pressing it again moves to the next candidate. Number keys pick one. Enter commits exactly what is shown. The romaji table is the same as Google Japanese Input (Mozc).
- **English.** English words that cannot become kana stay in English letters (enter, merge). Space in the middle of English inserts a space. A word you have committed as English shows up in English from then on (git).
- **Candidates.** English words for katakana loanwords (デスクトップ → desktop) and emoji (ねこ → 🐈).
- **Learning.** The candidates you pick come first next time. Nothing is learned in password fields or password managers.
- **Re-ranking.** A small Japanese language model (rinna japanese-gpt2-xsmall) runs on your Mac and re-orders the full-sentence candidates so the natural one comes first. There is no network access. It adds about 140 MB of memory (peak about 181 MB instead of 41 MB, measured with `/usr/bin/time -l` on a long headless input), takes about 8 ms per decision at the median, and needs the model files (see below); without them the engine's own order is used.
- **Live conversion (optional).** Shows the converted text as you type. Toggle it from the input menu.

## Install

Requirements: macOS 26 or later, Apple Silicon (M1 or later). Intel Macs are not supported.

Download `Shizuku-<version>.pkg` from Releases and open it. The installer puts Shizuku in `~/Library/Input Methods` (only for you, so it does not ask for a password). The package is signed with Developer ID and notarized by Apple.

The first time, add Shizuku in System Settings > Keyboard > Input Sources. If it is not listed yet, log out and back in first.

### Update

Open the newer `.pkg`. It installs over the old version, and Shizuku stays in Input Sources. Do not delete the old app first: deleting it drops Shizuku from Input Sources, and you would have to add it again.

### Uninstall

1. Remove Shizuku from Input Sources.
2. Delete `~/Library/Input Methods/Shizuku.app`.
3. Remove the install receipt: `pkgutil --forget dev.n-seiji.inputmethod.Shizuku.pkg --volume ~` (the receipt is in your home, so plain `pkgutil --forget` without `--volume ~` does not find it).
4. To also erase what Shizuku learned and its settings, delete `~/Library/Application Support/Shizuku`, `~/Library/Logs/Shizuku` and `~/Library/Preferences/dev.n-seiji.inputmethod.Shizuku.plist`.

### Build from source

Needs [mise](https://mise.jdx.dev/) and Rust. Re-ranking needs model files, which are not in the repository. Build them once (downloads about 41 MB from Hugging Face; [uv](https://docs.astral.sh/uv/) is installed by mise), then install. You can skip the first step and get the engine's own order.

```sh
mise run judge-model
mise run install
```

This installs `~/Library/Input Methods/Shizuku.app`. Log out and back in, then add Shizuku in System Settings > Keyboard > Input Sources.

### Dependencies

Dependency versions published less than 7 days ago are not used. `mise run release-age` (`scripts/check-release-age.sh`) fails if `Cargo.lock` pins a crates.io version younger than that (`MIN_AGE_DAYS` overrides it); CI runs it on every push and pull request. The judge model script sets uv's `exclude-newer = "7 days"` and `mise.toml` sets `minimum_release_age = "7d"`. For an emergency exception, add a line `name version # reason` to `tools/release-age-allow.txt` and remove it once the version is old enough.

## No network at runtime

Shizuku opens no network connections. `scripts/check-no-network.sh` checks the sources and the built binary for network use, and CI runs it on every push and pull request. This is a guarantee about runtime: the judge model is fetched once, at build time. You can also check yourself by reading the sources and looking at `Cargo.lock` for the crates it links.

## Keys

| Key | What it does |
|---|---|
| Space | Convert / next candidate. A plain space in the middle of English |
| Shift+Space | Previous candidate. Converts in the middle of English |
| ↑ ↓ | Move through candidates |
| 1–9 | Pick a candidate and commit |
| Enter | Commit exactly what is shown |
| Esc / Backspace | Cancel the conversion |
| Ctrl+J / K / L / ; / : | Hiragana / katakana / full-width alphanumeric / half-width katakana / half-width alphanumeric |
| F6–F10 | Same as above |

## Where data lives

Learning is stored in `~/Library/Application Support/Shizuku/rust-learning/` (`learning.tsv` and `english.txt`). Run `mise run reset-memory` to erase it. The key-event trace is off by default, and it never records what you type.

## Releasing

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

## Credits

- Kana-kanji conversion (`src/kkc/`) is a Rust port of [AzooKeyKanaKanjiConverter](https://github.com/azooKey/AzooKeyKanaKanjiConverter) (MIT, © 2023 Miwa / Ensan).
- The conversion dictionary (`dictionary/`) is data from [azooKey_dictionary_storage](https://github.com/ensan-hcl/azooKey_dictionary_storage) (Apache-2.0).
- The romaji table (`src/romaji_table.tsv`) is [Mozc](https://github.com/google/mozc)'s `romanji-hiragana.tsv` (BSD-3-Clause, © Google Inc.).
- English words for loanwords (`resources/loanwords.tsv`) are built from [EDRDG](https://www.edrdg.org/)'s JMdict (CC BY-SA 4.0).
- Emoji (`resources/emoji.tsv`) are built from Unicode emoji data and CLDR Japanese annotations (Unicode License v3) via [azooKey_emoji_dictionary_storage](https://github.com/ensan-hcl/azooKey_emoji_dictionary_storage).
- Re-ranking uses [rinna/japanese-gpt2-xsmall](https://huggingface.co/rinna/japanese-gpt2-xsmall) (MIT, © rinna Co., Ltd.), converted by `tools/gen_judge_model.py` and not included in this repository.
- The wordmark is outlined from [Quicksand](https://github.com/andrew-paglinawan/QuicksandFamily) (SIL Open Font License 1.1).

Full license texts are in [`NOTICE`](NOTICE) and [`dictionary/NOTICE`](dictionary/NOTICE).

## License

GPL-3.0-or-later. See [`LICENSE`](LICENSE). Third-party notices are in [`NOTICE`](NOTICE), and the licenses of the Rust crates are in [`THIRD_PARTY_LICENSES`](THIRD_PARTY_LICENSES).
