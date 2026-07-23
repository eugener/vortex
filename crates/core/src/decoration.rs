//! Decorations: everything the frontend paints *at a position* (SPEC §5).
//!
//! LSP diagnostics (underlines + gutter severity marks), syntax highlighting
//! (M4), git diff signs (M8) and inline hints are all the same shape - a payload
//! attached to a buffer position that must survive concurrent edits. Giving each
//! its own [`ViewSnapshot`](crate::view::ViewSnapshot) field would re-plumb the
//! seam, the snapshot builder, and the render loop once per feature, so they
//! share **one** channel: this typed set.
//!
//! **Positions survive edits, and cheaply.** Like [`Selection`](crate::selection),
//! a decoration stores plain byte offsets and the *bias* is applied at transform
//! time ([`DecorationSet::transform_through`]) rather than stored per endpoint -
//! the same mechanism, not a parallel one. Diagnostics are the first production
//! consumer of [`Bias::Before`](crate::anchor::Bias), which is why it existed
//! unused until now.
//!
//! **Styling stays frontend-owned.** A [`Severity`] or [`GutterKind`] is a
//! *semantic* tag, never an RGB color: the theme (SPEC §10.5) maps tags to
//! concrete styles, so identical core output themes light/dark and
//! truecolor/256-color without the core knowing terminal capabilities.
//!
//! **Producers are independent.** Each writes its own bucket
//! ([`DecorationSource`]), so the LSP republishing diagnostics cannot wipe
//! tree-sitter's highlights and vice versa - the property that lets M4 land on
//! this channel without touching M2's code.
//!
//! Not serialized: decorations ride the `ViewSnapshot`, which is explicitly
//! local-only (SPEC §5). A remote frontend needs its own incremental decoration
//! stream, deferred to `proto/` with the rest of that work (SPEC §11).

use std::collections::BTreeMap;

use crate::anchor::{Anchor, Edit};
use crate::buffer::{ByteRange, Text};

/// How bad a diagnostic is - a semantic tag, not a color (SPEC §5). Ordered
/// least to most severe, so `max()` over a line's marks picks the one to paint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Hint,
    Information,
    Warning,
    Error,
}

/// What a gutter mark means. Diagnostics fill this in M2; git add/change/remove
/// signs join as further variants in M8 - the reason this is an enum wrapping
/// [`Severity`] rather than a bare severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum GutterKind {
    /// An LSP diagnostic starts on this line.
    Diagnostic(Severity),
}

/// The semantic category of a syntax-highlighted span (M4) - a tag the theme
/// maps to a color, never a color itself (SPEC §5), exactly like [`Severity`].
///
/// The variants are the granularity the syntax producer resolves a grammar's
/// capture names to (`syntax::highlight`): a tree-sitter query captures
/// `@function.method`, `@function.macro`, `@type.builtin` and so on, and the
/// producer collapses each to the nearest variant here. Kept a *fixed* core enum
/// rather than an open-ended string so styling stays a closed, themeable set and
/// tree-sitter's own types never cross the seam - the same discipline that keeps
/// `lsp_types` out of the core's public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum HighlightKind {
    Attribute,
    Comment,
    Constant,
    ConstantBuiltin,
    Constructor,
    Escape,
    Function,
    Macro,
    Keyword,
    Label,
    Operator,
    Property,
    Punctuation,
    String,
    Type,
    TypeBuiltin,
    Variable,
    Parameter,
}

/// One painted overlay. `Highlight` (tree-sitter, M4) and `VirtualText` (inlay
/// hints, M8) are additive variants on this same enum - adding them later costs
/// a variant, not a new channel, which is the whole point of SPEC §5's decision
/// to unify these.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Decoration {
    /// Underline a byte span (a diagnostic's squiggle). Kept separate from a
    /// future `Highlight` so one cell can carry a syntax foreground color *and*
    /// an independent error undercurl at once (SPEC §5).
    Underline {
        range: ByteRange,
        severity: Severity,
    },
    /// Mark the line containing `offset`. Stored as an offset rather than a line
    /// index so it rides edits with the text: inserting a line above moves the
    /// mark down without the producer republishing.
    GutterMark { offset: usize, kind: GutterKind },
    /// Color a byte span with a syntax category (tree-sitter, M4). Distinct from
    /// [`Decoration::Underline`] so one cell can carry both a syntax foreground
    /// *and* an independent diagnostic undercurl at once (SPEC §5): the frontend
    /// paints highlights first, then diagnostic underlines and carets on top.
    Highlight {
        range: ByteRange,
        kind: HighlightKind,
    },
}

/// Which subsystem produced a decoration. Each owns its own bucket so producers
/// replace only their own output (SPEC §5: "producers are independent and
/// async"). Ordered because the buckets live in a `BTreeMap`, which keeps
/// iteration deterministic - a frontend painting overlapping spans must not see
/// them reorder between frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum DecorationSource {
    /// The LSP client (M2): diagnostics.
    Lsp,
    /// The syntax highlighter (M4): tree-sitter highlight spans. Its own bucket,
    /// so a diagnostics republish cannot wipe highlights and a reparse cannot wipe
    /// squiggles - the independence that lets both producers run async (SPEC §5).
    Syntax,
}

/// Every decoration currently attached to a buffer, bucketed by producer.
///
/// Shared behind an `Arc` on the snapshot, so publishing one is a ref-count bump
/// regardless of how many decorations it holds (SPEC §5). Resolution is the
/// frontend's job and is bounded by the *viewport*, never the file: see
/// [`Self::underlines_in`] and [`Self::gutter_mark`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecorationSet {
    by_source: BTreeMap<DecorationSource, Vec<Decoration>>,
}

