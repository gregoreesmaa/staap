/* Staap: Linux GTK4/libadwaita shell over the core C ABI.
 *
 * Dumb renderer over the core run registry: the core owns the roster,
 * selection, filter, PTYs, statuses, key table, feed reconciler, and
 * persistence — this shell owns widgets, event wiring, and byte
 * transport only. Every run feature goes through `core_bridge.h` (which
 * wraps `include/staap.h`).
 *
 * Parity wiring (same contract as the macOS/Windows shells):
 *   roster      core registry rows (staap_session_count/json live),
 *               grouped working → idle (needs-input shares idle,
 *               issue #105), history collapsed in its own toggle
 *   spawn       header New run (repeat-last) + ▾ picker → staap_run_spawn
 *   restart     header Restart / Ctrl+R → staap_run_restart (same id)
 *   close       header Close run / Ctrl+W → staap_run_close + autosave
 *   converse    key event → staap_key_encode → staap_run_write; pump →
 *               staap_feed_delta → VTE
 *   copy/paste  native VTE selection + Ctrl+Shift+C/V + right-click menu
 *   scroll      VTE scrollback (capped) in a GtkScrolledWindow
 *   search      sidebar filter (core-owned text) + terminal find (Ctrl+F)
 *   history     rows show project/harness/age glyphs, restored every
 *               launch; ended rows offer restart inline
 *   theme       follows the system appearance (no manual override)
 *   persist     automatic (throttled pump autosave + close hook)
 *
 * The core owns its emulator; the VTE widget owns a second one fed with
 * snapshot deltas (`bridge_feed_delta`, the shared reconciler). No PTY is
 * ever spawned inside VTE: typed keys are core-encoded and forwarded with
 * staap_run_write, and echoed output arrives via the pump. This keeps
 * exactly one line discipline (the core's) so nothing double-echoes.
 */

#define _POSIX_C_SOURCE 200809L /* strdup under strict C11 */

#include <ctype.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <adwaita.h>
#include <gtk/gtk.h>
#include <vte/vte.h>

#include "core_bridge.h"

/* Bounded per-session VTE scrollback (local-only trust + bounded growth:
 * an accumulate-forever buffer would leak memory over long agent runs).
 * Named for the widget (not SCROLLBACK_LINES: that is the core vt100
 * emulator's retention over staap.h, a different buffer). */
#define VTE_SCROLLBACK_LINES 10000
/* Roster/status refresh rides on the same 50ms pump tick as the Swift shell. */
#define PUMP_MS 50
#define DEFAULT_COLS 80
#define DEFAULT_ROWS 24

/* ------------------------------------------------------------------ */
/* Minimal JSON field extraction (roster rows are ChatSession objects).
 * Used only to pull display fields for native widgets; matching,
 * grouping, preview, and key rules all live in the core. strndup is
 * POSIX (present under _POSIX_C_SOURCE above); MSVC builds use the
 * local copy below. */
#ifdef _MSC_VER
static char *staap_strndup(const char *s, size_t n) {
    char *out = malloc(n + 1);
    if (!out) {
        return NULL;
    }
    memcpy(out, s, n);
    out[n] = '\0';
    return out;
}
#define strndup staap_strndup
#endif
/* ------------------------------------------------------------------ */

/* Find the value of top-level string key `key` in a flat JSON object.
 * Returns a malloc'd unescaped string, or NULL when absent/non-string. */
static char *json_string(const char *json, const char *key) {
    char pat[128];
    snprintf(pat, sizeof pat, "\"%s\"", key);
    const char *p = json;
    while ((p = strstr(p, pat)) != NULL) {
        p += strlen(pat);
        while (*p == ' ' || *p == '\t' || *p == '\n' || *p == '\r') {
            p++;
        }
        if (*p != ':') {
            continue;
        }
        p++;
        while (*p == ' ' || *p == '\t' || *p == '\n' || *p == '\r') {
            p++;
        }
        if (*p != '"') {
            return NULL; /* present but not a string (e.g. null cwd) */
        }
        p++;
        /* First pass: decoded length. */
        size_t cap = 64, len = 0;
        char *out = malloc(cap);
        if (!out) {
            return NULL;
        }
        int ok = 1;
        for (; *p && *p != '"'; p++) {
            char c = *p;
            if (c == '\\') {
                p++;
                switch (*p) {
                case '"': c = '"'; break;
                case '\\': c = '\\'; break;
                case '/': c = '/'; break;
                case 'b': c = '\b'; break;
                case 'f': c = '\f'; break;
                case 'n': c = '\n'; break;
                case 'r': c = '\r'; break;
                case 't': c = '\t'; break;
                case 'u': {
                    /* Basic-plane \uXXXX -> UTF-8 (enough for titles). */
                    unsigned cp = 0;
                    for (int i = 1; i <= 4; i++) {
                        char h = p[i];
                        cp <<= 4;
                        if (h >= '0' && h <= '9') {
                            cp |= (unsigned)(h - '0');
                        } else if (h >= 'a' && h <= 'f') {
                            cp |= (unsigned)(h - 'a' + 10);
                        } else if (h >= 'A' && h <= 'F') {
                            cp |= (unsigned)(h - 'A' + 10);
                        } else {
                            ok = 0;
                        }
                    }
                    if (!ok) {
                        break;
                    }
                    p += 4;
                    char enc[3];
                    int n = 0;
                    if (cp < 0x80) {
                        enc[n++] = (char)cp;
                    } else if (cp < 0x800) {
                        enc[n++] = (char)(0xC0 | (cp >> 6));
                        enc[n++] = (char)(0x80 | (cp & 0x3F));
                    } else {
                        enc[n++] = (char)(0xE0 | (cp >> 12));
                        enc[n++] = (char)(0x80 | ((cp >> 6) & 0x3F));
                        enc[n++] = (char)(0x80 | (cp & 0x3F));
                    }
                    for (int i = 0; i < n; i++) {
                        if (len + 1 >= cap) {
                            cap *= 2;
                            char *nb = realloc(out, cap);
                            if (!nb) {
                                ok = 0;
                                break;
                            }
                            out = nb;
                        }
                        out[len++] = enc[i];
                    }
                    continue;
                }
                default:
                    c = *p; /* lenient: keep unknown escapes literally */
                    break;
                }
                if (*p == '\0' || !ok) {
                    break;
                }
            }
            if (!ok) {
                break;
            }
            if (len + 1 >= cap) {
                cap *= 2;
                char *nb = realloc(out, cap);
                if (!nb) {
                    ok = 0;
                    break;
                }
                out = nb;
            }
            out[len++] = c;
        }
        if (!ok || *p != '"') {
            free(out);
            return NULL;
        }
        out[len] = '\0';
        return out;
    }
    return NULL;
}

static long long json_int(const char *json, const char *key, long long dflt) {
    char pat[128];
    snprintf(pat, sizeof pat, "\"%s\"", key);
    const char *p = strstr(json, pat);
    if (!p) {
        return dflt;
    }
    p = strchr(p + strlen(pat), ':');
    if (!p) {
        return dflt;
    }
    return atoll(p + 1);
}

/* ------------------------------------------------------------------ */
/* Shell state (dumb renderer: the core registry owns everything else). */
/* ------------------------------------------------------------------ */

typedef struct {
    AdwApplication *app;
    StaapCore *core;
    /* Display cache: badge label per row id, rebuilt by refresh_roster.
     * Rows, selection, filter, live-ness, and statuses all live in the
     * core run registry — this shell keeps no roster copy and no live
     * PTY map (dumb-shell contract: render what the core reports). */
    GHashTable *row_widgets;   /* id -> GtkWidget* (badge label) */
    GtkListBox *list;
    GtkSearchEntry *filter;
    /* History affordance (dumb renderer: membership + expansion live in
     * the core; this header only toggles and labels). Rows stay siblings
     * in `list` (sorted last), like the macOS shell. */
    GtkExpander *history_expander;
    VteTerminal *term;
    AdwWindowTitle *header_title;
    GtkButton *spawn_btn;
    GtkRevealer *find_revealer;
    GtkSearchEntry *find_entry;
    GtkScrolledWindow *term_scroll;
    int cols, rows_grid;
    /* Feed state: snapshot already shown per selected row (the core owns
     * the screens; the shell only reconciles view deltas). */
    char *fed_id;
    char *fed_text;
} Shell;

/* Status header text via the core (single copy: "Needs input"/"Working"
 * replaces the old "Active" label). The returned string is core-owned:
 * free with bridge_string_free(). */
static char *section_for(int st) {
    return bridge_section_title(st);
}

/* Human age via the core (single copy of the bucket rule). */
static char *age_via_core(long long epoch) {
    return bridge_age_string((long long)time(NULL), epoch);
}

/* Non-color status marker via the core (`●`/`◐`/`○`): rows are never
 * color-only, matching the Windows glyphs. */
static char *glyph_via_core(int st) {
    return bridge_status_glyph(st);
}

/* Follow the system appearance (no manual theme override): the VTE
 * palette tracks libadwaita's dark bit, like the macOS shell follows
 * the system appearance and the Windows shell uses ThemeResources. */
static void apply_system_theme(Shell *sh) {
    AdwStyleManager *sm = adw_style_manager_get_default();
    adw_style_manager_set_color_scheme(sm, ADW_COLOR_SCHEME_DEFAULT);
    GdkRGBA fg, bg;
    if (!adw_style_manager_get_dark(sm)) {
        gdk_rgba_parse(&fg, "#1e1e1e");
        gdk_rgba_parse(&bg, "#ffffff");
    } else {
        gdk_rgba_parse(&fg, "#e6e6e6");
        gdk_rgba_parse(&bg, "#1e1e2e");
    }
    vte_terminal_set_colors(sh->term, &fg, &bg, NULL, 0);
}

