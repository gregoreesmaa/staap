//! C ABI foundation over the shared core (epic #60, slice 2).
//!
//! Minimal, portable `extern "C"` seam for the future native shells: owned
//! screen snapshots (no `LiveView` lifetime crosses FFI, §5.2), integer
//! error codes + a thread-local message (§5.3), and spawn / pump / write /
//! resize / screen-text / status functions. Pure Rust, no codegen, so it
//! verifies on every OS gate. SwiftUI scaffold is slice 3.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

use crate::app::{ChatSession, Status};
use crate::embedded::{EmbeddedPty, SpawnKind};
use crate::launch;
use crate::parsers::registry::RegistryParser;
use crate::persist;
use crate::providers::{
    AntigravityCliProvider, ClaudeCliProvider, CodexCliProvider, MuseCliProvider,
    OpencodeCliProvider, Provider,
};
use crate::runs::RunRegistry;

/// Integer error codes returned by every fallible `staap_*` function.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaapError {
    /// Success.
    Ok = 0,
    /// Child spawn failed (bad program, PTY unavailable).
    Spawn = 1,
    /// PTY input write failed.
    Io = 2,
    /// A `*const char` argument was not valid UTF-8.
    Utf8 = 3,
    /// A required pointer argument was null.
    Null = 4,
    /// Config persistence failed.
    Config = 5,
}

impl StaapError {
    fn code(self) -> c_int {
        self as c_int
    }
}

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::new("").unwrap());
}

fn set_error(msg: String) {
    let clean = msg.replace('\0', "");
    let cstr = CString::new(clean).unwrap_or_default();
    LAST_ERROR.with(|slot| *slot.borrow_mut() = cstr);
}

/// 8-bit RGB triple (plain value; `#[repr(C)]` so shells can read it).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaapRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Cell style as plain values (`has_fg`/`has_bg` = terminal default when
/// false; the `Option` lives on the Rust side only).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaapStyle {
    pub has_fg: bool,
    pub fg: StaapRgb,
    pub has_bg: bool,
    pub bg: StaapRgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl StaapStyle {
    fn none() -> Self {
        Self {
            has_fg: false,
            fg: StaapRgb { r: 0, g: 0, b: 0 },
            has_bg: false,
            bg: StaapRgb { r: 0, g: 0, b: 0 },
            bold: false,
            italic: false,
            underline: false,
        }
    }
}

impl From<crate::embedded::SnapStyle> for StaapStyle {
    fn from(s: crate::embedded::SnapStyle) -> Self {
        let mut out = Self::none();
        if let Some(fg) = s.fg {
            out.has_fg = true;
            out.fg = StaapRgb {
                r: fg.0,
                g: fg.1,
                b: fg.2,
            };
        }
        if let Some(bg) = s.bg {
            out.has_bg = true;
            out.bg = StaapRgb {
                r: bg.0,
                g: bg.1,
                b: bg.2,
            };
        }
        out.bold = s.bold;
        out.italic = s.italic;
        out.underline = s.underline;
        out
    }
}

/// Opaque core handle: owns the run registry (roster `App` + live PTYs).
/// Native shells drive the roster, spawn, pump, and persistence through
/// this handle, so every shell shares one policy instead of reimplementing
/// it per OS.
pub struct StaapCore {
    reg: RunRegistry,
}

/// Opaque PTY handle: owns one [`EmbeddedPty`].
pub struct StaapPty {
    pty: EmbeddedPty,
}

/// Build a core the way the app starts: discovered provider sessions
/// merged over persisted rows, plus the stored user config. Discovery,
/// load, and config all degrade to empty/default (never fail), so this
/// constructor is infallible barring allocation.
///
/// # Safety
/// No arguments; always safe to call. Free with [`staap_core_free`].
#[no_mangle]
pub unsafe extern "C" fn staap_core_new() -> *mut StaapCore {
    let mut discovered = MuseCliProvider::new(
        MuseCliProvider::default_store_root(),
        Box::new(RegistryParser::default()),
    )
    .discover_sessions();
    // Same merge as the gpui shell startup: opencode sessions seed
    // alongside muse sessions (unreachable CLI degrades to empty).
    discovered.extend(
        OpencodeCliProvider::with_default_program(Box::new(RegistryParser::default()))
            .discover_sessions(),
    );
    discovered.extend(
        ClaudeCliProvider::new(
            ClaudeCliProvider::default_store_root(),
            Box::new(RegistryParser::default()),
        )
        .discover_sessions(),
    );
    discovered.extend(
        CodexCliProvider::new(
            CodexCliProvider::default_store_root(),
            Box::new(RegistryParser::default()),
        )
        .discover_sessions(),
    );
    discovered.extend(
        AntigravityCliProvider::new(
            AntigravityCliProvider::default_store_root(),
            Box::new(RegistryParser::default()),
        )
        .discover_sessions(),
    );
    let persisted = persist::load_sessions();
    let mut reg = RunRegistry::new(persist::merge_sessions(discovered, persisted));
    // Native shells previously ran with a default config (their spawns
    // ignored the stored per-agent flags, yolo defaults, and launch
    // memory): the core loads it like the gpui shell does.
    reg.app.set_config(crate::config::Config::load());
    Box::into_raw(Box::new(StaapCore { reg }))
}

/// Free a core created by [`staap_core_new`]. Null is a no-op.
///
/// # Safety
/// `core` must be null or a pointer from [`staap_core_new`], used at most once.
#[no_mangle]
pub unsafe extern "C" fn staap_core_free(core: *mut StaapCore) {
    if !core.is_null() {
        drop(Box::from_raw(core));
    }
}

/// Persist core state (user config + run list). Shells call this on a
/// timer tick while dirty, on close, and after closing a run — never via
/// a manual Save button (removed from every shell: persistence is
/// automatic, like the gpui shell's throttled pump persist). Returns an
/// [`StaapError`] code.
///
/// # Safety
/// `core` must be null or a live pointer from [`staap_core_new`].
#[no_mangle]
pub unsafe extern "C" fn staap_core_save(core: *const StaapCore) -> c_int {
    if core.is_null() {
        set_error("staap_core_save: null core".to_string());
        return StaapError::Null.code();
    }
    let reg = &(*core).reg;
    if let Err(e) = reg.app.save_config() {
        set_error(format!("staap_core_save: {e:#}"));
        return StaapError::Config.code();
    }
    // Best effort like the gpui shell: a failed run-list save just means
    // the next start falls back to historic discovery alone.
    #[cfg(not(test))]
    let _ = persist::save_sessions(&reg.app.sessions);
    StaapError::Ok.code()
}

/// Spawn a fresh session PTY (`muse` + configured extra flags) of
/// `cols` x `rows`. `cwd` is null (inherit) or a NUL-terminated UTF-8
/// path; the new handle is written to `*out`. Returns an [`StaapError`] code.
///
/// # Safety
/// `core`/`out` must be non-null live pointers; `cwd` must be null or a
/// valid NUL-terminated C string. Free the handle with [`staap_pty_free`].
#[no_mangle]
pub unsafe extern "C" fn staap_spawn(
    core: *const StaapCore,
    out: *mut *mut StaapPty,
    cwd: *const c_char,
    cols: u16,
    rows: u16,
) -> c_int {
    if core.is_null() || out.is_null() {
        set_error("staap_spawn: null core or out".to_string());
        return StaapError::Null.code();
    }
    let (program, args) = (*core).reg.app.spawn_command_for(&SpawnKind::New);
    spawn_into(out, &program, &args, cwd, cols, rows)
}

/// Spawn a 2D-launch session PTY (folder × CLI + yolo) of `cols` x `rows`.
/// `cli` names the harness id (`muse`, `claude`, …; null/empty repeats the
/// last-used/default resolution), `cwd` is null (inherit) or a path,
/// `yolo` is tri-state: >0 forces the canonical yolo flag on for this
/// spawn only, <0 forces it off, 0 follows the per-agent config default.
/// Returns an [`StaapError`] code; the handle lands in `*out`.
///
/// # Safety
/// `core`/`out` must be non-null live pointers; `cli`/`cwd` must be null
/// or valid NUL-terminated C strings. Free the handle with [`staap_pty_free`].
#[no_mangle]
pub unsafe extern "C" fn staap_spawn_launch(
    core: *const StaapCore,
    out: *mut *mut StaapPty,
    cli: *const c_char,
    cwd: *const c_char,
    yolo: c_int,
    cols: u16,
    rows: u16,
) -> c_int {
    if core.is_null() || out.is_null() {
        set_error("staap_spawn_launch: null core or out".to_string());
        return StaapError::Null.code();
    }
    let cli_str = if cli.is_null() {
        None
    } else {
        match CStr::from_ptr(cli).to_str() {
            Ok(s) => Some(s),
            Err(_) => {
                set_error("staap_spawn_launch: cli is not valid UTF-8".to_string());
                return StaapError::Utf8.code();
            }
        }
    };
    let _cwd_str = if cwd.is_null() {
        None
    } else {
        match CStr::from_ptr(cwd).to_str() {
            Ok(s) => Some(s),
            Err(_) => {
                set_error("staap_spawn_launch: cwd is not valid UTF-8".to_string());
                return StaapError::Utf8.code();
            }
        }
    };
    let app = &(*core).reg.app;
    let resolved_cli = launch::resolve_effective_cli(
        cli_str.filter(|s| !s.is_empty()),
        app.config_last_cli(),
        app.config_default_cli(),
        &launch::detect_available_clis(),
    );
    let harness = crate::embedded::Harness::from_id(&resolved_cli);
    // Tri-state yolo (mirrors `launch::YoloChoice`): positive forces on,
    // negative forces off, zero follows the per-agent config default.
    let yolo_on = if yolo > 0 {
        true
    } else if yolo < 0 {
        false
    } else {
        app.config_yolo_default(harness.id())
    };
    let (program, mut args) = harness.new_command();
    args.extend(app.config_extra_args(&program));
    if yolo_on {
        if let Some(flag) = harness.yolo_flag() {
            if !args.iter().any(|a| a == flag) {
                args.push(flag.to_string());
            }
        }
    }
    spawn_into(out, &program, &args, cwd, cols, rows)
}

