use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Wrap};

#[allow(dead_code)]
pub(crate) struct DiagramMeta {
    pub name: &'static str,
    pub converge: bool,
    pub squash: bool,
    pub close: bool,
}

#[allow(dead_code)]
pub(crate) struct DiagramInfo {
    pub meta: DiagramMeta,
    pub diagram: &'static str,
    pub note: &'static str,
}

impl DiagramInfo {
    /// Height needed for the info section: gap + diagram lines + gap + wrapped note lines.
    pub(crate) fn height(&self, area_width: u16) -> u16 {
        let diagram_lines = self.diagram.lines().count() as u16;
        let note_w = area_width.saturating_sub(2).max(1) as usize;
        let note_lines = (self.note.len().div_ceil(note_w)).max(1) as u16;
        1 + diagram_lines + 1 + note_lines
    }

    /// Draw diagram + note starting at `y_start` within `inner`.
    pub(crate) fn draw(&self, frame: &mut Frame, inner: Rect, y_start: u16) {
        let max_y = inner.y + inner.height;
        let mut info_y = y_start;

        // Diagram — one line per row, no wrapping.
        for line in self.diagram.lines() {
            if info_y >= max_y {
                break;
            }
            frame.render_widget(
                Paragraph::new(Span::styled(format!("  {line}"), Style::default().dim())),
                Rect::new(inner.x, info_y, inner.width, 1),
            );
            info_y += 1;
        }

        // Gap before note.
        info_y += 1;

        // Note — with dynamic line wrapping.
        let note_h = max_y.saturating_sub(info_y);
        if note_h > 0 {
            let note_w = inner.width.saturating_sub(2);
            frame.render_widget(
                Paragraph::new(Span::styled(self.note, Style::default().dim().italic()))
                    .wrap(Wrap { trim: false }),
                Rect::new(inner.x + 2, info_y, note_w, note_h),
            );
        }
    }
}

#[allow(dead_code)]
pub(crate) enum Diagram {
    Merge,
    MergeSquash,
    MergeClose,
    MergeSquashClose,
    FastForward,
    FastForwardTargetClose,
    Rebase,
    Detach,
    Abandon,
}

impl Diagram {
    pub(crate) fn info(&self) -> DiagramInfo {
        match self {
            Diagram::Merge => DiagramInfo {
                meta: DiagramMeta {
                    name: "merge",
                    converge: true,
                    squash: false,
                    close: false,
                },
                diagram: "\
| INITIAL |  t@: A \u{2192} B \u{2192} C
|  STATE  |  s@: \u{21B3} X \u{2192} Y \u{2192} Z

                         @
t@:  A \u{2192} B \u{2192} C ----\u{2198}   \u{2197} Nt
       \u{2198}             M
s@:      X \u{2192} Y \u{2192} Z \u{2197}   \u{2198} Ns
                         @",
                note: "merge (chiasma merge)",
            },
            Diagram::MergeSquash => DiagramInfo {
                meta: DiagramMeta {
                    name: "merge",
                    converge: true,
                    squash: true,
                    close: false,
                },
                diagram: "\
| INITIAL |  t@: A \u{2192} B \u{2192} C
|  STATE  |  s@: \u{21B3} X \u{2192} Y \u{2192} Z

                     @
t@:  A \u{2192} B \u{2192} C \u{2198}   \u{2197} Nt
       \u{2198}         M
s@:      X* ---\u{2197} \u{21B3} Ns
                   @",
                note: "squash merge",
            },
            Diagram::MergeClose => DiagramInfo {
                meta: DiagramMeta {
                    name: "merge",
                    converge: true,
                    squash: false,
                    close: true,
                },
                diagram: "\
| INITIAL |  t@: A \u{2192} B \u{2192} C
|  STATE  |  s@: \u{21B3} X \u{2192} Y \u{2192} Z

                   @
t@:  A \u{2192} B \u{2192} C  \u{2192}  M
     \u{21B3} X \u{2192} Y \u{2192} Z \u{2197}",
                note: "merge into target, close source",
            },
            Diagram::MergeSquashClose => DiagramInfo {
                meta: DiagramMeta {
                    name: "merge",
                    converge: true,
                    squash: true,
                    close: true,
                },
                diagram: "\
| INITIAL |  t@: A \u{2192} B \u{2192} C
|  STATE  |  s@: \u{21B3} X \u{2192} Y \u{2192} Z

                 @
t@:  A \u{2192} B \u{2192} C \u{2192} M
     \u{21B3} X* -----\u{2197}",
                note: "squash merge into target, close source",
            },
            Diagram::FastForward => DiagramInfo {
                meta: DiagramMeta {
                    name: "fast-forward",
                    converge: true,
                    squash: false,
                    close: false,
                },
                diagram: "\
| INITIAL |  t@: A \u{2192} B \u{2192} C
|  STATE  |  s@: \u{21B3} X \u{2192} Y \u{2192} Z

                     @
t@:  A \u{2192} X \u{2192} Y \u{2192} Z \u{2192} Nt
s@:              \u{21B3} Ns
                   @",
                note: "fast-forward (linearized merge)",
            },
            Diagram::FastForwardTargetClose => DiagramInfo {
                meta: DiagramMeta {
                    name: "fast-forward",
                    converge: true,
                    squash: false,
                    close: true,
                },
                diagram: "\
| INITIAL |  t@: A
|  STATE  |  s@: \u{21B3} X \u{2192} Y \u{2192} Z

                 @
t@:  A \u{2192} X \u{2192} Y \u{2192} Z",
                note: "fast-forward target to source, close source",
            },
            Diagram::Rebase => DiagramInfo {
                meta: DiagramMeta {
                    name: "rebase",
                    converge: true,
                    squash: false,
                    close: false,
                },
                diagram: "\
| INITIAL |  t@: A \u{2192} B \u{2192} C
|  STATE  |  s@: \u{21B3} X \u{2192} Y \u{2192} Z

                 @
t@:  A \u{2192} B \u{2192} C \u{2192} N
s@:          \u{21B3} X' \u{2192} Y' \u{2192} Z'
                         @",
                note: "rebase source onto target; linear history (t@N, s@Z')",
            },
            Diagram::Detach => DiagramInfo {
                meta: DiagramMeta {
                    name: "detach",
                    converge: false,
                    squash: false,
                    close: true,
                },
                diagram: "\
| INITIAL |  t@: A \u{2192} B \u{2192} C
|  STATE  |  s@: \u{21B3} X \u{2192} Y \u{2192} Z

            @
t@: A \u{2192} B \u{2192} C    (unchanged)
    \u{21B3} X \u{2192} Y \u{2192} Z",
                note: "workspace removed, revisions remain",
            },
            Diagram::Abandon => DiagramInfo {
                meta: DiagramMeta {
                    name: "abandon",
                    converge: false,
                    squash: false,
                    close: true,
                },
                diagram: "\
| INITIAL |  t@: A \u{2192} B \u{2192} C
|  STATE  |  s@: \u{21B3} X \u{2192} Y \u{2192} Z

            @
t@: A \u{2192} B \u{2192} C    (unchanged)",
                note: "workspace removed, revisions abandoned",
            },
        }
    }
}
