# mtty architecture

> A cross-platform (macOS / Linux / Windows) terminal **engine** plus the `mtty`
> application built on it.
> This is the pre-implementation design. **Locked** items are decisions; changing
> one requires an ADR under `docs/decisions/`.
> 简体中文版: [`ARCHITECTURE.zh-CN.md`](ARCHITECTURE.zh-CN.md).

## 0. Scope, in one sentence

**`portable-pty` + `alacritty_terminal` + `vte` as the core, `winit` + `wgpu` for the
window and drawing, the terminal grid self-drawn and the surrounding UI in egui; the
engine (`mtty-*`) is separate from the app (`mtty-app`), and the control plane
(MTP) is decoupled from the engine.**

## 1. Goals · Non-goals · Constraints

**Goals**
- One codebase on three platforms, **including Windows (ConPTY)**.
- Hot-path performance at Alacritty's level: input latency P95 ≤ 16 ms (target ≤ 8 ms),
  first frame ≤ 100 ms, no dropped frames while scrolling, idle CPU ≈ 0.
- The engine is embeddable; `mtty-app` is its first consumer (the only in-repo example is
  `crates/mtty-render/examples/pipeline_probe.rs`, a render-pipeline probe).
- Reuse the existing control plane: `mtp` types, `mtty-cli`, plugins, agent/shell hooks.

**Non-goals (for now)**
- Matching Ghostty's rendering polish or config ecosystem; macOS-only integrations
  (AppleScript/Sparkle).
- A plugin marketplace / "framework"-level extension system; cross-device sync.

**Constraints**
- Permissive licenses only (Apache-2.0/MIT/BSD/ISC); **no GPL/AGPL inside the engine**
  (e.g. Pebrel is read-only reference, never copied).
- Every new external crate passes a performance and license review.
- Outbound license: **Apache-2.0** for the engine and the app.
- Rust stable; MSRV in `rust-toolchain.toml`.

## 2. Locked decisions (summary)

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | Core uses `alacritty_terminal` + `vte` + `portable-pty` | Most mature, cross-platform (incl. ConPTY), permissive |
| D2 | Self-drawn rendering on `wgpu` (glyphs via `glyphon`/`cosmic-text`/`swash`) | Controllable performance; Metal/Vulkan/DX12 |
| D3 | Surrounding UI (panels/settings) uses `egui`, **co-rendered in one frame** with the terminal | Dev speed + a fast self-drawn terminal |
| D4 | **Own tab/split model** (not OS-native tabs) | Cross-platform consistency; controllable |
| D5 | Concurrency per **Alacritty's lock discipline** (`FairMutex<Term>` + `EventListener`) | Proven; avoids inventing a snapshot protocol |
| D6 | Platform differences only in `core::pty` / `widget::platform` / `mtp::transport` | Contain complexity |
| D7 | `mtty-mtp` decoupled from the engine (Unix socket / Windows named pipe) | A crash in one doesn't take down the other; reuse the protocol |
| D8 | Build the app first, extract the library later; phase the extension points | Real needs drive the API |
| D9 | Engine crates licensed `Apache-2.0` | Permissive; easy to embed |
| D10 | One native `mtty` executable delegates to `mtty-widget` (winit + wgpu); the eframe host is retired | Single product identity and direct rendering; see APP-IDENTITY.md |

## 3. Layering (DAG) and rules

```
   mtty-graphics    mtty-config    mtty-mtp     (leaf crates, no engine deps)
        │                │            │
        ▼                │            │
   mtty-core             │            │
        │                │            │
        ▼                │            │
   mtty-render           │            │
        │                │            │
        ▼                ▼            │
   mtty-ui ◄─────────────┘            │
        │                             │
        ▼                             │
   mtty-widget ◄──────────────────────┘   (native host: winit + wgpu)
        ▲
        └── mtty-app   (native executable; depends on mtty-widget)
   mtty-cli ──► mtty-mtp
```

