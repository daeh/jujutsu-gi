use anyhow::{Context, Result, bail};
use libproc::bsd_info::BSDInfo;
use libproc::proc_pid::{pidinfo, pidpath};
use std::ops::Range;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

// =====================================================================
// Wrapper / completion content
// =====================================================================

const MARKER_BEGIN: &str = "# >>> ji shell integration >>>";
const MARKER_END: &str = "# <<< ji shell integration <<<";
const MANAGED_HEADER: &str = "# ji-managed: do not edit";

const ZSH_WRAPPER: &str = r#"if command -v ji >/dev/null 2>&1; then
    ji() {
        local directive_file exit_code=0
        directive_file="$(mktemp)"

        JI_DIRECTIVE_FILE="$directive_file" command ji "$@" || exit_code=$?

        if [[ -s "$directive_file" ]]; then
            source "$directive_file"
            local directive_status=$?
            if [[ $exit_code -eq 0 ]]; then
                exit_code=$directive_status
            fi
        fi

        rm -f "$directive_file"
        return "$exit_code"
    }
fi
"#;

const BASH_WRAPPER: &str = r#"if command -v ji >/dev/null 2>&1; then
    ji() {
        local directive_file exit_code=0
        directive_file="$(mktemp)"

        JI_DIRECTIVE_FILE="$directive_file" command ji "$@" || exit_code=$?

        if [[ -s "$directive_file" ]]; then
            source "$directive_file"
            local directive_status=$?
            if [[ $exit_code -eq 0 ]]; then
                exit_code=$directive_status
            fi
        fi

        rm -f "$directive_file"
        return "$exit_code"
    }
fi
"#;

const FISH_WRAPPER: &str = r#"function ji --description 'ji workspace switcher (with cd directive support)'
    set -l directive_file (mktemp)

    JI_DIRECTIVE_FILE=$directive_file command ji $argv
    set -l exit_code $status

    if test -s $directive_file
        eval (string collect <$directive_file)
        set -l directive_status $status
        if test $exit_code -eq 0
            set exit_code $directive_status
        end
    end

    command rm -f $directive_file
    return $exit_code
end
"#;

const SUPPORTED_SHELLS: &str = "zsh, bash, fish";

fn shell_kind(shell: &str) -> Result<clap_complete::Shell> {
    match shell {
        "zsh" => Ok(clap_complete::Shell::Zsh),
        "bash" => Ok(clap_complete::Shell::Bash),
        "fish" => Ok(clap_complete::Shell::Fish),
        other => bail!("unsupported shell: {other} (supported: {SUPPORTED_SHELLS})"),
    }
}

fn wrapper_script(shell: &str) -> Result<&'static str> {
    match shell {
        "zsh" => Ok(ZSH_WRAPPER),
        "bash" => Ok(BASH_WRAPPER),
        "fish" => Ok(FISH_WRAPPER),
        other => bail!("unsupported shell: {other} (supported: {SUPPORTED_SHELLS})"),
    }
}

fn completion_script(shell: &str, cmd: &mut clap::Command) -> Result<String> {
    let kind = shell_kind(shell)?;
    let mut buf: Vec<u8> = Vec::new();
    clap_complete::generate(kind, cmd, "ji", &mut buf);
    let script = String::from_utf8(buf).context("clap_complete generated invalid UTF-8")?;
    // zsh `compdef` only exists after `autoload -U compinit && compinit` has run.
    // Guard so users without compinit get the wrapper but silently skip completion
    // instead of an error at shell startup.
    if shell == "zsh" {
        Ok(format!(
            "if (( ${{+functions[compdef]}} )); then\n{script}\nfi\n"
        ))
    } else {
        Ok(script)
    }
}

/// What `ji config shell init <shell>` streams to stdout.
pub fn print_init(shell: &str, cmd: &mut clap::Command) -> Result<()> {
    let body = render_managed_body(shell, cmd)?;
    print!("{body}");
    Ok(())
}

// =====================================================================
// Active-shell detection
//
// `$SHELL` is the *login* shell, not the shell the user is actively running.
// We instead walk up the process tree (via libproc) and stop at the nearest
// ancestor that is a recognized shell — that is the shell that invoked `ji`.
// `$SHELL` is consulted only as a fallback, and only when no shell is found.
// =====================================================================

/// Shells `ji` can install integration for. Single source of truth; the
/// human-readable [`SUPPORTED_SHELLS`] string is kept in sync by a test.
const SUPPORTED: &[&str] = &["zsh", "bash", "fish"];

/// Recognized shells `ji` does *not* support integration for. Enumerated
/// exhaustively on purpose: a shell missing from this set is classified as a
/// non-shell and walked past, which could let an outer supported shell win and
/// violate "nearest recognized shell ancestor wins".
const UNSUPPORTED_SHELLS: &[&str] = &[
    "sh",
    "dash",
    "ash",
    "busybox",
    "ksh",
    "mksh",
    "pdksh",
    "loksh",
    "oksh",
    "rksh",
    "csh",
    "tcsh",
    "nu",
    "nushell",
    "pwsh",
    "powershell",
    "xonsh",
    "elvish",
    "ion",
    "oil",
    "osh",
    "rc",
    "es",
    "yash",
    "scsh",
    "tclsh",
    "murex",
];

/// Bound on the process-tree walk — guards against cycles / pathological trees.
const MAX_WALK_DEPTH: usize = 24;

/// Classification of a process executable's basename.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Class {
    /// A shell `ji` supports (zsh/bash/fish).
    Supported(String),
    /// A recognized shell `ji` does not support.
    Unsupported(String),
    /// Not a recognized shell — keep walking up the process tree.
    NotAShell,
}

/// Outcome of walking the process tree for the active shell.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Detection {
    Supported(String),
    Unsupported(String),
    /// No recognized shell found in the ancestry.
    Unknown,
}

/// Reduce an executable path or `$SHELL` value to a bare shell name: take the
/// file name, strip a leading `-` (login shells exec as `-zsh`), lowercase.
fn shell_basename(path: &str) -> Option<String> {
    let name = Path::new(path).file_name()?.to_str()?;
    let name = name.strip_prefix('-').unwrap_or(name);
    (!name.is_empty()).then(|| name.to_ascii_lowercase())
}

fn classify(name: &str) -> Class {
    if SUPPORTED.contains(&name) {
        Class::Supported(name.to_string())
    } else if UNSUPPORTED_SHELLS.contains(&name) {
        Class::Unsupported(name.to_string())
    } else {
        Class::NotAShell
    }
}

/// Walk up from `start_pid`'s parent and return the first recognized shell
/// ancestor (and its executable path, for display). Pure over the injected
/// `parent_of` / `name_of` lookups so it is unit-testable with synthetic trees.
/// `name_of` returns a process's full executable path.
fn resolve_in_tree(
    start_pid: i32,
    parent_of: impl Fn(i32) -> Option<i32>,
    name_of: impl Fn(i32) -> Option<String>,
    max_depth: usize,
) -> (Detection, Option<String>) {
    let mut cur = parent_of(start_pid);
    let mut depth = 0;
    while let Some(pid) = cur {
        if depth >= max_depth {
            break;
        }
        depth += 1;
        if let Some(path) = name_of(pid)
            && let Some(base) = shell_basename(&path)
        {
            match classify(&base) {
                Class::Supported(s) => return (Detection::Supported(s), Some(path)),
                Class::Unsupported(s) => return (Detection::Unsupported(s), Some(path)),
                // Not a shell (sudo/env/make/login/…) — keep walking.
                Class::NotAShell => {}
            }
        }
        cur = parent_of(pid);
    }
    (Detection::Unknown, None)
}

/// The full fallback contract, pure over the injected raw `$SHELL` value so it
/// (including basename normalization) is unit-testable without the environment.
/// `$SHELL` is consulted **only** when no shell was found in the process tree.
fn decide(active: Detection, env_shell_raw: Option<&str>) -> Result<String> {
    match active {
        Detection::Supported(s) => Ok(s),
        Detection::Unsupported(name) => bail!(
            "detected active shell '{name}', which ji doesn't support (supported: {SUPPORTED_SHELLS}); \
             pass the shell explicitly, e.g. `ji config shell install zsh`"
        ),
        Detection::Unknown => {
            let Some(raw) = env_shell_raw.filter(|s| !s.is_empty()) else {
                bail!("SHELL not set — pass the shell explicitly");
            };
            match shell_basename(raw).map(|b| classify(&b)) {
                Some(Class::Supported(s)) => Ok(s),
                Some(Class::Unsupported(name)) => bail!(
                    "could not identify your active shell; $SHELL names an unsupported login shell \
                     '{name}' (supported: {SUPPORTED_SHELLS}); pass the shell explicitly"
                ),
                _ => bail!("could not determine your shell; pass it explicitly (zsh|bash|fish)"),
            }
        }
    }
}

/// `parent_of` over the live process tree. `ppid <= 1` (launchd / reparented)
/// stops the walk.
fn live_parent(pid: i32) -> Option<i32> {
    pidinfo::<BSDInfo>(pid, 0)
        .ok()
        .map(|info| info.pbi_ppid as i32)
        .filter(|&ppid| ppid > 1)
}

/// `name_of` over the live process tree: the full executable path, or `None`
/// if it can't be resolved (e.g. the process exited) — the walk then skips it.
fn live_exe_path(pid: i32) -> Option<String> {
    pidpath(pid).ok()
}

/// Detect the shell that invoked `ji`: the nearest recognized shell ancestor,
/// falling back to `$SHELL` only when none is found. Errors clearly when the
/// active shell is a recognized-but-unsupported shell.
pub fn detect_shell() -> Result<String> {
    let me = std::process::id() as i32;
    let (active, _) = resolve_in_tree(me, live_parent, live_exe_path, MAX_WALK_DEPTH);
    decide(active, std::env::var("SHELL").ok().as_deref())
}

// =====================================================================
// Parent-shell cd directive + reason-aware diagnosis
//
// `ji` changes the parent shell's directory only when its wrapper is active
// (the wrapper exports `JI_DIRECTIVE_FILE` and sources what we write). When the
// wrapper is NOT active we can't cd, so instead of crashing we diagnose WHY and
// print a terse, reason-specific note. Diagnosis (process-tree walk + filesystem
// probes) runs ONLY on the cd-unavailable path — never on the wrapped happy path.
// =====================================================================

/// Why `ji` could not change the parent shell's directory.
#[derive(Debug, Clone)]
enum CdUnavailable {
    /// Nothing on disk for the active shell → install.
    NotInstalled,
    /// Present but the on-disk wrapper isn't exactly what ji ships (drift /
    /// cross-file / shadow / unresolved) → reinstall or remove the conflict.
    NeedsRefresh,
    /// Exact working wrapper on disk, just not active for this invocation.
    InstalledInactive,
    /// Invoked via an explicit path; the wrapper only wraps bare `ji`.
    RanByPath,
    /// The active shell isn't one ji supports.
    UnsupportedShell(String),
}

enum CdRequest {
    /// Wrapper active; directive written, parent shell will cd.
    Directed,
    /// Wrapper not active; carries the diagnosed reason.
    Unavailable(CdUnavailable),
}

