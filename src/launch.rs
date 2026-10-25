//! Two-dimensional new-session launch: working folder × agent CLI.
//!
//! Framework-free by contract (epic #60): plain structs + pure functions,
//! no toolkit types, so every native shell binds one model. The picker
//! picks a [`LaunchSelection`] (folder + CLI + resolved yolo); the spawn
//! seam turns it into `(program, args, cwd)` with the per-agent yolo flag.
//!
//! Consensus from the 10-persona review (see `docs/new-session-picker.md`):
//! split-button repeat-last (`n` / `+ New` main), full picker on `N` / `▾`,
//! yolo as a per-run tri-state defaulting safe, autodetect before listing.

use serde::{Deserialize, Serialize};

/// Agent CLIs the picker can offer, in default preference order. `muse`
/// stays first: the historic default and the README prerequisite.
pub const SUPPORTED_CLIS: &[&str] = &["muse", "claude", "opencode", "codex"];

/// Cap for the persisted most-recently-used folder list (issue #29 family:
/// bounded display over unbounded data — same rule as link caps).
pub const MAX_RECENT_FOLDERS: usize = 10;

/// Canonical yolo flag per known CLI. `None` means the CLI has no
/// picker-supported yolo flag: the toggle hides instead of guessing.
/// Values match [`crate::embedded::Harness::yolo_flag`] (verified against
/// live CLI references: `muse --yolo`, `claude
/// --dangerously-skip-permissions`, `codex
/// --dangerously-bypass-approvals-and-sandbox`, `opencode --auto`);
/// unknown ids map to `None` so future harnesses never get a guessed flag.
pub fn yolo_flag_for(cli: &str) -> Option<&'static str> {
    match cli {
        "muse" => Some("--yolo"),
        "claude" => Some("--dangerously-skip-permissions"),
        "codex" => Some("--dangerously-bypass-approvals-and-sandbox"),
        "opencode" => Some("--auto"),
        _ => None,
    }
}

/// One CLI row in the picker: always listed, never silently dropped.
/// `path` is `Some` when the binary resolves on this host (autodetected),
/// `None` when it is missing from `PATH`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableCli {
    pub id: String,
    pub program: String,
    pub path: Option<String>,
    pub available: bool,
}

impl AvailableCli {
    pub fn missing(id: &str) -> Self {
        Self {
            id: id.to_string(),
            program: id.to_string(),
            path: None,
            available: false,
        }
    }
}

/// Autodetect the supported CLIs against the process `PATH` (plus a few
/// GUI-sparse extra dirs, see [`extra_search_dirs`]). Cheap `which`-style
/// probe only — no `--version` subprocess, no background threads, no
/// cache: callers re-run it on picker open so the list is never stale.
/// Order follows [`SUPPORTED_CLIS`].
pub fn detect_available_clis() -> Vec<AvailableCli> {
    detect_with_path(&std::env::var_os("PATH").unwrap_or_default())
}

/// Pure half of [`detect_available_clis`] over an explicit `PATH` value
/// (headlessly testable on every OS).
pub fn detect_with_path(path_value: &std::ffi::OsStr) -> Vec<AvailableCli> {
    let exts = pathext_suffixes();
    let mut out = Vec::with_capacity(SUPPORTED_CLIS.len());
    for id in SUPPORTED_CLIS {
        let found = resolve_bare_name(id, path_value, &exts).or_else(|| {
            extra_search_dirs()
                .iter()
                .find_map(|d| probe_dir(d, id, &exts))
        });
        let available = found.is_some();
        out.push(AvailableCli {
            id: id.to_string(),
            program: id.to_string(),
            path: found.map(|p| p.to_string_lossy().into_owned()),
            available,
        });
    }
    out
}

/// Remote shell snippet probing every supported CLI with `command -v`,
/// one `id:path` line per CLI (`id:` when missing). Built from
/// [`SUPPORTED_CLIS`] so the probe and the picker never drift apart.
pub fn remote_probe_script() -> String {
    let mut script = String::from("for c in");
    for id in SUPPORTED_CLIS {
        script.push(' ');
        script.push_str(id);
    }
    script.push_str(
        "; do p=$(command -v \"$c\" 2>/dev/null); \
         if [ -n \"$p\" ]; then printf '%s:%s\\n' \"$c\" \"$p\"; \
         else printf '%s:\\n' \"$c\"; fi; done",
    );
    script
}

/// Remote probe for Windows SSH hosts (PowerShell default shell): emits
/// the same `id:path-or-empty` rows as [`remote_probe_script`], so
/// [`parse_remote_probe_output`] parses either spelling. `Get-Command`
/// resolves through the remote `PATH` exactly like `command -v` does on
/// POSIX remotes; rows split on the first `:` downstream, so Windows
/// `C:\…` paths survive intact.
pub fn remote_ps_probe_script() -> String {
    // IDs ride single-quoted: bare words are commands in expression
    // context, so unquoted ids are a parse error on the remote.
    let mut script = String::from("foreach ($c in ");
    script.push_str(
        &SUPPORTED_CLIS
            .iter()
            .map(|id| format!("'{id}'"))
            .collect::<Vec<_>>()
            .join(", "),
    );
    script.push_str(
        ") { $p = Get-Command $c -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source; if ($p) { Write-Output ($c + ':' + $p) } else { Write-Output ($c + ':') } }",
    );
    script
}

/// `ssh` argv probing `host` for the supported CLIs: no pty (`-T`),
/// non-interactive (`BatchMode=yes`, so the probe never blocks on a
/// password prompt), and a short connect timeout. The interactive spawn
/// path deliberately uses none of these: auth prompts surface in the
/// terminal there. `host` passes through untouched so system ssh config
/// (`~/.ssh/config`, `user@`, jump aliases) keeps resolving.
pub fn remote_probe_command(host: &str) -> (String, Vec<String>) {
    (
        "ssh".to_string(),
        vec![
            "-T".to_string(),
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "ConnectTimeout=5".to_string(),
            host.to_string(),
            remote_probe_script(),
        ],
    )
}

/// Parse one remote probe run into the picker catalog: same rows and order
/// as [`detect_available_clis`], availability from the remote `PATH`.
/// Unknown ids are ignored, blank/malformed lines are skipped, the first
/// hit per id wins, and missing ids stay listed as not installed (never
/// hidden — same fail-visible rule as the local catalog).
pub fn parse_remote_probe_output(text: &str) -> Vec<AvailableCli> {
    use std::collections::HashMap;
    let mut found: HashMap<&str, Option<String>> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((id, path)) = line.split_once(':') else {
            continue;
        };
        let id = id.trim();
        if !SUPPORTED_CLIS.contains(&id) || found.contains_key(id) {
            continue;
        }
        let path = path.trim();
        found.insert(
            id,
            if path.is_empty() {
                None
            } else {
                Some(path.to_string())
            },
        );
    }
    SUPPORTED_CLIS
        .iter()
        .map(|id| match found.remove(*id) {
            Some(Some(path)) => AvailableCli {
                id: id.to_string(),
                program: id.to_string(),
                path: Some(path),
                available: true,
            },
            _ => AvailableCli::missing(id),
        })
        .collect()
}

/// Run the remote probe over `ssh` and parse it. Blank hosts and every
/// failure (no `ssh` binary, unreachable host, key-only auth without
/// keys, non-zero exit) degrade to the all-listed-none-available catalog:
/// discovery never fails, the picker stays usable, and the spawn error
/// surfaces visibly at launch like a missing local CLI.
///
/// Blocking note for shells: this runs one `ssh` round trip (bounded by
/// the connect timeout in [`remote_probe_command`]). Call it when the
/// remote host is picked — not on every local picker open — and off the
/// UI thread, showing [`parse_remote_probe_output`]`("")` meanwhile.
pub fn detect_remote_clis(host: &str) -> Vec<AvailableCli> {
    let target = host.trim();
    if target.is_empty() {
        return parse_remote_probe_output("");
    }
    match run_remote_posix_then_ps(target, &remote_probe_script(), &remote_ps_probe_script()) {
        Some(text) => parse_remote_probe_output(&text),
        None => parse_remote_probe_output(""),
    }
}

