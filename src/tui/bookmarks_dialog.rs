use super::dialog_common;
use super::line_edit::LineEditor;
use crate::jujutsu::{RevisionInfo, Workspace};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph};
use std::collections::HashSet;

/// Whether the action bar performs tug or delete.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum BookmarkAction {
    Tug,
    Delete,
}

#[derive(Clone, Copy, PartialEq)]
enum PopupField {
    Name,
    ChangeId,
}

pub(crate) struct NewBookmarkPopup {
    name: String,
    cursor_pos: usize,
    change_id: String,
    revision_choices: Vec<String>,
    revision_index: Option<usize>,
    active_field: PopupField,
}

struct BookmarkEntry {
    annotation: String,
    name: String,
    at_head: bool,
}

pub(crate) struct BookmarksDialog {
    pub head_id: String,
    entries: Vec<BookmarkEntry>,
    selected: HashSet<usize>,
    cursor: usize,
    scroll_offset: usize,
    pub action: BookmarkAction,
    state: ListState,
    pub new_popup: Option<NewBookmarkPopup>,
    revisions: Vec<RevisionInfo>,
}

impl BookmarksDialog {
    /// `effective_head` is the non-trivial head for tug operations.
    /// Pass `None` to fall back to `ws.revisions.first()` (= `@`).
    pub(crate) fn new(ws: &Workspace, effective_head: Option<String>) -> Self {
        let head_id = effective_head.unwrap_or_else(|| {
            ws.revisions
                .first()
                .map(|r| r.change_id.clone())
                .unwrap_or_else(|| ws.change_id.clone())
        });

        let mut entries = Vec::new();
        for (bm, bm_id) in &ws.bookmarks_at_head {
            let ann = super::bookmark_annotation(bm_id, true, &ws.change_id, &ws.revisions);
            entries.push(BookmarkEntry {
                annotation: ann,
                name: bm.clone(),
                at_head: true,
            });
        }
        for (bm, bm_id) in &ws.bookmarks_behind {
            let ann = super::bookmark_annotation(bm_id, false, &ws.change_id, &ws.revisions);
            entries.push(BookmarkEntry {
                annotation: ann,
                name: bm.clone(),
                at_head: false,
            });
        }

        let mut state = ListState::default();
        if !entries.is_empty() {
            state.select(Some(0));
        }

        Self {
            head_id,
            entries,
            selected: HashSet::new(),
            cursor: 0,
            scroll_offset: 0,
            action: BookmarkAction::Tug,
            state,
            new_popup: None,
            revisions: ws.revisions.clone(),
        }
    }

    // -- Navigation --

    pub(crate) fn move_up(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.cursor = if self.cursor == 0 {
            self.entries.len() - 1
        } else {
            self.cursor - 1
        };
        self.state.select(Some(self.cursor));
    }

    pub(crate) fn move_down(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.cursor = if self.cursor >= self.entries.len() - 1 {
            0
        } else {
            self.cursor + 1
        };
        self.state.select(Some(self.cursor));
    }

    // -- Selection --

    pub(crate) fn toggle_selection(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        if self.selected.contains(&self.cursor) {
            self.selected.remove(&self.cursor);
        } else {
            self.selected.insert(self.cursor);
        }
    }

    /// Move cursor to a specific index and toggle its selection.
    pub(crate) fn click_entry(&mut self, idx: usize) {
        if idx < self.entries.len() {
            self.cursor = idx;
            self.state.select(Some(idx));
            self.toggle_selection();
        }
    }

    pub(crate) fn toggle_select_all(&mut self) {
        if self.selected.len() == self.entries.len() {
            self.selected.clear();
        } else {
            self.selected = (0..self.entries.len()).collect();
        }
    }

    pub(crate) fn has_selection(&self) -> bool {
        !self.selected.is_empty()
    }

    pub(crate) fn selected_bookmark_names(&self) -> Vec<String> {
        self.selected
            .iter()
            .filter_map(|&i| self.entries.get(i))
            .map(|e| e.name.clone())
            .collect()
    }

    // -- Key handling --

