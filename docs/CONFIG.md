# Configuration

[简体中文](CONFIG.zh-CN.md)

Every key in mtty's configuration is optional. A file with a single line is a
valid configuration, and an absent file is the same as an empty one.

## Where the file lives

| Platform | Path |
|---|---|
| Linux / macOS | `~/.config/mtty/config.toml`, or `$XDG_CONFIG_HOME/mtty/config.toml` |
| Windows | `%APPDATA%\mtty\config.toml` |

Saved state (sessions, the queue, window geometry) goes in the same directory;
on Windows that is `%LOCALAPPDATA%\mtty`. Other files named `~/.config/mtty/...`
elsewhere in the documentation live in this directory too.

Up to v0.0.5 the application was called `miaotty`. On first start,
`$XDG_CONFIG_HOME/miaotty` is copied to `$XDG_CONFIG_HOME/mtty` when the latter
does not exist, and the old directory is kept so an older build still works.
See [identity and migration](APP-IDENTITY.md).

## A minimal configuration

```toml
font-size   = 13                 # default 13
font-family = "JetBrains Mono"   # default; falls back to the system monospace
theme       = "nord"             # nord | dracula | gruvbox | mtty | solarized | tokyo-night

[colors]                          # explicit colors override the named theme
background = "#2e3440"
foreground = "#d8dee9"
palette    = ["#3b4252", "#bf616a", "#a3be8c", "#ebcb8b",
              "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
              "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b",
              "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4"]
```

If no mtty configuration exists, ghostty's `config` and alacritty's
`alacritty.toml` are imported automatically.

## Keys

