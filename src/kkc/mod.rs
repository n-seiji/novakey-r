//! azooKey 互換の辞書リーダー (かな漢字変換の Rust 移植 P1)。
//!
//! 辞書ファイル形式は AzooKeyKanaKanjiConverter v0.11.2 (`LOUDS.swift` / `DicdataStore.swift` /
//! `DictionaryBuilder.swift`) と同一。変換ロジック (lattice など) はまだ持たない。
//! 現在の IME 経路 (Swift) からは使われないため、未使用警告は止めておく。
#![forbid(unsafe_code)]
#![allow(dead_code, unused_imports)]

pub mod check;
pub mod dict;
pub mod louds;

pub use dict::{DictError, Dictionary, Entry};
