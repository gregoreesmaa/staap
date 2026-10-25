# New-session picker (2D launch): folder × CLI + yolo

Status: implemented in the core (`src/launch.rs`) + gpui shell
(`src/gui/picker.rs`); native shells bind the same core model over FFI.

This records the synthesis of the 10-persona review (competitive scans of
VS Code / Windows Terminal / JetBrains / agent harnesses; cognitive-load
and heuristics theory; keyboard-power, newcomer, multi-shell, settings-IA,
autodetect, and visual-vibe passes). Unanimous points kept; conflicts
resolved below.

## Decision

- **Split button.** `+ New` main (`n`) repeats the last launch instantly
  (1 keystroke, unchanged expert path). The `▾` caret (`N`) opens the full
  picker. No modal grid, no 3-axis picker.
- **Two axes only in the picker:** working folder × agent CLI. Yolo is a
  per-run tri-state row (default / on once / off once), never a third
  picker dimension and never a global master switch (destructive flags
  differ per CLI and must stay opt-in).
- **Core owns everything portable:** catalog + autodetect (`launch`),
  last-used/default resolution, folder MRU, yolo defaults, spawn-command
  synthesis (`App::spawn_command_for`), validation. Shells render natively
  and forward keys — they never build `argv` or probe `PATH` themselves.
- **Fail visible:** missing CLIs stay listed as `not installed` (never
  hidden); missing folders refuse with an inline `no such folder` flash
  and stay open for a fix; zero detected CLIs keeps the historic `muse`
  spawn so the sticky error + Retry names the recovery.

## Interaction (gpui reference)

- `n` / `+ New`: repeat-last (folder + CLI + yolo default), focus
  terminal. `N` / `▾` / `Choose folder × CLI (N)`: open picker.
- Picker keys: type to refine folder, Up/Down steps recents, Left/Right
  or `c` steps CLI, Tab jumps folder ↔ CLI axis, `y` cycles yolo
  (default → on once → off once), Enter confirms, Esc cancels with zero
  side effects. Pane jumps (Cmd+1/2) drop the picker likewise.
- Status line while open: `new run: {folder} · {cli} ({ready|not
  installed}) · yolo: {label} · Tab: axis · Enter: start · Esc: cancel ·
  runs: {preview}`. Confirm flashes `runs: {preview}[ · yolo]`.
- The `w` folder capture stays as the inline single-axis shortcut; the
  picker preselects the first recent folder when no default is set.

## Settings home

- No settings window. Two homes only: at point of use (the picker:
  per-run CLI/folder/yolo override, never auto-written back) and the
  flat local config file (`~/.config/staap/config.json`).
- New keys (all serde-defaulted, old files load unchanged):
  `default_cli`, `default_cwd`, `last_cli` (repeat memory, refreshed on
  every confirmed spawn), `recent_folders` (MRU, capped at 10),
  `agents.<name>.yolo` (per-agent default, off unless set).
- Yolo split: global = none (unsafe); default = per-agent persisted;
  per-run = picker tri-state, ephemeral. Canonical flags: `muse
  --yolo`, `claude --dangerously-skip-permissions`; codex/opencode carry
  no picker flag until verified against a live binary.

## Autodetect

- Cheap `which`-style probe on picker open (exact name, then `PATHEXT`
  on Windows) over `muse, claude, opencode, codex` in order, plus
  GUI-sparse extra dirs (macOS `/opt/homebrew`, Linux `/snap/bin` and
  flatpak exports, user `~/.local/bin`). No `--version` subprocess, no
  background threads, no cache file — re-run on open so the list never
  goes stale. Zero cost at idle.
- Remote hosts probe differently: one bounded `ssh -T -o BatchMode=yes
  -o ConnectTimeout=5 <host>` round trip running `command -v` per CLI
  (see `remote_probe_command` / `parse_remote_probe_output`). Blank hosts
  and every probe failure degrade to all-listed-none-available, never an
  empty picker; shells run it off the UI thread on host pick and show the
  unchecked catalog meanwhile.
- Rows show all four CLIs in fixed order with `ready` / `not installed`;
  spawn of a missing CLI stays possible (explicit pick) and fails
  visibly at spawn with Retry, like today.

## Remote sessions (SSH)

One manual host, no remote daemon: the local PTY runs `ssh -t <host>
<cli…>` and the agent TUI renders through it. Auth rides the user's own
ssh setup only (`~/.ssh/config`, agents, `user@`, jump aliases all keep
resolving; interactive password/2FA/host-key prompts surface in the
terminal). The spawn deliberately carries none of the probe-only flags
(`-T`, `BatchMode=yes`, timeouts) — a spawn must surface auth, never
fail it silently.

- Core owns everything portable: `SshTarget { host, remote_cwd }` +
  `ssh_spawn_command` (`launch`), the `SpawnKind::Remote` wrapper and its
  `ssh -t` synthesis in `App::spawn_command_for` (extra_args + yolo still
  apply to the inner CLI), per-run target on `ChatSession`, per-host
  folder recents + last host in `Config`, FFI `staap_run_spawn_remote` /
  `staap_remote_recent_json`. Shells only render.
- The remote folder never touches the local PTY cwd: without one the
  spawn is `ssh -t <host> <cli> <args…>`; with one it folds into
  `ssh -t <host> 'cd <dir> && exec <cli> …'` (quoted). Preview reads
  `ssh <host>: <cli> [--flag] in <dir>`.
- Persisted state is host + folder only (serde-defaulted, old files load
  as local; restarts redial the stored target). Passwords, keys, and
  passphrases never touch staap state. Blank hosts refuse at spawn with a
  fail-visible error; discovery degrades to all-listed-none-available.

