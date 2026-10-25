# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

NovakeyR is a macOS Input Method Engine (IME) written entirely in Rust (no Swift). It converts romaji to kana, converts kana to kanji with a Rust port of azooKey's conversion engine, re-ranks full-sentence candidates with a small on-device language model (the judge), and learns the candidates you commit. It also intentionally turns some typed characters into "typos" (`l` -> `I` / `|`, `O` -> `0`, ...) for a playful typing experience.

## Development Commands

Tasks are defined in `mise.toml`.

### Building and Packaging
- `mise run app` - Build the release binary and package `output/NovakeyR.app` (copies `Info.plist`, `resources/` including `resources/judge/` if present, `dictionary/`, `NOTICE`)
- `mise run judge-model` - Download rinna/japanese-gpt2-xsmall (MIT, pinned revision) from Hugging Face and write `resources/judge/model_q8_0.gguf` + `tokenizer.json` with `tools/gen_judge_model.py` (uv script; set `HF_HUB_OFFLINE=1` to use only the local HF cache). The files are 41MB, gitignored and never committed. Run it before `mise run app`; without them the app still works on the engine's order and logs `judge: not configured` once
- `cargo build --release` - Build the Rust binary only (must stay free of warnings)
- `mise run install` / `mise run uninstall` - Copy to / remove from `~/Library/Input Methods`
- `mise run logs` - Stream the IME's NSLog output
- `mise run reset-memory` - Delete the learning data

### Testing
- `cargo test` and `cargo test --release` - Unit tests (composer, session, controller, keymap, kkc, ...)
- `mise run scenarios` - Replay `tools/scenarios.txt` headless; a line with an expectation that does not hold fails
- `mise run scenarios-learning` - Same for `tools/learning_scenarios.txt` with learning on (memory goes to a temp dir)
- `mise run simulate -- 'kyouha<space><enter>'` - Run a key sequence through the real key path without Cocoa (`NOVAKEY_SIM_LEARNING=1` to enable learning)
  - Besides keys, the sequence takes IMK-side event tokens (numbers are 1-based): `<pick:N>` (click a candidate), `<highlight:N>`, `<live>` / `<space-mode>`, `<commit-all>` (commitComposition). See the header of `src/simulate.rs`.
- `mise run wysiwyg` - Same files with `--check-preedit` (also valid for `--simulate`): after each key, if no romaji is pending, the underlined text must equal what Enter would commit (`Composer::preedit` vs `commit_preview`); a break fails the line as `WYSI`
- `mise run main-queue` - Post tasks from another thread with `platform::run_on_main_boxed` and check they run on the main thread, in order, with a panic in one not stopping the rest (no network)
- `mise run kkc-compare` - Compare the Rust engine against the Swift golden data in `tools/golden/` (top-1/3/9 must stay at 100%)
- `mise run judge-bench -- <testset.tsv>` - Judge accuracy and time over the real engine + judge path (`--judge-bench`; each line is `reading<TAB>gold1|gold2`, prints engine top-1 vs judged top-1 and judge ms). Needs `resources/judge/` (or `NOVAKEY_JUDGE_DIR`). `--judge-dump <nbest.json>` prints per-candidate log-likelihoods
- `NOVAKEY_SIM_TIMING=1` with `--simulate*` prints per-key and judge time percentiles and RSS (set `NOVAKEY_JUDGE_DIR` to make the simulator load the judge; it is off there by default so the oracles stay deterministic). The key-event trace also records `keyTime` and `judge` (milliseconds only)
- Other dev tools: `--kkc-check`, `--kkc-convert <reading>`, `--kkc-bench <reading>` (run `./target/release/novakey_r <flag>`)
- Manual testing of the IME itself needs the app installed in `~/Library/Input Methods` and added in System Settings > Keyboard > Input Sources.

### Dictionary location
Order: `NOVAKEY_DICTIONARY` (an invalid value is an error, no silent fallback) -> `<exe>/../Resources/dictionary` (app bundle) -> the repo's `dictionary/` **only in debug builds and tests**. Release binaries never fall back to the repo, so a missing bundle copy shows up in dev installs. The mise tasks above set `NOVAKEY_DICTIONARY` for you; when running `./target/release/novakey_r --kkc-*` / `--simulate*` by hand, set it to `$PWD/dictionary`.

If the dictionary cannot be opened, the failure is logged once (NSLog), reopening is retried at most every 5 minutes, and conversion degrades to hiragana / katakana / raw candidates.

