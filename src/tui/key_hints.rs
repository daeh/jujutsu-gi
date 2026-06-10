use ratatui::prelude::*;
use unicode_width::UnicodeWidthStr;

/// One atomic unit on a help bar — never split across rows by `wrap_hints`.
///
/// Each hint tracks two widths: the visible content (always counted) and the
/// trailing inter-chunk separator (counted only when followed by another chunk
/// on the same row). This lets the wrapper place a chunk at the row's right
/// edge without spuriously bumping it to a new row because its trailing spaces
/// wouldn't fit.
pub(crate) struct Hint {
    spans: Vec<Span<'static>>,
    content_w: u16,
    sep_w: u16,
}

/// Split a string at the boundary between visible content and trailing
/// inter-chunk separator. "Trailing whitespace" here means ASCII space
/// (U+0020) only — tabs, NBSP, and other whitespace stay as content. This
/// matches the convention used at every call site, where separators are
/// always literal `"  "`.
fn split_trailing_spaces(s: &'static str) -> (&'static str, &'static str) {
    let trimmed = s.trim_end_matches(' ');
    let sep = &s[trimmed.len()..];
    (trimmed, sep)
}

/// Bold key + dim label. The `label` should include any leading space between
/// key and label, plus the conventional 2-space trailing separator.
pub(crate) fn key_pair(key: &'static str, label: &'static str) -> Hint {
    let (label_body, sep) = split_trailing_spaces(label);
    let content_w = (UnicodeWidthStr::width(key) + UnicodeWidthStr::width(label_body)) as u16;
    let sep_w = UnicodeWidthStr::width(sep) as u16;
    Hint {
        spans: vec![
            Span::styled(key, Style::default().bold()),
            Span::styled(label, Style::default().dim()),
        ],
        content_w,
        sep_w,
    }
}

/// Two keys sharing one label, separated by a styled token (e.g. `/` dim).
pub(crate) fn key_pair_two(
    key_a: &'static str,
    sep_token: &'static str,
    key_b: &'static str,
    label: &'static str,
) -> Hint {
    let (label_body, trailing) = split_trailing_spaces(label);
    let content_w = (UnicodeWidthStr::width(key_a)
        + UnicodeWidthStr::width(sep_token)
        + UnicodeWidthStr::width(key_b)
        + UnicodeWidthStr::width(label_body)) as u16;
    Hint {
        spans: vec![
            Span::styled(key_a, Style::default().bold()),
            Span::styled(sep_token, Style::default().dim()),
            Span::styled(key_b, Style::default().bold()),
            Span::styled(label, Style::default().dim()),
        ],
        content_w,
        sep_w: UnicodeWidthStr::width(trailing) as u16,
    }
}

/// Arbitrary styled marker (e.g. `STATUS ` in red bold). Trailing spaces in
/// `text` are treated as the separator, matching the `key_pair` convention.
pub(crate) fn marker(text: &'static str, style: Style) -> Hint {
    let (body, sep) = split_trailing_spaces(text);
    Hint {
        spans: vec![Span::styled(text, style)],
        content_w: UnicodeWidthStr::width(body) as u16,
        sep_w: UnicodeWidthStr::width(sep) as u16,
    }
}

/// Escape hatch for callers that need fully-custom span sequences.
#[allow(dead_code)]
pub(crate) fn custom(spans: Vec<Span<'static>>, content_width: u16, separator_width: u16) -> Hint {
    Hint {
        spans,
        content_w: content_width,
        sep_w: separator_width,
    }
}

