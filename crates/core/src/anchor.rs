//! Anchors: positions that survive edits (SPEC §2.1).
//!
//! An [`Anchor`] is a byte offset plus a [`Bias`] - the correctness lynchpin the
//! SPEC calls out. Insert text before an anchor and it moves with the text; delete
//! the text under it and it collapses to the deletion boundary. This is not a
//! collaboration feature: the moment anything holds a position across an edit it did
//! not itself make - a diagnostic ("error at byte 1234"), a mark, a fold, a second
//! cursor's caret while another cursor edits - a raw byte offset points at the wrong
//! place and an anchor does not.
//!
//! **This module is the mechanism, not a storage type.** Selections stay offset-based
//! (a [`Selection`](crate::selection) resolves to concrete bytes for the frontend);
//! the anchor layer is how those offsets *survive an edit*. [`Selection`] endpoints
//! carry no stored bias - bias is applied at transform time by the caller (a caret
//! is [`Bias::After`] so typing pushes it right; a selection's start is `Before` and
//! its end `After` so typing at either boundary grows it, SPEC §2.1). The M2
//! decoration channel (`decoration.rs`) reuses exactly this
//! [`Anchor::transform_through`] so LSP diagnostics ride over concurrent edits -
//! a squiggle's span is `After`-biased at its start and `Before`-biased at its end,
//! the mirror of a selection, so typing beside it shifts it instead of growing it.
//!
//! **Implementation baseline (SPEC §2.1):** anchors are maintained by transforming
//! them through each edit (an offset shift). The API is shaped so a future CRDT
//! backend (stable per-anchor IDs) is a drop-in behind the same `transform` calls,
//! without touching call sites.

/// Which side an anchor sticks to when an insertion lands *exactly at* its offset
/// (SPEC §2.1). `Before` keeps the anchor put (inserted text ends up to its right);
/// `After` moves it past the inserted text (the text ends up to its left). At a
/// non-boundary position bias is irrelevant - both sides agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bias {
    /// Stick to the text before this position: an insertion here does not move the
    /// anchor. A decoration span's *end* uses this so typing at an underline's edge
    /// does not swallow the new text (SPEC §5); a selection's *start* would use it
    /// to grow the other way.
    Before,
    /// Stick to the text after this position: an insertion here pushes the anchor
    /// right by the inserted length. A caret and a selection's *end* use this.
    After,
}

/// One applied edit in *base* (pre-edit) coordinates: replace `start..old_end` with
/// `insert_len` bytes. This is the [`Delta`](crate::view::Delta) shape reduced to the
/// numbers a position transform needs - the text content itself never matters to
/// where an anchor lands, only the lengths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Edit {
    /// Byte offset where the replaced range begins.
    pub start: usize,
    /// Byte offset where the replaced range ends (`start` for a pure insert).
    pub old_end: usize,
    /// Bytes inserted in the range's place (`0` for a pure deletion).
    pub insert_len: usize,
}

/// A position that survives edits: a byte offset plus the [`Bias`] deciding its
/// behavior at an insertion boundary (SPEC §2.1). Opaque handle semantics: resolve
/// it to a current offset with [`Anchor::offset`]; move it across an edit with
/// [`Anchor::transform`] / [`Anchor::transform_through`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Anchor {
    offset: usize,
    bias: Bias,
}

impl Anchor {
    /// An anchor at `offset` biased toward the text before it (an insertion at
    /// `offset` leaves it put). Decoration span ends use this (SPEC §5).
    pub(crate) fn before(offset: usize) -> Self {
        Self {
            offset,
            bias: Bias::Before,
        }
    }

    /// An anchor at `offset` biased toward the text after it (an insertion at
    /// `offset` pushes it right). Carets and selection ends use this.
    pub(crate) fn after(offset: usize) -> Self {
        Self {
            offset,
            bias: Bias::After,
        }
    }

    /// The anchor's current byte offset.
    pub(crate) fn offset(self) -> usize {
        self.offset
    }

    /// Move this anchor across one edit replacing `start..old_end` with `insert_len`
    /// bytes, all in the anchor's current coordinate space. The rule (SPEC §2.1):
    /// - **before the edit** (`offset < start`): unchanged.
    /// - **after the edit** (`offset > old_end`): shifted by the net length change.
    /// - **at or inside** (`start <= offset <= old_end`): collapses to the edit's
    ///   boundary - the left edge (`start`) for `Before`, the right edge of the
    ///   inserted text (`start + insert_len`) for `After`. This one arm covers an
    ///   insertion boundary, a deletion the anchor sits inside, and a replacement,
    ///   deterministically (the "documented deletion collapse" the SPEC requires).
    pub(crate) fn transform(self, start: usize, old_end: usize, insert_len: usize) -> Self {
        debug_assert!(start <= old_end, "edit range must not be inverted");
        let offset = if self.offset < start {
            self.offset
        } else if self.offset > old_end {
            // Net shift = inserted - removed; computed without underflow even when
            // the deletion outweighs the insertion.
            self.offset + insert_len - (old_end - start)
        } else {
            match self.bias {
                Bias::Before => start,
                Bias::After => start + insert_len,
            }
        };
        Self { offset, ..self }
    }

    /// Move this anchor across a batch of edits that are **disjoint and sorted
    /// ascending by `start`**, given in base (pre-edit) coordinates. Each edit's
    /// coordinates are shifted by the net effect of the earlier edits so the anchor
    /// and the edit stay in one coordinate space as the batch applies - the standard
    /// way to compose offset shifts for a multi-edit action (one keystroke over N
    /// cursors, SPEC §2.2/§2.4).
    pub(crate) fn transform_through(self, edits: &[Edit]) -> Self {
        let mut cur = self;
        let mut shift: isize = 0;
        for e in edits {
            // Rebase this edit into the current (already-shifted) space. `max(0)`
            // is defensive: consistent edits never shift a real position negative,
            // but a bad input must not panic on the cast (SPEC §8).
            let start = (e.start as isize + shift).max(0) as usize;
            let old_end = (e.old_end as isize + shift).max(0) as usize;
            cur = cur.transform(start, old_end, e.insert_len);
            shift += e.insert_len as isize - (e.old_end - e.start) as isize;
        }
        cur
    }
}

#[cfg(test)]
#[path = "anchor_tests.rs"]
mod tests;
