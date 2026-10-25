//! User configuration: per-agent startup flags, loaded from disk.
//!
//! Issue #33: `muse --yolo`, `claude --dangerously-skip-permissions`,
//! and friends. The file is JSON at
//! `~/.config/staap/config.json` (overridable for tests via
//! `STAAP_CONFIG`):
//!
//! ```json
//! { "agents": { "muse": { "extra_args": ["--yolo"] } } }
//! ```
//!
//! Missing file, unreadable file, or unknown keys all fall back to the
//! default (no extra args): a bad config never blocks startup. Keys are
//! matched against the spawned program name, so any supported agent can
//! carry its own flags.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One saved SSH host for the picker (SSH picker): a display name, the
/// SSH target as typed (`mybox`, `dev@jump-box`, an ssh-config alias),
/// and an optional per-host default folder. No secrets ever: auth rides
/// the user's own ssh setup (config file, agent, interactive prompts),
/// so only `host` + `default_folder` persist here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedHost {
    /// Display name shown in the picker (e.g. `"prod jump box"`).
    pub name: String,
    /// SSH target as typed for `ssh`: `user@` prefixes and ssh-config
    /// aliases keep resolving through the system ssh config untouched.
    pub host: String,
    /// Default folder on this host (`None`/blank = fall through to the
    /// host MRU, then the remote home — see
    /// [`Config::remote_default_folder_for`]).
    #[serde(default)]
    pub default_folder: Option<String>,
}

/// Ceiling for the saved-host list (SSH picker): same bounded-growth
/// rule as the folder MRUs. Updates to an already-saved host always
/// succeed; only brand-new hosts beyond the cap refuse (with a message
/// naming the fix), so a save never silently drops an existing entry.
pub const MAX_SAVED_HOSTS: usize = 50;

/// Startup flags for one agent binary (keyed by program name, e.g.
/// `"muse"` or `"claude"`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Extra CLI flags appended to every spawn of this agent.
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// Yolo default for this agent (2D launch): when true, the canonical
    /// yolo flag for the CLI (see `launch::yolo_flag_for`) rides every
    /// spawn unless a per-run picker choice forces it off. Off unless
    /// explicitly set: destructive flags are opt-in, never silent.
    #[serde(default)]
    pub yolo: bool,
}

/// Whole-file user configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Per-agent startup flags, keyed by program name.
    #[serde(default)]
    pub agents: HashMap<String, AgentConfig>,
    /// App theme choice (issue #34).
    #[serde(default)]
    pub theme: ThemePreference,
    /// Terminal-pane font (issue #35).
    #[serde(default)]
    pub terminal: TerminalConfig,
    /// Sessions-panel width in pixels (issue #29: `[`/`]` keys adjust
    /// and persist it; narrow viewports still collapse the panel).
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    /// History section expansion (issue #55): toggled by the History
    /// header click, `h`, or Enter on a hidden history selection, and
    /// restored on launch. Absent in older files means collapsed.
    #[serde(default)]
    pub history_expanded: bool,
    /// Default CLI for the 2D new-session picker (2D launch): preselected
    /// when set and supported, otherwise the last-used CLI, otherwise the
    /// first autodetected binary. Absent means pure autodetect + memory.
    #[serde(default)]
    pub default_cli: Option<String>,
    /// Default folder for the 2D picker (`None`/blank inherits the app
    /// directory, the historic behavior). Prefills the folder axis.
    #[serde(default)]
    pub default_cwd: Option<String>,
    /// Last-used CLI id (2D launch): updated on every confirmed spawn so
    /// `n` repeats the last combination. Survives restarts like the other
    /// comfort settings.
    #[serde(default)]
    pub last_cli: Option<String>,
    /// Most-recently-used folders for the picker (2D launch): MRU-first,
    /// capped at `launch::MAX_RECENT_FOLDERS`, missing dirs dropped on
    /// display (never blocking launch on a stale entry).
    #[serde(default)]
    pub recent_folders: Vec<String>,
    /// Last remote SSH host typed in the picker (2D launch): preselects
    /// the host axis like `last_cli` does for CLIs. Host names only —
    /// passwords, keys, and passphrases never touch this file (auth rides
    /// the user's own ssh setup: config, agent, interactive prompts).
    #[serde(default)]
    pub last_host: Option<String>,
    /// Most-recently-used remote folders per SSH host (2D launch):
    /// host → MRU-first remote dirs, each capped at
    /// `launch::MAX_RECENT_FOLDERS`. Same bounded-display rule as
    /// `recent_folders`, keyed by host so `mybox:/srv/api` and
    /// `other:/srv/api` never mix.
    #[serde(default)]
    pub remote_folders: HashMap<String, Vec<String>>,
    /// Saved SSH hosts for the picker (SSH picker): display name + SSH
    /// target + optional per-host default folder, in saved order. Hosts
    /// and folders only — auth stays in the user's own ssh setup, so
    /// passwords, keys, and passphrases never touch this file.
    #[serde(default)]
    pub saved_hosts: Vec<SavedHost>,
}

