#ifndef STAAP_H
#define STAAP_H

/* Warning: regenerate with cbindgen, do not hand-edit beyond the checked-in snapshot. */

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <stddef.h>

/**
 * Max parsed links retained per list per run (PRs and related alike).
 * Cap-not-drop display keeps the full story bounded: feeding 10k links
 * keeps memory flat and surfaces [`ChatSession::links_truncated`].
 */
#define MAX_STORED_LINKS 50

/**
 * Max parsed-link rows shown per run in the sessions panel; the rest
 * fold behind an `N more` disclosure ([`visible_links`]).
 */
#define MAX_VISIBLE_LINKS 20

/**
 * A run counts as actively working while it produced output recently.
 */
#define WORKING_WINDOW_SECS 60

/**
 * Rows moved by one PgUp/PgDn step. List state carries no viewport
 * height, so paging is a fixed step, clamped at the ends.
 */
#define PAGE_STEP 5

/**
 * Lines of scrollback retained by the vt100 emulator.
 */
#define SCROLLBACK_LINES 2000

/**
 * Cap for the persisted most-recently-used folder list (issue #29 family:
 * bounded display over unbounded data — same rule as link caps).
 */
#define MAX_RECENT_FOLDERS 10

/**
 * Lines of output retained for the pager (mirrors the vt100 buffer size).
 */
#define MAX_RETAINED_LINES 2000

/**
 * Live-run ceiling shared by every shell (was `gui::runs::MAX_LIVE_RUNS`,
 * hard-coded `10` in two native shells, unbounded on macOS).
 */
#define MAX_LIVE_RUNS 10

/**
 * Pump cadence shared by every shell (50 ms, matching the gpui loop and
 * all three native tickers).
 */
#define PUMP_INTERVAL_MS 50

/**
 * Bounds for the resizable sidebar (Windows grip + GTK split view share
 * these; SwiftUI uses the system default sidebar width).
 */
#define SIDEBAR_MIN_PX 220.0

#define SIDEBAR_MAX_PX 480.0

/**
 * Keyboard nudge step for the resize grip.
 */
#define SIDEBAR_KEY_STEP_PX 8.0

/**
 * Max tail bytes the provider reads from `session.jsonl`.
 */
#define MAX_TAIL_BYTES (64 * 1024)

/**
 * Max messages kept per session (most recent win).
 */
#define MAX_MESSAGES 200

/**
 * Max chars kept per message.
 */
#define MAX_MESSAGE_CHARS 4000

/**
 * Max chars for derived single-line titles.
 */
#define MAX_TITLE_CHARS 60

/**
 * Opaque core handle: owns the run registry (roster `App` + live PTYs).
 * Native shells drive the roster, spawn, pump, and persistence through
 * this handle, so every shell shares one policy instead of reimplementing
 * it per OS.
 */
typedef struct StaapCore StaapCore;

/**
 * Opaque PTY handle: owns one [`EmbeddedPty`].
 */
typedef struct StaapPty StaapPty;

/**
 * Build a core the way the app starts: discovered provider sessions
 * merged over persisted rows, plus the stored user config. Discovery,
 * load, and config all degrade to empty/default (never fail), so this
 * constructor is infallible barring allocation.
 *
 * # Safety
 * No arguments; always safe to call. Free with [`staap_core_free`].
 */
struct StaapCore *staap_core_new(void);

/**
 * Free a core created by [`staap_core_new`]. Null is a no-op.
 *
 * # Safety
 * `core` must be null or a pointer from [`staap_core_new`], used at most once.
 */
void staap_core_free(struct StaapCore *core);

/**
 * Persist core state (user config + run list). Shells call this on a
 * timer tick while dirty, on close, and after closing a run — never via
 * a manual Save button (removed from every shell: persistence is
 * automatic, like the gpui shell's throttled pump persist). Returns an
 * [`StaapError`] code.
 *
 * # Safety
 * `core` must be null or a live pointer from [`staap_core_new`].
 */
