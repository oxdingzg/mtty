//! UI icons, rendered from the Tabler Icons font (MIT) so they are crisp and
//! consistent. The host registers the `tabler` family.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Terminal,
    Info,
    Agent,
    Outline,
    Git,
    Files,
    Ports,
    Queue,
    Sidebar,
    Details,
    Plus,
    Folder,
    File,
    Server,
    GitBranch,
    Command,
    Search,
    Refresh,
    /// Agent states are drawn as shapes, including distinct pause, clock and
    /// question markers rather than using color alone to explain idle work.
    StateEmpty,
    StateBusy,
    StateWait,
    StateBackground,
    StatePaused,
    StateUnknown,
    StateFull,
}

/// The Tabler Icons glyph for an icon.
pub fn glyph(icon: Icon) -> char {
    match icon {
        Icon::Terminal => '\u{ebdc}',
        Icon::Info => '\u{eac5}',                  // info-circle
        Icon::Agent => '\u{f00b}',                 // robot
        Icon::Outline => '\u{eb6b}',               // list
        Icon::Git | Icon::GitBranch => '\u{eab2}', // git-branch
        Icon::Files | Icon::Folder => '\u{eaad}',  // folder
        Icon::Ports => '\u{ebd9}',                 // plug
        Icon::Queue => '\u{eb6a}',                 // list-check
        Icon::Sidebar => '\u{eada}',               // layout-sidebar
        Icon::Details => '\u{ead4}',               // layout-columns
        Icon::Plus => '\u{eb0b}',
        Icon::File => '\u{eaa4}',
        Icon::Server => '\u{eb1f}',
        Icon::Command => '\u{ea78}',
        Icon::Search => '\u{eb1c}',
        Icon::Refresh => '\u{eb13}',
        // Drawn as shapes by `draw`; the plain circles stand in elsewhere.
        Icon::StateEmpty => '\u{25cb}',
        Icon::StateBusy => '\u{25d0}',
        Icon::StateWait => '\u{25c9}',
        Icon::StateBackground => '\u{25f7}',
        Icon::StatePaused => '\u{23f8}',
        Icon::StateUnknown => '?',
        Icon::StateFull => '\u{25cf}',
    }
}

/// Draw `icon` centered in `rect`, filled with `color`.
pub fn draw(p: &egui::Painter, rect: egui::Rect, icon: Icon, color: egui::Color32) {
    let size = rect.height().min(rect.width()).max(8.0);
    if matches!(
        icon,
        Icon::StateEmpty
            | Icon::StateBusy
            | Icon::StateWait
            | Icon::StateBackground
            | Icon::StatePaused
            | Icon::StateUnknown
            | Icon::StateFull
    ) {
        draw_state(p, rect.center(), size * 0.36, icon, color);
        return;
    }
    if matches!(icon, Icon::Sidebar | Icon::Details) {
        // Matching left/right panels, rather than an equal-column layout glyph.
        let frame =
            egui::Rect::from_center_size(rect.center(), egui::vec2(size * 0.75, size * 0.75));
        let stroke = egui::Stroke::new(size / 12.0, color);
        p.rect_stroke(frame, egui::Rounding::same(size / 12.0), stroke);
        let fraction = if icon == Icon::Sidebar {
            1.0 / 3.0
        } else {
            2.0 / 3.0
        };
        let x = frame.left() + frame.width() * fraction;
        p.line_segment(
            [egui::pos2(x, frame.top()), egui::pos2(x, frame.bottom())],
            stroke,
        );
        return;
    }
    let font = egui::FontId::new(size, egui::FontFamily::Name("tabler".into()));
    p.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph(icon),
        font,
        color,
    );
}

