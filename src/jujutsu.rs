use crate::hooks::HookVars;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const MIN_JJ_VERSION: &str = "0.42.0";
const JJ_INSTALL_URL: &str = "https://jj-vcs.github.io/jj/latest/install-and-setup/";

pub struct Workspace {
    pub name: String,
    pub change_id: String,
    pub description: String,
    /// Bookmarks at the head of this workspace's revision chain: (name, change_id).
    pub bookmarks_at_head: Vec<(String, String)>,
    /// Bookmarks behind the head (in the ancestry but not at the head): (name, change_id).
    pub bookmarks_behind: Vec<(String, String)>,
    pub is_current: bool,
    pub path: PathBuf,
    /// Revisions unique to this workspace (not shared with default). Empty for default workspace.
    pub revisions: Vec<RevisionInfo>,
    /// Epoch seconds of the most recent non-empty revision in this workspace's ancestry.
    pub last_modified: Option<i64>,
}

impl Workspace {
    /// Classify a bookmark as at-head or behind based on whether its change_id
    /// matches the effective head of the revision chain.
    pub(crate) fn classify_bookmark(&mut self, name: String, change_id: String) {
        let head_id = self
            .revisions
            .first()
            .map(|r| r.change_id.as_str())
            .unwrap_or(&self.change_id);
        if head_id == change_id {
            self.bookmarks_at_head.push((name, change_id));
        } else {
            self.bookmarks_behind.push((name, change_id));
        }
    }
}

#[derive(Clone)]
pub struct RevisionInfo {
    pub change_id: String,
    pub description: String,
}

/// A single jj operation from `jj op log`.
pub struct Operation {
    pub id: String,
    pub description: String,
    pub is_snapshot: bool,
    pub is_current: bool,
    pub tags: String,
    pub timestamp: String,
}

/// Maximum revisions that can be abandoned/squashed in one operation.
/// Safety cap to prevent catastrophic data loss from bad revsets.
const MAX_DESTRUCTIVE_REVISIONS: usize = 50;

// ---------------------------------------------------------------------------
// Template delimiter convention
// ---------------------------------------------------------------------------
//
// All jj templates in this codebase emit structured output using three ASCII
// control bytes:
//   - DELIM_RECORD (\x1e, ASCII RS) between top-level records.
//   - DELIM_FIELD  (\x1f, ASCII US) between fields within a record.
//   - DELIM_LIST   (\x01)           between elements of a list-valued field.
//
// Use `\x1e` for record separation everywhere — even when fields are
// bounded-line text and `\n` would technically work. Uniformity makes
// templates auditable and removes the silent-mis-parse failure mode that
// arises when a record-separator choice is changed without updating its
// parser.
//
// Rust-side parsers must split on the corresponding DELIM_* constant —
// record-framed output goes through `records()` below — so a template-edit
// that forgets to update the parser is caught by the source-level drift
// test (see tests/jj_integration.rs).

pub(crate) const DELIM_RECORD: char = '\x1e';
pub(crate) const DELIM_FIELD: char = '\x1f';
pub(crate) const DELIM_LIST: char = '\x01';

/// Template: `change_id \x1f description \x1e`.
pub(crate) const TMPL_ID_DESC: &str = r#"change_id ++ "\x1f" ++ description ++ "\x1e""#;

/// Split `DELIM_RECORD`-framed jj output into records: framing newlines
/// trimmed (descriptions and jj's trailing output newline land inside/after
/// records), empty records skipped (including the tail after the final
/// terminator). Field/arity handling stays with the caller — it varies by
/// record layout.
pub(crate) fn records(output: &str) -> impl Iterator<Item = &str> {
    output
        .split(DELIM_RECORD)
        .map(|r| r.trim_matches('\n'))
        .filter(|r| !r.is_empty())
}

// ---------------------------------------------------------------------------
// Working-copy snapshot policy — taxonomy and census
// ---------------------------------------------------------------------------
//
// TOTAL RULE: a jj call auto-snapshots the working copy iff it omits
// `--ignore-working-copy`.
//   - omit (live):    `jj_cmd_wc`, `jj_cmd_ws_wc` (+ the `run_jj_ws_live`
//                     wrapper)
//   - include (read): `jj_cmd`, `jj_cmd_ws`, `jj_cmd_bootstrap` (+ `run_jj`,
//                     `run_jj_bootstrap_timeout`, and the raw readers
//                     consuming them)
// Every jj subprocess in production code is constructed via one of these
// builders (`run_jj*`, `run_jj_output`, and direct `.status()` calls all
// consume one); the only exceptions are the builder bodies themselves and
// the version-detection call in `jj_version`. Pinned by the census test
// `jj_call_census_is_maintained` below.
//
// Variant taxonomy — which form belongs at a site:
//
// 1. Read recorded state ........ ignore-WC (`jj_cmd`/`jj_cmd_ws`/`run_jj`);
//    per-keystroke shell completion reads via `jj_cmd_bootstrap` +
//    `run_jj_bootstrap_timeout` (cwd-relative, no version check, no `-R`,
//    bounded ~9s timeout) — `completion_workspaces`/`completion_local_bookmarks`.
// 2. Involved-workspace freshness (gate / CLI entry) ... explicit
//    `snapshot_ws` (conditional) via `commands::snapshot_workspaces` —
//    capture src/tgt pending edits so the planned operation reflects
//    reality.
// 3. Execution-time freshness + third-party protection ... explicit
//    `snapshot_ws`, broad + fail-loud, descendant-first, in
//    `commands::prepare_execution_freshness` — capture every workspace's
//    pending edits before a rewrite can rebase/stale them.
// 4. WC-behind detection (which workspaces need `update-stale`) ... live
//    `jj status` (`is_workspace_stale`/`stale_workspace_names`); the
//    conditional snapshot side effect is incidental (and load-bearing only
//    at the TUI dialog-open probe in `refresh()`).
// 5. Safety inspection of the live WC ... live `jj log`
//    (`jj_utils::check_revision_safety`/`check_working_copy_safety`/
//    `check_chain_safety`) — forces pending edits into the emptiness test.
// 6. WC-ahead probe (pending edits without folding them) ...
//    `has_unsnapshotted_changes` — reserved for a future indicator; no
//    production caller.
// 7. Mutation that reads/writes the WC ... live builders — auto-snapshot
//    inherent (`op_restore`, `create_workspace`, `split_revision`,
//    `update_stale`, `update_workspace_stale`, `new_merge`;
//    `jj_utils::step_head`/`make_head`/`edit_workspace_head` via
//    `run_jj_ws_live`).
// 8. Mutation under ignore-WC with freshness from a preceding explicit
//    snapshot ... `workspace_forget` (here), the `operations.rs` merge
//    paths (`snapshot_ws(tgt)` then ignore-WC merge) and create's
//    step-source path.
//
// Census of live/snapshot call sites (update alongside the tripwire table
// in `jj_call_census_is_maintained` when adding/removing a site):
//   - `snapshot_ws`: `workspace_forget` (here);
//     `commands::snapshot_workspaces` (bucket 2);
//     `commands::prepare_execution_freshness` ×2 tiers (bucket 3);
//     `operations.rs` create step-source, `merge`, `merge_squash`,
//     `merge_abandon_parents_old` target snapshots (bucket 8).
//   - `run_jj_ws_live`: `jj_utils` head movers + safety checks (buckets
//     5, 7).
//   - `jj_cmd_wc` (repo-live): `op_restore`, `create_workspace`,
//     `split_revision`, `update_stale`, `is_working_copy_stale`.
//   - `jj_cmd_ws_wc` (workspace-live): `snapshot_ws`,
//     `has_unsnapshotted_changes` preview, `run_jj_ws_live`, `new_merge`,
//     `is_workspace_stale`, `update_workspace_stale`.
//   - `is_workspace_stale`: `stale_workspace_names` (batch), the TUI
//     `refresh()` dialog-open probe, `operations::forget_workspace`,
//     the protection phase's stale-skip classifier (bucket 3).
//   - `stale_workspace_names`: `commands::post_op_stale` (WC-behind
//     report), TUI selection staleness.
//   - `update_workspace_stale`: post-op staleness resolution in
//     sync/close/transfer + `resolve_predicted_stale` + TUI stale actions.

// ---------------------------------------------------------------------------
// Core helpers — all jj commands go through here
// ---------------------------------------------------------------------------

/// Build a `Command` targeting a repo via `--repository`. Includes `--ignore-working-copy`.
/// Sets `current_dir(repo)` so that any paths in jj output are repo-root-relative.
fn jj_cmd(repo: &Path) -> Command {
    let mut cmd = Command::new("jj");
    cmd.current_dir(repo)
        .arg("--repository")
        .arg(repo)
        .arg("--ignore-working-copy");
    cmd
}

/// Build a `Command` targeting a repo via `--repository` without `--ignore-working-copy`.
/// Use only for commands that must read/write the working copy (Live policy).
fn jj_cmd_wc(repo: &Path) -> Command {
    let mut cmd = Command::new("jj");
    cmd.current_dir(repo).arg("--repository").arg(repo);
    cmd
}

/// Build a `Command` targeting a workspace via `current_dir`. Includes `--ignore-working-copy`.
pub(crate) fn jj_cmd_ws(ws_path: &Path) -> Command {
    let mut cmd = Command::new("jj");
    cmd.current_dir(ws_path).arg("--ignore-working-copy");
    cmd
}

/// Build a `Command` targeting a workspace via `current_dir` without `--ignore-working-copy`.
/// Use only for commands that must read/write the working copy (Live policy).
fn jj_cmd_ws_wc(ws_path: &Path) -> Command {
    let mut cmd = Command::new("jj");
    cmd.current_dir(ws_path);
    cmd
}

