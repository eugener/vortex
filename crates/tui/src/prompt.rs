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

/// A yes/no confirmation on the prompt line: a question plus a single keypress.
///
/// The sibling of [`Prompt`] rather than a special case of it - a confirmation is
/// answered with one key, not typed and submitted, and making the destructive answer
/// require Enter would be the wrong shape for "are you sure". `y` commits, anything
/// else (`n`, Esc, a stray key) cancels, so the safe answer is every answer but one.
pub struct Confirm {
    question: String,
    /// Emitted if the user says yes; dropped otherwise.
    on_yes: Vec<Command>,
    style: Style,
    finished: bool,
    outbox: Vec<Command>,
}

impl Confirm {
    pub fn new(question: impl Into<String>, on_yes: Vec<Command>, style: Style) -> Self {
        Self {
            question: question.into(),
            on_yes,
            style,
            finished: false,
            outbox: Vec::new(),
        }
    }
}

impl Layer for Confirm {
    fn render(&self, screen: Rect, buf: &mut Buffer) {
        let Some(row) = Prompt::row(screen) else {
            return;
        };
        buf.set_style(row, self.style);
        buf.set_stringn(row.x, row.y, &self.question, row.width as usize, self.style);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        // A Ctrl/Cmd chord is a shortcut, deferred as in `Prompt` so the keymap stays
        // the single source of bindings.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER)
        {
            return EventResult::Ignored;
        }
        if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            self.outbox.append(&mut self.on_yes);
        }
        // Every other key answers no. Nothing is swallowed silently into a "keep
        // asking" state: one keypress always closes the question.
        self.finished = true;
        EventResult::Consumed
    }

    fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.outbox)
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    fn restyle(&mut self, theme: &Theme) {
        self.style = theme.palette;
    }
}

/// Ask whether to close a buffer with unsaved changes, committing a forced
/// [`Action::CloseBuffer`] if the user accepts (SPEC §8: the core refused the
/// unforced close precisely so this question gets asked).
pub fn confirm_close(
    theme: &Theme,
    id: vortex_core::BufferId,
    path: Option<&std::path::Path>,
) -> Box<dyn Layer> {
    // The unnamed case reads "this buffer" rather than the `[No Name]` placeholder,
    // because this is a sentence rather than a label.
    let name = path.map_or_else(
        || "this buffer".to_string(),
        |p| crate::layout::buffer_display_name(Some(p), false),
    );
    Box::new(Confirm::new(
        format!("{name} has unsaved changes. Close anyway? (y/N) "),
        vec![Command::Editor(Action::CloseBuffer { id, force: true })],
        theme.palette,
    ))
}