## Architecture

- `src/main.rs` - Entry point; dispatches dev-tool flags, otherwise starts the IME (`imk::register_controller`, `imk::run`)
- `src/platform.rs` - Cocoa / InputMethodKit / libdispatch glue via `objc2` (NSLog, main-queue dispatch wrapped in `catch_unwind`, candidate panel, defaults, secure-input check). Keep new `unsafe` here (it also holds the Accelerate `cblas_sgemm` (`sgemm_abt`) and NEON `q8_dot_rows` code the judge calls, plus the QoS / App Nap calls); `src/imk_client.rs` (host app text client) and the Objective-C class declaration in `src/imk.rs` are the only other places with it. `src/judge.rs` is `#![forbid(unsafe_code)]`. All other modules are `#![forbid(unsafe_code)]` or have none.
- `src/imk.rs` - `NovakeyRInputController` (IMKInputController subclass). Turns IMK callbacks into `Session` calls and executes the returned `Effect`s. Holds the per-controller state.
- `src/session.rs` - One key's worth of decisions (delegates to the composer); returns `Effect`s. Headless, drives `simulate.rs` and the tests.
- `src/keymap.rs`, `src/romaji_converter.rs`, `src/translit.rs`, `src/symbols.rs`, `src/loanword.rs`, `src/emoji.rs` - Key classification, romaji -> kana, script conversion (F6-F10), symbol variants, katakana-word -> English candidates, reading -> emoji candidates (data in `resources/*.tsv`, generated by `tools/gen_*.py`)
- `src/composer.rs` - Composition phases (Composing / Converting / Dangling / ...), the candidate list, and the only owner of the pending romaji (`preedit()` decides the underlined text, `commit_preview()` what Enter commits). Falls back to hiragana + katakana candidates when the engine returns nothing.
- `src/kkc/` - azooKey-compatible kana-kanji engine: `dict` / `louds` (dictionary files), `lattice` (N-best Viterbi), `wise` (dynamic entries), `result` (candidate assembly), `learning` (committed candidates -> `learning.tsv`; in-memory update is immediate, a background writer coalesces file writes), `compare` / `check` (dev tools). `RustEngine` is the `CandidateEngine` used by the IME.
- `src/judge.rs` - On-device judge: rinna/japanese-gpt2-xsmall (GPT-2, 6 layers) kept as q8_0 blocks. `RustEngine::convert` scores every `Source::Full` candidate by log-likelihood and reorders them together with the engine score (live and Space modes). Small batches use an integer dot product (NEON sdot, `platform::q8_dot_rows`), larger ones dequantize per layer into Accelerate sgemm; a shared-prefix KV cache spans keystrokes; the model loads on a background thread at startup (`preload`) and workers run at QOS_USER_INTERACTIVE with App Nap held off. Keys typed before the load finishes just use the engine order. No network, and the model files are looked up under `Resources/judge` (`NOVAKEY_JUDGE_DIR` overrides). Peak RSS is about 175-190MB with the judge loaded. Median judge time is about 8ms
- `src/outbound.rs` - Whether learning is allowed (not under secure input or in password managers).
- `src/trace.rs` - Optional key-event trace (`defaults write me.sijis.inputmethod.NovakeyR NovakeyRTrace -bool true`); never records typed text.

### Rules worth remembering
- Never log or print user text (readings, candidates, surrounding text).
- Learning data lives in `~/Library/Application Support/NovakeyR/rust-learning/learning.tsv` (`NOVAKEY_MEMORY_DIR` to override, `NOVAKEY_DISABLE_LEARNING=1` to turn off). Concurrent IME processes are last-writer-wins. Words committed as English (Dangling) go to `english.txt` in the same directory (`src/english_memory.rs`).
- Live conversion is one process-wide setting (`NovakeyRLiveConversionEnabled`, default off = Space); every controller follows it on its next key / menu open.

### Dependencies
- `objc2`, `objc2-foundation` - Objective-C runtime bindings
- `libc`, `rand`, `once_cell`, `regex`, `unicode-segmentation`, `unicode-normalization`, `serde_json`
- `candle-core` (GGUF reader and Accelerate), `tokenizers` (onig), `rayon` - the on-device judge

## Claude Code Configuration

### Ignored Directories
Claude Code should ignore the following directories (as specified in .gitignore):
- `.build/` - Build artifacts
- `output/` - Generated app bundles
- `target/` - Rust compilation artifacts
