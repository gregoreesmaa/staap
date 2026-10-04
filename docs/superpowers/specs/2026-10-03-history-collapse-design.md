# Historic Sessions Collapse Design

## Context

staap (in `C:\Users\grego\projects\agent-manager`) is a local-first
mission control for agent runs: a framework-free Rust core plus four
thin shells (macOS gpui in `src/gui/`, SwiftUI in `swift/`, GTK4 in
`native/linux/`, WinUI 3 in `native/windows/`). The core owns the
roster, selection, filter, live PTYs, statuses, links, key table,
reconciler, SGR renderer, preview, display strings, and persistence;
shells own widgets, event wiring, clipboard, focus, and byte transport.

Today the History story diverges per shell: gpui collapses History but
its `is_history` treats exited-yet-attached runs as historic; Windows
has a collapsed `Expander` with unpersisted local state; Swift renders
History always-expanded and its live groups do not exclude non-live
rows (double-listing); Linux has no collapse at all (flat sorted list).
The FFI exposes no `history_expanded` getter/setter, so native shells
cannot share the persisted toggle state. The repo carries agent
instructions in lowercase `agents.md` only (no `AGENTS.md`).

## Goals

- Historic agent sessions are hidden by default (collapsed `History`
  group) on all four UIs; expanding is an explicit user action.
- Historic runs are separated from active runs everywhere: active =
  every row with a live PTY attached in the core registry
  (`staap_is_live`), grouped Needs input → Working → Idle; historic =
  every row with no live PTY, in the collapsed History group.
- Exited-but-attached runs count as active until closed (attached =
  active, even when the child is dead); closing or never-attaching
  puts a row in History. Re-attaching (restart/resume) moves a row
  back to active.
- All grouping, membership, filter-match, and expansion-state logic
  lives in the Rust core; shells are dumb renderers over it.
- Agent instructions are renamed to uppercase `AGENTS.md` and
  strengthened to state the dumb-shell rule explicitly.
- A PR with the change, per `.github/PULL_REQUEST_TEMPLATE.md`,
  opened from `main` with green CI.

## Non-goals

- No new persistence mechanism: the existing config
  `history_expanded` field and `staap_core_save` path are reused.
- No full roster-model API from core (sections-as-JSON snapshot):
  the grouping helpers (`group_sections`, `roster_view`) already
  exist; this change adds only the history membership + expansion
  contract. A full model API is a possible follow-up.
- No per-shell expansion-state divergence: one core-owned toggle,
  persisted once, shared by every shell.
- No behavior change to attention classification, spawn flow, link
  extraction, or transcript handling.

## Design

### 1. Core rule (membership + grouping)

- The single membership rule: a row is historic iff it has no live
  PTY in the run registry (`!reg.is_live(id)`). Attached =
  active, even if the child already exited. Detached (discovered
  sessions, exited-then-closed runs, never-attached rows) =
  historic.
- New registry-level `is_history(id)` helper on `RunRegistry`;
  `ShellView::is_history` (gpui, `src/gui/runs_panel.rs`) is
  repointed at the same registry rule. Behavior change: exited-
  but-attached rows move back from History into the active
  groups until closed.
- Active rows group Needs input → Working → Idle (existing
  `status_sections` / `group_sections` order); historic rows
  render only inside the collapsed-by-default `History (N)`
  group. Expansion state stays core-owned (`App.history_expanded`,
  persisted in config) as today.
- Headless Rust tests pin the rule: attached = active including
  exited-but-attached; detached = historic; restart/resume moves a
  row back to active; toggle round-trips through config.

### 2. Core + FFI seam changes

- Expose `history_expanded` over FFI: a getter plus a toggle/setter
  wired into the existing `staap_core_save` persistence (no new
  persistence path).
- Regenerate `include/staap.h` with cbindgen after the FFI change;
  verify with `cc -fsyntax-only` and the symbol check per
  `docs/native-core-seam.md`.
- Extend the `core_bridge` wrappers (C, for Linux/Windows) and
  `CoreBridge` (Swift) to call the new functions instead of any
  shell-local expansion state.
- Update `docs/native-core-seam.md`: the dumb-shell contract plus
  the C ABI table gain the history membership + expansion rows.

### 3. All four shells adopt (dumb renderers)

- gpui (`src/gui/`): adopt the registry rule; keep the existing
  collapsed header, toggle funnel (`h`/click/Enter), skip-collapsed
  j/k/paging, and Enter-to-reveal behavior.
- Swift (`swift/Sources/StaapMac/`): exclude historic rows from the
  live `statusSections` (fixes the current double-listing); wrap
  history in a collapsed-by-default disclosure bound to core state.
- Linux (`native/linux/src/main.c`): add a collapsible History
  section bound to core state (replacing the always-visible flat
  list tail).
- Windows (`native/windows/src/winui/`): bind the existing
  `HistoryExpander` to core state and persist via the core
  (replacing the unpersisted local state).
- What stays in shells: widgets, disclosure/expander controls,
  event wiring, clipboard access, focus management, byte
  transport. No grouping, filtering, or expansion logic outside
  the core.

### 4. AGENTS.md rename + strengthen, docs, PR

- `git mv agents.md AGENTS.md`, keeping every existing line, and
  append a strengthened dumb-shell clause: shells own
  widgets/events/transport only; grouping, filter match, history
  membership + expansion, statuses, links, key table, reconciler,
  persistence answers all come from the Rust core (via
  `shell_shared`/FFI for native shells); never fix one shell by
  editing another; `grep gpui` over core stays zero.
- PR per `.github/PULL_REQUEST_TEMPLATE.md` (`Fix #N`, pasted
  `fmt`/`test`/`clippy` output), opened only from `main` with
  green CI.

## Testing

- `cargo test --all-targets` (macOS gate; covers the new
  membership/toggle/persistence tests plus the existing
  history/panel suites in `app.rs`, `gui/runs_panel.rs`,
  `gui/nav.rs`, `config.rs`).
- `cargo fmt --check` and
  `cargo clippy --all-targets -- -D warnings`.
- `swift test` (macOS) and the Linux meson suite.
- cbindgen header regen verified (`cc -fsyntax-only -std=c99
  -Wall include/staap.h` clean + header/binary symbol parity).
- Manual smoke per shell: launch with historic sessions present,
  History collapsed by default; expand, restart a historic row
  (moves to active), close it (back to History); toggle persists
  across restarts.

## Risks

- gpui `is_history` behavior change (exited-but-attached rows move
  to active): intended per the agreed rule, but reviewers should
  confirm no workflow depended on exited rows hiding in History.
- Swift live-group exclusion changes visible rows for users who
  saw historic sessions double-listed: strictly a fix, but worth
  calling out in the PR.
- Four-shell verification burden: CI covers build/test per OS,
  but visual collapse behavior needs one manual pass per shell.
