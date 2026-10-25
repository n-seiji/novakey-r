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

  # macOS drops an input source from Input Sources when its bundle directory is deleted, which
  # `input_method` does on every upgrade. Replace the bundle's contents in place instead, so the
  # directory (and the registration) survives upgrades.
  stage_only true

  # Install steps run in Homebrew's sandbox with a temporary HOME, so paths under the real home use
  # `base: :home`. Command arguments have no such base, so they spell the home as /Users/{{user}}
  # ({{user}} is $USER): the install fails if the home directory is not /Users/$USER.
  # The steps stop at the first failure, and are ordered so a failure never leaves a half-replaced bundle:
  # 1. probe that the existing bundle is writable (nothing is changed if it is not),
  # 2. copy without --delete (a failure here leaves old and new files together, never an empty bundle),
  # 3. only then prune files that are gone (--delete),
  # 4. check the signature of the result.
  # --extended-attributes keeps the quarantine flag, as the `input_method` artifact did.
  postflight_steps do
    terminate_process "novakey_r"
    mkdir_p "Library/Input Methods", base: :home
    if_path_exists "Library/Input Methods/NovakeyR.app/Contents", base: :home do
      touch "Library/Input Methods/NovakeyR.app/Contents/.homebrew-write-probe", base: :home
      remove "Library/Input Methods/NovakeyR.app/Contents/.homebrew-write-probe", base: :home
    end
    run "/usr/bin/rsync",
        args:           ["-a", "--extended-attributes", "{{staged_path}}/NovakeyR.app/",
                         "/Users/{{user}}/Library/Input Methods/NovakeyR.app/"],
        writable_paths: ["Library/Input Methods"],
        writable_base:  :home
    run "/usr/bin/rsync",
        args:           ["-a", "--delete", "--extended-attributes", "{{staged_path}}/NovakeyR.app/",
                         "/Users/{{user}}/Library/Input Methods/NovakeyR.app/"],
        writable_paths: ["Library/Input Methods"],
        writable_base:  :home
    run "/usr/bin/codesign",
        args: ["--verify", "--strict", "/Users/{{user}}/Library/Input Methods/NovakeyR.app"]
  end

  # No `uninstall` stanza. `uninstall` and `zap` both count as activatable artifacts, so with `stage_only`
  # Homebrew rejects the cask once `zap` is defined after `uninstall`. Its `delete` would also run on upgrade,
  # removing the directory we keep. The postflight stops the running IME on install and upgrade. Only `zap`
  # removes the bundle, so a plain `brew uninstall` leaves ~/Library/Input Methods/NovakeyR.app in place.
  zap signal: [["TERM", "dev.n-seiji.inputmethod.NovakeyR"]],
      trash:  [
        "~/Library/Application Support/NovakeyR",
        "~/Library/Input Methods/NovakeyR.app",
        "~/Library/Logs/NovakeyR",
        "~/Library/Preferences/dev.n-seiji.inputmethod.NovakeyR.plist",
      ]

  caveats <<~EOS
    Add NovakeyR once in System Settings > Keyboard > Input Sources (Japanese). The first time, you may
    need to log out and back in. Upgrades replace the app in place, so it stays in Input Sources.

    To remove NovakeyR completely, first remove it from Input Sources, then run:
      brew uninstall --zap --cask novakey-r
    A plain `brew uninstall` leaves ~/Library/Input Methods/NovakeyR.app in place.

    Installing needs the home directory to be /Users/$USER.
  EOS
end
