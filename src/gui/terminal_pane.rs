//! Terminal-pane render + mouse selection for [`super::shell::ShellView`].
//!
//! Hand-rolled gpui terminal: vt100 screen → styled rows, drag-to-select
//! with copy-on-select, clipboard copy/paste, the sticky-error banner,
//! the first-run empty pane, PTY sizing, and the text-partition helper
//! behind it. Framework-free mapping details stay in [`super::terminal`].

use gpui::{
    div, px, rgb, App as GpuiApp, ClipboardItem, Context, ElementId, Font, FontFallbacks,
    ParentElement, Pixels, Point, Styled, StyledText, TextRun, UnderlineStyle,
};
use gpui_component::button::{Button, ButtonVariants as _};

use crate::config::TerminalConfig;
use crate::embedded::LiveView;

use super::shell::{term_theme, ShellView, TermTheme};
use super::terminal::{
    point_to_cell, screen_fingerprint, screen_rows, selection_highlight_rows, selection_rows,
    selection_text, snap_drag_to_content, snap_press_to_content, to_hsla, CellPos,
};
use gpui_component::Sizable as _;
use gpui_component::Theme;

/// Cursor cell the frame renders, if any: exited runs and hidden cursors
/// paint no caret. Shared by the render path and the cache fingerprint
/// so the key can never disagree with the pixels.
pub(crate) fn resolved_cursor(view: &LiveView) -> Option<CellPos> {
    if view.exited || view.screen.hide_cursor() {
        None
    } else {
        Some(view.screen.cursor_position())
    }
}

/// One cached terminal frame: the flattened text + gpui runs for a
/// (run id, screen fingerprint, light-mode) key. Single-entry: a miss
/// overwrites, so memory stays flat and run switches invalidate by
/// construction. The theme mode joins the key because a theme flip
/// repaints identical screens in different colors (issue #43).
#[derive(Default)]
pub(crate) struct TermFrameCache {
    key: Option<(String, u64, bool)>,
    full: String,
    runs: Vec<TextRun>,
}

impl TermFrameCache {
    /// Hit: hand back a clone of the cached frame. `StyledText` takes
    /// ownership per repaint, so one `String` + one `Vec` clone is the
    /// per-frame price — not a full grid rebuild.
    pub(crate) fn get(
        &self,
        run_id: &str,
        fingerprint: u64,
        light: bool,
    ) -> Option<(String, Vec<TextRun>)> {
        if self
            .key
            .as_ref()
            .is_some_and(|(id, f, l)| id == run_id && *f == fingerprint && *l == light)
        {
            Some((self.full.clone(), self.runs.clone()))
        } else {
            None
        }
    }

    /// Miss: remember this frame, dropping whatever was cached.
    pub(crate) fn store(
        &mut self,
        run_id: String,
        fingerprint: u64,
        light: bool,
        full: String,
        runs: Vec<TextRun>,
    ) {
        self.key = Some((run_id, fingerprint, light));
        self.full = full;
        self.runs = runs;
    }
}

impl ShellView {
    /// Normalized, non-empty mouse selection in terminal cells, if any.
    pub(crate) fn selection_pair(&self) -> Option<(CellPos, CellPos)> {
        let (a, b) = (self.sel_anchor?, self.sel_active?);
        let (start, end) = super::terminal::normalize_selection(a, b);
        if start == end {
            None
        } else {
            Some((start, end))
        }
    }

    /// Text covered by the mouse selection, via the emulator's own
    /// `contents_between` (accurate for wrapped/wide cells).
    pub(crate) fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection_pair()?;
        let view = self.active_view()?;
        let text = selection_text(view.screen, start, end);
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    pub(crate) fn clear_selection(&mut self) {
        self.sel_anchor = None;
        self.sel_active = None;
        self.selecting = false;
    }

    /// Map a window-space mouse point to a terminal cell using the captured
    /// text bounds plus the measured cell size and live grid size.
    pub(crate) fn mouse_cell(&self, pos: Point<Pixels>) -> Option<CellPos> {
        let bounds = (*self.term_text_bounds.borrow())?;
        let view = self.active_view()?;
        let (rows, cols) = view.screen.size();
        Some(point_to_cell(
            (f32::from(pos.x), f32::from(pos.y)),
            (f32::from(bounds.origin.x), f32::from(bounds.origin.y)),
            self.char_w,
            self.line_h,
            cols,
            rows,
        ))
    }

    /// Start a drag: arms only when the press lands on a shown character
    /// (issue #42). A mousedown on empty padding or an empty row clears
    /// any selection instead of arming a void one.
    pub(crate) fn begin_selection(&mut self, pos: Point<Pixels>) {
        let snapped = self.mouse_cell(pos).and_then(|cell| self.snap_press(cell));
        if let Some(cell) = snapped {
            self.sel_anchor = Some(cell);
            self.sel_active = Some(cell);
            self.selecting = true;
        } else {
            self.clear_selection();
        }
    }

    pub(crate) fn update_selection(&mut self, pos: Point<Pixels>) {
        if !self.selecting {
            return;
        }
        // Void drag positions keep the previous endpoint, so a drag that
        // leaves the text block stops at the last shown character.
        if let Some(cell) = self.mouse_cell(pos).and_then(|c| self.snap_drag(c)) {
            self.sel_active = Some(cell);
        }
    }

    /// Snap a mousedown cell to shown characters, if a live grid exists.
    fn snap_press(&self, cell: CellPos) -> Option<CellPos> {
        let view = self.active_view()?;
        snap_press_to_content(view.screen, cell)
    }

    /// Snap a drag cell to shown characters, if a live grid exists.
    fn snap_drag(&self, cell: CellPos) -> Option<CellPos> {
        let view = self.active_view()?;
        snap_drag_to_content(view.screen, cell)
    }

