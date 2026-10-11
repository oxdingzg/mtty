//! Terminal: PTY + VT parser/screen + a reader thread.
//!
//! The screen model is `alacritty_terminal` (see [`crate::aterm`], ADR 0001),
//! hidden behind this type so the app never depends on the parser crate.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::aterm::ATerm;
use crate::hosted::Incoming;

type Fallible<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Nominal cell height (px) used only to advance the cursor below a Sixel image.
/// The host owns real font metrics; this keeps the following prompt below the
/// image without threading the cell size through the engine.
const NOMINAL_CELL_H: u32 = 16;

/// Maximum host metadata payload retained as a title or working directory.
const MAX_OSC: usize = 8 * 1024;

const PTY_READ_BYTES: usize = 8192;
// Backpressure bounds queued output to 256 KiB per pane, plus one reader chunk.
const PTY_QUEUE_CHUNKS: usize = 32;
// Yield between chunks so a continuously writing child cannot monopolize the UI.
const PTY_DRAIN_BYTES: usize = 64 * 1024;
const PTY_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(4);

/// A running terminal: a child shell on a PTY plus the parsed screen state.
pub struct Terminal {
    screen: ATerm,
    /// The PTY master, for a shell session; `None` for a plain byte pipe
    /// (serial, Telnet, raw TCP — ADR 0037).
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Box<dyn Write + Send>,
    /// The child shell, for a PTY session; `None` for a byte pipe.
    #[allow(dead_code)]
    child: Option<Box<dyn Child + Send + Sync>>,
    rx: Receiver<Incoming>,
    /// Stop an idle byte-pipe reader when its pane closes or replaces it.
    reader_stop: Option<std::sync::Arc<AtomicBool>>,
    /// The PTY host, for a pane whose program outlives the app (ADR 0041).
    host: Option<crate::hosted::HostLink>,
    waker: std::sync::Arc<dyn Fn() + Send + Sync>,
    rows: u16,
    cols: u16,
    exited: bool,
    cwd: Option<String>,
    cwd_checked: std::time::Instant,
    cwd_reported: bool,
    title: Option<String>,
    scanner: mtty_graphics::Scanner,
    graphics: crate::graphics::GraphicsLayer,
    graphics_enabled: bool,
    scrollback: usize,
    cell_px: (u16, u16),
    default_colors: Option<([u8; 3], [u8; 3])>,
    /// Output bytes of the running command, between OSC 133 `C` and `D`.
    capture: Option<Vec<u8>>,
    /// The last finished command's output (OSC 133 semantic prompts).
    last_output: Option<CommandOutput>,
}

/// A finished command's output, as plain text, with its exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub text: String,
    pub exit: Option<i32>,
    /// The output was longer than the capture limit and was cut.
    pub truncated: bool,
}

/// The most output kept for one command.
const MAX_CAPTURE: usize = 2 * 1024 * 1024;

/// Closing the PTY can block: on Windows `ClosePseudoConsole` waits for the
/// client process to exit, so a running shell would stall teardown for minutes
/// (closing a tab or quitting the app). Kill and reap the child *before* the
/// master is dropped.
impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(stop) = &self.reader_stop {
            stop.store(true, Ordering::Release);
        }
        // Teardown must not block: closing a ConPTY (`ClosePseudoConsole`) waits
        // for the client process to exit, which can stall for minutes on Windows
        // when a shell was running — and closing a tab or quitting the app must
        // never hang. Kill the tree, reap, drain, then close off-thread.
        // A signal handler or a failed kill must never turn pane teardown into
        // a blocking wait on the UI thread. Keep the master alive until the
        // child is reaped, especially for ConPTY.
        let child = self.child.take();
        let master = self.master.take();
        if child.is_some() || master.is_some() {
            std::thread::spawn(move || {
                if let Some(mut child) = child {
                    #[cfg(windows)]
                    if let Some(pid) = child.process_id() {
                        use std::os::windows::process::CommandExt;
                        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                        let _ = std::process::Command::new("taskkill")
                            .args(["/T", "/F", "/PID", &pid.to_string()])
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .creation_flags(CREATE_NO_WINDOW)
                            .status();
                    }
                    // portable-pty's graceful Unix kill starts with SIGHUP.
                    // Closing a pane is final: use the native forceful kill
                    // for its std child, including shells that trap SIGHUP.
                    #[cfg(unix)]
                    if let Some(native) =
                        (&mut *child as &mut dyn Child).downcast_mut::<std::process::Child>()
                    {
                        let _ = std::process::Child::kill(native);
                    } else {
                        let _ = child.kill();
                    }
                    #[cfg(not(unix))]
                    let _ = child.kill();
                    let _ = child.wait();
                }
                drop(master);
            });
        }
        // A hosted program ends with its pane, unless it was detached.
        if let Some(host) = &self.host {
            use mtty_ptyhost::proto::ToHost;
            host.send(if host.end_on_drop {
                ToHost::Kill
            } else {
                ToHost::Detach
            });
        }
        // Drain output the shell already produced (bounded).
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        while std::time::Instant::now() < deadline && self.rx.try_recv().is_ok() {}
    }
}

fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