/// Marker error: a *switch*/rescue did its repo-side work but couldn't cd the
/// parent shell. The caller already printed guidance, so `main` maps this to a
/// quiet non-zero exit (no `Error:` line). Empty `Display` on purpose.
#[derive(Debug)]
struct CdNotApplied;

impl std::fmt::Display for CdNotApplied {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}
impl std::error::Error for CdNotApplied {}

/// True when `e` is the [`CdNotApplied`] marker — used by `main` to exit non-zero
/// quietly (the human-readable note was already printed to stderr).
pub fn is_cd_not_applied(e: &anyhow::Error) -> bool {
    e.downcast_ref::<CdNotApplied>().is_some()
}

/// POSIX single-quote a path for a `cd` command line (safe for spaces/quotes).
/// Shared by the directive writer and the rescue/unavailable notes.
fn sq(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Write a `cd` directive for the parent-shell wrapper, or diagnose why we can't.
/// `Err` is reserved for a genuine directive-file write failure (wrapper active
/// but the write fails) — distinct from `Unavailable` (wrapper not active).
fn request_cd(path: &Path) -> Result<CdRequest> {
    match std::env::var("JI_DIRECTIVE_FILE") {
        Ok(f) if !f.is_empty() => {
            std::fs::write(&f, format!("cd {}\n", sq(path)))
                .with_context(|| format!("failed to write directive to {f}"))?;
            Ok(CdRequest::Directed)
        }
        // Unset OR empty → can't cd. (Empty is an abnormal/broken directive; diagnose
        // it like the unset case rather than silently exiting 0.)
        _ => Ok(CdRequest::Unavailable(diagnose_cd_unavailable())),
    }
}

/// How close the on-disk wrapper is to what `ji` would install right now.
/// Only `Working` (byte-exact + nothing shadowing/conflicting) justifies a
/// "restart" hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WrapperState {
    Working,
    Drift,
    Absent,
}

fn diagnose_cd_unavailable() -> CdUnavailable {
    let report = detect_active_report();
    let action = status_action(&report);
    let state = match (&action, ShellEnv::from_process_env().ok()) {
        (StatusAction::Inspect(sh), Some(env)) => wrapper_state(&env, sh),
        _ => WrapperState::Absent,
    };
    cd_reason(
        &action,
        &report.detected,
        report.shell_env_raw.as_deref(),
        state,
        invoked_by_path(),
    )
}

/// Pure decision core (mirrors the `decide`/`status_action` test pattern).
fn cd_reason(
    action: &StatusAction,
    detected: &Detection,
    env_shell: Option<&str>,
    state: WrapperState,
    by_path: bool,
) -> CdUnavailable {
    match action {
        StatusAction::ReportOnly => {
            // Mirror detect_shell/decide's $SHELL fallback: prefer the process-tree
            // unsupported name, else an unsupported $SHELL.
            let unsupported = match detected {
                Detection::Unsupported(name) => Some(name.clone()),
                _ => env_shell
                    .and_then(shell_basename)
                    .and_then(|b| match classify(&b) {
                        Class::Unsupported(n) => Some(n),
                        _ => None,
                    }),
            };
            unsupported.map_or(CdUnavailable::NotInstalled, CdUnavailable::UnsupportedShell)
        }
        StatusAction::Inspect(_) => match state {
            WrapperState::Absent => CdUnavailable::NotInstalled,
            WrapperState::Drift => CdUnavailable::NeedsRefresh,
            WrapperState::Working if by_path => CdUnavailable::RanByPath,
            WrapperState::Working => CdUnavailable::InstalledInactive,
        },
    }
}

/// Pure core: argv[0] contains '/'. Bare `ji` (PATH lookup or `command ji`)
/// yields just `ji`; `./ji` or `/usr/local/bin/ji` contain '/'.
fn invoked_by_path_arg(arg0: &std::ffi::OsStr) -> bool {
    arg0.to_string_lossy().contains('/')
}
fn invoked_by_path() -> bool {
    std::env::args_os()
        .next()
        .as_deref()
        .map(invoked_by_path_arg)
        .unwrap_or(false)
}

/// Is a *working* wrapper installed for `shell`? Only an exact canonical install
/// with nothing shadowing/conflicting counts as `Working`; a scan failure is
/// indeterminate → `Drift` (never falsely "restart"). Reuses the same
/// comparators (`state_for`, `rc_stanza_status`, `find_hits`) that `status` uses.
fn wrapper_state(env: &ShellEnv, shell: &str) -> WrapperState {
    match shell {
        "fish" => {
            let exact = matches!(
                state_for(&fish_primary(env), &render_fish_wrapper_body()),
                "present"
            );
            let shadow = fish_shadow_path(env).exists();
            let Ok(hits) = find_hits(&scan_locations(env, "fish", &fish_primary(env)), env) else {
                return WrapperState::Drift;
            };
            let any_hit = hits.iter().any(|h| {
                matches!(
                    h.kind,
                    HitKind::MarkerBlock { .. }
                        | HitKind::Legacy
                        | HitKind::MalformedMarker
                        | HitKind::UnresolvedSource(_)
                )
            });
            if exact && !shadow && !any_hit {
                WrapperState::Working
            } else if fish_primary(env).exists() || shadow || any_hit {
                WrapperState::Drift
            } else {
                WrapperState::Absent
            }
        }
        "zsh" | "bash" => {
            use clap::CommandFactory;
            let mut cmd = crate::cli::Cli::command();
            let primary = if shell == "zsh" {
                zsh_primary(env)
            } else {
                bash_primary(env)
            };
            let Ok(body) = render_managed_body(shell, &mut cmd) else {
                return WrapperState::Drift;
            };
            let Ok(stanza) = render_rc_stanza(shell) else {
                return WrapperState::Drift;
            };
            let managed_exact = matches!(
                state_for(&env.managed_file(shell).unwrap_or_default(), &body),
                "present"
            );
            let stanza_exact = rc_stanza_status(&primary, &stanza) == TriState::Present;
            let Ok(hits) = find_hits(&scan_locations(env, shell, &primary), env) else {
                return WrapperState::Drift;
            };
            // Working requires EXACTLY the canonical install and nothing else: a single hit
            // that is the primary rc's marker block. Duplicate primary blocks, or any
            // cross-file/legacy/malformed/unresolved hit ⇒ not canonical (review fix).
            let canonical_only = hits.len() == 1
                && hits[0].path == primary
                && matches!(hits[0].kind, HitKind::MarkerBlock { .. });
            let any_integration = hits.iter().any(|h| {
                matches!(
                    h.kind,
                    HitKind::MarkerBlock { .. }
                        | HitKind::Legacy
                        | HitKind::MalformedMarker
                        | HitKind::UnresolvedSource(_)
                )
            });
            if managed_exact && stanza_exact && canonical_only {
                WrapperState::Working
            } else if env.managed_file(shell).map(|m| m.exists()).unwrap_or(false)
                || any_integration
            {
                WrapperState::Drift
            } else {
                WrapperState::Absent
            }
        }
        _ => WrapperState::Absent,
    }
}

/// Per-reason "how to make auto-cd work" line. Shared by the switch/follow-up
/// note and the rescue message's subordinate secondary line.
fn cd_reason_advice(reason: &CdUnavailable) -> String {
    match reason {
        CdUnavailable::NotInstalled => "enable auto-cd with: ji config shell install".into(),
        CdUnavailable::NeedsRefresh => {
            "shell integration on disk doesn't match ji — inspect with: ji config shell status, \
             then reinstall or remove the conflicting entry"
                .into()
        }
        CdUnavailable::InstalledInactive => {
            "shell integration is installed but wasn't active for this command — open a new shell, \
             or run `ji` directly (not via `command ji`/a path)"
                .into()
        }
        CdUnavailable::RanByPath => {
            "invoke `ji` as a bare command — the shell wrapper only wraps `ji`".into()
        }
        CdUnavailable::UnsupportedShell(n) => {
            format!("auto-cd isn't supported for {n} (supported: {SUPPORTED_SHELLS})")
        }
    }
}

fn warn_cd_unavailable(reason: &CdUnavailable, target: &Path) {
    eprintln!("(ji)::cd did not change directory to {}", sq(target));
    eprintln!("(ji)::cd {}", cd_reason_advice(reason));
}

/// The cd policy for a call site, and the outcome the policy maps a request to.
enum Policy {
    /// cd is the operation (switch) → non-zero on unavailable.
    Switch,
    /// cd is incidental to a succeeded op (`new`) → exit 0.
    Followup,
    /// post-close: rescue iff the cwd no longer exists.
    Close { origin_exists: bool },
}
enum CdAction {
    Done,
    Warn(CdUnavailable),
    WarnNonzero(CdUnavailable),
    Rescue(CdUnavailable),
}

/// PURE decision core: request + policy → outcome (no I/O). Unit-tested.
fn decide_cd(req: &CdRequest, policy: Policy) -> CdAction {
    match req {
        CdRequest::Directed => CdAction::Done,
        CdRequest::Unavailable(r) => match policy {
            Policy::Switch => CdAction::WarnNonzero(r.clone()),
            Policy::Followup => CdAction::Warn(r.clone()),
            Policy::Close {
                origin_exists: true,
            } => CdAction::Warn(r.clone()),
            Policy::Close {
                origin_exists: false,
            } => CdAction::Rescue(r.clone()),
        },
    }
}

/// Thin I/O wrapper: render the decided action (printing + exit marker).
fn perform_cd(action: CdAction, safety: &Path) -> Result<()> {
    match action {
        CdAction::Done => Ok(()),
        CdAction::Warn(r) => {
            warn_cd_unavailable(&r, safety);
            Ok(())
        }
        CdAction::WarnNonzero(r) => {
            warn_cd_unavailable(&r, safety);
            Err(CdNotApplied.into())
        }
        // Rescue NEVER uses the generic `warn_cd_unavailable`: it LEADS with the escape,
        // and the reason-advice is a strictly subordinate secondary line.
        CdAction::Rescue(r) => {
            eprintln!(
                "(ji)::cd current directory was removed — run: cd {}",
                sq(safety)
            );
            eprintln!("(ji)::cd {}", cd_reason_advice(&r));
            Err(CdNotApplied.into())
        }
    }
}

/// (1) Switch cd: cd *is* the op. Unavailable → reason note + quiet non-zero.
pub fn apply_switch_cd(path: &Path) -> Result<()> {
    perform_cd(decide_cd(&request_cd(path)?, Policy::Switch), path)
}

/// (2) Follow-up cd: op succeeded, shell dir still valid (CLI `ji new`). A
/// wrapper-inactive cannot-cd prints the note and exits 0; a genuine directive
/// I/O error still propagates (don't swallow disk/permission failures).
pub fn apply_followup_cd(path: &Path) -> Result<()> {
    perform_cd(decide_cd(&request_cd(path)?, Policy::Followup), path)
}

