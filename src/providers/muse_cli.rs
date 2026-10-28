//! Muse CLI session discovery.
//!
//! Store assumption (verified 2026-09-26 against a live machine):
//! sessions live under `${XDG_DATA_HOME:-$HOME/.local/share}/muse/sessions/`
//! as `YYYY/MM/DD/<session-id>/session.jsonl` (plus `cli-*.log` and
//! `session.peer-history.sqlite3` siblings). A `.msp-view-v1/` cache holds
//! binary snapshots; it is not parsed here.
//!
//! If the store root does not exist or is unreadable, discovery returns an
//! empty list instead of failing, so the TUI still starts (documented
//! degraded mode). Status heuristics: a session whose `session.jsonl` was
//! modified within the last 60 s counts as [`Status::Working`]; parse or
//! approval markers flip it to [`Status::Attention`]; otherwise
//! [`Status::Idle`].

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::{ChatSession, Status};
use crate::parsers::Parser;
use crate::providers::traits::Provider;
use crate::transcript::{
    derive_title, extract_project, extract_session_name, parse_transcript, MAX_TAIL_BYTES,
};

/// Discovers sessions from the on-disk Muse CLI store.
pub struct MuseCliProvider {
    store_root: PathBuf,
    parser: Box<dyn Parser>,
}

impl MuseCliProvider {
    pub fn new(store_root: PathBuf, parser: Box<dyn Parser>) -> Self {
        Self { store_root, parser }
    }

    /// Default store location honoring `XDG_DATA_HOME`.
    pub fn default_store_root() -> PathBuf {
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            PathBuf::from(xdg).join("muse/sessions")
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".local/share/muse/sessions")
        } else {
            PathBuf::from(".local/share/muse/sessions")
        }
    }

    /// Read up to `max_bytes` from the end of the log. Returns the tail plus
    /// whether the head was cut (in which case the first, partial line is
    /// dropped so only whole records are parsed).
    fn read_tail(path: &std::path::Path, max_bytes: u64) -> (String, bool) {
        let Ok(meta) = std::fs::metadata(path) else {
            return (String::new(), false);
        };
        let was_cut = meta.len() > max_bytes;
        let Ok(mut f) = std::fs::File::open(path) else {
            return (String::new(), false);
        };
        use std::io::{Read, Seek, SeekFrom};
        let start = meta.len().saturating_sub(max_bytes);
        let mut buf = String::new();
        if f.seek(SeekFrom::Start(start)).is_ok() {
            let _ = f.read_to_string(&mut buf);
        }
        if was_cut {
            if let Some(i) = buf.find('\n') {
                buf = buf[i + 1..].to_string();
            } else {
                buf.clear();
            }
        }
        (buf, was_cut)
    }

    /// Historic leg of the one `app`-owned classifier: the transcript
    /// tail plays the screen role, the session log mtime plays output
    /// recency, and historic sessions never count as exited.
    fn classify(&self, dir: &std::path::Path, tail: &str) -> Status {
        let log = dir.join("session.jsonl");
        let age = std::fs::metadata(&log)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|mtime| SystemTime::now().duration_since(mtime).ok());
        crate::app::classify(tail, age, false)
    }
}

