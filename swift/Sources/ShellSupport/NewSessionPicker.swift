/// Two-dimensional new-session picker model (folder × CLI + yolo).
///
/// Pure Swift with no C ABI references, so it unit-tests without linking
/// the core staticlib. The preview/yolo/folder rules now live in the
/// core (`staap_spawn_preview`, `staap_yolo_value`); this type keeps the widget
/// state the sheet renders (CLI cursor, folder text, recents, yolo
/// choice) and delegates the rules to `Core` where possible.
///
/// Split-button contract (see `docs/new-session-picker.md`): the main
/// action repeats the last launch instantly (null CLI/folder through
/// `staap_run_spawn`, which resolves the core's effective default); the
/// picker confirms an explicit folder × CLI + yolo combination.
public struct NewSessionPicker {
    /// CLI catalog rows in core order (muse, claude, opencode, codex).
    /// Rows are always listed; `available == false` renders disabled
    /// with an install hint, never hidden.
    public struct CliRow: Equatable {
        public var id: String
        public var program: String
        public var path: String?
        public var available: Bool

        public init(id: String, program: String, path: String?, available: Bool) {
            self.id = id
            self.program = program
            self.path = path
            self.available = available
        }
    }

    /// Per-run yolo choice: safe by default, explicit per run, never
    /// auto-written back to the config.
    public enum YoloChoice: Hashable {
        case useDefault
        case forceOn
        case forceOff

        /// Cycle for the picker toggle: default → on → off → default.
        public mutating func cycle() {
            switch self {
            case .useDefault: self = .forceOn
            case .forceOn: self = .forceOff
            case .forceOff: self = .useDefault
            }
        }

        /// Short label for the picker row (text, never color-only).
        public var label: String {
            switch self {
            case .useDefault: "Default"
            case .forceOn: "On (once)"
            case .forceOff: "Off (once)"
            }
        }
    }

    /// Placeholder hint for the machine axis (dropdown caption and the
    /// add-sheet host field): always shows a `user@hostname` example so
    /// the expected shape is visible before typing.
    public static let machinePlaceholder = "e.g. you@gpu-box — a user@hostname or ssh-config alias"

    /// Sentinel dropdown tag for the "Add remote machine…" row: never a
    /// real host (hosts never contain spaces).
    public static let addMachineTag = " Add remote machine… "

    /// One saved SSH host for the machine dropdown (mirrors the core's
    /// `{name,host,default_folder}` row; no secret fields exist).
    public struct SavedHostRow: Equatable {
        public var name: String
        public var host: String
        public var defaultFolder: String?

        public init(name: String, host: String, defaultFolder: String? = nil) {
            self.name = name
            self.host = host
            self.defaultFolder = defaultFolder
        }

        /// Dropdown label: display name with the SSH target, or the
        /// bare target when unnamed.
        public var displayLabel: String {
            name.isEmpty ? host : "\(name) (\(host))"
        }
    }

    /// One remote directory entry for the folder browser (mirrors the
    /// core's `{name,is_dir}` row). Directories navigate; files are
    /// inert (shown, never entered).
    public struct RemoteEntryRow: Equatable {
        public var name: String
        public var isDir: Bool

        public init(name: String, isDir: Bool) {
            self.name = name
            self.isDir = isDir
        }
    }

    /// Ordered machine-dropdown tags: "" (This machine), then the saved
    /// hosts in order, then the Add sentinel. An unsaved `lastHost`
    /// (the shell's last-used memory) is inserted before the sentinel
    /// so the previous host stays selectable until one is picked.
    public static func machineTags(savedHosts: [SavedHostRow], lastHost: String) -> [String] {
        var tags = [""]
        for saved in savedHosts where !saved.host.isEmpty && !tags.contains(saved.host) {
            tags.append(saved.host)
        }
        let trimmed = lastHost.trimmingCharacters(in: .whitespaces)
        if !trimmed.isEmpty && !tags.contains(trimmed) && trimmed != addMachineTag {
            tags.append(trimmed)
        }
        tags.append(addMachineTag)
        return tags
    }

    /// Dropdown label for one machine tag (display names for saved
    /// hosts, plain targets otherwise).
    public static func machineLabel(tag: String, savedHosts: [SavedHostRow]) -> String {
        if tag.isEmpty { return "This machine" }
        if tag == addMachineTag { return "Add remote machine…" }
        if let saved = savedHosts.first(where: { $0.host == tag }) {
            return saved.displayLabel
        }
        return tag
    }