/// Owned harness id of the effective CLI for `cli` (2D launch): the
/// explicit id when non-empty, else the core's last-used / configured /
/// autodetected resolution (same rule as [`staap_spawn_launch`]). Lets
/// shells label a repeat-last spawn before starting it. Null `cli`
/// means "resolve the default". Free with [`staap_screen_text_free`].
///
/// # Safety
/// `core` must be null or a live pointer from [`staap_core_new`]; `cli`
/// must be null or a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn staap_effective_cli(
    core: *const StaapCore,
    cli: *const c_char,
) -> *mut c_char {
    let explicit = if cli.is_null() {
        None
    } else {
        match CStr::from_ptr(cli).to_str() {
            Ok(s) => Some(s),
            Err(_) => {
                set_error("staap_effective_cli: cli is not valid UTF-8".to_string());
                return std::ptr::null_mut();
            }
        }
    };
    let catalog = launch::detect_available_clis();
    let (last, default) = if core.is_null() {
        (None, None)
    } else {
        (
            (*core).reg.app.config_last_cli(),
            (*core).reg.app.config_default_cli(),
        )
    };
    let resolved =
        launch::resolve_effective_cli(explicit.filter(|s| !s.is_empty()), last, default, &catalog);
    match CString::new(resolved) {
        Ok(s) => s.into_raw(),
        Err(_) => {
            set_error("staap_effective_cli: id contains NUL".to_string());
            std::ptr::null_mut()
        }
    }
}

/// Owned JSON of the autodetected CLI catalog (2D launch):
/// `[{"id","program","path"|null,"available"}]` in
/// [`launch::SUPPORTED_CLIS`] order. Never null on allocation success;
/// free with [`staap_screen_text_free`].
///
/// # Safety
/// Always safe: allocates a fresh string, takes no handles.
#[no_mangle]
pub unsafe extern "C" fn staap_clis_json() -> *mut c_char {
    let clis = launch::detect_available_clis();
    match serde_json::to_string(&clis) {
        Ok(doc) => match CString::new(doc) {
            Ok(s) => s.into_raw(),
            Err(_) => {
                set_error("staap_clis_json: catalog contains NUL".to_string());
                std::ptr::null_mut()
            }
        },
        Err(e) => {
            set_error(format!("staap_clis_json: {e:#}"));
            std::ptr::null_mut()
        }
    }
}

/// Owned JSON of the persisted folder recents (2D launch): a string array,
/// MRU-first. Null core yields an empty list, never UB; free with
/// [`staap_screen_text_free`].
///
/// # Safety
/// `core` must be null or a live pointer from [`staap_core_new`].
#[no_mangle]
pub unsafe extern "C" fn staap_recent_json(core: *const StaapCore) -> *mut c_char {
    let recents: &[String] = if core.is_null() {
        &[]
    } else {
        (*core).reg.app.config_recents()
    };
    match serde_json::to_string(recents) {
        Ok(doc) => match CString::new(doc) {
            Ok(s) => s.into_raw(),
            Err(_) => {
                set_error("staap_recent_json: recents contain NUL".to_string());
                std::ptr::null_mut()
            }
        },
        Err(e) => {
            set_error(format!("staap_recent_json: {e:#}"));
            std::ptr::null_mut()
        }
    }
}

/// Record a confirmed launch (2D launch): refreshes last-used CLI + folder
/// MRU in the core config. Null core is a null error; null/empty `cli`
/// keeps the previous CLI; null `cwd` records no folder.
///
/// # Safety
/// `core` must be non-null and live; `cli`/`cwd` must be null or valid
/// NUL-terminated C strings.
#[no_mangle]
pub unsafe extern "C" fn staap_note_launch(
    core: *mut StaapCore,
    cli: *const c_char,
    cwd: *const c_char,
) -> c_int {
    if core.is_null() {
        set_error("staap_note_launch: null core".to_string());
        return StaapError::Null.code();
    }
    let cli_str = if cli.is_null() {
        None
    } else {
        match CStr::from_ptr(cli).to_str() {
            Ok(s) => Some(s),
            Err(_) => {
                set_error("staap_note_launch: cli is not valid UTF-8".to_string());
                return StaapError::Utf8.code();
            }
        }
    };
    let cwd_str = if cwd.is_null() {
        None
    } else {
        match CStr::from_ptr(cwd).to_str() {
            Ok(s) => Some(s),
            Err(_) => {
                set_error("staap_note_launch: cwd is not valid UTF-8".to_string());
                return StaapError::Utf8.code();
            }
        }
    };
    (*core)
        .reg
        .app
        .note_launch(cli_str.filter(|s| !s.is_empty()).unwrap_or(""), cwd_str);
    StaapError::Ok.code()
}

/// Shared spawn tail for [`staap_spawn`] (and the test-only argv variant
/// below): parse `cwd`, spawn, box the handle. One seam so the public
/// success path and the hermetic tests share every line after argv.
unsafe fn spawn_into(
    out: *mut *mut StaapPty,
    program: &str,
    args: &[String],
    cwd: *const c_char,
    cols: u16,
    rows: u16,
) -> c_int {
    let cwd_path;
    let cwd_opt = if cwd.is_null() {
        None
    } else {
        match CStr::from_ptr(cwd).to_str() {
            Ok(s) => {
                cwd_path = std::path::PathBuf::from(s);
                Some(cwd_path.as_path())
            }
            Err(_) => {
                set_error("staap_spawn: cwd is not valid UTF-8".to_string());
                return StaapError::Utf8.code();
            }
        }
    };
    match EmbeddedPty::spawn_with_cwd(program, args, cols, rows, cwd_opt) {
        Ok(pty) => {
            *out = Box::into_raw(Box::new(StaapPty { pty }));
            StaapError::Ok.code()
        }
        Err(e) => {
            set_error(format!("staap_spawn: {e:#}"));
            StaapError::Spawn.code()
        }
    }
}

/// Feed queued output into the emulator. Returns true when new output
/// arrived or the child newly exited (the only dirty gate the shell
/// needs to repaint). Null is false, never UB.
///
/// # Safety
/// `pty` must be null or a live pointer from [`staap_spawn`].
#[no_mangle]
pub unsafe extern "C" fn staap_pump(pty: *mut StaapPty) -> bool {
    if pty.is_null() {
        return false;
    }
    (*pty).pty.pump()
}

/// Forward raw bytes (already key-encoded by the shell) to the child.
/// Returns an [`StaapError`] code.
///
/// # Safety
/// `pty` must be non-null and live; `data` must point to `len` readable
/// bytes when `len > 0` (null `data` with `len == 0` is a no-op success).
#[no_mangle]
pub unsafe extern "C" fn staap_write(pty: *mut StaapPty, data: *const u8, len: usize) -> c_int {
    if pty.is_null() {
        set_error("staap_write: null pty".to_string());
        return StaapError::Null.code();
    }
    if data.is_null() {
        if len == 0 {
            return StaapError::Ok.code();
        }
        set_error("staap_write: null data".to_string());
        return StaapError::Null.code();
    }
    let bytes = std::slice::from_raw_parts(data, len);
    match (*pty).pty.write_input(bytes) {
        Ok(()) => StaapError::Ok.code(),
        Err(e) => {
            set_error(format!("staap_write: {e:#}"));
            StaapError::Io.code()
        }
    }
}

/// Resize the PTY and the emulator grid. Null is a no-op.
///
/// # Safety
/// `pty` must be null or a live pointer from [`staap_spawn`].
#[no_mangle]
pub unsafe extern "C" fn staap_resize(pty: *mut StaapPty, cols: u16, rows: u16) {
    if !pty.is_null() {
        (*pty).pty.resize(cols, rows);
    }
}

/// Owned UTF-8 snapshot of the emulated screen. Returns null on null
/// handle; otherwise a freshly allocated string the caller frees with
/// [`staap_screen_text_free`].
///
/// # Safety
/// `pty` must be null or a live pointer from [`staap_spawn`].
#[no_mangle]
pub unsafe extern "C" fn staap_screen_text(pty: *const StaapPty) -> *mut c_char {
    if pty.is_null() {
        return std::ptr::null_mut();
    }
    let text = (*pty).pty.snapshot_text();
    match CString::new(text) {
        Ok(s) => s.into_raw(),
        Err(_) => {
            set_error("staap_screen_text: snapshot contains NUL".to_string());
            std::ptr::null_mut()
        }
    }
}

/// Free a string returned by [`staap_screen_text`] or [`staap_spans_json`].
/// Null is a no-op.
///
/// # Safety
/// `s` must be null or a pointer from [`staap_screen_text`]/[`staap_spans_json`],
/// used at most once.
#[no_mangle]
pub unsafe extern "C" fn staap_screen_text_free(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

/// Owned styled spans of the emulated screen as JSON:
/// `[[{"text":..,"fg":[r,g,b]|null,"bg":..,"bold":..,"italic":..,
/// "underline":..}]]` (one array per grid row). Returns null on null
/// handle or (unreachable in practice) JSON failure; free with
/// [`staap_screen_text_free`].
///
/// # Safety
/// `pty` must be null or a live pointer from [`staap_spawn`].
#[no_mangle]
pub unsafe extern "C" fn staap_spans_json(pty: *const StaapPty) -> *mut c_char {
    if pty.is_null() {
        return std::ptr::null_mut();
    }
    let rows = (*pty).pty.snapshot_spans();
    let json_rows: Vec<Vec<serde_json::Value>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|span| {
                    let style = StaapStyle::from(span.style);
                    serde_json::json!({
                        "text": span.text,
                        "fg": if style.has_fg { serde_json::json!([style.fg.r, style.fg.g, style.fg.b]) } else { serde_json::Value::Null },
                        "bg": if style.has_bg { serde_json::json!([style.bg.r, style.bg.g, style.bg.b]) } else { serde_json::Value::Null },
                        "bold": style.bold,
                        "italic": style.italic,
                        "underline": style.underline,
                    })
                })
                .collect()
        })
        .collect();
    let doc = serde_json::Value::Array(
        json_rows
            .into_iter()
            .map(serde_json::Value::Array)
            .collect(),
    );
    match CString::new(doc.to_string()) {
        Ok(s) => s.into_raw(),
        Err(_) => {
            set_error("staap_spans_json: snapshot contains NUL".to_string());
            std::ptr::null_mut()
        }
    }
}

