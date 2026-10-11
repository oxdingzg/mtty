# Installing / building mtty

[简体中文](INSTALL.zh-CN.md)

mtty (formerly miaotty) is the native winit/wgpu application.
There is one GUI executable and one CLI. See [identity and migration](APP-IDENTITY.md).

## Install a release

Choose the latest package from [GitHub Releases](https://github.com/oxdingzg/mtty/releases/latest).

| Platform | Package and first launch |
|---|---|
| macOS | Choose the arm64 zip for Apple silicon or x86_64 for Intel. Unzip, move `mtty.app` to Applications, then open it |
| Linux | Install the `.deb` on Debian/Ubuntu, or make the AppImage executable and run it. The tar archive also contains the binaries |
| Windows | Run the MSI installer, or extract the zip and open `mtty.exe`. The MSI also installs `mtty-cli` on PATH |

For current platform signing status and download prompts, see the release notes
and [Windows download guidance](https://mtty.dev/docs/about/windows-downloads/).
The `.sig` files and `minisign.pub` are for optional manual verification, not applications to open.

## Your first workspace

1. Open mtty and use the initial local terminal, or **New SSH Session…** for a remote shell.
2. Open the command palette (`⌘K` on macOS, `Ctrl+Shift+K` elsewhere). **Open Quickly** is `⌘⇧O` / `Ctrl+Shift+Alt+O`.
3. Run an installed agent CLI such as `miao` in a pane. miao reports state automatically; other supported agents expose setup actions for their state hooks.
4. Open a file beside the terminal, then use the details panel (`⌘⇧R` / `Ctrl+Shift+Alt+R`) for files, Git, agent state and queued prompts.

See [shortcuts](SHORTCUTS.md), [configuration](CONFIG.md), [editor](EDITOR.md)
and [remote connections](REMOTE.md) for the next steps.

## From source

```sh
cargo run --release -p mtty-app
cargo build --release -p mtty-app -p mtty-cli
./target/release/mtty --version
```

Rust stable and the platform's wgpu/window-system libraries are required.
The first build compiles wgpu/glyphon and may take a few minutes.

## macOS

```sh
scripts/package-macos.sh
python3 scripts/smoke-hosts.py --bundle dist/mtty.app
bash scripts/install-macos.sh
```

Produces one `dist/mtty.app` with `mtty`, `mtty-cli`, the icon and URL schemes,
signed with a local code-signing identity when one is available (`Apple
Development`, or `MTTY_CODESIGN_IDENTITY`) and ad-hoc otherwise. Installation
archives old known bundles, installs mtty and removes the old native launcher
without removing user configuration.
`PROFILE=debug scripts/package-macos.sh` builds a development bundle.
The installed app uses the macOS system menu bar; a bare binary uses an in-window menu.

## Release packages

[release.yml](../.github/workflows/release.yml) requires Apple Silicon macOS,
Intel macOS, Linux and Windows runner builds. A `v*` tag publishes a release;
manual dispatch rehearses packaging without publishing.

- macOS: a zip containing only `mtty.app`.
- Linux: tar, DEB and AppImage, containing `mtty` and `mtty-cli`.
- Windows: zip and MSI, containing `mtty.exe` and `mtty-cli.exe`.

Apple Developer ID signing/notarization, Windows MSI signing and minisign
artifact signatures use the optional secrets described in [RELEASE.md](RELEASE.md).
`dist-workspace.toml` remains a cargo-dist scaffold, not the active release pipeline.


## Configuration and links

Configuration is `~/.config/mtty/config.toml`, or
`$XDG_CONFIG_HOME/mtty/config.toml`; on Windows `%APPDATA%\mtty\config.toml`
(saved state goes to `%LOCALAPPDATA%\mtty`; a `~/.config/mtty` that already
exists under a `HOME` set by Git Bash keeps being used). Ghostty/Alacritty config import and the
zsh ZDOTDIR integration retain their existing behavior. Existing native and
eframe session formats are read through the compatibility migration.

macOS and Linux register `mtty://`, `ssh://` and `x-man-page://`.
Windows MSI registers only `mtty://`. Subsequent launches forward to the
running instance through the existing control socket/inbox. The `mtty://`
identity remains unchanged.
