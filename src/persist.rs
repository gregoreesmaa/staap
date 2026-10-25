//! Local-only run-state persistence + markdown export.
//!
//! Per-run parsed links (+ transcript) survive restarts in a plain JSON
//! file under the platform data dir; the selected run exports its visible
//! text plus links to a local markdown file. No account, no sync, no
//! network — everything stays on this machine. Missing or corrupt files
//! degrade to empty, so the app always starts.

use std::path::{Path, PathBuf};

use crate::app::{ChatSession, Status};

/// Filename of the persisted run list inside [`data_dir`].
pub const RUNS_FILENAME: &str = "runs.json";
/// Subdir of [`data_dir`] holding exported markdown.
pub const EXPORTS_DIRNAME: &str = "exports";

/// Platform data dir for local-only state (`staap` subdir).
pub fn data_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(xdg).join("staap")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share/staap")
    } else {
        PathBuf::from(".local/share/staap")
    }
}

/// Default persist path (`runs.json` in the local data dir).
pub fn runs_path() -> PathBuf {
    data_dir().join(RUNS_FILENAME)
}

/// Default export dir (created on first export).
pub fn exports_dir() -> PathBuf {
    data_dir().join(EXPORTS_DIRNAME)
}

/// Save `sessions` to the default path (creating parent dirs). Best
/// effort: callers ignore the result; a failed save just means the next
/// start falls back to historic discovery alone. Production-only: unit
/// tests must never rewrite the developer's live runs file (the
/// roundtrip is covered through `save_sessions_to` with temp paths).
#[cfg(not(test))]
pub fn save_sessions(sessions: &[ChatSession]) -> std::io::Result<()> {
    save_sessions_to(&runs_path(), sessions)
}

/// Save `sessions` to `path` (creating parent dirs).
pub fn save_sessions_to(path: &Path, sessions: &[ChatSession]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(sessions)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, text)
}

/// Load persisted sessions from the default path; empty on any failure
/// (missing/corrupt file is a documented degraded mode, not an error).
pub fn load_sessions() -> Vec<ChatSession> {
    load_sessions_from(&runs_path())
}

/// Load persisted sessions from `path`; empty on any failure.
pub fn load_sessions_from(path: &Path) -> Vec<ChatSession> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<ChatSession>>(&text).ok())
        .unwrap_or_default()
}

/// Fold `donor`'s retained state into `keep` without changing `keep`'s
/// identity (issue #54): link lists union (so links that scrolled off
/// keep surviving; display order is re-applied by `push_links`), an empty
/// transcript fills from the donor, and the fresher activity stamp wins.
/// Title, project, and status always stay `keep`'s — identity is the id,
/// never the title.
fn union_into(keep: &mut ChatSession, mut donor: ChatSession) {
    keep.push_links(
        std::mem::take(&mut donor.pr_links),
        std::mem::take(&mut donor.related_links),
    );
    if keep.transcript.is_empty() && !donor.transcript.is_empty() {
        keep.transcript = donor.transcript;
        keep.transcript_truncated = donor.transcript_truncated;
    }
    // A merged row keeps its remote target: discovery never reports one,
    // so a local discovery copy must not wipe the persisted target (the
    // restart would otherwise redial locally instead of over SSH).
    if keep.remote.as_ref().is_none_or(|t| !t.is_valid())
        && donor.remote.as_ref().is_some_and(|t| t.is_valid())
    {
        keep.remote = donor.remote;
    }
    if donor.last_active > keep.last_active {
        keep.last_active = donor.last_active;
    }
}

