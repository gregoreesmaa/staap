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
 *   spawn       exactly one header New session button opens the
 *               in-place screen on the right side (folder x CLI + yolo,
 *               local or over SSH) → staap_run_spawn
 *               (this machine) or staap_run_spawn_remote (saved host)
 *   restart     header Restart / Ctrl+R → staap_run_restart (same id)
 *   close       header Close run / Ctrl+W → staap_run_close + autosave
 *               (no restart/close dropdown buttons exist in this shell:
 *               restart/close are header buttons + keyboard only)
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

/* In-place new-session form state (defined below; the Shell only holds
 * a pointer while the screen is open). */
typedef struct PickerUi PickerUi;

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
    /* In-place new-session screen (right side): replaces the old modal
     * picker dialog — shown every time New session is pressed. While
     * open the terminal scroll hides and the form fills the same
     * column; `ns` owns the live form, NULL when closed. */
    GtkScrolledWindow *ns_scroll;
    gboolean ns_open;
    PickerUi *ns;
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
static char *trimmed_or_null(const char *raw);

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
            sub = g_strdup("No runs yet — New session spawns one");
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
    /* The single New session button opens the in-place screen on every
     * press, so it stays sensitive with or without a selection (the
     * empty roster is a starting point, not a dead end). */
    (void)sh;
}

/* Forward: the single header button + Ctrl+N/Ctrl+Shift+N all open the
 * in-place new-session screen (defined with the picker below). */
static void show_new_session(Shell *sh);

/* ------------------------------------------------------------------ */
/* 2D new-session picker (folder x CLI + yolo, local or over SSH).     */
/* ------------------------------------------------------------------ */

/* Picker catalog row (parsed from the core CLI catalog JSON). */
typedef struct {
    char *id;
    char *path; /* NULL when unavailable */
    int available;
} PickerCli;

struct PickerUi {
    Shell *sh;
    /* In-place form root (parents modal children like Choose/Add). */
    GtkWidget *page;
    PickerCli *clis;
    size_t n_clis;
    GtkCheckButton **cli_btns;
    GtkWidget *cli_box; /* radio rows live here so a probe can rebuild them */
    GtkEntry *folder;
    /* SSH machine axis: dropdown over This machine + saved hosts + the
     * Add remote machine... row. NULL host means this machine. */
    GtkDropDown *machine;
    char **machine_hosts; /* parallel to dropdown rows; NULL = local/Add */
    size_t n_machine_rows;
    guint machine_sel; /* last non-Add selection (Add reverts to this) */
    char *machine_host; /* trimmed selected host, NULL = this machine */
    GtkDropDown *recent;
    char **recents;
    size_t n_recents;
    /* Remote folder browser (remote hosts only; local keeps Choose...). */
    GtkWidget *browser_wrap; /* hidden for This machine */
    GtkListBox *browser;
    GtkLabel *browse_path_label;
    GtkLabel *browse_status;
    char *browse_path; /* last listed remote dir (g_free family) */
    unsigned browse_gen; /* bumped per listing; stale arrivals drop */
    GtkDropDown *yolo;
    GtkLabel *preview;
    GtkLabel *status; /* probing line under the host axis, else empty */
    GtkLabel *error;
    unsigned probe_gen; /* bumped per host edit; stale arrivals are dropped */
};

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
    /* machine_host is already trimmed (NULL = this machine). */
    const char *host = pu->machine_host;
    if (host && text) {
        char *remote = g_strdup_printf("ssh %s: %s", host, text);
        gtk_label_set_text(pu->preview, remote ? remote : text);
        g_free(remote);
    } else {
        gtk_label_set_text(pu->preview, text ? text : "runs: muse");
    }
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

/* Live pickers (main thread only). The remote-catalog probe runs on a
 * worker thread and never touches a PickerUi off-thread: its result hops
 * back via g_idle_add and is applied only when the picker is still open
 * and the generation still matches (stale arrivals are dropped). */
static GSList *live_pickers = NULL;

/* Trim ASCII spaces/tabs; NULL/blank yields NULL (malloc'd otherwise). */
static char *trimmed_or_null(const char *raw) {
    if (!raw) {
        return NULL;
    }
    while (*raw == ' ' || *raw == '\t') {
        raw++;
    }
    size_t len = strlen(raw);
    while (len > 0 && (raw[len - 1] == ' ' || raw[len - 1] == '\t')) {
        len--;
    }
    if (len == 0) {
        return NULL;
    }
    char *out = malloc(len + 1);
    if (!out) {
        return NULL;
    }
    memcpy(out, raw, len);
    out[len] = '\0';
    return out;
}

/* Rebuild the CLI radio rows from a catalog JSON payload (local catalog
 * at open, remote catalog on probe arrival). The previously selected CLI
 * id is preserved when it is still listed and available; otherwise the
 * first available CLI is preselected. Handlers connect after
 * preselection, so the initial set_active stays silent — callers refresh
 * the yolo label and preview explicitly. */
static void picker_replace_clis(PickerUi *pu, const char *json) {
    char selected[128] = { 0 };
    for (size_t i = 0; i < pu->n_clis; i++) {
        if (pu->cli_btns && gtk_check_button_get_active(pu->cli_btns[i])) {
            snprintf(selected, sizeof selected, "%s", pu->clis[i].id);
            break;
        }
    }
    GtkWidget *child;
    while ((child = gtk_widget_get_first_child(pu->cli_box)) != NULL) {
        gtk_box_remove(GTK_BOX(pu->cli_box), child);
    }
    clis_free(pu->clis, pu->n_clis);
    free(pu->cli_btns);
    pu->clis = NULL;
    pu->cli_btns = NULL;
    pu->n_clis = 0;
    pu->clis = parse_clis(json ? json : "[]", &pu->n_clis);
    if (pu->clis) {
        pu->cli_btns =
            calloc(pu->n_clis ? pu->n_clis : 1, sizeof *pu->cli_btns);
    }
    if (!pu->clis || !pu->cli_btns) {
        return;
    }
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
                     ? (pu->clis[i].path ? pu->clis[i].path : pu->clis[i].id)
                     : "Install this CLI and ensure it is on PATH.");
        gtk_box_append(GTK_BOX(pu->cli_box), row);
        pu->cli_btns[i] = GTK_CHECK_BUTTON(row);
    }
    size_t pick = pu->n_clis;
    if (selected[0]) {
        for (size_t i = 0; i < pu->n_clis; i++) {
            if (pu->clis[i].available &&
                strcmp(pu->clis[i].id, selected) == 0) {
                pick = i;
                break;
            }
        }
    }
    if (pick == pu->n_clis) {
        for (size_t i = 0; i < pu->n_clis; i++) {
            if (pu->clis[i].available) {
                pick = i;
                break;
            }
        }
    }
    if (pick < pu->n_clis) {
        gtk_check_button_set_active(pu->cli_btns[pick], TRUE);
    }
    for (size_t i = 0; i < pu->n_clis; i++) {
        g_signal_connect(pu->cli_btns[i], "toggled",
                         G_CALLBACK(picker_cli_toggled), pu);
    }
}

