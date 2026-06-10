use crate::jujutsu::{DiffSummary, FileChangeKind, Workspace};
use crate::text_utils;
use crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Padding, Paragraph, Wrap};
use std::collections::{HashMap, HashSet};

const GUTTER_W: usize = 2;
const NAME_W: usize = 20;
const ID_W: usize = 10;
const BOOKMARK_W: usize = 20;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum SortOrder {
    LogOrder,
    Alphabetical,
    LastModified,
}

pub(crate) struct WorkspaceList {
    workspaces: Vec<Workspace>,
    state: ListState,
    sort_order: SortOrder,
    stale_names: HashSet<String>,
    /// Graph position of each workspace's change_id (first occurrence in line_heads).
    graph_positions: HashMap<String, usize>,
    /// Vertical scroll offset for the description/files panel.
    desc_scroll: u16,
    /// Cached rect of the description panel (set during draw).
    desc_area: Rect,
    /// Total content height in wrapped lines (set during draw).
    desc_content_height: u16,
}

impl WorkspaceList {
    pub(crate) fn new(
        workspaces: Vec<Workspace>,
        selected_name: Option<&str>,
        sort_order: SortOrder,
        line_heads: &[Option<String>],
    ) -> Self {
        let mut graph_positions: HashMap<String, usize> = HashMap::new();
        for (i, head) in line_heads.iter().enumerate() {
            if let Some(id) = head {
                graph_positions.entry(id.clone()).or_insert(i);
            }
        }
        let mut s = Self {
            workspaces,
            state: ListState::default(),
            sort_order,
            stale_names: HashSet::new(),
            graph_positions,
            desc_scroll: 0,
            desc_area: Rect::default(),
            desc_content_height: 0,
        };
        s.sort_workspaces();
        let idx = selected_name
            .and_then(|name| s.workspaces.iter().position(|w| w.name == name))
            .unwrap_or(0);
        s.state.select(Some(idx));
        s
    }

    pub(crate) fn selected_workspace(&self) -> Option<&Workspace> {
        self.state.selected().and_then(|i| self.workspaces.get(i))
    }

    pub(crate) fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    pub(crate) fn sort_order(&self) -> SortOrder {
        self.sort_order
    }

    pub(crate) fn toggle_sort(&mut self) {
        let selected_name = self.selected_workspace().map(|ws| ws.name.clone());
        self.sort_order = match self.sort_order {
            SortOrder::LogOrder => SortOrder::Alphabetical,
            SortOrder::Alphabetical => SortOrder::LastModified,
            SortOrder::LastModified => SortOrder::LogOrder,
        };
        self.sort_workspaces();
        let idx = selected_name
            .and_then(|name| self.workspaces.iter().position(|w| w.name == name))
            .unwrap_or(0);
        self.state.select(Some(idx));
    }

    fn sort_workspaces(&mut self) {
        let fallback = self.graph_positions.values().max().unwrap_or(&0) + 1;
        self.workspaces.sort_by(|a, b| {
            // "default" always first, regardless of sort mode
            match (a.name == "default", b.name == "default") {
                (true, false) => return std::cmp::Ordering::Less,
                (false, true) => return std::cmp::Ordering::Greater,
                _ => {}
            }
            match self.sort_order {
                SortOrder::LogOrder => {
                    let key_a = a.change_id.as_str();
                    let key_b = b.change_id.as_str();
                    let pos_a = self.graph_positions.get(key_a).unwrap_or(&fallback);
                    let pos_b = self.graph_positions.get(key_b).unwrap_or(&fallback);
                    pos_a.cmp(pos_b)
                }
                SortOrder::Alphabetical => a.name.cmp(&b.name),
                SortOrder::LastModified => b
                    .last_modified
                    .unwrap_or(0)
                    .cmp(&a.last_modified.unwrap_or(0)),
            }
        });
    }

    pub(crate) fn set_stale_names(&mut self, names: Vec<String>) {
        self.stale_names = names.into_iter().collect();
    }

    pub(crate) fn is_stale(&self, name: &str) -> bool {
        self.stale_names.contains(name)
    }

    pub(crate) fn select_index(&mut self, index: usize) {
        if index < self.workspaces.len() {
            self.state.select(Some(index));
        }
    }

