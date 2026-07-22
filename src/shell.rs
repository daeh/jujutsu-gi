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
        if [[ -n ${COMPLETE-} ]]; then
            command ji "$@"
            return
        fi

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
        if [[ -n ${COMPLETE-} ]]; then
            command ji "$@"
            return
        fi

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
    if set -q COMPLETE
        command ji $argv
        return $status
    end

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

fn wrapper_script(shell: &str) -> Result<&'static str> {
    match shell {
        "zsh" => Ok(ZSH_WRAPPER),
        "bash" => Ok(BASH_WRAPPER),
        "fish" => Ok(FISH_WRAPPER),
        other => bail!("unsupported shell: {other} (supported: {SUPPORTED_SHELLS})"),
    }
}

/// Raw clap_complete per-shell dynamic registration (no extra wrapping). The
/// single source for both the sourced init form (`completion_script`) and the
/// packaged form (`packaged_completion`). Deterministic, so install/status
/// drift detection still recomputes an equal body. Not the static
/// `clap_complete::generate` form — that cannot carry the dynamic
/// `ArgValueCompleter` candidates.
fn raw_registration(shell: &str) -> Result<String> {
    use clap_complete::env::{Bash, EnvCompleter, Fish, Zsh};
    let completer: &dyn EnvCompleter = match shell {
        "zsh" => &Zsh,
        "bash" => &Bash,
        "fish" => &Fish,
        other => bail!("unsupported shell: {other} (supported: {SUPPORTED_SHELLS})"),
    };
    let mut buf: Vec<u8> = Vec::new();
    completer
        .write_registration("COMPLETE", "ji", "ji", "ji", &mut buf)
        .context("generating dynamic completion registration")?;
    String::from_utf8(buf).context("clap_complete generated invalid UTF-8")
}

/// Dynamic completion registration for the *sourced* init body. When sourced it
/// registers a completer that invokes `ji` with the tokenized command line at
/// completion time.
fn completion_script(shell: &str) -> Result<String> {
    let script = raw_registration(shell)?;
    match shell {
        // zsh `compdef` only exists after `compinit`; guard so users without it
        // get the wrapper but silently skip completion instead of erroring.
        "zsh" => Ok(format!(
            "if (( ${{+functions[compdef]}} )); then\n{script}\nfi\n"
        )),
        // The fish wrapper has no outer `command -v ji` guard (unlike the zsh/
        // bash rc sourcing), so guard the fish registration itself.
        "fish" => Ok(format!("if command -q ji\n{script}\nend\n")),
        _ => Ok(script),
    }
}

/// Completion file contents for `cargo xtask gen-completions` → the packaged
/// (Homebrew / zsh `site-functions`) install, so packaged completions are
/// dynamic rather than stale static. bash/fish use clap's registration as-is
/// (Homebrew sources the bash file; fish autoloads the `complete` line, and
/// ji's `$COMPLETE` wrapper short-circuit prevents the fish-wrapper recursion
/// clap's bare-command call would otherwise cause). zsh is made autoload-safe
/// for an fpath `_ji`.
pub fn packaged_completion(shell: &str) -> Result<String> {
    let script = raw_registration(shell)?;
    match shell {
        "zsh" => Ok(make_zsh_autoload_safe(&script)),
        _ => Ok(script),
    }
}

/// Make clap's dynamic zsh registration safe to autoload from `fpath` as `_ji`.
/// clap's registration ends with `compdef <func> ji`, which assumes the script
/// is sourced/eval'd. Installed as `site-functions/_ji` and autoloaded by
/// compinit, the file body instead runs AS `_ji` on the first completion, where
/// a bare `compdef` is too late. Replace the trailing `compdef` with a
/// dual-mode guard: in autoload mode (`funcstack[1]` == `_ji`) call clap's
/// completer directly; otherwise (sourced) register it via `compdef`. If clap's
/// output format ever changes so the trailing line isn't found, the script is
/// returned unchanged (still functional in the sourced mode).
fn make_zsh_autoload_safe(script: &str) -> String {
    let func = "_clap_dynamic_completer_ji";
    let trailing = format!("compdef {func} ji");
    let replacement = format!(
        "if [ \"$funcstack[1]\" = \"_ji\" ]; then\n    {func} \"$@\"\nelse\n    compdef {func} ji\nfi"
    );
    script.replace(&trailing, &replacement)
}

