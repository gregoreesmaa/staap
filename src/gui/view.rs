//! Window composition for [`super::shell::ShellView`].
//!
//! The root [`Render`] impl: runs panel beside the terminal column
//! (header, error banner, terminal), plus the status bar, window options,
//! and viewport-driven pane geometry (`fit_pty`). Event wiring lives in
//! [`super::shell`]; panel and terminal rendering live in
//! [`super::runs_panel`] and [`super::terminal_pane`]; pure geometry lives
//! in [`super::layout`].

use gpui::{
    div, px, rgb, ClickEvent, Context, ElementId, Font, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Render, SharedString,
    Size, StatefulInteractiveElement, Styled, TitlebarOptions, Window, WindowControlArea,
    WindowOptions,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::sidebar::{SidebarMenu, SidebarMenuItem};
use gpui_component::Sizable as _;

use super::layout::{
    effective_sidebar_width_for, sidebar_visible_for_width, MIN_WINDOW_HEIGHT, MIN_WINDOW_WIDTH,
    STATUS_HEIGHT,
};
use super::shell::ShellView;

impl Render for ShellView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.list_focus.is_none() {
            self.list_focus = Some(cx.focus_handle());
            self.term_focus = Some(cx.focus_handle());
        }
        if !self.focused_once {
            self.focused_once = true;
            if let Some(h) = &self.list_focus {
                window.focus(h);
            }
        }
        self.refresh();
        self.fit_pty(window, cx);
        // Narrow layout (issue #6): below ~700px the sidebar collapses and
        // the terminal pane takes the full width; keyboard nav (j/k/Tab)
        // keeps switching runs while collapsed.
        let viewport_w = f32::from(window.viewport_size().width);
        let show_sidebar = sidebar_visible_for_width(viewport_w);

        // Issue #32: no terminal header row and no dedicated status
        // bar in wide mode — the status text lives in the sidebar
        // footer, so the terminal owns every vertical pixel. Ended-run
        // recovery stays on keyboard `r` (hinted in the status text).
        let mut mid_row = div().flex().flex_row().flex_1();
        if show_sidebar {
            mid_row = mid_row.child(self.render_runs(cx, viewport_w));
        }

        // Narrow mode has no sidebar to host the footer, so the status
        // line stays a slim bar under the terminal there.
        let mut term_col = div()
            .flex()
            .flex_col()
            .flex_1()
            .child(self.render_error_banner(cx))
            // In-app help (`?` toggle) and the new-session screen (open
            // picker) replace the terminal pane while open; `?` again
            // (or Close) returns, Enter/Esc confirms/cancels the screen.
            .child(if self.show_help {
                self.render_help(cx)
            } else if self.picker_open() {
                self.render_new_session_screen(cx)
            } else {
                self.render_terminal(cx)
            });
        if !show_sidebar {
            term_col = term_col.child(self.render_status_bar(viewport_w));
        }

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x11111b))
            .track_focus(&self.list_focus.clone().unwrap())
            .id(ElementId::Name("app-root".into()))
            .on_key_down(cx.listener(|this, ev, window, cx| {
                this.on_key(ev, window, cx);
            }))
            .child(
                mid_row.child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .child(term_col)
                        // Focus frame (issue #56): a constant 1px border
                        // around the terminal pane, visible only while
                        // it owns the keyboard. The width never changes,
                        // so toggling focus never resizes the PTY grid;
                        // the status-line tag carries the non-color cue.
                        .border_1()
                        .border_color(if self.app.is_terminal_focused() {
                            rgb(super::shell::TERMINAL_FOCUS_BORDER)
                        } else {
                            rgb(0x11111b)
                        })
                        // Tracked: clicking here must move real keyboard
                        // focus, or typed keys never reach `muse`. Drag
                        // highlights terminal text (copy-on-select);
                        // Cmd+C copies, Cmd/Ctrl+V pastes.
                        .track_focus(&self.term_focus.clone().unwrap())
                        .id(ElementId::Name("terminal-pane".into()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseDownEvent, window, _cx| {
                                this.focus_term(window);
                                this.begin_selection(ev.position);
                            }),
                        )
                        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _window, _cx| {
                            this.update_selection(ev.position);
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseUpEvent, _window, cx| {
                                this.end_selection(ev.position, cx);
                            }),
                        )
                        .on_mouse_up_out(
                            MouseButton::Left,
                            cx.listener(|this, ev: &MouseUpEvent, _window, cx| {
                                this.end_selection(ev.position, cx);
                            }),
                        )
                        .on_click(cx.listener(|this, _ev, window, _cx| {
                            this.focus_term(window);
                        })),
                ),
            )
    }
}

