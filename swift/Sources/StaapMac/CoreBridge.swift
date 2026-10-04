import Foundation

// Swift declarations of the core staticlib C ABI. The canonical
// contract is `include/staap.h` at the repo root; these
// signatures must match it exactly (name, arity, and C type mapping:
// `size_t` -> Int, `_Bool` -> Bool, `char *` -> CChar pointer).
//
// Dumb-shell contract: the roster, selection, filter, live PTYs,
// statuses, key table, feed reconciler, and persistence all live in the
// core run registry — this shell only renders what the core reports.
@_silgen_name("staap_core_new") private func staap_core_new() -> OpaquePointer?
@_silgen_name("staap_core_free") private func staap_core_free(_ core: OpaquePointer?)
@_silgen_name("staap_core_save") private func staap_core_save(_ core: OpaquePointer?) -> Int32
@_silgen_name("staap_session_count") private func staap_session_count(_ core: OpaquePointer?) -> Int
@_silgen_name("staap_session_json") private func staap_session_json(
    _ core: OpaquePointer?, _ row: Int
) -> UnsafeMutablePointer<CChar>?
// Owned JSON of the autodetected CLI catalog ([{id,program,path,
// available}]) and of the folder recents ([String], MRU-first).
@_silgen_name("staap_clis_json") private func staap_clis_json() -> UnsafeMutablePointer<CChar>?
@_silgen_name("staap_recent_json") private func staap_recent_json(
    _ core: OpaquePointer?
) -> UnsafeMutablePointer<CChar>?
// Owned harness id of the effective CLI for `cli` (explicit id, or the
// core's last-used / configured / autodetected resolution for NULL).
@_silgen_name("staap_effective_cli") private func staap_effective_cli(
    _ core: OpaquePointer?,
    _ cli: UnsafePointer<CChar>?
) -> UnsafeMutablePointer<CChar>?
@_silgen_name("staap_screen_text_free") private func staap_screen_text_free(
    _ s: UnsafeMutablePointer<CChar>?
)
@_silgen_name("staap_status") private func staap_status(
    _ core: OpaquePointer?, _ row: Int
) -> Int32
// Run registry: spawn/restart/close/pump/write/resize/screen/exit by row
// id, plus the shared live-count/cap/quit-confirm gates.
@_silgen_name("staap_max_runs") private func staap_max_runs() -> Int
@_silgen_name("staap_live_count") private func staap_live_count(
    _ core: OpaquePointer?
) -> Int
@_silgen_name("staap_is_live") private func staap_is_live(
    _ core: OpaquePointer?, _ id: UnsafePointer<CChar>?
) -> Bool
@_silgen_name("staap_pump_all") private func staap_pump_all(
    _ core: OpaquePointer?
) -> Bool
@_silgen_name("staap_run_spawn") private func staap_run_spawn(
    _ core: OpaquePointer?,
    _ cli: UnsafePointer<CChar>?,
    _ cwd: UnsafePointer<CChar>?,
    _ yolo: Int32,
    _ cols: UInt16,
    _ rows: UInt16,
    _ idOut: UnsafeMutablePointer<CChar>?,
    _ idCap: Int
) -> Int32
@_silgen_name("staap_run_restart") private func staap_run_restart(
    _ core: OpaquePointer?,
    _ id: UnsafePointer<CChar>?,
    _ cols: UInt16,
    _ rows: UInt16
) -> Int32
@_silgen_name("staap_run_close") private func staap_run_close(
    _ core: OpaquePointer?,
    _ id: UnsafePointer<CChar>?
) -> Int32
@_silgen_name("staap_needs_quit_confirm") private func staap_needs_quit_confirm(
    _ core: OpaquePointer?
) -> Bool
@_silgen_name("staap_run_pump") private func staap_run_pump(
    _ core: OpaquePointer?, _ id: UnsafePointer<CChar>?
) -> Bool
@_silgen_name("staap_run_write") private func staap_run_write(
    _ core: OpaquePointer?,
    _ id: UnsafePointer<CChar>?,
    _ data: UnsafePointer<UInt8>?,
    _ len: Int
) -> Int32
@_silgen_name("staap_run_resize") private func staap_run_resize(
    _ core: OpaquePointer?,
    _ id: UnsafePointer<CChar>?,
    _ cols: UInt16,
    _ rows: UInt16
)
@_silgen_name("staap_run_screen_text") private func staap_run_screen_text(
    _ core: OpaquePointer?, _ id: UnsafePointer<CChar>?
) -> UnsafeMutablePointer<CChar>?
@_silgen_name("staap_run_spans_json") private func staap_run_spans_json(
    _ core: OpaquePointer?, _ id: UnsafePointer<CChar>?
) -> UnsafeMutablePointer<CChar>?
@_silgen_name("staap_run_exited") private func staap_run_exited(
    _ core: OpaquePointer?, _ id: UnsafePointer<CChar>?
) -> Bool
// Shared shell helpers: key table, feed reconciler, SGR renderer,
// preview, yolo, age, glyphs, headers, sidebar clamp, filter/selection.
@_silgen_name("staap_key_encode") private func staap_key_encode(
    _ key: UnsafePointer<CChar>?,
    _ keyChar: UnsafePointer<CChar>?,
    _ ctrl: Int32,
    _ alt: Int32,
    _ bytesOut: UnsafeMutablePointer<UInt8>?,
    _ cap: Int
) -> Int32
@_silgen_name("staap_feed_delta") private func staap_feed_delta(
    _ old: UnsafePointer<CChar>?, _ new: UnsafePointer<CChar>?
) -> UnsafeMutablePointer<CChar>?
@_silgen_name("staap_ansi_render") private func staap_ansi_render(
    _ json: UnsafePointer<CChar>?
) -> UnsafeMutablePointer<CChar>?
@_silgen_name("staap_spawn_preview") private func staap_spawn_preview(
    _ cli: UnsafePointer<CChar>?,
    _ folder: UnsafePointer<CChar>?,
    _ yolo: Int32
) -> UnsafeMutablePointer<CChar>?
@_silgen_name("staap_yolo_value") private func staap_yolo_value(
    _ selected: Int32
) -> Int32
@_silgen_name("staap_age_string") private func staap_age_string(
    _ nowUnix: Int64, _ thenUnix: Int64
) -> UnsafeMutablePointer<CChar>?
@_silgen_name("staap_status_glyph") private func staap_status_glyph(
    _ code: Int32
) -> UnsafeMutablePointer<CChar>?
@_silgen_name("staap_section_title") private func staap_section_title(
    _ code: Int32
) -> UnsafeMutablePointer<CChar>?
@_silgen_name("staap_clamp_sidebar") private func staap_clamp_sidebar(
    _ px: Double
) -> Double
@_silgen_name("staap_row_matches") private func staap_row_matches(
    _ core: OpaquePointer?, _ row: Int, _ query: UnsafePointer<CChar>?
) -> Bool
@_silgen_name("staap_set_filter") private func staap_set_filter(
    _ core: OpaquePointer?, _ query: UnsafePointer<CChar>?
)
@_silgen_name("staap_selected") private func staap_selected(
    _ core: OpaquePointer?
) -> Int
@_silgen_name("staap_select") private func staap_select(
    _ core: OpaquePointer?, _ row: Int
)
@_silgen_name("staap_select_step") private func staap_select_step(
    _ core: OpaquePointer?, _ forward: Int32
)
// History membership + expansion (core-owned; the shell only renders).
@_silgen_name("staap_is_history") private func staap_is_history(
    _ core: OpaquePointer?, _ id: UnsafePointer<CChar>?
) -> Bool
@_silgen_name("staap_history_expanded") private func staap_history_expanded(
    _ core: OpaquePointer?
) -> Bool
@_silgen_name("staap_set_history_expanded") private func staap_set_history_expanded(
    _ core: OpaquePointer?, _ expanded: Int32
)
@_silgen_name("staap_last_error") private func staap_last_error() -> UnsafePointer<CChar>
@_silgen_name("staap_pty_free") private func staap_pty_free(_ pty: OpaquePointer?)

