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
}
