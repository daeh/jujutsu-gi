use super::dialog_common::{
    self, DialogLayout, HelpBinding, SyncMode, SyncModeInfo, TargetWorkspace,
};
use super::dialog_info::Diagram;
use crate::{commands, jj_utils, jujutsu};
use crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear};
use std::collections::HashMap;

const NAME_W: usize = 20;

pub(crate) struct SyncDialog {
    pub source_name: String,
    pub source_path: std::path::PathBuf,
    pub targets: Vec<TargetWorkspace>,
    target_index: usize,
    repo_root: std::path::PathBuf,
    sync_info: Option<SyncModeInfo>,
    shortest_ids: HashMap<String, String>,
    /// Config workspace-path template (for singular-bookmark identification).
    workspace_path_template: String,
    /// Repository display name (for singular-bookmark identification).
    repo_name: String,
    /// Cached singular bookmark for the source workspace.
    src_singular_bookmark: Option<String>,
    /// Cached singular bookmark for the current target workspace.
    tgt_singular_bookmark: Option<String>,
    /// Transient notices rendered at the top of the dialog (stale-refresh
    /// banner, snapshot failures). Cleared on target cycle.
    pub(crate) notice: Vec<String>,
    /// Op head at the last point the dialog's data was made fresh (open or an
    /// in-place stale refresh, both preceded by a snapshot). The execute gate
    /// compares the *current* op head against THIS, not `sync_info.op_head` —
    /// `recompute()` re-stamps the latter to "now" on every target change,
    /// which would otherwise mask external movement from the gate.
    pub(crate) freshness_baseline: String,
}

impl SyncDialog {
    pub(crate) fn new(
        source_name: String,
        source_path: std::path::PathBuf,
        targets: Vec<TargetWorkspace>,
        default_target_idx: usize,
        repo_root: std::path::PathBuf,
        workspace_path_template: String,
        repo_name: String,
    ) -> Self {
        let mut dialog = Self {
            source_name,
            source_path,
            targets,
            target_index: default_target_idx,
            repo_root,
            sync_info: None,
            shortest_ids: HashMap::new(),
            workspace_path_template,
            repo_name,
            src_singular_bookmark: None,
            tgt_singular_bookmark: None,
            notice: Vec::new(),
            freshness_baseline: String::new(),
        };
        dialog.recompute();
        // Baseline = op head when this dialog's data was first made fresh
        // (the open handler snapshotted via refresh() immediately before).
        dialog.freshness_baseline = dialog
            .sync_info
            .as_ref()
            .map(|i| i.op_head.clone())
            .unwrap_or_default();
        dialog
    }

    /// Re-anchor the freshness baseline (call after an in-place refresh, whose
    /// snapshot makes the data current again).
    pub(crate) fn set_freshness_baseline(&mut self, op_head: String) {
        self.freshness_baseline = op_head;
    }

    pub(crate) fn selected_target(&self) -> Option<&TargetWorkspace> {
        self.targets.get(self.target_index)
    }

    /// Return the cached sync mode info (if computed).
    pub(crate) fn sync_info(&self) -> Option<&SyncModeInfo> {
        self.sync_info.as_ref()
    }

    /// Replace the cached sync mode info (execute-gate Equivalent path:
    /// identical plan, newer op head).
    pub(crate) fn set_sync_info(&mut self, info: SyncModeInfo) {
        self.sync_info = Some(info);
    }

    /// Re-resolve executable inputs from fresh workspace data after a stale
    /// gate detection: replace the target list (preserving the selection by
    /// name) and update the source path. Returns notices for anything that
    /// could not be preserved. Caller runs `recompute()` afterwards.
    pub(crate) fn refresh_entries(
        &mut self,
        source_path: Option<std::path::PathBuf>,
        targets: Vec<TargetWorkspace>,
    ) -> Vec<String> {
        let mut notices = Vec::new();
        let prev = self.selected_target().map(|t| t.name.clone());
        self.targets = targets;
        self.target_index = match prev
            .as_deref()
            .and_then(|name| self.targets.iter().position(|t| t.name == name))
        {
            Some(idx) => idx,
            None => {
                if prev.is_some() {
                    notices.push("previous target no longer exists — selection reset".to_string());
                }
                0
            }
        };
        match source_path {
            Some(p) => self.source_path = p,
            None => notices.push(format!(
                "source workspace {} no longer exists",
                self.source_name
            )),
        }
        notices
    }