/// Copy an owned core string into Swift, then free the core side.
private func copyOwnedString(_ ptr: UnsafeMutablePointer<CChar>?) -> String? {
    guard let ptr else { return nil }
    defer { staap_screen_text_free(ptr) }
    return String(validatingUTF8: ptr)
}

/// Errors from the fallible C ABI calls, carrying the core's
/// thread-local message for display.
enum CoreError: Error, LocalizedError {
    case spawn(message: String)
    case write(message: String)
    case save(message: String)

    var errorDescription: String? {
        switch self {
        case let .spawn(m): "Could not start session: \(m)"
        case let .write(m): "Could not send input: \(m)"
        case let .save(m): "Could not save: \(m)"
        }
    }
}

/// One CLI catalog row decoded from `staap_clis_json`. `available` with a
/// `path` means launchable; anything else renders disabled with its
/// install hint (fail visible, never a silent blank row).
struct CliRow: Decodable {
    var id: String
    var program: String
    var path: String?
    var available: Bool
}

/// One roster row decoded from the core's `ChatSession` JSON
/// (`staap_session_json`). Only the fields the shell renders are kept.
struct SessionRow: Identifiable, Decodable {
    var id: String
    var title: String
    var project: String
    var statusName: String
    var harness: String
    var lastActive: Int64
    var cwd: String?
    var prLinks: [String]?
    var relatedLinks: [String]?

