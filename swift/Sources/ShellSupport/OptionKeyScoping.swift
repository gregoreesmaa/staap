/// Option-key scoping for the Swift shell (issue #116).
///
/// With SwiftTerm's `optionAsMetaKey = true`, every Option+non-arrow
/// key emits ESC + `charactersIgnoringModifiers`, so Estonian
/// Option+2 sends ESC `2` instead of the layout-composed `@`. This
/// classifier decides which Option+key events the app-global key
/// monitor must compose itself (returning the text to send) and which
/// stay on SwiftTerm's Meta path (returning nil): Meta is scoped to
/// non-printable keys, so Option+number yields symbols while
/// Option+arrows keep their word-jump bytes.
///
/// Pure strings in, no AppKit: the monitor owns the modifier flags
/// (Command/Control exclusion) and the focused-view check, so this
/// stays unit-testable without a view.
import Foundation

public enum OptionKeyScoping {
    /// Returns the layout-composed text to send for an Option+key
    /// event, or nil when Meta (or another rule) owns the key.
    public static func composedText(
        characters: String?,
        charactersIgnoringModifiers: String?
    ) -> String? {
        // The bare key must be exactly one typewriter scalar: arrows,
        // function keys, Escape, Tab, Backspace, and Enter report C0
        // controls or private-use scalars instead.
        guard let composed = characters, !composed.isEmpty,
              let ignoring = charactersIgnoringModifiers,
              ignoring.unicodeScalars.count == 1,
              let base = ignoring.unicodeScalars.first,
              isTypewriterKey(base),
              // Only bypass Meta when the layout actually composed
              // something: a key that yields its own bare character
              // has no symbol to preserve.
              composed != ignoring,
              // The composed text must carry a real character: a lone
              // combining mark belongs to a dead-key sequence and stays
              // with the input method, and control/private-use output
              // must never be synthesized here.
              composed.unicodeScalars.contains(where: isSendableScalar)
        else {
            return nil
        }
        return composed
    }

    /// True for a single typewriter-key scalar: anything that is not a
    /// C0/C1 control (Escape, Tab, Enter, Backspace/Delete report
    /// those) and not an Apple function-key scalar (arrows and F-keys
    /// are U+F700 and up).
    private static func isTypewriterKey(_ scalar: Unicode.Scalar) -> Bool {
        let v = scalar.value
        if v < 0x20 || (0x7F ... 0x9F).contains(v) { return false }
        if (0xF700 ... 0xF8FF).contains(v) { return false }
        return true
    }

    /// True for a scalar the monitor may send: a real character, not a
    /// combining mark (dead key), control, or function-key scalar.
    private static func isSendableScalar(_ scalar: Unicode.Scalar) -> Bool {
        if CharacterSet.nonBaseCharacters.contains(scalar) { return false }
        return isTypewriterKey(scalar)
    }
}