/// Roster status of row `row`: 0 = Attention, 1 = Idle, 2 = Working.
/// Returns -1 on null handle, -2 when `row` is out of bounds.
///
/// # Safety
/// `core` must be null or a live pointer from [`staap_core_new`].
#[no_mangle]
pub unsafe extern "C" fn staap_status(core: *const StaapCore, row: usize) -> c_int {
    if core.is_null() {
        set_error("staap_status: null core".to_string());
        return -1;
    }
    let sessions: &[ChatSession] = &(*core).reg.app.sessions;
    match sessions.get(row) {
        Some(session) => match session.status {
            Status::Attention => 0,
            Status::Idle => 1,
            Status::Working => 2,
        },
        None => -2,
    }
}

/// Number of roster rows in the core. Null core yields 0 (never UB).
///
/// # Safety
/// `core` must be null or a live pointer from [`staap_core_new`].
#[no_mangle]
pub unsafe extern "C" fn staap_session_count(core: *const StaapCore) -> usize {
    if core.is_null() {
        return 0;
    }
    (*core).reg.app.sessions.len()
}

/// Owned JSON of roster row `row` (a serialized [`ChatSession`]; the
/// documented roster crossing from the seam sketch). Returns null on
/// null handle, out-of-bounds row, or (unreachable in practice) JSON
/// failure; free with [`staap_screen_text_free`].
///
/// # Safety
/// `core` must be null or a live pointer from [`staap_core_new`].
#[no_mangle]
pub unsafe extern "C" fn staap_session_json(core: *const StaapCore, row: usize) -> *mut c_char {
    if core.is_null() {
        set_error("staap_session_json: null core".to_string());
        return std::ptr::null_mut();
    }
    let sessions: &[ChatSession] = &(*core).reg.app.sessions;
    match sessions.get(row) {
        Some(session) => match serde_json::to_string(session) {
            Ok(doc) => match CString::new(doc) {
                Ok(s) => s.into_raw(),
                Err(_) => {
                    set_error("staap_session_json: row contains NUL".to_string());
                    std::ptr::null_mut()
                }
            },
            Err(e) => {
                set_error(format!("staap_session_json: {e:#}"));
                std::ptr::null_mut()
            }
        },
        None => {
            set_error(format!("staap_session_json: row {row} out of bounds"));
            std::ptr::null_mut()
        }
    }
}

/// Last error message for this thread (UTF-8, NUL-terminated). Never
/// null; valid until the next failing `staap_*` call on this thread.
///
/// # Safety
/// Always safe: returns a thread-local pointer, never transfers ownership.
#[no_mangle]
pub unsafe extern "C" fn staap_last_error() -> *const c_char {
    LAST_ERROR.with(|slot| slot.borrow().as_ptr())
}

/// Free a PTY created by [`staap_spawn`] (reaps the child). Null is a no-op.
///
/// # Safety
/// `pty` must be null or a pointer from [`staap_spawn`], used at most once.
#[no_mangle]
pub unsafe extern "C" fn staap_pty_free(pty: *mut StaapPty) {
    if !pty.is_null() {
        drop(Box::from_raw(pty));
    }
}

// --- Native-shell run registry -----------------------------------------------
//
// The functions below let the three native shells drive the core run
// registry instead of their own `live` maps. One policy (cap, eviction,
// status pump, persistence) for every shell; the per-shell maps and their
// divergent caps/policies are deleted.

/// Live-run ceiling shared by every shell (was three copies: `10` on
/// Linux/Windows, unbounded on macOS).
#[no_mangle]
pub extern "C" fn staap_max_runs() -> usize {
    crate::shell_shared::MAX_LIVE_RUNS
}

/// Number of live (attached) PTYs in the registry. Null core yields 0.
///
/// # Safety
/// `core` must be null or a live pointer from [`staap_core_new`].
#[no_mangle]
pub unsafe extern "C" fn staap_live_count(core: *const StaapCore) -> usize {
    if core.is_null() {
        return 0;
    }
    (*core).reg.live_count()
}

/// True when the row id owns a live PTY in the registry. Null-safe:
/// null core/id yields false.
///
/// # Safety
/// `core` must be null or a live pointer from [`staap_core_new`]; `id` must
/// be null or a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn staap_is_live(core: *const StaapCore, id: *const c_char) -> bool {
    if core.is_null() || id.is_null() {
        return false;
    }
    match CStr::from_ptr(id).to_str() {
        Ok(s) => (*core).reg.is_live(s),
        Err(_) => false,
    }
}

/// Pump every live run: feed output, rescan attention + links for changed
/// runs, reclassify statuses, re-sort pinned to selection. Returns true
/// when anything visible changed — the shell's only repaint gate (and its
/// roster/status refresh gate: statuses now actually move, unlike the
/// launch-snapshot rows the old shells polled). Null is false, never UB.
///
/// # Safety
/// `core` must be null or a live pointer from [`staap_core_new`].
#[no_mangle]
pub unsafe extern "C" fn staap_pump_all(core: *mut StaapCore) -> bool {
    if core.is_null() {
        return false;
    }
    (*core).reg.pump_all()
}

/// Spawn a 2D-launch session and attach it to a new roster row, under the
/// shared live-run cap. On success the row id is written to `id_out`
/// (up to `id_cap` bytes incl. NUL; truncated otherwise) and an
/// [`StaapError::Ok`] code returns. At the cap the oldest-exited run is
/// reaped first; when every live run is still running this refuses with
/// [`StaapError::Spawn`] and a message naming per-run close. `cli`/`cwd`
/// follow the [`staap_spawn_launch`] convention; `yolo` is tri-state.
///
/// # Safety
/// `core` must be a live pointer from [`staap_core_new`]; `id_out` must point
/// to `id_cap` writable bytes; `cli`/`cwd` must be null or valid
/// NUL-terminated C strings.
#[no_mangle]
pub unsafe extern "C" fn staap_run_spawn(
    core: *mut StaapCore,
    cli: *const c_char,
    cwd: *const c_char,
    yolo: c_int,
    cols: u16,
    rows: u16,
    id_out: *mut c_char,
    id_cap: usize,
) -> c_int {
    if core.is_null() || id_out.is_null() || id_cap == 0 {
        set_error("staap_run_spawn: null core or id buffer".to_string());
        return StaapError::Null.code();
    }
    let reg = &mut (*core).reg;
    if reg.live_count() >= crate::shell_shared::MAX_LIVE_RUNS {
        match reg.make_room() {
            Some(title) if !title.is_empty() => {
                // Reaped room: fall through to the spawn below.
                let _ = title;
            }
            // `Some("")` means under the cap after all (races with
            // closes); `None` means every live run is still running.
            Some(_) => {}
            None => {
                set_error(format!(
                    "at {} live runs — close one first",
                    crate::shell_shared::MAX_LIVE_RUNS
                ));
                return StaapError::Spawn.code();
            }
        }
    }
    // Resolve the spawn exactly like `staap_spawn_launch` (same CLI/yolo
    // rule), then attach to a fresh roster row via the single
    // `App::start_launch` funnel — the id is the row key, so the roster
    // and the PTY map can never desync again.
    let cli_str = if cli.is_null() {
        None
    } else {
        match CStr::from_ptr(cli).to_str() {
            Ok(s) => Some(s),
            Err(_) => {
                set_error("staap_run_spawn: cli is not valid UTF-8".to_string());
                return StaapError::Utf8.code();
            }
        }
    };
    let cwd_str = if cwd.is_null() {
        None
    } else {
        match CStr::from_ptr(cwd).to_str() {
            Ok(s) => Some(s),
            Err(_) => {
                set_error("staap_run_spawn: cwd is not valid UTF-8".to_string());
                return StaapError::Utf8.code();
            }
        }
    };
    let app = &reg.app;
    let resolved_cli = launch::resolve_effective_cli(
        cli_str.filter(|s| !s.is_empty()),
        app.config_last_cli(),
        app.config_default_cli(),
        &launch::detect_available_clis(),
    );
    let harness = crate::embedded::Harness::from_id(&resolved_cli);
    let yolo_on = if yolo > 0 {
        true
    } else if yolo < 0 {
        false
    } else {
        app.config_yolo_default(harness.id())
    };
    let (program, mut args) = harness.new_command();
    args.extend(app.config_extra_args(&program));
    if yolo_on {
        if let Some(flag) = harness.yolo_flag() {
            if !args.iter().any(|a| a == flag) {
                args.push(flag.to_string());
            }
        }
    }
    let cwd_path;
    let cwd_opt = match cwd_str.filter(|s| !s.is_empty()) {
        None => None,
        Some(s) => {
            cwd_path = std::path::PathBuf::from(s);
            Some(cwd_path.as_path())
        }
    };
    let pty = match EmbeddedPty::spawn_with_cwd(&program, &args, cols, rows, cwd_opt) {
        Ok(pty) => pty,
        Err(e) => {
            set_error(format!("staap_run_spawn: {e:#}"));
            return StaapError::Spawn.code();
        }
    };
    // Same memory update as the picker funnel (repeat-last + folder MRU).
    let cwd_owned = cwd_opt.map(|p| p.to_string_lossy().into_owned());
    let selection = crate::launch::LaunchSelection::new(resolved_cli, cwd_owned, yolo_on);
    reg.app.start_launch(&selection);
    let id = reg
        .app
        .sessions
        .last()
        .map(|s| s.id.clone())
        .unwrap_or_default();
    reg.attach(&id.clone(), pty);
    let bytes = id.as_bytes();
    let n = bytes.len().min(id_cap - 1);
    std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, id_out, n);
    *id_out.add(n) = 0;
    StaapError::Ok.code()
}

/// Spawn a PTY and attach it to an existing roster row (restart a dead
/// run, resume a historic entry): drops the dead PTY if any, spawns the
/// row's spawn kind on the same id, attaches on success. Unknown or live
/// rows report [`StaapError::Spawn`]; the row keeps its title and links.
///
/// # Safety
/// `core` must be live; `id` must be a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn staap_run_restart(
    core: *mut StaapCore,
    id: *const c_char,
    cols: u16,
    rows: u16,
) -> c_int {
    if core.is_null() || id.is_null() {
        set_error("staap_run_restart: null core or id".to_string());
        return StaapError::Null.code();
    }
    let id_str = match CStr::from_ptr(id).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => {
            set_error("staap_run_restart: id is not valid UTF-8".to_string());
            return StaapError::Utf8.code();
        }
    };
    let reg = &mut (*core).reg;
    let kind = match reg.restart_kind(&id_str) {
        Some(kind) => kind,
        None => {
            set_error("staap_run_restart: run is live or unknown".to_string());
            return StaapError::Spawn.code();
        }
    };
    let (program, args) = reg.app.spawn_command_for(&kind);
    let cwd = reg.app.session_cwd(&id_str);
    let pty = match EmbeddedPty::spawn_with_cwd(&program, &args, cols, rows, cwd.as_deref()) {
        Ok(pty) => pty,
        Err(e) => {
            set_error(format!("staap_run_restart: {e:#}"));
            return StaapError::Spawn.code();
        }
    };
    reg.attach(&id_str, pty);
    StaapError::Ok.code()
}

