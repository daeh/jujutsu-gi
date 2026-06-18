use super::dialog_info::DiagramInfo;
use crate::text_utils;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

// Re-export from commands::types for existing TUI consumers.
pub(crate) use crate::commands::types::{SyncMode, SyncModeInfo, TargetWorkspace};

/// Height of the help box (top border + content + bottom border).
const HELP_BOX_HEIGHT: u16 = 3;
/// Maximum number of command lines shown in the preview.
const MAX_VISIBLE_CMDS: u16 = 6;

/// Truncate a change ID to 4 chars for display.
/// Assumes ASCII-only IDs (jj uses lowercase alpha change IDs).
pub(crate) fn short(id: &str) -> &str {
    &id[..id.len().min(4)]
}

/// Render a `label: value` line with an inverse-video cursor cell at
/// `cursor_pos`. Grapheme-aware: the cursor cell holds one full grapheme
/// (not one codepoint), or a space at end-of-line.
pub(crate) fn field_with_cursor<'a>(
    label: &'a str,
    value: &'a str,
    cursor_pos: usize,
    label_style: Style,
    val_style: Style,
) -> Line<'a> {
    let (before, after) = value.split_at(cursor_pos);
    let (cursor_display, after_cursor) = text_utils::peel_first_grapheme_for_cursor(after);
    Line::from(vec![
        Span::styled(label, label_style),
        Span::styled(before, val_style),
        Span::styled(
            cursor_display,
            Style::default().bg(Color::White).fg(Color::Black),
        ),
        Span::styled(after_cursor, val_style),
    ])
}

/// Layout state for dialog rendering.
///
/// Tracks position within a dialog's inner area. All y-coordinates are
/// relative to `inner.y` — `y_offset` is the number of rows consumed
/// from the top, and `max_y` is the row budget (typically `inner.height`
/// minus space reserved for the bottom section).
pub(crate) struct DialogLayout {
    pub(crate) inner: Rect,
    y_offset: u16,
    max_y: u16,
}

impl DialogLayout {
    pub(crate) fn new(inner: Rect, max_y: u16) -> Self {
        Self {
            inner,
            y_offset: 0,
            max_y,
        }
    }

    /// Current y_offset (rows consumed from top).
    pub(crate) fn y_offset(&self) -> u16 {
        self.y_offset
    }

    /// Skip n blank rows.
    pub(crate) fn skip(&mut self, n: u16) {
        self.y_offset = (self.y_offset + n).min(self.max_y);
    }

    /// Skip one row only if there's room.
    pub(crate) fn gap(&mut self) {
        if self.y_offset < self.max_y {
            self.y_offset += 1;
        }
    }

    /// Render a single line of spans, advancing y_offset.
    pub(crate) fn draw_line(&mut self, frame: &mut Frame, spans: &[Span]) {
        if self.y_offset >= self.max_y {
            return;
        }
        let line = Line::from(spans.to_vec());
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(
                self.inner.x,
                self.inner.y + self.y_offset,
                self.inner.width,
                1,
            ),
        );
        self.y_offset += 1;
    }

    /// Render the target workspace row: arrows, name, change ID.
    pub(crate) fn draw_target_row(
        &mut self,
        frame: &mut Frame,
        target: &TargetWorkspace,
        multi_target: bool,
        max_name_width: Option<usize>,
    ) {
        let arrows = if multi_target {
            "\u{2190}\u{2192}"
        } else {
            "  "
        };
        let name_display = match max_name_width {
            Some(w) => text_utils::truncate_end(&target.name, w),
            None => target.name.clone(),
        };
        self.draw_line(
            frame,
            &[
                Span::styled(format!(" {arrows}"), Style::default().dim()),
                Span::styled("  target: ", Style::default().dim()),
                Span::styled(name_display, Style::default()),
                Span::styled(
                    format!(" ({})", short(&target.change_id)),
                    Style::default().fg(Color::Yellow),
                ),
            ],
        );
    }

    /// Render a toggle line: key + description.
    pub(crate) fn draw_toggle(&mut self, frame: &mut Frame, key: &str, desc: &str, style: Style) {
        if self.y_offset >= self.max_y {
            return;
        }
        let line = Line::from(vec![
            Span::styled(format!("  {key}"), Style::default().bold()),
            Span::styled(format!("  {desc}"), style),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(
                self.inner.x,
                self.inner.y + self.y_offset,
                self.inner.width,
                1,
            ),
        );
        self.y_offset += 1;
    }

    /// Render a bordered help bar with key bindings.
    ///
    /// Spacing: 2 spaces before first key, 3-space gap between binding pairs,
    /// 2 spaces between key and label within each pair.
    pub(crate) fn draw_help_box(&mut self, frame: &mut Frame, bindings: &[HelpBinding<'_>]) {
        if self.y_offset + 2 >= self.max_y {
            return;
        }
        let mut spans: Vec<Span> = Vec::new();
        for (i, b) in bindings.iter().enumerate() {
            let key_pad = if i == 0 { "  " } else { "   " };
            spans.push(Span::styled(format!("{key_pad}{}", b.key), b.style));
            spans.push(Span::styled(
                format!("  {}", b.label),
                Style::default().dim(),
            ));
        }
        let help_area = Rect::new(
            self.inner.x,
            self.inner.y + self.y_offset,
            self.inner.width,
            HELP_BOX_HEIGHT.min(self.max_y - self.y_offset),
        );
        let help_block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(Style::default().dim());
        frame.render_widget(
            Paragraph::new(Line::from(spans)).block(help_block),
            help_area,
        );
        self.y_offset += HELP_BOX_HEIGHT;
    }
}

pub(crate) struct HelpBinding<'a> {
    pub(crate) key: &'a str,
    pub(crate) label: &'a str,
    pub(crate) style: Style,
}

/// Height needed for the bottom section: help box + commands + info diagram.
pub(crate) fn bottom_section_height(
    cmd_count: u16,
    info: Option<&DiagramInfo>,
    area_width: u16,
) -> u16 {
    let info_h = info.map_or(0u16, |i| i.height(area_width));
    HELP_BOX_HEIGHT + cmd_count.min(MAX_VISIBLE_CMDS) + info_h
}

/// Render command preview lines (with `$ ` prefix) and optional diagram below.
///
/// `y_start` is relative to `inner.y`. This renders into the bottom section
/// (below the DialogLayout's `max_y` boundary) using the full `inner` height.
pub(crate) fn draw_command_preview(
    frame: &mut Frame,
    inner: Rect,
    y_start: u16,
    cmds: &[Line<'_>],
    info: Option<&DiagramInfo>,
) {
    let max_y = inner.y + inner.height;
    let cmd_y_start = inner.y + y_start;
    let remaining = max_y.saturating_sub(cmd_y_start) as usize;
    if remaining == 0 {
        return;
    }
    for (i, cmd_line) in cmds.iter().take(remaining).enumerate() {
        let cy = cmd_y_start + i as u16;
        if cy >= max_y {
            break;
        }
        let mut spans = vec![super::cmd_spans::lit("  $ ")];
        spans.extend(cmd_line.spans.iter().cloned());
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(inner.x, cy, inner.width, 1),
        );
    }
    if let Some(info) = info {
        let info_y = cmd_y_start + cmds.len().min(remaining) as u16 + 1;
        info.draw(frame, inner, info_y);
    }
}
