# One application: mtty

[简体中文](APP-IDENTITY.zh-CN.md)

## Name and implementation

The application is **mtty** (site: `mtty.dev`), the command is `mtty`, and the
companion CLI is `mtty-cli`. The repository and the embeddable engine are both
**mtty**. Up to v0.0.5 the application was called **miaotty**; the rename
and its compatibility rules are [ADR 0032](decisions/0032-rename-mtty.md).

`mtty-app/src/main.rs` launches the native `mtty-widget` library; the
former eframe application and the separate `miaotty-native` binary are retired
(this superseded the dual-host phase of ADRs 0030/0031). `mtty --version`
prints `mtty <version> (native)` so checks verify the actual implementation
instead of inferring it from the filename.

Every release contains one GUI application and its CLI:

- macOS: one `mtty.app`, executable `mtty`, bundle ID `dev.mtty.terminal`;
- Linux: `mtty` and `mtty-cli` in tar/DEB/AppImage (the DEB also installs
  `miaotty` / `miaotty-cli` aliases and replaces the `miaotty` package);
- Windows: `mtty.exe` and `mtty-cli.exe` in zip/MSI (the MSI upgrades an
  installed miaotty in place).

The four-runner release workflow remains the release gate; a local macOS build
does not replace the Linux/Windows gates.

## Upgrading from miaotty

| What | Behaviour |
|---|---|
| Config directory | On first start, `$XDG_CONFIG_HOME/miaotty` is copied to `$XDG_CONFIG_HOME/mtty` when the latter does not exist. The old directory is kept, so an older build still works. |
| Environment | `MTTY_*` is read first, then `MIAOTTY_*`. Panes export both sets, and `MTTY_CLI` / `MIAOTTY_CLI` point at the bundled CLI. |
| Agent hooks, miao | Installed hook scripts and miao's integration read `MIAOTTY_PANE_ID` / `MIAOTTY_CLI`; both are still exported, so they keep reporting state without changes. Newly installed hooks use the `MTTY_*` names. |
| CLI and socket | The socket is `mtty.sock`; `miaotty.sock` is linked to it for older clients. `mtty-cli` falls back to an older host's socket or pipe. |
| Links | `mtty://` and `miaotty://` both open mtty. |
| macOS | `scripts/install-macos.sh` archives `miaotty.app` (by bundle identity) and installs `mtty.app`. The new bundle ID means macOS asks for notification permission again. |

## Saved state

`session.json`, `queue.json` and `window` live in the config directory. Reads
use this priority:

1. canonical state in the config directory;
2. former native `native-session.json`, `native-queue.json`, `native-window`;
3. if no native session exists, the retired eframe app's
   `$XDG_DATA_HOME/miaotty/session.json` (default `~/.local/share/miaotty`).

Legacy files remain intact. The indexed eframe layout and pane focus are
converted to native IDs, including split directions/ratios, cwd, tab title,
mark, group, active tab and recent files; recipes use the same
conversion. Restoring brings back layouts and starts new shells; it does not
preserve running processes.

## Build, install and verify

```sh
cargo build --release -p mtty-app -p mtty-cli
scripts/package-macos.sh
python3 scripts/check-macos-bundle.py dist/mtty.app
python3 scripts/smoke-hosts.py --bundle dist/mtty.app
bash scripts/install-macos.sh
```

The desktop smoke checks the **packaged** executable and bundled CLI: copying a
former miaotty config directory, split-session migration, pane focus/close,
shell commands, both pane environment sets, a pre-rename hook script, the former
socket path, file read/write, history, agent state, view/edit dispatch and a
fresh GPU screenshot. `MTTY_SHOT_AFTER=<seconds>` writes `/tmp/mtty_shot.ppm`
and exits.

The two-host audit in `UI-AUDIT.md` is historical evidence. The native app keeps
its version-check UI; the removed eframe app's automatic download/install UI is
not a feature of this application.
