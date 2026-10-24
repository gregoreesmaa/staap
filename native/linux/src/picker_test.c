/* Bridge contract tests for the shared core logic (dumb-shell parity).
 *
 * The reconciler, key table, preview, yolo, age, glyph, and picker
 * helpers now live in the core (`staap_feed_delta`, `staap_key_encode`,
 * `staap_spawn_preview`, ...), pinned by `cargo test --lib`. This target
 * pins the bridge's own thin wrappers (malloc/free discipline, NULL
 * conventions) against the real staticlib — no GTK needed.
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
    bridge_core_free(core);

    if (failures == 0) {
        printf("PICKER-OK\n");
    }
    return failures ? 1 : 0;
}
