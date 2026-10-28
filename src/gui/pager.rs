//! Scrollback pager over the retained output buffer.
//!
//! Each live run mirrors its PTY output into a [`ScrollbackLog`] (issue
//! #25; the vt100 0.15 API exposes no scrollback rows, so the mirror is
//! the retained buffer). `Shift+PgUp`/`Shift+PgDn` move a per-run line
//! offset up from the live bottom; offset 0 is the live screen. New
//! output snaps back to the bottom. Selection and copy work over pager
//! rows exactly like the live grid. (Wheel scrolling needs a paint-phase
//! mouse listener gpui 0.2 only offers to elements, so keys are the
//! binding; the offset math here stays wheel-ready.)

use super::shell::ShellView;
use super::terminal::{CellPos, TermSpan};

/// Largest legal offset: total retained lines minus one visible page (0
/// when everything fits on screen).
pub fn max_scroll_offset(total_lines: usize, visible_rows: usize) -> usize {
    total_lines.saturating_sub(visible_rows.max(1))
}

/// Clamp an offset into the legal range for this buffer size.
pub fn clamp_scroll(offset: usize, total_lines: usize, visible_rows: usize) -> usize {
    offset.min(max_scroll_offset(total_lines, visible_rows))
}

/// Visible window `[start, end)` over retained lines for an offset up from
/// the live bottom.
pub fn scroll_window(total_lines: usize, visible_rows: usize, offset: usize) -> (usize, usize) {
    let offset = clamp_scroll(offset, total_lines, visible_rows);
    let end = total_lines.saturating_sub(offset);
    let start = end.saturating_sub(visible_rows.max(1));
    (start, end)
}

/// Selected text over plain pager rows (same exclusive-edge semantics as
/// the emulator's `contents_between`): first row from `start_col`, last
/// row up to `end_col`, interior rows whole. Columns clamp per row in
/// chars, so wide/multibyte text never panics on byte boundaries.
pub fn pager_selection_text(rows: &[String], start: CellPos, end: CellPos) -> String {
    let (mut start, mut end) = super::terminal::normalize_selection(start, end);
    if start == end || rows.is_empty() {
        return String::new();
    }
    start.0 = start.0.min(rows.len().saturating_sub(1) as u16);
    end.0 = end.0.min(rows.len().saturating_sub(1) as u16);
    let mut out = String::new();
    for r in start.0..=end.0 {
        let chars: Vec<char> = rows[r as usize].chars().collect();
        let from = if r == start.0 { start.1 as usize } else { 0 };
        let to = if r == end.0 {
            end.1 as usize
        } else {
            chars.len()
        };
        let from = from.min(chars.len());
        let to = to.min(chars.len());
        if from < to {
            out.extend(chars[from..to].iter());
        }
        if r != end.0 {
            out.push('\n');
        }
    }
    out
}

impl ShellView {
    /// Active run's pager offset (0 = live screen).
    pub(crate) fn pager_offset(&self) -> usize {
        self.active_id()
            .as_ref()
            .and_then(|id| self.runs.get(id))
            .map(|run| run.scroll_offset)
            .unwrap_or(0)
    }

    /// Retained lines for the pager: the mirror plus the in-progress line.
    fn retained_rows(&self) -> Option<Vec<String>> {
        let id = self.active_id()?;
        let run = self.runs.get(&id)?;
        let log = run.pty.scrollback_log();
        let mut rows: Vec<String> = log.lines().iter().cloned().collect();
        let pending = log.pending_line();
        if !pending.is_empty() {
            rows.push(pending);
        }
        Some(rows)
    }

    /// The visible pager slice when scrolled up, else `None` (the live
    /// screen renders). Always a full page unless the buffer is shorter.
    pub(crate) fn pager_text_rows(&self) -> Option<Vec<String>> {
        let offset = self.pager_offset();
        if offset == 0 {
            return None;
        }
        let rows = self.retained_rows()?;
        let (start, end) = scroll_window(rows.len(), self.rows as usize, offset);
        // Defense-in-depth (issue #111): the window already derives from
        // this snapshot's length, so this clamp is a no-op. It stays so a
        // future length-source split fails as a short page, never a panic.
        let end = end.min(rows.len());
        Some(rows[start..end].to_vec())
    }