/// Build a `Command` for early-bootstrap calls (e.g. `workspace_root_by_name`)
/// that run before we know which repo we are operating on. Includes
/// `--ignore-working-copy`. Routes through `run_jj_raw` (no version check)
/// so shell-integration commands that don't need jj can still call it.
fn jj_cmd_bootstrap() -> Command {
    let mut cmd = Command::new("jj");
    cmd.arg("--ignore-working-copy");
    cmd
}

/// Run a jj command with `--ignore-working-copy` (default policy).
pub(crate) fn run_jj(repo: &Path, args: &[&str]) -> Result<String> {
    run_jj_inner(jj_cmd(repo), args)
}

/// Snapshot the working copy of a specific workspace (captures pending edits).
///
/// `jj util snapshot` is conditional (verified on jj 0.42): a clean working
/// copy — including mtime-only changes — produces "No snapshot needed." and
/// leaves the op head untouched; only a content change creates an operation.
///
/// Fails on a jj-stale working copy. Callers rely on this both as a safety
/// property (`workspace_forget`) and as a classifier (the execution-time
/// protection phase skips workspaces whose snapshot failed *because* they
/// are stale — see `commands::prepare_execution_freshness`).
pub fn snapshot_ws(ws_path: &Path) -> Result<()> {
    run_jj_inner(jj_cmd_ws_wc(ws_path), &["util", "snapshot"])?;
    Ok(())
}

/// Probe whether a workspace's working copy has changes not yet snapshotted
/// into `@`, without integrating any operation: op heads and the visible op
/// log are unchanged by the probe (the preview runs in a staged,
/// never-integrated operation — staged data is written to the op store but
/// never becomes a head).
///
/// Two queries:
///   A: `@`'s currently recorded commit id (stale view, no working-copy scan).
///   B: the would-be commit id after a snapshot, computed under
///      `--no-integrate-operation` (`--quiet` suppresses the staged-op hint).
/// Unsnapshotted changes exist iff A != B. Full commit ids are compared —
/// any content difference changes the id, unlike file-list comparisons.
///
/// Not to be confused with `is_workspace_stale`: that detects a working copy
/// *behind* the repo (needs `jj workspace update-stale`); this detects one
/// *ahead* of it (pending edits).
///
/// No production caller, by design: this is the non-mutating WC-ahead
/// primitive reserved for a future unsaved-edits indicator. It is not a
/// cheap pre-check for `snapshot_ws` — it scans the whole working copy and
/// writes staged (non-integrated) op-store data, so probe-then-snapshot is
/// pure overhead over a direct (already-conditional) `snapshot_ws`.
/// Exercised by the `probe_detects_*` integration tests.
pub fn has_unsnapshotted_changes(ws_path: &Path) -> Result<bool> {
    let recorded = run_jj_inner(
        jj_cmd_ws(ws_path),
        &[
            "log",
            "--no-graph",
            "--revision",
            "@",
            "--template",
            "commit_id",
        ],
    )
    .context("probe recorded @ commit id")?;
    let preview = run_jj_inner(
        jj_cmd_ws_wc(ws_path),
        &[
            "--no-integrate-operation",
            "--quiet",
            "log",
            "--no-graph",
            "--revision",
            "@",
            "--template",
            "commit_id",
        ],
    )
    .context("probe would-be @ commit id (preview snapshot)")?;
    Ok(recorded != preview)
}

/// Run a jj command in a workspace directory without `--ignore-working-copy`.
/// jj will auto-snapshot the workspace's working copy before executing.
pub(crate) fn run_jj_ws_live(ws_path: &Path, args: &[&str]) -> Result<String> {
    run_jj_inner(jj_cmd_ws_wc(ws_path), args)
}

/// Classify a spawn-time failure from `Command::output()`. Specifically
/// converts `io::ErrorKind::NotFound` (jj binary missing) into a friendly
/// install-pointing message.
fn classify_jj_spawn_failure(err: io::Error, args: &[&str]) -> anyhow::Error {
    if err.kind() == io::ErrorKind::NotFound {
        anyhow::anyhow!(
            "jj binary not found on PATH. Install jj from {JJ_INSTALL_URL}, then retry."
        )
    } else {
        anyhow::Error::new(err).context(format!("failed to run jj {}", args.join(" ")))
    }
}

/// Classify a non-zero exit from a jj subprocess. Detects the common
/// "not in a jj repo" stderr substring and points the user at `ji init`.
fn classify_jj_nonzero(args: &[&str], stderr: &[u8]) -> anyhow::Error {
    let stderr_str = String::from_utf8_lossy(stderr);
    if stderr_str.contains("There is no jj repo in") || stderr_str.contains("no jj repo") {
        anyhow::anyhow!(
            "not inside a jj repository. Run `ji init` to create one, or cd into an existing jj repo.\n(jj stderr: {})",
            stderr_str.trim()
        )
    } else {
        anyhow::anyhow!("jj {} failed: {}", args.join(" "), stderr_str)
    }
}

/// Run a jj subprocess without a version check. Used by:
/// - the bootstrap path (`workspace_root_by_name`) so shell-integration
///   commands like `ji config shell install` don't require a working jj.
/// - the version check itself (`jj_version()`), to avoid recursion.
///
/// **stdout encoding invariant:** parsed as UTF-8 because jj structured
/// templates guarantee it. Callers that need to handle commands which may
/// emit arbitrary filenames (e.g. non-UTF-8 OS paths on platforms that
/// allow them) must instead use `run_jj_output`, which returns the raw
/// `Output`, and parse stdout as `OsStr` / `&[u8]` directly.
fn run_jj_raw(mut cmd: Command, args: &[&str]) -> Result<String> {
    let start = Instant::now();
    let output = cmd
        .args(args)
        .output()
        .map_err(|e| classify_jj_spawn_failure(e, args))?;
    crate::subprocess_log::log_subprocess(&args.join(" "), start.elapsed());
    if !output.status.success() {
        return Err(classify_jj_nonzero(args, &output.stderr));
    }
    Ok(String::from_utf8(output.stdout)
        .context("invalid utf-8")?
        .trim()
        .to_string())
}

pub(crate) fn run_jj_inner(cmd: Command, args: &[&str]) -> Result<String> {
    check_min_jj_version_once()?;
    run_jj_raw(cmd, args)
}

/// Run a bootstrap jj command (`--ignore-working-copy`, no `-R`, no version
/// check) under a wall-clock timeout, for per-keystroke shell completion.
///
/// Snapshot policy: bucket 1 (reads recorded state) — `cmd` must come from
/// `jj_cmd_bootstrap()` so `--ignore-working-copy` is present.
///
/// Polls the child's exit against the deadline, so the prompt can't freeze even
/// if jj closes stdout but never exits — which waiting on `.output()` or on the
/// reader's EOF would allow. stderr is discarded; stdout drains on a reader
/// thread so a full pipe can't deadlock the child. On timeout the child is
/// killed, reaped, and the reader detached. Returns the trimmed stdout, or an
/// error (timeout, spawn failure, nonzero exit) the caller turns into an empty
/// candidate list. Call sites pass `--no-pager` so no pager grandchild lingers
/// on the stdout pipe.
fn run_jj_bootstrap_timeout(mut cmd: Command, args: &[&str], timeout: Duration) -> Result<String> {
    use std::io::Read as _;
    let start = Instant::now();
    let mut child = cmd
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| classify_jj_spawn_failure(e, args))?;

    let mut stdout = child.stdout.take().expect("stdout was piped");
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    // Poll for exit against the deadline; kill if it overruns.
    let deadline = start + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // reap (SIGKILL is uninterruptible)
                    drop(reader); // detach — never block on a grandchild-held pipe
                    anyhow::bail!("jj completion query timed out after {timeout:?}");
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(reader);
                return Err(anyhow::Error::new(e).context("waiting on jj completion query"));
            }
        }
    };
    // Exited in time. Take the drained stdout, but don't block forever if
    // something still holds the pipe open.
    let bytes = match rx.recv_timeout(Duration::from_secs(1)) {
        Ok(buf) => {
            let _ = reader.join(); // reader has sent, so join returns immediately
            buf
        }
        Err(_) => Vec::new(),
    };
    // Nonzero exit = a failed query (e.g. not in a repo); don't parse the
    // partial output it left behind.
    if !status.success() {
        anyhow::bail!("jj completion query exited unsuccessfully ({status})");
    }
    crate::subprocess_log::log_subprocess(&args.join(" "), start.elapsed());
    Ok(String::from_utf8(bytes)
        .context("invalid utf-8")?
        .trim()
        .to_string())
}

/// Run a `Command`, log the invocation if logging is enabled, and return the
/// raw `Output`. For call sites that bypass `run_jj_inner`.
///
/// Performs the same one-shot version check as `run_jj_inner`, but reports
/// a failed version check as an `io::Error` so the existing `io::Result`
/// signature is preserved.
pub(crate) fn run_jj_output(cmd: &mut Command, label: &str) -> io::Result<std::process::Output> {
    if let Err(e) = check_min_jj_version_once() {
        return Err(io::Error::other(e.to_string()));
    }
    let start = Instant::now();
    let result = cmd.output().map_err(|e| {
        let kind = e.kind();
        let friendly = classify_jj_spawn_failure(e, &[label]).to_string();
        io::Error::new(kind, friendly)
    });
    crate::subprocess_log::log_subprocess(label, start.elapsed());
    result
}

// ---------------------------------------------------------------------------
// jj version detection
// ---------------------------------------------------------------------------

static JJ_VERSION: OnceLock<Result<semver::Version, String>> = OnceLock::new();
static VERSION_CHECK_DONE: OnceLock<Result<(), String>> = OnceLock::new();