    /// Finish a drag: copy-on-select when the drag covered text, mirroring
    /// terminal copy-on-select behavior.
    pub(crate) fn end_selection(&mut self, pos: Point<Pixels>, cx: &mut GpuiApp) {
        if !self.selecting {
            return;
        }
        self.selecting = false;
        if let Some(cell) = self.mouse_cell(pos).and_then(|c| self.snap_drag(c)) {
            self.sel_active = Some(cell);
        }
        // A paged run copies from the visible pager slice (issue #25).
        if let Some(text) = self.selected_pager_text().or_else(|| self.selected_text()) {
            let chars = text.chars().count();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.app
                .set_status(format!("copied selection ({chars} chars)"));
        } else {
            self.clear_selection();
        }
    }

    pub(crate) fn render_terminal(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        // Missing-primary probe (issue #41): one status hint per
        // configured family, never a per-frame font enumeration.
        self.probe_terminal_font(cx);
        // Resolved theme mode (issue #43): the live global theme already
        // folds dark / light / system-follows-OS, so the pane tracks the
        // `t`-key cycle and OS changes with no extra plumbing.
        let light = term_is_light(cx);
        let theme = term_theme(light);
        // Pager (issue #25): a scrolled-up run shows its retained slice
        // instead of the live grid. The offset moves with no screen
        // change, so this bypasses the fingerprint cache below. The
        // pager keeps grid highlight spans (plain retained rows, no
        // emulator grid to snap against).
        if let Some(spans) = self.pager_spans() {
            let cols = self
                .active_view()
                .map(|v| v.screen.size().1)
                .unwrap_or(self.cols);
            let term_font = self.term_font();
            let (full, runs) = layout_text(&spans, &term_font, theme.fg);
            let highlight = match self.selection_pair() {
                Some((s, e)) => selection_rows(s, e, cols),
                None => Vec::new(),
            };
            return self.assemble_live_terminal(full, runs, highlight, &theme);
        }
        // Cache the flattened frame per (run, screen fingerprint, theme):
        // an unchanged screen skips the `screen_rows` + `layout_text`
        // rebuild (thousands of allocations) on every repaint and reuses
        // the cloned text + runs instead.
        let (run_id, fingerprint) = match (self.active_id(), self.active_view()) {
            (Some(id), Some(view)) => {
                let fingerprint = screen_fingerprint(view.screen, resolved_cursor(&view));
                (id, fingerprint)
            }
            _ => return self.render_empty_pane(cx),
        };
        if let Some((full, runs)) = self.term_frame.get(&run_id, fingerprint, light) {
            let highlight = self.live_highlight();
            return self.assemble_live_terminal(full, runs, highlight, &theme);
        }
        let Some(view) = self.active_view() else {
            return self.render_empty_pane(cx);
        };
        let rows = screen_rows(view.screen, resolved_cursor(&view), theme.cursor, light);
        let term_font = self.term_font();
        let (full, runs) = layout_text(&rows, &term_font, theme.fg);
        self.term_frame
            .store(run_id, fingerprint, light, full.clone(), runs.clone());
        let highlight = self.live_highlight();
        self.assemble_live_terminal(full, runs, highlight, &theme)
    }

    /// Content-aware highlight spans for the live grid (issue #42):
    /// shown characters only, via the emulator screen.
    fn live_highlight(&self) -> Vec<(u16, u16, u16)> {
        let Some((s, e)) = self.selection_pair() else {
            return Vec::new();
        };
        let Some(view) = self.active_view() else {
            return Vec::new();
        };
        selection_highlight_rows(view.screen, s, e)
    }

    /// Assemble the terminal element from flattened text + runs: the
    /// `StyledText` plus the mouse-selection highlight behind it. Takes
    /// no window context so headless tests can build the element without
    /// a gpui harness; production always reaches it through the cached
    /// [`Self::render_terminal`] path.
    fn assemble_live_terminal(
        &self,
        full: String,
        runs: Vec<TextRun>,
        highlight: Vec<(u16, u16, u16)>,
        theme: &TermTheme,
    ) -> gpui::Div {
        let text = StyledText::new(full).with_runs(runs);
        // Mouse selection highlight: cell rectangles behind the text. The
        // inner wrapper has no padding, so overlay origin == text origin and
        // no padding constant is needed.
        let slot = std::rc::Rc::clone(&self.term_text_bounds);
        let mut inner = div().relative().h_full().w_full();
        for (row, sc, ec) in highlight {
            if ec <= sc {
                continue;
            }
            inner = inner.child(
                div()
                    .absolute()
                    .left(px(sc as f32 * self.char_w))
                    .top(px(row as f32 * self.line_h))
                    .w(px((ec - sc) as f32 * self.char_w))
                    .h(px(self.line_h))
                    .bg(rgb(theme.selection)),
            );
        }
        inner = inner.child(
            div()
                .child(text)
                .on_children_prepainted(move |bounds, _, _| {
                    if let Some(b) = bounds.first() {
                        *slot.borrow_mut() = Some(*b);
                    }
                }),
        );
        // Full monospace stack (issue #41): the primary family plus the
        // emoji/CJK/system fallback tail, so TextRuns and the element
        // agree on one font and box-drawing/emoji/CJK never fall back to
        // a proportional face mid-row.
        let term_font = self.term_font();
        div()
            .flex_1()
            .h_full()
            .bg(rgb(theme.bg))
            .p_2()
            .font(term_font)
            .text_size(px(self.term_font_size()))
            // Rendered row pitch == mapped cell pitch (issue #59):
            // without this the text inherits gpui's default phi (1.618x)
            // line height, so every glyph row below row 0 lands lower
            // than `mouse_cell`/highlight assume and drags snap to void.
            .line_height(px(self.line_h))
            .child(inner)
    }

