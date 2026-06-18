use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

use crate::{jj_utils, jujutsu, operations};

use super::types::{CloseTransferResult, Operation, SyncModeInfo, TransferMethod};

/// Parameters for a transfer operation.
pub struct TransferParams<'a> {
    pub repo_root: &'a Path,
    pub source_name: &'a str,
    pub source_path: &'a Path,
    pub target_name: &'a str,
    pub target_path: &'a Path,
    pub method: TransferMethod,
    pub workspace_path_template: &'a str,
    pub repo_name: &'a str,
    pub author: Option<&'a str>,
}

/// Owned-data mirror of `TransferParams` for the deferred-handoff path
/// (see `PendingHandoff::Transfer`).
pub struct TransferParamsOwned {
    pub repo_root: PathBuf,
    pub source_name: String,
    pub source_path: PathBuf,
    pub target_name: String,
    pub target_path: PathBuf,
    pub method: TransferMethod,
    pub workspace_path_template: String,
    pub repo_name: String,
    pub author: Option<String>,
}

/// Execute transfer, computing sync info fresh.
pub fn transfer(params: &TransferParams<'_>) -> Result<CloseTransferResult> {
    let info = super::detect_sync_mode(params.repo_root, params.source_name, params.target_name);
    transfer_with_info(params, &info)
}

/// Execute transfer with pre-computed sync info.
pub fn transfer_with_info(
    params: &TransferParams<'_>,
    info: &SyncModeInfo,
) -> Result<CloseTransferResult> {
    let repo = params.repo_root;

    // Resolve adaptive to concrete operation.
    let operation = match params.method {
        TransferMethod::Adaptive => super::resolve_adaptive_transfer(&info.mode)?,
        TransferMethod::Merge => Operation::Merge,
        TransferMethod::FastForwardTarget => Operation::FastForwardTarget,
        TransferMethod::FastForwardSource => Operation::FastForwardSource,
        TransferMethod::MergeAbandonOld => Operation::MergeAbandonOld,
        TransferMethod::Rebase => Operation::Rebase,
        TransferMethod::MergeSquash => Operation::MergeSquash,
    };

    // Strict validation: the transfer plan spans more than SyncModeInfo
    // (target entries, selected method semantics), so any op-head movement
    // since `info` was computed bails. Heads AND lca come from the
    // validated info — never from the possibly stale `info`.
    let validated =
        super::validate_head_info(repo, info, params.source_name, params.target_name, false)?;

    // Execution-time freshness + protection (all workspaces, fail-loud):
    // broad descendant-first snapshot + snapshot-only revalidation.
    let fresh = super::prepare_execution_freshness(
        repo,
        &validated,
        params.source_name,
        params.source_path,
        params.target_name,
        params.target_path,
        true,
        true,
        None,
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
    let predicted_stale = super::predict_stale_workspaces(
        repo,
        operation,
        &validated,
        &[],
        &[params.source_name, params.target_name],
    );

    // Warnings from non-critical cleanup during operations.
    let mut op_warnings: Vec<String> = Vec::new();

    // Execute the operation.
    match operation {
        Operation::FastForwardTarget => {
            // Safety: target @ is moved to a new child of source's head. The old
            // target @ stays as an ancestor, but unsnapshot'd edits would be
            // attributed to the new revision.
            if tgt_hi.trivial_id.is_none() {
                let tgt_revset = jj_utils::ws_head_revset(params.target_name);
                if let jj_utils::RevisionSafety::AtRisk { change_id } =
                    jj_utils::check_working_copy_safety(repo, &tgt_revset, params.target_path)?
                {
                    bail!(
                        "target workspace {} has undescribed work in {change_id}; \
                         describe the revision first",
                        params.target_name,
                    );
                }
            }
            op_warnings = operations::fast_forward(
                repo,
                tgt,
                src.name,
                &src.info.effective_head,
                params.author,
            )?;
            // Step source forward so both have fresh @.
            let _ = jj_utils::step_head(
                repo,
                params.source_name,
                params.source_path,
                None,
                params.author,
            );
        }
        Operation::FastForwardSource => {
            // Safety: source @ is moved to a new child of target's head.
            if src_hi.trivial_id.is_none() {
                let src_revset = jj_utils::ws_head_revset(params.source_name);
                if let jj_utils::RevisionSafety::AtRisk { change_id } =
                    jj_utils::check_working_copy_safety(repo, &src_revset, params.source_path)?
                {
                    bail!(
                        "source workspace {} has undescribed work in {change_id}; \
                         describe the revision first",
                        params.source_name,
                    );
                }
            }
            op_warnings = operations::fast_forward(
                repo,
                src,
                tgt.name,
                &tgt.info.effective_head,
                params.author,
            )?;
            // Step target forward so both have fresh @.
            let _ = jj_utils::step_head(
                repo,
                params.target_name,
                params.target_path,
                None,
                params.author,
            );
        }
        Operation::Merge => {
            op_warnings = operations::merge(repo, src, tgt, params.author)?;
        }
        Operation::MergeAbandonOld => {
            op_warnings = operations::merge_abandon_parents_old(repo, src, tgt, params.author)?;
        }
        Operation::Rebase => {
            operations::rebase(repo, src.info, tgt, &validated.lca, params.author)?;
        }
        Operation::MergeSquash => {
            op_warnings = operations::merge_squash(repo, src, tgt, &validated.lca, params.author)?;
        }
        _ => bail!("unexpected operation for transfer: {operation:?}"),
    }

    // Resolve the legitimate stale state created when each workspace's `@`
    // was rewritten from the other via --ignore-working-copy. Not a hedge
    // against jj 0.41's reduced sibling-op false positives (#9314).
    let _ = jujutsu::update_workspace_stale(params.source_path);
    let _ = jujutsu::update_workspace_stale(params.target_path);
    // Resolve predicted third-party stale workspaces — excluding any that
    // were already stale pre-op (the protection phase's skip-set).
    let to_resolve: Vec<String> = predicted_stale
        .iter()
        .filter(|n| !fresh.stale_skipped.contains(n))
        .cloned()
        .collect();
    super::resolve_predicted_stale(&to_resolve, &fresh.ws_paths);

    // Post-op: staleness warnings + operation warnings.
    let mut stale_warnings = super::post_op_stale(&fresh.ws_paths, &fresh.stale_skipped);
    stale_warnings.extend(op_warnings);

    // Post-op: singular bookmarks (both survive → advance both to effective head).
    let bm_result = super::post_op_bookmarks(
        repo,
        operation,
        params.source_name,
        params.target_name,
        Some(&validated),
        params.workspace_path_template,
        params.repo_name,
        Vec::new(),
        None,
    );
    stale_warnings.extend(bm_result.warnings);

    Ok(CloseTransferResult {
        stale_warnings,
        predicted_stale,
        pending_remove_path: None,
    })
}
