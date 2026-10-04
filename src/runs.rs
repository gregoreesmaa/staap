//! Core run registry: live PTYs keyed by roster row id.
//!
//! The gpui shell keeps this state in `gui::runs::Run` + `ShellView.runs`;
//! the three native shells each kept their own `live` map (`LivePty`,
//! `ptys`) with three different eviction/attach policies. This module is
//! the single framework-free registry behind the FFI run functions
//! (`staap_run_*`): spawn attaches a live PTY to a roster row, the pump
//! refreshes statuses/links/recency like `gui::pump::refresh`, and shells
//! only render what the getters report.
//!
//! Design: `RunRegistry` owns `App` (roster + config) plus one
//! `LiveRun` per attached row. Native shells drive it through `ffi`
//! (`StaapCore` wraps this type); the gpui shell keeps its own `ShellView`
//! (which predates this registry and owns extra view state like pager
//! offsets — no behavior change there).

use std::collections::HashMap;
use std::time::Instant;

use crate::app::{classify_with_attention, needs_attention, App, Status};
use crate::embedded::{EmbeddedPty, SpawnKind};
use crate::shell_shared::MAX_LIVE_RUNS;

/// One live run: its PTY, output recency, and the cached attention bit
/// (unchanged screens skip re-scanning — same rule as `gui::runs::Run`).
pub struct LiveRun {
    pub pty: EmbeddedPty,
    pub last_output: Instant,
    pub attention: bool,
}

impl LiveRun {
    pub fn new(pty: EmbeddedPty) -> Self {
        Self {
            pty,
            last_output: Instant::now(),
            attention: false,
        }
    }

    /// Feed queued output into the emulator, refreshing output recency.
    /// Returns true when new output arrived or the child newly exited.
    pub fn pump(&mut self) -> bool {
        let exited_before = self.pty.view().exited;
        if self.pty.pump() {
            self.last_output = Instant::now();
            true
        } else {
            self.pty.view().exited != exited_before
        }
    }

    pub fn exited(&self) -> bool {
        self.pty.view().exited
    }
}

/// Oldest-exited live run: least-recently-active among exited children.
/// `None` when every live run is still running (refuse, don't reap).
pub fn oldest_exited_id(runs: &HashMap<String, LiveRun>) -> Option<String> {
    runs.iter()
        .filter(|(_, run)| run.exited())
        .min_by(|(_, a), (_, b)| a.last_output.cmp(&b.last_output))
        .map(|(id, _)| id.clone())
}

/// Outcome of a registry spawn request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnOutcome {
    /// A PTY was attached to this row id.
    Spawned { id: String },
    /// At the cap: the named exited run was reaped first (flash it).
    Reaped { id: String, reaped_title: String },
    /// At the cap with every run live: refused, entry kept, error set.
    Refused,
}

/// The core run registry: roster + config (`App`) plus live PTYs.
pub struct RunRegistry {
    pub app: App,
    pub runs: HashMap<String, LiveRun>,
}

impl RunRegistry {
    pub fn new(sessions: Vec<crate::app::ChatSession>) -> Self {
        Self {
            app: App::new(sessions),
            runs: HashMap::new(),
        }
    }

    /// Roster row ids in core order.
    pub fn row_ids(&self) -> Vec<String> {
        self.app.sessions.iter().map(|s| s.id.clone()).collect()
    }

    pub fn is_live(&self, id: &str) -> bool {
        self.runs.contains_key(id)
    }

    /// True when the row is historic: no live PTY attached. Attached =
    /// active, even when the child already exited (active until closed);
    /// detached rows (discovered sessions, closed runs, unknown ids) are
    /// historic. This is the single membership rule every shell renders.
    pub fn is_history(&self, id: &str) -> bool {
        !self.is_live(id)
    }

    pub fn live_count(&self) -> usize {
        self.runs.len()
    }

    /// Attach a spawned PTY to a roster row (records output recency,
    /// clears any sticky error — same success rule as
    /// `gui::spawn::note_spawn_success`).
    pub fn attach(&mut self, id: &str, pty: EmbeddedPty) {
        let mut run = LiveRun::new(pty);
        run.last_output = Instant::now();
        self.runs.insert(id.to_string(), run);
        self.app.clear_error();
    }

    /// Ensure room for one more live PTY under the cap: reap the
    /// oldest-exited run first. Returns the reaped title, or `None` when
    /// every live run is still running (caller refuses instead).
    pub fn make_room(&mut self) -> Option<String> {
        if self.runs.len() < MAX_LIVE_RUNS {
            return Some(String::new());
        }
        let victim = oldest_exited_id(&self.runs)?;
        self.runs.remove(&victim);
        self.app.remove_session(&victim)
    }

