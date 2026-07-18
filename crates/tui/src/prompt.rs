//! A single-line text prompt overlay (SPEC §7.5 "prompt line") - the first concrete
//! compositor [`Layer`].
//!
//! It docks to the bottom row, captures typing locally, and on Enter turns its text
//! into an `Action` via a caller-supplied builder: the §7.5 seam rule in miniature -
//! navigation and editing stay frontend-local; only the *committed* intent crosses
//! to the core. Esc cancels with no action. Editing is grapheme-aware so multi-byte
//! input (CJK, emoji) moves and deletes by whole characters, never mid-codepoint
//! (SPEC §4).
//!
//! Today its one use is the file-open prompt ([`open_file`]), which builds an
//! existing `Action::Open` - no core change. A richer command surface (palette,
//! pickers) reuses this same layer machinery at M7.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Clear, Widget};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use vortex_core::Action;

use crate::compositor::{EventResult, Layer};

/// A modal single-line input. Consumes every key while open (so the editor beneath
/// never moves), editing an in-memory string with a grapheme-boundary cursor.
pub struct Prompt {
    /// Fixed leading text, e.g. `"Open: "`.
    label: String,
    /// The text the user has entered so far.
    input: String,
    /// Caret position as a byte offset into `input`, always on a grapheme boundary.
    cursor: usize,
    /// Turns the submitted text into an `Action`, or `None` to submit nothing (e.g.
    /// an empty path). A plain `fn` pointer - the builders capture no state.
    build: fn(&str) -> Option<Action>,
    /// Fill style for the prompt row (from the theme).
    style: Style,
    /// Set once the prompt is submitted or cancelled, so the compositor pops it.
    finished: bool,
    /// The action a submit produced, drained by the compositor via [`take_actions`].
    ///
    /// [`take_actions`]: Layer::take_actions
    committed: Option<Action>,
}

impl Prompt {
    /// A prompt showing `label`, styled with `style`, whose submitted text is turned
    /// into an action by `build`.
    fn new(label: impl Into<String>, style: Style, build: fn(&str) -> Option<Action>) -> Self {
        Self {
            label: label.into(),
            input: String::new(),
            cursor: 0,
            build,
            style,
            finished: false,
            committed: None,
        }
    }

