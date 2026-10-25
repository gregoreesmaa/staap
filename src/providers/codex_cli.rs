//! Codex CLI session discovery.
//!
//! Store assumption (from the Codex developer-commands reference: `codex
//! resume` continues an interactive session by id; sessions persist as
//! rollout files under `~/.codex/sessions/`, honoring `CODEX_HOME`).
//! Rollout logs are JSONL: one event object per line. Discovery leans on
//! stable shapes only — `payload.type` (`user` input text / `assistant`
//! message text) for transcript and titles, `payload.cwd` for the
//! project, and the `id`/`session_id` fields for the resume handle
//! (`codex resume <id>`).
//!
//! Message extraction lives in [`crate::transcript`] (`parse_codex_tail`,
//! `extract_codex_project`, `extract_codex_session_id`) so headless tests
//! pin it without touching the filesystem; this module only walks the
//! store, tails each log, and maps rows onto [`ChatSession`].
//!
//! If the store root does not exist or is unreadable, discovery returns an
//! empty list instead of failing, so the app still starts (documented
//! degraded mode). Status reuses the one `app`-owned classifier: the
//! transcript tail plays the screen role, the log mtime plays output
//! recency, and historic sessions never count as exited.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::{ChatSession, Status};
use crate::parsers::Parser;
use crate::providers::traits::Provider;
use crate::transcript::{
    derive_title, extract_codex_project, extract_codex_session_id, parse_codex_tail, MAX_TAIL_BYTES,
};

/// Discovers sessions from the on-disk Codex CLI store.
pub struct CodexCliProvider {
    store_root: PathBuf,
    parser: Box<dyn Parser>,
}

impl CodexCliProvider {
    pub fn new(store_root: PathBuf, parser: Box<dyn Parser>) -> Self {
        Self { store_root, parser }
    }

    /// Default store location honoring `CODEX_HOME`.
    pub fn default_store_root() -> PathBuf {
        if let Ok(home) = std::env::var("CODEX_HOME") {
            if !home.trim().is_empty() {
                return PathBuf::from(home).join("sessions");
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            if !home.trim().is_empty() {
                return PathBuf::from(home).join(".codex/sessions");
            }
        }
        if let Ok(profile) = std::env::var("USERPROFILE") {
            if !profile.trim().is_empty() {
                return PathBuf::from(profile).join(".codex/sessions");
            }
        }
        PathBuf::from(".codex/sessions")
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
    fn classify(&self, log: &std::path::Path, tail: &str) -> Status {
        let age = std::fs::metadata(log)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|mtime| SystemTime::now().duration_since(mtime).ok());
        crate::app::classify(tail, age, false)
    }

    /// Recursively collect rollout log files (`*.jsonl` / `*.json`) under
    /// the store root, skipping dot-directories.
    fn collect_logs(root: &std::path::Path) -> Vec<PathBuf> {
        let mut logs = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    if p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with('.'))
                    {
                        continue;
                    }
                    stack.push(p);
                } else if p.is_file() {
                    let is_log = p
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| e == "jsonl" || e == "json");
                    if is_log {
                        logs.push(p);
                    }
                }
            }
        }
        logs
    }
}

impl Provider for CodexCliProvider {
    fn discover_sessions(&self) -> Vec<ChatSession> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut out = Vec::new();
        for log in Self::collect_logs(&self.store_root) {
            let Some(file_id) = log.file_stem().and_then(|n| n.to_str()) else {
                continue;
            };
            let (tail, tail_cut) = Self::read_tail(&log, MAX_TAIL_BYTES);
            let parsed = self.parser.parse(&tail);
            let transcript = parse_codex_tail(&tail);
            let provider_session_id =
                extract_codex_session_id(&tail).unwrap_or_else(|| file_id.to_string());
            let id = provider_session_id.clone();
            // Single-line summary title: transcript first, then the
            // parser title, then the id (same chain as muse).
            let raw_title = if transcript.messages.is_empty() {
                parsed.title.unwrap_or_else(|| id.clone())
            } else {
                derive_title(&transcript.messages, &id)
            };
            let mtime = std::fs::metadata(&log)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(now);
            out.push(ChatSession {
                id: id.clone(),
                title: shorten(&raw_title),
                project: extract_codex_project(&tail)
                    .or(parsed.project)
                    .unwrap_or_else(|| "codex".to_string()),
                status: self.classify(&log, &tail),
                harness: crate::app::HARNESS_CODEX.to_string(),
                last_active: mtime,
                pr_links: parsed.pr_links,
                related_links: parsed.related_links,
                links_truncated: false,
                transcript: transcript.messages,
                transcript_truncated: transcript.truncated || tail_cut,
                provider_session_id: Some(provider_session_id),
                title_locked: true,
                pending_input: String::new(),
                cwd: None,
                remote: None,
            });
        }
        out
    }
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

    fn user_event(text: &str) -> String {
        serde_json::json!({
            "id": "evt-1",
            "timestamp": "2026-09-30T00:00:00Z",
            "payload": {"type": "user", "text": text, "cwd": "/tmp/work/blog"},
        })
        .to_string()
    }

    fn assistant_event(text: &str) -> String {
        serde_json::json!({
            "id": "evt-2",
            "timestamp": "2026-09-30T00:00:01Z",
            "session_id": "sess-codex-1",
            "payload": {"type": "assistant", "text": text},
        })
        .to_string()
    }

    fn seed(files: &[(&str, String)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "staap-codex-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for (rel, body) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
        }
        root
    }

    #[test]
    fn unreachable_store_yields_empty_list() {
        let p = CodexCliProvider::new(
            PathBuf::from("/nonexistent-codex-store-xyz"),
            Box::new(RegistryParser::default()),
        );
        assert!(p.discover_sessions().is_empty());
    }

    #[test]
    fn discovers_rollout_jsonl_layout() {
        let root = seed(&[(
            "2026/09/30/rollout-sess-codex-1.jsonl",
            format!(
                "{}\n{}\n",
                user_event("Draft the release notes"),
                assistant_event("Done, see https://github.com/acme/repo/pull/7"),
            ),
        )]);
        let p = CodexCliProvider::new(root.clone(), Box::new(RegistryParser::default()));
        let sessions = p.discover_sessions();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "sess-codex-1");
        assert_eq!(s.harness, crate::app::HARNESS_CODEX);
        assert_eq!(s.project, "blog");
        assert_eq!(s.title, "Draft the release notes");
        assert_eq!(s.transcript.len(), 2);
        assert_eq!(s.pr_links, vec!["https://github.com/acme/repo/pull/7"]);
        assert_eq!(s.provider_session_id.as_deref(), Some("sess-codex-1"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn falls_back_to_filename_without_session_id() {
        let root = seed(&[("plain.jsonl", "Fix login\n".to_string())]);
        let p = CodexCliProvider::new(root.clone(), Box::new(RegistryParser::default()));
        let sessions = p.discover_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "plain");
        assert_eq!(sessions[0].provider_session_id.as_deref(), Some("plain"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn skips_non_log_files_and_approval_marks_attention() {
        let root = seed(&[(
            "sess-attn.jsonl",
            format!("{}\n", user_event("waiting for your approval to proceed")),
        )]);
        std::fs::write(root.join("notes.txt"), "ignored").unwrap();
        let p = CodexCliProvider::new(root.clone(), Box::new(RegistryParser::default()));
        let sessions = p.discover_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, crate::app::Status::Attention);
        std::fs::remove_dir_all(&root).ok();
    }
}
