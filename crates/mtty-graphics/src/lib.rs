//! Inline terminal graphics: a stateful scanner that pulls Sixel (DCS), Kitty
//! (APC `G`) and iTerm2 (`OSC 1337;File=`) payloads out of a raw PTY byte
//! stream, plus decoders for each. The scanner is protocol-agnostic about the
//! underlying VT parser: it only isolates the sequences so the caller can feed
//! ordinary text to the parser and render the images itself.
//!
//! Everything is bounded: an unterminated sequence is flushed back as text once
//! it exceeds [`MAX_SEQUENCE`], and decoders cap the pixel count.

pub mod kitty;
pub mod sixel;

/// Largest graphics sequence held while waiting for its terminator.
pub const MAX_SEQUENCE: usize = 32 * 1024 * 1024;

/// A decoded, straight (non-premultiplied) RGBA image.
#[derive(Clone, Debug)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    pub fn pixel_count(&self) -> usize {
        self.width as usize * self.height as usize
    }
}

/// A single Kitty APC command.
#[derive(Clone, Debug, PartialEq)]
pub struct KittyCmd {
    /// Action: `t`/`T` transfer, `p` put, `d` delete, `q` query.
    pub action: char,
    pub id: Option<u64>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    /// `C=0` means "move the cursor" (default is not to move it).
    pub move_cursor: bool,
    pub z: i32,
    /// `X`/`Y`: pixel offsets from the cursor cell.
    pub x: i32,
    pub y: i32,
    /// Format: 24 = RGB, 32 = RGBA, 100 = PNG.
    pub format: u32,
    /// `o=z`: payload is zlib-compressed.
    pub compressed: bool,
    /// `m=1`: more chunks follow.
    pub more: bool,
    /// `s=WxH` pixel size (needed for raw formats).
    pub size: Option<(u32, u32)>,
    /// `d` (delete what): a = all, i = by id, p = at cell, c/r = column/row, z = z-index.
    pub delete: Option<char>,
    /// `x`/`y`: cell coordinates (viewport-relative) for `a=d,d=p/c/r`.
    pub cell_x: Option<u16>,
    pub cell_y: Option<u16>,
    /// `a=a` animation state (`s`) and loop count (`c`).
    pub state: Option<u32>,
    pub loops: Option<u32>,
    /// The `;`-separated payload (raw bytes; usually base64 text).
    pub data: Vec<u8>,
}

/// A graphics command extracted from the stream.
#[derive(Clone, Debug, PartialEq)]
pub enum Graphic {
    Sixel { transparent: bool, data: Vec<u8> },
    Kitty(KittyCmd),
    Iterm2 { name: Option<String>, data: Vec<u8> },
}

/// Scanner output, in stream order.
#[derive(Clone, Debug, PartialEq)]
pub enum Segment {
    /// Ordinary bytes; feed these to the VT parser.
    Text(Vec<u8>),
    /// An extracted graphics command (do **not** feed to the VT parser).
    Graphics(Graphic),
}

impl Segment {
    pub fn text(&self) -> Option<&[u8]> {
        match self {
            Segment::Text(b) => Some(b),
            _ => None,
        }
    }
}