impl Terminal {
    /// Spawn a shell on a new PTY.
    pub fn new(
        shell: Option<String>,
        cols: u16,
        rows: u16,
        scrollback: usize,
        cwd: Option<std::path::PathBuf>,
        extra_env: &[(String, String)],
        waker: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> Fallible<Self> {
        let (cmd, cwd) = shell_command(shell, cwd, extra_env);
        let (master, child, reader, writer) = spawn_local(cmd, cols, rows)?;
        Ok(Self::pipe(
            cols,
            rows,
            scrollback,
            reader,
            writer,
            waker,
            Some(master),
            Some(child),
            cwd,
        ))
    }

    /// Spawn a shell whose PTY lives in a host process (ADR 0041), so it
    /// outlives the app. Fails only when the host cannot be started at all;
    /// a host that never answers turns the pane into a local one.
    #[allow(clippy::too_many_arguments)]
    pub fn new_hosted(
        config: &crate::hosted::HostConfig,
        shell: Option<String>,
        cols: u16,
        rows: u16,
        scrollback: usize,
        cwd: Option<std::path::PathBuf>,
        extra_env: &[(String, String)],
        waker: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> Fallible<Self> {
        use mtty_ptyhost::launch;
        let (cmd, cwd) = shell_command(shell, cwd, extra_env);
        let dir = launch::hosts_dir()?;
        let id = launch::new_id()?;
        let socket = launch::socket_path(&dir, &id);
        let args = mtty_ptyhost::host::HostArgs {
            socket: socket.clone(),
            meta: Some(launch::meta_path(&dir, &id)),
            cols,
            rows,
            ring: config.ring,
            timeout: config.timeout,
            cwd: cmd.get_cwd().map(std::path::PathBuf::from),
            argv: cmd.get_argv().clone(),
        };
        launch::spawn(&config.binary, &args, cmd.iter_full_env_as_str())?;
        let (tx, rx) = mpsc::sync_channel::<Incoming>(PTY_QUEUE_CHUNKS);
        let mut host = crate::hosted::start(socket, id, None, 0, tx, waker.clone());
        host.fallback = Some(cmd);
        let writer = host.writer();
        let mut term = Self::assemble(
            ATerm::new(cols, rows, scrollback),
            scrollback,
            rx,
            writer,
            waker,
            None,
            None,
            cwd,
        );
        term.host = Some(host);
        Ok(term)
    }

    /// Attach to a host that kept running while the app was away. With a
    /// snapshot, the screen is rebuilt exactly as it was and only the output
    /// since is replayed; without one, everything the host kept is.
    pub fn reattach(
        id: &str,
        socket: &std::path::Path,
        snapshot: Option<crate::hosted::HostSnapshot>,
        cols: u16,
        rows: u16,
        scrollback: usize,
        waker: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> Fallible<Self> {
        if !mtty_ptyhost::launch::valid_id(id) {
            return Err("not a host id".into());
        }
        let (stream, child_pid) = crate::hosted::connect_existing(socket)?;
        let snapshot = snapshot.filter(|s| s.id == id);
        let (screen, from) = match &snapshot {
            Some(s) => (ATerm::restore_state(&s.state, scrollback), s.offset),
            None => (ATerm::new(cols, rows, scrollback), 0),
        };
        let (tx, rx) = mpsc::sync_channel::<Incoming>(PTY_QUEUE_CHUNKS);
        let mut host = crate::hosted::start(
            socket.to_path_buf(),
            id.to_string(),
            Some(stream),
            from,
            tx,
            waker.clone(),
        );
        host.set_child_pid(child_pid);
        host.boundary = snapshot.as_ref().map_or(true, |s| s.boundary);
        host.replaying = true;
        host.reattached = true;
        host.pending_resize = Some((rows, cols));
        let writer = host.writer();
        let mut term = Self::assemble(screen, scrollback, rx, writer, waker, None, None, None);
        term.title = snapshot.as_ref().and_then(|s| s.title.clone());
        term.host = Some(host);
        Ok(term)
    }
}

/// The shell to run in a pane, with mtty's environment and integration, and
/// the directory it starts in.
fn shell_command(
    shell: Option<String>,
    cwd: Option<std::path::PathBuf>,
    extra_env: &[(String, String)],
) -> (CommandBuilder, Option<String>) {
    let shell_path = shell.unwrap_or_else(default_shell);
    let mut cmd = CommandBuilder::new(&shell_path);
    cmd.args(crate::shell::startup_args(&shell_path));
    // CommandBuilder already inherits the environment and, on Windows,
    // refreshes machine/user variables from the registry. Reapplying the
    // host's startup snapshot here would undo updated PATH/proxy values.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    if let Some((k, v)) = default_colour_env(|name| cmd.get_env(name).is_some()) {
        cmd.env(k, v);
    }
    #[cfg(unix)]
    if ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .all(|name| std::env::var(name).map_or(true, |value| value.is_empty()))
    {
        // Finder and minimal launch environments omit the locale. Bash
        // otherwise interprets UTF-8 bytes as Meta keys (including Tab).
        cmd.env(
            "LC_CTYPE",
            if cfg!(target_os = "macos") {
                "UTF-8"
            } else {
                "C.UTF-8"
            },
        );
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    // The environment the shell will see, for values the shim restores.
    let seen = |name: &str| {
        extra_env
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .or_else(|| {
                cmd.get_env(name)
                    .map(|value| value.to_string_lossy().into_owned())
            })
    };
    let integration = crate::shell::integration(&shell_path, &seen);
    cmd.args(&integration.args);
    for (k, v) in integration.env {
        cmd.env(k, v);
    }
    let cwd = cwd.or_else(|| std::env::current_dir().ok());
    if let Some(dir) = &cwd {
        cmd.cwd(dir);
    }
    (cmd, cwd.map(|p| p.to_string_lossy().into_owned()))
}

/// Forward what `reader` yields to a channel, waking the UI each time so
/// output (the echo of a keystroke, say) is drawn promptly.
fn read_into_channel(
    mut reader: Box<dyn Read + Send>,
    waker: std::sync::Arc<dyn Fn() + Send + Sync>,
) -> (Receiver<Incoming>, std::sync::Arc<AtomicBool>) {
    let (tx, rx) = mpsc::sync_channel::<Incoming>(PTY_QUEUE_CHUNKS);
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let reader_stop = stop.clone();
    thread::spawn(move || {
        let mut buf = [0u8; PTY_READ_BYTES];
        while !reader_stop.load(Ordering::Acquire) {
            match reader.read(&mut buf) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    // Serial reads time out while the device is idle. Keep the
                    // connection alive and avoid spinning for nonblocking pipes.
                    thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(Incoming::Bytes(buf[..n].to_vec())).is_err() {
                        break;
                    }
                    waker();
                }
            }
        }
    });
    (rx, stop)
}

/// The process group that owns `pid`'s controlling terminal (`tpgid`).
#[cfg(unix)]
fn terminal_group(pid: u32) -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>();
        // proc_pidinfo writes this fixed-size structure only on success.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size as libc::c_int,
            )
        };
        if n as usize != size {
            return None;
        }
        let group = unsafe { info.assume_init() }.e_tpgid;
        (group != 0).then_some(group)
    }
    #[cfg(not(target_os = "macos"))]
    {
        // `pid (comm) state ppid pgrp session tty_nr tpgid …`; comm may hold
        // spaces and parentheses, so count from the last ')'.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let fields = &stat[stat.rfind(')')? + 1..];
        let group: i64 = fields.split_whitespace().nth(5)?.parse().ok()?;
        u32::try_from(group).ok().filter(|&g| g != 0)
    }
}

/// Hosted panes (ADR 0041).
impl Terminal {
    pub fn is_hosted(&self) -> bool {
        self.host.is_some()
    }

    /// The host's id and socket, to find it again after a restart.
    pub fn host_id(&self) -> Option<(&str, &std::path::Path)> {
        self.host
            .as_ref()
            .map(|h| (h.id.as_str(), h.socket.as_path()))
    }

    /// The screen as it stands and the output offset it covers, for
    /// reattaching later; `None` for a pane that is not hosted, or while a
    /// replay is still arriving.
    pub fn host_snapshot(&mut self, max_history: usize) -> Option<crate::hosted::HostSnapshot> {
        let host = self.host.as_ref().filter(|h| !h.replaying)?;
        let (id, socket, offset, boundary) = (
            host.id.clone(),
            host.socket.clone(),
            host.offset,
            host.boundary,
        );
        Some(crate::hosted::HostSnapshot {
            id,
            socket,
            offset,
            boundary,
            title: self.title.clone(),
            state: self.screen.snapshot_state(max_history),
        })
    }

    /// Leave the program running when this terminal goes away (the app is
    /// updating or quitting with its sessions kept).
    pub fn detach_host(&mut self) {
        if let Some(host) = self.host.as_mut() {
            host.end_on_drop = false;
            host.send(mtty_ptyhost::proto::ToHost::Detach);
        }
    }

    /// End the hosted program now (the app quits without keeping sessions;
    /// `process::exit` would skip `Drop`).
    pub fn end_host(&mut self) {
        if let Some(host) = &self.host {
            host.send(mtty_ptyhost::proto::ToHost::Kill);
        }
    }

    /// Output was lost while detached: take on the modes the host saw.
    fn apply_host_modes(&mut self, modes: &[u8]) {
        let Some(modes) = mtty_ptyhost::scanner::Modes::decode(modes) else {
            return;
        };
        if modes.alt_screen() != self.screen.alternate_screen() {
            let switch: &[u8] = if modes.alt_screen() {
                b"\x1b[?1049h"
            } else {
                b"\x1b[?1049l"
            };
            self.screen.process(switch);
        }
        self.screen.process(modes.sequences().as_bytes());
        // Nothing here was asked by the program.
        self.screen.take_responses(0, 0);
    }

    /// The replay is in: replies go out again, the screen takes the pane's
    /// size, and a reattached program is made to redraw (`SIGWINCH`), which
    /// also restores what no snapshot holds (its scroll region, say).
    fn host_live(&mut self) {
        let Some(host) = self.host.as_mut() else {
            return;
        };
        host.replaying = false;
        let size = host.pending_resize.take();
        let reattached = std::mem::take(&mut host.reattached);
        if let Some((rows, cols)) = size {
            self.resize(rows, cols);
        }
        if reattached {
            if let Some(host) = &self.host {
                use mtty_ptyhost::proto::ToHost;
                let (rows, cols) = (self.rows, self.cols);
                for rows in [rows.saturating_sub(1).max(1), rows] {
                    host.send(ToHost::Resize {
                        cols,
                        rows,
                        px_w: 0,
                        px_h: 0,
                    });
                }
            }
        }
    }

    /// The host never answered. Before any output, run the shell in the app
    /// instead, so a pane is never lost; after, the pane has ended.
    fn host_failed(&mut self, reason: &str) -> bool {
        let fallback = self.host.as_mut().and_then(|h| h.fallback.take());
        let Some(cmd) = fallback else {
            self.exited = true;
            return true;
        };
        match spawn_local(cmd, self.cols, self.rows) {
            Ok((master, child, reader, writer)) => {
                if let Some(host) = &self.host {
                    host.send(mtty_ptyhost::proto::ToHost::Kill);
                }
                self.host = None;
                if let Some(stop) = self.reader_stop.take() {
                    stop.store(true, Ordering::Release);
                }
                let (rx, stop) = read_into_channel(reader, self.waker.clone());
                self.rx = rx;
                self.reader_stop = Some(stop);
                self.writer = writer;
                self.master = Some(master);
                self.child = Some(child);
                let note = format!(
                    "\x1b[2m[mtty] The PTY host did not start ({reason}); this pane runs inside mtty.\x1b[0m\r\n"
                );
                self.screen.process(note.as_bytes());
            }
            Err(_) => self.exited = true,
        }
        true
    }
}

