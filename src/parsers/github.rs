//! Per-chat GitHub PR link extraction and status iconification via `gh` CLI.
//!
//! This module extends link extraction to include real-time PR state queries
//! using the GitHub CLI tool (`gh`). It supports graceful degradation when
//! `gh` is unavailable, cached state to avoid excessive API calls, and text-only
//! Unicode glyphs for display in sidebars.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;

use regex::Regex;

/// PR states extracted from `gh pr view` JSON response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Convert PrState to display glyph (text-only Unicode).
pub fn pr_status_glyph(state: PrState, approval_count: Option<u32>) -> &'static str {
    match state {
        PrState::OpenUnreviewed => "◴", // white circle left quarter shaded
        PrState::OpenApproved(_n) => {
            // Append count directly to checkmark: ✓4 means 4 approvals
            if let Some(count) = approval_count {
                Box::leak(format!("✓{}", count).into_boxed_str())
            } else {
                "✓"
            }
        }
        PrState::OpenChangesRequested => "⚠", // warning sign
        PrState::Closed => "✕",               // X mark
        PrState::Merged => "✔",               // heavy checkmark (→ also acceptable)
        PrState::Draft => "⋯",                // vertical ellipsis
        PrState::Unknown => "–",              // minus sign (fallback when gh unavailable)
    }
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

/// Query PR state via `gh pr view` and convert to icon.
/// Caches results with 5-minute TTL to avoid excessive API calls.
pub fn query_pr_state(_repo: &str, _pr_number: u64) -> PrState {
    // TODO: Implement gh CLI query with JSON parsing
    // This is a placeholder for the full implementation
    // Expected `gh pr view <number> --repo <owner/repo> --json state,number,isDraft,status`

    // For now, return Unknown - will implement in follow-up
    PrState::Unknown
}

/// Resolve status icon for a PR URL with caching.
pub fn resolve_pr_icon(url: &str) -> PrStatusIcon {
    if !gh_cli_available() {
        return PrStatusIcon {
            glyph: pr_status_glyph(PrState::Unknown, None),
            approval_count: None,
        };
    }

    match parse_pr_url(url) {
        Some((repo, number)) => {
            let state = query_pr_state(repo, number);
            PrStatusIcon {
                glyph: pr_status_glyph(state, None),
                approval_count: match state {
                    PrState::OpenApproved(count) => Some(count),
                    _ => None,
                },
            }
        }
        None => PrStatusIcon {
            glyph: pr_status_glyph(PrState::Unknown, None),
            approval_count: None,
        },
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
        assert_eq!(pr_status_glyph(PrState::OpenUnreviewed, None), "◴");
    }

    #[test]
    fn status_glyph_for_approved_with_count() {
        assert_eq!(pr_status_glyph(PrState::OpenApproved(3), Some(3)), "✓3");
        assert_eq!(pr_status_glyph(PrState::OpenApproved(1), Some(1)), "✓1");
    }

    #[test]
    fn status_glyph_for_closed_merged_draft() {
        assert_eq!(pr_status_glyph(PrState::Closed, None), "✕");
        assert_eq!(pr_status_glyph(PrState::Merged, None), "✔");
        assert_eq!(pr_status_glyph(PrState::Draft, None), "⋯");
    }

    #[test]
    fn status_glyph_for_unknown_fallback() {
        assert_eq!(pr_status_glyph(PrState::Unknown, None), "–");
    }
}