/// (3) Post-close cd: the user's workspace was closed; land them at `safety`
/// (repo root). `origin` is the actual launch cwd (may be a subdirectory), so
/// rescue is keyed to whether `origin` itself still exists. `origin` is probed
/// ONLY when the cd didn't happen — no filesystem cost on the wrapped happy path.
pub fn apply_close_cd(origin: &Path, safety: &Path) -> Result<()> {
    match request_cd(safety) {
        Ok(CdRequest::Directed) => Ok(()),
        Ok(CdRequest::Unavailable(r)) => {
            // `try_exists()` not `exists()`: the latter reports false on a metadata
            // error; treat Err as "still here" (benign) so we never falsely shout.
            let origin_exists = origin.try_exists().unwrap_or(true);
            perform_cd(
                decide_cd(&CdRequest::Unavailable(r), Policy::Close { origin_exists }),
                safety,
            )
        }
        Err(e) => {
            // Rescue guarantee: if the cwd is gone AND the directive write failed, still
            // LEAD with the loud escape — never a bare anyhow dump.
            if origin.try_exists().unwrap_or(true) {
                Err(e)
            } else {
                eprintln!(
                    "(ji)::cd current directory was removed — run: cd {}",
                    sq(safety)
                );
                eprintln!("(ji)::cd (could not write cd directive: {e})");
                Err(CdNotApplied.into())
            }
        }
    }
}

// =====================================================================
// Environment
// =====================================================================

#[derive(Debug, Clone)]
pub struct ShellEnv {
    pub home: PathBuf,
    pub xdg_config_home: PathBuf,
    pub zdotdir: PathBuf,
    pub zsh_custom: Option<PathBuf>,
    pub omz_root: Option<PathBuf>,
}

impl ShellEnv {
    pub fn from_process_env() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME not set")?;
        let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let zdotdir = std::env::var_os("ZDOTDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.clone());
        let omz_root = std::env::var_os("ZSH").map(PathBuf::from).or_else(|| {
            let p = home.join(".oh-my-zsh");
            p.is_dir().then_some(p)
        });
        let zsh_custom = std::env::var_os("ZSH_CUSTOM")
            .map(PathBuf::from)
            .or_else(|| omz_root.as_ref().map(|r| r.join("custom")));
        Ok(Self {
            home,
            xdg_config_home,
            zdotdir,
            zsh_custom,
            omz_root,
        })
    }

    fn managed_file(&self, shell: &str) -> Result<PathBuf> {
        let name = match shell {
            "zsh" => "init.zsh",
            "bash" => "init.bash",
            other => bail!("managed_file is only defined for zsh and bash, got: {other}"),
        };
        Ok(self.xdg_config_home.join("ji").join(name))
    }
}

// =====================================================================
// Options
// =====================================================================

#[derive(Debug, Default, Clone, Copy)]
pub struct InstallOpts {
    pub dry_run: bool,
    pub force: bool,
    /// Show a preview and prompt `[y/N/?]` before writing. Decided at the CLI
    /// boundary (`!yes && stdin().is_terminal()`); defaults false so the library
    /// `install()` is non-interactive (never reads stdin).
    pub interactive: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UninstallOpts {
    pub dry_run: bool,
    pub force: bool,
}

// =====================================================================
// Line classification
// =====================================================================

fn strip_inline_comment(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let b = bytes[i];
        if !in_double && b == b'\'' {
            in_single = !in_single;
        } else if !in_single && b == b'"' {
            in_double = !in_double;
        } else if !in_single
            && !in_double
            && b == b'#'
            && (i == 0 || bytes[i - 1].is_ascii_whitespace())
        {
            return s[..i].trim_end();
        }
        i += 1;
    }
    s.trim_end()
}

/// Heuristic detector for "this line installs ji shell integration". Used only
/// to refuse on cross-file hits. Conservative — requires the line to start with
/// one of a small set of invocation tokens, ruling out comments, aliases, echo
/// statements, etc.
fn is_legacy_line(line: &str) -> bool {
    let line = strip_inline_comment(line.trim_start());
    if line.is_empty() || line.starts_with('#') {
        return false;
    }
    let mentions_init = line.contains("ji config shell init")
        || line.contains("/ji/init.zsh")
        || line.contains("/ji/init.bash")
        || line.contains("/ji/init.fish");
    if !mentions_init {
        return false;
    }
    let first = line.split_whitespace().next().unwrap_or("");
    matches!(
        first,
        "eval" | "source" | "." | "if" | "command" | "type" | "hash" | "ji" | "[" | "[["
    )
}

// =====================================================================
// Sourced-file follow-up
// =====================================================================

#[derive(Debug, Clone)]
enum SourceArg {
    Resolved(Vec<PathBuf>),
    Unresolved(String),
}

fn extract_source_args(env: &ShellEnv, base_dir: &Path, line: &str) -> Vec<SourceArg> {
    let line = strip_inline_comment(line);
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Vec::new();
    }

    if let Some(arg) = extract_for_loop_source(trimmed) {
        return vec![match resolve_path(env, base_dir, &arg) {
            Some(p) => SourceArg::Resolved(expand_glob(&p)),
            None => SourceArg::Unresolved(arg),
        }];
    }

    let work = strip_test_guard(trimmed);
    let work = strip_if_then(work);
    let work = strip_logical_guard(work);

    let arg = match extract_source_directive_arg(work.trim_start()) {
        Some(a) => a,
        None => return Vec::new(),
    };

    vec![match resolve_path(env, base_dir, &arg) {
        Some(p) => SourceArg::Resolved(vec![p]),
        None => SourceArg::Unresolved(arg),
    }]
}

fn strip_if_then(line: &str) -> &str {
    let rest = match line.strip_prefix("if ") {
        Some(r) => r,
        None => return line,
    };
    let (_, body) = match rest.split_once(';') {
        Some(p) => p,
        None => return line,
    };
    let body = body.trim_start();
    let body = body
        .strip_prefix("then ")
        .or_else(|| body.strip_prefix("then\t"))
        .unwrap_or(body);
    let body = body.trim_start();
    body.rsplit_once(';').map(|(s, _)| s.trim()).unwrap_or(body)
}

fn strip_test_guard(line: &str) -> &str {
    if let Some(idx) = line.find("&&") {
        let head = line[..idx].trim();
        if head.starts_with('[') && head.ends_with(']') {
            return line[idx + 2..].trim_start();
        }
    }
    line
}

fn strip_logical_guard(line: &str) -> &str {
    if let Some(idx) = line.find("&&") {
        let head = line[..idx].trim();
        if (head.starts_with("command -v")
            || head.starts_with("type ")
            || head.starts_with("hash "))
            && head.contains("ji")
        {
            return line[idx + 2..].trim_start();
        }
    }
    line
}

fn extract_source_directive_arg(s: &str) -> Option<String> {
    let rest = if let Some(r) = s.strip_prefix(". ").or_else(|| s.strip_prefix(".\t")) {
        r.trim_start()
    } else if let Some(r) = s
        .strip_prefix("source ")
        .or_else(|| s.strip_prefix("source\t"))
    {
        r.trim_start()
    } else {
        return None;
    };
    let arg = match rest.chars().next()? {
        '"' => {
            let end = rest[1..].find('"')?;
            rest[1..1 + end].to_string()
        }
        '\'' => {
            let end = rest[1..].find('\'')?;
            rest[1..1 + end].to_string()
        }
        _ => rest
            .split_whitespace()
            .next()?
            .trim_end_matches(';')
            .to_string(),
    };
    Some(arg)
}

fn extract_for_loop_source(line: &str) -> Option<String> {
    let rest = line.strip_prefix("for ")?;
    let (_var, rest) = rest.split_once(" in ")?;
    let (glob_part, do_part) = rest.split_once(';')?;
    let glob = glob_part.trim();
    if !do_part.contains("source")
        && !do_part.contains(". \"")
        && !do_part.trim_start().starts_with(". ")
    {
        return None;
    }
    Some(glob.to_string())
}

fn resolve_path(env: &ShellEnv, base_dir: &Path, raw: &str) -> Option<PathBuf> {
    let mut s = raw.to_string();
    if let Some(rest) = s.strip_prefix("~/") {
        s = env.home.join(rest).to_string_lossy().into_owned();
    } else if s == "~" {
        s = env.home.to_string_lossy().into_owned();
    }

    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let var = if chars.peek() == Some(&'{') {
            chars.next();
            let mut buf = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                buf.push(c);
            }
            buf
        } else {
            let mut buf = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_alphanumeric() || c == '_' {
                    buf.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            buf
        };
        let value = lookup_var(env, &var)?;
        out.push_str(&value);
    }

    if out.contains('$') {
        return None;
    }

    let p = PathBuf::from(&out);
    if p.is_absolute() {
        Some(p)
    } else {
        Some(base_dir.join(p))
    }
}

fn lookup_var(env: &ShellEnv, raw: &str) -> Option<String> {
    let (name, default) = if let Some(idx) = raw.find(":-") {
        (&raw[..idx], Some(&raw[idx + 2..]))
    } else if let Some(idx) = raw.find('-') {
        (&raw[..idx], Some(&raw[idx + 1..]))
    } else {
        (raw, None)
    };
    let resolved = match name {
        "HOME" => Some(env.home.to_string_lossy().into_owned()),
        "ZDOTDIR" => Some(env.zdotdir.to_string_lossy().into_owned()),
        "XDG_CONFIG_HOME" => Some(env.xdg_config_home.to_string_lossy().into_owned()),
        "ZSH" => env
            .omz_root
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        "ZSH_CUSTOM" => env
            .zsh_custom
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        _ => None,
    };
    if let Some(v) = resolved {
        return Some(v);
    }
    if let Some(d) = default {
        let mut expanded = String::new();
        let mut chars = d.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '$' {
                let var = if chars.peek() == Some(&'{') {
                    chars.next();
                    let mut buf = String::new();
                    for c in chars.by_ref() {
                        if c == '}' {
                            break;
                        }
                        buf.push(c);
                    }
                    buf
                } else {
                    let mut buf = String::new();
                    while let Some(&c) = chars.peek() {
                        if c.is_alphanumeric() || c == '_' {
                            buf.push(c);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    buf
                };
                let v = lookup_var(env, &var)?;
                expanded.push_str(&v);
            } else {
                expanded.push(c);
            }
        }
        return Some(expanded);
    }
    None
}

fn expand_glob(pattern: &Path) -> Vec<PathBuf> {
    let s = pattern.to_string_lossy().into_owned();
    let star_idx = match s.find('*') {
        Some(i) => i,
        None => return vec![pattern.to_path_buf()],
    };
    let dir_end = s[..star_idx].rfind('/').unwrap_or(0);
    let dir = if dir_end == 0 {
        PathBuf::from("/")
    } else {
        PathBuf::from(&s[..dir_end])
    };
    let seg_end = s[star_idx..]
        .find('/')
        .map(|i| star_idx + i)
        .unwrap_or(s.len());
    let segment = &s[dir_end + 1..seg_end];
    let trailing = &s[seg_end..];

    let (prefix, suffix) = match segment.split_once('*') {
        Some(p) => p,
        None => return Vec::new(),
    };

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with(prefix) && name_str.ends_with(suffix) {
            let mut p = dir.join(name_str.as_ref());
            if !trailing.is_empty() {
                let tail = trailing.strip_prefix('/').unwrap_or(trailing);
                if !tail.is_empty() {
                    p = p.join(tail);
                }
            }
            out.push(p);
        }
    }
    out
}

