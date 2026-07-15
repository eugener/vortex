//! Cursor state as a `SelectionSet`, never a single cursor (SPEC §2.2).
//!
//! A [`Selection`] is a range with a fixed `anchor` and a moving `head` (the
//! caret); a plain cursor is a zero-width selection (`anchor == head`). The
//! editor always holds a [`SelectionSet`], and every motion/edit maps over the
//! set - so multi-cursor and block selection are the default model, not a later
//! retrofit. M1 drives a single selection, but the set machinery (sort + merge,
//! primary tracking) is built and tested from commit one because bolting it on
//! afterwards is one of the most painful editor refactors (§2.2).
//!
//! Positions here are **byte offsets** (SPEC §4 canonical space). M1 stores raw
//! offsets; M3 swaps them for anchors that survive concurrent edits (§2.1) - the
//! motion API is shaped so that is a representation change, not a call-site one.
//!
//! Cursor motion is **by grapheme cluster** (§4), computed against a single
//! line's text so cost is bounded by line length, never the file (§10.4).

use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;

use crate::buffer::Text;

/// One selection: a range from `anchor` (fixed) to `head` (the moving caret),
/// both byte offsets. `anchor == head` is a plain cursor. Either end may be the
/// larger; the *direction* (whether head is before or after anchor) is preserved
/// by motions so extend-then-shrink behaves naturally.
///
/// `goal_column` caches the grapheme column vertical motion aims for, so moving
/// down through a short line and back returns to the original column. It is
/// deliberately excluded from equality (see [`PartialEq`]): two selections at the
/// same span are equal regardless of a transient vertical-motion goal.
#[derive(Debug, Clone, Copy)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
    goal_column: Option<usize>,
}

impl Selection {
    /// A zero-width cursor at `offset`.
    pub fn cursor(offset: usize) -> Self {
        Self {
            anchor: offset,
            head: offset,
            goal_column: None,
        }
    }

    /// A selection from `anchor` to `head`.
    pub fn new(anchor: usize, head: usize) -> Self {
        Self {
            anchor,
            head,
            goal_column: None,
        }
    }

    /// The lower bound of the covered range.
    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    /// The upper bound of the covered range.
    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    /// True if this is a plain cursor (zero-width).
    pub fn is_cursor(&self) -> bool {
        self.anchor == self.head
    }
}

/// Equality ignores `goal_column`: it is a motion cache, not identity. Two
/// selections covering the same anchor/head are the same selection.
impl PartialEq for Selection {
    fn eq(&self, other: &Self) -> bool {
        self.anchor == other.anchor && self.head == other.head
    }
}

impl Eq for Selection {}

/// A motion's intent (SPEC §1: intent, not keystrokes). Direction/granularity are
/// named, not encoded as key events; the frontend maps keys to these. Serializable
/// because it rides inside `Action` (SPEC §8.1 journal / remote wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Motion {
    /// Previous grapheme cluster (wraps to end of the previous line at col 0).
    Left,
    /// Next grapheme cluster (wraps to start of the next line at line end).
    Right,
    /// Same grapheme column on the previous line (uses the goal column).
    Up,
    /// Same grapheme column on the next line (uses the goal column).
    Down,
    /// First column of the current line.
    LineStart,
    /// Last column of the current line (before its terminator).
    LineEnd,
    /// Up by `n` lines (one screen page), keeping the goal column. The frontend
    /// supplies `n` because page size is the viewport height, which only it knows
    /// (SPEC §5, §12.2 "minimal view intent"); the core does the buffer math.
    PageUp(usize),
    /// Down by `n` lines (one screen page), keeping the goal column.
    PageDown(usize),
    /// Start of the buffer.
    BufferStart,
    /// End of the buffer.
    BufferEnd,
}

/// The editor's cursor state: a non-empty, sorted, disjoint set of selections
/// with one designated primary (SPEC §2.2). The primary drives viewport-follow
/// and prompts. Invariant, upheld after every mutation: `selections` is sorted by
/// [`Selection::start`] and no two overlap or touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionSet {
    selections: Vec<Selection>,
    primary: usize,
}

impl SelectionSet {
    /// A set with a single selection (the M1 common case).
    pub fn single(selection: Selection) -> Self {
        Self {
            selections: vec![selection],
            primary: 0,
        }
    }

    /// A single cursor at the buffer start.
    pub fn at_origin() -> Self {
        Self::single(Selection::cursor(0))
    }

    /// Build a set from cursors already sorted ascending by offset and known to
    /// be non-overlapping (the post-edit case: each edit leaves one caret and the
    /// carets cannot collide once edit offsets are shifted). Coincident cursors
    /// are still merged defensively so the disjoint invariant always holds.
    /// Primary is the first selection. An empty input falls back to the origin
    /// cursor, since the set is never empty (SPEC §2.2).
    pub(crate) fn from_sorted_cursors(cursors: Vec<Selection>) -> Self {
        if cursors.is_empty() {
            return Self::at_origin();
        }
        let mut set = Self {
            selections: cursors,
            primary: 0,
        };
        // Reuse the merge half of normalize: primary_head = first cursor's head.
        let head = set.selections[0].head;
        set.normalize(head);
        set
    }

