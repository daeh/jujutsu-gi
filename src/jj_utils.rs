use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;

use crate::{hooks, jujutsu};

// ---------------------------------------------------------------------------
// jj template constants (file-local)
//
// See src/jujutsu.rs for the project-wide delimiter convention. Fields use
// DELIM_FIELD (\x1f); records use DELIM_RECORD (\x1e); lists use DELIM_LIST
// (\x01). The constants live in jujutsu.rs and are re-exported via
// `use crate::jujutsu::{DELIM_RECORD, DELIM_FIELD};` at call sites here.
// ---------------------------------------------------------------------------

/// Template: `change_id \x1f empty? \x1f merge? \x1f description \x1e`.
const TMPL_ID_EMPTY_MERGE_DESC: &str = r#"change_id ++ "\x1f" ++ if(empty, "true", "false") ++ "\x1f" ++ if(parents.len() > 1, "true", "false") ++ "\x1f" ++ description ++ "\x1e""#;

/// Template: `change_id \x1f bookmarks \x1f description \x1e`.
const TMPL_ID_BOOKMARKS_DESC: &str =
    r#"change_id ++ "\x1f" ++ bookmarks ++ "\x1f" ++ description ++ "\x1e""#;

/// Template: `change_id \x1f empty? \x1e`.
const TMPL_ID_EMPTY: &str = r#"change_id ++ "\x1f" ++ if(empty, "true", "false") ++ "\x1e""#;

/// Template: `change_id \x1f empty? \x1f parents.len() \x1f description` —
/// single-record query, no record terminator.
const TMPL_ID_EMPTY_PARENTS_DESC: &str = r#"change_id ++ "\x1f" ++ if(empty, "true", "false") ++ "\x1f" ++ parents.len() ++ "\x1f" ++ description"#;

// ---------------------------------------------------------------------------
// Structured descriptions
// ---------------------------------------------------------------------------

/// Namespace prefix for ji-generated commit descriptions.
pub const PREFIX: &str = "ji::";

/// Operation types for structured commit descriptions.
pub enum Op {
    Step,
    FastForward,
    Merge,
    Squash,
}

impl Op {
    /// The canonical operation name.
    pub fn operation(&self) -> &'static str {
        match self {
            Op::Step => "step-forward",
            Op::FastForward => "fast-forward",
            Op::Merge => "merge",
            Op::Squash => "squash",
        }
    }
}

