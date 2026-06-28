//! Dynamic shell-completion providers for `ji`.
//!
//! Driven by clap_complete's `unstable-dynamic` engine: at completion time the
//! binary runs `CompleteEnv::try_complete` (see `main`), which invokes these
//! completers. Each reads live jj state through one non-mutating,
//! bounded-timeout subprocess and is fail-soft — any error yields no
//! candidates, never an error at the prompt.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};

use crate::jujutsu::{self, CompletionWorkspace};
use crate::text_utils::relative_time_short;

/// Shell name captured in `main` before `CompleteEnv::try_complete` strips the
/// `COMPLETE` env var (clap removes it before invoking completers, so a
/// completer can't read it directly). Normalized like clap: `COMPLETE` may be a
/// path such as `/bin/bash`, so take the file stem and lowercase it.
static SHELL: OnceLock<Option<String>> = OnceLock::new();

/// Record the active completion shell. Call once, in `main`, before
/// `try_complete`. Idempotent — later calls are ignored.
pub fn capture_shell(raw: Option<String>) {
    let normalized = raw.map(|s| {
        Path::new(&s).file_stem().map_or_else(
            || s.clone(),
            |stem| stem.to_string_lossy().to_ascii_lowercase(),
        )
    });
    let _ = SHELL.set(normalized);
}

/// Whether the active completion shell is bash. bash's dynamic completion does
/// not prefix-filter candidate values, so bash candidates are filtered in the
/// completer; zsh/fish do their own substring/fuzzy matching and receive the
/// full list (matching worktrunk).
fn is_bash() -> bool {
    SHELL.get().and_then(Clone::clone).as_deref() == Some("bash")
}

/// Current epoch seconds (UTC), or 0 if the clock predates the epoch.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Name of the workspace whose root is the longest canonicalized prefix of the
/// canonicalized cwd — the "current" workspace. Canonicalizing both sides (and
/// using `Path::starts_with`, not string prefixing) avoids macOS `/var` vs
/// `/private/var` alias mismatches.
fn current_workspace_name(workspaces: &[CompletionWorkspace]) -> Option<String> {
    let cwd = std::env::current_dir().ok()?.canonicalize().ok()?;
    let mut best: Option<(usize, &str)> = None;
    for ws in workspaces {
        let Some(root) = ws.root.as_deref() else {
            continue;
        };
        let Ok(root) = root.canonicalize() else {
            continue;
        };
        if cwd.starts_with(&root) {
            let depth = root.components().count();
            if best.is_none_or(|(d, _)| depth > d) {
                best = Some((depth, &ws.name));
            }
        }
    }
    best.map(|(_, name)| name.to_string())
}

/// Annotated workspace candidates: each workspace name with help
/// `(<reltime>)[<sym>]`, where `<sym>` is `*` (current) or `x` (orphaned), and
/// the brackets are omitted when neither applies. Sorted non-orphaned first,
/// then most-recently-touched, then name; `display_order` carries that order
/// through clap's engine. `current` is the token being completed.
fn workspace_candidates(current: &OsStr) -> Vec<CompletionCandidate> {
    let token = current.to_string_lossy();
    if token.starts_with('-') {
        return Vec::new(); // completing a flag, not a workspace
    }
    let Ok(workspaces) = jujutsu::completion_workspaces() else {
        return Vec::new(); // fail-soft
    };
    let current_name = current_workspace_name(&workspaces);
    render_workspace_candidates(
        workspaces,
        current_name.as_deref(),
        now_secs(),
        is_bash(),
        &token,
    )
}

/// Classify (current / orphaned), sort (non-orphaned first, then most-recently-
/// touched, then name), bash-only prefix-filter, and format each workspace as a
/// candidate with `(<reltime>)[<sym>]` help and engine `display_order`. Split
/// from the jj fetch + cwd lookup so the pure logic is unit-testable.
fn render_workspace_candidates(
    workspaces: Vec<CompletionWorkspace>,
    current_name: Option<&str>,
    now: i64,
    bash: bool,
    token: &str,
) -> Vec<CompletionCandidate> {
    // (workspace, is_current, is_orphaned)
    let mut rows: Vec<(CompletionWorkspace, bool, bool)> = workspaces
        .into_iter()
        .map(|ws| {
            let is_current = current_name == Some(ws.name.as_str());
            let is_orphaned = ws.root.as_deref().is_none_or(|root| !root.exists());
            (ws, is_current, is_orphaned)
        })
        .collect();

    // non-orphaned before orphaned (false < true); then last_touched desc; name asc.
    rows.sort_by(|a, b| {
        a.2.cmp(&b.2)
            .then_with(|| b.0.last_touched.cmp(&a.0.last_touched))
            .then_with(|| a.0.name.cmp(&b.0.name))
    });

    rows.into_iter()
        .filter(|(ws, _, _)| !bash || ws.name.starts_with(token))
        .enumerate()
        .map(|(i, (ws, is_current, is_orphaned))| {
            let reltime = ws
                .last_touched
                .map_or_else(|| "?".to_string(), |t| relative_time_short(t, now));
            let sym = if is_current {
                "[*]"
            } else if is_orphaned {
                "[x]"
            } else {
                ""
            };
            CompletionCandidate::new(ws.name.as_str())
                .help(Some(format!("({reltime}){sym}").into()))
                .display_order(Some(i))
        })
        .collect()
}

