//! vt100 screen → styled text rows for the gpui terminal pane.
//!
//! Framework-light by design (regression shield): everything here works on
//! plain vt100 types plus one tiny gpui conversion ([`to_hsla`]); every
//! mapping below has a unit test pinning it, so a gpui upgrade cannot
//! silently change terminal rendering.

/// 8-bit RGB triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb8(pub u8, pub u8, pub u8);

/// Cell style in framework-free form (`None` = terminal default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellStyle {
    pub fg: Option<Rgb8>,
    pub bg: Option<Rgb8>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// One coalesced same-style span of a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermSpan {
    pub text: String,
    pub style: CellStyle,
}

/// Standard xterm 16-color palette as RGB triples.
const PALETTE_16: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

/// Light-mode 16-color palette (issue #43): same hues as [`PALETTE_16`]
/// darkened so every entry stays readable on the light terminal surface.
/// Dark colors keep their values; near-white entries map to dark grays.
const PALETTE_16_LIGHT: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 130, 0),
    (122, 122, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 130, 130),
    (116, 116, 116),
    (85, 85, 85),
    (230, 0, 0),
    (0, 140, 0),
    (110, 110, 0),
    (92, 92, 255),
    (200, 0, 200),
    (0, 140, 140),
    (51, 51, 51),
];

/// Map a vt100 color to RGB (`None` = terminal default), picking the
/// dark or light 16-color palette by `light` (issue #43). The 256-color
/// cube and grays are absolute, so only the 16 base entries vary.
pub fn vt_color_for(color: vt100::Color, light: bool) -> Option<Rgb8> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => {
            let i = i as usize;
            if i < 16 {
                let (r, g, b) = if light {
                    PALETTE_16_LIGHT[i]
                } else {
                    PALETTE_16[i]
                };
                Some(Rgb8(r, g, b))
            } else if i < 232 {
                let v = i - 16;
                let comp = |c: usize| {
                    if c == 0 {
                        0
                    } else {
                        (55 + 40 * c) as u8
                    }
                };
                Some(Rgb8(comp(v / 36), comp((v % 36) / 6), comp(v % 6)))
            } else {
                let g = (8 + 10 * (i - 232)) as u8;
                Some(Rgb8(g, g, g))
            }
        }
        vt100::Color::Rgb(r, g, b) => Some(Rgb8(r, g, b)),
    }
}

/// Map a vt100 color to RGB on the dark palette (`None` = default).
/// The historic mapping: dark rendering resolves exactly as before
/// (issue #43).
pub fn vt_color(color: vt100::Color) -> Option<Rgb8> {
    vt_color_for(color, false)
}