    /// The selections, in sorted order.
    pub fn all(&self) -> &[Selection] {
        &self.selections
    }

    /// The primary selection - always present (the set is never empty).
    pub fn primary(&self) -> &Selection {
        &self.selections[self.primary]
    }

    /// Index of the primary within [`Self::all`]. Carried into the snapshot so the
    /// frontend follows the *primary* caret, not a positional guess (SPEC §2.2,
    /// §5) - which matters once M3 multi-cursor makes the primary != index 0.
    pub fn primary_index(&self) -> usize {
        self.primary
    }

    /// Number of selections.
    pub fn len(&self) -> usize {
        self.selections.len()
    }

    /// Always false: the set is never empty. Present for lint-clean `len` pairing.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Apply `motion` to every selection, then re-establish the sorted/disjoint
    /// invariant. `extend` keeps each anchor fixed (growing the selection);
    /// otherwise the selection collapses to a cursor at the new head.
    ///
    /// One call is one logical action over the whole set - the basis for "one
    /// Action is one undo unit even across N cursors" (SPEC §2.4), enforced when
    /// undo lands in M3.
    pub fn move_all(&mut self, text: &Text, motion: Motion, extend: bool) {
        // Track the primary's head so we can keep the primary designation on the
        // selection that ends up owning that position after a merge.
        let primary_head = self.selections[self.primary].head;
        for sel in &mut self.selections {
            *sel = move_selection(text, *sel, motion, extend);
        }
        self.normalize(primary_head);
    }

    /// Sort by start and merge overlapping/touching selections, then point
    /// `primary` at whichever surviving selection covers `primary_head`.
    fn normalize(&mut self, primary_head: usize) {
        self.selections.sort_by_key(Selection::start);

        let mut merged: Vec<Selection> = Vec::with_capacity(self.selections.len());
        for sel in self.selections.drain(..) {
            match merged.last_mut() {
                // Sorted by start, so `sel.start() >= prev.start()`. Merge when
                // the ranges overlap or touch (`sel.start() <= prev.end()`); this
                // also collapses coincident cursors. Touching-merges keep the set
                // strictly disjoint, which the invariant depends on.
                Some(prev) if sel.start() <= prev.end() => {
                    let start = prev.start();
                    let end = prev.end().max(sel.end());
                    // Forward-oriented merged selection. Direction preservation
                    // across merges is a refinement (only reachable with >1
                    // selection, i.e. post-M1 multi-cursor); noted, not built.
                    *prev = Selection::new(start, end);
                }
                _ => merged.push(sel),
            }
        }

        // Primary = the selection covering the old primary head (its span
        // includes that offset). Falls back to 0 if somehow not found.
        self.primary = merged
            .iter()
            .position(|s| s.start() <= primary_head && primary_head <= s.end())
            .unwrap_or(0);
        self.selections = merged;
    }
}

/// Apply a single motion to a single selection against `text`. Pure: returns the
/// new selection, mutating nothing. Grapheme work is bounded to the affected
/// line(s) (SPEC §4, §10.4).
fn move_selection(text: &Text, sel: Selection, motion: Motion, extend: bool) -> Selection {
    let (new_head, goal) = match motion {
        Motion::Left => (grapheme_before(text, sel.head), None),
        Motion::Right => (grapheme_after(text, sel.head), None),
        Motion::LineStart => (line_start(text, sel.head), None),
        Motion::LineEnd => (line_end(text, sel.head), None),
        Motion::BufferStart => (0, None),
        Motion::BufferEnd => (text.byte_len(), None),
        // Vertical motions share one path parameterized by a signed line delta
        // (single-step is ±1, a page is ±n lines). The goal column is established
        // from the current head if none is carried, so it survives across them.
        Motion::Up => vstep(text, sel, -1),
        Motion::Down => vstep(text, sel, 1),
        Motion::PageUp(n) => vstep(text, sel, -(n as isize)),
        Motion::PageDown(n) => vstep(text, sel, n as isize),
    };

    let anchor = if extend { sel.anchor } else { new_head };
    Selection {
        anchor,
        head: new_head,
        // `goal` is `Some` only for the vertical arm and `None` for every other,
        // so carrying it straight through keeps the goal column across
        // consecutive vertical motions and clears it on any horizontal/absolute
        // one - no separate direction check needed.
        goal_column: goal,
    }
}

/// (line index, byte column within the line) for a byte offset.
fn line_col(text: &Text, offset: usize) -> (usize, usize) {
    let line = text.line_of_byte(offset);
    let start = text.byte_of_line(line).unwrap_or(0);
    (line, offset.saturating_sub(start))
}