    /// Dialog-internal key handling, branching on whether the new-bookmark
    /// popup is open: popup field toggling and text editing, or list
    /// navigation, selection, and the tug/delete action toggle. Esc, Enter,
    /// and popup revision cycling (which drives the graph pane) stay with
    /// the App.
    pub(crate) fn handle_key(&mut self, key: KeyEvent) {
        if self.new_popup.is_some() {
            match key.code {
                KeyCode::Tab | KeyCode::Up | KeyCode::Down => {
                    self.new_popup_toggle_field();
                }
                KeyCode::Char(c) => {
                    if key.modifiers.contains(KeyModifiers::CONTROL) {
                        match c {
                            'u' => self.new_popup_delete_to_start(),
                            'k' => self.new_popup_delete_to_end(),
                            'a' => self.new_popup_move_home(),
                            'e' => self.new_popup_move_end(),
                            _ => {}
                        }
                    } else {
                        self.new_popup_insert_char(c);
                    }
                }
                KeyCode::Backspace => {
                    self.new_popup_delete_char();
                }
                KeyCode::Home => {
                    self.new_popup_move_home();
                }
                KeyCode::End => {
                    self.new_popup_move_end();
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Char('n') => {
                    self.open_new_popup();
                }
                KeyCode::Char('t') => {
                    self.action = BookmarkAction::Tug;
                }
                KeyCode::Char('x') => {
                    self.action = BookmarkAction::Delete;
                }
                KeyCode::Left => {
                    self.action = BookmarkAction::Tug;
                }
                KeyCode::Right => {
                    self.action = BookmarkAction::Delete;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.move_up();
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.move_down();
                }
                KeyCode::Char(' ') => {
                    self.toggle_selection();
                }
                KeyCode::Char('a') => {
                    self.toggle_select_all();
                }
                _ => {}
            }
        }
    }

    // -- New bookmark popup --

    pub(crate) fn open_new_popup(&mut self) {
        let change_id = self.head_id.clone();
        let revision_choices: Vec<String> =
            self.revisions.iter().map(|r| r.change_id.clone()).collect();
        let revision_index = if revision_choices.is_empty() {
            None
        } else {
            Some(0)
        };
        self.new_popup = Some(NewBookmarkPopup {
            name: String::new(),
            cursor_pos: 0,
            change_id,
            revision_choices,
            revision_index,
            active_field: PopupField::Name,
        });
    }

    pub(crate) fn close_new_popup(&mut self) {
        self.new_popup = None;
    }

    /// Returns (bookmark_name, change_id) if valid, None if name is empty.
    pub(crate) fn confirm_new_popup(&self) -> Option<(String, String)> {
        let popup = self.new_popup.as_ref()?;
        if popup.name.trim().is_empty() {
            return None;
        }
        Some((popup.name.clone(), popup.change_id.clone()))
    }

    pub(crate) fn new_popup_toggle_field(&mut self) {
        if let Some(popup) = &mut self.new_popup {
            popup.active_field = match popup.active_field {
                PopupField::Name => PopupField::ChangeId,
                PopupField::ChangeId => PopupField::Name,
            };
            popup.cursor_pos = popup.active_ref().len();
        }
    }

    /// Cycle through revision choices by `delta` (+1 = older, -1 = newer).
    /// Returns the new change_id if cycling occurred.
    pub(crate) fn new_popup_cycle_revision(&mut self, delta: isize) -> Option<&str> {
        let popup = self.new_popup.as_mut()?;
        if popup.active_field != PopupField::ChangeId {
            return None;
        }
        if popup.revision_choices.is_empty() {
            return None;
        }
        let len = popup.revision_choices.len();
        let current = popup.revision_index.unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(len as isize) as usize;
        popup.revision_index = Some(next);
        popup.change_id = popup.revision_choices[next].clone();
        popup.cursor_pos = popup.change_id.len();
        Some(&self.new_popup.as_ref().unwrap().change_id)
    }

    // Text/cursor mutation delegates to `LineEditor` (grapheme-aware); the
    // popup layers its field-specific semantics on top: the name space ban
    // and revision-cycle exits.

    pub(crate) fn new_popup_insert_char(&mut self, c: char) {
        if let Some(popup) = &mut self.new_popup {
            // Disallow spaces in bookmark name
            if popup.active_field == PopupField::Name && c == ' ' {
                return;
            }
            popup.exit_revision_cycle();
            popup.editor().insert_char(c);
        }
    }

    pub(crate) fn new_popup_delete_char(&mut self) {
        if let Some(popup) = &mut self.new_popup {
            popup.exit_revision_cycle();
            popup.editor().backspace();
        }
    }

    pub(crate) fn new_popup_delete_to_start(&mut self) {
        if let Some(popup) = &mut self.new_popup {
            popup.exit_revision_cycle();
            popup.editor().delete_to_start();
        }
    }

    pub(crate) fn new_popup_delete_to_end(&mut self) {
        if let Some(popup) = &mut self.new_popup {
            popup.exit_revision_cycle();
            popup.editor().delete_to_end();
        }
    }

    pub(crate) fn new_popup_move_home(&mut self) {
        if let Some(popup) = &mut self.new_popup {
            popup.cursor_pos = 0;
        }
    }

    pub(crate) fn new_popup_move_end(&mut self) {
        if let Some(popup) = &mut self.new_popup {
            popup.cursor_pos = popup.active_ref().len();
        }
    }

    // -- Hit testing for mouse --

    /// Returns which zone was clicked, if any.
    /// `area` is the workspace_area passed to draw().
    pub(crate) fn hit_test(&self, area: Rect, col: u16, row: u16) -> Option<HitZone> {
        let block = Block::bordered();
        let inner = block.inner(area);
        if col < inner.x || col >= inner.x + inner.width {
            return None;
        }

        // Row offsets within inner:
        // 0: gap
        // 1: "(n) new bookmark"
        // 2: gap
        // 3: "(t) tug    (x) delete"
        // 4: gap
        // 5..: bookmark list
        let rel_y = row.checked_sub(inner.y)?;

        if rel_y == 1 {
            return Some(HitZone::NewBookmark);
        }
        if rel_y == 3 {
            // Left half = tug, right half = delete
            let mid = inner.x + inner.width / 2;
            return if col < mid {
                Some(HitZone::ToggleTug)
            } else {
                Some(HitZone::ToggleDelete)
            };
        }
        let list_start = 5u16;
        let help_height = 3u16; // top border + help line + bottom border
        let list_end = inner.height.saturating_sub(help_height);
        if rel_y >= list_start && rel_y < list_end {
            let list_idx = (rel_y - list_start) as usize + self.scroll_offset;
            if list_idx < self.entries.len() {
                return Some(HitZone::BookmarkEntry(list_idx));
            }
        }

        None
    }

    // -- Rendering --

    pub(crate) fn draw(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        let block = Block::bordered()
            .title(" Bookmarks ")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height < 8 || inner.width < 20 {
            return;
        }

        let mut y = 0u16;

        // Section 1: (n) new bookmark
        y += 1; // gap
        let new_line = Line::from(vec![
            Span::styled("  (", Style::default().dim()),
            Span::styled("n", Style::default().bold()),
            Span::styled(") new bookmark", Style::default().dim()),
        ]);
        frame.render_widget(
            Paragraph::new(new_line),
            Rect::new(inner.x, inner.y + y, inner.width, 1),
        );
        y += 1;

        // Section 2: (t) tug / (x) delete toggle
        y += 1; // gap
        let tug_style = if self.action == BookmarkAction::Tug {
            Style::default().bold()
        } else {
            Style::default().dim()
        };
        let del_style = if self.action == BookmarkAction::Delete {
            Style::default().bold().fg(Color::Red)
        } else {
            Style::default().dim()
        };
        let toggle_line = Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled("(t)", tug_style),
            Span::styled(" tug    ", Style::default().dim()),
            Span::styled("(x)", del_style),
            Span::styled(" delete", del_style),
        ]);
        frame.render_widget(
            Paragraph::new(toggle_line),
            Rect::new(inner.x, inner.y + y, inner.width, 1),
        );
        y += 1;