/// One `ssh` round trip for the read-only remote probes
/// ([`detect_remote_clis`], [`list_remote_dir`], [`test_remote_host`]):
/// `Some` stdout only on a zero-exit run, `None` on every failure (spawn
/// error, signal, non-zero exit). Split from parsing so tests cover the
/// pure halves without touching the network.
fn run_remote_command(host: &str, script: &str) -> Option<String> {
    let out = std::process::Command::new("ssh")
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            host,
            script,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// One `ssh` round trip with a PowerShell fallback for Windows SSH hosts:
/// try the POSIX spelling first, and only when the remote shell rejects
/// it (non-zero exit — e.g. a PowerShell/cmd default shell choking on
/// `&&` or `command -v`) run the PowerShell spelling. Both spellings
/// share one wire format, so callers parse either output identically.
/// POSIX remotes pay one round trip as before; Windows remotes pay a
/// second, fast-failing round trip per user action (probes and listings,
/// never per-tick).
fn run_remote_posix_then_ps(host: &str, posix_script: &str, ps_script: &str) -> Option<String> {
    match run_remote_command(host, posix_script) {
        Some(text) => Some(text),
        None => run_remote_command(host, ps_script),
    }
}

/// One remote directory entry from the listing probe (SSH picker): `name`
/// without any `ls` indicator suffix, `is_dir` from the trailing `/`
/// that `ls -p` appends to directories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
}

/// Cap for one parsed remote listing (SSH picker): bounded display over
/// unbounded remote dirs — same rule as the folder MRU and link caps.
pub const MAX_REMOTE_LIST_ENTRIES: usize = 500;

/// Remote shell snippet listing one directory: `ls -1A -p` (one name per
/// line, directories with a trailing `/`, dotfiles included, `.`/`..`
/// excluded). The `cd` runs first so a missing dir fails before `ls`
/// emits anything (degrading to empty downstream); the path is
/// single-quote wrapped (see [`SshTarget`] quoting) so spaces and quotes
/// survive the remote shell. Names containing newlines cannot round-trip
/// through line output and split into two rows — an accepted `ls`
/// limitation, documented here rather than silently worked around.
pub fn remote_list_script(path: &str) -> String {
    format!("{} && exec ls -1A -p", remote_cd_snippet(path))
}

/// Quote one word for a PowerShell remote snippet: single-quote wrap
/// with embedded quotes doubled (`'` → `''`). Backslashes stay literal —
/// single quotes never escape in PowerShell.
fn ps_quote(word: &str) -> String {
    let mut out = String::with_capacity(word.len() + 2);
    out.push('\'');
    out.push_str(&word.replace('\'', "''"));
    out.push('\'');
    out
}

/// `Set-Location` snippet for one remote directory (Windows SSH hosts):
/// absolute paths ride single-quoted (see [`ps_quote`]); a leading `~`
/// rewrites to `$HOME` in double quotes, mirroring [`remote_cd_snippet`]
/// — PowerShell's own `~` handling varies across 5.1/7+, so the rewrite
/// keeps home resolution explicit. Inside double quotes only the
/// backtick, `$`, and `"` escape (backtick-prefixed); `\` is literal,
/// and `/` separators work as-is.
fn remote_ps_cd_snippet(dir: &str) -> String {
    let mut snippet = String::from("Set-Location -LiteralPath ");
    if let Some(rest) = dir.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') {
            snippet.push_str("\"$HOME");
            for c in rest.chars() {
                match c {
                    '`' | '$' | '"' => {
                        snippet.push('`');
                        snippet.push(c);
                    }
                    _ => snippet.push(c),
                }
            }
            snippet.push('"');
            return snippet;
        }
    }
    snippet.push_str(&ps_quote(dir));
    snippet
}

/// Remote shell snippet listing one directory on a Windows SSH host
/// (PowerShell default shell): best-effort UTF-8 output, a strict
/// directory change, then one `Name` per line with `/` appended to
/// directories — the same wire format as [`remote_list_script`], so
/// [`parse_remote_list_output`] parses either spelling.
/// `-ErrorAction Stop` makes a missing dir exit non-zero (degrading to
/// empty downstream, like the POSIX `cd` first); without it
/// `Get-ChildItem` would list the *previous* directory on failure.
pub fn remote_ps_list_script(path: &str) -> String {
    format!(
        "try {{ [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 }} catch {{}}; {} -ErrorAction Stop; Get-ChildItem -LiteralPath . -Force | ForEach-Object {{ $_.Name + $(if ($_.PSIsContainer) {{ '/' }}) }}",
        remote_ps_cd_snippet(path)
    )
}

/// `ssh` argv listing `path` on `host` (SSH picker): no pty (`-T`),
/// non-interactive (`BatchMode=yes`, so the probe never blocks on a
/// password prompt), and a short connect timeout — a single round trip.
/// The host passes through untouched so system ssh config (`~/.ssh/config`,
/// `user@`, jump aliases) keeps resolving.
pub fn remote_list_command(host: &str, path: &str) -> (String, Vec<String>) {
    (
        "ssh".to_string(),
        vec![
            "-T".to_string(),
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "ConnectTimeout=5".to_string(),
            host.to_string(),
            remote_list_script(path),
        ],
    )
}

/// Parse one remote listing run (`ls -1A -p`, one name per line,
/// directories with a trailing `/`) into entries: blank lines and
/// `.`/`..` are skipped, names keep every other byte (no trimming —
/// remote names may carry spaces), output caps at
/// [`MAX_REMOTE_LIST_ENTRIES`], and entries sort directories-first,
/// case-insensitively — the native Finder/Explorer/Nautilus order every
/// shell's remote browser inherits with no per-shell sort. Pure so tests
/// cover it without a network.
pub fn parse_remote_list_output(text: &str) -> Vec<RemoteEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line == "." || line == ".." || line == "./" || line == "../" {
            continue;
        }
        let (name, is_dir) = match line.strip_suffix('/') {
            Some(dir) if !dir.is_empty() => (dir, true),
            _ => (line, false),
        };
        out.push(RemoteEntry {
            name: name.to_string(),
            is_dir,
        });
        if out.len() >= MAX_REMOTE_LIST_ENTRIES {
            break;
        }
    }
    out.sort_by_key(|e| (!e.is_dir, e.name.to_lowercase(), e.name.clone()));
    out
}

/// List `path` on `host` over bounded `ssh` round trips (SSH picker):
/// the folder-completion half of the picker, alongside the CLI catalog
/// from [`detect_remote_clis`]. The POSIX spelling runs first; when the
/// remote shell rejects it (Windows SSH hosts) the PowerShell spelling
/// [`remote_ps_list_script`] runs next — both parse through
/// [`parse_remote_list_output`]. Blank hosts or paths and every failure
/// (no `ssh` binary, unreachable host, missing dir, non-zero exit)
/// degrade to empty: the picker offers no completions rather than an
/// error, and the spawn path validates the folder separately.
///
/// Blocking note for shells: one `ssh` round trip on POSIX remotes (two
/// fast ones on Windows remotes), bounded by the connect timeout in
/// [`remote_list_command`]. Call it off the UI thread, on host/folder
/// input — never on every picker open.
pub fn list_remote_dir(host: &str, path: &str) -> Vec<RemoteEntry> {
    let (host, path) = (host.trim(), path.trim());
    if host.is_empty() || path.is_empty() {
        return Vec::new();
    }
    match run_remote_posix_then_ps(
        host,
        &remote_list_script(path),
        &remote_ps_list_script(path),
    ) {
        Some(text) => parse_remote_list_output(&text),
        None => Vec::new(),
    }
}

