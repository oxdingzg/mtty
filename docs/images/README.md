# Screenshot provenance

Refreshed 2026-10-04 from the mtty v0.1.3 development build.

The captures use the actual application's GPU rendering, with an isolated
example Git project, Markdown/Mermaid source and Rust code. Agent-state events
and editor proposals are driven through the real MTP control plane. The split
terminal runs an actual local HTTP server. SSH host destinations and queued
prompts are examples.

- `mtty.png`: editor, preview and terminal in one workspace.
- `mtty-workspace.png`, `mtty-states.gif`: session badges and agent-state events.
- `mtty-editor.*`, `mtty-review.*`: live preview and inline proposal acceptance.
- `mtty-hosts.png`, `mtty-splits.png`: host library and split terminals.
- `mtty-palette.*`, `mtty-vim.png`: commands, vim Normal mode and tree-sitter folds.
- `mtty-tasks.png`, `mtty-queue.png`, `mtty-panels.gif`: command history and details
  panels. The panel GIF is a sequence of seven fresh still captures.

Stills are at most 1600 px wide. GIFs are 960 px, 2 fps and under 180 KB each.
The website uses responsive WebP stills and smaller MP4 demos with playback
controls. See the [shared media notes](https://github.com/mttydev/mtty.dev/blob/main/docs/media.md)
for capture guidelines and budgets.