/// Greedily pack hints into rows of at most `width` display columns. Each hint
/// is atomic. The trailing separator of the right-most chunk on a row is not
/// counted against `width`. Returns one `Line` per row.
///
/// At widths smaller than the widest hint's content_w, that hint will overflow
/// and be visually clipped by ratatui — the only failure mode. At realistic
/// terminal widths (≥ 50 cols) this does not occur with the current hint sets.
pub(crate) fn wrap_hints(hints: Vec<Hint>, width: u16) -> Vec<Line<'static>> {
    if hints.is_empty() || width == 0 {
        return Vec::new();
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    // Width committed to the row so far, *excluding* the trailing separator
    // of the most-recent chunk. The trailing sep is held in `last_sep_w` and
    // counted only if another chunk is added to the same row.
    let mut current_committed: u16 = 0;
    let mut last_sep_w: u16 = 0;
    for hint in hints {
        let projected = current_committed
            .saturating_add(last_sep_w)
            .saturating_add(hint.content_w);
        if !current.is_empty() && projected > width {
            lines.push(Line::from(std::mem::take(&mut current)));
            current_committed = 0;
            last_sep_w = 0;
        }
        current.extend(hint.spans);
        current_committed = current_committed
            .saturating_add(last_sep_w)
            .saturating_add(hint.content_w);
        last_sep_w = hint.sep_w;
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn empty_hints_yield_no_lines() {
        assert!(wrap_hints(vec![], 80).is_empty());
    }

    #[test]
    fn zero_width_yields_no_lines() {
        let hints = vec![key_pair("?", " help  ")];
        assert!(wrap_hints(hints, 0).is_empty());
    }

    #[test]
    fn all_fit_one_line() {
        let hints = vec![
            key_pair("?", " help  "),
            key_pair("q", " exit  "),
            key_pair("n", " new  "),
        ];
        let lines = wrap_hints(hints, 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(span_text(&lines[0]), "? help  q exit  n new  ");
    }

    #[test]
    fn wraps_on_overflow() {
        // " help  ", " exit  ", " new  " widths: 6+2, 6+2, 5+2 = 8, 8, 7.
        // At width 14: ?+help (6) + sep(2) + q+exit content(6) = 14, fits.
        // Next: 14 + sep(2) + n+new(5) = 21 > 14 → wrap.
        let hints = vec![
            key_pair("?", " help  "),
            key_pair("q", " exit  "),
            key_pair("n", " new  "),
        ];
        let lines = wrap_hints(hints, 14);
        assert_eq!(lines.len(), 2);
        assert_eq!(span_text(&lines[0]), "? help  q exit  ");
        assert_eq!(span_text(&lines[1]), "n new  ");
    }

    #[test]
    fn trailing_sep_does_not_force_wrap() {
        // Two hints, content widths 6 and 6, separators 2 and 2. Width 14.
        // Naive (counting both seps): 6+2+6+2 = 16 > 14 → would wrap.
        // Correct (last-sep excluded): 6+2+6 = 14 = 14 → fits on one row.
        let hints = vec![key_pair("?", " help  "), key_pair("q", " exit  ")];
        let lines = wrap_hints(hints, 14);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn oversized_chunk_stays_on_own_line() {
        // content_w of "Enter switch" = 5 + 7 = 12 > width 5. Still emitted.
        let hints = vec![key_pair("Enter", " switch  ")];
        let lines = wrap_hints(hints, 5);
        assert_eq!(lines.len(), 1);
        assert_eq!(span_text(&lines[0]), "Enter switch  ");
    }

    #[test]
    fn cjk_widths_counted() {
        // CJK char "界" is 2 cols. "k 界  " body = "k" + " 界" = 1 + 3 = 4 cols, sep 2.
        let hints = vec![
            key_pair("k", " \u{754c}  "),
            key_pair("k", " \u{754c}  "),
            key_pair("k", " \u{754c}  "),
        ];
        // Each content+sep = 6. Width 12: row 1 = 6+4 = 10 (chunk1+content of 2) → fit;
        // 10 + sep(2) + content(4) = 16 > 12 → wrap. Effectively 2 per row.
        let lines = wrap_hints(hints, 12);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn marker_packs_with_pairs() {
        let hints = vec![
            key_pair("?", " help  "),
            marker("STATUS ", Style::default().fg(Color::Red).bold()),
            key_pair("p", " copy  "),
        ];
        let lines = wrap_hints(hints, 80);
        assert_eq!(lines.len(), 1);
        let text = span_text(&lines[0]);
        // Marker has 1 trailing space (its own sep_w), so STATUS is followed
        // by exactly one space before "p".
        assert_eq!(text, "? help  STATUS p copy  ");
    }

    #[test]
    fn key_pair_two_styling() {
        let hint = key_pair_two("o", "/", "Esc", " back  ");
        assert_eq!(hint.content_w, 1 + 1 + 3 + 5);
        assert_eq!(hint.sep_w, 2);
        assert_eq!(hint.spans.len(), 4);
        assert_eq!(hint.spans[0].content, "o");
        assert_eq!(hint.spans[1].content, "/");
        assert_eq!(hint.spans[2].content, "Esc");
        assert_eq!(hint.spans[3].content, " back  ");
        assert!(hint.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(hint.spans[2].style.add_modifier.contains(Modifier::BOLD));
        assert!(hint.spans[1].style.add_modifier.contains(Modifier::DIM));
        assert!(hint.spans[3].style.add_modifier.contains(Modifier::DIM));
    }

    /// Build the maximum workspace hint set (all conditional flags true) and
    /// record row counts at common widths. Acts as regression detection: if
    /// the hint set or wrap algorithm changes, this surfaces the diff.
    fn workspace_full_hint_set() -> Vec<Hint> {
        vec![
            key_pair("?", " help  "),
            key_pair("q", " exit  "),
            key_pair("Enter", " switch  "),
            key_pair("n", " new  "),
            key_pair("s", " sync  "),
            key_pair("x", " close  "),
            key_pair("t", " transfer  "),
            key_pair("b", " bookmarks  "),
            key_pair("c", " copy  "),
            key_pair("i", " files  "),
            key_pair("o", " op-log  "),
            key_pair("u", " update  "),
            key_pair("Z", " undo  "),
            marker("STATUS ", Style::default().fg(Color::Red).bold()),
            key_pair("p", " copy  "),
            key_pair("P", " clear  "),
        ]
    }

    #[test]
    fn width_target_workspace_full_set_at_160() {
        let lines = wrap_hints(workspace_full_hint_set(), 160);
        assert_eq!(lines.len(), 1, "at 160 cols all hints fit on one row");
    }

    #[test]
    fn width_target_workspace_full_set_at_120() {
        let lines = wrap_hints(workspace_full_hint_set(), 120);
        assert!(
            lines.len() <= 2,
            "at 120 cols ≤ 2 rows; got {}",
            lines.len()
        );
    }

    #[test]
    fn width_target_workspace_full_set_at_80() {
        let lines = wrap_hints(workspace_full_hint_set(), 80);
        assert!(
            (2..=3).contains(&lines.len()),
            "at 80 cols expect 2-3 rows; got {}",
            lines.len()
        );
    }

    #[test]
    fn width_target_workspace_full_set_at_60() {
        let lines = wrap_hints(workspace_full_hint_set(), 60);
        assert!(
            (3..=4).contains(&lines.len()),
            "at 60 cols expect 3-4 rows; got {}",
            lines.len()
        );
    }

    #[test]
    fn split_trailing_spaces_basic() {
        assert_eq!(split_trailing_spaces(" help  "), (" help", "  "));
        assert_eq!(split_trailing_spaces("STATUS "), ("STATUS", " "));
        assert_eq!(split_trailing_spaces("noend"), ("noend", ""));
        assert_eq!(split_trailing_spaces("   "), ("", "   "));
    }
}
