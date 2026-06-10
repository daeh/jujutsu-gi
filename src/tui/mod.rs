mod bookmarks_dialog;
mod cmd_spans;
mod copy_dialog;
mod create_dialog;
mod dialog_common;
mod dialog_info;
mod graph_pane;
mod key_hints;
mod op_log_pane;
mod revision_picker;
mod sync_dialog;
mod transfer_close_dialog;
mod update_stale_dialog;
mod workspace_list;

use crate::{action_history, commands, config, hooks, jj_utils, jujutsu, operations, shell};
use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event,
    KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use dialog_common::TargetWorkspace;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use transfer_close_dialog::{CloseDialog, DialogIntent, Operation};

use bookmarks_dialog::BookmarksDialog;
use copy_dialog::CopyDialog;
use create_dialog::CreateDialog;
use graph_pane::GraphPane;
use revision_picker::RevisionPicker;
use sync_dialog::SyncDialog;
use update_stale_dialog::UpdateStaleDiffDialog;
use workspace_list::WorkspaceList;

/// Build a position annotation for a bookmark based on its change_id.
///
/// - `@`  if at the workspace working copy
/// - `@-` if at the parent of the working copy (next revision in the chain)
/// - `^`  if at the head of the revision chain (`revisions[0]`)
///
/// Fixed-width 4-char annotation: `^@  `, `^   `, ` @  `, ` @- `, `    `.
fn bookmark_annotation(
    bm_change_id: &str,
    is_at_head: bool,
    ws_change_id: &str,
    revisions: &[jujutsu::RevisionInfo],
) -> String {
    let head_char = if is_at_head { "^" } else { " " };

    let wc_part = if ws_change_id == bm_change_id {
        "@"
    } else {
        let wc_idx = revisions.iter().position(|r| ws_change_id == r.change_id);
        if let Some(idx) = wc_idx
            && idx + 1 < revisions.len()
            && bm_change_id == revisions[idx + 1].change_id
        {
            "@-"
        } else {
            ""
        }
    };

    format!("{head_char}{wc_part:<3}")
}

enum Mode {
    List,
    Create,
    Close,
    Sync,
    ConfirmRemoveFiles,
    Copy,
    Bookmarks,
    Split,
    StaleAlert,
    UpdateStale,
    ConfigWarning,
    OpRestore,
    /// Confirm undo/redo when non-snapshot ops occurred since last ji action.
    UndoRedoConfirm {
        is_redo: bool,
    },
}

#[derive(PartialEq)]
enum Focus {
    Workspaces,
    Graph,
}

#[derive(PartialEq)]
enum LeftPanel {
    Workspaces,
    OpLog,
}

struct App {
    mode: Mode,
    focus: Focus,
    workspace_list: WorkspaceList,
    graph_pane: GraphPane,
    create_dialog: CreateDialog,
    close_dialog: Option<CloseDialog>,
    sync_dialog: Option<SyncDialog>,
    copy_dialog: Option<CopyDialog>,
    bookmarks_dialog: Option<BookmarksDialog>,
    /// Path of workspace files to potentially remove after close.
    pending_remove_path: Option<PathBuf>,
    /// A jj subprocess (or file-write side effect) deferred to run between
    /// terminal-handoff (exit raw mode) and re-entry. Each variant captures
    /// all owned params needed to invoke a jujutsu/commands function from
    /// `run_tui`'s drain block (`drain_pending_handoff`).
    ///
    /// **Coverage:** every mutating jj or file-write site in the TUI event
    /// loop is deferred via this enum. The one synchronous exception is
    /// `Command::new("pbcopy")` in `copy_dialog.rs::copy_to_clipboard` — it
    /// runs in ≈20 ms and writes nothing to the terminal, so it doesn't need
    /// the raw-mode handoff the deferred sites use.
    pending_handoff: Option<PendingHandoff>,
    revision_picker: Option<RevisionPicker>,
    config: config::Config,
    repo_root: PathBuf,
    current_root: PathBuf,
    repo_name: String,
    action: Option<Action>,
    /// Whether the working copy is currently stale.
    stale: bool,
    /// Error message from a failed update-stale attempt.
    stale_error: Option<String>,
    /// Transient status/error messages (rendered as stacked lines).
    status_messages: Vec<String>,
    /// Target workspace for the update-stale dialog.
    update_stale_target: Option<(String, PathBuf)>,
    /// Diff dialog for stale workspaces.
    update_stale_diff: Option<UpdateStaleDiffDialog>,
    /// Receiver for background stale-diff computation.
    stale_diff_rx: Option<std::sync::mpsc::Receiver<jujutsu::StaleDiffMsg>>,
    /// Progress of background stale-diff: (checked, total).
    stale_diff_progress: Option<(usize, usize)>,
    /// Whether the help pane is shown instead of the graph.
    show_help: bool,
    /// Last time staleness was polled due to selection change.
    last_selection_stale_poll: std::time::Instant,
    /// Whether the terminal window has focus.
    terminal_focused: bool,
    /// Cached height of the graph pane for scroll calculations.
    graph_visible_height: u16,
    /// Cached terminal size for mouse hit-testing in dialogs.
    terminal_area: Rect,
    /// Cached panel areas for mouse hit-testing.
    workspace_area: Rect,
    graph_area: Rect,
    /// Index into the selected workspace's `revisions` vec, if browsing individual revisions.
    /// None = whole-workspace mode. 0 = head, 1 = parent of head, etc.
    revision_cursor: Option<usize>,
    /// When true, the description area shows modified files instead of the message.
    show_files: bool,
    /// Cache of change_id → diff summary, to avoid re-fetching.
    cached_files: HashMap<String, jujutsu::DiffSummary>,
    /// Undo/redo stack for compound ji actions.
    action_history: action_history::ActionHistory,
    /// Path to the persisted action history JSON file.
    history_path: PathBuf,
    /// Which panel is shown on the left.
    left_panel: LeftPanel,
    /// Op log pane (created on first open).
    op_log_pane: Option<op_log_pane::OpLogPane>,
    /// Debounce timer for graph-at-operation fetches.
    op_graph_debounce: std::time::Instant,
    /// The operation ID whose graph is currently displayed.
    op_graph_current_id: Option<String>,
}

enum Action {
    SwitchTo(PathBuf),
    Quit,
}

/// A jj subprocess (or file/clipboard side effect) deferred until after
/// `run_tui`'s drain block can exit raw mode. See `App::pending_handoff`
/// for the coverage invariant.
///
/// Variants must hold OWNED data only — the drain block consumes the value
/// (`match handoff { … }`), so any borrowed reference would prevent the
/// move-into-borrow-typed-param-struct pattern used by `Close`/`Transfer`
/// (and the equivalent move-semantics future variants need).
enum PendingHandoff {
    /// `jj split <change_id>` — interactive, needs raw-mode exit.
    Split { change_id: String },
    /// `jj op restore <op_id>` — undo/redo from the action-history stack.
    /// Carries the redo direction so the status message can be customised.
    OpRestore {
        op_id: String,
        label: String,
        is_redo: bool,
    },
    /// `std::fs::remove_dir_all(path)` — recursive directory deletion
    /// invoked after the user confirms `Mode::ConfirmRemoveFiles` with 'y'.
    /// Records nothing in action history (directory deletion isn't a jj op).
    RemoveDirAll { path: std::path::PathBuf },
    /// Save the currently-selected file's diff to `.ji/diffs/`. Invokes
    /// `update_stale_dialog::save_diff_inline`, which shells out to
    /// `jj file show` and writes a unified diff.
    SaveDiff {
        ws_path: std::path::PathBuf,
        kind: update_stale_dialog::DiffKind,
        rel_path: String,
    },
    /// Save diffs for every item in the update-stale dialog.
    SaveAllDiffs {
        ws_path: std::path::PathBuf,
        items: Vec<(update_stale_dialog::DiffKind, String)>,
    },
    /// `jj workspace forget <name>` — orphan-workspace cleanup. Records
    /// nothing in action history.
    WorkspaceForget { name: String },
    /// `jj bookmark create --revision <change_id> -- <name>`. Records
    /// `"bookmark create {name}"` in action history.
    BookmarkCreate { name: String, change_id: String },
    /// Sequence: save_all_stale_diffs(repo_root) THEN update_stale(repo_root).
    /// Fused so the user sees one terminal handoff rather than two flickers.
    /// Triggered from `Mode::StaleAlert` + 'r'.
    UpdateStaleSequence,
    /// Sequence: save_all_stale_diffs(ws_path) THEN update_workspace_stale(ws_path).
    /// Per-workspace counterpart of `UpdateStaleSequence`. Triggered from
    /// `Mode::UpdateStale` + 'y'.
    UpdateWorkspaceStale {
        ws_path: std::path::PathBuf,
        name: String,
    },
    /// `jj op restore <op_id>` invoked from the op-log pane UI (distinct
    /// from `OpRestore` which is the action-history-driven undo/redo path).
    /// Records nothing in action history; afterward transitions to
    /// `Mode::List` and re-enters the op-log pane.
    OpRestoreFromLog { op_id: String },
    /// Loop `jj bookmark set <name> --revision <head_id>` over a Vec of
    /// bookmark names (multi-select from `bookmarks_dialog`). Records
    /// one combined history entry `"bookmark tug <names.join(", ")>"`.
    /// Dialog rebuilds after refresh (stays open on the freshly-refreshed
    /// workspace state).
    BookmarkTug { names: Vec<String>, head_id: String },
    /// Loop `jj bookmark delete <name>` over a Vec of bookmark names.
    /// Symmetric with `BookmarkTug`.
    BookmarkDelete { names: Vec<String> },
    /// `commands::sync::sync_with_info` — workspace sync orchestration.
    /// `label` is pre-computed at deferral time (e.g. "sync src → tgt").
    Sync {
        params: commands::sync::SyncParamsOwned,
        label: String,
    },
    /// `commands::create::create` — workspace creation. No pre-computed label;
    /// the drain arm builds `"create {result.workspace_name}"` after the call
    /// returns, then sets the workspace_list selection to the new name.
    Create {
        params: commands::create::CreateParamsOwned,
    },
    /// `commands::close::close_with_info` — workspace close orchestration.
    /// `info` is the resolved SyncModeInfo (either captured from the dialog
    /// or freshly computed for Detach/Abandon at deferral time).
    Close {
        params: commands::close::CloseParamsOwned,
        info: commands::types::SyncModeInfo,
        label: String,
    },
    /// `commands::transfer::transfer_with_info` — workspace transfer orchestration.
    Transfer {
        params: commands::transfer::TransferParamsOwned,
        info: commands::types::SyncModeInfo,
        label: String,
    },
}

impl App {
    #[allow(clippy::too_many_arguments)]
    fn new(
        workspaces: Vec<jujutsu::Workspace>,
        config: config::Config,
        repo_root: PathBuf,
        current_root: PathBuf,
        repo_name: String,
        graph_output: &str,
        line_heads: Vec<Option<String>>,
        action_history: action_history::ActionHistory,
        history_path: PathBuf,
    ) -> Self {
        let current_name: Option<String> = workspaces
            .iter()
            .find(|w| w.is_current)
            .map(|w| w.name.clone());
        Self {
            mode: Mode::List,
            focus: Focus::Workspaces,
            workspace_list: WorkspaceList::new(
                workspaces,
                current_name.as_deref(),
                workspace_list::SortOrder::LogOrder,
                &line_heads,
            ),
            graph_pane: GraphPane::new(graph_output, line_heads),
            create_dialog: CreateDialog::new(),
            close_dialog: None,
            sync_dialog: None,
            copy_dialog: None,
            bookmarks_dialog: None,
            pending_remove_path: None,
            pending_handoff: None,
            revision_picker: None,
            config,
            repo_root,
            current_root,
            repo_name,
            action: None,
            stale: false,
            stale_error: None,
            status_messages: Vec::new(),
            update_stale_target: None,
            update_stale_diff: None,
            stale_diff_rx: None,
            stale_diff_progress: None,
            show_help: false,
            last_selection_stale_poll: std::time::Instant::now(),
            terminal_focused: true,
            graph_visible_height: 0,
            terminal_area: Rect::ZERO,
            workspace_area: Rect::ZERO,
            graph_area: Rect::ZERO,
            revision_cursor: None,
            show_files: false,
            cached_files: HashMap::new(),
            action_history,
            history_path,
            left_panel: LeftPanel::Workspaces,
            op_log_pane: None,
            op_graph_debounce: std::time::Instant::now(),
            op_graph_current_id: None,
        }
    }

    /// Sync the graph pane scroll position and highlighting to the currently selected workspace.
    fn sync_graph_to_selection(&mut self) {
        let Some(ws) = self.workspace_list.selected_workspace() else {
            return;
        };
        let change_id = ws.change_id.clone();
        let is_default = ws.name == "default";

        // Collect the active set: change_ids that should stay bright.
        // Everything else gets dimmed (dim_matched = false).
        let active: HashSet<String> = if is_default {
            // Default owns ::default@ minus revisions owned by other workspaces.
            let mut owned: HashSet<String> = jujutsu::ancestor_ids(&self.repo_root, &change_id)
                .unwrap_or_default()
                .into_iter()
                .collect();
            for other in self.workspace_list.workspaces() {
                if other.name == "default" {
                    continue;
                }
                for rev in &other.revisions {
                    owned.remove(&rev.change_id);
                }
            }
            owned
        } else {
            // Non-default owns ::ws@ ~ ::default@ (its unique revisions).
            ws.revisions.iter().map(|r| r.change_id.clone()).collect()
        };

        self.graph_pane
            .scroll_to_change_id(&change_id, self.graph_visible_height);
        let id_refs: HashSet<&str> = active.iter().map(|s| s.as_str()).collect();
        self.graph_pane.highlight(&id_refs, false);
    }

