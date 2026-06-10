use crate::action_history::ActionRecord;
use crate::jujutsu::Operation;
use crate::text_utils;
use crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Paragraph, Wrap};

const GUTTER_W: usize = 2;
const ID_W: usize = 13;
const TIME_W: usize = 6;

/// Colors for ji action chunks, cycling through this palette.
const JI_COLORS: &[Color] = &[
    Color::Blue,
    Color::Green,
    Color::Magenta,
    Color::Cyan,
    Color::LightBlue,
    Color::LightGreen,
];

pub(crate) struct OpLogPane {
    operations: Vec<Operation>,
    /// Indices into `operations` after snapshot filtering.
    filtered_indices: Vec<usize>,
    state: ListState,
    show_snapshots: bool,
    /// Cached op-show text for the currently selected operation.
    detail_text: Option<String>,
    /// Operation ID corresponding to `detail_text`.
    detail_op_id: Option<String>,
    detail_scroll: u16,
    detail_content_height: u16,
    /// Cached area of the list pane (for mouse hit-testing).
    list_area: Rect,
    /// Cached area of the detail pane (for mouse hit-testing).
    detail_area: Rect,
    /// View scroll offset: index of the first visible item.
    list_scroll: usize,
    /// Cached number of visible rows in the list area.
    list_visible_height: u16,
    /// Pre/post op ID pairs from the action history, for coloring.
    action_ranges: Vec<(String, String)>,
    /// Per-filtered-index color: None = not a ji action, Some(idx) = JI_COLORS index.
    color_map: Vec<Option<usize>>,
}

impl OpLogPane {
    pub(crate) fn new(operations: Vec<Operation>, action_records: &[ActionRecord]) -> Self {
        let action_ranges: Vec<(String, String)> = action_records
            .iter()
            .map(|r| (r.pre_op_id.clone(), r.post_op_id.clone()))
            .collect();
        let mut pane = Self {
            operations,
            filtered_indices: Vec::new(),
            state: ListState::default(),
            show_snapshots: false,
            detail_text: None,
            detail_op_id: None,
            detail_scroll: 0,
            detail_content_height: 0,
            list_area: Rect::default(),
            detail_area: Rect::default(),
            list_scroll: 0,
            list_visible_height: 0,
            action_ranges,
            color_map: Vec::new(),
        };
        pane.rebuild_filter();
        pane
    }

    pub(crate) fn selected_operation(&self) -> Option<&Operation> {
        self.state
            .selected()
            .and_then(|i| self.filtered_indices.get(i))
            .and_then(|&idx| self.operations.get(idx))
    }

    /// Select by visual row position (for mouse clicks).
    /// Accounts for the current scroll offset.
    pub(crate) fn select_visual_row(&mut self, row: usize) {
        let index = row + self.list_scroll;
        if index < self.filtered_indices.len() {
            self.state.select(Some(index));
            self.detail_scroll = 0;
        }
    }

    pub(crate) fn toggle_snapshots(&mut self) {
        let selected_id = self.selected_operation().map(|op| op.id.clone());
        self.show_snapshots = !self.show_snapshots;
        self.rebuild_filter();
        // Preserve selection by op id
        if let Some(id) = selected_id
            && let Some(pos) = self
                .filtered_indices
                .iter()
                .position(|&idx| self.operations[idx].id == id)
        {
            self.state.select(Some(pos));
            self.ensure_selection_visible();
            return;
        }
        if !self.filtered_indices.is_empty() {
            self.state.select(Some(0));
            self.list_scroll = 0;
        }
    }

    fn rebuild_filter(&mut self) {
        self.filtered_indices = self
            .operations
            .iter()
            .enumerate()
            .filter(|(_, op)| self.show_snapshots || !op.is_snapshot)
            .map(|(i, _)| i)
            .collect();
        // Clamp selection
        if self.filtered_indices.is_empty() {
            self.state.select(None);
        } else if let Some(sel) = self.state.selected() {
            if sel >= self.filtered_indices.len() {
                self.state.select(Some(self.filtered_indices.len() - 1));
            }
        } else {
            self.state.select(Some(0));
        }
        // Clamp list_scroll
        let max_scroll = self
            .filtered_indices
            .len()
            .saturating_sub(self.list_visible_height as usize);
        self.list_scroll = self.list_scroll.min(max_scroll);
        self.ensure_selection_visible();
        self.rebuild_color_map();
    }

    /// Compute per-item color assignments based on action history ranges.
    ///
    /// The op log is ordered newest-first. For each action record, post_op_id
    /// is newer (lower index) and pre_op_id is older (higher index). Operations
    /// between them (inclusive of post, exclusive of pre) belong to that action.
    fn rebuild_color_map(&mut self) {
        self.color_map = vec![None; self.filtered_indices.len()];

        for (action_idx, (pre_id, post_id)) in self.action_ranges.iter().enumerate() {
            let post_pos = self
                .filtered_indices
                .iter()
                .position(|&oi| self.operations[oi].id == *post_id);
            let pre_pos = self
                .filtered_indices
                .iter()
                .position(|&oi| self.operations[oi].id == *pre_id);

            if let (Some(post_i), Some(pre_i)) = (post_pos, pre_pos) {
                // Newest-first: post_i <= pre_i. Mark post_i..pre_i (exclusive of pre).
                let start = post_i.min(pre_i);
                let end = post_i.max(pre_i);
                let color_idx = action_idx % JI_COLORS.len();
                for i in start..end {
                    self.color_map[i] = Some(color_idx);
                }
            }
        }
    }

