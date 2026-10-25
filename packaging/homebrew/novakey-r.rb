# frozen_string_literal: true

# Template for n-seiji/homebrew-tap (Casks/novakey-r.rb).
# scripts/bump-cask.sh rewrites version and sha256 from a release.
cask "novakey-r" do
  version "0.1.0"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"

  url "https://github.com/n-seiji/novakey-r/releases/download/v#{version}/NovakeyR-#{version}.app.zip"
  name "NovakeyR"
  desc "Japanese input method with on-device conversion"
  homepage "https://github.com/n-seiji/novakey-r"

  depends_on arch: :arm64
  depends_on macos: :tahoe

  input_method "NovakeyR.app"

  uninstall signal: [["TERM", "dev.n-seiji.inputmethod.NovakeyR"]], on_upgrade: :signal

  zap trash: [
    "~/Library/Application Support/NovakeyR",
    "~/Library/Logs/NovakeyR",
    "~/Library/Preferences/dev.n-seiji.inputmethod.NovakeyR.plist",
  ]

  caveats <<~EOS
    Add NovakeyR in System Settings > Keyboard > Input Sources (Japanese).
    You may need to log out and back in first.
    After an upgrade, add NovakeyR to Input Sources again: macOS drops it
    when the bundle is replaced.
    Before uninstalling, remove NovakeyR from Input Sources.
  EOS
end
