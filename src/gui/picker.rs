//! Two-dimensional new-session picker (folder × CLI + yolo) for the gpui shell.
//!
//! Split-button model: `n` / the `+ New` main click repeats the last launch
//! instantly (expert fast path, 1 keystroke, unchanged); `N` / the `▾` caret
//! / the empty-pane picker button opens this capture, where the folder axis
//! and the CLI axis are picked independently before Enter confirms.
//!
//! Remote rows add a host axis on top: typing a host (Tab reaches it from
//! the folder axis) retargets the pick at an SSH host. Committing the host
//! swaps the CLI catalog to the unchecked shape meanwhile and probes the
//! host with [`crate::launch::detect_remote_clis`] off the UI thread; the
//! arrival swaps the real catalog in (keeping the selected CLI). Confirming
//! a remote row skips the local folder check (a remote dir never exists
//! here by construction) and funnels through the shared `App::start_launch`
//! remote path, so the local PTY runs `ssh -t <host> <cli…>`.
//!
//! Thin by contract: the keyboard state machine lives in the framework-free
//! `crate::launch::LaunchPicker` (tested headlessly on every OS gate); this
//! module only opens/accepts/cancels it, carries the host input the core
//! must never own, and renders the preview/status copy the shell shows.
//! Rendering (a follow-up over `runs_panel.rs`) reads the same state
//! natively.

use std::sync::mpsc;

use crate::launch::{AvailableCli, LaunchPicker, LaunchSelection, SshTarget};

use super::shell::ShellView;

/// Remote half of the 2D-launch picker: the manual SSH host input plus
/// the background CLI-probe state.
///
/// The core [`LaunchPicker`] stays local-only (folder × CLI + yolo); this
/// struct carries only what the core must never own: the host text, which
/// axis owns typing, and the in-flight [`crate::launch::detect_remote_clis`]
/// round trip. Blank-host local is the default, so every existing
/// open/confirm path behaves exactly as before until a host is typed.
#[derive(Default)]
pub(crate) struct PickerRemote {
    /// Manual SSH host (`""` = local spawn). Preselected from the last
    /// used host on open; the folder input doubles as the remote dir.
    pub host_input: String,
    /// True while the host axis owns typing (Tab cycles folder → host →
    /// CLI → folder). Printable keys extend [`Self::host_input`] and never
    /// reach the core picker.
    pub host_focus: bool,
    /// Host the shown catalog was committed for (`None` = the local
    /// catalog). Compared against the trimmed input so re-committing an
    /// unchanged host is a no-op (no second probe).
    pub probed_host: Option<String>,
    /// In-flight [`crate::launch::detect_remote_clis`] round trip, if any.
    /// Polled on every picker key and pump tick; the sender lives on the
    /// spawned thread, so dropping the receiver cancels delivery without
    /// ever blocking the UI.
    pub in_flight: Option<mpsc::Receiver<(String, Vec<AvailableCli>)>>,
}

/// Swap a picker catalog while keeping the selected CLI when it survives
/// (same ids, new availability): the cursor never jumps rows just because
/// a probe landed. Empty catalogs are refused — the rows stay put.
fn restage_catalog(picker: &mut LaunchPicker, catalog: Vec<AvailableCli>) {
    if catalog.is_empty() || picker.clis.is_empty() {
        return;
    }
    let keep = picker.selected_cli().id.clone();
    picker.clis = catalog;
    picker.cli_cursor = picker.clis.iter().position(|c| c.id == keep).unwrap_or(0);
}

impl ShellView {
    /// Picker capture state (2D launch): `Some` while the picker owns the
    /// keyboard. `None` is every other mode (filter/folder captures,
    /// terminal, nav). Read by nav dispatch tests and (on macOS) the
    /// status-bar render.
    pub(crate) fn picker_open(&self) -> bool {
        self.launch_picker.is_some()
    }

    /// True while the picker targets a remote host (non-blank host input).
    pub(crate) fn picker_is_remote(&self) -> bool {
        self.launch_picker.is_some() && !self.picker_remote.host_input.trim().is_empty()
    }

