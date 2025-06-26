# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

NovakeyR is a novelty macOS Input Method Engine (IME) written in Rust that intentionally introduces "typos" by randomly converting certain characters (like 'l' to 'I' or '|', 'O' to '0', etc.) to create a playful typing experience.

## Development Commands

### Building and Packaging
- `make app` - Build the release binary and package it into a macOS app bundle in `output/NovakeyR.app`
- `cargo build --release` - Build the Rust binary only
- `make clean` - Remove the generated app bundle
- `make run` - Run the compiled app directly

### Testing and Development
- The project currently has no automated tests
- Manual testing requires installing the IME in `/Library/Input Methods` and adding it in System Preferences

## Architecture

### Core Components
- `src/main.rs` - Entry point that initializes the NSApplication and IMKServer connection
- `src/imk.rs` - Contains the Input Method Kit integration and character conversion logic

### Key Technical Details
- Uses macOS InputMethodKit framework via Objective-C bindings
- Character conversion happens in `convert()` function with randomized selection from predefined character mappings
- Custom `NovakeyRInputController` class extends `IMKInputController` to handle input events
- Uses unsafe Rust blocks extensively for Objective-C interop via the `objc` and `cocoa` crates

### Dependencies
- `objc` - Objective-C runtime bindings
- `cocoa` - macOS Cocoa framework bindings  
- `libc` - C library bindings
- `rand` - Random number generation for character selection

### Installation Process
The built app must be manually copied to `/Library/Input Methods` and requires logout/login or restart to activate. Users then add "NovakeyR" from System Preferences > Keyboard > Input Sources.

## Claude Code Configuration

### Ignored Directories
Claude Code should ignore the following directories (as specified in .gitignore):
- `.build/` - Build artifacts
- `output/` - Generated app bundles
- `target/` - Rust compilation artifacts