//! Antigravity CLI (`agy`) session discovery.
//!
//! Store assumption (verified 2026-09-30 against a live machine):
//! history lives as JSONL at
//! `~/.gemini/antigravity-cli/history.jsonl` — one object per prompt
//! with `display` (the prompt text), `workspace` (the project dir),
//! `timestamp` (millis), and `conversationId` (the resume handle,
//! `agy --conversation <id>`); slash-commands (`type: slash_command`)
//! are activity, not prompts, and never title a row. Per-conversation
//! transcripts live at
//! `brain/<conversation-id>/.system_generated/logs/transcript*.jsonl`
//! (parsed by [`crate::transcript::parse_antigravity_tail`]); the
//! history workspace names the project when no transcript exists.
//! `~/.gemini/antigravity/` (IDE surface) is out of scope: this provider
//! covers the CLI store only.
//!
//! Message extraction lives in [`crate::transcript`] so headless tests
//! pin it without touching the filesystem; this module only reads the
//! history + transcript logs and maps rows onto [`ChatSession`].
//!
//! If the history file does not exist or is unreadable, discovery returns
//! an empty list instead of failing, so the app still starts (documented
//! degraded mode). Status reuses the one `app`-owned classifier: the
//! display/title text plays the screen role, the history timestamp plays
//! output recency, and historic sessions never count as exited.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app::{ChatSession, Status};
use crate::parsers::Parser;
use crate::providers::traits::Provider;
use crate::transcript::{derive_title, parse_antigravity_tail, MAX_TAIL_BYTES};

/// One prompt row from `history.jsonl`.
struct HistoryRow {
    display: String,
    workspace: Option<String>,
    timestamp_ms: Option<i64>,
}

/// Discovers sessions from the on-disk Antigravity CLI store.
pub struct AntigravityCliProvider {
    /// Directory holding `history.jsonl` (default
    /// `~/.gemini/antigravity-cli`).
    store_root: PathBuf,
    parser: Box<dyn Parser>,
}

impl AntigravityCliProvider {
    pub fn new(store_root: PathBuf, parser: Box<dyn Parser>) -> Self {
        Self { store_root, parser }
    }

    /// Default store location honoring `HOME` / `USERPROFILE` (Windows,
    /// where `HOME` is usually unset).
    pub fn default_store_root() -> PathBuf {
        if let Ok(home) = std::env::var("HOME") {
            if !home.trim().is_empty() {
                return PathBuf::from(home).join(".gemini/antigravity-cli");
            }
        }
        if let Ok(profile) = std::env::var("USERPROFILE") {
            if !profile.trim().is_empty() {
                return PathBuf::from(profile).join(".gemini/antigravity-cli");
            }
        }
        PathBuf::from(".gemini/antigravity-cli")
    }

    fn history_path(&self) -> PathBuf {
        self.store_root.join("history.jsonl")
    }

    /// Per-conversation transcript candidates, richest first.
    fn transcript_paths(&self, conversation_id: &str) -> [PathBuf; 2] {
        let base = self
            .store_root
            .join("brain")
            .join(conversation_id)
            .join(".system_generated/logs");
        [
            base.join("transcript_full.jsonl"),
            base.join("transcript.jsonl"),
        ]
    }

    /// Read up to `max_bytes` from the end of the log. Returns the tail plus
    /// whether the head was cut (in which case the first, partial line is
    /// dropped so only whole records are parsed).
    fn read_tail(path: &Path, max_bytes: u64) -> (String, bool) {
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

    /// Historic leg of the one `app`-owned classifier: the display/title
    /// text plays the screen role, the history timestamp plays output
    /// recency, and historic sessions never count as exited.
    fn classify(&self, text: &str, timestamp_ms: Option<i64>) -> Status {
        let age = timestamp_ms
            .and_then(|ms| {
                SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_millis(ms as u64))
            })
            .and_then(|t| SystemTime::now().duration_since(t).ok());
        crate::app::classify(text, age, false)
    }
}

