//! A single-line prompt overlay (SPEC §7.5 "prompt line") - the bottom-docked text
//! input behind save-as and, later, any other "type one value and commit it" flow
//! (rename, go-to-line).
//!
//! It is the simplest kind of [`Layer`]: no list, no fuzzy matching, just an editable
//! line docked to the screen's bottom row. Type to edit, Enter to commit, Esc to
//! cancel. A commit runs the prompt's `submit` function on the typed text and emits
//! whatever [`Command`]s it returns - the §7.5 seam rule, so the committed value
//! reaches the core through the identical dispatch path a bound key uses; navigating
//! *inside* the prompt (typing, backspace) never crosses the seam.
//!
//! Like [`crate::picker`], this is pure logic with no terminal I/O, so it is
//! unit-testable end to end against synthetic [`KeyEvent`]s (SPEC §13).

use std::path::PathBuf;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;
use vortex_core::Action;

use crate::command::Command;
use crate::compositor::{EventResult, Layer};
use crate::config::Theme;

/// Turns the prompt's committed text into the commands to dispatch (SPEC §7.5). An
/// empty result commits nothing (e.g. blank input) while still closing the prompt.
type Submit = Box<dyn Fn(&str) -> Vec<Command>>;

/// A one-line text prompt: a fixed `prefix` label followed by the editable `input`,
/// with a `submit` that turns the committed text into the commands to dispatch.
pub struct Prompt {
    /// The label shown before the input (e.g. `"Save as: "`). Not editable.
    prefix: String,
    /// The text the user is typing (appended / backspaced; caret sits at its end).
    input: String,
    /// Turns the committed text into the commands to emit.
    submit: Submit,
    style: Style,
    finished: bool,
    /// Commands committed on Enter, drained by [`Layer::take_commands`].
    outbox: Vec<Command>,
}

impl Prompt {
    /// A prompt labelled `prefix`, pre-filled with `initial`, that runs `submit` on
    /// the text when the user commits it.
    pub fn new(
        prefix: impl Into<String>,
        initial: impl Into<String>,
        style: Style,
        submit: impl Fn(&str) -> Vec<Command> + 'static,
    ) -> Self {
        Self {
            prefix: prefix.into(),
            input: initial.into(),
            submit: Box::new(submit),
            style,
            finished: false,
            outbox: Vec::new(),
        }
    }

    /// The prompt's row: the screen's bottom line (where the status bar normally
    /// sits, which the prompt takes over while it is open). `None` when the screen
    /// has no rows, so the caller draws nothing.
    fn row(screen: Rect) -> Option<Rect> {
        (screen.height > 0 && screen.width > 0)
            .then(|| Rect::new(screen.x, screen.bottom() - 1, screen.width, 1))
    }
}

impl Layer for Prompt {
    fn render(&self, screen: Rect, buf: &mut Buffer) {
        let Some(row) = Self::row(screen) else {
            return;
        };
        let line = format!("{}{}", self.prefix, self.input);
        buf.set_style(row, self.style);
        buf.set_stringn(row.x, row.y, &line, row.width as usize, self.style);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        // A Ctrl/Cmd chord is a keybinding, not prompt input: defer it (Ignored) so
        // the shortcut runs and the loop dismisses the prompt - the same rule the
        // picker follows, keeping the keymap the single source of shortcuts (§7.5).
        if key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER)
        {
            return EventResult::Ignored;
        }
        match key.code {
            KeyCode::Esc => self.finished = true,
            KeyCode::Enter => {
                self.outbox.extend((self.submit)(&self.input));
                self.finished = true;
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            // Alt passes through for composed accented input; Ctrl/Cmd already
            // returned above.
            KeyCode::Char(c) => self.input.push(c),
            // Modal: swallow anything else so it never reaches the editor beneath.
            _ => {}
        }
        EventResult::Consumed
    }

    fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.outbox)
    }

    fn cursor(&self, screen: Rect) -> Option<Position> {
        let row = Self::row(screen)?;
        // Caret after the prefix plus the typed text, clamped to the last cell so it
        // never leaves the row when the input overruns the width.
        let col = self.prefix.width() + self.input.width();
        let x = (row.x as usize + col).min(row.right().saturating_sub(1) as usize) as u16;
        Some(Position::new(x, row.y))
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    fn restyle(&mut self, theme: &Theme) {
        self.style = theme.palette;
    }
}

