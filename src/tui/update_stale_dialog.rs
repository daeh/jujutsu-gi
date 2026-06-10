use crate::jujutsu::{self, StaleDiff};
use anyhow::{Context, Result};
use crossterm::event::KeyCode;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub(crate) enum DiffKind {
    Modified,
    DiskOnly,
    JjOnly,
}

pub(crate) struct UpdateStaleDiffDialog {
    items: Vec<(DiffKind, String)>,
    state: ListState,
    summary: String,
    ws_path: PathBuf,
}

impl UpdateStaleDiffDialog {
    /// Snapshot for the deferred PendingHandoff::SaveDiff variant — clones
    /// the currently selected item plus the workspace path so the drain
    /// block can call `save_diff_inline` without a borrow of the dialog.
    pub(crate) fn save_diff_args(&self) -> Option<(PathBuf, DiffKind, String)> {
        let idx = self.state.selected()?;
        let (kind, rel_path) = self.items.get(idx)?;
        Some((self.ws_path.clone(), *kind, rel_path.clone()))
    }

    /// Snapshot for PendingHandoff::SaveAllDiffs. Clones the items + ws_path.
    pub(crate) fn save_all_diffs_args(&self) -> (PathBuf, Vec<(DiffKind, String)>) {
        (self.ws_path.clone(), self.items.clone())
    }
}

impl UpdateStaleDiffDialog {
    pub(crate) fn new(diff: &StaleDiff, ws_path: &Path) -> Self {
        let mut items = Vec::new();
        for p in &diff.modified {
            items.push((DiffKind::Modified, p.clone()));
        }
        for p in &diff.disk_only {
            items.push((DiffKind::DiskOnly, p.clone()));
        }
        for p in &diff.jj_only {
            items.push((DiffKind::JjOnly, p.clone()));
        }
        let mut state = ListState::default();
        if !items.is_empty() {
            state.select(Some(0));
        }
        Self {
            items,
            state,
            summary: diff.summary(),
            ws_path: ws_path.to_path_buf(),
        }
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
        let visible = self.items.len().min(10);
        let height = (visible as u16) + 5; // border(2) + summary(1) + gap(1) + help(1)
        let width = 64u16.min(area.width.saturating_sub(4));
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);
        let block = Block::bordered()
            .title(" Update stale ")
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

        // Summary line
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Span::styled(&self.summary, Style::default().dim())),
            Rect::new(
                chunks[0].x + 1,
                chunks[0].y,
                chunks[0].width.saturating_sub(2),
                1,
            ),
        );

        // File list
        let list_items: Vec<ListItem> = self
            .items
            .iter()
            .map(|(kind, path)| {
                let (prefix, color) = match kind {
                    DiffKind::Modified => ("M ", Color::Yellow),
                    DiffKind::DiskOnly => ("A ", Color::Green),
                    DiffKind::JjOnly => ("D ", Color::Red),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(color).bold()),
                    Span::raw(path),
                ]))
            })
            .collect();

        let list = List::new(list_items).highlight_style(Style::default().bg(Color::DarkGray));
        frame.render_stateful_widget(list, chunks[1], &mut self.state.clone());

        // Help line
        let help = Line::from(vec![
            Span::styled("Enter", Style::default().fg(Color::Cyan).bold()),
            Span::styled(" save diff  ", Style::default().dim()),
            Span::styled("a", Style::default().fg(Color::Cyan).bold()),
            Span::styled(" save all  ", Style::default().dim()),
            Span::styled("y", Style::default().fg(Color::Green).bold()),
            Span::styled(" update  ", Style::default().dim()),
            Span::styled("Esc", Style::default().bold()),
            Span::styled(" cancel", Style::default().dim()),
        ]);
        frame.render_widget(help, chunks[2]);
    }
}

/// Save one file's diff to `.ji/diffs/`, callable from the
/// `PendingHandoff::SaveDiff` arm without a borrow of the dialog.
pub(crate) fn save_diff_inline(
    ws_path: &Path,
    kind: DiffKind,
    rel_path: &str,
) -> Result<Option<String>> {
    crate::jj_utils::ensure_ji_dir(ws_path)?;
    let diffs_dir = ws_path.join(".ji").join("diffs");
    write_one_diff_inline(&diffs_dir, ws_path, kind, rel_path)?;
    let out_path = diffs_dir.join(rel_path);
    Ok(Some(out_path.display().to_string()))
}