// =====================================================================
// Scan + hit detection
// =====================================================================

#[derive(Debug, Clone)]
struct Hit {
    path: PathBuf,
    line_no: usize,
    line_text: String,
    kind: HitKind,
    via: Option<(PathBuf, usize)>,
}

#[derive(Debug, Clone)]
enum HitKind {
    /// A complete `# >>> … <<<` marker block; byte range covers both marker lines.
    MarkerBlock {
        byte_range: Range<usize>,
    },
    /// A non-marker line that mentions ji integration via eval/source/etc.
    Legacy,
    MalformedMarker,
    UnresolvedSource(String),
}

#[derive(Debug, Clone)]
struct ScanLocation {
    path: PathBuf,
    via: Option<(PathBuf, usize)>,
}

fn scan_locations(env: &ShellEnv, shell: &str, primary: &Path) -> Vec<ScanLocation> {
    let mut out = Vec::new();
    let mut push = |p: PathBuf, via: Option<(PathBuf, usize)>| {
        if !out.iter().any(|l: &ScanLocation| l.path == p) {
            out.push(ScanLocation { path: p, via });
        }
    };

    push(primary.to_path_buf(), None);

    match shell {
        "zsh" => {
            for name in [
                ".zshenv",
                ".zprofile",
                ".zlogin",
                ".zshrc.local",
                ".zshrc.custom",
                ".zshrc.custom.zsh",
            ] {
                push(env.zdotdir.join(name), None);
            }
            if let Some(zc) = &env.zsh_custom
                && let Ok(entries) = std::fs::read_dir(zc)
            {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|s| s.to_str()) == Some("zsh") {
                        push(p, None);
                    }
                }
            }
        }
        "bash" => {
            for name in [
                ".bashrc",
                ".bash_profile",
                ".bash_login",
                ".profile",
                ".bashrc.local",
            ] {
                push(env.home.join(name), None);
            }
            let d = env.home.join(".bashrc.d");
            if let Ok(entries) = std::fs::read_dir(&d) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|s| s.to_str()) == Some("sh") {
                        push(p, None);
                    }
                }
            }
        }
        "fish" => {
            push(env.xdg_config_home.join("fish/config.fish"), None);
            let conf_d = env.xdg_config_home.join("fish/conf.d");
            if let Ok(entries) = std::fs::read_dir(&conf_d) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|s| s.to_str()) == Some("fish") {
                        push(p, None);
                    }
                }
            }
        }
        _ => {}
    }

    if matches!(shell, "zsh" | "bash") && primary.is_file() {
        let contents = std::fs::read_to_string(primary).unwrap_or_default();
        let base_dir = primary
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf();
        for (i, line) in contents.lines().enumerate() {
            for arg in extract_source_args(env, &base_dir, line) {
                if let SourceArg::Resolved(paths) = arg {
                    for p in paths {
                        if p.is_file() {
                            push(p, Some((primary.to_path_buf(), i + 1)));
                        }
                    }
                }
            }
        }
    }

    out
}

fn find_hits(locations: &[ScanLocation], env: &ShellEnv) -> Result<Vec<Hit>> {
    let mut out = Vec::new();
    for loc in locations {
        if !loc.path.is_file() {
            continue;
        }
        let contents = std::fs::read_to_string(&loc.path)
            .with_context(|| format!("failed to read {}", loc.path.display()))?;
        let marker_ranges = find_marker_blocks(&loc.path, &contents, &mut out, loc.via.clone());
        let primary_dir = loc
            .path
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf();
        scan_for_legacy(
            env,
            &loc.path,
            &contents,
            &marker_ranges,
            &primary_dir,
            loc.via.clone(),
            &mut out,
        );
    }
    Ok(out)
}

fn find_marker_blocks(
    path: &Path,
    contents: &str,
    out: &mut Vec<Hit>,
    via: Option<(PathBuf, usize)>,
) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut begin: Option<(usize, usize)> = None;
    let mut byte = 0usize;
    let mut malformed_double_begin = false;
    for (i, line) in contents.split_inclusive('\n').enumerate() {
        let line_trim = line.trim_end_matches('\n');
        if line_trim == MARKER_BEGIN {
            if let Some((_, prev_line)) = begin {
                out.push(Hit {
                    path: path.to_path_buf(),
                    line_no: prev_line,
                    line_text: line_trim.to_string(),
                    kind: HitKind::MalformedMarker,
                    via: via.clone(),
                });
                malformed_double_begin = true;
            }
            begin = Some((byte, i + 1));
        } else if line_trim == MARKER_END {
            match begin.take() {
                Some((start, start_line)) => {
                    let end = byte + line.len();
                    if !malformed_double_begin {
                        ranges.push(start..end);
                        out.push(Hit {
                            path: path.to_path_buf(),
                            line_no: start_line,
                            line_text: MARKER_BEGIN.to_string(),
                            kind: HitKind::MarkerBlock {
                                byte_range: start..end,
                            },
                            via: via.clone(),
                        });
                    }
                    malformed_double_begin = false;
                }
                None => {
                    out.push(Hit {
                        path: path.to_path_buf(),
                        line_no: i + 1,
                        line_text: line_trim.to_string(),
                        kind: HitKind::MalformedMarker,
                        via: via.clone(),
                    });
                }
            }
        }
        byte += line.len();
    }
    if let Some((_, start_line)) = begin {
        out.push(Hit {
            path: path.to_path_buf(),
            line_no: start_line,
            line_text: MARKER_BEGIN.to_string(),
            kind: HitKind::MalformedMarker,
            via,
        });
    }
    ranges
}

fn scan_for_legacy(
    env: &ShellEnv,
    path: &Path,
    contents: &str,
    marker_ranges: &[Range<usize>],
    base_dir: &Path,
    via: Option<(PathBuf, usize)>,
    out: &mut Vec<Hit>,
) {
    let mut byte = 0usize;
    for (i, line) in contents.split_inclusive('\n').enumerate() {
        let in_marker = marker_ranges
            .iter()
            .any(|r| byte >= r.start && byte < r.end);
        if !in_marker {
            let line_trim = line.trim_end_matches('\n');
            if is_legacy_line(line_trim) {
                out.push(Hit {
                    path: path.to_path_buf(),
                    line_no: i + 1,
                    line_text: line_trim.to_string(),
                    kind: HitKind::Legacy,
                    via: via.clone(),
                });
            } else {
                for arg in extract_source_args(env, base_dir, line_trim) {
                    if let SourceArg::Unresolved(raw) = arg {
                        out.push(Hit {
                            path: path.to_path_buf(),
                            line_no: i + 1,
                            line_text: line_trim.to_string(),
                            kind: HitKind::UnresolvedSource(raw),
                            via: via.clone(),
                        });
                    }
                }
            }
        }
        byte += line.len();
    }
}

// =====================================================================
// Managed-body rendering
// =====================================================================

fn render_managed_body(shell: &str, cmd: &mut clap::Command) -> Result<String> {
    let wrapper = wrapper_script(shell)?;
    let completion = completion_script(shell, cmd)?;
    Ok(format!("{MANAGED_HEADER}\n{wrapper}\n{completion}"))
}

fn render_fish_wrapper_body() -> String {
    format!("{MANAGED_HEADER}\n{FISH_WRAPPER}")
}

fn render_fish_completion_body(cmd: &mut clap::Command) -> Result<String> {
    let completion = completion_script("fish", cmd)?;
    Ok(format!("{MANAGED_HEADER}\n{completion}"))
}

fn render_rc_stanza(shell: &str) -> Result<String> {
    let init = match shell {
        "zsh" => "init.zsh",
        "bash" => "init.bash",
        other => bail!("render_rc_stanza is only defined for zsh and bash, got: {other}"),
    };
    Ok(format!(
        "{MARKER_BEGIN}\n{MANAGED_HEADER}. Run `ji config shell uninstall` to remove.\n[ -f \"${{XDG_CONFIG_HOME:-$HOME/.config}}/ji/{init}\" ] \\\n    && . \"${{XDG_CONFIG_HOME:-$HOME/.config}}/ji/{init}\"\n{MARKER_END}\n",
    ))
}

fn is_managed(s: &str) -> bool {
    s.contains(MANAGED_HEADER)
}

// =====================================================================
// Atomic write
// =====================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteOutcome {
    Created,
    Updated,
    Unchanged,
}

fn write_atomic(path: &Path, contents: &str) -> Result<WriteOutcome> {
    let target = if path.exists() {
        path.canonicalize()
            .with_context(|| format!("failed to canonicalize {}", path.display()))?
    } else {
        path.to_path_buf()
    };

    if let Ok(existing) = std::fs::read_to_string(&target)
        && existing == contents
    {
        return Ok(WriteOutcome::Unchanged);
    }

    let mode = std::fs::metadata(&target).ok().map(|m| {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode()
    });
    let mode = mode.unwrap_or(0o644) & 0o7777;

    let parent = target
        .parent()
        .with_context(|| format!("{} has no parent", target.display()))?;
    if !parent.exists() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let pid = std::process::id();
    let file_name = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(".{file_name}.ji.tmp.{pid}"));

    if tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
    }

    let outcome = if target.exists() {
        WriteOutcome::Updated
    } else {
        WriteOutcome::Created
    };

    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        f.write_all(contents.as_bytes())
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        f.sync_all().ok();
    }

    std::fs::rename(&tmp, &target)
        .with_context(|| format!("failed to rename {} -> {}", tmp.display(), target.display()))?;

    if let Ok(dir_f) = std::fs::File::open(parent) {
        let _ = dir_f.sync_all();
    }

    Ok(outcome)
}

// =====================================================================
// Primary targets
// =====================================================================

fn zsh_primary(env: &ShellEnv) -> PathBuf {
    env.zdotdir.join(".zshrc")
}

fn bash_primary(env: &ShellEnv) -> PathBuf {
    for name in [".bash_profile", ".bash_login", ".profile"] {
        let p = env.home.join(name);
        if p.exists() {
            return p;
        }
    }
    env.home.join(".bash_profile")
}

fn fish_primary(env: &ShellEnv) -> PathBuf {
    env.xdg_config_home.join("fish/functions/ji.fish")
}

fn fish_completions_path(env: &ShellEnv) -> PathBuf {
    env.xdg_config_home.join("fish/completions/ji.fish")
}

/// Any file at this path would be auto-sourced by fish at shell start and
/// would define `ji` before our lazy `functions/ji.fish` ever loads —
/// silently shadowing us. Treated as a hard conflict at install time.
fn fish_shadow_path(env: &ShellEnv) -> PathBuf {
    env.xdg_config_home.join("fish/conf.d/ji.fish")
}

// =====================================================================
// chezmoi / dotfile-manager detection
// =====================================================================

fn chezmoi_strong_signal(canonical: &Path) -> bool {
    let parents = canonical.ancestors().skip(1).take(2);
    for p in parents {
        for marker in [
            "chezmoi.toml",
            ".chezmoiroot",
            ".chezmoiignore",
            ".chezmoiversion",
        ] {
            if p.join(marker).exists() {
                return true;
            }
        }
    }
    false
}