    /// Returns true if the selection changed.
    pub(crate) fn handle_key(&mut self, key: KeyCode) -> bool {
        let len = self.filtered_indices.len();
        if len == 0 {
            return false;
        }
        let current = self.state.selected().unwrap_or(0);
        match key {
            KeyCode::Up | KeyCode::Char('k') => {
                let next = if current == 0 { len - 1 } else { current - 1 };
                self.state.select(Some(next));
                self.detail_scroll = 0;
                self.ensure_selection_visible();
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let next = if current >= len - 1 { 0 } else { current + 1 };
                self.state.select(Some(next));
                self.detail_scroll = 0;
                self.ensure_selection_visible();
                true
            }
            _ => false,
        }
    }

    pub(crate) fn list_area(&self) -> Rect {
        self.list_area
    }

    pub(crate) fn detail_area(&self) -> Rect {
        self.detail_area
    }

    /// Scroll the list view up. Clamps selection to stay visible.
    pub(crate) fn scroll_list_up(&mut self) {
        if self.list_scroll > 0 {
            self.list_scroll -= 1;
            self.clamp_selection_to_view();
        }
    }

    /// Scroll the list view down. Clamps selection to stay visible.
    pub(crate) fn scroll_list_down(&mut self) {
        let max = self
            .filtered_indices
            .len()
            .saturating_sub(self.list_visible_height as usize);
        if self.list_scroll < max {
            self.list_scroll += 1;
            self.clamp_selection_to_view();
        }
    }

    /// If the selected item is outside the visible viewport, pull it to the nearest edge.
    fn clamp_selection_to_view(&mut self) {
        if let Some(sel) = self.state.selected() {
            let visible = self.list_visible_height as usize;
            if visible == 0 {
                return;
            }
            if sel < self.list_scroll {
                self.state.select(Some(self.list_scroll));
                self.detail_scroll = 0;
            } else if sel >= self.list_scroll + visible {
                self.state
                    .select(Some((self.list_scroll + visible).saturating_sub(1)));
                self.detail_scroll = 0;
            }
        }
    }

    /// Auto-scroll `list_scroll` so the selected item is within the visible viewport.
    fn ensure_selection_visible(&mut self) {
        if let Some(sel) = self.state.selected() {
            let visible = self.list_visible_height as usize;
            if visible == 0 {
                return;
            }
            if sel < self.list_scroll {
                self.list_scroll = sel;
            } else if sel >= self.list_scroll + visible {
                self.list_scroll = sel - visible + 1;
            }
        }
    }

    pub(crate) fn scroll_detail_up(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_sub(1);
    }

    pub(crate) fn scroll_detail_down(&mut self) {
        let visible = self.detail_area.height.saturating_sub(2); // borders
        let max = self.detail_content_height.saturating_sub(visible);
        if self.detail_scroll < max {
            self.detail_scroll += 1;
        }
    }

    /// Set the cached op-show detail text for a given operation.
    pub(crate) fn set_detail(&mut self, op_id: &str, text: String) {
        self.detail_op_id = Some(op_id.to_string());
        self.detail_text = Some(text);
        self.detail_scroll = 0;
    }

    /// Returns the op_id that needs detail fetching, if the cache is stale.
    pub(crate) fn needs_detail_fetch(&self) -> Option<&str> {
        let op = self.selected_operation()?;
        if self.detail_op_id.as_deref() == Some(&op.id) {
            None
        } else {
            Some(&op.id)
        }
    }