    enum CodingKeys: String, CodingKey {
        case id
        case title
        case project
        case statusName = "status"
        case harness
        case lastActive = "last_active"
        case cwd
        case prLinks = "pr_links"
        case relatedLinks = "related_links"
    }

    var linkCount: Int { (prLinks?.count ?? 0) + (relatedLinks?.count ?? 0) }
}

/// Roster status codes, matching `staap_status` (Attention=0, Idle=1,
/// Working=2; negatives are null handle / out of bounds).
enum RunStatus: Int {
    case attention = 0
    case idle = 1
    case working = 2
}

/// Owned wrapper around the core's opaque roster handle.
final class Core {
    private let handle: OpaquePointer

    init?() {
        guard let handle = staap_core_new() else { return nil }
        self.handle = handle
    }

    deinit { staap_core_free(handle) }

    static func lastError() -> String {
        String(cString: staap_last_error())
    }

    var sessionCount: Int { staap_session_count(handle) }

    func sessionRow(_ row: Int) -> SessionRow? {
        guard let text = copyOwnedString(staap_session_json(handle, row)),
              let data = text.data(using: .utf8)
        else { return nil }
        return try? JSONDecoder().decode(SessionRow.self, from: data)
    }

    func status(_ row: Int) -> Int32 { staap_status(handle, row) }

    /// Persistence is automatic (throttled pump autosave + quit hook):
    /// no manual Save button in any shell.
    func save() throws {
        if staap_core_save(handle) != 0 {
            throw CoreError.save(message: Self.lastError())
        }
    }

    // MARK: - Run registry (dumb-shell contract)

    /// Shared live-run ceiling (was hard-coded per shell).
    static var maxRuns: Int { staap_max_runs() }

    /// Live (attached) run count.
    var liveCount: Int { staap_live_count(handle) }

    /// True when the row id owns a live PTY in the core registry.
    func isLive(_ id: String) -> Bool {
        id.withCString { staap_is_live(handle, $0) }
    }

    /// Pump every live run (statuses/links refresh + re-sort inside).
    /// Returns true when anything visible changed — the roster/status
    /// repaint gate.
    func pumpAll() -> Bool { staap_pump_all(handle) }