/// Ask whether to discard unsaved edits in favour of the file's own contents,
/// after the core reported that it changed underneath (SPEC §10.2).
///
/// The mirror image of [`confirm_close`], and refused by the core for the same
/// reason: a reload throws away work, so the question has to be asked. Declining
/// keeps the buffer - the file's version is still on disk, so nothing is lost by
/// saying no, which is why "no" is the default here too.
pub fn confirm_reload(
    theme: &Theme,
    id: vortex_core::BufferId,
    path: Option<&std::path::Path>,
) -> Box<dyn Layer> {
    let name = path.map_or_else(
        || "this buffer".to_string(),
        |p| crate::layout::buffer_display_name(Some(p), false),
    );
    Box::new(Confirm::new(
        format!("{name} changed on disk. Discard your changes and reload? (y/N) "),
        vec![Command::Editor(Action::Reload { id, force: true })],
        theme.palette,
    ))
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

    // --- Confirm (the close guard's other half, SPEC §8) ---------------------

    fn close_confirm() -> Box<dyn Layer> {
        confirm_close(
            &Theme::default(),
            vortex_core::BufferId(7),
            Some(std::path::Path::new("dir/notes.md")),
        )
    }

    #[test]
    fn confirming_a_close_commits_a_forced_close_for_that_buffer() {
        let mut layer = close_confirm();
        assert_eq!(layer.handle_key(key('y')), EventResult::Consumed);
        assert!(layer.is_finished());
        assert_eq!(
            layer.take_commands(),
            vec![Command::Editor(Action::CloseBuffer {
                id: vortex_core::BufferId(7),
                force: true,
            })]
        );
    }

    #[test]
    fn an_uppercase_yes_confirms_too() {
        let mut layer = close_confirm();
        layer.handle_key(key('Y'));
        assert_eq!(layer.take_commands().len(), 1);
    }

    #[test]
    fn confirming_a_reload_commits_a_forced_reload_for_that_buffer() {
        // The other half of the external-change conflict (SPEC §10.2): the core
        // refused the unforced reload precisely so this question gets asked.
        let mut layer = confirm_reload(
            &Theme::default(),
            vortex_core::BufferId(3),
            Some(std::path::Path::new("dir/notes.md")),
        );
        assert_eq!(layer.handle_key(key('y')), EventResult::Consumed);
        assert!(layer.is_finished());
        assert_eq!(
            layer.take_commands(),
            vec![Command::Editor(Action::Reload {
                id: vortex_core::BufferId(3),
                force: true,
            })]
        );
    }

    #[test]
    fn declining_a_reload_keeps_the_buffer() {
        // Saying no must commit nothing at all - the file's version is still on
        // disk, so declining loses nothing, which is why it is the default.
        let mut layer = confirm_reload(&Theme::default(), vortex_core::BufferId(3), None);
        layer.handle_key(press(KeyCode::Esc));
        assert!(layer.is_finished());
        assert!(layer.take_commands().is_empty());
    }

    #[test]
    fn anything_but_yes_cancels_the_close() {
        // The destructive answer is exactly one key; every other key - the explicit
        // no, Esc, or a mistyped letter - closes the question having discarded nothing.
        for cancel in [
            key('n'),
            press(KeyCode::Esc),
            key('q'),
            press(KeyCode::Enter),
        ] {
            let mut layer = close_confirm();
            assert_eq!(layer.handle_key(cancel), EventResult::Consumed);
            assert!(layer.is_finished(), "one keypress always answers");
            assert!(
                layer.take_commands().is_empty(),
                "no close was committed for {cancel:?}"
            );
        }
    }

    #[test]
    fn a_ctrl_chord_over_the_confirm_is_deferred() {
        // Same rule as the prompt: a shortcut is not an answer, so the keymap stays
        // the single source of bindings.
        let mut layer = close_confirm();
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(layer.handle_key(ctrl_s), EventResult::Ignored);
        assert!(!layer.is_finished());
        assert!(layer.take_commands().is_empty());
    }

    #[test]
    fn the_confirm_names_the_file_it_would_discard() {
        // The question has to say *what* is about to be lost, not just ask.
        let layer = close_confirm();
        let mut terminal = Terminal::new(TestBackend::new(60, 4)).unwrap();
        terminal
            .draw(|frame| layer.render(frame.area(), frame.buffer_mut()))
            .unwrap();
        let row = crate::testutil::row_text(&terminal.backend().buffer().clone(), 3);
        assert!(row.contains("notes.md"), "confirm row: {row:?}");
        assert!(row.contains("y/N"), "confirm row: {row:?}");
        // A confirmation takes no text, so it shows no caret.
        assert_eq!(layer.cursor(Rect::new(0, 0, 60, 4)), None);
    }

    #[test]
    fn an_unnamed_buffer_still_gets_a_readable_question() {
        let layer = confirm_close(&Theme::default(), vortex_core::BufferId(1), None);
        let mut terminal = Terminal::new(TestBackend::new(60, 3)).unwrap();
        terminal
            .draw(|frame| layer.render(frame.area(), frame.buffer_mut()))
            .unwrap();
        let row = crate::testutil::row_text(&terminal.backend().buffer().clone(), 2);
        assert!(row.contains("this buffer"), "confirm row: {row:?}");
    }

    #[test]
    fn a_zero_height_screen_draws_no_confirm() {
        let layer = close_confirm();
        let screen = Rect::new(0, 0, 0, 0);
        let mut buf = Buffer::empty(screen);
        layer.render(screen, &mut buf);
        assert_eq!(buf, Buffer::empty(screen));
    }

    #[test]
    fn restyling_the_confirm_adopts_the_new_palette() {
        let mut layer = Confirm::new("ok? ", Vec::new(), Style::default());
        let theme = Theme {
            palette: Style::new().bg(Color::Rgb(1, 2, 3)),
            ..Theme::default()
        };
        layer.restyle(&theme);
        assert_eq!(layer.style, theme.palette);
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
