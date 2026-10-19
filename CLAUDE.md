# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

NovakeyR is a Japanese Input Method Engine (IME) written in Rust for macOS. It provides romaji-to-hiragana conversion functionality while maintaining the original character conversion features for creating playful typing experiences.

## Development Commands

### Building and Packaging (mise tasks, no Makefile)
- `mise run app` - Build the release binary and package it into `output/NovakeyR.app`
- `mise run build` - Build the Rust binary only (`cargo build --release`)
- `mise run test` - Run unit tests (`cargo test`)
- `mise run install` - Build, package, and install into `~/Library/Input Methods` (no sudo)
- `mise run uninstall` - Remove the installed app
- `mise run logs` - Stream NSLog output from the running IME
- `mise run clean` - Remove the generated app bundle
- `mise run run` - Run the packaged binary directly (smoke test)

### Environment Setup
- Requires Rust installation via mise: `mise install rust`
- Use `mise use rust` to activate Rust environment
- **IMPORTANT**: mise environment is already activated in this shell session. DO NOT use `eval "$(mise activate zsh)"` prefix for commands.

### Testing and Development
- Unit tests cover the romaji conversion engine (`cargo test` / `mise run test`)
- Manual testing: `mise run install`, then add "NovakeyR" in System Settings > Keyboard > Input Sources (re-login may be required the first time)

## Architecture

### Core Components
- `src/main.rs` - Entry point that initializes the NSApplication and IMKServer connection
- `src/imk.rs` - Input Method Kit integration (controller class, marked text, candidate window, playful conversion)
- `src/romaji_converter.rs` - Pure-Rust romaji-to-hiragana conversion engine (unit tested)
- `src/composer.rs` - 未確定文の状態機械(ひらがなバッファ+同期n-best候補+選択状態、unit tested)
- `src/loanword.rs` + `resources/loanwords.tsv` - カタカナ語→英単語の表(JMdict由来、`tools/gen_loanwords.py` で生成)。変換候補に desktop などを足す
- `src/kkc_bridge.rs` + `swift/AzooKeyBridge/` - AzooKeyKanaKanjiConverter(辞書ラティス)への同期FFI
- `src/llm_bridge.rs` + `swift/LLMBridge.swift` - FoundationModels によるLLM候補リランキング(非同期・失敗無害)
- 変換フロー: 既定は非ライブ(未確定文はひらがな表示、メニューでライブ変換に切替可)。打鍵ごとに辞書エンジンで同期変換し、スペース1回目で第1候補、2回目以降で候補送り+候補ウィンドウ(1〜9で候補確定)。ローマ字そのままの候補(raw)を第1候補の直後に追加。先頭が大文字の語は英単語としてそのまま入力。局面は composer の Phase、キー対応表は keymap.rs。
  Enter 確定(学習更新)。Esc で生ひらがな。LLM は裏で候補番号を選び未操作なら第1候補を差し替える
- 辞書リソースバンドルを .app の Contents/MacOS/ に同梱(mise run app が実施)

### Key Technical Details
- Uses macOS InputMethodKit framework via Objective-C bindings (`objc2` / `objc2-foundation`)
- Custom `NovakeyRInputController` class extends `IMKInputController` via `declare_class!`;
  `register_controller()` must run before IMKServer connects so the class is registered
  with the Objective-C runtime
- Per-controller state: each controller instance owns a `RomajiConverter`
  (`RefCell` ivar set in `initWithServer:delegate:client:`)
- Japanese input processing:
  - Alphabetic keys feed `RomajiConverter::process_input`; converted kana is committed
    via `insertText:replacementRange:`, pending romaji is shown as marked text
    (`setMarkedText:selectionRange:replacementRange:`)
  - Non-character keys (delete / return / escape) are handled in `didCommandBySelector:client:`
  - `commitComposition:` flushes pending romaji when focus changes
- Playful character conversion (`playful_convert()`) randomizes select characters (1/0/space, etc.)
- Uses unsafe Rust blocks for Objective-C interop

### Dependencies
- `objc2` - Objective-C runtime bindings
- `objc2-foundation` - Foundation framework bindings (NSString, NSRange, ...)
- `libc` - C library bindings
- `rand` - Random number generation for character selection
- `once_cell` - Lazy statics

### Installation Process
`mise run install` copies the built app to `~/Library/Input Methods` (user-level, no sudo). First activation may require logout/login. Users then add "NovakeyR" from System Preferences > Keyboard > Input Sources.

### Japanese Input Features
- Romaji to hiragana conversion (a→あ, ka→か, etc.)
- Supports common romaji patterns including alternative spellings (si/shi, tu/tsu, etc.)
- Kana-kanji conversion happens incrementally as you type; pending romaji is shown after the current candidate as marked text
- Space / newline / focus change commit any remaining buffered romaji
- Automatic buffer management for multi-character sequences
- Backspace handling for romaji buffer editing
- Support for Japanese character repertoires: Hiragana, Katakana, Kanji, Latin

## Git Branch Strategy

### Branch Structure
- `main` - Production-ready code, stable releases
- `develop` - Development branch, integration of features
- `feature/*` - Feature branches for new functionality
- `fix/*` - Bug fix branches
- `hotfix/*` - Emergency fixes for production

### Workflow
1. All development work happens on `develop` branch
2. Create feature branches from `develop` for new features
3. Create fix branches from `develop` for bug fixes
4. Merge completed work back to `develop` via pull requests
5. When ready for release, merge `develop` to `main`
6. Hotfixes branch from `main` and merge back to both `main` and `develop`

### Current Branch Status
- **Active branch**: `develop` (as shown in git status)
- **Main branch**: `main` (target for production releases)

### Commit Convention
- Use descriptive commit messages
- Include emoji prefixes when appropriate:
  - `:sparkles:` (✨) - New features
  - `:bug:` (🐛) - Bug fixes
  - `:books:` (📚) - Documentation
  - `:wrench:` (🔧) - Configuration changes
  - `:construction:` (🚧) - Work in progress
  - `:recycle:` (♻️) - Refactoring

## Claude Code Configuration

### Ignored Directories
Claude Code should ignore the following directories (as specified in .gitignore):
- `.build/` - Build artifacts
- `output/` - Generated app bundles
- `target/` - Rust compilation artifacts