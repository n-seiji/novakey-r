#![forbid(unsafe_code)]
//! 学習してよいか・素通しにするかの判定。純粋関数と const テーブルだけで構成する。

/// パスワードを扱うアプリ。学習と文脈の記憶だけを止める (変換は通常どおり使える)。
const PASSWORD_MANAGERS: &[&str] = &[
    "com.1password.1password",
    "com.agilebits.onepassword7",
    "com.bitwarden.desktop",
    "com.apple.keychainaccess",
    "com.apple.Passwords",
    "org.keepassxc.keepassxc",
];

/// 確定を学習し、文脈を覚えてよいか。セキュア入力中・パスワード管理アプリでは学習しない (ファイルや記憶に入力が残るため)。
pub fn learning_allowed(secure_input: bool, bundle_id: Option<&str>) -> bool {
    !secure_input && !bundle_id.is_some_and(|id| PASSWORD_MANAGERS.contains(&id))
}

/// 変換せず、打鍵をそのまま半角 ASCII で素通しすべきか。セキュア入力中だけ (アプリを問わない)。
/// パスワード管理アプリでもセキュア入力でなければ日本語を打てる。
pub fn direct_input(secure_input: bool) -> bool {
    secure_input
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learning_is_blocked_for_secure_input_and_password_managers_only() {
        assert!(learning_allowed(false, Some("com.apple.TextEdit")));
        assert!(learning_allowed(false, None));
        assert!(!learning_allowed(true, Some("com.apple.TextEdit")));
        assert!(!learning_allowed(true, None));
        for id in PASSWORD_MANAGERS {
            assert!(!learning_allowed(false, Some(id)), "{id}");
        }
        assert!(learning_allowed(false, Some("com.apple.Terminal")), "ターミナルでは学習する");
    }

    #[test]
    fn direct_input_is_on_for_secure_input_only() {
        assert!(!direct_input(false));
        assert!(direct_input(true));
    }
}