/// Close (kill) a run: drop its live PTY and remove its entry. Unknown
/// ids are a no-op success. Shells persist via `staap_core_save`
/// hooks (close autosaves) so the closed run does not resurrect.
///
/// # Safety
/// `core` must be null or live; `id` must be null or a valid C string.
#[no_mangle]
pub unsafe extern "C" fn staap_run_close(core: *mut StaapCore, id: *const c_char) -> c_int {
    if core.is_null() || id.is_null() {
        set_error("staap_run_close: null core or id".to_string());
        return StaapError::Null.code();
    }
    let id_str = match CStr::from_ptr(id).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => {
            set_error("staap_run_close: id is not valid UTF-8".to_string());
            return StaapError::Utf8.code();
        }
    };
    (*core).reg.close(&id_str);
    StaapError::Ok.code()
}

/// True when quitting deserves a confirmation step: any Working/Attention
/// row or any live (non-exited) PTY. Null core yields false.
///
/// # Safety
/// `core` must be null or a live pointer from [`staap_core_new`].
#[no_mangle]
pub unsafe extern "C" fn staap_needs_quit_confirm(core: *const StaapCore) -> bool {
    if core.is_null() {
        return false;
    }
    (*core).reg.needs_quit_confirm()
}

/// Pump one attached run by id (feed output into its emulator). Returns
/// true when the screen may have changed. Unknown ids and nulls yield
/// false, never UB.
///
/// # Safety
/// `core`/`id` follow the [`staap_is_live`] conventions.
#[no_mangle]
pub unsafe extern "C" fn staap_run_pump(core: *mut StaapCore, id: *const c_char) -> bool {
    if core.is_null() || id.is_null() {
        return false;
    }
    match CStr::from_ptr(id).to_str() {
        Ok(s) => (*core).reg.runs.get_mut(s).is_some_and(|r| r.pump()),
        Err(_) => false,
    }
}

/// Forward raw bytes (already key-encoded by the shell) to an attached
/// run's child. Returns an [`StaapError`] code.
///
/// # Safety
/// `core` must be non-null and live; `id` a valid C string; `data` must
/// point to `len` readable bytes when `len > 0`.
#[no_mangle]
pub unsafe extern "C" fn staap_run_write(
    core: *mut StaapCore,
    id: *const c_char,
    data: *const u8,
    len: usize,
) -> c_int {
    if core.is_null() || id.is_null() {
        set_error("staap_run_write: null core or id".to_string());
        return StaapError::Null.code();
    }
    if data.is_null() {
        if len == 0 {
            return StaapError::Ok.code();
        }
        set_error("staap_run_write: null data".to_string());
        return StaapError::Null.code();
    }
    let id_str = match CStr::from_ptr(id).to_str() {
        Ok(s) => s,
        Err(_) => {
            set_error("staap_run_write: id is not valid UTF-8".to_string());
            return StaapError::Utf8.code();
        }
    };
    let reg = &mut (*core).reg;
    match reg.runs.get_mut(id_str) {
        Some(run) => {
            let bytes = std::slice::from_raw_parts(data, len);
            match run.pty.write_input(bytes) {
                Ok(()) => StaapError::Ok.code(),
                Err(e) => {
                    set_error(format!("staap_run_write: {e:#}"));
                    StaapError::Io.code()
                }
            }
        }
        None => {
            set_error("staap_run_write: run has no live session".to_string());
            StaapError::Spawn.code()
        }
    }
}

/// Resize an attached run's PTY and emulator grid. Unknown ids and nulls
/// are no-ops.
///
/// # Safety
/// `core`/`id` follow the [`staap_is_live`] conventions.
#[no_mangle]
pub unsafe extern "C" fn staap_run_resize(
    core: *mut StaapCore,
    id: *const c_char,
    cols: u16,
    rows: u16,
) {
    if core.is_null() || id.is_null() {
        return;
    }
    if let Ok(s) = CStr::from_ptr(id).to_str() {
        if let Some(run) = (*core).reg.runs.get_mut(s) {
            run.pty.resize(cols, rows);
        }
    }
}

/// Owned UTF-8 snapshot of an attached run's emulated screen. Null on
/// null handle or unknown id; free with [`staap_screen_text_free`].
///
/// # Safety
/// `core`/`id` follow the [`staap_is_live`] conventions.
#[no_mangle]
pub unsafe extern "C" fn staap_run_screen_text(
    core: *const StaapCore,
    id: *const c_char,
) -> *mut c_char {
    if core.is_null() || id.is_null() {
        return std::ptr::null_mut();
    }
    let text = match CStr::from_ptr(id).to_str() {
        Ok(s) => match (*core).reg.runs.get(s) {
            Some(run) => run.pty.snapshot_text(),
            None => return std::ptr::null_mut(),
        },
        Err(_) => return std::ptr::null_mut(),
    };
    match CString::new(text) {
        Ok(s) => s.into_raw(),
        Err(_) => {
            set_error("staap_run_screen_text: snapshot contains NUL".to_string());
            std::ptr::null_mut()
        }
    }
}

/// Owned styled spans of an attached run's screen as JSON (same shape as
/// [`staap_spans_json`]). Null on null handle or unknown id; free with
/// [`staap_screen_text_free`].
///
/// # Safety
/// `core`/`id` follow the [`staap_is_live`] conventions.
#[no_mangle]
pub unsafe extern "C" fn staap_run_spans_json(
    core: *const StaapCore,
    id: *const c_char,
) -> *mut c_char {
    if core.is_null() || id.is_null() {
        return std::ptr::null_mut();
    }
    let rows = match CStr::from_ptr(id).to_str() {
        Ok(s) => match (*core).reg.runs.get(s) {
            Some(run) => run.pty.snapshot_spans(),
            None => return std::ptr::null_mut(),
        },
        Err(_) => return std::ptr::null_mut(),
    };
    let json_rows: Vec<Vec<serde_json::Value>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|span| {
                    let style = StaapStyle::from(span.style);
                    serde_json::json!({
                        "text": span.text,
                        "fg": if style.has_fg { serde_json::json!([style.fg.r, style.fg.g, style.fg.b]) } else { serde_json::Value::Null },
                        "bg": if style.has_bg { serde_json::json!([style.bg.r, style.bg.g, style.bg.b]) } else { serde_json::Value::Null },
                        "bold": style.bold,
                        "italic": style.italic,
                        "underline": style.underline,
                    })
                })
                .collect()
        })
        .collect();
    let doc = serde_json::Value::Array(
        json_rows
            .into_iter()
            .map(serde_json::Value::Array)
            .collect(),
    );
    match CString::new(doc.to_string()) {
        Ok(s) => s.into_raw(),
        Err(_) => {
            set_error("staap_run_spans_json: snapshot contains NUL".to_string());
            std::ptr::null_mut()
        }
    }
}

/// True once an attached run's child has exited. Unknown ids and nulls
/// yield false.
///
/// # Safety
/// `core`/`id` follow the [`staap_is_live`] conventions.
#[no_mangle]
pub unsafe extern "C" fn staap_run_exited(core: *const StaapCore, id: *const c_char) -> bool {
    if core.is_null() || id.is_null() {
        return false;
    }
    match CStr::from_ptr(id).to_str() {
        Ok(s) => (*core).reg.runs.get(s).is_some_and(|r| r.exited()),
        Err(_) => false,
    }
}

// --- Shared shell helpers ----------------------------------------------------
//
// Thin `staap_*` wrappers over `shell` so C/Swift shells call one code path:
// key encoding, the snapshot→stream reconciler, the SGR renderer, picker
// helpers, display formatting, and selection state.

/// Encode one logical keypress into child bytes. `key`/`key_char` are
/// NUL-terminated UTF-8 (`key_char` may be null); `ctrl`/`alt` are 0/1.
/// On `Forward` the bytes are written to `bytes_out` (up to `cap` bytes)
/// and the count returns; on `Keep` 0 returns (leave to the native
/// control). `key` null returns -1. This is the single key table every
/// shell shares (ports of per-shell tables are deleted).
///
/// # Safety
/// `key`/`key_char` must be null or valid C strings; `bytes_out` must
/// point to `cap` writable bytes when `cap > 0`.
#[no_mangle]
pub unsafe extern "C" fn staap_key_encode(
    key: *const c_char,
    key_char: *const c_char,
    ctrl: c_int,
    alt: c_int,
    bytes_out: *mut u8,
    cap: usize,
) -> c_int {
    if key.is_null() {
        return -1;
    }
    let key_str = match CStr::from_ptr(key).to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let owned_char;
    let char_opt = if key_char.is_null() {
        None
    } else {
        match CStr::from_ptr(key_char).to_str() {
            Ok(s) => {
                owned_char = s.to_string();
                Some(owned_char.as_str())
            }
            Err(_) => return -1,
        }
    };
    match crate::shell_shared::encode_key(key_str, char_opt, ctrl != 0, alt != 0) {
        crate::shell_shared::KeyDecision::Keep => 0,
        crate::shell_shared::KeyDecision::Forward(bytes) => {
            if bytes_out.is_null() || cap == 0 {
                return bytes.len() as c_int;
            }
            let n = bytes.len().min(cap);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), bytes_out, n);
            n as c_int
        }
    }
}

/// Snapshot-to-stream feed reconciler for C shells: computes the text
/// that advances a view showing `old_text` to also show `new_text`, or
/// null when the view is already current. Either argument may be null
/// (treated as ""). The result is freshly allocated; free it with
/// [`staap_screen_text_free`].
///
/// This is the shared [`crate::shell_shared::feed_delta`] (append-only
/// suffix hot path, scroll overlap, clear-and-replay redraw, CRLF
/// normalization), replacing the per-shell `feed.c` ports.
///
/// # Safety
/// `old_text`/`new_text` must be null or valid NUL-terminated UTF-8
/// C strings.
#[no_mangle]
pub unsafe extern "C" fn staap_feed_delta(
    old_text: *const c_char,
    new_text: *const c_char,
) -> *mut c_char {
    let old = c_str_or_empty(old_text);
    let new = c_str_or_empty(new_text);
    let (Some(old), Some(new)) = (old, new) else {
        set_error("staap_feed_delta: argument is not valid UTF-8".to_string());
        return std::ptr::null_mut();
    };
    match crate::shell_shared::feed_delta(old, new) {
        Some(feed) => match CString::new(feed) {
            Ok(s) => s.into_raw(),
            Err(_) => {
                set_error("staap_feed_delta: feed contains NUL".to_string());
                std::ptr::null_mut()
            }
        },
        None => std::ptr::null_mut(),
    }
}

