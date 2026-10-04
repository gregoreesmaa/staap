//! Agent models, conversation status, and sort order.

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::embedded::SpawnKind;
use crate::transcript::TranscriptMessage;

/// Conversation attention state, ordered by urgency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Status {
    /// Needs user attention (e.g. approval prompt, error, recent question).
    Attention,
    /// Waiting / no recent activity.
    Idle,
    /// Actively producing output.
    Working,
}

/// Max parsed links retained per list per run (PRs and related alike).
/// Cap-not-drop display keeps the full story bounded: feeding 10k links
/// keeps memory flat and surfaces [`ChatSession::links_truncated`].
pub const MAX_STORED_LINKS: usize = 50;

/// Max parsed-link rows shown per run in the sessions panel; the rest
/// fold behind an `N more` disclosure ([`visible_links`]).
pub const MAX_VISIBLE_LINKS: usize = 20;

/// Harness id for sessions backed by the Muse CLI (issue #46): every
/// current session reports this. Kept as a plain string (not an enum)
/// so unknown/future harness ids survive a save/load roundtrip and
/// render via the generic fallback instead of failing to parse.
pub const HARNESS_MUSE: &str = "muse";

/// Harness id for sessions backed by the opencode CLI: discovered from
/// the opencode session store (`~/.local/share/opencode`), spawned as
/// plain `opencode`, resumed as `opencode --session <id>`.
pub const HARNESS_OPENCODE: &str = "opencode";

/// Harness id for sessions backed by the Claude Code CLI: discovered
/// from `~/.claude/projects/<project>/<session-id>.jsonl`, spawned as
/// plain `claude`, resumed as `claude --resume <id>`.
pub const HARNESS_CLAUDE: &str = "claude";

/// Harness id for sessions backed by the Codex CLI: discovered from
/// rollout logs under `~/.codex/sessions/`, spawned as plain `codex`,
/// resumed as `codex resume <id>`.
pub const HARNESS_CODEX: &str = "codex";

/// Harness id for sessions backed by the Antigravity CLI (`agy`):
/// discovered from `history.jsonl` under `~/.gemini/antigravity-cli`
/// plus per-conversation transcripts under `brain/`, spawned as plain
/// `agy`, resumed as `agy --conversation <id>`.
pub const HARNESS_ANTIGRAVITY: &str = "antigravity";

/// Default harness for newly spawned runs and for persisted sessions
/// predating the field (serde fills it in): everything today is muse.
pub fn default_harness() -> String {
    HARNESS_MUSE.to_string()
}

/// `(glyph, short tag)` badge for a harness id (issue #46): distinct
/// per known harness, generic fallback for unknown ids — never blank.
/// Glyphs avoid the [`Status`] row markers (`!`, `·`, `>`), and the
/// short tag keeps rows distinguishable with color removed. Adding a
/// harness later only registers a new id + icon arm here.
pub fn harness_badge(harness: &str) -> (&'static str, &'static str) {
    match harness {
        "muse" => ("⬢", "mu"),
        "codex" => ("⬣", "cx"),
        "claude" => ("▲", "cc"),
        "opencode" => ("⬓", "oc"),
        "antigravity" => ("⬔", "ag"),
        _ => ("○", "??"),
    }
}

/// Fresh stable row key for a session started in-app (issue #54).
///
/// UUID-shaped (`8-4-4-4-12` lowercase hex, version/variant bits set),
/// minted from wall-clock time, the process id, and a process-wide
/// counter — unique across restarts without coordination, std-only (no
/// new dependency). Stored on [`ChatSession::id`] and persisted with run
/// state, so a row keeps its identity (and its live PTY) across
/// restarts; titles are display-only and may repeat. Legacy `run-N` ids
/// from older state files keep loading untouched and never collide with
/// these.
pub fn new_session_id() -> String {
    fn mix64(mut z: u64) -> u64 {
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let lo_nanos = nanos as u64;
    let hi_nanos = (nanos >> 64) as u64;
    let hi = mix64(
        lo_nanos
            .wrapping_add(pid.rotate_left(17))
            .wrapping_add(count.wrapping_mul(0x9E37_79B9_7F4A_7C15)),
    );
    let lo = mix64(
        hi_nanos
            .wrapping_add(pid.wrapping_mul(0x85EB_CA6B))
            .wrapping_add(count ^ 0xC2B2_AE35_1752_7463),
    );
    // Version 4 + RFC-4122 variant bits on the top halves.
    let hi = (hi & 0xFFFF_FFFF_FFFF_0FFF) | 0x0000_0000_0000_4000;
    let lo = (lo & 0x3FFF_FFFF_FFFF_FFFF) | 0x8000_0000_0000_0000;
    let v = ((hi as u128) << 64) | lo as u128;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (v >> 96) & 0xFFFF_FFFF,
        (v >> 80) & 0xFFFF,
        (v >> 64) & 0xFFFF,
        (v >> 48) & 0xFFFF,
        v & 0xFFFF_FFFF_FFFF,
    )
}

/// One chat/agent conversation surfaced by a [`crate::providers::Provider`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSession {
    /// Stable row key (issue #54): a [`new_session_id`] UUID for runs
    /// started in-app, the provider session id for discovered ones.
    /// Persisted with run state, unique per row, and the key for every
    /// roster lookup (selection pinning, PTY map, merge, actions) — the
    /// title is display-only and may repeat across rows.
    pub id: String,
    pub title: String,
    pub project: String,
    pub status: Status,
    /// Which agent harness backs the run (issue #46), set at
    /// discovery/spawn time. All current sessions are [`HARNESS_MUSE`].
    #[serde(default = "default_harness")]
    pub harness: String,
    /// Unix seconds of last observed activity (for display only).
    pub last_active: i64,
    /// GitHub PR URLs extracted from the conversation transcript.
    pub pr_links: Vec<String>,
    /// Non-PR references (issues, commits, file refs), same treatment.
    #[serde(default)]
    pub related_links: Vec<String>,
    /// True once either link list hit [`MAX_STORED_LINKS`].
    #[serde(default)]
    pub links_truncated: bool,
    /// Most recent chat messages parsed from the `session.jsonl` tail.
    #[serde(default)]
    pub transcript: Vec<TranscriptMessage>,
    /// True when older messages were dropped (tail or message cap).
    #[serde(default)]
    pub transcript_truncated: bool,
    /// True once a submitted prompt replaced the placeholder animal title.
    #[serde(default)]
    pub title_locked: bool,
    /// Current input line being typed into this run (Terminal focus).
    /// Session state, not PTY state: title tracking works before the
    /// child spawns and after it exits. Cleared on submit and restart.
    #[serde(default)]
    pub pending_input: String,
    /// Provider-side conversation id for `--resume` (empty for live/new
    /// runs). Survives save/load so restarts resume the same transcript.
    #[serde(default)]
    pub provider_session_id: Option<String>,
    /// Working directory the child spawns in (issue #48). `None` means
    /// the current behavior: inherit the app's own directory. Survives
    /// save/load with the rest of run state (local-only, plain file).
    #[serde(default)]
    pub cwd: Option<String>,
}

impl ChatSession {
    /// Merge freshly scanned links in first-seen order, capping each list
    /// at [`MAX_STORED_LINKS`] and raising `links_truncated` on overflow.
    /// Underlying data is never dropped by the display cap: the panel
    /// folds extras behind `N more` via [`visible_links`].
    pub fn push_links(&mut self, pr_fresh: Vec<String>, related_fresh: Vec<String>) {
        for link in pr_fresh {
            if !self.pr_links.contains(&link) {
                if self.pr_links.len() >= MAX_STORED_LINKS {
                    self.links_truncated = true;
                } else {
                    self.pr_links.push(link);
                }
            }
        }
        for link in related_fresh {
            if !self.related_links.contains(&link) {
                if self.related_links.len() >= MAX_STORED_LINKS {
                    self.links_truncated = true;
                } else {
                    self.related_links.push(link);
                }
            }
        }
    }
}

/// Runs currently needing user input (issue #24): the status-bar and
/// header badge counts these, clearing itself as runs settle.
pub fn attention_count(sessions: &[ChatSession]) -> usize {
    sessions
        .iter()
        .filter(|s| s.status == Status::Attention)
        .count()
}

/// Split a retained link list into the rows the panel shows plus the
/// folded count: the first [`MAX_VISIBLE_LINKS`] stay visible, the rest
/// collapse into the `N more` disclosure.
pub fn visible_links(links: &[String]) -> (&[String], usize) {
    if links.len() > MAX_VISIBLE_LINKS {
        (&links[..MAX_VISIBLE_LINKS], links.len() - MAX_VISIBLE_LINKS)
    } else {
        (links, 0)
    }
}

