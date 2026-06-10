use crate::jj_utils::WorkspaceHeadInfo;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Sync mode detection
// ---------------------------------------------------------------------------

/// Sync state between two workspaces, determined by comparing effective heads
/// to their most-recent common ancestor.
#[derive(Clone, PartialEq)]
pub enum SyncMode {
    /// Both at last common ancestor — nothing to do.
    InSync,
    /// Only source has new work.
    SourceOnly,
    /// Only target has new work.
    TargetOnly,
    /// Both have new work.
    Diverged,
    /// Failed to determine sync state.
    Error(String),
}

impl SyncMode {
    /// Derive sync mode from whether source/target are at the last common ancestor.
    pub fn from_heads(src_at_lca: bool, tgt_at_lca: bool) -> Self {
        match (src_at_lca, tgt_at_lca) {
            (true, true) => SyncMode::InSync,
            (false, true) => SyncMode::SourceOnly,
            (true, false) => SyncMode::TargetOnly,
            (false, false) => SyncMode::Diverged,
        }
    }
}

/// Cached head data for sync-mode detection between two workspaces.
#[derive(Clone)]
pub struct SyncModeInfo {
    pub mode: SyncMode,
    pub src_effective_head: String,
    pub tgt_effective_head: String,
    pub src_actual_head: String,
    pub tgt_actual_head: String,
    pub src_trivial_id: Option<String>,
    pub tgt_trivial_id: Option<String>,
    /// Last common ancestor of src and tgt effective heads.
    pub lca: String,
    /// Operation head when this info was computed (for staleness detection).
    pub op_head: String,
}

impl SyncModeInfo {
    /// Extract source workspace head info for passing to operations.
    pub fn src_head_info(&self) -> WorkspaceHeadInfo {
        WorkspaceHeadInfo {
            effective_head: self.src_effective_head.clone(),
            actual_head: self.src_actual_head.clone(),
            trivial_id: self.src_trivial_id.clone(),
        }
    }

    /// Extract target workspace head info for passing to operations.
    pub fn tgt_head_info(&self) -> WorkspaceHeadInfo {
        WorkspaceHeadInfo {
            effective_head: self.tgt_effective_head.clone(),
            actual_head: self.tgt_actual_head.clone(),
            trivial_id: self.tgt_trivial_id.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Workspace target (shared between TUI and CLI)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct TargetWorkspace {
    pub name: String,
    pub change_id: String,
    pub path: PathBuf,
}

// ---------------------------------------------------------------------------
// Operation types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Operation {
    // Transfer operations (push/pull, both workspaces kept)
    Merge,
    AdaptiveMerge,
    FastForwardTarget,
    FastForwardSource,
    MergeAbandonOld,
    Rebase,
    MergeSquash,
    // Close (merge into target, forget source)
    AdaptiveClose,
    MergeClose,
    MergeSquashClose,
    FastForwardTargetClose,
    // Disposal (close only)
    Detach,
    Abandon,
}

#[derive(Clone, Copy, PartialEq, Default)]
pub enum BookmarkAction {
    #[default]
    NoAction,
    Advance,
    Delete,
}

#[derive(Clone, Copy, PartialEq)]
pub enum DialogIntent {
    Transfer,
    Close,
}

// ---------------------------------------------------------------------------
// CLI method enums (map to Operation for execution)
// ---------------------------------------------------------------------------

/// CLI-facing close method selection.
#[derive(Clone, Copy, Debug, PartialEq, Default, clap::ValueEnum)]
pub enum CloseMethod {
    #[default]
    Adaptive,
    Merge,
    SquashMerge,
    FastForward,
    Detach,
    Abandon,
}

/// CLI-facing transfer method selection.
#[derive(Clone, Copy, PartialEq, Default, clap::ValueEnum)]
pub enum TransferMethod {
    #[default]
    Adaptive,
    Merge,
    FastForwardTarget,
    FastForwardSource,
    MergeAbandonOld,
    Rebase,
    MergeSquash,
}

// ---------------------------------------------------------------------------
// Operation → method conversions
// ---------------------------------------------------------------------------

impl TryFrom<Operation> for CloseMethod {
    type Error = Operation;
    fn try_from(op: Operation) -> Result<Self, Operation> {
        match op {
            Operation::AdaptiveClose => Ok(CloseMethod::Adaptive),
            Operation::MergeClose => Ok(CloseMethod::Merge),
            Operation::MergeSquashClose => Ok(CloseMethod::SquashMerge),
            Operation::FastForwardTargetClose => Ok(CloseMethod::FastForward),
            Operation::Detach => Ok(CloseMethod::Detach),
            Operation::Abandon => Ok(CloseMethod::Abandon),
            other => Err(other),
        }
    }
}

impl TryFrom<Operation> for TransferMethod {
    type Error = Operation;
    fn try_from(op: Operation) -> Result<Self, Operation> {
        match op {
            Operation::AdaptiveMerge => Ok(TransferMethod::Adaptive),
            Operation::Merge => Ok(TransferMethod::Merge),
            Operation::FastForwardTarget => Ok(TransferMethod::FastForwardTarget),
            Operation::FastForwardSource => Ok(TransferMethod::FastForwardSource),
            Operation::MergeAbandonOld => Ok(TransferMethod::MergeAbandonOld),
            Operation::Rebase => Ok(TransferMethod::Rebase),
            Operation::MergeSquash => Ok(TransferMethod::MergeSquash),
            other => Err(other),
        }
    }
}

// ---------------------------------------------------------------------------
// Command results
// ---------------------------------------------------------------------------

/// Result of close/transfer execution.
#[allow(dead_code)]
pub struct CloseTransferResult {
    /// The concrete operation that was executed.
    pub operation_used: Operation,
    /// Workspaces that remained stale after post-op cleanup.
    pub stale_warnings: Vec<String>,
    /// Third-party workspaces predicted to become stale by this operation.
    pub predicted_stale: Vec<String>,
    /// Source workspace was closed (forgotten).
    pub source_forgotten: bool,
    /// Path to remove if user confirms file deletion.
    pub pending_remove_path: Option<PathBuf>,
}

/// Result of create execution.
pub struct CreateResult {
    pub workspace_name: String,
    pub workspace_path: PathBuf,
}