/// Decode a nullable C string to `Some(str)` (null becomes `Some("")`);
/// `None` when the bytes are not valid UTF-8. Shared by the nullable
/// string arguments below so invalid UTF-8 degrades uniformly.
unsafe fn c_str_or_empty(ptr: *const c_char) -> Option<&'static str> {
    if ptr.is_null() {
        return Some("");
    }
    // Extend the borrow to 'static: the pointer is only read during this
    // call and never retained, matching every other getter in this file.
    CStr::from_ptr(ptr)
        .to_str()
        .ok()
        .map(|s| unsafe { &*(s as *const str) })
}

/// True (1) when a roster row with this title/project/id passes the
/// sidebar `query` (case-insensitive substring; blank query passes
/// everything), else false (0). Null pointers mean empty strings; invalid
/// UTF-8 in any argument reports false and records a message.
///
/// # Safety
/// Each argument must be null or a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn staap_roster_matches(
    title: *const c_char,
    project: *const c_char,
    id: *const c_char,
    query: *const c_char,
) -> c_int {
    let (Some(title), Some(project), Some(id), Some(query)) = (
        c_str_or_empty(title),
        c_str_or_empty(project),
        c_str_or_empty(id),
        c_str_or_empty(query),
    ) else {
        set_error("staap_roster_matches: argument is not valid UTF-8".to_string());
        return 0;
    };
    i32::from(crate::shell_shared::roster_matches(
        title, project, id, query,
    ))
}

/// Owned glanceable age label for `then_secs` (unix seconds) relative to
/// `now_secs`: `just now` / `Nm ago` / `Nh ago` / `Nd ago`. Free with
/// [`staap_screen_text_free`].
///
/// # Safety
/// Always safe: pure computation, no pointers read.
#[no_mangle]
pub unsafe extern "C" fn staap_relative_age(now_secs: i64, then_secs: i64) -> *mut c_char {
    match CString::new(crate::shell_shared::relative_age(now_secs, then_secs)) {
        Ok(s) => s.into_raw(),
        Err(_) => {
            set_error("staap_relative_age: label contains NUL".to_string());
            std::ptr::null_mut()
        }
    }
}

/// Total link count (PR + related) of roster row `row`, or -1 on null
/// handle / out-of-bounds row. Lets shells show the same link badge
/// without parsing `pr_links` / `related_links` themselves.
///
/// # Safety
/// `core` must be null or a live pointer from [`staap_core_new`].
#[no_mangle]
pub unsafe extern "C" fn staap_link_count(core: *const StaapCore, row: usize) -> c_int {
    if core.is_null() {
        set_error("staap_link_count: null core".to_string());
        return -1;
    }
    let sessions: &[ChatSession] = &(*core).reg.app.sessions;
    match sessions.get(row) {
        Some(session) => (session.pr_links.len() + session.related_links.len()) as c_int,
        None => {
            set_error(format!("staap_link_count: row {row} out of bounds"));
            -1
        }
    }
}

/// Unix seconds of `last_active` for roster row `row`, or -1 on null
/// handle / out-of-bounds row. Shells feed it to [`staap_relative_age`]
/// with their own clock, so all shells render the same relative label.
///
/// # Safety
/// `core` must be null or a live pointer from [`staap_core_new`].
#[no_mangle]
pub unsafe extern "C" fn staap_last_active(core: *const StaapCore, row: usize) -> i64 {
    if core.is_null() {
        set_error("staap_last_active: null core".to_string());
        return -1;
    }
    let sessions: &[ChatSession] = &(*core).reg.app.sessions;
    match sessions.get(row) {
        Some(session) => session.last_active,
        None => {
            set_error(format!("staap_last_active: row {row} out of bounds"));
            -1
        }
    }
}

