use crate::hooks::{self, HookVars};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Bookmark,
    Revision,
    Path,
    Msg,
}

const FIELDS: [Field; 4] = [Field::Bookmark, Field::Revision, Field::Path, Field::Msg];
const LABEL_WIDTH: u16 = 12;

pub(crate) struct CreateDialogValues {
    pub(crate) bookmark: String,
    pub(crate) revision: String,
    pub(crate) path: String,
    pub(crate) msg: String,
}

pub(crate) struct CreateDialog {
    bookmark: String,
    revision: String,
    path: String,
    msg: String,
    active_field: Field,
    cursor_pos: usize,
    /// Selection anchor (byte offset). Selection spans from anchor to cursor_pos.
    selection_anchor: Option<usize>,
    default_path_template: String,
    repo_name: String,
    home: String,
    default_workspace_path: String,
    /// Change IDs available for cycling (newest-first).
    revision_choices: Vec<String>,
    /// Current index into `revision_choices` when cycling; None for free-text mode.
    revision_index: Option<usize>,
}

impl CreateDialog {
    pub(crate) fn new() -> Self {
        Self {
            bookmark: String::new(),
            revision: "@".to_string(),
            path: String::new(),
            msg: String::new(),
            active_field: Field::Bookmark,
            cursor_pos: 0,
            selection_anchor: None,
            default_path_template: String::new(),
            repo_name: String::new(),
            home: String::new(),
            default_workspace_path: String::new(),
            revision_choices: Vec::new(),
            revision_index: None,
        }
    }

    pub(crate) fn reset(
        &mut self,
        path_template: &str,
        repo_name: &str,
        revision: &str,
        home: &str,
        default_workspace_path: &str,
    ) {
        self.bookmark.clear();
        self.revision = revision.to_string();
        self.path.clear();
        self.msg.clear();
        self.active_field = Field::Bookmark;
        self.cursor_pos = 0;
        self.selection_anchor = None;
        self.default_path_template = path_template.to_string();
        self.repo_name = repo_name.to_string();
        self.home = home.to_string();
        self.default_workspace_path = default_workspace_path.to_string();
        self.revision_choices.clear();
        self.revision_index = None;
    }

    /// Whether the Path field is using the template default (user hasn't typed a custom path).
    pub(crate) fn path_is_default(&self) -> bool {
        self.path.is_empty()
    }

    pub(crate) fn values(&self) -> CreateDialogValues {
        let path = if self.path.is_empty() {
            self.computed_default_path()
        } else {
            self.path.clone()
        };
        CreateDialogValues {
            bookmark: self.bookmark.clone(),
            revision: self.revision.clone(),
            path,
            msg: self.msg.clone(),
        }
    }

    /// Set the revision choices available for left/right cycling.
    pub(crate) fn set_revision_choices(&mut self, choices: Vec<String>) {
        self.revision_index = choices.iter().position(|id| id == &self.revision);
        self.revision_choices = choices;
    }

    /// Whether the Revision field is currently active.
    pub(crate) fn active_is_revision(&self) -> bool {
        self.active_field == Field::Revision
    }

    /// Cycle through revision choices by `delta` (+1 = older, -1 = newer).
    /// Returns the new change_id if cycling occurred, None if no choices available.
    pub(crate) fn cycle_revision(&mut self, delta: isize) -> Option<&str> {
        if self.revision_choices.is_empty() {
            return None;
        }
        let len = self.revision_choices.len();
        let current = self.revision_index.unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(len as isize) as usize;
        self.revision_index = Some(next);
        self.revision = self.revision_choices[next].clone();
        self.cursor_pos = self.revision.len();
        self.clear_selection();
        Some(&self.revision)
    }

    /// Exit revision cycling mode (called when the user edits the field manually).
    fn exit_revision_cycle(&mut self) {
        if self.active_field == Field::Revision {
            self.revision_index = None;
        }
    }

