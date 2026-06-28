use std::collections::HashSet;

use ansi_to_tui::IntoText;
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};

pub(crate) struct GraphPane {
    original_text: Text<'static>,
    text: Text<'static>,
    scroll: u16,
    line_count: u16,
    /// Per-line change_id map (parallel to text lines). None for graph-only lines.
    line_heads: Vec<Option<String>>,
    /// Custom border title override (e.g. " Log @ <op_id> ").
    title_override: Option<String>,
}

impl GraphPane {
    pub(crate) fn new(ansi_output: &str, line_heads: Vec<Option<String>>) -> Self {
        let text = ansi_output
            .into_text()
            .unwrap_or_else(|_| Text::from("failed to parse graph"));
        let line_count = text.lines.len() as u16;
        Self {
            original_text: text.clone(),
            text,
            scroll: 0,
            line_count,
            line_heads,
            title_override: None,
        }
    }

    /// Scroll the graph to center the first line matching the given change_id.
    pub(crate) fn scroll_to_change_id(&mut self, change_id: &str, visible_height: u16) {
        if let Some(line) = self
            .line_heads
            .iter()
            .position(|h| h.as_deref().is_some_and(|id| change_id == id))
        {
            let line = line as u16;
            let half = visible_height / 2;
            let max = self.line_count.saturating_sub(visible_height);
            self.scroll = line.saturating_sub(half).min(max);
        }
    }

    /// Apply dim styling based on change_id membership.
    ///
    /// When `dim_matched` is false, dims lines whose change_id is not in `ids`.
    /// When `dim_matched` is true, dims lines whose change_id is in `ids`.
    /// Lines with no change_id (graph connectors) are left undimmed.
    pub(crate) fn highlight(&mut self, ids: &HashSet<&str>, dim_matched: bool) {
        let mut text = self.original_text.clone();
        for (i, line) in text.lines.iter_mut().enumerate() {
            let dim = self
                .line_heads
                .get(i)
                .and_then(|h| h.as_deref())
                .is_some_and(|id| ids.contains(id) == dim_matched);
            if dim {
                for span in &mut line.spans {
                    span.style = span.style.add_modifier(Modifier::DIM);
                }
            }
        }
        self.text = text;
    }

    /// Return the change_id at the given visual row (0-based, relative to inner content area),
    /// accounting for the current scroll offset. Returns `None` for graph-connector-only lines.
    pub(crate) fn change_id_at_row(&self, visual_row: u16) -> Option<&str> {
        let line = self.scroll as usize + visual_row as usize;
        self.line_heads.get(line)?.as_deref()
    }

    pub(crate) fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    pub(crate) fn scroll_down(&mut self, visible_height: u16) {
        let max = self.line_count.saturating_sub(visible_height);
        if self.scroll < max {
            self.scroll += 1;
        }
    }

    pub(crate) fn scroll_up_half(&mut self, visible_height: u16) {
        self.scroll = self.scroll.saturating_sub(visible_height / 2);
    }

    pub(crate) fn scroll_down_half(&mut self, visible_height: u16) {
        let max = self.line_count.saturating_sub(visible_height);
        self.scroll = (self.scroll + visible_height / 2).min(max);
    }

    pub(crate) fn set_title(&mut self, title: Option<String>) {
        self.title_override = title;
    }

    pub(crate) fn draw(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let border_style = if focused {
            Style::default().fg(Color::White).bold()
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let title = self.title_override.as_deref().unwrap_or(" Log ");
        let block = Block::bordered()
            .title(title)
            .border_type(BorderType::Rounded)
            .border_style(border_style);
        let inner = block.inner(area);

        let paragraph = Paragraph::new(self.text.clone())
            .block(block)
            .scroll((self.scroll, 0));
        frame.render_widget(paragraph, area);

        // Scrollbar
        if self.line_count > inner.height {
            let mut scrollbar_state = ScrollbarState::default()
                .content_length(self.line_count as usize)
                .position(self.scroll as usize);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
            frame.render_stateful_widget(scrollbar, area, &mut scrollbar_state);
        }
    }
}
