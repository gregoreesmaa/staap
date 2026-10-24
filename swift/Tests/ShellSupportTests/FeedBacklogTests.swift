import XCTest

@testable import ShellSupport

final class FeedBacklogTests: XCTestCase {
    func testTakeOnEmptyBacklogIsNil() {
        var backlog = FeedBacklog()
        XCTAssertNil(backlog.take(after: 0))
    }

    func testEmptyPushIsIgnored() {
        var backlog = FeedBacklog()
        backlog.push("")
        XCTAssertNil(backlog.take(after: 0))
    }

    func testPushesConcatenateAndTakeConsumes() {
        // The view drains slower than the pump: both deltas must survive.
        var backlog = FeedBacklog()
        backlog.push("first\r\n")
        backlog.push("second\r\n")
        XCTAssertEqual(backlog.take(after: 0), "first\r\nsecond\r\n")
        // Consumed: the next take sees nothing new.
        XCTAssertNil(backlog.take(after: 1))
        XCTAssertNil(backlog.take(after: 999))
    }

    func testTakeAfterSeenSequenceIsNil() {
        var backlog = FeedBacklog()
        backlog.push("x")
        XCTAssertEqual(backlog.take(after: 0), "x")
        // seq is now 1: the coordinator already fed it.
        XCTAssertNil(backlog.take(after: 1))
    }

    func testFreshPushAfterTakeAccumulatesClean() {
        var backlog = FeedBacklog()
        backlog.push("a")
        XCTAssertEqual(backlog.take(after: 0), "a")
        backlog.push("b")
        XCTAssertEqual(backlog.take(after: 1), "b")
    }

    func testCapDropsStalledBacklog() {
        var backlog = FeedBacklog()
        backlog.push("stalled")
        let huge = String(repeating: "y", count: FeedBacklog.maxBytes)
        backlog.push(huge)
        // Over the cap: only the latest delta survives (resync follows).
        XCTAssertEqual(backlog.take(after: 1), huge)
    }
}
