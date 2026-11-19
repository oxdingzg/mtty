//! Host-agnostic key encoding: a neutral key + modifiers → PTY bytes.
//!
//! Each host maps its own event type (winit `KeyEvent`, egui events) into
//! [`KeyKind`]/[`Modifiers`] and calls [`encode_key`], so the encoding rules —
//! control bytes, modifier-aware navigation, Backspace variants, and the kitty
//! keyboard protocol (CSI-u) — live in exactly one place.

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Command (macOS ⌘) / Super / Win. Reserved for host shortcuts.
    pub sup: bool,
}

impl Modifiers {
    /// xterm modifier parameter: 1 + Shift(1) + Alt(2) + Ctrl(4).
    fn number(self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Char(char),
    Enter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    PageUp,
    PageDown,
    Insert,
    /// Function key F1–F12.
    F(u8),
    Other,
}

/// xterm encoding of a function key: SS3 for unmodified F1–F4, otherwise
/// `CSI 1;n P..S` / `CSI code[;n] ~`. `n` is the modifier parameter (1 = none).
fn function_key(f: u8, n: u8) -> Option<String> {
    let tilde = |code: u8| {
        if n > 1 {
            format!("\x1b[{code};{n}~")
        } else {
            format!("\x1b[{code}~")
        }
    };
    Some(match f {
        1..=4 => {
            let c = (b'P' + f - 1) as char;
            if n > 1 {
                format!("\x1b[1;{n}{c}")
            } else {
                format!("\x1bO{c}")
            }
        }
        5 => tilde(15),
        6..=10 => tilde(f + 11),
        11 | 12 => tilde(f + 12),
        _ => return None,
    })
}

/// Kitty keyboard protocol flags, as `CSI > flags u` sets them.
pub const KITTY_DISAMBIGUATE: u8 = 1;
pub const KITTY_REPORT_EVENTS: u8 = 2;
pub const KITTY_REPORT_ALTERNATE: u8 = 4;
pub const KITTY_REPORT_ALL_KEYS: u8 = 8;
pub const KITTY_REPORT_TEXT: u8 = 16;

/// Escape sequence options for a key press.
#[derive(Clone, Copy, Default)]
pub struct EncodeOpts {
    pub app_cursor: bool,
    pub bracketed: bool,
    /// Kitty keyboard protocol flags; 0 is off.
    pub kitty: u8,
    /// Event type when [`KITTY_REPORT_EVENTS`] is requested: 1 press,
    /// 2 repeat, 3 release (0 is treated as 1).
    pub event: u8,
    pub has_selection: bool,
}

/// Extra kitty key data read off the event: the unshifted key code, the
/// shifted key (while Shift is held) and the PC-101 base-layout key. `0` means
/// "not known", so the field is omitted.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct KittyAlternates {
    /// The unshifted key code in the active layout; 0 derives it from the key.
    pub unshifted: u32,
    /// The shifted key code, reported only while Shift is held; 0 otherwise.
    pub shifted: u32,
    /// The PC-101 base-layout key code; 0 when unknown or equal to the key.
    pub base: u32,
}

/// Which kitty sub-fields a sequence carries.
#[derive(Clone, Copy)]
struct KittySeq {
    report_alternate: bool,
    report_events: bool,
    report_text: bool,
    event: u8,
}

impl KittySeq {
    fn from_flags(flags: u8, event: u8) -> Self {
        Self {
            report_alternate: flags & KITTY_REPORT_ALTERNATE != 0,
            report_events: flags & KITTY_REPORT_EVENTS != 0,
            report_text: flags & KITTY_REPORT_TEXT != 0,
            event,
        }
    }
}

/// The `key-code[:shifted[:base]]` sub-field. A base-layout key with no shifted
/// key is sent with an empty shifted sub-field (`code::base`).
fn kitty_key_field(code: u32, alt: KittyAlternates, report_alternate: bool) -> String {
    let mut field = code.to_string();
    if report_alternate {
        let shifted = (alt.shifted != 0 && alt.shifted != code).then_some(alt.shifted);
        let base =
            (alt.base != 0 && alt.base != code && Some(alt.base) != shifted).then_some(alt.base);
        if let Some(shifted) = shifted {
            field.push(':');
            field.push_str(&shifted.to_string());
        }
        if let Some(base) = base {
            if shifted.is_none() {
                field.push(':');
            }
            field.push(':');
            field.push_str(&base.to_string());
        }
    }
    field
}