/// Default terminal-pane font: OFL-licensed, full box-drawing + block
/// coverage, Nerd Font symbols baked in (issue #35).
pub fn default_terminal_font() -> String {
    "JetBrainsMono Nerd Font".to_string()
}

/// Default terminal font size in points (matches the historic 13.0).
pub fn default_terminal_font_size() -> f32 {
    13.0
}

/// Default sessions-panel width in pixels (issue #29: `[`/`]` keys).
pub fn default_sidebar_width() -> f32 {
    264.0
}

/// Default fallback chain behind the primary font (issue #35): emoji,
/// then CJK monospace fallbacks, then system monospace last resort.
/// A user `font_family` override replaces the head of this chain, never
/// the emoji/CJK tail.
pub fn default_fallback_fonts() -> Vec<String> {
    vec![
        "Apple Color Emoji".to_string(),
        "Noto Color Emoji".to_string(),
        "Noto Sans Mono CJK SC".to_string(),
        "Noto Sans Mono CJK JP".to_string(),
        "Hiragino Kaku Gothic ProN".to_string(),
        "PingFang SC".to_string(),
        "Menlo".to_string(),
        "DejaVu Sans Mono".to_string(),
    ]
}

/// Terminal-pane font setting (issue #35). No `Eq`: `font_size` is a
/// float (config equality is still exact via `PartialEq` in tests).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalConfig {
    /// Primary monospace font family. Default is the recommended
    /// JetBrainsMono Nerd Font (see README for install); set this to any
    /// installed patched font (FiraCode, Hack, Iosevka, …) to override.
    #[serde(default = "default_terminal_font")]
    pub font_family: String,
    /// Font size in points.
    #[serde(default = "default_terminal_font_size")]
    pub font_size: f32,
    /// Ordered fallback families behind `font_family`.
    #[serde(default = "default_fallback_fonts")]
    pub fallback_fonts: Vec<String>,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            font_family: default_terminal_font(),
            font_size: default_terminal_font_size(),
            fallback_fonts: default_fallback_fonts(),
        }
    }
}

impl TerminalConfig {
    /// Ordered font stack: the (possibly overridden) primary first, then
    /// the emoji/CJK/system fallbacks with any duplicate of the primary
    /// removed. The override replaces the head, never the tail.
    pub fn font_stack(&self) -> Vec<String> {
        let mut stack = vec![self.font_family.clone()];
        for fallback in &self.fallback_fonts {
            if *fallback != self.font_family && !stack.contains(fallback) {
                stack.push(fallback.clone());
            }
        }
        stack
    }
}

/// App theme choice (issue #34): explicit dark/light, or follow the OS.
/// Serialized lowercase (`"dark"`, `"light"`, `"system"`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    /// Follow the OS appearance (dark when unknown).
    #[default]
    System,
    Dark,
    Light,
}

