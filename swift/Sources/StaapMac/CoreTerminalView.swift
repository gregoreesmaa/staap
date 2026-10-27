import AppKit
import ShellSupport
import SwiftTerm
import SwiftUI

/// The session terminal: a SwiftTerm `TerminalView` (native selection,
/// copy/paste, scrollback, and Cmd-F find) driven by the core.
///
/// Output flows core PTY -> `staap_pump`/`staap_screen_text`+`staap_spans_json` ->
/// SGR snapshot delta -> `view.feed(text:)`; keystrokes flow view ->
/// `send` delegate ->
/// `staap_write`; window resizes flow view -> `sizeChanged` -> `staap_resize`.
/// The view follows the theme (text colors resolved against an explicit
/// light/dark appearance) and owns all of the terminal grid; no terminal
/// grid is drawn here.
struct CoreTerminalView: NSViewRepresentable {
    @ObservedObject var state: AppState
    var rowId: String
    var darkMode: Bool

    func makeCoordinator() -> Coordinator {
        Coordinator(state: state, rowId: rowId)
    }

    func makeNSView(context: Context) -> TerminalView {
        let font = NSFont.monospacedSystemFont(ofSize: 13, weight: .regular)
        let view = TerminalView(frame: .zero, font: font)
        // Make Option produce macOS characters rather than Meta/ESC sequences,
        // so the terminal feels like a normal macOS app (e.g., Estonian Option+' → `).
        view.optionAsMetaKey = false
        applyThemeColors(to: view, darkMode: darkMode)
        view.terminalDelegate = context.coordinator
        // Fresh view per row: replay the full current text first, then
        // only newer feeds (issue #106).
        let initial = context.coordinator.initialText
        if !initial.isEmpty {
            view.feed(text: Self.normalized(initial))
        }
        context.coordinator.lastDarkMode = darkMode
        context.coordinator.startPolling(view: view)
        // Become key so keystrokes reach the child right away.
        DispatchQueue.main.async {
            view.window?.makeFirstResponder(view)
        }
        return view
    }

    static func normalized(_ text: String) -> String {
        Coordinator.normalized(text)
    }

    func updateNSView(_ view: TerminalView, context: Context) {
        context.coordinator.drainFeed(into: view)
        if context.coordinator.lastDarkMode != darkMode {
            context.coordinator.lastDarkMode = darkMode
            applyThemeColors(to: view, darkMode: darkMode)
        }
    }

    static func dismantleNSView(_: TerminalView, coordinator: Coordinator) {
        coordinator.stopPolling()
    }

    /// Terminal colors as a pure function of the theme prop — never of
    /// ambient appearance. Snapshotting ambient colors can catch the
    /// pre-flip mode (e.g. a fresh view still on the system appearance
    /// under a forced theme), and the change gate above would then lock
    /// those stale colors in, leaving the terminal one mode behind the
    /// sidebar forever. Background is pure terminal black/white, not
    /// the dark-gray system fill.
    private func applyThemeColors(to view: TerminalView, darkMode: Bool) {
        var fg: NSColor?
        if let appearance = NSAppearance(named: darkMode ? .darkAqua : .aqua) {
            appearance.performAsCurrentDrawingAppearance {
                fg = NSColor.textColor.usingColorSpace(.sRGB)
            }
        }
        view.nativeForegroundColor = fg ?? .textColor
        view.nativeBackgroundColor = darkMode ? .black : .white
    }

    @MainActor
    final class Coordinator: NSObject, @MainActor TerminalViewDelegate {
        private let state: AppState
        private let rowId: String
        private static let probeEnabled = ProcessInfo.processInfo.environment["STAAP_CARET_DEBUG"] != nil
        fileprivate var lastFedSeq: UInt64 = 0
        fileprivate var initialText = ""
        fileprivate var lastDarkMode = false
        fileprivate var lastPlacedRow: Int = -1
        fileprivate var lastPlacedCol: Int = -1
        fileprivate var lastProbeLog = Date.distantPast
        private var pollTimer: Timer?
        private weak var pollView: TerminalView?

