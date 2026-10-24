import XCTest

@testable import ShellSupport

final class RosterSequenceTests: XCTestCase {
    func testOrderMatters() {
        XCTAssertNotEqual(
            rosterSequenceKey(active: ["a", "b"], history: nil),
            rosterSequenceKey(active: ["b", "a"], history: nil)
        )
    }

    func testMembershipMatters() {
        XCTAssertNotEqual(
            rosterSequenceKey(active: ["a", "b"], history: nil),
            rosterSequenceKey(active: ["a", "b", "c"], history: nil)
        )
    }

    func testHistoryToggleMatters() {
        XCTAssertNotEqual(
            rosterSequenceKey(active: ["a"], history: nil),
            rosterSequenceKey(active: ["a"], history: ["h"])
        )
    }

    func testHistoryOrderMatters() {
        XCTAssertNotEqual(
            rosterSequenceKey(active: ["a"], history: ["h1", "h2"]),
            rosterSequenceKey(active: ["a"], history: ["h2", "h1"])
        )
    }

    func testStableAcrossNonStructuralChanges() {
        // Statuses, ages, and selection live inside the cells; the key
        // must not move for them or every tick would rebuild the List.
        let before = rosterSequenceKey(active: ["a", "b"], history: ["h"])
        XCTAssertEqual(before, rosterSequenceKey(active: ["a", "b"], history: ["h"]))
    }

    func testEmptyRoster() {
        XCTAssertEqual(rosterSequenceKey(active: [], history: nil), "|-")
    }
}