/* ------------------------------------------------------------------ */
/* Roster. */
/* ------------------------------------------------------------------ */

/* Rebuild the roster list from the core registry (rows, statuses,
 * live-ness all come from the core now — no launch snapshot, no local
 * row cache). Rows carry their JSON + status + id as widget data; the
 * filter/sort callbacks read the same core state, so grouping is the
 * shared working → idle → history order (issue #105). */
static void refresh_roster(Shell *sh);

/* Subtitle: selection context + live-ness + attention count, all from
 * the core registry. */
static void reload_statuses(Shell *sh) {
    size_t n = bridge_session_count(sh->core);
    size_t sel = bridge_selected(sh->core);
    int attention = 0;
    for (size_t i = 0; i < n; i++) {
        if (bridge_status(sh->core, i) == STAAP_STATUS_ATTENTION) {
            attention++;
        }
    }
    char *sub;
    if (sel < n) {
        char *json = bridge_session_json(sh->core, sel);
        char *project = json_string(json ? json : "{}", "project");
        char *harness = json_string(json ? json : "{}", "harness");
        char *id = json_string(json ? json : "{}", "id");
        long long last = json_int(json ? json : "{}", "last_active", 0);
        bridge_string_free(json);
        char *age = age_via_core(last);
        int live = (id && bridge_is_live(sh->core, id)) ? 1 : 0;
        /* Badge labels follow the registry statuses on every tick. */
        for (size_t i = 0; i < n; i++) {
            char *rj = bridge_session_json(sh->core, i);
            char *rid = json_string(rj ? rj : "{}", "id");
            bridge_string_free(rj);
            if (!rid) {
                continue;
            }
            GtkWidget *badge = g_hash_table_lookup(sh->row_widgets, rid);
            if (badge) {
                int st = bridge_status(sh->core, i);
                char *hdr = section_for(st < 0 ? 1 : st);
                char *glyph = glyph_via_core(st < 0 ? 1 : st);
                char *label = g_strdup_printf(
                    "%s %s", glyph ? glyph : "", hdr ? hdr : "");
                gtk_label_set_text(GTK_LABEL(badge), label);
                g_free(label);
                bridge_string_free(hdr);
                bridge_string_free(glyph);
            }
            free(rid);
        }
        if (attention > 0) {
            sub = g_strdup_printf(
                "%s · %s · %s%s · %d need input",
                project ? project : "", harness ? harness : "",
                age ? age : "", live ? " · live" : "", attention);
        } else {
            sub = g_strdup_printf("%s · %s · %s%s",
                                  project ? project : "",
                                  harness ? harness : "", age ? age : "",
                                  live ? " · live" : "");
        }
        free(project);
        free(harness);
        free(id);
        bridge_string_free(age);
    } else {
        if (attention > 0) {
            sub = g_strdup_printf("%zu runs · %d need input", n, attention);
        } else if (n == 0) {
            sub = g_strdup("No runs yet — New run spawns one");
        } else {
            sub = g_strdup_printf("%zu runs", n);
        }
    }
    adw_window_title_set_subtitle(sh->header_title, sub);
    g_free(sub);
}

static gboolean row_matches(GtkListBoxRow *row, gpointer data) {
    Shell *sh = data;
    const char *q = gtk_editable_get_text(GTK_EDITABLE(sh->filter));
    /* Collapsed History hides historic rows (core-owned membership +
     * expansion; expanded history renders as the sorted tail). */
    int idx = GPOINTER_TO_INT(g_object_get_data(G_OBJECT(row), "staap-idx"));
    if (idx >= 0 && !bridge_history_expanded(sh->core)) {
        char *json = bridge_session_json(sh->core, (size_t)idx);
        char *id = json_string(json ? json : "{}", "id");
        bridge_string_free(json);
        int historic = (id && bridge_is_history(sh->core, id)) ? 1 : 0;
        free(id);
        if (historic) {
            return FALSE;
        }
    }
    if (!q || !*q) {
        return TRUE;
    }
    /* Core-owned filter rule (title/project/id, case-insensitive). The
     * row index rides on the widget; the core answers match/no-match. */
    if (idx < 0) {
        return TRUE;
    }
    return bridge_row_matches(sh->core, (size_t)idx, q) ? TRUE : FALSE;
}

/* Order: working first, then idle (needs-input shares idle, issue
 * #105), history last; recent first within. Statuses + ids come
 * from the core registry. */
static int row_order(GtkListBoxRow *a, GtkListBoxRow *b, gpointer data) {
    Shell *sh = data;
    int ia = GPOINTER_TO_INT(g_object_get_data(G_OBJECT(a), "staap-idx"));
    int ib = GPOINTER_TO_INT(g_object_get_data(G_OBJECT(b), "staap-idx"));
    if (ia < 0 || ib < 0) {
        return 0;
    }
    int sa = bridge_status(sh->core, (size_t)ia);
    int sb = bridge_status(sh->core, (size_t)ib);
    int la = bridge_is_live(sh->core, NULL);
    (void)la;
    /* History (no live PTY) sorts last regardless of status. */
    char *ja = bridge_session_json(sh->core, (size_t)ia);
    char *jb = bridge_session_json(sh->core, (size_t)ib);
    char *ida = json_string(ja ? ja : "{}", "id");
    char *idb = json_string(jb ? jb : "{}", "id");
    bridge_string_free(ja);
    bridge_string_free(jb);
    int live_a = (ida && bridge_is_live(sh->core, ida)) ? 1 : 0;
    int live_b = (idb && bridge_is_live(sh->core, idb)) ? 1 : 0;
    free(ida);
    free(idb);
    if (!live_a && live_b) {
        return 1;
    }
    if (live_a && !live_b) {
        return -1;
    }
    /* Issue #105: idle and needs-input share one section. */
    int pa = sa == 2 ? 0 : 1;
    int pb = sb == 2 ? 0 : 1;
    if (pa != pb) {
        return pa - pb;
    }
    char *ka = bridge_session_json(sh->core, (size_t)ia);
    char *kb = bridge_session_json(sh->core, (size_t)ib);
    long long ta = json_int(ka ? ka : "{}", "last_active", 0);
    long long tb = json_int(kb ? kb : "{}", "last_active", 0);
    char *tita = json_string(ka ? ka : "{}", "title");
    char *titb = json_string(kb ? kb : "{}", "title");
    bridge_string_free(ka);
    bridge_string_free(kb);
    int rc;
    if (ta != tb) {
        rc = ta < tb ? 1 : -1;
    } else {
        rc = strcmp(tita ? tita : "", titb ? titb : "");
    }
    free(tita);
    free(titb);
    return rc;
}

/* ------------------------------------------------------------------ */
/* Forward declarations for callbacks. */
/* ------------------------------------------------------------------ */

static void show_selected_in_terminal(Shell *sh);
static gboolean pump_tick(gpointer data);

/* ------------------------------------------------------------------ */
/* Spawn / converse. */
/* ------------------------------------------------------------------ */

static void toast(Shell *sh, const char *msg) {
    GtkWindow *win =
        gtk_application_get_active_window(GTK_APPLICATION(sh->app));
    GtkWidget *overlay = g_object_get_data(G_OBJECT(win), "staap-toast-overlay");
    if (overlay) {
        adw_toast_overlay_add_toast(ADW_TOAST_OVERLAY(overlay),
                                    adw_toast_new(msg));
    }
}

static void update_spawn_state(Shell *sh) {
    /* Split-button repeat-last mints a local id, so both header buttons
     * stay sensitive with or without a selection (the empty roster is a
     * starting point, not a dead end). */
    (void)sh;
}

static void on_spawn(GtkButton *btn, gpointer data) {
    (void)btn;
    Shell *sh = data;
    /* Split-button main: repeat the last launch instantly (the
     * null-CLI/null-folder/zero-yolo form resolves the core's effective
     * default, so a picker-confirmed claude/yolo combo repeats here).
     * The core run registry attaches the PTY to a new roster row under
     * the shared cap — no shell-local ids, no shell-local live map. */
    char id[128] = { 0 };
    char *err = NULL;
    int rc = bridge_run_spawn(sh->core, NULL, NULL, 0,
                              (unsigned)sh->cols,
                              (unsigned)sh->rows_grid, id, sizeof id, &err);
    if (rc != 0) {
        char *msg = g_strdup_printf("Could not start session: %s",
                                    err ? err : "unknown error");
        toast(sh, msg);
        g_free(msg);
        free(err);
        return;
    }
    /* Name the effective CLI so the repeat is verifiable (the null
     * form resolves last-used / configured / autodetected). */
    char *eff = bridge_effective_cli(sh->core, NULL);
    char *preview =
        bridge_spawn_preview(eff ? eff : "muse", NULL, 0);
    bridge_string_free(eff);
    toast(sh, preview ? preview : "Session started.");
    free(preview);
    refresh_roster(sh);
    show_selected_in_terminal(sh);
    update_spawn_state(sh);
    reload_statuses(sh);
}

/* ------------------------------------------------------------------ */
/* 2D new-session picker (folder x CLI + yolo).                        */
/* ------------------------------------------------------------------ */

/* Picker catalog row (parsed from the core CLI catalog JSON). */
typedef struct {
    char *id;
    char *path; /* NULL when unavailable */
    int available;
} PickerCli;