        init(state: AppState, rowId: String) {
            self.state = state
            self.rowId = rowId
            // Replay baseline: the view is (re)created per row (see
            // `.id(rowId)`), so it starts from the row's full current
            // text and only drains newer pump feeds after that (#106).
            let replay = state.replayText(for: rowId)
            self.initialText = replay.text
            self.lastFedSeq = replay.seq
        }

        /// Feed anything the pump produced since the last drain, then place
        /// the same-tick caret. Chunks arrive pre-anchored (absolute CUP
        /// plus delta) or as position-independent resyncs, so a
        /// cursor-addressed caret mid-screen cannot shift the grid.
        /// Static screens re-place only on moves.
        func drainFeed(into view: TerminalView) {
            let seq = state.feedSeq(for: rowId)
            var fed = false
            if seq != lastFedSeq,
               let text = state.takeFeed(for: rowId, after: lastFedSeq)
            {
                lastFedSeq = seq
                view.feed(text: text)
                fed = true
            }
            guard let cursor = state.coreCursor(for: rowId) else { return }
            if fed || cursor.row != lastPlacedRow || cursor.col != lastPlacedCol {
                view.feed(text: cupPosition(row: cursor.row, col: cursor.col))
                lastPlacedRow = cursor.row
                lastPlacedCol = cursor.col
            }
        probeCaret(view: view, core: cursor, fed: fed)
        }

        /// Caret-divergence probe for the Claude-only 1-row-low caret
    /// (issue #104): after placing the core caret, compare SwiftTerm's
    /// own grid caret and drawn caret row against it. Gated by
    /// STAAP_CARET_DEBUG so normal runs pay one branch; mismatches log
    /// to stderr throttled to one line per 2 s. Ship the log from a
    /// repro run instead of guessing at the diverging byte.
    private func probeCaret(view: TerminalView, core: (row: Int, col: Int), fed: Bool) {
        guard Self.probeEnabled else { return }
        let term = view.getTerminal()
        let loc = term.getCursorLocation()
        let frame = view.caretFrame
        let drawnRow: Int
        if frame.height > 0 {
            if view.isFlipped {
                drawnRow = Int((frame.minY / frame.height).rounded())
            } else {
                drawnRow = Int(((view.bounds.height - frame.minY - frame.height) / frame.height).rounded())
            }
        } else {
            drawnRow = -1
        }
        guard loc.x != core.col || loc.y != core.row || (drawnRow >= 0 && drawnRow != core.row) else { return }
        let now = Date()
        guard now.timeIntervalSince(lastProbeLog) > 2 else { return }
        lastProbeLog = now
        // Raw frame numbers validate the drawnRow math; sync flags a
        // transient mid-block sample (displays pause inside DEC 2026).
        let sync = term.synchronizedOutputActive
        let scroll = view.scrollPosition
        // Text-shift check: the view's visible tail vs the core snapshot
        // tail. A parse divergence lands text on different rows than the
        // core thinks while both carets still agree.
        let yDisp = term.getTopVisibleRow()
        let vtext = term.getText(start: Position(col: 0, row: yDisp), end: Position(col: term.cols, row: yDisp + term.rows))
        let snapLines = stripProbeSGR(state.replayText(for: rowId).text).split(separator: "\n", omittingEmptySubsequences: false).map { $0.trimmingCharacters(in: .whitespaces) }
        let viewLines = vtext.split(separator: "\n", omittingEmptySubsequences: false).map { $0.trimmingCharacters(in: .whitespaces) }
        let tail = min(min(snapLines.count, viewLines.count), term.rows)
        let snapTail = snapLines.suffix(tail).joined(separator: "|")
        let viewTail = viewLines.suffix(tail).joined(separator: "|")
        // View-identity census: a stale/detached view would explain a live
        // grid with a frozen caret frame (no window, never composited).
        let inWindow = view.window != nil
        let noSuperview = view.superview == nil
        let subviewFrames = String(view.subviews.map { "\($0.frame)" }.joined(separator: ";").prefix(200))
        let line = "staap caret-probe row=\(rowId) core=(\(core.row),\(core.col)) view=(\(loc.y),\(loc.x)) drawnRow=\(drawnRow) grid=\(term.cols)x\(term.rows) fed=\(fed) sync=\(sync) frameRatio=\(frame.minY)/\(frame.height)/\(view.bounds.height) flipped=\(view.isFlipped) scroll=\(scroll) textMatch=\(snapTail == viewTail) inWindow=\(inWindow) noSuperview=\(noSuperview) subviews=\(subviewFrames) snap=\(String(snapTail.prefix(80))) view=\(String(viewTail.prefix(80)))\n"
        if let data = line.data(using: .utf8) {
            try? FileHandle.standardError.write(contentsOf: data)
        }
    }

