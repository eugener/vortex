//! Viewport math and display-column layout - the frontend's coordinate work
//! (SPEC §4, §5).
//!
//! The core deals in byte/grapheme/line-column spaces; **display columns**
//! (terminal cells, with tab expansion and wide-character width) are the
//! frontend's job (SPEC §4). The core never assumes 1 char = 1 cell. These are
//! pure functions so they are unit-testable without a terminal (SPEC §13).
//!
//! The frontend owns the viewport: which lines are visible and the scroll offset
//! are computed here from the primary cursor and the terminal size, with **zero
//! round-trips to the core** (the anti-Xi rule, SPEC §5).

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::Path;

use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use vortex_core::{BufferId, BufferInfo, FileFormat, Selection, Text};

use crate::config::LineNumbers;

/// Shown in the head bar when the buffer has no bound file (SPEC §10 lifecycle).
pub const NO_NAME: &str = "[No Name]";

/// Minimum digit field for the line-number gutter, so even a short file gets a
/// tidy left margin instead of a cramped single column.
const MIN_GUTTER_DIGITS: usize = 3;

/// The primary cursor's on-screen position, derived from the buffer text and the
/// primary caret's byte offset. Returns `(line, byte_col_within_line, line_text)`.
///
/// Defensive (SPEC §8): an out-of-range `head` is clamped to the buffer end and an
/// empty buffer yields line 0 / column 0 / empty text, so a bad offset renders in
/// the corner rather than panicking. Kept here (not in the I/O shell) so it is
/// unit-testable without a terminal (SPEC §13).
pub fn cursor_line_col(text: &Text, head: usize) -> (usize, usize, String) {
    let pos = text.position_of_byte(head);
    let line_text = text.line(pos.line).unwrap_or_default();
    (pos.line, pos.col, line_text)
}

/// Cells one grapheme occupies starting from display column `col`: a tab advances
/// to the next `tab_width` stop; any other grapheme takes its `unicode-width`.
/// The single source of truth for tab-stop semantics, shared by [`display_column`]
/// and [`expand_tabs`] so the cursor column and painted glyphs can never drift.
fn cells_for(grapheme: &str, col: usize, tab_width: usize) -> usize {
    if grapheme == "\t" {
        tab_width - (col % tab_width) // to the next multiple, at least one cell
    } else {
        grapheme.width()
    }
}

/// Display width (terminal cells) of the prefix of `line` up to `byte_col`.
///
/// Tabs expand to the next `tab_width` stop; wide characters (CJK, emoji) count
/// as their `unicode-width`. `byte_col` must be a grapheme boundary within the
/// line; it is clamped to the line length defensively. This maps the core's
/// byte/grapheme column to the cell the cursor should paint in (SPEC §4).
pub fn display_column(line: &str, byte_col: usize, tab_width: usize) -> usize {
    let end = byte_col.min(line.len());
    line[..end]
        .graphemes(true)
        .fold(0, |col, g| col + cells_for(g, col, tab_width))
}

/// Byte column within `line` nearest display column `target` - the inverse of
/// [`display_column`], for mapping a pointer's cell back to a caret position. Walks
/// graphemes accumulating cells; when `target` falls on a grapheme, the nearer edge
/// wins (past the midpoint rounds to the following boundary) so a click on the right
/// half of a wide glyph or a tab lands where the pointer visually is. Clamped to the
/// line's content length (a click past the last glyph goes to end-of-line).
pub fn byte_col_at_display(line: &str, target: usize, tab_width: usize) -> usize {
    let mut col = 0;
    for (byte_idx, g) in line.grapheme_indices(true) {
        let w = cells_for(g, col, tab_width);
        if target < col + w {
            return if target - col >= w.div_ceil(2) {
                byte_idx + g.len()
            } else {
                byte_idx
            };
        }
        col += w;
    }
    line.len()
}

/// Buffer byte offset for a pointer at body-relative `(row, col)` cells - the
/// inverse of the paint math, kept here so the pointer->position mapping is
/// unit-testable without a terminal (SPEC §13). `row` is 0-based within the text
/// body (the caller has already subtracted the head bar) and is clamped to the last
/// line; `col` is an absolute body column, so a click in the gutter
/// (`col < gutter_width`) lands at the line start. Both scroll offsets are the
/// frontend's current viewport, so the lookup needs no core round-trip (SPEC §5).
pub fn offset_at_cell(
    text: &Text,
    scroll: usize,
    h_scroll: usize,
    gutter_width: usize,
    tab_width: usize,
    row: usize,
    col: usize,
) -> usize {
    let line = (scroll + row).min(display_line_count(text).saturating_sub(1));
    let line_start = text.byte_of_line(line).unwrap_or(0);
    let byte_col = if col < gutter_width {
        0 // a click in the gutter selects the start of the line
    } else {
        let raw = text.line(line).unwrap_or_default();
        byte_col_at_display(&raw, col - gutter_width + h_scroll, tab_width)
    };
    line_start + byte_col
}

/// Expand tabs in `line` to spaces at `tab_width` stops, so the painted glyphs
/// occupy the same cells [`display_column`] computes for the cursor. Without this
/// the terminal advances tabs to *its own* stops while the cursor uses ours, and
/// the two drift apart (a real "cursor off after a tab" bug). Non-tab graphemes
/// are copied verbatim; wide chars keep their width because only tabs are
/// rewritten.
pub fn expand_tabs(line: &str, tab_width: usize) -> String {
    if !line.contains('\t') {
        return line.to_string(); // fast path: most lines have no tabs
    }
    let mut out = String::with_capacity(line.len());
    let mut col = 0;
    for g in line.graphemes(true) {
        if g == "\t" {
            // Fill with the same cell count display_column charges for this tab.
            let fill = cells_for(g, col, tab_width);
            out.extend(std::iter::repeat_n(' ', fill));
            col += fill;
        } else {
            out.push_str(g);
            col += cells_for(g, col, tab_width);
        }
    }
    out
}

/// Number of display lines - every line the cursor can reach, each of which gets
/// a row and a gutter number. This includes the empty line after a trailing
/// newline (press Enter at end of file) and the sole line of an empty buffer.
///
/// crop's [`Text::line_count`] omits both: it counts `""` as 0 lines and does not
/// count the empty line following a final `"\n"` (a trailing newline is a
/// *terminator*, not a new line). So `"a\nb\n"` is 2 to the rope but 3 lines on
/// screen. [`Text::last_line_index`] is the core's canonical "last line a cursor
/// can be on" - the same ceiling vertical motion uses - so gutter numbering and
/// the navigable range can never disagree (SPEC §4).
pub fn display_line_count(text: &Text) -> usize {
    text.last_line_index() + 1
}

/// A visible row, fetched from the rope once. The painter needs the line in two
/// forms: `raw` (tabs intact) drives byte<->display-column mapping for selections
/// and carets, while the tab-`expanded` form is what actually gets painted.
/// Returning both here means the caller never re-fetches (a second rope traversal)
/// the line it just tab-checked.
pub struct VisibleLine {
    /// The line as stored, tabs intact.
    pub raw: String,
    /// Tab-expanded text to paint, present only when it differs from `raw` (a
    /// tab-free line paints as-is, so no second full-line copy is made).
    pub expanded: Option<String>,
}

impl VisibleLine {
    /// The text to paint: the tab-expanded form, or `raw` when it had no tabs.
    pub fn display(&self) -> &str {
        self.expanded.as_deref().unwrap_or(&self.raw)
    }
}

/// The visible lines to paint: the `height` rows starting at `scroll`, each with
/// tabs expanded to `tab_width` stops so glyphs align with the cursor column.
/// Bounded by [`display_line_count`] so a trailing empty line (and the empty
/// buffer's sole line) still gets a row; a line past the rope's content resolves
/// to `""`. Stops at the last display line (no blank padding - the terminal
/// backend clears unused rows). Pure and line-bounded (SPEC §10.4), so the
/// viewport slice is unit-testable rather than buried in the draw closure (§13).
pub fn visible_lines(
    text: &Text,
    scroll: usize,
    height: usize,
    tab_width: usize,
) -> Vec<VisibleLine> {
    (scroll..display_line_count(text))
        .take(height)
        .map(|i| {
            let raw = text.line(i).unwrap_or_default();
            // Only tab-bearing lines need a rewrite; a tab-free line paints as-is
            // (`expanded` stays None) to avoid a second full-line copy per row.
            let expanded = raw.contains('\t').then(|| expand_tabs(&raw, tab_width));
            VisibleLine { raw, expanded }
        })
        .collect()
}

/// The glyph an indent guide paints (SPEC §7.5). A *character*, unlike a ruler's
/// ground tint, because the two markers want different cells: a ruler's cell is the
/// one a long line is already using, while a guide's is by construction whitespace,
/// so a guide can occupy its cell without displacing anything. It is one cell wide,
/// so it stands in for the space it replaces without shifting the columns after it.
///
/// Deliberately not the same glyph as the scrollbar's track (`║`): the two are both
/// full-height vertical rules, and a reader should not have to work out which of
/// them is in the margin.
const INDENT_GUIDE: &str = "│";

/// How far **past the window** [`indent_guides`] will look for a blank line's indent
/// before giving up. Bounded because nothing off the viewport may scan the whole buffer
/// (SPEC §10.4); a blank row whose nearest non-blank neighbour is further away than this
/// gets no guides, which is the honest answer, since past a screenful of nothing "what
/// block is this inside" has stopped being a question the eye is asking.
///
/// It bounds **only** the search outside the window. The search *within* it is already
/// bounded by the viewport's own height, which is the thing §10.4 is about - so a run of
/// blank lines that fits on screen still inherits from whatever encloses it, however
/// long the run is.
const GUIDE_SCAN: usize = 64;

/// Display width of `line`'s leading whitespace, or `None` when the line is blank
/// (empty, or whitespace all the way to its end).
///
/// A blank line reports `None` rather than a width because the whitespace it carries
/// is trailing, not leading, and so says nothing about which block it sits in. What
/// it should show is inherited instead ([`indent_guides`]) - without that, the blank
/// line between two statements would punch a hole through every guide crossing it,
/// which is exactly the case that makes the feature look broken.
/// **Indentation is spaces and tabs, not every character Unicode calls whitespace.**
/// That is narrower than `char::is_whitespace` on purpose, and the narrowness is what
/// makes the two halves of the feature agree: [`expand_tabs`] turns a tab into spaces,
/// so a prefix of spaces and tabs is a prefix of *spaces* by the time it is painted,
/// and every column [`indent_guides`] hands to [`with_indent_guides`] is therefore a
/// cell holding `' '`. Counting a NO-BREAK SPACE as indentation would break that: the
/// guide's column would land on a character that cannot be replaced, leaving the cell
/// recolored but unmarked. A line indented with those is simply not indented here.
pub fn indent_width(line: &str, tab_width: usize) -> Option<usize> {
    let mut col = 0;
    for g in line.graphemes(true) {
        if !matches!(g, " " | "\t") {
            return Some(col);
        }
        col += cells_for(g, col, tab_width);
    }
    None
}

/// The display columns to draw an indent guide at, for each row of the window
/// `lines` (already fetched by [`visible_lines`], starting at line `scroll`).
///
/// A line's guides sit at every tab stop strictly inside its indentation - so a line
/// indented 8 with 4-wide tabs gets guides at columns 0 and 4, and its text starts at
/// 8. **Column 0 is included**: it is the left edge of the block the indented text is
/// inside, which is the one level a reader cannot otherwise see, and dropping it would
/// leave a singly-indented line with no guide at all.
///
/// A blank line inherits the *shallower* of its nearest non-blank neighbours, so a
/// guide runs through the gaps inside a block but stops at the blank line that follows
/// its last statement rather than running past the closing brace.
///
/// The neighbour search is answered from `lines` where it can be, and crosses into
/// `text` only at the window's edges - resolved once for the whole frame rather than
/// per row, since every blank row that runs off an edge wants the same answer and a
/// screenful of blank lines would otherwise pay a rope fetch each (SPEC §10.4).
pub fn indent_guides(
    text: &Text,
    lines: &[VisibleLine],
    scroll: usize,
    tab_width: usize,
) -> Vec<Vec<usize>> {
    // A zero tab width has no stops to hang guides on, and `step_by(0)` panics.
    if tab_width == 0 {
        return vec![Vec::new(); lines.len()];
    }
    let own: Vec<Option<usize>> = lines
        .iter()
        .map(|line| indent_width(&line.raw, tab_width))
        .collect();
    // Only a blank row can ever consult the lines outside the window, and most windows
    // have none - so the rope is not touched at all unless one does. Each search then
    // stops at the first non-blank line, which in real text is the very next one; the
    // `GUIDE_SCAN` bound only costs anything inside a long run of blanks.
    let (above_window, below_window) = if own.iter().any(Option::is_none) {
        let outside = |index: usize| indent_width(&text.line(index).unwrap_or_default(), tab_width);
        let after = scroll + own.len();
        (
            (scroll.saturating_sub(GUIDE_SCAN)..scroll)
                .rev()
                .find_map(outside),
            (after..(after + GUIDE_SCAN).min(display_line_count(text))).find_map(outside),
        )
    } else {
        (None, None)
    };

    own.iter()
        .enumerate()
        .map(|(row, &indent)| {
            let indent = indent.unwrap_or_else(|| {
                let above = own[..row].iter().rev().find_map(|&i| i).or(above_window);
                let below = own[row + 1..].iter().find_map(|&i| i).or(below_window);
                // A missing neighbour is column 0, not "ignore this side": a blank
                // line at the very top of the file is inside nothing.
                above.unwrap_or(0).min(below.unwrap_or(0))
            });
            (0..indent).step_by(tab_width).collect()
        })
        .collect()
}

/// The stretch of `columns` falling inside the horizontal window
/// `[h_scroll, h_scroll + width)` - the guides a row can actually show.
///
/// A **subslice, not a filter**: [`indent_guides`] produces ascending columns, so the
/// window is a contiguous range within them and clipping costs two binary searches and
/// no allocation.
///
/// Worth doing rather than leaving to the painter's own clipping, because the guides a
/// row is offered are one per tab stop of its *indentation*, and indentation is a length
/// the file chooses. An absurdly indented line otherwise makes the frame pay for
/// thousands of guides twice over - once walking cells in [`with_indent_guides`], and
/// again in [`render_line`], whose `style_at` scans every overlay for every painted
/// cell - to draw none of them. That is the §10.4 rule: work in a frame is bounded by
/// the viewport, never by the buffer.
///
/// The clipping is **invisible in the painted output**, which is the whole point - it
/// removes work, not marks. That is also why it is a function with its own test rather
/// than three lines inside the paint loop: nothing downstream can observe whether it
/// happened.
pub fn guides_in_window(columns: &[usize], h_scroll: usize, width: usize) -> &[usize] {
    let first = columns.partition_point(|&col| col < h_scroll);
    let last = columns.partition_point(|&col| col < h_scroll.saturating_add(width));
    &columns[first..last]
}

