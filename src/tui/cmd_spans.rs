use ratatui::prelude::*;

/// Base style for command preview lines.
fn base() -> Style {
    Style::default().fg(Color::Yellow).dim()
}

/// Literal command text (yellow dim).
pub(crate) fn lit(s: &str) -> Span<'static> {
    Span::styled(s.to_string(), base())
}

/// Revision ID (#800B80 italic).
pub(crate) fn rev(s: &str) -> Span<'static> {
    Span::styled(
        s.to_string(),
        Style::default().fg(Color::Rgb(0x80, 0x0B, 0x80)).italic(),
    )
}

/// Quoted message: wraps text in `"…"` all in dim style.
pub(crate) fn quoted_msg(s: &str) -> Span<'static> {
    Span::styled(format!("\"{s}\""), Style::default().dim())
}

/// Extract plain text from styled command lines (for clipboard).
pub(crate) fn lines_to_plain(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