    /// SGR-only strip for the probe's snapshot tail (the styled render
    /// carries `ESC [ params m` runs; plain snapshots pass through).
    private func stripProbeSGR(_ s: String) -> String {
        var out = ""
        out.reserveCapacity(s.count)
        var i = s.startIndex
        while i < s.endIndex {
            if s[i] == "\u{1B}", let nxt = s.index(i, offsetBy: 1, limitedBy: s.endIndex), nxt < s.endIndex, s[nxt] == "[" {
                var j = s.index(after: nxt)
                while j < s.endIndex, s[j] != "m" { j = s.index(after: j) }
                i = j < s.endIndex ? s.index(after: j) : j
            } else {
                out.append(s[i])
                i = s.index(after: i)
            }
        }
        return out
    }

    /// CRLF normalization for full replays (mirrors the shared feed
        /// reconciler: a bare LF is line-feed-only in the view and would
        /// stair-step the replay).
        fileprivate static func normalized(_ text: String) -> String {
            text.replacingOccurrences(of: "\r\n", with: "\n")
                .replacingOccurrences(of: "\n", with: "\r\n")
        }

        /// Poll the feed cache on the main runloop so the terminal stays
        /// live without routing every output chunk through SwiftUI state
        /// (which would re-evaluate the whole sidebar 20x/sec while any
        /// run streams, leaving stale hover/selection ghosts behind).
        func startPolling(view: TerminalView) {
            stopPolling()
            pollView = view
            pollTimer = Timer.scheduledTimer(withTimeInterval: 0.05, repeats: true) { [weak self] _ in
                MainActor.assumeIsolated {
                    guard let strongSelf = self, let view = strongSelf.pollView else { return }
                    strongSelf.drainFeed(into: view)
                }
            }
        }

        /// Stop the poll (paired with `dismantleNSView`).
        func stopPolling() {
            pollTimer?.invalidate()
            pollTimer = nil
        }

        // MARK: - TerminalViewDelegate

        /// SwiftTerm hands us raw key bytes; they travel to the child
        /// through the core run registry (`staap_run_write`). SwiftTerm's
        /// own encoding already matches the shared core key table's
        /// output for the keys it produces (Return → CR, Ctrl+C → ETX),
        /// so no re-encoding is needed here.
        func send(source _: TerminalView, data: ArraySlice<UInt8>) {
            state.sendToPty(rowId: rowId, bytes: Array(data))
        }

        /// The view recomputes its grid from the window frame; keep the
        /// core PTY and its emulator the same size (core null-op safe).
        func sizeChanged(source _: TerminalView, newCols: Int, newRows: Int) {
            state.terminalResized(rowId: rowId, cols: newCols, rows: newRows)
        }

        func setTerminalTitle(source _: TerminalView, title _: String) {}
        func hostCurrentDirectoryUpdate(source _: TerminalView, directory _: String?) {}
        func scrolled(source _: TerminalView, position _: Double) {}

        /// The view's own damage tracker; the shell repaints from the
        /// pump instead, so this is a no-op.
        func rangeChanged(source _: TerminalView, startY _: Int, endY _: Int) {}
    }
}
