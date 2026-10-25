/* Two-dimensional new-session picker rows for the WinUI shell.
 *
 * Only the catalog/recents JSON shapes parse locally (native ComboBox
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
 * `bridge_run_spawn_remote`).
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

} /* namespace picker */

#endif /* STAAP_WIN_PICKER_H */
