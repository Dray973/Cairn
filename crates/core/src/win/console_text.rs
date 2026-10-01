//! Text written by console programs, decoded from the code page it was written in, and a
//! streaming decoder that turns a program's redirected output into lines and progress
//! updates.

use windows::Win32::Globalization::{GetOEMCP, MultiByteToWideChar, MULTI_BYTE_TO_WIDE_CHAR_FLAGS};

/// Longest input [`decode_code_page`] decodes; the rest is dropped.
const MAX_DECODE_BYTES: usize = 16 * 1024 * 1024;

/// The OEM code page, which console programs such as `chkdsk` and `netsh` write their
/// output in when it goes to a file or a pipe.
pub fn oem_code_page() -> u32 {
    // SAFETY: GetOEMCP takes no arguments.
    unsafe { GetOEMCP() }
}

/// Decodes console output. Input that is valid UTF-8 and contains a non-ASCII byte is
/// taken as UTF-8; anything else is decoded from `code_page`, with bytes that are invalid
/// in it replaced by the code page's default character (U+FFFD where it has one). At most
/// 16 MiB are decoded.
pub fn decode_code_page(bytes: &[u8], code_page: u32) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let bytes = &bytes[..bytes.len().min(MAX_DECODE_BYTES)];
    if !bytes.is_ascii() {
        if let Ok(text) = std::str::from_utf8(bytes) {
            return text.to_owned();
        }
    }
    let flags = MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0);
    // SAFETY: `bytes` is a valid slice; a missing output buffer asks for the size only.
    let needed = unsafe { MultiByteToWideChar(code_page, flags, bytes, None) };
    if needed <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut wide = vec![0u16; needed as usize];
    // SAFETY: `wide` is writable for its whole length, which is passed alongside it.
    let written = unsafe { MultiByteToWideChar(code_page, flags, bytes, Some(&mut wide)) };
    if written <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    wide.truncate(written as usize);
    String::from_utf16_lossy(&wide)
}

// ───────────────────────────── Streaming decoder ─────────────────────────────

/// The encoding a console program's redirected output is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEncoding {
    /// UTF-16 little endian, which `sfc` writes when its output is redirected.
    Utf16Le,
    Utf8,
    /// A Windows code page, usually the OEM code page.
    CodePage(u32),
}

/// One unit of decoded output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputEvent {
    /// Text ended by a line feed, with or without carriage returns before it.
    Line(String),
    /// Text ended by carriage returns that a line feed does not follow: a progress display
    /// the program redraws in place.
    Progress(String),
}

/// Longest line or progress text [`OutputDecoder`] emits, in characters; longer text is
/// cut and ends with "…".
pub const MAX_LINE_CHARS: usize = 2000;

/// Bytes after which [`OutputDecoder`] stops waiting for a sign of the encoding.
const SNIFF_WINDOW: usize = 256;
/// Most bytes of one line or progress segment that are decoded; the rest is dropped.
const MAX_SEGMENT_BYTES: usize = 64 * 1024;

const CR: u16 = 0x0D;
const LF: u16 = 0x0A;
const UTF16_BOM: &[u8] = &[0xFF, 0xFE];
const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// The encoding the start of a program's output shows, if it shows one: a UTF-16LE or
/// UTF-8 byte order mark, or a NUL byte at an odd offset, which UTF-16LE text always has
/// (every space, digit and line end has one) and text in a code page never does.
pub fn sniff(window: &[u8]) -> Option<TextEncoding> {
    if window.starts_with(UTF16_BOM) {
        return Some(TextEncoding::Utf16Le);
    }
    if window.starts_with(UTF8_BOM) {
        return Some(TextEncoding::Utf8);
    }
    if window.iter().skip(1).step_by(2).any(|&b| b == 0) {
        return Some(TextEncoding::Utf16Le);
    }
    None
}