/// Placeholder names for runs before the user types their first prompt.
const ANIMALS: &[&str] = &[
    "otter", "fox", "badger", "heron", "mole", "wren", "stoat", "newt", "vole", "ibex", "gecko",
    "quail", "shrew", "egret", "marmot", "dormouse", "grebe", "polecat", "siskin", "tenrec",
    "uakari", "xerus", "yak", "zapus",
];

/// Deterministic placeholder title for the n-th run (1-based).
pub fn animal_name(n: usize) -> String {
    let base = ANIMALS[(n.saturating_sub(1)) % ANIMALS.len()];
    if n > ANIMALS.len() {
        format!("{base}-{n}")
    } else {
        base.to_string()
    }
}

/// Screen-text markers suggesting `muse` waits on the user (approval
/// prompts, permission questions, errors). Matched case-insensitively.
/// This is the single reconciled marker list, precompiled once: the live
/// shell and the historic provider classifier both funnel through
/// [`classify`], so a run can never show a different status live vs
/// historic by construction, and no per-tick allocation happens here.
static ATTENTION_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r#"(?i)approval|approve|permission|needs_input|needs input|\"error\"|\(y/n\)|allow once|allow always|would you like|press enter to confirm"#,
    )
    .expect("static attention regex")
});

/// True when `text` carries an attention marker. Allocation-free:
/// a single precompiled case-insensitive scan, no lowercase copy.
pub fn needs_attention(text: &str) -> bool {
    ATTENTION_RE.is_match(text)
}

/// A run counts as actively working while it produced output recently.
pub const WORKING_WINDOW_SECS: u64 = 60;

/// The one status classifier, owned by `app`. `screen_text` is the live
/// screen (or the historic transcript tail), `output_age` is how long ago
/// the run last produced output (`None` = never/unknown), and `exited`
/// reports the child state (always `false` for historic sessions).
/// Attention markers win over everything; an exited run without markers is
/// idle; otherwise recency inside the working window decides.
pub fn classify(
    screen_text: &str,
    output_age: Option<std::time::Duration>,
    exited: bool,
) -> Status {
    classify_with_attention(needs_attention(screen_text), output_age, exited)
}

/// [`classify`] with the marker scan already done. The shell caches the
/// scan per run and skips re-scanning unchanged screens; attention still
/// outranks exit and recency exactly as in [`classify`].
pub fn classify_with_attention(
    attention: bool,
    output_age: Option<std::time::Duration>,
    exited: bool,
) -> Status {
    if attention {
        return Status::Attention;
    }
    if exited {
        return Status::Idle;
    }
    match output_age {
        Some(age) if age < std::time::Duration::from_secs(WORKING_WINDOW_SECS) => Status::Working,
        _ => Status::Idle,
    }
}

/// Top-down sort: attention first, then idle, then working; stable by
/// `last_active` descending within a status bucket.
pub fn sort_sessions(sessions: &mut [ChatSession]) {
    sessions.sort_by(|a, b| {
        a.status
            .cmp(&b.status)
            .then_with(|| b.last_active.cmp(&a.last_active))
    });
}

/// Left-panel section header for a status bucket.
pub fn section_title(status: Status) -> &'static str {
    match status {
        Status::Attention => "Needs input",
        Status::Idle => "Idle",
        Status::Working => "Active",
    }
}

/// Group already-sorted sessions into status sections, in [`Status`] order
/// (attention, idle, working). Returns `(status, indices)` pairs, skipping
/// empty buckets; within a bucket the slice order is preserved.
pub fn status_sections(sessions: &[ChatSession]) -> Vec<(Status, Vec<usize>)> {
    let mut out: Vec<(Status, Vec<usize>)> = Vec::new();
    for status in [Status::Attention, Status::Idle, Status::Working] {
        let ids: Vec<usize> = sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.status == status)
            .map(|(i, _)| i)
            .collect();
        if !ids.is_empty() {
            out.push((status, ids));
        }
    }
    out
}

/// Basename of the current working directory, for labeling user runs.
fn current_dir_name() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "muse".to_string())
}

/// Glanceable tail of a session folder for the runs list (issue #48):
/// the last two path components (`repo/sub`), falling back to fewer
/// when the path is short. The full path stays on the session for the
/// footer detail line, so this never has to be unique — just short.
pub fn short_cwd(cwd: &str) -> String {
    const MAX: usize = 28;
    let parts: Vec<&str> = cwd.split('/').filter(|p| !p.is_empty()).collect();
    let tail = if parts.len() >= 2 {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        parts.join("/")
    };
    if tail.chars().count() > MAX {
        let mut cut: String = tail
            .chars()
            .skip(tail.chars().count() - (MAX - 1))
            .collect();
        cut.insert(0, '…');
        cut
    } else {
        tail
    }
}

/// Home directory, portably: `$HOME` on Unix, `%USERPROFILE%` (then
/// `%HOMEDRIVE%%HOMEPATH%`) on Windows, where `HOME` is usually unset.
pub fn home_dir() -> Option<String> {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return Some(home);
        }
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        if !profile.is_empty() {
            return Some(profile);
        }
    }
    let drive = std::env::var("HOMEDRIVE").unwrap_or_default();
    let path = std::env::var("HOMEPATH").unwrap_or_default();
    if !path.is_empty() {
        return Some(format!("{drive}{path}"));
    }
    None
}

/// Normalize folder-picker input (issue #48): blank means the default
/// (inherit, `None`); a leading `~` expands to the home directory;
/// anything else passes through untouched (existence is checked at
/// confirm time).
pub fn expand_cwd_input(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix('~') {
        // `\` only separates on Windows: on Unix `~\foo` is a real name.
        let sep = rest.starts_with('/') || (cfg!(windows) && rest.starts_with('\\'));
        if rest.is_empty() || sep {
            if let Some(home) = home_dir() {
                return Some(format!("{home}{rest}"));
            }
        }
    }
    Some(trimmed.to_string())
}

/// Current Unix time in seconds, for run bookkeeping.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Which pane owns the keyboard: list navigation or the embedded `muse`
/// terminal. In [`Focus::Terminal`] every key (except the focus key itself)
/// is forwarded to `muse`; app navigation is suspended until focus returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// List navigation: j/k move, PgUp/PgDn page, o focuses a parsed
    /// link, Enter copies the focused link, n starts a new session,
    /// ? toggles the in-app help panel.
    #[default]
    Nav,
    /// Typing: keys go to the embedded `muse` PTY.
    Terminal,
}

/// Application state: the live run list plus list selection, keyboard
/// focus, the pending PTY spawn the main loop must start, and a counter for
/// placeholder animal titles. The list starts empty: entries appear only when the
/// user starts a new `muse` session in-app.
pub struct App {
    pub sessions: Vec<ChatSession>,
    pub selected: usize,
    pub focus: Focus,
    /// Title-substring filter (issue #29): the panel shows only matching
    /// runs, in unchanged sort order. Empty means unfiltered.
    pub filter: String,
    /// History section expansion (issues #39/#55): collapsed unless the
    /// persisted config says expanded (restored on launch via
    /// [`Self::set_config`]); toggled by click/`h`/Enter, mirrored
    /// into the config so every toggle persists.
    pub history_expanded: bool,
    pending_spawn: Option<SpawnKind>,
    /// Placeholder-title counter (row identity is the UUID on
    /// [`ChatSession::id`): the n-th run starts titled [`animal_name`]`(n)`.
    next_run: usize,
    status_msg: Option<(String, std::time::Instant)>,
    sticky_error: Option<String>,
    /// User configuration (issue #33: per-agent extra CLI flags).
    config: Config,
}

/// How long a transient status-bar message stays visible.
const STATUS_TTL: std::time::Duration = std::time::Duration::from_secs(3);

/// Rows moved by one PgUp/PgDn step. List state carries no viewport
/// height, so paging is a fixed step, clamped at the ends.
pub const PAGE_STEP: usize = 5;

impl App {
    pub fn new(mut sessions: Vec<ChatSession>) -> Self {
        sort_sessions(&mut sessions);
        Self {
            sessions,
            selected: 0,
            focus: Focus::Nav,
            filter: String::new(),
            history_expanded: false,
            pending_spawn: None,
            next_run: 0,
            status_msg: None,
            sticky_error: None,
            config: Config::default(),
        }
    }

