/* Bridge tests for the shared picker/display helpers (dumb-shell
 * parity). The preview, yolo, age, glyph, and section rules live in the
 * core (`staap_spawn_preview`, ...), pinned by `cargo test --lib`; the
 * catalog/recents JSON shapes still parse locally for native widgets
 * (picker.h), pinned here. Links the real staticlib via core_bridge.c;
 * exit 0 prints PICKER-OK, 1 on first failure. */

#include "core_bridge.h"
#include "picker.h"

#include <cstdio>
#include <cstring>
#include <string>

static int failures = 0;

#define CHECK(cond, what)                              \
    do {                                               \
        if (!(cond)) {                                 \
            std::printf("PICKER-FAIL %s\n", what);     \
            ++failures;                                \
        }                                              \
    } while (0)

static void catalog_parses_in_order(void) {
    std::string catalog =
        "[{\"id\":\"muse\",\"program\":\"muse\",\"path\":\"C:\\\\muse.exe\","
        "\"available\":true},"
        "{\"id\":\"claude\",\"program\":\"claude\",\"path\":null,"
        "\"available\":false}]";
    auto clis = picker::parse_clis(catalog);
    CHECK(clis.size() == 2, "catalog count");
    CHECK(clis[0].id == "muse", "catalog order");
    CHECK(clis[0].available, "muse available");
    CHECK(clis[0].path == "C:\\muse.exe", "muse path");
    CHECK(clis[1].id == "claude", "claude second");
    CHECK(!clis[1].available, "claude missing");
    CHECK(clis[1].path.empty(), "missing path empty");
    CHECK(picker::parse_clis("[]").empty(), "empty catalog");
    CHECK(picker::parse_clis("nope").empty(), "malformed catalog");
}

static void recents_parse(void) {
    auto recents = picker::parse_recents("[\"C:\\\\a\",\"D:\\\\b\"]");
    CHECK(recents.size() == 2, "recents count");
    CHECK(recents[0] == "C:\\a", "recents order");
    CHECK(picker::parse_recents("[]").empty(), "empty recents");
}

static void shared_helpers_come_from_core(void) {
    /* Preview / yolo / age / glyph / section through the bridge. */
    char *prev = bridge_spawn_preview("muse", "C:\\a", 1);
    CHECK(prev && strcmp(prev, "runs: muse in C:\\a + yolo") == 0,
          "preview full");
    free(prev);
    char *bare = bridge_spawn_preview("claude", "", 0);
    CHECK(bare && strcmp(bare, "runs: claude") == 0, "preview bare");
    free(bare);
    char *off = bridge_spawn_preview("muse", "", -1);
    CHECK(off && strcmp(off, "runs: muse (yolo off)") == 0, "preview off");
    free(off);
    CHECK(bridge_yolo_value(0) == 0, "yolo default");
    CHECK(bridge_yolo_value(1) == 1, "yolo on");
    CHECK(bridge_yolo_value(2) == -1, "yolo off");
    CHECK(bridge_yolo_default(nullptr, "muse") == 0, "yolo default null");
    CHECK(bridge_yolo_default(nullptr, nullptr) == 0, "yolo default nulls");
    CHECK(bridge_run_cursor(nullptr, "x", nullptr, nullptr) != 0, "cursor nulls");
    char *age = bridge_age_string(600, 0);
    CHECK(age && strcmp(age, "10m ago") == 0, "age buckets");
    bridge_string_free(age);
    char *hdr = bridge_section_title(2);
    CHECK(hdr && strcmp(hdr, "Working") == 0, "section working");
    bridge_string_free(hdr);
    /* Registry surface on a fresh core (no PTY needed). */
    StaapCore *core = bridge_core_new();
    CHECK(core != nullptr, "core new");
    CHECK(bridge_max_runs() == 10, "cap is 10");
    CHECK(bridge_live_count(core) == 0, "no live runs");
    /* History contract (core-owned): unknown ids are historic;
     * expansion is collapsed by default and round-trips. */
    CHECK(bridge_is_history(core, "missing") != 0, "unknown is history");
    CHECK(bridge_is_history(nullptr, "missing") == 0, "null core no history");
    CHECK(bridge_history_expanded(core) == 0, "history starts collapsed");
    CHECK(bridge_history_expanded(nullptr) == 0, "null core not expanded");
    bridge_set_history_expanded(nullptr, 1);
    bridge_set_history_expanded(core, 1);
    CHECK(bridge_history_expanded(core) != 0, "history set expanded");
    bridge_set_history_expanded(core, 0);
    CHECK(bridge_history_expanded(core) == 0, "history set collapsed");
    bridge_core_free(core);
}

static void host_axis_helpers(void) {
    CHECK(!picker::is_remote(""), "blank not remote");
    CHECK(!picker::is_remote("   "), "whitespace not remote");
    CHECK(picker::trim_host("  nas\t") == "nas", "host trims");
    CHECK(picker::is_remote("nas"), "host is remote");
    CHECK(picker::with_ssh_prefix("", "runs: muse") == "runs: muse",
          "local preview unchanged");
    CHECK(picker::with_ssh_prefix("nas", "runs: muse") ==
              "ssh nas: runs: muse",
          "remote preview prefixed");
}

static void remote_bridge_surface(void) {
    /* Hermetic: null/blank hosts degrade, unknown hosts yield no
     * recents, and a blank-host spawn refuses before any spawn — no
     * network in any path. */
    StaapCore *core = bridge_core_new();
    CHECK(core != nullptr, "remote core new");
    char *rnull = bridge_clis_remote(nullptr);
    CHECK(rnull && std::strstr(rnull, "\"available\":false") != nullptr,
          "remote clis null degrades");
    if (rnull) {
        bridge_string_free(rnull);
    }
    char *rblank = bridge_clis_remote("");
    CHECK(rblank && rblank[0] == '[' &&
              std::strstr(rblank, "\"available\":false") != nullptr,
          "remote clis blank degrades");
    if (rblank) {
        bridge_string_free(rblank);
    }
    char *rrec = bridge_remote_recent_json(core, "no-such-host-xyz");
    CHECK(rrec && std::strcmp(rrec, "[]") == 0,
          "remote recents unknown empty");
    if (rrec) {
        bridge_string_free(rrec);
    }
    char *rrec0 = bridge_remote_recent_json(nullptr, "h");
    CHECK(rrec0 && std::strcmp(rrec0, "[]") == 0,
          "remote recents null core empty");
    if (rrec0) {
        bridge_string_free(rrec0);
    }
    {
        char id[128] = {0};
        char *err = nullptr;
        int rc = bridge_run_spawn_remote(core, "", nullptr, nullptr, 0, 80,
                                         24, id, sizeof id, &err);
        CHECK(rc != 0, "remote spawn blank refuses");
        CHECK(err && err[0] != '\0', "remote spawn blank names fix");
        free(err);
    }
    {
        char id[128] = {0};
        char *err = nullptr;
        int rc = bridge_run_spawn_remote(core, nullptr, nullptr, nullptr, 0,
                                         80, 24, id, sizeof id, &err);
        CHECK(rc != 0, "remote spawn null host refuses");
        CHECK(err && err[0] != '\0', "remote spawn null host names fix");
        free(err);
    }
    bridge_core_free(core);
}

int main(void) {
    catalog_parses_in_order();
    recents_parse();
    host_axis_helpers();
    remote_bridge_surface();
    shared_helpers_come_from_core();
    if (failures == 0) {
        std::printf("PICKER-OK\n");
    }
    return failures ? 1 : 0;
}