type LocalPty = (
    Box<dyn MasterPty + Send>,
    Box<dyn Child + Send + Sync>,
    Box<dyn Read + Send>,
    Box<dyn Write + Send>,
);

/// Run `cmd` on a new PTY in this process.
fn spawn_local(cmd: CommandBuilder, cols: u16, rows: u16) -> Fallible<LocalPty> {
    let pair = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    Ok((pair.master, child, reader, writer))
}

impl Terminal {
    /// Build a terminal over a byte stream: a reader thread forwards chunks
    /// into `rx` and wakes the UI, exactly as for a PTY. `master`/`child` are
    /// `None` for serial, Telnet and raw TCP sessions (ADR 0037).
    // A private constructor: the arguments are the terminal's own fields.
    #[allow(clippy::too_many_arguments)]
    fn pipe(
        cols: u16,
        rows: u16,
        scrollback: usize,
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        waker: std::sync::Arc<dyn Fn() + Send + Sync>,
        master: Option<Box<dyn MasterPty + Send>>,
        child: Option<Box<dyn Child + Send + Sync>>,
        cwd: Option<String>,
    ) -> Self {
        let (rx, stop) = read_into_channel(reader, waker.clone());
        let mut term = Self::assemble(
            ATerm::new(cols, rows, scrollback),
            scrollback,
            rx,
            writer,
            waker,
            master,
            child,
            cwd,
        );
        term.reader_stop = Some(stop);
        term
    }

