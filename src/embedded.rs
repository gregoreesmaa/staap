//! Embedded interactive `muse` session behind a PTY with vt100 emulation.
//!
//! The right pane is a live terminal: [`EmbeddedPty`] spawns a child command
//! (normally plain `muse` for a new session) under a PTY from the
//! `portable-pty` crate, feeds every output byte into a [`vt100::Parser`],
//! and forwards focused keystrokes. The parser keeps the full terminal
//! state — cursor addressing, colors, alternate screen — so the UI renders
//! exactly what `muse` draws, instead of a stripped line scrollback.
//! [`LiveView`] is the render snapshot handed to [`crate::ui`].
//!
//! Tests use fake commands (`echo`, shell) so they never need a real `muse`.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use anyhow::{Context, Result};

use crate::scrollback::ScrollbackLog;

/// Command used to start a fresh session: plain interactive `muse`.
pub fn new_session_command() -> (String, Vec<String>) {
    ("muse".to_string(), Vec::new())
}

/// Which agent CLI backs a spawn.
///
/// `Muse` is the default: bare [`SpawnKind::New`] / [`SpawnKind::Resume`]
/// keep meaning muse, so every existing call site compiles untouched.
/// Other harnesses spawn through [`SpawnKind::NewOn`] /
/// [`SpawnKind::ResumeOn`]. The picker catalog (`crate::launch`) may list
/// more CLIs than this enum resolves (codex spawns by program name until
/// it grows full provider support here); `from_id` maps those to
/// the closest spawnable harness so rows still re-attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Harness {
    /// `muse` — the original and default harness.
    #[default]
    Muse,
    /// `opencode` — the opencode CLI.
    Opencode,
    /// `claude` — the Claude Code CLI (discovery + transcript land with
    /// its provider; resume reuses the muse `--resume <id>` shape).
    Claude,
    /// `codex` — the Codex CLI (discovery + transcript land with
    /// its provider; resume is the `resume <id>` subcommand).
    Codex,
    /// `agy` — the Antigravity CLI (discovery + transcript land with
    /// its provider; resume is `agy --conversation <id>`).
    Antigravity,
}

impl Harness {
    /// Stable harness id, matching [`crate::app::harness_badge`] and the
    /// `harness` field stored on [`crate::app::ChatSession`].
    pub fn id(self) -> &'static str {
        match self {
            Harness::Muse => crate::app::HARNESS_MUSE,
            Harness::Opencode => crate::app::HARNESS_OPENCODE,
            Harness::Claude => crate::app::HARNESS_CLAUDE,
            Harness::Codex => crate::app::HARNESS_CODEX,
            Harness::Antigravity => crate::app::HARNESS_ANTIGRAVITY,
        }
    }

    /// Map a stored harness id back to a harness. Unknown/future ids fall
    /// back to muse (whose resume shape is the long-standing default), so
    /// old or foreign rows still re-attach instead of failing to spawn.
    pub fn from_id(id: &str) -> Self {
        if id == crate::app::HARNESS_OPENCODE {
            Harness::Opencode
        } else if id == crate::app::HARNESS_CLAUDE {
            Harness::Claude
        } else if id == crate::app::HARNESS_CODEX {
            Harness::Codex
        } else if id == crate::app::HARNESS_ANTIGRAVITY {
            Harness::Antigravity
        } else {
            Harness::Muse
        }
    }

    /// Program to spawn for this harness.
    pub fn program(self) -> &'static str {
        match self {
            Harness::Muse => "muse",
            Harness::Opencode => "opencode",
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Antigravity => "agy",
        }
    }

    /// Canonical yolo flag for this harness, if the picker supports one.
    /// (`muse --yolo` and `claude --dangerously-skip-permissions` are the
    /// documented config flags; codex `--dangerously-bypass-approvals-and-
    /// sandbox` and opencode `--auto` are verified against the live CLI
    /// references.) Unknown/future harnesses carry none — the toggle
    /// hides instead of guessing.
    pub fn yolo_flag(self) -> Option<&'static str> {
        match self {
            Harness::Muse => Some("--yolo"),
            Harness::Claude => Some("--dangerously-skip-permissions"),
            Harness::Codex => Some("--dangerously-bypass-approvals-and-sandbox"),
            Harness::Opencode => Some("--auto"),
            Harness::Antigravity => None,
        }
    }

    /// argv (after the program) starting a fresh interactive session.
    pub fn new_args(self) -> Vec<String> {
        Vec::new()
    }

    /// argv (after the program) resuming a historic conversation.
    /// Shape is per-CLI: muse and claude take `--resume <id>`, opencode
    /// takes `--session <id>`, codex takes `resume <id>` (subcommand, per
    /// the developer-commands reference), agy takes `--conversation <id>`.
    pub fn resume_args(self, session_id: &str) -> Vec<String> {
        match self {
            Harness::Opencode => vec!["--session".to_string(), session_id.to_string()],
            Harness::Codex => vec!["resume".to_string(), session_id.to_string()],
            Harness::Antigravity => {
                vec!["--conversation".to_string(), session_id.to_string()]
            }
            _ => vec!["--resume".to_string(), session_id.to_string()],
        }
    }

    /// Full fresh-session command for this harness.
    pub fn new_command(self) -> (String, Vec<String>) {
        (self.program().to_string(), self.new_args())
    }

    /// Full resume command for this harness.
    pub fn resume_command(self, session_id: &str) -> (String, Vec<String>) {
        (self.program().to_string(), self.resume_args(session_id))
    }
}