fn dotfile_manager_soft_signal(env: &ShellEnv, canonical: &Path) -> bool {
    if !canonical.starts_with(&env.home) {
        return true;
    }
    if let Some(parent) = canonical.parent()
        && let Ok(entries) = std::fs::read_dir(parent)
    {
        for e in entries.flatten() {
            let n = e.file_name();
            let s = n.to_string_lossy();
            if s.starts_with("dot_") || s.starts_with("private_") || s.starts_with("symlink_") {
                return true;
            }
        }
    }
    false
}

// =====================================================================
// Block manipulation
// =====================================================================

fn append_block(rc_contents: &str, stanza: &str) -> String {
    let mut out = rc_contents.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() && !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(stanza);
    out
}

fn replace_block(rc_contents: &str, byte_range: &Range<usize>, stanza: &str) -> String {
    let mut out = String::with_capacity(rc_contents.len());
    out.push_str(&rc_contents[..byte_range.start]);
    out.push_str(stanza);
    out.push_str(&rc_contents[byte_range.end..]);
    out
}

fn remove_block(rc_contents: &str, byte_range: &Range<usize>) -> String {
    let mut out = String::with_capacity(rc_contents.len());
    let start = byte_range.start;
    let mut real_start = start;
    let mut end = byte_range.end;
    let before = &rc_contents[..start];
    if before.ends_with("\n\n") {
        real_start -= 1;
    }
    let after = &rc_contents[end..];
    if after.starts_with('\n') && before.ends_with("\n\n") {
        end += 1;
    }
    out.push_str(&rc_contents[..real_start]);
    out.push_str(&rc_contents[end..]);
    out
}

// =====================================================================
// Install
// =====================================================================

pub fn install(
    env: &ShellEnv,
    shell: &str,
    cmd: &mut clap::Command,
    opts: InstallOpts,
) -> Result<()> {
    match shell {
        "zsh" | "bash" => install_posix(env, shell, cmd, opts),
        "fish" => install_fish(env, cmd, opts),
        other => bail!("unsupported shell: {other} (supported: {SUPPORTED_SHELLS})"),
    }
}

enum Confirm {
    Yes,
    No,
    Reshow,
}

/// Pure: parse a `[y/N/?]` reply. `?` re-shows the preview; anything else
/// (including empty / `n`) declines.
fn parse_confirm(s: &str) -> Confirm {
    match s.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Confirm::Yes,
        "?" => Confirm::Reshow,
        _ => Confirm::No,
    }
}

/// Drives the confirm loop over injected IO (so it's unit-testable). `read_line`
/// returns `None` on EOF. Only called when interactive — no TTY check here.
fn confirm_core(
    mut read_line: impl FnMut() -> std::io::Result<Option<String>>,
    mut prompt: impl FnMut() -> std::io::Result<()>,
    mut preview: impl FnMut(),
) -> std::io::Result<bool> {
    preview();
    loop {
        prompt()?;
        match read_line()? {
            None => return Ok(false), // EOF → decline
            Some(s) => match parse_confirm(&s) {
                Confirm::Yes => return Ok(true),
                Confirm::No => return Ok(false),
                Confirm::Reshow => preview(),
            },
        }
    }
}

/// Live wrapper binding stdin/stderr into `confirm_core` (called only when
/// `opts.interactive` and there's a change to confirm).
fn confirm_install(preview: impl Fn()) -> Result<bool> {
    use std::io::Write;
    confirm_core(
        || {
            let mut l = String::new();
            match std::io::stdin().read_line(&mut l)? {
                0 => Ok(None),
                _ => Ok(Some(l)),
            }
        },
        || {
            let mut e = std::io::stderr();
            write!(e, "(ji)::shell apply these changes? [y/N/?] ")?;
            e.flush()
        },
        preview,
    )
    .map_err(Into::into)
}

fn install_posix(
    env: &ShellEnv,
    shell: &str,
    cmd: &mut clap::Command,
    opts: InstallOpts,
) -> Result<()> {
    let primary = if shell == "zsh" {
        zsh_primary(env)
    } else {
        bash_primary(env)
    };

    if primary.exists() {
        let canonical = primary
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", primary.display()))?;
        if chezmoi_strong_signal(&canonical) && !opts.force {
            bail!(
                "(ji)::shell: {} resolves into a chezmoi source directory ({}); edits will be reverted by `chezmoi apply`. Pass --force to install anyway.",
                primary.display(),
                canonical.display(),
            );
        }
        if dotfile_manager_soft_signal(env, &canonical) {
            eprintln!(
                "(ji)::shell: note: {} resolves to {} (outside $HOME or dotfile-manager-shaped layout); edits may be reverted by your dotfile tool.",
                primary.display(),
                canonical.display(),
            );
        }
        let md = std::fs::metadata(&primary)
            .with_context(|| format!("failed to stat {}", primary.display()))?;
        if md.permissions().readonly() {
            bail!("(ji)::shell: {} is not writable", primary.display());
        }
    }

    let locations = scan_locations(env, shell, &primary);
    let hits = find_hits(&locations, env)?;

    let malformed: Vec<&Hit> = hits
        .iter()
        .filter(|h| matches!(h.kind, HitKind::MalformedMarker))
        .collect();
    if !malformed.is_empty() && !opts.force {
        let first = malformed[0];
        bail!(
            "(ji)::shell: malformed marker block in {}:{} — edit manually or pass --force to overwrite",
            first.path.display(),
            first.line_no,
        );
    }

    let primary_marker = hits
        .iter()
        .find(|h| h.path == primary && matches!(h.kind, HitKind::MarkerBlock { .. }));
    let primary_legacy = hits
        .iter()
        .find(|h| h.path == primary && matches!(h.kind, HitKind::Legacy));
    let nonprimary_hits: Vec<&Hit> = hits
        .iter()
        .filter(|h| {
            h.path != primary && matches!(h.kind, HitKind::MarkerBlock { .. } | HitKind::Legacy)
        })
        .collect();
    let unresolved: Vec<&Hit> = hits
        .iter()
        .filter(|h| matches!(h.kind, HitKind::UnresolvedSource(_)))
        .collect();

    for u in &unresolved {
        if let HitKind::UnresolvedSource(raw) = &u.kind {
            eprintln!(
                "(ji)::shell: note: unresolved source-like line in {}:{} (`{}`)",
                u.path.display(),
                u.line_no,
                raw,
            );
        }
    }

    if !nonprimary_hits.is_empty() && !opts.force {
        let first = nonprimary_hits[0];
        let via = first
            .via
            .as_ref()
            .map(|(p, n)| format!(" (via source line {} in {})", n, p.display()))
            .unwrap_or_default();
        bail!(
            "(ji)::shell: existing integration found in {}:{}{} — remove it manually, or pass --force to install anyway.",
            first.path.display(),
            first.line_no,
            via,
        );
    }

    if let Some(h) = primary_legacy
        && primary_marker.is_none()
        && !opts.force
    {
        bail!(
            "(ji)::shell: existing integration line found in {}:{} — remove it manually, or pass --force to install alongside it.",
            h.path.display(),
            h.line_no,
        );
    }

    let managed_path = env.managed_file(shell)?;
    let managed_body = render_managed_body(shell, cmd)?;
    let stanza = render_rc_stanza(shell)?;

    let primary_contents = if primary.exists() {
        std::fs::read_to_string(&primary)
            .with_context(|| format!("failed to read {}", primary.display()))?
    } else {
        String::new()
    };

    let new_primary_contents = if let Some(h) = primary_marker {
        let HitKind::MarkerBlock { byte_range } = &h.kind else {
            bail!(
                "internal: primary_marker hit at {}:{} had unexpected kind (expected MarkerBlock)",
                h.path.display(),
                h.line_no,
            );
        };
        let after = &primary_contents[byte_range.end..];
        let trailing = if after.is_empty() || after.starts_with('\n') {
            ""
        } else {
            "\n"
        };
        let block = format!("{stanza}{trailing}");
        replace_block(&primary_contents, byte_range, &block)
    } else {
        append_block(&primary_contents, &stanza)
    };

    // One diff-set, shared by dry-run and the interactive preview (no drift).
    let diffs = [
        (
            managed_path.clone(),
            existing_or_empty(&managed_path),
            managed_body.clone(),
        ),
        (
            primary.clone(),
            primary_contents.clone(),
            new_primary_contents.clone(),
        ),
    ];
    let changed = diffs.iter().any(|(_, before, after)| before != after);
    let show = || {
        for (p, before, after) in &diffs {
            print_dry_run_diff(p, before, after);
        }
    };
    if opts.dry_run {
        show();
        return Ok(());
    }
    // Skip the prompt when nothing would change (already up to date): no diff to confirm.
    if opts.interactive && changed && !confirm_install(show)? {
        eprintln!("(ji)::shell install cancelled");
        return Ok(());
    }

    let managed_outcome = write_atomic(&managed_path, &managed_body)?;
    match managed_outcome {
        WriteOutcome::Created => eprintln!(
            "(ji)::shell installed managed file {}",
            managed_path.display()
        ),
        WriteOutcome::Updated => eprintln!(
            "(ji)::shell updated managed file {}",
            managed_path.display()
        ),
        WriteOutcome::Unchanged => {}
    }

    let primary_outcome = if new_primary_contents == primary_contents {
        WriteOutcome::Unchanged
    } else {
        if !primary.exists()
            && let Some(parent) = primary.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        write_atomic(&primary, &new_primary_contents)?
    };
    match primary_outcome {
        WriteOutcome::Created => eprintln!(
            "(ji)::shell created {} with managed block",
            primary.display()
        ),
        WriteOutcome::Updated => eprintln!(
            "(ji)::shell updated {} with managed block",
            primary.display()
        ),
        WriteOutcome::Unchanged => {
            eprintln!("(ji)::shell {} already up-to-date", primary.display());
        }
    }

    Ok(())
}

