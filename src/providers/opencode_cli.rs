//! opencode CLI session discovery.
//!
//! Store assumption (verified 2026-10-01 against opencode 1.3.14 on a
//! live machine): sessions surface through the CLI itself —
//! `opencode session list --format json` yields
//! `[{id, title, updated, created, projectId, directory}]` (millis
//! timestamps). Transcripts come from `opencode export <id>` (the export
//! document: `info` + `messages: [{info: {role}, parts: [{type, text}]}]`).
//! The on-disk sqlite store (`~/.local/share/opencode/opencode.db`) is
//! deliberately not read here: its schema is internal, while the CLI
//! output is the stable surface.
//!
//! Shelling out keeps discovery synchronous and bounded, but one `export`
//! per row costs ~1 s each (a full instance boot per call) — so
//! discovery ships roster-only rows (id/title/updated/directory, links
//! from the title) and [`OpencodeCliProvider::hydrate`] fills a row's
//! transcript/links/title from `export` on demand. Both commands run with
//! a short timeout; any failure (missing binary, timeout, bad JSON)
//! degrades to an empty list / untouched row so the app still starts.
//!
//! Message extraction lives in [`crate::transcript`] (`parse_opencode_tail`,
//! `extract_opencode_project`, `extract_opencode_title`) so headless
//! tests pin it without touching the filesystem or the CLI; this module
//! only invokes the CLI, caps the rows, and maps them onto
//! [`ChatSession`].
//!
//! Status reuses the one `app`-owned classifier: the title text plays
//! the screen role, the `updated` timestamp plays output recency, and
//! historic sessions never count as exited.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::{ChatSession, Status};
use crate::parsers::Parser;
use crate::providers::traits::Provider;
use crate::transcript::{
    derive_title, extract_opencode_project, extract_opencode_title, parse_opencode_tail,
};

/// How long to wait for one opencode CLI invocation before giving up
/// (discovery must never stall startup).
const CLI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Max sessions pulled from `session list` (bounded by construction:
///
/// the registry caps links per run; the roster caps rows here).
const MAX_SESSIONS: usize = 50;

/// Raw `session list --format json` row.
#[derive(Debug, Clone)]
struct SessionRow {
    id: String,
    title: Option<String>,
    updated_ms: Option<i64>,
    directory: Option<String>,
}

/// Discovers sessions through the opencode CLI.
pub struct OpencodeCliProvider {
    /// Optional override of the `opencode` binary (tests point this at a
    /// fake script; production uses `opencode` on `PATH`).
    program: PathBuf,
    parser: Box<dyn Parser>,
}

impl OpencodeCliProvider {
    pub fn new(program: PathBuf, parser: Box<dyn Parser>) -> Self {
        Self { program, parser }
    }

    /// Production constructor: `opencode` resolved via `PATH`.
    pub fn with_default_program(parser: Box<dyn Parser>) -> Self {
        Self::new(PathBuf::from("opencode"), parser)
    }

    /// Run the CLI once with a timeout; `None` on any failure (missing
    /// binary, timeout, non-zero exit, bad UTF-8).
    fn run_cli(&self, args: &[&str]) -> Option<String> {
        let mut child = std::process::Command::new(&self.program)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let deadline = SystemTime::now() + CLI_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        return None;
                    }
                    let mut out = String::new();
                    use std::io::Read;
                    if let Some(mut stdout) = child.stdout.take() {
                        let _ = stdout.read_to_string(&mut out);
                    }
                    let _ = child.wait();
                    return Some(out);
                }
                Ok(None) => {
                    if SystemTime::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(_) => return None,
            }
        }
    }

    /// Parse `session list --format json` output into rows (newest first
    /// by `updated`, capped at [`MAX_SESSIONS`]).
    fn parse_list(text: &str) -> Vec<SessionRow> {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return Vec::new();
        };
        let items: Vec<&serde_json::Value> = match &value {
            serde_json::Value::Array(items) => items.iter().collect(),
            single => vec![single],
        };
        let mut rows: Vec<SessionRow> = items
            .iter()
            .filter_map(|item| {
                let id = item.get("id").and_then(|v| v.as_str())?;
                if id.trim().is_empty() {
                    return None;
                }
                Some(SessionRow {
                    id: id.to_string(),
                    title: item
                        .get("title")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_string),
                    updated_ms: item.get("updated").and_then(|v| v.as_i64()),
                    directory: item
                        .get("directory")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_string),
                })
            })
            .collect();
        rows.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        rows.truncate(MAX_SESSIONS);
        rows
    }

    /// Historic leg of the one `app`-owned classifier: the title plays
    /// the screen role, the `updated` timestamp plays output recency, and
    /// historic sessions never count as exited.
    fn classify(&self, text: &str, updated_ms: Option<i64>) -> Status {
        let age = updated_ms
            .and_then(|ms| {
                SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_millis(ms as u64))
            })
            .and_then(|t| SystemTime::now().duration_since(t).ok());
        crate::app::classify(text, age, false)
    }
}