impl ThemePreference {
    /// Next choice in the `t`-key cycle: dark → light → system → dark.
    pub fn cycle(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::System,
            Self::System => Self::Dark,
        }
    }

    /// Config-file label (`t`-key status flash, README).
    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::System => "system",
        }
    }

    /// Resolve to a concrete theme. `System` follows `appearance`;
    /// `Unknown` (headless/tests) resolves system to dark, preserving
    /// the pre-#34 default.
    pub fn resolve(self, appearance: OsAppearance) -> EffectiveTheme {
        match self {
            Self::Dark => EffectiveTheme::Dark,
            Self::Light => EffectiveTheme::Light,
            Self::System => match appearance {
                OsAppearance::Dark => EffectiveTheme::Dark,
                OsAppearance::Light => EffectiveTheme::Light,
                OsAppearance::Unknown => EffectiveTheme::Dark,
            },
        }
    }

    /// Convenience for shells and FFI bindings: true when
    /// [`Self::resolve`] yields dark.
    pub fn is_dark(self, appearance: OsAppearance) -> bool {
        self.resolve(appearance).is_dark()
    }
}

/// OS appearance in framework-free form (volatility shield: the core never
/// names framework appearance types; the shell maps its live appearance
/// onto this in `crate::gui::theme::theme_mode_for`, and native shells
/// map their own appearance value the same way).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OsAppearance {
    Dark,
    Light,
    /// Unknown / headless (tests, no window yet).
    #[default]
    Unknown,
}

/// Resolved theme in framework-free form: what a shell should actually
/// render. Shells match on this (or [`EffectiveTheme::is_dark`]) instead
/// of touching component theme types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveTheme {
    Dark,
    Light,
}

impl EffectiveTheme {
    /// Convenience for FFI/SwiftUI (`ColorScheme`) bindings: dark or not.
    pub fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            agents: HashMap::new(),
            theme: ThemePreference::default(),
            terminal: TerminalConfig::default(),
            sidebar_width: default_sidebar_width(),
            history_expanded: false,
            default_cli: None,
            default_cwd: None,
            last_cli: None,
            recent_folders: Vec::new(),
            last_host: None,
            remote_folders: HashMap::new(),
            saved_hosts: Vec::new(),
        }
    }
}

impl Config {
    /// Path of the config file: `$STAAP_CONFIG` when set (tests),
    /// otherwise `~/.config/staap/config.json`.
    pub fn config_path() -> PathBuf {
        if let Ok(path) = std::env::var("STAAP_CONFIG") {
            return PathBuf::from(path);
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".config")
            .join("staap")
            .join("config.json")
    }

    /// Load from [`Self::config_path`]; any failure (missing file,
    /// unreadable, malformed JSON) yields the default config.
    pub fn load() -> Self {
        Self::load_from(&Self::config_path())
    }

    /// Load from an explicit path; same forgiving policy as [`Self::load`].
    pub fn load_from(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Persist to [`Self::config_path`], creating parent directories.
    /// Returns the IO/serialization error so callers can surface it.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text)?;
        Ok(())
    }

    /// Extra startup flags for `agent` (the spawned program name).
    /// Unknown agents get none.
    pub fn extra_args_for(&self, agent: &str) -> Vec<String> {
        self.agents
            .get(agent)
            .map(|a| a.extra_args.clone())
            .unwrap_or_default()
    }

    /// Yolo default for `agent` (2D launch): opt-in per agent, off unless
    /// explicitly set. Unknown agents default to safe.
    pub fn yolo_default_for(&self, agent: &str) -> bool {
        self.agents.get(agent).is_some_and(|a| a.yolo)
    }

    /// Record a confirmed launch (2D launch): last-used CLI + folder MRU.
    /// Called by the spawn path so `n` repeats the last combination.
    pub fn note_launch(&mut self, cli: &str, cwd: Option<&str>) {
        if crate::launch::SUPPORTED_CLIS.contains(&cli) {
            self.last_cli = Some(cli.to_string());
        }
        if let Some(dir) = cwd {
            crate::launch::push_recent_folder(&mut self.recent_folders, dir);
        }
    }