/// Result of the add-dialog tester round trip (SSH picker): whether
/// `host` answered one bounded non-interactive `ssh` round trip, plus
/// the CLI catalog parsed from the same run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteReachability {
    pub reachable: bool,
    pub clis: Vec<AvailableCli>,
}

/// Pure half of [`test_remote_host`]: `Some` probe output means the host
/// answered (reachable, with the parsed catalog); `None` (spawn error,
/// signal, non-zero exit) means unreachable with the
/// all-listed-none-available catalog. Split so tests cover the mapping
/// without touching the network.
pub fn reachability_from_probe(output: Option<&str>) -> RemoteReachability {
    match output {
        Some(text) => RemoteReachability {
            reachable: true,
            clis: parse_remote_probe_output(text),
        },
        None => RemoteReachability {
            reachable: false,
            clis: parse_remote_probe_output(""),
        },
    }
}

/// Test whether `host` answers bounded non-interactive `ssh` round trips
/// (SSH picker): the add-dialog tester. The POSIX probe runs first, then
/// the PowerShell spelling on Windows SSH hosts. Same flags as
/// [`remote_probe_command`], so auth prompts never hang the dialog —
/// key-only hosts without keys report unreachable instead of blocking.
/// Blank hosts and every failure degrade to unreachable plus the
/// all-listed-none-available catalog: the tester reports, never throws.
///
/// Blocking note for shells: one `ssh` round trip, bounded by the
/// connect timeout. Call it off the UI thread, on tester-button press
/// only — never on picker open or per keystroke.
pub fn test_remote_host(host: &str) -> RemoteReachability {
    let target = host.trim();
    if target.is_empty() {
        return reachability_from_probe(None);
    }
    reachability_from_probe(
        run_remote_posix_then_ps(target, &remote_probe_script(), &remote_ps_probe_script())
            .as_deref(),
    )
}

/// Resolve the picker's default remote folder for a host (SSH picker)
/// through one documented fallback chain: the saved host default wins,
/// then the most recent folder used on that host, then `None` (the remote
/// login directory / home). Pure so every shell shares the chain; blank
/// entries at any step fall through to the next, and every input blank
/// resolves to home.
///
/// Resolution order mirrors the local picker (configured default, then
/// recent, then inherit) so both axes feel like one rule.
pub fn resolve_remote_default_folder(
    host_default: Option<&str>,
    mru_first: Option<&str>,
) -> Option<String> {
    [host_default, mru_first]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .next()
}

/// First available CLI id, if any (repeat-last / default resolution falls
/// back to this before the hardcoded `"muse"`).
pub fn first_available(clis: &[AvailableCli]) -> Option<String> {
    clis.iter().find(|c| c.available).map(|c| c.id.clone())
}

/// Resolve the effective CLI: explicit pick wins, then the last-used CLI
/// (if still supported), then the configured default, then the first
/// autodetected binary, then `"muse"` (the historic spawn, whose failure
/// stays fail-visible via the sticky error + Retry).
pub fn resolve_effective_cli(
    explicit: Option<&str>,
    last_used: Option<&str>,
    default_cli: Option<&str>,
    catalog: &[AvailableCli],
) -> String {
    if let Some(cli) = explicit.filter(|c| !c.is_empty()) {
        return cli.to_string();
    }
    let supported = |c: &str| SUPPORTED_CLIS.contains(&c);
    if let Some(last) = last_used.filter(|c| supported(c)) {
        return last.to_string();
    }
    if let Some(def) = default_cli.filter(|c| supported(c)) {
        return def.to_string();
    }
    first_available(catalog).unwrap_or_else(|| "muse".to_string())
}

/// Per-run yolo choice in the picker: safe by default, explicit per run,
/// never auto-written back to the config (see `docs/new-session-picker.md`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum YoloChoice {
    /// Follow the per-agent config default (`agents.<cli>.yolo`, off unless set).
    #[default]
    UseDefault,
    /// Force yolo on for this run only.
    ForceOn,
    /// Force yolo off for this run only.
    ForceOff,
}

impl YoloChoice {
    /// Cycle for the `y` key: default → on → off → default.
    pub fn cycle(self) -> Self {
        match self {
            Self::UseDefault => Self::ForceOn,
            Self::ForceOn => Self::ForceOff,
            Self::ForceOff => Self::UseDefault,
        }
    }

    /// Short label for the picker row + status flash (text, never color-only).
    pub fn label(self) -> &'static str {
        match self {
            Self::UseDefault => "default",
            Self::ForceOn => "on once",
            Self::ForceOff => "off once",
        }
    }

    /// Resolve against the persisted per-agent default.
    pub fn resolve(self, default: bool) -> bool {
        match self {
            Self::UseDefault => default,
            Self::ForceOn => true,
            Self::ForceOff => false,
        }
    }
}

/// Remote SSH target for a launch: one manually entered host, resolved
/// entirely through the user's own ssh setup (`~/.ssh/config`, agents,
/// `user@` prefixes, jump aliases all keep working untouched).
///
/// No daemon, no stored secrets: the local PTY runs `ssh <host> <cli…>`
/// and interactive auth prompts (password, 2FA, host-key acceptance)
/// surface in that terminal. Only the host name and the remote folder
/// persist (see `config::Config::remote_folders`); passwords and keys
/// never touch staap state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTarget {
    /// Manual host as typed (`mybox`, `dev@jump-box`, an ssh-config alias).
    pub host: String,
    /// Working directory on the remote host (`None` = the remote login
    /// directory). Encoded into the remote command (`cd … && exec …`),
    /// never into the local PTY's cwd.
    #[serde(default)]
    pub remote_cwd: Option<String>,
}

impl SshTarget {
    pub fn new(host: String, remote_cwd: Option<String>) -> Self {
        Self { host, remote_cwd }
    }

    /// True when there is a host to dial (blank input means "not remote").
    pub fn is_valid(&self) -> bool {
        !self.host.trim().is_empty()
    }

    /// Host with surrounding whitespace removed (what `ssh` receives).
    pub fn normalized_host(&self) -> String {
        self.host.trim().to_string()
    }
}

/// Quote one shell word for the remote `cd … && exec …` snippet:
/// single-quote wrap with embedded quotes escaped (`'` → `'"'"'`).
fn shell_quote(word: &str) -> String {
    let mut out = String::with_capacity(word.len() + 2);
    out.push('\'');
    out.push_str(&word.replace('\'', "'\"'\"'"));
    out.push('\'');
    out
}

/// `cd` snippet for one remote directory: absolute paths ride single-quoted
/// (see [`shell_quote`]); a leading `~` must stay expandable, so `~` and
/// `~/…` rewrite to `$HOME` with the remainder double-quoted — single quotes
/// would suppress tilde expansion and `cd '~'` fails on the remote.
fn remote_cd_snippet(dir: &str) -> String {
    let mut snippet = String::from("cd ");
    if let Some(rest) = dir.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') {
            snippet.push_str("\"$HOME");
            for c in rest.chars() {
                match c {
                    '"' | '$' | '`' | '\\' => {
                        snippet.push('\\');
                        snippet.push(c);
                    }
                    _ => snippet.push(c),
                }
            }
            snippet.push('"');
            return snippet;
        }
    }
    snippet.push_str(&shell_quote(dir));
    snippet
}

