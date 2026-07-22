//! A generic fuzzy picker overlay (SPEC §7.5) - a filter query over a list of named
//! items, each carrying the [`Command`] to run when it is chosen.
//!
//! This is the shared machinery behind the command palette ([`crate::palette`]) and
//! the file picker ([`crate::filepicker`]): they differ only in what fills the list
//! and what a pick runs, not in how you filter, move, and select. Type to filter
//! (via `nucleo`, Helix's matcher), Up/Down to move, Enter to run the highlighted
//! item, Esc to cancel. Picking emits that item's command - the §7.5 seam rule, so a
//! pick and a bound key run through the identical dispatch path.
//!
//! Filtering runs on this thread; `nucleo-matcher`'s `match_list` is meant for the
//! small-to-moderate lists here (a command set, a capped file walk). A very large
//! corpus would want the async high-level `nucleo` crate instead - deferred.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, Clear, Widget};
use unicode_width::UnicodeWidthStr;

use crate::command::Command;
use crate::compositor::{EventResult, Layer};
use crate::config::Theme;

/// One selectable row: a user-facing label, an optional shortcut to show
/// right-aligned (the key that runs it, if any), and what running it does.
pub struct Item {
    pub label: String,
    pub shortcut: Option<String>,
    pub command: Command,
}

/// A label paired with its index, so `nucleo`'s `match_list` (which needs
/// `AsRef<str>` haystacks) hands the index straight back after ranking - no lookup.
struct Ranked<'a> {
    idx: usize,
    label: &'a str,
}

impl AsRef<str> for Ranked<'_> {
    fn as_ref(&self) -> &str {
        self.label
    }
}

/// A fuzzy picker: a titled box with a filter query, the ranked subset of items
/// matching it, and the highlighted row.
pub struct Picker {
    title: String,
    items: Vec<Item>,
    /// The current filter text (appended/backspaced; a filter, not a full editor).
    query: String,
    /// Indices into `items`, ranked by `query` (all, in order, when empty).
    filtered: Vec<usize>,
    /// Row into `filtered` that is highlighted.
    selected: usize,
    matcher: Matcher,
    style: Style,
    selected_style: Style,
    finished: bool,
    /// Commands the picker has committed, drained by [`Layer::take_commands`].
    /// A list, not a single slot, because a previewing picker emits as you move.
    outbox: Vec<Command>,
    /// Set by [`Self::previewing`]: the command that undoes a preview. Its presence
    /// is what turns preview mode on.
    cancel: Option<Command>,
    /// Item last previewed, so a key that leaves the highlight where it was does not
    /// re-emit (typing a filter that does not move it, Up at the top, …).
    previewed: Option<usize>,
}

impl Picker {
    /// A picker titled `title` over `items`. `match_paths` tunes the matcher for
    /// path-shaped haystacks (a file picker) versus plain labels (a command palette).
    pub fn new(
        title: impl Into<String>,
        items: Vec<Item>,
        match_paths: bool,
        style: Style,
        selected_style: Style,
    ) -> Self {
        let config = if match_paths {
            Config::DEFAULT.match_paths()
        } else {
            Config::DEFAULT
        };
        let filtered = (0..items.len()).collect();
        Self {
            title: title.into(),
            items,
            query: String::new(),
            filtered,
            selected: 0,
            matcher: Matcher::new(config),
            style,
            selected_style,
            finished: false,
            outbox: Vec::new(),
            cancel: None,
            previewed: None,
        }
    }

    /// Start with row `index` highlighted instead of the first - so a picker over
    /// "which of these is in use" opens on the one that is.
    pub fn with_selected(mut self, index: usize) -> Self {
        self.selected = index.min(self.filtered.len().saturating_sub(1));
        self
    }

    /// Preview as the highlight moves: every move emits the newly highlighted item's
    /// command, and Esc emits `cancel` to undo it. Opening previews nothing - only
    /// moving does - so the picker is free to open over an unrelated state.
    ///
    /// Escaping is the *only* undo: if a keybinding fires over the picker (SPEC §7.5
    /// dismisses the stack) the last preview stands, which is the honest reading of
    /// "you saw it applied and moved on".
    pub fn previewing(mut self, cancel: Command) -> Self {
        self.cancel = Some(cancel);
        self.previewed = self.highlighted();
        self
    }

    /// The item the highlight sits on, if the filtered list is not empty.
    fn highlighted(&self) -> Option<usize> {
        self.filtered.get(self.selected).copied()
    }

