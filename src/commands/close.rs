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
}

/// Owned-data mirror of `CloseParams`, used by the deferred-handoff path
/// in the TUI (see `PendingHandoff::Close`). The drain block destructures
/// this and constructs a `CloseParams<'_>` inline: `bookmarks` is moved
/// out (`Vec<String>` → `Vec<String>`); `revisions` is borrowed via
/// `&owned.revisions` (which deref-coerces `&Vec<T>` to `&[T]`); the
/// remaining `&'a str` / `&'a Path` fields are also borrowed;
/// `author: Option<String>` becomes `author.as_deref()`.
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

    // Sync-state-consuming methods cannot run off an Error-mode info.
    // Disposal ops (Detach/Abandon) don't consume sync state and may proceed
    // with one — their freshness is still validated below (Error infos carry
    // the real op head).
    if !matches!(operation, Operation::Detach | Operation::Abandon)
        && let super::types::SyncMode::Error(e) = &info.mode
    {
        bail!("sync state could not be determined: {e}");
    }

    // Validate head info — strict mode for ALL close methods, including
    // Detach/Abandon (previously exempt): the close plan spans revisions,
    // bookmarks, and target entries beyond SyncModeInfo, so any op-head
    // movement since `info` was computed bails rather than executing a
    // possibly divergent plan. Heads AND lca come from the validated info.
    let validated =
        super::validate_head_info(repo, info, params.source_name, params.target_name, false)?;

    // Execution-time freshness + protection (all workspaces, fail-loud):
    // broad descendant-first snapshot + snapshot-only revalidation. Disposal
    // ops (Detach/Abandon) consume only the source and run on possibly
    // Error-mode info, so their target is not involved and the plan is not
    // re-proven via `plan_equivalent` (they re-verify the live source).
    let disposal = matches!(operation, Operation::Detach | Operation::Abandon);
    let advance_id = (params.bookmark_action == BookmarkAction::Advance
        && !params.target_change_id.is_empty())
    .then_some(params.target_change_id);
    let fresh = super::prepare_execution_freshness(
        repo,
        &validated,
        params.source_name,
        params.source_path,
        params.target_name,
        params.target_path,
        !disposal,
        !disposal,
        advance_id,
    )?;
    let validated = fresh.info;
    let src_hi = validated.src_head_info();
    let tgt_hi = validated.tgt_head_info();
    let src = operations::WsRef {
        name: params.source_name,
        path: params.source_path,
        info: &src_hi,
    };
    let tgt = operations::WsRef {
        name: params.target_name,
        path: params.target_path,
        info: &tgt_hi,
    };

    // Predict which third-party workspaces will become stale (post-phase,
    // pre-op query; reporting/resolution only — edit survival is already
    // covered by the protection snapshot above).
    let abandoned_ids: Vec<&str> = match operation {
        Operation::Abandon => params
            .revisions
            .iter()
            .map(|r| r.change_id.as_str())
            .collect(),
        _ => vec![],
    };
    // Abandon's target is not involved: a workspace descending from the
    // abandoned revisions must stay in the resolution/report set even if it
    // is the named target.
    let involved: Vec<&str> = if matches!(operation, Operation::Abandon) {
        vec![params.source_name]
    } else {
        vec![params.source_name, params.target_name]
    };
    let predicted_stale =
        super::predict_stale_workspaces(repo, operation, &validated, &abandoned_ids, &involved);

    // Detach resolves the source effective head before forget.
    let mut detach_src_head: Option<String> = None;

    // Warnings from non-critical cleanup during operations.
    let mut op_warnings: Vec<String> = Vec::new();

    // Execute the operation.
    match operation {
        Operation::MergeClose => {
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
            op_warnings.extend(operations::merge_close(repo, src, tgt, params.author)?);
            // Resolve target staleness from the --ignore-working-copy mutation
            // (see sync.rs / transfer.rs for the full rationale).
            let _ = jujutsu::update_workspace_stale(params.target_path);
        }
        Operation::MergeSquashClose => {
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
                src,
                tgt,
                &validated.lca,
                params.author,
            )?);
            // Resolve target staleness from the --ignore-working-copy mutation.
            let _ = jujutsu::update_workspace_stale(params.target_path);
        }
        Operation::FastForwardTargetClose => {
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
            op_warnings = operations::fast_forward_close(repo, src, params.target_path, &tgt_hi)?;
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
            // The frozen `revisions` list was computed when the dialog/CLI
            // gathered state. Interior-chain inserts keep all HEAD change-ids
            // stable (change ids survive rewrites), so op-head/plan checks
            // cannot see a grown chain — verify the live set against the
            // frozen set with the same producer definition (default-relative)
            // before destroying anything.
            let live = jj_utils::workspace_unique_change_ids(repo, params.source_name)?;
            let live_set: std::collections::HashSet<&str> =
                live.iter().map(String::as_str).collect();
            let frozen_set: std::collections::HashSet<&str> = params
                .revisions
                .iter()
                .map(|r| r.change_id.as_str())
                .collect();
            if live_set != frozen_set {
                bail!("source revisions changed, please retry");
            }
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

    // Post-op: resolve predicted third-party stale workspaces — excluding
    // any that were already stale pre-op (the protection phase's skip-set);
    // those belong to the update-stale workflow and surface in the report.
    let to_resolve: Vec<String> = predicted_stale
        .iter()
        .filter(|n| !fresh.stale_skipped.contains(n))
        .cloned()
        .collect();
    super::resolve_predicted_stale(&to_resolve, &fresh.ws_paths);

    // Post-op: staleness warnings + operation warnings.
    let mut stale_warnings = super::post_op_stale(&fresh.ws_paths, &fresh.stale_skipped);
    stale_warnings.extend(op_warnings);

    // Post-op: singular bookmarks.
    let bm_result = super::post_op_bookmarks(
        repo,
        operation,
        params.source_name,
        params.target_name,
        Some(&validated),
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
        stale_warnings,
        predicted_stale,
        pending_remove_path,
    })
}
