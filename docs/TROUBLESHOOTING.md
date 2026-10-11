# Troubleshooting

[简体中文](TROUBLESHOOTING.zh-CN.md)

## Investigating high CPU or memory usage

mtty records process resource samples by default, at startup and every 30 seconds,
without blocking the UI. Set `MTTY_MONITOR=0` before launching to disable recording.
New builds must be launched before they can record; older installations do not gain
this feature automatically.

| Platform | Recorder directory |
|---|---|
| macOS | `~/Library/Logs/mtty/monitor/` |
| Linux | `$XDG_STATE_HOME/mtty/monitor/`, or `~/.local/state/mtty/monitor/` |
| Windows | `%LOCALAPPDATA%\\mtty\\logs\\monitor\\` |

Each run writes `process-<timestamp>-<pid>.jsonl`. Records include version,
platform, uptime, RSS bytes, cumulative process CPU microseconds, interval CPU
percentage (100% = one logical core), and cumulative render calls and wall time.
Render counters cover the main and picture-in-picture windows, including
render calls that return early; they are not GPU execution times. Thread counts
are available on macOS/Linux, and file-descriptor counts on Linux. Unsupported
or failed probes are `null`, not zero. These samples describe mtty itself,
not its PTY hosts or shell/agent child processes. RSS is not the same metric as
macOS Activity Monitor's memory footprint.

Recorder schema 2 additionally reports `mainPresentedFrames` and
`pipPresentedFrames`: cumulative frames submitted to each surface after a
successful acquisition. Divide their consecutive differences by elapsed seconds
to measure presentation rate. `renderCalls` still counts attempts, including
deferred or hidden redraws, so it must not be interpreted as FPS. Schema 1 also
counted the early-return PiP call after every main-window render, even without a
PiP window. `renderWallUs` includes CPU preparation and presentation waits, not
GPU busy time; vsync can increase that wait while reducing GPU submissions.

Compare consecutive records: rising CPU time with unchanged render counters
points toward non-render work; rising render counts during an otherwise idle
period points toward unwanted refreshes. Observe memory across repeated similar
tasks and idle periods rather than treating one high sample as a leak.

Files rotate before exceeding 4 MiB with one `.previous.jsonl` backup. The latest
16 exited-process logs and their backups are retained, subject to a **64 MiB total
resource-log budget**. Startup removes excess historical runs; each write also
removes older backups or exited-process logs when space is needed. Active-process
primary logs are preserved: if these fill the budget, new samples are dropped
rather than letting disk use grow. A cross-process lease serializes quota checks
and writes. Sampling failures are best-effort and do not terminate the application.
The adjacent `panic.log` is separately limited to **1 MiB plus one backup**, with
**2 MiB total**, including cleanup of oversized legacy crash logs at startup. No terminal contents, command arguments,
or working directories are included. For Rust panics, consult the adjacent
`panic.log`; the resource recorder preserves the preceding samples.

## The build fails, or the first build takes minutes

| Requirement | Detail |
|---|---|
| Rust | The **stable** toolchain, pinned in [`rust-toolchain.toml`](../rust-toolchain.toml); MSRV 1.80 |
| GPU | A driver supporting Metal (macOS), Vulkan (Linux) or DX12 (Windows) |
| Linux | The usual `winit`/`wgpu` system libraries (X11 or Wayland development packages) |

The first build compiles `wgpu` and `glyphon` and may take a few minutes. Later
builds are incremental.

## macOS says the app is from an unidentified developer

`scripts/package-macos.sh` produces an **ad-hoc signed** bundle. Release
packages carry a Developer ID signature and notarization only when the optional
signing secrets are configured in the release workflow, and Windows MSI signing
is in the same position. Until then, macOS asks for confirmation the first time
the app is opened; the standard path is System Settings → Privacy & Security →
"Open Anyway".

You can check which build you have:

```sh
mtty --version     # prints "mtty <version> (native)"
```

## macOS asks for folder access again after every rebuild

The terminal inherits macOS's folder protections: when a program in a pane (or
the shell itself) reads `~/Documents`, `~/Desktop` or `~/Downloads`, macOS asks
the app for permission and remembers the answer **for that app identity**. A
development bundle from `scripts/package-macos.sh` used to be ad-hoc signed,
which produces a new identity on every build, so the stored answer never matched
again and macOS asked after each rebuild.

The script now signs with a stable identity when one is present
(`MTTY_CODESIGN_IDENTITY`, else the first `Apple Development` identity reported
by `security find-identity -p codesigning`). Reinstall a bundle built that way,
grant the permission once, and later builds keep it. If the prompt is about the
whole home directory, or you would rather not be asked at all, give the app
**Full Disk Access** in System Settings → Privacy & Security.

## My configuration is not being read