    /// Open the 2D picker (`N` in nav focus, the `▾` caret, the empty-pane
    /// picker button): autodetect fresh on every open so the CLI list is
    /// never stale, preselecting the configured default / last-used CLI
    /// and the default folder. No-op when already open.
    pub(crate) fn open_launch_picker(&mut self) {
        if self.launch_picker.is_some() {
            return;
        }
        let clis = crate::launch::detect_available_clis();
        if clis.is_empty() {
            // Unreachable in practice (the catalog always lists all four):
            // kept fail-visible instead of opening an empty picker.
            self.app.set_error(
                "no supported agent CLI found on PATH (muse, claude, opencode, codex) · install one, then N: retry",
            );
            return;
        }
        let default_cli = self.app.config_default_cli().map(str::to_string);
        let last_cli = self.app.config_last_cli().map(str::to_string);
        let effective = crate::launch::resolve_effective_cli(
            None,
            last_cli.as_deref(),
            default_cli.as_deref(),
            &clis,
        );
        let default_cwd = self.app.config_default_cwd().map(str::to_string);
        let recents = self.app.config_recents().to_vec();
        self.launch_picker = Some(crate::launch::LaunchPicker::new(
            clis,
            Some(&effective),
            default_cwd.as_deref(),
            recents,
        ));
        // Remote half: blank-host local by default, preselecting the last
        // used host so a repeat remote dial stays one Enter. A remembered
        // host commits immediately: the unchecked catalog shows meanwhile
        // and the background probe swaps the real one in on arrival.
        self.picker_remote = PickerRemote {
            host_input: self
                .app
                .config_last_host()
                .map(str::to_string)
                .unwrap_or_default(),
            ..Default::default()
        };
        if !self.picker_remote.host_input.trim().is_empty() {
            self.commit_picker_host();
        }
    }

    /// Close the picker with zero side effects (Esc, Cmd+1/2 pane jumps):
    /// no session starts, memory untouched.
    pub(crate) fn cancel_launch_picker(&mut self) {
        self.launch_picker = None;
        self.picker_remote = PickerRemote::default();
    }

    /// Set the picker host explicitly (host pick / tests): assigns the
    /// input and commits it — blank restores the local catalog, non-blank
    /// shows the unchecked catalog meanwhile and probes in the background.
    pub(crate) fn set_picker_host(&mut self, host: &str) {
        self.picker_remote.host_input = host.to_string();
        self.picker_remote.host_focus = false;
        self.commit_picker_host();
    }

    /// Commit the current host input to the catalog: blank restores the
    /// cheap local rescan, non-blank swaps in the unchecked catalog and
    /// (off the UI thread) runs `detect_remote_clis`. Re-committing an
    /// unchanged host is a no-op — no second probe, no cursor jump.
    pub(crate) fn commit_picker_host(&mut self) {
        let host = self.picker_remote.host_input.trim().to_string();
        if host.is_empty() {
            if self.picker_remote.probed_host.is_none() && self.picker_remote.in_flight.is_none() {
                return; // already showing the local catalog
            }
            self.picker_remote.in_flight = None;
            self.picker_remote.probed_host = None;
            if let Some(picker) = self.launch_picker.as_mut() {
                restage_catalog(picker, crate::launch::detect_available_clis());
            }
            return;
        }
        if self.picker_remote.probed_host.as_deref() == Some(host.as_str()) {
            return; // already covered: no second probe
        }
        // Show the unchecked catalog meanwhile (same fail-visible shape as
        // every probe failure: all rows listed, none available).
        if let Some(picker) = self.launch_picker.as_mut() {
            restage_catalog(picker, crate::launch::parse_remote_probe_output(""));
        }
        self.picker_remote.probed_host = Some(host.clone());
        // Off the UI thread: one bounded `ssh` round trip. The result is
        // picked up by `poll_remote_probe` (every picker key + pump tick).
        // Tests skip the spawn (no network in tests) and inject arrivals
        // via `apply_remote_catalog`.
        #[cfg(not(test))]
        {
            let (tx, rx) = mpsc::channel();
            self.picker_remote.in_flight = Some(rx);
            if std::thread::Builder::new()
                .name("staap-remote-probe".to_string())
                .spawn(move || {
                    let catalog = crate::launch::detect_remote_clis(&host);
                    let _ = tx.send((host, catalog));
                })
                .is_err()
            {
                // Spawn failed: keep the unchecked catalog (fail-visible
                // rows) instead of an abandoned in-flight marker.
                self.picker_remote.in_flight = None;
            }
        }
    }