/// Turns the bytes a console program writes, received in chunks of any size, into
/// [`OutputEvent`]s.
///
/// - The encoding is chosen before anything is emitted. When a code page is expected, it is
///   chosen as soon as the first CR LF pair has arrived within the first 256 bytes with no
///   NUL byte before it: the expected code page, or UTF-8 after a UTF-8 byte order mark.
///   Otherwise it is chosen as soon as [`sniff`] recognises the first 256 bytes; else once
///   256 bytes have arrived, the expected encoding, except that UTF-16LE expected for 256
///   bytes without a single NUL byte falls back to the OEM code page; else at
///   [`OutputDecoder::finish`], the expected encoding. A byte order mark is skipped.
/// - Text is split on line ends before it is decoded, in bytes or, for UTF-16LE, in 16-bit
///   units, so a character split across two chunks waits for the rest.
/// - A run of carriage returns followed by a line feed ends a line (`sfc` writes `\r\r\n`);
///   a run followed by anything else ends a progress segment.
/// - Control characters other than TAB and trailing whitespace are removed, and text is cut
///   to [`MAX_LINE_CHARS`] characters.
/// - Empty lines before the first text are dropped, runs of empty lines become one, and
///   empty progress segments are dropped.
///
/// The events do not depend on how the input is split into chunks.
/// [`OutputDecoder::pending_progress`] shows the redraw that no event has reported yet.
#[derive(Debug)]
pub struct OutputDecoder {
    expected: TextEncoding,
    encoding: Option<TextEncoding>,
    /// Input received before the encoding was chosen.
    prefix: Vec<u8>,
    /// The first byte of a UTF-16 unit whose second byte has not arrived.
    odd: Option<u8>,
    /// Raw units (bytes, or UTF-16 units) of the segment being collected.
    segment: Vec<u16>,
    /// Carriage returns seen since the segment's last text.
    pending_crs: usize,
    /// The segment began after a run of carriage returns that ended a progress segment
    /// (empty or not), so it is the next redraw of a progress display.
    redraw: bool,
    /// A line with text has been emitted.
    seen_text: bool,
    /// The last line emitted was empty.
    last_empty: bool,
}

impl OutputDecoder {
    pub fn new(expected: TextEncoding) -> OutputDecoder {
        OutputDecoder {
            expected,
            encoding: None,
            prefix: Vec::new(),
            odd: None,
            segment: Vec::new(),
            pending_crs: 0,
            redraw: false,
            seen_text: false,
            last_empty: false,
        }
    }

    /// The encoding in use; `None` until it has been chosen.
    pub fn encoding(&self) -> Option<TextEncoding> {
        self.encoding
    }