impl Provider for AntigravityCliProvider {
    fn discover_sessions(&self) -> Vec<ChatSession> {
        let Ok(history) = std::fs::read_to_string(self.history_path()) else {
            // Unreachable store: degraded empty list (see module docs).
            return Vec::new();
        };
        // One row per conversation: earliest prompt titles the row (the
        // conversation's first intent), the freshest timestamp/activity
        // wins for status, workspaces union toward the latest non-empty.
        #[derive(Default)]
        struct Agg {
            first_display: Option<String>,
            workspace: Option<String>,
            latest_ms: Option<i64>,
            seen_displays: Vec<String>,
        }
        let mut convos: HashMap<String, Agg> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for line in history.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            // Slash-commands are activity, not prompts: never title a row.
            if value.get("type").and_then(|v| v.as_str()) == Some("slash_command") {
                continue;
            }
            let display = value
                .get("display")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if display.is_empty() {
                continue;
            }
            // Rows without a conversation id are stray one-shots (e.g. a
            // bare `exit` line): not resumable, not discoverable.
            let Some(convo) = value
                .get("conversationId")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
            else {
                continue;
            };
            let row = HistoryRow {
                display,
                workspace: value
                    .get("workspace")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string),
                timestamp_ms: value.get("timestamp").and_then(|v| v.as_i64()),
            };
            let agg = convos.entry(convo.to_string()).or_default();
            if !order.contains(&convo.to_string()) {
                order.push(convo.to_string());
            }
            if agg.first_display.is_none() {
                agg.first_display = Some(row.display.clone());
            }
            if !agg.seen_displays.contains(&row.display) {
                agg.seen_displays.push(row.display);
            }
            if let Some(ws) = row.workspace {
                agg.workspace = Some(ws);
            }
            match (agg.latest_ms, row.timestamp_ms) {
                (_, None) => {}
                (None, Some(ms)) => agg.latest_ms = Some(ms),
                (Some(cur), Some(ms)) if ms > cur => agg.latest_ms = Some(ms),
                _ => {}
            }
        }
        let mut out = Vec::new();
        for convo in order {
            let Some(agg) = convos.remove(&convo) else {
                continue;
            };
            // Richest transcript available: full, then compact, else the
            // history prompts alone.
            let mut tail = String::new();
            let mut tail_cut = false;
            for path in self.transcript_paths(&convo) {
                let (t, cut) = Self::read_tail(&path, MAX_TAIL_BYTES);
                if !t.trim().is_empty() {
                    tail = t;
                    tail_cut = cut;
                    break;
                }
            }
            let transcript = parse_antigravity_tail(&tail);
            let parsed = self.parser.parse(&tail);
            let history_text = agg.seen_displays.join("\n");
            let raw_title = if transcript.messages.is_empty() {
                agg.first_display
                    .clone()
                    .or(parsed.title)
                    .unwrap_or_else(|| convo.clone())
            } else {
                derive_title(&transcript.messages, &convo)
            };
            let project = extract_antigravity_project(&tail, agg.workspace.as_deref())
                .or(parsed.project)
                .unwrap_or_else(|| "antigravity".to_string());
            let screen_text = if tail.trim().is_empty() {
                history_text.clone()
            } else {
                tail.clone()
            };
            let mtime = agg
                .latest_ms
                .map(|ms| (ms / 1000).max(0))
                .unwrap_or_else(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0)
                });
            let mut pr_links = parsed.pr_links;
            let mut related_links = parsed.related_links;
            // History-only conversations (no transcript file): links still
            // accumulate from the prompt text itself.
            if tail.trim().is_empty() {
                let hist_parsed = self.parser.parse(&history_text);
                for link in hist_parsed.pr_links {
                    if !pr_links.contains(&link) {
                        pr_links.push(link);
                    }
                }
                for link in hist_parsed.related_links {
                    if !related_links.contains(&link) {
                        related_links.push(link);
                    }
                }
            }
            out.push(ChatSession {
                id: convo.clone(),
                title: shorten(&raw_title),
                project,
                status: self.classify(&screen_text, agg.latest_ms),
                harness: crate::app::HARNESS_ANTIGRAVITY.to_string(),
                last_active: mtime,
                pr_links,
                pr_states: Default::default(),
                related_links,
                links_truncated: false,
                transcript: transcript.messages,
                transcript_truncated: transcript.truncated || tail_cut,
                provider_session_id: Some(convo),
                title_locked: true,
                pending_input: String::new(),
                cwd: None,
                remote: None,
            });
        }
        out
    }
}

