# History Collapse Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Historic sessions are hidden in a collapsed History group by default on all four UIs, with membership and expansion owned by the Rust core.

**Architecture:** Add a registry-level `is_history` rule plus `history_expanded` FFI to the core, repoint gpui at the registry rule, bind all three native shells to the core state, and rename/strengthen the agent instructions file.

**Tech Stack:** Rust core (`src/runs.rs`, `src/ffi.rs`), gpui shell (`src/gui/`), SwiftUI (`swift/`), C bridges + GTK4 (`native/linux/`), C++ bridge + WinUI (`native/windows/`), cbindgen header (`include/staap.h`).

**Spec:** `docs/superpowers/specs/2026-10-03-history-collapse-design.md`

## Global Constraints

- Active = row with a live PTY attached in the core registry (`staap_is_live`); attached-but-exited stays active until closed.
- Historic = row with no live PTY; renders only inside the collapsed-by-default `History (N)` group; active groups are Needs input → Working → Idle.
- No grouping, filtering, or expansion logic outside the Rust core; shells own widgets, event wiring, clipboard, focus, byte transport only.
- Never fix one shell by editing another.
- Reuse the existing config `history_expanded` field and `staap_core_save` persistence; no new persistence mechanism.
- Regenerate `include/staap.h` with cbindgen after any FFI change; `cc -fsyntax-only -std=c99 -Wall include/staap.h` stays clean with header/binary symbol parity.
- Gates: `cargo test --all-targets`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `swift test`, Linux meson suite.
- PR per `.github/PULL_REQUEST_TEMPLATE.md`, opened only from `main` with green CI.

## Review Focus

- Exited-but-attached row must still render in its active status group, not History, on every shell.
- Restart/resume of a historic row must move it to active immediately (before first output).
- Toggling History expansion on any shell must persist across restarts via the core config.
- A filtered-out historic selection must keep working (Enter expands to reveal it on gpui; native shells keep core selection intact).
- `staap_is_live` on unknown ids and null FFI args must stay null-safe (false), never UB.

---

### Task 1: Core history rule + tests

**Files:**
- Modify: `src/runs.rs`
- Test: `src/runs.rs` (module tests), `src/app.rs` (existing history tests keep passing)

**Interfaces:**
- Consumes: `RunRegistry.is_live(&str) -> bool`, `App.history_expanded`, `App.toggle_history()`
- Produces: `RunRegistry::is_history(&self, id: &str) -> bool` (historic iff no live PTY attached)

- [ ] **Step 1: Write the failing test** in `src/runs.rs` module tests: attach a PTY-holding entry and assert `!is_history`; an entry with no PTY asserts `is_history`; an exited-but-attached run asserts `!is_history` (active until closed).
- [ ] **Step 2: Run it to verify it fails.** Run: `cargo test --lib runs::` Expected: FAIL (no such method).
- [ ] **Step 3: Implement `pub fn is_history(&self, id: &str) -> bool` on `RunRegistry`** returning `!self.is_live(id)`, with a doc comment stating the active-until-closed rule.
- [ ] **Step 4: Run tests to verify they pass.** Run: `cargo test --lib` Expected: PASS.
- [ ] **Step 5: Commit.** `git add src/runs.rs` / `git commit -m "feat: core registry owns history membership (attached = active)"`

### Task 2: History expansion over FFI + header regen

**Files:**
- Modify: `src/ffi.rs` (new `staap_history_expanded` getter + `staap_set_history_expanded` setter/toggle following the `staap_set_filter` null-safety pattern)
- Modify: `include/staap.h` (cbindgen regen)
- Test: `src/ffi.rs` (module tests, mirroring the `staap_set_filter` round-trip test)

**Interfaces:**
- Consumes: `RunRegistry::is_history` (Task 1), `App.history_expanded`, `App.toggle_history()`, `staap_core_save` persistence
- Produces: `staap_history_expanded(core) -> bool` (null core = false); `staap_set_history_expanded(core, expanded)` (null core = no-op); optionally `staap_is_history(core, id)` per-id membership for shells that need it

- [ ] **Step 1: Write the failing test** in `src/ffi.rs` module tests: default expanded is false; set true, assert getter true; null core getter false, setter no-op.
- [ ] **Step 2: Run it to verify it fails.** Run: `cargo test --lib ffi::` Expected: FAIL (symbols missing).
- [ ] **Step 3: Implement the new `extern "C"` functions** in the filter/selection section of `src/ffi.rs`, with `# Safety` docs matching neighboring functions.
- [ ] **Step 4: Run tests to verify they pass.** Run: `cargo test --lib ffi::` Expected: PASS.
- [ ] **Step 5: Regenerate the header** with `cbindgen --config cbindgen.toml --crate staap --output include/staap.h`; verify `cc -fsyntax-only -std=c99 -Wall include/staap.h` clean and header/binary symbol parity per `docs/native-core-seam.md`.
- [ ] **Step 6: Commit.** `git add src/ffi.rs include/staap.h` / `git commit -m "feat: expose history expansion over FFI"`

### Task 3: gpui shell adopts the registry rule

**Files:**
- Modify: `src/gui/runs_panel.rs` (`ShellView::is_history`)
- Test: existing `src/gui/runs_panel.rs` + `src/gui/nav.rs` suites (update exited-but-attached expectations to active)

**Interfaces:**
- Consumes: `RunRegistry::is_history` (Task 1)
- Produces: unchanged UI behavior except exited-but-attached rows render in active groups

