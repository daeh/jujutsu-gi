use super::dialog_common::{
    self, DialogLayout, HelpBinding, SyncMode, SyncModeInfo, TargetWorkspace,
};
use super::dialog_info::Diagram;
pub(crate) use crate::commands::types::{BookmarkAction, DialogIntent, Operation};
use crate::{commands, graph, jj_utils, jujutsu, jujutsu::RevisionInfo};
use crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) struct CloseDialog {
    pub workspace_name: String,
    pub workspace_path: PathBuf,
    pub revisions: Vec<RevisionInfo>,
    pub delete_files: bool,
    pub intent: DialogIntent,
    pub targets: Vec<TargetWorkspace>,
    pub target_index: usize,
    /// Index into all_operations() for the currently highlighted operation.
    pub selected_index: usize,
    /// Repository root (needed for close-mode recomputation on target cycle).
    pub repo_root: PathBuf,
    /// Mode info for transfer/close state detection.
    pub close_info: Option<SyncModeInfo>,
    /// Bookmark action to perform on close.
    pub bookmark_action: BookmarkAction,
    /// Non-singular bookmark names in the workspace's revision chain.
    pub bookmarks: Vec<String>,
    /// The singular bookmark derived from the source workspace name (auto-advanced).
    pub singular_bookmark: Option<String>,
    /// The singular bookmark derived from the current target workspace name.
    /// Recomputed when the target cycles.
    pub target_singular_bookmark: Option<String>,
    /// Config workspace-path template (for target singular-bookmark identification).
    workspace_path_template: String,
    /// Repository display name (for target singular-bookmark identification).
    repo_name: String,
    /// Cached shortest-prefix forms of change IDs (full → shortest).
    shortest_ids: HashMap<String, String>,
    /// Cached result of `is_effectively_linear` — recomputed when target cycles.
    squashable: bool,
    /// Resolved change ID of the squash target (set when squashable is true).
    squash_target: Option<String>,
    /// Transient notices rendered at the top of the dialog (stale-refresh
    /// banner, restored-selection notes). Cleared on target cycle.
    pub notice: Vec<String>,
    /// Set when the selected target cannot back a target-consuming operation
    /// (e.g. it vanished during a stale refresh). While set, only disposal
    /// operations (Detach/Abandon) are offered. Cleared on target cycle.
    pub target_unavailable: Option<String>,
    /// Set when the source workspace vanished during a stale refresh: every
    /// close/transfer operation consumes the source, so no operations are
    /// offered while this is set (Enter inert).
    pub source_missing: bool,
    /// Op head at the last point the dialog's data was made fresh (open or an
    /// in-place stale refresh). The execute gate compares the current op head
    /// against this, not `close_info.op_head` — `recompute_close_mode()`
    /// re-stamps the latter to "now" on every target cycle, which would
    /// otherwise mask external movement (stale revisions/bookmarks/target ids)
    /// from the gate.
    pub freshness_baseline: String,
}

