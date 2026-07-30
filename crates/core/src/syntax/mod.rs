//! The syntax highlighter (SPEC §3, §5, M4).
//!
//! **A second decoration producer, the shape of the first.** M2 built the LSP
//! client as a producer that takes buffer text in and emits decorations out over
//! channels, spawned by the frontend on its own executor; this is the same shape
//! with a different engine. [`highlighter`] returns a [`SyntaxHandle`] plus a loop
//! `Future` the frontend spawns (mirroring [`crate::editor::new`] and
//! [`crate::lsp::client`]), so `vortex-core` still names no executor and the parse
//! runs off the editor actor - never on the keystroke path.
//!
//! **The grammar is injected, not bundled.** [`highlighter`] takes a
//! `tree_sitter::Language` and the query strings as parameters, exactly as
//! [`crate::lsp::client`] takes a server *command* rather than hard-coding one.
//! The frontend owns the file-type -> grammar mapping and loads grammars at
//! runtime (dynamically, from config), so adding a language never touches the
//! core. tree-sitter's own types appear in no other public signature: the loop
//! emits the core's semantic [`HighlightKind`], which the theme maps to a color
//! (SPEC §5), keeping styling frontend-owned.
//!
//! **Full reparse per snapshot, not incremental.** This is the same call M2 made
//! for LSP `didChange` (see [`crate::lsp::DocumentSync`]): tree-sitter *can* parse
//! incrementally from the previous tree plus `InputEdit`s, but that requires the
//! producer and editor to agree on a version-by-version edit history, and one
//! desync silently mis-colors every span after it. A full reparse cannot desync,
//! and tree-sitter parses a typical file in well under a frame off the keystroke
//! path. Incremental parsing is an optimization to make against a benchmark, not
//! a default to assume - deferred with the interval index the decoration channel
//! also wants (SPEC §14).
//!
//! **Scopes cost a second parse, and that is the honest price** (M8). The sticky
//! context header needs the parse *tree* - "which named nodes enclose this row" -
//! but `tree_sitter_highlight::Highlighter::highlight` parses internally and hands
//! back only an event stream, with no way to reach the tree or to supply one. The
//! alternatives were to drop `tree_sitter_highlight` and re-implement highlighting
//! over a raw `Query` (re-deriving its locals and injection machinery), or to run
//! a second `tree_sitter::Parser` over the same source. This does the latter, and
//! only for grammars that ship a `context.scm`: the extra parse is on the
//! highlighter's own thread, off the keystroke path, and coalesced by the same
//! drain-to-newest that already keeps fast typing from queueing parses. It
//! collapses back to one parse if incremental reparse ever lands, since that
//! change means owning the tree here anyway.

pub(crate) mod engine;
pub(crate) mod highlight;
pub(crate) mod scope;

use crate::buffer::{ByteRange, Text};
use crate::decoration::HighlightKind;
use crate::view::BufferId;

pub use engine::{SyntaxError, SyntaxHandle, highlighter};

/// The loop the frontend spawns, resolving to why it stopped (SPEC §8). A named
/// alias so the frontend does not spell out the boxed-future type, matching
/// [`crate::lsp::BoxLspLoop`].
pub type BoxSyntaxLoop = crate::editor::BoxFuture<Result<(), SyntaxError>>;

/// editor -> highlighter: the buffer text to reparse, and which buffer at which
/// version it is (SPEC §5). Full-document, for the "cannot desync" reason in the
/// module doc; the identity rides along so a highlight batch is recognizable as
/// what it was computed against, the same role it plays for LSP `didChange`.
///
/// The `buffer_id` is echoed back on [`SyntaxEvent::Highlights`] and is not
/// redundant with the version: versions are **per-buffer** (SPEC §5), so two open
/// documents are routinely at the same version, and a batch parsed against one
/// would otherwise be indistinguishable from a current batch for the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxSync {
    pub buffer_id: BufferId,
    pub version: u64,
    /// The buffer text as the core's cheap `Arc`-backed handle. It becomes owned
    /// bytes only inside the highlighter loop, on the highlighter's own thread -
    /// never on the editor actor, which would copy the whole file per keystroke.
    pub text: Text,
}

/// One highlighted span, in the core's own vocabulary rather than tree-sitter's:
/// a byte range plus the semantic [`HighlightKind`] the theme colors. Byte
/// offsets, resolved by the producer against the text it parsed - they become a
/// [`Decoration::Highlight`](crate::decoration::Decoration::Highlight) unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSpan {
    pub range: ByteRange,
    pub kind: HighlightKind,
}

/// highlighter -> editor: a fresh, complete highlight set for a version. Like LSP
/// `publishDiagnostics`, this is a full replacement of the syntax bucket, not a
/// delta (SPEC §5); an empty list means "nothing highlights" (e.g. an empty
/// buffer), which clears the bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SyntaxEvent {
    Highlights {
        /// Which buffer these spans were parsed against, echoed from the
        /// [`SyntaxSync`] that produced them. The editor drops a batch whose
        /// `(buffer_id, version)` no longer matches: applying one to the wrong
        /// buffer would *misplace* spans, not merely leave them a frame stale,
        /// which the SPEC §5 overlay contract forbids.
        buffer_id: BufferId,
        version: u64,
        spans: Vec<HighlightSpan>,
    },
    /// The structural scopes of a version - the ranges the sticky context header
    /// pins a first line from (SPEC §7.5, M8). A full replacement of the
    /// [`Scope`](crate::decoration::DecorationSource::Scope) bucket, exactly as
    /// [`Self::Highlights`] replaces the syntax one.
    ///
    /// A **separate event**, not a field on `Highlights`, so a grammar that ships
    /// no `context.scm` simply never sends one - the highlighting path stays
    /// untouched for every language that has not opted in, which is the same
    /// additive-variant property that let M4 land on M2's channel.
    Scopes {
        buffer_id: BufferId,
        version: u64,
        /// Byte ranges of the enclosing nodes, outermost first among those
        /// sharing a start (`syntax::scope`).
        spans: Vec<ByteRange>,
    },
}