/// An agent-state marker of `radius` at `center`: an empty ring (idle), a
/// rotating arc (executing), a ring with a filled core (waiting for you), a
/// clock (background wait), pause bars (unfinished), question (unavailable),
/// or a solid disc (a finished turn, done or failed).
fn draw_state(
    p: &egui::Painter,
    center: egui::Pos2,
    radius: f32,
    icon: Icon,
    color: egui::Color32,
) {
    let stroke = egui::Stroke::new((radius / 3.0).max(1.3), color);
    match icon {
        Icon::StatePaused => {
            for offset in [-0.4, 0.4] {
                p.line_segment(
                    [
                        center + radius * egui::vec2(offset, -0.8),
                        center + radius * egui::vec2(offset, 0.8),
                    ],
                    stroke,
                );
            }
        }
        Icon::StateUnknown => {
            p.text(
                center,
                egui::Align2::CENTER_CENTER,
                "?",
                egui::FontId::proportional(radius * 2.8),
                color,
            );
        }
        Icon::StateBackground => {
            p.circle_stroke(center, radius, stroke);
            p.line_segment([center, center + radius * egui::vec2(0.0, -0.6)], stroke);
            p.line_segment([center, center + radius * egui::vec2(0.5, 0.2)], stroke);
        }
        Icon::StateFull => {
            p.circle_filled(center, radius, color);
        }
        Icon::StateWait => {
            // A ring with a solid core, so "waiting for you" reads apart from
            // the solid disc of a finished turn even without colour.
            p.circle_stroke(center, radius, stroke);
            p.circle_filled(center, radius * 0.42, color);
        }
        Icon::StateBusy => {
            // A rotating three-quarter arc: unmistakably "working", and it
            // moves so a long turn never looks stalled.
            let t = p.ctx().input(|i| i.time) as f32;
            let start = t * std::f32::consts::TAU / 1.2;
            let span = std::f32::consts::TAU * 0.75;
            let steps = 28;
            let points: Vec<egui::Pos2> = (0..=steps)
                .map(|i| {
                    let a = start + span * i as f32 / steps as f32;
                    center + radius * egui::vec2(a.cos(), a.sin())
                })
                .collect();
            p.add(egui::Shape::line(points, stroke));
            p.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }
        _ => {
            p.circle_stroke(center, radius, stroke);
        }
    }
}

/// A small icon button (allocates space, draws the icon, hover highlight).
pub fn icon_button(ui: &mut egui::Ui, icon: Icon, color: egui::Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(24.0, 20.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        if resp.hovered() {
            ui.painter().rect_filled(
                rect,
                egui::Rounding::same(4.0),
                ui.visuals().widgets.hovered.bg_fill,
            );
        }
        draw(ui.painter(), rect, icon, color);
    }
    resp
}

/// A tab's icon: a built-in [`Icon`], or a glyph chosen by a view rule
/// (ADR 0007), optionally colored.
#[derive(Clone, Debug, PartialEq)]
pub struct TabIcon {
    pub icon: Icon,
    pub glyph: Option<String>,
    pub color: Option<crate::theme::Rgb>,
    /// Localized explanation of the agent state, shared by tab and sidebar hover.
    pub hint: Option<String>,
}

impl From<Icon> for TabIcon {
    fn from(icon: Icon) -> Self {
        Self {
            icon,
            glyph: None,
            color: None,
            hint: None,
        }
    }
}

/// Draw a tab icon: the rule's glyph (from the bundled Symbols Nerd Font or
/// an emoji) when there is one, else the built-in icon.
pub fn draw_tab_icon(p: &egui::Painter, rect: egui::Rect, icon: &TabIcon, color: egui::Color32) {
    let color = icon
        .color
        .map(|c| egui::Color32::from_rgb(c.0, c.1, c.2))
        .unwrap_or(color);
    match &icon.glyph {
        Some(glyph) => {
            let size = rect.height().min(rect.width()).max(8.0);
            p.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                glyph,
                egui::FontId::proportional(size),
                color,
            );
        }
        None => draw(p, rect, icon.icon, color),
    }
}