/// Byte offset at the end of `line_index`'s content, before its terminator
/// (line start + the line's byte length). The one place this "start + content
/// length" idiom lives.
fn line_content_end(text: &Text, line_index: usize) -> usize {
    let start = text.byte_of_line(line_index).unwrap_or(0);
    // `line_len` reads the slice's byte length without materializing the line.
    let len = text.line_len(line_index).unwrap_or(0);
    start + len
}

/// Byte offset one grapheme cluster before `offset`. At column 0, crosses to the
/// end of the previous line's content (so leftward motion and backspace both step
/// over the line break); at buffer start, stays put. Shared by `Motion::Left` and
/// `Action::DeleteBackward`.
pub(crate) fn grapheme_before(text: &Text, offset: usize) -> usize {
    let (line, col) = line_col(text, offset);
    if col == 0 {
        if line == 0 {
            return 0;
        }
        // End of previous line's content.
        return line_content_end(text, line - 1);
    }
    let line_text = text.line(line).unwrap_or_default();
    // `col` can exceed the line's content length when the caret sits inside a
    // multi-byte terminator (a "\r\n" split by an edit/paste), so clamp before
    // slicing - mirrors the guards in `grapheme_after`/`grapheme_column` and keeps
    // this off the panic path (SPEC §8).
    let end = col.min(line_text.len());
    // The previous grapheme boundary before `end` within this line.
    let back = line_text[..end]
        .graphemes(true)
        .next_back()
        .map(|g| g.len())
        .unwrap_or(0);
    offset - back
}

/// Byte offset one grapheme cluster after `offset`. At end of a line's content,
/// crosses to the start of the next line; at buffer end, stays put. Shared by
/// `Motion::Right` and `Action::DeleteForward`.
pub(crate) fn grapheme_after(text: &Text, offset: usize) -> usize {
    let (line, col) = line_col(text, offset);
    let line_text = text.line(line).unwrap_or_default();
    if col >= line_text.len() {
        // At (or past) the line's content end: step to the next line's start if
        // there is one, else clamp to the buffer end.
        return text
            .byte_of_line(line + 1)
            .unwrap_or_else(|| text.byte_len());
    }
    let fwd = line_text[col..]
        .graphemes(true)
        .next()
        .map(|g| g.len())
        .unwrap_or(0);
    offset + fwd
}

/// First-column byte offset of the line containing `offset`.
fn line_start(text: &Text, offset: usize) -> usize {
    let line = text.line_of_byte(offset);
    text.byte_of_line(line).unwrap_or(0)
}

/// Byte offset at the end of the content of the line containing `offset`.
fn line_end(text: &Text, offset: usize) -> usize {
    line_content_end(text, text.line_of_byte(offset))
}

/// Grapheme column (count of grapheme clusters from line start) of `offset`.
fn grapheme_column(text: &Text, offset: usize) -> usize {
    let (line, col) = line_col(text, offset);
    let line_text = text.line(line).unwrap_or_default();
    line_text[..col.min(line_text.len())]
        .graphemes(true)
        .count()
}

/// The `(new_head, goal)` for a vertical motion of `delta` lines: establish the
/// goal column from the current head if the selection is not already carrying one,
/// move, and return the goal so it persists across consecutive vertical motions.
/// Shared by Up/Down (±1) and PageUp/PageDown (±n).
fn vstep(text: &Text, sel: Selection, delta: isize) -> (usize, Option<usize>) {
    let goal = sel
        .goal_column
        .unwrap_or_else(|| grapheme_column(text, sel.head));
    (vertical(text, sel.head, delta, goal), Some(goal))
}

/// Move `delta` lines (negative = up) to `goal` grapheme column. The target line
/// is clamped to the buffer's line range, so a page motion past the top/bottom
/// lands on the first/last line rather than overshooting; a single step off the
/// edge stays put (clamp keeps it on the same line). Column is clamped to the
/// target line's content length.
fn vertical(text: &Text, offset: usize, delta: isize, goal: usize) -> usize {
    let line_count = text.line_count();
    if line_count == 0 {
        return offset; // empty buffer: nowhere to go
    }
    let (line, _) = line_col(text, offset);
    // The last navigable line, which for a newline-terminated buffer is the virtual
    // empty line *after* the final terminator (index `line_count`, reachable by
    // Right at end-of-file) - one past `line_count - 1`. Deriving the ceiling from
    // the trailing byte's line index keeps it in step with `line_col`, so a Down on
    // that trailing line stays put instead of collapsing to a line above it.
    let max_line = text.line_of_byte(text.byte_len());
    // Saturating signed add, then clamp into `0..=max_line`.
    let target_line = (line as isize + delta).clamp(0, max_line as isize) as usize;
    let start = text.byte_of_line(target_line).unwrap_or(0);
    let line_text = text.line(target_line).unwrap_or_default();
    // Byte offset of the goal-th grapheme, clamped to the line's content end.
    let col = line_text
        .grapheme_indices(true)
        .nth(goal)
        .map(|(i, _)| i)
        .unwrap_or(line_text.len());
    start + col
}

#[cfg(test)]
#[path = "selection_tests.rs"]
mod tests;
