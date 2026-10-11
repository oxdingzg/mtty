//! Escape-sequence boundaries and private modes (ADR 0041 §4).
//!
//! The host never emulates a screen, but it must cut output only where a
//! terminal parser is back in its ground state, so a client that rebuilds a
//! screen from a snapshot taken at a frame end resumes on a clean boundary.
//! It also follows the private modes, so a client that cannot replay the
//! output that set them (it fell out of the ring) still learns them.
//!
//! The states follow vte's: CAN/SUB abort a sequence, ESC starts a new one
//! (and ends a string), C0 controls inside a CSI are executed in place.

/// DEC private modes the host follows.
pub const TRACKED_MODES: [u16; 13] = [
    1, 6, 7, 25, 1000, 1002, 1003, 1004, 1005, 1006, 1007, 1049, 2004,
];

/// Private modes, the keypad mode and the kitty keyboard flags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Modes {
    /// On/off per entry of [`TRACKED_MODES`].
    private: [bool; TRACKED_MODES.len()],
    keypad: bool,
    /// Kitty keyboard stacks of the main and the alternate screen.
    kitty: [Vec<u8>; 2],
}

impl Default for Modes {
    fn default() -> Self {
        let mut private = [false; TRACKED_MODES.len()];
        // Line wrap (7), the visible cursor (25) and alternate scroll (1007)
        // start on.
        for (i, m) in TRACKED_MODES.iter().enumerate() {
            private[i] = matches!(m, 7 | 25 | 1007);
        }
        Self {
            private,
            keypad: false,
            kitty: [Vec::new(), Vec::new()],
        }
    }
}

impl Modes {
    pub fn get(&self, mode: u16) -> Option<bool> {
        let i = TRACKED_MODES.iter().position(|&m| m == mode)?;
        Some(self.private[i])
    }

    pub fn alt_screen(&self) -> bool {
        self.get(1049) == Some(true)
    }

    pub fn keypad(&self) -> bool {
        self.keypad
    }

    /// The active screen's kitty keyboard flags.
    pub fn kitty_flags(&self) -> u8 {
        self.kitty[usize::from(self.alt_screen())]
            .last()
            .copied()
            .unwrap_or(0)
    }

    fn set_private(&mut self, mode: u16, on: bool) {
        let Some(i) = TRACKED_MODES.iter().position(|&m| m == mode) else {
            return;
        };
        // The mouse protocols are exclusive, as in the terminal.
        if on && matches!(mode, 1000 | 1002 | 1003) {
            for other in [1000, 1002, 1003] {
                self.set_private(other, false);
            }
        }
        self.private[i] = on;
    }

    /// The sequences that put a terminal into these modes, the alternate
    /// screen excepted (switching screens is the caller's).
    pub fn sequences(&self) -> String {
        let mut out = String::new();
        let mut on = Vec::new();
        for (i, &m) in TRACKED_MODES.iter().enumerate() {
            if m == 1049 {
                continue;
            }
            if self.private[i] {
                on.push(m);
            } else {
                out.push_str(&format!("\x1b[?{m}l"));
            }
        }
        for m in on {
            out.push_str(&format!("\x1b[?{m}h"));
        }
        out.push_str(if self.keypad { "\x1b=" } else { "\x1b>" });
        let flags = self.kitty_flags();
        if flags != 0 {
            out.push_str(&format!("\x1b[>{flags}u"));
        }
        out
    }

