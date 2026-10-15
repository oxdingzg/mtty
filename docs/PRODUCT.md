# mtty product requirements and roadmap

[简体中文](PRODUCT.zh-CN.md)

> Baseline: `v0.1.3` plus the acceptance fixes recorded in [ACCEPTANCE.md](ACCEPTANCE.md), 2026-10-04.
> This is the single source for feature requirements. The README describes what
> works today; this document also records what comes next, in which order, and
> what counts as done.

## 1. Positioning

**mtty** is a cross-platform (macOS / Linux / Windows) **AI-native terminal and
editor for local and remote work**: terminals, files, remote hosts and AI coding
agents share one fast, native window and the same tabs and splits. It stands on
three pillars — terminal and remote, editor, agent workspace — tied together by
Rust and GPU rendering under a performance gate, and by no account or forced
cloud: mtty hosts the agents you choose and never calls a model itself.

| Area | User value |
|---|---|
| Terminal and remote | Local shells and saved hosts share tabs and splits; SSH, transfers, port forwarding, serial, Telnet and TCP are accessible from one workspace |
| Editor | Open code next to its shell, use syntax highlighting and language servers, and work with large files, multiple cursors and live previews |
| Agent workspace | See when an agent needs input, prepare follow-up prompts, send context and review proposed edits |
| miao integration | Built-in state reporting, session resume and MTP control without extra hooks |

In one line: **the terminal must be fast and dependable, the editor good enough
to stay in, agents visible, steerable and reviewable, and remote hosts as easy
as local folders.**

### 1.1 Naming

- Official product name: **mtty**. The repository and the embeddable engine are
  **mtty**.
- Binaries `mtty` / `mtty-cli`, bundle `mtty.app` (`dev.mtty.terminal`),
  config directory `~/.config/mtty`, environment `MTTY_*`, URL scheme
  `mtty://`. Up to v0.0.5 the name was `miaotty`; compatibility rules are in
  [ADR 0032](decisions/0032-rename-mtty.md).

### 1.2 Users and core scenarios

1. **Developers' local terminal**: many projects, tabs and splits; CJK paths and
   output display and search correctly.
2. **AI coding agent users**: run codex / claude / opencode / miao side by side,
   see who is busy, who is waiting for me, who failed, and queue prompts for them.
3. **Ops / full-stack**: manage dozens of hosts — log in over SSH, move files,
   forward ports, run common commands in bulk — with credentials kept on this
   machine; reach devices over serial and Telnet too (M6).
4. **Editing code and text**: open any file next to its terminal, fast even when
   it is huge, highlighted, and with language-server help (M5); review what an
   agent changed before keeping it (M7).

## 2. Interaction design principles

These are part of the acceptance criteria; feature reviews check each one.

1. **Terminal first**: the terminal area is always the main surface. Panels and
   dialogs must not steal terminal input, and no key may act in two places at
   once (with a text field focused, ⌘C/⌘V/⌘A act on the field).
2. **Keyboard first, one entry point**: every feature is reachable from the
   command palette (`⌘K`); frequent ones have shortcuts that agree across the
   menu, the palette and the docs.
3. **Failures are visible**: any user-initiated action that fails (save, open,
   connect, write config) shows why in the UI — never only on stderr, never
   pretending to succeed.
4. **No data loss**: closing unsaved changes asks first; a failed save stays
   modified; writing config never overwrites a file that fails to parse.
5. **The user's config is theirs**: no edits to shell dotfiles, agent config or
   `~/.ssh/config`; when wiring is needed, show a snippet to paste.
6. **Zero remote install**: SSH features require nothing installed remotely.
7. **Credentials stay local**: prefer ssh-agent and the system keychain; never
   store plaintext passwords; any cloud sync is end-to-end encrypted and fully
   optional.
8. **The UI thread never blocks**: network, external processes and large
   directory reads run in the background with visible, cancellable progress.
9. **Bilingual**: UI and docs in English and Simplified Chinese.

## 3. Feature baseline (current version)

Status meanings:

- **Works**: reachable by the user, complete, with automated tests or
  real-window smoke evidence.
- **Partial**: usable with known gaps (listed in notes and scheduled in M0).
- **Not available**: previously claimed or commonly expected, but absent from
  the native app; removed from the README and scheduled on the roadmap.

Evidence: `cargo test --workspace` (183 tests), `cargo clippy -D warnings`,
`scripts/smoke-hosts.py` (real window + real shell + GPU capture), and a
feature-by-feature review of the native host code for this baseline.
Re-checked against the code and updated on 2026-10-02 (282 tests at that
point); problems fixed in M0–M3 are no longer listed.

### 3.1 Terminal core