/// Parse `jj --version` output (format: "jj 0.41.0").
fn parse_jj_version_output(stdout: &str) -> Result<semver::Version> {
    let version_str = stdout
        .trim()
        .strip_prefix("jj ")
        .with_context(|| format!("unexpected `jj --version` output: {stdout:?}"))?;
    semver::Version::parse(version_str)
        .with_context(|| format!("could not parse jj version: {version_str:?}"))
}

/// Cached `jj --version` result. Skips the version check itself to avoid
/// recursion. Failure is cached as a String (Version is not Clone-friendly
/// across an Err alternative; storing String makes this OnceLock-safe).
fn jj_version() -> Result<semver::Version> {
    JJ_VERSION
        .get_or_init(|| {
            run_jj_raw(Command::new("jj"), &["--version"])
                .and_then(|out| parse_jj_version_output(&out))
                .map_err(|e| format!("{e:#}"))
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

/// One-shot check that the installed jj is >= `MIN_JJ_VERSION`. Cached so
/// it only fires once per process. Returns Ok on every subsequent call.
fn check_min_jj_version_once() -> Result<()> {
    VERSION_CHECK_DONE
        .get_or_init(|| {
            let installed = jj_version().map_err(|e| format!("{e:#}"))?;
            let min = semver::Version::parse(MIN_JJ_VERSION)
                .expect("MIN_JJ_VERSION is a valid semver constant");
            if installed < min {
                Err(format!(
                    "installed jj {installed} is older than required {MIN_JJ_VERSION}. Upgrade jj — see {JJ_INSTALL_URL}"
                ))
            } else {
                Ok(())
            }
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

// ---------------------------------------------------------------------------
// Graph / log
// ---------------------------------------------------------------------------

/// Build a Pass 2 template that emits `change_id` on every line,
/// matching the line count of the user's display template.
///
/// Line count is determined by counting `"\n"` occurrences in the template
/// string. Templates with conditional newlines may mismatch — an acceptable
/// limitation.
fn build_pass2_template(display_template: &str) -> String {
    let line_count = display_template.matches(r#""\n""#).count().max(1);
    std::iter::repeat_n(r#"change_id"#, line_count)
        .collect::<Vec<_>>()
        .join(r#" ++ "\n" ++ "#)
}

/// Dual-pass graph fetch: colored ANSI graph + per-line change_id map.
///
/// `extra_args` are prepended to both passes (e.g. `--at-operation=<id>`).
fn log_graph_with_heads_inner(
    repo: &Path,
    log_template: Option<&str>,
    extra_args: &[&str],
) -> Result<(String, Vec<Option<String>>)> {
    // Pass 1: colored graph
    let mut pass1 = jj_cmd(repo);
    pass1.args(extra_args);
    pass1.args(["log", "--color", "always"]);
    if let Some(tmpl) = log_template {
        pass1.args(["--template", tmpl]);
    }
    let graph_out =
        run_jj_output(&mut pass1, "log --color always").context("failed to run jj log (graph)")?;
    if !graph_out.status.success() {
        anyhow::bail!(
            "jj log failed: {}",
            String::from_utf8_lossy(&graph_out.stderr)
        );
    }
    let graph = String::from_utf8(graph_out.stdout).context("invalid utf-8")?;

    // Pass 2: structured change_id per line, matching the line count of Pass 1.
    let pass2_template = match log_template {
        Some(tmpl) => build_pass2_template(tmpl),
        None => r#"change_id ++ "\n" ++ change_id"#.to_string(),
    };
    let mut pass2 = jj_cmd(repo);
    pass2.args(extra_args);
    pass2.args(["log", "--color", "never", "--template", &pass2_template]);
    let heads_out = run_jj_output(&mut pass2, "log --color never (heads pass)")
        .context("failed to run jj log (heads)")?;
    let heads_str = String::from_utf8(heads_out.stdout).context("invalid utf-8")?;

    // Parse: each line has graph characters followed by the change_id.
    let line_heads: Vec<Option<String>> = heads_str
        .lines()
        .map(|line| {
            line.split_whitespace()
                .last()
                .filter(|s| s.len() == 32 && s.chars().all(|c| c.is_ascii_lowercase()))
                .map(|s| s.to_string())
        })
        .collect();

    Ok((graph, line_heads))
}

/// Fetch `jj log` with ANSI colors and a per-line change_id map.
pub fn log_graph_with_heads(
    repo: &Path,
    log_template: Option<&str>,
) -> Result<(String, Vec<Option<String>>)> {
    log_graph_with_heads_inner(repo, log_template, &[])
}

/// Fetch `jj log` at a specific operation, with ANSI colors and a per-line change_id map.
pub fn log_graph_at_operation(
    repo: &Path,
    op_id: &str,
    log_template: Option<&str>,
) -> Result<(String, Vec<Option<String>>)> {
    let at_op = format!("--at-operation={op_id}");
    log_graph_with_heads_inner(repo, log_template, &[&at_op])
}

/// Get a deterministic key representing the current jj operation head(s).
/// With divergent op heads (multiple files), all names are sorted and joined
/// so that successive polls produce the same result regardless of readdir order.
pub fn current_op_head(repo_root: &Path) -> Option<String> {
    let heads_dir = repo_root.join(".jj/repo/op_heads/heads");
    let mut names: Vec<String> = std::fs::read_dir(&heads_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    if names.is_empty() {
        return None;
    }
    names.sort_unstable();
    Some(names.join(","))
}

/// Get the current operation ID as a single hash string.
/// Returns `None` if there are zero or multiple (divergent) op heads.
pub fn current_op_id(repo_root: &Path) -> Option<String> {
    let heads_dir = repo_root.join(".jj/repo/op_heads/heads");
    let mut entries = std::fs::read_dir(&heads_dir).ok()?;
    let first = entries.next()?.ok()?;
    // If there's a second entry, we have divergent heads — refuse.
    if entries.next().is_some() {
        return None;
    }
    Some(first.file_name().to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------
// Operation log
// ---------------------------------------------------------------------------

/// Fetch the operation log, returning parsed `Operation` structs.
///
/// Layout: id \x1f desc \x1f snapshot? \x1f current? \x1f tags \x1f timestamp \x1e
pub fn op_log(repo: &Path, limit: usize) -> Result<Vec<Operation>> {
    let template = concat!(
        r#"id ++ "\x1f" ++ description.first_line() ++ "\x1f""#,
        r#" ++ if(self.snapshot(), "true", "false") ++ "\x1f""#,
        r#" ++ if(self.current_operation(), "true", "false") ++ "\x1f""#,
        r#" ++ tags ++ "\x1f" ++ self.time().start() ++ "\x1e""#,
    );
    let limit_str = limit.to_string();
    let output = run_jj(
        repo,
        &[
            "--at-op=@",
            "op",
            "log",
            "--no-graph",
            "--limit",
            &limit_str,
            "--template",
            template,
        ],
    )?;
    let mut ops = Vec::new();
    for record in records(&output) {
        let fields: Vec<&str> = record.split(DELIM_FIELD).collect();
        if fields.len() < 6 {
            continue;
        }
        ops.push(Operation {
            id: fields[0].to_string(),
            description: fields[1].to_string(),
            is_snapshot: fields[2] == "true",
            is_current: fields[3] == "true",
            tags: fields[4].to_string(),
            timestamp: fields[5].to_string(),
        });
    }
    Ok(ops)
}

/// Fetch the op-show output for a specific operation (changed commits, bookmarks, etc.).
pub fn op_show(repo: &Path, op_id: &str) -> Result<String> {
    run_jj(repo, &["--at-op=@", "op", "show", op_id])
}

/// Restore the repository to a specific operation.
pub fn op_restore(repo: &Path, op_id: &str) -> Result<String> {
    run_jj_inner(jj_cmd_wc(repo), &["op", "restore", op_id])
}

/// Check if all operations between `expected_op_id` and the current head are snapshots.
///
/// Returns `Ok(true)` if the heads match or only snapshots intervene.
/// Returns `Ok(false)` if any non-snapshot operation exists between them.
/// If `expected_op_id` is not found within `limit` operations, assumes
/// non-trivial work (fail-closed). Pick `limit` generously for callers that
/// themselves create many snapshot ops before checking (the execution-time
/// protection phase snapshots every workspace).
pub fn only_snapshots_since(repo: &Path, expected_op_id: &str, limit: usize) -> Result<bool> {
    // Layout: id \x1f snapshot? \x1e
    let template = r#"id ++ "\x1f" ++ if(self.snapshot(), "true", "false") ++ "\x1e""#;
    let limit_str = limit.to_string();
    let output = run_jj(
        repo,
        &[
            "--at-op=@",
            "op",
            "log",
            "--no-graph",
            "--limit",
            &limit_str,
            "--template",
            template,
        ],
    )?;
    for record in records(&output) {
        let Some((id, is_snap)) = record.split_once(DELIM_FIELD) else {
            continue;
        };
        if id == expected_op_id {
            // Reached the expected head — everything between was snapshots.
            return Ok(true);
        }
        if is_snap != "true" {
            return Ok(false);
        }
    }
    // expected_op_id not found within the limit — assume non-trivial work.
    Ok(false)
}

// ---------------------------------------------------------------------------
// Workspace queries
// ---------------------------------------------------------------------------

/// Returns the root of the **default** workspace.
/// This is the one bootstrap function that doesn't take a repo path.
pub fn workspace_root() -> Result<PathBuf> {
    workspace_root_by_name(Some("default"))
}

/// Returns the root of the current workspace (whichever workspace the user is in).
pub fn current_workspace_root() -> Result<PathBuf> {
    workspace_root_by_name(None)
}

fn workspace_root_by_name(name: Option<&str>) -> Result<PathBuf> {
    // Bootstrap path: this runs at process startup (before we know whether
    // the caller's command needs a jj repo) and from main.rs's logger-init
    // probe. Use `run_jj_raw` to skip the version check — shell-integration
    // commands like `ji config shell install` must succeed without a working
    // jj binary.
    let mut args: Vec<&str> = vec!["workspace", "root"];
    if let Some(n) = name {
        args.push("--name");
        args.push(n);
    }
    let stdout = run_jj_raw(jj_cmd_bootstrap(), &args)?;
    Ok(PathBuf::from(stdout))
}

// Layout (DELIM_FIELD between fields, DELIM_RECORD between records):
//   name \x1f root \x1f target.change_id \x1f target.description \x1e
const LIST_TEMPLATE: &str = concat!(
    r#"name ++ "\x1f""#,
    r#" ++ self.root() ++ "\x1f""#,
    r#" ++ self.target().change_id() ++ "\x1f""#,
    r#" ++ self.target().description()"#,
    r#" ++ "\x1e""#,
);

/// Unquote a workspace name from `jj workspace list` output: jj wraps names
/// containing spaces in double quotes and escapes inner quotes/backslashes.
pub(crate) fn unquote_ws_name(raw: &str) -> String {
    if raw.len() > 1 && raw.starts_with('"') && raw.ends_with('"') {
        raw[1..raw.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        raw.to_string()
    }
}

/// Parse a `self.root()` field: jj emits `<Error: …>` for a deleted workspace
/// directory; map that (and only that) to `None`, otherwise the path.
pub(crate) fn parse_ws_root(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.contains("<Error") {
        None
    } else {
        Some(PathBuf::from(raw))
    }
}

/// Lightweight workspace record from a single `jj workspace list` call.
///
/// Used where only identity-level data is needed (the execution-time
/// freshness phase, path/change-id revalidation) — `list_workspaces` costs
/// several extra jj calls per workspace for revisions/bookmarks/timestamps.
#[derive(Clone)]
pub struct WorkspaceEntry {
    pub name: String,
    /// Empty when jj reports the root as an error (e.g. deleted directory).
    pub path: PathBuf,
    /// Change id of the workspace's working-copy commit (`<name>@`).
    pub change_id: String,
    pub description: String,
}

/// List workspaces with one jj call (no per-workspace enrichment).
pub fn list_workspace_entries(repo: &Path) -> Result<Vec<WorkspaceEntry>> {
    let stdout = run_jj(repo, &["workspace", "list", "--template", LIST_TEMPLATE])?;
    let mut entries = Vec::new();
    for record in records(&stdout) {
        let fields: Vec<&str> = record.splitn(4, DELIM_FIELD).collect();
        if fields.len() < 4 {
            anyhow::bail!("unexpected workspace list format: {record}");
        }
        // jj quotes names containing spaces and emits a `<Error…>` sentinel
        // for a deleted workspace root; shared helpers handle both.
        let name = unquote_ws_name(fields[0]);
        let path = parse_ws_root(fields[1]).unwrap_or_default();
        entries.push(WorkspaceEntry {
            name,
            path,
            change_id: fields[2].to_string(),
            description: fields[3].to_string(),
        });
    }
    Ok(entries)
}

pub fn list_workspaces(repo: &Path) -> Result<Vec<Workspace>> {
    let entries = list_workspace_entries(repo)?;
    let default_change_id = entries
        .iter()
        .find(|e| e.name == "default")
        .map(|e| e.change_id.clone())
        .unwrap_or_default();
    let mut workspaces: Vec<Workspace> = entries
        .into_iter()
        .map(|e| Workspace {
            name: e.name,
            path: e.path,
            change_id: e.change_id,
            description: e.description,
            bookmarks_at_head: Vec::new(),
            bookmarks_behind: Vec::new(),
            is_current: false,
            revisions: Vec::new(),
            last_modified: None,
        })
        .collect();

    // Pre-fetch revisions and bookmarks unique to each non-default workspace
    // (combined into a single jj call per workspace).
    if !default_change_id.is_empty() {
        for ws in &mut workspaces {
            if ws.name == "default" {
                continue;
            }
            let (revs, bms) =
                workspace_revisions_with_bookmarks(repo, &ws.change_id, &default_change_id)
                    .unwrap_or_default();
            ws.revisions = revs;

            for (bm_name, bm_id) in bms {
                ws.classify_bookmark(bm_name, bm_id);
            }
        }
    }

    // Populate revisions for the default workspace (all ancestors).
    if !default_change_id.is_empty()
        && let Ok(revs) = default_workspace_revisions(repo, &default_change_id)
        && let Some(default_ws) = workspaces.iter_mut().find(|ws| ws.name == "default")
    {
        default_ws.revisions = revs;

        for (bm_name, bm_id) in
            workspace_bookmarks(repo, &default_change_id, &default_change_id).unwrap_or_default()
        {
            default_ws.classify_bookmark(bm_name, bm_id);
        }
    }

    // Fetch last-modified timestamps for all workspaces
    for ws in &mut workspaces {
        ws.last_modified = last_nonempty_timestamp(repo, &ws.change_id);
    }

    Ok(workspaces)
}

// ---------------------------------------------------------------------------
// Shell-completion readers (per-keystroke; one non-mutating bootstrap call each)
// ---------------------------------------------------------------------------

/// Wall-clock ceiling for a single completion query: a safety net for a wedged
/// jj, not the expected latency (normally well under 100 ms).
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(9);

/// Template: `name \x1f committer-epoch \x1f root \x1e`. Root goes last because
/// `self.root()` is an unescaped filesystem path that may contain our `\x1f`
/// delimiter; as the final field it survives `splitn` whole. The name is
/// jj-escaped and the epoch is digits, so neither holds a delimiter.
const COMPLETION_WS_TEMPLATE: &str = concat!(
    r#"name ++ "\x1f""#,
    r#" ++ self.target().committer().timestamp().format("%s") ++ "\x1f""#,
    r#" ++ self.root()"#,
    r#" ++ "\x1e""#,
);

/// A workspace as needed for shell completion.
pub struct CompletionWorkspace {
    pub name: String,
    /// `None` when jj reports the `<Error…>` sentinel (deleted dir).
    pub root: Option<PathBuf>,
    /// `@`'s committer timestamp (epoch seconds) — "last touched". Not
    /// `last_nonempty_timestamp`, which would cost a jj call per workspace; this
    /// rides along in the single `workspace list` call.
    pub last_touched: Option<i64>,
}

/// List workspaces for shell completion in one non-mutating jj subprocess from
/// cwd (no version check, no `-R` — jj resolves the repo from cwd) under a
/// bounded timeout. Any error (not in a repo, timeout, …) is returned for the
/// caller to turn into an empty candidate list.
pub fn completion_workspaces() -> Result<Vec<CompletionWorkspace>> {
    let stdout = run_jj_bootstrap_timeout(
        jj_cmd_bootstrap(),
        &[
            "--no-pager",
            "workspace",
            "list",
            "--template",
            COMPLETION_WS_TEMPLATE,
        ],
        COMPLETION_TIMEOUT,
    )?;
    Ok(parse_completion_workspaces(&stdout))
}

/// Parse `COMPLETION_WS_TEMPLATE` records. `splitn(3)` is safe because root —
/// the only field that can hold a delimiter — comes last (see the template).
fn parse_completion_workspaces(stdout: &str) -> Vec<CompletionWorkspace> {
    let mut out = Vec::new();
    for record in records(stdout) {
        let mut fields = record.splitn(3, DELIM_FIELD);
        let (Some(name), Some(ts), Some(root)) = (fields.next(), fields.next(), fields.next())
        else {
            continue; // fewer than 3 fields → malformed → skip (fail-soft)
        };
        out.push(CompletionWorkspace {
            name: unquote_ws_name(name),
            root: parse_ws_root(root),
            last_touched: ts.trim().parse::<i64>().ok(),
        });
    }
    out
}

/// Local bookmark names with their commit's committer timestamp, for `ji new`
/// completion — one non-mutating bootstrap subprocess under a bounded timeout.
/// Local only: `local_bookmarks` is a `jj log` template keyword (not valid in
/// `jj bookmark list --template`), and a commit matched by `bookmarks()` that
/// carries only a remote bookmark yields an empty field, skipped below.
pub fn completion_local_bookmarks() -> Result<Vec<(String, Option<i64>)>> {
    // Epoch (digits) first; the name list goes last so `splitn` keeps it whole
    // if a name ever holds a `\x1f` (defensive — they don't in practice).
    const TEMPLATE: &str = concat!(
        r#"committer.timestamp().format("%s") ++ "\x1f""#,
        r#" ++ local_bookmarks.map(|b| b.name()).join("\x01")"#,
        r#" ++ "\x1e""#,
    );
    let stdout = run_jj_bootstrap_timeout(
        jj_cmd_bootstrap(),
        &[
            "--no-pager",
            "log",
            "--no-graph",
            "--revision",
            "bookmarks()",
            "--template",
            TEMPLATE,
        ],
        COMPLETION_TIMEOUT,
    )?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for record in records(&stdout) {
        let mut fields = record.splitn(2, DELIM_FIELD);
        let ts = fields.next().and_then(|t| t.trim().parse::<i64>().ok());
        let names = fields.next().unwrap_or("");
        for name in names.split(DELIM_LIST).filter(|n| !n.is_empty()) {
            if seen.insert(name.to_string()) {
                out.push((name.to_string(), ts));
            }
        }
    }
    Ok(out)
}

/// Topologically order workspaces, children before parents (descendant-first).
///
/// Returns indices into `entries`. one `jj log` over all workspace `@`s
/// (jj log's default order is reverse-topological), mapped back to entries
/// by change id. Entries whose change id doesn't appear (e.g. raced away)
/// keep their relative order at the end.
///
/// The execution-time protection phase snapshots workspaces in this order:
/// snapshotting a dirty workspace amends its `@` and rebases descendants
/// (which stales them), so each descendant must be captured before any
/// ancestor's snapshot can rebase it.
pub fn descendant_first_workspaces(repo: &Path, entries: &[WorkspaceEntry]) -> Result<Vec<usize>> {
    if entries.len() <= 1 {
        return Ok((0..entries.len()).collect());
    }
    let revset = entries
        .iter()
        .map(|e| format!(r#""{}"@"#, escape_revset_string(&e.name)))
        .collect::<Vec<_>>()
        .join(" | ");
    let stdout = run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            &revset,
            "--template",
            r#"change_id ++ "\n""#,
        ],
    )?;
    let rank: HashMap<&str, usize> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .enumerate()
        .map(|(i, id)| (id, i))
        .collect();
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by_key(|&i| {
        rank.get(entries[i].change_id.as_str())
            .copied()
            .unwrap_or(usize::MAX)
    });
    Ok(order)
}

/// Get the committer timestamp (epoch seconds) of the most recent non-empty
/// revision in the ancestry of `change_id`. Returns `None` on failure or if
/// every ancestor is empty.
fn last_nonempty_timestamp(repo: &Path, change_id: &str) -> Option<i64> {
    let revset = format!("::{change_id} ~ empty()");
    run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--limit",
            "1",
            "--revision",
            &revset,
            "--template",
            r#"committer.timestamp().format("%s")"#,
        ],
    )
    .ok()
    .and_then(|s| s.trim().parse::<i64>().ok())
}

// ---------------------------------------------------------------------------
// Workspace mutations
// ---------------------------------------------------------------------------

pub fn create_workspace(repo: &Path, path: &Path, revision: &str, msg: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent directory {}", parent.display()))?;
    }
    // Live policy: workspace add must materialize new WC files.
    run_jj_inner(
        jj_cmd_wc(repo),
        &[
            "workspace",
            "add",
            "--revision",
            revision,
            "--message",
            msg,
            "--",
            &path.to_string_lossy(),
        ],
    )?;
    Ok(())
}

/// Escape a string for use inside a jj revset double-quoted string.
/// Handles `\` → `\\` and `"` → `\"`.
pub(crate) fn escape_revset_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn create_bookmark(repo: &Path, ws_name: &str, bookmark_name: &str) -> Result<()> {
    // Use the workspace name revset (e.g. "feature-alpha@") so that the
    // bookmark targets the new workspace's working copy, not default's.
    let revset = format!(r#""{}"@"#, escape_revset_string(ws_name));
    run_jj(
        repo,
        &[
            "bookmark",
            "create",
            "--revision",
            &revset,
            "--",
            bookmark_name,
        ],
    )?;
    Ok(())
}

/// Move a bookmark to point at a revision.
pub fn bookmark_set(repo: &Path, bookmark: &str, revision: &str) -> Result<()> {
    run_jj(
        repo,
        &["bookmark", "set", "--revision", revision, "--", bookmark],
    )?;
    Ok(())
}

/// Delete a bookmark.
pub fn bookmark_delete(repo: &Path, bookmark: &str) -> Result<()> {
    run_jj(repo, &["bookmark", "delete", "--", bookmark])?;
    Ok(())
}

/// List local bookmark names on a revision. Returns empty vec if none exist.
///
/// Bookmark names are list items within a single record — uses DELIM_LIST.
pub fn local_bookmarks_on(repo: &Path, revset: &str) -> Result<Vec<String>> {
    let output = run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            revset,
            "--template",
            r#"local_bookmarks.map(|b| b.name()).join("\x01")"#,
        ],
    )?;
    if output.is_empty() {
        return Ok(Vec::new());
    }
    Ok(output.split(DELIM_LIST).map(String::from).collect())
}

/// Create a new bookmark pointing at an arbitrary revision.
///
/// Uses `bookmark create` (not `set`) so that duplicate names produce an error
/// rather than silently moving an existing bookmark.
pub fn create_bookmark_at(repo: &Path, bookmark_name: &str, revision: &str) -> Result<()> {
    run_jj(
        repo,
        &[
            "bookmark",
            "create",
            "--revision",
            revision,
            "--",
            bookmark_name,
        ],
    )?;
    Ok(())
}

/// Resolve a bookmark to its change_id (8-char short form). Returns None if
/// the bookmark doesn't exist.
pub fn bookmark_change_id(repo: &Path, bookmark: &str) -> Option<String> {
    let escaped = escape_revset_string(bookmark);
    let revset = format!("latest(bookmarks(exact:\"{escaped}\"))");
    run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            &revset,
            "--template",
            "change_id",
        ],
    )
    .ok()
    .filter(|s| !s.is_empty())
}

/// Resolve a bookmark name that may have been sanitized during workspace path
/// creation ('/' → '-'). Returns `(actual_name, change_id)`.
///
/// Tries an exact match first, then falls back to a glob where each '-' could
/// also be '/'. Among glob results, picks the bookmark whose sanitized form
/// (`/` → `-`) matches the derived name.
pub fn resolve_sanitized_bookmark(repo: &Path, derived: &str) -> Option<(String, String)> {
    // Exact match — covers bookmarks without slashes.
    if let Some(id) = bookmark_change_id(repo, derived) {
        return Some((derived.to_string(), id));
    }

    // No hyphens → slashes couldn't have been sanitized away.
    if !derived.contains('-') {
        return None;
    }

    // Build glob: each '-' could be the original '-' or a sanitized '/'.
    let glob_pat = derived.replace('-', "[-/]");
    let escaped = escape_revset_string(&glob_pat);
    let revset = format!("latest(bookmarks(glob:\"{escaped}\"))");
    // Layout: each list element is `name \x1f change_id`, joined by DELIM_LIST.
    let template = r#"local_bookmarks.map(|b| b.name() ++ "\x1f" ++ change_id).join("\x01")"#;
    let output = run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            &revset,
            "--template",
            template,
        ],
    )
    .ok()?;

    // Among returned bookmarks, find the one whose sanitized form matches.
    for entry in output.split(DELIM_LIST) {
        if let Some((name, id)) = entry.split_once(DELIM_FIELD)
            && name.replace('/', "-") == derived
        {
            return Some((name.to_string(), id.to_string()));
        }
    }
    None
}

/// The kind of change to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// A single file change from `jj diff --summary`.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub kind: FileChangeKind,
    pub path: String,
}

/// Parsed result of `jj diff --summary` + `jj diff --stat`.
#[derive(Debug, Clone)]
pub struct DiffSummary {
    pub files: Vec<FileChange>,
    /// The summary line from `--stat`, e.g. "4 files changed, 803 insertions(+), 222 deletions(-)".
    pub stat_line: Option<String>,
}

/// Parse a single `jj diff --summary` line into a `FileChange`.
fn parse_summary_line(line: &str) -> Option<FileChange> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let (kind, rest) = if let Some(rest) = line.strip_prefix("R ") {
        (FileChangeKind::Renamed, rest)
    } else if let Some(rest) = line.strip_prefix("C ") {
        // jj shows copies as C {old => new} — treat as added
        (FileChangeKind::Added, rest)
    } else if let Some(rest) = line.strip_prefix("A ") {
        (FileChangeKind::Added, rest)
    } else if let Some(rest) = line.strip_prefix("M ") {
        (FileChangeKind::Modified, rest)
    } else if let Some(rest) = line.strip_prefix("D ") {
        (FileChangeKind::Deleted, rest)
    } else {
        return None;
    };
    Some(FileChange {
        kind,
        path: rest.to_string(),
    })
}

/// Get the diff summary and stat for a revision.
pub fn revision_diff_summary(repo: &Path, change_id: &str) -> Result<DiffSummary> {
    let summary_out = run_jj(repo, &["diff", "--revision", change_id, "--summary"])?;
    let files: Vec<FileChange> = summary_out.lines().filter_map(parse_summary_line).collect();

    let stat_line = run_jj(repo, &["diff", "--revision", change_id, "--stat"])
        .ok()
        .and_then(|out| {
            out.lines()
                .last()
                .filter(|l| l.contains("changed"))
                .map(|l| l.trim().to_string())
        });

    Ok(DiffSummary { files, stat_line })
}

/// Parse `change_id \x1f description \x1e` records into `RevisionInfo` structs.
fn parse_revision_records(stdout: &str) -> Vec<RevisionInfo> {
    records(stdout)
        .filter_map(|record| {
            let (id, desc) = record.split_once(DELIM_FIELD)?;
            Some(RevisionInfo {
                change_id: id.to_string(),
                description: desc.trim_end().to_string(),
            })
        })
        .collect()
}

/// Bookmarks as `(name, change_id)` pairs.
type BookmarkList = Vec<(String, String)>;

/// List revisions unique to a workspace (not shared with default), newest first.
/// Also returns bookmarks found on those revisions as `(bookmark_name, change_id)`.
fn workspace_revisions_with_bookmarks(
    repo: &Path,
    ws_change_id: &str,
    default_change_id: &str,
) -> Result<(Vec<RevisionInfo>, BookmarkList)> {
    let revset = format!("{default_change_id}..{ws_change_id}");
    // Layout: change_id \x1f bookmarks (\x01-list) \x1f description \x1e
    let template = r#"change_id ++ "\x1f" ++ local_bookmarks.map(|b| b.name()).join("\x01") ++ "\x1f" ++ description ++ "\x1e""#;
    let stdout = run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            &revset,
            "--template",
            template,
        ],
    )?;

    let mut revisions = Vec::new();
    let mut bookmarks = Vec::new();

    for record in records(&stdout) {
        let mut fields = record.splitn(3, DELIM_FIELD);
        let Some(id) = fields.next() else { continue };
        let bm_field = fields.next().unwrap_or("");
        let desc = fields.next().unwrap_or("").trim_end();

        revisions.push(RevisionInfo {
            change_id: id.to_string(),
            description: desc.to_string(),
        });

        if !bm_field.is_empty() {
            for bm_name in bm_field.split(DELIM_LIST) {
                if !bm_name.is_empty() {
                    bookmarks.push((bm_name.to_string(), id.to_string()));
                }
            }
        }
    }

    Ok((revisions, bookmarks))
}

/// All ancestors of the default workspace head. Newest first.
fn default_workspace_revisions(repo: &Path, default_change_id: &str) -> Result<Vec<RevisionInfo>> {
    let revset = format!("::{default_change_id}");
    let template = r#"change_id ++ "\x1f" ++ description ++ "\x1e""#;
    let stdout = run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            &revset,
            "--template",
            template,
        ],
    )?;
    Ok(parse_revision_records(&stdout))
}

