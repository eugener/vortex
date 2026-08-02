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
use ratatui::crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;
use vortex_core::Action;

use crate::command::Command;
use crate::compositor::{EventResult, Layer};
use crate::config::Theme;
// The keymap's `Command` is the *binding* side - what a chord names - while this
// module's `Command` is what a committed choice dispatches (see `picker`).
use crate::keymap::{Command as Bound, Context, Keymap};

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

    fn context(&self) -> Context {
        Context::Prompt
    }

    fn handle_key(&mut self, key: KeyEvent, bound: Option<Bound>) -> EventResult {
        match bound {
            Some(Bound::Cancel) => self.finished = true,
            Some(Bound::Accept) => {
                self.outbox.extend((self.submit)(&self.input));
                self.finished = true;
            }
            Some(Bound::DeleteBackward) => {
                self.input.pop();
            }
            // `nop`, and anything this context should not have been able to hold.
            Some(_) => {}
            // Unbound: a printable key is the input, and anything else falls through
            // to the context below (SPEC §10.5).
            None => match crate::keymap::text_key(&key) {
                Some(c) => self.input.push(c),
                None => return EventResult::Ignored,
            },
        }
        EventResult::Consumed
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, screen: Rect) -> EventResult {
        let on_row = Self::row(screen)
            .is_some_and(|row| row.contains(Position::new(mouse.column, mouse.row)));
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) && !on_row {
            // Clicking away means "never mind", the same as Esc. Nothing has been
            // committed, so nothing is lost by reading it that way.
            self.finished = true;
        }
        // A click *on* the row is swallowed and does nothing. Moving the caret to it
        // is the obvious next step, but the caret only ever sits at the end of the
        // input here - typing appends and backspace removes, with no way to be
        // anywhere else - so a mid-line caret has to arrive as a prompt-editing
        // change, not be smuggled in as a mouse one.
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

/// The answer hint a confirmation's question ends with, rendered from the `confirm`
/// context rather than written out as `(y/N)`.
///
/// **Only the committing chord is named.** Every other key declines, so spelling the
/// no key too would suggest it is the only way to say no; what the reader needs is the
/// one key that is not safe to press by accident.
///
/// Empty when `confirm_yes` is unbound - the question then has no answer that commits,
/// and advertising a key that does nothing is exactly the drift this rule exists to
/// stop (SPEC §10.5). The question still closes on any key, so nothing is stuck.
fn yes_hint(keymap: &Keymap) -> String {
    keymap
        .shortcut_for(Bound::ConfirmYes, Context::Confirm)
        .map(|chord| format!("({chord} to confirm) "))
        .unwrap_or_default()
}

/// A yes/no confirmation on the prompt line: a question plus a single keypress.
///
/// The sibling of [`Prompt`] rather than a special case of it - a confirmation is
/// answered with one key, not typed and submitted, and making the destructive answer
/// require Enter would be the wrong shape for "are you sure". `confirm_yes` commits,
/// anything else (`confirm_no`, `cancel`, a stray printable key) declines, so the safe
/// answer is every answer but one.
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

    fn context(&self) -> Context {
        Context::Confirm
    }

    fn handle_key(&mut self, key: KeyEvent, bound: Option<Bound>) -> EventResult {
        match bound {
            Some(Bound::ConfirmYes) => {
                self.outbox.append(&mut self.on_yes);
                self.finished = true;
            }
            // Cancel and an explicit no are the same answer, and so is any *unbound*
            // printable key: the safe answer is every answer but one, which is what
            // makes a mistyped key harmless. Nothing is swallowed silently into a
            // "keep asking" state - one keypress always closes the question.
            Some(Bound::ConfirmNo | Bound::Cancel) => self.finished = true,
            // `nop`, and anything this context should not have been able to hold.
            Some(_) => {}
            None => {
                if crate::keymap::is_command_chord(&key) {
                    // Somebody's shortcut, not an answer: the context below gets its
                    // turn, so Ctrl+S still saves while a question is up (SPEC §10.5).
                    return EventResult::Ignored;
                }
                // Everything else - a letter, Backspace, Tab, an arrow - is a decline.
                // A question is modal over the keyboard, so the *only* keys that leave
                // it are the ones that were never its to answer. Deferring by
                // printability instead would send Backspace to the editor, which
                // deletes a character behind the question and dismisses it unanswered.
                self.finished = true;
            }
        }
        EventResult::Consumed
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, _screen: Rect) -> EventResult {
        // Every answer but one is no, and a click is not that one: `y` is the only
        // way to commit something destructive, so a click anywhere - on the question
        // or away from it - closes it as a decline. Nothing here is worth a
        // misclick's worth of a discarded buffer.
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            self.finished = true;
        }
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
    keymap: &Keymap,
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
        format!(
            "{name} has unsaved changes. Close anyway? {}",
            yes_hint(keymap)
        ),
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
    keymap: &Keymap,
    id: vortex_core::BufferId,
    path: Option<&std::path::Path>,
) -> Box<dyn Layer> {
    let name = path.map_or_else(
        || "this buffer".to_string(),
        |p| crate::layout::buffer_display_name(Some(p), false),
    );
    Box::new(Confirm::new(
        format!(
            "{name} changed on disk. Discard your changes and reload? {}",
            yes_hint(keymap)
        ),
        vec![Command::Editor(Action::Reload { id, force: true })],
        theme.palette,
    ))
}