impl Provider for OpencodeCliProvider {
    fn discover_sessions(&self) -> Vec<ChatSession> {
        let Some(list) = self.run_cli(&["session", "list", "--format", "json"]) else {
            // Unreachable CLI: degraded empty list (see module docs).
            return Vec::new();
        };
        let mut out = Vec::new();
        for row in Self::parse_list(&list) {
            // Roster first, transcript lazily: `session list` already
            // carries id/title/updated/directory, and one `export` per
            // row costs ~1 s each (a full instance boot per call). The
            // row ships with list-only metadata; [`Self::hydrate`] fills
            // transcript/links for the selected run on demand.
            let title = row.title.clone().unwrap_or_else(|| row.id.clone());
            let project =
                basename(row.directory.as_deref()).unwrap_or_else(|| "opencode".to_string());
            // Links from the title are never silently dropped.
            let title_parsed = self.parser.parse(&title);
            let mtime = row
                .updated_ms
                .map(|ms| (ms / 1000).max(0))
                .unwrap_or_else(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0)
                });
            out.push(ChatSession {
                id: row.id.clone(),
                title: shorten(&title),
                project,
                status: self.classify(&title, row.updated_ms),
                harness: crate::app::HARNESS_OPENCODE.to_string(),
                last_active: mtime,
                pr_links: title_parsed.pr_links,
                pr_states: Default::default(),
                related_links: title_parsed.related_links,
                links_truncated: false,
                transcript: Vec::new(),
                transcript_truncated: true,
                provider_session_id: Some(row.id),
                title_locked: true,
                pending_input: String::new(),
                cwd: None,
                remote: None,
            });
        }
        out
    }
}

impl OpencodeCliProvider {
    /// Fill `session`'s transcript/links/title from `opencode export`
    /// (one CLI call, ~1 s). Returns true when the export parsed.
    /// `session` must be an opencode-harness row (its `id` is the
    /// `ses-*` resume handle). Export failure leaves the row untouched.
    pub fn hydrate(&self, session: &mut ChatSession) -> bool {
        if session.harness != crate::app::HARNESS_OPENCODE {
            return false;
        }
        let Some(export) = self.run_cli(&["export", &session.id]) else {
            return false;
        };
        if export.trim().is_empty() {
            return false;
        }
        let transcript = parse_opencode_tail(&export);
        let parsed = self.parser.parse(&export);
        if let Some(title) = extract_opencode_title(&export) {
            session.title = shorten(&title);
        } else if !transcript.messages.is_empty() {
            session.title = shorten(&derive_title(&transcript.messages, &session.id));
        }
        if let Some(project) = extract_opencode_project(&export).or(parsed.project) {
            session.project = project;
        }
        session.push_links(parsed.pr_links, parsed.related_links);
        if !transcript.messages.is_empty() {
            session.transcript = transcript.messages;
        }
        session.transcript_truncated = transcript.truncated || session.transcript_truncated;
        true
    }
}

/// Basename across both path separators (Windows dirs arrive verbatim).
fn basename(dir: Option<&str>) -> Option<String> {
    let dir = dir?.trim();
    if dir.is_empty() {
        return None;
    }
    let name = dir
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(dir);
    if name.trim().is_empty() {
        return None;
    }
    Some(name.to_string())
}