    /// Close (kill) a run: drop its live PTY — `Drop` reaps the child —
    /// and remove its entry. Returns the removed title, if any.
    pub fn close(&mut self, id: &str) -> Option<String> {
        self.runs.remove(id);
        self.app.remove_session(id)
    }

    /// Restart an ended run (or resume a historic entry): drop the dead
    /// PTY if any, clear per-run transient state, and report the spawn
    /// kind the shell should launch on the same id. `None` when the run
    /// is live (never kill by accident) or unknown.
    pub fn restart_kind(&mut self, id: &str) -> Option<SpawnKind> {
        if !self.app.sessions.iter().any(|s| s.id == id) {
            return None;
        }
        let resumable = !self.runs.contains_key(id);
        let restartable = self.runs.get(id).is_some_and(|r| r.exited());
        if !resumable && !restartable {
            return None;
        }
        self.runs.remove(id);
        if let Some(s) = self.app.sessions.iter_mut().find(|s| s.id == id) {
            s.pending_input.clear();
        }
        Some(self.app.respawn_kind(id))
    }

    /// True when quitting deserves a confirmation step: any
    /// Working/Attention row or any live (non-exited) PTY.
    pub fn needs_quit_confirm(&self) -> bool {
        self.app.needs_quit_confirm() || self.runs.values().any(|r| !r.exited())
    }

    /// One pump iteration over every live run: feed output, rescan
    /// attention + links for changed runs, reclassify statuses, re-sort
    /// pinned to selection. Returns true when anything visible changed
    /// (fresh output, new exit, status flip, new link) — the shell's only
    /// repaint gate. Mirrors `gui::pump::refresh` minus view-only state
    /// (pager offsets, bells).
    pub fn pump_all(&mut self) -> bool {
        let mut rescanned: Vec<String> = Vec::new();
        let mut fresh_any = false;
        for (id, run) in self.runs.iter_mut() {
            if run.pump() {
                fresh_any = true;
                rescanned.push(id.clone());
            }
        }
        let mut changed = fresh_any;
        let now = Instant::now();
        for id in self.runs.keys().cloned().collect::<Vec<_>>() {
            let (attention, exited, fresh_links) = {
                let run = &self.runs[&id];
                if rescanned.contains(&id) {
                    let text = run.pty.view().screen.contents();
                    let attention = needs_attention(&text);
                    let pr = crate::parsers::pr_links(&text);
                    let related = crate::parsers::related_links(&text);
                    (attention, run.exited(), Some((pr, related)))
                } else {
                    (run.attention, run.exited(), None)
                }
            };
            if rescanned.contains(&id) {
                if let Some(run) = self.runs.get_mut(&id) {
                    run.attention = attention;
                }
            }
            let age = self
                .runs
                .get(&id)
                .map(|run| now.duration_since(run.last_output));
            let status = classify_with_attention(attention, age, exited);
            if let Some(s) = self.app.sessions.iter_mut().find(|s| s.id == id) {
                if s.status != status {
                    s.status = status;
                    changed = true;
                }
                if let Some((pr, related)) = fresh_links {
                    let before = (s.pr_links.len(), s.related_links.len(), s.links_truncated);
                    s.push_links(pr, related);
                    if (s.pr_links.len(), s.related_links.len(), s.links_truncated) != before {
                        changed = true;
                    }
                }
            }
        }
        // Live statuses still decay with the clock: a Working run with no
        // fresh output goes Idle once its recency leaves the window, even
        // on a clean pump (same rule as the gpui shell).
        let mut decayed = false;
        let now2 = Instant::now();
        for s in self.app.sessions.iter_mut() {
            if !self.runs.contains_key(&s.id) {
                continue;
            }
            let run = &self.runs[&s.id];
            let age = now2.duration_since(run.last_output);
            let status = classify_with_attention(run.attention, Some(age), run.exited());
            if s.status != status {
                s.status = status;
                decayed = true;
            }
        }
        if changed || decayed {
            self.app.resort_keep_selection();
            return true;
        }
        // Historic rows never change here (no live PTY), so no re-sort.
        let _ = Status::Idle;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ChatSession, HARNESS_MUSE};

    fn sess(id: &str, status: Status) -> ChatSession {
        ChatSession {
            id: id.into(),
            title: id.into(),
            project: "proj".into(),
            status,
            harness: HARNESS_MUSE.into(),
            last_active: 1,
            pr_links: vec![],
            related_links: vec![],
            links_truncated: false,
            transcript: vec![],
            transcript_truncated: false,
            title_locked: true,
            pending_input: String::new(),
            provider_session_id: None,
            cwd: None,
        }
    }

