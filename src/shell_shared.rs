//! Shared native-shell helpers: one dumb-renderer contract over the core.
//!
//! Framework-free logic the three native UIs (macOS SwiftUI, Linux GTK4,
//! Windows WinUI) previously reimplemented in three languages: the
//! snapshot→stream reconciler, roster grouping/filter, status badges,
//! attention counts, the new-session preview, key encoding, and the
//! styled-span→SGR renderer. Shells collect native widgets/events, call
//! into these pure helpers (or the thin `staap_*` wrappers in [`crate::ffi`]),
//! and render whatever comes back. Visual styling stays native per OS;
//! behavior is identical because it is the same code.
//!
//! What stays in the shell: widget construction, event wiring, clipboard
//! access, focus management, and byte transport. Everything else — which
//! row shows where, what a keypress means, what bytes reach the view —
//! is answered here.
//!
//! History: the feed reconciler was previously ported 1:1 into Swift
//! `ShellSupport.TerminalFeed` plus C copies in `native/linux/src/feed.c`
//! and `native/windows/src/feed.c`; the C ports are gone, so every C
//! shell reconciles through this module. The Swift `TerminalFeed`
//! implementation stays as the Swift-idiomatic original (it feeds a
//! SwiftTerm view directly); production may call the core (`staap_feed_delta`)
//! so all shells reconcile identically.

/// ANSI reset the terminal views understand: clear screen, home cursor.
/// Matches Swift `TerminalFeed.clearScreen` and the old `AM_FEED_CLEAR`.
pub const FEED_CLEAR: &str = "\x1b[2J\x1b[H";

/// Feed text that advances a view showing `old` to also show `new`, or
/// `None` when the view is already current. Mirrors Swift
/// `TerminalFeed.delta` case for case:
///
/// - identical snapshots feed nothing;
/// - an append-only snapshot feeds just the suffix (the hot path:
///   streaming agent output and echoed typing);
/// - a scrolled snapshot feeds the new trailing lines (the overlap
///   between the old tail and the new head is already on screen);
/// - anything else (redraw, reflow after resize, cursor-addressed
///   programs) clears and replays the whole snapshot.
///
/// Newlines are normalized to CRLF: views interpret a bare LF as
/// line-feed-only, which would stair-step the output.
pub fn feed_delta(old: &str, new: &str) -> Option<String> {
    if new == old {
        return None;
    }
    if !old.is_empty() {
        if let Some(suffix) = new.strip_prefix(old) {
            if suffix.is_empty() {
                return None;
            }
            return Some(normalize_feed_newlines(suffix));
        }
    } else {
        return Some(normalize_feed_newlines(new));
    }
    let olds: Vec<&str> = old.split('\n').collect();
    let news: Vec<&str> = new.split('\n').collect();
    let overlap = largest_overlap(&olds, &news);
    if overlap > 0 {
        let mut out = String::from("\r\n");
        out.push_str(&news[overlap..].join("\r\n"));
        return Some(out);
    }
    Some(format!("{FEED_CLEAR}{}", normalize_feed_newlines(new)))
}

/// Largest k such that the last k lines of `old` equal the first k lines
/// of `new`. Quadratic in the worst case; snapshots are a few hundred
/// short lines, so this stays in the microseconds.
pub fn largest_overlap(old: &[&str], new: &[&str]) -> usize {
    let max_k = old.len().min(new.len());
    for k in (1..=max_k).rev() {
        if old[old.len() - k..] == new[..k] {
            return k;
        }
    }
    0
}

/// CRLF normalization for fed text: collapse `"\r\n"` to `"\n"` first,
/// then expand every `"\n"` to `"\r\n"` (mirrors Swift
/// `normalizeNewlines`; a CRLF pair counts as one newline).
pub fn normalize_feed_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// True when a roster row passes the sidebar filter: case-insensitive
/// substring over title, project, and id; everything passes when the
/// query is blank (whitespace-trimmed). This is the Swift/Linux match
/// semantics; the Windows shell previously filtered the raw row JSON
/// case-sensitively (which even matched JSON keys), now fixed.
pub fn roster_matches(title: &str, project: &str, id: &str, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    title.to_lowercase().contains(&q)
        || project.to_lowercase().contains(&q)
        || id.to_lowercase().contains(&q)
}

use std::collections::HashMap;

