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
| `picker` | `staap-picker-test`: the bridge's shared-helper wrappers (feed/key/preview/yolo/age/glyph/registry surface) against the real staticlib, no GTK, plus the hermetic remote wrappers (`bridge_clis_remote` null/blank degrade, `bridge_remote_recent_json` unknown/null-core empty, `bridge_run_spawn_remote` blank/null-host refuse, plus the SSH-picker wrappers (`bridge_saved_hosts_json` round trip, add blank-name/host refuses, rename round trip + unknown/blank/null refuses, remove unknown refuses, `bridge_remote_default_folder` unknown/blank/null yields null, `bridge_remote_list_json` null/blank degrades to `[]`, `bridge_remote_test_json` null/blank unreachable) — no network) |
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
| Spawn | exactly one New session button / Ctrl+N / Ctrl+Shift+N opens the in-place screen on the right side on every press (no repeat-last, no caret, no modal dialog): folder entry + Choose dialog (this machine) or remote folder browser (saved hosts only: Up/Refresh, open-a-folder fills the entry, files inert, one bounded ssh listing per action with stale-drop, defaulting to the host default folder via `bridge_remote_default_folder`), per-host recents (`bridge_remote_recent_json`, core-owned, no network), machine dropdown (This machine / saved hosts by display name / Add remote machine... opening the SSH add dialog: display name, user@hostname-or-alias host with a user@hostname example hint, default folder, Test via `bridge_remote_test_json` showing reachable/unreachable plus detected CLIs, Save/Cancel via `bridge_saved_host_add` + autosave), CLI radios with install-path tooltips over the autodetected catalog (remote probe `bridge_clis_remote`, stale-drop, silent preselect), yolo Default labeled on/off via `bridge_yolo_default`, live core preview `bridge_spawn_preview` (`ssh <host>:`-prefixed when remote); Start routes to `bridge_run_spawn` (this machine) or `bridge_run_spawn_remote` (local PTY runs `ssh -t`, blank folder = remote login dir), Cancel returns to the terminal; header Restart / Close run buttons stay (no restart/close dropdown buttons exist in this shell: restart/close are header buttons + keyboard only) |
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
- Remote sessions reuse the system SSH setup only (no daemon, no stored
  passwords/keys): the local PTY runs `ssh -t <host> <cli...>`, so
  interactive auth prompts surface in the terminal. The machine axis is a dropdown (This machine, saved hosts by display name, Add remote machine...) with Edit.../Remove buttons beside it for the saved selection (Delete on the focused dropdown removes too; all three are keyboard-reachable); the add dialog stores display name + user@hostname-or-alias host + optional default folder through the core saved-hosts API (persisted by the normal autosave) — in edit mode it prefills and saves rename in place — and its Test button reports reachable/unreachable plus the detected remote CLIs. Remote folders browse from the host default (saved default, then host MRU, then the remote login directory); per-host folder recents stay core-owned via `bridge_remote_recent_json`.
- This directory must stay free of the macOS GUI framework in code and
  prose alike (CI enforces it with a literal grep gate): the Linux shell
  binds the C ABI only.