/// The `;modifiers[:event][;text]` tail; empty when everything is default.
fn kitty_params(n: u8, seq: KittySeq, text: Option<&str>) -> String {
    let text = text.filter(|text| !text.is_empty() && seq.report_text);
    if text.is_none() && !seq.report_events && n <= 1 {
        return String::new();
    }
    let modifiers = if seq.report_events {
        format!("{n}:{}", if seq.event == 0 { 1 } else { seq.event })
    } else {
        n.to_string()
    };
    match text {
        Some(text) => {
            let cps: Vec<String> = text.chars().map(|c| (c as u32).to_string()).collect();
            format!(";{modifiers};{}", cps.join(":"))
        }
        None => format!(";{modifiers}"),
    }
}

/// `CSI key-code ; modifiers u`, with the alternate and text sub-fields.
fn kitty_csi_u(
    code: u32,
    alt: KittyAlternates,
    n: u8,
    seq: KittySeq,
    text: Option<&str>,
) -> Vec<u8> {
    let field = kitty_key_field(code, alt, seq.report_alternate);
    format!("\x1b[{field}{}u", kitty_params(n, seq, text)).into_bytes()
}

/// `CSI code ; modifiers letter`; the leading `1` goes with no modifiers.
fn kitty_letter(code: u8, letter: char, n: u8, seq: KittySeq) -> Vec<u8> {
    let params = kitty_params(n, seq, None);
    if params.is_empty() {
        format!("\x1b[{letter}").into_bytes()
    } else {
        format!("\x1b[{code}{params}{letter}").into_bytes()
    }
}

/// `CSI code ; modifiers ~`.
fn kitty_tilde(code: u8, n: u8, seq: KittySeq) -> Vec<u8> {
    format!("\x1b[{code}{}~", kitty_params(n, seq, None)).into_bytes()
}

/// The kitty form of a functional key. Bare Enter, Tab and Backspace keep
/// their C0 bytes, so the caller handles those.
fn kitty_functional(kind: KeyKind, n: u8, seq: KittySeq) -> Option<Vec<u8>> {
    let out = match kind {
        KeyKind::Escape => kitty_csi_u(27, KittyAlternates::default(), n, seq, None),
        KeyKind::Enter => kitty_csi_u(13, KittyAlternates::default(), n, seq, None),
        KeyKind::Tab => kitty_csi_u(9, KittyAlternates::default(), n, seq, None),
        KeyKind::Backspace => kitty_csi_u(127, KittyAlternates::default(), n, seq, None),
        KeyKind::Up => kitty_letter(1, 'A', n, seq),
        KeyKind::Down => kitty_letter(1, 'B', n, seq),
        KeyKind::Right => kitty_letter(1, 'C', n, seq),
        KeyKind::Left => kitty_letter(1, 'D', n, seq),
        KeyKind::Home => kitty_letter(1, 'H', n, seq),
        KeyKind::End => kitty_letter(1, 'F', n, seq),
        KeyKind::Insert => kitty_tilde(2, n, seq),
        KeyKind::Delete => kitty_tilde(3, n, seq),
        KeyKind::PageUp => kitty_tilde(5, n, seq),
        KeyKind::PageDown => kitty_tilde(6, n, seq),
        // F3 is `13 ~` here; `CSI R` would clash with the cursor report.
        KeyKind::F(3) => kitty_tilde(13, n, seq),
        KeyKind::F(f) if (1..=4).contains(&f) => kitty_letter(1, (b'P' + f - 1) as char, n, seq),
        KeyKind::F(5) => kitty_tilde(15, n, seq),
        KeyKind::F(f) if (6..=10).contains(&f) => kitty_tilde(f + 11, n, seq),
        KeyKind::F(f) if f == 11 || f == 12 => kitty_tilde(f + 12, n, seq),
        _ => return None,
    };
    Some(out)
}

/// Plain text (a typed character / IME commit).
pub fn encode_text(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

/// Paste, honoring bracketed-paste mode and normalizing newlines to CR.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let body = text.replace('\n', "\r");
    if bracketed {
        format!("\x1b[200~{body}\x1b[201~").into_bytes()
    } else {
        body.into_bytes()
    }
}