use crate::app::{attention_count, short_cwd, visible_links, ChatSession, Status};
use crate::config::{EffectiveTheme, OsAppearance, ThemePreference};
use crate::keys::{keystroke_to_pty, KeyPress};
use crate::launch::{push_recent_folder, resolve_effective_cli, LaunchSelection, YoloChoice};
use crate::{app, launch};

/// Live-run ceiling shared by every shell (was `gui::runs::MAX_LIVE_RUNS`,
/// hard-coded `10` in two native shells, unbounded on macOS).
pub const MAX_LIVE_RUNS: usize = 10;

/// Pump cadence shared by every shell (50 ms, matching the gpui loop and
/// all three native tickers).
pub const PUMP_INTERVAL_MS: u64 = 50;

/// Bounds for the resizable sidebar (Windows grip + GTK split view share
/// these; SwiftUI uses the system default sidebar width).
pub const SIDEBAR_MIN_PX: f64 = 220.0;
pub const SIDEBAR_MAX_PX: f64 = 480.0;
/// Keyboard nudge step for the resize grip.
pub const SIDEBAR_KEY_STEP_PX: f64 = 8.0;

/// Clamp a sidebar width into range. Pure so every shell shares it.
pub fn clamp_sidebar_width(px: f64) -> f64 {
    px.clamp(SIDEBAR_MIN_PX, SIDEBAR_MAX_PX)
}

/// Roster status code crossing FFI as a plain int (mirrors `staap_status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    Attention,
    Idle,
    Working,
}

impl RowStatus {
    /// Code used across the C ABI (`staap_status`).
    pub fn code(self) -> i32 {
        match self {
            Self::Attention => 0,
            Self::Idle => 1,
            Self::Working => 2,
        }
    }

    pub fn from_code(code: i32) -> Self {
        match code {
            0 => Self::Attention,
            2 => Self::Working,
            _ => Self::Idle,
        }
    }
}

impl From<Status> for RowStatus {
    fn from(s: Status) -> Self {
        match s {
            Status::Attention => Self::Attention,
            Status::Idle => Self::Idle,
            Status::Working => Self::Working,
        }
    }
}

impl From<RowStatus> for Status {
    fn from(s: RowStatus) -> Self {
        match s {
            RowStatus::Attention => Self::Attention,
            RowStatus::Idle => Self::Idle,
            RowStatus::Working => Self::Working,
        }
    }
}

/// Section header text for a status bucket (was three divergent copies:
/// Linux `"Needs input"/"Active"/"Idle"`, macOS
/// `"Needs input"/"Working"/"Idle"`, Windows group order Needs/Idle/Working).
pub fn section_title(status: RowStatus) -> &'static str {
    match status {
        RowStatus::Attention => "Needs input",
        RowStatus::Idle => "Idle",
        RowStatus::Working => "Working",
    }
}

/// Non-color status marker for a row. Every shell shows these glyphs (plus
/// its native accent color when available); no shell may use color alone.
pub fn status_glyph(status: RowStatus) -> &'static str {
    match status {
        RowStatus::Attention => "●",
        RowStatus::Working => "◐",
        RowStatus::Idle => "○",
    }
}

/// One display-ready roster row: everything a shell needs to render a list
/// entry, computed from a [`ChatSession`] plus its live-ness. No toolkit
/// types: shells map these fields onto native labels/badges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterRow {
    pub id: String,
    pub title: String,
    pub project: String,
    pub harness: String,
    pub status: RowStatus,
    /// `true` once this shell attached a live PTY (the core run registry
    /// knows; see [`crate::runs`]).
    pub live: bool,
    /// Short folder tail for the detail line (`repo/sub` or `…` cut).
    pub folder_short: String,
    /// `project · harness · <age>`, the shared subtitle/detail format.
    pub detail: String,
    /// Total parsed-link count (PR + related), for the `N links` badge.
    pub link_count: usize,
    /// Links folded behind the `N more` disclosure.
    pub hidden_links: usize,
    /// `true` when the row still matches the sidebar filter.
    pub visible: bool,
}