/// What the app asked the PTY layer to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnKind {
    /// Start a brand-new `muse` session.
    New,
    /// Start a brand-new session on another harness (`New` stays muse).
    /// `yolo` forces the harness's canonical yolo flag for this run only
    /// (the config default applies when false) — the 2D-launch path.
    NewOn { harness: Harness, yolo: bool },
    /// Resume a historic provider conversation with `muse --resume <id>`.
    /// The app routes the spawn to the selected run entry, so no run id
    /// travels with the request.
    Resume { session_id: String },
    /// Resume a historic conversation on another harness (per-harness
    /// resume shape via [`Harness::resume_command`]).
    ResumeOn {
        harness: Harness,
        session_id: String,
    },
}

impl SpawnKind {
    pub fn command(&self) -> (String, Vec<String>) {
        match self {
            SpawnKind::New => Harness::Muse.new_command(),
            SpawnKind::Resume { session_id } => Harness::Muse.resume_command(session_id),
            SpawnKind::NewOn { harness, .. } => harness.new_command(),
            SpawnKind::ResumeOn {
                harness,
                session_id,
            } => harness.resume_command(session_id),
        }
    }

    /// Harness this spawn runs (the config key for `extra_args`/yolo is
    /// [`Harness::id`]).
    pub fn harness(&self) -> Harness {
        match self {
            SpawnKind::New => Harness::Muse,
            SpawnKind::NewOn { harness, .. } => *harness,
            SpawnKind::Resume { .. } => Harness::Muse,
            SpawnKind::ResumeOn { harness, .. } => *harness,
        }
    }

    /// CLI this spawn runs (the config key for `extra_args`/yolo).
    pub fn cli_id(&self) -> &str {
        self.harness().id()
    }

    /// One-shot yolo for this spawn (2D launch): config default when false.
    pub fn yolo_once(&self) -> bool {
        match self {
            SpawnKind::NewOn { yolo, .. } => *yolo,
            _ => false,
        }
    }
}

/// Lines of scrollback retained by the vt100 emulator.
pub const SCROLLBACK_LINES: usize = 2000;

/// A device query is short; keep this many trailing bytes across chunks so a
/// query split over two reads is still recognized.
const QUERY_CARRY: usize = 16;

/// Answer a CPR (cursor-position request) that arrived in `data` (plus
/// `carry` from the previous chunk) with the emulator's current cursor.
/// Returns the reply bytes plus the new carry. A real terminal answers
/// `\x1b[6n` with `\x1b[{row};{col}R` (1-based); without that answer `muse`
/// retries twice and then gives up and exits.
fn cpr_replies(data: &[u8], carry: &[u8], cursor: (u16, u16)) -> (Vec<u8>, Vec<u8>) {
    const QUERY: &[u8] = b"\x1b[6n";
    const DEC_QUERY: &[u8] = b"\x1b[?6n";
    let mut window = Vec::with_capacity(carry.len() + data.len());
    window.extend_from_slice(carry);
    window.extend_from_slice(data);
    let mut replies = Vec::new();
    // Matches wholly inside the old carry were already answered last round —
    // only answer matches that touch new data, otherwise the child would
    // receive duplicate replies as ghost input.
    let old = carry.len();
    let mut i = 0;
    while i + QUERY.len() <= window.len() {
        if window[i..].starts_with(QUERY) {
            if i + QUERY.len() > old {
                replies.extend_from_slice(
                    format!("\x1b[{};{}R", cursor.0 + 1, cursor.1 + 1).as_bytes(),
                );
            }
            i += QUERY.len();
        } else if i + DEC_QUERY.len() <= window.len() && window[i..].starts_with(DEC_QUERY) {
            if i + DEC_QUERY.len() > old {
                replies.extend_from_slice(
                    format!("\x1b[?{};{}R", cursor.0 + 1, cursor.1 + 1).as_bytes(),
                );
            }
            i += DEC_QUERY.len();
        } else {
            i += 1;
        }
    }
    let keep = window.len().min(QUERY_CARRY);
    let new_carry = window[window.len() - keep..].to_vec();
    (replies, new_carry)
}

/// Render snapshot of the live pane, borrowed from the [`EmbeddedPty`]
/// each frame. Issue #32 removed the title row, so the view carries no
/// header text: the screen (with its last frame) is the whole story.
pub struct LiveView<'a> {
    /// Emulated terminal screen (already processed output).
    pub screen: &'a vt100::Screen,
    /// True once the child has exited (last frame stays visible).
    pub exited: bool,
}

/// 8-bit RGB triple, framework-free (epic #60 slice 2: the owned FFI
/// shape; mirrors the gui-local `gui::terminal::Rgb8` without moving it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapRgb(pub u8, pub u8, pub u8);

/// Cell style in framework-free form (`None` = terminal default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapStyle {
    pub fg: Option<SnapRgb>,
    pub bg: Option<SnapRgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// One coalesced same-style span of a row (owned text, no lifetime).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapSpan {
    pub text: String,
    pub style: SnapStyle,
}

/// Owned screen snapshot for the FFI edge: plain text plus style spans,
/// so no `LiveView` lifetime or `&vt100::Screen` crosses the boundary
/// (epic #60 §5.2). `cursor` is the 0-based caret cell, if known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSnapshot {
    pub text: String,
    pub rows: u16,
    pub cols: u16,
    pub cursor: Option<(u16, u16)>,
    pub exited: bool,
    pub spans: Vec<Vec<SnapSpan>>,
}

/// Standard xterm 16-color palette as RGB triples (same values as the
/// gui-local table; duplicated so the core stays framework-free).
const SNAP_PALETTE_16: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

