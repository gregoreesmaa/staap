//! Per-chat GitHub PR link extraction and status iconification via `gh` CLI.
//!
//! This module extends link extraction to include real-time PR state queries
//! using the GitHub CLI tool (`gh`). It supports graceful degradation when
//! `gh` is unavailable, cached state to avoid excessive API calls, and text-only
//! Unicode glyphs for display in sidebars.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Deserialize;

/// Refresh cadence for PR states (issue #115): one `gh` spawn per URL
/// at most this often; fresher entries are served from memory.
pub const PR_CACHE_TTL: Duration = Duration::from_secs(300);

/// Bound on the in-memory PR state cache (bounded-growth rule: no
/// accumulate-forever store; eviction drops expired entries first).
const PR_CACHE_MAX_URLS: usize = 256;

/// PR states extracted from `gh pr view` JSON response.
///
/// Serialized into [`crate::app::ChatSession`] persistence, so old state
/// files (without the field) still load via `#[serde(default)]` on the
/// owning field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
pub enum PrState {
    /// Open and awaiting review - default open state
    OpenUnreviewed,
    /// Open with at least one approval
    OpenApproved(u32),
    /// Open with changes requested by reviewer
    OpenChangesRequested,
    /// Closed without merging (not a draft)
    Closed,
    /// Successfully merged into base branch
    Merged,
    /// Draft PR (not ready for review yet)
    Draft,
    /// Unknown state due to gh unavailable or API error
    Unknown,
}

/// Display icon for a PR status. Maps PrState → Unicode glyph + optional approval count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrStatusIcon {
    pub glyph: &'static str,
    pub approval_count: Option<u32>,
}

/// GitHub CLI availability check. Returns true if `gh` command exists on PATH.
pub fn gh_cli_available() -> bool {
    GH_AVAILABLE.load(Ordering::SeqCst)
}

/// Static flag tracking whether gh CLI is available (checked once at startup).
static GH_AVAILABLE: LazyLock<AtomicBool> = LazyLock::new(|| {
    let available = std::process::Command::new("gh")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    AtomicBool::new(available)
});

/// Convert PrState to its pinned display glyph (text-only Unicode).
/// Always a static string: the approval count rides separately in
/// [`PrStatusIcon::approval_count`] / [`pr_status_label`], so rendering
/// can never leak (no per-call allocation, fixing the old `Box::leak`
/// on every approved render).
pub fn pr_status_glyph(state: PrState) -> &'static str {
    match state {
        PrState::OpenUnreviewed => "◴",  // white circle left quarter shaded
        PrState::OpenApproved(_) => "✓", // bare checkmark; count via label
        PrState::OpenChangesRequested => "⚠", // warning sign
        PrState::Closed => "✕",          // X mark
        PrState::Merged => "✔",          // heavy checkmark
        PrState::Draft => "⋯",           // mid-line ellipsis
        PrState::Unknown => "–",         // en dash (fallback when gh unavailable)
    }
}

/// Approval count carried by a state, if any.
pub fn pr_approval_count(state: PrState) -> Option<u32> {
    match state {
        PrState::OpenApproved(n) => Some(n),
        _ => None,
    }
}

/// Single display label for a state: the pinned glyph with the approval
/// count appended for approvals (`✓3`), bare glyph otherwise. Owned
/// (built by the caller on demand) — the leak-free replacement for
/// formatting inside [`pr_status_glyph`].
pub fn pr_status_label(state: PrState) -> String {
    match pr_approval_count(state) {
        Some(n) => format!("✓{n}"),
        None => pr_status_glyph(state).to_string(),
    }
}

/// Subset of `gh pr view --json` this module reads. Field names verified
/// against the installed gh 2.92.0 (`gh pr view --help` JSON FIELDS plus
/// live `gh pr view`/`gh pr list` probes): `state` is OPEN/CLOSED/MERGED,
/// `reviewDecision` is `""` (none), REVIEW_REQUIRED, APPROVED, or
/// CHANGES_REQUESTED, `reviews[].state` carries per-review verdicts with
/// `author.login`. Unknown extra fields (e.g. `mergeStateStatus`,
/// `number`) are ignored by construction.
#[derive(Debug, Clone, Default, Deserialize)]
struct GhPrView {
    #[serde(default)]
    state: String,
    #[serde(rename = "isDraft", default)]
    is_draft: bool,
    #[serde(rename = "reviewDecision", default)]
    review_decision: String,
    #[serde(default)]
    reviews: Vec<GhReview>,
}