/// Build the display row for one session.
pub fn roster_row(session: &ChatSession, live: bool, filter: &str, now: i64) -> RosterRow {
    let status = RowStatus::from(session.status);
    let (_, hidden_pr) = visible_links(&session.pr_links);
    let (_, hidden_rel) = visible_links(&session.related_links);
    let folder_short = session.cwd.as_deref().map(short_cwd).unwrap_or_default();
    let detail = format!(
        "{} · {} · {}",
        session.project,
        session.harness,
        relative_age(now, session.last_active)
    );
    RosterRow {
        id: session.id.clone(),
        title: session.title.clone(),
        project: session.project.clone(),
        harness: session.harness.clone(),
        status,
        live,
        folder_short,
        detail,
        link_count: session.pr_links.len() + session.related_links.len(),
        hidden_links: hidden_pr + hidden_rel,
        visible: row_matches_filter(session, filter),
    }
}

/// Case-insensitive substring match over title/project/id on a session —
/// the single sidebar-filter rule over [`ChatSession`]. (The
/// title/project/id/query form lives in [`roster_matches`] below.)
pub fn row_matches_filter(session: &ChatSession, query: &str) -> bool {
    roster_matches(&session.title, &session.project, &session.id, query)
}

/// Group row indices into urgency sections: working, then idle —
/// (idle holds needs-input rows first — issue #105), then history (rows
/// with no live PTY) last. Returns `(header, indices)` pairs, skipping
/// nothing: empty groups still render headers. This is the single rule
/// replacing the Linux sort-fn, the macOS section builder, and Windows order.
pub fn group_sections(
    sessions: &[ChatSession],
    live: &dyn Fn(&str) -> bool,
) -> Vec<(String, Vec<usize>)> {
    let mut attn = Vec::new();
    let mut working = Vec::new();
    let mut idle = Vec::new();
    let mut history = Vec::new();
    // Recent-first within a group (mirrors the Linux comparator).
    let mut order: Vec<usize> = (0..sessions.len()).collect();
    order.sort_by(|&a, &b| {
        sessions[b]
            .last_active
            .cmp(&sessions[a].last_active)
            .then_with(|| sessions[a].title.cmp(&sessions[b].title))
    });
    for i in order {
        if !live(&sessions[i].id) {
            history.push(i);
            continue;
        }
        match sessions[i].status {
            Status::Attention => attn.push(i),
            Status::Working => working.push(i),
            Status::Idle => idle.push(i),
        }
    }
    // Issue #105: idle and needs-input share one section (attention
    // rows first, so urgency still reads top-down); the per-row glyph
    // and the needs-input badge keep saying which idle rows wait.
    let mut idle_all = attn;
    idle_all.extend(idle);
    vec![
        (
            format!("{} ({})", section_title(RowStatus::Working), working.len()),
            working,
        ),
        (
            format!("{} ({})", section_title(RowStatus::Idle), idle_all.len()),
            idle_all,
        ),
        (format!("History ({})", history.len()), history),
    ]
}

/// Number of rows needing input — the header badge count.
pub fn needs_input_count(sessions: &[ChatSession]) -> usize {
    attention_count(sessions)
}

// --- Key encoding -----------------------------------------------------------

/// What a shell key event means after core encoding: forward these bytes
/// to the child via `staap_write`, or keep the key for the native control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyDecision {
    /// Forward these bytes to the child.
    Forward(Vec<u8>),
    /// Leave to the native control (copy/paste, modifiers, media keys).
    Keep,
}

/// Encode one logical keypress into child bytes, using the same table as
/// the gpui shell (`crate::keys::keystroke_to_pty`). Shells translate their
/// native key event into `(key, key_char, ctrl, alt)` and call this —
/// no per-shell key tables anymore.
pub fn encode_key(key: &str, key_char: Option<&str>, ctrl: bool, alt: bool) -> KeyDecision {
    // Reserve rule (was duplicated in Linux `on_key_pressed` and Windows
    // `keep_for_control`): Ctrl+Shift+C / Ctrl+Shift+V stay with the
    // native control for copy/paste. Shells pass `key` through with the
    // shift state folded in: "C"/"V" with ctrl already consumed these.
    let press = KeyPress {
        key,
        key_char,
        ctrl,
        alt,
    };
    match keystroke_to_pty(&press) {
        Some(bytes) => KeyDecision::Forward(bytes),
        None => KeyDecision::Keep,
    }
}

/// Ctrl+Shift+C / Ctrl+Shift+V stay with the native text control for
/// copy/paste and must never reach the encoder. Shells that cannot
/// express the reserve inside `encode_key` (Linux VTE, WinUI TextBox)
/// gate on this first.
pub fn keep_for_control(key: &str, ctrl: bool, shift: bool) -> bool {
    ctrl && shift && (key.eq_ignore_ascii_case("c") || key.eq_ignore_ascii_case("v"))
}

