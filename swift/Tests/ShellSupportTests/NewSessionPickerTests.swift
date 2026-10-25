import XCTest

@testable import ShellSupport

/// Picker-model contract for the 2D new-session sheet (folder × CLI +
/// yolo). Pure Swift, no C ABI: runs on Linux CI the same as macOS.
final class NewSessionPickerTests: XCTestCase {
    private func picker() -> NewSessionPicker {
        NewSessionPicker(
            clis: [
                .init(id: "muse", program: "muse", path: "/bin/muse", available: true),
                .init(id: "claude", program: "claude", path: nil, available: false),
            ],
            cliIndex: 0,
            folder: "",
            recents: ["/tmp/api"]
        )
    }

    func testBlankFolderInherits() {
        var p = picker()
        XCTAssertNil(p.effectiveFolder)
        p.folder = "   "
        XCTAssertNil(p.effectiveFolder)
        p.folder = "/tmp/api"
        XCTAssertEqual(p.effectiveFolder, "/tmp/api")
    }

    func testYoloCyclesAndLabels() {
        var p = picker()
        XCTAssertEqual(p.yolo, .useDefault)
        XCTAssertEqual(p.yolo.label, "Default")
        p.yolo.cycle()
        XCTAssertEqual(p.yolo, .forceOn)
        p.yolo.cycle()
        XCTAssertEqual(p.yolo, .forceOff)
        XCTAssertEqual(p.yolo.label, "Off (once)")
        p.yolo.cycle()
        XCTAssertEqual(p.yolo, .useDefault)
    }

    func testCliStepsWrap() {
        var p = picker()
        XCTAssertEqual(p.selectedCli?.id, "muse")
        p.stepCli(forward: true)
        XCTAssertEqual(p.selectedCli?.id, "claude")
        p.stepCli(forward: true)
        XCTAssertEqual(p.selectedCli?.id, "muse")
        p.stepCli(forward: false)
        XCTAssertEqual(p.selectedCli?.id, "claude")
    }

    func testPreviewNamesCombination() {
        var p = picker()
        XCTAssertEqual(p.preview, "muse")
        p.folder = "/tmp/api"
        XCTAssertEqual(p.preview, "muse in /tmp/api")
        p.yolo = .forceOn
        XCTAssertEqual(p.preview, "muse in /tmp/api + yolo")
        // Missing CLIs stay selectable for their detail row.
        p.stepCli(forward: true)
        XCTAssertTrue(p.preview.contains("claude"))
    }

    func testBlankHostIsLocal() {
        var p = picker()
        XCTAssertFalse(p.isRemote)
        XCTAssertNil(p.effectiveRemoteHost)
        p.host = "   "
        XCTAssertFalse(p.isRemote)
        XCTAssertNil(p.effectiveRemoteHost)
        XCTAssertEqual(p.remotePreview, p.preview)
    }

    func testHostMarksRemoteAndNamesPreview() {
        var p = picker()
        p.host = "  mybox  "
        XCTAssertTrue(p.isRemote)
        XCTAssertEqual(p.effectiveRemoteHost, "mybox")
        XCTAssertEqual(p.remotePreview, "ssh mybox: muse")
        p.folder = "/tmp/api"
        p.yolo = .forceOn
        XCTAssertEqual(p.remotePreview, "ssh mybox: muse in /tmp/api + yolo")
    }

    func testEmptyCatalogSelectsNothing() {
        var p = NewSessionPicker(clis: [], cliIndex: 0, folder: "", recents: [])
        XCTAssertNil(p.selectedCli)
        p.stepCli(forward: true) // no crash on empty
        XCTAssertEqual(p.preview, "muse")
    }

    func testMachinePlaceholderShowsUserAtHostnameExample() {
        XCTAssertTrue(NewSessionPicker.machinePlaceholder.contains("user@hostname"))
    }