- [ ] **Step 1: Update the failing expectations**: adjust `runs_panel.rs` tests asserting exited-but-attached is history to assert active instead.
- [ ] **Step 2: Run to verify they fail.** Run: `cargo test --all-targets gui::` Expected: FAIL on the adjusted tests.
- [ ] **Step 3: Repoint `ShellView::is_history`** at the registry rule (attached = active, even when exited; detached = historic), keeping the fresh-pending-spawn behavior (no PTY yet, no provider id → active).
- [ ] **Step 4: Run tests to verify they pass.** Run: `cargo test --all-targets` Expected: PASS.
- [ ] **Step 5: Commit.** `git add src/gui/` / `git commit -m "fix: gpui history follows registry rule (exited-but-attached stays active)"`

### Task 4: Swift shell — collapse History, fix double-listing

**Files:**
- Modify: `swift/Sources/StaapMac/ContentView.swift` (collapsed-by-default disclosure bound to core state; exclude historic rows from live `statusSections`)
- Modify: `swift/Sources/StaapMac/CoreBridge.swift` + `swift/Sources/StaapMac/AppState.swift` (history expansion via new FFI, no local state)
- Test: `swift test` (extend roster tests for exclusion + default-collapsed)

**Interfaces:**
- Consumes: `staap_history_expanded` / `staap_set_history_expanded` / `staap_is_live` (Task 2)
- Produces: History collapsed by default; live groups contain only attached rows

- [ ] **Step 1: Write the failing test** (Swift roster test): a non-live row appears in history rows and in no live status group; default expansion false.
- [ ] **Step 2: Run to verify it fails.** Run: `cd swift && swift test` Expected: FAIL.
- [ ] **Step 3: Implement**: filter historic rows out of `statusSections`, wrap History in a collapsed-by-default disclosure bound to core expansion state via `CoreBridge`.
- [ ] **Step 4: Run to verify it passes.** Run: `cd swift && swift test` Expected: PASS.
- [ ] **Step 5: Commit.** `git add swift/` / `git commit -m "fix: Swift history collapsed by default, live groups exclude historic rows"`

### Task 5: Linux shell — collapsible History bound to core

**Files:**
- Modify: `native/linux/src/main.c` (collapsible History section via `bridge_is_live` grouping + new history-expansion bridge), `native/linux/src/core_bridge.c/h` (wrappers for the new FFI)
- Test: `staap-feed-test` / `picker_test` meson suites keep passing; add a bridge-level check for expansion round-trip

**Interfaces:**
- Consumes: new FFI from Task 2 via `core_bridge.h`
- Produces: History section collapsed by default, toggle persists via core

- [ ] **Step 1: Write the failing check** in the Linux test suite for expansion default-false + set/get round-trip through the bridge.
- [ ] **Step 2: Run to verify it fails.** Run: meson suite per `native/linux/README.md` Expected: FAIL.
- [ ] **Step 3: Implement** the collapsible History section + bridge wrappers; historic tail of the flat list moves into the collapsed section.
- [ ] **Step 4: Run to verify it passes.** Run: meson suite Expected: PASS.
- [ ] **Step 5: Commit.** `git add native/linux/` / `git commit -m "feat: Linux history collapses by default via core state"`

### Task 6: Windows shell — bind Expander to core + persist

**Files:**
- Modify: `native/windows/src/winui/MainWindow.xaml.cpp` (bind `HistoryExpander` to core expansion state, persist via core save), `native/windows/src/core_bridge.c/h` (wrappers)
- Test: CTest suite keeps passing; add expansion round-trip check mirroring Linux

**Interfaces:**
- Consumes: new FFI from Task 2 via `core_bridge.h`
- Produces: `HistoryExpander` reflects core state; toggle persists across restarts

- [ ] **Step 1: Write the failing check** for expansion default-false + round-trip through the Windows bridge.
- [ ] **Step 2: Run to verify it fails.** Run: CTest per `native/windows/README.md` Expected: FAIL.
- [ ] **Step 3: Implement** the binding + persist; remove unpersisted local expansion state.
- [ ] **Step 4: Run to verify it passes.** Run: CTest Expected: PASS.
- [ ] **Step 5: Commit.** `git add native/windows/` / `git commit -m "fix: Windows history expander follows persisted core state"`

### Task 7: AGENTS.md rename + strengthen, seam doc, full gates

**Files:**
- Rename: `agents.md` → `AGENTS.md` via `git mv`
- Modify: `AGENTS.md` (append strengthened dumb-shell clause), `docs/native-core-seam.md` (contract + C ABI table rows for the new FFI)

**Interfaces:**
- Consumes: all prior tasks
- Produces: instruction file + seam doc mirror the core-owned history rule

- [ ] **Step 1: `git mv agents.md AGENTS.md`** and append the clause: shells own widgets/events/transport only; grouping, filter match, history membership + expansion, statuses, links, key table, reconciler, persistence answers come from the Rust core; never fix one shell by editing another; `grep gpui` over core stays zero.
- [ ] **Step 2: Update `docs/native-core-seam.md`** dumb-shell contract + C ABI table with the new history functions.
- [ ] **Step 3: Run the full gates.** Run: `cargo fmt --check`, `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings` Expected: all PASS.
- [ ] **Step 4: Commit.** `git add AGENTS.md docs/native-core-seam.md` / `git commit -m "docs: rename to AGENTS.md, strengthen dumb-shell contract"`

### Task 8: PR

**Files:** none (process task)

- [ ] **Step 1: Verify branch state**: on `main`, `git status` clean except the planned commits; `git log --oneline -10` shows only the intended series.
- [ ] **Step 2: Push and open the PR** per `.github/PULL_REQUEST_TEMPLATE.md` (`Fix #N`, pasted `fmt`/`test`/`clippy` output), opened only with green CI.
- [ ] **Step 3: Report the PR URL** back to the user.