/// Build the interactive spawn `(program, args)` for a remote target over
/// an already-resolved inner CLI command: `ssh -t <host> <cli> <args…>`.
/// `-t` forces a remote pty so interactive auth prompts and the agent
/// TUI render; the probe-only flags (`-T`, `BatchMode=yes`,
/// `ConnectTimeout`) deliberately stay on [`remote_probe_command`], never
/// here — a spawn must surface auth, never fail it silently. With a remote
/// folder the command becomes `ssh -t <host> 'cd <dir> && exec <cli> …'`
/// (one remote-shell snippet: `ssh` joins argv into one remote shell
/// anyway). A blank host falls back to the inner command unchanged (never
/// emit a bare `ssh ""`); shells validate the host before spawning.
pub fn ssh_spawn_command(
    target: &SshTarget,
    inner_program: &str,
    inner_args: &[String],
) -> (String, Vec<String>) {
    let host = target.normalized_host();
    if host.is_empty() {
        return (inner_program.to_string(), inner_args.to_vec());
    }
    let dir = target
        .remote_cwd
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty());
    match dir {
        None => {
            let mut args = Vec::with_capacity(inner_args.len() + 2);
            args.push("-t".to_string());
            args.push(host);
            args.push(inner_program.to_string());
            args.extend(inner_args.iter().cloned());
            ("ssh".to_string(), args)
        }
        Some(dir) => {
            let mut snippet = remote_cd_snippet(dir);
            snippet.push_str(" && exec ");
            snippet.push_str(&shell_quote(inner_program));
            for arg in inner_args {
                snippet.push(' ');
                snippet.push_str(&shell_quote(arg));
            }
            ("ssh".to_string(), vec!["-t".to_string(), host, snippet])
        }
    }
}

/// A confirmed 2D launch: where the child spawns, with what, how freely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSelection {
    pub cli: String,
    pub cwd: Option<String>,
    pub yolo: bool,
    /// Remote SSH target (`None` = local spawn). Remote runs keep the
    /// local `cwd` empty (the local PTY just inherits) and carry the
    /// folder on the target instead (see [`SshTarget::remote_cwd`]).
    pub remote: Option<SshTarget>,
}

impl LaunchSelection {
    pub fn new(cli: String, cwd: Option<String>, yolo: bool) -> Self {
        Self {
            cli,
            cwd,
            yolo,
            remote: None,
        }
    }

    /// Remote launch: local `cwd` stays empty, the folder rides on the
    /// target. Blank hosts still construct (validation lives at spawn).
    pub fn new_remote(cli: String, yolo: bool, remote: SshTarget) -> Self {
        Self {
            cli,
            cwd: None,
            yolo,
            remote: Some(remote),
        }
    }

    /// Builder for attaching a remote target to a local selection.
    pub fn with_remote(mut self, remote: SshTarget) -> Self {
        self.remote = Some(remote);
        self
    }

    /// True when this launch dials a remote host.
    pub fn is_remote(&self) -> bool {
        self.remote.as_ref().is_some_and(|t| t.is_valid())
    }

    /// One-line spawn preview for the picker + status flash
    /// (`muse --yolo in ~/api`, remote `ssh mybox: muse --yolo in ~/api`),
    /// so the launch is verifiable before it runs.
    pub fn preview(&self) -> String {
        let mut cmd = self.cli.clone();
        if self.yolo {
            if let Some(flag) = yolo_flag_for(&self.cli) {
                cmd.push(' ');
                cmd.push_str(flag);
            }
        }
        let local = match &self.cwd {
            Some(dir) => format!("{cmd} in {dir}"),
            None => cmd,
        };
        match &self.remote {
            Some(target) if target.is_valid() => {
                let dir = target
                    .remote_cwd
                    .as_deref()
                    .map(str::trim)
                    .filter(|d| !d.is_empty());
                match dir {
                    Some(dir) => format!("ssh {}: {local} in {dir}", target.normalized_host()),
                    None => format!("ssh {}: {local}", target.normalized_host()),
                }
            }
            _ => local,
        }
    }
}

/// Build the spawn `(program, args)` for a resolved [`LaunchSelection`]:
/// the CLI program plus the canonical yolo flag when on (deduped against
/// a user-configured identical `extra_args` entry). Configured
/// `extra_args` ride along separately in `App::spawn_command_for` — this
/// is the yolo half only, so shells never synthesize flags themselves.
pub fn yolo_args_for(selection: &LaunchSelection) -> Vec<String> {
    if !selection.yolo {
        return Vec::new();
    }
    yolo_flag_for(&selection.cli)
        .map(|f| vec![f.to_string()])
        .unwrap_or_default()
}

/// Push `dir` to the front of the MRU folder list: deduped, capped at
/// [`MAX_RECENT_FOLDERS`], blanks ignored. Pure so shells and tests share it.
pub fn push_recent_folder(recents: &mut Vec<String>, dir: &str) {
    let dir = dir.trim();
    if dir.is_empty() {
        return;
    }
    recents.retain(|d| d != dir);
    recents.insert(0, dir.to_string());
    recents.truncate(MAX_RECENT_FOLDERS);
}

/// Keyboard-first picker model: two axes (folder × CLI) plus the yolo
/// tri-state. Framework-free: shells render it natively and forward keys.
/// Enter confirms, Esc cancels with zero side effects.
#[derive(Debug, Clone)]
pub struct LaunchPicker {
    pub clis: Vec<AvailableCli>,
    pub cli_cursor: usize,
    pub folder_input: String,
    pub recent_folders: Vec<String>,
    pub yolo: YoloChoice,
    /// Which axis owns typing: true = folder input, false = CLI/yolo
    /// axis (`c` steps CLI, `y` cycles yolo, typing does nothing).
    pub folder_focus: bool,
}

impl LaunchPicker {
    pub fn new(
        clis: Vec<AvailableCli>,
        default_cli: Option<&str>,
        default_cwd: Option<&str>,
        recent_folders: Vec<String>,
    ) -> Self {
        let effective = resolve_effective_cli(None, None, default_cli, &clis);
        let cli_cursor = clis.iter().position(|c| c.id == effective).unwrap_or(0);
        // Preselect the first recent folder when no default is set, so
        // repeat-folder is one Enter (recognition over recall).
        let folder_input = match default_cwd {
            Some(dir) => dir.to_string(),
            None => recent_folders.first().cloned().unwrap_or_default(),
        };
        Self {
            clis,
            cli_cursor,
            folder_input,
            recent_folders,
            yolo: YoloChoice::UseDefault,
            folder_focus: true,
        }
    }

    pub fn selected_cli(&self) -> &AvailableCli {
        &self.clis[self.cli_cursor % self.clis.len().max(1)]
    }

    pub fn step_cli(&mut self, forward: bool) {
        if self.clis.is_empty() {
            return;
        }
        let n = self.clis.len();
        self.cli_cursor = if forward {
            (self.cli_cursor + 1) % n
        } else {
            (self.cli_cursor + n - 1) % n
        };
    }

    /// Step the folder input through the recent-folder presets: cycling
    /// recents into the input keeps full editability (type to refine,
    /// Backspace to clear to inherit).
    pub fn step_recent(&mut self, forward: bool) {
        if self.recent_folders.is_empty() {
            return;
        }
        let current = self
            .recent_folders
            .iter()
            .position(|r| *r == self.folder_input);
        let next = match current {
            Some(i) if forward => (i + 1) % self.recent_folders.len(),
            Some(i) => (i + self.recent_folders.len() - 1) % self.recent_folders.len(),
            None if forward => 0,
            None => self.recent_folders.len() - 1,
        };
        self.folder_input = self.recent_folders[next].clone();
    }