/// Map a vt100 color to RGB on the dark palette (`None` = default).
fn snap_color(color: vt100::Color) -> Option<SnapRgb> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => {
            let i = i as usize;
            if i < 16 {
                let (r, g, b) = SNAP_PALETTE_16[i];
                Some(SnapRgb(r, g, b))
            } else if i < 232 {
                let v = i - 16;
                let comp = |c: usize| {
                    if c == 0 {
                        0
                    } else {
                        (55 + 40 * c) as u8
                    }
                };
                Some(SnapRgb(comp(v / 36), comp((v % 36) / 6), comp(v % 6)))
            } else {
                let g = (8 + 10 * (i - 232)) as u8;
                Some(SnapRgb(g, g, g))
            }
        }
        vt100::Color::Rgb(r, g, b) => Some(SnapRgb(r, g, b)),
    }
}

/// Render the emulated screen grid as rows of coalesced owned spans.
/// Same coalescing as the gui-local `screen_rows` minus the framework
/// color conversion and cursor highlight (the caret crosses separately
/// via [`ScreenSnapshot::cursor`]).
pub fn snapshot_rows(screen: &vt100::Screen) -> Vec<Vec<SnapSpan>> {
    let (rows, cols) = screen.size();
    let mut out = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let mut spans: Vec<SnapSpan> = Vec::new();
        let mut buf = String::new();
        let mut cur = SnapStyle {
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
        };
        let mut open = false;
        for c in 0..cols {
            let Some(cell) = screen.cell(r, c) else {
                break;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let (mut fg, mut bg) = (snap_color(cell.fgcolor()), snap_color(cell.bgcolor()));
            if cell.inverse() {
                std::mem::swap(&mut fg, &mut bg);
            }
            let style = SnapStyle {
                fg,
                bg,
                bold: cell.bold(),
                italic: cell.italic(),
                underline: cell.underline(),
            };
            if !open {
                cur = style;
                open = true;
            } else if style != cur {
                spans.push(SnapSpan {
                    text: std::mem::take(&mut buf),
                    style: cur,
                });
                cur = style;
            }
            // Never-written grid cells carry no contents; render the gap
            // as a space so cursor-addressed words keep their separation
            // (issue #104: "Runningthetest" in the spans-fed shells).
            let text = cell.contents();
            if text.is_empty() {
                buf.push(' ');
            } else {
                buf.push_str(&text);
            }
        }
        if open {
            spans.push(SnapSpan {
                text: buf,
                style: cur,
            });
        }
        out.push(spans);
    }
    out
}

/// A live child process behind a PTY with a vt100-emulated screen.
///
/// Output is collected by a background reader thread into a channel; call
/// [`EmbeddedPty::pump`] regularly on the app thread to feed it into the
/// emulator. Input is written straight to the PTY master.
pub struct EmbeddedPty {
    writer: Box<dyn Write + Send>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    rx: Receiver<Vec<u8>>,
    _reader_thread: JoinHandle<()>,
    parser: vt100::Parser,
    /// Retained plain-text mirror of every output byte (issue #25): the
    /// vt100 0.15 API exposes no scrollback rows, so the pager reads here.
    scrollback: ScrollbackLog,
    query_carry: Vec<u8>,
    exited: bool,
}

/// Whether a resolved Windows program path can be launched directly by
/// `CreateProcessW` (what the ConPTY backend calls). Only native PE
/// executables (`.exe`) qualify: batch files (`.bat`/`.cmd`) and other
/// script shims need a command interpreter (`cmd /d /s /c`), which
/// `CreateProcessW` does not implicitely provide — spawning them directly
/// surfaces a console host / elevation-style prompt instead of running
/// the CLI (the `muse` shim on Windows is a `.cmd` launcher).
fn is_directly_spawnable(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
}

/// Resolve a bare command name against explicit search dirs, trying the
/// exact name first and then each `exts` suffix (Windows `PATHEXT`
/// probing). Pure so unit tests cover it on every OS.
///
/// Background (issue #64): on Windows portable-pty rebuilds its spawn
/// environment from the machine/user registry, discarding process-local
/// `PATH` entries — so a bare `muse` that only exists via this process's
/// `PATH` (direnv-style shells, the smoke harness's fake `muse.exe` dir)
/// never resolves and `CreateProcessW` fails with "file not found".
/// Pre-resolving to an absolute path honors the process environment on
/// every backend. Names carrying a directory, resolving nowhere, or
/// resolving to a non-directly-spawnable script shim (`.cmd`/`.bat`;
/// see [`is_directly_spawnable`]) yield `None` so the backend keeps its
/// own lookup and error — or, on Windows, the caller routes through the
/// command interpreter instead of handing a script to `CreateProcessW`.
fn resolve_bare_program(
    program: &str,
    path_value: &std::ffi::OsStr,
    exts: &[&str],
) -> Option<std::path::PathBuf> {
    if program.is_empty() || program.contains('/') || program.contains('\\') {
        return None;
    }
    for dir in std::env::split_paths(path_value) {
        let base = dir.join(program);
        // Extensionless hits (Unix binaries, hermetic test fixtures) pass
        // through on every OS; hits carrying an extension must be directly
        // spawnable on Windows so `.cmd`/`.bat` shims never reach
        // `CreateProcessW` (they route via `cmd /d /s /c` instead).
        let base_ok = base.is_file()
            && (base.extension().is_none() || cfg!(not(windows)) || is_directly_spawnable(&base));
        if base_ok {
            return Some(base);
        }
        for ext in exts {
            let candidate = dir.join(format!("{program}{ext}"));
            if candidate.is_file() && (cfg!(not(windows)) || is_directly_spawnable(&candidate)) {
                return Some(candidate);
            }
        }
    }
    None
}

