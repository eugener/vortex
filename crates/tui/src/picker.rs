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
//!
//! **Two different things here are called "preview".** [`Picker::previewing`] applies
//! the highlighted item as you move over it and undoes that on Esc - the theme
//! picker, where the only way to judge a theme is to see it. [`Picker::with_preview_pane`]
//! shows the highlighted item's *content* in a second column beside the list, changing
//! nothing - the file picker, where the list is paths and the question is what is in
//! them. They are independent: a picker can arm either, both, or neither.

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

/// Rows the box is at most tall, and at most wide - with and without a preview
/// pane, which is the whole reason the wider one exists.
const MAX_HEIGHT: u16 = 18;
const MAX_WIDTH: u16 = 60;
const MAX_WIDTH_WITH_PANE: u16 = 100;

/// Screen width below which the preview pane is dropped and the picker is the plain
/// list it always was. Half of a narrow screen each would give two columns too thin
/// to read - a truncated path list next to truncated code is worse than the list
/// alone, which at least fits.
const MIN_PANE_SCREEN: u16 = 80;

/// Lines a preview source is asked for: what the tallest possible pane can show
/// (the interior, which the pane fills - unlike the list it has no query row).
const PREVIEW_LINES: usize = (MAX_HEIGHT - 2) as usize;

/// One selectable row: a user-facing label, an optional shortcut to show
/// right-aligned (the key that runs it, if any), and what running it does.
pub struct Item {
    pub label: String,
    pub shortcut: Option<String>,
    pub command: Command,
}

/// Fills a preview pane: given the highlighted item and how many lines the pane can
/// show, the lines to paint (already clipped to that many, control characters
/// resolved - the pane paints them as given).
///
/// Called when the highlight *moves*, not once per frame, so it may do bounded I/O.
/// It runs on the UI thread, which is the same bargain the file picker's directory
/// walk already makes: bound the work rather than move it off-thread (see
/// [`crate::filepicker`]).
pub type PreviewSource = Box<dyn Fn(&Item, usize) -> Vec<String>>;

/// The preview pane: where its content comes from, that content, and the item it was
/// fetched for - so a keystroke that leaves the highlight where it was does not read
/// the same file again.
struct Pane {
    source: PreviewSource,
    lines: Vec<String>,
    item: Option<usize>,
}

/// Supplies a picker's rows from the query instead of holding them all up front.
///
/// The ordinary picker ranks a list it was handed - every command, every open
/// buffer. Some lists cannot be handed over: the matches for a pattern across a
/// project are not knowable until the pattern is, and arrive over time once it is.
/// Such a picker implements this instead, and the query text stops being a filter
/// and becomes the search.
///
/// [`Self::query`] starts (or restarts, or clears) the work; [`Self::take`] is
/// called once per render tick for whatever has arrived since. Neither may block:
/// both run on the UI thread.
pub trait ItemSource {
    /// Start over for a new query. An empty query means "no results wanted" - the
    /// picker is open but nothing has been asked for yet.
    fn query(&mut self, query: &str);

    /// Rows that have arrived since the last call, if any.
    fn take(&mut self) -> Vec<Item>;

