/* Core C ABI bridge implementation for the Linux GTK4 shell (issue #63).
 *
 * Thin, dependency-free wrappers over `include/staap.h` at the
 * repo root. Mirrors swift/CoreBridge.swift: same owned-handle discipline
 * (core/PTY freed exactly once, strings freed with staap_screen_text_free),
 * same status-code contract (Attention=0, Idle=1, Working=2; negatives
 * are null handle / out of bounds).
 */

#include "core_bridge.h"

#include <stdlib.h>
#include <string.h>

StaapCore *bridge_core_new(void) {
    return staap_core_new();
}

void bridge_core_free(StaapCore *core) {
    staap_core_free(core);
}

/* Copy the core's thread-local message into a malloc'd buffer the caller
 * owns (freed with free()). Never returns NULL. */
static char *copy_last_error(void) {
    const char *msg = staap_last_error();
    if (!msg) {
        msg = "unknown core error";
    }
    size_t n = strlen(msg) + 1;
    char *copy = malloc(n);
    if (copy) {
        memcpy(copy, msg, n);
    }
    return copy;
}

int bridge_core_save(StaapCore *core, char **msg_out) {
    int rc = staap_core_save(core);
    if (rc != 0 && msg_out) {
        *msg_out = copy_last_error();
    }
    return rc;
}

size_t bridge_session_count(const StaapCore *core) {
    return staap_session_count(core);
}

int bridge_status(const StaapCore *core, size_t row) {
    return staap_status(core, row);
}

char *bridge_session_json(const StaapCore *core, size_t row) {
    return staap_session_json(core, row);
}

void bridge_string_free(char *s) {
    staap_screen_text_free(s);
}

StaapPty *bridge_spawn(const StaapCore *core, unsigned cols, unsigned rows,
                    char **msg_out) {
    StaapPty *pty = NULL;
    int rc = staap_spawn(core, &pty, NULL, (uint16_t)cols, (uint16_t)rows);
    if (rc != 0) {
        if (msg_out) {
            *msg_out = copy_last_error();
        }
        return NULL;
    }
    return pty;
}

StaapPty *bridge_spawn_launch(const StaapCore *core, const char *cli,
                            const char *cwd, int yolo, unsigned cols,
                            unsigned rows, char **msg_out) {
    StaapPty *pty = NULL;
    int rc = staap_spawn_launch(core, &pty, cli, cwd, (int32_t)yolo,
                             (uint16_t)cols, (uint16_t)rows);
    if (rc != 0) {
        if (msg_out) {
            *msg_out = copy_last_error();
        }
        return NULL;
    }
    return pty;
}

char *bridge_clis_json(void) {
    return staap_clis_json();
}

char *bridge_recent_json(const StaapCore *core) {
    return staap_recent_json(core);
}

char *bridge_effective_cli(const StaapCore *core, const char *cli) {
    return staap_effective_cli(core, cli);
}

int bridge_note_launch(StaapCore *core, const char *cli, const char *cwd,
                       char **msg_out) {
    int rc = staap_note_launch(core, cli, cwd);
    if (rc != 0 && msg_out) {
        *msg_out = copy_last_error();
    }
    return rc;
}

void bridge_pty_free(StaapPty *pty) {
    staap_pty_free(pty);
}

int bridge_pump(StaapPty *pty) {
    return staap_pump(pty) ? 1 : 0;
}

int bridge_pump_all(StaapCore *core) {
    return staap_pump_all(core) ? 1 : 0;
}

size_t bridge_live_count(const StaapCore *core) {
    return staap_live_count(core);
}

size_t bridge_max_runs(void) {
    return staap_max_runs();
}

int bridge_is_live(const StaapCore *core, const char *id) {
    return staap_is_live(core, id) ? 1 : 0;
}

int bridge_is_history(const StaapCore *core, const char *id) {
    return staap_is_history(core, id) ? 1 : 0;
}

int bridge_history_expanded(const StaapCore *core) {
    return staap_history_expanded(core) ? 1 : 0;
}

void bridge_set_history_expanded(StaapCore *core, int expanded) {
    staap_set_history_expanded(core, expanded);
}

int bridge_run_spawn(StaapCore *core, const char *cli, const char *cwd,
                     int yolo, unsigned cols, unsigned rows, char *id_out,
                     size_t id_cap, char **msg_out) {
    int rc = staap_run_spawn(core, cli, cwd, (int32_t)yolo, (uint16_t)cols,
                          (uint16_t)rows, id_out, id_cap);
    if (rc != 0 && msg_out) {
        *msg_out = copy_last_error();
    }
    return rc;
}