    /// Sync graph to highlight a single revision selected by the revision cursor.
    fn sync_graph_to_revision(&mut self) {
        let Some(ws) = self.workspace_list.selected_workspace() else {
            return;
        };
        let Some(cursor) = self.revision_cursor else {
            return;
        };
        let Some(rev) = ws.revisions.get(cursor) else {
            return;
        };

        let change_id = rev.change_id.clone();
        self.graph_pane
            .scroll_to_change_id(&change_id, self.graph_visible_height);
        let ids: HashSet<&str> = HashSet::from([change_id.as_str()]);
        self.graph_pane.highlight(&ids, false);
    }

    /// Sync the graph highlight to either the focused revision or the workspace selection,
    /// depending on whether `revision_cursor` is set.
    fn sync_graph_to_cursor(&mut self) {
        if self.revision_cursor.is_some() {
            self.sync_graph_to_revision();
        } else {
            self.sync_graph_to_selection();
        }
    }

    /// Handle a left-click on a revision line in the graph pane.
    /// Called only when the graph is already focused (second click).
    fn click_graph_revision(&mut self, row: u16) {
        if self.left_panel != LeftPanel::Workspaces {
            return;
        }
        if self.graph_area.height < 3 {
            return;
        }
        if row <= self.graph_area.y || row >= self.graph_area.y + self.graph_area.height - 1 {
            return; // border click
        }
        let visual_row = row - self.graph_area.y - 1;
        let Some(clicked_id) = self
            .graph_pane
            .change_id_at_row(visual_row)
            .map(str::to_owned)
        else {
            return;
        };

        // Find the workspace + revision that owns this change_id.
        // Check non-default workspaces first: the default workspace's ancestry overlaps
        // with non-default revisions, so checking default first would claim the wrong match.
        let workspaces = self.workspace_list.workspaces();
        let matched = workspaces
            .iter()
            .enumerate()
            .filter(|(_, ws)| ws.name != "default")
            .chain(
                workspaces
                    .iter()
                    .enumerate()
                    .filter(|(_, ws)| ws.name == "default"),
            )
            .find_map(|(ws_idx, ws)| {
                if let Some(rev_idx) = ws.revisions.iter().position(|r| r.change_id == clicked_id) {
                    Some((ws_idx, Some(rev_idx)))
                } else if ws.change_id == clicked_id {
                    Some((ws_idx, None))
                } else {
                    None
                }
            });

        let Some((ws_idx, rev_idx)) = matched else {
            return;
        };

        self.workspace_list.select_index(ws_idx);
        self.revision_cursor = rev_idx;
        self.workspace_list.reset_desc_scroll();
        self.sync_graph_to_cursor();
        if self.last_selection_stale_poll.elapsed() >= std::time::Duration::from_secs(5) {
            self.last_selection_stale_poll = std::time::Instant::now();
            self.refresh_staleness();
        }
        if self.show_files {
            self.ensure_files_cached();
        }
    }

    /// Highlight revisions for both source and target workspaces in the graph.
    fn sync_graph_to_pair(&mut self, source_name: &str, target_name: &str) {
        let workspaces = self.workspace_list.workspaces();

        let src_ws = workspaces.iter().find(|ws| ws.name == source_name);
        let tgt_ws = workspaces.iter().find(|ws| ws.name == target_name);

        let mut active: HashSet<String> = src_ws
            .map(|ws| ws.revisions.iter().map(|r| r.change_id.clone()).collect())
            .unwrap_or_default();

        if let Some(ws) = tgt_ws {
            for rev in &ws.revisions {
                active.insert(rev.change_id.clone());
            }
        }

        // Also include both workspace @ change_ids themselves.
        if let Some(ws) = src_ws {
            active.insert(ws.change_id.clone());
        }
        if let Some(ws) = tgt_ws {
            active.insert(ws.change_id.clone());
        }

        let id_refs: HashSet<&str> = active.iter().map(|s| s.as_str()).collect();
        self.graph_pane.highlight(&id_refs, false);
    }

    fn sync_graph_to_sync(&mut self) {
        let Some(dialog) = &self.sync_dialog else {
            return;
        };
        let Some(target) = dialog.selected_target() else {
            return;
        };
        let src = dialog.source_name.clone();
        let tgt = target.name.clone();
        self.sync_graph_to_pair(&src, &tgt);
    }

    fn sync_graph_to_close(&mut self) {
        let Some(dialog) = &self.close_dialog else {
            return;
        };
        let Some(target) = dialog.selected_target() else {
            return;
        };
        let src = dialog.workspace_name.clone();
        let tgt = target.name.clone();
        self.sync_graph_to_pair(&src, &tgt);
    }

    /// Log an error, appending to the status message. Returns Some(value) on Ok.
    fn log_err<T, E: std::fmt::Display>(
        &mut self,
        result: Result<T, E>,
        context: &str,
    ) -> Option<T> {
        match result {
            Ok(v) => Some(v),
            Err(e) => {
                self.append_status(format!("{context}: {e}"));
                None
            }
        }
    }

    fn clear_status(&mut self) {
        self.status_messages.clear();
    }

    /// Replace all status messages with a single message.
    fn set_status(&mut self, msg: String) {
        self.status_messages.clear();
        self.status_messages.push(msg);
    }

    /// Append a status message (stacks vertically with existing messages).
    fn append_status(&mut self, msg: String) {
        self.status_messages.push(msg);
    }

    /// Save action history to disk. Errors are silently ignored.
    fn save_history(&self) {
        let _ = self.action_history.save(&self.history_path);
    }

    /// Execute the undo or redo, updating op head and persisting.
    /// Called after the guard check passes (or user confirms).
    fn execute_undo_redo(&mut self, is_redo: bool) {
        let result = if is_redo {
            self.action_history
                .redo()
                .map(|(id, label)| (id.to_string(), label.to_string()))
        } else {
            self.action_history
                .undo()
                .map(|(id, label)| (id.to_string(), label.to_string()))
        };
        let Some((op_id, label)) = result else {
            return;
        };
        // Defer to the run_tui drain block so the subprocess runs with the
        // terminal restored. Post-action work (status, history update, refresh,
        // op-log re-entry) is dispatched after the drain block completes.
        self.pending_handoff = Some(PendingHandoff::OpRestore {
            op_id,
            label,
            is_redo,
        });
    }

    /// Check whether undo/redo is safe and either execute immediately
    /// or enter the confirmation dialog.
    fn try_undo_redo(&mut self, is_redo: bool) {
        // Verify there's something to undo/redo.
        let can = if is_redo {
            self.action_history.can_redo()
        } else {
            self.action_history.can_undo()
        };
        if !can {
            return;
        }

        // If no last_op_head recorded, proceed without guard.
        let Some(expected) = self.action_history.last_op_head.as_deref() else {
            self.execute_undo_redo(is_redo);
            return;
        };

        // Compare current op head to expected.
        let current = jujutsu::current_op_id(&self.repo_root);
        if current.as_deref() == Some(expected) {
            self.execute_undo_redo(is_redo);
            return;
        }

        // Op head differs — check if only snapshots intervened.
        match jujutsu::only_snapshots_since(&self.repo_root, expected) {
            Ok(true) => {
                self.execute_undo_redo(is_redo);
            }
            Ok(false) => {
                self.mode = Mode::UndoRedoConfirm { is_redo };
            }
            Err(_) => {
                // Can't determine — show the confirm dialog to be safe.
                self.mode = Mode::UndoRedoConfirm { is_redo };
            }
        }
    }

    /// Refresh all data from the repo (called when op head changes).
    fn refresh(&mut self) {
        self.cached_files.clear();
        // Capture the focused revision identity before rebuilding so we can restore it.
        let focused = self.revision_cursor.and_then(|i| {
            self.workspace_list.selected_workspace().and_then(|ws| {
                ws.revisions
                    .get(i)
                    .map(|r| (ws.name.clone(), r.change_id.clone()))
            })
        });
        if let Ok(mut workspaces) = jujutsu::list_workspaces(&self.repo_root) {
            let selected_name = self
                .workspace_list
                .selected_workspace()
                .map(|ws| ws.name.clone());
            let sort_order = self.workspace_list.sort_order();
            for ws in &mut workspaces {
                ws.is_current = ws.path == self.current_root;
                // Add bookmark derived from workspace name if not already found
                // by the chain-based search.
                if let Some((actual_name, bm_id)) = jj_utils::identify_singular_bookmark(
                    &self.repo_root,
                    &self.config.workspace_path,
                    &self.repo_name,
                    &ws.name,
                ) {
                    let already = ws
                        .bookmarks_at_head
                        .iter()
                        .chain(ws.bookmarks_behind.iter())
                        .any(|(name, _)| name == &actual_name);
                    if !already {
                        ws.classify_bookmark(actual_name, bm_id);
                    }
                }
            }
            let heads_for_sort: Vec<Option<String>>;
            if let Ok((graph, heads)) =
                jujutsu::log_graph_with_heads(&self.repo_root, self.config.log_template.as_deref())
            {
                sort_revisions_by_graph(&heads, &mut workspaces);
                heads_for_sort = heads.clone();
                self.graph_pane = GraphPane::new(&graph, heads);
            } else {
                heads_for_sort = Vec::new();
            }
            self.workspace_list = WorkspaceList::new(
                workspaces,
                selected_name.as_deref(),
                sort_order,
                &heads_for_sort,
            );
            // Restore cursor by change_id if the same workspace+revision still exists.
            if let Some((ref ws_name, ref cid)) = focused {
                self.revision_cursor = self
                    .workspace_list
                    .selected_workspace()
                    .filter(|ws| ws.name == *ws_name)
                    .and_then(|ws| ws.revisions.iter().position(|r| r.change_id == *cid));
            }
            self.sync_graph_to_cursor();
            if self.show_files {
                self.ensure_files_cached();
            }
        } else if let Ok((graph, heads)) =
            jujutsu::log_graph_with_heads(&self.repo_root, self.config.log_template.as_deref())
        {
            self.graph_pane = GraphPane::new(&graph, heads);
            self.sync_graph_to_cursor();
        }
        self.refresh_staleness();
    }

    /// Query all workspaces for staleness and update the visual indicators.
    fn refresh_staleness(&mut self) {
        let ws_paths: Vec<(String, PathBuf)> = self
            .workspace_list
            .workspaces()
            .iter()
            .filter(|w| !w.path.as_os_str().is_empty() && w.path.exists())
            .map(|w| (w.name.clone(), w.path.clone()))
            .collect();
        let stale_names = jujutsu::stale_workspace_names(&ws_paths);
        self.workspace_list.set_stale_names(stale_names);
    }

