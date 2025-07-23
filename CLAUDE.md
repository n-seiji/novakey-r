# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

NovakeyR is a Japanese Input Method Engine (IME) written in Rust for macOS. It provides romaji-to-hiragana conversion functionality while maintaining the original character conversion features for creating playful typing experiences.

## Development Commands

### Building and Packaging
- `eval "$(mise activate zsh)" && make app` - Build the release binary and package it into a macOS app bundle in `output/NovakeyR.app` (requires mise for Rust environment)
- `eval "$(mise activate zsh)" && cargo build --release` - Build the Rust binary only (requires mise for Rust environment)
- `make clean` - Remove the generated app bundle
- `make run` - Run the compiled app directly

### Environment Setup
- Requires Rust installation via mise: `mise install rust`
- Use `mise use rust` to activate Rust environment
- All cargo/rust commands need to be run with `eval "$(mise activate zsh)" &&` prefix

### Testing and Development
- The project currently has no automated tests
- Manual testing requires installing the IME in `/Library/Input Methods` and adding it in System Preferences

## Architecture

### Core Components
- `src/main.rs` - Entry point that initializes the NSApplication and IMKServer connection
- `src/imk.rs` - Contains the Input Method Kit integration and character conversion logic

### Key Technical Details
- Uses macOS InputMethodKit framework via Objective-C bindings
- Japanese input processing:
  - Romaji-to-hiragana conversion via `romaji_to_hiragana()` function
  - Buffered input system for multi-character romaji sequences
  - Space key triggers conversion from buffered romaji to hiragana
  - Hiragana-to-katakana conversion via `hiragana_to_katakana()` function
- Character conversion happens in `convert()` function with randomized selection from predefined character mappings
- Custom `NovakeyRInputController` class extends `IMKInputController` to handle input events
- Uses unsafe Rust blocks extensively for Objective-C interop via the `objc` and `cocoa` crates
- Global `ROMAJI_BUFFER` for tracking romaji input state

### Dependencies
- `objc` - Objective-C runtime bindings
- `cocoa` - macOS Cocoa framework bindings  
- `libc` - C library bindings
- `rand` - Random number generation for character selection

### Installation Process
The built app must be manually copied to `/Library/Input Methods` and requires logout/login or restart to activate. Users then add "NovakeyR" from System Preferences > Keyboard > Input Sources.

### Japanese Input Features
- Romaji to hiragana conversion (a→あ, ka→か, etc.)
- Supports common romaji patterns including alternative spellings (si/shi, tu/tsu, etc.)
- Space key converts buffered romaji to hiragana
- Automatic buffer management for multi-character sequences
- Backspace handling for romaji buffer editing
- Support for Japanese character repertoires: Hiragana, Katakana, Kanji, Latin

## Claude Code Configuration

### Ignored Directories
Claude Code should ignore the following directories (as specified in .gitignore):
- `.build/` - Build artifacts
- `output/` - Generated app bundles
- `target/` - Rust compilation artifacts