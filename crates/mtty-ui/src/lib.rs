//! `mtty-ui` — host-agnostic pieces used by mtty and embedders.
//!
//! Nothing here owns an event loop or a GPU: it is theme, input encoding,
//! selection, row building for the renderer, the split layout model, and the
//! egui chrome widgets. Keeping them here is what lets the chrome be shared
//! instead of duplicated (ADR 0030, step 2/4).

pub mod acp_adapters;
pub mod agentloop;
pub mod attention;
pub mod chrome;
pub mod data_preview;
pub mod file_link;
pub mod forward;
pub mod ftp;
pub mod hints;
pub mod hostkeys;
pub mod hotkey;
pub mod i18n;
pub mod icons;
pub mod input;
pub mod install;
pub mod integration;
pub mod launch;
pub mod layout;
pub mod markdown;
pub mod menu;
pub mod mermaid;
pub mod palette;
pub mod project;
pub mod remote_control;
pub mod selection;
pub mod sftp;
pub mod ssh;
pub mod syntax;
pub mod task_review;
pub mod tasks;
pub mod theme;
pub mod transport;
pub mod update;
pub mod vim;

use mtty_core::ATerm;
use mtty_render::Span;
use theme::{Rgb, Theme};

pub use input::{
    encode_key, encode_paste, encode_text, EncodeOpts, KeyKind, Modifiers, KITTY_DISAMBIGUATE,
    KITTY_REPORT_ALL_KEYS, KITTY_REPORT_ALTERNATE, KITTY_REPORT_EVENTS, KITTY_REPORT_TEXT,
};
pub use layout::{Layout, Rect, SplitDir};
pub use selection::Selection;
pub use theme::{CursorStyle, Theme as UiTheme};

/// Build per-row, cell-pinned spans for the grid renderer (wide chars get their
/// own span so every glyph lands on an exact cell).
pub fn build_rows(screen: &ATerm, theme: &Theme, cursor: Option<(u16, u16)>) -> Vec<Vec<Span>> {
    let (rows, cols) = screen.size();
    let mut out = Vec::with_capacity(rows as usize);
    for row in 0..rows {
        let mut spans: Vec<Span> = Vec::new();
        let mut run_col: Option<u16> = None;
        let mut run_color = (0u8, 0u8, 0u8);
        let mut run_text = String::new();
        let mut col = 0u16;
        while col < cols {
            let Some(cell) = screen.cell(row, col) else {
                col += 1;
                continue;
            };
            if cell.wide_spacer {
                col += 1;
                continue;
            }
            let width = if cell.wide { 2 } else { 1 };
            let color = if cursor == Some((row, col)) {
                rgb(theme.bg)
            } else if cell.inverse {
                rgb(theme.color(cell.bg, false))
            } else {
                rgb(cell_foreground(theme, &cell))
            };
            if width >= 2 {
                if let Some(rc) = run_col.take() {
                    spans.push(Span::new(rc, std::mem::take(&mut run_text), run_color));
                }
                spans.push(Span::new(col, cell.ch.to_string(), color));
                col += 2;
            } else {
                if let Some(rc) = run_col {
                    if run_color == color {
                        run_text.push(cell.ch);
                    } else {
                        spans.push(Span::new(rc, std::mem::take(&mut run_text), run_color));
                        run_col = Some(col);
                        run_color = color;
                        run_text.push(cell.ch);
                    }
                } else {
                    run_col = Some(col);
                    run_color = color;
                    run_text.push(cell.ch);
                }
                col += 1;
            }
        }
        if let Some(rc) = run_col.take() {
            spans.push(Span::new(rc, run_text, run_color));
        }
        out.push(spans);
    }
    out
}

/// The colour behind a cell. Inverse video (SGR 7, zsh's highlight for
/// pasted text) swaps the colours: the text takes the background (see
/// [`build_rows`]) and the background the text colour, so a default-coloured
/// cell shows as light on dark turned dark on light, never as text in the
/// background colour on the background.
pub fn cell_background(theme: &Theme, cell: &mtty_core::aterm::CellView) -> Rgb {
    if cell.inverse {
        cell_foreground(theme, cell)
    } else {
        theme.color(cell.bg, false)
    }
}

/// Box drawing and block elements paint TUI surfaces, including half-cell
/// edges whose foreground intentionally matches the adjacent background.
/// Raising their contrast turns those edges into unwanted bright borders.
fn cell_foreground(theme: &Theme, cell: &mtty_core::aterm::CellView) -> Rgb {
    let color = if matches!(cell.ch, '\u{2500}'..='\u{259f}') {
        theme.color(cell.fg, false)
    } else {
        theme.foreground(cell.fg, cell.bg)
    };
    if cell.dim {
        // Faint is meant to read as secondary: dim after the contrast lift,
        // which would otherwise undo it.
        faint(color, theme.color(cell.bg, false))
    } else {
        color
    }
}

/// SGR 2: the colour two thirds of the way from the background to `color`.
fn faint(color: Rgb, bg: Rgb) -> Rgb {
    let mix = |c: u8, b: u8| ((u16::from(c) * 2 + u16::from(b)) / 3) as u8;
    Rgb(mix(color.0, bg.0), mix(color.1, bg.1), mix(color.2, bg.2))
}