    pub(crate) fn handle_key(&mut self, key: KeyCode) {
        let len = self.targets.len();
        // With 0 or 1 target there is nothing to cycle — return before the
        // cycle branches so a single-target dialog's stale-refresh banner is
        // not cleared by a no-op Left/Right.
        if len <= 1 {
            return;
        }
        match key {
            KeyCode::Char('k') | KeyCode::Left => {
                // Clear the stale-refresh banner only when the target actually
                // cycles (matches close/transfer's cycle_target behavior); a
                // stray key must not erase the warning before re-confirmation.
                self.notice.clear();
                self.target_index = if self.target_index == 0 {
                    len - 1
                } else {
                    self.target_index - 1
                };
            }
            KeyCode::Char('j') | KeyCode::Right => {
                self.notice.clear();
                self.target_index = if self.target_index >= len - 1 {
                    0
                } else {
                    self.target_index + 1
                };
            }
            _ => {}
        }
    }

    /// Recompute cached mode info for the current target.
    /// Called on construction and after target changes.
    pub(crate) fn recompute(&mut self) {
        let Some(target) = self.selected_target() else {
            self.sync_info = None;
            self.shortest_ids.clear();
            return;
        };
        let tgt_owned = target.name.clone();
        let repo = &self.repo_root;

        let info = commands::detect_sync_mode(repo, &self.source_name, &tgt_owned);

        // Batch-resolve shortest prefixes for display.
        let mut all_ids: Vec<&str> = vec![&info.src_effective_head, &info.tgt_effective_head];
        all_ids.push(&info.src_actual_head);
        all_ids.push(&info.tgt_actual_head);
        if let Some(id) = &info.src_trivial_id {
            all_ids.push(id);
        }
        if let Some(id) = &info.tgt_trivial_id {
            all_ids.push(id);
        }
        self.shortest_ids = jujutsu::shortest_change_ids(repo, &all_ids);

        self.sync_info = Some(info);

        // Cache singular bookmarks for both workspaces.
        let ws_tmpl = &self.workspace_path_template;
        let rn = &self.repo_name;
        self.src_singular_bookmark =
            jj_utils::identify_singular_bookmark(repo, ws_tmpl, rn, &self.source_name)
                .map(|(name, _)| name);
        self.tgt_singular_bookmark =
            jj_utils::identify_singular_bookmark(repo, ws_tmpl, rn, &tgt_owned)
                .map(|(name, _)| name);
    }

    /// Look up the shortest prefix for a change ID from the cache.
    fn shortest<'a>(&'a self, id: &'a str) -> &'a str {
        self.shortest_ids
            .get(id)
            .map(|s| s.as_str())
            .unwrap_or(&id[..id.len().min(4)])
    }

    /// Whether the current target has an actionable sync operation (not InSync/Error).
    pub(crate) fn has_operation(&self) -> bool {
        self.sync_info
            .as_ref()
            .is_some_and(|info| !matches!(info.mode, SyncMode::InSync | SyncMode::Error(_)))
    }

    /// Build the list of jj commands that will be executed (styled for display).
    pub(crate) fn planned_commands(&self) -> Vec<Line<'static>> {
        use super::cmd_spans::{lit, quoted_msg, rev};

        let Some(target) = self.selected_target() else {
            return vec![];
        };
        let Some(info) = &self.sync_info else {
            return vec![];
        };
        let src = &self.source_name;
        let tgt = &target.name;
        let s = |id: &str| -> String { self.shortest(id).to_string() };

