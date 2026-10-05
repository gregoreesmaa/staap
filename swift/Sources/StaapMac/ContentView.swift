import AppKit
import ShellSupport
import SwiftUI

/// Roster sidebar + session terminal (dumb renderer over the core run
/// registry).
///
/// - Roster: core registry rows grouped Needs input → Working → Idle →
///   History, with an inline sidebar filter field (core-owned text).
/// - Spawn: the sidebar New Session button (or the detail Spawn
///   button) attaches a real roster row in the core via `staap_run_spawn`;
///   Restart/Close act on the selected row via `staap_run_restart` /
///   `staap_run_close`. Typing in the terminal converses through
///   `staap_run_write`.
/// - History: every row shows its project, harness, and last-active age;
///   the rows themselves come from discovery + the persisted store, so
///   they survive relaunches. Ended rows offer restart inline.
/// - Theme: follows the system appearance (no manual override).
/// - Persistence: automatic (throttled pump autosave + quit/close
///   hooks); no manual Save button.
struct ContentView: View {
    @ObservedObject var state: AppState
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        NavigationSplitView {
            VStack(spacing: 0) {
                // Split-button 2D launch: the main action repeats the
                // last launch instantly; the menu opens the full picker
                // (folder × CLI + yolo) or repeats explicitly.
                Menu {
                    Button("Repeat last session", action: state.repeatLastSession)
                    Button("Choose folder, CLI, options…") { state.pickerOpen = true }
                        .keyboardShortcut("n", modifiers: [.command, .shift])
                    Button("Restart selected run", action: state.restartSelected)
                        .keyboardShortcut("r", modifiers: .command)
                    Button("Close selected run", action: state.closeSelected)
                        .keyboardShortcut("w", modifiers: .command)
                } label: {
                    Label("New Session", systemImage: "plus")
                }
                .menuStyle(.button)
                .buttonStyle(.borderedProminent)
                .keyboardShortcut("n", modifiers: .command)
                .help("Repeat the last session, or pick folder × CLI + yolo")
                .padding(8)
                Divider()
                List(selection: Binding(
                    get: { state.selection },
                    set: { id in
                        if let id, let row = state.rows.first(where: { $0.id == id }) {
                            state.select(row: row)
                        }
                    }
                )) {
                    // No sidebar heading above the filter (Windows parity).
                    TextField("Search sessions", text: $state.filter)
                        .textFieldStyle(.roundedBorder)
                    if state.filteredRows.isEmpty {
                        Text("No sessions match.").foregroundStyle(.secondary)
                    }
                    ForEach(statusSections, id: \.status) { section in
                        if !section.rows.isEmpty {
                            Section {
                                ForEach(section.rows) { row in
                                    rowLabel(row)
                                        .tag(row.id)
                                        .badge(state.hasLivePty(row.id) ? "live" : nil)
                                        .contextMenu {
                                            if state.hasLivePty(row.id) {
                                                Button("Close run") {
                                                    state.selection = row.id
                                                    state.closeSelected()
                                                }
                                            } else {
                                                Button("Restart / resume") {
                                                    state.selection = row.id
                                                    state.restartSelected()
                                                }
                                            }
                                        }
                                }
                            } header: {
                                // Core-owned "Working (1)" format (issue
                                // #103): title and count ride one label.
                                Text(sectionHeader(section))
                            }
                        }
                    }
                    if state.hasHistory {
                        DisclosureGroup(
                            isExpanded: Binding(
                                get: { state.historyExpanded },
                                set: { _ in state.toggleHistory() }
                            )
                        ) {
                            ForEach(state.historyRows) { row in
                                rowLabel(row)
                                    .tag(row.id)
                                    .contextMenu {
                                        Button("Restart / resume") {
                                            state.selection = row.id
                                            state.restartSelected()
                                        }
                                    }
                            }
                        } label: {
                            HStack {
                                Text("History (\(state.historyRows.count))")
                                Spacer()
                            }
                        }
                    }
                }
                .listStyle(.sidebar)
                // No sidebar footer: Restart/Close ride the New Session
                // menu, row context menus, keyboard shortcuts, and the
                // detail pane (Windows parity — no headings, no footer).
            }
        } detail: {
            // SwiftUI counts the toolbar height as detail safe area,
            // leaving a toolbar-tall dead gap above the terminal: ignore
            // only the top container inset so the terminal starts at the
            // window edge. Keyboard safe area is untouched.
            detailView.ignoresSafeArea(.container, edges: .top)
        }
        // Repaint the window background when the system appearance
        // changes mid-run.
        .onChange(of: colorScheme) { _, _ in
            WindowPaint.sync()
        }
        .alert("Session error", isPresented: errorPresented) {
            Button("OK", role: .cancel) { state.pendingError = nil }
        } message: {
            Text(state.pendingError ?? "")
        }
        .sheet(isPresented: $state.pickerOpen) {
            NewSessionSheet(state: state, isPresented: $state.pickerOpen)
        }
    }

    // MARK: - Sidebar

    // No sidebar heading and no footer: Restart/Close ride the New
    // Session menu, row context menus, keyboard shortcuts, and the
    // detail pane (Windows parity). Persistence is automatic (no Save
    // button); the shell follows the system appearance (no manual
    // theme override).

    private struct StatusSection {
        var status: Int
        var rows: [SessionRow]
    }

    /// Shared group order (matches the core): Working, then Idle over
    /// attached (live) rows only; needs-input rows share the Idle
    /// section (issue #105). History (no live PTY) renders separately
    /// below, collapsed by default. Headers use the core "Title (n)"
    /// format (issue #103).
    private var statusSections: [StatusSection] {
        [
            StatusSection(
                status: RunStatus.working.rawValue,
                rows: state.rows(with: .working)
            ),
            StatusSection(
                status: RunStatus.idle.rawValue,
                rows: state.rows(with: .attention) + state.rows(with: .idle)
            ),
        ]
    }

    /// Core-owned section header ("Working (1)"), matching
    /// `group_sections` exactly (issues #103/#105).
    private func sectionHeader(_ section: StatusSection) -> String {
        Core.sectionTitle(Int32(section.status)) + " (" + String(section.rows.count) + ")"
    }

    private func rowLabel(_ row: SessionRow) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack {
                // Non-color marker (shared core glyphs) + color dot: rows are
                // never color-only, matching the other shells.
                Text(state.glyph(of: row))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                statusDot(state.status(of: row))
                VStack(alignment: .leading, spacing: 2) {
                    Text(row.title).lineLimit(1)
                    Text("\(row.project) · \(row.harness) · \(state.age(of: row))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
                Spacer(minLength: 8)
                if row.linkCount > 0 {
                    Button {
                        state.toggleLinks(row.id)
                    } label: {
                        Label(
                            "\(row.linkCount) link\(row.linkCount == 1 ? "" : "s")",
                            systemImage: state.expandedLinks.contains(row.id)
                                ? "chevron.down" : "chevron.right"
                        )
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    }
                    .buttonStyle(.plain)
                    .help("Show the parsed links for this run")
                }
            }
            if state.expandedLinks.contains(row.id) {
                linkList(row)
                    .padding(.leading, 20)
            }
        }
        .padding(.vertical, 4)
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityLabel("\(row.title), \(state.statusName(of: row))")
    }

    private func statusDot(_ status: RunStatus) -> some View {
        let color: Color = switch status {
        case .attention: .red
        case .working: .green
        case .idle: .gray
        }
        return Circle().fill(color).frame(width: 8, height: 8)
            .accessibilityHidden(true)
    }

    /// Inline parsed-link list for an expanded row, with copy/open
    /// actions per link (issue #103).
    private func linkList(_ row: SessionRow) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            ForEach(allLinks(row), id: \.self) { link in
                Text(link)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .contextMenu {
                        Button("Copy link") {
                            NSPasteboard.general.clearContents()
                            NSPasteboard.general.setString(link, forType: .string)
                        }
                        if let url = URL(string: link), url.scheme != nil {
                            Button("Open link") {
                                NSWorkspace.shared.open(url)
                            }
                        }
                    }
            }
        }
    }

    private func allLinks(_ row: SessionRow) -> [String] {
        (row.prLinks ?? []) + (row.relatedLinks ?? [])
    }

    // MARK: - Detail

    @ViewBuilder
    private var detailView: some View {
        if let id = state.selection,
           let row = state.rows.first(where: { $0.id == id })
        {
            if state.hasLivePty(id) {
                VStack(spacing: 0) {
                    // Deadzone drag strip: the hidden title bar leaves
                    // no grab area, and the terminal eats mouse events
                    // for selection — this strip moves the window
                    // instead (issue #101).
                    WindowDragStrip().frame(height: 14)
                    CoreTerminalView(
                        state: state, rowId: id,
                        darkMode: colorScheme == .dark
                    )
                    // Fresh view per selected row: without this the
                    // representable reuses one TerminalView (and its
                    // row-bound coordinator), so switching rows keeps
                    // showing the previous session (issue #106).
                    .id(id)
                }
            } else {
                // Ended run or historic entry: restart/resume on the same
                // id (keeps title and links), or close it. Same recovery
                // rule as the other shells.
                VStack(spacing: 12) {
                    Text(row.title).font(.title2)
                    Text("\(row.project) · \(row.harness)")
                        .foregroundStyle(.secondary)
                    HStack(spacing: 12) {
                        Button("Restart / resume", action: state.restartSelected)
                            .keyboardShortcut(.defaultAction)
                            .buttonStyle(.borderedProminent)
                        Button("Close run", action: state.closeSelected)
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        } else if state.rows.isEmpty {
            VStack(spacing: 12) {
                Text("No sessions yet").font(.title2)
                Text("Spawned sessions appear here; history is restored on launch.")
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            Text("Select a session").foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private var errorPresented: Binding<Bool> {
        Binding(
            get: { state.pendingError != nil },
            set: { if !$0 { state.pendingError = nil } }
        )
    }
}

/// Deadzone drag strip above the terminal (issue #101): a clear strip
/// that moves the window, so the hidden title bar still leaves a grab
/// area above the full-height terminal.
private struct WindowDragStrip: NSViewRepresentable {
    func makeNSView(context _: Context) -> NSView {
        DragView()
    }

    func updateNSView(_: NSView, context _: Context) {}

    private final class DragView: NSView {
        override var mouseDownCanMoveWindow: Bool { true }
    }
}

/// 2D new-session sheet: working folder × agent CLI + one-shot yolo.
///
/// - Folder: Choose… (directory panel) or a text field (blank =
///   inherit) plus the persisted recents for one-click refill. A
///   missing folder refuses inline — the sheet never spawns into nothing.
/// - CLI: a picker over the autodetected catalog; missing CLIs render
///   disabled with an install hint, never hidden.
/// - Yolo: a tri-state toggle (labeled default on/off + on/off once),
///   safe by default; the footer previews the exact combination.
private struct NewSessionSheet: View {
    @ObservedObject var state: AppState
    @Binding var isPresented: Bool

    @State private var clis: [CliRow] = []
    @State private var cliId = "muse"
    @State private var folder = ""
    @State private var yolo = NewSessionPicker.YoloChoice.useDefault
    @State private var folderError: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Start a new run").font(.title2)
            // Folder axis.
            Text("Where should it work?").font(.headline)
            HStack {
                TextField("Blank = current folder", text: $folder)
                    .textFieldStyle(.roundedBorder)
                Button("Choose…") { chooseFolder() }
            }
            if !recents.isEmpty {
                Picker("Recent", selection: $folder) {
                    Text("Type a folder…").tag("")
                    ForEach(recents, id: \.self) { recent in
                        Text(recent).tag(recent)
                    }
                }
                .pickerStyle(.menu)
            }
            if let folderError {
                Text(folderError).foregroundStyle(.red).font(.caption)
            }
            // CLI axis.
            Text("Who should do it?").font(.headline)
            Picker("Agent CLI", selection: $cliId) {
                ForEach(clis, id: \.id) { cli in
                    Label {
                        VStack(alignment: .leading) {
                            Text(cli.id)
                            Text(cliDetail(cli))
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                    } icon: {
                        Image(systemName: cli.available ? "checkmark.circle.fill" : "xmark.circle")
                            .foregroundStyle(cli.available ? .green : .secondary)
                    }
                    .tag(cli.id)
                    .disabled(!cli.available)
                }
            }
            .pickerStyle(.radioGroup)
            // Yolo tri-state (safe default; per-run only).
            Text("Permission mode").font(.headline)
            Picker("Yolo", selection: $yolo) {
                Text(state.yoloDefaultLabel(cli: cliId)).tag(NewSessionPicker.YoloChoice.useDefault)
                Text("On (once)").tag(NewSessionPicker.YoloChoice.forceOn)
                Text("Off (once)").tag(NewSessionPicker.YoloChoice.forceOff)
            }
            .pickerStyle(.segmented)
            Text("Default follows the per-agent config; On/Off apply once.")
                .font(.caption)
                .foregroundStyle(.secondary)
            Text(previewText)
                .font(.caption)
                .foregroundStyle(.secondary)
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { isPresented = false }
                Button("Spawn") { spawn() }
                    .keyboardShortcut(.defaultAction)
                    .buttonStyle(.borderedProminent)
                    .disabled(clis.isEmpty)
            }
        }
        .padding(20)
        .frame(minWidth: 360)
        .onAppear {
            clis = state.pickerCatalog()
            // Preselect the first available CLI (catalog order); a
            // configured-but-missing default stays listed but never
            // preselected — Spawn would fail for certain.
            if let firstUp = clis.first(where: { $0.available }) {
                if !clis.contains(where: { $0.id == cliId && $0.available }) {
                    cliId = firstUp.id
                }
            } else {
                cliId = clis.first?.id ?? "muse"
            }
            folder = state.pickerRecents().first ?? ""
        }
    }

    /// Directory picker for the folder axis (issue #102): choosing in
    /// the UI beats typing a path, and keeps first-run launches near
    /// the two-click goal.
    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        if panel.runModal() == .OK, let path = panel.url?.path {
            folder = path
        }
    }

    private var recents: [String] { state.pickerRecents() }

    /// Detail caption for a CLI row: install path when ready, install
    /// hint when missing (issue #102).
    private func cliDetail(_ cli: CliRow) -> String {
        if cli.available {
            return cli.path ?? cli.program
        }
        return "not installed"
    }

    /// One-line spawn preview through the shared core helper (single
    /// copy of the preview rule on every shell).
    private var previewText: String {
        let yoloArg: Int32
        switch yolo {
        case .useDefault: yoloArg = 0
        case .forceOn: yoloArg = 1
        case .forceOff: yoloArg = -1
        }
        return Core.spawnPreview(cli: cliId, folder: folder, yolo: yoloArg)
    }

    private func spawn() {
        let trimmed = folder.trimmingCharacters(in: .whitespaces)
        let folderOrNil = trimmed.isEmpty ? nil : trimmed
        if let dir = folderOrNil {
            var isDir: ObjCBool = false
            let exists = FileManager.default.fileExists(
                atPath: dir, isDirectory: &isDir)
            if !exists || !isDir.boolValue {
                folderError = "No such folder: \(dir)"
                return
            }
        }
        let yoloArg: Int32
        switch yolo {
        case .useDefault: yoloArg = 0
        case .forceOn: yoloArg = 1
        case .forceOff: yoloArg = -1
        }
        state.confirmPicker(cli: cliId, folder: folderOrNil, yolo: yoloArg)
        isPresented = false
    }
}
