use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Truncate to fit within `max_cols` display columns, appending "\u{2026}" if truncated.
pub fn truncate_end(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(s) <= max_cols {
        return s.to_string();
    }
    // Reserve 1 column for the ellipsis.
    let limit = max_cols - 1;
    let mut width = 0;
    let mut truncated = String::new();
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if width + cw > limit {
            break;
        }
        width += cw;
        truncated.push(c);
    }
    truncated.push('\u{2026}');
    truncated
}

/// Truncate from the start to fit within `max_cols` display columns, prepending "\u{2026}".
pub fn truncate_start(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let total = UnicodeWidthStr::width(s);
    if total <= max_cols {
        return s.to_string();
    }
    // We need to skip enough from the start so that 1 (ellipsis) + remaining <= max_cols.
    let keep_cols = max_cols - 1;
    // Walk from the left, accumulating width to find the byte offset where the
    // remaining suffix fits within keep_cols.
    let mut skip_width = 0;
    let target_skip = total - keep_cols;
    for (byte_idx, c) in s.char_indices() {
        if skip_width >= target_skip {
            return format!("\u{2026}{}", &s[byte_idx..]);
        }
        skip_width += UnicodeWidthChar::width(c).unwrap_or(0);
    }
    // Entire string skipped — just the ellipsis.
    "\u{2026}".to_string()
}

/// Convert a screen column offset to a byte offset in `s`.
/// Uses display width: CJK/full-width chars occupy 2 columns.
///
/// Stops at grapheme boundaries (via `unicode-segmentation`) so a click
/// between code units of a multi-codepoint grapheme (e.g. a regional-
/// indicator pair `🇯🇵`) snaps to the next grapheme boundary rather than
/// splitting the grapheme.
pub fn col_to_byte_offset(s: &str, target_col: usize) -> usize {
    let mut col = 0;
    for (byte_idx, g) in s.grapheme_indices(true) {
        if col >= target_col {
            return byte_idx;
        }
        col += UnicodeWidthStr::width(g);
    }
    s.len()
}

/// Byte offset of the grapheme boundary at or after `byte_pos`.
/// Returns `byte_pos` if it is already at a boundary or beyond the string.
pub fn next_grapheme_boundary(s: &str, byte_pos: usize) -> usize {
    if byte_pos >= s.len() {
        return s.len();
    }
    s.grapheme_indices(true)
        .map(|(i, g)| i + g.len())
        .find(|&end| end > byte_pos)
        .unwrap_or(s.len())
}

/// Byte offset of the grapheme boundary at or before `byte_pos`.
/// Returns 0 if there is no earlier grapheme.
pub fn prev_grapheme_boundary(s: &str, byte_pos: usize) -> usize {
    let clamped = byte_pos.min(s.len());
    s.grapheme_indices(true)
        .map(|(i, _)| i)
        .take_while(|&i| i < clamped)
        .last()
        .unwrap_or(0)
}

/// Byte offset of the previous ASCII-space-delimited word boundary at or
/// before `byte_pos`. Iterates by grapheme so the returned offset always
/// falls on a grapheme cluster boundary, never inside one.
///
/// Steps back past any spaces, then past the run of non-space graphemes,
/// returning the offset immediately after the last preceding space (or 0).
/// Sound because ASCII `b' '` is a single-byte, single-grapheme entity —
/// no multi-byte grapheme can begin with `b' '`.
pub fn prev_word_boundary(s: &str, byte_pos: usize) -> usize {
    let clamped = byte_pos.min(s.len());
    if clamped == 0 {
        return 0;
    }
    // Collect graphemes that fall strictly before `clamped`. A mid-grapheme
    // `byte_pos` is tolerated by keeping only graphemes whose start offset
    // is < clamped (we don't try to split a grapheme in half).
    let graphemes: Vec<(usize, &str)> = s
        .grapheme_indices(true)
        .take_while(|(i, _)| *i < clamped)
        .collect();
    let Some(last_non_space) = graphemes
        .iter()
        .rposition(|(_, g)| g.as_bytes().first() != Some(&b' '))
    else {
        // All-space prefix: jump to start.
        return 0;
    };
    // Walk further back past any preceding space, returning the offset
    // immediately after it (i.e. the offset of the run of non-space graphemes).
    for i in (0..last_non_space).rev() {
        if graphemes[i].1.as_bytes().first() == Some(&b' ') {
            return graphemes[i + 1].0;
        }
    }
    0
}