    func testMachineTagsOrderThisMachineSavedThenAdd() {
        let saved = [
            NewSessionPicker.SavedHostRow(name: "GPU box", host: "you@gpu-box", defaultFolder: "~/work"),
            NewSessionPicker.SavedHostRow(name: "", host: "alias-box"),
        ]
        XCTAssertEqual(
            NewSessionPicker.machineTags(savedHosts: saved, lastHost: ""),
            ["", "you@gpu-box", "alias-box", NewSessionPicker.addMachineTag]
        )
    }

    func testMachineTagsKeepUnsavedLastHostSelectable() {
        XCTAssertEqual(
            NewSessionPicker.machineTags(savedHosts: [], lastHost: "  you@lab  "),
            ["", "you@lab", NewSessionPicker.addMachineTag]
        )
        // A saved last host is never duplicated.
        let saved = [NewSessionPicker.SavedHostRow(name: "Lab", host: "you@lab")]
        XCTAssertEqual(
            NewSessionPicker.machineTags(savedHosts: saved, lastHost: "you@lab"),
            ["", "you@lab", NewSessionPicker.addMachineTag]
        )
    }

    func testMachineLabels() {
        let saved = [NewSessionPicker.SavedHostRow(name: "GPU box", host: "you@gpu-box")]
        XCTAssertEqual(NewSessionPicker.machineLabel(tag: "", savedHosts: saved), "This machine")
        XCTAssertEqual(
            NewSessionPicker.machineLabel(tag: NewSessionPicker.addMachineTag, savedHosts: saved),
            "Add remote machine…"
        )
        XCTAssertEqual(
            NewSessionPicker.machineLabel(tag: "you@gpu-box", savedHosts: saved),
            "GPU box (you@gpu-box)"
        )
        XCTAssertEqual(
            NewSessionPicker.machineLabel(tag: "bare-alias", savedHosts: saved),
            "bare-alias"
        )
    }

    func testResolveRemoteDefaultPrefersSavedThenMruThenHome() {
        XCTAssertEqual(
            NewSessionPicker.resolveRemoteDefault(savedDefault: "~/work", recents: ["~/other"]),
            "~/work"
        )
        XCTAssertEqual(
            NewSessionPicker.resolveRemoteDefault(savedDefault: "  ", recents: ["", "~/mru"]),
            "~/mru"
        )
        XCTAssertNil(NewSessionPicker.resolveRemoteDefault(savedDefault: nil, recents: ["  "]))
        XCTAssertNil(NewSessionPicker.resolveRemoteDefault(savedDefault: nil, recents: []))
    }

    func testRemoteParentAndJoin() {
        XCTAssertEqual(NewSessionPicker.remoteParent(of: "~/a/b"), "~/a")
        XCTAssertEqual(NewSessionPicker.remoteParent(of: "~/a"), "~")
        XCTAssertEqual(NewSessionPicker.remoteParent(of: "/a"), "/")
        XCTAssertNil(NewSessionPicker.remoteParent(of: "~"))
        XCTAssertNil(NewSessionPicker.remoteParent(of: "/"))
        XCTAssertNil(NewSessionPicker.remoteParent(of: ""))
        XCTAssertNil(NewSessionPicker.remoteParent(of: "bare"))
        XCTAssertEqual(NewSessionPicker.remoteParent(of: "~/a/"), "~")
        XCTAssertEqual(NewSessionPicker.remoteJoin(base: "~/a", name: "b"), "~/a/b")
        XCTAssertEqual(NewSessionPicker.remoteJoin(base: "~/a/", name: "b"), "~/a/b")
        XCTAssertEqual(NewSessionPicker.remoteJoin(base: "", name: "b"), "b")
    }

    func testRemoteEntryRowsDistinguishDirsFromFiles() {
        let dir = NewSessionPicker.RemoteEntryRow(name: "src", isDir: true)
        let file = NewSessionPicker.RemoteEntryRow(name: "notes.txt", isDir: false)
        XCTAssertTrue(dir.isDir)
        XCTAssertFalse(file.isDir)
        XCTAssertNotEqual(dir, file)
    }
}
