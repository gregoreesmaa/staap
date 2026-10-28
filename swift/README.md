# StaapMac — native macOS shell (issue #62)

SwiftUI + SwiftTerm front end over the core staticlib C ABI
(`include/staap.h` at the repo root). Core Rust files are
untouched by this shell except the FFI layer's additive roster getters
(`staap_session_count`, `staap_session_json`).

## Build

The core staticlib must exist first (the linker searches
`target/debug` and `target/release`):

```sh
cargo build --lib          # produces target/debug/libstaap.a
cd swift && swift build    # SPM-only build (no Xcode project needed)
swift test                 # pure-Swift unit tests (reconciler, feed backlog, roster gate)
python3 -m unittest discover -s swift/Tests -p "test_*.py"  # overlap-gate unit tests (needs pillow)
.build/debug/StaapMac --smoke   # headless C ABI runtime check
```

Run the app with `swift run` or `.build/debug/StaapMac`.

Sidebar regression: `.build/debug/StaapMac --shot-spawn 3` attaches three
live runs up front; capture the window and run `python3
swift/Tests/sidebar_shot_check.py shot.png --expect-rows 3`, which asserts
the roster renders as distinct rows (CI runs the same check over seeded
mock sessions, plus the gate's own unit tests).

## Wiring (dumb renderer over the core run registry)

The core owns the roster, selection, filter, PTYs, statuses, links, key
table, feed reconciler, preview copy, and persistence; this shell owns
SwiftUI views, event wiring, and byte transport only. Same contract as
the Linux/Windows shells (native look, identical behavior).

| Feature | Path |
|---|---|
| Roster | core registry rows (`staap_session_count` + `staap_session_json` live, `staap_pump_all` refreshes statuses/links), grouped Working → Idle (needs-input shares Idle, #105) → History; headers use the core `Title (n)` format; rows show glyph + project/harness/age + expandable parsed links (PR rows lead with the stored core state icon via `staap_pr_glyph`, #115; refresh via `staap_pr_refresh` runs off the pump tick); single selection stored in the core (`staap_selected`/`staap_select`) |
| Spawn | single New Session button (Cmd-N) shows the in-place 2D-launch screen on the right side every press (no sheet/modal, no split-button repeat): folder field + Choose panel + recents from `staap_recent_json`, CLI radio with status icons/install paths over `staap_clis_json`, yolo Default labeled on/off via `staap_yolo_default`, core preview `staap_spawn_preview` → `staap_run_spawn` (attaches a real roster row under the shared cap); machine dropdown (`This machine`, saved hosts with display names, `Add remote machine…` over `staap_saved_hosts_json`) opens the SSH add sheet (display name, `user@hostname`-or-alias host, default folder, Test via `staap_remote_test_json` showing reachable/unreachable + detected CLIs, Save/Cancel via `staap_saved_host_add` + `staap_core_save` (a saved pick also offers Edit… (Command-E, sheet prefilled, saves via `staap_saved_host_rename`) and Delete (Command-Delete, via `staap_saved_host_remove` + autosave, falling back to This machine), and a remembered-but-unsaved last host offers Save…/Forget (`staap_forget_last_host`)); the last host (core `staap_last_host`, saved or not) preselects; a picked host probes the remote catalog off the main thread (`staap_remote_clis_json`, stale arrivals dropped), swaps in per-host recents (`staap_remote_recent_json`), starts the folder at the host default (`staap_remote_default_folder`: saved default → host MRU → home), browses via `staap_remote_list_json` (Up/Refresh/Go, dirs navigate, files inert, stale listings dropped), previews `ssh <host>: …`, and spawns via `staap_run_spawn_remote` (blank host refuses fail-visible; auth prompts surface in the terminal) |
| Restart / close | Cmd-R / Cmd-W + row context menus → `staap_run_restart` (same id, keeps title/links) / `staap_run_close` + autosave; ended rows offer restart inline in the detail pane (no sidebar dropdown entries, no footer) |
| Converse | keystrokes `send` -> `staap_run_write`; output `staap_run_pump` -> `staap_run_screen_text`/`staap_run_spans_json` -> `staap_feed_delta`/`staap_ansi_render` -> view feed; same-tick caret follows every feed; deltas re-anchor with an absolute CUP (or resync) so a mid-screen caret cannot shift the grid; row switch recreates the view with a full replay |
| Select / copy / paste / scroll | native SwiftTerm view and scrollback |
| Search / filter | sidebar search field writes the core filter (`staap_set_filter`), rows match via `staap_row_matches`; terminal find via Cmd-F (SwiftTerm find bar) |
| History | roster rows carry project, harness, last-active age (core strings); restored every launch; history group holds rows with no live PTY |
| Theme | follows the system appearance (no manual override) + native terminal colors |
| Persistence | automatic: throttled pump autosave + quit/close hooks call `staap_core_save` (no Save button) |

## Notes

- The core owns its emulator; the view owns a second one fed with
  snapshot deltas (`staap_feed_delta` via `Core.feedDelta`, the shared
  reconciler — the pure-Swift `TerminalFeed` stays for unit tests only).
  Streaming output and typing converge line by line; full redraws
  (resize, cursor-addressed programs) clear and replay the snapshot.
  Styled spans render through the shared `staap_ansi_render`.
- New windows resize the view; the view reports its grid back and the
  shell forwards it with `staap_run_resize` (every attached run resizes on
  Linux; macOS/Windows resize the selected run's PTY).
- Window drag: a 14pt clear strip above the terminal moves the window
  (the hidden title bar leaves no grab area; issue #101).
- Spawning runs the effective CLI (last-used, configured default, or
  first autodetected, with the stored per-agent flags — the core loads
  the user config, so native spawns honor it); without any CLI on PATH
  the shell shows the core's error message.
- Option+printable composes the layout symbol: the app-global key
  monitor sends `event.characters` straight to the view
  (`ShellSupport.OptionKeyScoping` decides; issue #116), so Estonian
  Option+2 types `@`. Option+arrows stay Meta via SwiftTerm
  `optionAsMetaKey` (ESC b / ESC f word jump, matching the core key
  table); the unmodified backtick is intercepted the same way (0x60).
