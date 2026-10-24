/// Pending terminal feed backlog for one run.
///
/// Deltas concatenate: each delta advances the view from the last-fed
/// snapshot, so appending is lossless, while overwriting drops everything
/// produced between two drains (the grid drifts and the cursor lands a
/// line off). A later clear+replay resets whatever came before, so a long
/// backlog still converges. Taking consumes the backlog.
public struct FeedBacklog {
    /// Cap: a stalled view drops the backlog rather than growing forever;
    /// the next clear+replay resyncs.
    public static let maxBytes = 65536

    private var pending = ""
    public private(set) var seq: UInt64 = 0

    public init() {}

    /// Append one reconciler delta (ignored when empty).
    public mutating func push(_ feed: String) {
        guard !feed.isEmpty else { return }
        seq += 1
        var next = pending + feed
        if next.utf8.count > Self.maxBytes {
            next = feed
        }
        pending = next
    }

    /// Take everything pending unless `after` already saw it; consuming
    /// clears the backlog so the next push accumulates fresh.
    public mutating func take(after: UInt64) -> String? {
        guard seq != after, !pending.isEmpty else { return nil }
        defer { pending = "" }
        return pending
    }
}