/// PATHEXT suffixes from the live process environment (`.exe` fallback
/// when unset/empty), in order.
fn pathext_suffixes() -> Vec<String> {
    std::env::var_os("PATHEXT")
        .map(|v| {
            std::env::split_paths(&v)
                .filter_map(|e| e.to_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![".exe".to_owned()])
}

/// Locate the raw `PATH` hit for `program` (no spawnability filter):
/// the exact file the shell would run, including `.cmd`/`.bat` shims.
/// `None` means "not a bare name or not found".
fn locate_on_process_path(program: &str) -> Option<std::path::PathBuf> {
    if program.is_empty() || program.contains('/') || program.contains('\\') {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    let pathext = pathext_suffixes();
    for dir in std::env::split_paths(&path) {
        let base = dir.join(program);
        if base.is_file() {
            return Some(base);
        }
        for ext in &pathext {
            let candidate = dir.join(format!("{program}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Build the `cmd /d /s /c "<shim> <args...>"` spawn for a Windows script
/// shim (`.cmd`/`.bat`) found on the process `PATH`. Returns `None` when
/// `program` is not a bare name, resolves nowhere, or already spawns
/// directly (native `.exe`) — those keep the existing path untouched.
///
/// Defined on every OS (pure PATH probing + string building) so the
/// `cfg!(windows)`-gated call site and the headless tests compile in the
/// cross-platform contract; it only ever returns `Some` for real `.cmd` /
/// `.bat` hits, which only occur on Windows.
fn script_shim_command(program: &str, args: &[String]) -> Option<(String, Vec<String>)> {
    let hit = locate_on_process_path(program)?;
    if is_directly_spawnable(&hit) {
        return None;
    }
    let ext = hit
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ext != "cmd" && ext != "bat" {
        return None;
    }
    // `cmd /d /s /c` plus the shim path and the CLI args as separate
    // argv elements: portable-pty quotes each element itself
    // (`append_quoted`), so the spaced path stays one word and cmd joins
    // everything back into `"path" args...` for the shim.
    let mut argv = vec!["/d".to_string(), "/s".to_string(), "/c".to_string()];
    argv.push(hit.to_string_lossy().into_owned());
    argv.extend(args.iter().cloned());
    Some((
        std::env::var_os("COMSPEC")
            .map(|v| v.to_string_lossy().into_owned())
            .unwrap_or_else(|| "cmd.exe".to_string()),
        argv,
    ))
}

/// Resolve `program` against the live process `PATH`/`PATHEXT`, exactly
/// what the backend should have searched. `None` means "not a bare name,
/// not found, or not directly spawnable": pass the original through
/// untouched (script shims are handled by [`script_shim_command`]).
fn resolve_against_process_path(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    let pathext = pathext_suffixes();
    let exts: Vec<&str> = pathext.iter().map(String::as_str).collect();
    resolve_bare_program(program, &path, &exts)
}

impl EmbeddedPty {
    /// Spawn `program` with `args` in a PTY of `cols` x `rows`.
    pub fn spawn(program: &str, args: &[String], cols: u16, rows: u16) -> Result<Self> {
        Self::spawn_with_cwd(program, args, cols, rows, None)
    }

    /// Same as [`EmbeddedPty::spawn`] but starting the child in `cwd`
    /// (issue #48): the single spawn seam every backend funnels through,
    /// so a picked folder applies agent-neutrally. `None` inherits the
    /// app's own directory (the behavior before per-session folders).
    pub fn spawn_with_cwd(
        program: &str,
        args: &[String],
        cols: u16,
        rows: u16,
        cwd: Option<&std::path::Path>,
    ) -> Result<Self> {
        let spawn_desc = if args.is_empty() {
            program.to_string()
        } else {
            format!("{prog} {args}", prog = program, args = args.join(" "))
        };
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;
        // Windows only (issue #64): the backend searches the registry
        // PATH instead of this process's, so pre-resolve bare names
        // against the live environment. A resolved script shim (`.cmd` /
        // `.bat`, e.g. the `muse` launcher) cannot run via `CreateProcessW`
        // directly — route it through `cmd /d /s /c` so New Session starts
        // the CLI instead of surfacing a console-host permission prompt.
        // Everywhere else (and for names with a directory, or misses) the
        // original passes through, so behavior and error text are
        // unchanged there.
        let interpreted: Option<(String, Vec<String>)> = if cfg!(windows) {
            script_shim_command(program, args)
        } else {
            None
        };
        let resolved: Option<std::path::PathBuf> = if cfg!(windows) && interpreted.is_none() {
            resolve_against_process_path(program)
        } else {
            None
        };
        let mut cmd = match &interpreted {
            Some((shell_prog, shell_args)) => {
                let mut builder = portable_pty::CommandBuilder::new(shell_prog);
                builder.args(shell_args);
                builder
            }
            None => match &resolved {
                Some(abs) => portable_pty::CommandBuilder::new(abs),
                None => portable_pty::CommandBuilder::new(program),
            },
        };
        if interpreted.is_none() {
            cmd.args(args);
        }
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("failed to spawn `{spawn_desc}`"))?;
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .context("pty take_writer failed")?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("pty try_clone_reader failed")?;
        let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::channel();
        let reader_thread = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF: child exited.
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break; // Owner gone.
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            writer,
            _master: pair.master,
            child,
            rx,
            _reader_thread: reader_thread,
            parser: vt100::Parser::new(rows, cols, SCROLLBACK_LINES),
            scrollback: ScrollbackLog::new(),
            query_carry: Vec::new(),
            exited: false,
        })
    }

    /// Feed queued output into the emulator. Returns true when new output
    /// arrived or the child newly exited (both change what the UI shows,
    /// so both must dirty the pump). Also polls the child, recording exit
    /// exactly once.
    pub fn pump(&mut self) -> bool {
        let mut fresh = false;
        while let Ok(chunk) = self.rx.try_recv() {
            fresh = true;
            self.ingest(&chunk);
        }
        if !self.exited {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    fresh = true;
                    self.exited = true;
                    // Feed any last bytes the reader thread already queued.
                    while let Ok(chunk) = self.rx.try_recv() {
                        fresh = true;
                        self.ingest(&chunk);
                    }
                    // No more chunks will complete a split tail: decode it.
                    self.scrollback.flush();
                }
                Ok(None) => {}
                Err(_) => {
                    fresh = true;
                    self.exited = true;
                }
            }
        }
        fresh
    }

    /// Forward raw bytes (already key-encoded by the app loop) to the child.
    pub fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes).context("pty write failed")?;
        self.writer.flush().context("pty flush failed")?;
        Ok(())
    }

    /// Resize the PTY and the emulator grid; errors after exit are ignored.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let _ = self._master.resize(portable_pty::PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.parser.set_size(rows, cols);
    }

    /// Retained mirror of all PTY output for the scrollback pager.
    pub fn scrollback_log(&self) -> &ScrollbackLog {
        &self.scrollback
    }

    /// Feed one output chunk into the emulator, answering any terminal
    /// queries (currently CPR) the way a real terminal would.
    fn ingest(&mut self, chunk: &[u8]) {
        self.scrollback.feed(chunk);
        self.parser.process(chunk);
        let cursor = self.parser.screen().cursor_position();
        let (replies, carry) = cpr_replies(chunk, &self.query_carry, cursor);
        self.query_carry = carry;
        if !replies.is_empty() {
            // Best effort: a closed pipe just means the child is gone.
            let _ = self.writer.write_all(&replies);
            let _ = self.writer.flush();
        }
    }

    /// Borrowed snapshot for the UI.
    pub fn view(&self) -> LiveView<'_> {
        LiveView {
            screen: self.parser.screen(),
            exited: self.exited,
        }
    }

    /// Owned plain-text snapshot of the emulated screen (FFI edge).
    pub fn snapshot_text(&self) -> String {
        self.parser.screen().contents()
    }

    /// Owned styled-span snapshot of the emulated screen (FFI edge).
    pub fn snapshot_spans(&self) -> Vec<Vec<SnapSpan>> {
        snapshot_rows(self.parser.screen())
    }

    /// Full owned snapshot: text, grid size, cursor, exit flag, spans.
    pub fn snapshot(&self) -> ScreenSnapshot {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        ScreenSnapshot {
            text: screen.contents(),
            rows,
            cols,
            cursor: Some(screen.cursor_position()),
            exited: self.exited,
            spans: snapshot_rows(screen),
        }
    }

    /// Plain-text contents of the emulated screen (for tests).
    #[cfg(test)]
    fn contents(&mut self) -> String {
        self.pump();
        self.parser.screen().contents()
    }
}