/// Merge persisted sessions under historic discovery: discovered entries
/// win on title/project (fresher), link lists union (so links that
/// scrolled off keep surviving; display order is re-applied),
/// transcripts fill in when discovery has none. Persisted ids with no
/// discovered counterpart return as [`Status::Idle`] — they own no live
/// PTY after a restart.
///
/// One row per id (issue #54): the same provider session can surface more
/// than once in discovery (e.g. resumed under a second date dir), and two
/// same-titled rows must stay independent entries. Duplicate discoveries
/// fold into the freshest entry (greatest `last_active`), so a stale dead
/// copy never shadows the live row.
pub fn merge_sessions(
    discovered: Vec<ChatSession>,
    persisted: Vec<ChatSession>,
) -> Vec<ChatSession> {
    let mut out: Vec<ChatSession> = Vec::with_capacity(discovered.len());
    for fresh in discovered {
        match out.iter_mut().find(|s| s.id == fresh.id) {
            Some(keep) => {
                if fresh.last_active >= keep.last_active {
                    let old = std::mem::replace(keep, fresh);
                    union_into(keep, old);
                } else {
                    union_into(keep, fresh);
                }
            }
            None => out.push(fresh),
        }
    }
    for saved in persisted {
        match out.iter_mut().find(|s| s.id == saved.id) {
            Some(live) => union_into(live, saved),
            None => {
                let mut saved = saved;
                saved.status = Status::Idle;
                // Pre-sort persisted rows converge loaded state with live
                // rows without waiting for the next link arrival.
                saved.sort_links();
                out.push(saved);
            }
        }
    }
    out
}

