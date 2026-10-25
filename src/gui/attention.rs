//! Background attention signal: needs-input count + bell.
//!
//! The status line — the sidebar footer in wide mode, the slim bar under
//! the terminal in narrow mode (issue #32) — carries the count of runs in
//! [`crate::app::Status::Attention`], and a non-selected run flipping to
//! Attention rings the terminal bell once (issue #24). The flip detector
//! is transition-triggered, so the 20 Hz pump and per-frame refreshes can
//! both call it without double-ringing: only the tick that observes the
//! flip counts. A scrolled-up pager position rides the same line (issue
//! #25) so it stays visible without its own chrome.

use crate::app::Status;

use super::layout::NARROW_BREAKPOINT;
use super::shell::ShellView;

/// Audible bell on the launching terminal (best effort: no terminal, no
/// sound, no error). Gated by `bell_enabled` so headless tests stay quiet.
pub(crate) fn ring_bell() {
    use std::io::Write as _;
    let _ = write!(std::io::stdout(), "\x07");
    let _ = std::io::stdout().flush();
}

impl ShellView {
    /// Runs currently needing input (any selection). The badge derives
    /// from live status every frame, so it clears itself as runs settle.
    pub(crate) fn attention_count(&self) -> usize {
        crate::app::attention_count(&self.app.sessions)
    }

    /// Suffix carrying the badge, or empty when nothing needs input.
    fn attention_suffix(&self) -> String {
        let n = self.attention_count();
        if n == 0 {
            String::new()
        } else {
            format!(" · {n} need input")
        }
    }

    /// Transition-triggered bell: call with a run's status before/after a
    /// pump merge. Rings (and counts) exactly when a *background* run —
    /// anything but the selected one — newly needs input. The selected
    /// run is already on screen, so its flips stay silent.
    pub(crate) fn note_attention_flip(
        &mut self,
        run_id: &str,
        old: Status,
        new: Status,
        selected_id: Option<&str>,
    ) {
        if old == Status::Attention || new != Status::Attention {
            return;
        }
        if Some(run_id) == selected_id {
            return;
        }
        self.bells_rung += 1;
        if self.bell_enabled {
            ring_bell();
        }
    }

    /// Wide (default) status hints. Test-only shorthand: production render
    /// always goes through [`Self::status_text_for_width`] with the live
    /// viewport width.
    #[cfg(test)]
    pub(crate) fn status_text(&self) -> String {
        self.status_text_for_width(f32::INFINITY)
    }

    /// Width-aware status text with the needs-input badge and the pager
    /// position. An armed quit outranks the badge (the user asked to
    /// leave) but not the pager position; otherwise both ride along so a
    /// background approval is visible without switching runs. Moved here
    /// from `shell` so the shell stays under its line budget as comfort
    /// features land.
    pub(crate) fn status_text_for_width(&self, viewport_w: f32) -> String {
        let base = self.base_status_text_for_width(viewport_w);
        let pager = self.pager_note();
        if self.quit_armed {
            return format!("{base}{pager}");
        }
        let suffix = self.attention_suffix();
        format!("{base}{suffix}{pager}")
    }

