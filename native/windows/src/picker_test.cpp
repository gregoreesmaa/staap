/* Bridge tests for the shared picker/display helpers (dumb-shell
 * parity). The preview, yolo, age, glyph, and section rules live in the
 * core (`staap_spawn_preview`, ...), pinned by `cargo test --lib`; the
 * catalog/recents JSON shapes still parse locally for native widgets
 * (picker.h), pinned here (plus the SSH-picker rows: `parse_saved_hosts`,
 * `parse_remote_list`, `remote_join`/`remote_parent`, and the hermetic
 * saved-host/default/listing/tester bridge surface — no network).
 * Links the real staticlib via core_bridge.c;
 * exit 0 prints PICKER-OK, 1 on first failure. */

#include "core_bridge.h"
#include "picker.h"

#include <cstdio>
#include <cstdlib>
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

static void saved_host_axis_parses(void) {
    /* Pure picker.h shapes for the machine dropdown + folder browser. */
    CHECK(picker::machine_placeholder_hint().find("user@hostname") !=
              std::string::npos,
          "machine hint shows user@hostname");
    std::string hosts =
        "[{\"name\":\"Build box\",\"host\":\"alice@buildbox\","
        "\"default_folder\":\"/srv/api\"},"
        "{\"name\":\"nas\",\"host\":\"nas\",\"default_folder\":null}]";
    auto rows = picker::parse_saved_hosts(hosts);
    CHECK(rows.size() == 2, "saved hosts count");
    CHECK(rows[0].name == "Build box", "saved host name");
    CHECK(rows[0].host == "alice@buildbox", "saved host order");
    CHECK(rows[0].default_folder == "/srv/api", "saved host folder");
    CHECK(rows[1].default_folder.empty(), "null folder empty");
    CHECK(picker::parse_saved_hosts("[]").empty(), "empty saved hosts");
    CHECK(picker::parse_saved_hosts("nope").empty(), "malformed hosts");
    CHECK(picker::parse_saved_hosts("[{\"name\":\"x\"}]").empty(),
          "hostless row drops");
    std::string list =
        "[{\"name\":\"src\",\"is_dir\":true},"
        "{\"name\":\"main.c\",\"is_dir\":false},"
        "{\"name\":\".\",\"is_dir\":true},"
        "{\"name\":\"..\",\"is_dir\":true}]";
    auto entries = picker::parse_remote_list(list);
    CHECK(entries.size() == 2, "remote list count");
    CHECK(entries[0].name == "src" && entries[0].is_dir, "remote dir");
    CHECK(entries[1].name == "main.c" && !entries[1].is_dir, "remote file");
    CHECK(picker::parse_remote_list("[]").empty(), "empty listing");
    CHECK(picker::parse_remote_list("nope").empty(), "malformed listing");
    CHECK(picker::remote_join("/srv", "api") == "/srv/api", "join slash");
    CHECK(picker::remote_join("/srv/", "api") == "/srv/api", "join trailing");
    CHECK(picker::remote_parent("/srv/api") == "/srv", "parent dir");
    CHECK(picker::remote_parent("/") == "/", "parent root");
    CHECK(picker::remote_parent("api").empty(), "parent relative none");
}