Not every edge is drawn: `mtty-widget` also depends on `mtty-core`/`mtty-render`, and
`mtty-app` delegates to `mtty-widget`. The workspace has nine members.

**Rules**
- Dependencies point inward only: `widget → render → core`. The one exception is
  `core → graphics` (the inline-graphics scanner/decoders); `core` still depends on no
  GPU/windowing/egui crate.
- `core` must not depend on `wgpu`/`winit`/`egui` (**it must build without a GPU**); `render` must not depend on `winit`.
- `config`/`mtp` must not depend on rendering or windowing.
- The app depends on the engine; the engine **never** depends on the app.
- CI compiles the whole workspace (`cargo check --workspace`) and runs `cargo fmt --check` +
  `cargo clippy --workspace --all-targets -- -D warnings`; there is no `cargo-deny` job and no
  `deny.toml`.

## 4. Crate / module responsibilities

| Crate | Does | Does not |
|-------|------|----------|
| `mtty-graphics` | Inline-graphics scanner + Sixel/Kitty/iTerm2 decoders | No rendering/GPU/windowing |
| `mtty-core` | PTY, vte parsing, grid/scrollback/cursor/modes, selection/search, OSC/CSI semantics, key/mouse→bytes encoding, events; owns the scanner in `mtty-graphics` | No GPU/window/config/business logic |
| `mtty-render` | Font load/shaping/atlas, grid instancing, draw passes, damage increments | No event loop/input |
| `mtty-ui` | Host-agnostic UI: theme, input encoding, selection, split layout, egui chrome, palette, hints, vim, markdown, ssh, update, agent integration | No window/event loop |
| `mtty-widget` | Native host library (`mtty-app` supplies the executable): winit event loop, wgpu surface, input/IME/clipboard/drag-drop, direct grid draw (ADR 0030) | No PTY/parser duplication |
| `mtty-editor` | Editing core for the editor pane: rope, transactions and undo, multiple selections, motions, find/replace (ADR 0034) | No UI, no GPU |
| `mtty-config` | Config model, themes, ghostty/alacritty import | No UI |
| `mtty-mtp` | Protocol envelope, transport, server/client, agent/history registries, revision + `core.wait` long-poll | No engine dependency |
| `mtty-app` | Native `mtty` entry point, command help/version and platform packaging metadata | No terminal core duplication |
| `mtty-cli` | MTP client for scripts/agents | No engine dependency |

## 5. Core types and traits (design sketch — superseded)

The traits/types sketched in the original draft (`Damage`, `TermEvent`, `EventSink`,
`Pty`, `InputEncoder`, `GlyphAtlas`, `Renderer`, `Host`) were **not adopted**. The
shipped API is:

- `mtty-core`: `Terminal` (PTY + parser + grid) and `ATerm` (the `alacritty_terminal`
  screen model), in `crates/mtty-core`.
- `mtty-render`: `TermRenderer`, `QuadRenderer`, `ImageRenderer`.
- Application: `mtty-app` launches `mtty-widget`, which owns the event loop
  and draws the grid directly with `mtty-render`, compositing egui chrome.

## 6. Threading model and lock discipline

Use Alacritty's proven model (`FairMutex<Term>` + `EventListener`); do not invent a snapshot protocol.

```
┌─ PTY reader thread ────────────────────┐     ┌─ main thread (winit loop) ────────────┐
│ loop read():                           │     │ winit event:                          │
│   lock(term) { vte.process(chunk) }    │     │   Keyboard/IME → core::input → pty    │
│   mark Damage; sink.send(Wakeup)       │     │   RedrawRequested:                    │
│   (lock covers parsing only)           │     │     lock(term){ renderer.update }     │
└────────────────────────────────────────┘     │     renderer.render(frame); present   │
                                               └───────────────────────────────────────┘
```

**Invariants**
1. The `Term` lock covers **only** "parse one chunk" or "build render instances" — it never
   spans `read()`/`write()`/GPU submit.
