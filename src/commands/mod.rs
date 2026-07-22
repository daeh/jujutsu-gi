pub mod close;
pub mod create;
pub mod switch;
pub mod sync;
pub mod transfer;
pub mod types;

use anyhow::{Context, Result, bail};
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
    // Capture the op head before the queries: the stored op head must be
    // <= the data's freshness, never >=. A mutation landing mid-detection
    // then makes the stored op head stale, so the next freshness check
    // re-detects instead of wrongly treating mixed old/new facts as current.
    let op_head = jujutsu::current_op_head(repo).unwrap_or_default();

    // Error infos carry the real op head too: disposal ops (detach/abandon)
    // legitimately execute with Error-mode info and still need freshness
    // validation against it.
    let err = |msg: String| SyncModeInfo {
        mode: SyncMode::Error(msg),
        src_effective_head: String::new(),
        tgt_effective_head: String::new(),
        src_actual_head: String::new(),
        tgt_actual_head: String::new(),
        src_trivial_id: None,
        tgt_trivial_id: None,
        src_trivial_ids: Vec::new(),
        tgt_trivial_ids: Vec::new(),
        lca: String::new(),
        op_head: op_head.clone(),
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
        src_trivial_ids: src_info.trivial_ids,
        tgt_trivial_ids: tgt_info.trivial_ids,
        lca,
        op_head,
    }
}

// ---------------------------------------------------------------------------
// Freshness primitives
// ---------------------------------------------------------------------------

/// Snapshot the given required (name, path) pairs (involved-workspace
/// freshness at the gate / CLI entry). `snapshot_ws` is itself conditional —
/// a clean working copy creates no operation. An empty or missing path is an
/// error — a required workspace that cannot be snapshotted means the plan
/// could be blind to pending edits, the bug class the freshness gates remove.
/// Callers pass only workspaces the pending operation actually consumes
/// (close-detach/abandon: source only); "not applicable" is expressed by
/// omission, never by silent skipping.
pub fn snapshot_workspaces(pairs: &[(&str, &Path)]) -> Result<()> {
    for (name, path) in pairs {
        if path.as_os_str().is_empty() || !path.exists() {
            bail!(
                "workspace {name} has no usable working-copy directory ({}); \
                 cannot snapshot pending edits",
                path.display()
            );
        }
        jujutsu::snapshot_ws(path).with_context(|| format!("snapshotting workspace {name}"))?;
    }
    Ok(())
}

/// Plan-relevant equality of two `SyncModeInfo`: same mode discriminant, same
/// src/tgt effective + actual heads, same src/tgt trivial ids, same lca.
/// `op_head` is deliberately excluded — it is the staleness trigger, not the
/// plan.
pub fn plan_equivalent(a: &SyncModeInfo, b: &SyncModeInfo) -> bool {
    std::mem::discriminant(&a.mode) == std::mem::discriminant(&b.mode)
        && a.src_effective_head == b.src_effective_head
        && a.tgt_effective_head == b.tgt_effective_head
        && a.src_actual_head == b.src_actual_head
        && a.tgt_actual_head == b.tgt_actual_head
        && a.src_trivial_id == b.src_trivial_id
        && a.tgt_trivial_id == b.tgt_trivial_id
        && a.src_trivial_ids == b.src_trivial_ids
        && a.tgt_trivial_ids == b.tgt_trivial_ids
        && a.lca == b.lca
}

/// Outcome of an execute-time freshness check.
pub enum Freshness {
    /// Op head unchanged since the cached info was computed.
    Unchanged,
    /// Op head moved but the re-detected plan is identical; carries the fresh
    /// info (newer op head) so drain-time validation passes without rework.
    /// Only produced when `allow_equivalent` is set (sync). Boxed to keep the
    /// enum small (clippy::large_enum_variant).
    Equivalent(Box<SyncModeInfo>),
    /// The plan (possibly) changed; the caller must refresh its view and
    /// re-confirm with the user.
    Changed,
}