/// The documented view-rule icon names (docs/VIEW-RULES.md). Code points are
/// taken from the cmap of the bundled `SymbolsNerdFontMono-Regular.ttf`,
/// mostly its Codicons set.
pub const RULE_ICONS: &[(&str, char)] = &[
    ("folder", '\u{ea83}'),     // cod-folder
    ("file", '\u{ea7b}'),       // cod-file
    ("file-text", '\u{eb26}'),  // cod-note
    ("terminal", '\u{ea85}'),   // cod-terminal
    ("code", '\u{eac4}'),       // cod-code
    ("git-branch", '\u{f418}'), // oct-git_branch
    ("git-commit", '\u{eafc}'), // cod-git_commit
    ("github", '\u{ea84}'),     // cod-github
    ("globe", '\u{eb01}'),      // cod-globe
    ("bug", '\u{eaaf}'),        // cod-bug
    ("flame", '\u{eaf2}'),      // cod-flame
    ("cpu", '\u{ec19}'),        // cod-chip
    ("cloud", '\u{ebaa}'),      // cod-cloud
    ("database", '\u{eace}'),   // cod-database
    ("package", '\u{eb29}'),    // cod-package
    ("box", '\u{ed75}'),        // fa-box
    ("coffee", '\u{ec15}'),     // cod-coffee
    ("heart", '\u{eb05}'),      // cod-heart
    ("star", '\u{eb59}'),       // cod-star_full
    ("flag", '\u{f024}'),       // fa-flag
    ("zap", '\u{26a1}'),        // oct-zap
    ("lock", '\u{ea75}'),       // cod-lock
    ("search", '\u{ea6d}'),     // cod-search
    ("settings", '\u{eb51}'),   // cod-settings_gear
    ("user", '\u{eb99}'),       // cod-account
    ("home", '\u{eb06}'),       // cod-home
    ("bell", '\u{eaa2}'),       // cod-bell
    ("layers", '\u{ebd2}'),     // cod-layers
    ("claude", '\u{ec10}'),     // cod-sparkle
];

/// The view-rule icon name for a file-tree entry, chosen from its extension
/// (`folder` for a directory). Unknown types fall back to `file`.
pub fn file_type(name: &str, is_dir: bool) -> &'static str {
    if is_dir {
        return "folder";
    }
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "py" | "go" | "rb" | "php"
        | "java" | "kt" | "kts" | "swift" | "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "cs"
        | "sh" | "bash" | "zsh" | "fish" | "lua" | "vim" | "scala" | "ex" | "exs" | "dart"
        | "r" | "sql" | "html" | "css" | "scss" | "vue" | "svelte" | "astro" => "code",
        "md" | "markdown" | "mdx" | "txt" | "rst" | "org" | "tex" | "log" => "file-text",
        "json" | "jsonc" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "conf" | "env"
        | "properties" => "settings",
        "lock" | "sum" => "lock",
        "db" | "sqlite" | "sqlite3" => "database",
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "rar" | "7z" | "jar" | "war" => {
            "package"
        }
        _ => "file",
    }
}

/// The glyph for a view rule's icon: a known `name`, else the `emoji`, else
/// a dot when only a color was given (docs/VIEW-RULES.md, "Icons").
pub fn rule_glyph(name: Option<&str>, emoji: Option<&str>, has_color: bool) -> Option<String> {
    let named = name.and_then(|n| {
        RULE_ICONS
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(n.trim()))
            .map(|(_, c)| c.to_string())
    });
    named
        .or_else(|| {
            emoji
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(str::to_string)
        })
        .or_else(|| has_color.then(|| "\u{25cf}".to_string()))
}

#[cfg(test)]
mod rule_icon_tests {
    use super::*;

    #[test]
    fn documented_names_resolve_and_fall_back_in_order() {
        let documented = "folder file file-text terminal code git-branch git-commit github \
                          globe bug flame cpu cloud database package box coffee heart star \
                          flag zap lock search settings user home bell layers claude";
        for name in documented.split_whitespace() {
            assert!(rule_glyph(Some(name), None, false).is_some(), "{name}");
        }
        assert_eq!(
            rule_glyph(Some("Git-Branch"), None, false),
            rule_glyph(Some("git-branch"), None, false)
        );
        assert_eq!(
            rule_glyph(Some("nope"), Some("🦀"), true).as_deref(),
            Some("🦀")
        );
        assert_eq!(
            rule_glyph(Some("nope"), None, true).as_deref(),
            Some("\u{25cf}")
        );
        assert_eq!(rule_glyph(None, None, false), None);
    }
}
