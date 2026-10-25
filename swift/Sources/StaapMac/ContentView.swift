import AppKit
import ShellSupport
import SwiftUI

/// Roster sidebar + session terminal (dumb renderer over the core run
/// registry).
///
/// - Roster: core registry rows grouped Needs input → Working → Idle →
///   History, with an inline sidebar filter field (core-owned text).
/// - Spawn: the single sidebar New Session button shows the in-place
///   new-session screen on the right side every press (folder × CLI +
///   yolo, with the machine dropdown and remote folder browser); Start
///   attaches a real roster row in the core via `staap_run_spawn`.
///   Restart/Close act on the selected row via `staap_run_restart` /
///   `staap_run_close` (row context menus, keyboard shortcuts, and the
///   detail pane — never the New Session button). Typing in the
///   terminal converses through `staap_run_write`.
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
                // Single New Session button: every press shows the
                // in-place new-session screen on the right side (folder
                // × CLI + yolo, machine dropdown, remote folder
                // browser). Restart/Close deliberately ride only the row
                // context menus, keyboard shortcuts, and the detail
                // pane — never this button.
                Button { state.pickerOpen = true } label: {
                    Label("New Session", systemImage: "plus")
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut("n", modifiers: .command)
                .help("Pick folder × CLI + yolo on the right side")
                .padding(8)
                Divider()
                // Filter sits above the list, not inside it: as a List
                // row it picked up row styling and lost focus on every
                // roster reload (Windows parity: no sidebar heading).
                TextField("Search sessions", text: $state.filter)
                    .textFieldStyle(.roundedBorder)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 6)
                Divider()
                List(selection: Binding(
                    get: { state.selection },
                    set: { id in
                        if let id, let row = state.rows.first(where: { $0.id == id }) {
                            state.select(row: row)
                        }
                    }
                )) {
                    if state.filteredRows.isEmpty {
                        Text("No sessions match.").foregroundStyle(.secondary)
                    }
                    ForEach(statusSections, id: \.status) { section in
                        if !section.rows.isEmpty {
                            Section {
                                ForEach(section.rows) { row in
                                    rowLabel(row)
                                        .tag(row.id)
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
                            // Hundreds of history rows: build them only
                            // while expanded (hidden rows still diff).
                            if state.historyExpanded {
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
                // No row animations: the roster re-sorts live and a torn
                // insert/delete crossfade paints two rows as one.
                .transaction { $0.animation = nil }
                // Fresh List on membership/order changes: cell reuse can
                // leave a stale cell painting the previous row behind the
                // new one (overprinted titles, frozen while live rows
                // move). Recreating the List exactly when the visible row
                // sequence changes drops every reused cell, so a ghost can
                // never stick; in-place updates keep the instance and the
                // scroll position.
                .id(rosterSequenceID)
                // No sidebar footer: Restart/Close ride the row context
                // menus, keyboard shortcuts, and the detail pane
                // (Windows parity — no headings, no footer).
            }
        } detail: {
            // The in-place new-session screen takes the whole right
            // side every time New Session is pressed (no sheet/modal).
            // Otherwise the terminal owns the detail: SwiftUI counts
            // the toolbar height as detail safe area, leaving a
            // toolbar-tall dead gap above the terminal, so ignore only
            // the top container inset and the terminal starts at the
            // window edge. Keyboard safe area is untouched.
            if state.pickerOpen {
                NewSessionScreen(state: state, isPresented: $state.pickerOpen)
            } else {
                detailView.ignoresSafeArea(.container, edges: .top)
            }
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
    }

    // MARK: - Sidebar

    // No sidebar heading and no footer: Restart/Close ride the row
    // context menus, keyboard shortcuts, and the detail pane (Windows
    // parity). Persistence is automatic (no Save button); the shell
    // follows the system appearance (no manual theme override).

    private struct StatusSection {
        var status: Int
        var rows: [SessionRow]
    }

    /// Visible row sequence: membership plus order, for the List’s
    /// structural identity. Sections and the (maybe collapsed) history
    /// read the same filtered rows the List renders, so the key moves
    /// exactly when cells would have to move.
    private var rosterSequenceID: String {
        rosterSequenceKey(
            active: statusSections.flatMap { $0.rows.map(\.id) },
            history: state.historyExpanded
                ? state.historyRows.map(\.id) : nil)
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
        // Stock sidebar row height: custom vertical padding and
        // full-width frames desync the row background from its
        // content (ghost pills, overprinted neighbors).
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

/// 2D new-session screen: working folder × agent CLI + one-shot yolo,
/// shown in place on the right side (no sheet/modal).
///
/// - Machine: a dropdown (This machine, saved hosts, Add remote
///   machine…) over the core saved-hosts. The Add row opens the SSH add
///   sheet (display name, `user@hostname`-or-alias host, default
///   folder, Test + Save/Cancel). A picked remote host runs the CLI on
///   that host over `ssh -t` and probes the remote catalog off the main
///   thread, so picking never blocks on the network.
/// - Folder: Choose… (directory panel) or a text field (blank =
///   inherit) plus the persisted recents for one-click refill locally;
///   remotely a text field plus the core-driven folder browser (remote
///   listing from the host default folder, stale arrivals dropped). A
///   missing local folder refuses inline — the screen never spawns into
///   nothing.
/// - CLI: a picker over the autodetected catalog; missing CLIs render
///   disabled with an install hint, never hidden.
/// - Yolo: a tri-state toggle (labeled default on/off + on/off once),
///   safe by default; the footer previews the exact combination.
private struct NewSessionScreen: View {
    @ObservedObject var state: AppState
    @Binding var isPresented: Bool

    @State private var clis: [CliRow] = []
    @State private var cliId = "muse"
    @State private var folder = ""
    @State private var yolo = NewSessionPicker.YoloChoice.useDefault
    @State private var folderError: String?
    @State private var machine = ""
    @State private var previousMachine = ""
    @State private var savedHosts: [SavedHost] = []
    @State private var unsavedLastHost = ""
    @State private var showAddSheet = false
    @State private var remoteClis: [CliRow]? = nil
    @State private var probingRemote = false
    @State private var probeToken = 0
    @State private var remotePath = "~"
    @State private var remoteEntries: [RemoteEntry] = []
    @State private var listingRemote = false
    @State private var listToken = 0

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                Text("Start a new run").font(.title2)
                // Machine axis: dropdown over the core saved-hosts (This
                // machine, saved hosts with display names, Add remote
                // machine…). The hint under it always shows the
                // `user@hostname` shape new hosts must take.
                Text("Which machine?").font(.headline)
                Picker("Machine", selection: $machine) {
                    Text("This machine").tag("")
                    ForEach(savedHosts, id: \.host) { saved in
                        Text(saved.displayLabel).tag(saved.host)
                    }
                    if !unsavedLastHost.isEmpty,
                       !savedHosts.contains(where: { $0.host == unsavedLastHost })
                    {
                        Text(unsavedLastHost).tag(unsavedLastHost)
                    }
                    Text("Add remote machine…").tag(NewSessionPicker.addMachineTag)
                }
                .pickerStyle(.menu)
                .onChange(of: machine) { _, _ in machineChanged() }
                Text(NewSessionPicker.machinePlaceholder)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                if isRemote {
                    Text(remoteStatusLine)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                // Folder axis: local directory panel + recents, or the
                // remote browser when a host is picked.
                if isRemote {
                    remoteFolderSection
                } else {
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
                }
                if let folderError {
                    Text(folderError).foregroundStyle(.red).font(.caption)
                }
            // CLI axis.
            Text("Who should do it?").font(.headline)
            Picker("Agent CLI", selection: $cliId) {
                ForEach(shownClis, id: \.id) { cli in
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
                Button("Start") { spawn() }
                    .keyboardShortcut(.defaultAction)
                    .buttonStyle(.borderedProminent)
                    .disabled(shownClis.isEmpty)
            }
            } // VStack (the ScrollView closes below; the screen scrolls)
            .padding(20)
            .frame(maxWidth: 560)
        }
        .onAppear {
            clis = state.pickerCatalog()
            savedHosts = state.pickerSavedHosts()
            // Preselect the last-used host (saved or not — an unsaved
            // last host renders as its own row until one is picked), or
            // This machine when nothing was ever used remotely.
            let last = state.lastRemoteHost.trimmingCharacters(in: .whitespaces)
            if !last.isEmpty {
                machine = last
                if !savedHosts.contains(where: { $0.host == last }) {
                    unsavedLastHost = last
                }
            }
            previousMachine = machine
            // Preselect the first available CLI (catalog order); a
            // configured-but-missing default stays listed but never
            // preselected — Start would fail for certain.
            if let firstUp = clis.first(where: { $0.available }) {
                if !clis.contains(where: { $0.id == cliId && $0.available }) {
                    cliId = firstUp.id
                }
            } else {
                cliId = clis.first?.id ?? "muse"
            }
            folder = state.pickerRecents().first ?? ""
            if effectiveHost != nil {
                applyRemoteDefaults()
                startRemoteProbe()
                startRemoteList()
            }
        }
        .sheet(isPresented: $showAddSheet, onDismiss: {
            // Dismissing the add sheet without saving returns the
            // dropdown to the previously picked machine.
            if machine == NewSessionPicker.addMachineTag {
                machine = previousMachine
            }
        }) {
            SshAddSheet(state: state, isPresented: $showAddSheet) { savedHost in
                savedHosts = state.pickerSavedHosts()
                unsavedLastHost = ""
                machine = savedHost
                previousMachine = savedHost
                hostChanged()
            }
        }
    }

    /// Remote folder browser: the working folder text plus the
    /// core-driven listing of the browsed remote path (directories
    /// navigate, files are inert). The listing starts at the host
    /// default folder (saved default, then host MRU, then home) and
    /// refreshes off the main thread with stale-arrival drops.
    private var remoteFolderSection: some View {
        Group {
            Text("Where should it work?").font(.headline)
            HStack {
                TextField("Remote folder (blank = home)", text: $folder)
                    .textFieldStyle(.roundedBorder)
                Button("Go") {
                    let trimmed = folder.trimmingCharacters(in: .whitespaces)
                    remotePath = trimmed.isEmpty ? "~" : trimmed
                    startRemoteList()
                }
                .help("List this remote folder below")
            }
            if !recents.isEmpty {
                Picker("Recent", selection: $folder) {
                    Text("Type a folder…").tag("")
                    ForEach(recents, id: \.self) { recent in
                        Text(recent).tag(recent)
                    }
                }
                .pickerStyle(.menu)
                .onChange(of: folder) { _, newValue in
                    let trimmed = newValue.trimmingCharacters(in: .whitespaces)
                    if !trimmed.isEmpty {
                        remotePath = trimmed
                        startRemoteList()
                    }
                }
            }
            HStack {
                Text(remotePath)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer()
                if NewSessionPicker.remoteParent(of: remotePath) != nil {
                    Button("Up") {
                        if let parent = NewSessionPicker.remoteParent(of: remotePath) {
                            remotePath = parent
                            startRemoteList()
                        }
                    }
                }
                Button("Refresh") { startRemoteList() }
            }
            if listingRemote {
                Text("Listing remote folder over ssh…")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else if remoteEntries.isEmpty {
                Text("Empty or unreachable folder.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                ForEach(remoteEntries, id: \.name) { entry in
                    if entry.isDir {
                        Button {
                            remotePath = NewSessionPicker.remoteJoin(
                                base: remotePath, name: entry.name)
                            startRemoteList()
                        } label: {
                            Label(entry.name, systemImage: "folder")
                        }
                        .buttonStyle(.plain)
                    } else {
                        // Files are inert: shown, never entered.
                        Text(entry.name)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                Button("Use this folder") { folder = remotePath }
            }
        }
    }

    /// Machine picked: the Add row opens the SSH add sheet, anything
    /// else drops the stale remote state and probes the new host off
    /// the main thread. Arrivals carry a token, so a stale machine's
    /// late arrival never overwrites the current one.
    private func machineChanged() {
        if machine == NewSessionPicker.addMachineTag {
            showAddSheet = true
            return
        }
        previousMachine = machine
        hostChanged()
    }

    /// Host changed: drop the stale remote catalog and listing, apply
    /// the host default folder, and probe the new host off the main
    /// thread (`staap_remote_clis_json` blocks on one bounded `ssh`
    /// round trip).
    private func hostChanged() {
        probeToken += 1
        listToken += 1
        if effectiveHost == nil {
            remoteClis = nil
            probingRemote = false
            remoteEntries = []
            listingRemote = false
            return
        }
        applyRemoteDefaults()
        startRemoteProbe()
        startRemoteList()
    }

    /// Blank folder inherits the host default (saved default, then host
    /// MRU, then home): the browser lists "~" while the folder stays
    /// blank-to-home. A typed folder is never clobbered.
    private func applyRemoteDefaults() {
        guard let remote = effectiveHost else { return }
        if folder.trimmingCharacters(in: .whitespaces).isEmpty {
            let recents = state.pickerRemoteRecents(host: remote)
            let savedDefault = savedHosts.first(where: { $0.host == remote })?.defaultFolder
                ?? state.pickerRemoteDefault(host: remote)
            if let resolved = NewSessionPicker.resolveRemoteDefault(
                savedDefault: savedDefault, recents: recents)
            {
                folder = resolved
            }
        }
        let trimmed = folder.trimmingCharacters(in: .whitespaces)
        remotePath = trimmed.isEmpty ? "~" : trimmed
    }

    private func startRemoteProbe() {
        guard let remote = effectiveHost else { return }
        probeToken += 1
        let token = probeToken
        probingRemote = true
        remoteClis = nil
        DispatchQueue.global(qos: .userInitiated).async {
            let catalog = Core.remoteCliCatalog(host: remote)
            DispatchQueue.main.async {
                guard token == probeToken else { return }
                probingRemote = false
                remoteClis = catalog
                if let firstUp = catalog.first(where: { $0.available }) {
                    if !catalog.contains(where: { $0.id == cliId && $0.available }) {
                        cliId = firstUp.id
                    }
                } else if let first = catalog.first {
                    cliId = first.id
                }
            }
        }
    }

    /// List the browsed remote path off the main thread
    /// (`staap_remote_list_json` blocks on one bounded `ssh` round
    /// trip). Arrivals carry a token plus their host/path, so a stale
    /// listing never overwrites the current one.
    private func startRemoteList() {
        guard let remote = effectiveHost else { return }
        let path = remotePath.trimmingCharacters(in: .whitespaces)
        guard !path.isEmpty else {
            remoteEntries = []
            listingRemote = false
            return
        }
        listToken += 1
        let token = listToken
        listingRemote = true
        DispatchQueue.global(qos: .userInitiated).async {
            let entries = Core.remoteList(host: remote, path: path)
            DispatchQueue.main.async {
                guard token == listToken,
                      effectiveHost == remote,
                      remotePath.trimmingCharacters(in: .whitespaces) == path
                else { return }
                listingRemote = false
                remoteEntries = entries
            }
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

    private var recents: [String] {
        if let remote = effectiveHost {
            return state.pickerRemoteRecents(host: remote)
        }
        return state.pickerRecents()
    }

    /// Dropdown machine backing the Add row: while the add sheet is
    /// open the tag sits on the sentinel, so fall back to the previous
    /// pick until the sheet saves or dismisses.
    private var effectiveMachine: String {
        machine == NewSessionPicker.addMachineTag ? previousMachine : machine
    }

    /// Trimmed SSH host, or nil for a local launch on this Mac.
    private var effectiveHost: String? {
        let trimmed = effectiveMachine.trimmingCharacters(in: .whitespaces)
        return trimmed.isEmpty ? nil : trimmed
    }

    private var isRemote: Bool { effectiveHost != nil }

    /// CLI catalog the picker renders: the probed remote catalog while
    /// a host is set, else the local autodetected one.
    private var shownClis: [CliRow] {
        effectiveHost != nil ? (remoteClis ?? []) : clis
    }

    /// Status line under the machine dropdown: probing state or availability.
    private var remoteStatusLine: String {
        if probingRemote { return "Probing remote CLIs over ssh…" }
        let n = (remoteClis ?? []).filter { $0.available }.count
        if n == 0 { return "No remote CLIs detected on this host." }
        return "\(n) remote CLI\(n == 1 ? "" : "s") available."
    }

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
        let local = Core.spawnPreview(cli: cliId, folder: folder, yolo: yoloArg)
        if let remote = effectiveHost {
            return "ssh \(remote): \(local)"
        }
        return local
    }

    private func spawn() {
        let trimmed = folder.trimmingCharacters(in: .whitespaces)
        let folderOrNil = trimmed.isEmpty ? nil : trimmed
        if effectiveHost == nil, let dir = folderOrNil {
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
        if let remote = effectiveHost {
            state.confirmRemotePicker(host: remote, remoteCwd: folderOrNil, cli: cliId, yolo: yoloArg)
        } else {
            state.confirmPicker(cli: cliId, folder: folderOrNil, yolo: yoloArg)
        }
        isPresented = false
    }
}

/// SSH add sheet for the machine dropdown's Add row: display name,
/// `user@hostname`-or-alias host, and default folder, backed by the
/// core saved-hosts. Test dials the core tester off the main thread
/// (reachable/unreachable plus the detected remote CLIs, stale arrivals
/// dropped); Save persists through the core and hands the host back so
/// the dropdown picks it.
private struct SshAddSheet: View {
    @ObservedObject var state: AppState
    @Binding var isPresented: Bool
    var onSaved: (String) -> Void

    @State private var displayName = ""
    @State private var host = ""
    @State private var defaultFolder = ""
    @State private var testStatus: String?
    @State private var testing = false
    @State private var testToken = 0
    @State private var saveError: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Add remote machine").font(.title2)
            Text("Display name").font(.headline)
            TextField("e.g. GPU box", text: $displayName)
                .textFieldStyle(.roundedBorder)
            Text("Host").font(.headline)
            TextField("user@hostname or alias", text: $host, prompt: Text(NewSessionPicker.machinePlaceholder))
                .textFieldStyle(.roundedBorder)
            Text("Default folder").font(.headline)
            TextField("Remote folder (blank = home)", text: $defaultFolder)
                .textFieldStyle(.roundedBorder)
            if testing {
                Text("Testing over ssh…")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else if let testStatus {
                Text(testStatus)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            if let saveError {
                Text(saveError).foregroundStyle(.red).font(.caption)
            }
            HStack {
                Spacer()
                Button("Test") { runTest() }
                    .disabled(testHost == nil || testing)
                    .help("Probe this host once (reachable + remote CLIs)")
                Button("Cancel", role: .cancel) { isPresented = false }
                Button("Save") { save() }
                    .keyboardShortcut(.defaultAction)
                    .buttonStyle(.borderedProminent)
                    .disabled(!canSave)
            }
        }
        .padding(20)
        .frame(minWidth: 360)
    }

    private var testHost: String? {
        let trimmed = host.trimmingCharacters(in: .whitespaces)
        return trimmed.isEmpty ? nil : trimmed
    }

    private var canSave: Bool {
        !displayName.trimmingCharacters(in: .whitespaces).isEmpty && testHost != nil
    }

    /// Probe the host once off the main thread (`staap_remote_test_json`
    /// blocks on one bounded non-interactive `ssh` round trip — never on
    /// every keystroke). Arrivals carry a token, so a stale host's late
    /// arrival never overwrites the current one.
    private func runTest() {
        guard let remote = testHost else {
            testStatus = "Enter a host first (\(NewSessionPicker.machinePlaceholder))."
            return
        }
        testToken += 1
        let token = testToken
        testing = true
        testStatus = nil
        saveError = nil
        DispatchQueue.global(qos: .userInitiated).async {
            let result = Core.remoteTest(host: remote)
            DispatchQueue.main.async {
                guard token == testToken else { return }
                testing = false
                if result.reachable {
                    let available = result.clis.filter { $0.available }
                    if available.isEmpty {
                        testStatus = "Reachable — no agent CLIs detected on this host."
                    } else {
                        let names = available.map { $0.id }.joined(separator: ", ")
                        testStatus = "Reachable — \(names) available."
                    }
                } else {
                    testStatus = "Unreachable — check the host and SSH access."
                }
            }
        }
    }

    private func save() {
        let name = displayName.trimmingCharacters(in: .whitespaces)
        guard let remote = testHost, !name.isEmpty else {
            saveError = "Display name and host are both required."
            return
        }
        let trimmedFolder = defaultFolder.trimmingCharacters(in: .whitespaces)
        do {
            try state.addSavedHost(
                name: name, host: remote,
                defaultFolder: trimmedFolder.isEmpty ? nil : trimmedFolder)
            onSaved(remote)
            isPresented = false
        } catch {
            saveError = error.localizedDescription
        }
    }
}
