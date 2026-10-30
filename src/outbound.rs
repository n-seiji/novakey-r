#![forbid(unsafe_code)]
//! セキュア入力・パスワード管理アプリかの判定 (素通しと学習禁止の条件)。純粋関数と const テーブルだけで構成する。

/// パスワードを扱うアプリ。学習を止める。
const PASSWORD_MANAGERS: &[&str] = &[
    "com.1password.1password",
    "com.agilebits.onepassword7",
    "com.bitwarden.desktop",
    "com.apple.keychainaccess",
    "com.apple.Passwords",
    "org.keepassxc.keepassxc",
];

/// 変換せず、打鍵をそのまま半角 ASCII で素通しすべきか。セキュア入力中・パスワード管理アプリが対象。
/// 学習も同じ条件で止める (ファイルに入力が残るため)。
pub fn direct_input(secure_input: bool, bundle_id: Option<&str>) -> bool {
    secure_input || bundle_id.is_some_and(|id| PASSWORD_MANAGERS.contains(&id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_input_is_on_for_secure_input_and_password_managers_only() {
        assert!(!direct_input(false, Some("com.apple.TextEdit")));
        assert!(!direct_input(false, None));
        assert!(direct_input(true, Some("com.apple.TextEdit")));
        assert!(direct_input(true, None));
        for id in PASSWORD_MANAGERS {
            assert!(direct_input(false, Some(id)), "{id}");
        }
        assert!(!direct_input(false, Some("com.apple.Terminal")), "ターミナルでは素通しにせず学習する");
    }
}
