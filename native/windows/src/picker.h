/* Two-dimensional new-session picker rows for the WinUI shell.
 *
 * The catalog/recents/tester-message JSON shapes parse locally (native ComboBox
 * rows need them); every rule — preview copy, yolo mapping, folder
 * trimming — lives once in the core (`staap_spawn_preview`,
 * `staap_yolo_value`, ...), pinned by `cargo test --lib` and the bridge
 * section of `picker_test.cpp`. Pure header-only C++ (no WinUI types),
 * so the CMake `staap-win-picker-test` pins it on every runner.
 *
 * Catalog rows come from `bridge_clis_json`
 * ([{id,program,path,available}] in core order); recents from
 * `bridge_recent_json` ([String], MRU-first). Missing CLIs stay listed
 * (available=false) with an install hint, never hidden. The SSH host
 * axis trims/prefixes locally too (`trim_host`, `is_remote`,
 * `with_ssh_prefix`); remote catalogs/recents/spawns stay on the
 * bridge (`bridge_clis_remote`, `bridge_remote_recent_json`,
 * `bridge_run_spawn_remote`); saved hosts and remote listings parse
 * locally too (`parse_saved_hosts`, `parse_remote_list`) for the
 * machine dropdown and the folder browser).
 */

#ifndef STAAP_WIN_PICKER_H
#define STAAP_WIN_PICKER_H

#include <string>
#include <vector>

