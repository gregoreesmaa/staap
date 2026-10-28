//! Claude Code CLI session discovery.
//!
//! Store assumption (from the Claude Code sessions docs: transcripts live
//! as JSONL under `~/.claude/projects/<project>/<session-id>.jsonl`, where
//! `<project>` is the working-directory path with non-alphanumeric
//! characters replaced by `-`; `CLAUDE_CONFIG_DIR` relocates the whole
//! store). Each line is a message/tool-use/metadata JSON object; the entry
//! format is internal and versioned, so discovery only leans on stable
//! shapes: `type` (`user`/`assistant`) + `message.content` text blocks for
//! transcript/titles, `cwd` for the project, and `sessionId` for the
//! resume handle (`claude --resume <id>`).
//!
//! Message extraction lives in [`crate::transcript`] (`parse_claude_tail`,
//! `extract_claude_project`, `extract_claude_session_id`) so headless
//! tests pin it without touching the filesystem; this module only walks
//! the store, tails each log, and maps rows onto [`ChatSession`].
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
    derive_title, extract_claude_project, extract_claude_session_id, parse_claude_tail,
    MAX_TAIL_BYTES,
};

/// Discovers sessions from the on-disk Claude Code CLI store.
pub struct ClaudeCliProvider {
    store_root: PathBuf,
    parser: Box<dyn Parser>,
}

impl ClaudeCliProvider {
    pub fn new(store_root: PathBuf, parser: Box<dyn Parser>) -> Self {
        Self { store_root, parser }
    }

    /// Default store location honoring `CLAUDE_CONFIG_DIR`.
    pub fn default_store_root() -> PathBuf {
        if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
            if !dir.trim().is_empty() {
                return PathBuf::from(dir).join("projects");
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            if !home.trim().is_empty() {
                return PathBuf::from(home).join(".claude/projects");
            }
        }
        if let Ok(profile) = std::env::var("USERPROFILE") {
            if !profile.trim().is_empty() {
                return PathBuf::from(profile).join(".claude/projects");
            }
        }
        PathBuf::from(".claude/projects")
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
}

impl Provider for ClaudeCliProvider {
    fn discover_sessions(&self) -> Vec<ChatSession> {
        let Ok(projects) = std::fs::read_dir(&self.store_root) else {
            // Unreachable store: degraded empty list (see module docs).
            return Vec::new();
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut out = Vec::new();
        for project in projects.flatten() {
            let project_dir = project.path();
            if !project_dir.is_dir() {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&project_dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let log = entry.path();
                if !log.is_file() {
                    continue;
                }
                if log.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(file_id) = log.file_stem().and_then(|n| n.to_str()) else {
                    continue;
                };
                let (tail, tail_cut) = Self::read_tail(&log, MAX_TAIL_BYTES);
                let parsed = self.parser.parse(&tail);
                let transcript = parse_claude_tail(&tail);
                let provider_session_id =
                    extract_claude_session_id(&tail).unwrap_or_else(|| file_id.to_string());
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
                    project: extract_claude_project(&tail)
                        .or(parsed.project)
                        .unwrap_or_else(|| "claude".to_string()),
                    status: self.classify(&log, &tail),
                    harness: crate::app::HARNESS_CLAUDE.to_string(),
                    last_active: mtime,
                    pr_links: parsed.pr_links,
                    pr_states: Default::default(),
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

    fn user_line(text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "sessionId": "sess-claude-1",
            "cwd": "/tmp/work/shop",
            "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        })
        .to_string()
    }

    fn assistant_line(text: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "sessionId": "sess-claude-1",
            "cwd": "/tmp/work/shop",
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
        })
        .to_string()
    }

    fn seed(files: &[(&str, String)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "staap-claude-{}",
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
        let p = ClaudeCliProvider::new(
            PathBuf::from("/nonexistent-claude-store-xyz"),
            Box::new(RegistryParser::default()),
        );
        assert!(p.discover_sessions().is_empty());
    }

    #[test]
    fn discovers_project_jsonl_layout() {
        let root = seed(&[(
            "tmp-work-shop/sess-claude-1.jsonl",
            format!(
                "{}\n{}\n",
                user_line("Fix checkout retries"),
                assistant_line("Done, see https://github.com/acme/repo/pull/42")
            ),
        )]);
        let p = ClaudeCliProvider::new(root.clone(), Box::new(RegistryParser::default()));
        let sessions = p.discover_sessions();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "sess-claude-1");
        assert_eq!(s.harness, crate::app::HARNESS_CLAUDE);
        assert_eq!(s.project, "shop");
        assert_eq!(s.title, "Fix checkout retries");
        assert_eq!(s.transcript.len(), 2);
        assert_eq!(s.pr_links, vec!["https://github.com/acme/repo/pull/42"]);
        assert_eq!(s.provider_session_id.as_deref(), Some("sess-claude-1"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn falls_back_to_filename_without_session_id() {
        let root = seed(&[("proj/plain.jsonl", "Fix login\n".to_string())]);
        let p = ClaudeCliProvider::new(root.clone(), Box::new(RegistryParser::default()));
        let sessions = p.discover_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "plain");
        assert_eq!(sessions[0].provider_session_id.as_deref(), Some("plain"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn skips_non_jsonl_files_and_approval_marks_attention() {
        let root = seed(&[(
            "proj/sess-attn.jsonl",
            format!("{}\n", user_line("waiting for your approval to proceed")),
        )]);
        std::fs::write(root.join("proj/notes.txt"), "ignored").unwrap();
        let p = ClaudeCliProvider::new(root.clone(), Box::new(RegistryParser::default()));
        let sessions = p.discover_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, crate::app::Status::Attention);
        std::fs::remove_dir_all(&root).ok();
    }
}