    /// Spawn a 2D-launch session attached to a new roster row, under the
    /// shared cap. Returns the row id; throws with the core message
    /// (cap refusal names per-run close).
    func runSpawn(cli: String?, cwd: String?, yolo: Int32, cols: Int, rows: Int) throws -> String {
        var idBuf = [CChar](repeating: 0, count: 256)
        let rc = withOptionalCString(cli) { cliPtr in
            withOptionalCString(cwd) { cwdPtr in
                staap_run_spawn(
                    handle, cliPtr, cwdPtr, yolo,
                    UInt16(clamping: cols), UInt16(clamping: rows),
                    &idBuf, idBuf.count
                )
            }
        }
        guard rc == 0 else {
            throw CoreError.spawn(message: Self.lastError())
        }
        return String(cString: idBuf)
    }

    /// Restart an ended run / resume a historic entry on the same id.
    func runRestart(id: String, cols: Int, rows: Int) throws {
        let rc = id.withCString {
            staap_run_restart(
                handle, $0,
                UInt16(clamping: cols), UInt16(clamping: rows)
            )
        }
        if rc != 0 {
            throw CoreError.spawn(message: Self.lastError())
        }
    }

    /// Close (kill) a run: drops its PTY, removes its entry.
    func runClose(id: String) {
        id.withCString { _ = staap_run_close(handle, $0) }
    }

    /// True when quitting deserves a confirmation step.
    var needsQuitConfirm: Bool { staap_needs_quit_confirm(handle) }

    /// Pump one attached run by id.
    func runPump(id: String) -> Bool {
        id.withCString { staap_run_pump(handle, $0) }
    }

    /// Forward key-encoded bytes to an attached run's child.
    func runWrite(id: String, bytes: [UInt8]) throws {
        let rc = id.withCString { idPtr in
            bytes.withUnsafeBufferPointer { buf in
                staap_run_write(handle, idPtr, buf.baseAddress, buf.count)
            }
        }
        if rc != 0 {
            throw CoreError.write(message: Self.lastError())
        }
    }

    func runResize(id: String, cols: Int, rows: Int) {
        id.withCString {
            staap_run_resize(
                handle, $0,
                UInt16(clamping: cols), UInt16(clamping: rows)
            )
        }
    }

    /// Owned plain-text snapshot of an attached run's screen.
    func runScreenText(id: String) -> String? {
        id.withCString { copyOwnedString(staap_run_screen_text(handle, $0)) }
    }

    /// Owned styled-span snapshot of an attached run's screen.
    func runSpansJson(id: String) -> String? {
        id.withCString { copyOwnedString(staap_run_spans_json(handle, $0)) }
    }

    func runExited(id: String) -> Bool {
        id.withCString { staap_run_exited(handle, $0) }
    }

    // MARK: - Shared shell helpers (one copy in the core)

    /// Encode one logical keypress via the shared key table. Returns the
    /// child bytes, or nil when the native control keeps the key.
    static func keyEncode(key: String, keyChar: String?, ctrl: Bool, alt: Bool) -> [UInt8]? {
        var buf = [UInt8](repeating: 0, count: 16)
        let n = key.withCString { keyPtr in
            withOptionalCString(keyChar) { charPtr in
                staap_key_encode(
                    keyPtr, charPtr,
                    ctrl ? 1 : 0, alt ? 1 : 0,
                    &buf, buf.count
                )
            }
        }
        guard n > 0 else { return nil }
        return Array(buf.prefix(Int(n)))
    }

    /// Feed text advancing a view showing `old` to also show `new`.
    static func feedDelta(old: String, new: String) -> String? {
        old.withCString { oldPtr in
            new.withCString { newPtr in
                copyOwnedString(staap_feed_delta(oldPtr, newPtr))
            }
        }
    }

    /// Render a spans-JSON document to an SGR stream (nil = fall back to
    /// the plain-text snapshot).
    static func ansiRender(json: String) -> String? {
        json.withCString { copyOwnedString(staap_ansi_render($0)) }
    }