/// Return full change_ids for all ancestors of `change_id` (inclusive).
pub fn ancestor_ids(repo: &Path, change_id: &str) -> Result<Vec<String>> {
    let revset = format!("::{change_id}");
    let stdout = run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            &revset,
            "--template",
            r#"change_id ++ "\n""#,
        ],
    )?;
    Ok(stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// Find bookmarks in a workspace's unique revision chain, tagged with their change_id.
///
/// For the default workspace (ws == default), searches the entire ancestry.
/// For other workspaces, searches only revisions unique to the workspace.
pub fn workspace_bookmarks(
    repo: &Path,
    ws_change_id: &str,
    default_change_id: &str,
) -> Result<Vec<(String, String)>> {
    let revset = if ws_change_id == default_change_id {
        format!("::{ws_change_id}")
    } else {
        format!("{default_change_id}..{ws_change_id}")
    };
    // Layout per revision: list of `name \x1f change_id` joined by DELIM_LIST,
    // then DELIM_RECORD to terminate the revision's record.
    let template =
        r#"local_bookmarks.map(|b| b.name() ++ "\x1f" ++ change_id).join("\x01") ++ "\x1e""#;
    let stdout = run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            &revset,
            "--template",
            template,
        ],
    )?;
    let bookmarks = stdout
        .split(DELIM_RECORD)
        .flat_map(|rec| rec.split(DELIM_LIST))
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| entry.split_once(DELIM_FIELD))
        .map(|(name, id)| (name.to_string(), id.to_string()))
        .collect();
    Ok(bookmarks)
}