/// Bytes for a paste whose only payload is an image the host wrote to
/// `MTTY_CLIPBOARD_FILE`. An empty bracketed paste is the signal an
/// application already treats as "read the clipboard", but ConPTY drops that
/// pair before the application sees it, so a Windows pane would receive no
/// input at all and silently ignore the image. The legacy Ctrl+V byte survives
/// ConPTY and asks the application to read the clipboard itself.
pub fn encode_image_paste(bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return Vec::new();
    }
    #[cfg(windows)]
    {
        vec![0x16]
    }
    #[cfg(not(windows))]
    {
        encode_paste("", true)
    }
}

/// Paste bytes for a terminal pane.
///
/// Windows runs panes under ConPTY, which turns a bracketed paste into key
/// presses: the markers are dropped and every line break arrives as Enter, so
/// a chat TUI submits each line instead of holding the text. When the pane's
/// application asked for bracketed paste, send the legacy Ctrl+V byte for a
/// multi-line paste instead; the application reads the clipboard itself (miao
/// does) and keeps the text as one draft. Single-line pastes, and applications
/// without bracketed paste, keep the text so shells and editors see it
/// unchanged.
pub fn encode_terminal_paste(text: &str, bracketed: bool) -> Vec<u8> {
    #[cfg(windows)]
    if bracketed && text.contains(['\n', '\r']) {
        return vec![0x16];
    }
    encode_paste(text, bracketed)
}

/// Encode a key press (special keys and modified keys; printable characters
/// arrive through [`encode_text`]).
pub fn encode_key(kind: KeyKind, mods: Modifiers, opts: EncodeOpts) -> Vec<u8> {
    encode_key_full(kind, mods, opts, KittyAlternates::default(), None)
}