        let src_short = s(&info.src_effective_head);
        let tgt_short = s(&info.tgt_effective_head);

        let step_msg_text = jj_utils::make_desc(jj_utils::Op::Step, None);
        let mut cmds: Vec<Line<'static>> = Vec::new();

        match info.mode {
            SyncMode::InSync => {
                cmds.push(Line::from(vec![lit("already in sync")]));
            }
            SyncMode::SourceOnly => {
                // Source has new work — fast-forward target.
                let detail = format!("{tgt}@ to {src}@{src_short}");
                let msg_text = jj_utils::make_desc(jj_utils::Op::FastForward, Some(&detail));
                cmds.push(Line::from(vec![
                    lit(&format!("[{tgt}] jj new ")),
                    rev(&src_short),
                    lit(" -m "),
                    quoted_msg(&msg_text),
                ]));
                if info.src_trivial_id.is_none() {
                    cmds.push(Line::from(vec![
                        lit(&format!("[{src}] jj new -m ")),
                        quoted_msg(&step_msg_text),
                    ]));
                }
                if let Some(id) = &info.tgt_trivial_id {
                    cmds.push(Line::from(vec![
                        lit(&format!("[{tgt}] jj abandon ")),
                        rev(&s(id)),
                    ]));
                }
            }
            SyncMode::TargetOnly => {
                // Target has new work — fast-forward source.
                let detail = format!("{src}@ to {tgt}@{tgt_short}");
                let msg_text = jj_utils::make_desc(jj_utils::Op::FastForward, Some(&detail));
                cmds.push(Line::from(vec![
                    lit(&format!("[{src}] jj new ")),
                    rev(&tgt_short),
                    lit(" -m "),
                    quoted_msg(&msg_text),
                ]));
                if info.tgt_trivial_id.is_none() {
                    cmds.push(Line::from(vec![
                        lit(&format!("[{tgt}] jj new -m ")),
                        quoted_msg(&step_msg_text),
                    ]));
                }
                if let Some(id) = &info.src_trivial_id {
                    cmds.push(Line::from(vec![
                        lit(&format!("[{src}] jj abandon ")),
                        rev(&s(id)),
                    ]));
                }
            }
            SyncMode::Diverged => {
                let src_at_short = s(&info.src_actual_head);
                let tgt_at_short = s(&info.tgt_actual_head);

                let detail = format!("{tgt}@{tgt_short} into {src}@{src_short}");
                let msg_text = jj_utils::make_desc(jj_utils::Op::Merge, Some(&detail));
                cmds.push(Line::from(vec![
                    lit("jj new "),
                    rev(&src_at_short),
                    lit(" "),
                    rev(&tgt_at_short),
                    lit(" -m "),
                    quoted_msg(&msg_text),
                ]));
                cmds.push(Line::from(vec![
                    lit(&format!("[{src}] jj new <merge> -m ")),
                    quoted_msg(&step_msg_text),
                ]));
                cmds.push(Line::from(vec![
                    lit(&format!("[{tgt}] jj new <merge> -m ")),
                    quoted_msg(&step_msg_text),
                ]));
                // Abandon trivial tips last.
                for (opt, ws) in [
                    (&info.src_trivial_id, src.as_str()),
                    (&info.tgt_trivial_id, tgt.as_str()),
                ] {
                    if let Some(id) = opt {
                        cmds.push(Line::from(vec![
                            lit(&format!("[{ws}] jj abandon ")),
                            rev(&s(id)),
                        ]));
                    }
                }
            }
            SyncMode::Error(ref e) => {
                cmds.push(Line::from(vec![lit(&format!("error: {e}"))]));
            }
        }