int bridge_run_restart(StaapCore *core, const char *id, unsigned cols,
                       unsigned rows, char **msg_out) {
    int rc = staap_run_restart(core, id, (uint16_t)cols, (uint16_t)rows);
    if (rc != 0 && msg_out) {
        *msg_out = copy_last_error();
    }
    return rc;
}

int bridge_run_close(StaapCore *core, const char *id) {
    return staap_run_close(core, id);
}

int bridge_needs_quit_confirm(const StaapCore *core) {
    return staap_needs_quit_confirm(core) ? 1 : 0;
}

int bridge_run_pump(StaapCore *core, const char *id) {
    return staap_run_pump(core, id) ? 1 : 0;
}

int bridge_run_write(StaapCore *core, const char *id,
                     const unsigned char *data, size_t len, char **msg_out) {
    int rc = staap_run_write(core, id, data, len);
    if (rc != 0 && msg_out) {
        *msg_out = copy_last_error();
    }
    return rc;
}

void bridge_run_resize(StaapCore *core, const char *id, unsigned cols,
                       unsigned rows) {
    staap_run_resize(core, id, (uint16_t)cols, (uint16_t)rows);
}

char *bridge_run_screen_text(const StaapCore *core, const char *id) {
    return staap_run_screen_text(core, id);
}

char *bridge_run_spans_json(const StaapCore *core, const char *id) {
    return staap_run_spans_json(core, id);
}

int bridge_run_exited(const StaapCore *core, const char *id) {
    return staap_run_exited(core, id) ? 1 : 0;
}

int bridge_key_encode(const char *key, const char *key_char, int ctrl,
                      int alt, unsigned char *bytes_out, size_t cap) {
    return staap_key_encode(key, key_char, ctrl, alt, bytes_out, cap);
}

char *bridge_feed_delta(const char *old_text, const char *new_text) {
    /* staap_feed_delta returns a core-owned string (freed with
     * staap_screen_text_free); copy it into a malloc'd buffer so the
     * caller frees uniformly with free(). NULL stays NULL. */
    char *core = staap_feed_delta(old_text, new_text);
    if (!core) {
        return NULL;
    }
    size_t n = strlen(core) + 1;
    char *out = malloc(n);
    if (out) {
        memcpy(out, core, n);
    }
    staap_screen_text_free(core);
    return out;
}

char *bridge_spawn_preview(const char *cli, const char *folder, int yolo) {
    char *core = staap_spawn_preview(cli, folder, (int32_t)yolo);
    if (!core) {
        return NULL;
    }
    size_t n = strlen(core) + 1;
    char *out = malloc(n);
    if (out) {
        memcpy(out, core, n);
    }
    staap_screen_text_free(core);
    return out;
}

int bridge_yolo_value(int selected) {
    return staap_yolo_value((int32_t)selected);
}

char *bridge_age_string(long long now_unix, long long then_unix) {
    return staap_age_string((int64_t)now_unix, (int64_t)then_unix);
}

char *bridge_status_glyph(int code) {
    return staap_status_glyph((int32_t)code);
}

char *bridge_section_title(int code) {
    return staap_section_title((int32_t)code);
}

double bridge_clamp_sidebar(double px) {
    return staap_clamp_sidebar(px);
}

int bridge_row_matches(const StaapCore *core, size_t row, const char *query) {
    return staap_row_matches(core, row, query) ? 1 : 0;
}

void bridge_set_filter(StaapCore *core, const char *query) {
    staap_set_filter(core, query);
}

size_t bridge_selected(const StaapCore *core) {
    return staap_selected(core);
}

void bridge_select(StaapCore *core, size_t row) {
    staap_select(core, row);
}

void bridge_select_step(StaapCore *core, int forward) {
    staap_select_step(core, forward);
}

int bridge_write(StaapPty *pty, const unsigned char *data, size_t len,
                 char **msg_out) {
    int rc = staap_write(pty, data, len);
    if (rc != 0 && msg_out) {
        *msg_out = copy_last_error();
    }
    return rc;
}

void bridge_resize(StaapPty *pty, unsigned cols, unsigned rows) {
    staap_resize(pty, (uint16_t)cols, (uint16_t)rows);
}

char *bridge_screen_text(const StaapPty *pty) {
    return staap_screen_text(pty);
}

char *bridge_last_error(void) {
    return copy_last_error();
}