    /// Install the user configuration (loaded once at startup in
    /// `main`). Tests keep the default (no extra flags). The persisted
    /// History expansion (issue #55) restores here, so a restart keeps
    /// the toggle state the last session left.
    pub fn set_config(&mut self, config: Config) {
        self.history_expanded = config.history_expanded;
        self.config = config;
    }

    /// Advance the theme choice one step (the `t`-key cycle) and return
    /// the new choice. The caller applies and persists it.
    pub fn cycle_theme(&mut self) -> crate::config::ThemePreference {
        self.config.theme = self.config.theme.cycle();
        self.config.theme
    }

    /// Persist the current configuration (theme choice) to disk.
    pub fn save_config(&self) -> anyhow::Result<()> {
        self.config.save()
    }

    /// FFI accessors for the 2D-launch C ABI (read-only views into the
    /// owned config; the core never hands out mutable config across FFI).
    pub fn config_last_cli(&self) -> Option<&str> {
        self.config.last_cli.as_deref()
    }
    pub fn config_default_cli(&self) -> Option<&str> {
        self.config.default_cli.as_deref()
    }

    pub fn config_recents(&self) -> &[String] {
        &self.config.recent_folders
    }

    /// Test helper: seed folder recents (production reaches them through
    /// confirmed launches via `note_launch`).
    #[cfg(test)]
    pub fn config_note_recents(&mut self, recents: Vec<String>) {
        self.config.recent_folders = recents;
    }

    pub fn config_default_cwd(&self) -> Option<&str> {
        self.config.default_cwd.as_deref()
    }

    pub fn config_extra_args(&self, agent: &str) -> Vec<String> {
        self.config.extra_args_for(agent)
    }

    pub fn config_yolo_default(&self, agent: &str) -> bool {
        self.config.yolo_default_for(agent)
    }

    /// Mutable config for tests that set up launch scenarios (the FFI
    /// tri-state test opts an agent into yolo). Production mutates via
    /// the typed helpers (`set_config`, `note_launch`, …), never this.
    #[cfg(test)]
    pub fn config_mut(&mut self) -> &mut crate::config::Config {
        &mut self.config
    }

    /// Record a confirmed launch from a shell that spawns via FFI (the
    /// in-process gpui path goes through `start_launch` instead): same
    /// memory update, no new row. Empty CLI keeps the previous memory.
    pub fn note_launch(&mut self, cli: &str, cwd: Option<&str>) {
        if cli.is_empty() {
            if let Some(dir) = cwd {
                crate::launch::push_recent_folder(&mut self.config.recent_folders, dir);
            }
            return;
        }
        self.config.note_launch(cli, cwd);
    }

    /// Terminal-pane font setting (issue #35).
    pub fn terminal_config(&self) -> &crate::config::TerminalConfig {
        &self.config.terminal
    }

    /// Sessions-panel width in pixels (issue #29): the comfort-adjusted
    /// value clamped to the sane range, so a hand-edited config can
    /// never collapse or explode the panel.
    pub fn sidebar_width(&self) -> f32 {
        self.config.sidebar_width.clamp(160.0, 480.0)
    }

    /// Comfort-key mutation half (issue #29, same split as the `t`-key
    /// theme cycle): clamp the terminal font size into range and return
    /// it for the status flash. The caller persists via
    /// [`App::save_config`].
    pub fn set_terminal_font_size(&mut self, size: f32) -> f32 {
        let size = size.clamp(8.0, 32.0);
        self.config.terminal.font_size = size;
        size
    }

    /// Comfort-key mutation half for the panel width: same
    /// mutate/flash/persist split as [`App::set_terminal_font_size`].
    pub fn set_sidebar_width(&mut self, width: f32) -> f32 {
        let width = width.clamp(160.0, 480.0);
        self.config.sidebar_width = width;
        width
    }

    /// Spawn command for `kind` with the configured per-agent extra flags
    /// appended (issue #33) plus the yolo default/override (2D launch).
    /// The key is the program name, so every supported agent (`muse`,
    /// `claude`, …) can carry its own flags. The canonical yolo flag is
    /// deduped against an identical user-configured `extra_args` entry so
    /// enabling yolo twice never doubles the flag.
    pub fn spawn_command_for(&self, kind: &SpawnKind) -> (String, Vec<String>) {
        let (program, mut args) = kind.command();
        args.extend(self.config.extra_args_for(&program));
        let yolo_on = match kind {
            SpawnKind::NewOn { harness, yolo } => {
                *yolo || self.config.yolo_default_for(harness.id())
            }
            _ => self.config.yolo_default_for(&program),
        };
        if yolo_on {
            if let Some(flag) = kind.harness().yolo_flag() {
                if !args.iter().any(|a| a == flag) {
                    args.push(flag.to_string());
                }
            }
        }
        (program, args)
    }

    /// One-line spawn description for UI affordances (`muse --yolo`).
    pub fn spawn_command_string(&self) -> String {
        self.spawn_command_string_for(&SpawnKind::New)
    }

    /// One-line spawn description for `kind` (2D launch preview).
    pub fn spawn_command_string_for(&self, kind: &SpawnKind) -> String {
        let (program, args) = self.spawn_command_for(kind);
        if args.is_empty() {
            program
        } else {
            format!("{program} {}", args.join(" "))
        }
    }

    /// Effective CLI for the next repeat-last spawn (2D launch): the
    /// last-used CLI when still supported, else the configured default,
    /// else the first autodetected binary. Pure resolution lives in
    /// `launch::resolve_effective_cli`; this reads the stored bits.
    pub fn repeat_cli(&self) -> String {
        crate::launch::resolve_effective_cli(
            None,
            self.config.last_cli.as_deref(),
            self.config.default_cli.as_deref(),
            &crate::launch::detect_available_clis(),
        )
    }

    /// Spawn kind for a repeat-last launch (2D launch): `n` / the `+ New`
    /// main click. Resolves the yolo flag against the per-agent default.
    pub fn repeat_spawn_kind(&self) -> SpawnKind {
        use crate::embedded::Harness;
        let cli = self.repeat_cli();
        if cli == HARNESS_MUSE
            && self.config.last_cli.is_none()
            && self.config.default_cli.is_none()
        {
            // No memory and no default: the historic plain path, so the
            // first-ever spawn stays exactly `muse`.
            SpawnKind::New
        } else {
            let harness = Harness::from_id(&cli);
            let yolo = self.config.yolo_default_for(harness.id());
            SpawnKind::NewOn { harness, yolo }
        }
    }