/// `line` (tab-expanded, as painted) with an indent guide standing in for the
/// whitespace at each of `columns`.
///
/// Substituted into the text rather than pushed as a style, because a guide is a
/// glyph and [`render_line`]'s overlays only restyle the cell they cover. That is
/// sound here for the reason a ruler's tint is not: `columns` come from
/// [`indent_guides`] and so always land in indentation, which is whitespace - the
/// guide replaces a space and takes its width, so nothing after it moves. The raw
/// line is untouched, so every byte↔column mapping still measures the buffer's own
/// text rather than what the row happens to show.
///
/// The tail pads out to the deepest guide, because an *inherited* indent can reach
/// past a blank line's own end, where the cells the guide wants do not exist yet.
///
/// `columns` must be **ascending**, which is what [`indent_guides`] produces. Both
/// walks below advance a single cursor through it rather than searching it per cell -
/// the trick [`ColumnWalker`] plays for syntax spans, and for the same reason: the
/// query sequence is monotonic, so a re-scan per cell is pure waste. It is not a
/// micro-optimization here. This runs per visible row per frame, and the length it
/// walks is a line's *indentation* - a number the file chooses, not the viewport - so
/// searching per cell makes the paint quadratic in something an input controls.
pub fn with_indent_guides<'a>(line: &'a str, columns: &[usize]) -> Cow<'a, str> {
    let Some(&deepest) = columns.iter().max() else {
        return Cow::Borrowed(line);
    };
    let mut out = String::with_capacity(line.len() + columns.len() * INDENT_GUIDE.len());
    let mut col = 0;
    // The cursor, shared by both loops: `col` only grows across the two, so `next`
    // never has to walk back.
    let mut next = 0;
    // Everything past the deepest guide is copied verbatim, so the walk stops there and
    // hands the remainder over in one piece. Guides only ever land in a line's
    // indentation, so this is a few cells of a line that is often hundreds - and it is
    // per visible row, per frame.
    let mut tail = "";
    for (at, g) in line.grapheme_indices(true) {
        if col > deepest {
            tail = &line[at..];
            break;
        }
        while next < columns.len() && columns[next] < col {
            next += 1;
        }
        // The cell is a space whenever `columns` came from `indent_guides`, which only
        // counts spaces and tabs as indentation and so cannot name a column holding
        // anything else. The guard keeps that a property of this function rather than
        // of its caller: a column list of someone else's devising recolors nothing it
        // could not also replace.
        if g == " " && columns.get(next) == Some(&col) {
            out.push_str(INDENT_GUIDE);
        } else {
            out.push_str(g);
        }
        // A wide grapheme can step straight over a guide column; the cursor skips it on
        // the next pass, which is right - a guide cannot be drawn inside a glyph.
        col += g.width();
    }
    out.push_str(tail);
    // Reached only when the line ended inside the guides (`tail` is then empty): an
    // inherited indent can reach past a blank line's own end.
    while col <= deepest {
        while next < columns.len() && columns[next] < col {
            next += 1;
        }
        out.push_str(if columns.get(next) == Some(&col) {
            INDENT_GUIDE
        } else {
            " "
        });
        col += 1;
    }
    Cow::Owned(out)
}

/// Keep index `cursor` visible within a window of `size` starting at `offset`,
/// returning the new offset. Generic 1-D scroll shared by both axes (SPEC §5):
/// pass `(cursor_line, top, rows)` for vertical scroll or
/// `(cursor_col, left, cols)` for horizontal. Scrolls toward the cursor by the
/// minimum needed; a zero-size window never scrolls.
pub fn scroll_to_show(cursor: usize, offset: usize, size: usize) -> usize {
    if size == 0 {
        return offset;
    }
    if cursor < offset {
        cursor
    } else if cursor >= offset + size {
        // Put the cursor on the last visible cell of the window.
        cursor + 1 - size
    } else {
        offset
    }
}

/// The scroll offset a press on row `row` of a `track`-tall scrollbar asks for,
/// given the buffer's largest useful offset (SPEC §7.5).
///
/// The track's ends are the buffer's ends: row 0 is the top of the file and the last
/// row is `max_scroll`, with everything between them linear. The obvious alternative -
/// put the *thumb's top* where the pointer is - cannot reach the bottom at all, since
/// the thumb's own height is then always left over below it.
///
/// This is not at odds with grabbing the thumb, which is the gesture it has to feel
/// like: the thumb covers the fraction of the track that the viewport covers of the
/// buffer, and under this mapping the pointer sits **inside** it at every offset - so a
/// drag reads as dragging the thumb rather than as throwing it.
///
/// That holds while the thumb is drawn proportionally, which is while the buffer is
/// under `track²` lines - some 1600 for a full-height terminal. Past that the thumb has
/// collapsed to its one-cell floor, one row of track is worth several screens of text,
/// and the pointer can land a cell off the thumb it is dragging. Nothing better is
/// available at that ratio: no single cell can stand for the viewport, so the two
/// roundings have nothing left to agree on. Reaching both ends of the file is the
/// property worth keeping there, and it is the one this keeps.
///
/// `None` when the track is too short to mean anything - a single row (a three-line
/// terminal) has every offset and no way to tell them apart. Answering `0` there would
/// throw a reader at line 900 back to line 1 for touching the only cell the bar has,
/// which is worse than the press doing nothing.
pub fn scroll_at_track_row(row: usize, track: usize, max_scroll: usize) -> Option<usize> {
    // Also the guard against dividing by a zero span.
    let span = track.checked_sub(1).filter(|&s| s > 0)?;
    // Rounded rather than truncated, so the row nearest an offset selects it instead
    // of the whole track drifting one line toward the top.
    Some((row.min(span) * max_scroll + span / 2) / span)
}

/// The editor's vertical scrollbar and the state that places its thumb (SPEC §7.5):
/// a track over content with `max_scroll` offsets, `viewport` rows of it on screen,
/// currently at `scroll`.
///
/// **Both halves come from here because they are one decision.** `content_length`
/// counts scroll *positions* rather than lines, and pairing that with
/// `viewport_content_length` is what works the thumb out to the fraction of the track
/// the window covers of the content, sitting at the fraction the offset has travelled.
/// That is exactly the geometry [`scroll_at_track_row`] inverts when a press comes back
/// the other way, and its promise that the pointer sits inside the thumb it is dragging
/// rests on it. Split across the two places that draw a bar - the body and the picker
/// overlay - the mapping and its inverse could drift with no compile error and no
/// failing test, leaving the thumb somewhere other than under the hand pulling it.
pub fn scrollbar(
    scroll: usize,
    max_scroll: usize,
    viewport: usize,
    track: Style,
    thumb: Style,
) -> (Scrollbar<'static>, ScrollbarState) {
    let state = ScrollbarState::new(max_scroll + 1)
        .position(scroll)
        .viewport_content_length(viewport);
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        // No arrow heads: they would eat two of the track's rows to offer a line-step
        // this editor already binds to a key and a wheel, and neither a short body nor
        // an eighteen-row picker has two rows to spare.
        .begin_symbol(None)
        .end_symbol(None)
        .track_style(track)
        .thumb_style(thumb);
    (bar, state)
}

/// Where a screen row falls in the editor's vertical split (SPEC §7.5).
///
/// The split is `paint`'s: the head bar owns row 0, the sticky context header the
/// `header_height` rows under it, the text the rest of the body, and the status bar
/// the last row. **Every hit test asks this rather than re-deriving it** - the four
/// that once did (`on_scrollbar`, `pointer_offset`, the scrollbar drag, and the
/// header's own press) agreed only by inspection, and the SPEC's structural-debt
/// note named sticky context as exactly the change that would make them disagree
/// with no compile error and no failing test.
///
/// The status bar is *not* distinguished here: it is a row the callers below never
/// receive (the event loop answers it first), and telling it apart would mean
/// carrying the screen height into a question none of them otherwise needs it for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    /// Row 0 - the head bar / bufferline.
    Head,
    /// A pinned sticky context row, by index from the top of the header.
    Header(usize),
    /// A text row, by index from the top of the *text* area (not the body).
    Text(usize),
}

/// Which part of the vertical split `screen_row` lands in, given the rows the
/// sticky context header took on the frame being hit-tested.
pub fn row_at(screen_row: u16, header_height: usize) -> Row {
    // Saturating rather than `screen_row + 1` anywhere: the row arrives from a
    // terminal, and a report of `u16::MAX` must be an answer rather than an
    // overflow on an input path.
    match (screen_row as usize).checked_sub(1) {
        None => Row::Head,
        Some(body) if body < header_height => Row::Header(body),
        Some(body) => Row::Text(body - header_height),
    }
}

/// The most rows the sticky context header will ever claim (SPEC §7.5, M8).
///
/// A cap is needed because nesting is a length the *file* chooses (§10.4) - a
/// deeply nested match arm inside a closure inside a method would otherwise pin
/// half the screen. Four is where the header stops answering "what am I inside"
/// and starts being an outline: the innermost few scopes are the ones a reader
/// has actually lost track of, and the module three levels out is not.
pub const STICKY_CONTEXT_MAX: usize = 4;

/// The header rows the sticky context feature would claim in a body of `height`
/// rows: [`STICKY_CONTEXT_MAX`], but never more than a third of the body.
///
/// The fraction is what keeps the feature honest on a short terminal: pinning
/// four rows of a twelve-row body spends a third of the file's own space on
/// where you are rather than what you are reading, and on a six-row body it would
/// leave two.
pub fn sticky_budget(height: usize) -> usize {
    STICKY_CONTEXT_MAX.min(height / 3)
}

/// The buffer lines to pin above the viewport: the first line of each scope
/// enclosing `top_line`, outermost first, at most `budget` of them (SPEC §7.5).
///
/// Every line returned is strictly above `top_line`, because a scope that starts
/// at or after the top row already has its own first line on screen
/// (`DecorationSet::scopes_at` is what draws that boundary) - so the header never
/// repeats a line the text below it is showing.
///
/// **When the chain is deeper than `budget`, the outermost rows are dropped, not
/// the innermost.** The row a reader needs is the function they are inside; the
/// module three levels out is the one they still know. Dropping from the other
/// end would spend the whole budget answering the question nobody asked.
///
/// Two scopes opening on one line (`impl Foo { fn bar()` written flat) collapse to
/// one row: the header is a list of *lines*, and a line pinned twice reads as a
/// duplicate rather than as depth.
pub fn sticky_lines(
    text: &Text,
    decorations: &vortex_core::DecorationSet,
    top_line: usize,
    budget: usize,
) -> Vec<usize> {
    if budget == 0 {
        return Vec::new();
    }
    // The scopes are byte ranges, so the question is asked in bytes: the first
    // byte of the top row. A line past the buffer's end encloses nothing.
    let Some(top_byte) = text.byte_of_line(top_line) else {
        return Vec::new();
    };
    let mut lines: Vec<usize> = Vec::new();
    for scope in decorations.scopes_at(top_byte) {
        let line = text.line_of_byte(scope.start);
        // Ascending by construction (the bucket is sorted by start), so a repeat
        // can only be the previous one.
        if lines.last() != Some(&line) {
            lines.push(line);
        }
    }
    // Keep the innermost `budget`.
    if lines.len() > budget {
        lines.drain(..lines.len() - budget);
    }
    lines
}

/// Render the tab-expanded `line` into styled spans for the display-column window
/// `[h_scroll, h_scroll + width)` - the frontend's one intra-line styling seam,
/// shared by selection highlighting now and syntax highlighting (M4) later.
///
/// Every cell in the window is emitted: content graphemes, then padding spaces
/// past the line's end, so `base` fills the *whole* width - the mechanism behind
/// the current-line tint. Each `overlay` (a display-column range plus a [`Style`])
/// patches `base` for the cells it covers, later overlays winning; a zero-overlay
/// call is just the clipped line. A wide grapheme (CJK/emoji) straddling either
/// edge is replaced by spaces for its visible cells so columns after it stay
/// aligned with the cursor (SPEC §4: display width != character count). Consecutive
/// equal-style cells coalesce into one span to keep the count low.
pub fn render_line(
    line: &str,
    h_scroll: usize,
    width: usize,
    base: Style,
    overlays: &[(Range<usize>, Style)],
) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let end = h_scroll + width;
    let mut runs: Vec<(String, Style)> = Vec::new();
    let mut col = 0;
    for g in line.graphemes(true) {
        let w = g.width();
        let g_end = col + w;
        if g_end <= h_scroll {
            // Entirely left of the window.
        } else if col >= end {
            break; // entirely right of the window; nothing further can fit
        } else if col >= h_scroll && g_end <= end {
            push_run(&mut runs, g, style_at(col, base, overlays)); // fully inside
        } else {
            // Straddles an edge: one styled space per visible cell so a partially
            // clipped wide glyph does not misalign the columns after it.
            for c in col.max(h_scroll)..g_end.min(end) {
                push_run(&mut runs, " ", style_at(c, base, overlays));
            }
        }
        col = g_end;
    }
    // Pad the window past the line's content so `base` (and any overlay covering
    // these cells - e.g. a selection that consumed the trailing newline) fills the
    // remaining width. An empty range (content already past the window) adds nothing.
    for c in col.max(h_scroll)..end {
        push_run(&mut runs, " ", style_at(c, base, overlays));
    }
    runs.into_iter()
        .map(|(text, style)| Span::styled(text, style))
        .collect()
}

/// Append `s` to the last run when it shares `style`, else start a new run. Keeps
/// [`render_line`] emitting one span per style change rather than one per cell.
fn push_run(runs: &mut Vec<(String, Style)>, s: &str, style: Style) {
    match runs.last_mut() {
        Some((buf, last)) if *last == style => buf.push_str(s),
        _ => runs.push((s.to_string(), style)),
    }
}

/// The style for display column `col`: `base` with every overlay whose range
/// covers `col` patched over it in order (later overlays win).
fn style_at(col: usize, base: Style, overlays: &[(Range<usize>, Style)]) -> Style {
    let mut style = base;
    for (range, overlay) in overlays {
        if range.contains(&col) {
            style = style.patch(*overlay);
        }
    }
    style
}

/// A left-to-right byte->display-column resolver for one line, so a *sorted*
/// sequence of queries costs a single grapheme pass instead of a fresh scan from
/// byte 0 per query. Built for the syntax-highlight hot path (SPEC §5): a
/// minified line can carry hundreds of spans, and resolving each independently is
/// O(spans * line_length) - one shared walker makes the line's whole span set
/// O(line_length + spans).
///
/// Queries must be **non-decreasing** in byte offset. The walker only advances;
/// a target behind the cursor returns the cursor's current column, not a rescan.
/// Highlights satisfy this (sorted, non-overlapping - see
/// [`vortex_core::DecorationSet::highlights_in`]); overlapping or unsorted spans
/// must use [`display_column`] directly.
pub struct ColumnWalker<'a> {
    graphemes: std::iter::Peekable<unicode_segmentation::GraphemeIndices<'a>>,
    /// Display column at `byte` - the accumulated width of every grapheme consumed.
    col: usize,
    tab_width: usize,
    len: usize,
}

impl<'a> ColumnWalker<'a> {
    /// A walker positioned at the start of `line` (column 0).
    pub fn new(line: &'a str, tab_width: usize) -> Self {
        Self {
            graphemes: line.grapheme_indices(true).peekable(),
            col: 0,
            tab_width,
            len: line.len(),
        }
    }