fn existing_or_empty(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn print_dry_run_diff(path: &Path, before: &str, after: &str) {
    if before == after {
        return;
    }
    println!("--- {}", path.display());
    println!("+++ {} (after install)", path.display());
    for line in before.lines() {
        println!("- {line}");
    }
    for line in after.lines() {
        println!("+ {line}");
    }
    println!();
}

fn install_fish(env: &ShellEnv, cmd: &mut clap::Command, opts: InstallOpts) -> Result<()> {
    let primary = fish_primary(env);
    let completions = fish_completions_path(env);
    let shadow = fish_shadow_path(env);

    if shadow.exists() && !opts.force {
        bail!(
            "(ji)::shell: {} exists and would shadow our `functions/ji.fish` (fish auto-sources conf.d at startup, blocking the autoload) — remove it manually, or pass --force.",
            shadow.display(),
        );
    }

    let locations = scan_locations(env, "fish", &primary);
    let hits = find_hits(&locations, env)?;

    let malformed: Vec<&Hit> = hits
        .iter()
        .filter(|h| matches!(h.kind, HitKind::MalformedMarker))
        .collect();
    if !malformed.is_empty() && !opts.force {
        let first = malformed[0];
        bail!(
            "(ji)::shell: malformed marker block in {}:{} — edit manually or pass --force to overwrite",
            first.path.display(),
            first.line_no,
        );
    }

    let nonprimary_hits: Vec<&Hit> = hits
        .iter()
        .filter(|h| {
            h.path != primary && matches!(h.kind, HitKind::MarkerBlock { .. } | HitKind::Legacy)
        })
        .collect();

    if !nonprimary_hits.is_empty() && !opts.force {
        let first = nonprimary_hits[0];
        bail!(
            "(ji)::shell: existing integration found in {}:{} — remove it manually, or pass --force to install anyway.",
            first.path.display(),
            first.line_no,
        );
    }

    let wrapper_body = render_fish_wrapper_body();
    let completion_body = render_fish_completion_body(cmd)?;

    for (path, label) in [(&primary, "wrapper"), (&completions, "completions")] {
        if path.exists() {
            let existing = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            if !is_managed(&existing) && !opts.force {
                bail!(
                    "(ji)::shell: {} exists and is not ji-managed — remove it manually, or pass --force to overwrite ({label}).",
                    path.display(),
                );
            }
        }
    }

    let diffs = [
        (
            primary.clone(),
            existing_or_empty(&primary),
            wrapper_body.clone(),
        ),
        (
            completions.clone(),
            existing_or_empty(&completions),
            completion_body.clone(),
        ),
    ];
    let changed = diffs.iter().any(|(_, before, after)| before != after);
    let show = || {
        for (p, before, after) in &diffs {
            print_dry_run_diff(p, before, after);
        }
    };
    if opts.dry_run {
        show();
        return Ok(());
    }
    if opts.interactive && changed && !confirm_install(show)? {
        eprintln!("(ji)::shell install cancelled");
        return Ok(());
    }

    let wo = write_atomic(&primary, &wrapper_body)?;
    match wo {
        WriteOutcome::Created => {
            eprintln!("(ji)::shell wrapper installed in {}", primary.display());
        }
        WriteOutcome::Updated => eprintln!("(ji)::shell wrapper updated in {}", primary.display()),
        WriteOutcome::Unchanged => eprintln!(
            "(ji)::shell wrapper already up-to-date in {}",
            primary.display()
        ),
    }
    let co = write_atomic(&completions, &completion_body)?;
    match co {
        WriteOutcome::Created => eprintln!(
            "(ji)::shell completions installed in {}",
            completions.display()
        ),
        WriteOutcome::Updated => eprintln!(
            "(ji)::shell completions updated in {}",
            completions.display()
        ),
        WriteOutcome::Unchanged => eprintln!(
            "(ji)::shell completions already up-to-date in {}",
            completions.display()
        ),
    }

    Ok(())
}

// =====================================================================
// Uninstall
// =====================================================================

pub fn uninstall(env: &ShellEnv, shell: &str, opts: UninstallOpts) -> Result<()> {
    match shell {
        "zsh" | "bash" => uninstall_posix(env, shell, opts),
        "fish" => uninstall_fish(env, opts),
        other => bail!("unsupported shell: {other} (supported: {SUPPORTED_SHELLS})"),
    }
}

fn uninstall_posix(env: &ShellEnv, shell: &str, opts: UninstallOpts) -> Result<()> {
    let primary = if shell == "zsh" {
        zsh_primary(env)
    } else {
        bash_primary(env)
    };
    let managed = env.managed_file(shell)?;

    let mut nothing_to_do = true;

    if primary.is_file() {
        let contents = std::fs::read_to_string(&primary)
            .with_context(|| format!("failed to read {}", primary.display()))?;
        let mut hits = Vec::new();
        let ranges = find_marker_blocks(&primary, &contents, &mut hits, None);
        if let Some(range) = ranges.first() {
            let after = remove_block(&contents, range);
            if opts.dry_run {
                print_dry_run_diff(&primary, &contents, &after);
            } else {
                write_atomic(&primary, &after)?;
                eprintln!(
                    "(ji)::shell removed managed block from {}",
                    primary.display()
                );
            }
            nothing_to_do = false;
        }
    }

    if managed.is_file() {
        if opts.dry_run {
            println!("- would delete {}", managed.display());
        } else {
            std::fs::remove_file(&managed)
                .with_context(|| format!("failed to remove {}", managed.display()))?;
            eprintln!("(ji)::shell removed managed file {}", managed.display());
        }
        nothing_to_do = false;
    }

    let _ = opts.force; // reserved for symmetry with install; nothing to override here

    if nothing_to_do {
        eprintln!("(ji)::shell nothing to uninstall for {shell}");
    }
    Ok(())
}

fn uninstall_fish(env: &ShellEnv, opts: UninstallOpts) -> Result<()> {
    let primary = fish_primary(env);
    let completions = fish_completions_path(env);
    let shadow = fish_shadow_path(env);

    let mut nothing_to_do = true;

    for path in [&primary, &completions, &shadow] {
        if !path.is_file() {
            continue;
        }
        let existing = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if is_managed(&existing) || opts.force {
            if opts.dry_run {
                println!("- would delete {}", path.display());
            } else {
                std::fs::remove_file(path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
                eprintln!("(ji)::shell removed {}", path.display());
            }
            nothing_to_do = false;
        } else {
            eprintln!(
                "(ji)::shell: leaving {} (not ji-managed; pass --force to remove)",
                path.display()
            );
        }
    }

    if nothing_to_do {
        eprintln!("(ji)::shell nothing to uninstall for fish");
    }
    Ok(())
}

// =====================================================================
// Status
// =====================================================================

/// Non-fatal snapshot of the active shell for the `status` diagnostic. Built
/// from the same process-tree walk as [`detect_shell`]; never errors.
#[derive(Debug, Clone)]
struct ActiveReport {
    detected: Detection,
    /// Executable path of the nearest shell ancestor (for display).
    active_exe: Option<PathBuf>,
    /// Raw `$SHELL` value (full path), if set and non-empty.
    shell_env_raw: Option<String>,
}

fn detect_active_report() -> ActiveReport {
    let me = std::process::id() as i32;
    let (detected, exe) = resolve_in_tree(me, live_parent, live_exe_path, MAX_WALK_DEPTH);
    ActiveReport {
        detected,
        active_exe: exe.map(PathBuf::from),
        shell_env_raw: std::env::var("SHELL").ok().filter(|s| !s.is_empty()),
    }
}

/// Which shell's integration `status` should inspect. `$SHELL` is only a
/// candidate when the tree walk found no shell at all — mirroring the
/// detection rule so we never inspect `$SHELL` after seeing a real shell.
/// Pure over [`ActiveReport`] so it is unit-testable.
#[derive(Debug, PartialEq, Eq)]
enum StatusAction {
    Inspect(String),
    ReportOnly,
}

fn status_action(r: &ActiveReport) -> StatusAction {
    match &r.detected {
        Detection::Supported(s) => StatusAction::Inspect(s.clone()),
        // A shell was identified but isn't supported — report it, don't fall back to $SHELL.
        Detection::Unsupported(_) => StatusAction::ReportOnly,
        Detection::Unknown => {
            match r
                .shell_env_raw
                .as_deref()
                .and_then(shell_basename)
                .map(|b| classify(&b))
            {
                Some(Class::Supported(s)) => StatusAction::Inspect(s),
                _ => StatusAction::ReportOnly,
            }
        }
    }
}

fn print_active_report(r: &ActiveReport) {
    let active = match &r.detected {
        Detection::Supported(s) => s.clone(),
        Detection::Unsupported(name) => format!("{name} (unsupported)"),
        Detection::Unknown => "unknown".to_string(),
    };
    let exe = r
        .active_exe
        .as_ref()
        .map(|p| format!("   [{}]", p.display()))
        .unwrap_or_default();
    println!("  active shell (detected): {active}{exe}");
    println!(
        "  login shell ($SHELL):    {}",
        r.shell_env_raw.as_deref().unwrap_or("<unset>")
    );
}

/// Report shell-integration state. The detection step is **non-fatal**: an
/// unsupported/unknown active shell is reported, not an error (only the
/// mutating commands error on that). Genuine I/O errors from the underlying
/// inspectors still propagate.
pub fn status(env: &ShellEnv, target: Option<&str>, cmd: &mut clap::Command) -> Result<()> {
    let report = detect_active_report();
    print_active_report(&report);

    let shell = match target {
        Some(t) => t.to_string(),
        None => match status_action(&report) {
            StatusAction::Inspect(s) => s,
            StatusAction::ReportOnly => {
                println!(
                    "  no supported active shell to inspect; pass one explicitly (zsh|bash|fish)"
                );
                return Ok(());
            }
        },
    };

    match shell.as_str() {
        "zsh" | "bash" => status_posix(env, &shell, cmd),
        "fish" => status_fish(env, cmd),
        other => bail!("unsupported shell: {other} (supported: {SUPPORTED_SHELLS})"),
    }
}

fn status_posix(env: &ShellEnv, shell: &str, cmd: &mut clap::Command) -> Result<()> {
    let primary = if shell == "zsh" {
        zsh_primary(env)
    } else {
        bash_primary(env)
    };
    let managed = env.managed_file(shell)?;
    let body = render_managed_body(shell, cmd)?;
    let stanza = render_rc_stanza(shell)?;

    println!("ji shell integration: {shell}");
    println!("  primary rc: {}", display_canonical(&primary));
    if shell == "bash" {
        let candidates: Vec<String> = [".bash_profile", ".bash_login", ".profile"]
            .iter()
            .map(|n| {
                let p = env.home.join(n);
                format!(
                    "{} ({})",
                    p.display(),
                    if p.exists() { "exists" } else { "absent" }
                )
            })
            .collect();
        println!("  bash candidates: {}", candidates.join(", "));
    }
    println!(
        "  managed file: {} ({})",
        display_canonical(&managed),
        state_for(&managed, &body)
    );
    println!("  rc stanza: {}", rc_stanza_state(&primary, &stanza));

    let locations = scan_locations(env, shell, &primary);
    let hits = find_hits(&locations, env)?;
    let mut cross: Vec<&Hit> = hits
        .iter()
        .filter(|h| {
            h.path != primary && matches!(h.kind, HitKind::MarkerBlock { .. } | HitKind::Legacy)
        })
        .collect();
    cross.sort_by_key(|h| (h.path.clone(), h.line_no));
    if cross.is_empty() {
        println!("  cross-file hits: none");
    } else {
        println!("  cross-file hits:");
        for h in cross {
            let via = h
                .via
                .as_ref()
                .map(|(p, n)| format!(" (via source line {} in {})", n, p.display()))
                .unwrap_or_default();
            println!(
                "    {}:{}{} — {}",
                h.path.display(),
                h.line_no,
                via,
                h.line_text
            );
        }
    }
    let unresolved: Vec<&Hit> = hits
        .iter()
        .filter(|h| matches!(h.kind, HitKind::UnresolvedSource(_)))
        .collect();
    if !unresolved.is_empty() {
        println!("  unresolved source lines:");
        for h in unresolved {
            println!("    {}:{} — {}", h.path.display(), h.line_no, h.line_text);
        }
    }

    Ok(())
}

fn status_fish(env: &ShellEnv, cmd: &mut clap::Command) -> Result<()> {
    let primary = fish_primary(env);
    let completions = fish_completions_path(env);
    let shadow = fish_shadow_path(env);
    let wrapper_body = render_fish_wrapper_body();
    let completion_body = render_fish_completion_body(cmd)?;

    println!("ji shell integration: fish");
    println!(
        "  wrapper file: {} ({})",
        display_canonical(&primary),
        state_for(&primary, &wrapper_body)
    );
    println!(
        "  completions file: {} ({})",
        display_canonical(&completions),
        state_for(&completions, &completion_body)
    );
    if shadow.exists() {
        println!(
            "  WARNING shadow file: {} (would override the autoload)",
            display_canonical(&shadow)
        );
    }

    let locations = scan_locations(env, "fish", &primary);
    let hits = find_hits(&locations, env)?;
    let cross: Vec<&Hit> = hits
        .iter()
        .filter(|h| {
            h.path != primary && matches!(h.kind, HitKind::MarkerBlock { .. } | HitKind::Legacy)
        })
        .collect();
    if cross.is_empty() {
        println!("  cross-file hits: none");
    } else {
        println!("  cross-file hits:");
        for h in cross {
            println!("    {}:{} — {}", h.path.display(), h.line_no, h.line_text);
        }
    }
    Ok(())
}

fn state_for(path: &Path, desired: &str) -> &'static str {
    if !path.exists() {
        return "absent";
    }
    match std::fs::read_to_string(path) {
        Ok(s) if s == desired => "present",
        Ok(_) => "drift",
        Err(_) => "absent",
    }
}

/// Whether the rc's ji marker block exactly matches the canonical `stanza`.
/// Shared comparator for `status` (formatted below) and cd diagnosis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriState {
    Present,
    Drift,
    Absent,
    Unreadable,
}