/// One entry of the `reviews` array in `gh pr view --json reviews`.
#[derive(Debug, Clone, Default, Deserialize)]
struct GhReview {
    #[serde(default)]
    state: String,
    #[serde(default)]
    author: Option<GhAuthor>,
}

/// Review author identity; only the login feeds the approval count.
#[derive(Debug, Clone, Default, Deserialize)]
struct GhAuthor {
    #[serde(default)]
    login: String,
}

/// Map a decoded `gh pr view` document to a [`PrState`].
fn pr_state_from_view(view: &GhPrView) -> PrState {
    match view.state.to_ascii_uppercase().as_str() {
        "MERGED" => return PrState::Merged,
        "CLOSED" => return PrState::Closed,
        "OPEN" => {}
        _ => return PrState::Unknown,
    }
    if view.is_draft {
        return PrState::Draft;
    }
    match view.review_decision.to_ascii_uppercase().as_str() {
        "APPROVED" => PrState::OpenApproved(distinct_approvers(view).max(1)),
        "CHANGES_REQUESTED" => PrState::OpenChangesRequested,
        _ => PrState::OpenUnreviewed,
    }
}

/// Distinct reviewer logins whose review state is APPROVED.
fn distinct_approvers(view: &GhPrView) -> u32 {
    let mut logins = HashSet::new();
    for review in &view.reviews {
        if !review.state.eq_ignore_ascii_case("APPROVED") {
            continue;
        }
        if let Some(author) = review.author.as_ref() {
            if !author.login.is_empty() {
                logins.insert(author.login.as_str());
            }
        }
    }
    logins.len() as u32
}

/// Parse raw `gh pr view --json ...` bytes into a [`PrState`]. Pure:
/// fixture bytes in, state out — the injected seam tests use instead of
/// spawning `gh`. Any failure (bad JSON, missing shape) is
/// [`PrState::Unknown`], never an error.
pub fn pr_state_from_json_bytes(bytes: &[u8]) -> PrState {
    serde_json::from_slice::<GhPrView>(bytes)
        .map(|view| pr_state_from_view(&view))
        .unwrap_or(PrState::Unknown)
}

/// One cached resolve: the state plus when `gh` produced it.
struct PrCacheEntry {
    state: PrState,
    at: Instant,
}

/// In-memory PR state cache keyed by full PR URL (5-min TTL, bounded).
static PR_CACHE: LazyLock<Mutex<HashMap<String, PrCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cache_lookup(url: &str, now: Instant) -> Option<PrState> {
    let cache = PR_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.get(url).and_then(|entry| {
        if now.saturating_duration_since(entry.at) < PR_CACHE_TTL {
            Some(entry.state)
        } else {
            None
        }
    })
}

fn cache_store(url: &str, state: PrState, now: Instant) {
    let mut cache = PR_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= PR_CACHE_MAX_URLS {
        cache.retain(|_, entry| now.saturating_duration_since(entry.at) < PR_CACHE_TTL);
        if cache.len() >= PR_CACHE_MAX_URLS {
            if let Some(key) = cache.keys().next().cloned() {
                cache.remove(&key);
            }
        }
    }
    cache.insert(url.to_string(), PrCacheEntry { state, at: now });
}