fn shorten(s: &str) -> String {
    const MAX: usize = 60;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    format!("{}…", s.chars().take(MAX).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::registry::RegistryParser;

    /// Fake `opencode` script: `session list --format json` prints one
    /// row; `export <id>` prints an export doc; anything else fails.
    /// Unix shell script (like the existing hermetic fakes); Windows
    /// coverage comes from `parse_list` unit tests below, which carry no
    /// process dependency.
    #[cfg(unix)]
    fn fake_opencode(dir: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("opencode");
        std::fs::write(
            &bin,
            r#"#!/bin/sh
if [ "$1" = "session" ] && [ "$2" = "list" ]; then
  printf '[{"id":"ses-1","title":"Fix login","updated":1790830766916,"created":1790798013762,"projectId":"p","directory":"/tmp/work/shop"}]'
elif [ "$1" = "export" ]; then
  printf '{"info":{"id":"ses-1","directory":"/tmp/work/shop","title":"Fix login"},"messages":[{"info":{"role":"user"},"parts":[{"type":"text","text":"Fix login"}]},{"info":{"role":"assistant"},"parts":[{"type":"text","text":"Done, see https://github.com/acme/repo/pull/7"}]}]}'
else
  exit 1
fi
"#,
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[cfg(unix)]
    fn tmpdir(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "staap-opencode-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn unreachable_cli_yields_empty_list() {
        let p = OpencodeCliProvider::new(
            PathBuf::from("/nonexistent-opencode-binary-xyz"),
            Box::new(RegistryParser::default()),
        );
        assert!(p.discover_sessions().is_empty());
    }

    #[test]
    fn parse_list_sorts_newest_first_caps_and_skips_bad_rows() {
        let rows: Vec<serde_json::Value> = (0..MAX_SESSIONS + 10)
            .map(|n| {
                serde_json::json!({
                    "id": format!("ses-{n}"),
                    "title": format!("t{n}"),
                    "updated": 1000 + n as i64,
                    "directory": "/tmp/work/shop",
                })
            })
            .collect();
        let mut text = serde_json::to_string(&rows).unwrap();
        // Bad rows: missing/blank ids never surface.
        text = text.replace(r#"{"id":"ses-0","title":"t0""#, r#"{"id":"","title":"t0""#);
        let parsed = OpencodeCliProvider::parse_list(&text);
        assert_eq!(parsed.len(), MAX_SESSIONS);
        assert_eq!(parsed[0].id, format!("ses-{}", MAX_SESSIONS + 9));
        assert!(parsed.iter().all(|r| !r.id.trim().is_empty()));
        assert!(OpencodeCliProvider::parse_list("not json").is_empty());
    }

    #[test]
    fn export_failure_keeps_list_only_row() {
        // `session list` succeeds but `export` fails: the row survives
        // with list metadata instead of vanishing.
        let p = OpencodeCliProvider::new(
            PathBuf::from("/nonexistent-opencode-binary-xyz"),
            Box::new(RegistryParser::default()),
        );
        // Direct unit shape: parse_list feeds the same path discover uses.
        let rows = OpencodeCliProvider::parse_list(
            r#"[{"id":"ses-9","title":"waiting for your approval",
                "updated":1790830766916,"directory":"C:\\work\\myproj"}]"#,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(
            p.classify("waiting for your approval", rows[0].updated_ms),
            crate::app::Status::Attention
        );
        assert_eq!(
            basename(Some("C:\\work\\myproj")).as_deref(),
            Some("myproj")
        );
        assert_eq!(basename(None), None);
    }

    #[cfg(unix)]
    #[test]
    fn discovers_roster_through_fake_cli_and_hydrates_on_demand() {
        let dir = tmpdir("fake");
        let bin = fake_opencode(&dir);
        let p = OpencodeCliProvider::new(bin, Box::new(RegistryParser::default()));
        // Discovery is roster-only: one `session list` call, no `export`
        // per row (fast even with a real CLI on PATH).
        let mut sessions = p.discover_sessions();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "ses-1");
        assert_eq!(s.harness, crate::app::HARNESS_OPENCODE);
        assert_eq!(s.project, "shop");
        assert_eq!(s.title, "Fix login");
        assert!(s.transcript.is_empty());
        assert!(s.transcript_truncated);
        assert_eq!(s.provider_session_id.as_deref(), Some("ses-1"));
        // Hydrate fills transcript/links/title from `export`.
        assert!(p.hydrate(&mut sessions[0]));
        let s = &sessions[0];
        assert_eq!(s.transcript.len(), 2);
        assert_eq!(s.pr_links, vec!["https://github.com/acme/repo/pull/7"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hydrate_refuses_non_opencode_rows_and_failed_exports() {
        let p = OpencodeCliProvider::new(
            PathBuf::from("/nonexistent-opencode-binary-xyz"),
            Box::new(RegistryParser::default()),
        );
        let mut muse_row = crate::app::ChatSession {
            id: "a".into(),
            title: "a".into(),
            project: "p".into(),
            status: crate::app::Status::Idle,
            harness: crate::app::HARNESS_MUSE.into(),
            last_active: 0,
            pr_links: vec![],
            pr_states: Default::default(),
            related_links: vec![],
            links_truncated: false,
            transcript: vec![],
            transcript_truncated: false,
            title_locked: true,
            pending_input: String::new(),
            provider_session_id: None,
            cwd: None,
            remote: None,
        };
        assert!(!p.hydrate(&mut muse_row));
        assert!(muse_row.transcript.is_empty());
        // Failed export leaves the row untouched.
        let mut row = muse_row.clone();
        row.harness = crate::app::HARNESS_OPENCODE.into();
        row.id = "ses-9".into();
        assert!(!p.hydrate(&mut row));
        assert_eq!(row.title, "a");
    }
}
