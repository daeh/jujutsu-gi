use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

use crate::{jj_utils, jujutsu, jujutsu::RevisionInfo, operations};

use super::types::{BookmarkAction, CloseMethod, CloseTransferResult, Operation, SyncModeInfo};

/// Parameters for a close operation.
pub struct CloseParams<'a> {
    pub repo_root: &'a Path,
    pub source_name: &'a str,
    pub source_path: &'a Path,
    pub target_name: &'a str,
    pub target_path: &'a Path,
    pub target_change_id: &'a str,
    pub method: CloseMethod,
    pub delete_files: bool,
    pub bookmark_action: BookmarkAction,
    pub bookmarks: Vec<String>,
    pub revisions: &'a [RevisionInfo],
    pub workspace_path_template: &'a str,
    pub repo_name: &'a str,
    pub author: Option<&'a str>,
    /// All workspace (name, path) pairs for staleness tracking.
    pub all_ws_paths: &'a [(String, PathBuf)],
}

/// Owned-data mirror of `CloseParams`, used by the deferred-handoff path
/// in the TUI (see `PendingHandoff::Close`). The drain block destructures
/// this and constructs a `CloseParams<'_>` inline: `bookmarks` is moved
/// out (`Vec<String>` → `Vec<String>`); `revisions` and `all_ws_paths`
/// are borrowed via `&owned.foo` (which deref-coerces `&Vec<T>` to
/// `&[T]`); the remaining `&'a str` / `&'a Path` fields are also
/// borrowed; `author: Option<String>` becomes `author.as_deref()`.
pub struct CloseParamsOwned {
    pub repo_root: PathBuf,
    pub source_name: String,
    pub source_path: PathBuf,
    pub target_name: String,
    pub target_path: PathBuf,
    pub target_change_id: String,
    pub method: CloseMethod,
    pub delete_files: bool,
    pub bookmark_action: BookmarkAction,
    pub bookmarks: Vec<String>,
    pub revisions: Vec<RevisionInfo>,
    pub workspace_path_template: String,
    pub repo_name: String,
    pub author: Option<String>,
    pub all_ws_paths: Vec<(String, PathBuf)>,
}

/// Execute close, computing sync info fresh.
pub fn close(params: &CloseParams<'_>) -> Result<CloseTransferResult> {
    let info = super::detect_sync_mode(params.repo_root, params.source_name, params.target_name);
    close_with_info(params, &info)
}