    /// Drain messages from the background stale-diff thread, if any.
    fn poll_stale_diff(&mut self) {
        let rx = match self.stale_diff_rx {
            Some(ref rx) => rx,
            None => return,
        };
        let mut done = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                jujutsu::StaleDiffMsg::Total(n) => {
                    self.stale_diff_progress = Some((0, n));
                }
                jujutsu::StaleDiffMsg::Checked(n) => {
                    if let Some((ref mut checked, _)) = self.stale_diff_progress {
                        *checked = n;
                    }
                }
                jujutsu::StaleDiffMsg::Done(result) => {
                    if let Some((_, ref ws_path)) = self.update_stale_target {
                        self.update_stale_diff =
                            result.ok().map(|d| UpdateStaleDiffDialog::new(&d, ws_path));
                    }
                    self.stale_diff_progress = None;
                    done = true;
                }
            }
        }
        if done {
            self.stale_diff_rx = None;
        }
    }

    /// Called when the user changes the selected workspace. Syncs the graph
    /// and triggers a deferred staleness poll (at most once per 5 seconds).
    fn on_selection_changed(&mut self) {
        self.revision_cursor = None;
        self.workspace_list.reset_desc_scroll();
        self.sync_graph_to_selection();
        if self.last_selection_stale_poll.elapsed() >= std::time::Duration::from_secs(5) {
            self.last_selection_stale_poll = std::time::Instant::now();
            self.refresh_staleness();
        }
    }

    /// Return the change ID of the currently focused revision (cursor or workspace head).
    fn focused_change_id(&self) -> Option<String> {
        let ws = self.workspace_list.selected_workspace()?;
        if let Some(i) = self.revision_cursor {
            ws.revisions.get(i).map(|r| r.change_id.clone())
        } else {
            Some(ws.change_id.clone())
        }
    }

    /// Fetch and cache the diff summary for the currently focused revision.
    fn ensure_files_cached(&mut self) {
        let Some(change_id) = self.focused_change_id() else {
            return;
        };
        if self.cached_files.contains_key(&change_id) {
            return;
        }
        if let Ok(files) = jujutsu::revision_diff_summary(&self.repo_root, &change_id) {
            self.cached_files.insert(change_id, files);
        }
    }

    /// Build the list of target workspaces for the sync/close dialog.
    fn build_target_list(&self, source_name: &str) -> Vec<TargetWorkspace> {
        self.workspace_list
            .workspaces()
            .iter()
            .filter(|ws| ws.name != source_name)
            .map(|ws| TargetWorkspace {
                name: ws.name.clone(),
                change_id: ws.change_id.clone(),
                path: ws.path.clone(),
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Inline execution (runs jj commands while TUI stays open)
    // -----------------------------------------------------------------------

    /// Defer a `commands::sync::sync_with_info` invocation to the
    /// `run_tui` drain block. Builds a `PendingHandoff::Sync` variant
    /// from the dialog state + App config and stashes it on `self`.
    /// The dialog itself is consumed; `sync_dialog`/`mode` cleanup
    /// happens at the caller.
    fn defer_sync(&mut self, dialog: SyncDialog, label: String) {
        let Some(target) = dialog.selected_target().cloned() else {
            return;
        };
        let Some(info) = dialog.sync_info() else {
            self.set_status("sync mode not detected".to_string());
            return;
        };
        let params = commands::sync::SyncParamsOwned {
            repo_root: self.repo_root.clone(),
            info: info.clone(),
            source_name: dialog.source_name.clone(),
            source_path: dialog.source_path.clone(),
            target_name: target.name,
            target_path: target.path,
            workspace_path_template: self.config.workspace_path.clone(),
            repo_name: self.repo_name.clone(),
            author: self.config.ji_author.clone(),
        };
        self.pending_handoff = Some(PendingHandoff::Sync { params, label });
    }

    /// Defer a `commands::create::create` invocation to the `run_tui`
    /// drain block. Validates the resolved workspace path inline (the
    /// validation is a pure check that doesn't shell out); if validation
    /// fails, sets a status and returns without deferring. Otherwise
    /// builds a `PendingHandoff::Create` variant from the dialog inputs
    /// and stashes it.
    fn defer_create(&mut self, bookmark: &str, revision: &str, ws_path: &Path, msg: &str) {
        // Derive source workspace path: the workspace whose revision we're
        // branching from (used for step-forward when the head is non-trivial).
        let source_ws_path = self
            .workspace_list
            .selected_workspace()
            .map(|w| w.path.clone())
            .unwrap_or_else(|| self.repo_root.clone());

        let ws_path = self.repo_root.join(ws_path);
        let template_arg = if self.create_dialog.path_is_default() {
            Some(self.config.workspace_path.as_str())
        } else {
            None
        };
        if let Err(e) =
            commands::create::validate_resolved_path(&ws_path, &self.repo_root, template_arg)
        {
            self.set_status(format!("{e:#}"));
            return;
        }
        let params = commands::create::CreateParamsOwned {
            repo_root: self.repo_root.clone(),
            config: self.config.clone(),
            repo_name: self.repo_name.clone(),
            bookmark: bookmark.to_string(),
            revision: revision.to_string(),
            source_ws_path,
            ws_path,
            msg: msg.to_string(),
        };
        self.pending_handoff = Some(PendingHandoff::Create { params });
    }

    /// Defer a `commands::close::close_with_info` or
    /// `commands::transfer::transfer_with_info` invocation (dispatch on
    /// dialog intent) to the `run_tui` drain block. Returns true if a
    /// handoff was successfully built; false if some pre-defer validation
    /// failed (and a status was set). The caller dismisses the dialog and
    /// transitions to `Mode::List` regardless — the dialog's job is done
    /// once dispatch is decided.
    fn defer_close_or_transfer(
        &mut self,
        operation: Operation,
        delete_files: bool,
        label: String,
    ) -> bool {
        let Some(dialog) = &self.close_dialog else {
            return false;
        };
        let name = dialog.workspace_name.clone();
        let path = dialog.workspace_path.clone();
        let intent = dialog.intent;
        let target_name = dialog
            .selected_target()
            .map(|t| t.name.clone())
            .unwrap_or_else(|| "default".to_string());
        let target_change_id = dialog
            .selected_target()
            .map(|t| t.change_id.clone())
            .unwrap_or_default();
        let target_ws_path = dialog
            .selected_target()
            .map(|t| t.path.clone())
            .unwrap_or_else(|| self.repo_root.clone());
        let close_info = dialog.close_info.clone();
        let bookmark_action = dialog.bookmark_action;
        let bookmarks = dialog.bookmarks.clone();
        let revisions = dialog.revisions.clone();

        let ws_paths: Vec<(String, PathBuf)> = self
            .workspace_list
            .workspaces()
            .iter()
            .filter(|w| !w.path.as_os_str().is_empty() && w.path.exists())
            .map(|w| (w.name.clone(), w.path.clone()))
            .collect();

        match intent {
            DialogIntent::Close => {
                let Ok(method) = commands::types::CloseMethod::try_from(operation) else {
                    self.set_status(format!("unexpected close operation: {operation:?}"));
                    return false;
                };
                let info = match close_info {
                    Some(info) => info,
                    None => {
                        if !matches!(
                            method,
                            commands::types::CloseMethod::Detach
                                | commands::types::CloseMethod::Abandon
                        ) {
                            self.set_status("sync mode not detected".to_string());
                            return false;
                        }
                        commands::detect_sync_mode(&self.repo_root, &name, &target_name)
                    }
                };
                let params = commands::close::CloseParamsOwned {
                    repo_root: self.repo_root.clone(),
                    source_name: name,
                    source_path: path,
                    target_name,
                    target_path: target_ws_path,
                    target_change_id,
                    method,
                    delete_files,
                    bookmark_action,
                    bookmarks,
                    revisions,
                    workspace_path_template: self.config.workspace_path.clone(),
                    repo_name: self.repo_name.clone(),
                    author: self.config.ji_author.clone(),
                    all_ws_paths: ws_paths,
                };
                self.pending_handoff = Some(PendingHandoff::Close {
                    params,
                    info,
                    label,
                });
                true
            }
            DialogIntent::Transfer => {
                let Ok(method) = commands::types::TransferMethod::try_from(operation) else {
                    self.set_status(format!("unexpected transfer operation: {operation:?}"));
                    return false;
                };
                let Some(info) = close_info else {
                    self.set_status("sync mode not detected".to_string());
                    return false;
                };
                let params = commands::transfer::TransferParamsOwned {
                    repo_root: self.repo_root.clone(),
                    source_name: name,
                    source_path: path,
                    target_name,
                    target_path: target_ws_path,
                    method,
                    workspace_path_template: self.config.workspace_path.clone(),
                    repo_name: self.repo_name.clone(),
                    author: self.config.ji_author.clone(),
                    all_ws_paths: ws_paths,
                };
                self.pending_handoff = Some(PendingHandoff::Transfer {
                    params,
                    info,
                    label,
                });
                true
            }
        }
    }

    // -----------------------------------------------------------------------
    // Event handling
    // -----------------------------------------------------------------------

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::FocusGained => {
                self.terminal_focused = true;
                self.refresh();
            }
            Event::FocusLost => {
                self.terminal_focused = false;
            }
            _ => {}
        }
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        match self.mode {
            Mode::Create => {
                let area = self.terminal_area;
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some((fi, off)) =
                            self.create_dialog.hit_test(area, mouse.column, mouse.row)
                        {
                            self.create_dialog.click_at(fi, off);
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some((fi, off)) =
                            self.create_dialog.hit_test(area, mouse.column, mouse.row)
                        {
                            self.create_dialog.drag_to(fi, off);
                        }
                    }
                    _ => {}
                }
            }
            Mode::List => {
                let pos = Position::new(mouse.column, mouse.row);
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if self.workspace_area.contains(pos) {
                            self.focus = Focus::Workspaces;
                            if self.left_panel == LeftPanel::Workspaces {
                                let inner_y =
                                    (mouse.row.saturating_sub(self.workspace_area.y + 2)) as usize;
                                self.workspace_list.select_index(inner_y);
                                self.on_selection_changed();
                                if self.show_files {
                                    self.ensure_files_cached();
                                }
                            } else if self.left_panel == LeftPanel::OpLog
                                && let Some(pane) = &self.op_log_pane
                            {
                                let la = pane.list_area();
                                if la.contains(pos) {
                                    // +2: skip border + header row
                                    let inner_y = (mouse.row.saturating_sub(la.y + 2)) as usize;
                                    if let Some(pane) = &mut self.op_log_pane {
                                        pane.select_visual_row(inner_y);
                                        self.op_graph_debounce = std::time::Instant::now();
                                    }
                                }
                            }
                        } else if self.graph_area.contains(pos) && !self.show_help {
                            if self.focus == Focus::Graph {
                                self.click_graph_revision(mouse.row);
                            } else {
                                self.focus = Focus::Graph;
                            }
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        if self.left_panel == LeftPanel::OpLog {
                            if let Some(pane) = &self.op_log_pane {
                                if pane.list_area().contains(pos) {
                                    if let Some(pane) = &mut self.op_log_pane {
                                        pane.scroll_list_up();
                                        self.op_graph_debounce = std::time::Instant::now();
                                    }
                                } else if pane.detail_area().contains(pos) {
                                    if let Some(pane) = &mut self.op_log_pane {
                                        pane.scroll_detail_up();
                                    }
                                } else if self.graph_area.contains(pos) && !self.show_help {
                                    self.graph_pane.scroll_up();
                                }
                            }
                        } else if self.workspace_list.desc_area().contains(pos) {
                            self.workspace_list.scroll_desc_up();
                        } else if self.graph_area.contains(pos) && !self.show_help {
                            self.graph_pane.scroll_up();
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if self.left_panel == LeftPanel::OpLog {
                            if let Some(pane) = &self.op_log_pane {
                                if pane.list_area().contains(pos) {
                                    if let Some(pane) = &mut self.op_log_pane {
                                        pane.scroll_list_down();
                                        self.op_graph_debounce = std::time::Instant::now();
                                    }
                                } else if pane.detail_area().contains(pos) {
                                    if let Some(pane) = &mut self.op_log_pane {
                                        pane.scroll_detail_down();
                                    }
                                } else if self.graph_area.contains(pos) && !self.show_help {
                                    self.graph_pane.scroll_down(self.graph_visible_height);
                                }
                            }
                        } else if self.workspace_list.desc_area().contains(pos) {
                            self.workspace_list.scroll_desc_down();
                        } else if self.graph_area.contains(pos) && !self.show_help {
                            self.graph_pane.scroll_down(self.graph_visible_height);
                        }
                    }
                    _ => {}
                }
            }
            Mode::Bookmarks => {
                let pos = Position::new(mouse.column, mouse.row);
                if let Some(dialog) = &mut self.bookmarks_dialog
                    && dialog.new_popup.is_none()
                {
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            if let Some(zone) =
                                dialog.hit_test(self.workspace_area, mouse.column, mouse.row)
                            {
                                match zone {
                                    bookmarks_dialog::HitZone::NewBookmark => {
                                        dialog.open_new_popup();
                                    }
                                    bookmarks_dialog::HitZone::ToggleTug => {
                                        dialog.action = bookmarks_dialog::BookmarkAction::Tug;
                                    }
                                    bookmarks_dialog::HitZone::ToggleDelete => {
                                        dialog.action = bookmarks_dialog::BookmarkAction::Delete;
                                    }
                                    bookmarks_dialog::HitZone::BookmarkEntry(idx) => {
                                        dialog.click_entry(idx);
                                    }
                                }
                            }
                        }
                        MouseEventKind::ScrollUp => {
                            if self.graph_area.contains(pos) && !self.show_help {
                                self.graph_pane.scroll_up();
                            } else {
                                dialog.move_up();
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            if self.graph_area.contains(pos) && !self.show_help {
                                self.graph_pane.scroll_down(self.graph_visible_height);
                            } else {
                                dialog.move_down();
                            }
                        }
                        _ => {}
                    }
                }
            }
            Mode::UpdateStale => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    if let Some(ref mut dlg) = self.update_stale_diff {
                        dlg.handle_key(KeyCode::Up);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(ref mut dlg) = self.update_stale_diff {
                        dlg.handle_key(KeyCode::Down);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match self.mode {
            Mode::List if self.left_panel == LeftPanel::OpLog => {
                match key.code {
                    KeyCode::Char('o') | KeyCode::Esc if self.focus == Focus::Workspaces => {
                        self.leave_op_log();
                    }
                    KeyCode::Esc if self.focus == Focus::Graph => {
                        self.focus = Focus::Workspaces;
                    }
                    KeyCode::Tab if !self.show_help => {
                        self.focus = match self.focus {
                            Focus::Workspaces => Focus::Graph,
                            Focus::Graph => Focus::Workspaces,
                        };
                    }
                    KeyCode::Char('?') | KeyCode::Char('/') => {
                        self.show_help = !self.show_help;
                        if self.show_help {
                            self.focus = Focus::Workspaces;
                        }
                    }
                    KeyCode::Char('s') if self.focus == Focus::Workspaces => {
                        if let Some(pane) = &mut self.op_log_pane {
                            pane.toggle_snapshots();
                            self.op_graph_debounce = std::time::Instant::now();
                        }
                    }
                    KeyCode::Char('R') if self.focus == Focus::Workspaces => {
                        if self
                            .op_log_pane
                            .as_ref()
                            .and_then(|p| p.selected_operation())
                            .is_some()
                        {
                            self.mode = Mode::OpRestore;
                        }
                    }
                    // Graph scroll
                    KeyCode::Up if self.focus == Focus::Graph => {
                        self.graph_pane.scroll_up();
                    }
                    KeyCode::Down if self.focus == Focus::Graph => {
                        self.graph_pane.scroll_down(self.graph_visible_height);
                    }
                    KeyCode::Char('k') if self.focus == Focus::Graph => {
                        self.graph_pane.scroll_up();
                    }
                    KeyCode::Char('j') if self.focus == Focus::Graph => {
                        self.graph_pane.scroll_down(self.graph_visible_height);
                    }
                    KeyCode::Char('u') if self.focus == Focus::Graph => {
                        self.graph_pane.scroll_up_half(self.graph_visible_height);
                    }
                    KeyCode::Char('d') if self.focus == Focus::Graph => {
                        self.graph_pane.scroll_down_half(self.graph_visible_height);
                    }
                    // Op log list navigation
                    KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k')
                        if self.focus == Focus::Workspaces =>
                    {
                        if let Some(pane) = &mut self.op_log_pane
                            && pane.handle_key(key.code)
                        {
                            self.op_graph_debounce = std::time::Instant::now();
                        }
                    }
                    KeyCode::Char('Z') if self.focus == Focus::Workspaces => {
                        self.try_undo_redo(false);
                    }
                    KeyCode::Char('Y') if self.focus == Focus::Workspaces => {
                        self.try_undo_redo(true);
                    }
                    _ => {}
                }
            }
            Mode::List => {
                match key.code {
                    KeyCode::Char('q') => {
                        self.action = Some(Action::Quit);
                    }
                    KeyCode::Esc if self.focus == Focus::Graph => {
                        self.focus = Focus::Workspaces;
                    }
                    KeyCode::Tab if !self.show_help => {
                        self.focus = match self.focus {
                            Focus::Workspaces => Focus::Graph,
                            Focus::Graph => Focus::Workspaces,
                        };
                    }
                    // Arrow keys: always route to the focused pane
                    KeyCode::Up if self.focus == Focus::Graph => {
                        self.graph_pane.scroll_up();
                    }
                    KeyCode::Down if self.focus == Focus::Graph => {
                        self.graph_pane.scroll_down(self.graph_visible_height);
                    }
                    KeyCode::Up | KeyCode::Down if self.focus == Focus::Workspaces => {
                        self.workspace_list.handle_key(key.code);
                        self.on_selection_changed();
                        if self.show_files {
                            self.ensure_files_cached();
                        }
                    }
                    KeyCode::Right => {
                        if let Some(ws) = self.workspace_list.selected_workspace()
                            && !ws.revisions.is_empty()
                        {
                            let max = ws.revisions.len() - 1;
                            self.revision_cursor = Some(match self.revision_cursor {
                                None => 0,
                                Some(i) if i < max => i + 1,
                                Some(i) => i,
                            });
                            self.workspace_list.reset_desc_scroll();
                            self.sync_graph_to_revision();
                            if self.show_files {
                                self.ensure_files_cached();
                            }
                        }
                    }
                    KeyCode::Left => {
                        if self.revision_cursor.is_some() {
                            self.revision_cursor = match self.revision_cursor {
                                Some(0) => None,
                                Some(i) => Some(i - 1),
                                None => None,
                            };
                            self.sync_graph_to_cursor();
                            self.workspace_list.reset_desc_scroll();
                            if self.show_files {
                                self.ensure_files_cached();
                            }
                        }
                    }
                    // Vim keys: graph scroll when graph focused, workspace nav when workspace focused
                    KeyCode::Char('k') if self.focus == Focus::Graph => {
                        self.graph_pane.scroll_up();
                    }
                    KeyCode::Char('j') if self.focus == Focus::Graph => {
                        self.graph_pane.scroll_down(self.graph_visible_height);
                    }
                    KeyCode::Char('u') if self.focus == Focus::Graph => {
                        self.graph_pane.scroll_up_half(self.graph_visible_height);
                    }
                    KeyCode::Char('d') if self.focus == Focus::Graph => {
                        self.graph_pane.scroll_down_half(self.graph_visible_height);
                    }
                    KeyCode::Char('P') if self.focus == Focus::Workspaces => {
                        self.clear_status();
                    }
                    KeyCode::Char('p') if self.focus == Focus::Workspaces => {
                        if !self.status_messages.is_empty() {
                            copy_dialog::copy_to_clipboard(&self.status_messages.join("\n"));
                        }
                    }
                    KeyCode::Char('u') if self.focus == Focus::Workspaces => {
                        if let Some(ws) = self.workspace_list.selected_workspace()
                            && self.workspace_list.is_stale(&ws.name)
                        {
                            self.update_stale_target = Some((ws.name.clone(), ws.path.clone()));
                            self.update_stale_diff = None;
                            self.mode = Mode::UpdateStale;
                        }
                    }
                    KeyCode::Char('?') | KeyCode::Char('/') => {
                        self.show_help = !self.show_help;
                        if self.show_help {
                            self.focus = Focus::Workspaces;
                        }
                    }
                    // Workspace actions — only when workspace pane is focused
                    KeyCode::Enter if self.focus == Focus::Workspaces => {
                        if let Some(ws) = self.workspace_list.selected_workspace() {
                            self.action = Some(Action::SwitchTo(ws.path.clone()));
                        }
                    }
                    KeyCode::Char('n') if self.stale && self.focus == Focus::Workspaces => {
                        self.mode = Mode::StaleAlert;
                    }
                    KeyCode::Char('n') if self.focus == Focus::Workspaces => {
                        let ws = self.workspace_list.selected_workspace();
                        let revision = if let Some(cursor) = self.revision_cursor {
                            ws.and_then(|w| w.revisions.get(cursor))
                                .map(|r| r.change_id.as_str())
                                .unwrap_or("@")
                        } else {
                            ws.map(|w| w.change_id.as_str()).unwrap_or("@")
                        };
                        let choices: Vec<String> = ws
                            .map(|w| w.revisions.iter().map(|r| r.change_id.clone()).collect())
                            .unwrap_or_default();
                        let home = std::env::var("HOME").unwrap_or_default();
                        self.create_dialog.reset(
                            &self.config.workspace_path,
                            &self.repo_name,
                            revision,
                            &home,
                            &self.repo_root.to_string_lossy(),
                        );
                        self.create_dialog.set_revision_choices(choices);
                        if self.config.warnings.is_empty() {
                            self.mode = Mode::Create;
                        } else {
                            self.mode = Mode::ConfigWarning;
                        }
                    }
                    KeyCode::Char('s') if self.focus == Focus::Workspaces => {
                        self.refresh();
                        if let Some(ws) = self.workspace_list.selected_workspace() {
                            if self.stale {
                                self.mode = Mode::StaleAlert;
                            } else {
                                let targets = self.build_target_list(&ws.name);
                                if !targets.is_empty() {
                                    let candidate_ids: Vec<&str> =
                                        targets.iter().map(|t| t.change_id.as_str()).collect();
                                    let parent_id = jujutsu::closest_ancestor_workspace(
                                        &self.repo_root,
                                        &ws.change_id,
                                        &candidate_ids,
                                    );
                                    let default_target_idx = parent_id
                                        .and_then(|id| {
                                            targets.iter().position(|t| t.change_id == id)
                                        })
                                        .or_else(|| {
                                            targets.iter().position(|t| t.name == "default")
                                        })
                                        .unwrap_or(0);
                                    self.sync_dialog = Some(SyncDialog::new(
                                        ws.name.clone(),
                                        ws.path.clone(),
                                        targets,
                                        default_target_idx,
                                        self.repo_root.clone(),
                                        self.config.workspace_path.clone(),
                                        self.repo_name.clone(),
                                    ));
                                    self.mode = Mode::Sync;
                                    self.sync_graph_to_sync();
                                }
                            }
                        }
                    }
                    KeyCode::Char('t') if self.focus == Focus::Workspaces => {
                        // Refresh before opening dialog to ensure workspace data is current.
                        self.refresh();
                        if let Some(ws) = self.workspace_list.selected_workspace() {
                            if ws.name == "default" {
                                return;
                            }
                            let orphaned = ws.path.as_os_str().is_empty();
                            if orphaned {
                                // Can't transfer an orphaned workspace — no-op.
                            } else if self.stale {
                                self.mode = Mode::StaleAlert;
                            } else {
                                let targets = self.build_target_list(&ws.name);
                                let candidate_ids: Vec<&str> =
                                    targets.iter().map(|t| t.change_id.as_str()).collect();
                                let parent_id = jujutsu::closest_ancestor_workspace(
                                    &self.repo_root,
                                    &ws.change_id,
                                    &candidate_ids,
                                );
                                let default_target_idx = parent_id
                                    .and_then(|id| targets.iter().position(|t| t.change_id == id))
                                    .or_else(|| targets.iter().position(|t| t.name == "default"))
                                    .unwrap_or(0);
                                let singular_bookmark = jj_utils::identify_singular_bookmark(
                                    &self.repo_root,
                                    &self.config.workspace_path,
                                    &self.repo_name,
                                    &ws.name,
                                )
                                .map(|(name, _)| name);
                                self.close_dialog = Some(CloseDialog::new(
                                    ws.name.clone(),
                                    ws.path.clone(),
                                    ws.revisions.clone(),
                                    DialogIntent::Transfer,
                                    targets,
                                    default_target_idx,
                                    self.repo_root.clone(),
                                    vec![],
                                    singular_bookmark,
                                    self.config.workspace_path.clone(),
                                    self.repo_name.clone(),
                                ));
                                self.mode = Mode::Close;
                                self.sync_graph_to_close();
                            }
                        }
                    }
                    KeyCode::Char('b') if self.stale && self.focus == Focus::Workspaces => {
                        self.mode = Mode::StaleAlert;
                    }
                    KeyCode::Char('b') if self.focus == Focus::Workspaces => {
                        if let Some(ws) = self.workspace_list.selected_workspace() {
                            let eff = jj_utils::find_effective_head(&self.repo_root, &ws.name).ok();
                            self.bookmarks_dialog = Some(BookmarksDialog::new(ws, eff));
                            self.mode = Mode::Bookmarks;
                        }
                    }
                    KeyCode::Char('c') if self.focus == Focus::Workspaces => {
                        if let Some(ws) = self.workspace_list.selected_workspace() {
                            self.copy_dialog = Some(CopyDialog::new(ws));
                            self.mode = Mode::Copy;
                        }
                    }
                    KeyCode::Char('v') if self.stale && self.focus == Focus::Workspaces => {
                        self.mode = Mode::StaleAlert;
                    }
                    KeyCode::Char('v') if self.focus == Focus::Workspaces => {
                        if let Some(ws) = self.workspace_list.selected_workspace()
                            && !ws.revisions.is_empty()
                        {
                            self.revision_picker =
                                Some(RevisionPicker::new(ws.name.clone(), ws.revisions.clone()));
                            self.mode = Mode::Split;
                        }
                    }
                    KeyCode::Char('x') if self.focus == Focus::Workspaces => {
                        self.refresh();
                        if let Some(ws) = self.workspace_list.selected_workspace() {
                            if ws.name == "default" {
                                return;
                            }
                            let orphaned = ws.path.as_os_str().is_empty();
                            if orphaned {
                                // Orphaned workspace: just forget it, no dialog needed.
                                // Bypass stale check — forgetting doesn't touch working copy.
                                let name = ws.name.clone();
                                self.pending_handoff =
                                    Some(PendingHandoff::WorkspaceForget { name });
                            } else if self.stale {
                                self.mode = Mode::StaleAlert;
                            } else {
                                let targets = self.build_target_list(&ws.name);
                                let candidate_ids: Vec<&str> =
                                    targets.iter().map(|t| t.change_id.as_str()).collect();
                                let parent_id = jujutsu::closest_ancestor_workspace(
                                    &self.repo_root,
                                    &ws.change_id,
                                    &candidate_ids,
                                );
                                let default_target_idx = parent_id
                                    .and_then(|id| targets.iter().position(|t| t.change_id == id))
                                    .or_else(|| targets.iter().position(|t| t.name == "default"))
                                    .unwrap_or(0);
                                let singular_bookmark = jj_utils::identify_singular_bookmark(
                                    &self.repo_root,
                                    &self.config.workspace_path,
                                    &self.repo_name,
                                    &ws.name,
                                )
                                .map(|(name, _)| name);
                                let bookmarks: Vec<String> = ws
                                    .bookmarks_at_head
                                    .iter()
                                    .chain(ws.bookmarks_behind.iter())
                                    .filter(|(name, _)| singular_bookmark.as_ref() != Some(name))
                                    .map(|(name, _)| name.clone())
                                    .collect();
                                self.close_dialog = Some(CloseDialog::new(
                                    ws.name.clone(),
                                    ws.path.clone(),
                                    ws.revisions.clone(),
                                    DialogIntent::Close,
                                    targets,
                                    default_target_idx,
                                    self.repo_root.clone(),
                                    bookmarks,
                                    singular_bookmark,
                                    self.config.workspace_path.clone(),
                                    self.repo_name.clone(),
                                ));
                                self.mode = Mode::Close;
                                self.sync_graph_to_close();
                            }
                        }
                    }
                    KeyCode::Char('i') if self.focus == Focus::Workspaces => {
                        self.show_files = !self.show_files;
                        self.workspace_list.reset_desc_scroll();
                        if self.show_files {
                            self.ensure_files_cached();
                        }
                    }
                    KeyCode::Char('r') if self.focus == Focus::Workspaces => {
                        self.revision_cursor = None;
                        self.workspace_list.toggle_sort();
                        self.sync_graph_to_selection();
                        if self.show_files {
                            self.ensure_files_cached();
                        }
                    }
                    KeyCode::Char('o') if self.focus == Focus::Workspaces => {
                        self.enter_op_log();
                    }
                    KeyCode::Char('Z') if self.focus == Focus::Workspaces => {
                        self.try_undo_redo(false);
                    }
                    KeyCode::Char('Y') if self.focus == Focus::Workspaces => {
                        self.try_undo_redo(true);
                    }
                    _ => {}
                }
            }
            Mode::Copy => match key.code {
                KeyCode::Esc => {
                    self.copy_dialog = None;
                    self.mode = Mode::List;
                }
                KeyCode::Enter => {
                    if let Some(dialog) = &self.copy_dialog
                        && let Some(value) = dialog.selected_value()
                    {
                        copy_dialog::copy_to_clipboard(value);
                    }
                    self.copy_dialog = None;
                    self.mode = Mode::List;
                }
                other => {
                    if let Some(dialog) = &mut self.copy_dialog {
                        dialog.handle_key(other);
                    }
                }
            },
            Mode::Bookmarks => {
                if let Some(dialog) = &mut self.bookmarks_dialog {
                    if dialog.new_popup.is_some() {
                        // Delegate to new-bookmark popup
                        match key.code {
                            KeyCode::Esc => {
                                dialog.close_new_popup();
                                self.sync_graph_to_selection();
                            }
                            KeyCode::Enter => {
                                if let Some((name, change_id)) = dialog.confirm_new_popup() {
                                    self.pending_handoff =
                                        Some(PendingHandoff::BookmarkCreate { name, change_id });
                                    self.bookmarks_dialog = None;
                                    self.mode = Mode::List;
                                }
                            }
                            KeyCode::Tab | KeyCode::Up | KeyCode::Down => {
                                dialog.new_popup_toggle_field();
                            }
                            KeyCode::Right => {
                                if let Some(change_id) = dialog.new_popup_cycle_revision(1) {
                                    let change_id = change_id.to_string();
                                    self.graph_pane
                                        .scroll_to_change_id(&change_id, self.graph_visible_height);
                                    let ids: HashSet<&str> = HashSet::from([change_id.as_str()]);
                                    self.graph_pane.highlight(&ids, false);
                                }
                            }
                            KeyCode::Left => {
                                if let Some(change_id) = dialog.new_popup_cycle_revision(-1) {
                                    let change_id = change_id.to_string();
                                    self.graph_pane
                                        .scroll_to_change_id(&change_id, self.graph_visible_height);
                                    let ids: HashSet<&str> = HashSet::from([change_id.as_str()]);
                                    self.graph_pane.highlight(&ids, false);
                                }
                            }
                            KeyCode::Char(c) => {
                                if key.modifiers.contains(KeyModifiers::CONTROL) {
                                    match c {
                                        'u' => dialog.new_popup_delete_to_start(),
                                        'k' => dialog.new_popup_delete_to_end(),
                                        'a' => dialog.new_popup_move_home(),
                                        'e' => dialog.new_popup_move_end(),
                                        _ => {}
                                    }
                                } else {
                                    dialog.new_popup_insert_char(c);
                                }
                            }
                            KeyCode::Backspace => {
                                dialog.new_popup_delete_char();
                            }
                            KeyCode::Home => {
                                dialog.new_popup_move_home();
                            }
                            KeyCode::End => {
                                dialog.new_popup_move_end();
                            }
                            _ => {}
                        }
                    } else {
                        match key.code {
                            KeyCode::Esc => {
                                self.bookmarks_dialog = None;
                                self.mode = Mode::List;
                            }
                            KeyCode::Char('n') => {
                                dialog.open_new_popup();
                            }
                            KeyCode::Char('t') => {
                                dialog.action = bookmarks_dialog::BookmarkAction::Tug;
                            }
                            KeyCode::Char('x') => {
                                dialog.action = bookmarks_dialog::BookmarkAction::Delete;
                            }
                            KeyCode::Left => {
                                dialog.action = bookmarks_dialog::BookmarkAction::Tug;
                            }
                            KeyCode::Right => {
                                dialog.action = bookmarks_dialog::BookmarkAction::Delete;
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                dialog.move_up();
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                dialog.move_down();
                            }
                            KeyCode::Char(' ') => {
                                dialog.toggle_selection();
                            }
                            KeyCode::Char('a') => {
                                dialog.toggle_select_all();
                            }
                            KeyCode::Char('y') | KeyCode::Enter => {
                                if dialog.has_selection() {
                                    let action = dialog.action;
                                    let names = dialog.selected_bookmark_names();
                                    let handoff = match action {
                                        bookmarks_dialog::BookmarkAction::Tug => {
                                            PendingHandoff::BookmarkTug {
                                                names,
                                                head_id: dialog.head_id.clone(),
                                            }
                                        }
                                        bookmarks_dialog::BookmarkAction::Delete => {
                                            PendingHandoff::BookmarkDelete { names }
                                        }
                                    };
                                    self.pending_handoff = Some(handoff);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            Mode::Sync => match key.code {
                KeyCode::Esc => {
                    self.sync_dialog = None;
                    self.sync_graph_to_selection();
                    self.mode = Mode::List;
                }
                KeyCode::Enter => {
                    if let Some(dialog) = self.sync_dialog.take() {
                        if dialog.has_operation() {
                            let label = format!(
                                "sync {} → {}",
                                dialog.source_name,
                                dialog
                                    .selected_target()
                                    .map(|t| t.name.as_str())
                                    .unwrap_or("?")
                            );
                            self.defer_sync(dialog, label);
                            self.sync_dialog = None;
                            self.mode = Mode::List;
                        } else {
                            self.sync_dialog = Some(dialog);
                        }
                    }
                }
                other => {
                    if let Some(dialog) = &mut self.sync_dialog {
                        dialog.handle_key(other);
                        dialog.recompute();
                    }
                    self.sync_graph_to_sync();
                }
            },
            Mode::Create => match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::List;
                }
                KeyCode::Enter => {
                    let vals = self.create_dialog.values();
                    if !vals.bookmark.is_empty() && !vals.path.is_empty() {
                        self.refresh_staleness();
                        self.defer_create(
                            &vals.bookmark,
                            &vals.revision,
                            Path::new(&vals.path),
                            &vals.msg,
                        );
                        self.mode = Mode::List;
                    }
                }
                KeyCode::Tab | KeyCode::Down => {
                    self.create_dialog.next_field();
                }
                KeyCode::BackTab | KeyCode::Up => {
                    self.create_dialog.prev_field();
                }
                KeyCode::Left => {
                    if self.create_dialog.active_is_revision() {
                        if let Some(id) = self.create_dialog.cycle_revision(-1) {
                            let id = id.to_string();
                            self.graph_pane
                                .scroll_to_change_id(&id, self.graph_visible_height);
                            let ids: HashSet<&str> = HashSet::from([id.as_str()]);
                            self.graph_pane.highlight(&ids, false);
                        }
                    } else {
                        self.create_dialog.move_left();
                    }
                }
                KeyCode::Right => {
                    if self.create_dialog.active_is_revision() {
                        if let Some(id) = self.create_dialog.cycle_revision(1) {
                            let id = id.to_string();
                            self.graph_pane
                                .scroll_to_change_id(&id, self.graph_visible_height);
                            let ids: HashSet<&str> = HashSet::from([id.as_str()]);
                            self.graph_pane.highlight(&ids, false);
                        }
                    } else {
                        self.create_dialog.move_right();
                    }
                }
                KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.create_dialog.move_home();
                }
                KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.create_dialog.move_end();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.create_dialog.delete_to_start();
                }
                KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.create_dialog.delete_to_end();
                }
                KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.create_dialog.delete_word_backward();
                }
                KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                    self.create_dialog.move_word_backward();
                }
                KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                    self.create_dialog.move_word_forward();
                }
                KeyCode::Char(c) => {
                    self.create_dialog.insert_char(c);
                }
                KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
                    self.create_dialog.delete_word_backward();
                }
                KeyCode::Backspace => {
                    self.create_dialog.delete_char();
                }
                _ => {}
            },
            Mode::Close => {
                if let Some(dialog) = &mut self.close_dialog {
                    match key.code {
                        // Navigate operations
                        KeyCode::Up => {
                            dialog.move_up();
                        }
                        KeyCode::Down => {
                            dialog.move_down();
                        }
                        // Cycle target workspace
                        KeyCode::Left if dialog.targets.len() > 1 => {
                            dialog.cycle_target_back();
                            dialog.recompute_close_mode();
                            self.sync_graph_to_close();
                        }
                        KeyCode::Right if dialog.targets.len() > 1 => {
                            dialog.cycle_target();
                            dialog.recompute_close_mode();
                            self.sync_graph_to_close();
                        }
                        // Key shortcuts to jump to operation
                        KeyCode::Char('a') => {
                            let op = match dialog.intent {
                                DialogIntent::Transfer => Operation::AdaptiveMerge,
                                DialogIntent::Close => Operation::AdaptiveClose,
                            };
                            dialog.jump_to(op);
                        }
                        KeyCode::Char(c @ '1'..='4') => {
                            dialog.jump_to_key(c);
                        }
                        KeyCode::Char('d') if dialog.intent == DialogIntent::Close => {
                            dialog.jump_to(Operation::Detach);
                        }
                        // Confirm highlighted operation
                        KeyCode::Char('y') | KeyCode::Enter => {
                            if let Some(op) = dialog.selected_op() {
                                let delete_files = dialog.delete_files;
                                let label = match dialog.intent {
                                    DialogIntent::Close => {
                                        format!("close {}", dialog.workspace_name)
                                    }
                                    DialogIntent::Transfer => {
                                        let tgt = dialog
                                            .selected_target()
                                            .map(|t| t.name.as_str())
                                            .unwrap_or("?");
                                        format!("transfer {} → {tgt}", dialog.workspace_name)
                                    }
                                };
                                let _ = self.defer_close_or_transfer(op, delete_files, label);
                                // Always dismiss the dialog and return to
                                // List; if the drain produces a pending
                                // remove_path it'll switch to
                                // ConfirmRemoveFiles after the handoff.
                                self.close_dialog = None;
                                self.mode = Mode::List;
                            }
                        }
                        // Toggles
                        KeyCode::Char('b')
                            if dialog.intent == DialogIntent::Close
                                && !dialog.bookmarks.is_empty() =>
                        {
                            dialog.cycle_bookmark_action();
                        }
                        KeyCode::Char('k') if dialog.intent == DialogIntent::Close => {
                            dialog.toggle_delete_files();
                        }
                        // Copy displayed commands to clipboard
                        KeyCode::Char('c') => {
                            if let Some(op) = dialog.selected_op() {
                                let cmds = dialog.planned_commands(&op);
                                if !cmds.is_empty() {
                                    let text = cmd_spans::lines_to_plain(&cmds);
                                    copy_dialog::copy_to_clipboard(&text);
                                    self.set_status("copied to clipboard".to_string());
                                }
                            }
                        }
                        KeyCode::Esc => {
                            self.mode = Mode::List;
                            self.close_dialog = None;
                        }
                        _ => {}
                    }
                }
            }
            Mode::ConfirmRemoveFiles => match key.code {
                KeyCode::Char('y') => {
                    if let Some(path) = self.pending_remove_path.take() {
                        if path == self.current_root {
                            self.action = Some(Action::SwitchTo(self.repo_root.clone()));
                        }
                        // Defer remove_dir_all to drain block (terminal handoff).
                        // Mode transition stays inline so the next render shows List
                        // (rather than ConfirmRemoveFiles with a now-empty path).
                        self.pending_handoff = Some(PendingHandoff::RemoveDirAll { path });
                    }
                    self.mode = Mode::List;
                }
                _ => {
                    if let Some(path) = &self.pending_remove_path
                        && *path == self.current_root
                    {
                        self.action = Some(Action::SwitchTo(self.repo_root.clone()));
                    }
                    self.pending_remove_path = None;
                    self.refresh();
                    self.mode = Mode::List;
                }
            },
            Mode::Split => {
                if let Some(picker) = &mut self.revision_picker {
                    match key.code {
                        KeyCode::Up | KeyCode::Char('k') => picker.up(),
                        KeyCode::Down | KeyCode::Char('j') => picker.down(),
                        KeyCode::Enter => {
                            if let Some(rev) = picker.selected_revision() {
                                self.pending_handoff = Some(PendingHandoff::Split {
                                    change_id: rev.change_id.clone(),
                                });
                            }
                            self.revision_picker = None;
                            self.mode = Mode::List;
                        }
                        KeyCode::Esc => {
                            self.revision_picker = None;
                            self.mode = Mode::List;
                        }
                        _ => {}
                    }
                }
            }
            Mode::StaleAlert => match key.code {
                KeyCode::Char('r') => {
                    // Defer: save_all_stale_diffs + update_stale as one fused
                    // terminal handoff (UpdateStaleSequence variant).
                    self.pending_handoff = Some(PendingHandoff::UpdateStaleSequence);
                }
                KeyCode::Char('q') | KeyCode::Esc => {
                    self.action = Some(Action::Quit);
                }
                _ => {}
            },
            Mode::UpdateStale => match key.code {
                KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k') => {
                    if let Some(ref mut dlg) = self.update_stale_diff {
                        dlg.handle_key(key.code);
                    }
                }
                KeyCode::Char('d')
                    if self.update_stale_diff.is_none() && self.stale_diff_rx.is_none() =>
                {
                    if let Some((_, ref ws_path)) = self.update_stale_target {
                        let (tx, rx) = std::sync::mpsc::channel();
                        let path = ws_path.clone();
                        std::thread::spawn(move || {
                            jujutsu::stale_workspace_diff_threaded(path, tx);
                        });
                        self.stale_diff_rx = Some(rx);
                        self.stale_diff_progress = Some((0, 0));
                    }
                }
                KeyCode::Enter => {
                    if let Some(ref dlg) = self.update_stale_diff
                        && let Some((ws_path, kind, rel_path)) = dlg.save_diff_args()
                    {
                        self.pending_handoff = Some(PendingHandoff::SaveDiff {
                            ws_path,
                            kind,
                            rel_path,
                        });
                    }
                }
                KeyCode::Char('a') => {
                    if let Some(ref dlg) = self.update_stale_diff {
                        let (ws_path, items) = dlg.save_all_diffs_args();
                        self.pending_handoff =
                            Some(PendingHandoff::SaveAllDiffs { ws_path, items });
                    }
                }
                KeyCode::Char('y') => {
                    if let Some((name, path)) = self.update_stale_target.take() {
                        self.pending_handoff = Some(PendingHandoff::UpdateWorkspaceStale {
                            ws_path: path,
                            name,
                        });
                    }
                    self.update_stale_diff = None;
                    self.stale_diff_rx = None;
                    self.stale_diff_progress = None;
                    self.mode = Mode::List;
                }
                KeyCode::Esc => {
                    self.update_stale_target = None;
                    self.update_stale_diff = None;
                    self.stale_diff_rx = None;
                    self.stale_diff_progress = None;
                    self.mode = Mode::List;
                }
                _ => {}
            },
            Mode::ConfigWarning => match key.code {
                KeyCode::Enter => {
                    let has_blocking = self
                        .config
                        .warnings
                        .iter()
                        .any(|w| w.kind == hooks::WarningKind::MissingBookmark);
                    if !has_blocking {
                        self.config.warnings.clear();
                        self.mode = Mode::Create;
                    }
                }
                KeyCode::Esc => {
                    self.mode = Mode::List;
                }
                _ => {}
            },
            Mode::OpRestore => match key.code {
                KeyCode::Enter => {
                    if let Some(op) = self
                        .op_log_pane
                        .as_ref()
                        .and_then(|p| p.selected_operation())
                    {
                        let op_id = op.id.clone();
                        self.pending_handoff = Some(PendingHandoff::OpRestoreFromLog { op_id });
                    }
                    self.mode = Mode::List;
                }
                KeyCode::Esc => {
                    self.mode = Mode::List;
                }
                _ => {}
            },
            Mode::UndoRedoConfirm { is_redo } => match key.code {
                KeyCode::Enter => {
                    self.mode = Mode::List;
                    self.execute_undo_redo(is_redo);
                }
                KeyCode::Esc => {
                    self.mode = Mode::List;
                }
                _ => {}
            },
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        self.terminal_area = frame.area();
        let content_area = if self.status_messages.is_empty() {
            frame.area()
        } else {
            let n = self.status_messages.len() as u16;
            let outer =
                Layout::vertical([Constraint::Min(1), Constraint::Length(n)]).split(frame.area());
            let lines: Vec<Line> = self
                .status_messages
                .iter()
                .map(|msg| Line::from(Span::styled(msg.as_str(), Style::default().fg(Color::Red))))
                .collect();
            frame.render_widget(Paragraph::new(lines), outer[1]);
            outer[0]
        };
        let main_chunks =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(content_area);

        // Left panel: workspace list or op log
        match self.left_panel {
            LeftPanel::Workspaces => {
                let selected_stale = self
                    .workspace_list
                    .selected_workspace()
                    .is_some_and(|ws| self.workspace_list.is_stale(&ws.name));
                let has_status = !self.status_messages.is_empty();
                let (desc_override_text, diff_summary);
                let desc_override;
                if self.show_files {
                    let change_id = self.focused_change_id();
                    diff_summary = change_id.as_ref().and_then(|id| self.cached_files.get(id));
                    desc_override = None;
                } else {
                    diff_summary = None;
                    desc_override_text = self.revision_cursor.and_then(|i| {
                        self.workspace_list
                            .selected_workspace()
                            .and_then(|ws| ws.revisions.get(i))
                            .map(|rev| rev.description.clone())
                    });
                    desc_override = desc_override_text.as_deref();
                }
                self.workspace_list.draw(
                    frame,
                    main_chunks[0],
                    self.focus == Focus::Workspaces,
                    selected_stale,
                    has_status,
                    desc_override,
                    diff_summary,
                    self.show_files,
                    self.action_history.can_undo(),
                );
            }
            LeftPanel::OpLog => {
                if let Some(pane) = &mut self.op_log_pane {
                    pane.draw(
                        frame,
                        main_chunks[0],
                        self.focus == Focus::Workspaces,
                        self.action_history.can_undo(),
                        self.action_history.can_redo(),
                    );
                }
            }
        }

        // Right panel: graph or help
        if self.show_help {
            self.draw_help_pane(frame, main_chunks[1]);
        } else {
            self.graph_pane
                .draw(frame, main_chunks[1], self.focus == Focus::Graph);
        }

        // Cache areas for scroll calculations and mouse hit-testing
        self.workspace_area = main_chunks[0];
        self.graph_area = main_chunks[1];
        self.graph_visible_height = main_chunks[1].height.saturating_sub(2); // minus borders

        // Dialogs overlay the full area
        match self.mode {
            Mode::Copy => {
                if let Some(dialog) = &self.copy_dialog {
                    dialog.draw(frame, self.workspace_area);
                }
            }
            Mode::Bookmarks => {
                if let Some(dialog) = &self.bookmarks_dialog {
                    dialog.draw(frame, self.workspace_area);
                }
            }
            Mode::Sync => {
                if let Some(dialog) = &self.sync_dialog {
                    dialog.draw(frame, self.workspace_area);
                }
            }
            Mode::Create => self.create_dialog.draw(frame, frame.area()),
            Mode::Close => {
                if let Some(dialog) = &self.close_dialog {
                    dialog.draw(frame, self.workspace_area);
                }
            }
            Mode::ConfirmRemoveFiles => {
                self.draw_remove_files_dialog(frame, frame.area());
            }
            Mode::Split => {
                if let Some(picker) = &mut self.revision_picker {
                    picker.draw(frame, frame.area());
                }
            }
            Mode::StaleAlert => {
                self.draw_stale_alert(frame, frame.area());
            }
            Mode::UpdateStale => {
                if let Some(ref dlg) = self.update_stale_diff {
                    dlg.draw(frame, frame.area());
                } else {
                    self.draw_update_stale_dialog(frame, frame.area());
                }
            }
            Mode::ConfigWarning => {
                self.draw_config_warning(frame, frame.area());
            }
            Mode::OpRestore => {
                self.draw_op_restore_dialog(frame, frame.area());
            }
            Mode::UndoRedoConfirm { is_redo } => {
                self.draw_undo_redo_confirm(frame, frame.area(), is_redo);
            }
            Mode::List => {}
        }
    }

    fn draw_stale_alert(&self, frame: &mut Frame, area: Rect) {
        let has_error = self.stale_error.is_some();
        let height = if has_error { 9 } else { 6 };
        let width = 56.min(area.width.saturating_sub(4));
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);
        let block = Block::bordered()
            .title(" Working Copy Stale ")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let mut y_off = 0u16;
        let text_width = inner.width.saturating_sub(2);

        let msg = "Another tool modified the repo. Mutating";
        frame.render_widget(
            Paragraph::new(Span::raw(msg)),
            Rect::new(inner.x + 1, inner.y + y_off, text_width, 1),
        );
        y_off += 1;
        frame.render_widget(
            Paragraph::new(Span::raw("commands will fail until resolved.")),
            Rect::new(inner.x + 1, inner.y + y_off, text_width, 1),
        );
        y_off += 2;

        if let Some(err) = &self.stale_error {
            let err_line = err.lines().next().unwrap_or("");
            frame.render_widget(
                Paragraph::new(Span::styled(err_line, Style::default().fg(Color::Red))),
                Rect::new(inner.x + 1, inner.y + y_off, text_width, 1),
            );
            y_off += 2;
        }

        let help = Line::from(vec![
            Span::styled("  r", Style::default().fg(Color::Green).bold()),
            Span::styled("  resolve  ", Style::default().dim()),
            Span::styled("q", Style::default().bold()),
            Span::styled("  exit", Style::default().dim()),
        ]);
        frame.render_widget(
            Paragraph::new(help),
            Rect::new(inner.x, inner.y + y_off, inner.width, 1),
        );
    }

    fn draw_update_stale_dialog(&self, frame: &mut Frame, area: Rect) {
        let name = self
            .update_stale_target
            .as_ref()
            .map(|(n, _)| n.as_str())
            .unwrap_or("?");
        let computing = self.stale_diff_progress.is_some();
        let title = format!(" Update stale: {name} ");
        let height = 6u16;
        let width = 50u16.min(area.width.saturating_sub(4));
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);
        let block = Block::bordered()
            .title(title)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let msg = if let Some((checked, total)) = self.stale_diff_progress {
            if total > 0 {
                format!("Computing diff... {checked}/{total} files")
            } else {
                "Computing diff...".to_string()
            }
        } else {
            "The working copy is out of date.".to_string()
        };
        frame.render_widget(
            Paragraph::new(Span::raw(&msg)),
            Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), 1),
        );

        let help = if computing {
            Line::from(vec![
                Span::styled("Esc", Style::default().bold()),
                Span::styled(" cancel", Style::default().dim()),
            ])
        } else {
            Line::from(vec![
                Span::styled("y", Style::default().fg(Color::Green).bold()),
                Span::styled(" update  ", Style::default().dim()),
                Span::styled("d", Style::default().fg(Color::Cyan).bold()),
                Span::styled(" diff  ", Style::default().dim()),
                Span::styled("Esc", Style::default().bold()),
                Span::styled(" cancel", Style::default().dim()),
            ])
        };
        frame.render_widget(
            Paragraph::new(help),
            Rect::new(inner.x, inner.y + 2, inner.width, 1),
        );
    }

    fn draw_help_pane(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .title(" Help ")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let key_style = Style::default().fg(Color::Cyan).bold();
        let desc_style = Style::default().dim();
        let header_style = Style::default().fg(Color::Green).bold();

        let lines = vec![
            Line::from(Span::styled(" Navigation", header_style)),
            Line::from(vec![
                Span::styled("  ↑/k", key_style),
                Span::styled("  up            ", desc_style),
                Span::styled("↓/j", key_style),
                Span::styled("  down", desc_style),
            ]),
            Line::from(vec![
                Span::styled("  →", key_style),
                Span::styled("  next rev      ", desc_style),
                Span::styled("←", key_style),
                Span::styled("  prev rev", desc_style),
            ]),
            Line::from(vec![
                Span::styled("  Enter", key_style),
                Span::styled("  switch        ", desc_style),
                Span::styled("Tab", key_style),
                Span::styled("  focus graph", desc_style),
            ]),
            Line::from(vec![
                Span::styled("  Esc", key_style),
                Span::styled("  focus list", desc_style),
            ]),
            Line::from(""),
            Line::from(Span::styled(" Workspaces", header_style)),
            Line::from(vec![
                Span::styled("  n", key_style),
                Span::styled("  new             ", desc_style),
                Span::styled("x", key_style),
                Span::styled("  close workspace", desc_style),
            ]),
            Line::from(vec![
                Span::styled("  s", key_style),
                Span::styled("  sync            ", desc_style),
                Span::styled("t", key_style),
                Span::styled("  transfer", desc_style),
            ]),
            Line::from(vec![
                Span::styled("  v", key_style),
                Span::styled("  split           ", desc_style),
                Span::styled("b", key_style),
                Span::styled("  bookmarks", desc_style),
            ]),
            Line::from(vec![
                Span::styled("  c", key_style),
                Span::styled("  copy info     ", desc_style),
                Span::styled("i", key_style),
                Span::styled("  show files", desc_style),
            ]),
            Line::from(vec![
                Span::styled("  r", key_style),
                Span::styled("  sort          ", desc_style),
                Span::styled("u", key_style),
                Span::styled("  update stale", desc_style),
            ]),
            Line::from(vec![
                Span::styled("  o", key_style),
                Span::styled("  op log        ", desc_style),
                Span::styled("Z", key_style),
                Span::styled("  undo", desc_style),
            ]),
            Line::from(vec![
                Span::styled("  Y", key_style),
                Span::styled("  redo", desc_style),
            ]),
            Line::from(""),
            Line::from(Span::styled(" Status", header_style)),
            Line::from(vec![
                Span::styled("  p", key_style),
                Span::styled("  copy status   ", desc_style),
                Span::styled("P", key_style),
                Span::styled("  clear status", desc_style),
            ]),
            Line::from(""),
            Line::from(Span::styled(" Graph", header_style)),
            Line::from(vec![
                Span::styled("  k/↑", key_style),
                Span::styled("  scroll up     ", desc_style),
                Span::styled("j/↓", key_style),
                Span::styled("  scroll down", desc_style),
            ]),
            Line::from(vec![
                Span::styled("  u", key_style),
                Span::styled("  half page up  ", desc_style),
                Span::styled("d", key_style),
                Span::styled("  half page down", desc_style),
            ]),
            Line::from(""),
            Line::from(Span::styled(" General", header_style)),
            Line::from(vec![
                Span::styled("  ?", key_style),
                Span::styled("  toggle help   ", desc_style),
                Span::styled("q", key_style),
                Span::styled("  quit", desc_style),
            ]),
        ];

        let text = Text::from(lines);
        let paragraph = Paragraph::new(text);
        frame.render_widget(paragraph, inner);
    }

    // ----- Op log helpers -----

    fn enter_op_log(&mut self) {
        match jujutsu::op_log(&self.repo_root, 200) {
            Ok(ops) => {
                self.op_log_pane = Some(op_log_pane::OpLogPane::new(
                    ops,
                    self.action_history.applied_records(),
                ));
                self.left_panel = LeftPanel::OpLog;
                self.focus = Focus::Workspaces;
                self.op_graph_current_id = None;
                self.op_graph_debounce = std::time::Instant::now();
            }
            Err(e) => {
                self.set_status(format!("op log failed: {e}"));
            }
        }
    }

    fn leave_op_log(&mut self) {
        self.left_panel = LeftPanel::Workspaces;
        self.op_log_pane = None;
        self.op_graph_current_id = None;
        self.mode = Mode::List;
        self.refresh();
    }

    /// Fetch and display the graph at the selected operation, with debounce.
    fn maybe_fetch_op_graph(&mut self) {
        if self.left_panel != LeftPanel::OpLog {
            return;
        }
        if self.op_graph_debounce.elapsed() < std::time::Duration::from_millis(300) {
            return;
        }
        let Some(pane) = &self.op_log_pane else {
            return;
        };
        let Some(op) = pane.selected_operation() else {
            return;
        };
        let op_id = op.id.clone();
        if self.op_graph_current_id.as_deref() == Some(&op_id) {
            // Already showing this op's graph — just fetch detail if needed
            self.maybe_fetch_op_detail();
            return;
        }
        if let Ok((graph, heads)) = jujutsu::log_graph_at_operation(
            &self.repo_root,
            &op_id,
            self.config.log_template.as_deref(),
        ) {
            self.graph_pane = GraphPane::new(&graph, heads);
            self.graph_pane.set_title(Some(format!(" Log @ {op_id} ")));
            self.op_graph_current_id = Some(op_id);
        }
        self.maybe_fetch_op_detail();
    }

    fn maybe_fetch_op_detail(&mut self) {
        let Some(pane) = &mut self.op_log_pane else {
            return;
        };
        if let Some(op_id) = pane.needs_detail_fetch() {
            let op_id = op_id.to_string();
            if let Ok(text) = jujutsu::op_show(&self.repo_root, &op_id) {
                pane.set_detail(&op_id, text);
            }
        }
    }

    fn draw_op_restore_dialog(&self, frame: &mut Frame, area: Rect) {
        let op = self
            .op_log_pane
            .as_ref()
            .and_then(|p| p.selected_operation());
        let (id, desc) = match op {
            Some(op) => (op.id.as_str(), op.description.as_str()),
            None => return,
        };

        let height = 7u16;
        let width = 52.min(area.width.saturating_sub(4));
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);
        let block = Block::bordered()
            .title(" Restore Operation? ")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let desc_trunc =
            crate::text_utils::truncate_end(desc, inner.width.saturating_sub(2) as usize);

        let lines = vec![
            Line::from(vec![
                Span::styled("Restore to: ", Style::default()),
                Span::styled(id, Style::default().fg(Color::Yellow)),
            ]),
            Line::from(Span::styled(desc_trunc, Style::default().dim())),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Enter", Style::default().fg(Color::Green).bold()),
                Span::styled("  confirm   ", Style::default().dim()),
                Span::styled("Esc", Style::default().bold()),
                Span::styled("  cancel", Style::default().dim()),
            ]),
        ];
        let text = Paragraph::new(lines);
        frame.render_widget(
            text,
            Rect::new(
                inner.x + 1,
                inner.y,
                inner.width.saturating_sub(2),
                inner.height,
            ),
        );
    }

    fn draw_undo_redo_confirm(&self, frame: &mut Frame, area: Rect, is_redo: bool) {
        let verb = if is_redo { "Redo" } else { "Undo" };
        let title = format!(" {verb}? ");

        let height = 7u16;
        let width = 56.min(area.width.saturating_sub(4));
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);
        let block = Block::bordered()
            .title(title)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let lines = vec![
            Line::from(Span::styled(
                "Non-snapshot jj operations occurred since",
                Style::default(),
            )),
            Line::from(Span::styled(
                format!("the last ji action. {verb} will also revert those."),
                Style::default(),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Enter", Style::default().fg(Color::Green).bold()),
                Span::styled("  confirm   ", Style::default().dim()),
                Span::styled("Esc", Style::default().bold()),
                Span::styled("  cancel", Style::default().dim()),
            ]),
        ];
        let text = Paragraph::new(lines);
        frame.render_widget(
            text,
            Rect::new(
                inner.x + 1,
                inner.y,
                inner.width.saturating_sub(2),
                inner.height,
            ),
        );
    }

    fn draw_config_warning(&self, frame: &mut Frame, area: Rect) {
        let warning_count = self.config.warnings.len();
        // warning lines + optional overflow line + 1 blank + 1 help line + 2 border
        let max_shown = 6.min(warning_count);
        let overflow_line = if warning_count > max_shown { 1u16 } else { 0 };
        let height = (4 + max_shown as u16 + overflow_line).min(area.height.saturating_sub(2));
        let width = 60.min(area.width.saturating_sub(4));
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);
        let block = Block::bordered()
            .title(" Config Warnings ")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let text_width = inner.width.saturating_sub(2);
        let mut y_off = 0u16;

        for w in self.config.warnings.iter().take(max_shown) {
            let line = format!("{w}");
            frame.render_widget(
                Paragraph::new(Span::styled(line, Style::default().fg(Color::Yellow))),
                Rect::new(inner.x + 1, inner.y + y_off, text_width, 1),
            );
            y_off += 1;
        }
        if warning_count > max_shown {
            let more = format!("  ...and {} more", warning_count - max_shown);
            frame.render_widget(
                Paragraph::new(Span::styled(more, Style::default().dim())),
                Rect::new(inner.x + 1, inner.y + y_off, text_width, 1),
            );
            y_off += 1;
        }
        y_off += 1;

        let has_blocking = self
            .config
            .warnings
            .iter()
            .any(|w| w.kind == hooks::WarningKind::MissingBookmark);
        let help = if has_blocking {
            Line::from(vec![
                Span::styled("  Esc", Style::default().bold()),
                Span::styled("  back", Style::default().dim()),
            ])
        } else {
            Line::from(vec![
                Span::styled("  Enter", Style::default().fg(Color::Green).bold()),
                Span::styled("  continue  ", Style::default().dim()),
                Span::styled("Esc", Style::default().bold()),
                Span::styled("  cancel", Style::default().dim()),
            ])
        };
        frame.render_widget(
            Paragraph::new(help),
            Rect::new(inner.x, inner.y + y_off, inner.width, 1),
        );
    }

    fn draw_remove_files_dialog(&self, frame: &mut Frame, area: Rect) {
        let Some(path) = &self.pending_remove_path else {
            return;
        };
        let width = 50.min(area.width.saturating_sub(4));
        let height = 5;
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);
        let block = Block::bordered()
            .title(" Remove files? ")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let path_str = path.display().to_string();
        let path_line = Line::from(Span::styled(&path_str, Style::default().dim()));
        frame.render_widget(
            Paragraph::new(path_line),
            Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), 1),
        );

        let help = Line::from(vec![
            Span::styled("  y", Style::default().fg(Color::Green).bold()),
            Span::styled("  remove  ", Style::default().dim()),
            Span::styled("n", Style::default().bold()),
            Span::styled("  keep", Style::default().dim()),
        ]);
        frame.render_widget(
            Paragraph::new(help),
            Rect::new(inner.x, inner.y + 2, inner.width, 1),
        );
    }

    fn should_quit(&self) -> bool {
        self.action.is_some()
    }
}

