pub mod close;
pub mod create;
pub mod switch;
pub mod sync;
pub mod transfer;
pub mod types;

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

use crate::{jj_utils, jujutsu};
use types::{Operation, SyncMode, SyncModeInfo};

// ---------------------------------------------------------------------------
// Sync mode detection
// ---------------------------------------------------------------------------

/// Detect the sync state between source and target workspaces.
///
/// Resolves head info for both workspaces, computes the LCA, and
/// determines the sync mode. Captures the current op head for later
/// validation.
pub fn detect_sync_mode(repo: &Path, src_name: &str, tgt_name: &str) -> SyncModeInfo {
    let err = |msg: String| SyncModeInfo {
        mode: SyncMode::Error(msg),
        src_effective_head: String::new(),
        tgt_effective_head: String::new(),
        src_actual_head: String::new(),
        tgt_actual_head: String::new(),
        src_trivial_id: None,
        tgt_trivial_id: None,
        lca: String::new(),
        op_head: String::new(),
    };

    let src_info = match jj_utils::resolve_workspace_head_info(repo, src_name) {
        Ok(info) if !info.effective_head.is_empty() => info,
        Ok(_) => return err(format!("empty head for {src_name}")),
        Err(e) => return err(format!("{src_name}: {e:#}")),
    };
    let tgt_info = match jj_utils::resolve_workspace_head_info(repo, tgt_name) {
        Ok(info) if !info.effective_head.is_empty() => info,
        Ok(_) => return err(format!("empty head for {tgt_name}")),
        Err(e) => return err(format!("{tgt_name}: {e:#}")),
    };
    let lca = match jujutsu::last_common_ancestor(
        repo,
        &src_info.effective_head,
        &tgt_info.effective_head,
    ) {
        Ok(m) if !m.is_empty() => m,
        Ok(_) => return err("no common ancestor".into()),
        Err(e) => return err(format!("lca: {e:#}")),
    };

    let mode = SyncMode::from_heads(
        src_info.effective_head == lca,
        tgt_info.effective_head == lca,
    );

    SyncModeInfo {
        mode,
        src_effective_head: src_info.effective_head,
        tgt_effective_head: tgt_info.effective_head,
        src_actual_head: src_info.actual_head,
        tgt_actual_head: tgt_info.actual_head,
        src_trivial_id: src_info.trivial_id,
        tgt_trivial_id: tgt_info.trivial_id,
        lca,
        op_head: jujutsu::current_op_head(repo).unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate that cached head info is still current.
///
/// If the repo's op head hasn't changed, returns the cached head info.
/// If it has changed, recomputes and verifies the sync mode hasn't shifted.
pub fn validate_head_info(
    repo: &Path,
    info: &SyncModeInfo,
    src_name: &str,
    tgt_name: &str,
) -> Result<(jj_utils::WorkspaceHeadInfo, jj_utils::WorkspaceHeadInfo)> {
    let current_op = jujutsu::current_op_head(repo).unwrap_or_default();
    if current_op == info.op_head {
        return Ok((info.src_head_info(), info.tgt_head_info()));
    }
    // Op head changed — recompute and verify mode hasn't shifted.
    let src = jj_utils::resolve_workspace_head_info(repo, src_name)?;
    let tgt = jj_utils::resolve_workspace_head_info(repo, tgt_name)?;
    let lca = jujutsu::last_common_ancestor(repo, &src.effective_head, &tgt.effective_head)?;
    let new_mode = SyncMode::from_heads(src.effective_head == lca, tgt.effective_head == lca);
    if std::mem::discriminant(&new_mode) != std::mem::discriminant(&info.mode) {
        bail!("repo changed externally, please retry");
    }
    Ok((src, tgt))
}

// ---------------------------------------------------------------------------
// Adaptive resolution
// ---------------------------------------------------------------------------

/// Resolve adaptive close: SyncMode -> concrete Operation.
pub fn resolve_adaptive_close(mode: &SyncMode) -> Result<Operation> {
    match mode {
        SyncMode::SourceOnly => Ok(Operation::FastForwardTargetClose),
        SyncMode::Diverged => Ok(Operation::MergeClose),
        _ => bail!("adaptive close unavailable for current sync state"),
    }
}

/// Resolve adaptive transfer: SyncMode -> concrete Operation.
pub fn resolve_adaptive_transfer(mode: &SyncMode) -> Result<Operation> {
    match mode {
        SyncMode::SourceOnly => Ok(Operation::FastForwardTarget),
        SyncMode::TargetOnly => Ok(Operation::FastForwardSource),
        SyncMode::Diverged => Ok(Operation::Merge),
        _ => bail!("adaptive merge unavailable for current sync state"),
    }
}

// ---------------------------------------------------------------------------
// Post-operation cleanup
// ---------------------------------------------------------------------------

/// Resolve stale workspaces and report any that remain stale.
///
/// Returns human-readable warning strings for workspaces that are still
/// stale after the fix attempt.
pub fn post_op_stale(ws_paths: &[(String, PathBuf)], pre_stale: &[String]) -> Vec<String> {
    // Re-check all workspaces and report anything still stale.
    let post_stale: Vec<String> = jujutsu::stale_workspace_names(ws_paths);
    let mut warnings = Vec::new();
    for ws_name in &post_stale {
        if pre_stale.contains(ws_name) {
            warnings.push(format!("{ws_name} (was already stale)"));
        } else {
            warnings.push(format!("{ws_name} (unexpected)"));
        }
    }
    warnings
}

/// Predict which third-party workspaces will become stale from a rewrite operation.
///
/// Must be called BEFORE the operation executes (queries pre-op graph state).
/// Best-effort: returns empty on query failure rather than blocking the operation.
pub fn predict_stale_workspaces(
    repo: &Path,
    operation: Operation,
    info: &SyncModeInfo,
    abandoned_ids: &[&str],
    source_name: &str,
    target_name: &str,
) -> Vec<String> {
    let revset = match operation {
        Operation::Rebase | Operation::MergeSquash | Operation::MergeSquashClose => {
            if info.lca == info.src_effective_head {
                return vec![];
            }
            format!("roots({}..{})", info.lca, info.src_effective_head)
        }
        Operation::Abandon => {
            if abandoned_ids.is_empty() {
                return vec![];
            }
            abandoned_ids.join(" | ")
        }
        _ => return vec![],
    };

    match jujutsu::workspaces_in_descendants(repo, &revset) {
        Ok(names) => names
            .into_iter()
            .filter(|n| n != source_name && n != target_name)
            .collect(),
        Err(_) => vec![],
    }
}

/// Snapshot predicted-stale workspaces before a rewriting operation.
///
/// Captures pending working-copy edits into each workspace's `@` so they
/// survive the auto-rebase that the operation will trigger.
pub fn snapshot_predicted_stale(predicted: &[String], all_ws_paths: &[(String, PathBuf)]) {
    for (name, path) in all_ws_paths {
        if predicted.contains(name) && !path.as_os_str().is_empty() && path.exists() {
            let _ = jujutsu::snapshot_ws(path);
        }
    }
}

/// Resolve predicted stale workspaces after an operation.
///
/// Calls `update_workspace_stale` on each workspace that was predicted to
/// become stale. Skips workspaces whose path is empty or missing.
pub fn resolve_predicted_stale(predicted: &[String], all_ws_paths: &[(String, PathBuf)]) {
    for (name, path) in all_ws_paths {
        if predicted.contains(name) && !path.as_os_str().is_empty() && path.exists() {
            let _ = jujutsu::update_workspace_stale(path);
        }
    }
}

/// Result of post-op bookmark handling.
pub struct BookmarkResult {
    /// Non-singular bookmarks not auto-handled (caller may apply manual action).
    pub remaining: Vec<String>,
    /// Warnings from bookmark operations that failed.
    pub warnings: Vec<String>,
}

/// Handle singular bookmarks after a close/transfer operation.
///
/// `src_effective_head_override` allows the caller to supply a fresh effective
/// head (e.g. for Detach, where the source workspace has been forgotten and
/// the cached value in `close_info` cannot be re-validated).
#[allow(clippy::too_many_arguments)]
pub fn post_op_bookmarks(
    repo: &Path,
    operation: Operation,
    source_name: &str,
    target_name: &str,
    close_info: Option<&SyncModeInfo>,
    workspace_path_template: &str,
    repo_name: &str,
    mut bookmarks: Vec<String>,
    src_effective_head_override: Option<&str>,
) -> BookmarkResult {
    let mut warnings = Vec::new();

    /// Collect a bookmark-operation result into the warnings list.
    fn collect(result: anyhow::Result<Option<String>>, warnings: &mut Vec<String>) {
        if let Err(e) = result {
            warnings.push(format!("{e:#}"));
        }
    }

    match operation {
        // Close: source forgotten + revisions abandoned -> delete bookmark.
        Operation::Abandon => {
            collect(
                jj_utils::delete_singular_bookmark(
                    repo,
                    workspace_path_template,
                    repo_name,
                    source_name,
                ),
                &mut warnings,
            );
        }
        // Close: source forgotten, revisions remain (detach or merged).
        Operation::MergeClose
        | Operation::MergeSquashClose
        | Operation::FastForwardTargetClose
        | Operation::Detach => {
            // Use override if provided (Detach), else fall back to cached info.
            let src_head = src_effective_head_override
                .map(String::from)
                .or_else(|| close_info.map(|i| i.src_effective_head.clone()));
            if let Some(head) = src_head {
                collect(
                    jj_utils::advance_singular_bookmark(
                        repo,
                        workspace_path_template,
                        repo_name,
                        source_name,
                        &head,
                    ),
                    &mut warnings,
                );
            }
            // Advance target bookmark for structure-preserving ops.
            if matches!(
                operation,
                Operation::MergeClose
                    | Operation::MergeSquashClose
                    | Operation::FastForwardTargetClose
            ) {
                collect(
                    jj_utils::advance_singular_bookmark_to_effective_head(
                        repo,
                        workspace_path_template,
                        repo_name,
                        target_name,
                    ),
                    &mut warnings,
                );
            }
        }
        // Transfer: both workspaces survive -> advance both to effective head.
        Operation::Merge
        | Operation::FastForwardTarget
        | Operation::FastForwardSource
        | Operation::MergeAbandonOld
        | Operation::Rebase
        | Operation::MergeSquash => {
            for ws_name in [source_name, target_name] {
                collect(
                    jj_utils::advance_singular_bookmark_to_effective_head(
                        repo,
                        workspace_path_template,
                        repo_name,
                        ws_name,
                    ),
                    &mut warnings,
                );
            }
        }
        // Adaptive* operations resolve before reaching here.
        Operation::AdaptiveMerge | Operation::AdaptiveClose => {}
    }

    // Remove singular bookmark from manual list (auto-handled above).
    if let Some((bm_name, _)) =
        jj_utils::identify_singular_bookmark(repo, workspace_path_template, repo_name, source_name)
    {
        bookmarks.retain(|bm| *bm != bm_name);
    }

    BookmarkResult {
        remaining: bookmarks,
        warnings,
    }
}

/// Apply manual bookmark action (close only, for remaining non-singular bookmarks).
///
/// Returns warnings for any bookmark operations that failed.
pub fn post_op_manual_bookmarks(
    repo: &Path,
    bookmarks: &[String],
    action: types::BookmarkAction,
    target_change_id: &str,
) -> Vec<String> {
    let mut warnings = Vec::new();
    match action {
        types::BookmarkAction::Advance => {
            for bm in bookmarks {
                if let Err(e) = jujutsu::bookmark_set(repo, bm, target_change_id) {
                    warnings.push(format!("advancing {bm}: {e:#}"));
                }
            }
        }
        types::BookmarkAction::Delete => {
            for bm in bookmarks {
                if let Err(e) = jujutsu::bookmark_delete(repo, bm) {
                    warnings.push(format!("deleting {bm}: {e:#}"));
                }
            }
        }
        types::BookmarkAction::NoAction => {}
    }
    warnings
}
