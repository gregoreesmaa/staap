//! Input handling for [`super::shell::ShellView`]: nav-focus key dispatch
//! plus the clipboard actions keys and mouse invoke.
//!
//! Key dispatch is pure state (headlessly testable): the caller applies
//! window focus and clipboard effects for the returned action. `muse`
//! captures keys if and only if focus is Terminal — nav keys never reach
//! the PTY. Parsed-link rows are keyboard-operable too: `o` cycles link
//! focus across the selected run's links, `Enter` opens the focused link
//! in the default browser (and copies it), `y` copies it without opening,
//! and `PgUp`/`PgDn` page the run list.

use gpui::{App as GpuiApp, ClipboardItem};

use super::shell::ShellView;

/// Explicit focus model (issue #56): where the next key goes. The
/// keyboard has exactly one owner at a time — a capture, the terminal
/// PTY, or nav dispatch — and [`ShellView::key_target`] computes it
/// from pure state so headless tests pin every route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyTarget {
    /// Title-filter capture owns every key (the `filtering` flag).
    FilterCapture,
    /// Folder-picker capture owns every key (the `cwd_capture` buffer).
    FolderCapture,
    /// 2D-launch picker owns every key (the `launch_picker` state).
    PickerCapture,
    /// Terminal focus: every typing key reaches the PTY, including
    /// `/`, `w`, and the comfort keys.
    Terminal,
    /// Nav focus: `/` starts filter capture, everything else dispatches.
    Nav,
    /// `/` in nav focus: begin filter capture (never reached while the
    /// terminal owns the keyboard).
    BeginFilter,
}

/// Outcome of a nav-focus keypress: state changes apply immediately,
/// window/clipboard effects are applied by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NavAction {
    Quit,
    FocusTerm,
    Copy,
    Paste,
    Retry,
    Restart,
    Close,
    Dismiss,
    /// Copy the keyboard-focused parsed link (see `link_cursor`).
    CopyLink,
    /// Open the keyboard-focused parsed link in the default browser
    /// (issue #49). The caller also copies it, so open and yank stay
    /// one key apart: `Enter` opens + copies, `y` copies only.
    OpenLink,
    /// Advance the theme choice (dark → light → system); the caller
    /// applies, persists, and flashes it.
    CycleTheme,
    /// Export the selected run's visible text plus links to markdown.
    Export,
    None,
}

/// In-app help entries: every nav key plus terminal-focus and mouse
/// bindings, so the full keymap no longer lives only in the README.
pub(crate) fn help_entries() -> Vec<(&'static str, &'static str)> {
    vec![
        ("n", "new session (repeat last folder × CLI)"),
        ("N", "new session picker (choose folder × CLI + yolo)"),
        ("j / k", "move selection between sessions"),
        ("↑ / ↓", "move selection between sessions"),
        ("PgUp / PgDn", "page the session list"),
        (
            "h",
            "collapse/expand History (Enter reveals a hidden selection)",
        ),
        (
            "Shift+PgUp / Shift+PgDn",
            "scroll the run's retained output (pager)",
        ),
        ("o", "cycle link focus across the selected run's links"),
        (
            "Enter",
            "open the focused link in the browser (+ copy), or type in muse when none",
        ),
        ("i", "type in muse"),
        ("Tab", "switch sessions ↔ terminal focus"),
        (
            "Cmd+1 / Cmd+2",
            "sessions list / type in muse (exits captures)",
        ),
        (
            "w",
            "new session in a chosen folder (blank = current folder)",
        ),
        (
            "y",
            "copy the focused link, else selection (or whole screen)",
        ),
        (
            "click link",
            "open in browser + copy · ⌘/Ctrl-click copies only",
        ),
        ("p", "paste clipboard into muse"),
        ("e", "export selected run to markdown (local file)"),
        ("r", "restart ended run / retry failed spawn"),
        ("x", "close (kill) the selected run"),
        ("d", "dismiss the sticky error"),
        ("/", "filter sessions by title substring"),
        ("+ / -", "terminal font size (saved locally)"),
        ("[ / ]", "sessions panel width (saved locally)"),
        ("?", "toggle this help"),
        ("Esc", "back to sessions · quit from sessions"),
        ("q", "quit (confirms first with live runs)"),
        ("drag", "select terminal text (copy-on-select)"),
        ("Cmd+C", "copy selection (or screen)"),
        ("Cmd/Ctrl+V", "paste clipboard"),
    ]
}

