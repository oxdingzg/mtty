Vendored copy of egui-winit 0.30.0 from crates.io.

Licence: MIT OR Apache-2.0 (see https://github.com/emilk/egui).

One change: an image-only clipboard now delivers an empty `Paste` event
instead of nothing, so a terminal host can forward the paste to the TUI.
