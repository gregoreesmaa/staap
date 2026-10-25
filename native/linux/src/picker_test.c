/* Bridge contract tests for the shared core logic (dumb-shell parity).
 *
 * The reconciler, key table, preview, yolo, age, glyph, and picker
 * helpers now live in the core (`staap_feed_delta`, `staap_key_encode`,
 * `staap_spawn_preview`, ...), pinned by `cargo test --lib`. This target
 * pins the bridge's own thin wrappers (malloc/free discipline, NULL
 * conventions) against the real staticlib — no GTK needed. Remote
 * coverage (`bridge_clis_remote`, `bridge_remote_recent_json`,
 * `bridge_run_spawn_remote`, plus the SSH-picker surface
 * `bridge_saved_hosts_json` / `bridge_saved_host_add` /
 * `bridge_saved_host_remove` / `bridge_remote_default_folder` /
 * `bridge_remote_list_json` / `bridge_remote_test_json`) is hermetic:
 * null/blank/unknown hosts never touch the network.
 *
 * Exit 0 on success (`PICKER-OK`), 1 on first failure. */

#include "core_bridge.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int failures = 0;

#define CHECK(cond, what)                           \
    do {                                            \
        if (!(cond)) {                              \
            fprintf(stderr, "PICKER-FAIL %s\n", what); \
            failures++;                             \
        }                                           \
    } while (0)