/// Byte offset of the next ASCII-space-delimited word boundary at or after
/// `byte_pos`. Grapheme-safe.
///
/// Steps forward to the next space, then past the run of spaces, returning
/// the absolute byte offset of the next non-space grapheme (or `s.len()`).
pub fn next_word_boundary(s: &str, byte_pos: usize) -> usize {
    let clamped = byte_pos.min(s.len());
    if clamped == s.len() {
        return s.len();
    }
    // Walk graphemes of the whole string, skipping those that end at or before
    // `clamped` (tolerates a mid-grapheme `byte_pos`). Cross a
    // space-then-non-space transition and return the non-space offset.
    let mut seen_space = false;
    for (i, g) in s.grapheme_indices(true) {
        let end = i + g.len();
        if end <= clamped {
            continue;
        }
        let is_space = g.as_bytes().first() == Some(&b' ');
        if seen_space && !is_space {
            return i;
        }
        if is_space {
            seen_space = true;
        }
    }
    s.len()
}

/// Peel the first grapheme cluster off `s` for cursor-cell rendering.
/// Returns `(grapheme, rest)`, or `(" ", "")` when `s` is empty —
/// the empty case substitutes a space so the cursor cell still has
/// a visible inverse-video block at end-of-line.
pub fn peel_first_grapheme_for_cursor(s: &str) -> (&str, &str) {
    match s.graphemes(true).next() {
        Some(g) => s.split_at(g.len()),
        None => (" ", ""),
    }
}