/// Conditionally snapshot the required workspaces, then decide whether `info`
/// still describes reality.
///
/// `required` lists exactly the workspaces the pending operation consumes
/// (sync/transfer: src+tgt; close-detach/abandon: src only). Re-detection
/// always uses both names. `allow_equivalent` is true for sync, where
/// `SyncModeInfo` is the complete executable plan; false for close/transfer,
/// whose plan also spans revisions/targets/bookmarks — for them any op-head
/// movement returns `Changed`. Probe/snapshot failure returns `Err` (blocks
/// execution; the caller surfaces it).
pub fn check_freshness(
    repo: &Path,
    info: &SyncModeInfo,
    src_name: &str,
    tgt_name: &str,
    required: &[(&str, &Path)],
    allow_equivalent: bool,
) -> Result<Freshness> {
    snapshot_workspaces(required)?;
    let current_op = jujutsu::current_op_head(repo).unwrap_or_default();
    if current_op == info.op_head {
        return Ok(Freshness::Unchanged);
    }
    if !allow_equivalent {
        return Ok(Freshness::Changed);
    }
    let fresh = detect_sync_mode(repo, src_name, tgt_name);
    if plan_equivalent(info, &fresh) {
        Ok(Freshness::Equivalent(Box::new(fresh)))
    } else {
        Ok(Freshness::Changed)
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate that cached head info is still current. Last check before
/// execution — there is no later gate to self-heal.
///
/// If the repo's op head hasn't changed, returns the cached info (the only
/// success path in strict mode). On movement:
///
/// - `allow_equivalent == false` (close/transfer, all methods including
///   Detach/Abandon): bail. Their executable plan spans revisions, bookmarks,
///   and target entries beyond `SyncModeInfo`; semantic identity is not
///   provable here, so silently executing is not acceptable. Normal flows
///   never hit this: the TUI gate / CLI entry snapshot ran moments earlier.
/// - `allow_equivalent == true` (sync): re-detect with stable detection —
///   re-read the op head after detection and retry once if the repo moved
///   *during* it — then return the fresh info iff `plan_equivalent`.
///
/// Callers use the returned info's heads and lca, so a recomputed plan never
/// executes against a stale lca.
pub fn validate_head_info(
    repo: &Path,
    info: &SyncModeInfo,
    src_name: &str,
    tgt_name: &str,
    allow_equivalent: bool,
) -> Result<SyncModeInfo> {
    let current_op = jujutsu::current_op_head(repo).unwrap_or_default();
    if current_op == info.op_head {
        return Ok(info.clone());
    }
    if !allow_equivalent {
        bail!("repo changed, please retry");
    }
    // Op head moved — re-detect with a stable detection window.
    let fresh = stable_detect_sync_mode(repo, src_name, tgt_name)?;
    if let SyncMode::Error(e) = &fresh.mode {
        bail!("repo changed externally, please retry ({e})");
    }
    if plan_equivalent(info, &fresh) {
        return Ok(fresh);
    }
    bail!("repo changed externally, please retry");
}

/// Detect sync mode requiring a stable detection window: re-read the op head
/// after detection and retry once if the repo moved *during* it (a fresh info
/// computed across a mutation mixes old/new facts). Bails if no stable window
/// is found in two attempts.
fn stable_detect_sync_mode(repo: &Path, src_name: &str, tgt_name: &str) -> Result<SyncModeInfo> {
    for _ in 0..2 {
        let fresh = detect_sync_mode(repo, src_name, tgt_name);
        let after = jujutsu::current_op_head(repo).unwrap_or_default();
        if after == fresh.op_head {
            return Ok(fresh);
        }
    }
    bail!("repo changed externally, please retry");
}

// ---------------------------------------------------------------------------
// Execution-time freshness + protection (all workspaces, fail-loud)
// ---------------------------------------------------------------------------

/// Op-log scan window for the post-phase snapshot-only gate. Generous: the
/// phase itself creates up to one snapshot op per workspace before checking.
const RECHECK_OP_SCAN_LIMIT: usize = 1000;

/// Output of [`prepare_execution_freshness`].
pub struct ExecutionFreshness {
    /// The plan to execute against. For plan-consuming methods this is the
    /// post-snapshot re-detection (verified `plan_equivalent` to the
    /// validated input); for disposal methods (Detach/Abandon) it is the
    /// validated input unchanged.
    pub info: SyncModeInfo,
    /// Fresh (name, path) pairs for every workspace with an existing on-disk
    /// directory — the sole list for all post-validation uses (post-op
    /// staleness scan, predicted-stale resolution).
    pub ws_paths: Vec<(String, PathBuf)>,
    /// Workspaces skipped by the protection snapshot because they are
    /// jj-stale. This is the pre-op stale baseline for `post_op_stale`
    /// (no separate detection pass), and is excluded from auto-resolution.
    pub stale_skipped: Vec<String>,
}

/// Execution-time freshness + protection phase. Runs after
/// `validate_head_info`, before the operation executes:
///
/// 1. fail-closed op-head sanity (empty / divergent baseline);
/// 2. fresh workspace re-list (never a caller-captured list);
/// 3. involved path check by name (and target change-id when a manual
///    bookmark Advance will execute against it) — abort on mismatch before
///    any snapshot;
/// 4. broad snapshot, descendant-first, third-party first and the involved
///    src/tgt last: every existing, non-stale workspace's pending edits are
///    captured (fail-loud) before the rewrite can rebase them; jj-stale
///    third-party workspaces are skipped (their edits belong to the
///    update-stale workflow); a failed required (involved) snapshot aborts;
/// 5. recheck: op-head movement since `validated` must be snapshot-only,
///    involved paths must still resolve, and (plan-consuming methods only)
///    a stable re-detection must be `plan_equivalent` to the validated plan.
///
/// `target_involved` is false for Detach/Abandon (their target is not
/// consumed). `plan_consuming` is false for Detach/Abandon, which
/// legitimately run on Error-mode info and rely on their own live source
/// validation instead of `plan_equivalent` (Error == Error compares only the
/// discriminant — not a real plan check).
#[allow(clippy::too_many_arguments)]
pub fn prepare_execution_freshness(
    repo: &Path,
    validated: &SyncModeInfo,
    src_name: &str,
    src_path: &Path,
    tgt_name: &str,
    tgt_path: &Path,
    target_involved: bool,
    plan_consuming: bool,
    advance_target_change_id: Option<&str>,
) -> Result<ExecutionFreshness> {
    // 1. Fail closed on an unusable baseline: an empty op head means the
    // read failed when `validated` was computed (and "" == "" would have
    // validated vacuously); a comma-joined key means divergent op heads.
    if validated.op_head.is_empty() || validated.op_head.contains(',') {
        bail!("repo changed, please retry");
    }

    // 2. Fresh re-list (one jj call).
    let entries = jujutsu::list_workspace_entries(repo)?;

    // 3. Involved path check — before any snapshot, so we never snapshot a
    // stale captured path and then discover the name resolved elsewhere.
    check_involved_paths(
        &entries,
        src_name,
        src_path,
        target_involved.then_some((tgt_name, tgt_path)),
    )?;
    if let Some(expected) = advance_target_change_id {
        // A manual bookmark Advance executes against this change id; change
        // ids are snapshot-stable, so our own phase cannot invalidate it.
        let fresh_id = entries
            .iter()
            .find(|e| e.name == tgt_name)
            .map(|e| e.change_id.as_str());
        if fresh_id != Some(expected) {
            bail!("repo changed, please retry");
        }
    }

    // Existing on-disk workspaces only (same filter the staleness scans use).
    let on_disk: Vec<jujutsu::WorkspaceEntry> = entries
        .into_iter()
        .filter(|e| !e.path.as_os_str().is_empty() && e.path.exists())
        .collect();
    let ws_paths: Vec<(String, PathBuf)> = on_disk
        .iter()
        .map(|e| (e.name.clone(), e.path.clone()))
        .collect();

    // 4. Broad snapshot. Descendant-first within each tier, third-party
    // tier first, involved src/tgt last: a dirty workspace's snapshot
    // amends its `@` and rebases (stales) descendants, so each descendant
    // is captured before any ancestor's snapshot can touch it. The
    // involved-last fail-loud snapshot doubles as the involved-staleness
    // guarantee: if an earlier third-party snapshot staled src/tgt, their
    // snapshot now fails and the operation aborts (on retry that ancestor
    // is clean, so it converges).
    let order = jujutsu::descendant_first_workspaces(repo, &on_disk)
        .context("ordering workspaces for the protection snapshot")?;
    let involved = |name: &str| name == src_name || (target_involved && name == tgt_name);

    let mut stale_skipped = Vec::new();
    // Third-party tier (optional): skip jj-stale, abort on any other failure.
    for &i in &order {
        let e = &on_disk[i];
        if involved(&e.name) {
            continue;
        }
        if let Err(err) = jujutsu::snapshot_ws(&e.path) {
            // Classify only after the failure — a pre-check via the live
            // status probe would snapshot behind the fail-loud snapshot's
            // back. Stale edits are the update-stale workflow's to recover.
            if jujutsu::is_workspace_stale(&e.path) {
                stale_skipped.push(e.name.clone());
            } else {
                return Err(err.context(format!(
                    "snapshotting workspace {} before execution",
                    e.name
                )));
            }
        }
    }
    // Involved tier (required): any failure aborts.
    for &i in &order {
        let e = &on_disk[i];
        if involved(&e.name) {
            jujutsu::snapshot_ws(&e.path)
                .with_context(|| format!("snapshotting workspace {} (repo changed?)", e.name))?;
        }
    }

    // 5. Recheck against the validated baseline.
    let info = recheck_execution_freshness(
        repo,
        validated,
        src_name,
        src_path,
        tgt_name,
        tgt_path,
        target_involved,
        plan_consuming,
    )?;

    Ok(ExecutionFreshness {
        info,
        ws_paths,
        stale_skipped,
    })
}

/// Resolve the involved workspaces by NAME from a fresh entry list and
/// require their paths to match the executable params. A relocated (forget +
/// re-add) or vanished involved workspace aborts — never a silent rebind.
fn check_involved_paths(
    entries: &[jujutsu::WorkspaceEntry],
    src_name: &str,
    src_path: &Path,
    target: Option<(&str, &Path)>,
) -> Result<()> {
    let mut required: Vec<(&str, &Path)> = vec![(src_name, src_path)];
    required.extend(target);
    for (name, path) in required {
        let matches = entries
            .iter()
            .find(|e| e.name == name)
            .is_some_and(|e| same_dir(&e.path, path));
        if !matches {
            bail!("repo changed, please retry");
        }
    }
    Ok(())
}

/// Directory identity, tolerant of unresolved symlinks: jj reports canonical
/// workspace roots while callers may hold an equivalent non-canonical path.
fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Post-snapshot revalidation. Deliberately not `validate_head_info` (which
/// bails on any op-head movement): the snapshot phase legitimately moves the
/// op head, so this tolerates snapshot-only movement and re-proves the plan.
#[allow(clippy::too_many_arguments)]
fn recheck_execution_freshness(
    repo: &Path,
    validated: &SyncModeInfo,
    src_name: &str,
    src_path: &Path,
    tgt_name: &str,
    tgt_path: &Path,
    target_involved: bool,
    plan_consuming: bool,
) -> Result<SyncModeInfo> {
    // Snapshot-only gate: any external (non-snapshot) operation since the
    // validated baseline invalidates executable state beyond `SyncModeInfo`
    // (revisions, bookmarks, target ids, paths).
    if !jujutsu::only_snapshots_since(repo, &validated.op_head, RECHECK_OP_SCAN_LIMIT)? {
        bail!("repo changed, please retry");
    }

    // Path recheck (complements the pre-snapshot check): the involved names
    // must still resolve to the executable paths.
    let entries = jujutsu::list_workspace_entries(repo)?;
    check_involved_paths(
        &entries,
        src_name,
        src_path,
        target_involved.then_some((tgt_name, tgt_path)),
    )?;

    if !plan_consuming {
        // Disposal (Detach/Abandon): Error-mode infos make `plan_equivalent`
        // a discriminant-only comparison, not a plan proof. They rely on the
        // snapshot-only gate above plus their own live source validation
        // (Abandon re-verifies the live revision set; Detach forgets the
        // live source).
        return Ok(validated.clone());
    }

    // Plan-consuming methods: re-detect (stable) and require equivalence.
    // The change-id comparison is the deliberate semantics: a content-only
    // edit folded by the phase keeps its change id and is incorporated by
    // change-id resolution; an edit that flips a trivial head / moves an
    // effective head changes the plan and must abort (the edit is already
    // safely captured — the user retries against the new plan).
    let fresh = stable_detect_sync_mode(repo, src_name, tgt_name)?;
    if let SyncMode::Error(e) = &fresh.mode {
        bail!("repo changed, please retry ({e})");
    }
    if !plan_equivalent(validated, &fresh) {
        bail!("repo changed, please retry");
    }
    Ok(fresh)
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

/// WC-behind report (live status): scan all workspaces post-op and report
/// any that are stale, tagged against the pre-op baseline (`pre_stale` — the
/// protection phase's stale skip-set). The live scan's snapshot side effect
/// is redundant for edit survival (the protection phase is the freshness mechanism)
/// but accepted as part of producing the report.
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
/// Must be called before the operation executes but after the protection
/// snapshot phase (the phase can move heads / rebase descendants, so a
/// pre-phase prediction would be stale). Used only for post-op resolution +
/// reporting — edit survival never depends on it (the broad snapshot covers
/// that). Best-effort: returns empty on query failure rather than blocking.
///
/// `involved` lists the workspaces the operation consumes (Abandon: source
/// only — its target is not involved, and a workspace descending from the
/// abandoned revisions must not be dropped from resolution/reporting).
pub fn predict_stale_workspaces(
    repo: &Path,
    operation: Operation,
    info: &SyncModeInfo,
    abandoned_ids: &[&str],
    involved: &[&str],
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
            .filter(|n| !involved.contains(&n.as_str()))
            .collect(),
        Err(_) => vec![],
    }
}

/// Resolve predicted stale workspaces after an operation.
///
/// Calls `update_workspace_stale` on each workspace that was predicted to
/// become stale. Skips workspaces whose path is empty or missing. Callers
/// must not pass workspaces that were already stale pre-op (the protection
/// phase's skip-set): those belong to the user's update-stale workflow and
/// surface in the report as "was already stale".
pub fn resolve_predicted_stale(predicted: &[String], ws_paths: &[(String, PathBuf)]) {
    for (name, path) in ws_paths {
        if predicted.contains(name) && !path.as_os_str().is_empty() && path.exists() {
            let _ = jujutsu::update_workspace_stale(path);
        }
    }
}

/// Result of post-op bookmark handling.
pub struct BookmarkResult {
    /// Non-singular bookmarks not auto-handled (caller may apply manual action).
    pub remaining: Vec<String>,
    /// Bookmark commands that failed after the primary operation.
    pub errors: Vec<types::PostOperationError>,
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
    let mut errors = Vec::new();

    /// Collect a failed bookmark operation as a structured post-op error.
    fn collect(
        result: anyhow::Result<Option<String>>,
        errors: &mut Vec<types::PostOperationError>,
    ) {
        if let Err(e) = result {
            errors.push(types::PostOperationError::from_anyhow(&e));
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
                &mut errors,
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
                    &mut errors,
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
                    &mut errors,
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
                    &mut errors,
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
        errors,
    }
}

/// Apply manual bookmark action (close only, for remaining non-singular bookmarks).
///
/// Returns structured errors for any bookmark operations that failed.
pub fn post_op_manual_bookmarks(
    repo: &Path,
    bookmarks: &[String],
    action: types::BookmarkAction,
    target_change_id: &str,
) -> Vec<types::PostOperationError> {
    let mut errors = Vec::new();
    match action {
        types::BookmarkAction::Advance => {
            for bm in bookmarks {
                if let Err(e) = jujutsu::bookmark_set_exact(repo, bm, target_change_id) {
                    errors.push(types::PostOperationError::from_anyhow(&e));
                }
            }
        }
        types::BookmarkAction::Delete => {
            for bm in bookmarks {
                if let Err(e) = jujutsu::bookmark_delete(repo, bm) {
                    errors.push(types::PostOperationError::from_anyhow(&e));
                }
            }
        }
        types::BookmarkAction::NoAction => {}
    }
    errors
}
