use crate::hooks::HookVars;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Instant;

const MIN_JJ_VERSION: &str = "0.40.0";
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
#[allow(dead_code)]
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
// Rust-side parsers must split on the corresponding DELIM_* constant
// (e.g. `stdout.split(DELIM_RECORD)`) so a template-edit that forgets to
// update the parser is caught by the source-level drift test (see
// tests/jj_integration.rs).

pub(crate) const DELIM_RECORD: char = '\x1e';
pub(crate) const DELIM_FIELD: char = '\x1f';
pub(crate) const DELIM_LIST: char = '\x01';

/// Template: `change_id \x1f description \x1e`.
pub(crate) const TMPL_ID_DESC: &str = r#"change_id ++ "\x1f" ++ description ++ "\x1e""#;

// ---------------------------------------------------------------------------
// Core helpers — all jj commands go through here
// ---------------------------------------------------------------------------

/// Working-copy access policy for jj subprocess calls.
#[allow(dead_code)]
pub(crate) enum WcPolicy {
    /// Default. Adds `--ignore-working-copy`. No WC snapshot or update.
    Ignore,
    /// Runs `jj util snapshot` first, then command with `--ignore-working-copy`.
    /// Use when pending WC edits must be captured before the command.
    SnapshotFirst,
    /// No `--ignore-working-copy`. jj snapshots WC before and updates after.
    /// Use when the command must read or write the working copy.
    Live,
}

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

/// Build a `Command` targeting a repo via `--repository` WITHOUT `--ignore-working-copy`.
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

/// Build a `Command` targeting a workspace via `current_dir` WITHOUT `--ignore-working-copy`.
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

/// Run a jj command with an explicit working-copy policy.
#[allow(dead_code)]
pub(crate) fn run_jj_with(repo: &Path, args: &[&str], policy: WcPolicy) -> Result<String> {
    match policy {
        WcPolicy::Ignore => run_jj_inner(jj_cmd(repo), args),
        WcPolicy::SnapshotFirst => {
            snapshot(repo)?;
            run_jj_inner(jj_cmd(repo), args)
        }
        WcPolicy::Live => run_jj_inner(jj_cmd_wc(repo), args),
    }
}

/// Snapshot the working copy of the default workspace (captures pending edits).
pub fn snapshot(repo: &Path) -> Result<()> {
    run_jj_inner(jj_cmd_wc(repo), &["util", "snapshot"])?;
    Ok(())
}

/// Snapshot the working copy of a specific workspace (captures pending edits).
pub fn snapshot_ws(ws_path: &Path) -> Result<()> {
    run_jj_inner(jj_cmd_ws_wc(ws_path), &["util", "snapshot"])?;
    Ok(())
}

/// Run a jj command in a workspace directory WITHOUT `--ignore-working-copy`.
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

