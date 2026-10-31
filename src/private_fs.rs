#![forbid(unsafe_code)]
//! 入力内容が残るファイルを、本人だけが読める権限 (ファイル 0600 / ディレクトリ 0700) で作る。
use std::fs::{DirBuilder, File, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// ディレクトリを 0700 で作る。mode は作成時にしか効かないので、既存の末端 (アプリ自身のディレクトリ) も 0700 に直す。
/// 親 (~/Library など) は触らない。
pub fn create_dir_all(dir: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, Permissions::from_mode(0o700))
}

/// 書き込み用ファイルを 0600 で開く (既存なら切り詰め、権限も 0600 に直す)。
pub fn create_file(path: &Path) -> std::io::Result<File> {
    let file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    file.set_permissions(Permissions::from_mode(0o600))?;
    Ok(file)
}

/// 追記用に、なければ 0600 で作って開く (既存でも権限は 0600 に直す)。
pub fn open_append(path: &Path) -> std::io::Result<File> {
    let file = OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
    file.set_permissions(Permissions::from_mode(0o600))?;
    Ok(file)
}

/// 一時ファイル `tmp` に書いて `sync_all` し、`path` へ rename する (電源断でも、半端なファイルが本物に置き換わらない)。
/// 親ディレクトリは 0700 で作り、失敗したら `tmp` を消す。`tmp` は呼び出し側が一意にする。
pub fn write_atomic(path: &Path, tmp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        create_dir_all(dir)?;
    }
    let written = (|| -> std::io::Result<()> {
        let mut file = create_file(tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    written
}

/// `std::fs::copy` の代わり。コピー先は元の権限を引き継がず 0600 になる。
pub fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut src = File::open(from)?;
    let mut dst = create_file(to)?;
    std::io::copy(&mut src, &mut dst)?;
    dst.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("shizuku-private-fs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn new_dirs_and_files_are_owner_only() {
        let root = scratch("new");
        let dir = root.join("a/b");
        create_dir_all(&dir).unwrap();
        assert_eq!(mode(&root.join("a")), 0o700);
        assert_eq!(mode(&dir), 0o700);
        create_file(&dir.join("f")).unwrap();
        open_append(&dir.join("g")).unwrap();
        assert_eq!(mode(&dir.join("f")), 0o600);
        assert_eq!(mode(&dir.join("g")), 0o600);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn existing_leaf_dir_becomes_0700_but_parent_is_untouched() {
        let root = scratch("dir");
        let dir = root.join("app");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, Permissions::from_mode(0o755)).unwrap();
        let parent_mode = mode(&root);
        create_dir_all(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&root), parent_mode);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn existing_files_become_0600() {
        let root = scratch("file");
        for (name, open) in [("w", create_file as fn(&Path) -> std::io::Result<File>), ("a", open_append)] {
            let path = root.join(name);
            std::fs::write(&path, "x").unwrap();
            std::fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
            open(&path).unwrap().write_all(b"y").unwrap();
            assert_eq!(mode(&path), 0o600, "{name}");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn write_atomic_replaces_the_file_and_removes_the_tmp_on_failure() {
        let root = scratch("atomic");
        let (path, tmp) = (root.join("new/data.txt"), root.join("new/data.txt.tmp"));
        write_atomic(&path, &tmp, b"one").unwrap();
        write_atomic(&path, &tmp, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        assert_eq!(mode(&path), 0o600);
        assert!(!tmp.exists());
        // rename が失敗する (行き先がディレクトリ) ときも、一時ファイルは残さない
        let dir_target = root.join("dir");
        std::fs::create_dir(&dir_target).unwrap();
        assert!(write_atomic(&dir_target, &tmp, b"x").is_err());
        assert!(!tmp.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn copy_makes_a_0600_file_even_from_a_world_readable_source() {
        let root = scratch("copy");
        let (from, to) = (root.join("from"), root.join("to"));
        std::fs::write(&from, "data").unwrap();
        std::fs::set_permissions(&from, Permissions::from_mode(0o644)).unwrap();
        copy(&from, &to).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "data");
        assert_eq!(mode(&to), 0o600);
        std::fs::remove_dir_all(&root).unwrap();
    }
}