    /// `[count][(mode u16 LE, on u8)…][keypad][kitty flags]`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![TRACKED_MODES.len() as u8];
        for (i, m) in TRACKED_MODES.iter().enumerate() {
            out.extend_from_slice(&m.to_le_bytes());
            out.push(u8::from(self.private[i]));
        }
        out.push(u8::from(self.keypad));
        out.push(self.kitty_flags());
        out
    }

    /// Decode [`Modes::encode`]; unknown modes from a newer host are ignored.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (&count, mut rest) = bytes.split_first()?;
        let mut modes = Modes::default();
        for _ in 0..count {
            if rest.len() < 3 {
                return None;
            }
            let mode = u16::from_le_bytes([rest[0], rest[1]]);
            if let Some(i) = TRACKED_MODES.iter().position(|&m| m == mode) {
                modes.private[i] = rest[2] != 0;
            }
            rest = &rest[3..];
        }
        let [keypad, kitty]: [u8; 2] = rest.get(..2)?.try_into().ok()?;
        modes.keypad = keypad != 0;
        let screen = usize::from(modes.alt_screen());
        if kitty != 0 {
            modes.kitty[screen].push(kitty);
        }
        Some(modes)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    /// Inside a UTF-8 character, with this many continuation bytes to come.
    Utf8(u8),
    Escape,
    EscapeIntermediate,
    Csi,
    /// OSC, DCS, SOS, PM, APC: a string until ST (and BEL for OSC).
    String {
        osc: bool,
        utf8_remaining: u8,
    },
}

/// The longest CSI kept for mode tracking; longer ones are only skipped.
const MAX_CSI: usize = 64;

#[derive(Clone, Debug)]
pub struct Scanner {
    state: State,
    csi: Vec<u8>,
    modes: Modes,
}

impl Default for Scanner {
    fn default() -> Self {
        Self {
            state: State::Ground,
            csi: Vec::new(),
            modes: Modes::default(),
        }
    }
}