static void ssh_picker_bridge_surface(void) {
    /* Hermetic SSH-picker surface: STAAP_CONFIG points at a scratch
     * name so the round trip never touches the live profile (nothing
     * is saved, so the file is never created); null/blank inputs
     * degrade without network in every path. */
    const char *old_cfg = std::getenv("STAAP_CONFIG");
    std::string saved_old = old_cfg ? old_cfg : "";
    bool had_old = old_cfg != nullptr;
    _putenv("STAAP_CONFIG=staap-win-picker-test-scratch.json");
    StaapCore *core = bridge_core_new();
    CHECK(core != nullptr, "ssh core new");
    char *hosts = bridge_saved_hosts_json(core);
    CHECK(hosts && std::strcmp(hosts, "[]") == 0,
          "saved hosts fresh empty");
    bridge_string_free(hosts);
    char *nullhosts = bridge_saved_hosts_json(nullptr);
    CHECK(nullhosts && std::strcmp(nullhosts, "[]") == 0,
          "saved hosts null ok");
    bridge_string_free(nullhosts);
    {
        char *err = nullptr;
        CHECK(bridge_saved_host_add(core, "", "h", nullptr, &err) != 0,
              "saved add blank name refuses");
        CHECK(err && err[0] != '\0', "saved add blank name names fix");
        free(err);
    }
    {
        char *err = nullptr;
        CHECK(bridge_saved_host_add(core, "n", "  ", nullptr, &err) != 0,
              "saved add blank host refuses");
        CHECK(err && err[0] != '\0', "saved add blank host names fix");
        free(err);
    }
    CHECK(bridge_saved_host_add(core, "", "h", nullptr, nullptr) != 0,
          "saved add null err refuses");
    CHECK(bridge_saved_host_add(core, "Build box", "alice@buildbox",
                                "/srv/api", nullptr) == 0,
          "saved add stores");
    {
        char *h = bridge_saved_hosts_json(core);
        CHECK(h && std::strstr(h, "alice@buildbox") &&
                  std::strstr(h, "Build box") && std::strstr(h, "/srv/api"),
              "saved hosts round trip");
        bridge_string_free(h);
    }
    {
        char *d = bridge_remote_default_folder(core, "alice@buildbox");
        CHECK(d && std::strcmp(d, "/srv/api") == 0,
              "remote default saved host");
        bridge_string_free(d);
    }
    {
        /* Rename in place (keyed by display name): the typo fix moves
         * the target, keeps the per-host default, and carries the
         * folder memory to the new target. */
        char *err = nullptr;
        CHECK(bridge_saved_host_rename(core, "Build box", "Prod box",
                                       "alice@prodbox", "/srv/v2",
                                       &err) == 0,
              "saved rename stores");
        CHECK(err == nullptr, "saved rename sets no error");
        char *h = bridge_saved_hosts_json(core);
        CHECK(h && std::strstr(h, "alice@prodbox") &&
                  std::strstr(h, "Prod box") &&
                  std::strstr(h, "/srv/v2") &&
                  !std::strstr(h, "alice@buildbox"),
              "saved rename round trip");
        bridge_string_free(h);
        char *d = bridge_remote_default_folder(core, "alice@prodbox");
        CHECK(d && std::strcmp(d, "/srv/v2") == 0,
              "remote default follows rename");
        bridge_string_free(d);
    }
    {
        char *err = nullptr;
        CHECK(bridge_saved_host_rename(core, "no-such-host-xyz", "n", "h",
                                       nullptr, &err) != 0,
              "saved rename unknown refuses");
        CHECK(err && err[0] != '\0', "saved rename unknown names fix");
        free(err);
    }
    CHECK(bridge_saved_host_rename(nullptr, "k", "n", "h", nullptr,
                                   nullptr) != 0,
          "saved rename null core refuses");
    CHECK(bridge_remote_default_folder(core, "no-such-host-xyz") == nullptr,
          "remote default unknown null");
    CHECK(bridge_remote_default_folder(core, "") == nullptr,
          "remote default blank null");
    CHECK(bridge_remote_default_folder(nullptr, "h") == nullptr,
          "remote default null core null");
    {
        char *err = nullptr;
        CHECK(bridge_saved_host_remove(core, "no-such-host-xyz", &err) != 0,
              "saved remove unknown refuses");
        CHECK(err && err[0] != '\0', "saved remove unknown names fix");
        free(err);
    }
    {
        char *err = nullptr;
        CHECK(bridge_saved_host_remove(nullptr, "x", &err) != 0,
              "saved remove null core refuses");
        free(err);
    }
    CHECK(bridge_saved_host_remove(core, "alice@prodbox", nullptr) == 0,
          "saved remove by renamed host");
    {
        char *h = bridge_saved_hosts_json(core);
        CHECK(h && std::strcmp(h, "[]") == 0,
              "saved hosts empty after remove");
        bridge_string_free(h);
    }
    {
        /* Last-host memory lives in the core: null when nothing launched,
         * forget of other hosts/blanks is a no-op success, nulls refuse.
         * (The pointed-at clear is covered by the core suite.) */
        char *none = bridge_last_host(core);
        CHECK(none == nullptr, "last host fresh null");
        char *err = nullptr;
        CHECK(bridge_forget_last_host(core, "alice@prodbox", &err) == 0,
              "last forget unknown ok");
        CHECK(err == nullptr, "last forget unknown sets no error");
        CHECK(bridge_forget_last_host(core, "   ", nullptr) == 0,
              "last forget blank ok");
        CHECK(bridge_forget_last_host(nullptr, "h", &err) != 0,
              "last forget null core refuses");
        free(err);
        CHECK(bridge_forget_last_host(core, nullptr, nullptr) != 0,
              "last forget null host refuses");
        CHECK(bridge_last_host(nullptr) == nullptr, "last host null ok");
    }
    {
        char *l = bridge_remote_list_json(nullptr, "/x");
        CHECK(l && std::strcmp(l, "[]") == 0,
              "remote list null host empty");
        bridge_string_free(l);
    }
    {
        char *l = bridge_remote_list_json("", "/x");
        CHECK(l && std::strcmp(l, "[]") == 0,
              "remote list blank host empty");
        bridge_string_free(l);
    }
    {
        char *l = bridge_remote_list_json("h", "");
        CHECK(l && std::strcmp(l, "[]") == 0,
              "remote list blank path empty");
        bridge_string_free(l);
    }
    {
        char *t = bridge_remote_test_json(nullptr);
        CHECK(t && std::strstr(t, "\"reachable\":false") != nullptr,
              "remote test null unreachable");
        CHECK(t && std::strstr(t, "\"reason\":\"empty-host\"") != nullptr,
              "remote test null names empty-host");
        bridge_string_free(t);
    }
    {
        char *t = bridge_remote_test_json("");
        CHECK(t && std::strstr(t, "\"reachable\":false") != nullptr,
              "remote test blank unreachable");
        CHECK(t && std::strstr(t, "\"reason\":\"empty-host\"") != nullptr,
              "remote test blank names empty-host");
        CHECK(t && std::strstr(t, "\"message\":\"no host entered") != nullptr,
              "remote test blank carries render copy");
        bridge_string_free(t);
    }
    bridge_core_free(core);
    if (had_old) {
        _putenv(("STAAP_CONFIG=" + saved_old).c_str());
    } else {
        _putenv("STAAP_CONFIG=");
    }
}