    pub(crate) fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
        can_undo: bool,
        can_redo: bool,
    ) {
        let border_style = if focused {
            Style::default().fg(Color::White).bold()
        } else {
            Style::default().fg(Color::DarkGray)
        };

        // Split into two panes: list on top, detail on bottom
        let detail_height = (area.height / 4).max(4);
        let panes = Layout::vertical([
            Constraint::Min(5),                // list pane
            Constraint::Length(detail_height), // detail pane
        ])
        .split(area);

        // --- Top pane: Op Log list ---
        self.list_area = panes[0];
        let list_title = if self.show_snapshots {
            " Op Log (all) "
        } else {
            " Op Log "
        };
        let list_block = Block::bordered()
            .title(list_title)
            .border_type(BorderType::Rounded)
            .border_style(border_style);
        let list_inner = list_block.inner(panes[0]);
        frame.render_widget(list_block, panes[0]);

        let list_chunks = Layout::vertical([
            Constraint::Length(1), // header
            Constraint::Min(1),    // items
        ])
        .split(list_inner);

        // Header
        let header = Line::from(vec![
            Span::styled(format!("{:<GUTTER_W$}", ""), Style::default()),
            Span::styled(format!("{:<ID_W$}", "Operation"), Style::default().bold()),
            Span::styled("Description", Style::default().bold()),
        ]);
        frame.render_widget(header, list_chunks[0]);

        // Cache list geometry
        self.list_visible_height = list_chunks[1].height;

        // Set our scroll offset before ratatui renders
        *self.state.offset_mut() = self.list_scroll;

        // Operation rows
        let items: Vec<ListItem> = self
            .filtered_indices
            .iter()
            .enumerate()
            .map(|(fi, &idx)| {
                let op = &self.operations[idx];
                let gutter = if op.is_current { "@ " } else { "  " };
                let gutter_style = if op.is_current {
                    Style::default().fg(Color::Green).bold()
                } else {
                    Style::default()
                };

                // ID color: grey for snapshots, cycling ji color for action
                // ranges, yellow for everything else.
                let id_style = if op.is_snapshot {
                    Style::default().fg(Color::DarkGray)
                } else if let Some(ci) = self.color_map.get(fi).copied().flatten() {
                    Style::default().fg(JI_COLORS[ci])
                } else {
                    Style::default().fg(Color::Yellow)
                };

                let time_str = extract_time(&op.timestamp);

                let desc_width = list_chunks[1]
                    .width
                    .saturating_sub((GUTTER_W + ID_W + TIME_W + 1) as u16)
                    as usize;
                let desc = text_utils::truncate_end(&op.description, desc_width);
                let desc_style = if op.is_snapshot {
                    Style::default().dim()
                } else if op.is_current {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default()
                };

                let line = Line::from(vec![
                    Span::styled(gutter, gutter_style),
                    Span::styled(
                        format!("{:<ID_W$}", &op.id[..op.id.len().min(ID_W - 1)]),
                        id_style,
                    ),
                    Span::styled(desc, desc_style),
                    Span::styled(format!("{time_str:>TIME_W$}"), Style::default().dim()),
                ]);
                ListItem::new(line)
            })
            .collect();

        let list = List::new(items).highlight_style(Style::default().bg(Color::DarkGray));
        frame.render_stateful_widget(list, list_chunks[1], &mut self.state);

        // --- Bottom pane: Detail ---
        self.detail_area = panes[1];
        let detail_block = Block::bordered()
            .title(" Detail ")
            .border_type(BorderType::Rounded)
            .border_style(border_style);
        let detail_inner = detail_block.inner(panes[1]);
        frame.render_widget(detail_block, panes[1]);

        // Build help hints first to size the help row count. Reserve at
        // least 1 row for the detail-text Min(1) chunk.
        let mut hints: Vec<super::key_hints::Hint> = vec![
            super::key_hints::key_pair("?", " help  "),
            super::key_hints::key_pair_two("o", "/", "Esc", " back  "),
            super::key_hints::key_pair("s", " snaps  "),
            super::key_hints::key_pair("R", " restore  "),
        ];
        if can_undo {
            hints.push(super::key_hints::key_pair("Z", " undo  "));
        }
        if can_redo {
            hints.push(super::key_hints::key_pair("Y", " redo  "));
        }
        let help_lines = super::key_hints::wrap_hints(hints, detail_inner.width);
        let max_help = detail_inner.height.saturating_sub(1).max(1);
        let help_rows = (help_lines.len() as u16).clamp(1, max_help);

        let detail_chunks = Layout::vertical([
            Constraint::Min(1),            // detail text
            Constraint::Length(help_rows), // help (variable rows)
        ])
        .split(detail_inner);

        // Detail text
        if let Some(text) = &self.detail_text {
            let styled = colorize_op_show(text);
            self.detail_content_height = styled.height() as u16;
            let widget = Paragraph::new(styled)
                .wrap(Wrap { trim: true })
                .scroll((self.detail_scroll, 0));
            frame.render_widget(widget, detail_chunks[0]);
        }

        // Render pre-wrapped help lines (built before the layout split).
        frame.render_widget(Paragraph::new(Text::from(help_lines)), detail_chunks[1]);
    }
}

/// Extract "HH:MM" from a jj timestamp like "2026-04-10 00:52:43.436 -04:00".
fn extract_time(ts: &str) -> String {
    // Find the time portion after the date
    let parts: Vec<&str> = ts.split_whitespace().collect();
    if parts.len() >= 2 {
        // parts[1] is "HH:MM:SS.mmm"
        if let Some(hhmm) = parts[1].get(..5) {
            return hhmm.to_string();
        }
    }
    String::new()
}

/// Apply basic syntax coloring to op-show output.
fn colorize_op_show(text: &str) -> Text<'static> {
    let lines: Vec<Line<'static>> = text
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("+ ") {
                Line::styled(line.to_string(), Style::default().fg(Color::Green))
            } else if trimmed.starts_with("- ") {
                Line::styled(line.to_string(), Style::default().fg(Color::Red))
            } else if trimmed.ends_with(':') {
                Line::styled(line.to_string(), Style::default().bold())
            } else {
                Line::styled(line.to_string(), Style::default().dim())
            }
        })
        .collect();
    Text::from(lines)
}
