#![forbid(unsafe_code)]
//! `--register [<path to .app>]`: バンドルを入力ソースとして登録し、有効にする。
//! バンドルが消えて置き直されると macOS は入力ソースの一覧から外すので、cask の postflight と
//! `mise run install` がこれを呼ぶ。IME のサーバーは起動しない。
use std::path::{Path, PathBuf};

use crate::platform;

const BUNDLE_ID: &str = "dev.n-seiji.inputmethod.NovakeyR";

/// `exe` を含む一番内側の `.app` ディレクトリ。無ければ None。
fn enclosing_app(exe: &Path) -> Option<PathBuf> {
    exe.ancestors().skip(1).find(|p| p.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf)
}

/// 対象のバンドルを決める。引数があればそれ、無ければ実行ファイルを含む `.app`。
fn target(arg: Option<&str>) -> Result<PathBuf, &'static str> {
    let path = match arg {
        Some(a) => PathBuf::from(a),
        None => {
            let exe = std::env::current_exe().map_err(|_| "cannot locate the executable")?;
            let exe = exe.canonicalize().unwrap_or(exe);
            enclosing_app(&exe).ok_or("the executable is not inside an .app bundle; pass --register <path to .app>")?
        }
    };
    let path = path.canonicalize().map_err(|_| "bundle path does not exist")?;
    if !path.is_dir() || path.extension().is_none_or(|e| e != "app") {
        return Err("bundle path is not an .app directory");
    }
    Ok(path)
}

/// 終了コードを返す。0 = TIS が登録と有効化を受け付けた、1 = それ以外。数だけを出す。
/// 受け付けられても、サードパーティの IME は macOS がユーザーの許可を待つことがある (macOS 12 以降)。
pub fn run_cli(arg: Option<&str>) -> i32 {
    let bundle = match target(arg.filter(|a| !a.starts_with("--"))) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("register: {e}");
            return 1;
        }
    };
    let r = platform::register_input_source(&bundle, BUNDLE_ID);
    println!(
        "register_status={} found={} enable_capable={} enable_ok={} enable_failures={}",
        r.register_status, r.found, r.enable_capable, r.enabled, r.enable_failures
    );
    i32::from(!(r.register_status == 0 && r.enabled > 0 && r.enable_failures == 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enclosing_app_finds_the_bundle_of_the_executable() {
        let exe = Path::new("/Users/x/Library/Input Methods/NovakeyR.app/Contents/MacOS/novakey_r");
        assert_eq!(enclosing_app(exe), Some(PathBuf::from("/Users/x/Library/Input Methods/NovakeyR.app")));
    }

    #[test]
    fn enclosing_app_is_none_outside_a_bundle() {
        assert_eq!(enclosing_app(Path::new("/repo/target/release/novakey_r")), None);
    }

    #[test]
    fn target_rejects_a_path_that_is_not_an_app() {
        assert!(target(Some("/")).is_err());
        assert!(target(Some("/nonexistent/NovakeyR.app")).is_err());
    }
}
