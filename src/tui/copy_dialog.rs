use crate::jujutsu::Workspace;
use crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState};

pub(crate) struct CopyDialog {
    items: Vec<(String, String)>, // (label, value)
    state: ListState,
}

impl CopyDialog {
    pub(crate) fn new(ws: &Workspace) -> Self {
        let mut items = vec![
            ("Workspace".to_string(), ws.name.clone()),
            ("Change ID".to_string(), ws.change_id.clone()),
            ("Path".to_string(), {
                let p = ws.path.display().to_string();
                if p.is_empty() {
                    "(unavailable)".to_string()
                } else {
                    p
                }
            }),
        ];

        let mut first_bookmark = true;
        for (bm, bm_id) in &ws.bookmarks_at_head {
            let ann = super::bookmark_annotation(bm_id, true, &ws.change_id, &ws.revisions);
            let label = if first_bookmark {
                first_bookmark = false;
                "Bookmark".to_string()
            } else {
                String::new()
            };
            items.push((format!("{label:<10}{ann}"), bm.clone()));
        }
        for (bm, bm_id) in &ws.bookmarks_behind {
            let ann = super::bookmark_annotation(bm_id, false, &ws.change_id, &ws.revisions);
            let label = if first_bookmark {
                first_bookmark = false;
                "Bookmark".to_string()
            } else {
                String::new()
            };
            items.push((format!("{label:<10}{ann}"), bm.clone()));
        }

        let mut state = ListState::default();
        state.select(Some(0));
        Self { items, state }
    }

    pub(crate) fn selected_value(&self) -> Option<&str> {
        self.state
            .selected()
            .and_then(|i| self.items.get(i))
            .map(|(_, v)| v.as_str())
    }

    pub(crate) fn handle_key(&mut self, key: KeyCode) {
        let len = self.items.len();
        if len == 0 {
            return;
        }
        let current = self.state.selected().unwrap_or(0);
        match key {
            KeyCode::Up | KeyCode::Char('k') => {
                let next = if current == 0 { len - 1 } else { current - 1 };
                self.state.select(Some(next));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let next = if current >= len - 1 { 0 } else { current + 1 };
                self.state.select(Some(next));
            }
            _ => {}
        }
    }

    pub(crate) fn draw(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        let block = Block::bordered()
            .title(" Copy ")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

        let list_items: Vec<ListItem> = self
            .items
            .iter()
            .map(|(label, value)| {
                let line = Line::from(vec![
                    Span::styled(format!("{label:<20}"), Style::default().bold()),
                    Span::styled(value, Style::default().dim()),
                ]);
                ListItem::new(line)
            })
            .collect();

        let list = List::new(list_items).highlight_style(Style::default().bg(Color::DarkGray));
        frame.render_stateful_widget(list, chunks[0], &mut self.state.clone());

        let help = Line::from(vec![
            Span::styled("Enter", Style::default().bold()),
            Span::styled(" copy  ", Style::default().dim()),
            Span::styled("Esc", Style::default().bold()),
            Span::styled(" cancel", Style::default().dim()),
        ]);
        frame.render_widget(help, chunks[1]);
    }
}

pub(crate) fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}