    // A private constructor: the arguments are the terminal's own fields.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        screen: ATerm,
        scrollback: usize,
        rx: Receiver<Incoming>,
        writer: Box<dyn Write + Send>,
        waker: std::sync::Arc<dyn Fn() + Send + Sync>,
        master: Option<Box<dyn MasterPty + Send>>,
        child: Option<Box<dyn Child + Send + Sync>>,
        cwd: Option<String>,
    ) -> Self {
        let (rows, cols) = screen.size();
        Self {
            screen,
            master,
            writer,
            child,
            rx,
            reader_stop: None,
            host: None,
            waker,
            rows,
            cols,
            exited: false,
            cwd,
            cwd_checked: std::time::Instant::now(),
            cwd_reported: false,
            title: None,
            scanner: mtty_graphics::Scanner::new(),
            graphics: crate::graphics::GraphicsLayer::new(),
            graphics_enabled: true,
            scrollback,
            capture: None,
            last_output: None,
            cell_px: (0, 0),
            default_colors: None,
        }
    }

    /// Run a terminal over a plain byte pipe with no child process behind it:
    /// a serial port, a Telnet connection or a raw TCP socket (ADR 0037).
    /// `exited()` becomes true when the pipe closes.
    pub fn from_pipe(
        cols: u16,
        rows: u16,
        scrollback: usize,
        reader: impl Read + Send + 'static,
        writer: Box<dyn Write + Send>,
        waker: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self::pipe(
            cols,
            rows,
            scrollback,
            Box::new(reader),
            writer,
            waker,
            None,
            None,
            None,
        )
    }

    /// Drain pending PTY output into the screen. Returns true if anything changed.
    pub fn process_pending(&mut self) -> bool {
        let mut changed = self
            .screen
            .expire_synchronized_output(std::time::Instant::now());
        if changed {
            self.send_screen_responses();
        }
        // Shell hooks may be absent or replaced by user startup files. Query
        // the local shell itself at a bounded cadence, without spawning tools.
        if !self.cwd_reported && self.cwd_checked.elapsed() >= std::time::Duration::from_millis(500)
        {
            self.cwd_checked = std::time::Instant::now();
            if let Some(path) = self.pid().and_then(process_cwd) {
                if self.cwd.as_deref() != Some(path.as_str()) {
                    self.cwd = Some(path);
                    changed = true;
                }
            }
        }
        let deadline = std::time::Instant::now() + PTY_DRAIN_BUDGET;
        let mut drained = 0;
        loop {
            match self.rx.try_recv() {
                Ok(incoming) => {
                    let bytes = match incoming {
                        Incoming::Bytes(bytes) => bytes,
                        Incoming::Hosted {
                            bytes,
                            end,
                            boundary,
                        } => {
                            changed |= self.feed(&bytes);
                            if let Some(host) = self.host.as_mut() {
                                host.offset = end;
                                host.boundary = boundary;
                                host.fallback = None;
                            }
                            drained += bytes.len();
                            Vec::new()
                        }
                        Incoming::Truncated(modes) => {
                            self.apply_host_modes(&modes);
                            changed = true;
                            Vec::new()
                        }
                        Incoming::Live => {
                            self.host_live();
                            changed = true;
                            Vec::new()
                        }
                        Incoming::HostFailed(reason) => {
                            changed |= self.host_failed(&reason);
                            // The channel may have been replaced.
                            break;
                        }
                    };
                    drained += bytes.len();
                    if !bytes.is_empty() {
                        changed |= self.feed(&bytes);
                    }
                    if drained >= PTY_DRAIN_BYTES || std::time::Instant::now() >= deadline {
                        // A queued continuation is essential: the reader can be
                        // blocked on a full queue and unable to send a new wake.
                        (self.waker)();
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.exited = true;
                    break;
                }
            }
        }
        self.graphics.sync_total(self.screen.total_lines());
        changed
    }

    /// Wake even if an application starts a synchronized update then stalls.
    pub fn output_deadline(&self) -> Option<std::time::Instant> {
        self.screen.synchronized_output_deadline()
    }

    /// Feed one chunk through the shared control scanner: borrowed text to VT,
    /// ordered metadata/DSR notifications and graphics to their host handlers.
    fn feed(&mut self, bytes: &[u8]) -> bool {
        let mut changed = false;
        let mut line_feeds = 0usize;
        // Move the scanner out temporarily so callbacks can update the screen,
        // metadata, images, and PTY writer without an intermediate segment Vec.
        let mut scanner = std::mem::take(&mut self.scanner);
        scanner.feed_with(bytes, |event| {
            match event {
                mtty_graphics::StreamEvent::Text(text) => {
                    if !text.is_empty() {
                        // Image-anchor accounting has no purpose in text-only
                        // panes. Avoid an additional full-byte scan in that case.
                        if !self.graphics.images.is_empty() {
                            line_feeds += count_line_feeds(text);
                        }
                        self.screen.process(text);
                        self.send_screen_responses();
                        if let Some(buf) = self.capture.as_mut() {
                            let room = MAX_CAPTURE.saturating_sub(buf.len());
                            buf.extend_from_slice(&text[..text.len().min(room + 1)]);
                        }
                        changed = true;
                    }
                }
                mtty_graphics::StreamEvent::Graphics(graphic) => {
                    if self.graphics_enabled {
                        changed |= self.handle_graphic(graphic);
                    }
                }
                mtty_graphics::StreamEvent::Osc(payload) => self.observe_osc(payload),
                mtty_graphics::StreamEvent::CursorReport => {
                    // Respond at the query's position in the stream, after any
                    // preceding text has updated the cursor (including ConPTY).
                    self.screen.flush_synchronized_output();
                    self.send_screen_responses();
                    let (row, col) = self.screen.cursor();
                    self.reply(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
                }
            }
        });
        self.scanner = scanner;
        // Keep image anchors aligned with content. Up to the ring cap this is
        // exact (the buffer grows); past it the grid rotates without growing, so
        // approximate with the line feeds seen in this chunk.
        let total = self.screen.total_lines();
        let cap = self.scrollback + self.rows as usize;
        if total < cap {
            self.graphics.sync_total(total);
        } else {
            if line_feeds > 0 {
                self.graphics.shift(line_feeds);
            }
            self.graphics.sync_total(total);
        }
        changed
    }

    fn send_screen_responses(&mut self) {
        for response in self.screen.take_responses(self.cell_px.0, self.cell_px.1) {
            self.reply(response.as_bytes());
        }
    }

    /// Answer a query in the output. Output replayed on reattaching was
    /// answered when it first ran: answering again would type the reply into
    /// the program.
    fn reply(&mut self, bytes: &[u8]) {
        if self.host.as_ref().is_some_and(|h| h.replaying) {
            return;
        }
        self.write(bytes);
    }

    fn handle_graphic(&mut self, g: mtty_graphics::Graphic) -> bool {
        use mtty_graphics as gfx;
        // Align existing anchors before adding to the layer.
        self.graphics.sync_total(self.screen.total_lines());
        let (line, col) = {
            let (r, c) = self.screen.cursor();
            (r as i32, c)
        };
        match g {
            gfx::Graphic::Sixel { transparent, data } => {
                let Some(img) = gfx::sixel::decode(&data, transparent, self.graphics.max_pixels)
                else {
                    return false;
                };
                let rows = img.height.div_ceil(self.cell_h()).max(1);
                self.graphics.place(img, line, col, None, None, 0, 0, 0);
                for _ in 0..rows {
                    self.screen.process(b"\r\n");
                }
                true
            }
            gfx::Graphic::Kitty(cmd) => {
                let move_now = cmd.move_cursor && matches!(cmd.action, 'T' | 'p');
                let rows_hint = cmd.rows;
                let before = self.graphics.images.last().map(|image| image.id);
                let view_offset = self.screen.scroll_offset() as i32;
                // `a=p` places at an explicit cell (viewport row y, column x).
                let (anchor, place_col) = match (cmd.action, cmd.cell_x, cmd.cell_y) {
                    ('p', Some(x), Some(y)) => (y as i32 - view_offset, x),
                    _ => (line, col),
                };
                let changed = self.graphics.kitty(cmd, anchor, place_col, view_offset);
                if move_now
                    && self
                        .graphics
                        .images
                        .last()
                        .is_some_and(|image| Some(image.id) != before)
                {
                    let rows = rows_hint
                        .map(u32::from)
                        .or_else(|| {
                            self.graphics
                                .images
                                .last()
                                .map(|i| i.image.height.div_ceil(self.cell_h()))
                        })
                        .unwrap_or(1)
                        .max(1);
                    for _ in 0..rows {
                        self.screen.process(b"\r\n");
                    }
                }
                changed
            }
            gfx::Graphic::Iterm2 { data, .. } => {
                let Some(img) = gfx::decode_iterm2(&data, self.graphics.max_pixels) else {
                    return false;
                };
                self.graphics.place(img, line, col, None, None, 0, 0, 0);
                true
            }
        }
    }

    /// Enable/disable inline graphics processing (config `graphics`).
    pub fn set_graphics_enabled(&mut self, on: bool) {
        self.graphics_enabled = on;
    }

    /// Report the cell size in physical pixels (used for `TIOCSWINSZ` so image
    /// tools size themselves to the grid, and to advance below Sixel images).
    pub fn set_cell_size(&mut self, w: u16, h: u16) {
        self.cell_px = (w, h);
    }

    /// Colors actually used by the host renderer, for OSC 10/11 queries.
    /// Adaptive TUIs use these to choose input and message backgrounds.
    pub fn set_default_colors(&mut self, foreground: [u8; 3], background: [u8; 3]) {
        self.default_colors = Some((foreground, background));
    }

    fn cell_h(&self) -> u32 {
        let h = self.cell_px.1 as u32;
        if h == 0 {
            NOMINAL_CELL_H
        } else {
            h
        }
    }

    /// Feed bytes as if they came from the PTY (tests only).
    #[cfg(test)]
    pub(crate) fn feed_for_test(&mut self, bytes: &[u8]) -> bool {
        self.feed(bytes)
    }

    /// Decoded inline images (for the renderer).
    pub fn graphics(&self) -> &crate::graphics::GraphicsLayer {
        &self.graphics
    }

    /// The graphics layer, for restoring saved placements.
    pub fn graphics_mut(&mut self) -> &mut crate::graphics::GraphicsLayer {
        &mut self.graphics
    }

    /// Snapshot the main screen for session restore, with the inline images
    /// anchored inside the captured window. `None` for the images when there is
    /// no text to anchor them to. See [`crate::graphics::GraphicsLayer::save_images`].
    pub fn snapshot_scrollback(
        &mut self,
        max_lines: usize,
    ) -> (String, Option<crate::graphics::SavedImages>) {
        let (text, last) = self.screen_mut().snapshot_scrollback(max_lines);
        let (Some(last), false) = (last, text.is_empty()) else {
            return (text, None);
        };
        let first = last.saturating_sub(max_lines.saturating_sub(1));
        let history = self.screen().history_size() as i32;
        let cols = self.screen().size().1;
        let saved = self
            .graphics
            .save_images(history, first as i32, last as i32, cols);
        let images = (!saved.images.is_empty()).then_some(saved);
        (text, images)
    }

    /// Send raw bytes (keyboard/paste) to the shell.
    pub fn write(&mut self, bytes: &[u8]) {
        if self.writer.write_all(bytes).is_ok() {
            let _ = self.writer.flush();
        }
    }

    /// Resize both the screen model and the PTY if the size changed.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows == 0 || cols == 0 {
            return;
        }
        // While replaying, the screen keeps the size the output was made for.
        if let Some(host) = self.host.as_mut().filter(|h| h.replaying) {
            host.pending_resize = Some((rows, cols));
            return;
        }
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.screen.resize(cols, rows);
        let (pw, ph) = (
            cols.saturating_mul(self.cell_px.0),
            rows.saturating_mul(self.cell_px.1),
        );
        let _ = self.master.as_ref().map(|m| {
            m.resize(PtySize {
                rows,
                cols,
                pixel_width: pw,
                pixel_height: ph,
            })
        });
        if let Some(host) = &self.host {
            host.send(mtty_ptyhost::proto::ToHost::Resize {
                cols,
                rows,
                px_w: pw,
                px_h: ph,
            });
        }
    }

    pub fn screen(&self) -> &ATerm {
        &self.screen
    }

    pub fn screen_mut(&mut self) -> &mut ATerm {
        &mut self.screen
    }

    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    pub fn exited(&self) -> bool {
        self.exited
    }

    /// The working directory reported by the shell via OSC 7, if any.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// The child process id, if the platform exposes one.
    pub fn pid(&self) -> Option<u32> {
        if let Some(host) = &self.host {
            return host.child_pid();
        }
        self.child.as_ref().and_then(|c| c.process_id())
    }

    /// The foreground process group's leader, unless it is the shell.
    #[cfg(unix)]
    fn foreground_leader(&self) -> Option<u32> {
        let leader = match &self.master {
            Some(master) => u32::try_from(master.process_group_leader()?).ok()?,
            // A hosted PTY's master is in the host: ask the kernel which
            // group owns the shell's terminal.
            None => terminal_group(self.pid()?)?,
        };
        (Some(leader) != self.pid()).then_some(leader)
    }

    /// The program running in the foreground of this pane (`vim`, `cargo`),
    /// or `None` while the shell itself is waiting for input. Unix only.
    pub fn foreground_command(&self) -> Option<String> {
        #[cfg(unix)]
        {
            let leader = self.foreground_leader()?;
            process_name(leader)
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// The foreground program's arguments (`["vim", "notes.md"]`), to offer
    /// running it again after a restart. `None` while the shell itself waits
    /// for input. Unix only.
    pub fn foreground_args(&self) -> Option<Vec<String>> {
        #[cfg(unix)]
        {
            let leader = self.foreground_leader()?;
            process_args(leader).filter(|args| !args.is_empty())
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// The file the foreground program was started on: its first argument that
    /// is not an option and names an existing file, resolved against the
    /// program's working directory (`src/main.rs` for `vim src/main.rs`).
    /// `None` while the shell itself waits for input.
    pub fn foreground_file(&self) -> Option<String> {
        #[cfg(unix)]
        {
            let leader = self.foreground_leader()?;
            let args = process_args(leader)?;
            file_argument(args.get(1..)?, process_cwd(leader).as_deref())
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// The last finished command's output, when the shell marks commands
    /// with OSC 133 (the mtty zsh integration does).
    pub fn last_command_output(&self) -> Option<&CommandOutput> {
        self.last_output.as_ref()
    }

    /// OSC 133: `C` starts a command's output, `D[;exit]` ends it.
    fn observe_semantic_prompt(&mut self, payload: &[u8]) {
        match payload.first() {
            Some(b'C') => self.capture = Some(Vec::new()),
            Some(b'D') => {
                let Some(buf) = self.capture.take() else {
                    return;
                };
                let exit = std::str::from_utf8(payload)
                    .ok()
                    .and_then(|p| p.split(';').nth(1))
                    .and_then(|code| code.trim().parse().ok());
                let truncated = buf.len() > MAX_CAPTURE;
                let end = buf.len().min(MAX_CAPTURE);
                self.last_output = Some(CommandOutput {
                    text: plain_text(&buf[..end]),
                    exit,
                    truncated,
                });
            }
            _ => {}
        }
    }

    /// The window title reported via OSC 0/2, if any.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Observe a complete OSC from the shared stream scanner, without copying
    /// raw PTY chunks into a second protocol buffer.
    fn observe_osc(&mut self, bytes: &[u8]) {
        if bytes.len() > MAX_OSC {
            return;
        }
        let Some(semi) = bytes.iter().position(|&b| b == b';') else {
            return;
        };
        let code = &bytes[..semi];
        if matches!(code, b"10" | b"11") {
            if let Some((fg, bg)) = self.default_colors {
                // OSC 10 can query consecutive slots (10;?;?). Reply to
                // queries only; changing the host's theme is not supported.
                let start = if code == b"10" { 10 } else { 11 };
                for (slot, value) in (start..=11).zip(bytes[semi + 1..].split(|&b| b == b';')) {
                    if value == b"?" {
                        let [r, g, b] = if slot == 10 { fg } else { bg };
                        self.reply(
                            format!(
                                "\x1b]{slot};rgb:{:04x}/{:04x}/{:04x}\x1b\\",
                                u16::from(r) * 257,
                                u16::from(g) * 257,
                                u16::from(b) * 257
                            )
                            .as_bytes(),
                        );
                    }
                }
            }
            return;
        }
        if code == b"133" {
            self.observe_semantic_prompt(&bytes[semi + 1..]);
            return;
        }
        if !matches!(code, b"0" | b"2" | b"7") {
            return;
        }
        let Ok(payload) = std::str::from_utf8(&bytes[semi + 1..]) else {
            return;
        };
        match code {
            b"7" => {
                if let Some(path) = parse_osc7(payload) {
                    self.cwd = Some(path);
                    self.cwd_reported = true;
                }
            }
            b"0" | b"2" => self.title = Some(payload.to_string()),
            _ => {}
        }
    }
}

/// Terminal output as plain text: escape sequences removed, `\r\n` as
/// newlines, a bare `\r` overwriting its line (progress bars), backspace
/// erasing, trailing blank space trimmed.
pub fn plain_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameters, then a final byte in @..~.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC / DCS / APC / PM / SOS: until BEL or ST (ESC \).
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                // Two-byte escapes (ESC 7, ESC ( B, …): drop the next char.
                Some('(' | ')' | '*' | '+') => {
                    chars.next();
                }
                _ => {}
            },
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    continue;
                }
                line.clear();
            }
            '\n' => lines.push(std::mem::take(&mut line)),
            '\x08' => {
                line.pop();
            }
            c if c.is_control() && c != '\t' => {}
            c => line.push(c),
        }
    }
    lines.push(line);
    let joined: Vec<String> = lines
        .into_iter()
        .map(|l| l.trim_end().to_string())
        .collect();
    joined.join("\n").trim_end().to_string()
}