impl ShellView {
    /// Status line element: the same text the sidebar footer shows in
    /// wide mode, kept as a slim bar under the terminal only where no
    /// sidebar exists to host it (narrow mode, issue #32). It doubles
    /// as the window drag region there (issue #44): the hidden title
    /// bar leaves narrow mode no other draggable chrome, and the bar
    /// itself has no clickable children to conflict with.
    pub(crate) fn render_status_bar(&self, viewport_w: f32) -> impl IntoElement {
        div()
            .h(px(STATUS_HEIGHT))
            .window_control_area(WindowControlArea::Drag)
            .px_2()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(super::theme::SECONDARY_FG))
            .text_sm()
            .truncate()
            .child(self.status_text_for_width(viewport_w))
            .id(ElementId::Name("status-bar".into()))
    }
}

impl ShellView {
    /// Resize the active PTY to the central pane, measured in monospace cells.
    /// Narrow viewports (<700px) collapse the sidebar, so the pane (and the
    /// PTY) use the full window width instead of squeezing beside 264px.
    pub(crate) fn fit_pty(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let viewport = window.viewport_size();
        let viewport_w = f32::from(viewport.width);
        let avail_w =
            viewport_w - effective_sidebar_width_for(viewport_w, self.app.sidebar_width());
        // Issue #32: wide mode has no header and no status bar, so the
        // terminal owns the full height; narrow mode keeps the slim bar.
        let avail_h = f32::from(viewport.height) - super::layout::chrome_height_for(viewport_w);
        // Metrics come from the font cache: only the grid math below
        // re-runs per frame, never the font-system measure.
        let (char_w, line_h) = self.cached_mono_metrics(cx);
        self.char_w = char_w;
        self.line_h = line_h;
        let (cols, rows) = super::layout::pty_grid_for(avail_w, avail_h, char_w, line_h);
        if (cols, rows) != (self.cols, self.rows) {
            self.cols = cols;
            self.rows = rows;
            if let Some(id) = self.active_id() {
                if let Some(run) = self.runs.get_mut(&id) {
                    run.pty.resize(cols, rows);
                }
            }
        }
    }
}

impl ShellView {
    /// Font-system metrics, cached per configured font (issues #35,
    /// #41): the text system is app-global rather than per-window, so
    /// resizes and display moves change the grid math in [`Self::fit_pty`]
    /// but never re-measure — unless the configured family/fallbacks/size
    /// changed, which drops the stale entry.
    fn cached_mono_metrics(&mut self, cx: &mut Context<ShellView>) -> (f32, f32) {
        // The PTY grid measures the resolved font (issue #53): advances
        // from a missing primary belong to gpui's proportional stand-in,
        // not to the monospace face that renders. The probe fills the
        // head on its first pass, so metrics converge by the next frame.
        self.probe_terminal_font(cx);
        let term_font = self.term_font();
        let size = self.term_font_size();
        let key = (mono_cache_key(&term_font), size.to_bits());
        if let Some((k, m)) = self.mono_metrics.as_ref() {
            if *k == key {
                return *m;
            }
        }
        let metrics = mono_metrics(cx, &term_font, size);
        self.mono_metrics = Some((key, metrics));
        metrics
    }
}

/// Cache key for the measured terminal font (issue #41): the primary
/// family plus the fallback tail, so a changed stack re-measures.
pub(crate) fn mono_cache_key(font: &Font) -> String {
    let mut key = font.family.to_string();
    if let Some(fallbacks) = font.fallbacks.as_ref() {
        for fallback in fallbacks.fallback_list() {
            key.push('\0');
            key.push_str(fallback);
        }
    }
    key
}

