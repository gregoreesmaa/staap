/// Structural identity for the sidebar List: the visible row sequence.
///
/// The List recreates itself exactly when this key moves, so a reused
/// cell can never paint a previous row behind the new one (overprinted
/// titles that freeze while live rows move). In-place updates — status
/// flips that keep the order, age ticks, selection moves, typing that
/// does not change the visible set — keep the instance and the scroll
/// position. Membership and order are the whole contract: statuses,
/// ages, and selection ride along inside the cells and must NOT be
/// part of the key, or every tick would rebuild the List.
public func rosterSequenceKey(active: [String], history: [String]?) -> String {
    active.joined(separator: ",") + "|" + (history?.joined(separator: ",") ?? "-")
}