    pub(crate) fn select_by_name(&mut self, name: &str) {
        if let Some(idx) = self.workspaces.iter().position(|w| w.name == name) {
            self.state.select(Some(idx));
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyCode) {
        let len = self.workspaces.len();
        if len == 0 {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        match key {
            KeyCode::Up => {
                let next = if current == 0 { len - 1 } else { current - 1 };
                self.state.select(Some(next));
            }
            KeyCode::Down => {
                let next = if current >= len - 1 { 0 } else { current + 1 };
                self.state.select(Some(next));
            }
            _ => {}
        }
    }

    pub(crate) fn desc_area(&self) -> Rect {
        self.desc_area
    }

    pub(crate) fn scroll_desc_up(&mut self) {
        self.desc_scroll = self.desc_scroll.saturating_sub(1);
    }

    pub(crate) fn scroll_desc_down(&mut self) {
        let visible = self.desc_area.height;
        let max = self.desc_content_height.saturating_sub(visible);
        if self.desc_scroll < max {
            self.desc_scroll += 1;
        }
    }

    pub(crate) fn reset_desc_scroll(&mut self) {
        self.desc_scroll = 0;
    }

    /// Build styled text for the file changes display.
    fn build_files_text(summary: &DiffSummary) -> Text<'static> {
        if summary.files.is_empty() {
            return Text::styled("(no changes)", Style::default().dim());
        }

        let mut lines: Vec<Line<'static>> = summary
            .files
            .iter()
            .map(|fc| {
                let (prefix, color) = match fc.kind {
                    FileChangeKind::Added => ("A ", Color::Green),
                    FileChangeKind::Modified => ("M ", Color::Yellow),
                    FileChangeKind::Deleted => ("D ", Color::Red),
                    FileChangeKind::Renamed => ("R ", Color::Cyan),
                };
                Line::from(vec![
                    Span::styled(prefix, Style::default().fg(color).bold()),
                    Span::styled(fc.path.clone(), Style::default().fg(color)),
                ])
            })
            .collect();

        if let Some(stat) = &summary.stat_line {
            lines.push(Line::from(Span::styled(
                stat.clone(),
                Style::default().dim(),
            )));
        }

        Text::from(lines)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
        selected_stale: bool,
        has_status: bool,
        description_override: Option<&str>,
        diff_summary: Option<&DiffSummary>,
        show_files: bool,
        can_undo: bool,
    ) {
        let border_style = if focused {
            Style::default().fg(Color::White).bold()
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let block = Block::bordered()
            .title(" Workspaces ")
            .border_type(BorderType::Rounded)
            .border_style(border_style);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let desc_max = area.height / 4; // 25% of total pane height

        // Build help hints first so we know how many rows to allocate for the
        // help bar. Wrapping happens at `inner.width` (the full content width;
        // chunks below don't add horizontal margin). The clamp keeps help
        // from running away on very narrow / very short panes — at extreme
        // shortness ratatui's solver will shrink the list below Min(1).
        let mut hints: Vec<super::key_hints::Hint> = vec![
            super::key_hints::key_pair("?", " help  "),
            super::key_hints::key_pair("q", " exit  "),
            super::key_hints::key_pair("Enter", " switch  "),
            super::key_hints::key_pair("n", " new  "),
            super::key_hints::key_pair("s", " sync  "),
            super::key_hints::key_pair("x", " close  "),
            super::key_hints::key_pair("t", " transfer  "),
            super::key_hints::key_pair("b", " bookmarks  "),
            super::key_hints::key_pair("c", " copy  "),
            super::key_hints::key_pair("i", if show_files { " msg  " } else { " files  " }),
            super::key_hints::key_pair("o", " op-log  "),
        ];
        if selected_stale {
            hints.push(super::key_hints::key_pair("u", " update  "));
        }
        if can_undo {
            hints.push(super::key_hints::key_pair("Z", " undo  "));
        }
        if has_status {
            hints.push(super::key_hints::marker(
                "STATUS ",
                Style::default().fg(Color::Red).bold(),
            ));
            hints.push(super::key_hints::key_pair("p", " copy  "));
            hints.push(super::key_hints::key_pair("P", " clear  "));
        }
        let help_lines = super::key_hints::wrap_hints(hints, inner.width);
        let max_help = inner.height.saturating_sub(5).max(1);
        let help_rows = (help_lines.len() as u16).clamp(1, max_help);

        let chunks = Layout::vertical([
            Constraint::Length(1),         // header
            Constraint::Min(1),            // list
            Constraint::Max(desc_max),     // description
            Constraint::Length(3),         // path (1 line + border)
            Constraint::Length(help_rows), // help (variable rows)
        ])
        .split(inner);

        // Header
        let header = Line::from(vec![
            Span::styled(format!("{:<GUTTER_W$}", ""), Style::default()),
            Span::styled(format!("{:<NAME_W$}", "Workspace"), Style::default().bold()),
            Span::styled(format!("{:<ID_W$}", "Change"), Style::default().bold()),
            Span::styled(
                format!("{:<BOOKMARK_W$}", "Bookmark"),
                Style::default().bold(),
            ),
            Span::styled("Description", Style::default().bold()),
        ]);
        frame.render_widget(header, chunks[0]);

        // Workspace rows
        let items: Vec<ListItem> = self
            .workspaces
            .iter()
            .map(|ws| {
                let short_id = if ws.change_id.len() > 8 {
                    &ws.change_id[..8]
                } else {
                    &ws.change_id
                };
                let gutter = if ws.is_current { "@ " } else { "+ " };
                let gutter_style = if ws.is_current {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().dim()
                };
                let orphaned = ws.path.as_os_str().is_empty();
                let stale = self.is_stale(&ws.name);
                let name_style = if orphaned {
                    Style::default().fg(Color::Blue)
                } else if stale {
                    Style::default().fg(Color::Red)
                } else if ws.is_current {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default()
                };

                // Build bookmark display: head bookmarks shown normally, behind in parens
                let mut bm_parts: Vec<String> = Vec::new();
                for (bm, _) in &ws.bookmarks_at_head {
                    bm_parts.push(bm.clone());
                }
                for (bm, _) in &ws.bookmarks_behind {
                    bm_parts.push(format!("({bm})"));
                }
                let bookmark_display = bm_parts.join(", ");
                let bookmark_display = text_utils::truncate_end(&bookmark_display, BOOKMARK_W - 1);

                let first_line = ws.description.lines().next().unwrap_or("");
                let desc = text_utils::truncate_end(first_line, 50);

                let name_display = text_utils::truncate_end(&ws.name, NAME_W - 1);

                let id_style = if stale {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::Yellow)
                };

                let line = Line::from(vec![
                    Span::styled(gutter, gutter_style),
                    Span::styled(format!("{name_display:<NAME_W$}"), name_style),
                    Span::styled(format!("{short_id:<ID_W$}"), id_style),
                    Span::styled(
                        format!("{bookmark_display:<BOOKMARK_W$}"),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(desc, Style::default().dim()),
                ]);
                ListItem::new(line)
            })
            .collect();

        let list = List::new(items).highlight_style(Style::default().bg(Color::DarkGray));

        frame.render_stateful_widget(list, chunks[1], &mut self.state);

        // Description of selected workspace (or overridden by revision cursor / files)
        self.desc_area = chunks[2];
        if self.selected_workspace().is_some() {
            let desc_block = Block::default()
                .padding(Padding::horizontal(1))
                .style(Style::default().bg(Color::Indexed(236)));
            let inner_width = desc_block.inner(chunks[2]).width;

            if let Some(summary) = diff_summary {
                // Colored file list with stat summary
                let text = Self::build_files_text(summary);
                self.desc_content_height = text.height() as u16;
                let widget = Paragraph::new(text)
                    .block(desc_block)
                    .wrap(Wrap { trim: true })
                    .scroll((self.desc_scroll, 0));
                frame.render_widget(widget, chunks[2]);
            } else {
                let ws = self.selected_workspace().unwrap();
                let desc_text = if let Some(overr) = description_override {
                    if overr.is_empty() {
                        "(no description)".to_string()
                    } else {
                        overr.to_string()
                    }
                } else if ws.description.is_empty() {
                    "(no description)".to_string()
                } else {
                    ws.description.clone()
                };
                self.desc_content_height = wrapped_line_count(&desc_text, inner_width);
                let widget = Paragraph::new(desc_text)
                    .style(Style::default().dim())
                    .block(desc_block)
                    .wrap(Wrap { trim: true })
                    .scroll((self.desc_scroll, 0));
                frame.render_widget(widget, chunks[2]);
            }
        }

        // Path of selected workspace (right-truncated: show the tail)
        if let Some(ws) = self.selected_workspace() {
            let path_raw = ws.path.display().to_string();
            let path_str = if path_raw.is_empty() {
                "(unavailable)".to_string()
            } else {
                path_raw
            };
            let path_block = Block::bordered()
                .border_style(Style::default().fg(Color::DarkGray))
                .border_type(BorderType::Rounded);
            let inner_w = chunks[3].width.saturating_sub(2) as usize; // minus borders
            let display = text_utils::truncate_start(&path_str, inner_w);
            let path = Paragraph::new(display)
                .style(Style::default().dim())
                .block(path_block);
            frame.render_widget(path, chunks[3]);
        }

        // Render pre-wrapped help lines (built before the layout split).
        frame.render_widget(Paragraph::new(Text::from(help_lines)), chunks[4]);
    }
}

/// Estimate the number of rendered lines after word-wrapping a string at `width`.
fn wrapped_line_count(text: &str, width: u16) -> u16 {
    use unicode_width::UnicodeWidthStr;
    if width == 0 {
        return text.lines().count().max(1) as u16;
    }
    let w = width as usize;
    let count: usize = text
        .lines()
        .map(|line| {
            let lw = UnicodeWidthStr::width(line);
            if lw == 0 { 1 } else { lw.div_ceil(w) }
        })
        .sum();
    // An empty string has no lines() but still occupies space
    count.max(1) as u16
}