impl ShellView {
    /// URL of the keyboard-focused parsed link, if the selected run has
    /// one at [`Self::link_cursor`]. The cursor walks PR links first,
    /// then related (issue/commit/commit) links, so every openable row
    /// the panel shows is keyboard-reachable (issue #49).
    pub(crate) fn focused_link_url(&self) -> Option<String> {
        let cursor = self.link_cursor?;
        let id = self.active_id()?;
        self.app.sessions.iter().find(|s| s.id == id).and_then(|s| {
            s.pr_links
                .iter()
                .chain(s.related_links.iter())
                .nth(cursor)
                .cloned()
        })
    }

    /// True when the keyboard-focused link is a PR link (rather than a
    /// related link): picks the "opened + copied PR link" vs "… link"
    /// status wording.
    pub(crate) fn focused_link_is_pr(&self) -> bool {
        let Some(cursor) = self.link_cursor else {
            return false;
        };
        let Some(id) = self.active_id() else {
            return false;
        };
        self.app
            .sessions
            .iter()
            .find(|s| s.id == id)
            .is_some_and(|s| cursor < s.pr_links.len())
    }

    /// Advance link focus through the selected run's parsed links,
    /// wrapping back to unfocused after the last one (so Enter can focus
    /// the terminal again without moving the run selection).
    pub(crate) fn cycle_link_focus(&mut self) {
        let count = match self.active_id() {
            Some(id) => self
                .app
                .sessions
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.pr_links.len() + s.related_links.len())
                .unwrap_or(0),
            None => 0,
        };
        if count == 0 {
            self.link_cursor = None;
            return;
        }
        self.link_cursor = match self.link_cursor {
            None => Some(0),
            Some(i) if i + 1 < count => Some(i + 1),
            _ => None,
        };
    }

    /// Toggle History expansion and persist it (issue #55): the single
    /// funnel for the `h` key, Enter on a hidden history selection,
    /// and the History header click — every path persists, so restarts
    /// restore the state. Skipped in unit-test builds (same split as
    /// the comfort-key persist): tests cover the state flip here while
    /// the save/load roundtrip is covered in `config.rs`.
    pub(crate) fn toggle_history_expanded(&mut self) {
        self.app.toggle_history();
        #[cfg(not(test))]
        let _ = self.app.save_config();
    }

    /// Enter reveals a hidden history selection (issue #55): the
    /// selection sits inside collapsed History (and passes the filter,
    /// so expanding actually shows it), so Enter expands instead of
    /// focusing the terminal of a dead run. False whenever the
    /// selection is already visible — normal Enter behavior applies.
    pub(crate) fn enter_expands_history(&self) -> bool {
        if self.app.history_expanded {
            return false;
        }
        self.app
            .selected_session()
            .is_some_and(|s| self.is_history(s) && self.app.matches_filter(s))
    }

    /// Selection visibility (issue #39): the selected row renders iff it
    /// passes the title filter and is not tucked inside collapsed
    /// History. A hidden selection keeps working (Enter expands to
    /// reveal it, `r` still acts on it) and the History header
    /// highlights as its visible anchor.
    pub(crate) fn selection_visible(&self) -> bool {
        match self.app.selected_session() {
            None => true,
            Some(s) => {
                self.app.matches_filter(s) && (self.app.history_expanded || !self.is_history(s))
            }
        }
    }

    /// j/k step that skips collapsed History (issue #39): a single step
    /// when the landing row is visible, otherwise keep stepping in the
    /// same direction — at most one full cycle, so an all-hidden list
    /// keeps its selection instead of looping forever.
    pub(crate) fn step_selection(&mut self, forward: bool) {
        let n = self.app.sessions.len();
        if n == 0 {
            return;
        }
        let start = self.app.selected;
        for _ in 0..n {
            if forward {
                self.app.select_next();
            } else {
                self.app.select_prev();
            }
            if self.app.selected == start || self.selection_visible() {
                break;
            }
        }
        self.clear_selection();
        self.link_cursor = None;
    }

    /// PgUp/PgDn that skip collapsed History (issue #39) while paging
    /// keeps working (issue #40): the page jump, then single steps out
    /// of hidden rows — restoring the pre-page selection when the whole
    /// direction is hidden.
    pub(crate) fn page_selection(&mut self, forward: bool) {
        let n = self.app.sessions.len();
        if n == 0 {
            return;
        }
        let start = self.app.selected;
        if forward {
            self.app.select_page_next();
        } else {
            self.app.select_page_prev();
        }
        if self.selection_visible() {
            self.clear_selection();
            self.link_cursor = None;
            return;
        }
        let landed = self.app.selected;
        for _ in 0..n {
            if forward {
                self.app.select_next();
            } else {
                self.app.select_prev();
            }
            if self.app.selected == landed || self.selection_visible() {
                break;
            }
        }
        if !self.selection_visible() {
            self.app.selected = start;
        }
        self.clear_selection();
        self.link_cursor = None;
    }

    /// Route the next key to its keyboard owner (issue #56), pure
    /// state (headlessly testable). Captures outrank focus; terminal
    /// focus owns every typing key (`/` included, so it can never
    /// hijack terminal input); nav focus routes `/` into a new filter
    /// capture and everything else to nav dispatch. `ctrl` combos never
    /// start the filter.
    pub(crate) fn key_target(&self, key: &str, key_char: Option<&str>, ctrl: bool) -> KeyTarget {
        if self.filtering {
            return KeyTarget::FilterCapture;
        }
        if self.cwd_capture.is_some() {
            return KeyTarget::FolderCapture;
        }
        if self.launch_picker.is_some() {
            return KeyTarget::PickerCapture;
        }
        if self.app.is_terminal_focused() {
            return KeyTarget::Terminal;
        }
        if !ctrl && (key == "/" || key_char == Some("/")) {
            return KeyTarget::BeginFilter;
        }
        KeyTarget::Nav
    }

    /// Global pane shortcut (Cmd+1 sessions / Cmd+2 terminal, issue
    /// #56): exits any capture — filter text is kept (accepted), the
    /// folder buffer is dropped — clears an armed quit, and moves app
    /// focus. The caller applies window focus on top. Unlike Tab this
    /// works from inside a capture, so a lost user can always jump
    /// panes with one chord.
    pub(crate) fn switch_pane(&mut self, terminal: bool) {
        if self.filtering {
            self.accept_filter();
        }
        self.cwd_capture = None;
        // The picker capture drops the same way (zero side effects),
        // including its remote host/probe half.
        self.cancel_launch_picker();
        self.quit_armed = false;
        if terminal {
            self.app.focus_terminal();
        } else {
            self.app.focus_nav();
        }
    }

    /// Non-color focus indicator (issue #56): the status line names the
    /// keyboard owner in words, so focus never depends on color alone.
    pub(crate) fn focus_indicator(&self) -> &'static str {
        if self.app.is_terminal_focused() {
            "▸ terminal"
        } else {
            "▸ sessions"
        }
    }

    /// Nav-focus key dispatch, pure state (headlessly testable). The caller
    /// applies window focus and clipboard effects for the returned action.
    /// `muse` captures keys if and only if focus is Terminal — nav keys
    /// never reach the PTY.
    pub(crate) fn nav_action(&mut self, key: &str, ctrl: bool) -> NavAction {
        let action = match (key, ctrl) {
            ("q", false) | ("escape", _) => NavAction::Quit,
            ("j", false) | ("down", _) => {
                self.step_selection(true);
                NavAction::None
            }
            ("k", false) | ("up", _) => {
                self.step_selection(false);
                NavAction::None
            }
            ("pagedown", _) => {
                self.page_selection(true);
                NavAction::None
            }
            ("pageup", _) => {
                self.page_selection(false);
                NavAction::None
            }
            ("h", false) => {
                self.toggle_history_expanded();
                NavAction::None
            }
            ("n", false) => {
                // Capped by the live-run policy (issue #31): a refused
                // 11th run stays in the list so the flash is readable.
                // 2D launch: instant repeat-last (expert fast path).
                if self.request_new_run() {
                    NavAction::FocusTerm
                } else {
                    NavAction::None
                }
            }
            ("N", false) => {
                // Full 2D picker (folder × CLI + yolo) on Shift-N.
                self.open_launch_picker();
                NavAction::None
            }
            ("o", false) => {
                self.cycle_link_focus();
                NavAction::None
            }
            ("i", false) => NavAction::FocusTerm,
            ("enter", _) if self.focused_link_url().is_some() => NavAction::OpenLink,
            // Issue #55: Enter on a hidden history selection expands
            // History (revealing it) instead of focusing the terminal.
            ("enter", _) if self.enter_expands_history() => {
                self.toggle_history_expanded();
                NavAction::None
            }
            ("enter", _) => NavAction::FocusTerm,
            // Yank (issue #49): with a link focused this copies the
            // link without opening it; otherwise it copies the
            // selection/screen exactly as before.
            ("y", false) if self.focused_link_url().is_some() => NavAction::CopyLink,
            ("y", false) => NavAction::Copy,
            ("p", false) => NavAction::Paste,
            ("e", false) => NavAction::Export,
            ("r", false) if self.can_restart() => NavAction::Restart,
            ("r", false) if self.can_retry() => NavAction::Retry,
            ("r", false) if self.can_resume() => NavAction::Restart,
            ("x", false) => NavAction::Close,
            ("d", false) if self.app.error_text().is_some() => NavAction::Dismiss,
            // `?` toggles the in-app help panel. (`/` used to be an alias;
            // since issue #29 it opens title-filter capture instead — the
            // shell routes it before `nav_action`, so it never reaches here.)
            ("?", _) => {
                self.show_help = !self.show_help;
                NavAction::None
            }
            ("t", false) => NavAction::CycleTheme,
            _ => NavAction::None,
        };
        // Any key other than a quit intent cancels an armed quit.
        if action != NavAction::Quit {
            self.quit_armed = false;
        }
        action
    }

    /// Copy the mouse selection when one exists, else the whole active
    /// screen, to the system clipboard.
    pub(crate) fn copy_screen(&mut self, cx: &mut GpuiApp) {
        // A paged run copies from the visible pager slice, not the live
        // grid underneath (issue #25).
        if let Some(text) = self.selected_pager_text().or_else(|| self.selected_text()) {
            let chars = text.chars().count();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.app
                .set_status(format!("copied selection ({chars} chars)"));
            return;
        }
        if let Some(view) = self.active_view() {
            let text = view.screen.contents();
            let lines = text.lines().count();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.app
                .set_status(format!("yanked {lines} lines to clipboard"));
        }
    }

    /// Platform command that opens `url` in the default browser
    /// (issue #49): `open` on macOS, `xdg-open` on Linux/Unix, `cmd /c
    /// start` on Windows. Pure construction so tests assert the exact
    /// URL is passed through without launching anything.
    pub(crate) fn browser_command(url: &str) -> std::process::Command {
        #[cfg(target_os = "macos")]
        {
            let mut cmd = std::process::Command::new("open");
            cmd.arg(url);
            cmd
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let mut cmd = std::process::Command::new("xdg-open");
            cmd.arg(url);
            cmd
        }
        #[cfg(target_os = "windows")]
        {
            let mut cmd = std::process::Command::new("cmd");
            cmd.args(["/c", "start", "", url]);
            cmd
        }
        #[cfg(not(any(unix, target_os = "windows")))]
        {
            let mut cmd = std::process::Command::new("xdg-open");
            cmd.arg(url);
            cmd
        }
    }

    /// Open `url` in the default browser (issue #49). Best effort like
    /// every other local spawn here: the caller reports the error in
    /// the status line instead of failing.
    pub(crate) fn open_url(url: &str) -> Result<(), String> {
        Self::browser_command(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Open-or-copy split for a link click (issue #49): a plain click
    /// opens the URL in the browser and copies it ("opened + copied");
    /// a modifier-click (Cmd/Ctrl/Shift) copies only ("copied"). The
    /// caller applies the clipboard write and the status flash for the
    /// returned outcome; opening itself runs here.
    pub(crate) fn activate_link(
        &mut self,
        url: &str,
        is_pr: bool,
        copy_only: bool,
        cx: &mut GpuiApp,
    ) {
        cx.write_to_clipboard(ClipboardItem::new_string(url.to_string()));
        let kind = if is_pr { "PR link" } else { "link" };
        if copy_only {
            self.app.set_status(format!("copied {kind}"));
            return;
        }
        match Self::open_url(url) {
            Ok(()) => self.app.set_status(format!("opened + copied {kind}")),
            Err(e) => self
                .app
                .set_status(format!("copied {kind} (open failed: {e})")),
        }
    }

    /// Paste the system clipboard into the active PTY as typed bytes.
    pub(crate) fn paste_clipboard(&mut self, cx: &mut GpuiApp) {
        let Some(item) = cx.read_from_clipboard() else {
            self.app.set_status("clipboard is empty");
            return;
        };
        let Some(text) = item.text() else {
            self.app.set_status("clipboard has no text");
            return;
        };
        if text.is_empty() {
            self.app.set_status("clipboard is empty");
            return;
        }
        let Some(id) = self.active_id() else {
            return;
        };
        if let Some(run) = self.runs.get_mut(&id) {
            if let Err(e) = run.pty.write_input(text.as_bytes()) {
                self.app.set_error(e.to_string());
            } else {
                let chars = text.chars().count();
                self.app.set_status(format!("pasted {chars} chars"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::runs::test_shell;
    use super::NavAction;

    #[test]
    fn nav_keys_never_reach_the_pty_only_terminal_focus_forwards() {
        let mut view = test_shell();
        // Nav: n repeats the last launch and yields FocusTerm; N opens the
        // 2D picker instead; q yields Quit.
        assert_eq!(view.nav_action("n", false), NavAction::FocusTerm);
        assert_eq!(view.app.sessions.len(), 1);
        assert!(view.app.is_terminal_focused());
        view.app.focus_nav();
        assert_eq!(view.nav_action("N", false), NavAction::None);
        assert!(view.picker_open());
        assert_eq!(view.app.sessions.len(), 1);
        view.cancel_launch_picker();
        assert_eq!(view.nav_action("q", false), NavAction::Quit);
        // y copies, p pastes; unknown keys are inert.
        assert_eq!(view.nav_action("y", false), NavAction::Copy);
        assert_eq!(view.nav_action("p", false), NavAction::Paste);
        assert_eq!(view.nav_action("z", false), NavAction::None);
        assert_eq!(view.nav_action("j", true), NavAction::None);
    }

    #[test]
    fn key_target_routes_by_focus_and_capture() {
        // Issue #56 explicit focus model: one keyboard owner at a time.
        use super::KeyTarget;
        let mut view = test_shell();
        view.app.focus_nav();
        assert_eq!(view.key_target("a", Some("a"), false), KeyTarget::Nav);
        assert_eq!(
            view.key_target("/", Some("/"), false),
            KeyTarget::BeginFilter
        );
        // Ctrl combos never start the filter.
        assert_eq!(view.key_target("/", Some("/"), true), KeyTarget::Nav);
        // Terminal focus owns every typing key, `/` included.
        view.app.focus_terminal();
        assert_eq!(view.key_target("/", Some("/"), false), KeyTarget::Terminal);
        assert_eq!(view.key_target("w", Some("w"), false), KeyTarget::Terminal);
        assert_eq!(view.key_target("a", Some("a"), false), KeyTarget::Terminal);
        // Captures outrank focus either way.
        view.begin_filter();
        assert_eq!(
            view.key_target("a", Some("a"), false),
            KeyTarget::FilterCapture
        );
        view.app.focus_terminal();
        assert_eq!(
            view.key_target("a", Some("a"), false),
            KeyTarget::FilterCapture
        );
        view.accept_filter();
        view.begin_cwd_capture();
        assert_eq!(
            view.key_target("a", Some("a"), false),
            KeyTarget::FolderCapture
        );
        view.cancel_cwd();
        // The 2D picker outranks nav dispatch like the other captures.
        view.open_launch_picker();
        assert_eq!(
            view.key_target("a", Some("a"), false),
            KeyTarget::PickerCapture
        );
        view.cancel_launch_picker();
    }

    #[test]
    fn slash_types_in_terminal_and_filters_only_in_nav() {
        // Issue #56: `/` focuses search without hijacking terminal
        // input — in terminal focus it types, in nav focus it captures.
        use super::KeyTarget;
        let mut view = test_shell();
        view.app.start_new_session();
        let _ = view.app.take_pending_spawn();
        assert!(view.app.is_terminal_focused());
        assert_eq!(view.key_target("/", Some("/"), false), KeyTarget::Terminal);
        view.forward_key("/", Some("/"), false, false);
        assert!(!view.filtering, "no capture steals terminal input");
        let id = view.active_id().unwrap();
        let session = view.app.sessions.iter().find(|s| s.id == id).unwrap();
        assert_eq!(session.pending_input, "/");
        // Nav focus: the same key opens capture instead of typing.
        view.app.focus_nav();
        assert_eq!(
            view.key_target("/", Some("/"), false),
            KeyTarget::BeginFilter
        );
        assert!(view.filter_key("/", Some("/"), false));
        assert!(view.filtering);
    }

    #[test]
    fn pane_shortcuts_move_focus_and_exit_captures() {
        // Issue #56: Cmd+1/Cmd+2 jump panes from anywhere — filter text
        // is kept (accepted), the folder buffer is dropped, an armed
        // quit disarms.
        let mut view = test_shell();
        view.app.focus_nav();
        view.begin_filter();
        view.filter_type("fox");
        view.quit_armed = true;
        view.switch_pane(true);
        assert!(view.app.is_terminal_focused());
        assert!(!view.filtering);
        assert_eq!(view.app.filter, "fox", "filter text kept");
        assert!(!view.quit_armed);
        view.switch_pane(false);
        assert!(!view.app.is_terminal_focused());
        // Folder capture drops its buffer instead of spawning.
        view.begin_cwd_capture();
        view.switch_pane(true);
        assert!(view.cwd_capture.is_none());
        assert!(view.app.is_terminal_focused());
        // The picker capture drops the same way (zero side effects).
        view.app.focus_nav();
        view.open_launch_picker();
        assert!(view.picker_open());
        view.switch_pane(true);
        assert!(!view.picker_open());
        assert!(view.app.is_terminal_focused());
    }

    #[test]
    fn focus_indicator_names_the_keyboard_owner() {
        // Issue #56 non-color cue: the status line tags the focused pane
        // in words, at every width, so focus never depends on color.
        let mut view = test_shell();
        view.app.focus_nav();
        assert_eq!(view.focus_indicator(), "▸ sessions");
        view.app.focus_terminal();
        assert_eq!(view.focus_indicator(), "▸ terminal");
        assert!(
            view.status_text_for_width(1280.0).starts_with("▸ terminal"),
            "wide tags terminal: {:?}",
            view.status_text_for_width(1280.0)
        );
        assert!(
            view.status_text_for_width(600.0).starts_with("▸ terminal"),
            "narrow tags terminal too"
        );
        view.app.focus_nav();
        assert!(
            view.status_text_for_width(1280.0).starts_with("▸ sessions"),
            "wide tags sessions"
        );
    }

    #[test]
    fn help_toggle_and_keymap_coverage() {
        // Issue #7: `?` toggles help in nav focus; the panel documents
        // every nav key plus terminal/mouse bindings, and every status
        // hint advertises `?`. (`/` was the discoverability alias; since
        // issue #29 it opens title-filter capture, routed in the shell
        // before `nav_action`, so it must not toggle help here.)
        let mut view = test_shell();
        assert!(!view.show_help);
        assert_eq!(view.nav_action("?", false), NavAction::None);
        assert!(view.show_help);
        assert_eq!(view.nav_action("?", false), NavAction::None);
        assert!(!view.show_help);
        assert_eq!(view.nav_action("/", false), NavAction::None);
        assert!(!view.show_help);
        let entries = super::help_entries();
        assert!(entries.len() >= 10);
        let keys: Vec<&str> = entries.iter().map(|(k, _)| *k).collect();
        for needed in [
            "n",
            "N",
            "j / k",
            "PgUp / PgDn",
            "o",
            "Enter",
            "i",
            "Tab",
            "y",
            "p",
            "r",
            "x",
            "d",
            "?",
            "q",
            "Esc",
            "drag",
            "Cmd+C",
            "Cmd/Ctrl+V",
        ] {
            assert!(keys.contains(&needed), "help documents {needed}");
        }
        for (k, what) in &entries {
            assert!(!k.is_empty() && !what.is_empty());
        }
        // Every status hint advertises `?`: empty, nav, and terminal.
        let mut view = test_shell();
        view.app.focus_nav();
        assert!(
            view.status_text().contains('?'),
            "empty hint advertises help: {}",
            view.status_text()
        );
        view.app.start_new_session();
        let _ = view.app.take_pending_spawn();
        view.app.focus_nav();
        assert!(
            view.status_text().contains('?'),
            "nav hint advertises help: {}",
            view.status_text()
        );
        view.app.focus_terminal();
        assert!(
            view.status_text().contains('?'),
            "terminal hint advertises help: {}",
            view.status_text()
        );
    }

    #[test]
    fn t_cycles_the_theme_choice() {
        // Issue #34: `t` dispatches CycleTheme without mutating (the
        // caller cycles, applies, and persists); the cycle itself starts
        // at the default system choice.
        use crate::config::ThemePreference;
        let mut view = test_shell();
        assert_eq!(view.nav_action("t", false), NavAction::CycleTheme);
        assert_eq!(view.app.cycle_theme(), ThemePreference::Dark);
        assert_eq!(view.app.cycle_theme(), ThemePreference::Light);
        assert_eq!(view.app.cycle_theme(), ThemePreference::System);
    }

    #[test]
    fn pageup_pagedown_page_the_run_list() {
        let mut view = test_shell();
        // No runs: paging is inert.
        assert_eq!(view.nav_action("pagedown", false), NavAction::None);
        assert_eq!(view.nav_action("pageup", false), NavAction::None);
        for _ in 0..8 {
            view.app.start_new_session();
            let _ = view.app.take_pending_spawn();
        }
        view.app.focus_nav();
        view.app.selected = 0;
        assert_eq!(view.nav_action("pagedown", false), NavAction::None);
        assert_eq!(view.app.selected, crate::app::PAGE_STEP);
        assert_eq!(view.nav_action("pagedown", false), NavAction::None);
        assert_eq!(view.app.selected, 7);
        assert_eq!(view.nav_action("pageup", false), NavAction::None);
        assert_eq!(view.app.selected, 7 - crate::app::PAGE_STEP);
        view.app.selected = 1;
        assert_eq!(view.nav_action("pageup", false), NavAction::None);
        assert_eq!(view.app.selected, 0);
    }

    #[test]
    fn o_cycles_link_focus_enter_opens_y_copies() {
        // Issue #49: Enter opens the focused link (OpenLink), `y`
        // copies it without opening (CopyLink); with no link focused
        // both fall back to the old behavior.
        let mut view = test_shell();
        view.app.start_new_session();
        let _ = view.app.take_pending_spawn();
        view.app.focus_nav();
        // No links: `o` stays unfocused, Enter focuses the terminal,
        // `y` copies the screen.
        assert_eq!(view.nav_action("o", false), NavAction::None);
        assert_eq!(view.link_cursor, None);
        assert_eq!(view.nav_action("enter", false), NavAction::FocusTerm);
        assert_eq!(view.nav_action("y", false), NavAction::Copy);

        let id = view.active_id().unwrap();
        let s = view.app.sessions.iter_mut().find(|s| s.id == id).unwrap();
        s.pr_links
            .push("https://github.com/acme/app/pull/42".to_string());
        s.pr_links
            .push("https://github.com/acme/app/pull/43".to_string());
        s.related_links
            .push("https://github.com/acme/app/issues/9".to_string());

        assert_eq!(view.nav_action("o", false), NavAction::None);
        assert_eq!(view.link_cursor, Some(0));
        assert!(view.focused_link_is_pr());
        assert_eq!(view.nav_action("enter", false), NavAction::OpenLink);
        assert_eq!(view.nav_action("y", false), NavAction::CopyLink);
        assert_eq!(
            view.focused_link_url().as_deref(),
            Some("https://github.com/acme/app/pull/42")
        );
        assert_eq!(view.nav_action("o", false), NavAction::None);
        assert_eq!(view.link_cursor, Some(1));
        assert_eq!(view.nav_action("enter", false), NavAction::OpenLink);
        // Link focus walks related links after the PRs, so every
        // openable row is keyboard-reachable.
        assert_eq!(view.nav_action("o", false), NavAction::None);
        assert_eq!(view.link_cursor, Some(2));
        assert!(!view.focused_link_is_pr());
        assert_eq!(
            view.focused_link_url().as_deref(),
            Some("https://github.com/acme/app/issues/9")
        );
        assert_eq!(view.nav_action("enter", false), NavAction::OpenLink);
        assert_eq!(view.nav_action("y", false), NavAction::CopyLink);
        // Past the last link focus wraps back to unfocused: Enter
        // types, `y` copies the screen again.
        assert_eq!(view.nav_action("o", false), NavAction::None);
        assert_eq!(view.link_cursor, None);
        assert_eq!(view.nav_action("enter", false), NavAction::FocusTerm);
        assert_eq!(view.nav_action("y", false), NavAction::Copy);
        // `i` always focuses the terminal, even with a link focused.
        assert_eq!(view.nav_action("o", false), NavAction::None);
        assert_eq!(view.link_cursor, Some(0));
        assert_eq!(view.nav_action("i", false), NavAction::FocusTerm);
        // Moving the run selection clears link focus.
        assert_eq!(view.nav_action("j", false), NavAction::None);
        assert_eq!(view.link_cursor, None);
    }

    #[test]
    fn browser_command_passes_the_exact_url_through() {
        // Issue #49 falsifiable check: activating a PR row opens that
        // exact `github.com/<owner>/<repo>/pull/<n>` URL. Construction
        // is pure, so this asserts the argv without launching anything.
        let url = "https://github.com/acme/app/pull/42";
        let cmd = super::ShellView::browser_command(url);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.contains(&url.to_string()),
            "exact URL in argv: {args:?}"
        );
        let program = cmd.get_program().to_string_lossy().into_owned();
        #[cfg(target_os = "macos")]
        assert_eq!(program, "open");
        #[cfg(all(unix, not(target_os = "macos")))]
        assert_eq!(program, "xdg-open");
    }
}
