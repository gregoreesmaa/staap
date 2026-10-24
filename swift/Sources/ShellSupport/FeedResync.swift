import Foundation

/// Absolute CUP for a 0-based caret cell.
///
/// Shared by the pump's re-anchor and the view's placement so the two
/// can never drift apart (1-based CUP is the one thing every view
/// agrees on).
public func cupPosition(row: Int, col: Int) -> String {
    "\u{1B}[\(row + 1);\(col + 1)H"
}

/// Position-independent resync for the terminal view: clear the grid
/// and replay the whole snapshot.
///
/// Relative deltas (appended suffix, scrolled tail) assume the view
/// cursor sits at the end of the previously fed text. A
/// cursor-addressed caret (fullscreen TUIs) rests mid-screen instead,
/// so feeding a relative delta from there prints from the wrong cell:
/// text lands rows off and the caret looks a row too low. Resyncing
/// re-anchors the grid before the caret is placed.
public func resyncFeed(current: String) -> String {
    TerminalFeed.clearScreen + TerminalFeed.normalizeNewlines(current)
}

/// End of a snapshot's fed stream: the cursor cell after feeding the
/// whole snapshot, or nil when the last line is not plain ASCII.
///
/// Rows are `\n`-separated with SGR spans inline; only the last line
/// matters. ASCII text is exactly one cell per scalar, so the answer
/// is exact; anything else returns nil and the caller resyncs instead
/// of guessing widths. A relative delta fed from this cell reproduces
/// the snapshot exactly, which is what makes the pump's re-anchor
/// correct by construction.
public func snapshotEnd(_ snapshot: String) -> (row: Int, col: Int)? {
    let lines = snapshot.split(separator: "\n", omittingEmptySubsequences: false)
    guard let lastLine = lines.last else { return (0, 0) }
    let text = stripSGR(String(lastLine))
    guard text.allSatisfy({ $0.isASCII }) else { return nil }
    return (lines.count - 1, text.count)
}

/// Strip SGR escape sequences (`ESC [ params m`), leaving cell text.
func stripSGR(_ s: String) -> String {
    var out = ""
    out.reserveCapacity(s.count)
    var i = s.startIndex
    while i < s.endIndex {
        if s[i] == "\u{1B}",
            let nxt = s.index(i, offsetBy: 1, limitedBy: s.endIndex),
            nxt < s.endIndex, s[nxt] == "["
        {
            var j = s.index(after: nxt)
            while j < s.endIndex, s[j] != "m" {
                j = s.index(after: j)
            }
            i = j < s.endIndex ? s.index(after: j) : j
        } else {
            out.append(s[i])
            i = s.index(after: i)
        }
    }
    return out
}
