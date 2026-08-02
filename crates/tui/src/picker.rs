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
use ratatui::crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use std::borrow::Cow;
use std::path::Path;

use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, Clear, StatefulWidget, Widget};
use unicode_width::UnicodeWidthStr;

use crate::command::Command;
use crate::compositor::{EventResult, Layer};
use crate::config::Theme;
// The keymap's `Command` is the *binding* side - what a chord names - while this
// module's `Command` is what a committed choice dispatches. Aliased rather than
// qualified because the two appear a line apart in `handle_key`.
use crate::keymap::{Command as Bound, Context};

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

/// A file's path as a picker row shows it: relative to `cwd` when it is under it,
/// absolute otherwise.
///
/// Rows are narrow, and within one project the part that tells two of them apart is
/// the end of the path - but a file opened from elsewhere must not read as a local
/// one, so it keeps its full path. Shared by every picker whose rows are files (the
/// buffer picker, the global-search picker), because two rules would mean the same
/// file reading differently depending on which picker you found it in.
///
/// `cwd` is passed rather than resolved here: a search delivers up to a thousand
/// rows, and that is a syscall each.
pub fn display_path(path: &Path, cwd: Option<&Path>) -> String {
    cwd.and_then(|cwd| path.strip_prefix(cwd).ok())
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

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
    ///
    /// Borrowed, not owned, because this is read once per render tick to notice a
    /// change - roughly sixty times a second for as long as the picker is open, and
    /// almost always to conclude that nothing changed. A `String` return would be an
    /// allocation per tick to answer "still the same".
    fn status(&self) -> Option<Cow<'_, str>> {
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
    /// The scrollbar's two halves, same slots the body's bar reads (SPEC §7.5) - a
    /// second pair for overlays would be two ways to say the same thing in a theme.
    track_style: Style,
    thumb_style: Style,
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
    /// The source's status as of the last tick, so a change in it can ask for a
    /// repaint. A source may change what it says without producing a row.
    status: Option<String>,
    /// Whether the pointer is dragging the scrollbar. The press decides whether the
    /// gesture belongs to the bar and every drag after it inherits that answer, so
    /// pulling a cell sideways off the one-column track keeps scrolling instead of
    /// dying halfway through the gesture - the same bargain the body's bar makes
    /// (SPEC §12), kept in the layer because the picker's geometry is the layer's.
    dragging: bool,
}