    /// Collect one finished background probe, if any: arrivals matching the
    /// current host input swap the catalog in (keeping the selected CLI);
    /// stale arrivals (host changed since) are dropped. Returns true when
    /// the visible catalog changed, so the pump repaints.
    pub(crate) fn poll_remote_probe(&mut self) -> bool {
        let mut latest: Option<(String, Vec<AvailableCli>)> = None;
        if let Some(rx) = self.picker_remote.in_flight.as_ref() {
            for msg in rx.try_iter() {
                latest = Some(msg);
            }
        }
        match latest {
            None => false,
            Some((host, catalog)) => {
                self.picker_remote.in_flight = None;
                self.apply_remote_catalog(&host, catalog)
            }
        }
    }

    /// Apply one remote catalog for `host` (probe arrival / test
    /// injection): stale hosts (input moved on) and closings are refused.
    /// Returns true when the rows changed.
    pub(crate) fn apply_remote_catalog(&mut self, host: &str, catalog: Vec<AvailableCli>) -> bool {
        if self.launch_picker.is_none() || catalog.is_empty() {
            return false;
        }
        if host != self.picker_remote.host_input.trim() {
            return false; // stale: the host moved on already
        }
        if let Some(picker) = self.launch_picker.as_mut() {
            restage_catalog(picker, catalog);
        }
        true
    }

    /// Tab cycles the typing axis folder → host → CLI → folder. Leaving
    /// the host commits it (unchecked catalog + background probe when the
    /// host changed); entering it changes nothing until commit.
    pub(crate) fn cycle_picker_axis(&mut self) {
        if self.launch_picker.is_none() {
            return;
        }
        if self.picker_remote.host_focus {
            // host → CLI
            self.picker_remote.host_focus = false;
            if let Some(picker) = self.launch_picker.as_mut() {
                picker.folder_focus = false;
            }
            self.commit_picker_host();
        } else if self.launch_picker.as_ref().is_some_and(|p| p.folder_focus) {
            // folder → host
            self.picker_remote.host_focus = true;
        } else {
            // CLI → folder
            if let Some(picker) = self.launch_picker.as_mut() {
                picker.folder_focus = true;
            }
        }
    }

    /// Step the folder input through the current host's remote folder
    /// recents (the remote analogue of the core recent-folder presets):
    /// cycling keeps full editability (type to refine, Backspace clears).
    /// No-op without a host or without recents — same as the local path.
    pub(crate) fn step_remote_recent(&mut self, forward: bool) {
        let host = self.picker_remote.host_input.trim().to_string();
        if host.is_empty() {
            return;
        }
        let recents = self.app.config_remote_recents(&host).to_vec();
        if recents.is_empty() {
            return;
        }
        let Some(picker) = self.launch_picker.as_mut() else {
            return;
        };
        let current = recents.iter().position(|r| *r == picker.folder_input);
        let next = match current {
            Some(i) if forward => (i + 1) % recents.len(),
            Some(i) => (i + recents.len() - 1) % recents.len(),
            None if forward => 0,
            None => recents.len() - 1,
        };
        picker.folder_input = recents[next].clone();
    }

    /// Resolve the picker's confirmed selection: a blank host stays local
    /// (folder + CLI + yolo); a host builds the remote selection with the
    /// folder riding on the target (never as the local cwd).
    fn picker_selection(&self, picker: &LaunchPicker, yolo_default: bool) -> LaunchSelection {
        let expanded = crate::app::expand_cwd_input(&picker.folder_input);
        let host = self.picker_remote.host_input.trim();
        if host.is_empty() {
            LaunchSelection::new(
                picker.selected_cli().id.clone(),
                expanded,
                picker.yolo.resolve(yolo_default),
            )
        } else {
            LaunchSelection::new_remote(
                picker.selected_cli().id.clone(),
                picker.yolo.resolve(yolo_default),
                SshTarget::new(host.to_string(), expanded),
            )
        }
    }