int staap_core_save(const struct StaapCore *core);

/**
 * Spawn a fresh session PTY (`muse` + configured extra flags) of
 * `cols` x `rows`. `cwd` is null (inherit) or a NUL-terminated UTF-8
 * path; the new handle is written to `*out`. Returns an [`StaapError`] code.
 *
 * # Safety
 * `core`/`out` must be non-null live pointers; `cwd` must be null or a
 * valid NUL-terminated C string. Free the handle with [`staap_pty_free`].
 */
int staap_spawn(const struct StaapCore *core,
                struct StaapPty **out,
                const char *cwd,
                uint16_t cols,
                uint16_t rows);

/**
 * Spawn a 2D-launch session PTY (folder × CLI + yolo) of `cols` x `rows`.
 * `cli` names the harness id (`muse`, `claude`, …; null/empty repeats the
 * last-used/default resolution), `cwd` is null (inherit) or a path,
 * `yolo` is tri-state: >0 forces the canonical yolo flag on for this
 * spawn only, <0 forces it off, 0 follows the per-agent config default.
 * Returns an [`StaapError`] code; the handle lands in `*out`.
 *
 * # Safety
 * `core`/`out` must be non-null live pointers; `cli`/`cwd` must be null
 * or valid NUL-terminated C strings. Free the handle with [`staap_pty_free`].
 */
int staap_spawn_launch(const struct StaapCore *core,
                       struct StaapPty **out,
                       const char *cli,
                       const char *cwd,
                       int yolo,
                       uint16_t cols,
                       uint16_t rows);

/**
 * Owned harness id of the effective CLI for `cli` (2D launch): the
 * explicit id when non-empty, else the core's last-used / configured /
 * autodetected resolution (same rule as [`staap_spawn_launch`]). Lets
 * shells label a repeat-last spawn before starting it. Null `cli`
 * means "resolve the default". Free with [`staap_screen_text_free`].
 *
 * # Safety
 * `core` must be null or a live pointer from [`staap_core_new`]; `cli`
 * must be null or a valid NUL-terminated C string.
 */
char *staap_effective_cli(const struct StaapCore *core, const char *cli);

/**
 * Owned JSON of the autodetected CLI catalog (2D launch):
 * `[{"id","program","path"|null,"available"}]` in
 * [`launch::SUPPORTED_CLIS`] order. Never null on allocation success;
 * free with [`staap_screen_text_free`].
 *
 * # Safety
 * Always safe: allocates a fresh string, takes no handles.
 */
char *staap_clis_json(void);

/**
 * Owned JSON of the persisted folder recents (2D launch): a string array,
 * MRU-first. Null core yields an empty list, never UB; free with
 * [`staap_screen_text_free`].
 *
 * # Safety
 * `core` must be null or a live pointer from [`staap_core_new`].
 */
char *staap_recent_json(const struct StaapCore *core);

/**
 * Record a confirmed launch (2D launch): refreshes last-used CLI + folder
 * MRU in the core config. Null core is a null error; null/empty `cli`
 * keeps the previous CLI; null `cwd` records no folder.
 *
 * # Safety
 * `core` must be non-null and live; `cli`/`cwd` must be null or valid
 * NUL-terminated C strings.
 */
int staap_note_launch(struct StaapCore *core, const char *cli, const char *cwd);

/**
 * Feed queued output into the emulator. Returns true when new output
 * arrived or the child newly exited (the only dirty gate the shell
 * needs to repaint). Null is false, never UB.
 *
 * # Safety
 * `pty` must be null or a live pointer from [`staap_spawn`].
 */
bool staap_pump(struct StaapPty *pty);

/**
 * Forward raw bytes (already key-encoded by the shell) to the child.
 * Returns an [`StaapError`] code.
 *
 * # Safety
 * `pty` must be non-null and live; `data` must point to `len` readable
 * bytes when `len > 0` (null `data` with `len == 0` is a no-op success).
 */
