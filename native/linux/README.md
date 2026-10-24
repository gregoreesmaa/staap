# StaapGTK — native Linux shell (issue #63)

GTK4 + libadwaita + VTE front end over the core staticlib C ABI
(`include/staap.h` at the repo root). Second native shell after
`swift/` (#62): it proves the C ABI is portable, not Swift-only. Core Rust
files are untouched by this shell — it links the framework-free core
library only (the macOS GUI-stack deps in `Cargo.toml` are
platform-scoped, so Linux never compiles that stack).

## Prerequisites (Ubuntu 24.04)

```sh
sudo apt install libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev \
  meson ninja-build pkg-config gcc xvfb
```

Versions this shell is proved against (container `ubuntu:24.04`):
GTK 4.14.5, libadwaita 1.5.0, VTE 0.76.0 (`vte-2.91-gtk4`).

## Build

The core staticlib must exist first (meson searches `target/debug` by
default, or pass `-Dcore_lib_dir=`):

```sh
cargo build --lib            # produces target/debug/libstaap.a
meson setup native/linux/build native/linux
meson compile -C native/linux/build
./native/linux/build/staap-gtk
```

## Tests (no display needed)

```sh
meson test -C native/linux/build
```

| Test | What it proves |
|---|---|
| `feed` | `staap-feed-test`: the bridge's reconciler wrapper (`bridge_feed_delta` over the shared `staap_feed_delta`) against the real staticlib, no GTK |
| `picker` | `staap-picker-test`: the bridge's shared-helper wrappers (feed/key/preview/yolo/age/glyph/registry surface) against the real staticlib, no GTK |
| `smoke` | `staap-gtk-smoke`: roster count/JSON/status over the real staticlib, OOB contract (`SMOKE-OK sessions=<n>`), plus the 2D-launch catalog and shared-helper surface |
| `smoke-live` | `smoke_live.sh`: registry spawn/pump/write/resize/close against a fake `muse` on `PATH` in a scratch `HOME` (`SMOKE-LIVE-OK`), hermetic — no real agent, no live config |

Headless UI run (window opens, pump ticks, quits on timeout):

```sh
xvfb-run -a ./native/linux/build/staap-gtk
```

## Wiring (dumb renderer over the core run registry)

The core owns the roster, selection, filter, PTYs, statuses, links,
key table, feed reconciler, preview copy, and persistence; this shell
owns GTK widgets, event wiring, and byte transport only. Same contract
as the macOS/Windows shells (native look, identical behavior).

| Feature | Path |
|---|---|
| Roster | core registry rows (`staap_session_count` + `staap_session_json` live, `staap_pump_all` refreshes statuses/links), grouped Working → Idle (needs-input shares Idle, #105) → History; single selection in the core (`staap_selected`/`staap_select`) |
| Spawn | split-button 2D launch: New-run button / Ctrl+N repeats the last folder × CLI + yolo via `bridge_run_spawn` (null CLI/folder, attaches a real roster row under the shared cap); the ▾ caret / Ctrl+Shift+N opens the picker dialog (folder entry + Choose dialog + recents, CLI radios with install-path tooltips over the autodetected catalog, yolo Default labeled on/off via `bridge_yolo_default`, core preview `bridge_spawn_preview`) → `bridge_run_spawn` |
| Restart / close | header buttons / Ctrl+R / Ctrl+W → `bridge_run_restart` (same id, keeps title/links) / `bridge_run_close` + autosave; ended rows offer restart inline in the terminal pane |
| Converse | key event → `bridge_key_encode` (shared table) → `bridge_run_write`; pump → `bridge_feed_delta` → `vte_terminal_feed` + caret from `bridge_run_cursor` (CUP after each feed) |
| Select / copy / paste | native VTE selection + Ctrl+Shift+C/V + right-click menu |
| Scroll | VTE scrollback capped at 10 000 lines, in a `GtkScrolledWindow` |
| Search / filter | sidebar `GtkSearchEntry` writes the core filter (`bridge_set_filter`), rows match via `bridge_row_matches`; Ctrl+F find bar via `VteRegex` search |
| History | rows show glyph + project/harness/age (core strings), restored every launch; ended rows offer restart inline |
| Theme | follows the system appearance (`AdwStyleManager` default + VTE palette, no manual override, no GSettings schema) |
| Persistence | automatic: throttled pump autosave + close hook → `bridge_core_save` (no Save button) |

## Notes

- The core owns its emulator; the VTE widget owns a second one fed with
  snapshot deltas (`bridge_feed_delta`, the shared reconciler — the old
  per-shell `src/feed.c` port is deleted).
  No PTY is ever spawned inside VTE: typed keys are core-encoded and
  forwarded, echoed output arrives via the pump. Exactly one line
  discipline (the core's) exists, so nothing double-echoes.
- Key encoding is the shared core table (`bridge_key_encode`): Return is
  CR, BackSpace is DEL, arrows/Home/End/navigation are xterm sequences,
  Ctrl+letter are control codes (Ctrl+C interrupts the child);
  Ctrl+Shift+C/V stay with VTE for copy/paste. Window resizes report the
  grid back via `bridge_run_resize` (every attached run).
- Spawning runs the effective CLI (last-used, configured default, or
  first autodetected, with the stored per-agent flags — the core loads
  the user config, so native spawns honor it); without any CLI on PATH
  the shell toasts the core's error message.
- This directory must stay free of the macOS GUI framework in code and
  prose alike (CI enforces it with a literal grep gate): the Linux shell
  binds the C ABI only.