// --- Styled-span → SGR renderer ----------------------------------------------

/// One styled run inside a snapshot row, matching the core's
/// `staap_spans_json` shape (`{text,fg,bg,bold,italic,underline}` with
/// `fg`/`bg` as `[r,g,b]` or null). Ports Swift `AnsiFeed`: every span
/// carries a complete SGR sequence (reset + attributes), so any fragment
/// of a render is self-contained and `feed_delta` works on rendered
/// strings unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnsiSpan {
    pub text: String,
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

fn sgr_for(span: &AnsiSpan) -> String {
    let mut params = vec!["0".to_string()];
    if span.bold {
        params.push("1".to_string());
    }
    if span.italic {
        params.push("3".to_string());
    }
    if span.underline {
        params.push("4".to_string());
    }
    if let Some((r, g, b)) = span.fg {
        params.push(format!("38;2;{r};{g};{b}"));
    }
    if let Some((r, g, b)) = span.bg {
        params.push(format!("48;2;{r};{g};{b}"));
    }
    format!("\x1b[{}m", params.join(";"))
}

/// Render decoded snapshot rows (one span vec per grid row) to an SGR
/// stream with `\n` row separators (the delta reconciler still normalizes
/// those to CRLF before feeding the view).
pub fn render_ansi(rows: &[Vec<AnsiSpan>]) -> String {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|s| format!("{}{}", sgr_for(s), s.text))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn rgb_value(v: &serde_json::Value) -> Option<(u8, u8, u8)> {
    let a = v.as_array()?;
    if a.len() != 3 {
        return None;
    }
    Some((
        a[0].as_u64()? as u8,
        a[1].as_u64()? as u8,
        a[2].as_u64()? as u8,
    ))
}

/// Render an `staap_spans_json` document, or `None` when it does not decode
/// (the pump then falls back to the plain-text snapshot).
pub fn render_ansi_json(json: &str) -> Option<String> {
    let rows: Vec<Vec<serde_json::Value>> = serde_json::from_str(json).ok()?;
    let mut out: Vec<Vec<AnsiSpan>> = Vec::with_capacity(rows.len());
    for row in rows {
        let mut spans = Vec::with_capacity(row.len());
        for s in row {
            let text = s.get("text")?.as_str()?.to_string();
            spans.push(AnsiSpan {
                text,
                fg: s.get("fg").and_then(rgb_value),
                bg: s.get("bg").and_then(rgb_value),
                bold: s.get("bold").and_then(|v| v.as_bool()).unwrap_or(false),
                italic: s.get("italic").and_then(|v| v.as_bool()).unwrap_or(false),
                underline: s
                    .get("underline")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            });
        }
        out.push(spans);
    }
    Some(render_ansi(&out))
}

// --- Picker helpers ----------------------------------------------------------

/// The 2D new-session preview line (`runs: muse in ~/api + yolo`).
/// Shells render their folder/CLI/yolo widgets natively and call this for
/// the footer + the repeat-last toast — one copy instead of four.
pub fn spawn_preview(cli: &str, folder: &str, yolo_value: i32) -> String {
    let cli = if cli.is_empty() { "muse" } else { cli };
    let folder = folder.trim();
    let tag = if yolo_value > 0 {
        " + yolo"
    } else if yolo_value < 0 {
        " (yolo off)"
    } else {
        ""
    };
    if folder.is_empty() {
        format!("runs: {cli}{tag}")
    } else {
        format!("runs: {cli} in {folder}{tag}")
    }
}