2. The PTY reader thread **owns** the reader; writes use a separate handle (`Pty::write`) so
   reads and writes don't block each other (especially ConPTY).
3. No polling when idle; redraw is requested only by `Wakeup` / input / the cursor-blink timer.
4. Resize: the main thread computes `cols/rows` → `Pty::resize`; `Term` resizes in sync.
5. Exit status comes from **OSC 133;D**, not the process exit code (unreliable under ConPTY/wrappers).

## 7. Data flow

- **Output**: PTY → reader thread → `vte` → `Term` (+OSC → `TermEvent`) → `Wakeup` → main-thread render.
- **Input**: winit key/mouse/IME → `core::input` encode → `Pty::write`; selection/paste honor bracketed-paste mode.
- **Control**: the `mtty-mtp` server runs on its own thread; in-process UI talks to the registries directly,
  external CLI/plugins go over the socket/pipe.
- **Metadata**: cwd (OSC 7), title, agent state, command history → events/registries → panel subscriptions.

## 8. Rendering (mtty-render)

1. **Glyphs**: `cosmic-text` resolve/fallback, `swash` rasterize → **R8 atlas** (shelf packing, LRU;
   key `(glyph_id, style, px, subpixel)`).
2. **Instancing**: each cell → quads (bg / glyph / underline / strike / cursor / selection); instance
   buffers update by dirty lines only.
3. **Passes**: `bg` (theme color) → `glyph` (atlas sample) → `cursor/decoration`; opacity/background image later.
4. **Present**: prefer `Mailbox` (low latency), fall back to `Fifo` on frame drops; cursor blink via a timer.
5. **Composition with egui**: egui drives the frame; the terminal draws its own rect via an
   `egui-wgpu` **`PaintCallback`** (separate pipeline, shared `wgpu::Device/Queue/Surface`).
6. **Optimization order**: full rebuild → dirty lines → atlas hits → zero per-frame allocation;
   annotate hot spots with `tracy`/`puffin`.

## 9. Terminal model (mtty-core)

- Grid: `Cell { char + combining + fg/bg/attrs + underline style }`; ring scrollback; reflow.
- Modes: application cursor keys, bracketed paste, mouse reporting, alternate screen, kitty keyboard
  (CSI u), focus reporting.
- Semantics: title, hyperlinks (OSC 8), cwd (OSC 7), progress (OSC 9;4), shell-integration markers
  (OSC 133 A/B/C/D).
- Selection/search: line/block, word boundaries (CJK/grapheme via `unicode-width` + grapheme
  boundaries), search highlight.
- Graphics protocols (kitty graphics / sixel / iTerm2) are implemented by `mtty-graphics`,
  wired through `mtty-core`/`mtty-render`, and on by default (`graphics = true`).

## 10. Platform abstraction layer

| Concern | Abstraction | macOS/Linux | Windows |
|---------|-------------|-------------|---------|
| PTY | `trait Pty` | `forkpty` | **ConPTY** |
| Transport | `mtp::transport` | Unix socket | `\\.\pipe\mtty` |
| Clipboard | `trait Clipboard` | NSPasteboard / X11-Wayland | Win32 clipboard |
| Fonts | `mtty-render::font` | CoreText / fontconfig | DirectWrite (`font-kit`) |
| IME | `widget::input` | native | **TSF** (highest risk, see §19) |

`#[cfg(...)]` is **only** allowed in the modules above; all other code stays platform-neutral.

## 11. Config / themes

- `mtty-config`: its own TOML; keys aligned with ghostty/alacritty for import.
- Default look is Nord (background `#2e3440`, font size 13), overridable.
- Built-in themes: `nord` (default), `dracula`, `gruvbox`/`gruvbox-dark`,
  `solarized`/`solarized-dark`, `tokyo-night`/`tokyonight`; plus custom palettes.
  Background opacity is wired; background images are later.