    /// Byte offset of the grapheme boundary just before `cursor` (for Backspace and
    /// Left), or 0 at the start.
    fn prev_boundary(&self) -> usize {
        self.input[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// Byte offset of the grapheme boundary just after `cursor` (for Delete and
    /// Right), or the end at the last position.
    fn next_boundary(&self) -> usize {
        self.input[self.cursor..]
            .graphemes(true)
            .next()
            .map(|g| self.cursor + g.len())
            .unwrap_or(self.cursor)
    }
}

impl Layer for Prompt {
    fn render(&self, screen: Rect, buf: &mut Buffer) {
        if screen.width == 0 || screen.height == 0 {
            return;
        }
        // Dock to the bottom row, over the status bar. Clear it first so the editor
        // chrome beneath does not show through (the ratatui popup idiom, SPEC §7.5).
        let row = Rect::new(screen.x, screen.bottom() - 1, screen.width, 1);
        Clear.render(row, buf);
        buf.set_style(row, self.style);
        let text = format!("{}{}", self.label, self.input);
        buf.set_stringn(row.x, row.y, &text, row.width as usize, self.style);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        match key.code {
            // Cancel: close with no committed action.
            KeyCode::Esc => self.finished = true,
            // Submit: build the action (if any) and close.
            KeyCode::Enter => {
                self.committed = (self.build)(&self.input);
                self.finished = true;
            }
            KeyCode::Backspace => {
                let start = self.prev_boundary();
                self.input.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            KeyCode::Delete => {
                let end = self.next_boundary();
                self.input.replace_range(self.cursor..end, "");
            }
            KeyCode::Left => self.cursor = self.prev_boundary(),
            KeyCode::Right => self.cursor = self.next_boundary(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            // Insert printable text. A Ctrl/Cmd-modified char is a command chord, not
            // text (same rule as the keymap's text fallback), so it is swallowed
            // rather than typed. Alt is allowed through for composed accented input.
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::SUPER) =>
            {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                self.input.insert_str(self.cursor, s);
                self.cursor += s.len();
            }
            // Any other key is swallowed: the prompt is modal, so nothing leaks to
            // the editor beneath while it is open.
            _ => {}
        }
        EventResult::Consumed
    }

    fn take_actions(&mut self) -> Vec<Action> {
        self.committed.take().into_iter().collect()
    }

    fn cursor(&self, screen: Rect) -> Option<Position> {
        if screen.width == 0 || screen.height == 0 {
            return None;
        }
        // Caret sits after the label plus the display width of the text left of it,
        // clamped to the last visible column so a long entry keeps it on screen.
        let col = self.label.width() + self.input[..self.cursor].width();
        let x = (screen.x as usize + col).min(screen.right().saturating_sub(1) as usize) as u16;
        Some(Position::new(x, screen.bottom() - 1))
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

/// The file-open prompt: `Open: <path>`, submitting a non-empty path as
/// `Action::Open`. An empty or whitespace-only entry submits nothing (just closes).
pub fn open_file(style: Style) -> Box<dyn Layer> {
    Box::new(Prompt::new("Open: ", style, |text| {
        let path = text.trim();
        (!path.is_empty()).then(|| Action::Open(std::path::PathBuf::from(path)))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A test prompt that echoes its text into `Action::Insert` so submission is
    /// observable without touching the filesystem.
    fn echo_prompt() -> Prompt {
        Prompt::new("> ", Style::default(), |t| {
            (!t.is_empty()).then(|| Action::Insert(t.to_string()))
        })
    }

    #[test]
    fn typing_builds_the_input_and_consumes_keys() {
        let mut p = echo_prompt();
        assert_eq!(p.handle_key(key('h')), EventResult::Consumed);
        p.handle_key(key('i'));
        assert_eq!(p.input, "hi");
        assert_eq!(p.cursor, 2);
        assert!(!p.is_finished());
    }

    #[test]
    fn enter_commits_the_built_action_and_finishes() {
        let mut p = echo_prompt();
        for c in "abc".chars() {
            p.handle_key(key(c));
        }
        p.handle_key(press(KeyCode::Enter));
        assert!(p.is_finished());
        assert!(matches!(p.take_actions().as_slice(), [Action::Insert(s)] if s == "abc"));
    }

    #[test]
    fn esc_cancels_with_no_action() {
        let mut p = echo_prompt();
        p.handle_key(key('x'));
        p.handle_key(press(KeyCode::Esc));
        assert!(p.is_finished());
        assert!(p.take_actions().is_empty());
    }

    #[test]
    fn empty_submit_finishes_without_an_action() {
        let mut p = echo_prompt();
        p.handle_key(press(KeyCode::Enter));
        assert!(p.is_finished());
        assert!(p.take_actions().is_empty());
    }

    #[test]
    fn backspace_deletes_the_grapheme_before_the_cursor() {
        let mut p = echo_prompt();
        for c in "abc".chars() {
            p.handle_key(key(c));
        }
        p.handle_key(press(KeyCode::Backspace));
        assert_eq!(p.input, "ab");
        assert_eq!(p.cursor, 2);
    }

    #[test]
    fn editing_is_grapheme_aware_for_multibyte_input() {
        // A multi-byte character (é, 2 bytes) must delete as one unit, not one byte.
        let mut p = echo_prompt();
        p.handle_key(key('é'));
        assert_eq!(p.cursor, 2, "cursor advances by the char's byte length");
        p.handle_key(press(KeyCode::Backspace));
        assert_eq!(p.input, "");
        assert_eq!(p.cursor, 0);
    }

    #[test]
    fn left_then_insert_edits_mid_string() {
        let mut p = echo_prompt();
        for c in "ac".chars() {
            p.handle_key(key(c));
        }
        p.handle_key(press(KeyCode::Left)); // between a and c
        p.handle_key(key('b'));
        assert_eq!(p.input, "abc");
    }

    #[test]
    fn delete_removes_the_grapheme_at_the_cursor() {
        let mut p = echo_prompt();
        for c in "abc".chars() {
            p.handle_key(key(c));
        }
        p.handle_key(press(KeyCode::Home));
        p.handle_key(press(KeyCode::Delete));
        assert_eq!(p.input, "bc");
        assert_eq!(p.cursor, 0);
    }

    #[test]
    fn ctrl_modified_char_is_not_typed() {
        // A leftover command chord (e.g. Ctrl+S) must not insert its letter.
        let mut p = echo_prompt();
        p.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(p.input, "");
    }

    #[test]
    fn cursor_sits_after_the_label_and_text() {
        let mut p = echo_prompt(); // label "> " is 2 columns
        for c in "hi".chars() {
            p.handle_key(key(c));
        }
        let screen = Rect::new(0, 0, 40, 10);
        // 2 (label) + 2 (hi) = column 4 on the bottom row (row 9).
        assert_eq!(p.cursor(screen), Some(Position::new(4, 9)));
    }

    #[test]
    fn renders_label_and_text_on_the_bottom_row() {
        let mut p = echo_prompt();
        for c in "hi".chars() {
            p.handle_key(key(c));
        }
        let mut terminal = Terminal::new(TestBackend::new(20, 4)).unwrap();
        terminal
            .draw(|frame| p.render(frame.area(), frame.buffer_mut()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let bottom: String = (0..20)
            .map(|x| buf.cell((x, 3)).unwrap().symbol().to_string())
            .collect();
        assert!(bottom.starts_with("> hi"), "bottom row: {bottom:?}");
    }

    #[test]
    fn open_file_builds_an_open_action_for_a_path() {
        // The real factory: a non-empty path submits Action::Open; empty submits none.
        let mut layer = open_file(Style::default());
        for c in "a.txt".chars() {
            layer.handle_key(key(c));
        }
        layer.handle_key(press(KeyCode::Enter));
        let actions = layer.take_actions();
        assert!(
            matches!(actions.as_slice(), [Action::Open(p)] if p.ends_with("a.txt")),
            "actions: {actions:?}"
        );
    }
}