/// Spawn boundary for `gh`: run
/// `gh pr view <n> --repo <owner/repo> --json
/// number,state,isDraft,reviewDecision,mergeStateStatus,reviews` and
/// return stdout on success, `None` on missing binary, non-zero exit, or
/// spawn failure. Production only — tests inject this boundary.
fn fetch_pr_json(repo: &str, pr_number: u64) -> Option<Vec<u8>> {
    let output = std::process::Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_number.to_string(),
            "--repo",
            repo,
            "--json",
            "number,state,isDraft,reviewDecision,mergeStateStatus,reviews",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

/// Resolve a PR URL to its state with injected seams: `gh_available`
/// stands in for [`gh_cli_available`] and `fetch` for [`fetch_pr_json`],
/// so tests cover the TTL and the `gh`-absent path without spawning.
/// Failures at every layer (absent binary, unparsable URL, spawn
/// failure, bad JSON, rate-limit non-zero exit) yield
/// [`PrState::Unknown`], never an error.
pub(crate) fn resolve_pr_state_with(
    url: &str,
    now: Instant,
    gh_available: bool,
    fetch: &mut dyn FnMut(&str, u64) -> Option<Vec<u8>>,
) -> PrState {
    if !gh_available {
        return PrState::Unknown;
    }
    let Some((repo, number)) = parse_pr_url(url) else {
        return PrState::Unknown;
    };
    if let Some(cached) = cache_lookup(url, now) {
        return cached;
    }
    let state = fetch(repo, number)
        .map(|body| pr_state_from_json_bytes(&body))
        .unwrap_or(PrState::Unknown);
    cache_store(url, state, now);
    state
}

/// Parse repository owner/repo and PR number from URL.
pub fn parse_pr_url(url: &str) -> Option<(&str, u64)> {
    // Expected format: https://github.com/OWNER/REPO/pull/NUMBER or http://
    const HTTPS_PREFIX: &str = "https://github.com/";
    const HTTP_PREFIX: &str = "http://github.com/";
    const PULL_PREFIX: &str = "/pull/";

    let rest = if let Some(rest) = url.strip_prefix(HTTPS_PREFIX) {
        rest
    } else if let Some(rest) = url.strip_prefix(HTTP_PREFIX) {
        rest
    } else {
        return None;
    };

    // Extract owner/repo (up to /pull/) and get position after it
    let (repo_part, _after_repo) = rest.split_once(PULL_PREFIX)?;

    // Parse PR number from rest after /pull/
    let pr_start = repo_part.len() + PULL_PREFIX.len();
    let pr_str = &rest[pr_start..];

    // Extract just the numeric part (URLs might have trailing query params)
    let pr_num: u64 = String::from_utf8(
        pr_str
            .bytes()
            .take_while(|b| b.is_ascii_digit())
            .collect::<Vec<_>>(),
    )
    .ok()?
    .parse()
    .ok()?;

    Some((repo_part, pr_num))
}

/// Query PR state via `gh pr view --json` (issue #115). Served from
/// the 5-minute TTL cache when fresh; spawns `gh` otherwise. Any
/// failure (no binary, non-zero exit, bad JSON) is [`PrState::Unknown`].
pub fn query_pr_state(repo: &str, pr_number: u64) -> PrState {
    let url = format!("https://github.com/{repo}/pull/{pr_number}");
    let mut fetch = |r: &str, n: u64| fetch_pr_json(r, n);
    resolve_pr_state_with(url.as_str(), Instant::now(), gh_cli_available(), &mut fetch)
}

/// Resolve the stored state for a PR URL (issue #115). Served from
/// the 5-minute TTL cache when fresh; one `gh` spawn per URL per 5
/// minutes at most; [`PrState::Unknown`] when `gh` is absent or fails.
pub fn resolve_pr_state(url: &str) -> PrState {
    let mut fetch = |r: &str, n: u64| fetch_pr_json(r, n);
    resolve_pr_state_with(url, Instant::now(), gh_cli_available(), &mut fetch)
}

/// Resolve the display icon for a PR URL (issue #115): the pinned
/// glyph plus approval count for the state from [`resolve_pr_state`].
pub fn resolve_pr_icon(url: &str) -> PrStatusIcon {
    let state = resolve_pr_state(url);
    PrStatusIcon {
        glyph: pr_status_glyph(state),
        approval_count: pr_approval_count(state),
    }
}

/// Compiled once: per-run-per-tick compilation was the hottest alloc in
/// the pump loop. Match semantics are unchanged (see [`extract_pr_links`]).
static PR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/pull/\d+")
        .expect("static PR regex")
});

/// Full issue URLs share the PR row treatment (click-to-copy under the
/// run title); bare `owner/repo#123` refs stay ignored, as with PRs.
static ISSUE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/issues/\d+")
        .expect("static issue regex")
});

/// Full commit URLs (`/commit/<sha>`), same row treatment.
static COMMIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/commit/[0-9a-f]{7,40}")
        .expect("static commit regex")
});

/// Extract `https://github.com/<owner>/<repo>/pull/<n>` URLs (also bare
/// `owner/repo#123` references are ignored — only full PR URLs count).
/// Preserves first-seen order, deduplicated, trailing punctuation trimmed.
pub fn extract_pr_links(text: &str) -> Vec<String> {
    collect_unique(&PR_RE, text)
}

