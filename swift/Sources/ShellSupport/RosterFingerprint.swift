/// One roster row's worth of fingerprint input (primitives only, so the
/// shell's stillness gate stays unit-testable without the core).
public struct RosterFingerprintRow {
    public var id: String
    public var title: String
    public var project: String
    public var harness: String
    public var lastActive: Int64
    public var linkCount: Int
    public var status: Int
    /// Live (attached PTY) vs historic (no live PTY): a restart/resume
    /// moves the row between the active sections and History without
    /// changing any other field, so the stillness gate must see it.
    public var live: Bool

    public init(
        id: String, title: String, project: String, harness: String,
        lastActive: Int64, linkCount: Int, status: Int, live: Bool = true
    ) {
        self.id = id
        self.title = title
        self.project = project
        self.harness = harness
        self.lastActive = lastActive
        self.linkCount = linkCount
        self.status = status
        self.live = live
    }
}

/// Fingerprint of the last published roster: reloads that change nothing
/// visible must skip the assignment so the sidebar List sits still
/// (constant re-diffs leave stale ghosts and torn animations). Ages only
/// move by the minute, so the clock arrives pre-bucketed by the caller.
public func rosterFingerprint(
    rows: [RosterFingerprintRow],
    historyExpanded: Bool,
    selection: String?,
    minute: Int
) -> String {
    var out = ""
    out.reserveCapacity(rows.count * 64)
    for row in rows {
        out += row.id + "\u{1F}" + row.title + "\u{1F}"
            + row.project + "\u{1F}" + row.harness + "\u{1F}"
            + String(row.lastActive) + "\u{1F}"
            + String(row.linkCount) + "\u{1F}"
            + String(row.status) + "\u{1F}"
            + (row.live ? "L" : "h") + "\n"
    }
    out += (historyExpanded ? "H" : "h") + (selection ?? "-") + String(minute)
    return out
}
