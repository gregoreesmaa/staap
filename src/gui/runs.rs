//! Live-run container: one [`Run`] per entry.
//!
//! The shell used to keep three parallel maps (`ptys`, `last_output`,
//! `pending_inputs`) plus an attention cache, all keyed by run id and
//! able to desync. A single `Run` struct per id removes that bug class:
//! spawning, pumping, keying, closing, and restarting all move one entry.

use std::collections::HashMap;
use std::time::Instant;

use crate::embedded::EmbeddedPty;

/// Live-run cap (issue #31): at most this many PTYs exist at once.
/// Product pick (per ROADMAP): the 11th run reaps the oldest-exited run
/// first; when every live run is still running the 11th is refused and
/// the user closes one explicitly (`x`, issues #4/#23).
pub const MAX_LIVE_RUNS: usize = 10;

/// Oldest-exited live run: least-recently-active among exited children.
/// Output recency is the closest proxy for exit age (output stops at
/// exit), so the stalest exited run is the one untouched longest.
/// `None` when every live run is still running (refuse, don't reap a
/// live child).
pub(crate) fn oldest_exited_id(runs: &HashMap<String, Run>) -> Option<String> {
    runs.iter()
        .filter(|(_, run)| run.exited())
        .min_by_key(|(_, run)| run.last_output)
        .map(|(id, _)| id.clone())
}

/// One live run: its PTY, output recency, and the cached attention bit
/// (unchanged screens skip re-scanning). The pending input line lives on
/// the [`crate::app::ChatSession`] instead: title tracking runs even
/// before the PTY exists (fast typing into a queued spawn) or after it
/// dies, so key handling never depends on the child being alive.
pub struct Run {
    pub pty: EmbeddedPty,
    /// When the child last produced output (None until the first pump
    /// observes any: a fresh run reads Idle, not Working - issues
    /// #103/#105, same rule as [crate::runs::LiveRun]).
    pub last_output: Option<Instant>,
    pub attention: bool,
    /// Pager offset in lines up from the live bottom (issue #25): 0 is
    /// the live screen, positive shows retained history. Fresh output
    /// snaps it back to 0 (see the pump); paging never touches the PTY.
    pub scroll_offset: usize,
}

impl Run {
    pub fn new(pty: EmbeddedPty) -> Self {
        Self {
            pty,
            last_output: None,
            attention: false,
            scroll_offset: 0,
        }
    }

    /// Feed queued output into the emulator, refreshing output recency.
    /// Returns true when new output arrived.
    pub fn pump(&mut self) -> bool {
        if self.pty.pump() {
            self.last_output = Some(Instant::now());
            true
        } else {
            false
        }
    }

    /// True once the child has exited (last frame stays visible).
    pub fn exited(&self) -> bool {
        self.pty.view().exited
    }
}

#[cfg(test)]
pub(crate) fn test_shell() -> super::shell::ShellView {
    super::shell::ShellView::new()
}

/// Start a run in `view` backed by a real child process, returning its id.
/// Headless tests use tiny fake commands (`echo`, `printf`, `sleep`,
/// `true`) so they never need a real `muse`.
#[cfg(test)]
pub(crate) fn insert_test_pty(
    view: &mut super::shell::ShellView,
    program: &str,
    args: &[&str],
) -> String {
    view.app.start_new_session();
    let _ = view.app.take_pending_spawn();
    let id = view.active_id().unwrap();
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let pty = EmbeddedPty::spawn(program, &owned, 80, 24).unwrap();
    view.runs.insert(id.clone(), Run::new(pty));
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_pump_refreshes_recency_and_reports_freshness() {
        let mut run = Run::new(EmbeddedPty::spawn("echo", &["hi".to_string()], 80, 24).unwrap());
        assert_eq!(run.last_output, None);
        std::thread::sleep(std::time::Duration::from_millis(50));
        // echo writes promptly: the pump sees bytes and moves recency.
        let mut saw_fresh = false;
        for _ in 0..100 {
            if run.pump() {
                saw_fresh = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(saw_fresh);
        assert!(run.last_output.is_some());
        assert!(!run.attention);
    }

    #[test]
    fn run_detects_exit() {
        let mut run = Run::new(EmbeddedPty::spawn("true", &[], 80, 24).unwrap());
        for _ in 0..100 {
            run.pump();
            if run.exited() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(run.exited());
    }

    #[test]
    fn oldest_exited_id_ignores_live_runs_and_picks_stalest() {
        use std::time::{Duration, Instant};

        let mut runs: HashMap<String, Run> = HashMap::new();
        // A live run, fresher than everything: never a reap victim.
        let mut live = Run::new(EmbeddedPty::spawn("sleep", &["5".to_string()], 80, 24).unwrap());
        live.last_output = Some(Instant::now());
        runs.insert("live".to_string(), live);
        for id in ["old", "new"] {
            let mut run = Run::new(EmbeddedPty::spawn("true", &[], 80, 24).unwrap());
            for _ in 0..100 {
                run.pump();
                if run.exited() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(run.exited());
            runs.insert(id.to_string(), run);
        }
        runs.get_mut("old").unwrap().last_output =
            Some(Instant::now().checked_sub(Duration::from_secs(60)).unwrap());
        runs.get_mut("new").unwrap().last_output = Some(Instant::now());
        assert_eq!(oldest_exited_id(&runs).as_deref(), Some("old"));
        runs.remove("old");
        runs.remove("new");
        assert_eq!(oldest_exited_id(&runs), None);
    }
}
