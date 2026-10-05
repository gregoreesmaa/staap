# StaapWinUI — native Windows shell (issue #64)

WinUI 3 + ConPTY front end over the core staticlib C ABI
(`include/staap.h` at the repo root). Third native shell after
`swift/` (#62) and `native/linux/` (#63): it completes the epic's
per-OS native promise. Core Rust files are untouched by this shell —
it links the framework-free core library only (the macOS GUI-stack
deps in `Cargo.toml` are platform-scoped, so Windows never compiles
that stack).

ConPTY note: no console is created on the WinUI side. The core's
`EmbeddedPty` on Windows is ConPTY-backed (`portable-pty` uses the
native Console Pseudo-terminal API), so `bridge_spawn` *is* the ConPTY
spawn; the WinUI surface renders core snapshots through feed deltas.
Exactly one line discipline (the core's) exists, so nothing
double-echoes — the same single-emulator rule as the Linux shell's
"no PTY inside VTE".

## Prerequisites (Windows 11)

- Visual Studio 2026 18.x with the **Desktop development with C++**
  workload (v145 toolset: what the vcxproj tracks), the UWP C++ build tools,
  and the **Windows 11 SDK** (the portable CMake/CTest suite also builds
  under VS2022 17.x).
- Rust 1.90.0 (for the core staticlib)
- CMake 3.21+ (for the portable C suite; on CI it ships with the runner)

## Build

The core staticlib must exist first (CMake and msbuild search
`target/debug` by default, or pass `-Dcore_lib_dir=` /
`/p:CoreLibDir=`):

```powershell
cargo build --lib   # produces target/debug/staap.lib
cmake -S native/windows -B native/windows/build
cmake --build native/windows/build --config Release
ctest --test-dir native/windows/build -C Release --output-on-failure
```

The WinUI app itself (unpackaged + self-contained — no MSIX/installer,
which is a non-goal of #64 and lives with the CI/packaging issue, and no
Windows App Runtime to preinstall — the runtime payload sits next to the
exe, so the release distribution is the whole output folder
`native/windows/x64/Release/StaapWinUI/`, never the exe alone):

```powershell
msbuild -t:restore native/windows/StaapWinUI.vcxproj
msbuild native/windows/StaapWinUI.vcxproj `
  /p:Configuration=Release /p:Platform=x64
```

One build is enough, including on a clean checkout: the page `.g.hpp`
sources compile in the stock post-Pass2 batch (`CompilerIteration=
`XamlGenerated`), after Pass2 rewrites them — no link-red dance.
Run these from a VS2026
`vcvars64` / Developer PowerShell environment (v145 toolset).

`Microsoft.WindowsAppSDK` (`Version="2.5.1"`) and `Microsoft.Windows.CppWinRT`
(`Version="3.0.260818.1"`) are pinned to exact stable builds in the vcxproj so restores never drift.

## Tests

```powershell
ctest --test-dir native/windows/build -C Release --output-on-failure
powershell -ExecutionPolicy Bypass `
  -File native/windows/tests/smoke_live.ps1 `
  -Smoke native/windows/build/Release/staap-win-smoke.exe
```

| Test | What it proves |
|---|---|
| `feed` | `staap-win-feed-test`: the bridge's reconciler wrapper (`bridge_feed_delta` over the shared `staap_feed_delta`) against the real staticlib, no WinUI |
| `picker` | `staap-win-picker-test`: the catalog/recents parse (native ComboBox rows) plus the shared preview/yolo/age/section/registry surface via the bridge, mirroring Linux `staap-picker-test` |
| `keys` | `staap-win-keys-test`: the bridge's key-table wrapper (`bridge_key_encode` over the shared core table) — Return→CR, Ctrl+C→ETX, keep-keys→0, the contract the terminal preview-tunnel relies on |
| `smoke` | `staap-win-smoke`: roster count/JSON/status over the real staticlib, OOB contract (`SMOKE-OK sessions=<n>`), plus the 2D-launch catalog and shared-helper surface |
| `smoke-live` | `smoke_live.ps1`: compiles `tests/fake_muse.c` to `muse.exe`, then registry spawn/pump/write/resize/close against it in a scratch profile (`SMOKE-LIVE-OK`), hermetic — no real agent, no live config |

## Wiring (dumb renderer over the core run registry)

The core owns the roster, selection, filter, PTYs, statuses, links,
key table, feed reconciler, preview copy, and persistence; this shell
owns WinUI controls, event wiring, clipboard access, and byte transport
only. Same contract as the macOS/Linux shells (native look, identical
behavior).

| Feature | Path |
|---|---|
| Roster | core registry rows (`staap_session_count` + `staap_session_json` live, `staap_pump_all` refreshes statuses/links), grouped Working → Idle (needs-input shares Idle, #105) → History (Needs-input list collapsed); one shared selection across the live lists, stored in the core (`staap_selected`/`staap_select`) |
| Spawn | split-button 2D launch: New Session face / Ctrl+N repeats the last folder × CLI + yolo via `bridge_run_spawn` (null CLI/folder, attaches a real roster row under the shared cap); the chevron / Ctrl+Shift+N opens the picker dialog (folder field + Browse picker + recents, CLI ComboBox with install paths over the autodetected catalog, yolo Default labeled on/off via `bridge_yolo_default`, core preview `bridge_spawn_preview`) → `bridge_run_spawn` |
| Restart / close | Restart + Close run buttons / Ctrl+R / Ctrl+W → `bridge_run_restart` (same id, keeps title/links) / `bridge_run_close` + autosave; ended rows offer restart inline in the terminal pane |
| Sidebar resize | drag the grip (or Tab to it + arrows/Home/End) — 220..480px (shared `bridge_clamp_sidebar` rule), persisted in `LocalSettings` |
| Converse | key event → `bridge_key_encode` (shared table; printables resolve through the thread layout via ToUnicode, so non-US layouts type correctly) → `bridge_run_write`; pump → `bridge_feed_delta` → append to the output box. Return and plain Ctrl+C ride `TermBox_PreviewKeyDown` (tunneling: the read-only box would otherwise swallow them before they bubble); everything else bubbles via `RootGrid_KeyDown`. New Session focuses the terminal, so typing + Enter submits immediately |
| Select / copy / paste | native read-only TextBox selection + Ctrl+Shift+C; Ctrl+V pastes via Clipboard → `bridge_run_write`; Ctrl+C forwards ETX (interrupts the child) |
| Scroll | output TextBox in a `ScrollViewer`, auto-tails; per-run text retained (capped at 100 000 chars) |
| Search / filter | sidebar search box writes the core filter (`bridge_set_filter`), rows match via `bridge_row_matches` in every group |
| History | collapsed group of rows with no live PTY, restored every launch; per-run output retained while the window lives |
| Persistence | automatic: throttled pump autosave + close hook → `bridge_core_save` (no manual control) |

## Notes

- Key encoding is the shared core table (`bridge_key_encode` — the old
  per-shell `src/terminal_keys.h` port is deleted): Return is CR,
  BackSpace is DEL, arrows/Home/End/navigation are xterm sequences,
  Ctrl+letter are control codes (Ctrl+C interrupts the child);
  Ctrl+Shift+C/V stay with the native control for copy. AltGr
  (Ctrl+Alt) passes through as a character modifier while bare Alt keeps
  the Linux ESC-prefix parity.
- Spawning runs the effective CLI (last-used, configured default, or
  first autodetected, with the stored per-agent flags — the core loads
  the user config, so native spawns honor it); without any CLI on PATH
  the shell reports the core's error message in the status bar.
- Styling (issue #72) targets the Windows App SDK gallery look: Mica system backdrop, content extended into the title bar with a custom drag region, card surfaces with rounded corners, Segoe UI Variable type ramp, and ThemeResource brushes throughout so the window follows the system theme. No behavior changes.
- This directory must stay free of the macOS GUI framework in code and
  prose alike (CI enforces it with a literal grep gate): the Windows
  shell binds the C ABI only.