impl Provider for MuseCliProvider {
    fn discover_sessions(&self) -> Vec<ChatSession> {
        let Ok(date_dirs) = collect_date_dirs(&self.store_root) else {
            // Unreachable store: degraded empty list (see module docs).
            return Vec::new();
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut out = Vec::new();
        for date_dir in date_dirs {
            let Ok(entries) = std::fs::read_dir(&date_dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let dir = entry.path();
                if !dir.is_dir() {
                    continue;
                }
                let log = dir.join("session.jsonl");
                if !log.is_file() {
                    continue;
                }
                let Some(id) = dir.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let (tail, tail_cut) = Self::read_tail(&log, MAX_TAIL_BYTES);
                let parsed = self.parser.parse(&tail);
                let transcript = parse_transcript(&tail);
                let auto_name = extract_session_name(&tail);
                // Single-line summary title: transcript first, then the
                // session auto-name / parser title, then the id.
                let raw_title = if transcript.messages.is_empty() {
                    auto_name.or(parsed.title).unwrap_or_else(|| id.to_string())
                } else {
                    derive_title(&transcript.messages, auto_name.as_deref().unwrap_or(id))
                };
                let mtime = std::fs::metadata(&log)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(now);
                out.push(ChatSession {
                    id: id.to_string(),
                    title: shorten(&raw_title),
                    project: extract_project(&tail)
                        .or(parsed.project)
                        .unwrap_or_else(|| "muse".to_string()),
                    status: self.classify(&dir, &tail),
                    harness: crate::app::HARNESS_MUSE.to_string(),
                    last_active: mtime,
                    pr_links: parsed.pr_links,
                    pr_states: Default::default(),
                    related_links: parsed.related_links,
                    links_truncated: false,
                    transcript: transcript.messages,
                    transcript_truncated: transcript.truncated || tail_cut,
                    provider_session_id: Some(id.to_string()),
                    title_locked: true,
                    pending_input: String::new(),
                    cwd: None,
                    remote: None,
                });
            }
        }
        out
    }
}

/// Recursively collect `YYYY/MM/DD` leaf dirs under the store root.
/// Depth-first walk without external deps; skips dot-directories
/// (e.g. `.msp-view-v1`, whose layout differs from dated sessions).
fn collect_date_dirs(root: &std::path::Path) -> std::io::Result<Vec<PathBuf>> {
    let mut leaves = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0u8)];
    while let Some((dir, depth)) = stack.pop() {
        if depth == 3 {
            leaves.push(dir);
            continue;
        }
        let entries = std::fs::read_dir(&dir)?;
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            if p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
            {
                continue;
            }
            stack.push((p, depth + 1));
        }
    }
    Ok(leaves)
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

    #[test]
    fn historic_classify_agrees_with_live_classifier() {
        // Same reconciled markers as the live shell: an approval marker
        // in the tail means Attention even though the log mtime is fresh
        // (which alone would read Working).
        let root = std::env::temp_dir().join(format!(
            "staap-classify-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let leaf = root.join("2026/09/26/attention-session");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(
            leaf.join("session.jsonl"),
            "waiting for your approval to proceed",
        )
        .unwrap();
        let p = MuseCliProvider::new(root.clone(), Box::new(RegistryParser::default()));
        let sessions = p.discover_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, crate::app::Status::Attention);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unreachable_store_yields_empty_list() {
        let p = MuseCliProvider::new(
            PathBuf::from("/nonexistent-store-xyz"),
            Box::new(RegistryParser::default()),
        );
        let sessions = p.discover_sessions();
        assert!(sessions.is_empty());
    }

    #[test]
    fn discovers_session_jsonl_layout() {
        let root = std::env::temp_dir().join(format!(
            "staap-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let leaf = root.join("2026/09/26/test-session-1");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(
            leaf.join("session.jsonl"),
            "{\"payload\":\"see https://github.com/acme/repo/pull/42 for details\"}",
        )
        .unwrap();
        // Backdate mtime by touching content then setting readonly? mtime is
        // now => Working or Attention; accept either non-Idle... instead just
        // assert discovery finds it.
        let p = MuseCliProvider::new(root.clone(), Box::new(RegistryParser::default()));
        let sessions = p.discover_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "test-session-1");
        assert_eq!(
            sessions[0].pr_links,
            vec!["https://github.com/acme/repo/pull/42"]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn populates_transcript_and_single_line_title() {
        let root = std::env::temp_dir().join(format!(
            "staap-transcript-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let leaf = root.join("2026/09/26/transcript-session");
        std::fs::create_dir_all(&leaf).unwrap();
        let log = leaf.join("session.jsonl");
        let lines = [
            serde_json::json!({
                "payload_type": "runtime.session.metadata",
                "payload": {"kind": "metadata", "record": {"workspace_root": "/tmp/work/myproj"}}
            })
            .to_string(),
            serde_json::json!({
                "payload_type": "runtime.user_intent.accepted",
                "payload": {"model_messages": [
                    {"content": [{"kind": "text", "text": "Fix login\nsecond line"}]}
                ]}
            })
            .to_string(),
            serde_json::json!({
                "payload_type": "runtime.session",
                "payload": {"kind": "run", "run_id": "r1", "event": {
                    "kind": "assistant_message_committed",
                    "message_id": "m", "response_id": "x",
                    "text": "Done, see https://github.com/acme/repo/pull/7"
                }}
            })
            .to_string(),
        ];
        std::fs::write(&log, lines.join("\n")).unwrap();
        let p = MuseCliProvider::new(root.clone(), Box::new(RegistryParser::default()));
        let sessions = p.discover_sessions();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.project, "myproj");
        assert!(!s.title.contains('\n'), "title must be single-line");
        assert_eq!(s.title, "Fix login second line");
        assert_eq!(s.transcript.len(), 2);
        assert_eq!(
            s.transcript[0].text, "Fix login\nsecond line",
            "transcript keeps full text"
        );
        assert_eq!(s.pr_links, vec!["https://github.com/acme/repo/pull/7"]);
        std::fs::remove_dir_all(&root).ok();
    }
}
