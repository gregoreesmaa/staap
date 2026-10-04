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
                                HStack {
                                    Text(section.title)
                                    Spacer()
                                    Text("\(section.rows.count)")
                                        .foregroundStyle(.secondary)
                                }
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
        var title: String
        var rows: [SessionRow]
    }

    /// Shared group order (matches the other shells): Needs input →
    /// Working → Idle over attached (live) rows only. History (no live
    /// PTY) renders separately below, collapsed by default.
    /// Counts ride the header HStack (title + count), not the title.
    private var statusSections: [StatusSection] {
        [
            StatusSection(
                status: RunStatus.attention.rawValue, title: "Needs input",
                rows: state.rows(with: .attention)
            ),
            StatusSection(
                status: RunStatus.working.rawValue, title: "Working",
                rows: state.rows(with: .working)
            ),
            StatusSection(
                status: RunStatus.idle.rawValue, title: "Idle",
                rows: state.rows(with: .idle)
            ),
        ]
    }

    private func rowLabel(_ row: SessionRow) -> some View {
        HStack {
            // Non-color marker (shared core glyphs) + color dot: rows are
            // never color-only, matching the other shells.
            Text(state.glyph(of: row))
                .font(.caption)
                .foregroundStyle(.secondary)
            statusDot(state.status(of: row))
            VStack(alignment: .leading) {
                Text(row.title).lineLimit(1)
                Text("\(row.project) · \(row.harness) · \(state.age(of: row))")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer()
            if row.linkCount > 0 {
                Text("\(row.linkCount) link\(row.linkCount == 1 ? "" : "s")")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
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

    // MARK: - Detail

    @ViewBuilder
    private var detailView: some View {
        if let id = state.selection,
           let row = state.rows.first(where: { $0.id == id })
        {
            if state.hasLivePty(id) {
                CoreTerminalView(
                    state: state, rowId: id,
                    darkMode: colorScheme == .dark
                )
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

/// 2D new-session sheet: working folder × agent CLI + one-shot yolo.
///
/// - Folder: a text field (blank = inherit) plus the persisted recents
///   for one-click refill. A missing folder refuses inline and stays
///   open for a fix — the sheet never spawns into nothing.
/// - CLI: a picker over the autodetected catalog; missing CLIs render
///   disabled with an install hint, never hidden.
/// - Yolo: a tri-state toggle (default / on once / off once), safe by
///   default; the footer previews the exact combination before Spawn.
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
            TextField("Blank = current folder", text: $folder)
                .textFieldStyle(.roundedBorder)
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
                    Text(cliLabel(cli)).tag(cli.id)
                        .disabled(!cli.available)
                }
            }
            .pickerStyle(.radioGroup)
            // Yolo tri-state (safe default; per-run only).
            Text("Permission mode").font(.headline)
            Picker("Yolo", selection: $yolo) {
                Text("Default").tag(NewSessionPicker.YoloChoice.useDefault)
                Text("On (once)").tag(NewSessionPicker.YoloChoice.forceOn)
                Text("Off (once)").tag(NewSessionPicker.YoloChoice.forceOff)
            }
            .pickerStyle(.segmented)
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

    private var recents: [String] { state.pickerRecents() }

    private func cliLabel(_ cli: CliRow) -> String {
        cli.available
            ? "\(cli.id) — ready"
            : "\(cli.id) — not installed"
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