| Key | Default | What it does |
|---|---|---|
| `font-size` | `13` | Font size in points |
| `font-family` | `JetBrains Mono` | Any installed family; falls back to the system monospace |
| `line-height` | `1.25` | A multiple of the font size |
| `cursor-style` | `block` | `block`, `bar` or `underline` |
| `background-opacity` | `1.0` | `0.1`–`1.0`; below 1 needs a compositing window manager |
| `notifications` | `true` | A system notification when an agent needs attention |
| `prevent-sleep` | `true` | Keep the machine awake while an agent is processing |
| `restore-scrollback` | `true` | Save terminals' contents at quit and show them on relaunch |
| `ssh-auto-reconnect` | `false` | Connect a restored SSH tab on startup instead of waiting for Enter |
| `pty-host` | `true` | Run each shell in a PTY host, so updates, relaunches and crashes do not end what runs in the panes |
| `keep-sessions-on-quit` | `false` | Quitting also keeps programs running for the next launch (tmux-like) |
| `detached-timeout` | `"24h"` | How long a kept program waits for mtty: `90s`, `30m`, `24h`, `7d` or seconds |
| `quick-terminal-hotkey` | — | System-wide Quick Terminal toggle, e.g. `cmd+shift+t` |
| `editor-vim` | `false` | Minimal vim mode in the built-in editor |
| `editor` | — | The command "Edit in Tab" runs, e.g. `code --wait` |
| `mermaid-command` | — | Render ` ```mermaid ` blocks with mermaid-cli; without it a built-in subset is used |
| `graphics` | `true` | Inline terminal graphics (Sixel / Kitty / iTerm2) |
| `remote-listen` | — | Serve the MTP control plane over TCP, e.g. `127.0.0.1:7273` (needs `MTTY_MTP_TOKEN`) |
| `language` | — | UI language, `en` or `zh`; `$LANG` is read as well |
| `update-auto-check` | `true` | Check for updates once on startup; `false` makes no request until you ask |
| `update-pubkey` | — | minisign public key; enables signature checks |
| `update-check-url` | the project's own manifest | Where an update check looks; see below |
| `theme` | — | A built-in named theme, overridden by an explicit `[colors]` |

### Themes

`theme` names a built-in palette, case-insensitively. **Nord** remains the
default.

```toml
theme = "mtty"   # nord | dracula | gruvbox | mtty
```

The presets are Nord, Dracula, Gruvbox and mtty. mtty is a navy
workspace palette; unlike the other three, its surrounding chrome — the window,
cards and sidebars — follows the preset instead of the neutral dark chrome. The
named themes `solarized`/`solarized-dark` and `tokyo-night`/`tokyonight` are
accepted too, and an explicit `[colors]` block overrides any named theme.

### Badges

`[badges]` chooses which agent states show on tabs: the state marker — an empty
ring when idle, a rotating arc while executing, a ring with a solid core when
waiting for you, and a solid disc when a turn finished (green) or failed (red)
— plus the `!` or finished mark. A state switched off shows the plain terminal
icon and no mark. All four are on by default; system notifications are not
affected:

```toml
[badges]
processing = true
idle = true
awaiting = true
error = true
```

### Language servers

The editor pane starts a language server for any of `rust-analyzer`,
`typescript-language-server`, `pyright-langserver`, `gopls` and `clangd` that is
on the login shell's `PATH`; files over 2 MB get none. Each entry takes a
command as a string split at spaces, or as a list, plus the markers that
identify a workspace root.

```toml
# [lsp]
# enabled = false                  # all of them off
# [lsp.rust]                       # rust | typescript | python | go | c
# command = "rust-analyzer"
# root-markers = ["Cargo.toml"]    # the nearest folder with one is the workspace
# [lsp.python]
# command = ["pylsp"]
# [lsp.go]
# enabled = false
```

Hovering over code shows its type, documentation and problems.

### ACP agents

Every `[acp]` entry can be started from the command palette's "ACP Agent…" and
drives a transcript window. `command` is a string split at spaces, or a list.

```toml
# [acp]
# [[acp.agent]]
# name = "codex"
# command = "codex acp"
# [[acp.agent]]
# name = "gemini"
# command = ["gemini", "--experimental-acp"]
```


Optional `env` values are passed to the agent process. `auth-method` selects an
ID advertised by that agent; the ACP window also offers an authentication
picker. `session-id` loads an existing conversation when the agent advertises
`loadSession`; an unsupported resume shows an error. The start dialog can
supply a session ID too.

```toml
# env = { EXAMPLE_SETTING = "value" }
# auth-method = "<agent-auth-method-id>"
# session-id = "<agent-session-id>"
```

ACP file reads include unsaved editor text. Writes open a proposal in the editor:
**Accept and Save** writes the file before acknowledging the agent; **Reject**
leaves the disk unchanged. Terminal commands require permission and their output
appears in the ACP window.

### Update checks

**One check on startup, then only when you ask.** mtty runs a single silent
check on startup — set `update-auto-check = false` to skip it and make no
request at all; a newer version is then shown in the status line. The menu's
*Check for Updates* and the retry button in the update dialog after a failure
check on demand. A check is a single plain `curl`:

```sh
curl -fsSL --max-time 8 <update-check-url>
```

`update-check-url` already points at the project's own release manifest, so
this key is only for pointing it somewhere else — a mirror, or an internal
host:

```toml
# update-check-url = "https://example.com/mtty/latest.json"
#   JSON manifest: {"version":"0.2.0","artifacts":{"macos-aarch64":{"url":"…","sha256":"…"}}}
#   A plain document whose first line is the version also works.
```

A downloaded artifact is verified against the `sha256` the manifest declares.
Set `update-pubkey` to a minisign public key to require a signature as well:

```toml
# update-pubkey = "RW…"
```

## Shell integration

A new pane's shell reports its working directory (OSC 7), where each command's
output starts and ends with its exit code (OSC 133) and its history, with no
manual setup. Shims are written to a private, user-only directory and load the
user's own startup files first:

| Shell | How the shim is loaded |
|---|---|
| zsh | a `ZDOTDIR` whose `.zshenv` restores the real `ZDOTDIR` |
| bash | `--rcfile`, which sources `~/.bashrc`; `PS0` on bash 4.4+, a DEBUG trap on older bash (macOS 3.2) |
| fish | a `vendor_conf.d` script found through `XDG_DATA_DIRS`, which it restores |
| PowerShell | `-NoExit -Command` after the profile; wraps `prompt` and PSReadLine (history needs PowerShell 7) |

Your own startup files are never modified. Each shim is tested end to end in a
real PTY (zsh, bash 3.2/5.x, fish 3.7, PowerShell 7.5 on Linux; Windows
PowerShell runs in CI).

## View rules

Pane titles, icons and badges come from rules in `views.json` in the same
directory. See [view rules](VIEW-RULES.md).

## Full reference

[`config.example.toml`](config.example.toml) is the annotated reference: every
key above, with its default, in one file.