    /// One-line spawn preview (`runs: muse in ~/api + yolo`).
    static func spawnPreview(cli: String?, folder: String?, yolo: Int32) -> String {
        let text = withOptionalCString(cli) { cliPtr in
            withOptionalCString(folder) { folderPtr in
                copyOwnedString(staap_spawn_preview(cliPtr, folderPtr, yolo))
            }
        }
        return text ?? "runs: muse"
    }

    /// Tri-state yolo int from a segmented-control index.
    static func yoloValue(_ selected: Int32) -> Int32 {
        staap_yolo_value(selected)
    }

    /// Human age for `lastActive` (`just now`, `5m ago`, ...).
    static func ageString(now: Int64, then: Int64) -> String {
        copyOwnedString(staap_age_string(now, then)) ?? "just now"
    }

    /// Non-color status marker for a status code.
    static func statusGlyph(_ code: Int32) -> String {
        copyOwnedString(staap_status_glyph(code)) ?? "○"
    }

    /// Section header for a status code.
    static func sectionTitle(_ code: Int32) -> String {
        copyOwnedString(staap_section_title(code)) ?? ""
    }

    /// True when roster row `row` passes the sidebar filter.
    func rowMatches(_ row: Int, query: String) -> Bool {
        query.withCString { staap_row_matches(handle, row, $0) }
    }

    /// Replace the title filter (snaps selection into the matches).
    func setFilter(_ query: String?) {
        withOptionalCString(query) { staap_set_filter(handle, $0) }
    }

    /// Selected roster row index (the shell highlights this row).
    var selected: Int { staap_selected(handle) }

    /// Move selection to a row (clamped into range).
    func select(_ row: Int) { staap_select(handle, row) }

    /// Step selection next/prev within the filter matches.
    func selectStep(forward: Bool) {
        staap_select_step(handle, forward ? 1 : 0)
    }

    /// True when the row id is historic (no live PTY attached —
    /// attached means active, even when the child already exited).
    func isHistory(_ id: String) -> Bool {
        id.withCString { staap_is_history(handle, $0) }
    }

    /// True when the History section renders expanded (collapsed by
    /// default; persisted via `save()`).
    var historyExpanded: Bool { staap_history_expanded(handle) }

    /// Set the History expansion state (persist with `save()`).
    func setHistoryExpanded(_ expanded: Bool) {
        staap_set_history_expanded(handle, expanded ? 1 : 0)
    }

    /// Autodetected CLI catalog ([CliRow] in core order). Decodes to []
    /// (never throws): an unreachable catalog renders as an empty picker
    /// section, not a startup failure.
    func cliCatalog() -> [CliRow] {
        guard let text = copyOwnedString(staap_clis_json()),
              let data = text.data(using: .utf8),
              let rows = try? JSONDecoder().decode([CliRow].self, from: data)
        else { return [] }
        return rows
    }

    /// Persisted folder recents (MRU-first). Decodes to [] when absent.
    func recentFolders() -> [String] {
        guard let text = copyOwnedString(staap_recent_json(handle)),
              let data = text.data(using: .utf8),
              let recents = try? JSONDecoder().decode([String].self, from: data)
        else { return [] }
        return recents
    }

    /// Harness id of the effective CLI for `cli` (explicit id, or the
    /// core's resolution for nil): what a repeat-last spawn will run.
    /// Falls back to "muse" when the core cannot answer. Repeat-last and
    /// picker spawns go through `runSpawn`, which records launch memory
    /// in the core — no separate note step.
    func effectiveCli(_ cli: String? = nil) -> String {
        let text = withOptionalCString(cli) { cliPtr in
            copyOwnedString(staap_effective_cli(handle, cliPtr))
        }
        guard let text, !text.isEmpty else { return "muse" }
        return text
    }
}

/// Run `body` with an optional Swift string as a nullable C string
/// (nil or empty Swift maps to NULL: inherit / effective default).
private func withOptionalCString<T>(
    _ value: String?,
    _ body: (UnsafePointer<CChar>?) throws -> T
) rethrows -> T {
    guard let value, !value.isEmpty else {
        return try body(nil)
    }
    return try value.withCString { try body($0) }
}
