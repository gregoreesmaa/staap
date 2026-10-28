//! Retained plain-text line buffer mirroring PTY output.
//!
//! The vt100 emulator keeps a 2000-line scrollback that its 0.15 API does
//! not expose (no public scrollback-row accessor, `set_scrollback` is
//! crate-private), so the pager mirrors every output byte into its own
//! retained lines instead. [`ScrollbackLog::feed`] strips ANSI escape
//! sequences with a small byte state machine and splits on newlines,
//! keeping at most [`MAX_RETAINED_LINES`] of the most recent lines plus
//! the in-progress line. A lone carriage return approximates overwrite
//! (clear the current line); CR before LF is one newline with no clear.
//! Backspaces pop a char, tabs pass through; anything else
//! below U+0020 (except `\n`) is dropped. Invalid UTF-8 decodes lossy, and
//! a multibyte sequence split across two `feed` calls is carried over so
//! no replacement char leaks in at chunk boundaries.

use std::collections::VecDeque;

/// Lines of output retained for the pager (mirrors the vt100 buffer size).
pub const MAX_RETAINED_LINES: usize = 2000;

/// Plain-text mirror of everything the child wrote, for the pager.
#[derive(Debug, Default)]
pub struct ScrollbackLog {
    lines: VecDeque<String>,
    cur: Vec<u8>,
    carry: Vec<u8>,
    cr_pending: bool,
    esc: EscState,
    /// True once the cap dropped older lines.
    pub truncated: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum EscState {
    #[default]
    Text,
    Esc,
    Csi,
    Osc,
    /// `ESC (`, `ESC )`, `ESC #`: skip one following byte.
    SkipOne,
}

impl ScrollbackLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw PTY output bytes into the mirror.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.carry.extend_from_slice(chunk);
        let take = strip_incomplete_tail(&self.carry);
        let bytes: Vec<u8> = self.carry.drain(..take).collect();
        // Decode lossily: a carry split can only occur at the tail, which
        // we held back, so interior undecodables are genuinely invalid.
        let text = String::from_utf8_lossy(&bytes);
        for c in text.chars() {
            self.feed_char(c);
        }
        // A pending CR at end of input stays pending: a `\n` opening the
        // next chunk still completes the CRLF pair.
    }

    /// Decode any held-back tail bytes (e.g. once the child exits and no
    /// more chunks will complete them) so trailing text is not lost.
    pub fn flush(&mut self) {
        if self.carry.is_empty() {
            return;
        }
        let bytes: Vec<u8> = self.carry.drain(..).collect();
        let text = String::from_utf8_lossy(&bytes);
        for c in text.chars() {
            self.feed_char(c);
        }
    }

    /// Complete retained lines (oldest first), excluding the in-progress one.
    pub fn lines(&self) -> &VecDeque<String> {
        &self.lines
    }

    /// The in-progress (not yet newline-terminated) line, decoded lossy.
    pub fn pending_line(&self) -> String {
        String::from_utf8_lossy(&self.cur).into_owned()
    }

    /// Retained line count including the in-progress line when non-empty.
    /// Legacy total: also counts a pending lone-CR line (empty `cur`
    /// with `cr_pending`) that contributes no sliceable row. The pager
    /// no longer clamps against this (issue #111); see `retained_len`.
    pub fn line_count(&self) -> usize {
        self.lines.len() + usize::from(!self.cur.is_empty() || self.cr_pending)
    }

    /// Sliceable pager rows: complete lines plus the in-progress line
    /// when non-empty. Single source of length truth for the pager: the
    /// clamp (`scroll_by`), the position note (`pager_note`), the window
    /// (`scroll_window`), and the slice (`pager_text_rows`) all derive
    /// from this length, so no offset addresses a row the snapshot does
    /// not contain (issue #111). Matches what `lines` + non-empty
    /// `pending_line` snapshots can slice.
    pub fn retained_len(&self) -> usize {
        self.lines.len() + usize::from(!self.cur.is_empty())
    }

    /// A pending CR turned out to be lone (no LF followed): it meant
    /// overwrite, so drop the line content the overwrite replaces.
    fn resolve_cr(&mut self) {
        if self.cr_pending {
            self.cr_pending = false;
            self.cur.clear();
        }
    }

    fn feed_char(&mut self, c: char) {
        match self.esc {
            EscState::Text => match c {
                '\u{1b}' => {
                    self.resolve_cr();
                    self.esc = EscState::Esc;
                }
                '\n' => {
                    // CRLF is one newline with no overwrite: the pending
                    // CR is consumed without clearing.
                    self.cr_pending = false;
                    self.push_line();
                }
                '\r' => {
                    // Defer: a following LF makes this half of a CRLF
                    // (no clear); anything else resolves it as a lone
                    // overwrite. A second CR resolves the first as lone.
                    if self.cr_pending {
                        self.cur.clear();
                    }
                    self.cr_pending = true;
                }
                '\u{08}' => {
                    // Backspace: erase one char (progress spinners, echoes).
                    self.resolve_cr();
                    pop_char(&mut self.cur);
                }
                c if (c as u32) < 0x20 => {
                    // Other C0 controls (bel, cursor queries, ...) carry no
                    // pager text.
                    self.resolve_cr();
                }
                _ => {
                    self.resolve_cr();
                    push_utf8(&mut self.cur, c);
                }
            },
            EscState::Esc => match c {
                '[' => self.esc = EscState::Csi,
                ']' => self.esc = EscState::Osc,
                '(' | ')' | '#' => self.esc = EscState::SkipOne,
                _ => self.esc = EscState::Text,
            },
            EscState::Csi => {
                // CSI ends on a final byte `@`..=`~`.
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    self.esc = EscState::Text;
                }
            }
            EscState::Osc => {
                // OSC ends on BEL or ESC (the ESC itself is consumed here;
                // a following `\` lands harmlessly in Text).
                if c == '\u{07}' || c == '\u{1b}' {
                    self.esc = EscState::Text;
                }
            }
            EscState::SkipOne => self.esc = EscState::Text,
        }
    }

    fn push_line(&mut self) {
        let line = String::from_utf8_lossy(&self.cur).into_owned();
        self.cur.clear();
        self.lines.push_back(line);
        while self.lines.len() > MAX_RETAINED_LINES {
            self.lines.pop_front();
            self.truncated = true;
        }
    }
}