/// Borrowed stream events. Text and OSC slices are valid only during the
/// callback; graphics commands own their decoded protocol parameters/payload.
/// OSC is a notification followed by either Text or Graphics for that sequence.
pub enum StreamEvent<'a> {
    Text(&'a [u8]),
    /// Standalone C1 ST. UTF-8 VT parsers may need its ESC-ST equivalent.
    C1Terminator(&'a [u8]),
    Graphics(Graphic),
    Osc(&'a [u8]),
    CursorReport,
}

/// Streaming scanner for graphics sequences and host-side control notifications.
/// Only an incomplete control sequence is buffered. Ordinary output is borrowed
/// directly, and each new fragment of a pending sequence is scanned once.
#[derive(Default)]
pub struct Scanner {
    buf: Vec<u8>,
    /// Continuations still pending in a string payload, including across feeds.
    utf8_remaining: u8,
    #[cfg(test)]
    inspected: usize,
}

const RETAINED_BUFFER_BYTES: usize = 64 * 1024;
const CURSOR_QUERY: &[u8] = b"\x1b[6n";

struct StringEnd {
    end: usize,
    len: usize,
    terminated: bool,
}

impl Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Owned compatibility API. Hosts should use `feed_with` to avoid copying
    /// ordinary terminal output and to receive OSC/DSR notifications in order.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Segment> {
        let mut out = Vec::new();
        self.feed_with(bytes, |event| match event {
            StreamEvent::Text(text) | StreamEvent::C1Terminator(text) => {
                out.push(Segment::Text(text.to_vec()))
            }
            StreamEvent::Graphics(graphic) => out.push(Segment::Graphics(graphic)),
            StreamEvent::CursorReport => out.push(Segment::Text(CURSOR_QUERY.to_vec())),
            StreamEvent::Osc(_) => {}
        });
        out
    }

    /// Visit complete events in stream order without allocating for plain text.
    pub fn feed_with(&mut self, mut bytes: &[u8], mut emit: impl FnMut(StreamEvent<'_>)) {
        while !bytes.is_empty() {
            if !self.buf.is_empty() {
                // A trailing ESC may be the introducer of a split sequence.
                if self.buf.len() == 1 {
                    if matches!(bytes[0], b'P' | b'_' | b']' | b'[') {
                        self.buf.push(bytes[0]);
                        bytes = &bytes[1..];
                    } else {
                        emit(StreamEvent::Text(&self.buf));
                        self.reset();
                        continue;
                    }
                }
                if self.buf[1] == b'[' {
                    // Only DSR is intercepted; other CSI stays in the VT parser.
                    while self.buf.len() < CURSOR_QUERY.len() && !bytes.is_empty() {
                        if bytes[0] != CURSOR_QUERY[self.buf.len()] {
                            break;
                        }
                        self.buf.push(bytes[0]);
                        bytes = &bytes[1..];
                    }
                    if self.buf.len() == CURSOR_QUERY.len() {
                        emit(StreamEvent::CursorReport);
                        self.reset();
                    } else if !bytes.is_empty() {
                        emit(StreamEvent::Text(&self.buf));
                        self.reset();
                    }
                    continue;
                }
                // Search only the new bytes, including ST split at ESC | '\'.
                // Never rescan the accumulated image payload.
                let is_osc = self.buf[1] == b']';
                let terminator = if self.buf.last() == Some(&0x1b) {
                    if bytes.first() == Some(&b'\\') {
                        Some(StringEnd {
                            end: 0,
                            len: 1,
                            terminated: true,
                        })
                    } else {
                        // A split ESC starts a new control sequence when it is
                        // not ST. Keep that ESC for the next scanner iteration.
                        emit(StreamEvent::Text(&self.buf[..self.buf.len() - 1]));
                        self.reset();
                        self.buf.push(0x1b);
                        continue;
                    }
                } else {
                    find_terminator(bytes, 0, is_osc, &mut self.utf8_remaining)
                };
                #[cfg(test)]
                {
                    self.inspected += terminator
                        .as_ref()
                        .map_or(bytes.len(), |end| end.end + end.len);
                }
                if let Some(StringEnd {
                    end,
                    len,
                    terminated,
                }) = terminator
                {
                    if terminated && self.buf.len().saturating_add(end + len) <= MAX_SEQUENCE {
                        self.append(&bytes[..end + len]);
                        emit_sequence(&self.buf, &mut emit);
                    } else {
                        emit(StreamEvent::Text(&self.buf));
                        emit(StreamEvent::Text(&bytes[..end + len]));
                    }
                    self.reset();
                    bytes = &bytes[end + len..];
                } else {
                    if self.buf.len().saturating_add(bytes.len()) > MAX_SEQUENCE {
                        emit(StreamEvent::Text(&self.buf));
                        emit(StreamEvent::Text(bytes));
                        self.reset();
                    } else {
                        self.append(bytes);
                    }
                    return;
                }
                continue;
            }

            let Some(start) = find_intro(bytes) else {
                emit(StreamEvent::Text(bytes));
                return;
            };
            if start > 0 {
                emit(StreamEvent::Text(&bytes[..start]));
                bytes = &bytes[start..];
            }
            if bytes.len() == 1 {
                self.buf.push(0x1b);
                return;
            }
            if bytes[1] == b'[' {
                if bytes.len() >= CURSOR_QUERY.len() {
                    emit(StreamEvent::CursorReport);
                    bytes = &bytes[CURSOR_QUERY.len()..];
                } else {
                    self.append(bytes);
                    return;
                }
                continue;
            }
            if let Some(StringEnd {
                end,
                len,
                terminated,
            }) = find_terminator(bytes, 2, bytes[1] == b']', &mut self.utf8_remaining)
            {
                if terminated && end + len <= MAX_SEQUENCE {
                    emit_sequence(&bytes[..end + len], &mut emit);
                } else {
                    emit(StreamEvent::Text(&bytes[..end + len]));
                }
                self.reset();
                bytes = &bytes[end + len..];
            } else {
                #[cfg(test)]
                {
                    self.inspected += bytes.len();
                }
                if bytes.len() > MAX_SEQUENCE {
                    emit(StreamEvent::Text(bytes));
                    self.reset();
                } else {
                    self.append(bytes);
                }
                return;
            }
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        let needed = self.buf.len() + bytes.len();
        if needed > self.buf.capacity() {
            let capacity = needed
                .max(self.buf.capacity().saturating_mul(2))
                .min(MAX_SEQUENCE);
            self.buf.reserve_exact(capacity - self.buf.len());
        }
        self.buf.extend_from_slice(bytes);
    }

    fn reset(&mut self) {
        self.utf8_remaining = 0;
        if self.buf.capacity() > RETAINED_BUFFER_BYTES {
            self.buf = Vec::new();
        } else {
            self.buf.clear();
        }
    }
}

fn emit_sequence(sequence: &[u8], emit: &mut impl FnMut(StreamEvent<'_>)) {
    let terminator_len = if sequence.ends_with(b"\x1b\\") { 2 } else { 1 };
    let payload = &sequence[2..sequence.len() - terminator_len];
    if sequence[1] == b']' {
        emit(StreamEvent::Osc(payload));
    }
    match classify(payload) {
        Some(graphic) => emit(StreamEvent::Graphics(graphic)),
        None if sequence.ends_with(b"\x9c") => {
            emit(StreamEvent::Text(&sequence[..sequence.len() - 1]));
            emit(StreamEvent::C1Terminator(&sequence[sequence.len() - 1..]));
        }
        None => emit(StreamEvent::Text(sequence)),
    }
}

/// Find a supported introducer, a partial DSR query, or a trailing ESC.
fn find_intro(s: &[u8]) -> Option<usize> {
    let mut from = 0;
    while let Some(offset) = memchr::memchr(0x1b, &s[from..]) {
        let k = from + offset;
        match s.get(k + 1) {
            None | Some(b'P' | b'_' | b']') => return Some(k),
            Some(b'[') if s[k..].starts_with(CURSOR_QUERY) || CURSOR_QUERY.starts_with(&s[k..]) => {
                return Some(k);
            }
            _ => from = k + 1,
        }
    }
    None
}

/// Find the end of a sequence body starting at `from`. Returns `(end, term_len)`
/// where `end` is exclusive of the terminator or cancellation. A new ESC
/// sequence cancels the old string without consuming its introducer.
fn find_terminator(
    s: &[u8],
    from: usize,
    is_osc: bool,
    utf8_remaining: &mut u8,
) -> Option<StringEnd> {
    let mut k = from;
    while k < s.len() {
        // C1 ST (0x9c) is also a UTF-8 continuation byte, for example in
        // Chinese titles. Only a standalone byte terminates the string.
        if *utf8_remaining > 0 {
            if (0x80..=0xbf).contains(&s[k]) {
                *utf8_remaining -= 1;
                k += 1;
                continue;
            }
            *utf8_remaining = 0;
        }
        match s[k] {
            0xc2..=0xdf => *utf8_remaining = 1,
            0xe0..=0xef => *utf8_remaining = 2,
            0xf0..=0xf4 => *utf8_remaining = 3,
            0x1b if k + 1 < s.len() => {
                let terminated = s[k + 1] == b'\\';
                return Some(StringEnd {
                    end: k,
                    len: if terminated { 2 } else { 0 },
                    terminated,
                });
            }
            0x18 | 0x1a => {
                return Some(StringEnd {
                    end: k,
                    len: 1,
                    terminated: false,
                })
            }
            0x9c => {
                return Some(StringEnd {
                    end: k,
                    len: 1,
                    terminated: true,
                })
            }
            0x07 if is_osc => {
                return Some(StringEnd {
                    end: k,
                    len: 1,
                    terminated: true,
                })
            }
            _ => {}
        }
        k += 1;
    }
    None
}

/// Classify a sequence body into a supported graphic, or `None` to pass through.
fn classify(payload: &[u8]) -> Option<Graphic> {
    match payload.first()? {
        // DCS: `...q<sixel data>`
        b'q' => Some(Graphic::Sixel {
            transparent: false,
            data: payload[1..].to_vec(),
        }),
        // APC Kitty: `G<params>;<data>`
        b'G' => Some(Graphic::Kitty(kitty::parse(&payload[1..]))),
        // OSC iTerm2 (and possibly other OSC we ignore).
        b'1' => {
            let body = payload.strip_prefix(b"1337;")?;
            iterm2(body)
        }
        _ => None,
    }
}

/// Parse `File=<params>:<base64>` from an OSC 1337 body.
fn iterm2(body: &[u8]) -> Option<Graphic> {
    let rest = body.strip_prefix(b"File=")?;
    let (params, data) = split_once(rest, b':')?;
    let params = String::from_utf8_lossy(params).to_string();
    let name = params
        .split(';')
        .find_map(|p| p.strip_prefix("name=").map(|s| s.to_string()));
    Some(Graphic::Iterm2 {
        name,
        data: data.to_vec(),
    })
}

fn split_once(s: &[u8], sep: u8) -> Option<(&[u8], &[u8])> {
    let i = s.iter().position(|&b| b == sep)?;
    Some((&s[..i], &s[i + 1..]))
}

/// Decode an iTerm2 base64 payload (raw PNG/JPEG/etc.).
pub fn decode_iterm2(data: &[u8], max_pixels: usize) -> Option<Image> {
    let bytes = decode_base64(data)?;
    decode_png(&bytes, max_pixels)
}

/// Encode a decoded image as a base64 PNG (straight RGBA), for saving inline
/// graphics alongside a session's scrollback. An image the PNG encoder refuses
/// yields an empty string, so callers can skip it.
pub fn encode_png_base64(image: &Image) -> String {
    use base64::Engine;
    use image::ImageEncoder;
    let mut png = Vec::new();
    if image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            &image.rgba,
            image.width,
            image.height,
            image::ExtendedColorType::Rgba8,
        )
        .is_err()
    {
        return String::new();
    }
    base64::engine::general_purpose::STANDARD.encode(png)
}

/// Decode a base64 PNG written by [`encode_png_base64`], bounded by `max_pixels`.
pub fn decode_png_base64(data: &str, max_pixels: usize) -> Option<Image> {
    let bytes = decode_base64(data.as_bytes())?;
    decode_png(&bytes, max_pixels)
}

/// Borrow ordinary base64; allocate a sanitized copy only when needed.
pub(crate) fn decode_base64(data: &[u8]) -> Option<Vec<u8>> {
    use base64::Engine;
    let text = if data.iter().any(u8::is_ascii_whitespace) {
        std::borrow::Cow::Owned(
            data.iter()
                .copied()
                .filter(|b| !b.is_ascii_whitespace())
                .collect::<Vec<_>>(),
        )
    } else {
        std::borrow::Cow::Borrowed(data)
    };
    base64::engine::general_purpose::STANDARD
        .decode(text.as_ref())
        .ok()
}

/// Decode a PNG (format 100) or raw RGB/RGBA (formats 24/32) with optional
/// zlib. `max_pixels` bounds the decoded size.
pub fn decode_bitmap(
    data: &[u8],
    format: u32,
    compressed: bool,
    max_pixels: usize,
) -> Option<Image> {
    let raw = if compressed {
        use std::io::Read;
        let mut out = Vec::new();
        // PNG container overhead is independent of its pixel count (even a
        // tiny PNG can exceed pixels*4+64). Bound the encoded container separately.
        let limit = if format == 100 {
            MAX_SEQUENCE
        } else {
            max_pixels.saturating_mul(4).saturating_add(64)
        };
        flate2::read::ZlibDecoder::new(data)
            .take(limit as u64)
            .read_to_end(&mut out)
            .ok()?;
        std::borrow::Cow::Owned(out)
    } else {
        std::borrow::Cow::Borrowed(data)
    };
    match format {
        100 => decode_png(&raw, max_pixels),
        24 | 32 => {
            let channels = if format == 24 { 3 } else { 4 };
            if raw.is_empty() || raw.len() % channels != 0 {
                return None;
            }
            rgba_from_raw(&raw, channels, max_pixels)
        }
        _ => None,
    }
}

/// Decode raw RGB/RGBA rows of 1 px height (raw formats carry no geometry, so
/// the caller derives width from `cols`; we infer height from length).
pub fn rgba_from_raw(raw: &[u8], channels: usize, max_pixels: usize) -> Option<Image> {
    let pixels = raw.len() / channels;
    if pixels == 0 || pixels > max_pixels {
        return None;
    }
    let rgba = if channels == 4 {
        raw[..pixels * 4].to_vec()
    } else {
        let mut rgba = Vec::with_capacity(pixels * 4);
        for px in raw.chunks_exact(channels) {
            rgba.extend_from_slice(&px[..3]);
            rgba.push(255);
        }
        rgba
    };
    Some(Image {
        width: pixels as u32,
        height: 1,
        rgba,
    })
}

fn decode_png(bytes: &[u8], max_pixels: usize) -> Option<Image> {
    use image::ImageDecoder;
    let mut decoder = image::codecs::png::PngDecoder::new(std::io::Cursor::new(bytes)).ok()?;
    let (w, h) = decoder.dimensions();
    let pixels = (w as usize).checked_mul(h as usize)?;
    // Check geometry before decoding/allocating pixels, not after the image has
    // already expanded. PNG can have 16-bit channels and decoder work buffers.
    if pixels == 0 || pixels > max_pixels {
        return None;
    }
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(max_pixels.saturating_mul(16) as u64);
    decoder.set_limits(limits).ok()?;
    let rgba = image::DynamicImage::from_decoder(decoder)
        .ok()?
        .into_rgba8();
    Some(Image {
        width: w,
        height: h,
        rgba: rgba.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_base64_round_trips_for_session_storage() {
        let image = Image {
            width: 2,
            height: 2,
            rgba: [255u8, 0, 0, 255].repeat(4),
        };
        let encoded = encode_png_base64(&image);
        assert!(!encoded.is_empty());
        let decoded = decode_png_base64(&encoded, 4).expect("round trip");
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert_eq!(decoded.rgba, image.rgba);
        assert!(
            decode_png_base64(&encoded, 3).is_none(),
            "the pixel budget still applies"
        );
        assert!(decode_png_base64("not base64", 4).is_none());
    }

    #[test]
    fn png_decoding_preserves_pixels_and_enforces_geometry_budget() {
        use image::ImageEncoder;
        let pixels = [255u8, 0, 0, 255].repeat(4);
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&pixels, 2, 2, image::ExtendedColorType::Rgba8)
            .unwrap();
        assert!(decode_png(&png, 3).is_none());
        assert_eq!(decode_png(&png, 4).unwrap().rgba, pixels);
        let mut wrapped = Vec::new();
        use std::io::Write;
        let mut encoder =
            flate2::write::ZlibEncoder::new(&mut wrapped, flate2::Compression::default());
        encoder.write_all(&png).unwrap();
        encoder.finish().unwrap();
        assert_eq!(decode_bitmap(&wrapped, 100, true, 4).unwrap().rgba, pixels);
    }

    #[derive(Debug, PartialEq)]
    enum OwnedEvent {
        Text(Vec<u8>),
        Graphics(Graphic),
        Osc(Vec<u8>),
        CursorReport,
    }

    fn events(bytes: &[u8], chunk_size: usize) -> Vec<OwnedEvent> {
        let mut scanner = Scanner::new();
        let mut out = Vec::new();
        for chunk in bytes.chunks(chunk_size) {
            scanner.feed_with(chunk, |event| match event {
                StreamEvent::Text(text) | StreamEvent::C1Terminator(text) => {
                    if let Some(OwnedEvent::Text(previous)) = out.last_mut() {
                        previous.extend_from_slice(text);
                    } else {
                        out.push(OwnedEvent::Text(text.to_vec()));
                    }
                }
                StreamEvent::Graphics(g) => out.push(OwnedEvent::Graphics(g)),
                StreamEvent::Osc(payload) => out.push(OwnedEvent::Osc(payload.to_vec())),
                StreamEvent::CursorReport => out.push(OwnedEvent::CursorReport),
            });
        }
        out
    }

    #[test]
    fn utf8_osc_titles_preserve_c1_continuation_bytes_across_feeds() {
        for title in [
            "miao | 检查 H3-2 云台相机拍照与照片拉取功能",
            "miao | 检查 Huawei testnet 交易持仓及 Tokyo03 L2 数据…",
            "miao | Ü Ĝ 😜",
        ] {
            for terminator in [b"\x07".as_slice(), b"\x1b\\", b"\x9c"] {
                let mut sequence = format!("\x1b]2;{title}").into_bytes();
                sequence.extend_from_slice(terminator);
                let mut expected = vec![
                    OwnedEvent::Osc(format!("2;{title}").into_bytes()),
                    OwnedEvent::Text(sequence.clone()),
                ];
                // A following title must not inherit the preceding UTF-8 state.
                sequence.extend_from_slice(b"\x1b]2;next\x07");
                expected.push(OwnedEvent::Osc(b"2;next".to_vec()));
                expected.push(OwnedEvent::Text(b"\x1b]2;next\x07".to_vec()));
                for size in 1..=sequence.len() {
                    assert_eq!(
                        events(&sequence, size),
                        expected,
                        "{title}: chunk size {size}"
                    );
                }
            }
        }
    }

    #[test]
    fn cancelled_strings_do_not_swallow_the_next_title_at_any_chunk_size() {
        for intro in [b"\x1b]2;".as_slice(), b"\x1bP", b"\x1b_"] {
            for cancellation in [b"\x18".as_slice(), b"\x1a", b""] {
                let mut stream = intro.to_vec();
                stream.extend_from_slice("中断相机".as_bytes());
                stream.extend_from_slice(cancellation);
                let aborted = stream.clone();
                stream.extend_from_slice(b"\x1b]2;fresh\x07after");
                let expected = vec![
                    OwnedEvent::Text(aborted),
                    OwnedEvent::Osc(b"2;fresh".to_vec()),
                    OwnedEvent::Text(b"\x1b]2;fresh\x07after".to_vec()),
                ];
                for size in 1..=stream.len() {
                    assert_eq!(
                        events(&stream, size),
                        expected,
                        "intro {intro:?}, size {size}"
                    );
                }
            }
        }
    }

    #[test]
    fn oversized_unfinished_string_resets_utf8_state_before_the_next_title() {
        let mut stream = b"\x1b]2;".to_vec();
        stream.resize(MAX_SEQUENCE + 1, b'a');
        stream.push(0xe6);
        let mut scanner = Scanner::new();
        scanner.feed_with(&stream, |event| {
            assert!(matches!(event, StreamEvent::Text(_)))
        });
        assert!(scanner.buf.is_empty());
        assert_eq!(scanner.utf8_remaining, 0);
        let mut titles = Vec::new();
        scanner.feed_with(b"\x1b]2;fresh\x9c", |event| {
            if let StreamEvent::Osc(title) = event {
                titles.push(title.to_vec());
            }
        });
        assert_eq!(titles, [b"2;fresh".to_vec()]);
    }

    #[test]
    fn stream_events_are_independent_of_every_chunk_boundary() {
        let mixed = b"text\x1b[31mred\x1b[0m\x1b[6n\x1b]2;title\x1b\\\x1b]7;file://localhost/tmp\x07\x1bPq~\x1b\\\x1b_Ga=d,d=a\x1b\\\x1b]1337;File=:AAAA\x9c\x1bPunknown\x1b\\\x1b[6X\x1b[?6n\x1b\x1b[6nend";
        let expected = events(mixed, mixed.len());
        for size in 1..=mixed.len() {
            assert_eq!(events(mixed, size), expected, "chunk size {size}");
        }
        assert_eq!(
            expected
                .iter()
                .filter(|e| **e == OwnedEvent::CursorReport)
                .count(),
            2
        );
    }

    #[test]
    fn ordinary_text_is_borrowed_and_graphics_payload_is_not_rescanned() {
        let text = b"normal \x1b[32mcolored\x1b[0m output";
        let mut scanner = Scanner::new();
        scanner.feed_with(text, |event| match event {
            StreamEvent::Text(borrowed) => assert_eq!(borrowed.as_ptr(), text.as_ptr()),
            _ => panic!("unexpected control event"),
        });
        assert_eq!(scanner.buf.capacity(), 0);
        scanner.feed_with(b"\x1b]1337;File=:", |_| {});
        let chunk = [b'A'; 512];
        for _ in 0..2048 {
            scanner.feed_with(&chunk, |_| panic!("sequence must remain buffered"));
        }
        scanner.feed_with(b"\x1b", |_| {});
        scanner.feed_with(b"\\", |_| {});
        assert!(
            scanner.inspected <= (1 << 20) + 32,
            "fragments must be scanned only once"
        );
        assert!(scanner.buf.capacity() <= RETAINED_BUFFER_BYTES);
    }

    #[test]
    fn unterminated_sequence_limit_releases_high_water_buffer() {
        let mut scanner = Scanner::new();
        scanner.buf = b"\x1b]1337;File=:".to_vec();
        scanner.buf.resize(MAX_SEQUENCE, b'A');
        let mut returned = 0;
        scanner.feed_with(b"A", |event| match event {
            StreamEvent::Text(text) => returned += text.len(),
            _ => panic!("oversized sequence must be returned as text"),
        });
        assert_eq!(returned, MAX_SEQUENCE + 1);
        assert_eq!(scanner.buf.capacity(), 0);
        assert_eq!(
            events(b"after overflow", 1),
            vec![OwnedEvent::Text(b"after overflow".to_vec())]
        );
    }

    #[test]
    #[ignore = "release fragmented scanner benchmark"]
    fn fragmented_sequence_scaling() {
        for size in [1 << 20, 2 << 20, 4 << 20] {
            let mut scanner = Scanner::new();
            let mut sequence = b"\x1b]1337;File=:".to_vec();
            sequence.resize(size, b'A');
            sequence.extend_from_slice(b"\x1b\\");
            let start = std::time::Instant::now();
            for chunk in sequence.chunks(512) {
                std::hint::black_box(scanner.feed(chunk));
            }
            println!(
                "fragmented sequence {} MiB, 512-byte chunks: {:.3} ms, retained capacity {} bytes",
                size >> 20,
                start.elapsed().as_secs_f64() * 1000.0,
                scanner.buf.capacity()
            );
            assert!(scanner.inspected <= sequence.len() + 32);
            assert!(scanner.buf.capacity() <= RETAINED_BUFFER_BYTES);
        }
    }

    #[test]
    fn passes_plain_text_through() {
        let mut s = Scanner::new();
        let segs = s.feed(b"hello world");
        assert_eq!(segs, vec![Segment::Text(b"hello world".to_vec())]);
    }

    #[test]
    fn extracts_a_sixel_sequence() {
        let mut s = Scanner::new();
        let segs = s.feed(b"before\x1bPq#0;2;0;0;0~-\x1b\\after");
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].text(), Some(&b"before"[..]));
        assert!(matches!(segs[1], Segment::Graphics(Graphic::Sixel { .. })));
        assert_eq!(segs[2].text(), Some(&b"after"[..]));
    }

    #[test]
    fn holds_a_split_sequence_until_complete() {
        let mut s = Scanner::new();
        assert_eq!(s.feed(b"a\x1bPq#0"), vec![Segment::Text(b"a".to_vec())]);
        let segs = s.feed(b"~-\x1b\\b");
        assert_eq!(segs.len(), 2);
        assert!(matches!(segs[0], Segment::Graphics(Graphic::Sixel { .. })));
        assert_eq!(segs[1].text(), Some(&b"b"[..]));
    }

    #[test]
    fn extracts_kitty_and_iterm2() {
        let mut s = Scanner::new();
        let segs = s.feed(b"\x1b_Ga=T,f=24;AAAA\x1b\\");
        match &segs[0] {
            Segment::Graphics(Graphic::Kitty(c)) => assert_eq!(c.action, 'T'),
            other => panic!("{other:?}"),
        }
        let segs = s.feed(b"\x1b]1337;File=name=YWJj:AAAA\x07");
        assert!(matches!(
            &segs[0],
            Segment::Graphics(Graphic::Iterm2 { name: Some(n), .. }) if n == "YWJj"
        ));
    }

    #[test]
    fn unknown_dcs_passes_through() {
        let mut s = Scanner::new();
        let segs = s.feed(b"\x1bP+q544e\x1b\\");
        assert_eq!(segs, vec![Segment::Text(b"\x1bP+q544e\x1b\\".to_vec())]);
    }

    #[test]
    fn raw_rgba_decodes() {
        let img = decode_bitmap(&[1, 2, 3, 4, 5, 6], 24, false, 100).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.rgba, vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }
}