typedef struct {
    Shell *sh;
    PickerCli *clis;
    size_t n_clis;
    GtkCheckButton **cli_btns;
    GtkEntry *folder;
    GtkDropDown *recent;
    char **recents;
    size_t n_recents;
    GtkDropDown *yolo;
    GtkLabel *preview;
    GtkLabel *error;
    AdwDialog *dialog;
} PickerUi;

/* Minimal catalog/recents JSON parse (flat shapes from the core; the
 * full picker-logic copies now live in the core — this only extracts
 * rows for native radio widgets). */
static PickerCli *parse_clis(const char *json, size_t *n_out) {
    *n_out = 0;
    if (!json) {
        return NULL;
    }
    size_t cap = 4, n = 0;
    PickerCli *out = calloc(cap, sizeof *out);
    if (!out) {
        return NULL;
    }
    const char *p = json;
    while ((p = strstr(p, "\"id\"")) != NULL) {
        const char *c = strchr(p, ':');
        if (!c) {
            break;
        }
        c++;
        while (*c == ' ' || *c == '\t' || *c == '"') {
            c++;
        }
        const char *e = strchr(c, '"');
        if (!e) {
            break;
        }
        /* Availability lives in the same object span: search between
         * here and the closing brace of this object. */
        const char *obj_end = strchr(e, '}');
        if (!obj_end) {
            obj_end = e + strlen(e);
        }
        int avail = 0;
        {
            const char *a = strstr(p, "\"available\"");
            if (a && a < obj_end) {
                const char *t = strstr(a, "true");
                avail = (t && t < obj_end) ? 1 : 0;
            }
        }
        if (n == cap) {
            cap *= 2;
            PickerCli *nb = realloc(out, cap * sizeof *out);
            if (!nb) {
                break;
            }
            out = nb;
        }
        out[n].id = strndup(c, (size_t)(e - c));
        out[n].path = NULL;
        out[n].available = avail;
        n++;
        p = e;
    }
    *n_out = n;
    return out;
}

static char **parse_recents(const char *json, size_t *n_out) {
    *n_out = 0;
    if (!json) {
        return NULL;
    }
    size_t cap = 4, n = 0;
    char **out = calloc(cap, sizeof *out);
    if (!out) {
        return NULL;
    }
    const char *p = strchr(json, '[');
    if (!p) {
        return out;
    }
    p++;
    while (*p && *p != ']') {
        while (*p == ' ' || *p == '\t' || *p == '\n' || *p == '\r' ||
               *p == ',') {
            p++;
        }
        if (*p != '"') {
            break;
        }
        p++;
        const char *e = p;
        while (*e && *e != '"') {
            e += (*e == '\\' && e[1]) ? 2 : 1;
        }
        if (n == cap) {
            cap *= 2;
            char **nb = realloc(out, cap * sizeof *out);
            if (!nb) {
                break;
            }
            out = nb;
        }
        out[n++] = strndup(p, (size_t)(e - p));
        p = (*e) ? e + 1 : e;
    }
    *n_out = n;
    return out;
}

static void clis_free(PickerCli *clis, size_t n) {
    if (!clis) {
        return;
    }
    for (size_t i = 0; i < n; i++) {
        free(clis[i].id);
        free(clis[i].path);
    }
    free(clis);
}

static void recents_free(char **recents, size_t n) {
    if (!recents) {
        return;
    }
    for (size_t i = 0; i < n; i++) {
        free(recents[i]);
    }
    free(recents);
}

/* Refresh the `runs: <cli> in <folder> + yolo` preview line from the
 * current widget state (core-owned preview copy). */
static void picker_refresh_preview(PickerUi *pu) {
    const char *cli = "muse";
    for (size_t i = 0; i < pu->n_clis; i++) {
        if (gtk_check_button_get_active(pu->cli_btns[i])) {
            cli = pu->clis[i].id;
            break;
        }
    }
    const char *folder = gtk_editable_get_text(GTK_EDITABLE(pu->folder));
    int yolo =
        bridge_yolo_value((int)gtk_drop_down_get_selected(pu->yolo));
    char *text = bridge_spawn_preview(cli, folder, yolo);
    gtk_label_set_text(pu->preview, text ? text : "runs: muse");
    free(text);
}

static void picker_on_changed(PickerUi *pu) {
    gtk_label_set_text(pu->error, "");
    picker_refresh_preview(pu);
}

/* Rebuild the yolo options so "Default" names its resolved state for
 * the selected CLI (issue #102). Keeps the current tri-state pick. */
static void picker_refresh_yolo(PickerUi *pu) {
    const char *cli = "muse";
    for (size_t i = 0; i < pu->n_clis; i++) {
        if (gtk_check_button_get_active(pu->cli_btns[i])) {
            cli = pu->clis[i].id;
            break;
        }
    }
    guint sel = gtk_drop_down_get_selected(pu->yolo);
    char def[32];
    snprintf(def, sizeof def, "Default (%s)",
             bridge_yolo_default(pu->sh->core, cli) ? "on" : "off");
    const char *opts[] = { def, "On (once)", "Off (once)", NULL };
    gtk_drop_down_set_model(pu->yolo,
                            G_LIST_MODEL(gtk_string_list_new(opts)));
    gtk_drop_down_set_selected(pu->yolo, sel);
}

static void picker_cli_toggled(GtkCheckButton *btn, gpointer data) {
    if (!gtk_check_button_get_active(btn)) {
        return; /* the newly activated sibling refreshes */
    }
    picker_on_changed(data);
    picker_refresh_yolo(data);
}

static void picker_folder_changed(GtkEditable *entry, gpointer data) {
    (void)entry;
    picker_on_changed(data);
}

static void picker_yolo_changed(GObject *obj, GParamSpec *pspec,
                                gpointer data) {
    (void)obj;
    (void)pspec;
    picker_on_changed(data);
}

/* A recent-folder pick refills the entry (which stays editable). */
static void picker_recent_changed(GObject *obj, GParamSpec *pspec,
                                  gpointer data) {
    (void)pspec;
    PickerUi *pu = data;
    guint idx = gtk_drop_down_get_selected(GTK_DROP_DOWN(obj));
    /* Index 0 is the "type a folder" placeholder; the rest are recents. */
    if (idx > 0 && idx - 1 < pu->n_recents) {
        gtk_editable_set_text(GTK_EDITABLE(pu->folder),
                              pu->recents[idx - 1]);
    }
    picker_on_changed(pu);
}

static void picker_free(PickerUi *pu) {
    if (!pu) {
        return;
    }
    clis_free(pu->clis, pu->n_clis);
    recents_free(pu->recents, pu->n_recents);
    free(pu->cli_btns);
    free(pu);
}

/* Spawn the confirmed combination: folder x CLI + one-shot yolo. A
 * missing folder refuses inline (the dialog stays open for a fix);
 * a spawn failure toasts and closes (the core error names the fix). */
static void picker_confirm(PickerUi *pu) {
    Shell *sh = pu->sh;
    const char *raw =
        gtk_editable_get_text(GTK_EDITABLE(pu->folder));
    char *folder = NULL;
    {
        const char *b = raw;
        while (*b == ' ' || *b == '\t') {
            b++;
        }
        size_t len = strlen(b);
        while (len > 0 && (b[len - 1] == ' ' || b[len - 1] == '\t')) {
            len--;
        }
        if (len > 0) {
            folder = strndup(b, len);
        }
    }
    if (folder && !g_file_test(folder, G_FILE_TEST_IS_DIR)) {
        char *msg = g_strdup_printf("No such folder: %s", folder);
        gtk_label_set_text(pu->error, msg);
        g_free(msg);
        free(folder);
        return;
    }
    const char *cli = "muse";
    for (size_t i = 0; i < pu->n_clis; i++) {
        if (gtk_check_button_get_active(pu->cli_btns[i])) {
            cli = pu->clis[i].id;
            break;
        }
    }
    /* Core yolo mapping (single copy of the tri-state rule). */
    int yolo =
        bridge_yolo_value((int)gtk_drop_down_get_selected(pu->yolo));
    /* The core registry enforces the shared cap (reaping oldest-exited
     * first); a refusal names per-run close like every other shell. */
    char id[128] = { 0 };
    char *err = NULL;
    int rc = bridge_run_spawn(sh->core, cli, folder, yolo,
                              (unsigned)sh->cols,
                              (unsigned)sh->rows_grid, id, sizeof id, &err);
    if (rc != 0) {
        if (err && strstr(err, "live runs")) {
            gtk_label_set_text(pu->error, err);
            free(err);
            free(folder);
            return;
        }
        char *msg = g_strdup_printf("Could not start session: %s",
                                    err ? err : "unknown error");
        toast(sh, msg);
        g_free(msg);
        free(err);
        free(folder);
        adw_dialog_close(pu->dialog);
        return;
    }
    char *preview = bridge_spawn_preview(cli, folder, yolo);
    toast(sh, preview ? preview : "Session started.");
    free(preview);
    free(folder);
    adw_dialog_close(pu->dialog);
    refresh_roster(sh);
    show_selected_in_terminal(sh);
    update_spawn_state(sh);
    reload_statuses(sh);
}

static void picker_spawn_clicked(GtkButton *btn, gpointer data) {
    (void)btn;
    picker_confirm(data);
}

static void picker_closed(AdwDialog *dialog, gpointer data) {
    (void)dialog;
    picker_free(data);
}