    /// Pager slice as single-span rows for the shared text layout.
    pub(crate) fn pager_spans(&self) -> Option<Vec<Vec<TermSpan>>> {
        self.pager_text_rows().map(|rows| {
            rows.into_iter()
                .map(|line| {
                    vec![TermSpan {
                        text: line,
                        style: super::terminal::CellStyle {
                            fg: None,
                            bg: None,
                            bold: false,
                            italic: false,
                            underline: false,
                        },
                    }]
                })
                .collect()
        })
    }

    /// Header suffix naming the pager position (`""` when live).
    pub(crate) fn pager_note(&self) -> String {
        let offset = self.pager_offset();
        if offset == 0 {
            return String::new();
        }
        let total = self
            .active_id()
            .as_ref()
            .and_then(|id| self.runs.get(id))
            .map(|run| run.pty.scrollback_log().retained_len())
            .unwrap_or(0);
        format!(" · scroll {offset}/{total}")
    }

    /// Text covered by the mouse selection when paged up, for copy.
    pub(crate) fn selected_pager_text(&self) -> Option<String> {
        let rows = self.pager_text_rows()?;
        let (start, end) = self.selection_pair()?;
        let text = pager_selection_text(&rows, start, end);
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// Move the pager by `delta` lines (positive = up into history),
    /// clamped to the retained buffer. Clears the mouse selection: its
    /// cells refer to replaced content.
    pub(crate) fn scroll_by(&mut self, delta: i32) {
        let Some(id) = self.active_id() else {
            return;
        };
        let rows = self.rows as usize;
        let Some(run) = self.runs.get_mut(&id) else {
            return;
        };
        // Clamp against the sliceable length (issue #111): `line_count`
        // can exceed it by one when a trailing CR pends with no content.
        let total = run.pty.scrollback_log().retained_len();
        let max = max_scroll_offset(total, rows);
        let next = (run.scroll_offset as i32 + delta).clamp(0, max as i32);
        run.scroll_offset = next as usize;
        self.clear_selection();
    }

    /// Page up (`up == true`) or down by one screenful.
    pub(crate) fn page_scrollback(&mut self, up: bool) {
        let step = self.rows.max(1) as i32;
        self.scroll_by(if up { step } else { -step });
    }
}

#[cfg(test)]
mod tests {
    use super::super::runs::{insert_test_pty, test_shell};
    use super::*;

    #[test]
    fn window_math_clamps_at_both_ends() {
        // 100 lines, 30-row page: bottom window, mid window, top clamp.
        assert_eq!(scroll_window(100, 30, 0), (70, 100));
        assert_eq!(scroll_window(100, 30, 10), (60, 90));
        assert_eq!(scroll_window(100, 30, 1000), (0, 30));
        // Short buffer: everything shows, offset clamps to 0.
        assert_eq!(scroll_window(10, 30, 5), (0, 10));
        assert_eq!(max_scroll_offset(10, 30), 0);
        assert_eq!(clamp_scroll(99, 100, 30), 70);
    }

    #[test]
    fn pager_selection_copies_scrolled_rows() {
        let rows = vec![
            "hello".to_string(),
            "world".to_string(),
            "again".to_string(),
        ];
        assert_eq!(pager_selection_text(&rows, (0, 0), (0, 5)), "hello");
        assert_eq!(pager_selection_text(&rows, (0, 3), (1, 2)), "lo\nwo");
        assert_eq!(pager_selection_text(&rows, (1, 2), (0, 3)), "lo\nwo");
        assert_eq!(pager_selection_text(&rows, (0, 0), (0, 0)), "");
        // Columns clamp per row instead of panicking.
        assert_eq!(pager_selection_text(&rows, (0, 0), (0, 99)), "hello");
    }