typedef struct {
    PickerUi *pu;
    unsigned gen;
    char *host; /* owned */
} ProbeReq;

typedef struct {
    PickerUi *pu;
    unsigned gen;
    char *host; /* owned, for the failure line */
    char *json; /* owned (bridge string); NULL on allocation failure */
} ProbeDone;

/* Main-thread landing for a finished probe: apply only when the picker
 * is still open and the generation still matches. */
static gboolean picker_probe_deliver(gpointer data) {
    ProbeDone *done = data;
    int current = g_slist_find(live_pickers, done->pu) != NULL &&
                  done->pu->probe_gen == done->gen;
    if (current) {
        if (done->json) {
            picker_replace_clis(done->pu, done->json);
            gtk_label_set_text(done->pu->status, "");
        } else {
            char *msg = g_strdup_printf("Probe of %s failed.", done->host);
            gtk_label_set_text(done->pu->status, msg);
            g_free(msg);
        }
        picker_refresh_yolo(done->pu);
        picker_on_changed(done->pu);
    }
    bridge_string_free(done->json);
    g_free(done->host);
    free(done);
    return G_SOURCE_REMOVE;
}

/* Worker thread: one bounded ssh round trip, off the UI thread. Never
 * touches widgets or the picker; the result hops back via idle. */
static gpointer picker_probe_thread(gpointer data) {
    ProbeReq *req = data;
    char *json = bridge_clis_remote(req->host);
    ProbeDone *done = malloc(sizeof *done);
    if (done) {
        done->pu = req->pu;
        done->gen = req->gen;
        done->host = req->host; /* ownership moves */
        done->json = json;
        req->host = NULL;
        g_idle_add(picker_probe_deliver, done);
    } else {
        bridge_string_free(json);
    }
    g_free(req->host);
    free(req);
    return NULL;
}

/* Refresh the CLI catalog for `host` (NULL/blank = this machine).
 * Remote hosts probe in the background with stale-drop; returning to
 * blank restores the synchronous local catalog. */
static void picker_probe_remote(PickerUi *pu, const char *host) {
    pu->probe_gen++;
    if (!host || !*host) {
        gtk_label_set_text(pu->status, "");
        char *json = bridge_clis_json();
        picker_replace_clis(pu, json ? json : "[]");
        bridge_string_free(json);
        picker_refresh_yolo(pu);
        picker_on_changed(pu);
        return;
    }
    unsigned gen = pu->probe_gen;
    char *msg = g_strdup_printf("Probing %s…", host);
    gtk_label_set_text(pu->status, msg);
    g_free(msg);
    ProbeReq *req = malloc(sizeof *req);
    if (!req) {
        return;
    }
    req->pu = pu;
    req->gen = gen;
    req->host = g_strdup(host);
    if (!req->host) {
        free(req);
        return;
    }
    GError *err = NULL;
    GThread *th =
        g_thread_try_new("staap-remote-probe", picker_probe_thread, req, &err);
    g_clear_error(&err);
    if (!th) {
        gtk_label_set_text(pu->status,
                           "Probe unavailable; showing local CLIs.");
        g_free(req->host);
        free(req);
        return;
    }
    g_thread_unref(th); /* detached; the result hops back via idle */
}

/* Reload the recents dropdown: per-host persisted recents when remote
 * (core-owned, never network), local MRU otherwise. Selection resets to
 * the placeholder; the entry keeps whatever the user typed. */
static void picker_reload_recents(PickerUi *pu, const char *host) {
    char *json;
    if (host && *host) {
        json = bridge_remote_recent_json(pu->sh->core, host);
    } else {
        json = bridge_recent_json(pu->sh->core);
    }
    recents_free(pu->recents, pu->n_recents);
    pu->recents = parse_recents(json ? json : "[]", &pu->n_recents);
    bridge_string_free(json);
    if (!pu->recents) {
        return;
    }
    GtkStringList *list = GTK_STRING_LIST(gtk_string_list_new(NULL));
    gtk_string_list_append(list, "Type a folder…");
    for (size_t i = 0; i < pu->n_recents; i++) {
        gtk_string_list_append(list, pu->recents[i]);
    }
    gtk_drop_down_set_model(pu->recent, G_LIST_MODEL(list));
    gtk_drop_down_set_selected(pu->recent, 0);
}

/* Forward declarations for the machine-axis helpers (defined below;
 * picker_probe_remote / picker_reload_recents live above). */
static void picker_machine_changed(GObject *obj, GParamSpec *pspec,
                                   gpointer data);
static void picker_after_machine(PickerUi *pu);
static void picker_open_add_dialog(PickerUi *pu);
static void picker_browse_refresh(PickerUi *pu);

/* Saved-host row (parsed from the core saved-hosts JSON). */
typedef struct {
    char *name;
    char *host;
    char *folder; /* NULL when none */
} SavedHost;

/* Extract the string value of `key` inside the object span [start, end).
 * Returns a malloc'd unescaped value, or NULL when absent/non-string. */
