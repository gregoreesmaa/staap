/* Core C ABI bridge for the Linux GTK4 shell (issue #63).
 *
 * Thin, dependency-free wrappers over `include/staap.h` at the
 * repo root. Mirrors swift/CoreBridge.swift: same owned-handle discipline
 * (core/PTY freed exactly once, strings freed with staap_screen_text_free),
 * same status-code contract (Attention=0, Idle=1, Working=2; negatives
 * are null handle / out of bounds).
 *
 * Run-registry half (dumb-shell contract): the shell keeps no `live`
 * map of its own — spawns attach to roster rows in the core
 * (`bridge_run_spawn`/`bridge_run_restart`), converse/resize/pump go by
 * row id, and `bridge_pump_all` refreshes statuses/links like the
 * shared pump does
 * pump. Shared helpers (key encoding, feed delta, preview, filter,
 * selection, display strings) are one-line forwards to the core so all
 * three shells behave identically.
 */

#ifndef STAAP_CORE_BRIDGE_H
#define STAAP_CORE_BRIDGE_H

#include <stddef.h>

#include "staap.h"

/* Roster status codes, matching `staap_status`. */
enum {
    STAAP_STATUS_ATTENTION = 0,
    STAAP_STATUS_IDLE = 1,
    STAAP_STATUS_WORKING = 2,
};

/* Owned core handle. NULL on allocation failure (mirrors Core.init?). */
StaapCore *bridge_core_new(void);
void bridge_core_free(StaapCore *core);

/* Persist the core config. Returns 0 on success; on failure returns
 * nonzero and, when `msg_out` is non-NULL, sets it to a malloc'd copy
 * of `staap_last_error` (caller frees with free()). */
int bridge_core_save(StaapCore *core, char **msg_out);

/* Roster snapshot helpers. */
size_t bridge_session_count(const StaapCore *core);
int bridge_status(const StaapCore *core, size_t row);

/* Owned JSON of roster row `row`, or NULL (null handle / OOB / JSON
 * failure). Free with bridge_string_free(). */
char *bridge_session_json(const StaapCore *core, size_t row);

/* Free a string from bridge_session_json / bridge_screen_text. */
void bridge_string_free(char *s);

/* Spawn a fresh session PTY of `cols` x `rows`. Returns NULL on failure
 * and, when `msg_out` is non-NULL, sets it to a malloc'd copy of
 * `staap_last_error`. Free the handle with bridge_pty_free(). */
StaapPty *bridge_spawn(const StaapCore *core, unsigned cols, unsigned rows,
                    char **msg_out);
void bridge_pty_free(StaapPty *pty);

/* 2D-launch spawn (folder x CLI + yolo) of `cols` x `rows`. `cli` is
 * NULL/empty (repeat the core's effective default: last-used, configured,
 * autodetected) or a harness id; `cwd` is NULL (inherit) or a path;
 * `yolo` is tri-state (1 = force on once, -1 = force off once, 0 = the
 * per-agent config default). Returns NULL on failure with `msg_out`
 * set like bridge_spawn. Free the handle with bridge_pty_free(). */
StaapPty *bridge_spawn_launch(const StaapCore *core, const char *cli,
                           const char *cwd, int yolo, unsigned cols,
                           unsigned rows, char **msg_out);

/* Owned JSON of the autodetected CLI catalog ([{id,program,path,
 * available}] in core order) or NULL on allocation failure. Free with
 * bridge_string_free(). Missing CLIs stay listed (available=false). */
char *bridge_clis_json(void);

/* Owned JSON string array of folder recents (MRU-first), or NULL on
 * allocation failure. Free with bridge_string_free(). */
char *bridge_recent_json(const StaapCore *core);

/* Owned harness id of the effective CLI for `cli` (2D launch): the
 * explicit id when non-empty, else the core's last-used / configured /
 * autodetected resolution. NULL `cli` resolves the default. Free with
 * bridge_string_free(). */
char *bridge_effective_cli(const StaapCore *core, const char *cli);

/* Record a confirmed picker launch (last-used CLI + folder MRU), so the
 * next repeat replays it. NULL/empty `cli` keeps the previous CLI; NULL
 * `cwd` records no folder. Returns 0 on success with `msg_out` set like
 * bridge_spawn on failure. */
int bridge_note_launch(StaapCore *core, const char *cli, const char *cwd,
                       char **msg_out);

/* Feed queued output into the emulator. Nonzero when the screen may
 * have changed (the shell's only repaint gate). */
int bridge_pump(StaapPty *pty);

/* Pump every attached run (statuses/links refresh + re-sort inside).
 * Nonzero when anything visible changed — the roster/status repaint
 * gate. Replaces per-shell status polling. */
int bridge_pump_all(StaapCore *core);

