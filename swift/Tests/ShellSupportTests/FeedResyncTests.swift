import XCTest

@testable import ShellSupport

final class FeedResyncTests: XCTestCase {
    func testCupIsOneBased() {
        XCTAssertEqual(cupPosition(row: 0, col: 0), "\u{1B}[1;1H")
        XCTAssertEqual(cupPosition(row: 4, col: 9), "\u{1B}[5;10H")
    }

    func testResyncClearsAndNormalizes() {
        XCTAssertEqual(
            resyncFeed(current: "a\nb"),
            "\u{1B}[2J\u{1B}[Ha\r\nb"
        )
    }

    func testSnapshotEndPlain() {
        XCTAssertEqual(snapshotEnd("ab\ncde")?.row, 1)
        XCTAssertEqual(snapshotEnd("ab\ncde")?.col, 3)
    }

    func testSnapshotEndStripsSGR() {
        let styled = "\u{1B}[0mab\n\u{1B}[0;1mcd\u{1B}[0m"
        let end = snapshotEnd(styled)
        XCTAssertEqual(end?.row, 1)
        XCTAssertEqual(end?.col, 2)
    }

    func testSnapshotEndTrailingEmptyRow() {
        let end = snapshotEnd("ab\n")
        XCTAssertEqual(end?.row, 1)
        XCTAssertEqual(end?.col, 0)
    }

    func testSnapshotEndEmpty() {
        XCTAssertEqual(snapshotEnd("")?.row, 0)
        XCTAssertEqual(snapshotEnd("")?.col, 0)
    }

    func testSnapshotEndRejectsNonASCII() {
        XCTAssertNil(snapshotEnd("ab\ncaf\u{00E9}"))
        XCTAssertNil(snapshotEnd("\u{1B}[0m\u{65E5}\u{1B}[0m"))
    }

    func testStripSGRLeavesPlain() {
        XCTAssertEqual(stripSGR("plain"), "plain")
        XCTAssertEqual(stripSGR("\u{1B}[0mA\u{1B}[0;38;2;1;2;3mB"), "AB")
        XCTAssertEqual(stripSGR("x\u{1B}["), "x")
    }
}