/// Project name: `brain/` transcripts carry no `cwd`, so the history
/// workspace (a real project dir) names it — basename across both path
/// separators, falling back to fewer components for short paths.
fn extract_antigravity_project(_tail: &str, workspace: Option<&str>) -> Option<String> {
    let ws = workspace?.trim();
    if ws.is_empty() {
        return None;
    }
    let name = ws
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(ws);
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

    fn history_line(display: &str, workspace: &str, convo: &str, ms: i64) -> String {
        serde_json::json!({
            "display": display,
            "timestamp": ms,
            "workspace": workspace,
            "conversationId": convo,
        })
        .to_string()
    }

    fn seed(files: &[(&str, String)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "staap-antigravity-{}",
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
        let p = AntigravityCliProvider::new(
            PathBuf::from("/nonexistent-antigravity-store-xyz"),
            Box::new(RegistryParser::default()),
        );
        assert!(p.discover_sessions().is_empty());
    }

    #[test]
    fn discovers_history_grouped_by_conversation() {
        let root = seed(&[(
            "history.jsonl",
            [
                history_line(
                    "Video root should be ingest. Make the edit!",
                    "C:\\Users\\grego\\projects\\video-clipper",
                    "convo-1",
                    1781428364071,
                ),
                history_line(
                    "Install me docker",
                    "C:\\Users\\grego\\projects\\video-clipper",
                    "convo-1",
                    1781428439383,
                ),
                history_line("Fix login", "/tmp/work/shop", "convo-2", 1781428000000),
            ]
            .join("\n"),
        )]);
        let p = AntigravityCliProvider::new(root.clone(), Box::new(RegistryParser::default()));
        let mut sessions = p.discover_sessions();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(sessions.len(), 2);
        // Earliest prompt titles; workspace basenames project (both
        // separators); harness + resume handle set.
        assert_eq!(sessions[0].id, "convo-1");
        assert_eq!(
            sessions[0].title,
            "Video root should be ingest. Make the edit!"
        );
        assert_eq!(sessions[0].project, "video-clipper");
        assert_eq!(sessions[0].harness, crate::app::HARNESS_ANTIGRAVITY);
        assert_eq!(sessions[0].provider_session_id.as_deref(), Some("convo-1"));
        assert_eq!(sessions[1].project, "shop");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn transcript_enriches_history_row() {
        let transcript = serde_json::json!({
            "step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT",
            "status": "DONE",
            "content": "<USER_REQUEST>\nFix checkout retries\n</USER_REQUEST>"
        })
        .to_string();
        let root = seed(&[
            (
                "history.jsonl",
                history_line(
                    "Fix checkout retries",
                    "/tmp/work/shop",
                    "convo-9",
                    1781428000000,
                ),
            ),
            (
                "brain/convo-9/.system_generated/logs/transcript.jsonl",
                transcript,
            ),
        ]);
        let p = AntigravityCliProvider::new(root.clone(), Box::new(RegistryParser::default()));
        let sessions = p.discover_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "Fix checkout retries");
        assert_eq!(sessions[0].transcript.len(), 1);
        assert_eq!(
            sessions[0].transcript[0].role,
            crate::transcript::Role::User
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn skips_slash_commands_stray_rows_and_marks_attention() {
        let root = seed(&[(
            "history.jsonl",
            [
                serde_json::json!({
                    "display": "/permissions", "timestamp": 1781428729809i64,
                    "workspace": "/tmp/work/shop",
                    "conversationId": "convo-s",
                    "type": "slash_command"
                })
                .to_string(),
                serde_json::json!({"display": "exit", "timestamp": 1781427996885i64}).to_string(),
                history_line(
                    "waiting for your approval to proceed",
                    "/tmp/work/shop",
                    "convo-a",
                    1781428000000,
                ),
            ]
            .join("\n"),
        )]);
        let p = AntigravityCliProvider::new(root.clone(), Box::new(RegistryParser::default()));
        let sessions = p.discover_sessions();
        // Only the real prompt row surfaces, as Attention.
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "convo-a");
        assert_eq!(sessions[0].status, crate::app::Status::Attention);
        std::fs::remove_dir_all(&root).ok();
    }
}
