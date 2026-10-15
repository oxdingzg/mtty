//! `mtty-config` — configuration and theming.
//!
//! A small TOML config (`~/.config/mtty/config.toml`) with sensible defaults
//! matching mtty's look (Nord), plus ghostty/alacritty import.

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub mod hosts;
pub mod snippets;
pub mod sync;
pub mod view;

/// The application's directory name under the XDG bases (ADR 0032).
pub const APP_DIR: &str = "mtty";
/// The former name, read for compatibility only (ADR 0032).
pub const LEGACY_APP_DIR: &str = "miaotty";

/// The user's home directory: `$HOME`, or on Windows (which has no `HOME`
/// unless a Unix-like shell sets one) `%USERPROFILE%`.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| {
            cfg!(windows)
                .then(|| std::env::var_os("USERPROFILE"))
                .flatten()
                .filter(|h| !h.is_empty())
        })
        .map(PathBuf::from)
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// The directory that holds `mtty/` (and the pre-rename `miaotty/`).
///
/// An explicit `$XDG_*_HOME` wins. On Windows the default is the platform
/// folder (`%APPDATA%` for config, `%LOCALAPPDATA%` for data): Windows has no
/// `HOME` unless a Unix-like shell sets one, and without it mtty had no config
/// directory at all (config.toml, hosts, snippets and sessions were ignored).
/// A `$HOME/.config/mtty` that already exists keeps being used, so a setup
/// made under Git Bash is not lost. Elsewhere: `$HOME/<home_rel>`.
fn base_dir(
    xdg: Option<PathBuf>,
    windows_folder: Option<PathBuf>,
    home: Option<PathBuf>,
    home_rel: &str,
    is_windows: bool,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if xdg.is_some() {
        return xdg;
    }
    let home_based = home.map(|h| h.join(home_rel));
    if is_windows {
        let keep_home = home_based
            .as_ref()
            .is_some_and(|h| exists(&h.join(APP_DIR)) || exists(&h.join(LEGACY_APP_DIR)));
        if !keep_home {
            if let Some(folder) = windows_folder {
                return Some(folder);
            }
        }
    }
    home_based
}

fn xdg_base(var: &str, home_rel: &str, windows_var: &str) -> Option<PathBuf> {
    base_dir(
        env_path(var),
        env_path(windows_var),
        std::env::var_os("HOME").map(PathBuf::from),
        home_rel,
        cfg!(windows),
        &|p: &Path| p.is_dir(),
    )
}

/// mtty's config and saved state: `$XDG_CONFIG_HOME/mtty`, by default
/// `~/.config/mtty`, or `%APPDATA%\mtty` on Windows.
pub fn config_dir() -> Option<PathBuf> {
    Some(xdg_base("XDG_CONFIG_HOME", ".config", "APPDATA")?.join(APP_DIR))
}

/// The pre-rename config directory, `$XDG_CONFIG_HOME/miaotty`.
pub fn legacy_config_dir() -> Option<PathBuf> {
    Some(xdg_base("XDG_CONFIG_HOME", ".config", "APPDATA")?.join(LEGACY_APP_DIR))
}

/// `$XDG_DATA_HOME/mtty` (default `~/.local/share/mtty`, or
/// `%LOCALAPPDATA%\mtty` on Windows).
pub fn data_dir() -> Option<PathBuf> {
    Some(xdg_base("XDG_DATA_HOME", ".local/share", "LOCALAPPDATA")?.join(APP_DIR))
}

/// The pre-rename data directory (the retired eframe app kept its session here).
pub fn legacy_data_dir() -> Option<PathBuf> {
    Some(xdg_base("XDG_DATA_HOME", ".local/share", "LOCALAPPDATA")?.join(LEGACY_APP_DIR))
}

/// An environment setting by its unprefixed name: `MTTY_<name>`, falling back
/// to the pre-rename `MIAOTTY_<name>` (ADR 0032). Empty values count as unset.
pub fn env(name: &str) -> Option<String> {
    ["MTTY_", "MIAOTTY_"]
        .iter()
        .filter_map(|prefix| std::env::var(format!("{prefix}{name}")).ok())
        .find(|v| !v.is_empty())
}

/// Copy the pre-rename config directory to the new one, once. Returns true when
/// a copy happened. Never overwrites: nothing is done when the new directory
/// already exists. The old directory is kept, so older builds still work.
pub fn migrate_legacy_config() -> std::io::Result<bool> {
    match (legacy_config_dir(), config_dir()) {
        (Some(from), Some(to)) => migrate_dir(&from, &to),
        _ => Ok(false),
    }
}

/// Copy `from` to `to` when `to` does not exist yet. The copy is assembled in a
/// sibling staging directory and renamed into place, so an interrupted copy
/// never leaves a half-filled `to` that would block the next attempt. The
/// runtime `inbox` is not copied.
pub fn migrate_dir(from: &Path, to: &Path) -> std::io::Result<bool> {
    if to.exists() || !from.is_dir() {
        return Ok(false);
    }
    let name = to.file_name().map(|n| n.to_string_lossy().to_string());
    let staging = to.with_file_name(format!(".{}.migrating", name.unwrap_or_default()));
    let _ = std::fs::remove_dir_all(&staging);
    copy_tree(from, &staging, true)?;
    std::fs::rename(&staging, to)?;
    Ok(true)
}