// ===========================================================================
// Non-interactive CLI commands (unchanged — these still exit after execution)
// ===========================================================================

/// Non-interactive workspace switch (used by `ji ws <target>`).
pub fn switch(target: &str) -> Result<()> {
    let repo_root = jujutsu::workspace_root()?;
    let ws_path = commands::switch::switch(&repo_root, target)?;
    shell::write_directive_cd(&ws_path)
}

/// Non-interactive create-or-switch (used by `ji ws <target> --create-if-necessary`).
pub fn create_or_switch(
    target: &str,
    revision: Option<&str>,
    path: Option<&str>,
    msg: Option<&str>,
) -> Result<()> {
    let repo_root = jujutsu::workspace_root()?;
    let workspaces = jujutsu::list_workspaces(&repo_root)?;

    let existing = workspaces.iter().find(|ws| {
        ws.name == target
            || ws.bookmarks_at_head.iter().any(|(b, _)| b == target)
            || ws.bookmarks_behind.iter().any(|(b, _)| b == target)
    });

    if let Some(ws) = existing {
        shell::write_directive_cd(&ws.path)
    } else {
        create(target, revision, path, msg)
    }
}

/// Non-interactive workspace creation (used by `ji ws --create`).
pub fn create(
    bookmark: &str,
    revision: Option<&str>,
    path: Option<&str>,
    msg: Option<&str>,
) -> Result<()> {
    let repo_root = jujutsu::workspace_root()?;
    let config = config::load_config(&repo_root)?;
    let repo_name = config::resolve_repo_name(&config, &repo_root);

    // Print any template warnings to stderr (CLI mode: warn and proceed).
    for w in &config.warnings {
        eprintln!("(ji)::config {w}");
        if matches!(w.kind, hooks::WarningKind::UnknownVariable) {
            let known = if w.context == "workspace-path" {
                hooks::known_vars_hint(hooks::PATH_VARS)
            } else {
                hooks::known_vars_hint(hooks::HOOK_VARS)
            };
            eprintln!("    known variables: {known}");
        }
    }

    let rev = revision.unwrap_or("@");
    let source_ws_path =
        std::env::current_dir().context("failed to get current directory for source workspace")?;

    let ws_path_abs =
        commands::create::resolve_workspace_path(&repo_root, &config, &repo_name, bookmark, path)?;

    let result = commands::create::create(
        &repo_root,
        &config,
        &repo_name,
        bookmark,
        rev,
        &source_ws_path,
        &ws_path_abs,
        msg.unwrap_or(""),
        false,
    )?;

    shell::write_directive_cd(&result.workspace_path)?;
    Ok(())
}

