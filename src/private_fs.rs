#![forbid(unsafe_code)]
//! 入力内容が残るファイルを、本人だけが読める権限 (ファイル 0600 / ディレクトリ 0700) で作る。
use std::fs::{DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

/// 新しく作るディレクトリだけ 0700 にする (既存のものは変えない)。
pub fn create_dir_all(dir: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// 新規作成の書き込み用ファイルを 0600 で開く。
pub fn create_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)
}

/// 追記用に、なければ 0600 で作って開く。
pub fn open_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).mode(0o600).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn new_dirs_and_files_are_owner_only() {
        let root = std::env::temp_dir().join(format!("novakey-r-private-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("a/b");
        create_dir_all(&dir).unwrap();
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&dir), 0o700);
        create_file(&dir.join("f")).unwrap();
        open_append(&dir.join("g")).unwrap();
        assert_eq!(mode(&dir.join("f")), 0o600);
        assert_eq!(mode(&dir.join("g")), 0o600);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