    /// Display column at byte offset `target` (a grapheme boundary, clamped to the
    /// line length). Consumes every remaining grapheme that starts before `target`,
    /// so it matches [`display_column`] exactly for non-decreasing `target`s.
    pub fn column_at(&mut self, target: usize) -> usize {
        let target = target.min(self.len);
        while let Some(&(idx, g)) = self.graphemes.peek() {
            if idx >= target {
                break;
            }
            self.col += cells_for(g, self.col, self.tab_width);
            self.graphemes.next();
        }
        self.col
    }
}

/// The display-column range a span covers on one buffer line, resolved against a
/// shared [`ColumnWalker`] - the [`selection_columns`] body, factored out so the
/// sorted highlight spans on a line share one forward pass. `line_len` is the
/// line's byte length, `line_start` its first byte offset, `line_end_excl` the
/// next line's start (or buffer end) so the terminator bytes are included. See
/// [`selection_columns`] for the newline-cell semantics; the walker's
/// non-decreasing contract applies to successive calls.
pub fn span_columns(
    walker: &mut ColumnWalker,
    line_len: usize,
    line_start: usize,
    line_end_excl: usize,
    span_start: usize,
    span_end: usize,
) -> Option<Range<usize>> {
    let content_end = line_start + line_len;
    let lo = span_start.max(line_start);
    let hi = span_end.min(line_end_excl);
    if lo >= hi {
        return None;
    }
    let lo_col = walker.column_at((lo - line_start).min(line_len));
    let hi_content = hi.min(content_end);
    let hi_col = walker.column_at((hi_content - line_start).min(line_len));
    // A span reaching past the content consumed this line's newline.
    let end_col = if hi > content_end { hi_col + 1 } else { hi_col };
    (lo_col < end_col).then_some(lo_col..end_col)
}

/// The display-column range a selection covers on one buffer line, or `None` when
/// the selection does not touch the line or is a zero-width cursor here (a cursor
/// renders as the terminal caret, not a highlight). `line` is the line's raw text
/// (tabs intact), `line_start` its first byte offset, and `line_end_excl` the next
/// line's start (or buffer end) so the line's terminator bytes are included.
///
/// When the selection runs through this line's terminator (its end lies past the
/// content), the range gets one extra cell so the consumed line break is visible
/// and blank lines inside a multi-line selection still show a highlight.
///
/// A one-shot walk over the line; when resolving many sorted spans on one line
/// (syntax highlights), build a single [`ColumnWalker`] and call [`span_columns`]
/// instead so the line is walked once, not once per span.
pub fn selection_columns(
    line: &str,
    line_start: usize,
    line_end_excl: usize,
    tab_width: usize,
    sel_start: usize,
    sel_end: usize,
) -> Option<Range<usize>> {
    let mut walker = ColumnWalker::new(line, tab_width);
    span_columns(
        &mut walker,
        line.len(),
        line_start,
        line_end_excl,
        sel_start,
        sel_end,
    )
}

/// Total grapheme clusters covered by `selections`, for the status readout when a
/// selection is active. Counts user-perceived characters (not bytes), line
/// terminators excluded; zero-width cursors contribute nothing. Bounded by the
/// selected span, materializing only its lines (SPEC §10.4). Cost is O(selected
/// bytes), so the event loop computes it once per snapshot and carries the value
/// across repaints rather than re-walking the selection every frame.
pub fn selected_grapheme_count(text: &Text, selections: &[Selection]) -> usize {
    selections
        .iter()
        .map(|s| grapheme_count_in(text, s.start(), s.end()))
        .sum()
}

/// Grapheme clusters in the byte range `[start, end)`, summed line by line so no
/// slice wider than one line is ever materialized.
fn grapheme_count_in(text: &Text, start: usize, end: usize) -> usize {
    if start >= end {
        return 0;
    }
    let mut count = 0;
    for line_idx in text.line_of_byte(start)..=text.line_of_byte(end) {
        let Some(line_start) = text.byte_of_line(line_idx) else {
            break;
        };
        let content = text.line(line_idx).unwrap_or_default();
        let lo = start.max(line_start);
        let hi = end.min(line_start + content.len());
        if lo < hi {
            count += content[lo - line_start..hi - line_start]
                .graphemes(true)
                .count();
        }
    }
    count
}

/// Columns the line-number gutter occupies: a right-aligned digit field (at least
/// [`MIN_GUTTER_DIGITS`] wide, widening for larger files) plus one space
/// separating the numbers from the text. Sized from the largest line number so
/// the gutter width never jitters as the cursor moves within a file. `line_count`
/// is the display count (see [`display_line_count`]), always >= 1.
pub fn gutter_width(line_count: usize) -> usize {
    digit_count(line_count.max(1)).max(MIN_GUTTER_DIGITS) + 1
}

/// Base-10 digit count of `n` (n >= 1), via the integer `ilog10` intrinsic (no
/// floating-point `log10`). The `n >= 1` precondition holds: `gutter_width` passes
/// `line_count.max(1)`, so `ilog10`'s zero-input panic is unreachable.
fn digit_count(n: usize) -> usize {
    n.ilog10() as usize + 1
}

/// The gutter text for the buffer line at 0-based `line_index`, right-aligned in
/// `gutter_width` columns with the trailing separator space. `gutter_width`
/// includes that space, so the digit field is one narrower.
///
/// Under [`LineNumbers::Relative`] the number is the row's distance from
/// `cursor_line` - the count you would type before a motion - except on the
/// cursor's own row, which keeps its absolute number (see [`LineNumbers`]). The
/// *width* is sized from the buffer either way, deliberately: a field that shrank
/// to fit the relative numbers would resize the gutter every time the cursor
/// crossed a power of ten, sliding the whole text body sideways under the reader.
pub fn gutter_label(
    line_index: usize,
    cursor_line: usize,
    gutter_width: usize,
    mode: LineNumbers,
) -> String {
    let field = gutter_width.saturating_sub(1);
    let number = match mode {
        LineNumbers::Absolute => line_index + 1,
        LineNumbers::Relative if line_index == cursor_line => line_index + 1,
        LineNumbers::Relative => line_index.abs_diff(cursor_line),
    };
    format!("{number:>field$} ")
}

/// 1-based grapheme column of `byte_col` within `line`, for the status readout.
/// Columns count grapheme clusters (user-perceived characters), not bytes, so a
/// multi-byte character advances the count by one, not by its byte length
/// (SPEC §4). `byte_col` is clamped to the line length defensively.
pub fn grapheme_column(line: &str, byte_col: usize) -> usize {
    let end = byte_col.min(line.len());
    line[..end].graphemes(true).count() + 1
}

/// The buffer's display name for the head bar: the file name of `path` (not the
/// full path, to keep the bar short), or [`NO_NAME`] when unnamed. A modified
/// buffer is prefixed with `● ` so unsaved work is visible at a glance (SPEC §8,
/// §10). A path ending in `..`/`/` (no file name component) falls back to its
/// lossy full form rather than the placeholder.
pub fn buffer_display_name(path: Option<&Path>, modified: bool) -> String {
    let name = match path {
        None => NO_NAME.to_string(),
        Some(p) => p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.to_string_lossy().into_owned()),
    };
    with_modified_marker(&name, modified)
}

/// The unsaved-work marker, prefixed to a buffer's name wherever one is shown.
/// One home for the glyph so the bufferline and the buffer picker cannot disagree
/// about it - they label differently (file name vs full path) but mark identically.
pub fn with_modified_marker(name: &str, modified: bool) -> String {
    if modified {
        format!("{MODIFIED_MARKER} {name}")
    } else {
        name.to_string()
    }
}

/// Shown before the name of a buffer with unsaved edits (SPEC §8, §10).
pub const MODIFIED_MARKER: &str = "●";

/// The head bar's right-hand segment: the buffer's display line count (see
/// [`display_line_count`], always >= 1). The `.max(1)` is a defensive floor so a
/// stray 0 still reads "1 line" rather than an empty count.
pub fn line_count_label(line_count: usize) -> String {
    match line_count.max(1) {
        1 => "1 line ".to_string(),
        n => format!("{n} lines "),
    }
}

/// Marker shown where the tab strip continues past the visible window.
const TAB_OVERFLOW: &str = "›";
/// …and on the left-hand side.
const TAB_OVERFLOW_LEFT: &str = "‹";
/// Divider painted between adjacent tabs. One cell, so it costs a column per gap;
/// padding alone was not enough to keep adjacent names from reading as one string.
const TAB_SEPARATOR: &str = "│";

/// What a bufferline segment *is*, which decides both how it paints and whether a
/// pointer landing on it selects anything.
///
/// An explicit kind rather than a nullable id: the strip already has three sorts of
/// segment and this bar is where M8's git state and diagnostic counts land, so a
/// consumer that forgets one should fail to compile rather than fall into a
/// catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment {
    /// A buffer's tab, and whether it is the one on screen.
    Tab { id: BufferId, active: bool },
    /// A separator or an overflow marker: painted dim, selects nothing.
    Chrome,
}

/// One segment of the painted bufferline.
///
/// Carries the buffer it stands for and its measured width, not just its text, so
/// neither the painter nor [`tab_at_column`] has to re-walk the label to lay the
/// strip out or to resolve a click against it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    /// The text to paint, already padded and fitted. `Cow` so the separators and
    /// overflow markers - which are `&'static str` and repaint every frame - cost no
    /// allocation at all.
    pub label: Cow<'static, str>,
    /// The label's display width, computed once where it was already known.
    pub cells: usize,
    pub kind: Segment,
}

impl Tab {
    /// A non-selectable segment: a separator or an overflow marker.
    fn chrome(label: &'static str) -> Self {
        Self {
            label: Cow::Borrowed(label),
            cells: label.width(),
            kind: Segment::Chrome,
        }
    }

    /// The buffer this segment selects, if it is a tab at all.
    pub fn id(&self) -> Option<BufferId> {
        match self.kind {
            Segment::Tab { id, .. } => Some(id),
            Segment::Chrome => None,
        }
    }

    /// Whether this is the tab of the buffer currently on screen.
    pub fn is_active(&self) -> bool {
        matches!(self.kind, Segment::Tab { active: true, .. })
    }
}

/// The bufferline's tab strip fitted to `width` display cells, in buffer order,
/// with a [`TAB_SEPARATOR`] between adjacent tabs (SPEC §7.5 head/tab bar).
///
/// Every tab is shown when they all fit. When they do not, the strip is windowed
/// **around the active tab** - which is the only tab guaranteed to be worth seeing -
/// with `‹`/`›` markers where it continues. Scrolling to follow the active buffer
/// rather than truncating from the right is what keeps the current file visible once
/// more buffers are open than the terminal is wide.
///
/// The active tab is returned flagged, not styled: the painter fills it with the
/// theme's accent, which is what separates "current" from "open" at a glance without
/// depending on foreground brightness alone.
///
/// Widths are display cells throughout (SPEC §4), so CJK names and emoji in
/// filenames account for the columns they really occupy. The returned labels,
/// separators and markers never sum past `width`.
pub fn bufferline(buffers: &[BufferInfo], active: BufferId, width: usize) -> Vec<Tab> {
    if width == 0 || buffers.is_empty() {
        return Vec::new();
    }
    // One `String` per tab, built once and moved into the strip. Everything else in
    // the strip is `&'static str`, so a repaint allocates per *buffer*, not per
    // segment.
    let names = disambiguated(buffers);
    let labels: Vec<String> = buffers
        .iter()
        .zip(&names)
        .map(|(info, name)| tab_label(name, info.modified))
        .collect();
    let widths: Vec<usize> = labels.iter().map(|l| l.width()).collect();
    let separator = TAB_SEPARATOR.width();
    // An `active` not in the list (a snapshot older than a close) falls back to the
    // first tab rather than panicking or showing nothing.
    let active_index = buffers.iter().position(|b| b.id == active).unwrap_or(0);

    let tab = |index: usize, label: String, cells: usize| Tab {
        label: Cow::Owned(label),
        cells,
        kind: Segment::Tab {
            id: buffers[index].id,
            active: index == active_index,
        },
    };

    // n tabs need n-1 separators, so the gaps are part of what has to fit.
    let gaps = separator * buffers.len().saturating_sub(1);
    if widths.iter().sum::<usize>() + gaps <= width {
        let mut strip = Vec::with_capacity(buffers.len() * 2);
        for (index, label) in labels.into_iter().enumerate() {
            if index > 0 {
                strip.push(Tab::chrome(TAB_SEPARATOR));
            }
            let cells = widths[index];
            strip.push(tab(index, label, cells));
        }
        return strip;
    }

    // Grow a window outward from the active tab, preferring to reveal what follows
    // it, and always leaving room for the overflow markers the window will need.
    // Each tab admitted brings its own separator with it.
    let (mut first, mut last) = (active_index, active_index);
    let mut used = widths[active_index].min(width);
    loop {
        let markers = usize::from(first > 0) + usize::from(last + 1 < buffers.len());
        let fits = |extra: usize| used + markers + extra <= width;
        if last + 1 < buffers.len() && fits(widths[last + 1] + separator) {
            used += widths[last + 1] + separator;
            last += 1;
        } else if first > 0 && fits(widths[first - 1] + separator) {
            used += widths[first - 1] + separator;
            first -= 1;
        } else {
            break;
        }
    }

    // The window was grown against the same budget it is emitted into, so everything
    // in it is already known to fit and needs no second accounting. The one case that
    // does not follow is a lone active tab wider than the whole bar - nothing could
    // have been admitted beside it - which is the only place truncation can happen.
    let markers = usize::from(first > 0) + usize::from(last + 1 < buffers.len());
    let mut strip = Vec::with_capacity((last - first) * 2 + 3);
    if first > 0 {
        strip.push(Tab::chrome(TAB_OVERFLOW_LEFT));
    }
    if first == last {
        let (text, cells) = truncate_to_cells(&labels[first], width.saturating_sub(markers));
        if cells > 0 {
            strip.push(tab(first, text, cells));
        }
    } else {
        for index in first..=last {
            if index > first {
                strip.push(Tab::chrome(TAB_SEPARATOR));
            }
            let cells = widths[index];
            strip.push(tab(index, labels[index].clone(), cells));
        }
    }
    // The trailing marker is only dropped when the leading one already took the bar's
    // single column.
    if last + 1 < buffers.len() && width > usize::from(first > 0) {
        strip.push(Tab::chrome(TAB_OVERFLOW));
    }
    strip
}