    /// Emit the highlighted item's command if the highlight has moved since the last
    /// preview. No-op unless [`Self::previewing`] armed it.
    fn preview(&mut self) {
        if self.cancel.is_none() || self.highlighted() == self.previewed {
            return;
        }
        self.previewed = self.highlighted();
        if let Some(idx) = self.previewed {
            self.outbox.push(self.items[idx].command.clone());
        }
    }

    /// Recompute the ranked subset for the current query. An empty query lists every
    /// item in its original order; otherwise `nucleo` ranks by fuzzy score.
    fn refilter(&mut self) {
        self.filtered = if self.query.is_empty() {
            (0..self.items.len()).collect()
        } else {
            let ranked = Pattern::parse(&self.query, CaseMatching::Ignore, Normalization::Smart)
                .match_list(
                    self.items.iter().enumerate().map(|(idx, item)| Ranked {
                        idx,
                        label: &item.label,
                    }),
                    &mut self.matcher,
                );
            ranked.into_iter().map(|(r, _score)| r.idx).collect()
        };
        // Keep the highlight in range as the list shrinks.
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    /// The centered box the picker occupies, clamped to the screen.
    fn area(screen: Rect) -> Rect {
        let w = screen.width.min(60);
        let h = screen.height.min(18);
        let x = screen.x + (screen.width - w) / 2;
        let y = screen.y + (screen.height - h) / 2;
        Rect::new(x, y, w, h)
    }

    /// The box's interior, or `None` when the screen is too small to hold a
    /// usable picker (the editor is then left unobstructed). One home for the
    /// minimum-size threshold and the border geometry, shared by [`Self::render`]
    /// and [`Self::cursor`] so the caret can never be placed for a box that was
    /// not drawn (or in the wrong cell after a size change).
    fn inner_area(screen: Rect) -> Option<Rect> {
        if screen.width < 10 || screen.height < 4 {
            return None;
        }
        let inner = Block::bordered().inner(Self::area(screen));
        (inner.width > 0 && inner.height > 0).then_some(inner)
    }
}

impl Layer for Picker {
    fn render(&self, screen: Rect, buf: &mut Buffer) {
        let Some(inner) = Self::inner_area(screen) else {
            return;
        };
        let area = Self::area(screen);
        Clear.render(area, buf);
        let block = Block::bordered()
            .title(format!(" {} ", self.title))
            .style(self.style);
        block.render(area, buf);
        // Query row at the top of the interior.
        let query_line = format!("> {}", self.query);
        buf.set_stringn(
            inner.x,
            inner.y,
            &query_line,
            inner.width as usize,
            self.style,
        );
        // The list fills the rows beneath it, scrolled to keep the highlight visible.
        let list_h = inner.height.saturating_sub(1) as usize;
        if list_h == 0 {
            return;
        }
        let scroll = self.selected.saturating_sub(list_h - 1);
        for (row, &idx) in self.filtered.iter().enumerate().skip(scroll).take(list_h) {
            let y = inner.y + 1 + (row - scroll) as u16;
            let style = if row == self.selected {
                self.selected_style
            } else {
                self.style
            };
            let item = &self.items[idx];
            let rect = Rect::new(inner.x, y, inner.width, 1);
            buf.set_style(rect, style);
            buf.set_stringn(
                inner.x,
                y,
                format!("  {}", item.label),
                inner.width as usize,
                style,
            );
            // The shortcut (if any) is drawn right-aligned, one cell in from the
            // border. Labels are short, so it does not collide with them.
            if let Some(shortcut) = &item.shortcut {
                let text = format!("{shortcut} ");
                let w = text.width() as u16;
                if w < inner.width {
                    buf.set_stringn(inner.x + inner.width - w, y, &text, w as usize, style);
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        // A Ctrl/Cmd chord is a keybinding, not picker input: defer it (Ignored) so
        // the shortcut runs and the loop dismisses the picker. Kept generic (not
        // naming keys), so configurable shortcuts (M5) work from a picker for free -
        // the keymap stays the single source the picker also *displays* (§7.5, §10.5).
        if key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER)
        {
            return EventResult::Ignored;
        }
        match key.code {
            KeyCode::Esc => {
                // Undo any preview this picker applied on the way here.
                self.outbox.extend(self.cancel.clone());
                self.finished = true;
            }
            KeyCode::Enter => {
                if let Some(idx) = self.highlighted() {
                    self.outbox.push(self.items[idx].command.clone());
                }
                self.finished = true;
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                let last = self.filtered.len().saturating_sub(1);
                self.selected = (self.selected + 1).min(last);
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.refilter();
                self.selected = 0;
            }
            // Typing filters (Alt passes through for composed accented input; Ctrl/Cmd
            // already returned above).
            KeyCode::Char(c) => {
                self.query.push(c);
                self.refilter();
                self.selected = 0;
            }
            // Modal: swallow anything else so it never reaches the editor beneath.
            _ => {}
        }
        // Enter and Esc have already said their piece (and finished); every other
        // key may have moved the highlight, which is what a preview follows.
        if !self.finished {
            self.preview();
        }
        EventResult::Consumed
    }

    fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.outbox)
    }

