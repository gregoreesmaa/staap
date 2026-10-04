# AGENTS.md

You are completely autonomous from now on.

## Vision

North star (see `VISION.md`): staap is the local-first mission control for agent runs — every run a real interactive session, any run needing input surfaces in seconds, no link or transcript ever lost, everything offline, free and open-source forever.

Durable principles:

- Live fidelity over simulation (real PTY, no mocks for live prompt).
- Triage by urgency, glanceable in seconds (needs-input surfaces in list, status bar, background title within 60s).
- Local-only trust (plain-file state, zero accounts/telemetry/sync).
- Small core, bounded growth (framework-free core, caps/eviction on accumulate-forever stores, idle ~zero).
- Open seams, agent-neutral (documented parser/provider extension points).
- Fail visible, recover in one click (no silent loss, keyboard + non-color parity).

Full detail lives in `VISION.md` (3-year north star, promises, scope, sustainability) plus `VISION-PRODUCT.md`, `VISION-TECHNICAL.md`, `VISION-UX.md`. Near-term work is tracked in `ROADMAP.md`. Keep changes aligned with these; `grep paywall|billing|telemetry src/` stays empty.

## Project structure

Framework-free Rust core + four thin shells. UI work must target the
right shell — each owns its own sidebar/chrome.

- Core library (`src/lib.rs`): `app`, `config`, `embedded`, `parsers`,
  `persist`, `providers`, `scrollback`, `transcript` — no toolkit types —
  plus the C ABI (`src/ffi.rs`, mirrored in `include/staap.h`;
  regenerate with cbindgen after any FFI change). Its headless tests are
  the cross-platform contract: they must pass unchanged on every OS.
- Shells (all over the core; seam spec in `docs/native-core-seam.md`):
  - `src/main.rs` + `src/gui/` — original macOS gpui shell
    (`cargo build`, `./target/debug/staap`). Sidebar is
    `gui/runs_panel.rs` on opaque `theme::SIDEBAR_BG`; native AppKit
    interop here is wontfix (`docs/57-sidebar-native-eval.md`).
  - `swift/` — native macOS shell (`StaapMac`, SwiftUI +
    SwiftTerm). Roster is a plain `NavigationSplitView` sidebar
    (`.listStyle(.sidebar)`, no custom backgrounds) in
    `Sources/StaapMac/ContentView.swift` with a hidden title bar
    and no toolbar; the detail ignores the top container safe area so
    the terminal starts at the window edge. Terminal and window are
    pure black / white following the system appearance.
    Build with `cargo build --lib` then `cd swift && swift build`;
    run `.build/debug/StaapMac`.
  - `native/linux/` — GTK4/libadwaita + VTE shell over the same C ABI.
  - `native/windows/` — WinUI 3 + ConPTY shell over the same C ABI.
- Per-shell wiring tables: `swift/README.md`, `native/linux/README.md`,
  `native/windows/README.md`.
- Gates: macOS `cargo test --all-targets`, `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings`; Swift `swift test`;
  Linux meson suite. Never fix one shell by editing another.

## Dumb-shell contract

UIs are dumb shells: every UI renders what the Rust core reports and
owns nothing else. Shells own widgets, event wiring, clipboard access,
focus management, and byte transport — nothing else.

Everything below lives in the core exactly once (native shells reach
it via `shell_shared` or the `staap_*` C ABI; the gpui shell calls it
directly) and must never be reimplemented per shell:

- Roster grouping and history membership: active = attached live PTY
  (`RunRegistry::is_live` / `staap_is_live`), grouped Needs input →
  Working → Idle; historic = no live PTY, hidden in the
  collapsed-by-default History group (`RunRegistry::is_history` /
  `staap_is_history`, expansion via `App.history_expanded` /
  `staap_history_expanded` / `staap_set_history_expanded`, persisted
  through `staap_core_save`).
- Filter match and selection (`row_matches_filter` /
  `staap_row_matches`, `App::set_filter` / `staap_set_filter`,
  `staap_selected` / `staap_select` / `staap_select_step`).
- Statuses and attention (`classify`, `attention_count`), links
  (`push_links`, `visible_links`), display strings (`relative_age`,
  `status_glyph`, `section_title`), key table (`encode_key` /
  `staap_key_encode`), feed reconciler (`feed_delta` /
  `staap_feed_delta`), SGR renderer (`render_ansi` /
  `staap_ansi_render`), spawn preview (`spawn_preview`), sidebar
  clamp (`clamp_sidebar_width`), live-run cap + eviction
  (`MAX_LIVE_RUNS`, `make_room`), persistence (`persist`, `Config`).

`grep -rni gpui` over the core module set (everything except
`src/gui/` + `src/main.rs`) returns zero hits. A new UI follows the
same seam: bind the C ABI, render the core, add its wiring table
README. Fix shell bugs in the owning shell; fix shared behavior in
the core with headless tests.
