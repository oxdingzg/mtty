# Local patches to winit 0.30.13

Only the macOS content view is touched (`src/platform_impl/macos/view.rs`):

1. `mouseDownCanMoveWindow` now returns `false`. With a transparent, full-size
   title bar (`with_titlebar_transparent` + `with_fullsize_content_view`) the
   view is non-opaque, so AppKit's default value for `mouseDownCanMoveWindow`
   is `true`. AppKit then turns a drag in the title strip — including a drag on
   a tab chip — into a system window move, so tab reordering never ran (only the
   right-click *Move Up / Move Down* menu worked), and text selection or sliders
   in that strip fought the window. The host (`mtty-widget`) already moves
   the window itself, per region, via `Window::drag_window` driven by
   `Chrome::on_title_drag_hover`; this patch lets those regions receive the drag.

Symptom without the patch: on macOS, pressing and dragging a tab chip moved the
whole window instead of reordering the tab.

2. `insertText:replacementRange:` reads the current event's characters only
   when the event is actually a key down. The CJK check compares the committed
   string against `NSApp.currentEvent.characters`; on macOS 26 the new
   NSInputAnalytics path can deliver `insertText:` while the current event is
   not a keyboard event, and `-[NSEvent characters]` then trips an
   NSAssertionHandler (SIGABRT, main thread). Two crashes were collected this
   way (2026-10-14, 2026-10-15), both on Chinese input.

Symptom without the patch: the app aborts mid-typing when the IME or macOS
input analytics commits text outside a key event.

Upstream is MIT / Apache-2.0 (see `LICENSE-MIT`, `LICENSE-APACHE`); only `src/`,
`Cargo.toml` and those licence files are vendored.