/// Forget a workspace. Snapshots WC first to capture pending edits.
///
/// Caller must resolve staleness before calling (snapshot fails on stale WC).
pub fn workspace_forget(repo: &Path, ws_path: &Path, name: &str) -> Result<()> {
    snapshot_ws(ws_path)?;
    run_jj(repo, &["workspace", "forget", "--", name])?;
    Ok(())
}

/// Forget an orphaned workspace (no directory on disk). No snapshot needed.
pub fn workspace_forget_orphaned(repo: &Path, name: &str) -> Result<()> {
    run_jj(repo, &["workspace", "forget", "--", name])?;
    Ok(())
}

/// Silently override the author of `@` in a workspace directory.
pub(crate) fn metaedit_author_in_workspace(ws_path: &Path, author: &str) -> Result<()> {
    run_jj_inner(jj_cmd_ws(ws_path), &["metaedit", "--author", author])?;
    Ok(())
}

/// Create a merge commit (multiple parents) in a specific workspace.
/// Uses `current_dir(ws_path)` so the workspace's `@` is updated.
/// Returns the change-id of the newly created merge revision.
///
/// Runs without `--ignore-working-copy` so jj auto-snapshots pending edits
/// into the current `@` before creating the merge. This is safe because
/// `new_merge` is always the first `@`-mutation of its workspace.
pub fn new_merge(
    ws_path: &Path,
    parents: &[&str],
    msg: &str,
    author: Option<&str>,
) -> Result<String> {
    if !ws_path.exists() {
        anyhow::bail!("merge skipped — directory missing: {}", ws_path.display());
    }
    let mut args: Vec<&str> = vec!["new", "--message", msg, "--"];
    args.extend(parents);
    run_jj_inner(jj_cmd_ws_wc(ws_path), &args)?;
    if let Some(a) = author {
        metaedit_author_in_workspace(ws_path, a)?;
    }

    run_jj_inner(
        jj_cmd_ws(ws_path),
        &[
            "log",
            "--no-graph",
            "--revision",
            "@",
            "--template",
            "change_id",
        ],
    )
}

