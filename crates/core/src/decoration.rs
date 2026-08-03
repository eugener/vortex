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

/// What a gutter mark means. Diagnostics filled this in M2 and git signs joined in
/// M8 - the reason it was an enum wrapping [`Severity`] from the start rather than a
/// bare severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum GutterKind {
    /// An LSP diagnostic starts on this line.
    Diagnostic(Severity),
    /// This line differs from the file's committed state (M8).
    Git(GitSign),
}

/// How a line differs from HEAD (M8) - a semantic tag the theme maps to a glyph and
/// a color, never either itself, exactly like [`Severity`].
///
/// The producer is the **frontend**, because reading a repository is filesystem work
/// (SPEC §3) - only the vocabulary lives here. It rides the decoration channel rather
/// than being painted from a list the frontend keeps, so a sign moves with the text
/// it marks: [`Decoration::GutterMark`] stores an offset, so inserting a line above a
/// changed one shifts its sign down without recomputing the diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum GitSign {
    /// The line is new since HEAD.
    Added,
    /// The line existed and its content changed.
    Modified,
    /// One or more lines were deleted *at* this line. Marked on the survivor rather
    /// than on nothing: a deletion has no line of its own to sit on.
    Removed,
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
    ///
    /// Carries the server's `message`, because the squiggle alone says only that
    /// *something* is wrong here. The status bar shows it once the caret has rested
    /// on the span (SPEC §7.5, M10), and the diagnostics picker lists it. Held as an
    /// owned `String` on a decoration the producer republishes wholesale, so no
    /// lifetime crosses the seam.
    Underline {
        range: ByteRange,
        severity: Severity,
        message: String,
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
    /// A structural scope the text at this range sits inside - a function, an
    /// `impl`, a class (tree-sitter, M8). The frontend pins the *first line* of
    /// every scope enclosing the viewport's top row as a sticky context header
    /// (SPEC §7.5), so what crosses the seam is the whole file's scope ranges,
    /// once per parse, rather than an answer for one row: the row in question
    /// changes on **scroll**, and scrolling is frontend-owned (SPEC §5) - asking
    /// the core would put a round-trip on the scroll path.
    ///
    /// The one decoration that is not painted *at* its position. It is here
    /// anyway because it is a position that must survive concurrent edits, which
    /// is what this channel is (SPEC §5): typing inside a function has to move
    /// the scope with the text, or the header names the wrong one until the next
    /// reparse lands.
    Scope { range: ByteRange },
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
    /// The syntax highlighter again (M8): [`Decoration::Scope`] ranges for the
    /// sticky context header. Same producer as [`Self::Syntax`] but a **separate
    /// bucket**, and that separation is load-bearing rather than tidy: scopes are
    /// *nested* by construction, so their ends are not monotonic, while
    /// [`DecorationSet::highlights_in`] binary-searches a bucket on exactly the
    /// sorted-and-non-overlapping invariant highlights have. Mixing the two would
    /// misplace the highlight search rather than merely slow it.
    Scope,
    /// The git-diff task (M8): [`GutterKind::Git`] marks. Its own bucket for the
    /// reason every bucket is one - a diff finishing must not wipe the diagnostics
    /// standing beside it in the same gutter, and the two producers run independently.
    Git,
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
    ///
    /// The bucket is kept **sorted by span start**. That invariant is what lets
    /// [`Self::highlights_in`] binary-search instead of scanning every span per
    /// painted line - the whole point of an interval query on the thousands of
    /// syntax spans a file carries. Sorting is cheap: tree-sitter already emits
    /// highlights in document order, so the sort is near-linear, and it is paid
    /// once per producer republish (amortized against the reparse), never per
    /// frame. [`Self::transform_through`] preserves the order (edits are disjoint
    /// and monotonic), so it need not re-sort.
    ///
    /// The editor itself splits this into [`Self::sorted_bucket`] +
    /// [`Self::bucket_differs`] + [`Self::set_bucket`] so an unchanged reparse
    /// clones nothing (see its `apply_bucket`); this is the same operation in one
    /// step, for a caller *building* a set rather than maintaining one.
    ///
    /// **Test surface, not production API.** Nothing in the editor calls it - the
    /// copy-on-write path above is the one producers take - and it is gated behind
    /// `test-support` rather than made plain `pub` so that stays true: a frontend's
    /// own tests can build a set to exercise resolution without standing up a core
    /// and a producer, without the crate growing a public way to bulk-replace a
    /// bucket around the discipline the rest of the mutation path is built on.
    #[cfg(any(test, feature = "test-support"))]
    pub fn replace(&mut self, source: DecorationSource, mut decorations: Vec<Decoration>) {
        if decorations.is_empty() {
            self.by_source.remove(&source);
        } else {
            decorations.sort_by_key(span_start);
            self.by_source.insert(source, decorations);
        }
    }

    /// Sort `decorations` into the order a bucket is stored in (by span start), so
    /// the editor can build a candidate bucket, compare it against the current one
    /// with [`Self::bucket_differs`], and only *then* take a copy-on-write mutable
    /// borrow via [`Self::set_bucket`] - avoiding cloning the whole set when a
    /// reparse produced no change (the common case while typing).
    pub(crate) fn sorted_bucket(mut decorations: Vec<Decoration>) -> Vec<Decoration> {
        decorations.sort_by_key(span_start);
        decorations
    }

    /// Whether `source`'s bucket differs from `candidate` (already sorted via
    /// [`Self::sorted_bucket`]). Compares only that one bucket, never the whole set.
    pub(crate) fn bucket_differs(
        &self,
        source: DecorationSource,
        candidate: &[Decoration],
    ) -> bool {
        match self.by_source.get(&source) {
            Some(existing) => existing.as_slice() != candidate,
            None => !candidate.is_empty(),
        }
    }

    /// Store `candidate` (already sorted) as `source`'s bucket, or clear the bucket
    /// if it is empty. The presorted twin of [`Self::replace`], used after
    /// [`Self::bucket_differs`] has already established a change.
    pub(crate) fn set_bucket(&mut self, source: DecorationSource, candidate: Vec<Decoration>) {
        if candidate.is_empty() {
            self.by_source.remove(&source);
        } else {
            self.by_source.insert(source, candidate);
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
                    ..
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
    /// **O(log n + k), not O(n).** This is the per-line-per-frame hot path over
    /// the thousands of syntax spans a file carries, so it exploits the
    /// sorted-by-start invariant [`Self::replace`] maintains. Highlights are also
    /// non-overlapping (one producer, innermost-wins in `syntax::highlight`), so
    /// their ends are monotonic and `partition_point` finds the first span reaching
    /// into `range` in O(log n); the forward scan then stops at the first span
    /// starting past `range.end`, bounded by the `k` spans actually on this line.
    /// Only the syntax bucket carries `Highlight`s, so a bucket without them
    /// contributes nothing regardless of where the search lands (its non-monotonic
    /// ends can misplace the search, but there is nothing there to miss).
    pub fn highlights_in(
        &self,
        range: ByteRange,
    ) -> impl Iterator<Item = (ByteRange, HighlightKind)> {
        self.by_source.values().flat_map(move |bucket| {
            let first = bucket.partition_point(|d| span_end(d) <= range.start);
            bucket[first..]
                .iter()
                .take_while(move |d| span_start(d) < range.end)
                .filter_map(move |d| match d {
                    Decoration::Highlight { range: span, kind } => {
                        let start = span.start.max(range.start);
                        let end = span.end.min(range.end);
                        (start < end).then_some((start..end, *kind))
                    }
                    _ => None,
                })
        })
    }

    /// The scopes enclosing `offset`, outermost first (M8) - the chain the
    /// frontend turns into sticky context header rows (SPEC §7.5).
    ///
    /// "Enclosing" is strict on the left and open on the right: a scope
    /// *starting* at `offset` is not enclosing it, because the frontend passes
    /// the first byte of the top visible row and a scope starting there already
    /// has its own first line on screen - pinning it would print that line twice.
    /// A scope merely *ending* at `offset` has no cell on that row and is out.
    ///
    /// Reads only the [`DecorationSource::Scope`] bucket rather than every
    /// decoration: it is the one bucket that can hold a [`Decoration::Scope`],
    /// and the file's syntax bucket beside it is three orders of magnitude
    /// larger. The walk then **stops at the first scope starting at or past
    /// `offset`** - the bucket is sorted by start, so nothing after that one can
    /// enclose it either. That is the same sorted-bucket exploit
    /// [`Self::highlights_in`] makes, minus the binary search: scopes are nested,
    /// so their *ends* are not monotonic and there is no second bound to search
    /// for. Cost is O(scopes above the offset), resolved once per frame rather
    /// than per painted row.
    pub fn scopes_at(&self, offset: usize) -> impl Iterator<Item = ByteRange> {
        self.by_source
            .get(&DecorationSource::Scope)
            .into_iter()
            .flatten()
            .take_while(move |d| span_start(d) < offset)
            .filter_map(move |d| match d {
                Decoration::Scope { range } if range.end > offset => Some(range.clone()),
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

    /// The diagnostic covering `offset`, most severe first, or `None`.
    ///
    /// What the status bar shows once the caret has *rested* on a flagged span
    /// (SPEC §7.5, M10). Asked about the caret's byte rather than its line, so a
    /// line carrying two diagnostics reports the one the caret is actually inside
    /// rather than whichever the producer happened to send first.
    ///
    /// Ties break on severity, for the reason [`Self::gutter_mark`] breaks them the
    /// same way: there is one segment and the worst thing wrong is what to say.
    pub fn diagnostic_at(&self, offset: usize) -> Option<(Severity, &str)> {
        self.by_source
            .values()
            .flatten()
            .filter_map(|d| match d {
                Decoration::Underline {
                    range,
                    severity,
                    message,
                } if range.contains(&offset) => Some((*severity, message.as_str())),
                _ => None,
            })
            .max_by_key(|(severity, _)| *severity)
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
                // A scope rides edits on the same bias, and it is the right one
                // for the same reason read from the other end: typing just before
                // a `fn` keyword must push the scope along rather than swallow the
                // new text, and typing just after its closing brace must leave the
                // scope where it is rather than stretch it over what follows.
                Decoration::Underline { range, .. }
                | Decoration::Highlight { range, .. }
                | Decoration::Scope { range } => {
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

/// The interval key a bucket is sorted by ([`DecorationSet::replace`]): a span's
/// start, or a gutter mark's line offset.
fn span_start(d: &Decoration) -> usize {
    match d {
        Decoration::Underline { range, .. }
        | Decoration::Highlight { range, .. }
        | Decoration::Scope { range } => range.start,
        Decoration::GutterMark { offset, .. } => *offset,
    }
}

/// A span's exclusive end, or a gutter mark's point. The monotonic key
/// [`DecorationSet::highlights_in`] binary-searches over the non-overlapping,
/// sorted highlight spans.
fn span_end(d: &Decoration) -> usize {
    match d {
        Decoration::Underline { range, .. }
        | Decoration::Highlight { range, .. }
        | Decoration::Scope { range } => range.end,
        Decoration::GutterMark { offset, .. } => *offset,
    }
}

#[cfg(test)]
#[path = "decoration_tests.rs"]
mod tests;