/// Bytes to decode now: everything except a possibly incomplete UTF-8
/// sequence at the tail, which stays in the carry for the next `feed`.
fn strip_incomplete_tail(buf: &[u8]) -> usize {
    let len = buf.len();
    // Count trailing continuation bytes (a sequence carries at most 3).
    let mut conts = 0;
    while conts < 3 && conts < len && is_continuation(buf[len - 1 - conts]) {
        conts += 1;
    }
    if conts == 0 {
        // No trailing continuations: incomplete only when the last byte
        // is a multibyte lead awaiting its continuation bytes (issue
        // #111: a bare lead was decoded as U+FFFD instead of held back).
        if len > 0 && utf8_width(buf[len - 1]) > 1 {
            return len - 1;
        }
        return len;
    }
    if conts == len {
        // Whole buffer is stray continuations: malformed, decode as-is.
        return len;
    }
    let width = utf8_width(buf[len - conts - 1]);
    if width > 1 && conts + 1 < width {
        // Lead plus too few continuations: hold the partial sequence.
        len - conts - 1
    } else {
        // Complete sequence, or stray continuations after a non-lead
        // (issue #111: complete emoji/CJK tails were held back and split
        // into U+FFFDs): decode as-is.
        len
    }
}

fn is_continuation(b: u8) -> bool {
    b & 0xC0 == 0x80
}

fn utf8_width(lead: u8) -> usize {
    if lead & 0x80 == 0 {
        1
    } else if lead & 0xE0 == 0xC0 {
        2
    } else if lead & 0xF0 == 0xE0 {
        3
    } else if lead & 0xF8 == 0xF0 {
        4
    } else {
        0
    }
}

fn push_utf8(buf: &mut Vec<u8>, c: char) {
    let mut tmp = [0u8; 4];
    buf.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
}