namespace picker {

/* One CLI catalog row. `path` is empty when the CLI is missing. */
struct CliRow {
    std::string id;
    std::string program;
    std::string path; /* empty when unavailable */
    bool available = false;
};

/* Parse the `bridge_clis_json` catalog (core order). Malformed input
 * yields zero rows, never a throw. */
inline std::vector<CliRow> parse_clis(const std::string &json) {
    std::vector<CliRow> out;
    std::size_t p = json.find('[');
    if (p == std::string::npos) {
        return out;
    }
    ++p;
    while (p < json.size() && json[p] != ']') {
        while (p < json.size() &&
               (json[p] == ' ' || json[p] == '\t' || json[p] == '\n' ||
                json[p] == '\r' || json[p] == ',')) {
            ++p;
        }
        if (p >= json.size() || json[p] != '{') {
            break;
        }
        std::size_t start = p;
        int depth = 0;
        bool in_str = false, esc = false;
        for (; p < json.size(); ++p) {
            char c = json[p];
            if (in_str) {
                if (esc) {
                    esc = false;
                } else if (c == '\\') {
                    esc = true;
                } else if (c == '"') {
                    in_str = false;
                }
            } else if (c == '"') {
                in_str = true;
            } else if (c == '{') {
                ++depth;
            } else if (c == '}') {
                if (--depth == 0) {
                    ++p;
                    break;
                }
            }
        }
        if (depth != 0) {
            break;
        }
        std::string obj = json.substr(start, p - start);
        auto raw = [&](const char *key) -> std::string {
            std::string pat = std::string("\"") + key + "\"";
            std::size_t q = obj.find(pat);
            if (q == std::string::npos) {
                return {};
            }
            q = obj.find(':', q + pat.size());
            if (q == std::string::npos) {
                return {};
            }
            ++q;
            while (q < obj.size() &&
                   (obj[q] == ' ' || obj[q] == '\t')) {
                ++q;
            }
            if (q >= obj.size()) {
                return {};
            }
            if (obj[q] != '"') {
                /* Non-string scalar (null/false/true): return the token
                 * so callers can compare it literally. */
                std::size_t e = q;
                while (e < obj.size() && obj[e] != ',' && obj[e] != '}' &&
                       obj[e] != ' ' && obj[e] != '\t') {
                    ++e;
                }
                return obj.substr(q, e - q);
            }
            std::string val;
            ++q;
            bool ok = true;
            while (q < obj.size() && obj[q] != '"') {
                if (obj[q] != '\\') {
                    val += obj[q++];
                    continue;
                }
                ++q;
                if (q >= obj.size()) {
                    ok = false;
                    break;
                }
                char e = obj[q++];
                switch (e) {
                case '"': val += '"'; break;
                case '\\': val += '\\'; break;
                case '/': val += '/'; break;
                case 'n': val += '\n'; break;
                case 't': val += '\t'; break;
                case 'r': val += '\r'; break;
                default: val += e; break;
                }
            }
            if (!ok) {
                return {};
            }
            return val;
        };
        CliRow row;
        row.id = raw("id");
        row.program = raw("program");
        std::string path = raw("path");
        row.path = (path == "null") ? std::string{} : path;
        row.available = raw("available") == "true";
        if (row.id.empty()) {
            row.id = "muse";
        }
        if (row.program.empty()) {
            row.program = row.id;
        }
        out.push_back(std::move(row));
    }
    return out;
}

/* Extract the core tester message from the bridge_remote_test_json payload
 * ({"reachable":bool,"reason":...,"message":...,"clis":[...]}). Returns the
 * render-ready core copy shells show verbatim, or empty when absent/null
 * (callers fall back to their generic copy). Malformed input yields empty,
 * never a throw. */
inline std::string test_message(const std::string &json) {
    std::size_t q = json.find("\"message\"");
    if (q == std::string::npos) {
        return {};
    }
    q = json.find(':', q + 9);
    if (q == std::string::npos) {
        return {};
    }
    ++q;
    while (q < json.size() && (json[q] == ' ' || json[q] == '\t')) {
        ++q;
    }
    if (q >= json.size() || json[q] != '"') {
        return {};
    }
    ++q;
    std::string val;
    while (q < json.size() && json[q] != '"') {
        if (json[q] != '\\') {
            val += json[q++];
            continue;
        }
        ++q;
        if (q >= json.size()) {
            return {};
        }
        char e = json[q++];
        switch (e) {
        case 'n': val += '\n'; break;
        case 't': val += '\t'; break;
        case 'r': val += '\r'; break;
        default: val += e; break;
        }
    }
    if (q >= json.size()) {
        return {};
    }
    return val;
}

/* Parse the `bridge_recent_json` string array. Malformed input yields
 * zero recents, never a throw. */
inline std::vector<std::string> parse_recents(const std::string &json) {
    std::vector<std::string> out;
    std::size_t p = json.find('[');
    if (p == std::string::npos) {
        return out;
    }
    ++p;
    while (p < json.size() && json[p] != ']') {
        while (p < json.size() &&
               (json[p] == ' ' || json[p] == '\t' || json[p] == '\n' ||
                json[p] == '\r' || json[p] == ',')) {
            ++p;
        }
        if (p >= json.size() || json[p] != '"') {
            break;
        }
        ++p;
        std::string val;
        bool ok = true;
        while (p < json.size() && json[p] != '"') {
            if (json[p] != '\\') {
                val += json[p++];
                continue;
            }
            ++p;
            if (p >= json.size()) {
                ok = false;
                break;
            }
            char e = json[p++];
            switch (e) {
            case '"': val += '"'; break;
            case '\\': val += '\\'; break;
            case '/': val += '/'; break;
            case 'n': val += '\n'; break;
            case 't': val += '\t'; break;
            case 'r': val += '\r'; break;
            default: val += e; break;
            }
        }
        if (!ok || p >= json.size()) {
            break;
        }
        ++p; /* closing quote */
        out.push_back(std::move(val));
    }
    return out;
}

/* SSH host axis (remote sessions): blank/whitespace means this machine
 * (local launch). Trimmed for routing, so "  " stays local and
 * " host " dials "host". */
inline std::string trim_host(const std::string &raw) {
    const char *ws = " \t\n\r";
    std::string::size_type b = raw.find_first_not_of(ws);
    if (b == std::string::npos) {
        return {};
    }
    std::string::size_type e = raw.find_last_not_of(ws);
    return raw.substr(b, e - b + 1);
}

/* True when the trimmed host names an SSH host (remote launch). */
inline bool is_remote(const std::string &host) {
    return !trim_host(host).empty();
}

/* Prefix a core preview line for a remote launch ("ssh <host>: ...");
 * local launches return the preview unchanged. */
inline std::string with_ssh_prefix(const std::string &host,
                                   const std::string &preview) {
    std::string h = trim_host(host);
    if (h.empty()) {
        return preview;
    }
    return "ssh " + h + ": " + preview;
}

/* Machine-axis hint for the host dropdown/add dialog: always shows a
 * user@hostname example so the expected shape is visible at a glance. */
inline std::string machine_placeholder_hint() {
    return "user@hostname, e.g. alice@buildbox (or ssh-config alias)";
}

/* One saved SSH host row (parsed from `bridge_saved_hosts_json`:
 * [{"name","host","default_folder"|null}] in saved order). Malformed
 * input yields zero rows, never a throw. */
struct SavedHostRow {
    std::string name;
    std::string host;
    std::string default_folder; /* empty when none */
};

inline std::vector<SavedHostRow> parse_saved_hosts(const std::string &json) {
    std::vector<SavedHostRow> out;
    std::size_t p = json.find('[');
    if (p == std::string::npos) {
        return out;
    }
    ++p;
    while (p < json.size() && json[p] != ']') {
        while (p < json.size() &&
               (json[p] == ' ' || json[p] == '\t' || json[p] == '\n' ||
                json[p] == '\r' || json[p] == ',')) {
            ++p;
        }
        if (p >= json.size() || json[p] != '{') {
            break;
        }
        std::size_t start = p;
        int depth = 0;
        bool in_str = false, esc = false;
        for (; p < json.size(); ++p) {
            char c = json[p];
            if (in_str) {
                if (esc) {
                    esc = false;
                } else if (c == '\\') {
                    esc = true;
                } else if (c == '"') {
                    in_str = false;
                }
            } else if (c == '"') {
                in_str = true;
            } else if (c == '{') {
                ++depth;
            } else if (c == '}') {
                if (--depth == 0) {
                    ++p;
                    break;
                }
            }
        }
        if (depth != 0) {
            break;
        }
        std::string obj = json.substr(start, p - start);
        auto raw = [&](const char *key) -> std::string {
            std::string pat = std::string("\"") + key + "\"";
            std::size_t q = obj.find(pat);
            if (q == std::string::npos) {
                return {};
            }
            q = obj.find(':', q + pat.size());
            if (q == std::string::npos) {
                return {};
            }
            ++q;
            while (q < obj.size() &&
                   (obj[q] == ' ' || obj[q] == '\t')) {
                ++q;
            }
            if (q >= obj.size() || obj[q] != '"') {
                return {}; /* null or non-string: no value */
            }
            std::string val;
            ++q;
            while (q < obj.size() && obj[q] != '"') {
                if (obj[q] != '\\') {
                    val += obj[q++];
                    continue;
                }
                ++q;
                if (q >= obj.size()) {
                    return {};
                }
                char e = obj[q++];
                switch (e) {
                case '"': val += '"'; break;
                case '\\': val += '\\'; break;
                case '/': val += '/'; break;
                case 'n': val += '\n'; break;
                case 't': val += '\t'; break;
                case 'r': val += '\r'; break;
                default: val += e; break;
                }
            }
            return val;
        };
        SavedHostRow row;
        row.name = raw("name");
        row.host = raw("host");
        row.default_folder = raw("default_folder");
        if (!row.name.empty() && !row.host.empty()) {
            out.push_back(std::move(row));
        }
    }
    return out;
}

/* One remote directory listing entry (parsed from
 * `bridge_remote_list_json`: [{"name","is_dir"}]). Malformed input
 * yields zero rows, never a throw. */
struct RemoteEntryRow {
    std::string name;
    bool is_dir = false;
};

inline std::vector<RemoteEntryRow> parse_remote_list(const std::string &json) {
    std::vector<RemoteEntryRow> out;
    std::size_t p = json.find('[');
    if (p == std::string::npos) {
        return out;
    }
    ++p;
    while (p < json.size() && json[p] != ']') {
        while (p < json.size() &&
               (json[p] == ' ' || json[p] == '\t' || json[p] == '\n' ||
                json[p] == '\r' || json[p] == ',')) {
            ++p;
        }
        if (p >= json.size() || json[p] != '{') {
            break;
        }
        std::size_t start = p;
        int depth = 0;
        bool in_str = false, esc = false;
        for (; p < json.size(); ++p) {
            char c = json[p];
            if (in_str) {
                if (esc) {
                    esc = false;
                } else if (c == '\\') {
                    esc = true;
                } else if (c == '"') {
                    in_str = false;
                }
            } else if (c == '"') {
                in_str = true;
            } else if (c == '{') {
                ++depth;
            } else if (c == '}') {
                if (--depth == 0) {
                    ++p;
                    break;
                }
            }
        }
        if (depth != 0) {
            break;
        }
        std::string obj = json.substr(start, p - start);
        auto raw = [&](const char *key) -> std::string {
            std::string pat = std::string("\"") + key + "\"";
            std::size_t q = obj.find(pat);
            if (q == std::string::npos) {
                return {};
            }
            q = obj.find(':', q + pat.size());
            if (q == std::string::npos) {
                return {};
            }
            ++q;
            while (q < obj.size() &&
                   (obj[q] == ' ' || obj[q] == '\t')) {
                ++q;
            }
            if (q >= obj.size() || obj[q] != '"') {
                return {};
            }
            std::string val;
            ++q;
            while (q < obj.size() && obj[q] != '"') {
                if (obj[q] != '\\') {
                    val += obj[q++];
                    continue;
                }
                ++q;
                if (q >= obj.size()) {
                    return {};
                }
                val += obj[q++];
            }
            return val;
        };
        std::string name = raw("name");
        if (name.empty() || name == "." || name == "..") {
            continue;
        }
        RemoteEntryRow row;
        row.name = std::move(name);
        row.is_dir = obj.find("\"is_dir\":true") != std::string::npos ||
                     obj.find("\"is_dir\": true") != std::string::npos;
        out.push_back(std::move(row));
    }
    return out;
}

/* A drive row (`C:` from a root listing on a Windows host) maps back
 * to its own root (`C:/`); anything else is not a drive. Letters are
 * matched ASCII-only (`Get-PSDrive` names); no locale dependence. */
inline bool is_drive_row(const std::string &name) {
    std::string s = name;
    while (s.size() > 2 && s.back() == '/') {
        s.pop_back();
    }
    return s.size() == 2 && s[1] == ':' &&
           ((s[0] >= 'A' && s[0] <= 'Z') || (s[0] >= 'a' && s[0] <= 'z'));
}

/* Join a remote base dir and an entry name with exactly one '/'. A
 * drive row joined at the root becomes its own root (`/` + `C:` ->
 * `C:/`, never `/C:`). */
inline std::string remote_join(const std::string &base,
                               const std::string &name) {
    if (is_drive_row(name) && (base.empty() || base == "/")) {
        std::string drive = name;
        while (drive.size() > 2 && drive.back() == '/') {
            drive.pop_back();
        }
        return drive + "/";
    }
    if (base.empty()) {
        return name;
    }
    if (base.back() == '/') {
        return base + name;
    }
    return base + "/" + name;
}

/* Parent of a remote path ("/" stays "/"); empty when there is no
 * parent to go up to (relative single segment). Drive roots live at
 * the filesystem root (`C:/` -> `/`) and drive parents keep their
 * slash (`C:/Users` -> `C:/`, never bare `C:`). */
inline std::string remote_parent(const std::string &path) {
    std::string cpy = path;
    while (cpy.size() > 1 && cpy.back() == '/') {
        cpy.pop_back();
    }
    if (is_drive_row(cpy)) {
        return "/";
    }
    std::size_t slash = cpy.find_last_of('/');
    if (slash == std::string::npos) {
        return {};
    }
    if (slash == 0) {
        return "/";
    }
    std::string parent = cpy.substr(0, slash);
    if (is_drive_row(parent)) {
        return parent + "/";
    }
    return parent;
}

} /* namespace picker */

#endif /* STAAP_WIN_PICKER_H */