/// Colour on by default for tools that ask for it (BSD/macOS `ls` with
/// `CLICOLOR`), unless the user chose otherwise: an existing `CLICOLOR`, or
/// `NO_COLOR` (no-color.org). Tools still colour only when writing to a
/// terminal, so pipes and redirected output stay plain.
fn default_colour_env(is_set: impl Fn(&str) -> bool) -> Option<(&'static str, &'static str)> {
    (!is_set("CLICOLOR") && !is_set("NO_COLOR")).then_some(("CLICOLOR", "1"))
}

/// A process's short name (`comm`).
#[cfg(unix)]
fn process_name(pid: u32) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut buf = [0u8; 256];
        // proc_name writes at most `buf.len()` bytes and returns the length.
        let n = unsafe {
            libc::proc_name(
                pid as libc::c_int,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
            )
        };
        if n <= 0 {
            return None;
        }
        String::from_utf8(buf[..n as usize].to_vec()).ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

/// The first of `args` that is not an option and is an existing file, as an
/// absolute path (relative ones resolved against `cwd`).
#[cfg(unix)]
fn file_argument(args: &[String], cwd: Option<&str>) -> Option<String> {
    args.iter()
        .filter(|a| !a.is_empty() && !a.starts_with('-') && !a.starts_with('+'))
        .find_map(|a| {
            let path = std::path::Path::new(a);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::path::Path::new(cwd?).join(path)
            };
            path.is_file().then(|| path.to_string_lossy().into_owned())
        })
}

/// A process's command line (`argv`), program name first.
#[cfg(unix)]
fn process_args(pid: u32) -> Option<Vec<String>> {
    #[cfg(target_os = "macos")]
    {
        // KERN_PROCARGS2: argc (a C int), the executable path, NUL padding,
        // then argc NUL-terminated arguments, then the environment.
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
        let mut size: libc::size_t = 0;
        // First call sizes the buffer; the second fills it.
        let ok = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if ok != 0 || size < 4 {
            return None;
        }
        let mut buf = vec![0u8; size];
        let ok = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if ok != 0 {
            return None;
        }
        buf.truncate(size);
        parse_procargs2(&buf)
    }
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let args: Vec<String> = raw
            .split(|&b| b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        (!args.is_empty()).then_some(args)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

/// Split a `KERN_PROCARGS2` buffer into its arguments.
#[cfg(any(target_os = "macos", test))]
fn parse_procargs2(buf: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
    let argc = usize::try_from(argc).ok()?;
    let rest = &buf[4..];
    // Skip the executable path and the NULs that pad it.
    let exec_end = rest.iter().position(|&b| b == 0)?;
    let start = exec_end + rest[exec_end..].iter().position(|&b| b != 0)?;
    let args: Vec<String> = rest[start..]
        .split(|&b| b == 0)
        .take(argc)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    (args.len() == argc).then_some(args)
}

fn process_cwd(pid: u32) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>();
        // proc_pidinfo writes this fixed-size structure only on success.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                info.as_mut_ptr().cast(),
                size as libc::c_int,
            )
        };
        if n as usize != size {
            return None;
        }
        let info = unsafe { info.assume_init() };
        let bytes: Vec<u8> = info
            .pvi_cdir
            .vip_path
            .iter()
            .flatten()
            .map(|&b| b as u8)
            .take_while(|&b| b != 0)
            .collect();
        String::from_utf8(bytes).ok().filter(|s| !s.is_empty())
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

/// Count line-advancing controls in a chunk (`\n`, IND `ESC D`, NEL `ESC E`).
fn count_line_feeds(bytes: &[u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                n += 1;
                i += 1;
            }
            0x1b if i + 1 < bytes.len() && (bytes[i + 1] == b'D' || bytes[i + 1] == b'E') => {
                n += 1;
                i += 2;
            }
            _ => i += 1,
        }
    }
    n
}

