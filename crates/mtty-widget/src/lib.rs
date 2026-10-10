//! `mtty-widget` — a native `winit` + `wgpu` host for the engine.
//!
//! The terminal grid is **self-drawn** (no egui immediate-mode frame flow): the
//! host owns the window and surface, and the PTY reader threads wake the loop so
//! echo is drawn on the next frame. The surrounding UI (tabs, sidebar, details,
//! status) is an **egui overlay composed in the same wgpu frame**. Shared,
//! host-agnostic pieces (theme, input encoding, selection, split layout, row
//! building, chrome widgets) live in `mtty-ui`.

use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mtty_core::Terminal;
use mtty_render::{ImageInstance, ImageRenderer, Quad, QuadRenderer, Span, TermRenderer};
use mtty_ui::layout::{Layout, Rect, SplitDir};
use mtty_ui::{
    build_rows, chrome, input,
    theme::{Rgb, Theme},
    Selection,
};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};

/// Events posted to the loop: a repaint wake-up, or the global Quick Terminal
/// hotkey (which is the only thing that should toggle the scratch tab).
#[derive(Clone, Copy)]
enum HostEvent {
    Wake,
    Hotkey,
    /// A command from the OS menu bar (macOS, inside an app bundle).
    #[cfg(target_os = "macos")]
    /// The flag says the menu item's shortcut was pressed, not clicked.
    Menu(mtty_ui::chrome::MenuId, bool),
}
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

mod drag;
mod editor_pane;
mod live_markdown;
#[cfg(target_os = "macos")]
mod macos_url;
mod quick_content;
mod redraw;
pub mod resource_metrics;
mod session;
#[cfg(all(unix, not(target_os = "macos")))]
mod wayland_dnd;

#[derive(Debug)]
enum UpdateResult {
    Current,
    Available {
        version: String,
        artifact: Option<mtty_ui::update::Artifact>,
    },
    Failed(String),
}

/// Downloading and installing an available update (B4.3).
#[derive(Debug, Clone, PartialEq)]
enum UpdateInstall {
    Idle,
    /// Downloading, then checking the checksum and signature.
    Working,
    /// Verified and ready to install.
    Ready(std::path::PathBuf),
    Failed(String),
}

const STATUS_H: f32 = 22.0;
const SIDEBAR_W: f32 = 200.0;
const DETAILS_W: f32 = 300.0;
const CARD_MARGIN: f32 = 6.0;
const CARD_RADIUS: f32 = 9.0;
const CARD_PAD: f32 = 8.0;
const BLINK: Duration = Duration::from_millis(530);
const IMAGE_FRAME_MS: u64 = 100;

fn next_image_frame(start: Instant, now: Instant) -> Instant {
    let phase = (now.duration_since(start).as_millis() % u128::from(IMAGE_FRAME_MS)) as u64;
    now + Duration::from_millis(IMAGE_FRAME_MS - phase)
}

/// Whether the app menu belongs in the OS menu bar: macOS, and only from inside
/// an app bundle — that is where the application icon lives that AppKit's about
/// panel wants, and `muda` needs one it can decode (ADR 0031).
fn menu_in_os() -> bool {
    #[cfg(target_os = "macos")]
    {
        // Asked on every layout pass; the answer cannot change while running.
        static IN_BUNDLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *IN_BUNDLE.get_or_init(|| {
            std::env::current_exe()
                .map(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
                .unwrap_or(false)
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Whether the window extends under a transparent title bar: the
/// traffic lights float over the sidebar header and the title row takes the
/// title bar's place. Windows uses a custom caption and compact menu in that
/// same row; macOS does so only when the menu lives in the OS menu bar.
fn unified_titlebar() -> bool {
    cfg!(windows) || (cfg!(target_os = "macos") && menu_in_os())
}

// With a transparent, full-size title bar macOS treats the whole title strip as
// window-move area: a press there starts a system window drag that swallows the
// very gesture meant to reorder or click tab chips. Turn the system drag off and
// keep the app's own dragging (on_title_drag_hover + drag_window), which the
// chrome already drives per region.
#[cfg(target_os = "macos")]
fn disable_native_titlebar_move(window: &winit::window::Window) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    // SAFETY: winit keeps the NSView alive for the window's lifetime, and
    // `window` / `setMovable:` exist on every supported macOS. Nil is checked.
    unsafe {
        let view = handle.ns_view.as_ptr().cast::<AnyObject>();
        let ns_window: *mut AnyObject = msg_send![view, window];
        if !ns_window.is_null() {
            let _: () = msg_send![ns_window, setMovable: false];
        }
    }
}

/// Room the traffic lights take at the leading edge of a unified title bar.
const TRAFFIC_LIGHTS_W: f32 = 76.0;

/// Resize handles for the undecorated Windows frame, in physical pixels.
#[cfg(any(windows, test))]
fn window_resize_edge(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    edge: f64,
) -> Option<winit::window::ResizeDirection> {
    use winit::window::ResizeDirection::*;
    if x < 0.0 || y < 0.0 || x >= w || y >= h {
        return None;
    }
    match (x < edge, x >= w - edge, y < edge, y >= h - edge) {
        (true, _, true, _) => Some(NorthWest),
        (_, true, true, _) => Some(NorthEast),
        (true, _, _, true) => Some(SouthWest),
        (_, true, _, true) => Some(SouthEast),
        (true, _, _, _) => Some(West),
        (_, true, _, _) => Some(East),
        (_, _, true, _) => Some(North),
        (_, _, _, true) => Some(South),
        _ => None,
    }
}

/// The OS menu bar, built from the shared menu table (ADR 0031).
#[cfg(target_os = "macos")]
mod appmenu {
    use muda::accelerator::Accelerator;
    use muda::{Menu, MenuEvent, MenuId as MudaId, MenuItem, PredefinedMenuItem, Submenu};
    use std::str::FromStr;
    use winit::event_loop::EventLoopProxy;

    use super::HostEvent;

    /// Everything created for the menu bar. It must stay alive for the life of
    /// the app: muda's native items keep a raw pointer to the Rust-side
    /// `MenuChild`, so dropping an item leaves the click handler reading freed
    /// memory (`CFString cannot be created from a negative number of bytes`,
    /// SIGTRAP). See ADR 0031.
    pub struct MenuHandle {
        _menu: Menu,
        _items: Vec<MenuItem>,
        _submenus: Vec<Submenu>,
    }

    /// Build and install the application menu.
    pub fn install(
        lang: mtty_ui::i18n::Lang,
        proxy: EventLoopProxy<HostEvent>,
    ) -> Option<MenuHandle> {
        let mut items: Vec<MenuItem> = Vec::new();
        let mut submenus: Vec<Submenu> = Vec::new();
        let menu = Menu::new();
        // AppKit treats the first submenu as the application menu.
        let app_sub = Submenu::new("mtty", true);
        let _ = app_sub.append(&PredefinedMenuItem::about(None, None));
        let _ = app_sub.append(&PredefinedMenuItem::separator());
        let _ = app_sub.append(&PredefinedMenuItem::services(None));
        let _ = app_sub.append(&PredefinedMenuItem::separator());
        let _ = app_sub.append(&PredefinedMenuItem::hide(None));
        let _ = app_sub.append(&PredefinedMenuItem::hide_others(None));
        let _ = app_sub.append(&PredefinedMenuItem::show_all(None));
        let _ = app_sub.append(&PredefinedMenuItem::separator());
        let quit = MenuItem::with_id(
            MudaId::new(mtty_ui::menu::key(mtty_ui::chrome::MenuId::Quit)),
            "Quit mtty",
            true,
            Accelerator::from_str("CmdOrCtrl+Q").ok(),
        );
        let _ = app_sub.append(&quit);
        items.push(quit);
        if menu.append(&app_sub).is_err() {
            return None;
        }
        submenus.push(app_sub);
        for (title, entries) in mtty_ui::menu::menus(lang) {
            let sub = Submenu::new(title, true);
            for entry in entries {
                match entry {
                    mtty_ui::menu::Entry::Item {
                        label,
                        id,
                        shortcut,
                    } => {
                        let acc = shortcut.and_then(|s| Accelerator::from_str(s).ok());
                        let item = MenuItem::with_id(
                            MudaId::new(mtty_ui::menu::key(id)),
                            label,
                            true,
                            acc,
                        );
                        if sub.append(&item).is_err() {
                            return None;
                        }
                        items.push(item);
                    }
                    mtty_ui::menu::Entry::Separator => {
                        let _ = sub.append(&PredefinedMenuItem::separator());
                    }
                    mtty_ui::menu::Entry::Link { label, .. } => {
                        let item =
                            MenuItem::with_id(MudaId::new("documentation"), label, true, None);
                        if sub.append(&item).is_err() {
                            return None;
                        }
                        items.push(item);
                    }
                }
            }
            if menu.append(&sub).is_err() {
                return None;
            }
            submenus.push(sub);
        }
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let key = e.id.0.as_str();
            match mtty_ui::menu::from_key(key) {
                Some(id) => {
                    let _ = proxy.send_event(HostEvent::Menu(id, current_event_is_key()));
                }
                None => {
                    if key == "documentation" {
                        crate::open_external("https://github.com/oxdingzg/mtty#readme");
                    }
                }
            }
        }));
        menu.init_for_nsapp();
        Some(MenuHandle {
            _menu: menu,
            _items: items,
            _submenus: submenus,
        })
    }

    /// Whether the event AppKit is handling is a key press: a menu item
    /// fired by its shortcut rather than a click. Called from the menu's
    /// action, on the main thread.
    fn current_event_is_key() -> bool {
        use objc2::runtime::AnyObject;
        use objc2::{class, msg_send};
        const NS_EVENT_TYPE_KEY_DOWN: usize = 10;
        // SAFETY: NSApplication and -currentEvent / -type exist on every
        // supported macOS; nil results are checked before use.
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            if app.is_null() {
                return false;
            }
            let event: *mut AnyObject = msg_send![app, currentEvent];
            if event.is_null() {
                return false;
            }
            let kind: usize = msg_send![event, type];
            kind == NS_EVENT_TYPE_KEY_DOWN
        }
    }
}

/// Point `MTTY_CLI` (and the former `MIAOTTY_CLI`, read by installed hooks and
/// miao) at the CLI shipped beside this executable: inside an app bundle it is
/// not on `PATH`, and an inherited value may belong to another build.
/// Where the host writes a clipboard image for the pane's application to read
/// (ADR 0036). The clipboard is global, so every pane shares one path.
fn clipboard_image_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("mtty-clipboard-{}.png", std::process::id()))
}

fn same_file_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    let resolve = |path: &std::path::Path| {
        std::fs::canonicalize(path)
            .ok()
            .or_else(|| {
                Some(
                    std::fs::canonicalize(path.parent()?)
                        .ok()?
                        .join(path.file_name()?),
                )
            })
            .or_else(|| std::path::absolute(path).ok())
    };
    a == b || matches!((resolve(a), resolve(b)), (Some(a), Some(b)) if a == b)
}

fn export_pane_environment() {
    // A TUI cannot read the pasteboard directly (on macOS that means shelling
    // out to osascript, which Script Editor owns), so the host writes image
    // pastes here and the application reads the file (ADR 0036).
    std::env::set_var("MTTY_CLIPBOARD_FILE", clipboard_image_path());

    let cli = std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name(format!("mtty-cli{}", std::env::consts::EXE_SUFFIX)));
    let Some(cli) = cli.filter(|p| p.is_file()) else {
        return;
    };
    for name in ["MTTY_CLI", "MIAOTTY_CLI"] {
        std::env::set_var(name, &cli);
    }
}

/// Report a failure that keeps the window from opening and quit: stderr for
/// terminal launches, a dialog for everyone else.
fn startup_failure(event_loop: &ActiveEventLoop, what: &str, err: impl std::fmt::Display) {
    eprintln!("mtty: {what}: {err}");
    mtty_ui::agentloop::alert("mtty cannot start", &format!("{what}: {err}"));
    event_loop.exit();
}

/// Run a native terminal window until it is closed.
pub fn run(title: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
    // Single instance (ADR 0019): a later launch — a deep link or a second
    // `mtty <url>` — is handed to the running instance, which drains
    // its inbox, and this process exits without opening a window.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let intent = mtty_ui::launch::Intent::from_args(&args);
    if mtty_ui::launch::forward_to_running(&intent.encode()) {
        eprintln!("mtty: forwarded to the running instance");
        return Ok(());
    }

    // ADR 0032: carry the pre-rename config directory over once.
    match mtty_config::migrate_legacy_config() {
        Ok(true) => eprintln!("mtty: copied the former miaotty configuration"),
        Ok(false) => {}
        Err(e) => eprintln!("mtty: could not copy the former miaotty configuration: {e}"),
    }
    export_pane_environment();

    // MTP control plane (ADR 0005): the shell inherits `MTTY_SOCKET` (and the
    // former `MIAOTTY_SOCKET`), so `mtty-cli`, plugins and agent hooks use the
    // same control plane.
    let socket = mtty_mtp::default_socket();
    for name in ["MTTY_SOCKET", "MIAOTTY_SOCKET"] {
        std::env::set_var(name, &socket);
    }
    // MTTY_MTP_TOKEN (if set) requires it on every request; MTTY_MTP_ALLOW
    // (if set) restricts which capabilities are accepted.
    let mtp = mtty_mtp::ServerState::with_config(
        mtty_config::env("MTP_TOKEN"),
        mtty_mtp::ServerState::parse_allow(mtty_config::env("MTP_ALLOW")),
    );
    match mtty_mtp::serve(&socket, mtp.clone()) {
        Ok(()) => {
            eprintln!("mtty: MTP host on {}", socket.display());
            // Older CLIs default to the pre-rename socket path.
            #[cfg(unix)]
            if let Err(e) = mtty_mtp::link_legacy_socket(&socket, &mtty_mtp::legacy_socket()) {
                eprintln!("mtty: could not link the former socket path: {e}");
            }
        }
        Err(e) => eprintln!("mtty: MTP host failed: {e}"),
    }
    if let Some(addr) = mtty_config::Config::load().remote_listen {
        match mtty_mtp::serve_tcp(&addr, mtp.clone()) {
            Ok(()) => eprintln!("mtty: remote MTP access on {addr}"),
            Err(e) => eprintln!("mtty: remote access disabled: {e}"),
        }
    }

    let event_loop = EventLoop::<HostEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    #[cfg(target_os = "macos")]
    {
        let proxy = proxy.clone();
        macos_url::install(move || {
            let _ = proxy.send_event(HostEvent::Wake);
        });
    }
    // A large file's background parse wakes the loop to draw its colours.
    {
        let proxy = proxy.clone();
        mtty_editor::set_parse_waker(move || {
            let _ = proxy.send_event(HostEvent::Wake);
        });
    }
    // Let the control plane wake the loop, so `mtty-cli` commands apply
    // immediately even while the window is idle or unfocused.
    {
        let proxy = proxy.clone();
        mtp.set_waker(Arc::new(move || {
            let _ = proxy.send_event(HostEvent::Wake);
        }));
    }
    let mut host = Host {
        title: title.to_string(),
        proxy,
        mtp,
        state: None,
        #[cfg(target_os = "macos")]
        menu: None,
    };
    event_loop.run_app(&mut host)?;
    Ok(())
}

struct Host {
    title: String,
    proxy: EventLoopProxy<HostEvent>,
    mtp: Arc<mtty_mtp::ServerState>,
    state: Option<State>,
    /// The OS menu bar, kept alive for the whole run (ADR 0031).
    #[cfg(target_os = "macos")]
    menu: Option<appmenu::MenuHandle>,
}

struct Pane {
    id: String,
    term: Terminal,
    scroll: usize,
    /// What Enter types once, offered after a restore: the ssh command that
    /// reconnects the session, or the program that was running at quit.
    on_enter: Option<String>,
    /// The last command output already published to the control plane.
    published_output: Option<mtty_core::CommandOutput>,
    /// Inline images read for a restored pane, placed once the pane has its
    /// final width. Their anchors are grid lines, so placing them before the
    /// layout settles would leave them over reflowed text.
    pending_images: Option<PendingImages>,
    /// The "[mtty] Restored…" note, processed after the images are placed so
    /// the note's own rows do not become an image's anchor.
    pending_note: Option<String>,
}

/// A restored pane's saved images.
struct PendingImages {
    saved: mtty_core::graphics::SavedImages,
}

impl Pane {
    /// Feed queued output to the emulator. A scrolled-back viewport stays on
    /// the same lines (the emulator grows its display offset as lines enter
    /// history), so agents that redraw a spinner every second do not yank the
    /// reader back to the bottom; input, paste and clears still return there.
    /// Every drain goes through here so the grown offset is never overwritten
    /// by a stale `scroll`.
    fn drain_output(&mut self, selection: &mut Option<(String, Selection)>) -> bool {
        let alternate = self.term.screen().alternate_screen();
        self.term.screen_mut().set_scrollback(self.scroll);
        let output = self.term.process_pending();
        if output {
            self.scroll = self.term.screen().scroll_offset();
        }
        // Selection coordinates belong to one screen, not to the pane across
        // main/alternate screen switches. Never paint shell highlights over
        // a full-screen program (or restore its highlights over the shell).
        if alternate != self.term.screen().alternate_screen()
            && selection.as_ref().is_some_and(|(id, _)| id == &self.id)
        {
            *selection = None;
        }
        output
    }
}

/// Input for a pane with an offer pending (reconnect, run again): Enter types
/// the offered command; anything else means the user wants the shell as it
/// is, so the offer ends and the input passes through unchanged.
fn on_enter_input(pending: &mut Option<String>, bytes: &[u8]) -> Option<Vec<u8>> {
    let typed = pending.take()?;
    (bytes == b"\r").then(|| typed.into_bytes())
}

/// An ssh command as typed into a pane's shell: a leading space keeps it out
/// of history (ignorespace), and `clear` wipes the echoed command line, so the
/// pane starts with the remote session.
fn typed_ssh(cmd: &str) -> String {
    mtty_ui::ssh::Syntax::local().typed(cmd)
}

/// What a restored SSH tab does at startup: connect by itself when
/// `ssh-auto-reconnect` is on, otherwise wait for the user to press Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoredSsh {
    Auto,
    OnEnter,
}

fn restored_ssh_action(auto_reconnect: bool) -> RestoredSsh {
    if auto_reconnect {
        RestoredSsh::Auto
    } else {
        RestoredSsh::OnEnter
    }
}

/// How a non-SSH pane reaches its byte stream (ADR 0037). Saved in the session
/// so the tab reconnects on restore.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum TransportTarget {
    Telnet {
        host: String,
        port: u16,
    },
    Tcp {
        host: String,
        port: u16,
    },
    Serial {
        device: String,
        baud: u32,
        data_bits: u8,
        parity: String,
        stop_bits: u8,
        flow: String,
    },
}

impl TransportTarget {
    fn label(&self) -> String {
        match self {
            TransportTarget::Telnet { host, port } => format!("telnet {host}:{port}"),
            TransportTarget::Tcp { host, port } => format!("tcp {host}:{port}"),
            TransportTarget::Serial { device, baud, .. } => format!("{device} @{baud}"),
        }
    }

    /// Telnet and raw TCP are unencrypted; serial is a cable.
    fn plaintext(&self) -> bool {
        matches!(
            self,
            TransportTarget::Telnet { .. } | TransportTarget::Tcp { .. }
        )
    }
}

struct Tab {
    layout: Layout,
    panes: Vec<Pane>,
    active: String,
    title: String,
    /// The title was chosen (Rename Tab, an ssh target, Quick) rather than a
    /// default: it wins over view rules, the program title and the folder.
    title_set: bool,
    /// The title shown when the session was saved. On restore it holds the
    /// tab's place only while the pane has no live context (no cwd and no
    /// program title yet), so a restart shows the same title at first and the
    /// automatic one takes over as soon as the pane reports.
    shown: Option<String>,
    /// Opened as an ssh session (shows a server icon).
    ssh: bool,
    /// The ssh target as the user typed it, to reconnect after a restore.
    ssh_target: Option<String>,
    /// The full ssh command the tab ran (saved hosts carry -p / -J), used to
    /// reconnect or duplicate it.
    ssh_cmd: Option<String>,
    /// A serial, Telnet or raw TCP session (ADR 0037); `None` for a shell.
    transport: Option<TransportTarget>,
    /// Optional short prefix shown before the tab title.
    prefix: Option<String>,
    /// A short user marker appended to the tab title (ADR 0011).
    mark: Option<String>,
    /// The session-list group this tab belongs to (ADR 0011).
    group: Option<String>,
    /// Something happened here while it was in the background (B2.5).
    attention: Option<Attention>,
    /// Editor panes (ADR 0034); the layout's leaves name these, `panes` or
    /// `previews` by id.
    editors: Vec<editor_pane::EditorPane>,
    /// Markdown previews of editors in this tab (ADR 0034, E4).
    previews: Vec<PreviewPane>,
}

/// A Markdown preview of an editor pane in the same tab (ADR 0034, E4): the
/// editor's text as it is typed, rendered by egui inside the pane's card.
struct PreviewPane {
    id: String,
    /// The editor pane it shows.
    source: String,
    /// The editor's text at `revision` (taken again when it changes).
    text: Arc<String>,
    revision: Option<u64>,
}

impl PreviewPane {
    fn new(source: String) -> Self {
        PreviewPane {
            id: gen_id(),
            source,
            text: Arc::default(),
            revision: None,
        }
    }
}

/// Why a background tab wants a look, most urgent last (so `max` wins).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Attention {
    /// New output.
    Unread,
    /// Its agent finished (processing -> idle).
    Done,
    /// Its agent waits for input or failed.
    Needs,
}

impl Attention {
    fn marker(self) -> &'static str {
        match self {
            Attention::Unread => " \u{2022}",
            Attention::Done => " \u{eab2}", // cod-check (Symbols Nerd Font)
            Attention::Needs => " !",
        }
    }

    /// Whether the mark is shown on the tab the user is looking at. A
    /// finished agent is: watching the tab does not mean the user saw it
    /// finish, and the spinner's last frame looks much like the one before.
    fn shows_while_visible(self) -> bool {
        self == Attention::Done
    }

    /// What is left of `mark` once the user looks at the tab: the finished
    /// mark stays until they type into it or the agent starts again.
    fn seen(mark: Option<Self>) -> Option<Self> {
        mark.filter(|a| a.shows_while_visible())
    }

    /// The attention an agent state change asks for, if any.
    fn for_transition(previous: Option<&str>, now: &str) -> Option<Self> {
        match now {
            "awaiting" | "error" => Some(Attention::Needs),
            "completed" => Some(Attention::Done),
            "idle" if previous == Some("processing") => Some(Attention::Done),
            _ => None,
        }
    }
}

impl Tab {
    /// Whether a pane of any kind has this id.
    fn has_pane(&self, id: &str) -> bool {
        self.panes.iter().any(|p| p.id == id)
            || self.editors.iter().any(|e| e.id == id)
            || self.previews.iter().any(|p| p.id == id)
    }

    /// Drop an editor pane and the previews showing it (from the layout too).
    fn remove_editor(&mut self, id: &str) {
        self.editors.retain(|e| e.id != id);
        for preview in self.previews.iter().filter(|p| p.source == id) {
            let _ = self.layout.remove(&preview.id);
        }
        self.previews.retain(|p| p.source != id);
    }

    fn session_value(&self) -> serde_json::Value {
        let panes: Vec<_> = self
            .panes
            .iter()
            .map(|p| {
                serde_json::json!({ "id": p.id, "cwd": p.term.cwd(), "host": pane_host(&p.term) })
            })
            .collect();
        let editors: Vec<_> = self
            .editors
            .iter()
            .map(|e| {
                serde_json::json!({
                    "id": e.id, "path": e.path,
                    // The ssh destination when the file lives on a host.
                    "remote": e.remote.as_ref().map(|r| r.dest.clone()),
                    "cursor": e.doc.selection().primary().head,
                    "scroll": e.scroll_line,
                    // View mode: the caret's file line (the window moves).
                    "line": e.is_view_only().then(|| e.caret_line_col().0 - 1),
                })
            })
            .collect();
        let previews: Vec<_> = self
            .previews
            .iter()
            .map(|p| serde_json::json!({ "id": p.id, "source": p.source }))
            .collect();
        serde_json::json!({
            "title": self.title, "active": self.active,
            "layout": layout_to_json(&self.layout), "panes": panes, "editors": editors,
            "previews": previews,
            "prefix": self.prefix, "mark": self.mark, "group": self.group,
            "title_set": self.title_set,
            "ssh": self.ssh, "ssh_target": self.ssh_target, "ssh_cmd": self.ssh_cmd,
            "transport": self.transport,
        })
    }

    fn restore_decorations(&mut self, value: &serde_json::Value) {
        let text = |key| value.get(key).and_then(|v| v.as_str()).map(str::to_string);
        self.prefix = text("prefix");
        self.mark = text("mark");
        self.group = text("group");
    }
}

/// Remove the entire tab, not just its active pane. Keep the last tab alive
/// and preserve the focused tab when a background tab before it is removed.
fn remove_whole_tab(tabs: &mut Vec<Tab>, active: &mut usize, i: usize) -> bool {
    if tabs.len() <= 1 || i >= tabs.len() {
        return false;
    }
    tabs.remove(i);
    if *active > i {
        *active -= 1;
    }
    *active = (*active).min(tabs.len() - 1);
    true
}

fn views_mtime() -> Option<std::time::SystemTime> {
    let path = mtty_config::view::RuleSet::path()?;
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Chinese for the fixed words of the details panel (titles, keys and the
/// status values the workers produce); anything else is shown as is.
fn localize_detail(lang: mtty_ui::i18n::Lang, text: &str) -> &str {
    if lang == mtty_ui::i18n::Lang::En {
        return text;
    }
    match text {
        "Info" => "信息",
        "Agent" => "Agent",
        "Outline" => "大纲",
        "Git" => "Git",
        "Files" => "文件",
        "Ports" => "端口",
        "Title" => "标题",
        "Directory" => "目录",
        "Size" => "尺寸",
        "Pane" => "Pane",
        "branch" => "分支",
        "status" => "状态",
        "clean" => "无改动",
        "not a git repository" => "不是 git 仓库",
        "unavailable" => "不可用",
        "ports" => "端口",
        "no listeners" => "无监听端口",
        "lsof unavailable" => "lsof 不可用",
        "state" => "状态",
        "session_id" => "会话 ID",
        "quota" => "配额",
        "agent" => "Agent",
        "tty" => "终端设备",
        "File" => "文件",
        "Lines" => "行数",
        "Mode" => "模式",
        "View only" => "只读查看",
        "File size" => "文件大小",
        "Cursor" => "光标",
        "Line ending" => "换行符",
        "Language" => "语言",
        "Plain Text" => "纯文本",
        _ => text,
    }
}

/// An agent state as the details panel shows it. Only the Agent rows' state
/// values go through this: a tab or file could be named `idle` too.
fn agent_state_label(lang: mtty_ui::i18n::Lang, state: &str) -> &str {
    if lang == mtty_ui::i18n::Lang::En {
        return match state {
            "processing" => "Working",
            "idle" => "Idle",
            "awaiting" => "Waiting for you",
            "completed" => "Completed",
            "incomplete" => "Paused · unfinished",
            "waiting" => "Waiting for background work",
            "unknown" => "Status unavailable",
            "error" => "Failed",
            _ => state,
        };
    }
    match state {
        "processing" => "处理中",
        "idle" => "空闲",
        "awaiting" => "等待你",
        "error" => "出错",
        "completed" => "已完成",
        "incomplete" => "暂停 · 未完成",
        "waiting" => "等待后台任务",
        "unknown" => "状态未知",
        _ => state,
    }
}

/// A short "3m ago" for a millisecond timestamp; empty when unknown.
fn ago(ts_ms: f64) -> String {
    if ts_ms <= 0.0 {
        return String::new();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);
    let secs = ((now - ts_ms) / 1000.0).max(0.0) as u64;
    match secs {
        0..=4 => "just now".into(),
        5..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// One compact line for an agent-reported quota object (ADR 0042, A4):
/// `used/limit unit (window)`. Missing parts are dropped.
fn quota_line(q: &serde_json::Value, warn_at: u8) -> Option<String> {
    let num = |k: &str| {
        q.get(k).map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    };
    let used = num("used")?;
    let mut line = match num("limit") {
        Some(limit) => format!("{used}/{limit}"),
        None => used,
    };
    if let Some(unit) = q
        .get("unit")
        .and_then(|v| v.as_str())
        .filter(|u| !u.is_empty())
    {
        line.push(' ');
        line.push_str(unit);
    }
    if let Some(window) = q
        .get("window")
        .and_then(|v| v.as_str())
        .filter(|w| !w.is_empty())
    {
        line.push_str(&format!(" ({window})"));
    }
    let warn = match (
        q.get("used").and_then(serde_json::Value::as_f64),
        q.get("limit").and_then(serde_json::Value::as_f64),
    ) {
        (Some(used), Some(limit)) if limit > 0.0 && used / limit >= f64::from(warn_at) / 100.0 => {
            "⚠ "
        }
        _ => "",
    };
    Some(format!("{warn}{line}"))
}

/// The host part of an ssh target as typed (`deploy@work:2200` → `work`).
fn ssh_host(target: &str) -> String {
    mtty_ui::ssh::Target::parse(target)
        .map(|t| t.host)
        .unwrap_or_else(|| target.to_string())
}

/// A floating window inside mtty: resizable both ways, kept on screen and no
/// larger than it. Pair it with [`window_body`] so wide content (long lines,
/// code blocks, full-width fields) scrolls instead of stretching the window
/// to a size it can then no longer be dragged below.
/// The cell under a logical point inside a pane's inner rect, clamped to it.
fn pane_cell_at(inner: Rect, cw: f32, ch: f32, at: (f32, f32)) -> Option<(u16, u16)> {
    if cw <= 0.0 || ch <= 0.0 || !inner.contains(at.0, at.1) {
        return None;
    }
    let col = ((at.0 - inner.x) / cw).floor().max(0.0) as u16;
    let row = ((at.1 - inner.y) / ch).floor().max(0.0) as u16;
    Some((row, col))
}

fn app_window<'a>(title: impl Into<egui::WidgetText>, ctx: &egui::Context) -> egui::Window<'a> {
    let screen = ctx.screen_rect();
    egui::Window::new(title)
        .resizable(true)
        .constrain(true)
        .default_size([640.0, 480.0])
        .max_size((screen.size() - egui::vec2(24.0, 24.0)).max(egui::vec2(200.0, 120.0)))
}

/// A dialog's right-aligned button row, one row high. A bare
/// `with_layout(right_to_left(..))` in an auto-sized window takes all the
/// height left on screen and centres its buttons in it, stretching the window
/// to the screen's height (Software Update did).
fn button_row<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add)
            .inner
    })
    .inner
}

/// The scrolling content area of an [`app_window`].
fn window_body<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, add)
        .inner
}

fn t_lang(lang: mtty_ui::i18n::Lang, en: &'static str, zh: &'static str) -> &'static str {
    mtty_ui::i18n::t(lang, en, zh)
}

/// The palette label for launching an agent.
fn launch_label(lang: mtty_ui::i18n::Lang, agent: &str) -> &'static str {
    use mtty_ui::i18n::t;
    match agent {
        "claude" => t(lang, "Launch claude", "启动 claude"),
        "codex" => t(lang, "Launch codex", "启动 codex"),
        "opencode" => t(lang, "Launch opencode", "启动 opencode"),
        "miao" => t(lang, "Launch miao", "启动 miao"),
        _ => t(lang, "Launch agent", "启动 agent"),
    }
}

/// The split divider under a pointer given in physical pixels, if any.
fn divider_at(
    handles: Vec<mtty_ui::layout::Handle>,
    px: f32,
    py: f32,
    scale: f32,
) -> Option<mtty_ui::layout::Handle> {
    handles.into_iter().find(|h| {
        px >= h.rect.x * scale
            && px < (h.rect.x + h.rect.w) * scale
            && py >= h.rect.y * scale
            && py < (h.rect.y + h.rect.h) * scale
    })
}

/// The split ratio while dragging a divider of `dir` across `area` (logical
/// points) to a pointer in physical pixels. `Layout::set_ratio` clamps it.
fn divider_ratio(dir: SplitDir, area: Rect, px: f32, py: f32, scale: f32) -> f32 {
    match dir.axis() {
        SplitDir::Right => (px / scale - area.x) / area.w.max(1.0),
        SplitDir::Down => (py / scale - area.y) / area.h.max(1.0),
        SplitDir::Left | SplitDir::Up => unreachable!("axis() yields only Right/Down"),
    }
}

/// What a modal text dialog did this frame.
#[derive(Debug, PartialEq, Eq)]
enum DialogOutcome {
    Open,
    Commit,
    Cancel,
}

/// The Rename Tab dialog: Enter or the button commits, Escape or the close
/// box cancels. A free function so it can be driven by replayed input.
fn rename_dialog(
    ctx: &egui::Context,
    lang: mtty_ui::i18n::Lang,
    buf: &mut String,
) -> DialogOutcome {
    let mut open = true;
    let mut commit = false;
    egui::Window::new(mtty_ui::i18n::t(lang, "Rename Tab", "重命名标签"))
        .id(egui::Id::new("rename_tab_dialog"))
        .collapsible(false)
        .open(&mut open)
        .show(ctx, |ui| {
            let resp = ui.add(egui::TextEdit::singleline(buf).id(egui::Id::new("rename_tab_text")));
            // Check Enter before re-taking focus: Enter makes the field give it up,
            // and taking it back first would hide that (`lost_focus` stays false).
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if !enter {
                resp.request_focus();
            }
            if enter {
                commit = true;
            }
            if ui
                .button(mtty_ui::i18n::t(lang, "Rename", "重命名"))
                .clicked()
            {
                commit = true;
            }
        });
    if commit {
        DialogOutcome::Commit
    } else if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        DialogOutcome::Cancel
    } else {
        DialogOutcome::Open
    }
}

/// The automatic title new tabs get (`shell 3`), as opposed to a chosen one.
/// The tab title that wins over the automatic ones: a name the user chose
/// (Rename Tab, an ssh target, Quick), or the title restored from the last
/// session so a restart shows the same title. `None` means fall through to the
/// automatic title (view rules, program title, cwd, `shell N`).
fn fixed_tab_title(title_set: bool, title: &str, shown: Option<&str>) -> Option<String> {
    if title_set && !title.is_empty() {
        return Some(title.to_string());
    }
    shown.filter(|s| !s.is_empty()).map(str::to_string)
}

/// The title a restored tab shows before its pane reports anything: the one
/// saved last session, so the restart does not flash `shell N`. Once the pane
/// has live context — its cwd or a program title — the saved value must yield,
/// or a stale saved title would freeze the tab for good and cd, agents and
/// program titles would never show.
fn restored_title(
    shown: Option<&str>,
    cwd: Option<&str>,
    osc_title: Option<&str>,
) -> Option<String> {
    if cwd.is_some() || osc_title.is_some() {
        return None;
    }
    shown.filter(|s| !s.is_empty()).map(str::to_string)
}

fn is_default_title(title: &str) -> bool {
    title
        .strip_prefix("shell ")
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// Remove every tab except `keep` (Close Other Tabs); returns the removed
/// tabs in their original order, for the reopen stack.
fn take_other_tabs(tabs: &mut Vec<Tab>, keep: usize) -> Vec<Tab> {
    if keep >= tabs.len() {
        return Vec::new();
    }
    let kept = tabs.remove(keep);
    std::mem::replace(tabs, vec![kept])
}

/// Remove the tabs after `i` (Close Tabs Below); returns them in order.
fn take_tabs_below(tabs: &mut Vec<Tab>, i: usize) -> Vec<Tab> {
    if i + 1 >= tabs.len() {
        return Vec::new();
    }
    tabs.split_off(i + 1)
}

/// The directory a new tab or split starts in: the active pane's, unless that
/// pane is an ssh session (its cwd is remote) or the directory is gone.
fn inherited_cwd(ssh: bool, cwd: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    if ssh {
        return None;
    }
    cwd.filter(|dir| dir.is_dir())
}

/// Background-computed details (git status / directory listing / ports).
#[derive(Default, Clone)]
struct DetailsData {
    git: Vec<(String, String)>,
    files: Vec<FileEntry>,
    ports: Vec<(String, String)>,
}

/// One entry in the Files panel.
#[allow(dead_code)]
#[derive(Clone)]
struct FileEntry {
    name: String,
    is_dir: bool,
    size: u64,
}

/// A simple built-in text file editor (with a naive Markdown preview).
struct Editor {
    path: std::path::PathBuf,
    text: String,
    original: String,
    preview: bool,
    /// Opened via MTP `app.view`: shown without an editable text field.
    readonly: bool,
    /// `(destination, remote path)` when editing a file over ssh.
    remote: Option<(String, String)>,
    /// Closing with unsaved changes was requested once; the next close discards.
    close_armed: bool,
    /// A remote save is running in the background.
    saving: bool,
    /// Close once the running remote save succeeds (vim `:wq`).
    quit_after_save: bool,
}

/// Work finished on a background thread. Network and process work never runs
/// on the UI thread; the result comes back through `State::jobs_rx`.
enum JobDone {
    /// Bytes read over ssh. `id` is `Some` when the pane already exists
    /// (session restore); `None` opens a new editor pane (remote dialog).
    RemoteRead {
        id: Option<String>,
        cursor: usize,
        scroll: usize,
        dest: String,
        path: String,
        result: std::io::Result<Vec<u8>>,
    },
    DirListed {
        dir: std::path::PathBuf,
        entries: Vec<FileEntry>,
    },
    AgentStatus(mtty_ui::hostkeys::Agent),
    SftpListed {
        dir: String,
        result: Result<Vec<mtty_ui::sftp::RemoteEntry>, String>,
    },
    /// A verified update download, or why it failed (B4.3).
    UpdateDownloaded(Result<std::path::PathBuf, String>),
    /// A sync finished (B4.5).
    Synced(Result<mtty_config::sync::Outcome, String>),
    SftpLocalListed {
        dir: std::path::PathBuf,
        entries: Vec<FileEntry>,
    },
    /// An SFTP operation finished; both sides are listed again.
    SftpDone {
        label: String,
        result: Result<(), String>,
    },
    HostKeyChecked {
        name: String,
        result: Result<mtty_ui::hostkeys::HostKey, String>,
    },
    TaskCreated {
        result: Result<mtty_ui::tasks::Task, String>,
        agent: Option<usize>,
    },
    TasksListed {
        repo: std::path::PathBuf,
        result: Result<Vec<mtty_ui::tasks::Task>, String>,
    },
    TaskDiff {
        name: String,
        result: Result<String, String>,
    },
    /// A merge (`merged`) or discard finished; the window then reloads.
    TaskDone {
        name: String,
        merged: bool,
        result: Result<String, String>,
    },
    RemoteWrite {
        /// `Some` for an editor pane, `None` for the floating editor.
        id: Option<String>,
        dest: String,
        path: String,
        /// The bytes as written; edits made meanwhile stay "modified".
        text: Vec<u8>,
        result: std::io::Result<()>,
    },
    /// A remote editor pane's length/mtime, probed over ssh in the
    /// background; `None` when the probe failed (a silent no-op).
    RemotePolled {
        id: String,
        stamp: Option<editor_pane::DiskStamp>,
    },
    /// Bytes of a remote editor pane's file, read in the background after the
    /// stamp changed, ready to reload if it still has no unsaved edits.
    RemoteReloaded {
        id: String,
        stamp: editor_pane::DiskStamp,
        result: std::io::Result<Vec<u8>>,
    },
    /// A serial/Telnet/TCP connection finished dialling (ADR 0037).
    TransportConnected {
        target: TransportTarget,
        /// A restored tab's title, else the connection's own label.
        title: Option<String>,
        result: Result<mtty_ui::transport::Connection, String>,
    },
    /// A PuTTY key was imported and written, or why it was not (ADR 0038).
    /// The `Ok` value is the path written.
    KeyImported(Result<String, String>),
    /// Streamed text from an ACP agent (ADR 0040, A2).
    AcpUpdate(String),
    /// An ACP agent's diff: replace `path` with the agent's new text, as an
    /// A1 proposal.
    AcpDiff {
        path: String,
        text: String,
    },
    AcpRead {
        path: String,
        reply: std::sync::mpsc::Sender<Option<String>>,
    },
    AcpWrite {
        path: String,
        text: String,
        reply: std::sync::mpsc::Sender<bool>,
    },
    AcpTerminal {
        id: String,
        output: String,
    },
    /// A response to one of our ACP requests.
    AcpResponse {
        id: u64,
        result: serde_json::Value,
        error: Option<serde_json::Value>,
    },
    /// An ACP agent asks to do something: answer `reply` (the agent waits).
    AcpPermission {
        question: String,
        reply: std::sync::mpsc::Sender<bool>,
    },
}

/// The Agent Tasks window: the repository, its tasks and a pending
/// confirmation for a destructive action.
struct TasksView {
    repo: std::path::PathBuf,
    tasks: Vec<mtty_ui::tasks::Task>,
    loading: bool,
    /// Index into `tasks` and whether it is a merge (else a discard).
    confirm: Option<(usize, bool)>,
}

/// End-to-end-encrypted sync of hosts and snippets (B4.5, ADR 0033).
struct SyncState {
    dir: Option<std::path::PathBuf>,
    key: Option<mtty_config::sync::Key>,
    running: bool,
    /// When the next background sync is due.
    due: Option<Instant>,
    last: Option<(
        std::time::SystemTime,
        Result<mtty_config::sync::Outcome, String>,
    )>,
}

/// The Sync window's form.
#[derive(Default)]
struct SyncView {
    dir: String,
    pairing: String,
    show_code: bool,
    error: Option<String>,
}

/// The Import PuTTY Key form (ADR 0038).
#[derive(Default)]
struct KeyImportDialog {
    /// The `.ppk` to read.
    path: String,
    /// The output name under `~/.ssh` (the `.ppk` stem when empty).
    name: String,
    /// The PPK's own passphrase, if it is encrypted.
    old: String,
    new: String,
    confirm: String,
    overwrite: bool,
    error: Option<String>,
}

/// A running ACP session (ADR 0040, A2): the client, its session id and the
/// transcript shown in the window.
struct AcpSession {
    client: mtty_acp::Client,
    cwd: String,
    session: Option<String>,
    transcript: String,
    prompt: String,
    status: String,
    initialize_id: u64,
    authenticate_id: Option<u64>,
    connect_id: Option<u64>,
    auth_method: Option<String>,
    auth_methods: Vec<(String, String)>,
    resume_id: Option<String>,
    load_supported: bool,
    terminals: std::collections::BTreeMap<String, String>,
}

/// The ACP start form: a configured agent, or a typed command.
#[derive(Default)]
struct AcpStart {
    index: usize,
    command: String,
    session_id: String,
    error: Option<String>,
}

/// Bridges an ACP agent's callbacks to the UI thread (ADR 0040, A2).
struct AcpBridge {
    /// `Mutex` because the handler must be `Sync`; sends are cheap.
    tx: std::sync::Mutex<std::sync::mpsc::Sender<JobDone>>,
    proxy: EventLoopProxy<HostEvent>,
}

impl AcpBridge {
    fn new(tx: std::sync::mpsc::Sender<JobDone>, proxy: EventLoopProxy<HostEvent>) -> Self {
        AcpBridge {
            tx: std::sync::Mutex::new(tx),
            proxy,
        }
    }

    fn send(&self, done: JobDone) {
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(done);
        }
        let _ = self.proxy.send_event(HostEvent::Wake);
    }
}

impl mtty_acp::Handler for AcpBridge {
    fn on_update(&self, _session: &str, update: &serde_json::Value) {
        if let Some((path, text)) = acp_diff(update) {
            self.send(JobDone::AcpDiff { path, text });
        }
        if let Some(text) = acp_update_text(update) {
            self.send(JobDone::AcpUpdate(text));
        }
    }
    fn on_response(&self, id: u64, result: &serde_json::Value, error: Option<&serde_json::Value>) {
        self.send(JobDone::AcpResponse {
            id,
            result: result.clone(),
            error: error.cloned(),
        });
    }
    fn on_error(&self, message: &str) {
        self.send(JobDone::AcpUpdate(format!("\n[error] {message}\n")));
    }
    fn read_text_file(&self, path: &str) -> Option<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(JobDone::AcpRead {
            path: path.into(),
            reply: tx,
        });
        rx.recv_timeout(std::time::Duration::from_secs(30))
            .ok()
            .flatten()
            .or_else(|| std::fs::read_to_string(path).ok())
    }
    fn write_text_file(&self, path: &str, content: &str) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(JobDone::AcpWrite {
            path: path.into(),
            text: content.into(),
            reply: tx,
        });
        // Success means the user accepted and the file actually reached disk.
        // The UI owns the sender, so closing the pane/app also ends the wait.
        rx.recv().unwrap_or(false)
    }
    fn on_terminal_output(&self, _session: &str, id: &str, output: &str) {
        self.send(JobDone::AcpTerminal {
            id: id.into(),
            output: output.into(),
        });
    }
    fn request_permission(&self, request: &serde_json::Value) -> bool {
        let question = acp_permission_text(request);
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(JobDone::AcpPermission {
            question,
            reply: tx,
        });
        // The agent waits for the answer; this runs on its reader thread.
        rx.recv().unwrap_or(false)
    }
}

/// A `session/update`'s text, for the transcript.
fn acp_update_text(update: &serde_json::Value) -> Option<String> {
    let text = |content: &serde_json::Value| -> Option<String> {
        match content.get("type").and_then(|v| v.as_str()) {
            Some("text") => content
                .get("text")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            _ => None,
        }
    };
    match update.get("sessionUpdate").and_then(|v| v.as_str())? {
        "agent_message_chunk" => update.get("content").and_then(text),
        "agent_thought_chunk" => update
            .get("content")
            .and_then(text)
            .map(|t| format!("[thinking] {t}")),
        "tool_call" => {
            let title = update
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("tool");
            let status = update.get("status").and_then(|v| v.as_str()).unwrap_or("");
            Some(format!("\n[tool] {title} {status}\n"))
        }
        "tool_call_update" => {
            let title = update.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let status = update.get("status").and_then(|v| v.as_str()).unwrap_or("");
            (!title.is_empty() || !status.is_empty())
                .then(|| format!("\n[tool] {title} {status}\n"))
        }
        _ => None,
    }
}

/// A diff in a `session/update`: the file and the agent's new text.
fn acp_diff(update: &serde_json::Value) -> Option<(String, String)> {
    let contents = update.get("content")?;
    let items: Vec<&serde_json::Value> = match contents {
        serde_json::Value::Array(a) => a.iter().collect(),
        other => vec![other],
    };
    for item in items {
        if item.get("type").and_then(|v| v.as_str()) != Some("diff") {
            continue;
        }
        if let (Some(path), Some(text)) = (
            item.get("path").and_then(|v| v.as_str()),
            item.get("newText").and_then(|v| v.as_str()),
        ) {
            return Some((path.to_string(), text.to_string()));
        }
    }
    None
}

/// A one-line description of a permission request, for the dialog.
fn acp_permission_text(request: &serde_json::Value) -> String {
    if let Some(title) = request.pointer("/toolCall/title").and_then(|v| v.as_str()) {
        return format!("Allow: {title}?");
    }
    if let Some(options) = request.get("options").and_then(|v| v.as_array()) {
        let names: Vec<&str> = options
            .iter()
            .filter_map(|o| o.get("name").and_then(|v| v.as_str()))
            .collect();
        if !names.is_empty() {
            return format!("Allow: {}?", names.join(" / "));
        }
    }
    "Allow this action?".to_string()
}

/// The New Serial/Telnet/TCP Session form (ADR 0037).
struct TransportDialog {
    /// 0 serial, 1 Telnet, 2 TCP.
    kind: usize,
    host: String,
    port: String,
    device: String,
    baud: String,
    /// Index into 5/6/7/8.
    data_bits: usize,
    /// 0 none, 1 odd, 2 even.
    parity: usize,
    /// 0 one, 1 two.
    stop_bits: usize,
    /// 0 none, 1 software, 2 hardware.
    flow: usize,
    error: Option<String>,
}

impl Default for TransportDialog {
    fn default() -> Self {
        TransportDialog {
            kind: 0,
            host: String::new(),
            port: String::new(),
            device: String::new(),
            baud: "115200".into(),
            data_bits: 3,
            parity: 0,
            stop_bits: 0,
            flow: 0,
            error: None,
        }
    }
}

/// The New SSH Session form: a full host editor rather than a bare
/// `[user@]host[:port]` prompt, with a quick-connect fast path.
struct SshForm {
    /// `[user@]host[:port]` fast path; connects without saving.
    quick: String,
    name: String,
    alias: bool,
    address: String,
    user: String,
    port: String,
    group: String,
    tags: String,
    jump: String,
    persistent: bool,
    tmux: String,
    mosh: bool,
    forwards: Vec<mtty_config::hosts::Forward>,
    new_forward: Option<(mtty_config::hosts::ForwardKind, String)>,
    advanced: bool,
    error: Option<String>,
}

impl Default for SshForm {
    fn default() -> Self {
        SshForm {
            quick: String::new(),
            name: String::new(),
            alias: false,
            address: String::new(),
            user: String::new(),
            port: String::new(),
            group: String::new(),
            tags: String::new(),
            jump: String::new(),
            // A persistent session is the sensible default for a named host.
            persistent: true,
            tmux: String::new(),
            mosh: false,
            forwards: Vec::new(),
            new_forward: None,
            advanced: false,
            error: None,
        }
    }
}

impl SshForm {
    /// Build a [`Host`](mtty_config::hosts::Host) from the form, or the
    /// reason it is not ready yet.
    fn build(&self, lang: mtty_ui::i18n::Lang) -> Result<mtty_config::hosts::Host, String> {
        use mtty_ui::i18n::t;
        let name = self.name.trim().to_string();
        let address = self.address.trim().to_string();
        let opt = |s: &str| {
            let v = s.trim();
            (!v.is_empty()).then(|| v.to_string())
        };
        if self.alias {
            if name.is_empty() {
                return Err(t(
                    lang,
                    "A name is required for an alias.",
                    "别名需要一个名称。",
                )
                .into());
            }
            return Ok(mtty_config::hosts::Host {
                name,
                alias: true,
                ..Default::default()
            });
        }
        let host = if !address.is_empty() {
            address
        } else if !name.is_empty() {
            name.clone()
        } else {
            return Err(t(lang, "Host or name is required.", "需要填写主机或名称。").into());
        };
        let port = match self.port.trim() {
            "" => None,
            p => Some(p.parse::<u16>().map_err(|_| {
                t(lang, "Port must be 0–65535.", "端口需在 0–65535 之间。").to_string()
            })?),
        };
        Ok(mtty_config::hosts::Host {
            name: if name.is_empty() { host.clone() } else { name },
            alias: false,
            address: Some(host),
            user: opt(&self.user),
            port,
            group: opt(&self.group),
            tags: self
                .tags
                .split([',', ' '])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            jump: opt(&self.jump),
            tmux: self
                .persistent
                .then(|| opt(&self.tmux).unwrap_or_else(|| "mtty".into())),
            mosh: self.mosh,
            forwards: self.forwards.clone(),
            kind: Default::default(),
            serial: None,
        })
    }
}

/// Connect to an FTP/FTPS server. The password stays in memory only.
#[derive(Default)]
struct FtpDialog {
    address: String,
    password: String,
    /// 0 plain, 1 explicit TLS, 2 implicit TLS.
    security: usize,
    insecure: bool,
    error: Option<String>,
}

/// The Snippets window: search, the add form, hosts picked for a run.
#[derive(Default)]
struct SnippetsView {
    filter: String,
    form: mtty_config::snippets::Snippet,
    form_tags: String,
    /// Saved host names to run the chosen snippet on.
    hosts: std::collections::BTreeSet<String>,
    confirm_delete: Option<usize>,
}

/// An ssh command that runs `command` on a host in a terminal tab (its
/// output stays visible; the tab returns to the local shell afterwards).
fn remote_run_command(destination: &str, options: &[String], command: &str) -> String {
    remote_run_command_with(mtty_ui::ssh::Syntax::local(), destination, options, command)
}

/// The same for a given local shell syntax (tests pin POSIX).
fn remote_run_command_with(
    syn: mtty_ui::ssh::Syntax,
    destination: &str,
    options: &[String],
    command: &str,
) -> String {
    let mut cmd = String::from("ssh -t");
    for option in options {
        cmd.push(' ');
        cmd.push_str(&syn.quote(option));
    }
    cmd.push(' ');
    cmd.push_str(&syn.quote(destination));
    cmd.push(' ');
    cmd.push_str(&syn.quote(command));
    cmd
}

/// The SFTP window (B3.4): this machine on the left, the host on the right.
struct SftpView {
    title: String,
    remote: mtty_ui::sftp::Endpoint,
    remote_dir: Option<String>,
    remote_entries: Vec<mtty_ui::sftp::RemoteEntry>,
    remote_sel: Option<String>,
    local_dir: std::path::PathBuf,
    local_entries: Vec<FileEntry>,
    local_sel: Option<String>,
    /// A running operation, and for downloads the local file and its size
    /// so progress can be read off the disk.
    busy: Option<(String, Option<(std::path::PathBuf, u64)>)>,
    error: Option<String>,
    /// Inline editors: rename (new name), chmod (mode), new folder (name).
    rename: Option<String>,
    chmod: Option<String>,
    new_folder: Option<String>,
    confirm_delete: bool,
    /// Show dot files on both sides (hidden by default, as in the Files tree).
    show_hidden: bool,
    /// Where the window was drawn, so files dropped on it are uploaded.
    rect: egui::Rect,
}

/// The Hosts window: search, a pending delete and the add-host form.
#[derive(Default)]
struct HostsView {
    filter: String,
    confirm_delete: Option<usize>,
    form: mtty_config::hosts::Host,
    form_port: String,
    /// The ssh agent, checked when the window opens (B3.2).
    agent: Option<mtty_ui::hostkeys::Agent>,
    /// The add-forward form: host name, kind and spec.
    new_forward: Option<(String, mtty_config::hosts::ForwardKind, String)>,
    /// Host key checks by host name: `None` while running.
    keys: HashMap<String, Option<Result<mtty_ui::hostkeys::HostKey, String>>>,
}

/// What a save request led to.
#[derive(Debug, PartialEq, Eq)]
enum SaveOutcome {
    Saved,
    Failed,
    /// A remote write is running; its result arrives as a [`JobDone`].
    Pending,
}

impl Editor {
    /// Write a local buffer; it is marked clean only on success. Remote buffers
    /// are written in the background (see `State::save_editor`).
    fn write(&mut self) -> std::io::Result<()> {
        if self.readonly {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "read-only",
            ));
        }
        if self.remote.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "remote files are written in the background",
            ));
        }
        std::fs::write(&self.path, &self.text)?;
        self.mark_saved(self.text.clone());
        Ok(())
    }

    /// Record that `written` reached the file.
    fn mark_saved(&mut self, written: String) {
        self.original = written;
        self.close_armed = false;
        self.saving = false
    }

    /// Whether a close may proceed. Unsaved changes arm the first request and
    /// let the second one discard them.
    fn may_close(&mut self) -> bool {
        if self.readonly || self.text == self.original || self.close_armed {
            return true;
        }
        self.close_armed = true;
        false
    }
}

/// The inline-image layer for the debug capture (quads + the pane scissor).
struct ImageLayer<'a> {
    quads: &'a [(u64, i32, u32, ImageInstance)],
    rects: &'a [(String, Rect)],
    scale: f32,
}

struct PaneDraw {
    id: String,
    rect: Rect,
    quads: Vec<Quad>,
    rows: Vec<Vec<Span>>,
}

/// An open pane context menu: where it is, which pane it targets, and the line
/// under it (for "About This Line").
#[derive(Clone)]
struct PaneMenu {
    pane: String,
    /// The screen point (logical) the menu opens at.
    at: (f32, f32),
    /// The row/column the click landed on, if any.
    cell: Option<(u16, u16)>,
}

/// One item of the pane context menu, mapped to a command when run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PaneMenuAction {
    Copy,
    Paste,
    CopyAnsi,
    PasteEscaped,
    Composer,
    SendToAgent,
    SelectAll,
    Search,
    SplitRight,
    SplitLeft,
    SplitDown,
    SplitUp,
    ClearScrollback,
}

/// Keep a context menu on screen: shift it left/up when it would run past the
/// bottom-right, leaving a small margin.
fn clamp_menu_pos(ctx: &egui::Context, at: (f32, f32)) -> (f32, f32) {
    let screen = ctx.screen_rect();
    let (w, h) = (220.0, 320.0);
    let x = at.0.min(screen.max.x - w).max(screen.min.x);
    let y = at.1.min(screen.max.y - h).max(screen.min.y);
    (x, y)
}

struct State {
    window: Arc<Window>,
    proxy: EventLoopProxy<HostEvent>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    quads: QuadRenderer,
    images: ImageRenderer,
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    pip: Option<Pip>,
    pip_request: bool,
    graphics_enabled: bool,
    renderers: HashMap<String, TermRenderer>,
    mtp: Arc<mtty_mtp::ServerState>,
    tabs: Vec<Tab>,
    active_tab: usize,
    theme: Theme,
    rules: mtty_config::view::RuleSet,
    /// `views.json`'s modification time when last loaded, and when it was
    /// last checked: edits apply without a restart.
    rules_mtime: Option<std::time::SystemTime>,
    rules_checked: Instant,
    cw: f32,
    ch: f32,
    font_size: f32,
    default_font_size: f32,
    line_ratio: f32,
    font_family: Option<String>,
    lang: mtty_ui::i18n::Lang,
    mods: ModifiersState,
    selection: Option<(String, Selection)>,
    /// A pane context menu open at a screen point, for a specific pane.
    pane_menu: Option<PaneMenu>,
    dragging: bool,
    /// A file drag is hovering the window. The drop target is painted so the
    /// destination is visible before the file is released.
    dropping: bool,
    /// What is being dragged (unknown on Wayland until the drop).
    drag_paths: Vec<std::path::PathBuf>,
    /// The pointer is known while dragging (see `drag`): the open band is
    /// offered.
    drag_live: bool,
    /// Next native drag pointer poll; repainting must not itself poll again.
    drag_wake: Option<Instant>,
    /// What the current drop does, decided at its first file so a drop of
    /// several is handled alike (opening one changes the tab under the
    /// pointer).
    drop_choice: Option<(drag::DropAction, Option<String>)>,
    /// QA: a drag set up by `MTTY_QA_DRAG` (its point is fixed and Alt is
    /// as given, not read from the system).
    qa_drag: Option<bool>,
    divider_drag: Option<(Vec<bool>, SplitDir, Rect)>,
    /// Mouse button currently forwarded to the application (0/1/2), if any.
    mouse_captured: Option<u8>,
    cursor: (f64, f64),
    /// File drops on native Wayland (winit handles X11's).
    #[cfg(all(unix, not(target_os = "macos")))]
    dnd: Option<wayland_dnd::Dnd>,
    /// Inline IME composition text (not yet committed to the shell).
    preedit: String,
    /// Where the IME candidate window was last anchored (logical points).
    /// The candidate window's anchor in points while no egui text field has
    /// focus (the terminal cursor or editor caret).
    ime_area: Option<egui::Rect>,
    show_sidebar: bool,
    show_details: bool,
    renaming: Option<usize>,
    rename_buf: String,
    theme_name: String,
    show_palette: bool,
    palette_query: String,
    palette_idx: usize,
    show_settings: bool,
    editor: Option<Editor>,
    open_path: String,
    show_open: bool,
    recipe_dialog: Option<bool>,
    recipe_name: String,
    recipe_list: Vec<String>,
    ssh_dialog: Option<SshForm>,
    /// New Serial/Telnet/TCP session form (ADR 0037); `None` when closed.
    transport_dialog: Option<TransportDialog>,
    /// Import PuTTY Key form (ADR 0038); `None` when closed.
    key_import: Option<KeyImportDialog>,
    /// A running ACP agent session (ADR 0040, A2).
    acp: Option<AcpSession>,
    acp_writes: HashMap<String, (u64, std::sync::mpsc::Sender<bool>)>,
    /// The ACP start form, if open.
    acp_start: Option<AcpStart>,
    /// A permission request the agent is waiting on.
    acp_permission: Option<(String, std::sync::mpsc::Sender<bool>)>,
    /// ACP agents from `config.toml`'s `[acp]` (ADR 0040, A2).
    acp_agents: Vec<mtty_config::AcpAgent>,
    /// Connections being dialled: an all-transport restore is not "empty".
    pending_transport_connects: usize,
    /// New Agent Task dialog: name and the chosen agent (B2.4).
    task_dialog: Option<(String, Option<usize>)>,
    /// The Agent Tasks window.
    tasks_view: Option<TasksView>,
    /// Saved SSH hosts (B3.1). `host_book_error` blocks saving over a
    /// hosts.toml that could not be read.
    host_book: mtty_config::hosts::HostBook,
    host_book_error: Option<String>,
    hosts_view: Option<HostsView>,
    sftp_view: Option<SftpView>,
    /// Typed input goes to every pane of the active tab (B3.5).
    broadcast: bool,
    /// Saved command snippets (B3.5); an unreadable file blocks saving.
    snippet_book: mtty_config::snippets::SnippetBook,
    snippet_book_error: Option<String>,
    snippets_view: Option<SnippetsView>,
    /// Running port forwards by (host name, rule spec) (B3.3).
    tunnels: HashMap<(String, String), mtty_ui::forward::Tunnel>,
    remote_dialog: Option<(String, String)>,
    ftp_dialog: Option<FtpDialog>,
    sync: SyncState,
    sync_view: Option<SyncView>,
    editor_vim: bool,
    /// The `editor` config key: what Edit in Tab runs (ADR 0017).
    editor_command: Option<String>,
    vim: Option<mtty_ui::vim::VimRuntime>,
    vim_for: String,
    cmark: egui_commonmark::CommonMarkCache,
    mmd: Mmd,
    recent_files: Vec<String>,
    open_counts: HashMap<String, u32>,
    integration_msg: Option<String>,
    read_only: bool,
    hint_mode: bool,
    hints: Vec<mtty_ui::hints::Hint>,
    tree_expanded: std::collections::HashSet<std::path::PathBuf>,
    tree_children: HashMap<std::path::PathBuf, Vec<FileEntry>>,
    /// Directories being listed in the background.
    tree_loading: std::collections::HashSet<std::path::PathBuf>,
    files_filter: String,
    prefix_renaming: Option<usize>,
    prefix_buf: String,
    mark_renaming: Option<usize>,
    mark_buf: String,
    group_renaming: Option<usize>,
    group_buf: String,
    hotkeys: Option<mtty_ui::hotkey::Hotkeys>,
    opacity: f32,
    notifications: bool,
    prevent_sleep: bool,
    restore_scrollback: bool,
    /// A restored SSH tab connects on startup (`ssh-auto-reconnect`).
    ssh_auto_reconnect: bool,
    /// Local shells run in PTY host processes and survive restarts
    /// (`pty-host`, ADR 0041).
    pty_host: bool,
    /// An ordinary quit keeps hosted programs running (`keep-sessions-on-quit`).
    keep_sessions_on_quit: bool,
    /// Which agent states show on tabs (`[badges]`).
    badges: mtty_config::Badges,
    /// Percentage of an agent's quota at which the Agent tab marks it.
    agent_quota_warn: u8,
    /// How long a host waits for mtty (`detached-timeout`).
    detached_timeout: Duration,
    /// Hosts still running from an earlier mtty that no pane reattached to
    /// (a crash before the session was saved), offered in the palette.
    recovered: Vec<mtty_ptyhost::launch::HostInfo>,
    scrollback_saved_at: Instant,
    sleep: mtty_ui::agentloop::SleepGuard,
    agent_states: HashMap<String, String>,
    composer: Option<String>,
    quick: Option<String>,
    /// Open Quickly's file-content and scrollback search, on a worker thread.
    quick_bg: Option<BgQuickContent>,
    /// The scrollback match Open Quickly last opened: (line, column, width).
    quick_hit: Option<(usize, u16, u16)>,
    closed: Vec<Option<std::path::PathBuf>>,
    /// Scratch "Quick" tab (ADR 0019): its pane id and the tab to return to.
    quick_pane: Option<String>,
    quick_return: Option<usize>,
    hover_pointer: bool,
    search: Option<String>,
    search_idx: usize,
    /// Find matches as (buffer line, start column, width in cells).
    search_hits: Vec<(usize, u16, u16)>,
    search_key: String,
    /// Find in an editor pane: the matches as char ranges, and the query
    /// they were found for (a new query jumps to the match after the caret).
    editor_hits: Vec<(usize, usize)>,
    editor_hits_query: String,
    /// Find options and the replace field, for editor panes (ADR 0034, E4).
    find_opts: FindOptions,
    /// Why the editor's pattern finds nothing (an invalid regex).
    find_error: Option<String>,
    /// Select the match at or after the caret on the next refresh, as a new
    /// query does (after a replace, the caret is past the replaced match).
    find_rejump: bool,
    /// The Go to Line prompt's text, while it is open.
    goto_line: Option<String>,
    /// The Go to Symbol picker's query and highlighted row, while it is open.
    goto_symbol: Option<(String, usize)>,
    /// The Resume Agent Session picker's query and highlighted row (ADR 0042).
    resume_picker: Option<(String, usize)>,
    /// The vim `:` command line's text, while it is open.
    vim_command: Option<String>,
    /// Language servers for editor panes (ADR 0034, E5).
    lsp: mtty_lsp::Lsp,
    /// `MTTY_QA_COMMAND` has run.
    qa_done: bool,
    /// The pointer resting on editor text, until a hover is asked for.
    hover_rest: Option<HoverRest>,
    hover: Option<HoverPopup>,
    completion: Option<CompletionPopup>,
    /// Find running on a thread: in a view-mode file (byte ranges) or a
    /// large document (char ranges).
    bg_search: Option<BgSearch>,
    /// A view-mode pane the user tried to edit: the confirm dialog's pane.
    large_edit_offer: Option<String>,
    /// A view-mode file loading for editing: its pane and the result.
    large_loading: Option<(
        String,
        std::sync::mpsc::Receiver<Result<mtty_editor::Document, String>>,
    )>,
    /// When the open editor panes were last checked against disk.
    editor_reload_checked: Instant,
    /// An editor pane whose file changed on disk while it had unsaved edits:
    /// the reload/keep dialog's pane and the stamp seen.
    editor_reload_offer: Option<(String, editor_pane::DiskStamp)>,
    update_url: Option<String>,
    /// Check for updates once on startup (config `update-auto-check`).
    update_auto_check: bool,
    /// Set until the one startup check has been kicked off.
    update_startup_pending: bool,
    update_rx: Option<std::sync::mpsc::Receiver<UpdateResult>>,
    update_install: UpdateInstall,
    /// Carry on from check to download to install without further clicks.
    update_auto: bool,
    /// minisign key for updates: `update-pubkey`, else the release key.
    update_pubkey: String,
    update_result: Option<UpdateResult>,
    update_notice_until: Option<Instant>,
    /// A transient status-line message (failed saves, opens) and its expiry.
    notice: Option<(String, Instant)>,
    /// Background work results (see [`JobDone`]).
    jobs_tx: std::sync::mpsc::Sender<JobDone>,
    jobs_rx: std::sync::mpsc::Receiver<JobDone>,
    /// The pane behind the last notification, so activating mtty soon after
    /// it jumps there (macOS notifications from osascript cannot be clicked
    /// through to a pane).
    alert_target: Option<(String, Instant)>,
    /// egui showed a resize cursor last frame (pointer on a panel edge).
    ui_resize_hover: bool,
    #[cfg(windows)]
    window_resize_hover: bool,
    /// Wheel/trackpad movement not yet amounting to a whole line.
    wheel_accum: f64,
    /// The pointer was on empty title-row space last frame: a press there
    /// moves the window (unified title bar only).
    title_drag_hover: bool,
    /// When the title row was last pressed, to turn a second press into zoom.
    title_pressed_at: Option<Instant>,
    /// Side panel widths (logical points), as the user dragged them.
    sidebar_w: f32,
    details_w: f32,
    /// Agent CLIs found on PATH, and when that was checked.
    agents_detected: Option<(Instant, Vec<bool>)>,
    /// Settings as last loaded/saved, to write back only what changed.
    saved_settings: Vec<(&'static str, String)>,
    /// Where settings came from when there was no config.toml.
    config_imported_from: Option<&'static str>,
    update_dialog: bool,
    details_tab: usize,
    details_cwd: Option<std::path::PathBuf>,
    details_data: Option<DetailsData>,
    details_rx: Option<std::sync::mpsc::Receiver<(std::path::PathBuf, DetailsData)>>,
    details_at: Instant,
    /// Prompts waiting for an agent pane to become idle (ADR 0010, B2.2).
    prompt_queue: mtty_ui::agentloop::PromptQueue,
    prompt_input: String,
    last_title: Option<String>,
    focused: bool,
    occluded: bool,
    redraw: redraw::Redraw,
    cursor_on: bool,
    last_blink: Instant,
    image_wake: Option<Instant>,
    start: Instant,
    shot_now: bool,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
}

/// Some platforms report minimized windows with their old nonzero size, so
/// size alone must not keep submitting GPU work to a hidden surface.
fn window_drawable(window: &Window, occluded: bool) -> bool {
    let size = window.inner_size();
    !occluded
        && size.width > 0
        && size.height > 0
        && window.is_visible() != Some(false)
        && window.is_minimized() != Some(true)
}

/// An always-on-top window mirroring the active pane (read-only).
struct Pip {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    quads: QuadRenderer,
    renderer: TermRenderer,
    occluded: bool,
    redraw: redraw::Redraw,
}

#[derive(Default)]
struct Shortcut {
    new_tab: bool,
    close: bool,
    select: Option<usize>,
    font: f32,
    split_right: bool,
    split_down: bool,
    cycle: i32,
    tab: i32,
    find: i32,
    hint: bool,
    toggle_sidebar: bool,
    toggle_details: bool,
    palette: bool,
    settings: bool,
    composer: bool,
    quickly: bool,
    reopen: bool,
    search: bool,
    quick_terminal: bool,
}

fn shortcut(event: &KeyEvent, mods: ModifiersState) -> Option<Shortcut> {
    if event.state != ElementState::Pressed {
        return None;
    }
    // Off macOS the chords hold Shift, so match the key as it is without
    // modifiers (`t`, not `T`; `1`, not `!`), in the user's layout.
    #[cfg(not(target_os = "macos"))]
    let key = {
        use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
        event.key_without_modifiers()
    };
    #[cfg(target_os = "macos")]
    let key = event.logical_key.clone();
    shortcut_for(&key, mods, cfg!(target_os = "macos"))
}

/// The app shortcut for a key. macOS uses ⌘ (and ⌘⇧). Linux and Windows
/// follow their terminals: Ctrl+Shift for what is ⌘ on macOS, Ctrl+Shift+Alt
/// for ⌘⇧, Alt+1…9 and Ctrl+PgUp/PgDn (or Ctrl+Tab) for tabs, Ctrl+, for
/// settings and Ctrl+= / Ctrl+- for the font. Plain Ctrl chords stay the
/// shell's and Super stays the desktop's.
fn shortcut_for(key: &Key, mods: ModifiersState, mac: bool) -> Option<Shortcut> {
    if mac {
        mac_shortcut(key, mods)
    } else {
        pc_shortcut(key, mods)
    }
}

fn pc_shortcut(key: &Key, mods: ModifiersState) -> Option<Shortcut> {
    let (ctrl, shift, alt) = (mods.control_key(), mods.shift_key(), mods.alt_key());
    if mods.super_key() {
        return None;
    }
    let mut s = Shortcut::default();
    let ch = match key {
        Key::Character(c) => c.to_lowercase(),
        _ => String::new(),
    };
    let digit = ch.parse::<usize>().ok().filter(|d| (1..=9).contains(d));
    match key {
        // Tabs: Ctrl+PgUp/PgDn and Ctrl+(Shift+)Tab, as in browsers and
        // other terminals.
        Key::Named(NamedKey::PageUp | NamedKey::PageDown) if ctrl && !shift && !alt => {
            s.tab = if matches!(key, Key::Named(NamedKey::PageUp)) {
                -1
            } else {
                1
            };
            return Some(s);
        }
        Key::Named(NamedKey::Tab) if ctrl && !alt => {
            s.tab = if shift { -1 } else { 1 };
            return Some(s);
        }
        _ => {}
    }
    if alt && !ctrl && !shift {
        s.select = Some(digit? - 1);
        return Some(s);
    }
    if ctrl && !alt && matches!(ch.as_str(), "=" | "+" | "-") {
        s.font = if ch == "-" { -1.0 } else { 1.0 };
        return Some(s);
    }
    if ctrl && !shift && !alt {
        if ch == "," {
            s.settings = true;
            return Some(s);
        }
        return None;
    }
    if !(ctrl && shift) {
        return None;
    }
    match (alt, ch.as_str()) {
        (false, "t") => s.new_tab = true,
        (false, "w") => s.close = true,
        (false, "e") => s.composer = true,
        (false, "f") => s.search = true,
        (false, "k" | "p") => s.palette = true,
        (false, "d") => s.split_right = true,
        (false, "g") => s.find = 1,
        // Pane focus, like ⌘[ / ⌘] on macOS.
        (false, "[") => s.cycle = -1,
        (false, "]") => s.cycle = 1,
        (true, "d") => s.split_down = true,
        (true, "g") => s.find = -1,
        (true, "t") => s.quick_terminal = true,
        (true, "z") => s.reopen = true,
        (true, "h") => s.hint = true,
        (true, "o") => s.quickly = true,
        (true, "l") => s.toggle_sidebar = true,
        (true, "r") => s.toggle_details = true,
        _ => return None,
    }
    Some(s)
}

fn mac_shortcut(key: &Key, mods: ModifiersState) -> Option<Shortcut> {
    if !mods.super_key() {
        return None;
    }
    let mut s = Shortcut::default();
    let mut matched = true;
    match key {
        Key::Character(c) => match c.as_str() {
            "t" if mods.shift_key() => s.quick_terminal = true,
            "t" => s.new_tab = true,
            "z" if mods.shift_key() => s.reopen = true,
            "e" => s.composer = true,
            "h" if mods.shift_key() => s.hint = true,
            "g" if mods.shift_key() => s.find = -1,
            "g" => s.find = 1,
            "o" if mods.shift_key() => s.quickly = true,
            "f" => s.search = true,
            "k" => s.palette = true,
            "p" if mods.shift_key() => s.palette = true,
            "," => s.settings = true,
            "w" => s.close = true,
            "d" => {
                if mods.shift_key() {
                    s.split_down = true;
                } else {
                    s.split_right = true;
                }
            }
            "l" if mods.shift_key() => s.toggle_sidebar = true,
            "r" if mods.shift_key() => s.toggle_details = true,
            "]" if mods.shift_key() => s.tab = 1,
            "[" if mods.shift_key() => s.tab = -1,
            "]" => s.cycle = 1,
            "[" => s.cycle = -1,
            "+" | "=" => s.font = 1.0,
            "-" => s.font = -1.0,
            "1" => s.select = Some(0),
            "2" => s.select = Some(1),
            "3" => s.select = Some(2),
            "4" => s.select = Some(3),
            "5" => s.select = Some(4),
            "6" => s.select = Some(5),
            "7" => s.select = Some(6),
            "8" => s.select = Some(7),
            "9" => s.select = Some(8),
            _ => matched = false,
        },
        _ => matched = false,
    }
    if matched {
        Some(s)
    } else {
        None
    }
}

/// A hyperlink resolved under the pointer, with its screen geometry.
struct LinkHit {
    url: String,
    start: u16,
    end: u16,
    row: u16,
    inner: Rect,
}

impl State {
    fn cell_size(font_size: f32, line_ratio: f32, family: Option<&str>) -> (f32, f32) {
        let mut probe = mtty_render::MetricsProbe::new();
        probe.cell(font_size, (font_size * line_ratio).round(), family)
    }

    fn window_size(&self) -> (u32, u32) {
        let s = self.window.inner_size();
        (s.width.max(1), s.height.max(1))
    }

    /// The central grid area in logical points (window minus chrome).
    fn grid_area(&self) -> Rect {
        let size = self.window.inner_size();
        let scale = self.window.scale_factor() as f32;
        let w = size.width as f32 / scale;
        let h = size.height as f32 / scale;
        let x = if self.show_sidebar {
            self.sidebar_w
        } else {
            0.0
        };
        let right = if self.show_details {
            self.details_w
        } else {
            0.0
        };
        let top = chrome::content_top(!menu_in_os() && !cfg!(windows));
        Rect {
            x,
            y: top,
            w: (w - x - right).max(1.0),
            h: (h - top - STATUS_H).max(1.0),
        }
    }

    /// Size every tab's terminals to their layout, not just the active tab's
    /// (a background tab is otherwise sized when it is first shown).
    fn fit_all_panes(&mut self) {
        let scale = self.window.scale_factor() as f32;
        let (cw, ch) = (self.cw * scale, self.ch * scale);
        let area = self.grid_area();
        for tab in &mut self.tabs {
            for (id, r) in tab.layout.rects(area) {
                if let Some(pane) = tab.panes.iter_mut().find(|p| p.id == id) {
                    let inner = card_inner(r);
                    let cols = ((inner.w * scale) / cw).floor().max(1.0) as u16;
                    let rows = ((inner.h * scale) / ch).floor().max(1.0) as u16;
                    pane.term.set_cell_size(cw as u16, ch as u16);
                    pane.term.resize(rows, cols);
                }
            }
        }
        self.apply_restored_images();
    }

    /// Finish restored panes now that every pane has its final width: place
    /// their inline images, then append the "[mtty] Restored…" note. Images
    /// anchor to the restored text's last row (the note is not there yet), and
    /// a pane that came back at a different width reflowed its text, so its
    /// saved rows no longer name the same content: those placements are dropped
    /// rather than drawn over the wrong text (see `GraphicsLayer::restore_images`).
    fn apply_restored_images(&mut self) {
        for pane in self.tabs.iter_mut().flat_map(|t| t.panes.iter_mut()) {
            if let Some(pending) = pane.pending_images.take() {
                if self.graphics_enabled {
                    if let Some(last) = pane.term.screen().last_content_index() {
                        let cols = pane.term.screen().size().1;
                        let history = pane.term.screen().history_size() as i32;
                        let max_pixels = pane.term.graphics().max_pixels;
                        pane.term.graphics_mut().restore_images(
                            &pending.saved,
                            history,
                            last as i32,
                            cols,
                            max_pixels,
                        );
                    }
                }
            }
            if let Some(note) = pane.pending_note.take() {
                pane.term.screen_mut().process(note.as_bytes());
            }
        }
    }

    fn pane_rects(&self) -> Vec<(String, Rect)> {
        match self.tabs.get(self.active_tab) {
            Some(tab) => tab.layout.rects(self.grid_area()),
            None => Vec::new(),
        }
    }

    fn active_pane_id(&self) -> Option<String> {
        self.tabs.get(self.active_tab).map(|t| t.active.clone())
    }

    /// The grid size a whole-area pane starts at: the shell must start at
    /// the size it is drawn at, or zsh's end-of-line mark wraps onto a line
    /// of its own.
    fn new_pane_size(&self) -> (u16, u16) {
        let scale = self.window.scale_factor() as f32;
        let (cw, ch) = (self.cw * scale, self.ch * scale);
        let area = card_inner(self.grid_area());
        let cols = ((area.w * scale) / cw).floor().max(1.0) as u16;
        let rows = ((area.h * scale) / ch).floor().max(1.0) as u16;
        (cols, rows)
    }

    fn pane_waker(&self) -> Arc<dyn Fn() + Send + Sync> {
        let proxy = self.proxy.clone();
        Arc::new(move || {
            let _ = proxy.send_event(HostEvent::Wake);
        })
    }

    /// Apply the app's terminal settings to a new pane's terminal.
    fn make_pane(&self, id: String, mut term: Terminal) -> Pane {
        term.set_graphics_enabled(self.graphics_enabled);
        let Rgb(r, g, b) = self.theme.fg;
        let Rgb(br, bg, bb) = self.theme.bg;
        term.set_default_colors([r, g, b], [br, bg, bb]);
        let scale = self.window.scale_factor() as f32;
        term.set_cell_size((self.cw * scale) as u16, (self.ch * scale) as u16);
        Pane {
            id,
            term,
            scroll: 0,
            on_enter: None,
            published_output: None,
            pending_images: None,
            pending_note: None,
        }
    }

    /// The installed PTY host, when `pty-host` is on (ADR 0041).
    fn host_config(&self) -> Option<mtty_core::HostConfig> {
        use mtty_ptyhost::launch;
        if !self.pty_host {
            return None;
        }
        let installed = launch::bundled_binary()
            .zip(mtty_config::data_dir())
            .map(|(binary, data)| launch::install(&binary, &data));
        match installed {
            Some(Ok(binary)) => Some(mtty_core::HostConfig {
                binary,
                ring: PTY_HOST_RING,
                timeout: self.detached_timeout,
            }),
            Some(Err(e)) => {
                eprintln!("mtty: cannot install the PTY host: {e}");
                None
            }
            None => {
                eprintln!("mtty: no PTY host next to the executable");
                None
            }
        }
    }

    fn spawn_pane(&self, cwd: Option<std::path::PathBuf>) -> Option<Pane> {
        let (cols, rows) = self.new_pane_size();
        let id = gen_id();
        let waker = self.pane_waker();
        // A Finder/Dock launch hands the app `/` as its working directory, so
        // inheriting the process cwd would drop every pane in the filesystem
        // root. Only trust it when it names a real place to work, and fall back
        // to the home directory otherwise; a launch from a terminal keeps the
        // directory the user typed `mtty` in. A requested directory that no
        // longer exists falls back the same way.
        let cwd = cwd.filter(|dir| dir.is_dir()).or_else(|| {
            std::env::current_dir()
                .ok()
                .filter(|dir| dir.as_path() != std::path::Path::new("/"))
                .or_else(mtty_config::home_dir)
        });
        // Both names: installed hooks and miao read the former one (ADR 0032).
        let env = vec![
            ("MTTY_PANE_ID".to_string(), id.clone()),
            ("MIAOTTY_PANE_ID".to_string(), id.clone()),
        ];
        if let Some(config) = self.host_config() {
            match Terminal::new_hosted(
                &config,
                None,
                cols,
                rows,
                10_000,
                cwd.clone(),
                &env,
                waker.clone(),
            ) {
                Ok(term) => return Some(self.make_pane(id, term)),
                Err(e) => eprintln!("mtty: PTY host failed, running the shell in the app: {e}"),
            }
        }
        Terminal::new(None, cols, rows, 10_000, cwd, &env, waker)
            .ok()
            .map(|term| self.make_pane(id, term))
    }

    /// Attach to the host a saved pane was running in, keeping its id (its
    /// shell's environment names it). `None` when the host is gone.
    fn reattach_pane(&self, saved: &serde_json::Value) -> Option<Pane> {
        let host = saved.get("host")?;
        let id = saved.get("id")?.as_str()?;
        let host_id = host.get("id")?.as_str()?;
        let socket = std::path::PathBuf::from(host.get("socket")?.as_str()?);
        let snapshot = scrollback_dir()
            .filter(|_| is_plain_file_name(id))
            .and_then(|dir| std::fs::read(dir.join(format!("{id}.host.json"))).ok())
            .and_then(|bytes| serde_json::from_slice::<mtty_core::HostSnapshot>(&bytes).ok());
        let (cols, rows) = self.new_pane_size();
        let term = Terminal::reattach(
            host_id,
            &socket,
            snapshot,
            cols,
            rows,
            10_000,
            self.pane_waker(),
        )
        .ok()?;
        Some(self.make_pane(id.to_string(), term))
    }

    /// Advertise the panes to the MTP control plane.
    fn publish_panes(&self) {
        let mut panes = Vec::new();
        for tab in &self.tabs {
            for p in &tab.panes {
                let title = p
                    .term
                    .title()
                    .map(str::to_string)
                    .unwrap_or_else(|| tab.title.clone());
                panes.push(serde_json::json!({ "id": p.id, "title": title, "kind": "terminal" }));
            }
            for e in &tab.editors {
                panes.push(serde_json::json!({
                    "id": e.id,
                    "title": e.title(),
                    "kind": "editor",
                    "path": e.path,
                    "remote": e.remote.as_ref().map(|r| r.dest.clone()),
                }));
            }
            for p in &tab.previews {
                panes.push(serde_json::json!({
                    "id": p.id,
                    "title": "Preview",
                    "kind": "preview",
                    "source": p.source,
                }));
            }
        }
        self.mtp.set_panes(panes);
        self.save_session();
    }

    fn new_tab(&mut self) {
        self.new_tab_in(None);
    }

    /// The active pane's directory for a new tab or split (see [`inherited_cwd`]).
    fn active_cwd_for_new(&self) -> Option<std::path::PathBuf> {
        let ssh = self.tabs.get(self.active_tab).is_some_and(|t| t.ssh);
        inherited_cwd(ssh, self.cwd())
    }

    /// Remember closed tabs' directories for Reopen Closed Tab.
    fn remember_closed(&mut self, tabs: &[Tab]) {
        for tab in tabs {
            let cwd = tab
                .panes
                .iter()
                .find(|p| p.id == tab.active)
                .and_then(|p| p.term.cwd().map(std::path::PathBuf::from));
            self.closed.push(cwd);
        }
    }

    fn new_tab_in(&mut self, cwd: Option<std::path::PathBuf>) {
        let Some(pane) = self.spawn_pane(cwd) else {
            return;
        };
        self.push_tab(pane);
    }

    /// Open `pane` in a new tab after the others and focus it.
    fn push_tab(&mut self, pane: Pane) {
        let id = pane.id.clone();
        let n = self.tabs.len() + 1;
        self.tabs.push(Tab {
            layout: Layout::leaf(id.clone()),
            panes: vec![pane],
            active: id,
            title: format!("shell {n}"),
            title_set: false,
            shown: None,
            ssh: false,
            ssh_target: None,
            ssh_cmd: None,
            transport: None,
            prefix: None,
            mark: None,
            group: None,
            attention: None,
            editors: Vec::new(),
            previews: Vec::new(),
        });
        self.active_tab = self.tabs.len() - 1;
        self.selection = None;
        self.publish_panes();
    }

    /// Reopen the most recently closed tab (Cmd+Shift+T) in its old cwd.
    /// Toggle the scratch "Quick" tab (ADR 0019): the first invocation opens a
    /// tab named *Quick* and remembers where to return; later ones flip between
    /// the Quick tab and that tab.
    fn toggle_quick_terminal(&mut self) {
        if let Some(id) = self.quick_pane.clone() {
            if let Some(ti) = self
                .tabs
                .iter()
                .position(|t| t.panes.iter().any(|p| p.id == id))
            {
                if self.active_tab == ti {
                    if let Some(prev) = self.quick_return.take() {
                        self.active_tab = prev.min(self.tabs.len().saturating_sub(1));
                        self.selection = None;
                    }
                } else {
                    self.quick_return = Some(self.active_tab);
                    self.active_tab = ti;
                    self.selection = None;
                }
                self.window.request_redraw();
                return;
            }
        }
        let prev = self.active_tab;
        self.new_tab();
        if let Some(tab) = self.tabs.last_mut() {
            tab.title = mtty_ui::i18n::t(self.lang, "Quick", "快速").to_string();
            tab.title_set = true;
            if let Some(pane) = tab.panes.first() {
                self.quick_pane = Some(pane.id.clone());
            }
        }
        self.quick_return = Some(prev);
        self.publish_panes();
    }

    fn reopen_tab(&mut self) {
        let Some(cwd) = self.closed.pop() else {
            return;
        };
        let Some(pane) = self.spawn_pane(cwd) else {
            return;
        };
        let id = pane.id.clone();
        let n = self.tabs.len() + 1;
        self.tabs.push(Tab {
            layout: Layout::leaf(id.clone()),
            panes: vec![pane],
            active: id,
            title: format!("shell {n}"),
            title_set: false,
            shown: None,
            ssh: false,
            ssh_target: None,
            ssh_cmd: None,
            transport: None,
            prefix: None,
            mark: None,
            group: None,
            attention: None,
            editors: Vec::new(),
            previews: Vec::new(),
        });
        self.active_tab = self.tabs.len() - 1;
        self.selection = None;
        self.publish_panes();
    }

    fn close_pane(&mut self) {
        if !self.confirm_close_active_editor() {
            return;
        }
        let cwd = self
            .active_pane()
            .and_then(|p| p.term.cwd().map(std::path::PathBuf::from));
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        let target = tab.active.clone();
        if let Some(preview) = tab.previews.iter().find(|p| p.id == target) {
            // A preview closes on its own; its editor takes the focus.
            let source = preview.source.clone();
            tab.previews.retain(|p| p.id != target);
            let _ = tab.layout.remove(&target);
            tab.active = source;
            self.publish_panes();
            return;
        }
        if tab.panes.len() + tab.editors.len() <= 1 {
            // The last tab stays alive as a shell: closing a lone editor there
            // opens a terminal tab in its place.
            let lone_editor = tab.panes.is_empty();
            if self.tabs.len() == 1 && lone_editor {
                let closing = self.active_tab;
                self.new_tab_in(cwd.clone());
                self.tabs.remove(closing);
                self.active_tab = self.tabs.len() - 1;
                self.selection = None;
                self.publish_panes();
                return;
            }
            // Close the tab.
            if self.tabs.len() > 1 {
                self.closed.push(cwd);
                self.tabs.remove(self.active_tab);
                self.active_tab = self.active_tab.min(self.tabs.len() - 1);
                self.selection = None;
                self.publish_panes();
            }
            return;
        }
        tab.panes.retain(|p| p.id != target);
        tab.remove_editor(&target);
        let _ = tab.layout.remove(&target);
        tab.active = tab.layout.ids().first().cloned().unwrap_or_default();
        self.selection = None;
        self.publish_panes();
    }

    /// The active pane's inner (terminal) rect in logical points.
    fn active_inner(&self) -> Option<Rect> {
        let id = self.active_pane_id()?;
        self.pane_rects()
            .into_iter()
            .find(|(pid, _)| *pid == id)
            .map(|(_, r)| card_inner(r))
    }

    /// The active pane's inner rect and cursor cell, for cursor-anchored
    /// overlays (IME preedit). `None` while the view is scrolled back.
    fn active_cursor(&self) -> Option<(Rect, (u16, u16))> {
        if let Some(ed) = self.active_editor() {
            let inner = self.active_inner()?;
            let (row, col) = ed.caret_cell()?;
            return Some((inner, (row as u16, col as u16)));
        }
        let id = self.active_pane_id()?;
        let pane = self
            .tabs
            .get(self.active_tab)?
            .panes
            .iter()
            .find(|p| p.id == id)?;
        if pane.scroll != 0 {
            return None;
        }
        let inner = self.active_inner()?;
        Some((inner, pane.term.screen().cursor_position()))
    }

    fn cancel_hints(&mut self) {
        self.hint_mode = false;
        self.hints.clear();
    }

    /// Collect Hint-Mode labels over visible URLs / absolute paths.
    fn build_hints(&mut self) {
        self.hints.clear();
        let Some(id) = self.active_pane_id() else {
            return;
        };
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let Some(pane) = tab.panes.iter().find(|p| p.id == id) else {
            return;
        };
        let screen = pane.term.screen();
        let (rows, _) = screen.size();
        let lines: Vec<String> = (0..rows).map(|r| screen.line_text(r)).collect();
        self.hints = mtty_ui::hints::scan(&lines);
        self.hint_mode = !self.hints.is_empty();
    }

    /// Lazily load a directory and (transitively) its expanded subdirectories.
    /// List `dir` in the background (a network mount or a huge directory must
    /// not stall the UI); [`State::tree_listed`] stores the result.
    fn load_tree(&mut self, dir: &std::path::Path) {
        if self.tree_children.contains_key(dir) || !self.tree_loading.insert(dir.to_path_buf()) {
            return;
        }
        let dir = dir.to_path_buf();
        self.spawn_job(move || {
            let entries = files_rows(&dir);
            JobDone::DirListed { dir, entries }
        });
    }

    fn tree_listed(&mut self, dir: std::path::PathBuf, entries: Vec<FileEntry>) {
        self.tree_loading.remove(&dir);
        let expanded: Vec<std::path::PathBuf> = entries
            .iter()
            .filter(|f| f.is_dir)
            .map(|f| dir.join(&f.name))
            .filter(|d| self.tree_expanded.contains(d))
            .collect();
        self.tree_children.insert(dir, entries);
        for sub in expanded {
            self.load_tree(&sub);
        }
    }

    fn files_body(&mut self, ui: &mut egui::Ui, lang: mtty_ui::i18n::Lang) {
        use mtty_ui::i18n::t;
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.files_filter)
                    .hint_text(t(lang, "Filter…", "过滤…"))
                    .desired_width(150.0),
            );
            if ui
                .small_button("\u{21bb}")
                .on_hover_text(t(lang, "Refresh", "刷新"))
                .clicked()
            {
                self.tree_children.clear();
                self.tree_loading.clear();
                self.ensure_details();
            }
        });
        let Some(root) = self.cwd() else {
            ui.label("\u{2014}");
            return;
        };
        self.load_tree(&root);
        if !self.tree_children.contains_key(&root) {
            ui.label(
                egui::RichText::new(t(lang, "Loading…", "加载中…"))
                    .size(12.0)
                    .color(chrome_rgb(self.theme.chrome().muted)),
            );
            return;
        }
        let filter = self.files_filter.to_lowercase();
        let ch = self.theme.chrome();
        let mut open_file = None;
        let mut toggle = None;
        render_dir_tree(
            ui,
            &self.tree_children,
            &self.tree_expanded,
            &root,
            0,
            &filter,
            &ch,
            &mut open_file,
            &mut toggle,
        );
        if let Some(d) = toggle {
            if !self.tree_expanded.insert(d.clone()) {
                self.tree_expanded.remove(&d);
            }
            self.load_tree(&d);
        }
        if let Some(f) = open_file {
            self.open_editor(f);
        }
    }

    /// Create the PiP window (called from the event loop when requested).
    fn create_pip(&mut self, event_loop: &ActiveEventLoop) {
        if self.pip.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("mtty \u{00b7} picture-in-picture")
            .with_inner_size(LogicalSize::new(720.0, 400.0))
            .with_window_level(winit::window::WindowLevel::AlwaysOnTop);
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("mtty: picture-in-picture window failed: {e}");
                return;
            }
        };
        let surface = match self.instance.create_surface(window.clone()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("mtty: pip surface failed: {e}");
                return;
            }
        };
        let caps = surface.get_capabilities(&self.adapter);
        let Some(format) = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .or_else(|| caps.formats.first().copied())
        else {
            self.show_notice("picture-in-picture: no surface format".to_string());
            return;
        };
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&self.device, &config);
        let quads = QuadRenderer::new(&self.device, format);
        let renderer = TermRenderer::new(&self.device, &self.queue, format);
        self.pip = Some(Pip {
            window,
            surface,
            config,
            quads,
            renderer,
            occluded: false,
            redraw: redraw::Redraw::default(),
        });
        self.window.request_redraw();
    }

    fn resize_pip(&mut self) {
        let (device, w, h) = (
            &self.device,
            self.pip.as_ref().map(|p| p.window.inner_size().width),
            self.pip.as_ref().map(|p| p.window.inner_size().height),
        );
        if let (Some(w), Some(h)) = (w, h) {
            if w > 0 && h > 0 {
                if let Some(pip) = self.pip.as_mut() {
                    pip.config.width = w;
                    pip.config.height = h;
                    pip.surface.configure(device, &pip.config);
                }
            }
        }
    }

    /// Render the active pane into the PiP window.
    fn render_pip(&mut self) {
        let _render_timer = resource_metrics::RenderTimer::start();
        let Some(pip) = self.pip.as_mut() else {
            return;
        };
        if !pip.redraw.begin(
            Instant::now(),
            pip.window.has_focus(),
            window_drawable(&pip.window, pip.occluded),
        ) {
            return;
        }
        let theme = self.theme.clone();
        let scale = pip.window.scale_factor() as f32;
        let win_size = (pip.config.width, pip.config.height);
        let rows = match self.tabs.get(self.active_tab) {
            Some(tab) => match tab.panes.iter().find(|p| p.id == tab.active) {
                Some(pane) => build_rows(pane.term.screen(), &theme, None),
                None => return,
            },
            None => return,
        };
        let Ok(frame) = pip.surface.get_current_texture() else {
            return;
        };
        let view = frame.texture.create_view(&Default::default());
        let full = Quad::new(
            (0.0, 0.0),
            (win_size.0 as f32, win_size.1 as f32),
            (theme.bg.0, theme.bg.1, theme.bg.2, 255),
        );
        pip.quads
            .prepare(&self.device, &self.queue, win_size, &[full]);
        pip.renderer.prepare(
            &self.device,
            &self.queue,
            win_size,
            scale,
            self.font_size,
            (self.font_size * self.line_ratio).round(),
            self.cw,
            0.0,
            0.0,
            (theme.fg.0, theme.fg.1, theme.fg.2),
            self.font_family.as_deref(),
            &rows,
        );
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = enc
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("pip"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                })
                .forget_lifetime();
            pip.quads.render(&mut pass);
            pip.renderer.render(&mut pass);
        }
        self.queue.submit(Some(enc.finish()));
        frame.present();
        resource_metrics::presented(true);
    }

    /// Close one pane by id (drops its tab when it was the last one).
    fn close_pane_id(&mut self, id: &str) {
        if let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|t| t.previews.iter().any(|p| p.id == id))
        {
            tab.previews.retain(|p| p.id != id);
            let _ = tab.layout.remove(id);
            if tab.active == id {
                tab.active = tab.layout.ids().first().cloned().unwrap_or_default();
            }
            self.publish_panes();
            return;
        }
        // An editor pane closes only without unsaved changes, unless the UI
        // already armed the discard (a second click on the pane's close
        // button): a client cannot answer "discard them?".
        if let Some(ti) = self
            .tabs
            .iter()
            .position(|t| t.editors.iter().any(|e| e.id == id))
        {
            let tab = &mut self.tabs[ti];
            if let Some(e) = tab.editors.iter().find(|e| e.id == id) {
                if e.doc.is_modified() && !e.close_armed {
                    let msg = format!(
                        "{}: {}",
                        e.title(),
                        mtty_ui::i18n::t(
                            self.lang,
                            "unsaved changes; not closed",
                            "有未保存的修改,未关闭"
                        )
                    );
                    self.show_notice(msg);
                    return;
                }
            }
            tab.remove_editor(id);
            let _ = tab.layout.remove(id);
            if tab.panes.is_empty() && tab.editors.is_empty() {
                if self.tabs.len() == 1 {
                    // Keep a shell when the last tab empties.
                    self.new_tab_in(None);
                }
                self.tabs.remove(ti);
                if ti < self.active_tab {
                    self.active_tab -= 1;
                }
                self.active_tab = self.active_tab.min(self.tabs.len() - 1);
            } else if tab.active == id {
                tab.active = tab.layout.ids().first().cloned().unwrap_or_default();
            }
            self.publish_panes();
            return;
        }
        let Some(ti) = self
            .tabs
            .iter()
            .position(|t| t.panes.iter().any(|p| p.id == id))
        else {
            return;
        };
        if self.quick_pane.as_deref() == Some(id) {
            self.quick_pane = None;
            self.quick_return = None;
        }
        let tab = &mut self.tabs[ti];
        tab.panes.retain(|p| p.id != id);
        let _ = tab.layout.remove(id);
        if tab.panes.is_empty() {
            self.tabs.remove(ti);
            if ti < self.active_tab {
                self.active_tab -= 1;
            }
        } else if tab.active == id {
            tab.active = tab
                .layout
                .ids()
                .first()
                .cloned()
                .unwrap_or_else(|| tab.panes[0].id.clone());
        }
        if self.tabs.is_empty() {
            self.new_tab();
        }
        self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
        self.selection = None;
        self.publish_panes();
    }

    /// Close panes whose shell has exited (so `exit` actually closes).
    fn reap_exited(&mut self) {
        let ids: Vec<String> = self
            .tabs
            .iter()
            .flat_map(|t| t.panes.iter())
            .filter(|p| p.term.exited())
            .map(|p| p.id.clone())
            .collect();
        let closed = !ids.is_empty();
        for id in ids {
            self.close_pane_id(&id);
        }
        if closed {
            self.retain_live_renderers();
            self.window.request_redraw();
        }
    }

    fn duplicate_tab(&mut self) {
        let cwd = self.cwd();
        let (title_set, title, ssh, transport, (target, ssh_cmd), prefix, mark, group) = self
            .tabs
            .get(self.active_tab)
            .map(|t| {
                (
                    t.title_set,
                    t.title.clone(),
                    t.ssh,
                    t.transport.clone(),
                    (t.ssh_target.clone(), t.ssh_cmd.clone()),
                    t.prefix.clone(),
                    t.mark.clone(),
                    t.group.clone(),
                )
            })
            .unwrap_or_default();
        // An ssh or transport tab is duplicated by connecting again, not as a
        // local shell that merely looks remote.
        if let Some(target) = transport {
            self.connect_transport(target, Some(title.clone()));
        } else {
            match (ssh, ssh_cmd, target) {
                (true, Some(cmd), target) => {
                    self.open_ssh_command(title.clone(), cmd, target.unwrap_or_default())
                }
                (true, None, Some(target)) => self.open_ssh(&target),
                _ => self.new_tab_in(inherited_cwd(ssh, cwd)),
            }
        }
        if let Some(t) = self.tabs.last_mut() {
            if title_set {
                t.title = title;
                t.title_set = true;
            }
            t.prefix = prefix;
            t.mark = mark;
            t.group = group;
        }
        self.publish_panes();
    }

    fn cycle_tab(&mut self, forward: bool) {
        let n = self.tabs.len();
        if n > 1 {
            let i = self.active_tab;
            self.active_tab = if forward {
                (i + 1) % n
            } else {
                (i + n - 1) % n
            };
            self.selection = None;
        }
    }

    fn split(&mut self, dir: SplitDir) {
        let Some(pane) = self.spawn_pane(self.active_cwd_for_new()) else {
            return;
        };
        let new_id = pane.id.clone();
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            let target = tab.active.clone();
            if tab.layout.split(&target, &new_id, dir) {
                tab.panes.push(pane);
                tab.active = new_id;
                self.selection = None;
            }
        }
        self.resize();
        self.publish_panes();
    }

    fn cycle_pane(&mut self, forward: bool) {
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            let ids = tab.layout.ids();
            if ids.len() > 1 {
                let cur = ids.iter().position(|x| x == &tab.active).unwrap_or(0);
                let next = if forward {
                    (cur + 1) % ids.len()
                } else {
                    (cur + ids.len() - 1) % ids.len()
                };
                tab.active = ids[next].clone();
                self.selection = None;
            }
        }
    }

    fn resize(&mut self) {
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        let scale = self.window.scale_factor() as f32;
        let cw = self.cw * scale;
        let ch = self.ch * scale;
        let rects = self.pane_rects();
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            for (id, r) in &rects {
                if let Some(pane) = tab.panes.iter_mut().find(|p| &p.id == id) {
                    let inner = card_inner(*r);
                    let cols = ((inner.w * scale) / cw).floor().max(1.0) as u16;
                    let rows = ((inner.h * scale) / ch).floor().max(1.0) as u16;
                    pane.term.set_cell_size(cw as u16, ch as u16);
                    pane.term.resize(rows, cols);
                }
            }
        }
    }

    fn write_input(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.read_only {
            return;
        }
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            if tab.attention == Some(Attention::Done) {
                tab.attention = None;
            }
            if let Some(pane) = tab.panes.iter_mut().find(|p| p.id == tab.active) {
                if let Some(command) = on_enter_input(&mut pane.on_enter, bytes) {
                    pane.scroll = 0;
                    pane.term.write(&command);
                    return;
                }
                pane.term.write(bytes);
                pane.scroll = 0;
            }
            // Broadcast (B3.5): the same input to every other pane of the tab.
            if self.broadcast {
                let active = tab.active.clone();
                for pane in tab.panes.iter_mut().filter(|p| p.id != active) {
                    pane.on_enter = None;
                    pane.term.write(bytes);
                    pane.scroll = 0;
                }
            }
        }
        self.window.request_redraw();
    }

    /// The hyperlink under the pointer in the active pane, if any.
    fn link_at_pointer(&self) -> Option<LinkHit> {
        let scale = self.window.scale_factor() as f32;
        let (px, py) = (self.cursor.0 as f32, self.cursor.1 as f32);
        let (id, r) = self.pane_rects().into_iter().find(|(_, r)| {
            px >= r.x * scale
                && px < (r.x + r.w) * scale
                && py >= r.y * scale
                && py < (r.y + r.h) * scale
        })?;
        let inner = card_inner(r);
        let col = ((px - inner.x * scale) / (self.cw * scale))
            .floor()
            .max(0.0) as u16;
        let row = ((py - inner.y * scale) / (self.ch * scale))
            .floor()
            .max(0.0) as u16;
        let tab = self.tabs.get(self.active_tab)?;
        let pane = tab.panes.iter().find(|p| p.id == id)?;
        let (url, start, end) = link_at(&pane.term.screen().line_text(row), col)?;
        Some(LinkHit {
            url,
            start,
            end,
            row,
            inner,
        })
    }

    /// The currently selected text, if any.
    fn selection_text(&self) -> Option<String> {
        let (pane_id, sel) = self.selection.as_ref()?;
        let tab = self.tabs.get(self.active_tab)?;
        let pane = tab.panes.iter().find(|p| &p.id == pane_id)?;
        let (r1, c1, r2, c2) = sel.ordered();
        Some(pane.term.screen().contents_between(r1, c1, r2, c2))
    }

    /// The selection with SGR colour codes.
    fn selection_ansi(&self) -> Option<String> {
        let (pane_id, sel) = self.selection.as_ref()?;
        let tab = self.tabs.get(self.active_tab)?;
        let pane = tab.panes.iter().find(|p| &p.id == pane_id)?;
        let (r1, c1, r2, c2) = sel.ordered();
        Some(pane.term.screen().contents_ansi_between(r1, c1, r2, c2))
    }

    fn copy_selection(&self, ctx: &egui::Context) {
        if let Some(ed) = self.active_editor() {
            let text = ed.copy();
            if !text.is_empty() {
                ctx.copy_text(text);
            }
            return;
        }
        if let Some(text) = self.selection_text() {
            if !text.is_empty() {
                ctx.copy_text(text);
            }
        }
    }

    fn find_in_all_tabs(&mut self) {
        let q = self
            .search
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| self.selection_text().map(|s| s.trim().to_string()))
            .unwrap_or_default();
        if q.is_empty() {
            return;
        }
        let ql = q.to_lowercase();
        for (ti, tab) in self.tabs.iter().enumerate() {
            let hit = tab.panes.iter().any(|pane| {
                let screen = pane.term.screen();
                (0..screen.total_lines())
                    .any(|b| screen.line_text_abs(b).to_lowercase().contains(&ql))
            });
            if hit {
                self.active_tab = ti;
                self.selection = None;
                self.search = Some(q);
                self.search_idx = 0;
                self.search_key.clear();
                self.refresh_search();
                self.scroll_to_search_hit();
                return;
            }
        }
    }

    fn paste(&mut self, text: &str) {
        if self.read_only {
            return;
        }
        if let Some(ed) = self.active_editor_mut() {
            if ed.is_view_only() {
                self.large_edit_offer = Some(ed.id.clone());
            } else {
                ed.paste(text);
            }
            self.window.request_redraw();
            return;
        }
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            if tab.attention == Some(Attention::Done) {
                tab.attention = None;
            }
            if let Some(pane) = tab.panes.iter_mut().find(|p| p.id == tab.active) {
                let bytes = input::encode_paste(text, pane.term.screen().bracketed_paste());
                pane.term.write(&bytes);
                pane.scroll = 0;
            }
        }
        self.window.request_redraw();
    }

    /// Menu-bar Copy/Paste/Select All arrive as commands, not key events: the
    /// macOS menu claims ⌘C/⌘V/⌘A before the view sees them. When a text field
    /// (editor, Composer, dialogs) has focus, hand the edit to egui instead of
    /// acting on the terminal. Returns true when egui takes it.
    fn edit_in_text_field(&mut self, event: egui::Event) -> bool {
        if !self.egui_ctx.wants_keyboard_input() {
            return false;
        }
        self.egui_state.egui_input_mut().events.push(event);
        self.window.request_redraw();
        true
    }

    /// The clipboard as text. On Wayland it is read through our data device:
    /// GNOME sends the selection to only one device per client, ours (see
    /// `wayland_dnd`).
    fn clipboard_text(&mut self) -> Option<String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        if let Some(dnd) = self.dnd.as_mut() {
            return dnd.selection_text();
        }
        self.egui_state.clipboard_text()
    }

    fn paste_clipboard(&mut self) {
        // Image pastes go through a file the host owns, so the application does
        // not need osascript to reach the pasteboard (ADR 0036). A text paste
        // clears any earlier image, so a later empty paste cannot read a stale
        // one.
        let path = clipboard_image_path();
        #[cfg(all(unix, not(target_os = "macos")))]
        let image = match self.dnd.as_mut() {
            Some(dnd) => dnd.selection_image(),
            None => mtty_platform::clipboard_image(),
        };
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        let image = mtty_platform::clipboard_image();
        if let Some(image) = image {
            if std::fs::write(&path, image).is_ok() {
                self.paste("");
                return;
            }
        }
        let _ = std::fs::remove_file(&path);
        let text = self.clipboard_text().unwrap_or_default();
        self.paste(&text);
    }

    /// Forward a mouse event to the pane under the pointer when the running
    /// application enabled mouse reporting. Returns true when it was consumed.
    ///
    /// `button`: 0 left, 1 middle, 2 right, 64 wheel-up, 65 wheel-down.
    /// `motion` marks a drag/hover report (bit 5 set, no press/release).
    fn forward_mouse(&mut self, px: f32, py: f32, button: u8, pressed: bool, motion: bool) -> bool {
        if self.read_only {
            return false;
        }
        let scale = self.window.scale_factor() as f32;
        let Some((id, r)) = self.pane_rects().into_iter().find(|(_, r)| {
            px >= r.x * scale
                && px < (r.x + r.w) * scale
                && py >= r.y * scale
                && py < (r.y + r.h) * scale
        }) else {
            return false;
        };
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return false;
        };
        let Some(pane) = tab.panes.iter_mut().find(|p| p.id == id) else {
            return false;
        };
        let Some((clicks, motion_mode, drag_mode, sgr)) = pane.term.screen().mouse_reporting()
        else {
            return false;
        };
        let wheel = button >= 64;
        if motion && !(motion_mode || drag_mode) {
            return false;
        }
        if !motion && !clicks && !wheel {
            return false;
        }
        let cw = self.cw * scale;
        let ch = self.ch * scale;
        let inner = card_inner(r);
        let col = ((px - inner.x * scale) / cw).floor().max(0.0) as u16 + 1;
        let row = ((py - inner.y * scale) / ch).floor().max(0.0) as u16 + 1;
        let Some(seq) = mouse_report(sgr, button, pressed, motion, col, row) else {
            return false;
        };
        pane.term.write(seq.as_bytes());
        self.window.request_redraw();
        true
    }

    /// Evaluate the view rule engine (ADR 0007) for a tab's active pane.
    fn view_for(&self, tab: &Tab) -> Option<mtty_config::view::Resolved> {
        let pane = tab.panes.iter().find(|p| p.id == tab.active)?;
        let agent = self
            .mtp
            .agent_for(&pane.id)
            .and_then(|a| a.get("agent").and_then(|v| v.as_str()).map(str::to_string));
        let cwd = pane.term.cwd().map(str::to_string);
        // The git branch is known for the directory the details worker last
        // looked at; only use it when that is this pane's directory.
        let branch = self
            .details_data
            .as_ref()
            .filter(|_| {
                self.details_cwd
                    .as_deref()
                    .map(|p| p.to_string_lossy().to_string())
                    == cwd
            })
            .and_then(|d| d.git.iter().find(|(k, _)| k == "branch"))
            .and_then(|(_, v)| v.split("...").next())
            .map(|b| b.split_whitespace().next().unwrap_or(b).to_string());
        let index = self
            .tabs
            .iter()
            .position(|t| t.active == tab.active)
            .map(|i| i + 1);
        let ctx = mtty_config::view::Context {
            cwd,
            command: pane.term.foreground_command(),
            agent,
            host: tab.ssh_target.as_deref().map(ssh_host),
            file: pane.term.foreground_file(),
            user: std::env::var("USER").ok(),
            shell: std::env::var("SHELL").ok(),
            branch,
            osc_title: pane.term.title().map(str::to_string),
            index,
        };
        self.rules.evaluate(&ctx)
    }

    fn title_of(&self, tab: &Tab) -> String {
        self.title_with(tab, self.view_for(tab))
    }

    /// The displayed title, given the tab's evaluated view rules.
    fn title_with(&self, tab: &Tab, view: Option<mtty_config::view::Resolved>) -> String {
        let restored = tab
            .panes
            .iter()
            .find(|p| p.id == tab.active)
            .and_then(|p| restored_title(tab.shown.as_deref(), p.term.cwd(), p.term.title()));
        if let Some(fixed) = fixed_tab_title(tab.title_set, &tab.title, restored.as_deref()) {
            return fixed;
        }
        if let Some(ed) = tab.editors.iter().find(|e| e.id == tab.active) {
            return ed.title();
        }
        if let Some(res) = view {
            if !res.title.is_empty() {
                return res.title;
            }
            if let Some(alias) = res.alias {
                return alias;
            }
        }
        // Fall back to the program title, then the cwd folder, then "shell N".
        if let Some(t) = tab
            .panes
            .iter()
            .find(|p| p.id == tab.active)
            .and_then(|p| p.term.title().map(str::to_string))
            .filter(|s| !s.is_empty())
        {
            return t;
        }
        tab.panes
            .iter()
            .find(|p| p.id == tab.active)
            .and_then(|p| p.term.cwd().map(str::to_string))
            .and_then(|c| {
                std::path::Path::new(&c)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| tab.title.clone())
    }

    /// Keep terminal control independent of painting: occluded/minimized
    /// windows still accept MTP input, commands, and session housekeeping.
    fn poll_control_plane(&mut self) {
        let writes = self.mtp.take_writes();
        let commands = self.mtp.take_commands();
        let changed = !writes.is_empty() || !commands.is_empty();
        // MTP `pane.send` / `pane.run` — inject bytes into the target pane.
        for (pane_id, data) in writes {
            for tab in &mut self.tabs {
                if let Some(p) = tab.panes.iter_mut().find(|p| p.id == pane_id) {
                    p.term.write(&data);
                    p.scroll = 0;
                }
                // An editor pane takes the text at its carets (`run`'s
                // trailing CR becomes a line break), as one undo step.
                if let Some(e) = tab.editors.iter_mut().find(|e| e.id == pane_id) {
                    let text = String::from_utf8_lossy(&data)
                        .replace("\r\n", "\n")
                        .replace('\r', "\n");
                    e.paste(&text);
                }
            }
        }
        // MTP `pane.focus` / `pane.close` / `app.view` / `app.edit`.
        for command in commands {
            match command {
                mtty_mtp::Command::Focus(id) => {
                    let found = self.tabs.iter().position(|t| t.has_pane(&id));
                    if let Some(ti) = found {
                        self.tabs[ti].active = id.clone();
                        self.active_tab = ti;
                        self.selection = None;
                        for p in self.tabs[ti].panes.iter_mut() {
                            p.scroll = 0;
                        }
                    }
                }
                mtty_mtp::Command::Close(id) => self.close_pane_id(&id),
                mtty_mtp::Command::View { path, line } => {
                    if self.open_editor_ro(std::path::PathBuf::from(path), true) {
                        self.go_active_editor_to(line, None);
                    }
                }
                mtty_mtp::Command::Edit { path, line, column } => {
                    if self.open_editor_ro(std::path::PathBuf::from(path), false) {
                        self.go_active_editor_to(line, column);
                    }
                }
                mtty_mtp::Command::ResumeAgent {
                    agent,
                    session,
                    cwd,
                } => {
                    self.resume_agent(&agent, &session, cwd.as_deref());
                }
                mtty_mtp::Command::Propose {
                    pane,
                    path,
                    edits,
                    text,
                    label,
                } => {
                    self.apply_proposal(pane, path, edits, text, label);
                }
            }
        }
        if changed {
            self.retain_live_renderers();
            self.window.request_redraw();
            if let Some(pip) = &self.pip {
                pip.window.request_redraw();
            }
        }
    }

    fn render(&mut self) {
        let _render_timer = resource_metrics::RenderTimer::start();
        let frame_start = Instant::now();
        if !self.redraw.begin(
            frame_start,
            self.focused,
            window_drawable(&self.window, self.occluded),
        ) {
            return;
        }
        if self.focused {
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                tab.attention = Attention::seen(tab.attention);
            }
        }
        self.refresh_search();
        self.maybe_request_hover();
        self.poll_details();
        self.ensure_details();

        // Sync the window title from the active pane's OSC 0/2.
        let title = self
            .tabs
            .get(self.active_tab)
            .map(|t| self.title_of(t))
            .filter(|s| !s.is_empty());
        if title != self.last_title {
            if let Some(t) = &title {
                self.window.set_title(t);
            }
            self.last_title = title;
        }

        // Build per-pane draw data (owning; releases the self.tabs borrow).
        let scale = self.window.scale_factor() as f32;
        let cw = self.cw * scale;
        let ch = self.ch * scale;
        let theme = self.theme.clone();
        let panel_bg = theme.chrome().bg;
        let window_bg = panel_bg;
        let selection = self.selection.clone();
        let rects = self.pane_rects();
        let active_id = self.active_pane_id().unwrap_or_default();
        let search_on = self.search.as_ref().map(|s| !s.is_empty()).unwrap_or(false);
        let search_hits = self.search_hits.clone();
        let search_idx = self.search_idx;
        let quick_hit = self.quick_hit;
        // An editor's Find matches, when they are char ranges (view-mode
        // matches are file bytes and show as the selection only).
        let editor_hits: &[(usize, usize)] =
            if search_on && !self.bg_search.as_ref().is_some_and(|b| b.bytes) {
                &self.editor_hits
            } else {
                &[]
            };
        let mut draws: Vec<PaneDraw> = Vec::new();
        let mut image_quads: Vec<(u64, i32, u32, ImageInstance)> = Vec::new();
        let mut image_uploads = Vec::new();
        let mut image_keep: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut image_wake: Option<Instant> = None;
        let image_now = Instant::now();
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            for (pane_idx, (id, r)) in rects.iter().enumerate() {
                let Some(pane) = tab.panes.iter_mut().find(|p| &p.id == id) else {
                    if let Some(ed) = tab.editors.iter_mut().find(|e| &e.id == id) {
                        draws.push(draw_editor(
                            ed,
                            id,
                            *r,
                            EditorFrame {
                                scale,
                                cw,
                                ch,
                                theme: &theme,
                                panel_bg,
                                focused: id == &active_id,
                                carets_on: self.cursor_on,
                                matches: if id == &active_id { editor_hits } else { &[] },
                                current_match: search_idx,
                            },
                        ));
                    } else if tab.previews.iter().any(|p| &p.id == id) {
                        // The card only: egui draws the preview inside it.
                        let bg = panel_bg;
                        let card = card_rect(*r);
                        draws.push(PaneDraw {
                            id: id.clone(),
                            rect: card_inner(*r),
                            quads: vec![Quad::rounded(
                                (card.x * scale, card.y * scale),
                                ((card.x + card.w) * scale, (card.y + card.h) * scale),
                                (bg.0, bg.1, bg.2, 255),
                                CARD_RADIUS * scale,
                            )],
                            rows: Vec::new(),
                        });
                    }
                    continue;
                };
                let inner = card_inner(*r);
                let cols = ((inner.w * scale) / cw).floor().max(1.0) as u16;
                let rows = ((inner.h * scale) / ch).floor().max(1.0) as u16;
                pane.term.resize(rows, cols);
                pane.term.screen_mut().set_scrollback(pane.scroll);
                let (sr, sc) = pane.term.size();
                let ox = inner.x * scale;
                let oy = inner.y * scale;

                let mut quads: Vec<Quad> = Vec::new();
                // Container card (border + terminal background).
                let card = Rect {
                    x: r.x + CARD_MARGIN,
                    y: r.y + CARD_MARGIN,
                    w: (r.w - CARD_MARGIN * 2.0).max(1.0),
                    h: (r.h - CARD_MARGIN * 2.0).max(1.0),
                };
                let bg = panel_bg;
                let radius = CARD_RADIUS * scale;
                quads.push(Quad::rounded(
                    (card.x * scale, card.y * scale),
                    ((card.x + card.w) * scale, (card.y + card.h) * scale),
                    (bg.0, bg.1, bg.2, 255),
                    radius,
                ));
                for row in 0..sr {
                    for col in 0..sc {
                        let Some(cell) = pane.term.screen().cell(row, col) else {
                            continue;
                        };
                        let bg = mtty_ui::cell_background(&theme, &cell);
                        if bg != theme.bg && bg != panel_bg {
                            quads.push(quad(ox, oy, row, col, cw, ch, (bg.0, bg.1, bg.2)));
                        }
                    }
                }
                if let Some((pid, sel)) = &selection {
                    if pid == id {
                        for (row, col) in sel.cells(sc) {
                            let s = theme.selection;
                            quads.push(quad(ox, oy, row, col, cw, ch, (s.0, s.1, s.2)));
                        }
                    }
                }
                if search_on && id == &active_id {
                    let hist = pane.term.screen().history_size() as i32;
                    let off = pane.term.screen().scroll_offset() as i32;
                    for (k, (b, col, width)) in search_hits.iter().enumerate() {
                        let row = *b as i32 - hist + off;
                        if row < 0 || row >= sr as i32 {
                            continue;
                        }
                        let color = if k == search_idx {
                            (0x2e, 0x5b, 0x8f)
                        } else {
                            (0x33, 0x3d, 0x4d)
                        };
                        for dc in 0..*width {
                            quads.push(quad(ox, oy, row as u16, col + dc, cw, ch, color));
                        }
                    }
                }
                // The scrollback match Open Quickly opened.
                if let Some((b, col, width)) = quick_hit {
                    if id == &active_id {
                        let hist = pane.term.screen().history_size() as i32;
                        let off = pane.term.screen().scroll_offset() as i32;
                        let row = b as i32 - hist + off;
                        if row >= 0 && row < sr as i32 {
                            for dc in 0..width {
                                quads.push(quad(
                                    ox,
                                    oy,
                                    row as u16,
                                    col + dc,
                                    cw,
                                    ch,
                                    (0x7a, 0x5f, 0x1d),
                                ));
                            }
                        }
                    }
                }
                // Scrollbar indicator.
                let sb = pane.term.screen().scrollback_len();
                if sb > 0 {
                    let total = (sb + sr as usize) as f32;
                    let track = r.h * scale;
                    let thumb = (track * sr as f32 / total).max(12.0);
                    let pos = pane.term.screen().scroll_offset() as f32 / sb as f32;
                    let y = oy + (track - thumb) * (1.0 - pos);
                    quads.push(Quad::new(
                        (ox + (r.w * scale) - 8.0, y),
                        (ox + (r.w * scale) - 4.0, y + thumb),
                        (0x4c, 0x56, 0x6a, 200),
                    ));
                }
                // Cursor (only on the focused pane, at the bottom).
                let cur = pane.term.screen().cursor_position();
                let show = id == &active_id
                    && self.cursor_on
                    && pane.scroll == 0
                    && !pane.term.screen().hide_cursor()
                    && cur.0 < sr
                    && cur.1 < sc;
                if show {
                    match theme.cursor {
                        mtty_ui::CursorStyle::Block => {
                            let f = theme.fg;
                            quads.push(quad(ox, oy, cur.0, cur.1, cw, ch, (f.0, f.1, f.2)));
                        }
                        mtty_ui::CursorStyle::Bar => {
                            let f = theme.fg;
                            quads.push(Quad::new(
                                (ox + cur.1 as f32 * cw, oy + cur.0 as f32 * ch),
                                (ox + cur.1 as f32 * cw + 2.0, oy + (cur.0 as f32 + 1.0) * ch),
                                (f.0, f.1, f.2, 255),
                            ));
                        }
                        mtty_ui::CursorStyle::Underline => {
                            let f = theme.fg;
                            quads.push(Quad::new(
                                (ox + cur.1 as f32 * cw, oy + (cur.0 as f32 + 1.0) * ch - 2.0),
                                (
                                    ox + (cur.1 as f32 + 1.0) * cw,
                                    oy + (cur.0 as f32 + 1.0) * ch,
                                ),
                                (f.0, f.1, f.2, 255),
                            ));
                        }
                    }
                }

                let cursor_cell = if show { Some(cur) } else { None };
                let rows_data = build_rows(pane.term.screen(), &theme, cursor_cell);
                // Glyph origin is relative to the pane's viewport (the egui-wgpu
                // callback sets the viewport), so we render per-pane with the
                // viewer origin at 0 for the glyph renderer.
                // Inline images (engine layer), drawn under the glyphs.
                if self.graphics_enabled {
                    let off = pane.term.screen().scroll_offset() as i32;
                    let px1 = ox + inner.w * scale;
                    let py1 = oy + inner.h * scale;
                    for im in pane.term.graphics().images.iter() {
                        let fi = if im.animating && im.frames.len() > 1 {
                            (image_now.duration_since(im.anim_start).as_millis()
                                / u128::from(IMAGE_FRAME_MS)) as usize
                                % im.frames.len()
                        } else {
                            0
                        };
                        let frame = &im.frames[fi.min(im.frames.len().saturating_sub(1))];
                        let key = mtty_core::graphics::image_key(id, im.id, fi);
                        image_keep.insert(key);
                        let w = frame.width as f32;
                        let h = frame.height as f32;
                        let bx = ox + im.col as f32 * cw;
                        let by = oy + (im.anchor + off) as f32 * ch;
                        let (x0, y0, x1, y1) = if let (Some(c), Some(r)) = (im.cols, im.rows) {
                            // Explicit cell footprint: fit inside it, centred.
                            let (tw, th) = (c as f32 * cw, r as f32 * ch);
                            let s = (tw / w).min(th / h);
                            let (dw, dh) = (w * s, h * s);
                            let (x, y) = (bx + (tw - dw) / 2.0, by + (th - dh) / 2.0);
                            (x, y, x + dw, y + dh)
                        } else {
                            let (x, y) = (bx + im.x_off as f32, by + im.y_off as f32);
                            (x, y, x + w, y + h)
                        };
                        if x1 < ox || y1 < oy || x0 > px1 || y0 > py1 {
                            continue;
                        }
                        if im.animating && im.frames.len() > 1 {
                            let at = next_image_frame(im.anim_start, image_now);
                            image_wake = Some(image_wake.map_or(at, |previous| previous.min(at)));
                        }
                        if !self.images.has(key) {
                            image_uploads.push((key, Arc::clone(frame)));
                        }
                        image_quads.push((
                            key,
                            im.z,
                            pane_idx as u32,
                            ImageInstance {
                                min: [x0, y0],
                                max: [x1, y1],
                                uv_min: [0.0, 0.0],
                                uv_max: [1.0, 1.0],
                            },
                        ));
                    }
                }
                draws.push(PaneDraw {
                    id: id.clone(),
                    rect: inner,
                    quads,
                    rows: rows_data,
                });
            }
        }

        // Split dividers between panes (a 1px `[divider]` token).
        if let Some(tab) = self.tabs.get(self.active_tab) {
            let border = theme.chrome().hover;
            let mut divider_quads = Vec::new();
            for h in tab.layout.handles(self.grid_area()) {
                let (x0, y0, x1, y1) = match h.dir.axis() {
                    SplitDir::Right => {
                        let cx = h.rect.x + h.rect.w / 2.0;
                        (cx - 0.5, h.rect.y, cx + 0.5, h.rect.y + h.rect.h)
                    }
                    SplitDir::Down => {
                        let cy = h.rect.y + h.rect.h / 2.0;
                        (h.rect.x, cy - 0.5, h.rect.x + h.rect.w, cy + 0.5)
                    }
                    SplitDir::Left | SplitDir::Up => unreachable!("axis() yields only Right/Down"),
                };
                divider_quads.push(Quad::new(
                    (x0 * scale, y0 * scale),
                    (x1 * scale, y1 * scale),
                    (border.0, border.1, border.2, 255),
                ));
            }
            if let Some(last) = draws.last_mut() {
                last.quads.extend(divider_quads);
            }
        }

        // Upload any new inline images, drop textures for images that are gone.
        for (key, frame) in &image_uploads {
            self.images.upload(
                &self.device,
                &self.queue,
                *key,
                frame.width,
                frame.height,
                &frame.rgba,
            );
        }
        // Only visible animations schedule a redraw, at the next actual frame
        // change rather than continuously at the display's refresh rate.
        self.image_wake = image_wake;
        self.images.retain(&image_keep);

        // GPU: quads (all panes) then per-pane glyphs.
        let all_quads: Vec<Quad> = draws.iter().flat_map(|d| d.quads.iter().copied()).collect();
        self.quads
            .prepare(&self.device, &self.queue, self.window_size(), &all_quads);
        self.images
            .prepare(&self.device, &self.queue, self.window_size(), &image_quads);
        let win_size = self.window_size();
        let format = self.config.format;
        for d in &draws {
            let device = &self.device;
            let queue = &self.queue;
            let renderer = self
                .renderers
                .entry(d.id.clone())
                .or_insert_with(|| TermRenderer::new(device, queue, format));
            // The pass viewport is the whole surface, so glyphon needs the full
            // resolution and the pane's origin as a logical offset.
            renderer.prepare(
                device,
                queue,
                win_size,
                scale,
                self.font_size,
                (self.font_size * self.line_ratio).round(),
                self.cw,
                d.rect.x,
                d.rect.y,
                (theme.fg.0, theme.fg.1, theme.fg.2),
                self.font_family.as_deref(),
                &d.rows,
            );
        }

        // egui chrome.
        let raw = self.egui_state.take_egui_input(&self.window);
        let events = raw.events.clone();
        let egui_ctx = self.egui_ctx.clone();
        let output = egui_ctx.run(raw, |ctx| self.chrome(ctx));
        // Honour delayed animations as well as immediate widget changes,
        // without a self-sustaining, uncapped request_redraw loop.
        let repaint_delay = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay)
            .unwrap_or(Duration::MAX);
        self.redraw.repaint_after(frame_start, repaint_delay);
        // A press on a panel edge belongs to the UI (see `resize_cursor`).
        self.ui_resize_hover = chrome::resize_cursor(output.platform_output.cursor_icon);
        let mut platform_output = output.platform_output;
        keep_ime_on(&mut platform_output, self.ime_area);
        self.egui_state
            .handle_platform_output(&self.window, platform_output);
        let ppp = self.egui_ctx.pixels_per_point();
        let paint_jobs = self.egui_ctx.tessellate(output.shapes, ppp);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point: ppp,
        };
        for (id, delta) in &output.textures_delta.set {
            self.egui_renderer
                .update_texture(&self.device, &self.queue, *id, delta);
        }
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        self.egui_renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &paint_jobs,
            &screen,
        );
        if self.shot_now
            || mtty_config::env("SHOT").is_some()
            || std::env::var_os("MIAOTTY_NATIVE_SHOT").is_some()
        {
            self.capture(
                &draws,
                ImageLayer {
                    quads: &image_quads,
                    rects: &rects,
                    scale,
                },
                window_bg,
                &paint_jobs,
                &screen,
            );
        }

        let Ok(frame) = self.surface.get_current_texture() else {
            return;
        };
        let view = frame.texture.create_view(&Default::default());
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("terminal"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(linear_color(window_bg, self.opacity)),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                })
                .forget_lifetime();
            self.quads.render(&mut pass);
            // Negative-z placements sit behind the text grid; the default and
            // positive-z placements draw over it. The glyph pass runs between
            // the two image layers.
            if !image_quads.is_empty() {
                self.render_image_layer(&mut pass, &draws, scale, true);
            }
            for d in &draws {
                if let Some(renderer) = self.renderers.get(&d.id) {
                    renderer.render(&mut pass);
                }
            }
            if !image_quads.is_empty() {
                self.render_image_layer(&mut pass, &draws, scale, false);
            }
            self.egui_renderer.render(&mut pass, &paint_jobs, &screen);
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        resource_metrics::presented(false);
        for id in &output.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }

        for ev in &events {
            if self.egui_ctx.wants_keyboard_input() {
                break;
            }
            match ev {
                egui::Event::Copy => self.copy_selection(&self.egui_ctx),
                egui::Event::Paste(text) => self.paste(text),
                egui::Event::Cut if !self.read_only => {
                    if let Some(ed) = self.active_editor_mut() {
                        let text = ed.cut();
                        if !text.is_empty() {
                            self.egui_ctx.copy_text(text);
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(pip) = &self.pip {
            pip.window.request_redraw();
        }
        self.retain_live_renderers();
    }

    /// Draw one layer of every pane's inline images, clipped to its pane.
    ///
    /// `behind` selects the negative-z placements that must sit under the glyph
    /// grid; `false` draws the default and positive-z placements above it.
    fn render_image_layer(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        draws: &[PaneDraw],
        scale: f32,
        behind: bool,
    ) {
        for (pane_idx, d) in draws.iter().enumerate() {
            let sx = (d.rect.x * scale).max(0.0) as u32;
            let sy = (d.rect.y * scale).max(0.0) as u32;
            let sw = ((d.rect.w * scale) as u32).min(self.config.width.saturating_sub(sx));
            let sh = ((d.rect.h * scale) as u32).min(self.config.height.saturating_sub(sy));
            if sw == 0 || sh == 0 {
                continue;
            }
            pass.set_scissor_rect(sx, sy, sw, sh);
            self.images.render(pass, pane_idx as u32, behind);
        }
        pass.set_scissor_rect(0, 0, self.config.width, self.config.height);
    }

    /// Release the glyph atlases and font systems of panes that no longer exist.
    /// A `TermRenderer` owns a whole `FontSystem` (including the CJK face) plus
    /// GPU atlas buffers, so a closed split, editor, preview, or tab must not
    /// leave one behind for the life of the process.
    fn retain_live_renderers(&mut self) {
        if self.renderers.is_empty() {
            return;
        }
        let live: std::collections::HashSet<&str> = self
            .tabs
            .iter()
            .flat_map(|tab| {
                tab.panes
                    .iter()
                    .map(|pane| pane.id.as_str())
                    .chain(tab.editors.iter().map(|editor| editor.id.as_str()))
                    .chain(tab.previews.iter().map(|preview| preview.id.as_str()))
            })
            .collect();
        self.renderers.retain(|id, _| live.contains(id.as_str()));
    }

    fn chrome(&mut self, ctx: &egui::Context) {
        // Shared, host-agnostic chrome (menu/tabs/sidebar/details/status).
        chrome::render(ctx, self);
        self.pane_context_menu(ctx);
        self.live_markdown_panes(ctx);
        self.preview_panes(ctx);
        self.lsp_popups(ctx);
        if self.update_dialog {
            use mtty_ui::i18n::t;
            let lang = self.lang;
            let checking = self.update_rx.is_some();
            let mut open = true;
            let mut dismiss = false;
            let mut retry = false;
            let mut download: Option<mtty_ui::update::Artifact> = None;
            let mut install: Option<std::path::PathBuf> = None;
            egui::Window::new(t(lang, "Software Update", "软件更新"))
                .id(egui::Id::new("software_update"))
                .open(&mut open)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .collapsible(false)
                .resizable(false)
                .default_width(340.0)
                .show(ctx, |ui| {
                    ui.add_space(8.0);
                    if checking {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(t(lang, "Checking for updates…", "正在检查更新…"));
                        });
                    } else {
                        match &self.update_result {
                            Some(UpdateResult::Available { version, artifact }) => {
                                ui.heading(t(lang, "A new version is available", "发现新版本"));
                                ui.add_space(6.0);
                                ui.label(format!("mtty {version}"));
                                ui.label(format!(
                                    "{} {}",
                                    t(lang, "Current version:", "当前版本："),
                                    env!("CARGO_PKG_VERSION")
                                ));
                                match artifact {
                                    None => {
                                        ui.label(t(
                                            lang,
                                            "No download is available for this platform.",
                                            "暂未提供此平台的下载。",
                                        ));
                                    }
                                    Some(a) if a.signature.is_none() => {
                                        ui.label(t(
                                            lang,
                                            "This download is not signed, so mtty will not install it.",
                                            "该下载未签名,mtty 不会自动安装。",
                                        ));
                                    }
                                    Some(_) => {}
                                }
                                ui.add_space(4.0);
                                match &self.update_install {
                                    UpdateInstall::Working => {
                                        ui.horizontal(|ui| {
                                            ui.spinner();
                                            ui.label(t(
                                                lang,
                                                "Downloading and checking the signature…",
                                                "正在下载并校验签名…",
                                            ));
                                        });
                                    }
                                    UpdateInstall::Ready(_) => {
                                        ui.label(t(
                                            lang,
                                            "Downloaded; checksum and signature verified.",
                                            "已下载;校验和与签名均已验证。",
                                        ));
                                    }
                                    UpdateInstall::Failed(e) => {
                                        ui.label(
                                            egui::RichText::new(e)
                                                .color(chrome_rgb(self.theme.chrome().negative)),
                                        );
                                    }
                                    UpdateInstall::Idle => {}
                                }
                            }
                            Some(UpdateResult::Failed(error)) => {
                                ui.heading(t(lang, "Unable to check for updates", "无法检查更新"));
                                ui.add_space(6.0);
                                ui.label(error);
                            }
                            _ => {}
                        }
                    }
                    ui.add_space(16.0);
                    button_row(ui, |ui| {
                        match &self.update_result {
                            Some(UpdateResult::Available {
                                artifact: Some(a), ..
                            }) if !checking => match (&self.update_install, &a.signature) {
                                (UpdateInstall::Ready(path), _) => {
                                    if ui
                                        .button(t(lang, "Install and Relaunch", "安装并重启"))
                                        .clicked()
                                    {
                                        install = Some(path.clone());
                                    }
                                }
                                (UpdateInstall::Working, _) => {}
                                (_, Some(_)) => {
                                    if ui.button(t(lang, "Download Update", "下载更新")).clicked() {
                                        download = Some(a.clone());
                                    }
                                }
                                (_, None) => {
                                    if ui
                                        .button(t(lang, "Open Download Page", "打开下载页"))
                                        .clicked()
                                    {
                                        open_external(&a.url);
                                        dismiss = true;
                                    }
                                }
                            },
                            Some(UpdateResult::Failed(_)) if !checking => {
                                retry = ui.button(t(lang, "Try Again", "重试")).clicked();
                            }
                            _ => {}
                        }
                        dismiss |= ui.button(t(lang, "Close", "关闭")).clicked();
                    });
                    ui.add_space(4.0);
                });
            self.update_dialog =
                open && !dismiss && !ctx.input(|i| i.key_pressed(egui::Key::Escape));
            if retry {
                self.check_updates();
            }
            if let Some(artifact) = download {
                self.start_update_download(artifact);
            }
            if let Some(path) = install {
                self.install_update(&path);
            }
        }
        // Hyperlink hover cue: hand cursor + underline while Cmd/Ctrl is held.
        let link = if self.mods.super_key() || self.mods.control_key() {
            self.link_at_pointer()
        } else {
            None
        };
        let want = link.is_some();
        if want != self.hover_pointer {
            self.hover_pointer = want;
            self.window.set_cursor(if want {
                winit::window::CursorIcon::Pointer
            } else {
                winit::window::CursorIcon::Default
            });
        }
        if self.hint_mode && !self.hints.is_empty() {
            if let Some(inner) = self.active_inner() {
                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("hints"),
                ));
                for h in &self.hints {
                    let x = inner.x + h.col as f32 * self.cw;
                    let y = inner.y + h.row as f32 * self.ch;
                    let rect = egui::Rect::from_min_size(
                        egui::pos2(x, y),
                        egui::vec2(self.cw * 1.6, self.ch),
                    );
                    painter.rect_filled(
                        rect,
                        egui::Rounding::same(3.0),
                        chrome_rgb(self.theme.chrome().warning),
                    );
                    painter.text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        &h.label,
                        egui::FontId::monospace(11.0),
                        egui::Color32::BLACK,
                    );
                }
            }
        }
        // Where the OS candidate window goes while no text field has focus:
        // the terminal cursor or the editor caret (see `keep_ime_on`).
        self.ime_area = if ctx.wants_keyboard_input() {
            None
        } else {
            self.active_cursor().map(|(inner, (row, col))| {
                egui::Rect::from_min_size(
                    egui::pos2(
                        (inner.x + col as f32 * self.cw).round(),
                        (inner.y + row as f32 * self.ch).round(),
                    ),
                    egui::vec2(self.cw, self.ch),
                )
            })
        };
        if !self.preedit.is_empty() {
            if let Some((inner, (row, col))) = self.active_cursor() {
                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("ime_preedit"),
                ));
                let ch = self.theme.chrome();
                let col_of = |c: mtty_ui::theme::Rgb| egui::Color32::from_rgb(c.0, c.1, c.2);
                let galley = painter.layout_no_wrap(
                    self.preedit.clone(),
                    egui::FontId::proportional(14.0),
                    col_of(ch.text),
                );
                let pos = egui::pos2(
                    inner.x + col as f32 * self.cw,
                    inner.y + row as f32 * self.ch,
                );
                let rect = egui::Rect::from_min_size(
                    pos,
                    egui::vec2(galley.size().x.max(self.cw), self.ch.max(galley.size().y)),
                );
                painter.rect_filled(rect, egui::Rounding::same(2.0), col_of(ch.active));
                painter.galley(pos, galley, col_of(ch.text));
                painter.line_segment(
                    [
                        egui::pos2(rect.min.x, rect.max.y),
                        egui::pos2(rect.max.x, rect.max.y),
                    ],
                    egui::Stroke::new(1.0_f32, col_of(ch.accent)),
                );
            }
        }
        if let Some(h) = link {
            let y = h.inner.y + (h.row as f32 + 1.0) * self.ch - 1.5;
            let x0 = h.inner.x + h.start as f32 * self.cw;
            let x1 = h.inner.x + (h.end as f32 + 1.0) * self.cw;
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("link_underline"),
            ));
            painter.line_segment(
                [egui::pos2(x0, y), egui::pos2(x1, y)],
                egui::Stroke::new(1.0_f32, chrome::fg_color(&self.theme)),
            );
        }
        if self.dropping {
            self.draw_drop_targets(ctx);
        }
        // Host-specific overlay windows.
        self.palette_window(ctx);
        self.settings_window(ctx);
        self.open_dialog_window(ctx);
        self.editor_window(ctx);
        self.recipe_dialog_window(ctx);
        self.ssh_dialog_window(ctx);
        self.transport_dialog_window(ctx);
        self.key_import_window(ctx);
        self.acp_start_window(ctx);
        self.acp_window(ctx);
        self.task_dialog_window(ctx);
        self.tasks_window(ctx);
        self.hosts_window(ctx);
        self.sftp_window(ctx);
        self.snippets_window(ctx);
        self.ftp_dialog_window(ctx);
        self.sync_window(ctx);
        self.remote_dialog_window(ctx);
        self.composer_window(ctx);
        self.quick_window(ctx);
        self.search_window(ctx);
        self.goto_line_window(ctx);
        self.go_to_symbol_window(ctx);
        self.resume_picker_window(ctx);
        self.vim_command_window(ctx);
        self.large_edit_window(ctx);
        self.reload_dialog_window(ctx);
        if let Some(i) = self.renaming {
            self.rename_window(ctx, i);
        }
        if let Some(i) = self.mark_renaming {
            let mut buf = std::mem::take(&mut self.mark_buf);
            let mut slot = self.mark_renaming;
            self.tab_text_window(ctx, i, "Tab Mark", "标签标记", &mut buf, &mut slot, false);
            self.mark_buf = buf;
            self.mark_renaming = slot;
        }
        if let Some(i) = self.group_renaming {
            let mut buf = std::mem::take(&mut self.group_buf);
            let mut slot = self.group_renaming;
            self.tab_text_window(ctx, i, "Tab Group", "标签分组", &mut buf, &mut slot, true);
            self.group_buf = buf;
            self.group_renaming = slot;
        }
        if let Some(i) = self.prefix_renaming {
            self.prefix_window(ctx, i);
        }
    }
    fn details_content(&self, tab: usize) -> (&'static str, Vec<(String, String)>) {
        match tab {
            1 => ("Agent", self.agent_rows()),
            2 => ("Outline", self.outline_rows()),
            3 => (
                "Git",
                self.details_data
                    .as_ref()
                    .map(|d| d.git.clone())
                    .unwrap_or_default(),
            ),
            4 => ("Files", Vec::new()),
            5 => (
                "Ports",
                self.details_data
                    .as_ref()
                    .map(|d| d.ports.clone())
                    .unwrap_or_default(),
            ),
            _ => ("Info", self.details_rows()),
        }
    }

    /// Persist tabs/panes/cwd/layout so the next launch restores the session.
    fn session_value(&self) -> serde_json::Value {
        let tabs: Vec<_> = self
            .tabs
            .iter()
            .map(|t| {
                let mut value = Tab::session_value(t);
                // Record the title the tab is showing now, so a restart shows
                // the same title even for a tab the user never renamed (its
                // live title comes from the pane's OSC name or cwd).
                if let Some(obj) = value.as_object_mut() {
                    obj.insert(
                        "shown".into(),
                        serde_json::json!(self.title_with(t, self.view_for(t))),
                    );
                }
                value
            })
            .collect();
        serde_json::json!({
            "active_tab": self.active_tab,
            "tabs": tabs,
            "recent": self.recent_files,
            "counts": self.open_counts,
        })
    }

    fn save_session(&self) {
        if let Some(path) = session_file() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(
                path,
                serde_json::to_vec(&self.session_value()).unwrap_or_default(),
            );
        }
    }

    /// Remember the window's chrome layout (size, panel widths, which panels
    /// are open, the details tab) so the next launch restores it.
    fn save_window_state(&self) {
        let size = self.window.inner_size();
        let scale = self.window.scale_factor() as f32;
        WindowState {
            size: (size.width as f32 / scale, size.height as f32 / scale),
            sidebar_w: self.sidebar_w,
            details_w: self.details_w,
            sidebar_open: self.show_sidebar,
            details_open: self.show_details,
            details_tab: self.details_tab.min(6),
        }
        .save();
    }

    fn toggle_sidebar(&mut self) {
        self.show_sidebar = !self.show_sidebar;
        self.save_window_state();
    }

    fn toggle_details(&mut self) {
        self.show_details = !self.show_details;
        self.save_window_state();
    }

    /// The session as mtty quits: also each terminal's contents (when
    /// `restore-scrollback` is on) and the program running in it, so the
    /// next launch can show them and offer to run it again.
    fn save_session_on_exit(&mut self) {
        let mut value = self.session_value();
        let dir = scrollback_dir();
        if let Some(dir) = &dir {
            // Contents from the last session that were never restored.
            let _ = std::fs::remove_dir_all(dir);
        }
        for (t, tab) in self.tabs.iter_mut().enumerate() {
            for pane in &mut tab.panes {
                let mut extra = serde_json::Map::new();
                if !tab.ssh {
                    if let Some(args) = pane.term.foreground_args() {
                        let line: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
                        extra.insert("command".into(), line.join(" ").into());
                    }
                }
                if self.restore_scrollback {
                    let (text, images) = pane.term.snapshot_scrollback(SCROLLBACK_LINES);
                    let file = format!("{}.ansi", pane.id);
                    if let Some(dir) = dir.as_deref().filter(|_| !text.is_empty()) {
                        if write_private(dir, &file, text.as_bytes()).is_ok() {
                            extra.insert("scrollback".into(), file.into());
                        }
                        if let Some(images) = images {
                            let name = format!("{}.images.json", pane.id);
                            if let Ok(json) = serde_json::to_vec(&images) {
                                if write_private(dir, &name, &json).is_ok() {
                                    extra.insert("images".into(), name.into());
                                }
                            }
                        }
                    }
                }
                if extra.is_empty() {
                    continue;
                }
                let saved = value["tabs"][t]["panes"]
                    .as_array_mut()
                    .and_then(|panes| panes.iter_mut().find(|p| p["id"] == pane.id.as_str()));
                if let Some(serde_json::Value::Object(saved)) = saved {
                    saved.extend(extra);
                }
            }
        }
        if let Some(path) = session_file() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, serde_json::to_vec(&value).unwrap_or_default());
        }
    }

    /// A restored pane: show what it held at quit, then offer the program
    /// that was running (never run it unasked).
    fn restore_pane_contents(&self, pane: &mut Pane, saved: &serde_json::Value) {
        let t = |en, zh| mtty_ui::i18n::t(self.lang, en, zh);
        // Named by a clean quit; after a crash, the periodic save's file for
        // the pane's id.
        let file = saved["scrollback"]
            .as_str()
            .map(str::to_string)
            .or_else(|| saved["id"].as_str().map(|id| format!("{id}.ansi")))
            .filter(|f| is_plain_file_name(f));
        if let Some(path) = file.and_then(|f| scrollback_dir().map(|d| d.join(f))) {
            if let Ok(text) = std::fs::read(&path) {
                let text = without_mtty_notes(&String::from_utf8_lossy(&text));
                pane.term.screen_mut().process(text.as_bytes());
                // Held back until the pane has its final width: the note's own
                // rows would otherwise be the anchor for a saved image.
                pane.pending_note = Some(format!(
                    "\x1b[0;2m[mtty] {}\x1b[0m\r\n",
                    t("Restored from the last session.", "以上为上次会话的内容。")
                ));
                pane.pending_images = load_pending_images(saved);
            }
            // Shown once: the next quit saves the contents afresh.
            let _ = std::fs::remove_file(&path);
        }
        if let Some(command) = saved["command"].as_str().filter(|c| !c.trim().is_empty()) {
            let note = format!(
                "\x1b[0;2m[mtty] {} {command}  {}\x1b[0m\r\n",
                t("Was running:", "上次正在运行:"),
                t("Press Enter to run it again.", "按回车重新运行。"),
            );
            // Same deferred path as the restore note: after the images.
            pane.pending_note = Some(match pane.pending_note.take() {
                Some(previous) => previous + &note,
                None => note,
            });
            pane.on_enter = Some(format!("{command}\r"));
        }
    }

    /// Quit leaving hosted programs running for the next launch to reattach
    /// (an update, a relaunch; ADR 0041): each hosted pane's exact screen and
    /// output offset are saved, then its host is detached. Panes without a
    /// host end as on any quit.
    fn quit_keeping_sessions(&mut self) {
        let all: Vec<usize> = (0..self.tabs.len()).collect();
        if !self.confirm_close_tabs(&all) {
            return;
        }
        self.save_window_state();
        if self.show_settings {
            self.persist_settings();
        }
        self.save_session_on_exit();
        self.keep_hosts();
        self.sleep.set_awake(false);
        std::process::exit(0);
    }

    /// Save each hosted pane's exact screen and output offset and detach its
    /// host, so the next launch reattaches (ADR 0041). Call after
    /// `save_session_on_exit`, which clears the folder these go to.
    fn keep_hosts(&mut self) {
        let dir = scrollback_dir();
        for pane in self.tabs.iter_mut().flat_map(|t| t.panes.iter_mut()) {
            if let (Some(dir), Some(snapshot)) =
                (dir.as_deref(), pane.term.host_snapshot(SCROLLBACK_LINES))
            {
                if let Ok(json) = serde_json::to_vec(&snapshot) {
                    let _ = write_private(dir, &format!("{}.host.json", pane.id), &json);
                }
            }
            pane.term.detach_host();
        }
    }

    /// How an ordinary quit leaves hosted programs: running for the next
    /// launch with `keep-sessions-on-quit`, ended otherwise.
    fn leave_hosts(&mut self) {
        if self.keep_sessions_on_quit {
            self.keep_hosts();
        } else {
            self.end_hosts();
        }
    }

    /// The app is quitting without keeping sessions: end every hosted
    /// program (exiting skips the terminals' destructors).
    fn end_hosts(&mut self) {
        for pane in self.tabs.iter_mut().flat_map(|t| t.panes.iter_mut()) {
            pane.term.end_host();
        }
    }

    /// Save each terminal's contents every minute, so a crash loses at most
    /// that much (a clean quit saves everything in `save_session_on_exit`).
    /// Panes under a full-screen program are saved too: the snapshot reads
    /// the shell output beneath without disturbing the program's screen.
    fn save_scrollback_periodically(&mut self) {
        if !self.restore_scrollback || self.scrollback_saved_at.elapsed() < SCROLLBACK_SAVE_EVERY {
            return;
        }
        self.scrollback_saved_at = Instant::now();
        let Some(dir) = scrollback_dir() else {
            return;
        };
        for pane in self.tabs.iter_mut().flat_map(|t| t.panes.iter_mut()) {
            let (text, images) = pane.term.snapshot_scrollback(SCROLLBACK_LINES);
            if !text.is_empty() {
                let _ = write_private(&dir, &format!("{}.ansi", pane.id), text.as_bytes());
                if let Some(images) = images {
                    if let Ok(json) = serde_json::to_vec(&images) {
                        let _ = write_private(&dir, &format!("{}.images.json", pane.id), &json);
                    }
                }
            }
            // A hosted pane also keeps its exact screen and output offset, so
            // after a crash it reattaches replaying only what came after.
            if let Some(snapshot) = pane.term.host_snapshot(SCROLLBACK_LINES) {
                if let Ok(json) = serde_json::to_vec(&snapshot) {
                    let _ = write_private(&dir, &format!("{}.host.json", pane.id), &json);
                }
            }
        }
    }

    /// Persist the prompt queue so it survives a restart.
    fn save_queue(&self) {
        if let Some(path) = queue_file() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let body = self.prompt_queue.to_json();
            let _ = std::fs::write(path, serde_json::to_vec(&body).unwrap_or_default());
        }
    }

    /// Drop every tab (and its panes/shells), e.g. before opening a recipe.
    fn clear_tabs(&mut self) {
        self.tabs.clear();
        self.selection = None;
        self.active_tab = 0;
    }

    fn restore_from_value(&mut self, v: &serde_json::Value) -> bool {
        let Some(value) = session::normalize(v.clone()) else {
            return false;
        };
        let v = &value;
        let Some(tabs) = v.get("tabs").and_then(|t| t.as_array()) else {
            return false;
        };
        // Panes that reattach to their hosts keep their ids; new ones must
        // not take them.
        for pane in tabs
            .iter()
            .filter_map(|t| t.get("panes")?.as_array())
            .flatten()
            .filter(|p| p.get("host").is_some_and(|h| !h.is_null()))
        {
            if let Some(id) = pane.get("id").and_then(|x| x.as_str()) {
                reserve_id(id);
            }
        }
        // Remote editors are re-fetched after the tabs are in place.
        let mut pending_remote: Vec<(String, String, String, usize, usize)> = Vec::new();
        // Serial/Telnet/TCP tabs reconnect after the rest of the session.
        let mut pending_transport: Vec<(TransportTarget, String)> = Vec::new();
        for t in tabs {
            let title = t
                .get("title")
                .and_then(|x| x.as_str())
                .unwrap_or("shell")
                .to_string();
            let ssh_target = t
                .get("ssh_target")
                .and_then(|x| x.as_str())
                .map(str::to_string);
            // Sessions saved before ssh targets were recorded cannot reconnect:
            // they come back as what they now are, local shells.
            let ssh_cmd = t
                .get("ssh_cmd")
                .and_then(|x| x.as_str())
                .map(str::to_string);
            let ssh = t.get("ssh").and_then(|x| x.as_bool()).unwrap_or(false)
                && (ssh_target.is_some() || ssh_cmd.is_some());
            // A serial/Telnet/TCP tab has no shell: reconnect it once the rest
            // of the session is in place.
            if let Some(target) = t
                .get("transport")
                .and_then(|x| serde_json::from_value::<TransportTarget>(x.clone()).ok())
            {
                pending_transport.push((target, title));
                continue;
            }
            let mut panes = Vec::new();
            let mut map = std::collections::HashMap::new();
            if let Some(arr) = t.get("panes").and_then(|p| p.as_array()) {
                for p in arr {
                    let cwd = p
                        .get("cwd")
                        .and_then(|x| x.as_str())
                        .map(std::path::PathBuf::from);
                    if let Some(pane) = self.reattach_pane(p) {
                        map.insert(pane.id.clone(), pane.id.clone());
                        panes.push(pane);
                        continue;
                    }
                    if let Some(mut pane) = self.spawn_pane(cwd) {
                        self.restore_pane_contents(&mut pane, p);
                        if let Some(old) = p.get("id").and_then(|x| x.as_str()) {
                            map.insert(old.to_string(), pane.id.clone());
                        }
                        panes.push(pane);
                    }
                }
            }
            // Editor panes reopen their files; one whose file is gone is
            // dropped (and pruned from the layout below).
            let mut editors = Vec::new();
            if let Some(arr) = t.get("editors").and_then(|e| e.as_array()) {
                for e in arr {
                    let Some(path) = e.get("path").and_then(|x| x.as_str()) else {
                        continue;
                    };
                    let id = gen_id();
                    // A remote file is re-read from its host after restore.
                    if let Some(dest) = e.get("remote").and_then(|x| x.as_str()) {
                        let cursor = e.get("cursor").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                        let scroll = e.get("scroll").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                        let mut ed = editor_pane::EditorPane::open_remote_pending(
                            id.clone(),
                            dest.to_string(),
                            path.to_string(),
                        );
                        ed.set_vim(self.editor_vim);
                        if let Some(old) = e.get("id").and_then(|x| x.as_str()) {
                            map.insert(old.to_string(), id.clone());
                        }
                        pending_remote.push((
                            id,
                            dest.to_string(),
                            path.to_string(),
                            cursor,
                            scroll,
                        ));
                        editors.push(ed);
                        continue;
                    }
                    if let Ok(mut ed) =
                        editor_pane::EditorPane::open(id.clone(), std::path::Path::new(path))
                    {
                        ed.set_vim(self.editor_vim);
                        if ed.is_view_only() {
                            let line = e.get("line").and_then(|x| x.as_u64()).unwrap_or(0);
                            ed.go_to_line(line as usize);
                            if let Some(old) = e.get("id").and_then(|x| x.as_str()) {
                                map.insert(old.to_string(), id);
                            }
                            editors.push(ed);
                            continue;
                        }
                        let len = ed.doc.rope().len_chars();
                        let cursor = e.get("cursor").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                        ed.doc
                            .set_selection(mtty_editor::Selection::cursor(cursor.min(len)));
                        ed.scroll_line =
                            e.get("scroll").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                        if let Some(old) = e.get("id").and_then(|x| x.as_str()) {
                            map.insert(old.to_string(), id);
                        }
                        editors.push(ed);
                    }
                }
            }
            // Previews follow their editors to the new ids.
            let mut previews = Vec::new();
            if let Some(arr) = t.get("previews").and_then(|e| e.as_array()) {
                for p in arr {
                    let source = p
                        .get("source")
                        .and_then(|x| x.as_str())
                        .and_then(|old| map.get(old).cloned());
                    let Some(source) = source.filter(|s| editors.iter().any(|e| &e.id == s)) else {
                        continue;
                    };
                    let preview = PreviewPane::new(source);
                    if let Some(old) = p.get("id").and_then(|x| x.as_str()) {
                        map.insert(old.to_string(), preview.id.clone());
                    }
                    previews.push(preview);
                }
            }
            let first = panes
                .first()
                .map(|p| p.id.clone())
                .or_else(|| editors.first().map(|e| e.id.clone()));
            let Some(first) = first else {
                continue;
            };
            // Queued prompts follow their panes to the new ids.
            self.prompt_queue.remap(&map);
            let mut layout = t
                .get("layout")
                .and_then(|l| json_to_layout(l, &map))
                .unwrap_or_else(|| Layout::leaf(first.clone()));
            for leaf in layout.ids() {
                let known = panes.iter().any(|p| p.id == leaf)
                    || editors.iter().any(|e| e.id == leaf)
                    || previews.iter().any(|p| p.id == leaf);
                if !known {
                    let _ = layout.remove(&leaf);
                }
            }
            let active = t
                .get("active")
                .and_then(|x| x.as_str())
                .map(|old| map.get(old).cloned().unwrap_or_else(|| old.to_string()))
                .filter(|id| {
                    panes.iter().any(|p| &p.id == id)
                        || editors.iter().any(|e| &e.id == id)
                        || previews.iter().any(|p| &p.id == id)
                })
                .unwrap_or(first);
            // Sessions saved before the flag existed: a title other than the
            // default "shell N" was chosen by the user.
            let title_set = t
                .get("title_set")
                .and_then(|x| x.as_bool())
                .unwrap_or_else(|| !is_default_title(&title));
            let mut tab = Tab {
                layout,
                panes,
                active,
                title,
                title_set,
                shown: t
                    .get("shown")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                ssh,
                ssh_target: ssh_target.filter(|_| ssh),
                ssh_cmd: ssh_cmd.filter(|_| ssh),
                transport: None,
                prefix: None,
                mark: None,
                group: None,
                attention: None,
                editors,
                previews,
            };
            tab.restore_decorations(t);
            if tab.ssh {
                self.offer_reconnect(&mut tab);
            }
            self.tabs.push(tab);
        }
        // Fetch the restored remote files now that their panes exist.
        for (id, dest, path, cursor, scroll) in pending_remote {
            self.spawn_job(move || {
                let result = mtty_ui::ssh::read_remote(&dest, &path);
                JobDone::RemoteRead {
                    id: Some(id),
                    cursor,
                    scroll,
                    dest,
                    path,
                    result,
                }
            });
        }
        // Reconnect the restored serial/Telnet/TCP sessions.
        for (target, title) in pending_transport {
            self.connect_transport(target, Some(title));
        }
        if self.tabs.is_empty() && self.pending_transport_connects == 0 {
            return false;
        }
        self.recent_files = v
            .get("recent")
            .and_then(|r| r.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        self.open_counts =
            serde_json::from_value(v.get("counts").cloned().unwrap_or(serde_json::Value::Null))
                .unwrap_or_default();
        self.active_tab = v.get("active_tab").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        self.publish_panes();
        true
    }

    /// Restore a saved session; returns false if there is nothing to restore.
    fn restore_session(&mut self) -> bool {
        let Some(path) = session_file() else {
            return false;
        };
        let Some(config) = path.parent() else {
            return false;
        };
        // The retired eframe app kept its session in the pre-rename data dir.
        let data = mtty_config::legacy_data_dir();
        let Some(v) = session::load(config, data.as_deref()) else {
            return false;
        };
        let restored = self.restore_from_value(&v);
        // Contents of panes that are gone (closed before a crash) are not
        // kept around; the running panes save afresh on the first frame, so
        // a crash right after a restore loses nothing.
        if let Some(dir) = scrollback_dir() {
            let _ = std::fs::remove_dir_all(dir);
        }
        if let Some(due) = Instant::now().checked_sub(SCROLLBACK_SAVE_EVERY) {
            self.scrollback_saved_at = due;
        }
        restored
    }

    fn active_pane(&self) -> Option<&Pane> {
        let tab = self.tabs.get(self.active_tab)?;
        tab.panes.iter().find(|p| p.id == tab.active)
    }

    fn cwd(&self) -> Option<std::path::PathBuf> {
        self.active_pane()
            .and_then(|p| p.term.cwd().map(std::path::PathBuf::from))
            .or_else(|| {
                self.active_editor()
                    .and_then(|e| e.path.parent().map(std::path::Path::to_path_buf))
            })
    }

    fn agent_rows(&self) -> Vec<(String, String)> {
        let mut rows = Vec::new();
        if let Some(p) = self.active_pane() {
            match self.mtp.agent_for(&p.id) {
                Some(a) => {
                    for k in ["agent", "state", "session_id", "tty"] {
                        if let Some(v) = a.get(k).and_then(|v| v.as_str()) {
                            let v = if k == "state" {
                                agent_state_label(self.lang, v)
                            } else {
                                v
                            };
                            rows.push((k.to_string(), v.to_string()));
                        }
                    }
                    if let Some(line) = a
                        .get("quota")
                        .and_then(|quota| quota_line(quota, self.agent_quota_warn))
                    {
                        rows.push(("quota".into(), line));
                    }
                    if rows.is_empty() {
                        rows.push(("state".into(), a.to_string()));
                    }
                }
                None => rows.push(("agent".into(), "—".into())),
            }
        }
        rows
    }

    fn outline_rows(&self) -> Vec<(String, String)> {
        let mut rows = Vec::new();
        if let Some(p) = self.active_pane() {
            for e in self.mtp.history_for(&p.id).iter().rev().take(200) {
                let cmd = e.get("command").and_then(|v| v.as_str()).unwrap_or("");
                let cwd = e.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
                rows.push((cwd.to_string(), cmd.to_string()));
            }
        }
        rows
    }

    /// Kick off a background refresh of git/files/ports for the active cwd.
    /// While the details panel is hidden only git runs (the status line shows
    /// the branch), and less often.
    fn ensure_details(&mut self) {
        let Some(cwd) = self.cwd() else {
            return;
        };
        let full = self.show_details;
        let interval = Duration::from_secs(if full { 2 } else { 10 });
        let same_dir = self.details_cwd.as_deref() == Some(cwd.as_path());
        let fresh = same_dir && self.details_at.elapsed() < interval;
        if fresh || self.details_rx.is_some() {
            return;
        }
        let pid = self.active_pane().and_then(|p| p.term.pid());
        let proxy = self.proxy.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let cwd2 = cwd.clone();
        let previous = self.details_data.clone().filter(|_| same_dir);
        std::thread::spawn(move || {
            let (files, ports) = if full {
                (files_rows(&cwd2), pid.map(ports_rows).unwrap_or_default())
            } else {
                let keep = previous.unwrap_or_default();
                (keep.files, keep.ports)
            };
            let data = DetailsData {
                git: git_rows(&cwd2),
                files,
                ports,
            };
            let _ = tx.send((cwd2, data));
            let _ = proxy.send_event(HostEvent::Wake);
        });
        self.details_rx = Some(rx);
    }

    fn poll_details(&mut self) {
        let Some(rx) = self.details_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok((cwd, data)) => {
                self.details_cwd = Some(cwd);
                self.details_data = Some(data);
                self.details_at = Instant::now();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => self.details_rx = Some(rx),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
    }

    /// The icon and colour an agent's tab shows (see `agent_icon`), or
    /// `None` when the pane runs no agent.
    fn agent_tab_icon(
        &self,
        tab: &Tab,
    ) -> Option<(mtty_ui::icons::Icon, Option<mtty_ui::theme::Rgb>)> {
        let a = self.mtp.agent_for(&tab.active)?;
        let state = a.get("state").and_then(|v| v.as_str())?;
        shown_agent_icon(&self.badges, &self.theme.chrome(), state, tab.attention)
    }

    fn details_rows(&self) -> Vec<(String, String)> {
        let mut rows = Vec::new();
        if let Some(tab) = self.tabs.get(self.active_tab) {
            if let Some(pane) = tab.panes.iter().find(|p| p.id == tab.active) {
                rows.push(("Title".into(), self.title_of(tab)));
                rows.push((
                    "Directory".into(),
                    pane.term.cwd().unwrap_or("—").to_string(),
                ));
                let (r, c) = pane.term.screen().size();
                rows.push(("Size".into(), format!("{c} × {r}")));
                rows.push(("Pane".into(), pane.id.clone()));
            }
            if let Some(ed) = tab.editors.iter().find(|e| e.id == tab.active) {
                let (line, col) = ed.caret_line_col();
                rows.push(("Title".into(), self.title_of(tab)));
                rows.push(("File".into(), ed.path.display().to_string()));
                let lines = ed.total_lines().to_string();
                match &ed.large {
                    Some(large) if !large.file.indexed() => {
                        rows.push(("Lines".into(), format!("{lines}\u{2026}")));
                    }
                    _ => rows.push(("Lines".into(), lines)),
                }
                if let Some(large) = &ed.large {
                    rows.push(("Mode".into(), "View only".into()));
                    rows.push(("File size".into(), human_bytes(large.file.len_bytes())));
                }
                rows.push(("Cursor".into(), format!("{line}:{col}")));
                rows.push(("Line ending".into(), ed.line_ending_name().into()));
                rows.push((
                    "Language".into(),
                    ed.language().unwrap_or("Plain Text").into(),
                ));
                rows.push(("Pane".into(), ed.id.clone()));
            }
        }
        rows
    }

    fn status_text(&self) -> String {
        use mtty_ui::i18n::t;
        let l = self.lang;
        let mut s = self
            .cwd()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "mtty".to_string());
        // Notices lead the line so a long cwd cannot truncate them away.
        if let Some((msg, until)) = &self.notice {
            if Instant::now() < *until {
                s = format!("{msg}   \u{00b7}   {s}");
            }
        }
        // Git branch (when the details worker has it).
        if let Some(branch) = self.details_data.as_ref().and_then(|d| {
            d.git
                .iter()
                .find(|(k, _)| k == "branch")
                .map(|(_, v)| v.clone())
        }) {
            let b = branch.split_whitespace().next().unwrap_or(&branch);
            if !b.is_empty() {
                s.push_str("   ");
                s.push_str(t(l, "branch", "分支"));
                s.push(' ');
                s.push_str(b);
            }
        }
        let panes = self
            .tabs
            .get(self.active_tab)
            .map(|t| t.panes.len())
            .unwrap_or(0);
        if panes > 1 {
            s.push_str(&format!("   {} {panes}", t(l, "panes", "分屏")));
        }
        if self.broadcast {
            s.push_str("   BROADCAST");
        }
        if !self.tunnels.is_empty() {
            s.push_str(&format!("   \u{21c4} {}", self.tunnels.len()));
        }
        if self.read_only {
            s.push_str("   RO");
        }
        if self
            .update_notice_until
            .is_some_and(|until| Instant::now() < until)
        {
            s.push_str("   \u{00b7}   ");
            s.push_str(t(l, "You're up to date", "已是最新版本"));
            s.push_str(concat!(" (v", env!("CARGO_PKG_VERSION"), ")"));
        }
        if let Some(UpdateResult::Available { version, .. }) = &self.update_result {
            s.push_str("   \u{00b7}   ");
            s.push_str(&format!(
                "{} v{version}",
                t(l, "Update available", "有可用更新")
            ));
        }
        s
    }
}

/// A palette command.
#[derive(Clone, Copy)]
enum Cmd {
    NewTab,
    /// Launch `integration::AGENTS[i]` in a new tab (B2.1).
    LaunchAgent(usize),
    /// Resume a reported agent session, from a picker (ADR 0042, A4).
    ResumeAgent,
    /// Reattach / end a host left running by an earlier mtty (ADR 0041).
    AttachRecovered(usize),
    EndRecovered(usize),
    /// The host library (B3.1).
    Hosts,
    /// SFTP for the active ssh tab (B3.4).
    SftpCurrent,
    /// Browse an FTP/FTPS server (B3.6).
    ConnectFtp,
    /// Encrypted sync of hosts and snippets (B4.5).
    SyncSettings,
    SyncNow,
    /// Snippets and broadcast input (B3.5).
    Snippets,
    ToggleBroadcast,
    /// Agent tasks in git worktrees (B2.4).
    NewTask,
    Tasks,
    /// The active pane's last command output (OSC 133, B2.3).
    CopyLastOutput,
    SendLastOutput,
    /// One-step context hand-off to the agent pane (ADR 0040, A3).
    SendSelectionToAgent,
    SendDiagnosticsToAgent,
    SendLastOutputToAgent,
    /// Accept / reject the active editor's pending agent edit (ADR 0040, A1).
    AcceptAgentEdit,
    RejectAgentEdit,
    /// Start an ACP agent session (ADR 0040, A2).
    AcpAgent,
    Composer,
    OpenQuickly,
    CheckUpdates,
    /// Check, download, verify and install in one go (B4.3).
    UpdateAndRelaunch,
    NewSsh,
    /// New serial, Telnet or raw TCP session (ADR 0037).
    NewTransport,
    OpenRemote,
    SaveRecipe,
    OpenRecipe,
    OpenFile,
    Save,
    Copy,
    Paste,
    SplitRight,
    SplitLeft,
    SplitDown,
    SplitUp,
    ClosePane,
    ToggleSidebar,
    ToggleDetails,
    FontUp,
    FontDown,
    FontReset,
    Palette,
    CopyAnsi,
    PasteEscaped,
    Find,
    /// Find with the replace field (editor panes).
    Replace,
    GoToLine,
    /// The outline picker for the active editor (ADR 0034, E6).
    GoToSymbol,
    /// Collapse every fold in the active editor.
    FoldAll,
    /// Expand every fold in the active editor.
    UnfoldAll,
    /// Collapse or expand the fold at the caret.
    ToggleFold,
    /// Open or close a Markdown preview beside the active editor.
    MarkdownPreview,
    /// The language server, at the active editor's caret (ADR 0034, E5).
    GoToDefinition,
    ShowHover,
    TriggerCompletion,
    NextProblem,
    PreviousProblem,
    /// Multiple cursors in the active editor (ADR 0034, E4).
    SelectAllOccurrences,
    CursorsAtLineEnds,
    FindNext,
    FindPrev,
    UseSelForFind,
    JumpToSel,
    EditLargeFile,
    FindInAllTabs,
    Fullscreen,
    ReadOnly,
    HintMode,
    Pip,
    ClearScreen,
    ClearScrollback,
    DuplicateTab,
    ReopenClosed,
    QuickTerminal,
    SelectAll,
    CopyPath,
    RevealCwd,
    OpenExternally,
    Settings,
    /// Quit and start again, keeping hosted programs running (ADR 0041).
    Relaunch,
    Quit,
}

impl State {
    fn handles(&self) -> Vec<mtty_ui::layout::Handle> {
        self.tabs
            .get(self.active_tab)
            .map(|t| t.layout.handles(self.grid_area()))
            .unwrap_or_default()
    }

    fn commands(&self) -> Vec<(Cmd, std::borrow::Cow<'static, str>)> {
        use mtty_ui::i18n::t;
        let l = self.lang;
        vec![
            (Cmd::NewTab, t(l, "New Tab", "新建标签")),
            (Cmd::Composer, "Composer"),
            (Cmd::Hosts, t(l, "Hosts…", "主机…")),
            (Cmd::Snippets, t(l, "Snippets…", "命令片段…")),
            (
                Cmd::ConnectFtp,
                t(l, "Connect over FTP/FTPS…", "连接 FTP/FTPS…"),
            ),
            (
                Cmd::SyncSettings,
                t(l, "Sync Hosts and Snippets…", "同步主机与片段…"),
            ),
            (Cmd::SyncNow, t(l, "Sync Now", "立即同步")),
            (
                Cmd::ToggleBroadcast,
                t(
                    l,
                    "Broadcast Input to All Panes in Tab",
                    "向本标签所有分屏广播输入",
                ),
            ),
            (
                Cmd::SftpCurrent,
                t(
                    l,
                    "Files over SFTP (this ssh tab)…",
                    "SFTP 文件(当前 SSH 标签)…",
                ),
            ),
            (Cmd::NewTask, t(l, "New Agent Task…", "新建 Agent 任务…")),
            (Cmd::Tasks, t(l, "Agent Tasks…", "Agent 任务…")),
            (
                Cmd::CopyLastOutput,
                t(l, "Copy Last Command Output", "复制上一条命令的输出"),
            ),
            (
                Cmd::SendLastOutput,
                t(
                    l,
                    "Send Last Command Output to Composer",
                    "把上一条命令的输出发到 Composer",
                ),
            ),
            (
                Cmd::SendSelectionToAgent,
                t(l, "Send Selection to Agent", "把选区发给 Agent"),
            ),
            (
                Cmd::SendDiagnosticsToAgent,
                t(l, "Send Diagnostics to Agent", "把诊断发给 Agent"),
            ),
            (
                Cmd::SendLastOutputToAgent,
                t(
                    l,
                    "Send Last Command Output to Agent",
                    "把上一条命令的输出发给 Agent",
                ),
            ),
            (
                Cmd::AcceptAgentEdit,
                t(l, "Accept Agent Edit", "接受 Agent 修改"),
            ),
            (
                Cmd::RejectAgentEdit,
                t(l, "Reject Agent Edit", "拒绝 Agent 修改"),
            ),
            (Cmd::AcpAgent, t(l, "ACP Agent…", "ACP Agent…")),
            (Cmd::OpenQuickly, t(l, "Open Quickly", "快速打开")),
            (Cmd::QuickTerminal, t(l, "Quick Terminal", "快速终端")),
            (Cmd::CheckUpdates, t(l, "Check for Updates", "检查更新")),
            (
                Cmd::UpdateAndRelaunch,
                t(l, "Update and Relaunch", "更新并重启"),
            ),
            (Cmd::NewSsh, t(l, "New SSH Session…", "新建 SSH 会话…")),
            (
                Cmd::NewTransport,
                t(
                    l,
                    "New Serial/Telnet/TCP Session…",
                    "新建串口/Telnet/TCP 会话…",
                ),
            ),
            (Cmd::OpenRemote, t(l, "Open Remote File…", "打开远端文件…")),
            (Cmd::SaveRecipe, t(l, "Save Recipe…", "保存配方…")),
            (Cmd::OpenRecipe, t(l, "Open Recipe…", "打开配方…")),
            (Cmd::OpenFile, t(l, "Open File…", "打开文件…")),
            (Cmd::Save, t(l, "Save", "保存")),
            (Cmd::Copy, t(l, "Copy", "复制")),
            (Cmd::Paste, t(l, "Paste", "粘贴")),
            (Cmd::SplitRight, t(l, "Split Right", "向右分屏")),
            (Cmd::SplitLeft, t(l, "Split Left", "向左分屏")),
            (Cmd::SplitDown, t(l, "Split Down", "向下分屏")),
            (Cmd::SplitUp, t(l, "Split Up", "向上分屏")),
            (Cmd::ClosePane, t(l, "Close Pane / Tab", "关闭 Pane/标签")),
            (Cmd::ToggleSidebar, t(l, "Toggle Sidebar", "开关侧栏")),
            (Cmd::ToggleDetails, t(l, "Toggle Details", "开关详情")),
            (Cmd::FontUp, t(l, "Increase Font Size", "增大字号")),
            (Cmd::FontDown, t(l, "Decrease Font Size", "减小字号")),
            (Cmd::FontReset, t(l, "Reset Font Size", "重置字号")),
            (Cmd::Palette, t(l, "Command Palette", "命令面板")),
            (
                Cmd::CopyAnsi,
                t(l, "Copy as ANSI Sequence", "复制为 ANSI 序列"),
            ),
            (
                Cmd::PasteEscaped,
                t(l, "Paste Escaping Special Characters", "转义粘贴"),
            ),
            (Cmd::Find, t(l, "Find…", "查找…")),
            (Cmd::Replace, t(l, "Replace…", "替换…")),
            (Cmd::GoToLine, t(l, "Go to Line…", "跳转到行…")),
            (
                Cmd::GoToSymbol,
                t(l, "Go to Symbol in File…", "转到文件中的符号…"),
            ),
            (Cmd::FoldAll, t(l, "Fold All", "全部折叠")),
            (Cmd::UnfoldAll, t(l, "Unfold All", "全部展开")),
            (Cmd::ToggleFold, t(l, "Toggle Fold", "切换折叠")),
            (
                Cmd::MarkdownPreview,
                t(l, "Toggle Markdown Preview", "开关 Markdown 预览"),
            ),
            (Cmd::GoToDefinition, t(l, "Go to Definition", "跳转到定义")),
            (Cmd::ShowHover, t(l, "Show Hover", "显示悬停信息")),
            (
                Cmd::TriggerCompletion,
                t(l, "Trigger Completion", "触发补全"),
            ),
            (Cmd::NextProblem, t(l, "Next Problem", "下一个问题")),
            (Cmd::PreviousProblem, t(l, "Previous Problem", "上一个问题")),
            (
                Cmd::SelectAllOccurrences,
                t(l, "Select All Occurrences", "选中所有相同项"),
            ),
            (
                Cmd::CursorsAtLineEnds,
                t(l, "Add Cursors to Line Ends", "在各行末尾添加光标"),
            ),
            (Cmd::FindNext, t(l, "Find Next", "查找下一个")),
            (Cmd::FindPrev, t(l, "Find Previous", "查找上一个")),
            (
                Cmd::UseSelForFind,
                t(l, "Use Selection for Find", "用所选内容查找"),
            ),
            (Cmd::JumpToSel, t(l, "Jump to Selection", "跳到所选")),
            (
                Cmd::EditLargeFile,
                t(l, "Switch to Editing…", "切换为可编辑…"),
            ),
            (
                Cmd::FindInAllTabs,
                t(l, "Find in All Tabs", "在所有标签中查找"),
            ),
            (Cmd::Fullscreen, t(l, "Toggle Full Screen", "全屏切换")),
            (Cmd::ReadOnly, t(l, "Read Only", "只读")),
            (
                Cmd::HintMode,
                t(l, "Open Link (Hint Mode)", "打开链接（提示模式）"),
            ),
            (Cmd::ClearScreen, t(l, "Clear Screen", "清屏")),
            (Cmd::ClearScrollback, t(l, "Clear Scrollback", "清除回滚")),
            (Cmd::DuplicateTab, t(l, "Duplicate Tab", "复制标签")),
            (
                Cmd::ReopenClosed,
                t(l, "Reopen Last Closed", "重开最近关闭"),
            ),
            (Cmd::SelectAll, t(l, "Select All", "全选")),
            (Cmd::CopyPath, t(l, "Copy Path", "复制路径")),
            (
                Cmd::RevealCwd,
                t(l, "Reveal in File Manager", "在文件管理器中显示"),
            ),
            (
                Cmd::OpenExternally,
                t(l, "Open Externally", "用系统默认程序打开"),
            ),
            (
                Cmd::ResumeAgent,
                t(l, "Resume Agent Session…", "恢复 Agent 会话…"),
            ),
            (Cmd::Settings, t(l, "Settings", "设置")),
            (Cmd::Quit, t(l, "Quit", "退出")),
        ]
        .into_iter()
        .chain(self.pty_host.then(|| {
            (
                Cmd::Relaunch,
                t(
                    l,
                    "Relaunch, Keeping Programs Running",
                    "重启 mtty(保留运行中的程序)",
                ),
            )
        }))
        .chain(
            // Agents found on PATH (the Settings check, refreshed every 5 s).
            mtty_ui::integration::AGENTS
                .iter()
                .enumerate()
                .filter(|(i, _)| {
                    self.agents_detected
                        .as_ref()
                        .is_some_and(|(_, found)| found.get(*i).copied().unwrap_or(false))
                })
                .map(|(i, a)| (Cmd::LaunchAgent(i), launch_label(l, a.name))),
        )
        .map(|(cmd, label)| (cmd, std::borrow::Cow::Borrowed(label)))
        .chain(self.recovered_commands())
        .collect()
    }

    /// Palette entries for hosts left running by an earlier mtty.
    fn recovered_commands(&self) -> Vec<(Cmd, std::borrow::Cow<'static, str>)> {
        {
            use mtty_ui::i18n::t;
            let l = self.lang;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let mut out = Vec::new();
            for (i, host) in self.recovered.iter().enumerate() {
                let program = std::path::Path::new(&host.program).file_name().map_or_else(
                    || host.program.clone(),
                    |n| n.to_string_lossy().into_owned(),
                );
                let minutes = now.saturating_sub(host.started_at) / 60;
                let what = match l {
                    mtty_ui::i18n::Lang::Zh => format!("{program}(已运行 {minutes} 分钟)"),
                    _ => format!("{program} (running {minutes} min)"),
                };
                out.push((
                    Cmd::AttachRecovered(i),
                    format!(
                        "{} {what}",
                        t(l, "Reattach Running Program:", "接回运行中的程序:")
                    )
                    .into(),
                ));
                out.push((
                    Cmd::EndRecovered(i),
                    format!(
                        "{} {what}",
                        t(l, "End Running Program:", "结束运行中的程序:")
                    )
                    .into(),
                ));
            }
            out
        }
    }

    /// Hosts that kept running but that no restored pane took back: a crash
    /// before the session file was written. Offer them rather than adopt them
    /// unasked; also drop host binaries nothing uses any more.
    fn find_recovered(&mut self) {
        use mtty_ptyhost::launch;
        let Ok(dir) = launch::hosts_dir() else {
            return;
        };
        if let Some(data) = mtty_config::data_dir() {
            launch::remove_unused_versions(&data, &dir);
        }
        let attached: Vec<String> = self
            .tabs
            .iter()
            .flat_map(|t| t.panes.iter())
            .filter_map(|p| p.term.host_id().map(|(id, _)| id.to_string()))
            .collect();
        self.recovered = launch::running_hosts(&dir)
            .into_iter()
            .filter(|h| !attached.contains(&h.id))
            .collect();
        if !self.recovered.is_empty() {
            let n = self.recovered.len();
            let msg = match self.lang {
                mtty_ui::i18n::Lang::Zh => {
                    format!("还有 {n} 个程序在上次的 mtty 中运行:在命令面板中接回或结束它们。")
                }
                _ => format!(
                    "{n} program(s) from an earlier mtty are still running: reattach or end them from the command palette."
                ),
            };
            self.show_notice(msg);
        }
    }

    /// Reattach a recovered host in a new tab, or end it.
    fn take_recovered(&mut self, i: usize, attach: bool) {
        if i >= self.recovered.len() {
            return;
        }
        let host = self.recovered.remove(i);
        if !attach {
            if let Ok((mut stream, _)) =
                mtty_ptyhost::client::connect(&host.socket, Duration::from_millis(500))
            {
                let _ = mtty_ptyhost::proto::ToHost::Kill.write(&mut stream);
            }
            return;
        }
        let (cols, rows) = self.new_pane_size();
        match Terminal::reattach(
            &host.id,
            &host.socket,
            None,
            cols,
            rows,
            10_000,
            self.pane_waker(),
        ) {
            Ok(term) => {
                let pane = self.make_pane(gen_id(), term);
                self.push_tab(pane);
            }
            Err(e) => self.show_notice(e.to_string()),
        }
    }

    /// Scanning PATH is file-system work: refresh it at most every 5 s, not on
    /// every frame Settings or the palette is open.
    fn refresh_agents_detected(&mut self) {
        if self
            .agents_detected
            .as_ref()
            .map_or(true, |(at, _)| at.elapsed() > Duration::from_secs(5))
        {
            let found = mtty_ui::integration::AGENTS
                .iter()
                .map(|a| mtty_ui::integration::detected(a.bin))
                .collect();
            self.agents_detected = Some((Instant::now(), found));
        }
    }

    /// Start an agent CLI in a new tab in the active pane's directory. The
    /// pane carries MTTY_PANE_ID, so the agent's hook reports to that tab.
    fn launch_agent(&mut self, index: usize) {
        let Some(agent) = mtty_ui::integration::AGENTS.get(index) else {
            return;
        };
        let cmd = mtty_ui::integration::launch_command(agent);
        self.new_tab_in(self.active_cwd_for_new());
        if let Some(tab) = self.tabs.last_mut() {
            let active = tab.active.clone();
            if let Some(pane) = tab.panes.iter_mut().find(|p| p.id == active) {
                pane.term.write(format!("{cmd}\r").as_bytes());
            }
        }
        self.publish_panes();
    }

    /// Open a tab in the session's directory and type the agent's resume
    /// command (ADR 0042, A4).
    fn resume_agent(&mut self, agent_name: &str, session: &str, cwd: Option<&str>) {
        let Some(agent) = mtty_ui::integration::AGENTS
            .iter()
            .find(|a| a.name == agent_name)
        else {
            return;
        };
        let Some(cmd) = mtty_ui::integration::resume_command(agent, session) else {
            return;
        };
        let dir = cwd
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from)
            .filter(|d| d.is_dir());
        self.new_tab_in(dir.or_else(|| self.active_cwd_for_new()));
        if let Some(tab) = self.tabs.last_mut() {
            let active = tab.active.clone();
            if let Some(pane) = tab.panes.iter_mut().find(|p| p.id == active) {
                pane.term.write(format!("{cmd}\r").as_bytes());
            }
        }
        self.publish_panes();
    }

    fn run_command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::LaunchAgent(i) => self.launch_agent(i),
            Cmd::ResumeAgent => self.resume_picker = Some((String::new(), 0)),
            Cmd::AttachRecovered(i) => {
                self.take_recovered(i, true);
            }
            Cmd::EndRecovered(i) => {
                self.take_recovered(i, false);
            }
            Cmd::SftpCurrent => {
                let tab = self.tabs.get(self.active_tab);
                let saved = tab.filter(|t| t.ssh).and_then(|t| {
                    self.host_book
                        .hosts
                        .iter()
                        .find(|h| h.name == t.title)
                        .cloned()
                });
                let typed = tab.filter(|t| t.ssh).and_then(|t| t.ssh_target.clone());
                match (saved, typed) {
                    (Some(host), _) => self.open_sftp(
                        host.name.clone(),
                        mtty_ui::sftp::Remote {
                            destination: host.destination(),
                            options: host.ssh_options(),
                        },
                    ),
                    (None, Some(target)) => {
                        let parsed = mtty_ui::ssh::Target::parse(&target);
                        let options = parsed
                            .as_ref()
                            .and_then(|t| t.port)
                            .map(|p| vec!["-p".to_string(), p.to_string()])
                            .unwrap_or_default();
                        let destination = parsed.map(|t| t.destination()).unwrap_or(target.clone());
                        self.open_sftp(
                            target,
                            mtty_ui::sftp::Remote {
                                destination,
                                options,
                            },
                        );
                    }
                    _ => self.show_notice(
                        mtty_ui::i18n::t(
                            self.lang,
                            "The active tab is not an ssh session; open Files from Hosts…",
                            "当前标签不是 SSH 会话;请从“主机…”中打开文件",
                        )
                        .into(),
                    ),
                }
            }
            Cmd::SyncSettings => {
                self.sync_view = Some(SyncView {
                    dir: self
                        .sync
                        .dir
                        .as_ref()
                        .map(|d| d.display().to_string())
                        .unwrap_or_default(),
                    ..Default::default()
                });
            }
            Cmd::SyncNow => {
                if self.sync.dir.is_some() && self.sync.key.is_some() {
                    self.sync.due = Some(Instant::now());
                    self.sync_tick();
                } else {
                    self.sync_view = Some(SyncView::default());
                }
            }
            Cmd::ConnectFtp => {
                self.ftp_dialog = Some(FtpDialog {
                    security: 1,
                    ..Default::default()
                })
            }
            Cmd::Snippets => {
                self.reload_snippets();
                self.reload_hosts();
                self.snippets_view = Some(SnippetsView::default());
            }
            Cmd::ToggleBroadcast => {
                self.broadcast = !self.broadcast;
                let msg = if self.broadcast {
                    t_lang(
                        self.lang,
                        "Broadcast on: typing goes to every pane of this tab",
                        "广播已开启:输入会发送到本标签的所有分屏",
                    )
                } else {
                    t_lang(self.lang, "Broadcast off", "广播已关闭")
                };
                self.show_notice(msg.to_string());
            }
            Cmd::Hosts => {
                self.reload_hosts();
                self.hosts_view = Some(HostsView::default());
                self.spawn_job(|| JobDone::AgentStatus(mtty_ui::hostkeys::agent_status()));
            }
            Cmd::NewTask => {
                self.refresh_agents_detected();
                let first = self
                    .agents_detected
                    .as_ref()
                    .and_then(|(_, found)| found.iter().position(|f| *f));
                self.task_dialog = Some((String::new(), first));
            }
            Cmd::Tasks => match self.cwd() {
                Some(repo) => {
                    self.tasks_view = Some(TasksView {
                        repo,
                        tasks: Vec::new(),
                        loading: true,
                        confirm: None,
                    });
                    self.reload_tasks();
                }
                None => self.show_notice(
                    mtty_ui::i18n::t(self.lang, "No current directory", "没有当前目录").into(),
                ),
            },
            Cmd::CopyLastOutput | Cmd::SendLastOutput => {
                match self
                    .active_pane()
                    .and_then(|p| p.term.last_command_output())
                    .cloned()
                {
                    Some(out) => {
                        if matches!(cmd, Cmd::CopyLastOutput) {
                            self.egui_ctx.copy_text(out.text);
                            let msg = mtty_ui::i18n::t(self.lang, "Copied", "已复制");
                            self.show_notice(msg.to_string());
                        } else {
                            // Fenced, so an agent sees where the output starts and ends.
                            let draft = self.composer.take().unwrap_or_default();
                            let sep = if draft.is_empty() { "" } else { "\n\n" };
                            self.composer = Some(format!("{draft}{sep}```\n{}\n```\n", out.text));
                        }
                    }
                    None => {
                        let msg = mtty_ui::i18n::t(
                            self.lang,
                            "No finished command here yet (needs the zsh integration).",
                            "这里还没有已结束的命令(需要 zsh 集成)。",
                        );
                        self.show_notice(msg.to_string());
                    }
                }
            }
            Cmd::SendSelectionToAgent => {
                let text = self.active_editor().map(|e| e.copy()).unwrap_or_default();
                if text.trim().is_empty() {
                    let msg = mtty_ui::i18n::t(
                        self.lang,
                        "Select some text in an editor pane first.",
                        "请先在编辑器 pane 中选中文字。",
                    )
                    .to_string();
                    self.show_notice(msg);
                } else {
                    let prompt = format!(
                        "{}\n```\n{text}\n```",
                        mtty_ui::i18n::t(
                            self.lang,
                            "Here is the selected code:",
                            "以下是选中的代码:"
                        )
                    );
                    self.send_to_agent(&prompt);
                }
            }
            Cmd::SendDiagnosticsToAgent => {
                let text = self
                    .active_editor()
                    .map(|ed| {
                        let rope = ed.doc.rope();
                        ed.diagnostics
                            .iter()
                            .map(|d| {
                                let line = rope.char_to_line(d.from.min(rope.len_chars())) + 1;
                                format!("{line}: {}", d.message)
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if text.is_empty() {
                    let msg = mtty_ui::i18n::t(
                        self.lang,
                        "No diagnostics in the active editor.",
                        "当前编辑器没有诊断。",
                    )
                    .to_string();
                    self.show_notice(msg);
                } else {
                    let prompt = format!(
                        "{}\n```\n{text}\n```",
                        mtty_ui::i18n::t(self.lang, "Here are the diagnostics:", "以下是诊断:")
                    );
                    self.send_to_agent(&prompt);
                }
            }
            Cmd::SendLastOutputToAgent => {
                match self
                    .tabs
                    .get(self.active_tab)
                    .and_then(|t| {
                        t.panes
                            .iter()
                            .find(|p| p.term.last_command_output().is_some())
                    })
                    .and_then(|p| p.term.last_command_output())
                    .cloned()
                {
                    Some(out) => {
                        let prompt = format!(
                            "{}\n```\n{}\n```",
                            mtty_ui::i18n::t(
                                self.lang,
                                "Here is the last command's output:",
                                "以下是上一条命令的输出:"
                            ),
                            out.text
                        );
                        self.send_to_agent(&prompt);
                    }
                    None => {
                        let msg = mtty_ui::i18n::t(
                            self.lang,
                            "No finished command in this tab (needs the shell integration).",
                            "本标签中没有已结束的命令(需要 shell 集成)。",
                        )
                        .to_string();
                        self.show_notice(msg);
                    }
                }
            }
            Cmd::AcpAgent => {
                self.acp_start = Some(AcpStart::default());
            }
            Cmd::AcceptAgentEdit => {
                if self
                    .active_editor()
                    .is_some_and(|ed| self.acp_writes.contains_key(&ed.id))
                {
                    self.save_active_editor();
                    self.poll_acp_writes();
                    self.window.request_redraw();
                    return;
                }
                if let Some(ed) = self.active_editor_mut() {
                    ed.accept_proposal();
                }
                self.window.request_redraw();
            }
            Cmd::RejectAgentEdit => {
                let rejected = self
                    .active_editor_mut()
                    .is_some_and(|ed| ed.reject_proposal());
                if rejected {
                    let msg = mtty_ui::i18n::t(
                        self.lang,
                        "Agent edit rejected.",
                        "已拒绝 agent 的修改。",
                    )
                    .to_string();
                    self.show_notice(msg);
                }
                self.window.request_redraw();
            }
            Cmd::NewTab => self.new_tab_in(self.active_cwd_for_new()),
            Cmd::QuickTerminal => self.toggle_quick_terminal(),
            Cmd::SplitRight => self.split(SplitDir::Right),
            Cmd::SplitLeft => self.split(SplitDir::Left),
            Cmd::SplitDown => self.split(SplitDir::Down),
            Cmd::SplitUp => self.split(SplitDir::Up),
            Cmd::ClosePane => self.close_pane(),
            Cmd::ToggleSidebar => self.toggle_sidebar(),
            Cmd::ToggleDetails => self.toggle_details(),
            Cmd::FontUp | Cmd::FontDown => {
                let d = if matches!(cmd, Cmd::FontUp) {
                    1.0
                } else {
                    -1.0
                };
                self.font_size = (self.font_size + d).clamp(6.0, 40.0);
                let (cw, ch) =
                    State::cell_size(self.font_size, self.line_ratio, self.font_family.as_deref());
                self.cw = cw;
                self.ch = ch;
                self.resize();
            }
            Cmd::FontReset => {
                self.font_size = self.default_font_size;
                let (cw, ch) =
                    State::cell_size(self.font_size, self.line_ratio, self.font_family.as_deref());
                self.cw = cw;
                self.ch = ch;
                self.resize();
            }
            Cmd::Palette => {
                self.show_palette = true;
                self.palette_query.clear();
                self.palette_idx = 0;
            }
            Cmd::Find => {
                self.search = Some(String::new());
                self.search_idx = 0;
                self.search_key.clear();
                self.find_opts.replace = None;
            }
            Cmd::Replace => {
                if !self.require_editor() {
                    return;
                }
                // Keep a query already typed; start from the selection
                // otherwise, as Use Selection for Find does.
                if self.search.as_deref().map_or(true, str::is_empty) {
                    let selected = self.active_editor().map(|e| e.copy()).unwrap_or_default();
                    self.search = Some(if selected.contains('\n') {
                        String::new()
                    } else {
                        selected
                    });
                    self.search_idx = 0;
                    self.search_key.clear();
                }
                if self.find_opts.replace.is_none() {
                    self.find_opts.replace = Some(String::new());
                }
            }
            Cmd::MarkdownPreview => self.toggle_markdown_preview(),
            Cmd::GoToDefinition => {
                if self.require_editor() {
                    self.request_definition();
                }
            }
            Cmd::TriggerCompletion => {
                if self.require_editor() {
                    self.request_completion(None, true);
                }
            }
            Cmd::ShowHover | Cmd::NextProblem | Cmd::PreviousProblem => {
                if !self.require_editor() {
                    return;
                }
                if !matches!(cmd, Cmd::ShowHover) {
                    let forward = matches!(cmd, Cmd::NextProblem);
                    self.run_editor_command(editor_pane::Command::NextProblem(forward));
                }
                let at = self
                    .active_editor()
                    .map(|e| e.doc.selection().primary().from());
                let pane = self.active_pane_id();
                if let (Some(at), Some(pane), Some(pos)) = (at, pane, self.caret_point()) {
                    self.show_hover(&pane, at, pos);
                }
            }
            Cmd::GoToLine => {
                if self.require_editor() {
                    self.goto_line = Some(String::new());
                }
            }
            Cmd::GoToSymbol => {
                if self.require_editor() {
                    self.goto_symbol = Some((String::new(), 0));
                }
            }
            Cmd::FoldAll => {
                if self.require_editor() {
                    self.run_editor_command(editor_pane::Command::FoldAll);
                }
            }
            Cmd::UnfoldAll => {
                if self.require_editor() {
                    self.run_editor_command(editor_pane::Command::UnfoldAll);
                }
            }
            Cmd::ToggleFold => {
                if self.require_editor() {
                    self.run_editor_command(editor_pane::Command::ToggleFold);
                }
            }
            Cmd::SelectAllOccurrences | Cmd::CursorsAtLineEnds => {
                if !self.require_editor() {
                    return;
                }
                let command = if matches!(cmd, Cmd::SelectAllOccurrences) {
                    editor_pane::Command::SelectAllOccurrences
                } else {
                    editor_pane::Command::CursorsAtLineEnds
                };
                if let Some(ed) = self.active_editor_mut() {
                    ed.run(command);
                }
            }
            Cmd::CopyAnsi => {
                // An editor's text has no terminal colours: copy it as is.
                if let Some(ed) = self.active_editor() {
                    let text = ed.copy();
                    if !text.is_empty() {
                        self.egui_ctx.copy_text(text);
                    }
                    return;
                }
                if let Some(t) = self.selection_ansi() {
                    if !t.is_empty() {
                        self.egui_ctx.copy_text(t);
                    }
                }
            }
            Cmd::PasteEscaped => {
                let text = self.clipboard_text().unwrap_or_default();
                if !text.is_empty() {
                    let escaped = shell_escape_text(&text);
                    self.paste(&escaped);
                }
            }
            Cmd::FindNext | Cmd::FindPrev => {
                let step = if matches!(cmd, Cmd::FindNext) { 1 } else { -1 };
                let n = self.search_count();
                if self.search.is_some() && n > 0 {
                    self.search_idx =
                        ((self.search_idx as i32 + step).rem_euclid(n as i32)) as usize;
                    self.scroll_to_search_hit();
                }
            }
            Cmd::UseSelForFind => {
                let selected = match self.active_editor() {
                    Some(ed) => Some(ed.copy()),
                    None => self.selection_text(),
                };
                if let Some(s) = selected {
                    let s = s.trim().to_string();
                    if !s.is_empty() {
                        self.search = Some(s);
                        self.search_idx = 0;
                        self.search_key.clear();
                    }
                }
            }
            Cmd::EditLargeFile => match self.active_editor().filter(|e| e.is_view_only()) {
                Some(ed) => self.large_edit_offer = Some(ed.id.clone()),
                None => self.show_notice(
                    mtty_ui::i18n::t(
                        self.lang,
                        "The active pane is not a file in view mode.",
                        "当前 pane 不是只读查看中的文件。",
                    )
                    .to_string(),
                ),
            },
            Cmd::JumpToSel => {
                if self.search.is_some() {
                    self.scroll_to_search_hit();
                }
            }
            Cmd::FindInAllTabs => self.find_in_all_tabs(),
            Cmd::ReadOnly => {
                self.read_only = !self.read_only;
                self.mtp.set_read_only(self.read_only);
            }
            Cmd::HintMode => self.build_hints(),
            Cmd::Pip => {
                if self.pip.is_some() {
                    self.pip = None;
                } else {
                    self.pip_request = true;
                }
            }
            Cmd::Fullscreen => {
                let full = self.window.fullscreen().is_some();
                self.window.set_fullscreen(if full {
                    None
                } else {
                    Some(winit::window::Fullscreen::Borderless(None))
                });
            }
            Cmd::ClearScrollback => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    let active = tab.active.clone();
                    if let Some(p) = tab.panes.iter_mut().find(|p| p.id == active) {
                        p.term.screen_mut().process(b"\x1b[3J");
                    }
                }
            }
            Cmd::DuplicateTab => self.duplicate_tab(),
            Cmd::ReopenClosed => self.reopen_tab(),
            Cmd::SelectAll => {
                let select_all = egui::Event::Key {
                    key: egui::Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::COMMAND,
                };
                if self.edit_in_text_field(select_all) {
                    return;
                }
                if let Some(ed) = self.active_editor_mut() {
                    ed.run(editor_pane::Command::SelectAll);
                    return;
                }
                if let Some(id) = self.active_pane_id() {
                    if let Some(tab) = self.tabs.get(self.active_tab) {
                        if let Some(p) = tab.panes.iter().find(|p| p.id == id) {
                            let (r, c) = p.term.size();
                            self.selection = Some((
                                id,
                                Selection {
                                    start: (0, 0),
                                    end: (r.saturating_sub(1), c.saturating_sub(1)),
                                },
                            ));
                        }
                    }
                }
            }
            Cmd::ClearScreen => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    let active = tab.active.clone();
                    if let Some(p) = tab.panes.iter_mut().find(|p| p.id == active) {
                        p.term.screen_mut().process(b"\x1b[2J\x1b[H");
                        p.scroll = 0;
                    }
                }
            }
            Cmd::CopyPath => {
                // An editor's path is its file's, not the folder's.
                if let Some(ed) = self.active_editor() {
                    self.egui_ctx.copy_text(ed.path.display().to_string());
                    return;
                }
                if let Some(cwd) = self.cwd() {
                    self.egui_ctx.copy_text(cwd.display().to_string());
                }
            }
            Cmd::RevealCwd => {
                if let Some(cwd) = self.cwd() {
                    open_external(&cwd.display().to_string());
                }
            }
            Cmd::OpenExternally => {
                // An editor's file, opened by the OS default application; a
                // terminal pane falls back to its directory.
                let path = self
                    .active_editor()
                    .filter(|ed| ed.remote.is_none())
                    .map(|ed| ed.path.clone());
                match path {
                    Some(path) => open_external(&path.display().to_string()),
                    None => {
                        if let Some(cwd) = self.cwd() {
                            open_external(&cwd.display().to_string());
                        }
                    }
                }
            }
            Cmd::Composer => self.composer = Some(String::new()),
            Cmd::OpenQuickly => self.quick = Some(String::new()),
            Cmd::CheckUpdates => self.check_updates(),
            Cmd::UpdateAndRelaunch => {
                self.update_auto = true;
                self.update_dialog = true;
                if matches!(self.update_result, Some(UpdateResult::Available { .. })) {
                    self.continue_auto_update();
                } else {
                    self.check_updates();
                }
            }
            Cmd::NewSsh => self.ssh_dialog = Some(SshForm::default()),
            Cmd::NewTransport => self.transport_dialog = Some(TransportDialog::default()),
            Cmd::OpenRemote => {
                // From an ssh tab the host is already known.
                let dest = self
                    .tabs
                    .get(self.active_tab)
                    .filter(|t| t.ssh)
                    .and_then(|t| t.ssh_target.clone())
                    .unwrap_or_default();
                self.remote_dialog = Some((dest, String::new()));
            }
            Cmd::SaveRecipe => {
                self.recipe_name.clear();
                self.recipe_dialog = Some(true);
            }
            Cmd::OpenRecipe => {
                self.recipe_list = list_recipes();
                self.recipe_dialog = Some(false);
            }
            Cmd::OpenFile => self.show_open = true,
            Cmd::Save => {
                if self.active_editor().is_some() {
                    self.save_active_editor();
                } else {
                    let _ = self.save_editor();
                }
            }
            Cmd::Copy => {
                if !self.edit_in_text_field(egui::Event::Copy) {
                    let ctx = self.egui_ctx.clone();
                    self.copy_selection(&ctx);
                }
            }
            Cmd::Paste => {
                let text = self.clipboard_text().unwrap_or_default();
                if !self.edit_in_text_field(egui::Event::Paste(text)) {
                    self.paste_clipboard();
                }
            }
            Cmd::Settings => self.show_settings = true,
            Cmd::Relaunch => match mtty_ui::install::relaunch() {
                Ok(()) => self.quit_keeping_sessions(),
                Err(e) => self.show_notice(e),
            },
            Cmd::Quit => {
                let all: Vec<usize> = (0..self.tabs.len()).collect();
                if !self.confirm_close_tabs(&all) {
                    return;
                }
                self.save_window_state();
                // process::exit skips destructors: persist the session and
                // release the sleep inhibitor first, as a window close does.
                if self.show_settings {
                    self.persist_settings();
                }
                self.save_session_on_exit();
                self.leave_hosts();
                self.sleep.set_awake(false);
                std::process::exit(0);
            }
        }
        self.window.request_redraw();
    }

    fn palette_window(&mut self, ctx: &egui::Context) {
        if !self.show_palette {
            return;
        }
        self.refresh_agents_detected();
        let cmds = self.commands();
        let mut query = std::mem::take(&mut self.palette_query);
        let mut chosen: Option<Cmd> = None;
        let mut open = true;
        let mut close = false;
        let ch = self.theme.chrome();
        egui::Window::new(mtty_ui::i18n::t(self.lang, "Command Palette", "命令面板"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 120.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.visuals_mut().selection.bg_fill = mtty_ui::chrome::bg_color(ch.active);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut query)
                        .hint_text(mtty_ui::i18n::t(self.lang, "Type a command…", "输入命令…"))
                        .desired_width(420.0),
                );
                // Enter makes the field give up focus; check it before re-taking focus.
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if !enter {
                    resp.request_focus();
                }
                let q = query.to_lowercase();
                let mut rows: Vec<(usize, Cmd, &str)> = cmds
                    .iter()
                    .filter_map(|(c, l)| {
                        mtty_ui::palette::score(l, "command", &q).map(|s| (s, *c, l.as_ref()))
                    })
                    .collect();
                rows.sort_by_key(|(s, _, _)| *s);
                let down =
                    ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown));
                let up = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp));
                let esc = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
                if down {
                    self.palette_idx = (self.palette_idx + 1).min(rows.len().saturating_sub(1));
                }
                if up {
                    self.palette_idx = self.palette_idx.saturating_sub(1);
                }
                if rows.is_empty() {
                    self.palette_idx = 0;
                } else {
                    self.palette_idx = self.palette_idx.min(rows.len() - 1);
                }
                for (i, (_, _, label)) in rows.iter().enumerate() {
                    if ui.selectable_label(i == self.palette_idx, *label).clicked() {
                        chosen = Some(rows[i].1);
                    }
                }
                if enter {
                    chosen = rows.get(self.palette_idx).map(|(_, c, _)| *c);
                }
                if esc {
                    close = true;
                }
            });
        self.palette_query = query;
        if let Some(cmd) = chosen {
            self.run_command(cmd);
            self.show_palette = false;
            self.palette_query.clear();
        }
        if !open || close {
            self.show_palette = false;
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = true;
        let mut font = self.font_size;
        let mut family = self.font_family.clone().unwrap_or_default();
        let mut line_ratio = self.line_ratio;
        let mut opacity = self.opacity;
        let mut cursor = self.theme.cursor;
        let mut graphics = self.graphics_enabled;
        let mut notifications = self.notifications;
        let mut prevent_sleep = self.prevent_sleep;
        let mut restore_scrollback = self.restore_scrollback;
        let mut install_agent: Option<&'static str> = None;
        let mut launch: Option<usize> = None;
        self.refresh_agents_detected();
        let detected = self
            .agents_detected
            .as_ref()
            .map(|(_, d)| d.clone())
            .unwrap_or_default();
        let current_theme = self.theme_name.clone();
        let mut chosen_theme: Option<&'static str> = None;
        app_window(mtty_ui::i18n::t(self.lang, "Settings", "设置"), ctx)
            .collapsible(false)
            .default_size([460.0, 560.0])
            .open(&mut open)
            .show(ctx, |ui| {
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.label(mtty_ui::i18n::t(self.lang, "Font size", "字号"));
                        ui.add(egui::Slider::new(&mut font, 6.0..=40.0));
                        ui.horizontal(|ui| {
                            ui.label(mtty_ui::i18n::t(self.lang, "Font family", "字体"));
                            ui.add(
                                egui::TextEdit::singleline(&mut family)
                                    .hint_text(mtty_ui::i18n::t(
                                        self.lang,
                                        "system default",
                                        "系统默认",
                                    ))
                                    .desired_width(170.0),
                            );
                        });
                        ui.label(mtty_ui::i18n::t(self.lang, "Line height", "行高"));
                        ui.add(egui::Slider::new(&mut line_ratio, 1.0..=2.0));
                        ui.label(mtty_ui::i18n::t(self.lang, "Opacity", "不透明度"));
                        ui.add(egui::Slider::new(&mut opacity, 0.1..=1.0));
                        ui.separator();
                        ui.label(mtty_ui::i18n::t(self.lang, "Cursor", "光标"));
                        ui.horizontal(|ui| {
                            for (s, n) in [
                                (
                                    mtty_ui::CursorStyle::Block,
                                    mtty_ui::i18n::t(self.lang, "Block", "方块"),
                                ),
                                (
                                    mtty_ui::CursorStyle::Bar,
                                    mtty_ui::i18n::t(self.lang, "Bar", "竖线"),
                                ),
                                (
                                    mtty_ui::CursorStyle::Underline,
                                    mtty_ui::i18n::t(self.lang, "Underline", "下划线"),
                                ),
                            ] {
                                if ui.radio(cursor == s, n).clicked() {
                                    cursor = s;
                                }
                            }
                        });
                        ui.separator();
                        ui.checkbox(
                            &mut graphics,
                            mtty_ui::i18n::t(self.lang, "Inline graphics", "终端内联图片"),
                        );
                        ui.checkbox(
                            &mut notifications,
                            mtty_ui::i18n::t(self.lang, "Notifications", "通知"),
                        );
                        ui.checkbox(
                            &mut prevent_sleep,
                            mtty_ui::i18n::t(self.lang, "Prevent sleep", "防休眠"),
                        );
                        ui.checkbox(
                            &mut restore_scrollback,
                            mtty_ui::i18n::t(
                                self.lang,
                                "Restore terminal contents on relaunch",
                                "重新打开时恢复终端内容",
                            ),
                        );
                        ui.separator();
                        ui.label(mtty_ui::i18n::t(
                            self.lang,
                            "Agent integrations",
                            "Agent 集成",
                        ));
                        for (i, (a, found)) in mtty_ui::integration::AGENTS
                            .iter()
                            .zip(&detected)
                            .enumerate()
                        {
                            ui.horizontal(|ui| {
                                ui.label(if *found { "\u{25cf}" } else { "\u{25cb}" });
                                ui.label(a.name);
                                if ui
                                    .add_enabled(
                                        *found,
                                        egui::Button::new(mtty_ui::i18n::t(
                                            self.lang, "Launch", "启动",
                                        )),
                                    )
                                    .clicked()
                                {
                                    launch = Some(i);
                                }
                                if ui
                                    .button(mtty_ui::i18n::t(self.lang, "Install hook", "安装钩子"))
                                    .clicked()
                                {
                                    install_agent = Some(a.name);
                                }
                            });
                        }
                        if let Some(msg) = self.integration_msg.as_mut() {
                            if ui
                                .button(mtty_ui::i18n::t(self.lang, "Copy", "复制"))
                                .clicked()
                            {
                                ui.ctx().copy_text(msg.clone());
                            }
                            // Selectable, monospace: the snippet is meant to be pasted.
                            ui.add(
                                egui::TextEdit::multiline(msg)
                                    .font(egui::TextStyle::Monospace)
                                    .desired_rows(6)
                                    .desired_width(ui.available_width()),
                            );
                        }
                        ui.separator();
                        ui.label(mtty_ui::i18n::t(self.lang, "Theme", "主题"));
                        for name in Theme::NAMES {
                            if ui.selectable_label(current_theme == name, name).clicked() {
                                chosen_theme = Some(name);
                            }
                        }
                    });
            });
        let family_opt = if family.trim().is_empty() {
            None
        } else {
            Some(family.trim().to_string())
        };
        if (font - self.font_size).abs() > 0.01
            || (line_ratio - self.line_ratio).abs() > 0.001
            || family_opt != self.font_family
        {
            self.font_size = font;
            self.line_ratio = line_ratio;
            self.font_family = family_opt;
            let (cw, ch) =
                State::cell_size(self.font_size, self.line_ratio, self.font_family.as_deref());
            self.cw = cw;
            self.ch = ch;
            self.resize();
        }
        self.opacity = opacity;
        self.theme.cursor = cursor;
        if let Some(i) = launch {
            self.launch_agent(i);
        }
        if let Some(name) = install_agent {
            let msg = match mtty_ui::integration::install(name) {
                Ok(path) => mtty_ui::integration::AGENTS
                    .iter()
                    .find(|a| a.name == name)
                    .map(|a| mtty_ui::integration::snippet(a, &path))
                    .unwrap_or_default(),
                Err(e) => format!(
                    "{}: {e}",
                    mtty_ui::i18n::t(self.lang, "Install failed", "安装失败")
                ),
            };
            self.integration_msg = Some(msg);
        }
        self.notifications = notifications;
        self.restore_scrollback = restore_scrollback;
        if self.prevent_sleep != prevent_sleep {
            self.prevent_sleep = prevent_sleep;
            if !prevent_sleep {
                self.sleep.set_awake(false);
            }
        }
        if graphics != self.graphics_enabled {
            self.graphics_enabled = graphics;
            for tab in &mut self.tabs {
                for pane in &mut tab.panes {
                    pane.term.set_graphics_enabled(graphics);
                }
            }
            self.window.request_redraw();
        }
        if let Some(n) = chosen_theme {
            if let Some(mut t) = Theme::named(n) {
                t.cursor = self.theme.cursor;
                self.theme = t;
                self.theme_name = n.to_string();
                configure_egui(ctx, &self.theme.chrome());
                let Rgb(r, g, b) = self.theme.fg;
                let Rgb(br, bg, bb) = self.theme.bg;
                for tab in &mut self.tabs {
                    for pane in &mut tab.panes {
                        pane.term.set_default_colors([r, g, b], [br, bg, bb]);
                    }
                }
                self.window.request_redraw();
            }
        }
        if !open {
            self.show_settings = false;
            self.persist_settings();
        }
    }

    /// The settings the window edits, as config.toml literals.
    fn settings_values(&self) -> Vec<(&'static str, String)> {
        use mtty_config::toml_string;
        let cursor = match self.theme.cursor {
            mtty_ui::CursorStyle::Block => "block",
            mtty_ui::CursorStyle::Bar => "bar",
            mtty_ui::CursorStyle::Underline => "underline",
        };
        let mut v = vec![
            ("font-size", format!("{:.1}", self.font_size)),
            (
                "font-family",
                toml_string(self.font_family.as_deref().unwrap_or("")),
            ),
            ("line-height", format!("{:.2}", self.line_ratio)),
            ("background-opacity", format!("{:.2}", self.opacity)),
            ("cursor-style", toml_string(cursor)),
            ("graphics", self.graphics_enabled.to_string()),
            ("notifications", self.notifications.to_string()),
            ("prevent-sleep", self.prevent_sleep.to_string()),
            ("restore-scrollback", self.restore_scrollback.to_string()),
        ];
        if !self.theme_name.is_empty() {
            v.push(("theme", toml_string(&self.theme_name.to_ascii_lowercase())));
        }
        v
    }

    /// Write changed settings to config.toml; failures stay visible.
    fn persist_settings(&mut self) {
        use mtty_ui::i18n::t;
        let current = self.settings_values();
        let changed: Vec<(&str, String)> = current
            .iter()
            .filter(|kv| !self.saved_settings.contains(kv))
            .cloned()
            .collect();
        if changed.is_empty() {
            return;
        }
        let msg = match mtty_config::Config::save_settings(&changed) {
            Ok(path) => {
                self.saved_settings = current;
                let mut msg = format!(
                    "{} {}",
                    t(self.lang, "Settings saved to", "设置已保存到"),
                    path.display()
                );
                if let Some(source) = self.config_imported_from.take() {
                    msg.push_str(&format!(
                        " ({source} {})",
                        t(
                            self.lang,
                            "settings are no longer imported",
                            "配置将不再导入"
                        )
                    ));
                }
                msg
            }
            Err(e) => format!("{}: {e}", t(self.lang, "Settings not saved", "设置未保存")),
        };
        self.show_notice(msg);
    }

    /// Debug: render the terminal grid offscreen and dump a PPM, then exit.
    fn capture(
        &self,
        draws: &[PaneDraw],
        images: ImageLayer<'_>,
        window_bg: mtty_ui::theme::Rgb,
        paint_jobs: &[egui::ClippedPrimitive],
        screen: &egui_wgpu::ScreenDescriptor,
    ) {
        let (w, h) = (self.config.width, self.config.height);
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("shot"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = enc
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(linear_color(window_bg, self.opacity)),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                })
                .forget_lifetime();
            self.quads.render(&mut pass);
            // Behind-text images, then the glyphs, then the rest (see `render`).
            if !images.quads.is_empty() {
                let (sx, sy, sw, sh) = grid_scissor(images.rects, images.scale, w, h);
                pass.set_scissor_rect(sx, sy, sw, sh);
                self.images.render_all(&mut pass, true);
                pass.set_scissor_rect(0, 0, w, h);
            }
            for d in draws {
                if let Some(r) = self.renderers.get(&d.id) {
                    r.render(&mut pass);
                }
            }
            if !images.quads.is_empty() {
                let (sx, sy, sw, sh) = grid_scissor(images.rects, images.scale, w, h);
                pass.set_scissor_rect(sx, sy, sw, sh);
                self.images.render_all(&mut pass, false);
                pass.set_scissor_rect(0, 0, w, h);
            }
            self.egui_renderer.render(&mut pass, paint_jobs, screen);
        }
        let bpr = (w * 4) as usize;
        let padded = (bpr + 255) & !255;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (padded * h as usize) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        enc.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &buf,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded as u32),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(enc.finish()));
        let slice = buf.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.device.poll(wgpu::Maintain::Wait);
        let data = slice.get_mapped_range();
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        for y in 0..h as usize {
            let row = &data[y * padded..y * padded + bpr];
            for x in 0..w as usize {
                ppm.push(row[x * 4 + 2]);
                ppm.push(row[x * 4 + 1]);
                ppm.push(row[x * 4]);
            }
        }
        drop(data);
        let _ = std::fs::write("/tmp/mtty_shot.ppm", ppm);
        std::process::exit(0);
    }

    /// Apply a URL-scheme / argv launch intent (ADR 0013).
    fn apply_launch(&mut self, intent: &mtty_ui::launch::Intent) {
        use mtty_ui::launch::Intent;
        match intent {
            // A second plain launch was forwarded here: bring the window forward.
            Intent::Activate => {
                self.window.set_visible(true);
                self.window.focus_window();
            }
            Intent::Quick => self.toggle_quick_terminal(),
            Intent::Focus(id) => {
                // Like MTP `pane.focus`: the tab and the pane inside it.
                if let Some(i) = self
                    .tabs
                    .iter()
                    .position(|t| t.panes.iter().any(|p| &p.id == id))
                {
                    self.tabs[i].active = id.clone();
                    self.active_tab = i;
                    self.selection = None;
                }
                self.window.set_visible(true);
                self.window.focus_window();
            }
            Intent::Sftp(name) => {
                self.reload_hosts();
                match self
                    .host_book
                    .hosts
                    .iter()
                    .find(|h| &h.name == name)
                    .cloned()
                {
                    Some(host) => self.open_sftp(
                        host.name.clone(),
                        mtty_ui::sftp::Remote {
                            destination: host.destination(),
                            options: host.ssh_options(),
                        },
                    ),
                    None => {
                        let msg = format!(
                            "{} {name}",
                            mtty_ui::i18n::t(
                                self.lang,
                                "No saved host named",
                                "没有名为此的已保存主机:"
                            )
                        );
                        self.show_notice(msg);
                    }
                }
                self.window.set_visible(true);
                self.window.focus_window();
            }
            Intent::Host(name) => {
                self.reload_hosts();
                match self
                    .host_book
                    .hosts
                    .iter()
                    .find(|h| &h.name == name)
                    .cloned()
                {
                    Some(host) => self.open_host(&host),
                    None => {
                        let msg = format!(
                            "{} {name}",
                            mtty_ui::i18n::t(
                                self.lang,
                                "No saved host named",
                                "没有名为此的已保存主机:"
                            )
                        );
                        self.show_notice(msg);
                    }
                }
                self.window.set_visible(true);
                self.window.focus_window();
            }
            Intent::Run(cmd) => {
                self.new_tab();
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    let active = tab.active.clone();
                    if let Some(pane) = tab.panes.iter_mut().find(|p| p.id == active) {
                        pane.term.write(format!("{cmd}\r").as_bytes());
                    }
                }
                self.publish_panes();
            }
        }
    }

    /// Notifications + sleep guard (ADR 0010).
    /// After a notification, bringing mtty forward shows the pane it was
    /// about (within two minutes, and only if that tab still wants a look).
    fn follow_recent_alert(&mut self) {
        let Some((pane, at)) = self.alert_target.take() else {
            return;
        };
        if at.elapsed() > Duration::from_secs(120) {
            return;
        }
        if let Some(i) = self
            .tabs
            .iter()
            .position(|t| t.attention.is_some() && t.panes.iter().any(|p| p.id == pane))
        {
            self.tabs[i].active = pane;
            self.active_tab = i;
            self.selection = None;
            self.window.request_redraw();
        }
    }

    /// Mark the tab holding `pane` unless it is what the user is looking at.
    fn raise_attention(&mut self, pane: &str, level: Attention) {
        let visible = self.focused
            && self
                .tabs
                .get(self.active_tab)
                .is_some_and(|t| t.panes.iter().any(|p| p.id == pane));
        if visible && !level.shows_while_visible() {
            return;
        }
        if let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|t| t.panes.iter().any(|p| p.id == pane))
        {
            tab.attention = tab.attention.max(Some(level));
            self.window.request_redraw();
        }
    }

    /// The user acted on `pane` (typed, pasted) or its agent started again:
    /// its tab's finished mark has done its job.
    fn clear_done(&mut self, pane: &str) {
        if let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|t| t.panes.iter().any(|p| p.id == pane))
        {
            if tab.attention == Some(Attention::Done) {
                tab.attention = None;
                self.window.request_redraw();
            }
        }
    }

    /// Type a queued prompt into its pane (or the active pane when it has no
    /// target). Read-only mode holds it back.
    fn deliver_prompt(&mut self, item: mtty_ui::agentloop::QueuedPrompt) {
        if self.read_only {
            self.prompt_queue.items.insert(0, item);
            return;
        }
        let target = item.pane.clone().or_else(|| self.active_pane_id());
        let pane = self
            .tabs
            .iter_mut()
            .flat_map(|t| t.panes.iter_mut())
            .find(|p| Some(&p.id) == target.as_ref());
        match pane {
            Some(pane) => {
                pane.scroll = 0;
                pane.term.write(format!("{}\r", item.text).as_bytes());
            }
            // The pane is gone: keep the prompt for the user to resend.
            None => self
                .prompt_queue
                .items
                .push(mtty_ui::agentloop::QueuedPrompt { pane: None, ..item }),
        }
    }

    fn agent_loop(&mut self) {
        // Transitions come from the control plane in order, so a quick
        // processing -> idle between two loop iterations is still seen (the
        // prompt queue and notifications both act on transitions).
        let focused_id = self.active_pane_id();
        let mut alert: Option<(String, String)> = None;
        let mut deliveries = Vec::new();
        for change in self.mtp.take_transitions() {
            let prev = self
                .agent_states
                .insert(change.pane.clone(), change.state.clone());
            if let Some(item) =
                self.prompt_queue
                    .on_state(&change.pane, prev.as_deref(), &change.state)
            {
                deliveries.push(item);
            }
            if matches!(
                change.state.as_str(),
                "processing" | "waiting" | "incomplete" | "unknown"
            ) || (change.agent == "miao" && change.state == "idle")
            {
                self.clear_done(&change.pane);
            }
            // miao reports task completion explicitly; idle only means no execution.
            let attention = if change.agent == "miao" && change.state == "idle" {
                None
            } else {
                Attention::for_transition(prev.as_deref(), &change.state)
            };
            if let Some(level) = attention {
                // A state switched off in `[badges]` marks nothing.
                if !switched_off(&self.badges, &change.state) {
                    self.raise_attention(&change.pane, level);
                }
            }
            let changed = prev.as_deref() != Some(change.state.as_str());
            let wants = matches!(change.state.as_str(), "awaiting" | "error");
            let focused = Some(&change.pane) == focused_id.as_ref();
            if changed && wants && self.notifications && !self.focused && !focused {
                let body = self
                    .tabs
                    .iter()
                    .find(|t| t.panes.iter().any(|p| p.id == change.pane))
                    .map(|t| self.title_of(t))
                    .unwrap_or_default();
                let agent = if change.agent.is_empty() {
                    "agent"
                } else {
                    change.agent.as_str()
                };
                alert = Some((format!("{agent} \u{00b7} {}", change.state), body));
                self.alert_target = Some((change.pane.clone(), Instant::now()));
            }
        }
        if !deliveries.is_empty() {
            for item in deliveries {
                self.deliver_prompt(item);
            }
            self.save_queue();
        }
        if let Some((title, body)) = alert {
            mtty_ui::agentloop::notify(&title, &body);
        }
        if self.prevent_sleep {
            let any_processing = self.tabs.iter().flat_map(|t| &t.panes).any(|p| {
                self.mtp
                    .agent_for(&p.id)
                    .and_then(|a| {
                        a.get("state")
                            .and_then(|v| v.as_str())
                            .map(|s| s == "processing")
                    })
                    .unwrap_or(false)
            });
            self.sleep.set_awake(any_processing);
        }
    }

    /// The active pane's most recent scrollback as `(absolute line, cells)`,
    /// newest first. Trailing blank cells are dropped and the capture is
    /// capped, so a long scrollback stays a small snapshot to hand to a
    /// worker thread.
    fn quick_scrollback_lines(&self) -> Vec<quick_content::ScrollLine> {
        let Some(pane) = self.active_pane() else {
            return Vec::new();
        };
        let screen = pane.term.screen();
        let total = screen.total_lines();
        let start = total.saturating_sub(QUICK_SCROLL_LINES);
        let mut out = Vec::new();
        let mut budget = QUICK_SCROLL_CELLS;
        for b in (start..total).rev() {
            let mut cells = screen.line_chars_abs(b);
            while matches!(cells.last(), Some((_, ' ', 1))) {
                cells.pop();
            }
            if cells.is_empty() {
                continue;
            }
            budget = budget.saturating_sub(cells.len());
            out.push((b, cells));
            if budget == 0 {
                break;
            }
        }
        out
    }

    /// The content hits found so far, and whether the scan has finished.
    fn quick_content_hits(
        &self,
    ) -> (
        Vec<quick_content::FileHit>,
        Vec<quick_content::ScrollHit>,
        bool,
    ) {
        let Some(bg) = &self.quick_bg else {
            return (Vec::new(), Vec::new(), true);
        };
        let files = bg
            .file_hits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let scroll = bg
            .scroll_hits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let done = bg.done.load(std::sync::atomic::Ordering::Acquire);
        (files, scroll, done)
    }

    /// Search files under `root` and the captured `lines` on a worker thread,
    /// so a slow directory never blocks typing. Replaces any previous scan,
    /// which its `Drop` cancels.
    fn start_quick_content(
        &mut self,
        key: String,
        query: String,
        root: Option<std::path::PathBuf>,
        lines: Vec<quick_content::ScrollLine>,
    ) {
        let file_hits = Arc::new(std::sync::Mutex::new(Vec::new()));
        let scroll_hits = Arc::new(std::sync::Mutex::new(Vec::new()));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (fh, sh, d, c) = (
            file_hits.clone(),
            scroll_hits.clone(),
            done.clone(),
            cancel.clone(),
        );
        let proxy = self.proxy.clone();
        let limits = quick_content::ScanLimits::default();
        let _ = std::thread::Builder::new()
            .name("mtty-quick".into())
            .spawn(move || {
                use std::sync::atomic::Ordering;
                let wake = || {
                    let _ = proxy.send_event(HostEvent::Wake);
                };
                let scroll = quick_content::scan_scrollback(&lines, &query, QUICK_SCROLL_MAX_HITS);
                if !c.load(Ordering::Relaxed) {
                    *sh.lock().unwrap_or_else(|e| e.into_inner()) = scroll;
                }
                wake();
                if let Some(root) = root {
                    let files = quick_content::scan_files(&root, &query, limits, &c);
                    if !c.load(Ordering::Relaxed) {
                        *fh.lock().unwrap_or_else(|e| e.into_inner()) = files;
                    }
                }
                d.store(true, Ordering::Release);
                wake();
            });
        self.quick_hit = None;
        self.quick_bg = Some(BgQuickContent {
            key,
            file_hits,
            scroll_hits,
            done,
            cancel,
        });
    }

    fn quick_window(&mut self, ctx: &egui::Context) {
        enum Pick {
            Tab(usize),
            Pane(usize, String),
            Host(usize),
            Snippet(usize),
            File(String),
            Dir(String),
            Path(std::path::PathBuf),
            /// A file-content match: open `path` in the editor at `line`.
            Content(std::path::PathBuf, usize),
            /// A scrollback match: scroll the active pane to absolute `line`
            /// and highlight its column.
            Scrollback(usize, u16, u16),
        }
        let cwd = self.cwd();
        // Files come from the background directory listing (shared with the
        // Files tree), so they are there whether or not that panel is open.
        if self.quick.is_none() {
            return;
        }
        if let Some(dir) = cwd.clone() {
            self.load_tree(&dir);
        }
        let tabs: Vec<(usize, String)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| (i, self.title_of(t)))
            .collect();
        let agents: Vec<(usize, String, String)> = self
            .tabs
            .iter()
            .enumerate()
            .flat_map(|(i, t)| {
                let title = self.title_of(t);
                t.panes
                    .iter()
                    .map(move |p| (i, p.id.clone(), title.clone()))
            })
            .filter_map(|(i, pane, title)| {
                let a = self.mtp.agent_for(&pane)?;
                let agent = a.get("agent").and_then(|v| v.as_str()).unwrap_or("agent");
                let state = a.get("state").and_then(|v| v.as_str()).unwrap_or("");
                Some((
                    i,
                    pane,
                    format!("{agent} \u{00b7} {state} \u{00b7} {title}"),
                ))
            })
            .collect();
        let snippets: Vec<(usize, String)> = self
            .snippet_book
            .snippets
            .iter()
            .enumerate()
            .map(|(i, s)| (i, format!("{} \u{00b7} {}", s.name, s.command)))
            .collect();
        let saved_hosts: Vec<(usize, String)> = self
            .host_book
            .hosts
            .iter()
            .enumerate()
            .map(|(i, h)| (i, format!("{} \u{00b7} {}", h.name, h.summary())))
            .collect();
        let files: Vec<(String, bool)> = cwd
            .as_ref()
            .and_then(|dir| self.tree_children.get(dir))
            .map(|entries| entries.iter().map(|f| (f.name.clone(), f.is_dir)).collect())
            .unwrap_or_default();
        let recents = self.recent_files.clone();
        let counts = self.open_counts.clone();
        // Content search runs on a worker thread; restart it when the query,
        // the directory or the active pane changes. Very short queries are
        // left alone: they would match nearly every line.
        let qtext = self.quick.clone().unwrap_or_default();
        let pane_id = self.active_pane_id().unwrap_or_default();
        let key = format!(
            "{qtext}\u{0}{}\u{0}{pane_id}",
            cwd.as_ref()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        if self.quick_bg.as_ref().map(|b| b.key.as_str()) != Some(key.as_str()) {
            if qtext.chars().count() >= QUICK_CONTENT_MIN_CHARS {
                let lines = self.quick_scrollback_lines();
                self.start_quick_content(key, qtext, cwd.clone(), lines);
            } else {
                self.quick_bg = None;
            }
        }
        let (content_hits, scroll_hits, content_done) = self.quick_content_hits();
        let Some(query) = self.quick.as_mut() else {
            return;
        };
        let mut chosen: Option<Pick> = None;
        let mut open = true;
        app_window(mtty_ui::i18n::t(self.lang, "Open Quickly", "快速打开"), ctx)
            .collapsible(false)
            .default_size([460.0, 420.0])
            .open(&mut open)
            .show(ctx, |ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(query)
                        .hint_text(mtty_ui::i18n::t(
                            self.lang,
                            "tab / agent / file / text",
                            "标签 / agent / 文件 / 内容",
                        ))
                        .desired_width(420.0),
                );
                // Enter makes the field give up focus; check it before re-taking focus.
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if !enter {
                    r.request_focus();
                }
                if !content_done {
                    ui.label(
                        egui::RichText::new(mtty_ui::i18n::t(self.lang, "Searching…", "搜索中…"))
                            .size(11.0)
                            .color(chrome_rgb(self.theme.chrome().muted)),
                    );
                }
                let q = query.to_lowercase();
                let freq = |p: &str| std::cmp::Reverse(*counts.get(p).unwrap_or(&0));
                let mut rows: Vec<(usize, std::cmp::Reverse<u32>, String, Pick)> = Vec::new();
                for (i, title) in &tabs {
                    if let Some(s) = mtty_ui::palette::score(title, "tab", &q) {
                        rows.push((s, freq(title), format!("\u{21e5} {title}"), Pick::Tab(*i)));
                    }
                }
                for (i, pane, label) in &agents {
                    if let Some(s) = mtty_ui::palette::score(label, "agent", &q) {
                        rows.push((
                            s,
                            freq(label),
                            format!("\u{2726} {label}"),
                            Pick::Pane(*i, pane.clone()),
                        ));
                    }
                }
                for (i, label) in &snippets {
                    if let Some(s) = mtty_ui::palette::score(label, "snippet", &q) {
                        rows.push((
                            s,
                            freq(label),
                            format!("\u{276f} {label}"),
                            Pick::Snippet(*i),
                        ));
                    }
                }
                for (i, label) in &saved_hosts {
                    if let Some(s) = mtty_ui::palette::score(label, "host ssh", &q) {
                        rows.push((s, freq(label), format!("\u{21c4} {label}"), Pick::Host(*i)));
                    }
                }
                for (name, is_dir) in &files {
                    let kind = if *is_dir { "dir" } else { "file" };
                    if let Some(s) = mtty_ui::palette::score(name, kind, &q) {
                        let icon = if *is_dir { "\u{ea83}" } else { " " };
                        let pick = if *is_dir {
                            Pick::Dir(name.clone())
                        } else {
                            Pick::File(name.clone())
                        };
                        let f = cwd
                            .as_ref()
                            .map(|c| c.join(name).to_string_lossy().to_string())
                            .unwrap_or_default();
                        rows.push((s, freq(&f), format!("{icon}  {name}"), pick));
                    }
                }
                for path in &recents {
                    if let Some(s) = mtty_ui::palette::score(path, "recent", &q) {
                        rows.push((
                            s,
                            freq(path),
                            format!("\u{21ba} {path}"),
                            Pick::Path(std::path::PathBuf::from(path)),
                        ));
                    }
                }
                for h in &content_hits {
                    let key = h.path.to_string_lossy();
                    let rel = cwd
                        .as_ref()
                        .and_then(|c| h.path.strip_prefix(c).ok())
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| key.to_string());
                    let label = format!("\u{ea6d} {rel}:{}  {}", h.line, h.text);
                    if let Some(s) = mtty_ui::palette::score(&label, "content", &q) {
                        rows.push((s, freq(&key), label, Pick::Content(h.path.clone(), h.line)));
                    }
                }
                for h in &scroll_hits {
                    let label = format!("\u{f1da} {}  {}", h.line + 1, h.text);
                    if let Some(s) = mtty_ui::palette::score(&label, "scrollback", &q) {
                        rows.push((
                            s,
                            std::cmp::Reverse(0u32),
                            label,
                            Pick::Scrollback(h.line, h.col, h.width),
                        ));
                    }
                }
                rows.sort_by_key(|r| (r.0, r.1));
                let clone_pick = |p: &Pick| match p {
                    Pick::Tab(i) => Pick::Tab(*i),
                    Pick::Pane(i, id) => Pick::Pane(*i, id.clone()),
                    Pick::Host(i) => Pick::Host(*i),
                    Pick::Snippet(i) => Pick::Snippet(*i),
                    Pick::File(n) => Pick::File(n.clone()),
                    Pick::Dir(n) => Pick::Dir(n.clone()),
                    Pick::Path(p) => Pick::Path(p.clone()),
                    Pick::Content(p, l) => Pick::Content(p.clone(), *l),
                    Pick::Scrollback(l, c, w) => Pick::Scrollback(*l, *c, *w),
                };
                for (_, _, label, pick) in rows.iter().take(50) {
                    if ui.selectable_label(false, label).clicked() {
                        chosen = Some(clone_pick(pick));
                    }
                }
                if enter {
                    if let Some((_, _, _, p)) = rows.first() {
                        chosen = Some(clone_pick(p));
                    }
                }
            });
        if let Some(p) = chosen {
            self.quick = None;
            self.quick_bg = None;
            match p {
                Pick::Tab(i) => {
                    if i < self.tabs.len() {
                        self.active_tab = i;
                        self.selection = None;
                    }
                }
                Pick::Host(i) => {
                    if let Some(host) = self.host_book.hosts.get(i).cloned() {
                        self.open_host(&host);
                    }
                }
                Pick::Snippet(i) => {
                    if let Some(sn) = self.snippet_book.snippets.get(i).cloned() {
                        self.run_snippet_here(&sn.command);
                    }
                }
                Pick::Pane(i, id) => {
                    if let Some(tab) = self.tabs.get_mut(i) {
                        if tab.panes.iter().any(|p| p.id == id) {
                            tab.active = id;
                        }
                        self.active_tab = i;
                        self.selection = None;
                    }
                }
                Pick::File(name) => {
                    if let Some(path) = cwd.map(|c| c.join(&name)) {
                        self.open_editor(path);
                    }
                }
                Pick::Dir(name) => {
                    if let Some(path) = cwd.map(|c| c.join(&name)) {
                        self.new_tab_in(Some(path));
                    }
                }
                Pick::Path(path) => {
                    self.open_editor(path);
                }
                Pick::Content(path, line) => {
                    if self.open_editor(path) {
                        if let Some(ed) = self.active_editor_mut() {
                            ed.go_to_line_col(line.saturating_sub(1), 0);
                            ed.reveal_cursor();
                        }
                    }
                }
                Pick::Scrollback(line, col, width) => {
                    if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                        if let Some(pane) = tab.panes.iter_mut().find(|p| p.id == tab.active) {
                            let hist = pane.term.screen().history_size();
                            pane.scroll = hist.saturating_sub(line);
                        }
                    }
                    self.quick_hit = Some((line, col, width));
                }
            }
        } else if !open {
            self.quick = None;
            self.quick_bg = None;
        }
    }

    fn compute_search_hits(&self) -> Vec<(usize, u16, u16)> {
        let Some(q) = self.search.as_deref() else {
            return Vec::new();
        };
        if q.is_empty() {
            return Vec::new();
        }
        let Some(pane) = self.active_pane() else {
            return Vec::new();
        };
        let screen = pane.term.screen();
        let mut out = Vec::new();
        for b in 0..screen.total_lines() {
            for (col, width) in find_in_cells(&screen.line_chars_abs(b), q) {
                out.push((b, col, width));
                if out.len() >= 2000 {
                    return out;
                }
            }
        }
        out
    }

    fn refresh_search(&mut self) {
        if self.search.is_none() {
            self.search_hits.clear();
            self.search_key.clear();
            self.editor_hits.clear();
            self.editor_hits_query.clear();
            self.find_error = None;
            self.bg_search = None;
            return;
        }
        if self.active_editor().is_some() {
            self.refresh_editor_search();
            return;
        }
        let key = format!(
            "{}\u{0}{}",
            self.search.as_deref().unwrap_or(""),
            self.active_pane_id().unwrap_or_default()
        );
        if key != self.search_key {
            self.search_key = key;
            self.search_hits = self.compute_search_hits();
            self.search_idx = self
                .search_idx
                .min(self.search_hits.len().saturating_sub(1));
        }
    }

    /// The Find bar's query with its options, for an editor pane.
    fn editor_query(&self) -> mtty_editor::SearchQuery {
        mtty_editor::SearchQuery {
            pattern: self.search.clone().unwrap_or_default(),
            regex: self.find_opts.regex,
            case_sensitive: self.find_opts.case_sensitive,
            whole_word: self.find_opts.whole_word,
        }
    }

    /// Find in the active editor pane: matches are found again when the
    /// query, the pane or the text changes; a new query selects the first
    /// match at or after the caret.
    fn refresh_editor_search(&mut self) {
        let query = self.search.clone().unwrap_or_default();
        let Some(ed) = self.active_editor() else {
            return;
        };
        let o = &self.find_opts;
        let key = format!(
            "{query}\u{0}{}\u{0}{}\u{0}{}\u{0}{}{}{}",
            ed.id,
            ed.doc.revision(),
            ed.is_view_only(),
            o.case_sensitive,
            o.whole_word,
            o.regex,
        );
        let background = ed.is_view_only() || ed.doc.rope().len_bytes() > BG_SEARCH_BYTES;
        let changed = key != self.search_key;
        let options = format!("{}{}{}", o.case_sensitive, o.whole_word, o.regex);
        let tagged = format!("{query}\u{0}{options}");
        let new_query = tagged != self.editor_hits_query || self.find_rejump;
        if background {
            if changed || self.find_rejump {
                self.find_rejump = false;
                self.search_key = key;
                self.editor_hits_query = tagged;
                self.editor_hits.clear();
                self.search_idx = 0;
                self.find_error = None;
                self.bg_search = Some(self.spawn_search(new_query));
            }
            self.poll_bg_search();
            return;
        }
        if !changed && !self.find_rejump {
            return;
        }
        let caret = ed.doc.selection().primary().from();
        let found = mtty_editor::search::find_all(ed.doc.rope(), &self.editor_query());
        self.find_rejump = false;
        self.find_error = match &found {
            Err(mtty_editor::SearchError::Invalid(why)) => Some(why.clone()),
            _ => None,
        };
        let found = found.unwrap_or_default();
        self.bg_search = None;
        self.search_key = key;
        self.editor_hits = found;
        if new_query {
            self.editor_hits_query = tagged;
            self.search_idx = first_hit_from(&self.editor_hits, caret);
            self.scroll_to_search_hit();
        } else {
            self.search_idx = self
                .search_idx
                .min(self.editor_hits.len().saturating_sub(1));
        }
    }

    /// Search the active editor on a thread: the file itself in view mode
    /// (byte ranges), else a snapshot of the document (char ranges).
    fn spawn_search(&self, jump: bool) -> BgSearch {
        let editor_query = self.editor_query();
        let query = editor_query.pattern.clone();
        use std::sync::atomic::{AtomicBool, Ordering};
        let hits = Arc::new(std::sync::Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let view = self
            .active_editor()
            .and_then(|e| e.large.as_ref().map(|l| l.file.clone()));
        let rope = self.active_editor().map(|e| e.doc.rope().clone());
        let bytes = view.is_some();
        let proxy = self.proxy.clone();
        let (h, d, c) = (hits.clone(), done.clone(), cancel.clone());
        let _ = std::thread::Builder::new()
            .name("mtty-find".into())
            .spawn(move || {
                let wake = || {
                    let _ = proxy.send_event(HostEvent::Wake);
                };
                if let Some(file) = view {
                    let mut batch = Vec::new();
                    let mut last = Instant::now();
                    file.search(&query, MAX_SEARCH_HITS, &c, |a, b| {
                        batch.push((a as usize, b as usize));
                        if last.elapsed() > Duration::from_millis(100) {
                            h.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .append(&mut batch);
                            last = Instant::now();
                            wake();
                        }
                    });
                    h.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .append(&mut batch);
                } else if let Some(rope) = rope {
                    let found =
                        mtty_editor::search::find_all(&rope, &editor_query).unwrap_or_default();
                    if !c.load(Ordering::Relaxed) {
                        let mut found = found;
                        found.truncate(MAX_SEARCH_HITS);
                        *h.lock().unwrap_or_else(|e| e.into_inner()) = found;
                    }
                }
                d.store(true, Ordering::Release);
                wake();
            });
        BgSearch {
            hits,
            done,
            cancel,
            jump,
            bytes,
        }
    }

    /// Take the matches a search thread found so far; a new query selects
    /// the first match at or after the caret once there is one.
    fn poll_bg_search(&mut self) {
        let Some(bg) = &self.bg_search else {
            return;
        };
        let (count, jump, bytes, done) = {
            let hits = bg.hits.lock().unwrap_or_else(|e| e.into_inner());
            if hits.len() != self.editor_hits.len() {
                self.editor_hits = hits.clone();
            }
            (
                hits.len(),
                bg.jump,
                bg.bytes,
                bg.done.load(std::sync::atomic::Ordering::Acquire),
            )
        };
        if !jump || count == 0 {
            return;
        }
        let Some(ed) = self.active_editor() else {
            return;
        };
        // Where the caret is, in the hits' units.
        let caret = if bytes {
            let (line, _) = ed.caret_line_col();
            ed.large
                .as_ref()
                .map_or(0, |l| l.file.line_start(line - 1) as usize)
        } else {
            ed.doc.selection().primary().from()
        };
        let after = self.editor_hits.iter().position(|&(a, _)| a >= caret);
        // Wait for a match after the caret unless the scan is over.
        if after.is_none() && !done {
            return;
        }
        self.search_idx = after.unwrap_or(0);
        if let Some(bg) = &mut self.bg_search {
            bg.jump = false;
        }
        self.scroll_to_search_hit();
    }

    /// Matches for the open Find bar, in the editor or the terminal.
    fn search_count(&self) -> usize {
        if self.active_editor().is_some() {
            self.editor_hits.len()
        } else {
            self.search_hits.len()
        }
    }

    fn scroll_to_search_hit(&mut self) {
        if self.active_editor().is_some() {
            let Some((start, end)) = self.editor_hits.get(self.search_idx).copied() else {
                return;
            };
            let bytes = self.bg_search.as_ref().is_some_and(|b| b.bytes);
            if let Some(ed) = self.active_editor_mut() {
                if bytes {
                    ed.select_bytes(start as u64, end as u64);
                    return;
                }
                ed.doc
                    .set_selection(mtty_editor::Selection::single(mtty_editor::Range::new(
                        start, end,
                    )));
                ed.reveal_cursor();
            }
            return;
        }
        let Some((b, _, _)) = self.search_hits.get(self.search_idx).copied() else {
            return;
        };
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        let Some(pane) = tab.panes.iter_mut().find(|p| p.id == tab.active) else {
            return;
        };
        let hist = pane.term.screen().history_size();
        pane.scroll = hist.saturating_sub(b);
    }

    /// Asks before loading a view-mode file for editing, with what it costs.
    fn large_edit_window(&mut self, ctx: &egui::Context) {
        self.poll_large_loading();
        let Some(id) = self.large_edit_offer.clone() else {
            return;
        };
        let Some((name, size)) = self.tabs.iter().find_map(|t| {
            t.editors
                .iter()
                .find(|e| e.id == id && e.is_view_only())
                .map(|e| {
                    (
                        e.path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        e.large.as_ref().map_or(0, |l| l.file.len_bytes()),
                    )
                })
        }) else {
            self.large_edit_offer = None;
            return;
        };
        use mtty_ui::i18n::t;
        let l = self.lang;
        let loading = self.large_loading.is_some();
        let (mut go, mut cancel) = (false, false);
        egui::Window::new(t(l, "Switch to Editing?", "切换为可编辑?"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(
                    if l == mtty_ui::i18n::Lang::En {
                        format!(
                            "{name} ({}) is open in view mode: read from disk a screen at a time, so it needs little memory.\n\nEditing loads the whole file into memory, about {} for this file. Opening, searching and saving it take longer too.",
                            human_bytes(size),
                            human_bytes(edit_memory_estimate(size)),
                        )
                    } else {
                        format!(
                            "{name}({})正以只读查看模式打开:按屏从磁盘读取,占用内存很少。\n\n切换为可编辑会把整个文件读入内存,此文件约需 {}。打开、查找和保存也会更慢。",
                            human_bytes(size),
                            human_bytes(edit_memory_estimate(size)),
                        )
                    }
                );
                ui.add_space(8.0);
                if loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t(l, "Loading for editing…", "正在加载为可编辑…"));
                    });
                } else {
                    ui.horizontal(|ui| {
                        go = ui.button(t(l, "Load for Editing", "加载为可编辑")).clicked();
                        cancel = ui.button(t(l, "Keep Viewing", "继续只读查看")).clicked();
                    });
                }
            });
        if cancel || (!loading && ctx.input(|i| i.key_pressed(egui::Key::Escape))) {
            self.large_edit_offer = None;
        }
        if go {
            let Some(path) = self.tabs.iter().find_map(|t| {
                t.editors
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.path.clone())
            }) else {
                return;
            };
            let (tx, rx) = std::sync::mpsc::channel();
            let proxy = self.proxy.clone();
            let _ = std::thread::Builder::new()
                .name("mtty-load".into())
                .spawn(move || {
                    let doc = std::fs::read(&path)
                        .map_err(|e| e.to_string())
                        .and_then(|b| {
                            mtty_editor::Document::from_bytes(&b).map_err(|e| e.to_string())
                        });
                    let _ = tx.send(doc);
                    let _ = proxy.send_event(HostEvent::Wake);
                });
            self.large_loading = Some((id, rx));
        }
    }

    /// A view-mode file finished loading for editing: the pane becomes an
    /// editor on the same line.
    fn poll_large_loading(&mut self) {
        let Some((id, rx)) = &self.large_loading else {
            return;
        };
        let Ok(result) = rx.try_recv() else {
            return;
        };
        let id = id.clone();
        self.large_loading = None;
        self.large_edit_offer = None;
        let lang = self.lang;
        let pane = self
            .tabs
            .iter_mut()
            .flat_map(|t| t.editors.iter_mut())
            .find(|e| e.id == id);
        match (result, pane) {
            (Ok(doc), Some(ed)) => {
                let (line, _) = ed.caret_line_col();
                let fresh = editor_pane::EditorPane::with_doc(ed.id.clone(), ed.path.clone(), doc);
                let (rows, cols) = (ed.rows, ed.cols);
                *ed = fresh;
                ed.set_vim(self.editor_vim);
                ed.rows = rows;
                ed.cols = cols;
                ed.go_to_line(line - 1);
                self.search_key.clear();
                self.bg_search = None;
                self.show_notice(
                    mtty_ui::i18n::t(lang, "The file is now editable.", "文件已可编辑。")
                        .to_string(),
                );
            }
            (Err(e), _) => {
                self.show_notice(format!(
                    "{}: {e}",
                    mtty_ui::i18n::t(lang, "Could not load for editing", "无法加载为可编辑")
                ));
            }
            (Ok(_), None) => {}
        }
        self.window.request_redraw();
    }

    /// Ask before an external edit replaces unsaved changes (see
    /// [`Self::reload_editors_if_changed`]). Escape keeps the local version.
    fn reload_dialog_window(&mut self, ctx: &egui::Context) {
        let Some((id, stamp)) = self.editor_reload_offer.clone() else {
            return;
        };
        let Some(name) = self.tabs.iter().find_map(|t| {
            t.editors.iter().find(|e| e.id == id).map(|e| {
                e.path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
        }) else {
            self.editor_reload_offer = None;
            return;
        };
        use mtty_ui::i18n::t;
        let l = self.lang;
        let (mut reload, mut keep) = (false, false);
        egui::Window::new(t(l, "File Changed on Disk", "文件已在磁盘上更改"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(440.0);
                ui.label(if l == mtty_ui::i18n::Lang::En {
                    format!(
                        "{name} changed on disk while this pane has unsaved edits.\n\nReloading replaces your text with the file's contents (you can undo it). Keeping your version asks again only if the file changes once more."
                    )
                } else {
                    format!(
                        "{name} 在磁盘上被修改,而此面板有未保存的编辑。\n\n“从磁盘重新加载”会用文件内容替换你的文本(可撤销);“保留我的版本”保留你的编辑,文件再次变化时才会再问。"
                    )
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    keep = ui.button(t(l, "Keep My Version", "保留我的版本")).clicked();
                    reload = ui.button(t(l, "Reload from Disk", "从磁盘重新加载")).clicked();
                });
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            keep = true;
        }
        if !reload && !keep {
            return;
        }
        self.editor_reload_offer = None;
        if let Some(ed) = self
            .tabs
            .iter_mut()
            .flat_map(|t| t.editors.iter_mut())
            .find(|e| e.id == id)
        {
            if reload {
                ed.reload_from_disk();
            } else {
                ed.disk = editor_pane::disk_stamp(&ed.path).or(Some(stamp));
                ed.missing_warned = false;
            }
        }
        self.window.request_redraw();
    }

    /// A right-click pane context menu, drawn as an egui popup. It reuses the
    /// same commands as the palette so behaviour stays identical.
    fn pane_context_menu(&mut self, ctx: &egui::Context) {
        let Some(menu) = self.pane_menu.clone() else {
            return;
        };
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let has_selection = self
            .selection
            .as_ref()
            .is_some_and(|(id, _)| id == &menu.pane);
        let clamped = clamp_menu_pos(ctx, menu.at);
        let chosen: std::cell::Cell<Option<PaneMenuAction>> = std::cell::Cell::new(None);
        let line_info = menu
            .cell
            .and_then(|(r, _)| self.active_line_info(&menu.pane, r));
        let mut open = true;
        egui::Area::new(egui::Id::new("pane-context-menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(clamped.0, clamped.1))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(190.0);
                    let item = |ui: &mut egui::Ui, label: &str, action: PaneMenuAction| {
                        if ui.button(label).clicked() {
                            chosen.set(Some(action));
                        }
                    };
                    if has_selection {
                        item(ui, t(lang, "Copy", "复制"), PaneMenuAction::Copy);
                    }
                    item(ui, t(lang, "Paste", "粘贴"), PaneMenuAction::Paste);
                    ui.menu_button(t(lang, "Copy / Paste as", "复制 / 粘贴为"), |ui| {
                        if ui
                            .add_enabled(
                                has_selection,
                                egui::Button::new(t(lang, "Copy as ANSI", "复制为 ANSI")),
                            )
                            .clicked()
                        {
                            chosen.set(Some(PaneMenuAction::CopyAnsi));
                            ui.close_menu();
                        }
                        if ui
                            .button(t(lang, "Paste as Shell-Escaped", "粘贴为 Shell 转义"))
                            .clicked()
                        {
                            chosen.set(Some(PaneMenuAction::PasteEscaped));
                            ui.close_menu();
                        }
                    });
                    ui.separator();
                    item(ui, t(lang, "Composer", "撰写"), PaneMenuAction::Composer);
                    item(
                        ui,
                        t(lang, "Send to Agent…", "发送给 Agent…"),
                        PaneMenuAction::SendToAgent,
                    );
                    ui.separator();
                    item(ui, t(lang, "Select All", "全选"), PaneMenuAction::SelectAll);
                    item(ui, t(lang, "Search…", "搜索…"), PaneMenuAction::Search);
                    ui.separator();
                    ui.menu_button(
                        t(lang, "About This Line", "关于本行"),
                        |ui| match &line_info {
                            Some(info) => {
                                for (k, v) in info {
                                    ui.label(format!("{k}: {v}"));
                                }
                            }
                            None => {
                                ui.label(t(lang, "No line under the cursor.", "光标下没有行。"));
                            }
                        },
                    );
                    ui.separator();
                    ui.menu_button(t(lang, "Split Pane", "分屏"), |ui| {
                        for (en, zh, action) in [
                            ("Split Right", "向右分屏", PaneMenuAction::SplitRight),
                            ("Split Left", "向左分屏", PaneMenuAction::SplitLeft),
                            ("Split Down", "向下分屏", PaneMenuAction::SplitDown),
                            ("Split Up", "向上分屏", PaneMenuAction::SplitUp),
                        ] {
                            if ui.button(t(lang, en, zh)).clicked() {
                                chosen.set(Some(action));
                                ui.close_menu();
                            }
                        }
                    });
                    ui.separator();
                    item(
                        ui,
                        t(lang, "Clear Scrollback", "清除回滚"),
                        PaneMenuAction::ClearScrollback,
                    );
                });
            });
        // Escape closes the menu; a click outside is handled by egui (no item
        // was chosen, and the area loses its popup).
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            open = false;
        }
        let action = chosen.get();
        if action.is_some() {
            open = false;
        }
        if !open {
            self.pane_menu = None;
        }
        if let Some(action) = action {
            // The menu targets `menu.pane`; focus it before running.
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                if tab.active != menu.pane && tab.panes.iter().any(|p| p.id == menu.pane) {
                    tab.active = menu.pane.clone();
                    self.selection = None;
                }
            }
            self.run_pane_menu_action(action);
        }
    }

    /// The lines under a pane's cursor row, for "About This Line".
    fn active_line_info(&self, pane: &str, row: u16) -> Option<Vec<(String, String)>> {
        let tab = self.tabs.get(self.active_tab)?;
        let p = tab.panes.iter().find(|p| p.id == pane)?;
        let text = p.term.screen().line_text(row);
        if text.trim().is_empty() {
            return None;
        }
        let (_, col) = p.term.screen().cursor_position();
        let cell = p.term.screen().cell(row, col);
        let mut info = vec![
            ("Row".to_string(), (row + 1).to_string()),
            ("Column".to_string(), (col + 1).to_string()),
        ];
        if let Some(c) = cell {
            info.push(("Char".to_string(), format!("{:?}", c.ch)));
            if c.wide_spacer {
                info.push(("Wide".to_string(), "yes".to_string()));
            }
        }
        info.push(("Line".to_string(), text.trim_end().to_string()));
        Some(info)
    }

    /// Run a menu action by reusing an existing command where one exists.
    fn run_pane_menu_action(&mut self, action: PaneMenuAction) {
        use PaneMenuAction::*;
        let cmd = match action {
            Copy => Cmd::Copy,
            Paste => Cmd::Paste,
            CopyAnsi => Cmd::CopyAnsi,
            PasteEscaped => Cmd::PasteEscaped,
            Composer => Cmd::Composer,
            SendToAgent => Cmd::SendSelectionToAgent,
            SelectAll => Cmd::SelectAll,
            Search => Cmd::Find,
            SplitRight => Cmd::SplitRight,
            SplitLeft => Cmd::SplitLeft,
            SplitDown => Cmd::SplitDown,
            SplitUp => Cmd::SplitUp,
            ClearScrollback => Cmd::ClearScrollback,
        };
        self.run_command(cmd);
    }

    /// Markdown is one variable-height writing surface, not a second leaf in
    /// the pane layout. Editing uses the same document as save/LSP/session APIs.
    fn live_markdown_panes(&mut self, ctx: &egui::Context) {
        let rects = self.pane_rects();
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        let read_only = self.read_only;
        let save = !read_only
            && tab
                .editors
                .iter()
                .any(|e| e.id == tab.active && e.markdown.is_some())
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S));
        let ch = self.theme.chrome();
        let fg = mtty_ui::chrome::bg_color(ch.text);
        let panel = mtty_ui::chrome::bg_color(ch.card);
        for ed in &mut tab.editors {
            let Some(live) = ed.markdown.as_mut() else {
                continue;
            };
            let Some((_, r)) = rects.iter().find(|(id, _)| *id == ed.id) else {
                continue;
            };
            let r = card_inner(*r);
            let rect = egui::Rect::from_min_size(egui::pos2(r.x, r.y), egui::vec2(r.w, r.h));
            let base = ed.path.parent();
            let area = egui::Area::new(egui::Id::new(("mtty-live-markdown", &ed.id)))
                .order(egui::Order::Background)
                .fixed_pos(rect.min)
                .show(ctx, |ui| {
                    ui.set_clip_rect(rect);
                    ui.set_width(rect.width());
                    ui.set_height(rect.height());
                    ui.add_enabled_ui(!read_only, |ui| {
                        live.show(ui, &mut ed.doc, |ui, text| {
                            render_markdown(
                                ui,
                                text,
                                base,
                                &mut self.cmark,
                                &mut self.mmd,
                                fg,
                                panel,
                            );
                        });
                    });
                });
            if ctx.input(|i| {
                i.pointer.any_pressed()
                    && i.pointer.interact_pos().is_some_and(|p| rect.contains(p))
            }) && ctx
                .layer_id_at(rect.center())
                .is_some_and(|l| l == area.response.layer_id)
            {
                tab.active = ed.id.clone();
                self.selection = None;
            }
        }
        if save {
            self.save_active_editor();
        }
    }

    fn preview_panes(&mut self, ctx: &egui::Context) {
        let rects = self.pane_rects();
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        let mut shown = Vec::new();
        for preview in &mut tab.previews {
            let Some(ed) = tab.editors.iter().find(|e| e.id == preview.source) else {
                continue;
            };
            let Some((_, r)) = rects.iter().find(|(id, _)| *id == preview.id) else {
                continue;
            };
            if preview.revision != Some(ed.doc.revision()) {
                preview.revision = Some(ed.doc.revision());
                preview.text = Arc::new(ed.doc.rope().to_string());
            }
            let base = ed.path.parent().map(std::path::Path::to_path_buf);
            shown.push((
                preview.id.clone(),
                card_inner(*r),
                preview.text.clone(),
                base,
            ));
        }
        let ch = self.theme.chrome();
        let fg = mtty_ui::chrome::bg_color(ch.text);
        let panel = mtty_ui::chrome::bg_color(ch.card);
        let mut focus = None;
        for (id, r, text, base) in shown {
            let rect = egui::Rect::from_min_size(egui::pos2(r.x, r.y), egui::vec2(r.w, r.h));
            // The background order keeps Find and other windows above it.
            let area = egui::Area::new(egui::Id::new(("mtty-preview", &id)))
                .order(egui::Order::Background)
                .fixed_pos(rect.min)
                .show(ctx, |ui| {
                    ui.set_clip_rect(rect);
                    ui.set_width(rect.width());
                    ui.set_height(rect.height());
                    egui::ScrollArea::both()
                        .id_salt(("mtty-preview-scroll", &id))
                        .auto_shrink([false, false])
                        .max_width(rect.width())
                        .max_height(rect.height())
                        .show(ui, |ui| {
                            ui.set_max_width(rect.width() - 12.0);
                            render_markdown(
                                ui,
                                &text,
                                base.as_deref(),
                                &mut self.cmark,
                                &mut self.mmd,
                                fg,
                                panel,
                            );
                        });
                });
            let pressed = ctx.input(|i| {
                i.pointer.any_pressed()
                    && i.pointer.interact_pos().is_some_and(|p| rect.contains(p))
            });
            if pressed
                && ctx
                    .layer_id_at(rect.center())
                    .is_some_and(|l| l == area.response.layer_id)
            {
                focus = Some(id);
            }
        }
        if let (Some(id), Some(tab)) = (focus, self.tabs.get_mut(self.active_tab)) {
            if tab.active != id {
                tab.active = id;
                self.selection = None;
            }
        }
    }

    /// Open a Markdown preview of the active editor to its right, or close
    /// it when there is one (from the editor or the preview itself).
    fn toggle_markdown_preview(&mut self) {
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        let active = tab.active.clone();
        let source = match tab.previews.iter().find(|p| p.id == active) {
            Some(p) => Some(p.source.clone()),
            None => tab
                .editors
                .iter()
                .find(|e| e.id == active && !e.is_view_only())
                .map(|e| e.id.clone()),
        };
        let Some(source) = source else {
            self.show_notice(
                mtty_ui::i18n::t(
                    self.lang,
                    "Markdown preview works for a file open in an editor pane.",
                    "Markdown 预览用于在编辑器 pane 中打开的文件。",
                )
                .to_string(),
            );
            return;
        };
        if let Some(preview) = tab.previews.iter().find(|p| p.source == source) {
            let id = preview.id.clone();
            tab.previews.retain(|p| p.id != id);
            let _ = tab.layout.remove(&id);
            tab.active = source;
        } else {
            self.add_markdown_preview();
        }
        self.publish_panes();
    }

    /// A preview of the active editor split to its right, unless it has one.
    /// The editor keeps the focus.
    fn add_markdown_preview(&mut self) {
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        let source = tab.active.clone();
        let editable = tab
            .editors
            .iter()
            .any(|e| e.id == source && !e.is_view_only());
        if !editable || tab.previews.iter().any(|p| p.source == source) {
            return;
        }
        let preview = PreviewPane::new(source.clone());
        if tab.layout.split(&source, &preview.id, SplitDir::Right) {
            tab.previews.push(preview);
        }
        self.publish_panes();
    }

    /// Language servers, once a frame (ADR 0034, E5): every editor's text
    /// goes to its server when it changed, closed panes close their
    /// documents, and answers are handled.
    fn lsp_frame(&mut self) {
        let mut open = std::collections::HashSet::new();
        for tab in &self.tabs {
            for ed in tab
                .editors
                .iter()
                .filter(|e| !e.is_view_only() && e.remote.is_none())
            {
                open.insert(ed.path.clone());
                self.lsp
                    .sync(&ed.path, ed.language(), ed.doc.rope(), ed.doc.revision());
            }
        }
        self.lsp.retain(&open);
        for event in self.lsp.poll() {
            self.lsp_event(event);
        }
    }

    /// Send the active editor's latest text before asking about it.
    fn lsp_sync_active(&mut self) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        if let Some(ed) = tab.editors.iter().find(|e| e.id == tab.active) {
            if !ed.is_view_only() && ed.remote.is_none() {
                self.lsp
                    .sync(&ed.path, ed.language(), ed.doc.rope(), ed.doc.revision());
            }
        }
    }

    fn lsp_event(&mut self, event: mtty_lsp::Event) {
        use mtty_lsp::Event;
        match event {
            Event::Diagnostics(path) => {
                let list = self.lsp.diagnostics(&path).to_vec();
                let encoding = self.lsp.encoding(&path);
                for ed in self
                    .tabs
                    .iter_mut()
                    .flat_map(|t| t.editors.iter_mut())
                    .filter(|e| e.path == path && e.remote.is_none())
                {
                    let rope = ed.doc.rope();
                    let mut diagnostics: Vec<editor_pane::PaneDiagnostic> = list
                        .iter()
                        .map(|d| {
                            let from = mtty_lsp::pos_to_char(rope, d.range.start, encoding);
                            let to = mtty_lsp::pos_to_char(rope, d.range.end, encoding);
                            editor_pane::PaneDiagnostic {
                                from,
                                to: to.max(from),
                                severity: d.severity.clamp(1, 4),
                                message: match &d.source {
                                    Some(source) => format!("{} ({source})", d.message),
                                    None => d.message.clone(),
                                },
                            }
                        })
                        .collect();
                    diagnostics.sort_by_key(|d| (d.from, d.to, d.severity));
                    ed.diagnostics = diagnostics;
                }
            }
            Event::Hover { path, at, markdown } => {
                if let Some(popup) = self.hover.as_mut() {
                    if popup.path == path && popup.at == at {
                        popup.markdown = Some(markdown);
                    }
                }
            }
            Event::Completion {
                ticket,
                items,
                incomplete,
                encoding,
                ..
            } => {
                if let Some(c) = self.completion.as_mut().filter(|c| c.ticket == ticket) {
                    c.items = items;
                    c.incomplete = incomplete;
                    c.encoding = encoding;
                    c.waiting = false;
                    c.selected = 0;
                }
                self.refilter_completion();
            }
            Event::Definition { targets, encoding } => match targets.into_iter().next() {
                Some((path, range)) => self.go_to_target(&path, range, encoding),
                None => self.show_notice(
                    mtty_ui::i18n::t(self.lang, "No definition found.", "未找到定义。").to_string(),
                ),
            },
            Event::Failed {
                server,
                message,
                configured,
            } => {
                // A default server that is not installed stays quiet; one
                // the user configured says why it is not working.
                eprintln!("mtty: language server {server}: {message}");
                if configured {
                    self.show_notice(format!(
                        "{} {server}: {message}",
                        mtty_ui::i18n::t(self.lang, "Language server", "语言服务器")
                    ));
                }
            }
        }
        self.window.request_redraw();
    }

    /// Open `path` (a definition) and put the caret at `range`'s start.
    fn go_to_target(
        &mut self,
        path: &std::path::Path,
        range: mtty_lsp::LspRange,
        encoding: mtty_lsp::Encoding,
    ) {
        if !self.open_editor_pane(path) {
            return;
        }
        if let Some(ed) = self.active_editor_mut() {
            let at = mtty_lsp::pos_to_char(ed.doc.rope(), range.start, encoding);
            let line = ed.doc.rope().char_to_line(at);
            ed.go_to_line(line);
            ed.doc.set_selection(mtty_editor::Selection::cursor(at));
            ed.reveal_cursor();
        }
        self.completion = None;
        self.hover = None;
    }

    /// F12 / ⌘-click: the definition of the symbol at the caret.
    fn request_definition(&mut self) {
        self.lsp_sync_active();
        let lang = self.lang;
        let Some(ed) = active_editor_of(&self.tabs, self.active_tab) else {
            return;
        };
        // No language server for a remote file.
        if ed.remote.is_some() {
            return;
        }
        let (path, at) = (ed.path.clone(), ed.doc.selection().primary().head);
        let asked = self.lsp.definition(&path, ed.doc.rope(), at);
        if !asked {
            self.show_notice(no_server_notice(lang));
        }
    }

    /// Completions at the caret; `trigger` is the character typed that
    /// asked, if any. The list shows once the answer arrives.
    fn request_completion(&mut self, trigger: Option<String>, invoked: bool) {
        self.lsp_sync_active();
        let lang = self.lang;
        let Some(ed) = active_editor_of(&self.tabs, self.active_tab) else {
            return;
        };
        // No language server for a remote file.
        if ed.remote.is_some() {
            if invoked {
                self.show_notice(no_server_notice(lang));
            }
            return;
        }
        let rope = ed.doc.rope();
        let caret = ed.doc.selection().primary().head;
        let start = word_start(rope, caret);
        let (pane, path) = (ed.id.clone(), ed.path.clone());
        match self.lsp.completion(&path, rope, caret, trigger.as_deref()) {
            Some(ticket) => {
                // Keep showing the last list while a newer one is asked for.
                let keep = self
                    .completion
                    .take()
                    .filter(|c| c.pane == pane && c.start == start);
                self.completion = Some(match keep {
                    Some(mut c) => {
                        c.ticket = ticket;
                        c.waiting = true;
                        c
                    }
                    None => CompletionPopup {
                        pane,
                        start,
                        ticket,
                        items: Vec::new(),
                        encoding: Default::default(),
                        shown: Vec::new(),
                        selected: 0,
                        incomplete: false,
                        waiting: true,
                    },
                });
            }
            None if invoked => self.show_notice(no_server_notice(lang)),
            None => {}
        }
    }

    /// After typing in an editor: a trigger character asks for completions,
    /// a word character narrows the open list (or asks for one), anything
    /// else closes it.
    fn after_typing(&mut self, text: &str) {
        let Some(ed) = self.active_editor() else {
            return;
        };
        if ed.is_view_only() || ed.remote.is_some() || !self.lsp.handles(&ed.path) {
            return;
        }
        let rope = ed.doc.rope();
        let caret = ed.doc.selection().primary().head;
        let before: String = rope.slice(caret.saturating_sub(4)..caret).chars().collect();
        let pane = ed.id.clone();
        let trigger = self
            .lsp
            .trigger_characters(&ed.path)
            .into_iter()
            .filter(|t| !t.is_empty() && text.ends_with(t.chars().last().unwrap_or(' ')))
            .find(|t| before.ends_with(t.as_str()));
        if let Some(t) = trigger {
            self.request_completion(Some(t), false);
            return;
        }
        let word = text.chars().last().is_some_and(is_word_char);
        if !word {
            self.completion = None;
            return;
        }
        match &self.completion {
            Some(c) if c.pane == pane && !c.incomplete => self.refilter_completion(),
            _ => self.request_completion(None, false),
        }
    }

    /// Narrow the open completion list to what was typed since it opened;
    /// closed when the caret left the word.
    fn refilter_completion(&mut self) {
        let Some(c) = self.completion.as_ref() else {
            return;
        };
        let Some(ed) = self.tabs.get(self.active_tab).and_then(|t| {
            t.editors
                .iter()
                .find(|e| e.id == c.pane && t.active == e.id)
        }) else {
            self.completion = None;
            return;
        };
        let rope = ed.doc.rope();
        let caret = ed.doc.selection().primary().head;
        if caret < c.start || ed.doc.selection().len() > 1 {
            self.completion = None;
            return;
        }
        let typed: String = rope.slice(c.start..caret).chars().collect();
        if !typed.chars().all(is_word_char) {
            self.completion = None;
            return;
        }
        let shown = filter_completions(&c.items, &typed);
        let waiting = c.waiting;
        if shown.is_empty() && !waiting {
            self.completion = None;
            return;
        }
        if let Some(c) = self.completion.as_mut() {
            c.selected = c.selected.min(shown.len().saturating_sub(1));
            c.shown = shown;
        }
    }

    /// Enter / Tab / a click in the list: write the chosen completion (and
    /// its extra edits, such as an import) as one undo step.
    fn accept_completion(&mut self, index: Option<usize>) {
        let Some(c) = self.completion.take() else {
            return;
        };
        let Some(item) = c
            .shown
            .get(index.unwrap_or(c.selected))
            .and_then(|&i| c.items.get(i))
            .cloned()
        else {
            return;
        };
        let Some(ed) = self.active_editor_mut().filter(|e| e.id == c.pane) else {
            return;
        };
        let caret = ed.doc.selection().primary().head;
        let (tx, after) = completion_edit(ed.doc.rope(), caret, c.start, &item, c.encoding);
        ed.doc.apply(
            tx,
            mtty_editor::Selection::cursor(after),
            mtty_editor::history::EditKind::Other,
        );
        ed.reveal_cursor();
    }

    /// Keys for an open completion list: ↑ ↓ choose, ↩ / ⇥ accept, ⎋ close.
    /// True when the key was the list's.
    fn completion_key(&mut self, kind: input::KeyKind, mods: bool) -> bool {
        use input::KeyKind;
        let active = self.active_pane_id();
        let Some(c) = self.completion.as_mut() else {
            return false;
        };
        if active.as_deref() != Some(c.pane.as_str()) {
            self.completion = None;
            return false;
        }
        if c.shown.is_empty() {
            if kind == KeyKind::Escape {
                self.completion = None;
                return true;
            }
            return false;
        }
        let n = c.shown.len();
        match kind {
            KeyKind::Up if !mods => c.selected = (c.selected + n - 1) % n,
            KeyKind::Down if !mods => c.selected = (c.selected + 1) % n,
            KeyKind::PageUp if !mods => c.selected = c.selected.saturating_sub(COMPLETION_ROWS),
            KeyKind::PageDown if !mods => c.selected = (c.selected + COMPLETION_ROWS).min(n - 1),
            KeyKind::Enter | KeyKind::Tab if !mods => self.accept_completion(None),
            KeyKind::Escape => self.completion = None,
            _ => return false,
        }
        true
    }

    /// The pointer rested on editor text: show its diagnostics and ask the
    /// server about it.
    fn maybe_request_hover(&mut self) {
        let Some(rest) = self.hover_rest.as_mut() else {
            return;
        };
        if rest.asked || rest.since.elapsed() < HOVER_DELAY {
            return;
        }
        rest.asked = true;
        let (pane, at, pos) = (rest.pane.clone(), rest.at, rest.pos);
        self.show_hover(&pane, at, pos);
    }

    /// A hover popup at `pos` (points) for char `at` of editor `pane`.
    fn show_hover(&mut self, pane: &str, at: usize, pos: (f32, f32)) {
        self.lsp_sync_active();
        let Some(ed) = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.editors.iter().find(|e| e.id == pane))
        else {
            return;
        };
        let diagnostics: Vec<(u8, String)> = ed
            .diagnostics_at(at)
            .into_iter()
            .map(|d| (d.severity, d.message.clone()))
            .collect();
        let path = ed.path.clone();
        let asked =
            !ed.is_view_only() && ed.remote.is_none() && self.lsp.hover(&path, ed.doc.rope(), at);
        if diagnostics.is_empty() && !asked {
            return;
        }
        self.hover = Some(HoverPopup {
            pane: pane.to_string(),
            path,
            at,
            pos,
            markdown: None,
            diagnostics,
        });
        self.window.request_redraw();
    }

    /// Where an editor caret is on screen (points, below its cell), for
    /// popups opened from the keyboard.
    fn caret_point(&self) -> Option<(f32, f32)> {
        let ed = self.active_editor()?;
        let (row, col) = ed.caret_cell()?;
        let inner = self.active_inner()?;
        Some((
            inner.x + col as f32 * self.cw,
            inner.y + (row + 1) as f32 * self.ch,
        ))
    }

    /// The hover popup and the completion list.
    fn lsp_popups(&mut self, ctx: &egui::Context) {
        let active = self.active_pane_id();
        if self
            .hover
            .as_ref()
            .is_some_and(|h| Some(&h.pane) != active.as_ref())
        {
            self.hover = None;
        }
        let ch = self.theme.chrome();
        let fg = mtty_ui::chrome::bg_color(ch.text);
        let panel = mtty_ui::chrome::bg_color(ch.card);
        if let Some(h) = &self.hover {
            if h.markdown.is_some() || !h.diagnostics.is_empty() {
                let (x, y) = h.pos;
                let text = h.markdown.clone();
                let diagnostics = h.diagnostics.clone();
                let base = h.path.parent().map(std::path::Path::to_path_buf);
                egui::Area::new(egui::Id::new("mtty-hover"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(egui::pos2(x, y + 4.0))
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_max_width(560.0);
                            egui::ScrollArea::vertical()
                                .max_height(320.0)
                                .show(ui, |ui| {
                                    for (severity, message) in &diagnostics {
                                        ui.label(
                                            egui::RichText::new(message)
                                                .color(severity_color32(*severity)),
                                        );
                                    }
                                    if let Some(text) = &text {
                                        if !diagnostics.is_empty() {
                                            ui.separator();
                                        }
                                        render_markdown(
                                            ui,
                                            text,
                                            base.as_deref(),
                                            &mut self.cmark,
                                            &mut self.mmd,
                                            fg,
                                            panel,
                                        );
                                    }
                                });
                        });
                    });
            }
        }
        let Some(c) = &self.completion else {
            return;
        };
        if c.shown.is_empty() || Some(&c.pane) != active.as_ref() {
            return;
        }
        let Some((x, y)) = self.caret_point() else {
            return;
        };
        let rows: Vec<(usize, String, Option<String>, u8)> = c
            .shown
            .iter()
            .enumerate()
            .skip(c.selected.saturating_sub(COMPLETION_ROWS - 1))
            .take(COMPLETION_ROWS)
            .map(|(row, &i)| {
                let item = &c.items[i];
                (row, item.label.clone(), item.detail.clone(), item.kind)
            })
            .collect();
        let (selected, total) = (c.selected, c.shown.len());
        let mut clicked = None;
        let muted = mtty_ui::chrome::bg_color(ch.muted);
        egui::Area::new(egui::Id::new("mtty-completion"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(x, y + 2.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(260.0);
                    ui.set_max_width(520.0);
                    for (row, label, detail, kind) in rows {
                        let text = egui::RichText::new(format!(
                            "{}  {label}",
                            completion_kind_letter(kind)
                        ))
                        .monospace();
                        let r = ui.horizontal(|ui| {
                            let r = ui.selectable_label(row == selected, text);
                            if let Some(detail) = detail {
                                ui.label(egui::RichText::new(detail).small().color(muted));
                            }
                            r
                        });
                        if r.inner.clicked() {
                            clicked = Some(row);
                        }
                    }
                    if total > COMPLETION_ROWS {
                        ui.label(
                            egui::RichText::new(format!("{} / {total}", selected + 1))
                                .small()
                                .color(muted),
                        );
                    }
                });
            });
        if let Some(row) = clicked {
            self.accept_completion(Some(row));
        }
    }

    /// Go to Line (⌃G / Ctrl+G): `line` or `line:column`, 1-based, in the
    /// active editor pane.
    fn goto_line_window(&mut self, ctx: &egui::Context) {
        let Some(text) = self.goto_line.as_mut() else {
            return;
        };
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let Some((total, (line, _))) = self
            .tabs
            .get(self.active_tab)
            .and_then(|tab| tab.editors.iter().find(|e| e.id == tab.active))
            .map(|e| (e.total_lines(), e.caret_line_col()))
        else {
            self.goto_line = None;
            return;
        };
        let target = editor_pane::parse_line_target(text);
        let (mut go, mut close) = (false, false);
        egui::Window::new(t(lang, "Go to Line", "跳转到行"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 40.0])
            .show(ctx, |ui| {
                let hint = if lang == mtty_ui::i18n::Lang::En {
                    format!("line[:column] — now {line} of {total}")
                } else {
                    format!("行[:列] —— 当前第 {line} 行,共 {total} 行")
                };
                let r = ui.add(
                    egui::TextEdit::singleline(text)
                        .hint_text(hint)
                        .desired_width(280.0),
                );
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if !enter {
                    r.request_focus();
                }
                go = enter;
                if !text.trim().is_empty() && target.is_none() {
                    ui.label(
                        egui::RichText::new(t(
                            lang,
                            "Type a line number, or line:column.",
                            "请输入行号,或 行:列。",
                        ))
                        .color(chrome_rgb(self.theme.chrome().negative)),
                    );
                }
                close = ui.input(|i| i.key_pressed(egui::Key::Escape));
            });
        if go {
            if let Some((line, col)) = target {
                if let Some(ed) = self.active_editor_mut() {
                    ed.go_to_line_col(line, col);
                }
                close = true;
            }
        }
        if close {
            self.goto_line = None;
            self.window.request_redraw();
        }
    }

    /// Go to Symbol (⌘R / Ctrl+R): a filterable outline of the active editor.
    fn go_to_symbol_window(&mut self, ctx: &egui::Context) {
        if self.goto_symbol.is_none() {
            return;
        }
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let symbols = match self.active_editor() {
            Some(ed) => ed
                .syntax
                .as_ref()
                .map(|s| s.outline(ed.doc.rope()))
                .unwrap_or_default(),
            None => {
                self.goto_symbol = None;
                return;
            }
        };
        let (query, selected) = self.goto_symbol.clone().unwrap_or_default();
        let needle = query.trim().to_lowercase();
        let shown: Vec<usize> = symbols
            .iter()
            .enumerate()
            .filter(|(_, s)| needle.is_empty() || s.name.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        let selected = selected.min(shown.len().saturating_sub(1));
        let mut new_text = query.clone();
        let mut chosen: Option<usize> = None;
        let mut move_up = false;
        let mut move_down = false;
        let mut cancel = false;
        egui::Window::new(t(lang, "Go to Symbol", "转到符号"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 40.0])
            .show(ctx, |ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut new_text)
                        .hint_text(t(lang, "Type to filter symbols…", "输入以过滤符号…"))
                        .desired_width(380.0),
                );
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter {
                    chosen = shown.get(selected).copied();
                } else {
                    r.request_focus();
                }
                move_up = ui.input(|i| i.key_pressed(egui::Key::ArrowUp));
                move_down = ui.input(|i| i.key_pressed(egui::Key::ArrowDown));
                cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
                ui.separator();
                if symbols.is_empty() {
                    ui.label(t(
                        lang,
                        "No outline for this file (no syntax tree).",
                        "此文件没有大纲(无语法树)。",
                    ));
                }
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for (row, &i) in shown.iter().enumerate() {
                            let s = &symbols[i];
                            let label = format!("{}{}", "  ".repeat(s.depth), s.name);
                            let r = ui.selectable_label(row == selected, label);
                            if r.clicked() {
                                chosen = Some(i);
                            }
                        }
                    });
            });
        if let Some(i) = chosen {
            if let Some(ed) = self.active_editor_mut() {
                ed.go_to_line(symbols[i].line);
            }
            self.goto_symbol = None;
            self.window.request_redraw();
            return;
        }
        if cancel {
            self.goto_symbol = None;
            self.window.request_redraw();
            return;
        }
        let selected = if new_text != query {
            0
        } else if move_down {
            (selected + 1).min(shown.len().saturating_sub(1))
        } else if move_up {
            selected.saturating_sub(1)
        } else {
            selected
        };
        self.goto_symbol = Some((new_text, selected));
    }

    /// Resume Agent Session… (ADR 0042, A4): pick a session an agent reported
    /// and relaunch it in its recorded directory.
    fn resume_picker_window(&mut self, ctx: &egui::Context) {
        let Some((query, selected)) = self.resume_picker.clone() else {
            return;
        };
        use mtty_ui::i18n::t;
        let lang = self.lang;
        // Newest first, from the states agents reported over MTP.
        let sessions = self.mtp.agent_sessions();
        let field = |v: &serde_json::Value, k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let rows: Vec<(String, String, String, String, f64)> = sessions
            .iter()
            .map(|s| {
                (
                    field(s, "agent"),
                    field(s, "session_id"),
                    field(s, "cwd"),
                    field(s, "pane"),
                    s.get("ts")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.0),
                )
            })
            .collect();
        let needle = query.trim().to_lowercase();
        let shown: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                needle.is_empty()
                    || r.0.to_lowercase().contains(&needle)
                    || r.1.to_lowercase().contains(&needle)
                    || r.2.to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect();
        let selected = selected.min(shown.len().saturating_sub(1));
        let mut new_text = query.clone();
        let mut chosen: Option<usize> = None;
        let mut move_up = false;
        let mut move_down = false;
        let mut cancel = false;
        egui::Window::new(t(lang, "Resume Agent Session", "恢复 Agent 会话"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 40.0])
            .show(ctx, |ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut new_text)
                        .hint_text(t(lang, "Type to filter sessions…", "输入以过滤会话…"))
                        .desired_width(420.0),
                );
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter {
                    chosen = shown.get(selected).copied();
                } else {
                    r.request_focus();
                }
                move_up = ui.input(|i| i.key_pressed(egui::Key::ArrowUp));
                move_down = ui.input(|i| i.key_pressed(egui::Key::ArrowDown));
                cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
                ui.separator();
                if rows.is_empty() {
                    ui.label(t(
                        lang,
                        "No agent has reported a session id yet.",
                        "还没有 agent 上报会话 id。",
                    ));
                }
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for (row, &i) in shown.iter().enumerate() {
                            let (agent, session, cwd, pane, ts) = &rows[i];
                            let mut label = if cwd.is_empty() {
                                format!("{agent}  {session}  ({pane})")
                            } else {
                                format!("{agent}  {session}  {cwd}")
                            };
                            let age = ago(*ts);
                            if !age.is_empty() {
                                label.push_str(&format!("  ·  {age}"));
                            }
                            let r = ui.selectable_label(row == selected, label);
                            if r.clicked() {
                                chosen = Some(i);
                            }
                        }
                    });
            });
        if let Some(i) = chosen {
            let (agent, session, cwd, _, _) = rows[i].clone();
            self.resume_picker = None;
            self.resume_agent(&agent, &session, Some(cwd.as_str()));
            self.window.request_redraw();
            return;
        }
        if cancel {
            self.resume_picker = None;
            self.window.request_redraw();
            return;
        }
        let selected = if new_text != query {
            0
        } else if move_down {
            (selected + 1).min(shown.len().saturating_sub(1))
        } else if move_up {
            selected.saturating_sub(1)
        } else {
            selected
        };
        self.resume_picker = Some((new_text, selected));
    }

    /// The vim `:` command line (see [`Self::run_vim_command`]).
    fn vim_command_window(&mut self, ctx: &egui::Context) {
        let Some(text) = self.vim_command.as_mut() else {
            return;
        };
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let (mut go, mut close) = (false, false);
        egui::Window::new(t(lang, "Vim Command", "Vim 命令"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 40.0])
            .show(ctx, |ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(text)
                        .hint_text(t(lang, "w, q, wq, or a line number", "w、q、wq 或行号"))
                        .desired_width(280.0),
                );
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if !enter {
                    r.request_focus();
                }
                go = enter;
                close = ui.input(|i| i.key_pressed(egui::Key::Escape));
            });
        let command = text.trim().to_string();
        if close {
            self.vim_command = None;
            self.window.request_redraw();
            return;
        }
        if !go {
            return;
        }
        self.vim_command = None;
        self.run_vim_command(&command);
        self.window.request_redraw();
    }

    /// Run a `:` command: `w` writes, `q`/`q!` close the pane, `wq` both, a
    /// number goes to that line.
    fn run_vim_command(&mut self, command: &str) {
        let command = command.trim();
        if command.is_empty() {
            return;
        }
        if let Ok(line) = command.parse::<usize>() {
            if line > 0 {
                if let Some(ed) = self.active_editor_mut() {
                    ed.go_to_line(line - 1);
                }
            }
            return;
        }
        let (save, quit) = match command {
            "w" => (true, false),
            "q" | "q!" => (false, true),
            "wq" | "x" => (true, true),
            _ => (false, false),
        };
        if save {
            // A remote `:wq` closes once the write lands, not before.
            if quit && self.active_editor().is_some_and(|ed| ed.remote.is_some()) {
                if let Some(ed) = self.active_editor_mut() {
                    ed.quit_after_save = true;
                }
                self.save_active_editor();
                return;
            }
            self.save_active_editor();
        }
        if quit {
            self.run_command(Cmd::ClosePane);
        }
    }

    fn search_window(&mut self, ctx: &egui::Context) {
        if self.search.is_none() {
            return;
        }
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let n = self.search_count();
        let idx = self.search_idx;
        // Still scanning: the count may grow.
        let more = self
            .bg_search
            .as_ref()
            .is_some_and(|b| !b.done.load(std::sync::atomic::Ordering::Acquire));
        // Options and replace apply to a document in memory; a file in view
        // mode is searched as literal text and cannot be changed.
        let editor = self.active_editor().map(|e| e.is_view_only());
        let can_replace = editor == Some(false) && !self.read_only;
        let error = self.find_error.clone();
        let Some(query) = self.search.as_mut() else {
            return;
        };
        let opts = &mut self.find_opts;
        let mut step = 0i32;
        let mut close = false;
        let (mut select_all, mut replace_one, mut replace_all) = (false, false, false);
        let replace_id = egui::Id::new("mtty-find-replace");
        egui::Window::new(t(lang, "Find", "查找"))
            .collapsible(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 40.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(query)
                            .hint_text(t(lang, "search…", "搜索…"))
                            .desired_width(260.0),
                    );
                    // Check Enter before re-taking focus: Enter makes the field give it up,
                    // and taking it back first would hide that (`lost_focus` stays false).
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let replacing = ui.memory(|m| m.has_focus(replace_id));
                    if !enter && !replacing {
                        r.request_focus();
                    }
                    if enter {
                        let m = ui.input(|i| i.modifiers);
                        if m.alt && editor == Some(false) {
                            select_all = true;
                        } else {
                            step = if m.shift { -1 } else { 1 };
                        }
                    }
                    if editor == Some(false) {
                        let toggles = [
                            (
                                &mut opts.case_sensitive,
                                "Aa",
                                t(lang, "Match Case", "区分大小写"),
                            ),
                            (
                                &mut opts.whole_word,
                                "ab",
                                t(lang, "Whole Word", "全字匹配"),
                            ),
                            (
                                &mut opts.regex,
                                ".*",
                                t(lang, "Regular Expression", "正则表达式"),
                            ),
                        ];
                        for (on, label, tip) in toggles {
                            let text = egui::RichText::new(label).monospace();
                            if ui.selectable_label(*on, text).on_hover_text(tip).clicked() {
                                *on = !*on;
                            }
                        }
                    }
                    let more = if more { "\u{2026}" } else { "" };
                    ui.label(format!("{} / {n}{more}", if n == 0 { 0 } else { idx + 1 }));
                });
                if let Some(why) = &error {
                    ui.label(
                        egui::RichText::new(why.lines().last().unwrap_or(why))
                            .color(chrome_rgb(self.theme.chrome().negative)),
                    );
                }
                if let (Some(replace), true) = (opts.replace.as_mut(), can_replace) {
                    ui.horizontal(|ui| {
                        let hint = if opts.regex {
                            t(lang, "replace ($1 for groups)…", "替换为($1 引用分组)…")
                        } else {
                            t(lang, "replace…", "替换为…")
                        };
                        let r = ui.add(
                            egui::TextEdit::singleline(replace)
                                .id(replace_id)
                                .hint_text(hint)
                                .desired_width(260.0),
                        );
                        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            let m = ui.input(|i| i.modifiers);
                            if m.command && m.alt {
                                replace_all = true;
                            } else {
                                replace_one = true;
                            }
                            r.request_focus();
                        }
                        replace_one |= ui.button(t(lang, "Replace", "替换")).clicked();
                        replace_all |= ui.button(t(lang, "Replace All", "全部替换")).clicked();
                    });
                }
                if editor == Some(false) && n > 0 {
                    let label = t(lang, "Select All Matches", "选中全部匹配");
                    let tip = if cfg!(target_os = "macos") {
                        "\u{2325}\u{21a9}"
                    } else {
                        "Alt+Enter"
                    };
                    select_all |= ui.small_button(label).on_hover_text(tip).clicked();
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            });
        if replace_one {
            self.replace_current_match();
        }
        if replace_all {
            self.replace_all_matches();
        }
        if select_all {
            self.select_all_matches();
            close = true;
        }
        if close {
            self.close_search();
            return;
        }
        if step != 0 && n > 0 {
            self.search_idx = ((idx as i32 + step).rem_euclid(n as i32)) as usize;
            self.scroll_to_search_hit();
            self.window.request_redraw();
        }
    }

    fn close_search(&mut self) {
        self.search = None;
        self.search_hits.clear();
        self.search_key.clear();
        self.editor_hits.clear();
        self.editor_hits_query.clear();
        self.find_error = None;
        self.find_opts.replace = None;
        self.bg_search = None;
        self.window.request_redraw();
    }

    /// Replace: the current match in the active editor, then on to the next
    /// one after it (found again once the text has changed).
    fn replace_current_match(&mut self) {
        let Some(replacement) = self.find_opts.replace.clone() else {
            return;
        };
        let Some((start, end)) = self.editor_hits.get(self.search_idx).copied() else {
            return;
        };
        let query = self.editor_query();
        let lang = self.lang;
        let Some(ed) = self.active_editor_mut().filter(|e| !e.is_view_only()) else {
            return;
        };
        match ed.doc.replace_match(&query, start, end, &replacement) {
            Ok(_) => {
                // A stale match is skipped the same way: on to the next one.
                ed.reveal_cursor();
                self.find_rejump = true;
            }
            Err(e) => self.show_notice(format!("{}: {e}", t_replace_failed(lang))),
        }
        self.window.request_redraw();
    }

    /// Replace All in the active editor: one undo step.
    fn replace_all_matches(&mut self) {
        let Some(replacement) = self.find_opts.replace.clone() else {
            return;
        };
        let query = self.editor_query();
        let lang = self.lang;
        let Some(ed) = self.active_editor_mut().filter(|e| !e.is_view_only()) else {
            return;
        };
        let msg = match ed.doc.replace_all(&query, &replacement) {
            Ok(n) => {
                ed.reveal_cursor();
                if lang == mtty_ui::i18n::Lang::En {
                    format!("Replaced {n} {}.", if n == 1 { "match" } else { "matches" })
                } else {
                    format!("已替换 {n} 处。")
                }
            }
            Err(e) => format!("{}: {e}", t_replace_failed(lang)),
        };
        self.show_notice(msg);
    }

    /// Select All Matches: every Find match in the editor becomes a selection.
    fn select_all_matches(&mut self) {
        let in_bytes = self.bg_search.as_ref().is_some_and(|b| b.bytes);
        if in_bytes || self.editor_hits.is_empty() {
            return;
        }
        let hits = std::mem::take(&mut self.editor_hits);
        if let Some(ed) = self.active_editor_mut() {
            let caret = ed.doc.selection().primary().from();
            ed.doc.select_ranges(&hits, caret);
            ed.reveal_cursor();
        }
        self.editor_hits = hits;
    }

    fn composer_window(&mut self, ctx: &egui::Context) {
        let target = self.active_pane().map(|p| p.id.clone()).unwrap_or_default();
        let Some(text) = self.composer.as_mut() else {
            return;
        };
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let mut open = true;
        let mut send = false;
        let mut queue = false;
        app_window(t(lang, "Composer", "Composer"), ctx)
            .open(&mut open)
            .default_size([520.0, 260.0])
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(format!("to {target}"))
                        .small()
                        .color(chrome_rgb(self.theme.chrome().muted)),
                );
                ui.add(
                    egui::TextEdit::multiline(text)
                        .hint_text(t(lang, "Prompt…", "提示词…"))
                        .desired_width(ui.available_width())
                        .desired_rows(8),
                );
                ui.horizontal(|ui| {
                    let ready = !text.trim().is_empty();
                    if ui
                        .add_enabled(ready, egui::Button::new(t(lang, "Send", "发送")))
                        .clicked()
                    {
                        send = true;
                    }
                    if ui
                        .add_enabled(ready, egui::Button::new(t(lang, "Queue it", "加入队列")))
                        .clicked()
                    {
                        queue = true;
                    }
                });
            });
        let draft = text.clone();
        if send {
            self.composer = None;
            self.write_input(format!("{draft}\r").as_bytes());
        } else if queue {
            self.prompt_queue.push(draft, self.active_pane_id());
            self.save_queue();
            self.composer = None;
        } else if !open {
            self.composer = None;
        }
    }

    fn check_updates(&mut self) {
        self.check_updates_inner(true);
    }

    /// A silent check: no dialog and no "up to date" toast, used on startup.
    fn check_updates_silent(&mut self) {
        self.check_updates_inner(false);
    }

    fn check_updates_inner(&mut self, open_dialog: bool) {
        // Reopening an in-flight check must not launch a second request.
        if open_dialog {
            self.update_dialog = true;
            self.update_notice_until = None;
        }
        if self.update_rx.is_some() {
            return;
        }
        self.update_result = None;
        let Some(url) = self.update_url.clone() else {
            self.update_result = Some(UpdateResult::Failed(
                mtty_ui::i18n::t(self.lang, "No update URL configured.", "未配置更新地址。")
                    .to_string(),
            ));
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let out = mtty_platform::background_command("curl")
                .args(["-fsSL", "--max-time", "8", &url])
                .output();
            let result = match out {
                Ok(o) if o.status.success() => {
                    match mtty_ui::update::parse_checked(&String::from_utf8_lossy(&o.stdout)) {
                        Ok(m) => {
                            if mtty_ui::update::is_newer(&m.version, env!("CARGO_PKG_VERSION")) {
                                let artifact = m.for_platform().cloned();
                                UpdateResult::Available {
                                    version: m.version,
                                    artifact,
                                }
                            } else {
                                UpdateResult::Current
                            }
                        }
                        Err(error) => UpdateResult::Failed(error.to_string()),
                    }
                }
                Ok(_) => UpdateResult::Failed("Could not fetch the update manifest.".to_string()),
                Err(error) => UpdateResult::Failed(error.to_string()),
            };
            let _ = tx.send(result);
            // An unfocused window can be waiting without redraws or PTY
            // output. Deliver the result even while its shell is idle.
            let _ = proxy.send_event(HostEvent::Wake);
        });
        self.update_rx = Some(rx);
    }

    /// A restored ssh tab: say it is disconnected and let Enter reconnect,
    /// or, with `ssh-auto-reconnect`, connect on startup. Either way the
    /// command runs in the pane's shell, so ssh still verifies host keys.
    fn offer_reconnect(&self, tab: &mut Tab) {
        let cmd = tab.ssh_cmd.clone().or_else(|| {
            tab.ssh_target
                .as_deref()
                .and_then(mtty_ui::ssh::session_command)
                .map(|(_, cmd)| cmd)
        });
        let Some(cmd) = cmd else {
            return;
        };
        let target = tab.ssh_target.clone().unwrap_or_else(|| tab.title.clone());
        let active = tab.active.clone();
        let Some(pane) = tab.panes.iter_mut().find(|p| p.id == active) else {
            return;
        };
        let command = typed_ssh(&cmd);
        match restored_ssh_action(self.ssh_auto_reconnect) {
            RestoredSsh::Auto => {
                pane.scroll = 0;
                pane.term.write(command.as_bytes());
            }
            RestoredSsh::OnEnter => {
                let note = format!(
                    "\x1b[2m[mtty] {} {target}. {}\x1b[0m\r\n",
                    mtty_ui::i18n::t(self.lang, "Disconnected from", "已与以下主机断开:"),
                    mtty_ui::i18n::t(self.lang, "Press Enter to reconnect.", "按回车重新连接。"),
                );
                pane.term.screen_mut().process(note.as_bytes());
                pane.on_enter = Some(command);
            }
        }
    }

    fn open_ssh(&mut self, input: &str) {
        let Some((title, cmd)) = mtty_ui::ssh::session_command(input) else {
            return;
        };
        self.open_ssh_command(title, cmd, input.trim().to_string());
    }

    fn reload_snippets(&mut self) {
        match mtty_config::snippets::SnippetBook::load() {
            Ok(book) => {
                self.snippet_book = book;
                self.snippet_book_error = None;
            }
            Err(e) => {
                self.show_notice(e.clone());
                self.snippet_book_error = Some(e);
            }
        }
    }

    fn save_snippets(&mut self) -> bool {
        let result = match self.snippet_book_error.clone() {
            Some(e) => Err(e),
            None => self.snippet_book.save(),
        };
        if let Err(e) = result {
            let msg = format!(
                "{}: {e}",
                t_lang(self.lang, "Snippets not saved", "片段未保存")
            );
            self.show_notice(msg);
            return false;
        }
        self.sync_soon();
        true
    }

    /// Type a snippet into the active pane (with Enter), like a typed command.
    fn run_snippet_here(&mut self, command: &str) {
        self.write_input(format!("{command}\r").as_bytes());
    }

    fn snippets_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let Some(mut view) = self.snippets_view.take() else {
            return;
        };
        let mut open = true;
        let mut run_here: Option<usize> = None;
        let mut run_hosts: Option<usize> = None;
        let mut delete: Option<usize> = None;
        let mut add = false;
        let host_names: Vec<String> = self
            .host_book
            .sorted()
            .into_iter()
            .map(|(_, h)| h.name.clone())
            .collect();
        app_window(t(lang, "Snippets", "命令片段"), ctx)
            .open(&mut open)
            .default_size([600.0, 460.0])
            .show(ctx, |ui| {
                window_body(ui, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut view.filter)
                            .hint_text(t(lang, "Search snippets…", "搜索片段…"))
                            .desired_width(280.0),
                    );
                    if self.snippet_book.snippets.is_empty() {
                        ui.label(t(
                            lang,
                            "No snippets yet. Add one below.",
                            "还没有片段,可在下方添加。",
                        ));
                    }
                    for (i, sn) in self.snippet_book.snippets.iter().enumerate() {
                        if !sn.matches(&view.filter) {
                            continue;
                        }
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&sn.name).strong());
                            if !sn.tags.is_empty() {
                                ui.label(egui::RichText::new(sn.tags.join(", ")).size(11.0));
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if view.confirm_delete == Some(i) {
                                        if ui.button(t(lang, "Cancel", "取消")).clicked() {
                                            view.confirm_delete = None;
                                        }
                                        if ui
                                            .button(t(lang, "Confirm delete", "确认删除"))
                                            .clicked()
                                        {
                                            delete = Some(i);
                                        }
                                    } else {
                                        if ui.button(t(lang, "Delete…", "删除…")).clicked() {
                                            view.confirm_delete = Some(i);
                                        }
                                        if ui
                                            .add_enabled(
                                                !view.hosts.is_empty(),
                                                egui::Button::new(t(
                                                    lang,
                                                    "Run on hosts",
                                                    "在所选主机上运行",
                                                )),
                                            )
                                            .clicked()
                                        {
                                            run_hosts = Some(i);
                                        }
                                        if ui.button(t(lang, "Run here", "在此运行")).clicked()
                                        {
                                            run_here = Some(i);
                                        }
                                    }
                                },
                            );
                        });
                        ui.label(egui::RichText::new(&sn.command).monospace().size(12.0));
                    }
                    ui.separator();
                    if !host_names.is_empty() {
                        ui.label(t(
                            lang,
                            "Hosts for \"Run on hosts\":",
                            "\"在所选主机上运行\"的目标主机:",
                        ));
                        ui.horizontal_wrapped(|ui| {
                            for name in &host_names {
                                let mut on = view.hosts.contains(name);
                                if ui.checkbox(&mut on, name).changed() {
                                    if on {
                                        view.hosts.insert(name.clone());
                                    } else {
                                        view.hosts.remove(name);
                                    }
                                }
                            }
                        });
                        ui.separator();
                    }
                    ui.collapsing(t(lang, "Add snippet", "添加片段"), |ui| {
                        ui.horizontal(|ui| {
                            ui.label(t(lang, "Name", "名称"));
                            ui.text_edit_singleline(&mut view.form.name);
                        });
                        ui.label(t(lang, "Command", "命令"));
                        ui.add(
                            egui::TextEdit::multiline(&mut view.form.command)
                                .code_editor()
                                .desired_rows(3)
                                .desired_width(ui.available_width()),
                        );
                        ui.horizontal(|ui| {
                            ui.label(t(lang, "Tags", "标签"));
                            ui.add(
                                egui::TextEdit::singleline(&mut view.form_tags)
                                    .hint_text("ops, db"),
                            );
                        });
                        let ok = !view.form.name.trim().is_empty()
                            && !view.form.command.trim().is_empty();
                        if ui
                            .add_enabled(ok, egui::Button::new(t(lang, "Save snippet", "保存片段")))
                            .clicked()
                        {
                            add = true;
                        }
                    });
                });
            });
        if let Some(i) = run_here.and_then(|i| self.snippet_book.snippets.get(i).cloned()) {
            self.run_snippet_here(&i.command);
        }
        if let Some(sn) = run_hosts.and_then(|i| self.snippet_book.snippets.get(i).cloned()) {
            let hosts: Vec<_> = self
                .host_book
                .hosts
                .iter()
                .filter(|h| view.hosts.contains(&h.name))
                .cloned()
                .collect();
            for host in hosts {
                let cmd = remote_run_command(&host.destination(), &host.ssh_options(), &sn.command);
                self.run_in_new_tab(&format!("{} @ {}", sn.name, host.name), &cmd);
            }
        }
        if let Some(i) = delete {
            if i < self.snippet_book.snippets.len() {
                let removed = self.snippet_book.snippets.remove(i);
                if !self.save_snippets() {
                    self.snippet_book.snippets.insert(i, removed);
                }
            }
            view.confirm_delete = None;
        }
        if add {
            let mut sn = std::mem::take(&mut view.form);
            sn.name = sn.name.trim().to_string();
            sn.command = sn.command.trim().to_string();
            sn.tags = view
                .form_tags
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            view.form_tags.clear();
            self.snippet_book.upsert(sn);
            self.save_snippets();
        }
        if open {
            self.snippets_view = Some(view);
        }
    }

    fn start_update_download(&mut self, artifact: mtty_ui::update::Artifact) {
        if self.update_install == UpdateInstall::Working {
            return;
        }
        self.update_install = UpdateInstall::Working;
        let key = self.update_pubkey.clone();
        self.spawn_job(move || {
            let dir = std::env::temp_dir().join(format!("mtty-download-{}", std::process::id()));
            JobDone::UpdateDownloaded(mtty_ui::update::download_verified(&artifact, &key, &dir))
        });
    }

    /// The next step of *Update and Relaunch*; it stops at anything that
    /// needs the user (no update, no signed download, a failure).
    fn continue_auto_update(&mut self) {
        let signed = match &self.update_result {
            Some(UpdateResult::Available {
                artifact: Some(a), ..
            }) if a.signature.is_some() => Some(a.clone()),
            _ => None,
        };
        match (self.update_install.clone(), signed) {
            (UpdateInstall::Ready(path), _) => {
                self.update_auto = false;
                self.install_update(&path);
            }
            (UpdateInstall::Working, _) => {}
            (UpdateInstall::Idle, Some(artifact)) => self.start_update_download(artifact),
            _ => self.update_auto = false,
        }
    }

    /// Hand a verified update to the platform helper and quit, or open the
    /// download where this install cannot replace itself.
    fn install_update(&mut self, path: &std::path::Path) {
        match mtty_ui::install::prepare(path) {
            Ok(mtty_ui::install::Plan::Helper(script)) => {
                match mtty_ui::install::launch(&script) {
                    // Hosted programs keep running through the update.
                    Ok(()) => self.quit_keeping_sessions(),
                    Err(e) => self.update_install = UpdateInstall::Failed(e.to_string()),
                }
            }
            Ok(mtty_ui::install::Plan::OpenDownload(reason)) => {
                self.show_notice(format!("{reason}: {}", path.display()));
                open_external(&path.to_string_lossy());
            }
            Err(e) => self.update_install = UpdateInstall::Failed(e),
        }
    }

    /// A file dropped on the window (X11 through winit, Wayland through
    /// `wayland_dnd`), at the pointer position in `self.cursor`.
    fn drop_file(&mut self, path: std::path::PathBuf) {
        // The first file of a drop decides for all of them.
        let first = std::mem::take(&mut self.dropping) || self.drop_choice.is_none();
        // Onto the SFTP window: upload to its remote folder.
        let scale = self.window.scale_factor() as f32;
        let at = egui::pos2(self.cursor.0 as f32 / scale, self.cursor.1 as f32 / scale);
        if self.sftp_view.as_ref().is_some_and(|v| v.rect.contains(at)) {
            self.sftp_upload(vec![path]);
            self.window.request_redraw();
            return;
        }
        let scale = self.window.scale_factor() as f32;
        let (px, py) = (self.cursor.0 as f32, self.cursor.1 as f32);
        // Onto the session list: a folder opens a new terminal there, a file
        // opens in its own tab (the editor, or view mode when large).
        if self.show_sidebar && px / scale < self.sidebar_w {
            if path.is_dir() {
                self.new_tab_in(Some(path));
            } else {
                self.open_editor(path);
            }
            self.window.request_redraw();
            return;
        }
        if first {
            self.drop_choice = Some(self.drop_target(px / scale, py / scale));
        }
        let Some((action, pane)) = self.drop_choice.clone() else {
            return;
        };
        match action {
            drag::DropAction::InsertPath => {
                if let Some(id) = pane {
                    if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                        tab.active = id;
                    }
                }
                // Into the terminal: the shell-quoted path.
                self.paste(&format!("{} ", local_path_arg(&path.to_string_lossy())));
            }
            // A folder never reaches the editor (`read_to_string` fails with
            // EISDIR): it opens a terminal there.
            drag::DropAction::Open if path.is_dir() => self.new_tab_in(Some(path)),
            drag::DropAction::Open => {
                self.open_editor(path);
            }
        }
        self.window.request_redraw();
    }

    /// While files are dragged over the window, show what a drop does where
    /// the pointer is (`drag`): over a terminal, its "insert path" area and
    /// the "open" band along its bottom, the one under the pointer lit;
    /// elsewhere the whole window opens. The platform only shows a generic
    /// drag cursor.
    fn draw_drop_targets(&self, ctx: &egui::Context) {
        let ch = self.theme.chrome();
        let accent = egui::Color32::from_rgb(ch.accent.0, ch.accent.1, ch.accent.2);
        let text = chrome::fg_color(&self.theme);
        let scale = self.window.scale_factor() as f32;
        let (x, y) = (self.cursor.0 as f32 / scale, self.cursor.1 as f32 / scale);
        let (action, _) = self.drop_target(x, y);
        let (insert, open) = drag::labels(self.lang, &self.drag_paths);
        let hint = drag::hint(self.lang);
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("file_drop"),
        ));
        let zone = |rect: egui::Rect, label: &str, lit: bool, hint: Option<&str>| {
            let alpha = if lit { 56 } else { 14 };
            painter.rect_filled(
                rect.shrink(2.0),
                egui::Rounding::same(6.0),
                egui::Color32::from_rgba_unmultiplied(ch.accent.0, ch.accent.1, ch.accent.2, alpha),
            );
            painter.rect_stroke(
                rect.shrink(2.0),
                egui::Rounding::same(6.0),
                egui::Stroke::new(
                    if lit { 2.0_f32 } else { 1.0 },
                    accent.gamma_multiply(if lit { 1.0 } else { 0.5 }),
                ),
            );
            let color = if lit {
                accent
            } else {
                text.gamma_multiply(0.6)
            };
            let size = if lit { 16.0 } else { 14.0 };
            let at = rect.center() - egui::vec2(0.0, if hint.is_some() { 9.0 } else { 0.0 });
            painter.text(
                at,
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(size),
                color,
            );
            if let Some(hint) = hint {
                painter.text(
                    at + egui::vec2(0.0, 20.0),
                    egui::Align2::CENTER_CENTER,
                    hint,
                    egui::FontId::proportional(12.0),
                    text.gamma_multiply(0.7),
                );
            }
        };
        let to_egui =
            |r: Rect| egui::Rect::from_min_size(egui::pos2(r.x, r.y), egui::vec2(r.w, r.h));
        match self.terminal_at(x, y) {
            Some((_, r)) if self.drag_live => {
                let band = drag::open_band_top(r);
                let top = Rect { h: band - r.y, ..r };
                let bottom = Rect {
                    y: band,
                    h: r.y + r.h - band,
                    ..r
                };
                let insert_lit = action == drag::DropAction::InsertPath;
                zone(
                    to_egui(top),
                    &insert,
                    insert_lit,
                    insert_lit.then_some(hint),
                );
                zone(
                    to_egui(bottom),
                    &open,
                    !insert_lit,
                    (!insert_lit).then_some(hint),
                );
            }
            Some((_, r)) => {
                let label = if action == drag::DropAction::InsertPath {
                    &insert
                } else {
                    &open
                };
                zone(to_egui(r), label, true, Some(hint));
            }
            None => zone(ctx.screen_rect(), &open, true, None),
        }
    }

    /// The terminal pane at `(x, y)` (logical points) and its text area,
    /// unless a floating layer covers the point (an editor pane is not a
    /// terminal).
    fn terminal_at(&self, x: f32, y: f32) -> Option<(String, Rect)> {
        let terminals: Vec<&str> = self
            .tabs
            .get(self.active_tab)
            .map(|t| t.panes.iter().map(|p| p.id.as_str()).collect())
            .unwrap_or_default();
        // Asked of the point itself: during a drag the window gets no pointer
        // motion, so egui's own pointer state is stale (a Wayland drop on the
        // terminal opened the editor).
        let over_ui = self
            .egui_ctx
            .layer_id_at(egui::pos2(x, y))
            .is_some_and(|layer| layer.order != egui::Order::Background);
        if over_ui {
            return None;
        }
        self.pane_rects()
            .into_iter()
            .find(|(id, r)| terminals.contains(&id.as_str()) && r.contains(x, y))
            .map(|(id, r)| (id, card_inner(r)))
    }

    /// What a drop at `(x, y)` does, and the terminal it goes to (`drag`).
    fn drop_target(&self, x: f32, y: f32) -> (drag::DropAction, Option<String>) {
        let pane = self.terminal_at(x, y);
        let alt = self.qa_drag.unwrap_or_else(|| drag::alt_down(self.mods));
        let action = drag::drop_action(pane.as_ref().map(|(_, r)| *r), y, self.drag_live, alt);
        (action, pane.map(|(id, _)| id))
    }

    /// A local change: sync in a moment rather than at the next minute.
    fn sync_soon(&mut self) {
        if self.sync.dir.is_some() && self.sync.key.is_some() {
            self.sync.due = Some(Instant::now() + Duration::from_secs(2));
        }
    }

    /// Start a background sync when one is due.
    fn sync_tick(&mut self) {
        let (Some(dir), Some(key)) = (self.sync.dir.clone(), self.sync.key.clone()) else {
            return;
        };
        if self.sync.running || self.sync.due.map_or(true, |due| Instant::now() < due) {
            return;
        }
        let Some(paths) = mtty_config::sync::Paths::default_paths() else {
            return;
        };
        self.sync.running = true;
        self.sync.due = None;
        self.spawn_job(move || {
            JobDone::Synced(mtty_config::sync::run(
                &dir,
                &key,
                &paths,
                mtty_config::sync::now_ms(),
            ))
        });
    }

    fn sync_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let Some(mut view) = self.sync_view.take() else {
            return;
        };
        let mut open = true;
        let mut turn_on = false;
        let mut turn_off = false;
        let mut new_key = false;
        let mut join = false;
        let mut sync_now = false;
        let have_key = self.sync.key.is_some();
        let on = self.sync.dir.is_some() && have_key;
        let red = chrome_rgb(self.theme.chrome().negative);
        let code = self.sync.key.as_ref().map(|k| k.pairing_code());
        let status = if self.sync.running {
            t(lang, "Syncing…", "正在同步…").to_string()
        } else {
            match &self.sync.last {
                Some((when, Ok(out))) => format!(
                    "{} {}s · {} {}",
                    t(lang, "Last sync", "上次同步"),
                    when.elapsed().map(|d| d.as_secs()).unwrap_or(0),
                    out.devices,
                    t(lang, "other device(s)", "台其他设备")
                ),
                Some((_, Err(e))) => e.clone(),
                None if on => t(lang, "Not synced yet", "尚未同步").to_string(),
                None => t(lang, "Sync is off", "同步未开启").to_string(),
            }
        };
        let running = self.sync.running;
        app_window(t(lang, "Sync Hosts and Snippets", "同步主机与片段"), ctx)
            .open(&mut open)
            .default_size([540.0, 360.0])
            .show(ctx, |ui| {
                window_body(ui, |ui| {
                    ui.label(t(
                        lang,
                        "Hosts and snippets are encrypted on this machine and written to a folder you already sync (iCloud Drive, Dropbox, Syncthing…). No account and no mtty server; the key never goes into the folder.",
                        "主机与片段在本机加密后写入一个你已在同步的文件夹(iCloud Drive、Dropbox、Syncthing…)。无需账号,不经 mtty 服务器;密钥永不进入该文件夹。",
                    ));
                    ui.add_space(8.0);
                    ui.label(t(lang, "Folder", "文件夹"));
                    ui.add(
                        egui::TextEdit::singleline(&mut view.dir)
                            .hint_text("~/Dropbox")
                            .desired_width(f32::INFINITY),
                    );
                    ui.add_space(8.0);
                    if let Some(code) = &code {
                        ui.horizontal(|ui| {
                            ui.label(t(lang, "Key: on this machine", "密钥:已在本机"));
                            let label = if view.show_code {
                                t(lang, "Hide pairing code", "隐藏配对码")
                            } else {
                                t(lang, "Show pairing code", "显示配对码")
                            };
                            if ui.button(label).clicked() {
                                view.show_code = !view.show_code;
                            }
                        });
                        if view.show_code {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(code).monospace().size(11.0));
                                if ui.small_button(t(lang, "Copy", "复制")).clicked() {
                                    ui.ctx().copy_text(code.clone());
                                }
                            });
                            ui.label(
                                egui::RichText::new(t(
                                    lang,
                                    "Paste it on your other device. Anyone with it can read the synced folder.",
                                    "在另一台设备上粘贴它。拿到配对码的人可以读取同步文件夹。",
                                ))
                                .size(11.0)
                                .color(red),
                            );
                        }
                    } else {
                        ui.label(t(
                            lang,
                            "First device: create a key. Another device: paste the first one's pairing code.",
                            "第一台设备:创建密钥。其他设备:粘贴第一台设备的配对码。",
                        ));
                        ui.horizontal(|ui| {
                            if ui.button(t(lang, "Create key", "创建密钥")).clicked() {
                                new_key = true;
                            }
                            ui.add(
                                egui::TextEdit::singleline(&mut view.pairing)
                                    .password(true)
                                    .hint_text("mtty-sync:…")
                                    .desired_width(220.0),
                            );
                            if ui
                                .add_enabled(
                                    !view.pairing.trim().is_empty(),
                                    egui::Button::new(t(lang, "Join", "加入")),
                                )
                                .clicked()
                            {
                                join = true;
                            }
                        });
                    }
                    if let Some(e) = &view.error {
                        ui.label(egui::RichText::new(e).color(red));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if on {
                            if ui
                                .add_enabled(!running, egui::Button::new(t(lang, "Sync Now", "立即同步")))
                                .clicked()
                            {
                                sync_now = true;
                            }
                            if ui.button(t(lang, "Save folder", "保存文件夹")).clicked() {
                                turn_on = true;
                            }
                            if ui.button(t(lang, "Turn off", "关闭同步")).clicked() {
                                turn_off = true;
                            }
                        } else if ui
                            .add_enabled(
                                have_key && !view.dir.trim().is_empty(),
                                egui::Button::new(t(lang, "Turn on", "开启同步")),
                            )
                            .clicked()
                        {
                            turn_on = true;
                        }
                    });
                    ui.label(
                        egui::RichText::new(&status)
                            .size(11.0)
                            .color(chrome_rgb(self.theme.chrome().muted)),
                    );
                });
            });
        if new_key || join {
            let key = if join {
                mtty_config::sync::Key::from_pairing_code(&view.pairing)
            } else {
                Ok(mtty_config::sync::Key::generate())
            };
            let saved = key.and_then(|key| {
                let path = mtty_config::sync::Key::path().ok_or("no config directory")?;
                key.save_to(&path)?;
                Ok(key)
            });
            match saved {
                Ok(key) => {
                    self.sync.key = Some(key);
                    view.pairing.clear();
                    view.error = None;
                    view.show_code = new_key;
                }
                Err(e) => view.error = Some(e),
            }
        }
        if turn_on {
            let dir = mtty_config::expand_home(view.dir.trim());
            if !dir.is_dir() {
                view.error = Some(format!(
                    "{}: {}",
                    t(lang, "Not a folder", "不是文件夹"),
                    dir.display()
                ));
            } else {
                let value = mtty_config::toml_string(&dir.to_string_lossy());
                match mtty_config::Config::save_settings(&[("sync-dir", value)]) {
                    Ok(_) => {
                        self.sync.dir = Some(dir);
                        self.sync.due = Some(Instant::now());
                        view.error = None;
                    }
                    Err(e) => view.error = Some(e.to_string()),
                }
            }
        }
        if turn_off {
            match mtty_config::Config::save_settings(&[("sync-dir", "\"\"".to_string())]) {
                Ok(_) => {
                    self.sync.dir = None;
                    self.sync.last = None;
                }
                Err(e) => view.error = Some(e.to_string()),
            }
        }
        if sync_now {
            self.sync.due = Some(Instant::now());
        }
        self.sync_tick();
        if open {
            self.sync_view = Some(view);
        }
    }

    fn ftp_dialog_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let Some(mut d) = self.ftp_dialog.take() else {
            return;
        };
        let mut open = true;
        let mut connect = false;
        app_window(t(lang, "Connect over FTP/FTPS", "连接 FTP/FTPS"), ctx)
            .open(&mut open)
            .default_size([460.0, 260.0])
            .show(ctx, |ui| {
                window_body(ui, |ui| {
                    ui.label(t(
                        lang,
                        "Server ([user@]host[:port])",
                        "服务器([用户@]主机[:端口])",
                    ));
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut d.address)
                            .hint_text("deploy@ftp.example.com")
                            .desired_width(f32::INFINITY),
                    );
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        connect = true;
                    }
                    if !r.has_focus() && d.address.is_empty() {
                        r.request_focus();
                    }
                    ui.label(t(
                        lang,
                        "Password (kept in memory only; empty: ~/.netrc or anonymous)",
                        "口令(仅保存在内存中;留空则用 ~/.netrc 或匿名)",
                    ));
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut d.password)
                            .password(true)
                            .desired_width(f32::INFINITY),
                    );
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        connect = true;
                    }
                    ui.horizontal(|ui| {
                        ui.radio_value(
                            &mut d.security,
                            1,
                            t(lang, "FTPS (explicit TLS)", "FTPS(显式 TLS)"),
                        );
                        ui.radio_value(
                            &mut d.security,
                            2,
                            t(lang, "FTPS (implicit)", "FTPS(隐式)"),
                        );
                        ui.radio_value(
                            &mut d.security,
                            0,
                            t(lang, "FTP (unencrypted)", "FTP(不加密)"),
                        );
                    });
                    if d.security != 0 {
                        ui.checkbox(
                            &mut d.insecure,
                            t(
                                lang,
                                "Accept any certificate (self-signed; unsafe)",
                                "接受任意证书(自签名;不安全)",
                            ),
                        );
                    }
                    if let Some(e) = &d.error {
                        ui.label(
                            egui::RichText::new(e).color(chrome_rgb(self.theme.chrome().negative)),
                        );
                    }
                    if ui.button(t(lang, "Connect", "连接")).clicked() {
                        connect = true;
                    }
                });
            });
        if connect {
            // A typed scheme wins over the radio buttons.
            let typed = d.address.trim();
            let address = if typed.contains("://") {
                typed.to_string()
            } else {
                let scheme = ["ftp", "ftpes", "ftps"][d.security.min(2)];
                format!("{scheme}://{typed}")
            };
            match mtty_ui::ftp::Remote::parse(&address) {
                Ok(mut remote) => {
                    remote.password =
                        Some(std::mem::take(&mut d.password)).filter(|p| !p.is_empty());
                    remote.insecure = d.insecure && !remote.is_plaintext();
                    self.open_files(remote.label(), mtty_ui::sftp::Endpoint::Ftp(remote));
                    return;
                }
                Err(e) => d.error = Some(e),
            }
        }
        if open {
            self.ftp_dialog = Some(d);
        }
    }

    fn open_sftp(&mut self, title: String, remote: mtty_ui::sftp::Remote) {
        self.open_files(title, mtty_ui::sftp::Endpoint::Sftp(remote));
    }

    fn open_files(&mut self, title: String, remote: mtty_ui::sftp::Endpoint) {
        let local_dir = self
            .active_cwd_for_new()
            .or_else(mtty_config::home_dir)
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        self.sftp_view = Some(SftpView {
            title,
            remote,
            remote_dir: None,
            remote_entries: Vec::new(),
            remote_sel: None,
            local_dir,
            local_entries: Vec::new(),
            local_sel: None,
            busy: None,
            error: None,
            rename: None,
            chmod: None,
            new_folder: None,
            confirm_delete: false,
            show_hidden: false,
            rect: egui::Rect::NOTHING,
        });
        self.sftp_refresh(true, true);
    }

    /// List the remote (from home when no directory yet) and/or local side.
    fn sftp_refresh(&mut self, remote: bool, local: bool) {
        let Some(view) = self.sftp_view.as_mut() else {
            return;
        };
        if remote {
            let r = view.remote.clone();
            let dir = view.remote_dir.clone();
            view.busy.get_or_insert(("listing".into(), None));
            self.spawn_job(move || {
                let dir = match dir {
                    Some(d) => Ok(d),
                    None => r.home(),
                };
                match dir {
                    Ok(dir) => {
                        let result = r.list(&dir);
                        JobDone::SftpListed { dir, result }
                    }
                    Err(e) => JobDone::SftpListed {
                        dir: "/".into(),
                        result: Err(e),
                    },
                }
            });
        }
        if local {
            let Some(view) = self.sftp_view.as_ref() else {
                return;
            };
            let dir = view.local_dir.clone();
            self.spawn_job(move || {
                let entries = list_dir(&dir, true);
                JobDone::SftpLocalListed { dir, entries }
            });
        }
    }

    /// Run one SFTP operation in the background; both sides refresh after.
    fn sftp_job(
        &mut self,
        label: String,
        progress: Option<(std::path::PathBuf, u64)>,
        work: impl FnOnce(&mtty_ui::sftp::Endpoint) -> Result<(), String> + Send + 'static,
    ) {
        let Some(view) = self.sftp_view.as_mut() else {
            return;
        };
        view.busy = Some((label.clone(), progress));
        let remote = view.remote.clone();
        self.spawn_job(move || JobDone::SftpDone {
            result: work(&remote),
            label,
        });
    }

    /// Upload dropped or selected local paths into the current remote folder.
    fn sftp_upload(&mut self, paths: Vec<std::path::PathBuf>) {
        let Some(dir) = self.sftp_view.as_ref().and_then(|v| v.remote_dir.clone()) else {
            return;
        };
        let names: Vec<String> = paths
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .collect();
        self.sftp_job(format!("\u{2191} {}", names.join(", ")), None, move |r| {
            paths.iter().try_for_each(|p| r.upload(p, &dir))
        });
    }

    fn sftp_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let Some(mut view) = self.sftp_view.take() else {
            return;
        };
        let mut open = true;
        let mut local_nav: Option<std::path::PathBuf> = None;
        let mut remote_nav: Option<String> = None;
        let mut upload = false;
        let mut download = false;
        let mut do_rename = false;
        let mut do_chmod = false;
        let mut do_mkdir = false;
        let mut do_delete = false;
        let mut refresh = false;
        let busy_text = view.busy.as_ref().map(|(label, progress)| match progress {
            Some((path, total)) if *total > 0 => {
                let done = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                format!(
                    "{label} \u{2014} {} / {}",
                    human_size(done),
                    human_size(*total)
                )
            }
            _ => format!("{label}\u{2026}"),
        });
        if view.busy.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        let kind = match &view.remote {
            mtty_ui::sftp::Endpoint::Sftp(_) => "SFTP",
            mtty_ui::sftp::Endpoint::Ftp(r) => match r.security {
                mtty_ui::ftp::Security::Plain => "FTP",
                _ => "FTPS",
            },
        };
        let plaintext = view.remote.is_plaintext();
        let response = app_window(format!("{kind} \u{00b7} {}", view.title), ctx)
            .open(&mut open)
            .default_size([860.0, 520.0])
            .show(ctx, |ui| {
                if plaintext {
                    ui.label(
                        egui::RichText::new(t(
                            lang,
                            "Plain FTP: the password and the files cross the network unencrypted.",
                            "明文 FTP:口令与文件均以未加密方式在网络上传输。",
                        ))
                        .color(chrome_rgb(self.theme.chrome().negative)),
                    );
                }
                ui.horizontal(|ui| {
                    if let Some(text) = &busy_text {
                        ui.spinner();
                        ui.label(text);
                    } else if let Some(e) = &view.error {
                        ui.label(
                            egui::RichText::new(e).color(chrome_rgb(self.theme.chrome().negative)),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new(t(
                                lang,
                                "Drop files here to upload them to the right-hand folder.",
                                "把文件拖到这里即上传到右侧目录。",
                            ))
                            .size(11.0)
                            .color(chrome_rgb(self.theme.chrome().muted)),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(t(lang, "Refresh", "刷新")).clicked() {
                            refresh = true;
                        }
                        if ui
                            .checkbox(
                                &mut view.show_hidden,
                                t(lang, "Show hidden", "显示隐藏文件"),
                            )
                            .changed()
                        {
                            refresh = true;
                        }
                    });
                });
                ui.separator();
                let idle = view.busy.is_none();
                ui.columns(2, |cols| {
                    // This machine.
                    let ui = &mut cols[0];
                    ui.horizontal(|ui| {
                        if ui
                            .small_button("\u{2191}")
                            .on_hover_text(t(lang, "Up", "上一级"))
                            .clicked()
                        {
                            if let Some(p) = view.local_dir.parent() {
                                local_nav = Some(p.to_path_buf());
                            }
                        }
                        ui.label(
                            egui::RichText::new(view.local_dir.display().to_string()).size(11.0),
                        );
                    });
                    egui::ScrollArea::vertical()
                        .id_salt("sftp-local")
                        .max_height(360.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for e in view
                                .local_entries
                                .iter()
                                .filter(|e| view.show_hidden || !e.name.starts_with('.'))
                            {
                                let label = if e.is_dir {
                                    format!("\u{ea83} {}", e.name)
                                } else {
                                    format!("    {}  \u{00b7} {}", e.name, human_size(e.size))
                                };
                                let sel = view.local_sel.as_deref() == Some(&e.name);
                                let r = ui.selectable_label(sel, label);
                                if r.clicked() {
                                    view.local_sel = Some(e.name.clone());
                                }
                                if r.double_clicked() && e.is_dir {
                                    local_nav = Some(view.local_dir.join(&e.name));
                                }
                            }
                        });
                    ui.add_enabled_ui(
                        idle && view.local_sel.is_some() && view.remote_dir.is_some(),
                        |ui| {
                            if ui
                                .button(t(lang, "Upload \u{2192}", "上传 \u{2192}"))
                                .clicked()
                            {
                                upload = true;
                            }
                        },
                    );
                    // The host.
                    let ui = &mut cols[1];
                    ui.horizontal(|ui| {
                        if ui
                            .small_button("\u{2191}")
                            .on_hover_text(t(lang, "Up", "上一级"))
                            .clicked()
                        {
                            if let Some(d) = &view.remote_dir {
                                remote_nav = Some(mtty_ui::sftp::parent(d));
                            }
                        }
                        ui.label(
                            egui::RichText::new(
                                view.remote_dir.clone().unwrap_or_else(|| "\u{2026}".into()),
                            )
                            .size(11.0),
                        );
                    });
                    egui::ScrollArea::vertical()
                        .id_salt("sftp-remote")
                        .max_height(360.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for e in view
                                .remote_entries
                                .iter()
                                .filter(|e| view.show_hidden || !e.name.starts_with('.'))
                            {
                                let label = if e.is_dir {
                                    format!("\u{ea83} {}", e.name)
                                } else {
                                    format!(
                                        "    {}  \u{00b7} {}  {}",
                                        e.name,
                                        human_size(e.size),
                                        e.perms
                                    )
                                };
                                let sel = view.remote_sel.as_deref() == Some(&e.name);
                                let r = ui.selectable_label(sel, label).on_hover_text(&e.modified);
                                if r.clicked() {
                                    view.remote_sel = Some(e.name.clone());
                                    view.rename = None;
                                    view.chmod = None;
                                    view.confirm_delete = false;
                                }
                                if r.double_clicked() && e.is_dir {
                                    if let Some(d) = &view.remote_dir {
                                        remote_nav = Some(mtty_ui::sftp::join(d, &e.name));
                                    }
                                }
                            }
                        });
                    let selected = view.remote_sel.is_some();
                    ui.add_enabled_ui(idle, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            if ui
                                .add_enabled(
                                    selected,
                                    egui::Button::new(t(
                                        lang,
                                        "\u{2190} Download",
                                        "\u{2190} 下载",
                                    )),
                                )
                                .clicked()
                            {
                                download = true;
                            }
                            if ui
                                .add_enabled(
                                    selected,
                                    egui::Button::new(t(lang, "Rename", "重命名")),
                                )
                                .clicked()
                            {
                                view.rename = view.remote_sel.clone();
                            }
                            if ui
                                .add_enabled(selected, egui::Button::new("chmod"))
                                .clicked()
                            {
                                view.chmod = Some("644".into());
                            }
                            if ui.button(t(lang, "New folder", "新建文件夹")).clicked() {
                                view.new_folder = Some(String::new());
                            }
                            if ui
                                .add_enabled(
                                    selected,
                                    egui::Button::new(t(lang, "Delete…", "删除…")),
                                )
                                .clicked()
                            {
                                view.confirm_delete = true;
                            }
                        });
                        if let Some(name) = view.rename.as_mut() {
                            ui.horizontal(|ui| {
                                ui.text_edit_singleline(name);
                                if ui.button(t(lang, "Rename", "重命名")).clicked() {
                                    do_rename = true;
                                }
                            });
                        }
                        if let Some(mode) = view.chmod.as_mut() {
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(mode).desired_width(60.0));
                                if ui.button(t(lang, "Apply", "应用")).clicked() {
                                    do_chmod = true;
                                }
                            });
                        }
                        if let Some(name) = view.new_folder.as_mut() {
                            ui.horizontal(|ui| {
                                ui.text_edit_singleline(name);
                                if ui.button(t(lang, "Create", "创建")).clicked() {
                                    do_mkdir = true;
                                }
                            });
                        }
                        if view.confirm_delete {
                            ui.horizontal(|ui| {
                                if ui
                                    .button(
                                        egui::RichText::new(t(lang, "Confirm delete", "确认删除"))
                                            .color(chrome_rgb(self.theme.chrome().negative)),
                                    )
                                    .clicked()
                                {
                                    do_delete = true;
                                }
                                if ui.button(t(lang, "Cancel", "取消")).clicked() {
                                    view.confirm_delete = false;
                                }
                            });
                        }
                    });
                });
            });
        if let Some(r) = response {
            view.rect = r.response.rect;
        }
        if !open {
            return;
        }
        let remote_dir = view.remote_dir.clone();
        let selected = view
            .remote_sel
            .as_ref()
            .and_then(|n| view.remote_entries.iter().find(|e| &e.name == n))
            .cloned();
        let local_sel = view.local_sel.clone().map(|n| view.local_dir.join(n));
        let local_dir = view.local_dir.clone();
        // An inline editor closes only when its action runs.
        let rename_to = if do_rename { view.rename.take() } else { None };
        let chmod_to = if do_chmod { view.chmod.take() } else { None };
        let mkdir_name = if do_mkdir {
            view.new_folder.take()
        } else {
            None
        };
        if do_delete {
            view.confirm_delete = false;
        }
        let local_changed = local_nav.is_some();
        if let Some(p) = local_nav {
            view.local_dir = p;
            view.local_sel = None;
        }
        if let Some(d) = remote_nav.clone() {
            view.remote_dir = Some(d);
            view.remote_sel = None;
        }
        self.sftp_view = Some(view);
        if refresh || remote_nav.is_some() {
            self.sftp_refresh(true, false);
        }
        if refresh || local_changed {
            self.sftp_refresh(false, true);
        }
        if upload {
            if let Some(p) = local_sel {
                self.sftp_upload(vec![p]);
            }
        }
        let (Some(dir), Some(entry)) = (remote_dir, selected) else {
            if let (Some(name), Some(dir)) = (
                mkdir_name,
                self.sftp_view.as_ref().and_then(|v| v.remote_dir.clone()),
            ) {
                let path = mtty_ui::sftp::join(&dir, name.trim());
                self.sftp_job(format!("mkdir {}", name.trim()), None, move |r| {
                    r.mkdir(&path)
                });
            }
            return;
        };
        let path = mtty_ui::sftp::join(&dir, &entry.name);
        if download {
            let target = local_dir.join(&entry.name);
            let progress = (!entry.is_dir).then_some((target, entry.size));
            let dest = local_dir.clone();
            let is_dir = entry.is_dir;
            self.sftp_job(format!("\u{2193} {}", entry.name), progress, move |r| {
                r.download(&path, is_dir, &dest)
            });
        } else if let Some(new) = rename_to {
            let to = mtty_ui::sftp::join(&dir, new.trim());
            self.sftp_job(format!("rename {}", entry.name), None, move |r| {
                r.rename(&path, &to)
            });
        } else if let Some(mode) = chmod_to {
            self.sftp_job(format!("chmod {mode} {}", entry.name), None, move |r| {
                r.chmod(mode.trim(), &path)
            });
        } else if do_delete {
            let is_dir = entry.is_dir;
            self.sftp_job(format!("delete {}", entry.name), None, move |r| {
                r.remove(&path, is_dir)
            });
        } else if let Some(name) = mkdir_name {
            let path = mtty_ui::sftp::join(&dir, name.trim());
            self.sftp_job(format!("mkdir {}", name.trim()), None, move |r| {
                r.mkdir(&path)
            });
        }
    }

    /// A new tab in the active directory that runs `cmd` (interactive tools
    /// like ssh-keygen ask for secrets there, never through mtty).
    fn run_in_new_tab(&mut self, title: &str, cmd: &str) {
        self.new_tab_in(self.active_cwd_for_new());
        if let Some(tab) = self.tabs.last_mut() {
            tab.title = title.to_string();
            tab.title_set = true;
            let active = tab.active.clone();
            if let Some(pane) = tab.panes.iter_mut().find(|p| p.id == active) {
                pane.term.write(format!("{cmd}\r").as_bytes());
            }
        }
        self.publish_panes();
    }

    /// Connect to a saved host (B3.1).
    fn open_host(&mut self, host: &mtty_config::hosts::Host) {
        use mtty_config::hosts::HostKind;
        match host.kind {
            HostKind::Ssh => self.open_ssh_host(host),
            HostKind::Serial => {
                let p = host.serial.clone().unwrap_or_default();
                self.connect_transport(
                    TransportTarget::Serial {
                        device: p.device,
                        baud: p.baud,
                        data_bits: p.data_bits,
                        parity: p.parity,
                        stop_bits: p.stop_bits,
                        flow: p.flow,
                    },
                    Some(host.name.clone()),
                );
            }
            HostKind::Telnet => {
                let target = TransportTarget::Telnet {
                    host: host.address.clone().unwrap_or_default(),
                    port: host.port.unwrap_or(23),
                };
                self.connect_transport(target, Some(host.name.clone()));
            }
            HostKind::Tcp => {
                let target = TransportTarget::Tcp {
                    host: host.address.clone().unwrap_or_default(),
                    port: host.port.unwrap_or(0),
                };
                self.connect_transport(target, Some(host.name.clone()));
            }
        }
    }

    /// Dial a serial, Telnet or raw TCP session on a background thread; the
    /// pane opens when the connection lands (ADR 0037).
    fn connect_transport(&mut self, target: TransportTarget, title: Option<String>) {
        use mtty_ui::transport as tp;
        self.pending_transport_connects += 1;
        let label = target.label();
        let msg = format!(
            "{} {label}…",
            mtty_ui::i18n::t(self.lang, "Connecting", "正在连接")
        );
        self.show_notice(msg);
        self.spawn_job(move || {
            let result = match &target {
                TransportTarget::Telnet { host, port } => {
                    tp::connect_telnet(host, *port, Duration::from_secs(10))
                }
                TransportTarget::Tcp { host, port } => {
                    tp::connect_tcp(host, *port, Duration::from_secs(10))
                }
                TransportTarget::Serial {
                    device,
                    baud,
                    data_bits,
                    parity,
                    stop_bits,
                    flow,
                } => tp::connect_serial(&tp::SerialConfig {
                    device: device.clone(),
                    baud: *baud,
                    data_bits: *data_bits,
                    parity: parity.clone(),
                    stop_bits: *stop_bits,
                    flow: flow.clone(),
                }),
            };
            JobDone::TransportConnected {
                target,
                title,
                result,
            }
        });
    }

    /// Open a pane over a live connection and make it the active tab.
    fn open_transport_pane(
        &mut self,
        target: TransportTarget,
        title: Option<String>,
        conn: mtty_ui::transport::Connection,
    ) {
        let Some(pane) = self.spawn_transport_pane(conn) else {
            let msg = mtty_ui::i18n::t(self.lang, "Could not start the session", "无法启动会话")
                .to_string();
            self.show_notice(msg);
            return;
        };
        let id = pane.id.clone();
        let title = title.unwrap_or_else(|| target.label());
        let plaintext = target.plaintext();
        self.tabs.push(Tab {
            layout: Layout::leaf(id.clone()),
            panes: vec![pane],
            active: id,
            title,
            title_set: true,
            shown: None,
            ssh: false,
            ssh_target: None,
            ssh_cmd: None,
            transport: Some(target),
            prefix: None,
            mark: None,
            group: None,
            attention: None,
            editors: Vec::new(),
            previews: Vec::new(),
        });
        self.active_tab = self.tabs.len() - 1;
        self.selection = None;
        self.publish_panes();
        if plaintext {
            let msg = mtty_ui::i18n::t(
                self.lang,
                "This connection is unencrypted.",
                "此连接未加密。",
            )
            .to_string();
            self.show_notice(msg);
        }
    }

    /// A pane whose terminal runs over a byte pipe instead of a PTY.
    fn spawn_transport_pane(&self, conn: mtty_ui::transport::Connection) -> Option<Pane> {
        let scale = self.window.scale_factor() as f32;
        let (cw, ch) = (self.cw * scale, self.ch * scale);
        let area = card_inner(self.grid_area());
        let cols = ((area.w * scale) / cw).floor().max(1.0) as u16;
        let rows = ((area.h * scale) / ch).floor().max(1.0) as u16;
        let id = gen_id();
        let proxy = self.proxy.clone();
        let waker: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = proxy.send_event(HostEvent::Wake);
        });
        let mut term = Terminal::from_pipe(cols, rows, 10_000, conn.reader, conn.writer, waker);
        term.set_graphics_enabled(self.graphics_enabled);
        let Rgb(r, g, b) = self.theme.fg;
        let Rgb(br, bg, bb) = self.theme.bg;
        term.set_default_colors([r, g, b], [br, bg, bb]);
        term.set_cell_size((self.cw * scale) as u16, (self.ch * scale) as u16);
        Some(Pane {
            id,
            term,
            scroll: 0,
            on_enter: None,
            published_output: None,
            pending_images: None,
            pending_note: None,
        })
    }

    /// The ssh branch of [`State::open_host`]: an alias or a full command.
    fn open_ssh_host(&mut self, host: &mtty_config::hosts::Host) {
        let destination = host.destination();
        let mosh = host.mosh && mtty_ui::ssh::on_path("mosh");
        if host.mosh && !mosh {
            self.show_notice(
                t_lang(
                    self.lang,
                    "mosh is not installed here; connecting with ssh",
                    "本机未安装 mosh,改用 ssh 连接",
                )
                .into(),
            );
        }
        let cmd = mtty_ui::ssh::persistent_host_command(
            &destination,
            &host.ssh_options(),
            host.tmux.as_deref(),
            mosh,
        );
        self.open_ssh_command(host.name.clone(), cmd, destination);
    }

    /// A new tab that runs an ssh command and remembers how to rerun it.
    fn open_ssh_command(&mut self, title: String, cmd: String, target: String) {
        self.new_tab();
        if let Some(tab) = self.tabs.last_mut() {
            tab.ssh_target = Some(target);
            tab.ssh_cmd = Some(cmd.clone());
            tab.title = title;
            tab.title_set = true;
            tab.ssh = true;
            let active = tab.active.clone();
            if let Some(pane) = tab.panes.iter_mut().find(|p| p.id == active) {
                pane.term.write(typed_ssh(&cmd).as_bytes());
            }
        }
        self.publish_panes();
    }

    fn reload_hosts(&mut self) {
        match mtty_config::hosts::HostBook::load() {
            Ok(book) => {
                self.host_book = book;
                self.host_book_error = None;
            }
            Err(e) => {
                self.show_notice(e.clone());
                self.host_book_error = Some(e);
            }
        }
    }

    /// Save the host library unless the file on disk could not be read.
    fn save_hosts(&mut self) -> bool {
        use mtty_ui::i18n::t;
        if let Some(e) = self.host_book_error.clone() {
            let msg = format!("{}: {e}", t(self.lang, "Hosts not saved", "主机未保存"));
            self.show_notice(msg);
            return false;
        }
        match self.host_book.save() {
            Ok(()) => {
                self.sync_soon();
                true
            }
            Err(e) => {
                let msg = format!("{}: {e}", t(self.lang, "Hosts not saved", "主机未保存"));
                self.show_notice(msg);
                false
            }
        }
    }

    /// Read a PuTTY `.ppk` and write an encrypted OpenSSH key (ADR 0038).
    fn import_putty_key(&mut self) {
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let Some(dlg) = self.key_import.take() else {
            return;
        };
        if dlg.path.trim().is_empty() {
            let msg = t(lang, "Choose a .ppk file.", "请选择 .ppk 文件。").to_string();
            return self.keep_key_import(dlg, msg);
        }
        if dlg.new.is_empty() || dlg.new != dlg.confirm {
            let msg = t(
                lang,
                "The new passphrases do not match (and must not be empty).",
                "新口令不一致(且不能为空)。",
            )
            .to_string();
            return self.keep_key_import(dlg, msg);
        }
        let Some(home) = mtty_config::home_dir() else {
            return self.keep_key_import(dlg, "no home directory".into());
        };
        let name = if dlg.name.trim().is_empty() {
            std::path::Path::new(&dlg.path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "id_imported".into())
        } else {
            dlg.name.trim().to_string()
        };
        let dest = home.join(".ssh").join(name);
        let (path, old, new, overwrite) = (
            dlg.path.trim().to_string(),
            dlg.old.clone(),
            dlg.new.clone(),
            dlg.overwrite,
        );
        self.spawn_job(move || {
            let result = import_key_file(&path, &old, &new, &dest, overwrite);
            JobDone::KeyImported(result)
        });
    }

    fn keep_key_import(&mut self, mut dlg: KeyImportDialog, msg: String) {
        dlg.error = Some(msg);
        self.key_import = Some(dlg);
    }

    /// The Import PuTTY Key form (ADR 0038).
    fn key_import_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let Some(dlg) = self.key_import.as_mut() else {
            return;
        };
        let mut open = true;
        let mut go = false;
        egui::Window::new(t(lang, "Import PuTTY Key", "导入 PuTTY 密钥"))
            .collapsible(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(t(lang, "File", "文件"));
                    ui.add(
                        egui::TextEdit::singleline(&mut dlg.path)
                            .hint_text("key.ppk")
                            .desired_width(240.0),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(t(lang, "Save as", "保存为"));
                    ui.add(
                        egui::TextEdit::singleline(&mut dlg.name)
                            .hint_text("~/.ssh/<name>")
                            .desired_width(200.0),
                    );
                });
                for (label, field, hint) in [
                    (
                        t(lang, "Key passphrase", "密钥口令"),
                        &mut dlg.old,
                        t(lang, "(empty if unencrypted)", "(未加密则留空)"),
                    ),
                    (t(lang, "New passphrase", "新口令"), &mut dlg.new, ""),
                    (t(lang, "Confirm", "确认新口令"), &mut dlg.confirm, ""),
                ] {
                    ui.horizontal(|ui| {
                        ui.label(label);
                        ui.add(
                            egui::TextEdit::singleline(field)
                                .password(true)
                                .hint_text(hint)
                                .desired_width(200.0),
                        );
                    });
                }
                ui.checkbox(
                    &mut dlg.overwrite,
                    t(lang, "Overwrite if it exists", "若已存在则覆盖"),
                );
                ui.label(
                    egui::RichText::new(t(
                        lang,
                        "The key is written encrypted; an empty new passphrase is refused.",
                        "密钥以加密形式写入;新口令为空会被拒绝。",
                    ))
                    .size(11.0)
                    .color(chrome_rgb(self.theme.chrome().muted)),
                );
                if let Some(err) = &dlg.error {
                    ui.colored_label(chrome_rgb(self.theme.chrome().negative), err);
                }
                if ui.button(t(lang, "Import", "导入")).clicked() {
                    go = true;
                }
            });
        if go {
            self.import_putty_key();
        } else if !open {
            self.key_import = None;
        }
    }

    fn hosts_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let Some(mut view) = self.hosts_view.take() else {
            return;
        };
        let mut open = true;
        let mut connect: Option<usize> = None;
        let mut delete: Option<usize> = None;
        let mut import = false;
        let mut add = false;
        let mut check_key: Option<usize> = None;
        let mut trust_key: Option<usize> = None;
        let mut copy_key: Option<usize> = None;
        let mut new_key = false;
        let mut import_key = false;
        let mut open_files: Option<usize> = None;
        let mut persist: Option<(usize, Option<String>, bool)> = None;
        // (host index, rule index, start?) / (host index, rule index) / host index
        let mut toggle_forward: Option<(usize, usize, bool)> = None;
        let mut remove_forward: Option<(usize, usize)> = None;
        let mut add_forward: Option<usize> = None;
        let tunnel_states: HashMap<(String, String), mtty_ui::forward::TunnelState> = self
            .tunnels
            .iter_mut()
            .map(|(k, t)| (k.clone(), t.state()))
            .collect();
        let rows: Vec<(usize, mtty_config::hosts::Host)> = self
            .host_book
            .sorted()
            .into_iter()
            .map(|(i, h)| (i, h.clone()))
            .collect();
        app_window(t(lang, "Hosts", "主机"), ctx)
            .collapsible(false)
            .open(&mut open)
            .default_size([560.0, 460.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut view.filter)
                            .hint_text(t(lang, "Search hosts…", "搜索主机…"))
                            .desired_width(260.0),
                    );
                    if ui
                        .button(t(lang, "Import from ~/.ssh/config", "从 ~/.ssh/config 导入"))
                        .clicked()
                    {
                        import = true;
                    }
                });
                ui.horizontal(|ui| {
                    use mtty_ui::hostkeys::Agent;
                    let agent = match &view.agent {
                        None => t(lang, "ssh-agent: checking…", "ssh-agent:检查中…").to_string(),
                        Some(Agent::Keys(k)) => format!(
                            "{} {}",
                            t(lang, "ssh-agent: keys loaded:", "ssh-agent:已加载密钥"),
                            k.len()
                        ),
                        Some(Agent::NoKeys) => t(
                            lang,
                            "ssh-agent: running, no keys (ssh-add)",
                            "ssh-agent:运行中,无密钥(ssh-add)",
                        )
                        .to_string(),
                        Some(Agent::NotRunning) => {
                            t(lang, "ssh-agent: not running", "ssh-agent:未运行").to_string()
                        }
                    };
                    ui.label(egui::RichText::new(agent).size(11.0));
                    if ui
                        .small_button(t(lang, "New SSH key…", "生成新密钥…"))
                        .on_hover_text(t(
                            lang,
                            "Runs ssh-keygen in a new tab; you choose the passphrase there.",
                            "在新标签中运行 ssh-keygen,口令由你在其中设置。",
                        ))
                        .clicked()
                    {
                        new_key = true;
                    }
                    if ui
                        .small_button(t(lang, "Import PuTTY Key…", "导入 PuTTY 密钥…"))
                        .on_hover_text(t(
                            lang,
                            "Read a PuTTY .ppk and write an encrypted OpenSSH key.",
                            "读取 PuTTY .ppk,写出加密的 OpenSSH 密钥。",
                        ))
                        .clicked()
                    {
                        import_key = true;
                    }
                });
                let q = view.filter.to_lowercase();
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    if rows.is_empty() {
                        ui.label(t(
                            lang,
                            "No saved hosts yet. Import them or add one below.",
                            "还没有保存的主机。可以导入,或在下方添加。",
                        ));
                    }
                    for (i, host) in &rows {
                        let haystack = format!(
                            "{} {} {} {}",
                            host.name,
                            host.summary(),
                            host.group.as_deref().unwrap_or(""),
                            host.tags.join(" ")
                        )
                        .to_lowercase();
                        if !q.is_empty() && !haystack.contains(&q) {
                            continue;
                        }
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&host.name).strong());
                            ui.label(
                                egui::RichText::new(host.summary())
                                    .size(11.0)
                                    .color(chrome_rgb(self.theme.chrome().muted)),
                            );
                            if let Some(group) = &host.group {
                                ui.label(egui::RichText::new(format!("[{group}]")).size(11.0));
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if view.confirm_delete == Some(*i) {
                                    if ui.button(t(lang, "Cancel", "取消")).clicked() {
                                        view.confirm_delete = None;
                                    }
                                    if ui.button(t(lang, "Confirm delete", "确认删除")).clicked() {
                                        delete = Some(*i);
                                    }
                                } else {
                                    if ui.button(t(lang, "Delete…", "删除…")).clicked() {
                                        view.confirm_delete = Some(*i);
                                    }
                                    if ui
                                        .button(t(lang, "Copy my key", "复制我的公钥"))
                                        .on_hover_text(t(
                                            lang,
                                            "Runs ssh-copy-id in a new tab",
                                            "在新标签中运行 ssh-copy-id",
                                        ))
                                        .clicked()
                                    {
                                        copy_key = Some(*i);
                                    }
                                    if ui.button(t(lang, "Files", "文件")).clicked() {
                                        open_files = Some(*i);
                                    }
                                    if ui.button(t(lang, "Check key", "检查主机密钥")).clicked() {
                                        check_key = Some(*i);
                                    }
                                    if ui.button(t(lang, "Connect", "连接")).clicked() {
                                        connect = Some(*i);
                                    }
                                }
                            });
                        });
                        egui::CollapsingHeader::new(t(lang, "Persistent session", "持久会话"))
                            .id_salt(("persist", &host.name))
                            .show(ui, |ui| {
                                let mut tmux = host.tmux.is_some();
                                let mut mosh = host.mosh;
                                let a = ui.checkbox(
                                    &mut tmux,
                                    t(lang, "Keep the shell in tmux (reconnecting returns to it)", "在 tmux 中保持 shell(重连后回到原会话)"),
                                );
                                let b = ui.checkbox(
                                    &mut mosh,
                                    t(lang, "Connect with mosh (survives sleep and network changes)", "使用 mosh 连接(休眠、换网络不断线)"),
                                );
                                if a.changed() || b.changed() {
                                    persist = Some((*i, tmux.then(|| host.tmux.clone().unwrap_or_else(|| "mtty".into())), mosh));
                                }
                            });
                        let title = format!(
                            "{} ({})",
                            t(lang, "Port forwards", "端口转发"),
                            host.forwards.len()
                        );
                        egui::CollapsingHeader::new(title)
                            .id_salt(("forwards", &host.name))
                            .show(ui, |ui| {
                                use mtty_config::hosts::ForwardKind;
                                use mtty_ui::forward::TunnelState;
                                for (r, fwd) in host.forwards.iter().enumerate() {
                                    let key = (host.name.clone(), fwd.spec.clone());
                                    let state = tunnel_states.get(&key);
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{} {}",
                                                fwd.kind.flag(),
                                                fwd.spec
                                            ))
                                            .monospace(),
                                        );
                                        match state {
                                            Some(TunnelState::Running) => {
                                                ui.label(
                                                    egui::RichText::new(t(lang, "\u{25cf} running", "\u{25cf} 运行中"))
                                                        .color(chrome_rgb(self.theme.chrome().positive)),
                                                );
                                                if ui.small_button(t(lang, "Stop", "停止")).clicked() {
                                                    toggle_forward = Some((*i, r, false));
                                                }
                                            }
                                            other => {
                                                if let Some(TunnelState::Exited(msg)) = other {
                                                    ui.label(
                                                        egui::RichText::new(msg)
                                                            .size(11.0)
                                                            .color(chrome_rgb(self.theme.chrome().negative)),
                                                    );
                                                }
                                                if ui.small_button(t(lang, "Start", "启动")).clicked() {
                                                    toggle_forward = Some((*i, r, true));
                                                }
                                                if ui.small_button(t(lang, "Remove", "删除")).clicked() {
                                                    remove_forward = Some((*i, r));
                                                }
                                            }
                                        }
                                    });
                                }
                                let form = view
                                    .new_forward
                                    .get_or_insert_with(|| (host.name.clone(), ForwardKind::Local, String::new()));
                                if form.0 != host.name {
                                    *form = (host.name.clone(), ForwardKind::Local, String::new());
                                }
                                ui.horizontal(|ui| {
                                    egui::ComboBox::from_id_salt(("fwd-kind", &host.name))
                                        .selected_text(form.1.flag())
                                        .width(48.0)
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(&mut form.1, ForwardKind::Local, "-L local");
                                            ui.selectable_value(&mut form.1, ForwardKind::Remote, "-R remote");
                                            ui.selectable_value(&mut form.1, ForwardKind::Dynamic, "-D SOCKS");
                                        });
                                    let hint = if form.1 == ForwardKind::Dynamic {
                                        "1080"
                                    } else {
                                        "8080:localhost:80"
                                    };
                                    ui.add(egui::TextEdit::singleline(&mut form.2).hint_text(hint).desired_width(200.0));
                                    let candidate = mtty_config::hosts::Forward {
                                        kind: form.1,
                                        spec: form.2.trim().to_string(),
                                    };
                                    let check = candidate.validate();
                                    if ui
                                        .add_enabled(check.is_ok(), egui::Button::new(t(lang, "Add", "添加")))
                                        .on_disabled_hover_text(check.err().unwrap_or_default())
                                        .clicked()
                                    {
                                        add_forward = Some(*i);
                                    }
                                });
                            });
                        if let Some(state) = view.keys.get(&host.name) {
                            use mtty_ui::hostkeys::HostKey;
                            let red = chrome_rgb(self.theme.chrome().negative);
                            let green = chrome_rgb(self.theme.chrome().positive);
                            match state {
                                None => {
                                    ui.label(t(lang, "Checking the host key…", "正在检查主机密钥…"));
                                }
                                Some(Ok(HostKey::Known)) => {
                                    ui.label(
                                        egui::RichText::new(t(
                                            lang,
                                            "\u{2713} Host key matches known_hosts",
                                            "\u{2713} 主机密钥与 known_hosts 一致",
                                        ))
                                        .color(green),
                                    );
                                }
                                Some(Ok(HostKey::Unknown { fingerprints })) => {
                                    ui.label(t(
                                        lang,
                                        "Unknown host key. Compare these fingerprints with the server's before trusting it:",
                                        "未知的主机密钥。信任前请与服务器上的指纹核对:",
                                    ));
                                    for f in fingerprints {
                                        ui.label(egui::RichText::new(f).monospace().size(11.0));
                                    }
                                    if fingerprints.is_empty() {
                                        ui.label(t(
                                            lang,
                                            "(behind a jump host: connect once in a tab to see and accept it)",
                                            "(经跳板机连接:请先在标签中连接一次以查看并接受)",
                                        ));
                                    } else if ui
                                        .button(t(lang, "Fingerprints match — trust", "指纹一致 —— 信任"))
                                        .clicked()
                                    {
                                        trust_key = Some(*i);
                                    }
                                }
                                Some(Ok(HostKey::Changed { fingerprints })) => {
                                    ui.label(
                                        egui::RichText::new(t(
                                            lang,
                                            "HOST KEY CHANGED. This can mean someone is intercepting the connection. Not trusted; verify with the server's administrator, then fix known_hosts (ssh-keygen -R).",
                                            "主机密钥已变化。这可能意味着连接被拦截。不会信任;请与服务器管理员核实后再处理 known_hosts(ssh-keygen -R)。",
                                        ))
                                        .color(red),
                                    );
                                    for f in fingerprints {
                                        ui.label(egui::RichText::new(f).monospace().size(11.0));
                                    }
                                }
                                Some(Err(e)) => {
                                    ui.label(egui::RichText::new(e).color(red));
                                }
                            }
                        }
                    }
                });
                ui.separator();
                ui.collapsing(t(lang, "Add host", "添加主机"), |ui| {
                    let field = |ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str| {
                        ui.horizontal(|ui| {
                            ui.label(label);
                            ui.add(egui::TextEdit::singleline(value).hint_text(hint).desired_width(220.0));
                        });
                    };
                    let opt = |o: &mut Option<String>| o.take().unwrap_or_default();
                    let mut address = opt(&mut view.form.address);
                    let mut user = opt(&mut view.form.user);
                    let mut group = opt(&mut view.form.group);
                    let mut jump = opt(&mut view.form.jump);
                    field(ui, t(lang, "Name", "名称"), &mut view.form.name, "web-1");
                    field(ui, t(lang, "Address", "地址"), &mut address, "203.0.113.5");
                    field(ui, t(lang, "User", "用户"), &mut user, "deploy");
                    field(ui, t(lang, "Port", "端口"), &mut view.form_port, "22");
                    field(ui, t(lang, "Group", "分组"), &mut group, "prod");
                    field(ui, t(lang, "Jump host", "跳板机"), &mut jump, "bastion");
                    let some = |v: String| Some(v.trim().to_string()).filter(|v| !v.is_empty());
                    view.form.address = some(address);
                    view.form.user = some(user);
                    view.form.group = some(group);
                    view.form.jump = some(jump);
                    let port_ok = view.form_port.trim().is_empty()
                        || view.form_port.trim().parse::<u16>().is_ok();
                    let valid = !view.form.name.trim().is_empty()
                        && view.form.address.is_some()
                        && port_ok;
                    if ui
                        .add_enabled(valid, egui::Button::new(t(lang, "Save host", "保存主机")))
                        .clicked()
                    {
                        add = true;
                    }
                });
                ui.label(
                    egui::RichText::new(t(
                        lang,
                        "Passwords and keys are never stored; ssh-agent and ~/.ssh/config are used.",
                        "不保存密码和密钥;使用 ssh-agent 与 ~/.ssh/config。",
                    ))
                    .size(11.0)
                    .color(chrome_rgb(self.theme.chrome().muted)),
                );
            });
        if let Some(i) = connect {
            if let Some(host) = self.host_book.hosts.get(i).cloned() {
                self.open_host(&host);
            }
        }
        if let Some((h, r, start)) = toggle_forward {
            if let Some(host) = self.host_book.hosts.get(h).cloned() {
                if let Some(fwd) = host.forwards.get(r) {
                    let key = (host.name.clone(), fwd.spec.clone());
                    if start {
                        match mtty_ui::forward::Tunnel::start(
                            &host.destination(),
                            &host.ssh_options(),
                            fwd,
                        ) {
                            Ok(tunnel) => {
                                self.tunnels.insert(key, tunnel);
                            }
                            Err(e) => self.show_notice(format!("{}: {e}", fwd.spec)),
                        }
                    } else {
                        self.tunnels.remove(&key);
                    }
                }
            }
        }
        if let Some((h, r)) = remove_forward {
            if let Some(host) = self.host_book.hosts.get_mut(h) {
                if r < host.forwards.len() {
                    let removed = host.forwards.remove(r);
                    let name = host.name.clone();
                    self.tunnels.remove(&(name, removed.spec.clone()));
                    if !self.save_hosts() {
                        if let Some(host) = self.host_book.hosts.get_mut(h) {
                            host.forwards.insert(r, removed);
                        }
                    }
                }
            }
        }
        if let Some(h) = add_forward {
            if let Some((_, kind, spec)) = view.new_forward.take() {
                let fwd = mtty_config::hosts::Forward {
                    kind,
                    spec: spec.trim().to_string(),
                };
                if let Some(host) = self.host_book.hosts.get_mut(h) {
                    if !host.forwards.contains(&fwd) {
                        host.forwards.push(fwd);
                        self.save_hosts();
                    }
                }
            }
        }
        if let Some((i, tmux, mosh)) = persist {
            if let Some(host) = self.host_book.hosts.get_mut(i) {
                let before = (host.tmux.clone(), host.mosh);
                host.tmux = tmux;
                host.mosh = mosh;
                if !self.save_hosts() {
                    if let Some(host) = self.host_book.hosts.get_mut(i) {
                        (host.tmux, host.mosh) = before;
                    }
                }
            }
        }
        if let Some(host) = open_files.and_then(|i| self.host_book.hosts.get(i).cloned()) {
            self.open_sftp(
                host.name.clone(),
                mtty_ui::sftp::Remote {
                    destination: host.destination(),
                    options: host.ssh_options(),
                },
            );
        }
        if let Some(host) = check_key.and_then(|i| self.host_book.hosts.get(i).cloned()) {
            view.keys.insert(host.name.clone(), None);
            self.spawn_job(move || {
                let result = mtty_ui::hostkeys::check(&host.destination(), &host.ssh_options());
                JobDone::HostKeyChecked {
                    name: host.name,
                    result,
                }
            });
        }
        if let Some(host) = trust_key.and_then(|i| self.host_book.hosts.get(i).cloned()) {
            view.keys.insert(host.name.clone(), None);
            self.spawn_job(move || {
                let (dest, opts) = (host.destination(), host.ssh_options());
                let result = mtty_ui::hostkeys::trust(&dest, &opts)
                    .and_then(|()| mtty_ui::hostkeys::check(&dest, &opts));
                JobDone::HostKeyChecked {
                    name: host.name,
                    result,
                }
            });
        }
        if let Some(host) = copy_key.and_then(|i| self.host_book.hosts.get(i).cloned()) {
            let syn = mtty_ui::ssh::Syntax::local();
            let mut cmd = String::from("ssh-copy-id");
            for (flag, value) in host
                .ssh_options()
                .chunks(2)
                .filter_map(|c| Some((c.first()?, c.get(1)?)))
            {
                match flag.as_str() {
                    "-p" => cmd.push_str(&format!(" -p {}", syn.quote(value))),
                    "-J" => {
                        cmd.push_str(&format!(" -o {}", syn.quote(&format!("ProxyJump={value}"))))
                    }
                    _ => {}
                }
            }
            cmd.push(' ');
            cmd.push_str(&syn.quote(&host.destination()));
            self.run_in_new_tab(&format!("ssh-copy-id {}", host.name), &cmd);
        }
        if new_key {
            self.run_in_new_tab(
                t(lang, "New SSH key", "生成 SSH 密钥"),
                "ssh-keygen -t ed25519 -C mtty",
            );
        }
        if import_key {
            self.key_import = Some(KeyImportDialog::default());
        }
        if let Some(i) = delete {
            if i < self.host_book.hosts.len() {
                let removed = self.host_book.hosts.remove(i);
                if !self.save_hosts() {
                    self.host_book.hosts.insert(i, removed);
                }
            }
            view.confirm_delete = None;
        }
        if import {
            let found = mtty_config::hosts::read_ssh_config()
                .map(|text| mtty_config::hosts::parse_ssh_config(&text))
                .unwrap_or_default();
            let total = found.len();
            let before = self.host_book.clone();
            let added = self.host_book.merge(found);
            if added > 0 && !self.save_hosts() {
                self.host_book = before;
            } else {
                let msg = format!(
                    "{} {added} / {total}",
                    t(
                        lang,
                        "Imported hosts from ~/.ssh/config:",
                        "已从 ~/.ssh/config 导入主机:"
                    )
                );
                self.show_notice(msg);
            }
        }
        if add {
            let mut host = std::mem::take(&mut view.form);
            host.name = host.name.trim().to_string();
            host.port = view.form_port.trim().parse().ok();
            view.form_port.clear();
            if let Some(existing) = self
                .host_book
                .hosts
                .iter_mut()
                .find(|h| h.name == host.name)
            {
                *existing = host;
            } else {
                self.host_book.hosts.push(host);
            }
            self.save_hosts();
        }
        if open {
            self.hosts_view = Some(view);
        }
    }

    /// List the tasks of the repository the window shows, in the background.
    fn reload_tasks(&mut self) {
        let Some(view) = self.tasks_view.as_mut() else {
            return;
        };
        view.loading = true;
        let repo = view.repo.clone();
        self.spawn_job(move || {
            let result = mtty_ui::tasks::list(&repo);
            JobDone::TasksListed { repo, result }
        });
    }

    fn task_dialog_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let Some((mut name, mut agent)) = self.task_dialog.take() else {
            return;
        };
        let lang = self.lang;
        let detected = self
            .agents_detected
            .as_ref()
            .map(|(_, d)| d.clone())
            .unwrap_or_default();
        let mut open = true;
        let mut create = false;
        egui::Window::new(t(lang, "New Agent Task", "新建 Agent 任务"))
            .collapsible(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(t(
                    lang,
                    "A git worktree and branch of the current repository, with its own tab.",
                    "当前仓库的一个 git worktree 与分支,并在独立标签中打开。",
                ));
                let r = ui.add(
                    egui::TextEdit::singleline(&mut name)
                        .hint_text(t(lang, "task name, e.g. fix-login", "任务名,如 fix-login"))
                        .desired_width(260.0),
                );
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if !enter {
                    r.request_focus();
                }
                ui.horizontal(|ui| {
                    ui.radio_value(&mut agent, None, t(lang, "shell only", "仅 shell"));
                    for (i, a) in mtty_ui::integration::AGENTS.iter().enumerate() {
                        if detected.get(i).copied().unwrap_or(false) {
                            ui.radio_value(&mut agent, Some(i), a.name);
                        }
                    }
                });
                let valid = mtty_ui::tasks::valid_name(&name);
                if !name.is_empty() && !valid {
                    ui.label(
                        egui::RichText::new(t(
                            lang,
                            "Use letters, digits, '-', '_' or '.'.",
                            "只能使用字母、数字、'-'、'_' 或 '.'。",
                        ))
                        .color(chrome_rgb(self.theme.chrome().negative)),
                    );
                }
                if ui
                    .add_enabled(valid, egui::Button::new(t(lang, "Create", "创建")))
                    .clicked()
                    || (enter && valid)
                {
                    create = true;
                }
            });
        if create {
            let Some(dir) = self.cwd() else {
                return;
            };
            let msg = format!(
                "{} {}…",
                t(lang, "Creating task", "正在创建任务"),
                name.trim()
            );
            self.show_notice(msg);
            self.spawn_job(move || {
                let result = mtty_ui::tasks::create(&dir, &name);
                JobDone::TaskCreated { result, agent }
            });
        } else if open && !ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.task_dialog = Some((name, agent));
        }
    }

    fn tasks_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let Some(view) = self.tasks_view.as_mut() else {
            return;
        };
        let mut open = true;
        let mut action: Option<(usize, &'static str)> = None;
        app_window(t(lang, "Agent Tasks", "Agent 任务"), ctx)
            .collapsible(false)
            .open(&mut open)
            .default_size([560.0, 420.0])
            .show(ctx, |ui| {
                window_body(ui, |ui| {
                    ui.label(
                        egui::RichText::new(view.repo.display().to_string())
                            .size(11.0)
                            .color(chrome_rgb(self.theme.chrome().muted)),
                    );
                    if view.loading {
                        ui.label(t(lang, "Loading…", "加载中…"));
                    } else if view.tasks.is_empty() {
                        ui.label(t(
                            lang,
                            "No tasks in this repository.",
                            "这个仓库还没有任务。",
                        ));
                    }
                    for (i, task) in view.tasks.iter().enumerate() {
                        ui.separator();
                        ui.label(egui::RichText::new(&task.name).strong());
                        ui.label(
                            egui::RichText::new(format!("{} \u{2190} {}", task.branch, task.base))
                                .size(11.0)
                                .color(chrome_rgb(self.theme.chrome().muted)),
                        );
                        ui.horizontal(|ui| {
                            if ui.button(t(lang, "Open", "打开")).clicked() {
                                action = Some((i, "open"));
                            }
                            if ui.button(t(lang, "Diff", "查看改动")).clicked() {
                                action = Some((i, "diff"));
                            }
                            match view.confirm {
                                Some((c, merge)) if c == i => {
                                    let label = if merge {
                                        t(lang, "Confirm merge", "确认合并")
                                    } else {
                                        t(
                                            lang,
                                            "Confirm discard (deletes its work)",
                                            "确认丢弃(删除其改动)",
                                        )
                                    };
                                    if ui
                                        .button(
                                            egui::RichText::new(label)
                                                .color(chrome_rgb(self.theme.chrome().warning)),
                                        )
                                        .clicked()
                                    {
                                        action = Some((i, if merge { "merge" } else { "discard" }));
                                    }
                                    if ui.button(t(lang, "Cancel", "取消")).clicked() {
                                        action = Some((i, "cancel"));
                                    }
                                }
                                _ => {
                                    if ui.button(t(lang, "Merge…", "合并…")).clicked() {
                                        action = Some((i, "ask-merge"));
                                    }
                                    if ui.button(t(lang, "Discard…", "丢弃…")).clicked() {
                                        action = Some((i, "ask-discard"));
                                    }
                                }
                            }
                        });
                    }
                });
            });
        if !open {
            self.tasks_view = None;
            return;
        }
        let Some((i, what)) = action else {
            return;
        };
        let Some(task) = view.tasks.get(i).cloned() else {
            return;
        };
        match what {
            "ask-merge" => view.confirm = Some((i, true)),
            "ask-discard" => view.confirm = Some((i, false)),
            "cancel" => view.confirm = None,
            "open" => {
                self.new_tab_in(Some(task.path.clone()));
                if let Some(tab) = self.tabs.last_mut() {
                    tab.title = format!("task: {}", task.name);
                    tab.title_set = true;
                }
                self.publish_panes();
            }
            "diff" => self.spawn_job(move || {
                let result = mtty_ui::tasks::diff(&task);
                JobDone::TaskDiff {
                    name: task.name.clone(),
                    result,
                }
            }),
            "merge" | "discard" => {
                view.confirm = None;
                let merged = what == "merge";
                self.spawn_job(move || {
                    let result = if merged {
                        mtty_ui::tasks::merge(&task)
                    } else {
                        mtty_ui::tasks::discard(&task).map(|()| String::new())
                    };
                    JobDone::TaskDone {
                        name: task.name.clone(),
                        merged,
                        result,
                    }
                });
            }
            _ => {}
        }
    }

    /// The New SSH Session form: a full host editor with a quick-connect path.
    fn ssh_dialog_window(&mut self, ctx: &egui::Context) {
        use mtty_config::hosts::ForwardKind;
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let Some(mut form) = self.ssh_dialog.take() else {
            return;
        };
        let mut open = true;
        // 0 keep, 1 connect (no save), 2 save & connect, 3 cancel.
        let mut action = 0u8;
        let mut remove_forward: Option<usize> = None;
        egui::Window::new(t(lang, "New SSH Session", "新建 SSH 会话"))
            .collapsible(false)
            .open(&mut open)
            .default_width(460.0)
            .show(ctx, |ui| {
                let field = |ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str| {
                    ui.horizontal(|ui| {
                        ui.add_sized([88.0, 18.0], egui::Label::new(label));
                        ui.add(
                            egui::TextEdit::singleline(value)
                                .hint_text(hint)
                                .desired_width(f32::INFINITY),
                        );
                    });
                };
                ui.label(egui::RichText::new(t(lang, "Quick connect", "快速连接")).strong());
                let quick = ui.add(
                    egui::TextEdit::singleline(&mut form.quick)
                        .hint_text("[user@]host[:port]")
                        .desired_width(f32::INFINITY),
                );
                if quick.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    action = 1;
                }
                ui.horizontal(|ui| {
                    if ui.button(t(lang, "Connect", "连接")).clicked() {
                        action = 1;
                    }
                    ui.label(
                        egui::RichText::new(t(
                            lang,
                            "Connects the typed target without saving.",
                            "直接连接,不保存。",
                        ))
                        .size(11.0)
                        .weak(),
                    );
                });
                ui.separator();
                ui.label(egui::RichText::new(t(lang, "Saved host", "保存主机")).strong());
                ui.checkbox(
                    &mut form.alias,
                    t(
                        lang,
                        "Use a ~/.ssh/config alias by name",
                        "按名称使用 ~/.ssh/config 别名",
                    ),
                );
                field(ui, t(lang, "Name", "名称"), &mut form.name, "web-1");
                if !form.alias {
                    field(ui, t(lang, "Host", "主机"), &mut form.address, "203.0.113.5");
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [88.0, 18.0],
                            egui::Label::new(t(lang, "User / port", "用户 / 端口")),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut form.user)
                                .hint_text("deploy")
                                .desired_width(140.0),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut form.port)
                                .hint_text("22")
                                .desired_width(64.0),
                        );
                    });
                    field(ui, t(lang, "Group", "分组"), &mut form.group, "prod");
                    field(ui, t(lang, "Tags", "标签"), &mut form.tags, "web, edge");
                }
                ui.checkbox(&mut form.advanced, t(lang, "Advanced options", "高级选项"));
                if form.advanced {
                    if !form.alias {
                        field(ui, t(lang, "Jump host", "跳板机"), &mut form.jump, "bastion");
                    }
                    ui.horizontal(|ui| {
                        ui.checkbox(
                            &mut form.persistent,
                            t(lang, "Keep the shell in tmux", "在 tmux 中保持 shell"),
                        );
                        if form.persistent {
                            ui.add(
                                egui::TextEdit::singleline(&mut form.tmux)
                                    .hint_text("mtty")
                                    .desired_width(120.0),
                            );
                        }
                    });
                    ui.checkbox(
                        &mut form.mosh,
                        t(
                            lang,
                            "Connect with mosh (survives sleep)",
                            "使用 mosh 连接(休眠不断线)",
                        ),
                    );
                    let title = format!(
                        "{} ({})",
                        t(lang, "Port forwards", "端口转发"),
                        form.forwards.len()
                    );
                    ui.collapsing(title, |ui| {
                        for (i, fwd) in form.forwards.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.monospace(format!("{} {}", fwd.kind.flag(), fwd.spec));
                                if ui.small_button(t(lang, "Remove", "删除")).clicked() {
                                    remove_forward = Some(i);
                                }
                            });
                        }
                        let draft = form
                            .new_forward
                            .get_or_insert_with(|| (ForwardKind::Local, String::new()));
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt("ssh-fwd-kind")
                                .selected_text(draft.0.flag())
                                .width(80.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut draft.0, ForwardKind::Local, "-L local");
                                    ui.selectable_value(&mut draft.0, ForwardKind::Remote, "-R remote");
                                    ui.selectable_value(&mut draft.0, ForwardKind::Dynamic, "-D SOCKS");
                                });
                            let hint = if draft.0 == ForwardKind::Dynamic {
                                "1080"
                            } else {
                                "8080:localhost:80"
                            };
                            ui.add(
                                egui::TextEdit::singleline(&mut draft.1)
                                    .hint_text(hint)
                                    .desired_width(190.0),
                            );
                            let candidate = mtty_config::hosts::Forward {
                                kind: draft.0,
                                spec: draft.1.trim().to_string(),
                            };
                            let check = candidate.validate();
                            if ui
                                .add_enabled(check.is_ok(), egui::Button::new(t(lang, "Add", "添加")))
                                .clicked()
                            {
                                form.forwards.push(candidate);
                                draft.1.clear();
                            }
                        });
                    });
                }
                if let Some(e) = &form.error {
                    ui.colored_label(chrome_rgb(self.theme.chrome().negative), e.as_str());
                }
                ui.horizontal(|ui| {
                    if ui.button(t(lang, "Cancel", "取消")).clicked() {
                        action = 3;
                    }
                    let ready =
                        form.alias || !form.address.trim().is_empty() || !form.name.trim().is_empty();
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(t(lang, "Save & Connect", "保存并连接")),
                        )
                        .clicked()
                    {
                        action = 2;
                    }
                });
                ui.label(
                    egui::RichText::new(t(
                        lang,
                        "Passwords and keys are never stored; ssh-agent and ~/.ssh/config are used.",
                        "不保存密码和密钥;使用 ssh-agent 与 ~/.ssh/config。",
                    ))
                    .size(11.0)
                    .weak(),
                );
            });
        if let Some(i) = remove_forward {
            form.forwards.remove(i);
        }
        match action {
            1 => {
                if !form.quick.trim().is_empty() {
                    let target = form.quick.trim().to_string();
                    self.ssh_dialog = None;
                    self.open_ssh(&target);
                    return;
                }
                match form.build(lang) {
                    Ok(host) => {
                        self.ssh_dialog = None;
                        self.open_ssh_host(&host);
                    }
                    Err(e) => {
                        form.error = Some(e);
                        self.ssh_dialog = Some(form);
                    }
                }
            }
            2 => match form.build(lang) {
                Ok(host) => {
                    match self
                        .host_book
                        .hosts
                        .iter()
                        .position(|h| h.name == host.name)
                    {
                        Some(pos) => self.host_book.hosts[pos] = host.clone(),
                        None => self.host_book.hosts.push(host.clone()),
                    }
                    self.save_hosts();
                    self.ssh_dialog = None;
                    self.open_ssh_host(&host);
                }
                Err(e) => {
                    form.error = Some(e);
                    self.ssh_dialog = Some(form);
                }
            },
            3 => self.ssh_dialog = None,
            _ => {
                if open {
                    self.ssh_dialog = Some(form);
                } else {
                    self.ssh_dialog = None;
                }
            }
        }
    }

    /// The New Serial/Telnet/TCP Session form (ADR 0037).
    fn transport_dialog_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let Some(dlg) = self.transport_dialog.as_mut() else {
            return;
        };
        const BAUD: &str = "115200";
        let mut open = true;
        let mut connect = false;
        egui::Window::new(t(
            lang,
            "New Serial/Telnet/TCP Session",
            "新建串口/Telnet/TCP 会话",
        ))
        .collapsible(false)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut dlg.kind, 0, t(lang, "Serial", "串口"));
                ui.selectable_value(&mut dlg.kind, 1, "Telnet");
                ui.selectable_value(&mut dlg.kind, 2, t(lang, "Raw TCP", "裸 TCP"));
            });
            if dlg.kind == 0 {
                ui.horizontal(|ui| {
                    ui.label(t(lang, "Device", "设备"));
                    ui.add(
                        egui::TextEdit::singleline(&mut dlg.device)
                            .hint_text("/dev/ttyUSB0")
                            .desired_width(170.0),
                    );
                    if ui.button(t(lang, "List", "列出")).clicked() {
                        dlg.error = Some(mtty_ui::transport::serial_ports().join("  "));
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(t(lang, "Baud", "波特率"));
                    ui.add(
                        egui::TextEdit::singleline(&mut dlg.baud)
                            .hint_text(BAUD)
                            .desired_width(74.0),
                    );
                    ui.label(t(lang, "Data bits", "数据位"));
                    egui::ComboBox::from_id_salt(("transport-data",))
                        .selected_text(["5", "6", "7", "8"][dlg.data_bits.min(3)])
                        .width(44.0)
                        .show_ui(ui, |ui| {
                            for (i, n) in ["5", "6", "7", "8"].iter().enumerate() {
                                ui.selectable_value(&mut dlg.data_bits, i, *n);
                            }
                        });
                    ui.label(t(lang, "Parity", "校验"));
                    egui::ComboBox::from_id_salt(("transport-parity",))
                        .selected_text(["none", "odd", "even"][dlg.parity.min(2)])
                        .width(74.0)
                        .show_ui(ui, |ui| {
                            for (i, n) in ["none", "odd", "even"].iter().enumerate() {
                                ui.selectable_value(&mut dlg.parity, i, *n);
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label(t(lang, "Stop bits", "停止位"));
                    egui::ComboBox::from_id_salt(("transport-stop",))
                        .selected_text(["1", "2"][dlg.stop_bits.min(1)])
                        .width(44.0)
                        .show_ui(ui, |ui| {
                            for (i, n) in ["1", "2"].iter().enumerate() {
                                ui.selectable_value(&mut dlg.stop_bits, i, *n);
                            }
                        });
                    ui.label(t(lang, "Flow", "流控"));
                    egui::ComboBox::from_id_salt(("transport-flow",))
                        .selected_text(["none", "software", "hardware"][dlg.flow.min(2)])
                        .width(88.0)
                        .show_ui(ui, |ui| {
                            for (i, n) in ["none", "software", "hardware"].iter().enumerate() {
                                ui.selectable_value(&mut dlg.flow, i, *n);
                            }
                        });
                });
            } else {
                ui.horizontal(|ui| {
                    ui.label(t(lang, "Host", "主机"));
                    ui.add(egui::TextEdit::singleline(&mut dlg.host).desired_width(180.0));
                    ui.label(t(lang, "Port", "端口"));
                    ui.add(
                        egui::TextEdit::singleline(&mut dlg.port)
                            .hint_text(if dlg.kind == 1 { "23" } else { "0" })
                            .desired_width(64.0),
                    );
                });
            }
            if let Some(err) = &dlg.error {
                ui.colored_label(chrome_rgb(self.theme.chrome().negative), err);
            }
            if ui.button(t(lang, "Connect", "连接")).clicked() {
                connect = true;
            }
        });
        let mut submit: Option<TransportTarget> = None;
        if connect {
            submit = match dlg.kind {
                0 => match dlg.baud.trim().parse::<u32>() {
                    Ok(baud) if !dlg.device.trim().is_empty() => Some(TransportTarget::Serial {
                        device: dlg.device.trim().to_string(),
                        baud,
                        data_bits: [5u8, 6, 7, 8][dlg.data_bits.min(3)],
                        parity: ["none", "odd", "even"][dlg.parity.min(2)].to_string(),
                        stop_bits: (dlg.stop_bits.min(1) + 1) as u8,
                        flow: ["none", "software", "hardware"][dlg.flow.min(2)].to_string(),
                    }),
                    _ => {
                        dlg.error = Some(
                            t(
                                lang,
                                "Enter a device and a baud rate.",
                                "请输入设备与波特率。",
                            )
                            .into(),
                        );
                        None
                    }
                },
                kind => match dlg.port.trim().parse::<u16>() {
                    Ok(port) if !dlg.host.trim().is_empty() => {
                        let host = dlg.host.trim().to_string();
                        Some(if kind == 1 {
                            TransportTarget::Telnet { host, port }
                        } else {
                            TransportTarget::Tcp { host, port }
                        })
                    }
                    _ => {
                        dlg.error =
                            Some(t(lang, "Enter a host and a port.", "请输入主机与端口。").into());
                        None
                    }
                },
            };
        }
        if let Some(target) = submit {
            self.transport_dialog = None;
            self.connect_transport(target, None);
        } else if !open {
            self.transport_dialog = None;
        }
    }

    fn remote_dialog_window(&mut self, ctx: &egui::Context) {
        let Some((dest, path)) = self.remote_dialog.as_mut() else {
            return;
        };
        let mut open = true;
        let mut do_open = false;
        egui::Window::new(mtty_ui::i18n::t(
            self.lang,
            "Open Remote File",
            "打开远端文件",
        ))
        .collapsible(false)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("SSH");
                ui.add(
                    egui::TextEdit::singleline(dest)
                        .hint_text(mtty_ui::i18n::t(self.lang, "host", "主机"))
                        .desired_width(140.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label(mtty_ui::i18n::t(self.lang, "Path", "路径"));
                ui.add(
                    egui::TextEdit::singleline(path)
                        .hint_text("/etc/hosts")
                        .desired_width(240.0),
                );
            });
            if ui
                .button(mtty_ui::i18n::t(self.lang, "Open", "打开"))
                .clicked()
            {
                do_open = true;
            }
        });
        if do_open {
            let (dest, path) = (dest.clone(), path.clone());
            self.remote_dialog = None;
            let msg = format!(
                "{} {dest}:{path}…",
                mtty_ui::i18n::t(self.lang, "Opening", "正在打开")
            );
            self.show_notice(msg);
            self.spawn_job(move || {
                let result = mtty_ui::ssh::read_remote(&dest, &path);
                JobDone::RemoteRead {
                    id: None,
                    cursor: 0,
                    scroll: 0,
                    dest,
                    path,
                    result,
                }
            });
        } else if !open {
            self.remote_dialog = None;
        }
    }

    fn recipe_dialog_window(&mut self, ctx: &egui::Context) {
        let Some(save) = self.recipe_dialog else {
            return;
        };
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let mut open = true;
        let mut do_save = false;
        let mut open_recipe: Option<String> = None;
        let title = if save {
            t(lang, "Save Recipe", "保存配方")
        } else {
            t(lang, "Open Recipe", "打开配方")
        };
        egui::Window::new(title)
            .collapsible(false)
            .open(&mut open)
            .show(ctx, |ui| {
                if save {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.recipe_name)
                            .hint_text(mtty_ui::i18n::t(self.lang, "name", "名称"))
                            .desired_width(240.0),
                    );
                    r.request_focus();
                    if ui.button(t(lang, "Save", "保存")).clicked() {
                        do_save = true;
                    }
                } else if self.recipe_list.is_empty() {
                    ui.label(t(lang, "No recipes yet", "还没有配方"));
                } else {
                    for name in &self.recipe_list {
                        if ui.button(name).clicked() {
                            open_recipe = Some(name.clone());
                        }
                    }
                }
            });
        if do_save {
            match recipe_file_name(&self.recipe_name) {
                Err(why) => self.show_notice(t(lang, why.0, why.1).to_string()),
                Ok(file) => {
                    let result = recipes_dir()
                        .ok_or_else(|| std::io::Error::other("no config directory"))
                        .and_then(|dir| {
                            std::fs::create_dir_all(&dir)?;
                            let data = serde_json::to_vec(&self.session_value())
                                .map_err(std::io::Error::other)?;
                            std::fs::write(dir.join(file), data)
                        });
                    let msg = match result {
                        Ok(()) => format!(
                            "{} {}",
                            t(lang, "Saved recipe", "已保存配方"),
                            self.recipe_name.trim()
                        ),
                        Err(e) => format!("{}: {e}", t(lang, "Recipe not saved", "配方未保存")),
                    };
                    self.show_notice(msg);
                    self.recipe_dialog = None;
                }
            }
        }
        if let Some(name) = open_recipe {
            let loaded = recipes_dir()
                .ok_or_else(|| "no config directory".to_string())
                .and_then(|dir| {
                    std::fs::read(dir.join(format!("{name}.json"))).map_err(|e| e.to_string())
                })
                .and_then(|bytes| {
                    serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|e| e.to_string())
                });
            match loaded {
                Ok(v) => {
                    self.clear_tabs();
                    if !self.restore_from_value(&v) {
                        self.new_tab();
                    }
                    // Recipes do not go through `fit_all_panes`; finish any
                    // restored notes (and images) here.
                    self.apply_restored_images();
                }
                Err(e) => {
                    let msg = format!(
                        "{} {name}: {e}",
                        t(lang, "Could not open recipe", "无法打开配方")
                    );
                    self.show_notice(msg);
                }
            }
            self.recipe_dialog = None;
        }
        if !open {
            self.recipe_dialog = None;
        }
    }

    /// Open a local file in the built-in editor. Returns false if it can't be
    /// read. Resets the vim runtime so a new file starts in Normal mode.
    fn active_editor(&self) -> Option<&editor_pane::EditorPane> {
        let tab = self.tabs.get(self.active_tab)?;
        tab.editors.iter().find(|e| e.id == tab.active)
    }

    fn active_editor_mut(&mut self) -> Option<&mut editor_pane::EditorPane> {
        let tab = self.tabs.get_mut(self.active_tab)?;
        let active = tab.active.clone();
        tab.editors.iter_mut().find(|e| e.id == active)
    }

    /// The text-area cell of the active editor pane under a pointer position
    /// (physical pixels). With `clamp`, a position outside the pane maps to
    /// its nearest cell (a drag past an edge); otherwise it must be inside.
    fn editor_cell(&self, px: f32, py: f32, clamp: bool) -> Option<(usize, usize)> {
        let scale = self.window.scale_factor() as f32;
        let (x, y) = (px / scale, py / scale);
        let tab = self.tabs.get(self.active_tab)?;
        let (id, r) = if clamp {
            self.pane_rects()
                .into_iter()
                .find(|(id, _)| *id == tab.active)?
        } else {
            self.pane_rects()
                .into_iter()
                .find(|(_, r)| r.contains(x, y))?
        };
        let ed = tab.editors.iter().find(|e| e.id == id)?;
        if ed.markdown.is_some() {
            return None;
        }
        let inner = card_inner(r);
        let col = ((x - inner.x) / self.cw).floor();
        let row = ((y - inner.y) / self.ch).floor();
        let col = (col - ed.gutter() as f32).max(0.0) as usize;
        // Past the bottom maps to the row just below the view, so a drag
        // there scrolls a line at a time (the pane reveals the caret); above
        // the top stops at the first row.
        let row = (row.max(0.0) as usize).min(ed.rows);
        Some((row, col))
    }

    /// Editor-only commands from the palette or menu: true with an editor
    /// pane active, otherwise a notice says what they need.
    fn require_editor(&mut self) -> bool {
        if self.active_editor().is_some() {
            return true;
        }
        self.show_notice(
            mtty_ui::i18n::t(
                self.lang,
                "This works in a file opened in an editor pane.",
                "此操作用于在编辑器 pane 中打开的文件。",
            )
            .to_string(),
        );
        false
    }

    /// QA: run a palette command by its English label (`MTTY_QA_COMMAND`),
    /// once, so windows that only open from the palette can be captured.
    fn run_qa_command(&mut self) {
        if self.qa_done {
            return;
        }
        self.qa_done = true;
        // `MTTY_QA_DRAG=<x>,<y>[,alt]:<path>[;<path>…]` holds a file drag at
        // a window point (logical) for a capture; with `MTTY_QA_DROP=1` the
        // files are dropped there (synthetic drags cannot be posted).
        if let Some(spec) = mtty_config::env("QA_DRAG") {
            if let Some((at, paths)) = spec.split_once(':') {
                let mut parts = at.split(',');
                let x = parts.next().and_then(|v| v.trim().parse::<f64>().ok());
                let y = parts.next().and_then(|v| v.trim().parse::<f64>().ok());
                if let (Some(x), Some(y)) = (x, y) {
                    let scale = self.window.scale_factor();
                    self.cursor = (x * scale, y * scale);
                    self.qa_drag = Some(parts.next().is_some_and(|m| m.trim() == "alt"));
                    self.drag_paths = paths.split(';').map(std::path::PathBuf::from).collect();
                    self.drag_live = true;
                    self.dropping = true;
                    self.drop_choice = None;
                    if mtty_config::env("QA_DROP").is_some() {
                        for path in self.drag_paths.clone() {
                            self.drop_file(path);
                        }
                    }
                }
            }
        }
        // `MTTY_QA_SCROLL=<lines>` scrolls the active pane back, like the
        // wheel, so scrollback behaviour can be captured without input events.
        if let Some(lines) =
            mtty_config::env("QA_SCROLL").and_then(|v| v.trim().parse::<usize>().ok())
        {
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                if let Some(pane) = tab.panes.iter_mut().find(|p| p.id == tab.active) {
                    pane.scroll = (pane.scroll + lines).min(pane.term.screen().scrollback_len());
                }
            }
        }
        // `MTTY_QA_MENU=<x>,<y>` opens the pane context menu at that logical
        // point, so the menu can be captured without a pointer event.
        if let Some(spec) = mtty_config::env("QA_MENU") {
            let mut parts = spec.split(',');
            let x = parts.next().and_then(|v| v.trim().parse::<f32>().ok());
            let y = parts.next().and_then(|v| v.trim().parse::<f32>().ok());
            if let (Some(x), Some(y)) = (x, y) {
                if let Some((id, outer)) = self
                    .pane_rects()
                    .into_iter()
                    .find(|(_, r)| r.contains(x, y))
                {
                    let inner = card_inner(outer);
                    let cell = pane_cell_at(inner, self.cw, self.ch, (x, y));
                    self.pane_menu = Some(PaneMenu {
                        pane: id,
                        at: (x, y),
                        cell,
                    });
                }
            }
        }
        let Some(label) = mtty_config::env("QA_COMMAND") else {
            return;
        };
        let lang = self.lang;
        self.lang = mtty_ui::i18n::Lang::En;
        let cmd = self
            .commands()
            .into_iter()
            .find(|(_, l)| l.eq_ignore_ascii_case(label.trim()))
            .map(|(c, _)| c);
        self.lang = lang;
        if let Some(cmd) = cmd {
            self.run_command(cmd);
        }
    }

    /// The pointer moved: over the active editor's text it starts (or
    /// keeps) resting on a word, for a hover; elsewhere the hover goes.
    fn track_hover(&mut self, px: f32, py: f32) {
        if self.egui_ctx.is_pointer_over_area() && self.hover.is_some() {
            // Over the popup itself: keep it.
            return;
        }
        let target = (|| {
            let (row, col) = self.editor_cell(px, py, false)?;
            let tab = self.tabs.get(self.active_tab)?;
            let ed = tab.editors.iter().find(|e| e.id == tab.active)?;
            // Only over the active pane's own text.
            let scale = self.window.scale_factor() as f32;
            let (_, r) = self
                .pane_rects()
                .into_iter()
                .find(|(id, _)| *id == tab.active)?;
            if !r.contains(px / scale, py / scale) || ed.dragging {
                return None;
            }
            let at = ed.char_under(row, col)?;
            let word = mtty_editor::motion::word_at(ed.doc.rope(), at);
            Some((ed.id.clone(), at, word))
        })();
        let Some((pane, at, word)) = target else {
            self.hover_rest = None;
            self.hover = None;
            return;
        };
        let same = self
            .hover_rest
            .as_ref()
            .is_some_and(|r| r.pane == pane && r.word == word && word.0 < word.1)
            || self
                .hover_rest
                .as_ref()
                .is_some_and(|r| r.pane == pane && r.at == at);
        if same {
            return;
        }
        self.hover = None;
        let scale = self.window.scale_factor() as f32;
        self.hover_rest = Some(HoverRest {
            pane,
            at,
            word,
            since: Instant::now(),
            pos: (px / scale, py / scale + self.ch * 0.5),
            asked: false,
        });
    }

    /// A key for the focused editor pane. False when it is the app's (⌘S,
    /// ⌘W, ⌘T…), so the shortcut path gets it.
    fn editor_key(&mut self, event: &KeyEvent) -> bool {
        // The live Markdown surface owns text input through egui. When no
        // block has focus, do not invisibly edit the underlying source grid.
        if self.active_editor().is_some_and(|ed| ed.markdown.is_some()) {
            return shortcut(event, self.mods).is_none();
        }
        let (shift, alt) = (self.mods.shift_key(), self.mods.alt_key());
        let (sup, ctrl) = (self.mods.super_key(), self.mods.control_key());
        // Chords match the key without modifiers: ⌥ turns ⇧⌥I into a dead
        // key and Ctrl+G into a control character.
        let kind = if alt || ctrl || sup {
            chord_key_kind(event)
        } else {
            winit_key_kind(event)
        };
        // Any key hides a hover; an open completion list takes its keys.
        self.hover = None;
        self.hover_rest = None;
        if self.completion_key(kind, alt || sup || ctrl) {
            return true;
        }
        // Vim mode owns the keyboard while it is on.
        if self.active_editor().and_then(|e| e.vim.as_ref()).is_some() {
            let action = match vim_key_from(kind, ctrl) {
                Some(key) => self.active_editor_mut().and_then(|ed| ed.vim_key(key)),
                None => None,
            };
            match action {
                Some(mtty_editor::vim::Action::Find) => self.run_command(Cmd::Find),
                Some(mtty_editor::vim::Action::CommandLine) => {
                    self.vim_command = Some(String::new());
                }
                Some(mtty_editor::vim::Action::Fold(fold)) => {
                    let command = match fold {
                        mtty_editor::vim::Fold::Toggle => editor_pane::Command::ToggleFold,
                        mtty_editor::vim::Fold::Close => editor_pane::Command::Fold,
                        mtty_editor::vim::Fold::Open => editor_pane::Command::Unfold,
                        mtty_editor::vim::Fold::CloseAll => editor_pane::Command::FoldAll,
                        mtty_editor::vim::Fold::OpenAll => editor_pane::Command::UnfoldAll,
                    };
                    self.run_editor_command(command);
                }
                _ => {}
            }
            self.refilter_completion();
            return true;
        }
        match editor_pane::keymap(kind, shift, alt, sup, ctrl) {
            Some(editor_pane::Command::GoToLine) => {
                self.run_command(Cmd::GoToLine);
                return true;
            }
            Some(editor_pane::Command::GoToSymbol) => {
                self.run_command(Cmd::GoToSymbol);
                return true;
            }
            Some(editor_pane::Command::FindReplace) => {
                self.run_command(Cmd::Replace);
                return true;
            }
            Some(editor_pane::Command::Complete) => {
                self.request_completion(None, true);
                return true;
            }
            Some(editor_pane::Command::GoToDefinition) => {
                self.request_definition();
                return true;
            }
            Some(command @ editor_pane::Command::NextProblem(_)) => {
                self.run_editor_command(command);
                // Show what the problem is, at the caret.
                let at = self
                    .active_editor()
                    .map(|e| e.doc.selection().primary().from());
                let pane = self.active_pane_id();
                if let (Some(at), Some(pane), Some(pos)) = (at, pane, self.caret_point()) {
                    self.show_hover(&pane, at, pos);
                }
                return true;
            }
            _ => {}
        }
        if let Some(command) = editor_pane::keymap(kind, shift, alt, sup, ctrl) {
            let handled = self.run_editor_command(command);
            // Moving or deleting narrows the list, or closes it.
            self.refilter_completion();
            return handled;
        }
        let read_only = self.read_only;
        let Some(ed) = self.active_editor_mut() else {
            return false;
        };
        // Editing a file in view mode asks first (see `large_edit_window`).
        let view = ed.is_view_only().then(|| ed.id.clone());
        if sup || ctrl {
            return false;
        }
        if matches!(kind, input::KeyKind::Char(_) | input::KeyKind::Other) {
            if let Some(text) = event.text.as_ref().filter(|t| !t.is_empty()) {
                if !text.chars().all(char::is_control) {
                    if let (Some(id), false) = (view, read_only) {
                        self.large_edit_offer = Some(id);
                    } else if !read_only {
                        ed.type_text(text);
                        let text = text.to_string();
                        self.after_typing(&text);
                    }
                    return true;
                }
            }
        }
        false
    }

    /// Run a command in the active editor pane: refused while read-only if it
    /// edits, and an edit to a file in view mode asks first. False without
    /// an editor pane.
    fn run_editor_command(&mut self, command: editor_pane::Command) -> bool {
        let read_only = self.read_only;
        let Some(ed) = self.active_editor_mut() else {
            return false;
        };
        let edits = editor_pane::EditorPane::edits(command);
        if edits && !read_only && ed.is_view_only() {
            // Editing a file in view mode asks first (see `large_edit_window`).
            self.large_edit_offer = Some(ed.id.clone());
            return true;
        }
        if !(read_only && edits) {
            ed.run(command);
        }
        true
    }

    /// A menu command whose shortcut is also an editor chord (⌘D, ⇧⌘Z,
    /// ⇧⌘L), pressed while an editor pane has the keyboard: the OS menu bar
    /// sees the key before the window does, so hand it to the editor as the
    /// key path would (ADR 0034). True when the editor took it.
    #[cfg(target_os = "macos")]
    fn menu_key_for_editor(&mut self, id: chrome::MenuId) -> bool {
        if !editor_takes_keys(
            self.active_editor().is_some(),
            self.hint_mode,
            self.egui_ctx.wants_keyboard_input(),
        ) {
            return false;
        }
        match editor_command_for_menu_key(id) {
            Some(command) => self.run_editor_command(command),
            None => false,
        }
    }

    /// Open `path` in an editor pane in a new tab, or switch to the pane
    /// that already has it.
    fn open_editor_pane(&mut self, path: &std::path::Path) -> bool {
        self.open_editor_pane_impl(path, false)
    }

    fn open_editor_pane_impl(&mut self, path: &std::path::Path, create: bool) -> bool {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        // The same file by another name (a symlinked /tmp, as a language
        // server may report it) is the same pane.
        let real = std::fs::canonicalize(&path).ok();
        let open = self.tabs.iter().enumerate().find_map(|(ti, tab)| {
            tab.editors
                .iter()
                .find(|e| {
                    e.path == path
                        || real.is_some()
                            && std::fs::canonicalize(&e.path).ok().as_ref() == real.as_ref()
                })
                .map(|e| (ti, e.id.clone()))
        });
        if let Some((ti, id)) = open {
            self.tabs[ti].active = id;
            self.active_tab = ti;
            self.selection = None;
            return true;
        }
        let id = gen_id();
        let opened = if create
            && std::fs::metadata(&path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            Ok(editor_pane::EditorPane::with_doc(
                id.clone(),
                path.clone(),
                mtty_editor::Document::from_text(""),
            ))
        } else {
            editor_pane::EditorPane::open(id.clone(), &path)
        };
        match opened {
            Ok(mut ed) => {
                ed.set_vim(self.editor_vim);
                let title = ed.title();
                self.tabs.push(Tab {
                    layout: Layout::leaf(id.clone()),
                    panes: Vec::new(),
                    active: id,
                    title,
                    title_set: false,
                    shown: None,
                    ssh: false,
                    ssh_target: None,
                    ssh_cmd: None,
                    transport: None,
                    prefix: None,
                    mark: None,
                    group: None,
                    attention: None,
                    editors: vec![ed],
                    previews: Vec::new(),
                });
                self.active_tab = self.tabs.len() - 1;
                self.selection = None;
                let key = path.to_string_lossy().to_string();
                *self.open_counts.entry(key.clone()).or_insert(0) += 1;
                self.recent_files.retain(|k| *k != key);
                self.recent_files.insert(0, key);
                self.recent_files.truncate(50);
                self.publish_panes();
                true
            }
            Err(e) => {
                let msg = format!(
                    "{} {}: {e}",
                    mtty_ui::i18n::t(self.lang, "Open failed", "打开失败"),
                    path.display()
                );
                self.show_notice(msg);
                false
            }
        }
    }

    /// Open bytes read from `dest:path` in a new editor pane (ADR 0034, E3),
    /// focusing an existing pane for the same file instead.
    fn open_remote_editor_pane(&mut self, dest: String, path: String, bytes: &[u8]) -> bool {
        if let Some((ti, id)) = self.tabs.iter().enumerate().find_map(|(ti, tab)| {
            tab.editors
                .iter()
                .find(|e| {
                    e.remote
                        .as_ref()
                        .is_some_and(|r| r.dest == dest && r.path == path)
                })
                .map(|e| (ti, e.id.clone()))
        }) {
            self.tabs[ti].active = id;
            self.active_tab = ti;
            self.selection = None;
            return true;
        }
        let id = gen_id();
        match editor_pane::EditorPane::open_remote(id.clone(), dest, path.clone(), bytes) {
            Ok(mut ed) => {
                ed.set_vim(self.editor_vim);
                let title = ed.title();
                self.tabs.push(Tab {
                    layout: Layout::leaf(id.clone()),
                    panes: Vec::new(),
                    active: id,
                    title,
                    title_set: false,
                    shown: None,
                    ssh: false,
                    ssh_target: None,
                    ssh_cmd: None,
                    transport: None,
                    prefix: None,
                    mark: None,
                    group: None,
                    attention: None,
                    editors: vec![ed],
                    previews: Vec::new(),
                });
                self.active_tab = self.tabs.len() - 1;
                self.selection = None;
                self.publish_panes();
                true
            }
            Err(e) => {
                let msg = format!(
                    "{} {path}: {e}",
                    mtty_ui::i18n::t(self.lang, "Open failed", "打开失败")
                );
                self.show_notice(msg);
                false
            }
        }
    }

    /// Fill a session-restored remote pane once its bytes arrive.
    fn remote_pane_loaded(&mut self, id: &str, bytes: &[u8], cursor: usize, scroll: usize) {
        for tab in &mut self.tabs {
            if let Some(ed) = tab.editors.iter_mut().find(|e| e.id == id) {
                ed.remote_loaded(bytes);
                let len = ed.doc.rope().len_chars();
                ed.doc
                    .set_selection(mtty_editor::Selection::cursor(cursor.min(len)));
                ed.scroll_line = scroll;
                break;
            }
        }
        self.publish_panes();
    }

    /// A background ssh write for an editor pane finished: mark the pane
    /// saved (only if it still holds the written bytes) and honour `:wq`.
    fn finish_pane_remote_write(
        &mut self,
        id: &str,
        dest: String,
        path: String,
        text: &[u8],
        result: std::io::Result<()>,
    ) {
        let mut quit = false;
        let mut found = false;
        for tab in &mut self.tabs {
            if let Some(ed) = tab.editors.iter_mut().find(|e| e.id == id) {
                found = true;
                match &result {
                    Ok(()) => {
                        ed.remote_saved(text);
                        quit = ed.quit_after_save;
                        ed.quit_after_save = false;
                    }
                    Err(_) => {
                        ed.saving = false;
                        ed.quit_after_save = false;
                    }
                }
                break;
            }
        }
        match result {
            Ok(()) if found => {
                if quit {
                    // `:wq`: discard anything typed while the write ran.
                    for tab in &mut self.tabs {
                        if let Some(ed) = tab.editors.iter_mut().find(|e| e.id == id) {
                            ed.close_armed = true;
                        }
                    }
                    self.close_pane_id(id);
                }
                self.window.request_redraw();
            }
            Err(e) => {
                let msg = format!(
                    "{} {dest}:{path}: {e}",
                    mtty_ui::i18n::t(self.lang, "Save failed", "保存失败")
                );
                self.show_notice(msg);
            }
            Ok(()) => {}
        }
    }

    /// A background ssh stamp probe for a remote pane finished (ADR 0034,
    /// E3). Reload silently when the file changed and the pane has no unsaved
    /// edits; keep the edits otherwise. A failed probe is a no-op.
    fn remote_pane_polled(&mut self, id: &str, stamp: Option<editor_pane::DiskStamp>) {
        let mut reload: Option<(String, String, String, editor_pane::DiskStamp)> = None;
        for tab in &mut self.tabs {
            let Some(ed) = tab.editors.iter_mut().find(|e| e.id == id) else {
                continue;
            };
            match editor_pane::remote_poll_outcome(ed.remote_disk, stamp, ed.doc.is_modified()) {
                editor_pane::RemotePollOutcome::Ignore => {
                    ed.remote_polling = false;
                    // Seed the baseline on the first successful probe.
                    if ed.remote_disk.is_none() {
                        if let Some(s) = stamp {
                            ed.note_remote_stamp(s);
                        }
                    }
                }
                editor_pane::RemotePollOutcome::KeepLocal => {
                    ed.remote_polling = false;
                    if let Some(s) = stamp {
                        ed.note_remote_stamp(s);
                    }
                }
                editor_pane::RemotePollOutcome::Reload => {
                    // The read is now in flight; `remote_polling` stays set so
                    // the next tick does not stack a second read.
                    if let (Some(remote), Some(s)) = (ed.remote.as_ref(), stamp) {
                        reload = Some((ed.id.clone(), remote.dest.clone(), remote.path.clone(), s));
                    } else {
                        ed.remote_polling = false;
                    }
                }
            }
            break;
        }
        if let Some((id, dest, path, stamp)) = reload {
            self.spawn_job(move || {
                let result = mtty_ui::ssh::read_remote(&dest, &path);
                JobDone::RemoteReloaded { id, stamp, result }
            });
        }
    }

    /// Read a changed remote file's bytes back for a pane. Reload only if it
    /// still has no unsaved edits; otherwise drop the bytes and adopt the
    /// stamp. A failed read is silent. Returns whether the text changed.
    fn remote_pane_reloaded(
        &mut self,
        id: &str,
        stamp: editor_pane::DiskStamp,
        result: std::io::Result<Vec<u8>>,
    ) -> bool {
        for tab in &mut self.tabs {
            let Some(ed) = tab.editors.iter_mut().find(|e| e.id == id) else {
                continue;
            };
            ed.remote_polling = false;
            let Ok(bytes) = result else {
                return false;
            };
            // Edited while the read ran: keep the local text.
            if ed.doc.is_modified() {
                ed.note_remote_stamp(stamp);
                return false;
            }
            return ed.reload_remote(&bytes, stamp);
        }
        false
    }

    /// Move the active editor pane to a 1-based line (and optional 0-based
    /// column), after MTP opened it (ADR 0040, A3).
    fn go_active_editor_to(&mut self, line: Option<usize>, column: Option<usize>) {
        let Some(line) = line else {
            return;
        };
        if let Some(ed) = self.active_editor_mut() {
            ed.go_to_line_col(line.saturating_sub(1), column.unwrap_or(0));
        }
    }

    /// The tab's agent pane, preferring the active pane (ADR 0040, A3).
    fn agent_pane(&self) -> Option<String> {
        let tab = self.tabs.get(self.active_tab)?;
        if self.mtp.agent_for(&tab.active).is_some() {
            return Some(tab.active.clone());
        }
        tab.panes
            .iter()
            .find(|p| self.mtp.agent_for(&p.id).is_some())
            .map(|p| p.id.clone())
    }

    /// Type a one-step prompt into the agent pane and press Enter (A3).
    fn send_to_agent(&mut self, text: &str) {
        let Some(id) = self.agent_pane() else {
            let msg = mtty_ui::i18n::t(
                self.lang,
                "No agent pane in this tab.",
                "当前标签没有 agent pane。",
            )
            .to_string();
            self.show_notice(msg);
            return;
        };
        let payload = format!("{text}\r");
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            if let Some(pane) = tab.panes.iter_mut().find(|p| p.id == id) {
                pane.term.write(payload.as_bytes());
            }
        }
    }

    /// Apply an agent's proposed edit to the target editor pane as one
    /// undoable transaction (ADR 0040, A1). The pane is found by id, else by
    /// path (opening it if needed), else the active pane.
    fn apply_proposal(
        &mut self,
        pane: Option<String>,
        path: Option<String>,
        edits: Vec<mtty_mtp::ProposedEdit>,
        text: Option<String>,
        label: Option<String>,
    ) -> bool {
        if let Some(p) = &path {
            let open = self.tabs.iter().any(|t| {
                t.editors
                    .iter()
                    .any(|e| e.remote.is_none() && same_file_path(&e.path, std::path::Path::new(p)))
            });
            if !open {
                self.open_editor_pane_impl(std::path::Path::new(p), text.is_some());
            }
        }
        let target = pane.or_else(|| match &path {
            Some(p) => self
                .tabs
                .iter()
                .flat_map(|t| t.editors.iter())
                .find(|e| e.remote.is_none() && same_file_path(&e.path, std::path::Path::new(p)))
                .map(|e| e.id.clone()),
            None => self.tabs.get(self.active_tab).map(|t| t.active.clone()),
        });
        let Some(target) = target else {
            return false;
        };
        let mut applied = false;
        for tab in &mut self.tabs {
            if let Some(ed) = tab.editors.iter_mut().find(|e| e.id == target) {
                if text.is_none() {
                    let mut ranges: Vec<_> = edits.iter().map(|e| (e.start, e.end)).collect();
                    ranges.sort_unstable();
                    let len = ed.doc.rope().len_chars();
                    let mut end = 0;
                    for (from, to) in ranges {
                        if from < end || from > to || to > len {
                            self.show_notice(
                                mtty_ui::i18n::t(
                                    self.lang,
                                    "Agent edit rejected: invalid or overlapping character ranges.",
                                    "已拒绝 Agent 修改：字符范围无效或相互重叠。",
                                )
                                .into(),
                            );
                            return false;
                        }
                        end = to;
                    }
                }
                match text {
                    Some(t) => ed.propose_text(t, label.clone()),
                    None => ed.propose(
                        edits
                            .iter()
                            .map(|e| (e.start, e.end, e.text.clone()))
                            .collect(),
                        label.clone(),
                    ),
                }
                applied = true;
                break;
            }
        }
        if applied {
            let msg = mtty_ui::i18n::t(
                self.lang,
                "Agent edit applied — keep editing to accept, or Reject in the palette to undo.",
                "已应用 agent 的修改——继续编辑即视为接受,或在命令面板中“拒绝”以撤销。",
            )
            .to_string();
            self.show_notice(msg);
            self.window.request_redraw();
        }
        applied
    }

    fn connect_acp_session(acp: &mut AcpSession) {
        acp.session = None;
        acp.connect_id = if let Some(session) = &acp.resume_id {
            if !acp.load_supported {
                acp.status = "session resume unsupported".into();
                acp.transcript
                    .push_str("\n[error] This agent does not support session/load.\n");
                return;
            }
            Some(acp.client.load_session(session, &acp.cwd))
        } else {
            Some(acp.client.new_session(&acp.cwd))
        };
        acp.status = "starting session".into();
    }

    /// Handle an ACP response: start a session after `initialize`, remember
    /// the session id, and note a finished turn (ADR 0040, A2).
    fn acp_response(
        &mut self,
        id: u64,
        result: serde_json::Value,
        error: Option<serde_json::Value>,
    ) {
        let Some(acp) = self.acp.as_mut() else {
            return;
        };
        if let Some(error) = error {
            let msg = error
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("error");
            acp.transcript.push_str(&format!("\n[error] {msg}\n"));
            acp.status = msg.to_string();
            self.window.request_redraw();
            return;
        }
        if id == acp.initialize_id {
            acp.load_supported = result
                .pointer("/agentCapabilities/loadSession")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            acp.auth_methods = result
                .get("authMethods")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| {
                    Some((
                        v.get("id")?.as_str()?.into(),
                        v.get("name")?.as_str()?.into(),
                    ))
                })
                .collect();
            if let Some(method) = &acp.auth_method {
                if acp.auth_methods.iter().any(|(id, _)| id == method) {
                    acp.authenticate_id = Some(acp.client.authenticate(method));
                    acp.status = "authenticating".into();
                } else {
                    acp.status = "unknown authentication method".into();
                    acp.transcript.push_str(
                        "\n[error] Configured auth-method was not advertised by this agent.\n",
                    );
                }
            } else {
                Self::connect_acp_session(acp);
            }
            self.window.request_redraw();
            return;
        }
        if acp.authenticate_id == Some(id) {
            acp.authenticate_id = None;
            Self::connect_acp_session(acp);
            self.window.request_redraw();
            return;
        }
        if acp.connect_id == Some(id) {
            acp.connect_id = None;
            acp.session = result
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .or_else(|| acp.resume_id.clone());
            acp.status = if acp.session.is_some() {
                "ready"
            } else {
                "missing session id"
            }
            .into();
            self.window.request_redraw();
            return;
        }
        if let Some(session) = result.get("sessionId").and_then(|v| v.as_str()) {
            acp.session = Some(session.to_string());
            acp.status = "ready".into();
            self.window.request_redraw();
            return;
        }
        if let Some(reason) = result.get("stopReason").and_then(|v| v.as_str()) {
            acp.transcript.push('\n');
            acp.status = format!("idle ({reason})");
            self.window.request_redraw();
        }
    }

    /// Launch a configured ACP agent and open its transcript (ADR 0040, A2).
    fn start_acp(&mut self, agent: mtty_config::AcpAgent) {
        let cwd = self
            .cwd()
            .or_else(mtty_config::home_dir)
            .unwrap_or_default();
        let Some((program, args)) = agent.command.split_first() else {
            return;
        };
        let program = program.clone();
        let args = args.to_vec();
        let bridge = Arc::new(AcpBridge::new(self.jobs_tx.clone(), self.proxy.clone()));
        let env: Vec<_> = agent.env.into_iter().collect();
        let resume_id = self
            .acp_start
            .as_ref()
            .map(|s| s.session_id.trim().to_string())
            .filter(|s| !s.is_empty())
            .or(agent.session_id);
        match mtty_acp::Client::spawn(&program, &args, Some(cwd.as_path()), &env, bridge) {
            Ok(client) => {
                client.enable_terminals(&cwd);
                let initialize_id = client.initialize("mtty", env!("CARGO_PKG_VERSION"));
                self.acp = Some(AcpSession {
                    client,
                    cwd: cwd.to_string_lossy().into_owned(),
                    session: None,
                    transcript: format!("[starting {}…]\n", agent.name),
                    prompt: String::new(),
                    status: "initializing".into(),
                    initialize_id,
                    authenticate_id: None,
                    connect_id: None,
                    auth_method: agent.auth_method,
                    auth_methods: Vec::new(),
                    resume_id,
                    load_supported: false,
                    terminals: Default::default(),
                });
                self.acp_start = None;
                self.window.set_visible(true);
                self.window.focus_window();
            }
            Err(e) => {
                if let Some(start) = self.acp_start.as_mut() {
                    start.error = Some(format!("{}: {e}", agent.name));
                }
            }
        }
    }

    /// The ACP start form (ADR 0040, A2).
    fn acp_start_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let lang = self.lang;
        let agents = self.acp_agents.clone();
        let Some(start) = self.acp_start.as_mut() else {
            return;
        };
        let mut open = true;
        let mut go = false;
        egui::Window::new(t(lang, "ACP Agent", "ACP Agent"))
            .collapsible(false)
            .open(&mut open)
            .show(ctx, |ui| {
                if agents.is_empty() {
                    ui.label(t(
                        lang,
                        "No [acp] agents configured; type a command.",
                        "未配置 [acp] agent;请输入命令。",
                    ));
                    ui.add(
                        egui::TextEdit::singleline(&mut start.command)
                            .hint_text("codex acp")
                            .desired_width(260.0),
                    );
                } else {
                    egui::ComboBox::from_id_salt(("acp-agent",))
                        .selected_text(
                            agents
                                .get(start.index)
                                .map(|a| a.name.as_str())
                                .unwrap_or(""),
                        )
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            for (i, a) in agents.iter().enumerate() {
                                ui.selectable_value(
                                    &mut start.index,
                                    i,
                                    format!("{} ({})", a.name, a.command.join(" ")),
                                );
                            }
                        });
                }
                ui.add(
                    egui::TextEdit::singleline(&mut start.session_id)
                        .hint_text(t(
                            lang,
                            "Session ID to resume (optional)",
                            "要恢复的会话 ID(可选)",
                        ))
                        .desired_width(260.0),
                );
                if let Some(err) = &start.error {
                    ui.colored_label(chrome_rgb(self.theme.chrome().negative), err);
                }
                if ui.button(t(lang, "Start", "启动")).clicked() {
                    go = true;
                }
            });
        if go {
            let agent = if agents.is_empty() {
                let command: Vec<String> = start
                    .command
                    .split_whitespace()
                    .map(str::to_string)
                    .collect();
                if command.is_empty() {
                    start.error = Some(t(lang, "Enter a command.", "请输入命令。").into());
                    return;
                }
                mtty_config::AcpAgent {
                    name: command[0].clone(),
                    command,
                    ..Default::default()
                }
            } else {
                agents[start.index.min(agents.len() - 1)].clone()
            };
            self.start_acp(agent);
        } else if !open {
            self.acp_start = None;
        }
    }

    /// The ACP transcript window and its prompt (ADR 0040, A2).
    fn acp_window(&mut self, ctx: &egui::Context) {
        use mtty_ui::i18n::t;
        let lang = self.lang;
        if let Some(id) = self.acp_writes.keys().next().cloned() {
            let mut choice = None;
            egui::Window::new(t(lang, "Review Agent Edit", "审阅 Agent 修改"))
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label(t(
                        lang,
                        "Deleted text is red; proposed text is green in the editor.",
                        "编辑器中，红色为删除内容，绿色为提议的新内容。",
                    ));
                    ui.horizontal(|ui| {
                        if ui
                            .button(t(lang, "Accept and Save", "接受并保存"))
                            .clicked()
                        {
                            choice = Some(Cmd::AcceptAgentEdit);
                        }
                        if ui.button(t(lang, "Reject", "拒绝")).clicked() {
                            choice = Some(Cmd::RejectAgentEdit);
                        }
                    });
                });
            if let Some(command) = choice {
                if let Some((ti, _)) = self
                    .tabs
                    .iter()
                    .enumerate()
                    .find(|(_, t)| t.editors.iter().any(|e| e.id == id))
                {
                    self.active_tab = ti;
                    self.tabs[ti].active = id;
                    self.run_command(command);
                }
            }
        }
        // A permission request is answered first; the agent is waiting.
        if let Some((question, _)) = self.acp_permission.as_ref() {
            let question = question.clone();
            let mut answer = None;
            let mut cancel = false;
            egui::Window::new(t(lang, "Agent Permission", "Agent 权限"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(question);
                    ui.horizontal(|ui| {
                        if ui.button(t(lang, "Allow", "允许")).clicked() {
                            answer = Some(true);
                        }
                        if ui.button(t(lang, "Deny", "拒绝")).clicked() {
                            answer = Some(false);
                        }
                        if ui.button(t(lang, "Cancel Turn", "取消本轮")).clicked() {
                            cancel = true;
                            answer = Some(false);
                        }
                    });
                });
            if let Some(allow) = answer {
                if let Some((_, reply)) = self.acp_permission.take() {
                    let _ = reply.send(allow);
                }
            }
            if cancel {
                for (_, (_, reply)) in self.acp_writes.drain() {
                    let _ = reply.send(false);
                }
                if let Some(acp) = &self.acp {
                    if let Some(session) = &acp.session {
                        acp.client.cancel(session);
                    }
                }
            }
            return;
        }
        let Some(acp) = self.acp.as_mut() else {
            return;
        };
        let mut open = true;
        let (mut send, mut stop, mut close, mut authenticate) = (false, false, false, false);
        egui::Window::new(t(lang, "ACP Agent", "ACP Agent"))
            .collapsible(false)
            .open(&mut open)
            .default_size([600.0, 420.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&acp.status).size(11.0));
                    if ui.small_button(t(lang, "Stop", "停止")).clicked() {
                        stop = true;
                    }
                    if ui.small_button(t(lang, "Close", "关闭")).clicked() {
                        close = true;
                    }
                });
                if !acp.auth_methods.is_empty() {
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("acp-auth-method")
                            .selected_text(acp.auth_method.as_deref().unwrap_or(t(
                                lang,
                                "Authentication",
                                "认证方式",
                            )))
                            .show_ui(ui, |ui| {
                                for (id, name) in &acp.auth_methods {
                                    ui.selectable_value(
                                        &mut acp.auth_method,
                                        Some(id.clone()),
                                        name,
                                    );
                                }
                            });
                        if ui
                            .add_enabled(
                                acp.auth_method.is_some(),
                                egui::Button::new(t(lang, "Authenticate", "认证")),
                            )
                            .clicked()
                        {
                            authenticate = true;
                        }
                    });
                }
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(
                            egui::RichText::new(&acp.transcript).monospace(),
                        ));
                        for (id, output) in &acp.terminals {
                            ui.collapsing(format!("Terminal {id}"), |ui| {
                                ui.label(egui::RichText::new(output).monospace());
                            });
                        }
                    });
                let r = ui.add(
                    egui::TextEdit::singleline(&mut acp.prompt)
                        .hint_text(t(lang, "Message the agent…", "给 agent 发消息…"))
                        .desired_width(f32::INFINITY),
                );
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter || ui.button(t(lang, "Send", "发送")).clicked() {
                    send = true;
                }
            });
        if authenticate {
            if let Some(method) = &acp.auth_method {
                acp.authenticate_id = Some(acp.client.authenticate(method));
                acp.status = "authenticating".into();
            }
        }
        if send && !acp.prompt.trim().is_empty() {
            if let Some(session) = acp.session.clone() {
                let text = std::mem::take(&mut acp.prompt);
                acp.client.prompt(&session, &text);
                acp.transcript.push_str(&format!("\n\u{203a} {text}\n\n"));
                acp.status = "working".into();
                self.window.request_redraw();
            }
        }
        if stop {
            for (_, (_, reply)) in self.acp_writes.drain() {
                let _ = reply.send(false);
            }
            if let Some(session) = acp.session.clone() {
                acp.client.cancel(&session);
            }
        }
        if close || !open {
            self.acp_writes.clear();
            self.acp = None;
        }
    }

    /// ⌘S in an editor pane. A failure stays visible and the pane modified.
    fn save_active_editor(&mut self) {
        let lang = self.lang;
        // A remote pane writes over ssh on a background thread.
        let remote = self.active_editor().and_then(|ed| {
            ed.remote
                .as_ref()
                .map(|r| (ed.id.clone(), r.dest.clone(), r.path.clone(), ed.saving))
        });
        if let Some((id, dest, path, saving)) = remote {
            if saving {
                return;
            }
            let bytes = match self.active_editor() {
                Some(ed) => ed.doc.to_bytes(),
                None => return,
            };
            if let Some(ed) = self.active_editor_mut() {
                ed.saving = true;
            }
            let msg = format!(
                "{} {dest}:{path}…",
                mtty_ui::i18n::t(lang, "Saving", "正在保存")
            );
            self.show_notice(msg);
            self.spawn_job(move || {
                let result = mtty_ui::ssh::write_remote(&dest, &path, &bytes);
                JobDone::RemoteWrite {
                    id: Some(id),
                    dest,
                    path,
                    text: bytes,
                    result,
                }
            });
            return;
        }
        let Some(ed) = self.active_editor_mut() else {
            return;
        };
        let saved = ed.save();
        if saved.is_ok() {
            let path = ed.path.clone();
            self.lsp.saved(&path);
        }
        let Some(ed) = self.active_editor() else {
            return;
        };
        let msg = match saved {
            Ok(()) => format!(
                "{} {}",
                mtty_ui::i18n::t(lang, "Saved", "已保存"),
                ed.path.display()
            ),
            Err(e) => format!(
                "{} {}: {e}",
                mtty_ui::i18n::t(lang, "Save failed", "保存失败"),
                ed.path.display()
            ),
        };
        self.show_notice(msg);
        self.publish_panes();
    }

    /// Closing the active editor pane: with unsaved changes the first close
    /// only warns; the next one discards.
    fn confirm_close_active_editor(&mut self) -> bool {
        let lang = self.lang;
        let Some(ed) = self.active_editor_mut() else {
            return true;
        };
        if !ed.doc.is_modified() || ed.close_armed {
            return true;
        }
        ed.close_armed = true;
        let msg = format!(
            "{}: {}",
            ed.title(),
            mtty_ui::i18n::t(
                lang,
                "unsaved changes. Save, or close again to discard them.",
                "有未保存的修改。请保存,或再次关闭以放弃修改。"
            )
        );
        self.show_notice(msg);
        false
    }

    /// Closing these tabs (or quitting): unsaved editor panes in them warn
    /// once; closing again discards.
    fn confirm_close_tabs(&mut self, tabs: &[usize]) -> bool {
        let mut names = Vec::new();
        for &i in tabs {
            if let Some(tab) = self.tabs.get_mut(i) {
                for e in &mut tab.editors {
                    if e.doc.is_modified() && !e.close_armed {
                        e.close_armed = true;
                        names.push(e.title());
                    }
                }
            }
        }
        if names.is_empty() {
            return true;
        }
        let msg = format!(
            "{}: {}",
            names.join(", "),
            mtty_ui::i18n::t(
                self.lang,
                "unsaved changes. Save, or close again to discard them.",
                "有未保存的修改。请保存,或再次关闭以放弃修改。"
            )
        );
        self.show_notice(msg);
        false
    }

    /// Close Others / Close Below never discard unsaved files: with any in
    /// those tabs nothing closes.
    fn refuse_unsaved(&mut self, tabs: &[usize]) -> bool {
        let names: Vec<String> = tabs
            .iter()
            .filter_map(|&i| self.tabs.get(i))
            .flat_map(|t| t.editors.iter())
            .filter(|e| e.doc.is_modified())
            .map(|e| e.title())
            .collect();
        if names.is_empty() {
            return true;
        }
        let msg = format!(
            "{}: {}",
            names.join(", "),
            mtty_ui::i18n::t(
                self.lang,
                "unsaved changes. Save or close these first.",
                "有未保存的修改。请先保存或单独关闭它们。"
            )
        );
        self.show_notice(msg);
        false
    }

    fn open_editor(&mut self, path: std::path::PathBuf) -> bool {
        self.open_editor_ro(path, false)
    }

    /// Open a file in the built-in editor; `readonly` is used by MTP `app.view`.
    fn open_editor_ro(&mut self, path: std::path::PathBuf, readonly: bool) -> bool {
        let large = std::fs::metadata(&path).is_ok_and(|m| m.len() > editor_pane::MAX_PANE_BYTES);
        // A large file opens in view mode whatever its kind: the floating
        // editor would read all of it into one text field. Markdown opens
        // in a single live editor pane; the
        // floating editor is left for read-only views (`app.view`).
        if large || !readonly {
            return self.open_editor_pane(&path);
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let preview = name.ends_with(".md") || name.ends_with(".markdown");
                let key = path.to_string_lossy().to_string();
                self.editor = Some(Editor {
                    path,
                    original: text.clone(),
                    text,
                    preview,
                    readonly,
                    remote: None,
                    close_armed: false,
                    saving: false,
                    quit_after_save: false,
                });
                self.vim_for.clear();
                self.recent_files.retain(|p| p != &key);
                *self.open_counts.entry(key.clone()).or_insert(0) += 1;
                self.recent_files.insert(0, key);
                self.recent_files.truncate(50);
                true
            }
            Err(e) => {
                let msg = format!(
                    "{} {}: {e}",
                    mtty_ui::i18n::t(self.lang, "Open failed", "打开失败"),
                    path.display()
                );
                self.show_notice(msg);
                false
            }
        }
    }

    fn open_dialog_window(&mut self, ctx: &egui::Context) {
        if !self.show_open {
            return;
        }
        let mut open = true;
        let mut path = std::mem::take(&mut self.open_path);
        let mut do_open = false;
        egui::Window::new(mtty_ui::i18n::t(self.lang, "Open File", "打开文件"))
            .collapsible(false)
            .open(&mut open)
            .show(ctx, |ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut path)
                        .hint_text("/path/to/file")
                        .desired_width(360.0),
                );
                // Check Enter before re-taking focus: Enter makes the field give it up,
                // and taking it back first would hide that (`lost_focus` stays false).
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if !enter {
                    r.request_focus();
                }
                if enter {
                    do_open = true;
                }
                if ui
                    .button(mtty_ui::i18n::t(self.lang, "Open", "打开"))
                    .clicked()
                {
                    do_open = true;
                }
            });
        self.open_path = path;
        if do_open && !self.open_path.is_empty() {
            let p = std::path::PathBuf::from(&self.open_path);
            if self.open_editor(p) {
                self.show_open = false;
            }
        }
        if !open {
            self.show_open = false;
        }
    }

    fn editor_window(&mut self, ctx: &egui::Context) {
        let Some(ed) = self.editor.as_mut() else {
            return;
        };
        let title = match &ed.remote {
            Some((dest, path)) => format!("{dest}:{path}"),
            None => ed
                .path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| ed.path.display().to_string()),
        };
        if self.vim_for != title {
            self.vim_for = title.clone();
            self.vim = self.editor_vim.then(mtty_ui::vim::VimRuntime::default);
        }
        let modified = ed.text != ed.original;
        let lang = mtty_ui::syntax::detect(&match &ed.remote {
            Some((_, p)) => p.clone(),
            None => ed.path.to_string_lossy().to_string(),
        });
        let mut layouter =
            mtty_ui::syntax::layouter(lang, chrome_rgb(self.theme.chrome().text), 13.0);
        let mut open = true;
        let mut save = false;
        let mut quit = false;
        let mut edit_in_tab = false;
        app_window(title, ctx).open(&mut open).show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ed.readonly {
                    ui.label(
                        egui::RichText::new(mtty_ui::i18n::t(self.lang, "read-only", "只读"))
                            .color(chrome_rgb(self.theme.chrome().muted)),
                    );
                } else if ui
                    .button(mtty_ui::i18n::t(self.lang, "Save", "保存"))
                    .clicked()
                {
                    save = true;
                }
                ui.checkbox(
                    &mut ed.preview,
                    mtty_ui::i18n::t(self.lang, "Markdown preview", "Markdown 预览"),
                );
                if ed.saving {
                    ui.label(
                        egui::RichText::new(mtty_ui::i18n::t(self.lang, "saving…", "保存中…"))
                            .color(chrome_rgb(self.theme.chrome().muted)),
                    );
                } else if modified && !ed.readonly {
                    ui.label(
                        egui::RichText::new(mtty_ui::i18n::t(self.lang, "modified", "已修改"))
                            .color(chrome_rgb(self.theme.chrome().warning)),
                    );
                }
                // A local file only: the external editor reads it from disk,
                // so unsaved changes must be saved (or dropped) first.
                if ed.remote.is_none() {
                    let resp = ui.add_enabled(
                        !modified,
                        egui::Button::new(mtty_ui::i18n::t(
                            self.lang,
                            "Edit in Tab",
                            "在标签中编辑",
                        )),
                    );
                    let resp = if modified {
                        resp.on_disabled_hover_text(mtty_ui::i18n::t(
                            self.lang,
                            "Save first: the editor opens the file on disk",
                            "请先保存:外部编辑器打开的是磁盘上的文件",
                        ))
                    } else {
                        resp.on_hover_text(edit_in_tab_command(
                            self.editor_command.as_deref(),
                            std::env::var("EDITOR").ok().as_deref(),
                            &ed.path.to_string_lossy(),
                        ))
                    };
                    edit_in_tab = resp.clicked();
                }
            });
            ui.separator();
            if ed.preview {
                let ch = self.theme.chrome();
                let fg = mtty_ui::chrome::bg_color(ch.text);
                let panel = mtty_ui::chrome::bg_color(ch.card);
                // Both ways: wide code blocks scroll instead of widening the window.
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let base = ed.remote.is_none().then(|| ed.path.parent()).flatten();
                        render_markdown(
                            ui,
                            &ed.text,
                            base,
                            &mut self.cmark,
                            &mut self.mmd,
                            fg,
                            panel,
                        );
                    });
            } else {
                let mut vim_effect = mtty_ui::vim::VimEffect::Nothing;
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            let lines = ed.text.lines().count().max(1);
                            let mut nums = String::new();
                            for i in 1..=lines {
                                nums.push_str(&format!("{i:>4}\n"));
                            }
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(nums)
                                        .monospace()
                                        .size(13.0)
                                        .color(chrome_rgb(self.theme.chrome().muted)),
                                )
                                .selectable(false),
                            );
                            let text_id = egui::Id::new("mtty-editor-text");
                            let mut edit = egui::TextEdit::multiline(&mut ed.text)
                                .id(text_id)
                                .code_editor()
                                .desired_width(f32::INFINITY)
                                .layouter(&mut layouter);
                            if ed.readonly {
                                edit = edit.interactive(false);
                            }
                            ui.add(edit);
                            if let Some(v) = self.vim.as_mut().filter(|_| !ed.readonly) {
                                vim_effect =
                                    mtty_ui::vim::vim_handle(&mut ed.text, v, ui.ctx(), text_id);
                            }
                        });
                    });
                if vim_effect == mtty_ui::vim::VimEffect::Save {
                    save = true;
                }
                if vim_effect == mtty_ui::vim::VimEffect::Quit {
                    quit = true;
                }
            }
        });
        let outcome = if save {
            self.save_editor()
        } else {
            SaveOutcome::Saved
        };
        if quit {
            match outcome {
                SaveOutcome::Saved => {
                    self.editor = None;
                    self.vim = None;
                    return;
                }
                SaveOutcome::Pending => {
                    if let Some(ed) = self.editor.as_mut() {
                        ed.quit_after_save = true;
                    }
                }
                SaveOutcome::Failed => {}
            }
        }
        if edit_in_tab {
            if let Some(ed) = self.editor.take() {
                // The new tab starts in the active directory, not the file's.
                let path = std::path::absolute(&ed.path).unwrap_or(ed.path.clone());
                let cmd = edit_in_tab_command(
                    self.editor_command.as_deref(),
                    std::env::var("EDITOR").ok().as_deref(),
                    &path.to_string_lossy(),
                );
                let title = ed
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.vim = None;
                self.run_in_new_tab(&title, &cmd);
            }
            return;
        }
        if !open {
            self.close_editor();
        }
    }

    /// Write the editor buffer (locally or over ssh). The buffer is marked
    /// clean only when the write succeeds; a failure stays visible.
    fn save_editor(&mut self) -> SaveOutcome {
        let Some(ed) = self.editor.as_mut() else {
            return SaveOutcome::Failed;
        };
        if let (Some((dest, path)), false) = (ed.remote.clone(), ed.readonly) {
            if !ed.saving {
                ed.saving = true;
                let text = ed.text.clone();
                self.spawn_job(move || {
                    let result = mtty_ui::ssh::write_remote(&dest, &path, text.as_bytes());
                    JobDone::RemoteWrite {
                        id: None,
                        dest,
                        path,
                        text: text.into_bytes(),
                        result,
                    }
                });
            }
            return SaveOutcome::Pending;
        }
        match ed.write() {
            Ok(()) => SaveOutcome::Saved,
            Err(e) => {
                let msg = format!(
                    "{}: {e}",
                    mtty_ui::i18n::t(self.lang, "Save failed", "保存失败")
                );
                self.show_notice(msg);
                SaveOutcome::Failed
            }
        }
    }

    /// Run `work` on a background thread; its result is handled by
    /// [`State::finish_job`] on the UI thread.
    fn spawn_job(&self, work: impl FnOnce() -> JobDone + Send + 'static) {
        let tx = self.jobs_tx.clone();
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let _ = tx.send(work());
            let _ = proxy.send_event(HostEvent::Wake);
        });
    }

    /// Pick up edits to `views.json` (checked at most every 2 s).
    fn reload_rules_if_changed(&mut self) {
        if self.rules_checked.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.rules_checked = Instant::now();
        let mtime = views_mtime();
        if mtime != self.rules_mtime {
            self.rules_mtime = mtime;
            self.rules = mtty_config::view::RuleSet::load();
            self.publish_panes();
            self.window.request_redraw();
        }
    }

    /// Notice files edited outside mtty (checked at most once a second). A
    /// pane with no unsaved edits reloads silently; one with edits asks
    /// before replacing them.
    fn reload_editors_if_changed(&mut self) {
        if self.editor_reload_checked.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.editor_reload_checked = Instant::now();
        let mut redraw = false;
        let mut offer: Option<(String, editor_pane::DiskStamp)> = None;
        let mut deleted: Option<String> = None;
        // Remote panes are probed over ssh in the background; collect them
        // while the tabs are borrowed, then spawn once the loop is done.
        let mut probes: Vec<(String, String, String)> = Vec::new();
        for tab in &mut self.tabs {
            for ed in &mut tab.editors {
                if ed.is_view_only() {
                    continue;
                }
                // A remote pane has no local stamp to read; probe its
                // length/mtime over ssh instead, one probe at a time.
                if let Some(remote) = &ed.remote {
                    if !ed.saving && !ed.remote_polling {
                        ed.remote_polling = true;
                        probes.push((ed.id.clone(), remote.dest.clone(), remote.path.clone()));
                    }
                    continue;
                }
                let Some(stamp) = editor_pane::disk_stamp(&ed.path) else {
                    if ed.disk.is_some() && !ed.missing_warned {
                        ed.missing_warned = true;
                        deleted = Some(ed.path.display().to_string());
                    }
                    continue;
                };
                if ed.disk == Some(stamp) {
                    continue;
                }
                ed.missing_warned = false;
                if ed.doc.is_modified() {
                    // One prompt at a time; the pane keeps its edits until
                    // the user answers.
                    if offer.is_none() && self.editor_reload_offer.is_none() {
                        offer = Some((ed.id.clone(), stamp));
                    }
                    continue;
                }
                if ed.reload_from_disk() {
                    redraw = true;
                }
            }
        }
        for (id, dest, path) in probes {
            self.spawn_job(move || {
                let stamp = mtty_ui::ssh::stat_remote(&dest, &path)
                    .ok()
                    .map(|(len, secs)| editor_pane::DiskStamp {
                        len,
                        modified: (secs >= 0).then(|| {
                            std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64)
                        }),
                    });
                JobDone::RemotePolled { id, stamp }
            });
        }
        if let Some(offer) = offer {
            self.editor_reload_offer = Some(offer);
            redraw = true;
        }
        if let Some(path) = deleted {
            let name = std::path::Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(path);
            let msg = if self.lang == mtty_ui::i18n::Lang::En {
                format!("{name} was deleted on disk.")
            } else {
                format!("{name} 已在磁盘上删除。")
            };
            self.show_notice(msg);
        }
        if redraw {
            self.window.request_redraw();
        }
    }

    fn poll_jobs(&mut self) {
        while let Ok(done) = self.jobs_rx.try_recv() {
            self.finish_job(done);
            self.window.request_redraw();
        }
        self.poll_acp_writes();
    }

    fn poll_acp_writes(&mut self) {
        let mut finished = Vec::new();
        for (id, (revision, _)) in &self.acp_writes {
            let ed = self
                .tabs
                .iter()
                .flat_map(|t| &t.editors)
                .find(|ed| &ed.id == id);
            let result = match ed {
                None => Some(false),
                Some(ed) if ed.doc.revision() != *revision => Some(false),
                Some(ed) if ed.disk.is_some() && !ed.doc.is_modified() => Some(true),
                Some(_) => None,
            };
            if let Some(result) = result {
                finished.push((id.clone(), result));
            }
        }
        for (id, result) in finished {
            if let Some((_, reply)) = self.acp_writes.remove(&id) {
                let _ = reply.send(result);
            }
        }
    }

    fn finish_job(&mut self, done: JobDone) {
        use mtty_ui::i18n::t;
        match done {
            JobDone::DirListed { dir, entries } => self.tree_listed(dir, entries),
            JobDone::SftpListed { dir, result } => {
                if let Some(view) = self.sftp_view.as_mut() {
                    view.busy = None;
                    match result {
                        Ok(entries) => {
                            view.remote_dir = Some(dir);
                            view.remote_entries = entries;
                            view.error = None;
                        }
                        Err(e) => view.error = Some(e),
                    }
                }
            }
            JobDone::Synced(result) => {
                self.sync.running = false;
                self.sync.due = Some(Instant::now() + Duration::from_secs(60));
                match &result {
                    Ok(out) => {
                        if out.hosts_changed {
                            self.reload_hosts();
                        }
                        if out.snippets_changed {
                            self.reload_snippets();
                        }
                        if !out.problems.is_empty() {
                            let msg = format!(
                                "{}: {}",
                                t_lang(self.lang, "Sync skipped files", "同步跳过了文件"),
                                out.problems.join("; ")
                            );
                            self.show_notice(msg);
                        }
                    }
                    Err(e) => {
                        let msg = format!("{}: {e}", t_lang(self.lang, "Sync failed", "同步失败"));
                        self.show_notice(msg);
                    }
                }
                self.sync.last = Some((std::time::SystemTime::now(), result));
            }
            JobDone::UpdateDownloaded(result) => {
                self.update_install = match result {
                    Ok(path) => UpdateInstall::Ready(path),
                    Err(e) => UpdateInstall::Failed(e),
                };
                if self.update_auto {
                    self.continue_auto_update();
                }
            }
            JobDone::SftpLocalListed { dir, entries } => {
                if let Some(view) = self.sftp_view.as_mut().filter(|v| v.local_dir == dir) {
                    view.local_entries = entries;
                }
            }
            JobDone::SftpDone { label, result } => {
                if let Some(view) = self.sftp_view.as_mut() {
                    view.busy = None;
                    match result {
                        Ok(()) => {
                            view.error = None;
                            self.show_notice(format!("SFTP: {label} \u{2713}"));
                        }
                        Err(e) => view.error = Some(format!("{label}: {e}")),
                    }
                }
                self.sftp_refresh(true, true);
            }
            JobDone::AgentStatus(agent) => {
                if let Some(view) = self.hosts_view.as_mut() {
                    view.agent = Some(agent);
                }
            }
            JobDone::HostKeyChecked { name, result } => {
                if let Some(view) = self.hosts_view.as_mut() {
                    view.keys.insert(name, Some(result));
                }
            }
            JobDone::TaskCreated { result, agent } => match result {
                Ok(task) => {
                    self.new_tab_in(Some(task.path.clone()));
                    if let Some(tab) = self.tabs.last_mut() {
                        tab.title = format!("task: {}", task.name);
                        tab.title_set = true;
                        let cmd = agent
                            .and_then(|i| mtty_ui::integration::AGENTS.get(i))
                            .map(mtty_ui::integration::launch_command);
                        let active = tab.active.clone();
                        if let (Some(cmd), Some(pane)) =
                            (cmd, tab.panes.iter_mut().find(|p| p.id == active))
                        {
                            pane.term.write(format!("{cmd}\r").as_bytes());
                        }
                    }
                    self.publish_panes();
                    let msg = format!(
                        "{} {} ({})",
                        t(self.lang, "Task created on branch", "任务已创建,分支"),
                        task.branch,
                        task.path.display()
                    );
                    self.show_notice(msg);
                }
                Err(e) => {
                    let msg = format!("{}: {e}", t(self.lang, "Task not created", "任务未创建"));
                    self.show_notice(msg);
                }
            },
            JobDone::TasksListed { repo, result } => {
                if let Some(view) = self.tasks_view.as_mut().filter(|v| v.repo == repo) {
                    view.loading = false;
                    match result {
                        Ok(tasks) => view.tasks = tasks,
                        Err(e) => {
                            self.tasks_view = None;
                            let msg = format!("{}: {e}", t(self.lang, "No tasks", "没有任务"));
                            self.show_notice(msg);
                        }
                    }
                }
            }
            JobDone::TaskDiff { name, result } => match result {
                Ok(text) => {
                    let text = if text.trim().is_empty() {
                        t(self.lang, "(no changes yet)", "(还没有改动)").to_string()
                    } else {
                        text
                    };
                    self.editor = Some(Editor {
                        path: std::path::PathBuf::from(format!("{name}.diff")),
                        original: text.clone(),
                        text,
                        preview: false,
                        readonly: true,
                        remote: None,
                        close_armed: false,
                        saving: false,
                        quit_after_save: false,
                    });
                }
                Err(e) => {
                    let msg = format!("{} {name}: {e}", t(self.lang, "Diff failed", "diff 失败"));
                    self.show_notice(msg);
                }
            },
            JobDone::TaskDone {
                name,
                merged,
                result,
            } => {
                let msg = match (&result, merged) {
                    (Ok(summary), true) => {
                        format!("{} {name}: {summary}", t(self.lang, "Merged", "已合并"))
                    }
                    (Ok(_), false) => format!("{} {name}", t(self.lang, "Discarded", "已丢弃")),
                    (Err(e), _) => format!("{name}: {e}"),
                };
                self.show_notice(msg);
                self.reload_tasks();
            }
            JobDone::RemoteRead {
                id,
                cursor,
                scroll,
                dest,
                path,
                result,
            } => match result {
                Ok(bytes) => match id {
                    Some(id) => self.remote_pane_loaded(&id, &bytes, cursor, scroll),
                    None => {
                        if self.open_remote_editor_pane(dest, path, &bytes) {
                            self.notice = None;
                        }
                    }
                },
                Err(e) => {
                    let msg = format!(
                        "{} {dest}:{path}: {e}",
                        t(self.lang, "Remote read failed", "读取远端文件失败")
                    );
                    self.show_notice(msg);
                    // A restored pane whose file cannot be read is dropped.
                    if let Some(id) = id {
                        self.close_pane_id(&id);
                    }
                }
            },
            JobDone::RemoteWrite {
                id,
                dest,
                path,
                text,
                result,
            } => {
                if let Some(id) = id {
                    self.finish_pane_remote_write(&id, dest, path, &text, result);
                } else {
                    let target = Some((dest.clone(), path.clone()));
                    let Some(ed) = self.editor.as_mut().filter(|ed| ed.remote == target) else {
                        // The editor moved on; still report a failure.
                        if let Err(e) = result {
                            let msg = format!(
                                "{} {dest}:{path}: {e}",
                                t(self.lang, "Save failed", "保存失败")
                            );
                            self.show_notice(msg);
                        }
                        return;
                    };
                    ed.saving = false;
                    match result {
                        Ok(()) => {
                            ed.mark_saved(String::from_utf8_lossy(&text).into_owned());
                            if ed.quit_after_save {
                                self.editor = None;
                                self.vim = None;
                            }
                        }
                        Err(e) => {
                            ed.quit_after_save = false;
                            let msg = format!(
                                "{} {dest}:{path}: {e}",
                                t(self.lang, "Save failed", "保存失败")
                            );
                            self.show_notice(msg);
                        }
                    }
                }
            }
            JobDone::RemotePolled { id, stamp } => {
                self.remote_pane_polled(&id, stamp);
            }
            JobDone::RemoteReloaded { id, stamp, result } => {
                self.remote_pane_reloaded(&id, stamp, result);
            }
            JobDone::TransportConnected {
                target,
                title,
                result,
            } => {
                self.pending_transport_connects = self.pending_transport_connects.saturating_sub(1);
                match result {
                    Ok(conn) => self.open_transport_pane(target, title, conn),
                    Err(e) => {
                        let msg = format!(
                            "{} {}: {e}",
                            t(self.lang, "Connection failed", "连接失败"),
                            target.label()
                        );
                        self.show_notice(msg);
                    }
                }
            }
            JobDone::KeyImported(result) => match result {
                Ok(path) => {
                    let msg = format!("{} {path}", t(self.lang, "Key written to", "密钥已写入"));
                    self.show_notice(msg);
                }
                Err(e) => self.show_notice(e),
            },
            JobDone::AcpUpdate(text) => {
                if let Some(acp) = self.acp.as_mut() {
                    acp.transcript.push_str(&text);
                }
                self.window.request_redraw();
            }
            JobDone::AcpDiff { path, text } => {
                self.apply_proposal(None, Some(path), Vec::new(), Some(text), Some("acp".into()));
            }
            JobDone::AcpRead { path, reply } => {
                let text = self
                    .tabs
                    .iter()
                    .flat_map(|t| &t.editors)
                    .find(|e| {
                        e.remote.is_none() && same_file_path(&e.path, std::path::Path::new(&path))
                    })
                    .filter(|e| !e.is_view_only())
                    .map(|e| e.doc.rope().to_string());
                let _ = reply.send(text);
            }
            JobDone::AcpWrite { path, text, reply } => {
                let wanted = text.clone();
                let applied = self.apply_proposal(
                    None,
                    Some(path.clone()),
                    Vec::new(),
                    Some(text),
                    Some("acp".into()),
                );
                let ed = self.tabs.iter().flat_map(|t| &t.editors).find(|e| {
                    e.remote.is_none() && same_file_path(&e.path, std::path::Path::new(&path))
                });
                if applied {
                    if let Some(ed) = ed {
                        if ed.disk.is_some()
                            && !ed.doc.is_modified()
                            && ed.doc.rope() == wanted.as_str()
                        {
                            let _ = reply.send(true);
                        } else {
                            self.acp_writes
                                .insert(ed.id.clone(), (ed.doc.revision(), reply));
                            self.show_notice(t(self.lang,
                                "Review the agent edit. Accept saves it; Reject leaves the file unchanged.",
                                "请审阅 Agent 修改。接受会保存文件；拒绝则保留磁盘原文。").into());
                        }
                    } else {
                        let _ = reply.send(false);
                    }
                } else {
                    let _ = reply.send(false);
                }
            }
            JobDone::AcpTerminal { id, output } => {
                if let Some(acp) = &mut self.acp {
                    acp.terminals.insert(id, output);
                }
                self.window.request_redraw();
            }
            JobDone::AcpResponse { id, result, error } => self.acp_response(id, result, error),
            JobDone::AcpPermission { question, reply } => {
                self.acp_permission = Some((question, reply));
                self.window.request_redraw();
            }
        }
    }

    /// Close the editor; unsaved changes need a second close to discard.
    fn close_editor(&mut self) {
        let Some(ed) = self.editor.as_mut() else {
            return;
        };
        if ed.may_close() {
            self.editor = None;
            return;
        }
        let msg = mtty_ui::i18n::t(
            self.lang,
            "Unsaved changes. Close again to discard them.",
            "有未保存的修改。再次关闭将丢弃修改。",
        )
        .to_string();
        self.show_notice(msg);
    }

    fn show_notice(&mut self, msg: String) {
        self.notice = Some((msg, Instant::now() + Duration::from_secs(8)));
        self.window.request_redraw();
    }

    fn prefix_window(&mut self, ctx: &egui::Context, i: usize) {
        let mut open = true;
        let mut buf = std::mem::take(&mut self.prefix_buf);
        let mut commit = false;
        egui::Window::new(mtty_ui::i18n::t(self.lang, "Tab Prefix", "标签前缀"))
            .collapsible(false)
            .open(&mut open)
            .show(ctx, |ui| {
                let r = ui.text_edit_singleline(&mut buf);
                // Check Enter before re-taking focus: Enter makes the field give it up,
                // and taking it back first would hide that (`lost_focus` stays false).
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if !enter {
                    r.request_focus();
                }
                if enter {
                    commit = true;
                }
                if ui
                    .button(mtty_ui::i18n::t(self.lang, "Set", "设置"))
                    .clicked()
                {
                    commit = true;
                }
            });
        self.prefix_buf = buf;
        if commit {
            if let Some(t) = self.tabs.get_mut(i) {
                let p = self.prefix_buf.trim().to_string();
                t.prefix = if p.is_empty() { None } else { Some(p) };
            }
            self.prefix_renaming = None;
            self.publish_panes();
        }
        if !open {
            self.prefix_renaming = None;
        }
    }

    /// A one-line editor for a tab's mark or group (ADR 0011). Empty clears it.
    #[allow(clippy::too_many_arguments)]
    fn tab_text_window(
        &mut self,
        ctx: &egui::Context,
        i: usize,
        en: &'static str,
        zh: &'static str,
        buf: &mut String,
        slot: &mut Option<usize>,
        is_group: bool,
    ) {
        let mut open = true;
        let mut text = std::mem::take(buf);
        let mut commit = false;
        egui::Window::new(mtty_ui::i18n::t(self.lang, en, zh))
            .collapsible(false)
            .open(&mut open)
            .show(ctx, |ui| {
                let r = ui.text_edit_singleline(&mut text);
                // Check Enter before re-taking focus: Enter makes the field give it up,
                // and taking it back first would hide that (`lost_focus` stays false).
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if !enter {
                    r.request_focus();
                }
                if enter {
                    commit = true;
                }
                if ui
                    .button(mtty_ui::i18n::t(self.lang, "Set", "设置"))
                    .clicked()
                {
                    commit = true;
                }
            });
        *buf = text;
        if commit {
            let value = buf.trim().to_string();
            if let Some(t) = self.tabs.get_mut(i) {
                let value = if value.is_empty() { None } else { Some(value) };
                if is_group {
                    t.group = value;
                } else {
                    t.mark = value;
                }
            }
            *slot = None;
            self.publish_panes();
        }
        if !open {
            *slot = None;
        }
    }

    fn rename_window(&mut self, ctx: &egui::Context, i: usize) {
        let mut buf = std::mem::take(&mut self.rename_buf);
        let outcome = rename_dialog(ctx, self.lang, &mut buf);
        self.rename_buf = buf;
        if outcome == DialogOutcome::Commit {
            if let Some(tab) = self.tabs.get_mut(i) {
                // An empty name goes back to the automatic title.
                let name = self.rename_buf.trim().to_string();
                tab.title_set = !name.is_empty();
                if !name.is_empty() {
                    tab.title = name;
                }
            }
            self.renaming = None;
            self.publish_panes();
        }
        if outcome == DialogOutcome::Cancel {
            self.renaming = None;
        }
    }
}

/// Draw a small chevron triangle (avoids font-glyph tofu).
fn chevron(p: &egui::Painter, rect: egui::Rect, open: bool, color: egui::Color32) {
    let c = rect.center();
    let (dx, dy) = (3.0, 4.0);
    let pts = if open {
        vec![
            egui::pos2(c.x - dx, c.y - dy * 0.5),
            egui::pos2(c.x + dx, c.y - dy * 0.5),
            egui::pos2(c.x, c.y + dy * 0.7),
        ]
    } else {
        vec![
            egui::pos2(c.x - dy * 0.5, c.y - dx),
            egui::pos2(c.x - dy * 0.5, c.y + dx),
            egui::pos2(c.x + dy * 0.7, c.y),
        ]
    };
    p.add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
}

/// Render a lazily-loaded directory tree.
#[allow(clippy::too_many_arguments)]
fn render_dir_tree(
    ui: &mut egui::Ui,
    children: &HashMap<std::path::PathBuf, Vec<FileEntry>>,
    expanded: &std::collections::HashSet<std::path::PathBuf>,
    dir: &std::path::Path,
    depth: usize,
    filter: &str,
    ch: &mtty_ui::theme::Chrome,
    open_file: &mut Option<std::path::PathBuf>,
    toggle: &mut Option<std::path::PathBuf>,
) {
    let Some(entries) = children.get(dir) else {
        return;
    };
    let muted = chrome_rgb(ch.muted);
    for e in entries {
        let is_dir = e.is_dir;
        if !is_dir && !filter.is_empty() && !e.name.to_lowercase().contains(filter) {
            continue;
        }
        let path = dir.join(&e.name);
        let is_open = expanded.contains(&path);
        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 12.0);
            // The disclosure triangle is itself clickable.
            let (ir, chev_resp) =
                ui.allocate_exact_size(egui::Vec2::splat(14.0), egui::Sense::click());
            if is_dir {
                chevron(ui.painter(), ir, is_open, muted);
                if chev_resp.clicked() {
                    *toggle = Some(path.clone());
                }
            }
            let (ird, icon_resp) =
                ui.allocate_exact_size(egui::Vec2::splat(14.0), egui::Sense::click());
            let icon = file_tree_icon(ch, &e.name, is_dir);
            mtty_ui::icons::draw_tab_icon(ui.painter(), ird, &icon, muted);
            if icon_resp.clicked() {
                if is_dir {
                    *toggle = Some(path.clone());
                } else {
                    *open_file = Some(path.clone());
                }
            }
            // Right-to-left so the size hugs the edge, then the name fills
            // what is left and truncates: a long name must not widen the panel.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                if !is_dir {
                    ui.label(
                        egui::RichText::new(human_size(e.size))
                            .size(10.5)
                            .color(muted),
                    );
                    ui.add_space(6.0);
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let label = egui::RichText::new(&e.name).size(12.0);
                    let label = if is_dir {
                        label.color(egui::Color32::from_rgb(
                            ch.folder.0,
                            ch.folder.1,
                            ch.folder.2,
                        ))
                    } else {
                        label
                    };
                    let resp = ui.selectable_label(false, label);
                    if resp.clicked() {
                        if is_dir {
                            *toggle = Some(path.clone());
                        } else {
                            *open_file = Some(path.clone());
                        }
                    }
                });
            });
        });
        if is_dir && is_open {
            render_dir_tree(
                ui,
                children,
                expanded,
                &path,
                depth + 1,
                filter,
                ch,
                open_file,
                toggle,
            );
        }
    }
}

/// The tree/list icon for a file entry: a Nerd-Font type glyph, tinted from the
/// chrome palette (directories blue, config amber, code accent, files muted).
fn file_tree_icon(
    ch: &mtty_ui::theme::Chrome,
    name: &str,
    is_dir: bool,
) -> mtty_ui::icons::TabIcon {
    use mtty_ui::icons::{self, Icon, TabIcon};
    let rule = icons::file_type(name, is_dir);
    let color = if is_dir {
        ch.folder
    } else if matches!(rule, "settings" | "lock") {
        ch.warning
    } else if rule == "code" {
        ch.accent
    } else {
        ch.file
    };
    TabIcon {
        icon: if is_dir { Icon::Folder } else { Icon::File },
        glyph: icons::rule_glyph(Some(rule), None, false),
        color: Some(color),
        hint: None,
    }
}

/// Human-readable byte size for the Files panel.
fn human_size(n: u64) -> String {
    const UNIT: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNIT.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNIT[i])
    }
}

/// A stable texture key for an image: unique across panes.
/// sRGB-encoded `Rgb` → a linear `wgpu::Color` (the surface is `*Srgb`, so clear
/// and quad colours must be linear to avoid a washed-out look).
fn linear_color(c: mtty_ui::theme::Rgb, alpha: f32) -> wgpu::Color {
    fn lin(v: u8) -> f64 {
        let s = v as f64 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    wgpu::Color {
        r: lin(c.0),
        g: lin(c.1),
        b: lin(c.2),
        a: alpha as f64,
    }
}

/// Pick a transparency-capable surface alpha mode when `transparent`, else the
/// first (usually Opaque). Falls back to Opaque if none is suitable.
fn pick_alpha_mode(
    modes: &[wgpu::CompositeAlphaMode],
    transparent: bool,
) -> wgpu::CompositeAlphaMode {
    use wgpu::CompositeAlphaMode as M;
    if transparent {
        // Only straight (post-multiplied) alpha matches our renderer, which
        // outputs non-premultiplied colours. If unsupported, stay opaque.
        if let Some(m) = modes.iter().copied().find(|m| *m == M::PostMultiplied) {
            return m;
        }
    }
    modes.first().copied().unwrap_or(M::Opaque)
}

/// Scissor rectangle (physical px) covering all panes, clamped to the surface.
fn grid_scissor(
    rects: &[(String, Rect)],
    scale: f32,
    width: u32,
    height: u32,
) -> (u32, u32, u32, u32) {
    let (mut minx, mut miny, mut maxx, mut maxy) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for (_, r) in rects {
        minx = minx.min(r.x);
        miny = miny.min(r.y);
        maxx = maxx.max(r.x + r.w);
        maxy = maxy.max(r.y + r.h);
    }
    if rects.is_empty() {
        return (0, 0, 0, 0);
    }
    let sx = (minx * scale).max(0.0).min(width as f32) as u32;
    let sy = (miny * scale).max(0.0).min(height as f32) as u32;
    let sw = ((maxx - minx) * scale).max(0.0) as u32;
    let sh = ((maxy - miny) * scale).max(0.0) as u32;
    (sx, sy, sw.min(width - sx), sh.min(height - sy))
}

/// The URL token under `col` in a line: `(url, start_col, end_col)`.
fn link_at(line: &str, col: u16) -> Option<(String, u16, u16)> {
    let chars: Vec<char> = line.chars().collect();
    let col = col as usize;
    if col >= chars.len() || chars[col].is_whitespace() {
        return None;
    }
    fn is_break(c: char) -> bool {
        c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '(' | ')' | '<' | '>' | '[' | ']')
    }
    let mut start = col;
    while start > 0 && !is_break(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < chars.len() && !is_break(chars[end + 1]) {
        end += 1;
    }
    let raw: String = chars[start..=end].iter().collect();
    let token = raw.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']']);
    if token.starts_with("http://")
        || token.starts_with("https://")
        || token.starts_with("ftp://")
        || token.starts_with("file://")
    {
        let end = start + token.chars().count() - 1;
        Some((token.to_string(), start as u16, end as u16))
    } else {
        None
    }
}

/// Escape shell metacharacters in pasted text (single-quote problem tokens).
fn shell_escape_text(s: &str) -> String {
    s.split_whitespace()
        .map(|tok| {
            if tok
                .chars()
                .all(|c| c.is_alphanumeric() || "/._-@%+=:,~".contains(c))
            {
                tok.to_string()
            } else {
                format!("'{}'", tok.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quote a path for the shell (single quotes when it contains anything
/// unusual), so a dropped file can be pasted into the terminal.
/// A local path as one argument for the pane's shell. Windows shells (cmd,
/// PowerShell) take double quotes, not POSIX single quotes; a dropped path
/// with a space was split into words there.
fn local_path_arg(path: &str) -> String {
    if cfg!(windows) {
        windows_path_arg(path)
    } else {
        shell_quote(path)
    }
}

/// `path` for cmd.exe and PowerShell: bare when it has no space or special
/// character, otherwise in double quotes (a Windows path cannot contain `"`).
fn windows_path_arg(path: &str) -> String {
    let plain = !path.is_empty()
        && !path
            .chars()
            .any(|c| c.is_whitespace() || "&()[]{}^=;!'+,`~$%@#".contains(c));
    if plain {
        path.to_string()
    } else {
        format!("\"{path}\"")
    }
}

/// The command Edit in Tab types into its new tab (ADR 0017): the `editor`
/// config key, else `$EDITOR`, else `vi` (Notepad on Windows), then the path
/// quoted for the pane's shell.
fn edit_in_tab_command(editor: Option<&str>, env_editor: Option<&str>, path: &str) -> String {
    let fallback = if cfg!(windows) { "notepad" } else { "vi" };
    let editor = [editor, env_editor]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|e| !e.is_empty())
        .unwrap_or(fallback);
    format!("{editor} {}", local_path_arg(path))
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_alphanumeric() || "/._-@%+=:,~".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Open a URL in the OS default browser.
fn open_external(target: &str) {
    #[cfg(target_os = "macos")]
    let cmd = ("open", vec![target]);
    #[cfg(target_os = "windows")]
    let cmd = ("cmd", vec!["/C", "start", "", target]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = ("xdg-open", vec![target]);
    let _ = mtty_platform::background_command(cmd.0).args(cmd.1).spawn();
}

fn quad(ox: f32, oy: f32, row: u16, col: u16, cw: f32, ch: f32, color: (u8, u8, u8)) -> Quad {
    Quad::new(
        (ox + col as f32 * cw, oy + row as f32 * ch),
        (ox + (col as f32 + 1.0) * cw, oy + (row as f32 + 1.0) * ch),
        (color.0, color.1, color.2, 255),
    )
}

static NEXT_PANE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn gen_id() -> String {
    format!(
        "pane{}",
        NEXT_PANE.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    )
}

/// Keep new ids clear of `id`, which a reattached pane keeps.
fn reserve_id(id: &str) {
    if let Some(n) = id.strip_prefix("pane").and_then(|n| n.parse::<u32>().ok()) {
        NEXT_PANE.fetch_max(n.saturating_add(1), std::sync::atomic::Ordering::SeqCst);
    }
}

/// An agent's tab icon, by shape first so it reads without colour: half a
/// circle while working, a full one when it finished and the user has not
/// acted on it yet (green) or when it waits for them (amber, red on error,
/// with `!` in the title), an empty ring otherwise.
fn agent_icon(
    ch: &mtty_ui::theme::Chrome,
    state: &str,
    attention: Option<Attention>,
) -> (mtty_ui::icons::Icon, Option<mtty_ui::theme::Rgb>) {
    use mtty_ui::icons::Icon;
    match state {
        "processing" => (Icon::StateBusy, Some(ch.accent)),
        "awaiting" => (Icon::StateWait, Some(ch.warning)),
        "waiting" => (Icon::StateBackground, Some(ch.accent)),
        "incomplete" => (Icon::StatePaused, Some(ch.warning)),
        "unknown" => (Icon::StateUnknown, Some(ch.muted)),
        "completed" => (Icon::StateFull, Some(ch.positive)),
        "error" => (Icon::StateFull, Some(ch.negative)),
        _ if attention == Some(Attention::Done) => (Icon::StateFull, Some(ch.positive)),
        _ => (Icon::StateEmpty, None),
    }
}

/// The agent icon a tab shows, unless `[badges]` switches its state off:
/// then the tab looks like a plain terminal. States the switches do not name
/// still show.
fn shown_agent_icon(
    badges: &mtty_config::Badges,
    ch: &mtty_ui::theme::Chrome,
    state: &str,
    attention: Option<Attention>,
) -> Option<(mtty_ui::icons::Icon, Option<mtty_ui::theme::Rgb>)> {
    (!switched_off(badges, state)).then(|| agent_icon(ch, state, attention))
}

/// `[badges]` turns this state off.
fn switched_off(badges: &mtty_config::Badges, state: &str) -> bool {
    let group = match state {
        "completed" | "incomplete" => "idle",
        "waiting" => "processing",
        "unknown" => "error",
        other => other,
    };
    matches!(group, "processing" | "idle" | "awaiting" | "error") && !badges.enabled(group)
}

/// A hosted pane's host, as the session records it.
fn pane_host(term: &Terminal) -> serde_json::Value {
    if let Some((id, socket)) = term.host_id() {
        return serde_json::json!({ "id": id, "socket": socket });
    }
    serde_json::Value::Null
}

impl ApplicationHandler<HostEvent> for Host {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        // Launch has finished: AppKit's quit handler is in place to replace.
        #[cfg(target_os = "macos")]
        macos_url::install_quit();
        let window_state = WindowState::load();
        let (init_w, init_h) = window_state.size;
        let opacity = mtty_config::Config::load()
            .background_opacity
            .clamp(0.1, 1.0);
        let attrs = Window::default_attributes()
            .with_title(&self.title)
            .with_inner_size(LogicalSize::new(init_w, init_h))
            // The chrome is dark; ask the OS for a dark title bar so the window
            // does not open with a light strip that clashes with the dark chrome.
            .with_theme(Some(winit::window::Theme::Dark))
            .with_transparent(opacity < 1.0);
        #[cfg(windows)]
        let attrs = {
            use winit::platform::windows::{IconExtWindows, WindowAttributesExtWindows};
            let icon = winit::window::Icon::from_resource(1, None).ok();
            attrs
                .with_window_icon(icon.clone())
                .with_taskbar_icon(icon)
                .with_decorations(false)
                .with_undecorated_shadow(true)
        };
        #[cfg(target_os = "macos")]
        let attrs = if unified_titlebar() {
            use winit::platform::macos::WindowAttributesExtMacOS;
            attrs
                .with_titlebar_transparent(true)
                .with_title_hidden(true)
                .with_fullsize_content_view(true)
        } else {
            attrs
        };
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => return startup_failure(event_loop, "could not create a window", e),
        };
        window.set_ime_allowed(true);
        #[cfg(target_os = "macos")]
        if unified_titlebar() {
            disable_native_titlebar_move(&window);
        }

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu_backends(),
            ..Default::default()
        });
        let surface = match instance.create_surface(window.clone()) {
            Ok(s) => s,
            Err(e) => return startup_failure(event_loop, "could not create a drawing surface", e),
        };
        let adapter =
            match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })) {
                Some(a) => a,
                None => {
                    return startup_failure(
                        event_loop,
                        "no usable GPU adapter (Metal, Vulkan or DX12)",
                        "wgpu found none",
                    )
                }
            };
        let (device, queue) = match pollster::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor::default(), None),
        ) {
            Ok(pair) => pair,
            Err(e) => return startup_failure(event_loop, "could not open the GPU device", e),
        };
        let caps = surface.get_capabilities(&adapter);
        let Some(format) = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .or_else(|| caps.formats.first().copied())
        else {
            return startup_failure(
                event_loop,
                "the window surface offers no pixel format",
                "empty surface capabilities",
            );
        };
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: pick_alpha_mode(&caps.alpha_modes, opacity < 1.0),
            view_formats: vec![],
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &config);

        let quads = QuadRenderer::new(&device, format);
        let images = ImageRenderer::new(&device, format);
        let egui_ctx = egui::Context::default();
        egui_extras::install_image_loaders(&egui_ctx);
        install_egui_fonts(&egui_ctx);
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let egui_renderer = egui_wgpu::Renderer::new(&device, format, None, 1, false);

        // Config (ADR: read `~/.config/mtty/config.toml`).
        let (cfg, config_problem) = mtty_config::Config::load_checked();
        let font_size = cfg.font_size;
        let line_ratio = cfg.line_height;
        let font_family = cfg.font_family.clone();
        let lang = mtty_ui::i18n::Lang::parse(cfg.language.as_deref());
        let mut theme = Theme::from_config(&cfg.theme, cfg.cursor_style);
        let theme_name = match cfg.theme_name.as_deref() {
            Some(n) => Theme::NAMES
                .iter()
                .find(|known| known.eq_ignore_ascii_case(n))
                .map(|known| known.to_string())
                .unwrap_or_default(),
            // No `theme` key: the default palette is Nord unless imported.
            None if cfg.imported_from.is_none() => "Nord".to_string(),
            None => String::new(),
        };
        theme.set_preset_name(&theme_name);
        let chrome = theme.chrome();
        configure_egui(&egui_ctx, &chrome);
        let (cw, ch) = State::cell_size(font_size, line_ratio, font_family.as_deref());

        let (jobs_tx, jobs_rx) = std::sync::mpsc::channel();
        // winit implements file drops only for X11; on Wayland we listen on
        // its connection ourselves.
        #[cfg(all(unix, not(target_os = "macos")))]
        let dnd = {
            use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
            match window.display_handle().map(|h| h.as_raw()) {
                // SAFETY: winit's live wl_display; the window (and with it
                // the connection) outlives State's fields.
                Ok(RawDisplayHandle::Wayland(h)) => unsafe {
                    wayland_dnd::Dnd::new(h.display.as_ptr())
                },
                _ => None,
            }
        };
        let mut state = State {
            window,
            #[cfg(all(unix, not(target_os = "macos")))]
            dnd,
            proxy: self.proxy.clone(),
            surface,
            device,
            queue,
            config,
            quads,
            images,
            instance,
            adapter,
            pip: None,
            pip_request: mtty_config::env("PIP").is_some(),
            graphics_enabled: cfg.graphics,
            renderers: HashMap::new(),
            mtp: self.mtp.clone(),
            tabs: Vec::new(),
            active_tab: 0,
            theme,
            rules: mtty_config::view::RuleSet::load(),
            rules_mtime: views_mtime(),
            rules_checked: Instant::now(),
            cw,
            ch,
            font_size,
            default_font_size: font_size,
            line_ratio,
            font_family,
            lang,
            mods: ModifiersState::empty(),
            selection: None,
            pane_menu: None,
            dragging: false,
            dropping: false,
            drag_paths: Vec::new(),
            drag_live: false,
            drag_wake: None,
            drop_choice: None,
            qa_drag: None,
            divider_drag: None,
            mouse_captured: None,
            cursor: (0.0, 0.0),
            // `MTTY_PREEDIT` seeds the IME overlay for captures/QA.
            preedit: mtty_config::env("PREEDIT").unwrap_or_default(),
            ime_area: None,
            show_sidebar: window_state.sidebar_open,
            show_details: window_state.details_open,
            renaming: None,
            rename_buf: String::new(),
            theme_name,
            show_palette: false,
            palette_query: String::new(),
            palette_idx: 0,
            show_settings: false,
            editor: None,
            open_path: String::new(),
            show_open: false,
            recipe_dialog: None,
            recipe_name: String::new(),
            recipe_list: Vec::new(),
            ssh_dialog: None,
            transport_dialog: None,
            key_import: None,
            acp: None,
            acp_writes: HashMap::new(),
            acp_start: None,
            acp_permission: None,
            acp_agents: cfg.acp.agents.clone(),
            pending_transport_connects: 0,
            task_dialog: None,
            tasks_view: None,
            host_book: Default::default(),
            host_book_error: None,
            hosts_view: None,
            tunnels: HashMap::new(),
            sftp_view: None,
            ftp_dialog: None,
            sync: SyncState {
                dir: cfg.sync_dir.clone(),
                key: mtty_config::sync::Key::path()
                    .and_then(|p| mtty_config::sync::Key::load_from(&p).ok().flatten()),
                running: false,
                due: Some(Instant::now()),
                last: None,
            },
            sync_view: None,
            broadcast: false,
            snippet_book: Default::default(),
            snippet_book_error: None,
            snippets_view: None,
            remote_dialog: None,
            editor_vim: cfg.editor_vim,
            editor_command: cfg.editor.clone(),
            vim: cfg.editor_vim.then(mtty_ui::vim::VimRuntime::default),
            vim_for: String::new(),
            cmark: egui_commonmark::CommonMarkCache::default(),
            mmd: Mmd {
                dir: window_file()
                    .map(|p| p.with_file_name("mermaid-cache"))
                    .unwrap_or_default(),
                cmd: cfg.mermaid_command.clone(),
                cache: Default::default(),
                wake: {
                    let proxy = self.proxy.clone();
                    Some(Arc::new(move || {
                        let _ = proxy.send_event(HostEvent::Wake);
                    }))
                },
            },
            recent_files: Vec::new(),
            open_counts: HashMap::new(),
            integration_msg: None,
            read_only: false,
            hint_mode: false,
            hints: Vec::new(),
            tree_expanded: std::collections::HashSet::new(),
            tree_children: HashMap::new(),
            tree_loading: std::collections::HashSet::new(),
            files_filter: String::new(),
            prefix_renaming: None,
            prefix_buf: String::new(),
            mark_renaming: None,
            mark_buf: String::new(),
            group_renaming: None,
            group_buf: String::new(),
            hotkeys: None,
            opacity,
            notifications: cfg.notifications,
            prevent_sleep: cfg.prevent_sleep,
            restore_scrollback: cfg.restore_scrollback,
            ssh_auto_reconnect: cfg.ssh_auto_reconnect,
            pty_host: cfg.pty_host,
            keep_sessions_on_quit: cfg.keep_sessions_on_quit,
            badges: cfg.badges,
            agent_quota_warn: cfg.agent_quota_warn,
            detached_timeout: cfg.detached_timeout,
            recovered: Vec::new(),
            scrollback_saved_at: Instant::now(),
            sleep: mtty_ui::agentloop::SleepGuard::new(),
            agent_states: HashMap::new(),
            composer: None,
            quick: None,
            quick_bg: None,
            quick_hit: None,
            closed: Vec::new(),
            quick_pane: None,
            quick_return: None,
            hover_pointer: false,
            search: None,
            search_idx: 0,
            search_hits: Vec::new(),
            search_key: String::new(),
            editor_hits: Vec::new(),
            editor_hits_query: String::new(),
            find_opts: FindOptions::default(),
            find_error: None,
            find_rejump: false,
            goto_line: None,
            goto_symbol: None,
            resume_picker: None,
            vim_command: None,
            lsp: {
                let proxy = self.proxy.clone();
                let settings = mtty_lsp::Settings {
                    disabled: cfg.lsp.disabled,
                    servers: cfg
                        .lsp
                        .servers
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.clone(),
                                mtty_lsp::ServerSettings {
                                    command: v.command.clone(),
                                    root_markers: v.root_markers.clone(),
                                    disabled: v.disabled,
                                },
                            )
                        })
                        .collect(),
                };
                mtty_lsp::Lsp::new(
                    settings,
                    Arc::new(move || {
                        let _ = proxy.send_event(HostEvent::Wake);
                    }),
                )
            },
            hover_rest: None,
            hover: None,
            completion: None,
            qa_done: false,
            bg_search: None,
            large_edit_offer: None,
            large_loading: None,
            editor_reload_checked: Instant::now(),
            editor_reload_offer: None,
            update_url: cfg.update_check_url.clone(),
            update_auto_check: cfg.update_auto_check,
            update_startup_pending: true,
            update_rx: None,
            update_install: UpdateInstall::Idle,
            update_auto: false,
            update_pubkey: cfg
                .update_pubkey
                .clone()
                .unwrap_or_else(|| mtty_ui::update::RELEASE_PUBKEY.to_string()),
            update_result: None,
            update_notice_until: None,
            notice: None,
            jobs_tx,
            jobs_rx,
            agents_detected: None,
            ui_resize_hover: false,
            #[cfg(windows)]
            window_resize_hover: false,
            wheel_accum: 0.0,
            title_drag_hover: false,
            title_pressed_at: None,
            sidebar_w: window_state.sidebar_w,
            details_w: window_state.details_w,
            alert_target: None,
            saved_settings: Vec::new(),
            config_imported_from: cfg.imported_from,
            update_dialog: false,
            details_tab: mtty_config::env("DETAILS_TAB")
                .and_then(|v| v.parse().ok())
                .unwrap_or(window_state.details_tab),
            details_cwd: None,
            details_data: None,
            details_rx: None,
            details_at: Instant::now(),
            prompt_queue: load_queue(),
            prompt_input: String::new(),
            last_title: None,
            focused: false,
            occluded: false,
            redraw: redraw::Redraw::default(),
            cursor_on: true,
            last_blink: Instant::now(),
            image_wake: None,
            start: Instant::now(),
            shot_now: false,
            egui_ctx,
            egui_state,
            egui_renderer,
        };
        if !state.restore_session() {
            state.new_tab();
        }
        state.find_recovered();
        // Restored splits start their shells at their own size, in every tab,
        // before the shells print a prompt.
        state.fit_all_panes();
        #[cfg(target_os = "macos")]
        if menu_in_os() {
            let proxy = self.proxy.clone();
            match appmenu::install(state.lang, proxy) {
                Some(menu) => self.menu = Some(menu),
                None => eprintln!("mtty: could not install the application menu"),
            }
        }
        let args: Vec<String> = std::env::args().skip(1).collect();
        let intent = mtty_ui::launch::Intent::from_args(&args);
        state.apply_launch(&intent);
        state.saved_settings = state.settings_values();
        state.reload_hosts();
        state.reload_snippets();
        // QA: run a palette command by its English label at startup, so
        // windows that only open from the palette can be captured.
        // `MTTY_QA_AFTER=<secs>` runs it that long after startup instead
        // (once a language server has started, say).
        if mtty_config::env("QA_AFTER").is_none() {
            state.run_qa_command();
        }
        if let Some(problem) = config_problem {
            let msg = format!(
                "{} {problem}",
                mtty_ui::i18n::t(
                    state.lang,
                    "config.toml was ignored:",
                    "config.toml 未生效:"
                )
            );
            state.show_notice(msg);
        }
        if let Some(spec) = cfg.quick_terminal_hotkey.clone() {
            let proxy = self.proxy.clone();
            state.hotkeys = mtty_ui::hotkey::Hotkeys::register(&spec, move || {
                let _ = proxy.send_event(HostEvent::Hotkey);
            });
            if state.hotkeys.is_none() {
                eprintln!("mtty: could not register hotkey {spec}");
            }
        }
        state.window.request_redraw();
        self.state = Some(state);
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: HostEvent) {
        let Some(state) = &mut self.state else {
            return;
        };
        match event {
            // PTY output / MTP work: just repaint.
            HostEvent::Wake => state.redraw.request(),
            // Global Quick Terminal hotkey (ADR 0019): one press toggles the
            // Quick tab and brings the window forward. This event is the only
            // trigger; the hotkey's pending flag is not polled as well.
            HostEvent::Hotkey => {
                state.toggle_quick_terminal();
                state.window.set_visible(true);
                state.window.focus_window();
            }
            // OS menu bar command.
            #[cfg(target_os = "macos")]
            HostEvent::Menu(id, by_key) => {
                if !(by_key && state.menu_key_for_editor(id)) {
                    chrome::Chrome::on_menu(state, id);
                }
                state.window.request_redraw();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(state) = &mut self.state {
            state.poll_control_plane();
            // LSP events and document changes are control-plane work too;
            // hidden editors must not accumulate an undrained event queue.
            state.lsp_frame();
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                let events = state.dnd.as_mut().map(|d| d.poll()).unwrap_or_default();
                let scale = state.window.scale_factor();
                for event in events {
                    match event {
                        wayland_dnd::DropEvent::Hover { x, y } => {
                            if !state.dropping {
                                state.drag_paths.clear();
                                state.drop_choice = None;
                            }
                            state.cursor = (x * scale, y * scale);
                            state.dropping = true;
                            state.drag_live = true;
                        }
                        wayland_dnd::DropEvent::Leave => {
                            state.dropping = false;
                            state.drag_live = false;
                        }
                        wayland_dnd::DropEvent::Drop { paths, x, y } => {
                            state.cursor = (x * scale, y * scale);
                            for path in paths {
                                state.drop_file(path);
                            }
                        }
                    }
                    state.window.request_redraw();
                }
            }
            // The cwd fallback must advance even when an idle/background shell
            // produces no output. This also keeps pending output draining.
            let mut changed = false;
            let active_tab = state.active_tab;
            let focused = state.focused;
            for (ti, tab) in state.tabs.iter_mut().enumerate() {
                for pane in &mut tab.panes {
                    let had_selection = state.selection.is_some();
                    let output = pane.drain_output(&mut state.selection);
                    if had_selection && state.selection.is_none() {
                        state.dragging = false;
                    }
                    changed |= output;
                    // An agent's pane redraws its spinner all the time: its tab
                    // is marked by the agent's state instead.
                    let agent = state.mtp.agent_for(&pane.id).is_some();
                    if output && !agent && (ti != active_tab || !focused) && tab.attention.is_none()
                    {
                        tab.attention = Some(Attention::Unread);
                    }
                    // Publish a newly finished command's output (OSC 133).
                    let latest = pane.term.last_command_output();
                    if latest != pane.published_output.as_ref() {
                        if let Some(out) = latest {
                            state.mtp.set_output(
                                &pane.id,
                                serde_json::json!({
                                    "text": out.text,
                                    "exit": out.exit,
                                    "truncated": out.truncated,
                                }),
                            );
                        }
                        pane.published_output = latest.cloned();
                    }
                }
            }
            if changed {
                state.window.request_redraw();
                if let Some(pip) = &state.pip {
                    pip.window.request_redraw();
                }
            }
            state.reap_exited();
            state.save_scrollback_periodically();
            let drawable = window_drawable(&state.window, state.occluded);
            let mut wake_at = Instant::now() + Duration::from_millis(500);
            for pane in state.tabs.iter().flat_map(|tab| &tab.panes) {
                if let Some(deadline) = pane.term.output_deadline() {
                    wake_at = wake_at.min(deadline);
                }
            }
            // A file drag sends no pointer motion or key events: follow the
            // pointer and Alt from the system so the drop targets track them.
            if drawable && state.dropping && state.qa_drag.is_none() {
                let now = Instant::now();
                if now >= state.drag_wake.unwrap_or(now) {
                    if let Some(at) = drag::pointer_in_window(&state.window) {
                        state.cursor = at;
                        state.drag_live = true;
                    }
                    state.redraw.request();
                    state.drag_wake = Some(now + Duration::from_millis(30));
                }
                wake_at = wake_at.min(state.drag_wake.unwrap());
            } else {
                state.drag_wake = None;
            }
            if let Some(at) = state.image_wake.filter(|_| drawable) {
                if Instant::now() >= at {
                    state.image_wake = None;
                    state.window.request_redraw();
                } else {
                    wake_at = wake_at.min(at);
                }
            }
            // Links opened from a browser or Finder (macOS Apple Events).
            #[cfg(target_os = "macos")]
            for url in macos_url::take() {
                let intent = mtty_ui::launch::Intent::from_args(&[url]);
                state.apply_launch(&intent);
                state.window.request_redraw();
            }
            // Quit from the Dock, logout or AppleScript: mtty's own quit.
            #[cfg(target_os = "macos")]
            if macos_url::take_quit() {
                state.run_command(Cmd::Quit);
            }
            // Launches forwarded by later processes (ADR 0019).
            for line in mtty_ui::launch::drain_inbox() {
                let intent = mtty_ui::launch::Intent::decode(&line);
                state.apply_launch(&intent);
                state.window.request_redraw();
            }
            state.poll_jobs();
            // Agent transitions: every loop iteration, not only on redraw (an
            // occluded window may not be redrawn at all).
            state.agent_loop();
            state.reload_rules_if_changed();
            state.reload_editors_if_changed();
            state.sync_tick();
            if state.update_startup_pending {
                state.update_startup_pending = false;
                if state.update_auto_check {
                    state.check_updates_silent();
                }
            }
            if let Some(rx) = state.update_rx.take() {
                match rx.try_recv() {
                    Ok(result) => {
                        if matches!(result, UpdateResult::Current) {
                            // Successful checks are brief feedback, not a persistent window.
                            if state.update_dialog {
                                state.update_notice_until =
                                    Some(Instant::now() + Duration::from_secs(4));
                            }
                            state.update_dialog = false;
                        }
                        state.update_result = Some(result);
                        if state.update_auto {
                            state.continue_auto_update();
                        }
                        state.window.request_redraw();
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => state.update_rx = Some(rx),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        state.update_result = Some(UpdateResult::Failed(
                            mtty_ui::i18n::t(
                                state.lang,
                                "Update check interrupted.",
                                "更新检查已中断。",
                            )
                            .to_string(),
                        ));
                        state.window.request_redraw();
                    }
                }
            }
            if let Some(until) = state.notice.as_ref().map(|(_, until)| *until) {
                if Instant::now() >= until {
                    state.notice = None;
                    state.window.request_redraw();
                } else {
                    wake_at = wake_at.min(until);
                }
            }
            if let Some(until) = state.update_notice_until {
                if Instant::now() >= until {
                    state.update_notice_until = None;
                    state.window.request_redraw();
                } else {
                    wake_at = wake_at.min(until);
                }
            }
            state.poll_details();
            state.ensure_details();
            if !state.shot_now {
                if let Some(v) = mtty_config::env("SHOT_AFTER")
                    .or_else(|| std::env::var("MIAOTTY_NATIVE_SHOT_AFTER").ok())
                {
                    let secs = v.parse::<f64>().unwrap_or(-1.0);
                    if secs >= 0.0 {
                        let at = state.start + Duration::from_secs_f64(secs);
                        if Instant::now() >= at {
                            state.shot_now = true;
                            state.window.request_redraw();
                        } else {
                            // Ensure we wake even without focus/blink events.
                            wake_at = wake_at.min(at);
                        }
                    }
                }
            }
            // A hover is due once the pointer has rested.
            if let Some(rest) = state.hover_rest.as_ref().filter(|r| drawable && !r.asked) {
                let due = rest.since + HOVER_DELAY;
                if Instant::now() >= due {
                    state.redraw.request();
                } else {
                    wake_at = wake_at.min(due);
                }
            }
            if let Some(secs) = mtty_config::env("QA_AFTER").and_then(|v| v.parse::<f64>().ok()) {
                if !state.qa_done {
                    let at = state.start + Duration::from_secs_f64(secs);
                    if Instant::now() >= at {
                        state.run_qa_command();
                        state.window.request_redraw();
                    } else {
                        wake_at = wake_at.min(at);
                    }
                }
            }
            if state.focused && drawable {
                if state.last_blink.elapsed() >= BLINK {
                    state.cursor_on = !state.cursor_on;
                    state.last_blink = Instant::now();
                    state.window.request_redraw();
                }
                wake_at = wake_at.min(state.last_blink + BLINK);
            }
            let now = Instant::now();
            if let Some(at) = state.redraw.deadline(now, state.focused, drawable) {
                if at <= now {
                    state.window.request_redraw();
                } else {
                    wake_at = wake_at.min(at);
                }
            }
            if let Some(pip) = &state.pip {
                if let Some(at) = pip.redraw.deadline(
                    now,
                    pip.window.has_focus(),
                    window_drawable(&pip.window, pip.occluded),
                ) {
                    if at <= now {
                        pip.window.request_redraw();
                    } else {
                        wake_at = wake_at.min(at);
                    }
                }
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(wake_at));
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if state.pip_request {
            state.pip_request = false;
            state.create_pip(event_loop);
        }
        // Route events for the picture-in-picture window.
        if state
            .pip
            .as_ref()
            .map(|p| p.window.id() == id)
            .unwrap_or(false)
        {
            match event {
                WindowEvent::CloseRequested => state.pip = None,
                WindowEvent::Resized(_) => {
                    state.resize_pip();
                    if let Some(pip) = &state.pip {
                        pip.window.request_redraw();
                    }
                }
                WindowEvent::Occluded(occluded) => {
                    if let Some(pip) = &mut state.pip {
                        pip.occluded = occluded;
                        if !occluded {
                            pip.window.request_redraw();
                        }
                    }
                }
                WindowEvent::Focused(_) => {
                    if let Some(pip) = &state.pip {
                        pip.window.request_redraw();
                    }
                }
                WindowEvent::RedrawRequested => state.render_pip(),
                _ => {}
            }
            return;
        }
        // winit replays keys still held when the window gains focus as
        // synthetic presses. They were never typed here: the Y of the Quick
        // Terminal hotkey (Ctrl+Alt+Y) landed in the new pane as a "y".
        if let WindowEvent::KeyboardInput {
            event,
            is_synthetic: true,
            ..
        } = &event
        {
            if event.state == ElementState::Pressed {
                return;
            }
        }
        let mut ui_consumed = false;
        if let WindowEvent::KeyboardInput { event, .. } = &event {
            if event.state == ElementState::Pressed
                && !state.egui_ctx.wants_keyboard_input()
                && terminal_paste_shortcut(
                    winit_key_kind(event),
                    state.mods.super_key(),
                    state.mods.control_key(),
                    state.mods.shift_key(),
                    state.mods.alt_key(),
                )
            {
                // egui emits Paste only for nonempty clipboard text. Handle
                // terminal paste here so image-only pastes reach the PTY too.
                state.paste_clipboard();
                return;
            }
        }
        // Tab (and Shift+Tab) in the terminal is the shell's completion. egui
        // also treats it as focus navigation: with nothing focused it focuses
        // the first focusable widget (the File menu), after which
        // `wants_keyboard_input` stays true and every key went to that widget
        // instead of the shell, until a click elsewhere. Tab reaches egui only
        // while one of its widgets already has the focus (dialog fields).
        let terminal_tab = keeps_tab_from_egui(&event, state.egui_ctx.wants_keyboard_input());
        // A paste into an egui text field on Wayland: egui's own clipboard no
        // longer sees the selection (see `clipboard_text`), so read it here
        // and hand egui the text instead of the key.
        #[cfg(all(unix, not(target_os = "macos")))]
        let field_paste = state.dnd.is_some()
            && state.egui_ctx.wants_keyboard_input()
            && matches!(&event, WindowEvent::KeyboardInput { event: k, .. }
            if k.state == ElementState::Pressed
                && terminal_paste_shortcut(
                    winit_key_kind(k),
                    state.mods.super_key(),
                    state.mods.control_key(),
                    state.mods.shift_key(),
                    state.mods.alt_key(),
                ));
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        let field_paste = false;
        if field_paste {
            if let Some(text) = state.clipboard_text().filter(|t| !t.is_empty()) {
                state
                    .egui_state
                    .egui_input_mut()
                    .events
                    .push(egui::Event::Paste(text));
            }
            state.window.request_redraw();
        }
        // Empty title-row space stands in for the title bar it covers: a press
        // moves the window, a second one soon after zooms it. The OS drag loop
        // swallows the release, so egui must not see the press either.
        #[cfg(windows)]
        if !state.window.is_maximized() {
            let size = state.window.inner_size();
            let edge = 5.0 * state.window.scale_factor();
            let (x, y) = match &event {
                WindowEvent::CursorMoved { position, .. } => (position.x, position.y),
                _ => state.cursor,
            };
            if let Some(direction) =
                window_resize_edge(x, y, f64::from(size.width), f64::from(size.height), edge)
            {
                use winit::window::{CursorIcon, ResizeDirection};
                let icon = match direction {
                    ResizeDirection::East | ResizeDirection::West => CursorIcon::EwResize,
                    ResizeDirection::North | ResizeDirection::South => CursorIcon::NsResize,
                    ResizeDirection::NorthEast | ResizeDirection::SouthWest => {
                        CursorIcon::NeswResize
                    }
                    ResizeDirection::NorthWest | ResizeDirection::SouthEast => {
                        CursorIcon::NwseResize
                    }
                };
                state.window.set_cursor(icon);
                state.window_resize_hover = true;
                if matches!(
                    &event,
                    WindowEvent::MouseInput {
                        state: ElementState::Pressed,
                        button: MouseButton::Left,
                        ..
                    }
                ) {
                    let _ = state.window.drag_resize_window(direction);
                    return;
                }
            } else if state.window_resize_hover {
                state.window_resize_hover = false;
                state.window.set_cursor(if state.hover_pointer {
                    winit::window::CursorIcon::Pointer
                } else {
                    winit::window::CursorIcon::Default
                });
            }
        }
        if state.title_drag_hover && unified_titlebar() {
            if let WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } = &event
            {
                let now = Instant::now();
                if state
                    .title_pressed_at
                    .is_some_and(|t| now.duration_since(t) < Duration::from_millis(400))
                {
                    state.title_pressed_at = None;
                    state.window.set_maximized(!state.window.is_maximized());
                } else {
                    state.title_pressed_at = Some(now);
                    let _ = state.window.drag_window();
                }
                return;
            }
        }
        if !matches!(event, WindowEvent::RedrawRequested) && !terminal_tab && !field_paste {
            let resp = state.egui_state.on_window_event(&state.window, &event);
            // Just outside a side panel egui does not claim the pointer, but a
            // press on its resize edge must drag the edge, not select text.
            ui_consumed = resp.consumed || state.ui_resize_hover;
            if resp.repaint {
                state.window.request_redraw();
            }
        }
        // Keep coordinates current even when egui owns the pointer. In
        // particular, dragging an overlay must never start a grid selection.
        if let WindowEvent::CursorMoved { position, .. } = &event {
            state.cursor = (position.x, position.y);
        }
        let pointer_event = matches!(
            event,
            WindowEvent::MouseInput { .. }
                | WindowEvent::CursorMoved { .. }
                | WindowEvent::MouseWheel { .. }
        );
        let over_terminal = state.pane_rects().iter().any(|(_, r)| {
            let scale = state.window.scale_factor() as f32;
            r.contains(state.cursor.0 as f32 / scale, state.cursor.1 as f32 / scale)
        });
        let terminal_gesture =
            state.dragging || state.divider_drag.is_some() || state.mouse_captured.is_some();
        if pointer_event && !pointer_to_terminal(ui_consumed, over_terminal, terminal_gesture) {
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                let all: Vec<usize> = (0..state.tabs.len()).collect();
                if !state.confirm_close_tabs(&all) {
                    return;
                }
                state.save_window_state();
                if state.show_settings {
                    state.persist_settings();
                }
                state.save_session_on_exit();
                state.leave_hosts();
                event_loop.exit();
            }
            WindowEvent::Focused(f) => {
                state.focused = f;
                if f {
                    state.follow_recent_alert();
                }
                state.window.request_redraw();
            }
            WindowEvent::Occluded(occluded) => {
                state.occluded = occluded;
                if !occluded {
                    state.window.request_redraw();
                }
            }
            WindowEvent::Resized(_) => {
                state.resize();
                state.window.request_redraw();
            }
            WindowEvent::ModifiersChanged(m) => {
                state.mods = m.state();
                state.window.request_redraw();
            }
            WindowEvent::Ime(winit::event::Ime::Preedit(text, _)) => {
                // Inline composition: drawn at the cursor until it commits.
                if state.preedit != text {
                    state.preedit = text;
                    state.window.request_redraw();
                }
            }
            WindowEvent::Ime(winit::event::Ime::Disabled) => {
                if !state.preedit.is_empty() {
                    state.preedit.clear();
                    state.window.request_redraw();
                }
            }
            WindowEvent::Ime(winit::event::Ime::Commit(text)) => {
                // A commit can carry a control character (Enter/Tab/…); those are
                // already sent by the key handler, so only forward real text.
                let control_only = !text.is_empty() && text.chars().all(char::is_control);
                state.preedit.clear();
                if !control_only && !state.egui_ctx.wants_keyboard_input() {
                    if state.read_only {
                        // Read-only blocks typing into any pane.
                    } else if let Some(ed) = state.active_editor_mut() {
                        if ed.markdown.is_some() {
                            // egui owns IME commits in the live writing surface.
                        } else if ed.is_view_only() {
                            state.large_edit_offer = Some(ed.id.clone());
                        } else if ed.vim.is_none() || ed.vim.as_ref().is_some_and(|v| v.is_insert())
                        {
                            ed.type_text(&text);
                        }
                        state.window.request_redraw();
                    } else {
                        state.write_input(text.as_bytes());
                    }
                }
            }
            WindowEvent::DroppedFile(path) => {
                // The drop point: a drag sends no pointer motion, so the
                // last known position is wherever the pointer left before.
                if let Some(at) = drag::pointer_in_window(&state.window) {
                    state.cursor = at;
                    state.drag_live = true;
                }
                state.drop_file(path);
            }
            WindowEvent::HoveredFile(path) => {
                // One event per dragged file; a new drag starts afresh.
                if !state.dropping {
                    state.drag_paths.clear();
                    state.drag_live = false;
                    state.drop_choice = None;
                }
                state.drag_paths.push(path);
                state.dropping = true;
                state.window.request_redraw();
            }
            WindowEvent::HoveredFileCancelled => {
                state.dropping = false;
                state.drag_live = false;
                state.window.request_redraw();
            }
            WindowEvent::MouseInput {
                state: es, button, ..
            } => match button {
                MouseButton::Left => {
                    let (px, py) = (state.cursor.0 as f32, state.cursor.1 as f32);
                    if es == ElementState::Pressed {
                        // Clicking a split pane must also move keyboard focus.
                        let scale = state.window.scale_factor() as f32;
                        let divider = state
                            .handles()
                            .iter()
                            .any(|h| h.rect.contains(px / scale, py / scale));
                        if !divider {
                            if let Some((id, _)) = state
                                .pane_rects()
                                .into_iter()
                                .find(|(_, r)| r.contains(px / scale, py / scale))
                            {
                                if let Some(tab) = state.tabs.get_mut(state.active_tab) {
                                    if tab.active != id {
                                        tab.active = id;
                                        state.selection = None;
                                        state.preedit.clear();
                                        state.window.request_redraw();
                                    }
                                }
                            }
                        }
                        if !divider {
                            if let Some((row, col)) = state.editor_cell(px, py, false) {
                                let (shift, add) = (state.mods.shift_key(), state.mods.alt_key());
                                // ⌘-click (Ctrl-click elsewhere): go to the definition.
                                let definition = if cfg!(target_os = "macos") {
                                    state.mods.super_key()
                                } else {
                                    state.mods.control_key()
                                };
                                state.hover = None;
                                state.completion = None;
                                if let Some(ed) = state.active_editor_mut() {
                                    ed.press(row, col, shift && !definition, add, Instant::now());
                                    if definition {
                                        ed.dragging = false;
                                    }
                                }
                                if definition {
                                    state.request_definition();
                                }
                                state.window.request_redraw();
                                return;
                            }
                        }
                        // ⌘/Ctrl-click stays a terminal-level link gesture.
                        let linked = if state.mods.super_key() || state.mods.control_key() {
                            match state.link_at_pointer() {
                                Some(hit) => {
                                    open_external(&hit.url);
                                    true
                                }
                                None => false,
                            }
                        } else {
                            false
                        };
                        if !linked {
                            if state.forward_mouse(px, py, 0, true, false) {
                                state.mouse_captured = Some(0);
                            } else {
                                // Prefer a divider under the pointer, else a selection.
                                let scale = state.window.scale_factor() as f32;
                                match divider_at(state.handles(), px, py, scale) {
                                    Some(h) => state.divider_drag = Some((h.path, h.dir, h.area)),
                                    None => {
                                        state.dragging = true;
                                        state.selection = None;
                                    }
                                }
                            }
                        }
                    } else {
                        if let Some(ed) = state.active_editor_mut() {
                            ed.dragging = false;
                        }
                        if state.mouse_captured.take() == Some(0) {
                            state.forward_mouse(px, py, 0, false, false);
                        } else if state.divider_drag.take().is_none() {
                            let ctx = state.egui_ctx.clone();
                            state.copy_selection(&ctx);
                        }
                        state.dragging = false;
                    }
                }
                MouseButton::Right => {
                    let (px, py) = (state.cursor.0 as f32, state.cursor.1 as f32);
                    if es == ElementState::Pressed {
                        if state.forward_mouse(px, py, 2, true, false) {
                            state.mouse_captured = Some(2);
                        } else {
                            // Open the pane context menu at the click. A later
                            // release with no capture does nothing.
                            let scale = state.window.scale_factor() as f32;
                            let logical = (px / scale, py / scale);
                            if let Some((id, outer)) = state
                                .pane_rects()
                                .into_iter()
                                .find(|(_, r)| r.contains(logical.0, logical.1))
                            {
                                let inner = card_inner(outer);
                                let cell = pane_cell_at(inner, state.cw, state.ch, logical);
                                state.pane_menu = Some(PaneMenu {
                                    pane: id,
                                    at: logical,
                                    cell,
                                });
                            } else {
                                state.pane_menu = None;
                            }
                        }
                    } else if state.mouse_captured.take() == Some(2) {
                        state.forward_mouse(px, py, 2, false, false);
                    }
                }
                MouseButton::Middle => {
                    let (px, py) = (state.cursor.0 as f32, state.cursor.1 as f32);
                    if es == ElementState::Pressed {
                        if state.forward_mouse(px, py, 1, true, false) {
                            state.mouse_captured = Some(1);
                        }
                    } else if state.mouse_captured.take() == Some(1) {
                        state.forward_mouse(px, py, 1, false, false);
                    }
                }
                _ => {}
            },
            WindowEvent::CursorMoved { position, .. } => {
                state.cursor = (position.x, position.y);
                if state.mods.super_key() || state.mods.control_key() {
                    state.window.request_redraw();
                }
                let scale = state.window.scale_factor() as f32;
                let (px, py) = (position.x as f32, position.y as f32);
                state.track_hover(px, py);
                if state.active_editor().is_some_and(|e| e.dragging) {
                    if let Some((row, col)) = state.editor_cell(px, py, true) {
                        if let Some(ed) = state.active_editor_mut() {
                            ed.drag_to(row, col);
                            ed.reveal_cursor();
                        }
                        state.window.request_redraw();
                    }
                } else if let Some((path, dir, area)) = state.divider_drag.clone() {
                    let r = divider_ratio(dir, area, px, py, scale);
                    if let Some(tab) = state.tabs.get_mut(state.active_tab) {
                        tab.layout.set_ratio(&path, r);
                    }
                    state.window.request_redraw();
                } else if state.dragging {
                    let rects = state.pane_rects();
                    if let Some((id, r)) = rects.iter().find(|(_, r)| {
                        px >= r.x * scale
                            && px < (r.x + r.w) * scale
                            && py >= r.y * scale
                            && py < (r.y + r.h) * scale
                    }) {
                        let cw = state.cw * scale;
                        let ch = state.ch * scale;
                        let inner = card_inner(*r);
                        let col = ((px - inner.x * scale) / cw).floor().max(0.0) as u16;
                        let row = ((py - inner.y * scale) / ch).floor().max(0.0) as u16;
                        let cell = (row, col);
                        match &mut state.selection {
                            Some((sid, sel)) if sid == id => sel.end = cell,
                            _ => state.selection = Some((id.clone(), Selection::cell(cell))),
                        }
                        state.window.request_redraw();
                    }
                } else if let Some(btn) = state.mouse_captured {
                    // Drag report for the button the application captured.
                    state.forward_mouse(px, py, btn, true, true);
                } else {
                    // Hover report, only if the application asked for motion.
                    state.forward_mouse(px, py, 3, false, true);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                use winit::event::MouseScrollDelta;
                let moved = match delta {
                    MouseScrollDelta::LineDelta(_, y) => f64::from(y),
                    MouseScrollDelta::PixelDelta(p) => {
                        let line_px = f64::from(state.ch) * state.window.scale_factor();
                        p.y / line_px.max(1.0)
                    }
                };
                let lines = wheel_lines(&mut state.wheel_accum, moved);
                if lines != 0 {
                    // Full-screen apps that grabbed the mouse (TUIs like miao)
                    // expect wheel events instead of scrollback navigation.
                    let (px, py) = (state.cursor.0 as f32, state.cursor.1 as f32);
                    let button = if lines > 0 { 64 } else { 65 };
                    let mut consumed = false;
                    for _ in 0..lines.unsigned_abs().min(16) {
                        consumed |= state.forward_mouse(px, py, button, true, false);
                    }
                    if !consumed {
                        let scale = state.window.scale_factor() as f32;
                        let hovered = state
                            .pane_rects()
                            .into_iter()
                            .find(|(_, r)| r.contains(px / scale, py / scale))
                            .map(|(id, _)| id);
                        if let Some(tab) = state.tabs.get_mut(state.active_tab) {
                            if let Some(ed) = tab
                                .editors
                                .iter_mut()
                                .find(|e| Some(&e.id) == hovered.as_ref())
                            {
                                ed.scroll_by(-(lines as isize) * 3);
                            }
                            if let Some(pane) = tab
                                .panes
                                .iter_mut()
                                .find(|p| Some(&p.id) == hovered.as_ref())
                                .filter(|p| p.scroll == 0 && p.term.screen().alternate_scroll())
                            {
                                // vim, less, man: arrow keys, as other terminals do.
                                let app_cursor = pane.term.screen().application_cursor();
                                pane.term.write(&alternate_scroll_keys(lines, app_cursor));
                            } else if let Some(pane) = tab
                                .panes
                                .iter_mut()
                                .find(|p| Some(&p.id) == hovered.as_ref())
                            {
                                let max = pane.term.screen().scrollback_len();
                                if lines > 0 {
                                    pane.scroll = (pane.scroll + lines as usize).min(max);
                                } else {
                                    pane.scroll = pane.scroll.saturating_sub((-lines) as usize);
                                }
                            }
                        }
                    }
                }
                state.window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // An editor pane takes its keys first: its chords (⌘D next
                // occurrence, ⇧⌘Z redo) win over the app's while it has focus.
                // A focused text field (Find, the palette, a rename) has the
                // keyboard instead: typing there must not edit the file.
                if editor_takes_keys(
                    state.active_editor().is_some(),
                    state.hint_mode,
                    state.egui_ctx.wants_keyboard_input(),
                ) {
                    if event.state == ElementState::Pressed && state.editor_key(&event) {
                        state.window.request_redraw();
                        return;
                    }
                    if shortcut(&event, state.mods).is_none() {
                        return;
                    }
                }
                if state.hint_mode && event.state == ElementState::Pressed {
                    {
                        let key = match &event.logical_key {
                            Key::Character(c) => c.chars().next(),
                            Key::Named(NamedKey::Escape) => Some('\u{1b}'),
                            _ => None,
                        };
                        if let Some(ch) = key {
                            if ch == '\u{1b}' {
                                state.cancel_hints();
                            } else if let Some(h) = state
                                .hints
                                .iter()
                                .find(|h| h.label.starts_with(ch))
                                .cloned()
                            {
                                state.cancel_hints();
                                if h.is_path {
                                    state.open_editor(std::path::PathBuf::from(&h.target));
                                } else {
                                    open_external(&h.target);
                                }
                            }
                            state.window.request_redraw();
                            return;
                        }
                    }
                }
                if state.egui_ctx.wants_keyboard_input() {
                    state.window.request_redraw();
                    return;
                }
                if let Some(s) = shortcut(&event, state.mods) {
                    if s.new_tab {
                        state.new_tab();
                    }
                    if s.reopen {
                        state.reopen_tab();
                    }
                    if s.quick_terminal {
                        state.toggle_quick_terminal();
                    }
                    if s.close {
                        state.close_pane();
                    }
                    if let Some(i) = s.select {
                        if i < state.tabs.len() {
                            state.active_tab = i;
                            state.selection = None;
                        }
                    }
                    if s.font != 0.0 {
                        state.font_size = (state.font_size + s.font).clamp(6.0, 40.0);
                        let (cw, ch) = State::cell_size(
                            state.font_size,
                            state.line_ratio,
                            state.font_family.as_deref(),
                        );
                        state.cw = cw;
                        state.ch = ch;
                        state.resize();
                    }
                    if s.split_right {
                        state.split(SplitDir::Right);
                    }
                    if s.split_down {
                        state.split(SplitDir::Down);
                    }
                    if s.cycle != 0 {
                        state.cycle_pane(s.cycle > 0);
                    }
                    if s.tab != 0 {
                        state.cycle_tab(s.tab > 0);
                    }
                    if s.hint {
                        state.build_hints();
                    }
                    if s.find != 0 {
                        let n = state.search_count();
                        if state.search.is_some() && n > 0 {
                            state.search_idx =
                                ((state.search_idx as i32 + s.find).rem_euclid(n as i32)) as usize;
                            state.scroll_to_search_hit();
                        }
                    }
                    if s.toggle_sidebar {
                        state.toggle_sidebar();
                    }
                    if s.toggle_details {
                        state.toggle_details();
                    }
                    if s.palette {
                        state.show_palette = true;
                        state.palette_query.clear();
                        state.palette_idx = 0;
                    }
                    if s.settings {
                        state.show_settings = true;
                    }
                    if s.composer {
                        state.composer = Some(String::new());
                    }
                    if s.quickly {
                        state.quick = Some(String::new());
                    }
                    if s.search {
                        state.search = Some(String::new());
                        state.search_idx = 0;
                        state.search_key.clear();
                    }
                    state.window.request_redraw();
                } else {
                    // winit emits a `KeyboardInput` for key-up as well as key-down
                    // (macOS always does). `encode_key` returns a sequence for
                    // special keys no matter the state, so acting on releases sent
                    // every key twice — one Return became a blank line. Releases
                    // are input only when the app asked for kitty event types.
                    let active = state
                        .tabs
                        .get(state.active_tab)
                        .and_then(|t| t.panes.iter().find(|p| p.id == t.active));
                    let kitty = active.map(|p| p.term.screen().kitty_flags()).unwrap_or(0);
                    let pressed = event.state == ElementState::Pressed;
                    if !pressed && (kitty & input::KITTY_REPORT_EVENTS) == 0 {
                        return;
                    }
                    let mods = input::Modifiers {
                        ctrl: state.mods.control_key(),
                        alt: state.mods.alt_key(),
                        shift: state.mods.shift_key(),
                        sup: state.mods.super_key(),
                    };
                    let opts = input::EncodeOpts {
                        app_cursor: active
                            .map(|p| p.term.screen().application_cursor())
                            .unwrap_or(false),
                        bracketed: active
                            .map(|p| p.term.screen().bracketed_paste())
                            .unwrap_or(false),
                        kitty,
                        event: if !pressed {
                            3
                        } else if event.repeat {
                            2
                        } else {
                            1
                        },
                        has_selection: state.selection.is_some(),
                    };
                    let kind = winit_key_kind(&event);
                    let alt = input::KittyAlternates {
                        unshifted: kitty_unshifted_code(&event),
                        shifted: if mods.shift {
                            kitty_logical_code(&event)
                        } else {
                            0
                        },
                        base: kitty_base_layout_code(event.physical_key),
                    };
                    // Ctrl+Shift+C copies (egui turns it into a Copy event);
                    // it must not also reach the shell as ^C.
                    if !cfg!(target_os = "macos")
                        && mods.ctrl
                        && mods.shift
                        && !mods.alt
                        && matches!(kind, input::KeyKind::Char('c' | 'C'))
                    {
                        return;
                    }
                    // Special keys are encoded below; only plain text keys (letters,
                    // space, symbols) carry a `text` payload to send here — otherwise
                    // Enter/Tab/etc. would be sent twice. With kitty's "all keys as
                    // escape codes" the key is encoded below instead.
                    let special = !matches!(kind, input::KeyKind::Char(_) | input::KeyKind::Other);
                    let mut bytes = Vec::new();
                    if !special && pressed && (kitty & input::KITTY_REPORT_ALL_KEYS) == 0 {
                        if let Some(text) = &event.text {
                            if !mods.ctrl && !mods.sup && !text.is_empty() {
                                bytes.extend_from_slice(&input::encode_text(text));
                            }
                        }
                    }
                    bytes.extend_from_slice(&input::encode_key_full(
                        kind,
                        mods,
                        opts,
                        alt,
                        event.text.as_deref(),
                    ));
                    state.write_input(&bytes);
                }
            }
            WindowEvent::RedrawRequested => state.render(),
            _ => {}
        }
    }
}

/// Case-insensitive matches of `query` in a line's cells (see
/// `ATerm::line_chars_abs`), as (start column, width in cells). Columns come
/// from the cells, so wide characters before or inside a match line up.
/// Documents larger than this are searched on a thread, so typing in Find
/// never waits on a scan.
const BG_SEARCH_BYTES: usize = 8 << 20;

/// Matches kept for one search.
const MAX_SEARCH_HITS: usize = 100_000;

/// Open Quickly starts a content search only from this many characters up:
/// one letter matches almost every line of every file.
const QUICK_CONTENT_MIN_CHARS: usize = 2;

/// Recent scrollback lines captured for an Open Quickly search.
const QUICK_SCROLL_LINES: usize = 5_000;

/// Cells captured for the scrollback search, whichever comes first.
const QUICK_SCROLL_CELLS: usize = 1_000_000;

/// Scrollback matches kept for one Open Quickly search.
const QUICK_SCROLL_MAX_HITS: usize = 200;

/// The active tab's editor pane, borrowing only `tabs` (so other fields
/// stay free to change).
fn active_editor_of(tabs: &[Tab], active_tab: usize) -> Option<&editor_pane::EditorPane> {
    let tab = tabs.get(active_tab)?;
    tab.editors.iter().find(|e| e.id == tab.active)
}

/// How long the pointer rests on editor text before a hover shows.
const HOVER_DELAY: Duration = Duration::from_millis(450);

/// Completion rows shown at once.
const COMPLETION_ROWS: usize = 10;

/// The pointer resting on editor text.
struct HoverRest {
    pane: String,
    at: usize,
    /// The word under it: moving within it keeps the rest.
    word: (usize, usize),
    since: Instant,
    /// Where the popup goes (points).
    pos: (f32, f32),
    asked: bool,
}

/// A hover: the diagnostics at a spot and, once it arrives, the server's
/// Markdown.
struct HoverPopup {
    pane: String,
    path: std::path::PathBuf,
    at: usize,
    pos: (f32, f32),
    markdown: Option<String>,
    diagnostics: Vec<(u8, String)>,
}

/// The completion list of an editor pane.
struct CompletionPopup {
    pane: String,
    /// Where the word being completed starts.
    start: usize,
    /// The newest request; older answers are dropped.
    ticket: u64,
    items: Vec<mtty_lsp::CompletionItem>,
    encoding: mtty_lsp::Encoding,
    /// Indices into `items` matching what was typed, best first.
    shown: Vec<usize>,
    selected: usize,
    /// The server said the list is partial: ask again as typing goes on.
    incomplete: bool,
    waiting: bool,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Where the word ending at `caret` starts.
fn word_start(rope: &mtty_editor::Rope, caret: usize) -> usize {
    let mut start = caret;
    while start > 0 && is_word_char(rope.char(start - 1)) {
        start -= 1;
    }
    start
}

/// The items matching `typed`, best first: those starting with it (case
/// ignored), then those containing its letters in order; each group in the
/// server's order (`sortText`).
fn filter_completions(items: &[mtty_lsp::CompletionItem], typed: &str) -> Vec<usize> {
    let typed: Vec<char> = typed.chars().flat_map(char::to_lowercase).collect();
    let mut ranked: Vec<(u8, &str, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let filter: Vec<char> = item.filter.chars().flat_map(char::to_lowercase).collect();
            let rank = if filter.starts_with(&typed) {
                0
            } else {
                let mut rest = filter.iter();
                if !typed.iter().all(|c| rest.any(|f| f == c)) {
                    return None;
                }
                1
            };
            Some((rank, item.sort.as_str(), i))
        })
        .collect();
    ranked.sort();
    ranked.into_iter().map(|(_, _, i)| i).collect()
}

/// What accepting `item` writes, with the caret at `caret` in a word that
/// starts at `start`: the item's text over its range (or the word) up to
/// the caret, plus its other edits (an import) that do not touch that; and
/// where the caret goes after.
fn completion_edit(
    rope: &mtty_editor::Rope,
    caret: usize,
    start: usize,
    item: &mtty_lsp::CompletionItem,
    encoding: mtty_lsp::Encoding,
) -> (mtty_editor::Transaction, usize) {
    use mtty_editor::{Assoc, Change, Transaction};
    let to_char = |p| mtty_lsp::pos_to_char(rope, p, encoding);
    let start = item
        .range
        .map(|r| to_char(r.start))
        .unwrap_or(start)
        .min(caret);
    let mut changes = vec![Change::replace(start, caret, item.insert.clone())];
    for (range, text) in &item.additional {
        let (a, b) = (to_char(range.start), to_char(range.end));
        // Never an edit across the completion itself.
        if b <= start || a >= caret.max(start + 1) {
            changes.push(Change::replace(a, b.max(a), text.clone()));
        }
    }
    changes.sort_by_key(|ch| (ch.start, ch.end));
    changes.dedup_by(|later, earlier| later.start < earlier.end);
    let tx = Transaction::new(changes);
    let offset = item.cursor.unwrap_or_else(|| item.insert.chars().count());
    let after = tx.map(start, Assoc::Before) + offset;
    (tx, after)
}

/// A letter for a completion's kind (the protocol's numbering).
fn completion_kind_letter(kind: u8) -> &'static str {
    match kind {
        2..=4 => "ƒ",
        5 | 10 => "·",
        6 => "v",
        7 | 22 => "C",
        8 => "I",
        9 => "M",
        13 => "E",
        14 => "k",
        15 => "s",
        21 => "c",
        25 => "T",
        _ => " ",
    }
}

/// A chrome palette colour as an egui colour.
fn chrome_rgb(c: mtty_ui::theme::Rgb) -> egui::Color32 {
    egui::Color32::from_rgb(c.0, c.1, c.2)
}

/// A diagnostic's colour: error, warning, information, hint.
fn severity_rgb(severity: u8) -> (u8, u8, u8) {
    match severity {
        1 => (0xe0, 0x6c, 0x75),
        2 => (0xe5, 0xc0, 0x7b),
        3 => (0x61, 0xaf, 0xef),
        _ => (0x7f, 0x84, 0x8e),
    }
}

fn severity_color32(severity: u8) -> egui::Color32 {
    let (r, g, b) = severity_rgb(severity);
    egui::Color32::from_rgb(r, g, b)
}

fn no_server_notice(lang: mtty_ui::i18n::Lang) -> String {
    mtty_ui::i18n::t(
        lang,
        "No language server for this file (see [lsp] in config.toml).",
        "此文件没有可用的语言服务器(见 config.toml 中的 [lsp])。",
    )
    .to_string()
}

/// How Find matches in an editor pane, and its replace field (shown while
/// `Some`). A file in view mode is searched as literal text, ignoring case.
#[derive(Default)]
struct FindOptions {
    case_sensitive: bool,
    whole_word: bool,
    regex: bool,
    replace: Option<String>,
}

/// A search running on a thread; dropping it cancels the thread.
struct BgSearch {
    hits: Arc<std::sync::Mutex<Vec<(usize, usize)>>>,
    done: Arc<std::sync::atomic::AtomicBool>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Select the first match after the caret once matches arrive.
    jump: bool,
    /// Hits are file byte ranges (view mode), not char ranges.
    bytes: bool,
}

impl Drop for BgSearch {
    fn drop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Open Quickly's content search; dropping it cancels the scan. The key is
/// the query plus the directory and pane it was started for, so the window
/// restarts the scan only when one of those changes.
struct BgQuickContent {
    key: String,
    file_hits: Arc<std::sync::Mutex<Vec<quick_content::FileHit>>>,
    scroll_hits: Arc<std::sync::Mutex<Vec<quick_content::ScrollHit>>>,
    done: Arc<std::sync::atomic::AtomicBool>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for BgQuickContent {
    fn drop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Memory to load a file for editing, from its size: measured at about
/// 2.2 times the file at the peak of loading (the bytes read plus the rope).
fn edit_memory_estimate(bytes: u64) -> u64 {
    bytes * 22 / 10
}

/// A byte count as people read it ("1.4 GB", "512 MB").
fn human_bytes(bytes: u64) -> String {
    const GB: f64 = (1u64 << 30) as f64;
    const MB: f64 = (1u64 << 20) as f64;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else {
        format!("{:.0} MB", (b / MB).max(1.0))
    }
}

/// Keep IME input on for the terminals and editors. egui-winit allows IME
/// only while an egui text field has focus: once a field (Find, the palette,
/// a rename) lost focus it turned IME off for the whole window, and mtty never
/// turned it back on, so typing gave plain letters until a restart while the
/// input method still showed Chinese. With no field focused, report the
/// caret as egui's IME area, which keeps IME on and places the candidate
/// window there.
fn keep_ime_on(output: &mut egui::PlatformOutput, caret: Option<egui::Rect>) {
    if output.ime.is_none() {
        let rect = caret
            .unwrap_or_else(|| egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1.0, 1.0)));
        output.ime = Some(egui::output::IMEOutput {
            rect,
            cursor_rect: rect,
        });
    }
}

/// Where a tab is, for its sidebar row's hover: the ssh target, the active
/// editor's file, or the active terminal's folder (`~` for home).
fn tab_location(tab: &Tab) -> String {
    if tab.ssh {
        if let Some(target) = tab.ssh_target.as_deref().or(tab.ssh_cmd.as_deref()) {
            return format!("ssh {target}");
        }
    }
    if let Some(target) = &tab.transport {
        let tag = if target.plaintext() {
            " (unencrypted)"
        } else {
            ""
        };
        return format!("{}{tag}", target.label());
    }
    let path = match tab.editors.iter().find(|e| e.id == tab.active) {
        Some(ed) => ed.path.display().to_string(),
        None => match tab.panes.iter().find(|p| p.id == tab.active) {
            Some(pane) => pane.term.cwd().unwrap_or("").to_string(),
            None => String::new(),
        },
    };
    home_relative(&path, std::env::var("HOME").ok().as_deref())
}

/// `path` with the home folder shown as `~`.
fn home_relative(path: &str, home: Option<&str>) -> String {
    match home.filter(|h| !h.is_empty() && *h != "/") {
        Some(home) if path == home => "~".to_string(),
        Some(home) => match path.strip_prefix(home) {
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => path.to_string(),
        },
        None => path.to_string(),
    }
}

/// Read a `.ppk`, convert it and write the encrypted OpenSSH key and its
/// `.pub` beside it (ADR 0038). Runs on a background thread.
fn import_key_file(
    path: &str,
    old: &str,
    new: &str,
    dest: &std::path::Path,
    overwrite: bool,
) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let imported = mtty_keys::import_ppk(&text, old, new)?;
    let dir = dest.parent().ok_or("the destination has no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if dest.exists() && !overwrite {
        return Err(format!("{} already exists", dest.display()));
    }
    std::fs::write(dest, imported.private_openssh.as_bytes())
        .map_err(|e| format!("{}: {e}", dest.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o600));
    }
    let pub_path = std::path::PathBuf::from(format!("{}.pub", dest.display()));
    std::fs::write(&pub_path, imported.public_openssh.as_bytes())
        .map_err(|e| format!("{}: {e}", pub_path.display()))?;
    Ok(dest.display().to_string())
}

/// The editor command a menu shortcut means while an editor pane has the
/// keyboard: the menu items whose shortcuts are editor chords.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn editor_command_for_menu_key(id: chrome::MenuId) -> Option<editor_pane::Command> {
    use chrome::MenuId;
    Some(match id {
        MenuId::SplitRight => editor_pane::Command::SelectNextOccurrence,
        MenuId::ReopenClosed => editor_pane::Command::Redo,
        MenuId::ToggleSidebar => editor_pane::Command::SelectAllOccurrences,
        _ => return None,
    })
}

/// Translate a key chord into a vim key (the pane owns the vim state).
fn vim_key_from(kind: mtty_ui::input::KeyKind, ctrl: bool) -> Option<mtty_editor::vim::Key> {
    use mtty_editor::vim::Key;
    Some(match kind {
        mtty_ui::input::KeyKind::Char(c) if ctrl => Key::Ctrl(c),
        mtty_ui::input::KeyKind::Char(c) => Key::Char(c),
        mtty_ui::input::KeyKind::Left => Key::Left,
        mtty_ui::input::KeyKind::Right => Key::Right,
        mtty_ui::input::KeyKind::Up => Key::Up,
        mtty_ui::input::KeyKind::Down => Key::Down,
        mtty_ui::input::KeyKind::Home => Key::Home,
        mtty_ui::input::KeyKind::End => Key::End,
        mtty_ui::input::KeyKind::Backspace => Key::Backspace,
        mtty_ui::input::KeyKind::Delete => Key::Delete,
        mtty_ui::input::KeyKind::Enter => Key::Enter,
        mtty_ui::input::KeyKind::Escape => Key::Esc,
        _ => return None,
    })
}

fn t_replace_failed(lang: mtty_ui::i18n::Lang) -> &'static str {
    mtty_ui::i18n::t(lang, "Replace failed", "替换失败")
}

/// Whether a key goes to the active editor pane before the app's shortcuts:
/// not while hint mode reads labels, and not while an egui text field (Find,
/// the palette, a rename) has the keyboard, so typing there never edits the
/// file.
fn editor_takes_keys(editor_active: bool, hint_mode: bool, text_field_focused: bool) -> bool {
    editor_active && !hint_mode && !text_field_focused
}

/// The match a new search starts at: the first at or after the caret,
/// wrapping to the first.
fn first_hit_from(hits: &[(usize, usize)], caret: usize) -> usize {
    hits.iter()
        .position(|&(start, _)| start >= caret)
        .unwrap_or(0)
}

fn find_in_cells(cells: &[(u16, char, u16)], query: &str) -> Vec<(u16, u16)> {
    let fold = |c: char| c.to_lowercase().next().unwrap_or(c);
    let q: Vec<char> = query.chars().map(fold).collect();
    let mut out = Vec::new();
    if q.is_empty() {
        return out;
    }
    let mut i = 0;
    while i + q.len() <= cells.len() {
        if cells[i..i + q.len()]
            .iter()
            .zip(&q)
            .all(|((_, c, _), qc)| fold(*c) == *qc)
        {
            let (start, _, _) = cells[i];
            let (last, _, w) = cells[i + q.len() - 1];
            out.push((start, last + w - start));
            i += q.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Whether a key event is a Tab meant for the terminal, which egui must not
/// see (it would move its focus away from the terminal).
/// The wgpu backends mtty draws with. On Windows the OpenGL (WGL) backend is
/// left out: probing it leaves a thread owning a hidden window ("wgpu Device
/// Class") that never processes messages, and Windows tells every thread's
/// windows about an input-language change synchronously, so switching to an
/// input method hung mtty for good. DX12 and Vulkan cover Windows 10 and 11.
/// `WGPU_BACKEND` (for example `gl`) still overrides.
fn wgpu_backends() -> wgpu::Backends {
    wgpu::util::backend_bits_from_env().unwrap_or(if cfg!(windows) {
        wgpu::Backends::DX12 | wgpu::Backends::VULKAN
    } else {
        wgpu::Backends::all()
    })
}

fn keeps_tab_from_egui(event: &WindowEvent, egui_has_focus: bool) -> bool {
    !egui_has_focus
        && matches!(
            event,
            WindowEvent::KeyboardInput { event, .. }
                if matches!(event.logical_key, Key::Named(NamedKey::Tab))
        )
}

fn terminal_paste_shortcut(
    key: input::KeyKind,
    super_key: bool,
    control: bool,
    shift: bool,
    alt: bool,
) -> bool {
    if alt {
        return false;
    }
    if cfg!(target_os = "macos") {
        super_key && !control && !shift && matches!(key, input::KeyKind::Char('v' | 'V'))
    } else {
        // Ctrl+Shift+V as in other Linux/Windows terminals; Ctrl+V too.
        let _ = shift;
        !super_key && control && matches!(key, input::KeyKind::Char('v' | 'V'))
    }
}

/// The key of a chord: the character without modifiers in the user's
/// layout, so ⌥ and Ctrl do not change it.
fn chord_key_kind(event: &KeyEvent) -> input::KeyKind {
    use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
    key_kind(&event.key_without_modifiers())
}

fn winit_key_kind(event: &KeyEvent) -> input::KeyKind {
    key_kind(&event.logical_key)
}

/// The unshifted codepoint of a character key in the active layout, for the
/// kitty `unicode-key-code` field.
fn kitty_unshifted_code(event: &KeyEvent) -> u32 {
    use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
    key_codepoint(&event.key_without_modifiers())
}

/// The codepoint of the key with its modifiers applied, i.e. the shifted key.
fn kitty_logical_code(event: &KeyEvent) -> u32 {
    key_codepoint(&event.logical_key)
}

fn key_codepoint(key: &Key) -> u32 {
    match key {
        Key::Character(s) => s.chars().next().map(u32::from).unwrap_or(0),
        _ => 0,
    }
}

/// The key at the same physical position in the standard PC-101 layout, for
/// kitty's `base-layout-key` field. 0 for keys without such a position, which
/// makes the caller omit the sub-field.
fn kitty_base_layout_code(physical: winit::keyboard::PhysicalKey) -> u32 {
    use winit::keyboard::{KeyCode, PhysicalKey};
    let PhysicalKey::Code(code) = physical else {
        return 0;
    };
    let ch = match code {
        KeyCode::KeyA => 'a',
        KeyCode::KeyB => 'b',
        KeyCode::KeyC => 'c',
        KeyCode::KeyD => 'd',
        KeyCode::KeyE => 'e',
        KeyCode::KeyF => 'f',
        KeyCode::KeyG => 'g',
        KeyCode::KeyH => 'h',
        KeyCode::KeyI => 'i',
        KeyCode::KeyJ => 'j',
        KeyCode::KeyK => 'k',
        KeyCode::KeyL => 'l',
        KeyCode::KeyM => 'm',
        KeyCode::KeyN => 'n',
        KeyCode::KeyO => 'o',
        KeyCode::KeyP => 'p',
        KeyCode::KeyQ => 'q',
        KeyCode::KeyR => 'r',
        KeyCode::KeyS => 's',
        KeyCode::KeyT => 't',
        KeyCode::KeyU => 'u',
        KeyCode::KeyV => 'v',
        KeyCode::KeyW => 'w',
        KeyCode::KeyX => 'x',
        KeyCode::KeyY => 'y',
        KeyCode::KeyZ => 'z',
        KeyCode::Digit0 => '0',
        KeyCode::Digit1 => '1',
        KeyCode::Digit2 => '2',
        KeyCode::Digit3 => '3',
        KeyCode::Digit4 => '4',
        KeyCode::Digit5 => '5',
        KeyCode::Digit6 => '6',
        KeyCode::Digit7 => '7',
        KeyCode::Digit8 => '8',
        KeyCode::Digit9 => '9',
        KeyCode::Backquote => '`',
        KeyCode::Minus => '-',
        KeyCode::Equal => '=',
        KeyCode::BracketLeft => '[',
        KeyCode::BracketRight => ']',
        KeyCode::Backslash => '\\',
        KeyCode::Semicolon => ';',
        KeyCode::Quote => '\'',
        KeyCode::Comma => ',',
        KeyCode::Period => '.',
        KeyCode::Slash => '/',
        KeyCode::Space => ' ',
        _ => return 0,
    };
    u32::from(ch)
}

fn key_kind(key: &Key) -> input::KeyKind {
    use input::KeyKind;
    match key {
        Key::Character(s) => s
            .chars()
            .next()
            .map(KeyKind::Char)
            .unwrap_or(KeyKind::Other),
        Key::Named(n) => match n {
            NamedKey::Enter => KeyKind::Enter,
            NamedKey::Backspace => KeyKind::Backspace,
            NamedKey::Tab => KeyKind::Tab,
            NamedKey::Escape => KeyKind::Escape,
            NamedKey::ArrowUp => KeyKind::Up,
            NamedKey::ArrowDown => KeyKind::Down,
            NamedKey::ArrowLeft => KeyKind::Left,
            NamedKey::ArrowRight => KeyKind::Right,
            NamedKey::Home => KeyKind::Home,
            NamedKey::End => KeyKind::End,
            NamedKey::Delete => KeyKind::Delete,
            NamedKey::PageUp => KeyKind::PageUp,
            NamedKey::PageDown => KeyKind::PageDown,
            NamedKey::Insert => KeyKind::Insert,
            NamedKey::F1 => KeyKind::F(1),
            NamedKey::F2 => KeyKind::F(2),
            NamedKey::F3 => KeyKind::F(3),
            NamedKey::F4 => KeyKind::F(4),
            NamedKey::F5 => KeyKind::F(5),
            NamedKey::F6 => KeyKind::F(6),
            NamedKey::F7 => KeyKind::F(7),
            NamedKey::F8 => KeyKind::F(8),
            NamedKey::F9 => KeyKind::F(9),
            NamedKey::F10 => KeyKind::F(10),
            NamedKey::F11 => KeyKind::F(11),
            NamedKey::F12 => KeyKind::F(12),
            _ => KeyKind::Other,
        },
        _ => KeyKind::Other,
    }
}

// ---- helpers ---------------------------------------------------------------

fn layout_to_json(l: &Layout) -> serde_json::Value {
    match l {
        Layout::Leaf(id) => serde_json::json!({ "leaf": id }),
        Layout::Split { dir, ratio, a, b } => serde_json::json!({
            "dir": match dir.axis() {
                SplitDir::Right => "right",
                SplitDir::Down => "down",
                SplitDir::Left | SplitDir::Up => unreachable!("axis() yields only Right/Down"),
            },
            "ratio": ratio,
            "a": layout_to_json(a),
            "b": layout_to_json(b),
        }),
    }
}

fn json_to_layout(
    v: &serde_json::Value,
    map: &std::collections::HashMap<String, String>,
) -> Option<Layout> {
    if let Some(id) = v.get("leaf").and_then(|x| x.as_str()) {
        return Some(Layout::leaf(
            map.get(id).cloned().unwrap_or_else(|| id.to_string()),
        ));
    }
    let dir = match v.get("dir").and_then(|x| x.as_str())? {
        "right" => SplitDir::Right,
        _ => SplitDir::Down,
    };
    let ratio = v.get("ratio").and_then(|x| x.as_f64()).unwrap_or(0.5) as f32;
    let a = json_to_layout(v.get("a")?, map)?;
    let b = json_to_layout(v.get("b")?, map)?;
    Some(Layout::Split {
        dir,
        ratio,
        a: Box::new(a),
        b: Box::new(b),
    })
}

/// What drawing an editor pane needs from the frame.
struct EditorFrame<'a> {
    scale: f32,
    cw: f32,
    ch: f32,
    theme: &'a mtty_ui::theme::Theme,
    panel_bg: mtty_ui::theme::Rgb,
    focused: bool,
    carets_on: bool,
    /// Find matches (char ranges) and the current one's index.
    matches: &'a [(usize, usize)],
    current_match: usize,
}

/// An editor pane's card, current-line band, selection and carets as quads,
/// and its glyph rows, in the same pipeline as a terminal pane.
fn draw_editor(
    ed: &mut editor_pane::EditorPane,
    id: &str,
    r: Rect,
    f: EditorFrame<'_>,
) -> PaneDraw {
    let inner = card_inner(r);
    let cols = ((inner.w * f.scale) / f.cw).floor().max(1.0) as usize;
    let rows = ((inner.h * f.scale) / f.ch).floor().max(1.0) as usize;
    ed.resize(cols, rows);
    ed.sync_syntax();
    if ed.markdown.is_some() {
        let card = card_rect(r);
        let bg = f.panel_bg;
        return PaneDraw {
            id: id.to_string(),
            rect: inner,
            rows: Vec::new(),
            quads: vec![Quad::rounded(
                (card.x * f.scale, card.y * f.scale),
                ((card.x + card.w) * f.scale, (card.y + card.h) * f.scale),
                (bg.0, bg.1, bg.2, 255),
                CARD_RADIUS * f.scale,
            )],
        };
    }
    let chrome = f.theme.chrome();
    let rgb = |c: mtty_ui::theme::Rgb| (c.0, c.1, c.2);
    let d = ed.draw(
        editor_pane::Palette {
            fg: rgb(f.theme.fg),
            gutter: rgb(chrome.muted),
            gutter_current: rgb(chrome.text),
        },
        f.focused,
        f.carets_on,
    );
    let (ox, oy) = (inner.x * f.scale, inner.y * f.scale);
    let (cw, ch) = (f.cw, f.ch);
    let cell = |row: usize, col: usize, w: usize, c: (u8, u8, u8, u8)| {
        Quad::new(
            (ox + col as f32 * cw, oy + row as f32 * ch),
            (ox + (col + w) as f32 * cw, oy + (row + 1) as f32 * ch),
            c,
        )
    };
    let mut quads = Vec::new();
    let card = Rect {
        x: r.x + CARD_MARGIN,
        y: r.y + CARD_MARGIN,
        w: (r.w - CARD_MARGIN * 2.0).max(1.0),
        h: (r.h - CARD_MARGIN * 2.0).max(1.0),
    };
    let bg = f.panel_bg;
    quads.push(Quad::rounded(
        (card.x * f.scale, card.y * f.scale),
        ((card.x + card.w) * f.scale, (card.y + card.h) * f.scale),
        (bg.0, bg.1, bg.2, 255),
        CARD_RADIUS * f.scale,
    ));
    if let (true, Some(row)) = (f.focused, d.current_line) {
        let h = chrome.hover;
        quads.push(cell(
            row,
            d.gutter,
            cols.saturating_sub(d.gutter),
            (h.0, h.1, h.2, 90),
        ));
    }
    // A jump-to-line highlight, over the current-line band.
    if let Some(row) = d.flash_line {
        let w = chrome.warning;
        quads.push(cell(
            row,
            d.gutter,
            cols.saturating_sub(d.gutter),
            (w.0, w.1, w.2, 120),
        ));
    }
    // A pending agent proposal: its changed lines tinted green.
    let pos = chrome.positive;
    for c in &d.proposal {
        quads.push(cell(c.row, c.col, c.width, (pos.0, pos.1, pos.2, 90)));
    }
    for c in &d.deletions {
        let neg = chrome.negative;
        quads.push(cell(c.row, c.col, c.width, (neg.0, neg.1, neg.2, 90)));
        let y = oy + (c.row as f32 + 0.5) * ch;
        quads.push(Quad::new(
            (ox + c.col as f32 * cw, y),
            (ox + (c.col + c.width) as f32 * cw, y + f.scale.max(1.0)),
            (neg.0, neg.1, neg.2, 200),
        ));
    }
    // Find matches under the selection, in the terminal's match colours.
    for (c, current) in ed.match_cells(f.matches, f.current_match) {
        let color = if current {
            (0x2e, 0x5b, 0x8f, 255)
        } else {
            (0x33, 0x3d, 0x4d, 255)
        };
        quads.push(cell(c.row, c.col, c.width, color));
    }
    // Diagnostics: a line under the cells, in the severity's colour.
    for (c, severity) in &d.underlines {
        let (r, g, b) = severity_rgb(*severity);
        let x0 = ox + c.col as f32 * cw;
        let y1 = oy + (c.row + 1) as f32 * ch;
        let thick = (1.5 * f.scale).max(1.0);
        quads.push(Quad::new(
            (x0, y1 - thick),
            (x0 + c.width as f32 * cw, y1),
            (r, g, b, if *severity <= 2 { 255 } else { 170 }),
        ));
    }
    let sel = f.theme.selection;
    for s in &d.selection {
        quads.push(cell(s.row, s.col, s.width, (sel.0, sel.1, sel.2, 255)));
    }
    let fg = f.theme.fg;
    for (row, col) in &d.carets {
        let x = ox + *col as f32 * cw;
        let y = oy + *row as f32 * ch;
        quads.push(Quad::new(
            (x, y),
            (x + 2.0 * f.scale, y + ch),
            (fg.0, fg.1, fg.2, 255),
        ));
    }
    PaneDraw {
        id: id.to_string(),
        rect: inner,
        quads,
        rows: d.rows,
    }
}

/// A pane's card: its rect less the margin between cards.
fn card_rect(r: Rect) -> Rect {
    Rect {
        x: r.x + CARD_MARGIN,
        y: r.y + CARD_MARGIN,
        w: (r.w - CARD_MARGIN * 2.0).max(1.0),
        h: (r.h - CARD_MARGIN * 2.0).max(1.0),
    }
}

fn card_inner(r: Rect) -> Rect {
    let card = card_rect(r);
    Rect {
        x: card.x + CARD_PAD,
        y: card.y + CARD_PAD,
        w: (card.w - CARD_PAD * 2.0).max(1.0),
        h: (card.h - CARD_PAD * 2.0).max(1.0),
    }
}

/// Whole lines to scroll for a wheel or trackpad movement of `moved` lines,
/// keeping the fraction for the next event: a trackpad sends many small
/// pixel deltas, each well under a line, which truncation used to drop.
fn wheel_lines(accum: &mut f64, moved: f64) -> i32 {
    if moved.signum() != accum.signum() && *accum != 0.0 {
        // A change of direction starts afresh.
        *accum = 0.0;
    }
    *accum += moved;
    let lines = accum.trunc();
    *accum -= lines;
    lines as i32
}

/// Arrow keys for `lines` of wheel movement (positive is up) sent to a
/// full-screen program on the alternate screen, in its cursor-key mode.
fn alternate_scroll_keys(lines: i32, app_cursor: bool) -> Vec<u8> {
    let key: &[u8] = match (lines > 0, app_cursor) {
        (true, true) => b"\x1bOA",
        (true, false) => b"\x1b[A",
        (false, true) => b"\x1bOB",
        (false, false) => b"\x1b[B",
    };
    key.repeat(lines.unsigned_abs().min(64) as usize)
}

/// Encode a mouse report for the terminal application.
///
/// `button`: 0 left, 1 middle, 2 right, 3 none (hover), 64 wheel-up, 65 wheel-down.
/// With SGR encoding (`?1006h`) a press ends in `M` and a release in `m`; wheel
/// and motion reports have no release, so they always use `M`. Otherwise the
/// legacy X10 encoding applies (coordinates are limited to 223).
fn mouse_report(
    sgr: bool,
    button: u8,
    pressed: bool,
    motion: bool,
    col: u16,
    row: u16,
) -> Option<String> {
    if col == 0 || row == 0 {
        return None;
    }
    let code = if motion && button < 64 {
        button + 32
    } else {
        button
    };
    if sgr {
        let end = if pressed || motion || button >= 64 {
            'M'
        } else {
            'm'
        };
        return Some(format!("\x1b[<{code};{col};{row}{end}"));
    }
    if col > 223 || row > 223 {
        return None;
    }
    let bytes = [
        0x1b,
        b'[',
        b'M',
        32u8.saturating_add(code),
        32u8.saturating_add(col as u8),
        32u8.saturating_add(row as u8),
    ];
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// A diagram slot: `None` while the external renderer runs, then its result.
type MmdSlot = Option<Option<std::path::PathBuf>>;

/// External Mermaid rendering (opt-in via `mermaid-command`) with a cache. The
/// renderer runs on a background thread; until it finishes the built-in subset
/// (or a placeholder) is shown, and `wake` repaints once the image is ready.
#[derive(Default)]
struct Mmd {
    dir: std::path::PathBuf,
    cmd: Option<String>,
    cache: Arc<std::sync::Mutex<std::collections::HashMap<u64, MmdSlot>>>,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Mmd {
    fn image(&mut self, source: &str) -> Option<std::path::PathBuf> {
        let cmd = self.cmd.clone()?;
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        source.hash(&mut h);
        let key = h.finish();
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = cache.get(&key) {
            return slot.clone().flatten();
        }
        cache.insert(key, None);
        drop(cache);
        let (shared, dir, wake) = (self.cache.clone(), self.dir.clone(), self.wake.clone());
        let source = source.to_string();
        std::thread::spawn(move || {
            let img = mtty_ui::mermaid::render_external(&source, &cmd, &dir);
            shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, Some(img));
            if let Some(wake) = wake {
                wake();
            }
        });
        None
    }
}

/// Render a Markdown run. `base` is the document's directory, which relative
/// image paths resolve against (a remote document has none).
fn open_commonmark(
    ui: &mut egui::Ui,
    cache: &mut egui_commonmark::CommonMarkCache,
    text: &str,
    base: Option<&std::path::Path>,
) {
    if !text.trim().is_empty() {
        let mut viewer = egui_commonmark::CommonMarkViewer::new();
        if let Some(dir) = base {
            viewer = viewer.default_implicit_uri_scheme(mtty_ui::markdown::dir_uri_scheme(dir));
        }
        viewer.show(ui, cache, text);
    }
}

/// Render Markdown, drawing ```mermaid blocks (built-in subset or `mmdc`).
fn render_markdown(
    ui: &mut egui::Ui,
    text: &str,
    base: Option<&std::path::Path>,
    cache: &mut egui_commonmark::CommonMarkCache,
    mmd: &mut Mmd,
    fg: egui::Color32,
    panel: egui::Color32,
) {
    let mut rest = text;
    loop {
        let Some(i) = rest.find("```mermaid") else {
            open_commonmark(ui, cache, rest, base);
            break;
        };
        open_commonmark(ui, cache, &rest[..i], base);
        let after = &rest[i + "```mermaid".len()..];
        let Some(j) = after.find("```") else {
            open_commonmark(ui, cache, after, base);
            break;
        };
        let body = &after[..j];
        if let Some(path) = mmd.image(body) {
            if let Some(uri) = mtty_ui::markdown::image_uri(&path.display().to_string(), None) {
                ui.add(
                    egui::Image::new(uri)
                        .max_width(ui.available_width())
                        .max_height(400.0),
                );
            }
        } else if let Some(d) = mtty_ui::mermaid::parse_diagram(body) {
            mtty_ui::mermaid::show_diagram(ui, &d, fg, panel);
        } else {
            ui.label(
                egui::RichText::new(mtty_ui::i18n::t(
                    mtty_ui::i18n::Lang::En,
                    "Mermaid diagram (not rendered)",
                    "Mermaid 图（未渲染）",
                ))
                .size(12.0)
                .color(fg.gamma_multiply(0.6)),
            );
        }
        rest = &after[j + 3..];
        if rest.trim().is_empty() {
            break;
        }
    }
}

/// A gesture retains ownership until release, even after leaving its pane.
fn pointer_to_terminal(ui_consumed: bool, over_terminal: bool, terminal_gesture: bool) -> bool {
    terminal_gesture || (over_terminal && !ui_consumed)
}

fn git_rows(cwd: &std::path::Path) -> Vec<(String, String)> {
    let out = mtty_platform::background_command("git")
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain=v1", "-b"])
        .output();
    let Ok(out) = out else {
        return vec![("git".into(), "unavailable".into())];
    };
    parse_git_status(out.status.success(), &String::from_utf8_lossy(&out.stdout))
}

/// Rows for `git status --porcelain=v1 -b`. A failed status (outside a
/// repository) is reported as such, never as "clean".
fn parse_git_status(success: bool, text: &str) -> Vec<(String, String)> {
    if !success {
        return vec![("git".into(), "not a git repository".into())];
    }
    let mut rows = Vec::new();
    for line in text.lines().take(200) {
        if let Some(branch) = line.strip_prefix("## ") {
            rows.push(("branch".into(), branch.to_string()));
        } else if line.len() > 3 {
            rows.push((line[..2].to_string(), line[3..].to_string()));
        }
    }
    if rows.is_empty() {
        rows.push(("status".into(), "clean".into()));
    }
    rows
}

/// The pane's shell and every process below it: dev servers usually run as
/// children (`npm run dev`, `cargo run`), not as the shell itself.
fn process_tree(root: u32, ps_output: &str) -> Vec<u32> {
    let pairs: Vec<(u32, u32)> = ps_output
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            Some((cols.next()?.parse().ok()?, cols.next()?.parse().ok()?))
        })
        .collect();
    let mut tree = vec![root];
    let mut i = 0;
    while i < tree.len() && tree.len() < 512 {
        let parent = tree[i];
        for (pid, ppid) in &pairs {
            if *ppid == parent && !tree.contains(pid) {
                tree.push(*pid);
            }
        }
        i += 1;
    }
    tree
}

fn ports_rows(pid: u32) -> Vec<(String, String)> {
    let ps = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid="])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let pids: Vec<String> = process_tree(pid, &ps).iter().map(u32::to_string).collect();
    let out = std::process::Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-a", "-p", &pids.join(",")])
        .output();
    let Ok(out) = out else {
        return vec![("ports".into(), "lsof unavailable".into())];
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut rows = Vec::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if let Some(name) = cols.get(8) {
            rows.push((
                cols.first().copied().unwrap_or("").to_string(),
                name.to_string(),
            ));
        }
    }
    if rows.is_empty() {
        rows.push(("ports".into(), "no listeners".into()));
    }
    rows
}

fn files_rows(cwd: &std::path::Path) -> Vec<FileEntry> {
    list_dir(cwd, false)
}

/// Directory entries, directories first; dot files only with `hidden`.
fn list_dir(cwd: &std::path::Path, hidden: bool) -> Vec<FileEntry> {
    let Ok(read) = std::fs::read_dir(cwd) else {
        return Vec::new();
    };
    let mut rows: Vec<FileEntry> = read
        .flatten()
        .filter(|e| hidden || !e.file_name().to_string_lossy().starts_with('.'))
        .take(500)
        .map(|e| {
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            FileEntry {
                name: e.file_name().to_string_lossy().to_string(),
                is_dir,
                size,
            }
        })
        .collect();
    // Directories first, then name.
    rows.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    rows
}

fn configure_egui(ctx: &egui::Context, ch: &mtty_ui::theme::Chrome) {
    let col = |c: mtty_ui::theme::Rgb| egui::Color32::from_rgb(c.0, c.1, c.2);
    let mut style = (*ctx.style()).clone();
    {
        let v = &mut style.visuals;
        v.dark_mode = true;
        v.window_fill = col(ch.bg);
        v.panel_fill = col(ch.card);
        v.extreme_bg_color = col(ch.bg);
        v.faint_bg_color = col(ch.hover);
        v.override_text_color = Some(col(ch.text));
        v.hyperlink_color = col(ch.accent);
        v.selection.bg_fill = col(ch.active);
        v.widgets.inactive.weak_bg_fill = col(ch.hover);
        v.widgets.hovered.weak_bg_fill = col(ch.active);
        v.widgets.active.weak_bg_fill = col(ch.active);
        v.widgets.inactive.bg_fill = col(ch.hover);
        v.widgets.hovered.bg_fill = col(ch.active);
        v.widgets.active.bg_fill = col(ch.active);
        // Panel separators: egui defaults to a flat grey; use the sidebar edge
        // colour so the split reads like a `[sidebar] border-right`.
        v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, col(ch.border));
    }
    let r = egui::Rounding::same(6.0);
    style.visuals.widgets.inactive.rounding = r;
    style.visuals.widgets.hovered.rounding = r;
    style.visuals.widgets.active.rounding = r;
    style.visuals.widgets.open.rounding = r;
    style.visuals.window_rounding = egui::Rounding::same(10.0);
    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    style.spacing.button_padding = egui::vec2(6.0, 2.0);
    // A floating window's corner overlaps its two edges. egui 0.30 gives a tie
    // to a target at most half as thick as the other, so with the default
    // 10/20 pt grab areas the bottom or right edge always won and the corner
    // never resized both ways; 16 pt corners over 10 pt edges win again.
    style.interaction.resize_grab_radius_corner = 8.0;
    ctx.set_style(style);
}

/// System fonts with CJK coverage for the egui chrome, first match wins.
fn cjk_font_candidates() -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = [
        // macOS
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        // Linux: Debian/Ubuntu, Arch, Fedora, then WenQuanYi
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
    ]
    .iter()
    .map(std::path::PathBuf::from)
    .collect();
    // Windows: Microsoft YaHei, SimSun, then Traditional Chinese, Japanese
    // and Korean system fonts.
    let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
    let fonts = std::path::PathBuf::from(windir).join("Fonts");
    for name in [
        "msyh.ttc",
        "msyh.ttf",
        "simsun.ttc",
        "msjh.ttc",
        "YuGothR.ttc",
        "meiryo.ttc",
        "malgun.ttf",
    ] {
        out.push(fonts.join(name));
    }
    out
}

/// The egui fonts: the bundled symbol fonts plus the first CJK font found.
/// The `cjk` family member is added only with font data behind it: egui
/// panics on a family that names missing data, which crashed mtty at start
/// on every machine without one of the listed fonts (all of Windows).
fn egui_font_definitions(cjk: &[std::path::PathBuf]) -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "nerd".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../../../assets/fonts/SymbolsNerdFontMono-Regular.ttf"
        ))),
    );
    let have_cjk = cjk.iter().any(|path| match std::fs::read(path) {
        Ok(bytes) => {
            fonts.font_data.insert(
                "cjk".to_owned(),
                Arc::new(egui::FontData::from_owned(bytes)),
            );
            true
        }
        Err(_) => false,
    });
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = fonts.families.entry(family).or_default();
        if have_cjk {
            list.push("cjk".to_owned());
        }
        list.push("nerd".to_owned());
    }
    // Tabler Icons (MIT), subset to the glyphs we use, as the UI icon family.
    fonts.font_data.insert(
        "tabler".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../../../assets/fonts/tabler-icons-subset.ttf"
        ))),
    );
    fonts.families.insert(
        egui::FontFamily::Name("tabler".into()),
        vec!["tabler".to_owned()],
    );
    fonts
}

fn install_egui_fonts(ctx: &egui::Context) {
    ctx.set_fonts(egui_font_definitions(&cjk_font_candidates()));
}

/// The file name for a recipe called `name`, or why the name cannot be used
/// (English, Chinese). A name is a single file in the recipes directory: no
/// separators, no `..`, no hidden or control characters.
fn recipe_file_name(name: &str) -> Result<String, (&'static str, &'static str)> {
    let name = name.trim();
    if name.is_empty() {
        return Err(("Enter a recipe name.", "请输入配方名称。"));
    }
    let bad = name.starts_with('.')
        || name.contains(['/', '\\', ':'])
        || name.chars().any(char::is_control)
        || name.chars().count() > 80;
    if bad {
        return Err((
            "A recipe name cannot contain / \\ : or start with a dot.",
            "配方名称不能包含 / \\ : 或以点开头。",
        ));
    }
    Ok(format!("{name}.json"))
}

fn recipes_dir() -> Option<std::path::PathBuf> {
    window_file().map(|p| p.with_file_name("recipes"))
}

fn list_recipes() -> Vec<String> {
    let Some(dir) = recipes_dir() else {
        return Vec::new();
    };
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = read
        .flatten()
        .filter_map(|e| {
            e.path()
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
        })
        .collect();
    out.sort();
    out
}

fn session_file() -> Option<std::path::PathBuf> {
    window_file().map(|p| p.with_file_name("session.json"))
}

/// Terminal contents saved at quit, one file per pane (see
/// `save_session_on_exit`).
fn scrollback_dir() -> Option<std::path::PathBuf> {
    mtty_config::data_dir().map(|d| d.join("scrollback"))
}

/// Rows of each terminal kept for the next launch.
const SCROLLBACK_LINES: usize = 5000;

/// Output a PTY host keeps for reattaching (ADR 0041).
const PTY_HOST_RING: usize = 8 << 20;

/// How often terminal contents are saved while mtty runs.
const SCROLLBACK_SAVE_EVERY: Duration = Duration::from_secs(60);

/// Saved contents without the notes mtty wrote when restoring them before,
/// so restarts do not stack "Restored from the last session" lines.
fn without_mtty_notes(text: &str) -> String {
    let plain = |line: &str| {
        let mut out = String::new();
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    };
    text.split_inclusive("\r\n")
        .filter(|line| !plain(line).starts_with("[mtty] "))
        .collect()
}

/// A bare file name (no directories), as the session names saved contents.
fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty() && std::path::Path::new(name).file_name() == Some(std::ffi::OsStr::new(name))
}

/// Read the inline images saved next to a pane's scrollback, if any. The file
/// is removed once read, like the scrollback itself: the next quit saves it
/// afresh.
fn load_pending_images(saved: &serde_json::Value) -> Option<PendingImages> {
    let file = saved["images"]
        .as_str()
        .map(str::to_string)
        .or_else(|| saved["id"].as_str().map(|id| format!("{id}.images.json")))
        .filter(|f| is_plain_file_name(f))?;
    let path = scrollback_dir()?.join(file);
    let bytes = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let saved = serde_json::from_slice::<mtty_core::graphics::SavedImages>(&bytes).ok()?;
    if saved.images.is_empty() {
        return None;
    }
    Some(PendingImages { saved })
}

/// Write `bytes` to `dir/name` readable by the owner only: terminal output
/// can hold secrets.
fn write_private(dir: &std::path::Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        builder.mode(0o700);
        options.mode(0o600);
    }
    builder.create(dir)?;
    options.open(dir.join(name))?.write_all(bytes)
}

fn queue_file() -> Option<std::path::PathBuf> {
    window_file().map(|p| p.with_file_name("queue.json"))
}

/// Load the persisted prompt queue (agent Composer drafts).
fn load_queue() -> mtty_ui::agentloop::PromptQueue {
    let Some(path) = queue_file() else {
        return Default::default();
    };
    let read_path = legacy_state_path(&path, "native-queue.json");
    std::fs::read(&read_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .map(|v| mtty_ui::agentloop::PromptQueue::from_json(&v))
        .unwrap_or_default()
}

fn window_file() -> Option<std::path::PathBuf> {
    Some(mtty_config::config_dir()?.join("window"))
}

fn legacy_state_path(path: &std::path::Path, legacy: &str) -> std::path::PathBuf {
    if path.exists() {
        path.to_path_buf()
    } else {
        path.with_file_name(legacy)
    }
}

/// The window's saved chrome layout: its size, the side panels' widths, which
/// panels are open, and the selected details tab. Written on quit and whenever
/// a panel is toggled, so a restart comes back the way it was left.
#[derive(Debug, Clone, Copy, PartialEq)]
struct WindowState {
    size: (f32, f32),
    sidebar_w: f32,
    details_w: f32,
    sidebar_open: bool,
    details_open: bool,
    details_tab: usize,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            size: (1100.0, 720.0),
            sidebar_w: SIDEBAR_W,
            details_w: DETAILS_W,
            sidebar_open: true,
            details_open: true,
            details_tab: 0,
        }
    }
}

impl WindowState {
    fn load() -> Self {
        let Some(path) = window_file() else {
            return Self::default();
        };
        let text =
            std::fs::read_to_string(legacy_state_path(&path, "native-window")).unwrap_or_default();
        Self::parse(&text)
    }

    /// `width height [sidebar details [sidebar_open details_open details_tab]]`.
    /// Files from older versions stop after the size or the widths; the missing
    /// fields fall back to the defaults (both panels open, the first tab).
    fn parse(text: &str) -> Self {
        let default = Self::default();
        let mut it = text.split_whitespace();
        let number = |raw: Option<&str>| raw.and_then(|v| v.parse::<f32>().ok());
        let size = match (number(it.next()), number(it.next())) {
            (Some(w), Some(h)) => (w, h),
            _ => default.size,
        };
        let clamp = |v: Option<f32>, range: std::ops::RangeInclusive<f32>, fallback: f32| {
            v.filter(|v| v.is_finite())
                .map(|v| v.clamp(*range.start(), *range.end()))
                .unwrap_or(fallback)
        };
        let flag = |raw: Option<&str>, fallback: bool| match raw {
            Some("0") => false,
            Some("1") => true,
            _ => fallback,
        };
        Self {
            size,
            sidebar_w: clamp(number(it.next()), chrome::SIDEBAR_RANGE, default.sidebar_w),
            details_w: clamp(number(it.next()), chrome::DETAILS_RANGE, default.details_w),
            sidebar_open: flag(it.next(), default.sidebar_open),
            details_open: flag(it.next(), default.details_open),
            details_tab: it
                .next()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(default.details_tab)
                .min(6),
        }
    }

    fn format(&self) -> String {
        format!(
            "{:.0} {:.0} {:.0} {:.0} {} {} {}\n",
            self.size.0,
            self.size.1,
            self.sidebar_w,
            self.details_w,
            self.sidebar_open as u8,
            self.details_open as u8,
            self.details_tab,
        )
    }

    fn save(&self) {
        let Some(path) = window_file() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, self.format());
    }
}

impl chrome::Chrome for State {
    fn draws_menu_bar(&self) -> bool {
        // macOS inside an app bundle uses the system menu bar instead (ADR 0031).
        !menu_in_os() && !cfg!(windows)
    }

    fn window_controls(&self) -> bool {
        cfg!(windows)
    }
    fn window_maximized(&self) -> bool {
        self.window.is_maximized()
    }
    fn on_minimize_window(&mut self) {
        self.window.set_minimized(true);
    }
    fn on_maximize_window(&mut self) {
        self.window.set_maximized(!self.window.is_maximized());
    }

    fn titlebar_inset(&self) -> f32 {
        if cfg!(target_os = "macos") && unified_titlebar() {
            TRAFFIC_LIGHTS_W
        } else {
            0.0
        }
    }

    fn on_title_drag_hover(&mut self, hovered: bool) {
        self.title_drag_hover = hovered;
    }

    fn pane_close_rects(&self) -> Vec<(String, egui::Rect)> {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return Vec::new();
        };
        // A split tab gives every pane its own close button. A lone pane has
        // none — its tab carries the affordance — except an editor, whose tab
        // shows no close button while the session list is up.
        let split = tab.panes.len() + tab.editors.len() > 1;
        let editors: std::collections::HashSet<&str> =
            tab.editors.iter().map(|e| e.id.as_str()).collect();
        tab.layout
            .rects(self.grid_area())
            .into_iter()
            .filter(|(id, _)| split || editors.contains(id.as_str()))
            .map(|(id, r)| {
                (
                    id,
                    egui::Rect::from_min_size(egui::pos2(r.x, r.y), egui::vec2(r.w, r.h)),
                )
            })
            .collect()
    }

    fn on_close_pane(&mut self, id: &str) {
        // Closing a modified editor from its pane button arms first and
        // discards on the second click, like the keyboard close. MTP
        // `pane.close` keeps refusing (see `close_pane_id`).
        let mut notice = None;
        for tab in &mut self.tabs {
            if let Some(e) = tab.editors.iter_mut().find(|e| e.id == id) {
                if e.doc.is_modified() && !e.close_armed {
                    e.close_armed = true;
                    notice = Some(format!(
                        "{}: {}",
                        e.title(),
                        mtty_ui::i18n::t(
                            self.lang,
                            "unsaved changes. Save, or close again to discard them.",
                            "有未保存的修改。请保存,或再次关闭以放弃修改。"
                        )
                    ));
                }
                break;
            }
        }
        if let Some(msg) = notice {
            self.show_notice(msg);
        } else {
            self.close_pane_id(id);
        }
        self.window.request_redraw();
    }

    fn lang(&self) -> mtty_ui::i18n::Lang {
        self.lang
    }
    fn tabs(&self) -> Vec<chrome::ChromeTab> {
        self.tabs
            .iter()
            .map(|t| {
                let agent = self.agent_tab_icon(t);
                let builtin = match agent {
                    Some((icon, _)) => icon,
                    None if t.ssh => mtty_ui::icons::Icon::Server,
                    None => mtty_ui::icons::Icon::Terminal,
                };
                let view = self.view_for(t);
                let mut icon = mtty_ui::icons::TabIcon::from(builtin);
                icon.color = agent.and_then(|(_, color)| color);
                icon.hint = self.mtp.agent_for(&t.active).and_then(|agent| {
                    agent
                        .get("state")
                        .and_then(|state| state.as_str())
                        .map(|state| {
                            format!(
                                "{}: {}",
                                mtty_ui::i18n::t(self.lang, "Agent state", "助手状态"),
                                agent_state_label(self.lang, state)
                            )
                        })
                });
                if let Some(rule) = view.as_ref().and_then(|v| v.icon.as_ref()) {
                    let color = rule.rgb();
                    icon.glyph = mtty_ui::icons::rule_glyph(
                        rule.name.as_deref(),
                        rule.emoji.as_deref(),
                        color.is_some(),
                    );
                    icon.color = color
                        .map(|c| mtty_ui::theme::Rgb(c.0, c.1, c.2))
                        .or(icon.color);
                }
                let rule_badge = view.as_ref().and_then(|v| v.badge.clone());
                let mut title = self.title_with(t, view);
                if let Some(p) = &t.prefix {
                    title = format!("[{p}] {title}");
                }
                if let Some(m) = &t.mark {
                    title = format!("{title}{m}");
                }
                if let Some(b) = rule_badge.filter(|b| !b.trim().is_empty()) {
                    title = format!("{title} \u{00b7} {b}");
                }
                // An agent's tab shows that it finished by its icon.
                if let Some(a) = t
                    .attention
                    .filter(|a| agent.is_none() || *a != Attention::Done)
                {
                    title.push_str(a.marker());
                }
                chrome::ChromeTab {
                    title,
                    badge: None,
                    icon,
                    location: tab_location(t),
                }
            })
            .collect()
    }
    fn active_tab(&self) -> usize {
        self.active_tab
    }
    fn show_sidebar(&self) -> bool {
        self.show_sidebar
    }
    fn show_details(&self) -> bool {
        self.show_details
    }
    fn details_tab(&self) -> usize {
        self.details_tab.min(6)
    }
    fn details_title(&self) -> String {
        let title = self.details_content(self.details_tab.min(6)).0;
        localize_detail(self.lang, title).to_string()
    }
    fn details_rows(&self) -> Vec<(String, String)> {
        self.details_content(self.details_tab.min(6))
            .1
            .into_iter()
            .map(|(k, v)| {
                (
                    localize_detail(self.lang, &k).to_string(),
                    localize_detail(self.lang, &v).to_string(),
                )
            })
            .collect()
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn status_right(&self) -> String {
        if let Some(ed) = self.active_editor() {
            let (line, col) = ed.caret_line_col();
            if let Some(large) = &ed.large {
                use mtty_ui::i18n::t;
                let l = self.lang;
                let mut s = format!(
                    "{} \u{00b7} {} \u{00b7} {line}:{col}",
                    t(l, "View only", "只读查看"),
                    human_bytes(large.file.len_bytes())
                );
                if !large.file.indexed() {
                    s.push_str(&format!(
                        " \u{00b7} {} {:.0}%",
                        t(l, "indexing", "建立索引"),
                        large.file.progress() * 100.0
                    ));
                }
                return s;
            }
            let lang = ed.language().unwrap_or("Plain Text");
            let mode = match ed.vim_mode() {
                Some(mtty_editor::vim::Mode::Normal) => "-- NORMAL -- ",
                Some(mtty_editor::vim::Mode::Insert) => "-- INSERT -- ",
                Some(mtty_editor::vim::Mode::Visual) => "-- VISUAL -- ",
                Some(mtty_editor::vim::Mode::VisualLine) => "-- V-LINE -- ",
                None => "",
            };
            let mut s = format!(
                "{mode}{lang} \u{00b7} {line}:{col} \u{00b7} {}",
                ed.line_ending_name()
            );
            // Problems from the language server: ✖ errors, ⚠ warnings.
            let (errors, warnings) = ed.problem_counts();
            if errors + warnings > 0 {
                s.push_str(&format!(" \u{00b7} \u{2716} {errors} \u{26a0} {warnings}"));
            }
            if let Some(p) = ed.proposal() {
                let label = p
                    .label
                    .clone()
                    .unwrap_or_else(|| mtty_ui::i18n::t(self.lang, "agent", "agent").to_string());
                s.push_str(&format!(
                    " \u{00b7} \u{270e} {label}: {} {}",
                    p.lines.len(),
                    mtty_ui::i18n::t(self.lang, "lines (Accept/Reject)", "行(接受/拒绝)")
                ));
            }
            return s;
        }
        if let Some(p) = self.active_pane() {
            if let Some(a) = self
                .mtp
                .agent_for(&p.id)
                .and_then(|v| v.get("agent").and_then(|x| x.as_str()).map(str::to_string))
            {
                return a;
            }
        }
        std::env::var("SHELL")
            .ok()
            .and_then(|s| s.rsplit('/').next().map(str::to_string))
            .unwrap_or_default()
    }
    fn theme(&self) -> Theme {
        self.theme.clone()
    }
    fn details_is_queue(&self) -> bool {
        self.details_tab == 6
    }
    fn details_list(&self) -> Option<Vec<chrome::ChromeItem>> {
        use chrome::ChromeItem;
        use mtty_ui::icons::Icon;
        let item = |icon: Icon, label: String, meta: String| ChromeItem {
            icon: icon.into(),
            label,
            meta,
            label_color: None,
        };
        match self.details_tab.min(6) {
            2 => Some(
                self.outline_rows()
                    .into_iter()
                    .map(|(cwd, cmd)| item(Icon::Terminal, cmd, cwd))
                    .collect(),
            ),
            3 => Some(
                self.details_data
                    .as_ref()
                    .map(|d| {
                        d.git
                            .iter()
                            .map(|(k, v)| item(Icon::Git, v.clone(), k.clone()))
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            4 => Some(
                self.details_data
                    .as_ref()
                    .map(|d| {
                        let ch = self.theme.chrome();
                        d.files
                            .iter()
                            .map(|f| {
                                let icon = file_tree_icon(&ch, &f.name, f.is_dir);
                                let meta = if f.is_dir {
                                    String::new()
                                } else {
                                    human_size(f.size)
                                };
                                ChromeItem {
                                    icon,
                                    label: f.name.clone(),
                                    meta,
                                    label_color: f.is_dir.then_some(ch.folder),
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            5 => Some(
                self.details_data
                    .as_ref()
                    .map(|d| {
                        d.ports
                            .iter()
                            .map(|(k, v)| item(Icon::Ports, v.clone(), k.clone()))
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            _ => None,
        }
    }
    fn details_body(&mut self, ui: &mut egui::Ui, lang: mtty_ui::i18n::Lang) -> bool {
        if self.details_tab.min(6) == 4 {
            self.files_body(ui, lang);
            true
        } else {
            false
        }
    }
    fn status(&self) -> String {
        self.status_text()
    }
    fn queue(&self) -> Vec<String> {
        // Show where each prompt goes; untargeted ones are sent by hand.
        self.prompt_queue
            .items
            .iter()
            .map(|item| {
                let target = item.pane.as_ref().and_then(|pane| {
                    self.tabs
                        .iter()
                        .find(|t| t.panes.iter().any(|p| &p.id == pane))
                        .map(|t| self.title_of(t))
                });
                match target {
                    Some(tab) => format!("{} \u{2192} {tab}", item.text),
                    None => item.text.clone(),
                }
            })
            .collect()
    }
    fn take_queue_input(&mut self) -> String {
        std::mem::take(&mut self.prompt_input)
    }
    fn set_queue_input(&mut self, input: String) {
        self.prompt_input = input;
    }
    fn on_new_tab(&mut self) {
        self.new_tab_in(self.active_cwd_for_new());
    }
    fn sidebar_width(&self) -> f32 {
        self.sidebar_w
    }
    fn details_width(&self) -> f32 {
        self.details_w
    }
    fn on_panel_widths(&mut self, sidebar: Option<f32>, details: Option<f32>) {
        let mut changed = false;
        if let Some(w) = sidebar.filter(|w| (w - self.sidebar_w).abs() > 0.5) {
            self.sidebar_w = w;
            changed = true;
        }
        if let Some(w) = details.filter(|w| (w - self.details_w).abs() > 0.5) {
            self.details_w = w;
            changed = true;
        }
        if changed {
            // The terminal area changed: reflow the panes to it.
            self.resize();
            self.window.request_redraw();
        }
    }
    fn hosts(&self) -> Vec<(String, Option<String>)> {
        self.host_book
            .sorted()
            .into_iter()
            .map(|(_, h)| (h.name.clone(), h.group.clone()))
            .collect()
    }
    fn on_host_connect(&mut self, i: usize) {
        let host = self.host_book.sorted().get(i).map(|(_, h)| (*h).clone());
        if let Some(host) = host {
            self.open_host(&host);
        }
    }
    fn on_switch_tab(&mut self, i: usize) {
        if i < self.tabs.len() {
            self.active_tab = i;
            self.selection = None;
        }
    }
    fn on_close_tab(&mut self, i: usize) {
        if !self.confirm_close_tabs(&[i]) {
            return;
        }
        let cwd = self.tabs.get(i).and_then(|tab| {
            tab.panes
                .iter()
                .find(|p| p.id == tab.active)
                .and_then(|p| p.term.cwd().map(std::path::PathBuf::from))
        });
        if remove_whole_tab(&mut self.tabs, &mut self.active_tab, i) {
            self.closed.push(cwd);
            self.selection = None;
            self.publish_panes();
        }
    }
    fn on_rename_tab(&mut self, i: usize) {
        let title = self
            .tabs
            .get(i)
            .map(|t| self.title_of(t))
            .unwrap_or_default();
        self.renaming = Some(i);
        self.rename_buf = title;
    }
    fn on_duplicate_tab(&mut self, i: usize) {
        if i < self.tabs.len() {
            self.active_tab = i;
            self.duplicate_tab();
        }
    }
    fn on_close_others(&mut self, i: usize) {
        let others: Vec<usize> = (0..self.tabs.len()).filter(|&j| j != i).collect();
        if !self.refuse_unsaved(&others) {
            return;
        }
        if i < self.tabs.len() {
            let removed = take_other_tabs(&mut self.tabs, i);
            self.remember_closed(&removed);
            self.active_tab = 0;
            self.selection = None;
            self.publish_panes();
        }
    }
    fn on_close_below(&mut self, i: usize) {
        let below: Vec<usize> = (i + 1..self.tabs.len()).collect();
        if !self.refuse_unsaved(&below) {
            return;
        }
        if i < self.tabs.len() {
            let removed = take_tabs_below(&mut self.tabs, i);
            self.remember_closed(&removed);
            self.active_tab = self.active_tab.min(self.tabs.len() - 1);
            self.selection = None;
            self.publish_panes();
        }
    }
    fn on_move_tab(&mut self, i: usize, delta: i32) {
        let j = i as i32 + delta;
        if i < self.tabs.len() && j >= 0 && (j as usize) < self.tabs.len() {
            self.tabs.swap(i, j as usize);
            if self.active_tab == i {
                self.active_tab = j as usize;
            } else if self.active_tab == j as usize {
                self.active_tab = i;
            }
            self.publish_panes();
        }
    }
    fn tab_groups(&self) -> Vec<Option<String>> {
        self.tabs.iter().map(|t| t.group.clone()).collect()
    }

    fn on_mark_tab(&mut self, i: usize) {
        if let Some(t) = self.tabs.get(i) {
            self.mark_buf = t.mark.clone().unwrap_or_default();
        }
        self.mark_renaming = Some(i);
    }

    fn on_group_tab(&mut self, i: usize) {
        if let Some(t) = self.tabs.get(i) {
            self.group_buf = t.group.clone().unwrap_or_default();
        }
        self.group_renaming = Some(i);
    }

    fn on_ungroup_tab(&mut self, i: usize) {
        if let Some(t) = self.tabs.get_mut(i) {
            t.group = None;
        }
        self.publish_panes();
    }

    fn on_set_prefix(&mut self, i: usize) {
        self.prefix_buf = self
            .tabs
            .get(i)
            .and_then(|t| t.prefix.clone())
            .unwrap_or_default();
        self.prefix_renaming = Some(i);
    }
    fn on_reorder_tab(&mut self, from: usize, to: usize) {
        if from < self.tabs.len() && to < self.tabs.len() && from != to {
            let was_active = self.active_tab == from;
            let tab = self.tabs.remove(from);
            self.tabs.insert(to, tab);
            if was_active {
                self.active_tab = to;
            }
            self.publish_panes();
        }
    }
    fn on_font_delta(&mut self, d: f32) {
        self.font_size = (self.font_size + d).clamp(6.0, 40.0);
        let (cw, ch) =
            State::cell_size(self.font_size, self.line_ratio, self.font_family.as_deref());
        self.cw = cw;
        self.ch = ch;
        self.resize();
    }
    fn on_toggle_sidebar(&mut self) {
        self.toggle_sidebar();
    }
    fn on_toggle_details(&mut self) {
        self.toggle_details();
    }
    fn on_details_tab(&mut self, i: usize) {
        self.details_tab = i;
        self.save_window_state();
    }
    fn on_queue_add(&mut self) {
        if !self.prompt_input.is_empty() {
            let p = std::mem::take(&mut self.prompt_input);
            self.prompt_queue.push(p, self.active_pane_id());
            self.save_queue();
        }
    }
    fn on_queue_send(&mut self, i: usize) {
        // Sending takes the prompt out of the queue, so it is not delivered
        // again when the agent next turns idle.
        if i < self.prompt_queue.items.len() {
            let item = self.prompt_queue.items.remove(i);
            self.deliver_prompt(item);
            self.save_queue();
        }
    }
    fn on_queue_remove(&mut self, i: usize) {
        if i < self.prompt_queue.items.len() {
            self.prompt_queue.items.remove(i);
            self.save_queue();
        }
    }
    fn on_queue_send_all(&mut self) {
        for item in std::mem::take(&mut self.prompt_queue.items) {
            self.deliver_prompt(item);
        }
        self.save_queue();
    }
    fn on_queue_clear(&mut self) {
        self.prompt_queue.items.clear();
        self.save_queue();
    }
    fn on_menu(&mut self, id: chrome::MenuId) {
        use chrome::MenuId::*;
        let cmd = match id {
            NewTab => Cmd::NewTab,
            QuickTerminal => Cmd::QuickTerminal,
            ClosePane => Cmd::ClosePane,
            OpenFile => Cmd::OpenFile,
            Save => Cmd::Save,
            SaveRecipe => Cmd::SaveRecipe,
            OpenRecipe => Cmd::OpenRecipe,
            NewSsh => Cmd::NewSsh,
            NewTransport => Cmd::NewTransport,
            OpenRemote => Cmd::OpenRemote,
            Composer => Cmd::Composer,
            CheckUpdates => Cmd::CheckUpdates,
            Copy => Cmd::Copy,
            Paste => Cmd::Paste,
            SplitRight => Cmd::SplitRight,
            SplitDown => Cmd::SplitDown,
            ToggleSidebar => Cmd::ToggleSidebar,
            ToggleDetails => Cmd::ToggleDetails,
            FontUp => Cmd::FontUp,
            FontDown => Cmd::FontDown,
            FontReset => Cmd::FontReset,
            Palette => Cmd::Palette,
            CopyAnsi => Cmd::CopyAnsi,
            PasteEscaped => Cmd::PasteEscaped,
            Find => Cmd::Find,
            Replace => Cmd::Replace,
            GoToLine => Cmd::GoToLine,
            GoToSymbol => Cmd::GoToSymbol,
            MarkdownPreview => Cmd::MarkdownPreview,
            FindNext => Cmd::FindNext,
            FindPrev => Cmd::FindPrev,
            UseSelForFind => Cmd::UseSelForFind,
            JumpToSel => Cmd::JumpToSel,
            FindInAllTabs => Cmd::FindInAllTabs,
            Fullscreen => Cmd::Fullscreen,
            ReadOnly => Cmd::ReadOnly,
            HintMode => Cmd::HintMode,
            Pip => Cmd::Pip,
            ClearScreen => Cmd::ClearScreen,
            ClearScrollback => Cmd::ClearScrollback,
            DuplicateTab => Cmd::DuplicateTab,
            ReopenClosed => Cmd::ReopenClosed,
            SelectAll => Cmd::SelectAll,
            CopyPath => Cmd::CopyPath,
            RevealCwd => Cmd::RevealCwd,
            OpenExternally => Cmd::OpenExternally,
            Settings => Cmd::Settings,
            Quit => Cmd::Quit,
        };
        self.run_command(cmd);
        self.window.request_redraw();
    }
}

#[cfg(test)]
mod tests {
    /// A pane over a byte pipe the test feeds chunk by chunk.
    fn piped_pane() -> (super::Pane, std::sync::mpsc::Sender<Vec<u8>>) {
        struct ChunkReader(std::sync::mpsc::Receiver<Vec<u8>>);
        impl std::io::Read for ChunkReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let Ok(chunk) = self.0.recv() else {
                    return Ok(0);
                };
                buf[..chunk.len()].copy_from_slice(&chunk);
                Ok(chunk.len())
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let term = mtty_core::Terminal::from_pipe(
            10,
            3,
            100,
            ChunkReader(rx),
            Box::new(std::io::sink()),
            std::sync::Arc::new(|| {}),
        );
        let pane = super::Pane {
            id: "p".into(),
            term,
            scroll: 0,
            on_enter: None,
            published_output: None,
            pending_images: None,
            pending_note: None,
        };
        (pane, tx)
    }

    /// Drain until the reader thread has delivered `bytes`.
    fn feed(pane: &mut super::Pane, tx: &std::sync::mpsc::Sender<Vec<u8>>, bytes: &[u8]) {
        feed_with_selection(pane, tx, bytes, &mut None);
    }

    fn feed_with_selection(
        pane: &mut super::Pane,
        tx: &std::sync::mpsc::Sender<Vec<u8>>,
        bytes: &[u8],
        selection: &mut Option<(String, super::Selection)>,
    ) {
        tx.send(bytes.to_vec()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !pane.drain_output(selection) {
            assert!(std::time::Instant::now() < deadline, "output never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn screen_switch_clears_only_its_panes_selection() {
        let (mut pane, tx) = piped_pane();
        for bytes in [b"\x1b[?1049h".as_slice(), b"\x1b[?1049l".as_slice()] {
            let mut selection = Some((pane.id.clone(), super::Selection::cell((1, 2))));
            feed_with_selection(&mut pane, &tx, bytes, &mut selection);
            assert!(selection.is_none(), "a selection must not cross screens");
        }
        let mut selection = Some(("other".into(), super::Selection::cell((1, 2))));
        feed_with_selection(&mut pane, &tx, b"\x1b[?1049h", &mut selection);
        assert_eq!(selection.as_ref().unwrap().0, "other");
    }

    #[test]
    fn ordinary_output_preserves_terminal_selection() {
        let (mut pane, tx) = piped_pane();
        let mut selection = Some((pane.id.clone(), super::Selection::cell((1, 2))));
        feed_with_selection(&mut pane, &tx, b"output", &mut selection);
        assert!(selection.is_some());
    }

    #[test]
    fn every_drain_keeps_a_scrolled_back_pane_anchored() {
        let (mut pane, tx) = piped_pane();
        feed(&mut pane, &tx, b"a\r\nb\r\nc\r\nd\r\ne");
        pane.scroll = 2;
        // Two drains in a row (the idle drain, then the frame's drain): the
        // offset the first one grew must survive into the second.
        feed(&mut pane, &tx, b"\r\nf");
        feed(&mut pane, &tx, b"\r\ng");
        assert_eq!(pane.scroll, 4);
        assert_eq!(pane.term.screen().line_text(0), "a");
        // A pane at the bottom follows output.
        pane.scroll = 0;
        feed(&mut pane, &tx, b"\r\nh");
        assert_eq!(pane.scroll, 0);
        assert_eq!(pane.term.screen().line_text(2), "h");
    }

    #[test]
    fn windows_frame_resize_handles_edges_and_corners() {
        use winit::window::ResizeDirection::*;
        for (x, y, expected) in [
            (1.0, 1.0, Some(NorthWest)),
            (799.0, 1.0, Some(NorthEast)),
            (1.0, 599.0, Some(SouthWest)),
            (799.0, 599.0, Some(SouthEast)),
            (1.0, 200.0, Some(West)),
            (799.0, 200.0, Some(East)),
            (200.0, 1.0, Some(North)),
            (200.0, 599.0, Some(South)),
            (200.0, 200.0, None),
            (-1.0, 1.0, None),
            (800.0, 200.0, None),
        ] {
            assert_eq!(super::window_resize_edge(x, y, 800.0, 600.0, 5.0), expected);
        }
    }

    #[test]
    fn transport_targets_round_trip_through_json() {
        let serial = super::TransportTarget::Serial {
            device: "/dev/ttyUSB0".into(),
            baud: 115_200,
            data_bits: 8,
            parity: "none".into(),
            stop_bits: 1,
            flow: "none".into(),
        };
        for target in [
            super::TransportTarget::Telnet {
                host: "203.0.113.2".into(),
                port: 23,
            },
            super::TransportTarget::Tcp {
                host: "203.0.113.2".into(),
                port: 9000,
            },
            serial.clone(),
        ] {
            let value = serde_json::to_value(&target).unwrap();
            assert_eq!(
                serde_json::from_value::<super::TransportTarget>(value).unwrap(),
                target
            );
        }
        assert!(super::TransportTarget::Telnet {
            host: "h".into(),
            port: 23
        }
        .plaintext());
        assert!(
            !serial.plaintext(),
            "a serial cable is not a plaintext network connection"
        );
    }

    #[test]
    fn image_redraw_deadlines_follow_frame_boundaries() {
        let start = Instant::now();
        for (elapsed, deadline) in [(0, 100), (99, 100), (100, 200), (235, 300)] {
            assert_eq!(
                next_image_frame(start, start + Duration::from_millis(elapsed)),
                start + Duration::from_millis(deadline)
            );
        }
    }

    /// Without any CJK font (a Windows machine before the fix had none of
    /// the listed paths) egui must still lay out text instead of panicking.
    #[test]
    fn egui_fonts_work_without_a_cjk_font() {
        for candidates in [
            vec![],
            vec![std::path::PathBuf::from("/nonexistent/font.ttc")],
        ] {
            let ctx = egui::Context::default();
            ctx.set_fonts(super::egui_font_definitions(&candidates));
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label("mtty 中文 \u{f07c}");
                });
            });
        }
        // The real candidates include the system font paths of every platform.
        let all = super::cjk_font_candidates();
        assert!(all.iter().any(|p| p.ends_with("msyh.ttc")));
        assert!(all.iter().any(|p| p.ends_with("PingFang.ttc")));
    }

    #[test]
    fn keymaps_use_command_on_macos_and_ctrl_shift_elsewhere() {
        use super::{shortcut_for, Key, ModifiersState, NamedKey};
        let ch = |c: &str| Key::Character(c.into());
        let m = |bits: &[ModifiersState]| bits.iter().fold(ModifiersState::empty(), |a, b| a | *b);
        let (cmd, ctrl, shift, alt) = (
            ModifiersState::SUPER,
            ModifiersState::CONTROL,
            ModifiersState::SHIFT,
            ModifiersState::ALT,
        );
        // macOS: ⌘T, ⌘⇧D, ⌘1.
        assert!(shortcut_for(&ch("t"), m(&[cmd]), true).unwrap().new_tab);
        assert!(
            shortcut_for(&ch("d"), m(&[cmd, shift]), true)
                .unwrap()
                .split_down
        );
        assert_eq!(
            shortcut_for(&ch("1"), m(&[cmd]), true).unwrap().select,
            Some(0)
        );
        assert!(shortcut_for(&ch("t"), m(&[ctrl, shift]), true).is_none());
        // Linux/Windows.
        let pc = |key: &Key, mods: &[ModifiersState]| shortcut_for(key, m(mods), false);
        assert!(pc(&ch("t"), &[ctrl, shift]).unwrap().new_tab);
        assert!(pc(&ch("k"), &[ctrl, shift]).unwrap().palette);
        assert!(pc(&ch("p"), &[ctrl, shift]).unwrap().palette);
        assert!(pc(&ch("d"), &[ctrl, shift]).unwrap().split_right);
        assert!(pc(&ch("d"), &[ctrl, shift, alt]).unwrap().split_down);
        assert!(pc(&ch("t"), &[ctrl, shift, alt]).unwrap().quick_terminal);
        assert_eq!(pc(&ch("g"), &[ctrl, shift, alt]).unwrap().find, -1);
        assert_eq!(pc(&ch("3"), &[alt]).unwrap().select, Some(2));
        let named = |n: NamedKey, mods: &[ModifiersState]| pc(&Key::Named(n), mods).unwrap();
        assert_eq!(named(NamedKey::PageDown, &[ctrl]).tab, 1);
        assert_eq!(named(NamedKey::PageUp, &[ctrl]).tab, -1);
        assert_eq!(named(NamedKey::Tab, &[ctrl, shift]).tab, -1);
        assert!(pc(&Key::Named(NamedKey::PageUp), &[ctrl, shift]).is_none());
        assert_eq!(pc(&ch("]"), &[ctrl, shift]).unwrap().cycle, 1);
        // macOS: ⌘] moves pane focus, ⌘⇧] switches tab.
        assert_eq!(shortcut_for(&ch("]"), m(&[cmd]), true).unwrap().cycle, 1);
        assert_eq!(
            shortcut_for(&ch("]"), m(&[cmd, shift]), true).unwrap().tab,
            1
        );
        assert!(pc(&ch(","), &[ctrl]).unwrap().settings);
        assert_eq!(pc(&ch("="), &[ctrl]).unwrap().font, 1.0);
        assert_eq!(pc(&ch("-"), &[ctrl]).unwrap().font, -1.0);
        // The shell keeps plain Ctrl chords, and the desktop keeps Super.
        for c in ["t", "w", "d", "k", "e", "f", "c", "r"] {
            assert!(
                pc(&ch(c), &[ctrl]).is_none(),
                "Ctrl+{c} belongs to the shell"
            );
        }
        assert!(pc(&ch("t"), &[cmd]).is_none());
        assert!(pc(&ch("1"), &[cmd]).is_none());
        assert!(pc(&ch("x"), &[ctrl, shift]).is_none());
    }

    #[test]
    fn windows_does_not_probe_the_opengl_backend() {
        if std::env::var_os("WGPU_BACKEND").is_some() {
            return;
        }
        let backends = super::wgpu_backends();
        assert_eq!(backends.contains(wgpu::Backends::GL), !cfg!(windows));
        assert!(backends.contains(if cfg!(windows) {
            wgpu::Backends::DX12
        } else {
            wgpu::Backends::all()
        }));
    }

    #[test]
    fn ctrl_shift_v_pastes_off_macos() {
        use super::{input::KeyKind, terminal_paste_shortcut};
        assert_eq!(
            terminal_paste_shortcut(KeyKind::Char('V'), false, true, true, false),
            !cfg!(target_os = "macos")
        );
    }

    #[test]
    fn terminal_clipboard_shortcut_preserves_application_control_v_on_macos() {
        use super::{input::KeyKind, terminal_paste_shortcut};
        assert_eq!(
            terminal_paste_shortcut(KeyKind::Char('v'), true, false, false, false),
            cfg!(target_os = "macos")
        );
        assert_eq!(
            terminal_paste_shortcut(KeyKind::Char('v'), false, true, false, false),
            !cfg!(target_os = "macos")
        );
        assert!(!terminal_paste_shortcut(
            KeyKind::Char('v'),
            true,
            false,
            true,
            false
        ));
        assert!(!terminal_paste_shortcut(
            KeyKind::Char('v'),
            true,
            false,
            false,
            true
        ));
        assert!(!terminal_paste_shortcut(
            KeyKind::Char('c'),
            true,
            false,
            false,
            false
        ));
    }

    #[test]
    fn link_detection() {
        let line = "see https://example.com/a?b=1 now";
        assert_eq!(
            super::link_at(line, 8).map(|(u, _, _)| u).as_deref(),
            Some("https://example.com/a?b=1")
        );
        assert_eq!(super::link_at(line, 0), None);
        assert_eq!(
            super::link_at("(https://x.io).", 2)
                .map(|(u, _, _)| u)
                .as_deref(),
            Some("https://x.io")
        );
    }

    use super::*;

    #[test]
    fn overlay_drag_does_not_start_terminal_selection() {
        let ctx = egui::Context::default();
        let mut window_rect = egui::Rect::NOTHING;
        let mut frame = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 700.0),
                )),
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                let response = egui::Window::new("Rename Tab")
                    .default_pos(egui::pos2(300.0, 200.0))
                    .show(ctx, |ui| {
                        ui.text_edit_singleline(&mut String::from("shell 1"));
                    });
                window_rect = response.unwrap().response.rect;
            });
            window_rect
        };
        frame(vec![]);
        let rect = frame(vec![]);
        let start = rect.min + egui::vec2(40.0, 12.0);
        frame(vec![egui::Event::PointerMoved(start)]);
        frame(vec![egui::Event::PointerButton {
            pos: start,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }]);
        assert!(ctx.wants_pointer_input());
        assert!(!pointer_to_terminal(ctx.wants_pointer_input(), true, false));
        let end = start + egui::vec2(100.0, 80.0);
        frame(vec![egui::Event::PointerMoved(end)]);
        assert!(!pointer_to_terminal(ctx.wants_pointer_input(), true, false));
        let moved = frame(vec![egui::Event::PointerButton {
            pos: end,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        assert!(
            moved.min.distance(rect.min) > 20.0,
            "the overlay itself must still move"
        );
        // Terminal-started drags must receive release outside their pane.
        assert!(pointer_to_terminal(true, false, true));
        assert!(!pointer_to_terminal(false, false, false));
        assert!(pointer_to_terminal(false, true, false));
    }

    /// Run one egui frame with replayed input.
    fn replay(ctx: &egui::Context, events: Vec<egui::Event>, mut ui: impl FnMut(&egui::Context)) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 700.0),
            )),
            events,
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| ui(ctx));
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// Drag a window's right edge `dx` points and return its width after.
    fn drag_window_edge(body: fn(&mut egui::Ui), dx: f32) -> (f32, f32) {
        let ctx = egui::Context::default();
        let mut rect = egui::Rect::NOTHING;
        let mut frame = |events: Vec<egui::Event>| {
            replay(&ctx, events, |ctx| {
                rect = app_window("doc", ctx)
                    .show(ctx, body)
                    .unwrap()
                    .response
                    .rect;
            });
            rect
        };
        frame(vec![]);
        let before = frame(vec![]);
        let start = egui::pos2(before.right() - 1.0, before.center().y);
        frame(vec![egui::Event::PointerMoved(start)]);
        frame(vec![egui::Event::PointerButton {
            pos: start,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }]);
        for step in 1..=6 {
            frame(vec![egui::Event::PointerMoved(
                start + egui::vec2(dx * step as f32 / 6.0, 0.0),
            )]);
        }
        frame(vec![egui::Event::PointerButton {
            pos: start + egui::vec2(dx, 0.0),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        let after = frame(vec![]);
        (before.width(), after.width())
    }

    #[test]
    fn trackpad_deltas_add_up_to_lines() {
        let mut acc = 0.0;
        let total: i32 = (0..10).map(|_| wheel_lines(&mut acc, 0.25)).sum();
        assert_eq!(total, 2, "ten quarter-line deltas are two lines, not zero");
        assert!((acc - 0.5).abs() < 1e-9);
        assert_eq!(wheel_lines(&mut acc, -0.75), 0, "a reversal starts afresh");
        assert_eq!(wheel_lines(&mut acc, -0.5), -1);
        assert_eq!(
            wheel_lines(&mut 0.0, 3.0),
            3,
            "a mouse wheel notch passes through"
        );
    }

    #[test]
    fn wheel_on_the_alternate_screen_sends_arrow_keys() {
        assert_eq!(alternate_scroll_keys(2, false), b"\x1b[A\x1b[A");
        assert_eq!(alternate_scroll_keys(-1, false), b"\x1b[B");
        assert_eq!(
            alternate_scroll_keys(1, true),
            b"\x1bOA",
            "application cursor keys"
        );
        assert_eq!(alternate_scroll_keys(-1000, true).len(), 64 * 3, "capped");
    }

    #[test]
    fn dialog_button_row_keeps_an_auto_sized_window_short() {
        // Software Update: anchored, not resizable, sized by its content.
        let ctx = egui::Context::default();
        let mut window = egui::Rect::NOTHING;
        let mut close = egui::Rect::NOTHING;
        for _ in 0..3 {
            replay(&ctx, vec![], |ctx| {
                window = egui::Window::new("Software Update")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                    .default_width(340.0)
                    .show(ctx, |ui| {
                        ui.heading("A new version is available");
                        ui.label("mtty 0.0.8");
                        ui.add_space(16.0);
                        button_row(ui, |ui| {
                            let _ = ui.button("Download Update");
                            close = ui.button("Close").rect;
                        });
                    })
                    .unwrap()
                    .response
                    .rect;
            });
        }
        assert!(window.height() < 200.0, "window is {window:?}");
        assert!(
            window.top() > 200.0,
            "the title stays on screen: {window:?}"
        );
        assert!(
            close.bottom() <= window.bottom() && close.right() > window.center().x,
            "buttons sit at the bottom right: {close:?} in {window:?}"
        );
    }

    /// Drag a window's bottom-right corner by `d`; its rect before and after.
    fn drag_window_corner(body: fn(&mut egui::Ui), d: egui::Vec2) -> (egui::Rect, egui::Rect) {
        let ctx = egui::Context::default();
        configure_egui(&ctx, &mtty_ui::theme::Chrome::dark());
        let mut rect = egui::Rect::NOTHING;
        let mut frame = |events: Vec<egui::Event>| {
            replay(&ctx, events, |ctx| {
                rect = app_window("doc", ctx)
                    .show(ctx, body)
                    .unwrap()
                    .response
                    .rect;
            });
            rect
        };
        frame(vec![]);
        let before = frame(vec![]);
        // The very corner, where the edges' grab areas overlap the corner's.
        let start = before.right_bottom();
        frame(vec![egui::Event::PointerMoved(start)]);
        frame(vec![egui::Event::PointerButton {
            pos: start,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }]);
        for step in 1..=6 {
            frame(vec![egui::Event::PointerMoved(
                start + d * (step as f32 / 6.0),
            )]);
        }
        frame(vec![egui::Event::PointerButton {
            pos: start + d,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        (before, frame(vec![]))
    }

    #[test]
    fn editor_window_corner_resizes_width_and_height_together() {
        // The editor's layout: a toolbar, then a line-number column beside a
        // full-width code field, both scrolling.
        fn body(ui: &mut egui::Ui) {
            ui.horizontal(|ui| {
                let _ = ui.button("Save");
            });
            ui.separator();
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        ui.label("   1\n   2\n");
                        let mut text = "fn main() {}\n".repeat(80);
                        ui.add(
                            egui::TextEdit::multiline(&mut text)
                                .code_editor()
                                .desired_width(f32::INFINITY),
                        );
                    });
                });
        }
        let (before, after) = drag_window_corner(body, egui::vec2(-150.0, -120.0));
        assert!(
            after.width() < before.width() - 100.0,
            "{before:?} -> {after:?}"
        );
        assert!(
            after.height() < before.height() - 80.0,
            "{before:?} -> {after:?}"
        );
        let (before, after) = drag_window_corner(body, egui::vec2(120.0, 100.0));
        assert!(
            after.width() > before.width() + 80.0,
            "{before:?} -> {after:?}"
        );
        assert!(
            after.height() > before.height() + 60.0,
            "{before:?} -> {after:?}"
        );
    }

    /// Whether the Markdown preview of `md` (a document in `base`) paints a
    /// loaded image, letting the background file loader finish.
    fn markdown_paints_an_image(md: &str, base: &std::path::Path) -> bool {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        let mut cache = egui_commonmark::CommonMarkCache::default();
        let mut mmd = Mmd::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let output = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    render_markdown(
                        ui,
                        md,
                        Some(base),
                        &mut cache,
                        &mut mmd,
                        egui::Color32::WHITE,
                        egui::Color32::BLACK,
                    );
                });
            });
            // egui 0.30 paints an image as a rect filled with its texture.
            let image = output.shapes.iter().any(|s| {
                matches!(&s.shape, egui::Shape::Rect(r)
                    if r.fill_texture_id != egui::TextureId::default())
            });
            if image {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn markdown_images_resolve_relative_to_the_document() {
        const PNG: [u8; 73] = [
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2,
            8, 2, 0, 0, 0, 253, 212, 154, 115, 0, 0, 0, 16, 73, 68, 65, 84, 120, 156, 99, 248, 207,
            192, 0, 68, 12, 16, 10, 0, 31, 238, 3, 253, 139, 95, 20, 212, 0, 0, 0, 0, 73, 69, 78,
            68, 174, 66, 96, 130,
        ];
        let dir = std::env::temp_dir().join(format!("mtty-md-img-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("img")).unwrap();
        std::fs::write(dir.join("img/dot.png"), PNG).unwrap();
        assert!(
            markdown_paints_an_image("![dot](img/dot.png)\n", &dir),
            "a relative path next to the document"
        );
        let absolute = format!("![dot]({})\n", dir.join("img/dot.png").display());
        assert!(
            markdown_paints_an_image(&absolute, std::path::Path::new("/nonexistent")),
            "an absolute path ignores the document directory"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edit_in_tab_prefers_the_config_then_editor_then_vi() {
        let path = if cfg!(windows) {
            "C:\\notes\\a b.md"
        } else {
            "/tmp/a b.md"
        };
        let quoted = local_path_arg(path);
        assert_eq!(
            edit_in_tab_command(Some("code --wait"), Some("nvim"), path),
            format!("code --wait {quoted}")
        );
        assert_eq!(
            edit_in_tab_command(Some("  "), Some("nvim"), path),
            format!("nvim {quoted}"),
            "a blank config value falls through"
        );
        let fallback = if cfg!(windows) { "notepad" } else { "vi" };
        assert_eq!(
            edit_in_tab_command(None, None, path),
            format!("{fallback} {quoted}")
        );
        if !cfg!(windows) {
            assert_eq!(quoted, "'/tmp/a b.md'", "spaces stay in one argument");
        }
    }

    #[test]
    fn markdown_table_columns_do_not_overlap() {
        // A key/description table with inline code, like miao's guide: the
        // first wrap patch narrowed the whole grid per cell, so the second
        // column was drawn over the first. mtty's own style: its row stripes
        // are opaque.
        let ctx = egui::Context::default();
        configure_egui(&ctx, &mtty_ui::theme::Chrome::dark());
        let md = "| Field | Purpose |\n|---|---|\n\
            | `model` | default model (`provider/model`) |\n\
            | `default_agent` | default agent |\n\
            | `permission` | permission rules (`allow` / `ask` / `deny`, by tool/path); \
            unmatched defaults to `ask` |\n\
            | `agents` | custom agents (model, system prompt, permissions, step cap) |\n\
            | `skills` / `commands` / `instructions` | skills, commands, instructions |\n\
            | `lsp` | language servers: `true` enables all built-ins, `false` disables, \
            or a per-name record. **Omitted = all disabled** |\n";
        let mut cache = egui_commonmark::CommonMarkCache::default();
        let mut texts: Vec<(String, egui::Rect)> = Vec::new();
        let mut starts: Vec<f32> = Vec::new();
        let mut viewport = egui::Rect::NOTHING;
        for _ in 0..4 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(500.0, 600.0),
                )),
                ..Default::default()
            };
            let output = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    viewport = egui::ScrollArea::both()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            egui_commonmark::CommonMarkViewer::new().show(ui, &mut cache, md);
                        })
                        .inner_rect;
                });
            });
            // A filled rect painted after a text (the next row's stripe)
            // hides whatever part of the text it covers.
            for (i, shape) in output.shapes.iter().enumerate() {
                let egui::Shape::Text(t) = &shape.shape else {
                    continue;
                };
                let text_rect = t.visual_bounding_rect().shrink(1.0);
                for later in &output.shapes[i + 1..] {
                    if let egui::Shape::Rect(r) = &later.shape {
                        assert!(
                            r.fill.a() == 0 || !r.rect.intersects(text_rect),
                            "{:?} at {text_rect:?} is painted over by {:?}",
                            t.galley.text(),
                            r.rect
                        );
                    }
                }
            }
            starts = output
                .shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::Shape::Text(t) if t.galley.text().starts_with("custom agents") => Some(t),
                    _ => None,
                })
                .flat_map(|t| {
                    t.galley
                        .rows
                        .iter()
                        .filter_map(move |r| r.glyphs.first().map(|g| t.pos.x + g.pos.x))
                })
                .collect();
            // One rect per laid-out line, spanning its glyphs: a wrapped run
            // starts mid-line, so its bounding box covers text before it.
            texts = output
                .shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::Shape::Text(t) if !t.galley.text().trim().is_empty() => Some(t),
                    _ => None,
                })
                .flat_map(|t| {
                    let text = t.galley.text().to_string();
                    t.galley.rows.iter().filter_map(move |r| {
                        let first = r.glyphs.iter().find(|g| !g.chr.is_whitespace())?;
                        let last = r.glyphs.iter().rev().find(|g| !g.chr.is_whitespace())?;
                        let rect = egui::Rect::from_min_max(
                            t.pos + egui::vec2(first.pos.x, r.rect.min.y),
                            t.pos + egui::vec2(last.max_x(), r.rect.max.y),
                        );
                        Some((text.clone(), rect.shrink(1.0)))
                    })
                })
                .collect();
        }
        for (i, (a, ra)) in texts.iter().enumerate() {
            for (b, rb) in &texts[i + 1..] {
                assert!(
                    !ra.intersects(*rb),
                    "{a:?} at {ra:?} overlaps {b:?} at {rb:?}"
                );
            }
            assert!(
                ra.right() <= viewport.right() + 1.0,
                "{a:?} at {ra:?} is cut off"
            );
        }
        // The key column keeps its natural width: `default_agent` on one line.
        let key = texts.iter().find(|(t, _)| t == "default_agent").unwrap().1;
        assert!(key.height() < 24.0, "{key:?}");
        // A wrapped cell's lines start at the same x (upstream put a
        // two-space label before each cell's first line).
        assert!(starts.len() >= 2, "the long cell wraps: {starts:?}");
        assert!(
            starts.iter().all(|x| (x - starts[0]).abs() < 0.5),
            "wrapped lines start at {starts:?}"
        );
    }

    #[test]
    fn markdown_table_cells_wrap_inside_the_preview() {
        // A long cell used to be truncated at the window edge (egui truncates
        // text in horizontal layouts), so scrolling could not reveal it.
        let ctx = egui::Context::default();
        let md = "| Product | For | What |\n|---|---|---|\n| Gateway | Developers | \
            one account and one key reach forty model vendors, billed per token, \
            routed to the cheapest or fastest channel, failing over to a backup \
            channel automatically END |\n";
        let mut cache = egui_commonmark::CommonMarkCache::default();
        let mut last = None;
        for _ in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(500.0, 400.0),
                )),
                ..Default::default()
            };
            let mut size = None;
            let output = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let o = egui::ScrollArea::both()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            egui_commonmark::CommonMarkViewer::new().show(ui, &mut cache, md);
                        });
                    size = Some((o.content_size, o.inner_rect));
                });
            });
            last = Some((output, size.unwrap()));
        }
        let (output, (content, viewport)) = last.unwrap();
        assert!(
            content.x <= viewport.width() + 1.0,
            "the table fits the preview: {content:?} in {viewport:?}"
        );
        let shown: String = output
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some(
                    t.galley
                        .rows
                        .iter()
                        .flat_map(|r| r.glyphs.iter().map(|g| g.chr))
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect();
        assert!(shown.contains("END"), "the whole cell is laid out: {shown}");
    }

    #[test]
    fn windows_with_wide_content_can_be_resized_both_ways() {
        // A long unwrapped line (a Markdown code block) and a full-width text
        // field used to pin the window at the widest size.
        fn body(ui: &mut egui::Ui) {
            let content = |ui: &mut egui::Ui| {
                ui.add(egui::Label::new("x".repeat(400)).wrap_mode(egui::TextWrapMode::Extend));
                let mut text = String::from("note");
                ui.add(egui::TextEdit::multiline(&mut text).desired_width(f32::INFINITY));
            };
            window_body(ui, content);
        }
        let (before, narrower) = drag_window_edge(body, -200.0);
        assert!(
            before < 1000.0,
            "the window does not fill the screen: {before}"
        );
        assert!(narrower < before - 150.0, "{before} -> {narrower}");
        let (before, wider) = drag_window_edge(body, 150.0);
        assert!(wider > before + 100.0, "{before} -> {wider}");
    }

    #[test]
    fn rename_dialog_commits_on_enter_and_cancels_on_escape() {
        use mtty_ui::i18n::Lang;
        let ctx = egui::Context::default();
        let mut buf = String::new();
        let mut outcome = DialogOutcome::Open;
        let mut frame = |events: Vec<egui::Event>, buf: &mut String| {
            replay(&ctx, events, |ctx| {
                outcome_set(&mut outcome, rename_dialog(ctx, Lang::En, buf))
            });
            std::mem::replace(&mut outcome, DialogOutcome::Open)
        };
        fn outcome_set(slot: &mut DialogOutcome, value: DialogOutcome) {
            *slot = value;
        }
        assert_eq!(frame(vec![], &mut buf), DialogOutcome::Open);
        assert_eq!(
            frame(vec![egui::Event::Text("api 服务".into())], &mut buf),
            DialogOutcome::Open
        );
        assert_eq!(buf, "api 服务", "typing reaches the focused field");
        assert_eq!(
            frame(vec![key(egui::Key::Enter)], &mut buf),
            DialogOutcome::Commit
        );

        let mut other = String::from("keep");
        frame(vec![], &mut other);
        assert_eq!(
            frame(vec![key(egui::Key::Escape)], &mut other),
            DialogOutcome::Cancel
        );
        assert_eq!(other, "keep");
    }

    #[test]
    fn a_paste_routed_to_a_focused_field_lands_there() {
        // Menu Paste is pushed into egui's input when a field has focus
        // (`State::edit_in_text_field`); the field must receive it.
        let ctx = egui::Context::default();
        let mut text = String::from("a");
        let mut wants = false;
        let mut frame = |events: Vec<egui::Event>, text: &mut String| {
            replay(&ctx, events, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add(egui::TextEdit::singleline(text).id(egui::Id::new("field")))
                        .request_focus();
                });
                wants = ctx.wants_keyboard_input();
            });
            wants
        };
        frame(vec![], &mut text);
        assert!(
            frame(vec![], &mut text),
            "a focused field wants the keyboard"
        );
        frame(vec![egui::Event::Paste("bc".into())], &mut text);
        assert!(text.contains("bc"), "{text:?}");
    }

    #[test]
    fn dragging_a_split_divider_resizes_the_panes() {
        let mut layout = Layout::leaf("a");
        assert!(layout.split("a", "b", SplitDir::Right));
        let area = Rect {
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 600.0,
        };
        let scale = 2.0;
        let handle = &layout.handles(area)[0];
        let (px, py) = (
            (handle.rect.x + handle.rect.w / 2.0) * scale,
            (handle.rect.y + handle.rect.h / 2.0) * scale,
        );
        let hit = divider_at(layout.handles(area), px, py, scale).expect("press on the divider");
        assert!(divider_at(layout.handles(area), 10.0, 10.0, scale).is_none());
        // Drag to 70% of the width (physical pixels at 2x).
        let r = divider_ratio(hit.dir, hit.area, 1400.0, py, scale);
        layout.set_ratio(&hit.path, r);
        let rects = layout.rects(area);
        let a = rects.iter().find(|(id, _)| id == "a").unwrap().1;
        assert!((a.w - 700.0).abs() < 2.0, "{a:?}");
        // Dragging past the edge is clamped.
        layout.set_ratio(
            &hit.path,
            divider_ratio(hit.dir, hit.area, 5000.0, py, scale),
        );
        let a = layout
            .rects(area)
            .into_iter()
            .find(|(id, _)| id == "a")
            .unwrap()
            .1;
        assert!(a.w <= 900.0 + 2.0, "{a:?}");
    }

    fn empty_tab(title: &str) -> Tab {
        Tab {
            layout: Layout::leaf(title),
            panes: vec![],
            active: title.into(),
            title: title.into(),
            title_set: false,
            shown: None,
            ssh: false,
            ssh_target: None,
            ssh_cmd: None,
            transport: None,
            prefix: None,
            mark: None,
            group: None,
            attention: None,
            editors: Vec::new(),
            previews: Vec::new(),
        }
    }

    fn item(label: &str, sort: &str) -> mtty_lsp::CompletionItem {
        mtty_lsp::CompletionItem {
            label: label.into(),
            kind: 0,
            detail: None,
            filter: label.into(),
            sort: sort.into(),
            insert: label.into(),
            cursor: None,
            range: None,
            additional: Vec::new(),
        }
    }

    #[test]
    fn completions_rank_prefixes_before_scattered_letters() {
        let items = vec![
            item("to_string", "2"),
            item("trim", "1"),
            item("to_owned", "1"),
            item("len", "0"),
            item("ToString", "3"),
        ];
        let label = |v: Vec<usize>| {
            v.into_iter()
                .map(|i| items[i].label.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            label(filter_completions(&items, "to")),
            vec!["to_owned", "to_string", "ToString"],
            "prefix (case ignored), then the server's order"
        );
        assert_eq!(
            label(filter_completions(&items, "ts")),
            vec!["to_string", "ToString"],
            "letters in order"
        );
        assert_eq!(filter_completions(&items, "").len(), 5);
        assert!(filter_completions(&items, "zz").is_empty());
    }

    #[test]
    fn accepting_a_completion_writes_its_range_snippet_and_import() {
        use mtty_lsp::{LspRange, Pos};
        let text = "fn main() {\n    v.pu\n}\n";
        let mut rope = mtty_editor::Rope::from_str(text);
        let caret = text.find("pu").unwrap() + 2;
        let start = caret - 2;
        let p = |line, character| Pos { line, character };
        let mut push = item("push", "");
        push.insert = "push(value)".into();
        push.cursor = Some(5);
        push.range = Some(LspRange {
            start: p(1, 6),
            end: p(1, 8),
        });
        push.additional = vec![(
            LspRange {
                start: p(0, 0),
                end: p(0, 0),
            },
            "use std::vec::Vec;\n".into(),
        )];
        let (tx, after) = completion_edit(&rope, caret, start, &push, mtty_lsp::Encoding::Utf16);
        tx.apply(&mut rope);
        let out = rope.to_string();
        assert_eq!(
            out,
            "use std::vec::Vec;\nfn main() {\n    v.push(value)\n}\n"
        );
        assert_eq!(&out[after..after + 5], "value", "caret on the placeholder");
        // No range: the typed word is replaced; the caret ends after it.
        let mut rope = mtty_editor::Rope::from_str("let x = le");
        let (tx, after) =
            completion_edit(&rope, 10, 8, &item("len", ""), mtty_lsp::Encoding::Utf16);
        tx.apply(&mut rope);
        assert_eq!((rope.to_string().as_str(), after), ("let x = len", 11));
    }

    #[test]
    fn the_word_before_the_caret() {
        let rope = mtty_editor::Rope::from_str("let x = foo_bar.ba");
        assert_eq!(word_start(&rope, 18), 16);
        assert_eq!(word_start(&rope, 15), 8);
        assert_eq!(word_start(&rope, 16), 16, "right after the dot");
    }

    #[test]
    fn closing_an_editor_takes_its_previews_along() {
        let mut tab = empty_tab("t1");
        let ed = editor_pane::EditorPane::with_doc(
            "e1".into(),
            "/tmp/notes.md".into(),
            mtty_editor::Document::from_text("# Notes\n"),
        );
        assert!(tab.layout.split("t1", "e1", SplitDir::Right));
        tab.editors.push(ed);
        let preview = PreviewPane::new("e1".into());
        let pid = preview.id.clone();
        assert!(tab.layout.split("e1", &pid, SplitDir::Right));
        tab.previews.push(preview);
        assert!(tab.has_pane(&pid) && tab.has_pane("e1") && !tab.has_pane("x"));
        let saved = tab.session_value();
        assert_eq!(saved["previews"][0]["source"], "e1");
        assert_eq!(saved["previews"][0]["id"], pid.as_str());
        tab.remove_editor("e1");
        assert!(tab.previews.is_empty());
        let _ = tab.layout.remove("e1");
        assert_eq!(tab.layout.ids(), vec!["t1".to_string()]);
    }

    #[test]
    fn restored_title_holds_until_the_pane_reports() {
        // A renamed tab keeps its chosen name.
        assert_eq!(fixed_tab_title(true, "work", None).as_deref(), Some("work"));
        // Before the pane reports, a restored tab shows the title it had
        // before the restart, not `shell N`.
        assert_eq!(
            fixed_tab_title(false, "shell 5", Some("shell 5")).as_deref(),
            Some("shell 5")
        );
        assert_eq!(
            fixed_tab_title(false, "shell 5", Some("miao")).as_deref(),
            Some("miao")
        );
        // A chosen name beats a stale restored one.
        assert_eq!(
            fixed_tab_title(true, "work", Some("miao")).as_deref(),
            Some("work")
        );
        // Nothing fixed: fall through to the automatic title.
        assert_eq!(fixed_tab_title(false, "shell 5", None), None);
        assert_eq!(fixed_tab_title(false, "shell 5", Some("")), None);
        // An empty chosen title is not a choice.
        assert_eq!(fixed_tab_title(true, "", None), None);
        // Once the pane reports live context the saved title yields, so a
        // stale one cannot freeze the tab over a cd or a new program.
        assert_eq!(restored_title(Some("home"), Some("/x/miao"), None), None);
        assert_eq!(restored_title(Some("home"), None, Some("vim")), None);
        assert_eq!(
            restored_title(Some("home"), None, None).as_deref(),
            Some("home")
        );
        assert_eq!(restored_title(None, None, None), None);
        assert_eq!(restored_title(Some(""), None, None), None);
    }

    #[test]
    fn tab_decorations_round_trip_and_legacy_defaults() {
        let mut tab = empty_tab("shell");
        tab.prefix = Some("dev".into());
        tab.mark = Some("★".into());
        tab.group = Some("work".into());
        let encoded = serde_json::to_vec(&tab.session_value()).unwrap();
        let value = serde_json::from_slice(&encoded).unwrap();
        let mut restored = empty_tab("shell");
        restored.restore_decorations(&value);
        assert_eq!(restored.prefix, tab.prefix);
        assert_eq!(restored.mark, tab.mark);
        assert_eq!(restored.group, tab.group);
        restored.restore_decorations(&serde_json::json!({"title": "legacy"}));
        assert_eq!(
            (restored.prefix, restored.mark, restored.group),
            (None, None, None)
        );
    }

    #[test]
    fn details_words_are_translated_and_data_is_kept() {
        use mtty_ui::i18n::Lang;
        assert_eq!(
            localize_detail(Lang::Zh, "not a git repository"),
            "不是 git 仓库"
        );
        assert_eq!(localize_detail(Lang::Zh, "src/main.rs"), "src/main.rs");
        assert_eq!(localize_detail(Lang::En, "Directory"), "Directory");
        assert_eq!(localize_detail(Lang::Zh, "tty"), "终端设备");
        assert_eq!(agent_state_label(Lang::Zh, "awaiting"), "等待你");
        assert_eq!(agent_state_label(Lang::Zh, "processing"), "处理中");
        assert_eq!(agent_state_label(Lang::En, "idle"), "Idle");
        assert_eq!(agent_state_label(Lang::En, "unknown"), "Status unavailable");
        assert_eq!(
            agent_state_label(Lang::En, "incomplete"),
            "Paused · unfinished"
        );
        assert_eq!(agent_state_label(Lang::Zh, "custom"), "custom");
        assert_eq!(
            localize_detail(Lang::Zh, "idle"),
            "idle",
            "only Agent state values are states"
        );
    }

    #[test]
    fn quota_line_formats_and_warns_at_the_threshold() {
        let q = serde_json::json!({ "used": 42, "limit": 100, "unit": "percent", "window": "5h" });
        assert_eq!(quota_line(&q, 80).as_deref(), Some("42/100 percent (5h)"));
        let q = serde_json::json!({ "used": 90, "limit": 100 });
        assert_eq!(quota_line(&q, 80).as_deref(), Some("⚠ 90/100"));
        assert_eq!(quota_line(&q, 95).as_deref(), Some("90/100"));
        assert_eq!(quota_line(&serde_json::json!({ "limit": 100 }), 80), None);
    }

    #[test]
    fn attention_follows_agent_transitions_and_keeps_the_most_urgent() {
        assert_eq!(
            Attention::for_transition(Some("processing"), "idle"),
            Some(Attention::Done)
        );
        assert_eq!(Attention::for_transition(None, "idle"), None);
        assert_eq!(
            Attention::for_transition(Some("idle"), "awaiting"),
            Some(Attention::Needs)
        );
        assert_eq!(Attention::for_transition(Some("idle"), "processing"), None);
        assert!(Attention::Needs > Attention::Done && Attention::Done > Attention::Unread);
        // Looking at a tab clears what it says except that its agent finished.
        assert_eq!(
            Attention::seen(Some(Attention::Done)),
            Some(Attention::Done)
        );
        assert_eq!(Attention::seen(Some(Attention::Unread)), None);
        assert_eq!(Attention::seen(Some(Attention::Needs)), None);
        assert_eq!(Attention::seen(None), None);
        assert!(Attention::Done.shows_while_visible());
        assert!(!Attention::Needs.shows_while_visible());
        assert_eq!(
            Some(Attention::Done).max(Some(Attention::Unread)),
            Some(Attention::Done)
        );
    }

    #[test]
    fn badges_switch_states_off_the_tabs() {
        use mtty_ui::icons::Icon;
        let ch = mtty_ui::theme::Chrome::dark();
        let all = mtty_config::Badges::default();
        assert_eq!(
            super::shown_agent_icon(&all, &ch, "processing", None).map(|i| i.0),
            Some(Icon::StateBusy)
        );
        let quiet = mtty_config::Badges {
            processing: false,
            idle: false,
            ..Default::default()
        };
        assert_eq!(
            super::shown_agent_icon(&quiet, &ch, "processing", None),
            None
        );
        assert_eq!(
            super::shown_agent_icon(&quiet, &ch, "idle", Some(Attention::Done)),
            None
        );
        assert_eq!(
            super::shown_agent_icon(&quiet, &ch, "awaiting", None).map(|i| i.0),
            Some(Icon::StateWait)
        );
        // A state the switches do not name still shows.
        assert_eq!(
            super::shown_agent_icon(&quiet, &ch, "busy", None).map(|i| i.0),
            Some(Icon::StateEmpty)
        );
    }

    #[test]
    fn agent_tabs_show_their_state_by_shape() {
        use mtty_ui::icons::Icon;
        let ch = mtty_ui::theme::Chrome::dark();
        let shape = |state, attention| super::agent_icon(&ch, state, attention).0;
        assert_eq!(shape("completed", None), Icon::StateFull);
        assert_eq!(
            shape("waiting", Some(Attention::Done)),
            Icon::StateBackground
        );
        assert_eq!(
            shape("incomplete", Some(Attention::Done)),
            Icon::StatePaused
        );
        assert_eq!(shape("unknown", Some(Attention::Done)), Icon::StateUnknown);
        assert_eq!(super::agent_icon(&ch, "unknown", None).1, Some(ch.muted));
        assert_ne!(shape("waiting", None), shape("awaiting", None));
        assert_ne!(shape("incomplete", None), shape("unknown", None));
        assert_eq!(shape("processing", None), Icon::StateBusy);
        assert_eq!(shape("processing", Some(Attention::Done)), Icon::StateBusy);
        assert_eq!(shape("idle", Some(Attention::Done)), Icon::StateFull);
        assert_eq!(shape("idle", None), Icon::StateEmpty, "seen and acted on");
        assert_eq!(shape("awaiting", None), Icon::StateWait);
        assert_eq!(shape("error", None), Icon::StateFull);
        // Waiting and finished differ by shape and by colour.
        assert_ne!(
            shape("awaiting", None),
            shape("idle", Some(Attention::Done))
        );
        assert_ne!(
            super::agent_icon(&ch, "idle", Some(Attention::Done)).1,
            super::agent_icon(&ch, "awaiting", None).1
        );
    }

    #[test]
    fn window_state_reads_old_and_new_files() {
        let default = WindowState::default();
        // Files from before panel widths existed: size only, panels open.
        assert_eq!(WindowState::parse("1100 720"), default);
        // Widths only; the panels stay open and the details tab is the first.
        assert_eq!(
            WindowState::parse("1100 720 260 340\n"),
            WindowState {
                sidebar_w: 260.0,
                details_w: 340.0,
                ..default
            }
        );
        assert_eq!(
            WindowState::parse("1100 720 9999 1"),
            WindowState {
                sidebar_w: 480.0,
                details_w: 200.0,
                ..default
            },
            "widths are clamped"
        );
        assert_eq!(WindowState::parse("1100 720 x NaN"), default);
        // A full line from this version.
        assert_eq!(
            WindowState::parse("1400 900 240 360 0 1 4"),
            WindowState {
                size: (1400.0, 900.0),
                sidebar_w: 240.0,
                details_w: 360.0,
                sidebar_open: false,
                details_open: true,
                details_tab: 4,
            }
        );
    }

    #[test]
    fn window_state_round_trips_through_save_format() {
        let state = WindowState {
            size: (1400.0, 900.0),
            sidebar_w: 240.0,
            details_w: 360.0,
            sidebar_open: false,
            details_open: false,
            details_tab: 6,
        };
        assert_eq!(WindowState::parse(&state.format()), state);
    }

    #[test]
    #[cfg(unix)]
    fn agent_paths_match_editor_buffers_through_directory_symlinks() {
        let root = std::env::temp_dir().join(format!("mtty-agent-path-{}", std::process::id()));
        let actual = root.join("actual");
        let alias = root.join("alias");
        std::fs::create_dir_all(&actual).unwrap();
        std::os::unix::fs::symlink(&actual, &alias).unwrap();
        // New files must match too, before either spelling exists on disk.
        assert!(same_file_path(
            &actual.join("new.txt"),
            &alias.join("new.txt")
        ));
        std::fs::write(actual.join("new.txt"), "unsaved buffer fixture").unwrap();
        assert!(same_file_path(
            &actual.join("new.txt"),
            &alias.join("new.txt")
        ));
        assert!(!same_file_path(
            &actual.join("new.txt"),
            &alias.join("other.txt")
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn snippets_run_on_hosts_through_one_quoted_ssh_command() {
        let command = "df -h | grep '/dev' && echo \"$HOME\"";
        let cmd = super::remote_run_command_with(
            mtty_ui::ssh::Syntax::Posix,
            "deploy@203.0.113.5",
            &["-p".into(), "2222".into()],
            command,
        );
        let args = cmd.strip_prefix("ssh -t ").unwrap();
        // The shell must split it back into exactly these arguments.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("for a in {args}; do printf '%s\\0' \"$a\"; done"))
            .output()
            .unwrap();
        let parsed: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        assert_eq!(parsed, ["-p", "2222", "deploy@203.0.113.5", command]);
    }

    #[test]
    fn view_rules_match_the_ssh_host() {
        assert_eq!(ssh_host("deploy@work:2200"), "work");
        assert_eq!(ssh_host("work"), "work");
    }

    #[test]
    fn default_titles_are_told_apart_from_chosen_ones() {
        assert!(is_default_title("shell 1"));
        assert!(is_default_title("shell 12"));
        for chosen in [
            "shell",
            "shell x",
            "deploy@work",
            "Quick",
            "api 服务",
            "shell 1a",
        ] {
            assert!(!is_default_title(chosen), "{chosen}");
        }
    }

    #[test]
    fn restored_contents_drop_earlier_mtty_notes() {
        let saved = "$ ls\r\nsrc\r\n\x1b[0;2m[mtty] Restored from the last session.\x1b[0m\r\n\x1b[0;2m[mtty] Was running: vim a  Press Enter to run it again.\x1b[0m\r\n$ echo [mtty] stays\r\n";
        assert_eq!(
            without_mtty_notes(saved),
            "$ ls\r\nsrc\r\n$ echo [mtty] stays\r\n",
            "only lines starting with the note go"
        );
    }

    #[test]
    fn ime_stays_on_after_a_text_field_loses_focus() {
        // Without a focused egui field, egui-winit would turn IME off for
        // the window; the terminal's caret keeps it on.
        let caret = egui::Rect::from_min_size(egui::pos2(40.0, 60.0), egui::vec2(8.0, 16.0));
        let mut output = egui::PlatformOutput::default();
        keep_ime_on(&mut output, Some(caret));
        assert_eq!(output.ime.map(|i| i.rect), Some(caret));
        // No caret known (scrolled back): IME still stays on.
        let mut output = egui::PlatformOutput::default();
        keep_ime_on(&mut output, None);
        assert!(output.ime.is_some());
        // A focused field's own IME area is left alone.
        let field = egui::Rect::from_min_size(egui::pos2(300.0, 20.0), egui::vec2(2.0, 18.0));
        let mut output = egui::PlatformOutput {
            ime: Some(egui::output::IMEOutput {
                rect: field,
                cursor_rect: field,
            }),
            ..Default::default()
        };
        keep_ime_on(&mut output, Some(caret));
        assert_eq!(output.ime.map(|i| i.rect), Some(field));
    }

    #[test]
    fn locations_show_home_as_a_tilde() {
        let home = Some("/h/me");
        assert_eq!(home_relative("/h/me", home), "~");
        assert_eq!(home_relative("/h/me/src/app", home), "~/src/app");
        assert_eq!(
            home_relative("/h/me2/x", home),
            "/h/me2/x",
            "not a prefix match"
        );
        assert_eq!(home_relative("/srv/x", home), "/srv/x");
        assert_eq!(home_relative("/srv/x", None), "/srv/x");
    }

    #[test]
    fn menu_shortcuts_that_are_editor_chords_reach_the_editor() {
        // The OS menu bar takes ⌘D, ⇧⌘Z and ⇧⌘L before the window sees them;
        // each must mean what the editor's keymap makes of the same chord.
        use mtty_ui::input::KeyKind;
        let mut mapped = 0;
        for (_, entries) in mtty_ui::menu::menus(mtty_ui::i18n::Lang::En) {
            for entry in entries {
                let mtty_ui::menu::Entry::Item { id, shortcut, .. } = entry else {
                    continue;
                };
                let Some(command) = editor_command_for_menu_key(id) else {
                    continue;
                };
                mapped += 1;
                let chord = shortcut.expect("a mapped item has a shortcut");
                let shift = chord.contains("Shift+");
                let key = chord.rsplit('+').next().unwrap().to_ascii_lowercase();
                let key = KeyKind::Char(key.chars().next().unwrap());
                assert_eq!(
                    editor_pane::keymap(key, shift, false, true, false),
                    if cfg!(target_os = "macos") {
                        Some(command)
                    } else {
                        editor_pane::keymap(key, shift, false, true, false)
                    },
                    "{chord}"
                );
            }
        }
        assert_eq!(mapped, 3);
    }

    #[test]
    fn a_focused_text_field_keeps_keys_from_the_editor() {
        // Typing in Find over an editor replaced the file's selection, and
        // ⌘A then Delete in the field emptied the file.
        assert!(editor_takes_keys(true, false, false));
        assert!(
            !editor_takes_keys(true, false, true),
            "Find/palette has the keyboard"
        );
        assert!(
            !editor_takes_keys(true, true, false),
            "hint mode reads labels"
        );
        assert!(!editor_takes_keys(false, false, false));
    }

    #[test]
    fn the_find_field_holds_the_keyboard_while_open() {
        // The editor's key routing relies on this: with Find open,
        // `wants_keyboard_input` is true, so keys stay out of the file.
        let ctx = egui::Context::default();
        let mut query = String::new();
        for _ in 0..3 {
            replay(&ctx, vec![egui::Event::Text("lev".into())], |ctx| {
                egui::Window::new("Find").show(ctx, |ui| {
                    ui.add(egui::TextEdit::singleline(&mut query))
                        .request_focus();
                });
            });
        }
        assert!(ctx.wants_keyboard_input());
        assert!(query.starts_with("lev"), "{query}");
        assert!(
            editor_takes_keys(true, false, false)
                && !editor_takes_keys(true, false, ctx.wants_keyboard_input())
        );
    }

    #[test]
    fn editor_search_starts_at_the_match_after_the_caret() {
        let hits = [(3, 6), (10, 13), (20, 23)];
        assert_eq!(first_hit_from(&hits, 0), 0);
        assert_eq!(first_hit_from(&hits, 10), 1, "a match at the caret counts");
        assert_eq!(first_hit_from(&hits, 11), 2);
        assert_eq!(first_hit_from(&hits, 30), 0, "wraps to the first");
        assert_eq!(first_hit_from(&[], 5), 0);
    }

    #[test]
    fn restored_panes_type_their_offer_on_enter_only() {
        // Typed for the local shell: " clear; …" for sh, "cls & …" for cmd.
        let typed = typed_ssh("ssh -t 'work'");
        assert!(typed.ends_with("ssh -t 'work'\r"), "{typed}");
        if cfg!(unix) {
            assert_eq!(typed, " clear; ssh -t 'work'\r");
        }
        let mut pending = Some(typed.clone());
        assert_eq!(
            on_enter_input(&mut pending, b"\r").as_deref(),
            Some(typed.as_bytes())
        );
        assert!(pending.is_none(), "the offer is made once");
        let mut pending = Some("vim notes.md\r".to_string());
        assert!(on_enter_input(&mut pending, b"l").is_none());
        assert!(pending.is_none(), "other input keeps the shell as it is");
        assert!(on_enter_input(&mut None, b"\r").is_none());
    }

    #[test]
    fn restored_ssh_tabs_wait_unless_auto_reconnect_is_configured() {
        let off = mtty_config::Config::from_toml("").unwrap();
        assert_eq!(
            restored_ssh_action(off.ssh_auto_reconnect),
            RestoredSsh::OnEnter
        );
        let on = mtty_config::Config::from_toml("ssh-auto-reconnect = true\n").unwrap();
        assert_eq!(
            restored_ssh_action(on.ssh_auto_reconnect),
            RestoredSsh::Auto
        );
    }

    #[test]
    fn saved_contents_stay_private_and_inside_their_folder() {
        assert!(is_plain_file_name("p1.ansi"));
        for name in ["", "../x.ansi", "/etc/passwd", "a/b.ansi", ".."] {
            assert!(!is_plain_file_name(name), "{name}");
        }
        let dir = std::env::temp_dir().join(format!("mtty-scrollback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_private(&dir, "p1.ansi", b"hello\r\n").unwrap();
        assert_eq!(std::fs::read(dir.join("p1.ansi")).unwrap(), b"hello\r\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&dir.join("p1.ansi")), 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recipe_names_stay_inside_the_recipes_directory() {
        assert_eq!(recipe_file_name(" work 工作 ").unwrap(), "work 工作.json");
        for bad in ["", "  ", "../x", "a/b", "a\\b", ".hidden", "c:x", "a\nb"] {
            assert!(recipe_file_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn close_others_and_below_return_what_they_removed() {
        let mut tabs = vec![
            empty_tab("a"),
            empty_tab("b"),
            empty_tab("c"),
            empty_tab("d"),
        ];
        let below = take_tabs_below(&mut tabs, 1);
        let names = |t: &[Tab]| t.iter().map(|t| t.title.clone()).collect::<Vec<_>>();
        assert_eq!(names(&below), ["c", "d"]);
        assert_eq!(names(&tabs), ["a", "b"]);
        assert!(take_tabs_below(&mut tabs, 1).is_empty());
        let others = take_other_tabs(&mut tabs, 1);
        assert_eq!(names(&others), ["a"]);
        assert_eq!(names(&tabs), ["b"]);
        assert!(take_other_tabs(&mut tabs, 5).is_empty());
    }

    #[test]
    fn new_tabs_inherit_only_a_real_local_directory() {
        let here = std::env::temp_dir();
        assert_eq!(inherited_cwd(false, Some(here.clone())), Some(here.clone()));
        assert_eq!(inherited_cwd(true, Some(here)), None, "ssh cwd is remote");
        let gone = std::env::temp_dir().join("mtty-no-such-dir-for-test");
        assert_eq!(inherited_cwd(false, Some(gone)), None);
        assert_eq!(inherited_cwd(false, None), None);
    }

    #[test]
    fn close_tab_removes_all_splits_and_preserves_focus() {
        let mut split = empty_tab("a");
        assert!(split.layout.split("a", "b", SplitDir::Right));
        let mut tabs = vec![split, empty_tab("c"), empty_tab("d")];
        let mut active = 2;
        assert!(remove_whole_tab(&mut tabs, &mut active, 0));
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[active].title, "d");
        assert_eq!(active, 1);
        assert!(!remove_whole_tab(&mut tabs, &mut active, 9));
        assert!(remove_whole_tab(&mut tabs, &mut active, 1));
        assert_eq!(active, 0);
        assert!(!remove_whole_tab(&mut tabs, &mut active, 0));
        assert_eq!(tabs[0].title, "c");
    }

    fn editor_at(path: std::path::PathBuf) -> Editor {
        Editor {
            path,
            text: "edited".into(),
            original: "original".into(),
            preview: false,
            readonly: false,
            remote: None,
            close_armed: false,
            saving: false,
            quit_after_save: false,
        }
    }

    #[test]
    fn failed_save_keeps_the_buffer_dirty() {
        let dir = std::env::temp_dir().join(format!("mtty-save-{}", std::process::id()));
        let mut ed = editor_at(dir.join("missing-dir").join("file.txt"));
        assert!(ed.write().is_err());
        assert_eq!(
            ed.original, "original",
            "a failed write must not look saved"
        );

        std::fs::create_dir_all(&dir).unwrap();
        let mut ed = editor_at(dir.join("file.txt"));
        ed.write().unwrap();
        assert_eq!(ed.original, "edited");
        assert_eq!(
            std::fs::read_to_string(dir.join("file.txt")).unwrap(),
            "edited"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remote_buffers_are_never_written_on_the_ui_thread() {
        let mut ed = editor_at(std::path::PathBuf::from("/etc/hosts"));
        ed.remote = Some(("host".into(), "/etc/hosts".into()));
        let err = ed.write().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
        assert_eq!(ed.original, "original");

        // A background save records what it wrote; later edits stay modified.
        ed.saving = true;
        let written = ed.text.clone();
        ed.text.push_str(" and more");
        ed.mark_saved(written);
        assert!(!ed.saving);
        assert_eq!(ed.original, "edited");
        assert_ne!(ed.text, ed.original);
    }

    #[test]
    fn git_status_outside_a_repository_is_not_clean() {
        assert_eq!(
            parse_git_status(false, ""),
            vec![("git".to_string(), "not a git repository".to_string())]
        );
        assert_eq!(
            parse_git_status(true, "## main\n"),
            vec![("branch".to_string(), "main".to_string())]
        );
        assert_eq!(
            parse_git_status(true, ""),
            vec![("status".to_string(), "clean".to_string())]
        );
    }

    #[test]
    fn ports_follow_the_shell_s_descendants() {
        let ps = "  1     0\n 10     1\n 11    10\n 12    11\n 20     1\n 13    10\n";
        let mut tree = process_tree(10, ps);
        tree.sort();
        assert_eq!(tree, vec![10, 11, 12, 13]);
        assert_eq!(process_tree(99, ps), vec![99]);
    }

    #[test]
    fn closing_unsaved_changes_needs_confirmation() {
        let mut ed = editor_at(std::path::PathBuf::from("unused"));
        assert!(!ed.may_close(), "first close keeps unsaved changes");
        assert!(ed.may_close(), "second close discards");

        let mut clean = editor_at(std::path::PathBuf::from("unused"));
        clean.original = clean.text.clone();
        assert!(clean.may_close());
    }

    #[test]
    fn find_columns_follow_wide_characters() {
        // "目录 abc 目录": 目(0,2) 录(2,2) ' '(4) a(5) b(6) c(7) ' '(8) 目(9,2) 录(11,2)
        let cells = vec![
            (0, '目', 2),
            (2, '录', 2),
            (4, ' ', 1),
            (5, 'a', 1),
            (6, 'B', 1),
            (7, 'c', 1),
            (8, ' ', 1),
            (9, '目', 2),
            (11, '录', 2),
        ];
        assert_eq!(find_in_cells(&cells, "abc"), vec![(5, 3)]);
        assert_eq!(find_in_cells(&cells, "目录"), vec![(0, 4), (9, 4)]);
        assert!(find_in_cells(&cells, "").is_empty());
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(super::windows_path_arg(r"C:\work\a.txt"), r"C:\work\a.txt");
        assert_eq!(
            super::windows_path_arg(r"C:\mttyacc\drop\a file 中文.txt"),
            r#""C:\mttyacc\drop\a file 中文.txt""#
        );
        assert_eq!(super::windows_path_arg(r"C:\a&b"), r#""C:\a&b""#);
        assert_eq!(super::shell_quote("/tmp/a.txt"), "/tmp/a.txt");
        assert_eq!(super::shell_quote("/a b/c"), "'/a b/c'");
        assert_eq!(super::shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn layout_json_round_trip() {
        let mut l = Layout::leaf("a");
        assert!(l.split("a", "b", SplitDir::Right));
        assert!(l.split("b", "c", SplitDir::Down));
        let map: std::collections::HashMap<String, String> = [("a", "x"), ("b", "y"), ("c", "z")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let back = json_to_layout(&layout_to_json(&l), &map).unwrap();
        let mut ids = back.ids();
        ids.sort();
        assert_eq!(ids, vec!["x".to_string(), "y".to_string(), "z".to_string()]);
    }

    #[test]
    fn sgr_mouse_reports() {
        assert_eq!(
            super::mouse_report(true, 0, true, false, 10, 5).unwrap(),
            "\x1b[<0;10;5M"
        );
        assert_eq!(
            super::mouse_report(true, 0, false, false, 10, 5).unwrap(),
            "\x1b[<0;10;5m"
        );
        // Drag: left button plus the motion bit.
        assert_eq!(
            super::mouse_report(true, 0, true, true, 10, 5).unwrap(),
            "\x1b[<32;10;5M"
        );
        // Hover without a button.
        assert_eq!(
            super::mouse_report(true, 3, false, true, 10, 5).unwrap(),
            "\x1b[<35;10;5M"
        );
        // Wheel events have no release.
        assert_eq!(
            super::mouse_report(true, 64, true, false, 10, 5).unwrap(),
            "\x1b[<64;10;5M"
        );
        assert_eq!(
            super::mouse_report(true, 65, false, false, 10, 5).unwrap(),
            "\x1b[<65;10;5M"
        );
    }

    #[test]
    fn x10_mouse_reports_and_bounds() {
        assert_eq!(
            super::mouse_report(false, 0, true, false, 1, 1).unwrap(),
            "\x1b[M\x20\x21\x21"
        );
        assert_eq!(
            super::mouse_report(false, 2, true, false, 5, 9).unwrap(),
            "\x1b[M\x22\x25\x29"
        );
        assert!(super::mouse_report(false, 0, true, false, 224, 1).is_none());
        assert!(super::mouse_report(false, 0, true, false, 1, 224).is_none());
        assert!(super::mouse_report(true, 0, true, false, 0, 1).is_none());
    }
}