        // Singular bookmark auto-advance commands (cached in recompute()).
        // After sync, each workspace's singular bookmark advances to its @.
        if !matches!(info.mode, SyncMode::InSync | SyncMode::Error(_)) {
            let src_escaped = jujutsu::escape_revset_string(src);
            let tgt_escaped = jujutsu::escape_revset_string(tgt);

            // Source bookmark — advances when source gets a new head.
            if matches!(info.mode, SyncMode::TargetOnly | SyncMode::Diverged)
                && let Some(bm) = &self.src_singular_bookmark
            {
                cmds.push(Line::from(vec![
                    lit("jj bookmark set --revision "),
                    rev(&format!("\"{src_escaped}\"@")),
                    lit(&format!(" -- {bm}")),
                ]));
            }
            // Target bookmark — advances when target gets a new head.
            if matches!(info.mode, SyncMode::SourceOnly | SyncMode::Diverged)
                && let Some(bm) = &self.tgt_singular_bookmark
            {
                cmds.push(Line::from(vec![
                    lit("jj bookmark set --revision "),
                    rev(&format!("\"{tgt_escaped}\"@")),
                    lit(&format!(" -- {bm}")),
                ]));
            }
        }

        cmds
    }

    pub(crate) fn draw(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);

        let target_name = self
            .selected_target()
            .map(|t| t.name.as_str())
            .unwrap_or("?");
        let Some(mi) = &self.sync_info else {
            return;
        };

        let title = match mi.mode {
            SyncMode::InSync => format!(" @{} and @{target_name} in sync ", self.source_name),
            SyncMode::SourceOnly => {
                format!(" Fast-forward @{target_name} to @{} ", self.source_name)
            }
            SyncMode::TargetOnly => {
                format!(" Fast-forward @{} to @{target_name} ", self.source_name)
            }
            SyncMode::Diverged => {
                format!(" Merge @{} and @{target_name} ", self.source_name)
            }
            SyncMode::Error(_) => " Sync error ".to_string(),
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().dim())
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height < 4 || inner.width < 20 {
            return;
        }

        // Compute info for current mode.
        let diagram_info = match mi.mode {
            SyncMode::Diverged => Some(Diagram::Merge.info()),
            SyncMode::SourceOnly | SyncMode::TargetOnly => Some(Diagram::FastForward.info()),
            _ => None,
        };

        // Compute bottom section size (help + commands + info).
        let cmds = self.planned_commands();
        let cmd_lines = cmds.len() as u16;
        let bottom_h =
            dialog_common::bottom_section_height(cmd_lines, diagram_info.as_ref(), inner.width);
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

        // Target workspace
        if let Some(target) = self.selected_target() {
            layout.draw_target_row(frame, target, self.targets.len() > 1, Some(NAME_W));
        }

        layout.skip(1);

        // Mode description
        let mode_desc = match mi.mode {
            SyncMode::InSync => "already in sync \u{2014} nothing to do".to_string(),
            SyncMode::SourceOnly => {
                format!("fast-forward {target_name} to {}", self.source_name)
            }
            SyncMode::TargetOnly => {
                format!("fast-forward {} to {target_name}", self.source_name)
            }
            SyncMode::Diverged => {
                format!("merge {} and {target_name}", self.source_name)
            }
            SyncMode::Error(ref msg) => msg.clone(),
        };
        layout.draw_line(
            frame,
            &[Span::styled(
                format!("  {mode_desc}"),
                Style::default().dim(),
            )],
        );

        layout.gap();

        // Help box (cancel / accept)
        let action = match mi.mode {
            SyncMode::InSync | SyncMode::Error(_) => "",
            SyncMode::SourceOnly | SyncMode::TargetOnly => "fast-forward",
            SyncMode::Diverged => "merge",
        };
        let accept_style = if action.is_empty() {
            Style::default().dim()
        } else {
            Style::default().fg(Color::Green).bold()
        };
        let accept_label = if action.is_empty() { "accept" } else { action };
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
                    label: accept_label,
                    style: accept_style,
                },
            ],
        );

        // --- BOTTOM: command preview + info diagram ---
        dialog_common::draw_command_preview(
            frame,
            inner,
            layout.y_offset(),
            &cmds,
            diagram_info.as_ref(),
        );
    }
}