/// Monospace cell metrics for PTY sizing, with sane fallbacks. The full
/// terminal [`Font`] (issue #41) comes from the terminal config via
/// [`ShellView::cached_mono_metrics`], so the PTY grid measures the same
/// font the pane renders — including the fallback resolution when the
/// primary family is missing.
pub(crate) fn mono_metrics(cx: &mut Context<ShellView>, font: &Font, size: f32) -> (f32, f32) {
    let size = px(size);
    let system = cx.text_system();
    let fid = system.resolve_font(font);
    let char_w = system
        .advance(fid, size, ' ')
        .map(|s| f32::from(s.width))
        .unwrap_or(8.0);
    let line_h = f32::from(system.ascent(fid, size)) + f32::from(system.descent(fid, size));
    let line_h = if line_h > 0.0 { line_h } else { 18.0 };
    (char_w.max(4.0), line_h.max(8.0))
}

impl ShellView {
    /// In-app help panel: the full keymap as text rows plus a Close button,
    /// dismissed by `?` — the keymap no longer lives only in the README.
    pub(crate) fn render_help(&self, cx: &mut Context<Self>) -> gpui::Div {
        let mut col = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .bg(rgb(0x1e1e2e))
            .text_color(super::terminal::to_hsla(super::shell::DEFAULT_FG))
            .text_sm()
            .child(
                div()
                    .text_color(rgb(super::theme::SECONDARY_FG))
                    .child("Keys — press ? to close".to_string()),
            );
        for (key, what) in super::nav::help_entries() {
            col = col.child(format!("{key}   {what}"));
        }
        col.child(
            Button::new(ElementId::Name("help-close-btn".into()))
                .label("Close (?)")
                .small()
                .on_click(cx.listener(|this, _ev, _window, _cx| {
                    this.show_help = false;
                })),
        )
    }

    /// First-run orientation copy: icon + message + error flag so the
    /// empty state (`No session yet`) and the failure state (`spawn
    /// failed`) differ by shape and color, not text alone.
    pub(crate) fn empty_pane_copy(&self) -> (&'static str, String, bool) {
        match self.app.error_text() {
            Some(e) => ("✕", format!("spawn failed: {e}"), true),
            None => (
                "○",
                // The effective spawn command is the UI affordance for
                // issue #33: configured flags are visible before launch.
                format!(
                    "No session yet. Press n to start: {}. Press ? for keys.",
                    self.app.spawn_command_string()
                ),
                false,
            ),
        }
    }

    /// Empty terminal pane: icon-split status line plus the single
    /// clickable `+ New` button (keyboard parity: `n` repeats the last
    /// launch, `N` opens the same in-place screen) so the first run
    /// does not depend on discovering the key hint.
    pub(crate) fn render_empty_pane(&self, cx: &mut Context<Self>) -> gpui::Div {
        let (icon, msg, is_error) = self.empty_pane_copy();
        let color = if is_error {
            rgb(0xff9999)
        } else {
            rgb(super::theme::SECONDARY_FG)
        };
        div().flex_1().h_full().p_4().child(
            div()
                .flex()
                .flex_col()
                .child(div().text_color(color).child(format!("{icon} {msg}")))
                .child(
                    div().pt_2().child(
                        Button::new(ElementId::Name("empty-new-run-btn".into()))
                            .label("+ New")
                            .primary()
                            .small()
                            .on_click(cx.listener(|this, _ev, _window, _cx| {
                                // The single New session button: always
                                // opens the in-place new-session screen on
                                // the right side.
                                this.open_launch_picker();
                            })),
                    ),
                ),
        )
    }
}