int main(void) {
    /* Feed delta through the shared reconciler. */
    char *feed = bridge_feed_delta("a", "a\nb\n");
    CHECK(feed && strcmp(feed, "\r\nb\r\n") == 0, "feed append");
    free(feed);
    CHECK(bridge_feed_delta("same", "same") == NULL, "feed identical");
    CHECK(bridge_feed_delta(NULL, NULL) == NULL, "feed nulls");

    /* Key table through the shared encoder. */
    unsigned char buf[16];
    CHECK(bridge_key_encode("enter", NULL, 0, 0, buf, sizeof buf) == 1 &&
              buf[0] == '\r',
          "key enter");
    CHECK(bridge_key_encode("up", NULL, 0, 0, buf, sizeof buf) == 3,
          "key up");
    CHECK(bridge_key_encode("c", NULL, 1, 0, buf, sizeof buf) == 1 &&
              buf[0] == 0x03,
          "key ctrl-c");
    CHECK(bridge_key_encode("shift", NULL, 0, 0, buf, sizeof buf) == 0,
          "key keep");
    CHECK(bridge_key_encode(NULL, NULL, 0, 0, buf, sizeof buf) == -1,
          "key null");

    /* Preview / yolo / age / glyph / section through the core. */
    char *prev = bridge_spawn_preview("muse", "/tmp/api", 1);
    CHECK(prev && strcmp(prev, "runs: muse in /tmp/api + yolo") == 0,
          "preview full");
    free(prev);
    char *bare = bridge_spawn_preview("claude", "", 0);
    CHECK(bare && strcmp(bare, "runs: claude") == 0, "preview bare");
    free(bare);
    CHECK(bridge_yolo_value(0) == 0, "yolo default");
    CHECK(bridge_yolo_value(1) == 1, "yolo on");
    CHECK(bridge_yolo_value(2) == -1, "yolo off");
    CHECK(bridge_yolo_default(NULL, "muse") == 0, "yolo default null");
    CHECK(bridge_yolo_default(NULL, NULL) == 0, "yolo default nulls");
    CHECK(bridge_run_cursor(NULL, "x", NULL, NULL) != 0, "cursor nulls");
    char *age = bridge_age_string(600, 0);
    CHECK(age && strcmp(age, "10m ago") == 0, "age buckets");
    bridge_string_free(age);
    char *glyph = bridge_status_glyph(0);
    CHECK(glyph && strcmp(glyph, "\342\227\217") == 0, "glyph attention");
    bridge_string_free(glyph);
    char *hdr = bridge_section_title(2);
    CHECK(hdr && strcmp(hdr, "Working") == 0, "section working");
    bridge_string_free(hdr);
    CHECK(bridge_clamp_sidebar(99.0) == 220.0, "clamp floor");
    CHECK(bridge_clamp_sidebar(900.0) == 480.0, "clamp ceiling");

    /* Registry surface on a fresh core (no PTY needed). */
    StaapCore *core = bridge_core_new();
    CHECK(core != NULL, "core new");
    CHECK(bridge_max_runs() == 10, "cap is 10");
    CHECK(bridge_live_count(core) == 0, "no live runs");
    CHECK(!bridge_is_live(core, "missing"), "unknown not live");
    CHECK(!bridge_needs_quit_confirm(core), "fresh quit needs no confirm");

    /* Remote bridge surface: hermetic, no network. Null/blank hosts
     * degrade to the all-listed-none-available catalog; unknown hosts
     * yield no recents; a blank-host spawn refuses before any spawn. */
    char *rnull = bridge_clis_remote(NULL);
    CHECK(rnull && rnull[0] == '[' &&
              strstr(rnull, "\"available\":false"),
          "remote clis null degrades");
    if (rnull) {
        bridge_string_free(rnull);
    }
    char *rblank = bridge_clis_remote("");
    CHECK(rblank && rblank[0] == '[' &&
              strstr(rblank, "\"available\":false"),
          "remote clis blank degrades");
    if (rblank) {
        bridge_string_free(rblank);
    }
    char *rrec = bridge_remote_recent_json(core, "no-such-host-xyz");
    CHECK(rrec && strcmp(rrec, "[]") == 0, "remote recents unknown empty");
    if (rrec) {
        bridge_string_free(rrec);
    }
    char *rrec0 = bridge_remote_recent_json(NULL, "h");
    CHECK(rrec0 && strcmp(rrec0, "[]") == 0,
          "remote recents null core empty");
    if (rrec0) {
        bridge_string_free(rrec0);
    }
    {
        char id[128] = { 0 };
        char *err = NULL;
        int rc = bridge_run_spawn_remote(core, "", NULL, NULL, 0, 80, 24,
                                         id, sizeof id, &err);
        CHECK(rc != 0, "remote spawn blank refuses");
        CHECK(err && err[0], "remote spawn blank names fix");
        free(err);
    }
    {
        char id[128] = { 0 };
        char *err = NULL;
        int rc = bridge_run_spawn_remote(core, NULL, NULL, NULL, 0, 80,
                                         24, id, sizeof id, &err);
        CHECK(rc != 0, "remote spawn null host refuses");
        CHECK(err && err[0], "remote spawn null host names fix");
        free(err);
    }

    /* Saved hosts + remote default/listing/tester (SSH picker):
     * hermetic — null/blank inputs degrade without network. */
    char *hosts = bridge_saved_hosts_json(core);
    CHECK(hosts && strcmp(hosts, "[]") == 0, "saved hosts fresh empty");
    bridge_string_free(hosts);
    CHECK(bridge_saved_hosts_json(NULL) != NULL, "saved hosts null ok");
    bridge_string_free(bridge_saved_hosts_json(NULL));
    {
        char *err = NULL;
        CHECK(bridge_saved_host_add(core, "", "h", NULL, &err) != 0,
              "saved add blank name refuses");
        CHECK(err && err[0], "saved add blank name names fix");
        free(err);
    }
    {
        char *err = NULL;
        CHECK(bridge_saved_host_add(core, "n", "  ", NULL, &err) != 0,
              "saved add blank host refuses");
        CHECK(err && err[0], "saved add blank host names fix");
        free(err);
    }
    CHECK(bridge_saved_host_add(core, "Build box", "alice@buildbox",
                                "/srv/api", NULL) == 0,
          "saved add stores");
    {
        char *h = bridge_saved_hosts_json(core);
        CHECK(h && strstr(h, "alice@buildbox") && strstr(h, "Build box") &&
                  strstr(h, "/srv/api"),
              "saved hosts round trip");
        bridge_string_free(h);
    }
    {
        char *d = bridge_remote_default_folder(core, "alice@buildbox");
        CHECK(d && strcmp(d, "/srv/api") == 0, "remote default saved host");
        bridge_string_free(d);
    }
    CHECK(bridge_remote_default_folder(core, "no-such-host-xyz") == NULL,
          "remote default unknown null");
    CHECK(bridge_remote_default_folder(core, "") == NULL,
          "remote default blank null");
    CHECK(bridge_remote_default_folder(NULL, "h") == NULL,
          "remote default null core null");
    {
        char *err = NULL;
        CHECK(bridge_saved_host_remove(core, "no-such-host-xyz", &err) != 0,
              "saved remove unknown refuses");
        CHECK(err && err[0], "saved remove unknown names fix");
        free(err);
    }
    CHECK(bridge_saved_host_remove(core, "alice@buildbox", NULL) == 0,
          "saved remove by host");
    {
        char *h = bridge_saved_hosts_json(core);
        CHECK(h && strcmp(h, "[]") == 0, "saved hosts empty after remove");
        bridge_string_free(h);
    }
    {
        char *l = bridge_remote_list_json(NULL, "/x");
        CHECK(l && strcmp(l, "[]") == 0, "remote list null host empty");
        bridge_string_free(l);
    }
    {
        char *l = bridge_remote_list_json("", "/x");
        CHECK(l && strcmp(l, "[]") == 0, "remote list blank host empty");
        bridge_string_free(l);
    }
    {
        char *l = bridge_remote_list_json("h", "");
        CHECK(l && strcmp(l, "[]") == 0, "remote list blank path empty");
        bridge_string_free(l);
    }
    {
        char *t = bridge_remote_test_json(NULL);
        CHECK(t && strstr(t, "\"reachable\":false"),
              "remote test null unreachable");
        CHECK(t && strstr(t, "\"reason\":\"empty-host\""),
              "remote test null names empty-host");
        bridge_string_free(t);
    }
    {
        char *t = bridge_remote_test_json("");
        CHECK(t && strstr(t, "\"reachable\":false"),
              "remote test blank unreachable");
        CHECK(t && strstr(t, "\"reason\":\"empty-host\""),
              "remote test blank names empty-host");
        CHECK(t && strstr(t, "\"message\":\"no host entered"),
              "remote test blank carries render copy");
        bridge_string_free(t);
    }

    bridge_core_free(core);

    if (failures == 0) {
        printf("PICKER-OK\n");
    }
    return failures ? 1 : 0;
}