    fn computed_default_path(&self) -> String {
        if self.bookmark.is_empty() {
            return String::new();
        }
        let sanitized = self.bookmark.replace('/', "-");
        let mut vars = HookVars::new();
        vars.insert("home".into(), self.home.clone());
        vars.insert("repo".into(), self.repo_name.clone());
        vars.insert("bookmark".into(), sanitized);
        vars.insert(
            "default_workspace_path".into(),
            self.default_workspace_path.clone(),
        );
        hooks::expand(&self.default_path_template, &vars)
    }

    // -- Selection helpers --

    fn selection_range(&self) -> Option<(usize, usize)> {
        self.selection_anchor.map(|anchor| {
            if anchor <= self.cursor_pos {
                (anchor, self.cursor_pos)
            } else {
                (self.cursor_pos, anchor)
            }
        })
    }

    fn delete_selection(&mut self) -> bool {
        if let Some((start, end)) = self.selection_range() {
            self.active_mut().drain(start..end);
            self.cursor_pos = start;
            self.selection_anchor = None;
            true
        } else {
            false
        }
    }

    fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    // -- Field navigation --

    pub(crate) fn next_field(&mut self) {
        let idx = FIELDS
            .iter()
            .position(|f| *f == self.active_field)
            .unwrap_or(0);
        self.active_field = FIELDS[(idx + 1) % FIELDS.len()];
        self.cursor_pos = self.active_ref().len();
        self.clear_selection();
    }

    pub(crate) fn prev_field(&mut self) {
        let idx = FIELDS
            .iter()
            .position(|f| *f == self.active_field)
            .unwrap_or(0);
        self.active_field = FIELDS[(idx + FIELDS.len() - 1) % FIELDS.len()];
        self.cursor_pos = self.active_ref().len();
        self.clear_selection();
    }

    // -- Editing --

    pub(crate) fn insert_char(&mut self, c: char) {
        if self.active_field == Field::Bookmark && c == ' ' {
            return;
        }
        self.exit_revision_cycle();
        self.delete_selection();
        let pos = self.cursor_pos;
        self.active_mut().insert(pos, c);
        self.cursor_pos += c.len_utf8();
    }

    pub(crate) fn delete_char(&mut self) {
        self.exit_revision_cycle();
        if self.delete_selection() {
            return;
        }
        if self.cursor_pos > 0 {
            let pos = self.cursor_pos;
            // Grapheme-aware backspace: deletes one user-perceived character
            // even when it is composed of multiple Unicode codepoints (e.g.
            // a regional-indicator pair `🇯🇵`).
            let prev = crate::text_utils::prev_grapheme_boundary(self.active_ref(), pos);
            self.active_mut().drain(prev..pos);
            self.cursor_pos = prev;
        }
    }

    pub(crate) fn delete_to_start(&mut self) {
        self.exit_revision_cycle();
        if self.delete_selection() {
            return;
        }
        let pos = self.cursor_pos;
        if pos > 0 {
            self.active_mut().drain(..pos);
            self.cursor_pos = 0;
        }
    }

    pub(crate) fn delete_to_end(&mut self) {
        self.exit_revision_cycle();
        if self.delete_selection() {
            return;
        }
        let pos = self.cursor_pos;
        self.active_mut().truncate(pos);
    }

    pub(crate) fn delete_word_backward(&mut self) {
        self.exit_revision_cycle();
        if self.delete_selection() {
            return;
        }
        let pos = self.cursor_pos;
        if pos == 0 {
            return;
        }
        // Grapheme-safe word boundary: never splits multi-codepoint graphemes
        // (e.g. 🇯🇵) but preserves the existing ASCII-space-delimited semantics.
        let new_pos = crate::text_utils::prev_word_boundary(self.active_ref(), pos);
        self.active_mut().drain(new_pos..pos);
        self.cursor_pos = new_pos;
    }

    // -- Cursor movement --