/// Render the selected run as markdown: header, visible text, then links.
/// Falsifiable: the export contains the screen text plus every link.
pub fn export_markdown(session: &ChatSession, body: &str) -> String {
    let mut out = format!(
        "# {title}\n\n- run: `{id}` · project: `{project}` · status: {status:?} · exported (unix): {now}\n",
        title = session.title,
        id = session.id,
        project = session.project,
        status = session.status,
        now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    // Issue #48: the session folder is part of the run detail, next to
    // the header — a re-opened export still says where the run lived.
    if let Some(dir) = session.cwd.as_deref() {
        out.push_str(&format!("\n- folder: `{dir}`\n"));
    }
    out.push_str("\n## Terminal\n\n```text\n");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("```\n");
    if !session.pr_links.is_empty() {
        out.push_str("\n## PR links\n\n");
        for link in &session.pr_links {
            out.push_str(&format!("- {link}\n"));
        }
    }
    if !session.related_links.is_empty() {
        out.push_str("\n## Related links\n\n");
        for link in &session.related_links {
            out.push_str(&format!("- {link}\n"));
        }
    }
    out
}

/// Write the export into `dir`, returning the file path. Filename is the
/// run id plus a sanitized title suffix so concurrent runs never collide.
pub fn write_export(session: &ChatSession, body: &str, dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let mut title: String = session
        .title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    title = collapse_dashes(&title);
    title.truncate(40);
    title = title.trim_matches('-').to_string();
    let name = if title.is_empty() {
        format!("{}.md", session.id)
    } else {
        format!("{}-{}.md", session.id, title)
    };
    let path = dir.join(name);
    std::fs::write(&path, export_markdown(session, body))?;
    Ok(path)
}

fn collapse_dashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.chars() {
        if c == '-' {
            if !last_dash {
                out.push(c);
            }
            last_dash = true;
        } else {
            out.push(c);
            last_dash = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Status;
    use crate::transcript::{Role, TranscriptMessage};

    fn sess(id: &str) -> ChatSession {
        ChatSession {
            id: id.into(),
            title: format!("{id} title"),
            project: "proj".into(),
            status: Status::Working,
            harness: crate::app::HARNESS_MUSE.into(),
            last_active: 7,
            provider_session_id: None,
            pr_links: vec!["https://github.com/acme/app/pull/1".into()],
            related_links: vec!["src/app.rs:9".into()],
            links_truncated: false,
            transcript: vec![TranscriptMessage {
                role: Role::User,
                text: "Fix login".into(),
            }],
            transcript_truncated: false,
            title_locked: true,
            pending_input: String::new(),
            cwd: None,
            remote: None,
        }
    }

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "staap-persist-test-{}-{name}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn session_folder_survives_a_save_load_roundtrip() {
        // Issue #48: the picked folder is run state — it persists
        // local-only with everything else, and pre-folder files (no
        // `cwd` key) still load as the default.
        let mut with_dir = sess("a");
        with_dir.cwd = Some("/tmp/demo-proj".to_string());
        let path = tmp_path("runs.json");
        save_sessions_to(&path, &[with_dir]).unwrap();
        let back = load_sessions_from(&path);
        assert_eq!(back[0].cwd.as_deref(), Some("/tmp/demo-proj"));
        let legacy = r#"[{"id":"old","title":"old","project":"p","status":"Idle",
            "last_active":1,"pr_links":[],"related_links":[]}]"#;
        let legacy_path = tmp_path("legacy.json");
        std::fs::write(&legacy_path, legacy).unwrap();
        let legacy_back = load_sessions_from(&legacy_path);
        assert_eq!(legacy_back.len(), 1);
        assert_eq!(legacy_back[0].cwd, None);
    }

    #[test]
    fn remote_target_survives_a_save_load_roundtrip() {
        // The SSH target is run state like the folder: host + remote dir
        // persist local-only, merge keeps a discovered copy from wiping
        // it, and pre-target files still load as local.
        use crate::launch::SshTarget;
        let mut with_remote = sess("a");
        with_remote.remote = Some(SshTarget::new(
            "mybox".to_string(),
            Some("/srv/api".to_string()),
        ));
        let path = tmp_path("runs.json");
        save_sessions_to(&path, &[with_remote]).unwrap();
        let back = load_sessions_from(&path);
        assert_eq!(
            back[0].remote,
            Some(SshTarget::new(
                "mybox".to_string(),
                Some("/srv/api".to_string())
            ))
        );
        // Merge: a discovered local copy never clears the stored target.
        let mut local = sess("a");
        local.last_active = 9;
        let merged = merge_sessions(vec![local], back);
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].remote.as_ref().map(|t| t.host.as_str()),
            Some("mybox")
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn export_names_the_session_folder() {
        // Issue #48: the run detail (markdown export) says where the
        // run lived; folder-less runs export exactly as before.
        let mut with_dir = sess("a");
        with_dir.cwd = Some("/tmp/demo-proj".to_string());
        let with = export_markdown(&with_dir, "body\n");
        assert!(with.contains("- folder: `/tmp/demo-proj`"), "{with:?}");
        let without = export_markdown(&sess("b"), "body\n");
        assert!(!without.contains("folder:"), "{without:?}");
    }

    #[test]
    fn links_and_transcript_survive_a_save_load_roundtrip() {
        let path = tmp_path("runs.json");
        let sessions = vec![sess("a"), sess("b")];
        save_sessions_to(&path, &sessions).unwrap();
        let back = load_sessions_from(&path);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].pr_links, sessions[0].pr_links);
        assert_eq!(back[0].related_links, sessions[0].related_links);
        assert_eq!(back[0].transcript, sessions[0].transcript);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_or_corrupt_runs_degrade_to_empty() {
        assert!(load_sessions_from(&tmp_path("missing.json")).is_empty());
        let path = tmp_path("corrupt.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[[[broken").unwrap();
        assert!(load_sessions_from(&path).is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn generated_ids_survive_a_save_load_roundtrip() {
        // Issue #54: the UUID row key is plain persisted state — a
        // restart loads the same ids back, so rows keep their identity
        // (and selection/PTY routing) instead of colliding by name.
        let path = tmp_path("uuids.json");
        let sessions: Vec<ChatSession> = (0..5)
            .map(|_| {
                let mut s = sess(&crate::app::new_session_id());
                s.title = "otter".into();
                s
            })
            .collect();
        let ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();
        save_sessions_to(&path, &sessions).unwrap();
        let back = load_sessions_from(&path);
        let back_ids: Vec<String> = back.iter().map(|s| s.id.clone()).collect();
        assert_eq!(back_ids, ids);
        assert_eq!(
            back_ids
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            5,
            "ids stay unique across the roundtrip"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn duplicate_discoveries_fold_into_one_live_row() {
        // Issue #54: the same provider session surfacing twice (e.g. a
        // resumed session under a second date dir) renders a single row —
        // the freshest entry wins, links union, and no stale dead copy
        // shadows the live one.
        let mut stale = sess("dup");
        stale.title = "stale title".into();
        stale.last_active = 3;
        stale.status = Status::Idle;
        stale.transcript.clear();
        let mut fresh = sess("dup");
        fresh.title = "fresh title".into();
        fresh.last_active = 9;
        fresh.status = Status::Working;
        fresh.pr_links.clear();
        let merged = merge_sessions(vec![stale, fresh], vec![]);
        assert_eq!(merged.len(), 1);
        let row = &merged[0];
        assert_eq!(row.id, "dup");
        assert_eq!(row.title, "fresh title");
        assert_eq!(row.status, Status::Working);
        assert_eq!(row.last_active, 9);
        // Links union across both copies: the stale copy's PR link plus
        // the fresh copy's related link.
        assert_eq!(
            row.pr_links,
            vec!["https://github.com/acme/app/pull/1".to_string()]
        );
        assert_eq!(row.related_links, vec!["src/app.rs:9".to_string()]);
        // Transcript fills from the copy that has one.
        assert_eq!(row.transcript.len(), 1);
    }

    #[test]
    fn same_titled_persisted_ghost_never_shadows_the_live_row() {
        // Issue #54: a dead persisted entry sharing the live row's title
        // parks alongside as its own idle row — identity is the id, so
        // lookups by id always reach the live session.
        let mut live = sess("live-uuid");
        live.title = "otter".into();
        live.status = Status::Working;
        let mut ghost = sess("ghost-uuid");
        ghost.title = "otter".into();
        ghost.status = Status::Working;
        let merged = merge_sessions(vec![live], vec![ghost]);
        assert_eq!(merged.len(), 2);
        assert_eq!(
            merged.iter().find(|s| s.id == "live-uuid").unwrap().status,
            Status::Working
        );
        let parked = merged.iter().find(|s| s.id == "ghost-uuid").unwrap();
        assert_eq!(parked.status, Status::Idle);
        assert_eq!(parked.title, "otter");
    }

    #[test]
    fn merge_unions_links_and_parks_unknown_ids_idle() {
        let mut discovered = vec![sess("a")];
        discovered[0].pr_links.clear();
        discovered[0].transcript.clear();
        let merged = merge_sessions(discovered, vec![sess("a"), sess("ghost")]);
        assert_eq!(merged.len(), 2);
        let a = merged.iter().find(|s| s.id == "a").unwrap();
        // Persisted links union back in; transcript fills the empty slot.
        assert_eq!(a.pr_links.len(), 1);
        assert_eq!(a.transcript.len(), 1);
        // Discovered title wins over the stale persisted one.
        let ghost = merged.iter().find(|s| s.id == "ghost").unwrap();
        assert_eq!(ghost.status, Status::Idle);
    }

    #[test]
    fn export_contains_screen_text_plus_links() {
        let s = sess("a");
        let md = export_markdown(&s, "waiting for approval\nline two");
        assert!(md.contains("waiting for approval"));
        assert!(md.contains("https://github.com/acme/app/pull/1"));
        assert!(md.contains("src/app.rs:9"));
        assert!(md.contains("# a title"));
    }

    #[test]
    fn export_filename_is_sanitized_and_unique_per_run() {
        let dir = tmp_path("exports");
        let mut s = sess("run-9");
        s.title = "Fix login! (urgent)".into();
        let path = write_export(&s, "body", &dir).unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("run-9-"), "got {name}");
        assert!(name.ends_with(".md"));
        assert!(!name.contains('!') && !name.contains(' '));
        assert!(path.is_file());
        std::fs::remove_dir_all(&dir).ok();
    }
}