static char *span_string(const char *start, const char *end, const char *key) {
    char pat[64];
    snprintf(pat, sizeof pat, "\"%s\"", key);
    const char *p = start;
    while ((p = strstr(p, pat)) != NULL && p < end) {
        const char *c = strchr(p + strlen(pat), ':');
        if (!c || c >= end) {
            return NULL;
        }
        c++;
        while (c < end &&
               (*c == ' ' || *c == '\t' || *c == '\n' || *c == '\r')) {
            c++;
        }
        if (c >= end || *c != '"') {
            return NULL; /* null or non-string */
        }
        c++;
        size_t cap = 32, len = 0;
        char *out = malloc(cap);
        if (!out) {
            return NULL;
        }
        int ok = 1;
        for (; c < end && *c != '"'; c++) {
            char ch = *c;
            if (ch == '\\' && c + 1 < end) {
                c++;
                ch = *c;
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
            out[len++] = ch;
        }
        if (!ok || c >= end) {
            free(out);
            return NULL;
        }
        out[len] = '\0';
        return out;
    }
    return NULL;
}

static SavedHost *parse_hosts(const char *json, size_t *n_out) {
    *n_out = 0;
    if (!json) {
        return NULL;
    }
    size_t cap = 4, n = 0;
    SavedHost *out = calloc(cap, sizeof *out);
    if (!out) {
        return NULL;
    }
    const char *p = json;
    while ((p = strchr(p, '{')) != NULL) {
        const char *e = strchr(p, '}');
        if (!e) {
            break;
        }
        char *name = span_string(p, e, "name");
        char *host = span_string(p, e, "host");
        char *folder = span_string(p, e, "default_folder");
        if (name && host) {
            if (n == cap) {
                cap *= 2;
                SavedHost *nb = realloc(out, cap * sizeof *out);
                if (!nb) {
                    free(name);
                    free(host);
                    free(folder);
                    break;
                }
                out = nb;
            }
            out[n].name = name;
            out[n].host = host;
            out[n].folder = folder;
            n++;
        } else {
            free(name);
            free(host);
            free(folder);
        }
        p = e + 1;
    }
    *n_out = n;
    return out;
}

static void hosts_free(SavedHost *hosts, size_t n) {
    if (!hosts) {
        return;
    }
    for (size_t i = 0; i < n; i++) {
        free(hosts[i].name);
        free(hosts[i].host);
        free(hosts[i].folder);
    }
    free(hosts);
}

/* Rebuild the machine dropdown (This machine + saved hosts + the Add
 * row). `select_host` names the host to preselect, if still saved. */
static void picker_rebuild_machine(PickerUi *pu, const char *select_host) {
    char *hosts_json = bridge_saved_hosts_json(pu->sh->core);
    size_t n = 0;
    SavedHost *hosts = parse_hosts(hosts_json ? hosts_json : "[]", &n);
    bridge_string_free(hosts_json);
    if (pu->machine_hosts) {
        for (size_t i = 0; i < pu->n_machine_rows; i++) {
            g_free(pu->machine_hosts[i]);
        }
        g_free(pu->machine_hosts);
        pu->machine_hosts = NULL;
        pu->n_machine_rows = 0;
    }
    pu->machine_hosts = g_malloc0((n + 2) * sizeof *pu->machine_hosts);
    if (!pu->machine_hosts) {
        hosts_free(hosts, n);
        return;
    }
    pu->n_machine_rows = n + 2;
    GtkStringList *list = GTK_STRING_LIST(gtk_string_list_new(NULL));
    gtk_string_list_append(list, "This machine");
    pu->machine_hosts[0] = NULL;
    guint sel = 0;
    for (size_t i = 0; i < n; i++) {
        char *label = g_strdup_printf("%s - %s", hosts[i].name, hosts[i].host);
        gtk_string_list_append(list, label);
        g_free(label);
        pu->machine_hosts[i + 1] = g_strdup(hosts[i].host);
        if (select_host && strcmp(select_host, hosts[i].host) == 0) {
            sel = (guint)(i + 1);
        }
    }
    hosts_free(hosts, n);
    gtk_string_list_append(list, "Add remote machine...");
    pu->machine_hosts[n + 1] = NULL;
    g_signal_handlers_block_by_func(pu->machine, picker_machine_changed, pu);
    gtk_drop_down_set_model(pu->machine, G_LIST_MODEL(list));
    gtk_drop_down_set_selected(pu->machine, sel);
    g_signal_handlers_unblock_by_func(pu->machine, picker_machine_changed, pu);
    pu->machine_sel = sel;
    g_free(pu->machine_host);
    pu->machine_host = (sel > 0 && pu->machine_hosts[sel])
        ? g_strdup(pu->machine_hosts[sel])
        : NULL;
}

/* Refresh everything the machine axis owns: folder default inherit,
 * recents, remote catalog probe, browser listing. */
static void picker_after_machine(PickerUi *pu) {
    char *host = pu->machine_host ? g_strdup(pu->machine_host) : NULL;
    if (host) {
        /* A saved host default fills a blank folder entry (never
         * clobbers what the user typed). */
        char *cur = trimmed_or_null(
            gtk_editable_get_text(GTK_EDITABLE(pu->folder)));
        if (!cur) {
            char *dflt = bridge_remote_default_folder(pu->sh->core, host);
            if (dflt && *dflt) {
                gtk_editable_set_text(GTK_EDITABLE(pu->folder), dflt);
            }
            bridge_string_free(dflt);
        }
        g_free(cur);
    }
    gtk_entry_set_placeholder_text(
        pu->folder, host ? "Blank = remote login directory"
                         : "Blank = current folder");
    picker_reload_recents(pu, host);
    picker_probe_remote(pu, host);
    picker_browse_refresh(pu);
    g_free(host);
    picker_on_changed(pu);
}

static void picker_machine_changed(GObject *obj, GParamSpec *pspec,
                                   gpointer data) {
    (void)pspec;
    PickerUi *pu = data;
    guint idx = gtk_drop_down_get_selected(GTK_DROP_DOWN(obj));
    if (pu->n_machine_rows > 0 && idx == pu->n_machine_rows - 1) {
        /* Add row: revert to the previous pick, open the SSH dialog. */
        g_signal_handlers_block_by_func(pu->machine, picker_machine_changed,
                                        pu);
        gtk_drop_down_set_selected(pu->machine, pu->machine_sel);
        g_signal_handlers_unblock_by_func(pu->machine, picker_machine_changed,
                                          pu);
        picker_open_add_dialog(pu);
        return;
    }
    pu->machine_sel = idx;
    g_free(pu->machine_host);
    pu->machine_host =
        (idx > 0 && idx < pu->n_machine_rows && pu->machine_hosts[idx])
        ? g_strdup(pu->machine_hosts[idx])
        : NULL;
    picker_after_machine(pu);
}

/* Remote folder browser (remote hosts only): one bounded ssh listing
 * per Refresh/Up/navigate/machine-change — never per keystroke or per
 * picker open. Listing/probing share the stale-drop discipline: worker
 * threads never touch widgets; results hop back via idle and apply
 * only when the screen is still open and the generation matches. */
typedef struct {
    PickerUi *pu;
    unsigned gen;
    char *host; /* owned */
    char *path; /* owned */
} BrowseReq;

typedef struct {
    PickerUi *pu;
    unsigned gen;
    char *path; /* owned: the listed path */
    char *json; /* owned (bridge string) */
} BrowseDone;

static char *remote_join(const char *base, const char *name) {
    if (!base || !*base) {
        return g_strdup(name);
    }
    size_t n = strlen(base);
    if (base[n - 1] == '/') {
        return g_strdup_printf("%s%s", base, name);
    }
    return g_strdup_printf("%s/%s", base, name);
}

static char *remote_parent(const char *path) {
    if (!path || !*path) {
        return NULL;
    }
    char *cpy = g_strdup(path);
    size_t n = strlen(cpy);
    while (n > 1 && cpy[n - 1] == '/') {
        cpy[--n] = '\0';
    }
    char *slash = strrchr(cpy, '/');
    if (!slash) {
        g_free(cpy);
        return NULL;
    }
    if (slash == cpy) {
        cpy[1] = '\0';
    } else {
        *slash = '\0';
    }
    return cpy;
}

/* Fill the browser rows from a listing payload. Directories navigate
 * on activate (filling the folder entry); files are inert. */
static void picker_render_browser(PickerUi *pu, const char *json) {
    GtkWidget *child;
    while ((child = gtk_widget_get_first_child(GTK_WIDGET(pu->browser)))) {
        gtk_list_box_remove(pu->browser, child);
    }
    size_t shown = 0;
    if (json) {
        const char *p = json;
        while ((p = strchr(p, '{')) != NULL) {
            const char *e = strchr(p, '}');
            if (!e) {
                break;
            }
            char *name = span_string(p, e, "name");
            int is_dir = 0;
            {
                const char *k = strstr(p, "\"is_dir\"");
                if (k && k < e) {
                    const char *t = strstr(k, "true");
                    is_dir = (t && t < e) ? 1 : 0;
                }
            }
            if (name && *name && strcmp(name, ".") != 0 &&
                strcmp(name, "..") != 0) {
                char *full = remote_join(pu->browse_path, name);
                char *label = is_dir ? g_strdup_printf("%s/", name)
                                     : g_strdup(name);
                GtkWidget *row = gtk_list_box_row_new();
                GtkWidget *lab = gtk_label_new(label);
                g_free(label);
                gtk_label_set_xalign(GTK_LABEL(lab), 0);
                gtk_label_set_ellipsize(GTK_LABEL(lab), PANGO_ELLIPSIZE_MIDDLE);
                gtk_list_box_row_set_child(GTK_LIST_BOX_ROW(row), lab);
                g_object_set_data_full(G_OBJECT(row), "staap-path", full,
                                       g_free);
                g_object_set_data(G_OBJECT(row), "staap-dir",
                                  GINT_TO_POINTER(is_dir));
                if (!is_dir) {
                    gtk_widget_set_sensitive(row, FALSE);
                }
                gtk_list_box_append(pu->browser, row);
                shown++;
            }
            free(name);
            p = e + 1;
        }
    }
    if (shown == 0) {
        gtk_label_set_text(pu->browse_status,
            "Empty or unreachable - check the path, or Test the host.");
    } else {
        char *msg = g_strdup_printf(
            "%zu entries - open a folder to fill the entry.", shown);
        gtk_label_set_text(pu->browse_status, msg);
        g_free(msg);
    }
}

static gboolean picker_browse_deliver(gpointer data) {
    BrowseDone *done = data;
    int current = g_slist_find(live_pickers, done->pu) != NULL &&
                  done->pu->browse_gen == done->gen;
    if (current) {
        g_free(done->pu->browse_path);
        done->pu->browse_path = g_strdup(done->path);
        gtk_label_set_text(done->pu->browse_path_label, done->path);
        picker_render_browser(done->pu, done->json);
    }
    g_free(done->path);
    bridge_string_free(done->json);
    free(done);
    return G_SOURCE_REMOVE;
}

static gpointer picker_browse_thread(gpointer data) {
    BrowseReq *req = data;
    char *json = bridge_remote_list_json(req->host, req->path);
    BrowseDone *done = malloc(sizeof *done);
    if (done) {
        done->pu = req->pu;
        done->gen = req->gen;
        done->path = req->path; /* ownership moves */
        req->path = NULL;
        done->json = json;
        g_idle_add(picker_browse_deliver, done);
    } else {
        bridge_string_free(json);
    }
    g_free(req->host);
    g_free(req->path);
    free(req);
    return NULL;
}

static void picker_browse_refresh(PickerUi *pu) {
    pu->browse_gen++;
    if (!pu->machine_host) {
        gtk_widget_set_visible(pu->browser_wrap, FALSE);
        gtk_label_set_text(pu->browse_status, "");
        return;
    }
    gtk_widget_set_visible(pu->browser_wrap, TRUE);
    /* The folder entry wins when non-blank, else the last listed dir,
     * else the host default (saved default, then host MRU, then home).
     * The entry is the single source the Start action reads. */
    char *entry = trimmed_or_null(
        gtk_editable_get_text(GTK_EDITABLE(pu->folder)));
    char *path = entry;
    if (!path) {
        if (pu->browse_path && *pu->browse_path) {
            path = g_strdup(pu->browse_path);
        } else {
            char *dflt =
                bridge_remote_default_folder(pu->sh->core, pu->machine_host);
            path = (dflt && *dflt) ? g_strdup(dflt) : NULL;
            bridge_string_free(dflt);
        }
    }
    if (!path) {
        gtk_label_set_text(pu->browse_status,
            "No default folder - type a path above, then press Refresh.");
        picker_render_browser(pu, "[]");
        return;
    }
    unsigned gen = pu->browse_gen;
    char *msg = g_strdup_printf("Listing %s ...", path);
    gtk_label_set_text(pu->browse_status, msg);
    g_free(msg);
    BrowseReq *req = malloc(sizeof *req);
    if (!req) {
        g_free(path);
        return;
    }
    req->pu = pu;
    req->gen = gen;
    req->host = g_strdup(pu->machine_host);
    req->path = path; /* ownership moves */
    if (!req->host) {
        g_free(req->path);
        free(req);
        return;
    }
    GError *err = NULL;
    GThread *th = g_thread_try_new("staap-remote-list", picker_browse_thread,
                                   req, &err);
    g_clear_error(&err);
    if (!th) {
        gtk_label_set_text(pu->browse_status,
            "Listing unavailable; check the folder and press Start.");
        g_free(req->host);
        g_free(req->path);
        free(req);
        return;
    }
    g_thread_unref(th); /* detached; the result hops back via idle */
}

static void picker_browser_row(GtkListBox *box, GtkListBoxRow *row,
                               gpointer data) {
    (void)box;
    PickerUi *pu = data;
    if (!GPOINTER_TO_INT(g_object_get_data(G_OBJECT(row), "staap-dir"))) {
        return; /* files are inert */
    }
    const char *path = g_object_get_data(G_OBJECT(row), "staap-path");
    if (!path) {
        return;
    }
    gtk_editable_set_text(GTK_EDITABLE(pu->folder), path);
    picker_browse_refresh(pu);
    picker_on_changed(pu);
}

static void picker_browse_up(GtkButton *btn, gpointer data) {
    (void)btn;
    PickerUi *pu = data;
    char *cur = trimmed_or_null(
        gtk_editable_get_text(GTK_EDITABLE(pu->folder)));
    if (!cur && pu->browse_path) {
        cur = g_strdup(pu->browse_path);
    }
    if (!cur) {
        return;
    }
    char *parent = remote_parent(cur);
    g_free(cur);
    if (!parent) {
        return;
    }
    gtk_editable_set_text(GTK_EDITABLE(pu->folder), parent);
    g_free(parent);
    picker_browse_refresh(pu);
    picker_on_changed(pu);
}

static void picker_browse_go(GtkButton *btn, gpointer data) {
    (void)btn;
    /* Manual refresh only (no per-keystroke ssh): lists the entry path. */
    picker_browse_refresh(data);
    picker_on_changed(data);
}

/* SSH add dialog (opened from the machine dropdown's Add row): display
 * name, user@hostname-or-alias host, default folder, Test/Save/Cancel.
 * Save stores a saved host via the core; Test runs the core tester
 * off-thread and reports reachable plus detected CLIs, or the core
 * reason line on failure. */
typedef struct {
    PickerUi *pu;
    GtkEntry *name;
    GtkEntry *host;
    GtkEntry *folder;
    GtkLabel *result;
    GtkLabel *error;
    AdwDialog *dialog;
    unsigned test_gen;
    int alive; /* cleared by the closed handler; stale tests drop */
} AddUi;

typedef struct {
    AddUi *au;
    unsigned gen;
    char *host; /* owned */
} AddTestReq;

typedef struct {
    AddUi *au;
    unsigned gen;
    char *host; /* owned */
    char *json; /* owned (bridge string) */
} AddTestDone;

/* Extract the core tester `message` string from the tester JSON
 * (`{"reachable":bool,"reason":...,"message":...,"clis":[...]}`).
 * Returns a malloc'd copy (JSON escapes resolved) or NULL when absent.
 * The core copy is static prose, but escapes resolve so a future
 * message never breaks the scan. */
static char *tester_message(const char *json) {
    if (!json) {
        return NULL;
    }
    const char *p = strstr(json, "\"message\"");
    if (!p) {
        return NULL;
    }
    p = strchr(p, ':');
    if (!p) {
        return NULL;
    }
    p++;
    while (*p == ' ' || *p == '\t') {
        p++;
    }
    if (strncmp(p, "null", 4) == 0) {
        return NULL;
    }
    if (*p != '"') {
        return NULL;
    }
    p++;
    GString *out = g_string_new(NULL);
    if (!out) {
        return NULL;
    }
    while (*p && *p != '"') {
        if (*p == '\\' && p[1]) {
            p++;
            switch (*p) {
            case 'n':
                g_string_append_c(out, '\n');
                break;
            case 't':
                g_string_append_c(out, '\t');
                break;
            case 'r':
                g_string_append_c(out, '\r');
                break;
            default:
                g_string_append_c(out, *p);
                break;
            }
            p++;
        } else {
            g_string_append_c(out, *p);
            p++;
        }
    }
    if (*p != '"') {
        g_string_free(out, TRUE);
        return NULL;
    }
    return g_string_free(out, FALSE);
}

static gboolean add_test_deliver(gpointer data) {
    AddTestDone *done = data;
    if (done->au->alive && done->au->test_gen == done->gen) {
        int reachable =
            done->json && strstr(done->json, "\"reachable\":true");
        if (reachable) {
            /* Name the detected CLIs (the test payload reuses the
             * catalog shape, so parse_clis reads it directly). */
            size_t n = 0;
            PickerCli *clis = parse_clis(done->json, &n);
            GString *have = g_string_new(NULL);
            size_t nhave = 0;
            for (size_t i = 0; i < n; i++) {
                if (clis[i].available) {
                    g_string_append_printf(have, "%s%s", nhave ? ", " : "",
                                           clis[i].id);
                    nhave++;
                }
            }
            char *msg = nhave ? g_strdup_printf("Reachable - CLIs: %s.",
                                                              have->str)
                              : g_strdup("Reachable - no known CLIs found.");
            gtk_label_set_text(done->au->result, msg);
            g_free(msg);
            g_string_free(have, TRUE);
            clis_free(clis, n);
        } else {
            /* Failure copy renders the core `message` verbatim
             * (dumb-shell contract: no per-shell error parsing); the
             * host stays in parens as before for context. */
            char *reason = tester_message(done->json);
            char *msg;
            if (reason) {
                msg = g_strdup_printf("%s (%s).", reason, done->host);
                g_free(reason);
            } else {
                msg = g_strdup_printf(
                    "Unreachable - check the host name and SSH access (%s).",
                    done->host);
            }
            gtk_label_set_text(done->au->result, msg);
            g_free(msg);
        }
    }
    g_free(done->host);
    bridge_string_free(done->json);
    free(done);
    return G_SOURCE_REMOVE;
}

static gpointer add_test_thread(gpointer data) {
    AddTestReq *req = data;
    char *json = bridge_remote_test_json(req->host);
    AddTestDone *done = malloc(sizeof *done);
    if (done) {
        done->au = req->au;
        done->gen = req->gen;
        done->host = req->host; /* ownership moves */
        req->host = NULL;
        done->json = json;
        g_idle_add(add_test_deliver, done);
    } else {
        bridge_string_free(json);
    }
    g_free(req->host);
    free(req);
    return NULL;
}

static void add_test_clicked(GtkButton *btn, gpointer data) {
    (void)btn;
    AddUi *au = data;
    au->test_gen++;
    char *host = trimmed_or_null(
        gtk_editable_get_text(GTK_EDITABLE(au->host)));
    if (!host) {
        gtk_label_set_text(au->result, "Enter a host to test.");
        return;
    }
    unsigned gen = au->test_gen;
    char *msg = g_strdup_printf("Testing %s ...", host);
    gtk_label_set_text(au->result, msg);
    g_free(msg);
    AddTestReq *req = malloc(sizeof *req);
    if (!req) {
        free(host);
        return;
    }
    req->au = au;
    req->gen = gen;
    req->host = g_strdup(host);
    free(host);
    if (!req->host) {
        free(req);
        return;
    }
    GError *err = NULL;
    GThread *th = g_thread_try_new("staap-remote-test", add_test_thread,
                                   req, &err);
    g_clear_error(&err);
    if (!th) {
        gtk_label_set_text(au->result, "Test unavailable right now.");
        g_free(req->host);
        free(req);
        return;
    }
    g_thread_unref(th); /* detached; the result hops back via idle */
}

static void add_save_clicked(GtkButton *btn, gpointer data) {
    (void)btn;
    AddUi *au = data;
    char *name = trimmed_or_null(
        gtk_editable_get_text(GTK_EDITABLE(au->name)));
    char *host = trimmed_or_null(
        gtk_editable_get_text(GTK_EDITABLE(au->host)));
    char *folder = trimmed_or_null(
        gtk_editable_get_text(GTK_EDITABLE(au->folder)));
    if (!name) {
        gtk_label_set_text(au->error, "Give the machine a display name.");
        goto out;
    }
    if (!host) {
        gtk_label_set_text(au->error,
            "Enter a host: user@hostname or an ssh-config alias.");
        goto out;
    }
    {
        char *err = NULL;
        if (bridge_saved_host_add(au->pu->sh->core, name, host, folder,
                                  &err) != 0) {
            gtk_label_set_text(au->error, err ? err : "Could not save.");
            free(err);
            goto out;
        }
        char *serr = NULL;
        if (bridge_core_save(au->pu->sh->core, &serr) != 0) {
            free(serr);
        }
    }
    {
        /* The closed handler frees au synchronously inside
         * adw_dialog_close, so keep only owned copies past it. */
        PickerUi *owner = au->pu;
        char *saved = g_strdup(host);
        adw_dialog_close(au->dialog);
        picker_rebuild_machine(owner, saved);
        g_free(saved);
        picker_after_machine(owner);
    }
out:
    free(name);
    free(host);
    free(folder);
}

static void add_dialog_closed(AdwDialog *dialog, gpointer data) {
    (void)dialog;
    AddUi *au = data;
    au->alive = 0;
    au->test_gen++;
    free(au);
}

static void picker_open_add_dialog(PickerUi *pu) {
    GtkWindow *win = GTK_WINDOW(gtk_widget_get_root(GTK_WIDGET(pu->page)));
    AddUi *au = calloc(1, sizeof *au);
    if (!au) {
        return;
    }
    au->pu = pu;
    au->alive = 1;

    AdwDialog *dialog = ADW_DIALOG(adw_dialog_new());
    au->dialog = dialog;
    adw_dialog_set_title(dialog, "Add remote machine");
    adw_dialog_set_content_width(dialog, 380);
    g_signal_connect(dialog, "closed", G_CALLBACK(add_dialog_closed), au);

    GtkWidget *box = gtk_box_new(GTK_ORIENTATION_VERTICAL, 10);
    gtk_widget_set_margin_start(box, 20);
    gtk_widget_set_margin_end(box, 20);
    gtk_widget_set_margin_top(box, 20);
    gtk_widget_set_margin_bottom(box, 20);

    GtkWidget *name_label = gtk_label_new("Display name");
    gtk_label_set_xalign(GTK_LABEL(name_label), 0);
    gtk_box_append(GTK_BOX(box), name_label);
    au->name = GTK_ENTRY(gtk_entry_new());
    gtk_entry_set_placeholder_text(au->name, "e.g. Build box");
    gtk_box_append(GTK_BOX(box), GTK_WIDGET(au->name));

    GtkWidget *host_label = gtk_label_new("Host");
    gtk_label_set_xalign(GTK_LABEL(host_label), 0);
    gtk_box_append(GTK_BOX(box), host_label);
    au->host = GTK_ENTRY(gtk_entry_new());
    gtk_entry_set_placeholder_text(
        au->host, "user@hostname, e.g. alice@buildbox (or ssh-config alias)");
    gtk_box_append(GTK_BOX(box), GTK_WIDGET(au->host));

    GtkWidget *folder_label = gtk_label_new("Default folder (optional)");
    gtk_label_set_xalign(GTK_LABEL(folder_label), 0);
    gtk_box_append(GTK_BOX(box), folder_label);
    au->folder = GTK_ENTRY(gtk_entry_new());
    gtk_entry_set_placeholder_text(au->folder,
                                   "Blank = remote home directory");
    gtk_box_append(GTK_BOX(box), GTK_WIDGET(au->folder));

    au->result = GTK_LABEL(gtk_label_new(""));
    gtk_label_set_xalign(au->result, 0);
    gtk_label_set_wrap(au->result, TRUE);
    gtk_box_append(GTK_BOX(box), GTK_WIDGET(au->result));
    au->error = GTK_LABEL(gtk_label_new(""));
    gtk_label_set_xalign(au->error, 0);
    gtk_widget_add_css_class(GTK_WIDGET(au->error), "error");
    gtk_box_append(GTK_BOX(box), GTK_WIDGET(au->error));

    GtkWidget *actions = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 8);
    gtk_widget_set_halign(actions, GTK_ALIGN_END);
    GtkWidget *test = gtk_button_new_with_label("Test");
    gtk_widget_set_tooltip_text(test, "Check SSH reachability now");
    g_signal_connect(test, "clicked", G_CALLBACK(add_test_clicked), au);
    gtk_box_append(GTK_BOX(actions), test);
    GtkWidget *cancel = gtk_button_new_with_label("Cancel");
    g_signal_connect_swapped(cancel, "clicked",
                             G_CALLBACK(adw_dialog_close), dialog);
    gtk_box_append(GTK_BOX(actions), cancel);
    GtkWidget *save = gtk_button_new_with_label("Save");
    gtk_widget_add_css_class(save, "suggested-action");
    g_signal_connect(save, "clicked", G_CALLBACK(add_save_clicked), au);
    gtk_box_append(GTK_BOX(actions), save);
    gtk_box_append(GTK_BOX(box), actions);

    adw_dialog_set_child(dialog, box);
    adw_dialog_present(dialog, GTK_WIDGET(win));
}