/// Create a new commit on a specific revision in a specific workspace.
/// Uses `current_dir(ws_path)` so the workspace's `@` is updated.
pub fn new_on_in_workspace(
    ws_path: &Path,
    revision: &str,
    msg: &str,
    author: Option<&str>,
) -> Result<()> {
    if !ws_path.exists() {
        anyhow::bail!("new skipped — directory missing: {}", ws_path.display());
    }
    run_jj_inner(
        jj_cmd_ws(ws_path),
        &["new", "--message", msg, "--", revision],
    )?;
    if let Some(a) = author {
        metaedit_author_in_workspace(ws_path, a)?;
    }
    Ok(())
}

/// Rebase `source_revset` and its descendants onto `onto`.
///
/// Runs `jj rebase --source <revset> --onto <onto>`.
pub fn rebase_source(repo: &Path, source_revset: &str, onto: &str) -> Result<String> {
    run_jj(repo, &["rebase", "--source", source_revset, "--onto", onto])
}

/// Squash revisions from `from_revset` into `into_rev`.
///
/// Runs `jj squash --from <revset> --into <rev> --message <msg>`.
pub fn squash_into(
    repo: &Path,
    from_revset: &str,
    into_rev: &str,
    message: &str,
) -> Result<String> {
    run_jj(
        repo,
        &[
            "squash",
            "--from",
            from_revset,
            "--into",
            into_rev,
            "--message",
            message,
        ],
    )
}

/// Fetch `(change_id, full_description)` pairs for a revset.
///
/// Returns newest-first (jj log default order). Uses `\x1e` record separators
/// (via DELIM_RECORD) to handle multi-line descriptions safely.
pub fn revision_descriptions(repo: &Path, revset: &str) -> Result<Vec<(String, String)>> {
    let template = TMPL_ID_DESC;
    let stdout = run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            revset,
            "--template",
            template,
        ],
    )?;
    let mut result = Vec::new();
    for record in records(&stdout) {
        if let Some((id, desc)) = record.split_once(DELIM_FIELD) {
            result.push((id.to_string(), desc.trim_end().to_string()));
        }
    }
    Ok(result)
}

/// Find which candidate workspace's head is the closest ancestor of the source.
/// Returns the change_id of the closest ancestor, or None.
pub fn closest_ancestor_workspace(
    repo: &Path,
    source_change_id: &str,
    candidate_ids: &[&str],
) -> Option<String> {
    if candidate_ids.is_empty() {
        return None;
    }
    let candidates = candidate_ids.join(" | ");
    let revset = format!("({candidates}) & ::{source_change_id}");
    run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--limit",
            "1",
            "--revision",
            &revset,
            "--template",
            "change_id",
        ],
    )
    .ok()
    .filter(|s| !s.is_empty())
}

/// Run `jj split --revision <change_id>` interactively.
/// Takes over the terminal (diff editor). Must be called after TUI exit.
pub fn split_revision(repo: &Path, change_id: &str) -> Result<()> {
    // Live policy: interactive diff editor needs the working copy.
    let status = jj_cmd_wc(repo)
        .args(["split", "--revision", change_id])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context("failed to run jj split")?;
    if !status.success() {
        anyhow::bail!("jj split failed with {status}");
    }
    Ok(())
}

/// Abandon revisions. Enforces a safety cap.
pub fn abandon_revisions(repo: &Path, change_ids: &[&str]) -> Result<()> {
    if change_ids.is_empty() {
        return Ok(());
    }
    if change_ids.len() > MAX_DESTRUCTIVE_REVISIONS {
        anyhow::bail!(
            "refusing to abandon {} revisions (safety cap is {}). \
             This likely indicates a bug in revision selection.",
            change_ids.len(),
            MAX_DESTRUCTIVE_REVISIONS
        );
    }
    let mut args = vec!["abandon", "--"];
    args.extend(change_ids);
    run_jj(repo, &args)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Stale working copy detection
// ---------------------------------------------------------------------------

/// Check if an error indicates a stale working copy.
pub fn is_stale_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    msg.contains("The working copy is stale")
        || msg.contains("Could not read working copy's operation")
}

/// Check whether the working copy is stale by running a lightweight command
/// without `--ignore-working-copy` (Live policy: must probe live WC state).
///
/// (`--no-graph` is not a `jj status` flag — passing it made this exit 2 at
/// argument parsing, so the check silently always returned `false`. Same bug
/// as the sibling `is_workspace_stale`.)
pub fn is_working_copy_stale(repo: &Path) -> bool {
    let output = run_jj_output(
        jj_cmd_wc(repo).args(["status", "--no-pager", "--color", "never", "--quiet"]),
        "status (stale check)",
    );
    match output {
        Ok(out) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            stderr.contains("The working copy is stale")
                || stderr.contains("Could not read working copy's operation")
        }
        _ => false,
    }
}

/// Attempt to resolve a stale working copy (default workspace via --repository).
/// Live policy: purpose is to fix the WC.
pub fn update_stale(repo: &Path) -> Result<String> {
    run_jj_inner(jj_cmd_wc(repo), &["workspace", "update-stale"])
}