impl CloseDialog {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        workspace_name: String,
        workspace_path: PathBuf,
        revisions: Vec<RevisionInfo>,
        intent: DialogIntent,
        targets: Vec<TargetWorkspace>,
        default_target_idx: usize,
        repo_root: PathBuf,
        bookmarks: Vec<String>,
        singular_bookmark: Option<String>,
        workspace_path_template: String,
        repo_name: String,
    ) -> Self {
        let mut dialog = Self {
            workspace_name,
            workspace_path,
            revisions,
            delete_files: matches!(intent, DialogIntent::Close),
            intent,
            targets,
            target_index: default_target_idx,
            selected_index: 0,
            repo_root,
            close_info: None,
            bookmark_action: BookmarkAction::NoAction,
            bookmarks,
            singular_bookmark,
            target_singular_bookmark: None,
            workspace_path_template,
            repo_name,
            shortest_ids: HashMap::new(),
            squashable: false,
            squash_target: None,
            notice: Vec::new(),
            target_unavailable: None,
            source_missing: false,
            freshness_baseline: String::new(),
        };
        dialog.recompute_close_mode();
        // Baseline = op head when this dialog's data was first made fresh
        // (the open handler snapshotted via refresh() immediately before).
        dialog.freshness_baseline = dialog
            .close_info
            .as_ref()
            .map(|i| i.op_head.clone())
            .unwrap_or_default();
        dialog
    }

    /// Re-anchor the freshness baseline (call after an in-place refresh).
    pub(crate) fn set_freshness_baseline(&mut self, op_head: String) {
        self.freshness_baseline = op_head;
    }

    pub(crate) fn toggle_delete_files(&mut self) {
        self.delete_files = !self.delete_files;
    }

    pub(crate) fn cycle_bookmark_action(&mut self) {
        self.bookmark_action = match self.bookmark_action {
            BookmarkAction::NoAction => BookmarkAction::Advance,
            BookmarkAction::Advance => BookmarkAction::Delete,
            BookmarkAction::Delete => BookmarkAction::NoAction,
        };
    }

    pub(crate) fn cycle_target(&mut self) {
        if !self.targets.is_empty() {
            self.target_index = (self.target_index + 1) % self.targets.len();
        }
        self.selected_index = 0;
        self.notice.clear();
        self.target_unavailable = None;
    }

    pub(crate) fn cycle_target_back(&mut self) {
        if !self.targets.is_empty() {
            self.target_index = (self.target_index + self.targets.len() - 1) % self.targets.len();
        }
        self.selected_index = 0;
        self.notice.clear();
        self.target_unavailable = None;
    }

    /// Recompute close-mode info for the current target.
    /// Always refreshes the shortest-ID cache (needed for both Transfer and Close).
    pub(crate) fn recompute_close_mode(&mut self) {
        self.close_info = self.selected_target().map(|target| {
            Self::detect_close_mode(&self.repo_root, &self.workspace_name, &target.name)
        });
        self.target_singular_bookmark = self
            .selected_target()
            .and_then(|t| {
                jj_utils::identify_singular_bookmark(
                    &self.repo_root,
                    &self.workspace_path_template,
                    &self.repo_name,
                    &t.name,
                )
            })
            .map(|(name, _)| name);
        self.squashable = self
            .close_info
            .as_ref()
            .map(|info| {
                graph::is_effectively_linear(&self.repo_root, &info.lca, &info.src_effective_head)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        self.squash_target = if self.squashable {
            self.close_info.as_ref().and_then(|info| {
                jj_utils::resolve_change_id(
                    &self.repo_root,
                    &format!("latest(roots({}..{}))", info.lca, info.src_effective_head),
                )
                .ok()
            })
        } else {
            None
        };
        self.recompute_shortest_ids();
    }

    /// Recompute shortest-prefix cache for all change IDs used in planned_commands.
    pub(crate) fn recompute_shortest_ids(&mut self) {
        let mut ids: Vec<&str> = Vec::new();
        // Target
        if let Some(t) = self.selected_target() {
            ids.push(&t.change_id);
        }
        // Revisions (for abandon display)
        for r in &self.revisions {
            ids.push(&r.change_id);
        }
        // Mode heads
        if let Some(info) = &self.close_info {
            ids.push(&info.src_effective_head);
            ids.push(&info.tgt_effective_head);
            ids.push(&info.src_actual_head);
            ids.push(&info.tgt_actual_head);
            if let Some(id) = &info.src_trivial_id {
                ids.push(id);
            }
            if let Some(id) = &info.tgt_trivial_id {
                ids.push(id);
            }
            if !info.lca.is_empty() {
                ids.push(&info.lca);
            }
        }
        if let Some(id) = &self.squash_target {
            ids.push(id);
        }
        self.shortest_ids = jujutsu::shortest_change_ids(&self.repo_root, &ids);
    }

    /// Look up the shortest prefix for a change ID, falling back to 4-char truncation.
    fn shortest<'a>(&'a self, id: &'a str) -> &'a str {
        self.shortest_ids
            .get(id)
            .map(|s| s.as_str())
            .unwrap_or(&id[..id.len().min(4)])
    }

    /// Detect the sync state between source and target workspaces.
    fn detect_close_mode(repo: &Path, src_name: &str, tgt_name: &str) -> SyncModeInfo {
        commands::detect_sync_mode(repo, src_name, tgt_name)
    }

    /// Dialog-internal key handling: operation navigation, target cycling,
    /// and toggles. Returns true if the target selection changed (the caller
    /// resyncs the graph). Confirm/copy/Esc — and the freshness gate and
    /// deferred execution they trigger — stay with the App.
    pub(crate) fn handle_key(&mut self, key: KeyCode) -> bool {
        match key {
            // Navigate operations
            KeyCode::Up => {
                self.move_up();
            }
            KeyCode::Down => {
                self.move_down();
            }
            // Cycle target workspace
            KeyCode::Left if self.targets.len() > 1 => {
                self.cycle_target_back();
                self.recompute_close_mode();
                return true;
            }
            KeyCode::Right if self.targets.len() > 1 => {
                self.cycle_target();
                self.recompute_close_mode();
                return true;
            }
            // Key shortcuts to jump to operation
            KeyCode::Char('a') => {
                let op = match self.intent {
                    DialogIntent::Transfer => Operation::AdaptiveMerge,
                    DialogIntent::Close => Operation::AdaptiveClose,
                };
                self.jump_to(op);
            }
            KeyCode::Char(c @ '1'..='4') => {
                self.jump_to_key(c);
            }
            KeyCode::Char('d') if self.intent == DialogIntent::Close => {
                self.jump_to(Operation::Detach);
            }
            // Toggles
            KeyCode::Char('b')
                if self.intent == DialogIntent::Close && !self.bookmarks.is_empty() =>
            {
                self.cycle_bookmark_action();
            }
            KeyCode::Char('k') if self.intent == DialogIntent::Close => {
                self.toggle_delete_files();
            }
            _ => {}
        }
        false
    }

    pub(crate) fn move_up(&mut self) {
        self.selected_index = self.selected_index.saturating_sub(1);
    }

    pub(crate) fn move_down(&mut self) {
        let max = self.all_operations().len().saturating_sub(1);
        if self.selected_index < max {
            self.selected_index += 1;
        }
    }

    /// Jump selection to the given operation, if it exists in the list.
    pub(crate) fn jump_to(&mut self, op: Operation) {
        if let Some(idx) = self.all_operations().iter().position(|o| *o == op) {
            self.selected_index = idx;
        }
    }

    /// Jump selection to the operation matching a key character (e.g. '1', '2', 'a', 'd').
    pub(crate) fn jump_to_key(&mut self, key: char) {
        let ops = self.all_operations();
        let mut num_idx = 0usize;
        for (i, op) in ops.iter().enumerate() {
            let op_key = Self::op_key(op, num_idx);
            if !matches!(
                op,
                Operation::AdaptiveMerge
                    | Operation::AdaptiveClose
                    | Operation::Detach
                    | Operation::Abandon
            ) {
                num_idx += 1;
            }
            if op_key == key.to_string() {
                self.selected_index = i;
                return;
            }
        }
    }

    /// The currently highlighted operation.
    pub(crate) fn selected_op(&self) -> Option<Operation> {
        self.all_operations().get(self.selected_index).copied()
    }

    pub(crate) fn selected_target(&self) -> Option<&TargetWorkspace> {
        self.targets.get(self.target_index)
    }

    /// All navigable operations for the current direction and intent.
    pub(crate) fn all_operations(&self) -> Vec<Operation> {
        // Every operation consumes the source workspace.
        if self.source_missing {
            return Vec::new();
        }
        // Target-consuming operations need a usable target; disposal ops
        // (Detach/Abandon) never consume one and stay offered.
        let mut ops = if self.target_unavailable.is_some() {
            Vec::new()
        } else {
            self.available_operations()
        };
        if self.intent == DialogIntent::Close {
            ops.push(Operation::Detach);
            if !self.revisions.is_empty() {
                ops.push(Operation::Abandon);
            }
        }
        ops
    }

    /// Whether the source's unique chain is effectively linear (squashable).
    /// Cached — recomputed in `recompute_close_mode()`.
    fn is_squashable(&self) -> bool {
        self.squashable
    }

    /// Merge/sync operations available for the current state.
    ///
    /// `Operation::MergeAbandonOld` ("merge (old)") is deliberately not offered:
    /// it is disabled at both entry points (here and the CLI `TransferMethod`
    /// value enum), leaving its execution path intact but unreachable.
    fn available_operations(&self) -> Vec<Operation> {
        let mode = self.close_info.as_ref().map(|i| &i.mode);
        let squashable = || self.is_squashable();
        match self.intent {
            DialogIntent::Transfer => match mode {
                Some(SyncMode::SourceOnly) => {
                    let mut ops = vec![
                        Operation::AdaptiveMerge,
                        Operation::FastForwardTarget,
                        Operation::Merge,
                    ];
                    if squashable() {
                        ops.push(Operation::MergeSquash);
                    }
                    ops
                }
                Some(SyncMode::TargetOnly) => vec![
                    Operation::AdaptiveMerge,
                    Operation::FastForwardSource,
                    Operation::Merge,
                ],
                Some(SyncMode::Diverged) => {
                    let mut ops = vec![
                        Operation::AdaptiveMerge,
                        Operation::Merge,
                        Operation::Rebase,
                    ];
                    if squashable() {
                        ops.push(Operation::MergeSquash);
                    }
                    ops
                }
                _ => vec![],
            },
            DialogIntent::Close => match mode {
                Some(SyncMode::SourceOnly) => {
                    let mut ops = vec![
                        Operation::AdaptiveClose,
                        Operation::FastForwardTargetClose,
                        Operation::MergeClose,
                    ];
                    if squashable() {
                        ops.push(Operation::MergeSquashClose);
                    }
                    ops
                }
                Some(SyncMode::Diverged) => {
                    let mut ops = vec![Operation::AdaptiveClose, Operation::MergeClose];
                    if squashable() {
                        ops.push(Operation::MergeSquashClose);
                    }
                    ops
                }
                // TargetOnly / InSync: source has no unique work, just disposal
                _ => vec![],
            },
        }
    }

    fn op_label(op: &Operation) -> &'static str {
        match op {
            Operation::Merge => "merge",
            Operation::AdaptiveMerge => "adaptive",
            Operation::FastForwardTarget | Operation::FastForwardSource => "fast forward",
            Operation::MergeAbandonOld => "merge (old)",
            Operation::Rebase => "rebase",
            Operation::MergeSquash => "squash merge",
            Operation::AdaptiveClose => "adaptive",
            Operation::MergeClose => "merge",
            Operation::MergeSquashClose => "squash merge",
            Operation::FastForwardTargetClose => "fast forward",
            Operation::Detach => "detach",
            Operation::Abandon => "abandon",
        }
    }

    /// Build description spans for an operation. Dynamic names are italic.
    fn op_desc_spans(&self, op: &Operation, base_style: Style) -> Vec<Span<'_>> {
        let source = self.workspace_name.as_str();
        let target = self
            .selected_target()
            .map(|t| t.name.as_str())
            .unwrap_or("default");
        let s = |text: &str| Span::styled(text.to_string(), base_style);
        let name = |n: &str| Span::styled(n.to_string(), base_style.italic());
        match op {
            Operation::Merge => vec![s(" merge "), name(source), s(" and "), name(target)],
            Operation::AdaptiveMerge => {
                let mode = self.close_info.as_ref().map(|i| &i.mode);
                match mode {
                    Some(SyncMode::SourceOnly) => {
                        vec![s(" fast-forward "), name(target), s(" to "), name(source)]
                    }
                    Some(SyncMode::TargetOnly) => {
                        vec![s(" fast-forward "), name(source), s(" to "), name(target)]
                    }
                    Some(SyncMode::Diverged) => {
                        vec![s(" merge "), name(source), s(" and "), name(target)]
                    }
                    _ => vec![s(" (unavailable)")],
                }
            }
            Operation::FastForwardTarget => {
                vec![s(" fast-forward "), name(target), s(" to "), name(source)]
            }
            Operation::FastForwardSource => {
                vec![s(" fast-forward "), name(source), s(" to "), name(target)]
            }
            Operation::MergeAbandonOld => {
                vec![s(" merge "), name(source), s(" and "), name(target)]
            }
            Operation::Rebase => {
                vec![s(" rebase "), name(source), s(" onto "), name(target)]
            }
            Operation::MergeSquash => {
                vec![
                    s(" squash "),
                    name(source),
                    s(", merge with "),
                    name(target),
                ]
            }
            Operation::AdaptiveClose => {
                let mode = self.close_info.as_ref().map(|i| &i.mode);
                match mode {
                    Some(SyncMode::SourceOnly) => {
                        vec![
                            s(" fast-forward "),
                            name(target),
                            s(" to "),
                            name(source),
                            s(", forget "),
                            name(source),
                        ]
                    }
                    Some(SyncMode::Diverged) => {
                        vec![
                            s(" merge into "),
                            name(target),
                            s(", forget "),
                            name(source),
                        ]
                    }
                    _ => vec![s(" (unavailable)")],
                }
            }
            Operation::MergeClose => {
                vec![
                    s(" merge into "),
                    name(target),
                    s(", forget "),
                    name(source),
                ]
            }
            Operation::MergeSquashClose => {
                vec![
                    s(" squash "),
                    name(source),
                    s(", merge into "),
                    name(target),
                    s(", forget "),
                    name(source),
                ]
            }
            Operation::FastForwardTargetClose => {
                vec![
                    s(" fast-forward "),
                    name(target),
                    s(" to "),
                    name(source),
                    s(", forget "),
                    name(source),
                ]
            }
            Operation::Detach => vec![s(" leave revisions in place")],
            Operation::Abandon => vec![s(" delete all revisions")],
        }
    }

    fn op_diagram(op: &Operation, close_mode: Option<&SyncMode>) -> Option<Diagram> {
        match op {
            Operation::Merge | Operation::MergeAbandonOld => Some(Diagram::Merge),
            Operation::Rebase => Some(Diagram::Rebase),
            Operation::MergeSquash => Some(Diagram::MergeSquash),
            Operation::AdaptiveMerge => match close_mode {
                Some(SyncMode::SourceOnly) | Some(SyncMode::TargetOnly) => {
                    Some(Diagram::FastForward)
                }
                Some(SyncMode::Diverged) => Some(Diagram::Merge),
                _ => None,
            },
            Operation::FastForwardTarget | Operation::FastForwardSource => {
                Some(Diagram::FastForward)
            }
            Operation::AdaptiveClose => match close_mode {
                Some(SyncMode::SourceOnly) => Some(Diagram::FastForwardTargetClose),
                Some(SyncMode::Diverged) => Some(Diagram::MergeClose),
                _ => None,
            },
            Operation::MergeClose => Some(Diagram::MergeClose),
            Operation::MergeSquashClose => Some(Diagram::MergeSquashClose),
            Operation::FastForwardTargetClose => Some(Diagram::FastForwardTargetClose),
            Operation::Detach => Some(Diagram::Detach),
            Operation::Abandon => Some(Diagram::Abandon),
        }
    }

    fn op_key(op: &Operation, index: usize) -> String {
        match op {
            Operation::AdaptiveMerge | Operation::AdaptiveClose => "a".to_string(),
            Operation::Detach => "d".to_string(),
            Operation::Abandon => " ".to_string(),
            _ => format!("{}", index + 1),
        }
    }

    /// Build the list of jj commands that will be executed for a given operation (styled for display).
    pub(crate) fn planned_commands(&self, operation: &Operation) -> Vec<Line<'static>> {
        use super::cmd_spans::{lit, quoted_msg, rev};

        let name = &self.workspace_name;
        let escaped_name = name.replace('\'', "'\\''");
        let mut cmds: Vec<Line<'static>> = Vec::new();

        let target_name = self
            .selected_target()
            .map(|t| t.name.as_str())
            .unwrap_or("default");
        let target_id = self.shortest(
            self.selected_target()
                .map(|t| t.change_id.as_str())
                .unwrap_or("?"),
        );

        // Helper: "jj workspace forget '<name>'"
        let forget_cmd = || Line::from(vec![lit(&format!("jj workspace forget '{escaped_name}'"))]);

        match operation {
            Operation::Merge => {
                if let Some(info) = &self.close_info {
                    let src_eff = self.shortest(&info.src_effective_head);
                    let tgt_eff = self.shortest(&info.tgt_effective_head);
                    let detail = format!("{name}@{src_eff} + {target_name}@{tgt_eff}");
                    let merge_msg = jj_utils::make_desc(jj_utils::Op::Merge, Some(&detail));
                    let step_msg = jj_utils::make_desc(jj_utils::Op::Step, None);

                    cmds.push(Line::from(vec![
                        lit(&format!("[{name}] jj new ")),
                        rev(src_eff),
                        lit(" "),
                        rev(tgt_eff),
                        lit(" -m "),
                        quoted_msg(&merge_msg),
                    ]));
                    cmds.push(Line::from(vec![
                        lit(&format!("[{name}] jj new -m ")),
                        quoted_msg(&step_msg),
                    ]));
                    cmds.push(Line::from(vec![
                        lit(&format!("[{target_name}] jj new <merge> -m ")),
                        quoted_msg(&step_msg),
                    ]));
                }
            }
            Operation::AdaptiveMerge => {
                let mode = self.close_info.as_ref().map(|i| &i.mode);
                let resolved = match mode {
                    Some(SyncMode::SourceOnly) => Operation::FastForwardTarget,
                    Some(SyncMode::TargetOnly) => Operation::FastForwardSource,
                    Some(SyncMode::Diverged) => Operation::Merge,
                    _ => return cmds,
                };
                return self.planned_commands(&resolved);
            }
            Operation::FastForwardTarget => {
                if let Some(info) = &self.close_info {
                    let src_eff = self.shortest(&info.src_effective_head);
                    let detail = format!("{target_name}@ to {name}@{src_eff}");
                    let ff_msg = jj_utils::make_desc(jj_utils::Op::FastForward, Some(&detail));
                    let step_msg = jj_utils::make_desc(jj_utils::Op::Step, None);

                    cmds.push(Line::from(vec![
                        lit(&format!("[{target_name}] jj new ")),
                        rev(src_eff),
                        lit(" -m "),
                        quoted_msg(&ff_msg),
                    ]));
                    if info.src_trivial_id.is_none() {
                        cmds.push(Line::from(vec![
                            lit(&format!("[{name}] jj new -m ")),
                            quoted_msg(&step_msg),
                        ]));
                    }
                    if let Some(id) = &info.tgt_trivial_id {
                        cmds.push(Line::from(vec![
                            lit(&format!("[{target_name}] jj abandon ")),
                            rev(self.shortest(id)),
                        ]));
                    }
                }
            }
            Operation::FastForwardSource => {
                if let Some(info) = &self.close_info {
                    let tgt_eff = self.shortest(&info.tgt_effective_head);
                    let detail = format!("{name}@ to {target_name}@{tgt_eff}");
                    let ff_msg = jj_utils::make_desc(jj_utils::Op::FastForward, Some(&detail));
                    let step_msg = jj_utils::make_desc(jj_utils::Op::Step, None);

                    cmds.push(Line::from(vec![
                        lit(&format!("[{name}] jj new ")),
                        rev(tgt_eff),
                        lit(" -m "),
                        quoted_msg(&ff_msg),
                    ]));
                    if info.tgt_trivial_id.is_none() {
                        cmds.push(Line::from(vec![
                            lit(&format!("[{target_name}] jj new -m ")),
                            quoted_msg(&step_msg),
                        ]));
                    }
                    if let Some(id) = &info.src_trivial_id {
                        cmds.push(Line::from(vec![
                            lit(&format!("[{name}] jj abandon ")),
                            rev(self.shortest(id)),
                        ]));
                    }
                }
            }
            Operation::MergeAbandonOld => {
                if let Some(info) = &self.close_info {
                    let src_eff = self.shortest(&info.src_effective_head);
                    let tgt_eff = self.shortest(&info.tgt_effective_head);
                    let step_msg = jj_utils::make_desc(jj_utils::Op::Step, None);

                    let src_at = self.shortest(&info.src_actual_head);
                    let tgt_at = self.shortest(&info.tgt_actual_head);
                    let detail = format!("{target_name}@{tgt_eff} into {name}@{src_eff}");
                    let merge_msg = jj_utils::make_desc(jj_utils::Op::Merge, Some(&detail));
                    cmds.push(Line::from(vec![
                        lit("jj new "),
                        rev(src_at),
                        lit(" "),
                        rev(tgt_at),
                        lit(" -m "),
                        quoted_msg(&merge_msg),
                    ]));
                    cmds.push(Line::from(vec![
                        lit(&format!("[{name}] jj new <merge> -m ")),
                        quoted_msg(&step_msg),
                    ]));
                    cmds.push(Line::from(vec![
                        lit(&format!("[{target_name}] jj new <merge> -m ")),
                        quoted_msg(&step_msg),
                    ]));

                    // Abandon trivial heads.
                    if let Some(id) = &info.src_trivial_id {
                        cmds.push(Line::from(vec![lit("jj abandon "), rev(self.shortest(id))]));
                    }
                    if let Some(id) = &info.tgt_trivial_id {
                        cmds.push(Line::from(vec![lit("jj abandon "), rev(self.shortest(id))]));
                    }
                }
            }
            Operation::Rebase => {
                if let Some(info) = &self.close_info {
                    let src_eff = self.shortest(&info.src_effective_head);
                    let tgt_eff = self.shortest(&info.tgt_effective_head);
                    let lca = self.shortest(&info.lca);
                    let step_msg = jj_utils::make_desc(jj_utils::Op::Step, None);

                    cmds.push(Line::from(vec![
                        lit("jj rebase --source "),
                        rev(&format!("roots({lca}..{src_eff})")),
                        lit(" --onto "),
                        rev(tgt_eff),
                    ]));
                    if info.tgt_trivial_id.is_none() {
                        cmds.push(Line::from(vec![
                            lit(&format!("[{target_name}] jj new -m ")),
                            quoted_msg(&step_msg),
                        ]));
                    }
                }
            }
            Operation::MergeSquash => {
                if let Some(info) = &self.close_info {
                    let src_eff = self.shortest(&info.src_effective_head);
                    let tgt_eff = self.shortest(&info.tgt_effective_head);
                    let lca = self.shortest(&info.lca);
                    let sq_tgt = self
                        .squash_target
                        .as_deref()
                        .map(|id| self.shortest(id).to_string())
                        .unwrap_or_else(|| format!("roots({lca}..{src_eff})"));
                    let detail = format!("{name}@{src_eff} + {target_name}@{tgt_eff}");
                    let merge_msg = jj_utils::make_desc(jj_utils::Op::Merge, Some(&detail));
                    let step_msg = jj_utils::make_desc(jj_utils::Op::Step, None);

                    cmds.push(Line::from(vec![
                        lit("jj squash --from "),
                        rev(&format!("{lca}..{src_eff}")),
                        lit(" --into "),
                        rev(&sq_tgt),
                        lit(" -m "),
                        quoted_msg(&jj_utils::make_desc(jj_utils::Op::Squash, Some("..."))),
                    ]));
                    cmds.push(Line::from(vec![
                        lit(&format!("[{name}] jj new ")),
                        rev(&sq_tgt),
                        lit(" "),
                        rev(tgt_eff),
                        lit(" -m "),
                        quoted_msg(&merge_msg),
                    ]));
                    cmds.push(Line::from(vec![
                        lit(&format!("[{name}] jj new -m ")),
                        quoted_msg(&step_msg),
                    ]));
                    cmds.push(Line::from(vec![
                        lit(&format!("[{target_name}] jj new <merge> -m ")),
                        quoted_msg(&step_msg),
                    ]));
                }
            }
            Operation::AdaptiveClose => {
                let mode = self.close_info.as_ref().map(|i| &i.mode);
                let resolved = match mode {
                    Some(SyncMode::SourceOnly) => Operation::FastForwardTargetClose,
                    Some(SyncMode::Diverged) => Operation::MergeClose,
                    _ => return cmds,
                };
                return self.planned_commands(&resolved);
            }
            Operation::MergeClose => {
                if let Some(info) = &self.close_info {
                    let src_eff = self.shortest(&info.src_effective_head);
                    let tgt_eff = self.shortest(&info.tgt_effective_head);
                    let detail = format!("{name}@{src_eff} + {target_name}@{tgt_eff}");
                    let merge_msg = jj_utils::make_desc(jj_utils::Op::Merge, Some(&detail));

                    cmds.push(forget_cmd());
                    cmds.push(Line::from(vec![
                        lit("jj new "),
                        rev(src_eff),
                        lit(" "),
                        rev(tgt_eff),
                        lit(" -m "),
                        quoted_msg(&merge_msg),
                    ]));
                }
            }
            Operation::MergeSquashClose => {
                if let Some(info) = &self.close_info {
                    let src_eff = self.shortest(&info.src_effective_head);
                    let tgt_eff = self.shortest(&info.tgt_effective_head);
                    let lca = self.shortest(&info.lca);
                    let sq_tgt = self
                        .squash_target
                        .as_deref()
                        .map(|id| self.shortest(id).to_string())
                        .unwrap_or_else(|| format!("roots({lca}..{src_eff})"));
                    let detail = format!("{name}@{src_eff} + {target_name}@{tgt_eff}");
                    let merge_msg = jj_utils::make_desc(jj_utils::Op::Merge, Some(&detail));

                    cmds.push(forget_cmd());
                    cmds.push(Line::from(vec![
                        lit("jj squash --from "),
                        rev(&format!("{lca}..{src_eff}")),
                        lit(" --into "),
                        rev(&sq_tgt),
                        lit(" -m "),
                        quoted_msg(&jj_utils::make_desc(jj_utils::Op::Squash, Some("..."))),
                    ]));
                    cmds.push(Line::from(vec![
                        lit("jj new "),
                        rev(&sq_tgt),
                        lit(" "),
                        rev(tgt_eff),
                        lit(" -m "),
                        quoted_msg(&merge_msg),
                    ]));
                }
            }
            Operation::FastForwardTargetClose => {
                if let Some(info) = &self.close_info {
                    let src_eff = self.shortest(&info.src_effective_head);

                    cmds.push(forget_cmd());
                    cmds.push(Line::from(vec![
                        lit(&format!("[{target_name}] jj edit ")),
                        rev(src_eff),
                    ]));
                    if let Some(id) = &info.tgt_trivial_id {
                        cmds.push(Line::from(vec![
                            lit(&format!("[{target_name}] jj abandon ")),
                            rev(self.shortest(id)),
                        ]));
                    }
                }
            }
            Operation::Detach => {
                cmds.push(forget_cmd());
            }
            Operation::Abandon => {
                cmds.push(forget_cmd());
                if !self.revisions.is_empty() {
                    let mut spans = vec![lit("jj abandon ")];
                    for (i, r) in self.revisions.iter().enumerate() {
                        if i > 0 {
                            spans.push(lit(" "));
                        }
                        spans.push(rev(self.shortest(r.change_id.as_str())));
                    }
                    cmds.push(Line::from(spans));
                }
            }
        }

        // Singular bookmark auto-advance.
        // Source bookmark.
        if let Some(bm) = &self.singular_bookmark {
            match operation {
                Operation::Abandon => {
                    cmds.push(Line::from(vec![lit(&format!(
                        "jj bookmark delete -- {bm}"
                    ))]));
                }
                Operation::Detach
                | Operation::MergeClose
                | Operation::MergeSquashClose
                | Operation::FastForwardTargetClose => {
                    if let Some(info) = &self.close_info {
                        let target = self.shortest(&info.src_effective_head);
                        cmds.push(Line::from(vec![
                            lit("jj bookmark set --allow-backwards --revision "),
                            rev(target),
                            lit(&format!(" -- {bm}")),
                        ]));
                    }
                }
                Operation::FastForwardTarget
                | Operation::FastForwardSource
                | Operation::Merge
                | Operation::MergeAbandonOld
                | Operation::Rebase
                | Operation::MergeSquash => {
                    let escaped = jujutsu::escape_revset_string(name);
                    cmds.push(Line::from(vec![
                        lit("jj bookmark set --allow-backwards --revision "),
                        rev(&format!("\"{escaped}\"@")),
                        lit(&format!(" -- {bm}")),
                    ]));
                }
                Operation::AdaptiveMerge | Operation::AdaptiveClose => {}
            }
        }
        // Target bookmark — advances when the target gets a new head.
        if let Some(bm) = &self.target_singular_bookmark {
            match operation {
                Operation::MergeClose
                | Operation::MergeSquashClose
                | Operation::FastForwardTargetClose => {
                    let escaped = jujutsu::escape_revset_string(target_name);
                    cmds.push(Line::from(vec![
                        lit("jj bookmark set --allow-backwards --revision "),
                        rev(&format!("\"{escaped}\"@")),
                        lit(&format!(" -- {bm}")),
                    ]));
                }
                Operation::FastForwardTarget
                | Operation::FastForwardSource
                | Operation::Merge
                | Operation::MergeAbandonOld
                | Operation::Rebase
                | Operation::MergeSquash => {
                    let escaped = jujutsu::escape_revset_string(target_name);
                    cmds.push(Line::from(vec![
                        lit("jj bookmark set --allow-backwards --revision "),
                        rev(&format!("\"{escaped}\"@")),
                        lit(&format!(" -- {bm}")),
                    ]));
                }
                _ => {}
            }
        }

        // Manual bookmark action commands (close only, before file deletion).
        if self.intent == DialogIntent::Close && !self.bookmarks.is_empty() {
            match self.bookmark_action {
                BookmarkAction::Advance => {
                    for bm in &self.bookmarks {
                        cmds.push(Line::from(vec![
                            lit("jj bookmark set --allow-backwards --revision "),
                            rev(target_id),
                            lit(&format!(" -- {bm}")),
                        ]));
                    }
                }
                BookmarkAction::Delete => {
                    for bm in &self.bookmarks {
                        cmds.push(Line::from(vec![lit(&format!(
                            "jj bookmark delete -- {bm}"
                        ))]));
                    }
                }
                BookmarkAction::NoAction => {}
            }
        }

        if self.intent == DialogIntent::Close
            && self.delete_files
            && !self.workspace_path.as_os_str().is_empty()
        {
            let escaped_path = self
                .workspace_path
                .display()
                .to_string()
                .replace('\'', "'\\''");
            cmds.push(Line::from(vec![lit(&format!("rm -rf '{escaped_path}'"))]));
        }

        cmds
    }

    pub(crate) fn draw(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);

        let title = match self.intent {
            DialogIntent::Transfer => format!(" Transfer: {} ", self.workspace_name),
            DialogIntent::Close => format!(" Close: {} ", self.workspace_name),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().dim())
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height < 6 || inner.width < 20 {
            return;
        }

        // Compute command section size.
        let all_ops = self.all_operations();
        let cmds = all_ops
            .get(self.selected_index)
            .map(|op| self.planned_commands(op))
            .unwrap_or_default();
        // Reserve space for: help box (3) + command lines + info
        let cmd_lines = cmds.len() as u16;
        let info = all_ops.get(self.selected_index).and_then(|op| {
            let cm = self.close_info.as_ref().map(|i| &i.mode);
            Self::op_diagram(op, cm).map(|d| d.info())
        });
        let bottom_h = dialog_common::bottom_section_height(cmd_lines, info.as_ref(), inner.width);
        let top_h = inner.height.saturating_sub(bottom_h);

        let mut layout = DialogLayout::new(inner, top_h);

        // --- TOP SECTION ---

        // Transient notices (stale-refresh banner, snapshot failures).
        if !self.notice.is_empty() {
            for n in &self.notice {
                layout.draw_line(
                    frame,
                    &[Span::styled(
                        format!(" \u{26a0} {n}"),
                        Style::default().fg(Color::Yellow),
                    )],
                );
            }
            layout.skip(1);
        }

        // Revision list
        if self.revisions.is_empty() {
            layout.draw_line(
                frame,
                &[Span::styled(
                    "  (no unique revisions)",
                    Style::default().dim(),
                )],
            );
        } else {
            let max_revs = top_h.saturating_sub(12) as usize;
            for (i, rev) in self.revisions.iter().take(max_revs).enumerate() {
                let is_head = i == 0;
                let marker = if is_head { " @" } else { "  " };
                layout.draw_line(
                    frame,
                    &[
                        Span::styled(marker, Style::default().fg(Color::Green)),
                        Span::styled(
                            format!(" {} ", dialog_common::short(&rev.change_id)),
                            Style::default().fg(Color::Yellow),
                        ),
                        Span::styled(&rev.description, Style::default().dim()),
                    ],
                );
            }
            if self.revisions.len() > max_revs {
                layout.draw_line(
                    frame,
                    &[Span::styled(
                        format!("  ... and {} more", self.revisions.len() - max_revs),
                        Style::default().dim(),
                    )],
                );
            }
        }

        layout.skip(1);

        // Target workspace
        if let Some(target) = self.selected_target() {
            layout.draw_target_row(frame, target, self.targets.len() > 1, None);
        }

        layout.skip(1);

        // Mode description
        {
            let target_name = self
                .selected_target()
                .map(|t| t.name.as_str())
                .unwrap_or("default");
            let mode_desc = match self.close_info.as_ref().map(|i| &i.mode) {
                Some(SyncMode::InSync) => "already in sync \u{2014} nothing to merge".to_string(),
                Some(SyncMode::SourceOnly) => {
                    format!("fast-forward {target_name} to {}", self.workspace_name)
                }
                Some(SyncMode::TargetOnly) => {
                    format!("fast-forward {} to {target_name}", self.workspace_name)
                }
                Some(SyncMode::Diverged) => {
                    format!("merge {} and {target_name}", self.workspace_name)
                }
                Some(SyncMode::Error(msg)) => msg.clone(),
                None => String::new(),
            };
            if !mode_desc.is_empty() {
                layout.draw_line(
                    frame,
                    &[Span::styled(
                        format!("  {mode_desc}"),
                        Style::default().dim(),
                    )],
                );
                layout.skip(1);
            }
        }

        // Operations (unified navigable list)
        if all_ops.is_empty() {
            layout.draw_line(
                frame,
                &[Span::styled(
                    "  (no operations available)",
                    Style::default().dim(),
                )],
            );
        } else {
            // Track the number-key index separately from the all_ops index
            let mut num_idx = 0usize;
            for (i, op) in all_ops.iter().enumerate() {
                // Visual gap before disposal operations and after adaptive
                if *op == Operation::Detach
                    || (i > 0
                        && matches!(
                            all_ops[i - 1],
                            Operation::AdaptiveMerge | Operation::AdaptiveClose
                        ))
                {
                    layout.skip(1);
                }
                let selected = i == self.selected_index;
                let key = Self::op_key(op, num_idx);
                if !matches!(
                    op,
                    Operation::AdaptiveMerge
                        | Operation::AdaptiveClose
                        | Operation::Detach
                        | Operation::Abandon
                ) {
                    num_idx += 1;
                }
                let label = Self::op_label(op);
                let label_w = 13;
                let (prefix, key_style, label_style, desc_style) = if selected {
                    (
                        "> ",
                        Style::default().fg(Color::Green).bold(),
                        Style::default().fg(Color::Green),
                        Style::default().fg(Color::Green).dim(),
                    )
                } else {
                    (
                        "  ",
                        Style::default().bold(),
                        Style::default(),
                        Style::default().dim(),
                    )
                };
                let mut spans = vec![
                    Span::styled(format!("{prefix}{key}"), key_style),
                    Span::styled(format!("  {label:<label_w$}"), label_style),
                ];
                spans.extend(self.op_desc_spans(op, desc_style));
                layout.draw_line(frame, &spans);
            }
        }

        layout.gap();

        if self.intent == DialogIntent::Close && !self.bookmarks.is_empty() {
            let (label, style) = match self.bookmark_action {
                BookmarkAction::NoAction => ("bookmarks [no action]", Style::default().dim()),
                BookmarkAction::Advance => ("bookmarks [advance]", Style::default().bold()),
                BookmarkAction::Delete => {
                    ("bookmarks [delete]", Style::default().fg(Color::Red).bold())
                }
            };
            layout.draw_toggle(frame, "b", label, style);
            for bm in &self.bookmarks {
                layout.draw_line(
                    frame,
                    &[Span::styled(
                        format!("       {bm}"),
                        Style::default().fg(Color::DarkGray),
                    )],
                );
            }
        }

        if self.intent == DialogIntent::Close {
            let (label, style) = if self.delete_files {
                ("delete files [on]", Style::default().fg(Color::Red).bold())
            } else {
                ("delete files [off]", Style::default().fg(Color::White))
            };
            layout.draw_toggle(frame, "k", label, style);
        }

        layout.gap();

        // Help box (cancel / accept)
        let accept_style = if all_ops.is_empty() {
            Style::default().dim()
        } else {
            Style::default().fg(Color::Green).bold()
        };
        layout.draw_help_box(
            frame,
            &[
                HelpBinding {
                    key: "Esc",
                    label: "cancel",
                    style: Style::default().bold(),
                },
                HelpBinding {
                    key: "y/Enter",
                    label: "accept",
                    style: accept_style,
                },
                HelpBinding {
                    key: "c",
                    label: "copy",
                    style: Style::default().bold(),
                },
            ],
        );

        // --- BOTTOM: command preview + info diagram ---
        dialog_common::draw_command_preview(frame, inner, layout.y_offset(), &cmds, info.as_ref());
    }
}