static void picker_free(PickerUi *pu) {
    if (!pu) {
        return;
    }
    live_pickers = g_slist_remove(live_pickers, pu);
    clis_free(pu->clis, pu->n_clis);
    recents_free(pu->recents, pu->n_recents);
    free(pu->cli_btns);
    if (pu->machine_hosts) {
        for (size_t i = 0; i < pu->n_machine_rows; i++) {
            g_free(pu->machine_hosts[i]);
        }
        g_free(pu->machine_hosts);
    }
    g_free(pu->machine_host);
    g_free(pu->browse_path);
    free(pu);
}

/* Spawn the confirmed combination: folder x CLI + one-shot yolo, local
 * or over SSH. A missing local folder refuses inline (the screen stays
 * open for a fix); remote paths skip that gate (they cannot be stated
 * locally) and let the core report failures. A spawn failure toasts and
 * closes (the core error names the fix). */
static void hide_new_session(Shell *sh);

static void picker_confirm(PickerUi *pu) {
    Shell *sh = pu->sh;
    char *folder = trimmed_or_null(
        gtk_editable_get_text(GTK_EDITABLE(pu->folder)));
    char *host =
        pu->machine_host ? g_strdup(pu->machine_host) : NULL;
    if (!host && folder && !g_file_test(folder, G_FILE_TEST_IS_DIR)) {
        char *msg = g_strdup_printf("No such folder: %s", folder);
        gtk_label_set_text(pu->error, msg);
        g_free(msg);
        free(folder);
        free(host);
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
    int rc;
    if (host) {
        /* Remote: the local PTY runs `ssh -t <host> <cli...>` under the
         * shared cap, so interactive auth prompts surface in the
         * terminal; a blank folder means the remote login directory. */
        rc = bridge_run_spawn_remote(sh->core, host, folder, cli, yolo,
                                     (unsigned)sh->cols,
                                     (unsigned)sh->rows_grid, id, sizeof id,
                                     &err);
    } else {
        rc = bridge_run_spawn(sh->core, cli, folder, yolo,
                              (unsigned)sh->cols,
                              (unsigned)sh->rows_grid, id, sizeof id, &err);
    }
    if (rc != 0) {
        if (err && strstr(err, "live runs")) {
            gtk_label_set_text(pu->error, err);
            free(err);
            free(folder);
            free(host);
            return;
        }
        char *msg = g_strdup_printf("Could not start session: %s",
                                    err ? err : "unknown error");
        toast(sh, msg);
        g_free(msg);
        free(err);
        free(folder);
        g_free(host);
        hide_new_session(sh);
        return;
    }
    char *preview = bridge_spawn_preview(cli, folder, yolo);
    if (host && preview) {
        char *remote = g_strdup_printf("ssh %s: %s", host, preview);
        toast(sh, remote ? remote : preview);
        g_free(remote);
    } else {
        toast(sh, preview ? preview : "Session started.");
    }
    free(preview);
    free(folder);
    g_free(host);
    hide_new_session(sh);
    refresh_roster(sh);
    show_selected_in_terminal(sh);
    update_spawn_state(sh);
    reload_statuses(sh);
}

static void picker_spawn_clicked(GtkButton *btn, gpointer data) {
    (void)btn;
    picker_confirm(data);
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
/* Hide the in-place screen and return to the terminal. */
static void hide_new_session(Shell *sh) {
    if (!sh->ns_open) {
        return;
    }
    sh->ns_open = FALSE;
    PickerUi *pu = sh->ns;
    sh->ns = NULL;
    /* Dropping the child destroys the form; stale thread arrivals find
     * no live picker and drop (same discipline as the old dialog). */
    gtk_scrolled_window_set_child(sh->ns_scroll, NULL);
    picker_free(pu);
    gtk_widget_set_visible(GTK_WIDGET(sh->ns_scroll), FALSE);
    gtk_widget_set_visible(GTK_WIDGET(sh->term_scroll), TRUE);
}

static void picker_cancel_clicked(GtkButton *btn, gpointer data) {
    (void)btn;
    hide_new_session(((PickerUi *)data)->sh);
}

/* Open the in-place new-session screen on the right side: folder entry
 * (+ Choose dialog for this machine, remote browser for saved hosts) +
 * recents, machine dropdown (This machine / saved hosts / Add row), CLI
 * radio rows, yolo tri-state, and the live `runs: ...` preview. Catalog,
 * recents, and saved hosts re-read on every open so the list is never
 * stale. Shown every time New session is pressed. */
static void show_new_session(Shell *sh) {
    if (sh->ns_open) {
        hide_new_session(sh);
    }
    PickerUi *pu = calloc(1, sizeof *pu);
    if (!pu) {
        return;
    }
    pu->sh = sh;
    live_pickers = g_slist_prepend(live_pickers, pu);

    /* Radio rows live in their own box so a remote probe can rebuild
     * them; the initial build is the synchronous local catalog. */
    pu->cli_box = gtk_box_new(GTK_ORIENTATION_VERTICAL, 6);
    {
        char *clis_json = bridge_clis_json();
        picker_replace_clis(pu, clis_json ? clis_json : "[]");
        bridge_string_free(clis_json);
    }
    char *recents_json = bridge_recent_json(sh->core);
    pu->recents =
        parse_recents(recents_json ? recents_json : "[]", &pu->n_recents);
    bridge_string_free(recents_json);
    if (!pu->recents) {
        picker_free(pu);
        return;
    }

    GtkWidget *box = gtk_box_new(GTK_ORIENTATION_VERTICAL, 12);
    gtk_widget_set_margin_start(box, 20);
    gtk_widget_set_margin_end(box, 20);
    gtk_widget_set_margin_top(box, 20);
    gtk_widget_set_margin_bottom(box, 20);
    pu->page = box;

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
    GtkWidget *choose = gtk_button_new_with_label("Choose...");
    g_signal_connect(choose, "clicked", G_CALLBACK(picker_choose_folder),
                     pu);
    gtk_box_append(GTK_BOX(folder_row), choose);
    gtk_box_append(GTK_BOX(box), folder_row);
    {
        GtkStringList *recent_list =
            GTK_STRING_LIST(gtk_string_list_new(NULL));
        gtk_string_list_append(recent_list, "Type a folder...");
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

    /* SSH machine axis: This machine, saved hosts, Add remote
     * machine... (blank = this machine, local launch). */
    GtkWidget *host_label = gtk_label_new("Which machine?");
    gtk_label_set_xalign(GTK_LABEL(host_label), 0);
    gtk_widget_add_css_class(host_label, "heading");
    gtk_box_append(GTK_BOX(box), host_label);
    pu->machine = GTK_DROP_DOWN(gtk_drop_down_new(NULL, NULL));
    g_signal_connect(pu->machine, "notify::selected",
                     G_CALLBACK(picker_machine_changed), pu);
    gtk_box_append(GTK_BOX(box), GTK_WIDGET(pu->machine));
    GtkWidget *host_hint =
        gtk_label_new("This machine, a saved host, or add one - "
                      "e.g. alice@buildbox");
    gtk_label_set_xalign(GTK_LABEL(host_hint), 0);
    gtk_widget_add_css_class(GTK_WIDGET(host_hint), "dim-label");
    gtk_box_append(GTK_BOX(box), host_hint);
    pu->status = GTK_LABEL(gtk_label_new(""));
    gtk_label_set_xalign(pu->status, 0);
    gtk_widget_add_css_class(GTK_WIDGET(pu->status), "dim-label");
    gtk_box_append(GTK_BOX(box), GTK_WIDGET(pu->status));

    /* Remote folder browser (saved hosts only; hidden otherwise). */
    pu->browser_wrap = gtk_box_new(GTK_ORIENTATION_VERTICAL, 6);
    {
        GtkWidget *browse_row = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 8);
        pu->browse_path_label = GTK_LABEL(gtk_label_new(""));
        gtk_label_set_xalign(pu->browse_path_label, 0);
        gtk_label_set_ellipsize(pu->browse_path_label,
                                PANGO_ELLIPSIZE_MIDDLE);
        gtk_widget_set_hexpand(GTK_WIDGET(pu->browse_path_label), TRUE);
        gtk_box_append(GTK_BOX(browse_row),
                       GTK_WIDGET(pu->browse_path_label));
        GtkWidget *up = gtk_button_new_with_label("Up");
        g_signal_connect(up, "clicked", G_CALLBACK(picker_browse_up), pu);
        gtk_box_append(GTK_BOX(browse_row), up);
        GtkWidget *refresh = gtk_button_new_with_label("Refresh");
        gtk_widget_set_tooltip_text(refresh, "List the folder above");
        g_signal_connect(refresh, "clicked", G_CALLBACK(picker_browse_go),
                         pu);
        gtk_box_append(GTK_BOX(browse_row), refresh);
        gtk_box_append(GTK_BOX(pu->browser_wrap), browse_row);
        pu->browser = GTK_LIST_BOX(gtk_list_box_new());
        g_signal_connect(pu->browser, "row-activated",
                         G_CALLBACK(picker_browser_row), pu);
        GtkWidget *browse_scroll = gtk_scrolled_window_new();
        gtk_widget_set_size_request(browse_scroll, -1, 140);
        gtk_scrolled_window_set_child(GTK_SCROLLED_WINDOW(browse_scroll),
                                      GTK_WIDGET(pu->browser));
        gtk_box_append(GTK_BOX(pu->browser_wrap), browse_scroll);
        GtkWidget *browse_hint =
            gtk_label_new("Open a folder to fill the entry above.");
        gtk_label_set_xalign(GTK_LABEL(browse_hint), 0);
        gtk_widget_add_css_class(GTK_WIDGET(browse_hint), "dim-label");
        gtk_box_append(GTK_BOX(pu->browser_wrap), browse_hint);
    }
    pu->browse_status = GTK_LABEL(gtk_label_new(""));
    gtk_label_set_xalign(pu->browse_status, 0);
    gtk_widget_add_css_class(GTK_WIDGET(pu->browse_status), "dim-label");
    gtk_box_append(GTK_BOX(pu->browser_wrap),
                   GTK_WIDGET(pu->browse_status));
    gtk_widget_set_visible(pu->browser_wrap, FALSE);
    gtk_box_append(GTK_BOX(box), pu->browser_wrap);

    pu->error = GTK_LABEL(gtk_label_new(""));
    gtk_label_set_xalign(pu->error, 0);
    gtk_widget_add_css_class(GTK_WIDGET(pu->error), "error");
    gtk_box_append(GTK_BOX(box), GTK_WIDGET(pu->error));

    GtkWidget *cli_label = gtk_label_new("Who should do it?");
    gtk_label_set_xalign(GTK_LABEL(cli_label), 0);
    gtk_widget_add_css_class(cli_label, "heading");
    gtk_box_append(GTK_BOX(box), cli_label);
    /* Rows were built up-front (and rebuild on probe arrival); the box
     * only needs placing. Preselection happened silently inside
     * picker_replace_clis (first available; catalog order: muse first). */
    gtk_box_append(GTK_BOX(box), pu->cli_box);

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
    g_signal_connect(cancel, "clicked", G_CALLBACK(picker_cancel_clicked),
                     pu);
    gtk_box_append(GTK_BOX(actions), cancel);
    GtkWidget *spawn = gtk_button_new_with_label("Start");
    gtk_widget_add_css_class(spawn, "suggested-action");
    g_signal_connect(spawn, "clicked", G_CALLBACK(picker_spawn_clicked),
                     pu);
    gtk_box_append(GTK_BOX(actions), spawn);
    gtk_box_append(GTK_BOX(box), actions);

    gtk_scrolled_window_set_child(sh->ns_scroll, box);
    gtk_widget_set_visible(GTK_WIDGET(sh->term_scroll), FALSE);
    gtk_widget_set_visible(GTK_WIDGET(sh->ns_scroll), TRUE);
    sh->ns = pu;
    sh->ns_open = TRUE;

    /* Label "Default" with its resolved state for the preselected CLI,
     * rebuild the machine list, then refresh everything the (local)
     * machine owns. */
    picker_refresh_yolo(pu);
    picker_rebuild_machine(pu, NULL);
    picker_after_machine(pu);
    gtk_widget_grab_focus(GTK_WIDGET(pu->folder));
}

static void on_pick_session(GtkButton *btn, gpointer data) {
    (void)btn;
    show_new_session((Shell *)data);
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
                              "(no live session — press New session)\r\n", -1);
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

/* App-level shortcuts: Ctrl+F find, Ctrl+N / Ctrl+Shift+N open the
 * in-place new-session screen, Ctrl+R restart/resume, Ctrl+W close run,
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
            on_pick_session(NULL, sh);
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

    /* Header: exactly one New session button - it opens the in-place
     * screen on the right side on every press (no repeat-last
     * split-button, no caret). */
    sh->spawn_btn = GTK_BUTTON(gtk_button_new_with_label("New session"));
    gtk_widget_add_css_class(GTK_WIDGET(sh->spawn_btn), "suggested-action");
    gtk_widget_set_tooltip_text(GTK_WIDGET(sh->spawn_btn),
                                "Start a new session (Ctrl+N)");
    g_signal_connect(sh->spawn_btn, "clicked",
                     G_CALLBACK(on_pick_session), sh);
    adw_header_bar_pack_start(ADW_HEADER_BAR(bar),
                              GTK_WIDGET(sh->spawn_btn));

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

        /* Terminal side: find revealer on top, VTE in a scrolled window,
     * plus the hidden in-place new-session screen (same column). */
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
    sh->ns_scroll = GTK_SCROLLED_WINDOW(gtk_scrolled_window_new());
    gtk_widget_set_vexpand(GTK_WIDGET(sh->ns_scroll), TRUE);
    gtk_widget_set_hexpand(GTK_WIDGET(sh->ns_scroll), TRUE);
    gtk_widget_set_visible(GTK_WIDGET(sh->ns_scroll), FALSE);
    gtk_box_append(GTK_BOX(term_box), GTK_WIDGET(sh->ns_scroll));
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
                                      "No runs yet — New session spawns one");
        vte_terminal_feed(sh->term, "No runs yet — press New session.\r\n", -1);
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