fn rc_stanza_status(rc: &Path, stanza: &str) -> TriState {
    if !rc.is_file() {
        return TriState::Absent;
    }
    let contents = match std::fs::read_to_string(rc) {
        Ok(s) => s,
        Err(_) => return TriState::Unreadable,
    };
    let mut tmp = Vec::new();
    let ranges = find_marker_blocks(rc, &contents, &mut tmp, None);
    match ranges.first() {
        None => TriState::Absent,
        Some(range) => {
            let block = &contents[range.clone()];
            if block.trim_end_matches('\n') == stanza.trim_end_matches('\n') {
                TriState::Present
            } else {
                TriState::Drift
            }
        }
    }
}

fn rc_stanza_state(rc: &Path, stanza: &str) -> String {
    let label = match rc_stanza_status(rc, stanza) {
        TriState::Present => "present",
        TriState::Drift => "drift",
        TriState::Absent => "absent",
        TriState::Unreadable => "unreadable",
    };
    format!("{} ({label})", rc.display())
}

fn display_canonical(p: &Path) -> String {
    match p.canonicalize() {
        Ok(c) if c != p => format!("{} -> {}", p.display(), c.display()),
        _ => p.display().to_string(),
    }
}

#[cfg(test)]
mod detection_tests {
    use super::*;
    use std::collections::HashMap;

    // ---- classify / registry ----

    #[test]
    fn classify_supported_unsupported_and_non_shells() {
        for s in ["zsh", "bash", "fish"] {
            assert_eq!(classify(s), Class::Supported(s.to_string()));
        }
        for s in ["sh", "dash", "nu", "nushell", "tcsh", "pwsh", "xonsh"] {
            assert_eq!(classify(s), Class::Unsupported(s.to_string()));
        }
        for s in ["make", "python", "login", "ji", "code"] {
            assert_eq!(classify(s), Class::NotAShell);
        }
    }

    #[test]
    fn every_unsupported_registry_entry_classifies_unsupported() {
        for &s in UNSUPPORTED_SHELLS {
            assert_eq!(
                classify(s),
                Class::Unsupported(s.to_string()),
                "registry entry {s}"
            );
        }
    }

    #[test]
    fn supported_display_string_stays_in_sync() {
        assert_eq!(SUPPORTED.join(", "), SUPPORTED_SHELLS);
    }

    // ---- basename normalization ----

    #[test]
    fn basename_strips_path_dash_and_lowercases() {
        assert_eq!(shell_basename("/bin/zsh").as_deref(), Some("zsh"));
        assert_eq!(shell_basename("-zsh").as_deref(), Some("zsh"));
        assert_eq!(
            shell_basename("/opt/homebrew/bin/fish").as_deref(),
            Some("fish")
        );
        assert_eq!(shell_basename("/usr/bin/ZSH").as_deref(), Some("zsh"));
        assert_eq!(shell_basename(""), None);
        assert_eq!(shell_basename("-"), None);
    }

    // ---- tree walk ----

    /// Walk a synthetic process tree described as `(pid, ppid, exe_path)` rows.
    /// A node with an empty exe string models a `name_of` lookup failure.
    fn walk(start: i32, rows: &[(i32, i32, &str)]) -> (Detection, Option<String>) {
        let parents: HashMap<i32, i32> = rows.iter().map(|&(p, pp, _)| (p, pp)).collect();
        let names: HashMap<i32, String> = rows
            .iter()
            .filter(|&&(_, _, n)| !n.is_empty())
            .map(|&(p, _, n)| (p, n.to_string()))
            .collect();
        resolve_in_tree(
            start,
            |p| parents.get(&p).copied().filter(|&pp| pp > 1),
            |p| names.get(&p).cloned(),
            MAX_WALK_DEPTH,
        )
    }

    #[test]
    fn direct_parent_supported_shell() {
        let (d, exe) = walk(100, &[(100, 50, "ji"), (50, 40, "/bin/zsh")]);
        assert_eq!(d, Detection::Supported("zsh".into()));
        assert_eq!(exe.as_deref(), Some("/bin/zsh"));
    }

    #[test]
    fn walks_through_non_shell_intermediaries() {
        // make, then bash.
        let (d, _) = walk(
            100,
            &[(100, 50, "ji"), (50, 40, "make"), (40, 30, "/bin/bash")],
        );
        assert_eq!(d, Detection::Supported("bash".into()));
        // sudo -> env -> fish.
        let (d, _) = walk(
            10,
            &[(10, 9, "ji"), (9, 8, "sudo"), (8, 7, "env"), (7, 6, "fish")],
        );
        assert_eq!(d, Detection::Supported("fish".into()));
    }

    #[test]
    fn nearest_shell_wins_sh_below_fish() {
        // sh is the nearest shell ancestor; fish is above it. sh wins (unsupported),
        // and $SHELL is never consulted by the walk.
        let (d, _) = walk(100, &[(100, 50, "ji"), (50, 40, "sh"), (40, 30, "fish")]);
        assert_eq!(d, Detection::Unsupported("sh".into()));
    }

    #[test]
    fn direct_parent_unsupported_shell() {
        let (d, _) = walk(100, &[(100, 50, "ji"), (50, 40, "nu")]);
        assert_eq!(d, Detection::Unsupported("nu".into()));
    }

    #[test]
    fn no_shell_chain_terminating_at_init_is_unknown() {
        // ppid 1 stops the walk; no recognized shell seen.
        let (d, exe) = walk(100, &[(100, 50, "ji"), (50, 40, "make"), (40, 1, "login")]);
        assert_eq!(d, Detection::Unknown);
        assert_eq!(exe, None);
    }

    #[test]
    fn name_lookup_failure_is_skipped() {
        // pid 40 has no name (lookup failure) but a real shell sits above it.
        let (d, _) = walk(
            100,
            &[
                (100, 50, "ji"),
                (50, 40, "make"),
                (40, 30, ""),
                (30, 20, "/bin/zsh"),
            ],
        );
        assert_eq!(d, Detection::Supported("zsh".into()));
    }

    #[test]
    fn depth_cap_bounds_the_walk() {
        // A non-shell chain longer than the cap, with a shell only beyond it → Unknown.
        let deep = 101 + MAX_WALK_DEPTH as i32 + 5;
        let mut rows: Vec<(i32, i32, &str)> = vec![(100, 101, "ji")];
        for pid in 101..deep {
            rows.push((pid, pid + 1, "make"));
        }
        rows.push((deep, deep + 1, "zsh")); // beyond MAX_WALK_DEPTH from the start
        let (d, _) = walk(100, &rows);
        assert_eq!(d, Detection::Unknown);
    }

    #[test]
    fn cycle_is_bounded() {
        // 50 <-> 60 cycle, all non-shells: the depth cap terminates the walk.
        let (d, _) = walk(100, &[(100, 50, "ji"), (50, 60, "make"), (60, 50, "make")]);
        assert_eq!(d, Detection::Unknown);
    }

    // ---- decide (full fallback contract, raw $SHELL) ----

    #[test]
    fn decide_active_supported_wins() {
        assert_eq!(
            decide(Detection::Supported("zsh".into()), Some("/bin/bash")).unwrap(),
            "zsh"
        );
    }

    #[test]
    fn decide_active_unsupported_errors_and_ignores_shell_env() {
        let err = decide(Detection::Unsupported("nu".into()), Some("/bin/zsh")).unwrap_err();
        let m = format!("{err}");
        assert!(m.contains("detected active shell 'nu'"), "{m}");
        // It is an error, so it can never have returned the $SHELL-derived "zsh".
    }

    #[test]
    fn decide_unknown_uses_supported_shell_env() {
        assert_eq!(
            decide(Detection::Unknown, Some("/usr/bin/zsh")).unwrap(),
            "zsh"
        );
        assert_eq!(decide(Detection::Unknown, Some("-bash")).unwrap(), "bash");
    }

    #[test]
    fn decide_unknown_unsupported_shell_env_has_distinct_message() {
        let err = decide(Detection::Unknown, Some("/bin/tcsh")).unwrap_err();
        let m = format!("{err}");
        assert!(m.contains("tcsh"), "{m}");
        assert!(
            m.contains("login shell"),
            "should name $SHELL as the source: {m}"
        );
    }

    #[test]
    fn decide_unknown_nonshell_shell_env() {
        let err = decide(Detection::Unknown, Some("/usr/bin/whatever")).unwrap_err();
        assert!(format!("{err}").contains("could not determine"));
    }

    #[test]
    fn decide_unknown_unset_shell_env() {
        assert!(
            format!("{}", decide(Detection::Unknown, None).unwrap_err()).contains("SHELL not set")
        );
        assert!(
            format!("{}", decide(Detection::Unknown, Some("")).unwrap_err())
                .contains("SHELL not set")
        );
    }

    // ---- status_action (pure status selection) ----

    fn report(detected: Detection, shell_env: Option<&str>) -> ActiveReport {
        ActiveReport {
            detected,
            active_exe: None,
            shell_env_raw: shell_env.map(String::from),
        }
    }

    #[test]
    fn status_inspects_supported_active() {
        assert_eq!(
            status_action(&report(
                Detection::Supported("zsh".into()),
                Some("/bin/bash")
            )),
            StatusAction::Inspect("zsh".into())
        );
    }