    fn restyle(&mut self, theme: &Theme) {
        self.style = theme.palette;
        self.selected_style = theme.palette_selected;
    }

    fn cursor(&self, screen: Rect) -> Option<Position> {
        let inner = Self::inner_area(screen)?;
        // Caret in the query row, after the "> " prompt plus the typed text.
        let col = 2 + self.query.width();
        let x = (inner.x as usize + col).min(inner.right().saturating_sub(1) as usize) as u16;
        Some(Position::new(x, inner.y))
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vortex_core::Action;

    fn items() -> Vec<Item> {
        [
            ("Save File", Some("Ctrl+S"), Command::Editor(Action::Save)),
            ("Open Palette", None, Command::OpenPalette),
            ("Quit", Some("Ctrl+Q"), Command::Editor(Action::Quit)),
            ("Copy", None, Command::Editor(Action::Copy)),
        ]
        .into_iter()
        .map(|(label, shortcut, command)| Item {
            label: label.to_string(),
            shortcut: shortcut.map(str::to_string),
            command,
        })
        .collect()
    }

    fn picker() -> Picker {
        Picker::new("Test", items(), false, Style::default(), Style::default())
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(p: &mut Picker, s: &str) {
        for c in s.chars() {
            p.handle_key(key(c));
        }
    }

    fn selected_label(p: &Picker) -> &str {
        &p.items[p.filtered[p.selected]].label
    }

    #[test]
    fn starts_listing_every_item() {
        let p = picker();
        assert_eq!(p.filtered.len(), p.items.len());
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn typing_fuzzy_filters_and_ranks() {
        let mut p = picker();
        type_str(&mut p, "quit");
        assert_eq!(p.filtered.len(), 1);
        assert_eq!(selected_label(&p), "Quit");
        // A non-matching query empties the list.
        let mut p = picker();
        type_str(&mut p, "zzzq");
        assert!(p.filtered.is_empty());
    }

    #[test]
    fn down_and_up_move_the_selection_clamped() {
        let mut p = picker();
        p.handle_key(press(KeyCode::Up)); // at top - clamps
        assert_eq!(p.selected, 0);
        p.handle_key(press(KeyCode::Down));
        assert_eq!(p.selected, 1);
        for _ in 0..100 {
            p.handle_key(press(KeyCode::Down));
        }
        assert_eq!(p.selected, p.filtered.len() - 1);
    }

    #[test]
    fn enter_commits_the_highlighted_command_and_finishes() {
        let mut p = picker();
        type_str(&mut p, "quit");
        p.handle_key(press(KeyCode::Enter));
        assert!(p.is_finished());
        assert_eq!(p.take_commands(), vec![Command::Editor(Action::Quit)]);
    }

    #[test]
    fn ctrl_chord_is_deferred_not_typed() {
        // A Ctrl/Cmd chord is a shortcut, not filter input: the picker ignores it
        // (so the loop runs the binding) and does not add it to the query.
        let mut p = picker();
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(p.handle_key(ctrl_s), EventResult::Ignored);
        assert!(p.query.is_empty(), "the chord must not filter");
        assert!(
            !p.is_finished(),
            "deferring does not close the picker itself"
        );
    }

    #[test]
    fn esc_cancels_with_no_command() {
        let mut p = picker();
        p.handle_key(press(KeyCode::Esc));
        assert!(p.is_finished());
        assert!(p.take_commands().is_empty());
    }

    #[test]
    fn a_picker_without_preview_stays_silent_until_enter() {
        // The palette and file picker must not emit as you arrow through them -
        // opening a file per row visited would be a disaster. Preview is opt-in.
        let mut p = picker();
        for code in [KeyCode::Down, KeyCode::Down, KeyCode::Up] {
            p.handle_key(press(code));
            assert!(p.take_commands().is_empty(), "moved but emitted");
        }
        type_str(&mut p, "cop");
        assert!(p.take_commands().is_empty(), "filtered but emitted");
    }

    #[test]
    fn with_selected_opens_on_a_given_row_clamped() {
        let p = picker().with_selected(2);
        assert_eq!(selected_label(&p), "Quit");
        // Past the end clamps to the last row rather than pointing at nothing.
        let p = picker().with_selected(99);
        assert_eq!(p.selected, p.filtered.len() - 1);
    }

    #[test]
    fn previewing_emits_on_every_move_and_undoes_on_cancel() {
        let cancel = Command::OpenPalette;
        let mut p = picker().previewing(cancel.clone());
        // Opening previews nothing: the highlight has not moved yet.
        assert!(p.take_commands().is_empty());

        p.handle_key(press(KeyCode::Down));
        assert_eq!(p.take_commands(), vec![Command::OpenPalette]); // row 1's command
        // A key that leaves the highlight where it is must not re-emit, or a held
        // Up at the top would fire the same preview over and over.
        p.handle_key(press(KeyCode::Up));
        assert_eq!(p.take_commands(), vec![Command::Editor(Action::Save)]);
        p.handle_key(press(KeyCode::Up));
        assert!(p.take_commands().is_empty(), "re-emitted without moving");

        // Cancelling emits the undo command; committing does not.
        p.handle_key(press(KeyCode::Esc));
        assert_eq!(p.take_commands(), vec![cancel]);
    }

    #[test]
    fn a_preview_over_an_empty_result_emits_nothing() {
        // Filtering down to nothing leaves no highlighted item; the preview must
        // simply stop rather than reaching for a row that is not there.
        let mut p = picker().previewing(Command::OpenPalette);
        type_str(&mut p, "zzzq");
        assert!(p.filtered.is_empty());
        assert!(p.take_commands().is_empty());
    }

    #[test]
    fn restyle_adopts_the_new_themes_palette_styles() {
        let mut p = picker();
        let theme = Theme {
            palette: Style::new().bg(ratatui::style::Color::Rgb(1, 2, 3)),
            palette_selected: Style::new().bg(ratatui::style::Color::Rgb(4, 5, 6)),
            ..Theme::default()
        };
        p.restyle(&theme);
        assert_eq!(p.style, theme.palette);
        assert_eq!(p.selected_style, theme.palette_selected);
    }

    #[test]
    fn enter_on_an_empty_result_commits_nothing() {
        let mut p = picker();
        type_str(&mut p, "zzzq");
        p.handle_key(press(KeyCode::Enter));
        assert!(p.is_finished());
        assert!(p.take_commands().is_empty());
    }

    #[test]
    fn backspace_widens_the_filter_again() {
        let mut p = picker();
        type_str(&mut p, "quit");
        assert_eq!(p.filtered.len(), 1);
        for _ in 0..4 {
            p.handle_key(press(KeyCode::Backspace));
        }
        assert!(p.query.is_empty());
        assert_eq!(p.filtered.len(), p.items.len());
    }

    #[test]
    fn renders_a_centered_titled_box_with_query_and_items() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut p = picker();
        type_str(&mut p, "sa");
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        terminal
            .draw(|frame| p.render(frame.area(), frame.buffer_mut()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("Test"), "border title present");
        assert!(text.contains("> sa"), "query row present");
        assert!(text.contains("Save File"), "a matching item is listed");
        let inner = Block::bordered().inner(Picker::area(Rect::new(0, 0, 40, 16)));
        assert_eq!(
            p.cursor(Rect::new(0, 0, 40, 16)),
            Some(Position::new(inner.x + 4, inner.y))
        );
    }

    #[test]
    fn renders_the_shortcut_right_aligned() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let p = picker(); // "Save File" carries "Ctrl+S"; "Open Palette" carries none
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        terminal
            .draw(|frame| p.render(frame.area(), frame.buffer_mut()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        // "Save File" is the top row of the list (row after the query line).
        let inner = Block::bordered().inner(Picker::area(Rect::new(0, 0, 40, 16)));
        let row_y = inner.y + 1;
        let row: String = (inner.x..inner.right())
            .map(|x| buf.cell((x, row_y)).unwrap().symbol().to_string())
            .collect();
        assert!(row.contains("Save File"), "label on the left: {row:?}");
        assert!(row.contains("Ctrl+S"), "shortcut shown: {row:?}");
        // The shortcut sits flush to the right, after the label.
        assert!(
            row.find("Save File").unwrap() < row.find("Ctrl+S").unwrap(),
            "shortcut is right of the label: {row:?}"
        );
        assert!(row.trim_end().ends_with("Ctrl+S"), "right-aligned: {row:?}");
    }
}
