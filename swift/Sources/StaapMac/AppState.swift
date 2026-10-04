import Foundation
import ShellSupport

/// Dumb-renderer state over the core run registry, on the main thread.
///
/// The roster, selection, filter, live PTYs, statuses, and links all
/// live in the core (`staap_pump_all` refreshes them on every tick, like
/// the shared pump): this type only mirrors rows for SwiftUI rendering,
/// routes widget events into the core, and carries the view-feed cache.
/// No shell-local PTY map, no shell-local ids — spawns attach real rows
/// in the core, so every run rows in the roster on every shell.
@MainActor
final class AppState: ObservableObject {
    /// Default grid until the terminal view reports its real size.
    static let defaultCols = 80
    static let defaultRows = 24

    @Published private(set) var rows: [SessionRow] = []
    @Published private(set) var statuses: [String: Int] = [:]
    @Published var selection: String?
    @Published var filter = "" {
        didSet {
            if filter != oldValue {
                core.setFilter(filter.isEmpty ? nil : filter)
                reloadRoster()
            }
        }
    }

    @Published var pendingError: String?
    /// 2D-launch picker sheet visibility (set by the menu, the caret,
    /// or Cmd-Shift-N; the sheet resets it on dismiss).
    @Published var pickerOpen = false

    /// Feeds waiting for the terminal view: row id -> (text, sequence).
    /// The sequence lets the view skip what it already fed.
    @Published private(set) var feeds: [String: (text: String, seq: UInt64)] = [:]

    private let core: Core
    private var fedText: [String: String] = [:]
    private var feedSeq: UInt64 = 0
    private var grid = (cols: defaultCols, rows: defaultRows)
    private var timer: Timer?
    private var saveTick = 0

    init(core: Core) {
        self.core = core
        reloadRoster()
        if selection == nil {
            selection = rows.first?.id
        }
        timer = Timer.scheduledTimer(withTimeInterval: 0.05, repeats: true) {
            [weak self] _ in
            MainActor.assumeIsolated { self?.pump() }
        }
    }

    deinit { timer?.invalidate() }

    // MARK: - Roster (mirrored from the core registry)

    var attentionCount: Int {
        statuses.values.filter { $0 == RunStatus.attention.rawValue }.count
    }

    var filteredRows: [SessionRow] {
        rows.filter { row in
            filter.isEmpty || core.rowMatches(index(of: row.id), query: filter)
        }
    }

    func rows(with status: RunStatus) -> [SessionRow] {
        rows.filter {
            statuses[$0.id] == status.rawValue
                && core.isLive($0.id)
                && (filter.isEmpty || core.rowMatches(index(of: $0.id), query: filter))
        }
    }

    private func index(of id: String) -> Int {
        rows.firstIndex(where: { $0.id == id }) ?? -1
    }

    /// History membership (core-owned rule): ids with no live PTY in the
    /// core registry. Attached means active, even when exited.
    func isHistory(_ id: String) -> Bool { core.isHistory(id) }

    func status(of row: SessionRow) -> RunStatus {
        RunStatus(rawValue: statuses[row.id] ?? RunStatus.idle.rawValue) ?? .idle
    }

    /// Accessible status name for a row (VoiceOver label suffix).
    func statusName(of row: SessionRow) -> String {
        switch status(of: row) {
        case .attention: "needs input"
        case .working: "working"
        case .idle: "idle"
        }
    }

    /// History rows: ids with no live PTY (collapsed group at the end,
    /// matching the other shells). Expansion lives in the core
    /// (persisted via `save()`); the shell only renders it.
    var historyRows: [SessionRow] {
        rows.filter {
            core.isHistory($0.id)
                && (filter.isEmpty || core.rowMatches(index(of: $0.id), query: filter))
        }
    }

    var hasHistory: Bool { !historyRows.isEmpty }

    /// History expansion, core-owned (collapsed by default).
    var historyExpanded: Bool { core.historyExpanded }

    /// Toggle History expansion and persist it (same funnel every shell
    /// shares: core state + autosave, no shell-local expansion state).
    func toggleHistory() {
        core.setHistoryExpanded(!core.historyExpanded)
        save()
        reloadRoster()
    }

    /// Non-color status marker for a row (shared core glyphs).
    func glyph(of row: SessionRow) -> String {
        Core.statusGlyph(Int32(statuses[row.id] ?? RunStatus.idle.rawValue))
    }

    /// Human age for a row (shared core buckets).
    func age(of row: SessionRow) -> String {
        let now = Int64(Date.now.timeIntervalSince1970)
        return Core.ageString(now: now, then: row.lastActive)
    }

    private func reloadRoster() {
        var loaded: [SessionRow] = []
        for i in 0 ..< core.sessionCount {
            if let row = core.sessionRow(i) {
                loaded.append(row)
                statuses[row.id] = Int(core.status(i))
            }
        }
        rows = loaded
        // Mirror the core selection (filter snaps it on the core side).
        let sel = core.selected
        if sel < rows.count {
            selection = rows[sel].id
        } else if !rows.isEmpty, selection == nil {
            selection = rows.first?.id
        }
    }

    // MARK: - Spawn / restart / close / converse / resize