    /// Confirm the picker (Enter): validate the folder (blank inherits),
    /// start the session through the shared `App::start_launch` funnel,
    /// flash the `runs: <preview>` confirmation. A missing folder refuses
    /// with a flash and stays open for a fix. Returns true on success.
    ///
    /// Remote rows skip the local folder check (a remote dir never exists
    /// on this machine by construction) and queue the `ssh -t` spawn
    /// through the same funnel instead.
    pub(crate) fn accept_launch_picker(&mut self) -> bool {
        if self.launch_picker.is_none() {
            return false;
        }
        self.poll_remote_probe();
        // A host typed but never tabbed out of still counts: commit it
        // (unchecked catalog + background probe) before confirming, so
        // Enter picks the host up without an extra Tab.
        let host = self.picker_remote.host_input.trim().to_string();
        if !host.is_empty() && self.picker_remote.probed_host.as_deref() != Some(host.as_str()) {
            self.commit_picker_host();
        }
        let Some(picker) = self.launch_picker.clone() else {
            return false;
        };
        let yolo_default = self
            .app
            .config_yolo_default(picker.selected_cli().id.as_str());
        let selection = self.picker_selection(&picker, yolo_default);
        // Local rows keep the fail-visible folder check; remote rows skip
        // it — a remote dir never exists on this machine by construction.
        if selection.remote.is_none() {
            if let Some(dir) = &selection.cwd {
                if !std::path::Path::new(dir).is_dir() {
                    self.app.set_status(format!("no such folder: {dir}"));
                    return false;
                }
            }
        }
        self.launch_picker = None;
        self.picker_remote = PickerRemote::default();
        self.link_cursor = None;
        if !self.request_new_launch(selection.clone()) {
            return false;
        }
        #[cfg(not(test))]
        let _ = self.app.save_config();
        let yolo_tag = if selection.yolo { " · yolo" } else { "" };
        self.app
            .set_status(format!("runs: {}{yolo_tag}", selection.preview()));
        true
    }

    /// Picker key dispatch: Enter confirms, Esc cancels, Tab cycles the
    /// typing axis (folder → host → CLI), everything else forwards to the
    /// framework-free [`crate::launch::LaunchPicker::key`] state machine —
    /// except on the host axis, which owns every key (printables extend the
    /// host input, Up/Down/Left/Right are inert) and never reaches the core.
    /// Remote Up/Down steps the host's folder recents instead of the local
    /// ones. Returns true when the picker owned the key.
    pub(crate) fn picker_key(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        ctrl: bool,
        platform: bool,
    ) -> bool {
        if self.launch_picker.is_none() {
            return false;
        }
        self.poll_remote_probe();
        // The host axis owns every key while focused: printable keys
        // extend the host input and nothing reaches the core picker.
        if self.picker_remote.host_focus {
            match key {
                "enter" => {
                    self.accept_launch_picker();
                }
                "escape" => {
                    self.cancel_launch_picker();
                }
                "tab" => {
                    self.cycle_picker_axis();
                }
                "backspace" => {
                    self.picker_remote.host_input.pop();
                }
                "up" | "down" | "left" | "right" => {
                    // No host presets to step: consumed, no motion.
                }
                _ => {
                    if ctrl || platform {
                        return true;
                    }
                    if let Some(c) = key_char {
                        if c.chars().count() == 1 {
                            self.picker_remote.host_input.push_str(c);
                        }
                    }
                }
            }
            return true;
        }
        match key {
            "enter" => {
                self.accept_launch_picker();
            }
            "escape" => {
                self.cancel_launch_picker();
            }
            "tab" => {
                self.cycle_picker_axis();
            }
            "up" | "down" if self.picker_is_remote() => {
                self.step_remote_recent(key == "down");
            }
            _ => {
                if let Some(picker) = self.launch_picker.as_mut() {
                    picker.key(key, key_char, ctrl, platform);
                }
            }
        }
        true
    }

    /// Preview line for the picker footer (`runs: muse --yolo in ~/api`,
    /// remote `runs: ssh mybox: muse --yolo in /srv/api`): the exact spawn,
    /// verifiable before it runs.
    pub(crate) fn picker_preview(&self) -> String {
        match &self.launch_picker {
            Some(picker) => {
                let yolo_default = self
                    .app
                    .config_yolo_default(picker.selected_cli().id.as_str());
                format!(
                    "runs: {}",
                    self.picker_selection(picker, yolo_default).preview()
                )
            }
            None => String::new(),
        }
    }