/// Execute close with pre-computed sync info.
pub fn close_with_info(
    params: &CloseParams<'_>,
    info: &SyncModeInfo,
) -> Result<CloseTransferResult> {
    let repo = params.repo_root;

    // Resolve adaptive to concrete operation.
    let operation = match params.method {
        CloseMethod::Adaptive => super::resolve_adaptive_close(&info.mode)?,
        CloseMethod::Merge => Operation::MergeClose,
        CloseMethod::SquashMerge => Operation::MergeSquashClose,
        CloseMethod::FastForward => Operation::FastForwardTargetClose,
        CloseMethod::Detach => Operation::Detach,
        CloseMethod::Abandon => Operation::Abandon,
    };

    // Validate head info (not needed for disposal ops).
    let head_info = match operation {
        Operation::Detach | Operation::Abandon => None,
        _ => Some(super::validate_head_info(
            repo,
            info,
            params.source_name,
            params.target_name,
        )?),
    };

    // Predict which third-party workspaces will become stale (pre-op query).
    let abandoned_ids: Vec<&str> = match operation {
        Operation::Abandon => params
            .revisions
            .iter()
            .map(|r| r.change_id.as_str())
            .collect(),
        _ => vec![],
    };
    let predicted_stale = super::predict_stale_workspaces(
        repo,
        operation,
        info,
        &abandoned_ids,
        params.source_name,
        params.target_name,
    );
    super::snapshot_predicted_stale(&predicted_stale, params.all_ws_paths);

    // Staleness snapshot: record which workspaces are already stale.
    let pre_stale: Vec<String> = jujutsu::stale_workspace_names(params.all_ws_paths);

    // Detach resolves the source effective head before forget.
    let mut detach_src_head: Option<String> = None;

    // Warnings from non-critical cleanup during operations.
    let mut op_warnings: Vec<String> = Vec::new();

    // Execute the operation.
    match operation {
        Operation::MergeClose => {
            let (ref src_hi, ref tgt_hi) = head_info.unwrap();
            // Safety: source @ will lose workspace status. Warn if undescribed work.
            let src_revset = jj_utils::ws_head_revset(params.source_name);
            if let jj_utils::RevisionSafety::AtRisk { change_id } =
                jj_utils::check_revision_safety(repo, &src_revset, Some(params.source_path))?
            {
                op_warnings.push(format!(
                    "source workspace {} has undescribed work in {change_id}; \
                     it will be preserved as a merge parent but may be hard to find",
                    params.source_name,
                ));
            }
            op_warnings.extend(operations::merge_close(
                repo,
                params.source_name,
                params.source_path,
                src_hi,
                params.target_name,
                params.target_path,
                tgt_hi,
                params.author,
            )?);
            // Resolve target staleness from the --ignore-working-copy mutation
            // (see sync.rs / transfer.rs for the full rationale).
            let _ = jujutsu::update_workspace_stale(params.target_path);
        }
        Operation::MergeSquashClose => {
            let (ref src_hi, ref tgt_hi) = head_info.unwrap();
            // Safety: source @ will lose workspace status. Warn if undescribed work.
            let src_revset = jj_utils::ws_head_revset(params.source_name);
            if let jj_utils::RevisionSafety::AtRisk { change_id } =
                jj_utils::check_revision_safety(repo, &src_revset, Some(params.source_path))?
            {
                op_warnings.push(format!(
                    "source workspace {} has undescribed work in {change_id}; \
                     it will be squashed but the revision may be hard to find",
                    params.source_name,
                ));
            }
            op_warnings.extend(operations::merge_squash_close(
                repo,
                params.source_name,
                params.source_path,
                src_hi,
                params.target_name,
                params.target_path,
                tgt_hi,
                &info.lca,
                params.author,
            )?);
            // Resolve target staleness from the --ignore-working-copy mutation.
            let _ = jujutsu::update_workspace_stale(params.target_path);
        }
        Operation::FastForwardTargetClose => {
            let (ref src_hi, ref tgt_hi) = head_info.unwrap();
            // Safety: target @ is displaced by jj edit. The head check would
            // short-circuit (target always has source-chain descendants), so
            // use check_working_copy_safety which skips it.
            if tgt_hi.trivial_id.is_none() {
                let tgt_revset = jj_utils::ws_head_revset(params.target_name);
                if let jj_utils::RevisionSafety::AtRisk { change_id } =
                    jj_utils::check_working_copy_safety(repo, &tgt_revset, params.target_path)?
                {
                    bail!(
                        "target workspace {} has undescribed work in {change_id}; \
                         describe the revision first, or use a merge-close instead",
                        params.target_name,
                    );
                }
            }
            op_warnings = operations::fast_forward_close(
                repo,
                params.source_name,
                params.source_path,
                src_hi,
                params.target_path,
                tgt_hi,
            )?;
            // Resolve target staleness from the jj edit (Live policy) — target
            // @ was reassigned, source workspace's WC operation lags.
            let _ = jujutsu::update_workspace_stale(params.target_path);
        }
        Operation::Detach => {
            // Safety: source @ will lose workspace status.
            let src_revset = jj_utils::ws_head_revset(params.source_name);
            if let jj_utils::RevisionSafety::AtRisk { change_id } =
                jj_utils::check_revision_safety(repo, &src_revset, Some(params.source_path))?
            {
                bail!(
                    "workspace {} has undescribed work in {change_id}; \
                     describe the revision before detaching",
                    params.source_name,
                );
            }
            // Resolve effective head BEFORE forget — workspace won't exist after.
            detach_src_head = Some(jj_utils::find_effective_head(repo, params.source_name)?);
            operations::forget_workspace(repo, params.source_path, params.source_name)?;
        }
        Operation::Abandon => {
            // Safety: entire unique chain is destroyed. Snapshot + check all revisions.
            let ids: Vec<&str> = params
                .revisions
                .iter()
                .map(|r| r.change_id.as_str())
                .collect();
            let at_risk = jj_utils::check_chain_safety(repo, &ids, Some(params.source_path))?;
            if !at_risk.is_empty() {
                bail!(
                    "refusing to abandon: revisions {} have work with no description; \
                     describe them first",
                    at_risk.join(", "),
                );
            }
            operations::forget_workspace(repo, params.source_path, params.source_name)?;
            if !ids.is_empty() {
                // After forget, some revisions may have been auto-abandoned
                // by jj (e.g. empty undescribed working-copy commits that lost
                // workspace protection). Only abandon those that still exist.
                let surviving = jj_utils::filter_existing_revisions(repo, &ids)?;
                if !surviving.is_empty() {
                    jujutsu::abandon_revisions(repo, &surviving)?;
                }
            }
        }
        _ => bail!("unexpected operation for close: {operation:?}"),
    }

    // Post-op: resolve predicted third-party stale workspaces.
    super::resolve_predicted_stale(&predicted_stale, params.all_ws_paths);

    // Post-op: staleness warnings + operation warnings.
    let mut stale_warnings = super::post_op_stale(params.all_ws_paths, &pre_stale);
    stale_warnings.extend(op_warnings);

    // Post-op: singular bookmarks.
    let bm_result = super::post_op_bookmarks(
        repo,
        operation,
        params.source_name,
        params.target_name,
        Some(info),
        params.workspace_path_template,
        params.repo_name,
        params.bookmarks.clone(),
        detach_src_head.as_deref(),
    );
    stale_warnings.extend(bm_result.warnings);

    // Post-op: manual bookmark action for remaining non-singular bookmarks.
    if !bm_result.remaining.is_empty() {
        let manual_warnings = super::post_op_manual_bookmarks(
            repo,
            &bm_result.remaining,
            params.bookmark_action,
            params.target_change_id,
        );
        stale_warnings.extend(manual_warnings);
    }

    // Determine if file removal should be prompted.
    let pending_remove_path = if params.delete_files
        && !params.source_path.as_os_str().is_empty()
        && params.source_path.exists()
    {
        Some(params.source_path.to_path_buf())
    } else {
        None
    };

    Ok(CloseTransferResult {
        operation_used: operation,
        stale_warnings,
        predicted_stale,
        source_forgotten: true,
        pending_remove_path,
    })
}