/// Pop one whole char off a UTF-8 byte buffer (backspace semantics).
fn pop_char(buf: &mut Vec<u8>) {
    if buf.is_empty() {
        return;
    }
    let mut len = buf.len();
    // Step back over continuation bytes to the lead byte.
    while len > 0 && is_continuation(buf[len - 1]) {
        len -= 1;
    }
    // Step over the lead byte itself (ASCII included: not a continuation).
    len = len.saturating_sub(1);
    buf.truncate(len);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_lines_split_on_newlines() {
        let mut log = ScrollbackLog::new();
        log.feed(b"hello\nworld\n");
        assert_eq!(log.line_count(), 2);
        let lines: Vec<&str> = log.lines().iter().map(String::as_str).collect();
        assert_eq!(lines, vec!["hello", "world"]);
        assert_eq!(log.pending_line(), "");
    }

    #[test]
    fn csi_colors_and_cursor_moves_are_stripped() {
        let mut log = ScrollbackLog::new();
        log.feed(b"\x1b[2J\x1b[1;1Htop \x1b[31mred\x1b[0m\n");
        let lines: Vec<&str> = log.lines().iter().map(String::as_str).collect();
        assert_eq!(lines, vec!["top red"]);
    }

    #[test]
    fn osc_hyperlinks_leave_only_text() {
        let mut log = ScrollbackLog::new();
        log.feed(b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07\n");
        let lines: Vec<&str> = log.lines().iter().map(String::as_str).collect();
        assert_eq!(lines, vec!["link"]);
    }

    #[test]
    fn crlf_is_one_newline_and_lone_cr_overwrites() {
        let mut log = ScrollbackLog::new();
        log.feed(b"one\r\ntwo\rxx\n");
        let lines: Vec<&str> = log.lines().iter().map(String::as_str).collect();
        assert_eq!(lines, vec!["one", "xx"]);
    }

    #[test]
    fn split_crlf_across_chunks_stays_one_line() {
        let mut log = ScrollbackLog::new();
        log.feed(b"one\r");
        log.feed(b"\n");
        let lines: Vec<&str> = log.lines().iter().map(String::as_str).collect();
        assert_eq!(lines, vec!["one"]);
    }

    #[test]
    fn multibyte_split_across_chunks_decodes_cleanly() {
        let mut log = ScrollbackLog::new();
        let bytes = "héllo ✓\n".as_bytes();
        log.feed(&bytes[..5]);
        // Incomplete tail is held back: nothing decodable is lost or
        // replaced yet beyond genuinely complete chars.
        log.feed(&bytes[5..]);
        let lines: Vec<&str> = log.lines().iter().map(String::as_str).collect();
        assert_eq!(lines, vec!["héllo ✓"]);
    }

    #[test]
    fn complete_multibyte_tail_decodes_cleanly() {
        // Issue #111 review focus: a chunk ending in a complete 3- or
        // 4-byte char must decode as-is, not held back as U+FFFD.
        let mut log = ScrollbackLog::new();
        log.feed("\u{2713}".as_bytes());
        log.feed("\u{1f60a}".as_bytes());
        log.feed(b"\n");
        let lines: Vec<&str> = log.lines().iter().map(String::as_str).collect();
        assert_eq!(lines, vec!["\u{2713}\u{1f60a}"]);
    }

    #[test]
    fn backspace_erases_one_char() {
        let mut log = ScrollbackLog::new();
        log.feed(b"abc\x08d\n");
        let lines: Vec<&str> = log.lines().iter().map(String::as_str).collect();
        assert_eq!(lines, vec!["abd"]);
    }

    #[test]
    fn retention_caps_at_2000_with_truncation_flag() {
        let mut log = ScrollbackLog::new();
        for n in 0..MAX_RETAINED_LINES + 100 {
            log.feed(format!("line {n}\n").as_bytes());
        }
        assert_eq!(log.lines().len(), MAX_RETAINED_LINES);
        assert!(log.truncated);
        assert_eq!(log.lines()[0], format!("line 100"));
    }

    #[test]
    fn pending_line_counts_until_terminated() {
        let mut log = ScrollbackLog::new();
        assert_eq!(log.line_count(), 0);
        log.feed(b"partial");
        assert_eq!(log.line_count(), 1);
        assert_eq!(log.pending_line(), "partial");
        log.feed(b"\n");
        assert_eq!(log.line_count(), 1);
    }

    #[test]
    fn retained_len_counts_sliceable_rows() {
        // Issue #111: the pager clamp, window, and slice share one
        // length. `retained_len` counts exactly the rows a `lines` +
        // non-empty `pending_line` snapshot can slice, including
        // wide/multibyte rows and a multibyte pending line.
        let mut log = ScrollbackLog::new();
        assert_eq!(log.retained_len(), 0);
        for n in 0..95 {
            log.feed(format!("row {n}\n").as_bytes());
        }
        log.feed("h\u{e9}llo \u{2713} done\n".as_bytes());
        assert_eq!(log.lines().len(), 96);
        assert_eq!(log.retained_len(), 96);
        log.feed("\u{65e5}\u{672c}\u{8a9e} pending \u{1f60a}".as_bytes());
        assert_eq!(
            log.pending_line(),
            "\u{65e5}\u{672c}\u{8a9e} pending \u{1f60a}"
        );
        assert_eq!(log.retained_len(), 97);
        log.feed(b"\n");
        assert_eq!(log.retained_len(), 97);
    }

    #[test]
    fn trailing_cr_without_content_adds_no_pager_row() {
        // Issue #111 root cause: a trailing `\r` with an empty current
        // line sets `cr_pending`, so `line_count` reported one row more
        // than any snapshot could slice. Clamping against that phantom
        // row mapped the top two offsets to the identical page.
        let mut log = ScrollbackLog::new();
        log.feed(b"a\nb\n\r");
        assert_eq!(log.pending_line(), "");
        assert_eq!(log.lines().len(), 2);
        assert_eq!(log.line_count(), 3, "legacy total keeps the phantom row");
        assert_eq!(log.retained_len(), 2, "pager truth has no phantom row");
    }
}