/// A save-as prompt: pre-filled with the buffer's current path (so a save-as is a
/// quick edit of the existing name, not retyping it) and committing an
/// [`Action::SaveAs`] for the entered path. Blank input commits nothing - there is
/// no meaningful "save to no name" - so the prompt just closes.
pub fn save_as(theme: &Theme, current_path: Option<&std::path::Path>) -> Box<dyn Layer> {
    let initial = current_path
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    Box::new(Prompt::new("Save as: ", initial, theme.palette, |text| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            Vec::new()
        } else {
            vec![Command::Editor(Action::SaveAs(PathBuf::from(trimmed)))]
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(p: &mut Prompt, s: &str) {
        for c in s.chars() {
            p.handle_key(key(c));
        }
    }

    /// A bare prompt that echoes its text back as an `Insert` command, so a test can
    /// assert on the committed value without going through save-as path building.
    fn echo_prompt() -> Prompt {
        Prompt::new("> ", "", Style::default(), |text| {
            vec![Command::Editor(Action::Insert(text.to_string()))]
        })
    }

    #[test]
    fn typing_and_backspace_edit_the_input() {
        let mut p = echo_prompt();
        type_str(&mut p, "abc");
        assert_eq!(p.input, "abc");
        p.handle_key(press(KeyCode::Backspace));
        assert_eq!(p.input, "ab");
        // Backspace on an empty input is a harmless no-op, not a panic.
        let mut empty = echo_prompt();
        empty.handle_key(press(KeyCode::Backspace));
        assert_eq!(empty.input, "");
    }

    #[test]
    fn enter_commits_the_submit_result_and_finishes() {
        let mut p = echo_prompt();
        type_str(&mut p, "hi");
        assert_eq!(p.handle_key(press(KeyCode::Enter)), EventResult::Consumed);
        assert!(p.is_finished());
        assert_eq!(
            p.take_commands(),
            vec![Command::Editor(Action::Insert("hi".into()))]
        );
    }

    #[test]
    fn esc_cancels_with_no_command() {
        let mut p = echo_prompt();
        type_str(&mut p, "discard me");
        p.handle_key(press(KeyCode::Esc));
        assert!(p.is_finished());
        assert!(p.take_commands().is_empty());
    }

    #[test]
    fn a_ctrl_chord_is_deferred_not_typed() {
        // A Ctrl chord is a shortcut, not input: the prompt ignores it (so the loop
        // runs the binding and dismisses the prompt) and never adds it to the text.
        let mut p = echo_prompt();
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(p.handle_key(ctrl_s), EventResult::Ignored);
        assert!(p.input.is_empty());
        assert!(!p.is_finished());
    }

    #[test]
    fn other_keys_are_swallowed_so_the_editor_never_sees_them() {
        let mut p = echo_prompt();
        assert_eq!(p.handle_key(press(KeyCode::Down)), EventResult::Consumed);
        assert!(p.input.is_empty());
        assert!(!p.is_finished());
    }

    #[test]
    fn save_as_prefills_the_current_path_and_commits_save_as() {
        let theme = Theme::default();
        let mut layer = save_as(&theme, Some(std::path::Path::new("dir/file.rs")));
        // Pre-filled: committing straight away saves to the existing path.
        let commands = {
            layer.handle_key(press(KeyCode::Enter));
            layer.take_commands()
        };
        assert_eq!(
            commands,
            vec![Command::Editor(Action::SaveAs(PathBuf::from(
                "dir/file.rs"
            )))]
        );
    }

    #[test]
    fn save_as_edits_the_prefilled_path_before_committing() {
        let theme = Theme::default();
        // Down-cast is not needed: drive it as a boxed Layer the way the compositor
        // does. Backspace off ".rs" and retype ".md".
        let mut layer = save_as(&theme, Some(std::path::Path::new("a.rs")));
        for _ in 0..2 {
            layer.handle_key(press(KeyCode::Backspace)); // "rs"
        }
        for c in "md".chars() {
            layer.handle_key(key(c));
        }
        layer.handle_key(press(KeyCode::Enter));
        assert_eq!(
            layer.take_commands(),
            vec![Command::Editor(Action::SaveAs(PathBuf::from("a.md")))]
        );
    }

    #[test]
    fn save_as_with_no_current_path_starts_empty_and_blank_commits_nothing() {
        let theme = Theme::default();
        let mut layer = save_as(&theme, None);
        // Committing an empty prompt saves nothing (no meaningful target) but still
        // closes the prompt.
        layer.handle_key(press(KeyCode::Enter));
        assert!(layer.is_finished());
        assert!(layer.take_commands().is_empty());
    }

    #[test]
    fn save_as_ignores_surrounding_whitespace() {
        let theme = Theme::default();
        let mut layer = save_as(&theme, None);
        // Leading/trailing spaces are trimmed; a path is not silently created with
        // stray whitespace in its name.
        for c in "  spaced.txt  ".chars() {
            layer.handle_key(key(c));
        }
        layer.handle_key(press(KeyCode::Enter));
        assert_eq!(
            layer.take_commands(),
            vec![Command::Editor(Action::SaveAs(PathBuf::from("spaced.txt")))]
        );
    }

    #[test]
    fn renders_the_prefix_and_input_on_the_bottom_row_with_the_caret() {
        let mut p = echo_prompt();
        type_str(&mut p, "note.txt");
        let mut terminal = Terminal::new(TestBackend::new(30, 5)).unwrap();
        terminal
            .draw(|frame| p.render(frame.area(), frame.buffer_mut()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        // The prompt paints the bottom row (row 4 of 0..5).
        let row = crate::testutil::row_text(&buf, 4);
        assert!(row.starts_with("> note.txt"), "prompt row: {row:?}");
        // The caret sits just after "> note.txt" (2 + 8 = 10) on that row.
        assert_eq!(p.cursor(Rect::new(0, 0, 30, 5)), Some(Position::new(10, 4)));
    }

    #[test]
    fn a_zero_height_screen_draws_nothing_and_shows_no_cursor() {
        let p = echo_prompt();
        let screen = Rect::new(0, 0, 0, 0);
        assert_eq!(p.cursor(screen), None);
        let mut buf = Buffer::empty(screen);
        p.render(screen, &mut buf);
        assert_eq!(buf, Buffer::empty(screen));
    }

    #[test]
    fn the_caret_clamps_to_the_row_when_the_input_overruns_the_width() {
        let mut p = echo_prompt();
        type_str(&mut p, "a-very-long-path-that-overflows");
        // On a 10-wide screen the caret cannot leave the row: it clamps to col 9.
        assert_eq!(p.cursor(Rect::new(0, 0, 10, 3)), Some(Position::new(9, 2)));
    }

    #[test]
    fn restyle_adopts_the_new_themes_palette_style() {
        let mut p = echo_prompt();
        let theme = Theme {
            palette: Style::new().bg(Color::Rgb(9, 8, 7)),
            ..Theme::default()
        };
        p.restyle(&theme);
        assert_eq!(p.style, theme.palette);
    }
}