    pub(crate) fn move_left(&mut self) {
        self.clear_selection();
        if self.cursor_pos > 0 {
            // Grapheme-aware so arrow-left over `🇯🇵` moves by one user-
            // perceived character rather than mid-grapheme.
            self.cursor_pos =
                crate::text_utils::prev_grapheme_boundary(self.active_ref(), self.cursor_pos);
        }
    }

    pub(crate) fn move_right(&mut self) {
        self.clear_selection();
        let field = self.active_ref();
        if self.cursor_pos < field.len() {
            self.cursor_pos = crate::text_utils::next_grapheme_boundary(field, self.cursor_pos);
        }
    }

    pub(crate) fn move_word_backward(&mut self) {
        self.clear_selection();
        self.cursor_pos = crate::text_utils::prev_word_boundary(self.active_ref(), self.cursor_pos);
    }

    pub(crate) fn move_word_forward(&mut self) {
        self.clear_selection();
        self.cursor_pos = crate::text_utils::next_word_boundary(self.active_ref(), self.cursor_pos);
    }

    pub(crate) fn move_home(&mut self) {
        self.clear_selection();
        self.cursor_pos = 0;
    }

    pub(crate) fn move_end(&mut self) {
        self.clear_selection();
        self.cursor_pos = self.active_ref().len();
    }

    // -- Mouse --

    /// Returns (field_index, byte_offset) if the position hits a field value area.
    pub(crate) fn hit_test(&self, area: Rect, col: u16, row: u16) -> Option<(usize, usize)> {
        let (inner, _) = self.layout(area);
        let value_x_start = inner.x + LABEL_WIDTH;

        for (i, field) in FIELDS.iter().enumerate() {
            let field_y = inner.y + 1 + (i as u16 * 2);
            if row == field_y && col >= value_x_start {
                let screen_col = (col - value_x_start) as usize;
                let field_val = match field {
                    Field::Bookmark => &self.bookmark,
                    Field::Revision => &self.revision,
                    Field::Path => &self.path,
                    Field::Msg => &self.msg,
                };
                return Some((
                    i,
                    crate::text_utils::col_to_byte_offset(field_val, screen_col),
                ));
            }
        }
        None
    }

    pub(crate) fn click_at(&mut self, field_index: usize, byte_offset: usize) {
        if field_index < FIELDS.len() {
            self.active_field = FIELDS[field_index];
            self.cursor_pos = byte_offset.min(self.active_ref().len());
            self.selection_anchor = None;
        }
    }