impl ShellView {
    /// In-place new-session screen: the right-side (terminal column)
    /// replacement for the old choose-folder/CLI/options modal. Shown
    /// every time New session is pressed (the single `+ New` button
    /// always opens the picker); Enter starts the run, Esc cancels.
    /// Every row is clickable and mirrors the keyboard capture, so
    /// mouse and keyboard never desync: machine rows pick the host,
    /// browser rows pick the remote dir, CLI rows move the cursor,
    /// and Start/Cancel confirm/abort like Enter/Esc.
    pub(crate) fn render_new_session_screen(&self, cx: &mut Context<Self>) -> gpui::Div {
        let mut col = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .text_sm()
            .child(div().child("New session".to_string()));
        col = col.child(self.render_screen_machine(cx));
        col = col.child(self.render_screen_folder(cx));
        col = col.child(self.render_screen_clis(cx));
        if self.picker_remote.ssh_add.is_some() {
            col = col.child(self.render_ssh_add_dialog(cx));
        }
        col = col.child(self.picker_preview());
        col.child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(
                    Button::new(ElementId::Name("new-screen-start-btn".into()))
                        .label("Start (Enter)")
                        .primary()
                        .small()
                        .on_click(cx.listener(|this, _ev, window, _cx| {
                            this.accept_launch_picker();
                            this.focus_term(window);
                        })),
                )
                .child(
                    Button::new(ElementId::Name("new-screen-cancel-btn".into()))
                        .label("Cancel (Esc)")
                        .small()
                        .on_click(cx.listener(|this, _ev, _window, _cx| {
                            this.cancel_launch_picker();
                        })),
                ),
        )
    }

    /// Machine section: the value row (click toggles the dropdown)
    /// plus one row per option while open (This machine, saved hosts,
    /// Add remote machine...). An empty focused axis shows the
    /// `user@hostname` placeholder hint.
    fn render_screen_machine(&self, cx: &mut Context<Self>) -> gpui::Div {
        let host = self.picker_remote.host_input.trim().to_string();
        let value = if host.is_empty() {
            super::picker::THIS_MACHINE_LABEL.to_string()
        } else {
            host.clone()
        };
        let hint = if host.is_empty() && self.picker_remote.host_focus {
            format!(" ({})", super::picker::MACHINE_PLACEHOLDER)
        } else {
            String::new()
        };
        let mut section = div().flex().flex_col().gap_1().child(
            div()
                .id(ElementId::Name("new-screen-machine-value".into()))
                .child(format!("Machine: {value}{hint}"))
                .on_click(cx.listener(|this, _ev, _window, _cx| {
                    this.toggle_machine_dropdown();
                })),
        );
        if self.picker_remote.machine_open {
            let mut items: Vec<SidebarMenuItem> = Vec::new();
            for (index, (_opt, label)) in self.machine_dropdown_rows().into_iter().enumerate() {
                let marker = if index == self.picker_remote.machine_cursor {
                    "> "
                } else {
                    "  "
                };
                items.push(
                    SidebarMenuItem::new(format!("{marker}{label}")).on_click(cx.listener(
                        move |this, _ev: &ClickEvent, _window, _cx| {
                            this.pick_machine_option(index);
                        },
                    )),
                );
            }
            section = section.child(SidebarMenu::new().children(items));
        }
        section
    }

    /// Folder section: the typed folder plus, on remote hosts, the
    /// folder browser (one `ls` round trip per host/folder, capped at
    /// 500 entries by the core parser). Clicking a directory picks it
    /// into the folder input; typing stays fully editable.
    fn render_screen_folder(&self, cx: &mut Context<Self>) -> gpui::Div {
        let folder = self
            .launch_picker
            .as_ref()
            .map(|p| p.folder_input.clone())
            .unwrap_or_default();
        let shown = if folder.trim().is_empty() {
            "(blank = current)".to_string()
        } else {
            folder
        };
        let mut section = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().child(format!("Folder: {shown}")));
        if self.picker_is_remote() {
            if self.picker_remote.remote_listing.is_empty() {
                section = section.child(
                    div()
                        .text_xs()
                        .child("(remote folder browser: listing...)".to_string()),
                );
            } else {
                let mut items: Vec<SidebarMenuItem> = Vec::new();
                for (index, entry) in self.picker_remote.remote_listing.iter().enumerate() {
                    let marker = if index == self.picker_remote.remote_listing_cursor {
                        "> "
                    } else {
                        "  "
                    };
                    let suffix = if entry.is_dir { "/" } else { "" };
                    let name = entry.name.clone();
                    items.push(
                        SidebarMenuItem::new(format!("{marker}{name}{suffix}")).on_click(
                            cx.listener(move |this, _ev: &ClickEvent, _window, _cx| {
                                this.pick_remote_listing(index);
                            }),
                        ),
                    );
                }
                section = section.child(SidebarMenu::new().children(items));
            }
        }
        section
    }

    /// CLI section: one row per detected CLI (availability in words,
    /// never color-only); clicking moves the cursor. The yolo row
    /// cycles default/on-once/off-once on click.
    fn render_screen_clis(&self, cx: &mut Context<Self>) -> gpui::Div {
        let mut items: Vec<SidebarMenuItem> = Vec::new();
        if let Some(picker) = self.launch_picker.as_ref() {
            for (index, cli) in picker.clis.iter().enumerate() {
                let marker = if index == picker.cli_cursor {
                    "> "
                } else {
                    "  "
                };
                let state = if cli.available {
                    "ready"
                } else {
                    "not installed"
                };
                let id = cli.id.clone();
                items.push(
                    SidebarMenuItem::new(format!("{marker}{id} ({state})")).on_click(cx.listener(
                        move |this, _ev: &ClickEvent, _window, _cx| {
                            this.select_picker_cli(index);
                        },
                    )),
                );
            }
        }
        let yolo = self.picker_yolo_label().to_string();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().child("Agent CLI: (click picks, Enter starts)".to_string()))
            .child(SidebarMenu::new().children(items))
            .child(
                div()
                    .id(ElementId::Name("new-screen-yolo".into()))
                    .child(format!("Yolo: {yolo} (click cycles)"))
                    .on_click(cx.listener(|this, _ev, _window, _cx| {
                        this.cycle_picker_yolo();
                    })),
            )
    }

    /// SSH add dialog: display name, host (`user@hostname` or alias),
    /// and default folder (typed, Tab cycles fields), plus the Test
    /// button wired to the core tester (reachable/unreachable in words
    /// plus the detected CLIs) and Save/Cancel backed by the core
    /// saved-hosts store.
    fn render_ssh_add_dialog(&self, cx: &mut Context<Self>) -> gpui::Div {
        let (name, host, folder, focus, error) = match self.picker_remote.ssh_add.as_ref() {
            Some(d) => (
                d.name_input.clone(),
                d.host_input.clone(),
                d.folder_input.clone(),
                d.focus,
                d.error.clone(),
            ),
            None => (String::new(), String::new(), String::new(), 0, None),
        };
        let shown = |value: &str| {
            if value.is_empty() {
                "(blank)".to_string()
            } else {
                value.to_string()
            }
        };
        let mark = |n: usize| if n == focus { "> " } else { "  " };
        let mut card = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().child("Add remote machine...".to_string()))
            .child(div().child(format!("{}Display name: {}", mark(0), shown(&name))))
            .child(div().child(format!(
                "{}Host (user@hostname or alias): {}",
                mark(1),
                shown(&host)
            )))
            .child(div().child(format!("{}Default folder: {}", mark(2), shown(&folder))));
        let status = match self.ssh_add_test_label() {
            Some(label) => format!("Test: {label}"),
            None => "Test: not run yet (click Test)".to_string(),
        };
        card = card.child(div().child(status));
        if let Some(err) = error {
            card = card.child(div().child(format!("Error: {err}")));
        }
        card.child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(
                    Button::new(ElementId::Name("ssh-add-test-btn".into()))
                        .label("Test")
                        .small()
                        .on_click(cx.listener(|this, _ev, _window, _cx| {
                            this.test_ssh_add();
                        })),
                )
                .child(
                    Button::new(ElementId::Name("ssh-add-save-btn".into()))
                        .label("Save")
                        .primary()
                        .small()
                        .on_click(cx.listener(|this, _ev, _window, _cx| {
                            this.save_ssh_add();
                        })),
                )
                .child(
                    Button::new(ElementId::Name("ssh-add-cancel-btn".into()))
                        .label("Cancel")
                        .small()
                        .on_click(cx.listener(|this, _ev, _window, _cx| {
                            this.cancel_ssh_add();
                        })),
                ),
        )
    }
}