    /// Yolo label for the picker row (text, never color-only).
    pub(crate) fn picker_yolo_label(&self) -> &'static str {
        match &self.launch_picker {
            Some(picker) => picker.yolo.label(),
            None => crate::launch::YoloChoice::UseDefault.label(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::runs::test_shell;
    use super::super::shell::ShellView;
    use super::*;

    fn open_picker() -> ShellView {
        let mut view = test_shell();
        view.open_launch_picker();
        assert!(view.picker_open(), "picker opens headlessly");
        view
    }

    fn ready_catalog() -> Vec<AvailableCli> {
        crate::launch::SUPPORTED_CLIS
            .iter()
            .map(|id| AvailableCli {
                id: id.to_string(),
                program: id.to_string(),
                path: Some(format!("/bin/{id}")),
                available: true,
            })
            .collect()
    }

    #[test]
    fn blank_host_keeps_the_local_picker_unchanged() {
        let view = open_picker();
        assert!(!view.picker_is_remote());
        assert_eq!(view.picker_remote.host_input, "");
        assert!(!view.picker_remote.host_focus);
        // Local catalog: all four rows in fixed order, cursor on one.
        let picker = view.launch_picker.as_ref().unwrap();
        let ids: Vec<&str> = picker.clis.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["muse", "claude", "opencode", "codex"]);
        assert!(picker.cli_cursor < picker.clis.len());
    }

    #[test]
    fn open_preselects_the_last_host_and_commits_unchecked() {
        let mut view = test_shell();
        view.app
            .config_mut()
            .note_remote_launch("mybox", Some("/srv/api"));
        view.open_launch_picker();
        assert!(view.picker_open());
        assert_eq!(view.picker_remote.host_input, "mybox");
        assert!(view.picker_is_remote());
        // Meanwhile (no network in tests): the unchecked catalog.
        let picker = view.launch_picker.as_ref().unwrap();
        assert_eq!(picker.clis.len(), 4);
        assert!(
            picker.clis.iter().all(|c| !c.available),
            "remembered host starts unchecked: {:?}",
            picker.clis
        );
        assert!(
            view.picker_remote.in_flight.is_none(),
            "no probe thread in tests"
        );
    }

    #[test]
    fn host_commit_shows_unchecked_catalog_until_the_probe_lands() {
        let mut view = open_picker();
        view.set_picker_host("mybox");
        assert!(view.picker_is_remote());
        let picker = view.launch_picker.as_ref().unwrap();
        assert!(
            picker.clis.iter().all(|c| !c.available),
            "meanwhile: all listed, none available"
        );
        // The probe arrival swaps the real catalog in, keeping the CLI.
        let before = picker.selected_cli().id.clone();
        assert!(view.apply_remote_catalog("mybox", ready_catalog()));
        let picker = view.launch_picker.as_ref().unwrap();
        assert!(picker.clis.iter().all(|c| c.available));
        assert_eq!(picker.selected_cli().id, before, "cursor keeps its CLI");
    }

    #[test]
    fn stale_probe_arrivals_are_dropped() {
        let mut view = open_picker();
        view.set_picker_host("box-a");
        // The host moved on before the reply landed: refused, rows stay.
        view.set_picker_host("box-b");
        assert!(!view.apply_remote_catalog("box-a", ready_catalog()));
        let picker = view.launch_picker.as_ref().unwrap();
        assert!(
            picker.clis.iter().all(|c| !c.available),
            "stale arrival never swaps the rows"
        );
        assert!(view.apply_remote_catalog("box-b", ready_catalog()));
        assert!(view
            .launch_picker
            .as_ref()
            .unwrap()
            .clis
            .iter()
            .all(|c| c.available));
    }

    #[test]
    fn clearing_the_host_restores_the_local_catalog() {
        let mut view = open_picker();
        view.set_picker_host("mybox");
        assert!(view.picker_is_remote());
        view.set_picker_host("  ");
        assert!(!view.picker_is_remote());
        assert_eq!(view.launch_picker.as_ref().unwrap().clis.len(), 4);
    }

    #[test]
    fn remote_accept_skips_local_folder_validation() {
        let mut view = open_picker();
        view.set_picker_host("mybox");
        if let Some(picker) = view.launch_picker.as_mut() {
            picker.folder_input = "/no/such/dir-anywhere".to_string();
        }
        assert!(
            view.accept_launch_picker(),
            "remote rows skip the local check"
        );
        assert!(!view.picker_open());
        let kind = view.app.take_pending_spawn().expect("remote spawn queued");
        assert!(kind.is_remote());
        let target = kind.remote_target().expect("remote target queued");
        assert_eq!(target.host, "mybox");
        assert_eq!(
            target.remote_cwd.as_deref(),
            Some("/no/such/dir-anywhere"),
            "the folder rides the target, never the local cwd"
        );
        let id = view.active_id().expect("new run selected");
        assert_eq!(view.app.session_cwd(&id), None);
        let flash = view.app.status_text().unwrap_or_default();
        assert!(
            flash.contains("ssh mybox:"),
            "confirm flash names the target: {flash}"
        );
    }

    #[test]
    fn local_accept_still_refuses_a_missing_folder() {
        let mut view = open_picker();
        assert!(!view.picker_is_remote());
        if let Some(picker) = view.launch_picker.as_mut() {
            picker.folder_input = "/no/such/dir-anywhere".to_string();
        }
        assert!(!view.accept_launch_picker());
        assert!(view.picker_open(), "stays open for a fix");
        assert!(view.app.take_pending_spawn().is_none());
        let status = view.app.status_text().unwrap_or_default();
        assert!(status.contains("no such folder"), "{status}");
    }

    #[test]
    fn tab_cycles_folder_host_cli_and_back() {
        let mut view = open_picker();
        assert!(view.launch_picker.as_ref().unwrap().folder_focus);
        assert!(!view.picker_remote.host_focus);
        view.picker_key("tab", None, false, false); // folder → host
        assert!(view.picker_remote.host_focus);
        // Typing on the host axis extends the host, never the folder.
        let folder_before = view.launch_picker.as_ref().unwrap().folder_input.clone();
        view.picker_key("m", Some("m"), false, false);
        assert_eq!(view.picker_remote.host_input, "m");
        assert_eq!(
            view.launch_picker.as_ref().unwrap().folder_input,
            folder_before
        );
        view.picker_key("tab", None, false, false); // host → CLI (commits)
        assert!(!view.picker_remote.host_focus);
        assert!(!view.launch_picker.as_ref().unwrap().folder_focus);
        assert!(view.picker_is_remote(), "enter-free Tab commits the host");
        view.picker_key("tab", None, false, false); // CLI → folder
        assert!(view.launch_picker.as_ref().unwrap().folder_focus);
        assert!(!view.picker_remote.host_focus);
    }

    #[test]
    fn remote_up_down_steps_per_host_folder_recents() {
        let mut view = open_picker();
        view.app
            .config_mut()
            .note_remote_launch("mybox", Some("/srv/api"));
        view.app
            .config_mut()
            .note_remote_launch("mybox", Some("/srv/web"));
        view.set_picker_host("mybox");
        // MRU-first: the latest remote dir comes first.
        view.picker_key("down", None, false, false);
        assert_eq!(
            view.launch_picker.as_ref().unwrap().folder_input,
            "/srv/web"
        );
        view.picker_key("down", None, false, false);
        assert_eq!(
            view.launch_picker.as_ref().unwrap().folder_input,
            "/srv/api"
        );
        view.picker_key("up", None, false, false);
        assert_eq!(
            view.launch_picker.as_ref().unwrap().folder_input,
            "/srv/web"
        );
    }

    #[test]
    fn remote_preview_names_the_ssh_target() {
        let mut view = open_picker();
        view.set_picker_host("mybox");
        let preview = view.picker_preview();
        assert!(
            preview.starts_with("runs: ssh mybox: "),
            "verifiable before launch: {preview}"
        );
        // Local preview keeps its exact historic shape.
        view.set_picker_host("");
        let local = view.picker_preview();
        assert!(local.starts_with("runs: "), "{local}");
        assert!(!local.contains("ssh"), "{local}");
    }

    #[test]
    fn enter_from_the_host_axis_picks_the_host_up() {
        let mut view = open_picker();
        view.picker_key("tab", None, false, false); // folder → host
        for c in ["m", "y", "b", "o", "x"] {
            view.picker_key(c, Some(c), false, false);
        }
        assert_eq!(view.picker_remote.host_input, "mybox");
        // Enter straight from the host axis (no extra Tab): remote queued.
        if let Some(picker) = view.launch_picker.as_mut() {
            picker.folder_input = String::new(); // remote login dir
        }
        view.picker_key("enter", None, false, false);
        let kind = view.app.take_pending_spawn().expect("remote spawn queued");
        assert!(kind.is_remote());
        assert_eq!(kind.remote_target().unwrap().host, "mybox");
    }
}