// ===========================================================================
// TUI entry point
// ===========================================================================

/// Restores terminal state on drop — ensures raw mode, alternate screen,
/// and mouse capture are always cleaned up, even on `?` or panic.
struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn new() -> Self {
        Self { active: true }
    }

    fn restore(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let _ = terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableFocusChange
        );
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Reorder each workspace's `revisions` to match the visual order in the graph.
/// The graph's `line_heads` provides the display order; revisions not found in
/// the graph are appended at the end.
fn sort_revisions_by_graph(line_heads: &[Option<String>], workspaces: &mut [jujutsu::Workspace]) {
    use std::collections::HashMap;

    // Build change_id → first-occurrence position in the graph.
    let mut position: HashMap<&str, usize> = HashMap::new();
    for (i, head) in line_heads.iter().enumerate() {
        if let Some(id) = head {
            position.entry(id.as_str()).or_insert(i);
        }
    }

    for ws in workspaces.iter_mut() {
        let fallback = line_heads.len();
        ws.revisions
            .sort_by_key(|r| *position.get(r.change_id.as_str()).unwrap_or(&fallback));
    }
}

/// Exit raw mode + alternate screen + mouse + focus capture. Mirror of the
/// crossterm setup sequence at the top of `run()`. Used by
/// `drain_pending_handoff` to surrender the terminal to a subprocess.
fn exit_raw_mode(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableFocusChange
    )?;
    Ok(())
}