int staap_write(struct StaapPty *pty, const uint8_t *data, uintptr_t len);

/**
 * Resize the PTY and the emulator grid. Null is a no-op.
 *
 * # Safety
 * `pty` must be null or a live pointer from [`staap_spawn`].
 */
void staap_resize(struct StaapPty *pty, uint16_t cols, uint16_t rows);

/**
 * Owned UTF-8 snapshot of the emulated screen. Returns null on null
 * handle; otherwise a freshly allocated string the caller frees with
 * [`staap_screen_text_free`].
 *
 * # Safety
 * `pty` must be null or a live pointer from [`staap_spawn`].
 */
char *staap_screen_text(const struct StaapPty *pty);

/**
 * Free a string returned by [`staap_screen_text`] or [`staap_spans_json`].
 * Null is a no-op.
 *
 * # Safety
 * `s` must be null or a pointer from [`staap_screen_text`]/[`staap_spans_json`],
 * used at most once.
 */
void staap_screen_text_free(char *s);

/**
 * Owned styled spans of the emulated screen as JSON:
 * `[[{"text":..,"fg":[r,g,b]|null,"bg":..,"bold":..,"italic":..,
 * "underline":..}]]` (one array per grid row). Returns null on null
 * handle or (unreachable in practice) JSON failure; free with
 * [`staap_screen_text_free`].
 *
 * # Safety
 * `pty` must be null or a live pointer from [`staap_spawn`].
 */
char *staap_spans_json(const struct StaapPty *pty);

/**
 * Roster status of row `row`: 0 = Attention, 1 = Idle, 2 = Working.
 * Returns -1 on null handle, -2 when `row` is out of bounds.
 *
 * # Safety
 * `core` must be null or a live pointer from [`staap_core_new`].
 */
int staap_status(const struct StaapCore *core, uintptr_t row);

/**
 * Number of roster rows in the core. Null core yields 0 (never UB).
 *
 * # Safety
 * `core` must be null or a live pointer from [`staap_core_new`].
 */
uintptr_t staap_session_count(const struct StaapCore *core);

/**
 * Owned JSON of roster row `row` (a serialized [`ChatSession`]; the
 * documented roster crossing from the seam sketch). Returns null on
 * null handle, out-of-bounds row, or (unreachable in practice) JSON
 * failure; free with [`staap_screen_text_free`].
 *
 * # Safety
 * `core` must be null or a live pointer from [`staap_core_new`].
 */
char *staap_session_json(const struct StaapCore *core, uintptr_t row);

/**
 * Last error message for this thread (UTF-8, NUL-terminated). Never
 * null; valid until the next failing `staap_*` call on this thread.
 *
 * # Safety
 * Always safe: returns a thread-local pointer, never transfers ownership.
 */
const char *staap_last_error(void);

/**
 * Free a PTY created by [`staap_spawn`] (reaps the child). Null is a no-op.
 *
 * # Safety
 * `pty` must be null or a pointer from [`staap_spawn`], used at most once.
 */
void staap_pty_free(struct StaapPty *pty);

/**
 * Live-run ceiling shared by every shell (was three copies: `10` on
 * Linux/Windows, unbounded on macOS).
 */
uintptr_t staap_max_runs(void);

/**
 * Number of live (attached) PTYs in the registry. Null core yields 0.
 *
 * # Safety
 * `core` must be null or a live pointer from [`staap_core_new`].
 */
uintptr_t staap_live_count(const struct StaapCore *core);

/**
 * True when the row id owns a live PTY in the registry. Null-safe:
 * null core/id yields false.
 *
 * # Safety
 * `core` must be null or a live pointer from [`staap_core_new`]; `id` must
 * be null or a valid NUL-terminated C string.
 */