    /// Width-aware status text: narrow viewports (<700px) get compact key
    /// hints that fit beside the collapsed layout; errors, transient
    /// flashes, quit-arm, and ended-run lines are identical at every width
    /// (only the default key-hint lines compact — the bar also truncates
    /// with an ellipsis, so long messages never push the layout).
    fn base_status_text_for_width(&self, viewport_w: f32) -> String {
        // 2D-launch picker capture outranks everything while it owns the
        // keyboard: the folder input, the CLI axis, yolo, and the exits.
        if self.picker_open() {
            let picker = self.launch_picker.as_ref().expect("picker open");
            let cli = picker.selected_cli();
            let host = self.picker_remote.host_input.trim().to_string();
            let remote = !host.is_empty();
            // Remote rows probe the host in the background: name the
            // target and show `probing` until the catalog lands, so the
            // meanwhile state is visible instead of stale `ready` rows.
            let cli_state = if remote && self.picker_remote.in_flight.is_some() {
                "probing"
            } else if cli.available {
                "ready"
            } else {
                "not installed"
            };
            let folder = if picker.folder_input.is_empty() {
                "(blank = current)".to_string()
            } else {
                picker.folder_input.clone()
            };
            // Machine-field placeholder hint (SSH picker): an empty host
            // axis names the `user@hostname` shape plus a concrete
            // example, so the field never looks like free prose.
            let host_part = if remote {
                format!(" · ssh {host}")
            } else if self.picker_remote.host_focus {
                format!(" · machine: {}", super::picker::MACHINE_PLACEHOLDER)
            } else {
                String::new()
            };
            return format!(
                "new run: {folder}{host_part} · {} ({cli_state}) · yolo: {} · Tab: axis · Enter: start · Esc: cancel · {}",
                cli.id,
                self.picker_yolo_label(),
                self.picker_preview(),
            );
        }
        // Folder-picker capture (issue #48) outranks everything while
        // it owns the keyboard: the typed path plus its two exits.
        if let Some(buf) = self.cwd_capture.as_deref() {
            if buf.is_empty() {
                return "new session folder: (blank = current) · Enter: start · Esc: cancel"
                    .to_string();
            }
            return format!("new session folder: {buf} · Enter: start · Esc: cancel");
        }
        // An armed quit outranks everything: the user asked to leave.
        if self.quit_armed {
            return "Live runs active — q again to quit · any other key cancels".to_string();
        }
        if let Some(msg) = self.app.status_text() {
            // Sticky errors keep their recovery hint while Retry applies.
            if self.app.error_text().is_some() && self.can_retry() {
                return format!("{msg} · r: retry");
            }
            return msg.to_string();
        }
        if self.can_restart() {
            return "run ended · r: restart · n: new · ?: help · q: quit".to_string();
        }
        if self.can_resume() {
            // Historic provider entries re-attach (`r: resume`); runs that
            // never started offer a plain start.
            let historic = self
                .active_id()
                .as_ref()
                .and_then(|id| {
                    self.app
                        .sessions
                        .iter()
                        .find(|s| &s.id == id)
                        .and_then(|s| s.provider_session_id.clone())
                })
                .is_some();
            if historic {
                return "historic run · r: resume · n: new · ?: help · q: quit".to_string();
            }
            return "run ready · r: start · n: new · ?: help · q: quit".to_string();
        }
        let narrow = viewport_w < NARROW_BREAKPOINT;
        // Focus tag (issue #56): every default hint line names the
        // keyboard owner in words, so focus never depends on color
        // alone — in the sidebar footer (wide) and the slim bar
        // (narrow) alike.
        let tag = self.focus_indicator();
        if self.app.is_terminal_focused() {
            if narrow {
                format!("{tag} · typing · Tab/Esc: sessions · Cmd+C: copy · Cmd+V: paste · ?: help")
            } else {
                format!("{tag} · typing in muse · Tab/Esc: sessions · drag: select · Cmd+C: copy · Cmd/Ctrl+V: paste · ?: help")
            }
        } else if self.app.sessions.is_empty() {
            format!("{tag} · n: new · N: picker · ?: help · q: quit")
        } else if narrow {
            format!("{tag} · n: new · N: picker · j/k: move · o/Enter: link · Tab: type · x: close · y/p: copy/paste · t: theme · ?: help · q: quit")
        } else {
            format!("{tag} · n: new · N: picker · w: folder · j/k: move · PgUp/PgDn: page · o/Enter: open link · Tab/i: type · x: close · drag: select · y: copy · p: paste · t: theme · ?: help · q: quit")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::runs::test_shell;
    use super::*;

    fn attention_shell() -> ShellView {
        let mut view = test_shell();
        view.app.start_new_session();
        let _ = view.app.take_pending_spawn();
        // Newest run is selected; index 0 is the background run.
        view.app.start_new_session();
        let _ = view.app.take_pending_spawn();
        view.app.focus_nav();
        view
    }

    #[test]
    fn background_flip_rings_once_selected_flip_stays_silent() {
        let mut view = attention_shell();
        view.bell_enabled = false;
        let bg = view.app.sessions[0].id.clone();
        let sel = view.app.selected_session().unwrap().id.clone();
        assert_ne!(bg, sel);
        // Background run flips to Attention: counted.
        view.note_attention_flip(&bg, Status::Idle, Status::Attention, Some(&sel));
        assert_eq!(view.bells_rung, 1);
        // Same flip again: not a transition, silent.
        view.note_attention_flip(&bg, Status::Attention, Status::Attention, Some(&sel));
        assert_eq!(view.bells_rung, 1);
        // Selected run flips: silent (already on screen).
        view.note_attention_flip(&sel, Status::Idle, Status::Attention, Some(&sel));
        assert_eq!(view.bells_rung, 1);
        // Non-attention landing: silent.
        view.note_attention_flip(&bg, Status::Working, Status::Idle, Some(&sel));
        assert_eq!(view.bells_rung, 1);
    }

    #[test]
    fn badge_counts_attention_in_the_status_line() {
        // The badge rides the status line (sidebar footer in wide mode,
        // slim bar in narrow mode) at every viewport width.
        let mut view = attention_shell();
        assert_eq!(view.attention_count(), 0);
        assert!(!view.status_text().contains("need input"));
        view.app.sessions[0].status = Status::Attention;
        assert_eq!(view.attention_count(), 1);
        assert!(view.status_text().contains("1 need input"));
        assert!(view.status_text_for_width(1280.0).contains("1 need input"));
        assert!(view.status_text_for_width(600.0).contains("1 need input"));
        // Armed quit still outranks the badge (but not the pager note).
        view.quit_armed = true;
        assert!(!view.status_text_for_width(1280.0).contains("need input"));
    }

    #[test]
    fn badge_clears_when_attention_settles() {
        let mut view = attention_shell();
        view.app.sessions[0].status = Status::Attention;
        assert!(view.status_text().contains("1 need input"));
        view.app.sessions[0].status = Status::Idle;
        assert_eq!(view.attention_count(), 0);
        assert!(!view.status_text().contains("need input"));
    }
}