impl Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn at_boundary(&self) -> bool {
        self.state == State::Ground
    }

    pub fn modes(&self) -> &Modes {
        &self.modes
    }

    /// Scan `bytes`; returns the length of the longest prefix that ends on a
    /// boundary (0 when none does).
    pub fn feed(&mut self, bytes: &[u8]) -> usize {
        let mut boundary = 0;
        for (i, &b) in bytes.iter().enumerate() {
            self.byte(b);
            if self.state == State::Ground {
                boundary = i + 1;
            }
        }
        boundary
    }

    fn byte(&mut self, b: u8) {
        // CAN and SUB abort any sequence; ESC starts a new one (ending a
        // string), except inside a UTF-8 character, which it ends.
        if self.state != State::Ground {
            match b {
                0x18 | 0x1a => {
                    self.state = State::Ground;
                    return;
                }
                0x1b => {
                    self.state = State::Escape;
                    return;
                }
                _ => {}
            }
        }
        match self.state {
            State::Ground => self.ground(b),
            State::Utf8(n) => {
                if (0x80..=0xbf).contains(&b) {
                    self.state = if n == 1 {
                        State::Ground
                    } else {
                        State::Utf8(n - 1)
                    };
                } else {
                    // A broken character: the byte starts afresh.
                    self.state = State::Ground;
                    self.ground(b);
                }
            }
            State::Escape => match b {
                b'[' => {
                    self.csi.clear();
                    self.state = State::Csi;
                }
                b']' => {
                    self.state = State::String {
                        osc: true,
                        utf8_remaining: 0,
                    }
                }
                b'P' | b'X' | b'^' | b'_' => {
                    self.state = State::String {
                        osc: false,
                        utf8_remaining: 0,
                    }
                }
                0x20..=0x2f => self.state = State::EscapeIntermediate,
                0x00..=0x1f => {}
                _ => {
                    match b {
                        b'=' => self.modes.keypad = true,
                        b'>' => self.modes.keypad = false,
                        b'c' => self.modes = Modes::default(),
                        _ => {}
                    }
                    self.state = State::Ground;
                }
            },
            State::EscapeIntermediate => {
                if (0x30..=0x7e).contains(&b) {
                    self.state = State::Ground;
                }
            }
            State::Csi => match b {
                0x40..=0x7e => {
                    self.csi_dispatch(b);
                    self.state = State::Ground;
                }
                0x20..=0x3f if self.csi.len() < MAX_CSI => self.csi.push(b),
                _ => {}
            },
            State::String {
                osc,
                utf8_remaining,
            } => {
                if utf8_remaining > 0 && (0x80..=0xbf).contains(&b) {
                    self.state = State::String {
                        osc,
                        utf8_remaining: utf8_remaining - 1,
                    };
                    return;
                }
                // A standalone C1 ST ends a string, but the same byte inside
                // a UTF-8 character must not create a replay boundary.
                self.state = if b == 0x9c || (osc && b == 0x07) {
                    State::Ground
                } else {
                    State::String {
                        osc,
                        utf8_remaining: match b {
                            0xc2..=0xdf => 1,
                            0xe0..=0xef => 2,
                            0xf0..=0xf4 => 3,
                            _ => 0,
                        },
                    }
                };
            }
        }
    }

    fn ground(&mut self, b: u8) {
        self.state = match b {
            0x1b => State::Escape,
            0xc2..=0xdf => State::Utf8(1),
            0xe0..=0xef => State::Utf8(2),
            0xf0..=0xf4 => State::Utf8(3),
            _ => State::Ground,
        };
    }

    fn csi_dispatch(&mut self, last: u8) {
        if self.csi.len() >= MAX_CSI {
            return;
        }
        let (prefix, params) = match self.csi.first() {
            Some(&p @ (b'?' | b'>' | b'<' | b'=')) => (Some(p), &self.csi[1..]),
            _ => (None, &self.csi[..]),
        };
        let params: Vec<Option<u16>> = std::str::from_utf8(params)
            .unwrap_or("")
            .split(';')
            .map(|p| p.parse().ok())
            .collect();
        match (prefix, last) {
            (Some(b'?'), b'h' | b'l') => {
                for mode in params.iter().flatten() {
                    let on = last == b'h';
                    if *mode == 1049 && self.modes.alt_screen() != on {
                        // The alternate screen has its own kitty stack,
                        // emptied on entry as in the terminal.
                        if on {
                            self.modes.kitty[1].clear();
                        }
                    }
                    self.modes.set_private(*mode, on);
                }
            }
            (Some(b'>'), b'u') => {
                let flags = params.first().copied().flatten().unwrap_or(0) as u8;
                let screen = usize::from(self.modes.alt_screen());
                self.modes.kitty[screen].push(flags);
            }
            (Some(b'<'), b'u') => {
                let n = params.first().copied().flatten().unwrap_or(1) as usize;
                let screen = usize::from(self.modes.alt_screen());
                let stack = &mut self.modes.kitty[screen];
                stack.truncate(stack.len().saturating_sub(n));
            }
            (Some(b'='), b'u') => {
                let flags = params.first().copied().flatten().unwrap_or(0) as u8;
                let how = params.get(1).copied().flatten().unwrap_or(1);
                let screen = usize::from(self.modes.alt_screen());
                let stack = &mut self.modes.kitty[screen];
                let current = stack.last().copied().unwrap_or(0);
                let new = match how {
                    2 => current | flags,
                    3 => current & !flags,
                    _ => flags,
                };
                match stack.last_mut() {
                    Some(top) => *top = new,
                    None => stack.push(new),
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn unicode_strings_only_end_at_a_standalone_c1_st() {
        for intro in [b"\x1b]2;".as_slice(), b"\x1bP", b"\x1b_"] {
            let mut bytes = intro.to_vec();
            bytes.extend_from_slice("相机功能 Ĝ 😜".as_bytes());
            bytes.push(0x9c);
            let mut scanner = Scanner::new();
            for (index, byte) in bytes.iter().enumerate() {
                let boundary = scanner.feed(std::slice::from_ref(byte));
                assert_eq!(boundary, usize::from(index + 1 == bytes.len()));
                assert_eq!(scanner.at_boundary(), index + 1 == bytes.len());
            }
            scanner.feed(b"\x1b[?2004h");
            assert_eq!(scanner.modes().get(2004), Some(true));
        }
    }

    use super::*;

    fn boundaries(bytes: &[u8]) -> Vec<usize> {
        let mut s = Scanner::new();
        (0..bytes.len())
            .filter(|&i| {
                s.byte(bytes[i]);
                s.at_boundary()
            })
            .map(|i| i + 1)
            .collect()
    }

    #[test]
    fn sequences_and_characters_are_atomic() {
        assert_eq!(boundaries(b"ab"), [1, 2]);
        assert_eq!(boundaries(b"\x1b[31mx"), [5, 6]);
        assert_eq!(boundaries("é中".as_bytes()), [2, 5]);
        assert_eq!(boundaries(b"\x1b]0;title\x07!"), [10, 11]);
        assert_eq!(boundaries(b"\x1b]0;t\x1b\\!"), [7, 8]);
        assert_eq!(boundaries(b"\x1bPq#0\x1b\\"), [7]);
        assert_eq!(boundaries(b"\x1b(0q"), [3, 4]);
        // Controls inside a CSI run in place; CAN aborts; ESC restarts.
        assert_eq!(boundaries(b"\x1b[1\r;2H"), [7]);
        assert_eq!(boundaries(b"\x1b[12\x18x"), [5, 6]);
        assert_eq!(boundaries(b"\x1b[1\x1b[2J"), [7]);
        // A broken UTF-8 character ends where the next one starts.
        assert_eq!(boundaries(b"\xe4a"), [2]);
    }

    #[test]
    fn feed_returns_the_last_boundary() {
        let mut s = Scanner::new();
        assert_eq!(s.feed(b"ok\x1b[3"), 2);
        assert!(!s.at_boundary());
        assert_eq!(s.feed(b"1mx"), 3);
        assert_eq!(s.feed("中".as_bytes().get(..1).unwrap()), 0);
    }

    #[test]
    fn private_modes_are_followed() {
        let mut s = Scanner::new();
        s.feed(b"\x1b[?1049h\x1b[?2004;1h\x1b[?1000h\x1b[?1003h\x1b=\x1b[?25l");
        let m = s.modes();
        assert!(m.alt_screen() && m.keypad());
        assert_eq!(m.get(2004), Some(true));
        assert_eq!(m.get(1), Some(true));
        assert_eq!(m.get(1000), Some(false), "mouse protocols are exclusive");
        assert_eq!(m.get(1003), Some(true));
        assert_eq!(m.get(25), Some(false));
        s.feed(b"\x1bc");
        assert_eq!(s.modes(), &Modes::default(), "RIS resets");
    }

    #[test]
    fn kitty_stacks_per_screen() {
        let mut s = Scanner::new();
        s.feed(b"\x1b[>1u");
        assert_eq!(s.modes().kitty_flags(), 1);
        s.feed(b"\x1b[?1049h");
        assert_eq!(s.modes().kitty_flags(), 0, "the alternate screen's own");
        s.feed(b"\x1b[>3u\x1b[=4;2u");
        assert_eq!(s.modes().kitty_flags(), 7);
        s.feed(b"\x1b[?1049l");
        assert_eq!(s.modes().kitty_flags(), 1);
        s.feed(b"\x1b[<u");
        assert_eq!(s.modes().kitty_flags(), 0);
    }

    #[test]
    fn modes_round_trip_and_replay() {
        let mut s = Scanner::new();
        s.feed(b"\x1b[?1h\x1b[?2004h\x1b[?1002h\x1b=\x1b[>5u");
        let m = s.modes().clone();
        let decoded = Modes::decode(&m.encode()).unwrap();
        assert_eq!(decoded.encode(), m.encode());
        // Replaying the sequences reproduces the modes.
        let mut again = Scanner::new();
        again.feed(m.sequences().as_bytes());
        assert_eq!(again.modes().encode(), m.encode());
        assert!(Modes::decode(&[3, 1]).is_none(), "truncated");
    }
}