bool staap_is_live(const struct StaapCore *core, const char *id);

/**
 * Pump every live run: feed output, rescan attention + links for changed
 * runs, reclassify statuses, re-sort pinned to selection. Returns true
 * when anything visible changed — the shell's only repaint gate (and its
 * roster/status refresh gate: statuses now actually move, unlike the
 * launch-snapshot rows the old shells polled). Null is false, never UB.
 *
 * # Safety
 * `core` must be null or a live pointer from [`staap_core_new`].
 */
bool staap_pump_all(struct StaapCore *core);

/**
 * Spawn a 2D-launch session and attach it to a new roster row, under the
 * shared live-run cap. On success the row id is written to `id_out`
 * (up to `id_cap` bytes incl. NUL; truncated otherwise) and an
 * [`StaapError::Ok`] code returns. At the cap the oldest-exited run is
 * reaped first; when every live run is still running this refuses with
 * [`StaapError::Spawn`] and a message naming per-run close. `cli`/`cwd`
 * follow the [`staap_spawn_launch`] convention; `yolo` is tri-state.
 *
 * # Safety
 * `core` must be a live pointer from [`staap_core_new`]; `id_out` must point
 * to `id_cap` writable bytes; `cli`/`cwd` must be null or valid
 * NUL-terminated C strings.
 */
int staap_run_spawn(struct StaapCore *core,
                    const char *cli,
                    const char *cwd,
                    int yolo,
                    uint16_t cols,
                    uint16_t rows,
                    char *id_out,
                    uintptr_t id_cap);

/**
 * Spawn a PTY and attach it to an existing roster row (restart a dead
 * run, resume a historic entry): drops the dead PTY if any, spawns the
 * row's spawn kind on the same id, attaches on success. Unknown or live
 * rows report [`StaapError::Spawn`]; the row keeps its title and links.
 *
 * # Safety
 * `core` must be live; `id` must be a valid NUL-terminated C string.
 */
int staap_run_restart(struct StaapCore *core, const char *id, uint16_t cols, uint16_t rows);

/**
 * Close (kill) a run: drop its live PTY and remove its entry. Unknown
 * ids are a no-op success. Shells persist via `staap_core_save`
 * hooks (close autosaves) so the closed run does not resurrect.
 *
 * # Safety
 * `core` must be null or live; `id` must be null or a valid C string.
 */
int staap_run_close(struct StaapCore *core, const char *id);

/**
 * True when quitting deserves a confirmation step: any Working/Attention
 * row or any live (non-exited) PTY. Null core yields false.
 *
 * # Safety
 * `core` must be null or a live pointer from [`staap_core_new`].
 */
bool staap_needs_quit_confirm(const struct StaapCore *core);

/**
 * Pump one attached run by id (feed output into its emulator). Returns
 * true when the screen may have changed. Unknown ids and nulls yield
 * false, never UB.
 *
 * # Safety
 * `core`/`id` follow the [`staap_is_live`] conventions.
 */
bool staap_run_pump(struct StaapCore *core, const char *id);

/**
 * Forward raw bytes (already key-encoded by the shell) to an attached
 * run's child. Returns an [`StaapError`] code.
 *
 * # Safety
 * `core` must be non-null and live; `id` a valid C string; `data` must
 * point to `len` readable bytes when `len > 0`.
 */
int staap_run_write(struct StaapCore *core, const char *id, const uint8_t *data, uintptr_t len);

/**
 * Resize an attached run's PTY and emulator grid. Unknown ids and nulls
 * are no-ops.
 *
 * # Safety
 * `core`/`id` follow the [`staap_is_live`] conventions.
 */
void staap_run_resize(struct StaapCore *core, const char *id, uint16_t cols, uint16_t rows);

/**
 * Owned UTF-8 snapshot of an attached run's emulated screen. Null on
 * null handle or unknown id; free with [`staap_screen_text_free`].
 *
 * # Safety
 * `core`/`id` follow the [`staap_is_live`] conventions.
 */