    /// Sticky-error banner above the terminal pane: spawn and PTY-write
    /// failures stay visible here (mirroring the status bar) until
    /// dismissed or superseded by a success. Retry appears only when the
    /// selected run owns no live PTY; Dismiss (`d`) always does.
    /// Empty (zero-size) when no error is sticky.
    pub(crate) fn render_error_banner(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(err) = self.app.error_text().map(str::to_string) else {
            return div();
        };
        let retry = self.can_retry();
        let mut row = div()
            .flex()
            .flex_row()
            .px_2()
            .py_1()
            .bg(rgb(0x3a1d1d))
            .text_color(rgb(0xff9999))
            .text_sm()
            .child(err);
        if retry {
            row = row.child(
                Button::new(ElementId::Name("retry-spawn-btn".into()))
                    .label("Retry (r)")
                    .primary()
                    .small()
                    .on_click(cx.listener(|this, _ev, window, _cx| {
                        this.retry_spawn();
                        this.focus_term(window);
                    })),
            );
        }
        row.child(
            Button::new(ElementId::Name("dismiss-error-btn".into()))
                .label("Dismiss (d)")
                .small()
                .on_click(cx.listener(|this, _ev, _window, _cx| {
                    this.app.clear_error();
                })),
        )
    }
}

/// Known-monospace families (issue #53), in head preference order: the
/// first installed entry heads the render stack when the configured
/// primary is missing or proportional. Latin-monospace only — the
/// emoji/CJK tail stays a tail, never a head, so Latin columns cannot
/// drift.
const MONO_HEAD_CANDIDATES: &[&str] = &[
    "Menlo",
    "SF Mono",
    "DejaVu Sans Mono",
    "Noto Sans Mono",
    "JetBrains Mono",
    "Fira Code",
    "Hack",
    "Iosevka",
    "Roboto Mono",
    "Cascadia Mono",
    "Cascadia Code",
    "Consolas",
];

/// Known-proportional families (issue #53): never honored as the
/// terminal head even when installed and explicitly configured — a UI
/// face breaks TUI column alignment exactly like a missing font does.
/// Matched case-insensitively, exact names only (`Roboto Mono` stays
/// valid while `Roboto` does not).
const PROPORTIONAL_FAMILIES: &[&str] = &[
    "Helvetica",
    "Helvetica Neue",
    "Arial",
    "Times",
    "Times New Roman",
    "Georgia",
    "Verdana",
    "SF Pro",
    "SF Pro Text",
    "SF Pro Display",
    "Segoe UI",
    "Roboto",
    "Inter",
    "New York",
    "PingFang SC",
    ".AppleSystemUIFont",
    ".SystemUIFont",
];

/// True when `name` is a known-proportional UI face (issue #53).
pub(crate) fn is_proportional_family(name: &str) -> bool {
    PROPORTIONAL_FAMILIES
        .iter()
        .any(|p| p.eq_ignore_ascii_case(name))
}

/// Effective terminal head (issue #53): the configured primary when it
/// is installed and monospace, else the first installed known-monospace
/// family, else the primary as a warned last resort. Returns the head
/// plus whether a fallback engaged (i.e. whether to flash the hint).
/// The stack can therefore never head a proportional face: a missing
/// primary used to fall through to gpui's proportional system face
/// because the `FontFallbacks` tail only cascades missing *glyphs* once
/// a base font loads — it never rescues a missing base.
pub(crate) fn effective_terminal_family(installed: &[String], primary: &str) -> (String, bool) {
    let present = |name: &str| installed.iter().any(|n| n.eq_ignore_ascii_case(name));
    if present(primary) && !is_proportional_family(primary) {
        return (primary.to_string(), false);
    }
    if let Some(mono) = MONO_HEAD_CANDIDATES.iter().find(|m| present(m)) {
        return (mono.to_string(), true);
    }
    (primary.to_string(), true)
}

/// Build the gpui [`Font`] for the terminal pane from the user setting
/// (issue #35): the configured primary family plus the explicit
/// emoji/CJK/monospace fallback chain, so one missing family never
/// silently changes metrics mid-row.
pub(crate) fn terminal_font(cfg: &TerminalConfig) -> Font {
    let stack = cfg.font_stack();
    let mut fallbacks = stack.clone();
    fallbacks.remove(0);
    Font {
        family: stack[0].clone().into(),
        features: Default::default(),
        weight: Default::default(),
        style: Default::default(),
        fallbacks: Some(FontFallbacks::from_fonts(fallbacks)),
    }
}

/// Terminal [`Font`] headed by an explicit family (issue #53): `head`
/// first, then the configured stack with any duplicate of the head
/// removed, so the resolved-monospace substitute keeps the same
/// emoji/CJK tail as the configured primary.
pub(crate) fn headed_terminal_font(head: &str, cfg: &TerminalConfig) -> Font {
    // One-way shortcut: the configured head is exactly the #35
    // constructor (`terminal_font` never calls back here).
    if head == cfg.font_family {
        return terminal_font(cfg);
    }
    let mut stack = vec![head.to_string()];
    for family in cfg.font_stack() {
        if family != head && !stack.contains(&family) {
            stack.push(family);
        }
    }
    let mut fallbacks = stack.clone();
    fallbacks.remove(0);
    Font {
        family: stack[0].clone().into(),
        features: Default::default(),
        weight: Default::default(),
        style: Default::default(),
        fallbacks: Some(FontFallbacks::from_fonts(fallbacks)),
    }
}

