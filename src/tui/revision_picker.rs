use crate::jujutsu::RevisionInfo;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

pub(crate) struct RevisionPicker {
    workspace_name: String,
    revisions: Vec<RevisionInfo>,
    selected: usize,
    scroll_offset: usize,
}

impl RevisionPicker {
    pub(crate) fn new(workspace_name: String, revisions: Vec<RevisionInfo>) -> Self {
        Self {
            workspace_name,
            revisions,
            selected: 0,
            scroll_offset: 0,
        }
    }

    pub(crate) fn up(&mut self) {
        if !self.revisions.is_empty() {
            self.selected = if self.selected == 0 {
                self.revisions.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    pub(crate) fn down(&mut self) {
        if !self.revisions.is_empty() {
            self.selected = if self.selected >= self.revisions.len() - 1 {
                0
            } else {
                self.selected + 1
            };
        }
    }

    pub(crate) fn selected_revision(&self) -> Option<&RevisionInfo> {
        self.revisions.get(self.selected)
    }

    pub(crate) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let rev_lines = self.revisions.len().min(12);
        let height = (rev_lines as u16 + 6).min(area.height.saturating_sub(2));
        let width = 60.min(area.width.saturating_sub(4));
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let title = format!(" Split: {} ", self.workspace_name);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().dim())
            .title(title);
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        if inner.height < 3 || inner.width < 20 {
            return;
        }

        let max_revs = (inner.height as usize).saturating_sub(3);

        // Adjust scroll to keep selection visible
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + max_revs {
            self.scroll_offset = self.selected + 1 - max_revs;
        }

        let visible = self
            .revisions
            .iter()
            .enumerate()
            .skip(self.scroll_offset)
            .take(max_revs);
        let mut y_offset = 0u16;

        for (i, rev) in visible {
            let is_selected = i == self.selected;
            let is_head = i == 0;
            let pointer = if is_selected { ">" } else { " " };
            let marker = if is_head { "@" } else { " " };

            let pointer_style = if is_selected {
                Style::default().fg(Color::Cyan).bold()
            } else {
                Style::default()
            };
            let id_style = if is_selected {
                Style::default().fg(Color::Yellow).bold()
            } else {
                Style::default().fg(Color::Yellow)
            };
            let desc_style = if is_selected {
                Style::default()
            } else {
                Style::default().dim()
            };

            let line = Line::from(vec![
                Span::styled(format!(" {pointer}"), pointer_style),
                Span::styled(marker.to_string(), Style::default().fg(Color::Green)),
                Span::styled(format!(" {} ", rev.change_id), id_style),
                Span::styled(&rev.description, desc_style),
            ]);
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, inner.y + y_offset, inner.width, 1),
            );
            y_offset += 1;
        }

        let hidden_below = self
            .revisions
            .len()
            .saturating_sub(self.scroll_offset + max_revs);
        if hidden_below > 0 {
            let more = format!("  ... and {hidden_below} more below");
            frame.render_widget(
                Paragraph::new(Span::styled(more, Style::default().dim())),
                Rect::new(inner.x, inner.y + y_offset, inner.width, 1),
            );
            y_offset += 1;
        }

        // Help line
        if y_offset + 1 < inner.height {
            y_offset = inner.height - 1;
        }
        let help = Line::from(vec![
            Span::styled("  Enter", Style::default().bold()),
            Span::styled(" split  ", Style::default().dim()),
            Span::styled("Esc", Style::default().bold()),
            Span::styled(" cancel", Style::default().dim()),
        ]);
        frame.render_widget(
            Paragraph::new(help),
            Rect::new(inner.x, inner.y + y_offset, inner.width, 1),
        );
    }
}