/// Native traffic-light anchor (issue #51): close-button origin at
/// 20pt leading (the macOS standard inset) and 14pt from the window
/// top. The position is window-relative, so wide mode (lights over the
/// sidebar header strip) and narrow mode (lights over the terminal
/// corner) share it — no per-width code, one assertion covers both.
pub(crate) const TRAFFIC_LIGHT_POS_X: f32 = 20.0;
pub(crate) const TRAFFIC_LIGHT_POS_Y: f32 = 14.0;
/// Traffic-light button diameter on macOS (12pt).
pub(crate) const TRAFFIC_LIGHT_DIAMETER: f32 = 12.0;
/// Bottom edge of the native traffic lights (`POS_Y + DIAMETER`,
/// 26pt): panel chrome starts below this (see
/// [`super::runs_panel::SIDEBAR_HEADER_TOP_PAD`]).
pub(crate) const TRAFFIC_LIGHT_BOTTOM: f32 = TRAFFIC_LIGHT_POS_Y + TRAFFIC_LIGHT_DIAMETER;

/// Window options for the main window. The minimum size is derived from
/// the PTY floors (see [`MIN_WINDOW_WIDTH`]/[`MIN_WINDOW_HEIGHT`]) so the
/// OS never shrinks the window past what the terminal grid can display.
///
/// Issues #44/#51/#58: no OS title bar, but the native traffic lights
/// stay. `titlebar: None` was wrong — on macOS it maps to a style mask
/// without `NSResizableWindowMask`, killing edge/corner resize (plus
/// close/minimize/zoom). `Some` + `appears_transparent` (the same shape
/// as `gpui_component::TitleBar::title_bar_options()`) keeps
/// Titled|Closable|Resizable|Miniaturizable with full-size content and
/// native, working lights at the explicit anchor above; dragging still
/// works through the custom drag regions (sidebar header strip in wide
/// mode, slim status bar in narrow mode). The custom drag regions cannot
/// swallow resize handles: on macOS gpui leaves
/// `on_hit_test_window_control` unimplemented, so window-control
/// hitboxes never divert edge hits from OS resize handling.
/// `is_resizable` stays explicit so a future edit cannot silently drop it.
pub fn window_options() -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some(SharedString::from("Staap")),
            appears_transparent: true,
            traffic_light_position: Some(gpui::Point {
                x: px(TRAFFIC_LIGHT_POS_X),
                y: px(TRAFFIC_LIGHT_POS_Y),
            }),
        }),
        is_resizable: true,
        window_min_size: Some(Size {
            width: px(MIN_WINDOW_WIDTH),
            height: px(MIN_WINDOW_HEIGHT),
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::super::layout::{
        FALLBACK_CHAR_W, FALLBACK_LINE_H, LEFT_WIDTH, MIN_COLS, MIN_ROWS, STATUS_HEIGHT,
    };
    use super::super::runs::test_shell;
    use super::super::terminal_pane::terminal_font;
    use super::*;

    #[test]
    fn hidden_titlebar_frees_the_os_row_without_touching_the_minimum() {
        // Issues #44/#51/#58: no OS title bar — the content runs
        // edge-to-edge behind a transparent titlebar (traffic lights
        // float over it), so the terminal gains the title-bar row's
        // pixels. The hidden titlebar is `Some`-transparent, never
        // `None` — `None` drops `NSResizableWindowMask` on macOS and the
        // window stops resizing entirely (plus close/miniaturize). The
        // native lights sit at the explicit window-relative anchor, so
        // wide and narrow mode share it.
        let opts = super::window_options();
        let titlebar = opts
            .titlebar
            .as_ref()
            .expect("hidden titlebar stays a transparent Some, not None");
        assert!(
            titlebar.appears_transparent,
            "OS title bar must stay hidden"
        );
        assert!(
            opts.is_resizable,
            "main window must stay user-resizable (issue #58)"
        );
        let pos = titlebar
            .traffic_light_position
            .expect("explicit traffic-light anchor (issue #51)");
        assert_eq!(f32::from(pos.x), super::TRAFFIC_LIGHT_POS_X);
        assert_eq!(f32::from(pos.y), super::TRAFFIC_LIGHT_POS_Y);
        assert!(
            opts.is_resizable && opts.is_minimizable && opts.is_movable,
            "zoom + minimize + drag must stay enabled (issue #51)"
        );
        let min = opts
            .window_min_size
            .expect("main window sets a minimum size");
        assert_eq!(f32::from(min.width), MIN_WINDOW_WIDTH);
        assert_eq!(f32::from(min.height), MIN_WINDOW_HEIGHT);
    }

    #[test]
    fn window_min_size_matches_pty_floors() {
        // Issue #6: the OS minimum must fit the PTY floors, not clip them.
        // Issue #32: no header anymore — only the narrow-mode status bar
        // sits below the grid at the minimum size.
        let min = super::window_options()
            .window_min_size
            .expect("main window sets a minimum size");
        assert_eq!(f32::from(min.width), MIN_WINDOW_WIDTH);
        assert_eq!(f32::from(min.height), MIN_WINDOW_HEIGHT);
        // At the minimum size the terminal pane still fits a full
        // MIN_COLS x MIN_ROWS grid at fallback metrics.
        let (cols, rows) = super::super::layout::pty_grid_for(
            f32::from(min.width) - LEFT_WIDTH,
            f32::from(min.height) - STATUS_HEIGHT,
            FALLBACK_CHAR_W,
            FALLBACK_LINE_H,
        );
        assert!(cols >= MIN_COLS, "min width fits {cols} cols");
        assert!(rows >= MIN_ROWS, "min height fits {rows} rows");
    }

    #[test]
    fn mono_cache_key_covers_primary_and_fallback_tail() {
        // Issue #41: the metrics cache re-measures when the rendered
        // font changes — a swapped primary or a changed tail drops the
        // stale entry, while identical configs reuse it.
        use crate::config::TerminalConfig;
        let base = terminal_font(&TerminalConfig::default());
        assert_eq!(mono_cache_key(&base), mono_cache_key(&base));
        let custom = terminal_font(&TerminalConfig {
            font_family: "Iosevka Nerd Font".to_string(),
            ..TerminalConfig::default()
        });
        assert_ne!(mono_cache_key(&base), mono_cache_key(&custom));
        let retailed = terminal_font(&TerminalConfig {
            fallback_fonts: vec!["Menlo".to_string()],
            ..TerminalConfig::default()
        });
        assert_ne!(mono_cache_key(&base), mono_cache_key(&retailed));
    }

    #[test]
    fn empty_pane_advertises_configured_spawn_flags() {
        // Issue #33: the empty pane names the effective spawn command so
        // configured flags are visible before launch.
        use crate::config::{AgentConfig, Config};
        let view = test_shell();
        let (_, msg, _) = view.empty_pane_copy();
        assert!(msg.starts_with("No session yet"));
        assert!(msg.contains("start: muse."), "plain spawn: {msg:?}");
        let mut flagged = test_shell();
        let mut cfg = Config::default();
        cfg.agents.insert(
            "muse".to_string(),
            AgentConfig {
                extra_args: vec!["--yolo".to_string()],
                yolo: false,
            },
        );
        flagged.app.set_config(cfg);
        let (_, msg, _) = flagged.empty_pane_copy();
        assert!(msg.contains("muse --yolo"), "flags shown: {msg:?}");
    }

    #[test]
    fn empty_pane_splits_empty_vs_failed_by_icon() {
        let mut view = test_shell();
        // No sessions, no error: the calm empty state.
        let (icon, msg, is_error) = view.empty_pane_copy();
        assert_eq!(icon, "○");
        assert!(msg.starts_with("No session yet"));
        assert!(!is_error);
        // A recorded spawn failure flips icon, copy, and error flag so the
        // states differ by shape, not text alone.
        view.app.start_new_session();
        let _ = view.app.take_pending_spawn();
        view.app.set_error("missing binary");
        let (icon, msg, is_error) = view.empty_pane_copy();
        assert_eq!(icon, "✕");
        assert!(msg.starts_with("spawn failed:"));
        assert!(is_error);
        // Dismissal restores the empty state.
        view.app.clear_error();
        let (icon, msg, is_error) = view.empty_pane_copy();
        assert_eq!(icon, "○");
        assert!(msg.starts_with("No session yet"));
        assert!(!is_error);
    }
}