/// Build a structured commit description.
///
/// Returns `"({PREFIX}{label})"` when `detail` is `None` or empty,
/// or `"({PREFIX}{label}) {detail}"` otherwise.
pub fn make_desc(op: Op, detail: Option<&str>) -> String {
    let operation = op.operation();
    match detail.filter(|s| !s.is_empty()) {
        Some(d) => format!("({PREFIX}{operation}) {d}"),
        None => format!("({PREFIX}{operation})"),
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Build the `"<ws_name>"` revset fragment for a workspace (without the `@` suffix).
fn ws_at_revset(ws_name: &str) -> String {
    format!(r#""{}""#, jujutsu::escape_revset_string(ws_name))
}

/// Change IDs of revisions unique to a workspace relative to default
/// (`"default"@.."<ws>"@`) — the same set definition `list_workspaces` uses
/// to build per-workspace revision lists (`workspace_revisions_with_bookmarks`
/// evaluates it with change-id endpoints; here the workspace refs resolve
/// live). Deliberately NOT an LCA-relative range: the LCA is source-vs-target
/// and diverges from this producer definition for non-default targets.
pub fn workspace_unique_change_ids(repo: &Path, ws_name: &str) -> Result<Vec<String>> {
    let revset = format!("{}..{}", ws_head_revset("default"), ws_head_revset(ws_name));
    let out = jujutsu::run_jj(
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
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Build the `"<ws_name>"@` revset for a workspace's working-copy commit.
pub fn ws_head_revset(ws_name: &str) -> String {
    format!("{}@", ws_at_revset(ws_name))
}

/// Query a single revision's change_id (8-char short form).
///
/// Errors if the revset matches more than one revision.
pub(crate) fn resolve_change_id(repo: &Path, revset: &str) -> Result<String> {
    let output = jujutsu::run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            revset,
            "--template",
            r#"change_id ++ "\n""#,
        ],
    )?;
    // The template appends \n per revision; trim() strips the trailing one.
    // A newline remaining means the revset matched multiple revisions.
    anyhow::ensure!(
        !output.contains('\n'),
        "expected single revision for `{revset}`, got multiple"
    );
    Ok(output)
}

/// Filter a list of change IDs to only those that still exist in the repo.
///
/// Wraps each ID in `present()` so that missing IDs resolve to `none()`
/// instead of erroring (a bare union revset fails on any missing member).
pub fn filter_existing_revisions<'a>(repo: &Path, change_ids: &[&'a str]) -> Result<Vec<&'a str>> {
    if change_ids.is_empty() {
        return Ok(Vec::new());
    }
    let revset = change_ids
        .iter()
        .map(|id| format!("present({id})"))
        .collect::<Vec<_>>()
        .join(" | ");
    let template = r#"change_id ++ "\n""#;
    let out = jujutsu::run_jj(
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
    let found: HashSet<&str> = out
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(change_ids
        .iter()
        .copied()
        .filter(|id| found.contains(id))
        .collect())
}

/// Check whether a revision's description is trivial.
///
/// Returns `true` if any of:
/// - empty
/// - single word (no whitespace)
/// - a single line starting with `(ji::` (ji-generated marker)
/// - matches jj's configured `templates.new_description` for this revision
fn is_trivial_description(repo: &Path, revset: &str, desc: &str) -> bool {
    let d = desc.trim();
    if d.is_empty() {
        return true;
    }
    if !d.contains(char::is_whitespace) {
        return true;
    }
    if !d.contains('\n') && d.starts_with(&format!("({PREFIX}")) {
        return true;
    }
    matches_default_new_description(repo, revset, d)
}

/// Check if `desc` matches what jj would generate for a new revision from the
/// user's `templates.new_description` config, evaluated against `revset`.
fn matches_default_new_description(repo: &Path, revset: &str, desc: &str) -> bool {
    let Ok(template) = jujutsu::run_jj(repo, &["config", "get", "templates.new_description"])
    else {
        return false;
    };
    if template.is_empty() {
        return false;
    }
    let Ok(default_desc) = jujutsu::run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            revset,
            "--template",
            &template,
        ],
    ) else {
        return false;
    };
    desc == default_desc.trim()
}

/// Returns `Some(change_id)` if the revision is a trivial head, `None` otherwise.
///
/// A revision is a trivial head iff it:
/// 0. is a graph head (no descendants)
/// 1. is empty (no file changes)
/// 2. is not a merge (single parent only)
/// 3. has a trivial description
pub fn check_trivial_head(repo: &Path, revset: &str) -> Result<Option<String>> {
    // Filter to heads only via revset: exclude revisions that are parents of
    // their own children (i.e. revisions that have descendants).
    let head_revset = format!("({revset}) ~ parents(children({revset}))");
    let template = TMPL_ID_EMPTY_MERGE_DESC;
    let stdout = jujutsu::run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            &head_revset,
            "--template",
            template,
        ],
    )?;

    // Empty output means the revision is not a head.
    if stdout.is_empty() {
        return Ok(None);
    }

    // Single-record template; strip the trailing DELIM_RECORD before splitting fields.
    let record = stdout.trim_end_matches(jujutsu::DELIM_RECORD);
    let parts: Vec<&str> = record.splitn(4, jujutsu::DELIM_FIELD).collect();
    if parts.len() < 4 {
        return Ok(None);
    }

    let change_id = parts[0];
    let is_empty = parts[1] == "true";
    let is_merge = parts[2] == "true";
    let desc = parts[3];

    // Merge commits are never trivial — their @- resolves to multiple parents.
    if !is_empty || is_merge {
        return Ok(None);
    }

    if is_trivial_description(repo, revset, desc) {
        Ok(Some(change_id.to_string()))
    } else {
        Ok(None)
    }
}

/// All head data needed by sync/close dialogs for a single workspace.
pub struct WorkspaceHeadInfo {
    /// Change ID of the effective head (@ if non-trivial, @- if trivial).
    pub effective_head: String,
    /// Change ID of the actual @ (always the literal working-copy revision).
    pub actual_head: String,
    /// If @ is a trivial head, its change ID; None otherwise.
    pub trivial_id: Option<String>,
}