    /// Decodes the next chunk of output and appends the events it completes to `out`.
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<OutputEvent>) {
        if self.encoding.is_some() {
            self.feed(bytes, out);
            return;
        }
        self.prefix.extend_from_slice(bytes);
        if let Some(encoding) = self.decide() {
            self.choose(encoding, out);
        }
    }

    /// The encoding the input received so far shows, if it is enough to choose one.
    fn decide(&self) -> Option<TextEncoding> {
        let window = &self.prefix[..self.prefix.len().min(SNIFF_WINDOW)];
        // Only the bytes up to the first line end are looked at, so the choice is the same
        // however the input is split. Code-page text has no NUL byte; UTF-16LE text has one
        // in every ASCII character. UTF-16LE text whose first 0D 0A pair lies within its
        // non-ASCII characters, before any ASCII one, would be taken for code-page text, so
        // this is done only where a code page is expected.
        if let TextEncoding::CodePage(_) = self.expected {
            if let Some(at) = window.windows(2).position(|pair| pair == b"\r\n") {
                let head = &window[..at + 2];
                if !head.contains(&0) {
                    return Some(sniff(head).unwrap_or(self.expected));
                }
            }
        }
        match sniff(window) {
            Some(encoding) => Some(encoding),
            None if self.prefix.len() >= SNIFF_WINDOW => Some(self.fallback()),
            None => None,
        }
    }

    /// Ends the input: the text after the last line end becomes a line, and a dangling
    /// half of a UTF-16 unit is dropped.
    pub fn finish(&mut self, out: &mut Vec<OutputEvent>) {
        if self.encoding.is_none() {
            let window = &self.prefix[..self.prefix.len().min(SNIFF_WINDOW)];
            let encoding = sniff(window).unwrap_or(self.expected);
            self.choose(encoding, out);
        }
        self.odd = None;
        self.pending_crs = 0;
        if !self.segment.is_empty() {
            self.emit_line(out);
        }
        self.redraw = false;
    }

    /// The progress display as the program is drawing it now, before the character that
    /// ends the redraw has arrived: the cleaned text collected since a progress segment
    /// ended. `None` when nothing has been collected since, or when the text follows a line
    /// (it may be a line itself). This is not an event: a redraw is reported as an event
    /// only once it has ended, so the events stay independent of how the input is split.
    pub fn pending_progress(&self) -> Option<String> {
        if !self.redraw || self.segment.is_empty() {
            return None;
        }
        let text = self.decode_segment();
        // A character whose remaining bytes have not arrived decodes as a replacement.
        let text = clean(text.trim_end_matches('\u{FFFD}'));
        (!text.is_empty()).then_some(text)
    }

    /// The encoding of 256 bytes that show none: the expected encoding, or the OEM code
    /// page when UTF-16LE was expected but the bytes hold no NUL at all.
    fn fallback(&self) -> TextEncoding {
        let window = &self.prefix[..self.prefix.len().min(SNIFF_WINDOW)];
        match self.expected {
            TextEncoding::Utf16Le if !window.contains(&0) => {
                TextEncoding::CodePage(oem_code_page())
            }
            other => other,
        }
    }

    fn choose(&mut self, encoding: TextEncoding, out: &mut Vec<OutputEvent>) {
        self.encoding = Some(encoding);
        let prefix = std::mem::take(&mut self.prefix);
        let bom = match encoding {
            TextEncoding::Utf16Le => UTF16_BOM,
            TextEncoding::Utf8 => UTF8_BOM,
            TextEncoding::CodePage(_) => &[],
        };
        let body = prefix.strip_prefix(bom).unwrap_or(&prefix);
        self.feed(body, out);
    }

    fn feed(&mut self, bytes: &[u8], out: &mut Vec<OutputEvent>) {
        if self.encoding != Some(TextEncoding::Utf16Le) {
            for &b in bytes {
                self.unit(u16::from(b), out);
            }
            return;
        }
        let mut rest = bytes;
        if let Some(low) = self.odd.take() {
            match rest.split_first() {
                Some((&high, tail)) => {
                    self.unit(u16::from_le_bytes([low, high]), out);
                    rest = tail;
                }
                None => {
                    self.odd = Some(low);
                    return;
                }
            }
        }
        let mut pairs = rest.chunks_exact(2);
        for pair in &mut pairs {
            self.unit(u16::from_le_bytes([pair[0], pair[1]]), out);
        }
        if let [last] = pairs.remainder() {
            self.odd = Some(*last);
        }
    }

    fn max_units(&self) -> usize {
        match self.encoding {
            Some(TextEncoding::Utf16Le) => MAX_SEGMENT_BYTES / 2,
            _ => MAX_SEGMENT_BYTES,
        }
    }

    fn unit(&mut self, unit: u16, out: &mut Vec<OutputEvent>) {
        match unit {
            CR => self.pending_crs += 1,
            LF => {
                self.pending_crs = 0;
                self.emit_line(out);
            }
            _ => {
                if self.pending_crs > 0 {
                    self.pending_crs = 0;
                    self.emit_progress(out);
                }
                if self.segment.len() < self.max_units() {
                    self.segment.push(unit);
                }
            }
        }
    }

    /// The collected segment, decoded.
    fn decode_segment(&self) -> String {
        match self.encoding {
            Some(TextEncoding::Utf16Le) => String::from_utf16_lossy(&self.segment),
            Some(TextEncoding::Utf8) => {
                let bytes: Vec<u8> = self.segment.iter().map(|&u| u as u8).collect();
                String::from_utf8_lossy(&bytes).into_owned()
            }
            Some(TextEncoding::CodePage(code_page)) => {
                let bytes: Vec<u8> = self.segment.iter().map(|&u| u as u8).collect();
                decode_code_page(&bytes, code_page)
            }
            None => String::new(),
        }
    }

    /// Decodes and clears the collected segment.
    fn take_text(&mut self) -> String {
        let text = self.decode_segment();
        self.segment.clear();
        clean(&text)
    }

    fn emit_line(&mut self, out: &mut Vec<OutputEvent>) {
        self.redraw = false;
        let text = self.take_text();
        if text.is_empty() {
            if !self.seen_text || self.last_empty {
                return;
            }
            self.last_empty = true;
        } else {
            self.seen_text = true;
            self.last_empty = false;
        }
        out.push(OutputEvent::Line(text));
    }

    fn emit_progress(&mut self, out: &mut Vec<OutputEvent>) {
        self.redraw = true;
        let text = self.take_text();
        if !text.is_empty() {
            out.push(OutputEvent::Progress(text));
        }
    }
}

