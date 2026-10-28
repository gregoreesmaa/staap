/// Option-key scoping for the Swift shell (issue #116).
///
/// RED scaffold: this stub always answers "no composition" (the
/// pre-fix behavior, where SwiftTerm Meta-encodes every Option+key),
/// so the new `KeyHandlingRegressionTests` fail against it. The real
/// classifier replaces this body in the fix commit.
enum OptionKeyScoping {
    /// Returns the layout-composed text to send for an Option+key
    /// event, or nil when Meta (or another rule) owns the key.
    static func composedText(characters _: String?, charactersIgnoringModifiers _: String?) -> String? {
        nil
    }
}