    #[test]
    fn paging_up_reveals_scrolled_off_output_for_copy() {
        let mut view = test_shell();
        let id = insert_test_pty(&mut view, "seq", &["1", "120"]);
        // Pump until the mirror retained the whole run.
        for _ in 0..200 {
            view.refresh();
            let count = view.runs[&id].pty.scrollback_log().line_count();
            if count >= 120 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(view.runs[&id].pty.scrollback_log().line_count() >= 120);
        assert_eq!(view.pager_offset(), 0);
        assert!(view.pager_text_rows().is_none(), "live at offset 0");
        // Page up: scrolled-off lines appear and stay copyable.
        view.page_scrollback(true);
        assert!(view.pager_offset() > 0);
        let page = view.pager_text_rows().expect("pager slice");
        assert_eq!(page.len(), view.rows as usize);
        assert!(page[0].trim().parse::<u32>().is_ok(), "numbered seq rows");
        view.sel_anchor = Some((0, 0));
        view.sel_active = Some((0, 1));
        let copied = view.selected_pager_text().expect("copyable");
        assert_eq!(copied, page[0].chars().take(1).collect::<String>());
        // Paging down past the bottom returns to the live screen.
        view.scroll_by(-100000);
        assert_eq!(view.pager_offset(), 0);
        assert!(view.pager_text_rows().is_none());
    }

    #[test]
    fn fresh_output_snaps_a_paged_run_back_to_live() {
        let mut view = test_shell();
        // Two-stage output with a pause: page up during the quiet gap,
        // then the second burst must snap back to live.
        let id = insert_test_pty(&mut view, "sh", &["-c", "seq 1 60; sleep 2; seq 61 120"]);
        for _ in 0..100 {
            view.refresh();
            if view.runs[&id].pty.scrollback_log().line_count() >= 60 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        view.page_scrollback(true);
        assert!(view.pager_offset() > 0);
        for _ in 0..200 {
            view.refresh();
            if view.pager_offset() == 0 && view.runs[&id].pty.scrollback_log().line_count() >= 120 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(view.pager_offset(), 0);
        assert!(view.runs[&id].pty.scrollback_log().line_count() >= 120);
    }

    #[test]
    fn pager_never_repeats_rows_with_pending_line() {
        // Issue #111: every offset over a buffer holding numbered rows,
        // multibyte rows, and a non-empty pending line yields a full page
        // inside the snapshot, and consecutive offsets shift by exactly
        // one row (no repeats, no gaps, no overruns).
        let mut view = test_shell();
        let id = insert_test_pty(
            &mut view,
            "sh",
            &[
                "-c",
                "seq 1 90; printf 'h\\303\\251llo \\342\\234\\223\\n\\346\\227\\245\\346\\234\\254\\350\\252\\236\\350\\241\\214\\ntail-pending'",
            ],
        );
        for _ in 0..200 {
            view.refresh();
            if view.runs[&id].pty.scrollback_log().retained_len() >= 93 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let total = view.runs[&id].pty.scrollback_log().retained_len();
        assert!(total >= 93, "mirror kept rows plus pending, got {total}");
        let visible = view.rows as usize;
        let max = total.saturating_sub(visible.max(1));
        assert!(max > 2, "buffer must exceed one page");
        assert_eq!(view.pager_offset(), 0);
        assert!(view.pager_text_rows().is_none(), "live at offset 0");
        // The stored clamp shares the slice's length truth: pushing past
        // the top lands on the top page, never past it.
        view.scroll_by(100000);
        assert_eq!(view.runs[&id].scroll_offset, max);
        let mut prev: Option<Vec<String>> = None;
        for offset in 1..=max {
            view.runs.get_mut(&id).unwrap().scroll_offset = offset;
            let page = view.pager_text_rows().expect("pager slice");
            assert_eq!(page.len(), visible, "full page at offset {offset}");
            if let Some(p) = prev {
                assert_eq!(p[1..], page[..visible - 1], "offset {offset} repeats a row");
            }
            prev = Some(page);
        }
        // Bottom page ends at the last complete (multibyte) row; the top
        // page starts at row 1. The pending line is counted in `total`.
        view.runs.get_mut(&id).unwrap().scroll_offset = 1;
        let bottom = view.pager_text_rows().expect("bottom page");
        assert_eq!(bottom[visible - 1], "\u{65e5}\u{672c}\u{8a9e}\u{884c}");
        assert!(bottom.contains(&"h\u{e9}llo \u{2713}".to_string()));
        view.runs.get_mut(&id).unwrap().scroll_offset = max;
        let top = view.pager_text_rows().expect("top page");
        assert_eq!(top[0], "1");
    }
}