/// Drops control characters other than TAB and trailing whitespace, and cuts the text to
/// [`MAX_LINE_CHARS`] characters.
fn clean(text: &str) -> String {
    let mut kept: String = text
        .chars()
        .filter(|&c| c == '\t' || u32::from(c) >= 0x20)
        .collect();
    let end = kept.trim_end().len();
    kept.truncate(end);
    if kept.chars().count() <= MAX_LINE_CHARS {
        return kept;
    }
    let mut cut: String = kept.chars().take(MAX_LINE_CHARS - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use OutputEvent::{Line, Progress};

    #[test]
    fn code_pages_decode_explicitly() {
        assert_eq!(decode_code_page(b"abc", 437), "abc");
        assert_eq!(decode_code_page(&[0x9A, b'b', b'e', b'r'], 850), "Über");
        assert_eq!(decode_code_page(&[0x81, b'b', b'e', b'r'], 437), "über");
        assert_eq!(decode_code_page(b"", 850), "");
    }

    #[test]
    fn utf8_with_non_ascii_text_is_taken_as_utf8() {
        assert_eq!(
            decode_code_page("Überprüfung".as_bytes(), 850),
            "Überprüfung"
        );
    }

    #[test]
    fn oem_code_page_is_known() {
        assert!(oem_code_page() > 0);
    }

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn line(text: &str) -> OutputEvent {
        Line(text.to_string())
    }

    fn progress(text: &str) -> OutputEvent {
        Progress(text.to_string())
    }

    fn decode_all(expected: TextEncoding, bytes: &[u8]) -> Vec<OutputEvent> {
        let mut decoder = OutputDecoder::new(expected);
        let mut out = Vec::new();
        decoder.push(bytes, &mut out);
        decoder.finish(&mut out);
        out
    }

    /// Output shaped like `sfc /verifyonly` redirected to a file.
    const SFC_TEXT: &str =
        "\r\nBeginning system scan.  This process will take some time.\r\r\n\r\n\
        Beginning verification phase of system scan.\r\r\n\
        Verification 5% complete.\rVerification 45% complete.\rVerification 100% complete.\r\r\n\
        \r\nWindows Resource Protection did not find any integrity violations.\r\r\n";

    fn sfc_events() -> Vec<OutputEvent> {
        vec![
            line("Beginning system scan.  This process will take some time."),
            line(""),
            line("Beginning verification phase of system scan."),
            progress("Verification 5% complete."),
            progress("Verification 45% complete."),
            line("Verification 100% complete."),
            line(""),
            line("Windows Resource Protection did not find any integrity violations."),
        ]
    }

    /// Output shaped like `dism /Online /Cleanup-Image /ScanHealth` redirected to a file.
    const DISM_TEXT: &str =
        "\r\nDeployment Image Servicing and Management tool\r\nVersion: 10.0.26100.1\r\n\r\n\
        Image Version: 10.0.26200.6584\r\n\r\n\
        \r[                           0.0%                           ] \
        \r[==                         5.0%                           ] \
        \r[===========               20.3%                           ] \
        \r[==========================100.0%==========================] \r\n\
        No component store corruption detected.\r\nThe operation completed successfully.\r\n";

    #[test]
    fn sfc_utf16_with_stray_cr_decodes_to_lines() {
        let mut bytes = UTF16_BOM.to_vec();
        bytes.extend(utf16(SFC_TEXT));
        assert_eq!(decode_all(TextEncoding::Utf16Le, &bytes), sfc_events());
        // Without a byte order mark the NUL bytes identify UTF-16LE as well.
        assert_eq!(
            decode_all(TextEncoding::CodePage(437), &utf16(SFC_TEXT)),
            sfc_events()
        );
    }

    #[test]
    fn transient_segments_become_progress() {
        let events = decode_all(
            TextEncoding::CodePage(437),
            b"Stage 1\r\n10%\r20%\r30%\r\r\ndone\r\n",
        );
        assert_eq!(
            events,
            [
                line("Stage 1"),
                progress("10%"),
                progress("20%"),
                line("30%"),
                line("done")
            ]
        );
        // A carriage return at the end of a chunk waits for what follows it.
        let mut decoder = OutputDecoder::new(TextEncoding::Utf8);
        let mut out = Vec::new();
        decoder.push(b"\xEF\xBB\xBFstep 1\r", &mut out);
        assert!(out.is_empty(), "{out:?}");
        decoder.push(b"step 2\r", &mut out);
        assert_eq!(out, [progress("step 1")]);
        decoder.push(b"\n", &mut out);
        assert_eq!(out, [progress("step 1"), line("step 2")]);
    }

    #[test]
    fn dism_bar_redraws_are_progress() {
        let events = decode_all(TextEncoding::CodePage(437), DISM_TEXT.as_bytes());
        let bar = |fill: &str| progress(fill);
        assert_eq!(
            events,
            [
                line("Deployment Image Servicing and Management tool"),
                line("Version: 10.0.26100.1"),
                line(""),
                line("Image Version: 10.0.26200.6584"),
                line(""),
                bar("[                           0.0%                           ]"),
                bar("[==                         5.0%                           ]"),
                bar("[===========               20.3%                           ]"),
                line("[==========================100.0%==========================]"),
                line("No component store corruption detected."),
                line("The operation completed successfully."),
            ]
        );
    }

    fn chunked(expected: TextEncoding, bytes: &[u8], cuts: &[usize]) -> Vec<OutputEvent> {
        let mut decoder = OutputDecoder::new(expected);
        let mut out = Vec::new();
        let mut start = 0;
        for &cut in cuts {
            decoder.push(&bytes[start..cut], &mut out);
            start = cut;
        }
        decoder.push(&bytes[start..], &mut out);
        decoder.finish(&mut out);
        out
    }

    #[test]
    fn every_chunk_boundary_gives_the_same_events() {
        let mut sfc = UTF16_BOM.to_vec();
        sfc.extend(utf16(SFC_TEXT));
        let long_oem: Vec<u8> = [
            b"\x9Aberpr\x81fung 12% \r".as_slice(),
            &[b'x'; 300],
            b"\r\n\r\n\r\ntail",
        ]
        .concat();
        // A code-page line end first, UTF-16LE (with NUL bytes) right after it.
        let mixed: Vec<u8> = [b"head\r\n".to_vec(), utf16("x y\r\n")].concat();
        let samples: [(TextEncoding, &[u8]); 5] = [
            (TextEncoding::Utf16Le, &sfc),
            (TextEncoding::CodePage(437), &utf16(SFC_TEXT)),
            (TextEncoding::CodePage(437), DISM_TEXT.as_bytes()),
            (TextEncoding::CodePage(850), &long_oem),
            (TextEncoding::CodePage(437), &mixed),
        ];
        for (expected, bytes) in samples {
            let whole = decode_all(expected, bytes);
            assert!(!whole.is_empty());
            for cut in 0..=bytes.len() {
                assert_eq!(
                    chunked(expected, bytes, &[cut]),
                    whole,
                    "split at {cut} of {}",
                    bytes.len()
                );
            }
            // One byte at a time.
            let cuts: Vec<usize> = (1..bytes.len()).collect();
            assert_eq!(chunked(expected, bytes, &cuts), whole);
        }
    }

    #[test]
    fn oem_code_pages_decode_explicitly() {
        assert_eq!(
            decode_all(TextEncoding::CodePage(850), b"\x9Aber\r\n"),
            [line("Über")]
        );
        assert_eq!(
            decode_all(TextEncoding::CodePage(437), b"\x81ber\r\n"),
            [line("über")]
        );
        let shift_jis = b"\x83\x60\x83\x46\x83\x62\x83\x4E\r\n";
        assert_eq!(
            decode_all(TextEncoding::CodePage(932), shift_jis),
            [line("チェック")]
        );
        assert_eq!(decode_code_page(&shift_jis[..8], 932), "チェック");
        // UTF-8 output is recognised in any code page, and decoded directly as UTF-8.
        assert_eq!(
            decode_all(TextEncoding::CodePage(850), "Überprüfung\r\n".as_bytes()),
            [line("Überprüfung")]
        );
        assert_eq!(
            decode_all(TextEncoding::Utf8, "Überprüfung\n".as_bytes()),
            [line("Überprüfung")]
        );
    }

    #[test]
    fn bom_and_sniffing_choose_the_encoding() {
        assert_eq!(sniff(&[0xFF, 0xFE, b'a']), Some(TextEncoding::Utf16Le));
        assert_eq!(sniff(&[0xEF, 0xBB, 0xBF, b'a']), Some(TextEncoding::Utf8));
        assert_eq!(sniff(&[b'a', 0, b'b', 0]), Some(TextEncoding::Utf16Le));
        assert_eq!(sniff(&[0, b'a', b'b']), None, "a NUL at an even offset");
        assert_eq!(sniff(b"plain text"), None);
        assert_eq!(sniff(&[0xFF]), None);

        let mut decoder = OutputDecoder::new(TextEncoding::CodePage(437));
        let mut out = Vec::new();
        decoder.push(&[0xEF, 0xBB], &mut out);
        assert_eq!(decoder.encoding(), None);
        decoder.push(&[0xBF], &mut out);
        assert_eq!(decoder.encoding(), Some(TextEncoding::Utf8));
        decoder.push("é\r\n".as_bytes(), &mut out);
        assert_eq!(out, [line("é")], "the byte order mark is skipped");

        let mut decoder = OutputDecoder::new(TextEncoding::CodePage(437));
        let mut bytes = UTF16_BOM.to_vec();
        bytes.extend(utf16("ok\r\n"));
        decoder.push(&bytes, &mut out);
        assert_eq!(decoder.encoding(), Some(TextEncoding::Utf16Le));
        assert_eq!(out.last(), Some(&line("ok")));

        // Short output without a sign is decoded as expected when it ends.
        let mut decoder = OutputDecoder::new(TextEncoding::CodePage(850));
        let mut out = Vec::new();
        decoder.push(b"short", &mut out);
        assert_eq!(decoder.encoding(), None);
        decoder.finish(&mut out);
        assert_eq!(decoder.encoding(), Some(TextEncoding::CodePage(850)));
        assert_eq!(out, [line("short")]);

        // That holds for short UTF-16LE text whose characters have no NUL byte as well.
        let bytes = utf16("完了");
        assert_eq!(sniff(&bytes), None);
        let mut decoder = OutputDecoder::new(TextEncoding::Utf16Le);
        let mut out = Vec::new();
        decoder.push(&bytes, &mut out);
        assert_eq!(decoder.encoding(), None);
        decoder.finish(&mut out);
        assert_eq!(decoder.encoding(), Some(TextEncoding::Utf16Le));
        assert_eq!(out, [line("完了")]);
    }

    #[test]
    fn bomless_utf16_starting_with_cyrillic_or_japanese_is_detected() {
        for text in [
            "Проверка системных файлов завершена.\r\r\nГотово.\r\r\n",
            "システムファイルチェッカー。\r\r\n完了しました。\r\r\n",
        ] {
            let bytes = utf16(text);
            // The first characters have no NUL byte; the line end brings one.
            assert_ne!(sniff(&bytes[..8]), Some(TextEncoding::Utf16Le));
            let mut decoder = OutputDecoder::new(TextEncoding::CodePage(866));
            let mut out = Vec::new();
            decoder.push(&bytes, &mut out);
            assert_eq!(decoder.encoding(), Some(TextEncoding::Utf16Le), "{text}");
            decoder.finish(&mut out);
            let lines: Vec<OutputEvent> = text
                .split("\r\r\n")
                .filter(|l| !l.is_empty())
                .map(line)
                .collect();
            assert_eq!(out, lines);
        }
    }

    #[test]
    fn code_page_text_without_nul_decides_after_256_bytes() {
        let text: Vec<u8> = [b"x".repeat(100), b"\r\n".to_vec(), b"y".repeat(200)].concat();
        let mut decoder = OutputDecoder::new(TextEncoding::Utf16Le);
        let mut out = Vec::new();
        decoder.push(&text[..255], &mut out);
        assert_eq!(decoder.encoding(), None);
        assert!(
            out.is_empty(),
            "nothing is emitted before the encoding is chosen"
        );
        decoder.push(&text[255..256], &mut out);
        assert_eq!(
            decoder.encoding(),
            Some(TextEncoding::CodePage(oem_code_page())),
            "UTF-16LE without a NUL falls back to the OEM code page"
        );
        assert_eq!(out, [line(&"x".repeat(100))]);
        decoder.push(&text[256..], &mut out);
        decoder.finish(&mut out);
        assert_eq!(out.last(), Some(&line(&"y".repeat(200))));

        let mut decoder = OutputDecoder::new(TextEncoding::CodePage(1252));
        decoder.push(&[b'a'; 256], &mut Vec::new());
        assert_eq!(decoder.encoding(), Some(TextEncoding::CodePage(1252)));
    }

    #[test]
    fn expected_code_page_is_chosen_at_the_first_line_end() {
        // DISM's header and its first bar are far shorter than 256 bytes.
        let header =
            "\r\nDeployment Image Servicing and Management tool\r\nVersion: 10.0.26100.1\r\n\r\n\
                      Image Version: 10.0.26200.6584\r\n\r\n";
        let bar = "\r[==                         5.0%                           ] ";
        let mut decoder = OutputDecoder::new(TextEncoding::CodePage(437));
        let mut out = Vec::new();
        decoder.push(header.as_bytes(), &mut out);
        assert_eq!(decoder.encoding(), Some(TextEncoding::CodePage(437)));
        let lines = [
            line("Deployment Image Servicing and Management tool"),
            line("Version: 10.0.26100.1"),
            line(""),
            line("Image Version: 10.0.26200.6584"),
            line(""),
        ];
        assert_eq!(out, lines);
        decoder.push(bar.as_bytes(), &mut out);
        assert!(header.len() + bar.len() < SNIFF_WINDOW);
        assert_eq!(out, lines, "the bar on screen is no event yet");
        assert_eq!(
            decoder.pending_progress().as_deref(),
            Some("[==                         5.0%                           ]")
        );

        // Nothing is chosen before the line end has arrived whole.
        let mut decoder = OutputDecoder::new(TextEncoding::CodePage(850));
        let mut out = Vec::new();
        decoder.push(b"Checking\r", &mut out);
        assert_eq!(decoder.encoding(), None);
        decoder.push(b"\n", &mut out);
        assert_eq!(decoder.encoding(), Some(TextEncoding::CodePage(850)));
        assert_eq!(out, [line("Checking")]);

        // A UTF-8 byte order mark before the line end still chooses UTF-8.
        let mut decoder = OutputDecoder::new(TextEncoding::CodePage(850));
        let mut out = Vec::new();
        decoder.push("\u{feff}Über\r\n".as_bytes(), &mut out);
        assert_eq!(decoder.encoding(), Some(TextEncoding::Utf8));
        assert_eq!(out, [line("Über")]);

        // A NUL byte before the line end leaves the choice to the NUL bytes.
        let mut decoder = OutputDecoder::new(TextEncoding::CodePage(437));
        let mut out = Vec::new();
        decoder.push(&[b'a', 0, b'\r', b'\n'], &mut out);
        assert_eq!(decoder.encoding(), Some(TextEncoding::Utf16Le));

        // UTF-16LE is still expected until its NUL bytes or 256 bytes show otherwise.
        let mut decoder = OutputDecoder::new(TextEncoding::Utf16Le);
        decoder.push(b"no nul\r\n", &mut Vec::new());
        assert_eq!(decoder.encoding(), None);
    }

    #[test]
    fn pending_progress_is_the_redraw_being_drawn() {
        // DISM draws each bar after a carriage return, so the bar on screen has no end yet.
        let mut decoder = OutputDecoder::new(TextEncoding::CodePage(437));
        let mut out = Vec::new();
        decoder.push(b"Image Version: 10.0\r\n\r\n", &mut out);
        assert_eq!(decoder.pending_progress(), None);
        decoder.push(b"\r[=     0.0%    ] ", &mut out);
        assert_eq!(
            decoder.pending_progress().as_deref(),
            Some("[=     0.0%    ]")
        );
        decoder.push(b"\r[===  62.3%    ] ", &mut out);
        assert_eq!(out.last(), Some(&progress("[=     0.0%    ]")));
        assert_eq!(
            decoder.pending_progress().as_deref(),
            Some("[===  62.3%    ]")
        );
        let events = out.len();
        // Half a bar is shown as far as it has arrived; the reader decides what to make of it.
        decoder.push(b"\r[====  6", &mut out);
        assert_eq!(decoder.pending_progress().as_deref(), Some("[====  6"));
        assert_eq!(out.len(), events + 1);
        decoder.push(b"4.0%    ] \r\nDone.\r\n", &mut out);
        assert_eq!(decoder.pending_progress(), None, "a line ended the display");
        assert_eq!(
            out[events + 1..],
            [line("[====  64.0%    ]"), line("Done.")]
        );

        // SFC and defrag end each redraw with a carriage return instead.
        let mut decoder = OutputDecoder::new(TextEncoding::CodePage(437));
        let mut out = Vec::new();
        decoder.push(b"Stage 1\r\n10%\r", &mut out);
        assert_eq!(
            decoder.pending_progress(),
            None,
            "text after a line may be a line"
        );
        decoder.push(b"20%\r", &mut out);
        assert_eq!(out, [line("Stage 1"), progress("10%")]);
        assert_eq!(decoder.pending_progress().as_deref(), Some("20%"));
        decoder.push(b"\r", &mut out);
        assert_eq!(decoder.pending_progress().as_deref(), Some("20%"));
        decoder.finish(&mut out);
        assert_eq!(decoder.pending_progress(), None);
        assert_eq!(out, [line("Stage 1"), progress("10%"), line("20%")]);

        // A report line after a line is never shown as progress, even before its line feed.
        let mut decoder = OutputDecoder::new(TextEncoding::CodePage(437));
        let mut out = Vec::new();
        decoder.push(
            b"Volume size = 952 GB\r\nTotal fragmented space = 5%\r",
            &mut out,
        );
        assert_eq!(decoder.pending_progress(), None);
        decoder.push(b"\n", &mut out);
        assert_eq!(decoder.pending_progress(), None);

        // An unfinished UTF-16 character is left out.
        let mut decoder = OutputDecoder::new(TextEncoding::Utf16Le);
        let mut out = Vec::new();
        let mut bytes = UTF16_BOM.to_vec();
        bytes.extend(utf16("Verification 5% complete.\rVerification 45%"));
        // The first half of a surrogate pair.
        bytes.extend([0x3D, 0xD8]);
        decoder.push(&bytes, &mut out);
        assert_eq!(out, [progress("Verification 5% complete.")]);
        assert_eq!(
            decoder.pending_progress().as_deref(),
            Some("Verification 45%")
        );
    }

    #[test]
    fn controls_and_long_lines_are_cleaned() {
        assert_eq!(
            decode_all(TextEncoding::CodePage(437), b"a\x07b\tc \x1b[0m  \r\n"),
            [line("ab\tc [0m")]
        );
        let long = "z".repeat(5000);
        let events = decode_all(TextEncoding::Utf8, format!("{long}\n").as_bytes());
        match &events[..] {
            [Line(text)] => {
                assert_eq!(text.chars().count(), MAX_LINE_CHARS);
                assert!(text.ends_with('…'));
                assert!(text.starts_with("zzz"));
            }
            other => panic!("{other:?}"),
        }
        // A segment longer than 64 KiB is capped before it is decoded.
        let huge = vec![b'q'; 200 * 1024];
        let mut decoder = OutputDecoder::new(TextEncoding::CodePage(437));
        let mut out = Vec::new();
        decoder.push(&huge, &mut out);
        assert!(decoder.segment.len() <= MAX_SEGMENT_BYTES);
        decoder.push(b"\r\nnext\r\n", &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1], line("next"));
    }

    #[test]
    fn empty_lines_collapse() {
        let events = decode_all(
            TextEncoding::CodePage(437),
            b"\r\n\r\n  \r\nA\r\n\r\n\r\n \t \r\n\r\nB\r\n\r\n\r\x08\r\rC\r\n",
        );
        assert_eq!(
            events,
            [line("A"), line(""), line("B"), line(""), line("C")]
        );
    }

    #[test]
    fn finish_flushes_the_pending_segment() {
        // Past the first 256 bytes, so the encoding is chosen before the input ends.
        let first = "f".repeat(300);
        let mut decoder = OutputDecoder::new(TextEncoding::CodePage(437));
        let mut out = Vec::new();
        decoder.push(format!("{first}\r\nno line end").as_bytes(), &mut out);
        assert_eq!(out, [line(&first)]);
        decoder.finish(&mut out);
        assert_eq!(out, [line(&first), line("no line end")]);

        // A dangling half of a UTF-16 unit is dropped.
        let mut bytes = UTF16_BOM.to_vec();
        bytes.extend(utf16("done"));
        bytes.push(b'!');
        let mut decoder = OutputDecoder::new(TextEncoding::Utf16Le);
        let mut out = Vec::new();
        decoder.push(&bytes, &mut out);
        assert!(out.is_empty());
        decoder.finish(&mut out);
        assert_eq!(out, [line("done")]);

        // Nothing at all gives nothing.
        let mut out = Vec::new();
        OutputDecoder::new(TextEncoding::Utf16Le).finish(&mut out);
        assert!(out.is_empty());
    }
}
