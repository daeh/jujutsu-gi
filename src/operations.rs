use anyhow::{Context, Result, bail};
use std::path::Path;

use crate::{jj_utils, jj_utils::WorkspaceHeadInfo, jujutsu};

/// A workspace as the operations layer consumes it: name, on-disk path, and
/// pre-computed head info.
#[derive(Clone, Copy)]
pub struct WsRef<'a> {
    pub name: &'a str,
    pub path: &'a Path,
    pub info: &'a WorkspaceHeadInfo,
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Resolve staleness and forget a workspace.
///
/// Resolves working-copy staleness first so the snapshot can capture pending
/// edits, then forgets the workspace.
pub(crate) fn forget_workspace(repo: &Path, ws_path: &Path, ws_name: &str) -> Result<()> {
    if jujutsu::is_workspace_stale(ws_path) {
        let _ = jujutsu::update_workspace_stale(ws_path);
    }
    jujutsu::workspace_forget(repo, ws_path, ws_name).context("forget failed (jj undo to recover)")
}

/// Format a merge detail string: `"name1@id1 + name2@id2"` (IDs truncated to 4 chars).
fn merge_detail(name1: &str, id1: &str, name2: &str, id2: &str) -> String {
    format!(
        "{}@{} + {}@{}",
        name1,
        &id1[..id1.len().min(4)],
        name2,
        &id2[..id2.len().min(4)],
    )
}

/// Squash a workspace's unique revision chain into a single commit.
///
/// Resolves the squash target (root of `lca..src_eff`), builds a numbered
/// description from each revision, and squashes the chain. Returns the
/// change-id of the squash target for the caller's subsequent merge.
fn squash_chain(
    repo: &Path,
    src_name: &str,
    src_eff: &str,
    tgt_name: &str,
    tgt_eff: &str,
    lca: &str,
) -> Result<String> {
    let unique_chain = format!("{lca}..{src_eff}");
    let squash_target =
        jj_utils::resolve_change_id(repo, &format!("latest(roots({lca}..{src_eff}))"))
            .context("resolve squash target")?;

    let full_descs = jujutsu::revision_descriptions(repo, &unique_chain)
        .context("fetch revision descriptions")?;
    let detail = merge_detail(src_name, src_eff, tgt_name, tgt_eff);
    let mut squash_msg = jj_utils::make_desc(jj_utils::Op::Squash, Some(&detail));
    let total = full_descs.len();
    for (i, (change_id, desc)) in full_descs.iter().rev().enumerate() {
        squash_msg.push_str(&format!(
            "\n\n### (ji::squash) revision {} of {} ({}) ###\n\n{}",
            i + 1,
            total,
            change_id,
            desc
        ));
    }

    jujutsu::squash_into(repo, &unique_chain, &squash_target, &squash_msg)
        .context("squash failed (jj undo to recover)")?;

    Ok(squash_target)
}

/// Abandon trivial heads from the given workspaces (non-critical cleanup).
/// Returns warnings from cleanup failures; bookmarked heads are retained for
/// the command layer to handle according to its bookmark policy.
fn find_abandon_trivial_heads(repo: &Path, workspaces: &[&WorkspaceHeadInfo]) -> Vec<String> {
    let ids: Vec<&str> = workspaces
        .iter()
        .filter_map(|ws| ws.trivial_id.as_deref())
        .collect();
    if ids.is_empty() {
        return Vec::new();
    }
    match jj_utils::abandon_trivial_heads(repo, &ids) {
        Ok(()) => Vec::new(),
        Err(e) => vec![format!("{e:#}")],
    }
}

// ---------------------------------------------------------------------------
// Workspace creation
// ---------------------------------------------------------------------------

