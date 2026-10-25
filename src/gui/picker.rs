//! Two-dimensional new-session picker (folder × CLI + yolo) for the gpui shell.
//!
//! Single-button model: `n` repeats the last launch instantly (expert fast
//! path, 1 keystroke, unchanged); `N` / the single `+ New` button (header
//! and empty pane alike) opens this capture in-place on the right side —
//! never a modal, never an instant repeat — where the folder axis and the
//! CLI axis are picked independently before Enter confirms.
//!
//! Remote rows add a machine axis on top: the machine dropdown (This
//! machine, saved hosts, Add remote machine...) retargets the pick at an
//! SSH host, with a `user@hostname` placeholder hint while empty and the
//! host default folder inherited on commit. Committing the host swaps the
//! CLI catalog to the unchecked shape meanwhile and probes the host with
//! [`crate::launch::detect_remote_clis`] off the UI thread; the arrival
//! swaps the real catalog in (keeping the selected CLI). A folder browser
//! ([`crate::launch::list_remote_dir`], same background discipline) offers
//! the remote dir for picking. Confirming a remote row skips the local
//! folder check (a remote dir never exists here by construction) and
//! funnels through the shared `App::start_launch` remote path, so the
//! local PTY runs `ssh -t <host> <cli…>`.
//!
//! Thin by contract: the keyboard state machine lives in the framework-free
//! `crate::launch::LaunchPicker` (tested headlessly on every OS gate); this
//! module only opens/accepts/cancels it, carries the host input the core
//! must never own, and renders the preview/status copy the shell shows.
//! Rendering (the in-place screen over the terminal column) reads the same
//! state natively.

use std::sync::mpsc;

use crate::launch::{AvailableCli, LaunchPicker, LaunchSelection, RemoteEntry, SshTarget};

use super::shell::ShellView;

/// Machine-field placeholder hint (SSH picker): the host axis shows this
/// while empty and focused, so the `user@hostname` shape plus a concrete
/// example is visible before anything is typed.
pub(crate) const MACHINE_PLACEHOLDER: &str = "user@hostname — e.g. dev@mybox";

/// Label of the last machine-dropdown row: opens the SSH add dialog.
pub(crate) const ADD_REMOTE_LABEL: &str = "Add remote machine...";

/// Label of the first machine-dropdown row: back to a local spawn.
pub(crate) const THIS_MACHINE_LABEL: &str = "This machine";

/// One row of the machine dropdown (SSH picker): `This machine`, one row
/// per saved host (display name + host), then `Add remote machine...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MachineOption {
    ThisMachine,
    Saved(usize),
    AddNew,
}