/* Number of live (attached) runs; the shared live-run ceiling. */
size_t bridge_live_count(const StaapCore *core);
size_t bridge_max_runs(void);

/* True when the row id owns a live PTY in the registry. */
int bridge_is_live(const StaapCore *core, const char *id);

/* History membership + expansion (core-owned; the shell only renders):
 * historic = no live PTY attached (attached means active, even when
 * the child already exited). Expansion is collapsed by default;
 * setters persist via bridge_core_save (callers save after toggling). */
int bridge_is_history(const StaapCore *core, const char *id);
int bridge_history_expanded(const StaapCore *core);
void bridge_set_history_expanded(StaapCore *core, int expanded);

/* Spawn a 2D-launch session attached to a new roster row, under the
 * shared cap. Returns 0 with the row id in `id_out` (up to `id_cap`
 * bytes incl. NUL); nonzero with `msg_out` set like bridge_core_save. */
int bridge_run_spawn(StaapCore *core, const char *cli, const char *cwd,
                     int yolo, unsigned cols, unsigned rows, char *id_out,
                     size_t id_cap, char **msg_out);

/* Restart an ended run / resume a historic entry on the same id.
 * Returns 0, or nonzero with `msg_out` set. */
int bridge_run_restart(StaapCore *core, const char *id, unsigned cols,
                       unsigned rows, char **msg_out);

/* Close (kill) a run: drops its PTY, removes its entry. Unknown ids are
 * a no-op success. */
int bridge_run_close(StaapCore *core, const char *id);

/* True when quitting deserves a confirmation step. */
int bridge_needs_quit_confirm(const StaapCore *core);

/* Pump / write / resize / screen-text / spans / exit by row id. Write
 * returns 0, or nonzero with `msg_out` set. Screen/spans strings free
 * with bridge_string_free(). */
int bridge_run_pump(StaapCore *core, const char *id);
int bridge_run_write(StaapCore *core, const char *id,
                     const unsigned char *data, size_t len,
                     char **msg_out);
void bridge_run_resize(StaapCore *core, const char *id, unsigned cols,
                       unsigned rows);
char *bridge_run_screen_text(const StaapCore *core, const char *id);
char *bridge_run_spans_json(const StaapCore *core, const char *id);
int bridge_run_exited(const StaapCore *core, const char *id);

/* Encode one logical keypress into child bytes (the single shared key
 * table). Returns the byte count in `bytes_out` (up to `cap` bytes), 0
 * when the native control keeps the key, -1 on null key. */
int bridge_key_encode(const char *key, const char *key_char, int ctrl,
                      int alt, unsigned char *bytes_out, size_t cap);

/* Feed text advancing a view showing `old` to also show `new`
 * (snapshot→stream reconciler). Malloc'd; NULL when current. Free with
 * free(). */
char *bridge_feed_delta(const char *old_text, const char *new_text);

/* One-line spawn preview (`runs: muse in ~/api + yolo`). Malloc'd; free
 * with free(). */
char *bridge_spawn_preview(const char *cli, const char *folder, int yolo);

/* Tri-state yolo int from a segmented-control index. */
int bridge_yolo_value(int selected);

/* Human age (`just now`, `5m ago`, ...), status glyph (`●`/`◐`/`○`),
 * section header (`Needs input`/`Idle`/`Working`). Malloc'd; free with
 * bridge_string_free(). */
char *bridge_age_string(long long now_unix, long long then_unix);
char *bridge_status_glyph(int code);
char *bridge_section_title(int code);

/* Clamp a sidebar width into the shared 220..480px range. */
double bridge_clamp_sidebar(double px);

/* Filter + selection (core-owned): row match test, replace the filter
 * (snaps selection), selected index, move/step the selection. */
int bridge_row_matches(const StaapCore *core, size_t row, const char *query);
void bridge_set_filter(StaapCore *core, const char *query);
size_t bridge_selected(const StaapCore *core);
void bridge_select(StaapCore *core, size_t row);
void bridge_select_step(StaapCore *core, int forward);

/* Forward raw bytes (already key-encoded by the shell) to the child.
 * Returns 0 on success; on failure returns nonzero with `msg_out`
 * set like bridge_core_save. */
int bridge_write(StaapPty *pty, const unsigned char *data, size_t len,
                 char **msg_out);

void bridge_resize(StaapPty *pty, unsigned cols, unsigned rows);

/* Owned plain-text snapshot of the emulated screen (NULL on null
 * handle). Free with bridge_string_free(). */
char *bridge_screen_text(const StaapPty *pty);

/* Copy of the thread-local last-error message (never NULL;
 * caller frees with free()). */
char *bridge_last_error(void);

#endif /* STAAP_CORE_BRIDGE_H */