/// Like [`encode_key`], with the kitty alternate-key and associated-text data
/// the widget read off the event.
pub fn encode_key_full(
    kind: KeyKind,
    mods: Modifiers,
    opts: EncodeOpts,
    alt: KittyAlternates,
    text: Option<&str>,
) -> Vec<u8> {
    // macOS Command+arrows are text-boundary navigation. Encode them as
    // Home/End so both legacy shells and keyboard-protocol TUIs can use them.
    if cfg!(target_os = "macos") && mods.sup && !mods.ctrl && !mods.alt {
        let boundary = match kind {
            KeyKind::Left => Some(KeyKind::Home),
            KeyKind::Right => Some(KeyKind::End),
            _ => None,
        };
        if let Some(boundary) = boundary {
            return encode_key_full(
                boundary,
                Modifiers { sup: false, ..mods },
                opts,
                KittyAlternates::default(),
                None,
            );
        }
    }
    // Other Cmd/Super combinations remain reserved for host shortcuts.
    if mods.sup {
        return Vec::new();
    }
    let mut out = Vec::new();

    // Kitty keyboard protocol: disambiguate escape codes, report every key as
    // an escape code, and carry event types, alternate keys and associated
    // text, as the requested flags ask.
    let flags = opts.kitty;
    if flags != 0 {
        let n = mods.number();
        let seq = KittySeq::from_flags(flags, opts.event);
        let disambiguate = (flags & KITTY_DISAMBIGUATE) != 0;
        let all_keys = (flags & KITTY_REPORT_ALL_KEYS) != 0;
        // The protocol code is the unshifted key in the active layout.
        let code = |c: char| {
            if alt.unshifted != 0 {
                alt.unshifted
            } else {
                c.to_ascii_lowercase() as u32
            }
        };
        if all_keys {
            if let KeyKind::Char(c) = kind {
                return kitty_csi_u(code(c), alt, n, seq, text);
            }
        }
        if disambiguate {
            if kind == KeyKind::Escape {
                return kitty_csi_u(27, alt, n, seq, None);
            }
            if let KeyKind::Char(c) = kind {
                if mods.ctrl {
                    return kitty_csi_u(code(c), alt, n, seq, text);
                }
            }
        }
        // Functional keys take the kitty forms, but bare Enter, Tab and
        // Backspace keep their C0 bytes.
        let bare = !(mods.shift || mods.alt || mods.ctrl);
        let c0 = matches!(kind, KeyKind::Enter | KeyKind::Tab | KeyKind::Backspace);
        if (disambiguate || all_keys) && !(c0 && bare) {
            if let Some(seq) = kitty_functional(kind, n, seq) {
                return seq;
            }
        }
    }

    // Ctrl combos (Unix control bytes). Ctrl+C with a selection is a copy.
    if mods.ctrl {
        if let KeyKind::Char(c) = kind {
            let c = c.to_ascii_uppercase();
            if ('@'..='_').contains(&c) {
                if c == 'C' && opts.has_selection {
                    return Vec::new();
                }
                out.push(c as u8 - 0x40);
                return out;
            }
        }
    }

    // Backspace variants: Ctrl+Backspace = ^W, Alt+Backspace = ESC DEL.
    if kind == KeyKind::Backspace {
        if mods.ctrl && !mods.alt {
            out.push(0x17);
            return out;
        }
        if mods.alt && !mods.ctrl {
            out.extend_from_slice(b"\x1b\x7f");
            return out;
        }
    }

    // Modifier-aware navigation (word movement, selection, …).
    if mods.shift || mods.alt || mods.ctrl {
        let n = mods.number();
        let seq = match kind {
            KeyKind::Up => Some(format!("\x1b[1;{n}A")),
            KeyKind::Down => Some(format!("\x1b[1;{n}B")),
            KeyKind::Right => Some(format!("\x1b[1;{n}C")),
            KeyKind::Left => Some(format!("\x1b[1;{n}D")),
            KeyKind::Home => Some(format!("\x1b[1;{n}H")),
            KeyKind::End => Some(format!("\x1b[1;{n}F")),
            KeyKind::Delete => Some(format!("\x1b[3;{n}~")),
            KeyKind::Insert => Some(format!("\x1b[2;{n}~")),
            KeyKind::F(f) => function_key(f, n),
            KeyKind::PageUp => Some(format!("\x1b[5;{n}~")),
            KeyKind::PageDown => Some(format!("\x1b[6;{n}~")),
            _ => None,
        };
        if let Some(seq) = seq {
            return seq.into_bytes();
        }
    }

    match kind {
        KeyKind::Enter => out.push(b'\r'),
        KeyKind::Backspace => out.push(0x7f),
        KeyKind::Tab => {
            if mods.shift {
                out.extend_from_slice(b"\x1b[Z");
            } else {
                out.push(b'\t');
            }
        }
        KeyKind::Escape => out.push(0x1b),
        KeyKind::Up => cursor(&mut out, b'A', opts.app_cursor),
        KeyKind::Down => cursor(&mut out, b'B', opts.app_cursor),
        KeyKind::Right => cursor(&mut out, b'C', opts.app_cursor),
        KeyKind::Left => cursor(&mut out, b'D', opts.app_cursor),
        KeyKind::Home => out.extend_from_slice(b"\x1b[H"),
        KeyKind::End => out.extend_from_slice(b"\x1b[F"),
        KeyKind::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyKind::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyKind::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyKind::Insert => out.extend_from_slice(b"\x1b[2~"),
        KeyKind::F(f) => {
            if let Some(seq) = function_key(f, 1) {
                out.extend_from_slice(seq.as_bytes());
            }
        }
        KeyKind::Char(_) | KeyKind::Other => {}
    }
    out
}

