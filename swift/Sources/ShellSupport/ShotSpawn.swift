/// Seed-spawn count from the launch arguments, accepting both
/// `--shot-spawn N` and `--shot-spawn=N`.
///
/// Screenshot/mock launchers usually split the flag from its value, so an
/// `=`-only match silently spawned nothing and captures came out empty.
/// Unknown, missing, or non-positive counts read as zero (no seeding).
public func shotSpawnCount(_ args: [String]) -> Int {
    for (i, arg) in args.enumerated() {
        if arg.hasPrefix("--shot-spawn=") {
            if let n = Int(arg.dropFirst("--shot-spawn=".count)), n > 0 {
                return n
            }
        } else if arg == "--shot-spawn", i + 1 < args.count {
            if let n = Int(args[i + 1]), n > 0 {
                return n
            }
        }
    }
    return 0
}
