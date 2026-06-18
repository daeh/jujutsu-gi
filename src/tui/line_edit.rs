//! Single-line text editing over a `(String, cursor)` pair.
//!
//! Dialogs that hold several `String` fields and one shared byte-offset
//! cursor construct a `LineEditor` view over the active field to apply an
//! edit. All operations are grapheme-aware via `crate::text_utils`, so
//! multi-codepoint graphemes (e.g. 🇯🇵) edit as one user-perceived
//! character. Invariant: `cursor` enters and leaves every operation as a
//! grapheme-boundary byte offset `<= text.len()`. Field-specific semantics
//! (space bans, revision-cycle exits, selection) stay with the dialogs.

use crate::text_utils;

pub(crate) struct LineEditor<'a> {
    pub text: &'a mut String,
    pub cursor: &'a mut usize,
}

impl LineEditor<'_> {
    pub(crate) fn insert_char(&mut self, c: char) {
        self.text.insert(*self.cursor, c);
        *self.cursor += c.len_utf8();
    }

    /// Grapheme-aware backspace: deletes one user-perceived character
    /// before the cursor.
    pub(crate) fn backspace(&mut self) {
        if *self.cursor > 0 {
            let prev = text_utils::prev_grapheme_boundary(self.text, *self.cursor);
            self.text.drain(prev..*self.cursor);
            *self.cursor = prev;
        }
    }

    pub(crate) fn delete_to_start(&mut self) {
        if *self.cursor > 0 {
            self.text.drain(..*self.cursor);
            *self.cursor = 0;
        }
    }

    pub(crate) fn delete_to_end(&mut self) {
        self.text.truncate(*self.cursor);
    }

    /// Grapheme-safe word delete: ASCII-space-delimited words, never splits
    /// a multi-codepoint grapheme.
    pub(crate) fn delete_word_backward(&mut self) {
        if *self.cursor == 0 {
            return;
        }
        let new_pos = text_utils::prev_word_boundary(self.text, *self.cursor);
        self.text.drain(new_pos..*self.cursor);
        *self.cursor = new_pos;
    }

    pub(crate) fn move_left(&mut self) {
        if *self.cursor > 0 {
            *self.cursor = text_utils::prev_grapheme_boundary(self.text, *self.cursor);
        }
    }

    pub(crate) fn move_right(&mut self) {
        if *self.cursor < self.text.len() {
            *self.cursor = text_utils::next_grapheme_boundary(self.text, *self.cursor);
        }
    }

    pub(crate) fn move_word_backward(&mut self) {
        *self.cursor = text_utils::prev_word_boundary(self.text, *self.cursor);
    }

    pub(crate) fn move_word_forward(&mut self) {
        *self.cursor = text_utils::next_word_boundary(self.text, *self.cursor);
    }

    pub(crate) fn move_home(&mut self) {
        *self.cursor = 0;
    }

    pub(crate) fn move_end(&mut self) {
        *self.cursor = self.text.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(text: &str, cursor: usize) -> (String, usize) {
        (text.to_string(), cursor)
    }

    fn ed<'a>(text: &'a mut String, cursor: &'a mut usize) -> LineEditor<'a> {
        LineEditor { text, cursor }
    }

    #[test]
    fn insert_ascii_advances_cursor() {
        let (mut t, mut c) = edit("ab", 1);
        ed(&mut t, &mut c).insert_char('x');
        assert_eq!(t, "axb");
        assert_eq!(c, 2);
    }

    #[test]
    fn insert_multibyte_advances_by_utf8_len() {
        let (mut t, mut c) = edit("ab", 1);
        ed(&mut t, &mut c).insert_char('é'); // 2 bytes
        assert_eq!(t, "aéb");
        assert_eq!(c, 3);
    }

    #[test]
    fn backspace_removes_whole_grapheme() {
        // 🇯🇵 is two regional-indicator codepoints (8 UTF-8 bytes) forming
        // one user-perceived character.
        let (mut t, mut c) = edit("a🇯🇵", 9);
        ed(&mut t, &mut c).backspace();
        assert_eq!(t, "a");
        assert_eq!(c, 1);
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let (mut t, mut c) = edit("ab", 0);
        ed(&mut t, &mut c).backspace();
        assert_eq!(t, "ab");
        assert_eq!(c, 0);
    }

    #[test]
    fn delete_to_start_clears_prefix() {
        let (mut t, mut c) = edit("hello", 3);
        ed(&mut t, &mut c).delete_to_start();
        assert_eq!(t, "lo");
        assert_eq!(c, 0);
    }

    #[test]
    fn delete_to_end_truncates_suffix() {
        let (mut t, mut c) = edit("hello", 3);
        ed(&mut t, &mut c).delete_to_end();
        assert_eq!(t, "hel");
        assert_eq!(c, 3);
    }

    #[test]
    fn delete_word_backward_removes_last_word() {
        let (mut t, mut c) = edit("foo bar", 7);
        ed(&mut t, &mut c).delete_word_backward();
        assert_eq!(t, "foo ");
        assert_eq!(c, 4);
    }

    #[test]
    fn delete_word_backward_at_start_is_noop() {
        let (mut t, mut c) = edit("foo", 0);
        ed(&mut t, &mut c).delete_word_backward();
        assert_eq!(t, "foo");
        assert_eq!(c, 0);
    }

    #[test]
    fn moves_are_grapheme_aware_and_clamped() {
        let (mut t, mut c) = edit("a🇯🇵b", 1);
        ed(&mut t, &mut c).move_right(); // over the flag
        assert_eq!(c, 9);
        ed(&mut t, &mut c).move_left(); // back over the flag
        assert_eq!(c, 1);
        ed(&mut t, &mut c).move_left();
        assert_eq!(c, 0);
        ed(&mut t, &mut c).move_left(); // clamped at start
        assert_eq!(c, 0);
        ed(&mut t, &mut c).move_end();
        assert_eq!(c, t.len());
        ed(&mut t, &mut c).move_right(); // clamped at end
        assert_eq!(c, t.len());
        ed(&mut t, &mut c).move_home();
        assert_eq!(c, 0);
    }

    #[test]
    fn word_moves_cross_space_runs() {
        let (mut t, mut c) = edit("foo  bar", 8);
        ed(&mut t, &mut c).move_word_backward();
        assert_eq!(c, 5); // start of "bar"
        ed(&mut t, &mut c).move_word_backward();
        assert_eq!(c, 0); // start of "foo"
        ed(&mut t, &mut c).move_word_forward();
        assert_eq!(c, 5); // next non-space after the space run
    }
}