/* Directory picker for the folder axis (issue #102): choosing in the
 * UI beats typing a path. GtkFileDialog is async (GTK 4.10+), so
 * the folder lands through its callback; the entry is reffed for
 * the flight so a picker closed meanwhile cannot dangle, and a
 * detached entry (closed picker) is left alone. */
static void picker_choose_folder_done(GObject *src, GAsyncResult *res,
                                      gpointer data) {
    GtkEntry *folder = GTK_ENTRY(data);
    GError *err = NULL;
    GFile *file = gtk_file_dialog_select_folder_finish(GTK_FILE_DIALOG(src),
                                                       res, &err);
    g_clear_error(&err);
    if (file && gtk_widget_get_root(GTK_WIDGET(folder))) {
        char *path = g_file_get_path(file);
        if (path) {
            gtk_editable_set_text(GTK_EDITABLE(folder), path);
            g_free(path);
        }
    }
    if (file) {
        g_object_unref(file);
    }
    g_object_unref(folder);
}
static void picker_choose_folder(GtkButton *btn, gpointer data) {
    (void)btn;
    PickerUi *pu = data;
    GtkWindow *win =
        GTK_WINDOW(gtk_widget_get_root(GTK_WIDGET(pu->folder)));
    GtkFileDialog *dlg = gtk_file_dialog_new();
    gtk_file_dialog_set_title(dlg, "Choose a folder");
    gtk_file_dialog_select_folder(dlg, win, NULL, picker_choose_folder_done,
                                  g_object_ref(pu->folder));
    g_object_unref(dlg);
}
/* Open the 2D picker dialog: folder entry + recents, CLI radio rows
 * (missing CLIs disabled with an install hint), yolo tri-state, and
 * the live `runs: ...` preview. Catalog + recents re-read on every
 * open so the list is never stale. */
static void on_pick_session(GtkButton *btn, gpointer data) {
    (void)btn;
    Shell *sh = data;
    GtkWindow *win =
        gtk_application_get_active_window(GTK_APPLICATION(sh->app));

    PickerUi *pu = calloc(1, sizeof *pu);
    if (!pu) {
        return;
    }
    pu->sh = sh;
    char *clis_json = bridge_clis_json();
    pu->clis = parse_clis(clis_json ? clis_json : "[]", &pu->n_clis);
    bridge_string_free(clis_json);
    char *recents_json = bridge_recent_json(sh->core);
    pu->recents =
        parse_recents(recents_json ? recents_json : "[]", &pu->n_recents);
    bridge_string_free(recents_json);
    if (!pu->clis) {
        picker_free(pu);
        toast(sh, "Could not load the agent catalog.");
        return;
    }
    pu->cli_btns = calloc(pu->n_clis ? pu->n_clis : 1, sizeof *pu->cli_btns);
    if (!pu->cli_btns) {
        picker_free(pu);
        return;
    }

    AdwDialog *dialog = ADW_DIALOG(adw_dialog_new());
    pu->dialog = dialog;
    adw_dialog_set_title(dialog, "Start a new run");
    adw_dialog_set_content_width(dialog, 420);
    g_signal_connect(dialog, "closed", G_CALLBACK(picker_closed), pu);

    GtkWidget *box =
        gtk_box_new(GTK_ORIENTATION_VERTICAL, 12);
    gtk_widget_set_margin_start(box, 20);
    gtk_widget_set_margin_end(box, 20);
    gtk_widget_set_margin_top(box, 20);
    gtk_widget_set_margin_bottom(box, 20);

    GtkWidget *folder_label = gtk_label_new("Where should it work?");
    gtk_label_set_xalign(GTK_LABEL(folder_label), 0);
    gtk_widget_add_css_class(folder_label, "heading");
    gtk_box_append(GTK_BOX(box), folder_label);
    GtkWidget *folder_row = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 8);
    pu->folder = GTK_ENTRY(gtk_entry_new());
    gtk_widget_set_hexpand(GTK_WIDGET(pu->folder), TRUE);
    gtk_entry_set_placeholder_text(pu->folder, "Blank = current folder");
    if (pu->n_recents > 0) {
        gtk_editable_set_text(GTK_EDITABLE(pu->folder), pu->recents[0]);
    }
    g_signal_connect(pu->folder, "changed",
                     G_CALLBACK(picker_folder_changed), pu);
    gtk_box_append(GTK_BOX(folder_row), GTK_WIDGET(pu->folder));
    GtkWidget *choose = gtk_button_new_with_label("Choose…");
    g_signal_connect(choose, "clicked", G_CALLBACK(picker_choose_folder),
                     pu);
    gtk_box_append(GTK_BOX(folder_row), choose);
    gtk_box_append(GTK_BOX(box), folder_row);
    if (pu->n_recents > 0) {
        GtkStringList *recent_list =
            GTK_STRING_LIST(gtk_string_list_new(NULL));
        gtk_string_list_append(recent_list, "Type a folder…");
        for (size_t i = 0; i < pu->n_recents; i++) {
            gtk_string_list_append(recent_list, pu->recents[i]);
        }
        pu->recent = GTK_DROP_DOWN(gtk_drop_down_new(
            G_LIST_MODEL(recent_list), NULL));
        gtk_drop_down_set_selected(pu->recent, 0);
        g_signal_connect(pu->recent, "notify::selected",
                         G_CALLBACK(picker_recent_changed), pu);
        gtk_box_append(GTK_BOX(box), GTK_WIDGET(pu->recent));
    }
    pu->error = GTK_LABEL(gtk_label_new(""));
    gtk_label_set_xalign(pu->error, 0);
    gtk_widget_add_css_class(GTK_WIDGET(pu->error), "error");
    gtk_box_append(GTK_BOX(box), GTK_WIDGET(pu->error));

    GtkWidget *cli_label = gtk_label_new("Who should do it?");
    gtk_label_set_xalign(GTK_LABEL(cli_label), 0);
    gtk_widget_add_css_class(cli_label, "heading");
    gtk_box_append(GTK_BOX(box), cli_label);
    GtkCheckButton *group = NULL;
    for (size_t i = 0; i < pu->n_clis; i++) {
        char *label;
        if (pu->clis[i].available) {
            label = g_strdup_printf("%s — ready", pu->clis[i].id);
        } else {
            label = g_strdup_printf("%s — not installed", pu->clis[i].id);
        }
        GtkWidget *row = gtk_check_button_new_with_label(label);
        g_free(label);
        gtk_check_button_set_group(GTK_CHECK_BUTTON(row), group);
        if (!group) {
            group = GTK_CHECK_BUTTON(row);
        }
        gtk_widget_set_sensitive(row, pu->clis[i].available ? TRUE : FALSE);
        gtk_widget_set_tooltip_text(
            row, pu->clis[i].available
                     ? (pu->clis[i].path ? pu->clis[i].path
                        : pu->clis[i].id)
                     : "Install this CLI and ensure it is on PATH.");
        g_signal_connect(row, "toggled", G_CALLBACK(picker_cli_toggled),
                         pu);
        gtk_box_append(GTK_BOX(box), row);
        pu->cli_btns[i] = GTK_CHECK_BUTTON(row);
    }
    /* Preselect the first available CLI (catalog order: muse first). */
    for (size_t i = 0; i < pu->n_clis; i++) {
        if (pu->clis[i].available) {
            gtk_check_button_set_active(pu->cli_btns[i], TRUE);
            break;
        }
    }

    GtkWidget *yolo_label = gtk_label_new("Permission mode");
    gtk_label_set_xalign(GTK_LABEL(yolo_label), 0);
    gtk_widget_add_css_class(yolo_label, "heading");
    gtk_box_append(GTK_BOX(box), yolo_label);
    const char *yolo_opts[] = { "Default", "On (once)", "Off (once)",
                                NULL };
    pu->yolo = GTK_DROP_DOWN(gtk_drop_down_new_from_strings(yolo_opts));
    gtk_widget_set_tooltip_text(
        GTK_WIDGET(pu->yolo),
        "Yolo lets the agent run commands without asking. "
        "Default follows the per-agent config; once-choices apply to "
        "this run only and are never saved.");
    g_signal_connect(pu->yolo, "notify::selected",
                     G_CALLBACK(picker_yolo_changed), pu);
    gtk_box_append(GTK_BOX(box), GTK_WIDGET(pu->yolo));

    pu->preview = GTK_LABEL(gtk_label_new("runs: muse"));
    gtk_label_set_xalign(pu->preview, 0);
    gtk_widget_add_css_class(GTK_WIDGET(pu->preview), "dim-label");
    gtk_box_append(GTK_BOX(box), GTK_WIDGET(pu->preview));

    GtkWidget *actions = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 8);
    gtk_widget_set_halign(actions, GTK_ALIGN_END);
    GtkWidget *cancel = gtk_button_new_with_label("Cancel");
    g_signal_connect_swapped(cancel, "clicked",
                             G_CALLBACK(adw_dialog_close), dialog);
    gtk_box_append(GTK_BOX(actions), cancel);
    GtkWidget *spawn = gtk_button_new_with_label("Spawn");
    gtk_widget_add_css_class(spawn, "suggested-action");
    g_signal_connect(spawn, "clicked", G_CALLBACK(picker_spawn_clicked),
                     pu);
    gtk_box_append(GTK_BOX(actions), spawn);
    gtk_box_append(GTK_BOX(box), actions);

    /* Label "Default" with its resolved state for the preselected CLI. */
    picker_refresh_yolo(pu);
    picker_refresh_preview(pu);
    adw_dialog_set_child(dialog, box);
    adw_dialog_present(dialog, GTK_WIDGET(win));
}

