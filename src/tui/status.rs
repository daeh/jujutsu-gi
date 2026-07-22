//! Status-line severity model: one source of truth for the color of transient
//! status messages and of the "STATUS" help-bar marker. `StatusStack` wraps a
//! private message stack; each message renders in its own level color and the
//! marker takes the most-severe level present.

use ratatui::prelude::*;

/// Severity of a transient status-line message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StatusLevel {
    Error,
    Warning,
    Info,
    Success,
}

impl StatusLevel {
    /// Message/marker color. All ANSI-16 named colors, so the palette follows
    /// the terminal theme (no hardcoded RGB). `Error` reuses the app's red — also
    /// the stale-workspace color; it renders orange-ish in most themes.
    pub(crate) fn color(self) -> Color {
        match self {
            Self::Error => Color::Red,
            Self::Warning => Color::Yellow,
            Self::Info => Color::LightBlue,
            Self::Success => Color::LightGreen,
        }
    }

    /// Attention severity for the STATUS marker when messages of different
    /// levels coexist: the most severe wins, so a problem is never hidden.
    /// Severity order Error > Warning > Info > Success (Success = the all-clear =
    /// least severe).
    fn severity(self) -> u8 {
        match self {
            Self::Error => 3,
            Self::Warning => 2,
            Self::Info => 1,
            Self::Success => 0,
        }
    }
}

/// A transient status message paired with its severity. Private to the module.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StatusMessage {
    text: String,
    level: StatusLevel,
}

/// The transient status area: a stack of messages that persist until the user
/// clears them. The backing vector is private, so the message/marker color
/// contract lives here.
#[derive(Default)]
pub(crate) struct StatusStack {
    messages: Vec<StatusMessage>,
}

impl StatusStack {
    /// Replace the stack with a single message.
    pub(crate) fn set(&mut self, text: String, level: StatusLevel) {
        self.messages.clear();
        self.messages.push(StatusMessage { text, level });
    }

    /// Append a message, preserving existing ones — e.g. several errors from one
    /// batched op stack until the user clears them.
    pub(crate) fn append(&mut self, text: String, level: StatusLevel) {
        self.messages.push(StatusMessage { text, level });
    }

    pub(crate) fn clear(&mut self) {
        self.messages.clear();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.messages
            .iter()
            .map(|message| message.text.split('\n').count())
            .sum()
    }

    /// Level painting the STATUS marker: the most-severe present, or `None`
    /// when empty. Equals the message level in the common single-message case.
    pub(crate) fn marker_level(&self) -> Option<StatusLevel> {
        self.messages
            .iter()
            .map(|m| m.level)
            .max_by_key(|l| l.severity())
    }

    /// Styled lines — one per message, each in its own level color.
    pub(crate) fn lines(&self) -> Vec<Line<'_>> {
        self.messages
            .iter()
            .flat_map(|message| {
                message.text.split('\n').map(move |text| {
                    Line::from(Span::styled(
                        text,
                        Style::default().fg(message.level.color()),
                    ))
                })
            })
            .collect()
    }

    /// Newline-joined message text (for the clipboard copy).
    pub(crate) fn plain_text(&self) -> String {
        self.messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_level_has_its_color() {
        assert_eq!(StatusLevel::Error.color(), Color::Red);
        assert_eq!(StatusLevel::Warning.color(), Color::Yellow);
        assert_eq!(StatusLevel::Info.color(), Color::LightBlue);
        assert_eq!(StatusLevel::Success.color(), Color::LightGreen);
    }

    #[test]
    fn set_replaces_the_stack() {
        let mut s = StatusStack::default();
        s.append("a".into(), StatusLevel::Error);
        s.append("b".into(), StatusLevel::Error);
        s.set("only".into(), StatusLevel::Success);
        assert_eq!(s.len(), 1);
        assert_eq!(s.plain_text(), "only");
        assert_eq!(s.marker_level(), Some(StatusLevel::Success));
    }

    #[test]
    fn clear_empties_the_stack() {
        let mut s = StatusStack::default();
        s.set("x".into(), StatusLevel::Info);
        assert!(!s.is_empty());
        s.clear();
        assert!(s.is_empty());
        assert_eq!(s.marker_level(), None);
    }

    #[test]
    fn append_preserves_prior_messages() {
        let mut s = StatusStack::default();
        s.set("done".into(), StatusLevel::Success);
        s.append("boom".into(), StatusLevel::Error);
        assert_eq!(s.len(), 2);
        assert_eq!(s.plain_text(), "done\nboom");
    }

    #[test]
    fn marker_level_is_most_severe() {
        assert_eq!(StatusStack::default().marker_level(), None);

        let mut single = StatusStack::default();
        single.set("s".into(), StatusLevel::Success);
        assert_eq!(single.marker_level(), Some(StatusLevel::Success));

        let mut success_then_error = StatusStack::default();
        success_then_error.set("ok".into(), StatusLevel::Success);
        success_then_error.append("err".into(), StatusLevel::Error);
        assert_eq!(success_then_error.marker_level(), Some(StatusLevel::Error));

        let mut info_then_warning = StatusStack::default();
        info_then_warning.set("fyi".into(), StatusLevel::Info);
        info_then_warning.append("careful".into(), StatusLevel::Warning);
        assert_eq!(info_then_warning.marker_level(), Some(StatusLevel::Warning));
    }

    #[test]
    fn marker_level_ignores_insertion_order() {
        let mut a = StatusStack::default();
        a.set("s".into(), StatusLevel::Success);
        a.append("i".into(), StatusLevel::Info);
        assert_eq!(a.marker_level(), Some(StatusLevel::Info));

        let mut b = StatusStack::default();
        b.set("i".into(), StatusLevel::Info);
        b.append("s".into(), StatusLevel::Success);
        assert_eq!(b.marker_level(), Some(StatusLevel::Info));
    }

    #[test]
    fn lines_color_each_message_by_its_own_level() {
        let mut s = StatusStack::default();
        s.set("ok".into(), StatusLevel::Success);
        s.append("err".into(), StatusLevel::Error);
        let lines = s.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::LightGreen));
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn plain_text_joins_with_newline() {
        let mut s = StatusStack::default();
        s.set("one".into(), StatusLevel::Info);
        s.append("two".into(), StatusLevel::Info);
        assert_eq!(s.plain_text(), "one\ntwo");
    }

    #[test]
    fn multiline_message_uses_one_render_line_per_input_line() {
        let mut s = StatusStack::default();
        s.set("one\ntwo\nthree".into(), StatusLevel::Error);
        assert_eq!(s.len(), 3);
        let lines = s.lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].spans[0].content, "one");
        assert_eq!(lines[1].spans[0].content, "two");
        assert_eq!(lines[2].spans[0].content, "three");
        assert_eq!(s.plain_text(), "one\ntwo\nthree");
    }
}