/// Extract `https://github.com/<owner>/<repo>/issues/<n>` URLs.
/// Same ordering and dedupe contract as [`extract_pr_links`].
pub fn extract_issue_links(text: &str) -> Vec<String> {
    collect_unique(&ISSUE_RE, text)
}

/// Extract `https://github.com/<owner>/<repo>/commit/<sha>` URLs.
/// Same ordering and dedupe contract as [`extract_pr_links`].
pub fn extract_commit_links(text: &str) -> Vec<String> {
    collect_unique(&COMMIT_RE, text)
}

fn collect_unique(re: &Regex, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for m in re.find_iter(text) {
        let url = m.as_str();
        if seen.insert(url) {
            out.push(url.to_string());
        }
    }
    out
}

/// Display sort key for accumulated sidepanel links: the lowercased link
/// with any trailing ASCII digit run compared numerically, so
/// `…/pull/9` sorts before `…/pull/42` and `src/app.rs:9` before
/// `src/app.rs:12`. The shared prefix compares lexically, which groups by
/// owner/repo/kind by construction. This is the single ordering rule
/// behind [`crate::app::ChatSession::push_links`]: every shell renders
/// stored order, so all sidepanels sort identically with no per-shell
/// sort.
pub fn link_sort_key(link: &str) -> (String, u64, String) {
    let lower = link.to_lowercase();
    let digits = lower.bytes().rev().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        (lower.clone(), u64::MAX, lower)
    } else {
        let (prefix, num) = lower.split_at(lower.len() - digits);
        (prefix.to_string(), num.parse().unwrap_or(u64::MAX), lower)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_dedupes_pull_urls() {
        let text = "see https://github.com/acme/app/pull/42, also \
                    https://github.com/acme/app/pull/42 and \
                    https://github.com/other/repo/pull/7.";
        assert_eq!(
            extract_pr_links(text),
            vec![
                "https://github.com/acme/app/pull/42",
                "https://github.com/other/repo/pull/7"
            ]
        );
    }

    #[test]
    fn ignores_issue_links_and_bare_refs() {
        assert!(extract_pr_links("acme/app#123 https://github.com/a/b/issues/5").is_empty());
    }

    #[test]
    fn extracts_issue_and_commit_urls_with_pr_ordering() {
        let text = "bug https://github.com/acme/app/issues/9, \
                    fix https://github.com/acme/app/commit/abcdef1234567890, \
                    again https://github.com/acme/app/issues/9";
        assert_eq!(
            extract_issue_links(text),
            vec!["https://github.com/acme/app/issues/9"]
        );
        assert_eq!(
            extract_commit_links(text),
            vec!["https://github.com/acme/app/commit/abcdef1234567890"]
        );
        // PR extraction stays blind to the new kinds.
        assert!(extract_pr_links(text).is_empty());
        // Short SHAs are not commits.
        assert!(extract_commit_links("https://github.com/a/b/commit/abc").is_empty());
    }

    #[test]
    fn empty_in_empty_out() {
        assert!(extract_pr_links("").is_empty());
    }

    #[test]
    fn link_sort_key_orders_trailing_numbers_numerically() {
        let mut links = [
            "https://github.com/acme/app/pull/42".to_string(),
            "https://github.com/acme/app/pull/100".to_string(),
            "https://github.com/acme/app/pull/9".to_string(),
        ];
        links.sort_by_key(|l| link_sort_key(l));
        assert_eq!(
            links,
            [
                "https://github.com/acme/app/pull/9".to_string(),
                "https://github.com/acme/app/pull/42".to_string(),
                "https://github.com/acme/app/pull/100".to_string()
            ]
        );
        // Case-insensitive grouping: same repo in different cases stays
        // adjacent, ordered by number.
        let mut mixed = [
            "https://github.com/ACME/app/pull/10".to_string(),
            "https://github.com/acme/app/pull/2".to_string(),
        ];
        mixed.sort_by_key(|l| link_sort_key(l));
        assert_eq!(mixed[0], "https://github.com/acme/app/pull/2");
        // File refs sort by the same rule on their raw text.
        let mut refs = ["src/app.rs:12".to_string(), "src/app.rs:9".to_string()];
        refs.sort_by_key(|l| link_sort_key(l));
        assert_eq!(
            refs,
            ["src/app.rs:9".to_string(), "src/app.rs:12".to_string()]
        );
    }

    // MARKER - Tests for PR state and iconification (issues #115)

    #[test]
    fn parse_pr_url_valid_https() {
        let url = "https://github.com/acme/app/pull/42";
        assert_eq!(parse_pr_url(url), Some(("acme/app", 42)));
    }

    #[test]
    fn parse_pr_url_http_fallback() {
        let url = "http://github.com/org/repo/pull/7";
        assert_eq!(parse_pr_url(url), Some(("org/repo", 7)));
    }

    #[test]
    fn parse_pr_url_trailing_slash() {
        let url = "https://github.com/acme/app/pull/42/";
        assert_eq!(parse_pr_url(url), Some(("acme/app", 42)));
    }

    #[test]
    fn parse_pr_url_invalid_prefix() {
        let invalid_urls = [
            "github.com/acme/app/pull/42",           // missing https://
            "https://gitlab.com/acme/app/pull/42",   // wrong domain
            "https://github.com/acme/app/issues/42", // issues not pull
            "",
        ];

        for url in invalid_urls {
            assert_eq!(parse_pr_url(url), None, "{url} should be rejected");
        }
    }

    #[test]
    fn parse_pr_url_non_numeric() {
        let url = "https://github.com/acme/app/pull/abc";
        assert_eq!(parse_pr_url(url), None);
    }

    #[test]
    fn status_glyph_for_open_unreviewed() {
        assert_eq!(pr_status_glyph(PrState::OpenUnreviewed), "◴");
    }

    #[test]
    fn status_glyph_for_approved_with_count() {
        // The glyph itself is always the static checkmark; the count
        // rides in the label (leak-free split of the old 2-arg form).
        assert_eq!(pr_status_glyph(PrState::OpenApproved(3)), "✓");
        assert_eq!(pr_status_label(PrState::OpenApproved(3)), "✓3");
        assert_eq!(pr_status_label(PrState::OpenApproved(1)), "✓1");
        assert_eq!(pr_approval_count(PrState::OpenApproved(3)), Some(3));
        assert_eq!(pr_approval_count(PrState::OpenUnreviewed), None);
    }

    #[test]
    fn status_glyph_for_closed_merged_draft() {
        assert_eq!(pr_status_glyph(PrState::Closed), "✕");
        assert_eq!(pr_status_glyph(PrState::Merged), "✔");
        assert_eq!(pr_status_glyph(PrState::Draft), "⋯");
    }

    #[test]
    fn status_glyph_for_unknown_fallback() {
        assert_eq!(pr_status_glyph(PrState::Unknown), "–");
    }

    // Issue #115: `gh pr view --json` fixtures map to PrState + glyph.
    // Shapes verified against gh 2.92.0 (`gh pr view --help` JSON FIELDS
    // + live `gh pr view`/`gh pr list` probes on staap + cli/cli):
    // state is OPEN/CLOSED/MERGED, reviewDecision is "" (none),
    // REVIEW_REQUIRED, APPROVED, or CHANGES_REQUESTED.
    #[test]
    fn gh_json_maps_to_pr_states() {
        let open = br#"{"number":1,"state":"OPEN","isDraft":false,"reviewDecision":"REVIEW_REQUIRED","mergeStateStatus":"BLOCKED"}"#;
        assert_eq!(pr_state_from_json_bytes(open), PrState::OpenUnreviewed);
        assert_eq!(pr_status_glyph(PrState::OpenUnreviewed), "◴");

        let approved = br#"{"number":2,"state":"OPEN","isDraft":false,"reviewDecision":"APPROVED","mergeStateStatus":"CLEAN","reviews":[{"state":"APPROVED","author":{"login":"a"}},{"state":"APPROVED","author":{"login":"b"}},{"state":"COMMENTED","author":{"login":"c"}}]}"#;
        assert_eq!(pr_state_from_json_bytes(approved), PrState::OpenApproved(2));
        assert_eq!(pr_status_label(PrState::OpenApproved(2)), "✓2");

        // APPROVED with no reviews array still counts the decision itself.
        let approved_bare = br#"{"number":3,"state":"OPEN","isDraft":false,"reviewDecision":"APPROVED","mergeStateStatus":"CLEAN"}"#;
        assert_eq!(
            pr_state_from_json_bytes(approved_bare),
            PrState::OpenApproved(1)
        );
        assert_eq!(pr_status_label(PrState::OpenApproved(1)), "✓1");

        let changes = br#"{"number":4,"state":"OPEN","isDraft":false,"reviewDecision":"CHANGES_REQUESTED","mergeStateStatus":"DIRTY"}"#;
        assert_eq!(
            pr_state_from_json_bytes(changes),
            PrState::OpenChangesRequested
        );
        assert_eq!(pr_status_glyph(PrState::OpenChangesRequested), "⚠");

        let closed = br#"{"number":5,"state":"CLOSED","isDraft":false,"reviewDecision":"","mergeStateStatus":"UNKNOWN"}"#;
        assert_eq!(pr_state_from_json_bytes(closed), PrState::Closed);
        assert_eq!(pr_status_glyph(PrState::Closed), "✕");

        let merged = br#"{"number":110,"state":"MERGED","isDraft":false,"reviewDecision":"","mergeStateStatus":"UNKNOWN"}"#;
        assert_eq!(pr_state_from_json_bytes(merged), PrState::Merged);
        assert_eq!(pr_status_glyph(PrState::Merged), "✔");

        let draft = br#"{"number":6,"state":"OPEN","isDraft":true,"reviewDecision":"REVIEW_REQUIRED","mergeStateStatus":"BLOCKED"}"#;
        assert_eq!(pr_state_from_json_bytes(draft), PrState::Draft);
        assert_eq!(pr_status_glyph(PrState::Draft), "⋯");

        // Bad JSON never becomes a state: Unknown, never an error row.
        assert_eq!(pr_state_from_json_bytes(b"not json"), PrState::Unknown);
        assert_eq!(pr_state_from_json_bytes(b"{}"), PrState::Unknown);
        assert_eq!(pr_status_glyph(PrState::Unknown), "–");
        assert_eq!(pr_status_label(PrState::Unknown), "–");
    }

    // Issue #115: refresh at most every 5 min per URL — two resolves
    // inside the TTL cost one spawn-boundary invocation. The spawn
    // boundary is injected (fixture bytes, never a real subprocess).
    #[test]
    fn pr_state_caches_five_minutes() {
        use std::time::{Duration, Instant};

        let url = "https://github.com/acme/cachetest/pull/7";
        let body = br#"{"number":7,"state":"OPEN","isDraft":false,"reviewDecision":"","mergeStateStatus":"UNKNOWN"}"#.to_vec();
        let calls = std::cell::Cell::new(0u32);
        let fetch = |_repo: &str, _n: u64| {
            calls.set(calls.get() + 1);
            Some(body.clone())
        };
        let mut fetch = fetch;
        let t0 = Instant::now();
        assert_eq!(
            resolve_pr_state_with(url, t0, true, &mut fetch),
            PrState::OpenUnreviewed
        );
        assert_eq!(
            resolve_pr_state_with(url, t0 + Duration::from_secs(299), true, &mut fetch),
            PrState::OpenUnreviewed
        );
        assert_eq!(calls.get(), 1, "second resolve inside TTL must not spawn");
        assert_eq!(
            resolve_pr_state_with(url, t0 + Duration::from_secs(301), true, &mut fetch),
            PrState::OpenUnreviewed
        );
        assert_eq!(calls.get(), 2, "resolve past the TTL must spawn again");
    }

    // Issue #115: no `gh` on PATH (or a non-PR URL) → Unknown glyph,
    // and the spawn boundary is never attempted.
    #[test]
    fn unknown_when_gh_missing() {
        use std::time::Instant;

        let mut must_not_spawn = |_repo: &str, _n: u64| -> Option<Vec<u8>> {
            panic!("must not attempt a spawn when gh is absent")
        };
        let url = "https://github.com/acme/ghmissing/pull/9";
        assert_eq!(
            resolve_pr_state_with(url, Instant::now(), false, &mut must_not_spawn),
            PrState::Unknown
        );
        // The icon built from that state is the Unknown fallback glyph.
        let icon = PrStatusIcon {
            glyph: pr_status_glyph(PrState::Unknown),
            approval_count: None,
        };
        assert_eq!(icon.glyph, "–");
        // Non-PR URLs never spawn either, even with gh present.
        assert_eq!(
            resolve_pr_state_with(
                "https://github.com/acme/app/issues/5",
                Instant::now(),
                true,
                &mut must_not_spawn
            ),
            PrState::Unknown
        );
    }
}