    /// A line to show in place of the list when there are no rows yet: what went
    /// wrong, or what the picker is waiting for. `None` shows nothing.
    fn status(&self) -> Option<String> {
        None
    }
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
    /// Set by [`Self::with_preview_pane`]: the second column and its content. `None`
    /// is a picker that is only a list.
    pane: Option<Pane>,
    /// Set by [`Self::with_item_source`]: rows come from the query rather than from
    /// fuzzy-ranking a fixed list. `None` is the ordinary filtering picker.
    source: Option<Box<dyn ItemSource>>,
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
            pane: None,
            source: None,
        }
    }

    /// Draw the rows from `source`, keyed on the query, instead of fuzzy-filtering
    /// the items the picker was built with (see [`ItemSource`]).
    ///
    /// The items passed to [`Self::new`] are then the *starting* list, which for a
    /// search picker is empty: every row after that comes from the source.
    pub fn with_item_source(mut self, source: Box<dyn ItemSource>) -> Self {
        self.source = Some(source);
        self
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

    /// Show the highlighted item's content beside the list, filled by `source`.
    ///
    /// A request, not a guarantee: the pane is dropped on a screen too narrow to
    /// hold both columns ([`MIN_PANE_SCREEN`]), and the picker is then exactly the
    /// list it would have been without this call.
    pub fn with_preview_pane(mut self, source: PreviewSource) -> Self {
        self.pane = Some(Pane {
            source,
            lines: Vec::new(),
            item: None,
        });
        self.fill_pane();
        self
    }

    /// The item the highlight sits on, if the filtered list is not empty.
    fn highlighted(&self) -> Option<usize> {
        self.filtered.get(self.selected).copied()
    }

    /// Refill the pane when the highlight has moved to a different item since it was
    /// last filled. Unlike [`Self::preview`] this also runs on open: a pane that
    /// stays blank until you press a key is just a hole in the box.
    fn fill_pane(&mut self) {
        let highlighted = self.highlighted();
        let Some(pane) = self.pane.as_mut() else {
            return;
        };
        if pane.item == highlighted {
            return;
        }
        pane.item = highlighted;
        // Nothing highlighted (a query that matched nothing) empties the pane rather
        // than leaving the last match's content under a list that no longer has it.
        pane.lines = match highlighted {
            Some(idx) => (pane.source)(&self.items[idx], PREVIEW_LINES),
            None => Vec::new(),
        };
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
    ///
    /// A picker with an [`ItemSource`] does neither: the query *is* the search, so
    /// the rows are discarded and asked for again rather than ranked.
    fn refilter(&mut self) {
        if let Some(source) = self.source.as_mut() {
            source.query(&self.query);
            self.items.clear();
            self.filtered.clear();
            self.selected = 0;
            return;
        }
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

    /// The centered box the picker occupies, clamped to the screen. Wider when it is
    /// carrying a preview pane, since the extra width *is* the pane.
    fn area(&self, screen: Rect) -> Rect {
        let max = if self.has_pane(screen) {
            MAX_WIDTH_WITH_PANE
        } else {
            MAX_WIDTH
        };
        let w = screen.width.min(max);
        let h = screen.height.min(MAX_HEIGHT);
        let x = screen.x + (screen.width - w) / 2;
        let y = screen.y + (screen.height - h) / 2;
        Rect::new(x, y, w, h)
    }

    /// Whether this screen gets a preview pane: one was armed, and there is room.
    fn has_pane(&self, screen: Rect) -> bool {
        self.pane.is_some() && screen.width >= MIN_PANE_SCREEN
    }

    /// First visible row of the list. The list scrolls only as far as it must to
    /// keep the highlight on screen, so the offset is derived from the selection
    /// rather than stored - one source of truth, and nothing to keep in step when
    /// the list is refiltered under it.
    ///
    /// Shared by [`Layer::render`] and [`Self::row_at`] for the same reason
    /// [`Self::columns`] is: a click must land on the row the user is looking
    /// at, which can only be guaranteed if both compute it the same way.
    fn list_scroll(&self, list_h: usize) -> usize {
        self.selected.saturating_sub(list_h.saturating_sub(1))
    }

    /// The row of [`Self::filtered`] under the pointer, or `None` if the pointer is
    /// on the border, the query row, the preview pane, or off the box entirely.
    fn row_at(&self, screen: Rect, column: u16, row: u16) -> Option<usize> {
        let (inner, _) = self.columns(screen)?;
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

    /// The box's interior split into its columns - the list, and the preview pane
    /// when there is one - or `None` when the screen is too small to hold a usable
    /// picker (the editor is then left unobstructed). The column between them is
    /// left for the divider.
    ///
    /// One home for the minimum-size threshold, the border geometry, and the split,
    /// shared by [`Self::render`], [`Self::row_at`] and [`Self::cursor`], so the
    /// caret can never be placed for a box that was not drawn (or in the wrong cell
    /// after a size change) and a click can never resolve against a column the paint
    /// put somewhere else.
    fn columns(&self, screen: Rect) -> Option<(Rect, Option<Rect>)> {
        if screen.width < 10 || screen.height < 4 {
            return None;
        }
        let inner = Block::bordered().inner(self.area(screen));
        if inner.width == 0 || inner.height == 0 {
            return None;
        }
        if !self.has_pane(screen) {
            return Some((inner, None));
        }
        let list_w = inner.width / 2;
        let list = Rect::new(inner.x, inner.y, list_w, inner.height);
        let pane = Rect::new(
            inner.x + list_w + 1,
            inner.y,
            inner.width - list_w - 1,
            inner.height,
        );
        Some((list, Some(pane)))
    }

    /// The preview pane: a divider against the list, then the fetched lines, each
    /// clipped to the pane rather than wrapped - wrapped code reads as a different
    /// file than the one you are about to open.
    fn render_pane(&self, state: &Pane, pane: Rect, buf: &mut Buffer) {
        for y in pane.y..pane.bottom() {
            buf.set_stringn(pane.x - 1, y, "│", 1, self.style);
        }
        for (row, line) in state.lines.iter().take(pane.height as usize).enumerate() {
            buf.set_stringn(
                pane.x,
                pane.y + row as u16,
                line,
                pane.width as usize,
                self.style,
            );
        }
    }
}

impl Layer for Picker {
    fn render(&self, screen: Rect, buf: &mut Buffer) {
        let Some((inner, pane)) = self.columns(screen) else {
            return;
        };
        let area = self.area(screen);
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
        // `columns` only hands back a pane when there is one to fill it, so the two
        // are always Some together.
        if let (Some(area), Some(state)) = (pane, self.pane.as_ref()) {
            self.render_pane(state, area, buf);
        }
        // The list fills the rows beneath it, scrolled to keep the highlight visible.
        let list_h = inner.height.saturating_sub(1) as usize;
        if list_h == 0 {
            return;
        }
        // Nothing to list yet: say why, if the source knows. An empty box under a
        // typed query is the one state a picker cannot explain by itself - a bad
        // pattern and a genuine no-match look identical.
        if self.filtered.is_empty()
            && let Some(status) = self.source.as_ref().and_then(|s| s.status())
        {
            buf.set_stringn(
                inner.x,
                inner.y + 1,
                format!("  {status}"),
                inner.width as usize,
                self.style,
            );
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
            self.fill_pane();
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
                } else if !self
                    .area(screen)
                    .contains(Position::new(mouse.column, mouse.row))
                {
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
            self.fill_pane();
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

    fn tick(&mut self) -> bool {
        let Some(source) = self.source.as_mut() else {
            return false;
        };
        let arrived = source.take();
        if arrived.is_empty() {
            return false;
        }
        // Appended in arrival order and never re-ranked: the rows under the pointer
        // must not move while results are still coming in, or a click lands on
        // something other than what it was aimed at.
        self.filtered
            .extend(self.items.len()..self.items.len() + arrived.len());
        self.items.extend(arrived);
        // The first result to arrive is what the highlight (and so the pane) was
        // waiting for.
        self.fill_pane();
        true
    }

    fn cursor(&self, screen: Rect) -> Option<Position> {
        let (list, _) = self.columns(screen)?;
        // Caret in the query row, after the "> " prompt plus the typed text.
        let col = 2 + self.query.width();
        let x = (list.x as usize + col).min(list.right().saturating_sub(1) as usize) as u16;
        Some(Position::new(x, list.y))
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

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

    /// The screen cell of the nth listed row, which is where a user would aim. Takes
    /// the picker because the box's own geometry depends on it (a preview pane
    /// widens it and halves the list column).
    fn row_cell(p: &Picker, n: u16) -> (u16, u16) {
        let (list, _) = p.columns(SCREEN).unwrap();
        (list.x + 2, list.y + 1 + n)
    }

    #[test]
    fn clicking_a_row_runs_it() {
        // One click picks: the row under the pointer is unambiguous, so there is
        // nothing for a second click to confirm.
        let mut p = picker();
        let (x, y) = row_cell(&p, 2); // "Quit", the third item
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
        let (x, y) = row_cell(&p, 0);
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
        let (list, _) = picker().columns(SCREEN).unwrap();
        let area = picker().area(SCREEN);
        for (x, y) in [
            (area.x, area.y),                                // the border corner
            (list.x + 1, list.y),                            // the query row
            (list.x + 1, list.y + 1 + items().len() as u16), // past the last item
            (list.x + 1, area.bottom() - 1),                 // the bottom border
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
        let (list, _) = p.columns(SCREEN).unwrap();
        let list_h = list.height.saturating_sub(1) as usize;
        // The highlight has driven the list down; the top visible row is not item 0.
        let scroll = p.list_scroll(list_h);
        assert!(
            scroll > 0,
            "the list must have scrolled for this to mean anything"
        );
        let (x, y) = row_cell(&p, 0);
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
        let (list, _) = p.columns(Rect::new(0, 0, 40, 16)).unwrap();
        assert_eq!(
            p.cursor(Rect::new(0, 0, 40, 16)),
            Some(Position::new(list.x + 4, list.y))
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
        let (list, _) = p.columns(Rect::new(0, 0, 40, 16)).unwrap();
        let row_y = list.y + 1;
        let row: String = (list.x..list.right())
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

    /// A picker with a preview pane that records which labels it was asked to fill
    /// from, so a test can tell a refill apart from a repaint.
    fn paned() -> (Picker, Rc<RefCell<Vec<String>>>) {
        let asked = Rc::new(RefCell::new(Vec::new()));
        let log = Rc::clone(&asked);
        let p = picker().with_preview_pane(Box::new(move |item, lines| {
            log.borrow_mut().push(item.label.clone());
            assert!(lines > 0);
            vec![format!("inside {}", item.label)]
        }));
        (p, asked)
    }

    fn painted(p: &Picker, screen: Rect) -> Buffer {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(screen.width, screen.height)).unwrap();
        terminal
            .draw(|frame| p.render(frame.area(), frame.buffer_mut()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn the_preview_pane_shows_the_highlighted_items_content_beside_the_list() {
        let (p, asked) = paned();
        // Filled on open: a pane that stays blank until you press a key is a hole.
        assert_eq!(*asked.borrow(), vec!["Save File"]);

        let buf = painted(&p, SCREEN);
        let (list, pane) = p.columns(SCREEN).unwrap();
        let pane = pane.expect("a pane on a screen with room for one");
        let row: String = (pane.x..pane.right())
            .map(|x| buf.cell((x, pane.y)).unwrap().symbol().to_string())
            .collect();
        assert!(row.starts_with("inside Save File"), "{row:?}");
        // Beside the list, with the divider between - not over it.
        assert_eq!(pane.x, list.right() + 1);
        assert_eq!(buf.cell((list.right(), pane.y)).unwrap().symbol(), "│");
        // The caret still belongs to the query row, which is the list's.
        assert!(p.cursor(SCREEN).unwrap().x < list.right());
    }

    #[test]
    fn the_pane_refills_when_the_highlight_moves_and_not_otherwise() {
        // Every keystroke repaints; only a *move* is worth re-reading a file for.
        let (mut p, asked) = paned();
        p.handle_key(press(KeyCode::Down));
        assert_eq!(*asked.borrow(), vec!["Save File", "Open Palette"]);
        p.handle_key(press(KeyCode::Up));
        assert_eq!(asked.borrow().len(), 3);
        p.handle_key(press(KeyCode::Up)); // already at the top: nothing moved
        assert_eq!(asked.borrow().len(), 3, "re-read without moving");
        // The wheel is a move like any other.
        p.handle_mouse(wheel(false), SCREEN);
        assert_eq!(asked.borrow().len(), 4);
    }

    #[test]
    fn filtering_refills_the_pane_for_whatever_is_now_highlighted() {
        let (mut p, asked) = paned();
        type_str(&mut p, "quit");
        assert_eq!(asked.borrow().last().unwrap(), "Quit");
        // A query that matches nothing empties it, rather than leaving the last
        // match's content sitting under a list that no longer has that row.
        type_str(&mut p, "zzz");
        assert!(p.filtered.is_empty());
        assert!(p.pane.as_ref().unwrap().lines.is_empty());
        let buf = painted(&p, SCREEN);
        let pane = p.columns(SCREEN).unwrap().1.unwrap();
        let painted_pane: String = (pane.y..pane.bottom())
            .flat_map(|y| (pane.x..pane.right()).map(move |x| (x, y)))
            .map(|cell| buf.cell(cell).unwrap().symbol().to_string())
            .collect();
        assert!(painted_pane.trim().is_empty(), "{painted_pane:?}");
    }

    #[test]
    fn a_screen_too_narrow_for_two_columns_drops_the_pane() {
        // Half of a narrow screen each is two unreadable columns instead of one
        // usable one - so the picker falls back to exactly the list it would be
        // without a source at all, geometry included.
        let (p, _) = paned();
        let narrow = Rect::new(0, 0, MIN_PANE_SCREEN - 1, 24);
        let (list, pane) = p.columns(narrow).unwrap();
        assert!(pane.is_none());
        assert_eq!(p.area(narrow).width, MAX_WIDTH);
        assert_eq!(Some(list), picker().columns(narrow).map(|(l, _)| l));
    }

    #[test]
    fn a_click_lands_on_the_row_it_shows_when_a_pane_has_narrowed_the_list() {
        // The pane widens the box and halves the list column, moving every row: a hit
        // test against the pane-less geometry would resolve to the wrong cell.
        let (mut p, _) = paned();
        let (x, y) = row_cell(&p, 2);
        p.handle_mouse(click(x, y), SCREEN);
        assert_eq!(p.take_commands(), vec![Command::Editor(Action::Quit)]);
    }

    #[test]
    fn a_click_in_the_preview_is_furniture_not_a_pick() {
        let (mut p, _) = paned();
        let pane = p.columns(SCREEN).unwrap().1.unwrap();
        assert_eq!(
            p.handle_mouse(click(pane.x + 1, pane.y + 1), SCREEN),
            EventResult::Consumed
        );
        assert!(!p.is_finished());
        assert!(p.take_commands().is_empty());
    }

    #[test]
    fn the_pane_asks_for_every_line_it_could_show() {
        // The source is asked once per move, so it cannot be asked per-frame for the
        // height of *this* frame - the constant has to cover the tallest pane there
        // can be, or the bottom of a full-height pane would paint blank.
        let requested = Rc::new(RefCell::new(0));
        let log = Rc::clone(&requested);
        let p = picker().with_preview_pane(Box::new(move |_, lines| {
            *log.borrow_mut() = lines;
            Vec::new()
        }));
        assert_eq!(*requested.borrow(), PREVIEW_LINES);
        let tallest = Rect::new(0, 0, 200, 200);
        let pane = p.columns(tallest).unwrap().1.unwrap();
        assert_eq!(pane.height as usize, PREVIEW_LINES);
    }

    #[test]
    fn a_pane_longer_than_its_height_is_clipped_rather_than_overflowing() {
        // A source may hand back more than fits (the pane it was sized for is the
        // tallest one, not this one); the extra must not spill past the border.
        let p = picker().with_preview_pane(Box::new(|_, lines| {
            (0..lines).map(|n| format!("line{n}")).collect()
        }));
        let screen = Rect::new(0, 0, 100, 8);
        let buf = painted(&p, screen);
        let pane = p.columns(screen).unwrap().1.unwrap();
        let shown: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(shown.contains("line0"));
        assert!(
            !shown.contains(&format!("line{}", pane.height)),
            "painted past the bottom of the pane"
        );
    }

    #[test]
    fn a_screen_too_small_for_a_usable_box_gets_no_picker_at_all() {
        // The editor is left unobstructed rather than covered by a box with no room
        // for a row in it - and nothing may then place a caret or resolve a click
        // against geometry that was never painted.
        let (p, _) = paned();
        let tiny = Rect::new(0, 0, 8, 3);
        assert!(p.columns(tiny).is_none());
        assert_eq!(p.cursor(tiny), None);
        assert_eq!(p.row_at(tiny, 1, 1), None);
        let mut buf = Buffer::empty(tiny);
        p.render(tiny, &mut buf);
        let painted: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(painted.trim().is_empty(), "{painted:?}");
    }

    /// An [`ItemSource`] a test can drive by hand: `query` records what it was
    /// asked, and rows are handed over only when a test says they have "arrived",
    /// which is how a worker thread's timing is expressed without one.
    #[derive(Default)]
    struct Fake {
        asked: Asked,
        pending: Pending,
        status: Option<String>,
    }

    impl ItemSource for Fake {
        fn query(&mut self, query: &str) {
            self.asked.borrow_mut().push(query.to_string());
            self.pending.borrow_mut().clear();
        }
        fn take(&mut self) -> Vec<Item> {
            std::mem::take(&mut self.pending.borrow_mut())
        }
        fn status(&self) -> Option<String> {
            self.status.clone()
        }
    }

    fn item(label: &str) -> Item {
        Item {
            label: label.to_string(),
            shortcut: None,
            command: Command::Editor(Action::Insert(label.to_string())),
        }
    }

    /// The queries a [`Fake`] was asked, shared with the test that drives it.
    type Asked = Rc<RefCell<Vec<String>>>;
    /// Rows a [`Fake`] will hand over on its next `take`, i.e. "what has arrived".
    type Pending = Rc<RefCell<Vec<Item>>>;

    /// A sourced picker plus handles on what it was asked and what it may receive.
    fn sourced() -> (Picker, Asked, Pending) {
        let asked = Rc::new(RefCell::new(Vec::new()));
        let pending = Rc::new(RefCell::new(Vec::new()));
        let source = Fake {
            asked: Rc::clone(&asked),
            pending: Rc::clone(&pending),
            status: None,
        };
        let p = Picker::new(
            "Search",
            Vec::new(),
            false,
            Style::default(),
            Style::default(),
        )
        .with_item_source(Box::new(source));
        (p, asked, pending)
    }

    #[test]
    fn a_sourced_picker_asks_its_source_instead_of_fuzzy_filtering() {
        let (mut p, asked, _) = sourced();
        type_str(&mut p, "ab");
        assert_eq!(*asked.borrow(), vec!["a", "ab"], "asked on every keystroke");
        p.handle_key(press(KeyCode::Backspace));
        assert_eq!(asked.borrow().last().unwrap(), "a");
    }

    #[test]
    fn rows_arriving_between_keystrokes_join_the_list() {
        // The whole point of the tick seam: results come from a worker, not from the
        // keystroke, so they must appear without one.
        let (mut p, _, pending) = sourced();
        type_str(&mut p, "x");
        assert!(p.filtered.is_empty(), "nothing has arrived yet");
        assert!(!p.tick(), "an empty tick is not a repaint");

        *pending.borrow_mut() = vec![item("first"), item("second")];
        assert!(p.tick(), "arrivals need a repaint");
        assert_eq!(p.filtered.len(), 2);
        assert_eq!(selected_label(&p), "first");

        // A second batch appends rather than replacing: a search delivers in pieces.
        *pending.borrow_mut() = vec![item("third")];
        p.tick();
        assert_eq!(p.items.len(), 3);
        assert_eq!(
            p.filtered,
            vec![0, 1, 2],
            "in arrival order, never re-ranked"
        );
    }

    #[test]
    fn a_new_query_clears_the_rows_the_last_one_produced() {
        // Otherwise the previous pattern's hits stay listed under the new one, and a
        // click runs a row that matches nothing on screen.
        let (mut p, _, pending) = sourced();
        type_str(&mut p, "x");
        *pending.borrow_mut() = vec![item("old hit")];
        p.tick();
        assert_eq!(p.filtered.len(), 1);

        type_str(&mut p, "y");
        assert!(p.items.is_empty(), "the old rows outlived their query");
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn committing_a_sourced_row_runs_the_row_that_arrived() {
        let (mut p, _, pending) = sourced();
        type_str(&mut p, "x");
        *pending.borrow_mut() = vec![item("a"), item("b")];
        p.tick();
        p.handle_key(press(KeyCode::Down));
        p.handle_key(press(KeyCode::Enter));
        assert_eq!(
            p.take_commands(),
            vec![Command::Editor(Action::Insert("b".to_string()))]
        );
    }

    #[test]
    fn a_source_status_is_shown_where_the_rows_would_be() {
        // An empty box under a typed query cannot explain itself: a bad pattern and
        // a genuine no-match look identical without this.
        let source = Fake {
            status: Some("searching…".to_string()),
            ..Fake::default()
        };
        let p = Picker::new(
            "Search",
            Vec::new(),
            false,
            Style::default(),
            Style::default(),
        )
        .with_item_source(Box::new(source));
        let buf = painted(&p, SCREEN);
        let (list, _) = p.columns(SCREEN).unwrap();
        let row: String = (list.x..list.right())
            .map(|x| buf.cell((x, list.y + 1)).unwrap().symbol().to_string())
            .collect();
        assert!(row.contains("searching…"), "{row:?}");
    }

    #[test]
    fn a_status_is_not_shown_over_rows_that_did_arrive() {
        let (mut p, _, pending) = sourced();
        *pending.borrow_mut() = vec![item("a real row")];
        p.tick();
        let buf = painted(&p, SCREEN);
        let (list, _) = p.columns(SCREEN).unwrap();
        let row: String = (list.x..list.right())
            .map(|x| buf.cell((x, list.y + 1)).unwrap().symbol().to_string())
            .collect();
        assert!(row.contains("a real row"), "{row:?}");
    }

    #[test]
    fn a_picker_without_a_source_ignores_the_tick() {
        let mut p = picker();
        assert!(!p.tick());
        assert_eq!(p.items.len(), 4, "the fixed list is untouched");
    }

    #[test]
    fn a_picker_with_no_pane_is_geometrically_unchanged() {
        // The pane is opt-in: the palette and the format pickers must be exactly the
        // 60-column box they were.
        let p = picker();
        assert_eq!(p.area(SCREEN).width, MAX_WIDTH);
        assert!(p.columns(SCREEN).unwrap().1.is_none());
    }
}
