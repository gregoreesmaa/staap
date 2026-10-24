//! Pump loop: polling PTYs, refreshing entries, gating repaints.
//!
//! The background task ticks at 20 Hz, but repaints are dirty-gated:
//! [`ShellView::tick`] returns true only when a spawn was consumed,
//! fresh output landed, a child exited, or a status/link/row change
//! happened, and the main loop notifies the window (plus re-sorts)
//! only then — idle costs ~zero. Text scans run only for changed runs;
//! recency statuses still refresh every tick from the clock, so Working
//! still decays to Idle with no output.

use std::time::Instant;

use crate::parsers::github::extract_pr_links;

use super::shell::ShellView;

impl ShellView {
    /// One pump iteration for the background task. Returns true when the
    /// pump observed anything visible — fresh output, a newly exited child,
    /// a consumed spawn, a status flip, a new link, or a moved row — so
    /// the caller repaints only then and idle costs ~zero instead of a full
    /// refresh + repaint at 20 Hz. Selection/focus moves are event-driven
    /// (they repaint directly), so the pump only tracks PTY-derived change.
    pub fn tick(&mut self) -> bool {
        self.refresh()
    }

    /// Pump every run, refresh each entry from its live screen (attention
    /// markers, working/idle by recency, links), re-sort pinned.
    /// Returns true when anything visible changed (see [`ShellView::tick`]).
    /// Text scans (attention regex + link extraction) run only for runs
    /// whose PTY delivered bytes or changed exit state since the last tick;
    /// unchanged screens reuse the cached attention bit, so an idle tick
    /// costs no screen allocs at all. Re-sorting runs on the same gate: row
    /// order only depends on status, so an unchanged pump leaves the order
    /// (and the selection index) untouched instead of re-sorting every tick.
    pub(crate) fn refresh(&mut self) -> bool {
        let spawned = self.spawn_queued();
        let mut fresh_any = false;
        let mut rescanned: Vec<String> = Vec::new();
        for (id, run) in self.runs.iter_mut() {
            let exited_before = run.exited();
            if run.pump() {
                fresh_any = true;
                rescanned.push(id.clone());
                // Fresh output snaps a paged run back to the live bottom
                // (issue #25): the offset points at history the new bytes
                // just pushed further down.
                run.scroll_offset = 0;
            } else if run.exited() != exited_before {
                // No new bytes, but the child just exited: the run's
                // status and links are stale, so rescan it as dirt.
                fresh_any = true;
                rescanned.push(id.clone());
            }
        }
        let now = Instant::now();
        let ids: Vec<String> = self.runs.keys().cloned().collect();
        // Selection for the attention flip detector: background flips ring,
        // the selected run is already on screen so its flips stay silent.
        let selected_id = self.active_id();
        let mut changed = spawned || fresh_any;
        // (run id, old status, new status) transitions observed this tick.
        let mut flips: Vec<(String, crate::app::Status, crate::app::Status)> = Vec::new();
        for id in ids {
            // Scope the run borrow: the merge below touches other fields.
            let (attention, exited, fresh_links) = {
                let run = &self.runs[&id];
                if rescanned.contains(&id) {
                    let text = run.pty.view().screen.contents();
                    let attention = crate::app::needs_attention(&text);
                    let pr = extract_pr_links(&text);
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
            // One classifier for live and historic runs alike (owned by
            // `app`): attention markers win, then exit, then recency.
            // No output observed yet reads Idle (issues #103/#105):
            // the same None-until-output rule as the core registry.
            let age = self
                .runs
                .get(&id)
                .and_then(|run| run.last_output.map(|t| now.duration_since(t)));
            let status = crate::app::classify_with_attention(attention, age, exited);
            // Accumulate links in first-seen order: the visible screen
            // is only a viewport (vt100 `contents()` shows the live grid,
            // not full scrollback), so replacing would drop links that
            // scrolled off. Merging keeps every link ever seen per run.
            if let Some(s) = self.app.sessions.iter_mut().find(|s| s.id == id) {
                if s.status != status {
                    flips.push((id.clone(), s.status, status));
                    s.status = status;
                    changed = true;
                }
                // Cap-not-drop merge (storage cap + truncation flag live
                // in `push_links`); the panel folds extras behind N more.
                // Gated on actual growth so a rescan with no new links
                // stays clean and never repaints.
                if let Some((pr, related)) = fresh_links {
                    let before = (s.pr_links.len(), s.related_links.len(), s.links_truncated);
                    s.push_links(pr, related);
                    if (s.pr_links.len(), s.related_links.len(), s.links_truncated) != before {
                        changed = true;
                    }
                }
            }
        }
        if changed {
            // Row order derives from status alone (last_active never moves
            // here), so a changed pump is exactly when rows could have
            // moved; a clean pump leaves order and selection index alone.
            self.app.resort_keep_selection();
            // Persist across restarts (issue #26), throttled so a
            // streaming run doesn't rewrite the file every 50 ms tick
            // (the first change always saves; quit/close save too).
            let due = self
                .last_persist
                .is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(5));
            if due {
                self.persist_runs();
            }
        }
        // Transition-triggered bells, after the borrows end: only the tick
        // that observes a background flip rings, so 20 Hz polling never
        // double-rings (issue #24).
        for (id, old, new) in flips {
            self.note_attention_flip(&id, old, new, selected_id.as_deref());
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::super::runs::{insert_test_pty, test_shell, Run};
    use super::*;
    use crate::app::Status;

    /// Plain-struct headless coverage: real PTYs, no window needed.
    fn view_with_echo() -> ShellView {
        let mut view = test_shell();
        view.app.start_new_session();
        let _ = view.app.take_pending_spawn();
        let id = view.active_id().unwrap();
        let pty = crate::embedded::EmbeddedPty::spawn("echo", &["hello-gui".to_string()], 80, 24)
            .unwrap();
        view.runs.insert(id, Run::new(pty));
        view
    }

    #[test]
    fn refresh_persists_run_state_without_touching_disk() {
        // Issue #26: a changed pump advances the persist clock (the file
        // write itself is production-only so tests never rewrite the
        // developer's live runs file; the roundtrip is covered in
        // `persist.rs` with temp paths).
        let mut view = view_with_echo();
        assert!(view.last_persist.is_none());
        for _ in 0..100 {
            view.refresh();
            if view.last_persist.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(view.last_persist.is_some());
    }

    #[test]
    fn refresh_tracks_live_output_and_exit() {
        let mut view = view_with_echo();
        view.refresh();
        let id = view.active_id().unwrap();
        let s = view.app.sessions.iter().find(|s| s.id == id).unwrap();
        assert!(s.pr_links.is_empty());
        // echo exits on its own; poll until the reaped exit flips it idle.
        for _ in 0..100 {
            view.refresh();
            let s = view.app.sessions.iter().find(|s| s.id == id).unwrap();
            if s.status == Status::Idle {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let s = view.app.sessions.iter().find(|s| s.id == id).unwrap();
        assert_eq!(s.status, Status::Idle);
    }

    #[test]
    fn tick_gates_repaint_on_dirtiness() {
        use std::time::Duration;

        // Clean: a live run with no output never requests a repaint, so
        // the 20 Hz pump idles instead of burning a refresh + repaint.
        let mut idle = test_shell();
        let idle_id = insert_test_pty(&mut idle, "sleep", &["5"]);
        let _ = idle_id;
        for _ in 0..5 {
            assert!(!idle.tick(), "idle pump must stay clean (no repaint)");
        }

        // Dirty: fresh PTY output requests a repaint.
        let mut live = test_shell();
        let live_id = insert_test_pty(&mut live, "printf", &["hello-dirty\\n"]);
        let mut saw_dirty = false;
        for _ in 0..100 {
            if live.tick() {
                saw_dirty = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(saw_dirty, "fresh PTY output must mark the pump dirty");

        // Settled: once output is drained and the run is idle, the pump
        // goes clean again (no per-tick repaint at 20 Hz).
        for _ in 0..100 {
            live.tick();
            let s = live.app.sessions.iter().find(|s| s.id == live_id).unwrap();
            if s.status == Status::Idle {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let s = live.app.sessions.iter().find(|s| s.id == live_id).unwrap();
        assert_eq!(s.status, Status::Idle);
        assert!(!live.tick(), "settled idle pump must stay clean");
        assert!(!live.tick(), "settled idle pump must stay clean");

        // Dirty without output: a child that exits silently still flips
        // the pump dirty once (the ended state needs its repaint).
        let mut quick = test_shell();
        insert_test_pty(&mut quick, "true", &[]);
        let mut saw_exit_dirty = false;
        for _ in 0..100 {
            if quick.tick() {
                saw_exit_dirty = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(saw_exit_dirty, "silent exit must mark the pump dirty");
    }

    #[test]
    fn refresh_extracts_pr_links_from_screen() {
        let mut view = test_shell();
        let id = insert_test_pty(
            &mut view,
            "printf",
            &["see https://github.com/acme/app/pull/42\\n"],
        );
        // Pump until the printf output lands, then refresh reads the screen.
        for _ in 0..100 {
            view.refresh();
            let s = view.app.sessions.iter().find(|s| s.id == id).unwrap();
            if !s.pr_links.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let s = view.app.sessions.iter().find(|s| s.id == id).unwrap();
        assert_eq!(
            s.pr_links,
            vec!["https://github.com/acme/app/pull/42".to_string()]
        );
    }

    #[test]
    fn attention_status_comes_from_screen_text() {
        let mut view = view_with_echo();
        view.refresh();
        // Fake an approval prompt on the screen via printf run.
        let id = view.active_id().unwrap();
        let pty = crate::embedded::EmbeddedPty::spawn(
            "printf",
            &["Waiting for your approval to proceed\\n".to_string()],
            80,
            24,
        )
        .unwrap();
        view.runs.insert(id.clone(), Run::new(pty));
        for _ in 0..100 {
            view.refresh();
            let s = view.app.sessions.iter().find(|s| s.id == id).unwrap();
            if s.status == Status::Attention {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let s = view.app.sessions.iter().find(|s| s.id == id).unwrap();
        assert_eq!(s.status, Status::Attention);
    }

    #[test]
    fn unchanged_screens_reuse_cached_attention() {
        let mut view = test_shell();
        let id = insert_test_pty(
            &mut view,
            "printf",
            &["Waiting for your approval to proceed\\n"],
        );
        for _ in 0..100 {
            view.refresh();
            let s = view.app.sessions.iter().find(|s| s.id == id).unwrap();
            if s.status == Status::Attention {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // The scan populated the cache; the now-static screen keeps its
        // Attention status across refreshes via the cached bit alone.
        assert!(view.runs[&id].attention);
        for _ in 0..10 {
            view.refresh();
        }
        let s = view.app.sessions.iter().find(|s| s.id == id).unwrap();
        assert_eq!(s.status, Status::Attention);
        assert!(view.runs[&id].attention);
    }

    #[test]
    fn exit_transition_counts_as_dirt_without_output_bytes() {
        let mut view = test_shell();
        let id = insert_test_pty(&mut view, "true", &[]);
        let mut saw_dirt = false;
        for _ in 0..100 {
            if view.refresh() {
                saw_dirt = true;
            }
            if view.runs[&id].exited() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(view.runs[&id].exited());
        assert!(saw_dirt, "the exit tick must report dirt");
    }

    #[test]
    fn issue_and_file_refs_accumulate_like_pr_links() {
        let mut view = test_shell();
        let id = insert_test_pty(
            &mut view,
            "printf",
            &["see https://github.com/acme/app/issues/7 at src/app.rs:9\\n"],
        );
        for _ in 0..100 {
            view.refresh();
            let s = view.app.sessions.iter().find(|s| s.id == id).unwrap();
            if !s.related_links.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let s = view.app.sessions.iter().find(|s| s.id == id).unwrap();
        assert_eq!(
            s.related_links,
            vec![
                "https://github.com/acme/app/issues/7".to_string(),
                "src/app.rs:9".to_string()
            ]
        );
    }

    #[test]
    fn pr_links_accumulate_first_seen_order_without_cap() {
        let mut view = test_shell();
        let id = insert_test_pty(
            &mut view,
            "printf",
            &["see https://github.com/acme/app/pull/42\\n"],
        );
        for _ in 0..100 {
            view.refresh();
            let s = view.app.sessions.iter().find(|s| s.id == id).unwrap();
            if !s.pr_links.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // A later screen showing a new PR keeps the old one (scrolled-off
        // links are not dropped) and shows every link, uncapped.
        {
            let s = view.app.sessions.iter_mut().find(|s| s.id == id).unwrap();
            assert_eq!(s.pr_links, vec!["https://github.com/acme/app/pull/42"]);
        }
        let pty2 = crate::embedded::EmbeddedPty::spawn(
            "printf",
            &["now https://github.com/acme/app/pull/43 and https://github.com/acme/app/pull/42\\n"
                .to_string()],
            80,
            24,
        )
        .unwrap();
        view.runs.insert(id.clone(), Run::new(pty2));
        for _ in 0..100 {
            view.refresh();
            let s = view.app.sessions.iter().find(|s| s.id == id).unwrap();
            if s.pr_links.len() == 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let s = view.app.sessions.iter().find(|s| s.id == id).unwrap();
        assert_eq!(
            s.pr_links,
            vec![
                "https://github.com/acme/app/pull/42".to_string(),
                "https://github.com/acme/app/pull/43".to_string()
            ]
        );
    }

    #[test]
    fn same_named_live_runs_operate_independently() {
        // Issue #54 falsifiable: two same-titled rows are independent
        // working rows — pump output, typed input, and close all route
        // by the UUID row key, never by title.
        let mut view = test_shell();
        let first = insert_test_pty(
            &mut view,
            "printf",
            &["see https://github.com/acme/app/pull/41\\n"],
        );
        let second = insert_test_pty(&mut view, "sleep", &["5"]);
        assert_ne!(first, second);
        // Force the reported collision: identical titles, distinct ids.
        for s in view.app.sessions.iter_mut() {
            s.title = "otter".into();
        }
        // Pump until the printf output lands — on the first run only.
        for _ in 0..100 {
            view.refresh();
            let s = view.app.sessions.iter().find(|s| s.id == first).unwrap();
            if !s.pr_links.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let a = view.app.sessions.iter().find(|s| s.id == first).unwrap();
        assert_eq!(
            a.pr_links,
            vec!["https://github.com/acme/app/pull/41".to_string()]
        );
        let b = view.app.sessions.iter().find(|s| s.id == second).unwrap();
        assert!(
            b.pr_links.is_empty(),
            "links leaked across same-named rows: {:?}",
            b.pr_links
        );
        assert_eq!(b.title, "otter");
        // Typed input reaches only the selected row: select the first
        // and type — the same-named neighbor keeps its title.
        view.app.selected = view
            .app
            .sessions
            .iter()
            .position(|s| s.id == first)
            .unwrap();
        view.app.focus_terminal();
        for c in ["f", "i", "x"] {
            view.forward_key(c, Some(c), false, false);
        }
        view.forward_key("enter", None, false, false);
        assert_eq!(
            view.app
                .sessions
                .iter()
                .find(|s| s.id == first)
                .unwrap()
                .title,
            "fix"
        );
        assert_eq!(
            view.app
                .sessions
                .iter()
                .find(|s| s.id == second)
                .unwrap()
                .title,
            "otter"
        );
        // Closing the selected row drops exactly its entry + PTY; the
        // neighbor keeps working.
        view.close_run();
        assert_eq!(view.app.sessions.len(), 1);
        assert_eq!(view.app.sessions[0].id, second);
        assert!(!view.runs.contains_key(&first));
        assert!(view.runs.contains_key(&second));
    }
}