/* Map one GDK keyval to the core's logical key name (the shared key
 * table in `staap_key_encode` owns the bytes; this only translates). */
static const char *gdk_key_name(guint keyval) {
    switch (keyval) {
    case GDK_KEY_Return:
    case GDK_KEY_KP_Enter:
    case GDK_KEY_ISO_Enter: return "enter";
    case GDK_KEY_BackSpace: return "backspace";
    case GDK_KEY_Tab: return "tab";
    case GDK_KEY_Escape: return "escape";
    case GDK_KEY_Up: return "up";
    case GDK_KEY_Down: return "down";
    case GDK_KEY_Right: return "right";
    case GDK_KEY_Left: return "left";
    case GDK_KEY_Home: return "home";
    case GDK_KEY_End: return "end";
    case GDK_KEY_Insert: return "insert";
    case GDK_KEY_Delete: return "delete";
    case GDK_KEY_Page_Up: return "pageup";
    case GDK_KEY_Page_Down: return "pagedown";
    case GDK_KEY_F1: return "f1";
    case GDK_KEY_F2: return "f2";
    case GDK_KEY_F3: return "f3";
    case GDK_KEY_F4: return "f4";
    case GDK_KEY_F5: return "f5";
    case GDK_KEY_F6: return "f6";
    case GDK_KEY_F7: return "f7";
    case GDK_KEY_F8: return "f8";
    case GDK_KEY_F9: return "f9";
    case GDK_KEY_F10: return "f10";
    case GDK_KEY_F11: return "f11";
    case GDK_KEY_F12: return "f12";
    default: return NULL;
    }
}

/* Selected row id from the core (owned copy; NULL when empty). */
static char *selected_id(Shell *sh) {
    size_t n = bridge_session_count(sh->core);
    size_t sel = bridge_selected(sh->core);
    if (sel >= n) {
        return NULL;
    }
    char *json = bridge_session_json(sh->core, sel);
    char *id = json_string(json ? json : "{}", "id");
    bridge_string_free(json);
    return id;
}

/* Forward one key press to the selected run's child via the shared core
 * key table. Returns TRUE when handled (stop VTE from also processing
 * the key: with no VTE-side PTY there is exactly one line discipline —
 * the core's — so nothing double-echoes). */
static gboolean on_key_pressed(GtkEventControllerKey *ctl, guint keyval,
                               guint keycode, GdkModifierType state,
                               gpointer data) {
    (void)ctl;
    (void)keycode;
    Shell *sh = data;
    char *id = selected_id(sh);
    if (!id || !bridge_is_live(sh->core, id)) {
        free(id);
        return FALSE;
    }
    gboolean ctrl = (state & GDK_CONTROL_MASK) != 0;
    gboolean shift = (state & GDK_SHIFT_MASK) != 0;
    gboolean alt = (state & GDK_ALT_MASK) != 0;

    /* Let VTE keep its own copy/paste shortcuts (shared reserve rule). */
    if (ctrl && shift && (keyval == GDK_KEY_C || keyval == GDK_KEY_V)) {
        free(id);
        return FALSE;
    }

    /* Logical key name + typed char for the core table. */
    const char *name = gdk_key_name(keyval);
    char *owned_name = NULL;
    char key_char[8] = { 0 };
    const char *key_char_arg = NULL;
    guint32 uc = gdk_keyval_to_unicode(keyval);
    if (uc != 0) {
        int m = g_unichar_to_utf8((gunichar)uc, key_char);
        if (m > 0) {
            key_char[m] = '\0';
            key_char_arg = key_char;
        }
        if (!name) {
            /* Single printable char: the key name is the char itself
             * (the core table prefers key_char, layout-correct). */
            if (g_utf8_strlen(key_char, -1) == 1) {
                owned_name = g_strdup(key_char);
                name = owned_name;
            }
        }
    }
    if (!name) {
        free(owned_name);
        free(id);
        return FALSE; /* Super, modifiers, media keys: leave to GTK. */
    }

    unsigned char buf[16];
    int n = bridge_key_encode(name, key_char_arg, ctrl ? 1 : 0,
                              alt ? 1 : 0, buf, sizeof buf);
    free(owned_name);
    if (n <= 0) {
        free(id);
        return n < 0 ? FALSE : TRUE;
    }

    char *err = NULL;
    if (bridge_run_write(sh->core, id, buf, (size_t)n, &err) != 0) {
        char *msg = g_strdup_printf("Could not send input: %s",
                                    err ? err : "unknown error");
        toast(sh, msg);
        g_free(msg);
        free(err);
    }
    free(id);
    return TRUE;
}

/* ------------------------------------------------------------------ */
/* Pump: the shell's only repaint gate (mirrors AppState.pump). */
/* ------------------------------------------------------------------ */

static void feed_to_vte(Shell *sh, const char *feed);

/* Place the VTE cursor where the child put it (issue #104): fed snapshot
 * text alone would leave it at the end of fed output. */
static void feed_cursor_to_vte(Shell *sh, const char *id) {
    uint16_t row = 0, col = 0;
    if (bridge_run_cursor(sh->core, id, &row, &col) != 0) {
        return;
    }
    char cup[32];
    snprintf(cup, sizeof cup, "\x1b[%u;%uH", (unsigned)row + 1,
             (unsigned)col + 1);
    feed_to_vte(sh, cup);
}

static void feed_to_vte(Shell *sh, const char *feed) {
    vte_terminal_feed(sh->term, feed, (gssize)strlen(feed));
}

/* Show the selected run's current snapshot in the VTE from scratch
 * (selection change or fresh spawn): reset, then feed the full screen.
 * Reads the core registry by row id — no shell-local PTY map. */
static void show_selected_in_terminal(Shell *sh) {
    vte_terminal_reset(sh->term, TRUE, TRUE);
    char *id = selected_id(sh);
    if (!id || !bridge_is_live(sh->core, id)) {
        /* No live session on the selected row: offer restart/resume
         * when the row exists (ended run or historic entry), else the
         * empty hint. Same recovery rule as the other shells. */
        size_t n = bridge_session_count(sh->core);
        size_t sel = bridge_selected(sh->core);
        if (sel < n) {
            vte_terminal_feed(sh->term,
                              "(run ended — press R to restart)\r\n", -1);
        } else {
            vte_terminal_feed(sh->term,
                              "(no live session — press New run)\r\n", -1);
        }
        free(id);
        free(sh->fed_id);
        sh->fed_id = NULL;
        free(sh->fed_text);
        sh->fed_text = strdup("");
        return;
    }
    char *snap = bridge_run_screen_text(sh->core, id);
    const char *text = snap ? snap : "";
    char *feed = bridge_feed_delta("", text);
    if (feed) {
        feed_to_vte(sh, feed);
        free(feed);
    }
    feed_cursor_to_vte(sh, id);
    free(sh->fed_id);
    sh->fed_id = id;
    free(sh->fed_text);
    sh->fed_text = strdup(text);
    bridge_string_free(snap);
}

/* Pump: the core registry feeds every live run (statuses, links, and
 * re-sort inside via `bridge_pump_all` — the roster now actually moves),
 * then the selected run's delta reaches the VTE, the roster rebuilds on
 * change, and persistence autosaves on a throttle. */
static int save_counter = 0;

static gboolean pump_tick(gpointer data) {
    Shell *sh = data;
    int dirty = bridge_pump_all(sh->core);
    /* Selected run's delta (by row id, not by a shell-local map). */
    char *id = selected_id(sh);
    if (id && bridge_is_live(sh->core, id)) {
        char *snap = bridge_run_screen_text(sh->core, id);
        const char *text = snap ? snap : "";
        const char *fed =
            (sh->fed_id && strcmp(sh->fed_id, id) == 0 && sh->fed_text)
                ? sh->fed_text
                : "";
        char *feed = bridge_feed_delta(fed, text);
        if (feed && feed[0]) {
            feed_to_vte(sh, feed);
        }
        feed_cursor_to_vte(sh, id);
        free(feed);
        free(sh->fed_id);
        sh->fed_id = id;
        id = NULL;
        free(sh->fed_text);
        sh->fed_text = strdup(text);
        bridge_string_free(snap);
        dirty = 1;
    }
    free(id);
    /* Roster rebuild on change (new rows attach in the core now). */
    refresh_roster(sh);
    reload_statuses(sh);
    /* Autosave throttle (replaces the manual Save button): persist at
     * most every ~5s while dirty, like the shared pump persist. */
    if (dirty && (++save_counter % 100) == 0) {
        char *err = NULL;
        if (bridge_core_save(sh->core, &err) != 0) {
            free(err);
        }
    }
    (void)dirty;
    return G_SOURCE_CONTINUE;
}

/* Track the VTE grid so spawns and resizes use the real size. */
static void on_term_resize(VteTerminal *term, guint w, guint h,
                           gpointer data) {
    (void)w;
    (void)h;
    Shell *sh = data;
    glong cols = vte_terminal_get_column_count(term);
    glong rows = vte_terminal_get_row_count(term);
    if (cols < 1 || rows < 1) {
        return;
    }
    if ((int)cols != sh->cols || (int)rows != sh->rows_grid) {
        sh->cols = (int)cols;
        sh->rows_grid = (int)rows;
        size_t n = bridge_session_count(sh->core);
        for (size_t i = 0; i < n; i++) {
            char *json = bridge_session_json(sh->core, i);
            char *rid = json_string(json ? json : "{}", "id");
            bridge_string_free(json);
            if (rid) {
                bridge_run_resize(sh->core, rid, (unsigned)cols,
                                  (unsigned)rows);
                free(rid);
            }
        }
        /* Reflow replays the selected screen from scratch. */
        show_selected_in_terminal(sh);
    }
}