/// Resolve effective head, actual head, and triviality for a workspace in 1-2 jj calls.
///
/// Replaces the pattern of calling `find_effective_head` + `check_trivial_head` +
/// `resolve_workspace_head` separately (3-5 jj calls) with a single pass that
/// reuses data from `check_trivial_head`'s internal query.
pub fn resolve_workspace_head_info(repo: &Path, ws_name: &str) -> Result<WorkspaceHeadInfo> {
    let ws = ws_at_revset(ws_name);
    let ws_at = format!("{ws}@");

    // Call 1: head check + data retrieval (same query as check_trivial_head).
    let head_revset = format!("({ws_at}) ~ parents(children({ws_at}))");
    let template = TMPL_ID_EMPTY_MERGE_DESC;
    let stdout = jujutsu::run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            &head_revset,
            "--template",
            template,
        ],
    )?;

    if stdout.is_empty() {
        // @ is not a graph head — not trivial, effective head = actual head = @.
        // Call 2: resolve @'s change_id.
        let actual = resolve_change_id(repo, &ws_at)?;
        return Ok(WorkspaceHeadInfo {
            effective_head: actual.clone(),
            actual_head: actual,
            trivial_id: None,
        });
    }

    let record = stdout.trim_end_matches(jujutsu::DELIM_RECORD);
    let parts: Vec<&str> = record.splitn(4, jujutsu::DELIM_FIELD).collect();
    if parts.len() < 4 {
        // Malformed output — fall back to resolving @ directly.
        let actual = resolve_change_id(repo, &ws_at)?;
        return Ok(WorkspaceHeadInfo {
            effective_head: actual.clone(),
            actual_head: actual,
            trivial_id: None,
        });
    }

    let change_id = parts[0].to_string();
    let is_empty = parts[1] == "true";
    let is_merge = parts[2] == "true";
    let desc = parts[3];

    // @ IS a graph head. We have its change_id. Check triviality.
    // Merge commits are never trivial — their @- resolves to multiple parents.
    let is_trivial = is_empty && !is_merge && is_trivial_description(repo, &ws_at, desc);

    if is_trivial {
        // Walk back past any stacked trivial-content ancestors.
        let effective = find_nontrivial_target(repo, &format!("{ws}@-"))?;
        Ok(WorkspaceHeadInfo {
            effective_head: effective,
            actual_head: change_id.clone(),
            trivial_id: Some(change_id),
        })
    } else {
        // @ is a head but not trivial. Effective head = actual head.
        // No extra call needed — we already have change_id from the template output.
        Ok(WorkspaceHeadInfo {
            effective_head: change_id.clone(),
            actual_head: change_id,
            trivial_id: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Returns `true` if the revision has no descendants (is a graph head).
pub fn is_head(repo: &Path, revset: &str) -> Result<bool> {
    let head_revset = format!("({revset}) ~ parents(children({revset}))");
    let out = jujutsu::run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            &head_revset,
            "--template",
            r#"".""#,
        ],
    )?;
    Ok(!out.is_empty())
}

/// Returns `true` if the revision identified by `revset` is a trivial head:
/// a graph head, empty, and with a trivial description.
pub fn is_trivial_head(repo: &Path, revset: &str) -> Result<bool> {
    Ok(check_trivial_head(repo, revset)?.is_some())
}

// ---------------------------------------------------------------------------
// Revision safety checks
// ---------------------------------------------------------------------------

/// Result of a revision safety check.
#[derive(Debug)]
pub enum RevisionSafety {
    /// No risk of work loss.
    Safe,
    /// Revision has file changes but no description — at risk of being lost.
    AtRisk { change_id: String },
}

/// Check whether orphaning a **head** revision would risk losing work.
///
/// Returns `AtRisk` only when ALL of:
/// 1. The revision is a graph head (no descendants)
/// 2. It has no local bookmarks (not findable by name)
/// 3. Its description is whitespace-only (user never described it)
/// 4. It is non-empty after snapshot (has file changes worth preserving)
///
/// Short-circuits cheapest checks first; snapshot is deferred to step 4.
///
/// `ws_path`: pass `Some` when checking `@` of a live workspace so jj
/// auto-snapshots pending working-directory edits before the emptiness query.
/// Pass `None` for already-committed revisions.
pub fn check_revision_safety(
    repo: &Path,
    revset: &str,
    ws_path: Option<&Path>,
) -> Result<RevisionSafety> {
    // Step 1: head + bookmark + description — one jj call, --ignore-working-copy.
    let head_revset = format!("({revset}) ~ parents(children({revset}))");
    let template = TMPL_ID_BOOKMARKS_DESC;
    let out = jujutsu::run_jj(
        repo,
        &[
            "log",
            "--no-graph",
            "--revision",
            &head_revset,
            "--template",
            template,
        ],
    )?;

    if out.is_empty() {
        return Ok(RevisionSafety::Safe); // not a head
    }

    let record = out.trim_end_matches(jujutsu::DELIM_RECORD);
    let parts: Vec<&str> = record.splitn(3, jujutsu::DELIM_FIELD).collect();
    if parts.len() < 3 {
        return Ok(RevisionSafety::Safe); // unexpected format, be conservative
    }
    let change_id = parts[0];
    let bookmarks = parts[1];
    let desc = parts[2];

    if !bookmarks.is_empty() {
        return Ok(RevisionSafety::Safe); // findable by bookmark
    }
    if !desc.trim().is_empty() {
        return Ok(RevisionSafety::Safe); // user described it
    }

    // Step 2: emptiness — snapshot if ws_path provided.
    check_emptiness(repo, revset, ws_path, change_id)
}

/// Check whether displacing a working-copy revision would risk losing work.
///
/// Like [`check_revision_safety`] but **skips the head check**. Use when the
/// revision's descendants don't protect it from loss — e.g. `fast_forward_close`
/// displaces target `@` via `jj edit`, and the target `@` always has descendants
/// (the source chain) but the working-directory edits can still be lost.
///
/// `ws_path` triggers jj auto-snapshot before the emptiness query.
pub fn check_working_copy_safety(
    repo: &Path,
    revset: &str,
    ws_path: &Path,
) -> Result<RevisionSafety> {
    // Step 1: bookmark + description — one jj call, --ignore-working-copy.
    let template = TMPL_ID_BOOKMARKS_DESC;
    let out = jujutsu::run_jj(
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

    if out.is_empty() {
        return Ok(RevisionSafety::Safe); // revision doesn't exist
    }

    let record = out.trim_end_matches(jujutsu::DELIM_RECORD);
    let parts: Vec<&str> = record.splitn(3, jujutsu::DELIM_FIELD).collect();
    if parts.len() < 3 {
        return Ok(RevisionSafety::Safe);
    }
    let change_id = parts[0];
    let bookmarks = parts[1];
    let desc = parts[2];

    if !bookmarks.is_empty() {
        return Ok(RevisionSafety::Safe);
    }
    if !desc.trim().is_empty() {
        return Ok(RevisionSafety::Safe);
    }

    // Step 2: emptiness — always snapshot via ws_path.
    check_emptiness(repo, revset, Some(ws_path), change_id)
}

/// Check whether abandoning a chain of revisions would risk losing work.
///
/// Unlike [`check_revision_safety`], skips the head check because the entire
/// chain is being destroyed (interior nodes' descendants are also victims).
///
/// Returns change IDs of at-risk revisions (non-empty with whitespace-only
/// description).
pub fn check_chain_safety(
    repo: &Path,
    change_ids: &[&str],
    ws_path: Option<&Path>,
) -> Result<Vec<String>> {
    if change_ids.is_empty() {
        return Ok(Vec::new());
    }

    // Step 1: bulk description query — --ignore-working-copy.
    // Use DELIM_RECORD as the record separator because descriptions can contain \n.
    let revset = change_ids.join(" | ");
    let template = jujutsu::TMPL_ID_DESC;
    let out = jujutsu::run_jj(
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

    let undescribed: Vec<&str> = jujutsu::records(&out)
        .filter_map(|record| {
            let (id, desc) = record.split_once(jujutsu::DELIM_FIELD)?;
            if id.is_empty() {
                return None;
            }
            if desc.trim().is_empty() {
                Some(id)
            } else {
                None
            }
        })
        .collect();

    if undescribed.is_empty() {
        return Ok(Vec::new());
    }

    // Step 2: bulk emptiness query — snapshot if ws_path provided.
    let empty_revset = undescribed.join(" | ");
    let empty_out = if let Some(ws) = ws_path {
        jujutsu::run_jj_ws_live(
            ws,
            &[
                "log",
                "--no-graph",
                "--revision",
                &empty_revset,
                "--template",
                TMPL_ID_EMPTY,
            ],
        )?
    } else {
        jujutsu::run_jj(
            repo,
            &[
                "log",
                "--no-graph",
                "--revision",
                &empty_revset,
                "--template",
                TMPL_ID_EMPTY,
            ],
        )?
    };

    let at_risk: Vec<String> = jujutsu::records(&empty_out)
        .filter_map(|record| {
            let (id, is_empty) = record.split_once(jujutsu::DELIM_FIELD)?;
            if is_empty == "false" {
                Some(id.to_string())
            } else {
                None
            }
        })
        .collect();

    Ok(at_risk)
}

/// Shared emptiness check for safety functions. Runs WITHOUT `--ignore-working-copy`
/// when `ws_path` is provided (triggering jj auto-snapshot), or with it when `None`.
fn check_emptiness(
    repo: &Path,
    revset: &str,
    ws_path: Option<&Path>,
    change_id: &str,
) -> Result<RevisionSafety> {
    let args = [
        "log",
        "--no-graph",
        "--revision",
        revset,
        "--template",
        r#"if(empty, "true", "false")"#,
    ];
    let out = if let Some(ws) = ws_path {
        jujutsu::run_jj_ws_live(ws, &args)?
    } else {
        jujutsu::run_jj(repo, &args)?
    };

    if out.trim() == "true" {
        Ok(RevisionSafety::Safe)
    } else {
        Ok(RevisionSafety::AtRisk {
            change_id: change_id.to_string(),
        })
    }
}

/// Abandon revisions only if each is still a trivial head (head + empty + trivial description).
/// Re-checks each revision immediately before abandoning. Abandons those that are still
/// trivial, then returns an error if any were not trivial heads.
///
/// If a trivial head has local bookmarks, moves them to the parent before
/// abandoning — prevents orphaning bookmarks on abandoned revisions.
pub fn abandon_trivial_heads(repo: &Path, change_ids: &[&str]) -> Result<()> {
    let mut to_abandon = Vec::new();
    let mut skipped = Vec::new();
    for id in change_ids {
        if check_trivial_head(repo, id)?.is_some() {
            // Move any local bookmarks to the parent before abandoning.
            let bookmarks = jujutsu::local_bookmarks_on(repo, id)?;
            if !bookmarks.is_empty() {
                let parent = resolve_change_id(repo, &format!("parents({id})"))?;
                for bm in &bookmarks {
                    jujutsu::bookmark_set(repo, bm, &parent)
                        .with_context(|| format!("moving bookmark {bm} off trivial head {id}"))?;
                }
            }
            to_abandon.push(*id);
        } else {
            skipped.push(*id);
        }
    }
    if !to_abandon.is_empty() {
        jujutsu::abandon_revisions(repo, &to_abandon)?;
    }
    anyhow::ensure!(
        skipped.is_empty(),
        "revisions not trivial heads: {}",
        skipped.join(", ")
    );
    Ok(())
}

/// Return the change-id of the effective head for a workspace.
///
/// Walks backward from `@` through trivial-content revisions (empty with a
/// trivial description) to find the first revision with meaningful content.
/// This prevents bookmarks from landing on empty WIP revisions, even when
/// multiple trivial revisions are stacked.
///
/// Stops and returns the current revision when:
/// - It has content (non-empty or non-trivial description)
/// - It is a merge (multiple parents — ambiguous which parent to follow)
/// - It has no parents (root commit)
/// - The depth limit is reached (returns the deepest revision checked)
pub fn find_effective_head(repo: &Path, ws_name: &str) -> Result<String> {
    let ws = ws_at_revset(ws_name);
    let ws_at = format!("{ws}@");
    find_nontrivial_target(repo, &ws_at)
}

/// Maximum depth to walk backward through trivial-content ancestors.
const MAX_TRIVIAL_WALK: usize = 10;

/// Walk backward from `starting_revset` to find the first revision with
/// non-trivial content (non-empty or meaningful description).
///
/// Unlike `check_trivial_head`, this checks **content** not **graph position**
/// — a revision doesn't need to be a graph head to be skipped. This matters
/// when `@` is trivial and `@-` is also empty: after `@` is abandoned, `@-`
/// becomes a trivial head. By walking past it now, we avoid placing bookmarks
/// on revisions that will become trivial heads after cleanup.
fn find_nontrivial_target(repo: &Path, starting_revset: &str) -> Result<String> {
    let template = TMPL_ID_EMPTY_PARENTS_DESC;

    let mut current_revset = starting_revset.to_string();
    let mut last_change_id = String::new();

    for _ in 0..MAX_TRIVIAL_WALK {
        let stdout = jujutsu::run_jj(
            repo,
            &[
                "log",
                "--no-graph",
                "--revision",
                &current_revset,
                "--template",
                template,
            ],
        )?;

        let parts: Vec<&str> = stdout.splitn(4, jujutsu::DELIM_FIELD).collect();
        if parts.len() < 4 {
            // Malformed output — return what we have.
            return resolve_change_id(repo, &current_revset);
        }

        let change_id = parts[0];
        let is_empty = parts[1] == "true";
        let parent_count: usize = parts[2].parse().unwrap_or(0);
        let desc = parts[3];

        last_change_id = change_id.to_string();

        // Non-empty revision → has content, return it.
        if !is_empty {
            return Ok(last_change_id);
        }

        // Merge → don't walk through (ambiguous parents), return it.
        // Root (no parents) → nowhere to go, return it.
        if parent_count != 1 {
            return Ok(last_change_id);
        }

        // Non-trivial description → meaningful even if empty, return it.
        if !is_trivial_description(repo, &current_revset, desc) {
            return Ok(last_change_id);
        }

        // Trivial content — walk to parent.
        current_revset = format!("parents({change_id})");
    }

    // Depth limit reached — return the last revision we examined.
    Ok(last_change_id)
}

/// Ensure the workspace has a fresh trivial head.
///
/// If `@` is already a trivial head, this is a no-op — returns the existing
/// `@`'s change-id.  Otherwise creates a `(ji::step-forward)` revision
/// and returns the new `@`'s change-id.
pub fn step_head(
    repo: &Path,
    ws_name: &str,
    ws_path: &Path,
    desc: Option<&str>,
    author: Option<&str>,
) -> Result<String> {
    let ws = ws_at_revset(ws_name);
    let ws_at = format!("{ws}@");

    if is_trivial_head(repo, &ws_at)? {
        return resolve_change_id(repo, &ws_at);
    }

    let msg = make_desc(Op::Step, desc);
    // Run without --ignore-working-copy: jj auto-snapshots pending edits
    // into current @ before creating the new child.
    jujutsu::run_jj_ws_live(ws_path, &["new", "--message", &msg])?;
    if let Some(a) = author {
        jujutsu::metaedit_author_in_workspace(ws_path, a)?;
    }
    resolve_change_id(repo, &ws_at)
}

/// Create a new revision on `parent` in the given workspace.
///
/// Runs `jj new "<parent>" --message "<desc>"` and returns the
/// change-id of the newly created `@`.
///
/// `parent` defaults to `"@"`.
pub fn make_head(
    repo: &Path,
    ws_name: &str,
    ws_path: &Path,
    parent: Option<&str>,
    op: Op,
    desc: Option<&str>,
    author: Option<&str>,
) -> Result<String> {
    let parent = parent.unwrap_or("@");
    let msg = make_desc(op, desc);
    // Run without --ignore-working-copy: jj auto-snapshots pending edits
    // into current @ before creating the new revision.
    jujutsu::run_jj_ws_live(ws_path, &["new", "--message", &msg, "--", parent])?;
    if let Some(a) = author {
        jujutsu::metaedit_author_in_workspace(ws_path, a)?;
    }

    resolve_change_id(repo, &format!("{}@", ws_at_revset(ws_name)))
}

/// Move a workspace's `@` onto an existing commit (no new commit created).
///
/// Runs `jj edit <commit>` inside the given workspace. Unlike `make_head`,
/// this does not create a child — it reassigns `@` to an existing revision.
pub fn edit_workspace_head(ws_path: &Path, commit: &str) -> Result<()> {
    // Run without --ignore-working-copy: jj auto-snapshots pending edits
    // into current @ before reassigning working copy.
    jujutsu::run_jj_ws_live(ws_path, &["edit", commit])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Singular bookmark helpers
// ---------------------------------------------------------------------------

/// Identify the singular bookmark associated with a workspace name.
///
/// Uses the config's `workspace-path` template to derive a bookmark name,
/// then resolves it to the actual bookmark (handling `/` → `-` sanitization).
/// Returns `(actual_bookmark_name, current_change_id)` if found.
pub fn identify_singular_bookmark(
    repo: &Path,
    workspace_path_template: &str,
    repo_name: &str,
    ws_name: &str,
) -> Option<(String, String)> {
    let derived = hooks::derive_bookmark_from_ws_name(workspace_path_template, repo_name, ws_name)?;
    jujutsu::resolve_sanitized_bookmark(repo, &derived)
}

/// Advance a workspace's singular bookmark to `target_revision`.
///
/// Returns `Ok(None)` if no singular bookmark exists or it already points at
/// the target. Returns `Ok(Some(name))` on success, `Err` if the jj command
/// fails.
pub fn advance_singular_bookmark(
    repo: &Path,
    workspace_path_template: &str,
    repo_name: &str,
    ws_name: &str,
    target_revision: &str,
) -> Result<Option<String>> {
    let (bm_name, current_id) =
        match identify_singular_bookmark(repo, workspace_path_template, repo_name, ws_name) {
            Some(pair) => pair,
            None => return Ok(None),
        };
    if current_id == target_revision {
        return Ok(None);
    }
    jujutsu::bookmark_set(repo, &bm_name, target_revision)
        .with_context(|| format!("advancing bookmark {bm_name}"))?;
    Ok(Some(bm_name))
}

/// Advance a workspace's singular bookmark to the workspace's effective head.
///
/// The effective head is `@` if non-trivial, or `@-` if `@` is a trivial head.
/// This prevents bookmarks from landing on empty WIP revisions.
///
/// Returns `Ok(None)` if no singular bookmark exists or it already points at
/// the effective head. Returns `Ok(Some(name))` on success, `Err` if the jj
/// command fails.
pub fn advance_singular_bookmark_to_effective_head(
    repo: &Path,
    workspace_path_template: &str,
    repo_name: &str,
    ws_name: &str,
) -> Result<Option<String>> {
    let (bm_name, current_id) =
        match identify_singular_bookmark(repo, workspace_path_template, repo_name, ws_name) {
            Some(pair) => pair,
            None => return Ok(None),
        };
    let effective = find_effective_head(repo, ws_name)?;
    if current_id == effective {
        return Ok(None);
    }
    jujutsu::bookmark_set(repo, &bm_name, &effective)
        .with_context(|| format!("advancing bookmark {bm_name}"))?;
    Ok(Some(bm_name))
}

/// Delete a workspace's singular bookmark.
///
/// Returns `Ok(None)` if no singular bookmark exists. Returns `Ok(Some(name))`
/// on success, `Err` if the jj command fails.
pub fn delete_singular_bookmark(
    repo: &Path,
    workspace_path_template: &str,
    repo_name: &str,
    ws_name: &str,
) -> Result<Option<String>> {
    let (bm_name, _) =
        match identify_singular_bookmark(repo, workspace_path_template, repo_name, ws_name) {
            Some(pair) => pair,
            None => return Ok(None),
        };
    jujutsu::bookmark_delete(repo, &bm_name)
        .with_context(|| format!("deleting bookmark {bm_name}"))?;
    Ok(Some(bm_name))
}

// ---------------------------------------------------------------------------
// .ji directory
// ---------------------------------------------------------------------------

/// Ensure the `.ji` per-workspace directory exists with a `.gitignore`.
///
/// The `.ji` dir holds ji's per-workspace state (logs, diffs, etc.)
/// and is excluded from VCS via an inner `.gitignore`.
pub fn ensure_ji_dir(ws_path: &Path) -> Result<()> {
    let ji_dir = ws_path.join(".ji");
    std::fs::create_dir_all(&ji_dir).context("failed to create .ji directory")?;
    let gitignore = ji_dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, "/*\n").context("failed to write .ji/.gitignore")?;
    }
    Ok(())
}