impl Drop for EmbeddedPty {
    fn drop(&mut self) {
        // Best effort: terminate and reap so no zombie survives.
        // try_wait reaps the zombie.
        if !self.exited {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.exited = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn pump_until(pty: &mut EmbeddedPty, needle: &str, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if pty.contents().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        pty.contents().contains(needle)
    }

    fn wait_exit(pty: &mut EmbeddedPty, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            pty.pump();
            if pty.view().exited {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        pty.pump();
        pty.view().exited
    }

    #[test]
    fn cpr_query_is_answered_with_one_based_cursor() {
        let (replies, carry) = cpr_replies(b"abc\x1b[6nxyz", &[], (4, 9));
        assert_eq!(replies, b"\x1b[5;10R");
        assert!(carry.len() <= 16);
    }

    #[test]
    fn cpr_query_split_across_chunks_is_answered_once() {
        let (r1, carry) = cpr_replies(b"ab\x1b[", &[], (0, 0));
        assert!(r1.is_empty());
        let (r2, carry) = cpr_replies(b"6n", &carry, (0, 0));
        assert_eq!(r2, b"\x1b[1;1R");
        // The answered query lingers in the carry but must not re-trigger.
        let (r3, _) = cpr_replies(b"plain", &carry, (0, 0));
        assert!(r3.is_empty());
    }

    #[test]
    fn dec_cpr_variant_is_answered() {
        let (replies, _) = cpr_replies(b"\x1b[?6n", &[], (2, 3));
        assert_eq!(replies, b"\x1b[?3;4R");
    }

    #[test]
    fn harness_ids_map_and_route_spawn_commands() {
        // opencode spawn surface: plain `opencode` starts fresh,
        // `opencode --session <id>` re-attaches a discovered session.
        assert_eq!(
            SpawnKind::NewOn {
                harness: Harness::Opencode,
                yolo: false,
            }
            .command(),
            ("opencode".to_string(), vec![])
        );
        assert_eq!(
            SpawnKind::ResumeOn {
                harness: Harness::Opencode,
                session_id: "ses-1".to_string(),
            }
            .command(),
            (
                "opencode".to_string(),
                vec!["--session".to_string(), "ses-1".to_string()]
            )
        );
        // Muse still routes through the same table (default harness).
        assert_eq!(
            SpawnKind::NewOn {
                harness: Harness::Muse,
                yolo: false,
            }
            .command(),
            ("muse".to_string(), vec![])
        );
        assert_eq!(Harness::Opencode.id(), crate::app::HARNESS_OPENCODE);
        assert_eq!(Harness::Muse.id(), crate::app::HARNESS_MUSE);
        // Unknown/future harness ids fall back to muse instead of failing.
        assert_eq!(Harness::from_id("opencode"), Harness::Opencode);
        assert_eq!(Harness::from_id("muse"), Harness::Muse);
        assert_eq!(Harness::from_id("claude"), Harness::Claude);
        assert_eq!(Harness::from_id("codex"), Harness::Codex);
        assert_eq!(Harness::from_id("future-harness"), Harness::Muse);
        assert_eq!(Harness::Opencode.program(), "opencode");
        assert_eq!(Harness::Claude.program(), "claude");
        assert_eq!(Harness::Codex.program(), "codex");
        assert_eq!(Harness::default(), Harness::Muse);
    }

    #[test]
    fn new_on_command_names_the_cli_without_flags() {
        // 2D launch: the command is the plain program; yolo rides via
        // the resolved selection, never baked into the base command.
        let kind = SpawnKind::NewOn {
            harness: Harness::Claude,
            yolo: true,
        };
        assert_eq!(kind.command(), ("claude".to_string(), Vec::new()));
        assert_eq!(kind.cli_id(), "claude");
        assert!(kind.yolo_once());
        assert!(!SpawnKind::New.yolo_once());
        assert_eq!(SpawnKind::New.cli_id(), "muse");
    }

    #[test]
    fn codex_harness_routes_spawn_commands() {
        // Codex spawn surface: plain `codex` starts fresh, `codex
        // resume <id>` (subcommand) re-attaches a discovered session.
        assert_eq!(
            SpawnKind::NewOn {
                harness: Harness::Codex,
                yolo: false,
            }
            .command(),
            ("codex".to_string(), vec![])
        );
        assert_eq!(
            SpawnKind::ResumeOn {
                harness: Harness::Codex,
                session_id: "sess-codex-1".to_string(),
            }
            .command(),
            (
                "codex".to_string(),
                vec!["resume".to_string(), "sess-codex-1".to_string()]
            )
        );
        assert_eq!(Harness::Codex.id(), crate::app::HARNESS_CODEX);
        assert_eq!(Harness::from_id("codex"), Harness::Codex);
        assert_eq!(Harness::Codex.program(), "codex");
    }

    #[test]
    fn antigravity_harness_routes_spawn_commands() {
        // Antigravity spawn surface: plain `agy` starts fresh,
        // `agy --conversation <id>` re-attaches a discovered session.
        assert_eq!(
            SpawnKind::NewOn {
                harness: Harness::Antigravity,
                yolo: false,
            }
            .command(),
            ("agy".to_string(), vec![])
        );
        assert_eq!(
            SpawnKind::ResumeOn {
                harness: Harness::Antigravity,
                session_id: "convo-1".to_string(),
            }
            .command(),
            (
                "agy".to_string(),
                vec!["--conversation".to_string(), "convo-1".to_string()]
            )
        );
        assert_eq!(Harness::Antigravity.id(), crate::app::HARNESS_ANTIGRAVITY);
        assert_eq!(Harness::from_id("antigravity"), Harness::Antigravity);
        assert_eq!(Harness::Antigravity.program(), "agy");
        assert_eq!(Harness::Antigravity.yolo_flag(), None);
    }

    #[test]
    fn resume_command_passes_provider_session_id() {
        // Issue #22: historic re-attach relaunches `muse --resume <id>`.
        let kind = SpawnKind::Resume {
            session_id: "sess-abc".to_string(),
        };
        assert_eq!(
            kind.command(),
            (
                "muse".to_string(),
                vec!["--resume".to_string(), "sess-abc".to_string()]
            )
        );
    }

    #[test]
    fn claude_harness_routes_spawn_commands() {
        // Claude spawn surface: plain `claude` starts fresh, `claude
        // --resume <id>` re-attaches a discovered conversation.
        assert_eq!(
            SpawnKind::NewOn {
                harness: Harness::Claude,
                yolo: false,
            }
            .command(),
            ("claude".to_string(), vec![])
        );
        assert_eq!(
            SpawnKind::ResumeOn {
                harness: Harness::Claude,
                session_id: "sess-claude-1".to_string(),
            }
            .command(),
            (
                "claude".to_string(),
                vec!["--resume".to_string(), "sess-claude-1".to_string()]
            )
        );
        assert_eq!(Harness::Claude.id(), crate::app::HARNESS_CLAUDE);
        assert_eq!(Harness::from_id("claude"), Harness::Claude);
        assert_eq!(Harness::Claude.program(), "claude");
    }

    /// Child-printed cwd contains the expected dir, tolerating the
    /// Windows verbatim prefix (`\\?\`) stripped by the caller plus
    /// Windows case-insensitivity (temp dirs may mix short/long case).
    fn cwd_matches(contents: &str, want: &str) -> bool {
        #[cfg(windows)]
        {
            contents.to_lowercase().contains(&want.to_lowercase())
        }
        #[cfg(not(windows))]
        {
            contents.contains(want)
        }
    }

    fn pump_until_cwd(pty: &mut EmbeddedPty, want: &str, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if cwd_matches(&pty.contents(), want) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cwd_matches(&pty.contents(), want)
    }

    #[test]
    fn spawn_with_cwd_runs_the_child_in_the_picked_folder() {
        // Issue #48 falsifiable check: a session started with folder
        // `/tmp/demo-proj` runs its agent with that cwd (the child
        // prints it). `None` keeps the old inherit behavior.
        // `pwd` is Unix-only (and Git-Bash `pwd` prints POSIX paths),
        // so Windows asks `cmd /C cd`, which prints the native path.
        let dir = std::env::temp_dir().join(format!(
            "staap-cwd-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let canon = std::fs::canonicalize(&dir).unwrap();
        let want = canon.to_string_lossy().into_owned();
        let want = want.strip_prefix(r"\\?\").unwrap_or(&want);
        let (program, args): (String, Vec<String>) = if cfg!(windows) {
            ("cmd".to_string(), vec!["/C".to_string(), "cd".to_string()])
        } else {
            ("pwd".to_string(), Vec::new())
        };
        let mut pty = EmbeddedPty::spawn_with_cwd(&program, &args, 80, 24, Some(canon.as_path()))
            .expect("cwd printer must spawn");
        assert!(pump_until_cwd(&mut pty, want, Duration::from_secs(5)));
        assert!(wait_exit(&mut pty, Duration::from_secs(5)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_without_cwd_inherits_as_before() {
        // The old seam is the new seam with `None`: still works.
        let mut pty =
            EmbeddedPty::spawn("echo", &["hi".to_string()], 80, 24).expect("echo must spawn");
        assert!(pump_until(&mut pty, "hi", Duration::from_secs(5)));
    }

    #[test]
    fn echo_output_reaches_emulated_screen_without_muse() {
        let mut pty = EmbeddedPty::spawn("echo", &["hello-pty".to_string()], 80, 24)
            .expect("echo must spawn");
        assert!(pump_until(&mut pty, "hello-pty", Duration::from_secs(5)));
        assert!(wait_exit(&mut pty, Duration::from_secs(5)));
    }

    #[test]
    fn cursor_addressing_renders_where_muse_draws() {
        // Direct emulator check: row/col addressing must place text, not
        // append lines — this is what a fullscreen muse TUI relies on.
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\x1b[2J\x1b[1;1Htop\x1b[5;10Hmiddle");
        let screen = parser.screen();
        let contents = screen.contents();
        assert!(contents.contains("top"));
        assert!(contents.contains("middle"));
        let cell = screen.cell(4, 9).expect("addressed cell exists");
        assert!(cell.contents().contains("m"));
    }

    #[test]
    fn snapshot_rows_preserves_spaces_between_addressed_words() {
        // Issue #104: a TUI writes words at addressed columns, leaving
        // never-written gaps between them. The spans snapshot must render
        // those gaps as spaces, or shells show "Runningthetest".
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\x1b[1;1H\xe2\x97\x8f\x1b[1;3HRunning\x1b[1;11Hthe");
        let rows = snapshot_rows(parser.screen());
        let line: String = rows[0].iter().map(|s| s.text.as_str()).collect();
        assert!(
            line.starts_with("\u{25cf} Running the"),
            "gaps stay spaces, got {line:?}"
        );
    }

    #[test]
    fn exit_is_detected_and_view_marks_exited() {
        // Issue #32 removed the header/exit-note text: the exited flag
        // (last frame stays visible) is the whole signal now.
        let mut pty = EmbeddedPty::spawn("true", &[], 80, 24).expect("true must spawn");
        assert!(wait_exit(&mut pty, Duration::from_secs(5)));
        assert!(pty.view().exited);
    }

    #[test]
    fn resize_after_spawn_does_not_panic() {
        let mut pty =
            EmbeddedPty::spawn("sleep", &["5".to_string()], 80, 24).expect("sleep must spawn");
        pty.resize(100, 30);
        pty.resize(80, 24);
    }

    #[test]
    fn input_forwarding_reaches_child() {
        // `head -n 1` echoes its first stdin line then exits: proves bytes
        // written via write_input arrive at the child.
        let mut pty = EmbeddedPty::spawn("head", &["-n".to_string(), "1".to_string()], 80, 24)
            .expect("head must spawn");
        // Give the child a moment to start, then send a line.
        std::thread::sleep(Duration::from_millis(300));
        pty.write_input(b"ping-input\n")
            .expect("write must succeed");
        assert!(pump_until(&mut pty, "ping-input", Duration::from_secs(5)));
    }

    #[test]
    fn owned_snapshot_helpers_match_borrowed_view() {
        // Epic #60 §5.2: the FFI edge copies text + spans, so the owned
        // snapshot must agree with the borrowed view at the same instant.
        let mut pty =
            EmbeddedPty::spawn("echo", &["snap-own".to_string()], 80, 24).expect("echo must spawn");
        assert!(pump_until(&mut pty, "snap-own", Duration::from_secs(5)));
        let snap = pty.snapshot();
        assert!(snap.text.contains("snap-own"));
        assert_eq!((snap.rows, snap.cols), pty.view().screen.size());
        assert_eq!(snap.exited, pty.view().exited);
        assert_eq!(snap.text, pty.snapshot_text());
        assert_eq!(snap.spans, pty.snapshot_spans());
        let joined: String = snap
            .spans
            .iter()
            .flatten()
            .map(|s| s.text.as_str())
            .collect();
        for line in snap.text.lines() {
            let line = line.trim_end();
            if !line.is_empty() {
                assert!(joined.contains(line), "spans must cover {line:?}");
            }
        }
    }

    #[test]
    fn bare_name_resolves_via_search_dirs() {
        // Issue #64: the Windows backend searches the registry PATH, so
        // the seam pre-resolves bare names against the process PATH.
        let dir = std::env::temp_dir().join(format!(
            "staap-resolve-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("muse"), b"fake").unwrap();
        let path = std::ffi::OsString::from(dir.as_os_str());
        assert_eq!(
            resolve_bare_program("muse", &path, &[]),
            Some(dir.join("muse"))
        );
        // PATHEXT probing: `muse` finds `muse.exe` when only that exists.
        std::fs::remove_file(dir.join("muse")).unwrap();
        std::fs::write(dir.join("muse.exe"), b"fake").unwrap();
        assert_eq!(
            resolve_bare_program("muse", &path, &[".exe"]),
            Some(dir.join("muse.exe"))
        );
        // Exact name still wins when spelled with its extension.
        assert_eq!(
            resolve_bare_program("muse.exe", &path, &[".exe"]),
            Some(dir.join("muse.exe"))
        );
        // Script shims never resolve for direct spawn on Windows: handing
        // a `.cmd`/`.bat` to `CreateProcessW` surfaces a console-host
        // permission prompt instead of running the CLI (the `muse`
        // launcher is a `.cmd`); the spawn seam routes those through
        // `cmd /d /s /c`. Off Windows the lookup is unchanged.
        std::fs::write(dir.join("muse.cmd"), b"fake").unwrap();
        assert_eq!(
            resolve_bare_program("muse.cmd", &path, &[".exe", ".cmd"]),
            if cfg!(windows) {
                None
            } else {
                Some(dir.join("muse.cmd"))
            }
        );
        std::fs::remove_file(dir.join("muse.cmd")).unwrap();
        // Misses and directory-carrying names pass through as None.
        assert_eq!(resolve_bare_program("nope-xyz", &path, &[".exe"]), None);
        assert_eq!(resolve_bare_program("sub/muse", &path, &[".exe"]), None);
        assert_eq!(resolve_bare_program(r"sub\muse", &path, &[".exe"]), None);
        assert_eq!(resolve_bare_program("", &path, &[".exe"]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_exe_counts_as_directly_spawnable() {
        assert!(is_directly_spawnable(std::path::Path::new("muse.exe")));
        assert!(is_directly_spawnable(std::path::Path::new("MUSE.EXE")));
        assert!(!is_directly_spawnable(std::path::Path::new("muse.cmd")));
        assert!(!is_directly_spawnable(std::path::Path::new("muse.bat")));
        assert!(!is_directly_spawnable(std::path::Path::new("muse")));
    }

    #[test]
    fn cmd_shim_routes_through_comspec() {
        // A `.cmd` hit on PATH becomes `cmd /d /s /c <shim> <args>`;
        // native `.exe` hits and misses stay on the direct path (None).
        // Runs on every OS: pure PATH probing + string building, joined
        // with the platform separator so the fake dir scans first.
        let dir = std::env::temp_dir().join(format!(
            "staap-shim-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("nexe.exe"), b"fake").unwrap();
        let old_path = std::env::var_os("PATH");
        let old_pathext = std::env::var_os("PATHEXT");
        let mut new_path = dir.as_os_str().to_owned();
        if let Some(old) = &old_path {
            new_path.push(if cfg!(windows) { ";" } else { ":" });
            new_path.push(old);
        }
        std::env::set_var("PATH", &new_path);
        // Unix has no PATHEXT, so spell one out (platform-joined) for the
        // `.cmd` probe; Windows already carries the real thing.
        if cfg!(not(windows)) {
            let pathext = std::env::join_paths([".CMD", ".EXE"]).unwrap();
            std::env::set_var("PATHEXT", &pathext);
        }
        // Uppercase suffix: the PATHEXT probe spells `.CMD`, and Unix
        // filesystems are case-sensitive (Windows is not, so this hits
        // on both).
        std::fs::write(dir.join("shim-tool.CMD"), b"fake").unwrap();
        let shim = script_shim_command("shim-tool", &["--version".to_string()]);
        assert!(shim.is_some(), "a .cmd hit must route via COMSPEC");
        let (shell, argv) = shim.unwrap();
        assert!(shell.to_ascii_lowercase().ends_with("cmd.exe"));
        assert_eq!(&argv[0..3], &["/d", "/s", "/c"]);
        assert!(
            argv[3].to_ascii_lowercase().contains("shim-tool.cmd"),
            "shim path: {:?}",
            argv
        );
        assert_eq!(&argv[4..], &["--version"]);
        assert!(script_shim_command("nexe", &[]).is_none());
        assert!(script_shim_command("definitely-not-a-real-binary-xyz", &[]).is_none());
        match old_path {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
        match old_pathext {
            Some(v) => std::env::set_var("PATHEXT", v),
            None => std::env::remove_var("PATHEXT"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_failure_reports_an_error() {
        let err = EmbeddedPty::spawn("definitely-not-a-real-binary-xyz", &[], 80, 24)
            .err()
            .expect("must fail");
        assert!(err.to_string().contains("definitely-not-a-real-binary-xyz"));
    }

    #[test]
    fn red_sgr_reaches_snapshot_spans() {
        // Issue #70: the FFI-edge span snapshot must preserve SGR colors —
        // the falsifiable `printf '\e[31mred\e[0m\n'` renders red because
        // this snapshot carries the style the plain text drops.
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\x1b[31mred\x1b[0m\n");
        let rows = snapshot_rows(parser.screen());
        let span = rows[0]
            .iter()
            .find(|s| s.text.contains("red"))
            .expect("red span survives");
        assert_eq!(span.style.fg, Some(SnapRgb(205, 0, 0)));
        assert!(!span.style.bold);
    }
}