/* ------------------------------------------------------------------ */
/* ------------------------------------------------------------------ */
/* UI construction. */
/* ------------------------------------------------------------------ */

static void on_row_selected(GtkListBox *box, GtkListBoxRow *row,
                            gpointer data) {
    (void)box;
    Shell *sh = data;
    /* Core-owned selection: the row index rides on the widget. */
    int idx = row
        ? GPOINTER_TO_INT(g_object_get_data(G_OBJECT(row), "staap-idx"))
        : -1;
    if (idx >= 0) {
        bridge_select(sh->core, (size_t)idx);
    }
    show_selected_in_terminal(sh);
    update_spawn_state(sh);
    reload_statuses(sh);
}

static void on_filter_changed(GtkSearchEntry *entry, gpointer data) {
    (void)entry;
    Shell *sh = data;
    /* Core-owned filter text (snaps selection); the list filter reads
     * the same core state. */
    bridge_set_filter(
        sh->core, gtk_editable_get_text(GTK_EDITABLE(sh->filter)));
    gtk_list_box_invalidate_filter(sh->list);
}

/* History expander toggled: the single funnel for expand/collapse —
 * core state + persist, then re-filter so historic rows show/hide.
 * Guarded by handler block in refresh_roster for programmatic sync. */
static void on_history_expanded(GtkExpander *expander, GParamSpec *pspec,
                                gpointer data) {
    (void)pspec;
    Shell *sh = data;
    bridge_set_history_expanded(
        sh->core, gtk_expander_get_expanded(expander) ? 1 : 0);
    char *err = NULL;
    if (bridge_core_save(sh->core, &err) != 0) {
        free(err);
    }
    gtk_list_box_invalidate_filter(sh->list);
}

/* Per-run close (parity with the `x` key everywhere): drop the selected
 * run's PTY + entry, persist, rebuild. */
static void on_close_run(GtkButton *btn, gpointer data) {
    (void)btn;
    Shell *sh = data;
    char *id = selected_id(sh);
    if (!id) {
        return;
    }
    bridge_run_close(sh->core, id);
    free(id);
    char *err = NULL;
    if (bridge_core_save(sh->core, &err) != 0) {
        free(err);
    }
    refresh_roster(sh);
    show_selected_in_terminal(sh);
    reload_statuses(sh);
}

/* Restart/resume the selected run (parity with the `r` key): re-attach
 * on the same id, keeping title and links. */
static void on_restart_run(GtkButton *btn, gpointer data) {
    (void)btn;
    Shell *sh = data;
    char *id = selected_id(sh);
    if (!id) {
        return;
    }
    char *err = NULL;
    if (bridge_run_restart(sh->core, id, (unsigned)sh->cols,
                           (unsigned)sh->rows_grid, &err) != 0) {
        char *msg = g_strdup_printf("Could not restart: %s",
                                    err ? err : "unknown error");
        toast(sh, msg);
        g_free(msg);
        free(err);
        free(id);
        return;
    }
    free(id);
    refresh_roster(sh);
    show_selected_in_terminal(sh);
    reload_statuses(sh);
}

static void on_find_changed(GtkSearchEntry *entry, gpointer data) {
    Shell *sh = data;
    const char *q = gtk_editable_get_text(GTK_EDITABLE(entry));
    if (!q || !*q) {
        vte_terminal_search_set_regex(sh->term, NULL, 0);
        return;
    }
    /* Literal search: escape regex metacharacters, match case-insensitively
     * (PCRE2 CASELESS is part of VTE_REGEX_FLAGS_DEFAULT). */
    char *escaped = g_regex_escape_string(q, -1);
    GError *error = NULL;
    VteRegex *re = vte_regex_new_for_search(escaped, -1,
                                            VTE_REGEX_FLAGS_DEFAULT, &error);
    g_free(escaped);
    if (error) {
        g_error_free(error);
    }
    vte_terminal_search_set_regex(sh->term, re, 0);
    if (re) {
        vte_terminal_search_set_wrap_around(sh->term, TRUE);
        vte_terminal_search_find_next(sh->term);
        vte_regex_unref(re);
    }
}

static gboolean on_find_key(GtkEventControllerKey *ctl, guint keyval,
                            guint keycode, GdkModifierType state,
                            gpointer data) {
    (void)ctl;
    (void)keycode;
    Shell *sh = data;
    if (keyval == GDK_KEY_Escape) {
        gtk_revealer_set_reveal_child(sh->find_revealer, FALSE);
        gtk_widget_grab_focus(GTK_WIDGET(sh->term));
        return TRUE;
    }
    if (keyval == GDK_KEY_Return) {
        if (state & GDK_SHIFT_MASK) {
            vte_terminal_search_find_previous(sh->term);
        } else {
            vte_terminal_search_find_next(sh->term);
        }
        return TRUE;
    }
    return FALSE;
}

static void toggle_find(Shell *sh) {
    gboolean open = gtk_revealer_get_reveal_child(sh->find_revealer);
    gtk_revealer_set_reveal_child(sh->find_revealer, !open);
    if (!open) {
        gtk_widget_grab_focus(GTK_WIDGET(sh->find_entry));
    } else {
        gtk_widget_grab_focus(GTK_WIDGET(sh->term));
    }
}

static void on_copy(GSimpleAction *action, GVariant *param, gpointer data) {
    (void)action;
    (void)param;
    vte_terminal_copy_clipboard_format(data, VTE_FORMAT_TEXT);
}

static void on_paste(GSimpleAction *action, GVariant *param, gpointer data) {
    (void)action;
    (void)param;
    vte_terminal_paste_clipboard(data);
}

static void on_term_menu_closed(GtkPopover *pop, gpointer data) {
    (void)data;
    gtk_widget_unparent(GTK_WIDGET(pop));
}

static void on_term_menu(GtkGestureClick *gest, int n_press, double x,
                         double y, gpointer data) {
    (void)gest;
    (void)n_press;
    Shell *sh = data;
    GMenu *m = g_menu_new();
    if (vte_terminal_get_has_selection(sh->term)) {
        g_menu_append(m, "Copy", "term.copy");
    }
    g_menu_append(m, "Paste", "term.paste");
    GSimpleActionGroup *ag = g_simple_action_group_new();
    GSimpleAction *copy = g_simple_action_new("copy", NULL);
    g_signal_connect(copy, "activate", G_CALLBACK(on_copy), sh->term);
    GSimpleAction *paste = g_simple_action_new("paste", NULL);
    g_signal_connect(paste, "activate", G_CALLBACK(on_paste), sh->term);
    g_action_map_add_action(G_ACTION_MAP(ag), G_ACTION(copy));
    g_action_map_add_action(G_ACTION_MAP(ag), G_ACTION(paste));
    GtkWidget *pop = gtk_popover_menu_new_from_model(G_MENU_MODEL(m));
    gtk_widget_insert_action_group(pop, "term", G_ACTION_GROUP(ag));
    gtk_widget_set_parent(pop, GTK_WIDGET(sh->term));
    g_signal_connect(pop, "closed", G_CALLBACK(on_term_menu_closed), NULL);
    GdkRectangle pt = { (int)x, (int)y, 1, 1 };
    gtk_popover_set_pointing_to(GTK_POPOVER(pop), &pt);
    gtk_popover_popup(GTK_POPOVER(pop));
    g_object_unref(m);
    g_object_unref(ag);
}

/* App-level shortcuts: Ctrl+F find, Ctrl+N repeat-last spawn,
 * Ctrl+Shift+N picker, Ctrl+R restart/resume, Ctrl+W close run,
 * Up/Down step selection. Persistence is automatic (no Ctrl+S). */
static gboolean on_window_key(GtkEventControllerKey *ctl, guint keyval,
                              guint keycode, GdkModifierType state,
                              gpointer data) {
    (void)ctl;
    (void)keycode;
    Shell *sh = data;
    if ((state & GDK_CONTROL_MASK) && !(state & GDK_ALT_MASK)) {
        if (keyval == GDK_KEY_f || keyval == GDK_KEY_F) {
            toggle_find(sh);
            return TRUE;
        }
        if ((keyval == GDK_KEY_n || keyval == GDK_KEY_N) &&
            !(state & GDK_SHIFT_MASK)) {
            on_spawn(NULL, sh);
            return TRUE;
        }
        if ((keyval == GDK_KEY_n || keyval == GDK_KEY_N) &&
            (state & GDK_SHIFT_MASK)) {
            on_pick_session(NULL, sh);
            return TRUE;
        }
        if (keyval == GDK_KEY_r || keyval == GDK_KEY_R) {
            on_restart_run(NULL, sh);
            return TRUE;
        }
        if (keyval == GDK_KEY_w || keyval == GDK_KEY_W) {
            on_close_run(NULL, sh);
            return TRUE;
        }
    }
    if (keyval == GDK_KEY_Up && (state & GDK_CONTROL_MASK)) {
        bridge_select_step(sh->core, 0);
        refresh_roster(sh);
        show_selected_in_terminal(sh);
        reload_statuses(sh);
        return TRUE;
    }
    if (keyval == GDK_KEY_Down && (state & GDK_CONTROL_MASK)) {
        bridge_select_step(sh->core, 1);
        refresh_roster(sh);
        show_selected_in_terminal(sh);
        reload_statuses(sh);
        return TRUE;
    }
    return FALSE;
}