static void tester_message_parses(void) {
    /* Pure tester-message extraction (picker.h): the core copy renders
     * verbatim; absent/null/unterminated yield empty for the generic
     * fallback. */
    CHECK(picker::test_message(
              "{\"reachable\":false,\"reason\":\"auth-failure\","
              "\"message\":\"auth failed - check keys\",\"clis\":[]}") ==
              "auth failed - check keys",
          "tester message extracts verbatim");
    CHECK(picker::test_message(
              "{\"reachable\":true,\"reason\":null,\"message\":null,"
              "\"clis\":[]}")
              .empty(),
          "reachable null message empty");
    CHECK(picker::test_message("{}").empty(), "missing message empty");
    CHECK(picker::test_message("{\"message\":\"a\\\"b\\\\c\\nd\"}") ==
              "a\"b\\c\nd",
          "tester escapes resolve");
    CHECK(picker::test_message("{\"message\":\"abc}").empty(),
          "unterminated message empty");
}

int main(void) {
    catalog_parses_in_order();
    recents_parse();
    host_axis_helpers();
    remote_bridge_surface();
    saved_host_axis_parses();
    ssh_picker_bridge_surface();
    tester_message_parses();
    shared_helpers_come_from_core();
    if (failures == 0) {
        std::printf("PICKER-OK\n");
    }
    return failures ? 1 : 0;
}