/// A name per buffer that no two tabs share (SPEC §7.5, M10).
///
/// A file name alone is the right label almost always, and useless in the case that
/// matters: `tui/layout.rs` open beside `core/layout.rs` gives two tabs both reading
/// `layout.rs`, so the strip stops telling you which one you are in - the one question
/// the head bar exists to answer.
///
/// **The shortest parent that separates them**, not the full path. A colliding pair
/// grows one directory at a time until the group is distinct or the paths run out;
/// every *other* tab keeps its bare file name, so disambiguating one pair does not
/// lengthen the whole strip. Paths that are equal all the way up (the same file opened
/// twice) stop when they run out rather than looping.
///
/// Returns names without the modified marker - that is [`with_modified_marker`]'s job,
/// and applying it here would make the collision test compare decorated strings.
fn disambiguated(buffers: &[BufferInfo]) -> Vec<String> {
    // How many trailing path components each label shows. Grown only for the tabs
    // that need it.
    let mut depth = vec![1usize; buffers.len()];
    let label = |index: usize, depth: usize| -> String {
        match buffers[index].path.as_deref() {
            None => NO_NAME.to_string(),
            Some(path) => tail(path, depth),
        }
    };
    loop {
        // Group by the label as it currently stands; any group above one is still
        // ambiguous. Unnamed buffers are excluded - they are all `[No Name]` and no
        // amount of path can separate them, which is what the `None` arm above says.
        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for index in 0..buffers.len() {
            if buffers[index].path.is_some() {
                groups
                    .entry(label(index, depth[index]))
                    .or_default()
                    .push(index);
            }
        }
        let mut grew = false;
        for (_, members) in groups.iter().filter(|(_, m)| m.len() > 1) {
            for &index in members {
                // Only a path with more left to show can grow; one that has run out
                // stays as it is, so an identical pair terminates instead of looping.
                let path = buffers[index].path.as_deref();
                if path.is_some_and(|p| p.components().count() > depth[index]) {
                    depth[index] += 1;
                    grew = true;
                }
            }
        }
        if !grew {
            return (0..buffers.len()).map(|i| label(i, depth[i])).collect();
        }
    }
}

/// The last `depth` components of `path`, joined the way the platform writes them.
fn tail(path: &Path, depth: usize) -> String {
    let components: Vec<_> = path.components().collect();
    let start = components.len().saturating_sub(depth.max(1));
    components[start..]
        .iter()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join(std::path::MAIN_SEPARATOR_STR)
}

/// A tab's painted label: the buffer's display name with one cell of padding either
/// side. Uniform padding on purpose - widening the active tab would reflow the whole
/// strip sideways on every switch.
fn tab_label(name: &str, modified: bool) -> String {
    let name = with_modified_marker(name, modified);
    let mut label = String::with_capacity(name.len() + 2);
    label.push(' ');
    label.push_str(&name);
    label.push(' ');
    label
}

/// The head bar's tab strip at terminal `width`, with the line-count segment's
/// cells reserved on the right.
///
/// The single place that reservation is applied, so the painter and the pointer
/// handler cannot disagree about where a tab sits - a click resolving against a
/// differently-fitted strip would select the wrong buffer.
pub fn head_bar_tabs(
    buffers: &[BufferInfo],
    active: BufferId,
    count_width: usize,
    width: usize,
) -> Vec<Tab> {
    bufferline(buffers, active, width.saturating_sub(count_width))
}

/// The buffer whose tab covers display `column` of the head bar, or `None` for a
/// column past the strip or on an overflow marker (SPEC §5: the frontend owns
/// screen->intent mapping, so the core sees only the chosen buffer).
pub fn tab_at_column(tabs: &[Tab], column: usize) -> Option<BufferId> {
    let mut start = 0;
    for tab in tabs {
        let end = start + tab.cells;
        if column < end {
            return tab.id();
        }
        start = end;
    }
    None
}

/// A byte count as a compact human-readable size: plain bytes under 1 KB, then
/// `KB`/`MB`/`GB` (1024-based) with one decimal, so the status bar stays short for
/// large buffers (`12_345_678` -> `11.8MB`). No space before the unit, matching the
/// other status metrics. `GB` is the ceiling - a text buffer never realistically
/// exceeds it, and Tier-3 huge-file handling is future work (SPEC §10.4).
pub fn human_size(bytes: usize) -> String {
    const UNITS: [&str; 3] = ["KB", "MB", "GB"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1}{}", UNITS[unit])
}

/// What the status bar is told about the frame it is describing.
///
/// A struct rather than eight positional arguments: the bar answers one question
/// (*where am I, and how is this written?*) out of several unrelated facts, and a
/// call site that passes them in the wrong order still compiles when they are all
/// `usize`.
#[derive(Debug, Clone, Copy)]
pub struct StatusInfo<'a> {
    /// 1-based caret line and column, for display.
    pub line: usize,
    pub col: usize,
    /// Grapheme count of the active selection; `0` when nothing is selected.
    pub selected: usize,
    /// How many carets there are. Shown above one, because the position readout
    /// describes the *primary* as though cursors were singular (SPEC §2.2).
    pub cursors: usize,
    /// The file's on-disk encoding and line terminator (SPEC §10.1).
    pub format: FileFormat,
    /// What the Tab key inserts, already rendered (`spaces:4`).
    pub indent: &'a str,
    /// The diagnostic under a *rested* caret, if any - it takes the right segment
    /// over the format readout. `None` while the caret is moving, which is what
    /// stops the segment strobing.
    pub diagnostic: Option<&'a str>,
    pub read_only: bool,
}

/// Status-bar segments `(left, right)` = (*where am I*, *how is this written*).
///
/// **What is not here is the design** (SPEC §7.5, M10): a chrome cell is spent on
/// what the user cannot otherwise learn. The buffer's byte size went, because the
/// filesystem answers it and it is never the question mid-edit; the document version
/// went, because it was instrumentation that §5 scoped to "while the delta model is
/// young" and it is no longer young. What stays is unlearnable from the screen -
/// the format, because the buffer holds UTF-8 with LF whatever the file holds, so
/// nothing else can tell you a CRLF latin-1 file was preserved; and the indent
/// style, because it silently decides what the Tab key inserts.
///
/// The cursor count and the selection size obey the **presence rule**: absent at
/// nominal, present when they have something to say. A segment reading `1 cursor`
/// forever teaches the eye to skip the place the interesting number will appear.
///
/// `read_only` leads the **left** segment, ahead of the cursor position. Placement
/// is the whole point: [`fit_bar`] drops the right segment first and then truncates
/// the left from its *end*, so the front of the left segment is the only spot that
/// survives any width - and of everything on this bar, "your edits will be refused"
/// is what a user must not have to widen the terminal to discover.
pub fn status_bar(info: StatusInfo<'_>) -> (String, String) {
    let lock = if info.read_only { " [read-only] " } else { "" };
    let mut left = format!("{lock} Ln {}, Col {}", info.line, info.col);
    if info.selected > 0 {
        left.push_str(&format!("  ({} selected)", info.selected));
    }
    if info.cursors > 1 {
        left.push_str(&format!("  {} cursors", info.cursors));
    }
    (left, status_right(info).0)
}

/// Something on the status bar that can be clicked (SPEC §7.5).
///
/// The bar is a readout, and these are the parts of it that are also a *control*:
/// both are settings the file carries and the detector may have guessed, so the
/// place that shows the answer is the place to change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTarget {
    Encoding,
    LineEnding,
}

/// The status bar's right segment, plus the display-column span each clickable word
/// occupies *within it*.
///
/// Built in one pass with the string so a span cannot describe a different layout
/// than the one painted - the same rule the picker's rows and the toast stack
/// follow. Absolute columns are the caller's to work out, because only the caller
/// knows where [`fit_bar`] put the segment.
fn status_right(info: StatusInfo<'_>) -> (String, [(Range<usize>, StatusTarget); 2]) {
    // A rested caret's diagnostic takes the whole segment. Nothing in it is
    // clickable then - the format words are not painted, and a span that described
    // where they *would* be is exactly the disagreement this function exists to
    // prevent, so the ranges collapse to empty.
    if let Some(message) = info.diagnostic {
        return (
            format!("{message} "),
            [
                (0..0, StatusTarget::Encoding),
                (0..0, StatusTarget::LineEnding),
            ],
        );
    }
    let bom = if info.format.bom { " BOM" } else { "" };
    let encoding = format!("{}{bom}", info.format.encoding_name());
    let eol = info.format.eol.name();
    let text = format!("{encoding} · {eol} · {} ", info.indent);
    // Both words sit at the front, separated by " · " (three display columns).
    let encoding_span = 0..encoding.width();
    let eol_start = encoding_span.end + SEPARATOR_WIDTH;
    let eol_span = eol_start..eol_start + eol.width();
    (
        text,
        [
            (encoding_span, StatusTarget::Encoding),
            (eol_span, StatusTarget::LineEnding),
        ],
    )
}

/// Display width of the `" · "` the status bar's right segment joins its fields
/// with. Named because the click spans have to step over it in exactly the width it
/// paints - the middle dot is one column, not the three bytes it takes to store.
const SEPARATOR_WIDTH: usize = 3;

/// Which clickable part of the status bar display column `column` falls on, if any.
///
/// Returns `None` for the position readout, the indent readout, the gaps - for every
/// column while a rested diagnostic has taken the segment - and for every column when
/// the bar is too narrow to have shown the right segment at all, since [`fit_bar`]
/// drops it there and nothing that is not painted can be clicked.
pub fn status_target(
    left: &str,
    info: StatusInfo<'_>,
    width: usize,
    column: usize,
) -> Option<StatusTarget> {
    let (right, spans) = status_right(info);
    let start = right_placement(left, &right, width)?;
    let within = column.checked_sub(start)?;
    spans
        .iter()
        .find(|(span, _)| span.contains(&within))
        .map(|(_, target)| *target)
}

/// Compose a bar of exactly `width` display cells: `left` flush to the start,
/// `right` flush to the end, spaces between. Returning the full-width string means
/// the caller's background fill covers every cell with no gaps.
///
/// When the two cannot both fit with a one-space gap, the right segment is dropped
/// and the left is truncated to `width` - the left half (name / cursor position)
/// is the more important one to keep. Truncation is grapheme-aware so a multi-byte
/// cluster is never split (SPEC §4).
/// The display column [`fit_bar`] would place `right` at, or `None` when it would
/// drop it for want of room.
///
/// The one home for that rule, so a hit test can never believe a segment is on
/// screen that the paint left off (or the reverse).
fn right_placement(left: &str, right: &str, width: usize) -> Option<usize> {
    if width == 0 {
        return None;
    }
    fits_beside(truncate_to_cells(left, width).1, right, width)
}

/// [`right_placement`] for a caller that has already measured the left segment - the
/// paint, which needs the truncated text anyway. The rule stays in one place while
/// `fit_bar` stops walking the left segment a second time on every frame, only to
/// recompute a cell count it is already holding.
fn fits_beside(left_cells: usize, right: &str, width: usize) -> Option<usize> {
    let right_cells = right.width();
    (left_cells + 1 + right_cells <= width).then(|| width - right_cells)
}

pub fn fit_bar(left: &str, right: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let (left_text, left_cells) = truncate_to_cells(left, width);
    let right_cells = right.width();
    // Room for both plus at least one separating space?
    if fits_beside(left_cells, right, width).is_some() {
        let gap = width - left_cells - right_cells;
        let mut out = String::with_capacity(left_text.len() + gap + right.len());
        out.push_str(&left_text);
        out.extend(std::iter::repeat_n(' ', gap));
        out.push_str(right);
        out
    } else {
        // Right cannot fit: pad the (possibly truncated) left to full width.
        let mut out = left_text;
        out.extend(std::iter::repeat_n(' ', width - left_cells));
        out
    }
}

