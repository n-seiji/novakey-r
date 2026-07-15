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
- `src/imk.rs` - Input Method Kit integration (controller class, marked text, playful conversion)
- `src/romaji_converter.rs` - Pure-Rust romaji-to-hiragana conversion engine (unit tested)

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
- Conversion happens incrementally as you type; pending romaji is shown as underlined marked text
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