impl Picker {
    /// A picker titled `title` over `items`. `match_paths` tunes the matcher for
    /// path-shaped haystacks (a file picker) versus plain labels (a command palette).
    ///
    /// Takes the whole theme rather than the handful of styles it reads, as
    /// [`Layer::restyle`] already does: the picker draws from four slots now, and a
    /// positional list of them is a swap waiting to happen.
    pub fn new(
        title: impl Into<String>,
        items: Vec<Item>,
        match_paths: bool,
        theme: &Theme,
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
            style: theme.palette,
            selected_style: theme.palette_selected,
            track_style: theme.scrollbar_track,
            thumb_style: theme.scrollbar_thumb,
            finished: false,
            outbox: Vec::new(),
            cancel: None,
            previewed: None,
            pane: None,
            source: None,
            status: None,
            dragging: false,
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

    /// The list column split into the rows the items paint on and the one-cell column
    /// reserved to their right for the scrollbar. `None` when the column has no room
    /// for both (a box one cell wide, or with no row under its query line).
    ///
    /// **The column is reserved whether or not a bar is drawn in it**, the bargain the
    /// body's scrollbar already makes (SPEC §7.5): the list overflows and stops
    /// overflowing as you type, and a column that came and went with it would re-clip
    /// every label and re-place every shortcut on the keystroke that changed the match
    /// count. One cell of a sixty-column row is a cheaper price than a list that
    /// twitches while you filter it.
    ///
    /// Shared by [`Layer::render`] and [`Self::row_at`], for the same reason
    /// [`Self::columns`] is: a click must land on the row the user is looking at, and
    /// must not pick one at all when it landed on the bar.
    fn list_split(list: Rect) -> Option<(Rect, Rect)> {
        // A query row plus a row to list under it, and a column for the bar beside
        // them: under either, there is no list left to split.
        if list.height < 2 || list.width < 2 {
            return None;
        }
        // The query row sits at `list.y`; the rows start beneath it.
        let (width, height) = (list.width - 1, list.height - 1);
        Some((
            Rect::new(list.x, list.y + 1, width, height),
            Rect::new(list.right() - 1, list.y + 1, 1, height),
        ))
    }

    /// The scrollbar's track, when there is a bar to grab - which is when the list
    /// outruns the box, the one condition [`Layer::render`] also paints under. `None`
    /// leaves the reserved column inert, which is what it is with nothing drawn in it:
    /// a press there must not scroll a list that has nowhere to scroll to.
    ///
    /// The track is as tall as the rows it stands for, so its `height` is also the
    /// window size the rest of the scroll math needs - no second copy to keep in step.
    fn bar(&self, screen: Rect) -> Option<Rect> {
        let (inner, _) = self.columns(screen)?;
        let (rows, bar) = Self::list_split(inner)?;
        (self.filtered.len() > rows.height as usize).then_some(bar)
    }

    /// Scroll to what a pointer on screen `row` of the track asks for.
    ///
    /// The bar has no offset of its own to write - the list's is
    /// [derived](Self::list_scroll) from the selection - so it moves the *highlight*,
    /// which is what the wheel over this picker already does. The highlight lands on
    /// the last row of the window the pointer chose, which is where it sits whenever
    /// the list is scrolled at all, since that is the only thing that scrolls it.
    ///
    /// Shares [`crate::layout::scroll_at_track_row`] with the body's bar so both read
    /// the track the same way: its ends are the list's ends, and the pointer stays
    /// inside the thumb it is dragging. A row above the track (a drag pulled up past
    /// the query line) means the top, as it does there.
    fn scroll_to_track(&mut self, screen: Rect, row: u16) {
        let Some(bar) = self.bar(screen) else {
            return;
        };
        let track = bar.height as usize;
        let max_scroll = self.filtered.len() - track;
        let offset = row.saturating_sub(bar.y) as usize;
        let Some(scroll) = crate::layout::scroll_at_track_row(offset, track, max_scroll) else {
            return;
        };
        self.selected = (scroll + track - 1).min(self.filtered.len() - 1);
    }

    /// The row of [`Self::filtered`] under the pointer, or `None` if the pointer is
    /// on the border, the query row, the scrollbar, the preview pane, or off the box
    /// entirely.
    fn row_at(&self, screen: Rect, column: u16, row: u16) -> Option<usize> {
        let (inner, _) = self.columns(screen)?;
        let (rows, _) = Self::list_split(inner)?;
        // The bar's column is excluded rather than treated as part of the row it sits
        // on: it is the track's, and a press there scrolls ([`Self::scroll_to_track`])
        // rather than running whatever the user was reaching past.
        if column < rows.x || column >= rows.right() {
            return None;
        }
        let offset = row.checked_sub(rows.y)? as usize;
        if offset >= rows.height as usize {
            return None;
        }
        let index = self.list_scroll(rows.height as usize) + offset;
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
        // The list fills the rows beneath it, scrolled to keep the highlight visible,
        // with the last column of each row left to the scrollbar.
        let Some((rows, _)) = Self::list_split(inner) else {
            return;
        };
        let list_h = rows.height as usize;
        // Nothing to list yet: say why, if the source knows. An empty box under a
        // typed query is the one state a picker cannot explain by itself - a bad
        // pattern and a genuine no-match look identical.
        if self.filtered.is_empty()
            && let Some(status) = self.source.as_ref().and_then(|s| s.status())
        {
            buf.set_stringn(
                rows.x,
                rows.y,
                format!("  {status}"),
                rows.width as usize,
                self.style,
            );
        }
        let scroll = self.list_scroll(list_h);
        for (row, &idx) in self.filtered.iter().enumerate().skip(scroll).take(list_h) {
            let y = rows.y + (row - scroll) as u16;
            let style = if row == self.selected {
                self.selected_style
            } else {
                self.style
            };
            let item = &self.items[idx];
            // The highlight's ground crosses the bar's column - the same carry the
            // body's current-line wash makes - so the selected row reads as one band
            // rather than one notched a cell short of the border. The bar paints after
            // this and sets only a foreground, so its glyphs survive the wash.
            let rect = Rect::new(inner.x, y, inner.width, 1);
            buf.set_style(rect, style);
            buf.set_stringn(
                rows.x,
                y,
                format!("  {}", item.label),
                rows.width as usize,
                style,
            );
            // The shortcut (if any) is drawn right-aligned, one cell in from the
            // border. Labels are short, so it does not collide with them.
            if let Some(shortcut) = &item.shortcut {
                let text = format!("{shortcut} ");
                let w = text.width() as u16;
                if w < rows.width {
                    buf.set_stringn(rows.x + rows.width - w, y, &text, w as usize, style);
                }
            }
        }
        // The bar, asked for from [`Self::bar`] rather than re-tested here, so what is
        // painted and what can be grabbed are the same question asked once: a track
        // with no thumb to pull would be a control over a list with nowhere to go.
        // Over the rows and not the query line, for the reason the body's runs over the
        // text and not the sticky header - the track stands for what scrolls.
        if let Some(bar) = self.bar(screen) {
            let (widget, mut state) = crate::layout::scrollbar(
                scroll,
                self.filtered.len() - list_h,
                list_h,
                self.track_style,
                self.thumb_style,
            );
            StatefulWidget::render(widget, bar, buf, &mut state);
        }
    }

    fn context(&self) -> Context {
        Context::Picker
    }

    fn handle_key(&mut self, key: KeyEvent, bound: Option<Bound>) -> EventResult {
        match bound {
            Some(Bound::Cancel) => self.cancel(),
            Some(Bound::Accept) => self.commit(),
            Some(Bound::PreviousItem) => self.selected = self.selected.saturating_sub(1),
            Some(Bound::NextItem) => {
                let last = self.filtered.len().saturating_sub(1);
                self.selected = (self.selected + 1).min(last);
            }
            Some(Bound::DeleteBackward) => {
                self.query.pop();
                self.refilter();
                self.selected = 0;
            }
            // `nop`, and anything the `picker` context should not have been able to
            // hold: swallowed here rather than deferred, because the chord *is* bound
            // - the table said this surface answers for it (SPEC §10.5).
            Some(_) => {}
            // Unbound. A printable key is the query - which is what a picker is for -
            // and everything else falls through to the context below, where an
            // unbound Esc finds `collapse_selections` and closes this picker on its
            // way past. That fall-through is why no config can lock a surface shut.
            None => match crate::keymap::text_key(&key) {
                Some(c) => {
                    self.query.push(c);
                    self.refilter();
                    self.selected = 0;
                }
                None => return EventResult::Ignored,
            },
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
                // The press decides whose gesture this is, and *always* answers: a
                // press that missed the bar ends any latch a lost release left behind,
                // rather than leaving one armed for the next drag to inherit.
                self.dragging = self
                    .bar(screen)
                    .is_some_and(|bar| bar.contains(Position::new(mouse.column, mouse.row)));
                if self.dragging {
                    // The bar is a control, not just a readout: a press anywhere on
                    // the track goes to that fraction of the list and holding drags
                    // it, which is the gesture a scrollbar owes the hand that grabs
                    // it. Checked before the rows, since the column is not one.
                    self.scroll_to_track(screen, mouse.row);
                } else if let Some(row) = self.row_at(screen, mouse.column, mouse.row) {
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
            // A drag only means the bar if the press that started it did. Anywhere
            // else it is a pointer moving over a modal box, which has nothing to
            // sweep: rows are chosen by the click that lands on them.
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                self.scroll_to_track(screen, mouse.row);
            }
            // Releasing ends the drag, the one thing this arm is here for. Also on a
            // release *outside* the box, which is where a fast drag ends up.
            MouseEventKind::Up(_) => self.dragging = false,
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
        // Held off while a drag is live, unlike every other way the highlight moves.
        // Both of these are I/O - a preview reloads a theme, a pane reads (and decodes)
        // a file, up to a megabyte of one for the project-search picker - and a drag
        // reports an event per cell the pointer crosses, so a hand wobbling on the bar
        // would queue one read per report. [`PreviewSource`] is documented as bounded
        // work per *move*, and a drag is one move however many cells it reports; the
        // release below clears the latch first, so the pane still lands on the row the
        // gesture finished on. The list itself scrolls throughout: `list_scroll` is
        // derived, so the rows and the thumb repaint on every event regardless.
        if !self.finished && !self.dragging {
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
        self.track_style = theme.scrollbar_track;
        self.thumb_style = theme.scrollbar_thumb;
    }

    fn tick(&mut self) -> bool {
        let Some(source) = self.source.as_mut() else {
            return false;
        };
        let arrived = source.take();
        // A source can change what it *says* without producing a row: a debounced
        // search starts, or fails to compile, on a tick of its own. Repainting only
        // on arrival would leave that line saying the wrong thing until the next
        // keystroke happened to redraw it. Compared borrowed and only copied when it
        // actually differs, since the answer is "unchanged" on almost every tick.
        let restated = source.status().as_deref() != self.status.as_deref();
        if restated {
            self.status = source.status().map(Cow::into_owned);
        }
        if arrived.is_empty() {
            return restated;
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
    use crate::compositor::send;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    use vortex_core::Action;

    fn items() -> Vec<Item> {
        [
            (
                "Save File",
                Some("Ctrl+S"),
                Command::Editor(Action::Save { force: false }),
            ),
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
        Picker::new("Test", items(), false, &Theme::default())
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(p: &mut Picker, s: &str) {
        for c in s.chars() {
            send(&mut *p, key(c));
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
        send(&mut p, press(KeyCode::Down));
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
        let mut p = long();
        for _ in 0..30 {
            send(&mut p, press(KeyCode::Down));
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
        send(&mut p, press(KeyCode::Up)); // at top - clamps
        assert_eq!(p.selected, 0);
        send(&mut p, press(KeyCode::Down));
        assert_eq!(p.selected, 1);
        for _ in 0..100 {
            send(&mut p, press(KeyCode::Down));
        }
        assert_eq!(p.selected, p.filtered.len() - 1);
    }

    #[test]
    fn enter_commits_the_highlighted_command_and_finishes() {
        let mut p = picker();
        type_str(&mut p, "quit");
        send(&mut p, press(KeyCode::Enter));
        assert!(p.is_finished());
        assert_eq!(p.take_commands(), vec![Command::Editor(Action::Quit)]);
    }

    #[test]
    fn ctrl_chord_is_deferred_not_typed() {
        // A Ctrl/Cmd chord is a shortcut, not filter input: the picker defers it (so
        // the loop runs the binding) and does not add it to the query. It defers
        // because the `picker` context does not *bind* it, not because it is a Ctrl
        // chord - the guard that used to say so is gone (SPEC §10.5).
        let mut p = picker();
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(send(&mut p, ctrl_s), EventResult::Ignored);
        assert!(p.query.is_empty(), "the chord must not filter");
        assert!(
            !p.is_finished(),
            "deferring does not close the picker itself"
        );
    }

    #[test]
    fn a_config_cannot_lock_the_picker_shut() {
        // Unbind `cancel` in `[keys.picker]` and Esc is unbound *there*. It is not
        // printable, so it is not taken as the query - so it falls through to the
        // editor context, where `collapse_selections` fires and the loop dismisses
        // the overlay on its way past. The way out survives a bad config by
        // construction rather than by a special case (SPEC §10.5).
        // A keymap whose `picker` context binds nothing at all - the worst a config
        // can do to this surface, since a row can be replaced but the table cannot be
        // emptied by hand.
        let stripped = crate::keymap::Keymap::from_pairs([("esc", "collapse_selections")]).unwrap();
        let esc = press(KeyCode::Esc);
        assert_eq!(stripped.bound(Context::Picker, esc), None);

        let mut p = picker();
        assert_eq!(
            p.handle_key(esc, stripped.bound(Context::Picker, esc)),
            EventResult::Ignored,
            "an unbound Esc is offered to the context below"
        );
        assert!(!p.is_finished(), "the picker did not answer for it");
        // What it finds down there is the editor's `collapse_selections`, and a binding
        // firing over an open overlay dismisses that overlay (the event loop's rule).
        assert!(
            crate::keymap::command_for_key(&stripped, esc, 10).is_some(),
            "the editor context answers, so the overlay is dismissed"
        );
    }

    #[test]
    fn a_nop_row_swallows_a_key_in_this_surface_alone() {
        // The other half of the rule: `nop` is how a table says "swallow this *here*",
        // as distinct from letting the chord fall through. Tab is `nop` in the built-in
        // `[picker]` table because Tab means completion everywhere a user has met a
        // picker, and falling through would type an indent into the buffer behind it.
        let keymap = crate::keymap::Keymap::default();
        let tab = press(KeyCode::Tab);
        assert_eq!(keymap.bound(Context::Picker, tab), Some(Bound::Nop));
        let mut p = picker();
        assert_eq!(send(&mut p, tab), EventResult::Consumed);
        assert!(p.query.is_empty(), "nop typed nothing");
        assert!(!p.is_finished());
    }

    #[test]
    fn a_key_the_picker_context_does_not_bind_falls_through() {
        // Left/Right are the editor's motions and mean nothing to a list, so the
        // picker declines them rather than swallowing them.
        let mut p = picker();
        for code in [KeyCode::Left, KeyCode::Right, KeyCode::Home] {
            assert_eq!(send(&mut p, press(code)), EventResult::Ignored, "{code:?}");
        }
        assert!(p.query.is_empty());
        assert!(!p.is_finished());
    }

    #[test]
    fn esc_cancels_with_no_command() {
        let mut p = picker();
        send(&mut p, press(KeyCode::Esc));
        assert!(p.is_finished());
        assert!(p.take_commands().is_empty());
    }

    #[test]
    fn a_picker_without_preview_stays_silent_until_enter() {
        // The palette and file picker must not emit as you arrow through them -
        // opening a file per row visited would be a disaster. Preview is opt-in.
        let mut p = picker();
        for code in [KeyCode::Down, KeyCode::Down, KeyCode::Up] {
            send(&mut p, press(code));
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

        send(&mut p, press(KeyCode::Down));
        assert_eq!(p.take_commands(), vec![Command::OpenPalette]); // row 1's command
        // A key that leaves the highlight where it is must not re-emit, or a held
        // Up at the top would fire the same preview over and over.
        send(&mut p, press(KeyCode::Up));
        assert_eq!(
            p.take_commands(),
            vec![Command::Editor(Action::Save { force: false })]
        );
        send(&mut p, press(KeyCode::Up));
        assert!(p.take_commands().is_empty(), "re-emitted without moving");

        // Cancelling emits the undo command; committing does not.
        send(&mut p, press(KeyCode::Esc));
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
            scrollbar_track: Style::new().fg(ratatui::style::Color::Rgb(7, 8, 9)),
            scrollbar_thumb: Style::new().fg(ratatui::style::Color::Rgb(10, 11, 12)),
            ..Theme::default()
        };
        p.restyle(&theme);
        assert_eq!(p.style, theme.palette);
        assert_eq!(p.selected_style, theme.palette_selected);
        // The bar too, or a picker open across a theme swap keeps a track in the
        // colours of the theme that is no longer on screen.
        assert_eq!(p.track_style, theme.scrollbar_track);
        assert_eq!(p.thumb_style, theme.scrollbar_thumb);
    }

    #[test]
    fn enter_on_an_empty_result_commits_nothing() {
        let mut p = picker();
        type_str(&mut p, "zzzq");
        send(&mut p, press(KeyCode::Enter));
        assert!(p.is_finished());
        assert!(p.take_commands().is_empty());
    }

    #[test]
    fn backspace_widens_the_filter_again() {
        let mut p = picker();
        type_str(&mut p, "quit");
        assert_eq!(p.filtered.len(), 1);
        for _ in 0..4 {
            send(&mut p, press(KeyCode::Backspace));
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
        send(&mut p, press(KeyCode::Down));
        assert_eq!(*asked.borrow(), vec!["Save File", "Open Palette"]);
        send(&mut p, press(KeyCode::Up));
        assert_eq!(asked.borrow().len(), 3);
        send(&mut p, press(KeyCode::Up)); // already at the top: nothing moved
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
        status: Status,
    }

    impl ItemSource for Fake {
        fn query(&mut self, query: &str) {
            self.asked.borrow_mut().push(query.to_string());
            self.pending.borrow_mut().clear();
        }
        fn take(&mut self) -> Vec<Item> {
            std::mem::take(&mut self.pending.borrow_mut())
        }
        fn status(&self) -> Option<Cow<'_, str>> {
            // Owned here only because the fake keeps its status behind a `RefCell`
            // for the test to change mid-run; a real source borrows from its state.
            self.status.borrow().clone().map(Cow::Owned)
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
    /// What a [`Fake`] says where the rows would be, changeable after the picker
    /// owns it - a real source restates itself on a tick, not on a keystroke.
    type Status = Rc<RefCell<Option<String>>>;

    /// A sourced picker plus handles on what it was asked and what it may receive.
    fn sourced() -> (Picker, Asked, Pending) {
        let asked = Rc::new(RefCell::new(Vec::new()));
        let pending = Rc::new(RefCell::new(Vec::new()));
        let source = Fake {
            asked: Rc::clone(&asked),
            pending: Rc::clone(&pending),
            status: Status::default(),
        };
        let p = Picker::new("Search", Vec::new(), false, &Theme::default())
            .with_item_source(Box::new(source));
        (p, asked, pending)
    }

    #[test]
    fn a_sourced_picker_asks_its_source_instead_of_fuzzy_filtering() {
        let (mut p, asked, _) = sourced();
        type_str(&mut p, "ab");
        assert_eq!(*asked.borrow(), vec!["a", "ab"], "asked on every keystroke");
        send(&mut p, press(KeyCode::Backspace));
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
        send(&mut p, press(KeyCode::Down));
        send(&mut p, press(KeyCode::Enter));
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
            status: Rc::new(RefCell::new(Some("searching…".to_string()))),
            ..Fake::default()
        };
        let p = Picker::new("Search", Vec::new(), false, &Theme::default())
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
    fn a_status_that_changes_without_a_row_still_asks_for_a_repaint() {
        // A debounced search starts - or turns out not to compile - on a tick of its
        // own, with no row to show for it. Repainting only on arrival would leave the
        // line where the rows go saying the wrong thing until the next keystroke.
        let status: Status = Rc::new(RefCell::new(None));
        let source = Fake {
            status: Rc::clone(&status),
            ..Fake::default()
        };
        let mut p = Picker::new("Search", Vec::new(), false, &Theme::default())
            .with_item_source(Box::new(source));
        assert!(!p.tick(), "nothing arrived and nothing was restated");
        *status.borrow_mut() = Some("searching…".to_string());
        assert!(p.tick(), "the line under the query changed");
        assert!(!p.tick(), "and stops asking once it has been painted");
    }

    #[test]
    fn a_path_under_the_working_directory_is_shown_relative() {
        // Long absolute paths push the distinguishing part off a narrow row, which
        // is the opposite of what a picker over same-named files is for.
        let cwd = std::path::PathBuf::from("/home/u/project");
        assert_eq!(
            display_path(&cwd.join("src/deep/thing.rs"), Some(&cwd)),
            "src/deep/thing.rs"
        );
    }

    #[test]
    fn a_path_outside_the_working_directory_stays_absolute() {
        // It must not read as a local file when it is not one.
        let cwd = std::path::PathBuf::from("/home/u/project");
        assert_eq!(
            display_path(Path::new("/etc/hosts"), Some(&cwd)),
            "/etc/hosts"
        );
        // And with no working directory to be relative to (deleted, or unreadable),
        // every row stays absolute rather than the picker losing its labels.
        assert_eq!(display_path(Path::new("/etc/hosts"), None), "/etc/hosts");
    }

    /// A picker over forty rows labelled `item-NN` - longer than any box this crate
    /// draws, so the list always overflows and the bar always has somewhere to go.
    fn long() -> Picker {
        let items = (0..40)
            .map(|k| Item {
                label: format!("item-{k:02}"),
                shortcut: None,
                command: Command::Editor(Action::Insert(format!("{k}"))),
            })
            .collect();
        Picker::new("Many", items, false, &Theme::default())
    }

    /// The rows and the bar's column, as [`Layer::render`] would place them.
    fn split(p: &Picker, screen: Rect) -> (Rect, Rect) {
        Picker::list_split(p.columns(screen).unwrap().0).unwrap()
    }

    /// What is painted down the bar's column, one symbol per row, top to bottom.
    fn bar_column(p: &Picker, screen: Rect) -> String {
        let buf = painted(p, screen);
        let (_, bar) = split(p, screen);
        (bar.y..bar.bottom())
            .map(|y| buf.cell((bar.x, y)).unwrap().symbol().to_string())
            .collect()
    }

    #[test]
    fn a_list_that_fits_its_box_gets_no_bar() {
        // A bar answers "where am I in something bigger than the box"; with nothing
        // bigger there is nothing for it to say, and a full-height thumb would be a
        // loud way of saying it.
        let column = bar_column(&picker(), SCREEN);
        assert!(column.trim().is_empty(), "{column:?}");
    }

    #[test]
    fn a_list_longer_than_its_box_gets_a_bar() {
        let column = bar_column(&long(), SCREEN);
        assert!(column.contains('█'), "a thumb: {column:?}");
        assert!(column.contains('║'), "with track around it: {column:?}");
    }

    #[test]
    fn the_thumb_travels_as_the_highlight_scrolls_the_list() {
        // The bar is a readout of the derived offset, so it has to move with it -
        // a thumb pinned at the top under a list scrolled to its end is worse than
        // no bar at all.
        let mut p = long();
        let top = bar_column(&p, SCREEN);
        assert_eq!(top.chars().next(), Some('█'), "starts at the top: {top:?}");
        for _ in 0..40 {
            send(&mut p, press(KeyCode::Down));
        }
        let bottom = bar_column(&p, SCREEN);
        assert_eq!(
            bottom.chars().last(),
            Some('█'),
            "and reaches the end: {bottom:?}"
        );
    }

    /// A left drag to a screen cell, and the release that ends it.
    fn drag(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn release(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn a_press_on_the_bar_scrolls_there_instead_of_picking() {
        // The track's ends are the list's ends: a press at the bottom of it shows the
        // end of the list, and never runs the row it happened to land beside.
        let mut p = long();
        let (_, bar) = split(&p, SCREEN);
        assert_eq!(
            p.handle_mouse(click(bar.x, bar.bottom() - 1), SCREEN),
            EventResult::Consumed
        );
        assert!(!p.is_finished(), "a press on the bar is not a pick");
        assert!(p.take_commands().is_empty());
        assert_eq!(selected_label(&p), "item-39", "scrolled to the end");

        p.handle_mouse(click(bar.x, bar.y), SCREEN);
        let (rows, _) = split(&p, SCREEN);
        assert_eq!(
            p.list_scroll(rows.height as usize),
            0,
            "and back to the start"
        );
    }

    #[test]
    fn dragging_the_bar_tracks_the_pointer_down_the_track() {
        let mut p = long();
        let (rows, bar) = split(&p, SCREEN);
        p.handle_mouse(click(bar.x, bar.y), SCREEN);
        let mut seen = Vec::new();
        for y in bar.y..bar.bottom() {
            p.handle_mouse(drag(bar.x, y), SCREEN);
            seen.push(p.list_scroll(rows.height as usize));
        }
        assert!(
            seen.windows(2).all(|w| w[0] <= w[1]),
            "the offset only ever moved down: {seen:?}"
        );
        assert_eq!(seen.first(), Some(&0), "from the top");
        assert_eq!(
            seen.last(),
            Some(&(p.filtered.len() - rows.height as usize)),
            "to the end"
        );
    }

    #[test]
    fn a_drag_that_strays_off_the_track_keeps_scrolling() {
        // Mouse mode reports a drag per cell crossed, and a hand pulling a one-column
        // track will leave it. The press decides whose gesture this is; the drags
        // after it inherit that rather than dying a cell to the left.
        let mut p = long();
        let (rows, bar) = split(&p, SCREEN);
        p.handle_mouse(click(bar.x, bar.y), SCREEN);
        p.handle_mouse(drag(0, bar.bottom() - 1), SCREEN);
        assert_eq!(
            p.list_scroll(rows.height as usize),
            p.filtered.len() - rows.height as usize
        );
        assert!(!p.is_finished(), "straying off is not a click-away cancel");
    }

    #[test]
    fn a_drag_that_did_not_start_on_the_bar_scrolls_nothing() {
        // Otherwise a pointer wandering across an open picker with the button down
        // would throw the list about.
        let mut p = long();
        let (rows, bar) = split(&p, SCREEN);
        p.handle_mouse(drag(bar.x, bar.bottom() - 1), SCREEN);
        assert_eq!(p.list_scroll(rows.height as usize), 0, "nothing moved");
    }

    #[test]
    fn releasing_ends_the_drag() {
        let mut p = long();
        let (rows, bar) = split(&p, SCREEN);
        p.handle_mouse(click(bar.x, bar.y), SCREEN);
        p.handle_mouse(release(bar.x, bar.y), SCREEN);
        p.handle_mouse(drag(bar.x, bar.bottom() - 1), SCREEN);
        assert_eq!(
            p.list_scroll(rows.height as usize),
            0,
            "the drag was over when the button came up"
        );
    }

    #[test]
    fn a_bar_gesture_previews_once_when_it_is_let_go() {
        // Preview is I/O - the theme picker reloads a theme file per emitted command -
        // and a drag reports an event per cell crossed. So the gesture previews what it
        // *landed* on, once, rather than every theme it swept past on the way.
        let mut p = long().previewing(Command::OpenPalette);
        let (_, bar) = split(&p, SCREEN);
        p.handle_mouse(click(bar.x, bar.y), SCREEN);
        p.handle_mouse(drag(bar.x, bar.bottom() - 1), SCREEN);
        assert!(
            p.take_commands().is_empty(),
            "silent while the button is down"
        );
        p.handle_mouse(release(bar.x, bar.bottom() - 1), SCREEN);
        assert_eq!(
            p.take_commands(),
            vec![Command::Editor(Action::Insert("39".to_string()))],
            "and previews the row it finished on"
        );
    }

    #[test]
    fn a_drag_does_not_refill_the_preview_pane_until_it_ends() {
        // The pane's source reads (and decodes) a file - up to a megabyte of one for
        // the project-search picker - so one read per reported cell is what a hand
        // resting on the bar would otherwise cost.
        let asked = Rc::new(RefCell::new(Vec::new()));
        let log = Rc::clone(&asked);
        let mut p = long().with_preview_pane(Box::new(move |item, _| {
            log.borrow_mut().push(item.label.clone());
            Vec::new()
        }));
        let (_, bar) = split(&p, SCREEN);
        asked.borrow_mut().clear(); // the fill on open
        p.handle_mouse(click(bar.x, bar.y), SCREEN);
        for y in bar.y..bar.bottom() {
            p.handle_mouse(drag(bar.x, y), SCREEN);
        }
        assert!(asked.borrow().is_empty(), "read a file mid-drag");
        p.handle_mouse(release(bar.x, bar.bottom() - 1), SCREEN);
        assert_eq!(
            *asked.borrow(),
            vec!["item-39"],
            "exactly one read, for the row the drag finished on"
        );
    }

    #[test]
    fn a_press_that_misses_the_bar_disarms_a_stranded_drag() {
        // A release can go missing - a terminal that drops the report, a button let go
        // off-window. The next press answers the question again rather than inheriting
        // a latch, so the picker cannot be left permanently in a drag.
        let mut p = long();
        let (rows, bar) = split(&p, SCREEN);
        p.handle_mouse(click(bar.x, bar.y), SCREEN); // arms it
        let scrolled = p.list_scroll(rows.height as usize);
        // A press on the query row: furniture, so the picker stays open, but it is not
        // the bar - and no release ever arrives.
        p.handle_mouse(click(rows.x, rows.y - 1), SCREEN);
        assert!(!p.dragging, "the latch did not survive a press that missed");
        p.handle_mouse(drag(bar.x, bar.bottom() - 1), SCREEN);
        assert_eq!(
            p.list_scroll(rows.height as usize),
            scrolled,
            "the stranded drag moved nothing"
        );
    }

    #[test]
    fn the_bars_column_is_reserved_whether_or_not_it_paints() {
        // A list overflows and stops overflowing as you type. A column that came and
        // went with it would re-clip every label and re-place every shortcut on the
        // keystroke that changed the match count.
        let (fits, _) = split(&picker(), SCREEN);
        let (overflows, bar) = split(&long(), SCREEN);
        assert_eq!(fits, overflows, "the rows do not move when a bar appears");
        // And the column is the bar's in both: a click there is furniture, not the
        // row it abuts, even with nothing drawn in it.
        let mut p = picker();
        assert_eq!(p.row_at(SCREEN, bar.x, bar.y), None);
        p.handle_mouse(click(bar.x, bar.y), SCREEN);
        assert!(!p.is_finished());
        assert!(p.take_commands().is_empty());
    }

    #[test]
    fn a_row_too_long_for_its_width_stops_short_of_the_bar() {
        // Clipped to the rows rather than to the list column: a label running under
        // the track would be half-overwritten by it, which reads as a corrupt row
        // rather than as a long one.
        let items = (0..40)
            .map(|k| Item {
                label: "x".repeat(200),
                shortcut: Some("y".repeat(200)),
                command: Command::Editor(Action::Insert(format!("{k}"))),
            })
            .collect();
        let p = Picker::new("Wide", items, false, &Theme::default());
        let column = bar_column(&p, SCREEN);
        assert!(
            column.chars().all(|c| c == '█' || c == '║'),
            "the label reached the bar's column: {column:?}"
        );
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