    pub(crate) fn drag_to(&mut self, field_index: usize, byte_offset: usize) {
        if field_index < FIELDS.len() && FIELDS[field_index] == self.active_field {
            let clamped = byte_offset.min(self.active_ref().len());
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor_pos);
            }
            self.cursor_pos = clamped;
        }
    }

    // -- Layout --

    fn active_ref(&self) -> &str {
        match self.active_field {
            Field::Bookmark => &self.bookmark,
            Field::Revision => &self.revision,
            Field::Path => &self.path,
            Field::Msg => &self.msg,
        }
    }

    fn active_mut(&mut self) -> &mut String {
        match self.active_field {
            Field::Bookmark => &mut self.bookmark,
            Field::Revision => &mut self.revision,
            Field::Path => &mut self.path,
            Field::Msg => &mut self.msg,
        }
    }

    /// Compute dialog inner area. Used by both `draw()` and `hit_test()`.
    fn layout(&self, area: Rect) -> (Rect, Rect) {
        let width = 64.min(area.width.saturating_sub(4));
        let height = 14.min(area.height.saturating_sub(2));
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);
        let inner = Rect::new(
            dialog_area.x + 1,
            dialog_area.y + 1,
            dialog_area.width.saturating_sub(2),
            dialog_area.height.saturating_sub(2),
        );
        (inner, dialog_area)
    }

    // -- Rendering --

    pub(crate) fn draw(&self, frame: &mut Frame, area: Rect) {
        let (inner, dialog_area) = self.layout(area);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().dim())
            .title(" Create Workspace ");
        frame.render_widget(block, dialog_area);

        if inner.height < 8 || inner.width < 20 {
            return;
        }

        let label_style = Style::default().dim();
        let default_path = self.computed_default_path();
        let sel_style = Style::default().bg(Color::DarkGray).fg(Color::White);

        let fields: [(Field, &str, &str, Option<&str>); 4] = [
            (Field::Bookmark, "  Bookmark: ", &self.bookmark, None),
            (Field::Revision, "  Revision: ", &self.revision, None),
            (Field::Path, "  Path:     ", &self.path, Some(&default_path)),
            (Field::Msg, "  Message:  ", &self.msg, None),
        ];

        for (i, (field, label, value, placeholder)) in fields.iter().enumerate() {
            let active = self.active_field == *field;
            let val_style = if active {
                Style::default()
            } else {
                Style::default().dim()
            };

            let is_placeholder = value.is_empty() && placeholder.is_some();
            let show_style = if is_placeholder && !active {
                Style::default().dim().italic()
            } else {
                val_style
            };

            let line = if active && !is_placeholder {
                if let Some((sel_start, sel_end)) = self.selection_range() {
                    // Render with selection highlight
                    let before_sel = &value[..sel_start];
                    let selected = &value[sel_start..sel_end];
                    let after_sel = &value[sel_end..];

                    // Place cursor block within or after selection. Each
                    // branch peels one grapheme (not one char) for the
                    // inverse-video cursor cell so multi-codepoint graphemes
                    // (e.g. 🇯🇵) display as a single cell.
                    if self.cursor_pos >= sel_end {
                        // Cursor is at or after selection end
                        let (cursor_display, after_cursor) =
                            crate::text_utils::peel_first_grapheme_for_cursor(after_sel);
                        Line::from(vec![
                            Span::styled(*label, label_style),
                            Span::styled(before_sel, val_style),
                            Span::styled(selected, sel_style),
                            Span::styled(
                                cursor_display,
                                Style::default().bg(Color::White).fg(Color::Black),
                            ),
                            Span::styled(after_cursor, val_style),
                        ])
                    } else {
                        // Cursor is at selection start (anchor is after cursor)
                        // Show first grapheme of selection as cursor block.
                        let (cursor_display, before_rest) =
                            crate::text_utils::peel_first_grapheme_for_cursor(selected);
                        Line::from(vec![
                            Span::styled(*label, label_style),
                            Span::styled(before_sel, val_style),
                            Span::styled(
                                cursor_display,
                                Style::default().bg(Color::White).fg(Color::Black),
                            ),
                            Span::styled(before_rest, sel_style),
                            Span::styled(after_sel, val_style),
                        ])
                    }
                } else {
                    // No selection — just cursor.
                    let (before, after) = value.split_at(self.cursor_pos);
                    let (cursor_display, after_cursor) =
                        crate::text_utils::peel_first_grapheme_for_cursor(after);
                    Line::from(vec![
                        Span::styled(*label, label_style),
                        Span::styled(before, val_style),
                        Span::styled(
                            cursor_display,
                            Style::default().bg(Color::White).fg(Color::Black),
                        ),
                        Span::styled(after_cursor, val_style),
                    ])
                }
            } else {
                let display_value = if value.is_empty() {
                    placeholder.unwrap_or("")
                } else {
                    value
                };
                Line::from(vec![
                    Span::styled(*label, label_style),
                    Span::styled(display_value, show_style),
                ])
            };
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, inner.y + 1 + (i as u16 * 2), inner.width, 1),
            );
        }

        // Help line
        let help = Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled("Tab", Style::default().bold()),
            Span::styled(" switch  ", Style::default().dim()),
            Span::styled("Enter", Style::default().bold()),
            Span::styled(" create  ", Style::default().dim()),
            Span::styled("Esc", Style::default().bold()),
            Span::styled(" cancel", Style::default().dim()),
        ]);
        frame.render_widget(
            Paragraph::new(help),
            Rect::new(
                inner.x,
                inner.y + inner.height.saturating_sub(1),
                inner.width,
                1,
            ),
        );
    }
}
