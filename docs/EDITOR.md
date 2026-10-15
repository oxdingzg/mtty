# The editor

[简体中文](EDITOR.zh-CN.md)

mtty puts a real editor next to the terminal, in the same tabs and splits, so a
file and the shell that builds it never leave the window. It is built on
`mtty-editor`, a rope-backed core with no UI code, so the same engine can be
embedded elsewhere. Editing is GPU-rendered through the terminal renderer,
which is what keeps very large files fast.

## Opening a file

The **Files** tab of the details panel lists the active directory; a click
opens the reader. Beyond that:

- **Open Quickly** (`⌘⇧O`) fuzzy-matches files, folders, recent files and text
  inside them, plus tabs, agents and saved hosts.
- **Open File…** opens a local file in a native editor pane.
- **Edit in Tab** runs your configured `editor`, then `$EDITOR`, then `vi`
  (Notepad on Windows) in a terminal tab; it is the external-editor path.
- **Open Externally** hands the file to an external editor from the palette.
- Jump-to-line navigation and `file:line` arguments open the pane at the line.

Local and remote files both open in a pane; remote files are read and written
over ssh (see [Remote hosts and SSH](REMOTE.md)).

## Syntax and large files

Highlighting uses 80 compiled-in **tree-sitter** grammars with incremental
reparse and visible-range queries. A file no grammar claims falls back to
syntect's Sublime syntaxes plus permissively licensed ones vendored from `bat`
(Julia, nginx, VHDL, Org and others); the language shows in the status chip.

Size is handled in stages rather than refused:

| File size | What happens |
|---|---|
| Up to 512 KB | Parsed on the UI thread; a keystroke reparses incrementally |
| Above 512 KB | The parse moves to a background thread; colour arrives shortly after |
| Above 8 MB | Tree-sitter is disabled to bound memory use; syntect fallback is limited to files up to 1 MB, so larger files remain plain text |
| Above 64 MB | The file opens in **view mode**: a window of lines is read from disk with a sparse index built in the background, so memory stays small at any size |

In view mode, typing, pasting or **Switch to Editing…** offers to load the file
for editing and states the memory it will take (about 2.2× the file). Find
scans the whole file on a thread.

## Editing

- **Multiple cursors**: `⇧⌘L` selects every occurrence (whole words from a bare
  caret), `⇧⌥I` puts a caret at each selected line's end, `⌥⌘↑` / `⌥⌘↓` add
  carets (`Ctrl+Alt+↑/↓` outside macOS).
- **Find and replace**: match case, whole word and regex; every match on screen
  is highlighted; *Replace*, *Replace All* (one undo step) and *Select All
  Matches* (`⌥↩`).
- **Go to Line** (`⌃G`) accepts `line:column`.
- Ordinary undo, redo, indent, grapheme- and word-wise motions, and a macOS-style
  keymap that keeps `⌘D`, `⇧⌘Z` and `⇧⌘L` for the editor.

### Vim mode

Set `editor-vim = true` for a vim state machine over the document:
`NORMAL` / `INSERT` / `VISUAL` / `VISUAL LINE`, counts, `h j k l w b e 0 ^ $ gg
G`, `i a I A o O`, `x`, `d`/`c`/`y` with motions (`dd`, `cc`, `yy`, `dw`, `d$`),
`p`/`P` with an internal register, `u` and `Ctrl-r`, `J`. `/` opens Find and `:`
runs `w`, `q`, `wq` or a line number; the `za` family drives folds. The status
bar shows the mode.

## Folding and outline

Folds come from the syntax tree (any named node spanning more than one line),
with indentation as the fallback. The gutter shows `▸`/`▾`, a collapsed header
ends in `⋯`, and up/down skip hidden lines. `⌥⌘[` / `⌥⌘]` fold and unfold;
*Fold All*, *Unfold All* and *Toggle Fold* are in the palette. `⌘R` opens a
filterable **outline** of the file's definitions, nested by depth.

## Markdown editing

Markdown files open in one writing pane. Blocks render in place; click a block
to edit its Markdown source at that position. **Source** shows the complete
source in the same pane. **Undo**, **Redo**, and `⌘S`/`Ctrl+S` use the original
rope-backed document and saving/history path, including external reloads.

The renderer covers headings, lists, quotes, tables, code and links, resolves
relative images against the document's folder, and renders Mermaid
`graph`/`flowchart`, `sequenceDiagram`, `stateDiagram`, `classDiagram`,
`erDiagram` and `pie`. Set `mermaid-command` to use `mermaid-cli` for full
Mermaid. *Toggle Markdown Preview* remains available for an optional separate
preview pane; it closes with its editor and is kept in a saved session.

This is block-level live editing. Inline token-level source reveal, advanced
table controls, and the remaining writing features are tracked in the
[Markdown roadmap](https://github.com/oxdingzg/mtty/issues/94).

## Language servers

With an `[lsp]` entry, the editor starts a language server for `rust-analyzer`,
`typescript-language-server`, `pyright-langserver`, `gopls` or `clangd` found on
the login shell's `PATH` — one server per language group and workspace root.
Files over 2 MB and remote SSH panes do not start a language server.

In the pane you get diagnostic underlines with ✖/⚠ counts in the status bar,
hover after the pointer rests, completion on trigger characters, words and
`Ctrl+Space` with client-side filtering (including snippets and import edits),
`F12` or `⌘`-click to a definition, and `F8` for the next problem. Servers and
root markers are configured under `[lsp]` in [`config.toml`](CONFIG.md).

## Saving without losing work

Local saves atomically replace the file and preserve its permissions. Failed saves
are reported and leave the buffer **modified**; closing unsaved changes,
Close Others/Below or quitting asks first. Local files are watched: clean panes
reload as one undoable transaction; modified panes ask whether to reload or keep
the local version. A deleted local file is reported once.

Remote saves compare the file with the bytes last loaded or saved, then replace
it using a temporary file in the same directory, preserving permissions and
following symbolic links. A transfer failure leaves the original intact. When
the remote file changed, a conflict window preserves your edits and shows the
local and remote versions: keep editing, reload the remote version, or save over
the reviewed version. The last option checks that version again before writing.
These checks are optimistic, not an atomic compare-and-swap against external
writers. Remote panes also poll for changes: clean panes reload; modified panes
offer conflict review. Failed probes or reads do not replace the buffer. Remote
files over 2 MiB are rejected rather than opening a truncated editable buffer.

## Configuration and keys

| Key | What it does |
|---|---|
| `editor` | The command *Edit in Tab* runs (for example `code --wait`) |
| `editor-vim` | Enable minimal vim mode |
| `mermaid-command` | Use `mermaid-cli` for full Mermaid rendering |
| `[lsp]` | Language servers, commands and workspace root markers |

[Keyboard shortcuts](SHORTCUTS.md) lists the window, terminal and editor keys.