    /// Picker key dispatch. Returns true when consumed: printable keys
    /// extend the folder input, Up/Down steps recent presets, Left/Right
    /// or `c` steps the CLI axis, Tab jumps folder ↔ CLI, `y` cycles the
    /// yolo tri-state, `ctrl`/`platform` combos never count as text.
    /// Confirm (Enter) and cancel (Esc) are owned by the shell, which
    /// validates the folder before spawning.
    pub fn key(&mut self, key: &str, key_char: Option<&str>, ctrl: bool, platform: bool) -> bool {
        match key {
            "tab" => {
                self.folder_focus = !self.folder_focus;
            }
            "up" => {
                self.step_recent(false);
            }
            "down" => {
                self.step_recent(true);
            }
            "left" => {
                self.step_cli(false);
            }
            "right" => {
                self.step_cli(true);
            }
            "backspace" => {
                if self.folder_focus {
                    self.folder_input.pop();
                }
            }
            _ => {
                if ctrl || platform {
                    return true;
                }
                if key.eq_ignore_ascii_case("c") && !self.folder_focus {
                    self.step_cli(true);
                    return true;
                }
                if key.eq_ignore_ascii_case("y") && !self.folder_focus {
                    self.yolo = self.yolo.cycle();
                    return true;
                }
                if self.folder_focus {
                    if let Some(c) = key_char {
                        if c.chars().count() == 1 {
                            self.folder_input.push_str(c);
                        }
                    }
                }
            }
        }
        true
    }

    /// Confirm into a resolved selection: blank folder means inherit
    /// (`None`, the historic behavior); `~` expands via the shared
    /// [`crate::app::expand_cwd_input`] rule so both captures agree.
    pub fn confirm(&self, yolo_default: bool) -> LaunchSelection {
        LaunchSelection {
            cli: self.selected_cli().id.clone(),
            cwd: crate::app::expand_cwd_input(&self.folder_input),
            yolo: self.yolo.resolve(yolo_default),
            // The local picker stays local: remote targets arrive via
            // `new_remote` / `with_remote` (shells with a host field).
            remote: None,
        }
    }

    /// Preview line for the picker footer (`runs: muse --yolo in ~/api`).
    pub fn preview(&self, yolo_default: bool) -> String {
        self.confirm(yolo_default).preview()
    }
}

// --- PATH probing (mirrors `embedded::resolve_bare_program`) ---

fn pathext_suffixes() -> Vec<String> {
    std::env::var_os("PATHEXT")
        .map(|v| {
            std::env::split_paths(&v)
                .filter_map(|e| e.to_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![".exe".to_owned()])
}

fn probe_dir(dir: &str, program: &str, exts: &[String]) -> Option<std::path::PathBuf> {
    let dir = expand_home(dir)?;
    let base = dir.join(program);
    if base.is_file() {
        return Some(base);
    }
    exts.iter().find_map(|ext| {
        let candidate = dir.join(format!("{program}{ext}"));
        candidate.is_file().then_some(candidate)
    })
}

fn expand_home(dir: &str) -> Option<std::path::PathBuf> {
    if let Some(rest) = dir.strip_prefix("~/") {
        return crate::app::home_dir().map(|h| std::path::PathBuf::from(h).join(rest));
    }
    if dir == "~" {
        return crate::app::home_dir().map(std::path::PathBuf::from);
    }
    Some(std::path::PathBuf::from(dir))
}

/// Extra dirs for GUI-launched processes whose `PATH` misses shell-added
/// entries (macOS `/opt/homebrew`, Linux `/snap/bin`, user `~/.local/bin`).
fn extra_search_dirs() -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        vec![
            "/opt/homebrew/bin".to_string(),
            "/usr/local/bin".to_string(),
            "~/.local/bin".to_string(),
        ]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        vec![
            "~/.local/bin".to_string(),
            "/snap/bin".to_string(),
            "/var/lib/flatpak/exports/bin".to_string(),
        ]
    }
    #[cfg(target_os = "windows")]
    {
        Vec::new()
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        Vec::new()
    }
}