static gboolean on_close(GtkWindow *win, gpointer data) {
    (void)win;
    Shell *sh = data;
    /* Best-effort persist on quit; a failure still closes (the error is
     * visible next launch via the core's degrade-to-empty policy). */
    char *err = NULL;
    if (bridge_core_save(sh->core, &err) != 0) {
        free(err);
    }
    return FALSE; /* let the window close */
}

static void build_ui(Shell *sh) {
    AdwApplicationWindow *win = ADW_APPLICATION_WINDOW(
        adw_application_window_new(GTK_APPLICATION(sh->app)));
    gtk_window_set_title(GTK_WINDOW(win), "Staap");
    gtk_window_set_default_size(GTK_WINDOW(win), 1100, 700);
    g_signal_connect(win, "close-request", G_CALLBACK(on_close), sh);

    AdwToastOverlay *toasts = ADW_TOAST_OVERLAY(adw_toast_overlay_new());
    g_object_set_data(G_OBJECT(win), "staap-toast-overlay", toasts);
    adw_application_window_set_content(win, GTK_WIDGET(toasts));

    /* Header: subtitle shows selection context + attention count. */
    AdwHeaderBar *bar = ADW_HEADER_BAR(adw_header_bar_new());
    GtkWidget *title = adw_window_title_new("Staap", NULL);
    sh->header_title = ADW_WINDOW_TITLE(title);
    adw_header_bar_set_title_widget(bar, title);

    /* Header: split-button 2D launch — "New run" repeats the last
     * launch instantly, the caret opens the folder x CLI picker. */
    GtkWidget *new_box = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 0);
    gtk_widget_add_css_class(new_box, "linked");
    sh->spawn_btn = GTK_BUTTON(gtk_button_new_with_label("New run"));
    gtk_widget_add_css_class(GTK_WIDGET(sh->spawn_btn), "suggested-action");
    gtk_widget_set_tooltip_text(GTK_WIDGET(sh->spawn_btn),
                                "Repeat the last session (Ctrl+N)");
    g_signal_connect(sh->spawn_btn, "clicked", G_CALLBACK(on_spawn), sh);
    gtk_box_append(GTK_BOX(new_box), GTK_WIDGET(sh->spawn_btn));
    GtkWidget *pick_btn = gtk_button_new_with_label("▾");
    gtk_widget_set_tooltip_text(pick_btn,
                                "Choose folder, CLI, options… (Ctrl+Shift+N)");
    g_signal_connect(pick_btn, "clicked", G_CALLBACK(on_pick_session), sh);
    gtk_box_append(GTK_BOX(new_box), pick_btn);
    gtk_widget_set_sensitive(pick_btn, TRUE);
    adw_header_bar_pack_start(ADW_HEADER_BAR(bar), new_box);

    /* Restart + close run buttons (parity with Ctrl+R / Ctrl+W and the
     * `r`/`x` keys everywhere). Persistence is automatic — no Save
     * button, no theme picker: the shell follows the system appearance
     * like macOS/Windows. */
    GtkWidget *restart_btn = gtk_button_new_with_label("Restart");
    gtk_widget_set_tooltip_text(restart_btn,
                                "Restart/resume the selected run (Ctrl+R)");
    g_signal_connect(restart_btn, "clicked", G_CALLBACK(on_restart_run),
                     sh);
    adw_header_bar_pack_end(ADW_HEADER_BAR(bar), restart_btn);
    GtkWidget *close_btn = gtk_button_new_with_label("Close run");
    gtk_widget_set_tooltip_text(close_btn, "Close the selected run (Ctrl+W)");
    g_signal_connect(close_btn, "clicked", G_CALLBACK(on_close_run), sh);
    adw_header_bar_pack_end(ADW_HEADER_BAR(bar), close_btn);

    /* Split view: roster sidebar + terminal. */
    AdwOverlaySplitView *split =
        ADW_OVERLAY_SPLIT_VIEW(adw_overlay_split_view_new());
    adw_overlay_split_view_set_max_sidebar_width(split, 340);
    adw_overlay_split_view_set_min_sidebar_width(split, 220);

    GtkWidget *side_box = gtk_box_new(GTK_ORIENTATION_VERTICAL, 0);
    sh->filter = GTK_SEARCH_ENTRY(gtk_search_entry_new());
    gtk_search_entry_set_placeholder_text(sh->filter, "Filter runs");
    gtk_search_entry_set_search_delay(sh->filter, 150);
    g_signal_connect(sh->filter, "search-changed",
                     G_CALLBACK(on_filter_changed), sh);
    gtk_box_append(GTK_BOX(side_box), GTK_WIDGET(sh->filter));

    GtkWidget *scroll = gtk_scrolled_window_new();
    gtk_widget_set_vexpand(scroll, TRUE);
    sh->list = GTK_LIST_BOX(gtk_list_box_new());
    gtk_list_box_set_selection_mode(sh->list, GTK_SELECTION_SINGLE);
    gtk_list_box_set_filter_func(sh->list, row_matches, sh, NULL);
    gtk_list_box_set_sort_func(sh->list, row_order, sh, NULL);
    g_signal_connect(sh->list, "row-selected", G_CALLBACK(on_row_selected),
                     sh);
    gtk_scrolled_window_set_child(GTK_SCROLLED_WINDOW(scroll),
                                  GTK_WIDGET(sh->list));
    gtk_box_append(GTK_BOX(side_box), scroll);
    /* History affordance below the list: the header is the expand
     * toggle (membership + expansion live in the core); rows stay
     * siblings in the list above, sorted last. Collapsed by default. */
    sh->history_expander = GTK_EXPANDER(gtk_expander_new("History (0)"));
    gtk_expander_set_expanded(sh->history_expander, FALSE);
    gtk_widget_set_margin_start(GTK_WIDGET(sh->history_expander), 8);
    gtk_widget_set_margin_end(GTK_WIDGET(sh->history_expander), 8);
    g_signal_connect(sh->history_expander, "notify::expanded",
                     G_CALLBACK(on_history_expanded), sh);
    gtk_box_append(GTK_BOX(side_box), GTK_WIDGET(sh->history_expander));
    adw_overlay_split_view_set_sidebar(split, side_box);

    /* Terminal side: find revealer on top, VTE in a scrolled window. */
    GtkWidget *term_box = gtk_box_new(GTK_ORIENTATION_VERTICAL, 0);
    sh->find_revealer = GTK_REVEALER(gtk_revealer_new());
    sh->find_entry = GTK_SEARCH_ENTRY(gtk_search_entry_new());
    gtk_search_entry_set_placeholder_text(sh->find_entry,
                                          "Find in terminal (Enter next, "
                                          "Shift+Enter previous, Esc close)");
    g_signal_connect(sh->find_entry, "search-changed",
                     G_CALLBACK(on_find_changed), sh);
    GtkEventController *find_keys = gtk_event_controller_key_new();
    g_signal_connect(find_keys, "key-pressed", G_CALLBACK(on_find_key), sh);
    gtk_widget_add_controller(GTK_WIDGET(sh->find_entry), find_keys);
    gtk_revealer_set_child(sh->find_revealer, GTK_WIDGET(sh->find_entry));
    gtk_box_append(GTK_BOX(term_box), GTK_WIDGET(sh->find_revealer));

    sh->term_scroll = GTK_SCROLLED_WINDOW(gtk_scrolled_window_new());
    gtk_widget_set_vexpand(GTK_WIDGET(sh->term_scroll), TRUE);
    gtk_widget_set_hexpand(GTK_WIDGET(sh->term_scroll), TRUE);
    sh->term = VTE_TERMINAL(vte_terminal_new());
    vte_terminal_set_scrollback_lines(sh->term, VTE_SCROLLBACK_LINES);
    vte_terminal_set_scroll_on_output(sh->term, TRUE);
    vte_terminal_set_scroll_on_keystroke(sh->term, TRUE);
    vte_terminal_set_cursor_blink_mode(sh->term, VTE_CURSOR_BLINK_ON);
    vte_terminal_set_allow_hyperlink(sh->term, TRUE);
    PangoFontDescription *font =
        pango_font_description_from_string("Monospace 11");
    vte_terminal_set_font(sh->term, font);
    pango_font_description_free(font);
    g_signal_connect(sh->term, "resize-window", G_CALLBACK(on_term_resize),
                     sh);
    GtkEventController *keys = gtk_event_controller_key_new();
    g_signal_connect(keys, "key-pressed", G_CALLBACK(on_key_pressed), sh);
    gtk_widget_add_controller(GTK_WIDGET(sh->term), keys);
    GtkGesture *right = gtk_gesture_click_new();
    gtk_gesture_single_set_button(GTK_GESTURE_SINGLE(right), 3);
    g_signal_connect(right, "pressed", G_CALLBACK(on_term_menu), sh);
    gtk_widget_add_controller(GTK_WIDGET(sh->term),
                              GTK_EVENT_CONTROLLER(right));
    gtk_scrolled_window_set_child(sh->term_scroll, GTK_WIDGET(sh->term));
    gtk_box_append(GTK_BOX(term_box), GTK_WIDGET(sh->term_scroll));
    adw_overlay_split_view_set_content(split, term_box);

    GtkWidget *toolbar = gtk_box_new(GTK_ORIENTATION_VERTICAL, 0);
    gtk_box_append(GTK_BOX(toolbar), GTK_WIDGET(bar));
    gtk_box_append(GTK_BOX(toolbar), GTK_WIDGET(split));
    gtk_widget_set_vexpand(GTK_WIDGET(split), TRUE);
    adw_toast_overlay_set_child(toasts, toolbar);

    GtkEventController *win_keys = gtk_event_controller_key_new();
    g_signal_connect(win_keys, "key-pressed", G_CALLBACK(on_window_key), sh);
    gtk_widget_add_controller(GTK_WIDGET(win), win_keys);

    /* Follow the system appearance (no manual override); rows come from
     * the core registry via refresh_roster below. */
    apply_system_theme(sh);
    refresh_roster(sh);

    size_t n = bridge_session_count(sh->core);
    if (n > 0) {
        GtkListBoxRow *first = gtk_list_box_get_row_at_index(sh->list, 0);
        if (first) {
            gtk_list_box_select_row(sh->list, first);
        }
    } else {
        adw_window_title_set_subtitle(sh->header_title,
                                      "No runs yet — New run spawns one");
        vte_terminal_feed(sh->term, "No runs yet — press New run.\r\n", -1);
    }
    update_spawn_state(sh);
    reload_statuses(sh);

    gtk_window_present(GTK_WINDOW(win));
    g_timeout_add(PUMP_MS, pump_tick, sh);
}