/// Run a jj subprocess WITHOUT a version check. Used by:
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
    for record in output.split(DELIM_RECORD) {
        let record = record.trim_matches('\n');
        if record.is_empty() {
            continue;
        }
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
/// If `expected_op_id` is not found within 200 operations, assumes non-trivial work.
pub fn only_snapshots_since(repo: &Path, expected_op_id: &str) -> Result<bool> {
    // Layout: id \x1f snapshot? \x1e
    let template = r#"id ++ "\x1f" ++ if(self.snapshot(), "true", "false") ++ "\x1e""#;
    let output = run_jj(
        repo,
        &[
            "--at-op=@",
            "op",
            "log",
            "--no-graph",
            "--limit",
            "200",
            "--template",
            template,
        ],
    )?;
    for record in output.split(DELIM_RECORD) {
        let record = record.trim_matches('\n');
        if record.is_empty() {
            continue;
        }
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

pub fn list_workspaces(repo: &Path) -> Result<Vec<Workspace>> {
    let stdout = run_jj(repo, &["workspace", "list", "--template", LIST_TEMPLATE])?;

    let mut workspaces = Vec::new();
    let mut default_change_id = String::new();
    for record in stdout.split(DELIM_RECORD) {
        let record = record.trim_matches('\n');
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.splitn(4, DELIM_FIELD).collect();
        if fields.len() < 4 {
            anyhow::bail!("unexpected workspace list format: {record}");
        }
        // jj wraps names containing spaces in double quotes for display
        // and escapes inner quotes/backslashes. Strip and unescape.
        let raw_name = fields[0];
        let name = if raw_name.starts_with('"') && raw_name.ends_with('"') && raw_name.len() > 1 {
            raw_name[1..raw_name.len() - 1]
                .replace("\\\"", "\"")
                .replace("\\\\", "\\")
        } else {
            raw_name.to_string()
        };
        if name == "default" {
            default_change_id = fields[2].to_string();
        }
        let raw_path = fields[1].trim();
        let path = if raw_path.contains("<Error") {
            PathBuf::new()
        } else {
            PathBuf::from(raw_path)
        };
        workspaces.push(Workspace {
            name,
            path,
            change_id: fields[2].to_string(),
            description: fields[3].to_string(),
            bookmarks_at_head: Vec::new(),
            bookmarks_behind: Vec::new(),
            is_current: false,
            revisions: Vec::new(),
            last_modified: None,
        });
    }

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

/// Check if a bookmark exists (resolves to at least one revision).
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
    stdout
        .split(DELIM_RECORD)
        .map(|r| r.trim_matches('\n'))
        .filter(|r| !r.is_empty())
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

    for record in stdout.split(DELIM_RECORD) {
        let record = record.trim_matches('\n');
        if record.is_empty() {
            continue;
        }
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

/// Resolve the change_ids on a merged workspace's unique branch (display only).
///
/// When a workspace has been merged into default, `workspace_revisions` returns
/// empty because `::{ws} ~ ::{default}` is ∅.  This finds the merge that
/// absorbed the workspace and subtracts the ancestry of its *other* parents,
/// returning just the short change_ids on the workspace's branch.
#[allow(dead_code)]
pub fn merged_branch_ids(
    repo: &Path,
    ws_change_id: &str,
    default_change_id: &str,
) -> Result<Vec<String>> {
    let revset = format!(
        "(parents(children({ws_change_id}) & merges() & ::{default_change_id}) ~ ::{ws_change_id})..{ws_change_id}"
    );
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

/// Create a new working copy commit on top of the given revision.
#[allow(dead_code)]
pub fn new_on(repo: &Path, revision: &str, msg: &str, author: Option<&str>) -> Result<()> {
    run_jj(repo, &["new", "--message", msg, "--", revision])?;
    if let Some(a) = author {
        run_jj(repo, &["metaedit", "--author", a])?;
    }
    Ok(())
}

/// Create a merge commit (multiple parents) in a specific workspace.
/// Uses `current_dir(ws_path)` so the workspace's `@` is updated.
/// Returns the change-id of the newly created merge revision.
///
/// Runs WITHOUT `--ignore-working-copy` so jj auto-snapshots pending edits
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
    for record in stdout.split(DELIM_RECORD) {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
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
pub fn is_working_copy_stale(repo: &Path) -> bool {
    let output = run_jj_output(
        jj_cmd_wc(repo).args([
            "status",
            "--no-pager",
            "--no-graph",
            "--color",
            "never",
            "--quiet",
        ]),
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
pub fn is_workspace_stale(ws_path: &Path) -> bool {
    if !ws_path.exists() {
        return false;
    }
    let output = run_jj_output(
        jj_cmd_ws_wc(ws_path).args([
            "status",
            "--no-pager",
            "--no-graph",
            "--color",
            "never",
            "--quiet",
        ]),
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
    for record in output.split(DELIM_RECORD) {
        let record = record.trim_matches('\n');
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
}