fn cursor(out: &mut Vec<u8>, c: u8, app_cursor: bool) {
    if app_cursor {
        out.extend_from_slice(&[0x1b, b'O', c]);
    } else {
        out.extend_from_slice(&[0x1b, b'[', c]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_letters_are_control_bytes() {
        let m = Modifiers {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(
            encode_key(KeyKind::Char('a'), m, EncodeOpts::default()),
            vec![1]
        );
        assert_eq!(
            encode_key(KeyKind::Char('c'), m, EncodeOpts::default()),
            vec![3]
        );
    }

    #[test]
    fn cmd_is_not_sent() {
        let m = Modifiers {
            sup: true,
            ..Default::default()
        };
        assert!(encode_key(KeyKind::Char('a'), m, EncodeOpts::default()).is_empty());
    }

    #[test]
    fn command_arrows_navigate_boundaries_on_macos() {
        for (kind, end) in [(KeyKind::Left, 'H'), (KeyKind::Right, 'F')] {
            for shift in [false, true] {
                for kitty in [0, KITTY_DISAMBIGUATE | KITTY_REPORT_EVENTS] {
                    for event in [1, 2, 3] {
                        let bytes = encode_key(
                            kind,
                            Modifiers {
                                sup: true,
                                shift,
                                ..Default::default()
                            },
                            EncodeOpts {
                                kitty,
                                event,
                                ..Default::default()
                            },
                        );
                        if !cfg!(target_os = "macos") {
                            assert!(bytes.is_empty());
                            continue;
                        }
                        let modifier = if shift { 2 } else { 1 };
                        let expected = if kitty != 0 {
                            format!("\x1b[1;{modifier}:{event}{end}")
                        } else if shift {
                            format!("\x1b[1;2{end}")
                        } else {
                            format!("\x1b[{end}")
                        };
                        assert_eq!(bytes, expected.as_bytes());
                    }
                }
            }
            for mods in [
                Modifiers {
                    sup: true,
                    ctrl: true,
                    ..Default::default()
                },
                Modifiers {
                    sup: true,
                    alt: true,
                    ..Default::default()
                },
            ] {
                assert!(encode_key(kind, mods, EncodeOpts::default()).is_empty());
            }
        }
    }

    #[test]
    fn shift_tab_and_arrows() {
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            encode_key(KeyKind::Tab, shift, EncodeOpts::default()),
            b"\x1b[Z"
        );
        let ctrl = Modifiers {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(
            encode_key(KeyKind::Right, ctrl, EncodeOpts::default()),
            b"\x1b[1;5C"
        );
    }

    #[test]
    fn function_keys_and_insert_use_xterm_sequences() {
        let none = Modifiers::default();
        let opts = EncodeOpts::default();
        assert_eq!(encode_key(KeyKind::F(1), none, opts), b"\x1bOP");
        assert_eq!(encode_key(KeyKind::F(4), none, opts), b"\x1bOS");
        assert_eq!(encode_key(KeyKind::F(5), none, opts), b"\x1b[15~");
        assert_eq!(encode_key(KeyKind::F(6), none, opts), b"\x1b[17~");
        assert_eq!(encode_key(KeyKind::F(10), none, opts), b"\x1b[21~");
        assert_eq!(encode_key(KeyKind::F(11), none, opts), b"\x1b[23~");
        assert_eq!(encode_key(KeyKind::F(12), none, opts), b"\x1b[24~");
        assert_eq!(encode_key(KeyKind::Insert, none, opts), b"\x1b[2~");
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(encode_key(KeyKind::F(1), shift, opts), b"\x1b[1;2P");
        assert_eq!(encode_key(KeyKind::F(5), shift, opts), b"\x1b[15;2~");
        assert_eq!(encode_key(KeyKind::Insert, shift, opts), b"\x1b[2;2~");
    }

    #[test]
    fn paste_normalizes_newlines() {
        assert_eq!(encode_paste("a\nb", false), b"a\rb");
        assert_eq!(encode_paste("a", true), b"\x1b[200~a\x1b[201~");
    }

    #[test]
    fn an_image_paste_reaches_a_windows_pane_as_ctrl_v() {
        assert!(encode_image_paste(false).is_empty());
        #[cfg(windows)]
        assert_eq!(encode_image_paste(true), b"");
        #[cfg(not(windows))]
        assert_eq!(encode_image_paste(true), b"[200~[201~");
    }

    #[test]
    fn a_multiline_paste_survives_conpty_as_ctrl_v() {
        #[cfg(windows)]
        {
            assert_eq!(encode_terminal_paste("a\r\nb", true), b"\x16");
            assert_eq!(encode_terminal_paste("a\nb", true), b"\x16");
            assert_eq!(
                encode_terminal_paste("one line", true),
                b"[200~one line[201~"
            );
            assert_eq!(encode_terminal_paste("a\nb", false), b"a\rb");
        }
        #[cfg(not(windows))]
        assert_eq!(
            encode_terminal_paste("a\nb", true),
            b"\x1b[200~a\rb\x1b[201~"
        );
    }

    fn kitty(flags: u8) -> EncodeOpts {
        EncodeOpts {
            kitty: flags,
            ..Default::default()
        }
    }

    #[test]
    fn kitty_disambiguates_escape_ctrl_and_function_keys() {
        let opts = kitty(KITTY_DISAMBIGUATE);
        assert_eq!(
            encode_key(KeyKind::Escape, Modifiers::default(), opts),
            b"\x1b[27u"
        );
        assert_eq!(
            encode_key(KeyKind::F(3), Modifiers::default(), opts),
            b"\x1b[13~"
        );
        assert_eq!(
            encode_key(KeyKind::F(1), Modifiers::default(), opts),
            b"\x1b[P"
        );
        let ctrl = Modifiers {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(encode_key(KeyKind::Char('a'), ctrl, opts), b"\x1b[97;5u");
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(encode_key(KeyKind::Tab, shift, opts), b"\x1b[9;2u");
        // Bare Enter, Tab and Backspace keep their C0 bytes.
        assert_eq!(
            encode_key(KeyKind::Enter, Modifiers::default(), opts),
            b"\r"
        );
        assert_eq!(encode_key(KeyKind::Tab, Modifiers::default(), opts), b"\t");
    }

    #[test]
    fn kitty_reports_event_types_when_asked() {
        let ctrl = Modifiers {
            ctrl: true,
            ..Default::default()
        };
        let opts = EncodeOpts {
            kitty: KITTY_DISAMBIGUATE | KITTY_REPORT_EVENTS,
            event: 2,
            ..Default::default()
        };
        assert_eq!(encode_key(KeyKind::Char('a'), ctrl, opts), b"\x1b[97;5:2u");
        let opts = EncodeOpts { event: 3, ..opts };
        assert_eq!(encode_key(KeyKind::Char('a'), ctrl, opts), b"\x1b[97;5:3u");
    }

    #[test]
    fn kitty_all_keys_encodes_text_keys() {
        let opts = kitty(KITTY_REPORT_ALL_KEYS);
        assert_eq!(
            encode_key(KeyKind::Char('a'), Modifiers::default(), opts),
            b"\x1b[97u"
        );
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(encode_key(KeyKind::Char('A'), shift, opts), b"\x1b[97;2u");
    }

    #[test]
    fn kitty_reports_alternate_keys_and_text() {
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        let opts = kitty(KITTY_REPORT_ALL_KEYS | KITTY_REPORT_ALTERNATE | KITTY_REPORT_TEXT);
        let letter = KittyAlternates {
            unshifted: 97,
            shifted: 65,
            base: 97,
        };
        assert_eq!(
            encode_key_full(KeyKind::Char('A'), shift, opts, letter, Some("A")),
            b"\x1b[97:65;2;65u"
        );
        // A base-layout key with no shifted key uses the empty sub-field, so a
        // Cyrillic ctrl+с reaches the program as ctrl+c.
        let ctrl = Modifiers {
            ctrl: true,
            ..Default::default()
        };
        let cyrillic = KittyAlternates {
            unshifted: 1089,
            shifted: 0,
            base: 99,
        };
        assert_eq!(
            encode_key_full(KeyKind::Char('с'), ctrl, opts, cyrillic, None),
            "\x1b[1089::99;5u".as_bytes()
        );
        // Flag 4 alone reports the shifted key; no text without flag 16.
        let opts = kitty(KITTY_REPORT_ALL_KEYS | KITTY_REPORT_ALTERNATE);
        let digit = KittyAlternates {
            unshifted: 50,
            shifted: 64,
            base: 50,
        };
        assert_eq!(
            encode_key_full(KeyKind::Char('@'), shift, opts, digit, Some("@")),
            b"\x1b[50:64;2u"
        );
        // Without the alternate/text flags the sequence is unchanged.
        let opts = kitty(KITTY_REPORT_ALL_KEYS);
        assert_eq!(
            encode_key_full(KeyKind::Char('A'), shift, opts, letter, Some("A")),
            b"\x1b[97;2u"
        );
    }

    #[test]
    fn kitty_associated_text_keeps_a_modifier_field() {
        let opts = kitty(KITTY_REPORT_ALL_KEYS | KITTY_REPORT_TEXT);
        let digit = KittyAlternates {
            unshifted: 50,
            shifted: 0,
            base: 50,
        };
        assert_eq!(
            encode_key_full(
                KeyKind::Char('2'),
                Modifiers::default(),
                opts,
                digit,
                Some("2")
            ),
            b"\x1b[50;1;50u"
        );
    }

    #[test]
    fn image_only_clipboard_still_notifies_bracketed_paste_application() {
        assert_eq!(encode_paste("", true), b"\x1b[200~\x1b[201~");
        assert!(encode_paste("", false).is_empty());
    }
}