/// Re-enter raw mode + alternate screen + mouse + focus capture, then clear.
/// Counterpart to `exit_raw_mode`.
fn re_enter_raw_mode(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    crossterm::execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableFocusChange
    )?;
    terminal::enable_raw_mode()?;
    terminal.clear()?;
    Ok(())
}

/// Drain one pending side effect from `app.pending_handoff`, with terminal
/// exit + dispatch + re-enter handoff. No-op if no handoff is pending.
///
/// Pattern: consume-by-value. Each match arm dispatches its variant and
/// performs any per-variant App-state mutations inline. The arm sets two
/// local flags — `history_label: Option<String>` and `enter_op_log_after:
/// bool` — that the post-match block consumes.
///
/// Pre/post jj op-ids surround the match so action-history records cover the
/// full deferred operation.
fn drain_pending_handoff(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    repo_root: &Path,
) -> io::Result<()> {
    let Some(handoff) = app.pending_handoff.take() else {
        return Ok(());
    };

    let pre = jujutsu::current_op_id(repo_root);
    exit_raw_mode(terminal)?;

    let mut history_label: Option<String> = None;
    let mut enter_op_log_after = false;
    let mut rebuild_bookmarks_after = false;
    let mut select_workspace_after: Option<String> = None;

    match handoff {
        PendingHandoff::Split { change_id } => {
            let r = jujutsu::split_revision(repo_root, &change_id);
            app.log_err(r, "split");
            history_label = Some(format!("split {change_id}"));
        }
        PendingHandoff::OpRestore {
            op_id,
            label,
            is_redo,
        } => {
            match jujutsu::op_restore(repo_root, &op_id) {
                Ok(_) => {
                    let verb = if is_redo { "redid" } else { "undid" };
                    app.set_status(format!("{verb}: {label}"));
                    app.action_history.last_op_head = jujutsu::current_op_id(repo_root);
                    app.save_history();
                }
                Err(e) => {
                    let verb = if is_redo { "redo" } else { "undo" };
                    app.set_status(format!("{verb} failed: {e}"));
                }
            }
            // op_restore records its own history above (last_op_head update);
            // returning None here so the post-match finalize does not double-record.
            enter_op_log_after = true;
        }
        PendingHandoff::RemoveDirAll { path } => {
            let r = std::fs::remove_dir_all(&path);
            app.log_err(r, "remove files");
            // Records nothing in action history.
        }
        PendingHandoff::SaveDiff {
            ws_path,
            kind,
            rel_path,
        } => match update_stale_dialog::save_diff_inline(&ws_path, kind, &rel_path) {
            Ok(Some(path)) => app.set_status(format!("Saved: {path}")),
            Ok(None) => {}
            Err(e) => app.set_status(format!("Diff failed: {e:#}")),
        },
        PendingHandoff::SaveAllDiffs { ws_path, items } => {
            match update_stale_dialog::save_all_diffs_inline(&ws_path, &items) {
                Ok(0) => app.set_status("No diffs to save".into()),
                Ok(n) => app.set_status(format!("Saved {n} diff(s) to .ji/diffs/")),
                Err(e) => app.set_status(format!("Diff failed: {e:#}")),
            }
        }
        PendingHandoff::WorkspaceForget { name } => {
            let r = jujutsu::workspace_forget_orphaned(repo_root, &name);
            app.log_err(r, "forget workspace");
            // Intentionally no history record.
        }
        PendingHandoff::BookmarkCreate { name, change_id } => {
            let r = jujutsu::create_bookmark_at(repo_root, &name, &change_id);
            app.log_err(r, "create bookmark");
            history_label = Some(format!("bookmark create {name}"));
        }
        PendingHandoff::UpdateStaleSequence => {
            // Save diffs first as a safety net, then resolve staleness. Both
            // steps run in one terminal handoff to avoid double-flicker.
            let saved = update_stale_dialog::save_all_stale_diffs(repo_root).unwrap_or(0);
            match jujutsu::update_stale(repo_root) {
                Ok(_) => {
                    app.stale = false;
                    app.stale_error = None;
                    app.mode = Mode::List;
                    if saved > 0 {
                        app.set_status(format!("Resolved. {saved} diff(s) saved to .ji/diffs/"));
                    }
                }
                Err(e) => {
                    app.stale_error = Some(format!("{e:#}"));
                }
            }
        }
        PendingHandoff::UpdateWorkspaceStale { ws_path, name } => {
            let saved = update_stale_dialog::save_all_stale_diffs(&ws_path).unwrap_or(0);
            match jujutsu::update_workspace_stale(&ws_path) {
                Ok(_) => {
                    let msg = if saved > 0 {
                        format!("{name}: resolved. {saved} diff(s) saved to .ji/diffs/")
                    } else {
                        format!("{name}: staleness resolved")
                    };
                    app.set_status(msg);
                    app.refresh_staleness();
                }
                Err(e) => {
                    app.set_status(format!("{name}: update failed: {e:#}"));
                }
            }
        }
        PendingHandoff::OpRestoreFromLog { op_id } => {
            let r = jujutsu::op_restore(repo_root, &op_id);
            app.log_err(r, "op restore");
            // Records nothing in action history. Re-enter the op-log pane
            // regardless of result.
            enter_op_log_after = true;
        }
        PendingHandoff::BookmarkTug { names, head_id } => {
            for bm in &names {
                let r = jujutsu::bookmark_set(repo_root, bm, &head_id);
                app.log_err(r, "tug bookmark");
            }
            history_label = Some(format!("bookmark tug {}", names.join(", ")));
            rebuild_bookmarks_after = true;
        }
        PendingHandoff::BookmarkDelete { names } => {
            for bm in &names {
                let r = jujutsu::bookmark_delete(repo_root, bm);
                app.log_err(r, "delete bookmark");
            }
            history_label = Some(format!("bookmark delete {}", names.join(", ")));
            rebuild_bookmarks_after = true;
        }
        PendingHandoff::Sync { params, label } => {
            let result = commands::sync::sync_with_info(
                &params.repo_root,
                &params.info,
                &params.source_name,
                &params.source_path,
                &params.target_name,
                &params.target_path,
                &params.workspace_path_template,
                &params.repo_name,
                params.author.as_deref(),
            );
            match result {
                Ok(operations::SyncOutcome::AlreadyInSync) => {
                    app.set_status("already in sync".to_string());
                }
                Ok(operations::SyncOutcome::Done { warnings }) => {
                    if !warnings.is_empty() {
                        app.set_status(warnings.join("; "));
                    }
                }
                Err(e) => {
                    app.set_status(format!("{e:#}"));
                }
            }
            history_label = Some(label);
        }
        PendingHandoff::Create { params } => {
            let result = commands::create::create(
                &params.repo_root,
                &params.config,
                &params.repo_name,
                &params.bookmark,
                &params.revision,
                &params.source_ws_path,
                &params.ws_path,
                &params.msg,
                true,
            );
            match result {
                Ok(create_result) => {
                    // Defer the select_by_name until AFTER the post-match
                    // `app.refresh()` — at this point `workspace_list` is
                    // still the pre-create snapshot and does not contain
                    // the new workspace, so calling select_by_name now would
                    // be a NOP. The new workspace appears only after refresh
                    // re-fetches the workspace list from jj.
                    select_workspace_after = Some(create_result.workspace_name.clone());
                    history_label = Some(format!("create {}", create_result.workspace_name));
                }
                Err(e) => {
                    app.set_status(format!("create workspace: {e}"));
                }
            }
        }
        PendingHandoff::Close {
            params,
            info,
            label,
        } => {
            // Build the borrow-typed CloseParams inline; `bookmarks`,
            // `revisions`, and `all_ws_paths` move out of `params` into
            // the borrow-typed struct without cloning.
            let source_path_for_check = params.source_path.clone();
            let close_params = commands::close::CloseParams {
                repo_root: &params.repo_root,
                source_name: &params.source_name,
                source_path: &params.source_path,
                target_name: &params.target_name,
                target_path: &params.target_path,
                target_change_id: &params.target_change_id,
                method: params.method,
                delete_files: params.delete_files,
                bookmark_action: params.bookmark_action,
                bookmarks: params.bookmarks,
                revisions: &params.revisions,
                workspace_path_template: &params.workspace_path_template,
                repo_name: &params.repo_name,
                author: params.author.as_deref(),
                all_ws_paths: &params.all_ws_paths,
            };
            match commands::close::close_with_info(&close_params, &info) {
                Ok(outcome) => {
                    if !outcome.stale_warnings.is_empty() {
                        app.set_status(format!("stale: {}", outcome.stale_warnings.join(", ")));
                    }
                    if let Some(remove_path) = outcome.pending_remove_path {
                        app.pending_remove_path = Some(remove_path);
                        app.mode = Mode::ConfirmRemoveFiles;
                    } else {
                        if source_path_for_check == app.current_root {
                            app.action = Some(Action::SwitchTo(app.repo_root.clone()));
                        }
                        app.mode = Mode::List;
                    }
                }
                Err(e) => {
                    app.set_status(format!("{e:#}"));
                    app.mode = Mode::List;
                }
            }
            history_label = Some(label);
        }
        PendingHandoff::Transfer {
            params,
            info,
            label,
        } => {
            let transfer_params = commands::transfer::TransferParams {
                repo_root: &params.repo_root,
                source_name: &params.source_name,
                source_path: &params.source_path,
                target_name: &params.target_name,
                target_path: &params.target_path,
                method: params.method,
                workspace_path_template: &params.workspace_path_template,
                repo_name: &params.repo_name,
                author: params.author.as_deref(),
                all_ws_paths: &params.all_ws_paths,
            };
            match commands::transfer::transfer_with_info(&transfer_params, &info) {
                Ok(outcome) => {
                    if !outcome.stale_warnings.is_empty() {
                        app.set_status(format!("stale: {}", outcome.stale_warnings.join(", ")));
                    }
                    app.mode = Mode::List;
                }
                Err(e) => {
                    app.set_status(format!("{e:#}"));
                    app.mode = Mode::List;
                }
            }
            history_label = Some(label);
        }
    }

    re_enter_raw_mode(terminal)?;
    let post = jujutsu::current_op_id(repo_root);

    if let Some(label) = history_label {
        app.action_history.maybe_record(label, pre, post);
        app.save_history();
    }
    app.refresh();
    // Create's `workspace_list.select_by_name` is deferred to here because
    // refresh re-fetches the workspace list from jj; only after that does
    // the newly-created workspace exist in the list. Calling
    // `select_by_name` inside the Create arm would NOP against the stale
    // pre-create list.
    if let Some(name) = select_workspace_after {
        app.workspace_list.select_by_name(&name);
    }
    // OpRestore may have changed the op-log; re-enter the pane if visible.
    // Runs after refresh so the re-entered pane reflects the restored op-log.
    if enter_op_log_after && app.left_panel == LeftPanel::OpLog {
        app.enter_op_log();
    }
    // Bookmark batch ops keep the dialog open against the freshly-refreshed
    // workspace state. Must run AFTER `app.refresh()` so workspace_list
    // reflects the new state.
    if rebuild_bookmarks_after {
        if let Some(ws) = app.workspace_list.selected_workspace() {
            let eff = jj_utils::find_effective_head(repo_root, &ws.name).ok();
            app.bookmarks_dialog = Some(BookmarksDialog::new(ws, eff));
        } else {
            app.bookmarks_dialog = None;
            app.mode = Mode::List;
        }
    }
    Ok(())
}