    /// Record a confirmed remote launch (2D launch): last-used host plus
    /// the per-host folder MRU. Host keys are trimmed (so ` mybox ` and
    /// `mybox` share one memory); blank hosts record nothing. Called by
    /// the spawn path alongside [`Self::note_launch`].
    pub fn note_remote_launch(&mut self, host: &str, remote_cwd: Option<&str>) {
        let host = host.trim();
        if host.is_empty() {
            return;
        }
        self.last_host = Some(host.to_string());
        if let Some(dir) = remote_cwd {
            let entry = self.remote_folders.entry(host.to_string()).or_default();
            crate::launch::push_recent_folder(entry, dir);
        }
    }

    /// MRU-first remote folders for `host` (2D launch): what the picker
    /// offers on the folder axis once a host is picked. Empty for unknown
    /// or blank hosts — never an error, never a network touch.
    pub fn remote_recents_for(&self, host: &str) -> &[String] {
        let host = host.trim();
        if host.is_empty() {
            return &[];
        }
        self.remote_folders
            .get(host)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Saved SSH hosts in saved order (SSH picker). What the picker
    /// lists; shells render it and forward add/remove intents here.
    pub fn saved_hosts(&self) -> &[SavedHost] {
        &self.saved_hosts
    }

    /// Save an SSH host for the picker (SSH picker): `name` is the display
    /// label, `host` the SSH target (`user@host` or an ssh-config alias),
    /// `default_folder` the per-host default (`None`/blank clears it). A
    /// host already saved (same trimmed `host`) updates in place and never
    /// counts against the cap; brand-new hosts past [`MAX_SAVED_HOSTS`]
    /// refuse with a message naming the fix. Blank names/hosts refuse the
    /// same way. No secret parameter exists by design: auth stays in the
    /// system ssh config, never in this file.
    pub fn add_saved_host(
        &mut self,
        name: &str,
        host: &str,
        default_folder: Option<&str>,
    ) -> Result<(), String> {
        let name = name.trim();
        let host = host.trim();
        if name.is_empty() {
            return Err("set a display name first".to_string());
        }
        if host.is_empty() {
            return Err("set an SSH host first".to_string());
        }
        let folder = default_folder
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string);
        if let Some(existing) = self.saved_hosts.iter_mut().find(|h| h.host == host) {
            existing.name = name.to_string();
            existing.default_folder = folder;
            return Ok(());
        }
        if self.saved_hosts.len() >= MAX_SAVED_HOSTS {
            return Err(format!(
                "at {MAX_SAVED_HOSTS} saved hosts — remove one first"
            ));
        }
        self.saved_hosts.push(SavedHost {
            name: name.to_string(),
            host: host.to_string(),
            default_folder: folder,
        });
        Ok(())
    }

    /// Remove a saved SSH host by `host` or display `name` (SSH picker).
    /// True when an entry matched (either key compares trimmed); false
    /// when nothing was saved under `key` — shells surface that as a
    /// fail-visible "no such host" flash, never a silent no-op.
    pub fn remove_saved_host(&mut self, key: &str) -> bool {
        let key = key.trim();
        if key.is_empty() {
            return false;
        }
        let before = self.saved_hosts.len();
        self.saved_hosts.retain(|h| h.host != key && h.name != key);
        self.saved_hosts.len() != before
    }

    /// Replace the per-host default folder of an already-saved host (SSH
    /// picker): `None`/blank clears it back to the MRU/home fallback.
    /// False when `host` is not saved — shells surface that like
    /// [`Self::remove_saved_host`]; use [`Self::add_saved_host`] for new
    /// hosts instead of growing the list here.
    pub fn set_host_default_folder(&mut self, host: &str, folder: Option<&str>) -> bool {
        let host = host.trim();
        match self.saved_hosts.iter_mut().find(|h| h.host == host) {
            Some(saved) => {
                saved.default_folder = folder
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .map(str::to_string);
                true
            }
            None => false,
        }
    }