/// Blank/whitespace folder means inherit (`None`); otherwise the trimmed
/// folder.
pub fn effective_folder(folder: &str) -> Option<String> {
    let t = folder.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Segmented-control index → tri-state yolo int for `staap_spawn_launch`
/// (1 = force on, 2 = force off, else config default).
pub fn yolo_value(selected: i32) -> i32 {
    if selected == 1 {
        1
    } else if selected == 2 {
        -1
    } else {
        0
    }
}

/// Resolve the effective CLI label for a repeat-last spawn (explicit id,
/// else the core's last-used / configured / autodetected resolution).
pub fn effective_cli_label(
    explicit: Option<&str>,
    last_used: Option<&str>,
    default_cli: Option<&str>,
    catalog: &[launch::AvailableCli],
) -> String {
    resolve_effective_cli(
        explicit.filter(|s| !s.is_empty()),
        last_used,
        default_cli,
        catalog,
    )
}

/// Confirm a picker into a resolved [`LaunchSelection`]: blank folder
/// inherits (`None`, `~` expands), yolo resolves against the per-agent
/// config default.
pub fn confirm_pick(
    cli: &str,
    folder: &str,
    yolo: YoloChoice,
    yolo_default: bool,
) -> LaunchSelection {
    let cwd = effective_folder(folder).and_then(|f| app::expand_cwd_input(&f));
    LaunchSelection::new(cli.to_string(), cwd, yolo.resolve(yolo_default))
}

/// Record a confirmed launch into MRU memory (last-used CLI + folder
/// recents): pure, shared by shells that keep their own config copy.
pub fn note_launch_memory(
    recents: &mut Vec<String>,
    last_cli: &mut Option<String>,
    cli: &str,
    cwd: Option<&str>,
) {
    if !cli.is_empty() && launch::SUPPORTED_CLIS.contains(&cli) {
        *last_cli = Some(cli.to_string());
    }
    if let Some(dir) = cwd {
        push_recent_folder(recents, dir);
    }
}

// --- Theme / appearance ------------------------------------------------------

/// Resolve the configured theme choice + the live OS appearance to a
/// concrete dark/light answer. The core equivalent of the removed per-shell
/// theme pickers: shells follow the OS by default (no manual override
/// chrome); where a shell still needs the resolved value it asks here.
pub fn resolve_theme(pref: ThemePreference, appearance: OsAppearance) -> EffectiveTheme {
    pref.resolve(appearance)
}

/// Map a shell's dark-mode bit onto the core appearance type.
pub fn appearance_for(dark: bool) -> OsAppearance {
    if dark {
        OsAppearance::Dark
    } else {
        OsAppearance::Light
    }
}

// --- Run-registry views ------------------------------------------------------

/// Display snapshot of the core run registry for dumb shells: the rows in
/// core order plus per-row live-ness. Shells render this and nothing else.
pub struct RosterView {
    pub rows: Vec<RosterRow>,
    pub needs_input: usize,
}

pub fn roster_view(
    sessions: &[ChatSession],
    is_live: &dyn Fn(&str) -> bool,
    filter: &str,
) -> RosterView {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let rows = sessions
        .iter()
        .map(|s| roster_row(s, is_live(&s.id), filter, now))
        .collect::<Vec<_>>();
    RosterView {
        needs_input: needs_input_count(sessions),
        rows,
    }
}

/// Live-map bookkeeping shared by shells that keep `HashMap<id, LivePty>`
/// locally: cap check + eviction pick. The core registry (see
/// [`crate::runs`]) owns this for FFI shells; local-map shells call these
/// so the policy is still one copy.
pub fn at_live_cap(live_count: usize) -> bool {
    live_count >= MAX_LIVE_RUNS
}

/// Pick an eviction victim among exited runs: the stalest `last_output`
/// first. `runs` maps id → (exited, last_output_unix). `None` when every
/// live run is still running (refuse, don't reap a live child).
pub fn eviction_victim(runs: &HashMap<String, (bool, i64)>) -> Option<String> {
    runs.iter()
        .filter(|(_, (exited, _))| *exited)
        .min_by_key(|(_, (_, last))| *last)
        .map(|(id, _)| id.clone())
}

/// Glanceable relative-age label for a roster row (`last_active` in unix
/// seconds): `just now` / `Nm ago` / `Nh ago` / `Nd ago`. Mirrors the
/// Swift sidebar; future times clamp to `just now`.
pub fn relative_age(now_secs: i64, then_secs: i64) -> String {
    let delta = now_secs.saturating_sub(then_secs).max(0);
    if delta < 60 {
        "just now".to_string()
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_snapshots_feed_nothing() {
        assert_eq!(feed_delta("a\nb", "a\nb"), None);
        assert_eq!(feed_delta("", ""), None);
    }

    #[test]
    fn appended_output_feeds_suffix_only() {
        assert_eq!(
            feed_delta("hello", "hello world"),
            Some(" world".to_string())
        );
    }

    #[test]
    fn appended_lines_normalize_to_crlf() {
        assert_eq!(feed_delta("a", "a\nb\n"), Some("\r\nb\r\n".to_string()));
    }

    #[test]
    fn first_snapshot_feeds_whole_screen() {
        assert_eq!(feed_delta("", "ready\n$ "), Some("ready\r\n$ ".to_string()));
    }

    #[test]
    fn scrolled_snapshot_feeds_new_trailing_lines() {
        assert_eq!(feed_delta("a\nb", "b\nc"), Some("\r\nc".to_string()));
        assert_eq!(
            feed_delta("a\nb\nc", "c\nd\ne"),
            Some("\r\nd\r\ne".to_string())
        );
    }

    #[test]
    fn redraw_clears_and_replays() {
        assert_eq!(
            feed_delta("menu: [x]", "other screen"),
            Some(format!("{FEED_CLEAR}other screen"))
        );
    }

    #[test]
    fn resize_reflow_falls_back_to_redraw() {
        let feed =
            feed_delta("a very long line here", "a very\nlong line\nhere").expect("reflow replays");
        assert!(feed.starts_with(FEED_CLEAR), "feed: {feed:?}");
    }

    #[test]
    fn overlap_counts_shared_edge_lines() {
        assert_eq!(largest_overlap(&["a", "b"], &["b", "c"]), 1);
        assert_eq!(largest_overlap(&["a"], &["b"]), 0);
        assert_eq!(largest_overlap(&[], &["b"]), 0);
    }

    #[test]
    fn styled_spans_do_not_break_the_hot_path() {
        // Colors must not break append detection: appended styled output
        // still feeds just the suffix, carrying its own style state
        // (mirrors AnsiFeedTests.testStyledAppendFeedsSuffixOnly).
        assert_eq!(
            feed_delta("\x1b[0mhello", "\x1b[0mhello\x1b[0;38;2;205;0;0m red"),
            Some("\x1b[0;38;2;205;0;0m red".to_string())
        );
    }

    #[test]
    fn filter_matches_title_project_id_case_insensitively() {
        assert!(roster_matches("Shop App", "shop", "a1", ""));
        assert!(roster_matches("Shop App", "shop", "a1", "  "));
        assert!(roster_matches("Shop App", "shop", "a1", "shop"));
        assert!(roster_matches("Shop App", "shop", "a1", "SHOP"));
        assert!(roster_matches("Shop App", "shop", "a1", "a1"));
        assert!(!roster_matches("Shop App", "shop", "a1", "blog"));
        // The old Windows bug: the query must not match JSON syntax.
        assert!(!roster_matches("Shop", "shop", "a1", "\"title\""));
    }

    #[test]
    fn age_labels_stay_glanceable() {
        let now = 1_786_000_000;
        assert_eq!(relative_age(now, now), "just now");
        assert_eq!(relative_age(now, now - 59), "just now");
        assert_eq!(relative_age(now, now + 30), "just now");
        assert_eq!(relative_age(now, now - 300), "5m ago");
        assert_eq!(relative_age(now, now - 7200), "2h ago");
        assert_eq!(relative_age(now, now - 172_800), "2d ago");
    }

    fn sess(id: &str, status: Status, last_active: i64) -> ChatSession {
        ChatSession {
            id: id.into(),
            title: format!("{id} title"),
            project: "proj".into(),
            status,
            harness: crate::app::HARNESS_MUSE.into(),
            last_active,
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

    #[test]
    fn section_titles_use_working_not_active() {
        assert_eq!(section_title(RowStatus::Attention), "Needs input");
        assert_eq!(section_title(RowStatus::Idle), "Idle");
        assert_eq!(section_title(RowStatus::Working), "Working");
    }

    #[test]
    fn status_glyphs_are_never_blank() {
        for st in [RowStatus::Attention, RowStatus::Idle, RowStatus::Working] {
            assert!(!status_glyph(st).is_empty());
        }
        assert_ne!(
            status_glyph(RowStatus::Idle),
            status_glyph(RowStatus::Attention)
        );
    }

    #[test]
    fn session_filter_matches_title_project_and_id() {
        let s = sess("abc-123", Status::Idle, 1);
        assert!(row_matches_filter(&s, ""));
        assert!(row_matches_filter(&s, "abc-123 tit"));
        assert!(row_matches_filter(&s, "PROJ"));
        assert!(row_matches_filter(&s, "abc-"));
        assert!(!row_matches_filter(&s, "zzz"));
    }

    #[test]
    fn groups_order_working_idle_then_history() {
        let sessions = vec![
            sess("h", Status::Working, 9),
            sess("a", Status::Attention, 1),
            sess("i", Status::Idle, 5),
            sess("w", Status::Working, 7),
        ];
        let live = |id: &str| id != "h";
        // Issue #105: needs-input shares the Idle section (attention first).
        let groups = group_sections(&sessions, &live);
        assert_eq!(groups.len(), 3);
        let ids = |n: usize| {
            groups[n]
                .1
                .iter()
                .map(|&i| sessions[i].id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(0), vec!["w"]);
        assert_eq!(ids(1), vec!["a", "i"]);
        assert_eq!(ids(2), vec!["h"]);
        assert!(groups[0].0.starts_with("Working (1)"));
        assert!(groups[1].0.starts_with("Idle (2)"));
        assert!(groups[2].0.starts_with("History (1)"));
    }

    #[test]
    fn key_encoding_matches_gui_table() {
        assert_eq!(
            encode_key("enter", None, false, false),
            KeyDecision::Forward(b"\r".to_vec())
        );
        assert_eq!(
            encode_key("up", None, false, false),
            KeyDecision::Forward(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key("a", Some("a"), false, false),
            KeyDecision::Forward(b"a".to_vec())
        );
        assert_eq!(
            encode_key("c", None, true, false),
            KeyDecision::Forward(vec![0x03])
        );
        assert_eq!(encode_key("shift", None, false, false), KeyDecision::Keep);
        assert!(keep_for_control("c", true, true));
        assert!(keep_for_control("V", true, true));
        assert!(!keep_for_control("c", true, false));
    }

    #[test]
    fn ansi_render_matches_swift_contract() {
        let json = r#"[[{"text":"red","fg":[205,0,0],"bg":null,"bold":false,"italic":false,"underline":false}]]"#;
        assert_eq!(
            render_ansi_json(json).as_deref(),
            Some("\u{1b}[0;38;2;205;0;0mred")
        );
        let plain = r#"[[{"text":"plain","fg":null,"bg":null,"bold":false,"italic":false,"underline":false}]]"#;
        assert_eq!(render_ansi_json(plain).as_deref(), Some("\u{1b}[0mplain"));
        assert_eq!(render_ansi_json("not json"), None);
        assert_eq!(render_ansi_json(""), None);
    }

    #[test]
    fn preview_names_cli_folder_and_yolo() {
        assert_eq!(
            spawn_preview("muse", "/tmp/api", 1),
            "runs: muse in /tmp/api + yolo"
        );
        assert_eq!(spawn_preview("claude", "", 0), "runs: claude");
        assert_eq!(spawn_preview("muse", "  ", -1), "runs: muse (yolo off)");
        assert_eq!(spawn_preview("", "", 0), "runs: muse");
        assert_eq!(yolo_value(0), 0);
        assert_eq!(yolo_value(1), 1);
        assert_eq!(yolo_value(2), -1);
        assert_eq!(
            effective_folder("  /tmp/api  ").as_deref(),
            Some("/tmp/api")
        );
        assert_eq!(effective_folder("   "), None);
    }

    #[test]
    fn clamp_sidebar_width_holds_bounds() {
        assert_eq!(clamp_sidebar_width(100.0), SIDEBAR_MIN_PX);
        assert_eq!(clamp_sidebar_width(900.0), SIDEBAR_MAX_PX);
        assert_eq!(clamp_sidebar_width(300.0), 300.0);
    }

    #[test]
    fn eviction_picks_stalest_exited_and_spares_live() {
        let mut runs = HashMap::new();
        runs.insert("live".to_string(), (false, 0));
        runs.insert("old".to_string(), (true, 10));
        runs.insert("new".to_string(), (true, 90));
        assert_eq!(eviction_victim(&runs).as_deref(), Some("old"));
        runs.remove("old");
        runs.remove("new");
        assert_eq!(eviction_victim(&runs), None);
    }

    #[test]
    fn roster_view_reports_needs_input_count() {
        let sessions = vec![
            sess("a", Status::Attention, 3),
            sess("b", Status::Idle, 2),
            sess("c", Status::Attention, 1),
        ];
        let view = roster_view(&sessions, &|_| true, "");
        assert_eq!(view.needs_input, 2);
        assert_eq!(view.rows.len(), 3);
        assert!(view.rows.iter().all(|r| r.live && r.visible));
        let filtered = roster_view(&sessions, &|_| true, "zzz-no-match");
        assert!(filtered.rows.iter().all(|r| !r.visible));
    }
}
