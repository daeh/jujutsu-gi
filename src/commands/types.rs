use crate::{jj_utils::WorkspaceHeadInfo, jujutsu::JjCommandError};
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
    pub src_trivial_ids: Vec<String>,
    pub tgt_trivial_ids: Vec<String>,
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
            trivial_ids: self.src_trivial_ids.clone(),
        }
    }

    /// Extract target workspace head info for passing to operations.
    pub fn tgt_head_info(&self) -> WorkspaceHeadInfo {
        WorkspaceHeadInfo {
            effective_head: self.tgt_effective_head.clone(),
            actual_head: self.tgt_actual_head.clone(),
            trivial_id: self.tgt_trivial_id.clone(),
            trivial_ids: self.tgt_trivial_ids.clone(),
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

impl Operation {
    pub fn close_method_label(self) -> Option<&'static str> {
        match self {
            Self::MergeClose => Some("merge"),
            Self::MergeSquashClose => Some("squash-merge"),
            Self::FastForwardTargetClose => Some("fast-forward"),
            Self::Detach => Some("detach"),
            Self::Abandon => Some("abandon"),
            _ => None,
        }
    }
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
pub struct CloseTransferResult {
    /// Workspaces that remained stale after post-op cleanup.
    pub stale_warnings: Vec<String>,
    /// Non-fatal informational warnings unrelated to workspace staleness.
    pub warnings: Vec<String>,
    /// Commands or checks that failed after the primary operation completed.
    pub post_errors: Vec<PostOperationError>,
    /// Concrete operation executed after resolving an adaptive request.
    pub resolved_operation: Operation,
    /// Third-party workspaces predicted to become stale by this operation.
    /// Production resolves these internally; integration tests read the set
    /// to pin the prediction logic.
    pub predicted_stale: Vec<String>,
    /// Path to remove if user confirms file deletion.
    pub pending_remove_path: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PostOperationError {
    Command { command: String, stderr: String },
    Message(String),
}

impl PostOperationError {
    pub fn from_anyhow(error: &anyhow::Error) -> Self {
        if let Some(command_error) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<JjCommandError>())
        {
            Self::Command {
                command: command_error.command().to_string(),
                stderr: command_error.stderr().to_string(),
            }
        } else {
            Self::Message(format!("{error:#}"))
        }
    }

    pub fn display_block(&self) -> String {
        match self {
            Self::Command { command, stderr } => format!("Cmd: `{command}`\n{stderr}"),
            Self::Message(message) => format!("Error: {message}"),
        }
    }
}

pub fn format_post_close_errors(workspace_name: &str, errors: &[PostOperationError]) -> String {
    let blocks = errors
        .iter()
        .map(PostOperationError::display_block)
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("workspace '{workspace_name}' closed with error.\n{blocks}")
}

/// Result of create execution.
pub struct CreateResult {
    pub workspace_name: String,
    pub workspace_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_close_command_error_matches_status_contract() {
        let error = PostOperationError::Command {
            command: "jj bookmark set --allow-backwards --revision abc -- recovery".into(),
            stderr: "Error: refused\nHint: retry".into(),
        };
        assert_eq!(
            format_post_close_errors("recovery", &[error]),
            "workspace 'recovery' closed with error.\n\
             Cmd: `jj bookmark set --allow-backwards --revision abc -- recovery`\n\
             Error: refused\n\
             Hint: retry"
        );
    }
}
