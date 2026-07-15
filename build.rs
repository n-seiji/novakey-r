use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let status = Command::new("swiftc")
        .args(["swift/LLMBridge.swift", "-emit-library", "-static", "-o"])
        .arg(out_dir.join("libLLMBridge.a"))
        .args(["-target", "arm64-apple-macos26.0"])
        .status()
        .expect("swiftc の起動に失敗");
    assert!(status.success(), "LLMBridge.swift のコンパイルに失敗");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=LLMBridge");
    println!("cargo:rustc-link-lib=framework=FoundationModels");
    // swiftc static lib は Swift ランタイムのリンクパスが必要
    println!("cargo:rustc-link-search=native=/usr/lib/swift");
    // Swift ランタイム dylib を実行時に解決するための rpath
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    println!("cargo:rerun-if-changed=swift/LLMBridge.swift");
}