    /// Default remote folder for `host` (SSH picker) through the shared
    /// fallback chain (see `launch::resolve_remote_default_folder`): the
    /// saved host default wins, then the host MRU head, then `None` (the
    /// remote login directory / home). Blank hosts resolve to home —
    /// never an error, never a network touch.
    pub fn remote_default_folder_for(&self, host: &str) -> Option<String> {
        let key = host.trim();
        if key.is_empty() {
            return None;
        }
        let saved = self
            .saved_hosts
            .iter()
            .find(|h| h.host == key)
            .and_then(|h| h.default_folder.as_deref());
        let mru = self
            .remote_folders
            .get(key)
            .and_then(|dirs| dirs.first().map(String::as_str));
        crate::launch::resolve_remote_default_folder(saved, mru)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scoped_env(path: &std::path::Path) -> Option<String> {
        // Single test in this module touches STAAP_CONFIG, so no
        // cross-test interference: nothing else reads this variable.
        let old = std::env::var("STAAP_CONFIG").ok();
        std::env::set_var("STAAP_CONFIG", path);
        old
    }

    #[test]
    fn default_config_carries_no_extra_args() {
        let cfg = Config::default();
        assert!(cfg.extra_args_for("muse").is_empty());
        assert!(cfg.extra_args_for("claude").is_empty());
        assert_eq!(cfg.theme, ThemePreference::System);
        // 2D launch additions default safe/empty: no yolo, no default
        // CLI/folder, no last CLI, no recents.
        assert!(!cfg.yolo_default_for("muse"));
        assert!(!cfg.yolo_default_for("claude"));
        assert_eq!(cfg.default_cli, None);
        assert_eq!(cfg.default_cwd, None);
        assert_eq!(cfg.last_cli, None);
        assert!(cfg.recent_folders.is_empty());
    }

    #[test]
    fn launch_memory_tracks_cli_and_folder_mru() {
        // 2D launch: every confirmed spawn refreshes repeat-last memory
        // (CLI + folder MRU), capped and deduped like the pure helper.
        let mut cfg = Config::default();
        cfg.note_launch("claude", Some("/tmp/api"));
        assert_eq!(cfg.last_cli.as_deref(), Some("claude"));
        assert_eq!(cfg.recent_folders, vec!["/tmp/api"]);
        cfg.note_launch("claude", Some("/tmp/api"));
        assert_eq!(cfg.recent_folders, vec!["/tmp/api"]);
        // Unknown CLI ids never poison the memory.
        cfg.note_launch("future-harness", None);
        assert_eq!(cfg.last_cli.as_deref(), Some("claude"));
        // Yolo default round-trips per agent.
        cfg.agents.insert(
            "muse".to_string(),
            AgentConfig {
                extra_args: vec![],
                yolo: true,
            },
        );
        assert!(cfg.yolo_default_for("muse"));
        assert!(!cfg.yolo_default_for("claude"));
        let back: Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
        // Old files without the new keys still load (serde defaults).
        let old: Config = serde_json::from_str(r#"{"theme": "dark"}"#).unwrap();
        assert_eq!(old.default_cli, None);
        assert!(old.recent_folders.is_empty());
    }

    #[test]
    fn remote_memory_tracks_host_and_per_host_folder_mru() {
        // 2D remote launch: every confirmed spawn refreshes the last host
        // plus the per-host folder MRU — capped, deduped, host-trimmed —
        // with no secrets anywhere in the persisted file.
        let mut cfg = Config::default();
        assert_eq!(cfg.last_host, None);
        assert!(cfg.remote_recents_for("mybox").is_empty());
        cfg.note_remote_launch("  mybox ", Some("/srv/api"));
        assert_eq!(cfg.last_host.as_deref(), Some("mybox"));
        assert_eq!(cfg.remote_recents_for("mybox"), vec!["/srv/api"]);
        // Hosts never mix folders.
        cfg.note_remote_launch("other", Some("/srv/api"));
        assert_eq!(cfg.last_host.as_deref(), Some("other"));
        assert_eq!(cfg.remote_recents_for("other"), vec!["/srv/api"]);
        assert_eq!(cfg.remote_recents_for("mybox"), vec!["/srv/api"]);
        // Dedupes, ignores blanks, caps per host — and blank hosts record
        // nothing at all.
        cfg.note_remote_launch("mybox", Some("/srv/api"));
        cfg.note_remote_launch("mybox", Some("   "));
        assert_eq!(cfg.remote_recents_for("mybox"), vec!["/srv/api"]);
        cfg.note_remote_launch("   ", Some("/srv/x"));
        assert!(cfg.remote_recents_for("").is_empty());
        for i in 0..20 {
            cfg.note_remote_launch("mybox", Some(&format!("/d{i}")));
        }
        assert_eq!(
            cfg.remote_recents_for("mybox").len(),
            crate::launch::MAX_RECENT_FOLDERS
        );
        assert_eq!(cfg.remote_recents_for("mybox")[0], "/d19");
        // Round-trips; old files without the keys still load empty.
        let back: Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
        let old: Config = serde_json::from_str(r#"{"theme": "dark"}"#).unwrap();
        assert_eq!(old.last_host, None);
        assert!(old.remote_folders.is_empty());
        // The persisted file carries hosts and dirs only — never secrets.
        let text = serde_json::to_string(&cfg).unwrap().to_lowercase();
        assert!(!text.contains("password"));
        assert!(!text.contains("secret"));
        assert!(!text.contains("private_key") && !text.contains("privatekey"));
    }

    #[test]
    fn saved_hosts_crud_defaults_cap_and_no_secrets() {
        // SSH picker: the store starts empty and old files without the
        // key still load (serde default).
        let mut cfg = Config::default();
        assert!(cfg.saved_hosts().is_empty());
        let old: Config = serde_json::from_str(r#"{"theme": "dark"}"#).unwrap();
        assert!(old.saved_hosts().is_empty());
        // Add trims, stores the per-host default, and updates in place on
        // a repeated host (rename + folder replace, no duplicate row).
        cfg.add_saved_host("  prod box ", " dev@jump-box ", Some(" /srv/api "))
            .unwrap();
        cfg.add_saved_host("prod", "dev@jump-box", None).unwrap();
        assert_eq!(cfg.saved_hosts().len(), 1);
        assert_eq!(cfg.saved_hosts()[0].name, "prod");
        assert_eq!(cfg.saved_hosts()[0].host, "dev@jump-box");
        assert_eq!(cfg.saved_hosts()[0].default_folder, None);
        // Blank defaults clear; unknown hosts never grow the list here.
        assert!(!cfg.set_host_default_folder("ghost", Some("/x")));
        assert!(cfg.set_host_default_folder("dev@jump-box", Some("/srv/v2")));
        assert_eq!(
            cfg.saved_hosts()[0].default_folder.as_deref(),
            Some("/srv/v2")
        );
        assert!(cfg.set_host_default_folder("dev@jump-box", Some("   ")));
        assert_eq!(cfg.saved_hosts()[0].default_folder, None);
        // Blank names/hosts refuse with a message (fail-visible add).
        assert!(cfg.add_saved_host("  ", "h", None).is_err());
        assert!(cfg.add_saved_host("n", "  ", None).is_err());
        // Remove matches host or display name (trimmed); unknown keys and
        // blanks report false instead of silently succeeding.
        cfg.add_saved_host("staging", "staging-box", None).unwrap();
        assert!(cfg.remove_saved_host(" staging-box "));
        assert!(cfg.remove_saved_host("prod"));
        assert!(!cfg.remove_saved_host("prod"));
        assert!(!cfg.remove_saved_host("   "));
        assert!(cfg.saved_hosts().is_empty());
        // Bounded: brand-new hosts past the cap refuse, while updates to
        // an already-saved host still succeed (never silently dropped).
        for i in 0..crate::config::MAX_SAVED_HOSTS {
            cfg.add_saved_host(&format!("box {i}"), &format!("box-{i}"), None)
                .unwrap();
        }
        assert!(cfg.add_saved_host("one more", "box-new", None).is_err());
        assert!(cfg.add_saved_host("renamed", "box-0", None).is_ok());
        assert_eq!(cfg.saved_hosts().len(), crate::config::MAX_SAVED_HOSTS);
        // Round-trips; the persisted file carries names/hosts/folders
        // only — never secrets (no secret field exists on the struct, and
        // the serialized text proves it).
        let back: Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
        let text = serde_json::to_string(&cfg).unwrap().to_lowercase();
        assert!(!text.contains("password"));
        assert!(!text.contains("secret"));
        assert!(!text.contains("private_key") && !text.contains("privatekey"));
        assert!(!text.contains("passphrase"));
        // `user@` targets and aliases persist verbatim (ssh resolves them).
        let mut alias = Config::default();
        alias.add_saved_host("alias", "my-alias", None).unwrap();
        alias.add_saved_host("jump", "dev@jump-box", None).unwrap();
        let back: Config = serde_json::from_str(&serde_json::to_string(&alias).unwrap()).unwrap();
        assert_eq!(back.saved_hosts()[0].host, "my-alias");
        assert_eq!(back.saved_hosts()[1].host, "dev@jump-box");
    }

    #[test]
    fn remote_default_folder_falls_back_default_then_mru_then_home() {
        // SSH picker: saved host default wins, then the host MRU head,
        // then home (None = remote login directory). No network, ever.
        let mut cfg = Config::default();
        assert_eq!(cfg.remote_default_folder_for("mybox"), None);
        assert_eq!(cfg.remote_default_folder_for(""), None);
        assert_eq!(cfg.remote_default_folder_for("   "), None);
        cfg.note_remote_launch("mybox", Some("/srv/mru"));
        assert_eq!(
            cfg.remote_default_folder_for("mybox").as_deref(),
            Some("/srv/mru")
        );
        cfg.add_saved_host("box", "mybox", Some("/srv/pinned"))
            .unwrap();
        assert_eq!(
            cfg.remote_default_folder_for("  mybox ").as_deref(),
            Some("/srv/pinned")
        );
        // Clearing the saved default falls back to the MRU, not home.
        assert!(cfg.set_host_default_folder("mybox", None));
        assert_eq!(
            cfg.remote_default_folder_for("mybox").as_deref(),
            Some("/srv/mru")
        );
        // Hosts never mix: other hosts still resolve to home.
        assert_eq!(cfg.remote_default_folder_for("other"), None);
        // Round-trips through JSON with the rest of the config.
        let back: Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
        assert_eq!(
            back.remote_default_folder_for("mybox").as_deref(),
            Some("/srv/mru")
        );
    }

    #[test]
    fn sidebar_width_defaults_and_survives_json() {
        // Issue #29: the `[`/`]` panel width persists in the same local
        // config file as the other comfort settings.
        assert_eq!(Config::default().sidebar_width, 264.0);
        let partial: Config = serde_json::from_str(r#"{"theme": "dark"}"#).unwrap();
        assert_eq!(partial.sidebar_width, 264.0);
        let mut cfg = Config {
            sidebar_width: 300.0,
            ..Default::default()
        };
        // A font-size change (the `+`/`-` keys) survives the same trip.
        cfg.terminal.font_size = 17.0;
        let back: Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.sidebar_width, 300.0);
        assert_eq!(back.terminal.font_size, 17.0);
    }

    #[test]
    fn history_expansion_defaults_collapsed_and_survives_json() {
        // Issue #55: expansion persists in the same local config file;
        // files written before the key existed (or with it absent)
        // still load as collapsed.
        assert!(!Config::default().history_expanded);
        let partial: Config = serde_json::from_str(r#"{"theme": "dark"}"#).unwrap();
        assert!(!partial.history_expanded);
        let cfg = Config {
            history_expanded: true,
            ..Default::default()
        };
        let back: Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert!(back.history_expanded);
    }

    #[test]
    fn terminal_font_stack_heads_primary_and_keeps_fallbacks() {
        // Issue #35: default head is JetBrainsMono Nerd Font with the
        // emoji/CJK tail behind it.
        let cfg = TerminalConfig::default();
        let stack = cfg.font_stack();
        assert_eq!(stack[0], "JetBrainsMono Nerd Font");
        assert!(stack.contains(&"Apple Color Emoji".to_string()));
        assert!(stack.contains(&"Noto Sans Mono CJK SC".to_string()));
        assert_eq!(stack.last().unwrap(), "DejaVu Sans Mono");
        assert_eq!(cfg.font_size, 13.0);
        // A user override replaces the head, never the tail.
        let custom = TerminalConfig {
            font_family: "FiraCode Nerd Font".to_string(),
            ..TerminalConfig::default()
        };
        let stack = custom.font_stack();
        assert_eq!(stack[0], "FiraCode Nerd Font");
        assert!(stack.contains(&"Apple Color Emoji".to_string()));
        assert!(stack.contains(&"Noto Sans Mono CJK SC".to_string()));
        assert_eq!(
            stack.iter().filter(|f| *f == "FiraCode Nerd Font").count(),
            1
        );
        // Partial JSON keeps defaults for missing keys.
        let partial: Config = serde_json::from_str(r#"{"terminal": {"font_size": 15.0}}"#).unwrap();
        assert_eq!(partial.terminal.font_family, "JetBrainsMono Nerd Font");
        assert_eq!(partial.terminal.font_size, 15.0);
        assert_eq!(partial.terminal.fallback_fonts, default_fallback_fonts());
    }

    #[test]
    fn theme_choice_cycles_parses_and_resolves() {
        // Issue #34: dark → light → system → dark.
        assert_eq!(ThemePreference::Dark.cycle(), ThemePreference::Light);
        assert_eq!(ThemePreference::Light.cycle(), ThemePreference::System);
        assert_eq!(ThemePreference::System.cycle(), ThemePreference::Dark);
        assert_eq!(ThemePreference::Dark.label(), "dark",);
        // Lowercase config labels parse; explicit choices ignore the OS.
        let parsed: Config = serde_json::from_str(r#"{"theme": "light"}"#).unwrap();
        assert_eq!(parsed.theme, ThemePreference::Light);
        assert_eq!(
            ThemePreference::Dark.resolve(OsAppearance::Light),
            EffectiveTheme::Dark
        );
        assert_eq!(
            ThemePreference::Light.resolve(OsAppearance::Dark),
            EffectiveTheme::Light
        );
        // System follows the OS; unknown stays on the historic dark.
        assert_eq!(
            ThemePreference::System.resolve(OsAppearance::Dark),
            EffectiveTheme::Dark
        );
        assert_eq!(
            ThemePreference::System.resolve(OsAppearance::Light),
            EffectiveTheme::Light
        );
        assert_eq!(
            ThemePreference::System.resolve(OsAppearance::Unknown),
            EffectiveTheme::Dark
        );
        assert!(ThemePreference::System.is_dark(OsAppearance::Dark));
        assert!(!ThemePreference::System.is_dark(OsAppearance::Light));
    }

    #[test]
    fn extra_args_round_trip_through_json() {
        let mut cfg = Config::default();
        cfg.agents.insert(
            "muse".to_string(),
            AgentConfig {
                extra_args: vec!["--yolo".to_string()],
                yolo: false,
            },
        );
        let text = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back, cfg);
        assert_eq!(back.extra_args_for("muse"), vec!["--yolo".to_string()]);
        // Unknown keys are ignored, not fatal.
        let lenient: Config = serde_json::from_str(r#"{"agents": {}, "future": 1}"#).unwrap();
        assert_eq!(lenient, Config::default());
    }

    #[test]
    fn missing_or_malformed_file_falls_back_to_default() {
        let dir = std::env::temp_dir().join("staap-cfg-test-missing");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(Config::load_from(&dir.join("nope.json")), Config::default());
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{not json").unwrap();
        assert_eq!(Config::load_from(&bad), Config::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_and_load_preserve_extra_args() {
        let dir = std::env::temp_dir().join("staap-cfg-test-save");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let old = scoped_env(&path);
        let mut cfg = Config::default();
        cfg.agents.insert(
            "claude".to_string(),
            AgentConfig {
                extra_args: vec!["--dangerously-skip-permissions".to_string()],
                yolo: false,
            },
        );
        cfg.save().unwrap();
        assert_eq!(Config::load(), cfg);
        if let Some(v) = old {
            std::env::set_var("STAAP_CONFIG", v);
        } else {
            std::env::remove_var("STAAP_CONFIG");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
