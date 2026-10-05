import AppKit
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
        applyThemeColors(to: view, darkMode: darkMode)
        view.terminalDelegate = context.coordinator
        // Fresh view per row: replay the full current text first, then
        // only newer feeds (issue #106).
        let initial = context.coordinator.initialText
        if !initial.isEmpty {
            view.feed(text: initial)
        }
        context.coordinator.lastDarkMode = darkMode
        // Become key so keystrokes reach the child right away.
        DispatchQueue.main.async {
            view.window?.makeFirstResponder(view)
        }
        return view
    }

    func updateNSView(_ view: TerminalView, context: Context) {
        context.coordinator.drainFeed(into: view)
        if context.coordinator.lastDarkMode != darkMode {
            context.coordinator.lastDarkMode = darkMode
            applyThemeColors(to: view, darkMode: darkMode)
        }
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
        fileprivate var lastFedSeq: UInt64 = 0
        fileprivate var initialText = ""
        fileprivate var lastDarkMode = false

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

        /// Feed anything the pump produced since the last drain.
        func drainFeed(into view: TerminalView) {
            let seq = state.feedSeq(for: rowId)
            guard seq != lastFedSeq,
                  let text = state.takeFeed(for: rowId, after: lastFedSeq)
            else { return }
            lastFedSeq = seq
            view.feed(text: text)
            // Place the caret where the child put it, not at the end of
            // fed text (issue #104).
            if let cursor = state.coreCursor(for: rowId) {
                var cup = String(Character(UnicodeScalar(UInt8(27)))) + "["
                cup += String(cursor.row + 1) + ";" + String(cursor.col + 1) + "H"
                view.feed(text: cup)
            }
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