fn copy_tree(from: &Path, to: &Path, top: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let name = entry.file_name();
        if top && name == "inbox" {
            continue;
        }
        let target = to.join(&name);
        if kind.is_dir() {
            copy_tree(&entry.path(), &target, false)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// An RGB color parsed from `#rrggbb`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn parse(s: &str) -> Option<Self> {
        let h = s.trim().trim_start_matches('#');
        if h.len() != 6 {
            return None;
        }
        let n = u32::from_str_radix(h, 16).ok()?;
        Some(Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
    }
}

fn default_font_size() -> f32 {
    13.0
}

/// How long a PTY host waits for mtty to come back (ADR 0041).
pub const DEFAULT_DETACHED_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(24 * 60 * 60);

/// A duration as `90s`, `30m`, `24h`, `7d` or plain seconds; `None` for
/// anything else, including zero.
pub fn parse_duration(text: &str) -> Option<std::time::Duration> {
    let text = text.trim();
    let (number, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => text.split_at(i),
        None => (text, "s"),
    };
    let n: u64 = number.parse().ok()?;
    let secs = match unit.trim() {
        "s" => n,
        "m" => n.checked_mul(60)?,
        "h" => n.checked_mul(3600)?,
        "d" => n.checked_mul(86_400)?,
        _ => return None,
    };
    (secs > 0).then(|| std::time::Duration::from_secs(secs))
}

/// Default terminal font, bundled with the app (see `assets/fonts/`).
fn default_font_family() -> Option<String> {
    Some("JetBrains Mono".to_string())
}

/// Theme colors.
#[derive(Debug, Clone)]
pub struct Theme {
    pub background: Rgb,
    pub foreground: Rgb,
    pub palette: [Rgb; 16],
}

impl Default for Theme {
    fn default() -> Self {
        // Nord
        let p = [
            "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
            "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4",
        ];
        let mut palette = [Rgb(0, 0, 0); 16];
        for (i, hex) in p.iter().enumerate() {
            palette[i] = Rgb::parse(hex).unwrap();
        }
        Self {
            background: Rgb::parse("#2e3440").unwrap(),
            foreground: Rgb::parse("#d8dee9").unwrap(),
            palette,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawColors {
    background: Option<String>,
    foreground: Option<String>,
    palette: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RawBadges {
    processing: Option<bool>,
    idle: Option<bool>,
    awaiting: Option<bool>,
    error: Option<bool>,
}

/// How the text cursor is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorStyle {
    Block,
    Bar,
    Underline,
}

impl CursorStyle {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "block" => Self::Block,
            "bar" | "beam" => Self::Bar,
            "underline" => Self::Underline,
            _ => return None,
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(rename = "font-size")]
    font_size: Option<f32>,
    #[serde(rename = "font-family")]
    font_family: Option<String>,
    #[serde(rename = "line-height")]
    line_height: Option<f32>,
    #[serde(rename = "cursor-style")]
    cursor_style: Option<String>,
    #[serde(rename = "background-opacity")]
    background_opacity: Option<f32>,
    notifications: Option<bool>,
    #[serde(rename = "prevent-sleep")]
    prevent_sleep: Option<bool>,
    #[serde(rename = "restore-scrollback")]
    restore_scrollback: Option<bool>,
    #[serde(rename = "ssh-auto-reconnect")]
    ssh_auto_reconnect: Option<bool>,
    #[serde(rename = "pty-host")]
    pty_host: Option<bool>,
    #[serde(rename = "keep-sessions-on-quit")]
    keep_sessions_on_quit: Option<bool>,
    #[serde(rename = "detached-timeout")]
    detached_timeout: Option<toml::Value>,
    badges: Option<RawBadges>,
    #[serde(rename = "agent-quota-warn")]
    agent_quota_warn: Option<toml::Value>,
    language: Option<String>,
    #[serde(rename = "update-check-url")]
    update_check_url: Option<String>,
    #[serde(rename = "update-auto-check")]
    update_auto_check: Option<bool>,
    editor: Option<String>,
    #[serde(rename = "editor-vim")]
    editor_vim: Option<bool>,
    #[serde(rename = "mermaid-command")]
    mermaid_command: Option<String>,
    graphics: Option<bool>,
    #[serde(rename = "remote-listen")]
    remote_listen: Option<String>,
    #[serde(rename = "quick-terminal-hotkey")]
    quick_terminal_hotkey: Option<String>,
    #[serde(rename = "update-pubkey")]
    update_pubkey: Option<String>,
    #[serde(rename = "sync-dir")]
    sync_dir: Option<String>,
    theme: Option<String>,
    colors: Option<RawColors>,
    /// `[lsp]`: `enabled`, and a table per server (`[lsp.rust]`).
    lsp: Option<toml::Table>,
    /// `[acp]`: `[[acp.agent]]` entries (ADR 0040, A2).
    acp: Option<toml::Table>,
}

fn theme_from(bg: &str, fg: &str, palette: &[&str; 16]) -> Theme {
    let mut p = [Rgb(0, 0, 0); 16];
    for (i, hex) in palette.iter().enumerate() {
        p[i] = Rgb::parse(hex).unwrap_or(Rgb(0, 0, 0));
    }
    Theme {
        background: Rgb::parse(bg).unwrap_or(Theme::default().background),
        foreground: Rgb::parse(fg).unwrap_or(Theme::default().foreground),
        palette: p,
    }
}

/// A built-in named theme.
pub fn theme_by_name(name: &str) -> Option<Theme> {
    let t = match name.to_ascii_lowercase().as_str() {
        "nord" => Theme::default(),
        "dracula" => theme_from(
            "#282a36",
            "#f8f8f2",
            &[
                "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd",
                "#f8f8f2", "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df",
                "#a4ffff", "#ffffff",
            ],
        ),
        "gruvbox" | "gruvbox-dark" => theme_from(
            "#282828",
            "#ebdbb2",
            &[
                "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a",
                "#a89984", "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b",
                "#8ec07c", "#ebdbb2",
            ],
        ),
        "solarized" | "solarized-dark" => theme_from(
            "#002b36",
            "#839496",
            &[
                "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198",
                "#eee8d5", "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4",
                "#93a1a1", "#fdf6e3",
            ],
        ),
        "tokyo-night" | "tokyonight" => theme_from(
            "#1a1b26",
            "#c0caf5",
            &[
                "#15161e", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff",
                "#a9b1d6", "#414868", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7",
                "#7dcfff", "#c0caf5",
            ],
        ),
        "mtty" => theme_from(
            "#212733",
            "#d7dee8",
            &[
                "#1b2029", "#ef6b6b", "#4ec26a", "#e5b567", "#4f8cff", "#c98bdb", "#56b6c2",
                "#d7dee8", "#5c6a7d", "#ff8a8a", "#6ee08a", "#f5cf8a", "#7aa7ff", "#e0a8ef",
                "#7fd8e2", "#eef3f9",
            ],
        ),
        _ => return None,
    };
    Some(t)
}

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub font_size: f32,
    pub font_family: Option<String>,
    /// Line height as a multiple of the font size.
    pub line_height: f32,
    pub cursor_style: CursorStyle,
    /// Window/terminal background opacity, 0.0–1.0.
    pub background_opacity: f32,
    /// Post a system notification when an agent needs attention.
    pub notifications: bool,
    /// Keep the machine awake while an agent is processing.
    pub prevent_sleep: bool,
    /// Save each terminal's contents when mtty quits and show them again
    /// in the restored panes (kept in the data directory, owner-only).
    pub restore_scrollback: bool,
    /// Connect a restored SSH tab on startup instead of waiting for Enter
    /// (`ssh-auto-reconnect`). Off by default, so nothing connects unasked.
    pub ssh_auto_reconnect: bool,
    /// Run each local shell in its own PTY host process, so it keeps
    /// running while mtty restarts and is reattached (ADR 0041): an update
    /// does not end what runs in the panes. On by default.
    pub pty_host: bool,
    /// An ordinary quit leaves hosted programs running for the next launch
    /// (tmux-like) instead of ending them.
    pub keep_sessions_on_quit: bool,
    /// How long a PTY host keeps its program with no mtty attached.
    pub detached_timeout: std::time::Duration,
    /// Which agent states show a tab badge.
    pub badges: Badges,
    /// Percentage of an agent's reported quota at which the Agent tab marks it.
    pub agent_quota_warn: u8,
    /// UI language tag (`en`, `zh`, …); `None` detects from `$LANG`.
    pub language: Option<String>,
    /// Optional URL checked for a newer version (see ADR 0013).
    pub update_check_url: Option<String>,
    /// Check for a newer version once on startup (ADR 0022). On by default.
    pub update_auto_check: bool,
    /// External editor command used by "Edit in Tab" (see ADR 0017).
    pub editor: Option<String>,
    /// Enable a minimal vim mode in the built-in editor (ADR 0029).
    pub editor_vim: bool,
    /// External Mermaid renderer (e.g. `mmdc`) for `mermaid` code blocks.
    /// `None` (default) draws the built-in `graph`/`flowchart` subset instead.
    pub mermaid_command: Option<String>,
    /// Inline terminal graphics (Sixel / Kitty / iTerm2). On by default.
    pub graphics: bool,
    /// `addr:port` to serve the MTP control plane over TCP (remote access).
    /// Requires `MTTY_MTP_TOKEN`; `None` (default) is unix-socket only.
    pub remote_listen: Option<String>,
    /// System-wide accelerator that toggles the Quick Terminal, e.g.
    /// `cmd+shift+t` (see ADR 0019). `None` disables it.
    pub quick_terminal_hotkey: Option<String>,
    /// minisign public key used to verify downloaded updates (see ADR 0023).
    pub update_pubkey: Option<String>,
    /// The folder hosts and snippets sync through, encrypted (ADR 0033);
    /// `None` (the default, or an empty value) leaves sync off.
    pub sync_dir: Option<PathBuf>,
    /// Language servers for the editor pane (ADR 0034, E5).
    pub lsp: LspConfig,
    /// ACP agents (ADR 0040, A2).
    pub acp: AcpConfig,
    pub theme: Theme,
    /// The named theme the colours started from (`theme = "…"`), if any.
    pub theme_name: Option<String>,
    /// Set when no `config.toml` was used and settings were imported
    /// (`"ghostty"` / `"alacritty"`).
    pub imported_from: Option<&'static str>,
}

/// `[lsp]` in `config.toml`: language servers are on unless
/// `enabled = false`; `[lsp.<server>]` (`rust`, `typescript`, `python`,
/// `go`, `c`) sets `command` (a string split at spaces, or a list),
/// `root-markers`, or `enabled = false` for that server.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LspConfig {
    pub disabled: bool,
    pub servers: std::collections::BTreeMap<String, LspServerConfig>,
    pub remotes: std::collections::BTreeMap<String, LspRemoteConfig>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LspRemoteConfig {
    pub root: String,
    pub disabled: bool,
    pub servers: std::collections::BTreeMap<String, LspServerConfig>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LspServerConfig {
    pub command: Option<Vec<String>>,
    pub root_markers: Option<Vec<String>>,
    pub disabled: bool,
}

impl LspConfig {
    fn from_table(table: &toml::Table) -> Self {
        let strings = |v: &toml::Value| -> Option<Vec<String>> {
            let list: Vec<String> = match v {
                toml::Value::String(s) => s.split_whitespace().map(str::to_string).collect(),
                toml::Value::Array(a) => a
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect(),
                _ => return None,
            };
            (!list.is_empty()).then_some(list)
        };
        let mut cfg = LspConfig::default();
        for (key, value) in table {
            match value {
                toml::Value::Boolean(on) if key == "enabled" => cfg.disabled = !on,
                toml::Value::Table(remotes) if key == "remote" => {
                    for (target, value) in remotes {
                        if let Some(table) = value.as_table() {
                            let nested = Self::from_table(table);
                            cfg.remotes.insert(
                                target.clone(),
                                LspRemoteConfig {
                                    root: table
                                        .get("root")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or("")
                                        .into(),
                                    disabled: nested.disabled,
                                    servers: nested.servers,
                                },
                            );
                        }
                    }
                }
                toml::Value::Table(server) => {
                    cfg.servers.insert(
                        key.clone(),
                        LspServerConfig {
                            command: server.get("command").and_then(strings),
                            root_markers: server.get("root-markers").and_then(strings),
                            disabled: server.get("enabled").and_then(|v| v.as_bool())
                                == Some(false),
                        },
                    );
                }
                _ => {}
            }
        }
        cfg
    }
}

/// `[acp]` in `config.toml`: one or more ACP agents to launch (ADR 0040, A2).
/// Each `[[acp.agent]]` has a `name` and a `command` (a string split at
/// spaces, or a list).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcpConfig {
    pub agents: Vec<AcpAgent>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcpAgent {
    pub name: String,
    /// The program and its arguments (`["codex", "acp"]`).
    pub command: Vec<String>,
    /// Environment passed to the agent subprocess (keep credentials private).
    pub env: std::collections::BTreeMap<String, String>,
    /// Optional authentication method advertised by the agent.
    pub auth_method: Option<String>,
    /// Optional existing conversation to resume when supported by the agent.
    pub session_id: Option<String>,
}

impl AcpConfig {
    fn from_table(table: &toml::Table) -> Self {
        let strings = |v: &toml::Value| -> Option<Vec<String>> {
            let list: Vec<String> = match v {
                toml::Value::String(s) => s.split_whitespace().map(str::to_string).collect(),
                toml::Value::Array(a) => a
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect(),
                _ => return None,
            };
            (!list.is_empty()).then_some(list)
        };
        let mut cfg = AcpConfig::default();
        if let Some(arr) = table.get("agent").and_then(|v| v.as_array()) {
            for a in arr {
                let name = a
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("agent")
                    .to_string();
                let Some(command) = a.get("command").and_then(strings) else {
                    continue;
                };
                let env = a
                    .get("env")
                    .and_then(toml::Value::as_table)
                    .map(|env| {
                        env.iter()
                            .filter_map(|(name, value)| {
                                value.as_str().map(|v| (name.clone(), v.to_owned()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let optional_string = |key| {
                    a.get(key)
                        .and_then(toml::Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                };
                cfg.agents.push(AcpAgent {
                    name,
                    command,
                    env,
                    auth_method: optional_string("auth_method"),
                    session_id: optional_string("session_id"),
                });
            }
        }
        cfg
    }
}

/// Per-state tab badge switches (`settings.agents.badge_*`).
#[derive(Debug, Clone, Copy)]
pub struct Badges {
    pub processing: bool,
    pub idle: bool,
    pub awaiting: bool,
    pub error: bool,
}

impl Default for Badges {
    fn default() -> Self {
        Self {
            processing: true,
            idle: true,
            awaiting: true,
            error: true,
        }
    }
}

impl Badges {
    pub fn enabled(&self, state: &str) -> bool {
        match state {
            "processing" => self.processing,
            "idle" => self.idle,
            "awaiting" => self.awaiting,
            "error" => self.error,
            _ => false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_size: default_font_size(),
            font_family: default_font_family(),
            line_height: 1.25,
            cursor_style: CursorStyle::Block,
            background_opacity: 1.0,
            notifications: true,
            prevent_sleep: true,
            restore_scrollback: true,
            ssh_auto_reconnect: false,
            pty_host: true,
            keep_sessions_on_quit: false,
            detached_timeout: DEFAULT_DETACHED_TIMEOUT,
            badges: Badges::default(),
            agent_quota_warn: 80,
            language: None,
            update_check_url: Some(
                "https://github.com/oxdingzg/mtty/releases/latest/download/latest.json".into(),
            ),
            update_auto_check: true,
            editor: None,
            editor_vim: false,
            mermaid_command: None,
            graphics: true,
            remote_listen: None,
            quick_terminal_hotkey: None,
            update_pubkey: None,
            sync_dir: None,
            lsp: LspConfig::default(),
            acp: AcpConfig::default(),
            theme_name: None,
            imported_from: None,
            theme: Theme::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct AlacrittyFontNormal {
    family: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AlacrittyFont {
    size: Option<f32>,
    normal: Option<AlacrittyFontNormal>,
}

#[derive(Debug, Deserialize)]
struct AlacrittyPrimary {
    background: Option<String>,
    foreground: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AlacrittyPalette {
    black: Option<String>,
    red: Option<String>,
    green: Option<String>,
    yellow: Option<String>,
    blue: Option<String>,
    magenta: Option<String>,
    cyan: Option<String>,
    white: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AlacrittyColors {
    primary: Option<AlacrittyPrimary>,
    normal: Option<AlacrittyPalette>,
    bright: Option<AlacrittyPalette>,
}

#[derive(Debug, Deserialize)]
struct AlacrittyConfig {
    font: Option<AlacrittyFont>,
    colors: Option<AlacrittyColors>,
}

fn alacritty_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".config")))?;
    Some(base.join("alacritty").join("alacritty.toml"))
}

fn ghostty_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".config")))?;
    Some(base.join("ghostty").join("config"))
}

/// `~/x` relative to the home directory; other paths unchanged.
pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// Replace or add top-level `key = value` assignments in config TOML text.
/// Comments, other keys and tables are kept; new keys go before the first
/// table header. Values are TOML literals (see [`toml_string`]).
pub fn upsert_top_level(text: &str, updates: &[(&str, String)]) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let first_table = lines
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .unwrap_or(lines.len());
    let mut insert_at = first_table;
    while insert_at > 0 && lines[insert_at - 1].trim().is_empty() {
        insert_at -= 1;
    }
    for (key, value) in updates {
        let assignment = format!("{key} = {value}");
        let existing = lines[..first_table.min(lines.len())].iter().position(|l| {
            l.trim_start()
                .strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        });
        match existing {
            Some(i) => lines[i] = assignment,
            None => {
                lines.insert(insert_at, assignment);
                insert_at += 1;
            }
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// A TOML string literal.
pub fn toml_string(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

impl Config {
    /// Apply settings to `config.toml` (see [`upsert_top_level`]). Refuses to
    /// touch a file that does not parse, so a typo is never made worse.
    pub fn save_settings(updates: &[(&str, String)]) -> std::io::Result<std::path::PathBuf> {
        use std::io::{Error, ErrorKind};
        let path =
            Self::path().ok_or_else(|| Error::new(ErrorKind::NotFound, "no config directory"))?;
        let current = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let invalid = |what: &str| Error::new(ErrorKind::InvalidData, what.to_string());
        if toml::from_str::<RawConfig>(&current).is_err() {
            return Err(invalid(
                "config.toml has errors; fix it before saving settings",
            ));
        }
        let next = upsert_top_level(&current, updates);
        if toml::from_str::<RawConfig>(&next).is_err() {
            return Err(invalid("settings would produce invalid TOML"));
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, next)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Config file path: `$XDG_CONFIG_HOME/mtty/config.toml` or `~/.config/mtty/config.toml`.
    pub fn path() -> Option<PathBuf> {
        Some(config_dir()?.join("config.toml"))
    }

    /// Load from the default path. If there is no mtty config, fall back to
    /// importing a ghostty config, then to defaults.
    pub fn load() -> Self {
        Self::load_checked().0
    }

    /// Like [`Config::load`], plus a message when `config.toml` exists but
    /// cannot be parsed. Its values are then ignored, which the user should
    /// hear about instead of silently getting imported or default settings.
    pub fn load_checked() -> (Self, Option<String>) {
        let mut problem = None;
        if let Some(path) = Self::path() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                match toml::from_str::<RawConfig>(&text) {
                    Ok(_) => {
                        if let Some(cfg) = Self::from_toml(&text) {
                            return (cfg, None);
                        }
                    }
                    Err(e) => {
                        let first = e.to_string();
                        let first = first.lines().next().unwrap_or_default().to_string();
                        // The file name is in the UI text; keep the reason visible.
                        problem = Some(first);
                    }
                }
            }
        }
        (Self::load_fallback(), problem)
    }

    fn load_fallback() -> Self {
        if let Some(ghostty) = ghostty_config_path() {
            if let Ok(text) = std::fs::read_to_string(ghostty) {
                let mut cfg = Self::from_ghostty_text(&text);
                cfg.imported_from = Some("ghostty");
                return cfg;
            }
        }
        if let Some(alacritty) = alacritty_config_path() {
            if let Ok(text) = std::fs::read_to_string(alacritty) {
                if let Some(mut cfg) = Self::from_alacritty_text(&text) {
                    cfg.imported_from = Some("alacritty");
                    return cfg;
                }
            }
        }
        Self::default()
    }

    /// Import the subset of an alacritty TOML config we understand
    /// (`font.size`, `colors.primary`, `colors.normal`/`bright`).
    pub fn from_alacritty_text(text: &str) -> Option<Self> {
        let raw: AlacrittyConfig = toml::from_str(text).ok()?;
        let mut cfg = Self::default();
        if let Some(font) = raw.font {
            if let Some(size) = font.size {
                cfg.font_size = size.clamp(6.0, 40.0);
            }
            if let Some(family) = font.normal.and_then(|n| n.family) {
                if !family.trim().is_empty() {
                    cfg.font_family = Some(family.trim().to_string());
                }
            }
        }
        if let Some(colors) = raw.colors {
            if let Some(primary) = colors.primary {
                if let Some(c) = primary.background.as_deref().and_then(Rgb::parse) {
                    cfg.theme.background = c;
                }
                if let Some(c) = primary.foreground.as_deref().and_then(Rgb::parse) {
                    cfg.theme.foreground = c;
                }
            }
            let apply = |palette: AlacrittyPalette, base: usize, cfg: &mut Config| {
                let slots = [
                    palette.black,
                    palette.red,
                    palette.green,
                    palette.yellow,
                    palette.blue,
                    palette.magenta,
                    palette.cyan,
                    palette.white,
                ];
                for (i, slot) in slots.into_iter().enumerate() {
                    if let Some(c) = slot.as_deref().and_then(Rgb::parse) {
                        cfg.theme.palette[base + i] = c;
                    }
                }
            };
            if let Some(normal) = colors.normal {
                apply(normal, 0, &mut cfg);
            }
            if let Some(bright) = colors.bright {
                apply(bright, 8, &mut cfg);
            }
        }
        Some(cfg)
    }

    /// Import the subset of a ghostty `config` file we understand
    /// (`background`, `foreground`, `font-size`, `palette = N=#hex`).
    pub fn from_ghostty_text(text: &str) -> Self {
        let mut cfg = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            match key {
                "background" => {
                    if let Some(c) = Rgb::parse(value) {
                        cfg.theme.background = c;
                    }
                }
                "foreground" => {
                    if let Some(c) = Rgb::parse(value) {
                        cfg.theme.foreground = c;
                    }
                }
                "font-size" => {
                    if let Ok(size) = value.parse::<f32>() {
                        cfg.font_size = size.clamp(6.0, 40.0);
                    }
                }
                "font-family" => {
                    if !value.is_empty() {
                        cfg.font_family = Some(value.to_string());
                    }
                }
                "theme" => {
                    if let Some(theme) = theme_by_name(value) {
                        cfg.theme = theme;
                    }
                }
                "line-height" => {
                    if let Ok(ratio) = value.parse::<f32>() {
                        cfg.line_height = ratio.clamp(0.8, 3.0);
                    }
                }
                "cursor-style" => {
                    if let Some(style) = CursorStyle::parse(value) {
                        cfg.cursor_style = style;
                    }
                }
                "background-opacity" => {
                    if let Ok(opacity) = value.parse::<f32>() {
                        cfg.background_opacity = opacity.clamp(0.1, 1.0);
                    }
                }
                "notifications" => {
                    if let Ok(v) = value.parse::<bool>() {
                        cfg.notifications = v;
                    }
                }
                "prevent-sleep" => {
                    if let Ok(v) = value.parse::<bool>() {
                        cfg.prevent_sleep = v;
                    }
                }
                "restore-scrollback" => {
                    if let Ok(v) = value.parse::<bool>() {
                        cfg.restore_scrollback = v;
                    }
                }
                "pty-host" => {
                    if let Ok(v) = value.parse::<bool>() {
                        cfg.pty_host = v;
                    }
                }
                "keep-sessions-on-quit" => {
                    if let Ok(v) = value.parse::<bool>() {
                        cfg.keep_sessions_on_quit = v;
                    }
                }
                "detached-timeout" => {
                    if let Some(v) = parse_duration(value) {
                        cfg.detached_timeout = v;
                    }
                }
                "palette" => {
                    if let Some((idx, hex)) = value.split_once('=') {
                        if let (Ok(i), Some(c)) = (idx.trim().parse::<usize>(), Rgb::parse(hex)) {
                            if i < 16 {
                                cfg.theme.palette[i] = c;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        cfg
    }

    pub fn from_toml(text: &str) -> Option<Self> {
        let raw: RawConfig = toml::from_str(text).ok()?;
        let mut cfg = Self::default();
        if let Some(size) = raw.font_size {
            cfg.font_size = size.clamp(6.0, 40.0);
        }
        if let Some(family) = raw.font_family {
            let family = family.trim();
            if !family.is_empty() {
                cfg.font_family = Some(family.to_string());
            }
        }
        if let Some(lh) = raw.line_height {
            cfg.line_height = lh.clamp(0.8, 3.0);
        }
        if let Some(style) = raw.cursor_style.as_deref().and_then(CursorStyle::parse) {
            cfg.cursor_style = style;
        }
        if let Some(opacity) = raw.background_opacity {
            cfg.background_opacity = opacity.clamp(0.1, 1.0);
        }
        if let Some(v) = raw.notifications {
            cfg.notifications = v;
        }
        if let Some(v) = raw.prevent_sleep {
            cfg.prevent_sleep = v;
        }
        if let Some(v) = raw.restore_scrollback {
            cfg.restore_scrollback = v;
        }
        if let Some(v) = raw.ssh_auto_reconnect {
            cfg.ssh_auto_reconnect = v;
        }
        if let Some(v) = raw.pty_host {
            cfg.pty_host = v;
        }
        if let Some(v) = raw.keep_sessions_on_quit {
            cfg.keep_sessions_on_quit = v;
        }
        let timeout = match &raw.detached_timeout {
            Some(toml::Value::Integer(secs)) => u64::try_from(*secs)
                .ok()
                .map(std::time::Duration::from_secs),
            Some(toml::Value::String(text)) => parse_duration(text),
            _ => None,
        };
        if let Some(v) = timeout {
            cfg.detached_timeout = v;
        }
        if let Some(lang) = raw.language {
            let lang = lang.trim();
            if !lang.is_empty() {
                cfg.language = Some(lang.to_string());
            }
        }
        if let Some(url) = raw.update_check_url {
            let url = url.trim();
            if !url.is_empty() {
                cfg.update_check_url = Some(url.to_string());
            }
        }
        if let Some(auto) = raw.update_auto_check {
            cfg.update_auto_check = auto;
        }
        if let Some(vim) = raw.editor_vim {
            cfg.editor_vim = vim;
        }
        if let Some(cmd) = raw.mermaid_command {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
                cfg.mermaid_command = Some(cmd.to_string());
            }
        }
        if let Some(g) = raw.graphics {
            cfg.graphics = g;
        }
        if let Some(addr) = raw.remote_listen {
            let addr = addr.trim();
            if !addr.is_empty() {
                cfg.remote_listen = Some(addr.to_string());
            }
        }
        if let Some(editor) = raw.editor {
            let editor = editor.trim();
            if !editor.is_empty() {
                cfg.editor = Some(editor.to_string());
            }
        }
        if let Some(hotkey) = raw.quick_terminal_hotkey {
            let hotkey = hotkey.trim();
            if !hotkey.is_empty() {
                cfg.quick_terminal_hotkey = Some(hotkey.to_string());
            }
        }
        if let Some(dir) = raw.sync_dir {
            let dir = dir.trim();
            if !dir.is_empty() {
                cfg.sync_dir = Some(expand_home(dir));
            }
        }
        if let Some(table) = raw.lsp {
            cfg.lsp = LspConfig::from_table(&table);
        }
        if let Some(table) = raw.acp {
            cfg.acp = AcpConfig::from_table(&table);
        }
        if let Some(key) = raw.update_pubkey {
            let key = key.trim();
            if !key.is_empty() {
                cfg.update_pubkey = Some(key.to_string());
            }
        }
        if let Some(b) = raw.badges {
            if let Some(v) = b.processing {
                cfg.badges.processing = v;
            }
            if let Some(v) = b.idle {
                cfg.badges.idle = v;
            }
            if let Some(v) = b.awaiting {
                cfg.badges.awaiting = v;
            }
            if let Some(v) = b.error {
                cfg.badges.error = v;
            }
        }
        if let Some(v) = raw.agent_quota_warn.as_ref().and_then(|v| v.as_integer()) {
            cfg.agent_quota_warn = v.clamp(0, 100) as u8;
        }
        if let Some(name) = raw.theme {
            if let Some(theme) = theme_by_name(&name) {
                cfg.theme = theme;
                cfg.theme_name = Some(name);
            }
        }
        if let Some(colors) = raw.colors {
            if let Some(bg) = colors.background.as_deref().and_then(Rgb::parse) {
                cfg.theme.background = bg;
            }
            if let Some(fg) = colors.foreground.as_deref().and_then(Rgb::parse) {
                cfg.theme.foreground = fg;
            }
            if let Some(palette) = colors.palette {
                for (i, hex) in palette.iter().take(16).enumerate() {
                    if let Some(c) = Rgb::parse(hex) {
                        cfg.theme.palette[i] = c;
                    }
                }
            }
        }
        Some(cfg)
    }
}

/// Config crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {

    #[test]
    fn lsp_servers_are_configured_per_key() {
        let cfg = Config::from_toml(
            "[lsp.rust]\ncommand = \"ra-multiplex client\"\n\n[lsp.python]\ncommand = [\"pylsp\"]\nroot-markers = [\"setup.py\"]\n\n[lsp.go]\nenabled = false\n",
        )
        .unwrap();
        assert!(!cfg.lsp.disabled);
        assert_eq!(
            cfg.lsp.servers["rust"].command,
            Some(vec!["ra-multiplex".to_string(), "client".to_string()])
        );
        assert_eq!(
            cfg.lsp.servers["python"].root_markers,
            Some(vec!["setup.py".to_string()])
        );
        assert!(cfg.lsp.servers["go"].disabled);
        let off = Config::from_toml("[lsp]\nenabled = false\n").unwrap();
        assert!(off.lsp.disabled);
        assert_eq!(Config::from_toml("").unwrap().lsp, LspConfig::default());
    }

    #[test]
    fn remote_language_server_settings_keep_target_scope() {
        let cfg=Config::from_toml("[lsp.remote.\"build.example.invalid\"]\nroot=\"/project\"\n[lsp.remote.\"build.example.invalid\".rust]\ncommand=[\"rust-analyzer\"]\n").unwrap();
        let remote = &cfg.lsp.remotes["build.example.invalid"];
        assert_eq!(remote.root, "/project");
        assert_eq!(
            remote.servers["rust"].command,
            Some(vec!["rust-analyzer".into()])
        );
        assert!(!cfg.lsp.servers.contains_key("remote"));
        assert!(Config::from_toml("").unwrap().lsp.remotes.is_empty());
    }

    #[test]
    fn acp_agents_are_configured() {
        let cfg = Config::from_toml(
            "[acp]\n\n[[acp.agent]]\nname = \"codex\"\ncommand = \"codex acp\"\n\n\
             [[acp.agent]]\nname = \"gemini\"\ncommand = [\"gemini\", \"--experimental-acp\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.acp.agents.len(), 2);
        assert_eq!(cfg.acp.agents[0].name, "codex");
        assert_eq!(
            cfg.acp.agents[0].command,
            vec!["codex".to_string(), "acp".to_string()]
        );
        assert_eq!(
            cfg.acp.agents[1].command,
            vec!["gemini".to_string(), "--experimental-acp".to_string()]
        );
        assert!(Config::from_toml("").unwrap().acp.agents.is_empty());
        assert!(cfg.acp.agents[0].env.is_empty());
        assert_eq!(cfg.acp.agents[0].auth_method, None);
        assert_eq!(cfg.acp.agents[0].session_id, None);
        let cfg = Config::from_toml(
            r#"
[[acp.agent]]
command = ["agent", "--acp"]
auth_method = "oauth"
session_id = "existing-session"
env = { AGENT_MODE = "test", IGNORED_NONSTRING = 5 }
"#,
        )
        .unwrap();
        let agent = &cfg.acp.agents[0];
        assert_eq!(agent.auth_method.as_deref(), Some("oauth"));
        assert_eq!(agent.session_id.as_deref(), Some("existing-session"));
        assert_eq!(
            agent.env.get("AGENT_MODE").map(String::as_str),
            Some("test")
        );
        assert_eq!(agent.env.len(), 1);
    }

    #[test]
    fn config_lives_in_the_platform_folder_on_windows() {
        let p = |s: &str| Some(PathBuf::from(s));
        let none = |_: &Path| false;
        // An explicit XDG directory always wins.
        assert_eq!(
            base_dir(
                p("/x"),
                p(r"C:\Users\me\AppData\Roaming"),
                p("/h"),
                ".config",
                true,
                &none
            ),
            p("/x")
        );
        // Windows without HOME (the usual case): %APPDATA%.
        assert_eq!(
            base_dir(
                None,
                p(r"C:\Users\me\AppData\Roaming"),
                None,
                ".config",
                true,
                &none
            ),
            p(r"C:\Users\me\AppData\Roaming")
        );
        // A HOME set by Git Bash does not move a new user's config…
        assert_eq!(
            base_dir(
                None,
                p("C:/AppData"),
                p("C:/Users/me"),
                ".config",
                true,
                &none
            ),
            p("C:/AppData")
        );
        // …but an existing ~/.config/mtty keeps being used.
        let home_has_mtty = |path: &Path| path.ends_with(".config/mtty");
        assert_eq!(
            base_dir(
                None,
                p("C:/AppData"),
                p("C:/Users/me"),
                ".config",
                true,
                &home_has_mtty
            ),
            Some(PathBuf::from("C:/Users/me").join(".config"))
        );
        // Elsewhere nothing changes.
        assert_eq!(
            base_dir(None, p("/ignored"), p("/home/me"), ".config", false, &none),
            p("/home/me/.config")
        );
        assert_eq!(base_dir(None, None, None, ".config", false, &none), None);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mtty-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn migration_copies_once_and_keeps_the_old_directory() {
        let root = scratch("migrate");
        let old = root.join("miaotty");
        let new = root.join("mtty");
        std::fs::create_dir_all(old.join("hooks")).unwrap();
        std::fs::create_dir_all(old.join("inbox")).unwrap();
        std::fs::write(old.join("config.toml"), "font-size = 15\n").unwrap();
        std::fs::write(old.join("hooks/claude.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(old.join("inbox/1"), "quick").unwrap();

        assert!(migrate_dir(&old, &new).unwrap());
        assert_eq!(
            std::fs::read_to_string(new.join("config.toml")).unwrap(),
            "font-size = 15\n"
        );
        assert!(new.join("hooks/claude.sh").is_file());
        assert!(!new.join("inbox").exists(), "runtime inbox is not migrated");
        assert!(
            old.join("config.toml").is_file(),
            "the old directory is kept"
        );
        assert!(!root.join(".mtty.migrating").exists());

        // Never overwrite: a second run (or an existing new dir) does nothing.
        std::fs::write(old.join("config.toml"), "font-size = 20\n").unwrap();
        assert!(!migrate_dir(&old, &new).unwrap());
        assert_eq!(
            std::fs::read_to_string(new.join("config.toml")).unwrap(),
            "font-size = 15\n"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn migration_without_an_old_directory_does_nothing() {
        let root = scratch("fresh");
        assert!(!migrate_dir(&root.join("miaotty"), &root.join("mtty")).unwrap());
        assert!(!root.join("mtty").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn env_prefers_the_new_prefix() {
        // Unique names keep this test independent of the real environment.
        std::env::set_var("MIAOTTY_CFGTEST_A", "old");
        assert_eq!(env("CFGTEST_A").as_deref(), Some("old"));
        std::env::set_var("MTTY_CFGTEST_A", "new");
        assert_eq!(env("CFGTEST_A").as_deref(), Some("new"));
        std::env::set_var("MTTY_CFGTEST_B", "");
        std::env::set_var("MIAOTTY_CFGTEST_B", "old");
        assert_eq!(
            env("CFGTEST_B").as_deref(),
            Some("old"),
            "empty counts as unset"
        );
        assert_eq!(env("CFGTEST_NONE"), None);
    }

    #[test]
    fn upsert_keeps_comments_and_tables() {
        let text = "# mine\nfont-size = 13 # big\n\n[colors]\nbackground = \"#000000\"\n";
        let out = upsert_top_level(
            text,
            &[
                ("font-size", "15".into()),
                ("theme", toml_string("dracula")),
            ],
        );
        assert_eq!(
            out,
            "# mine\nfont-size = 15\ntheme = \"dracula\"\n\n[colors]\nbackground = \"#000000\"\n"
        );
        let cfg = Config::from_toml(&out).unwrap();
        assert_eq!(cfg.font_size, 15.0);
        assert_eq!(cfg.theme_name.as_deref(), Some("dracula"));
        assert_eq!(
            cfg.theme.background,
            Rgb(0, 0, 0),
            "explicit colours still win"
        );
    }

    #[test]
    fn upsert_into_an_empty_file_and_does_not_confuse_prefixes() {
        let out = upsert_top_level("", &[("font-family", toml_string("Menlo"))]);
        assert_eq!(out, "font-family = \"Menlo\"\n");
        // `font-size` must not match a `font-size-extra` key.
        let out = upsert_top_level("font-size-extra = 1\n", &[("font-size", "12".into())]);
        assert_eq!(out, "font-size-extra = 1\nfont-size = 12\n");
    }

    #[test]
    fn string_values_are_escaped() {
        for family in ["a\"b", "it's", "x\\y", "等宽 Mono"] {
            let out = upsert_top_level("", &[("font-family", toml_string(family))]);
            let cfg = Config::from_toml(&out).unwrap();
            assert_eq!(cfg.font_family.as_deref(), Some(family), "{out}");
        }
    }
    use super::*;

    #[test]
    fn parses_hex() {
        assert_eq!(Rgb::parse("#2e3440"), Some(Rgb(0x2e, 0x34, 0x40)));
        assert_eq!(Rgb::parse("2e3440"), Some(Rgb(0x2e, 0x34, 0x40)));
        assert_eq!(Rgb::parse("nope"), None);
    }

    #[test]
    fn defaults_to_nord() {
        let cfg = Config::default();
        assert_eq!(cfg.theme.background, Rgb(0x2e, 0x34, 0x40));
        assert_eq!(cfg.font_size, 13.0);
        assert_eq!(cfg.font_family.as_deref(), Some("JetBrains Mono"));
    }

    #[test]
    fn imports_alacritty() {
        let cfg = Config::from_alacritty_text(
            r##"
            [font]
            size = 15
            [colors.primary]
            background = "#000000"
            foreground = "#cccccc"
            [colors.normal]
            red = "#ff0000"
            [colors.bright]
            red = "#ff5555"
            "##,
        )
        .unwrap();
        assert_eq!(cfg.font_size, 15.0);
        assert_eq!(cfg.theme.background, Rgb(0, 0, 0));
        assert_eq!(cfg.theme.foreground, Rgb(0xcc, 0xcc, 0xcc));
        assert_eq!(cfg.theme.palette[1], Rgb(0xff, 0, 0));
        assert_eq!(cfg.theme.palette[9], Rgb(0xff, 0x55, 0x55));
    }

    #[test]
    fn named_theme_and_font_family() {
        let cfg = Config::from_toml(
            r##"
            theme = "Dracula"
            font-family = "JetBrains Mono"
            "##,
        )
        .unwrap();
        assert_eq!(cfg.theme.background, Rgb(0x28, 0x2a, 0x36));
        assert_eq!(cfg.font_family.as_deref(), Some("JetBrains Mono"));

        // The mtty preset is reachable from the config by name, so the
        // widget can resolve it to its chrome as well as its terminal colours.
        let cfg = Config::from_toml("theme = \"mtty\"\n").unwrap();
        assert_eq!(cfg.theme.background, Rgb(0x21, 0x27, 0x33));
        assert_eq!(cfg.theme_name.as_deref(), Some("mtty"));
    }

    #[test]
    fn imports_ghostty() {
        let cfg = Config::from_ghostty_text(
            "# comment\nbackground = #212733\nforeground = #e5e5e5\nfont-size = 16\npalette = 4=#6d95b4\n",
        );
        assert_eq!(cfg.theme.background, Rgb(0x21, 0x27, 0x33));
        assert_eq!(cfg.theme.foreground, Rgb(0xe5, 0xe5, 0xe5));
        assert_eq!(cfg.font_size, 16.0);
        assert_eq!(cfg.theme.palette[4], Rgb(0x6d, 0x95, 0xb4));
    }

    #[test]
    fn overrides_from_toml() {
        let cfg = Config::from_toml(
            r##"
            font-size = 16
            [colors]
            background = "#000000"
            foreground = "#ffffff"
            "##,
        )
        .unwrap();
        assert_eq!(cfg.font_size, 16.0);
        assert_eq!(cfg.theme.background, Rgb(0, 0, 0));
        assert_eq!(cfg.theme.foreground, Rgb(255, 255, 255));
    }

    #[test]
    fn ssh_auto_reconnect_is_off_unless_asked() {
        assert!(
            !Config::from_toml("").unwrap().ssh_auto_reconnect,
            "opt-in: nothing connects on startup by default"
        );
        assert!(
            Config::from_toml("ssh-auto-reconnect = true\n")
                .unwrap()
                .ssh_auto_reconnect
        );
        assert!(
            !Config::from_toml("ssh-auto-reconnect = false\n")
                .unwrap()
                .ssh_auto_reconnect
        );
    }

    #[test]
    fn pty_host_is_on_unless_turned_off() {
        assert!(Config::from_toml("").unwrap().pty_host);
        assert!(!Config::from_toml("pty-host = false\n").unwrap().pty_host);
        assert!(!Config::from_ghostty_text("pty-host = false\n").pty_host);
    }

    #[test]
    fn pty_host_sessions_and_timeout() {
        let default = Config::from_toml("").unwrap();
        assert!(!default.keep_sessions_on_quit);
        assert_eq!(default.detached_timeout, DEFAULT_DETACHED_TIMEOUT);
        let cfg =
            Config::from_toml("keep-sessions-on-quit = true\ndetached-timeout = \"7d\"\n").unwrap();
        assert!(cfg.keep_sessions_on_quit);
        assert_eq!(cfg.detached_timeout.as_secs(), 7 * 86_400);
        let secs = Config::from_toml("detached-timeout = 90\n").unwrap();
        assert_eq!(secs.detached_timeout.as_secs(), 90);
        let bad = Config::from_toml("detached-timeout = \"soon\"\n").unwrap();
        assert_eq!(
            bad.detached_timeout, DEFAULT_DETACHED_TIMEOUT,
            "kept on a bad value"
        );
        let ghostty = Config::from_ghostty_text("detached-timeout = 30m\n");
        assert_eq!(ghostty.detached_timeout.as_secs(), 1800);
        for (text, secs) in [
            ("90s", 90),
            ("30m", 1800),
            ("24h", 86_400),
            ("2d", 172_800),
            ("45", 45),
        ] {
            assert_eq!(
                parse_duration(text).map(|d| d.as_secs()),
                Some(secs),
                "{text}"
            );
        }
        for text in ["", "0", "0h", "1w", "h", "-5", "1.5h"] {
            assert_eq!(parse_duration(text), None, "{text}");
        }
    }

    #[test]
    fn agent_quota_warn_defaults_and_clamps() {
        assert_eq!(Config::from_toml("").unwrap().agent_quota_warn, 80);
        assert_eq!(
            Config::from_toml("agent-quota-warn = 95\n")
                .unwrap()
                .agent_quota_warn,
            95
        );
        assert_eq!(
            Config::from_toml("agent-quota-warn = 300\n")
                .unwrap()
                .agent_quota_warn,
            100,
            "clamped"
        );
        assert_eq!(
            Config::from_toml("agent-quota-warn = \"lots\"\n")
                .unwrap()
                .agent_quota_warn,
            80,
            "kept on a bad value"
        );
    }
}