    /// True when `session` passes the title filter (case-insensitive
    /// substring; everything passes when the filter is empty).
    pub fn matches_filter(&self, session: &ChatSession) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        session
            .title
            .to_lowercase()
            .contains(&self.filter.to_lowercase())
    }

    /// Indices of the sessions the panel shows: filter matches in list
    /// order, so filtering never re-sorts.
    pub fn visible_indices(&self) -> Vec<usize> {
        self.sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| self.matches_filter(s))
            .map(|(i, _)| i)
            .collect()
    }

    /// Toggle the History section between collapsed and expanded
    /// (issues #39/#55). Collapsed by default; the selection is left
    /// alone so a selected history entry keeps its position. Mirrored
    /// into the config — the ShellView toggle funnel persists it, so
    /// the state survives restarts.
    pub fn toggle_history(&mut self) {
        self.set_history_expanded(!self.history_expanded);
    }

    /// Set the History expansion state, mirroring it into the config so
    /// the state survives restarts (same funnel as [`Self::toggle_history`];
    /// the FFI setter routes through here so the invariant has one home).
    pub fn set_history_expanded(&mut self, expanded: bool) {
        self.history_expanded = expanded;
        self.config.history_expanded = expanded;
    }

    /// Replace the title filter, snapping the selection into the matches
    /// (first match when the selected run is filtered out).
    pub fn set_filter(&mut self, text: String) {
        self.filter = text;
        if self.filter.is_empty() {
            return;
        }
        let visible = self.visible_indices();
        if !visible.contains(&self.selected) {
            if let Some(&first) = visible.first() {
                self.selected = first;
            }
        }
    }

    /// Flash a transient message in the status bar.
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_msg = Some((msg.into(), std::time::Instant::now()));
    }

    /// Record a sticky error (spawn/PTY-write failure). It stays until an
    /// explicit [`App::clear_error`] or the next success — repaints and the
    /// transient TTL never clear it.
    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.sticky_error = Some(msg.into());
    }

    /// Dismiss the sticky error, if any.
    pub fn clear_error(&mut self) {
        self.sticky_error = None;
    }

    /// The sticky error, if one is being shown.
    pub fn error_text(&self) -> Option<&str> {
        self.sticky_error.as_deref()
    }

    /// Current status-bar message. Split policy: info flashes for
    /// [`STATUS_TTL`], errors stay until dismissed or superseded by a
    /// success. A sticky error always wins over transient info.
    pub fn status_text(&self) -> Option<&str> {
        if let Some(err) = self.sticky_error.as_deref() {
            return Some(err);
        }
        self.status_msg.as_ref().and_then(|(msg, at)| {
            if at.elapsed() < STATUS_TTL {
                Some(msg.as_str())
            } else {
                None
            }
        })
    }

    /// Take the pending spawn request, if any (the main loop spawns it,
    /// then owns the live handle until the next request).
    pub fn take_pending_spawn(&mut self) -> Option<SpawnKind> {
        self.pending_spawn.take()
    }

    /// Re-queue a spawn for the selected run (Retry after a spawn failure).
    /// Creates no new run entry: the failed run keeps its id and title.
    /// `kind` preserves the origin: historic/retry selections resume,
    /// live ones relaunch fresh. No-op when no run is selected.
    pub fn retry_spawn(&mut self, kind: SpawnKind) {
        if self.selected_session().is_some() {
            self.pending_spawn = Some(kind);
        }
    }

    /// Whether a spawn is queued and not yet consumed by the main loop.
    pub fn has_pending_spawn(&self) -> bool {
        self.pending_spawn.is_some()
    }

    /// Spawn kind for a sidebar selection: historic entries resume,
    /// live entries relaunch fresh. Resume routes through the stored
    /// harness: opencode-backed rows re-attach as `opencode --session`,
    /// claude-backed rows as `claude --resume <id>`,
    /// codex-backed rows as `codex resume <id>`,
    /// antigravity-backed rows as `agy --conversation <id>`,
    /// everything else as `muse --resume <id>`.
    pub fn respawn_kind(&self, run_id: &str) -> SpawnKind {
        use crate::embedded::Harness;
        match self.sessions.iter().find(|s| s.id == run_id) {
            Some(s) if s.provider_session_id.is_some() => {
                let session_id = s.provider_session_id.clone().unwrap_or_default();
                match Harness::from_id(&s.harness) {
                    Harness::Muse => SpawnKind::Resume { session_id },
                    harness => SpawnKind::ResumeOn {
                        harness,
                        session_id,
                    },
                }
            }
            _ => SpawnKind::New,
        }
    }

    /// Remove the run entry with `run_id` (per-run close/kill: the shell
    /// drops the live PTY alongside, so `Drop` reaps the child).
    /// Returns the removed title for the confirmation flash. Selection
    /// clamps into the shrunken list; a pending spawn for the closed run
    /// is dropped with it. No-op (returns `None`) when missing.
    pub fn remove_session(&mut self, run_id: &str) -> Option<String> {
        let pos = self.sessions.iter().position(|s| s.id == run_id)?;
        // A queued spawn always belongs to the selected (newest) run; it
        // dies with that run, never with a bystander.
        let closing_selected = pos == self.selected;
        let removed = self.sessions.remove(pos);
        if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len().saturating_sub(1);
        }
        if closing_selected {
            self.pending_spawn = None;
        }
        Some(removed.title)
    }

    /// True when quitting deserves a confirmation step: any run is
    /// Working or Attention. (The shell ORs in live PTYs it owns.)
    pub fn needs_quit_confirm(&self) -> bool {
        self.sessions
            .iter()
            .any(|s| matches!(s.status, Status::Working | Status::Attention))
    }

    /// Enter terminal focus: keys go to the embedded `muse`.
    pub fn focus_terminal(&mut self) {
        self.focus = Focus::Terminal;
    }

    /// Return to list navigation.
    pub fn focus_nav(&mut self) {
        self.focus = Focus::Nav;
    }

    /// Toggle between navigation and terminal focus (bound to Tab).
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Nav => Focus::Terminal,
            Focus::Terminal => Focus::Nav,
        };
    }

    pub fn is_terminal_focused(&self) -> bool {
        self.focus == Focus::Terminal
    }

    /// Position of the selection inside the visible matches (issue #29):
    /// the snapped index when selected is visible, else the head.
    fn visible_pos(&self, visible: &[usize]) -> usize {
        visible
            .iter()
            .position(|&i| i == self.selected)
            .unwrap_or(0)
    }

    pub fn select_next(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        if self.filter.is_empty() {
            self.selected = (self.selected + 1) % self.sessions.len();
            return;
        }
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        let pos = self.visible_pos(&visible);
        self.selected = visible[(pos + 1) % visible.len()];
    }

    pub fn select_prev(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        if self.filter.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.sessions.len() - 1);
            return;
        }
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        let pos = self.visible_pos(&visible);
        self.selected = visible[(pos + visible.len() - 1) % visible.len()];
    }

    /// Page down: move selection toward the tail, clamped at the last run.
    pub fn select_page_next(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        if self.filter.is_empty() {
            self.selected = (self.selected + PAGE_STEP).min(self.sessions.len() - 1);
            return;
        }
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        let pos = self.visible_pos(&visible);
        self.selected = visible[(pos + PAGE_STEP).min(visible.len() - 1)];
    }

    /// Page up: move selection toward the head, clamped at the first run.
    pub fn select_page_prev(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        if self.filter.is_empty() {
            self.selected = self.selected.saturating_sub(PAGE_STEP);
            return;
        }
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        let pos = self.visible_pos(&visible);
        self.selected = visible[pos.saturating_sub(PAGE_STEP)];
    }

    /// Create a new live-run entry, queue a brand-new `muse` session for it,
    /// and hand it the keyboard. Switching runs never kills the others: the
    /// main loop keeps one live PTY per entry. The entry starts with a
    /// placeholder animal title until the first submitted prompt renames it.
    pub fn start_new_session(&mut self) {
        self.start_new_session_in(None);
    }

    /// Same as [`App::start_new_session`] but spawning the child in
    /// `cwd` (issue #48): `None` keeps the current behavior (inherit the
    /// app's directory). The folder rides on the session so the runs
    /// list, restarts, and persisted state all see it.
    pub fn start_new_session_in(&mut self, cwd: Option<String>) {
        self.start_launch(&crate::launch::LaunchSelection::new(
            self.repeat_cli(),
            cwd,
            self.config.yolo_default_for(&self.repeat_cli()),
        ));
    }

    /// Create a new live-run entry from a confirmed 2D [`LaunchSelection`]
    /// (folder × CLI + yolo): records the harness, queues the spawn kind,
    /// and notes the launch for repeat-last memory + folder MRU. This is
    /// the single funnel every new-run path uses (`n`, `+ New`, picker).
    /// Unknown CLI ids fall back to the historic muse harness (same rule
    /// as [`crate::embedded::Harness::from_id`]), never a broken row.
    pub fn start_launch(&mut self, selection: &crate::launch::LaunchSelection) {
        use crate::embedded::Harness;
        let harness = if crate::launch::SUPPORTED_CLIS.contains(&selection.cli.as_str()) {
            Harness::from_id(&selection.cli)
        } else {
            Harness::Muse
        };
        let kind = if harness == Harness::Muse && !selection.yolo {
            SpawnKind::New
        } else {
            SpawnKind::NewOn {
                harness,
                yolo: selection.yolo,
            }
        };
        let cli = harness.id().to_string();
        self.next_run += 1;
        let n = self.next_run;
        self.sessions.push(ChatSession {
            // UUID row key (issue #54): never reused, so a restart can
            // never mint an id that collides with a persisted row.
            id: new_session_id(),
            title: animal_name(n),
            project: current_dir_name(),
            status: Status::Working,
            harness: cli.clone(),
            last_active: now_secs(),
            pr_links: vec![],
            related_links: vec![],
            links_truncated: false,
            transcript: vec![],
            transcript_truncated: false,
            provider_session_id: None,
            title_locked: false,
            pending_input: String::new(),
            cwd: selection.cwd.clone(),
        });
        self.selected = self.sessions.len() - 1;
        self.pending_spawn = Some(kind);
        self.focus = Focus::Terminal;
        self.config.note_launch(&cli, selection.cwd.as_deref());
    }

    /// Working directory recorded on `run_id`, if the session picked one
    /// (issue #48). The spawn seam reads this so every backend launches
    /// in the chosen folder without per-backend handling.
    pub fn session_cwd(&self, run_id: &str) -> Option<std::path::PathBuf> {
        self.sessions
            .iter()
            .find(|s| s.id == run_id)
            .and_then(|s| s.cwd.as_deref())
            .map(std::path::PathBuf::from)
    }

    /// Record a prompt line the user submitted to `run_id`: the first one
    /// becomes the run's summary title so the list stays distinguishable,
    /// flashing a `renamed to '<title>'` confirmation so the silent rename
    /// is noticed. Later prompts and blank lines never rename nor flash.
    pub fn note_submitted_prompt(&mut self, run_id: &str, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let renamed = if let Some(s) = self.sessions.iter_mut().find(|s| s.id == run_id) {
            if !s.title_locked {
                s.title = crate::transcript::single_line(line);
                s.title_locked = true;
                Some(s.title.clone())
            } else {
                None
            }
        } else {
            None
        };
        if let Some(title) = renamed {
            self.set_status(format!("renamed to '{title}'"));
        }
    }

    /// Re-sort by activity (attention, idle, working) while keeping the
    /// selection pinned to the same run.
    pub fn resort_keep_selection(&mut self) {
        let selected_id = self.selected_session().map(|s| s.id.clone());
        sort_sessions(&mut self.sessions);
        if let Some(id) = selected_id {
            if let Some(pos) = self.sessions.iter().position(|s| s.id == id) {
                self.selected = pos;
            }
        }
    }

    pub fn selected_session(&self) -> Option<&ChatSession> {
        self.sessions.get(self.selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(id: &str, status: Status, last_active: i64) -> ChatSession {
        ChatSession {
            id: id.into(),
            title: id.into(),
            project: "proj".into(),
            status,
            harness: HARNESS_MUSE.into(),
            last_active,
            pr_links: vec![],
            related_links: vec![],
            links_truncated: false,
            transcript: vec![],
            transcript_truncated: false,
            provider_session_id: None,
            title_locked: true,
            pending_input: String::new(),
            cwd: None,
        }
    }

    #[test]
    fn link_storage_caps_at_50_with_truncation_flag() {
        let mut s = sess("a", Status::Working, 1);
        // 60 fresh PR links: first 50 retained in order, flag raised.
        let fresh: Vec<String> = (0..60)
            .map(|n| format!("https://github.com/acme/app/pull/{n}"))
            .collect();
        s.push_links(fresh, vec![]);
        assert_eq!(s.pr_links.len(), MAX_STORED_LINKS);
        assert_eq!(s.pr_links[0], "https://github.com/acme/app/pull/0");
        assert!(s.links_truncated);
        // Related lists cap independently; duplicates never double-count.
        let related: Vec<String> = (0..55).map(|n| format!("src/f{n}.rs:1")).collect();
        s.push_links(vec![s.pr_links[0].clone()], related);
        assert_eq!(s.pr_links.len(), MAX_STORED_LINKS);
        assert_eq!(s.related_links.len(), MAX_STORED_LINKS);
    }

    #[test]
    fn visible_links_folds_beyond_20_behind_n_more() {
        let links: Vec<String> = (0..25).map(|n| format!("l{n}")).collect();
        let (shown, hidden) = visible_links(&links);
        assert_eq!(shown.len(), MAX_VISIBLE_LINKS);
        assert_eq!(hidden, 5);
        let short: Vec<String> = (0..3).map(|n| format!("l{n}")).collect();
        let (shown, hidden) = visible_links(&short);
        assert_eq!(shown.len(), 3);
        assert_eq!(hidden, 0);
    }

    #[test]
    fn attention_sorts_first_then_idle_then_working() {
        let mut v = vec![
            sess("work", Status::Working, 99),
            sess("idle", Status::Idle, 1),
            sess("attn", Status::Attention, 1),
        ];
        sort_sessions(&mut v);
        assert_eq!(v[0].id, "attn");
        assert_eq!(v[1].id, "idle");
        assert_eq!(v[2].id, "work");
    }

    #[test]
    fn recency_breaks_status_ties() {
        let mut v = vec![
            sess("old", Status::Working, 1),
            sess("new", Status::Working, 5),
        ];
        sort_sessions(&mut v);
        assert_eq!(v[0].id, "new");
    }

    #[test]
    fn selection_wraps() {
        let mut app = App::new(vec![sess("a", Status::Idle, 1), sess("b", Status::Idle, 2)]);
        app.select_prev();
        assert_eq!(app.selected, 1);
        app.select_next();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn page_selection_moves_by_page_step_and_clamps_at_the_ends() {
        let sessions: Vec<ChatSession> = (0..8)
            .map(|n| sess(&format!("r{n}"), Status::Idle, n))
            .collect();
        let mut app = App::new(sessions);
        // Sorted newest-first; pin to the head for deterministic steps.
        app.selected = 0;
        app.select_page_next();
        assert_eq!(app.selected, PAGE_STEP);
        app.select_page_next();
        assert_eq!(app.selected, 7);
        app.select_page_next();
        assert_eq!(app.selected, 7);
        app.select_page_prev();
        assert_eq!(app.selected, 7 - PAGE_STEP);
        app.selected = 1;
        app.select_page_prev();
        assert_eq!(app.selected, 0);
        // Shorter than one step: a single page lands on the last run.
        let short: Vec<ChatSession> = (0..3)
            .map(|n| sess(&format!("s{n}"), Status::Idle, n))
            .collect();
        let mut short_app = App::new(short);
        short_app.selected = 0;
        short_app.select_page_next();
        assert_eq!(short_app.selected, 2);
        // Empty list: paging is inert.
        let mut empty = App::new(vec![]);
        empty.select_page_next();
        empty.select_page_prev();
        assert_eq!(empty.selected, 0);
    }

    #[test]
    fn startup_spawns_nothing_list_starts_empty() {
        let mut app = App::new(vec![]);
        assert!(app.sessions.is_empty());
        assert!(app.take_pending_spawn().is_none());
    }

    #[test]
    fn selection_never_spawns_switching_keeps_runs_alive() {
        let mut app = App::new(vec![sess("a", Status::Idle, 1), sess("b", Status::Idle, 2)]);
        app.select_next();
        assert!(app.take_pending_spawn().is_none());
        app.select_prev();
        assert!(app.take_pending_spawn().is_none());
    }

    #[test]
    fn start_new_session_creates_run_and_focuses_terminal() {
        let mut app = App::new(vec![]);
        assert!(!app.is_terminal_focused());
        app.start_new_session();
        assert_eq!(app.sessions.len(), 1);
        let first_id = app.sessions[0].id.clone();
        assert!(!first_id.is_empty());
        assert_eq!(app.selected, 0);
        assert_eq!(app.take_pending_spawn(), Some(SpawnKind::New));
        assert!(app.is_terminal_focused());
        app.focus_nav();
        app.start_new_session();
        assert_eq!(app.sessions.len(), 2);
        // UUID row keys (issue #54): every run mints a fresh id, so two
        // rows never share an identity even with equal titles.
        assert_ne!(app.sessions[1].id, first_id);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn configured_extra_args_append_to_spawn_command() {
        // Issue #33: default spawns stay plain; configured flags append.
        use crate::config::{AgentConfig, Config};
        let mut app = App::new(vec![]);
        assert_eq!(app.spawn_command_string(), "muse");
        let (program, args) = app.spawn_command_for(&SpawnKind::New);
        assert_eq!((program.as_str(), args.len()), ("muse", 0));
        let mut cfg = Config::default();
        cfg.agents.insert(
            "muse".to_string(),
            AgentConfig {
                extra_args: vec!["--yolo".to_string()],
                yolo: false,
            },
        );
        app.set_config(cfg);
        assert_eq!(app.spawn_command_string(), "muse --yolo");
        let (program, args) = app.spawn_command_for(&SpawnKind::New);
        assert_eq!(program, "muse");
        assert_eq!(args, vec!["--yolo".to_string()]);
    }

    #[test]
    fn launch_selection_records_harness_queues_kind_and_notes_memory() {
        // 2D launch: confirming a folder × CLI + yolo records the
        // harness on the row, queues the matching kind, and refreshes
        // repeat-last memory + folder MRU — the single funnel every
        // new-run path shares.
        use crate::embedded::Harness;
        use crate::launch::{LaunchSelection, YoloChoice};
        let mut app = App::new(vec![]);
        assert!(app.config.last_cli.is_none());
        let sel = LaunchSelection::new(
            "claude".to_string(),
            Some("/tmp/api".to_string()),
            YoloChoice::ForceOn.resolve(false),
        );
        app.start_launch(&sel);
        assert_eq!(app.sessions.len(), 1);
        let row = &app.sessions[0];
        assert_eq!(row.harness, "claude");
        assert_eq!(row.cwd.as_deref(), Some("/tmp/api"));
        assert_eq!(
            app.take_pending_spawn(),
            Some(SpawnKind::NewOn {
                harness: Harness::Claude,
                yolo: true,
            })
        );
        assert_eq!(app.config.last_cli.as_deref(), Some("claude"));
        assert_eq!(app.config.recent_folders, vec!["/tmp/api"]);
        // Yolo default dedupes: config flag + picker-on yields one flag.
        let mut cfg = crate::config::Config::default();
        cfg.agents.insert(
            "muse".to_string(),
            crate::config::AgentConfig {
                extra_args: vec!["--yolo".to_string()],
                yolo: false,
            },
        );
        app.set_config(cfg);
        let (program, args) = app.spawn_command_for(&SpawnKind::NewOn {
            harness: Harness::Muse,
            yolo: true,
        });
        assert_eq!(program, "muse");
        assert_eq!(args, vec!["--yolo".to_string()]);
        // Unknown CLI ids fall back to the historic harness, never a
        // broken row: the row spawns instead of failing to parse.
        app.start_launch(&LaunchSelection::new("future".to_string(), None, false));
        assert_eq!(app.sessions.last().unwrap().harness, HARNESS_MUSE);
    }

    #[test]
    fn new_runs_get_animal_titles_until_first_prompt() {
        assert_eq!(animal_name(1), "otter");
        assert_eq!(animal_name(2), "fox");
        let mut app = App::new(vec![]);
        app.start_new_session();
        assert_eq!(app.sessions[0].title, "otter");
        let id = app.sessions[0].id.clone();
        app.note_submitted_prompt(&id, "  fix the login redirect  ");
        assert_eq!(app.sessions[0].title, "fix the login redirect");
        // Second prompt does not rename: the first summary sticks.
        app.note_submitted_prompt(&id, "something else entirely");
        assert_eq!(app.sessions[0].title, "fix the login redirect");
        // Blank lines never rename.
        let mut app2 = App::new(vec![]);
        app2.start_new_session();
        let id2 = app2.sessions[0].id.clone();
        app2.note_submitted_prompt(&id2, "   ");
        assert_eq!(app2.sessions[0].title, "otter");
    }

    #[test]
    fn unified_classifier_covers_both_live_and_historic_paths() {
        use std::time::Duration;
        // Attention wins over exit and recency on either path.
        assert_eq!(
            classify(
                "Waiting for your approval",
                Some(Duration::from_secs(0)),
                false
            ),
            Status::Attention
        );
        assert_eq!(classify("allow once? (y/n)", None, true), Status::Attention);
        // Exited without markers is idle, however recent.
        assert_eq!(
            classify("done", Some(Duration::from_secs(0)), true),
            Status::Idle
        );
        // Recency decides the rest.
        assert_eq!(
            classify("working…", Some(Duration::from_secs(5)), false),
            Status::Working
        );
        assert_eq!(
            classify("old output", Some(Duration::from_secs(3600)), false),
            Status::Idle
        );
        assert_eq!(classify("nothing yet", None, false), Status::Idle);
    }

    #[test]
    fn cached_attention_agrees_with_full_scan() {
        use std::time::Duration;
        // The skip path (precomputed bit) decides exactly like the scan.
        assert_eq!(
            classify_with_attention(true, None, true),
            classify("allow once? (y/n)", None, true)
        );
        assert_eq!(
            classify_with_attention(false, Some(Duration::from_secs(5)), false),
            classify("plain output", Some(Duration::from_secs(5)), false)
        );
    }

    #[test]
    fn first_prompt_rename_flashes_confirmation() {
        let mut app = App::new(vec![]);
        app.start_new_session();
        assert_eq!(app.status_text(), None);
        let id = app.sessions[0].id.clone();
        app.note_submitted_prompt(&id, "  fix the login redirect  ");
        assert_eq!(app.sessions[0].title, "fix the login redirect");
        assert_eq!(
            app.status_text(),
            Some("renamed to 'fix the login redirect'")
        );
        // Second prompt keeps the first title and leaves the flash alone.
        app.note_submitted_prompt(&id, "something else entirely");
        assert_eq!(app.sessions[0].title, "fix the login redirect");
        assert_eq!(
            app.status_text(),
            Some("renamed to 'fix the login redirect'")
        );
        // Blank lines and unknown ids flash nothing.
        let mut app2 = App::new(vec![]);
        app2.start_new_session();
        let id2 = app2.sessions[0].id.clone();
        app2.note_submitted_prompt(&id2, "   ");
        assert_eq!(app2.status_text(), None);
        app2.note_submitted_prompt("run-9", "hello");
        assert_eq!(app2.status_text(), None);
    }

    #[test]
    fn attention_markers_match_approval_and_error_text() {
        assert!(needs_attention("Waiting for your approval to proceed"));
        assert!(needs_attention("Allow once? (y/n)"));
        assert!(needs_attention("permission denied by policy"));
        assert!(needs_attention("Tool failed with \"error\""));
        // Precompiled case-insensitive scan: same hits as the old
        // lowercase-copy version, without the per-tick allocation.
        assert!(needs_attention(
            "APPROVAL REQUIRED — Press ENTER to confirm"
        ));
        assert!(needs_attention("Would You Like to continue?"));
        assert!(!needs_attention("Muse Code 1.4.0"));
        assert!(!needs_attention(""));
    }

    #[test]
    fn resort_keeps_selection_on_the_same_run() {
        let mut app = App::new(vec![
            sess("a", Status::Working, 5),
            sess("b", Status::Idle, 1),
        ]);
        // Constructor sorts idle-first: [b, a]; select "a".
        assert_eq!(app.sessions[1].id, "a");
        app.selected = 1;
        app.sessions[0].status = Status::Attention; // "b" jumps first
        app.resort_keep_selection();
        assert_eq!(app.sessions[app.selected].id, "a");
        assert_eq!(app.sessions[0].id, "b");
    }

    #[test]
    fn sections_group_by_status_in_sort_order_skipping_empty() {
        let v = vec![
            sess("w1", Status::Working, 9),
            sess("i1", Status::Idle, 1),
            sess("a1", Status::Attention, 1),
            sess("w2", Status::Working, 2),
        ];
        // NB: not sorted; sections still bucket in Attention/Idle/Working
        // order and preserve slice order within a bucket.
        let sections = status_sections(&v);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].0, Status::Attention);
        assert_eq!(sections[0].1, vec![2]);
        assert_eq!(sections[1].0, Status::Idle);
        assert_eq!(sections[1].1, vec![1]);
        assert_eq!(sections[2].0, Status::Working);
        assert_eq!(sections[2].1, vec![0, 3]);
        assert_eq!(section_title(Status::Attention), "Needs input");
        assert_eq!(section_title(Status::Idle), "Idle");
        assert_eq!(section_title(Status::Working), "Active");
        // Empty buckets are skipped.
        let only_idle = vec![sess("i", Status::Idle, 1)];
        let sections = status_sections(&only_idle);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].0, Status::Idle);
    }

    #[test]
    fn sticky_error_wins_over_transient_until_dismissed() {
        let mut app = App::new(vec![]);
        assert_eq!(app.error_text(), None);
        app.set_status("copied selection (12 chars)");
        assert_eq!(app.status_text(), Some("copied selection (12 chars)"));
        // An error supersedes info and survives: later info flashes do not
        // replace it, and repaints (repeated reads) never clear it.
        app.set_error("pty write failed");
        assert_eq!(app.error_text(), Some("pty write failed"));
        app.set_status("pasted 3 chars");
        assert_eq!(app.status_text(), Some("pty write failed"));
        assert_eq!(app.status_text(), Some("pty write failed"));
        // Explicit dismissal falls back to whatever transient info is live.
        app.clear_error();
        assert_eq!(app.error_text(), None);
        assert_eq!(app.status_text(), Some("pasted 3 chars"));
    }

    #[test]
    fn close_removes_entry_clamps_selection_and_drops_its_spawn() {
        let mut app = App::new(vec![sess("a", Status::Idle, 1), sess("b", Status::Idle, 2)]);
        // Constructor sorts recency-desc: [b, a]; select "a" (index 1).
        app.selected = 1;
        app.pending_spawn = Some(SpawnKind::New);
        assert_eq!(app.remove_session("a"), Some("a".to_string()));
        // Closing the selected run drops its queued spawn; selection
        // clamps back into the shrunken list.
        assert!(app.take_pending_spawn().is_none());
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.selected, 0);
        // Missing ids are a no-op; dirty runs force a quit confirm.
        assert_eq!(app.remove_session("zzz"), None);
        assert!(!app.needs_quit_confirm());
        app.sessions[0].status = Status::Working;
        assert!(app.needs_quit_confirm());
        app.sessions[0].status = Status::Attention;
        assert!(app.needs_quit_confirm());
    }

    #[test]
    fn respawn_kind_resumes_historic_and_renews_live() {
        let mut app = App::new(vec![sess("a", Status::Idle, 1)]);
        assert_eq!(app.respawn_kind("a"), SpawnKind::New);
        assert!(!app.has_pending_spawn());
        app.sessions[0].provider_session_id = Some("s-1".into());
        assert_eq!(
            app.respawn_kind("a"),
            SpawnKind::Resume {
                session_id: "s-1".into()
            }
        );
        assert_eq!(app.respawn_kind("missing"), SpawnKind::New);
        app.retry_spawn(SpawnKind::New);
        assert!(app.has_pending_spawn());
    }

    #[test]
    fn respawn_kind_routes_opencode_rows_to_opencode_resume() {
        use crate::embedded::Harness;
        let mut app = App::new(vec![sess("a", Status::Idle, 1)]);
        app.sessions[0].harness = HARNESS_OPENCODE.to_string();
        app.sessions[0].provider_session_id = Some("ses-1".into());
        // opencode-backed historic rows re-attach via `opencode --session`.
        assert_eq!(
            app.respawn_kind("a"),
            SpawnKind::ResumeOn {
                harness: Harness::Opencode,
                session_id: "ses-1".into(),
            }
        );
        let (program, args) = app.spawn_command_for(&app.respawn_kind("a")).clone();
        assert_eq!(program, "opencode");
        assert_eq!(args, vec!["--session".to_string(), "ses-1".to_string()]);
        // Live rows still relaunch fresh, regardless of harness.
        app.sessions[0].provider_session_id = None;
        assert_eq!(app.respawn_kind("a"), SpawnKind::New);
    }

    #[test]
    fn respawn_kind_routes_claude_rows_to_claude_resume() {
        use crate::embedded::Harness;
        let mut app = App::new(vec![sess("a", Status::Idle, 1)]);
        app.sessions[0].harness = HARNESS_CLAUDE.to_string();
        app.sessions[0].provider_session_id = Some("sess-claude-1".into());
        // Claude-backed historic rows re-attach via `claude --resume`.
        assert_eq!(
            app.respawn_kind("a"),
            SpawnKind::ResumeOn {
                harness: Harness::Claude,
                session_id: "sess-claude-1".into(),
            }
        );
        let (program, args) = app.spawn_command_for(&app.respawn_kind("a")).clone();
        assert_eq!(program, "claude");
        assert_eq!(
            args,
            vec!["--resume".to_string(), "sess-claude-1".to_string()]
        );
        // Live rows still relaunch fresh, regardless of harness.
        app.sessions[0].provider_session_id = None;
        assert_eq!(app.respawn_kind("a"), SpawnKind::New);
    }

    #[test]
    fn respawn_kind_routes_codex_rows_to_codex_resume() {
        use crate::embedded::Harness;
        let mut app = App::new(vec![sess("a", Status::Idle, 1)]);
        app.sessions[0].harness = HARNESS_CODEX.to_string();
        app.sessions[0].provider_session_id = Some("sess-codex-1".into());
        // Codex-backed historic rows re-attach via `codex resume`.
        assert_eq!(
            app.respawn_kind("a"),
            SpawnKind::ResumeOn {
                harness: Harness::Codex,
                session_id: "sess-codex-1".into(),
            }
        );
        let (program, args) = app.spawn_command_for(&app.respawn_kind("a")).clone();
        assert_eq!(program, "codex");
        assert_eq!(args, vec!["resume".to_string(), "sess-codex-1".to_string()]);
        // Live rows still relaunch fresh, regardless of harness.
        app.sessions[0].provider_session_id = None;
        assert_eq!(app.respawn_kind("a"), SpawnKind::New);
    }

    #[test]
    fn respawn_kind_routes_antigravity_rows_to_agy_resume() {
        use crate::embedded::Harness;
        let mut app = App::new(vec![sess("a", Status::Idle, 1)]);
        app.sessions[0].harness = HARNESS_ANTIGRAVITY.to_string();
        app.sessions[0].provider_session_id = Some("convo-1".into());
        // Antigravity-backed historic rows re-attach via agy.
        assert_eq!(
            app.respawn_kind("a"),
            SpawnKind::ResumeOn {
                harness: Harness::Antigravity,
                session_id: "convo-1".into(),
            }
        );
        let (program, args) = app.spawn_command_for(&app.respawn_kind("a")).clone();
        assert_eq!(program, "agy");
        assert_eq!(
            args,
            vec!["--conversation".to_string(), "convo-1".to_string()]
        );
        // Live rows still relaunch fresh, regardless of harness.
        app.sessions[0].provider_session_id = None;
        assert_eq!(app.respawn_kind("a"), SpawnKind::New);
    }

    #[test]
    fn opencode_badge_is_distinct_and_non_blank() {
        // New harness arm: distinct from muse/codex/claude/unknown, never
        // blank, never colliding with the status row markers.
        let oc = harness_badge(HARNESS_OPENCODE);
        assert_eq!(oc, ("⬓", "oc"));
        assert_ne!(oc, harness_badge("muse"));
        assert_ne!(oc, harness_badge("codex"));
        assert_ne!(oc, harness_badge("claude"));
        assert_ne!(oc, harness_badge("future-harness"));
        for marker in ["!", "·", ">"] {
            assert_ne!(oc.0, marker);
        }
    }

    #[test]
    fn antigravity_badge_is_distinct_and_non_blank() {
        // New harness arm: distinct from muse/codex/claude/unknown, never
        // blank, never colliding with the status row markers.
        let ag = harness_badge(HARNESS_ANTIGRAVITY);
        assert_eq!(ag, ("⬔", "ag"));
        assert_ne!(ag, harness_badge("muse"));
        assert_ne!(ag, harness_badge("codex"));
        assert_ne!(ag, harness_badge("claude"));
        assert_ne!(ag, harness_badge("future-harness"));
        for marker in ["!", "·", ">"] {
            assert_ne!(ag.0, marker);
        }
    }

    #[test]
    fn retry_requeues_spawn_without_new_run_entry() {
        let mut app = App::new(vec![]);
        // No selection: no-op, never spawns.
        app.retry_spawn(SpawnKind::New);
        assert!(app.take_pending_spawn().is_none());
        app.start_new_session();
        assert_eq!(app.sessions.len(), 1);
        // Simulate the main loop consuming the spawn, then failing.
        assert_eq!(app.take_pending_spawn(), Some(SpawnKind::New));
        app.retry_spawn(SpawnKind::New);
        assert_eq!(app.take_pending_spawn(), Some(SpawnKind::New));
        // Retry reuses the same run id: no extra entry.
        assert_eq!(app.sessions.len(), 1);
    }

    #[test]
    fn harness_badges_cover_known_ids_with_generic_fallback() {
        // Issue #46: every known harness maps to a distinct non-blank
        // badge; unknown/future ids render the generic fallback, never
        // a blank or broken row.
        let muse = harness_badge("muse");
        let codex = harness_badge("codex");
        let claude = harness_badge("claude");
        let unknown = harness_badge("future-harness");
        for (glyph, short) in [muse, codex, claude, unknown] {
            assert!(!glyph.is_empty() && !short.is_empty());
        }
        assert_ne!(muse, codex);
        assert_ne!(muse, claude);
        assert_ne!(codex, claude);
        assert_eq!(unknown, ("○", "??"));
        assert_eq!(harness_badge(""), ("○", "??"));
        // Badge glyphs never collide with the status row markers, so
        // the icon and the status cue stay distinguishable in
        // monochrome (issue #8 bar). Markers are owned by the shell
        // layer (`gui::theme::row_marker`); mirrored literally here so
        // the framework-free core keeps zero shell/toolkit references
        // (issue #60: core builds as a library without `gui/`).
        for marker in ["!", "·", ">"] {
            for (glyph, _) in [muse, codex, claude, unknown] {
                assert_ne!(glyph, marker, "harness glyph vs {marker} marker");
            }
        }
    }

    #[test]
    fn harness_defaults_to_muse_and_survives_serde() {
        // Issue #46: all current sessions report muse, including
        // persisted runs predating the field.
        assert_eq!(default_harness(), HARNESS_MUSE);
        let mut app = App::new(vec![]);
        app.start_new_session();
        assert_eq!(app.sessions[0].harness, HARNESS_MUSE);
        // Old runs.json entries without the field load as muse.
        let old: ChatSession = serde_json::from_str(
            r#"{"id":"a","title":"a","project":"p","status":"Idle","last_active":1,"pr_links":[],"related_links":[]}"#,
        )
        .unwrap();
        assert_eq!(old.harness, HARNESS_MUSE);
        // Unknown future ids roundtrip instead of failing to parse.
        let mut future = sess("f", Status::Idle, 1);
        future.harness = "future-harness".into();
        let text = serde_json::to_string(&future).unwrap();
        let back: ChatSession = serde_json::from_str(&text).unwrap();
        assert_eq!(back.harness, "future-harness");
        assert_eq!(harness_badge(&back.harness), ("○", "??"));
    }

    #[test]
    fn history_starts_collapsed_and_toggles() {
        // Issue #39: collapsed by default on every launch; the toggle
        // flips both ways and leaves the selection alone.
        let mut app = App::new(vec![sess("a", Status::Idle, 1)]);
        assert!(!app.history_expanded);
        app.selected = 0;
        app.toggle_history();
        assert!(app.history_expanded);
        assert_eq!(app.selected, 0);
        app.toggle_history();
        assert!(!app.history_expanded);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn history_toggle_mirrors_config_and_restores_on_launch() {
        // Issue #55: the toggle writes through to the config (what the
        // ShellView funnel persists), and installing a config restores
        // the bit — a restart keeps the last expansion state.
        let mut app = App::new(vec![sess("a", Status::Idle, 1)]);
        app.toggle_history();
        assert!(app.config.history_expanded);
        // A fresh launch with that saved config starts expanded.
        let saved = app.config.clone();
        let mut relaunched = App::new(vec![sess("a", Status::Idle, 1)]);
        assert!(!relaunched.history_expanded);
        relaunched.set_config(saved);
        assert!(relaunched.history_expanded);
        // And collapsing restores collapsed.
        relaunched.toggle_history();
        assert!(!relaunched.config.history_expanded);
    }

    #[test]
    fn focus_key_toggles_between_nav_and_typing() {
        let mut app = App::new(vec![sess("a", Status::Idle, 1)]);
        assert_eq!(app.focus, Focus::Nav);
        app.toggle_focus();
        assert!(app.is_terminal_focused());
        app.toggle_focus();
        assert_eq!(app.focus, Focus::Nav);
        app.focus_terminal();
        app.focus_nav();
        assert_eq!(app.focus, Focus::Nav);
    }

    #[test]
    fn new_session_in_folder_records_cwd_default_stays_unset() {
        // Issue #48: the plain path keeps the current behavior (no
        // folder recorded); the folder path sticks it on the session
        // for the spawn seam, the list, and persistence.
        let mut app = App::new(vec![]);
        app.start_new_session();
        let _ = app.take_pending_spawn();
        assert_eq!(app.sessions[0].cwd, None);
        let plain_id = app.sessions[0].id.clone();
        assert_eq!(app.session_cwd(&plain_id), None);
        app.start_new_session_in(Some("/tmp/demo-proj".to_string()));
        let _ = app.take_pending_spawn();
        assert_eq!(app.sessions[1].cwd.as_deref(), Some("/tmp/demo-proj"));
        let dir_id = app.sessions[1].id.clone();
        assert_eq!(
            app.session_cwd(&dir_id),
            Some(std::path::PathBuf::from("/tmp/demo-proj"))
        );
        assert_eq!(app.session_cwd("missing"), None);
    }

    #[test]
    fn short_cwd_shows_the_tail_and_caps_length() {
        // Issue #48: rows stay glanceable — last two components, with
        // an ellipsis cap for absurd tails.
        assert_eq!(short_cwd("/tmp/demo-proj"), "tmp/demo-proj");
        assert_eq!(short_cwd("/Users/greg/projects/staap"), "projects/staap");
        assert_eq!(short_cwd("/tmp"), "tmp");
        assert_eq!(short_cwd("/"), "");
        let long = "/a/very-long-directory-name-here/another-long-one";
        let short = short_cwd(long);
        assert!(short.chars().count() <= 28, "capped: {short:?}");
        assert!(short.starts_with('…'), "ellipsis marks the cut: {short:?}");
        assert!(short.ends_with("another-long-one"));
    }

    #[test]
    fn cwd_input_blank_means_default_tilde_expands() {
        // Issue #48: the picker normalizes — blank/whitespace is the
        // default folder, `~` grows to $HOME, the rest passes through
        // for the is-dir check at confirm time.
        assert_eq!(expand_cwd_input(""), None);
        assert_eq!(expand_cwd_input("   "), None);
        assert_eq!(
            expand_cwd_input("/tmp/demo-proj"),
            Some("/tmp/demo-proj".to_string())
        );
        assert_eq!(expand_cwd_input("  /tmp/x  "), Some("/tmp/x".to_string()));
        let home = home_dir().expect("test env has a home dir");
        assert_eq!(expand_cwd_input("~/proj"), Some(format!("{home}/proj")));
        // `~other` is a real relative name, not a home: untouched.
        assert_eq!(expand_cwd_input("~other"), Some("~other".to_string()));
    }

    #[test]
    fn new_session_ids_are_unique_uuid_shaped_and_restart_safe() {
        // Issue #54: row keys are UUIDs, unique per run, so a restart can
        // never mint an id that collides with a persisted row (the old
        // `run-N` counter reset to 0 on every launch and shadowed the
        // persisted row with a same-titled dead duplicate).
        fn is_uuid(id: &str) -> bool {
            let parts: Vec<&str> = id.split('-').collect();
            if parts.len() != 5 {
                return false;
            }
            let lens = [8, 4, 4, 4, 12];
            for (part, len) in parts.iter().zip(lens) {
                if part.len() != len || !part.chars().all(|c| c.is_ascii_hexdigit()) {
                    return false;
                }
            }
            // Version 4 + RFC-4122 variant bits.
            parts[2].starts_with('4')
                && matches!(parts[3].chars().next(), Some('8' | '9' | 'a' | 'b'))
        }
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let id = new_session_id();
            assert!(is_uuid(&id), "not UUID-shaped: {id}");
            assert!(seen.insert(id), "duplicate id minted");
        }
        // Simulated restart: previously persisted ids (UUIDs and legacy
        // `run-N`) load, then a fresh run mints an id outside both sets.
        let mut app = App::new(vec![sess("run-1", Status::Idle, 1)]);
        app.start_new_session();
        let fresh = app.sessions.last().unwrap().id.clone();
        assert!(is_uuid(&fresh));
        assert_ne!(fresh, "run-1");
        assert!(!app.sessions[0].id.is_empty());
    }

    #[test]
    fn same_named_sessions_operate_independently() {
        // Issue #54 falsifiable: two rows sharing a title are still
        // independent rows — prompts, removal, cwd, and selection
        // pinning all route by id, never by title.
        let mut app = App::new(vec![]);
        app.start_new_session();
        app.start_new_session();
        let (a, b) = (app.sessions[0].id.clone(), app.sessions[1].id.clone());
        assert_ne!(a, b);
        for s in app.sessions.iter_mut() {
            s.title = "otter".into();
            s.title_locked = false;
        }
        // A prompt submitted to one renames only that row.
        app.note_submitted_prompt(&b, "fix the login redirect");
        assert_eq!(
            app.sessions.iter().find(|s| s.id == b).unwrap().title,
            "fix the login redirect"
        );
        assert_eq!(
            app.sessions.iter().find(|s| s.id == a).unwrap().title,
            "otter"
        );
        // Per-run folders resolve per id.
        app.sessions.iter_mut().find(|s| s.id == a).unwrap().cwd = Some("/tmp/a-proj".to_string());
        assert_eq!(
            app.session_cwd(&a),
            Some(std::path::PathBuf::from("/tmp/a-proj"))
        );
        assert_eq!(app.session_cwd(&b), None);
        // Re-sorting with duplicate titles keeps the selection pinned to
        // the same row by id.
        app.selected = app.sessions.iter().position(|s| s.id == b).unwrap();
        app.sessions.iter_mut().find(|s| s.id == a).unwrap().status = Status::Attention;
        app.resort_keep_selection();
        assert_eq!(app.sessions[app.selected].id, b);
        // Closing one removes exactly that row; the neighbor is untouched.
        assert_eq!(app.remove_session(&a), Some("otter".to_string()));
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.sessions[0].id, b);
        assert_eq!(app.sessions[0].title, "fix the login redirect");
        assert_eq!(app.remove_session(&a), None);
    }
}