/// Save all changed files' diffs to `.ji/diffs/`, for the
/// `PendingHandoff::SaveAllDiffs` arm.
pub(crate) fn save_all_diffs_inline(ws_path: &Path, items: &[(DiffKind, String)]) -> Result<usize> {
    if items.is_empty() {
        return Ok(0);
    }
    crate::jj_utils::ensure_ji_dir(ws_path)?;
    let diffs_dir = ws_path.join(".ji").join("diffs");
    for (kind, rel_path) in items {
        write_one_diff_inline(&diffs_dir, ws_path, *kind, rel_path)?;
    }
    Ok(items.len())
}

fn write_one_diff_inline(
    diffs_dir: &Path,
    ws_path: &Path,
    kind: DiffKind,
    rel_path: &str,
) -> Result<()> {
    let out_path = diffs_dir.join(rel_path);
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let content = match kind {
        DiffKind::Modified => {
            let disk = std::fs::read(ws_path.join(rel_path))
                .with_context(|| format!("failed to read {rel_path}"))?;
            let jj = jujutsu::file_show_raw(ws_path, rel_path)?;
            unified_diff(rel_path, &jj, &disk)
        }
        DiffKind::DiskOnly => {
            let disk = std::fs::read(ws_path.join(rel_path))
                .with_context(|| format!("failed to read {rel_path}"))?;
            all_added(rel_path, &disk)
        }
        DiffKind::JjOnly => {
            let jj = jujutsu::file_show_raw(ws_path, rel_path)?;
            all_removed(rel_path, &jj)
        }
    };
    std::fs::write(&out_path, &content)
        .with_context(|| format!("failed to write {}", out_path.display()))?;
    Ok(())
}

/// Compute the stale diff and save diffs for all changed files to `.ji/diffs/`.
/// Returns the number of diff files written.
pub(crate) fn save_all_stale_diffs(ws_path: &Path) -> Result<usize> {
    let diff = jujutsu::stale_workspace_diff(ws_path)?;

    let total = diff.modified.len() + diff.disk_only.len() + diff.jj_only.len();
    if total == 0 {
        return Ok(0);
    }

    crate::jj_utils::ensure_ji_dir(ws_path)?;
    let diffs_dir = ws_path.join(".ji").join("diffs");
    let mut saved = 0usize;

    for rel_path in &diff.modified {
        let disk = std::fs::read(ws_path.join(rel_path))
            .with_context(|| format!("failed to read {rel_path}"))?;
        let jj = jujutsu::file_show_raw(ws_path, rel_path)?;
        let content = unified_diff(rel_path, &jj, &disk);
        write_diff_file(&diffs_dir, rel_path, &content)?;
        saved += 1;
    }

    for rel_path in &diff.disk_only {
        let disk = std::fs::read(ws_path.join(rel_path))
            .with_context(|| format!("failed to read {rel_path}"))?;
        let content = all_added(rel_path, &disk);
        write_diff_file(&diffs_dir, rel_path, &content)?;
        saved += 1;
    }

    for rel_path in &diff.jj_only {
        let jj = jujutsu::file_show_raw(ws_path, rel_path)?;
        let content = all_removed(rel_path, &jj);
        write_diff_file(&diffs_dir, rel_path, &content)?;
        saved += 1;
    }

    Ok(saved)
}

fn write_diff_file(diffs_dir: &Path, rel_path: &str, content: &str) -> Result<()> {
    let out_path = diffs_dir.join(rel_path);
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&out_path, content)
        .with_context(|| format!("failed to write {}", out_path.display()))?;
    Ok(())
}

fn unified_diff(path: &str, old: &[u8], new: &[u8]) -> String {
    let old_s = String::from_utf8_lossy(old);
    let new_s = String::from_utf8_lossy(new);
    let old_lines: Vec<&str> = old_s.lines().collect();
    let new_lines: Vec<&str> = new_s.lines().collect();
    let mut out = format!("--- jj@/{path}\n+++ disk/{path}\n");
    out.push_str(&format!(
        "@@ -1,{} +1,{} @@\n",
        old_lines.len(),
        new_lines.len()
    ));
    for line in &old_lines {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in &new_lines {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn all_added(path: &str, content: &[u8]) -> String {
    let s = String::from_utf8_lossy(content);
    let lines: Vec<&str> = s.lines().collect();
    let mut out = format!("--- /dev/null\n+++ disk/{path}\n");
    out.push_str(&format!("@@ -0,0 +1,{} @@\n", lines.len()));
    for line in &lines {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn all_removed(path: &str, content: &[u8]) -> String {
    let s = String::from_utf8_lossy(content);
    let lines: Vec<&str> = s.lines().collect();
    let mut out = format!("--- jj@/{path}\n+++ /dev/null\n");
    out.push_str(&format!("@@ -1,{} +0,0 @@\n", lines.len()));
    for line in &lines {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    out
}