/// Format `epoch_secs` relative to `now_secs` as a compact age string
/// (`17h`, `2w`, `4mo`, `1y`), matching worktrunk's `format_relative_time_short`:
/// a future timestamp → `"future"`, under a minute → `"now"`, otherwise the
/// largest whole unit by floor division through y/mo/w/d/h/m, with a 30-day
/// month and 365-day year (so `360d` is `12mo`, not `1y`).
pub fn relative_time_short(epoch_secs: i64, now_secs: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;
    const UNITS: &[(i64, &str)] = &[
        (YEAR, "y"),
        (MONTH, "mo"),
        (WEEK, "w"),
        (DAY, "d"),
        (HOUR, "h"),
        (MINUTE, "m"),
    ];

    let seconds_ago = now_secs - epoch_secs;
    if seconds_ago < 0 {
        return "future".to_string();
    }
    if seconds_ago < MINUTE {
        return "now".to_string();
    }
    for &(unit, abbrev) in UNITS {
        let value = seconds_ago / unit;
        if value > 0 {
            return format!("{value}{abbrev}");
        }
    }
    // Unreachable: seconds_ago >= MINUTE guarantees the MINUTE arm matches.
    "now".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grapheme_boundary_emoji() {
        // 🇯🇵 is two regional-indicator codepoints (8 UTF-8 bytes) forming
        // one user-perceived character.
        let s = "a🇯🇵b";
        assert_eq!(s.len(), 1 + 8 + 1);
        assert_eq!(next_grapheme_boundary(s, 0), 1); // past 'a'
        assert_eq!(next_grapheme_boundary(s, 1), 1 + 8); // past the flag
        assert_eq!(prev_grapheme_boundary(s, 1 + 8), 1); // back to before flag
        assert_eq!(prev_grapheme_boundary(s, 1 + 8 + 1), 1 + 8); // back to before 'b'
    }

    #[test]
    fn word_boundary_ascii() {
        let s = "hello world foo";
        // Cursor in the middle of "world" (between 'w' and 'orld'); prev
        // returns the offset of 'w' (start of the word).
        let mid_world = "hello ".len() + 1; // after 'w'
        assert_eq!(prev_word_boundary(s, mid_world), "hello ".len());
        // From end of string, prev returns the offset of "foo" (start of
        // the last word).
        assert_eq!(prev_word_boundary(s, s.len()), "hello world ".len());
        // From start of string, next-word returns offset of "world".
        assert_eq!(next_word_boundary(s, 0), "hello ".len());
        // From middle of "world", next-word skips through the rest of "world"
        // and the space, landing at "foo".
        assert_eq!(next_word_boundary(s, mid_world), "hello world ".len());
        // From end, next is end.
        assert_eq!(next_word_boundary(s, s.len()), s.len());
    }

    #[test]
    fn word_boundary_with_emoji() {
        // 🇯🇵 = 8 UTF-8 bytes, two regional-indicator codepoints, one grapheme.
        let s = "alpha🇯🇵 beta";
        let alpha = "alpha".len(); // 5
        let flag_end = alpha + 8; // 13
        let space = flag_end; // ' ' at byte 13
        let beta_start = space + 1; // 14
        assert_eq!(s.len(), beta_start + "beta".len());

        // From end-of-string, prev returns the start of "beta".
        assert_eq!(prev_word_boundary(s, s.len()), beta_start);
        // From byte_pos == flag_end (just past the flag), prev returns 0
        // (we walk back past the flag and "alpha" — there's no preceding space).
        assert_eq!(prev_word_boundary(s, flag_end), 0);
        // Mid-flag positions are clamped by grapheme walking — they must
        // never return an offset inside the flag (i.e. > alpha and < flag_end).
        // We exercise `flag_end - 4` which would be mid-flag if the impl
        // walked bytes; grapheme walking treats the flag as opaque.
        let mid_flag = alpha + 4;
        let prev = prev_word_boundary(s, mid_flag);
        assert!(
            prev <= alpha || prev >= flag_end,
            "prev_word_boundary returned mid-grapheme offset: {prev}"
        );

        // next-word from inside "alpha" jumps past the space at flag_end.
        let next = next_word_boundary(s, 0);
        assert_eq!(next, beta_start);
    }

    #[test]
    fn word_boundary_with_cjk() {
        // 世界 = two CJK ideographs, each 3 bytes, each its own grapheme.
        let s = "alpha 世界 beta";
        let alpha_end = "alpha".len(); // 5 (just before space)
        let cjk_start = "alpha ".len(); // 6
        let cjk_end = cjk_start + 6; // 12
        let beta_start = cjk_end + 1; // 13
        assert_eq!(s.len(), beta_start + "beta".len());

        // prev from end → start of "beta".
        assert_eq!(prev_word_boundary(s, s.len()), beta_start);
        // prev from end-of-CJK → start of CJK.
        assert_eq!(prev_word_boundary(s, cjk_end), cjk_start);
        // From mid-CJK (between the two ideographs is a grapheme boundary,
        // but inside an ideograph is not). Test mid-first-ideograph.
        let mid_first_cjk = cjk_start + 1;
        let prev = prev_word_boundary(s, mid_first_cjk);
        assert!(
            prev <= alpha_end || prev >= cjk_start,
            "prev_word_boundary returned mid-grapheme offset: {prev}"
        );

        // next from start → past space → CJK start.
        assert_eq!(next_word_boundary(s, 0), cjk_start);
        // next from start of CJK → past space → beta start.
        assert_eq!(next_word_boundary(s, cjk_start), beta_start);
    }

    #[test]
    fn peel_first_grapheme_for_cursor_cases() {
        // Empty: substitutes a space so the cursor cell stays visible.
        assert_eq!(peel_first_grapheme_for_cursor(""), (" ", ""));
        // ASCII: peels one byte.
        assert_eq!(peel_first_grapheme_for_cursor("abc"), ("a", "bc"));
        // Emoji flag: peels the whole 8-byte grapheme, not just the first
        // regional indicator.
        let (g, rest) = peel_first_grapheme_for_cursor("🇯🇵x");
        assert_eq!(g.len(), 8);
        assert_eq!(rest, "x");
        // CJK: peels the 3-byte ideograph.
        let (g, rest) = peel_first_grapheme_for_cursor("世界");
        assert_eq!(g, "世");
        assert_eq!(rest, "界");
    }

    #[test]
    fn col_to_byte_offset_emoji() {
        // Emoji-flag width is 2 columns under most width tables.
        let s = "a🇯🇵b";
        assert_eq!(col_to_byte_offset(s, 0), 0);
        assert_eq!(col_to_byte_offset(s, 1), 1); // 'a' takes 1 col
        // Click past the flag jumps to the byte after the whole grapheme,
        // not into the middle of it.
        let past_flag = col_to_byte_offset(s, 3);
        assert!(past_flag == 1 + 8, "expected past flag, got {past_flag}");
    }

    // --- truncate_end ---

    #[test]
    fn truncate_end_ascii_fits() {
        assert_eq!(truncate_end("hello", 10), "hello");
    }

    #[test]
    fn truncate_end_ascii_exact() {
        assert_eq!(truncate_end("hello", 5), "hello");
    }

    #[test]
    fn truncate_end_ascii_over() {
        assert_eq!(truncate_end("hello world", 5), "hell\u{2026}");
    }

    #[test]
    fn truncate_end_empty() {
        assert_eq!(truncate_end("", 5), "");
    }

    #[test]
    fn truncate_end_max_zero() {
        assert_eq!(truncate_end("hello", 0), "");
    }

    #[test]
    fn truncate_end_max_one() {
        assert_eq!(truncate_end("hello", 1), "\u{2026}");
    }

    #[test]
    fn truncate_end_emoji() {
        // Each emoji is 2 display columns in unicode-width 0.2.
        // 3 emoji = 6 cols. max_cols=5 → fit 2 emoji (4 cols) + ellipsis (1 col) = 5.
        assert_eq!(
            truncate_end("\u{1F600}\u{1F601}\u{1F602}", 5),
            "\u{1F600}\u{1F601}\u{2026}"
        );
    }

    #[test]
    fn truncate_end_emoji_tight() {
        // max_cols=3 → fit 1 emoji (2 cols) + ellipsis (1 col) = 3.
        assert_eq!(truncate_end("\u{1F600}\u{1F601}", 3), "\u{1F600}\u{2026}");
    }

    #[test]
    fn truncate_end_cjk() {
        // 世界 = 2 chars, 4 display columns.
        assert_eq!(truncate_end("\u{4e16}\u{754c}", 4), "\u{4e16}\u{754c}");
        // max_cols=3 → fit 1 CJK (2 cols) + ellipsis (1 col) = 3.
        assert_eq!(truncate_end("\u{4e16}\u{754c}", 3), "\u{4e16}\u{2026}");
    }

    #[test]
    fn truncate_end_cjk_boundary() {
        // max_cols=2 → limit = 1 col for content. CJK char is 2 cols, doesn't fit.
        // Result: just the ellipsis.
        assert_eq!(truncate_end("\u{4e16}\u{754c}", 2), "\u{2026}");
    }

    // --- truncate_start ---

    #[test]
    fn truncate_start_ascii_fits() {
        assert_eq!(truncate_start("hello", 10), "hello");
    }

    #[test]
    fn truncate_start_ascii_over() {
        assert_eq!(truncate_start("hello world", 5), "\u{2026}orld");
    }

    #[test]
    fn truncate_start_empty() {
        assert_eq!(truncate_start("", 5), "");
    }

    #[test]
    fn truncate_start_max_zero() {
        assert_eq!(truncate_start("hello", 0), "");
    }

    #[test]
    fn truncate_start_max_one() {
        assert_eq!(truncate_start("hello", 1), "\u{2026}");
    }

    #[test]
    fn truncate_start_cjk() {
        // 世界好 = 3 chars, 6 display columns.
        // max_cols=5 → keep 4 cols from end + ellipsis = 5.
        // Skip first char (2 cols), keep 界好 (4 cols).
        assert_eq!(
            truncate_start("\u{4e16}\u{754c}\u{597d}", 5),
            "\u{2026}\u{754c}\u{597d}"
        );
    }

    // --- col_to_byte_offset ---

    #[test]
    fn col_to_byte_ascii() {
        assert_eq!(col_to_byte_offset("hello", 2), 2);
    }

    #[test]
    fn col_to_byte_multibyte() {
        // "ab\u{00e9}cd" — \u{00e9} is 2 bytes, 1 display column.
        let s = "ab\u{00e9}cd";
        assert_eq!(col_to_byte_offset(s, 0), 0);
        assert_eq!(col_to_byte_offset(s, 2), 2);
        assert_eq!(col_to_byte_offset(s, 3), 4); // after 2-byte \u{00e9}
    }

    #[test]
    fn col_to_byte_past_end() {
        assert_eq!(col_to_byte_offset("hi", 10), 2);
    }

    #[test]
    fn col_to_byte_emoji() {
        // Emoji is 2 display columns. 'x' starts at column 2.
        let s = "\u{1F600}x";
        assert_eq!(col_to_byte_offset(s, 0), 0);
        assert_eq!(col_to_byte_offset(s, 2), 4);
    }

    #[test]
    fn col_to_byte_cjk() {
        // 世x = CJK (2 cols) + 'x' (1 col). 'x' at column 2, byte 3.
        let s = "\u{4e16}x";
        assert_eq!(col_to_byte_offset(s, 0), 0);
        assert_eq!(col_to_byte_offset(s, 2), 3);
    }

    #[test]
    fn col_to_byte_mixed() {
        // "ab世cd" = a(1) b(1) 世(2) c(1) d(1) = 6 cols.
        let s = "ab\u{4e16}cd";
        assert_eq!(col_to_byte_offset(s, 0), 0); // 'a'
        assert_eq!(col_to_byte_offset(s, 2), 2); // '世' at byte 2
        assert_eq!(col_to_byte_offset(s, 4), 5); // 'c' at byte 5 (after 3-byte 世)
    }

    // --- relative_time_short ---

    #[test]
    fn relative_time_short_boundaries() {
        // epoch=0, now=secs_ago → seconds_ago == secs_ago.
        let ago = |secs: i64| relative_time_short(0, secs);
        const DAY: i64 = 86_400;

        assert_eq!(relative_time_short(10, 0), "future"); // epoch after now
        assert_eq!(ago(0), "now");
        assert_eq!(ago(59), "now");
        assert_eq!(ago(60), "1m");
        assert_eq!(ago(59 * 60), "59m");
        assert_eq!(ago(60 * 60), "1h");
        assert_eq!(ago(17 * 3600), "17h");
        assert_eq!(ago(23 * 3600), "23h");
        assert_eq!(ago(DAY), "1d");
        assert_eq!(ago(6 * DAY), "6d");
        assert_eq!(ago(7 * DAY), "1w");
        assert_eq!(ago(29 * DAY), "4w"); // 29/30 month floors to 0 → weeks
        assert_eq!(ago(30 * DAY), "1mo");
        assert_eq!(ago(120 * DAY), "4mo");
        assert_eq!(ago(360 * DAY), "12mo"); // 30-day month, 365-day year
        assert_eq!(ago(365 * DAY), "1y");
    }
}
