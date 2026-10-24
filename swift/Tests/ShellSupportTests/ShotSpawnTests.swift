import XCTest

@testable import ShellSupport

final class ShotSpawnTests: XCTestCase {
    func testSpaceFormSpawns() {
        XCTAssertEqual(shotSpawnCount(["StaapMac", "--shot-spawn", "3"]), 3)
    }

    func testEqualsFormSpawns() {
        XCTAssertEqual(shotSpawnCount(["StaapMac", "--shot-spawn=2"]), 2)
    }

    func testMissingCountIsZero() {
        XCTAssertEqual(shotSpawnCount(["StaapMac", "--shot-spawn"]), 0)
        XCTAssertEqual(shotSpawnCount(["StaapMac"]), 0)
    }

    func testNonPositiveCountsAreZero() {
        XCTAssertEqual(shotSpawnCount(["StaapMac", "--shot-spawn", "0"]), 0)
        XCTAssertEqual(shotSpawnCount(["StaapMac", "--shot-spawn=-1"]), 0)
        XCTAssertEqual(shotSpawnCount(["StaapMac", "--shot-spawn", "many"]), 0)
    }
}