    #[test]
    fn status_unsupported_active_reports_only_never_shell_env() {
        assert_eq!(
            status_action(&report(
                Detection::Unsupported("nu".into()),
                Some("/bin/zsh")
            )),
            StatusAction::ReportOnly
        );
    }

    #[test]
    fn status_unknown_active_may_use_supported_shell_env() {
        assert_eq!(
            status_action(&report(Detection::Unknown, Some("/usr/bin/fish"))),
            StatusAction::Inspect("fish".into())
        );
        assert_eq!(
            status_action(&report(Detection::Unknown, Some("/bin/tcsh"))),
            StatusAction::ReportOnly
        );
        assert_eq!(
            status_action(&report(Detection::Unknown, None)),
            StatusAction::ReportOnly
        );
    }

    // ---- live smoke test (environment-dependent) ----

    #[test]
    fn detect_shell_live_is_ok_or_clear_error() {
        match detect_shell() {
            Ok(s) => assert!(
                SUPPORTED.contains(&s.as_str()),
                "returned unsupported shell {s}"
            ),
            Err(e) => {
                let m = format!("{e}");
                assert!(
                    m.contains("shell") || m.contains("SHELL"),
                    "unexpected error: {m}"
                );
            }
        }
    }
}

#[cfg(test)]
mod cd_tests {
    use super::*;
    use clap::CommandFactory;
    use tempfile::TempDir;

    fn env_for(tmp: &TempDir) -> ShellEnv {
        let home = tmp.path().to_path_buf();
        ShellEnv {
            home: home.clone(),
            xdg_config_home: home.join(".config"),
            zdotdir: home,
            zsh_custom: None,
            omz_root: None,
        }
    }

    // ---- sq quoting ----

    #[test]
    fn sq_quotes_spaces_and_single_quotes() {
        assert_eq!(sq(Path::new("/a/b")), "'/a/b'");
        assert_eq!(sq(Path::new("/a b/c")), "'/a b/c'");
        assert_eq!(sq(Path::new("/a'b")), "'/a'\\''b'");
    }

    // ---- argv[0] explicit-path detection ----

    #[test]
    fn invoked_by_path_arg_detects_slash() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        assert!(!invoked_by_path_arg(OsStr::new("ji")));
        assert!(invoked_by_path_arg(OsStr::new("./ji")));
        assert!(invoked_by_path_arg(OsStr::new("/usr/local/bin/ji")));
        assert!(invoked_by_path_arg(OsStr::from_bytes(b"/\xff/ji")));
    }

    // ---- [y/N/?] parsing + loop ----

    #[test]
    fn parse_confirm_cases() {
        assert!(matches!(parse_confirm("y"), Confirm::Yes));
        assert!(matches!(parse_confirm("Y\n"), Confirm::Yes));
        assert!(matches!(parse_confirm("yes"), Confirm::Yes));
        assert!(matches!(parse_confirm("?"), Confirm::Reshow));
        assert!(matches!(parse_confirm(""), Confirm::No));
        assert!(matches!(parse_confirm("n"), Confirm::No));
        assert!(matches!(parse_confirm("garbage"), Confirm::No));
    }

    #[test]
    fn confirm_core_eof_declines_after_one_prompt() {
        let mut previews = 0;
        let mut prompts = 0;
        let r = confirm_core(
            || Ok(None),
            || {
                prompts += 1;
                Ok(())
            },
            || previews += 1,
        )
        .unwrap();
        assert!(!r);
        assert_eq!(previews, 1);
        assert_eq!(prompts, 1);
    }

    #[test]
    fn confirm_core_yes() {
        let r = confirm_core(|| Ok(Some("y\n".into())), || Ok(()), || {}).unwrap();
        assert!(r);
    }

    #[test]
    fn confirm_core_reshow_then_no_previews_twice() {
        let mut previews = 0;
        let mut answers = ["?\n".to_string(), "n\n".to_string()].into_iter();
        let r = confirm_core(|| Ok(answers.next()), || Ok(()), || previews += 1).unwrap();
        assert!(!r);
        assert_eq!(previews, 2); // initial + one reshow
    }

    // ---- cd_reason (pure) ----

    #[test]
    fn cd_reason_inspect_states() {
        let inspect = StatusAction::Inspect("zsh".into());
        let unknown = Detection::Unknown;
        assert!(matches!(
            cd_reason(&inspect, &unknown, None, WrapperState::Absent, false),
            CdUnavailable::NotInstalled
        ));
        assert!(matches!(
            cd_reason(&inspect, &unknown, None, WrapperState::Drift, false),
            CdUnavailable::NeedsRefresh
        ));
        // "restart"-class advice is reachable ONLY from Working:
        assert!(matches!(
            cd_reason(&inspect, &unknown, None, WrapperState::Working, false),
            CdUnavailable::InstalledInactive
        ));
        assert!(matches!(
            cd_reason(&inspect, &unknown, None, WrapperState::Working, true),
            CdUnavailable::RanByPath
        ));
    }

    #[test]
    fn cd_reason_unsupported_from_tree_or_env_shell() {
        let report_only = StatusAction::ReportOnly;
        assert!(matches!(
            cd_reason(&report_only, &Detection::Unsupported("nu".into()), None, WrapperState::Absent, false),
            CdUnavailable::UnsupportedShell(n) if n == "nu"
        ));
        // Unknown tree but $SHELL names an unsupported shell → UnsupportedShell.
        assert!(matches!(
            cd_reason(&report_only, &Detection::Unknown, Some("/usr/bin/nu"), WrapperState::Absent, false),
            CdUnavailable::UnsupportedShell(n) if n == "nu"
        ));
        // Unknown tree, no usable $SHELL → NotInstalled.
        assert!(matches!(
            cd_reason(
                &report_only,
                &Detection::Unknown,
                None,
                WrapperState::Absent,
                false
            ),
            CdUnavailable::NotInstalled
        ));
    }

    // ---- decide_cd (pure exit-code/rescue policy) ----

    #[test]
    fn decide_cd_policies() {
        let directed = CdRequest::Directed;
        let unavail = CdRequest::Unavailable(CdUnavailable::NotInstalled);
        assert!(matches!(
            decide_cd(&directed, Policy::Switch),
            CdAction::Done
        ));
        assert!(matches!(
            decide_cd(&directed, Policy::Followup),
            CdAction::Done
        ));
        assert!(matches!(
            decide_cd(
                &directed,
                Policy::Close {
                    origin_exists: false
                }
            ),
            CdAction::Done
        ));
        assert!(matches!(
            decide_cd(&unavail, Policy::Switch),
            CdAction::WarnNonzero(_)
        ));
        assert!(matches!(
            decide_cd(&unavail, Policy::Followup),
            CdAction::Warn(_)
        ));
        assert!(matches!(
            decide_cd(
                &unavail,
                Policy::Close {
                    origin_exists: true
                }
            ),
            CdAction::Warn(_)
        ));
        assert!(matches!(
            decide_cd(
                &unavail,
                Policy::Close {
                    origin_exists: false
                }
            ),
            CdAction::Rescue(_)
        ));
    }

    // ---- rc_stanza_status comparator + formatting ----

    #[test]
    fn rc_stanza_status_present_drift_absent() {
        let tmp = TempDir::new().unwrap();
        let rc = tmp.path().join(".zshrc");
        assert_eq!(rc_stanza_status(&rc, "anything"), TriState::Absent);

        let stanza = render_rc_stanza("zsh").unwrap();
        std::fs::write(&rc, &stanza).unwrap();
        assert_eq!(rc_stanza_status(&rc, &stanza), TriState::Present);
        assert_eq!(
            rc_stanza_status(&rc, "completely different"),
            TriState::Drift
        );

        // `rc_stanza_state` still formats the same labels.
        assert!(rc_stanza_state(&rc, &stanza).contains("(present)"));
    }

    // ---- wrapper_state (temp env; uses the real Cli command, matching install) ----

    #[test]
    fn wrapper_state_canonical_zsh_install_is_working() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp);
        install(
            &env,
            "zsh",
            &mut crate::cli::Cli::command(),
            InstallOpts::default(),
        )
        .unwrap();
        assert_eq!(wrapper_state(&env, "zsh"), WrapperState::Working);
    }

    #[test]
    fn wrapper_state_absent_when_nothing_installed() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp);
        assert_eq!(wrapper_state(&env, "zsh"), WrapperState::Absent);
    }

    #[test]
    fn wrapper_state_drift_when_managed_file_corrupted() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp);
        install(
            &env,
            "zsh",
            &mut crate::cli::Cli::command(),
            InstallOpts::default(),
        )
        .unwrap();
        std::fs::write(env.managed_file("zsh").unwrap(), "garbage\n").unwrap();
        assert_eq!(wrapper_state(&env, "zsh"), WrapperState::Drift);
    }

    #[test]
    fn wrapper_state_fish_shadow_blocks_working() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp);
        install(
            &env,
            "fish",
            &mut crate::cli::Cli::command(),
            InstallOpts::default(),
        )
        .unwrap();
        assert_eq!(wrapper_state(&env, "fish"), WrapperState::Working);
        let shadow = fish_shadow_path(&env);
        std::fs::create_dir_all(shadow.parent().unwrap()).unwrap();
        std::fs::write(&shadow, "# shadow\n").unwrap();
        assert_eq!(wrapper_state(&env, "fish"), WrapperState::Drift);
    }

    #[test]
    fn wrapper_state_rc_stanza_drift_is_drift() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp);
        install(
            &env,
            "zsh",
            &mut crate::cli::Cli::command(),
            InstallOpts::default(),
        )
        .unwrap();
        // A valid marker block whose contents drifted from the canonical stanza.
        std::fs::write(
            env.home.join(".zshrc"),
            format!("{MARKER_BEGIN}\nsource /somewhere/else\n{MARKER_END}\n"),
        )
        .unwrap();
        assert_eq!(wrapper_state(&env, "zsh"), WrapperState::Drift);
    }

    #[test]
    fn wrapper_state_duplicate_marker_block_is_not_working() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp);
        install(
            &env,
            "zsh",
            &mut crate::cli::Cli::command(),
            InstallOpts::default(),
        )
        .unwrap();
        assert_eq!(wrapper_state(&env, "zsh"), WrapperState::Working);
        // A SECOND primary marker block ⇒ not an exact single canonical install (review fix).
        let rc = env.home.join(".zshrc");
        let mut contents = std::fs::read_to_string(&rc).unwrap();
        contents.push_str(&format!("\n{MARKER_BEGIN}\nsource /dup\n{MARKER_END}\n"));
        std::fs::write(&rc, contents).unwrap();
        assert_eq!(wrapper_state(&env, "zsh"), WrapperState::Drift);
    }

    #[test]
    fn wrapper_state_legacy_line_is_drift_not_working() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp);
        // A legacy `eval "$(ji config shell init zsh)"` line, no canonical install.
        std::fs::write(
            env.home.join(".zshrc"),
            "eval \"$(ji config shell init zsh)\"\n",
        )
        .unwrap();
        assert_eq!(wrapper_state(&env, "zsh"), WrapperState::Drift);
    }
}