fn resolve_bare_name(
    program: &str,
    path_value: &std::ffi::OsStr,
    exts: &[String],
) -> Option<std::path::PathBuf> {
    if program.is_empty() || program.contains('/') || program.contains('\\') {
        return None;
    }
    let ext_refs: Vec<&str> = exts.iter().map(String::as_str).collect();
    for dir in std::env::split_paths(path_value) {
        let base = dir.join(program);
        if base.is_file() {
            return Some(base);
        }
        for ext in &ext_refs {
            let candidate = dir.join(format!("{program}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(ids: &[(&str, bool)]) -> Vec<AvailableCli> {
        ids.iter()
            .map(|(id, up)| AvailableCli {
                id: id.to_string(),
                program: id.to_string(),
                path: up.then(|| format!("/bin/{id}")),
                available: *up,
            })
            .collect()
    }

    #[test]
    fn yolo_flags_cover_documented_clis_only() {
        assert_eq!(yolo_flag_for("muse"), Some("--yolo"));
        assert_eq!(
            yolo_flag_for("claude"),
            Some("--dangerously-skip-permissions")
        );
        assert_eq!(
            yolo_flag_for("codex"),
            Some("--dangerously-bypass-approvals-and-sandbox")
        );
        assert_eq!(yolo_flag_for("opencode"), Some("--auto"));
        assert_eq!(yolo_flag_for("future"), None);
    }

    #[test]
    fn yolo_choice_cycles_and_resolves() {
        assert_eq!(YoloChoice::UseDefault.cycle(), YoloChoice::ForceOn);
        assert_eq!(YoloChoice::ForceOn.cycle(), YoloChoice::ForceOff);
        assert_eq!(YoloChoice::ForceOff.cycle(), YoloChoice::UseDefault);
        assert!(YoloChoice::ForceOn.resolve(false));
        assert!(!YoloChoice::ForceOff.resolve(true));
        assert!(YoloChoice::UseDefault.resolve(true));
        assert!(!YoloChoice::UseDefault.resolve(false));
    }

    #[test]
    fn effective_cli_prefers_explicit_then_last_then_default_then_detect() {
        let cat = catalog(&[("muse", true), ("claude", true)]);
        assert_eq!(
            resolve_effective_cli(Some("codex"), Some("claude"), Some("muse"), &cat),
            "codex"
        );
        assert_eq!(
            resolve_effective_cli(None, Some("claude"), Some("muse"), &cat),
            "claude"
        );
        assert_eq!(
            resolve_effective_cli(None, None, Some("claude"), &cat),
            "claude"
        );
        assert_eq!(resolve_effective_cli(None, None, None, &cat), "muse");
        // Unsupported last/default fall through to autodetect.
        assert_eq!(
            resolve_effective_cli(None, Some("nope"), Some("nope"), &cat),
            "muse"
        );
        // Nothing detected anywhere: historic hardcoded default keeps the
        // failure fail-visible (sticky error + Retry) instead of an empty pick.
        assert_eq!(resolve_effective_cli(None, None, None, &[]), "muse");
    }

    #[test]
    fn selection_preview_names_command_and_folder() {
        let sel = LaunchSelection::new("muse".to_string(), Some("/tmp/api".to_string()), true);
        assert_eq!(sel.preview(), "muse --yolo in /tmp/api");
        let plain = LaunchSelection::new("claude".to_string(), None, false);
        assert_eq!(plain.preview(), "claude");
        // Unknown CLI: no guessed flag, never a broken command.
        let unknown = LaunchSelection::new("future".to_string(), None, true);
        assert_eq!(unknown.preview(), "future");
        assert!(yolo_args_for(&unknown).is_empty());
        // Verified flags ride the preview + args for codex/opencode.
        let cx = LaunchSelection::new("codex".to_string(), None, true);
        assert_eq!(
            cx.preview(),
            "codex --dangerously-bypass-approvals-and-sandbox"
        );
        assert_eq!(
            yolo_args_for(&cx),
            vec!["--dangerously-bypass-approvals-and-sandbox".to_string()]
        );
        let oc = LaunchSelection::new("opencode".to_string(), None, true);
        assert_eq!(oc.preview(), "opencode --auto");
    }

    #[test]
    fn recents_dedupe_cap_and_ignore_blanks() {
        let mut recents = vec!["/b".to_string()];
        push_recent_folder(&mut recents, "/a");
        assert_eq!(recents, vec!["/a", "/b"]);
        push_recent_folder(&mut recents, "/a");
        assert_eq!(recents, vec!["/a", "/b"]);
        push_recent_folder(&mut recents, "   ");
        assert_eq!(recents, vec!["/a", "/b"]);
        for i in 0..20 {
            push_recent_folder(&mut recents, &format!("/d{i}"));
        }
        assert_eq!(recents.len(), MAX_RECENT_FOLDERS);
        assert_eq!(recents[0], "/d19");
    }

    #[test]
    fn picker_confirms_blank_as_inherit_and_cycles_clis() {
        let cat = catalog(&[("muse", true), ("claude", false)]);
        let mut picker = LaunchPicker::new(cat, Some("claude"), None, vec![]);
        // Default CLI preselected even when missing (listed, not hidden).
        assert_eq!(picker.selected_cli().id, "claude");
        picker.step_cli(true);
        assert_eq!(picker.selected_cli().id, "muse");
        picker.step_cli(false);
        assert_eq!(picker.selected_cli().id, "claude");
        // Blank folder inherits; yolo resolves against the config default.
        let sel = picker.confirm(false);
        assert_eq!(sel.cwd, None);
        assert!(!sel.yolo);
        picker.yolo = YoloChoice::ForceOn;
        assert!(picker.confirm(false).yolo);
        picker.folder_input = "~/proj".to_string();
        let home = crate::app::home_dir().expect("test env has a home dir");
        assert_eq!(picker.confirm(false).cwd, Some(format!("{home}/proj")));
    }

    #[test]
    fn picker_key_drives_both_axes_and_yolo() {
        let cat = catalog(&[("muse", true), ("claude", true)]);
        let mut picker = LaunchPicker::new(cat, Some("muse"), None, vec!["/tmp".to_string()]);
        // Recent preselected when no default folder is set.
        assert_eq!(picker.folder_input, "/tmp");
        // Typing refines the folder input; Backspace edits.
        assert!(picker.key("x", Some("x"), false, false));
        assert!(picker.folder_input.ends_with('x'));
        assert!(picker.key("backspace", None, false, false));
        assert_eq!(picker.folder_input, "/tmp");
        // Ctrl combos never count as path text.
        assert!(picker.key("c", Some("c"), true, false));
        assert_eq!(picker.folder_input, "/tmp");
        // Tab jumps to the CLI axis: `c` steps it, `y` cycles yolo.
        assert!(picker.key("tab", None, false, false));
        assert!(!picker.folder_focus);
        assert!(picker.key("c", Some("c"), false, false));
        assert_eq!(picker.selected_cli().id, "claude");
        assert_eq!(picker.yolo, YoloChoice::UseDefault);
        assert!(picker.key("y", Some("y"), false, false));
        assert_eq!(picker.yolo, YoloChoice::ForceOn);
        // Typing on the CLI axis does nothing to the folder.
        assert!(picker.key("q", Some("q"), false, false));
        assert_eq!(picker.folder_input, "/tmp");
        // Left/Right step the CLI axis directly.
        assert!(picker.key("left", None, false, false));
        assert_eq!(picker.selected_cli().id, "muse");
        assert!(picker.key("right", None, false, false));
        assert_eq!(picker.selected_cli().id, "claude");
        // Up/Down cycles recents into the input.
        picker.folder_focus = true;
        assert!(picker.key("down", None, false, false));
        assert_eq!(picker.folder_input, "/tmp");
    }

    #[test]
    fn remote_probe_command_names_ssh_with_bounded_noninteractive_opts() {
        let (program, args) = remote_probe_command("mybox");
        assert_eq!(program, "ssh");
        // No pty, no password prompts, bounded connect: the probe never
        // hangs the picker on auth the way an interactive spawn may.
        assert!(args.contains(&"-T".to_string()));
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(args.contains(&"ConnectTimeout=5".to_string()));
        assert!(args.contains(&"mybox".to_string()));
        // The remote script covers every supported CLI with `command -v`.
        let script = args.last().unwrap();
        for id in SUPPORTED_CLIS {
            assert!(script.contains(id), "probe script covers {id}");
        }
        assert!(script.contains("command -v"));
        // The host passes through untouched (user@, ssh-config aliases).
        let (_, alias_args) = remote_probe_command("dev@jump-box");
        assert!(alias_args.contains(&"dev@jump-box".to_string()));
    }

    #[test]
    fn remote_probe_output_parses_per_host_availability_in_order() {
        let out = "muse:/usr/local/bin/muse\nclaude:\nopencode:/home/u/.local/bin/opencode\ncodex:/usr/bin/codex\n";
        let clis = parse_remote_probe_output(out);
        let ids: Vec<&str> = clis.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, SUPPORTED_CLIS);
        assert!(clis[0].available);
        assert_eq!(clis[0].path.as_deref(), Some("/usr/local/bin/muse"));
        assert!(!clis[1].available);
        assert_eq!(clis[1].path, None);
        assert!(clis[2].available);
        assert!(clis[3].available);
    }

    #[test]
    fn remote_probe_output_ignores_noise_and_keeps_first_hit() {
        // Banners, unknown ids, blanks, and malformed lines never leak
        // into the catalog; duplicates keep the first hit.
        let out = "Warning: Permanently added 'mybox' (ED25519).\n\
                   future:/opt/future/bin/cli\n\
                   muse:/bin/muse\n\
                   not-a-pair\n\
                   muse:/other/muse\n\
                   claude:  \n";
        let clis = parse_remote_probe_output(out);
        assert_eq!(clis[0].path.as_deref(), Some("/bin/muse"));
        assert!(!clis[1].available);
        assert!(!clis[2].available);
        assert!(!clis[3].available);
    }

    #[test]
    fn remote_probe_empty_host_and_output_degrade_to_all_listed() {
        // Blank hosts never spawn ssh: the catalog still lists every CLI
        // as not installed, so the picker stays usable and fail-visible.
        for clis in [
            detect_remote_clis(""),
            detect_remote_clis("   "),
            parse_remote_probe_output(""),
        ] {
            let ids: Vec<&str> = clis.iter().map(|c| c.id.as_str()).collect();
            assert_eq!(ids, SUPPORTED_CLIS);
            assert!(clis.iter().all(|c| !c.available && c.path.is_none()));
        }
    }

    #[test]
    fn ssh_target_validates_and_normalizes_host() {
        let t = SshTarget::new("  dev@jump-box ".to_string(), None);
        assert!(t.is_valid());
        assert_eq!(t.normalized_host(), "dev@jump-box");
        assert!(!SshTarget::new("".to_string(), None).is_valid());
        assert!(!SshTarget::new("   ".to_string(), None).is_valid());
        // Host-only target: no remote dir means the remote login directory.
        assert_eq!(t.remote_cwd, None);
        // Serde round-trip; old files without `remote_cwd` still load.
        let back: SshTarget = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(back, t);
        let legacy: SshTarget = serde_json::from_str(r#"{"host":"mybox"}"#).unwrap();
        assert_eq!(legacy.remote_cwd, None);
    }

    #[test]
    fn ssh_spawn_wraps_inner_command_without_probe_flags() {
        let inner = vec!["--yolo".to_string()];
        // No remote dir: plain `ssh -t <host> <cli> <args…>`.
        let (program, args) =
            ssh_spawn_command(&SshTarget::new("mybox".to_string(), None), "muse", &inner);
        assert_eq!(program, "ssh");
        assert_eq!(args, vec!["-t", "mybox", "muse", "--yolo"]);
        // Interactive auth must surface: none of the probe-only flags
        // (`-T`, `BatchMode=yes`, timeouts) ride the spawn.
        assert!(!args.iter().any(|a| a.contains("BatchMode")));
        assert!(!args.iter().any(|a| a == "-T"));
        assert!(!args.iter().any(|a| a.contains("ConnectTimeout")));
        // Host passes through untouched (user@, ssh-config aliases).
        let (_, alias_args) = ssh_spawn_command(
            &SshTarget::new("dev@jump-box".to_string(), None),
            "claude",
            &[],
        );
        assert_eq!(alias_args, vec!["-t", "dev@jump-box", "claude"]);
        // Yolo/extra args stay on the inner CLI, never interpreted by ssh.
        let (_, codex_args) = ssh_spawn_command(
            &SshTarget::new("mybox".to_string(), None),
            "codex",
            &["--dangerously-bypass-approvals-and-sandbox".to_string()],
        );
        assert_eq!(
            codex_args,
            vec![
                "-t",
                "mybox",
                "codex",
                "--dangerously-bypass-approvals-and-sandbox"
            ]
        );
    }

    #[test]
    fn ssh_spawn_encodes_remote_dir_as_cd_exec_snippet() {
        let inner = vec!["--yolo".to_string()];
        let (program, args) = ssh_spawn_command(
            &SshTarget::new("mybox".to_string(), Some("/srv/api".to_string())),
            "muse",
            &inner,
        );
        assert_eq!(program, "ssh");
        assert_eq!(args.len(), 3);
        assert_eq!(args[0], "-t");
        assert_eq!(args[1], "mybox");
        assert_eq!(args[2], "cd '/srv/api' && exec 'muse' '--yolo'");
        // Quoting survives spaces and single quotes in dir and args.
        let (_, quoted) = ssh_spawn_command(
            &SshTarget::new("mybox".to_string(), Some("/srv/my api's".to_string())),
            "muse",
            &["it's".to_string()],
        );
        assert_eq!(
            quoted[2],
            "cd '/srv/my api'\"'\"'s' && exec 'muse' 'it'\"'\"'s'"
        );
        // Blank remote dir behaves like no dir (remote login directory).
        let (_, blank) = ssh_spawn_command(
            &SshTarget::new("mybox".to_string(), Some("   ".to_string())),
            "muse",
            &[],
        );
        assert_eq!(blank, vec!["-t", "mybox", "muse"]);
    }

    #[test]
    fn ssh_spawn_blank_host_falls_back_to_inner_command() {
        // Never emit a bare `ssh ""`: shells validate first, and the core
        // degrades to the local command instead of a broken spawn.
        let inner = vec!["--yolo".to_string()];
        let (program, args) =
            ssh_spawn_command(&SshTarget::new("   ".to_string(), None), "muse", &inner);
        assert_eq!(program, "muse");
        assert_eq!(args, inner);
    }

    #[test]
    fn selection_preview_names_remote_host_and_dir() {
        // Local selections preview exactly as before (no `ssh:` prefix).
        let local = LaunchSelection::new("muse".to_string(), Some("/tmp/api".to_string()), true);
        assert_eq!(local.preview(), "muse --yolo in /tmp/api");
        assert!(!local.is_remote());
        // Remote: `ssh host: cli in dir`; yolo flag still names the inner CLI.
        let remote = LaunchSelection::new_remote(
            "muse".to_string(),
            true,
            SshTarget::new("mybox".to_string(), Some("/srv/api".to_string())),
        );
        assert_eq!(remote.preview(), "ssh mybox: muse --yolo in /srv/api");
        assert!(remote.is_remote());
        assert_eq!(remote.cwd, None);
        // No remote dir: the remote login directory, no `in` suffix.
        let nodir = LaunchSelection::new_remote(
            "claude".to_string(),
            false,
            SshTarget::new("mybox".to_string(), None),
        );
        assert_eq!(nodir.preview(), "ssh mybox: claude");
        // Blank host is not remote: preview stays local and fail-visible.
        let blank = LaunchSelection::new("muse".to_string(), None, false)
            .with_remote(SshTarget::new("  ".to_string(), None));
        assert!(!blank.is_remote());
        assert_eq!(blank.preview(), "muse");
    }

    #[test]
    fn detect_lists_every_supported_cli_in_order() {
        let clis = detect_with_path(std::ffi::OsStr::new(""));
        let ids: Vec<&str> = clis.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, SUPPORTED_CLIS);
    }

    #[test]
    fn detect_finds_a_fake_cli_on_path() {
        let dir = std::env::temp_dir().join(format!(
            "staap-launch-detect-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("codex"), b"fake").unwrap();
        let clis = detect_with_path(dir.as_os_str());
        let codex = clis.iter().find(|c| c.id == "codex").unwrap();
        assert!(codex.available);
        assert!(codex.path.as_deref().is_some_and(|p| p.contains("codex")));
        let muse = clis.iter().find(|c| c.id == "muse").unwrap();
        assert!(!muse.available || muse.path.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remote_cd_keeps_tilde_expandable_and_quotes_the_rest() {
        // `~` and `~/…` rewrite to expandable `$HOME` (single quotes would
        // suppress tilde expansion: `cd '~'` fails on the remote).
        assert_eq!(remote_cd_snippet("~"), "cd \"$HOME\"");
        assert_eq!(remote_cd_snippet("~/"), "cd \"$HOME/\"");
        assert_eq!(remote_cd_snippet("~/srv/api"), "cd \"$HOME/srv/api\"");
        // Spaces ride double-quoted; `$`, backtick, quote, and backslash
        // stay inert instead of executing on the remote shell.
        assert_eq!(remote_cd_snippet("~/my dir"), "cd \"$HOME/my dir\"");
        assert_eq!(
            remote_cd_snippet("~/$x `id` \"q\" \\"),
            "cd \"$HOME/\\$x \\`id\\` \\\"q\\\" \\\\\""
        );
        // Anything else keeps the exact single-quoted behavior.
        assert_eq!(remote_cd_snippet("/srv/api"), "cd '/srv/api'");
        assert_eq!(remote_cd_snippet("~otheruser"), "cd '~otheruser'");
        // Both consumers quote through the one helper.
        assert_eq!(remote_list_script("~"), "cd \"$HOME\" && exec ls -1A -p");
        let (program, args) = ssh_spawn_command(
            &SshTarget::new("mybox".to_string(), Some("~".to_string())),
            "muse",
            &[],
        );
        assert_eq!(program, "ssh");
        assert_eq!(args[2], "cd \"$HOME\" && exec 'muse'");
    }

    #[test]
    fn remote_list_command_is_single_bounded_noninteractive_round_trip() {
        let (program, args) = remote_list_command("mybox", "/srv/api");
        assert_eq!(program, "ssh");
        // No pty, no password prompts, bounded connect: the listing probe
        // never hangs the picker on auth the way an interactive spawn may.
        assert!(args.contains(&"-T".to_string()));
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(args.contains(&"ConnectTimeout=5".to_string()));
        assert!(args.contains(&"mybox".to_string()));
        // Exactly one remote script arg (a single round trip), quoting the
        // path and listing with `ls -1A -p` after a gating `cd`.
        let script = args.last().unwrap();
        assert_eq!(
            script, "cd '/srv/api' && exec ls -1A -p",
            "one script, one round trip: {script}"
        );
        // The host passes through untouched (user@, ssh-config aliases).
        let (_, alias_args) = remote_list_command("dev@jump-box", "/x");
        assert!(alias_args.contains(&"dev@jump-box".to_string()));
        // Quoting survives spaces and single quotes in the path.
        let (_, quoted) = remote_list_command("mybox", "/srv/my api's");
        assert_eq!(
            quoted.last().unwrap(),
            "cd '/srv/my api'\"'\"'s' && exec ls -1A -p"
        );
    }

    #[test]
    fn remote_list_output_parses_dirs_files_and_caps() {
        // Sorted directories-first, case-insensitively: `.hidden`
        // precedes `main.rs` once raw `ls` order no longer surfaces.
        let entries = parse_remote_list_output("src/\nmain.rs\n.hidden\n");
        assert_eq!(
            entries,
            vec![
                RemoteEntry {
                    name: "src".to_string(),
                    is_dir: true,
                },
                RemoteEntry {
                    name: ".hidden".to_string(),
                    is_dir: false,
                },
                RemoteEntry {
                    name: "main.rs".to_string(),
                    is_dir: false,
                },
            ]
        );
        // Blanks and dot entries never surface; a bare `/` stays a file
        // row rather than vanishing into an empty dir name.
        let edge = parse_remote_list_output("\n.\n..\n/\n");
        assert_eq!(
            edge,
            vec![RemoteEntry {
                name: "/".to_string(),
                is_dir: false,
            }]
        );
        // Names keep interior bytes (no trimming — remote names may carry
        // spaces); only the `ls -p` dir marker is stripped.
        let spaced = parse_remote_list_output("my dir/\nnotes.txt\n");
        assert_eq!(spaced[0].name, "my dir");
        assert!(spaced[0].is_dir);
        // Bounded: huge remote dirs cannot grow the picker without limit.
        let big = (0..MAX_REMOTE_LIST_ENTRIES + 50)
            .map(|i| format!("f{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            parse_remote_list_output(&big).len(),
            MAX_REMOTE_LIST_ENTRIES
        );
        // Serde round-trip for the FFI JSON shape.
        let back: Vec<RemoteEntry> =
            serde_json::from_str(&serde_json::to_string(&entries).unwrap()).unwrap();
        assert_eq!(back, entries);
    }

    #[test]
    fn remote_list_output_sorts_dirs_first_case_insensitively() {
        let entries = parse_remote_list_output("zeta.txt\nAlpha/\nbeta.txt\nGamma/\n");
        let names: Vec<(&str, bool)> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.is_dir))
            .collect();
        assert_eq!(
            names,
            vec![
                ("Alpha", true),
                ("Gamma", true),
                ("beta.txt", false),
                ("zeta.txt", false)
            ]
        );
    }

    #[test]
    fn remote_ps_scripts_quote_and_share_the_posix_wire_format() {
        // Single quotes double; backslashes stay literal.
        assert_eq!(ps_quote("C:\\srv\\api"), "'C:\\srv\\api'");
        assert_eq!(ps_quote("it's"), "'it''s'");
        // Home rewrites to $HOME like the POSIX snippet; backslash
        // separators survive (Windows users type them).
        assert_eq!(
            remote_ps_cd_snippet("~"),
            "Set-Location -LiteralPath \"$HOME\""
        );
        assert_eq!(
            remote_ps_cd_snippet("~\\srv\\api"),
            "Set-Location -LiteralPath \"$HOME\\srv\\api\""
        );
        assert_eq!(
            remote_ps_cd_snippet("C:\\srv\\it's"),
            "Set-Location -LiteralPath 'C:\\srv\\it''s'"
        );
        // The listing spells a strict cd plus the same name/ wire format
        // the POSIX parser reads, so one parser serves both remotes.
        let script = remote_ps_list_script("C:\\srv\\api");
        assert!(script.contains("-ErrorAction Stop"), "{script}");
        assert!(
            script.contains("Get-ChildItem -LiteralPath . -Force"),
            "{script}"
        );
        assert!(script.contains("PSIsContainer"), "{script}");
        let entries = parse_remote_list_output("Windows/\nnotes.txt\n");
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.name.as_str(), e.is_dir))
                .collect::<Vec<_>>(),
            vec![("Windows", true), ("notes.txt", false)]
        );
        // The probe spells every supported CLI and emits id:path rows the
        // POSIX probe parser reads — Windows C:\ paths survive the
        // first-colon split.
        let probe = remote_ps_probe_script();
        for id in SUPPORTED_CLIS {
            assert!(probe.contains(&format!("'{id}'")), "{probe}");
        }
        let clis = parse_remote_probe_output("muse:C:\\tools\\muse.exe\nclaude:\n");
        assert!(clis[0].available);
        assert_eq!(clis[0].path.as_deref(), Some("C:\\tools\\muse.exe"));
        assert!(!clis[1].available);
    }

    #[test]
    fn remote_list_degrades_to_empty_without_network() {
        // Blank hosts and blank paths never spawn ssh: the picker offers
        // no completions rather than an error, with zero network touch.
        assert!(list_remote_dir("", "/srv/api").is_empty());
        assert!(list_remote_dir("   ", "/srv/api").is_empty());
        assert!(list_remote_dir("mybox", "").is_empty());
        assert!(list_remote_dir("mybox", "   ").is_empty());
    }

    #[test]
    fn reachability_maps_probe_output_without_network() {
        // Pure half: answered output means reachable with the parsed
        // catalog; no output means unreachable with the degraded catalog.
        let up = reachability_from_probe(Some("muse:/bin/muse\nclaude:\n"));
        assert!(up.reachable);
        assert!(up.clis[0].available);
        assert!(!up.clis[1].available);
        let down = reachability_from_probe(None);
        assert!(!down.reachable);
        assert!(down.clis.iter().all(|c| !c.available));
        // Blank hosts never spawn ssh: unreachable + degraded catalog.
        for host in ["", "   "] {
            let t = test_remote_host(host);
            assert!(!t.reachable, "blank host reports unreachable");
            let ids: Vec<&str> = t.clis.iter().map(|c| c.id.as_str()).collect();
            assert_eq!(ids, SUPPORTED_CLIS);
            assert!(t.clis.iter().all(|c| !c.available));
        }
        // Serde round-trip for the tester JSON shape.
        let back: RemoteReachability =
            serde_json::from_str(&serde_json::to_string(&up).unwrap()).unwrap();
        assert_eq!(back, up);
    }

    #[test]
    fn remote_default_folder_falls_back_host_default_then_mru_then_home() {
        // Saved host default wins over everything.
        assert_eq!(
            resolve_remote_default_folder(Some("/srv/api"), Some("/srv/other")),
            Some("/srv/api".to_string())
        );
        // Blank defaults fall through to the host MRU.
        assert_eq!(
            resolve_remote_default_folder(Some("   "), Some("/srv/other")),
            Some("/srv/other".to_string())
        );
        assert_eq!(
            resolve_remote_default_folder(None, Some("/srv/other")),
            Some("/srv/other".to_string())
        );
        // Everything blank resolves to home (None = remote login dir).
        assert_eq!(resolve_remote_default_folder(None, None), None);
        assert_eq!(resolve_remote_default_folder(Some(""), Some("  ")), None);
        // Surrounding whitespace never leaks into the resolved folder.
        assert_eq!(
            resolve_remote_default_folder(Some("  /srv/api  "), None),
            Some("/srv/api".to_string())
        );
    }
}