## 12. Control plane (MTP)

- `mtty-mtp` implements the server; the local transport is a Unix socket
  (`$XDG_RUNTIME_DIR/mtty.sock`, falling back to `$TMPDIR/mtty.sock`) or a Windows named pipe.
- Remote access: `remote-listen = addr:port` also serves the control plane over TCP; it
  **requires** `MTTY_MTP_TOKEN`, and the client sends that token on every request.
- Reuse the existing `mtp` messages and `mtty-cli`; **in-process UI talks to the registries directly**,
  external callers go over the socket/pipe.
- Methods: `core.ping/health/wait/subscribe`, `agent.state.*`, `history.*`,
  `pane.list/send/run/focus/close`, `app.view/edit` (open a file in the reader/editor),
  `file.read/write` (offset/length, base64, 2 MB cap; optional token via `MTTY_MTP_TOKEN`).
- Change notification comes in two forms: a **revision counter** with the `core.wait` long-poll
  (returns as soon as the revision moves past the caller's value), and **server-push** via
  `core.subscribe`, which upgrades the connection to a stream of `{"kind":"event", …}` lines for
  the topics `agent.state`, `panes` and `history` (`mtty-cli events`). `cwd.changed` is not
  emitted.
- Transport: `interprocess` (local socket / named pipe); TCP for `remote-listen`.

## 13. Application layer (mtty-app)

- Model: `Window → Tab[] → SplitTree<Surface>`; a surface is one terminal instance (core + render view).
- Window: one OS window holds a set of tabs; splits are a tree inside a tab (ours, not OS tabs).
- Panels (egui): **left Tabs sidebar** (title/⌘N/mark/dividers/context menu), **right Details**
  (Info/Outline/Git/Files), badges, settings, command palette.
- Unified chrome: panels share the terminal background, no divider line, hover highlight.
- OS integration: hook install, URL open, notifications, sleep inhibition (per-platform).

## 14. Extension points (phased; don't front-load)

- **Now**: the engine API (`Terminal`/`ATerm`, `mtty-render`) + MTP (enough).
- **R4+**: MTP `provider.*` (custom Details components, kind=tui/web).
- **Later (only if demanded)**: renderer/panel plugins, theme packs — only then consider "framework"-izing.

## 15. Errors and security

- Libraries use `thiserror`, the app uses `anyhow`; **no panics on the hot path**; recoverable errors
  become events/logs.
- Security: paste confirmation, OSC injection surface, URL-scheme allowlist, clipboard policy,
  optional secure input.
- Crashes: install a panic hook that logs and best-effort cleans up PTY/child processes.

## 16. Testing / CI / performance gates

- **Conformance**: no `vttest`/`esctest`/`cargo-fuzz` harness has been added; parser behavior is
  covered by the crates' own unit tests.
- **Integration**: spawn a real shell, feed byte sequences, assert grid/events.
- **Performance gate (ubuntu-latest only)**: `cargo test --release -p mtty-core -p mtty-app -- --ignored`
  plus `scripts/check-perf-baseline.py`. Budgets: input latency P95 ≤ 16 ms, first frame ≤ 100 ms,
  no dropped frames on a large `cat`, idle CPU ≈ 0.
- **CI** (`.github/workflows/ci.yml`): jobs `changes`, `privacy`, `lint`, `check-linux`
  (`cargo check --workspace`), `test-linux`, `macos`, `windows`, `render-linux`, `perf`. `lint`
  (`cargo fmt --check` and `cargo clippy --workspace --all-targets -- -D warnings`) and
  `check-linux` run on pushes (macOS does a compile-only check); tests/perf/render/windows run on
  PRs, the nightly `17 4 * * *` schedule and `workflow_dispatch`.
- **Render**: `render-linux` installs Mesa lavapipe (software Vulkan) and runs the headless smoke test
  with `MIAO_REQUIRE_GPU=1 WGPU_BACKEND=vulkan`.
- There is no `cargo-deny` job and no `deny.toml`.

## 17. Packaging / release / versioning

- Engine crates `Apache-2.0`, internal first; **not published to crates.io yet** (the API is not stable).
- App artifacts (`.github/workflows/release.yml`): macOS `.app` zip; Linux tar.gz + `.AppImage` + `.deb`;
  Windows zip + MSI (WiX via `cargo-wix`).
- Signing is optional: macOS `codesign`/`notarize`, Windows `signtool` with an optional PFX, plus
  optional `minisign` of the artifacts.
- `wgpu` DX12 needs a bundled `dxcompiler.dll` or static `static-dxc`.

## 18. Milestones

R0 minimal loop (pty→vt→grid→render→input, measure latency) → **R0.5 IME** → R1 usable terminal →
R2 three platforms + packaging → R3 `mtty-mtp` → R4 panels → R5 polish/signing. Each exit must pass its performance gate.

## 19. Risks

| Risk | Mitigation |
|------|------------|
| Self-drawn rendering not reaching Alacritty level | Start with glyphon → damage/vsync/atlas optimization; reference Alacritty/Rio |
| **IME/CJK (riskiest on Windows)** | R0.5 focus; winit `Ime` + `set_ime_cursor_area`; width/grapheme test set |
| ConPTY performance / exit codes | Concurrent read+write + OSC 133;D + separate baseline |
| egui co-frame compatibility/perf | `PaintCallback` approach; fall back to manual viewport if needed |
| Over-abstraction too early | Engine "good enough" first; phase the extension points |
| `alacritty_terminal` API churn | Pin versions; wrap with a `mtty-core` adapter layer |

## 20. Performance impact analysis (vs Ghostty / Alacritty)

**Conclusion: the choices themselves have no structural performance loss** (the core is Alacritty's,
rendering is GPU). Risks are: ① egui immediate-mode idling every frame if ungated; ② a first self-drawn
renderer is less polished than Ghostty's years-tuned Metal compute; ③ wgpu abstraction + macOS present
modes; ④ Windows ConPTY platform cost (inherent to any approach).

| Dimension | Baseline | Impact | Mitigation |
|-----------|----------|--------|------------|
| Input latency | Alacritty class | No structural loss | Event-driven (no polling) + prefer Mailbox/Immediate; R0 gate |
| Throughput | Alacritty class | **Lock contention** is the main risk | Chunked parsing, short critical sections; snapshot for render if needed |
| Rendering | vs Ghostty Metal | Weaker at first | glyphon start → damage/persistent instance buffers → compute if needed |
| **egui co-frame** | new overhead | Immediate mode may rebuild UI every frame | **Run egui only when chrome is dirty**; terminal redraw driven by damage; no continuous repaint |
| wgpu abstraction | vs direct Metal | Tiny (same Metal backend) | Keep a specialized path possible; benchmark head-to-head |
| present/vsync | macOS mode limits | Forced Fifo increases latency | Measure Mailbox/Immediate; choose dynamically |
| Fonts/first frame | — | Load + shaping slow the first frame | Parallel init + on-disk cache |
| Memory | parity | Atlas/instance buffers | LRU atlas, buffer reuse, dirty-region atlas uploads |
| Idle CPU | parity (≈0) | Blink/animation makes it nonzero | **Blink only when focused**; no timed polling |
| Windows | ConPTY cost | Inherently slower (ConHost + VT re-encode) | Concurrent I/O; a Windows-specific baseline |
| macOS polish | loses subpixel/emoji/blur | **Visual** loss, not performance | Dedicated later |

**Red line**: R0 must pass input latency P95 ≤ 16 ms / no dropped frames on large output / idle CPU ≈ 0
before later milestones. egui idling, present mode, and atlas uploads are the three key things to measure.

## 21. ADRs (`docs/decisions/`)

Full index: [`docs/decisions/README.md`](decisions/README.md) (ADRs 0001–0030).