Configuration is one file:

| Platform | Path |
|---|---|
| Linux / macOS | `~/.config/mtty/config.toml`, or `$XDG_CONFIG_HOME/mtty/config.toml` |
| Windows | `%APPDATA%\mtty\config.toml` |

Two specific traps:

- **Windows under Git Bash.** A `~/.config/mtty` that already exists under the
  `HOME` Git Bash sets keeps being used, rather than `%APPDATA%`.
- **Coming from miaotty.** On first start, `$XDG_CONFIG_HOME/miaotty` is copied
  to `$XDG_CONFIG_HOME/mtty` when the latter does not exist. The old directory
  is kept, so editing it changes nothing. Move your edits to the new path.

If no mtty configuration exists at all, ghostty's `config` and alacritty's
`alacritty.toml` are imported automatically — so a setting may be coming from
there. See [configuration](CONFIG.md).

## A pane does not report its directory or history

That is the shell shim, which loads your own startup files first and never
modifies them:

| Shell | How the shim is loaded |
|---|---|
| zsh | a `ZDOTDIR` whose `.zshenv` restores the real `ZDOTDIR` |
| bash | `--rcfile` sourcing `~/.bashrc`; `PS0` on bash 4.4+, a DEBUG trap on older bash (macOS 3.2) |
| fish | a `vendor_conf.d` script reached through `XDG_DATA_DIRS` |
| PowerShell | `-NoExit -Command` after the profile; PSReadLine history needs PowerShell 7 |

A startup file that resets `ZDOTDIR`, `XDG_DATA_DIRS` or `PS0` after the shim
has run will break the report. The shims are tested end to end in a real PTY
for zsh, bash 3.2/5.x, fish 3.7 and PowerShell 7.5 on Linux, and for Windows
PowerShell in CI.

## Restoring a session does not bring back what was running

Programs keep running through an update, *Relaunch, Keeping Programs
Running* and a crash: each shell runs in a PTY host (`pty-host`, on by
default) and the relaunched mtty reattaches it. An ordinary quit ends them
unless you set:

```toml
keep-sessions-on-quit = true
```

Panes opened while `pty-host = false`, and SSH, serial, Telnet and TCP tabs,
start afresh: their layout and contents come back, the processes do not.

## `mtty-cli` cannot connect

| Check | Detail |
|---|---|
| Socket | `$XDG_RUNTIME_DIR/mtty.sock`, falling back to `$TMPDIR`; pass `--socket PATH` or set `MTTY_SOCKET` to override |
| Older clients | `miaotty.sock` is linked to `mtty.sock`, and `mtty-cli` falls back to an older host's socket or pipe |
| Remote TCP | `remote-listen` refuses to start without `MTTY_MTP_TOKEN`, and every request must then carry it |
| Capabilities | `MTTY_MTP_ALLOW` rejects anything outside the allowlist with `forbidden` |

`mtty-cli ping` reports the capabilities the host actually allows, in
`allowed`. See [the control plane](CLI.md).

## Launching the app again does nothing

That is deliberate: a second launch forwards to the running instance through
the existing control socket, rather than starting a second process. Use
`⌘⇧T` / `Ctrl+Shift+Alt+T` (Quick Terminal) or a new tab for another terminal.

## Links, and upgrading from miaotty

| What | Behaviour |
|---|---|
| URL schemes | macOS and Linux register `mtty://`, `ssh://` and `x-man-page://`; the Windows MSI registers only `mtty://` |
| `mtty://` vs `miaotty://` | Both open mtty |
| Agent hooks | Installed hook scripts and miao's integration read `MIAOTTY_PANE_ID` / `MIAOTTY_CLI`, which are still exported, so they keep reporting state; newly installed hooks use the `MTTY_*` names |
| Notifications on macOS | The rename changed the bundle ID, so macOS asks for notification permission again |

See [identity and migration](APP-IDENTITY.md) for the full table.

## The editor has no completions, diagnostics or hover

The editor pane starts a language server for `rust-analyzer`,
`typescript-language-server`, `pyright-langserver`, `gopls` or `clangd` when it
is on the **login shell's** `PATH`. Two things to check:

- The server is not installed, or is not on the login shell's `PATH` — the app
  does not read a shell's interactive aliases.
- The file is over 2 MB, which gets no language server.

An explicit `[lsp] enabled = false` disables the lot. See
[configuration](CONFIG.md).

## Nothing here matches what I see

The rest of the documentation is in the repository: [installation](INSTALL.md),
[view rules](VIEW-RULES.md), and the annotated
[`config.example.toml`](config.example.toml). For anything else, open an issue
on [oxdingzg/mtty](https://github.com/oxdingzg/mtty/issues), or write
to <contact@mtty.dev>.
