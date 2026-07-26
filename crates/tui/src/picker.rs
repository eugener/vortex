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
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
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

    /// First visible row of the list. The list scrolls only as far as it must to
    /// keep the highlight on screen, so the offset is derived from the selection
    /// rather than stored - one source of truth, and nothing to keep in step when
    /// the list is refiltered under it.
    ///
    /// Shared by [`Layer::render`] and [`Self::row_at`] for the same reason
    /// [`Self::inner_area`] is: a click must land on the row the user is looking
    /// at, which can only be guaranteed if both compute it the same way.
    fn list_scroll(&self, list_h: usize) -> usize {
        self.selected.saturating_sub(list_h.saturating_sub(1))
    }

    /// The row of [`Self::filtered`] under the pointer, or `None` if the pointer is
    /// on the border, the query row, or off the box entirely.
    fn row_at(&self, screen: Rect, column: u16, row: u16) -> Option<usize> {
        let inner = Self::inner_area(screen)?;
        if column < inner.x || column >= inner.right() {
            return None;
        }
        let list_h = inner.height.saturating_sub(1) as usize;
        if list_h == 0 {
            return None;
        }
        // The query row sits at `inner.y`; the list starts beneath it.
        let offset = row.checked_sub(inner.y + 1)? as usize;
        if offset >= list_h {
            return None;
        }
        let index = self.list_scroll(list_h) + offset;
        // Past the last match: the rows are painted, but there is nothing on them.
        (index < self.filtered.len()).then_some(index)
    }

    /// Run the highlighted item and close, the one path a commit takes whether it
    /// came from Enter or from a click.
    fn commit(&mut self) {
        if let Some(idx) = self.highlighted() {
            self.outbox.push(self.items[idx].command.clone());
        }
        self.finished = true;
    }

    /// Close without running anything, undoing any preview applied on the way here.
    fn cancel(&mut self) {
        self.outbox.extend(self.cancel.clone());
        self.finished = true;
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
        let scroll = self.list_scroll(list_h);
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
            KeyCode::Esc => self.cancel(),
            KeyCode::Enter => self.commit(),
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

    fn handle_mouse(&mut self, mouse: MouseEvent, screen: Rect) -> EventResult {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(row) = self.row_at(screen, mouse.column, mouse.row) {
                    // One click picks. A picker exists to choose something, and the
                    // row under the pointer is unambiguous in a way a caret position
                    // is not - so there is nothing for a second click to confirm.
                    self.selected = row;
                    self.commit();
                } else if !Self::area(screen).contains(Position::new(mouse.column, mouse.row)) {
                    // Outside the box: the click means "not this", which is Esc.
                    self.cancel();
                }
                // Anywhere else in the box - border, query row, empty rows below the
                // last match - is a click on the picker's own furniture. Swallowed.
            }
            // The wheel moves the highlight, which is what scrolls the list: the view
            // offset is derived from the selection, so there is no way to scroll the
            // list away from it and then click on a row that is not what it says.
            // Previewing pickers preview as it moves, exactly as for an arrow key.
            MouseEventKind::ScrollUp => self.selected = self.selected.saturating_sub(1),
            MouseEventKind::ScrollDown => {
                let last = self.filtered.len().saturating_sub(1);
                self.selected = (self.selected + 1).min(last);
            }
            _ => {}
        }
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

    /// The screen every mouse test hit-tests against. Deliberately larger than the
    /// picker's 60x18 maximum, so the box is genuinely centered and there is an
    /// *outside* to click - on a smaller screen the box fills it and the
    /// click-away case cannot be expressed at all.
    const SCREEN: Rect = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 24,
    };

    /// A left press at a screen cell.
    fn click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn wheel(up: bool) -> MouseEvent {
        MouseEvent {
            kind: if up {
                MouseEventKind::ScrollUp
            } else {
                MouseEventKind::ScrollDown
            },
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// The screen cell of the nth listed row, which is where a user would aim.
    fn row_cell(n: u16) -> (u16, u16) {
        let inner = Picker::inner_area(SCREEN).unwrap();
        (inner.x + 2, inner.y + 1 + n)
    }

    #[test]
    fn clicking_a_row_runs_it() {
        // One click picks: the row under the pointer is unambiguous, so there is
        // nothing for a second click to confirm.
        let mut p = picker();
        let (x, y) = row_cell(2); // "Quit", the third item
        assert_eq!(p.handle_mouse(click(x, y), SCREEN), EventResult::Consumed);
        assert!(p.is_finished());
        assert_eq!(
            p.take_commands(),
            vec![Command::Editor(Action::Quit)],
            "the clicked row's command, not the highlighted one's"
        );
    }

    #[test]
    fn clicking_a_row_of_a_filtered_list_runs_what_is_shown() {
        // The rows are the *filtered* list, so a click has to resolve through it -
        // clicking row 0 after filtering must not run item 0.
        let mut p = picker();
        type_str(&mut p, "op"); // ranks "Open Palette" (and maybe others)
        let expected = p.items[p.filtered[0]].command.clone();
        let (x, y) = row_cell(0);
        p.handle_mouse(click(x, y), SCREEN);
        assert_eq!(p.take_commands(), vec![expected]);
    }

    #[test]
    fn clicking_outside_the_box_cancels() {
        let mut p = picker();
        assert_eq!(p.handle_mouse(click(0, 0), SCREEN), EventResult::Consumed);
        assert!(p.is_finished());
        assert!(p.take_commands().is_empty(), "cancelling runs nothing");
    }

    #[test]
    fn clicking_outside_a_previewing_picker_undoes_the_preview() {
        // Esc's other job: a theme picker that previewed as you moved has to put the
        // old one back. A click outside means the same thing, so it does the same.
        let mut p = picker().previewing(Command::OpenPalette);
        p.handle_key(press(KeyCode::Down));
        let _ = p.take_commands(); // the preview
        p.handle_mouse(click(0, 0), SCREEN);
        assert_eq!(p.take_commands(), vec![Command::OpenPalette]);
    }

    #[test]
    fn clicking_the_pickers_own_furniture_does_nothing() {
        // The border, the query row, and the empty rows past the last match are all
        // part of the picker - a click there is neither a pick nor a dismissal.
        let inner = Picker::inner_area(SCREEN).unwrap();
        for (x, y) in [
            (Picker::area(SCREEN).x, Picker::area(SCREEN).y), // the border corner
            (inner.x + 1, inner.y),                           // the query row
            (inner.x + 1, inner.y + 1 + items().len() as u16), // past the last item
        ] {
            let mut p = picker();
            assert_eq!(p.handle_mouse(click(x, y), SCREEN), EventResult::Consumed);
            assert!(!p.is_finished(), "click at ({x},{y}) closed the picker");
            assert!(p.take_commands().is_empty());
        }
    }

    #[test]
    fn the_wheel_moves_the_highlight() {
        let mut p = picker();
        p.handle_mouse(wheel(false), SCREEN);
        assert_eq!(selected_label(&p), "Open Palette");
        p.handle_mouse(wheel(true), SCREEN);
        assert_eq!(selected_label(&p), "Save File");
        // ...and stops at each end rather than wrapping or overflowing.
        p.handle_mouse(wheel(true), SCREEN);
        assert_eq!(p.selected, 0);
        for _ in 0..10 {
            p.handle_mouse(wheel(false), SCREEN);
        }
        assert_eq!(p.selected, p.filtered.len() - 1);
    }

    #[test]
    fn the_wheel_previews_like_an_arrow_key() {
        let mut p = picker().previewing(Command::OpenPalette);
        p.handle_mouse(wheel(false), SCREEN);
        assert_eq!(
            p.take_commands(),
            vec![p.items[p.filtered[1]].command.clone()]
        );
    }

    #[test]
    fn a_click_lands_on_the_row_the_user_sees_after_the_list_has_scrolled() {
        // The list scrolls by following the highlight, so a long list is offset -
        // and a hit test that ignored that would run the wrong item entirely.
        let many: Vec<Item> = (0..40)
            .map(|n| Item {
                label: format!("item-{n:02}"),
                shortcut: None,
                command: Command::Editor(Action::Insert(format!("{n}"))),
            })
            .collect();
        let mut p = Picker::new("Many", many, false, Style::default(), Style::default());
        for _ in 0..30 {
            p.handle_key(press(KeyCode::Down));
        }
        let inner = Picker::inner_area(SCREEN).unwrap();
        let list_h = inner.height.saturating_sub(1) as usize;
        // The highlight has driven the list down; the top visible row is not item 0.
        let scroll = p.list_scroll(list_h);
        assert!(
            scroll > 0,
            "the list must have scrolled for this to mean anything"
        );
        let (x, y) = row_cell(0);
        p.handle_mouse(click(x, y), SCREEN);
        assert_eq!(
            p.take_commands(),
            vec![Command::Editor(Action::Insert(format!("{scroll}")))],
            "the top visible row is the scrolled-to item, not item 0"
        );
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
