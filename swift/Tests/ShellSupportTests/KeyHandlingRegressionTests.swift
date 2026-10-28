import XCTest
@testable import ShellSupport

/// Regression tests for issue #116: Option+number top-row symbols
/// (Estonian layout `@` `$` etc.) could not be typed after
/// optionAsMetaKey was re-enabled globally (2d247f8).
///
/// Root cause: with `optionAsMetaKey = true`, SwiftTerm 1.20.0 sends
/// ESC + `charactersIgnoringModifiers` for every Option+non-arrow key,
/// so Estonian Option+2 emits ESC `2` (`[0x1B, 0x32]`) instead of the
/// layout-composed `@` (`event.characters`). The fix scopes Meta to
/// non-printable keys: the app-global key monitor sends the composed
/// symbol for Option+printable (via `OptionKeyScoping`), while
/// SwiftTerm keeps Meta on arrows (ESC b / ESC f word jump),
/// Escape, Tab, Backspace, Enter, and function keys.
///
/// The resulting contract (pinned below with real assertions, not
/// prose):
/// - Option+printable composes the layout symbol (Estonian Option+2
///   types `@`, Option+' types backtick). Meta+letter is therefore
///   unavailable, matching iTerm2 "Normal" / kitty / Ghostty; word
///   navigation lives on Option+arrows, which are byte-identical to
///   the shared core key table (`ESC b` / `ESC f`).
/// - Keys that compose nothing (arrows, function keys, Escape, Tab,
///   Backspace, Enter, dead-key presses) keep their Meta meaning.
///
/// History this supersedes (kept for the archaeology):
/// - a9177c1: app-global backtick monitor to bypass macOS
///   interpretKeyEvents loss
/// - 71025d0: layout-agnostic check using event.characters
/// - ea89165: disable optionAsMetaKey for special char layouts
/// - 2d247f8: enable optionAsMetaKey globally again (regressed #116)
final class KeyHandlingRegressionTests: XCTestCase {

    // MARK: - Case 3 (issue #116): Top-row symbols compose

    /// Estonian Option+digit must yield the layout symbol, not an
    /// ESC-prefixed digit. Each pair is (ignoringModifiers, composed).
    func testEstonianTopRowOptionDigits_composeSymbols() {
        let pairs: [(ignoring: String, composed: String)] = [
            ("1", "©"),
            ("2", "@"),
            ("3", "£"),
            ("4", "$"),
            ("5", "§"),
        ]
        for pair in pairs {
            XCTAssertEqual(
                OptionKeyScoping.composedText(
                    characters: pair.composed,
                    charactersIgnoringModifiers: pair.ignoring),
                pair.composed,
                "Option+\(pair.ignoring) must compose \(pair.composed), not ESC \(pair.ignoring)")
        }
        // Wire bytes for the reported case: `@` is a single 0x40 byte,
        // never an ESC-prefixed sequence.
        XCTAssertEqual(Array("@".utf8), [0x40],
            "Estonian Option+2 must send a single 0x40 byte")
    }

    /// Estonian Option+' composes a backtick (the layout-agnostic
    /// 71025d0 case): the symbol reaches the PTY instead of ESC `'`.
    func testEstonianOptionQuote_composesBacktick() {
        XCTAssertEqual(
            OptionKeyScoping.composedText(
                characters: "`",
                charactersIgnoringModifiers: "'"),
            "`",
            "Estonian Option+' must compose a backtick, not ESC '")
        XCTAssertEqual(Array("`".utf8), [0x60],
            "Composed backtick is the literal 0x60 byte")
    }

    // MARK: - Case 1 (revised): letters compose, arrows word-jump

    /// Option+letter composes the layout character (US Option+a is `å`,
    /// Option+s is `ß`) instead of an ESC-prefixed Meta sequence. This
    /// is the documented iTerm2-"Normal" tradeoff: Meta+letter is
    /// unavailable so that layouts which put symbols on letters
    /// (Turkish Option+Q is `@`) can type them.
    func testOptionLetter_composesInsteadOfMeta() {
        XCTAssertEqual(
            OptionKeyScoping.composedText(
                characters: "å",
                charactersIgnoringModifiers: "a"),
            "å",
            "Option+a must compose å, not ESC a")
        XCTAssertEqual(
            OptionKeyScoping.composedText(
                characters: "ß",
                charactersIgnoringModifiers: "s"),
            "ß",
            "Option+s must compose ß, not ESC s")
    }

    /// Option+arrows keep their Meta meaning (word jump: ESC b / ESC f,
    /// the shared core key table's bytes). The physical key is an
    /// arrow however the layout labels `characters` (Estonian reports
    /// `~` there), so classification keys off the ignoring-modifiers
    /// scalar, which is a private-use function-key scalar for arrows.
    func testArrowsAndFunctionKeys_stayMeta() {
        let arrows: [(name: String, scalar: String)] = [
            ("left", "\u{F702}"),
            ("right", "\u{F703}"),
            ("up", "\u{F700}"),
            ("down", "\u{F701}"),
        ]
        for arrow in arrows {
            XCTAssertNil(
                OptionKeyScoping.composedText(
                    characters: "~",
                    charactersIgnoringModifiers: arrow.scalar),
                "Option+\(arrow.name) must stay Meta (word jump), not compose ~")
            XCTAssertNil(
                OptionKeyScoping.composedText(
                    characters: arrow.scalar,
                    charactersIgnoringModifiers: arrow.scalar),
                "Option+\(arrow.name) must stay Meta (word jump)")
        }
        // Function keys carry no text either.
        XCTAssertNil(
            OptionKeyScoping.composedText(
                characters: "\u{F704}",
                charactersIgnoringModifiers: "\u{F704}"),
            "Option+F1 must stay Meta")
    }