/// Terminal [`Font`] that truly renders monospace (issue #53): headed
/// by [`effective_terminal_family`] against the installed families, so
/// every TextRun, the text element, and the PTY metrics agree on one
/// installed monospace face.
pub(crate) fn resolved_terminal_font(installed: &[String], cfg: &TerminalConfig) -> Font {
    let (head, _) = effective_terminal_family(installed, &cfg.font_family);
    headed_terminal_font(&head, cfg)
}

/// Whether the terminal pane renders the light palette (issue #43):
/// true when a global component theme exists and is not dark. Headless
/// (no theme global) stays dark, preserving the historic default.
pub(crate) fn term_is_light(cx: &gpui::App) -> bool {
    cx.try_global::<Theme>().is_some_and(|t| !t.is_dark())
}

/// One-line hint naming the resolved terminal font (issues #41/#53):
/// `None` when `primary` is installed and monospace; otherwise names
/// the primary plus the substitute head, so the resolved font is
/// always visible on the status line instead of failing silently.
pub(crate) fn missing_font_hint(installed: &[String], primary: &str) -> Option<String> {
    let installed_primary = installed
        .iter()
        .any(|name| name.eq_ignore_ascii_case(primary));
    let (head, _) = effective_terminal_family(installed, primary);
    if installed_primary && !is_proportional_family(primary) {
        None
    } else if installed_primary {
        Some(format!(
            "terminal font '{primary}' is proportional — using {head} for column alignment"
        ))
    } else if head == primary {
        Some(format!(
            "terminal font '{primary}' not installed and no monospace fallback found"
        ))
    } else {
        Some(format!(
            "terminal font '{primary}' not installed — using {head}"
        ))
    }
}

impl ShellView {
    /// Probe the installed families for the configured primary (issues
    /// #41/#53): resolves the monospace head once per configured
    /// family and flashes the one-line hint when a fallback engaged, so
    /// the per-frame render never re-enumerates system fonts.
    pub(crate) fn probe_terminal_font(&mut self, cx: &gpui::App) {
        let primary = self.app.terminal_config().font_family.clone();
        if self
            .font_probe
            .as_ref()
            .is_some_and(|(checked, _)| *checked == primary)
        {
            return;
        }
        let installed = cx.text_system().all_font_names();
        let hint = missing_font_hint(&installed, &primary);
        // The cached head is the family of the resolved stack itself,
        // never a parallel computation that could disagree with it.
        let head = resolved_terminal_font(&installed, self.app.terminal_config())
            .family
            .to_string();
        self.font_probe = Some((primary, head));
        if let Some(hint) = hint {
            self.app.set_status(hint);
        }
    }

    /// Family actually heading the terminal render stack (issue #53):
    /// the probed head when it covers the configured primary, else the
    /// configured primary (first frame / headless). This is the
    /// debugging surface for the monospace investigation: whatever this
    /// returns is what every TextRun and the metrics measure.
    pub(crate) fn resolved_font_head(&self) -> String {
        let primary = self.app.terminal_config().font_family.clone();
        match &self.font_probe {
            Some((checked, head)) if *checked == primary => head.clone(),
            _ => primary,
        }
    }

    /// Font actually rendering in the terminal pane (issue #53): the
    /// resolved head plus the configured tail, so TextRuns, the text
    /// element, and the PTY metrics agree on one installed monospace
    /// face instead of silently falling back to a proportional face.
    pub(crate) fn term_font(&self) -> Font {
        headed_terminal_font(&self.resolved_font_head(), self.app.terminal_config())
    }
}

