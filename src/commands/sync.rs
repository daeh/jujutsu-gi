use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::{jj_utils, jujutsu, operations};

use super::types::SyncModeInfo;

/// Owned-data mirror of the borrow-typed args to `sync_with_info`, used to
/// shuttle a deferred sync invocation from a TUI key-handler into the
/// `run_tui` drain block (see `PendingHandoff::Sync`).
pub struct SyncParamsOwned {
    pub repo_root: PathBuf,
    pub info: SyncModeInfo,
    pub source_name: String,
    pub source_path: PathBuf,
    pub target_name: String,
    pub target_path: PathBuf,
    pub workspace_path_template: String,
    pub repo_name: String,
    pub author: Option<String>,
    pub preserve_finder_xattrs: bool,
}

/// Execute sync between two workspaces.
///
/// Detects sync mode, validates head info, calls `operations::sync`,
/// and handles post-op cleanup (staleness + bookmark advancement).
#[allow(clippy::too_many_arguments)]
pub fn sync(
    repo: &Path,
    src_name: &str,
    src_path: &Path,
    tgt_name: &str,
    tgt_path: &Path,
    workspace_path_template: &str,
    repo_name: &str,
    author: Option<&str>,
    preserve_finder_xattrs: bool,
) -> Result<operations::SyncOutcome> {
    sync_with_info(
        repo,
        &super::detect_sync_mode(repo, src_name, tgt_name),
        src_name,
        src_path,
        tgt_name,
        tgt_path,
        workspace_path_template,
        repo_name,
        author,
        preserve_finder_xattrs,
    )
}

/// Execute sync with pre-computed sync info (TUI path).
#[allow(clippy::too_many_arguments)]
pub fn sync_with_info(
    repo: &Path,
    info: &SyncModeInfo,
    src_name: &str,
    src_path: &Path,
    tgt_name: &str,
    tgt_path: &Path,
    workspace_path_template: &str,
    repo_name: &str,
    author: Option<&str>,
    preserve_finder_xattrs: bool,
) -> Result<operations::SyncOutcome> {
    // Lenient validation: SyncModeInfo is the complete sync plan, so an
    // op-head move with a plan-equivalent re-detection may proceed (with the
    // fresh info). Anything else bails.
    let validated = super::validate_head_info(repo, info, src_name, tgt_name, true)?;

    // Execution-time freshness + protection (all workspaces, fail-loud).
    // Sync rewrites no interior revisions, but its required src/tgt
    // snapshots and its own fast-forward/merge can rebase a third-party
    // workspace whose `@` descends from src/tgt — the broad descendant-first
    // snapshot captures those edits before they could be staled.
    let fresh = super::prepare_execution_freshness(
        repo,
        &validated,
        src_name,
        src_path,
        tgt_name,
        tgt_path,
        true,
        true,
        None,
        preserve_finder_xattrs,
    )?;
    let validated = fresh.info;
    let src_hi = validated.src_head_info();
    let tgt_hi = validated.tgt_head_info();
    let src = operations::WsRef {
        name: src_name,
        path: src_path,
        info: &src_hi,
    };
    let tgt = operations::WsRef {
        name: tgt_name,
        path: tgt_path,
        info: &tgt_hi,
    };

    let outcome = operations::sync(repo, src, tgt, author)?;

    if let operations::SyncOutcome::Done { mut warnings } = outcome {
        // Resolve the legitimate stale state created when each workspace's `@`
        // was rewritten from the other via --ignore-working-copy. Not a hedge
        // against jj 0.41's reduced sibling-op false positives (#9314).
        let _ = jujutsu::update_workspace_stale(src_path);
        let _ = jujutsu::update_workspace_stale(tgt_path);
        // Materializations are done — restore Finder metadata and report
        // link fidelity.
        warnings.extend(fresh.xattr_guard.restore(&fresh.ws_paths));
        // Advance singular bookmarks for both workspaces to their effective head.
        for ws_name in [src_name, tgt_name] {
            if let Err(e) = jj_utils::advance_singular_bookmark_to_effective_head(
                repo,
                workspace_path_template,
                repo_name,
                ws_name,
            ) {
                warnings.push(format!("{e:#}"));
            }
        }
        for chain in [&src_hi.trivial_ids, &tgt_hi.trivial_ids] {
            let ids: Vec<&str> = chain.iter().map(String::as_str).collect();
            if let Err(e) = jj_utils::abandon_trivial_heads(repo, &ids) {
                warnings.push(format!("{e:#}"));
            }
        }
        // WC-behind report: third-party workspaces staled by the sync (no
        // predicted-stale resolution — sync rewrites no interior revisions).
        warnings.extend(super::post_op_stale(&fresh.ws_paths, &fresh.stale_skipped));
        return Ok(operations::SyncOutcome::Done { warnings });
    }

    Ok(outcome)
}