/// Create a new workspace branching from `branch_rev`.
///
/// 1. If `branch_rev` is not a graph head, branch directly from it.
/// 2. If it is a head but trivial (empty WIP), branch from its parent.
/// 3. If it is a head with real work, branch from it and step the source
///    workspace forward so the two workspaces don't share the same `@`.
pub fn create_workspace(
    repo: &Path,
    source_ws_path: &Path,
    branch_rev: &str,
    new_ws_path: &Path,
    msg: &str,
    author: Option<&str>,
) -> Result<()> {
    let junction =
        if !jj_utils::is_head(repo, branch_rev).context("check if branch revision is a head")? {
            // Interior revision — branch directly.
            jj_utils::resolve_change_id(repo, branch_rev).context("resolve branch revision")?
        } else if jj_utils::is_trivial_head(repo, branch_rev)
            .context("check if branch revision is trivial")?
        {
            // Trivial head — junction is the parent.
            let parent_revset = format!("({branch_rev})-");
            jj_utils::resolve_change_id(repo, &parent_revset)
                .context("resolve parent of trivial head")?
        } else {
            // Non-trivial head — junction is this revision; step source forward.
            let junction =
                jj_utils::resolve_change_id(repo, branch_rev).context("resolve branch revision")?;
            // Snapshot source before stepping — captures pending edits into current @.
            jujutsu::snapshot_ws(source_ws_path)?;
            let step_msg = jj_utils::make_desc(jj_utils::Op::Step, None);
            jujutsu::progress_workspace(source_ws_path, &step_msg, author)
                .context("step source workspace forward")?;
            junction
        };

    jujutsu::create_workspace(repo, new_ws_path, &junction, msg)
        .context("jj workspace add failed")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

/// Outcome of a sync operation.
#[derive(Debug)]
pub enum SyncOutcome {
    AlreadyInSync,
    /// Sync completed. May carry warnings from non-critical cleanup
    /// (e.g. bookmark moves during trivial-head abandonment).
    Done {
        warnings: Vec<String>,
    },
}

/// Converge two workspaces.
///
/// Uses pre-computed head info to determine the sync strategy (fast-forward
/// or merge) via the last common ancestor, then executes the operation.
///
/// Callers are responsible for resolving workspace staleness afterward.
pub fn sync(
    repo: &Path,
    src: WsRef<'_>,
    tgt: WsRef<'_>,
    author: Option<&str>,
) -> Result<SyncOutcome> {
    let src_head = &src.info.effective_head;
    let tgt_head = &tgt.info.effective_head;

    let lca = jujutsu::last_common_ancestor(repo, src_head, tgt_head)
        .context("failed to find common ancestor")?;

    let src_at_lca = *src_head == lca;
    let tgt_at_lca = *tgt_head == lca;

    if src_at_lca && tgt_at_lca {
        return Ok(SyncOutcome::AlreadyInSync);
    }

    if tgt_at_lca {
        // Safety: target @ is moved to new child of source's head.
        if tgt.info.trivial_id.is_none() {
            let tgt_revset = jj_utils::ws_head_revset(tgt.name);
            if let jj_utils::RevisionSafety::AtRisk { change_id } =
                jj_utils::check_working_copy_safety(repo, &tgt_revset, tgt.path)?
            {
                bail!(
                    "workspace {ws_name} has undescribed work in {change_id}; \
                     describe the revision first",
                    ws_name = tgt.name
                );
            }
        }
        let warnings = fast_forward(repo, tgt, src.name, src_head, author)?;
        let _ = jj_utils::step_head(repo, src.name, src.path, None, author);
        return Ok(SyncOutcome::Done { warnings });
    }

    if src_at_lca {
        // Safety: source @ is moved to new child of target's head.
        if src.info.trivial_id.is_none() {
            let src_revset = jj_utils::ws_head_revset(src.name);
            if let jj_utils::RevisionSafety::AtRisk { change_id } =
                jj_utils::check_working_copy_safety(repo, &src_revset, src.path)?
            {
                bail!(
                    "workspace {ws_name} has undescribed work in {change_id}; \
                     describe the revision first",
                    ws_name = src.name
                );
            }
        }
        let warnings = fast_forward(repo, src, tgt.name, tgt_head, author)?;
        let _ = jj_utils::step_head(repo, tgt.name, tgt.path, None, author);
        return Ok(SyncOutcome::Done { warnings });
    }

    let warnings = merge(repo, src, tgt, author)?;
    Ok(SyncOutcome::Done { warnings })
}

/// Fast-forward the `behind` workspace to `ahead_head`.
///
/// `behind.info.trivial_id` is trusted as pre-computed — avoids a redundant
/// `check_trivial_head` call.
///
/// Callers are responsible for resolving workspace staleness afterward.
pub fn fast_forward(
    repo: &Path,
    behind: WsRef<'_>,
    ahead_name: &str,
    ahead_head: &str,
    author: Option<&str>,
) -> Result<Vec<String>> {
    // 1. Create FF revision — moves behind's @ to a new revision on ahead_head.
    let detail = format!(
        "{behind_name}@ to {ahead_name}@{ahead_head}",
        behind_name = behind.name
    );
    jj_utils::make_head(
        repo,
        behind.name,
        behind.path,
        Some(ahead_head),
        jj_utils::Op::FastForward,
        Some(&detail),
        author,
    )
    .context("fast-forward failed (jj undo to recover)")?;

    // 2. Abandon the old trivial head now that the new structure is in place.
    Ok(find_abandon_trivial_heads(repo, &[behind.info]))
}

/// Merge two workspaces using pre-computed effective heads.
///
/// Callers are responsible for resolving workspace staleness afterward.
pub fn merge(
    repo: &Path,
    src: WsRef<'_>,
    tgt: WsRef<'_>,
    author: Option<&str>,
) -> Result<Vec<String>> {
    let src_eff = &src.info.effective_head;
    let tgt_eff = &tgt.info.effective_head;

    // Snapshot target before mutating (source is handled by new_merge's
    // auto-snapshot — it runs without --ignore-working-copy).
    jujutsu::snapshot_ws(tgt.path)?;

    // 1. Create merge with effective heads as parents.
    let detail = merge_detail(src.name, src_eff, tgt.name, tgt_eff);
    let merge_msg = jj_utils::make_desc(jj_utils::Op::Merge, Some(&detail));
    let merge_id = jujutsu::new_merge(src.path, &[src_eff, tgt_eff], &merge_msg, author)
        .context("merge failed (jj undo to recover)")?;

    // 2. Step both workspaces forward from the merge.
    let step_msg = jj_utils::make_desc(jj_utils::Op::Step, None);
    jujutsu::progress_workspace(src.path, &step_msg, author)
        .with_context(|| format!("step {}", src.name))?;
    jujutsu::new_on_in_workspace(tgt.path, &merge_id, &step_msg, author)
        .with_context(|| format!("step {}", tgt.name))?;

    // 3. Abandon trivial heads (non-critical cleanup).
    let warnings = find_abandon_trivial_heads(repo, &[src.info, tgt.info]);

    Ok(warnings)
}

// ---------------------------------------------------------------------------
// Close (merge into target, forget source)
// ---------------------------------------------------------------------------

/// Merge source into target using pre-computed effective heads, then forget source.
///
/// Callers are responsible for resolving workspace staleness afterward.
pub fn merge_close(
    repo: &Path,
    src: WsRef<'_>,
    tgt: WsRef<'_>,
    author: Option<&str>,
) -> Result<Vec<String>> {
    let src_eff = &src.info.effective_head;
    let tgt_eff = &tgt.info.effective_head;

    // 1. Forget source workspace (snapshots source before forgetting).
    forget_workspace(repo, src.path, src.name)?;

    // 2. Create merge with effective heads as parents (in target workspace).
    // new_merge auto-snapshots target (runs without --ignore-working-copy).
    let detail = merge_detail(src.name, src_eff, tgt.name, tgt_eff);
    let merge_msg = jj_utils::make_desc(jj_utils::Op::Merge, Some(&detail));
    jujutsu::new_merge(tgt.path, &[src_eff, tgt_eff], &merge_msg, author)
        .context("merge failed (jj undo to recover)")?;

    // 3. Abandon trivial heads (non-critical cleanup).
    let warnings = find_abandon_trivial_heads(repo, &[src.info, tgt.info]);

    Ok(warnings)
}

/// Squash source's unique chain, merge with target, then forget source.
///
/// Callers are responsible for resolving workspace staleness afterward.
pub fn merge_squash_close(
    repo: &Path,
    src: WsRef<'_>,
    tgt: WsRef<'_>,
    lca: &str,
    author: Option<&str>,
) -> Result<Vec<String>> {
    let src_eff = &src.info.effective_head;
    let tgt_eff = &tgt.info.effective_head;

    // 1. Forget source workspace (snapshots source before forgetting).
    forget_workspace(repo, src.path, src.name)?;

    // 2-3. Squash unique chain into a single commit.
    let squash_target = squash_chain(repo, src.name, src_eff, tgt.name, tgt_eff, lca)?;

    // 4. Create merge with squashed source and target (in target workspace).
    let detail = merge_detail(src.name, &squash_target, tgt.name, tgt_eff);
    let merge_msg = jj_utils::make_desc(jj_utils::Op::Merge, Some(&detail));
    jujutsu::new_merge(tgt.path, &[&squash_target, tgt_eff], &merge_msg, author)
        .context("merge failed (jj undo to recover)")?;

    // 5. Abandon trivial heads.
    let warnings = find_abandon_trivial_heads(repo, &[src.info, tgt.info]);

    Ok(warnings)
}

/// Fast-forward target to source's effective head, then forget source.
///
/// Target's `@` is reassigned directly to `src_effective_head` with `jj edit`
/// (no new commit). Target's old trivial head, if any, is abandoned.
///
/// Callers are responsible for resolving workspace staleness afterward.
pub fn fast_forward_close(
    repo: &Path,
    src: WsRef<'_>,
    tgt_path: &Path,
    tgt_info: &WorkspaceHeadInfo,
) -> Result<Vec<String>> {
    // 1. Forget source workspace. src_effective_head remains in the graph.
    forget_workspace(repo, src.path, src.name)?;

    // 2. Move target's @ directly onto src_effective_head (no new commit).
    jj_utils::edit_workspace_head(tgt_path, &src.info.effective_head)
        .context("fast-forward failed (jj undo to recover)")?;

    // 3. Abandon target's old trivial head, if any.
    let warnings = find_abandon_trivial_heads(repo, &[tgt_info]);

    Ok(warnings)
}

/// Rebase source's unique chain onto target's effective head.
///
/// Produces linear history: source commits are replayed on top of the target
/// head instead of creating a merge commit. Only the target is stepped forward;
/// the source stays at its rebased effective head.
///
/// Callers are responsible for resolving workspace staleness afterward.
pub fn rebase(
    repo: &Path,
    src_info: &WorkspaceHeadInfo,
    tgt: WsRef<'_>,
    lca: &str,
    author: Option<&str>,
) -> Result<()> {
    let src_eff = &src_info.effective_head;
    let tgt_eff = &tgt.info.effective_head;

    // 1. Rebase source's unique chain onto target's effective head.
    let source_revset = format!("roots({lca}..{src_eff})");
    jujutsu::rebase_source(repo, &source_revset, tgt_eff)
        .context("rebase failed (jj undo to recover)")?;

    // 2. Step target forward (no-op if already has trivial head).
    jj_utils::step_head(repo, tgt.name, tgt.path, None, author)?;

    Ok(())
}

/// Squash source's unique chain into a single commit, then confluence-merge
/// with the target.
///
/// Callers are responsible for resolving workspace staleness afterward.
pub fn merge_squash(
    repo: &Path,
    src: WsRef<'_>,
    tgt: WsRef<'_>,
    lca: &str,
    author: Option<&str>,
) -> Result<Vec<String>> {
    let src_eff = &src.info.effective_head;
    let tgt_eff = &tgt.info.effective_head;

    // Snapshot target before mutating (source is handled by new_merge's
    // auto-snapshot — it runs without --ignore-working-copy).
    jujutsu::snapshot_ws(tgt.path)?;

    // 1-2. Squash unique chain into a single commit.
    let squash_target = squash_chain(repo, src.name, src_eff, tgt.name, tgt_eff, lca)?;

    // 3. Create merge with squashed source (change_id stable) and target.
    let detail = merge_detail(src.name, &squash_target, tgt.name, tgt_eff);
    let merge_msg = jj_utils::make_desc(jj_utils::Op::Merge, Some(&detail));
    let merge_id = jujutsu::new_merge(src.path, &[&squash_target, tgt_eff], &merge_msg, author)
        .context("merge failed (jj undo to recover)")?;

    // 5. Step both workspaces forward from the merge.
    let step_msg = jj_utils::make_desc(jj_utils::Op::Step, None);
    jujutsu::progress_workspace(src.path, &step_msg, author)
        .with_context(|| format!("step {}", src.name))?;
    jujutsu::new_on_in_workspace(tgt.path, &merge_id, &step_msg, author)
        .with_context(|| format!("step {}", tgt.name))?;

    // 6. Abandon both trivial heads.
    // Source trivial head is outside the squash range (beyond src_eff)
    // and persists as an orphan after the squash. Must explicitly abandon.
    let warnings = find_abandon_trivial_heads(repo, &[src.info, tgt.info]);

    Ok(warnings)
}

/// Merge using actual `@` heads, with pre-computed head info.
///
/// Callers are responsible for resolving workspace staleness afterward.
pub fn merge_abandon_parents_old(
    repo: &Path,
    src: WsRef<'_>,
    tgt: WsRef<'_>,
    author: Option<&str>,
) -> Result<Vec<String>> {
    let src_head = &src.info.actual_head;
    let tgt_head = &tgt.info.actual_head;
    let src_eff = &src.info.effective_head;
    let tgt_eff = &tgt.info.effective_head;

    // Snapshot target before mutating (source is handled by new_merge's
    // auto-snapshot — it runs without --ignore-working-copy).
    jujutsu::snapshot_ws(tgt.path)?;

    // 1. Create merge with actual heads as parents.
    let merge_detail = format!(
        "{tgt_name}@{tgt_eff} into {src_name}@{src_eff}",
        tgt_name = tgt.name,
        src_name = src.name
    );
    let merge_msg = jj_utils::make_desc(jj_utils::Op::Merge, Some(&merge_detail));
    let merge_id = jujutsu::new_merge(src.path, &[src_head, tgt_head], &merge_msg, author)
        .context("merge failed (jj undo to recover)")?;

    // 2. Step both workspaces forward from the merge.
    let step_msg = jj_utils::make_desc(jj_utils::Op::Step, None);
    jujutsu::new_on_in_workspace(src.path, &merge_id, &step_msg, author)
        .with_context(|| format!("step {}", src.name))?;
    jujutsu::new_on_in_workspace(tgt.path, &merge_id, &step_msg, author)
        .with_context(|| format!("step {}", tgt.name))?;

    // 3. Abandon trivial heads — jj rebases the merge onto their parents.
    let warnings = find_abandon_trivial_heads(repo, &[src.info, tgt.info]);

    Ok(warnings)
}