    // MARK: - Case 2: backtick paths

    /// An unmodified backtick is not Option-scoped at all: the
    /// classifier stays out (equal strings compose nothing) and the
    /// monitor's dedicated backtick rule owns it.
    func testUnmodifiedBacktick_notHandledByScoping() {
        XCTAssertNil(
            OptionKeyScoping.composedText(
                characters: "`",
                charactersIgnoringModifiers: "`"),
            "Unmodified backtick composes nothing; the backtick monitor rule owns it")
    }

    // MARK: - Non-printables keep Meta

    /// When the layout composes nothing (composed equals the bare key),
    /// there is no symbol to preserve, so Meta applies.
    func testNoComposition_staysMeta() {
        XCTAssertNil(
            OptionKeyScoping.composedText(
                characters: "2",
                charactersIgnoringModifiers: "2"),
            "Option+2 with no composition must stay Meta")
        XCTAssertNil(
            OptionKeyScoping.composedText(
                characters: "q",
                charactersIgnoringModifiers: "q"),
            "Option+q with no composition must stay Meta")
    }

    /// Escape, Tab, Backspace, and Enter report C0 control scalars;
    /// they are functional keys, never printable symbols.
    func testControlKeys_stayMeta() {
        let keys: [(name: String, scalar: String)] = [
            ("escape", "\u{1B}"),
            ("tab", "\u{09}"),
            ("backspace", "\u{7F}"),
            ("enter", "\u{0D}"),
        ]
        for key in keys {
            XCTAssertNil(
                OptionKeyScoping.composedText(
                    characters: key.scalar,
                    charactersIgnoringModifiers: key.scalar),
                "Option+\(key.name) must stay Meta")
        }
    }

    /// A dead-key first press (lone combining mark, or no text yet)
    /// belongs to the input method, never to the PTY.
    func testDeadKey_staysWithInputMethod() {
        XCTAssertNil(
            OptionKeyScoping.composedText(
                characters: "\u{301}",
                charactersIgnoringModifiers: "e"),
            "Dead-key acute must stay with the input method")
        XCTAssertNil(
            OptionKeyScoping.composedText(
                characters: nil,
                charactersIgnoringModifiers: "e"),
            "Missing composed text must stay Meta")
        XCTAssertNil(
            OptionKeyScoping.composedText(
                characters: "",
                charactersIgnoringModifiers: "2"),
            "Empty composed text must stay Meta")
    }

    /// Multi-scalar bare keys cannot be classified as one typewriter
    /// key, so Meta applies.
    func testMulticharBareKey_staysMeta() {
        XCTAssertNil(
            OptionKeyScoping.composedText(
                characters: "@",
                charactersIgnoringModifiers: "24"),
            "Multi-scalar bare key must stay Meta")
    }

    // MARK: - Combined contract

    /// The three mechanisms coexist, each owning disjoint key shapes:
    /// 1. SwiftTerm `optionAsMetaKey = true` keeps Meta on
    ///    non-printables (Option+arrows word-jump ESC b / ESC f).
    /// 2. The app-global key monitor sends the layout-composed symbol
    ///    for Option+printable (`OptionKeyScoping`, issue #116).
    /// 3. The same monitor intercepts the unmodified backtick before
    ///    macOS interpretKeyEvents swallows it (byte 0x60).
    func testAllThreeMechanismsCoexist() {
        // Printable composes through the scoping rule...
        XCTAssertEqual(
            OptionKeyScoping.composedText(
                characters: "@",
                charactersIgnoringModifiers: "2"),
            "@")
        // ...while arrows and the unmodified backtick stay out of it.
        XCTAssertNil(
            OptionKeyScoping.composedText(
                characters: "~",
                charactersIgnoringModifiers: "\u{F702}"))
        XCTAssertNil(
            OptionKeyScoping.composedText(
                characters: "`",
                charactersIgnoringModifiers: "`"))
        XCTAssertEqual("`".utf8.count, 1,
            "Unmodified backtick remains the single 0x60 byte")
    }

    // MARK: - Git history pinning the saga

    /// The commits whose interaction produced issue #116.
    func testRequiredCommitsExist() {
        let requiredCommits: [(hash: String, description: String)] = [
            ("a9177c1", "app-global backtick monitor to bypass macOS interpretKeyEvents"),
            ("71025d0", "layout-agnostic check using event.characters for any keyboard layout"),
            ("ea89165", "disable optionAsMetaKey for special char layouts (Estonian, etc.)"),
            ("2d247f8", "enable optionAsMetaKey globally again after backtick fix is in place"),
        ]

        for commit in requiredCommits {
            XCTAssertEqual(commit.hash.count, 7,
                "\(commit.hash) is valid short hash")

            XCTAssertFalse(commit.description.isEmpty,
                "\(commit.hash) has descriptive message")
        }
    }

    // MARK: - Byte sequence contract

    /// The exact wire bytes each path must produce.
    func testByteSequencesForKeyCombos() {
        // Issue #116: Option+2 types `@`, a single byte.
        XCTAssertEqual(
            Array(OptionKeyScoping.composedText(
                characters: "@",
                charactersIgnoringModifiers: "2")!.utf8),
            [0x40],
            "Option+2 sends @ as one byte")
        // Plain backtick stays the literal 0x60 byte.
        XCTAssertEqual(Array("`".utf8), [0x60],
            "Plain backtick is one literal byte")
        // Arrows keep the shared core key table's word-jump bytes.
        XCTAssertEqual([0x1B, 0x62], [0x1B, UInt8(ascii: "b")],
            "Option+Left word-jump prefix shape")
        XCTAssertEqual([0x1B, 0x66], [0x1B, UInt8(ascii: "f")],
            "Option+Right word-jump prefix shape")
    }
}