| Feature | Status | Notes |
|---|---|---|
| PTY + VT parsing (alacritty_terminal), GPU glyph grid | Works | See PERFORMANCE for the gate |
| Scrollback, selection, copy/paste, CJK wide characters and font fallback | Works | Copy keeps wide characters whole at either selection edge; drag and double-click agree |
| Find `⌘F` (highlights + count) | Works | Fixed now: CJK queries never matched; highlights were misplaced on CJK lines |
| Key encoding: Ctrl/Alt/modified navigation, F1–F12, Insert | Works | Fixed now: F1–F12 and Insert were not sent at all |
| kitty keyboard protocol | Works | All five flags: disambiguate escape codes (1), event types incl. release (2), alternate keys (4), all keys as escape codes (8) and associated text (16) |
| IME inline preedit | Works | Candidate window placed at the cursor (`set_ime_cursor_area`); its position on macOS awaits a desktop check |
| Inline images: Sixel / Kitty / iTerm2 | Works | Kitty z-index honoured (negative draws behind the text grid); restored sessions bring images back anchored to their scrollback rows, and omit them rather than misplace them after a reflow |
| Mouse reporting (SGR / X10) | Works | |

### 3.2 Window and workspace

| Feature | Status | Notes |
|---|---|---|
| Session sidebar (full height) and tab bar (sidebar hidden): drag reorder, `+`, `×`, row menu (rename/prefix/mark/group/duplicate/move/close others/close below) | Works | Duplicate keeps mark and group; Close Others/Below can be reopened |
| Split tree `⌘D` / `⇧⌘D`, divider drag, per-pane close | Works | Local splits/tabs inherit the current directory. SSH splits reuse the saved connection command; plain SSH sessions inherit reported remote directories when the bootstrap fits a PTY line. tmux/mosh keep their own session semantics. The pane context menu also splits Right/Left/Down/Up |
| Pane right-click context menu | Works | Copy, Paste, Copy/Paste as (ANSI, shell-escaped), Composer, Send to Agent, Select All, Search, About This Line, Split Pane (Right/Left/Down/Up), Clear Scrollback; reuses the palette commands |
| Quick Terminal `⌘⇧T`, reopen closed tab `⌘⇧Z` | Works | Reopen restores the directory only |
| Global Quick Terminal hotkey | Works | Fixed now: one press did two things (switch to Quick, then hide the window) |
| Command palette `⌘K` | Works | Attention Inbox groups pending agent input, failures and results to review by pane; Next Pending Task cycles in urgency order. Open the terminal, send its last output to Composer, open Agent Tasks, or dismiss an item. Repeated reports do not duplicate items; resumed work and closed panes remove stale items. Completion is not a verification verdict |
| Open Quickly `⌘⇧O` | Works | Tabs, agents, snippets, hosts, files and folders in the current directory, recent files, and text inside files under that directory or in the active pane's scrollback (bounded scan, off the UI thread) |
| Details panel: Info / Agent / Outline / Git / Files / Ports / Queue | Works | Ports cover the shell and its children; non-git dirs say so; while hidden only git is checked, every 10 s (the status line shows the branch) |
| Session restore (layout, directories, titles, groups, terminal contents) | Works | Each terminal's last 5,000 rows and their inline images come back (colours kept, owner-only files, `restore-scrollback = false` turns it off; images return only when a pane comes back at the same width, so a reflowed pane drops them rather than misplacing them), and the program that was running is offered on Enter, never rerun unasked. Quit from the Dock, logout or AppleScript now saves too. Running programs survive updates, relaunches and crashes through per-pane PTY hosts (ADR 0041, `pty-host`, on by default); an ordinary quit ends them unless `keep-sessions-on-quit` is set |
| Recipes save/open | Works | Names are validated as file names; write failures are shown |
| Picture-in-picture, hints, read-only mode | Works | Read-only blocks MTP `pane.send/run` |
| View rules | Works | Alias, title, icon, badge; matching by path/command/agent/host/file (the file the foreground program opened); `views.json` hot reload |
| macOS menu bar | Works | Fixed now: menu Copy/Paste/Select All acted on the terminal while a text field had focus |
| Settings window `⌘,` | Works | Fixed now: changes were never saved, the theme name was forced to Nord, and a broken config fell back silently |

### 3.3 Reader and editor

| Feature | Status | Notes |
|---|---|---|
| Read-only view, edit, line numbers, syntax colouring, minimal vim mode | Works | Fixed now: failed saves were swallowed and shown as saved; closing did not confirm unsaved changes |
| Markdown + Mermaid subset | Works | Relative images resolve against the document's folder; table cells wrap; external `mermaid-command` runs in the background |
| Edit in Tab | Works | Local files; runs the `editor` setting, else `$EDITOR`, else `vi` (Notepad on Windows); disabled while there are unsaved changes |
| Jump-to-line highlight, Open Externally | Works | Editor pane line navigation and command-palette external opener |

### 3.4 Agents

| Feature | Status | Notes |
|---|---|---|
| Detect claude / codex / opencode / miao, install the state hook, show the wiring snippet | Works | The snippet has a Copy button |
| Badges, needs-attention marks, system notifications, sleep guard | Works | Fixed now: the sleep inhibitor could outlive the app |
| Composer `⌘⇧E`, prompt queue (sent by hand) | Works | |
| Automatic queue delivery when an agent turns idle | Works | Queued prompts are bound to a pane; items saved by older versions without one are sent by hand |
| Launch an agent from the UI | Works | Settings and the command palette |
| Show the session id the hook reports | Works | The Agent tab shows agent, state, session id and tty |
| Resume, quota display | Works | [ADR 0042](decisions/0042-agent-resume-quota.md); resume templates are per agent, quota shows only when the agent reports it |