/// Flatten screen rows into one string plus gpui text runs. Every byte of
/// the string belongs to exactly one non-empty run — gpui validates this
/// partition and panics otherwise (crashed the first launch). Every run
/// carries the monospace stack `mono` (issue #41), so no span can fall
/// back to a proportional face mid-row; `default_fg` resolves
/// default-color spans against the active theme (issue #43).
pub(crate) fn layout_text(
    rows: &[Vec<super::terminal::TermSpan>],
    mono: &Font,
    default_fg: super::terminal::Rgb8,
) -> (String, Vec<TextRun>) {
    let plain = || TextRun {
        len: 0,
        font: mono.clone(),
        color: to_hsla(default_fg),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let mut full = String::new();
    let mut runs = Vec::new();
    let push_text = |full: &mut String, runs: &mut Vec<TextRun>, text: &str, run: TextRun| {
        let start = full.len();
        full.push_str(text);
        let mut run = run;
        run.len = full.len() - start;
        if run.len > 0 {
            runs.push(run);
        }
    };
    for (ri, row) in rows.iter().enumerate() {
        if ri > 0 {
            push_text(&mut full, &mut runs, "\n", plain());
        }
        if row.is_empty() {
            push_text(&mut full, &mut runs, " ", plain());
        }
        for span in row {
            let fg = span
                .style
                .fg
                .map(to_hsla)
                .unwrap_or_else(|| to_hsla(default_fg));
            let bg = span.style.bg.map(to_hsla);
            push_text(
                &mut full,
                &mut runs,
                &span.text,
                TextRun {
                    len: 0,
                    font: if span.style.bold {
                        mono.clone().bold()
                    } else {
                        mono.clone()
                    },
                    color: fg,
                    background_color: bg,
                    underline: span.style.underline.then_some(UnderlineStyle::default()),
                    strikethrough: None,
                },
            );
        }
    }
    (full, runs)
}

#[cfg(test)]
mod tests {
    use super::super::runs::{insert_test_pty, test_shell};
    use super::super::shell::{term_theme, ShellView};
    use super::super::terminal::{screen_rows, Rgb8};
    use super::*;

    #[test]
    fn layout_text_partition_satisfies_with_runs() {
        // Regression test for the "new session" crash: gpui validates that
        // run lengths partition the text byte-exactly and panics otherwise.
        // This calls the real constructor, so it panics here first.
        use crate::config::TerminalConfig;
        let mono = terminal_font(&TerminalConfig::default());
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\x1b[2J\x1b[1;1Htop \x1b[31mred\x1b[0m \xc3\xa9\xe2\x9d\xaf");
        let rows = screen_rows(parser.screen(), Some((0, 0)), Rgb8(200, 200, 200), false);
        let (full, runs) = layout_text(&rows, &mono, term_theme(false).fg);
        let total: usize = runs.iter().map(|r| r.len).sum();
        assert_eq!(total, full.len(), "runs must cover every byte");
        assert!(runs.iter().all(|r| r.len > 0), "no empty runs");
        let _ = gpui::StyledText::new(full).with_runs(runs);
        // Empty screen still partitions (single covered space per row).
        let mut empty = vt100::Parser::new(24, 80, 0);
        empty.process(b"");
        let rows = screen_rows(empty.screen(), None, Rgb8(0, 0, 0), false);
        let (full, runs) = layout_text(&rows, &mono, term_theme(false).fg);
        let total: usize = runs.iter().map(|r| r.len).sum();
        assert_eq!(total, full.len());
        let _ = gpui::StyledText::new(full).with_runs(runs);
    }

    #[test]
    fn terminal_font_heads_primary_with_fallback_chain() {
        // Issue #35: the gpui font carries the configured primary plus
        // the explicit fallback list; an override swaps only the head.
        use crate::config::TerminalConfig;
        let head = terminal_font(&TerminalConfig::default());
        assert_eq!(head.family.as_ref(), "JetBrainsMono Nerd Font");
        let fallbacks = head
            .fallbacks
            .expect("fallback chain is explicit")
            .fallback_list()
            .to_vec();
        assert!(fallbacks.contains(&"Apple Color Emoji".to_string()));
        assert!(fallbacks.contains(&"Noto Sans Mono CJK SC".to_string()));
        assert!(!fallbacks.contains(&"JetBrainsMono Nerd Font".to_string()));
        let custom = TerminalConfig {
            font_family: "Iosevka Nerd Font".to_string(),
            ..TerminalConfig::default()
        };
        let swapped = terminal_font(&custom);
        assert_eq!(swapped.family.as_ref(), "Iosevka Nerd Font");
        let fallbacks = swapped
            .fallbacks
            .as_ref()
            .expect("fallbacks survive override")
            .fallback_list()
            .to_vec();
        assert!(fallbacks.contains(&"Apple Color Emoji".to_string()));
        // Bold keeps the same chain (styled spans must not change metrics).
        assert_eq!(swapped.clone().bold().family, swapped.family);
        assert_eq!(swapped.clone().bold().fallbacks, swapped.fallbacks);
    }

    #[test]
    fn coverage_fixture_renders_without_tofu_or_column_drift() {
        // Issue #35 acceptance fixture: every glyph class the CLIs can
        // emit passes through screen_rows → layout_text intact, partitions
        // byte-exactly, and wide chars still occupy two cells.
        use crate::config::TerminalConfig;
        let mono = terminal_font(&TerminalConfig::default());
        let mut parser = vt100::Parser::new(24, 100, 0);
        // Box-drawing, blocks/shades, powerline + Nerd Font icons,
        // bold/italic styles, CJK, and emoji.
        let fixture = "─│┌┐└┘├┤┬┴┼ █▉▊▋▌▍▎▏▓▒░▀▄ \u{e0b0}\u{e0b1}\u{e0b2}  \u{f0244} \x1b[1mbold\x1b[0m \x1b[3mital\x1b[0m 你好世界 \u{1f600}";
        parser.process(fixture.as_bytes());
        let screen = parser.screen();
        let contents = screen.contents();
        for needle in ["─│┌┐", "▓▒░▀", "bold", "ital", "你好世界"] {
            assert!(
                contents.contains(needle),
                "fixture keeps {needle:?}: {contents:?}"
            );
        }
        // Wide chars (CJK/emoji) occupy two cells: a continuation cell
        // follows each lead, so the vt100 column grid cannot drift.
        let mut saw_continuation = false;
        let (rows, cols) = screen.size();
        for r in 0..rows {
            for c in 0..cols {
                if let Some(cell) = screen.cell(r, c) {
                    if cell.is_wide_continuation() {
                        saw_continuation = true;
                    }
                }
            }
        }
        assert!(saw_continuation, "wide chars take two cells");
        // And the whole grid still partitions for gpui.
        let grid = screen_rows(screen, None, Rgb8(200, 200, 200), false);
        let (full, runs) = layout_text(&grid, &mono, term_theme(false).fg);
        let total: usize = runs.iter().map(|r| r.len).sum();
        assert_eq!(total, full.len());
        let _ = gpui::StyledText::new(full).with_runs(runs);
    }

    #[test]
    fn frame_cache_hits_reuse_output_and_misses_on_change_or_run() {
        use super::super::terminal::screen_fingerprint;
        let mut cache = TermFrameCache::default();
        assert!(cache.get("run-1", 42, false).is_none());
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"hello");
        let fp = screen_fingerprint(parser.screen(), Some((0, 5)));
        let rows = screen_rows(parser.screen(), Some((0, 5)), Rgb8(0, 0, 0), false);
        let mono = terminal_font(&TerminalConfig::default());
        let (full, runs) = layout_text(&rows, &mono, term_theme(false).fg);
        cache.store("run-1".to_string(), fp, false, full.clone(), runs.clone());
        // Same run + fingerprint + theme: the cached text and runs come
        // back byte-identical (the repaint reuses them instead of
        // rebuilding).
        let (hit_full, hit_runs) = cache.get("run-1", fp, false).expect("cache hit");
        assert_eq!(hit_full, full);
        assert_eq!(hit_runs, runs);
        // A theme flip misses under the same screen: identical text
        // repaints in the other palette (issue #43).
        assert!(cache.get("run-1", fp, true).is_none());
        // A different run never aliases, even with an identical screen.
        assert!(cache.get("run-2", fp, false).is_none());
        // New output misses under the old key: the caller rebuilds and
        // the store drops the stale frame.
        parser.process(b"!");
        let fp2 = screen_fingerprint(parser.screen(), Some((0, 6)));
        assert_ne!(fp2, fp);
        assert!(cache.get("run-1", fp2, false).is_none());
    }

    #[test]
    fn render_terminal_with_live_pty_does_not_panic() {
        // End-to-end of the "new session" crash path: a real child writes
        // colored output, and render_terminal builds the gpui element.
        // Pre-fix this panicked inside StyledText::with_runs.
        let mut view = ShellView::new();
        view.app.start_new_session();
        let _ = view.app.take_pending_spawn();
        let id = view.active_id().unwrap();
        let pty = crate::embedded::EmbeddedPty::spawn(
            "printf",
            &["\\x1b[2J\\x1b[1;1Hhi \\x1b[31mred\\n\"".to_string()],
            80,
            24,
        )
        .unwrap();
        view.runs
            .insert(id.clone(), super::super::runs::Run::new(pty));
        for _ in 0..50 {
            view.refresh();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let live = view.active_view().expect("live pty has a view");
        // Same pieces the cached path assembles: rows, layout, element.
        // Both themes assemble without panic (issue #43).
        for light in [false, true] {
            let theme = term_theme(light);
            let rows = screen_rows(live.screen, resolved_cursor(&live), theme.cursor, light);
            let mono = terminal_font(&TerminalConfig::default());
            let (full, runs) = layout_text(&rows, &mono, theme.fg);
            let highlight =
                selection_highlight_rows(live.screen, (0, 0), (0, live.screen.size().1));
            let _ = view.assemble_live_terminal(full, runs, highlight, &theme);
        }
    }

    #[test]
    fn selection_pair_normalizes_and_rejects_empty() {
        let mut view = test_shell();
        assert!(view.selection_pair().is_none());
        view.sel_anchor = Some((1, 7));
        view.sel_active = Some((1, 3));
        assert_eq!(view.selection_pair(), Some(((1, 3), (1, 7))));
        view.sel_active = Some((1, 7));
        assert!(view.selection_pair().is_none());
        view.clear_selection();
        assert!(view.selection_pair().is_none());
    }

    #[test]
    fn selected_text_uses_emulator_between_cells() {
        let mut view = test_shell();
        let id = insert_test_pty(&mut view, "printf", &["hello world\\n"]);
        for _ in 0..100 {
            view.refresh();
            if let Some(v) = view.active_view() {
                if v.screen.contents().contains("hello") {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = id;
        view.sel_anchor = Some((0, 0));
        view.sel_active = Some((0, 5));
        assert_eq!(view.selected_text().as_deref(), Some("hello"));
        view.clear_selection();
        assert!(view.selected_text().is_none());
    }

    #[test]
    fn layout_runs_all_carry_the_monospace_stack() {
        // Issue #41: every TextRun (default spans, styled spans, plain
        // newlines and padding) renders in the full monospace stack, so
        // no run can fall back to a proportional face mid-row.
        use crate::config::TerminalConfig;
        let mono = terminal_font(&TerminalConfig::default());
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"plain \x1b[1mbold\x1b[0m \x1b[31mred\x1b[0m");
        let rows = screen_rows(parser.screen(), None, Rgb8(0, 0, 0), false);
        let (full, runs) = layout_text(&rows, &mono, term_theme(false).fg);
        assert!(!runs.is_empty());
        let total: usize = runs.iter().map(|r| r.len).sum();
        assert_eq!(total, full.len());
        for run in &runs {
            assert_eq!(run.font.family, mono.family, "run keeps the primary");
            assert_eq!(
                run.font.fallbacks, mono.fallbacks,
                "run keeps the fallback tail"
            );
        }
        // The styled (bold) run keeps the same chain as the plain runs.
        assert!(runs.len() >= 2);
        let _ = gpui::StyledText::new(full).with_runs(runs);
    }

    #[test]
    fn missing_font_hint_names_the_absent_primary() {
        // Issue #41: a missing primary yields a one-line hint naming it;
        // an installed primary (any case) yields none, and a user
        // override is checked by its own name with the same tail.
        let installed = vec![
            "Menlo".to_string(),
            "Apple Color Emoji".to_string(),
            "JetBrainsMono Nerd Font".to_string(),
        ];
        assert_eq!(
            missing_font_hint(&installed, "JetBrainsMono Nerd Font"),
            None
        );
        assert_eq!(
            missing_font_hint(&installed, "jetbrainsmono nerd font"),
            None
        );
        let hint =
            missing_font_hint(&installed, "Iosevka Nerd Font").expect("absent primary hints");
        assert!(hint.contains("Iosevka Nerd Font"), "names it: {hint}");
        assert!(missing_font_hint(&[], "Menlo").is_some());
    }

    #[test]
    fn effective_head_never_leaves_the_monospace_stack() {
        // Issue #53 core: a missing primary resolves to the first
        // installed known-monospace family — never the raw missing name
        // (which gpui renders with a proportional stand-in) and never an
        // unrelated installed proportional face.
        use crate::config::TerminalConfig;
        let installed = vec![
            "Helvetica".to_string(),
            "Apple Color Emoji".to_string(),
            "Menlo".to_string(),
        ];
        let (head, fell_back) =
            effective_terminal_family(&installed, &TerminalConfig::default().font_family);
        assert_eq!(head, "Menlo");
        assert!(fell_back);
        // The hint surfaces the substitute for debugging.
        let hint = missing_font_hint(&installed, &TerminalConfig::default().font_family)
            .expect("fallback hints");
        assert!(hint.contains("Menlo"), "names the substitute: {hint}");
    }

    #[test]
    fn proportional_primary_is_rejected_even_when_installed() {
        // Issue #53: explicitly configuring a proportional UI face still
        // renders monospace — the head refuses it exactly like a missing
        // primary. This is the regression test that fails under a
        // proportional font: the resolved head must differ from it.
        let installed = vec!["Helvetica".to_string(), "Menlo".to_string()];
        let (head, fell_back) = effective_terminal_family(&installed, "Helvetica");
        assert_ne!(head, "Helvetica");
        assert_eq!(head, "Menlo");
        assert!(fell_back);
        let hint = missing_font_hint(&installed, "Helvetica").expect("proportional hints");
        assert!(hint.contains("proportional"), "says why: {hint}");
        assert!(hint.contains("Menlo"), "names the substitute: {hint}");
        // Near-miss names stay valid: Roboto Mono is monospace even
        // though Roboto is proportional.
        assert!(!is_proportional_family("Roboto Mono"));
        assert!(is_proportional_family("Roboto"));
    }

    #[test]
    fn installed_monospace_primary_is_honored() {
        // No fallback, no hint when the configured family is present.
        let installed = vec!["Iosevka Nerd Font".to_string(), "Menlo".to_string()];
        let (head, fell_back) = effective_terminal_family(&installed, "Iosevka Nerd Font");
        assert_eq!(head, "Iosevka Nerd Font");
        assert!(!fell_back);
        assert_eq!(missing_font_hint(&installed, "Iosevka Nerd Font"), None);
    }

    #[test]
    fn resolved_runs_carry_the_substitute_head_with_the_same_tail() {
        // Issue #53 end to end: with the default primary absent, the
        // runs gpui shapes head the installed monospace substitute and
        // keep the configured emoji/CJK tail — so box-drawing, bold,
        // and newline runs all share one monospace face.
        use crate::config::TerminalConfig;
        let cfg = TerminalConfig::default();
        let installed = vec!["Menlo".to_string(), "Apple Color Emoji".to_string()];
        let resolved = resolved_terminal_font(&installed, &cfg);
        assert_eq!(resolved.family.as_ref(), "Menlo");
        let tail = resolved
            .fallbacks
            .as_ref()
            .expect("tail survives the substitute head")
            .fallback_list()
            .to_vec();
        assert!(tail.contains(&"Apple Color Emoji".to_string()));
        assert!(!tail.contains(&"Menlo".to_string()), "no head duplicate");
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process("─┌─┐ \x1b[1mbold\x1b[0m \x1b[31mred\x1b[0m".as_bytes());
        let rows = screen_rows(parser.screen(), None, Rgb8(0, 0, 0), false);
        let (full, runs) = layout_text(&rows, &resolved, term_theme(false).fg);
        assert!(!runs.is_empty());
        let total: usize = runs.iter().map(|r| r.len).sum();
        assert_eq!(total, full.len());
        for run in &runs {
            assert_eq!(run.font.family.as_ref(), "Menlo", "run heads mono");
            assert_eq!(run.font.fallbacks, resolved.fallbacks, "run keeps the tail");
        }
        let _ = gpui::StyledText::new(full).with_runs(runs);
    }

    #[test]
    fn box_drawing_borders_occupy_single_columns() {
        // Issue #53 falsifiable half at the grid level: every TUI
        // border glyph takes exactly one cell (no wide continuation),
        // so rows align in columns by construction — and the resolved
        // head above is what keeps them aligned on pixels too.
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process("┌──┐\r\n│hi│\r\n└──┘".as_bytes());
        let screen = parser.screen();
        let contents = screen.contents();
        assert!(contents.contains("┌──┐"), "top border: {contents:?}");
        assert!(contents.contains("└──┘"), "bottom border: {contents:?}");
        for r in 0..3u16 {
            for c in 0..4u16 {
                let cell = screen.cell(r, c).expect("border cell exists");
                assert!(
                    !cell.is_wide_continuation(),
                    "border cell ({r}, {c}) is single-width"
                );
            }
        }
        assert_eq!(
            super::super::terminal::row_content_range(screen, 0),
            Some((0, 4))
        );
    }

    #[test]
    fn term_font_head_defaults_to_primary_before_the_probe() {
        // Headless (no text system): the render font heads the
        // configured primary. A stored probe head swaps only the head —
        // the tail is untouched — which is also the debugging surface:
        // `resolved_font_head` is what reaches the TextRuns.
        use crate::config::TerminalConfig;
        let view = ShellView::new();
        assert_eq!(
            view.resolved_font_head(),
            TerminalConfig::default().font_family
        );
        assert_eq!(
            view.term_font().family.as_ref(),
            TerminalConfig::default().font_family
        );
        let mut probed = ShellView::new();
        probed.font_probe = Some((TerminalConfig::default().font_family, "Menlo".to_string()));
        assert_eq!(probed.resolved_font_head(), "Menlo");
        let font = probed.term_font();
        assert_eq!(font.family.as_ref(), "Menlo");
        let tail = font.fallbacks.expect("tail").fallback_list().to_vec();
        assert!(tail.contains(&"Apple Color Emoji".to_string()));
        // A stale probe (different primary) never applies.
        let mut term = probed.app.terminal_config().clone();
        term.font_family = "Iosevka".to_string();
        probed.app.set_config(crate::config::Config {
            terminal: term,
            ..Default::default()
        });
        assert_eq!(probed.resolved_font_head(), "Iosevka");
    }

    #[test]
    fn begin_selection_arms_only_on_shown_text() {
        // Issue #42 end-to-end: a mousedown on shown characters arms a
        // selection; a mousedown past end-of-line or on an empty row
        // clears instead of arming.
        let mut view = test_shell();
        let _ = insert_test_pty(&mut view, "printf", &["hi"]);
        for _ in 0..100 {
            view.refresh();
            if let Some(v) = view.active_view() {
                if v.screen.contents().contains("hi") {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Text bounds captured each frame; the mapping only needs the
        // origin, so any plausible pane bounds will do.
        *view.term_text_bounds.borrow_mut() = Some(gpui::Bounds {
            origin: gpui::point(px(0.0), px(0.0)),
            size: gpui::size(px(800.0), px(600.0)),
        });
        // Default metrics: 8px cells, 18px rows.
        view.begin_selection(gpui::point(px(4.0), px(9.0)));
        assert_eq!(view.sel_anchor, Some((0, 0)));
        assert!(view.selecting);
        // Past end-of-line on the same row: no selection arms.
        view.begin_selection(gpui::point(px(400.0), px(9.0)));
        assert!(view.selection_pair().is_none());
        assert!(!view.selecting);
        // An empty row arms nothing either.
        view.begin_selection(gpui::point(px(4.0), px(99.0)));
        assert!(view.selection_pair().is_none());
        assert!(!view.selecting);
    }

    #[test]
    fn drag_selects_hello_on_a_lower_row_at_render_pitch() {
        // Issue #59: mouse selection was dead in normal use. The pane
        // rendered glyph rows at gpui's inherited default text line
        // height (`phi`, 1.618034 x font size) while `mouse_cell` and
        // the highlight mapped with the measured cell (`line_h` =
        // ascent + descent): row 0 survived (both pitches start at the
        // text origin) but lower rows drifted, so a press centered on
        // a rendered glyph row mapped to the wrong grid row,
        // snap-to-content found no content, and `begin_selection`
        // cleared. The pane now renders at `line_h`, so glyph rows
        // land exactly on the mapped grid. This drives the real
        // window-space path: production-structured bounds (sidebar +
        // border + pane padding origin, as captured by
        // `on_children_prepainted`) with press positions where the
        // pane renders the glyphs (the `line_h` render pitch, which
        // must track `assemble_live_terminal`'s `.line_height`),
        // through begin/update_selection: the highlight must cover
        // exactly `hello` and the copy payload must be `hello`.
        let mut view = test_shell();
        let _ = insert_test_pty(&mut view, "printf", &["a\\nb\\nc\\nd\\nhello"]);
        for _ in 0..100 {
            view.refresh();
            if let Some(v) = view.active_view() {
                if v.screen.contents().contains("hello") {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Production-captured bounds shape: the text origin sits past
        // the 264px sidebar, the 1px focus border, and the 8px pane
        // padding — window-space, like the mouse events.
        let origin_x = super::super::layout::LEFT_WIDTH + 1.0 + 8.0;
        let origin_y = 40.0;
        *view.term_text_bounds.borrow_mut() = Some(gpui::Bounds {
            origin: gpui::point(px(origin_x), px(origin_y)),
            size: gpui::size(px(800.0), px(600.0)),
        });
        // Where the pane renders glyph rows: the `line_h` pitch from
        // `assemble_live_terminal`'s `.line_height` (same pitch
        // `mouse_cell` maps with). Must track that override: if the
        // render pitch ever diverges again, presses modeled here stop
        // matching the glyphs, exactly the issue #59 failure.
        let render_pitch = view.line_h;
        let char_w = view.char_w;
        let press = |col_cells: f32, row: f32| {
            gpui::point(
                px(origin_x + col_cells * char_w),
                px(origin_y + (row + 0.5) * render_pitch),
            )
        };
        // Sanity: `hello` really is on grid row 4, span 0..5.
        {
            let live = view.active_view().expect("live pty");
            assert_eq!(
                super::super::terminal::row_content_range(live.screen, 4),
                Some((0, 5)),
                "fixture row"
            );
        }
        // Drag across `hello`: press on `h`, release just past `o`.
        view.begin_selection(press(0.25, 4.0));
        assert!(view.selecting, "press on rendered hello arms a selection");
        assert_eq!(view.sel_anchor, Some((4, 0)));
        view.update_selection(press(5.25, 4.0));
        assert_eq!(view.selection_pair(), Some(((4, 0), (4, 5))));
        // Highlight covers exactly hello; the copy payload is hello
        // (`end_selection` writes `selected_text()` verbatim to the
        // clipboard, which needs a gpui `App` and is covered by that
        // straight-line path plus this payload assertion).
        assert_eq!(view.live_highlight(), vec![(4, 0, 5)]);
        assert_eq!(view.selected_text().as_deref(), Some("hello"));
        // Structural pin: the built terminal element itself carries
        // the `line_h` pitch, so reverting the `.line_height`
        // override fails here even though mapping-pitch presses still
        // map. Read back through the public `Styled::style` surface.
        let (full, runs) = {
            let live = view.active_view().expect("live pty");
            let theme = term_theme(false);
            let grid = screen_rows(live.screen, None, theme.cursor, false);
            layout_text(&grid, &view.term_font(), theme.fg)
        };
        let theme = term_theme(false);
        let mut el = view.assemble_live_terminal(full, runs, vec![], &theme);
        let rendered = el
            .style()
            .text_style()
            .clone()
            .and_then(|t| t.line_height)
            .expect("terminal element sets an explicit line height");
        assert_eq!(
            rendered,
            gpui::DefiniteLength::from(px(view.line_h)),
            "render pitch == mapping pitch"
        );
    }
}