/// Check if a specific workspace's working copy is stale.
/// Live policy: must probe live WC state.
///
/// Load-bearing side effect: the live `jj status` snapshots the workspace's
/// pending working-copy edits. The TUI's `refresh()` runs this probe before
/// rebuilding its workspace list precisely so that list-derived dialog data
/// (revisions, bookmarks, change ids) reflects post-snapshot reality. Do not
/// add `--ignore-working-copy` here — a stale-check structurally cannot use
/// it, and the snapshot side effect is relied upon.
/// (`--no-graph` is not a `jj status` flag; passing it made the whole probe
/// exit 2 at argument parsing — no staleness detection, no snapshot.)
pub fn is_workspace_stale(ws_path: &Path) -> bool {
    if !ws_path.exists() {
        return false;
    }
    let output = run_jj_output(
        jj_cmd_ws_wc(ws_path).args(["status", "--no-pager", "--color", "never", "--quiet"]),
        "status (workspace stale check)",
    );
    match output {
        Ok(out) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            stderr.contains("The working copy is stale")
                || stderr.contains("Could not read working copy's operation")
        }
        _ => false,
    }
}

/// Fix a specific workspace's stale working copy.
/// Live policy: purpose is to fix the WC.
pub fn update_workspace_stale(ws_path: &Path) -> Result<String> {
    if !ws_path.exists() {
        anyhow::bail!("workspace directory missing: {}", ws_path.display());
    }
    run_jj_inner(jj_cmd_ws_wc(ws_path), &["workspace", "update-stale"])
}

/// Batch check: which workspaces have stale working copies?
pub fn stale_workspace_names(workspaces: &[(String, PathBuf)]) -> Vec<String> {
    workspaces
        .iter()
        .filter(|(_, path)| {
            !path.as_os_str().is_empty() && path.exists() && is_workspace_stale(path)
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Find workspace names whose `@` is a descendant of the given revset.
///
/// Uses the `working_copies` template to identify which revisions in
/// `descendants(revset)` are workspace working-copy commits. Returns
/// workspace names (without the `@` suffix).
pub fn workspaces_in_descendants(repo: &Path, revset: &str) -> Result<Vec<String>> {
    let full_revset = format!("({revset})::"); // descendants (inclusive)
    // Each record (one per revision) is the space-separated working_copies
    // string; DELIM_RECORD separates records, no fields within.
    let template = r#"if(working_copies, working_copies ++ "\x1e")"#;
    let output = run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            &full_revset,
            "--template",
            template,
        ],
    )?;
    let names: Vec<String> = output
        .split(DELIM_RECORD)
        .filter(|s| !s.is_empty())
        .flat_map(|s| s.split_whitespace())
        .map(|s| s.trim_end_matches('@').to_string())
        .collect();
    Ok(names)
}

// ---------------------------------------------------------------------------
// Stale workspace diff
// ---------------------------------------------------------------------------

/// Summary of differences between the actual files on disk and what jj expects
/// `@` to contain at the current operation.
#[derive(Debug, Default)]
pub struct StaleDiff {
    /// Files present on disk with different content than jj expects.
    pub modified: Vec<String>,
    /// Files present on disk but not tracked by jj at `@`.
    pub disk_only: Vec<String>,
    /// Files jj expects at `@` but missing from disk.
    pub jj_only: Vec<String>,
}

impl StaleDiff {
    /// One-line summary, e.g. "2 modified, 1 added on disk, 0 missing".
    pub fn summary(&self) -> String {
        format!(
            "{} modified, {} added on disk, {} missing from disk",
            self.modified.len(),
            self.disk_only.len(),
            self.jj_only.len(),
        )
    }
}

/// Compare the actual files on disk in a stale workspace against what jj thinks
/// `@` should contain (at the current operation, ignoring the working copy).
pub fn stale_workspace_diff(ws_path: &Path) -> Result<StaleDiff> {
    stale_workspace_diff_inner(ws_path, None)
}

/// Progress message from a background stale-diff computation.
pub enum StaleDiffMsg {
    /// Total number of jj-tracked files to check.
    Total(usize),
    /// Number of files checked so far.
    Checked(usize),
    /// Computation finished (success or failure).
    Done(Result<StaleDiff>),
}

/// Run [`stale_workspace_diff`] on a background thread, sending progress via `tx`.
pub fn stale_workspace_diff_threaded(ws_path: PathBuf, tx: std::sync::mpsc::Sender<StaleDiffMsg>) {
    let result = stale_workspace_diff_inner(&ws_path, Some(&tx));
    let _ = tx.send(StaleDiffMsg::Done(result));
}

fn stale_workspace_diff_inner(
    ws_path: &Path,
    tx: Option<&std::sync::mpsc::Sender<StaleDiffMsg>>,
) -> Result<StaleDiff> {
    use std::collections::HashSet;

    // 1. Get the list of files jj expects at @.
    let file_list_output = run_jj_output(
        jj_cmd_ws(ws_path).stdin(std::process::Stdio::null()).args([
            "file",
            "list",
            "--revision",
            "@",
        ]),
        "file list --revision @",
    )
    .context("failed to run jj file list")?;
    if !file_list_output.status.success() {
        anyhow::bail!(
            "jj file list failed: {}",
            String::from_utf8_lossy(&file_list_output.stderr)
        );
    }
    let file_list_str =
        String::from_utf8(file_list_output.stdout).context("invalid utf-8 in file list")?;
    let jj_files: Vec<&str> = file_list_str.lines().filter(|l| !l.is_empty()).collect();
    let jj_set: HashSet<&str> = jj_files.iter().copied().collect();

    if let Some(tx) = tx {
        let _ = tx.send(StaleDiffMsg::Total(jj_files.len()));
    }

    let mut diff = StaleDiff::default();

    // 2. For each jj-tracked file, compare content.
    for (i, &rel_path) in jj_files.iter().enumerate() {
        let disk_path = ws_path.join(rel_path);
        if disk_path.exists() {
            let mut hasher = blake3::Hasher::new();
            hasher
                .update_reader(
                    &mut std::fs::File::open(&disk_path)
                        .with_context(|| format!("failed to read {}", disk_path.display()))?,
                )
                .with_context(|| format!("failed to hash {}", disk_path.display()))?;
            let disk_hash = hasher.finalize();

            // Get jj's expected content and hash it.
            let show_output = run_jj_output(
                jj_cmd_ws(ws_path).stdin(std::process::Stdio::null()).args([
                    "file",
                    "show",
                    "--revision",
                    "@",
                    rel_path,
                ]),
                &format!("file show --revision @ {rel_path}"),
            )
            .with_context(|| format!("failed to run jj file show for {rel_path}"))?;
            if show_output.status.success() {
                let jj_hash = blake3::hash(&show_output.stdout);
                if disk_hash != jj_hash {
                    diff.modified.push(rel_path.to_string());
                }
            } else {
                diff.modified.push(rel_path.to_string());
            }
        } else {
            diff.jj_only.push(rel_path.to_string());
        }

        if let Some(tx) = tx {
            let _ = tx.send(StaleDiffMsg::Checked(i + 1));
        }
    }

    // 3. Walk disk to find files not in jj's list.
    let mut dirs = vec![ws_path.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Skip VCS internals.
            if name_str == ".jj" || name_str == ".git" || name_str == ".ji" {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.is_file() {
                let rel = path
                    .strip_prefix(ws_path)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                if !jj_set.contains(rel.as_str()) {
                    diff.disk_only.push(rel);
                }
            }
        }
    }

    diff.modified.sort();
    diff.disk_only.sort();
    diff.jj_only.sort();

    Ok(diff)
}

/// Get raw file content from jj at the current `@` revision (ignoring working copy).
pub fn file_show_raw(ws_path: &Path, rel_path: &str) -> Result<Vec<u8>> {
    let output = run_jj_output(
        jj_cmd_ws(ws_path).args(["file", "show", "--revision", "@", rel_path]),
        &format!("file show --revision @ {rel_path}"),
    )
    .with_context(|| format!("failed to run jj file show for {rel_path}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "jj file show failed for {}: {}",
            rel_path,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

// ---------------------------------------------------------------------------
// Hook variable construction
// ---------------------------------------------------------------------------

/// Variables available for workspace-path template expansion (pre-creation).
pub fn path_vars(repo_root: &Path, bookmark: &str, repo_name: &str) -> HookVars {
    let sanitized = bookmark.replace('/', "-");

    let mut vars = HookVars::new();
    vars.insert("home".into(), std::env::var("HOME").unwrap_or_default());
    vars.insert("repo".into(), repo_name.to_string());
    vars.insert("bookmark".into(), sanitized);
    vars.insert(
        "default_workspace_path".into(),
        repo_root.to_string_lossy().to_string(),
    );
    vars
}

pub fn hook_vars(
    repo: &Path,
    ws_path: &Path,
    ws_name: &str,
    bookmark: &str,
    repo_name: &str,
) -> Result<HookVars> {
    let mut vars = path_vars(repo, bookmark, repo_name);

    // Use the workspace name revset (e.g. "feature-ws@") to get its change_id
    let change_id = run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            &format!(r#""{}"@"#, escape_revset_string(ws_name)),
            "--template",
            "change_id",
        ],
    )?;

    vars.insert(
        "workspace_path".into(),
        ws_path.to_string_lossy().to_string(),
    );
    vars.insert("workspace_name".into(), ws_name.to_string());
    vars.insert("change_id".into(), change_id);
    Ok(vars)
}

// ---------------------------------------------------------------------------
// Progress parent workspace
// ---------------------------------------------------------------------------

/// Find the last common ancestor of two revisions.
pub fn last_common_ancestor(repo: &Path, id_a: &str, id_b: &str) -> Result<String> {
    let revset = format!("latest(fork_point({id_a} | {id_b}))");
    run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            &revset,
            "--template",
            "change_id",
        ],
    )
}

/// Resolve full change IDs to their shortest unambiguous prefixes.
///
/// Returns a map from full change ID → shortest prefix.  IDs that fail to
/// resolve (e.g. empty or invalid) are silently omitted from the result.
pub fn shortest_change_ids(repo: &Path, ids: &[&str]) -> HashMap<String, String> {
    let unique: Vec<&str> = {
        let mut seen = HashSet::new();
        ids.iter()
            .filter(|id| !id.is_empty() && seen.insert(**id))
            .copied()
            .collect()
    };
    if unique.is_empty() {
        return HashMap::new();
    }
    let revset = unique
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(" | ");
    // Layout: change_id \x1f change_id.shortest() \x1e
    let template = r#"change_id ++ "\x1f" ++ change_id.shortest() ++ "\x1e""#;
    let output = match run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            &revset,
            "--template",
            template,
        ],
    ) {
        Ok(o) => o,
        Err(_) => return HashMap::new(),
    };
    let mut map = HashMap::new();
    for record in records(&output) {
        if let Some((full, short)) = record.split_once(DELIM_FIELD) {
            map.insert(full.to_string(), short.to_string());
        }
    }
    map
}