        // Section 3: bookmark list
        y += 1; // gap
        let help_height = 3u16;
        let list_height = inner.height.saturating_sub(y + help_height);

        if !self.entries.is_empty() && list_height > 0 {
            let ann_w = 8;
            let list_items: Vec<ListItem> = self
                .entries
                .iter()
                .enumerate()
                .map(|(i, entry)| {
                    let checked = if self.selected.contains(&i) {
                        "● "
                    } else {
                        "○ "
                    };
                    let base_style = if entry.at_head {
                        Style::default().fg(Color::DarkGray)
                    } else {
                        Style::default()
                    };
                    let check_style =
                        if self.selected.contains(&i) && self.action == BookmarkAction::Delete {
                            Style::default().fg(Color::Red)
                        } else {
                            base_style
                        };
                    let line = Line::from(vec![
                        Span::styled(checked, check_style),
                        Span::styled(format!("{:<ann_w$}", entry.annotation), base_style.bold()),
                        Span::styled(&entry.name, base_style),
                    ]);
                    ListItem::new(line)
                })
                .collect();

            let list = List::new(list_items).highlight_style(Style::default().bg(Color::DarkGray));
            let list_area = Rect::new(inner.x, inner.y + y, inner.width, list_height);
            frame.render_stateful_widget(list, list_area, &mut self.state.clone());
        } else if list_height > 0 {
            let empty = Span::styled("  (no bookmarks)", Style::default().dim());
            frame.render_widget(
                Paragraph::new(Line::from(empty)),
                Rect::new(inner.x, inner.y + y, inner.width, 1),
            );
        }