## SSH picker (saved hosts + tester)

The add-dialog model for picking a remote host lives in the core once;
shells render rows and forward intents, never probe or resolve.

- Saved hosts (`Config::saved_hosts`, `MAX_SAVED_HOSTS = 50`): display
  name + SSH target (`user@host` or an ssh-config alias) + optional
  per-host default folder. Add updates in place on a repeated host;
  rename (`Config::rename_saved_host`, keyed by current host or display
  name) edits label + target + folder in place — the typo fix,
  cap-exempt, saved order kept; remove matches host or display name;
  past the cap only brand-new hosts refuse. Folder memory: a rename
  moves the per-host folder MRU to the new target (merged ahead of any
  MRU the target already earned, still capped; `last_host` retargets
  when it pointed at the old host), while a delete drops that host's
  MRU and clears `last_host` when it pointed there — rename keeps,
  delete forgets. Old files without the key load empty; no secret field
  exists — auth stays in the system ssh config.
- Shell affordances (every shell renders these over the same core
  entries): the machine list dials a row on pick and offers Edit (the
  add dialog prefilled; saving renames) plus Delete/Remove (core remove
  + persist, falling back to This machine when the picked host dies).
  Every action is keyboard-reachable and reported in words, never
  color-only.
- Default folder (`launch::resolve_remote_default_folder`,
  `Config::remote_default_folder_for`): saved host default, then the
  host MRU head (`remote_folders`), then the remote home (`None`).
- Folder completion (`launch::remote_list_command` /
  `parse_remote_list_output` / `list_remote_dir`): one bounded
  non-interactive `ssh` round trip (`-T`, `BatchMode=yes`,
  `ConnectTimeout=5`, safe-quoted `cd <dir> && exec ls -1A -p`,
  capped at `MAX_REMOTE_LIST_ENTRIES = 500`), degrading to empty on
  every failure.
- Tester (`launch::test_remote_host`): one bounded round trip reporting
  `reachable` plus the detected CLI catalog — unreachable and the
  all-listed-none-available catalog on blank hosts and every failure,
  never an auth hang.
- FFI: `staap_saved_hosts_json`, `staap_saved_host_add`,
  `staap_saved_host_rename`, `staap_saved_host_remove`,
  `staap_remote_default_folder`, `staap_remote_list_json`,
  `staap_remote_test_json` (header regenerated with cbindgen; persist
  via `staap_core_save`).

## Visual / triage

- Rows keep the harness badge (glyph + short tag) + folder tail; picker
  preview is the text `runs: {cli} [--flag] in {dir}` — verifiable
  before launch, grayscale-safe, keyboard-reachable.
- A yolo run flashes `· yolo` in the confirm status; list-level yolo
  badging stays a follow-up (row suffix `[yolo]` + `△` marker proposal).

## Conflicts resolved

- Picker vs instant `n` (power vs newcomer): split button keeps both —
  `n` never opens UI (persona 5 veto), `N`/caret always does.
- Yolo in picker vs settings-only (personas 1 vs 5): tri-state row in
  the picker defaulting from per-agent config — one-key `y` at spawn
  time, safe default, no global switch.
- Modal vs popover vs inline (persona 7): core state machine is
  widget-agnostic; gpui renders it as the status-line capture (same as
  the `w`/`/` captures), native shells map it to Menu/Popover/MenuFlyout.
- Grid (persona 2) vs menu (personas 1, 10): menu/list wins — fits the
  264px sidebar, narrow mode, and grayscale parity; the preview line
  replaces the grid cell.
- Version probing (persona 9's 2s probe): deferred — cheap PATH scan
  only for now; `--version` verification is a background follow-up.

## Follow-ups (not in this change)

- List-level `[yolo]` row suffix for triage parity.
- `R` rescan affordance + `--version` background verification.
- Settings-menu UI for per-agent yolo defaults (config-file only today).
- cbindgen header regen for the new FFI entry points.

## Native shells (landed, dumb renderers over the core registry)

All three native shells are dumb renderers over the core run registry:
spawns attach real roster rows via `staap_run_spawn` (repeat-last and picker
confirm share one funnel), restart/resume via `staap_run_restart`, close via
`staap_run_close`, statuses/links refresh via `staap_pump_all`, and persistence
is automatic (throttled autosave + close hooks — no Save buttons, no
theme pickers; every shell follows the system appearance). The preview
(`staap_spawn_preview`), yolo mapping (`staap_yolo_value`), key table
(`staap_key_encode`), feed reconciler (`staap_feed_delta`), SGR renderer
(`staap_ansi_render`), age/glyph/headers, filter/selection, and sidebar
clamp each live once in the core — the per-shell C/Swift ports are
deleted, and shells call the core instead of reimplementing it:

- Swift (`swift/`): split Menu (repeat-last + picker sheet with folder
  field/recents, CLI radio, tri-state yolo, core preview); widget state
  in `NewSessionPicker` (pinned by `swift test`), rules in the core.
- GTK (`native/linux/`): linked New-run + ▾ caret (Ctrl+N / Ctrl+Shift+N);
  AdwDialog picker over the bridge wrappers (pinned by `staap-picker-test`);
  smoke asserts the catalog + shared-helper surface.
- WinUI (`native/windows/`): New Session repeat + Choose dialog
  (ContentDialog with folder/CLI/yolo + core preview, Ctrl+N /
  Ctrl+Shift+N); catalog/recents parse stays native for ComboBox rows
  (pinned by `staap-win-picker-test`), rules in the core; smoke asserts
  the catalog + shared-helper surface.