    fn live_run(program: &str, args: &[&str]) -> LiveRun {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        LiveRun::new(EmbeddedPty::spawn(program, &owned, 80, 24).unwrap())
    }

    #[test]
    fn registry_pump_tracks_output_and_exit() {
        let mut reg = RunRegistry::new(vec![sess("a", Status::Working)]);
        reg.attach("a", live_run("echo", &["hello-reg"]).pty);
        let mut saw_dirty = false;
        for _ in 0..100 {
            if reg.pump_all() {
                saw_dirty = true;
            }
            if reg.app.sessions[0].status == Status::Idle {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(saw_dirty);
        // echo exits on its own: the run settles Idle.
        assert_eq!(reg.app.sessions[0].status, Status::Idle);
    }

    #[test]
    fn registry_pump_extracts_links_and_attention() {
        let mut reg = RunRegistry::new(vec![sess("a", Status::Working)]);
        reg.attach(
            "a",
            live_run(
                "printf",
                &["see https://github.com/acme/app/pull/42 approval needed\\n"],
            )
            .pty,
        );
        for _ in 0..100 {
            reg.pump_all();
            let s = &reg.app.sessions[0];
            if !s.pr_links.is_empty() && s.status == Status::Attention {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let s = &reg.app.sessions[0];
        assert_eq!(
            s.pr_links,
            vec!["https://github.com/acme/app/pull/42".to_string()]
        );
        assert_eq!(s.status, Status::Attention);
    }

    #[test]
    fn registry_close_drops_entry_and_pty() {
        let mut reg = RunRegistry::new(vec![sess("a", Status::Idle), sess("b", Status::Idle)]);
        reg.attach("b", live_run("sleep", &["5"]).pty);
        assert_eq!(reg.close("b").as_deref(), Some("b"));
        assert_eq!(reg.app.sessions.len(), 1);
        assert!(!reg.is_live("b"));
        assert_eq!(reg.close("missing"), None);
    }

    #[test]
    fn registry_restart_offers_only_ended_or_historic() {
        let mut reg = RunRegistry::new(vec![sess("a", Status::Idle)]);
        // Historic (no PTY): resume offered.
        assert!(reg.restart_kind("a").is_some());
        // Live child: never offered.
        reg.attach("a", live_run("sleep", &["5"]).pty);
        assert_eq!(reg.restart_kind("a"), None);
        // Unknown id: nothing.
        assert_eq!(reg.restart_kind("zzz"), None);
    }

    #[test]
    fn registry_quit_confirm_covers_rows_and_ptys() {
        let mut reg = RunRegistry::new(vec![sess("a", Status::Idle)]);
        assert!(!reg.needs_quit_confirm());
        reg.app.sessions[0].status = Status::Working;
        assert!(reg.needs_quit_confirm());
        reg.app.sessions[0].status = Status::Idle;
        reg.attach("a", live_run("sleep", &["5"]).pty);
        assert!(reg.needs_quit_confirm());
    }

    #[test]
    fn history_means_no_live_pty_attached() {
        let mut reg = RunRegistry::new(vec![sess("a", Status::Idle), sess("b", Status::Idle)]);
        // Detached rows are historic.
        assert!(reg.is_history("a"));
        assert!(reg.is_history("b"));
        // Attached rows are active.
        reg.attach("a", live_run("sleep", &["5"]).pty);
        assert!(!reg.is_history("a"));
        assert!(reg.is_history("b"));
        // Closing drops the PTY: the entry is gone (close removes it).
        reg.close("a");
        assert!(reg.is_history("a"));
        // Unknown ids own no PTY: historic by definition.
        assert!(reg.is_history("zzz"));
    }

    #[test]
    fn make_room_reaps_oldest_exited_first() {
        let mut sessions = Vec::new();
        for i in 0..MAX_LIVE_RUNS {
            sessions.push(sess(&format!("r{i}"), Status::Idle));
        }
        let mut reg = RunRegistry::new(sessions);
        for i in 0..MAX_LIVE_RUNS {
            let id = format!("r{i}");
            reg.attach(&id.clone(), live_run("true", &[]).pty);
        }
        for _ in 0..200 {
            reg.pump_all();
            if reg.runs.values().all(|r| r.exited()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(reg.runs.values().all(|r| r.exited()));
        let before = reg.app.sessions.len();
        let title = reg.make_room().expect("an exited run is reaped");
        assert!(!title.is_empty());
        assert_eq!(reg.live_count(), MAX_LIVE_RUNS - 1);
        assert_eq!(reg.app.sessions.len(), before - 1);
    }
}