    /// Spawn (or re-attach) the selected row: historic entries resume,
    /// ended runs restart on the same id (keeps title and links).
    func spawnSelected() {
        guard let id = selection else { return }
        do {
            try core.runRestart(id: id, cols: grid.cols, rows: grid.rows)
            fedText[id] = ""
            reloadRoster()
        } catch {
            pendingError = error.localizedDescription
        }
    }

    // MARK: - 2D launch (folder × CLI + yolo)

    /// Split-button main action: repeat the last launch instantly (the
    /// null-CLI/null-folder/zero-yolo form resolves the core's effective
    /// default: last-used, configured, autodetected). Attaches a real
    /// roster row in the core under the shared cap. A failed repeat
    /// surfaces in `pendingError`, never silently.
    func repeatLastSession() {
        do {
            let id = try core.runSpawn(cli: nil, cwd: nil, yolo: 0,
                                       cols: grid.cols, rows: grid.rows)
            fedText[id] = ""
            reloadRoster()
            selection = id
        } catch {
            pendingError = error.localizedDescription
        }
    }

    /// Confirmed picker launch: explicit folder × CLI + tri-state yolo
    /// (1 = on once, -1 = off once, 0 = config default). Records the
    /// combination in the core so the next repeat replays it.
    func confirmPicker(cli: String, folder: String?, yolo: Int32) {
        do {
            let id = try core.runSpawn(cli: cli, cwd: folder, yolo: yolo,
                                       cols: grid.cols, rows: grid.rows)
            fedText[id] = ""
            reloadRoster()
            selection = id
        } catch {
            pendingError = error.localizedDescription
        }
    }

    /// Restart/resume the selected run on the same id.
    func restartSelected() {
        guard let id = selection else { return }
        do {
            try core.runRestart(id: id, cols: grid.cols, rows: grid.rows)
            fedText[id] = ""
            reloadRoster()
        } catch {
            pendingError = error.localizedDescription
        }
    }

    /// Close (kill) the selected run + entry, then persist.
    func closeSelected() {
        guard let id = selection else { return }
        core.runClose(id: id)
        fedText.removeValue(forKey: id)
        feeds.removeValue(forKey: id)
        save()
        reloadRoster()
    }

    /// Fresh CLI catalog for the picker sheet (re-read on every open so
    /// the list is never stale).
    func pickerCatalog() -> [CliRow] { core.cliCatalog() }

    /// Folder recents for the picker sheet (MRU-first).
    func pickerRecents() -> [String] { core.recentFolders() }

    func hasLivePty(_ id: String) -> Bool { core.isLive(id) }

    func sendToPty(rowId: String, bytes: [UInt8]) {
        do {
            try core.runWrite(id: rowId, bytes: bytes)
        } catch {
            pendingError = error.localizedDescription
        }
    }

    func terminalResized(rowId: String, cols: Int, rows: Int) {
        grid = (cols, rows)
        core.runResize(id: rowId, cols: cols, rows: rows)
    }

    /// Feed text the terminal view has not shown yet, if any.
    func takeFeed(for rowId: String, after seq: UInt64) -> String? {
        guard let feed = feeds[rowId], feed.seq != seq else { return nil }
        return feed.text
    }

    func feedSeq(for rowId: String) -> UInt64 { feeds[rowId]?.seq ?? 0 }

    // MARK: - Selection (core-owned)

    func select(row: SessionRow) {
        if let i = rows.firstIndex(where: { $0.id == row.id }) {
            core.select(i)
            selection = row.id
        }
    }

    func selectStep(forward: Bool) {
        core.selectStep(forward: forward)
        reloadRoster()
    }

    // MARK: - Persistence (automatic; no manual Save)

    /// Autosave: throttled pump persist + quit/close hooks. Kept explicit
    /// (not private) so the app delegate and close path call one funnel.
    func save() {
        do {
            try core.save()
        } catch {
            pendingError = error.localizedDescription
        }
    }

    /// Dirty-quit gate: confirm when any run is Working/Attention or any
    /// PTY is live (core-owned rule, same as every shell).
    var needsQuitConfirm: Bool { core.needsQuitConfirm }

    // MARK: - Pump

    private func pump() {
        // The core registry feeds every live run (statuses, links, and
        // re-sort inside) — the roster now actually moves, unlike the
        // old launch-snapshot rows.
        let dirty = core.pumpAll()
        // Mirror new/changed rows (spawn attaches rows in the core now).
        reloadRoster()
        for row in rows {
            let id = row.id
            guard core.isLive(id) else { continue }
            // Color-preserving render of the same screen: the styled spans
            // re-emitted as SGR when they decode, else the plain snapshot
            // (both through the shared core helpers now).
            let current: String
            if let spans = core.runSpansJson(id: id),
               let styled = Core.ansiRender(json: spans)
            {
                current = styled
            } else if let snapshot = core.runScreenText(id: id) {
                current = snapshot
            } else {
                continue
            }
            let shown = fedText[id, default: ""]
            if let feed = Core.feedDelta(old: shown, new: current),
               !feed.isEmpty
            {
                feedSeq += 1
                feeds[id] = (feed, feedSeq)
            }
            fedText[id] = current
        }
        // Autosave throttle (replaces the manual Save button): persist at
        // most every ~5s while dirty, like the shared pump persist.
        if dirty {
            saveTick += 1
            if saveTick % 100 == 0 {
                save()
            }
        }
    }
}