/// Convert RGB to gpui HSL. Kept in-house (and tested) so terminal colors do
/// not depend on any gpui helper surviving upgrades.
pub fn to_hsla(Rgb8(r, g, b): Rgb8) -> gpui::Hsla {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < f32::EPSILON {
        return gpui::Hsla {
            h: 0.0,
            s: 0.0,
            l,
            a: 1.0,
        };
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() < f32::EPSILON {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if (max - g).abs() < f32::EPSILON {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    gpui::Hsla { h, s, l, a: 1.0 }
}

/// Terminal cell position: 0-based `(row, col)`. `col` may equal the grid
/// width to denote end-of-line (exclusive selection edge).
pub type CellPos = (u16, u16);

/// Order two cell endpoints so the first is the selection start (top, then
/// left). Pure so mouse-drag direction never matters to callers.
pub fn normalize_selection(a: CellPos, b: CellPos) -> (CellPos, CellPos) {
    if (a.0, a.1) <= (b.0, b.1) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Map a window-space point to a terminal cell. `origin` is the window-space
/// top-left of the terminal text (the StyledText bounds), `char_w`/`line_h`
/// the monospace cell size, `cols`/`rows` the live grid size. The result is
/// clamped into the grid; `col` may be `cols` (exclusive line end) so a drag
/// past the right edge still selects to end-of-line.
pub fn point_to_cell(
    mouse: (f32, f32),
    origin: (f32, f32),
    char_w: f32,
    line_h: f32,
    cols: u16,
    rows: u16,
) -> CellPos {
    if char_w <= 0.0 || line_h <= 0.0 || cols == 0 || rows == 0 {
        return (0, 0);
    }
    let row = ((mouse.1 - origin.1) / line_h).floor() as i32;
    let col = ((mouse.0 - origin.0) / char_w).floor() as i32;
    let row = row.clamp(0, rows as i32 - 1) as u16;
    let col = col.clamp(0, cols as i32) as u16;
    (row, col)
}

/// Split a normalized multi-row selection into per-row `(row, start_col,
/// end_col)` spans with exclusive ends, for highlight overlays and tests.
/// `cols` is the grid width used to fill interior rows.
pub fn selection_rows(start: CellPos, end: CellPos, cols: u16) -> Vec<(u16, u16, u16)> {
    let (start, end) = normalize_selection(start, end);
    if start == end {
        return vec![];
    }
    if start.0 == end.0 {
        return vec![(start.0, start.1.min(cols), end.1.min(cols))];
    }
    let mut out = Vec::new();
    out.push((start.0, start.1.min(cols), cols));
    for r in start.0.saturating_add(1)..end.0 {
        out.push((r, 0, cols));
    }
    out.push((end.0, 0, end.1.min(cols)));
    out
}

/// Selected terminal text between two cells, via the emulator's own
/// [`vt100::Screen::contents_between`] (handles wrapping and wide cells).
/// Returns an empty string for an empty (same-cell) selection. Trailing
/// blank padding is stripped per line and at the end (issue #42), so a
/// selection dragged past end-of-line copies only shown characters.
pub fn selection_text(screen: &vt100::Screen, anchor: CellPos, active: CellPos) -> String {
    let (start, end) = normalize_selection(anchor, active);
    if start == end {
        return String::new();
    }
    let raw = screen.contents_between(start.0, start.1, end.0, end.1);
    raw.split('\n')
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

/// Shown-character span of one grid row (issue #42): the inclusive
/// start and exclusive end columns of the row's non-blank content, or
/// `None` when the row shows nothing. Blank means an empty cell or a
/// whitespace-only cell (trailing spaces are padding, not shown text).
/// Wide characters count two cells via their leading edge, so the span
/// never splits a 2-cell character.
pub fn row_content_range(screen: &vt100::Screen, row: u16) -> Option<(u16, u16)> {
    let (_, cols) = screen.size();
    if row >= screen.size().0 {
        return None;
    }
    let mut span: Option<(u16, u16)> = None;
    for c in 0..cols {
        let Some(cell) = screen.cell(row, c) else {
            continue;
        };
        if cell.is_wide_continuation() || cell.contents().trim().is_empty() {
            continue;
        }
        let end = c
            .saturating_add(if cell.is_wide() { 2 } else { 1 })
            .min(cols);
        span = Some(match span {
            Some((s, _)) => (s, end.max(s)),
            None => (c, end),
        });
    }
    span
}

/// Snap a mousedown cell to shown characters (issue #42): `Some` only
/// when the press lands inside the row's shown-character span (a
/// continuation cell snaps back to its leading edge). Presses on empty
/// rows, leading/trailing padding, or past end-of-line return `None` so
/// the caller arms no selection there.
pub fn snap_press_to_content(screen: &vt100::Screen, cell: CellPos) -> Option<CellPos> {
    let (row, col) = cell;
    let (start, end) = row_content_range(screen, row)?;
    if col < start || col >= end {
        return None;
    }
    if col > 0
        && screen
            .cell(row, col)
            .is_some_and(|c| c.is_wide_continuation())
    {
        return Some((row, col.saturating_sub(1)));
    }
    Some((row, col))
}

/// Snap a drag cell to shown characters (issue #42): the column clamps
/// into the row's shown-character span (a continuation cell snaps back
/// to its leading edge), so drags leaving the text block stop at the
/// last shown character. Rows showing nothing return `None` so the
/// caller keeps the previous endpoint instead of extending over void.
pub fn snap_drag_to_content(screen: &vt100::Screen, cell: CellPos) -> Option<CellPos> {
    let (row, col) = cell;
    let (start, end) = row_content_range(screen, row)?;
    let col = col.clamp(start, end);
    if col < end
        && col > 0
        && screen
            .cell(row, col)
            .is_some_and(|c| c.is_wide_continuation())
    {
        return Some((row, col.saturating_sub(1)));
    }
    Some((row, col))
}

/// Per-row highlight spans for a mouse selection that cover shown
/// characters only (issue #42): each row's span is intersected with its
/// content range, and rows showing nothing contribute no span. Callers
/// pass already-snapped endpoints; unsnapped columns still clamp safely.
pub fn selection_highlight_rows(
    screen: &vt100::Screen,
    start: CellPos,
    end: CellPos,
) -> Vec<(u16, u16, u16)> {
    let (start, end) = normalize_selection(start, end);
    if start == end {
        return vec![];
    }
    let mut out = Vec::new();
    if start.0 == end.0 {
        if let Some((s, e)) = row_content_range(screen, start.0) {
            let (a, b) = (start.1.max(s).min(e), end.1.max(s).min(e));
            if a < b {
                out.push((start.0, a, b));
            }
        }
        return out;
    }
    if let Some((s, e)) = row_content_range(screen, start.0) {
        let a = start.1.max(s).min(e);
        if a < e {
            out.push((start.0, a, e));
        }
    }
    for r in start.0.saturating_add(1)..end.0 {
        if let Some((s, e)) = row_content_range(screen, r) {
            out.push((r, s, e));
        }
    }
    if let Some((s, e)) = row_content_range(screen, end.0) {
        let b = end.1.max(s).min(e);
        if s < b {
            out.push((end.0, s, b));
        }
    }
    out
}

/// Fingerprint of everything [`screen_rows`] renders: grid size, the
/// resolved cursor, and the formatted grid (text plus styles, in one
/// allocation). vt100 exposes no generation counter, so the render cache
/// keys on this instead of rebuilding rows (thousands of allocations)
/// every frame. Hashing the *formatted* grid — not plain `contents()` —
/// keeps style-only changes (a moved highlight with identical text) from
/// going stale.
pub fn screen_fingerprint(screen: &vt100::Screen, cursor: Option<(u16, u16)>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    screen.size().hash(&mut h);
    cursor.hash(&mut h);
    screen.contents_formatted().hash(&mut h);
    h.finish()
}

/// Render the emulated screen grid as rows of coalesced spans. `cursor` is
/// the 0-based cursor cell, painted with `cursor_bg` when given (this is how
/// the child tool's caret stays visible). `light` picks the light 16-color
/// palette variant so explicit colors stay readable on a light surface
/// (issue #43); default-color cells keep `None` styles for the caller to
/// resolve against the themed default foreground.
pub fn screen_rows(
    screen: &vt100::Screen,
    cursor: Option<(u16, u16)>,
    cursor_bg: Rgb8,
    light: bool,
) -> Vec<Vec<TermSpan>> {
    let (rows, cols) = screen.size();
    let mut out = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let mut spans: Vec<TermSpan> = Vec::new();
        let mut buf = String::new();
        let mut cur = CellStyle {
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
        };
        let mut open = false;
        for c in 0..cols {
            let Some(cell) = screen.cell(r, c) else {
                break;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            // Dark resolves through the historic entry point, so dark
            // rendering is unchanged by construction (issue #43).
            let (mut fg, mut bg) = if light {
                (
                    vt_color_for(cell.fgcolor(), true),
                    vt_color_for(cell.bgcolor(), true),
                )
            } else {
                (vt_color(cell.fgcolor()), vt_color(cell.bgcolor()))
            };
            if cell.inverse() {
                std::mem::swap(&mut fg, &mut bg);
            }
            if cursor == Some((r, c)) {
                bg = Some(cursor_bg);
            }
            let style = CellStyle {
                fg,
                bg,
                bold: cell.bold(),
                italic: cell.italic(),
                underline: cell.underline(),
            };
            if !open {
                cur = style;
                open = true;
            } else if style != cur {
                spans.push(TermSpan {
                    text: std::mem::take(&mut buf),
                    style: cur,
                });
                cur = style;
            }
            // Never-written grid cells carry no contents; render the gap
            // as a space so cursor-addressed words keep their separation
            // (issue #104: "Runningthetest" in the spans-fed shells).
            let text = cell.contents();
            if text.is_empty() {
                buf.push(' ');
            } else {
                buf.push_str(&text);
            }
        }
        if open {
            spans.push(TermSpan {
                text: buf,
                style: cur,
            });
        }
        out.push(spans);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_red_maps_to_xterm_value() {
        assert_eq!(vt_color(vt100::Color::Idx(1)), Some(Rgb8(205, 0, 0)));
        assert_eq!(vt_color(vt100::Color::Idx(9)), Some(Rgb8(255, 0, 0)));
    }

    #[test]
    fn default_maps_to_none_and_rgb_passes_through() {
        assert_eq!(vt_color(vt100::Color::Default), None);
        assert_eq!(vt_color(vt100::Color::Rgb(1, 2, 3)), Some(Rgb8(1, 2, 3)));
    }

    #[test]
    fn cube_and_grayscale_ramps() {
        // 196 = 5,0,0 → pure red; 232 = first gray step.
        assert_eq!(vt_color(vt100::Color::Idx(196)), Some(Rgb8(255, 0, 0)));
        assert_eq!(vt_color(vt100::Color::Idx(232)), Some(Rgb8(8, 8, 8)));
        assert_eq!(vt_color(vt100::Color::Idx(255)), Some(Rgb8(238, 238, 238)));
    }

    #[test]
    fn rgb_to_hsl_primaries() {
        let gpui::Hsla { h, s, l, .. } = to_hsla(Rgb8(255, 0, 0));
        assert!((h - 0.0).abs() < 1e-5 && (s - 1.0).abs() < 1e-5 && (l - 0.5).abs() < 1e-5);
        let gpui::Hsla { s, l, .. } = to_hsla(Rgb8(255, 255, 255));
        assert!((s - 0.0).abs() < 1e-5 && (l - 1.0).abs() < 1e-5);
        let gpui::Hsla { l, .. } = to_hsla(Rgb8(0, 0, 0));
        assert!((l - 0.0).abs() < 1e-5);
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive_to_text_style_and_cursor() {
        let mut a = vt100::Parser::new(24, 80, 0);
        a.process(b"hello");
        let mut b = vt100::Parser::new(24, 80, 0);
        b.process(b"hello");
        // Same bytes, same cursor: identical fingerprint (cache hit).
        assert_eq!(
            screen_fingerprint(a.screen(), Some((0, 5))),
            screen_fingerprint(b.screen(), Some((0, 5)))
        );
        // New text changes it.
        b.process(b"!");
        assert_ne!(
            screen_fingerprint(a.screen(), Some((0, 5))),
            screen_fingerprint(b.screen(), Some((0, 6)))
        );
        // A moved cursor alone changes it (the caret cell repaints).
        let mut c = vt100::Parser::new(24, 80, 0);
        c.process(b"hello");
        assert_ne!(
            screen_fingerprint(a.screen(), Some((0, 5))),
            screen_fingerprint(c.screen(), Some((0, 0)))
        );
        // Style-only change with identical text changes it: recoloring
        // "hello" red must not reuse the unstyled frame.
        let mut d = vt100::Parser::new(24, 80, 0);
        d.process(b"\x1b[31mhello\x1b[0m");
        assert_ne!(
            screen_fingerprint(a.screen(), None),
            screen_fingerprint(d.screen(), None)
        );
    }

    #[test]
    fn addressed_text_lands_in_its_row() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\x1b[2J\x1b[1;1Htop-left\x1b[10;20Hmid");
        let rows = screen_rows(parser.screen(), None, Rgb8(255, 255, 255), false);
        let row0: String = rows[0].iter().map(|s| s.text.as_str()).collect();
        let row9: String = rows[9].iter().map(|s| s.text.as_str()).collect();
        assert!(row0.contains("top-left"), "row0 was {row0:?}");
        assert!(row9.contains("mid"), "row9 was {row9:?}");
    }

    #[test]
    fn screen_rows_preserves_spaces_between_addressed_words() {
        // Same contract as the core spans snapshot (issue #104): gaps
        // between cursor-addressed words render as spaces.
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\x1b[1;1H\xe2\x97\x8f\x1b[1;3HRunning\x1b[1;11Hthe");
        let rows = screen_rows(parser.screen(), None, Rgb8(255, 255, 255), false);
        let line: String = rows[0].iter().map(|s| s.text.as_str()).collect();
        assert!(
            line.starts_with("\u{25cf} Running the"),
            "gaps stay spaces, got {line:?}"
        );
    }

    #[test]
    fn inverse_swaps_fg_to_bg() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\x1b[31;7mX");
        let rows = screen_rows(parser.screen(), None, Rgb8(255, 255, 255), false);
        let span = rows[0].iter().find(|s| s.text.contains('X')).unwrap();
        assert_eq!(span.style.fg, None);
        assert_eq!(span.style.bg, Some(Rgb8(205, 0, 0)));
    }

    #[test]
    fn cursor_cell_gets_cursor_background() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"AB");
        // Cursor sits after "AB" → cell (0,2), which is blank.
        let rows = screen_rows(parser.screen(), Some((0, 2)), Rgb8(200, 200, 200), false);
        let flat: Vec<&TermSpan> = rows[0].iter().collect();
        assert!(flat.iter().any(|s| s.style.bg == Some(Rgb8(200, 200, 200))));
    }

    #[test]
    fn selection_endpoints_normalize_regardless_of_drag_direction() {
        assert_eq!(normalize_selection((2, 9), (0, 1)), ((0, 1), (2, 9)));
        assert_eq!(normalize_selection((1, 3), (1, 7)), ((1, 3), (1, 7)));
        assert_eq!(normalize_selection((1, 7), (1, 3)), ((1, 3), (1, 7)));
    }

    #[test]
    fn point_maps_to_cells_and_clamps_to_grid() {
        // 10px chars, 20px lines, origin at (100, 50), grid 80x24.
        assert_eq!(
            point_to_cell((115.0, 62.0), (100.0, 50.0), 10.0, 20.0, 80, 24),
            (0, 1)
        );
        assert_eq!(
            point_to_cell((100.0, 50.0), (100.0, 50.0), 10.0, 20.0, 80, 24),
            (0, 0)
        );
        // Above/left clamps to origin; far below/right clamps to grid edge
        // (col may be `cols` for an exclusive end-of-line edge).
        assert_eq!(
            point_to_cell((0.0, 0.0), (100.0, 50.0), 10.0, 20.0, 80, 24),
            (0, 0)
        );
        assert_eq!(
            point_to_cell((9999.0, 9999.0), (100.0, 50.0), 10.0, 20.0, 80, 24),
            (23, 80)
        );
        assert_eq!(
            point_to_cell((0.0, 0.0), (0.0, 0.0), 0.0, 20.0, 80, 24),
            (0, 0)
        );
    }

    #[test]
    fn multi_row_selection_splits_into_per_row_spans() {
        assert!(selection_rows((1, 2), (1, 2), 80).is_empty());
        assert_eq!(selection_rows((1, 2), (1, 5), 80), vec![(1, 2, 5)]);
        assert_eq!(
            selection_rows((0, 70), (2, 10), 80),
            vec![(0, 70, 80), (1, 0, 80), (2, 0, 10)]
        );
        // Reverse drag normalizes the same way.
        assert_eq!(
            selection_rows((2, 10), (0, 70), 80),
            vec![(0, 70, 80), (1, 0, 80), (2, 0, 10)]
        );
    }

    #[test]
    fn selected_text_comes_from_emulator_between_cells() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"hello world");
        let text = selection_text(parser.screen(), (0, 0), (0, 5));
        assert_eq!(text, "hello");
        assert!(selection_text(parser.screen(), (0, 3), (0, 3)).is_empty());
        // Reversed endpoints select the same text.
        assert_eq!(selection_text(parser.screen(), (0, 5), (0, 0)), "hello");
    }

    #[test]
    fn colored_span_carries_its_style() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\x1b[31mred-text\x1b[0m");
        let rows = screen_rows(parser.screen(), None, Rgb8(255, 255, 255), false);
        let span = rows[0]
            .iter()
            .find(|s| s.text.contains("red-text"))
            .expect("colored span survives");
        assert_eq!(span.style.fg, Some(Rgb8(205, 0, 0)));
    }

    #[test]
    fn dark_palette_is_unchanged_and_light_variant_darkens_brights() {
        // Issue #43: the historic mapping is the dark palette; light mode
        // keeps dark hues and maps near-white entries to dark grays.
        assert_eq!(vt_color(vt100::Color::Idx(1)), Some(Rgb8(205, 0, 0)));
        assert_eq!(
            vt_color_for(vt100::Color::Idx(1), false),
            vt_color(vt100::Color::Idx(1))
        );
        assert_eq!(
            vt_color_for(vt100::Color::Idx(1), true),
            Some(Rgb8(205, 0, 0))
        );
        // Bright green/white would vanish on a light surface.
        assert_ne!(
            vt_color_for(vt100::Color::Idx(10), true),
            vt_color_for(vt100::Color::Idx(10), false)
        );
        assert_eq!(
            vt_color_for(vt100::Color::Idx(15), true),
            Some(Rgb8(51, 51, 51))
        );
        assert_eq!(vt_color_for(vt100::Color::Default, true), None);
        // Extended colors are absolute in both modes.
        assert_eq!(
            vt_color_for(vt100::Color::Idx(196), true),
            Some(Rgb8(255, 0, 0))
        );
        assert_eq!(
            vt_color_for(vt100::Color::Rgb(1, 2, 3), true),
            Some(Rgb8(1, 2, 3))
        );
    }

    #[test]
    fn light_palette_stays_readable_on_the_light_surface() {
        // Issue #43: every light-palette entry keeps contrast >= 3.5
        // against the light terminal surface (accents, not body text).
        use super::super::shell::{term_theme, LIGHT_TERMINAL_BG};
        use super::super::theme::contrast_ratio;
        let bg = LIGHT_TERMINAL_BG;
        assert!(bg != super::super::shell::DARK_TERMINAL_BG);
        for i in 0..16u8 {
            let Rgb8(r, g, b) =
                vt_color_for(vt100::Color::Idx(i), true).expect("palette entry maps");
            let fg = (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
            let ratio = contrast_ratio(fg, bg);
            assert!(ratio >= 3.5, "light palette {i} ratio {ratio:.2}");
        }
        // Default text clears the 4.5 body-text floor in both modes.
        let light = term_theme(true);
        let light_fg =
            (u32::from(light.fg.0) << 16) | (u32::from(light.fg.1) << 8) | u32::from(light.fg.2);
        assert!(contrast_ratio(light_fg, light.bg) >= 4.5);
        let dark = term_theme(false);
        let dark_fg =
            (u32::from(dark.fg.0) << 16) | (u32::from(dark.fg.1) << 8) | u32::from(dark.fg.2);
        assert!(contrast_ratio(dark_fg, dark.bg) >= 4.5);
        // Dark theme values are the historic constants.
        assert_eq!(dark.bg, super::super::shell::DARK_TERMINAL_BG);
        assert_eq!(dark.fg, super::super::shell::DEFAULT_FG);
        assert_eq!(dark.cursor, super::super::shell::CURSOR_BG);
        assert_eq!(dark.selection, super::super::shell::SELECTION_BG);
    }

    #[test]
    fn content_range_skips_padding_and_counts_wide_cells() {
        // Issue #42: endpoint snapping works from the shown characters.
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"hi");
        let screen = parser.screen();
        assert_eq!(row_content_range(screen, 0), Some((0, 2)));
        // Untouched rows show nothing.
        assert_eq!(row_content_range(screen, 5), None);
        // Out-of-grid rows show nothing.
        assert_eq!(row_content_range(screen, 99), None);
        // Trailing spaces are padding, not shown text.
        let mut spaced = vt100::Parser::new(24, 80, 0);
        spaced.process(b"hi   ");
        assert_eq!(row_content_range(spaced.screen(), 0), Some((0, 2)));
        // Cursor-addressed text mid-row starts its span at the text.
        let mut moved = vt100::Parser::new(24, 80, 0);
        moved.process(b"\x1b[1;11Hmid");
        assert_eq!(row_content_range(moved.screen(), 0), Some((10, 13)));
        // A wide character occupies two cells via its leading edge.
        let mut wide = vt100::Parser::new(24, 80, 0);
        wide.process("你好".as_bytes());
        assert_eq!(row_content_range(wide.screen(), 0), Some((0, 4)));
    }

    #[test]
    fn press_arms_only_on_shown_characters() {
        // Issue #42: mousedown on empty padding arms nothing.
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"hi");
        let screen = parser.screen();
        assert_eq!(snap_press_to_content(screen, (0, 0)), Some((0, 0)));
        assert_eq!(snap_press_to_content(screen, (0, 1)), Some((0, 1)));
        // Past end-of-line and on empty rows: no selection arms.
        assert_eq!(snap_press_to_content(screen, (0, 2)), None);
        assert_eq!(snap_press_to_content(screen, (0, 40)), None);
        assert_eq!(snap_press_to_content(screen, (5, 0)), None);
        // A wide continuation snaps back to its leading edge.
        let mut wide = vt100::Parser::new(24, 80, 0);
        wide.process("hi你".as_bytes());
        let screen = wide.screen();
        assert_eq!(row_content_range(screen, 0), Some((0, 4)));
        assert_eq!(snap_press_to_content(screen, (0, 3)), Some((0, 2)));
    }

    #[test]
    fn drag_clamps_to_last_shown_character() {
        // Issue #42: drags leaving the text block stop at shown text.
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"hi");
        let screen = parser.screen();
        assert_eq!(snap_drag_to_content(screen, (0, 0)), Some((0, 0)));
        assert_eq!(snap_drag_to_content(screen, (0, 40)), Some((0, 2)));
        // Empty rows contribute nothing (caller keeps the last endpoint).
        assert_eq!(snap_drag_to_content(screen, (5, 40)), None);
        // A drag into a continuation snaps to the leading edge.
        let mut wide = vt100::Parser::new(24, 80, 0);
        wide.process("hi你".as_bytes());
        let screen = wide.screen();
        assert_eq!(snap_drag_to_content(screen, (0, 3)), Some((0, 2)));
        assert_eq!(snap_drag_to_content(screen, (0, 79)), Some((0, 4)));
    }

    #[test]
    fn highlight_covers_shown_characters_only() {
        // Issue #42: the overlay never extends over padding or void.
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"hello\r\nworld\r\n\r\nlast");
        let screen = parser.screen();
        // Past end-of-line clamps to the shown text.
        assert_eq!(
            selection_highlight_rows(screen, (0, 0), (0, 79)),
            vec![(0, 0, 5)]
        );
        // Empty interior rows contribute no span.
        assert_eq!(
            selection_highlight_rows(screen, (1, 0), (3, 4)),
            vec![(1, 0, 5), (3, 0, 4)]
        );
        // A selection over only void selects nothing visible.
        assert!(selection_highlight_rows(screen, (2, 0), (2, 79)).is_empty());
        assert!(selection_highlight_rows(screen, (1, 1), (1, 1)).is_empty());
        // Reverse drags highlight the same spans.
        assert_eq!(
            selection_highlight_rows(screen, (0, 79), (0, 0)),
            vec![(0, 0, 5)]
        );
    }

    #[test]
    fn copied_text_has_no_trailing_blank_padding() {
        // Issue #42: dragging past end-of-line copies shown text only.
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"hello\r\nworld");
        let screen = parser.screen();
        assert_eq!(selection_text(screen, (0, 0), (0, 79)), "hello");
        assert_eq!(selection_text(screen, (0, 0), (1, 79)), "hello\nworld");
        // Multi-row selections keep interior newlines but shed padding.
        assert_eq!(selection_text(screen, (0, 2), (1, 3)), "llo\nwor");
    }
}