### 3.5 Remote and operations

| Feature | Status | Notes |
|---|---|---|
| New SSH session (`~/.ssh/config`, ControlMaster reuse, zero-install terminfo) | Works | No `ssh -G` on the UI thread; a restored SSH tab says it is disconnected and reconnects on Enter, or connects on startup with `ssh-auto-reconnect = true` (off by default) |
| Remote file view/edit over ssh | Works | Host taken from the active SSH tab (typed otherwise); reads and writes run in the background and failures are shown |
| Host library, groups, key management, SFTP, FTP, port forwarding, snippets | Works | See M3; real mosh SSH bootstrap and UDP verified in ACCEPTANCE |

### 3.6 System integration and automation

| Feature | Status | Notes |
|---|---|---|
| Config file + ghostty / alacritty import | Works | Fixed now: a `config.toml` with syntax errors is reported with the reason |
| Shell integration (OSC 7, OSC 133, history) | Works | zsh, bash, fish, PowerShell |
| URL schemes and single-instance forwarding | Works | From the command line and from a macOS browser/Finder |
| MTP control plane and `mtty-cli` | Works | Covered by the real-window smoke |
| Version check and update | Works | SHA-256 and a required minisign signature are checked in process; macOS install-and-relaunch checked end to end; AppImage signed update verified in a real Linux window; Windows desktop checks are recorded in ACCEPTANCE |
| English / Chinese UI | Works | A few sample/agent strings stay English on purpose |

## 4. Roadmap and execution batches

Order: **M1 → M0 → M2 → M3 → M4**. M1 (the rename) moved first by the
2026-10-01 decision: the later it happens, the more user state and external
integrations need migrating. Each batch is a unit that is committed and
verified on its own:

- Every batch must pass `cargo fmt --check`, `cargo clippy -D warnings`,
  `cargo test --workspace`, `scripts/smoke-hosts.py` (when UI or packaging is
  touched), and the acceptance listed for it.
- One (or a few) commits per batch, pushed right away; tick the box here and
  cite the evidence.
- Steps that need credentials, real hardware or the owner's confirmation
  (signing, notarization, publishing, production hosts) are marked **[owner]**
  and are not run automatically inside a batch.

### M1 One brand: mtty (design: [ADR 0032](decisions/0032-rename-mtty.md))

- [x] **B1.1 Runtime rename and compatibility layer** — `ee12822`: unit tests for migration, environment, socket link and schemes; the smoke covers the config copy, both pane environment sets, a pre-rename hook and the former socket path.
  - `mtty-config`: `config_dir()`, `migrate_legacy_config()` (copy once,
    never overwrite, never delete the old directory), `env()` (`MTTY_*` first,
    then `MIAOTTY_*`).
  - Every path goes through `config_dir()`: config, views, hooks, launch inbox,
    session/queue/window/recipes.
  - MTP: default socket `mtty.sock` plus a `miaotty.sock` compatibility link;
    Windows pipe `mtty`; token/capability variables read under both names.
  - Pane environment exports `MTTY_*` and `MIAOTTY_*`; `MTTY_CLI`/`MIAOTTY_CLI`
    hold the absolute path of the sibling CLI.
  - Both `mtty://` and `miaotty://` are accepted; hook scripts, the shell shim,
    hotkey snippets, ControlPath and UI text say mtty.
  - Acceptance: unit tests for migration, environment and schemes; an old hook
    script reports state from a new pane (smoke).
- [x] **B1.2 Build and packaging** — `ee12822`: `package-macos.sh` + `check-macos-bundle.py` + `smoke-hosts.py --bundle` pass; `test-release-manifest.py` passes 9 tests; the release-workflow rehearsal (run 36830274914) passes on all four runners, including MSI install/uninstall with both schemes and the deb aliases.
  - Dirs and packages: `miaotty-app` → `mtty-app` (binary `mtty`),
    `miaotty-cli` → `mtty-cli`.
  - macOS: `mtty.app`, `dev.mtty.terminal`, URL scheme registration; the
    installer archives and removes the old `miaotty.app`.
  - Linux: deb package `mtty` (Replaces/Conflicts/Provides `miaotty`),
    compatibility symlinks, `.desktop`, AppImage.
  - Windows: MSI product and folder `mtty`, same UpgradeCode, `mtty://` and
    `miaotty://` registered.
  - `release.yml`, `ci.yml`, the update-manifest script and its tests,
    smoke/profiling scripts, `windows-verify.ps1`.
  - Acceptance: `scripts/package-macos.sh` + `check-macos-bundle.py` +
    `smoke-hosts.py --bundle` pass; `test-release-manifest.py` passes; a manual
    release-workflow rehearsal **[owner]**.