/// Longest grapheme-boundary prefix of `s` fitting in `max_cells` display columns,
/// with its actual cell width. Never splits a cluster (SPEC §4).
fn truncate_to_cells(s: &str, max_cells: usize) -> (String, usize) {
    let mut out = String::new();
    let mut cells = 0;
    for g in s.graphemes(true) {
        let w = g.width();
        if cells + w > max_cells {
            break;
        }
        out.push_str(g);
        cells += w;
    }
    (out, cells)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_display_column_is_byte_column() {
        assert_eq!(display_column("hello", 0, 4), 0);
        assert_eq!(display_column("hello", 3, 4), 3);
        assert_eq!(display_column("hello", 5, 4), 5);
    }

    #[test]
    fn tab_expands_to_next_stop() {
        // At col 0 a tab jumps to 4 (tab_width). "a\t" -> 'a' at 0, tab fills to 4.
        assert_eq!(display_column("\t", 1, 4), 4);
        assert_eq!(display_column("a\t", 2, 4), 4);
        assert_eq!(display_column("ab\t", 3, 4), 4);
        assert_eq!(display_column("abcd\t", 5, 4), 8); // already at a stop -> +4
    }

    #[test]
    fn wide_chars_take_two_cells() {
        // Each CJK char is 2 cells wide. "日本" prefix of 3 bytes = one char = 2.
        assert_eq!(display_column("日本", 3, 4), 2);
        assert_eq!(display_column("日本", 6, 4), 4);
    }

    #[test]
    fn zwj_emoji_is_one_grapheme() {
        // A ZWJ family renders as a single (wide) grapheme; the whole cluster is
        // measured as one unit, not per code point.
        let family = "👨‍👩‍👧";
        // width of the cluster is implementation-defined but stable and > 0;
        // the point is it is measured once, and the byte length maps to it.
        let w = display_column(family, family.len(), 4);
        assert!(
            w >= 2,
            "emoji cluster should occupy at least 2 cells, got {w}"
        );
    }

    #[test]
    fn byte_col_clamped_to_line_length() {
        assert_eq!(display_column("hi", 99, 4), 2);
    }

    #[test]
    fn byte_col_at_display_is_inverse_of_display_column() {
        // Round-trips on the boundaries of a plain ASCII line.
        assert_eq!(byte_col_at_display("hello", 0, 4), 0);
        assert_eq!(byte_col_at_display("hello", 3, 4), 3);
        // Past the last glyph clamps to end-of-line.
        assert_eq!(byte_col_at_display("hello", 99, 4), 5);
    }

    #[test]
    fn byte_col_at_display_rounds_to_nearer_edge_of_a_wide_glyph() {
        // "日本": each glyph is 2 cells. A click on the left cell of "本" (col 2)
        // lands before it (byte 3); the right cell (col 3) lands after it (byte 6).
        assert_eq!(byte_col_at_display("日本", 2, 4), 3);
        assert_eq!(byte_col_at_display("日本", 3, 4), 6);
    }

    #[test]
    fn byte_col_at_display_handles_tabs() {
        // "a\tb": 'a' at col 0, tab spans cols 1..4, 'b' at col 4. A click at col 4
        // rounds onto 'b' (byte 2); a click at col 1 stays before the tab (byte 1).
        assert_eq!(byte_col_at_display("a\tb", 4, 4), 2);
        assert_eq!(byte_col_at_display("a\tb", 1, 4), 1);
    }

    #[test]
    fn offset_at_cell_maps_row_and_column_to_a_buffer_offset() {
        let t = text_of("ab\ncdef");
        // Gutter 4 wide, no scroll. Body row 1 is "cdef" (starts at byte 3); column
        // 4 is the gutter edge -> 'c' (offset 3), column 6 -> 'e' (offset 5).
        assert_eq!(offset_at_cell(&t, 0, 0, 4, 4, 1, 4), 3);
        assert_eq!(offset_at_cell(&t, 0, 0, 4, 4, 1, 6), 5);
    }

    #[test]
    fn offset_at_cell_click_in_gutter_is_line_start() {
        let t = text_of("ab\ncdef");
        // Any column inside the 4-wide gutter maps to the line's first byte.
        assert_eq!(offset_at_cell(&t, 0, 0, 4, 4, 1, 1), 3);
    }

    #[test]
    fn offset_at_cell_accounts_for_scroll() {
        let t = text_of("l0\nl1\nl2\nl3");
        // Scrolled down 2 lines: body row 0 is "l2" (byte 6); its start via a gutter
        // click is offset 6.
        assert_eq!(offset_at_cell(&t, 2, 0, 4, 4, 0, 0), 6);
        // Horizontal scroll of 1 shifts the column mapping: on "l3" (starts byte 9),
        // gutter edge col 4 + h_scroll 1 = display col 1 -> the '3' at offset 10.
        assert_eq!(offset_at_cell(&t, 3, 1, 4, 4, 0, 4), 10);
    }

    #[test]
    fn offset_at_cell_clamps_row_past_the_end() {
        let t = text_of("only");
        // A body row below the content clamps to the last line.
        assert_eq!(offset_at_cell(&t, 0, 0, 4, 4, 50, 4), 0);
    }

    #[test]
    fn scroll_stays_when_cursor_visible() {
        assert_eq!(scroll_to_show(5, 3, 10), 3); // 5 within [3, 13)
    }

    #[test]
    fn scroll_up_when_cursor_above_top() {
        assert_eq!(scroll_to_show(2, 5, 10), 2);
    }

    #[test]
    fn scroll_down_when_cursor_below_bottom() {
        // window height 10, top 0 shows lines 0..9; cursor at 12 -> top 3 (3..12).
        assert_eq!(scroll_to_show(12, 0, 10), 3);
    }

    #[test]
    fn scroll_cursor_on_last_visible_row_is_stable() {
        // Cursor exactly on the bottom row stays put.
        assert_eq!(scroll_to_show(9, 0, 10), 0);
    }

    #[test]
    fn scroll_zero_height_is_noop() {
        assert_eq!(scroll_to_show(5, 2, 0), 2);
    }

    #[test]
    fn scroll_to_show_works_for_horizontal_axis() {
        // Same helper drives horizontal scroll: cursor col 20, window of 10 cols
        // from left 0 -> scroll right so col 20 sits on the last cell (left 11).
        assert_eq!(scroll_to_show(20, 0, 10), 11);
        // Cursor col 3 left of a left=5 window -> scroll left to 3.
        assert_eq!(scroll_to_show(3, 5, 10), 3);
    }

    /// Concatenated text of an unstyled [`render_line`] over the window - the
    /// clipping/padding behavior, ignoring styles.
    fn rendered(line: &str, h_scroll: usize, width: usize) -> String {
        render_line(line, h_scroll, width, Style::default(), &[])
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    /// The style [`render_line`] assigned to display column `col`, by walking the
    /// spans and their widths.
    fn style_at_col(spans: &[Span], col: usize) -> Style {
        let mut c = 0;
        for s in spans {
            let w = s.width();
            if col < c + w {
                return s.style;
            }
            c += w;
        }
        Style::default()
    }

    #[test]
    fn render_line_slices_ascii_window() {
        // "abcdefgh", window [2, 2+3) -> "cde".
        assert_eq!(rendered("abcdefgh", 2, 3), "cde");
    }

    #[test]
    fn render_line_from_zero_is_left_aligned_prefix() {
        assert_eq!(rendered("abcdefgh", 0, 4), "abcd");
    }

    #[test]
    fn render_line_pads_short_line_to_width() {
        // The line ends before the window does: the remainder is padded with spaces
        // so a row-wide base style (the current-line tint) fills the whole width.
        assert_eq!(rendered("abc", 0, 5), "abc  ");
    }

    #[test]
    fn render_line_past_end_is_all_padding() {
        // Scrolled entirely past the content: the window is all padding spaces.
        assert_eq!(rendered("abc", 10, 5), "     ");
    }

    #[test]
    fn render_line_zero_width_is_empty() {
        assert!(render_line("abc", 0, 0, Style::default(), &[]).is_empty());
    }

    #[test]
    fn render_line_keeps_wide_char_fully_inside() {
        // "日本語" is 3 chars x 2 cells = 6 cells. Window [2, 2+2) is exactly the
        // second char "本".
        assert_eq!(rendered("日本語", 2, 2), "本");
    }

    #[test]
    fn render_line_replaces_wide_char_straddling_left_edge_with_spaces() {
        // Window starts at col 1, mid-"日" (cols 0..2). The 1 visible cell of that
        // glyph becomes a space so "本" (cols 2..4) still lands at the right place.
        assert_eq!(rendered("日本語", 1, 3), " 本");
    }

    #[test]
    fn render_line_replaces_wide_char_straddling_right_edge_with_spaces() {
        // Window [0, 3): "日" fits, then "本" straddles the right edge -> 1 space.
        assert_eq!(rendered("日本語", 0, 3), "日 ");
    }

    #[test]
    fn render_line_overlay_styles_only_its_columns() {
        use ratatui::style::Color;
        let sel = Style::new().bg(Color::Blue);
        let spans = render_line("hello", 0, 5, Style::default(), &[(1..3, sel)]);
        assert_eq!(style_at_col(&spans, 0).bg, None);
        assert_eq!(style_at_col(&spans, 1).bg, Some(Color::Blue));
        assert_eq!(style_at_col(&spans, 2).bg, Some(Color::Blue));
        assert_eq!(style_at_col(&spans, 3).bg, None);
    }

    #[test]
    fn render_line_base_style_fills_padding() {
        use ratatui::style::Color;
        // The base (current-line tint) reaches the padded cells past the content.
        let base = Style::new().bg(Color::Indexed(236));
        let spans = render_line("ab", 0, 5, base, &[]);
        assert_eq!(style_at_col(&spans, 1).bg, Some(Color::Indexed(236)));
        assert_eq!(style_at_col(&spans, 4).bg, Some(Color::Indexed(236)));
    }

    #[test]
    fn render_line_overlay_patches_over_base() {
        use ratatui::style::Color;
        // Selection over the current-line tint: the overlay bg wins on its columns,
        // the base bg holds elsewhere (the layering used on the cursor's row).
        let base = Style::new().bg(Color::Indexed(236));
        let sel = Style::new().bg(Color::Blue);
        let spans = render_line("abcd", 0, 4, base, &[(1..3, sel)]);
        assert_eq!(style_at_col(&spans, 0).bg, Some(Color::Indexed(236)));
        assert_eq!(style_at_col(&spans, 1).bg, Some(Color::Blue));
        assert_eq!(style_at_col(&spans, 3).bg, Some(Color::Indexed(236)));
    }

    #[test]
    fn selection_columns_partial_within_line() {
        // "hello", select bytes 1..4 ("ell") -> display columns 1..4.
        assert_eq!(selection_columns("hello", 0, 6, 4, 1, 4), Some(1..4));
    }

    #[test]
    fn selection_columns_cursor_is_none() {
        // A zero-width selection highlights nothing (the terminal caret shows it).
        assert_eq!(selection_columns("hello", 0, 6, 4, 2, 2), None);
    }

    #[test]
    fn selection_columns_outside_the_line_is_none() {
        // Selection entirely before this line's byte span.
        assert_eq!(selection_columns("hello", 10, 16, 4, 0, 5), None);
    }

    #[test]
    fn selection_columns_through_newline_adds_a_cell() {
        // "ab" + newline (line span [0, 3)); selecting through the break gives the
        // 2 content columns plus one cell for the consumed newline.
        assert_eq!(selection_columns("ab", 0, 3, 4, 0, 3), Some(0..3));
    }

    #[test]
    fn selection_columns_empty_line_in_selection_shows_one_cell() {
        // An empty line swept by a multi-line selection still shows a 1-cell mark.
        assert_eq!(selection_columns("", 5, 6, 4, 0, 10), Some(0..1));
    }

    #[test]
    fn selection_columns_expands_tabs() {
        // "a\tb": selecting the tab (bytes 1..2) covers columns 1..4 (the tab spans
        // to the next 4-stop), matching the painted glyphs.
        assert_eq!(selection_columns("a\tb", 0, 4, 4, 1, 2), Some(1..4));
    }

    #[test]
    fn column_walker_matches_display_column_for_sorted_targets() {
        // The walker's incremental columns equal display_column at every boundary,
        // across tabs and wide chars, for non-decreasing queries.
        let line = "a\t日bc\td";
        let mut w = ColumnWalker::new(line, 4);
        let boundaries = [0, 1, 2, 5, 6, 7, 8, line.len()];
        for &b in &boundaries {
            assert_eq!(w.column_at(b), display_column(line, b, 4), "at byte {b}");
        }
    }

    #[test]
    fn column_walker_clamps_target_past_line_end() {
        let mut w = ColumnWalker::new("hi", 4);
        assert_eq!(w.column_at(99), 2);
    }

    #[test]
    fn column_walker_non_decreasing_returns_current_column() {
        // The contract: a target behind the cursor is not a rescan; the walker only
        // advances, so it reports the column it already reached (never a lower one).
        let mut w = ColumnWalker::new("abcdef", 4);
        assert_eq!(w.column_at(4), 4);
        assert_eq!(w.column_at(2), 4); // does not walk backward
    }

    #[test]
    fn span_columns_matches_selection_columns_for_each_span() {
        // A shared walker over sorted, non-overlapping spans yields the same ranges
        // selection_columns computes span-by-span - the highlight hot path invariant.
        let line = "let\tx = 日本;";
        let line_end_excl = line.len() + 1; // one trailing newline byte
        let spans = [(0, 3), (4, 5), (8, 14)]; // "let", "x", "日本"
        let mut w = ColumnWalker::new(line, 4);
        for &(s, e) in &spans {
            assert_eq!(
                span_columns(&mut w, line.len(), 0, line_end_excl, s, e),
                selection_columns(line, 0, line_end_excl, 4, s, e),
                "span {s}..{e}"
            );
        }
    }

    #[test]
    fn span_columns_preserves_newline_cell() {
        // A span running past the content (through the terminator) still gets the
        // extra cell, matching selection_columns.
        let mut w = ColumnWalker::new("ab", 4);
        assert_eq!(span_columns(&mut w, 2, 0, 3, 0, 3), Some(0..3));
    }

    #[test]
    fn selected_grapheme_count_counts_clusters_not_bytes() {
        // "héllo": é is 2 bytes, so 6 bytes but 5 graphemes.
        let t = text_of("héllo");
        assert_eq!(
            selected_grapheme_count(&t, &[vortex_core::Selection::new(0, 6)]),
            5
        );
    }

    #[test]
    fn selected_grapheme_count_cursor_is_zero() {
        let t = text_of("hello");
        assert_eq!(
            selected_grapheme_count(&t, &[vortex_core::Selection::cursor(3)]),
            0
        );
    }

    #[test]
    fn selected_grapheme_count_spans_multiple_lines() {
        // "ab\ncd", select bytes 1..5: "b" + "c" + "d" = 3 graphemes (newline
        // excluded from the count).
        let t = text_of("ab\ncd");
        assert_eq!(
            selected_grapheme_count(&t, &[vortex_core::Selection::new(1, 5)]),
            3
        );
    }

    #[test]
    fn expand_tabs_no_tabs_is_identity() {
        assert_eq!(expand_tabs("hello", 4), "hello");
        assert_eq!(expand_tabs("日本", 4), "日本");
    }

    #[test]
    fn expand_tabs_fills_to_stop() {
        // Leading tab -> 4 spaces; tab after "ab" -> 2 spaces (to col 4).
        assert_eq!(expand_tabs("\t", 4), "    ");
        assert_eq!(expand_tabs("ab\t", 4), "ab  ");
        assert_eq!(expand_tabs("abcd\tx", 4), "abcd    x");
    }

    #[test]
    fn expand_tabs_matches_display_column() {
        // The whole point: expanded text length in cells == display_column of the
        // original at its end, so cursor and glyphs never drift.
        let line = "a\tbc\td";
        let expanded = expand_tabs(line, 4);
        assert_eq!(
            expanded.chars().count(),
            display_column(line, line.len(), 4)
        );
    }

    #[test]
    fn expand_tabs_with_wide_char_before_tab() {
        // "日" is 2 cells, so the following tab fills 2 to reach col 4.
        assert_eq!(expand_tabs("日\t", 4), "日  ");
    }

    fn text_of(s: &str) -> Text {
        use vortex_core::{Buffer, RopeBuffer};
        RopeBuffer::from(s).text()
    }

    #[test]
    fn cursor_line_col_on_first_line() {
        let (line, col, text) = cursor_line_col(&text_of("hello\nworld"), 3);
        assert_eq!((line, col), (0, 3));
        assert_eq!(text, "hello");
    }

    #[test]
    fn cursor_line_col_on_second_line() {
        // Offset 8 is 'r' in "world" (line 1 starts at byte 6).
        let (line, col, text) = cursor_line_col(&text_of("hello\nworld"), 8);
        assert_eq!((line, col), (1, 2));
        assert_eq!(text, "world");
    }

    #[test]
    fn cursor_line_col_clamps_out_of_range_head() {
        let (line, col, _) = cursor_line_col(&text_of("ab"), 999);
        assert_eq!((line, col), (0, 2));
    }

    #[test]
    fn cursor_line_col_empty_buffer() {
        let (line, col, text) = cursor_line_col(&text_of(""), 0);
        assert_eq!((line, col), (0, 0));
        assert_eq!(text, "");
    }

    /// The painted text of each visible row, for asserting on the window contents.
    fn displayed(lines: &[VisibleLine]) -> Vec<&str> {
        lines.iter().map(VisibleLine::display).collect()
    }

    #[test]
    fn visible_lines_window_from_scroll() {
        let t = text_of("l0\nl1\nl2\nl3\nl4");
        // Window of 2 rows starting at line 1.
        assert_eq!(displayed(&visible_lines(&t, 1, 2, 4)), vec!["l1", "l2"]);
    }

    #[test]
    fn visible_lines_stops_at_buffer_end() {
        let t = text_of("a\nb");
        // Height exceeds the buffer: only the two real lines, no blank padding.
        assert_eq!(displayed(&visible_lines(&t, 0, 10, 4)), vec!["a", "b"]);
    }

    #[test]
    fn visible_lines_expands_tabs() {
        let t = text_of("a\tb\nplain");
        let rows = visible_lines(&t, 0, 2, 4);
        assert_eq!(displayed(&rows), vec!["a   b", "plain"]);
        // The tab line carries an expanded copy; the tab-free line paints its raw
        // form directly (no second allocation).
        assert_eq!(rows[0].raw, "a\tb");
        assert_eq!(rows[0].expanded.as_deref(), Some("a   b"));
        assert_eq!(rows[1].expanded, None);
    }

    #[test]
    fn visible_lines_scroll_past_end_is_empty() {
        let t = text_of("only");
        assert!(visible_lines(&t, 5, 3, 4).is_empty());
    }

    #[test]
    fn display_line_count_includes_reachable_empty_lines() {
        // The empty buffer still has one line the cursor sits on.
        assert_eq!(display_line_count(&text_of("")), 1);
        assert_eq!(display_line_count(&text_of("hello")), 1);
        // A trailing newline opens a new (empty) line the cursor can reach - crop's
        // line_count() reports 1 here, but the screen shows 2.
        assert_eq!(display_line_count(&text_of("hello\n")), 2);
        assert_eq!(display_line_count(&text_of("a\nb")), 2);
        assert_eq!(display_line_count(&text_of("a\nb\n")), 3);
    }

    #[test]
    fn visible_lines_renders_empty_buffer_as_one_blank_row() {
        // Regression: an empty buffer must still paint one (blank) row so its
        // gutter number "1" shows, rather than zero rows.
        assert_eq!(displayed(&visible_lines(&text_of(""), 0, 10, 4)), vec![""]);
    }

    #[test]
    fn visible_lines_renders_trailing_empty_line() {
        // Regression: pressing Enter at end of file ("a\n") must show line 2 as a
        // blank row so it gets a gutter number, not be swallowed as a terminator.
        assert_eq!(
            displayed(&visible_lines(&text_of("a\n"), 0, 10, 4)),
            vec!["a", ""]
        );
    }

    /// The guide columns for every line of `src`, painted from the top.
    fn guides_of(src: &str, tab_width: usize) -> Vec<Vec<usize>> {
        let t = text_of(src);
        let lines = visible_lines(&t, 0, 100, tab_width);
        indent_guides(&t, &lines, 0, tab_width)
    }

    #[test]
    fn indent_width_measures_leading_whitespace_and_blanks_report_none() {
        assert_eq!(indent_width("code", 4), Some(0));
        assert_eq!(indent_width("    code", 4), Some(4));
        // A tab counts as its stop, not as one cell, so guide columns land where the
        // painted glyphs do.
        assert_eq!(indent_width("\tcode", 4), Some(4));
        assert_eq!(indent_width("\t\tcode", 4), Some(8));
        // Blank lines - empty, spaces, or a lone tab - have no indent to report: the
        // whitespace is trailing, not leading.
        assert_eq!(indent_width("", 4), None);
        assert_eq!(indent_width("   ", 4), None);
        assert_eq!(indent_width("\t", 4), None);
    }

    #[test]
    fn only_spaces_and_tabs_count_as_indentation() {
        // Narrower than `char::is_whitespace` on purpose. A NO-BREAK SPACE (common in
        // pasted text) is a character `with_indent_guides` cannot replace, so counting
        // it would name a guide column that gets recolored but never marked - a dimmed
        // character where a rule should be.
        assert_eq!(indent_width("\u{a0}\u{a0}code", 4), Some(0));
        // Same for the ideographic space, which is two cells wide and would otherwise
        // put a guide column inside a glyph.
        assert_eq!(indent_width("\u{3000}\u{3000}code", 4), Some(0));
        // A line of nothing but those is content at column 0, not a blank line.
        assert_eq!(indent_width("\u{a0}", 4), Some(0));
        // The invariant this buys: every column offered for substitution holds a space
        // once the tabs are expanded.
        let indent = indent_width("\t  code", 4).unwrap();
        let expanded = expand_tabs("\t  code", 4);
        assert!(
            expanded[..indent].chars().all(|c| c == ' '),
            "guide columns must land on spaces, got {:?}",
            &expanded[..indent]
        );
    }

    #[test]
    fn guides_sit_at_every_stop_inside_the_indent() {
        // Column 0 is included - it marks the left edge of the block the indented
        // text is inside, and dropping it would leave line 2 with no guide at all.
        let guides = guides_of("fn a() {\n    x();\n        y();\n}", 4);
        assert_eq!(
            guides[0],
            Vec::<usize>::new(),
            "top level has no block above"
        );
        assert_eq!(guides[1], vec![0]);
        assert_eq!(guides[2], vec![0, 4]);
        assert_eq!(guides[3], Vec::<usize>::new());
    }

    #[test]
    fn a_misaligned_indent_only_gets_the_stops_it_passed() {
        // 6 columns of indent with 4-wide tabs passes stop 0 and stop 4, and stop 8
        // is still ahead of it - a guide there would sit on text.
        assert_eq!(guides_of("x\n      y", 4)[1], vec![0, 4]);
    }

    #[test]
    fn a_blank_line_inside_a_block_keeps_the_guides_running_through_it() {
        // The case the feature lives or dies on: without inheritance the blank line
        // punches a hole through every guide crossing it.
        let guides = guides_of("fn a() {\n    x();\n\n    y();\n}", 4);
        assert_eq!(guides[2], vec![0], "inherited from both sides");
    }

    #[test]
    fn a_blank_line_takes_the_shallower_neighbour() {
        // Trailing a block, the blank sits between an indented statement and the
        // closing brace: the shallower side wins, so the guide stops rather than
        // running past the brace.
        let guides = guides_of("fn a() {\n    x();\n\n}", 4);
        assert_eq!(guides[2], Vec::<usize>::new());
        // Leading one, the same rule read the other way.
        let guides = guides_of("fn a() {\n\n    x();\n}", 4);
        assert_eq!(guides[1], Vec::<usize>::new());
    }

    #[test]
    fn a_blank_line_at_the_edge_of_the_file_is_inside_nothing() {
        // A missing neighbour is column 0, not "use the other side": nothing encloses
        // the top or the bottom of a file.
        let guides = guides_of("\n    x();", 4);
        assert_eq!(guides[0], Vec::<usize>::new());
        let guides = guides_of("    x();\n", 4);
        assert_eq!(guides[1], Vec::<usize>::new());
    }

    #[test]
    fn a_blank_row_inherits_across_the_window_edge() {
        // The scan crosses out of the window into the rope, or the top row of the
        // screen would lose its guides every time a scroll landed a blank line there.
        let t = text_of("fn a() {\n    x();\n\n    y();\n}");
        let lines = visible_lines(&t, 2, 2, 4);
        // The window is [blank, "    y();"]; the blank's *above* neighbour is line 1,
        // which is off screen.
        assert_eq!(indent_guides(&t, &lines, 2, 4)[0], vec![0]);
    }

    #[test]
    fn the_scan_gives_up_when_the_nearest_neighbour_is_too_far_outside_the_window() {
        // The bound is on the search that leaves the window (SPEC §10.4), so exercising
        // it needs a window sitting *inside* a run of blanks with the enclosing code
        // further than GUIDE_SCAN away - a whole-file window would answer from `own`
        // and never consult the rope at all.
        // A blank line takes the *shallower* of its two sides, so a side that gives up
        // counts as column 0 and settles it alone - which means the window has to sit
        // far enough into the run for *both* scans to run out.
        let long = 2 * GUIDE_SCAN + 4;
        let t = text_of(&format!("    x();\n{}    y();", "\n".repeat(long)));
        let middle = long / 2;
        let lines = visible_lines(&t, middle, 2, 4);
        assert_eq!(indent_guides(&t, &lines, middle, 4)[0], Vec::<usize>::new());

        // The bound is the only reason for that: the same shape with a run short enough
        // to see across inherits normally. Without this half, deleting GUIDE_SCAN
        // outright would leave the assertion above passing.
        let t = text_of(&format!("    x();\n{}    y();", "\n".repeat(4)));
        let lines = visible_lines(&t, 2, 2, 4);
        assert_eq!(indent_guides(&t, &lines, 2, 4)[0], vec![0]);
    }

    #[test]
    fn a_zero_tab_width_draws_no_guides() {
        // Guard: there are no stops to hang them on, and stepping by zero panics.
        assert_eq!(guides_of("    x", 0)[0], Vec::<usize>::new());
    }

    #[test]
    fn only_the_guides_the_window_can_show_are_kept() {
        // The clip removes work rather than marks, so nothing downstream can observe
        // it - this is the only place it is checkable at all.
        let columns: Vec<usize> = (0..1_000).step_by(4).collect();
        // A 36-column window at the left edge holds nine tab stops.
        assert_eq!(guides_in_window(&columns, 0, 36), &columns[..9]);
        // Scrolled right, both ends move: the low end is trimmed as well, not just the
        // far edge, since a guide left of the window is as unpaintable as one past it.
        assert_eq!(
            guides_in_window(&columns, 100, 36),
            &[100, 104, 108, 112, 116, 120, 124, 128, 132]
        );
        // Scrolled past every guide, nothing is left to draw.
        assert!(guides_in_window(&columns, 5_000, 36).is_empty());
        // A window landing between two stops keeps only the stop inside it.
        assert_eq!(guides_in_window(&columns, 5, 4), &[8]);
        // Degenerate windows are empty rather than a panic, and a huge offset must not
        // overflow the addition.
        assert!(guides_in_window(&columns, 0, 0).is_empty());
        assert!(guides_in_window(&columns, usize::MAX, 36).is_empty());
        assert!(guides_in_window(&[], 0, 36).is_empty());
    }

    #[test]
    fn a_guide_stands_in_for_the_space_it_replaces() {
        // One cell for one cell, so nothing after the indent moves.
        assert_eq!(with_indent_guides("        x", &[0, 4]), "│   │   x");
        // No columns is the untouched line, borrowed rather than rebuilt.
        assert!(matches!(
            with_indent_guides("        x", &[]),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn an_inherited_guide_pads_out_past_a_blank_line_s_end() {
        // A blank line has no cells for its inherited guides to replace, so the tail
        // grows to reach them - the guide marks a column the line has not reached.
        assert_eq!(with_indent_guides("", &[0, 4]), "│   │");
        assert_eq!(with_indent_guides("  ", &[0, 4]), "│   │");
    }

    #[test]
    fn the_scrollbar_tracks_ends_are_the_buffers_ends() {
        // The property that rules out "put the thumb's top at the pointer": the last
        // row has to reach the last line, which that mapping cannot do.
        assert_eq!(scroll_at_track_row(0, 10, 90), Some(0));
        assert_eq!(scroll_at_track_row(9, 10, 90), Some(90));
        // Linear between them, rounded to the nearest offset.
        assert_eq!(scroll_at_track_row(5, 10, 90), Some(50));
        assert_eq!(scroll_at_track_row(4, 10, 9), Some(4));
    }

    #[test]
    fn a_press_past_the_scrollbar_track_lands_on_its_last_row() {
        // A drag pulled below the body still means "the bottom", not an offset past
        // the end that the paint would silently clamp back.
        assert_eq!(scroll_at_track_row(99, 10, 90), Some(90));
    }

    #[test]
    fn the_thumb_lands_where_the_track_row_that_selects_it_would() {
        // The two halves of one mapping, checked against each other: paint the bar for
        // the offset `scroll_at_track_row` hands back for a row, and the thumb has to
        // cover that row. This is the invariant that lets a drag read as dragging the
        // thumb rather than throwing it, and the reason both halves live in this file.
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::StatefulWidget;

        let (track, viewport, max_scroll) = (10, 20, 90);
        let area = Rect::new(0, 0, 1, track as u16);
        for row in 0..track {
            let scroll = scroll_at_track_row(row, track, max_scroll).unwrap();
            let (bar, mut state) = scrollbar(
                scroll,
                max_scroll,
                viewport,
                Style::default(),
                Style::default(),
            );
            let mut buf = Buffer::empty(area);
            StatefulWidget::render(bar, area, &mut buf, &mut state);
            let painted: Vec<String> = (0..track)
                .map(|y| buf.cell((0, y as u16)).unwrap().symbol().to_string())
                .collect();
            assert_eq!(
                painted[row], "█",
                "row {row} selects offset {scroll}, whose thumb is at {painted:?}"
            );
        }
    }

    #[test]
    fn a_scrollbar_track_too_short_to_mean_anything_answers_nothing() {
        // A single-row track has every offset and no way to tell them apart. Answering
        // `0` would throw a reader at line 90 back to the top for touching the only
        // cell the bar has - so the press has to mean nothing instead. Also the guard
        // against dividing by a zero span.
        assert_eq!(scroll_at_track_row(0, 1, 90), None);
        assert_eq!(scroll_at_track_row(3, 0, 90), None);
    }

    #[test]
    fn gutter_width_has_minimum_then_widens_with_digits() {
        // 3-digit minimum field + 1 separator space, until the file needs more.
        assert_eq!(gutter_width(1), 4);
        assert_eq!(gutter_width(999), 4);
        assert_eq!(gutter_width(1000), 5); // 4 digits + space
        assert_eq!(gutter_width(0), 4); // defensive floor: still sizes for line "1"
    }

    #[test]
    fn digit_count_counts_base_ten_digits() {
        assert_eq!(digit_count(1), 1);
        assert_eq!(digit_count(9), 1);
        assert_eq!(digit_count(10), 2);
        assert_eq!(digit_count(99), 2);
        assert_eq!(digit_count(100), 3);
    }

    #[test]
    fn gutter_label_is_one_based_and_right_aligned() {
        // width 4 = 3-digit field + trailing space. The cursor line is irrelevant
        // to absolute numbering, so a far-away one must change nothing.
        let abs = LineNumbers::Absolute;
        assert_eq!(gutter_label(0, 500, 4, abs), "  1 ");
        assert_eq!(gutter_label(41, 500, 4, abs), " 42 ");
        assert_eq!(gutter_label(998, 500, 4, abs), "999 ");
    }

    #[test]
    fn gutter_label_relative_counts_distance_in_both_directions() {
        let rel = LineNumbers::Relative;
        // Cursor on line index 41 (displayed 42).
        assert_eq!(gutter_label(39, 41, 4, rel), "  2 "); // two above
        assert_eq!(gutter_label(40, 41, 4, rel), "  1 ");
        assert_eq!(gutter_label(42, 41, 4, rel), "  1 "); // one below reads the same
        assert_eq!(gutter_label(44, 41, 4, rel), "  3 ");
    }

    #[test]
    fn gutter_label_relative_keeps_the_absolute_number_on_the_cursor_line() {
        // The one row whose relative number would be a useless 0 shows where you
        // actually are instead.
        assert_eq!(gutter_label(41, 41, 4, LineNumbers::Relative), " 42 ");
        assert_eq!(gutter_label(0, 0, 4, LineNumbers::Relative), "  1 ");
    }

    #[test]
    fn gutter_label_relative_does_not_narrow_the_field() {
        // Width is sized from the buffer, not from the numbers relative mode
        // happens to print, so the text body never slides sideways as the cursor
        // moves. A 5-wide gutter stays 5 wide for a distance of 1.
        assert_eq!(gutter_label(1000, 999, 5, LineNumbers::Relative), "   1 ");
    }

    #[test]
    fn grapheme_column_is_one_based_and_counts_clusters() {
        assert_eq!(grapheme_column("hello", 0), 1); // start of line
        assert_eq!(grapheme_column("hello", 3), 4);
        // "日本": each char is 3 bytes; byte_col 3 is after one cluster -> col 2.
        assert_eq!(grapheme_column("日本", 3), 2);
        assert_eq!(grapheme_column("日本", 6), 3);
    }

    #[test]
    fn grapheme_column_counts_zwj_cluster_once() {
        let family = "👨‍👩‍👧";
        // The whole cluster is one column: past it is column 2, not column 8.
        assert_eq!(grapheme_column(family, family.len()), 2);
    }

    #[test]
    fn grapheme_column_clamps_out_of_range_byte_col() {
        assert_eq!(grapheme_column("hi", 99), 3);
    }

    #[test]
    fn line_count_label_pluralizes() {
        assert_eq!(line_count_label(1), "1 line ");
        assert_eq!(line_count_label(4), "4 lines ");
        // Empty buffer reads as one line, matching the single rendered row.
        assert_eq!(line_count_label(0), "1 line ");
    }

    // --- Bufferline (SPEC §7.5) ----------------------------------------------

    fn buffers(specs: &[(u64, Option<&str>, bool)]) -> Vec<BufferInfo> {
        specs
            .iter()
            .map(|&(id, path, modified)| BufferInfo {
                id: BufferId(id),
                path: path.map(std::path::PathBuf::from),
                modified,
            })
            .collect()
    }

    /// Total display width of a rendered strip - what the painter will occupy.
    fn strip_width(strip: &[Tab]) -> usize {
        strip.iter().map(|tab| tab.cells).sum()
    }

    /// The strip's labels, for asserting on text without the ids.
    fn strip_labels(strip: &[Tab]) -> Vec<&str> {
        strip.iter().map(|tab| &*tab.label).collect()
    }

    #[test]
    fn every_tab_shows_when_they_all_fit() {
        let list = buffers(&[(1, Some("a.rs"), false), (2, Some("b.rs"), false)]);
        let strip = bufferline(&list, BufferId(2), 40);
        assert_eq!(strip_labels(&strip), vec![" a.rs ", "\u{2502}", " b.rs "]);
        assert_eq!(strip[0].id(), Some(BufferId(1)));
        assert_eq!(strip[1].id(), None, "the divider selects nothing");
        assert_eq!(strip[2].id(), Some(BufferId(2)));
        assert_eq!(
            (
                strip[0].is_active(),
                strip[1].is_active(),
                strip[2].is_active()
            ),
            (false, false, true)
        );
    }

    #[test]
    fn the_active_tab_is_the_only_one_flagged() {
        let list = buffers(&[
            (1, Some("a.rs"), false),
            (2, Some("b.rs"), false),
            (3, Some("c.rs"), false),
        ]);
        let flags: Vec<bool> = bufferline(&list, BufferId(2), 40)
            .into_iter()
            .filter(|tab| tab.id().is_some())
            .map(|tab| tab.is_active())
            .collect();
        assert_eq!(flags, vec![false, true, false]);
    }

    #[test]
    fn a_modified_buffer_shows_its_marker_on_its_tab() {
        let list = buffers(&[(1, Some("a.rs"), true)]);
        assert_eq!(bufferline(&list, BufferId(1), 40)[0].label, " ● a.rs ");
    }

    #[test]
    fn an_unnamed_buffer_gets_the_placeholder_tab() {
        let list = buffers(&[(1, None, false)]);
        assert_eq!(bufferline(&list, BufferId(1), 40)[0].label, " [No Name] ");
    }

    #[test]
    fn a_lone_buffer_gets_no_separator() {
        // Nothing to divide it from, so the divider would be noise.
        let list = buffers(&[(1, Some("only.rs"), false)]);
        let strip = bufferline(&list, BufferId(1), 40);
        assert_eq!(strip.len(), 1);
        assert!(strip[0].id().is_some());
    }

    #[test]
    fn separators_sit_between_every_pair_of_tabs() {
        let list = buffers(&[
            (1, Some("a.rs"), false),
            (2, Some("b.rs"), false),
            (3, Some("c.rs"), false),
        ]);
        let strip = bufferline(&list, BufferId(1), 60);
        // tab, sep, tab, sep, tab - never two dividers running together, and never a
        // leading or trailing one.
        assert_eq!(
            strip.iter().map(|t| t.id().is_some()).collect::<Vec<_>>(),
            vec![true, false, true, false, true]
        );
    }

    #[test]
    fn separators_are_counted_when_the_strip_has_to_fit() {
        // The dividers cost a column each, so a strip that fits only when they are
        // ignored must still be windowed. Without counting them the bar overflows by
        // one cell per gap - invisible with two buffers, obvious with eight.
        let specs: Vec<(u64, Option<&str>, bool)> =
            (1..=6).map(|i| (i, Some("file.rs"), false)).collect();
        let list = buffers(&specs);
        for width in 8..60 {
            let strip = bufferline(&list, BufferId(3), width);
            assert!(
                strip_width(&strip) <= width,
                "width {width} overflowed: {:?}",
                strip_labels(&strip)
            );
        }
    }

    #[test]
    fn a_windowed_strip_never_ends_on_a_dangling_separator() {
        // A divider with no tab after it is a cell spent on nothing.
        let specs: Vec<(u64, Option<&str>, bool)> =
            (1..=8).map(|i| (i, Some("file.rs"), false)).collect();
        let list = buffers(&specs);
        for width in 6..40 {
            let strip = bufferline(&list, BufferId(4), width);
            if let Some(last) = strip.last() {
                assert_ne!(
                    last.label,
                    "│",
                    "width {width} left a dangling divider: {:?}",
                    strip_labels(&strip)
                );
            }
        }
    }

    #[test]
    fn a_narrow_bar_windows_around_the_active_tab() {
        // Eight buffers, room for two or three: the active one must be visible, which
        // is the whole reason the strip scrolls instead of truncating from the right.
        let specs: Vec<(u64, Option<&str>, bool)> =
            (1..=8).map(|i| (i, Some("file.rs"), false)).collect();
        let list = buffers(&specs);
        let strip = bufferline(&list, BufferId(6), 24);
        assert!(
            strip.iter().any(|tab| tab.is_active()),
            "the active tab must be in the window: {strip:?}"
        );
        assert!(strip_width(&strip) <= 24, "overflowed the bar: {strip:?}");
    }

    #[test]
    fn a_windowed_strip_marks_where_it_continues() {
        let specs: Vec<(u64, Option<&str>, bool)> =
            (1..=8).map(|i| (i, Some("file.rs"), false)).collect();
        let list = buffers(&specs);
        // Active in the middle: the strip continues in both directions.
        let strip = bufferline(&list, BufferId(4), 24);
        assert_eq!(strip.first().map(|t| &*t.label), Some("‹"));
        assert_eq!(strip.last().map(|t| &*t.label), Some("›"));

        // Active at the very start: nothing precedes it, so no leading marker.
        let strip = bufferline(&list, BufferId(1), 24);
        assert_ne!(strip.first().map(|t| &*t.label), Some("‹"));
        assert_eq!(strip.last().map(|t| &*t.label), Some("›"));
    }

    #[test]
    fn a_tab_wider_than_the_bar_is_truncated_not_overflowed() {
        let list = buffers(&[(1, Some("an-extremely-long-file-name.rs"), false)]);
        let strip = bufferline(&list, BufferId(1), 10);
        assert!(strip_width(&strip) <= 10, "overflowed: {strip:?}");
    }

    #[test]
    fn wide_glyph_names_are_measured_in_display_cells() {
        // CJK names occupy two cells each (SPEC §4); measuring in chars would let the
        // strip overrun the bar.
        let list = buffers(&[(1, Some("日本語.rs"), false), (2, Some("한글.rs"), false)]);
        let strip = bufferline(&list, BufferId(1), 14);
        assert!(strip_width(&strip) <= 14, "overflowed: {strip:?}");
    }

    #[test]
    fn a_zero_width_bar_or_empty_session_draws_nothing() {
        let list = buffers(&[(1, Some("a.rs"), false)]);
        assert!(bufferline(&list, BufferId(1), 0).is_empty());
        assert!(bufferline(&[], BufferId(1), 40).is_empty());
    }

    #[test]
    fn a_column_resolves_to_the_tab_it_lands_on() {
        // Clicking a tab selects that buffer, so every cell of a tab - including its
        // padding - has to resolve to it, and the boundary must not be off by one.
        let list = buffers(&[(1, Some("a.rs"), false), (2, Some("b.rs"), false)]);
        let strip = bufferline(&list, BufferId(1), 40);
        // " a.rs " is columns 0..6, the divider is column 6, " b.rs " is 7..13.
        assert_eq!(tab_at_column(&strip, 0), Some(BufferId(1)));
        assert_eq!(tab_at_column(&strip, 3), Some(BufferId(1)));
        assert_eq!(tab_at_column(&strip, 5), Some(BufferId(1)));
        assert_eq!(
            tab_at_column(&strip, 6),
            None,
            "the divider is not a target"
        );
        assert_eq!(tab_at_column(&strip, 7), Some(BufferId(2)));
        assert_eq!(tab_at_column(&strip, 12), Some(BufferId(2)));
        // Past the strip is the empty bar, which selects nothing.
        assert_eq!(tab_at_column(&strip, 13), None);
        assert_eq!(tab_at_column(&strip, 999), None);
    }

    #[test]
    fn a_column_on_an_overflow_marker_selects_nothing() {
        // The markers are chrome, not targets: clicking one must not switch to some
        // arbitrary neighbouring buffer.
        let specs: Vec<(u64, Option<&str>, bool)> =
            (1..=8).map(|i| (i, Some("file.rs"), false)).collect();
        let list = buffers(&specs);
        let strip = bufferline(&list, BufferId(4), 24);
        assert_eq!(&*strip[0].label, "‹");
        assert_eq!(tab_at_column(&strip, 0), None);
        let last = strip_width(&strip) - 1;
        assert_eq!(&*strip.last().unwrap().label, "›");
        assert_eq!(tab_at_column(&strip, last), None);
    }

    #[test]
    fn a_column_over_a_wide_glyph_tab_resolves_to_that_buffer() {
        // Cells, not characters: a CJK name occupies two columns per glyph, and a
        // click on its second cell must still land on that tab.
        let list = buffers(&[(1, Some("日本.rs"), false), (2, Some("b.rs"), false)]);
        let strip = bufferline(&list, BufferId(1), 40);
        let first_width = strip[0].cells;
        assert_eq!(tab_at_column(&strip, first_width - 1), Some(BufferId(1)));
        assert_eq!(tab_at_column(&strip, first_width), None, "the divider");
        assert_eq!(tab_at_column(&strip, first_width + 1), Some(BufferId(2)));
    }

    #[test]
    fn an_empty_strip_resolves_no_column() {
        assert_eq!(tab_at_column(&[], 0), None);
    }

    #[test]
    fn head_bar_tabs_reserves_room_for_the_line_count() {
        // The painter and the pointer handler share this, so the reservation has to
        // live in one place: a strip fitted to the full width would put every tab a
        // few cells right of where a click resolves it.
        let list = buffers(&[(1, Some("a.rs"), false), (2, Some("b.rs"), false)]);
        let full = bufferline(&list, BufferId(1), 40);
        let reserved = head_bar_tabs(&list, BufferId(1), 7, 40);
        // Both fit here, so the strips match; the difference shows when they cannot.
        assert_eq!(full, reserved);

        let specs: Vec<(u64, Option<&str>, bool)> =
            (1..=8).map(|i| (i, Some("file.rs"), false)).collect();
        let many = buffers(&specs);
        let count_cells = line_count_label(7).width();
        let strip = head_bar_tabs(&many, BufferId(1), 7, 40);
        assert!(
            strip_width(&strip) <= 40 - count_cells,
            "the tab strip must not run under the line count: {strip:?}"
        );
    }

    #[test]
    fn an_unknown_active_buffer_still_renders_the_strip() {
        // A snapshot older than a close can name a buffer no longer listed; the strip
        // falls back to the first tab rather than panicking or blanking.
        let list = buffers(&[(1, Some("a.rs"), false), (2, Some("b.rs"), false)]);
        let strip = bufferline(&list, BufferId(99), 40);
        assert_eq!(strip.iter().filter(|t| t.id().is_some()).count(), 2);
        assert!(strip[0].is_active(), "fell back to marking the first tab");
    }

    /// A [`StatusInfo`] with the nominal values, so a test names only what it is
    /// about. `..` on the literal would work too, but a helper keeps the presence
    /// rule's baseline (one cursor, nothing selected, no rested diagnostic) in one
    /// place - it is what "nominal" means for this bar.
    fn info(line: usize, col: usize) -> StatusInfo<'static> {
        StatusInfo {
            line,
            col,
            selected: 0,
            cursors: 1,
            format: FileFormat::default(),
            indent: "tabs:4",
            diagnostic: None,
            read_only: false,
        }
    }

    #[test]
    fn colliding_tab_names_grow_the_shortest_parent_that_separates_them() {
        // Two tabs both reading `layout.rs` stop telling you which one you are in,
        // which is the one question the head bar exists to answer (SPEC §7.5, M10).
        let list = buffers(&[
            (1, Some("crates/tui/src/layout.rs"), false),
            (2, Some("crates/core/src/layout.rs"), false),
            (3, Some("README.md"), false),
        ]);
        let strip = bufferline(&list, BufferId(1), 120);
        let labels: Vec<&str> = strip.iter().map(|t| t.label.as_ref()).collect();
        let joined = labels.join("");
        assert!(joined.contains("tui/src/layout.rs"), "{labels:?}");
        assert!(joined.contains("core/src/layout.rs"), "{labels:?}");
        // The *shortest* parent that separates them - `src/layout.rs` would still
        // collide, and the full path is longer than it needs to be.
        assert!(
            !joined.contains("crates/tui"),
            "grew further than it had to"
        );
        // And the tab that never collided keeps its bare name, so disambiguating one
        // pair does not lengthen the whole strip.
        assert!(joined.contains(" README.md "), "{labels:?}");
    }

    #[test]
    fn a_name_that_does_not_collide_stays_a_bare_file_name() {
        let list = buffers(&[
            (1, Some("src/main.rs"), false),
            (2, Some("docs/SPEC.md"), false),
        ]);
        let labels: Vec<String> = bufferline(&list, BufferId(1), 120)
            .iter()
            .map(|t| t.label.to_string())
            .collect();
        assert!(labels.iter().any(|l| l == " main.rs "), "{labels:?}");
        assert!(labels.iter().any(|l| l == " SPEC.md "), "{labels:?}");
    }

    #[test]
    fn identical_paths_stop_growing_instead_of_looping() {
        // The same file open twice cannot be separated by any amount of path. The
        // walk has to notice it has run out rather than spin.
        let list = buffers(&[(1, Some("a/b/c.rs"), false), (2, Some("a/b/c.rs"), false)]);
        let labels: Vec<String> = bufferline(&list, BufferId(1), 120)
            .iter()
            .map(|t| t.label.to_string())
            .collect();
        assert_eq!(labels.iter().filter(|l| l.contains("c.rs")).count(), 2);
        // Unnamed buffers are the same case and must not drive the walk either.
        let unnamed = buffers(&[(1, None, false), (2, None, false)]);
        let labels: Vec<String> = bufferline(&unnamed, BufferId(1), 120)
            .iter()
            .map(|t| t.label.to_string())
            .collect();
        assert_eq!(labels.iter().filter(|l| l.contains(NO_NAME)).count(), 2);
    }

    #[test]
    fn a_disambiguated_tab_still_carries_its_modified_marker() {
        // The marker is applied after the name is chosen, so growing a path cannot
        // drop it - and the collision test compares undecorated names.
        let list = buffers(&[
            (1, Some("tui/layout.rs"), true),
            (2, Some("core/layout.rs"), false),
        ]);
        let labels: Vec<String> = bufferline(&list, BufferId(1), 120)
            .iter()
            .map(|t| t.label.to_string())
            .collect();
        assert!(
            labels
                .iter()
                .any(|l| l.contains(MODIFIED_MARKER) && l.contains("tui/layout.rs")),
            "{labels:?}"
        );
    }

    #[test]
    fn status_bar_composes_position_and_format() {
        let (left, right) = status_bar(info(2, 5));
        assert_eq!(left, " Ln 2, Col 5");
        // No byte size and no version: the filesystem answers the first and the
        // second was instrumentation (SPEC §7.5, M10). What is left is what the
        // screen cannot otherwise tell you.
        assert_eq!(right, "UTF-8 · LF · tabs:4 ");
    }

    #[test]
    fn status_bar_appends_selection_count_when_active() {
        // A held selection surfaces its size next to the position; an empty one
        // (count 0) leaves the position untouched.
        let (left, _) = status_bar(StatusInfo {
            selected: 12,
            ..info(2, 5)
        });
        assert_eq!(left, " Ln 2, Col 5  (12 selected)");
    }

    #[test]
    fn status_bar_counts_cursors_only_above_one() {
        // The presence rule: the position readout describes the *primary* as though
        // cursors were singular, so the count is worth a cell exactly when it
        // contradicts that. A permanent `1 cursor` would teach the eye to skip it.
        let (left, _) = status_bar(info(2, 5));
        assert!(!left.contains("cursor"), "{left:?}");
        let (left, _) = status_bar(StatusInfo {
            cursors: 3,
            ..info(2, 5)
        });
        assert_eq!(left, " Ln 2, Col 5  3 cursors");
    }

    #[test]
    fn a_rested_caret_diagnostic_takes_the_whole_right_segment() {
        // It replaces the format readout rather than crowding in beside it: at 80
        // columns there is no room for both, and the message is the more urgent.
        let (_, right) = status_bar(StatusInfo {
            diagnostic: Some("cannot find value `x` in this scope"),
            ..info(2, 5)
        });
        assert_eq!(right, "cannot find value `x` in this scope ");
        // And nothing in it is clickable, because the format words are not painted.
        let with_diagnostic = StatusInfo {
            diagnostic: Some("mismatched types"),
            ..info(1, 1)
        };
        let (left, right) = status_bar(with_diagnostic);
        let width = 60;
        let start = width - right.width();
        assert_eq!(status_target(&left, with_diagnostic, width, start), None);
    }

    #[test]
    fn status_bar_marks_a_read_only_buffer_where_truncation_cannot_reach_it() {
        let (left, _) = status_bar(StatusInfo {
            read_only: true,
            ..info(2, 5)
        });
        assert_eq!(left, " [read-only]  Ln 2, Col 5");
        let (left, _) = status_bar(StatusInfo {
            selected: 3,
            read_only: true,
            ..info(2, 5)
        });
        assert_eq!(left, " [read-only]  Ln 2, Col 5  (3 selected)");
        // A 20-cell bar has room for neither the format nor the whole position -
        // the marker is at the front, so it is what is left.
        assert!(fit_bar(&left, "UTF-8 · LF · tabs:4 ", 20).contains("[read-only]"));
    }

    #[test]
    fn status_bar_reports_the_files_own_encoding_and_terminator() {
        // The readout is the only place a preserved CRLF or BOM is visible: the
        // buffer itself is always UTF-8 with LF terminators (SPEC §10.1).
        let mut format = FileFormat::default();
        format.eol = vortex_core::LineEnding::Crlf;
        format.bom = true;
        let (_, right) = status_bar(StatusInfo {
            format,
            ..info(1, 1)
        });
        assert_eq!(right, "UTF-8 BOM · CRLF · tabs:4 ");
    }

    #[test]
    fn human_size_scales_at_each_1024_mark() {
        // Whole bytes below 1 KB, then KB/MB/GB with one decimal at each boundary.
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(1023), "1023B");
        assert_eq!(human_size(1024), "1.0KB");
        assert_eq!(human_size(1536), "1.5KB");
        assert_eq!(human_size(1024 * 1024 - 1), "1024.0KB"); // just under 1 MB
        assert_eq!(human_size(1024 * 1024), "1.0MB");
        assert_eq!(human_size(3 * 1024 * 1024 + 512 * 1024), "3.5MB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0GB");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024), "5.0GB"); // caps at GB
    }

    #[test]
    fn the_status_bars_encoding_and_terminator_are_clickable() {
        // The bar shows what the detector guessed; clicking the guess is how it gets
        // corrected, so the words have to map back to a target (SPEC §7.5, §10.1).
        let nominal = info(1, 1);
        let (left, right) = status_bar(nominal);
        assert_eq!(right, "UTF-8 · LF · tabs:4 ");
        let width = 60;
        let start = width - right.width();
        // "UTF-8" occupies the first five columns of the segment, "LF" the two after
        // the separator.
        let target = |column| status_target(&left, nominal, width, column);
        assert_eq!(target(start), Some(StatusTarget::Encoding));
        assert_eq!(target(start + 4), Some(StatusTarget::Encoding));
        assert_eq!(target(start + 5), None, "the separator is not a target");
        assert_eq!(target(start + 8), Some(StatusTarget::LineEnding));
        assert_eq!(target(start + 9), Some(StatusTarget::LineEnding));
        assert_eq!(target(start + 10), None);
        // The size and version are readouts, not controls.
        assert_eq!(target(width - 2), None);
        // ...and so is everything left of the segment.
        assert_eq!(target(0), None);
        assert_eq!(target(start - 1), None);
    }

    #[test]
    fn a_bom_widens_the_encoding_target_to_match_what_is_painted() {
        // The BOM marker is part of the encoding word, so it must be part of the
        // word's hit region too - otherwise its last four columns do nothing.
        let mut format = FileFormat::default();
        format.bom = true;
        let bommed = StatusInfo {
            format,
            ..info(1, 1)
        };
        let (left, right) = status_bar(bommed);
        assert!(right.starts_with("UTF-8 BOM ·"));
        let width = 60;
        let start = width - right.width();
        assert_eq!(
            status_target(&left, bommed, width, start + 8),
            Some(StatusTarget::Encoding),
            "the M of BOM"
        );
        assert_eq!(
            status_target(&left, bommed, width, start + 12),
            Some(StatusTarget::LineEnding)
        );
    }

    #[test]
    fn nothing_on_a_bar_too_narrow_to_show_the_segment_is_clickable() {
        // `fit_bar` drops the right segment when it will not fit; a hit test that
        // still reported targets would open a picker from a blank cell.
        let nominal = info(1, 1);
        let (left, right) = status_bar(nominal);
        let width = left.width() + right.width(); // one column short of a gap
        assert!(!fit_bar(&left, &right, width).contains("UTF-8"));
        for column in 0..width {
            assert_eq!(
                status_target(&left, nominal, width, column),
                None,
                "column {column} on a bar with no segment"
            );
        }
    }

    #[test]
    fn a_wide_encoding_name_moves_the_terminator_target_with_it() {
        // The spans are computed from the words themselves, so a longer encoding
        // name shifts the line-ending target rather than leaving it where UTF-8 put
        // it - the bug a hardcoded offset would have.
        let format = FileFormat::default().with_encoding("windows-1252").unwrap();
        let wide = StatusInfo {
            format,
            ..info(1, 1)
        };
        let (left, right) = status_bar(wide);
        let width = 60;
        let start = width - right.width();
        assert_eq!(
            status_target(&left, wide, width, start + 11),
            Some(StatusTarget::Encoding),
            "windows-1252 is twelve columns"
        );
        assert_eq!(
            status_target(&left, wide, width, start + 15),
            Some(StatusTarget::LineEnding)
        );
    }

    #[test]
    fn buffer_display_name_uses_file_name_not_full_path() {
        assert_eq!(
            buffer_display_name(Some(Path::new("/home/user/src/main.rs")), false),
            "main.rs"
        );
    }

    #[test]
    fn buffer_display_name_unnamed_buffer_is_placeholder() {
        assert_eq!(buffer_display_name(None, false), NO_NAME);
    }

    #[test]
    fn buffer_display_name_marks_modified_with_dot() {
        assert_eq!(
            buffer_display_name(Some(Path::new("a.txt")), true),
            "● a.txt"
        );
        assert_eq!(buffer_display_name(None, true), "● [No Name]");
    }

    #[test]
    fn buffer_display_name_falls_back_when_no_file_name_component() {
        // A path ending in "/" or ".." has no file_name; use the lossy full form
        // rather than the unnamed placeholder.
        assert_eq!(buffer_display_name(Some(Path::new("..")), false), "..");
    }

    #[test]
    fn fit_bar_pushes_segments_to_each_edge() {
        // width 20: "ab" (2) + gap + "cd" (2) -> gap of 16.
        let bar = fit_bar("ab", "cd", 20);
        assert_eq!(bar, "ab".to_string() + &" ".repeat(16) + "cd");
        assert_eq!(bar.width(), 20);
    }

    #[test]
    fn fit_bar_exact_fit_keeps_single_space() {
        // "ab" + 1 space + "cd" = 5 cells exactly.
        assert_eq!(fit_bar("ab", "cd", 5), "ab cd");
    }

    #[test]
    fn fit_bar_drops_right_and_pads_left_when_tight() {
        // width 4 can't hold "ab" + space + "cd" (needs 5): keep left, pad.
        assert_eq!(fit_bar("ab", "cd", 4), "ab  ");
    }

    #[test]
    fn fit_bar_truncates_left_when_wider_than_bar() {
        // Left alone exceeds width: truncate it, drop right, fill exactly.
        assert_eq!(fit_bar("abcdef", "xy", 4), "abcd");
        assert_eq!(fit_bar("abcdef", "xy", 4).width(), 4);
    }

    #[test]
    fn fit_bar_zero_width_is_empty() {
        assert_eq!(fit_bar("ab", "cd", 0), "");
    }

    #[test]
    fn fit_bar_never_splits_a_wide_cluster() {
        // "日" is 2 cells. width 3 fits one (2 cells) then can't fit the second;
        // the padded result is 3 cells with the cluster intact.
        let bar = fit_bar("日本", "", 3);
        assert_eq!(bar, "日 ");
        assert_eq!(bar.width(), 3);
    }

    /// A decoration set holding `ranges` as scopes, as the syntax producer publishes
    /// them (M8).
    fn scopes_of(ranges: &[(usize, usize)]) -> vortex_core::DecorationSet {
        use vortex_core::{Decoration, DecorationSet, DecorationSource};
        let mut set = DecorationSet::new();
        set.replace(
            DecorationSource::Scope,
            ranges
                .iter()
                .map(|&(start, end)| Decoration::Scope { range: start..end })
                .collect(),
        );
        set
    }

    #[test]
    fn row_at_splits_the_screen_the_way_paint_does() {
        // Head bar, then the pinned rows, then the text - the split every hit test
        // now asks about instead of re-deriving.
        assert_eq!(row_at(0, 0), Row::Head);
        assert_eq!(row_at(1, 0), Row::Text(0));
        assert_eq!(row_at(9, 0), Row::Text(8));

        assert_eq!(row_at(0, 2), Row::Head);
        assert_eq!(row_at(1, 2), Row::Header(0));
        assert_eq!(row_at(2, 2), Row::Header(1));
        // The text is indexed from the top of the *text* area, not of the body: the
        // header's rows are chrome, and a caller mapping to a buffer line adds its
        // own scroll offset to this.
        assert_eq!(row_at(3, 2), Row::Text(0));
    }

    #[test]
    fn row_at_answers_a_row_no_terminal_should_report() {
        // It arrives from outside, so the answer is an answer rather than an
        // overflow (CLAUDE.md: no panics on input paths).
        assert_eq!(row_at(u16::MAX, 0), Row::Text(u16::MAX as usize - 1));
        // A header taller than the screen leaves no text row at all, rather than
        // wrapping into one.
        assert_eq!(row_at(5, usize::MAX), Row::Header(4));
    }

    #[test]
    fn sticky_budget_is_capped_by_the_body_as_well_as_the_constant() {
        // A tall body gets the constant; a short one gets a third of itself, so the
        // header can never take the screen the file is supposed to be on.
        assert_eq!(sticky_budget(60), STICKY_CONTEXT_MAX);
        assert_eq!(sticky_budget(12), 4);
        assert_eq!(sticky_budget(9), 3);
        assert_eq!(sticky_budget(5), 1);
        assert_eq!(sticky_budget(2), 0);
        assert_eq!(sticky_budget(0), 0);
    }

    #[test]
    fn sticky_lines_pins_the_first_line_of_each_enclosing_scope() {
        // Lines: 0 `mod m {`, 1 `  fn a() {`, 2 `    x;`, 3 `  }`, 4 `}`.
        let src = "mod m {\n  fn a() {\n    x;\n  }\n}\n";
        let t = text_of(src);
        let set = scopes_of(&[
            (0, src.len()),
            (src.find("fn a").unwrap(), src.find("\n}").unwrap()),
        ]);
        // Viewing from line 2, both scopes enclose it, outermost first.
        assert_eq!(sticky_lines(&t, &set, 2, 4), vec![0, 1]);
    }

    #[test]
    fn a_scope_whose_own_first_line_is_on_screen_is_not_pinned() {
        // Pinning it would print that line twice, once in the header and once as the
        // top text row.
        let src = "mod m {\n  fn a() {\n    x;\n  }\n}\n";
        let t = text_of(src);
        let set = scopes_of(&[(0, src.len())]);
        assert!(sticky_lines(&t, &set, 0, 4).is_empty());
        assert_eq!(sticky_lines(&t, &set, 1, 4), vec![0]);
    }

    #[test]
    fn the_deepest_scopes_win_when_the_chain_is_over_budget() {
        // The row a reader needs is the function they are in, not the module three
        // levels out, so the budget is spent from the inside.
        let src = "a\nb\nc\nd\ne\nf\n";
        let t = text_of(src);
        let set = scopes_of(&[(0, 11), (2, 11), (4, 11), (6, 11)]);
        assert_eq!(sticky_lines(&t, &set, 5, 4), vec![0, 1, 2, 3]);
        assert_eq!(sticky_lines(&t, &set, 5, 2), vec![2, 3]);
        assert_eq!(sticky_lines(&t, &set, 5, 1), vec![3]);
        assert!(sticky_lines(&t, &set, 5, 0).is_empty());
    }

    #[test]
    fn two_scopes_opening_on_one_line_pin_one_row() {
        // `impl Foo { fn bar() {` written flat: the header lists *lines*, and a line
        // pinned twice reads as a duplicate rather than as depth.
        let src = "impl Foo { fn bar() {\n    x;\n} }\n";
        let t = text_of(src);
        let set = scopes_of(&[(0, src.len()), (0, src.len() - 1)]);
        assert_eq!(sticky_lines(&t, &set, 1, 4), vec![0]);
    }

    #[test]
    fn sticky_lines_of_a_line_past_the_end_is_empty() {
        // The offset is clamped elsewhere, but a viewport that outran the buffer must
        // answer "nothing" rather than panic on the lookup (CLAUDE.md: no panics).
        let t = text_of("a\nb\n");
        let set = scopes_of(&[(0, 4)]);
        assert!(sticky_lines(&t, &set, 99, 4).is_empty());
    }

    #[test]
    fn nothing_is_pinned_without_scopes() {
        // The overwhelming default: a buffer with no grammar, or a language shipping
        // no context query, costs the frame one empty lookup.
        let t = text_of("a\nb\nc\n");
        assert_eq!(
            sticky_lines(&t, &vortex_core::DecorationSet::new(), 2, 4),
            Vec::<usize>::new()
        );
    }
}
