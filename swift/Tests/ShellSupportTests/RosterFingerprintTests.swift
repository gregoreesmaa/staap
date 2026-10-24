import XCTest

@testable import ShellSupport

final class RosterFingerprintTests: XCTestCase {
    private func rows() -> [RosterFingerprintRow] {
        [
            RosterFingerprintRow(
                id: "a", title: "wren", project: "staap", harness: "claude",
                lastActive: 100, linkCount: 0, status: 2
            ),
            RosterFingerprintRow(
                id: "b", title: "mole", project: "staap", harness: "claude",
                lastActive: 90, linkCount: 1, status: 2
            ),
        ]
    }

    private func print(
        _ rows: [RosterFingerprintRow]? = nil, historyExpanded: Bool = false,
        selection: String? = "a", minute: Int = 7
    ) -> String {
        rosterFingerprint(
            rows: rows ?? self.rows(), historyExpanded: historyExpanded,
            selection: selection, minute: minute
        )
    }

    func testIdenticalRostersMatch() {
        XCTAssertEqual(print(), print())
    }

    func testEmptyRosterIsJustTheSuffix() {
        XCTAssertEqual(print([]), "h" + "a" + "7")
    }

    func testReorderChanges() {
        XCTAssertNotEqual(print(), print(Array(rows().reversed())))
    }

    func testRenameChanges() {
        var renamed = rows()
        renamed[0].title = "stoat"
        XCTAssertNotEqual(print(), print(renamed))
    }

    func testStatusFlipChanges() {
        var flipped = rows()
        flipped[1].status = 1
        XCTAssertNotEqual(print(), print(flipped))
    }

    func testLinkGrowthChanges() {
        var linked = rows()
        linked[0].linkCount = 2
        XCTAssertNotEqual(print(), print(linked))
    }

    func testActivityChanges() {
        var active = rows()
        active[0].lastActive = 101
        XCTAssertNotEqual(print(), print(active))
    }

    func testHistorySelectionAndMinuteChange() {
        XCTAssertNotEqual(print(), print(historyExpanded: true))
        XCTAssertNotEqual(print(), print(selection: "b"))
        XCTAssertNotEqual(print(), print(selection: nil))
        XCTAssertNotEqual(print(), print(minute: 8))
    }
}