char *staap_run_screen_text(const struct StaapCore *core, const char *id);

/**
 * Owned styled spans of an attached run's screen as JSON (same shape as
 * [`staap_spans_json`]). Null on null handle or unknown id; free with
 * [`staap_screen_text_free`].
 *
 * # Safety
 * `core`/`id` follow the [`staap_is_live`] conventions.
 */
char *staap_run_spans_json(const struct StaapCore *core, const char *id);

/**
 * True once an attached run's child has exited. Unknown ids and nulls
 * yield false.
 *
 * # Safety
 * `core`/`id` follow the [`staap_is_live`] conventions.
 */
bool staap_run_exited(const struct StaapCore *core, const char *id);

/**
 * Encode one logical keypress into child bytes. `key`/`key_char` are
 * NUL-terminated UTF-8 (`key_char` may be null); `ctrl`/`alt` are 0/1.
 * On `Forward` the bytes are written to `bytes_out` (up to `cap` bytes)
 * and the count returns; on `Keep` 0 returns (leave to the native
 * control). `key` null returns -1. This is the single key table every
 * shell shares (ports of per-shell tables are deleted).
 *
 * # Safety
 * `key`/`key_char` must be null or valid C strings; `bytes_out` must
 * point to `cap` writable bytes when `cap > 0`.
 */
int staap_key_encode(const char *key,
                     const char *key_char,
                     int ctrl,
                     int alt,
                     uint8_t *bytes_out,
                     uintptr_t cap);

/**
 * Snapshot-to-stream feed reconciler for C shells: computes the text
 * that advances a view showing `old_text` to also show `new_text`, or
 * null when the view is already current. Either argument may be null
 * (treated as ""). The result is freshly allocated; free it with
 * [`staap_screen_text_free`].
 *
 * This is the shared [`crate::shell_shared::feed_delta`] (append-only
 * suffix hot path, scroll overlap, clear-and-replay redraw, CRLF
 * normalization), replacing the per-shell `feed.c` ports.
 *
 * # Safety
 * `old_text`/`new_text` must be null or valid NUL-terminated UTF-8
 * C strings.
 */
char *staap_feed_delta(const char *old_text, const char *new_text);

/**
 * True (1) when a roster row with this title/project/id passes the
 * sidebar `query` (case-insensitive substring; blank query passes
 * everything), else false (0). Null pointers mean empty strings; invalid
 * UTF-8 in any argument reports false and records a message.
 *
 * # Safety
 * Each argument must be null or a valid NUL-terminated C string.
 */
int staap_roster_matches(const char *title, const char *project, const char *id, const char *query);

/**
 * Owned glanceable age label for `then_secs` (unix seconds) relative to
 * `now_secs`: `just now` / `Nm ago` / `Nh ago` / `Nd ago`. Free with
 * [`staap_screen_text_free`].
 *
 * # Safety
 * Always safe: pure computation, no pointers read.
 */
char *staap_relative_age(int64_t now_secs, int64_t then_secs);

/**
 * Total link count (PR + related) of roster row `row`, or -1 on null
 * handle / out-of-bounds row. Lets shells show the same link badge
 * without parsing `pr_links` / `related_links` themselves.
 *
 * # Safety
 * `core` must be null or a live pointer from [`staap_core_new`].
 */
int staap_link_count(const struct StaapCore *core, uintptr_t row);

/**
 * Unix seconds of `last_active` for roster row `row`, or -1 on null
 * handle / out-of-bounds row. Shells feed it to [`staap_relative_age`]
 * with their own clock, so all shells render the same relative label.
 *
 * # Safety
 * `core` must be null or a live pointer from [`staap_core_new`].
 */
int64_t staap_last_active(const struct StaapCore *core, uintptr_t row);