/// Ask whether to write over a file that changed underneath the buffer, after the
/// core refused the save (SPEC §8, §10.2).
///
/// The third of the family, and the one whose stakes point outward: closing or
/// reloading discards *your* work, while this discards someone else's. Declining
/// leaves both copies intact - the buffer keeps its edits and the file keeps its
/// own - so "no" is the default here as well.
///
/// A `removed` file is a different sentence. There is nothing to overwrite, and
/// saving is how the buffer gets written back, so the question is whether to
/// recreate it - which is usually yes, but is still the user's call, since the file
/// may have been deleted on purpose.
pub fn confirm_overwrite(
    theme: &Theme,
    keymap: &Keymap,
    path: Option<&std::path::Path>,
    removed: bool,
) -> Box<dyn Layer> {
    let name = path.map_or_else(
        || "this buffer".to_string(),
        |p| crate::layout::buffer_display_name(Some(p), false),
    );
    let hint = yes_hint(keymap);
    let question = if removed {
        format!("{name} was deleted. Save it back? {hint}")
    } else {
        format!("{name} changed on disk. Overwrite it with your version? {hint}")
    };
    Box::new(Confirm::new(
        question,
        vec![Command::Editor(Action::Save { force: true })],
        theme.palette,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::send;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::style::Color;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(p: &mut Prompt, s: &str) {
        for c in s.chars() {
            send(&mut *p, key(c));
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
        send(&mut p, press(KeyCode::Backspace));
        assert_eq!(p.input, "ab");
        // Backspace on an empty input is a harmless no-op, not a panic.
        let mut empty = echo_prompt();
        send(&mut empty, press(KeyCode::Backspace));
        assert_eq!(empty.input, "");
    }

    #[test]
    fn enter_commits_the_submit_result_and_finishes() {
        let mut p = echo_prompt();
        type_str(&mut p, "hi");
        assert_eq!(send(&mut p, press(KeyCode::Enter)), EventResult::Consumed);
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
        send(&mut p, press(KeyCode::Esc));
        assert!(p.is_finished());
        assert!(p.take_commands().is_empty());
    }

    #[test]
    fn a_ctrl_chord_is_deferred_not_typed() {
        // A Ctrl chord is a shortcut, not input: the prompt ignores it (so the loop
        // runs the binding and dismisses the prompt) and never adds it to the text.
        let mut p = echo_prompt();
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(send(&mut p, ctrl_s), EventResult::Ignored);
        assert!(p.input.is_empty());
        assert!(!p.is_finished());
    }

    #[test]
    fn a_key_the_prompt_context_does_not_bind_falls_through_to_the_editor() {
        // The prompt binds Enter, Esc, Backspace and Tab, and takes printable keys as
        // input. Everything else is not its business: it defers rather than swallowing,
        // which is what stops a config from locking the surface shut (SPEC §10.5) - an
        // unbound Esc would find `collapse_selections` beneath and close this prompt.
        let mut p = echo_prompt();
        assert_eq!(send(&mut p, press(KeyCode::Down)), EventResult::Ignored);
        assert!(p.input.is_empty());
        assert!(!p.is_finished());
    }

    #[test]
    fn save_as_prefills_the_current_path_and_commits_save_as() {
        let theme = Theme::default();
        let mut layer = save_as(&theme, Some(std::path::Path::new("dir/file.rs")));
        // Pre-filled: committing straight away saves to the existing path.
        let commands = {
            send(&mut *layer, press(KeyCode::Enter));
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
            send(&mut *layer, press(KeyCode::Backspace)); // "rs"
        }
        for c in "md".chars() {
            send(&mut *layer, key(c));
        }
        send(&mut *layer, press(KeyCode::Enter));
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
        send(&mut *layer, press(KeyCode::Enter));
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
            send(&mut *layer, key(c));
        }
        send(&mut *layer, press(KeyCode::Enter));
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
            &Keymap::default(),
            vortex_core::BufferId(7),
            Some(std::path::Path::new("dir/notes.md")),
        )
    }

    #[test]
    fn confirming_a_close_commits_a_forced_close_for_that_buffer() {
        let mut layer = close_confirm();
        assert_eq!(send(&mut *layer, key('y')), EventResult::Consumed);
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
    fn a_shifted_yes_is_not_the_yes_key() {
        // `shift+y` was an alias for `y` and is not a row any more - the question
        // renders its chord from the table, so an alias would decide what it says.
        // Shift+Y is an unbound printable key, which declines like every other.
        let mut layer = close_confirm();
        send(&mut *layer, key('Y'));
        assert!(layer.is_finished(), "one keypress always answers");
        assert!(layer.take_commands().is_empty(), "nothing was committed");
    }

    /// A left press at a screen cell.
    fn click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn clicking_away_from_a_prompt_cancels_it() {
        let screen = Rect::new(0, 0, 40, 10);
        let mut p = echo_prompt();
        type_str(&mut p, "half-typed");
        // Row 9 is the prompt itself; row 0 is the editor above it.
        assert_eq!(p.handle_mouse(click(5, 0), screen), EventResult::Consumed);
        assert!(p.is_finished());
        assert!(p.take_commands().is_empty(), "cancelling commits nothing");
    }

    #[test]
    fn clicking_on_the_prompt_row_leaves_it_open() {
        // The prompt's own row is not a dismissal - it is where the user is typing.
        let screen = Rect::new(0, 0, 40, 10);
        let mut p = echo_prompt();
        type_str(&mut p, "still typing");
        assert_eq!(p.handle_mouse(click(5, 9), screen), EventResult::Consumed);
        assert!(!p.is_finished());
    }

    #[test]
    fn a_click_never_answers_yes_to_a_confirmation() {
        // The destructive answer takes a deliberate `y`. A click closes the question
        // as a decline, wherever it lands - a misclick must not discard a buffer.
        let screen = Rect::new(0, 0, 40, 10);
        for (x, y) in [(5, 9), (5, 0)] {
            let mut layer = close_confirm();
            assert_eq!(
                layer.handle_mouse(click(x, y), screen),
                EventResult::Consumed
            );
            assert!(layer.is_finished());
            assert!(
                layer.take_commands().is_empty(),
                "a click at ({x},{y}) committed the close"
            );
        }
    }

    #[test]
    fn confirming_a_reload_commits_a_forced_reload_for_that_buffer() {
        // The other half of the external-change conflict (SPEC §10.2): the core
        // refused the unforced reload precisely so this question gets asked.
        let mut layer = confirm_reload(
            &Theme::default(),
            &Keymap::default(),
            vortex_core::BufferId(3),
            Some(std::path::Path::new("dir/notes.md")),
        );
        assert_eq!(send(&mut *layer, key('y')), EventResult::Consumed);
        assert!(layer.is_finished());
        assert_eq!(
            layer.take_commands(),
            vec![Command::Editor(Action::Reload {
                id: vortex_core::BufferId(3),
                force: true,
            })]
        );
    }

    /// The bottom row a confirmation paints - the question itself.
    fn question_row(layer: Box<dyn Layer>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(70, 3)).unwrap();
        terminal
            .draw(|frame| layer.render(frame.area(), frame.buffer_mut()))
            .unwrap();
        crate::testutil::row_text(&terminal.backend().buffer().clone(), 2)
    }

    #[test]
    fn confirming_an_overwrite_commits_a_forced_save() {
        // The core refused the unforced save precisely so this question gets asked -
        // and this is the one whose "yes" discards someone else's work rather than
        // the user's own.
        let mut layer = confirm_overwrite(
            &Theme::default(),
            &Keymap::default(),
            Some(std::path::Path::new("dir/shared.txt")),
            false,
        );
        assert_eq!(send(&mut *layer, key('y')), EventResult::Consumed);
        assert!(layer.is_finished());
        assert_eq!(
            layer.take_commands(),
            vec![Command::Editor(Action::Save { force: true })]
        );
    }

    #[test]
    fn declining_an_overwrite_writes_nothing() {
        // Both copies survive a "no": the buffer keeps its edits and the file keeps
        // its own, which is why "no" is the default.
        let mut layer = confirm_overwrite(&Theme::default(), &Keymap::default(), None, false);
        send(&mut *layer, press(KeyCode::Esc));
        assert!(layer.is_finished());
        assert!(layer.take_commands().is_empty());
    }

    #[test]
    fn a_deleted_file_is_asked_about_differently() {
        // Nothing to overwrite, so the question is whether to put it back - which is
        // usually yes, but is still the user's call.
        let deleted = confirm_overwrite(
            &Theme::default(),
            &Keymap::default(),
            Some(std::path::Path::new("gone.txt")),
            true,
        );
        let changed = confirm_overwrite(
            &Theme::default(),
            &Keymap::default(),
            Some(std::path::Path::new("gone.txt")),
            false,
        );
        assert!(question_row(deleted).contains("was deleted"));
        assert!(question_row(changed).contains("changed on disk"));
    }

    #[test]
    fn declining_a_reload_keeps_the_buffer() {
        // Saying no must commit nothing at all - the file's version is still on
        // disk, so declining loses nothing, which is why it is the default.
        let mut layer = confirm_reload(
            &Theme::default(),
            &Keymap::default(),
            vortex_core::BufferId(3),
            None,
        );
        send(&mut *layer, press(KeyCode::Esc));
        assert!(layer.is_finished());
        assert!(layer.take_commands().is_empty());
    }

    #[test]
    fn anything_but_yes_cancels_the_close() {
        // The destructive answer is exactly one key; every other *printable* key - the
        // explicit no, or a mistyped letter - closes the question having discarded
        // nothing, and so does Esc, which the `confirm` context binds to `cancel`.
        for cancel in [key('n'), press(KeyCode::Esc), key('q'), key('Z')] {
            let mut layer = close_confirm();
            assert_eq!(send(&mut *layer, cancel), EventResult::Consumed);
            assert!(layer.is_finished(), "one keypress always answers");
            assert!(
                layer.take_commands().is_empty(),
                "no close was committed for {cancel:?}"
            );
        }
    }

    #[test]
    fn a_question_is_modal_over_every_key_that_is_not_a_shortcut() {
        // Regression. "Every other key declines" was read as "every other *printable*
        // key", which sent Enter, Backspace and Tab to the context below - so Backspace
        // at "discard your changes?" deleted a character behind the question and
        // dismissed it unanswered, and Enter committed whatever overlay was buried
        // under the question. Only a command chord is not this surface's to answer.
        for key in [
            press(KeyCode::Enter),
            press(KeyCode::Backspace),
            press(KeyCode::Tab),
            press(KeyCode::Down),
        ] {
            let mut layer = close_confirm();
            assert_eq!(
                send(&mut *layer, key),
                EventResult::Consumed,
                "{key:?} escaped the question"
            );
            assert!(layer.is_finished(), "{key:?} left the question open");
            assert!(
                layer.take_commands().is_empty(),
                "{key:?} committed the close"
            );
        }
    }

    #[test]
    fn a_ctrl_chord_over_the_confirm_is_deferred() {
        // Same rule as the prompt: a shortcut is not an answer, so the keymap stays
        // the single source of bindings.
        let mut layer = close_confirm();
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(send(&mut *layer, ctrl_s), EventResult::Ignored);
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
        // Rendered from the `confirm` context rather than written as `(y/N)`, and only
        // the committing chord is named - every other key declines (SPEC §10.5).
        assert!(row.contains("(Y to confirm)"), "confirm row: {row:?}");
        // A confirmation takes no text, so it shows no caret.
        assert_eq!(layer.cursor(Rect::new(0, 0, 60, 4)), None);
    }

    #[test]
    fn an_unnamed_buffer_still_gets_a_readable_question() {
        let layer = confirm_close(
            &Theme::default(),
            &Keymap::default(),
            vortex_core::BufferId(1),
            None,
        );
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
