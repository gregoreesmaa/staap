//! staap (gpui shell, macOS-only): live agent runs beside an embedded PTY.
//!
//! Left panel: live runs started in-app (`n` or the New button); starts
//! empty, shows animal placeholder titles until the first submitted prompt
//! renames a run, groups by Needs input / Working / Idle with counts (then
//! History), and lists every link ever seen in each run (accumulated, not
//! just the visible screen). Central pane: the embedded interactive agent
//! terminal, rendered from the vt100 emulator grid; drag to highlight text
//! (copy-on-select), Cmd+C copies, Cmd/Ctrl+V pastes.
//! Keys (nav focus): j/k move, n new, y copy selection-or-screen, p paste,
//! Tab/i type, ? help, q quit. Typing focus: keys go to `muse`; Tab/Esc back to
//! the list; Cmd+1/Cmd+2 jump to either pane from anywhere.

// Core lives in the library target (`src/lib.rs`, issue #60) so native
// shells bind one crate; this binary consumes it like any other client.
// The re-export keeps the existing `crate::<module>` paths in `gui/`
// working with no module moves.
//
// macOS-only binary (issues #63/#64): the `gpui` / `gpui-component`
// deps are macOS-scoped in Cargo.toml, so the whole shell below is
// gated on `target_os = "macos"`. On other targets the binary is a
// stub that fails loud at runtime; `cargo test --all-targets` stays
// green on every runner because the portable gate is the lib suite
// (each per-OS shell proves itself in its own CI job).
#[cfg(target_os = "macos")]
pub use staap::{
    app, config, embedded, keys, launch, parsers, persist, providers, scrollback, shell_shared,
    transcript,
};
#[cfg(target_os = "macos")]
mod gui;

#[cfg(target_os = "macos")]
use gpui::{AppContext, Application, Entity};
#[cfg(target_os = "macos")]
use gpui_component::{Root, Theme};

#[cfg(target_os = "macos")]
use gui::shell::ShellView;
#[cfg(target_os = "macos")]
use providers::{
    AntigravityCliProvider, ClaudeCliProvider, CodexCliProvider, MuseCliProvider,
    OpencodeCliProvider, Provider,
};

#[cfg(target_os = "macos")]
fn main() {
    // Historic attach: provider-discovered sessions seed the list before
    // any PTY exists (unreachable stores degrade to an empty list, never
    // a startup failure). `r` on a seeded entry re-attaches it. Muse,
    // opencode, Claude Code, codex, and Antigravity stores merge here; ids
    // are provider-scoped (muse session dirs, opencode `ses-*` ids, claude
    // `sessionId`s, codex rollout `session_id`s, agy conversationIds), and
    // `merge_sessions` keeps one row per id: opencode rows re-attach via
    // `opencode --session <id>`, claude rows via `claude --resume <id>`,
    // codex rows via `codex resume <id>`, antigravity rows via
    // `agy --conversation <id>`.
    let mut discovered = MuseCliProvider::new(
        MuseCliProvider::default_store_root(),
        Box::new(parsers::registry::RegistryParser::default()),
    )
    .discover_sessions();
    discovered.extend(
        OpencodeCliProvider::with_default_program(Box::new(
            parsers::registry::RegistryParser::default(),
        ))
        .discover_sessions(),
    );
    discovered.extend(
        ClaudeCliProvider::new(
            ClaudeCliProvider::default_store_root(),
            Box::new(parsers::registry::RegistryParser::default()),
        )
        .discover_sessions(),
    );
    discovered.extend(
        CodexCliProvider::new(
            CodexCliProvider::default_store_root(),
            Box::new(parsers::registry::RegistryParser::default()),
        )
        .discover_sessions(),
    );
    discovered.extend(
        AntigravityCliProvider::new(
            AntigravityCliProvider::default_store_root(),
            Box::new(parsers::registry::RegistryParser::default()),
        )
        .discover_sessions(),
    );
    // Issues #33/#34: user config (per-agent flags, theme choice); a
    // missing file means plain `muse` + follow-system theme.
    let config = config::Config::load();
    let startup_theme = config.theme;
    // Persisted links/transcripts merge under discovery (issue #26):
    // a missing or corrupt file degrades to discovery alone.
    let seeded = persist::merge_sessions(discovered, persist::load_sessions());
    Application::new().run(move |cx| {
        // Native chrome components (sidebar, buttons): init once. The
        // theme applies per window below so `system` can follow the OS.
        gpui_component::init(cx);
        let view: Entity<ShellView> = cx.new(|_cx| {
            let mut shell = ShellView::new_with_sessions(seeded.clone());
            shell.app.set_config(config.clone());
            shell
        });
        let pump_view = view.clone();
        // Pump loop: poll PTYs at 20 Hz so background runs keep streaming;
        // the window repaints only when the pump reports dirtiness (fresh
        // output, exit, spawn, status/link/row change), so idle costs ~zero.
        cx.spawn(async move |cx| loop {
            cx.background_executor()
                .timer(gui::shell::PUMP_INTERVAL)
                .await;
            let alive = cx.update(|cx| {
                pump_view.update(cx, |view, cx| {
                    if view.tick() {
                        cx.notify();
                    }
                })
            });
            if alive.is_err() {
                break;
            }
        })
        .detach();
        // Root must be the window's first view: it provides the theme
        // context the sidebar/buttons read, plus dialog/notification layers.
        // Issue #34: apply the configured theme first (explicit dark/light
        // or the live OS appearance) so chrome renders in it immediately.
        cx.open_window(gui::view::window_options(), |window, cx| {
            Theme::change(
                gui::theme::theme_mode_for(startup_theme, window.appearance()),
                Some(window),
                cx,
            );
            cx.new(|cx| Root::new(view.clone(), window, cx))
        })
        .unwrap();
    });
}

/// Non-macOS stub: the gpui shell runs on macOS only. Windows and Linux
/// have their own native shells over the core C ABI (`native/windows`,
/// `native/linux`); this stub exists so the binary target — and with it
/// `cargo test --all-targets` — still builds on every runner.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!(
        "staap: the gpui shell runs on macOS only; on Windows use \
         the WinUI shell (native/windows) and on Linux the GTK shell \
         (native/linux), both over the core C ABI."
    );
    std::process::exit(1);
}