/// Create a new revision on a workspace, progressing its @ past the branch point.
pub fn progress_workspace(ws_path: &Path, msg: &str, author: Option<&str>) -> Result<()> {
    if !ws_path.exists() {
        anyhow::bail!(
            "progress skipped — directory missing: {}",
            ws_path.display()
        );
    }
    run_jj_inner(jj_cmd_ws(ws_path), &["new", "--message", msg])?;
    if let Some(a) = author {
        metaedit_author_in_workspace(ws_path, a)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift test: every named template literal in this module must reference
    /// the documented DELIM_FIELD (`\x1f`) and DELIM_RECORD (`\x1e`) escapes.
    /// If a future edit changes the template byte without updating its parser
    /// (or vice versa), this assertion fails and points at the regression.
    ///
    /// This catches the textual-escape form (the templates are jj-template
    /// source code, not raw bytes — they contain the literal text `"\x1e"`,
    /// six characters, which jj parses into the byte). Pair with
    /// `tests/jj_integration.rs` for the actual byte-level round-trip.
    #[test]
    fn templates_use_documented_delimiters() {
        // TMPL_ID_DESC: change_id \x1f description \x1e
        assert!(
            TMPL_ID_DESC.contains(r#""\x1f""#),
            "TMPL_ID_DESC missing \\x1f field delim"
        );
        assert!(
            TMPL_ID_DESC.contains(r#""\x1e""#),
            "TMPL_ID_DESC missing \\x1e record delim"
        );

        // LIST_TEMPLATE: 4 fields with \x1f, terminated with \x1e
        assert!(
            LIST_TEMPLATE.contains(r#""\x1f""#),
            "LIST_TEMPLATE missing \\x1f field delim"
        );
        assert!(
            LIST_TEMPLATE.contains(r#""\x1e""#),
            "LIST_TEMPLATE missing \\x1e record delim"
        );

        // Constants match their documented byte values.
        assert_eq!(DELIM_RECORD, '\x1e');
        assert_eq!(DELIM_FIELD, '\x1f');
        assert_eq!(DELIM_LIST, '\x01');

        // No template should contain a bare `\0` field delim (the legacy scheme).
        // This catches half-migrations that leave one template behind.
        assert!(
            !TMPL_ID_DESC.contains(r#""\0""#),
            "TMPL_ID_DESC still uses legacy \\0 delim"
        );
        assert!(
            !LIST_TEMPLATE.contains(r#""\0""#),
            "LIST_TEMPLATE still uses legacy \\0 delim"
        );
    }

    /// Collect all production source files (`src/**/*.rs`).
    fn production_sources() -> Vec<(std::path::PathBuf, String)> {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        let mut dirs = vec![src];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let content = std::fs::read_to_string(&path).expect("read source file");
                    files.push((path, content));
                }
            }
        }
        assert!(!files.is_empty(), "no source files found");
        files
    }

    /// Count occurrences of `pat` across non-comment lines.
    fn count_in_code(content: &str, pat: &str) -> usize {
        content
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .map(|l| l.matches(pat).count())
            .sum()
    }

    #[test]
    fn bootstrap_timeout_bounds_a_slow_child() {
        // A child that sleeps far past the deadline must return Err promptly
        // (kill + reap), never blocking the prompt. Uses `sleep`, not jj, so
        // the `Command::new("jj")` census count is unaffected.
        let start = Instant::now();
        let res =
            run_jj_bootstrap_timeout(Command::new("sleep"), &["30"], Duration::from_millis(150));
        assert!(res.is_err(), "expected timeout error, got {res:?}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeout did not bound the wait: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn bootstrap_timeout_propagates_nonzero_exit() {
        // `false` exits 1 with no output → no timeout, but a nonzero status →
        // Err, so the caller yields no candidates (never parses partial stdout).
        let res = run_jj_bootstrap_timeout(Command::new("false"), &[], Duration::from_secs(5));
        assert!(res.is_err(), "nonzero exit must be an error, got {res:?}");
    }

    #[test]
    fn unquote_ws_name_strips_and_unescapes() {
        assert_eq!(unquote_ws_name("plain"), "plain");
        assert_eq!(unquote_ws_name(r#""with space""#), "with space");
        // jj escapes an inner quote as \" inside the quoted name.
        assert_eq!(unquote_ws_name(r#""a \" b""#), r#"a " b"#);
        // and an inner backslash as \\.
        assert_eq!(unquote_ws_name(r#""a \\ b""#), r"a \ b");
    }

    #[test]
    fn completion_workspaces_root_with_delimiter_not_misparsed() {
        // `self.root()` is an unescaped filesystem path that can contain the
        // field delimiter; it is last in the template, so splitn captures the
        // whole path rather than misparsing into a truncated/orphaned candidate
        // (the concrete case a reviewer found: a workspace root with `\x1f`).
        let f = DELIM_FIELD;
        let r = DELIM_RECORD;
        let stdout =
            format!("ws-a{f}1700000000{f}/normal/root{r}weird{f}1700000001{f}/has{f}delim{r}");
        let parsed = parse_completion_workspaces(&stdout);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "ws-a");
        assert_eq!(parsed[0].last_touched, Some(1_700_000_000));
        assert_eq!(parsed[0].root.as_deref(), Some(Path::new("/normal/root")));
        // The embedded delimiter stays inside the captured (last) root field.
        assert_eq!(parsed[1].name, "weird");
        assert_eq!(parsed[1].last_touched, Some(1_700_000_001));
        assert_eq!(
            parsed[1].root.as_deref(),
            Some(Path::new(&format!("/has{f}delim")))
        );
    }

    /// Census tripwire — keeps the snapshot-policy census (doc block above
    /// the core helpers) maintained:
    ///
    /// (a) Every jj subprocess in production `src/` is constructed via a
    ///     `jj_cmd*` builder: `Command::new("jj")` appears only in this
    ///     file — the five builders plus the version-detection call.
    /// (b) The live/snapshot wrappers have a pinned number of mentions. A
    ///     new call site through an existing wrapper (a new live/snapshot
    ///     vector) fails this test until the census doc block and this
    ///     table are updated together.
    ///
    /// Counts are textual occurrences on non-comment lines (definitions
    /// included), summed across `src/**/*.rs`.
    #[test]
    fn jj_call_census_is_maintained() {
        let files = production_sources();

        // (a) Builder boundary. The pattern is assembled at runtime so this
        // test's own source never matches it.
        let cmd_pat = format!("{}({:?})", "Command::new", "jj");
        for (path, content) in &files {
            let n = count_in_code(content, &cmd_pat);
            if path.ends_with("jujutsu.rs") {
                assert_eq!(
                    n, 6,
                    "expected exactly 6 jj Command constructions in jujutsu.rs \
                     (5 builders + version detection), found {n}"
                );
            } else {
                assert_eq!(
                    n,
                    0,
                    "raw jj subprocess outside jujutsu.rs in {} — route it \
                     through a jj_cmd* builder",
                    path.display()
                );
            }
        }

        // (b) Live/snapshot wrapper tripwire. Function names only — the `(`
        // is appended at runtime so the table doesn't count itself.
        let expected: &[(&str, usize)] = &[
            ("snapshot_ws", 9),
            ("run_jj_ws_live", 6),
            ("jj_cmd_wc", 6),
            ("jj_cmd_ws_wc", 7),
            ("is_workspace_stale", 5),
            ("stale_workspace_names", 3),
            ("update_workspace_stale", 12),
            ("update_stale", 2),
            ("has_unsnapshotted_changes", 1),
            // Non-mutating completion bootstrap readers (ignore-WC, bounded
            // timeout): def + the two completion fns (+ timeout-runner tests for
            // run_jj_bootstrap_timeout). Pinned so new completion readers trip
            // this until classified in the census doc block above.
            ("jj_cmd_bootstrap", 4),
            ("run_jj_bootstrap_timeout", 5),
        ];
        for (name, want) in expected {
            let pat = format!("{name}(");
            let got: usize = files.iter().map(|(_, c)| count_in_code(c, &pat)).sum();
            assert_eq!(
                got, *want,
                "census drift for `{pat}`: expected {want} mentions, found {got}. \
                 Classify the new/removed site in the snapshot-policy census \
                 (src/jujutsu.rs) and update this table."
            );
        }
    }
}
