# Native core seam (proposal)

Status: proposal only — except for the three landed consumers: `swift/`
(`StaapMac`, #62) binds the C ABI below via SwiftTerm + SwiftUI
(see `swift/README.md` for its wiring table), `native/linux/`
(`staap-gtk`, #63) binds the same C ABI via GTK4/libadwaita +
VTE (see `native/linux/README.md` for its wiring table), and
`native/windows/` (`StaapWinUI`, #64) binds it via WinUI 3 +
ConPTY (see `native/windows/README.md` for its wiring table). The rest
of this doc records the audit result, the decoupling already landed,
and the exact core API a per-OS shell binds against.

Volatility-shield rule (from `VISION-TECHNICAL.md`): no framework types
in the core. Core = everything except `src/gui/` and `src/main.rs`.

## 1. Leak inventory + what moved

Full-repo grep for framework references (`gpui`, `ThemeMode`,
`WindowAppearance`, `Hsla`, `SharedString`, component imports):

| Location | Kind | Verdict |
|---|---|---|
| `src/config.rs` (`ThemePreference::theme_mode`) | **core leak** — signature named `WindowAppearance` and returned `ThemeMode` | **moved** (see below) |
| `src/gui/runs_panel.rs`, `shell.rs`, `terminal_pane.rs`, `nav.rs`, `view.rs` | direct `use gpui` / `use gpui-component` | expected: these are the thin shell |
| `src/gui/terminal.rs` (`to_hsla`) | returns `gpui::Hsla`; rest of the module is plain vt100 math | contained gui-local coupling, left as-is (see §4) |
| `src/main.rs` | `Application`, `Root`, `Theme` | expected: entry point is shell |
| `src/app.rs`, `persist.rs`, `scrollback.rs`, `transcript.rs`, `parsers/`, `providers/`, `embedded.rs` | — | clean, zero framework refs (verified by grep) |

Decoupling landed (minimal, behavior-preserving):

- `config::ThemePreference::theme_mode(Option<WindowAppearance>) -> ThemeMode`
  is replaced by framework-free `resolve(OsAppearance) -> EffectiveTheme`
  plus `is_dark(OsAppearance) -> bool`. New plain types
  `config::OsAppearance::{Dark, Light, Unknown}` (`Unknown` = headless,
  keeps the historic resolve-to-dark default) and
  `config::EffectiveTheme::{Dark, Light}` with `is_dark()`.
- The single framework touchpoint moved to
  `gui::theme::theme_mode_for(ThemePreference, WindowAppearance) -> ThemeMode`,
  used by both `main` (startup) and `shell` (the `t`-key cycle). A framework
  upgrade touches one function; its vibrant-variant mapping is pinned by
  `vibrant_appearances_resolve_to_their_base_mode`, and the core contract
  by `theme_choice_cycles_parses_and_resolves`.
- After the move, `grep -rni gpui` over the core module set returns zero
  hits (code and prose alike), so a future CI gate can enforce it literally.

## 2. Stable core API for native shells

All paths are Rust items inside the current binary crate; §5 notes the
one structural prerequisite (promote core to a library) before any shell
can link them.

### Session roster CRUD (`app`)

- Types: `ChatSession` (serde JSON; stable row key `id`, display-only
  `title`, `project`, `status: Status`, `harness`, `last_active`,
  `pr_links` / `related_links` / `links_truncated`, `transcript` /
  `transcript_truncated`, `title_locked`, `pending_input`,
  `provider_session_id`, `cwd: Option<String>`), `Status::{Attention, Idle,
  Working}`, `Focus`, `App` (`sessions`, `selected`, `filter`,
  `history_expanded` fields).
- Roster: `App::new(sessions)`, `start_new_session()`,
  `start_new_session_in(Option<String>)`, `remove_session(&str)`,
  `selected_session()`, `select_next/select_prev/select_page_next/
  select_page_prev`, `resort_keep_selection()`, `visible_indices()`,
  `matches_filter()`, `new_session_id()`, `sort_sessions()`,
  `status_sections()`, `section_title()`, `attention_count()`.
- Run flow: `take_pending_spawn() -> Option<SpawnKind>`,
  `retry_spawn(SpawnKind)`, `has_pending_spawn()`,
  `respawn_kind(&str) -> SpawnKind`, `session_cwd(&str)`,
  `note_submitted_prompt(&str, &str)`, `spawn_command_for(&SpawnKind) ->
  (String, Vec<String>)`, `spawn_command_string()`,
  `spawn_command_string_for(&SpawnKind)`.
- 2D launch (folder × CLI + yolo): `App::start_launch(&LaunchSelection)`
  (the single funnel every new-run path shares), `repeat_cli()`,
  `repeat_spawn_kind()`, `note_launch(&str, Option<&str>)`, plus the
  FFI config views (`config_last_cli/default_cli/recents/default_cwd/
  extra_args/yolo_default`).

### 2D launch picker (`launch` + `config`)

- Catalog: `SUPPORTED_CLIS` (`muse`, `claude`, `opencode`, `codex`),
  `yolo_flag_for(cli)` (`muse --yolo`,
  `claude --dangerously-skip-permissions`; codex/opencode map to `None`
  until their flags are verified against a live binary),
  `MAX_RECENT_FOLDERS` (10).
- Autodetect: `detect_available_clis()` / `detect_with_path(OsStr)` —
  `PATH` (+ GUI-sparse extra dirs) scan, no subprocess; rows always list
  every CLI with `available` + `path` (`None` = missing, listed but
  disabled).
- Resolution: `resolve_effective_cli(explicit, last, default, catalog)`,
  `first_available(&[AvailableCli])`; per-run `YoloChoice::{UseDefault,
  ForceOn, ForceOff}` (`cycle`/`label`/`resolve`); `LaunchSelection::new/
  preview`; `yolo_args_for(&LaunchSelection)`; `push_recent_folder`.
- Picker model: `LaunchPicker::new/selected_cli/step_cli/step_recent/key/
  confirm/preview` (blank folder = inherit, `~` expands via
  `app::expand_cwd_input`; `key` owns Tab/arrows/`c`/`y`/typing dispatch).
- Config: `AgentConfig.yolo` (per-agent yolo default, opt-in),
  `Config.{default_cli, default_cwd, last_cli, recent_folders}`,
  `yolo_default_for(agent)`, `note_launch(cli, cwd)`; old files load via
  serde defaults; the picker override never writes back implicitly.
- FFI: `staap_spawn_launch(core, out, cli, cwd, yolo, cols, rows)`,
  `staap_clis_json()`, `staap_recent_json(core)`, `staap_note_launch(core, cli,
  cwd)` (regenerate the header with cbindgen after any FFI change).

### Spawn with cwd/flags (`embedded` + `config`)

- `EmbeddedPty::spawn(program, args, cols, rows)` and
  `EmbeddedPty::spawn_with_cwd(program, args, cols, rows,
  Option<&Path>)` — the single spawn seam; `None` cwd inherits the app
  directory. `SpawnKind::{New, NewOn { cli, yolo }, Resume { session_id }}`
  with `SpawnKind::command()` (`NewOn` = plain program, yolo rides via
  `App::spawn_command_for`) and `SpawnKind::cli_id()`;
  `cli_session_command(cli)` builds the plain per-CLI command.
- Per-agent flags: `Config::extra_args_for(agent) -> Vec<String>`; the
  shell concatenates these onto the spawn command (same as
  `App::spawn_command_for`).

### stdin writes

- `EmbeddedPty::write_input(&[u8]) -> anyhow::Result<()>` — raw bytes,
  already key-encoded by the shell; flush included.

### Output pump / polling (`embedded` + `scrollback`)

- Poll model, no callbacks: the shell runs its own timer (≈20 Hz today)
  and calls `EmbeddedPty::pump() -> bool`; `true` means fresh output or a
  new exit — the only dirty gate the shell needs to repaint.
- Snapshot: `EmbeddedPty::view() -> LiveView { screen: &vt100::Screen,
  exited: bool }`, `resize(cols, rows)`,
  `scrollback_log() -> &ScrollbackLog` (`lines()`, `pending_line()`,
  `line_count()`, `truncated`).
- Portable render helpers a native shell can reuse or port
  (`gui/terminal.rs`, plain functions over vt100 types): `screen_rows`,
  `selection_rows`, `selection_text`, `point_to_cell`,
  `normalize_selection`, `vt_color_for` / `vt_color`, `screen_fingerprint`.
  Only `to_hsla` returns a framework type; its RGB→HSL math is plain and
  ports 1:1 (see §4).

### Status classification (`app`)

- One classifier: `classify(screen_text, output_age, exited) -> Status`,
  `classify_with_attention(bool, output_age, exited)`,
  `needs_attention(&str) -> bool`, `WORKING_WINDOW_SECS` (60),
  `harness_badge(&str)`, `short_cwd(&str)`, `MAX_STORED_LINKS` (50) /
  `MAX_VISIBLE_LINKS` (20), `ChatSession::push_links`,
  `visible_links(&[String])`.

### Persistence (`persist`)

- `load_sessions() / load_sessions_from(&Path)`,
  `save_sessions_to(&Path, &[ChatSession])` (production `save_sessions`
  is `#[cfg(not(test))]` so tests never touch the live file),
  `merge_sessions(discovered, persisted)`, `runs_path()`, `data_dir()`,
  `export_markdown(&ChatSession, &str)`, `write_export()`. Policy the
  shell inherits: missing/corrupt files degrade to empty, never fail
  startup.

### Config get/set (`config` + `App`)

- `Config::load() / load_from(&Path) / save()`, `config_path()`,
  `ThemePreference::{cycle, label, resolve, is_dark}`,
  `TerminalConfig::{font_stack, font_family, font_size, fallback_fonts}`,
  `default_sidebar_width()`; via app: `set_config`, `save_config`,
  `cycle_theme`, `terminal_config`, `sidebar_width`,
  `set_terminal_font_size`, `set_sidebar_width`.

### Link extraction (`parsers`)

- `Parser` trait + `RegistryParser::default()` (fan-out; the seam for new
  content types — no new pipelines per type), `pr_links(&str)`,
  `related_links(&str)`, `github::{extract_pr_links,
  extract_issue_links, extract_commit_links}`,
  `refs::extract_file_refs`, `Parsed { title, project, pr_links,
  related_links }`.

### Historic discovery (`providers` + `transcript`)

- `Provider::discover_sessions() -> Vec<ChatSession>` (never fails;
  unreachable store yields `[]`), `MuseCliProvider::new(store_root,
  Box<dyn Parser>)`, `MuseCliProvider::default_store_root()`.
- `transcript::{parse_transcript, derive_title, single_line,
  extract_project, extract_session_name}`, `TranscriptMessage { role:
  Role, text }`, caps `MAX_TAIL_BYTES` / `MAX_MESSAGES` /
  `MAX_MESSAGE_CHARS` / `MAX_TITLE_CHARS`.

## 3. Binding shape sketches

### Option A — C ABI (`extern "C"` over opaque handles; sketch, superseded by §5 Landed C ABI)

```c
// Opaque handles; the shell never sees Rust internals.
typedef struct StaapCore StaapCore;   // owns App + Config
typedef struct StaapPty StaapPty;     // owns one EmbeddedPty

StaapCore *staap_core_new(void);      // discovery + persistence merge inside
void staap_core_free(StaapCore *);
int32_t staap_spawn(StaapCore *, StaapPty **out, const char *cwd); // 0 = ok
bool staap_pump(StaapPty *);          // dirty gate
int32_t staap_write(StaapPty *, const uint8_t *, size_t);
const char *staap_screen_text(StaapPty *);  // + runs/spans variant for styling
int32_t staap_status(StaapCore *, size_t row);  // Attention/Idle/Working as int
// Roster/config/persist/link lists cross as JSON (ChatSession serde) or
// small getter batches; errors return as thread-local message strings.
```

Rationale: this seam is narrow and poll-based — spawn, pump, write,
resize, and getters are all plain values in and out, with no async
callbacks, no streaming, and no shared borrowing across the boundary
(`view()` borrows become copied text/spans at the FFI edge). A C ABI
keeps the build trivial (one `staticlib` + modulemap, no codegen phase
in Xcode), gives full control over threading (the shell keeps its own
pump timer, matching today's dirty-gated loop), and forces the one
healthy conversion — borrowed screen snapshots to owned bytes — to
happen explicitly. JSON for roster/config rows reuses the already-tested
serde shapes instead of inventing parallel structs.

### Option B — UniFFI-style generated bindings

```rust
// UDL / proc-macro sketch: records cross by value, PTYs as objects.
record SessionRow { id: String, title: String, project: String,
    status: Status, last_active: i64, pr_links: Vec<String>, … }
enum Status { Attention, Idle, Working }
interface Core {
    constructor(store_root: String);
    Vec<SessionRow> roster();
    u64 spawn(optional String cwd, u16 cols, u16 rows);
}
interface Pty { bool pump(); void write(Vec<u8> bytes);
    string screen_text(); boolean exited(); void resize(u16 c, u16 r); }
```

Rationale: if the surface grows (per-session options, export, provider
configuration) and hand-marshalled JSON/getter batches become their own
bug class, generated Swift (and later Kotlin) bindings remove that class
entirely — records, enums, `Result`, and `Option` map 1:1 and stay in
sync by construction. The price is a new build dependency and Xcode
build phase, core types reshaped into UniFFI-supported shapes, and a
`Send + Sync` audit of everything behind an object handle (the PTY owns
a reader thread and channel today, so interior mutability and the
borrowed `LiveView` need redesigning into owned snapshots first — work
the C ABI also wants, but UniFFI requires up front).

### Recommendation

Start with the C ABI. It matches the seam as it exists — a thin
poll-driven shell over an already framework-free core — with the least
new machinery: no codegen, no dependency, no type reshaping, and the
borrowed-screen-to-owned-text conversion made explicit at one edge.
Revisit UniFFI when a second generated language (Kotlin) is actually on
the roadmap or the getter surface demonstrably outgrows JSON rows; both
paths want the same two prerequisites (§5), so nothing is thrown away.

## 4. Known remaining gui-local coupling (not core leaks)

- `gui/terminal.rs::to_hsla(Rgb8) -> gpui::Hsla`: pure RGB→HSL math
  returning a framework color. A native shell ports the ~25-line math and
  returns its own color (SwiftUI `Color(hue:saturation:brightness:)`).
  If the gpui shell ever needs churn here again, prefer changing the
  return to a plain `{ h, s, l, a: f32 }` struct and converting at the
  three call sites (`terminal_pane.rs`, `view.rs`) — one step closer to a
  shareable terminal-color module.
- `gui/terminal_pane.rs::term_is_light(&gpui::App)` and the font-probe
  helpers take framework contexts; they are render-layer adapters with no
  core equivalent needed (native shells read `EffectiveTheme::is_dark`
  and `TerminalConfig::font_stack` directly).

### Dumb-shell contract (all three native shells)

Every native shell is a dumb renderer over the core run registry: the
core owns the roster, selection, filter, live PTYs, statuses, links,
key table, feed reconciler, SGR renderer, preview copy, display strings
(age/glyph/headers), and persistence. Shells own widgets, event wiring,
clipboard access, focus, and byte transport — nothing else. Shared
behavior with native look:

- Roster: core registry rows, grouped Needs input → Working → Idle →
  History on every shell (active = attached live PTY, even when
  exited; historic = no live PTY, hidden in the collapsed-by-default
  History group; expansion is core-owned and persisted); one shared
  cap (10, oldest-exited reaped first); spawns attach real rows
  (`staap_run_spawn`), restart/resume keep the id
  (`staap_run_restart`), close drops entry + PTY (`staap_run_close`).
- Statuses actually move: `staap_pump_all` refreshes attention/links and
  re-sorts inside (the old launch-snapshot rows never changed).
- Persistence is automatic (throttled pump autosave + close hooks):
  no Save buttons, no Ctrl/Cmd+S. Theme follows the system appearance:
  no theme pickers (the Linux System/Dark/Light dropdown and its
  plain-file pref are deleted, like the never-existing macOS/Windows
  overrides).
- One key table (`staap_key_encode`), one reconciler (`staap_feed_delta`), one
  SGR renderer (`staap_ansi_render`), one preview (`staap_spawn_preview`), one
  filter/selection (`staap_row_matches`/`staap_set_filter`/`staap_selected`/
  `staap_select`/`staap_select_step`), one sidebar clamp (`staap_clamp_sidebar`).
  The per-shell C/Swift ports (`feed.c`, `picker.c`, `terminal_keys.h`,
  the Swift feed/ANSI duplicates as production paths) are deleted; the
  pure-Swift helpers stay for unit tests only, production calls the core.

### Linux notes (`native/linux/`, #63)

The Linux shell is the portability proof: it links only the core
`staticlib` plus system GTK libs. Two things future shells should copy:

- The `gpui` / `gpui-component` deps in `Cargo.toml` are
  `[target.'cfg(target_os = "macos")'.dependencies]`-scoped. The core
  library compiles (and `cargo test --lib` passes) on Linux without ever
  building the macOS GUI stack; the `staap` binary stays
  macOS-only. On Linux the gates are `cargo build --lib`,
  `cargo test --lib`, and the meson suite — `cargo test --all-targets`
  stays the macOS gate.
- One emulator only: no PTY is spawned inside VTE. The shell translates
  key events to logical names and encodes via the shared core table
  (`bridge_key_encode` over `shell_shared::encode_key`), then feeds core
  snapshots as a stream through the core reconciler (`staap_feed_delta` in
  `src/shell_shared.rs`, unit-pinned by `staap-feed-test` mirroring
  `TerminalFeedTests`). A second line discipline would double-echo.
- Reconciler parity is structural, not ported: every C shell calls the
  same `staap_feed_delta` (append-suffix hot path, scroll overlap,
  clear-and-replay, CRLF normalization), so all shells show identical
  screens from identical snapshots. Swift's `TerminalFeed` stays as the
  Swift-idiomatic original feeding the SwiftTerm view directly (production
  may call the core `staap_feed_delta` instead); the old per-shell `feed.c`
  ports are gone.

### Windows notes (`native/windows/`, #64)

The Windows shell closes the epic (macOS → Linux → Windows) with a
WinUI 3 UI over the same C ABI. What future maintainers should know:

- ConPTY lives in the core, not the shell: on Windows the core's
  `EmbeddedPty` is ConPTY-backed (`portable-pty` uses the native
  Console Pseudo-terminal API), so `bridge_run_spawn` *is* the ConPTY
  spawn. The WinUI surface never creates a console — the same
  single-emulator rule as Linux's "no PTY inside VTE", which keeps
  exactly one line discipline and no double-echo.
- The portable C core (`core_bridge`, `smoke`) is shared design, not
  shared files, with `native/linux/`: each shell vendors its own copy
  so per-OS shells stay independently buildable (the Windows copy adds
  only `extern "C"` guards for its C++ consumer). Shared presentation
  logic (feed reconciler, roster filter match, relative-age label,
  per-row link/age getters, plus the run registry, key table, SGR
  renderer, and preview/selection helpers) lives in the core itself
  (`src/shell_shared.rs` + `src/runs.rs`, bound as `staap_feed_delta` /
  `staap_roster_matches` / `staap_relative_age` / `staap_link_count` /
  `staap_last_active` and the `staap_run_*` / `staap_key_*` / `staap_ansi_*` /
  `staap_select_*` family); `staap-win-feed-test` and `staap-feed-test` pin that
  C ABI edge instead of a vendored copy.
- Key encoding goes through the shared core table
  (`bridge_key_encode` over `shell_shared::encode_key`, replacing the
  old `src/terminal_keys.h` port) but resolves printables through the
  thread layout (`ToUnicode`), with AltGr-as-character documented at the
  one place it diverges from the Linux Alt handling.
- The hermetic live proof compiles `tests/fake_muse.c` to `muse.exe`
  because CreateProcess cannot execute the shell-script fake the unix
  harnesses use; the core's own `public_spawn_success_path` stays
  `#[cfg(unix)]` for the same reason, with `smoke_live.ps1` as its
  Windows counterpart.
- Unlike the Linux job, the Windows CI job runs
  `cargo test --all-targets`: the gpui binary is macOS-gated in
  `src/main.rs` (the `gpui` / `gpui-component` deps stay macOS-scoped
  in `Cargo.toml`; the Windows shell links only `staap.lib` +
  system libs), so on `windows-latest` that exercises the full
  portable lib suite plus a stub bin. Scoping is about what the
  *native shells* link, not what the toolchain could build — and the
  gate keeps `--all-targets` meaningful-green on every runner instead
  of rotting outside macOS.

## 5. Prerequisites before any shell links the core

1. Promote the core to a library: add `[lib] path = "src/core.rs"` (or
   `src/lib.rs`) re-exporting `app`, `config`, `embedded`, `parsers`,
   `persist`, `providers`, `scrollback`, `transcript`, leaving `main.rs`
   + `gui/` as the existing binary consumer. No module moves needed —
   only visibility (`gui`-facing helpers already `pub`/`pub(crate)` where
   the shell needs them).
2. ✅ DONE (slice 2, #60): `EmbeddedPty::snapshot_text()` /
   `snapshot_spans()` / `snapshot()` return owned text + spans
   (`embedded::SnapSpan`-shaped: text + fg/bg/bold/italic/underline as
   plain values; palette duplicated from `gui/terminal.rs`, gui untouched).
   `view()` stays for the gui. No lifetime crosses the FFI boundary.
3. ✅ DONE (slice 2, #60): `src/ffi.rs` maps errors to `StaapError` int
   codes (`Ok=0, Spawn=1, Io=2, Utf8=3, Null=4, Config=5`) + a
   thread-local message via `staap_last_error()`. Degrade-to-empty on
   missing/corrupt files is preserved (discovery + load inside
   `staap_core_new` never fail).

### Landed C ABI (`src/ffi.rs`, crate-type `staticlib` + `rlib`)

Opaque handles (`StaapCore` owns the run registry — roster `App` + live
PTYs; `StaapPty` owns one `EmbeddedPty` for the legacy spawn path); plain
`#[repr(C)]` `StaapRgb` / `StaapStyle` (`has_fg`/`has_bg` presence flags —
`Option` stays on the Rust side). Framework-free core modules behind
the FFI: `shell` (roster rows, key table, feed reconciler, SGR
renderer, preview, display strings, sidebar bounds), `runs` (live-run
registry), `keys` (shared key table, also used by the gpui shell),
plus the pre-existing `app`/`config`/`launch`/`embedded`/`parsers`/
`persist`/`providers`/`scrollback`/`transcript`.

| fn | contract |
|---|---|
| `staap_core_new` / `staap_core_free` | discovery + persistence merge + user config load inside; null-safe free |
| `staap_core_save` | persist config + run list (automatic: pump throttle + close hooks, no Save button); `Config` code on failure |
| `staap_spawn(core, out, cwd, cols, rows)` | fresh `muse` session; null cwd inherits; int code (legacy path; shells use `staap_run_spawn`) |
| `staap_spawn_launch(core, out, cli, cwd, yolo, cols, rows)` | 2D-launch spawn (folder × CLI + one-shot yolo); null/empty cli repeats last/default resolution; int code (legacy path; shells use `staap_run_spawn`) |
| `staap_pump` | dirty gate; null → false (legacy path; shells use `staap_pump_all`/`staap_run_pump`) |
| `staap_write(pty, bytes, len)` | raw input bytes; int code (legacy path; shells use `staap_run_write`) |
| `staap_resize` | null no-op (legacy path; shells use `staap_run_resize`) |
| `staap_screen_text` + `staap_screen_text_free` | owned UTF-8, caller frees (legacy path; shells use `staap_run_screen_text`) |
| `staap_spans_json` | owned styled spans as JSON (rows of `{text,fg,bg,bold,italic,underline}`), freed with `staap_screen_text_free` (legacy path; shells use `staap_run_spans_json`) |
| `staap_status(core, row)` | 0 Attention / 1 Idle / 2 Working; -1 null, -2 out of bounds |
| `staap_session_count` | roster row count; 0 on null (#62) |
| `staap_session_json(core, row)` | owned `ChatSession` JSON; null on null/OOB; freed with `staap_screen_text_free` (#62) |
| `staap_clis_json()` | owned JSON of the autodetected CLI catalog (`AvailableCli` rows in `SUPPORTED_CLIS` order); freed with `staap_screen_text_free` |
| `staap_effective_cli(core, cli)` | owned harness id of the effective CLI (explicit or core resolution); freed with `staap_screen_text_free` |
| `staap_recent_json(core)` | owned JSON string array of folder recents (MRU-first); null core yields `[]`; freed with `staap_screen_text_free` |
| `staap_note_launch(core, cli, cwd)` | record a confirmed FFI-side launch (last-used CLI + folder MRU); int code |
| `staap_max_runs` | shared live-run ceiling (10) |
| `staap_live_count` / `staap_is_live` | attached-run count / per-id live test; null-safe |
| `staap_pump_all` | pump every live run (statuses/links refresh + re-sort); true = repaint |
| `staap_run_spawn` | 2D-launch spawn attached to a new roster row under the cap; row id out; cap refusal names per-run close |
| `staap_run_restart` | restart/resume on the same id (keeps title/links) |
| `staap_run_close` | drop PTY + remove entry (unknown = no-op success) |
| `staap_needs_quit_confirm` | Working/Attention row or live PTY |
| `staap_run_pump` / `staap_run_write` / `staap_run_resize` | per-id pump / write / resize |
| `staap_run_screen_text` / `staap_run_spans_json` | per-id owned snapshots; freed with `staap_screen_text_free` |
| `staap_run_exited` | per-id exit flag |
| `staap_key_encode` | shared key table: bytes out + count, 0 = Keep, -1 = null key |
| `staap_feed_delta` | shared reconciler; owned string or null when current |
| `staap_ansi_render` | shared SGR renderer; null when undecodable |
| `staap_spawn_preview` / `staap_yolo_value` | shared preview copy / tri-state mapping |
| `staap_age_string` / `staap_status_glyph` / `staap_section_title` | shared display strings; freed with `staap_screen_text_free` |
| `staap_clamp_sidebar` | shared 220..480px sidebar clamp |
| `staap_row_matches` / `staap_set_filter` | core-owned filter test / replace (snaps selection) |
| `staap_is_history` | historic membership (no live PTY attached); null-safe |
| `staap_history_expanded` / `staap_set_history_expanded` | History expansion get/set (collapsed default; persist via `staap_core_save`) |
| `staap_selected` / `staap_select` / `staap_select_step` | core-owned selection |
| `staap_last_error` | thread-local message; never null |
| `staap_pty_free` | reaps the child; null no-op |

### C header (`include/staap.h`, hardened #61)

The header is checked in and mirrors `src/ffi.rs` exactly (20 exports;
verified: every `staap_*` in the header is a `T` symbol in
`target/debug/libstaap.a` and vice versa). Regenerate after any
FFI change with [`cbindgen`](https://github.com/mozilla/cbindgen)
(`cbindgen.toml` at the repo root):

```sh
cargo install cbindgen
cbindgen --config cbindgen.toml --crate staap \
  --output include/staap.h
```

`cc -fsyntax-only -std=c99 -Wall include/staap.h` must stay
clean. Error/status codes are pinned by tests, not just docs:
`error_codes_round_trip` (`Ok=0..Config=5`),
`status_codes_map_roster_status` (Attention=0, Idle=1, Working=2, -1
null, -2 out of bounds), and every failure above asserts its
`staap_last_error` message. The public `staap_spawn` success path is covered
hermetically (`public_spawn_success_path`: fake `muse` on `PATH`) and
`staap_core_save` success + failure without touching live config
(`core_save_round_trips_to_scoped_config` via `$STAAP_CONFIG`).