/**
 * Render an `staap_spans_json` document to an SGR stream
 * (`shell_shared::render_ansi_json`), or null when it does not decode
 * (the pump then falls back to the plain-text snapshot). Free with
 * [`staap_screen_text_free`].
 *
 * # Safety
 * `json` must be null or a valid C string.
 */
char *staap_ansi_render(const char *json);

/**
 * One-line spawn preview (`runs: muse in ~/api + yolo`). `cli`/`folder`
 * may be null (= default/inherit); `yolo` is the tri-state int. Free with
 * [`staap_screen_text_free`].
 *
 * # Safety
 * `cli`/`folder` must be null or valid C strings.
 */
char *staap_spawn_preview(const char *cli, const char *folder, int yolo);

/**
 * Tri-state yolo int from a segmented-control index (1 = force on,
 * 2 = force off, else config default).
 */
int staap_yolo_value(int selected);

/**
 * Human age for `last_active` (`just now`, `5m ago`, …). Free with
 * [`staap_screen_text_free`].
 */
char *staap_age_string(int64_t now_unix, int64_t then_unix);

/**
 * Non-color status marker for a status code (`●`/`◐`/`○`).
 * Free with [`staap_screen_text_free`].
 */
char *staap_status_glyph(int code);

/**
 * Section header for a status code (`Needs input`/`Idle`/`Working`).
 * Free with [`staap_screen_text_free`].
 */
char *staap_section_title(int code);

/**
 * Clamp a sidebar width into the shared 220..480px range.
 */
double staap_clamp_sidebar(double px);

/**
 * True when roster row `row` passes the sidebar filter `query`
 * (case-insensitive substring over title/project/id). Null-safe: null
 * core/query yields false; out-of-bounds yields false.
 *
 * # Safety
 * `core` must be null or live; `query` must be null or a valid C string.
 */
bool staap_row_matches(const struct StaapCore *core, uintptr_t row, const char *query);

/**
 * Replace the title filter, snapping the selection into the matches.
 * Null query clears. Always persists via [`staap_core_save`] semantics? No —
 * callers persist on close; this only mutates in-memory state.
 *
 * # Safety
 * `core` must be null or live; `query` must be null or a valid C string.
 */
void staap_set_filter(struct StaapCore *core, const char *query);

/**
 * Selected roster row index (the shell highlights this row).
 *
 * # Safety
 * `core` must be null or a live pointer from [`staap_core_new`].
 */
uintptr_t staap_selected(const struct StaapCore *core);

/**
 * Move selection to row `row` (clamped into range). Resets nothing else;
 * the shell repaints the detail from the newly selected row.
 *
 * # Safety
 * `core` must be null or live.
 */
void staap_select(struct StaapCore *core, uintptr_t row);

/**
 * Step selection next/prev (`forward` nonzero = next), wrapping within
 * the current filter matches — the same rule as the gpui list keys.
 *
 * # Safety
 * `core` must be null or live.
 */
void staap_select_step(struct StaapCore *core, int forward);

/**
 * True when roster row id `id` is historic (no live PTY attached —
 * attached means active, even when the child already exited).
 * Null-safe: null core/id yields false; unknown ids yield true.
 *
 * # Safety
 * `core` must be null or live; `id` must be null or a valid C string.
 */
bool staap_is_history(const struct StaapCore *core, const char *id);

/**
 * True when the History section renders expanded. Collapsed by default;
 * null core yields false. Callers persist via [`staap_core_save`].
 *
 * # Safety
 * `core` must be null or a live pointer from [`staap_core_new`].
 */
bool staap_history_expanded(const struct StaapCore *core);

/**
 * Set the History expansion state (`expanded` nonzero = expanded).
 * Null core is a no-op. Callers persist via [`staap_core_save`].
 *
 * # Safety
 * `core` must be null or live.
 */
void staap_set_history_expanded(struct StaapCore *core, int expanded);

#endif  /* STAAP_H */
