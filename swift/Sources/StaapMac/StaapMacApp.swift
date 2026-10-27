import AppKit
import ShellSupport
import SwiftTerm
import SwiftUI

/// macOS shell over the core staticlib (issue #62).
///
/// The roster, spawn, pump, write, resize, and screen-text calls all go
/// through the C ABI in `CoreBridge.swift`; rendering is SwiftUI plus a
/// SwiftTerm terminal view. Entry is via `main.swift` (which also hosts
/// the `--smoke` runtime check).
struct StaapMacApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) var delegate
    @StateObject private var appState: AppState

    init() {
        guard let core = Core() else {
            fatalError("staap_core_new returned NULL (allocation failure)")
        }
        let state = AppState(core: core)
        _appState = StateObject(wrappedValue: state)
        delegate.state = state
        // Screenshot-seed spawns (CI mock captures): --shot-spawn N or
        // --shot-spawn=N attaches N real runs up front so captures
        // show live rows (shared ShellSupport parsing).
        let shotCount = shotSpawnCount(CommandLine.arguments)
        if shotCount > 0 {
            MainActor.assumeIsolated {
                for _ in 0 ..< shotCount {
                    state.repeatLastSession()
                }
            }
        }
    }

    var body: some Scene {
        WindowGroup {
            ContentView(state: appState)
                .frame(minWidth: 720, minHeight: 460)
        }
        // No title bar and no toolbar: the window keeps only the
        // floating traffic lights, so the sidebar and terminal own the
        // full height.
        .windowStyle(.hiddenTitleBar)
        .commands {
            CommandGroup(replacing: .newItem) {
                // Single New Session entry: every press shows the
                // in-place screen on the right side (no sheet/modal,
                // no split-button repeat).
                Button("New Session") {
                    appState.pickerOpen = true
                }
                .keyboardShortcut("n", modifiers: .command)
                // Restart/close parity with the other shells (Ctrl+R /
                // Ctrl+W there; Cmd-R / Cmd-W here). Persistence is
                // automatic — no Save command.
                Button("Restart Selected Run", action: appState.restartSelected)
                    .keyboardShortcut("r", modifiers: .command)
                Button("Close Selected Run", action: appState.closeSelected)
                    .keyboardShortcut("w", modifiers: .command)
            }
        }
    }
}

/// App delegate: activation fix + config persistence on quit.
///
/// This target is a bare SwiftPM executable (no app bundle, no
/// Info.plist), so AppKit never grants it regular activation: the
/// process stays invisible to the window server and the WindowGroup
/// window never appears (issue #67). Becoming a regular app and
/// activating on launch is what makes the window show.
final class AppDelegate: NSObject, NSApplicationDelegate {
    var state: AppState?
    /// Token for the app-global backtick monitor (see installBacktickMonitor).
    private var backtickMonitor: Any?

    func applicationDidFinishLaunching(_: Notification) {
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
        // The manual theme override is gone; drop the stale default.
        UserDefaults.standard.removeObject(forKey: "appearance")
        WindowPaint.sync()
        installBacktickMonitor()
        // The WindowGroup window may not exist yet at launch, so paint
        // on every main-window change too.
        _ = NotificationCenter.default.addObserver(
            forName: NSWindow.didBecomeMainNotification,
            object: nil, queue: .main
        ) { _ in WindowPaint.sync() }
    }

    func applicationWillTerminate(_: Notification) {
        // Best effort on the way out; errors have nowhere to show.
        MainActor.assumeIsolated {
            self.state?.save()
        }
        uninstallBacktickMonitor()
    }

    /// Backticks reach the PTY even though AppKit's `interpretKeyEvents`
    /// path swallows a plain one (routing it to `cancelOperation` → ESC)
    /// before SwiftTerm can insert it.
    ///
    /// SwiftTerm's `keyDown` is `public`, not `open`, so it can't be
    /// overridden from this module — and the fix is the shell's byte
    /// transport to own, not the core's. An app-global local monitor
    /// therefore intercepts the key *before* it reaches any responder:
    /// when the focused view is a terminal and the key is an *unmodified*
    /// backtick, send byte 0x60 straight to that view and swallow the event.
    /// With `optionAsMetaKey` disabled, Option-modified backticks
    /// (e.g., Option+' on Estonian) and all other Option characters are
    /// handled correctly by SwiftTerm via the normal insertText path, so
    /// the monitor only patches the unmodified case and avoids double-send.
    /// Command is excluded, and non-terminal first responders such as sheet
    /// text fields pass through untouched. Sending to the focused view keeps
    /// this a single monitor for every terminal, with no per-row state to leak.
    private func installBacktickMonitor() {
        guard backtickMonitor == nil else { return }
        backtickMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
            // Intercept only an unmodified backtick. The interpretKeyEvents path
            // swallows a plain backtick (routing to cancelOperation → ESC) before
            // SwiftTerm can insert it. With optionAsMetaKey disabled, Option-modified
            // backticks (e.g., Option+' on Estonian) are handled correctly by SwiftTerm
            // via insertText, so we only need to patch the unmodified case.
            guard !event.modifierFlags.contains(.command),
                  event.modifierFlags.intersection([.option, .control, .shift]) == [],
                  let chars = event.characters,
                  chars.unicodeScalars.contains(where: { $0.value == 0x60 }),
                  let terminal = event.window?.firstResponder as? TerminalView
            else {
                return event
            }
            terminal.send(data: [0x60][...])
            return nil
        }
    }

    private func uninstallBacktickMonitor() {
        if let monitor = backtickMonitor {
            NSEvent.removeMonitor(monitor)
            backtickMonitor = nil
        }
    }
}

/// Window background = terminal background (pure black in Dark Mode,
/// white in Light Mode), so any gap around the terminal reads as
/// terminal instead of window gray. Follows the system appearance:
/// no manual theme override.
enum WindowPaint {
    static func sync() {
        let darkMode = NSApp.effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
        for window in NSApp.windows {
            window.backgroundColor = darkMode ? .black : .white
        }
    }
}