    /// Starting remote folder for a host: the saved per-host default,
    /// else the host MRU head, else nil (the remote login directory).
    /// Never synthesizes a path: nil tells the shell to list "~" and
    /// leave the folder blank-to-home.
    public static func resolveRemoteDefault(savedDefault: String?, recents: [String]) -> String? {
        let def = (savedDefault ?? "").trimmingCharacters(in: .whitespaces)
        if !def.isEmpty { return def }
        for recent in recents {
            let trimmed = recent.trimmingCharacters(in: .whitespaces)
            if !trimmed.isEmpty { return trimmed }
        }
        return nil
    }

    /// Parent of a remote path ("~/a/b" -> "~/a", "~/a" -> "~",
    /// "/a" -> "/"). Nil means already at the top ("~", "/", a bare
    /// name, or blank): the Up button hides there.
    public static func remoteParent(of path: String) -> String? {
        var current = path.trimmingCharacters(in: .whitespaces)
        while current.hasSuffix("/") && current.count > 1 { current.removeLast() }
        guard !current.isEmpty, current != "~", current != "/" else { return nil }
        guard let slash = current.lastIndex(of: "/") else { return nil }
        let parent = String(current[..<slash])
        return parent.isEmpty ? "/" : parent
    }

    /// Join a browser entry onto its directory ("~/a" + "b" -> "~/a/b").
    public static func remoteJoin(base: String, name: String) -> String {
        var cleaned = base.trimmingCharacters(in: .whitespaces)
        while cleaned.hasSuffix("/") && cleaned.count > 1 { cleaned.removeLast() }
        if cleaned.isEmpty { return name }
        return cleaned + "/" + name
    }

    public var clis: [CliRow]
    public var cliIndex: Int
    public var folder: String
    public var recents: [String]
    public var host: String
    public var yolo: YoloChoice = .useDefault

    public init(clis: [CliRow], cliIndex: Int, folder: String, recents: [String], host: String = "") {
        self.clis = clis
        self.cliIndex = cliIndex
        self.folder = folder
        self.recents = recents
        self.host = host
    }

    /// Blank folder means inherit (the historic behavior); anything else
    /// spawns in the typed directory (the shell validates is-dir before
    /// confirming, mirroring the shared picker's inline refuse-and-fix).
    public var effectiveFolder: String? {
        let trimmed = folder.trimmingCharacters(in: .whitespaces)
        return trimmed.isEmpty ? nil : trimmed
    }

    public var selectedCli: CliRow? {
        guard !clis.isEmpty else { return nil }
        return clis[cliIndex % clis.count]
    }

    /// One-line spawn preview (`muse in ~/api`), so the launch is
    /// verifiable before it runs. The yolo flag itself rides inside
    /// `staap_spawn_launch`; the preview names the combination, not argv.
    public var preview: String {
        let cli = selectedCli?.id ?? "muse"
        let where_ = effectiveFolder.map { " in \($0)" } ?? ""
        let yoloTag: String
        switch yolo {
        case .useDefault: yoloTag = ""
        case .forceOn: yoloTag = " + yolo"
        case .forceOff: yoloTag = " (yolo off)"
        }
        return "\(cli)\(where_)\(yoloTag)"
    }

    /// Blank host means a local launch (the historic behavior); anything
    /// else dials that SSH host and skips the local is-dir validation
    /// (the folder is a remote path then).
    public var isRemote: Bool {
        effectiveRemoteHost != nil
    }

    /// Trimmed SSH host, or nil for a local launch.
    public var effectiveRemoteHost: String? {
        let trimmed = host.trimmingCharacters(in: .whitespaces)
        return trimmed.isEmpty ? nil : trimmed
    }

    /// One-line spawn preview with the SSH leg named when remote
    /// (`ssh mybox: muse in ~/api + yolo`), so the launch is verifiable
    /// before it runs.
    public var remotePreview: String {
        guard let remote = effectiveRemoteHost else { return preview }
        return "ssh \(remote): \(preview)"
    }

    public mutating func stepCli(forward: Bool) {
        guard !clis.isEmpty else { return }
        if forward {
            cliIndex = (cliIndex + 1) % clis.count
        } else {
            cliIndex = (cliIndex + clis.count - 1) % clis.count
        }
    }
}