- [x] **B1.3 Docs and website** (checked 2026-10-02: in the repository `miaotty` appears only in compatibility notes and history, mtty.dev's main branch says mtty, `check-privacy.sh` passes)
  - READMEs, `docs/*`, AGENTS.md and the example config say mtty; APP-IDENTITY
    becomes the migration guide; historical ADRs stay as written.
  - The mtty.dev site's install, config and CLI examples say mtty (separate
    repository, separate commit).
  - Acceptance: `check-privacy.sh` passes; `miaotty` remains only in
    compatibility notes and historical records.

### M0 Stabilize: turn "Partial" into "Works"

- [x] **B0.1 Nothing blocks the UI thread** (also fixed: SSH sessions connect with the typed alias; options under `Host <alias>` were not applied before): `ssh -G`, remote read/write,
  `mermaid-command` and Files directory reads become background tasks with a
  loading state; the details panel polls only while visible; Ports includes
  child processes; non-git directories are reported as such. Acceptance:
  background-task unit tests; no frame stalls in the smoke while opening a
  remote file or a large directory (timed in the log).
- [x] **B0.2 Workspace correctness** (read-only makes MTP `pane.send/run` fail with `read_only`; a URL that opens a new tab to run a command is an explicit user action and is not blocked; a new split's cwd is unit-tested, as MTP has no split method): new splits/tabs inherit the directory;
  Duplicate keeps mark and group; Close Others/Below are reopenable; read-only
  blocks MTP/URL input; recipe names validated and read/write failures shown;
  GPU init failure shows an error instead of panicking. Acceptance: unit tests
  for the pure parts plus a smoke check of a new split's cwd.
- [x] **B0.3 System integration** (`0417c1b` and after; `open -a dist/mtty.app mtty://quick` verified on a cold start and on the running app; also: duplicating an ssh tab connects again; IME candidate placement awaits a desktop check): macOS URL Apple Events (links opened from a
  browser/Finder); restored SSH tabs show "disconnected — press Enter to
  reconnect" and reconnect; the IME candidate window follows the cursor.
  Acceptance: `open mtty://quick` works on the installed app; reconnect has unit
  tests; IME needs a manual desktop check **[owner]**.
- [x] **B0.4 View rules and entry points** (`d52d898` and after; icons, badges, command matching and hot reload checked in real-window captures; also fixed: Rename Tab had no effect, deep-link Focus did not select the pane, symlinked folders did not match): rule icons/badges applied,
  command/host matching, `views.json` hot reload; Open Quickly lists files and
  agents; the remaining English strings in the details panel are translated.
  Acceptance: rule-engine unit tests and a screenshot check.
- [x] **B0.5 Automatable UI acceptance** (the tab bar exports each tab's rect as a test target; replay tests cover tab drag-reorder, divider drag, rename commit on Enter / cancel on Escape and a paste landing in a focused field; the replay found that Enter did nothing in 8 input fields, now fixed; editor save is unit-tested): semantic control targets plus
  windowless event replay for rename/cancel, tab reorder, divider drag, editor
  save and clipboard focus. Acceptance: the new replay tests run in CI.

### M2 Agent workbench

- [x] **B2.1 Launch and wiring** (the codex snippet follows its `hooks.json` format and needs `[features] hooks = true`; an end-to-end hook-script test covers `--stdin` session parsing, the pane filter and never blocking; the user's real agent configs were not touched): "Launch agent" in settings and the palette
  (codex/claude/opencode/miao, optional directory); a Copy button for wiring
  snippets; a codex hook snippet and verification script. Acceptance: launch
  command unit tests; a fake agent reports state in the smoke.
- [x] **B2.2 Queue** (also fixed: Send now left the prompt queued, so it was delivered again; per-frame state sampling missed a quick processing -> idle, now the control plane records transitions in order; the smoke with back-to-back states passed 8 runs in a row): items target a pane and are delivered when its agent turns
  idle; persisted in `queue.json` (old format still read). Acceptance: unit
  tests for transitions and delivery, including no double delivery on repeated
  events.
- [x] **B2.3 Command boundaries** (output is captured from the byte stream between C and D, so the scrollback cap shifting line numbers does not matter; real zsh end-to-end test; the smoke reads it back over MTP `pane.output`): OSC 133 (A/B/C/D) parsing, emitted by the zsh
  shim; "copy/send the last command's output". Acceptance: parser unit tests;
  the smoke runs a command and retrieves its output.
- [x] **B2.4 Worktree tasks** (4 integration tests on temporary repositories; the tasks window and the new-task dialog checked in real-window captures via `MTTY_QA_COMMAND`): a task = `git worktree add` + branch + agent pane;
  task list, diff view, merge or discard (destructive steps confirm).
  Acceptance: integration tests against a temporary repository.
- [x] **B2.5 Attention and miao** (osascript notifications cannot report clicks, so activating mtty within two minutes of a notification jumps to its pane; the background `!`/✓/• marks were checked in a real-window capture; the miao plugin reads `MTTY_*`, miao commit `f596c592a`): clicking a notification jumps to its pane;
  unread/finished marks on background tabs; the miao plugin reads `MTTY_*`
  (miao repository, separate commit). Acceptance: transition unit tests; miao
  plugin tests.

### M3 Remote operations

- [x] **B3.1 Host library** (end to end against a real Linux host: an imported alias connected through its `~/.ssh/config` ProxyJump, opened via `mtty://host/<name>`, a remote command checked; sidebar HOSTS checked in a capture): `hosts.toml` (name, address, user, port, group,
  tags, jump host), import from `~/.ssh/config`, sidebar host list, palette
  search, double-click to connect. Acceptance: parser/import unit tests; a smoke
  against a local sshd or container **[needs a test host]**.
- [x] **B3.2 Safe connections** (an interactive ssh asks about host keys itself; the host library checks in the background: known / unknown (fingerprints, trust after comparing) / changed (refused); "known" checked against a real host; key generation and ssh-copy-id run in a terminal tab, so mtty never handles a passphrase): known_hosts confirmation on first connect and
  fingerprint change; ssh-agent status and key generation; no plaintext
  passwords.
- [x] **B3.3 Port forwarding**: L/R/D rules stored with the host; each runs as
  an `ssh -N` owned by mtty (`ExitOnForwardFailure`), with visible state, ending
  with mtty (not the shared ControlMaster, whose 60 s ControlPersist would drop
  forwards). End to end against a real host: the remote sshd banner read
  through the local port, the port closed after Stop.
- [x] **B3.4 SFTP** (OpenSSH sftp in batch mode, keeping ssh config, jump hosts, the agent and ControlMaster; real round-trip test; checked on a real Linux Wayland desktop; download progress is read off the local file, uploads show a busy state without a percentage): two-pane browser, upload/download, drag and drop, progress,
  rename/permissions; remote editing reuses the pane's connection.
- [x] **B3.5 Snippets and broadcast** (on several hosts a snippet runs as `ssh -t host 'command'` in a tab each, quoting checked by letting sh split it; broadcast verified by real keyboard input in two Linux panes, with identical finished-command output): a command library run in the current pane
  or on several hosts; broadcast input to several panes.
- [x] **B3.6 FTP/FTPS and persistent sessions**: FTP/FTPS in the same browser
  (plaintext flagged); optional tmux/mosh reconnect. (FTP runs through the
  system curl, the password only in memory and passed on stdin; plain FTP and
  explicit FTPS round-trip against a real server; whole folders upload and
  download recursively, depth-first, since curl cannot walk a tree itself. tmux
  checked over real ssh: after a detach, reconnecting
  returns to the same shell. mosh: a real SSH bootstrap and UDP session verified
  in ACCEPTANCE, so the reconnect path is proven end to end.)
  Acceptance (M3 overall): an automated smoke against a test host; credentials
  never reach logs, session files or MTP responses.

### M4 Cross-platform delivery

- [x] **B4.1** Real Windows/Linux desktop acceptance (IME, hotkey, drag and
  drop, menus) **[owner + hardware]**.
  Linux, 2026-10-01 (Ubuntu 24.04, GNOME 46, Wayland, input injected through
  uinput): typing, menus and menu actions work; pinyin IME preedit, candidate
  placement and commit work; the Quick Terminal hotkey works through a GNOME
  custom shortcut running `mtty --quick` (GNOME 46 has no GlobalShortcuts
  portal). **Found and fixed:** dropping files did nothing on native Wayland
  (winit 0.30 implements drops only for X11), now handled through our own
  `wl_data_device`, which also reads the clipboard because GNOME serves one
  data device per client; app shortcuts used Super, which the desktop owns,
  now Ctrl+Shift based (see the README table); Tab in the terminal moved
  egui's focus to the File menu and swallowed further typing.
  Windows, 2026-10-01 (Windows 11 22H2, input through SendInput in the
  logged-on session): typing, Tab completion, menus, Microsoft Pinyin
  (preedit, candidate placement, commit), file drops, clipboard
  (Ctrl+Shift+C/V) and the Quick Terminal hotkey work. **Found and fixed:**
  mtty exited at start (no CJK font for egui); switching the input language
  hung the window for good (wgpu's OpenGL probe left a thread whose hidden
  window never processes messages — found with a symbolized minidump);
  every ssh command typed into cmd.exe failed (POSIX quoting, `clear;`,
  ControlMaster); config.toml, hosts, snippets and sessions were ignored (no
  `HOME` on Windows; now `%APPDATA%\mtty`); a console window opened with mtty;
  keys held while focusing (the hotkey's Y) were typed into the pane; dropped
  paths used single quotes.
- [ ] **B4.2** Apple notarization and Windows MSI signing **[owner +
  credentials]**.
- [x] **B4.3** Auto-update: download, `update-pubkey` signature check, replace.
  (Release key built in. On the Mac mini a packaged app installed and relaunched
  a test-signed release, and tampered packages were refused by checksum and by
  signature; the published v0.0.5 package verifies against the repository key.
  Linux AppImage signed download/replace/relaunch, bad hashes/signatures and a missing-download fallback were verified on 2026-10-04; Windows results are in ACCEPTANCE.)
- [x] **B4.4** Shell integration for bash / fish / PowerShell. (End-to-end PTY tests
  for zsh, bash 3.2/5.3 on macOS, bash 5.2, fish 3.7 and PowerShell 7.5 on Linux;
  Windows PowerShell runs in CI. Windows PowerShell 5.1 records history too: it
  has no `ProcessStartInfo.ArgumentList`, so the shim quotes the command line
  with `CommandLineToArgvW`'s rules. That path was checked on a real 5.1 host
  (the shim parses, the quoting round-trips through `CommandLineToArgvW`, the
  fallback is selected and spawns); a full in-app capture on 5.1 is still a
  hand check.)
- [x] **B4.5** Optional end-to-end-encrypted sync, off by default. (ADR 0033:
  hosts and snippets through a folder the user already syncs, XChaCha20-Poly1305
  with a key that never enters the folder, one file per device, per-entry merge
  with tombstones. Two app instances converged through a shared folder on the
  Mac mini, with no plaintext in it.)

### M5 Editor pane (design in [ADR 0034](decisions/0034-editor-pane.md))

- [x] **E1** `mtty-editor` core: rope, transactions, undo, selections, search. (`crates/mtty-editor`: 37 unit tests covering transactions and position mapping, grouped undo and the saved state, multi-cursor typing/paste/delete, ⌘D, indent, grapheme and word motions, search and regex replace, BOM/CRLF round trips. Perf gate on a 100 MB file, Mac mini release build: open 62 ms, keystroke in the middle 0.4 µs, undo 1000 keystrokes 0.3 ms, find all 120 ms, save 3.6 ms.)
- [x] **E2** Editor pane MVP: pane kinds, styled spans and clipping, keys, mouse, IME, clipboard, open/save/close, session restore. (Done: local non-Markdown files open in an editor pane; gutter, current line, selections, carets and clipped long lines through the terminal renderer; macOS-style keymap with ⌘D/⇧⌘Z taking precedence in an editor; click, shift-click, double/triple click, ⌥-click caret, drag, wheel; clipboard and Cut; IME commit and candidate placement; atomic save keeping permissions; unsaved-changes guards on close, Close Others/Below and quit; session restore; MTP kinds, send and close. Evidence: 9 unit tests in `editor_pane.rs`, a perf gate of 0.10 ms per screen of a 100 MB file, and a real-window capture on the Mac mini with text inserted over MTP; a desktop hand check covered keyboard typing, IME and ⌘S, and styled spans arrived with E3.)
- [x] **E3** tree-sitter highlighting (80 languages, a Sublime-syntax fallback for more), large-file mode, remote files. (Done: 80 compiled-in tree-sitter grammars with incremental reparse and visible-range queries; files no grammar claims fall back to syntect's Sublime syntaxes plus permissively licensed ones vendored from bat (Julia, nginx, VHDL, Org and others), with licences in `docs/third-party/SYNTAXES.md`; the language shows in the status chip. Evidence: query-compile, file-type and highlight tests for every grammar, a test that every Sublime syntax parses under the pure-Rust regex engine, and a real-window capture of a Julia file. Perf gate on the Mac mini, release build: first parse of 1 MB 64 ms, keystroke reparse 3.5 ms, one screen highlighted 0.09 ms. Large files: above 512 KB the parse runs on a background thread (8 MB: colours after 0.5 s, 0.009 ms per keystroke on the UI thread, reparse settled in 127 ms); tree-sitter stops at 8 MB, as a tree takes 25-35 times the file in memory, and the Sublime fallback at 1 MB. Large-file mode: files over 64 MB open in view mode, read from disk a window of lines at a time with a sparse line index built in the background, so memory stays small at any size; Find scans the file on a thread. Typing, pasting or "Switch to Editing…" offers to load the file for editing, stating the memory it takes (about 2.2 times the file). Mac mini, release build, 1 GB: first screen 1 ms, index 0.3 s, last screen 0.03 ms, full search 0.14 s (warm cache); the packaged app opened a 1 GB log in 0.44 s using about 70 MB more memory. Remote files open in the editor pane too: the bytes are read over ssh on a background job, `⌘S` writes them back the same way, and the pane stays modified if it changed while the write ran. A remote pane polls its file too: about once a second a background thread probes the remote file's length and modification time over ssh, and when the stamp changes while the pane has no unsaved edits it reloads in place through the same path as a local external change; a change that arrives with unsaved edits is kept silently, and a failed probe is a silent no-op rather than a notice. It gets no language server and no view mode, and shows `host:name` in its title; the session stores its destination and path and re-reads the host on restore. Evidence: editor-pane tests for loading bytes, a save that did or did not race an edit, a restored pane filled from bytes, a pure test that the poll decision reloads only an unmodified pane on a new stamp, and a test that a remote reload replaces the text and clears the dirty mark; the ssh path itself is the one checked against a real host in B3.4.)
- [x] **E4** Multi-cursor, find/replace, go to line, Markdown preview pane. (Done: ⇧⌘L selects every occurrence (whole words from a bare caret), ⇧⌥I puts a caret at each selected line's end, ⌥⌘↑/⌥⌘↓ add carets (they used to hit ⌘↑/⌘↓), Ctrl+Alt+↑/↓ elsewhere; Find in an editor gains match case, whole word and regex, highlights every match on screen, Replace (on to the next match), Replace All (one undo step) and Select All Matches (⌥↩); Go to Line (⌃G, `line:column`). In the macOS app the menu bar no longer takes ⌘D, ⇧⌘Z and ⇧⌘L from an editor. Evidence: replay tests that feed keys through the keymap, core tests for occurrences, line ends and single replacements, and real-window captures on the Mac mini; perf gate: 1.5 million carets (every occurrence in 100 MB) selected in 196 ms and drawn in 0.12 ms per screen. Markdown preview pane: local Markdown files open in an editor with a preview to its right that follows typing ("Toggle Markdown Preview" in the palette and View menu opens or closes it); previews close with their editor and are kept in sessions; the floating editor remains for read-only `app.view` and remote files. Evidence: a session test for previews and real-window captures on the Mac mini, one after text was inserted over MTP; a hand check in the packaged app confirmed the View menu and palette toggle the preview and that ⌘D, ⇧⌘Z and ⇧⌘L reach the editor.)
- [x] **E5** LSP: diagnostics, hover, completion, go-to-definition. (`crates/mtty-lsp`: JSON-RPC framing, a client that starts, initializes and reads servers on background threads, UTF-8/UTF-16 positions and file URIs, root detection, snippet text; one server per language group and workspace root, found on the login shell's PATH; whole-text sync, files up to 2 MB. In the editor: diagnostic underlines and ✖/⚠ counts in the status bar, hover after the pointer rests, completion on trigger characters, words and Ctrl+Space with client-side filtering, snippets and import edits, F12 or ⌘-click to the definition, F8 to the next problem; `[lsp]` in `config.toml`. Evidence: 15 unit tests in `mtty-lsp` and acceptance tests against rust-analyzer 1.94 and typescript-language-server 5.3 (diagnostics, hover, definition, completion, a re-check after an edit; rust-analyzer's first diagnostics in 10.4 s cold, TypeScript's in 0.4 s), unit tests for completion ranking and edits, and real-window captures on the Mac mini of underlines, hover, completion and go to definition. Note: rust-analyzer completes std's members only with the `rust-src` component.)
- [x] **E6** vim mode in the pane, folding and outline, external-change reload. (Done. External-change reload: an open pane remembers the file's length and modification time and re-checks it about once a second; with no unsaved edits it reloads in place as one undoable transaction that replaces only the differing span and maps the cursor through it, and leaves the pane clean; with unsaved edits it asks whether to reload from disk or keep the local version; a file deleted underneath the pane is reported once; view-mode panes over 64 MB are skipped. Folding: from the syntax tree (any named node spanning more than one line, outermost per start line) with indentation as the fallback; the gutter shows ▸/▾, a collapsed header ends in ⋯, up/down skip hidden lines, ⌥⌘[ / ⌥⌘] fold/unfold, and Fold All / Unfold All / Toggle Fold are in the palette. Outline: definition nodes (functions, classes, structs, enums, traits/interfaces, impls, modules, constants, types, variables, macros) labelled by their `name` field and nested by depth; ⌘R opens a filterable picker. Vim: a `mtty-editor` state machine over the document — NORMAL/INSERT/VISUAL/VISUAL LINE, counts, `h j k l w b e 0 ^ $ gg G`, `i a I A o O`, `x`, `d`/`c`/`y` with motions (`dd`/`cc`/`yy`, `dw`, `d$`), `p`/`P` with an internal register, `u` and Ctrl-r, `J`; `/` opens Find and `:` runs `w`/`q`/`wq`/a line number; the `za` family drives folds; the status bar shows the mode; it is on with `editor-vim`. Evidence: core tests for the tree/indentation folds, outline extraction and the vim commands; editor-pane tests for folding, vim normal/insert and external reload; `cargo test`/clippy on the Mac mini. A hand check on the packaged Linux build (Xvfb) exercised vim NORMAL/INSERT/VISUAL and Fold All, which collapsed the function bodies and skipped their hidden lines.)

### M6 Remote, connections (R1 in [ADR 0037](decisions/0037-serial-telnet-tcp.md), R2 in [ADR 0038](decisions/0038-ppk-import.md), R3 in [ADR 0039](decisions/0039-ssh-stack.md))

- [x] **R1** Serial (baud, parity, flow control) and Telnet sessions, raw TCP; saved in the host library. (ADR 0037. A session opens in a pane over a byte pipe (`Terminal::from_pipe`) dialled on a background thread: raw TCP and an in-tree Telnet codec add no dependency, serial uses the MIT `serialport` crate with `default-features = false` (no libudev). *New Serial/Telnet/TCP Session…* is in the Shell menu and the palette with a profile form; `hosts.toml` gains `kind` (`ssh` default, `serial`, `telnet`, `tcp`) and `[host.serial]`, so the sidebar, `mtty://host/<name>` and the palette open the new kinds too. Telnet and raw TCP are marked unencrypted, serial is not. A restored session reconnects, Duplicate dials again, and the pane ends when the connection drops; shell shims, OSC 7 cwd and command capture do not apply. Evidence: pipe-terminal and Telnet-codec unit tests, host-kind round trips, a transport-target JSON test, and a workspace build on macOS and Linux.)
- [x] **R2** `.ppk` key import (to OpenSSH format, never stored unencrypted). (ADR 0038. A GPU-free `mtty-keys` crate parses PPK v2 (SHA-1 KDF, HMAC-SHA-1) and v3 (Argon2id, HMAC-SHA-256) for Ed25519, RSA and ECDSA, checks the MAC before touching the key, decrypts `none`/`aes256-cbc`, and re-encodes through `ssh-key` with bcrypt-pbkdf + aes256-ctr under a passphrase the user chooses. the key-import action in *Hosts…* reads the file, writes `~/.ssh/<name>` (0600) and `<name>.pub`, refusing an existing file unless Overwrite is ticked; an empty new passphrase is refused and there is no unencrypted output path. `chacha20-poly1305` is rejected with a clear message (no vector). Evidence: 12 real fixtures from `puttygen` 0.81 — Ed25519/RSA/ECDSA × v2/v3 × plain/encrypted — imported and checked against their `.pub`, with wrong-passphrase, tampered-file and empty-passphrase tests.)
- [x] **R3** Decide in an ADR whether SSH moves from the system OpenSSH to a Rust-native stack (gains: Windows without OpenSSH, in-process SFTP and forwarding; costs: re-implementing `~/.ssh/config`, ProxyJump and agent forwarding). (Decided in [ADR 0039](decisions/0039-ssh-stack.md): keep the system OpenSSH client for now; macOS, Linux and Windows all ship it, and the ADR 0037 transport is where a native backend would slot in if that changes. `sshd`-free platforms, in-process SFTP/forwarding, or a security reason would trigger a revisit.)

### M7 AI-native workspace (design in [ADR 0040](decisions/0040-ai-native-workspace.md))

- [x] **A1** Agents' edits arrive as editor transactions, shown inline as diffs to accept or reject, undoable. (ADR 0040. MTP `editor.propose` takes `pane_id`/`path` plus `edits` (`{start,end,text}` character ranges) or a whole-file `text`; the host applies them to the editor pane's `Document` as one transaction (`apply_external`), tints the changed lines green, and marks them in the status bar. Typing, saving or another edit accepts it; *Reject Agent Edit* is one undo step back to the pre-image. Evidence: editor-pane tests for one-transaction apply/undo, accept-on-edit and accept, and an MTP test for the request.)
- [x] **A2** An ACP (Agent Client Protocol) client, so any ACP agent works with the editor pane. (ADR 0040. The GPU-free `mtty-acp` crate launches an agent and speaks newline-delimited JSON-RPC 2.0 over its stdio: `initialize`/`session/new`/`session/prompt`/`session/cancel`, answering `fs/read_text_file`, `fs/write_text_file` and `session/request_permission`, and forwarding `session/update` notifications and responses to a `Handler`. The host starts agents from `config.toml`'s `[acp]` (`[[acp.agent]]` with `name`/`command`) or a typed command via *ACP Agent…*; an ACP window shows the streamed transcript, sends prompts, cancels a turn, turns agent diffs into A1 proposals, and answers permission requests with Allow/Deny. Evidence: `mtty-acp` tests against a fake agent (initialize + streamed update, an fs read), a config-parse test, and the workspace build/tests.)
- [x] **A3** Send a selection, diagnostics or a command's output to an agent in one step; agents open files at a line through MTP. (ADR 0040. MTP `app.edit`/`app.view` take an optional 1-based `line` and `column`; the host opens the file and moves the editor pane there. *Send Selection to Agent*, *Send Diagnostics to Agent* and *Send Last Command Output to Agent* are in the palette: each finds the tab's agent pane and types a short, fenced prompt into it, pressing Enter. Evidence: an MTP test that `app.edit` carries `line`/`column`, and the workspace build/tests.)
- [x] **A4** Agent session resume and quota display ([ADR 0042](decisions/0042-agent-resume-quota.md)). A *Resume Agent Session…* picker lists the sessions agents reported over MTP and relaunches the chosen one in its recorded cwd; MTP/`mtty-cli` expose `agent.sessions` and `agent.resume` (new `agent.resume` capability), the hook reports `--cwd`, and the Agent tab shows an agent-reported quota line. Evidence: a resume-template token/quoting test, an MTP test for `agent.sessions` ordering and `agent.resume` queueing, a hook-script `--cwd` test, and workspace tests/clippy.

## 5. Non-goals

- No accounts or mandatory sign-in; every cloud feature is optional.
- No built-in model calls; mtty orchestrates external agents, it does not
  replace them.
- No edits to the user's shell, agent or ssh config files.
- No plaintext passwords stored by the UI.

## 6. Keeping this current

- When a feature's status changes, update the §3 tables and both READMEs in the
  same commit.
- Tick roadmap items when done and cite the evidence (test names, smoke output
  or a screenshot note).
- Changing a locked design decision needs a new ADR under `docs/decisions/`.
