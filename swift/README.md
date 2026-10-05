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
swift test                 # pure-Swift unit tests (snapshot delta)
.build/debug/StaapMac --smoke   # headless C ABI runtime check
```

Run the app with `swift run` or `.build/debug/StaapMac`.

## Wiring (dumb renderer over the core run registry)

The core owns the roster, selection, filter, PTYs, statuses, links, key
table, feed reconciler, preview copy, and persistence; this shell owns
SwiftUI views, event wiring, and byte transport only. Same contract as
the Linux/Windows shells (native look, identical behavior).

| Feature | Path |
|---|---|
| Roster | core registry rows (`staap_session_count` + `staap_session_json` live, `staap_pump_all` refreshes statuses/links), grouped Working → Idle (needs-input shares Idle, #105) → History; headers use the core `Title (n)` format; rows show glyph + project/harness/age + expandable parsed links; single selection stored in the core (`staap_selected`/`staap_select`) |
| Spawn | split-button 2D launch: `Repeat last session` (Cmd-N) replays the last folder × CLI + yolo via `staap_run_spawn` (null CLI/folder, attaches a real roster row under the shared cap); `Choose Folder, CLI, Options…` (Cmd-Shift-N) opens the picker sheet (folder field + Choose panel + recents from `staap_recent_json`, CLI radio with status icons/install paths over `staap_clis_json`, yolo Default labeled on/off via `staap_yolo_default`, core preview `staap_spawn_preview`) → `staap_run_spawn` |
| Restart / close | footer buttons / Cmd-R / Cmd-W + row context menus → `staap_run_restart` (same id, keeps title/links) / `staap_run_close` + autosave; ended rows offer restart inline in the detail pane |
| Converse | keystrokes `send` -> `staap_run_write`; output `staap_run_pump` -> `staap_run_screen_text`/`staap_run_spans_json` -> `staap_feed_delta`/`staap_ansi_render` -> view feed; caret placed from `staap_run_cursor` (CUP after each feed); row switch recreates the view with a full replay |
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