pub fn rgb(c: Rgb) -> (u8, u8, u8) {
    (c.0, c.1, c.2)
}

/// Width in terminal cells (2 for wide CJK/emoji, else 1).
pub fn width_of(c: char) -> u16 {
    let u = c as u32;
    if u >= 0x1100
        && (u <= 0x115f
            || (0x2e80..=0xa4cf).contains(&u)
            || (0xac00..=0xd7a3).contains(&u)
            || (0xf900..=0xfaff).contains(&u)
            || (0xfe30..=0xfe4f).contains(&u)
            || (0xff00..=0xff60).contains(&u)
            || (0xffe0..=0xffe6).contains(&u)
            || (0x1f300..=0x1faff).contains(&u)
            || (0x20000..=0x3fffd).contains(&u))
    {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod row_tests {
    use super::*;

    #[test]
    fn unicode_symbols_do_not_skip_the_following_grid_cell() {
        for symbol in ['\u{1f5a5}', '\u{1f5d2}', '\u{303f}', '中'] {
            let mut screen = ATerm::new(10, 2, 100);
            screen.process(format!("{symbol}\x1b[31mX\x1b[0mY").as_bytes());
            let rows = build_rows(&screen, &Theme::nord(), None);
            let x_col = (0..10)
                .find(|&col| screen.cell(0, col).unwrap().ch == 'X')
                .unwrap();
            let x = rows[0].iter().find(|span| span.text == "X");
            assert!(x.is_some(), "{symbol:?} swallowed the following X");
            assert_eq!(x.unwrap().col, x_col);
        }
    }

    #[test]
    fn faint_text_reads_dimmer_than_normal_text() {
        let mut screen = ATerm::new(20, 3, 100);
        screen.process(b"ab\x1b[2mcd\x1b[0mef");
        let theme = Theme::nord();
        let rows = build_rows(&screen, &theme, None);
        let colors: Vec<_> = rows[0].iter().map(|s| (s.text.as_str(), s.color)).collect();
        assert_eq!(colors.len(), 3, "{colors:?}");
        let rgb_of = |c: (u8, u8, u8)| Rgb(c.0, c.1, c.2);
        let (normal, faint) = (rgb_of(colors[0].1), rgb_of(colors[1].1));
        assert_eq!(colors[1].0, "cd");
        assert_eq!(colors[2].1, colors[0].1, "back to normal after SGR 0");
        // Faint sits between the text and the background, and is not lifted
        // back to full contrast.
        let bg = theme.color(
            mtty_core::aterm::Color::Named(mtty_core::aterm::NamedColor::Background),
            false,
        );
        assert!(
            faint.0 < normal.0 && faint.0 > bg.0,
            "{faint:?} vs {normal:?}"
        );
        assert_eq!(faint, super::faint(normal, bg));
    }

    #[test]
    fn tui_drawing_preserves_low_contrast_colors_while_text_is_lifted() {
        let mut screen = ATerm::new(20, 3, 100);
        screen.process("\x1b[38;2;29;33;31;48;2;12;15;14m▀━█X".as_bytes());
        let theme = Theme::nord();
        let rows = build_rows(&screen, &theme, None);
        assert_eq!(rows[0][0].text, "▀━█");
        assert_eq!(rows[0][0].color, (29, 33, 31));
        assert_eq!(rows[0][1].text, "X");
        assert_ne!(rows[0][1].color, (29, 33, 31));

        screen.process("\r\n\x1b[7m▀".as_bytes());
        let cell = screen.cell(1, 0).unwrap();
        assert_eq!(cell_background(&theme, &cell), Rgb(29, 33, 31));
        assert_eq!(rows[0][0].color, rgb(cell_background(&theme, &cell)));
    }

    #[test]
    fn inverse_video_uses_the_cells_background_for_text() {
        let mut screen = ATerm::new(20, 3, 100);
        screen.process(b"\x1b[31;42;7mX\x1b[0m");
        let theme = Theme::nord();
        let rows = build_rows(&screen, &theme, None);
        assert_eq!(rows[0][0].text, "X");
        assert_eq!(rows[0][0].color, rgb(theme.palette[2]));
        let cell = screen.cell(0, 0).unwrap();
        assert_eq!(cell_background(&theme, &cell), theme.palette[1]);
    }

    #[test]
    fn inverse_default_colours_paint_a_light_background() {
        // zsh highlights a pasted (or dropped) path with standout: text in
        // the background colour must sit on the foreground colour.
        let mut screen = ATerm::new(20, 3, 100);
        screen.process(b"cd \x1b[7m/some/path\x1b[27m");
        let theme = Theme::nord();
        let plain = screen.cell(0, 0).unwrap();
        assert_eq!(cell_background(&theme, &plain), theme.bg);
        let pasted = screen.cell(0, 3).unwrap();
        assert_eq!(cell_background(&theme, &pasted), theme.fg);
        let rows = build_rows(&screen, &theme, None);
        let span = rows[0].iter().find(|s| s.text.starts_with('/')).unwrap();
        assert_eq!(span.color, rgb(theme.bg));
    }
}