/// Existing local bookmark names, annotated with `(<reltime>)`, for
/// `ji new <bookmark>` (novel names remain free-form). No `*`/`x` — those are
/// workspace states.
fn new_bookmark_candidates(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    if cur.starts_with('-') {
        return Vec::new();
    }
    let Ok(mut bookmarks) = jujutsu::completion_local_bookmarks() else {
        return Vec::new();
    };
    let now = now_secs();
    let bash = is_bash();
    // most-recently-touched first, then name.
    bookmarks.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    bookmarks
        .into_iter()
        .filter(|(name, _)| !bash || name.starts_with(&*cur))
        .enumerate()
        .map(|(i, (name, ts))| {
            let help = ts.map(|t| format!("({})", relative_time_short(t, now)));
            CompletionCandidate::new(name)
                .help(help.map(Into::into))
                .display_order(Some(i))
        })
        .collect()
}

/// Completer for workspace-target args (`switch`/`close`/`transfer`/`sync` and
/// their `--source` flags).
pub fn workspace_completer() -> ArgValueCompleter {
    ArgValueCompleter::new(workspace_candidates)
}

/// Completer for `ji new <bookmark>` (existing local bookmarks).
pub fn new_bookmark_completer() -> ArgValueCompleter {
    ArgValueCompleter::new(new_bookmark_candidates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ws(name: &str, root: Option<&str>, last: Option<i64>) -> CompletionWorkspace {
        CompletionWorkspace {
            name: name.to_string(),
            root: root.map(PathBuf::from),
            last_touched: last,
        }
    }

    /// Render to (value, help, display_order) triples for assertions.
    fn render(
        wss: Vec<CompletionWorkspace>,
        current: Option<&str>,
        bash: bool,
        token: &str,
    ) -> Vec<(String, String, Option<usize>)> {
        render_workspace_candidates(wss, current, 1_000_000, bash, token)
            .into_iter()
            .map(|c| {
                (
                    c.get_value().to_string_lossy().into_owned(),
                    c.get_help().map(ToString::to_string).unwrap_or_default(),
                    c.get_display_order(),
                )
            })
            .collect()
    }

    #[test]
    fn sort_symbols_and_display_order() {
        let now = 1_000_000;
        let wss = vec![
            ws("alpha", Some("/"), Some(now - 7200)), // 2h, exists
            ws("bravo", Some("/"), Some(now - 3600)), // 1h, exists, current
            ws("gone", None, Some(now - 86_400)),     // orphaned (root None), 1d
        ];
        let out = render(wss, Some("bravo"), false, "");
        // non-orphaned first by recency desc, then orphaned last.
        let names: Vec<&str> = out.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(names, ["bravo", "alpha", "gone"]);
        assert_eq!(out[0].1, "(1h)[*]"); // current
        assert_eq!(out[1].1, "(2h)"); // normal — brackets omitted
        assert_eq!(out[2].1, "(1d)[x]"); // orphaned
        // display_order strictly increasing from 0 (engine preserves our order).
        let orders: Vec<usize> = out.iter().map(|(_, _, d)| d.unwrap()).collect();
        assert_eq!(orders, [0, 1, 2]);
    }

    #[test]
    fn bash_prefix_filters_zsh_fish_do_not() {
        let now = 1_000_000;
        let make = || {
            vec![
                ws("main", Some("/"), Some(now - 10)),
                ws("maint", Some("/"), Some(now - 20)),
                ws("other", Some("/"), Some(now - 5)),
            ]
        };
        // bash filters to the typed prefix, inside the completer.
        let bash: Vec<String> = render(make(), None, true, "mai")
            .into_iter()
            .map(|(n, _, _)| n)
            .collect();
        assert_eq!(bash, ["main", "maint"]);
        // zsh/fish receive all candidates (they do their own matching).
        assert_eq!(render(make(), None, false, "mai").len(), 3);
    }
}
