<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/wordmark-dark.svg">
  <img src="docs/assets/wordmark-light.svg" alt="NovakeyR" width="320">
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

Requires macOS 26 or later, [mise](https://mise.jdx.dev/), and Rust.

Re-ranking needs model files, which are not in the repository. Build them once (downloads about 41 MB from Hugging Face; [uv](https://docs.astral.sh/uv/) is installed by mise), then install. You can skip the first step and get the engine's own order.

```sh
mise run judge-model
mise run install
```

This installs `~/Library/Input Methods/NovakeyR.app`. Log out and back in, then add NovakeyR in System Settings > Keyboard > Input Sources.

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

Learning is stored in `~/Library/Application Support/NovakeyR/rust-learning/` (`learning.tsv` and `english.txt`). Run `mise run reset-memory` to erase it.

## Credits

- Kana-kanji conversion (`src/kkc/`) is a Rust port of [AzooKeyKanaKanjiConverter](https://github.com/azooKey/AzooKeyKanaKanjiConverter) (MIT, © 2023 Miwa / Ensan).
- The conversion dictionary (`dictionary/`) is data from [azooKey_dictionary_storage](https://github.com/ensan-hcl/azooKey_dictionary_storage) (Apache-2.0).
- The romaji table (`src/romaji_table.tsv`) is [Mozc](https://github.com/google/mozc)'s `romanji-hiragana.tsv` (BSD-3-Clause, © Google Inc.).
- English words for loanwords (`resources/loanwords.tsv`) are built from [EDRDG](https://www.edrdg.org/)'s JMdict (CC BY-SA 4.0).
- Emoji (`resources/emoji.tsv`) are built from Unicode emoji data and CLDR Japanese annotations (Unicode License v3) via [azooKey_emoji_dictionary_storage](https://github.com/ensan-hcl/azooKey_emoji_dictionary_storage).
- Re-ranking uses [rinna/japanese-gpt2-xsmall](https://huggingface.co/rinna/japanese-gpt2-xsmall) (MIT, © rinna Co., Ltd.), converted by `tools/gen_judge_model.py` and not included in this repository.
- The wordmark is outlined from [Newsreader](https://github.com/productiontype/Newsreader) (SIL Open Font License 1.1).

Full license texts are in [`NOTICE`](NOTICE) and [`dictionary/NOTICE`](dictionary/NOTICE).