        // Section 4: help bar
        let help_y = inner.y + inner.height.saturating_sub(help_height);
        let help_block = ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::TOP)
            .border_style(Style::default().dim());
        let action_label = match self.action {
            BookmarkAction::Tug => "tug",
            BookmarkAction::Delete => "delete",
        };
        let exec_style = if self.has_selection() {
            Style::default().bold()
        } else {
            Style::default().dim()
        };
        let exec_desc_style = Style::default().dim();
        let help_line = Line::from(vec![
            Span::styled("  y/Enter", exec_style),
            Span::styled(format!("  {action_label}   "), exec_desc_style),
            Span::styled("a", Style::default().bold()),
            Span::styled("  all   ", Style::default().dim()),
            Span::styled("Esc", Style::default().bold()),
            Span::styled("  back", Style::default().dim()),
        ]);
        frame.render_widget(
            Paragraph::new(help_line).block(help_block),
            Rect::new(inner.x, help_y, inner.width, help_height),
        );

        // Draw new-bookmark popup on top if open
        if let Some(popup) = &self.new_popup {
            self.draw_new_popup(popup, frame, area);
        }
    }

    fn draw_new_popup(&self, popup: &NewBookmarkPopup, frame: &mut Frame, area: Rect) {
        let width = 48.min(area.width.saturating_sub(4));
        let height = 8.min(area.height.saturating_sub(2));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);
        let block = Block::bordered()
            .title(" New Bookmark ")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        if inner.height < 4 || inner.width < 20 {
            return;
        }

        let label_style = Style::default().dim();

        // Name field
        let name_active = popup.active_field == PopupField::Name;
        let name_val_style = if name_active {
            Style::default()
        } else {
            Style::default().dim()
        };
        let name_line = if name_active {
            dialog_common::field_with_cursor(
                "  Name:     ",
                &popup.name,
                popup.cursor_pos,
                label_style,
                name_val_style,
            )
        } else {
            Line::from(vec![
                Span::styled("  Name:     ", label_style),
                Span::styled(&popup.name, name_val_style),
            ])
        };
        frame.render_widget(
            Paragraph::new(name_line),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );

        // Revision field
        let rev_active = popup.active_field == PopupField::ChangeId;
        let rev_val_style = if rev_active {
            Style::default()
        } else {
            Style::default().dim()
        };
        let rev_line = if rev_active {
            dialog_common::field_with_cursor(
                "  Revision: ",
                &popup.change_id,
                popup.cursor_pos,
                label_style,
                rev_val_style,
            )
        } else {
            Line::from(vec![
                Span::styled("  Revision: ", label_style),
                Span::styled(&popup.change_id, rev_val_style),
            ])
        };
        frame.render_widget(
            Paragraph::new(rev_line),
            Rect::new(inner.x, inner.y + 3, inner.width, 1),
        );

        // Help line
        let can_create = !popup.name.trim().is_empty();
        let enter_style = if can_create {
            Style::default().bold()
        } else {
            Style::default().dim()
        };
        let help = Line::from(vec![
            Span::styled("  Enter", enter_style),
            Span::styled("  create   ", Style::default().dim()),
            Span::styled("Tab", Style::default().bold()),
            Span::styled("  switch   ", Style::default().dim()),
            Span::styled("Esc", Style::default().bold()),
            Span::styled("  back", Style::default().dim()),
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

impl NewBookmarkPopup {
    fn active_ref(&self) -> &str {
        match self.active_field {
            PopupField::Name => &self.name,
            PopupField::ChangeId => &self.change_id,
        }
    }

    /// Line-editor view over the active field and the shared cursor.
    fn editor(&mut self) -> LineEditor<'_> {
        let text = match self.active_field {
            PopupField::Name => &mut self.name,
            PopupField::ChangeId => &mut self.change_id,
        };
        LineEditor {
            text,
            cursor: &mut self.cursor_pos,
        }
    }

    /// Typing in the ChangeId field exits revision-cycling mode.
    fn exit_revision_cycle(&mut self) {
        if self.active_field == PopupField::ChangeId {
            self.revision_index = None;
        }
    }
}

pub(crate) enum HitZone {
    NewBookmark,
    ToggleTug,
    ToggleDelete,
    BookmarkEntry(usize),
}