impl DecorationSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether anything is decorated at all. The frontend's cheap "skip the
    /// overlay pass entirely" check for the overwhelmingly common case of a
    /// buffer with no LSP attached.
    pub fn is_empty(&self) -> bool {
        self.by_source.values().all(|d| d.is_empty())
    }

    /// Replace everything `source` previously contributed. This is the whole
    /// producer contract: LSP `publishDiagnostics` is defined as a full
    /// replacement for a file, and tree-sitter republishes a reparsed range the
    /// same way. Other producers' buckets are untouched.
    pub(crate) fn replace(&mut self, source: DecorationSource, decorations: Vec<Decoration>) {
        if decorations.is_empty() {
            self.by_source.remove(&source);
        } else {
            self.by_source.insert(source, decorations);
        }
    }

    /// Underline spans overlapping `range`, clipped to it, with the byte range
    /// expressed in buffer coordinates.
    ///
    /// The frontend calls this per painted line, so it borrows rather than
    /// allocating: cost is O(decorations) per call today, which is right for the
    /// handful of diagnostics a file carries. When M4 puts thousands of syntax
    /// highlights on this channel it needs an interval index behind the same
    /// signature - a change confined to this method, which is why resolution is
    /// a method and not a public field.
    pub fn underlines_in(&self, range: ByteRange) -> impl Iterator<Item = (ByteRange, Severity)> {
        self.by_source
            .values()
            .flatten()
            .filter_map(move |d| match d {
                Decoration::Underline {
                    range: span,
                    severity,
                } => {
                    let start = span.start.max(range.start);
                    let end = span.end.min(range.end);
                    // Overlap must be non-empty: a span that merely touches the
                    // line's edge paints nothing on it.
                    (start < end).then_some((start..end, *severity))
                }
                _ => None,
            })
    }

    /// Highlight spans overlapping `range`, clipped to it, in buffer coordinates
    /// (M4). Mirrors [`Self::underlines_in`]: the frontend calls it per painted
    /// line and paints each as a foreground color, so it borrows rather than
    /// allocating.
    ///
    /// Cost is O(decorations) per call - fine for a diagnostic's handful, but a
    /// syntax-highlighted file puts *thousands* of spans here. That is the case
    /// the doc-comment on this type flags for an interval index; it stays hidden
    /// behind this signature, so adding it later touches only this method and
    /// [`Self::transform_through`], not the frontend.
    pub fn highlights_in(
        &self,
        range: ByteRange,
    ) -> impl Iterator<Item = (ByteRange, HighlightKind)> {
        self.by_source
            .values()
            .flatten()
            .filter_map(move |d| match d {
                Decoration::Highlight { range: span, kind } => {
                    let start = span.start.max(range.start);
                    let end = span.end.min(range.end);
                    (start < end).then_some((start..end, *kind))
                }
                _ => None,
            })
    }

    /// The most severe gutter mark on `line`, or `None`. Several diagnostics
    /// commonly start on one line and the gutter has one cell, so the worst wins.
    pub fn gutter_mark(&self, text: &Text, line: usize) -> Option<GutterKind> {
        self.by_source
            .values()
            .flatten()
            .filter_map(|d| match d {
                Decoration::GutterMark { offset, kind } => {
                    (text.line_of_byte(*offset) == line).then_some(*kind)
                }
                _ => None,
            })
            .max()
    }

    /// Move every decoration across a batch of applied edits (SPEC §2.1, §5), so
    /// overlays keep pointing at the right text between a producer's refreshes.
    /// `edits` are in base coordinates, disjoint and sorted ascending - the same
    /// contract [`Anchor::transform_through`] takes.
    ///
    /// **Spans shift, they never grow.** A span's start is [`Bias::After`] and
    /// its end [`Bias::Before`] - the *opposite* of a selection, deliberately.
    /// Typing immediately before an underlined identifier pushes the underline
    /// along rather than swallowing the new text, and typing at its end leaves it
    /// put; a selection wants the reverse because the user is extending it by
    /// hand. Deleting the flagged text collapses the span to empty, so the
    /// squiggle disappears instead of hanging over unrelated text until the
    /// server republishes.
    ///
    /// Cost is O(decorations) per edit. Correct for M2's handful of diagnostics;
    /// M4's thousands of highlights are what justifies revisiting it, and that is
    /// the same interval-index change [`Self::underlines_in`] wants.
    pub(crate) fn transform_through(&mut self, edits: &[Edit]) {
        for decoration in self.by_source.values_mut().flatten() {
            match decoration {
                // A highlight rides edits with the same shift-don't-grow bias as
                // an underline: typing at a token's edge moves its color along
                // rather than swallowing the new (as-yet-unparsed) text, which the
                // next reparse then colors correctly (SPEC §5, overlays trail by a
                // frame). Sharing the arm keeps the two span kinds identical here.
                Decoration::Underline { range, .. } | Decoration::Highlight { range, .. } => {
                    range.start = Anchor::after(range.start).transform_through(edits).offset();
                    range.end = Anchor::before(range.end).transform_through(edits).offset();
                    // A deletion spanning the whole range collapses the two ends
                    // onto the same point from opposite sides; the end can land
                    // left of the start when the edit removed text between them.
                    range.end = range.end.max(range.start);
                }
                Decoration::GutterMark { offset, .. } => {
                    *offset = Anchor::after(*offset).transform_through(edits).offset();
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "decoration_tests.rs"]
mod tests;