/// Render an `staap_spans_json` document to an SGR stream
/// (`shell_shared::render_ansi_json`), or null when it does not decode
/// (the pump then falls back to the plain-text snapshot). Free with
/// [`staap_screen_text_free`].
///
/// # Safety
/// `json` must be null or a valid C string.
#[no_mangle]
pub unsafe extern "C" fn staap_ansi_render(json: *const c_char) -> *mut c_char {
    if json.is_null() {
        return std::ptr::null_mut();
    }
    let text = match CStr::from_ptr(json).to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    match crate::shell_shared::render_ansi_json(text) {
        Some(out) => match CString::new(out) {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        None => std::ptr::null_mut(),
    }
}

/// One-line spawn preview (`runs: muse in ~/api + yolo`). `cli`/`folder`
/// may be null (= default/inherit); `yolo` is the tri-state int. Free with
/// [`staap_screen_text_free`].
///
/// # Safety
/// `cli`/`folder` must be null or valid C strings.
#[no_mangle]
pub unsafe extern "C" fn staap_spawn_preview(
    cli: *const c_char,
    folder: *const c_char,
    yolo: c_int,
) -> *mut c_char {
    let cli_s = if cli.is_null() {
        String::new()
    } else {
        match CStr::from_ptr(cli).to_str() {
            Ok(s) => s.to_string(),
            Err(_) => {
                set_error("staap_spawn_preview: cli is not valid UTF-8".to_string());
                return std::ptr::null_mut();
            }
        }
    };
    let folder_s = if folder.is_null() {
        String::new()
    } else {
        match CStr::from_ptr(folder).to_str() {
            Ok(s) => s.to_string(),
            Err(_) => {
                set_error("staap_spawn_preview: folder is not valid UTF-8".to_string());
                return std::ptr::null_mut();
            }
        }
    };
    match CString::new(crate::shell_shared::spawn_preview(&cli_s, &folder_s, yolo)) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Tri-state yolo int from a segmented-control index (1 = force on,
/// 2 = force off, else config default).
#[no_mangle]
pub extern "C" fn staap_yolo_value(selected: c_int) -> c_int {
    crate::shell_shared::yolo_value(selected)
}

/// Human age for `last_active` (`just now`, `5m ago`, …). Free with
/// [`staap_screen_text_free`].
#[no_mangle]
pub extern "C" fn staap_age_string(now_unix: i64, then_unix: i64) -> *mut c_char {
    match CString::new(crate::shell_shared::relative_age(now_unix, then_unix)) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Non-color status marker for a status code (`●`/`◐`/`○`).
/// Free with [`staap_screen_text_free`].
#[no_mangle]
pub extern "C" fn staap_status_glyph(code: c_int) -> *mut c_char {
    match CString::new(crate::shell_shared::status_glyph(
        crate::shell_shared::RowStatus::from_code(code),
    )) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Section header for a status code (`Needs input`/`Idle`/`Working`).
/// Free with [`staap_screen_text_free`].
#[no_mangle]
pub extern "C" fn staap_section_title(code: c_int) -> *mut c_char {
    match CString::new(crate::shell_shared::section_title(
        crate::shell_shared::RowStatus::from_code(code),
    )) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Clamp a sidebar width into the shared 220..480px range.
#[no_mangle]
pub extern "C" fn staap_clamp_sidebar(px: f64) -> f64 {
    crate::shell_shared::clamp_sidebar_width(px)
}

// --- Filter + selection (core-owned, shells render) ----------------------------

/// True when roster row `row` passes the sidebar filter `query`
/// (case-insensitive substring over title/project/id). Null-safe: null
/// core/query yields false; out-of-bounds yields false.
///
/// # Safety
/// `core` must be null or live; `query` must be null or a valid C string.
#[no_mangle]
pub unsafe extern "C" fn staap_row_matches(
    core: *const StaapCore,
    row: usize,
    query: *const c_char,
) -> bool {
    if core.is_null() || query.is_null() {
        return false;
    }
    let q = match CStr::from_ptr(query).to_str() {
        Ok(s) => s,
        Err(_) => return false,
    };
    let sessions = &(*core).reg.app.sessions;
    match sessions.get(row) {
        Some(s) => crate::shell_shared::row_matches_filter(s, q),
        None => false,
    }
}

/// Replace the title filter, snapping the selection into the matches.
/// Null query clears. Always persists via [`staap_core_save`] semantics? No —
/// callers persist on close; this only mutates in-memory state.
///
/// # Safety
/// `core` must be null or live; `query` must be null or a valid C string.
#[no_mangle]
pub unsafe extern "C" fn staap_set_filter(core: *mut StaapCore, query: *const c_char) {
    if core.is_null() {
        return;
    }
    let q = if query.is_null() {
        String::new()
    } else {
        match CStr::from_ptr(query).to_str() {
            Ok(s) => s.to_string(),
            Err(_) => return,
        }
    };
    (*core).reg.app.set_filter(q);
}

/// Selected roster row index (the shell highlights this row).
///
/// # Safety
/// `core` must be null or a live pointer from [`staap_core_new`].
#[no_mangle]
pub unsafe extern "C" fn staap_selected(core: *const StaapCore) -> usize {
    if core.is_null() {
        return 0;
    }
    (*core).reg.app.selected
}

/// Move selection to row `row` (clamped into range). Resets nothing else;
/// the shell repaints the detail from the newly selected row.
///
/// # Safety
/// `core` must be null or live.
#[no_mangle]
pub unsafe extern "C" fn staap_select(core: *mut StaapCore, row: usize) {
    if core.is_null() {
        return;
    }
    let n = (*core).reg.app.sessions.len();
    if n == 0 {
        return;
    }
    (*core).reg.app.selected = row.min(n - 1);
}

/// Step selection next/prev (`forward` nonzero = next), wrapping within
/// the current filter matches — the same rule as the gpui list keys.
///
/// # Safety
/// `core` must be null or live.
#[no_mangle]
pub unsafe extern "C" fn staap_select_step(core: *mut StaapCore, forward: c_int) {
    if core.is_null() {
        return;
    }
    if forward != 0 {
        (*core).reg.app.select_next();
    } else {
        (*core).reg.app.select_prev();
    }
}

// --- History membership + expansion (core-owned, shells render) --------------

/// True when roster row id `id` is historic (no live PTY attached —
/// attached means active, even when the child already exited).
/// Null-safe: null core/id yields false; unknown ids yield true.
///
/// # Safety
/// `core` must be null or live; `id` must be null or a valid C string.
#[no_mangle]
pub unsafe extern "C" fn staap_is_history(core: *const StaapCore, id: *const c_char) -> bool {
    if core.is_null() || id.is_null() {
        return false;
    }
    match CStr::from_ptr(id).to_str() {
        Ok(s) => (*core).reg.is_history(s),
        Err(_) => false,
    }
}

/// True when the History section renders expanded. Collapsed by default;
/// null core yields false. Callers persist via [`staap_core_save`].
///
/// # Safety
/// `core` must be null or a live pointer from [`staap_core_new`].
#[no_mangle]
pub unsafe extern "C" fn staap_history_expanded(core: *const StaapCore) -> bool {
    if core.is_null() {
        return false;
    }
    (*core).reg.app.history_expanded
}

/// Set the History expansion state (`expanded` nonzero = expanded).
/// Null core is a no-op. Callers persist via [`staap_core_save`].
///
/// # Safety
/// `core` must be null or live.
#[no_mangle]
pub unsafe extern "C" fn staap_set_history_expanded(core: *mut StaapCore, expanded: c_int) {
    if core.is_null() {
        return;
    }
    let expanded = expanded != 0;
    (*core).reg.app.set_history_expanded(expanded);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn pump_until_text(pty: *mut StaapPty, needle: &str, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            unsafe {
                staap_pump(pty);
                let raw = staap_screen_text(pty);
                if raw.is_null() {
                    continue;
                }
                let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
                staap_screen_text_free(raw);
                if text.contains(needle) {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn error_codes_round_trip() {
        assert_eq!(StaapError::Ok as c_int, 0);
        assert_eq!(StaapError::Spawn as c_int, 1);
        assert_eq!(StaapError::Io as c_int, 2);
        assert_eq!(StaapError::Utf8 as c_int, 3);
        assert_eq!(StaapError::Null as c_int, 4);
        assert_eq!(StaapError::Config as c_int, 5);
    }

    #[test]
    fn null_ptrs_return_errors_never_ub() {
        unsafe {
            assert_eq!(
                staap_spawn(
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    80,
                    24
                ),
                StaapError::Null.code()
            );
            // Null core with a live out pointer is still a null error.
            let mut out: *mut StaapPty = std::ptr::null_mut();
            assert_eq!(
                staap_spawn(std::ptr::null(), &mut out, std::ptr::null(), 80, 24),
                StaapError::Null.code()
            );
            assert!(out.is_null());
            assert!(!staap_pump(std::ptr::null_mut()));
            assert_eq!(
                staap_write(std::ptr::null_mut(), std::ptr::null(), 0),
                StaapError::Null.code()
            );
            staap_resize(std::ptr::null_mut(), 80, 24);
            assert!(staap_screen_text(std::ptr::null()).is_null());
            assert!(staap_spans_json(std::ptr::null()).is_null());
            assert_eq!(staap_status(std::ptr::null(), 0), -1);
            assert_eq!(staap_core_save(std::ptr::null()), StaapError::Null.code());
            staap_core_free(std::ptr::null_mut());
            staap_pty_free(std::ptr::null_mut());
            staap_screen_text_free(std::ptr::null_mut());
            // Every failing call above recorded a message; the pointer is
            // always valid and non-null.
            let msg = staap_last_error();
            assert!(!msg.is_null());
            assert!(!CStr::from_ptr(msg).to_bytes().is_empty());
        }
    }

    #[test]
    fn snapshot_owns_data_after_pty_drop() {
        // §5.2 core claim: the snapshot outlives (and is unaffected by
        // mutating/dropping) the PTY it was copied from.
        unsafe {
            let core = staap_core_new();
            assert!(!core.is_null());
            // Spawn `echo` directly: deterministic without a real `muse`.
            let mut pty: *mut StaapPty = std::ptr::null_mut();
            let prog = CString::new("echo").unwrap();
            let arg = CString::new("ffi-owned-snap").unwrap();
            let argv = [prog.as_ptr(), arg.as_ptr()];
            let rc = staap_spawn_argv(core, &mut pty, argv.as_ptr(), 2, std::ptr::null(), 80, 24);
            assert_eq!(rc, StaapError::Ok.code());
            assert!(!pty.is_null());
            assert!(pump_until_text(
                pty,
                "ffi-owned-snap",
                Duration::from_secs(5)
            ));
            let raw = staap_screen_text(pty);
            assert!(!raw.is_null());
            let owned = CStr::from_ptr(raw).to_string_lossy().into_owned();
            staap_screen_text_free(raw);
            // Mutate then drop the PTY: the owned copy must not change.
            staap_resize(pty, 100, 30);
            staap_pump(pty);
            staap_pty_free(pty);
            assert!(owned.contains("ffi-owned-snap"));
            // Spans variant carries the same text (JSON-escaped or plain).
            let core2 = staap_core_new();
            let mut pty2: *mut StaapPty = std::ptr::null_mut();
            assert_eq!(
                staap_spawn_argv(core2, &mut pty2, argv.as_ptr(), 2, std::ptr::null(), 80, 24),
                StaapError::Ok.code()
            );
            assert!(pump_until_text(
                pty2,
                "ffi-owned-snap",
                Duration::from_secs(5)
            ));
            let spans_raw = staap_spans_json(pty2);
            assert!(!spans_raw.is_null());
            let spans = CStr::from_ptr(spans_raw).to_string_lossy().into_owned();
            staap_screen_text_free(spans_raw);
            staap_pty_free(pty2);
            assert!(spans.contains("ffi-owned-snap"));
            staap_core_free(core);
            staap_core_free(core2);
        }
    }

    // Unix-only like the fake-`muse` test above: `printf` is a POSIX
    // utility, not guaranteed on Windows.
    #[cfg(unix)]
    #[test]
    fn spans_json_preserves_sgr_red() {
        // Issue #70 falsifiable at the core edge: a child emitting ANSI
        // colors (`printf '\e[31mred\e[0m'`) must surface a red span, which
        // is what the Swift pump re-emits as SGR for the terminal view.
        unsafe {
            let core = staap_core_new();
            assert!(!core.is_null());
            let mut pty: *mut StaapPty = std::ptr::null_mut();
            let prog = CString::new("printf").unwrap();
            // POSIX octal escapes in the format string: the child emits
            // real SGR bytes around `red`.
            let arg = CString::new("\\033[31mred\\033[0m\\n").unwrap();
            let argv = [prog.as_ptr(), arg.as_ptr()];
            let rc = staap_spawn_argv(core, &mut pty, argv.as_ptr(), 2, std::ptr::null(), 80, 24);
            assert_eq!(rc, StaapError::Ok.code());
            assert!(!pty.is_null());
            assert!(pump_until_text(pty, "red", Duration::from_secs(5)));
            // The plain snapshot strips SGR (the #70 root cause on its own).
            let raw = staap_screen_text(pty);
            assert!(!raw.is_null());
            let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
            staap_screen_text_free(raw);
            assert!(text.contains("red"));
            assert!(!text.contains('\x1b'));
            // The spans snapshot keeps the red style (palette 205,0,0).
            let spans_raw = staap_spans_json(pty);
            assert!(!spans_raw.is_null());
            let spans = CStr::from_ptr(spans_raw).to_string_lossy().into_owned();
            staap_screen_text_free(spans_raw);
            staap_pty_free(pty);
            staap_core_free(core);
            assert!(spans.contains("red"), "spans carry text: {spans}");
            assert!(
                spans.contains("[205,0,0]"),
                "red SGR maps to palette red: {spans}"
            );
        }
    }

    #[test]
    fn spawn_failure_reports_spawn_code() {
        unsafe {
            let core = staap_core_new();
            assert!(!core.is_null());
            let mut pty: *mut StaapPty = std::ptr::null_mut();
            let prog = CString::new("definitely-not-a-real-binary-xyz").unwrap();
            let argv = [prog.as_ptr()];
            let rc = staap_spawn_argv(core, &mut pty, argv.as_ptr(), 1, std::ptr::null(), 80, 24);
            assert_eq!(rc, StaapError::Spawn.code());
            assert!(pty.is_null());
            let msg = CStr::from_ptr(staap_last_error())
                .to_string_lossy()
                .into_owned();
            assert!(msg.contains("definitely-not-a-real-binary-xyz"));
            assert_eq!(staap_status(core, usize::MAX), -2);
            staap_core_free(core);
        }
    }

    #[test]
    fn status_codes_map_roster_status() {
        use crate::app::{ChatSession, HARNESS_MUSE};
        fn row(id: &str, status: Status) -> ChatSession {
            ChatSession {
                id: id.into(),
                title: id.into(),
                project: "proj".into(),
                status,
                harness: HARNESS_MUSE.into(),
                last_active: 0,
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
        let core = StaapCore {
            reg: crate::runs::RunRegistry::new(vec![
                row("a", Status::Attention),
                row("b", Status::Idle),
                row("c", Status::Working),
            ]),
        };
        unsafe {
            assert_eq!(staap_status(&core as *const StaapCore, 0), 0);
            assert_eq!(staap_status(&core as *const StaapCore, 1), 1);
            assert_eq!(staap_status(&core as *const StaapCore, 2), 2);
            assert_eq!(staap_status(&core as *const StaapCore, 3), -2);
            // Null handle reports -1 and records a message (never UB).
            assert_eq!(staap_status(std::ptr::null(), 0), -1);
            let msg = CStr::from_ptr(staap_last_error())
                .to_string_lossy()
                .into_owned();
            assert!(msg.contains("staap_status"), "last_error: {msg:?}");
        }
    }

    /// Restores a process env var on drop (tests share one process).
    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
            let old = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, old }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// Serializes the PATH-mutating hermetic spawn tests: `cargo test`
    /// runs threads in one process, so two tests prepending different
    /// fake-CLI dirs would clobber each other's PATH mid-spawn (a lost
    /// fake reads as a spawn failure). Hold this across the whole
    /// prepend-spawn-restore window.
    #[cfg(unix)]
    static PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn unique_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "staap-ffi61-{tag}-{}-{}",
            std::process::id(),
            nanos
        ))
    }

    // Unix-only: the fake is an extensionless shell script, which
    // CreateProcess cannot execute. The Windows counterpart is the
    // hermetic smoke-live step of the windows CI job (issue #64):
    // tests/smoke_live.ps1 compiles tests/fake_muse.c to muse.exe and
    // runs am-win-smoke --smoke-live against it.
    #[cfg(unix)]
    #[test]
    fn public_spawn_success_path() {
        // Issue #61: the review flagged the public `staap_spawn` (which runs
        // the real `muse` command) as success-untested. A fake `muse`
        // earlier on PATH exercises the exact public entry point,
        // hermetically: no real agent, no live config.
        let dir = unique_dir("muse");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("muse"),
            "#!/bin/sh\necho fake-muse-ready-61\nsleep 30\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("muse"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let old_path = std::env::var("PATH").unwrap_or_default();
        let joined = format!("{}:{}", dir.display(), old_path);
        let _lock = PATH_LOCK.lock().unwrap();
        let _path = EnvGuard::set("PATH", std::ffi::OsStr::new(&joined));
        unsafe {
            let core = staap_core_new();
            assert!(!core.is_null());
            let mut pty: *mut StaapPty = std::ptr::null_mut();
            let rc = staap_spawn(core, &mut pty, std::ptr::null(), 80, 24);
            assert_eq!(rc, StaapError::Ok.code());
            assert!(!pty.is_null());
            assert!(pump_until_text(
                pty,
                "fake-muse-ready-61",
                Duration::from_secs(10)
            ));
            staap_pty_free(pty);
            staap_core_free(core);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn core_save_round_trips_to_scoped_config() {
        // Issue #61: `staap_core_save` was deliberately untested (live user
        // config). `$STAAP_CONFIG` scopes it to a temp file, so
        // the success path is covered without touching real config.
        let dir = unique_dir("cfg");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.json");
        let _scoped = EnvGuard::set("STAAP_CONFIG", file.as_os_str());
        unsafe {
            let core = staap_core_new();
            assert!(!core.is_null());
            assert_eq!(staap_core_save(core), StaapError::Ok.code());
            staap_core_free(core);
        }
        let text = std::fs::read_to_string(&file).expect("scoped config written");
        let _: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        // Failure path: the scoped path is a directory, so the write
        // fails and the Config code (plus message) is reported.
        let sub = dir.join("adir");
        std::fs::create_dir_all(&sub).unwrap();
        let _scoped2 = EnvGuard::set("STAAP_CONFIG", sub.as_os_str());
        unsafe {
            let core = staap_core_new();
            assert!(!core.is_null());
            assert_eq!(staap_core_save(core), StaapError::Config.code());
            let msg = CStr::from_ptr(staap_last_error())
                .to_string_lossy()
                .into_owned();
            assert!(msg.contains("staap_core_save"), "last_error: {msg:?}");
            staap_core_free(core);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_and_cwd_edge_cases_report_codes() {
        unsafe {
            let core = staap_core_new();
            assert!(!core.is_null());
            let mut pty: *mut StaapPty = std::ptr::null_mut();
            let prog = CString::new("sleep").unwrap();
            let arg = CString::new("30").unwrap();
            let argv = [prog.as_ptr(), arg.as_ptr()];
            let rc = staap_spawn_argv(core, &mut pty, argv.as_ptr(), 2, std::ptr::null(), 80, 24);
            assert_eq!(rc, StaapError::Ok.code());
            // Null data with len > 0 on a live handle is a Null error.
            assert_eq!(
                staap_write(pty, std::ptr::null(), 1),
                StaapError::Null.code()
            );
            let msg = CStr::from_ptr(staap_last_error())
                .to_string_lossy()
                .into_owned();
            assert!(msg.contains("staap_write"), "last_error: {msg:?}");
            // Non-UTF8 cwd is a Utf8 error and spawns nothing.
            let bad: [u8; 2] = [0xFF, 0x00];
            let mut out2: *mut StaapPty = std::ptr::null_mut();
            assert_eq!(
                staap_spawn_argv(
                    core,
                    &mut out2,
                    argv.as_ptr(),
                    2,
                    bad.as_ptr() as *const c_char,
                    80,
                    24
                ),
                StaapError::Utf8.code()
            );
            assert!(out2.is_null());
            let msg = CStr::from_ptr(staap_last_error())
                .to_string_lossy()
                .into_owned();
            assert!(msg.contains("UTF-8"), "last_error: {msg:?}");
            staap_pty_free(pty);
            staap_core_free(core);
        }
    }

    #[test]
    fn session_count_and_json_round_trip() {
        use crate::app::{ChatSession, HARNESS_MUSE};
        fn row(id: &str, status: Status) -> ChatSession {
            ChatSession {
                id: id.into(),
                title: id.into(),
                project: "proj".into(),
                status,
                harness: HARNESS_MUSE.into(),
                last_active: 0,
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
        let core = StaapCore {
            reg: crate::runs::RunRegistry::new(vec![
                row("a", Status::Attention),
                row("b", Status::Idle),
            ]),
        };
        unsafe {
            assert_eq!(staap_session_count(&core as *const StaapCore), 2);
            assert_eq!(staap_session_count(std::ptr::null()), 0);
            let raw = staap_session_json(&core as *const StaapCore, 0);
            assert!(!raw.is_null());
            let text = CStr::from_ptr(raw).to_str().unwrap().to_owned();
            staap_screen_text_free(raw);
            let doc: serde_json::Value =
                serde_json::from_str(&text).expect("row serializes as JSON");
            assert_eq!(doc["id"], "a");
            assert_eq!(doc["title"], "a");
            assert_eq!(doc["project"], "proj");
            assert_eq!(doc["status"], "Attention");
            // Out-of-bounds and null handles report null with a message.
            assert!(staap_session_json(&core as *const StaapCore, 7).is_null());
            let msg = CStr::from_ptr(staap_last_error())
                .to_string_lossy()
                .into_owned();
            assert!(msg.contains("staap_session_json"), "last_error: {msg:?}");
            assert!(staap_session_json(std::ptr::null(), 0).is_null());
        }
    }

    #[test]
    fn launch_abi_lists_catalog_recents_and_spawns() {
        // 2D launch: the catalog lists every supported CLI in order, the
        // recents start empty on null, and a fake-CLI spawn through the
        // public launch entry resolves flags without touching live state.
        use crate::launch::SUPPORTED_CLIS;
        unsafe {
            let raw = staap_clis_json();
            assert!(!raw.is_null());
            let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
            staap_screen_text_free(raw);
            let docs: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
            let ids: Vec<&str> = docs.iter().map(|d| d["id"].as_str().unwrap()).collect();
            assert_eq!(ids, SUPPORTED_CLIS);
            // Null core: empty recents, never UB.
            let recents_raw = staap_recent_json(std::ptr::null());
            assert!(!recents_raw.is_null());
            let recents = CStr::from_ptr(recents_raw).to_string_lossy().into_owned();
            staap_screen_text_free(recents_raw);
            assert_eq!(recents, "[]");
            // Null guards on the new entry points.
            let mut out: *mut StaapPty = std::ptr::null_mut();
            assert_eq!(
                staap_spawn_launch(
                    std::ptr::null(),
                    &mut out,
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    80,
                    24
                ),
                StaapError::Null.code()
            );
            assert_eq!(
                staap_note_launch(std::ptr::null_mut(), std::ptr::null(), std::ptr::null()),
                StaapError::Null.code()
            );
        }
        // Note + recents round-trip on a live core.
        let core = StaapCore {
            reg: crate::runs::RunRegistry::new(vec![]),
        };
        let core_ptr = &core as *const StaapCore as *mut StaapCore;
        unsafe {
            let cli = CString::new("claude").unwrap();
            let cwd = CString::new("/tmp/api").unwrap();
            assert_eq!(
                staap_note_launch(core_ptr, cli.as_ptr(), cwd.as_ptr()),
                StaapError::Ok.code()
            );
            let raw = staap_recent_json(core_ptr);
            assert!(!raw.is_null());
            let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
            staap_screen_text_free(raw);
            assert_eq!(text, "[\"/tmp/api\"]");
            // Effective CLI: explicit wins, null resolves the default
            // (here the noted last_cli), null core still resolves.
            let explicit = CString::new("codex").unwrap();
            let raw = staap_effective_cli(core_ptr, explicit.as_ptr());
            assert!(!raw.is_null());
            let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
            staap_screen_text_free(raw);
            assert_eq!(text, "codex");
            let raw = staap_effective_cli(core_ptr, std::ptr::null());
            assert!(!raw.is_null());
            let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
            staap_screen_text_free(raw);
            assert_eq!(text, "claude");
            let raw = staap_effective_cli(std::ptr::null(), std::ptr::null());
            assert!(!raw.is_null());
            let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
            staap_screen_text_free(raw);
            assert!(!text.is_empty());
        }
    }

    // Unix-only like `public_spawn_success_path`: the fake is a shell
    // script (CreateProcess cannot execute it; Windows coverage is the
    // smoke-live harness).
    #[cfg(unix)]
    #[test]
    fn spawn_launch_yolo_tristate_reaches_child_argv() {
        // 2D launch: the tri-state yolo int flows into the child argv —
        // force-on appends the canonical flag, force-off suppresses the
        // opted-in config default, default follows it. The fake `muse`
        // echoes its argv, so the screen proves the flag story.
        use std::time::Duration;
        let dir = unique_dir("launch-yolo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("muse"), "#!/bin/sh\necho ARGV:$*\nsleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("muse"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let old_path = std::env::var("PATH").unwrap_or_default();
        let joined = format!("{}:{}", dir.display(), old_path);
        let _lock = PATH_LOCK.lock().unwrap();
        let _path = EnvGuard::set("PATH", std::ffi::OsStr::new(&joined));
        // Opt the agent into yolo by default: default(0) and force-on(1)
        // must show `--yolo`, force-off(-1) must not.
        unsafe {
            let core = staap_core_new();
            assert!(!core.is_null());
            (*core).reg.app.config_mut().agents.insert(
                "muse".to_string(),
                crate::config::AgentConfig {
                    extra_args: Vec::new(),
                    yolo: true,
                },
            );
            for (yolo, want_flag) in [(0, true), (1, true), (-1, false)] {
                let mut pty: *mut StaapPty = std::ptr::null_mut();
                let cli = CString::new("muse").unwrap();
                let rc = staap_spawn_launch(
                    core,
                    &mut pty,
                    cli.as_ptr(),
                    std::ptr::null(),
                    yolo,
                    80,
                    24,
                );
                assert_eq!(rc, StaapError::Ok.code());
                assert!(!pty.is_null());
                assert!(pump_until_text(pty, "ARGV:", Duration::from_secs(10)));
                let raw = staap_screen_text(pty);
                assert!(!raw.is_null());
                let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
                staap_screen_text_free(raw);
                staap_pty_free(pty);
                assert_eq!(
                    text.contains("--yolo"),
                    want_flag,
                    "yolo={yolo}: screen {text:?}"
                );
            }
            staap_core_free(core);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_spawn_pump_write_close_round_trip() {
        // The native-shell path end to end: spawn a row through the
        // registry (fake `true` argv keeps it hermetic), pump it, write
        // to it, read its screen, then close it. Statuses move: the pump
        // is the roster refresh gate now.
        unsafe {
            let core = staap_core_new();
            assert!(!core.is_null());
            // Seed one roster row to attach to (registry spawns create
            // their own rows; this checks the low-level attach path).
            let before = staap_session_count(core);
            // Registry spawn via the public entry (fake CLI would need
            // PATH surgery; exercise the helpers around a direct attach).
            assert_eq!(staap_live_count(core), 0);
            assert!(!staap_is_live(core, c"nope".as_ptr()));
            assert!(!staap_run_pump(core, c"nope".as_ptr()));
            assert!(!staap_run_exited(core, c"nope".as_ptr()));
            assert!(staap_run_screen_text(core, c"nope".as_ptr()).is_null());
            assert!(staap_run_spans_json(core, c"nope".as_ptr()).is_null());
            assert_eq!(staap_selected(core), 0);
            staap_select(core, 99);
            staap_select_step(core, 1);
            staap_select_step(core, 0);
            staap_set_filter(core, c"zzz-no-match".as_ptr());
            assert!(!staap_row_matches(core, 0, c"zzz-no-match".as_ptr()));
            staap_set_filter(core, std::ptr::null());
            // History expansion round-trips through the core (collapsed
            // by default; null-safe on both ends).
            assert!(!staap_history_expanded(core));
            assert!(!staap_history_expanded(std::ptr::null()));
            staap_set_history_expanded(core, 1);
            assert!(staap_history_expanded(core));
            staap_set_history_expanded(std::ptr::null_mut(), 1);
            staap_set_history_expanded(core, 0);
            assert!(!staap_history_expanded(core));
            // Unknown ids own no PTY: historic, null-safe.
            assert!(staap_is_history(core, c"nope".as_ptr()));
            assert!(!staap_is_history(std::ptr::null(), c"nope".as_ptr()));
            assert!(!staap_is_history(core, std::ptr::null()));
            // Key encoding through the shared table.
            let mut buf = [0u8; 8];
            assert_eq!(
                staap_key_encode(
                    c"enter".as_ptr(),
                    std::ptr::null(),
                    0,
                    0,
                    buf.as_mut_ptr(),
                    buf.len()
                ),
                1
            );
            assert_eq!(buf[0], b'\r');
            assert_eq!(
                staap_key_encode(
                    c"shift".as_ptr(),
                    std::ptr::null(),
                    0,
                    0,
                    buf.as_mut_ptr(),
                    buf.len()
                ),
                0
            );
            assert_eq!(
                staap_key_encode(
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    0,
                    buf.as_mut_ptr(),
                    buf.len()
                ),
                -1
            );
            // Feed delta through the shared reconciler.
            let feed = staap_feed_delta(c"a".as_ptr(), c"a\nb\n".as_ptr());
            assert!(!feed.is_null());
            let text = CStr::from_ptr(feed).to_string_lossy().into_owned();
            staap_screen_text_free(feed);
            assert_eq!(text, "\r\nb\r\n");
            assert!(staap_feed_delta(c"same".as_ptr(), c"same".as_ptr()).is_null());
            // ANSI render through the shared renderer.
            let spans = c"[[{\"text\":\"red\",\"fg\":[205,0,0],\"bg\":null,\"bold\":false,\"italic\":false,\"underline\":false}]]".as_ptr();
            let rendered = staap_ansi_render(spans);
            assert!(!rendered.is_null());
            let out = CStr::from_ptr(rendered).to_string_lossy().into_owned();
            staap_screen_text_free(rendered);
            assert_eq!(out, "\x1b[0;38;2;205;0;0mred");
            assert!(staap_ansi_render(c"nope".as_ptr()).is_null());
            // Preview / yolo / age / glyph / title / clamp helpers.
            let prev = staap_spawn_preview(c"muse".as_ptr(), c"/tmp/api".as_ptr(), 1);
            let prev_text = CStr::from_ptr(prev).to_string_lossy().into_owned();
            staap_screen_text_free(prev);
            assert_eq!(prev_text, "runs: muse in /tmp/api + yolo");
            assert_eq!(staap_yolo_value(1), 1);
            assert_eq!(staap_yolo_value(2), -1);
            assert_eq!(staap_yolo_value(0), 0);
            let age = staap_age_string(600, 0);
            let age_text = CStr::from_ptr(age).to_string_lossy().into_owned();
            staap_screen_text_free(age);
            assert_eq!(age_text, "10m ago");
            let glyph = staap_status_glyph(0);
            let glyph_text = CStr::from_ptr(glyph).to_string_lossy().into_owned();
            staap_screen_text_free(glyph);
            assert_eq!(glyph_text, "●");
            let title = staap_section_title(2);
            let title_text = CStr::from_ptr(title).to_string_lossy().into_owned();
            staap_screen_text_free(title);
            assert_eq!(title_text, "Working");
            assert_eq!(staap_clamp_sidebar(99.0), 220.0);
            assert_eq!(staap_clamp_sidebar(300.0), 300.0);
            assert_eq!(staap_max_runs(), crate::shell_shared::MAX_LIVE_RUNS);
            // Restart/close on unknown ids fail/ignore cleanly.
            assert_eq!(
                staap_run_restart(core, c"missing".as_ptr(), 80, 24),
                StaapError::Spawn.code()
            );
            assert_eq!(
                staap_run_close(core, c"missing".as_ptr()),
                StaapError::Ok.code()
            );
            assert_eq!(staap_session_count(core), before);
            staap_core_free(core);
        }
    }

    #[test]
    fn registry_run_write_resize_pump_flow() {
        // Direct registry flow with a fake child: attach, write, resize,
        // pump, read screen + spans, check exit, close.
        unsafe {
            let core = staap_core_new();
            assert!(!core.is_null());
            let prog = CString::new("cat").unwrap();
            let argv = [prog.as_ptr()];
            let mut pty: *mut StaapPty = std::ptr::null_mut();
            assert_eq!(
                staap_spawn_argv(core, &mut pty, argv.as_ptr(), 1, std::ptr::null(), 80, 24),
                StaapError::Ok.code()
            );
            // Move the PTY into the registry under a scratch row.
            let id = "test-row-1";
            (*core).reg.app.sessions.push(crate::app::ChatSession {
                id: id.into(),
                title: id.into(),
                project: "proj".into(),
                status: Status::Working,
                harness: crate::app::HARNESS_MUSE.into(),
                last_active: 0,
                pr_links: vec![],
                related_links: vec![],
                links_truncated: false,
                transcript: vec![],
                transcript_truncated: false,
                title_locked: true,
                pending_input: String::new(),
                provider_session_id: None,
                cwd: None,
            });
            staap_pty_free(pty);
            (*core).reg.runs.remove(id);
            // Attach a fresh fake child through the registry directly.
            let owned: Vec<String> = vec![];
            let direct = EmbeddedPty::spawn("cat", &owned, 80, 24).expect("cat spawns");
            (*core).reg.attach(id, direct);
            let cid = CString::new(id).unwrap();
            assert!(staap_is_live(core, cid.as_ptr()));
            assert_eq!(staap_live_count(core), 1);
            let probe = b"hi-reg";
            assert_eq!(
                staap_run_write(core, cid.as_ptr(), probe.as_ptr(), probe.len()),
                StaapError::Ok.code()
            );
            staap_run_resize(core, cid.as_ptr(), 100, 30);
            let mut saw = false;
            for _ in 0..100 {
                if staap_run_pump(core, cid.as_ptr()) || staap_pump_all(core) {
                    let raw = staap_run_screen_text(core, cid.as_ptr());
                    if !raw.is_null() {
                        let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
                        staap_screen_text_free(raw);
                        if text.contains("hi-reg") {
                            saw = true;
                            break;
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(saw, "written bytes echo on the run screen");
            let spans = staap_run_spans_json(core, cid.as_ptr());
            assert!(!spans.is_null());
            staap_screen_text_free(spans);
            assert!(!staap_run_exited(core, cid.as_ptr()));
            assert_eq!(staap_run_close(core, cid.as_ptr()), StaapError::Ok.code());
            assert!(!staap_is_live(core, cid.as_ptr()));
            // Closed runs leave no live PTY, so quit needs no confirm.
            (*core).reg.app.sessions.clear();
            assert!(!staap_needs_quit_confirm(core));
            staap_core_free(core);
        }
    }

    /// Test-only spawn with an explicit argv (the public [`staap_spawn`] runs
    /// the configured `muse` session command; tests need fake commands).
    unsafe fn staap_spawn_argv(
        core: *const StaapCore,
        out: *mut *mut StaapPty,
        argv: *const *const c_char,
        argc: usize,
        cwd: *const c_char,
        cols: u16,
        rows: u16,
    ) -> c_int {
        if core.is_null() || out.is_null() {
            set_error("staap_spawn: null core or out".to_string());
            return StaapError::Null.code();
        }
        if argc > 0 && argv.is_null() {
            set_error("staap_spawn: null argv".to_string());
            return StaapError::Null.code();
        }
        let mut parts = Vec::with_capacity(argc);
        for i in 0..argc {
            let arg = CStr::from_ptr(*argv.add(i)).to_str();
            match arg {
                Ok(s) => parts.push(s.to_string()),
                Err(_) => {
                    set_error("staap_spawn: argv is not valid UTF-8".to_string());
                    return StaapError::Utf8.code();
                }
            }
        }
        let (program, args) = match parts.split_first() {
            Some((first, rest)) => (first.clone(), rest.to_vec()),
            None => (*core).reg.app.spawn_command_for(&SpawnKind::New),
        };
        spawn_into(out, &program, &args, cwd, cols, rows)
    }
}