/// SSH add dialog (SSH picker): display name, `user@hostname`-or-alias
/// host, optional default folder, plus the tester result. Focus cycles
/// name → host → folder; Enter saves, Esc cancels, and the Test button
/// (or `ctrl+t`) probes the host through the core tester without saving.
/// Opened from a saved row it edits instead (`edit_key` set): fields
/// prefill and saving renames in place — the typo fix.
#[derive(Default)]
pub(crate) struct SshAddDialog {
    /// Display name shown in the machine dropdown.
    pub name_input: String,
    /// SSH target as typed (`dev@mybox`, an ssh-config alias).
    pub host_input: String,
    /// Default folder on the host (blank = host MRU, then remote home).
    pub folder_input: String,
    /// Original host (or name) being edited (`None` = adding a new
    /// machine). Saving with this set renames through the core instead
    /// of adding, so a typo'd target is fixed, not duplicated.
    pub edit_key: Option<String>,
    /// Which field owns typing: 0 = name, 1 = host, 2 = folder.
    pub focus: usize,
    /// Last tester round trip, if any (reachable flag + detected CLIs).
    pub test: Option<crate::launch::RemoteReachability>,
    /// In-flight [`crate::launch::test_remote_host`] round trip, if any.
    /// Polled on every picker key; dropping the receiver cancels.
    pub in_flight: Option<mpsc::Receiver<(String, crate::launch::RemoteReachability)>>,
    /// Refusal flash (blank name/host, duplicate cap) shown in-dialog.
    pub error: Option<String>,
}

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
    /// Machine dropdown open (SSH picker): while true, Up/Down steps the
    /// `machine_cursor` across This machine / saved hosts / Add remote
    /// machine..., previewing each host; Enter on Add opens the add
    /// dialog instead of confirming. Tab onto the machine axis opens it.
    pub machine_open: bool,
    /// Highlighted machine-dropdown row (index into
    /// [`ShellView::machine_options`]).
    pub machine_cursor: usize,
    /// Folder browser for the current host (SSH picker): one
    /// [`crate::launch::list_remote_dir`] round trip per host/folder,
    /// shown under the folder input; Up/Down (local) or click picks a
    /// directory into the folder input. Empty until the first host
    /// commits; refreshed off the UI thread, injected in tests.
    pub remote_listing: Vec<RemoteEntry>,
    /// Highlighted folder-browser row.
    pub remote_listing_cursor: usize,
    /// (host, path) the visible listing was taken for; stale arrivals
    /// (host or folder moved on) are dropped like stale CLI probes.
    pub remote_listing_for: Option<(String, String)>,
    /// In-flight [`crate::launch::list_remote_dir`] round trip, if any.
    pub listing_in_flight: Option<mpsc::Receiver<(String, String, Vec<RemoteEntry>)>>,
    /// SSH add dialog state (`None` = closed). While open it owns every
    /// picker key.
    pub ssh_add: Option<SshAddDialog>,
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

    /// Open the 2D picker (`N` in nav focus, the single `+ New` button,
    /// the empty-pane button): autodetect fresh on every open so the CLI list is
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
        self.picker_remote = PickerRemote::default();
        // Preselect the last-used host so a repeat remote dial stays
        // one Enter; routing through `set_picker_host` keeps the
        // single commit funnel (unchecked meanwhile-catalog plus
        // background probe).
        let last = self
            .app
            .config_last_host()
            .map(str::to_string)
            .unwrap_or_default();
        if !last.trim().is_empty() {
            self.set_picker_host(&last);
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
        self.picker_remote.machine_open = false;
        self.commit_picker_host();
    }

    /// Machine dropdown rows for the current config (SSH picker): This
    /// machine first, one row per saved host in saved order, Add remote
    /// machine... last.
    pub(crate) fn machine_options(&self) -> Vec<MachineOption> {
        let mut opts = vec![MachineOption::ThisMachine];
        for i in 0..self.app.config_saved_hosts().len() {
            opts.push(MachineOption::Saved(i));
        }
        opts.push(MachineOption::AddNew);
        opts
    }

    /// Human label for one machine-dropdown row.
    pub(crate) fn machine_option_label(&self, opt: &MachineOption) -> String {
        match opt {
            MachineOption::ThisMachine => THIS_MACHINE_LABEL.to_string(),
            MachineOption::Saved(i) => match self.app.config_saved_hosts().get(*i) {
                Some(h) => format!("{} · {}", h.name, h.host),
                None => "saved host".to_string(),
            },
            MachineOption::AddNew => ADD_REMOTE_LABEL.to_string(),
        }
    }

    /// Open the machine dropdown on the current host (Tab onto the
    /// machine axis): the cursor starts on the matching row (This
    /// machine for blank input, the saved row for a known host, else
    /// the closest row) so Enter confirms without motion.
    pub(crate) fn open_machine_dropdown(&mut self) {
        let host = self.picker_remote.host_input.trim().to_string();
        let options = self.machine_options();
        let cursor = if host.is_empty() {
            0
        } else {
            options
                .iter()
                .position(|o| match o {
                    MachineOption::Saved(i) => self
                        .app
                        .config_saved_hosts()
                        .get(*i)
                        .is_some_and(|h| h.host == host || h.name == host),
                    _ => false,
                })
                .unwrap_or(0)
        };
        self.picker_remote.machine_cursor = cursor;
        self.picker_remote.machine_open = true;
    }

    /// Step the machine-dropdown cursor (Up/Down on the machine axis),
    /// previewing the highlighted host like Tab-commit does: This
    /// machine clears the host (local catalog), a saved row dials that
    /// host (unchecked meanwhile-catalog + background probe + folder
    /// browser), Add stays put until Enter opens the dialog.
    pub(crate) fn step_machine_dropdown(&mut self, forward: bool) {
        if !self.picker_remote.machine_open {
            return;
        }
        let n = self.machine_options().len();
        if n == 0 {
            return;
        }
        let next = if forward {
            (self.picker_remote.machine_cursor + 1) % n
        } else {
            (self.picker_remote.machine_cursor + n - 1) % n
        };
        self.picker_remote.machine_cursor = next;
        match self.machine_options()[next].clone() {
            MachineOption::ThisMachine => {
                self.picker_remote.host_input.clear();
                self.commit_picker_host();
            }
            MachineOption::Saved(i) => {
                if let Some(host) = self.app.config_saved_hosts().get(i).map(|h| h.host.clone()) {
                    self.picker_remote.host_input = host;
                    self.commit_picker_host();
                }
            }
            MachineOption::AddNew => {}
        }
    }

    /// Commit the highlighted machine-dropdown row (Enter on the machine
    /// axis): This machine / saved rows confirm through the normal host
    /// commit; Add opens the SSH add dialog instead of starting a run.
    /// Returns true when the dialog opened (the picker stays open).
    pub(crate) fn commit_machine_dropdown(&mut self) -> bool {
        if !self.picker_remote.machine_open {
            return false;
        }
        let opt = self.machine_options()[self
            .picker_remote
            .machine_cursor
            .min(self.machine_options().len().saturating_sub(1))]
        .clone();
        self.picker_remote.machine_open = false;
        match opt {
            MachineOption::AddNew => {
                self.open_ssh_add();
                true
            }
            _ => {
                self.commit_picker_host();
                false
            }
        }
    }

    /// Commit the current host input to the catalog: blank restores the
    /// cheap local rescan, non-blank swaps in the unchecked catalog and
    /// (off the UI thread) runs `detect_remote_clis`. A blank folder
    /// inherits the host default folder (saved default → host MRU →
    /// remote home) before the folder browser refreshes, so the browser
    /// and the confirm path start from the same dir. Re-committing an
    /// unchanged host is a no-op — no second probe, no cursor jump.
    pub(crate) fn commit_picker_host(&mut self) {
        let host = self.picker_remote.host_input.trim().to_string();
        if host.is_empty() {
            if self.picker_remote.probed_host.is_none() && self.picker_remote.in_flight.is_none() {
                return; // already showing the local catalog
            }
            self.picker_remote.in_flight = None;
            self.picker_remote.probed_host = None;
            self.picker_remote.listing_in_flight = None;
            self.picker_remote.remote_listing.clear();
            self.picker_remote.remote_listing_for = None;
            if let Some(picker) = self.launch_picker.as_mut() {
                restage_catalog(picker, crate::launch::detect_available_clis());
            }
            return;
        }
        // Folder inheritance (SSH picker): a blank folder starts from
        // the host default folder so the browser and the spawn agree.
        if self
            .launch_picker
            .as_ref()
            .is_some_and(|p| p.folder_input.trim().is_empty())
        {
            if let Some(default) = self.app.config_remote_default_folder(&host) {
                if let Some(picker) = self.launch_picker.as_mut() {
                    picker.folder_input = default;
                }
            }
        }
        self.refresh_remote_listing();
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

    /// Tab cycles the typing axis folder → machine → CLI → folder.
    /// Leaving the machine commits it (unchecked catalog + background
    /// probe when the host changed); entering it opens the dropdown.
    pub(crate) fn cycle_picker_axis(&mut self) {
        if self.launch_picker.is_none() {
            return;
        }
        if self.picker_remote.host_focus {
            // machine → CLI
            self.picker_remote.host_focus = false;
            self.picker_remote.machine_open = false;
            if let Some(picker) = self.launch_picker.as_mut() {
                picker.folder_focus = false;
            }
            self.commit_picker_host();
        } else if self.launch_picker.as_ref().is_some_and(|p| p.folder_focus) {
            // folder → machine (focus is exclusive: leaving the folder
            // clears it, mirroring toggle_machine_dropdown and the
            // machine → CLI arm below — otherwise a later tab overshoots
            // the machine axis with the dropdown closed).
            self.picker_remote.host_focus = true;
            if let Some(picker) = self.launch_picker.as_mut() {
                picker.folder_focus = false;
            }
            self.open_machine_dropdown();
        } else {
            // CLI → folder
            if let Some(picker) = self.launch_picker.as_mut() {
                picker.folder_focus = true;
            }
        }
    }

    /// Listing path for the folder browser (SSH picker): the folder
    /// input, or `~` (remote home) when blank. `list_remote_dir` degrades
    /// to empty on blank paths, so the home fallback keeps the browser
    /// populated on a fresh host pick.
    pub(crate) fn remote_listing_path(&self) -> String {
        let folder = self
            .launch_picker
            .as_ref()
            .map(|p| p.folder_input.trim().to_string())
            .unwrap_or_default();
        if folder.is_empty() {
            "~".to_string()
        } else {
            folder
        }
    }

    /// Refresh the folder browser for the current host + folder (SSH
    /// picker): one bounded `ssh` round trip off the UI thread. Same
    /// host + path never re-fires (no probe storm while typing); blank
    /// hosts clear the browser. Tests inject via `apply_remote_listing`.
    pub(crate) fn refresh_remote_listing(&mut self) {
        let host = self.picker_remote.host_input.trim().to_string();
        if host.is_empty() || self.launch_picker.is_none() {
            return;
        }
        let path = self.remote_listing_path();
        if self.picker_remote.remote_listing_for.as_ref() == Some(&(host.clone(), path.clone()))
            || self.picker_remote.listing_in_flight.is_some()
        {
            return;
        }
        self.picker_remote.listing_in_flight = None;
        #[cfg(not(test))]
        {
            let (tx, rx) = mpsc::channel();
            self.picker_remote.listing_in_flight = Some(rx);
            if std::thread::Builder::new()
                .name("staap-remote-list".to_string())
                .spawn(move || {
                    let entries = crate::launch::list_remote_dir(&host, &path);
                    let _ = tx.send((host, path, entries));
                })
                .is_err()
            {
                self.picker_remote.listing_in_flight = None;
            }
        }
    }

    /// Collect one finished folder-browser round trip, if any: arrivals
    /// matching the current host + folder swap the browser in; stale
    /// arrivals are dropped. Returns true when the browser changed.
    pub(crate) fn poll_remote_listing(&mut self) -> bool {
        let mut latest: Option<(String, String, Vec<RemoteEntry>)> = None;
        if let Some(rx) = self.picker_remote.listing_in_flight.as_ref() {
            for msg in rx.try_iter() {
                latest = Some(msg);
            }
        }
        match latest {
            None => false,
            Some((host, path, entries)) => {
                self.picker_remote.listing_in_flight = None;
                self.apply_remote_listing(&host, &path, entries)
            }
        }
    }

    /// Apply one folder-browser listing (arrival / test injection):
    /// stale host + path pairs and closings are refused. Returns true
    /// when the browser changed.
    pub(crate) fn apply_remote_listing(
        &mut self,
        host: &str,
        path: &str,
        entries: Vec<RemoteEntry>,
    ) -> bool {
        if self.launch_picker.is_none() {
            return false;
        }
        if host != self.picker_remote.host_input.trim() || *path != self.remote_listing_path() {
            return false; // stale: the host or folder moved on already
        }
        self.picker_remote.remote_listing_cursor = 0;
        self.picker_remote.remote_listing = entries;
        self.picker_remote.remote_listing_for = Some((host.to_string(), path.to_string()));
        true
    }

    /// Test-only shorthand: inject a browser listing without a network
    /// round trip (skips the staleness gate — the injector is the
    /// arrival for the current host + folder).
    #[cfg(test)]
    pub(crate) fn set_remote_listing(&mut self, entries: Vec<RemoteEntry>) {
        let host = self.picker_remote.host_input.trim().to_string();
        let path = self.remote_listing_path();
        self.picker_remote.remote_listing_cursor = 0;
        self.picker_remote.remote_listing = entries;
        self.picker_remote.remote_listing_for = Some((host, path));
    }

    /// Step the folder-browser cursor (Up/Down over the directories).
    pub(crate) fn step_remote_listing(&mut self, forward: bool) {
        if self.picker_remote.remote_listing.is_empty() {
            return;
        }
        let n = self.picker_remote.remote_listing.len();
        self.picker_remote.remote_listing_cursor = if forward {
            (self.picker_remote.remote_listing_cursor + 1) % n
        } else {
            (self.picker_remote.remote_listing_cursor + n - 1) % n
        };
    }

    /// Pick the highlighted browser directory into the folder input
    /// (Enter on the browser / click): typing stays fully editable
    /// afterwards, and the browser refreshes for the new dir on commit.
    /// Files are inert (no-op) — only directories change the folder.
    pub(crate) fn confirm_remote_listing(&mut self) -> bool {
        let entry = match self
            .picker_remote
            .remote_listing
            .get(self.picker_remote.remote_listing_cursor)
        {
            Some(e) if e.is_dir => e.clone(),
            _ => return false,
        };
        let base = self.remote_listing_path();
        let joined = if base == "~" || base.is_empty() {
            format!("~/{}", entry.name)
        } else {
            format!("{}/{}", base.trim_end_matches('/'), entry.name)
        };
        if let Some(picker) = self.launch_picker.as_mut() {
            picker.folder_input = joined;
        }
        self.picker_remote.remote_listing.clear();
        self.picker_remote.remote_listing_for = None;
        self.refresh_remote_listing();
        true
    }

    /// Open the SSH add dialog (SSH picker): blank fields, name focused,
    /// no tester result yet. The picker stays open underneath; saving
    /// dials the new host, cancelling returns to the dropdown.
    pub(crate) fn open_ssh_add(&mut self) {
        self.picker_remote.ssh_add = Some(SshAddDialog::default());
    }

    /// Open the SSH dialog to edit one saved host (SSH picker): fields
    /// prefill from the saved row and saving renames in place through
    /// the core (typo fix, cap-exempt, folder MRU follows) instead of
    /// adding a new row. Out-of-range indices are a no-op.
    pub(crate) fn open_ssh_add_for_edit(&mut self, index: usize) {
        let Some(saved) = self.app.config_saved_hosts().get(index).cloned() else {
            return;
        };
        self.picker_remote.ssh_add = Some(SshAddDialog {
            name_input: saved.name.clone(),
            host_input: saved.host.clone(),
            folder_input: saved.default_folder.clone().unwrap_or_default(),
            edit_key: Some(saved.host.clone()),
            ..Default::default()
        });
    }

    /// Open the edit dialog for the highlighted machine-dropdown row
    /// (`ctrl+e` on the machine axis): saved rows prefill the dialog, This
    /// machine / Add are inert. The dialog owns every key once open.
    pub(crate) fn open_highlighted_saved_host_editor(&mut self) {
        if !self.picker_remote.machine_open {
            return;
        }
        let options = self.machine_options();
        let cursor = self
            .picker_remote
            .machine_cursor
            .min(options.len().saturating_sub(1));
        if let Some(MachineOption::Saved(i)) = options.get(cursor).cloned() {
            self.open_ssh_add_for_edit(i);
        }
    }

    /// Delete the highlighted machine-dropdown row (Delete on the machine
    /// axis): saved rows remove through the core and persist like the add
    /// dialog; This machine / Add are inert. When the typed host dies
    /// with its row the picker falls back to local instead of dialing a
    /// ghost; the dropdown stays open with the cursor clamped. Refusals
    /// flash sticky (fail-visible, never a silent no-op).
    pub(crate) fn delete_highlighted_saved_host(&mut self) {
        if !self.picker_remote.machine_open {
            return;
        }
        let options = self.machine_options();
        let cursor = self
            .picker_remote
            .machine_cursor
            .min(options.len().saturating_sub(1));
        let Some(MachineOption::Saved(i)) = options.get(cursor).cloned() else {
            return;
        };
        let Some(saved) = self.app.config_saved_hosts().get(i).cloned() else {
            return;
        };
        if !self.app.config_mut().remove_saved_host(&saved.host) {
            self.app
                .set_error(format!("could not remove {}", saved.host));
            return;
        }
        #[cfg(not(test))]
        let _ = self.app.save_config();
        if self.picker_remote.host_input.trim() == saved.host {
            self.picker_remote.host_input.clear();
            self.commit_picker_host();
        }
        self.picker_remote.machine_cursor =
            cursor.min(self.machine_options().len().saturating_sub(1));
    }

    /// Close the SSH add dialog with zero side effects.
    pub(crate) fn cancel_ssh_add(&mut self) {
        if let Some(dialog) = self.picker_remote.ssh_add.as_mut() {
            dialog.in_flight = None;
        }
        self.picker_remote.ssh_add = None;
    }

    /// Run the add-dialog tester (SSH picker): one bounded non-blocking
    /// `ssh` round trip through the core tester. Blank hosts degrade to
    /// empty-host without a spawn. Tests inject via
    /// `apply_ssh_add_test`.
    // The early return is load-bearing in non-test builds (it skips the
    // probe thread); in test builds the trailing probe block compiles out
    // and clippy flags the return there — allowed, never removed.
    #[allow(clippy::needless_return)]
    pub(crate) fn test_ssh_add(&mut self) {
        let Some(dialog) = self.picker_remote.ssh_add.as_mut() else {
            return;
        };
        let host = dialog.host_input.trim().to_string();
        dialog.in_flight = None;
        if host.is_empty() {
            dialog.test = Some(crate::launch::test_remote_host(&host));
            return;
        }
        #[cfg(not(test))]
        {
            let (tx, rx) = mpsc::channel();
            dialog.in_flight = Some(rx);
            if std::thread::Builder::new()
                .name("staap-remote-test".to_string())
                .spawn(move || {
                    let result = crate::launch::test_remote_host(&host);
                    let _ = tx.send((host, result));
                })
                .is_err()
            {
                dialog.in_flight = None;
                dialog.test = Some(crate::launch::reachability_from_probe(Err(
                    crate::launch::SshFailureReason::Unreachable,
                )));
            }
        }
    }

    /// Collect one finished tester round trip, if any: stale arrivals
    /// (host edited since) are dropped. Returns true when the tester
    /// result changed.
    pub(crate) fn poll_ssh_add_test(&mut self) -> bool {
        let mut latest: Option<(String, crate::launch::RemoteReachability)> = None;
        if let Some(dialog) = self.picker_remote.ssh_add.as_ref() {
            if let Some(rx) = dialog.in_flight.as_ref() {
                for msg in rx.try_iter() {
                    latest = Some(msg);
                }
            }
        }
        match latest {
            None => false,
            Some((host, result)) => {
                if let Some(dialog) = self.picker_remote.ssh_add.as_mut() {
                    dialog.in_flight = None;
                }
                self.apply_ssh_add_test(&host, result)
            }
        }
    }

    /// Apply one tester result for `host` (arrival / test injection):
    /// stale hosts and closings are refused. Returns true when set.
    pub(crate) fn apply_ssh_add_test(
        &mut self,
        host: &str,
        result: crate::launch::RemoteReachability,
    ) -> bool {
        let Some(dialog) = self.picker_remote.ssh_add.as_mut() else {
            return false;
        };
        if host != dialog.host_input.trim() {
            return false; // stale
        }
        dialog.test = Some(result);
        dialog.error = None;
        true
    }

    /// One-line tester copy for the dialog (`reachable · muse, claude`
    /// / the core reason line, e.g. `auth failed — …`): text, never
    /// color-only. The failure copy renders the core `message` verbatim
    /// (dumb-shell contract: no per-shell error parsing).
    pub(crate) fn ssh_add_test_label(&self) -> Option<String> {
        let dialog = self.picker_remote.ssh_add.as_ref()?;
        let result = dialog.test.as_ref()?;
        if result.reachable {
            let ids: Vec<&str> = result
                .clis
                .iter()
                .filter(|c| c.available)
                .map(|c| c.id.as_str())
                .collect();
            if ids.is_empty() {
                Some("reachable · no supported CLI detected".to_string())
            } else {
                Some(format!("reachable · {}", ids.join(", ")))
            }
        } else {
            Some(
                result
                    .message
                    .clone()
                    .unwrap_or_else(|| "unreachable — check the host and keys".to_string()),
            )
        }
    }

    /// Save the add dialog (SSH picker): blank name/host refuses with an
    /// in-dialog flash; add mode stores through the core saved-hosts add
    /// (cap of 50 — updating an existing host always succeeds), edit mode
    /// renames in place (typo fix, cap-exempt, folder MRU follows);
    /// success closes the dialog, dials the host, and persists the
    /// config. Returns true on success.
    pub(crate) fn save_ssh_add(&mut self) -> bool {
        let (name, host, folder, edit_key) = match self.picker_remote.ssh_add.as_ref() {
            Some(d) => (
                d.name_input.trim().to_string(),
                d.host_input.trim().to_string(),
                d.folder_input.trim().to_string(),
                d.edit_key.clone(),
            ),
            None => return false,
        };
        if name.is_empty() || host.is_empty() {
            if let Some(dialog) = self.picker_remote.ssh_add.as_mut() {
                dialog.error = Some("name and host are required".to_string());
            }
            return false;
        }
        let folder = if folder.is_empty() {
            None
        } else {
            Some(folder.as_str())
        };
        let saved = match edit_key {
            Some(key) => self
                .app
                .config_mut()
                .rename_saved_host(&key, &name, &host, folder),
            None => self.app.config_mut().add_saved_host(&name, &host, folder),
        };
        if let Err(msg) = saved {
            if let Some(dialog) = self.picker_remote.ssh_add.as_mut() {
                dialog.error = Some(msg);
            }
            return false;
        }
        self.picker_remote.ssh_add = None;
        #[cfg(not(test))]
        let _ = self.app.save_config();
        self.set_picker_host(&host);
        true
    }

    /// Add-dialog key dispatch (SSH picker): Tab cycles name → host →
    /// folder → name, printable keys extend the focused field, Enter
    /// saves, Esc cancels, `ctrl+t` runs the tester without saving (a
    /// plain `t` must keep typing into the fields). Returns true when
    /// the dialog owned the key.
    pub(crate) fn ssh_add_key(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        ctrl: bool,
        platform: bool,
    ) -> bool {
        if self.picker_remote.ssh_add.is_none() {
            return false;
        }
        self.poll_ssh_add_test();
        match key {
            "escape" => {
                self.cancel_ssh_add();
            }
            "tab" => {
                if let Some(dialog) = self.picker_remote.ssh_add.as_mut() {
                    dialog.focus = (dialog.focus + 1) % 3;
                }
            }
            "enter" => {
                self.save_ssh_add();
            }
            "backspace" => {
                if let Some(dialog) = self.picker_remote.ssh_add.as_mut() {
                    let field = match dialog.focus {
                        0 => &mut dialog.name_input,
                        1 => &mut dialog.host_input,
                        _ => &mut dialog.folder_input,
                    };
                    field.pop();
                }
            }
            _ => {
                if ctrl || platform {
                    // `ctrl+t` runs the tester without saving.
                    if ctrl && key == "t" {
                        self.test_ssh_add();
                    }
                    return true;
                }
                if let Some(c) = key_char {
                    if c.chars().count() == 1 {
                        if let Some(dialog) = self.picker_remote.ssh_add.as_mut() {
                            let field = match dialog.focus {
                                0 => &mut dialog.name_input,
                                1 => &mut dialog.host_input,
                                _ => &mut dialog.folder_input,
                            };
                            field.push_str(c);
                        }
                    }
                }
            }
        }
        true
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
        self.poll_remote_listing();
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
    /// typing axis (folder → machine → CLI), everything else forwards to
    /// the framework-free [`crate::launch::LaunchPicker::key`] state
    /// machine — except on the machine axis, which owns every key
    /// (printables extend the host input, Up/Down steps the machine
    /// dropdown, Enter picks the highlighted row, Delete removes it and
    /// `ctrl+e` edits it when it is a saved machine) and never reaches
    /// the core. The SSH add dialog owns every key while open. Remote Up/Down
    /// on the folder axis steps the folder browser (falling back to the
    /// host's folder recents while the browser is empty). Returns true
    /// when the picker owned the key.
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
        self.poll_remote_listing();
        // The SSH add dialog owns every key while open (Tab cycles its
        // fields, Enter saves, Esc cancels).
        if self.picker_remote.ssh_add.is_some() {
            return self.ssh_add_key(key, key_char, ctrl, platform);
        }
        // The machine axis owns every key while focused: printable keys
        // extend the host input and nothing reaches the core picker.
        if self.picker_remote.host_focus {
            match key {
                "enter" => {
                    // Add remote machine... opens the dialog (the picker
                    // stays open); any other row confirms the launch.
                    if !self.commit_machine_dropdown() {
                        self.accept_launch_picker();
                    }
                }
                "escape" => {
                    if self.picker_remote.machine_open {
                        // First Esc only folds the dropdown (the typed
                        // host is kept); a second Esc cancels.
                        self.picker_remote.machine_open = false;
                    } else {
                        self.cancel_launch_picker();
                    }
                }
                "tab" => {
                    self.cycle_picker_axis();
                }
                "backspace" => {
                    self.picker_remote.host_input.pop();
                }
                "delete" => {
                    // Forward Delete on an open dropdown removes the
                    // highlighted saved machine (This machine / Add are
                    // inert); closed, the no-op below used to own it.
                    self.delete_highlighted_saved_host();
                }
                "up" | "down" => {
                    self.step_machine_dropdown(key == "down");
                }
                "left" | "right" => {
                    // No horizontal motion on the machine axis: consumed.
                }
                _ => {
                    if ctrl || platform {
                        // `ctrl+e` (or cmd+e) edits the highlighted saved
                        // machine without leaving the dropdown; other
                        // chords stay consumed no-ops on this axis.
                        if key == "e" {
                            self.open_highlighted_saved_host_editor();
                        }
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
                if self.picker_remote.remote_listing.is_empty() {
                    self.step_remote_recent(key == "down");
                } else {
                    self.step_remote_listing(key == "down");
                }
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

    /// Toggle the machine dropdown (clicking the machine value row):
    /// opening preselects the matching row, closing keeps the host.
    pub(crate) fn toggle_machine_dropdown(&mut self) {
        if self.launch_picker.is_none() {
            return;
        }
        if self.picker_remote.machine_open {
            self.picker_remote.machine_open = false;
        } else {
            self.picker_remote.host_focus = true;
            if let Some(picker) = self.launch_picker.as_mut() {
                picker.folder_focus = false;
            }
            self.open_machine_dropdown();
        }
    }

    /// Click one machine-dropdown row (in-place screen): This machine
    /// clears the host, a saved row dials that host, Add opens the
    /// SSH add dialog.
    pub(crate) fn pick_machine_option(&mut self, index: usize) {
        if self.launch_picker.is_none() {
            return;
        }
        let options = self.machine_options();
        let Some(opt) = options.get(index).cloned() else {
            return;
        };
        self.picker_remote.machine_cursor = index;
        self.picker_remote.machine_open = false;
        match opt {
            MachineOption::ThisMachine => self.set_picker_host(""),
            MachineOption::Saved(i) => {
                if let Some(host) = self.app.config_saved_hosts().get(i).map(|h| h.host.clone()) {
                    self.set_picker_host(&host);
                }
            }
            MachineOption::AddNew => self.open_ssh_add(),
        }
    }

    /// Click one CLI row (in-place screen): moves the cursor without
    /// confirming, so the preview updates before Enter.
    pub(crate) fn select_picker_cli(&mut self, index: usize) {
        if let Some(picker) = self.launch_picker.as_mut() {
            if index < picker.clis.len() {
                picker.cli_cursor = index;
            }
        }
    }

    /// Cycle the yolo tri-state (in-place screen row / `y` key).
    pub(crate) fn cycle_picker_yolo(&mut self) {
        if let Some(picker) = self.launch_picker.as_mut() {
            picker.yolo = picker.yolo.cycle();
        }
    }

    /// Click one folder-browser directory (in-place screen): highlights
    /// it and picks it into the folder input.
    pub(crate) fn pick_remote_listing(&mut self, index: usize) {
        if index < self.picker_remote.remote_listing.len() {
            self.picker_remote.remote_listing_cursor = index;
            self.confirm_remote_listing();
        }
    }

    /// Rows for the in-place screen machine dropdown: each option plus
    /// its human label, in highlight order.
    pub(crate) fn machine_dropdown_rows(&self) -> Vec<(MachineOption, String)> {
        let options = self.machine_options();
        options
            .into_iter()
            .map(|o| {
                let label = self.machine_option_label(&o);
                (o, label)
            })
            .collect()
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
        // The blank folder inherits the host default first (the MRU
        // head), so stepping starts from there, MRU-first.
        assert_eq!(
            view.launch_picker.as_ref().unwrap().folder_input,
            "/srv/web"
        );
        view.picker_key("down", None, false, false);
        assert_eq!(
            view.launch_picker.as_ref().unwrap().folder_input,
            "/srv/api"
        );
        view.picker_key("down", None, false, false);
        assert_eq!(
            view.launch_picker.as_ref().unwrap().folder_input,
            "/srv/web"
        );
        view.picker_key("up", None, false, false);
        assert_eq!(
            view.launch_picker.as_ref().unwrap().folder_input,
            "/srv/api"
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

    #[test]
    fn machine_placeholder_names_user_at_hostname() {
        assert!(
            super::MACHINE_PLACEHOLDER.contains("user@hostname"),
            "placeholder shows the shape: {}",
            super::MACHINE_PLACEHOLDER
        );
        // An example host rides along so the shape is concrete.
        assert!(
            super::MACHINE_PLACEHOLDER.contains('@')
                && super::MACHINE_PLACEHOLDER.len() > "user@hostname".len(),
            "placeholder carries an example: {}",
            super::MACHINE_PLACEHOLDER
        );
    }

    #[test]
    fn machine_dropdown_lists_this_machine_saved_hosts_and_add() {
        let mut view = test_shell();
        view.app
            .config_mut()
            .add_saved_host("prod box", "dev@mybox", Some("/srv/api"))
            .unwrap();
        view.open_launch_picker();
        let rows = view.machine_dropdown_rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, super::MachineOption::ThisMachine);
        assert_eq!(rows[0].1, super::THIS_MACHINE_LABEL);
        assert_eq!(rows[1].0, super::MachineOption::Saved(0));
        assert!(
            rows[1].1.contains("prod box") && rows[1].1.contains("dev@mybox"),
            "saved row names the host: {}",
            rows[1].1
        );
        assert_eq!(rows[2].0, super::MachineOption::AddNew);
        assert_eq!(rows[2].1, super::ADD_REMOTE_LABEL);
    }

    #[test]
    fn machine_dropdown_up_down_previews_saved_hosts() {
        let mut view = test_shell();
        view.app
            .config_mut()
            .add_saved_host("prod box", "dev@mybox", Some("/srv/api"))
            .unwrap();
        view.open_launch_picker();
        view.picker_key("tab", None, false, false); // folder → machine
        assert!(view.picker_remote.machine_open);
        assert_eq!(view.picker_remote.machine_cursor, 0); // This machine
        view.picker_key("down", None, false, false); // → saved host
        assert_eq!(view.picker_remote.host_input, "dev@mybox");
        assert!(view.picker_is_remote());
        view.picker_key("down", None, false, false); // → Add (stays put)
        assert_eq!(view.picker_remote.host_input, "dev@mybox");
        view.picker_key("up", None, false, false); // → saved host again
        assert_eq!(view.picker_remote.host_input, "dev@mybox");
        view.picker_key("up", None, false, false); // → This machine
        assert_eq!(view.picker_remote.host_input, "");
        assert!(!view.picker_is_remote());
    }

    #[test]
    fn machine_dropdown_add_opens_the_ssh_dialog() {
        let mut view = open_picker();
        view.picker_key("tab", None, false, false); // folder → machine
                                                    // Step down to Add remote machine... (no saved hosts: index 1).
        view.picker_key("down", None, false, false);
        view.picker_key("enter", None, false, false);
        assert!(view.picker_open(), "dialog opens over the picker");
        assert!(view.picker_remote.ssh_add.is_some());
        assert!(view.app.take_pending_spawn().is_none());
    }

    #[test]
    fn machine_dropdown_delete_removes_the_saved_row() {
        let mut view = test_shell();
        view.app
            .config_mut()
            .add_saved_host("prod box", "dev@mybox", Some("/srv/api"))
            .unwrap();
        view.open_launch_picker();
        view.picker_key("tab", None, false, false); // folder → machine
        view.picker_key("down", None, false, false); // → saved host (dials it)
        assert_eq!(view.picker_remote.host_input, "dev@mybox");
        view.picker_key("delete", None, false, false);
        assert!(view.app.config_saved_hosts().is_empty(), "row is gone");
        assert_eq!(
            view.picker_remote.host_input, "",
            "typed host falls back to local"
        );
        assert!(!view.picker_is_remote());
        assert!(view.picker_remote.machine_open, "dropdown stays open");
        assert_eq!(view.machine_options().len(), 2, "This machine + Add only");
        // Delete on This machine / Add is inert (never an error).
        view.picker_key("delete", None, false, false);
        assert!(view.picker_open());
        view.picker_key("down", None, false, false); // → Add remote machine...
        view.picker_key("delete", None, false, false);
        assert!(view.picker_open());
        assert!(view.picker_remote.ssh_add.is_none(), "Add never deletes");
    }

    #[test]
    fn machine_dropdown_delete_with_closed_dropdown_is_inert() {
        let mut view = test_shell();
        view.app
            .config_mut()
            .add_saved_host("prod box", "dev@mybox", None)
            .unwrap();
        view.open_launch_picker();
        view.picker_key("delete", None, false, false); // folder axis: no-op
        assert_eq!(view.app.config_saved_hosts().len(), 1);
        assert!(!view.picker_remote.machine_open);
    }

    #[test]
    fn machine_dropdown_ctrl_e_edits_the_saved_row_in_place() {
        let mut view = test_shell();
        view.app
            .config_mut()
            .add_saved_host("prod box", "dev@mybox", Some("/srv/api"))
            .unwrap();
        view.open_launch_picker();
        view.picker_key("tab", None, false, false); // folder → machine
        view.picker_key("down", None, false, false); // → saved host
        view.picker_key("e", None, true, false); // ctrl+e edits
        let dialog = view
            .picker_remote
            .ssh_add
            .as_ref()
            .expect("edit dialog opens");
        assert_eq!(dialog.name_input, "prod box", "name prefills");
        assert_eq!(dialog.host_input, "dev@mybox", "host prefills");
        assert_eq!(dialog.folder_input, "/srv/api", "folder prefills");
        assert_eq!(dialog.edit_key.as_deref(), Some("dev@mybox"));
        // Fix the typo in the target: renames, never duplicates.
        {
            let dialog = view.picker_remote.ssh_add.as_mut().unwrap();
            dialog.host_input = "dev@prodbox".to_string();
        }
        assert!(view.save_ssh_add());
        let saved = view.app.config_saved_hosts();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "prod box");
        assert_eq!(saved[0].host, "dev@prodbox");
        assert_eq!(saved[0].default_folder.as_deref(), Some("/srv/api"));
        assert_eq!(
            view.picker_remote.host_input, "dev@prodbox",
            "dials the fixed host"
        );
        // ctrl+e on This machine is inert (nothing to edit).
        view.picker_key("tab", None, false, false); // → folder
        view.picker_key("tab", None, false, false); // → machine (on the saved row)
        view.picker_key("up", None, false, false); // → This machine
        assert_eq!(view.picker_remote.host_input, "");
        view.picker_key("e", None, true, false);
        assert!(view.picker_remote.ssh_add.is_none());
    }

    #[test]
    fn ssh_edit_save_onto_another_saved_host_refuses_in_dialog() {
        let mut view = open_picker();
        view.app
            .config_mut()
            .add_saved_host("prod box", "dev@mybox", None)
            .unwrap();
        view.app
            .config_mut()
            .add_saved_host("staging", "staging-box", None)
            .unwrap();
        view.open_ssh_add_for_edit(0);
        {
            let dialog = view.picker_remote.ssh_add.as_mut().unwrap();
            dialog.host_input = "staging-box".to_string();
        }
        assert!(!view.save_ssh_add(), "collision refuses, no silent merge");
        let dialog = view.picker_remote.ssh_add.as_ref().unwrap();
        assert!(dialog
            .error
            .as_deref()
            .is_some_and(|e| e.contains("already saved")));
        assert_eq!(view.app.config_saved_hosts().len(), 2);
    }

    #[test]
    fn ssh_add_save_requires_name_and_host() {
        let mut view = open_picker();
        view.open_ssh_add();
        assert!(!view.save_ssh_add());
        let dialog = view.picker_remote.ssh_add.as_ref().unwrap();
        assert!(
            dialog.error.as_deref() == Some("name and host are required"),
            "refusal is in-dialog: {:?}",
            dialog.error
        );
        assert!(view.picker_open());
    }

    #[test]
    fn ssh_add_save_persists_and_dials_the_host() {
        let mut view = open_picker();
        view.open_ssh_add();
        {
            let dialog = view.picker_remote.ssh_add.as_mut().unwrap();
            dialog.name_input = "prod box".to_string();
            dialog.host_input = "dev@mybox".to_string();
            dialog.folder_input = "/srv/api".to_string();
        }
        // Tester arrival (injected): reachable with a catalog.
        view.test_ssh_add();
        assert!(view.apply_ssh_add_test(
            "dev@mybox",
            crate::launch::reachability_from_probe(Ok(
                "muse:/bin/muse\nclaude:\nopencode:\ncodex:\n"
            ))
        ));
        let label = view.ssh_add_test_label().expect("tester copy");
        assert!(label.starts_with("reachable"), "{label}");
        assert!(label.contains("muse"), "names the detected CLIs: {label}");
        assert!(view.save_ssh_add());
        assert!(view.picker_remote.ssh_add.is_none());
        assert_eq!(view.picker_remote.host_input, "dev@mybox");
        // Backed by the core saved-hosts store (name, host, folder).
        let saved = view.app.config_saved_hosts();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "prod box");
        assert_eq!(saved[0].host, "dev@mybox");
        assert_eq!(saved[0].default_folder.as_deref(), Some("/srv/api"));
    }

    #[test]
    fn ssh_add_tester_reports_unreachable_in_words() {
        let mut view = open_picker();
        view.open_ssh_add();
        view.picker_remote.ssh_add.as_mut().unwrap().host_input = "gone-box".to_string();
        assert!(view.apply_ssh_add_test(
            "gone-box",
            crate::launch::reachability_from_probe(Err(
                crate::launch::SshFailureReason::Unreachable
            ))
        ));
        assert_eq!(
            view.ssh_add_test_label().as_deref(),
            Some("unreachable — check the host and keys")
        );
        // Stale arrivals (host edited since) never overwrite the result.
        view.picker_remote.ssh_add.as_mut().unwrap().host_input = "other-box".to_string();
        assert!(!view.apply_ssh_add_test(
            "gone-box",
            crate::launch::reachability_from_probe(Ok("muse:/bin/muse\n"))
        ));
        assert_eq!(
            view.ssh_add_test_label().as_deref(),
            Some("unreachable — check the host and keys")
        );
    }

    #[test]
    fn ssh_add_tester_reports_each_reason_in_words() {
        // Every core reason renders verbatim in the dialog (keyboard and
        // non-color parity: the copy is the signal, not styling).
        let mut view = open_picker();
        view.open_ssh_add();
        view.picker_remote.ssh_add.as_mut().unwrap().host_input = "mybox".to_string();
        for reason in [
            crate::launch::SshFailureReason::UnknownHost,
            crate::launch::SshFailureReason::Timeout,
            crate::launch::SshFailureReason::AuthFailure,
            crate::launch::SshFailureReason::MissingSshBinary,
            crate::launch::SshFailureReason::RemoteShellError,
            crate::launch::SshFailureReason::Unreachable,
        ] {
            assert!(view
                .apply_ssh_add_test("mybox", crate::launch::reachability_from_probe(Err(reason))));
            assert_eq!(
                view.ssh_add_test_label().as_deref(),
                Some(reason.message()),
                "{reason:?} renders its core copy"
            );
        }
    }

    #[test]
    fn ssh_add_blank_host_degrades_to_empty_host_without_spawn() {
        let mut view = open_picker();
        view.open_ssh_add();
        view.test_ssh_add(); // blank host: no thread, empty-host
        assert_eq!(
            view.ssh_add_test_label().as_deref(),
            Some(crate::launch::SshFailureReason::EmptyHost.message())
        );
    }

    #[test]
    fn blank_folder_inherits_the_host_default_folder() {
        let mut view = test_shell();
        view.app
            .config_mut()
            .add_saved_host("prod box", "dev@mybox", Some("/srv/pinned"))
            .unwrap();
        view.open_launch_picker();
        // Folder starts blank (no local default configured in tests).
        assert!(view.launch_picker.as_ref().unwrap().folder_input.is_empty());
        view.set_picker_host("dev@mybox");
        // Saved default wins (see the core fallback chain).
        assert_eq!(
            view.launch_picker.as_ref().unwrap().folder_input,
            "/srv/pinned"
        );
        // An explicit folder is never clobbered by the default.
        view.set_picker_host("");
        if let Some(picker) = view.launch_picker.as_mut() {
            picker.folder_input = "/typed".to_string();
        }
        view.set_picker_host("dev@mybox");
        assert_eq!(view.launch_picker.as_ref().unwrap().folder_input, "/typed");
    }

    #[test]
    fn folder_browser_lists_dirs_and_picks_into_the_folder() {
        let mut view = open_picker();
        view.set_picker_host("mybox");
        let entries = crate::launch::parse_remote_list_output("api/\nweb/\nnotes.txt\n");
        assert_eq!(entries.len(), 3);
        view.set_remote_listing(entries);
        assert_eq!(view.picker_remote.remote_listing.len(), 3);
        // Step to the second directory and pick it: the folder input
        // extends, staying fully editable afterwards.
        view.step_remote_listing(true);
        assert_eq!(view.picker_remote.remote_listing_cursor, 1);
        assert!(view.confirm_remote_listing());
        let folder = view.launch_picker.as_ref().unwrap().folder_input.clone();
        assert!(folder.ends_with("/web"), "{folder}");
        // Files are inert: picking a file row is a no-op.
        view.set_remote_listing(crate::launch::parse_remote_list_output("notes.txt\n"));
        assert!(!view.confirm_remote_listing());
        assert_eq!(view.launch_picker.as_ref().unwrap().folder_input, folder);
    }

    #[test]
    fn folder_browser_stale_arrivals_are_dropped() {
        let mut view = open_picker();
        view.set_picker_host("box-a");
        if let Some(picker) = view.launch_picker.as_mut() {
            picker.folder_input = "/srv/a".to_string();
        }
        assert!(view.apply_remote_listing(
            "box-a",
            "/srv/a",
            crate::launch::parse_remote_list_output("a/\n")
        ));
        // The host moved on: the late arrival for box-a is refused.
        view.set_picker_host("box-b");
        assert!(!view.apply_remote_listing(
            "box-a",
            "/srv/a",
            crate::launch::parse_remote_list_output("stale/\n")
        ));
    }

    #[test]
    fn esc_folds_the_dropdown_before_cancelling() {
        let mut view = open_picker();
        view.picker_key("tab", None, false, false); // folder → machine
        assert!(view.picker_remote.machine_open);
        view.picker_key("escape", None, false, false); // folds, stays open
        assert!(view.picker_open());
        assert!(!view.picker_remote.machine_open);
        view.picker_key("escape", None, false, false); // cancels
        assert!(!view.picker_open());
    }

    #[test]
    fn dialog_keys_type_save_and_cancel() {
        let mut view = open_picker();
        view.open_ssh_add();
        for c in ["p", "r", "o", "d"] {
            view.picker_key(c, Some(c), false, false);
        }
        assert_eq!(
            view.picker_remote.ssh_add.as_ref().unwrap().name_input,
            "prod"
        );
        view.picker_key("tab", None, false, false); // name → host
        for c in ["m", "y", "b", "o", "x"] {
            view.picker_key(c, Some(c), false, false);
        }
        view.picker_key("enter", None, false, false); // save
        assert!(view.picker_remote.ssh_add.is_none());
        assert_eq!(view.picker_remote.host_input, "mybox");
        assert_eq!(view.app.config_saved_hosts()[0].name, "prod");
    }
}