/* Rebuild the roster list from the core registry. Fingerprinted on
 * (row count + per-row id/status/live + filter + selection) so ticks
 * leave the selection alone unless the roster actually changed. */
static void refresh_roster(Shell *sh) {
    static char *last_fp = NULL;
    size_t n = bridge_session_count(sh->core);
    const char *fq =
        gtk_editable_get_text(GTK_EDITABLE(sh->filter));
    GString *fp = g_string_new(NULL);
    g_string_append_printf(fp, "%zu|%s|%zu|%d|", n, fq ? fq : "",
                           bridge_selected(sh->core),
                           bridge_history_expanded(sh->core));
    for (size_t i = 0; i < n; i++) {
        char *json = bridge_session_json(sh->core, i);
        char *id = json_string(json ? json : "{}", "id");
        bridge_string_free(json);
        g_string_append_printf(fp, "%d%s;", bridge_status(sh->core, i),
                               (id && bridge_is_live(sh->core, id)) ? "L"
                                                                   : "h");
        free(id);
    }
    if (last_fp && strcmp(last_fp, fp->str) == 0) {
        g_string_free(fp, TRUE);
        return;
    }
    free(last_fp);
    last_fp = g_strdup(fp->str);
    g_string_free(fp, TRUE);

    /* Clear and rebuild (fingerprint-gated: only on real change). */
    GtkWidget *child;
    while ((child = gtk_widget_get_first_child(GTK_WIDGET(sh->list)))) {
        gtk_list_box_remove(sh->list, child);
    }
    g_hash_table_remove_all(sh->row_widgets);
    for (size_t i = 0; i < n; i++) {
        char *json = bridge_session_json(sh->core, i);
        if (!json) {
            continue;
        }
        char *id = json_string(json, "id");
        char *title = json_string(json, "title");
        char *project = json_string(json, "project");
        char *harness = json_string(json, "harness");
        long long last = json_int(json, "last_active", 0);
        bridge_string_free(json);
        if (!id) {
            free(title);
            free(project);
            free(harness);
            continue;
        }
        int st = bridge_status(sh->core, i);
        if (st < 0) {
            st = 1;
        }
        GtkWidget *row = gtk_list_box_row_new();
        g_object_set_data_full(G_OBJECT(row), "staap-row-id", strdup(id),
                               free);
        g_object_set_data(G_OBJECT(row), "staap-idx",
                          GINT_TO_POINTER((int)i));
        GtkWidget *hbox = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 8);
        gtk_widget_set_margin_start(hbox, 8);
        gtk_widget_set_margin_end(hbox, 8);
        gtk_widget_set_margin_top(hbox, 6);
        gtk_widget_set_margin_bottom(hbox, 6);
        /* Non-color marker + title + detail (single shared format). */
        char *glyph = glyph_via_core(st);
        GtkWidget *mark =
            gtk_label_new(glyph ? glyph : "");
        bridge_string_free(glyph);
        GtkWidget *vbox = gtk_box_new(GTK_ORIENTATION_VERTICAL, 2);
        gtk_widget_set_hexpand(vbox, TRUE);
        GtkWidget *t = gtk_label_new(title ? title : "(untitled)");
        gtk_label_set_xalign(GTK_LABEL(t), 0);
        gtk_label_set_ellipsize(GTK_LABEL(t), PANGO_ELLIPSIZE_END);
        char *age = age_via_core(last);
        char *detail = g_strdup_printf(
            "%s · %s · %s%s", project ? project : "",
            harness ? harness : "", age ? age : "",
            bridge_is_live(sh->core, id) ? " · live" : "");
        bridge_string_free(age);
        GtkWidget *d = gtk_label_new(detail);
        g_free(detail);
        gtk_label_set_xalign(GTK_LABEL(d), 0);
        gtk_widget_add_css_class(d, "dim-label");
        gtk_box_append(GTK_BOX(vbox), t);
        gtk_box_append(GTK_BOX(vbox), d);
        char *hdr = section_for(st);
        GtkWidget *badge = gtk_label_new(hdr ? hdr : "");
        bridge_string_free(hdr);
        gtk_label_set_xalign(GTK_LABEL(badge), 1);
        gtk_widget_add_css_class(badge, "caption");
        gtk_box_append(GTK_BOX(hbox), mark);
        gtk_box_append(GTK_BOX(hbox), vbox);
        gtk_box_append(GTK_BOX(hbox), badge);
        gtk_list_box_row_set_child(GTK_LIST_BOX_ROW(row), hbox);
        gtk_list_box_append(sh->list, row);
        g_hash_table_insert(sh->row_widgets, strdup(id), badge);
        free(id);
        free(title);
        free(project);
        free(harness);
    }
    /* Restore the core selection into the list by row id (never by
     * position: the sort func + collapsed History mean visual position
     * and core index diverge, so a positional select would highlight a
     * different row than R/restart acts on). */
    char *sel_json = NULL;
    char *sel_id = NULL;
    {
        size_t sel = bridge_selected(sh->core);
        sel_json = bridge_session_json(sh->core, sel);
        sel_id = json_string(sel_json ? sel_json : "{}", "id");
    }
    if (sel_id) {
        GtkListBoxRow *at = NULL;
        for (GtkWidget *child =
                 gtk_widget_get_first_child(GTK_WIDGET(sh->list));
             child && !at;
             child = gtk_widget_get_next_sibling(child)) {
            if (!GTK_IS_LIST_BOX_ROW(child)) {
                continue;
            }
            const char *rid = g_object_get_data(G_OBJECT(child),
                                                "staap-row-id");
            if (rid && strcmp(rid, sel_id) == 0) {
                at = GTK_LIST_BOX_ROW(child);
            }
        }
        if (at) {
            gtk_list_box_select_row(sh->list, at);
        }
    }
    free(sel_id);
    bridge_string_free(sel_json);
    /* Sync the History affordance to the core (count label + expanded
     * state; the toggle handler is blocked during the programmatic
     * set so syncing never writes back). */
    size_t history_n = 0;
    for (size_t i = 0; i < n; i++) {
        char *json = bridge_session_json(sh->core, i);
        char *id = json_string(json ? json : "{}", "id");
        bridge_string_free(json);
        if (id && bridge_is_history(sh->core, id)) {
            history_n++;
        }
        free(id);
    }
    char *history_label = g_strdup_printf("History (%zu)", history_n);
    gtk_expander_set_label(sh->history_expander, history_label);
    g_free(history_label);
    g_signal_handlers_block_by_func(sh->history_expander,
                                    on_history_expanded, sh);
    gtk_expander_set_expanded(sh->history_expander,
                              bridge_history_expanded(sh->core) ? TRUE : FALSE);
    g_signal_handlers_unblock_by_func(sh->history_expander,
                                      on_history_expanded, sh);
}

static void on_activate(GtkApplication *app, gpointer data) {
    (void)app;
    build_ui(data);
}

/* ------------------------------------------------------------------ */
/* Entry point. */
/* ------------------------------------------------------------------ */

int main(int argc, char **argv) {
    Shell *sh = calloc(1, sizeof *sh);
    sh->cols = DEFAULT_COLS;
    sh->rows_grid = DEFAULT_ROWS;
    /* No roster copy, no live map, no theme pref: the core registry owns
     * rows/selection/filter/live-ness, persistence is automatic, and the
     * shell follows the system appearance. */
    sh->row_widgets =
        g_hash_table_new_full(g_str_hash, g_str_equal, free, NULL);

    sh->core = bridge_core_new();
    if (!sh->core) {
        g_printerr("staap-gtk: core init failed: %s\n",
                   bridge_last_error());
        return 1;
    }

    sh->app = ADW_APPLICATION(
        adw_application_new("com.example.staap", G_APPLICATION_DEFAULT_FLAGS));
    g_signal_connect(sh->app, "activate", G_CALLBACK(on_activate), sh);
    int rc = g_application_run(G_APPLICATION(sh->app), argc, argv);

    g_hash_table_destroy(sh->row_widgets);
    free(sh->fed_id);
    free(sh->fed_text);
    bridge_core_free(sh->core);
    g_object_unref(sh->app);
    free(sh);
    return rc;
}