fn parse_osc7(payload: &str) -> Option<String> {
    let rest = payload.strip_prefix("file://")?;
    let slash = rest.find('/')?;
    let bytes = &rest.as_bytes()[slash..];
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded)
        .ok()
        .filter(|s| !s.contains('\0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_keeps_reading_after_idle_and_interrupted_reads() {
        struct IntermittentReader(usize);
        impl Read for IntermittentReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0 += 1;
                match self.0 {
                    1 => Err(std::io::ErrorKind::Interrupted.into()),
                    2 => Err(std::io::ErrorKind::TimedOut.into()),
                    3 => Err(std::io::ErrorKind::WouldBlock.into()),
                    4 => {
                        buf[..5].copy_from_slice(b"ready");
                        Ok(5)
                    }
                    _ => Ok(0),
                }
            }
        }
        let (rx, _stop) =
            read_into_channel(Box::new(IntermittentReader(0)), std::sync::Arc::new(|| {}));
        match rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap() {
            Incoming::Bytes(bytes) => assert_eq!(bytes, b"ready"),
            _ => panic!("expected pipe bytes"),
        }
        assert!(matches!(
            rx.recv_timeout(std::time::Duration::from_secs(2)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn closing_idle_pipe_releases_reader_without_waking_ui() {
        struct IdleReader(mpsc::Sender<()>);
        impl Read for IdleReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::TimedOut.into())
            }
        }
        impl Drop for IdleReader {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = wakes.clone();
        let term = Terminal::from_pipe(
            10,
            5,
            10,
            IdleReader(dropped_tx),
            Box::new(std::io::sink()),
            std::sync::Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
            }),
        );
        thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
        drop(term);
        dropped_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("idle reader closes with its pane");
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn colour_is_on_by_default_unless_the_user_chose() {
        let set = |names: &'static [&'static str]| move |n: &str| names.contains(&n);
        assert_eq!(default_colour_env(set(&[])), Some(("CLICOLOR", "1")));
        assert_eq!(default_colour_env(set(&["CLICOLOR"])), None, "keep theirs");
        assert_eq!(default_colour_env(set(&["NO_COLOR"])), None, "no-color.org");
    }

    fn make() -> Terminal {
        let waker: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(|| {});
        Terminal::new(None, 20, 5, 100, None, &[], waker).expect("spawn shell")
    }

    /// A writer that records what the terminal sends over the pipe.
    #[derive(Clone, Default)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn tui_queries_reply_without_leaking_into_the_input_row() {
        let stream = b"\x1b[5n\x1b[c\x1b[18t\x1b[14t\x1b[?2026$p\x1b[?2026h> n\x1b[6n\x1b[?2026l\x1b[1;3H\x1b[KTry a prompt";
        for chunk_size in 1..=stream.len() {
            let replies = SharedBuf::default();
            let mut term = Terminal::from_pipe(
                20,
                5,
                100,
                std::io::Cursor::new(Vec::<u8>::new()),
                Box::new(replies.clone()),
                std::sync::Arc::new(|| {}),
            );
            term.set_cell_size(8, 16);
            for chunk in stream.chunks(chunk_size) {
                term.feed(chunk);
            }
            assert_eq!(
                &*replies.0.lock().unwrap(),
                b"\x1b[0n\x1b[?6c\x1b[8;5;20t\x1b[4;80;160t\x1b[?2026;2$y\x1b[1;4R",
                "chunk size {chunk_size}"
            );
            assert_eq!(term.screen.line_text(0), "> Try a prompt");
            // A full clear and redraw must also remove a previous first char.
            term.feed(b"\r\x1b[2K> a\x08 \x08");
            assert_eq!(term.screen.line_text(0), ">");
        }
    }

    #[test]
    fn default_color_queries_reply_with_host_colors_across_chunks() {
        let stream = b"before\x1b]10;?\x07\x1b]11;?\x1b\\after";
        for chunk_size in [1, 2, 3, 8, stream.len()] {
            let replies = SharedBuf::default();
            let mut term = Terminal::from_pipe(
                20,
                5,
                100,
                std::io::Cursor::new(Vec::<u8>::new()),
                Box::new(replies.clone()),
                std::sync::Arc::new(|| {}),
            );
            term.set_default_colors([0xd8, 0xde, 0xe9], [0x2e, 0x34, 0x40]);
            for chunk in stream.chunks(chunk_size) {
                term.feed(chunk);
            }
            assert_eq!(
                &*replies.0.lock().unwrap(),
                b"\x1b]10;rgb:d8d8/dede/e9e9\x1b\\\x1b]11;rgb:2e2e/3434/4040\x1b\\"
            );
            assert_eq!(term.screen.line_text(0), "beforeafter");
            replies.0.lock().unwrap().clear();
            term.set_default_colors([255, 255, 255], [0, 0, 0]);
            term.feed(b"\x1b]10;?;?\x07");
            assert_eq!(
                &*replies.0.lock().unwrap(),
                b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\"
            );
            replies.0.lock().unwrap().clear();
            term.feed(b"\x1b]11;rgb:ffff/ffff/ffff\x07\x1b]12;?\x07");
            assert!(replies.0.lock().unwrap().is_empty());
        }
    }

    /// A terminal over a plain byte pipe has no child, consumes what the pipe
    /// sends and reports EOF as exit (ADR 0037).
    #[test]
    fn a_pipe_terminal_reads_and_writes_without_a_child() {
        let waker: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(|| {});
        let out = SharedBuf::default();
        let writer: Box<dyn Write + Send> = Box::new(out.clone());
        let reader = std::io::Cursor::new(b"\x1b[31mhi\x1b[0m\r\n".to_vec());
        let mut term = Terminal::from_pipe(20, 5, 100, reader, writer, waker);
        assert_eq!(term.pid(), None);
        assert_eq!(term.foreground_command(), None);
        for _ in 0..200 {
            term.process_pending();
            if term.exited() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(term.exited(), "a closed pipe ends the session");
        assert_eq!(term.screen().line_text(0).trim_end(), "hi");
        term.write(b"ping");
        assert_eq!(&*out.0.lock().unwrap(), b"ping");
        term.resize(6, 30);
    }

    #[test]
    #[ignore = "release full input-path benchmark"]
    fn full_input_path_throughput() {
        let mut term = make();
        let line = b"\x1b[31mhello\x1b[0m world \x1b[1;32mfoo\x1b[0m bar 1234567890\r\n";
        let mut data = Vec::with_capacity(16 << 20);
        while data.len() < 16 << 20 {
            data.extend_from_slice(line);
        }
        for chunk in data[..64 * 1024].chunks(8192) {
            term.feed(chunk);
        }
        let start = std::time::Instant::now();
        for chunk in data.chunks(8192) {
            term.feed(chunk);
        }
        let mbps = data.len() as f64 / 1e6 / start.elapsed().as_secs_f64();
        println!("full input path (8 KiB chunks): {mbps:.2} MB/s");
        let scale = std::env::var("MTTY_PERF_SCALE")
            .or_else(|_| std::env::var("MIAOTTY_PERF_SCALE"))
            .ok()
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(1.0);
        assert!(mbps > 15.0 / scale, "full input-path throughput regression");
    }

    #[test]
    fn output_queue_backpressure_and_drain_continuation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let mut term = make();
        let wakes = Arc::new(AtomicUsize::new(0));
        let notified = Arc::clone(&wakes);
        term.waker = Arc::new(move || {
            notified.fetch_add(1, Ordering::Relaxed);
        });
        let (tx, rx) = mpsc::sync_channel(PTY_QUEUE_CHUNKS);
        term.rx = rx;
        for _ in 0..PTY_QUEUE_CHUNKS {
            tx.try_send(Incoming::Bytes(vec![b'x'; PTY_READ_BYTES]))
                .unwrap();
        }
        assert!(matches!(
            tx.try_send(Incoming::Bytes(b"\r\ndrain complete".to_vec())),
            Err(mpsc::TrySendError::Full(_))
        ));
        assert!(term.process_pending());
        assert!(
            wakes.load(Ordering::Relaxed) > 0,
            "yield must schedule continuation"
        );
        assert!(!term.exited());
        tx.try_send(Incoming::Bytes(b"\r\ndrain complete".to_vec()))
            .unwrap();
        drop(tx);
        // Even with no further producer wakes, the queued continuation drains
        // all output in order and eventually observes EOF.
        for _ in 0..PTY_QUEUE_CHUNKS + 2 {
            term.process_pending();
            if term.exited() {
                break;
            }
        }
        assert!(term.exited());
        assert!(term
            .screen()
            .contents_between(0, 0, 4, 19)
            .contains("drain complete"));
    }

    #[test]
    fn unified_stream_updates_metadata_and_replies_at_the_query_position() {
        use std::sync::{Arc, Mutex};
        struct Replies(Arc<Mutex<Vec<u8>>>);
        impl Write for Replies {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let stream = b"\x1b[?2026habc\x1b[6n\x1b[?2026l\r\n\x1b[?2026hxy\x1b[6n\x1b[?2026l\x1b]2;new title\x1b\\\x1b]7;file://localhost/tmp/a%20b\x07done";
        for chunk_size in [1, 2, 3, 4, 8, stream.len()] {
            let mut term = make();
            let replies = Arc::new(Mutex::new(Vec::new()));
            term.writer = Box::new(Replies(Arc::clone(&replies)));
            for chunk in stream.chunks(chunk_size) {
                term.feed(chunk);
            }
            assert_eq!(&*replies.lock().unwrap(), b"\x1b[1;4R\x1b[2;3R");
            assert_eq!(term.title(), Some("new title"));
            assert_eq!(term.cwd(), Some("/tmp/a b"));
            assert_eq!(term.screen.line_text(0), "abc");
            assert_eq!(term.screen.line_text(1), "xydone");
        }
    }

    #[test]
    fn osc7_decodes_directory_uri() {
        assert_eq!(
            parse_osc7("file://localhost/tmp/a%20b/%E7%9B%AE%E5%BD%95"),
            Some("/tmp/a b/目录".into())
        );
        assert!(parse_osc7("file://localhost/tmp/%00").is_none());
        assert!(parse_osc7("file://localhost/tmp/%ZZ").is_none());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn shell_directory_tracks_cd_without_osc_hooks() {
        let root = std::env::temp_dir().join(format!("mtty-cwd-{}", std::process::id()));
        let next = root.join("space 目录");
        std::fs::create_dir_all(&next).unwrap();
        let mut term = Terminal::new(
            Some("/bin/sh".into()),
            80,
            24,
            100,
            Some(root.clone()),
            &[],
            std::sync::Arc::new(|| {}),
        )
        .unwrap();
        assert_eq!(term.cwd(), Some(root.to_string_lossy().as_ref()));
        term.write("cd 'space 目录'\r".as_bytes());
        let expected = std::fs::canonicalize(&next)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while term.cwd() != Some(expected.as_str()) && std::time::Instant::now() < deadline {
            term.process_pending();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            term.cwd(),
            Some(expected.as_str()),
            "directory panels must follow a plain shell's cd; screen: {}",
            term.screen().contents_between(0, 0, 23, 79)
        );
        drop(term);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn foreground_command_names_the_running_program() {
        let mut term = Terminal::new(
            Some("/bin/sh".into()),
            80,
            24,
            100,
            None,
            &[],
            std::sync::Arc::new(|| {}),
        )
        .unwrap();
        let wait_for = |term: &mut Terminal, want: Option<&str>| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                term.process_pending();
                let got = term.foreground_command();
                if got.as_deref() == want || std::time::Instant::now() > deadline {
                    return got;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        assert_eq!(
            wait_for(&mut term, None),
            None,
            "an idle shell has no command"
        );
        term.write(b"sleep 3\r");
        assert_eq!(wait_for(&mut term, Some("sleep")).as_deref(), Some("sleep"));
    }

    #[cfg(unix)]
    #[test]
    fn file_argument_skips_options_and_missing_paths() {
        let dir = std::env::temp_dir().join(format!("mtty-fg-file-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        let cwd = dir.to_string_lossy().to_string();
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let want = dir.join("src/main.rs").to_string_lossy().to_string();
        assert_eq!(
            file_argument(
                &args(&["-R", "+12", "missing.rs", "src/main.rs"]),
                Some(&cwd)
            ),
            Some(want.clone())
        );
        assert_eq!(file_argument(&args(&[&want]), None), Some(want));
        assert_eq!(
            file_argument(&args(&["src"]), Some(&cwd)),
            None,
            "a directory"
        );
        assert_eq!(file_argument(&args(&["src/main.rs"]), None), None, "no cwd");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn procargs2_buffers_split_into_arguments() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/usr/bin/vim\0\0\0\0vim\0notes.md\0HOME=/x\0");
        assert_eq!(
            parse_procargs2(&buf),
            Some(vec!["vim".to_string(), "notes.md".to_string()])
        );
        assert_eq!(parse_procargs2(&buf[..3]), None);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn foreground_file_names_the_file_the_program_opened() {
        let dir = std::env::temp_dir().join(format!("mtty-fg-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.md");
        std::fs::write(&file, "x\n").unwrap();
        let mut term = Terminal::new(
            Some("/bin/sh".into()),
            80,
            24,
            100,
            Some(dir.clone()),
            &[],
            std::sync::Arc::new(|| {}),
        )
        .unwrap();
        assert_eq!(term.foreground_file(), None, "an idle shell has no file");
        term.write(b"tail -f notes.md\r");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut got = None;
        while got.is_none() && std::time::Instant::now() < deadline {
            term.process_pending();
            got = term.foreground_file();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let want = std::fs::canonicalize(&file).unwrap();
        let got = got.map(|g| std::fs::canonicalize(g).unwrap());
        assert_eq!(got, Some(want));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn closing_a_shell_that_ignores_hup_is_prompt_and_reaps_it() {
        let mut term = Terminal::new(
            Some("/bin/sh".into()),
            80,
            24,
            100,
            None,
            &[],
            std::sync::Arc::new(|| {}),
        )
        .unwrap();
        let pid = term.child.as_ref().unwrap().process_id().unwrap();
        term.write(b"trap '' HUP; printf '%s%s\\n' close- ready\r");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let ready = |t: &Terminal| (0..24).any(|r| t.screen().line_text(r).contains("close-ready"));
        while !ready(&term) && std::time::Instant::now() < deadline {
            term.process_pending();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ready(&term), "shell installed its SIGHUP handler");
        let started = std::time::Instant::now();
        drop(term);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "pane teardown waited on its child"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success();
            if !alive {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "shell was not reaped");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Run `shell` through its integration in an empty HOME (the user's own
    /// startup files stay out) and check command output capture, exit codes
    /// and the cwd it reports after `cd`.
    fn integration_end_to_end(shell: &str, ok_then_fail: &str, fail_code: i32) {
        let home = std::env::temp_dir().join(format!(
            "mtty-shell-home-{}-{}",
            shell.rsplit(['/', '\\']).next().unwrap(),
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("a dir")).unwrap();
        let home_s = home.to_string_lossy().to_string();
        let env = [
            ("HOME".to_string(), home_s.clone()),
            ("USERPROFILE".to_string(), home_s.clone()),
            ("XDG_CONFIG_HOME".to_string(), format!("{home_s}/.config")),
        ];
        let mut term = Terminal::new(
            Some(shell.into()),
            100,
            24,
            100,
            Some(home.clone()),
            &env,
            std::sync::Arc::new(|| {}),
        )
        .unwrap();
        let wait = |term: &mut Terminal, done: &dyn Fn(&Terminal) -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while !done(term) && std::time::Instant::now() < deadline {
                term.process_pending();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        // Line editors such as PSReadLine start after the prompt is drawn and
        // drop a typed-ahead Enter, so after the prompt give them a moment,
        // as a person would.
        // `cwd()` starts as the spawn directory, so it cannot tell that the
        // shell is up. Nothing has been typed yet, so anything on screen is
        // the shell's own (banner or prompt): wait for that, as long as a cold
        // PowerShell start on a CI runner needs.
        let started = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while (0..24).all(|r| term.screen().line_text(r).trim().is_empty())
            && std::time::Instant::now() < started
        {
            term.process_pending();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            (0..24).any(|r| !term.screen().line_text(r).trim().is_empty()),
            "{shell}: no prompt within 60 s"
        );
        let settle = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        while std::time::Instant::now() < settle {
            term.process_pending();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        term.write(ok_then_fail.as_bytes());
        std::thread::sleep(std::time::Duration::from_millis(200));
        term.process_pending();
        // A slow machine may still be loading the line editor: if the Enter
        // was dropped (no output after a while), press it again. An extra
        // Enter on an empty line runs nothing.
        for _ in 0..3 {
            term.write(b"\r");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
            while term.last_command_output().is_none() && std::time::Instant::now() < deadline {
                term.process_pending();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if term.last_command_output().is_some() {
                break;
            }
        }
        let out = term.last_command_output().cloned().unwrap_or_else(|| {
            let screen: Vec<String> = (0..24).map(|r| term.screen().line_text(r)).collect();
            panic!(
                "{shell}: no OSC 133 C/D; screen:\n{}",
                screen.join("\n").trim_end()
            )
        });
        assert_eq!(out.text, "first\nsecond", "{shell}");
        assert_eq!(out.exit, Some(fail_code), "{shell}");
        term.write("echo 中文\r".as_bytes());
        wait(&mut term, &|t| {
            t.last_command_output().is_some_and(|o| o.exit == Some(0))
        });
        assert_eq!(term.last_command_output().unwrap().text, "中文", "{shell}");
        term.write(b"cd 'a dir'\r");
        wait(&mut term, &|t| {
            t.cwd().is_some_and(|c| c.ends_with("a dir"))
        });
        let cwd = term.cwd().map(str::to_string);
        assert!(
            cwd.as_deref().is_some_and(|c| c.ends_with("a dir")),
            "{shell}: cwd {cwd:?}"
        );
        drop(term);
        let _ = std::fs::remove_dir_all(&home);
        eprintln!("shell integration checked: {shell}");
    }

    /// The shells installed here (CI images differ); missing ones are skipped.
    fn installed(candidates: &[&str]) -> Vec<String> {
        let mut found: Vec<String> = candidates
            .iter()
            .filter(|p| std::path::Path::new(p).exists())
            .map(|p| p.to_string())
            .collect();
        found.dedup();
        found
    }

    #[cfg(unix)]
    #[test]
    fn zsh_integration_marks_commands_end_to_end() {
        for zsh in installed(&["/bin/zsh", "/usr/bin/zsh"]).into_iter().take(1) {
            integration_end_to_end(&zsh, "printf 'first\\nsecond\\n'; false", 1);
        }
    }

    /// bash 3.2 (macOS, DEBUG trap) and 4.4+ (PS0) both.
    #[cfg(unix)]
    #[test]
    fn bash_integration_marks_commands_end_to_end() {
        for bash in installed(&["/bin/bash", "/opt/homebrew/bin/bash", "/usr/local/bin/bash"]) {
            integration_end_to_end(&bash, "printf 'first\\nsecond\\n'; (exit 3)", 3);
        }
    }

    #[cfg(unix)]
    #[test]
    fn fish_integration_marks_commands_end_to_end() {
        let fish = installed(&[
            "/usr/bin/fish",
            "/opt/homebrew/bin/fish",
            "/usr/local/bin/fish",
        ]);
        for fish in fish.into_iter().take(1) {
            integration_end_to_end(&fish, "printf 'first\\nsecond\\n'; false", 1);
        }
    }

    #[test]
    fn powershell_integration_marks_commands_end_to_end() {
        let pwsh = installed(&[
            "/usr/bin/pwsh",
            "/usr/local/bin/pwsh",
            "/opt/homebrew/bin/pwsh",
            "/snap/bin/pwsh",
            r"C:\Program Files\PowerShell\7\pwsh.exe",
        ]);
        let pwsh = pwsh.into_iter().next().or_else(|| {
            std::env::var("MTTY_TEST_PWSH")
                .ok()
                .filter(|p| std::path::Path::new(p).exists())
        });
        if let Some(pwsh) = pwsh {
            let fail = if cfg!(windows) {
                "'first'; 'second'; cmd /c exit 4"
            } else {
                "'first'; 'second'; sh -c 'exit 4'"
            };
            integration_end_to_end(&pwsh, fail, 4);
        }
    }

    #[test]
    fn plain_text_drops_escapes_and_keeps_the_last_overwrite() {
        let raw =
            b"\x1b[31mred\x1b[0m line\r\nprogress 10%\rprogress 100%\r\nab\x08c\x1b]0;title\x07\n";
        assert_eq!(plain_text(raw), "red line\nprogress 100%\nac");
        assert_eq!(plain_text("中文 输出\r\n".as_bytes()), "中文 输出");
    }

    #[test]
    fn semantic_prompts_capture_the_last_command_output() {
        let mut t = make();
        t.feed_for_test(b"$ \x1b]133;C\x07hello\r\n\x1b[1mworld\x1b[0m\r\n\x1b]133;D;1\x07$ ");
        let out = t.last_command_output().unwrap();
        assert_eq!(out.text, "hello\nworld");
        assert_eq!(out.exit, Some(1));
        assert!(!out.truncated);
        // A D without a C (the first prompt) changes nothing.
        t.feed_for_test(b"\x1b]133;D;0\x07");
        assert_eq!(t.last_command_output().unwrap().exit, Some(1));
        // The ST terminator works too, and a missing exit code is None.
        t.feed_for_test(b"\x1b]133;C\x1b\\ok\r\n\x1b]133;D\x1b\\");
        let out = t.last_command_output().unwrap();
        assert_eq!((out.text.as_str(), out.exit), ("ok", None));
    }

    #[test]
    fn sixel_reaches_the_graphics_layer() {
        let mut t = make();
        t.feed_for_test(b"\x1bPq#0;2;100;0;0#0~~~~~~~~~~\x1b\\");
        let imgs = &t.graphics().images;
        assert_eq!(imgs.len(), 1, "one image placed");
        assert_eq!((imgs[0].image.width, imgs[0].image.height), (10, 6));
        // Red at the top-left pixel.
        assert_eq!(&imgs[0].image.rgba[..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn snapshot_scrollback_anchors_images_to_the_captured_text() {
        let mut t = make();
        t.feed_for_test(b"one\r\ntwo\r\n");
        t.feed_for_test(b"\x1bPq#0;2;100;0;0#0~~~~~~~~~~\x1b\\");
        t.feed_for_test(b"after\r\n");
        let (text, images) = t.snapshot_scrollback(100);
        assert!(text.contains("one") && text.contains("after"));
        let saved = images.expect("the sixel is saved");
        assert_eq!(saved.images.len(), 1);
        assert_eq!(saved.cols, t.screen().size().1);
        // Sixel sits on the row the cursor was on, then advances one row before
        // "after": one row above the captured window's last row.
        assert_eq!(saved.images[0].delta, -1);
    }

    #[test]
    fn kitty_rgba_reaches_the_graphics_layer() {
        let mut t = make();
        // 2x2 RGBA (f=32) transmit+display with an explicit 8x8 cell footprint.
        t.feed_for_test(b"\x1b_Ga=T,f=32,s=2x2,i=1,c=8,r=8;/wAA//8AAP//AAD//wAA/w==\x1b\\");
        let imgs = &t.graphics().images;
        assert_eq!(imgs.len(), 1, "one image placed");
        assert_eq!((imgs[0].image.width, imgs[0].image.height), (2, 2));
        assert_eq!(&imgs[0].image.rgba[..4], &[255, 0, 0, 255]);
        assert_eq!((imgs[0].cols, imgs[0].rows), (Some(8), Some(8)));
    }

    #[test]
    fn kitty_cursor_advance_survives_old_image_eviction() {
        let mut term = make();
        term.graphics.max_image_bytes = 4;
        for id in [1, 2] {
            term.feed_for_test(format!("\x1b_Ga=T,f=24,s=1x1,C=0,i={id};AAAA\x1b\\").as_bytes());
            assert_eq!(term.graphics.images.len(), 1);
            assert_eq!(term.screen.cursor().0, id as u16);
        }
    }

    #[test]
    fn kitty_put_uses_cell_coordinates() {
        let mut t = make();
        // a=p at x=5,y=2 with a 1x1 raw RGB pixel (base64 "AAAA").
        t.feed_for_test(b"\x1b_Ga=p,f=24,s=1x1,x=5,y=2;AAAA\x1b\\");
        let imgs = &t.graphics().images;
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].col, 5);
        assert_eq!(imgs[0].anchor, 2);
    }

    #[test]
    fn cell_height_drives_sixel_cursor_advance() {
        let mut t = make(); // 5-row screen
        t.set_cell_size(8, 16);
        // One 6px band => ceil(6/16) = 1 row below.
        t.feed_for_test(b"\x1bPq#0;2;100;0;0#0@\x1b\\");
        assert_eq!(t.screen().cursor().0, 1, "cursor moved one row down");
        // Same image with a 6px cell => still one row.
        let mut t2 = make();
        t2.set_cell_size(8, 6);
        t2.feed_for_test(b"\x1bPq#0;2;100;0;0#0@\x1b\\");
        assert_eq!(t2.screen().cursor().0, 1);
    }

    #[test]
    fn graphics_can_be_disabled() {
        let mut t = make();
        t.set_graphics_enabled(false);
        t.feed_for_test(b"\x1bPq#0;2;100;0;0#0~~~~~~~~~~\x1b\\");
        assert!(t.graphics().images.is_empty());
    }

    #[test]
    fn plain_text_still_flows() {
        let mut t = make();
        t.feed_for_test(b"hi\r\n");
        assert_eq!(t.screen().line_text(0), "hi");
        assert!(t.graphics().images.is_empty());
    }
}