pub fn run() -> Result<()> {
    let repo_root = jujutsu::workspace_root()?;
    let current_root = jujutsu::current_workspace_root()?;
    let mut workspaces = jujutsu::list_workspaces(&repo_root)?;
    for ws in &mut workspaces {
        ws.is_current = ws.path == current_root;
    }
    let config = config::load_config(&repo_root)?;

    if workspaces.is_empty() {
        anyhow::bail!("no workspaces found");
    }

    // Enter TUI
    terminal::enable_raw_mode().context("failed to enable raw mode")?;
    let mut guard = TerminalGuard::new();
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableFocusChange
    )
    .context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let repo_name = config::resolve_repo_name(&config, &repo_root);
    let (graph_output, line_heads) =
        jujutsu::log_graph_with_heads(&repo_root, config.log_template.as_deref())
            .unwrap_or_else(|_| (String::new(), Vec::new()));
    // Reorder workspace revisions to match the graph's visual order.
    sort_revisions_by_graph(&line_heads, &mut workspaces);

    // Load persisted action history.
    jj_utils::ensure_ji_dir(&repo_root)?;
    let history_path = repo_root.join(".ji/action-history.json");
    let action_history = action_history::ActionHistory::load(&history_path);

    let mut app = App::new(
        workspaces,
        config,
        repo_root.clone(),
        current_root,
        repo_name,
        &graph_output,
        line_heads,
        action_history,
        history_path,
    );

    // Sync graph to initially selected workspace
    app.sync_graph_to_selection();

    // Op head tracking for live refresh
    let mut last_op_head = jujutsu::current_op_head(&repo_root);
    let mut poll_count: u32 = 0;
    let mut stale_poll = std::time::Instant::now();

    // Initial staleness check
    app.refresh_staleness();

    // Event loop
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        // Only exit if there's no pending deferred work. An event handler
        // can set BOTH `app.action = Some(Action::SwitchTo(...))` AND
        // `app.pending_handoff = Some(...)` in the same step (e.g. the
        // `ConfirmRemoveFiles` 'y' handler, which switches workspaces AND
        // queues a directory deletion); if we honoured the
        // quit signal here we'd skip the drain entirely and the deferred
        // action would be lost.
        if app.should_quit() && app.pending_handoff.is_none() {
            break;
        }

        drain_pending_handoff(&mut app, &mut terminal, &repo_root)?;

        // Re-check quit immediately after the drain. A drain arm may set
        // `app.action = Some(Action::SwitchTo(...))` (e.g. the Close arm,
        // when the source workspace was the current one);
        // honour it here before polling any further input that could
        // overwrite it with Action::Quit (e.g. a 'q' typed during the
        // raw-mode-off window of the drain, queued in stdin, then read in
        // the next event::poll).
        if app.should_quit() {
            break;
        }

        if event::poll(std::time::Duration::from_millis(100))? {
            let ev = event::read()?;
            app.handle_event(ev);
        }

        app.poll_stale_diff();
        app.maybe_fetch_op_graph();

        // Skip background polling when the terminal is not focused.
        // A full refresh fires on FocusGained, so nothing is missed.
        if app.terminal_focused {
            // Check for repo changes every ~1s
            poll_count += 1;
            if poll_count.is_multiple_of(10) {
                let new_op_head = jujutsu::current_op_head(&repo_root);
                if new_op_head != last_op_head {
                    last_op_head = new_op_head;
                    app.refresh();
                    // Check if the working copy became stale
                    if jujutsu::is_working_copy_stale(&repo_root) {
                        app.stale = true;
                        app.stale_error = None;
                        app.mode = Mode::StaleAlert;
                    } else {
                        app.stale = false;
                    }
                }
            }

            // Periodic staleness check (every 60s)
            if stale_poll.elapsed() >= std::time::Duration::from_mins(1) {
                stale_poll = std::time::Instant::now();
                app.refresh_staleness();
            }
        }
    }

    // Restore terminal (guard also handles this on error/panic, but
    // explicit restore avoids relying solely on Drop ordering).
    guard.restore();

    // Only SwitchTo needs post-TUI execution (for the shell cd directive)
    if let Some(Action::SwitchTo(path)) = app.action {
        shell::write_directive_cd(&path)?;
    }

    Ok(())
}
