use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let clang_cache = manifest_dir.join("target/clang-module-cache");
    let swiftpm_cache = manifest_dir.join("target/swiftpm-module-cache");
    let _ = std::fs::create_dir_all(&clang_cache);
    let _ = std::fs::create_dir_all(&swiftpm_cache);
    // swift ランタイムのリンクパスと、実行時に解決するための rpath
    println!("cargo:rustc-link-search=native=/usr/lib/swift");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    // AzooKeyBridge: SPM パッケージを静的ライブラリとしてビルド
    let status = Command::new("swift")
        .env("CLANG_MODULE_CACHE_PATH", &clang_cache)
        .env("SWIFTPM_MODULECACHE_OVERRIDE", &swiftpm_cache)
        .args(["build", "-c", "release", "--disable-sandbox", "--package-path", "swift/AzooKeyBridge"])
        .status()
        .expect("swift build の起動に失敗");
    assert!(status.success(), "AzooKeyBridge のビルドに失敗");

    let bridge_out = PathBuf::from("swift/AzooKeyBridge/.build/release");
    println!(
        "cargo:rustc-link-search=native={}",
        bridge_out.canonicalize().unwrap().display()
    );
    println!("cargo:rustc-link-lib=static=AzooKeyBridge");

    let sdk_path = String::from_utf8(
        Command::new("xcrun")
            .env("CLANG_MODULE_CACHE_PATH", &clang_cache)
            .args(["--show-sdk-path"])
            .output()
            .expect("xcrun の起動に失敗")
            .stdout,
    )
    .unwrap();
    println!("cargo:rustc-link-search=native={}/usr/lib/swift", sdk_path.trim());
    let clt_swift = "/Library/Developer/CommandLineTools/usr/lib/swift/macosx";
    if std::path::Path::new(clt_swift).exists() {
        println!("cargo:rustc-link-search=native={}", clt_swift);
    }

    let profile = env::var("PROFILE").unwrap();
    let target_dir = PathBuf::from(format!("target/{}", profile));
    let bundle_name = "AzooKeyKanaKanjiConverter_KanaKanjiConverterModuleWithDefaultDictionary.bundle";
    let _ = Command::new("rm")
        .arg("-rf")
        .arg(target_dir.join(bundle_name))
        .status();
    let status = Command::new("cp")
        .arg("-R")
        .arg(bridge_out.join(bundle_name))
        .arg(&target_dir)
        .status()
        .expect("bundle コピーの起動に失敗");
    assert!(status.success(), "辞書バンドルのコピーに失敗");

    println!("cargo:rerun-if-changed=swift/AzooKeyBridge/Package.swift");
    println!("cargo:rerun-if-changed=swift/AzooKeyBridge/Sources");
}