/// What `ji config shell init <shell>` streams to stdout.
pub fn print_init(shell: &str) -> Result<()> {
    let body = render_managed_body(shell)?;
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
// wrapper is not active we can't cd, so instead of crashing we diagnose why and
// print a terse, reason-specific note. Diagnosis (process-tree walk + filesystem
// probes) runs only on the cd-unavailable path — never on the wrapped happy path.
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
            let primary = if shell == "zsh" {
                zsh_primary(env)
            } else {
                bash_primary(env)
            };
            let Ok(body) = render_managed_body(shell) else {
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
            // Working requires exactly the canonical install and nothing else: a single hit
            // that is the primary rc's marker block. Duplicate primary blocks, or any
            // cross-file/legacy/malformed/unresolved hit ⇒ not canonical.
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
             or run `ji` directly (not via `command ji`/a path) — diagnose with: ji config shell status"
                .into()
        }
        CdUnavailable::RanByPath => {
            "invoke `ji` as a bare command — the shell wrapper only wraps `ji` — diagnose with: ji config shell status".into()
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

/// Pure gate (unit-testable): only offer when the active shell is a supported
/// Inspect shell, the reason is genuinely not-installed, we're on a TTY, and the
/// active shell hasn't been declined.
fn should_offer(action: &StatusAction, reason: &CdUnavailable, tty: bool, declined: bool) -> bool {
    matches!(reason, CdUnavailable::NotInstalled)
        && tty
        && !declined
        && matches!(action, StatusAction::Inspect(_))
}

/// Best-effort inline install offer. Returns Ok(true) if it handled output (so the
/// caller can stay silent), Ok(false) to fall back to the plain cd note. Never errors
/// out of the cd path — all problems are printed inside.
fn try_offer_install(reason: &CdUnavailable) -> Result<bool> {
    use std::io::IsTerminal;
    if !matches!(reason, CdUnavailable::NotInstalled) {
        return Ok(false);
    }
    let report = detect_active_report();
    let action = status_action(&report);
    let active = match &action {
        StatusAction::Inspect(s) => s.clone(),
        StatusAction::ReportOnly => return Ok(false),
    };
    let Some(active_static) = SUPPORTED.iter().copied().find(|s| *s == active) else {
        return Ok(false);
    };
    let Ok(env) = ShellEnv::from_process_env() else {
        return Ok(false);
    };
    let tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let declined = install_prompt_declined(&env, &active);
    if !should_offer(&action, reason, tty, declined) {
        return Ok(false);
    }

    let probe = probe_shells(&env);
    let mut attempt: Vec<&str> = vec![active_static];
    for &s in &probe.present {
        if s != active_static {
            attempt.push(s);
        }
    }
    // One bucket per shell: only absent shells are "skipped"; errored ones go to
    // failures (via probe_errors). Both exclude attempted shells (active ∪ present).
    let skipped: Vec<&str> = probe
        .absent
        .iter()
        .copied()
        .filter(|s| !attempt.contains(s))
        .collect();
    let probe_errors: Vec<(&str, std::io::Error)> = probe
        .errored
        .into_iter()
        .filter(|(s, _)| !attempt.contains(s))
        .collect();

    let outcome = multi_install(
        &env,
        &attempt,
        &skipped,
        probe_errors,
        InstallOpts {
            dry_run: false,
            force: false,
            interactive: true,
        },
        "(ji)::shell install integration now? [y/N/?] ",
    )?;
    render_multi_outcome(&env, &outcome);

    if outcome.cancelled {
        set_install_prompt_declined(&env, &active);
        return Ok(true);
    }
    // Clear the per-shell decline marker for every shell verified installed by
    // this offer (it configures active ∪ present), not just the active one.
    for s in outcome.applied.iter().chain(outcome.unchanged.iter()) {
        clear_install_prompt_declined(&env, s);
    }
    if active_configured(&outcome, &active) {
        let active_shadow = outcome
            .shadows
            .iter()
            .find(|(s, h)| s == &active && h.shadows_wrapper());
        if let Some((_, h)) = active_shadow {
            eprintln!(
                "(ji)::shell installed, but `ji` is shadowed by {}:{} — remove it, then restart your shell",
                h.path.display(),
                h.line_no
            );
        } else {
            eprintln!(
                "(ji)::shell installed — restart your shell (or open a new one) to activate auto-cd"
            );
        }
    }
    Ok(true)
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

/// Pure decision core: request + policy → outcome (no I/O). Unit-tested.
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
            let _ = try_offer_install(&r);
            Ok(())
        }
        CdAction::WarnNonzero(r) => {
            warn_cd_unavailable(&r, safety);
            let _ = try_offer_install(&r);
            Err(CdNotApplied.into())
        }
        // Rescue never uses the generic `warn_cd_unavailable`: it leads with the escape,
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
/// only when the cd didn't happen — no filesystem cost on the wrapped happy path.
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
            // Rescue guarantee: if the cwd is gone and the directive write failed, still
            // lead with the loud escape — never a bare anyhow dump.
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

/// Source of the fish `$fish_complete_path` probe. `Live` spawns fish (production);
/// `Probed`/`Failed` are injected by tests so detection and the probe-failure
/// path are deterministic without a real fish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FishProbeSource {
    Live,
    Probed(Vec<PathBuf>),
    Failed,
}

#[derive(Debug, Clone)]
pub struct ShellEnv {
    pub home: PathBuf,
    pub xdg_config_home: PathBuf,
    pub zdotdir: PathBuf,
    pub zsh_custom: Option<PathBuf>,
    pub omz_root: Option<PathBuf>,
    /// How `fish_completion_state` obtains fish's `$fish_complete_path`.
    /// `Live` in production; tests inject `Probed`/`Failed`.
    pub fish_complete_path: FishProbeSource,
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
            fish_complete_path: FishProbeSource::Live,
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

fn declined_marker(env: &ShellEnv, shell: &str) -> PathBuf {
    env.xdg_config_home
        .join("ji")
        .join(format!("install-prompt-declined-{shell}"))
}

fn install_prompt_declined(env: &ShellEnv, shell: &str) -> bool {
    declined_marker(env, shell).exists()
}

fn set_install_prompt_declined(env: &ShellEnv, shell: &str) {
    let p = declined_marker(env, shell);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&p, b"");
}

fn clear_install_prompt_declined(env: &ShellEnv, shell: &str) {
    let _ = std::fs::remove_file(declined_marker(env, shell));
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

fn detect_bypass_alias(line: &str) -> Option<(String, String)> {
    let line = strip_inline_comment(line.trim_start());
    let rest = line.strip_prefix("alias")?;
    if !rest.chars().next().is_some_and(|c| c.is_whitespace()) {
        return None;
    }
    let mut rest = rest.trim_start();
    loop {
        let tok = rest.split_whitespace().next().unwrap_or("");
        if matches!(tok, "-g" | "-s" | "-r" | "--") {
            rest = rest[tok.len()..].trim_start();
        } else {
            break;
        }
    }

    let name_end = rest.find(|c: char| c == '=' || c.is_whitespace())?;
    let name = rest[..name_end].to_string();
    if name.is_empty() {
        return None;
    }
    let after = &rest[name_end..];
    let raw_value = match after.strip_prefix('=') {
        Some(v) => v,
        None => after.trim_start(),
    };

    let vt = raw_value.trim_start();
    if vt.starts_with("$(") || vt.starts_with('`') {
        return None;
    }

    let unq = unquote_one(raw_value);
    let mut words = unq.split_whitespace();
    let mut first = words.next()?;
    if first == "eval" {
        return None;
    }

    let mut forced = false;
    if matches!(first, "command" | "env" | "exec" | "nohup") {
        forced = true;
        // Skip any `VAR=val` env-assignments that follow the wrapper (e.g.
        // `env FOO=1 /opt/bin/ji`); a following `-flag` (e.g. `command -v`) is a
        // lookup, not a clean bypass, so bail false-positive-free.
        first = loop {
            match words.next() {
                None => return None,
                Some(w) if w.starts_with('-') => return None,
                Some(w) if !is_var_assignment(w) => break w,
                Some(_) => {} // skip a VAR=val assignment, keep looking
            }
        };
    }

    if first.contains('/') || first.contains('\\') {
        let base = first
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(first)
            .to_ascii_lowercase();
        if base == "ji" || base == "ji.exe" {
            return Some((name, unq.to_string()));
        }
    } else if forced && first.eq_ignore_ascii_case("ji") {
        return Some((name, unq.to_string()));
    }
    None
}

fn unquote_one(s: &str) -> &str {
    let t = s.trim();
    for q in ['\'', '"'] {
        if t.len() >= 2 && t.starts_with(q) && t.ends_with(q) {
            return &t[1..t.len() - 1];
        }
    }
    t
}

/// Is `tok` a shell `VAR=value` assignment (a valid identifier before `=`)?
/// Used to skip `env FOO=1 …` style prefixes when classifying an alias value.
fn is_var_assignment(tok: &str) -> bool {
    match tok.find('=') {
        Some(eq) if eq > 0 => {
            let name = &tok[..eq];
            name.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        _ => false,
    }
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
    } else {
        let r = s
            .strip_prefix("source ")
            .or_else(|| s.strip_prefix("source\t"))?;
        r.trim_start()
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShadowKind {
    Alias,
    Function,
}

#[derive(Debug, Clone)]
struct AliasHit {
    path: PathBuf,
    line_no: usize,
    name: String,
    target: String,
    kind: ShadowKind,
}

impl AliasHit {
    /// True iff this shadows the `ji` wrapper itself (vs a differently-named
    /// alias that merely runs the binary directly).
    fn shadows_wrapper(&self) -> bool {
        matches!(self.kind, ShadowKind::Function) || self.name == "ji"
    }
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

fn scan_bypass_aliases(env: &ShellEnv, shell: &str, primary: &Path) -> Vec<AliasHit> {
    let skip_path = if shell == "fish" {
        Some(fish_primary(env))
    } else {
        env.managed_file(shell).ok()
    };
    let mut out = Vec::new();
    for loc in scan_locations(env, shell, primary) {
        if skip_path.as_ref().is_some_and(|p| p == &loc.path) {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&loc.path) else {
            continue;
        };
        for (i, line) in contents.lines().enumerate() {
            let line_no = i + 1;
            if let Some((name, target)) = detect_bypass_alias(line) {
                out.push(AliasHit {
                    path: loc.path.clone(),
                    line_no,
                    name,
                    target,
                    kind: ShadowKind::Alias,
                });
            }
            if shell == "fish" {
                let line = strip_inline_comment(line.trim_start());
                if line == "function ji"
                    || line.starts_with("function ji ")
                    || line.starts_with("function ji;")
                {
                    out.push(AliasHit {
                        path: loc.path.clone(),
                        line_no,
                        name: "ji".into(),
                        target: String::new(),
                        kind: ShadowKind::Function,
                    });
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

fn render_managed_body(shell: &str) -> Result<String> {
    let wrapper = wrapper_script(shell)?;
    let completion = completion_script(shell)?;
    Ok(format!("{MANAGED_HEADER}\n{wrapper}\n{completion}"))
}

fn render_fish_wrapper_body() -> String {
    format!("{MANAGED_HEADER}\n{FISH_WRAPPER}")
}

fn render_fish_completion_body() -> Result<String> {
    let completion = completion_script("fish")?;
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
// fish vendor-completion detection ($fish_complete_path)
//
// fish autoloads `ji.fish` from the first dir in `$fish_complete_path` that
// has it. `ji config shell install fish` writes a completion into the user
// dir (position 1), which shadows Homebrew's dynamic vendor completion. When
// a reachable dynamic vendor completion exists, install removes the redundant
// user file instead of writing another; otherwise it writes the user file.
// =====================================================================

/// Whether `contents` is ji's *dynamic* clap_complete registration — as opposed
/// to the pre-0.1.4 static `generate` output or an unrelated file. Requires the
/// exact callback signature ji emits via `write_registration("COMPLETE", …)`
/// (see [`raw_registration`]): the `COMPLETE=fish ji` env-completer invocation
/// and the `ji` command name. The static output contains neither.
fn is_dynamic_ji_registration(contents: &str) -> bool {
    contents.contains("COMPLETE=fish ji")
        && (contents.contains("--command ji") || contents.contains("-c ji"))
}

/// One `$fish_complete_path` dir that contains a `ji.fish`. An existing but
/// unreadable file is recorded with `is_dynamic == false` (a blocker), never
/// skipped, so first-match order is honored.
struct ProviderFact {
    index: usize,
    is_user: bool,
    is_dynamic: bool,
    path: PathBuf,
}

/// Order-aware conclusion of the fish completion scan (see [`fish_decide`]).
struct FishCompletionScan {
    /// Physical existence of `~/.config/fish/completions/ji.fish`, independent
    /// of whether the user dir is on `$fish_complete_path`.
    user_file_exists: bool,
    /// The first non-user `ji.fish` in path order, iff it is a dynamic ji
    /// registration (a reachable vendor completion).
    vendor_provider: Option<PathBuf>,
    /// The first non-user `ji.fish` in path order, iff it is not dynamic
    /// (static / foreign / unreadable) — it shadows anything later.
    blocking_non_user: Option<PathBuf>,
    /// The user dir has a `ji.fish` before a reachable dynamic vendor provider.
    user_shadows_vendor: bool,
    /// A blocking non-user file precedes the user dir (exotic: user dir not
    /// first), so a written user file would not take effect.
    blocking_precedes_user: bool,
}

struct FishCompletionState {
    scan: FishCompletionScan,
    probe_failed: bool,
}

/// Canonicalize-tolerant directory identity: canonicalize a path that exists,
/// else compare its lexically-normalized absolute form (a fresh cargo install
/// may not have created `~/.config/fish/completions` yet — a failed
/// `canonicalize` must not drop the user dir's position).
fn norm_dir(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| {
        use std::path::Component;
        let mut out = PathBuf::new();
        for c in p.components() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                other => out.push(other.as_os_str()),
            }
        }
        out
    })
}

fn same_dir(a: &Path, b: &Path) -> bool {
    norm_dir(a) == norm_dir(b)
}

/// Walk the ordered `$fish_complete_path`, recording each dir with a `ji.fish`
/// (all filesystem/content reads live here). Returns the facts in path order
/// and the user dir's index (present regardless of whether it holds a
/// `ji.fish`; `None` if the user dir isn't on the path).
fn fish_collect_providers(
    complete_path: &[PathBuf],
    user_dir: &Path,
) -> (Vec<ProviderFact>, Option<usize>) {
    let mut facts = Vec::new();
    let mut user_dir_index = None;
    for (index, dir) in complete_path.iter().enumerate() {
        let is_user = same_dir(dir, user_dir);
        if is_user && user_dir_index.is_none() {
            user_dir_index = Some(index);
        }
        let ji = dir.join("ji.fish");
        if !ji.is_file() {
            continue;
        }
        // Unreadable → not dynamic (a blocker), never skipped.
        let is_dynamic = std::fs::read_to_string(&ji).is_ok_and(|c| is_dynamic_ji_registration(&c));
        facts.push(ProviderFact {
            index,
            is_user,
            is_dynamic,
            path: ji,
        });
    }
    (facts, user_dir_index)
}

/// Pure decision over collected facts (no filesystem/fish). The first non-user
/// `ji.fish` is decisive under fish's first-available-file rule: an earlier
/// static/foreign/unreadable file shadows any later dynamic vendor, so we never
/// skip it to promote a later dynamic file.
fn fish_decide(
    facts: &[ProviderFact],
    user_dir_index: Option<usize>,
    user_file_exists: bool,
) -> FishCompletionScan {
    let fnu = facts.iter().find(|f| !f.is_user);
    let (vendor_provider, blocking_non_user) = match fnu {
        Some(f) if f.is_dynamic => (Some(f.path.clone()), None),
        Some(f) => (None, Some(f.path.clone())),
        None => (None, None),
    };
    let user_provider_index = facts.iter().find(|f| f.is_user).map(|f| f.index);
    let user_shadows_vendor = vendor_provider.is_some()
        && matches!((user_provider_index, fnu), (Some(ui), Some(f)) if ui < f.index);
    let blocking_precedes_user = matches!(
        (fnu, user_dir_index),
        (Some(f), Some(ui)) if !f.is_dynamic && f.index < ui
    );
    FishCompletionScan {
        user_file_exists,
        vendor_provider,
        blocking_non_user,
        user_shadows_vendor,
        blocking_precedes_user,
    }
}

/// Monotonic per-call counter for the probe nonce (std-only; no `rand` dep).
fn fish_probe_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Query fish for its ordered `$fish_complete_path`. `None` on any failure
/// (fish missing / spawn error / nonzero exit / timeout / missing success
/// sentinel); `Some(dirs)` — possibly empty — on success. A per-call nonce on
/// the success sentinel makes an empty-but-successful result distinguishable
/// from a failed or noisy `config.fish`. Bounded timeout with child-kill and a
/// stdout-drain reader thread, mirroring `run_jj_bootstrap_timeout`
/// (`src/jujutsu.rs`). `fish --no-config` yields an empty `$fish_complete_path`,
/// so config must run.
fn fish_complete_path_live() -> Option<Vec<PathBuf>> {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let nonce = format!("{}-{}", std::process::id(), fish_probe_seq());
    let script = format!(
        "printf 'JI_FCP_OK:{nonce}\\n'; for d in $fish_complete_path; printf 'JI_FCP\\t%s\\n' $d; end"
    );
    let timeout = Duration::from_secs(5);
    let start = Instant::now();
    let mut child = Command::new("fish")
        .arg("-c")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let deadline = start + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    drop(reader);
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(reader);
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let bytes = rx.recv_timeout(Duration::from_secs(1)).ok()?;
    let _ = reader.join();
    let out = String::from_utf8(bytes).ok()?;

    let sentinel = format!("JI_FCP_OK:{nonce}");
    if !out.lines().any(|l| l == sentinel) {
        return None; // no success sentinel → failure or noise, not an empty path
    }
    Some(
        out.lines()
            .filter_map(|l| l.strip_prefix("JI_FCP\t"))
            .map(PathBuf::from)
            .collect(),
    )
}

/// The full fish completion picture for install/status: resolve the probe
/// source (injected in tests; live fish otherwise), collect providers, decide.
fn fish_completion_state(env: &ShellEnv) -> FishCompletionState {
    let (dirs, probe_failed): (Vec<PathBuf>, bool) = match &env.fish_complete_path {
        FishProbeSource::Probed(dirs) => (dirs.clone(), false),
        FishProbeSource::Failed => (Vec::new(), true),
        FishProbeSource::Live => match fish_complete_path_live() {
            Some(dirs) => (dirs, false),
            None => (Vec::new(), true),
        },
    };
    let completions = fish_completions_path(env);
    let user_dir = completions
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let (facts, user_dir_index) = fish_collect_providers(&dirs, &user_dir);
    let scan = fish_decide(&facts, user_dir_index, completions.exists());
    FishCompletionState { scan, probe_failed }
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteRole {
    ManagedFile,
    PrimaryRc,
    FishWrapper,
    FishCompletions,
}

struct PlannedWrite {
    path: PathBuf,
    before: String,
    after: String,
    role: WriteRole,
}

/// A file the plan will delete (fish only): a redundant/stale user-dir
/// completion healed because a dynamic vendor completion provides one.
struct PlannedRemoval {
    path: PathBuf,
    existed: bool,
}

/// The fish completion delivery the plan settled on, recorded so post-apply
/// health (`installed_ok`) is deterministic without re-probing fish.
#[derive(Clone, Debug, PartialEq, Eq)]
enum FishCompletionMode {
    /// No reachable dynamic vendor — ji wrote the user-dir completion.
    UserFile,
    /// A dynamic vendor provides completions — ji relies on it (and removed a
    /// redundant/stale user-dir file if present).
    Vendor { provider: PathBuf },
}

struct ShellInstallPlan {
    shell: String,
    writes: Vec<PlannedWrite>,
    removals: Vec<PlannedRemoval>,
    shadows: Vec<AliasHit>,
    /// `Some` for fish, `None` for zsh/bash.
    fish_completion_mode: Option<FishCompletionMode>,
}

impl ShellInstallPlan {
    fn changed(&self) -> bool {
        self.writes.iter().any(|w| w.before != w.after) || self.removals.iter().any(|r| r.existed)
    }

    fn show(&self, w: &mut dyn std::io::Write) {
        for p in &self.writes {
            print_dry_run_diff(w, &p.path, &p.before, &p.after);
        }
        for r in &self.removals {
            if r.existed {
                let _ = writeln!(w, "- would delete {}", r.path.display());
            }
        }
    }
}

pub fn install(env: &ShellEnv, shell: &str, opts: InstallOpts) -> Result<()> {
    let plan = plan_install(env, shell, opts.force, &mut std::io::stderr())?;
    if opts.dry_run {
        plan.show(&mut std::io::stdout());
        return Ok(());
    }
    if opts.interactive && plan.changed() {
        let label = "(ji)::shell apply these changes? [y/N/?] ";
        let confirmed = confirm_install(label, || plan.show(&mut std::io::stdout()))?;
        if !confirmed {
            eprintln!("(ji)::shell install cancelled");
            return Ok(());
        }
    }
    apply_plan(&plan)?;
    if installed_ok(env, &plan) {
        clear_install_prompt_declined(env, shell);
    }
    Ok(())
}

struct ShellProbe {
    present: Vec<&'static str>,
    absent: Vec<&'static str>,
    errored: Vec<(&'static str, std::io::Error)>,
}

fn probe_shells(env: &ShellEnv) -> ShellProbe {
    let mut probe = ShellProbe {
        present: Vec::new(),
        absent: Vec::new(),
        errored: Vec::new(),
    };

    for &shell in SUPPORTED {
        let candidates = match shell {
            "zsh" => vec![zsh_primary(env)],
            "bash" => vec![
                env.home.join(".bash_profile"),
                env.home.join(".bash_login"),
                env.home.join(".profile"),
            ],
            "fish" => vec![env.xdg_config_home.join("fish")],
            _ => Vec::new(),
        };

        let mut first_error = None;
        let mut found = false;
        for path in candidates {
            match path.try_exists() {
                // fish "present" means its config DIRECTORY exists, not a stray
                // file at that path (which would otherwise be attempted and fail).
                Ok(true) if shell != "fish" || path.is_dir() => {
                    found = true;
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
            }
        }

        if found {
            probe.present.push(shell);
        } else if let Some(e) = first_error {
            probe.errored.push((shell, e));
        } else {
            probe.absent.push(shell);
        }
    }

    probe
}

/// Post-apply "files are canonical" health, derived deterministically from the
/// plan (no fish re-probe — a transient probe timeout must not misclassify a
/// healthy vendor install as failed).
fn installed_ok(env: &ShellEnv, plan: &ShellInstallPlan) -> bool {
    match plan.shell.as_str() {
        "zsh" | "bash" => {
            let shell = plan.shell.as_str();
            let primary = if shell == "zsh" {
                zsh_primary(env)
            } else {
                bash_primary(env)
            };
            let (Ok(managed), Ok(body), Ok(stanza)) = (
                env.managed_file(shell),
                render_managed_body(shell),
                render_rc_stanza(shell),
            ) else {
                return false;
            };
            state_for(&managed, &body) == "present"
                && rc_stanza_status(&primary, &stanza) == TriState::Present
        }
        "fish" => {
            let wrapper_body = render_fish_wrapper_body();
            let wrapper_ok = state_for(&fish_primary(env), &wrapper_body) == "present";
            let completions_ok = match &plan.fish_completion_mode {
                Some(FishCompletionMode::UserFile) => {
                    let Ok(body) = render_fish_completion_body() else {
                        return false;
                    };
                    state_for(&fish_completions_path(env), &body) == "present"
                }
                // Vendor mode: the provider must still be a dynamic ji completion,
                // and every planned removal must have landed. `plan_install_fish`
                // guarantees no *shadowing* user file survives unplanned, so this
                // is exactly "no shadowing user file remains"; an intentionally
                // inert non-shadowing file may remain.
                Some(FishCompletionMode::Vendor { provider }) => {
                    std::fs::read_to_string(provider).is_ok_and(|c| is_dynamic_ji_registration(&c))
                        && plan.removals.iter().all(|r| !r.path.exists())
                }
                None => false,
            };
            wrapper_ok && completions_ok
        }
        _ => false,
    }
}

struct MultiOutcome {
    applied: Vec<String>,
    unchanged: Vec<String>,
    skipped: Vec<String>,
    failures: Vec<(String, anyhow::Error)>,
    cancelled: bool,
    dry_run: bool,
    shadows: Vec<(String, AliasHit)>,
}

fn skipped_strings(skipped: &[&str]) -> Vec<String> {
    skipped.iter().map(|s| s.to_string()).collect()
}

fn multi_install(
    env: &ShellEnv,
    attempt: &[&str],
    skipped: &[&str],
    probe_errors: Vec<(&str, std::io::Error)>,
    opts: InstallOpts,
    prompt: &str,
) -> Result<MultiOutcome> {
    let mut failures = Vec::new();
    for (s, e) in probe_errors {
        let err = anyhow::anyhow!("could not probe {s} shell config: {e}");
        eprintln!("(ji)::shell {s}: {err}");
        failures.push((s.to_string(), err));
    }

    let mut plans = Vec::new();
    for shell in attempt {
        match plan_install(env, shell, opts.force, &mut std::io::stderr()) {
            Ok(plan) => plans.push(plan),
            Err(e) => {
                eprintln!("(ji)::shell {shell}: {e}");
                failures.push((shell.to_string(), e));
            }
        }
    }

    let any_changed = plans.iter().any(|p| p.changed());

    if opts.dry_run {
        for p in &plans {
            p.show(&mut std::io::stdout());
        }
        return Ok(MultiOutcome {
            applied: Vec::new(),
            unchanged: Vec::new(),
            skipped: skipped_strings(skipped),
            failures,
            cancelled: false,
            dry_run: true,
            shadows: Vec::new(),
        });
    }

    if opts.interactive && any_changed {
        let confirmed = confirm_install(prompt, || {
            let mut e = std::io::stderr();
            for p in &plans {
                p.show(&mut e);
            }
        })?;
        if !confirmed {
            return Ok(MultiOutcome {
                applied: Vec::new(),
                unchanged: Vec::new(),
                skipped: skipped_strings(skipped),
                failures,
                cancelled: true,
                dry_run: false,
                shadows: Vec::new(),
            });
        }
    }

    let mut outcome = MultiOutcome {
        applied: Vec::new(),
        unchanged: Vec::new(),
        skipped: skipped_strings(skipped),
        failures,
        cancelled: false,
        dry_run: false,
        shadows: Vec::new(),
    };

    for plan in &plans {
        match apply_plan(plan) {
            Ok(()) if installed_ok(env, plan) => {
                if plan.changed() {
                    outcome.applied.push(plan.shell.clone());
                } else {
                    outcome.unchanged.push(plan.shell.clone());
                }
                outcome.shadows.extend(
                    plan.shadows
                        .iter()
                        .cloned()
                        .map(|hit| (plan.shell.clone(), hit)),
                );
                debug_assert!(active_configured(&outcome, &plan.shell));
            }
            Ok(()) => {
                let err = anyhow::anyhow!("managed files not canonical after apply");
                eprintln!("(ji)::shell {}: {err}", plan.shell);
                outcome.failures.push((plan.shell.clone(), err));
            }
            Err(e) => {
                eprintln!("(ji)::shell {}: {e}", plan.shell);
                outcome.failures.push((plan.shell.clone(), e));
            }
        }
    }

    Ok(outcome)
}

fn integration_present(o: &MultiOutcome) -> bool {
    !o.applied.is_empty() || !o.unchanged.is_empty()
}

fn active_configured(o: &MultiOutcome, active: &str) -> bool {
    o.applied.iter().any(|s| s == active) || o.unchanged.iter().any(|s| s == active)
}

fn render_multi_outcome(env: &ShellEnv, o: &MultiOutcome) {
    if o.dry_run {
        return;
    }
    let _ = o.shadows.len();

    for shell in &o.skipped {
        match shell.as_str() {
            "bash" => eprintln!(
                "(ji)::shell skipped bash; no {0}/.bash_profile, {0}/.bash_login, or {0}/.profile",
                env.home.display()
            ),
            "zsh" => eprintln!(
                "(ji)::shell skipped zsh; no {}/.zshrc",
                env.zdotdir.display()
            ),
            "fish" => eprintln!(
                "(ji)::shell skipped fish; {}/fish not found",
                env.xdg_config_home.display()
            ),
            _ => {}
        }
    }

    if o.cancelled {
        eprintln!("(ji)::shell install cancelled");
    } else if !o.applied.is_empty() {
        eprintln!("(ji)::shell configured {} shell(s)", o.applied.len());
    } else if integration_present(o) {
        eprintln!("(ji)::shell all configured shells already up-to-date");
    } else if o.failures.is_empty() {
        eprintln!("(ji)::shell no shells with an existing config found");
    }
}

pub fn install_all(env: &ShellEnv, opts: InstallOpts) -> Result<()> {
    let probe = probe_shells(env);
    let outcome = multi_install(
        env,
        &probe.present,
        &probe.absent,
        probe.errored,
        opts,
        "(ji)::shell apply these changes? [y/N/?] ",
    )?;
    render_multi_outcome(env, &outcome);
    for s in outcome.applied.iter().chain(outcome.unchanged.iter()) {
        clear_install_prompt_declined(env, s);
    }
    if !outcome.failures.is_empty() {
        bail!(
            "(ji)::shell {} shell(s) failed to configure",
            outcome.failures.len()
        );
    }
    Ok(())
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
fn confirm_install(prompt: &str, preview: impl Fn()) -> Result<bool> {
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
            write!(e, "{prompt}")?;
            e.flush()
        },
        preview,
    )
    .map_err(Into::into)
}

fn plan_install(
    env: &ShellEnv,
    shell: &str,
    force: bool,
    notes: &mut dyn std::io::Write,
) -> Result<ShellInstallPlan> {
    match shell {
        "zsh" | "bash" => plan_install_posix(env, shell, force, notes),
        "fish" => plan_install_fish(env, force, notes),
        other => bail!("unsupported shell: {other} (supported: {SUPPORTED_SHELLS})"),
    }
}

fn emit_shadow_notes(notes: &mut dyn std::io::Write, shadows: &[AliasHit]) {
    for h in shadows {
        if h.shadows_wrapper() {
            let what = if matches!(h.kind, ShadowKind::Function) {
                "function ji".into()
            } else {
                format!("alias {}", h.name)
            };
            let _ = writeln!(
                notes,
                "(ji)::shell: note: `ji` shadowed by {} in {}:{} — remove it for auto-cd to work",
                what,
                h.path.display(),
                h.line_no
            );
        } else {
            let _ = writeln!(
                notes,
                "(ji)::shell: note: alias {} in {}:{} runs ji directly (bypassing auto-cd; use `ji`)",
                h.name,
                h.path.display(),
                h.line_no
            );
        }
    }
}

fn plan_install_posix(
    env: &ShellEnv,
    shell: &str,
    force: bool,
    notes: &mut dyn std::io::Write,
) -> Result<ShellInstallPlan> {
    let primary = if shell == "zsh" {
        zsh_primary(env)
    } else {
        bash_primary(env)
    };

    if primary.exists() {
        let canonical = primary
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", primary.display()))?;
        if chezmoi_strong_signal(&canonical) && !force {
            bail!(
                "(ji)::shell: {} resolves into a chezmoi source directory ({}); edits will be reverted by `chezmoi apply`. Pass --force to install anyway.",
                primary.display(),
                canonical.display(),
            );
        }
        if dotfile_manager_soft_signal(env, &canonical) {
            let _ = writeln!(
                notes,
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
    let shadows = scan_bypass_aliases(env, shell, &primary);

    let malformed: Vec<&Hit> = hits
        .iter()
        .filter(|h| matches!(h.kind, HitKind::MalformedMarker))
        .collect();
    if !malformed.is_empty() && !force {
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
            let _ = writeln!(
                notes,
                "(ji)::shell: note: unresolved source-like line in {}:{} (`{}`)",
                u.path.display(),
                u.line_no,
                raw,
            );
        }
    }
    emit_shadow_notes(notes, &shadows);

    if !nonprimary_hits.is_empty() && !force {
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
        && !force
    {
        bail!(
            "(ji)::shell: existing integration line found in {}:{} — remove it manually, or pass --force to install alongside it.",
            h.path.display(),
            h.line_no,
        );
    }

    let managed_path = env.managed_file(shell)?;
    let managed_body = render_managed_body(shell)?;
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

    Ok(ShellInstallPlan {
        shell: shell.to_string(),
        writes: vec![
            PlannedWrite {
                path: managed_path.clone(),
                before: existing_or_empty(&managed_path),
                after: managed_body,
                role: WriteRole::ManagedFile,
            },
            PlannedWrite {
                path: primary,
                before: primary_contents,
                after: new_primary_contents,
                role: WriteRole::PrimaryRc,
            },
        ],
        removals: Vec::new(),
        shadows,
        fish_completion_mode: None,
    })
}

fn existing_or_empty(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn print_dry_run_diff(w: &mut dyn std::io::Write, path: &Path, before: &str, after: &str) {
    if before == after {
        return;
    }
    let _ = writeln!(w, "--- {}", path.display());
    let _ = writeln!(w, "+++ {} (after install)", path.display());
    for line in before.lines() {
        let _ = writeln!(w, "- {line}");
    }
    for line in after.lines() {
        let _ = writeln!(w, "+ {line}");
    }
    let _ = writeln!(w);
}

fn plan_install_fish(
    env: &ShellEnv,
    force: bool,
    notes: &mut dyn std::io::Write,
) -> Result<ShellInstallPlan> {
    let primary = fish_primary(env);
    let completions = fish_completions_path(env);
    let shadow = fish_shadow_path(env);

    if shadow.exists() && !force {
        bail!(
            "(ji)::shell: {} exists and would shadow our `functions/ji.fish` (fish auto-sources conf.d at startup, blocking the autoload) — remove it manually, or pass --force.",
            shadow.display(),
        );
    }

    let locations = scan_locations(env, "fish", &primary);
    let hits = find_hits(&locations, env)?;
    let shadows = scan_bypass_aliases(env, "fish", &primary);

    let malformed: Vec<&Hit> = hits
        .iter()
        .filter(|h| matches!(h.kind, HitKind::MalformedMarker))
        .collect();
    if !malformed.is_empty() && !force {
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

    if !nonprimary_hits.is_empty() && !force {
        let first = nonprimary_hits[0];
        bail!(
            "(ji)::shell: existing integration found in {}:{} — remove it manually, or pass --force to install anyway.",
            first.path.display(),
            first.line_no,
        );
    }
    emit_shadow_notes(notes, &shadows);

    // The wrapper is always installed; a non-ji-managed wrapper still hard-bails.
    if primary.exists() {
        let existing = std::fs::read_to_string(&primary)
            .with_context(|| format!("failed to read {}", primary.display()))?;
        if !is_managed(&existing) && !force {
            bail!(
                "(ji)::shell: {} exists and is not ji-managed — remove it manually, or pass --force to overwrite (wrapper).",
                primary.display(),
            );
        }
    }

    let mut writes = vec![PlannedWrite {
        path: primary.clone(),
        before: existing_or_empty(&primary),
        after: render_fish_wrapper_body(),
        role: WriteRole::FishWrapper,
    }];
    let mut removals = Vec::new();

    // Completion delivery. When a reachable *dynamic* vendor completion exists
    // (e.g. Homebrew's `vendor_completions.d/ji.fish`), rely on it and remove a
    // redundant/stale user-dir completion rather than writing another that would
    // shadow it (fish autoloads the first `ji.fish` in `$fish_complete_path`).
    // Otherwise (cargo/manual, or probe failure) write the user-dir completion.
    let state = fish_completion_state(env);
    let scan = &state.scan;
    let user_managed = scan.user_file_exists
        && std::fs::read_to_string(&completions)
            .map(|c| is_managed(&c))
            .unwrap_or(false);

    let fish_completion_mode = if let Some(provider) = scan.vendor_provider.clone() {
        if scan.user_file_exists {
            if user_managed || force {
                removals.push(PlannedRemoval {
                    path: completions.clone(),
                    existed: true,
                });
                let _ = writeln!(
                    notes,
                    "(ji)::shell: fish completions are provided by {}; removing the redundant/stale user-dir completion {}",
                    provider.display(),
                    completions.display(),
                );
            } else if scan.user_shadows_vendor {
                bail!(
                    "(ji)::shell: {} shadows the vendor completion {} and is not ji-managed — remove it manually, or pass --force.",
                    completions.display(),
                    provider.display(),
                );
            } else {
                let _ = writeln!(
                    notes,
                    "(ji)::shell: fish completions are provided by {} ({} is present but not in effect).",
                    provider.display(),
                    completions.display(),
                );
            }
        } else {
            let _ = writeln!(
                notes,
                "(ji)::shell: fish completions are provided by {}.",
                provider.display(),
            );
        }
        FishCompletionMode::Vendor { provider }
    } else {
        if scan.user_file_exists && !user_managed && !force {
            bail!(
                "(ji)::shell: {} exists and is not ji-managed — remove it manually, or pass --force to overwrite (completions).",
                completions.display(),
            );
        }
        // Exotic: a non-ji `ji.fish` earlier in the path would shadow the file we
        // write, so warn (still write it — harmless).
        if scan.blocking_precedes_user
            && let Some(blocker) = &scan.blocking_non_user
        {
            let _ = writeln!(
                notes,
                "(ji)::shell: note: {} is earlier in $fish_complete_path and is not ji's dynamic completion — the user completion won't take effect until it's removed manually.",
                blocker.display(),
            );
        }
        writes.push(PlannedWrite {
            path: completions.clone(),
            before: existing_or_empty(&completions),
            after: render_fish_completion_body()?,
            role: WriteRole::FishCompletions,
        });
        FishCompletionMode::UserFile
    };

    Ok(ShellInstallPlan {
        shell: "fish".to_string(),
        writes,
        removals,
        shadows,
        fish_completion_mode: Some(fish_completion_mode),
    })
}

fn apply_plan(plan: &ShellInstallPlan) -> Result<()> {
    let _shell = plan.shell.as_str();
    for w in &plan.writes {
        let outcome = write_atomic(&w.path, &w.after)?;
        match (w.role, outcome) {
            (WriteRole::ManagedFile, WriteOutcome::Created) => {
                eprintln!("(ji)::shell installed managed file {}", w.path.display());
            }
            (WriteRole::ManagedFile, WriteOutcome::Updated) => {
                eprintln!("(ji)::shell updated managed file {}", w.path.display());
            }
            (WriteRole::ManagedFile, WriteOutcome::Unchanged) => {}
            (WriteRole::PrimaryRc, WriteOutcome::Created) => eprintln!(
                "(ji)::shell created {} with managed block",
                w.path.display()
            ),
            (WriteRole::PrimaryRc, WriteOutcome::Updated) => eprintln!(
                "(ji)::shell updated {} with managed block",
                w.path.display()
            ),
            (WriteRole::PrimaryRc, WriteOutcome::Unchanged) => {
                eprintln!("(ji)::shell {} already up-to-date", w.path.display());
            }
            (WriteRole::FishWrapper, WriteOutcome::Created) => {
                eprintln!("(ji)::shell wrapper installed in {}", w.path.display());
            }
            (WriteRole::FishWrapper, WriteOutcome::Updated) => {
                eprintln!("(ji)::shell wrapper updated in {}", w.path.display());
            }
            (WriteRole::FishWrapper, WriteOutcome::Unchanged) => eprintln!(
                "(ji)::shell wrapper already up-to-date in {}",
                w.path.display()
            ),
            (WriteRole::FishCompletions, WriteOutcome::Created) => {
                eprintln!("(ji)::shell completions installed in {}", w.path.display());
            }
            (WriteRole::FishCompletions, WriteOutcome::Updated) => {
                eprintln!("(ji)::shell completions updated in {}", w.path.display());
            }
            (WriteRole::FishCompletions, WriteOutcome::Unchanged) => eprintln!(
                "(ji)::shell completions already up-to-date in {}",
                w.path.display()
            ),
        }
    }

    // Removals (fish heal): delete a redundant/stale user-dir completion shadowed
    // by a dynamic vendor completion. Each op is individually atomic; the plan is
    // a sequential multi-file apply, like the writes above.
    for r in &plan.removals {
        match std::fs::remove_file(&r.path) {
            Ok(()) => eprintln!("(ji)::shell removed {}", r.path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::Error::new(e))
                    .with_context(|| format!("failed to remove {}", r.path.display()));
            }
        }
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
                print_dry_run_diff(&mut std::io::stdout(), &primary, &contents, &after);
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
    if !opts.dry_run {
        clear_install_prompt_declined(env, shell);
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
    if !opts.dry_run {
        clear_install_prompt_declined(env, "fish");
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

fn print_active_report(r: &ActiveReport, w: &mut dyn std::io::Write) {
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
    let _ = writeln!(w, "  active shell (detected): {active}{exe}");
    let _ = writeln!(
        w,
        "  login shell ($SHELL):    {}",
        r.shell_env_raw.as_deref().unwrap_or("<unset>")
    );
}

/// Report shell-integration state. The detection step is **non-fatal**: an
/// unsupported/unknown active shell is reported, not an error (only the
/// mutating commands error on that). Genuine I/O errors from the underlying
/// inspectors still propagate. Output goes through an injected writer so tests
/// can capture and assert it.
pub fn status(env: &ShellEnv, target: Option<&str>) -> Result<()> {
    status_to(env, target, &mut std::io::stdout())
}

fn status_to(env: &ShellEnv, target: Option<&str>, w: &mut dyn std::io::Write) -> Result<()> {
    let report = detect_active_report();
    print_active_report(&report, w);

    let shell = match target {
        Some(t) => t.to_string(),
        None => match status_action(&report) {
            StatusAction::Inspect(s) => s,
            StatusAction::ReportOnly => {
                let _ = writeln!(
                    w,
                    "  no supported active shell to inspect; pass one explicitly (zsh|bash|fish)"
                );
                return Ok(());
            }
        },
    };

    match shell.as_str() {
        "zsh" | "bash" => status_posix(env, &shell, w),
        "fish" => status_fish(env, w),
        other => bail!("unsupported shell: {other} (supported: {SUPPORTED_SHELLS})"),
    }
}

fn status_posix(env: &ShellEnv, shell: &str, w: &mut dyn std::io::Write) -> Result<()> {
    let primary = if shell == "zsh" {
        zsh_primary(env)
    } else {
        bash_primary(env)
    };
    let managed = env.managed_file(shell)?;
    let body = render_managed_body(shell)?;
    let stanza = render_rc_stanza(shell)?;

    let _ = writeln!(w, "ji shell integration: {shell}");
    let _ = writeln!(w, "  primary rc: {}", display_canonical(&primary));
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
        let _ = writeln!(w, "  bash candidates: {}", candidates.join(", "));
    }
    let _ = writeln!(
        w,
        "  managed file: {} ({})",
        display_canonical(&managed),
        state_for(&managed, &body)
    );
    let _ = writeln!(w, "  rc stanza: {}", rc_stanza_state(&primary, &stanza));

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
        let _ = writeln!(w, "  cross-file hits: none");
    } else {
        let _ = writeln!(w, "  cross-file hits:");
        for h in cross {
            let via = h
                .via
                .as_ref()
                .map(|(p, n)| format!(" (via source line {} in {})", n, p.display()))
                .unwrap_or_default();
            let _ = writeln!(
                w,
                "    {}:{}{} — {}",
                h.path.display(),
                h.line_no,
                via,
                h.line_text
            );
        }
    }
    let shadows = scan_bypass_aliases(env, shell, &primary);
    if shadows.is_empty() {
        let _ = writeln!(w, "  bypass aliases: none");
    } else {
        let _ = writeln!(w, "  bypass aliases:");
        for h in &shadows {
            let what = if matches!(h.kind, ShadowKind::Function) {
                "function ji".to_string()
            } else {
                format!("alias {} -> {}", h.name, h.target)
            };
            let note = if h.shadows_wrapper() {
                "shadows the wrapper"
            } else {
                "runs ji directly"
            };
            let _ = writeln!(
                w,
                "    {}:{} — {} ({})",
                h.path.display(),
                h.line_no,
                what,
                note
            );
        }
    }
    let unresolved: Vec<&Hit> = hits
        .iter()
        .filter(|h| matches!(h.kind, HitKind::UnresolvedSource(_)))
        .collect();
    if !unresolved.is_empty() {
        let _ = writeln!(w, "  unresolved source lines:");
        for h in unresolved {
            println!("    {}:{} — {}", h.path.display(), h.line_no, h.line_text);
        }
    }

    Ok(())
}

/// The completion-related status line(s) for fish, given the scan, whether the
/// probe failed, and the user file's state/managed-ness. Pure/unit-testable.
fn fish_completion_status_lines(
    scan: &FishCompletionScan,
    probe_failed: bool,
    user_path: &Path,
    user_state: &str,
    user_managed: bool,
) -> Vec<String> {
    let up = user_path.display();
    if probe_failed {
        return vec![
            "  note: could not query fish's $fish_complete_path (fish missing or slow); vendor-shadow detection unavailable — run `ji config shell install fish` if completions misbehave".to_string(),
            format!("  completions file: {up} ({user_state})"),
        ];
    }
    let mut lines = Vec::new();
    if let Some(vendor) = &scan.vendor_provider {
        let vp = vendor.display();
        if scan.user_file_exists && scan.user_shadows_vendor {
            if !user_managed {
                lines.push(format!(
                    "  WARNING {up} (not ji-managed) shadows {vp} — remove it manually or run `ji config shell install fish --force`"
                ));
            } else if user_state == "drift" {
                lines.push(format!(
                    "  WARNING {up} is a stale completion shadowing {vp} — run `ji config shell install fish` to remove it"
                ));
            } else {
                lines.push(format!(
                    "  note: {up} shadows {vp} (redundant); `ji config shell install fish` will remove it"
                ));
            }
        } else if scan.user_file_exists {
            lines.push(format!(
                "  note: {up} is present but not in effect; {vp} is used"
            ));
        } else {
            lines.push(format!("  completions: provided by {vp}"));
        }
    } else {
        lines.push(format!("  completions file: {up} ({user_state})"));
    }
    if let Some(p) = &scan.blocking_non_user {
        lines.push(format!(
            "  WARNING {} is earlier in $fish_complete_path and is not ji's dynamic completion — it shadows any ji completion later in the path; remove it manually",
            p.display()
        ));
    }
    lines
}

fn status_fish(env: &ShellEnv, w: &mut dyn std::io::Write) -> Result<()> {
    let primary = fish_primary(env);
    let completions = fish_completions_path(env);
    let shadow = fish_shadow_path(env);
    let wrapper_body = render_fish_wrapper_body();

    let _ = writeln!(w, "ji shell integration: fish");
    let _ = writeln!(
        w,
        "  wrapper file: {} ({})",
        display_canonical(&primary),
        state_for(&primary, &wrapper_body)
    );

    // Completion state (vendor-aware): the user-dir completion may be shadowing a
    // dynamic vendor completion, or absent because a vendor provides it.
    let state = fish_completion_state(env);
    let completion_body = render_fish_completion_body()?;
    let user_state = state_for(&completions, &completion_body);
    let user_managed = completions.exists()
        && std::fs::read_to_string(&completions)
            .map(|c| is_managed(&c))
            .unwrap_or(false);
    for line in fish_completion_status_lines(
        &state.scan,
        state.probe_failed,
        &completions,
        user_state,
        user_managed,
    ) {
        let _ = writeln!(w, "{line}");
    }

    if shadow.exists() {
        let _ = writeln!(
            w,
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
        let _ = writeln!(w, "  cross-file hits: none");
    } else {
        let _ = writeln!(w, "  cross-file hits:");
        for h in cross {
            let _ = writeln!(
                w,
                "    {}:{} — {}",
                h.path.display(),
                h.line_no,
                h.line_text
            );
        }
    }
    let shadows = scan_bypass_aliases(env, "fish", &primary);
    if shadows.is_empty() {
        let _ = writeln!(w, "  bypass aliases: none");
    } else {
        let _ = writeln!(w, "  bypass aliases:");
        for h in &shadows {
            let what = if matches!(h.kind, ShadowKind::Function) {
                "function ji".to_string()
            } else {
                format!("alias {} -> {}", h.name, h.target)
            };
            let note = if h.shadows_wrapper() {
                "shadows the wrapper"
            } else {
                "runs ji directly"
            };
            let _ = writeln!(
                w,
                "    {}:{} — {} ({})",
                h.path.display(),
                h.line_no,
                what,
                note
            );
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
mod alias_tests {
    use super::*;

    #[test]
    fn detect_bypass_alias_flags_direct_binary_aliases() {
        for (line, name, target) in [
            ("alias ji=/opt/bin/ji", "ji", "/opt/bin/ji"),
            ("alias j=/opt/bin/ji", "j", "/opt/bin/ji"),
            (
                "alias ji='/opt/bin/ji --color'",
                "ji",
                "/opt/bin/ji --color",
            ),
            ("alias ji='command ji'", "ji", "command ji"),
            (
                "alias gji='command /opt/bin/ji'",
                "gji",
                "command /opt/bin/ji",
            ),
            (r#"alias ji="$HOME/bin/ji""#, "ji", "$HOME/bin/ji"),
            ("alias ji /usr/bin/ji", "ji", "/usr/bin/ji"),
            ("alias ji 'command ji'", "ji", "command ji"),
            (
                "alias ji='env FOO=bar /opt/bin/ji'",
                "ji",
                "env FOO=bar /opt/bin/ji",
            ),
            ("alias ji='env FOO=1 ji'", "ji", "env FOO=1 ji"),
        ] {
            assert_eq!(
                detect_bypass_alias(line),
                Some((name.to_string(), target.to_string())),
                "{line}"
            );
        }
    }

    #[test]
    fn detect_bypass_alias_rejects_non_bypass_aliases() {
        for line in [
            "alias ji=ji",
            "alias ji='ji --x'",
            "alias x='echo /opt/bin/ji'",
            "alias ji='command -v /opt/bin/ji'",
            "alias x=/p/jj",
            "alias jcsi='ji config shell init zsh'",
            "alias ji='$(which ji)'",
            "unalias ji",
            "# alias ji=",
            "alias ji",
        ] {
            assert_eq!(detect_bypass_alias(line), None, "{line}");
        }
    }

    #[test]
    fn alias_hit_shadows_wrapper_for_ji_alias_or_function() {
        let alias_ji = AliasHit {
            path: PathBuf::from("/tmp/rc"),
            line_no: 1,
            name: "ji".into(),
            target: "/opt/bin/ji".into(),
            kind: ShadowKind::Alias,
        };
        let alias_j = AliasHit {
            name: "j".into(),
            ..alias_ji.clone()
        };
        let function_ji = AliasHit {
            target: String::new(),
            kind: ShadowKind::Function,
            ..alias_ji.clone()
        };

        assert!(alias_ji.shadows_wrapper());
        assert!(!alias_j.shadows_wrapper());
        assert!(function_ji.shadows_wrapper());
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
    use tempfile::TempDir;

    fn env_for(tmp: &TempDir) -> ShellEnv {
        let home = tmp.path().to_path_buf();
        let xdg = home.join(".config");
        ShellEnv {
            home: home.clone(),
            xdg_config_home: xdg.clone(),
            zdotdir: home,
            zsh_custom: None,
            omz_root: None,
            // Deterministic no-vendor: only the temp user completions dir on the
            // path, so no real fish is spawned in the test suite.
            fish_complete_path: FishProbeSource::Probed(vec![xdg.join("fish/completions")]),
        }
    }

    // ---- inline install offer gate + decline markers ----

    #[test]
    fn should_offer_only_for_not_installed_inspect_tty_not_declined() {
        let inspect = StatusAction::Inspect("zsh".into());
        assert!(should_offer(
            &inspect,
            &CdUnavailable::NotInstalled,
            true,
            false
        ));

        for reason in [
            CdUnavailable::InstalledInactive,
            CdUnavailable::RanByPath,
            CdUnavailable::NeedsRefresh,
            CdUnavailable::UnsupportedShell("nu".into()),
        ] {
            assert!(!should_offer(&inspect, &reason, true, false), "{reason:?}");
        }

        assert!(!should_offer(
            &StatusAction::ReportOnly,
            &CdUnavailable::NotInstalled,
            true,
            false
        ));
        assert!(!should_offer(
            &inspect,
            &CdUnavailable::NotInstalled,
            false,
            false
        ));
        assert!(!should_offer(
            &inspect,
            &CdUnavailable::NotInstalled,
            true,
            true
        ));
    }

    #[test]
    fn install_prompt_declined_marker_is_per_shell() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp);
        let expected = env
            .xdg_config_home
            .join("ji")
            .join("install-prompt-declined-zsh");
        assert_eq!(declined_marker(&env, "zsh"), expected);

        set_install_prompt_declined(&env, "zsh");
        assert!(install_prompt_declined(&env, "zsh"));
        assert!(!install_prompt_declined(&env, "bash"));

        clear_install_prompt_declined(&env, "zsh");
        assert!(!install_prompt_declined(&env, "zsh"));
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
        // "restart"-class advice is reachable only from Working:
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

    // ---- packaged (xtask) completion ----

    #[test]
    fn packaged_zsh_is_autoload_safe() {
        let zsh = packaged_completion("zsh").unwrap();
        assert!(
            zsh.starts_with("#compdef ji"),
            "packaged _ji must start with the #compdef autoload header"
        );
        assert!(
            zsh.contains("funcstack[1]"),
            "packaged _ji must carry the dual-mode autoload guard"
        );
        // The bare trailing `compdef … ji` must have been replaced by the guard.
        assert!(
            !zsh.trim_end()
                .ends_with("compdef _clap_dynamic_completer_ji ji")
        );
        // bash/fish pass clap's registration through unchanged (dynamic).
        assert!(packaged_completion("bash").unwrap().contains("COMPLETE"));
        let fish = packaged_completion("fish").unwrap();
        assert!(fish.contains("complete") && fish.contains("COMPLETE=fish"));
    }

    // ---- wrapper_state (temp env; matches install) ----

    #[test]
    fn wrapper_state_canonical_zsh_install_is_working() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp);
        install(&env, "zsh", InstallOpts::default()).unwrap();
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
        install(&env, "zsh", InstallOpts::default()).unwrap();
        std::fs::write(env.managed_file("zsh").unwrap(), "garbage\n").unwrap();
        assert_eq!(wrapper_state(&env, "zsh"), WrapperState::Drift);
    }

    #[test]
    fn wrapper_state_fish_shadow_blocks_working() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp);
        install(&env, "fish", InstallOpts::default()).unwrap();
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
        install(&env, "zsh", InstallOpts::default()).unwrap();
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
        install(&env, "zsh", InstallOpts::default()).unwrap();
        assert_eq!(wrapper_state(&env, "zsh"), WrapperState::Working);
        // A second primary marker block ⇒ not an exact single canonical install.
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

#[cfg(test)]
mod fish_completion_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn dynamic_body() -> String {
        packaged_completion("fish").unwrap()
    }

    /// A stale static user completion (pre-0.1.4 `generate` output), ji-managed:
    /// contains the managed header but not ji's dynamic callback signature.
    fn static_managed() -> String {
        format!(
            "{MANAGED_HEADER}\ncomplete -c ji -n \"__fish_ji_needs_command\" -f -a \"switch\"\n"
        )
    }

    fn write_file(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn env_for(tmp: &TempDir, probe: FishProbeSource) -> ShellEnv {
        let home = tmp.path().to_path_buf();
        let xdg = home.join(".config");
        ShellEnv {
            home: home.clone(),
            xdg_config_home: xdg,
            zdotdir: home,
            zsh_custom: None,
            omz_root: None,
            fish_complete_path: probe,
        }
    }

    fn fact(index: usize, is_user: bool, is_dynamic: bool) -> ProviderFact {
        ProviderFact {
            index,
            is_user,
            is_dynamic,
            path: PathBuf::from(format!("/d{index}/ji.fish")),
        }
    }

    // ---- content classification ----

    #[test]
    fn is_dynamic_registration_pinned_to_real_bodies() {
        assert!(is_dynamic_ji_registration(
            &packaged_completion("fish").unwrap()
        ));
        assert!(is_dynamic_ji_registration(
            &render_fish_completion_body().unwrap()
        ));
        assert!(!is_dynamic_ji_registration(&static_managed()));
        // Mentions COMPLETE=fish but is not ji's callback → not a false positive.
        assert!(!is_dynamic_ji_registration(
            "set -x COMPLETE=fish\n# unrelated\n"
        ));
    }

    // ---- pure order-aware decision ----

    #[test]
    fn decide_user_before_dynamic_vendor_shadows() {
        let facts = vec![fact(0, true, false), fact(1, false, true)];
        let s = fish_decide(&facts, Some(0), true);
        assert_eq!(s.vendor_provider.as_deref(), Some(Path::new("/d1/ji.fish")));
        assert!(s.user_shadows_vendor);
        assert!(s.blocking_non_user.is_none());
    }

    #[test]
    fn decide_dynamic_vendor_only_no_user() {
        let facts = vec![fact(1, false, true)];
        let s = fish_decide(&facts, Some(0), false);
        assert_eq!(s.vendor_provider.as_deref(), Some(Path::new("/d1/ji.fish")));
        assert!(!s.user_file_exists);
        assert!(!s.user_shadows_vendor);
    }

    #[test]
    fn decide_user_only_no_vendor() {
        let facts = vec![fact(0, true, true)];
        let s = fish_decide(&facts, Some(0), true);
        assert!(s.vendor_provider.is_none());
        assert!(s.blocking_non_user.is_none());
    }

    #[test]
    fn decide_static_first_nonuser_blocks_later_dynamic() {
        // First non-user is static (d1); a dynamic vendor (d2) is later but
        // unreachable under fish first-match → no vendor_provider.
        let facts = vec![
            fact(0, true, false),
            fact(1, false, false),
            fact(2, false, true),
        ];
        let s = fish_decide(&facts, Some(0), true);
        assert!(s.vendor_provider.is_none());
        assert_eq!(
            s.blocking_non_user.as_deref(),
            Some(Path::new("/d1/ji.fish"))
        );
        assert!(!s.user_shadows_vendor);
    }

    #[test]
    fn collect_unreadable_nonuser_is_blocking_not_skipped() {
        let tmp = TempDir::new().unwrap();
        let user_dir = tmp.path().join("user");
        let block_dir = tmp.path().join("block");
        let vendor = tmp.path().join("vendor");
        std::fs::create_dir_all(&user_dir).unwrap();
        // Dynamic content but unreadable → must be treated as a (non-dynamic) blocker.
        write_file(&block_dir.join("ji.fish"), &dynamic_body());
        std::fs::set_permissions(
            block_dir.join("ji.fish"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        write_file(&vendor.join("ji.fish"), &dynamic_body());
        // Skip if the sandbox ignores perms (e.g. running as root) — the file
        // would then read as dynamic and the "unreadable" premise wouldn't hold.
        if std::fs::read_to_string(block_dir.join("ji.fish")).is_ok() {
            return;
        }
        let path = vec![user_dir.clone(), block_dir.clone(), vendor];
        let (facts, _idx) = fish_collect_providers(&path, &user_dir);
        let s = fish_decide(&facts, Some(0), false);
        assert!(s.vendor_provider.is_none());
        assert_eq!(
            s.blocking_non_user.as_deref(),
            Some(block_dir.join("ji.fish").as_path())
        );
    }

    // ---- plan_install_fish decisions ----

    fn user_completion_path(env: &ShellEnv) -> PathBuf {
        fish_completions_path(env)
    }

    #[test]
    fn install_vendor_present_removes_stale_managed_user_file() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp, FishProbeSource::Live);
        let user = user_completion_path(&env);
        let vendor = tmp.path().join("vendor");
        write_file(&vendor.join("ji.fish"), &dynamic_body());
        write_file(&user, &static_managed());
        let env = ShellEnv {
            fish_complete_path: FishProbeSource::Probed(vec![
                user.parent().unwrap().to_path_buf(),
                vendor.clone(),
            ]),
            ..env
        };

        let plan = plan_install_fish(&env, false, &mut Vec::new()).unwrap();
        assert_eq!(
            plan.fish_completion_mode,
            Some(FishCompletionMode::Vendor {
                provider: vendor.join("ji.fish")
            })
        );
        assert_eq!(plan.removals.len(), 1);
        assert_eq!(plan.removals[0].path, user);
        assert!(
            !plan
                .writes
                .iter()
                .any(|w| w.role == WriteRole::FishCompletions)
        );
        assert!(plan.changed());
    }

    #[test]
    fn install_vendor_present_no_user_file_is_noop_completion() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp, FishProbeSource::Live);
        let user = user_completion_path(&env);
        let vendor = tmp.path().join("vendor");
        write_file(&vendor.join("ji.fish"), &dynamic_body());
        let env = ShellEnv {
            fish_complete_path: FishProbeSource::Probed(vec![
                user.parent().unwrap().to_path_buf(),
                vendor,
            ]),
            ..env
        };
        let plan = plan_install_fish(&env, false, &mut Vec::new()).unwrap();
        assert!(plan.removals.is_empty());
        assert!(
            !plan
                .writes
                .iter()
                .any(|w| w.role == WriteRole::FishCompletions)
        );
        assert!(matches!(
            plan.fish_completion_mode,
            Some(FishCompletionMode::Vendor { .. })
        ));
    }

    #[test]
    fn install_vendor_present_nonmanaged_shadow_bails() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp, FishProbeSource::Live);
        let user = user_completion_path(&env);
        let vendor = tmp.path().join("vendor");
        write_file(&vendor.join("ji.fish"), &dynamic_body());
        write_file(&user, "# hand authored, not ji-managed\n");
        let env = ShellEnv {
            fish_complete_path: FishProbeSource::Probed(vec![
                user.parent().unwrap().to_path_buf(),
                vendor,
            ]),
            ..env
        };
        let err = plan_install_fish(&env, false, &mut Vec::new())
            .err()
            .unwrap();
        assert!(format!("{err}").contains("shadows the vendor completion"));
    }

    #[test]
    fn install_no_vendor_writes_user_file() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(
            &tmp,
            FishProbeSource::Probed(vec![tmp.path().join(".config/fish/completions")]),
        );
        let plan = plan_install_fish(&env, false, &mut Vec::new()).unwrap();
        assert_eq!(
            plan.fish_completion_mode,
            Some(FishCompletionMode::UserFile)
        );
        let cw = plan
            .writes
            .iter()
            .find(|w| w.role == WriteRole::FishCompletions)
            .expect("completion write");
        assert!(is_dynamic_ji_registration(&cw.after));
        assert!(plan.removals.is_empty());
    }

    #[test]
    fn install_probe_failed_refreshes_managed_user_file() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp, FishProbeSource::Failed);
        write_file(&user_completion_path(&env), &static_managed());
        let plan = plan_install_fish(&env, false, &mut Vec::new()).unwrap();
        assert_eq!(
            plan.fish_completion_mode,
            Some(FishCompletionMode::UserFile)
        );
        let cw = plan
            .writes
            .iter()
            .find(|w| w.role == WriteRole::FishCompletions)
            .expect("completion write");
        assert!(is_dynamic_ji_registration(&cw.after));
        assert!(plan.removals.is_empty());
    }

    #[test]
    fn install_probe_failed_nonmanaged_user_file_bails() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp, FishProbeSource::Failed);
        write_file(&user_completion_path(&env), "# hand authored\n");
        let err = plan_install_fish(&env, false, &mut Vec::new())
            .err()
            .unwrap();
        assert!(format!("{err}").contains("not ji-managed"));
    }

    #[test]
    fn dry_run_show_lists_removal() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp, FishProbeSource::Live);
        let user = user_completion_path(&env);
        let vendor = tmp.path().join("vendor");
        write_file(&vendor.join("ji.fish"), &dynamic_body());
        write_file(&user, &static_managed());
        let env = ShellEnv {
            fish_complete_path: FishProbeSource::Probed(vec![
                user.parent().unwrap().to_path_buf(),
                vendor,
            ]),
            ..env
        };
        let plan = plan_install_fish(&env, false, &mut Vec::new()).unwrap();
        let mut buf = Vec::new();
        plan.show(&mut buf);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("- would delete"));
        assert!(out.contains("completions/ji.fish"));
    }

    // ---- installed_ok (plan-driven) ----

    fn fish_plan(mode: FishCompletionMode, removals: Vec<PlannedRemoval>) -> ShellInstallPlan {
        ShellInstallPlan {
            shell: "fish".to_string(),
            writes: Vec::new(),
            removals,
            shadows: Vec::new(),
            fish_completion_mode: Some(mode),
        }
    }

    #[test]
    fn installed_ok_vendor_mode_healthy() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp, FishProbeSource::Live);
        write_file(&fish_primary(&env), &render_fish_wrapper_body());
        let vendor_ji = tmp.path().join("vendor/ji.fish");
        write_file(&vendor_ji, &dynamic_body());
        let plan = fish_plan(
            FishCompletionMode::Vendor {
                provider: vendor_ji,
            },
            Vec::new(),
        );
        assert!(installed_ok(&env, &plan));
    }

    #[test]
    fn installed_ok_vendor_mode_unapplied_removal_is_unhealthy() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp, FishProbeSource::Live);
        write_file(&fish_primary(&env), &render_fish_wrapper_body());
        let vendor_ji = tmp.path().join("vendor/ji.fish");
        write_file(&vendor_ji, &dynamic_body());
        let leftover = tmp.path().join("leftover/ji.fish");
        write_file(&leftover, &static_managed());
        let plan = fish_plan(
            FishCompletionMode::Vendor {
                provider: vendor_ji,
            },
            vec![PlannedRemoval {
                path: leftover,
                existed: true,
            }],
        );
        assert!(!installed_ok(&env, &plan));
    }

    #[test]
    fn installed_ok_user_file_mode_healthy() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp, FishProbeSource::Live);
        write_file(&fish_primary(&env), &render_fish_wrapper_body());
        write_file(
            &fish_completions_path(&env),
            &render_fish_completion_body().unwrap(),
        );
        let plan = fish_plan(FishCompletionMode::UserFile, Vec::new());
        assert!(installed_ok(&env, &plan));
    }

    // ---- status lines ----

    fn scan(
        user_file_exists: bool,
        vendor: Option<&str>,
        blocking: Option<&str>,
        user_shadows_vendor: bool,
    ) -> FishCompletionScan {
        FishCompletionScan {
            user_file_exists,
            vendor_provider: vendor.map(PathBuf::from),
            blocking_non_user: blocking.map(PathBuf::from),
            user_shadows_vendor,
            blocking_precedes_user: false,
        }
    }

    #[test]
    fn status_lines_stale_managed_shadow_warns_with_install_fix() {
        let s = scan(true, Some("/v/ji.fish"), None, true);
        let lines = fish_completion_status_lines(&s, false, Path::new("/u/ji.fish"), "drift", true);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("stale completion shadowing")
                    && l.contains("ji config shell install fish"))
        );
    }

    #[test]
    fn status_lines_nonmanaged_shadow_warns_with_force() {
        let s = scan(true, Some("/v/ji.fish"), None, true);
        let lines =
            fish_completion_status_lines(&s, false, Path::new("/u/ji.fish"), "drift", false);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("not ji-managed") && l.contains("--force"))
        );
    }

    #[test]
    fn status_lines_inert_present_not_reported_as_shadow() {
        let s = scan(true, Some("/v/ji.fish"), None, false);
        let lines =
            fish_completion_status_lines(&s, false, Path::new("/u/ji.fish"), "present", true);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("present but not in effect"))
        );
        assert!(!lines.iter().any(|l| l.contains("shadow")));
    }

    #[test]
    fn status_lines_vendor_healthy_and_no_vendor_and_probe_failed() {
        let healthy = fish_completion_status_lines(
            &scan(false, Some("/v/ji.fish"), None, false),
            false,
            Path::new("/u/ji.fish"),
            "absent",
            false,
        );
        assert!(healthy.iter().any(|l| l.contains("provided by /v/ji.fish")));

        let no_vendor = fish_completion_status_lines(
            &scan(true, None, None, false),
            false,
            Path::new("/u/ji.fish"),
            "present",
            true,
        );
        assert!(no_vendor.iter().any(|l| l.contains("completions file:")));

        let failed = fish_completion_status_lines(
            &scan(true, None, None, false),
            true,
            Path::new("/u/ji.fish"),
            "drift",
            true,
        );
        assert!(failed.iter().any(|l| l.contains("could not query fish")));
    }

    #[test]
    fn status_lines_blocking_non_user_warns() {
        let s = scan(false, None, Some("/early/ji.fish"), false);
        let lines =
            fish_completion_status_lines(&s, false, Path::new("/u/ji.fish"), "absent", false);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("/early/ji.fish")
                    && l.contains("earlier in $fish_complete_path"))
        );
    }

    #[test]
    fn status_fish_output_names_vendor_and_fix() {
        let tmp = TempDir::new().unwrap();
        let env = env_for(&tmp, FishProbeSource::Live);
        let user = fish_completions_path(&env);
        let vendor = tmp.path().join("vendor");
        write_file(&vendor.join("ji.fish"), &dynamic_body());
        write_file(&user, &static_managed());
        write_file(&fish_primary(&env), &render_fish_wrapper_body());
        let env = ShellEnv {
            fish_complete_path: FishProbeSource::Probed(vec![
                user.parent().unwrap().to_path_buf(),
                vendor.clone(),
            ]),
            ..env
        };
        let mut buf = Vec::new();
        status_fish(&env, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains(&vendor.join("ji.fish").display().to_string()));
        assert!(out.contains("ji config shell install fish"));
    }
}
