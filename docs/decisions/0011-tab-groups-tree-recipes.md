# ADR 0011 — Tab groups, the file tree, and recipes

Status: accepted.

## Context

Roadmap U1 asks for richer tabs (mark/divider/group) and a browsable
sidebar; U7 asks to save and replay a workspace (*recipes*) and to export
config. These are all small, local additions on top of the existing tab/session
model.

## Decision

**Tab appearance (U1).** A tab gains two optional fields — `mark` (after the
title) and `group`. The tab bar draws a vertical divider wherever the group
changes, so tabs of one group sit together. Both are set from the sidebar
context menu (Rename / Mark / Group, plus *Remove from Group*) through one
dialog keyed by `TabField`, and they round-trip through `session.json` as
`#[serde(default)]` fields (older sessions load unchanged).
The state stays the tab's icon; the title names the group (`[work]
title★`), so a narrow tab strip still says where the tab belongs. A tab that joins a group moves next to the last tab carrying that
name, and the Group dialog lists the groups already in use so joining one is a
click rather than a retyped name.

**File tree (U1).** The left sidebar shows a tree of the focused pane's cwd
below the tab list. Directories expand/collapse (state in
`tree_expanded: HashSet<PathBuf>`); double-clicking a file opens it in the
reader/editor from ADR 0009. To bound cost it skips hidden entries, caps each
directory at 200 entries and recursion at depth 6.

**Recipes (U7).** The whole workspace (tabs, splits, cwds — the existing
`Session`) is saved as JSON under `~/.config/miaotty/recipes/<name>.json` and
restored with the same `restore` path used at startup. The palette offers
*Save Recipe…* and *Open Recipe…*; names are sanitized to a safe filename
charset. Config export is the existing *Save to config.toml* plus *Save
views.json*.

## Consequences

- Grouping/mark are pure presentation and serialized with defaults, so no
  migration is needed.
- Recipes reuse the session format, so anything that can be restored at startup
  can be saved and replayed.
- The file tree re-reads directories as it is drawn; the caps keep it cheap, and
  caching/async loading can replace it later without changing the UI.
